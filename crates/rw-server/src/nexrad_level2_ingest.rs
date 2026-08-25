//! Server-owned, request-independent NEXRAD Level II acquisition.
//!
//! Only explicitly configured sites are followed. The provider boundary is an
//! S3-compatible public archive contract rather than a hard-coded hostname,
//! while exact object keys, sizes, timestamps, and SHA-256 digests are carried
//! into the stored scientific selector. HTTP requests never create or steer a
//! worker; they can only read status or coalesce an operator refresh.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone as _, Utc};
use quick_xml::de::from_str;
use rw_observations::{
    NexradIngestOptions, NexradSourceIdentity, RadarGridMode, RadarMoment, StoredFrameRef,
    ingest_nexrad_level2,
};
use rw_query::RunSnapshot;
use rw_store::atomic::atomic_write_bytes;
use rw_store::lock::RunLock;
use rw_store::run::RwsRunManifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::AppState;
use crate::config::{NexradLevel2IngestConfig, NexradLevel2ProviderConfig, NexradLevel2SiteConfig};

const STATUS_SCHEMA: &str = "rw-server.nexrad-level2-ingest-status.v1";
const REFRESH_SCHEMA: &str = "rw-server.nexrad-level2-ingest-refresh.v1";
const STATE_SCHEMA: &str = "rw-server.nexrad-level2-ingest-state.v1";
const STATE_FILE: &str = "state-v1.json";
const RADAR_MODEL: &str = "obs-radar";
const RETIRED_RUN_PREFIX: &str = ".rw-nexrad-level2-retired-";
const HTTP_READ_CHUNK: usize = 64 * 1024;
const MAX_STATUS_ERROR_BYTES: usize = 1_024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NexradLevel2SitePhase {
    Starting,
    Waiting,
    Fetching,
    Ready,
    Degraded,
    Stopped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NexradLevel2SourceObjectStatus {
    pub provider_id: String,
    pub object_key: String,
    pub object_bytes: u64,
    pub sha256: String,
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NexradLevel2StoredFrameStatus {
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub storage_slot: u16,
    pub valid_unix: i64,
    pub variables: Vec<String>,
    pub grid_hash: String,
    pub duplicate: bool,
    pub source: NexradLevel2SourceObjectStatus,
}

impl NexradLevel2StoredFrameStatus {
    fn from_stored(
        frame: StoredFrameRef,
        snapshot_id: String,
        source: NexradLevel2SourceObjectStatus,
    ) -> Self {
        Self {
            model: frame.model,
            run: frame.run,
            snapshot_id,
            storage_slot: frame.storage_slot,
            valid_unix: frame.valid_unix,
            variables: frame.variables,
            grid_hash: frame.grid_hash,
            duplicate: frame.duplicate,
            source,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NexradLevel2SiteStatus {
    pub site_id: String,
    pub provider_id: String,
    pub attribution: String,
    pub resolution_m: f64,
    pub radius_km: f64,
    pub coordinates_supplied: bool,
    pub phase: NexradLevel2SitePhase,
    pub attempts: u64,
    pub consecutive_failures: u32,
    pub last_attempt_unix: Option<i64>,
    pub last_success_unix: Option<i64>,
    pub latest_valid_unix: Option<i64>,
    pub source_age_seconds: Option<i64>,
    pub fresh: bool,
    pub next_attempt_unix: Option<i64>,
    pub latest: Option<NexradLevel2StoredFrameStatus>,
    pub ingested_volumes: u64,
    pub duplicate_volumes: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NexradLevel2IngestStatus {
    pub schema: &'static str,
    pub enabled: bool,
    pub ready: bool,
    pub gate_server_readiness: bool,
    pub checked_unix: i64,
    pub poll_interval_seconds: u64,
    pub stale_after_seconds: u64,
    pub maximum_backoff_seconds: u64,
    pub request_timeout_seconds: u64,
    pub request_retries: u32,
    pub concurrency: usize,
    pub maximum_listing_bytes: usize,
    pub maximum_object_bytes: usize,
    pub catch_up_hours: u64,
    pub retention_hours: u64,
    pub in_flight: usize,
    pub wake_epoch: u64,
    pub last_retention_unix: Option<i64>,
    pub retention_removed_runs: u64,
    pub sites: Vec<NexradLevel2SiteStatus>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct NexradLevel2RefreshResponse {
    pub schema: &'static str,
    pub accepted: bool,
    pub wake_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableSiteCursor {
    provider_id: String,
    object_key: String,
    object_valid_unix: i64,
    latest: NexradLevel2StoredFrameStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableState {
    schema: String,
    sites: BTreeMap<String, DurableSiteCursor>,
}

impl Default for DurableState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.into(),
            sites: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct SiteRuntime {
    spec: NexradLevel2SiteConfig,
    provider: NexradLevel2ProviderConfig,
    phase: NexradLevel2SitePhase,
    attempts: u64,
    consecutive_failures: u32,
    last_attempt_unix: Option<i64>,
    last_success_unix: Option<i64>,
    next_attempt_unix: Option<i64>,
    cursor: Option<DurableSiteCursor>,
    ingested_volumes: u64,
    duplicate_volumes: u64,
    last_error: Option<String>,
}

#[derive(Debug)]
struct RuntimeState {
    sites: Vec<SiteRuntime>,
    in_flight: usize,
    last_retention_unix: Option<i64>,
    retention_removed_runs: u64,
}

#[derive(Clone, Debug)]
pub struct NexradLevel2IngestMonitor {
    config: Arc<NexradLevel2IngestConfig>,
    state: Arc<Mutex<RuntimeState>>,
    wake: watch::Sender<u64>,
}

impl NexradLevel2IngestMonitor {
    pub fn new(config: &NexradLevel2IngestConfig) -> Self {
        let providers = config
            .providers
            .iter()
            .map(|provider| (provider.id.to_ascii_lowercase(), provider.clone()))
            .collect::<BTreeMap<_, _>>();
        let sites = config
            .sites
            .iter()
            .cloned()
            .filter_map(|spec| {
                let provider = providers
                    .get(&spec.provider_id.to_ascii_lowercase())?
                    .clone();
                Some(SiteRuntime {
                    spec,
                    provider,
                    phase: if config.enabled {
                        NexradLevel2SitePhase::Starting
                    } else {
                        NexradLevel2SitePhase::Stopped
                    },
                    attempts: 0,
                    consecutive_failures: 0,
                    last_attempt_unix: None,
                    last_success_unix: None,
                    next_attempt_unix: None,
                    cursor: None,
                    ingested_volumes: 0,
                    duplicate_volumes: 0,
                    last_error: None,
                })
            })
            .collect();
        let (wake, _receiver) = watch::channel(0);
        Self {
            config: Arc::new(config.clone()),
            state: Arc::new(Mutex::new(RuntimeState {
                sites,
                in_flight: 0,
                last_retention_unix: None,
                retention_removed_runs: 0,
            })),
            wake,
        }
    }

    pub fn status(&self) -> NexradLevel2IngestStatus {
        self.status_at(now_unix())
    }

    fn status_at(&self, checked_unix: i64) -> NexradLevel2IngestStatus {
        let state = lock_unpoisoned(&self.state);
        let sites = state
            .sites
            .iter()
            .map(|site| {
                let latest_valid_unix = site.cursor.as_ref().map(|cursor| cursor.latest.valid_unix);
                let source_age_seconds =
                    latest_valid_unix.map(|valid_unix| checked_unix.saturating_sub(valid_unix));
                let fresh = source_age_seconds.is_some_and(|age| {
                    age >= -300 && age <= self.config.stale_after_seconds as i64
                });
                NexradLevel2SiteStatus {
                    site_id: site.spec.site_id.to_ascii_uppercase(),
                    provider_id: site.provider.id.clone(),
                    attribution: site.provider.attribution.clone(),
                    resolution_m: site.spec.resolution_m,
                    radius_km: site.spec.radius_km,
                    coordinates_supplied: site.spec.latitude.is_some(),
                    phase: site.phase,
                    attempts: site.attempts,
                    consecutive_failures: site.consecutive_failures,
                    last_attempt_unix: site.last_attempt_unix,
                    last_success_unix: site.last_success_unix,
                    latest_valid_unix,
                    source_age_seconds,
                    fresh,
                    next_attempt_unix: site.next_attempt_unix,
                    latest: site.cursor.as_ref().map(|cursor| cursor.latest.clone()),
                    ingested_volumes: site.ingested_volumes,
                    duplicate_volumes: site.duplicate_volumes,
                    last_error: site.last_error.clone(),
                }
            })
            .collect::<Vec<_>>();
        let ready = !self.config.enabled
            || (!sites.is_empty()
                && sites
                    .iter()
                    .all(|site| site.fresh && site.phase != NexradLevel2SitePhase::Stopped));
        NexradLevel2IngestStatus {
            schema: STATUS_SCHEMA,
            enabled: self.config.enabled,
            ready,
            gate_server_readiness: self.config.gate_server_readiness,
            checked_unix,
            poll_interval_seconds: self.config.poll_interval_seconds,
            stale_after_seconds: self.config.stale_after_seconds,
            maximum_backoff_seconds: self.config.maximum_backoff_seconds,
            request_timeout_seconds: self.config.request_timeout_seconds,
            request_retries: self.config.request_retries,
            concurrency: self.config.concurrency,
            maximum_listing_bytes: self.config.maximum_listing_bytes,
            maximum_object_bytes: self.config.maximum_object_bytes,
            catch_up_hours: self.config.catch_up_hours,
            retention_hours: self.config.retention_hours,
            in_flight: state.in_flight,
            wake_epoch: *self.wake.borrow(),
            last_retention_unix: state.last_retention_unix,
            retention_removed_runs: state.retention_removed_runs,
            sites,
        }
    }

    #[must_use]
    pub fn server_readiness_ok(&self) -> bool {
        !self.config.gate_server_readiness || self.status().ready
    }

    #[must_use]
    pub fn is_degraded(&self) -> bool {
        let status = self.status();
        status.enabled
            && (!status.ready
                || status
                    .sites
                    .iter()
                    .any(|site| site.phase == NexradLevel2SitePhase::Degraded))
    }

    pub fn request_refresh(&self) -> u64 {
        self.wake
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        *self.wake.borrow()
    }

    fn wake_receiver(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    fn restore(&self, durable: DurableState, store_root: &Path) {
        let mut state = lock_unpoisoned(&self.state);
        for site in &mut state.sites {
            let id = site.spec.site_id.to_ascii_uppercase();
            let Some(cursor) = durable.sites.get(&id) else {
                continue;
            };
            if !cursor.provider_id.eq_ignore_ascii_case(&site.provider.id) {
                site.last_error = Some("durable cursor provider no longer matches configuration; source will be rediscovered".into());
                continue;
            }
            let valid = RunSnapshot::open(store_root, &cursor.latest.model, &cursor.latest.run)
                .ok()
                .filter(|snapshot| snapshot.descriptor().snapshot_id == cursor.latest.snapshot_id)
                .and_then(|snapshot| snapshot.timepoint(cursor.latest.storage_slot).ok())
                .is_some_and(|time| time.valid_unix == cursor.latest.valid_unix);
            if valid {
                site.cursor = Some(cursor.clone());
                site.phase = NexradLevel2SitePhase::Waiting;
            } else {
                site.last_error = Some("durable cursor did not reopen an exact stored frame; source will be fetched again".into());
            }
        }
    }

    fn durable_snapshot(&self) -> DurableState {
        let sites = lock_unpoisoned(&self.state)
            .sites
            .iter()
            .filter_map(|site| {
                site.cursor
                    .as_ref()
                    .map(|cursor| (site.spec.site_id.to_ascii_uppercase(), cursor.clone()))
            })
            .collect();
        DurableState {
            schema: STATE_SCHEMA.into(),
            sites,
        }
    }

    fn begin_attempt(&self, index: usize, attempted_unix: i64) -> Option<DurableSiteCursor> {
        let mut state = lock_unpoisoned(&self.state);
        state.in_flight = state.in_flight.saturating_add(1);
        let site = state.sites.get_mut(index)?;
        site.phase = NexradLevel2SitePhase::Fetching;
        site.attempts = site.attempts.saturating_add(1);
        site.last_attempt_unix = Some(attempted_unix);
        site.next_attempt_unix = None;
        site.cursor.clone()
    }

    fn finish_success(&self, index: usize, completed_unix: i64, outcome: CycleOutcome) {
        let mut state = lock_unpoisoned(&self.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(site) = state.sites.get_mut(index) {
            site.phase = NexradLevel2SitePhase::Ready;
            site.consecutive_failures = 0;
            site.last_success_unix = Some(completed_unix);
            site.next_attempt_unix =
                Some(completed_unix.saturating_add(self.config.poll_interval_seconds as i64));
            site.ingested_volumes = site.ingested_volumes.saturating_add(outcome.ingested);
            site.duplicate_volumes = site.duplicate_volumes.saturating_add(outcome.duplicates);
            if let Some(cursor) = outcome.cursor {
                site.cursor = Some(cursor);
            }
            site.last_error = None;
        }
    }

    fn finish_failure(&self, index: usize, completed_unix: i64, error: String) -> Duration {
        let mut state = lock_unpoisoned(&self.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        let Some(site) = state.sites.get_mut(index) else {
            return Duration::from_secs(self.config.maximum_backoff_seconds);
        };
        site.phase = NexradLevel2SitePhase::Degraded;
        site.consecutive_failures = site.consecutive_failures.saturating_add(1);
        site.last_error = Some(bound_error(error));
        let delay = retry_delay(&self.config, site.consecutive_failures);
        site.next_attempt_unix = Some(completed_unix.saturating_add(delay.as_secs() as i64));
        delay
    }

    fn mark_waiting(&self, index: usize) {
        if let Some(site) = lock_unpoisoned(&self.state).sites.get_mut(index)
            && site.phase != NexradLevel2SitePhase::Degraded
        {
            site.phase = NexradLevel2SitePhase::Waiting;
        }
    }

    fn mark_stopped(&self, index: usize) {
        if let Some(site) = lock_unpoisoned(&self.state).sites.get_mut(index) {
            site.phase = NexradLevel2SitePhase::Stopped;
            site.next_attempt_unix = None;
        }
    }

    fn retention_due(&self, checked_unix: i64) -> bool {
        lock_unpoisoned(&self.state)
            .last_retention_unix
            .is_none_or(|last| checked_unix.saturating_sub(last) >= 60 * 60)
    }

    fn record_retention(&self, completed_unix: i64, removed_runs: u64) {
        let mut state = lock_unpoisoned(&self.state);
        state.last_retention_unix = Some(completed_unix);
        state.retention_removed_runs = state.retention_removed_runs.saturating_add(removed_runs);
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug)]
struct CycleOutcome {
    cursor: Option<DurableSiteCursor>,
    ingested: u64,
    duplicates: u64,
}

trait NexradLevel2CycleRunner: Send + Sync + 'static {
    fn run(
        &self,
        store_root: &Path,
        site: &NexradLevel2SiteConfig,
        provider: &NexradLevel2ProviderConfig,
        previous: Option<DurableSiteCursor>,
        now: DateTime<Utc>,
    ) -> Result<CycleOutcome, String>;
}

#[derive(Debug, Clone)]
struct ArchiveObject {
    key: String,
    bytes: u64,
    last_modified: Option<String>,
    valid_time: DateTime<Utc>,
}

struct ProductionCycleRunner {
    config: NexradLevel2IngestConfig,
    agents: BTreeMap<String, ureq::Agent>,
}

impl ProductionCycleRunner {
    fn new(config: &NexradLevel2IngestConfig) -> Result<Self, String> {
        let mut agents = BTreeMap::new();
        for provider in &config.providers {
            agents.insert(
                provider.id.to_ascii_lowercase(),
                hardened_agent(provider, config.request_timeout_seconds)?,
            );
        }
        Ok(Self {
            config: config.clone(),
            agents,
        })
    }
}

impl NexradLevel2CycleRunner for ProductionCycleRunner {
    fn run(
        &self,
        store_root: &Path,
        site: &NexradLevel2SiteConfig,
        provider: &NexradLevel2ProviderConfig,
        previous: Option<DurableSiteCursor>,
        now: DateTime<Utc>,
    ) -> Result<CycleOutcome, String> {
        let agent = self
            .agents
            .get(&provider.id.to_ascii_lowercase())
            .ok_or_else(|| format!("provider '{}' has no HTTP adapter", provider.id))?;
        let objects =
            discover_objects(agent, provider, site, previous.as_ref(), now, &self.config)?;
        let mut cursor = previous;
        let mut ingested = 0u64;
        let mut duplicates = 0u64;
        for object in objects {
            let bytes = request_with_retries(self.config.request_retries, || {
                download_object(agent, provider, &object, self.config.maximum_object_bytes)
            })?;
            let sha256 = hex_sha256(&bytes);
            let source = NexradLevel2SourceObjectStatus {
                provider_id: provider.id.clone(),
                object_key: object.key.clone(),
                object_bytes: bytes.len() as u64,
                sha256: sha256.clone(),
                last_modified: object.last_modified.clone(),
            };
            let options = NexradIngestOptions {
                site_id: Some(site.site_id.to_ascii_uppercase()),
                site_latitude: site.latitude,
                site_longitude: site.longitude,
                site_elevation_m: site.elevation_m,
                moment: RadarMoment::Reflectivity,
                mode: RadarGridMode::Lowest,
                resolution_m: site.resolution_m,
                radius_km: site.radius_km,
                collection: Some(site.site_id.to_ascii_lowercase()),
                variable: Some("radar_reflectivity".into()),
                source_identity: Some(NexradSourceIdentity {
                    provider_id: provider.id.clone(),
                    attribution: provider.attribution.clone(),
                    object_key: object.key.clone(),
                    object_bytes: bytes.len() as u64,
                    sha256,
                    last_modified: object.last_modified.clone(),
                }),
            };
            // The operator-selected resolution and radius are the resource
            // contract. The follower does not impose a second hidden cell cap.
            let stored = ingest_nexrad_level2(store_root, &bytes, &options, usize::MAX)
                .map_err(|error| format!("{} decode/store failed: {error}", object.key))?;
            let snapshot_id = RunSnapshot::open(store_root, &stored.model, &stored.run)
                .map_err(|error| format!("stored NEXRAD snapshot validation failed: {error}"))?
                .descriptor()
                .snapshot_id
                .clone();
            if stored.duplicate {
                duplicates = duplicates.saturating_add(1);
            } else {
                ingested = ingested.saturating_add(1);
            }
            cursor = Some(DurableSiteCursor {
                provider_id: provider.id.clone(),
                object_key: object.key,
                object_valid_unix: object.valid_time.timestamp(),
                latest: NexradLevel2StoredFrameStatus::from_stored(stored, snapshot_id, source),
            });
        }
        Ok(CycleOutcome {
            cursor,
            ingested,
            duplicates,
        })
    }
}

pub struct NexradLevel2IngestSupervisor {
    cancel: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl NexradLevel2IngestSupervisor {
    pub fn start(
        config: &NexradLevel2IngestConfig,
        store_root: &Path,
        monitor: NexradLevel2IngestMonitor,
    ) -> Result<Self, String> {
        let runner = Arc::new(ProductionCycleRunner::new(config)?);
        Self::start_with_runner(config, store_root, monitor, runner)
    }

    fn start_with_runner(
        config: &NexradLevel2IngestConfig,
        store_root: &Path,
        monitor: NexradLevel2IngestMonitor,
        runner: Arc<dyn NexradLevel2CycleRunner>,
    ) -> Result<Self, String> {
        let (cancel, _receiver) = watch::channel(false);
        if !config.enabled {
            return Ok(Self {
                cancel,
                tasks: Vec::new(),
            });
        }
        let durable = load_durable_state(&config.state_root)?;
        monitor.restore(durable, store_root);
        save_durable_state(&config.state_root, &monitor.durable_snapshot())?;

        let save_lock = Arc::new(Mutex::new(()));
        let limiter = Arc::new(Semaphore::new(config.concurrency));
        let mut tasks = Vec::with_capacity(config.sites.len());
        let providers = config
            .providers
            .iter()
            .map(|provider| (provider.id.to_ascii_lowercase(), provider.clone()))
            .collect::<BTreeMap<_, _>>();
        for (index, site) in config.sites.iter().cloned().enumerate() {
            let provider = providers
                .get(&site.provider_id.to_ascii_lowercase())
                .cloned()
                .ok_or_else(|| format!("site '{}' names an absent provider", site.site_id))?;
            let worker = WorkerContext {
                index,
                site,
                provider,
                config: config.clone(),
                store_root: store_root.to_path_buf(),
                monitor: monitor.clone(),
                runner: runner.clone(),
                limiter: limiter.clone(),
                save_lock: save_lock.clone(),
                cancel: cancel.subscribe(),
                wake: monitor.wake_receiver(),
            };
            tasks.push(tokio::spawn(run_worker(worker)));
        }
        Ok(Self { cancel, tasks })
    }

    #[must_use]
    pub fn worker_count(&self) -> usize {
        self.tasks.len()
    }

    pub async fn shutdown(&mut self) {
        self.cancel.send_replace(true);
        for task in self.tasks.drain(..) {
            if let Err(error) = task.await {
                warn!(%error, "NEXRAD Level II ingest worker join failed");
            }
        }
    }
}

impl Drop for NexradLevel2IngestSupervisor {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

struct WorkerContext {
    index: usize,
    site: NexradLevel2SiteConfig,
    provider: NexradLevel2ProviderConfig,
    config: NexradLevel2IngestConfig,
    store_root: PathBuf,
    monitor: NexradLevel2IngestMonitor,
    runner: Arc<dyn NexradLevel2CycleRunner>,
    limiter: Arc<Semaphore>,
    save_lock: Arc<Mutex<()>>,
    cancel: watch::Receiver<bool>,
    wake: watch::Receiver<u64>,
}

async fn run_worker(mut worker: WorkerContext) {
    let mut next_delay = Duration::ZERO;
    loop {
        if wait_for_cycle(&mut worker.cancel, &mut worker.wake, next_delay).await {
            break;
        }
        let permit = tokio::select! {
            result = worker.limiter.clone().acquire_owned() => match result {
                Ok(permit) => permit,
                Err(_) => break,
            },
            changed = worker.cancel.changed() => {
                if changed.is_err() || *worker.cancel.borrow() {
                    break;
                }
                continue;
            }
        };
        let attempted_unix = now_unix();
        let previous = worker.monitor.begin_attempt(worker.index, attempted_unix);
        let runner = worker.runner.clone();
        let store_root = worker.store_root.clone();
        let site = worker.site.clone();
        let provider = worker.provider.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            runner.run(&store_root, &site, &provider, previous, Utc::now())
        })
        .await;
        let completed_unix = now_unix();
        match result {
            Ok(Ok(outcome)) => {
                let newest = outcome
                    .cursor
                    .as_ref()
                    .map(|cursor| cursor.latest.valid_unix);
                let ingested = outcome.ingested;
                worker
                    .monitor
                    .finish_success(worker.index, completed_unix, outcome);
                let save_result = {
                    let _save = lock_unpoisoned(&worker.save_lock);
                    save_durable_state(
                        &worker.config.state_root,
                        &worker.monitor.durable_snapshot(),
                    )
                };
                match save_result {
                    Ok(()) => {
                        next_delay = Duration::from_secs(worker.config.poll_interval_seconds);
                        info!(
                            site = %worker.site.site_id,
                            latest_valid_unix = ?newest,
                            ingested,
                            "NEXRAD Level II background site is current"
                        );
                        if worker.index == 0 && worker.monitor.retention_due(completed_unix) {
                            let cutoff = completed_unix.saturating_sub(
                                i64::try_from(worker.config.retention_hours.saturating_mul(3600))
                                    .unwrap_or(i64::MAX),
                            );
                            match prune_expired_nexrad_runs(
                                &worker.store_root,
                                cutoff,
                                &worker.config.sites,
                            ) {
                                Ok(removed) => {
                                    worker.monitor.record_retention(completed_unix, removed)
                                }
                                Err(error) => {
                                    warn!(%error, "NEXRAD Level II retention pass failed")
                                }
                            }
                        }
                    }
                    Err(error) => {
                        next_delay = worker.monitor.finish_failure(
                            worker.index,
                            completed_unix,
                            format!("durable cursor commit failed: {error}"),
                        );
                    }
                }
            }
            Ok(Err(error)) => {
                warn!(site = %worker.site.site_id, %error, "NEXRAD Level II background cycle failed");
                next_delay = worker
                    .monitor
                    .finish_failure(worker.index, completed_unix, error);
            }
            Err(error) => {
                warn!(site = %worker.site.site_id, %error, "NEXRAD Level II blocking worker failed");
                next_delay = worker.monitor.finish_failure(
                    worker.index,
                    completed_unix,
                    format!("blocking worker failed: {error}"),
                );
            }
        }
        worker.monitor.mark_waiting(worker.index);
    }
    worker.monitor.mark_stopped(worker.index);
}

async fn wait_for_cycle(
    cancel: &mut watch::Receiver<bool>,
    wake: &mut watch::Receiver<u64>,
    delay: Duration,
) -> bool {
    if *cancel.borrow() {
        return true;
    }
    if delay.is_zero() {
        return false;
    }
    tokio::select! {
        changed = cancel.changed() => changed.is_err() || *cancel.borrow(),
        changed = wake.changed() => {
            let _ = changed;
            false
        },
        () = tokio::time::sleep(delay) => false,
    }
}

fn retry_delay(config: &NexradLevel2IngestConfig, failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(20);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_secs(
        config
            .poll_interval_seconds
            .saturating_mul(multiplier)
            .min(config.maximum_backoff_seconds),
    )
}

fn discover_objects(
    agent: &ureq::Agent,
    provider: &NexradLevel2ProviderConfig,
    site: &NexradLevel2SiteConfig,
    previous: Option<&DurableSiteCursor>,
    now: DateTime<Utc>,
    config: &NexradLevel2IngestConfig,
) -> Result<Vec<ArchiveObject>, String> {
    let threshold =
        now - chrono::Duration::hours(i64::try_from(config.catch_up_hours).unwrap_or(i64::MAX));
    let cursor_time =
        previous.and_then(|cursor| Utc.timestamp_opt(cursor.object_valid_unix, 0).single());
    let start_time = cursor_time.unwrap_or(threshold).max(threshold);
    let mut date = start_time.date_naive();
    let end = now.date_naive();
    let site_id = site.site_id.to_ascii_uppercase();
    let mut objects = Vec::new();
    while date <= end {
        let start_after = previous
            .filter(|cursor| object_key_date(&cursor.object_key) == Some(date))
            .map(|cursor| cursor.object_key.as_str());
        objects.extend(list_archive_day(
            agent,
            provider,
            &site_id,
            date,
            start_after,
            config,
        )?);
        date = date
            .succ_opt()
            .ok_or_else(|| "NEXRAD listing date overflow".to_owned())?;
    }
    objects.retain(|object| {
        object.valid_time >= threshold
            && previous.is_none_or(|cursor| {
                (object.valid_time.timestamp(), object.key.as_str())
                    > (cursor.object_valid_unix, cursor.object_key.as_str())
            })
    });
    objects.sort_by(|left, right| {
        left.valid_time
            .cmp(&right.valid_time)
            .then(left.key.cmp(&right.key))
    });
    objects.dedup_by(|left, right| left.key == right.key);
    Ok(objects)
}

fn list_archive_day(
    agent: &ureq::Agent,
    provider: &NexradLevel2ProviderConfig,
    site: &str,
    date: NaiveDate,
    start_after: Option<&str>,
    config: &NexradLevel2IngestConfig,
) -> Result<Vec<ArchiveObject>, String> {
    let prefix = format!(
        "{:04}/{:02}/{:02}/{site}/",
        date.year(),
        date.month(),
        date.day()
    );
    let mut continuation: Option<String> = None;
    let mut seen_tokens = BTreeSet::new();
    let mut objects = Vec::new();
    loop {
        let mut url = format!(
            "{}/?list-type=2&prefix={}",
            provider.listing_base_url.trim_end_matches('/'),
            percent_encode(prefix.as_bytes())
        );
        if let Some(start_after) = start_after.filter(|_| continuation.is_none()) {
            url.push_str("&start-after=");
            url.push_str(&percent_encode(start_after.as_bytes()));
        }
        if let Some(token) = &continuation {
            url.push_str("&continuation-token=");
            url.push_str(&percent_encode(token.as_bytes()));
        }
        let document = request_with_retries(config.request_retries, || {
            fetch_document(
                agent,
                &url,
                "application/xml,text/xml",
                config.maximum_listing_bytes,
                "NEXRAD archive listing",
            )
        })?;
        let text = std::str::from_utf8(&document)
            .map_err(|_| "NEXRAD archive listing is not UTF-8 XML".to_owned())?;
        let page: S3ListBucketResult = from_str(text)
            .map_err(|error| format!("NEXRAD archive listing XML failed: {error}"))?;
        for entry in page.contents {
            if let Some(object) = archive_object(site, entry) {
                objects.push(object);
            }
        }
        if !page.is_truncated {
            break;
        }
        let next = page.next_continuation_token.ok_or_else(|| {
            "truncated NEXRAD archive listing omitted its continuation token".to_owned()
        })?;
        if next.is_empty() || !seen_tokens.insert(next.clone()) {
            return Err("NEXRAD archive listing repeated an invalid continuation token".into());
        }
        continuation = Some(next);
    }
    Ok(objects)
}

#[derive(Debug, Deserialize)]
struct S3ListBucketResult {
    #[serde(rename = "Contents", default)]
    contents: Vec<S3ObjectEntry>,
    #[serde(rename = "IsTruncated", default)]
    is_truncated: bool,
    #[serde(rename = "NextContinuationToken")]
    next_continuation_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct S3ObjectEntry {
    #[serde(rename = "Key")]
    key: String,
    #[serde(rename = "Size")]
    size: u64,
    #[serde(rename = "LastModified")]
    last_modified: Option<String>,
}

fn archive_object(site: &str, entry: S3ObjectEntry) -> Option<ArchiveObject> {
    if entry.size == 0 || entry.key.contains("_MDM") {
        return None;
    }
    let valid_time = parse_archive_key_time(site, &entry.key)?;
    Some(ArchiveObject {
        key: entry.key,
        bytes: entry.size,
        last_modified: entry.last_modified,
        valid_time,
    })
}

fn parse_archive_key_time(site: &str, key: &str) -> Option<DateTime<Utc>> {
    let name = key.rsplit('/').next()?;
    let prefix = site.to_ascii_uppercase();
    let rest = name.strip_prefix(&prefix)?;
    let timestamp = rest.get(..15)?;
    if timestamp.as_bytes().get(8) != Some(&b'_') {
        return None;
    }
    let suffix = rest.get(15..)?;
    if !suffix.starts_with("_V") || suffix.ends_with("_MDM") {
        return None;
    }
    chrono::NaiveDateTime::parse_from_str(timestamp, "%Y%m%d_%H%M%S")
        .ok()
        .map(|value| value.and_utc())
}

fn object_key_date(key: &str) -> Option<NaiveDate> {
    let mut parts = key.split('/');
    let year = parts.next()?.parse().ok()?;
    let month = parts.next()?.parse().ok()?;
    let day = parts.next()?.parse().ok()?;
    NaiveDate::from_ymd_opt(year, month, day)
}

fn download_object(
    agent: &ureq::Agent,
    provider: &NexradLevel2ProviderConfig,
    object: &ArchiveObject,
    maximum: usize,
) -> Result<Vec<u8>, String> {
    if object.bytes == 0 || object.bytes > maximum as u64 {
        return Err(format!(
            "NEXRAD object '{}' advertises {} bytes outside the configured 1..={maximum} byte network guard",
            object.key, object.bytes
        ));
    }
    let encoded_key = object
        .key
        .split('/')
        .map(|segment| percent_encode(segment.as_bytes()))
        .collect::<Vec<_>>()
        .join("/");
    let url = format!(
        "{}/{}",
        provider.object_base_url.trim_end_matches('/'),
        encoded_key
    );
    let bytes = fetch_document(
        agent,
        &url,
        "application/octet-stream,*/*",
        maximum,
        "NEXRAD Level II object",
    )?;
    if bytes.len() as u64 != object.bytes {
        return Err(format!(
            "NEXRAD object '{}' length changed between listing ({}) and download ({})",
            object.key,
            object.bytes,
            bytes.len()
        ));
    }
    Ok(bytes)
}

fn fetch_document(
    agent: &ureq::Agent,
    url: &str,
    accept: &str,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, String> {
    let mut response = agent
        .get(url)
        .header("accept", accept)
        .header("accept-encoding", "identity")
        .header("user-agent", "rusty-weather-nexrad-level2-ingest/1")
        .call()
        .map_err(|error| format!("{label} request failed: {error}"))?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) || response.headers().contains_key("location") {
        return Err(format!("{label} returned status {status}"));
    }
    if response.headers().contains_key("content-encoding") {
        return Err(format!("{label} returned unexpected HTTP content encoding"));
    }
    if let Some(length) = response.headers().get("content-length") {
        let length = length
            .to_str()
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| format!("{label} content-length is invalid"))?;
        if length == 0 || length > maximum {
            return Err(format!("{label} must be 1..={maximum} bytes"));
        }
    }
    read_bounded(response.body_mut().as_reader(), maximum, label)
}

fn read_bounded(mut reader: impl Read, maximum: usize, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; HTTP_READ_CHUNK.min(maximum.saturating_add(1))];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("failed to read {label}: {error}"))?;
        if read == 0 {
            break;
        }
        if bytes.len().saturating_add(read) > maximum {
            return Err(format!("{label} exceeds {maximum} bytes"));
        }
        bytes
            .try_reserve(read)
            .map_err(|_| format!("could not allocate bounded {label}"))?;
        bytes.extend_from_slice(&buffer[..read]);
    }
    if bytes.is_empty() {
        Err(format!("{label} is empty"))
    } else {
        Ok(bytes)
    }
}

fn request_with_retries<T>(
    retries: u32,
    mut request: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let mut errors = Vec::new();
    for attempt in 0..=retries {
        match request() {
            Ok(value) => return Ok(value),
            Err(error) => errors.push(format!("attempt {}: {error}", attempt + 1)),
        }
    }
    Err(errors.join("; "))
}

fn hardened_agent(
    provider: &NexradLevel2ProviderConfig,
    timeout_seconds: u64,
) -> Result<ureq::Agent, String> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let timeout = Duration::from_secs(timeout_seconds);
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(!provider.allow_http)
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(4)
        .timeout_global(Some(timeout))
        .timeout_per_call(Some(timeout))
        .timeout_resolve(Some(timeout))
        .timeout_connect(Some(timeout))
        .timeout_send_request(Some(timeout))
        .timeout_send_body(Some(timeout))
        .timeout_recv_response(Some(timeout))
        .timeout_recv_body(Some(timeout))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .build(),
        )
        .build();
    Ok(ureq::Agent::new_with_config(config))
}

fn percent_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(bytes.len());
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[(byte >> 4) as usize]));
            encoded.push(char::from(HEX[(byte & 0x0f) as usize]));
        }
    }
    encoded
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
    }
    output
}

fn load_durable_state(root: &Path) -> Result<DurableState, String> {
    ensure_state_root(root)?;
    let path = root.join(STATE_FILE);
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DurableState::default());
        }
        Err(error) => return Err(format!("failed to inspect NEXRAD state: {error}")),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("NEXRAD durable state must be a regular non-symlink file".into());
    }
    let bytes = fs::read(&path).map_err(|error| format!("failed to read NEXRAD state: {error}"))?;
    let state: DurableState =
        serde_json::from_slice(&bytes).map_err(|error| format!("NEXRAD state JSON: {error}"))?;
    if state.schema != STATE_SCHEMA {
        return Err(format!(
            "unsupported NEXRAD state schema '{}'",
            state.schema
        ));
    }
    Ok(state)
}

fn save_durable_state(root: &Path, state: &DurableState) -> Result<(), String> {
    ensure_state_root(root)?;
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|error| format!("failed to encode NEXRAD state: {error}"))?;
    atomic_write_bytes(&root.join(STATE_FILE), &bytes)
        .map_err(|error| format!("failed to commit NEXRAD state: {error}"))
}

fn ensure_state_root(root: &Path) -> Result<(), String> {
    if let Ok(metadata) = fs::symlink_metadata(root)
        && (metadata.file_type().is_symlink() || !metadata.is_dir())
    {
        return Err("NEXRAD state root must be a real directory".into());
    }
    fs::create_dir_all(root)
        .map_err(|error| format!("failed to create NEXRAD state root: {error}"))?;
    let metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("failed to inspect NEXRAD state root: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("NEXRAD state root must be a real directory".into());
    }
    Ok(())
}

fn prune_expired_nexrad_runs(
    store_root: &Path,
    cutoff_unix: i64,
    configured_sites: &[NexradLevel2SiteConfig],
) -> std::io::Result<u64> {
    let model_root = store_root.join(RADAR_MODEL);
    let prefixes = configured_sites
        .iter()
        .map(|site| {
            format!(
                "{}-ref-lowest-",
                rw_observations::sanitize_token(&site.site_id)
            )
        })
        .collect::<Vec<_>>();
    let entries = match fs::read_dir(&model_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0u64;
    for entry in entries {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(RETIRED_RUN_PREFIX) {
            if fs::remove_dir_all(entry.path()).is_ok() {
                removed = removed.saturating_add(1);
            }
            continue;
        }
        if !prefixes.iter().any(|prefix| name.starts_with(prefix)) {
            continue;
        }
        let run_dir = entry.path();
        let manifest =
            match RwsRunManifest::load_for_run(&run_dir.join("run.json"), RADAR_MODEL, &name) {
                Ok(manifest) => manifest,
                Err(_) => continue,
            };
        let newest = manifest
            .hours
            .values()
            .map(|hour| hour.valid_unix)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().max());
        if newest.is_none_or(|valid| valid >= cutoff_unix) {
            continue;
        }
        let Some(lock) = RunLock::try_acquire(&run_dir).map_err(std::io::Error::other)? else {
            continue;
        };
        let locked =
            match RwsRunManifest::load_for_run(&run_dir.join("run.json"), RADAR_MODEL, &name) {
                Ok(manifest) => manifest,
                Err(_) => {
                    drop(lock);
                    continue;
                }
            };
        let still_expired = locked
            .hours
            .values()
            .map(|hour| hour.valid_unix)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().max())
            .is_some_and(|valid| valid < cutoff_unix);
        if !still_expired {
            drop(lock);
            continue;
        }
        let digest = blake3::hash(name.as_bytes()).to_hex();
        let retired = model_root.join(format!(
            "{RETIRED_RUN_PREFIX}{}-{}",
            &digest[..16],
            now_unix()
        ));
        #[cfg(windows)]
        drop(lock);
        let renamed = fs::rename(&run_dir, &retired);
        #[cfg(not(windows))]
        drop(lock);
        if renamed.is_err() {
            continue;
        }
        if fs::remove_dir_all(retired).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

fn bound_error(mut error: String) -> String {
    if error.len() <= MAX_STATUS_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_STATUS_ERROR_BYTES;
    while !error.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    error.truncate(end);
    error.push('…');
    error
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

pub(crate) fn router(state: AppState) -> Router<AppState> {
    if !state.config.nexrad_level2_ingest.enabled {
        return Router::new();
    }
    Router::new()
        .route("/v1/observations/nexrad/level2/ingest/status", get(status))
        .route(
            "/v1/observations/nexrad/level2/ingest/refresh",
            post(refresh),
        )
}

async fn status(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    private_json(StatusCode::OK, state.nexrad_level2_ingest.status())
}

async fn refresh(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let wake_epoch = state.nexrad_level2_ingest.request_refresh();
    private_json(
        StatusCode::ACCEPTED,
        NexradLevel2RefreshResponse {
            schema: REFRESH_SCHEMA,
            accepted: true,
            wake_epoch,
        },
    )
}

fn private_json<T: Serialize>(status: StatusCode, value: T) -> Response {
    let mut response = (status, Json(value)).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    fn provider(base: &str) -> NexradLevel2ProviderConfig {
        NexradLevel2ProviderConfig {
            id: "fixture-provider".into(),
            listing_base_url: base.into(),
            object_base_url: base.into(),
            attribution: "Fixture archive".into(),
            allow_http: true,
        }
    }

    fn site() -> NexradLevel2SiteConfig {
        NexradLevel2SiteConfig {
            site_id: "KTLX".into(),
            provider_id: "fixture-provider".into(),
            resolution_m: 250.0,
            radius_km: 230.0,
            latitude: None,
            longitude: None,
            elevation_m: None,
        }
    }

    #[test]
    fn archive_listing_parser_rejects_metadata_and_preserves_exact_identity() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult>
  <IsTruncated>false</IsTruncated>
  <Contents><Key>2026/08/23/KTLX/KTLX20260823_080001_V06</Key><LastModified>2026-08-23T08:05:00.000Z</LastModified><Size>12345</Size></Contents>
  <Contents><Key>2026/08/23/KTLX/KTLX20260823_080001_V06_MDM</Key><LastModified>2026-08-23T08:06:00.000Z</LastModified><Size>700000</Size></Contents>
</ListBucketResult>"#;
        let page: S3ListBucketResult = from_str(xml).unwrap();
        let objects = page
            .contents
            .into_iter()
            .filter_map(|entry| archive_object("KTLX", entry))
            .collect::<Vec<_>>();
        assert_eq!(objects.len(), 1);
        assert_eq!(objects[0].key, "2026/08/23/KTLX/KTLX20260823_080001_V06");
        assert_eq!(objects[0].bytes, 12_345);
        assert_eq!(objects[0].valid_time.timestamp(), 1_787_472_001);
    }

    #[test]
    fn listing_and_object_fetch_use_mock_http_and_bounded_exact_bytes() {
        let object = b"fixture-level2-object".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let object_for_server = object.clone();
        let server = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                reader.read_line(&mut first).unwrap();
                let path = first.split_whitespace().nth(1).unwrap().to_string();
                seen.lock().unwrap().push(path.clone());
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                let body = if path.starts_with("/?list-type=2") {
                    format!(
                        "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>2026/08/23/KTLX/KTLX20260823_080001_V06</Key><LastModified>2026-08-23T08:05:00Z</LastModified><Size>{}</Size></Contents></ListBucketResult>",
                        object_for_server.len()
                    )
                    .into_bytes()
                } else {
                    object_for_server.clone()
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        let provider = provider(&base);
        let config = NexradLevel2IngestConfig {
            request_retries: 0,
            maximum_listing_bytes: 32 * 1024,
            maximum_object_bytes: 32 * 1024,
            ..NexradLevel2IngestConfig::default()
        };
        let agent = hardened_agent(&provider, 5).unwrap();
        let objects = list_archive_day(
            &agent,
            &provider,
            "KTLX",
            NaiveDate::from_ymd_opt(2026, 8, 23).unwrap(),
            None,
            &config,
        )
        .unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(
            download_object(&agent, &provider, &objects[0], 32 * 1024).unwrap(),
            object
        );
        server.join().unwrap();
        let requests = requests.lock().unwrap();
        assert!(requests[0].contains("prefix=2026%2F08%2F23%2FKTLX%2F"));
        assert_eq!(requests[1], "/2026/08/23/KTLX/KTLX20260823_080001_V06");
    }

    #[test]
    fn durable_state_refuses_symlinks_and_round_trips_exact_cursor() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("state");
        let cursor = DurableSiteCursor {
            provider_id: "fixture-provider".into(),
            object_key: "2026/08/23/KTLX/KTLX20260823_080001_V06".into(),
            object_valid_unix: 1_787_472_001,
            latest: NexradLevel2StoredFrameStatus {
                model: RADAR_MODEL.into(),
                run: "ktlx-ref-lowest-test".into(),
                snapshot_id: "a".repeat(64),
                storage_slot: 0,
                valid_unix: 1_787_472_001,
                variables: vec!["radar_reflectivity".into()],
                grid_hash: "b".repeat(64),
                duplicate: false,
                source: NexradLevel2SourceObjectStatus {
                    provider_id: "fixture-provider".into(),
                    object_key: "2026/08/23/KTLX/KTLX20260823_080001_V06".into(),
                    object_bytes: 10,
                    sha256: "c".repeat(64),
                    last_modified: None,
                },
            },
        };
        let state = DurableState {
            schema: STATE_SCHEMA.into(),
            sites: BTreeMap::from([("KTLX".into(), cursor.clone())]),
        };
        save_durable_state(&root, &state).unwrap();
        assert_eq!(load_durable_state(&root).unwrap().sites["KTLX"], cursor);

        #[cfg(unix)]
        {
            let link = directory.path().join("linked-state");
            std::os::unix::fs::symlink(&root, &link).unwrap();
            assert!(load_durable_state(&link).is_err());
        }
    }

    #[test]
    fn no_site_count_or_resolution_ceiling_is_added_by_follower_config() {
        let config = NexradLevel2IngestConfig {
            enabled: true,
            providers: vec![provider("http://127.0.0.1:1")],
            sites: (0..128)
                .map(|index| NexradLevel2SiteConfig {
                    site_id: format!("X{index}"),
                    provider_id: "fixture-provider".into(),
                    resolution_m: if index == 0 { 25.0 } else { 250.0 },
                    ..site()
                })
                .collect(),
            ..NexradLevel2IngestConfig::default()
        };
        let app = crate::config::AppConfig {
            nexrad_level2_ingest: config,
            ..crate::config::AppConfig::default()
        };
        app.validate(true).unwrap();
    }
}

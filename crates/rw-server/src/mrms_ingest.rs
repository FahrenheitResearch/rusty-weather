//! Server-owned, request-independent NOAA MRMS ingestion.
//!
//! The follower deliberately reuses `rw-observations` for download, GRIB
//! decoding, missing-value normalization, and exact-time storage. This module
//! owns only lifecycle, bounded scheduling, freshness, and conservative
//! retention. A coalescing watch epoch lets operator refresh requests wake the
//! existing workers; it never creates one fetch job per HTTP client.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rw_observations::{
    MrmsIngestRequest, MrmsMessageSelector, StoredFrameRef, StoredPlaneRef,
    fetch_mrms_frame_with_policy, write_observation_frame_with_limit,
};
use rw_query::RunSnapshot;
use rw_store::lock::RunLock;
use rw_store::run::RwsRunManifest;
use serde::Serialize;
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;
use tracing::{info, warn};
use utoipa::ToSchema;

use crate::AppState;
use crate::config::{MrmsFollowSpec, MrmsIngestConfig};

const STATUS_SCHEMA: &str = "rw-server.mrms-ingest-status.v1";
const REFRESH_SCHEMA: &str = "rw-server.mrms-ingest-refresh.v1";
const MRMS_MODEL: &str = "obs-mrms";
const RETIRED_RUN_PREFIX: &str = ".rw-mrms-retired-";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum MrmsProductPhase {
    Starting,
    Waiting,
    Fetching,
    Ready,
    Degraded,
    Stopped,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MrmsStoredFrameStatus {
    pub model: String,
    pub run: String,
    /// Canonical validated `rw-query` identity for the complete run snapshot
    /// containing this frame. Snapshot-bound analysis requests must echo it.
    pub snapshot_id: String,
    pub storage_slot: u16,
    pub valid_unix: i64,
    pub variables: Vec<String>,
    pub grid_hash: String,
    pub duplicate: bool,
}

impl MrmsStoredFrameStatus {
    fn from_stored(frame: StoredFrameRef, snapshot_id: String) -> Self {
        Self {
            model: frame.model,
            run: frame.run,
            snapshot_id,
            storage_slot: frame.storage_slot,
            valid_unix: frame.valid_unix,
            variables: frame.variables,
            grid_hash: frame.grid_hash,
            duplicate: frame.duplicate,
        }
    }
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MrmsProductStatus {
    pub product: String,
    pub collection: String,
    pub variable: String,
    pub phase: MrmsProductPhase,
    pub attempts: u64,
    pub consecutive_failures: u32,
    pub last_attempt_unix: Option<i64>,
    pub last_success_unix: Option<i64>,
    pub latest_valid_unix: Option<i64>,
    pub source_age_seconds: Option<i64>,
    /// Effective freshness window for this product: its configured
    /// per-product override, or the worker-level `stale_after_seconds`.
    pub stale_after_seconds: u64,
    pub fresh: bool,
    pub next_attempt_unix: Option<i64>,
    pub latest: Option<MrmsStoredFrameStatus>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MrmsIngestStatus {
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
    pub retention_hours: u64,
    pub in_flight: usize,
    pub wake_epoch: u64,
    pub last_retention_unix: Option<i64>,
    pub retention_removed_runs: u64,
    pub products: Vec<MrmsProductStatus>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MrmsRefreshResponse {
    pub schema: &'static str,
    pub accepted: bool,
    pub wake_epoch: u64,
}

/// Exact identity of one fresh native MRMS plane already in `rw-store`.
///
/// Server-side consumers such as automatic storm-cell analysis should ask the
/// monitor for this identity and pass [`Self::stored_plane_ref`] to
/// `rw_observations::read_stored_plane`. That path reuses the follower's
/// authoritative decoded frame and never starts another NOAA download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MrmsLatestPlaneIdentity {
    pub product: String,
    pub collection: String,
    pub variable: String,
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub storage_slot: u16,
    pub valid_unix: i64,
    pub grid_hash: String,
    pub source_age_seconds: i64,
}

impl MrmsLatestPlaneIdentity {
    #[must_use]
    pub fn stored_plane_ref(&self) -> StoredPlaneRef {
        StoredPlaneRef {
            model: self.model.clone(),
            run: self.run.clone(),
            storage_slot: self.storage_slot,
            variable: self.variable.clone(),
        }
    }
}

#[derive(Debug)]
struct ProductRuntime {
    spec: MrmsFollowSpec,
    phase: MrmsProductPhase,
    attempts: u64,
    consecutive_failures: u32,
    last_attempt_unix: Option<i64>,
    last_success_unix: Option<i64>,
    latest_valid_unix: Option<i64>,
    next_attempt_unix: Option<i64>,
    latest: Option<MrmsStoredFrameStatus>,
    last_error: Option<String>,
}

#[derive(Debug)]
struct RuntimeState {
    products: Vec<ProductRuntime>,
    in_flight: usize,
    last_retention_unix: Option<i64>,
    retention_removed_runs: u64,
}

#[derive(Clone, Debug)]
pub struct MrmsIngestMonitor {
    config: Arc<MrmsIngestConfig>,
    state: Arc<Mutex<RuntimeState>>,
    wake: watch::Sender<u64>,
    committed: watch::Sender<u64>,
}

impl MrmsIngestMonitor {
    pub fn new(config: &MrmsIngestConfig) -> Self {
        let products = config
            .products
            .iter()
            .cloned()
            .map(|spec| ProductRuntime {
                spec,
                phase: if config.enabled {
                    MrmsProductPhase::Starting
                } else {
                    MrmsProductPhase::Stopped
                },
                attempts: 0,
                consecutive_failures: 0,
                last_attempt_unix: None,
                last_success_unix: None,
                latest_valid_unix: None,
                next_attempt_unix: None,
                latest: None,
                last_error: None,
            })
            .collect();
        let (wake, _receiver) = watch::channel(0);
        let (committed, _receiver) = watch::channel(0);
        Self {
            config: Arc::new(config.clone()),
            state: Arc::new(Mutex::new(RuntimeState {
                products,
                in_flight: 0,
                last_retention_unix: None,
                retention_removed_runs: 0,
            })),
            wake,
            committed,
        }
    }

    pub fn status(&self) -> MrmsIngestStatus {
        self.status_at(now_unix())
    }

    /// Per-product freshness window: the product's configured override, or the
    /// worker-level default.
    fn effective_stale_after_seconds(&self, spec: &MrmsFollowSpec) -> u64 {
        spec.stale_after_seconds
            .unwrap_or(self.config.stale_after_seconds)
    }

    fn status_at(&self, checked_unix: i64) -> MrmsIngestStatus {
        let state = lock_unpoisoned(&self.state);
        let products = state
            .products
            .iter()
            .map(|product| {
                let source_age_seconds = product
                    .latest_valid_unix
                    .map(|valid| checked_unix.saturating_sub(valid));
                let stale_after_seconds = self.effective_stale_after_seconds(&product.spec);
                let fresh = source_age_seconds.is_some_and(|age| {
                    // A small negative age tolerates host/upstream clock skew
                    // without accepting a manifest implausibly far ahead.
                    age >= -300 && age <= stale_after_seconds as i64
                });
                MrmsProductStatus {
                    product: product.spec.product.clone(),
                    collection: product.spec.collection.clone(),
                    variable: product.spec.variable.clone(),
                    phase: product.phase,
                    attempts: product.attempts,
                    consecutive_failures: product.consecutive_failures,
                    last_attempt_unix: product.last_attempt_unix,
                    last_success_unix: product.last_success_unix,
                    latest_valid_unix: product.latest_valid_unix,
                    source_age_seconds,
                    stale_after_seconds,
                    fresh,
                    next_attempt_unix: product.next_attempt_unix,
                    latest: product.latest.clone(),
                    last_error: product.last_error.clone(),
                }
            })
            .collect::<Vec<_>>();
        let ready = !self.config.enabled
            || (!products.is_empty()
                && products
                    .iter()
                    .all(|product| product.fresh && product.phase != MrmsProductPhase::Stopped));
        MrmsIngestStatus {
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
            retention_hours: self.config.retention_hours,
            in_flight: state.in_flight,
            wake_epoch: *self.wake.borrow(),
            last_retention_unix: state.last_retention_unix,
            retention_removed_runs: state.retention_removed_runs,
            products,
        }
    }

    pub fn is_ready(&self) -> bool {
        self.status().ready
    }

    /// Whether MRMS freshness has been explicitly promoted from subsystem
    /// health to a deployment-wide traffic gate.
    #[must_use]
    pub fn server_readiness_ok(&self) -> bool {
        !self.config.gate_server_readiness || self.is_ready()
    }

    /// Whether the optional follower should be surfaced as degraded while the
    /// core server remains usable. A failed cycle is visible immediately even
    /// while its last successful frame is still within the freshness window.
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        let status = self.status();
        status.enabled
            && (!status.ready
                || status
                    .products
                    .iter()
                    .any(|product| product.phase == MrmsProductPhase::Degraded))
    }

    /// Return every configured plane whose already-stored source frame is
    /// currently fresh. The result is a point-in-time snapshot and performs no
    /// I/O; callers should handle a plane aging out or being retained between
    /// this lookup and their store read.
    #[must_use]
    pub fn latest_fresh_planes(&self) -> Vec<MrmsLatestPlaneIdentity> {
        self.latest_fresh_planes_at(now_unix())
    }

    fn latest_fresh_planes_at(&self, checked_unix: i64) -> Vec<MrmsLatestPlaneIdentity> {
        if !self.config.enabled {
            return Vec::new();
        }
        lock_unpoisoned(&self.state)
            .products
            .iter()
            .filter_map(|product| {
                let latest = product.latest.as_ref()?;
                let valid_unix = product.latest_valid_unix?;
                let stale_after_seconds = self.effective_stale_after_seconds(&product.spec) as i64;
                let source_age_seconds = checked_unix.saturating_sub(valid_unix);
                if !(-300..=stale_after_seconds).contains(&source_age_seconds)
                    || latest.valid_unix != valid_unix
                    || !latest
                        .variables
                        .iter()
                        .any(|name| name == &product.spec.variable)
                {
                    return None;
                }
                Some(MrmsLatestPlaneIdentity {
                    product: product.spec.product.clone(),
                    collection: product.spec.collection.clone(),
                    variable: product.spec.variable.clone(),
                    model: latest.model.clone(),
                    run: latest.run.clone(),
                    snapshot_id: latest.snapshot_id.clone(),
                    storage_slot: latest.storage_slot,
                    valid_unix,
                    grid_hash: latest.grid_hash.clone(),
                    source_age_seconds,
                })
            })
            .collect()
    }

    pub fn request_refresh(&self) -> u64 {
        self.wake
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        *self.wake.borrow()
    }

    fn wake_receiver(&self) -> watch::Receiver<u64> {
        self.wake.subscribe()
    }

    /// Coalescing notification emitted only after a new exact MRMS frame has
    /// been committed and its immutable snapshot identity has been reopened.
    /// Derived workers rescan the store on wake instead of trusting event
    /// payloads, which makes restart reconciliation and live updates share one
    /// source-of-truth path.
    pub fn committed_receiver(&self) -> watch::Receiver<u64> {
        self.committed.subscribe()
    }

    #[cfg(test)]
    pub(crate) fn notify_committed_for_test(&self) {
        self.committed
            .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }

    fn begin_attempt(&self, index: usize, attempted_unix: i64) -> Option<i64> {
        let mut state = lock_unpoisoned(&self.state);
        if index >= state.products.len() {
            return None;
        }
        state.in_flight = state.in_flight.saturating_add(1);
        let product = state.products.get_mut(index)?;
        product.phase = MrmsProductPhase::Fetching;
        product.attempts = product.attempts.saturating_add(1);
        product.last_attempt_unix = Some(attempted_unix);
        product.next_attempt_unix = None;
        product.latest_valid_unix
    }

    fn finish_success(&self, index: usize, completed_unix: i64, outcome: CycleOutcome) {
        let committed_new_frame = outcome.stored.is_some();
        let mut state = lock_unpoisoned(&self.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        if let Some(product) = state.products.get_mut(index) {
            product.phase = MrmsProductPhase::Ready;
            product.consecutive_failures = 0;
            product.last_success_unix = Some(completed_unix);
            product.latest_valid_unix = Some(outcome.valid_unix);
            if let Some(stored) = outcome.stored {
                product.latest = Some(MrmsStoredFrameStatus::from_stored(
                    stored.frame,
                    stored.snapshot_id,
                ));
            }
            product.last_error = None;
            product.next_attempt_unix =
                Some(completed_unix.saturating_add(self.config.poll_interval_seconds as i64));
        }
        drop(state);
        if committed_new_frame {
            self.committed
                .send_modify(|epoch| *epoch = epoch.wrapping_add(1));
        }
    }

    fn finish_failure(&self, index: usize, completed_unix: i64, error: String) -> Duration {
        let mut state = lock_unpoisoned(&self.state);
        state.in_flight = state.in_flight.saturating_sub(1);
        let Some(product) = state.products.get_mut(index) else {
            return Duration::from_secs(self.config.maximum_backoff_seconds);
        };
        product.phase = MrmsProductPhase::Degraded;
        product.consecutive_failures = product.consecutive_failures.saturating_add(1);
        product.last_error = Some(bound_error(error));
        let delay = retry_delay(&self.config, product.consecutive_failures);
        product.next_attempt_unix = Some(completed_unix.saturating_add(delay.as_secs() as i64));
        delay
    }

    fn mark_waiting(&self, index: usize) {
        let mut state = lock_unpoisoned(&self.state);
        if let Some(product) = state.products.get_mut(index)
            && product.phase != MrmsProductPhase::Degraded
        {
            product.phase = MrmsProductPhase::Waiting;
        }
    }

    fn mark_stopped(&self, index: usize) {
        let mut state = lock_unpoisoned(&self.state);
        if let Some(product) = state.products.get_mut(index) {
            product.phase = MrmsProductPhase::Stopped;
            product.next_attempt_unix = None;
        }
    }

    fn record_retention(&self, completed_unix: i64, removed_runs: u64) {
        let mut state = lock_unpoisoned(&self.state);
        state.last_retention_unix = Some(completed_unix);
        state.retention_removed_runs = state.retention_removed_runs.saturating_add(removed_runs);
    }

    fn retention_due(&self, checked_unix: i64) -> bool {
        lock_unpoisoned(&self.state)
            .last_retention_unix
            .is_none_or(|last| checked_unix.saturating_sub(last) >= 60 * 60)
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug)]
struct CycleOutcome {
    valid_unix: i64,
    stored: Option<CycleStoredFrame>,
}

#[derive(Debug)]
struct CycleStoredFrame {
    frame: StoredFrameRef,
    snapshot_id: String,
}

trait MrmsCycleRunner: Send + Sync + 'static {
    fn run(
        &self,
        store_root: &Path,
        spec: &MrmsFollowSpec,
        previous_valid_unix: Option<i64>,
    ) -> Result<CycleOutcome, String>;
}

struct ProductionCycleRunner {
    request_timeout: Duration,
    request_retries: u32,
}

impl MrmsCycleRunner for ProductionCycleRunner {
    fn run(
        &self,
        store_root: &Path,
        spec: &MrmsFollowSpec,
        previous_valid_unix: Option<i64>,
    ) -> Result<CycleOutcome, String> {
        let request = MrmsIngestRequest {
            product: spec.product.clone(),
            collection: Some(spec.collection.clone()),
            variable: Some(spec.variable.clone()),
            // Scientific units are decoded from the official GRIB metadata;
            // operator configuration cannot overwrite them.
            units: None,
            selector: MrmsMessageSelector::default(),
        };
        let frame =
            fetch_mrms_frame_with_policy(&request, self.request_timeout, self.request_retries)
                .map_err(|error| error.to_string())?;
        if previous_valid_unix == Some(frame.valid_unix) {
            return Ok(CycleOutcome {
                valid_unix: frame.valid_unix,
                stored: None,
            });
        }
        let stored =
            write_observation_frame_with_limit(store_root, &frame, rustwx_core::MAX_GRID_CELLS)
                .map_err(|error| error.to_string())?;
        // Opening the just-published append-only run validates the manifest,
        // grid, time axis, and physical files and computes the canonical
        // snapshot identity used by snapshot-bound scientific requests.
        let snapshot_id = RunSnapshot::open(store_root, &stored.model, &stored.run)
            .map_err(|error| format!("stored MRMS snapshot validation failed: {error}"))?
            .descriptor()
            .snapshot_id
            .clone();
        Ok(CycleOutcome {
            valid_unix: frame.valid_unix,
            stored: Some(CycleStoredFrame {
                frame: stored,
                snapshot_id,
            }),
        })
    }
}

pub struct MrmsIngestSupervisor {
    cancel: watch::Sender<bool>,
    tasks: Vec<JoinHandle<()>>,
}

impl MrmsIngestSupervisor {
    pub fn start(config: &MrmsIngestConfig, store_root: &Path, monitor: MrmsIngestMonitor) -> Self {
        Self::start_with_runner(
            config,
            store_root,
            monitor,
            Arc::new(ProductionCycleRunner {
                request_timeout: Duration::from_secs(config.request_timeout_seconds),
                request_retries: config.request_retries,
            }),
        )
    }

    fn start_with_runner(
        config: &MrmsIngestConfig,
        store_root: &Path,
        monitor: MrmsIngestMonitor,
        runner: Arc<dyn MrmsCycleRunner>,
    ) -> Self {
        let (cancel, _receiver) = watch::channel(false);
        if !config.enabled {
            return Self {
                cancel,
                tasks: Vec::new(),
            };
        }
        let limiter = Arc::new(Semaphore::new(config.concurrency));
        let mut tasks = Vec::with_capacity(config.products.len());
        for (index, spec) in config.products.iter().cloned().enumerate() {
            let worker = WorkerContext {
                index,
                spec,
                config: config.clone(),
                store_root: store_root.to_path_buf(),
                monitor: monitor.clone(),
                runner: runner.clone(),
                limiter: limiter.clone(),
                cancel: cancel.subscribe(),
                wake: monitor.wake_receiver(),
            };
            tasks.push(tokio::spawn(run_worker(worker)));
        }
        Self { cancel, tasks }
    }

    pub fn worker_count(&self) -> usize {
        self.tasks.len()
    }

    pub async fn shutdown(&mut self) {
        self.cancel.send_replace(true);
        for task in self.tasks.drain(..) {
            if let Err(error) = task.await {
                warn!(%error, "MRMS ingest worker join failed");
            }
        }
    }
}

impl Drop for MrmsIngestSupervisor {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
    }
}

struct WorkerContext {
    index: usize,
    spec: MrmsFollowSpec,
    config: MrmsIngestConfig,
    store_root: PathBuf,
    monitor: MrmsIngestMonitor,
    runner: Arc<dyn MrmsCycleRunner>,
    limiter: Arc<Semaphore>,
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
        let previous_valid_unix = worker.monitor.begin_attempt(worker.index, attempted_unix);
        let runner = worker.runner.clone();
        let store_root = worker.store_root.clone();
        let spec = worker.spec.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            runner.run(&store_root, &spec, previous_valid_unix)
        })
        .await;
        let completed_unix = now_unix();
        match result {
            Ok(Ok(outcome)) => {
                let valid_unix = outcome.valid_unix;
                worker
                    .monitor
                    .finish_success(worker.index, completed_unix, outcome);
                next_delay = Duration::from_secs(worker.config.poll_interval_seconds);
                info!(
                    product = %worker.spec.product,
                    valid_unix,
                    "MRMS background product is current"
                );
                if worker.index == 0 && worker.monitor.retention_due(completed_unix) {
                    let cutoff = completed_unix.saturating_sub(
                        i64::try_from(worker.config.retention_hours.saturating_mul(3600))
                            .unwrap_or(i64::MAX),
                    );
                    match prune_expired_mrms_runs(
                        &worker.store_root,
                        cutoff,
                        &worker.config.products,
                    ) {
                        Ok(removed) => worker.monitor.record_retention(completed_unix, removed),
                        Err(error) => warn!(%error, "MRMS retention pass failed"),
                    }
                }
            }
            Ok(Err(error)) => {
                warn!(product = %worker.spec.product, %error, "MRMS background cycle failed");
                next_delay = worker
                    .monitor
                    .finish_failure(worker.index, completed_unix, error);
            }
            Err(error) => {
                warn!(product = %worker.spec.product, %error, "MRMS blocking worker failed");
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

/// Returns true when cancellation wins. Refresh epochs are coalesced: any
/// number of changes since the last cycle produces one immediate next cycle.
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

fn retry_delay(config: &MrmsIngestConfig, failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(20);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    Duration::from_secs(
        config
            .poll_interval_seconds
            .saturating_mul(multiplier)
            .min(config.maximum_backoff_seconds),
    )
}

fn bound_error(mut error: String) -> String {
    const MAX_ERROR_BYTES: usize = 1024;
    if error.len() <= MAX_ERROR_BYTES {
        return error;
    }
    let mut end = MAX_ERROR_BYTES;
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

/// Conservatively retire only complete exact-time MRMS runs whose newest
/// frame is older than the cutoff. The run lock excludes cooperating writers;
/// an atomic same-parent rename removes it from catalogs before recursive
/// deletion. A failed rename is a safe skip, including on restrictive Windows
/// filesystems. Hidden leftovers from an interrupted prior deletion are
/// cleaned on the next pass.
fn prune_expired_mrms_runs(
    store_root: &Path,
    cutoff_unix: i64,
    configured_products: &[MrmsFollowSpec],
) -> std::io::Result<u64> {
    let model_root = store_root.join(MRMS_MODEL);
    let selected_run_prefixes = configured_products
        .iter()
        .map(|spec| {
            format!(
                "{}-{}-",
                rw_observations::sanitize_token(&spec.collection),
                rw_observations::sanitize_token(&spec.product)
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
        if !selected_run_prefixes
            .iter()
            .any(|prefix| name.starts_with(prefix))
        {
            continue;
        }
        let run_dir = entry.path();
        let manifest =
            match RwsRunManifest::load_for_run(&run_dir.join("run.json"), MRMS_MODEL, &name) {
                Ok(manifest) => manifest,
                Err(_) => continue,
            };
        let Some(newest_valid) = manifest
            .hours
            .values()
            .map(|hour| hour.valid_unix)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().max())
        else {
            continue;
        };
        if newest_valid >= cutoff_unix {
            continue;
        }
        let Some(lock) = RunLock::try_acquire(&run_dir).map_err(std::io::Error::other)? else {
            continue;
        };
        // The first manifest read is only a cheap eligibility check. Reload
        // after taking the writer lock so a just-appended frame cannot be
        // retired from a stale pre-lock snapshot.
        let locked_manifest =
            match RwsRunManifest::load_for_run(&run_dir.join("run.json"), MRMS_MODEL, &name) {
                Ok(manifest) => manifest,
                Err(_) => {
                    drop(lock);
                    continue;
                }
            };
        let still_expired = locked_manifest
            .hours
            .values()
            .map(|hour| hour.valid_unix)
            .collect::<Option<Vec<_>>>()
            .and_then(|values| values.into_iter().max())
            .is_some_and(|newest| newest < cutoff_unix);
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
        // Unix permits a directory rename while its advisory lock file is
        // open, which atomically hides the run before releasing the writer.
        // Windows denies that rename while our own handle is open; release it
        // immediately before rename. A competing writer that acquires in that
        // gap keeps its lock handle open and causes a safe rename failure.
        #[cfg(windows)]
        drop(lock);
        let rename_result = fs::rename(&run_dir, &retired);
        #[cfg(not(windows))]
        drop(lock);
        if rename_result.is_err() {
            continue;
        }
        if fs::remove_dir_all(&retired).is_ok() {
            removed = removed.saturating_add(1);
        }
    }
    Ok(removed)
}

pub(crate) fn router(state: AppState) -> Router<AppState> {
    if !state.config.mrms_ingest.enabled {
        return Router::new();
    }
    Router::new()
        .route("/v1/observations/mrms/ingest/status", get(status))
        .route("/v1/observations/mrms/ingest/refresh", post(refresh))
}

async fn status(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    private_json(StatusCode::OK, state.mrms_ingest.status())
}

async fn refresh(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let wake_epoch = state.mrms_ingest.request_refresh();
    private_json(
        StatusCode::ACCEPTED,
        MrmsRefreshResponse {
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use rustwx_core::{GridShape, LatLonGrid};
    use rw_observations::{
        GridPlane, ObservationFamily, ObservationFrame, write_observation_frame,
    };
    use tower::ServiceExt as _;

    use super::*;

    struct FakeRunner {
        calls: AtomicUsize,
        active: AtomicUsize,
        maximum_active: AtomicUsize,
    }

    impl FakeRunner {
        fn new() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                maximum_active: AtomicUsize::new(0),
            }
        }
    }

    impl MrmsCycleRunner for FakeRunner {
        fn run(
            &self,
            _store_root: &Path,
            spec: &MrmsFollowSpec,
            _previous_valid_unix: Option<i64>,
        ) -> Result<CycleOutcome, String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.maximum_active.fetch_max(active, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(25));
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(CycleOutcome {
                valid_unix: now_unix().saturating_add(call as i64),
                stored: Some(CycleStoredFrame {
                    frame: StoredFrameRef {
                        schema: "rw-observations.stored-frame.v1".into(),
                        model: MRMS_MODEL.into(),
                        run: format!("{}-run", spec.collection),
                        storage_slot: u16::try_from(call).unwrap_or(u16::MAX),
                        valid_unix: now_unix(),
                        variables: vec![spec.variable.clone()],
                        grid_hash: "abc".into(),
                        frame_file: "f000.rws".into(),
                        bytes: 10,
                        duplicate: false,
                    },
                    snapshot_id: "a".repeat(64),
                }),
            })
        }
    }

    #[test]
    fn freshness_and_disabled_readiness_are_explicit() {
        let disabled = MrmsIngestMonitor::new(&MrmsIngestConfig::default());
        assert!(disabled.status_at(1_000).ready);

        let config = MrmsIngestConfig {
            enabled: true,
            ..MrmsIngestConfig::default()
        };
        let monitor = MrmsIngestMonitor::new(&config);
        assert!(!monitor.status_at(1_000).ready);
        monitor.begin_attempt(0, 900);
        monitor.finish_success(
            0,
            900,
            CycleOutcome {
                valid_unix: 850,
                stored: None,
            },
        );
        assert!(monitor.status_at(1_000).ready);
        assert!(!monitor.status_at(2_000).ready);
    }

    /// NOAA publishes multi-sensor QPE pass 2 about two hours after its
    /// accumulation window, so it carries a per-product freshness override.
    /// The override must widen only that product's window — the worker-level
    /// window still governs every other product — and a product beyond its
    /// own override is honestly stale again.
    #[test]
    fn per_product_stale_override_governs_only_its_own_product() {
        let mut delayed = MrmsFollowSpec::reflectivity_at_lowest_altitude();
        delayed.product = "MultiSensor_QPE_01H_Pass2".into();
        delayed.variable = "mrms_precip_accum_1h".into();
        delayed.stale_after_seconds = Some(14_400);
        let config = MrmsIngestConfig {
            enabled: true,
            stale_after_seconds: 600,
            products: vec![MrmsFollowSpec::reflectivity_at_lowest_altitude(), delayed],
            ..MrmsIngestConfig::default()
        };
        let monitor = MrmsIngestMonitor::new(&config);
        for index in 0..2 {
            monitor.begin_attempt(index, 10_000);
            monitor.finish_success(
                index,
                10_000,
                CycleOutcome {
                    valid_unix: 10_000,
                    stored: None,
                },
            );
        }

        // Both products fresh inside the worker-level window.
        assert!(monitor.status_at(10_100).ready);

        // Two hours later the QPE override keeps its product fresh while the
        // default-window product is stale, so the worker is not ready.
        let status = monitor.status_at(17_200);
        assert!(!status.ready);
        assert!(!status.products[0].fresh);
        assert_eq!(status.products[0].stale_after_seconds, 600);
        assert!(status.products[1].fresh);
        assert_eq!(status.products[1].stale_after_seconds, 14_400);

        // Beyond its own override the QPE product is honestly stale too.
        assert!(!monitor.status_at(25_000).products[1].fresh);
    }

    #[test]
    fn fresh_plane_identity_reuses_the_exact_stored_native_plane() {
        let config = MrmsIngestConfig {
            enabled: true,
            stale_after_seconds: 600,
            ..MrmsIngestConfig::default()
        };
        let monitor = MrmsIngestMonitor::new(&config);
        monitor.begin_attempt(0, 950);
        monitor.finish_success(
            0,
            960,
            CycleOutcome {
                valid_unix: 900,
                stored: Some(CycleStoredFrame {
                    frame: StoredFrameRef {
                        schema: "rw-observations.stored-frame.v1".into(),
                        model: MRMS_MODEL.into(),
                        run: "mrms-conus-reflectivityatlowestaltitude-20231114".into(),
                        storage_slot: 7,
                        valid_unix: 900,
                        variables: vec!["mrms_reflectivity_lowest_altitude".into()],
                        grid_hash: "native-grid".into(),
                        frame_file: "f007.rws".into(),
                        bytes: 42,
                        duplicate: false,
                    },
                    snapshot_id: "b".repeat(64),
                }),
            },
        );

        let identities = monitor.latest_fresh_planes_at(1_000);
        assert_eq!(identities.len(), 1);
        let identity = &identities[0];
        assert_eq!(identity.product, "ReflectivityAtLowestAltitude");
        assert_eq!(identity.valid_unix, 900);
        assert_eq!(identity.snapshot_id, "b".repeat(64));
        assert_eq!(identity.source_age_seconds, 100);
        assert_eq!(
            monitor.status_at(1_000).products[0]
                .latest
                .as_ref()
                .unwrap()
                .snapshot_id,
            "b".repeat(64)
        );
        assert_eq!(
            identity.stored_plane_ref(),
            StoredPlaneRef {
                model: MRMS_MODEL.into(),
                run: "mrms-conus-reflectivityatlowestaltitude-20231114".into(),
                storage_slot: 7,
                variable: "mrms_reflectivity_lowest_altitude".into(),
            }
        );
        assert!(monitor.latest_fresh_planes_at(1_501).is_empty());
    }

    #[test]
    fn refresh_notifications_coalesce_to_the_latest_epoch() {
        let config = MrmsIngestConfig {
            enabled: true,
            ..MrmsIngestConfig::default()
        };
        let monitor = MrmsIngestMonitor::new(&config);
        let mut receiver = monitor.wake_receiver();
        for _ in 0..10 {
            monitor.request_refresh();
        }
        assert_eq!(*receiver.borrow_and_update(), 10);
        assert!(!receiver.has_changed().unwrap());
    }

    #[test]
    fn retry_backoff_is_exponential_and_capped() {
        let config = MrmsIngestConfig {
            poll_interval_seconds: 60,
            maximum_backoff_seconds: 300,
            ..MrmsIngestConfig::default()
        };
        assert_eq!(retry_delay(&config, 1), Duration::from_secs(60));
        assert_eq!(retry_delay(&config, 3), Duration::from_secs(240));
        assert_eq!(retry_delay(&config, 20), Duration::from_secs(300));
    }

    #[test]
    fn retention_removes_only_fully_expired_mrms_runs() {
        let directory = tempfile::tempdir().unwrap();
        let grid = LatLonGrid::new(GridShape::new(1, 1).unwrap(), vec![35.0], vec![-97.0]).unwrap();
        let frame = ObservationFrame {
            family: ObservationFamily::Mrms,
            collection: "conus".into(),
            product: "ReflectivityAtLowestAltitude".into(),
            valid_unix: 1_700_000_000,
            grid,
            projection: None,
            planes: vec![GridPlane {
                name: "mrms_reflectivity_lowest_altitude".into(),
                units: "dBZ".into(),
                selector: serde_json::json!({"mrms": {"product": "ReflectivityAtLowestAltitude"}}),
                values: vec![42.0],
            }],
            provenance_provider: "noaa-mrms".into(),
            provenance_roles: vec!["radar".into()],
            provenance_products: vec!["reflectivityatlowestaltitude".into()],
        };
        let stored = write_observation_frame(directory.path(), &frame).unwrap();
        let run_path = directory.path().join(MRMS_MODEL).join(&stored.run);
        let mut unselected = frame.clone();
        unselected.product = "MergedReflectivityQCComposite".into();
        unselected.planes[0].name = "mrms_composite_reflectivity".into();
        let unselected = write_observation_frame(directory.path(), &unselected).unwrap();
        let unselected_path = directory.path().join(MRMS_MODEL).join(unselected.run);
        assert!(run_path.is_dir());
        assert_eq!(
            prune_expired_mrms_runs(
                directory.path(),
                frame.valid_unix,
                &[MrmsFollowSpec::reflectivity_at_lowest_altitude()],
            )
            .unwrap(),
            0
        );
        assert_eq!(
            prune_expired_mrms_runs(
                directory.path(),
                frame.valid_unix + 1,
                &[MrmsFollowSpec::reflectivity_at_lowest_altitude()],
            )
            .unwrap(),
            1
        );
        assert!(!run_path.exists());
        assert!(unselected_path.is_dir());
    }

    #[tokio::test]
    async fn configured_concurrency_bounds_independent_products() {
        let mut second = MrmsFollowSpec::reflectivity_at_lowest_altitude();
        second.product = "MergedReflectivityQCComposite".into();
        second.variable = "mrms_composite_reflectivity".into();
        let config = MrmsIngestConfig {
            enabled: true,
            poll_interval_seconds: 3_600,
            stale_after_seconds: 7_200,
            maximum_backoff_seconds: 3_600,
            concurrency: 1,
            products: vec![MrmsFollowSpec::reflectivity_at_lowest_altitude(), second],
            ..MrmsIngestConfig::default()
        };
        let monitor = MrmsIngestMonitor::new(&config);
        let runner = Arc::new(FakeRunner::new());
        let directory = tempfile::tempdir().unwrap();
        let mut supervisor = MrmsIngestSupervisor::start_with_runner(
            &config,
            directory.path(),
            monitor.clone(),
            runner.clone(),
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if monitor
                    .status()
                    .products
                    .iter()
                    .all(|product| product.last_success_unix.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        supervisor.shutdown().await;
        assert_eq!(runner.maximum_active.load(Ordering::SeqCst), 1);
        assert_eq!(runner.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn stale_mrms_degrades_core_readiness_unless_explicitly_gated() {
        const TOKEN: &str = "mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm";
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.mrms_ingest.enabled = true;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(true).unwrap();
        let state = crate::AppState::new(config, tokens).unwrap();
        let app = crate::build_router(state).unwrap();

        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/observations/mrms/ingest/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let authorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/observations/mrms/ingest/status")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authorized.status(), StatusCode::OK);
        assert_eq!(
            authorized.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );

        let readiness = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(readiness.status(), StatusCode::OK);
        let body = to_bytes(readiness.into_body(), 64 * 1024).await.unwrap();
        let health: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(health["status"], "degraded");
        assert_eq!(
            health["degraded_subsystems"],
            serde_json::json!(["mrms_ingest"])
        );

        let gated_directory = tempfile::tempdir().unwrap();
        let mut gated = crate::AppConfig::default();
        gated.server.store_root = gated_directory.path().join("store");
        gated.server.artifact_root = gated_directory.path().join("artifacts");
        gated.server.cache_root = gated_directory.path().join("cache");
        gated.mrms_ingest.enabled = true;
        gated.mrms_ingest.gate_server_readiness = true;
        fs::create_dir_all(&gated.server.store_root).unwrap();
        fs::create_dir_all(&gated.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        gated.validate(true).unwrap();
        let gated_app = crate::build_router(crate::AppState::new(gated, tokens).unwrap()).unwrap();
        let readiness = gated_app
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(readiness.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(readiness.into_body(), 64 * 1024).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "MRMS_INGEST_NOT_READY");
    }
}

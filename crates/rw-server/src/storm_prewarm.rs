//! Request-independent MRMS deterministic storm-cell reconciliation.
//!
//! The worker runs once at startup, then only when the MRMS follower announces
//! a committed immutable frame. Watch-channel epochs coalesce bursts; there is
//! no HTTP-client or UI polling loop. Every fill uses the same exact-frame
//! memory/disk/compute path as `/v1/ops/storms/cells`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use rw_ops_protocol::StormSource;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::AppState;
use crate::config::{MrmsFollowSpec, StormCacheRetention, StormPrewarmConfig};
use crate::storm_cache::STORM_CACHE_REVISION;
use crate::storms::{
    DetectionRequest, StoredStormGridRef, StormCellsRequest, StormMethodRequest,
    obtain_cached_frame, storm_frame_cache_key,
};

const STATUS_SCHEMA: &str = "rw-server.storm-prewarm-status.v1";
const REQUEST_SCHEMA: &str = "rw.server.storm-cells-request.v1";
const MRMS_MODEL: &str = "obs-mrms";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StormPrewarmPhase {
    Disabled,
    Starting,
    WaitingForSource,
    Reconciling,
    Ready,
    Degraded,
    Stopped,
}

#[derive(Clone, Debug, Serialize)]
pub struct StormPrewarmSourceStatus {
    pub product: String,
    pub variable: String,
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub storage_slot: u16,
    pub valid_at_unix_ms: i64,
    pub cache_key: String,
    pub method: &'static str,
    pub cache_revision: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct StormPrewarmStatus {
    pub schema: &'static str,
    pub enabled: bool,
    pub ready: bool,
    pub phase: StormPrewarmPhase,
    pub checked_at_unix_ms: i64,
    pub cache_revision: &'static str,
    pub backfill_frames: usize,
    pub retention: StormCacheRetention,
    pub trigger_epoch: u64,
    pub coalesced_triggers: u64,
    pub in_flight: bool,
    pub restart_reconciled: bool,
    pub last_attempt_unix_ms: Option<i64>,
    pub last_success_unix_ms: Option<i64>,
    pub last_source_valid_unix_ms: Option<i64>,
    pub stale: bool,
    pub reconciled_frames: u64,
    pub latest_source: Option<StormPrewarmSourceStatus>,
    pub last_error_unix_ms: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Debug)]
struct StatusState {
    phase: StormPrewarmPhase,
    trigger_epoch: u64,
    coalesced_triggers: u64,
    in_flight: bool,
    restart_reconciled: bool,
    last_attempt_unix_ms: Option<i64>,
    last_success_unix_ms: Option<i64>,
    last_source_valid_unix_ms: Option<i64>,
    reconciled_frames: u64,
    latest_source: Option<StormPrewarmSourceStatus>,
    last_error_unix_ms: Option<i64>,
    last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct StormPrewarmStatusHandle {
    config: Arc<StormPrewarmConfig>,
    state: Arc<Mutex<StatusState>>,
}

impl StormPrewarmStatusHandle {
    pub fn new(config: &StormPrewarmConfig) -> Self {
        Self {
            config: Arc::new(config.clone()),
            state: Arc::new(Mutex::new(StatusState {
                phase: if config.enabled {
                    StormPrewarmPhase::Starting
                } else {
                    StormPrewarmPhase::Disabled
                },
                trigger_epoch: 0,
                coalesced_triggers: 0,
                in_flight: false,
                restart_reconciled: false,
                last_attempt_unix_ms: None,
                last_success_unix_ms: None,
                last_source_valid_unix_ms: None,
                reconciled_frames: 0,
                latest_source: None,
                last_error_unix_ms: None,
                last_error: None,
            })),
        }
    }

    pub fn status(&self) -> StormPrewarmStatus {
        let checked_at_unix_ms = now_unix_ms();
        let state = self.lock();
        let stale = state.last_source_valid_unix_ms.is_some_and(|valid| {
            checked_at_unix_ms.saturating_sub(valid)
                > millis_from_seconds(self.config.stale_after_seconds)
        });
        StormPrewarmStatus {
            schema: STATUS_SCHEMA,
            enabled: self.config.enabled,
            ready: !self.config.enabled
                || (state.restart_reconciled
                    && !state.in_flight
                    && state.phase == StormPrewarmPhase::Ready
                    && !stale),
            phase: state.phase,
            checked_at_unix_ms,
            cache_revision: STORM_CACHE_REVISION,
            backfill_frames: self.config.backfill_frames,
            retention: self.config.retention.clone(),
            trigger_epoch: state.trigger_epoch,
            coalesced_triggers: state.coalesced_triggers,
            in_flight: state.in_flight,
            restart_reconciled: state.restart_reconciled,
            last_attempt_unix_ms: state.last_attempt_unix_ms,
            last_success_unix_ms: state.last_success_unix_ms,
            last_source_valid_unix_ms: state.last_source_valid_unix_ms,
            stale,
            reconciled_frames: state.reconciled_frames,
            latest_source: state.latest_source.clone(),
            last_error_unix_ms: state.last_error_unix_ms,
            last_error: state.last_error.clone(),
        }
    }

    fn begin(&self, trigger_epoch: u64) {
        let mut state = self.lock();
        if state.last_attempt_unix_ms.is_some() {
            state.coalesced_triggers = state.coalesced_triggers.saturating_add(
                trigger_epoch
                    .wrapping_sub(state.trigger_epoch)
                    .saturating_sub(1),
            );
        }
        state.trigger_epoch = trigger_epoch;
        state.phase = StormPrewarmPhase::Reconciling;
        state.in_flight = true;
        state.last_attempt_unix_ms = Some(now_unix_ms());
    }

    fn succeed(
        &self,
        restart: bool,
        reconciled_frames: usize,
        latest_source: Option<StormPrewarmSourceStatus>,
    ) {
        let mut state = self.lock();
        state.phase = if latest_source.is_some() {
            StormPrewarmPhase::Ready
        } else {
            StormPrewarmPhase::WaitingForSource
        };
        state.in_flight = false;
        state.restart_reconciled |= restart;
        state.last_success_unix_ms = Some(now_unix_ms());
        state.reconciled_frames = state
            .reconciled_frames
            .saturating_add(reconciled_frames as u64);
        if let Some(source) = latest_source {
            state.last_source_valid_unix_ms = Some(source.valid_at_unix_ms);
            state.latest_source = Some(source);
        }
        state.last_error = None;
    }

    fn fail(&self, restart: bool, error: impl Into<String>) {
        let mut state = self.lock();
        state.phase = StormPrewarmPhase::Degraded;
        state.in_flight = false;
        state.restart_reconciled |= restart;
        state.last_error_unix_ms = Some(now_unix_ms());
        state.last_error = Some(bound_error(error.into()));
    }

    fn stop(&self) {
        let mut state = self.lock();
        if self.config.enabled {
            state.phase = StormPrewarmPhase::Stopped;
            state.in_flight = false;
        }
    }

    fn lock(&self) -> MutexGuard<'_, StatusState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

pub struct StormPrewarmSupervisor {
    cancel: watch::Sender<bool>,
    task: Option<JoinHandle<()>>,
}

impl StormPrewarmSupervisor {
    #[must_use]
    pub fn start(
        config: StormPrewarmConfig,
        state: AppState,
        committed: watch::Receiver<u64>,
    ) -> Self {
        let (cancel, cancel_rx) = watch::channel(false);
        let task = config
            .enabled
            .then(|| tokio::spawn(run_worker(config, state, committed, cancel_rx)));
        Self { cancel, task }
    }

    pub async fn shutdown(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            warn!(%error, "storm prewarm worker join failed");
        }
    }
}

impl Drop for StormPrewarmSupervisor {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn run_worker(
    config: StormPrewarmConfig,
    state: AppState,
    mut committed: watch::Receiver<u64>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut restart = true;
    loop {
        if *cancel.borrow() {
            break;
        }
        let trigger_epoch = *committed.borrow_and_update();
        state.storm_prewarm_status.begin(trigger_epoch);
        match reconcile(&config, &state).await {
            Ok((count, source)) => {
                state.storm_prewarm_status.succeed(restart, count, source);
                info!(
                    frames = count,
                    trigger_epoch, "MRMS storm-frame cache reconciled"
                );
            }
            Err(error) => {
                warn!(%error, trigger_epoch, "MRMS storm-frame cache reconciliation failed");
                state.storm_prewarm_status.fail(restart, error);
            }
        }
        restart = false;
        tokio::select! {
            changed = committed.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
            }
        }
    }
    state.storm_prewarm_status.stop();
}

async fn reconcile(
    config: &StormPrewarmConfig,
    state: &AppState,
) -> Result<(usize, Option<StormPrewarmSourceStatus>), String> {
    let catalog = state.catalog.clone();
    let products = state.config.mrms_ingest.products.clone();
    let backfill_frames = config.backfill_frames;
    let requests = state
        .run_light(move || discover_requests(&catalog, &products, backfill_frames))
        .await
        .map_err(|error| format!("MRMS storm discovery worker failed: {error}"))?
        .map_err(|error| format!("MRMS storm discovery failed: {error}"))?;

    let mut latest = None;
    for request in requests.iter().cloned() {
        let runtime_revision = state
            .storms
            .as_ref()
            .map_or(0, super::storms::StormRuntime::cache_revision);
        let cache_key = storm_frame_cache_key(&request, runtime_revision)
            .map_err(|error| format!("storm cache identity failed: {error}"))?;
        let source = source_status(&request, cache_key);
        obtain_cached_frame(state, request)
            .await
            .map_err(|error| match error.as_ref() {
                crate::storms::StormFrameFillError::Service(error) => error.to_string(),
                crate::storms::StormFrameFillError::Execution(error) => error.to_string(),
            })?;
        if latest
            .as_ref()
            .is_none_or(|current: &StormPrewarmSourceStatus| {
                source.valid_at_unix_ms > current.valid_at_unix_ms
            })
        {
            latest = Some(source);
        }
    }
    if let Some(cache) = &state.storm_disk_cache {
        cache
            .prune()
            .map_err(|error| format!("storm cache retention failed: {error}"))?;
        let health = cache.health();
        if !health.ready {
            return Err(format!(
                "durable storm cache is degraded: {}",
                health
                    .last_error
                    .as_deref()
                    .unwrap_or("an unclassified cache I/O failure occurred")
            ));
        }
    }
    Ok((requests.len(), latest))
}

fn discover_requests(
    catalog: &crate::origin_catalog::PublishedStoreCatalog,
    products: &[MrmsFollowSpec],
    backfill_frames: usize,
) -> Result<Vec<StormCellsRequest>, rw_query::QueryError> {
    let mut runs = catalog.list_runs(MRMS_MODEL)?;
    runs.sort_by(|left, right| {
        right
            .run
            .last_valid_unix
            .cmp(&left.run.last_valid_unix)
            .then_with(|| right.run.run.cmp(&left.run.run))
    });
    let mut requests = Vec::new();
    for product in products
        .iter()
        .filter(|product| is_lowest_altitude_reflectivity(&product.product))
    {
        let mut source_requests = Vec::new();
        let mut seen = BTreeSet::new();
        for run in &runs {
            let snapshot = catalog.snapshot(MRMS_MODEL, &run.run.run)?;
            let descriptor = snapshot.descriptor();
            for time in snapshot.time_axis().iter().rev() {
                let Some(hour) = snapshot.manifest().hours.get(&time.storage_slot) else {
                    continue;
                };
                if !hour.variables.iter().any(|name| name == &product.variable)
                    || !seen.insert((time.valid_unix, descriptor.grid_hash.clone()))
                {
                    continue;
                }
                let Some(valid_at_unix_ms) = time.valid_unix.checked_mul(1_000) else {
                    continue;
                };
                source_requests.push(StormCellsRequest {
                    schema: REQUEST_SCHEMA.into(),
                    grid: StoredStormGridRef {
                        model: descriptor.model.clone(),
                        run: descriptor.run.clone(),
                        expected_snapshot_id: descriptor.snapshot_id.clone(),
                        expected_grid_hash: descriptor.grid_hash.clone(),
                        storage_slot: time.storage_slot,
                        variable: product.variable.clone(),
                    },
                    source: StormSource::Mrms {
                        product: product.product.clone(),
                        valid_at_unix_ms,
                        grid_hash: descriptor.grid_hash.clone(),
                    },
                    method: StormMethodRequest::Deterministic {
                        config: DetectionRequest::default(),
                    },
                });
            }
        }
        source_requests.sort_by(|left, right| {
            source_valid_ms(&right.source)
                .cmp(&source_valid_ms(&left.source))
                .then_with(|| right.grid.run.cmp(&left.grid.run))
        });
        source_requests.truncate(backfill_frames);
        requests.extend(source_requests);
    }
    requests
        .sort_by(|left, right| source_valid_ms(&left.source).cmp(&source_valid_ms(&right.source)));
    Ok(requests)
}

fn source_status(request: &StormCellsRequest, cache_key: String) -> StormPrewarmSourceStatus {
    let (product, valid_at_unix_ms) = match &request.source {
        StormSource::Mrms {
            product,
            valid_at_unix_ms,
            ..
        } => (product.clone(), *valid_at_unix_ms),
        StormSource::NexradLevel2 {
            volume_at_unix_ms, ..
        } => ("nexrad_level2".into(), *volume_at_unix_ms),
    };
    StormPrewarmSourceStatus {
        product,
        variable: request.grid.variable.clone(),
        model: request.grid.model.clone(),
        run: request.grid.run.clone(),
        snapshot_id: request.grid.expected_snapshot_id.clone(),
        grid_hash: request.grid.expected_grid_hash.clone(),
        storage_slot: request.grid.storage_slot,
        valid_at_unix_ms,
        cache_key,
        method: "deterministic_reflectivity_cells",
        cache_revision: STORM_CACHE_REVISION,
    }
}

fn is_lowest_altitude_reflectivity(product: &str) -> bool {
    normalize_identity(product) == "reflectivityatlowestaltitude"
}

fn normalize_identity(value: &str) -> String {
    value
        .bytes()
        .filter(u8::is_ascii_alphanumeric)
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect()
}

fn source_valid_ms(source: &StormSource) -> i64 {
    match source {
        StormSource::Mrms {
            valid_at_unix_ms, ..
        } => *valid_at_unix_ms,
        StormSource::NexradLevel2 {
            volume_at_unix_ms, ..
        } => *volume_at_unix_ms,
    }
}

fn millis_from_seconds(seconds: u64) -> i64 {
    i64::try_from(seconds)
        .unwrap_or(i64::MAX)
        .saturating_mul(1_000)
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn bound_error(mut error: String) -> String {
    if error.len() > 2_048 {
        let mut end = 2_048;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
    }
    error
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{GridShape, LatLonGrid};
    use rw_observations::{
        GridPlane, ObservationFamily, ObservationFrame, write_observation_frame_with_limit,
    };
    use serde_json::json;
    use std::fs;
    use std::time::Duration;

    const TOKEN: &str = "ssssssssssssssssssssssssssssssss";
    const FIRST_VALID: i64 = 1_780_000_000;

    struct Fixture {
        _directory: tempfile::TempDir,
        config: crate::AppConfig,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.operations.enabled = true;
        config.operations.root = directory.path().join("operations");
        config.storm_prewarm.enabled = true;
        config.storm_prewarm.backfill_frames = 8;
        config.storm_prewarm.retention = StormCacheRetention::Unlimited;
        let tokens = directory.path().join("ops-read.tokens");
        crate::test_support::write_private_file(&tokens, TOKEN);
        config.auth.ops_read_token_file = Some(tokens);
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        fs::create_dir_all(&config.operations.root).unwrap();
        write_frame(&config.server.store_root, FIRST_VALID);
        Fixture {
            _directory: directory,
            config,
        }
    }

    fn write_frame(store_root: &std::path::Path, valid_unix: i64) {
        let nx = 5;
        let ny = 5;
        let mut latitudes = Vec::new();
        let mut longitudes = Vec::new();
        for y in 0..ny {
            for x in 0..nx {
                latitudes.push(35.0 + y as f32 * 0.1);
                longitudes.push(-98.0 + x as f32 * 0.1);
            }
        }
        let grid = LatLonGrid::new(GridShape::new(nx, ny).unwrap(), latitudes, longitudes).unwrap();
        let mut reflectivity = vec![10.0_f32; nx * ny];
        for y in 1..4 {
            for x in 1..4 {
                reflectivity[y * nx + x] = 50.0;
            }
        }
        let frame = ObservationFrame {
            family: ObservationFamily::Mrms,
            collection: "conus".into(),
            product: "ReflectivityAtLowestAltitude".into(),
            valid_unix,
            grid,
            projection: None,
            planes: vec![GridPlane {
                name: "mrms_reflectivity_lowest_altitude".into(),
                units: "dBZ".into(),
                selector: json!({
                    "mrms": {
                        "product": "ReflectivityAtLowestAltitude",
                        "parameter_name": "ReflectivityAtLowestAltitude"
                    },
                    "display": {"semantics": "reflectivity"}
                }),
                values: reflectivity,
            }],
            provenance_provider: "noaa-mrms".into(),
            provenance_roles: vec!["radar".into(), "mosaic".into()],
            provenance_products: vec!["reflectivity-at-lowest-altitude".into()],
        };
        write_observation_frame_with_limit(store_root, &frame, nx * ny).unwrap();
    }

    async fn wait_for_source(
        status: &StormPrewarmStatusHandle,
        valid_at_unix_ms: i64,
    ) -> StormPrewarmStatus {
        for _ in 0..250 {
            let current = status.status();
            if current.restart_reconciled
                && current
                    .latest_source
                    .as_ref()
                    .is_some_and(|source| source.valid_at_unix_ms == valid_at_unix_ms)
                && !current.in_flight
            {
                return current;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("storm prewarm did not reconcile source {valid_at_unix_ms}");
    }

    #[tokio::test]
    async fn committed_new_frame_is_prewarmed_without_an_http_request() {
        let fixture = fixture();
        let state = AppState::new(fixture.config.clone(), crate::TokenSet::default()).unwrap();
        let monitor = state.mrms_ingest_monitor();
        let mut supervisor = StormPrewarmSupervisor::start(
            fixture.config.storm_prewarm.clone(),
            state.clone(),
            monitor.committed_receiver(),
        );
        wait_for_source(&state.storm_prewarm_status, FIRST_VALID * 1_000).await;
        let old_request = discover_requests(
            &state.catalog,
            &state.config.mrms_ingest.products,
            fixture.config.storm_prewarm.backfill_frames,
        )
        .unwrap()
        .pop()
        .unwrap();

        let second = FIRST_VALID + 300;
        write_frame(&fixture.config.server.store_root, second);
        let invalidated = obtain_cached_frame(&state, old_request).await;
        assert!(matches!(
            invalidated,
            Err(ref error)
                if matches!(
                    error.as_ref(),
                    crate::storms::StormFrameFillError::Service(
                        crate::storms::StormServiceError::SnapshotMismatch
                    )
                )
        ));
        monitor.notify_committed_for_test();
        let status = wait_for_source(&state.storm_prewarm_status, second * 1_000).await;
        // The fixed scientific fixture is intentionally old relative to the
        // wall clock. Reconciliation succeeds, while status honestly marks
        // that exact cached source stale instead of calling it current.
        assert_eq!(status.phase, StormPrewarmPhase::Ready);
        assert!(status.stale);
        assert!(!status.ready);
        assert!(status.reconciled_frames >= 3);
        let disk = state.storm_disk_cache.as_ref().unwrap().health();
        // Appending the second exact time creates a new immutable run snapshot
        // identity. Unlimited retention intentionally keeps the first
        // snapshot's historical frame plus both frames under the new snapshot.
        assert_eq!(disk.entries, 3);
        assert_eq!(disk.atomic_store_writes, 3);
        supervisor.shutdown().await;
    }

    #[tokio::test]
    async fn restart_backfill_uses_durable_results_instead_of_recomputing() {
        let fixture = fixture();
        let first_state =
            AppState::new(fixture.config.clone(), crate::TokenSet::default()).unwrap();
        let monitor = first_state.mrms_ingest_monitor();
        let mut first = StormPrewarmSupervisor::start(
            fixture.config.storm_prewarm.clone(),
            first_state.clone(),
            monitor.committed_receiver(),
        );
        wait_for_source(&first_state.storm_prewarm_status, FIRST_VALID * 1_000).await;
        assert_eq!(
            first_state
                .storm_disk_cache
                .as_ref()
                .unwrap()
                .health()
                .atomic_store_writes,
            1
        );
        first.shutdown().await;
        drop(first_state);

        let restarted = AppState::new(fixture.config.clone(), crate::TokenSet::default()).unwrap();
        let monitor = restarted.mrms_ingest_monitor();
        let mut second = StormPrewarmSupervisor::start(
            fixture.config.storm_prewarm.clone(),
            restarted.clone(),
            monitor.committed_receiver(),
        );
        wait_for_source(&restarted.storm_prewarm_status, FIRST_VALID * 1_000).await;
        let disk = restarted.storm_disk_cache.as_ref().unwrap().health();
        assert_eq!(disk.disk_hits, 1);
        assert_eq!(disk.atomic_store_writes, 0);
        second.shutdown().await;
    }
}

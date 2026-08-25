use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use moka::{future::Cache as AsyncCache, sync::Cache};
use rw_query::{QueryLimits, StoreCatalog};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinError;

use crate::auth::OperationsTokenSets;
use crate::community::CommunityService;
use crate::community_relay::CommunityRelayService;
use crate::federation::FederationService;
use crate::federation_proxy::{
    ServerFederationProxy, load_federation_origin_tokens, open_server_federation_proxy,
    validate_credential_isolation,
};
use crate::generation_replication::ServerGenerationReplication;
use crate::mrms_ingest::MrmsIngestMonitor;
use crate::nexrad_level2_ingest::NexradLevel2IngestMonitor;
use crate::origin_catalog::PublishedStoreCatalog;
use crate::satellite_prewarm::SatellitePrewarmStatusHandle;
use crate::satellite_tile_cache::SatelliteTileDiskCache;
use crate::storm_cache::{CachedStormFrame, StormFrameDiskCache};
use crate::storm_prewarm::StormPrewarmStatusHandle;
use crate::{AppConfig, JobError, JobManager, Metrics, TokenSet};

/// Immutable native-satellite render identity. Every input that can change
/// output pixels is explicit so two renderer recipes or source frames can
/// never share a cached PNG.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SatelliteTileCacheKey {
    pub(crate) recipe: String,
    pub(crate) source_revision: String,
    pub(crate) platform: String,
    pub(crate) sector: String,
    pub(crate) product: String,
    pub(crate) frame: String,
    pub(crate) zoom: u8,
    pub(crate) x: u32,
    pub(crate) y: u32,
    pub(crate) tile_size: u32,
}

impl SatelliteTileCacheKey {
    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.recipe.len()
            + self.source_revision.len()
            + self.platform.len()
            + self.sector.len()
            + self.product.len()
            + self.frame.len()
    }
}

/// Encoded response and validators computed once by the single-flight cache
/// fill. `Bytes` keeps cache hits zero-copy through Axum's response body.
#[derive(Debug)]
pub(crate) struct CachedSatelliteTile {
    pub(crate) png: Bytes,
    pub(crate) etag: String,
    pub(crate) frame_id: String,
    pub(crate) source_revision: String,
    pub(crate) valid_unix: i64,
}

impl CachedSatelliteTile {
    fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.png.len()
            + self.etag.len()
            + self.frame_id.len()
            + self.source_revision.len()
    }
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub tokens: Arc<TokenSet>,
    pub(crate) operations_tokens: Arc<OperationsTokenSets>,
    pub(crate) mrms_ingest: MrmsIngestMonitor,
    pub(crate) nexrad_level2_ingest: NexradLevel2IngestMonitor,
    pub storms: Option<crate::storms::StormRuntime>,
    pub federation_origin_tokens: Arc<TokenSet>,
    pub catalog: Arc<PublishedStoreCatalog>,
    pub metrics: Arc<Metrics>,
    pub jobs: JobManager,
    pub community: CommunityService,
    pub community_relay: CommunityRelayService,
    pub federation: FederationService,
    pub(crate) federation_proxy: Option<Arc<ServerFederationProxy>>,
    pub generation_replication: ServerGenerationReplication,
    pub response_cache: Cache<String, Bytes>,
    /// Separately bounded native PNG cache. It has no time expiry because its
    /// keys are exact-frame and recipe-versioned; capacity eviction bounds
    /// memory while retaining genuinely hot immutable tiles.
    pub(crate) satellite_tile_cache: AsyncCache<SatelliteTileCacheKey, Arc<CachedSatelliteTile>>,
    /// Restart-reusable exact-frame cache underneath the process-local Moka
    /// layer. Each entry is atomically installed and verified before reuse.
    pub(crate) satellite_tile_disk_cache: SatelliteTileDiskCache,
    pub(crate) satellite_prewarm_status: SatellitePrewarmStatusHandle,
    /// Immutable storm frames keyed by the complete snapshot, source, method,
    /// and in-process model-runtime revision. Moka's async cache makes a miss
    /// single-flight, so concurrent app and website clients share one contour
    /// computation instead of each scanning the same native grid.
    pub(crate) storm_frame_cache: AsyncCache<String, Arc<CachedStormFrame>>,
    /// Verified restart-reusable canonical/GeoJSON pairs below the configured
    /// mutable cache root. This is present whenever private storm processing
    /// is enabled, even if automatic prewarming itself is disabled.
    pub(crate) storm_disk_cache: Option<StormFrameDiskCache>,
    pub(crate) storm_prewarm_status: StormPrewarmStatusHandle,
    pub started_at: Instant,
    light: Arc<Semaphore>,
    heavy: Arc<Semaphore>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppState")
            .field("config", &self.config)
            .field("tokens", &self.tokens)
            .field("started_at", &self.started_at)
            .finish_non_exhaustive()
    }
}

impl AppState {
    pub fn new(config: AppConfig, tokens: TokenSet) -> Result<Self, JobError> {
        validate_credential_isolation(&config, &tokens).map_err(|detail| {
            JobError::Invalid(format!("federation credential isolation: {detail}"))
        })?;
        let mrms_ingest = MrmsIngestMonitor::new(&config.mrms_ingest);
        let nexrad_level2_ingest = NexradLevel2IngestMonitor::new(&config.nexrad_level2_ingest);
        let operations_tokens = if config.operations.enabled {
            OperationsTokenSets::load(&config.auth, &tokens)
                .map_err(|error| JobError::Invalid(format!("operations authentication: {error}")))?
        } else {
            OperationsTokenSets::default()
        };
        if config.operations.enabled && operations_tokens.is_empty() {
            return Err(JobError::Invalid(
                "enabled operations APIs require at least one authenticated credential".into(),
            ));
        }
        let storms = if config.operations.enabled {
            Some(
                crate::storms::StormRuntime::open(&config.operations.root).map_err(|error| {
                    JobError::Invalid(format!(
                        "failed to initialize private storm service: {error}"
                    ))
                })?,
            )
        } else {
            None
        };
        let limits = QueryLimits {
            // Catalog enumeration is RAM/address-space limited. Observation
            // clients may explicitly request a smaller response, but the
            // server does not silently hide older runs behind a fixed count.
            max_catalog_entries: usize::MAX,
            max_time_points: config.limits.catalog_time_points,
            max_selected_time_points: config.limits.temporal_frames,
            max_variables: config.limits.variables_per_query,
            max_reduction_cells: config.limits.sync_result_values,
            max_temporal_reduction_cells: config.limits.temporal_reduction_cells,
            max_temporal_output_values: config.limits.temporal_output_values,
            max_point_values: config.limits.sync_result_values,
        };
        let catalog = StoreCatalog::with_limits_and_reader_cache_bytes(
            &config.server.store_root,
            limits,
            config.limits.reader_cache_bytes,
        );
        let generation_replication = ServerGenerationReplication::open(
            &config.generation_replication,
            &config.server.store_root,
        )
        .map_err(|error| {
            JobError::Invalid(format!(
                "failed to initialize generation replication: {error}"
            ))
        })?;
        let catalog = PublishedStoreCatalog::new(catalog, config.origin_catalog.clone())
            .with_generation_replication(generation_replication.clone());
        let light = Arc::new(Semaphore::new(config.limits.light_concurrency));
        let heavy = Arc::new(Semaphore::new(config.limits.heavy_concurrency));
        let jobs = JobManager::open(
            &config.server.artifact_root,
            config.limits.queued_jobs,
            config.limits.job_result_bytes,
            config.limits.job_history_records,
            config.limits.job_retention_seconds,
        )?;
        let response_cache = Cache::builder()
            .max_capacity(config.limits.response_cache_bytes)
            .time_to_live(Duration::from_secs(config.catalog.response_cache_seconds))
            .weigher(|_key: &String, value: &Bytes| u32::try_from(value.len()).unwrap_or(u32::MAX))
            .build();
        let satellite_tile_cache = AsyncCache::builder()
            .max_capacity(config.limits.response_cache_bytes)
            .weigher(
                |key: &SatelliteTileCacheKey, value: &Arc<CachedSatelliteTile>| {
                    u32::try_from(key.estimated_bytes() + value.estimated_bytes())
                        .unwrap_or(u32::MAX)
                },
            )
            .build();
        let satellite_tile_disk_cache = SatelliteTileDiskCache::open(
            &config.server.cache_root,
            config.limits.satellite_tile_cache_bytes,
        )
        .map_err(|error| {
            JobError::Invalid(format!(
                "failed to initialize durable satellite tile cache: {error}"
            ))
        })?;
        let satellite_prewarm_status = SatellitePrewarmStatusHandle::new(&config.satellite_prewarm);
        let storm_disk_cache = storms
            .as_ref()
            .map(|_| {
                StormFrameDiskCache::open(
                    &config.server.cache_root,
                    config.storm_prewarm.retention.clone(),
                )
            })
            .transpose()
            .map_err(|error| {
                JobError::Invalid(format!(
                    "failed to initialize durable storm-frame cache: {error}"
                ))
            })?;
        let storm_prewarm_status = StormPrewarmStatusHandle::new(&config.storm_prewarm);
        let storm_frame_cache = AsyncCache::builder()
            .max_capacity(config.limits.response_cache_bytes)
            .weigher(|key: &String, frame: &Arc<CachedStormFrame>| {
                u32::try_from(key.len() + frame.estimated_bytes()).unwrap_or(u32::MAX)
            })
            .build();
        let community = CommunityService::open(&config.community).map_err(|error| {
            JobError::Invalid(format!("failed to initialize Community Cache: {error}"))
        })?;
        let community_relay = CommunityRelayService::open(
            &config.community,
            rw_community_protocol::ProtocolLimits {
                max_manifest_bytes: config.community.quotas.maximum_manifest_bytes,
                max_encoded_bytes: config.community.quotas.maximum_object_bytes,
                max_decoded_bytes: config.community.quotas.maximum_decompressed_bytes,
                max_case_artifacts: config.community.cases.maximum_objects_per_case,
                ..rw_community_protocol::ProtocolLimits::default()
            },
        )
        .map_err(|error| {
            JobError::Invalid(format!(
                "failed to initialize Community Cache relay broker: {error}"
            ))
        })?;
        let metrics = Arc::new(Metrics::new());
        metrics.set_relay_kill_switch(
            config.community.relay.enabled && config.community.relay.kill_switch,
        );
        if let Ok(status) = generation_replication.startup_status() {
            metrics.set_replication_kill_switch(status.kill_switch);
        }
        let federation = FederationService::open(&config.federation).map_err(|error| {
            JobError::Invalid(format!(
                "failed to initialize public-origin federation: {error}"
            ))
        })?;
        let federation_origin_tokens =
            load_federation_origin_tokens(&config, &tokens).map_err(|detail| {
                JobError::Invalid(format!("federation origin authentication: {detail}"))
            })?;
        let federation_proxy =
            open_server_federation_proxy(&config, community.clone(), federation.clone()).map_err(
                |detail| {
                    JobError::Invalid(format!("failed to initialize federation proxy: {detail}"))
                },
            )?;
        if let Some(proxy) = &federation_proxy {
            metrics.set_federation_proxy_kill_switch(proxy.startup_status().kill_switch);
        }
        Ok(Self {
            config: Arc::new(config),
            tokens: Arc::new(tokens),
            operations_tokens: Arc::new(operations_tokens),
            mrms_ingest,
            nexrad_level2_ingest,
            storms,
            federation_origin_tokens: Arc::new(federation_origin_tokens),
            catalog: Arc::new(catalog),
            metrics,
            jobs,
            community,
            community_relay,
            federation,
            federation_proxy,
            generation_replication,
            response_cache,
            satellite_tile_cache,
            satellite_tile_disk_cache,
            satellite_prewarm_status,
            storm_frame_cache,
            storm_disk_cache,
            storm_prewarm_status,
            started_at: Instant::now(),
            light,
            heavy,
        })
    }

    pub async fn run_light<T, F>(&self, operation: F) -> Result<T, ExecutionError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.run_bounded(
            self.light.clone(),
            Duration::from_secs(self.config.limits.sync_timeout_seconds),
            operation,
        )
        .await
    }

    pub async fn run_heavy_sync<T, F>(&self, operation: F) -> Result<T, ExecutionError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.run_bounded(
            self.heavy.clone(),
            Duration::from_secs(self.config.limits.sync_timeout_seconds),
            operation,
        )
        .await
    }

    pub async fn run_heavy_job<T, F>(&self, operation: F) -> Result<T, ExecutionError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.run_bounded(
            self.heavy.clone(),
            Duration::from_secs(self.config.limits.job_timeout_seconds),
            operation,
        )
        .await
    }

    async fn run_bounded<T, F>(
        &self,
        semaphore: Arc<Semaphore>,
        deadline: Duration,
        operation: F,
    ) -> Result<T, ExecutionError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let started = Instant::now();
        let permit = tokio::time::timeout(deadline, semaphore.acquire_owned())
            .await
            .map_err(|_| ExecutionError::AdmissionTimeout)?
            .map_err(|_| ExecutionError::ShuttingDown)?;
        let remaining = deadline
            .checked_sub(started.elapsed())
            .ok_or(ExecutionError::ExecutionTimeout)?;
        if remaining.is_zero() {
            return Err(ExecutionError::ExecutionTimeout);
        }
        let task = tokio::task::spawn_blocking(move || run_with_permit(permit, operation));
        match tokio::time::timeout(remaining, task).await {
            Ok(result) => result.map_err(ExecutionError::Join),
            Err(_) => Err(ExecutionError::ExecutionTimeout),
        }
    }

    pub fn uptime(&self) -> Duration {
        self.started_at.elapsed()
    }

    pub fn mrms_ingest_monitor(&self) -> MrmsIngestMonitor {
        self.mrms_ingest.clone()
    }

    pub fn nexrad_level2_ingest_monitor(&self) -> NexradLevel2IngestMonitor {
        self.nexrad_level2_ingest.clone()
    }
}

fn run_with_permit<T, F>(_permit: OwnedSemaphorePermit, operation: F) -> T
where
    F: FnOnce() -> T,
{
    operation()
}

#[derive(Debug, Error)]
pub enum ExecutionError {
    #[error("query admission timed out")]
    AdmissionTimeout,
    #[error("query execution exceeded its deadline")]
    ExecutionTimeout,
    #[error("service is shutting down")]
    ShuttingDown,
    #[error("blocking query worker failed: {0}")]
    Join(#[source] JoinError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocking_work_runs_under_a_bounded_permit() {
        let mut config = AppConfig::default();
        config.limits.light_concurrency = 1;
        let directory = tempfile::tempdir().unwrap();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        let state = AppState::new(config, TokenSet::default()).unwrap();
        assert_eq!(state.run_light(|| 42).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn synchronous_heavy_work_uses_the_sync_deadline() {
        let mut config = AppConfig::default();
        config.limits.heavy_concurrency = 1;
        config.limits.sync_timeout_seconds = 1;
        config.limits.job_timeout_seconds = 60;
        let directory = tempfile::tempdir().unwrap();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        let state = AppState::new(config, TokenSet::default()).unwrap();
        let permit = state.heavy.clone().acquire_owned().await.unwrap();

        let started = Instant::now();
        let result = state.run_heavy_sync(|| 42).await;
        assert!(matches!(result, Err(ExecutionError::AdmissionTimeout)));
        assert!(started.elapsed() < Duration::from_secs(5));
        drop(permit);
    }

    #[tokio::test]
    async fn admission_and_execution_share_one_wall_clock_budget() {
        let mut config = AppConfig::default();
        config.limits.heavy_concurrency = 1;
        config.limits.sync_timeout_seconds = 1;
        let directory = tempfile::tempdir().unwrap();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        let state = AppState::new(config, TokenSet::default()).unwrap();
        let permit = state.heavy.clone().acquire_owned().await.unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(600));
            drop(permit);
        });

        let started = Instant::now();
        let result = state
            .run_heavy_sync(|| {
                std::thread::sleep(Duration::from_millis(600));
                42
            })
            .await;
        assert!(matches!(result, Err(ExecutionError::ExecutionTimeout)));
        assert!(
            started.elapsed() < Duration::from_millis(1_300),
            "one configured second must not become separate admission and execution seconds"
        );
        release.join().unwrap();
    }
}

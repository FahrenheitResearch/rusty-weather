use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use moka::sync::Cache;
use rw_query::{QueryLimits, StoreCatalog};
use thiserror::Error;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinError;

use crate::{AppConfig, JobError, JobManager, Metrics, TokenSet};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub tokens: Arc<TokenSet>,
    pub catalog: Arc<StoreCatalog>,
    pub metrics: Arc<Metrics>,
    pub jobs: JobManager,
    pub response_cache: Cache<String, Bytes>,
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
        let limits = QueryLimits {
            max_catalog_entries: 10_000,
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
        Ok(Self {
            config: Arc::new(config),
            tokens: Arc::new(tokens),
            catalog: Arc::new(catalog),
            metrics: Arc::new(Metrics::new()),
            jobs,
            response_cache,
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
        config.server.artifact_root = directory.path().join("artifacts");
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
        config.server.artifact_root = directory.path().join("artifacts");
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
        config.server.artifact_root = directory.path().join("artifacts");
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

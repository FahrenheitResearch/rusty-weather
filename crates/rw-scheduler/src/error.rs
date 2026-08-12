use rustwx_core::ModelId;

pub type SchedulerResult<T> = Result<T, SchedulerError>;

#[derive(Debug, thiserror::Error)]
pub enum SchedulerError {
    #[error("invalid scheduler configuration: {0}")]
    InvalidConfig(String),
    #[error("model '{0}' has no ready ingest capability")]
    UnsupportedModel(ModelId),
    #[error("model '{model}' does not publish a {cycle_hour:02}z cycle")]
    UnsupportedCycle { model: ModelId, cycle_hour: u8 },
    #[error("invalid job plan: {0}")]
    InvalidPlan(String),
    #[error("invalid job state: {0}")]
    InvalidState(String),
    #[error("invalid run coverage: {0}")]
    InvalidCoverage(String),
    #[error("scheduler capacity invariant violated: {0}")]
    Capacity(String),
    #[error(transparent)]
    Core(#[from] rustwx_core::RustwxError),
    #[error(transparent)]
    Model(#[from] rustwx_models::ModelError),
    #[error("ingest failed: {0}")]
    Ingest(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
}

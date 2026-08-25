use std::io;

use thiserror::Error;

pub type QueryResult<T> = Result<T, QueryError>;

#[derive(Debug, Error)]
pub enum QueryError {
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("legacy v1 run '{run}' has no canonical UTC origin: {reason}")]
    InvalidLegacyRunSlug { run: String, reason: String },
    #[error("invalid query: {0}")]
    InvalidRequest(String),
    #[error("query limit exceeded for {what}: requested {requested}, limit {limit}")]
    LimitExceeded {
        what: &'static str,
        requested: usize,
        limit: usize,
    },
    #[error("invalid half-open time range: start {start:?} must be before end {end:?}")]
    InvalidTimeRange {
        start: Option<i64>,
        end: Option<i64>,
    },
    #[error("time range selects no stored samples")]
    EmptyTimeSelection,
    #[error("expected valid time {valid_unix} is missing from the run snapshot")]
    MissingExpectedTime { valid_unix: i64 },
    #[error("latitude/longitude ({lat}, {lon}) is outside the stored grid")]
    PointOutsideGrid { lat: f64, lon: f64 },
    #[error("storage slot {0} is absent from this snapshot")]
    UnknownStorageSlot(u16),
    #[error("model '{0}' is not present in the store catalog")]
    UnknownModel(String),
    #[error("run '{run}' is not present for model '{model}'")]
    UnknownRun { model: String, run: String },
    #[error("variable '{0}' is not present in the selected data")]
    UnknownVariable(String),
    #[error("variable '{variable}' does not store a {level_hpa} hPa level")]
    UnknownPressureLevel { variable: String, level_hpa: u16 },
    #[error("variable '{variable}' has kind '{actual}', expected '{expected}'")]
    WrongVariableKind {
        variable: String,
        expected: &'static str,
        actual: String,
    },
    #[error("variable '{variable}' metadata changed between hours: {detail}")]
    InconsistentVariable { variable: String, detail: String },
    #[error("storage slot {slot} manifest inventory does not match its hour metadata")]
    VariableInventoryMismatch { slot: u16 },
    #[error("storage slot {slot} was atomically replaced during the query")]
    SnapshotInvalidated { slot: u16 },
    #[error("the run manifest changed during the query")]
    ManifestInvalidated,
    #[error("query cancelled")]
    Cancelled,
    #[error("variable '{variable}' is missing from storage slot {slot}")]
    MissingVariable { variable: String, slot: u16 },
    #[error("variable '{variable}' has a non-finite value at slot {slot}, x={x}, y={y}")]
    MissingValue {
        variable: String,
        slot: u16,
        x: usize,
        y: usize,
    },
    #[error(
        "variable '{variable}' has invalid categorical value {value} at slot {slot}, x={x}, y={y}"
    )]
    InvalidCategory {
        variable: String,
        slot: u16,
        x: usize,
        y: usize,
        value: f32,
    },
    #[error("cannot allocate {what}: {detail}")]
    Allocation { what: &'static str, detail: String },
}

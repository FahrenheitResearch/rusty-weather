mod derived;
mod mosaic;
mod mrms;
mod nexrad;
mod store;
mod types;

pub use derived::*;
pub use mosaic::*;
pub use mrms::*;
pub use nexrad::*;
pub use store::*;
pub use types::*;

use thiserror::Error;

pub type ObservationResult<T> = Result<T, ObservationError>;

#[derive(Debug, Error)]
pub enum ObservationError {
    #[error("invalid observation request: {0}")]
    Invalid(String),
    #[error("MRMS ingest failed: {0}")]
    Mrms(String),
    #[error("NEXRAD ingest failed: {0}")]
    Nexrad(String),
    #[error("observation transform failed: {0}")]
    Transform(String),
    #[error(transparent)]
    Core(#[from] rustwx_core::RustwxError),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
    #[error(transparent)]
    Query(#[from] rw_query::QueryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    RustwxIo(#[from] rustwx_io::IoError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub const DEFAULT_MAXIMUM_GRID_CELLS: usize = 8_000_000;

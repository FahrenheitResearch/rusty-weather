//! Bounded, HTTP-independent queries over validated `rw-store` runs.
//!
//! A [`RunSnapshot`] pins one validated manifest, grid, and physical time
//! axis. Every opened hour is validated against that snapshot, preventing a
//! query from silently mixing cycles or grids. Phase one reduces one complete
//! field at a time and retains only output accumulators. A tile-first reducer
//! is the planned follow-up for grids that exceed that explicit memory bound.

mod capability;
mod catalog;
mod error;
mod point;
mod profile;
mod reduce;
mod snapshot;
mod temporal;
mod time;
mod types;

pub use capability::*;
pub use catalog::StoreCatalog;
pub use error::{QueryError, QueryResult};
pub use point::query_point_series;
pub use profile::query_profile;
pub use reduce::reduce_scalar_temporal;
pub use snapshot::RunSnapshot;
pub use temporal::*;
pub use time::parse_legacy_run_origin_unix;
pub use types::*;

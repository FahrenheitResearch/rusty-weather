//! HTTP-independent queries over validated `rw-store` runs.
//!
//! A [`RunSnapshot`] pins one validated manifest, grid, and physical time
//! axis. Every opened hour is validated against that snapshot, preventing a
//! query from silently mixing cycles or grids. Query allocations use checked
//! arithmetic and fallible reservation. Hosts that need admission control pass
//! explicit [`QueryLimits`]; direct/library defaults do not silently reduce
//! valid data cardinality or resolution.

mod capability;
mod catalog;
mod error;
mod geographic;
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
pub use geographic::*;
pub use point::query_point_series;
pub use profile::{query_profile, query_profile_cycle, query_profile_cycle_with_cancel};
pub use reduce::reduce_scalar_temporal;
pub use snapshot::{
    PressureLevelField2D, RunSnapshot, SurfaceField2D, ensure_variable_metadata_compatible,
};
pub use temporal::*;
pub use time::{
    parse_legacy_observation_day_origin_unix, parse_legacy_observation_hhmm_slot,
    parse_legacy_run_origin_unix,
};
pub use types::*;

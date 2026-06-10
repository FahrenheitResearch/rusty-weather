//! Live GRIB -> `.rws` ingest as a library: the exact fetch/extract/derive/
//! write flow the `rw_ingest`/`rw_batch` bins have always run, extracted
//! from their `#[path]`-shared modules so interactive hosts (the
//! rusty-weather-ui shell) can run ingests in-process on background
//! threads with progress events and cancellation.
//!
//! Layout mirrors the old bin-side modules one-to-one:
//! - [`ingest_hour`] (re-exported at the crate root): per-hour
//!   [`fetch_hour`] / [`process_fetched_hour`] / [`ingest_hour()`],
//!   [`IngestConfig`], [`planned_store_variables`], [`parse_hours`].
//! - [`ingest_profile`]: what one run fetches/extracts/computes/stores
//!   (`full` / `sounding` / `view` presets + overrides + validation).
//! - [`ingest_compute`]: the derived/heavy precompute over the products
//!   decode lane.
//! - [`size_estimate`]: exact (`walk_hour_sizes`) and predictive
//!   (`estimate` against a [`size_estimate::Calibration`]) sizing.
//! - [`throttle`]: polite scheduling — the bins' process-wide knobs plus
//!   the per-thread / dedicated-pool variants for interactive hosts.
//! - [`events`] (re-exported at the crate root): [`IngestEvent`] progress
//!   stream, [`IngestStage`], [`IngestError`] (with a `Cancelled` variant),
//!   and [`print_event`] — the sink that reproduces the bins' historical
//!   stdout/stderr lines byte-for-byte.

mod events;
pub mod ingest_hour;
pub mod throttle;

// Child modules of `ingest_hour` historically; kept reachable both ways.
pub use ingest_hour::ingest_compute;
pub use ingest_hour::ingest_profile;
pub use ingest_hour::size_estimate;

pub use events::{IngestError, IngestEvent, IngestStage, NEVER_CANCEL, print_event};
pub use ingest_hour::{
    FetchedHour, IngestConfig, IngestedHour, PlannedStoreVariables, VolumeSummary, cache_state,
    fetch_hour, ingest_hour as ingest_hour_serial, parse_hours, planned_store_variables,
    process_fetched_hour,
};

/// Short git SHA (plus `-dirty`) of the build that produced this crate, the
/// same stamp `write_hour_from_fields_with_derived` records in `run.json`.
pub fn build_sha() -> &'static str {
    env!("RW_BUILD_SHA")
}

/// Whether this crate can ingest `model` today. The flow hardcodes the
/// HRRR-style `prs`/`sfc` product-file pair (and the surface plan mirrors
/// the HRRR sfc inventory), so only HRRR is supported until per-model fetch
/// plans land. UI pickers gate enablement on this so the list self-updates.
pub fn ingest_supported(model: rustwx_core::ModelId) -> bool {
    matches!(model, rustwx_core::ModelId::Hrrr)
}

/// Crate-local profiling scope: expands to `puffin::profile_scope!` under
/// the `profiling` feature and to nothing otherwise, so call sites stay
/// clean and headless bins compile puffin out entirely.
#[cfg(feature = "profiling")]
macro_rules! profile_scope {
    ($($arg:tt)*) => {
        puffin::profile_scope!($($arg)*);
    };
}
#[cfg(not(feature = "profiling"))]
macro_rules! profile_scope {
    ($($arg:tt)*) => {};
}
pub(crate) use profile_scope;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_sha_is_stamped() {
        assert!(!build_sha().is_empty());
    }

    #[test]
    fn only_hrrr_is_ingest_supported_today() {
        use rustwx_core::ModelId;
        assert!(ingest_supported(ModelId::Hrrr));
        for model in rustwx_models::supported_models() {
            if model != ModelId::Hrrr {
                assert!(
                    !ingest_supported(model),
                    "{model} must stay gated until its fetch plan exists"
                );
            }
        }
    }
}

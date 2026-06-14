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
    FetchedHour, IngestConfig, IngestedHour, PlannedStoreVariables, SpilledFetchedHour,
    VolumeSummary, cache_state, fetch_hour, ingest_hour as ingest_hour_serial, parse_hours,
    planned_store_variables, process_fetched_hour, validate_forecast_hours,
};

/// Short git SHA (plus `-dirty`) of the build that produced this crate, the
/// same stamp `write_hour_from_fields_with_derived` records in `run.json`.
pub fn build_sha() -> &'static str {
    env!("RW_BUILD_SHA")
}

/// One product file to fetch for an hour and the roles its messages serve.
/// The per-hour flow has two extraction roles — a pressure-source file (the
/// 3D isobaric volumes + render-grade isobaric planes + the prs-side thermo
/// decode) and a surface-source file (the 2D surface set + the surface-side
/// thermo decode). HRRR splits them across two physical files (`prs`/`sfc`);
/// GFS/RAP single pressure-grid files carry both, so one entry sets both
/// roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductFetch {
    /// Product token passed to [`rustwx_core::ModelRunRequest::new`].
    pub product: &'static str,
    /// The 2D surface field extraction reads this file.
    pub surface_source: bool,
    /// The 3D volume / isobaric-plane extraction reads this file.
    pub pressure_source: bool,
}

/// The per-model fetch plan: which product file(s) one hour downloads and
/// which extraction roles each serves. HRRR keeps its historical two-file
/// pair (pressure = `prs`, surface = `sfc`) in that exact order so its
/// fetch URLs and extraction sequence stay byte-identical; GFS/RAP fetch
/// one pressure-grid file once and serve both roles from it.
///
/// Models that are not ingest-supported (see [`ingest_supported`]) return an
/// error rather than a plan — callers gate on `ingest_supported` first, so
/// this is a defensive guard, not the primary check.
pub fn fetch_plan(model: rustwx_core::ModelId) -> Result<Vec<ProductFetch>, IngestError> {
    use rustwx_core::ModelId;
    match model {
        ModelId::Hrrr => Ok(vec![
            ProductFetch {
                product: "prs",
                surface_source: false,
                pressure_source: true,
            },
            ProductFetch {
                product: "sfc",
                surface_source: true,
                pressure_source: false,
            },
        ]),
        ModelId::Gfs => Ok(vec![ProductFetch {
            product: "pgrb2.0p25",
            surface_source: true,
            pressure_source: true,
        }]),
        ModelId::Rap => Ok(vec![ProductFetch {
            product: "awp130pgrb",
            surface_source: true,
            pressure_source: true,
        }]),
        other => Err(events::other(format!(
            "model '{other}' has no ingest fetch plan (not ingest-supported)"
        ))),
    }
}

/// Whether this crate can ingest `model` today. Backed by [`fetch_plan`]:
/// a model is ingest-supported exactly when a per-model fetch plan exists
/// for it (HRRR's `prs`/`sfc` pair, GFS/RAP single pressure-grid files).
/// UI pickers gate enablement on this so the list self-updates as plans
/// land.
pub fn ingest_supported(model: rustwx_core::ModelId) -> bool {
    fetch_plan(model).is_ok()
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
    fn hrrr_gfs_and_rap_are_ingest_supported() {
        use rustwx_core::ModelId;
        assert!(ingest_supported(ModelId::Hrrr));
        assert!(ingest_supported(ModelId::Gfs));
        assert!(ingest_supported(ModelId::Rap));
        // Every other catalog model stays gated until its fetch plan lands.
        for model in rustwx_models::supported_models() {
            if !matches!(model, ModelId::Hrrr | ModelId::Gfs | ModelId::Rap) {
                assert!(
                    !ingest_supported(model),
                    "{model} must stay gated until its fetch plan exists"
                );
            }
        }
        // A model with no plan (e.g. NBM) is explicitly unsupported.
        assert!(!ingest_supported(ModelId::Nbm));
    }

    #[test]
    fn fetch_plan_hrrr_is_the_historical_two_file_pair() {
        use rustwx_core::ModelId;
        let plan = fetch_plan(ModelId::Hrrr).expect("HRRR plan");
        assert_eq!(plan.len(), 2, "HRRR fetches prs + sfc");
        // Order is load-bearing: pressure (prs) first, surface (sfc) second,
        // matching the historical fetch sequence.
        assert_eq!(plan[0].product, "prs");
        assert!(plan[0].pressure_source && !plan[0].surface_source);
        assert_eq!(plan[1].product, "sfc");
        assert!(plan[1].surface_source && !plan[1].pressure_source);
    }

    #[test]
    fn fetch_plan_gfs_is_one_file_serving_both_roles() {
        use rustwx_core::ModelId;
        let plan = fetch_plan(ModelId::Gfs).expect("GFS plan");
        assert_eq!(plan.len(), 1, "GFS fetches a single pgrb2.0p25 file");
        assert_eq!(plan[0].product, "pgrb2.0p25");
        assert!(
            plan[0].surface_source && plan[0].pressure_source,
            "the one GFS file serves both the surface and pressure roles"
        );
    }

    #[test]
    fn fetch_plan_rap_is_one_file_serving_both_roles() {
        use rustwx_core::ModelId;
        let plan = fetch_plan(ModelId::Rap).expect("RAP plan");
        assert_eq!(plan.len(), 1, "RAP fetches a single awp130pgrb file");
        assert_eq!(plan[0].product, "awp130pgrb");
        assert!(
            plan[0].surface_source && plan[0].pressure_source,
            "the RAP pressure-grid file serves both the surface and pressure roles"
        );
    }

    #[test]
    fn fetch_plan_rejects_unsupported_model() {
        use rustwx_core::ModelId;
        let err = fetch_plan(ModelId::Nbm).expect_err("NBM has no fetch plan");
        assert!(!err.is_cancelled());
        assert!(
            err.to_string().contains("no ingest fetch plan"),
            "got: {err}"
        );
    }

    /// The GFS fetch-plan token resolves to a well-formed AWS GRIB URL
    /// through the same `ModelRunRequest` -> `resolve_urls` path the ingest
    /// fetch uses. AWS is GFS source priority 2 (NOMADS is 1), so the test
    /// picks the AWS entry explicitly and asserts the exact archive URL.
    #[test]
    fn gfs_fetch_plan_token_resolves_a_well_formed_aws_url() {
        use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, SourceId};
        let plan = fetch_plan(ModelId::Gfs).expect("GFS plan");
        let cycle = CycleSpec::new("20260414", 18).expect("valid cycle");
        let request =
            ModelRunRequest::new(ModelId::Gfs, cycle, 12, plan[0].product).expect("GFS request");
        let urls = rustwx_models::resolve_urls(&request).expect("GFS urls resolve");
        let aws = urls
            .iter()
            .find(|url| url.source == SourceId::Aws)
            .expect("AWS is a GFS source");
        assert_eq!(
            aws.grib_url,
            "https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20260414/18/atmos/gfs.t18z.pgrb2.0p25.f012"
        );
    }

    #[test]
    fn rap_fetch_plan_token_resolves_a_well_formed_aws_url() {
        use rustwx_core::{CycleSpec, ModelId, ModelRunRequest, SourceId};
        let plan = fetch_plan(ModelId::Rap).expect("RAP plan");
        let cycle = CycleSpec::new("20260502", 0).expect("valid cycle");
        let request =
            ModelRunRequest::new(ModelId::Rap, cycle, 21, plan[0].product).expect("RAP request");
        let urls = rustwx_models::resolve_urls(&request).expect("RAP urls resolve");
        let aws = urls
            .iter()
            .find(|url| url.source == SourceId::Aws)
            .expect("AWS is a RAP source");
        assert_eq!(
            aws.grib_url,
            "https://noaa-rap-pds.s3.amazonaws.com/rap.20260502/rap.t00z.awp130pgrbf21.grib2"
        );
    }
}

//! Brand-neutral scheduling core and executable host for building and
//! maintaining [`rw_store`](https://docs.rs/rw-store) model runs.
//!
//! Pure planning/state/coverage/retention APIs remain injectable and
//! deterministic. [`SchedulerHost`] composes them with provider discovery,
//! bounded ingest workers, durable restart recovery, disk admission, graceful
//! cancellation, and dry-run-first retention for operational deployments.

mod durable;

pub mod alias;
pub mod config;
pub mod coverage;
pub mod error;
pub mod executor;
pub mod limits;
pub mod plan;
pub mod retention;
pub mod state;

pub use alias::{RunCandidate, select_latest, select_latest_available, select_latest_covering};
pub use coverage::{RunCoverage, SlotMismatch, ValidTime, verify_manifest, verify_run_json};
pub use error::{SchedulerError, SchedulerResult};
pub use executor::{
    CycleDiscovery, DiscoveredCycle, DiscoveredModelCycle, DiscoveryReport, ExecutionReport,
    ProviderCycleDiscovery, SchedulerHost, StatusReport, deterministic_jittered_delay,
};
pub use limits::{AdmissionDecision, SchedulerLimits};
pub use plan::{
    ExpectedValidTime, IngestProductPlan, JOB_PLAN_SCHEMA, JobPlan, PersistedIngestProfile,
    canonical_job_id, canonical_run_id, cycle_origin_unix,
};
pub use retention::{
    RetentionExecution, RetentionPlan, RetentionRun, RunKey, execute_retention,
    plan_owned_retention, plan_retention,
};
pub use state::{JOB_STATE_SCHEMA, JobRecord, JobState, JobStateStore, RetryPolicy};

#[cfg(test)]
mod tests;

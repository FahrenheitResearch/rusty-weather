//! Hardened, self-hosted HTTP delivery for Rusty Weather stores.
//!
//! The transport layer is deliberately thin. All weather-data semantics live
//! in `rw-query`; this crate owns configuration, authentication, admission
//! control, observability, HTTP problem responses, and process lifecycle.

pub mod auth;
pub mod community;
pub mod community_store;
pub mod config;
pub mod jobs;
pub mod metrics;
pub mod openapi;
pub mod problem;
pub mod routes;
pub mod state;

pub use auth::{AuthError, TokenSet};
pub use config::{AppConfig, ConfigError};
pub use jobs::{ArtifactRef, CancellationToken, JobError, JobManager, JobStatus, JobView};
pub use metrics::Metrics;
pub use problem::ProblemDetails;
pub use routes::build_router;
pub use state::{AppState, ExecutionError};

/// Generate the versioned service-configuration schema used by both the CLI
/// and the checked-in contract drift test.
pub fn config_schema_document() -> schemars::Schema {
    schemars::schema_for!(AppConfig)
}

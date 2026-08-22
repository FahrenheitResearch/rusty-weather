//! Hardened, self-hosted HTTP delivery for Rusty Weather stores.
//!
//! The transport layer is deliberately thin. All weather-data semantics live
//! in `rw-query`; this crate owns configuration, authentication, admission
//! control, observability, HTTP problem responses, and process lifecycle.

pub mod auth;
pub mod community;
pub mod community_relay;
pub mod community_relay_provider;
pub mod community_store;
pub mod config;
pub mod federation;
pub mod federation_proxy;
pub mod generation_replication;
pub mod jobs;
pub mod metrics;
pub mod observations;
pub mod openapi;
pub mod origin_catalog;
pub mod problem;
pub mod routes;
pub mod satellite;
pub mod state;

pub use auth::{AuthError, TokenSet};
pub use config::{AppConfig, ConfigError};
pub use federation::{
    FederationError, FederationHealthStatus, FederationOriginHealthState,
    FederationOriginHealthStatus, FederationService,
};
pub use generation_replication::{
    GenerationReplicationError, ReplicationGarbageCollectionResponse, ReplicationKillSwitchRequest,
    ReplicationStatusResponse, ServerGenerationReplication,
};
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

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::Path;

    /// Write a test credential with the same private-file invariant enforced
    /// by production loaders. Linux CI otherwise creates fixtures as 0644,
    /// which correctly fails the production 0600 gate.
    pub(crate) fn write_private_file(path: &Path, bytes: impl AsRef<[u8]>) {
        std::fs::write(path, bytes).expect("write private test credential");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .expect("set private test credential permissions");
        }
    }
}

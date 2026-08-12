//! Authenticated HTTP-facing seam for advanced full-generation replication.
//!
//! This module never participates in operational local/R2/HTTPS delivery or
//! historical Community Cache relay selection. It domain-separates the normal
//! bearer principal before passing an owner identity to the network-neutral
//! replication engine, and exposes only coarse identity-free operator state.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rw_community_protocol::{
    BeginRunGenerationRequest, FinalizeRunGenerationRequest, PublishedRunGeneration,
    RevokeRunGenerationRequest, RunGenerationMissingPage, RunGenerationTombstone,
    RunGenerationUploadStatus, SignedRunGenerationManifest,
};
use rw_generation_replication::{
    AuthenticatedOwner, FinalizeOutcome, GarbageCollectionReport, GenerationReplicationService,
    ReplicationConfig, ReplicationError, ReplicationServiceStatus,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use utoipa::ToSchema;

use crate::config::GenerationReplicationConfig;

const OWNER_DOMAIN: &[u8] = b"rw-server-generation-replication-owner-v1\0";
const MAX_SECRET_BYTES: u64 = 64 * 1024;
pub const REPLICATION_KILL_SWITCH_SCHEMA: &str = "rw.server.generation-replication-kill-switch.v1";
pub const REPLICATION_STATUS_SCHEMA: &str = "rw.server.generation-replication-status.v1";
pub const REPLICATION_GC_SCHEMA: &str = "rw.server.generation-replication-gc.v1";
pub const REPLICATION_OWNER_SCHEMA: &str = "rw.server.generation-replication-owner.v1";

#[derive(Debug, Error)]
pub enum GenerationReplicationError {
    #[error("generation replication is disabled")]
    Disabled,
    #[error("generation replication operator authorization was rejected")]
    Forbidden,
    #[error("generation replication request is invalid")]
    Invalid,
    #[error("generation replication secret is unsafe or malformed")]
    UnsafeSecret,
    #[error(transparent)]
    Engine(#[from] ReplicationError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplicationKillSwitchRequest {
    pub schema: String,
    pub engaged: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, ToSchema)]
pub struct ReplicationStatusResponse {
    pub schema: String,
    pub enabled: bool,
    pub kill_switch: bool,
    pub healthy: bool,
    pub active_uploads: usize,
    pub published_generations: usize,
    pub tombstones: usize,
    /// Terminal, non-public generations awaiting bounded physical cleanup.
    pub pending_retirements: usize,
    pub reserved_bytes: u64,
    pub published_bytes: u64,
    pub pending_retirement_bytes: u64,
    pub monthly_accepted_upload_bytes: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema, ToSchema)]
pub struct ReplicationGarbageCollectionResponse {
    pub schema: String,
    pub expired_uploads: usize,
    pub expired_publications: usize,
    pub retired_generations: usize,
    pub pending_retirements: usize,
    pub orphan_chunks: usize,
    pub orphan_manifests: usize,
    pub stale_candidates: usize,
}

/// The caller's replication-domain owner identifier. It is derived from the
/// already authenticated principal and reveals neither the bearer token nor
/// any other owner's identity.
#[derive(Debug, Clone, Serialize, JsonSchema, ToSchema)]
pub struct ReplicationOwnerResponse {
    pub schema: String,
    pub owner_principal_sha256: String,
}

#[derive(Clone, Default)]
pub struct ServerGenerationReplication {
    engine: Option<Arc<GenerationReplicationService>>,
    operator_principals: Arc<BTreeSet<String>>,
}

impl std::fmt::Debug for ServerGenerationReplication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerGenerationReplication")
            .field("enabled", &self.engine.is_some())
            .finish_non_exhaustive()
    }
}

impl ServerGenerationReplication {
    pub fn open(
        config: &GenerationReplicationConfig,
        store_root: &Path,
    ) -> Result<Self, GenerationReplicationError> {
        if !config.enabled {
            return Ok(Self::default());
        }
        let signing_key = load_signing_key(
            config
                .signing_key_file
                .as_deref()
                .ok_or(GenerationReplicationError::UnsafeSecret)?,
        )?;
        let engine = GenerationReplicationService::open(ReplicationConfig {
            enabled: true,
            control_root: config.control_root.clone(),
            store_root: store_root.to_path_buf(),
            limits: config.protocol_limits(),
            policy: config.replication_policy(),
            signing_key_id: config.signing_key_id.clone(),
            signing_key,
        })?;
        if config.kill_switch {
            engine.set_kill_switch(true)?;
        }
        Ok(Self {
            engine: Some(Arc::new(engine)),
            operator_principals: Arc::new(config.operator_principals.iter().cloned().collect()),
        })
    }

    pub fn is_enabled(&self) -> bool {
        self.engine.is_some()
    }

    pub fn begin(
        &self,
        principal: &str,
        request: BeginRunGenerationRequest,
    ) -> Result<RunGenerationUploadStatus, GenerationReplicationError> {
        self.engine()?
            .begin(&owner(principal)?, request, now_unix())
            .map_err(Into::into)
    }

    pub fn owner_identity(
        &self,
        principal: &str,
    ) -> Result<ReplicationOwnerResponse, GenerationReplicationError> {
        self.engine()?;
        let owner = owner(principal)?;
        Ok(ReplicationOwnerResponse {
            schema: REPLICATION_OWNER_SCHEMA.into(),
            owner_principal_sha256: owner.sha256().into(),
        })
    }

    /// Internal coarse state for startup diagnostics and metrics. HTTP callers
    /// must use `operator_status`, which checks the configured operator set.
    pub fn startup_status(&self) -> Result<ReplicationStatusResponse, GenerationReplicationError> {
        let status = self.engine()?.service_status()?;
        Ok(status_response(status))
    }

    pub fn upload_status(
        &self,
        principal: &str,
        generation_id: &str,
    ) -> Result<RunGenerationUploadStatus, GenerationReplicationError> {
        require_generation_id(generation_id)?;
        self.engine()?
            .status(&owner(principal)?, generation_id, now_unix())
            .map_err(Into::into)
    }

    pub fn missing_chunks(
        &self,
        principal: &str,
        generation_id: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<RunGenerationMissingPage, GenerationReplicationError> {
        require_generation_id(generation_id)?;
        self.engine()?
            .missing_chunks(&owner(principal)?, generation_id, after, limit, now_unix())
            .map_err(Into::into)
    }

    pub fn upload_chunk(
        &self,
        principal: &str,
        generation_id: &str,
        sha256: &str,
        bytes: &[u8],
    ) -> Result<(), GenerationReplicationError> {
        require_generation_id(generation_id)?;
        require_sha256(sha256)?;
        self.engine()?
            .upload_chunk(&owner(principal)?, generation_id, sha256, bytes, now_unix())
            .map_err(Into::into)
    }

    pub fn finalize(
        &self,
        principal: &str,
        generation_id: &str,
        request: FinalizeRunGenerationRequest,
    ) -> Result<FinalizeOutcome, GenerationReplicationError> {
        require_generation_id(generation_id)?;
        self.engine()?
            .finalize(&owner(principal)?, generation_id, request, now_unix())
            .map_err(Into::into)
    }

    pub fn revoke(
        &self,
        principal: &str,
        generation_id: &str,
        request: RevokeRunGenerationRequest,
    ) -> Result<RunGenerationTombstone, GenerationReplicationError> {
        require_generation_id(generation_id)?;
        self.engine()?
            .revoke(&owner(principal)?, generation_id, request, now_unix())
            .map_err(Into::into)
    }

    pub fn set_kill_switch(
        &self,
        principal: &str,
        request: ReplicationKillSwitchRequest,
    ) -> Result<ReplicationStatusResponse, GenerationReplicationError> {
        self.require_operator(principal)?;
        if request.schema != REPLICATION_KILL_SWITCH_SCHEMA {
            return Err(GenerationReplicationError::Invalid);
        }
        self.engine()?.set_kill_switch(request.engaged)?;
        self.operator_status(principal)
    }

    pub fn operator_status(
        &self,
        principal: &str,
    ) -> Result<ReplicationStatusResponse, GenerationReplicationError> {
        self.require_operator(principal)?;
        let status = self.engine()?.service_status()?;
        Ok(status_response(status))
    }

    pub fn garbage_collect(
        &self,
        principal: &str,
    ) -> Result<ReplicationGarbageCollectionResponse, GenerationReplicationError> {
        self.require_operator(principal)?;
        let report = self.engine()?.garbage_collect(now_unix())?;
        Ok(gc_response(report))
    }

    pub fn authorized_publications(
        &self,
    ) -> Result<Vec<PublishedRunGeneration>, GenerationReplicationError> {
        self.engine()?.authorized_publications().map_err(Into::into)
    }

    pub fn authorize_query(
        &self,
        model: &str,
        run: &str,
    ) -> Result<PublishedRunGeneration, GenerationReplicationError> {
        self.engine()?
            .authorize_query(model, run)
            .map_err(Into::into)
    }

    pub fn signed_manifest(
        &self,
        generation_id: &str,
    ) -> Result<Option<SignedRunGenerationManifest>, GenerationReplicationError> {
        require_generation_id(generation_id)?;
        Ok(self
            .engine()?
            .published(generation_id)?
            .map(|outcome| outcome.signed_manifest))
    }

    fn engine(&self) -> Result<&Arc<GenerationReplicationService>, GenerationReplicationError> {
        self.engine
            .as_ref()
            .ok_or(GenerationReplicationError::Disabled)
    }

    fn require_operator(&self, principal: &str) -> Result<(), GenerationReplicationError> {
        if self.operator_principals.contains(principal) {
            Ok(())
        } else {
            Err(GenerationReplicationError::Forbidden)
        }
    }
}

fn status_response(status: ReplicationServiceStatus) -> ReplicationStatusResponse {
    ReplicationStatusResponse {
        schema: REPLICATION_STATUS_SCHEMA.into(),
        enabled: status.enabled,
        kill_switch: status.kill_switch,
        healthy: status.enabled,
        active_uploads: status.active_uploads,
        published_generations: status.published_generations,
        tombstones: status.tombstones,
        pending_retirements: status.pending_retirements,
        reserved_bytes: status.reserved_bytes,
        published_bytes: status.published_bytes,
        pending_retirement_bytes: status.pending_retirement_bytes,
        monthly_accepted_upload_bytes: status.monthly_accepted_upload_bytes,
    }
}

fn gc_response(report: GarbageCollectionReport) -> ReplicationGarbageCollectionResponse {
    ReplicationGarbageCollectionResponse {
        schema: REPLICATION_GC_SCHEMA.into(),
        expired_uploads: report.expired_uploads,
        expired_publications: report.expired_publications,
        retired_generations: report.retired_generations,
        pending_retirements: report.pending_retirements,
        orphan_chunks: report.orphan_chunks,
        orphan_manifests: report.orphan_manifests,
        stale_candidates: report.stale_candidates,
    }
}

fn owner(principal: &str) -> Result<AuthenticatedOwner, GenerationReplicationError> {
    if !is_lower_sha256(principal) {
        return Err(GenerationReplicationError::Invalid);
    }
    let mut digest = Sha256::new();
    digest.update(OWNER_DOMAIN);
    digest.update(principal.as_bytes());
    AuthenticatedOwner::from_sha256(format!("{:x}", digest.finalize())).map_err(Into::into)
}

fn require_generation_id(value: &str) -> Result<(), GenerationReplicationError> {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(GenerationReplicationError::Invalid)
    }
}

fn require_sha256(value: &str) -> Result<(), GenerationReplicationError> {
    if is_lower_sha256(value) {
        Ok(())
    } else {
        Err(GenerationReplicationError::Invalid)
    }
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn load_signing_key(path: &Path) -> Result<SigningKey, GenerationReplicationError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_SECRET_BYTES
    {
        return Err(GenerationReplicationError::UnsafeSecret);
    }
    validate_private_permissions(&metadata)?;
    let secret = fs::read_to_string(path)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(secret.trim())
        .map_err(|_| GenerationReplicationError::UnsafeSecret)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| GenerationReplicationError::UnsafeSecret)?;
    Ok(SigningKey::from_bytes(&bytes))
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &fs::Metadata) -> Result<(), GenerationReplicationError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        Err(GenerationReplicationError::UnsafeSecret)
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn validate_private_permissions(
    _metadata: &fs::Metadata,
) -> Result<(), GenerationReplicationError> {
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_identity_is_domain_separated_and_paths_are_closed() {
        let auth_principal = "ab".repeat(32);
        let replication_owner = owner(&auth_principal).unwrap();
        assert_ne!(replication_owner.sha256(), auth_principal);
        assert_eq!(replication_owner.sha256().len(), 64);
        for hostile in ["../run", "a/b", "a\\b", ".", "", "run.json"] {
            assert!(require_generation_id(hostile).is_err());
        }
        assert!(require_generation_id("wrf-case-20260812").is_ok());
        assert!(require_sha256(&"af".repeat(32)).is_ok());
        assert!(require_sha256(&"AF".repeat(32)).is_err());
    }

    #[test]
    fn disabled_service_does_not_create_or_open_state() {
        let config = GenerationReplicationConfig::default();
        let service = ServerGenerationReplication::open(&config, Path::new("not-used")).unwrap();
        assert!(!service.is_enabled());
        assert!(matches!(
            service.operator_status(&"ab".repeat(32)),
            Err(GenerationReplicationError::Forbidden)
        ));
    }
}

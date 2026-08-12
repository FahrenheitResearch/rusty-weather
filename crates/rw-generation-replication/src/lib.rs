//! Durable, fail-closed replication of complete immutable rw-store runs.
//!
//! The network-neutral service accepts only the closed file inventory defined
//! by `rw-community-protocol`: `run.json`, `grid.rwg`, and registered `.rws`
//! hour files. Chunks are content addressed, publication is candidate-first,
//! and a generation is not visible until store-deep validation and an
//! `rw-query` snapshot open both succeed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine as _;
use chrono::{DateTime, Datelike, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use fs4::{FileExt, TryLockError};
use rw_community_protocol::{
    BeginRunGenerationRequest, CANCELLED_RUN_GENERATION_SCHEMA, CancelledRunGeneration,
    FinalizeRunGenerationRequest, PUBLISHED_RUN_GENERATION_SCHEMA, PublishedRunGeneration,
    RUN_GENERATION_MISSING_PAGE_SCHEMA, RUN_GENERATION_OWNER_CAPABILITIES_SCHEMA,
    RUN_GENERATION_OWNER_LIST_SCHEMA, RUN_GENERATION_OWNER_RECORD_SCHEMA,
    RUN_GENERATION_TOMBSTONE_SCHEMA, RUN_GENERATION_UPLOAD_STATUS_SCHEMA,
    RevokeRunGenerationRequest, RunGenerationAdvertisedLimits, RunGenerationFile,
    RunGenerationFileKind, RunGenerationLimits, RunGenerationMissingChunk,
    RunGenerationMissingPage, RunGenerationOwnerCapabilities, RunGenerationOwnerListPage,
    RunGenerationOwnerQuota, RunGenerationOwnerRecord, RunGenerationOwnerRecordState,
    RunGenerationOwnerUsage, RunGenerationReplicationManifest, RunGenerationTombstone,
    RunGenerationUploadStatus, SignatureAlgorithm, SignedRunGenerationManifest,
    canonical_run_generation_bytes, sign_run_generation, verify_run_generation_chunk,
};
use rw_query::RunSnapshot;
use rw_store::atomic::atomic_write_bytes;
use rw_store::run::{RwsRunManifest, validate_store_component};
use rw_store::{RunLock, ValidateDepth, validate_run_dir};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

const STATE_SCHEMA: &str = "rw.generation-replication.state.v1";
const STATE_ENVELOPE_SCHEMA: &str = "rw.generation-replication.state-envelope.v1";
const STATE_SIGNATURE_DOMAIN: &[u8] = b"rw-generation-replication-state-v1\0";
const MAX_ABSOLUTE_STATE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_REASONABLE_UPLOAD_TTL: i64 = 7 * 24 * 60 * 60;
const MAX_ABSOLUTE_GC_ENTRIES: usize = 1_000_000;
const SCHEDULER_OWNER_MARKER: &str = ".rw-scheduler-owner.json";
const CLOCK_SKEW_SECONDS: i64 = 300;

#[derive(Debug, Error)]
pub enum ReplicationError {
    #[error("generation replication is disabled")]
    Disabled,
    #[error("generation replication is stopped by the operator kill switch")]
    KillSwitch,
    #[error("authenticated owner identity is invalid")]
    InvalidOwner,
    #[error("the authenticated owner does not own this generation")]
    WrongOwner,
    #[error("generation upload was not found")]
    NotFound,
    #[error("generation upload expired")]
    Expired,
    #[error("generation already exists with a different identity")]
    Conflict,
    #[error("replication quota exceeded: {0}")]
    Quota(&'static str),
    #[error("replication service is already open or the target run is busy")]
    Busy,
    #[error("required chunk is missing")]
    MissingChunk,
    #[error("chunk is not part of the signed generation inventory")]
    UnknownChunk,
    #[error("persistent replication state failed authentication or validation")]
    CorruptState,
    #[error("replicated generation failed closed validation: {0}")]
    InvalidGeneration(String),
    #[error("protocol validation failed: {0}")]
    Protocol(#[from] rw_community_protocol::ProtocolError),
    #[error("store validation failed: {0}")]
    Store(#[from] rw_store::RwStoreError),
    #[error("query validation failed: {0}")]
    Query(#[from] rw_query::QueryError),
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("state encoding failed: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, ReplicationError>;

/// A backend-authenticated principal digest. The engine never accepts or
/// persists a raw username, token, e-mail address, or network address.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AuthenticatedOwner(String);

impl AuthenticatedOwner {
    pub fn from_sha256(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        if is_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(ReplicationError::InvalidOwner)
        }
    }

    pub fn sha256(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for AuthenticatedOwner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("AuthenticatedOwner([redacted])")
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReplicationPolicy {
    pub max_owner_storage_bytes: u64,
    pub max_total_storage_bytes: u64,
    pub max_owner_generations: usize,
    pub max_total_generations: usize,
    pub max_owner_concurrent_uploads: usize,
    pub max_total_concurrent_uploads: usize,
    /// Valid chunk bytes admitted in one UTC calendar month. Every verified
    /// request is charged, including retries/deduplicated replays, and a disk
    /// write failure does not refund the charge.
    pub max_owner_monthly_upload_bytes: u64,
    pub max_total_monthly_upload_bytes: u64,
    pub upload_ttl_seconds: i64,
    pub max_state_bytes: u64,
    /// Maximum directory entries one explicit collection pass may inspect.
    pub max_gc_entries: usize,
    /// Maximum immutable objects/manifests one collection pass may remove.
    pub max_gc_deletions: usize,
}

impl ReplicationPolicy {
    pub fn validate(&self, limits: &RunGenerationLimits) -> Result<()> {
        limits.validate()?;
        if self.max_owner_storage_bytes == 0
            || self.max_total_storage_bytes < self.max_owner_storage_bytes
            || self.max_owner_generations == 0
            || self.max_total_generations < self.max_owner_generations
            || self.max_total_generations > MAX_ABSOLUTE_GC_ENTRIES
            || self.max_owner_concurrent_uploads == 0
            || self.max_total_concurrent_uploads < self.max_owner_concurrent_uploads
            || self.max_owner_monthly_upload_bytes == 0
            || self.max_total_monthly_upload_bytes < self.max_owner_monthly_upload_bytes
            || self.upload_ttl_seconds <= 0
            || self.upload_ttl_seconds > MAX_REASONABLE_UPLOAD_TTL
            || self.max_state_bytes == 0
            || self.max_state_bytes > MAX_ABSOLUTE_STATE_BYTES
            || self.max_gc_entries == 0
            || self.max_gc_entries > MAX_ABSOLUTE_GC_ENTRIES
            || self.max_gc_deletions == 0
            || self.max_gc_deletions > self.max_gc_entries
        {
            return Err(ReplicationError::InvalidGeneration(
                "replication policy is zero, inconsistent, or exceeds a hard safety bound".into(),
            ));
        }
        Ok(())
    }
}

pub struct ReplicationConfig {
    pub enabled: bool,
    pub control_root: PathBuf,
    pub store_root: PathBuf,
    pub limits: RunGenerationLimits,
    pub policy: ReplicationPolicy,
    pub signing_key_id: String,
    pub signing_key: SigningKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinalizeOutcome {
    pub published: PublishedRunGeneration,
    pub signed_manifest: SignedRunGenerationManifest,
    /// True when this call reconciled an already durable exact publication
    /// (for example after a restart or a lost successful HTTP response).
    pub was_already_published: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GarbageCollectionReport {
    pub expired_uploads: usize,
    /// Publications whose exclusive signed retention deadline was crossed and
    /// durably converted to terminal tombstones during this pass.
    pub expired_publications: usize,
    /// Tombstoned generations whose local run bytes were retired and whose
    /// durable cleanup-queue record was removed during this pass.
    pub retired_generations: usize,
    /// Durable, non-public retirements still awaiting a bounded cleanup pass.
    pub pending_retirements: usize,
    pub orphan_chunks: usize,
    pub orphan_manifests: usize,
    pub stale_candidates: usize,
}

/// Coarse, identity-free operator state. It intentionally contains no owner,
/// model, run, source URL, generation id, or filesystem path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReplicationServiceStatus {
    pub enabled: bool,
    pub kill_switch: bool,
    pub active_uploads: usize,
    pub published_generations: usize,
    pub tombstones: usize,
    pub pending_retirements: usize,
    pub reserved_bytes: u64,
    pub published_bytes: u64,
    pub pending_retirement_bytes: u64,
    pub monthly_accepted_upload_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadRecord {
    manifest: RunGenerationReplicationManifest,
    created_unix: i64,
    expires_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedRecord {
    signed_manifest: SignedRunGenerationManifest,
    local_snapshot_id: String,
    published_unix: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistentState {
    schema: String,
    kill_switch: bool,
    uploads: BTreeMap<String, UploadRecord>,
    published: BTreeMap<String, PublishedRecord>,
    tombstones: BTreeMap<String, RunGenerationTombstone>,
    /// Terminal publications awaiting idempotent physical cleanup. The
    /// authenticated state retains the exact manifest and local snapshot so a
    /// restart can retry without ever trusting an arbitrary directory.
    retirements: BTreeMap<String, PublishedRecord>,
    billing: BillingLedger,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BillingLedger {
    utc_month: String,
    owner_upload_bytes: BTreeMap<String, u64>,
    total_upload_bytes: u64,
}

impl Default for BillingLedger {
    fn default() -> Self {
        Self {
            utc_month: "1970-01".into(),
            owner_upload_bytes: BTreeMap::new(),
            total_upload_bytes: 0,
        }
    }
}

impl Default for PersistentState {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.into(),
            kill_switch: false,
            uploads: BTreeMap::new(),
            published: BTreeMap::new(),
            tombstones: BTreeMap::new(),
            retirements: BTreeMap::new(),
            billing: BillingLedger::default(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateEnvelope {
    schema: String,
    signing_key_id: String,
    state: PersistentState,
    signature_base64: String,
}

/// One process owns a control root at a time. Every operation is serialized
/// through `state`; per-run advisory locks additionally coordinate with normal
/// rw-store writers during the final namespace swap.
pub struct GenerationReplicationService {
    enabled: bool,
    store_root: PathBuf,
    chunks_root: PathBuf,
    manifests_root: PathBuf,
    staging_root: PathBuf,
    state_path: PathBuf,
    limits: RunGenerationLimits,
    policy: ReplicationPolicy,
    signing_key_id: String,
    signing_key: SigningKey,
    state: Mutex<PersistentState>,
    _process_lock: File,
    #[cfg(test)]
    fail_next_persist: AtomicBool,
}

impl GenerationReplicationService {
    pub fn open(config: ReplicationConfig) -> Result<Self> {
        config.policy.validate(&config.limits)?;
        validate_key_id(&config.signing_key_id)?;
        create_real_directory(&config.control_root)?;
        create_real_directory(&config.store_root)?;
        let control_root = fs::canonicalize(&config.control_root)?;
        let store_root = fs::canonicalize(&config.store_root)?;
        let chunks_root = control_root.join("chunks");
        let manifests_root = control_root.join("manifests");
        let staging_root = store_root.join(".rw-generation-replication-staging");
        create_real_directory(&chunks_root)?;
        create_real_directory(&manifests_root)?;
        create_real_directory(&staging_root)?;
        let chunks_root = fs::canonicalize(chunks_root)?;
        let manifests_root = fs::canonicalize(manifests_root)?;
        let staging_root = fs::canonicalize(staging_root)?;
        require_contained(&control_root, &chunks_root)?;
        require_contained(&control_root, &manifests_root)?;
        require_contained(&store_root, &staging_root)?;

        let lock_path = control_root.join("service.lock");
        let process_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;
        match FileExt::try_lock(&process_lock) {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(ReplicationError::Busy),
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }

        let state_path = control_root.join("state.json");
        let state = if state_path.exists() {
            load_state(
                &state_path,
                config.policy.max_state_bytes,
                &config.signing_key_id,
                &config.signing_key,
                &config.limits,
            )?
        } else {
            PersistentState::default()
        };
        validate_state(
            &state,
            &config.limits,
            &config.signing_key_id,
            &config.signing_key,
        )?;
        if state
            .published
            .len()
            .saturating_add(state.uploads.len())
            .saturating_add(state.retirements.len())
            > config.policy.max_total_generations
        {
            return Err(ReplicationError::Quota("total generation count"));
        }

        let service = Self {
            enabled: config.enabled,
            store_root,
            chunks_root,
            manifests_root,
            staging_root,
            state_path,
            limits: config.limits,
            policy: config.policy,
            signing_key_id: config.signing_key_id,
            signing_key: config.signing_key,
            state: Mutex::new(state),
            _process_lock: process_lock,
            #[cfg(test)]
            fail_next_persist: AtomicBool::new(false),
        };
        if !service.state_path.exists() {
            let state = service.lock_state()?;
            service.persist(&state)?;
        }
        Ok(service)
    }

    /// Return the exact runtime protocol limits, upload lifetime, per-owner
    /// ceilings, and this owner's durable usage without exposing any global or
    /// other-owner capacity information. This remains available while the kill
    /// switch is engaged so clients can pause safely and reconcile local work.
    pub fn owner_capabilities(
        &self,
        owner: &AuthenticatedOwner,
        now_unix: i64,
    ) -> Result<RunGenerationOwnerCapabilities> {
        self.require_enabled()?;
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        let month = utc_month(now_unix)?;
        let state = self.lock_state()?;
        let uploads: Vec<_> = state
            .uploads
            .values()
            .filter(|record| record.manifest.owner_principal_sha256 == owner.sha256())
            .collect();
        let publications: Vec<_> = state
            .published
            .values()
            .filter(|record| {
                record.signed_manifest.manifest.owner_principal_sha256 == owner.sha256()
            })
            .collect();
        let retirements: Vec<_> = state
            .retirements
            .values()
            .filter(|record| {
                record.signed_manifest.manifest.owner_principal_sha256 == owner.sha256()
            })
            .collect();
        let tombstones = state
            .tombstones
            .values()
            .filter(|record| record.owner_principal_sha256 == owner.sha256())
            .count();
        let active_uploads = usize_as_u64(uploads.len())?;
        let live_publications = usize_as_u64(publications.len())?;
        let pending_retirements = usize_as_u64(retirements.len())?;
        let reserved_bytes = sum_upload_bytes(uploads.iter().copied())?;
        let published_bytes = sum_published_bytes(publications.iter().copied())?;
        let pending_retirement_bytes = sum_published_bytes(retirements.iter().copied())?;
        let monthly_accepted_upload_bytes = if state.billing.utc_month == month {
            state
                .billing
                .owner_upload_bytes
                .get(owner.sha256())
                .copied()
                .unwrap_or(0)
        } else {
            0
        };
        let owner_generations = active_uploads
            .checked_add(live_publications)
            .and_then(|value| value.checked_add(pending_retirements))
            .ok_or(ReplicationError::CorruptState)?;
        let owner_storage = reserved_bytes
            .checked_add(published_bytes)
            .and_then(|value| value.checked_add(pending_retirement_bytes))
            .ok_or(ReplicationError::CorruptState)?;
        let total_generations = state
            .uploads
            .len()
            .saturating_add(state.published.len())
            .saturating_add(state.retirements.len());
        let total_storage = state
            .uploads
            .values()
            .map(|record| record.manifest.total_bytes)
            .chain(
                state
                    .published
                    .values()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .chain(
                state
                    .retirements
                    .values()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
            .ok_or(ReplicationError::CorruptState)?;
        let total_monthly_upload_bytes = if state.billing.utc_month == month {
            state.billing.total_upload_bytes
        } else {
            0
        };
        let capabilities = RunGenerationOwnerCapabilities {
            schema: RUN_GENERATION_OWNER_CAPABILITIES_SCHEMA.into(),
            owner_principal_sha256: owner.sha256().into(),
            accepting_uploads: !state.kill_switch
                && active_uploads < usize_as_u64(self.policy.max_owner_concurrent_uploads)?
                && owner_generations < usize_as_u64(self.policy.max_owner_generations)?
                && owner_storage < self.policy.max_owner_storage_bytes
                && monthly_accepted_upload_bytes < self.policy.max_owner_monthly_upload_bytes
                && state.uploads.len() < self.policy.max_total_concurrent_uploads
                && total_generations < self.policy.max_total_generations
                && total_storage < self.policy.max_total_storage_bytes
                && total_monthly_upload_bytes < self.policy.max_total_monthly_upload_bytes,
            limits: RunGenerationAdvertisedLimits {
                maximum_generation_bytes: self.limits.max_generation_bytes,
                maximum_files: usize_as_u64(self.limits.max_files)?,
                maximum_chunks: usize_as_u64(self.limits.max_chunks)?,
                maximum_chunk_bytes: self.limits.max_chunk_bytes,
                maximum_manifest_bytes: usize_as_u64(self.limits.max_manifest_bytes)?,
                minimum_retention_seconds: 1,
                maximum_retention_seconds: self.limits.max_retention_seconds,
                maximum_provenance_entries: usize_as_u64(self.limits.max_provenance_entries)?,
                maximum_attributions: usize_as_u64(self.limits.max_attributions)?,
                upload_ttl_seconds: self.policy.upload_ttl_seconds,
            },
            quota: RunGenerationOwnerQuota {
                maximum_storage_bytes: self.policy.max_owner_storage_bytes,
                maximum_generations: usize_as_u64(self.policy.max_owner_generations)?,
                maximum_concurrent_uploads: usize_as_u64(self.policy.max_owner_concurrent_uploads)?,
                maximum_monthly_upload_bytes: self.policy.max_owner_monthly_upload_bytes,
            },
            usage: RunGenerationOwnerUsage {
                active_uploads,
                live_publications,
                pending_retirements,
                tombstones: usize_as_u64(tombstones)?,
                reserved_bytes,
                published_bytes,
                pending_retirement_bytes,
                billing_utc_month: month,
                monthly_accepted_upload_bytes,
            },
        };
        capabilities.validate()?;
        Ok(capabilities)
    }

    pub fn begin(
        &self,
        owner: &AuthenticatedOwner,
        request: BeginRunGenerationRequest,
        now_unix: i64,
    ) -> Result<RunGenerationUploadStatus> {
        self.require_enabled()?;
        request.validate(&self.limits)?;
        validate_engine_manifest(&request.manifest)?;
        if request.manifest.owner_principal_sha256 != owner.sha256() {
            return Err(ReplicationError::WrongOwner);
        }
        if now_unix
            < request
                .manifest
                .published_unix
                .saturating_sub(CLOCK_SKEW_SECONDS)
            || now_unix >= request.manifest.retain_until_unix
        {
            return Err(ReplicationError::Expired);
        }
        declared_chunks(&request.manifest)?;
        self.expire_due_publications(now_unix)?;

        let mut state = self.lock_state()?;
        self.require_running(&state)?;
        if state
            .tombstones
            .contains_key(&request.manifest.generation_id)
        {
            return Err(ReplicationError::Conflict);
        }
        if let Some(existing) = state.uploads.get(&request.manifest.generation_id) {
            require_owner(owner, &existing.manifest)?;
            if existing.manifest != request.manifest {
                return Err(ReplicationError::Conflict);
            }
            require_active(existing, now_unix)?;
            return self.status_for(existing);
        }
        if state
            .published
            .contains_key(&request.manifest.generation_id)
        {
            return Err(ReplicationError::Conflict);
        }
        // The concrete rw-store namespace is `(model, run)`, not the opaque
        // upload id. Two generation ids must never reserve or authorize the
        // same directory: retiring either one could otherwise remove bytes
        // still referenced by the other. Perform this admission check before
        // quota accounting or accepting any chunks.
        if namespace_is_occupied_by_other_generation(&state, &request.manifest) {
            return Err(ReplicationError::Conflict);
        }
        self.check_quotas(&state, owner, request.manifest.total_bytes)?;
        let expires_unix = now_unix
            .saturating_add(self.policy.upload_ttl_seconds)
            .min(request.manifest.retain_until_unix);
        let record = UploadRecord {
            manifest: request.manifest,
            created_unix: now_unix,
            expires_unix,
        };
        let generation_id = record.manifest.generation_id.clone();
        state.uploads.insert(generation_id.clone(), record);
        if let Err(error) = self.persist(&state) {
            state.uploads.remove(&generation_id);
            return Err(error);
        }
        self.status_for(state.uploads.get(&generation_id).expect("inserted upload"))
    }

    pub fn status(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        now_unix: i64,
    ) -> Result<RunGenerationUploadStatus> {
        self.require_enabled()?;
        let state = self.lock_state()?;
        self.require_running(&state)?;
        let upload = state
            .uploads
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?;
        require_owner(owner, &upload.manifest)?;
        require_active(upload, now_unix)?;
        self.status_for(upload)
    }

    /// Cancel one active owner upload and durably release its storage/count/
    /// concurrency reservation. Cancellation is intentionally allowed while
    /// the kill switch is engaged; it cannot publish bytes or authorize data.
    pub fn cancel_upload(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        now_unix: i64,
    ) -> Result<CancelledRunGeneration> {
        self.require_enabled()?;
        require_generation_id(generation_id)?;
        if now_unix < 0 {
            return Err(ReplicationError::Expired);
        }
        let mut state = self.lock_state()?;
        let upload = state
            .uploads
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?;
        require_owner(owner, &upload.manifest)?;
        let response = CancelledRunGeneration {
            schema: CANCELLED_RUN_GENERATION_SCHEMA.into(),
            generation_id: generation_id.into(),
            generation_sha256: upload.manifest.generation_sha256.clone(),
            cancelled_unix: now_unix,
            released_reserved_bytes: upload.manifest.total_bytes,
        };
        response.validate()?;
        let previous = state
            .uploads
            .remove(generation_id)
            .expect("owned upload checked above");
        if let Err(error) = self.persist(&state) {
            state.uploads.insert(generation_id.into(), previous);
            return Err(error);
        }
        Ok(response)
    }

    /// Exact owner-only publication/tombstone lookup. An absent generation and
    /// another owner's generation are intentionally indistinguishable.
    pub fn owner_record(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        now_unix: i64,
    ) -> Result<RunGenerationOwnerRecord> {
        self.require_enabled()?;
        require_generation_id(generation_id)?;
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        let state = self.lock_state()?;
        let record = if let Some(record) = state.published.get(generation_id) {
            if record.signed_manifest.manifest.owner_principal_sha256 != owner.sha256() {
                return Err(ReplicationError::NotFound);
            }
            owner_publication_record(record)?
        } else if let Some(tombstone) = state.tombstones.get(generation_id) {
            if tombstone.owner_principal_sha256 != owner.sha256() {
                return Err(ReplicationError::NotFound);
            }
            owner_tombstone_record(tombstone)?
        } else {
            return Err(ReplicationError::NotFound);
        };
        record.validate()?;
        Ok(record)
    }

    /// Deterministic generation-id ordered page containing only this owner's
    /// live publications and terminal tombstones. Active uploads are queried
    /// through their existing exact status route and are not mixed here.
    pub fn owner_records(
        &self,
        owner: &AuthenticatedOwner,
        after: Option<&str>,
        limit: usize,
        now_unix: i64,
    ) -> Result<RunGenerationOwnerListPage> {
        self.require_enabled()?;
        if limit == 0 || limit > rw_community_protocol::MAX_RUN_GENERATION_OWNER_PAGE {
            return Err(ReplicationError::InvalidGeneration(
                "owner generation page limit is invalid".into(),
            ));
        }
        if let Some(cursor) = after {
            require_generation_id(cursor)?;
        }
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        let state = self.lock_state()?;
        let mut owned = BTreeMap::new();
        for (generation_id, record) in state
            .published
            .iter()
            .filter(|(generation_id, record)| {
                after.is_none_or(|cursor| generation_id.as_str() > cursor)
                    && record.signed_manifest.manifest.owner_principal_sha256 == owner.sha256()
            })
            .take(limit.saturating_add(1))
        {
            owned.insert(generation_id.clone(), owner_publication_record(record)?);
        }
        for (generation_id, tombstone) in state
            .tombstones
            .iter()
            .filter(|(generation_id, tombstone)| {
                after.is_none_or(|cursor| generation_id.as_str() > cursor)
                    && tombstone.owner_principal_sha256 == owner.sha256()
            })
            .take(limit.saturating_add(1))
        {
            owned.insert(generation_id.clone(), owner_tombstone_record(tombstone)?);
        }
        let mut records: Vec<_> = owned.into_values().take(limit.saturating_add(1)).collect();
        let has_more = records.len() > limit;
        records.truncate(limit);
        let page = RunGenerationOwnerListPage {
            schema: RUN_GENERATION_OWNER_LIST_SCHEMA.into(),
            next_after: has_more.then(|| {
                records
                    .last()
                    .expect("non-empty bounded owner page")
                    .generation_id
                    .clone()
            }),
            records,
        };
        page.validate()?;
        Ok(page)
    }

    pub fn missing_chunks(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        after: Option<&str>,
        limit: usize,
        now_unix: i64,
    ) -> Result<RunGenerationMissingPage> {
        self.require_enabled()?;
        if limit == 0 || limit > rw_community_protocol::MAX_RUN_GENERATION_MISSING_PAGE {
            return Err(ReplicationError::InvalidGeneration(
                "missing-chunk page limit is invalid".into(),
            ));
        }
        if after.is_some_and(|cursor| !is_sha256(cursor)) {
            return Err(ReplicationError::InvalidGeneration(
                "missing-chunk cursor is invalid".into(),
            ));
        }
        let state = self.lock_state()?;
        self.require_running(&state)?;
        let upload = state
            .uploads
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?;
        require_owner(owner, &upload.manifest)?;
        require_active(upload, now_unix)?;
        let declared = declared_chunks(&upload.manifest)?;
        let mut missing = Vec::new();
        for (sha256, byte_size) in declared {
            if after.is_some_and(|cursor| sha256.as_str() <= cursor) {
                continue;
            }
            if !self.chunk_available(&sha256, byte_size)? {
                missing.push(RunGenerationMissingChunk {
                    object_sha256: sha256,
                    byte_size,
                });
            }
            if missing.len() > limit {
                break;
            }
        }
        let has_more = missing.len() > limit;
        missing.truncate(limit);
        let page = RunGenerationMissingPage {
            schema: RUN_GENERATION_MISSING_PAGE_SCHEMA.into(),
            generation_id: generation_id.into(),
            next_after: has_more.then(|| {
                missing
                    .last()
                    .expect("non-empty bounded page")
                    .object_sha256
                    .clone()
            }),
            chunks: missing,
        };
        page.validate(&self.limits)?;
        Ok(page)
    }

    pub fn upload_chunk(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        object_sha256: &str,
        bytes: &[u8],
        now_unix: i64,
    ) -> Result<()> {
        self.require_enabled()?;
        let mut state = self.lock_state()?;
        self.require_running(&state)?;
        let upload = state
            .uploads
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?;
        require_owner(owner, &upload.manifest)?;
        require_active(upload, now_unix)?;
        let declared = declared_chunks(&upload.manifest)?;
        let expected_size = declared
            .get(object_sha256)
            .copied()
            .ok_or(ReplicationError::UnknownChunk)?;
        let descriptor = upload
            .manifest
            .files
            .iter()
            .flat_map(|file| &file.chunks)
            .find(|chunk| chunk.object_sha256 == object_sha256)
            .cloned()
            .ok_or(ReplicationError::UnknownChunk)?;
        if expected_size != descriptor.byte_size {
            return Err(ReplicationError::CorruptState);
        }
        verify_run_generation_chunk(&descriptor, bytes, &self.limits)?;
        self.charge_upload(&mut state, owner, bytes.len() as u64, now_unix)?;
        if self.chunk_available(object_sha256, expected_size)? {
            return Ok(());
        }
        self.write_chunk_candidate(object_sha256, bytes)?;
        Ok(())
    }

    pub fn finalize(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        request: FinalizeRunGenerationRequest,
        now_unix: i64,
    ) -> Result<FinalizeOutcome> {
        self.require_enabled()?;
        request.validate()?;
        require_generation_id(generation_id)?;
        self.expire_due_publications(now_unix)?;
        let mut state = self.lock_state()?;
        if let Some(record) = state.published.get(generation_id) {
            require_owner(owner, &record.signed_manifest.manifest)?;
            require_publication_time(record, now_unix)?;
            if request.generation_sha256 != record.signed_manifest.manifest.generation_sha256 {
                return Err(ReplicationError::Conflict);
            }
            let mut outcome = finalize_outcome(record)?;
            outcome.was_already_published = true;
            return Ok(outcome);
        }
        if let Some(tombstone) = state.tombstones.get(generation_id) {
            if tombstone.owner_principal_sha256 != owner.sha256() {
                return Err(ReplicationError::WrongOwner);
            }
            return Err(ReplicationError::Conflict);
        }
        self.require_running(&state)?;
        let upload = state
            .uploads
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?;
        require_owner(owner, &upload.manifest)?;
        require_active(upload, now_unix)?;
        if request.generation_sha256 != upload.manifest.generation_sha256 {
            return Err(ReplicationError::Conflict);
        }
        for (sha256, size) in declared_chunks(&upload.manifest)? {
            if !self.chunk_available(&sha256, size)? {
                return Err(ReplicationError::MissingChunk);
            }
        }

        let upload = upload.clone();
        let signed_manifest = sign_run_generation(
            upload.manifest.clone(),
            self.signing_key_id.clone(),
            &self.signing_key,
            &self.limits,
        )?;
        self.persist_signed_manifest(&signed_manifest)?;
        let mut transaction = self.install_candidate(&upload.manifest, now_unix)?;
        let published = PublishedRunGeneration {
            schema: PUBLISHED_RUN_GENERATION_SCHEMA.into(),
            generation_id: upload.manifest.generation_id.clone(),
            generation_sha256: upload.manifest.generation_sha256.clone(),
            source_snapshot_id: upload.manifest.source_snapshot_id.clone(),
            local_snapshot_id: transaction.local_snapshot_id.clone(),
            grid_hash: upload.manifest.grid_hash.clone(),
            model: upload.manifest.model.clone(),
            run: upload.manifest.run.clone(),
            published_unix: now_unix,
        };
        published.validate()?;
        let published_record = PublishedRecord {
            signed_manifest: signed_manifest.clone(),
            local_snapshot_id: published.local_snapshot_id.clone(),
            published_unix: now_unix,
        };
        let previous_upload = state.uploads.remove(generation_id);
        let previous_published = state
            .published
            .insert(generation_id.to_string(), published_record);
        if let Err(error) = self.persist(&state) {
            if let Some(previous_upload) = previous_upload {
                state
                    .uploads
                    .insert(generation_id.to_string(), previous_upload);
            }
            match previous_published {
                Some(previous) => {
                    state.published.insert(generation_id.to_string(), previous);
                }
                None => {
                    state.published.remove(generation_id);
                }
            }
            transaction.rollback()?;
            return Err(error);
        }
        transaction.commit()?;
        Ok(FinalizeOutcome {
            published,
            signed_manifest,
            was_already_published: false,
        })
    }

    pub fn revoke(
        &self,
        owner: &AuthenticatedOwner,
        generation_id: &str,
        request: RevokeRunGenerationRequest,
        now_unix: i64,
    ) -> Result<RunGenerationTombstone> {
        self.require_enabled()?;
        request.validate()?;
        self.expire_due_publications(now_unix)?;
        let mut state = self.lock_state()?;
        let record = state
            .published
            .get(generation_id)
            .ok_or(ReplicationError::NotFound)?
            .clone();
        let manifest = &record.signed_manifest.manifest;
        require_owner(owner, manifest)?;
        require_publication_time(&record, now_unix)?;
        if request.generation_sha256 != manifest.generation_sha256 {
            return Err(ReplicationError::Conflict);
        }
        let tombstone = RunGenerationTombstone {
            schema: RUN_GENERATION_TOMBSTONE_SCHEMA.into(),
            generation_id: generation_id.into(),
            generation_sha256: manifest.generation_sha256.clone(),
            owner_principal_sha256: manifest.owner_principal_sha256.clone(),
            revoked_unix: now_unix,
            rights_withdrawn: true,
            reason: request.reason,
        };
        tombstone.validate()?;

        let previous = state.published.remove(generation_id);
        let old_tombstone = state
            .tombstones
            .insert(generation_id.to_string(), tombstone.clone());
        let old_retirement = state
            .retirements
            .insert(generation_id.to_string(), record.clone());
        if let Err(error) = self.persist(&state) {
            if let Some(previous) = previous {
                state.published.insert(generation_id.to_string(), previous);
            }
            match old_tombstone {
                Some(old) => {
                    state.tombstones.insert(generation_id.to_string(), old);
                }
                None => {
                    state.tombstones.remove(generation_id);
                }
            }
            match old_retirement {
                Some(old) => {
                    state.retirements.insert(generation_id.to_string(), old);
                }
                None => {
                    state.retirements.remove(generation_id);
                }
            }
            return Err(error);
        }
        drop(state);
        // Rights end at the durable tombstone. Physical retirement happens
        // afterward, so a filesystem cleanup failure cannot restore rights.
        let _ = self.drain_retirements(1);
        let _ = self.garbage_collect(now_unix);
        Ok(tombstone)
    }

    pub fn set_kill_switch(&self, engaged: bool) -> Result<()> {
        self.require_enabled()?;
        let mut state = self.lock_state()?;
        let prior = state.kill_switch;
        state.kill_switch = engaged;
        if let Err(error) = self.persist(&state) {
            state.kill_switch = prior;
            return Err(error);
        }
        Ok(())
    }

    pub fn published(&self, generation_id: &str) -> Result<Option<FinalizeOutcome>> {
        self.published_at(generation_id, Utc::now().timestamp())
    }

    /// Time-explicit publication lookup used by deterministic callers and
    /// tests. `retain_until_unix` is an exclusive authorization expiry, not a
    /// minimum-custody promise. Crossing it durably tombstones the publication
    /// before this method can return it.
    pub fn published_at(
        &self,
        generation_id: &str,
        now_unix: i64,
    ) -> Result<Option<FinalizeOutcome>> {
        self.require_enabled()?;
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        let state = self.lock_state()?;
        self.require_running(&state)?;
        let Some(record) = state.published.get(generation_id) else {
            return Ok(None);
        };
        require_publication_time(record, now_unix)?;
        Ok(Some({
            let manifest = &record.signed_manifest.manifest;
            FinalizeOutcome {
                published: PublishedRunGeneration {
                    schema: PUBLISHED_RUN_GENERATION_SCHEMA.into(),
                    generation_id: manifest.generation_id.clone(),
                    generation_sha256: manifest.generation_sha256.clone(),
                    source_snapshot_id: manifest.source_snapshot_id.clone(),
                    local_snapshot_id: record.local_snapshot_id.clone(),
                    grid_hash: manifest.grid_hash.clone(),
                    model: manifest.model.clone(),
                    run: manifest.run.clone(),
                    published_unix: record.published_unix,
                },
                signed_manifest: record.signed_manifest.clone(),
                was_already_published: true,
            }
        }))
    }

    /// Return only currently authorized, local publication identities. Every
    /// item is re-opened through `authorize_query`; an orphan directory or a
    /// durable record whose bytes changed therefore fails closed.
    pub fn authorized_publications(&self) -> Result<Vec<PublishedRunGeneration>> {
        self.authorized_publications_at(Utc::now().timestamp())
    }

    pub fn authorized_publications_at(&self, now_unix: i64) -> Result<Vec<PublishedRunGeneration>> {
        self.require_enabled()?;
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        let identities = {
            let state = self.lock_state()?;
            self.require_running(&state)?;
            state
                .published
                .values()
                .map(|record| {
                    let manifest = &record.signed_manifest.manifest;
                    (manifest.model.clone(), manifest.run.clone())
                })
                .collect::<BTreeSet<_>>()
        };
        identities
            .into_iter()
            .map(|(model, run)| self.authorize_query_current(&model, &run, now_unix))
            .collect()
    }

    pub fn service_status(&self) -> Result<ReplicationServiceStatus> {
        if self.enabled {
            self.expire_due_publications(Utc::now().timestamp())?;
            let _ = self.drain_retirements(1);
        }
        let state = self.lock_state()?;
        let reserved_bytes = state
            .uploads
            .values()
            .try_fold(0_u64, |total, record| {
                total.checked_add(record.manifest.total_bytes)
            })
            .ok_or(ReplicationError::CorruptState)?;
        let published_bytes = state
            .published
            .values()
            .try_fold(0_u64, |total, record| {
                total.checked_add(record.signed_manifest.manifest.total_bytes)
            })
            .ok_or(ReplicationError::CorruptState)?;
        let pending_retirement_bytes = state
            .retirements
            .values()
            .try_fold(0_u64, |total, record| {
                total.checked_add(record.signed_manifest.manifest.total_bytes)
            })
            .ok_or(ReplicationError::CorruptState)?;
        Ok(ReplicationServiceStatus {
            enabled: self.enabled,
            kill_switch: state.kill_switch,
            active_uploads: state.uploads.len(),
            published_generations: state.published.len(),
            tombstones: state.tombstones.len(),
            pending_retirements: state.retirements.len(),
            reserved_bytes,
            published_bytes,
            pending_retirement_bytes,
            monthly_accepted_upload_bytes: state.billing.total_upload_bytes,
        })
    }

    /// Authoritative catalog/query gate. A directory that was renamed into
    /// place but whose durable publication record was not committed is not
    /// authorized. Callers must not expose replicated runs through a raw
    /// `StoreCatalog` scan without this check.
    pub fn authorize_query(&self, model: &str, run: &str) -> Result<PublishedRunGeneration> {
        self.authorize_query_at(model, run, Utc::now().timestamp())
    }

    pub fn authorize_query_at(
        &self,
        model: &str,
        run: &str,
        now_unix: i64,
    ) -> Result<PublishedRunGeneration> {
        self.require_enabled()?;
        self.expire_due_publications(now_unix)?;
        let _ = self.drain_retirements(1);
        self.authorize_query_current(model, run, now_unix)
    }

    fn authorize_query_current(
        &self,
        model: &str,
        run: &str,
        now_unix: i64,
    ) -> Result<PublishedRunGeneration> {
        let state = self.lock_state()?;
        self.require_running(&state)?;
        let record = state
            .published
            .values()
            .find(|record| {
                record.signed_manifest.manifest.model == model
                    && record.signed_manifest.manifest.run == run
            })
            .ok_or(ReplicationError::NotFound)?;
        require_publication_time(record, now_unix)?;
        let manifest = &record.signed_manifest.manifest;
        let snapshot = RunSnapshot::open(&self.store_root, model, run)?;
        if snapshot.descriptor().snapshot_id != record.local_snapshot_id
            || snapshot.descriptor().grid_hash != manifest.grid_hash
        {
            return Err(ReplicationError::Conflict);
        }
        let published = PublishedRunGeneration {
            schema: PUBLISHED_RUN_GENERATION_SCHEMA.into(),
            generation_id: manifest.generation_id.clone(),
            generation_sha256: manifest.generation_sha256.clone(),
            source_snapshot_id: manifest.source_snapshot_id.clone(),
            local_snapshot_id: record.local_snapshot_id.clone(),
            grid_hash: manifest.grid_hash.clone(),
            model: manifest.model.clone(),
            run: manifest.run.clone(),
            published_unix: record.published_unix,
        };
        published.validate()?;
        Ok(published)
    }

    pub fn garbage_collect(&self, now_unix: i64) -> Result<GarbageCollectionReport> {
        let expired_publications = self.expire_due_publications(now_unix)?;
        let retired_generations = self.drain_retirements(self.policy.max_gc_deletions)?;
        let mut state = self.lock_state()?;
        let expired: Vec<_> = state
            .uploads
            .iter()
            .filter(|(_, upload)| now_unix >= upload.expires_unix)
            .map(|(generation_id, _)| generation_id.clone())
            .collect();
        for generation_id in &expired {
            state.uploads.remove(generation_id);
        }
        if !expired.is_empty() {
            self.persist(&state)?;
        }
        let referenced = referenced_chunks(&state)?;
        let published_manifests: BTreeSet<_> = state.published.keys().cloned().collect();

        let mut report = GarbageCollectionReport {
            expired_uploads: expired.len(),
            expired_publications,
            retired_generations,
            pending_retirements: state.retirements.len(),
            ..GarbageCollectionReport::default()
        };
        let mut inspected = 0_usize;
        // A generation retirement is one bounded immutable-generation cleanup
        // operation for this pass; account it against the same deletion cap as
        // individual orphan objects and manifests.
        let mut deleted = retired_generations;
        for prefix in read_real_directories_bounded(
            &self.chunks_root,
            &mut inspected,
            self.policy.max_gc_entries,
        )? {
            for path in
                read_real_files_bounded(&prefix, &mut inspected, self.policy.max_gc_entries)?
            {
                let name = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("");
                if name.starts_with('.') && name.contains(".candidate-") {
                    require_gc_deletion_budget(deleted, self.policy.max_gc_deletions)?;
                    fs::remove_file(&path)?;
                    deleted += 1;
                    report.stale_candidates += 1;
                } else if is_sha256(name) && !referenced.contains(name) {
                    require_gc_deletion_budget(deleted, self.policy.max_gc_deletions)?;
                    fs::remove_file(&path)?;
                    deleted += 1;
                    report.orphan_chunks += 1;
                }
            }
        }
        for path in read_real_files_bounded(
            &self.manifests_root,
            &mut inspected,
            self.policy.max_gc_entries,
        )? {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            let Some(generation_id) = name.strip_suffix(".json") else {
                continue;
            };
            if !published_manifests.contains(generation_id) {
                require_gc_deletion_budget(deleted, self.policy.max_gc_deletions)?;
                fs::remove_file(path)?;
                deleted += 1;
                report.orphan_manifests += 1;
            }
        }
        drop(state);
        Ok(report)
    }

    /// Atomically converts every due publication in one bounded state batch to
    /// a terminal tombstone plus durable physical-retirement work item. The
    /// engine never derives authorization from wall-clock history, so a later
    /// rollback cannot move one of these entries back into `published`.
    fn expire_due_publications(&self, now_unix: i64) -> Result<usize> {
        if now_unix < 0 {
            return Err(ReplicationError::Expired);
        }
        let mut state = self.lock_state()?;
        let due: Vec<_> = state
            .published
            .iter()
            .filter(|(_, record)| now_unix >= record.signed_manifest.manifest.retain_until_unix)
            .take(self.policy.max_total_generations)
            .map(|(generation_id, record)| (generation_id.clone(), record.clone()))
            .collect();
        if due.is_empty() {
            return Ok(0);
        }
        let previous = state.clone();
        for (generation_id, record) in &due {
            let manifest = &record.signed_manifest.manifest;
            state.published.remove(generation_id);
            state
                .retirements
                .insert(generation_id.clone(), record.clone());
            state.tombstones.insert(
                generation_id.clone(),
                RunGenerationTombstone {
                    schema: RUN_GENERATION_TOMBSTONE_SCHEMA.into(),
                    generation_id: generation_id.clone(),
                    generation_sha256: manifest.generation_sha256.clone(),
                    owner_principal_sha256: manifest.owner_principal_sha256.clone(),
                    revoked_unix: manifest.retain_until_unix,
                    rights_withdrawn: true,
                    reason: "Signed publication retention expired.".into(),
                },
            );
        }
        if let Err(error) = self.persist(&state) {
            *state = previous;
            return Err(error);
        }
        Ok(due.len())
    }

    /// Retires at most `limit` authenticated work items. A cleanup failure
    /// leaves its item durable for restart/retry; publication rights have
    /// already ended and are never restored.
    fn drain_retirements(&self, limit: usize) -> Result<usize> {
        let work: Vec<_> = {
            let state = self.lock_state()?;
            state
                .retirements
                .iter()
                .take(limit)
                .map(|(generation_id, record)| (generation_id.clone(), record.clone()))
                .collect()
        };
        let mut retired = 0;
        for (generation_id, record) in work {
            let manifest = &record.signed_manifest.manifest;
            let mut state = self.lock_state()?;
            if state.retirements.get(&generation_id) != Some(&record) {
                continue;
            }
            // State validation and begin admission make this impossible for
            // newly written state. Keep the state lock across the filesystem
            // retirement as defense in depth for a legacy/corrupt in-memory
            // duplicate: never delete a namespace another upload/publication/
            // retirement still references.
            if namespace_is_occupied_by_other_generation(&state, manifest) {
                return Err(ReplicationError::CorruptState);
            }
            if self
                .retire_if_current(manifest, &record.local_snapshot_id)
                .is_err()
            {
                continue;
            }
            state.retirements.remove(&generation_id);
            if let Err(error) = self.persist(&state) {
                state.retirements.insert(generation_id, record);
                return Err(error);
            }
            retired += 1;
        }
        Ok(retired)
    }

    fn require_enabled(&self) -> Result<()> {
        if self.enabled {
            Ok(())
        } else {
            Err(ReplicationError::Disabled)
        }
    }

    fn require_running(&self, state: &PersistentState) -> Result<()> {
        if state.kill_switch {
            Err(ReplicationError::KillSwitch)
        } else {
            Ok(())
        }
    }

    fn lock_state(&self) -> Result<std::sync::MutexGuard<'_, PersistentState>> {
        self.state
            .lock()
            .map_err(|_| ReplicationError::CorruptState)
    }

    fn check_quotas(
        &self,
        state: &PersistentState,
        owner: &AuthenticatedOwner,
        requested_bytes: u64,
    ) -> Result<()> {
        let owner_uploads: Vec<_> = state
            .uploads
            .values()
            .filter(|record| record.manifest.owner_principal_sha256 == owner.sha256())
            .collect();
        let owner_published: Vec<_> = state
            .published
            .values()
            .filter(|record| {
                record.signed_manifest.manifest.owner_principal_sha256 == owner.sha256()
            })
            .collect();
        let owner_retirements: Vec<_> = state
            .retirements
            .values()
            .filter(|record| {
                record.signed_manifest.manifest.owner_principal_sha256 == owner.sha256()
            })
            .collect();
        if owner_uploads.len() >= self.policy.max_owner_concurrent_uploads
            || state.uploads.len() >= self.policy.max_total_concurrent_uploads
        {
            return Err(ReplicationError::Quota("concurrent uploads"));
        }
        if owner_uploads
            .len()
            .saturating_add(owner_published.len())
            .saturating_add(owner_retirements.len())
            >= self.policy.max_owner_generations
        {
            return Err(ReplicationError::Quota("generation count"));
        }
        if state
            .uploads
            .len()
            .saturating_add(state.published.len())
            .saturating_add(state.retirements.len())
            >= self.policy.max_total_generations
        {
            return Err(ReplicationError::Quota("total generation count"));
        }
        let owner_bytes = owner_uploads
            .iter()
            .map(|record| record.manifest.total_bytes)
            .chain(
                owner_published
                    .iter()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .chain(
                owner_retirements
                    .iter()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .try_fold(0_u64, u64::checked_add)
            .ok_or(ReplicationError::Quota("owner storage"))?;
        let total_bytes = state
            .uploads
            .values()
            .map(|record| record.manifest.total_bytes)
            .chain(
                state
                    .published
                    .values()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .chain(
                state
                    .retirements
                    .values()
                    .map(|record| record.signed_manifest.manifest.total_bytes),
            )
            .try_fold(0_u64, u64::checked_add)
            .ok_or(ReplicationError::Quota("total storage"))?;
        if owner_bytes
            .checked_add(requested_bytes)
            .is_none_or(|value| value > self.policy.max_owner_storage_bytes)
        {
            return Err(ReplicationError::Quota("owner storage"));
        }
        if total_bytes
            .checked_add(requested_bytes)
            .is_none_or(|value| value > self.policy.max_total_storage_bytes)
        {
            return Err(ReplicationError::Quota("total storage"));
        }
        Ok(())
    }

    fn charge_upload(
        &self,
        state: &mut PersistentState,
        owner: &AuthenticatedOwner,
        byte_size: u64,
        now_unix: i64,
    ) -> Result<()> {
        let month = utc_month(now_unix)?;
        let previous = state.billing.clone();
        if state.billing.utc_month != month {
            state.billing = BillingLedger {
                utc_month: month,
                ..BillingLedger::default()
            };
        }
        let owner_total = state
            .billing
            .owner_upload_bytes
            .get(owner.sha256())
            .copied()
            .unwrap_or(0)
            .checked_add(byte_size)
            .ok_or(ReplicationError::Quota("monthly owner upload"))?;
        let global_total = state
            .billing
            .total_upload_bytes
            .checked_add(byte_size)
            .ok_or(ReplicationError::Quota("monthly total upload"))?;
        if owner_total > self.policy.max_owner_monthly_upload_bytes {
            state.billing = previous;
            return Err(ReplicationError::Quota("monthly owner upload"));
        }
        if global_total > self.policy.max_total_monthly_upload_bytes {
            state.billing = previous;
            return Err(ReplicationError::Quota("monthly total upload"));
        }
        state
            .billing
            .owner_upload_bytes
            .insert(owner.sha256().into(), owner_total);
        state.billing.total_upload_bytes = global_total;
        if let Err(error) = self.persist(state) {
            state.billing = previous;
            return Err(error);
        }
        Ok(())
    }

    fn status_for(&self, upload: &UploadRecord) -> Result<RunGenerationUploadStatus> {
        let declared = declared_chunks(&upload.manifest)?;
        let mut missing = 0_u32;
        for (sha256, size) in &declared {
            if !self.chunk_available(sha256, *size)? {
                missing = missing.saturating_add(1);
            }
        }
        let status = RunGenerationUploadStatus {
            schema: RUN_GENERATION_UPLOAD_STATUS_SCHEMA.into(),
            generation_id: upload.manifest.generation_id.clone(),
            generation_sha256: upload.manifest.generation_sha256.clone(),
            total_chunks: u32::try_from(declared.len()).map_err(|_| {
                ReplicationError::InvalidGeneration("too many unique chunks".into())
            })?,
            missing_chunks: missing,
            upload_expires_unix: upload.expires_unix,
        };
        status.validate()?;
        Ok(status)
    }

    fn chunk_path(&self, sha256: &str) -> Result<PathBuf> {
        if !is_sha256(sha256) {
            return Err(ReplicationError::UnknownChunk);
        }
        Ok(self.chunks_root.join(&sha256[..2]).join(sha256))
    }

    fn chunk_available(&self, sha256: &str, expected_size: u64) -> Result<bool> {
        let path = self.chunk_path(sha256)?;
        if !path.exists() {
            return Ok(false);
        }
        self.require_existing_chunk_path(&path)?;
        let metadata = fs::metadata(&path)?;
        if metadata.len() != expected_size || hash_file(&path, expected_size)? != sha256 {
            return Err(ReplicationError::CorruptState);
        }
        Ok(true)
    }

    fn require_existing_chunk_path(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or(ReplicationError::CorruptState)?;
        let metadata = fs::symlink_metadata(parent)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(ReplicationError::CorruptState);
        }
        let canonical_parent = fs::canonicalize(parent)?;
        require_contained(&self.chunks_root, &canonical_parent)?;
        require_regular_file(path)
    }

    fn write_chunk_candidate(&self, sha256: &str, bytes: &[u8]) -> Result<()> {
        let target = self.chunk_path(sha256)?;
        let parent = target.parent().expect("content object has prefix parent");
        create_real_directory(parent)?;
        let parent = fs::canonicalize(parent)?;
        require_contained(&self.chunks_root, &parent)?;
        let candidate = unique_child(&parent, &format!(".{sha256}.candidate"));
        let result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            if target.exists() {
                require_regular_file(&target)?;
                if fs::metadata(&target)?.len() != bytes.len() as u64
                    || hash_file(&target, bytes.len() as u64)? != sha256
                {
                    return Err(ReplicationError::CorruptState);
                }
                fs::remove_file(&candidate)?;
                return Ok(());
            }
            fs::rename(&candidate, &target)?;
            sync_directory(&parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&candidate);
        }
        result
    }

    fn persist(&self, state: &PersistentState) -> Result<()> {
        #[cfg(test)]
        if self.fail_next_persist.swap(false, Ordering::SeqCst) {
            return Err(ReplicationError::Io(std::io::Error::other(
                "injected durable-state failure",
            )));
        }
        validate_state(state, &self.limits, &self.signing_key_id, &self.signing_key)?;
        let state_bytes = serde_json::to_vec(state)?;
        let signature = self
            .signing_key
            .sign(&state_preimage(&self.signing_key_id, &state_bytes));
        let envelope = StateEnvelope {
            schema: STATE_ENVELOPE_SCHEMA.into(),
            signing_key_id: self.signing_key_id.clone(),
            state: state.clone(),
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        };
        let bytes = serde_json::to_vec(&envelope)?;
        if bytes.is_empty() || bytes.len() as u64 > self.policy.max_state_bytes {
            return Err(ReplicationError::Quota("state bytes"));
        }
        atomic_write_bytes(&self.state_path, &bytes)?;
        Ok(())
    }

    fn persist_signed_manifest(&self, signed: &SignedRunGenerationManifest) -> Result<()> {
        let generation_id = &signed.manifest.generation_id;
        validate_store_component("generation manifest id", generation_id)?;
        let path = self.manifests_root.join(format!("{generation_id}.json"));
        let bytes = serde_json::to_vec(signed)?;
        if bytes.is_empty() || bytes.len() > self.limits.max_manifest_bytes {
            return Err(ReplicationError::Quota("signed manifest bytes"));
        }
        if path.exists() {
            require_regular_file(&path)?;
            let current = fs::read(&path)?;
            if current != bytes {
                return Err(ReplicationError::Conflict);
            }
            return Ok(());
        }
        atomic_write_bytes(&path, &bytes)?;
        Ok(())
    }

    fn install_candidate(
        &self,
        manifest: &RunGenerationReplicationManifest,
        now_unix: i64,
    ) -> Result<InstallTransaction> {
        let stage_base = unique_child(&self.staging_root, &manifest.generation_id);
        let stage_store_root = stage_base.join("store");
        let stage_run = stage_store_root.join(&manifest.model).join(&manifest.run);
        create_real_directory(&stage_run)?;
        let stage_base = fs::canonicalize(&stage_base)?;
        require_contained(&self.staging_root, &stage_base)?;
        let stage_store_root = stage_base.join("store");
        let stage_run = stage_store_root.join(&manifest.model).join(&manifest.run);
        let build_result = (|| -> Result<()> {
            for file in &manifest.files {
                self.reconstruct_file(&stage_run, file)?;
            }
            validate_reconstructed(&stage_store_root, &stage_run, manifest)?;
            Ok(())
        })();
        if let Err(error) = build_result {
            let _ = fs::remove_dir_all(&stage_base);
            return Err(error);
        }

        let model_dir = self.store_root.join(&manifest.model);
        create_real_directory(&model_dir)?;
        let model_dir = fs::canonicalize(model_dir)?;
        require_contained(&self.store_root, &model_dir)?;
        let final_run = model_dir.join(&manifest.run);
        if final_run.exists() {
            require_real_directory(&final_run)?;
            require_contained(&model_dir, &fs::canonicalize(&final_run)?)?;
        }
        let target_lock = acquire_target_lock(&model_dir, &manifest.model, &manifest.run)?;
        let run_lock = if final_run.exists() {
            Some(RunLock::try_acquire(&final_run)?.ok_or(ReplicationError::Busy)?)
        } else {
            None
        };
        // Cross-platform rw-query currently resolves a run by its concrete
        // directory name rather than through a versioned alias. Replacing a
        // non-empty directory cannot provide an old-or-new namespace view on
        // every supported Windows filesystem. Adopt an already-installed
        // *exact* inventory (crash recovery/idempotence), but fail closed on
        // any different valid or invalid destination instead of creating a
        // visibility gap by moving it away first.
        if final_run.exists() {
            // A scheduler-owned directory remains exclusively controlled by
            // scheduler retention. Replication must never adopt it into its
            // durable publication authority, even when the weather bytes are
            // otherwise exact, because the scheduler could later prune it.
            if fs::symlink_metadata(final_run.join(SCHEDULER_OWNER_MARKER)).is_ok() {
                let _ = fs::remove_dir_all(&stage_base);
                return Err(ReplicationError::Conflict);
            }
            let existing = validate_reconstructed(&self.store_root, &final_run, manifest)
                .map_err(|_| ReplicationError::Conflict)?;
            let local_snapshot_id = existing.descriptor().snapshot_id.clone();
            drop(existing);
            let _ = fs::remove_dir_all(&stage_base);
            return Ok(InstallTransaction {
                final_run,
                stage_base,
                rollback_target: None,
                local_snapshot_id,
                _target_lock: target_lock,
                _run_lock: run_lock,
                owns_final: false,
                finished: false,
                now_unix,
            });
        }
        if let Err(error) = fs::rename(&stage_run, &final_run) {
            let _ = fs::remove_dir_all(&stage_base);
            return Err(error.into());
        }
        let local_snapshot = match validate_reconstructed(&self.store_root, &final_run, manifest) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let failed = unique_child(&model_dir, ".rw-generation-failed");
                let _ = fs::rename(&final_run, &failed);
                let _ = fs::remove_dir_all(&failed);
                let _ = fs::remove_dir_all(&stage_base);
                return Err(error);
            }
        };
        let local_snapshot_id = local_snapshot.descriptor().snapshot_id.clone();
        drop(local_snapshot);
        let transaction = InstallTransaction {
            final_run,
            stage_base,
            rollback_target: Some(stage_run),
            local_snapshot_id,
            _target_lock: target_lock,
            _run_lock: run_lock,
            owns_final: true,
            finished: false,
            now_unix,
        };
        Ok(transaction)
    }

    fn reconstruct_file(&self, stage_run: &Path, file: &RunGenerationFile) -> Result<()> {
        validate_store_component("replicated filename", &file.file_name)?;
        let output = stage_run.join(&file.file_name);
        let mut writer = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&output)?;
        let mut digest = Sha256::new();
        let mut written = 0_u64;
        for chunk in &file.chunks {
            let path = self.chunk_path(&chunk.object_sha256)?;
            self.require_existing_chunk_path(&path)?;
            if fs::metadata(&path)?.len() != chunk.byte_size {
                return Err(ReplicationError::CorruptState);
            }
            let mut reader = File::open(&path)?;
            let mut remaining = chunk.byte_size;
            let mut buffer = [0_u8; 1024 * 1024];
            while remaining > 0 {
                let take = usize::try_from(remaining.min(buffer.len() as u64))
                    .expect("bounded buffer length");
                reader.read_exact(&mut buffer[..take])?;
                writer.write_all(&buffer[..take])?;
                digest.update(&buffer[..take]);
                remaining -= take as u64;
                written = written.checked_add(take as u64).ok_or_else(|| {
                    ReplicationError::InvalidGeneration("file size overflow".into())
                })?;
            }
            let mut trailing = [0_u8; 1];
            if reader.read(&mut trailing)? != 0 {
                return Err(ReplicationError::CorruptState);
            }
        }
        writer.sync_all()?;
        if written != file.byte_size || hex_digest(digest.finalize()) != file.file_sha256 {
            return Err(ReplicationError::InvalidGeneration(
                "reconstructed file hash or size does not match inventory".into(),
            ));
        }
        Ok(())
    }

    fn retire_if_current(
        &self,
        manifest: &RunGenerationReplicationManifest,
        expected_local_snapshot_id: &str,
    ) -> Result<()> {
        self.retire_if_current_with_hook(manifest, expected_local_snapshot_id, |_| Ok(()))
    }

    fn retire_if_current_with_hook<F>(
        &self,
        manifest: &RunGenerationReplicationManifest,
        expected_local_snapshot_id: &str,
        after_optimistic_check: F,
    ) -> Result<()>
    where
        F: FnOnce(&Path) -> Result<()>,
    {
        let final_run = self.store_root.join(&manifest.model).join(&manifest.run);
        if !final_run.exists() {
            return Ok(());
        }
        let snapshot = match RunSnapshot::open(&self.store_root, &manifest.model, &manifest.run) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(()),
        };
        if snapshot.descriptor().snapshot_id != expected_local_snapshot_id {
            return Ok(());
        }
        drop(snapshot);
        after_optimistic_check(&final_run)?;
        let model_dir = final_run
            .parent()
            .expect("run has model parent")
            .to_path_buf();
        let _target_lock = acquire_target_lock(&model_dir, &manifest.model, &manifest.run)?;
        let run_lock = RunLock::try_acquire(&final_run)?.ok_or(ReplicationError::Busy)?;
        // The optimistic check above avoids taking locks for a run that is
        // already unrelated, but it is not authority to delete. A local or
        // scheduler publisher may have replaced the namespace before both
        // deletion locks were acquired. Re-open and deep-validate the exact
        // inventory while the locks are held, and refuse scheduler ownership,
        // immediately before removing run.json.
        if fs::symlink_metadata(final_run.join(SCHEDULER_OWNER_MARKER)).is_ok() {
            return Ok(());
        }
        let locked_snapshot = match validate_reconstructed(&self.store_root, &final_run, manifest) {
            Ok(snapshot) => snapshot,
            Err(_) => return Ok(()),
        };
        if locked_snapshot.descriptor().snapshot_id != expected_local_snapshot_id {
            return Ok(());
        }
        drop(locked_snapshot);
        // Remove run.json first. Even a raw StoreCatalog stops recognizing
        // the generation before the remaining immutable bytes are reclaimed.
        let run_manifest = final_run.join("run.json");
        if run_manifest.exists() {
            fs::remove_file(run_manifest)?;
        }
        for file in &manifest.files {
            if file.file_name != "run.json" {
                let path = final_run.join(&file.file_name);
                if path.exists() {
                    fs::remove_file(path)?;
                }
            }
        }
        drop(run_lock);
        let _ = fs::remove_dir(&final_run);
        Ok(())
    }
}

struct InstallTransaction {
    final_run: PathBuf,
    stage_base: PathBuf,
    rollback_target: Option<PathBuf>,
    local_snapshot_id: String,
    _target_lock: File,
    _run_lock: Option<RunLock>,
    owns_final: bool,
    finished: bool,
    #[allow(dead_code)]
    now_unix: i64,
}

impl InstallTransaction {
    fn rollback(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        if self.owns_final && self.final_run.exists() {
            let rollback_target = self
                .rollback_target
                .as_ref()
                .expect("owned publication has rollback target");
            if let Some(parent) = rollback_target.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::rename(&self.final_run, rollback_target)?;
        }
        self.finished = true;
        let _ = fs::remove_dir_all(&self.stage_base);
        Ok(())
    }

    fn commit(&mut self) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        let _ = fs::remove_dir_all(&self.stage_base);
        Ok(())
    }
}

impl Drop for InstallTransaction {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.rollback();
        }
    }
}

fn validate_engine_manifest(manifest: &RunGenerationReplicationManifest) -> Result<()> {
    validate_store_component("replicated model", &manifest.model)?;
    validate_store_component("replicated run", &manifest.run)?;
    for file in &manifest.files {
        validate_store_component("replicated filename", &file.file_name)?;
    }
    Ok(())
}

fn validate_reconstructed(
    store_root: &Path,
    run_dir: &Path,
    manifest: &RunGenerationReplicationManifest,
) -> Result<RunSnapshot> {
    for file in &manifest.files {
        let path = run_dir.join(&file.file_name);
        require_regular_file(&path)?;
        if fs::metadata(&path)?.len() != file.byte_size
            || hash_file(&path, file.byte_size)? != file.file_sha256
        {
            return Err(ReplicationError::InvalidGeneration(format!(
                "{} does not match its exact signed hash and size",
                file.file_name
            )));
        }
    }
    let report = validate_run_dir(run_dir, ValidateDepth::Deep)?;
    if !report.is_ok() {
        return Err(ReplicationError::InvalidGeneration(format!(
            "deep rw-store validation reported {} error(s)",
            report.errors.len()
        )));
    }
    let run_manifest =
        RwsRunManifest::load_for_run(&run_dir.join("run.json"), &manifest.model, &manifest.run)?;
    if run_manifest.grid_hash != manifest.grid_hash {
        return Err(ReplicationError::InvalidGeneration(
            "run grid hash does not match replication inventory".into(),
        ));
    }
    let inventory_hours: BTreeMap<_, _> = manifest
        .files
        .iter()
        .filter_map(|file| match file.kind {
            RunGenerationFileKind::Hour {
                storage_slot,
                valid_unix,
            } => Some((storage_slot, (file.file_name.as_str(), valid_unix))),
            _ => None,
        })
        .collect();
    if inventory_hours.len() != run_manifest.hours.len() {
        return Err(ReplicationError::InvalidGeneration(
            "run hour count does not match replication inventory".into(),
        ));
    }
    for (&slot, entry) in &run_manifest.hours {
        let (file_name, _) = inventory_hours.get(&slot).ok_or_else(|| {
            ReplicationError::InvalidGeneration("run contains an uninventoried hour slot".into())
        })?;
        if entry.file != *file_name {
            return Err(ReplicationError::InvalidGeneration(
                "run hour filename does not match replication inventory".into(),
            ));
        }
    }
    let snapshot = RunSnapshot::open(store_root, &manifest.model, &manifest.run)?;
    if snapshot.descriptor().model != manifest.model
        || snapshot.descriptor().run != manifest.run
        || snapshot.descriptor().grid_hash != manifest.grid_hash
    {
        return Err(ReplicationError::InvalidGeneration(
            "opened snapshot identity does not match replication inventory".into(),
        ));
    }
    if snapshot.time_axis().len() != inventory_hours.len()
        || snapshot.time_axis().iter().any(|time| {
            inventory_hours
                .get(&time.storage_slot)
                .is_none_or(|(_, valid_unix)| *valid_unix != time.valid_unix)
        })
    {
        return Err(ReplicationError::InvalidGeneration(
            "opened snapshot time axis does not match replication inventory".into(),
        ));
    }
    let actual_sources: Vec<_> = snapshot
        .descriptor()
        .source_provenance
        .iter()
        .map(|source| (&source.provider, &source.roles, &source.products))
        .collect();
    let declared_sources: Vec<_> = manifest
        .source_provenance
        .iter()
        .map(|source| (&source.provider, &source.roles, &source.products))
        .collect();
    if actual_sources != declared_sources {
        return Err(ReplicationError::InvalidGeneration(
            "stored source provenance does not match signed replication provenance".into(),
        ));
    }
    Ok(snapshot)
}

fn publication_for(record: &PublishedRecord) -> Result<PublishedRunGeneration> {
    let manifest = &record.signed_manifest.manifest;
    let publication = PublishedRunGeneration {
        schema: PUBLISHED_RUN_GENERATION_SCHEMA.into(),
        generation_id: manifest.generation_id.clone(),
        generation_sha256: manifest.generation_sha256.clone(),
        source_snapshot_id: manifest.source_snapshot_id.clone(),
        local_snapshot_id: record.local_snapshot_id.clone(),
        grid_hash: manifest.grid_hash.clone(),
        model: manifest.model.clone(),
        run: manifest.run.clone(),
        published_unix: record.published_unix,
    };
    publication.validate()?;
    Ok(publication)
}

fn finalize_outcome(record: &PublishedRecord) -> Result<FinalizeOutcome> {
    Ok(FinalizeOutcome {
        published: publication_for(record)?,
        signed_manifest: record.signed_manifest.clone(),
        was_already_published: true,
    })
}

fn owner_publication_record(record: &PublishedRecord) -> Result<RunGenerationOwnerRecord> {
    let publication = publication_for(record)?;
    let owner_record = RunGenerationOwnerRecord {
        schema: RUN_GENERATION_OWNER_RECORD_SCHEMA.into(),
        state: RunGenerationOwnerRecordState::Published,
        generation_id: publication.generation_id.clone(),
        generation_sha256: publication.generation_sha256.clone(),
        publication: Some(publication),
        tombstone: None,
    };
    owner_record.validate()?;
    Ok(owner_record)
}

fn owner_tombstone_record(tombstone: &RunGenerationTombstone) -> Result<RunGenerationOwnerRecord> {
    let owner_record = RunGenerationOwnerRecord {
        schema: RUN_GENERATION_OWNER_RECORD_SCHEMA.into(),
        state: RunGenerationOwnerRecordState::Tombstone,
        generation_id: tombstone.generation_id.clone(),
        generation_sha256: tombstone.generation_sha256.clone(),
        publication: None,
        tombstone: Some(tombstone.clone()),
    };
    owner_record.validate()?;
    Ok(owner_record)
}

fn sum_upload_bytes<'a>(records: impl Iterator<Item = &'a UploadRecord>) -> Result<u64> {
    records
        .map(|record| record.manifest.total_bytes)
        .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
        .ok_or(ReplicationError::CorruptState)
}

fn sum_published_bytes<'a>(records: impl Iterator<Item = &'a PublishedRecord>) -> Result<u64> {
    records
        .map(|record| record.signed_manifest.manifest.total_bytes)
        .try_fold(0_u64, |total, bytes| total.checked_add(bytes))
        .ok_or(ReplicationError::CorruptState)
}

fn usize_as_u64(value: usize) -> Result<u64> {
    u64::try_from(value).map_err(|_| ReplicationError::CorruptState)
}

fn require_generation_id(value: &str) -> Result<()> {
    if !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        Ok(())
    } else {
        Err(ReplicationError::InvalidGeneration(
            "generation id is invalid".into(),
        ))
    }
}

fn require_owner(
    owner: &AuthenticatedOwner,
    manifest: &RunGenerationReplicationManifest,
) -> Result<()> {
    if manifest.owner_principal_sha256 == owner.sha256() {
        Ok(())
    } else {
        Err(ReplicationError::WrongOwner)
    }
}

fn require_active(upload: &UploadRecord, now_unix: i64) -> Result<()> {
    if now_unix < 0
        || now_unix < upload.created_unix.saturating_sub(CLOCK_SKEW_SECONDS)
        || now_unix >= upload.expires_unix
        || now_unix >= upload.manifest.retain_until_unix
    {
        Err(ReplicationError::Expired)
    } else {
        Ok(())
    }
}

fn require_publication_time(record: &PublishedRecord, now_unix: i64) -> Result<()> {
    let manifest = &record.signed_manifest.manifest;
    if now_unix < 0
        || now_unix < manifest.published_unix.saturating_sub(CLOCK_SKEW_SECONDS)
        || now_unix >= manifest.retain_until_unix
    {
        Err(ReplicationError::Expired)
    } else {
        Ok(())
    }
}

fn declared_chunks(manifest: &RunGenerationReplicationManifest) -> Result<BTreeMap<String, u64>> {
    let mut chunks = BTreeMap::new();
    for chunk in manifest.files.iter().flat_map(|file| &file.chunks) {
        if let Some(previous) = chunks.insert(chunk.object_sha256.clone(), chunk.byte_size)
            && previous != chunk.byte_size
        {
            return Err(ReplicationError::InvalidGeneration(
                "one content hash is declared with conflicting sizes".into(),
            ));
        }
    }
    Ok(chunks)
}

fn referenced_chunks(state: &PersistentState) -> Result<BTreeSet<String>> {
    let mut referenced = BTreeSet::new();
    for manifest in state.uploads.values().map(|record| &record.manifest).chain(
        state
            .published
            .values()
            .map(|record| &record.signed_manifest.manifest),
    ) {
        referenced.extend(declared_chunks(manifest)?.into_keys());
    }
    Ok(referenced)
}

fn load_state(
    path: &Path,
    max_bytes: u64,
    expected_key_id: &str,
    signing_key: &SigningKey,
    limits: &RunGenerationLimits,
) -> Result<PersistentState> {
    require_regular_file(path)?;
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    if length == 0 || length > max_bytes {
        return Err(ReplicationError::CorruptState);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(max_bytes + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        return Err(ReplicationError::CorruptState);
    }
    let envelope: StateEnvelope = serde_json::from_slice(&bytes)?;
    if envelope.schema != STATE_ENVELOPE_SCHEMA || envelope.signing_key_id != expected_key_id {
        return Err(ReplicationError::CorruptState);
    }
    let state_bytes = serde_json::to_vec(&envelope.state)?;
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&envelope.signature_base64)
        .map_err(|_| ReplicationError::CorruptState)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ReplicationError::CorruptState)?;
    signing_key
        .verifying_key()
        .verify(&state_preimage(expected_key_id, &state_bytes), &signature)
        .map_err(|_| ReplicationError::CorruptState)?;
    validate_state(&envelope.state, limits, expected_key_id, signing_key)?;
    Ok(envelope.state)
}

fn validate_state(
    state: &PersistentState,
    limits: &RunGenerationLimits,
    signing_key_id: &str,
    signing_key: &SigningKey,
) -> Result<()> {
    if state.schema != STATE_SCHEMA {
        return Err(ReplicationError::CorruptState);
    }
    if utc_month_token_valid(&state.billing.utc_month)
        && state
            .billing
            .owner_upload_bytes
            .keys()
            .all(|owner| is_sha256(owner))
        && state
            .billing
            .owner_upload_bytes
            .values()
            .try_fold(0_u64, |total, bytes| total.checked_add(*bytes))
            == Some(state.billing.total_upload_bytes)
    {
        // Valid durable calendar-month accounting.
    } else {
        return Err(ReplicationError::CorruptState);
    }
    for (generation_id, upload) in &state.uploads {
        upload
            .manifest
            .validate(limits)
            .map_err(|_| ReplicationError::CorruptState)?;
        validate_engine_manifest(&upload.manifest).map_err(|_| ReplicationError::CorruptState)?;
        declared_chunks(&upload.manifest).map_err(|_| ReplicationError::CorruptState)?;
        if generation_id != &upload.manifest.generation_id
            || upload.created_unix < 0
            || upload.expires_unix <= upload.created_unix
            || upload.expires_unix > upload.manifest.retain_until_unix
        {
            return Err(ReplicationError::CorruptState);
        }
    }
    if state.uploads.keys().any(|generation_id| {
        state.published.contains_key(generation_id)
            || state.tombstones.contains_key(generation_id)
            || state.retirements.contains_key(generation_id)
    }) || state.published.keys().any(|generation_id| {
        state.tombstones.contains_key(generation_id)
            || state.retirements.contains_key(generation_id)
    }) {
        return Err(ReplicationError::CorruptState);
    }
    for (generation_id, record) in state.published.iter().chain(&state.retirements) {
        let manifest = &record.signed_manifest.manifest;
        manifest
            .validate(limits)
            .map_err(|_| ReplicationError::CorruptState)?;
        validate_engine_manifest(manifest).map_err(|_| ReplicationError::CorruptState)?;
        if generation_id != &manifest.generation_id
            || !is_sha256(&record.local_snapshot_id)
            || record.published_unix < 0
            || record.published_unix >= manifest.retain_until_unix
            || record.signed_manifest.signature.signing_key_id != signing_key_id
            || record.signed_manifest.signature.algorithm != SignatureAlgorithm::Ed25519
        {
            return Err(ReplicationError::CorruptState);
        }
        let preimage = canonical_run_generation_bytes(manifest, signing_key_id, limits)
            .map_err(|_| ReplicationError::CorruptState)?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&record.signed_manifest.signature.signature_base64)
            .map_err(|_| ReplicationError::CorruptState)?;
        let signature =
            Signature::from_slice(&bytes).map_err(|_| ReplicationError::CorruptState)?;
        signing_key
            .verifying_key()
            .verify(&preimage, &signature)
            .map_err(|_| ReplicationError::CorruptState)?;
    }
    for (generation_id, tombstone) in &state.tombstones {
        tombstone
            .validate()
            .map_err(|_| ReplicationError::CorruptState)?;
        if generation_id != &tombstone.generation_id {
            return Err(ReplicationError::CorruptState);
        }
    }
    for (generation_id, record) in &state.retirements {
        let manifest = &record.signed_manifest.manifest;
        let tombstone = state
            .tombstones
            .get(generation_id)
            .ok_or(ReplicationError::CorruptState)?;
        if tombstone.generation_sha256 != manifest.generation_sha256
            || tombstone.owner_principal_sha256 != manifest.owner_principal_sha256
            || tombstone.revoked_unix > manifest.retain_until_unix
        {
            return Err(ReplicationError::CorruptState);
        }
    }
    validate_unique_live_namespaces(state)?;
    Ok(())
}

fn same_run_namespace(
    left: &RunGenerationReplicationManifest,
    right: &RunGenerationReplicationManifest,
) -> bool {
    left.model == right.model && left.run == right.run
}

/// Whether a distinct generation id currently owns the candidate's concrete
/// rw-store namespace. Retirements count as owners until their authenticated
/// cleanup completes, preventing a replacement upload from racing deletion.
fn namespace_is_occupied_by_other_generation(
    state: &PersistentState,
    candidate: &RunGenerationReplicationManifest,
) -> bool {
    state.uploads.iter().any(|(generation_id, record)| {
        generation_id != &candidate.generation_id && same_run_namespace(&record.manifest, candidate)
    }) || state.published.iter().any(|(generation_id, record)| {
        generation_id != &candidate.generation_id
            && same_run_namespace(&record.signed_manifest.manifest, candidate)
    }) || state.retirements.iter().any(|(generation_id, record)| {
        generation_id != &candidate.generation_id
            && same_run_namespace(&record.signed_manifest.manifest, candidate)
    })
}

fn validate_unique_live_namespaces(state: &PersistentState) -> Result<()> {
    let mut namespaces = BTreeSet::<(&str, &str)>::new();
    for manifest in state
        .uploads
        .values()
        .map(|record| &record.manifest)
        .chain(
            state
                .published
                .values()
                .map(|record| &record.signed_manifest.manifest),
        )
        .chain(
            state
                .retirements
                .values()
                .map(|record| &record.signed_manifest.manifest),
        )
    {
        if !namespaces.insert((manifest.model.as_str(), manifest.run.as_str())) {
            return Err(ReplicationError::CorruptState);
        }
    }
    Ok(())
}

fn state_preimage(key_id: &str, state_bytes: &[u8]) -> Vec<u8> {
    let mut bytes =
        Vec::with_capacity(STATE_SIGNATURE_DOMAIN.len() + key_id.len() + state_bytes.len() + 16);
    bytes.extend_from_slice(STATE_SIGNATURE_DOMAIN);
    bytes.extend_from_slice(&(key_id.len() as u64).to_be_bytes());
    bytes.extend_from_slice(key_id.as_bytes());
    bytes.extend_from_slice(&(state_bytes.len() as u64).to_be_bytes());
    bytes.extend_from_slice(state_bytes);
    bytes
}

fn validate_key_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(ReplicationError::InvalidGeneration(
            "signing key id is invalid".into(),
        ));
    }
    Ok(())
}

fn create_real_directory(path: &Path) -> Result<()> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            return Err(ReplicationError::CorruptState);
        }
    } else {
        fs::create_dir_all(path)?;
    }
    Ok(())
}

fn require_real_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
        Ok(())
    } else {
        Err(ReplicationError::CorruptState)
    }
}

fn require_regular_file(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ReplicationError::CorruptState);
    }
    Ok(())
}

fn require_contained(root: &Path, path: &Path) -> Result<()> {
    if path.starts_with(root) {
        Ok(())
    } else {
        Err(ReplicationError::CorruptState)
    }
}

fn acquire_target_lock(model_dir: &Path, model: &str, run: &str) -> Result<File> {
    let mut digest = Sha256::new();
    digest.update(b"rw-generation-target-lock-v1\0");
    digest.update(model.as_bytes());
    digest.update([0]);
    digest.update(run.as_bytes());
    let name = format!(
        ".rw-replication-{}.lock",
        &hex_digest(digest.finalize())[..24]
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(model_dir.join(name))?;
    match FileExt::try_lock(&file) {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => Err(ReplicationError::Busy),
        Err(TryLockError::Error(error)) => Err(error.into()),
    }
}

fn hash_file(path: &Path, expected_size: u64) -> Result<String> {
    let mut file = File::open(path)?;
    let mut digest = Sha256::new();
    let mut remaining = expected_size;
    let mut buffer = [0_u8; 1024 * 1024];
    while remaining > 0 {
        let take = usize::try_from(remaining.min(buffer.len() as u64)).expect("bounded buffer");
        file.read_exact(&mut buffer[..take])?;
        digest.update(&buffer[..take]);
        remaining -= take as u64;
    }
    let mut trailing = [0_u8; 1];
    if file.read(&mut trailing)? != 0 {
        return Err(ReplicationError::CorruptState);
    }
    Ok(hex_digest(digest.finalize()))
}

fn hex_digest(bytes: impl AsRef<[u8]>) -> String {
    let bytes = bytes.as_ref();
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn utc_month(now_unix: i64) -> Result<String> {
    let date = DateTime::<Utc>::from_timestamp(now_unix, 0)
        .ok_or_else(|| ReplicationError::InvalidGeneration("timestamp is out of range".into()))?;
    Ok(format!("{:04}-{:02}", date.year(), date.month()))
}

fn utc_month_token_valid(value: &str) -> bool {
    value.len() == 7
        && value.as_bytes()[4] == b'-'
        && value[..4].bytes().all(|byte| byte.is_ascii_digit())
        && value[5..].bytes().all(|byte| byte.is_ascii_digit())
        && value[5..]
            .parse::<u8>()
            .is_ok_and(|month| (1..=12).contains(&month))
}

fn unique_child(parent: &Path, stem: &str) -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    parent.join(format!(
        "{stem}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
fn read_real_directories(root: &Path) -> Result<Vec<PathBuf>> {
    let mut inspected = 0;
    read_real_directories_bounded(root, &mut inspected, MAX_ABSOLUTE_GC_ENTRIES)
}

fn read_real_directories_bounded(
    root: &Path,
    inspected: &mut usize,
    maximum: usize,
) -> Result<Vec<PathBuf>> {
    let mut directories = Vec::new();
    for entry in fs::read_dir(root)? {
        *inspected = inspected
            .checked_add(1)
            .ok_or(ReplicationError::Quota("garbage collection entries"))?;
        if *inspected > maximum {
            return Err(ReplicationError::Quota("garbage collection entries"));
        }
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() {
            return Err(ReplicationError::CorruptState);
        }
        if metadata.file_type().is_dir() {
            directories.push(entry.path());
        }
    }
    Ok(directories)
}

#[cfg(test)]
fn read_real_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut inspected = 0;
    read_real_files_bounded(root, &mut inspected, MAX_ABSOLUTE_GC_ENTRIES)
}

fn read_real_files_bounded(
    root: &Path,
    inspected: &mut usize,
    maximum: usize,
) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(root)? {
        *inspected = inspected
            .checked_add(1)
            .ok_or(ReplicationError::Quota("garbage collection entries"))?;
        if *inspected > maximum {
            return Err(ReplicationError::Quota("garbage collection entries"));
        }
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
            return Err(ReplicationError::CorruptState);
        }
        files.push(entry.path());
    }
    Ok(files)
}

fn require_gc_deletion_budget(deleted: usize, maximum: usize) -> Result<()> {
    if deleted >= maximum {
        Err(ReplicationError::Quota("garbage collection deletions"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{GridShape, LatLonGrid};
    use rw_community_protocol::{
        AttributionNotice, BEGIN_RUN_GENERATION_SCHEMA, DataOrigin, FINALIZE_RUN_GENERATION_SCHEMA,
        PublicationGrant, REVOKE_RUN_GENERATION_SCHEMA, RUN_GENERATION_CHUNK_SCHEMA_V1,
        RUN_GENERATION_FILE_SCHEMA, RUN_GENERATION_REPLICATION_SCHEMA, RunGenerationFileChunk,
        SourceProvenance, generation_content_sha256,
    };
    use rw_store::ingest::HourIngestWriter;
    use rw_store::{RwsExactTime, RwsSourceProvenance};

    const MODEL: &str = "wrf-test";
    const RUN: &str = "20260812_00z";
    const VALID: i64 = 1_800_000_000;
    const NOW: i64 = VALID + 100;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let path = unique_child(&std::env::temp_dir(), &format!("rw-repl-{label}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn limits() -> RunGenerationLimits {
        RunGenerationLimits {
            max_generation_bytes: 16 * 1024 * 1024,
            max_files: 64,
            max_chunks: 256,
            max_chunk_bytes: 8 * 1024 * 1024,
            max_manifest_bytes: 2 * 1024 * 1024,
            max_retention_seconds: 86_400,
            max_provenance_entries: 8,
            max_attributions: 8,
        }
    }

    fn policy() -> ReplicationPolicy {
        ReplicationPolicy {
            max_owner_storage_bytes: 64 * 1024 * 1024,
            max_total_storage_bytes: 256 * 1024 * 1024,
            max_owner_generations: 8,
            max_total_generations: 32,
            max_owner_concurrent_uploads: 4,
            max_total_concurrent_uploads: 16,
            max_owner_monthly_upload_bytes: 128 * 1024 * 1024,
            max_total_monthly_upload_bytes: 512 * 1024 * 1024,
            upload_ttl_seconds: 3_600,
            max_state_bytes: 8 * 1024 * 1024,
            max_gc_entries: 100_000,
            max_gc_deletions: 10_000,
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[41; 32])
    }

    fn config(base: &Path, store: &Path, custom_policy: ReplicationPolicy) -> ReplicationConfig {
        ReplicationConfig {
            enabled: true,
            control_root: base.join("control"),
            store_root: store.into(),
            limits: limits(),
            policy: custom_policy,
            signing_key_id: "replication-test-v1".into(),
            signing_key: key(),
        }
    }

    fn owner(byte: u8) -> AuthenticatedOwner {
        AuthenticatedOwner::from_sha256(format!("{byte:02x}").repeat(32)).unwrap()
    }

    fn grid() -> LatLonGrid {
        LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![40.0, 40.0, 41.0, 41.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap()
    }

    fn build_source(store: &Path, value: f32) -> PathBuf {
        let mut writer = HourIngestWriter::begin_exact(
            store,
            MODEL,
            RUN,
            0,
            RwsExactTime::new(0, VALID),
            &grid(),
            None,
            "replication-test",
        )
        .unwrap();
        writer
            .set_source_provenance(vec![
                RwsSourceProvenance::new(
                    "simulation-owner",
                    vec!["generation".into()],
                    vec!["rws".into()],
                )
                .unwrap(),
            ])
            .unwrap();
        writer
            .add_derived_2d(
                "temperature",
                "K",
                &[value, value + 1.0, value + 2.0, value + 3.0],
            )
            .unwrap();
        writer.finish(VALID as u64).unwrap();
        store.join(MODEL).join(RUN)
    }

    fn bytes_sha(bytes: &[u8]) -> String {
        hex_digest(Sha256::digest(bytes))
    }

    fn descriptor(kind: RunGenerationFileKind, name: &str, bytes: &[u8]) -> RunGenerationFile {
        RunGenerationFile {
            schema: RUN_GENERATION_FILE_SCHEMA.into(),
            kind,
            file_name: name.into(),
            byte_size: bytes.len() as u64,
            file_sha256: bytes_sha(bytes),
            chunks: vec![RunGenerationFileChunk {
                schema: RUN_GENERATION_CHUNK_SCHEMA_V1.into(),
                ordinal: 0,
                file_offset: 0,
                object_sha256: bytes_sha(bytes),
                byte_size: bytes.len() as u64,
            }],
        }
    }

    fn source_manifest(
        source_store: &Path,
        generation_id: &str,
        authenticated_owner: &AuthenticatedOwner,
    ) -> RunGenerationReplicationManifest {
        let run_dir = source_store.join(MODEL).join(RUN);
        let source = RunSnapshot::open(source_store, MODEL, RUN).unwrap();
        let run_bytes = fs::read(run_dir.join("run.json")).unwrap();
        let grid_bytes = fs::read(run_dir.join("grid.rwg")).unwrap();
        let hour_bytes = fs::read(run_dir.join("f000.rws")).unwrap();
        let files = vec![
            descriptor(RunGenerationFileKind::RunManifest, "run.json", &run_bytes),
            descriptor(RunGenerationFileKind::Grid, "grid.rwg", &grid_bytes),
            descriptor(
                RunGenerationFileKind::Hour {
                    storage_slot: 0,
                    valid_unix: VALID,
                },
                "f000.rws",
                &hour_bytes,
            ),
        ];
        let mut manifest = RunGenerationReplicationManifest {
            schema: RUN_GENERATION_REPLICATION_SCHEMA.into(),
            generation_id: generation_id.into(),
            model: MODEL.into(),
            run: RUN.into(),
            source_snapshot_id: source.descriptor().snapshot_id.clone(),
            grid_hash: source.descriptor().grid_hash.clone(),
            owner_principal_sha256: authenticated_owner.sha256().into(),
            publication: PublicationGrant {
                data_origin: DataOrigin::PrivateWrf,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            source_provenance: vec![SourceProvenance {
                provider: "simulation-owner".into(),
                roles: vec!["generation".into()],
                products: vec!["rws".into()],
            }],
            total_bytes: files.iter().map(|file| file.byte_size).sum(),
            files,
            generation_sha256: "00".repeat(32),
            published_unix: VALID,
            retain_until_unix: VALID + 86_400,
            attributions: vec![AttributionNotice {
                provider: "simulation-owner".into(),
                notice: "Published by the simulation owner.".into(),
                source_url: "https://example.invalid/source".into(),
                license: "Owner-authorized redistribution".into(),
                license_url: "https://example.invalid/license".into(),
                terms_url: "https://example.invalid/terms".into(),
                disclaimer: "Experimental simulation.".into(),
            }],
            modification_notices: vec!["Inventoried as immutable rws chunks.".into()],
        };
        manifest.generation_sha256 = generation_content_sha256(&manifest).unwrap();
        manifest.validate(&limits()).unwrap();
        manifest
    }

    fn begin_request(manifest: RunGenerationReplicationManifest) -> BeginRunGenerationRequest {
        BeginRunGenerationRequest {
            schema: BEGIN_RUN_GENERATION_SCHEMA.into(),
            manifest,
        }
    }

    fn finalize_request(
        manifest: &RunGenerationReplicationManifest,
    ) -> FinalizeRunGenerationRequest {
        FinalizeRunGenerationRequest {
            schema: FINALIZE_RUN_GENERATION_SCHEMA.into(),
            generation_sha256: manifest.generation_sha256.clone(),
        }
    }

    fn upload_all(
        service: &GenerationReplicationService,
        owner: &AuthenticatedOwner,
        manifest: &RunGenerationReplicationManifest,
        source_run: &Path,
    ) {
        upload_all_at(service, owner, manifest, source_run, NOW);
    }

    fn upload_all_at(
        service: &GenerationReplicationService,
        owner: &AuthenticatedOwner,
        manifest: &RunGenerationReplicationManifest,
        source_run: &Path,
        now_unix: i64,
    ) {
        for file in &manifest.files {
            let bytes = fs::read(source_run.join(&file.file_name)).unwrap();
            service
                .upload_chunk(
                    owner,
                    &manifest.generation_id,
                    &file.chunks[0].object_sha256,
                    &bytes,
                    now_unix,
                )
                .unwrap();
        }
    }

    fn copy_inventory(source_run: &Path, target_run: &Path) {
        fs::create_dir_all(target_run).unwrap();
        for name in ["run.json", "grid.rwg", "f000.rws"] {
            fs::copy(source_run.join(name), target_run.join(name)).unwrap();
        }
    }

    fn reidentify_manifest(
        manifest: &RunGenerationReplicationManifest,
        generation_id: &str,
        authenticated_owner: &AuthenticatedOwner,
    ) -> RunGenerationReplicationManifest {
        let mut identified = manifest.clone();
        identified.generation_id = generation_id.into();
        identified.owner_principal_sha256 = authenticated_owner.sha256().into();
        // Content identity deliberately excludes transport/owner identity.
        assert_eq!(
            identified.generation_sha256,
            generation_content_sha256(&identified).unwrap()
        );
        identified.validate(&limits()).unwrap();
        identified
    }

    fn write_authenticated_state_without_validation(
        service: &GenerationReplicationService,
        state: PersistentState,
    ) {
        let state_bytes = serde_json::to_vec(&state).unwrap();
        let signature = service
            .signing_key
            .sign(&state_preimage(&service.signing_key_id, &state_bytes));
        let envelope = StateEnvelope {
            schema: STATE_ENVELOPE_SCHEMA.into(),
            signing_key_id: service.signing_key_id.clone(),
            state,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        };
        atomic_write_bytes(&service.state_path, &serde_json::to_vec(&envelope).unwrap()).unwrap();
    }

    #[test]
    fn run_namespace_is_exclusive_before_quota_or_chunk_work_and_survives_restart() {
        let dir = TestDir::new("namespace-admission");
        let source_store = dir.0.join("source");
        build_source(&source_store, 279.0);
        let target_store = dir.0.join("target");
        let first_owner = owner(0x0a);
        let second_owner = owner(0x0b);
        let first = source_manifest(&source_store, "namespace-first", &first_owner);
        let same_owner_duplicate =
            reidentify_manifest(&first, "namespace-same-owner-duplicate", &first_owner);
        let other_owner_duplicate =
            reidentify_manifest(&first, "namespace-other-owner-duplicate", &second_owner);

        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        let initial = service
            .begin(&first_owner, begin_request(first.clone()), NOW)
            .unwrap();
        let replay = service
            .begin(&first_owner, begin_request(first.clone()), NOW + 1)
            .unwrap();
        assert_eq!(replay.generation_id, initial.generation_id);
        assert_eq!(replay.generation_sha256, initial.generation_sha256);
        assert_eq!(replay.missing_chunks, initial.missing_chunks);

        assert!(matches!(
            service.begin(
                &first_owner,
                begin_request(same_owner_duplicate.clone()),
                NOW + 1
            ),
            Err(ReplicationError::Conflict)
        ));
        assert!(matches!(
            service.begin(
                &second_owner,
                begin_request(other_owner_duplicate.clone()),
                NOW + 1
            ),
            Err(ReplicationError::Conflict)
        ));
        {
            let state = service.lock_state().unwrap();
            assert_eq!(state.uploads.len(), 1);
            assert!(state.uploads.contains_key(&first.generation_id));
            assert_eq!(state.billing.total_upload_bytes, 0);
        }
        assert_eq!(
            fs::read_dir(dir.0.join("control/chunks")).unwrap().count(),
            0,
            "begin conflicts must not create or charge chunk storage"
        );
        drop(service);

        let reopened =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        assert!(matches!(
            reopened.begin(&first_owner, begin_request(same_owner_duplicate), NOW + 2),
            Err(ReplicationError::Conflict)
        ));
        assert!(matches!(
            reopened.begin(&second_owner, begin_request(other_owner_duplicate), NOW + 2),
            Err(ReplicationError::Conflict)
        ));
    }

    #[test]
    fn revoke_and_expiry_release_a_namespace_only_after_authenticated_retirement() {
        let dir = TestDir::new("namespace-retirement");
        let source_store = dir.0.join("source");
        let source_run = build_source(&source_store, 281.0);
        let target_store = dir.0.join("target");
        let authenticated_owner = owner(0x0c);
        let first = source_manifest(&source_store, "namespace-revoke", &authenticated_owner);
        let replacement =
            reidentify_manifest(&first, "namespace-after-revoke", &authenticated_owner);
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(first.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &first, &source_run);
        service
            .finalize(
                &authenticated_owner,
                &first.generation_id,
                finalize_request(&first),
                NOW,
            )
            .unwrap();
        assert!(matches!(
            service.begin(
                &authenticated_owner,
                begin_request(replacement.clone()),
                NOW + 1
            ),
            Err(ReplicationError::Conflict)
        ));
        service
            .revoke(
                &authenticated_owner,
                &first.generation_id,
                RevokeRunGenerationRequest {
                    schema: REVOKE_RUN_GENERATION_SCHEMA.into(),
                    generation_sha256: first.generation_sha256.clone(),
                    rights_withdrawn: true,
                    reason: "Owner is replacing this generation.".into(),
                },
                NOW + 2,
            )
            .unwrap();
        assert!(!target_store.join(MODEL).join(RUN).join("run.json").exists());
        service
            .begin(
                &authenticated_owner,
                begin_request(replacement.clone()),
                NOW + 3,
            )
            .unwrap();
        service
            .cancel_upload(&authenticated_owner, &replacement.generation_id, NOW + 3)
            .unwrap();

        let expiry_dir = TestDir::new("namespace-expiry");
        let expiry_target = expiry_dir.0.join("target");
        let expiry_service =
            GenerationReplicationService::open(config(&expiry_dir.0, &expiry_target, policy()))
                .unwrap();
        let expiring = reidentify_manifest(&first, "namespace-expiring", &authenticated_owner);
        expiry_service
            .begin(
                &authenticated_owner,
                begin_request(expiring.clone()),
                NOW + 4,
            )
            .unwrap();
        upload_all_at(
            &expiry_service,
            &authenticated_owner,
            &expiring,
            &source_run,
            NOW + 4,
        );
        expiry_service
            .finalize(
                &authenticated_owner,
                &expiring.generation_id,
                finalize_request(&expiring),
                NOW + 4,
            )
            .unwrap();
        let mut after_expiry =
            reidentify_manifest(&first, "namespace-after-expiry", &authenticated_owner);
        after_expiry.published_unix = expiring.retain_until_unix;
        after_expiry.retain_until_unix = expiring.retain_until_unix + 3_600;
        after_expiry.validate(&limits()).unwrap();
        assert!(matches!(
            expiry_service.begin(
                &authenticated_owner,
                begin_request(after_expiry.clone()),
                expiring.retain_until_unix
            ),
            Err(ReplicationError::Conflict)
        ));
        let report = expiry_service
            .garbage_collect(expiring.retain_until_unix)
            .unwrap();
        assert_eq!(report.retired_generations, 1);
        assert!(
            !expiry_target
                .join(MODEL)
                .join(RUN)
                .join("run.json")
                .exists()
        );

        expiry_service
            .begin(
                &authenticated_owner,
                begin_request(after_expiry),
                expiring.retain_until_unix,
            )
            .unwrap();
    }

    #[test]
    fn corrupt_duplicate_namespace_cannot_delete_live_bytes_or_survive_restart() {
        let dir = TestDir::new("namespace-corrupt-runtime");
        let source_store = dir.0.join("source");
        let source_run = build_source(&source_store, 283.0);
        let target_store = dir.0.join("target");
        let first_owner = owner(0x0d);
        let second_owner = owner(0x0e);
        let first = source_manifest(&source_store, "namespace-retiring", &first_owner);
        let second = reidentify_manifest(&first, "namespace-still-live", &second_owner);
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&first_owner, begin_request(first.clone()), NOW)
            .unwrap();
        upload_all(&service, &first_owner, &first, &source_run);
        let published = service
            .finalize(
                &first_owner,
                &first.generation_id,
                finalize_request(&first),
                NOW,
            )
            .unwrap();
        {
            let mut state = service.lock_state().unwrap();
            let retiring = state.published.remove(&first.generation_id).unwrap();
            state
                .retirements
                .insert(first.generation_id.clone(), retiring);
            state.tombstones.insert(
                first.generation_id.clone(),
                RunGenerationTombstone {
                    schema: RUN_GENERATION_TOMBSTONE_SCHEMA.into(),
                    generation_id: first.generation_id.clone(),
                    generation_sha256: first.generation_sha256.clone(),
                    owner_principal_sha256: first_owner.sha256().into(),
                    revoked_unix: NOW + 1,
                    rights_withdrawn: true,
                    reason: "Synthetic legacy retirement collision.".into(),
                },
            );
            state.published.insert(
                second.generation_id.clone(),
                PublishedRecord {
                    signed_manifest: sign_run_generation(
                        second,
                        service.signing_key_id.clone(),
                        &service.signing_key,
                        &service.limits,
                    )
                    .unwrap(),
                    local_snapshot_id: published.published.local_snapshot_id.clone(),
                    published_unix: NOW,
                },
            );
        }
        assert!(matches!(
            service.drain_retirements(1),
            Err(ReplicationError::CorruptState)
        ));
        assert!(
            target_store
                .join(MODEL)
                .join(RUN)
                .join("run.json")
                .is_file()
        );

        let restart_dir = TestDir::new("namespace-corrupt-restart");
        let restart_source = restart_dir.0.join("source");
        build_source(&restart_source, 284.0);
        let restart_target = restart_dir.0.join("target");
        let restart_owner = owner(0x0f);
        let restart_first = source_manifest(&restart_source, "restart-first", &restart_owner);
        let restart_second =
            reidentify_manifest(&restart_first, "restart-duplicate", &restart_owner);
        let restart_service =
            GenerationReplicationService::open(config(&restart_dir.0, &restart_target, policy()))
                .unwrap();
        restart_service
            .begin(&restart_owner, begin_request(restart_first.clone()), NOW)
            .unwrap();
        let corrupt_state = {
            let state = restart_service.lock_state().unwrap();
            let mut corrupt = state.clone();
            let mut duplicate = corrupt.uploads[&restart_first.generation_id].clone();
            duplicate.manifest = restart_second;
            corrupt
                .uploads
                .insert(duplicate.manifest.generation_id.clone(), duplicate);
            corrupt
        };
        write_authenticated_state_without_validation(&restart_service, corrupt_state);
        drop(restart_service);
        assert!(matches!(
            GenerationReplicationService::open(config(&restart_dir.0, &restart_target, policy())),
            Err(ReplicationError::CorruptState)
        ));
    }

    #[test]
    fn resumable_restart_finalize_authorization_and_tombstone_lifecycle() {
        let dir = TestDir::new("lifecycle");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x11);
        let other = owner(0x22);
        let manifest = source_manifest(&source_store, "wrf-case-lifecycle", &authenticated_owner);

        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        let status = service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        assert_eq!(status.missing_chunks, 3);
        let page = service
            .missing_chunks(&authenticated_owner, &manifest.generation_id, None, 2, NOW)
            .unwrap();
        assert_eq!(page.chunks.len(), 2);
        assert!(page.next_after.is_some());
        assert!(matches!(
            service.upload_chunk(
                &other,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                &fs::read(source_run.join("run.json")).unwrap(),
                NOW
            ),
            Err(ReplicationError::WrongOwner)
        ));
        service
            .upload_chunk(
                &authenticated_owner,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                &fs::read(source_run.join("run.json")).unwrap(),
                NOW,
            )
            .unwrap();
        drop(service);

        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        assert_eq!(
            service
                .status(&authenticated_owner, &manifest.generation_id, NOW)
                .unwrap()
                .missing_chunks,
            2
        );
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        let finalized = service
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();
        assert_eq!(
            finalized.published.source_snapshot_id,
            manifest.source_snapshot_id
        );
        assert!(is_sha256(&finalized.published.local_snapshot_id));
        assert_ne!(
            finalized.published.local_snapshot_id,
            manifest.source_snapshot_id
        );
        assert_eq!(
            service.authorize_query_at(MODEL, RUN, NOW).unwrap(),
            finalized.published
        );
        assert!(
            dir.0
                .join("control/manifests/wrf-case-lifecycle.json")
                .is_file()
        );
        assert!(matches!(
            service.revoke(
                &other,
                &manifest.generation_id,
                RevokeRunGenerationRequest {
                    schema: REVOKE_RUN_GENERATION_SCHEMA.into(),
                    generation_sha256: manifest.generation_sha256.clone(),
                    rights_withdrawn: true,
                    reason: "Owner withdrew publication rights.".into(),
                },
                NOW + 1,
            ),
            Err(ReplicationError::WrongOwner)
        ));
        service
            .revoke(
                &authenticated_owner,
                &manifest.generation_id,
                RevokeRunGenerationRequest {
                    schema: REVOKE_RUN_GENERATION_SCHEMA.into(),
                    generation_sha256: manifest.generation_sha256.clone(),
                    rights_withdrawn: true,
                    reason: "Owner withdrew publication rights.".into(),
                },
                NOW + 1,
            )
            .unwrap();
        assert!(!target_store.join(MODEL).join(RUN).join("run.json").exists());
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_err());
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
        assert!(matches!(
            service.begin(&authenticated_owner, begin_request(manifest), NOW + 2),
            Err(ReplicationError::Conflict)
        ));
        let orphan_manifest = dir.0.join("control/manifests/orphan-generation.json");
        fs::write(&orphan_manifest, b"unreferenced signed-manifest candidate").unwrap();
        let report = service.garbage_collect(NOW + 2).unwrap();
        assert_eq!(report.orphan_manifests, 1);
        assert!(!orphan_manifest.exists());
        assert!(
            read_real_directories(&dir.0.join("control/chunks"))
                .unwrap()
                .into_iter()
                .all(|prefix| read_real_files(&prefix).unwrap().is_empty())
        );
    }

    #[test]
    fn signed_retention_is_exclusive_terminal_and_restart_safe_under_cleanup_contention() {
        let dir = TestDir::new("retention-terminal");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 281.0);
        let authenticated_owner = owner(0x2b);
        let manifest = source_manifest(
            &source_store,
            "wrf-case-retention-terminal",
            &authenticated_owner,
        );
        let generation_id = manifest.generation_id.clone();
        let expiry = manifest.retain_until_unix;
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        let finalized = service
            .finalize(
                &authenticated_owner,
                &generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();

        assert_eq!(
            service.authorize_query_at(MODEL, RUN, expiry - 1).unwrap(),
            finalized.published
        );
        assert!(
            service
                .published_at(&generation_id, expiry - 1)
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            service.authorize_query_at(
                MODEL,
                RUN,
                manifest.published_unix - CLOCK_SKEW_SECONDS - 1
            ),
            Err(ReplicationError::Expired)
        ));

        // Hold the cooperating store lock across the deadline. Authorization
        // must still end durably even though byte retirement must be retried.
        let installed = target_store.join(MODEL).join(RUN);
        let held = RunLock::try_acquire(&installed).unwrap().unwrap();
        let report = service.garbage_collect(expiry).unwrap();
        assert_eq!(report.expired_publications, 1);
        assert_eq!(report.retired_generations, 0);
        assert_eq!(report.pending_retirements, 1);
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_ok());
        let status = service.service_status().unwrap();
        assert_eq!(status.published_bytes, 0);
        assert_eq!(status.pending_retirement_bytes, manifest.total_bytes);

        // A backward wall clock cannot resurrect the publication once the
        // terminal transition has been authenticated and persisted.
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
        assert!(service.published_at(&generation_id, NOW).unwrap().is_none());
        assert!(service.authorized_publications_at(NOW).unwrap().is_empty());
        assert!(matches!(
            service.begin(&authenticated_owner, begin_request(manifest.clone()), NOW),
            Err(ReplicationError::Conflict)
        ));
        {
            let state = service.lock_state().unwrap();
            let tombstone = state.tombstones.get(&generation_id).unwrap();
            assert_eq!(tombstone.revoked_unix, expiry);
            assert_eq!(tombstone.reason, "Signed publication retention expired.");
            assert!(!state.published.contains_key(&generation_id));
            assert!(state.retirements.contains_key(&generation_id));
        }

        drop(held);
        let retry = service.garbage_collect(NOW).unwrap();
        assert_eq!(retry.expired_publications, 0);
        assert_eq!(retry.retired_generations, 1);
        assert_eq!(retry.pending_retirements, 0);
        assert_eq!(
            service.service_status().unwrap().pending_retirement_bytes,
            0
        );
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_err());
        drop(service);

        let reopened =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        assert!(matches!(
            reopened.authorize_query_at(MODEL, RUN, expiry - 1),
            Err(ReplicationError::NotFound)
        ));
        assert!(reopened.authorized_publications_at(NOW).unwrap().is_empty());
        let state = reopened.lock_state().unwrap();
        assert!(state.tombstones.contains_key(&generation_id));
        assert!(!state.published.contains_key(&generation_id));
        assert!(!state.retirements.contains_key(&generation_id));
    }

    #[test]
    fn retirement_rechecks_namespace_ownership_after_acquiring_deletion_locks() {
        let dir = TestDir::new("retirement-race");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 282.0);
        let authenticated_owner = owner(0x5b);
        let manifest = source_manifest(
            &source_store,
            "wrf-case-retirement-race",
            &authenticated_owner,
        );
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        let finalized = service
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();

        service
            .retire_if_current_with_hook(
                &manifest,
                &finalized.published.local_snapshot_id,
                |installed| {
                    // Deterministically model a scheduler taking ownership in
                    // the exact interval between optimistic inspection and
                    // acquisition of both deletion locks.
                    fs::write(installed.join(SCHEDULER_OWNER_MARKER), b"scheduler-owned")?;
                    Ok(())
                },
            )
            .unwrap();

        let installed = target_store.join(MODEL).join(RUN);
        assert!(installed.join("run.json").is_file());
        assert!(installed.join(SCHEDULER_OWNER_MARKER).is_file());
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_ok());
    }

    #[test]
    fn finalize_missing_tamper_and_durable_state_failure_fail_closed() {
        let dir = TestDir::new("rollback");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x31);
        let manifest = source_manifest(&source_store, "wrf-case-rollback", &authenticated_owner);
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        assert!(matches!(
            service.finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW
            ),
            Err(ReplicationError::MissingChunk)
        ));
        let mut tampered = fs::read(source_run.join("run.json")).unwrap();
        tampered[0] ^= 1;
        assert!(
            service
                .upload_chunk(
                    &authenticated_owner,
                    &manifest.generation_id,
                    &manifest.files[0].chunks[0].object_sha256,
                    &tampered,
                    NOW
                )
                .is_err()
        );
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        service.fail_next_persist.store(true, Ordering::SeqCst);
        assert!(
            service
                .finalize(
                    &authenticated_owner,
                    &manifest.generation_id,
                    finalize_request(&manifest),
                    NOW
                )
                .is_err()
        );
        assert!(!target_store.join(MODEL).join(RUN).exists());
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
        let finalized = service
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();
        assert_eq!(
            service.authorize_query_at(MODEL, RUN, NOW).unwrap(),
            finalized.published
        );
    }

    #[test]
    fn preinstalled_exact_generation_is_hidden_then_idempotently_adopted() {
        let dir = TestDir::new("orphan-adopt");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x41);
        let manifest = source_manifest(&source_store, "wrf-case-adopt", &authenticated_owner);
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        copy_inventory(&source_run, &target_store.join(MODEL).join(RUN));
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_ok());
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
        let result = service
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();
        assert_eq!(
            service.authorize_query_at(MODEL, RUN, NOW).unwrap(),
            result.published
        );
    }

    #[test]
    fn scheduler_owned_exact_generation_is_never_adopted() {
        let dir = TestDir::new("scheduler-owner-conflict");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x49);
        let manifest = source_manifest(
            &source_store,
            "wrf-case-scheduler-owned",
            &authenticated_owner,
        );
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        let installed = target_store.join(MODEL).join(RUN);
        copy_inventory(&source_run, &installed);
        fs::write(installed.join(SCHEDULER_OWNER_MARKER), b"scheduler-owned").unwrap();

        assert!(matches!(
            service.finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW
            ),
            Err(ReplicationError::Conflict)
        ));
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
        assert!(installed.join(SCHEDULER_OWNER_MARKER).is_file());
    }

    #[test]
    fn different_existing_generation_is_never_replaced_or_moved() {
        let dir = TestDir::new("conflict");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 280.0);
        let target_run = build_source(&target_store, 310.0);
        let before = bytes_sha(&fs::read(target_run.join("f000.rws")).unwrap());
        let authenticated_owner = owner(0x51);
        let manifest = source_manifest(&source_store, "wrf-case-conflict", &authenticated_owner);
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        assert!(matches!(
            service.finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW
            ),
            Err(ReplicationError::Conflict)
        ));
        assert_eq!(
            before,
            bytes_sha(&fs::read(target_run.join("f000.rws")).unwrap())
        );
        assert!(RunSnapshot::open(&target_store, MODEL, RUN).is_ok());
        assert!(matches!(
            service.authorize_query_at(MODEL, RUN, NOW),
            Err(ReplicationError::NotFound)
        ));
    }

    #[test]
    fn rights_ecmwf_paths_quotas_expiry_kill_switch_and_state_tamper_fail_closed() {
        let dir = TestDir::new("controls");
        let source_store = dir.0.join("source");
        build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x61);
        let manifest = source_manifest(&source_store, "wrf-case-controls", &authenticated_owner);

        let mut no_publication = manifest.clone();
        no_publication.publication.explicit_owner_publication = false;
        no_publication.generation_sha256 = generation_content_sha256(&no_publication).unwrap();
        let service =
            GenerationReplicationService::open(config(&dir.0, &dir.0.join("target"), policy()))
                .unwrap();
        assert!(
            service
                .begin(&authenticated_owner, begin_request(no_publication), NOW)
                .is_err()
        );

        let mut ecmwf = manifest.clone();
        ecmwf.source_provenance = vec![SourceProvenance {
            provider: "ecmwf-open-data".into(),
            roles: vec!["generation".into()],
            products: vec!["ifs".into()],
        }];
        ecmwf.attributions.clear();
        ecmwf.modification_notices.clear();
        ecmwf.generation_sha256 = generation_content_sha256(&ecmwf).unwrap();
        assert!(
            service
                .begin(&authenticated_owner, begin_request(ecmwf), NOW)
                .is_err()
        );

        let mut reserved = manifest.clone();
        reserved.files[2].file_name = "CON.rws".into();
        reserved.generation_sha256 = generation_content_sha256(&reserved).unwrap();
        assert!(
            service
                .begin(&authenticated_owner, begin_request(reserved), NOW)
                .is_err()
        );

        service.set_kill_switch(true).unwrap();
        assert!(matches!(
            service.begin(&authenticated_owner, begin_request(manifest.clone()), NOW),
            Err(ReplicationError::KillSwitch)
        ));
        service.set_kill_switch(false).unwrap();
        assert!(matches!(
            service.begin(
                &authenticated_owner,
                begin_request(manifest.clone()),
                VALID + 86_400
            ),
            Err(ReplicationError::Expired)
        ));
        drop(service);

        let quota_dir = TestDir::new("quota");
        let mut tiny = policy();
        tiny.max_owner_storage_bytes = manifest.total_bytes - 1;
        let quota_service = GenerationReplicationService::open(config(
            &quota_dir.0,
            &quota_dir.0.join("target"),
            tiny,
        ))
        .unwrap();
        assert!(matches!(
            quota_service.begin(&authenticated_owner, begin_request(manifest.clone()), NOW),
            Err(ReplicationError::Quota("owner storage"))
        ));
        drop(quota_service);

        let tamper_dir = TestDir::new("state-tamper");
        let tamper_service = GenerationReplicationService::open(config(
            &tamper_dir.0,
            &tamper_dir.0.join("target"),
            policy(),
        ))
        .unwrap();
        tamper_service
            .begin(&authenticated_owner, begin_request(manifest), NOW)
            .unwrap();
        drop(tamper_service);
        let state_path = tamper_dir.0.join("control/state.json");
        let mut state = fs::read_to_string(&state_path).unwrap();
        state = state.replacen("\"kill_switch\":false", "\"kill_switch\":true", 1);
        fs::write(&state_path, state).unwrap();
        assert!(matches!(
            GenerationReplicationService::open(config(
                &tamper_dir.0,
                &tamper_dir.0.join("target"),
                policy()
            )),
            Err(ReplicationError::CorruptState)
        ));
    }

    #[test]
    fn corrupt_reconstructed_run_is_rejected_before_visibility() {
        let dir = TestDir::new("corrupt-run");
        let source_store = dir.0.join("source");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x71);
        let mut manifest = source_manifest(&source_store, "wrf-case-corrupt", &authenticated_owner);
        let malformed = b"{";
        manifest.files[0] = descriptor(RunGenerationFileKind::RunManifest, "run.json", malformed);
        manifest.total_bytes = manifest.files.iter().map(|file| file.byte_size).sum();
        manifest.generation_sha256 = generation_content_sha256(&manifest).unwrap();
        manifest.validate(&limits()).unwrap();
        let target_store = dir.0.join("target");
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        service
            .upload_chunk(
                &authenticated_owner,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                malformed,
                NOW,
            )
            .unwrap();
        for file in &manifest.files[1..] {
            let bytes = fs::read(source_run.join(&file.file_name)).unwrap();
            service
                .upload_chunk(
                    &authenticated_owner,
                    &manifest.generation_id,
                    &file.chunks[0].object_sha256,
                    &bytes,
                    NOW,
                )
                .unwrap();
        }
        assert!(matches!(
            service.finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW
            ),
            Err(ReplicationError::InvalidGeneration(_))
                | Err(ReplicationError::Store(_))
                | Err(ReplicationError::Query(_))
        ));
        assert!(!target_store.join(MODEL).join(RUN).exists());
    }

    #[test]
    fn monthly_upload_accounting_charges_replays_survives_restart_and_rolls_utc_month() {
        let dir = TestDir::new("monthly-accounting");
        let source_store = dir.0.join("source");
        let source_run = build_source(&source_store, 280.0);
        let authenticated_owner = owner(0x81);
        let manifest = source_manifest(&source_store, "wrf-case-month-one", &authenticated_owner);
        let run_bytes = fs::read(source_run.join("run.json")).unwrap();
        let mut bounded_policy = policy();
        bounded_policy.max_owner_monthly_upload_bytes = run_bytes.len() as u64;
        bounded_policy.max_total_monthly_upload_bytes = run_bytes.len() as u64;
        let target_store = dir.0.join("target");
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, bounded_policy))
                .unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        service
            .upload_chunk(
                &authenticated_owner,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                &run_bytes,
                NOW,
            )
            .unwrap();
        assert!(matches!(
            service.upload_chunk(
                &authenticated_owner,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                &run_bytes,
                NOW,
            ),
            Err(ReplicationError::Quota("monthly owner upload"))
        ));
        drop(service);

        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, bounded_policy))
                .unwrap();
        assert!(matches!(
            service.upload_chunk(
                &authenticated_owner,
                &manifest.generation_id,
                &manifest.files[0].chunks[0].object_sha256,
                &run_bytes,
                NOW,
            ),
            Err(ReplicationError::Quota("monthly owner upload"))
        ));

        let next_month = NOW + 40 * 24 * 60 * 60;
        service.garbage_collect(next_month).unwrap();
        let mut next = manifest;
        next.generation_id = "wrf-case-month-two".into();
        next.published_unix = next_month - 100;
        next.retain_until_unix = next_month + 3_600;
        next.validate(&limits()).unwrap();
        service
            .begin(
                &authenticated_owner,
                begin_request(next.clone()),
                next_month,
            )
            .unwrap();
        // The exact bytes may have been present before GC; either way a valid
        // admitted request is charged in the new UTC calendar month.
        service
            .upload_chunk(
                &authenticated_owner,
                &next.generation_id,
                &next.files[0].chunks[0].object_sha256,
                &run_bytes,
                next_month,
            )
            .unwrap();
        let state = service.lock_state().unwrap();
        assert_eq!(state.billing.utc_month, utc_month(next_month).unwrap());
        assert_eq!(state.billing.total_upload_bytes, run_bytes.len() as u64);
    }

    #[test]
    fn successful_finalize_reconciles_exactly_after_restart_and_lost_response() {
        let dir = TestDir::new("finalize-reconcile");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 289.0);
        let authenticated_owner = owner(0x91);
        let other = owner(0x92);
        let manifest = source_manifest(
            &source_store,
            "wrf-case-finalize-reconcile",
            &authenticated_owner,
        );
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        upload_all(&service, &authenticated_owner, &manifest, &source_run);
        let first = service
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW,
            )
            .unwrap();
        assert!(!first.was_already_published);
        service.set_kill_switch(true).unwrap();
        drop(service);

        let reopened =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        let replay = reopened
            .finalize(
                &authenticated_owner,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW + 1,
            )
            .unwrap();
        assert!(replay.was_already_published);
        assert_eq!(replay.published, first.published);
        assert_eq!(replay.signed_manifest, first.signed_manifest);
        assert!(matches!(
            reopened.finalize(
                &other,
                &manifest.generation_id,
                finalize_request(&manifest),
                NOW + 1
            ),
            Err(ReplicationError::WrongOwner)
        ));
        let mut wrong_hash = finalize_request(&manifest);
        wrong_hash.generation_sha256 = "ab".repeat(32);
        assert!(matches!(
            reopened.finalize(
                &authenticated_owner,
                &manifest.generation_id,
                wrong_hash,
                NOW + 1
            ),
            Err(ReplicationError::Conflict)
        ));
        assert!(
            !reopened
                .owner_capabilities(&authenticated_owner, NOW + 1)
                .unwrap()
                .accepting_uploads
        );
    }

    #[test]
    fn owner_records_are_isolated_paginated_and_cancel_releases_while_killed() {
        let dir = TestDir::new("owner-records-cancel");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        let source_run = build_source(&source_store, 291.0);
        let authenticated_owner = owner(0xa1);
        let other = owner(0xa2);
        let first_manifest = source_manifest(
            &source_store,
            "alpha-owner-generation",
            &authenticated_owner,
        );
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(
                &authenticated_owner,
                begin_request(first_manifest.clone()),
                NOW,
            )
            .unwrap();
        upload_all(&service, &authenticated_owner, &first_manifest, &source_run);
        service
            .finalize(
                &authenticated_owner,
                &first_manifest.generation_id,
                finalize_request(&first_manifest),
                NOW,
            )
            .unwrap();
        assert_eq!(
            service
                .owner_record(&authenticated_owner, &first_manifest.generation_id, NOW)
                .unwrap()
                .state,
            RunGenerationOwnerRecordState::Published
        );
        service
            .revoke(
                &authenticated_owner,
                &first_manifest.generation_id,
                RevokeRunGenerationRequest {
                    schema: REVOKE_RUN_GENERATION_SCHEMA.into(),
                    generation_sha256: first_manifest.generation_sha256.clone(),
                    rights_withdrawn: true,
                    reason: "Owner replaced the test publication.".into(),
                },
                NOW + 1,
            )
            .unwrap();
        assert!(!target_store.join(MODEL).join(RUN).join("run.json").exists());

        let second_generation_id = "bravo-owner-generation";
        {
            let mut state = service.lock_state().unwrap();
            state.tombstones.insert(
                second_generation_id.into(),
                RunGenerationTombstone {
                    schema: RUN_GENERATION_TOMBSTONE_SCHEMA.into(),
                    generation_id: second_generation_id.into(),
                    generation_sha256: "ba".repeat(32),
                    owner_principal_sha256: authenticated_owner.sha256().into(),
                    revoked_unix: NOW + 2,
                    rights_withdrawn: true,
                    reason: "Synthetic prior owner publication for paging.".into(),
                },
            );
            service.persist(&state).unwrap();
        }

        let first_page = service
            .owner_records(&authenticated_owner, None, 1, NOW + 3)
            .unwrap();
        assert_eq!(first_page.records.len(), 1);
        assert_eq!(
            first_page.records[0].state,
            RunGenerationOwnerRecordState::Tombstone
        );
        assert_eq!(
            first_page.next_after.as_deref(),
            Some("alpha-owner-generation")
        );
        let second_page = service
            .owner_records(
                &authenticated_owner,
                first_page.next_after.as_deref(),
                1,
                NOW + 3,
            )
            .unwrap();
        assert_eq!(second_page.records.len(), 1);
        assert_eq!(
            second_page.records[0].state,
            RunGenerationOwnerRecordState::Tombstone
        );
        assert!(second_page.next_after.is_none());
        assert!(matches!(
            service.owner_record(&other, second_generation_id, NOW + 3),
            Err(ReplicationError::NotFound)
        ));
        assert!(
            service
                .owner_records(&other, None, 10, NOW + 3)
                .unwrap()
                .records
                .is_empty()
        );

        let mut third_manifest = source_manifest(
            &source_store,
            "charlie-active-generation",
            &authenticated_owner,
        );
        third_manifest.model = "wrf-test-pending".into();
        third_manifest.generation_sha256 = generation_content_sha256(&third_manifest).unwrap();
        third_manifest.validate(&limits()).unwrap();
        service
            .begin(
                &authenticated_owner,
                begin_request(third_manifest.clone()),
                NOW + 3,
            )
            .unwrap();
        let before = service
            .owner_capabilities(&authenticated_owner, NOW + 3)
            .unwrap();
        assert_eq!(before.usage.active_uploads, 1);
        assert_eq!(before.usage.reserved_bytes, third_manifest.total_bytes);
        assert_eq!(before.usage.live_publications, 0);
        assert_eq!(before.usage.tombstones, 2);
        service.set_kill_switch(true).unwrap();
        assert!(matches!(
            service.status(&authenticated_owner, &third_manifest.generation_id, NOW + 3),
            Err(ReplicationError::KillSwitch)
        ));
        assert!(matches!(
            service.cancel_upload(&other, &third_manifest.generation_id, NOW + 3),
            Err(ReplicationError::WrongOwner)
        ));
        let cancelled = service
            .cancel_upload(&authenticated_owner, &third_manifest.generation_id, NOW + 3)
            .unwrap();
        assert_eq!(
            cancelled.released_reserved_bytes,
            third_manifest.total_bytes
        );
        let after = service
            .owner_capabilities(&authenticated_owner, NOW + 3)
            .unwrap();
        assert!(!after.accepting_uploads);
        assert_eq!(after.usage.active_uploads, 0);
        assert_eq!(after.usage.reserved_bytes, 0);
        drop(service);

        let reopened =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        assert_eq!(
            reopened
                .owner_capabilities(&authenticated_owner, NOW + 4)
                .unwrap()
                .usage
                .active_uploads,
            0
        );
        assert!(matches!(
            reopened.cancel_upload(&authenticated_owner, &third_manifest.generation_id, NOW + 4),
            Err(ReplicationError::NotFound)
        ));
    }

    #[test]
    fn lowered_owner_quota_reports_truthful_overage_and_keeps_cancel_available() {
        let dir = TestDir::new("lowered-owner-quota");
        let source_store = dir.0.join("source");
        let target_store = dir.0.join("target");
        build_source(&source_store, 293.0);
        let authenticated_owner = owner(0xb1);
        let manifest = source_manifest(
            &source_store,
            "wrf-case-lowered-owner-quota",
            &authenticated_owner,
        );
        let service =
            GenerationReplicationService::open(config(&dir.0, &target_store, policy())).unwrap();
        service
            .begin(&authenticated_owner, begin_request(manifest.clone()), NOW)
            .unwrap();
        drop(service);

        let mut lowered = policy();
        lowered.max_owner_storage_bytes = 1;
        let reopened =
            GenerationReplicationService::open(config(&dir.0, &target_store, lowered)).unwrap();
        let capabilities = reopened
            .owner_capabilities(&authenticated_owner, NOW + 1)
            .unwrap();
        assert_eq!(capabilities.quota.maximum_storage_bytes, 1);
        assert_eq!(capabilities.usage.reserved_bytes, manifest.total_bytes);
        assert!(capabilities.usage.reserved_bytes > capabilities.quota.maximum_storage_bytes);
        assert!(!capabilities.accepting_uploads);
        reopened
            .cancel_upload(&authenticated_owner, &manifest.generation_id, NOW + 1)
            .unwrap();
        assert_eq!(
            reopened
                .owner_capabilities(&authenticated_owner, NOW + 1)
                .unwrap()
                .usage
                .reserved_bytes,
            0
        );
    }
}

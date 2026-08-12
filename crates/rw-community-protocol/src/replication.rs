//! Signed, content-addressed replication contract for complete `.rws` runs.
//!
//! The contract inventories only the three closed Rusty Weather file families:
//! `run.json`, `grid.rwg`, and manifest-listed `.rws` hour files. It cannot
//! carry a local path, arbitrary filename, URL, or raw `wrfout` file. A server
//! must still deep-validate a reconstructed generation before atomically
//! publishing it into an rw-store.

use std::collections::BTreeSet;

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    AttributionNotice, DataOrigin, ProtocolError, PublicationGrant, SignatureAlgorithm,
    SignatureBlock, SourceProvenance, TrustedSigningKeys,
};

pub const RUN_GENERATION_REPLICATION_SCHEMA: &str = "rw.community.run-generation-replication.v1";
pub const RUN_GENERATION_FILE_SCHEMA: &str = "rw.community.run-generation-file.v1";
pub const RUN_GENERATION_CHUNK_SCHEMA_V1: &str = "rw.community.run-generation-chunk.v1";
pub const BEGIN_RUN_GENERATION_SCHEMA: &str = "rw.community.run-generation-begin.v1";
pub const RUN_GENERATION_UPLOAD_STATUS_SCHEMA: &str =
    "rw.community.run-generation-upload-status.v1";
pub const RUN_GENERATION_MISSING_PAGE_SCHEMA: &str = "rw.community.run-generation-missing-page.v1";
pub const FINALIZE_RUN_GENERATION_SCHEMA: &str = "rw.community.run-generation-finalize.v1";
pub const PUBLISHED_RUN_GENERATION_SCHEMA: &str = "rw.community.run-generation-published.v1";
pub const REVOKE_RUN_GENERATION_SCHEMA: &str = "rw.community.run-generation-revoke.v1";
pub const RUN_GENERATION_TOMBSTONE_SCHEMA: &str = "rw.community.run-generation-tombstone.v1";
pub const BEGIN_RUN_GENERATION_PATH: &str = "/v1/community/generations";
pub const RUN_GENERATION_PATH_TEMPLATE: &str = "/v1/community/generations/{generation_id}";
pub const RUN_GENERATION_MISSING_PATH_TEMPLATE: &str =
    "/v1/community/generations/{generation_id}/missing";
pub const FINALIZE_RUN_GENERATION_PATH_TEMPLATE: &str =
    "/v1/community/generations/{generation_id}/finalize";
pub const REVOKE_RUN_GENERATION_PATH_TEMPLATE: &str =
    "/v1/community/generations/{generation_id}/revoke";
pub const RUN_GENERATION_CHUNK_PATH_TEMPLATE: &str =
    "/v1/community/generations/{generation_id}/chunks/{sha256}";
pub const GET_RUN_GENERATION_OBJECT_PATH_TEMPLATE: &str =
    "/v1/community/generation-objects/{sha256}";
pub const MAX_RUN_GENERATION_MISSING_PAGE: usize = 1024;

const SIGNATURE_DOMAIN: &[u8] = b"rw-community-run-generation-signature-v1\0";
const CONTENT_ID_DOMAIN: &[u8] = b"rw-community-run-generation-content-v1\0";
const ABSOLUTE_MAX_GENERATION_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;
const ABSOLUTE_MAX_FILES: usize = 65_538;
const ABSOLUTE_MAX_CHUNKS: usize = 1_000_000;
const ABSOLUTE_MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
const ABSOLUTE_MAX_MANIFEST_BYTES: usize = 32 * 1024 * 1024;
const ABSOLUTE_MAX_RETENTION_SECONDS: i64 = 366 * 24 * 60 * 60;

/// Operator-selected replication bounds. These are safety ceilings, not a
/// storage allocation: enabling a service still requires an audited capacity
/// configuration and durable quota accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunGenerationLimits {
    pub max_generation_bytes: u64,
    pub max_files: usize,
    pub max_chunks: usize,
    pub max_chunk_bytes: u64,
    pub max_manifest_bytes: usize,
    pub max_retention_seconds: i64,
    pub max_provenance_entries: usize,
    pub max_attributions: usize,
}

impl RunGenerationLimits {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.max_generation_bytes == 0
            || self.max_generation_bytes > ABSOLUTE_MAX_GENERATION_BYTES
            || !(3..=ABSOLUTE_MAX_FILES).contains(&self.max_files)
            || self.max_chunks == 0
            || self.max_chunks > ABSOLUTE_MAX_CHUNKS
            || self.max_chunk_bytes == 0
            || self.max_chunk_bytes > ABSOLUTE_MAX_CHUNK_BYTES
            || self.max_manifest_bytes == 0
            || self.max_manifest_bytes > ABSOLUTE_MAX_MANIFEST_BYTES
            || self.max_retention_seconds <= 0
            || self.max_retention_seconds > ABSOLUTE_MAX_RETENTION_SECONDS
            || !(1..=64).contains(&self.max_provenance_entries)
            || !(1..=64).contains(&self.max_attributions)
        {
            return invalid(
                "limits",
                "replication limits are zero or exceed hard safety bounds",
            );
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunGenerationFileKind {
    RunManifest,
    Grid,
    Hour { storage_slot: u16, valid_unix: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationFileChunk {
    pub schema: String,
    pub ordinal: u32,
    pub file_offset: u64,
    pub object_sha256: String,
    pub byte_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationFile {
    pub schema: String,
    pub kind: RunGenerationFileKind,
    /// A single safe component. Its value is closed by `kind`: `run.json`,
    /// `grid.rwg`, or a bounded `.rws` filename for an hour.
    pub file_name: String,
    pub byte_size: u64,
    pub file_sha256: String,
    pub chunks: Vec<RunGenerationFileChunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationReplicationManifest {
    pub schema: String,
    pub generation_id: String,
    pub model: String,
    pub run: String,
    /// Snapshot identity observed at the publishing source. Rusty snapshot
    /// identities include the local `run.json` file identity, so this is
    /// signed provenance rather than an identity portable across filesystems.
    pub source_snapshot_id: String,
    pub grid_hash: String,
    pub owner_principal_sha256: String,
    pub publication: PublicationGrant,
    pub source_provenance: Vec<SourceProvenance>,
    pub files: Vec<RunGenerationFile>,
    pub total_bytes: u64,
    /// Canonical hash of the model/run/snapshot/grid and exact file inventory.
    pub generation_sha256: String,
    pub published_unix: i64,
    /// Exclusive publication-authorization deadline. At or after this second
    /// an origin must stop exposing the generation and durably retire its
    /// publication identity. This is an expiry, not a promise to retain or
    /// seed the bytes for a minimum custody period.
    pub retain_until_unix: i64,
    #[serde(default)]
    pub attributions: Vec<AttributionNotice>,
    #[serde(default)]
    pub modification_notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRunGenerationManifest {
    pub manifest: RunGenerationReplicationManifest,
    pub signature: SignatureBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeginRunGenerationRequest {
    pub schema: String,
    pub manifest: RunGenerationReplicationManifest,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationUploadStatus {
    pub schema: String,
    pub generation_id: String,
    pub generation_sha256: String,
    pub total_chunks: u32,
    pub missing_chunks: u32,
    pub upload_expires_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationMissingChunk {
    pub object_sha256: String,
    pub byte_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationMissingPage {
    pub schema: String,
    pub generation_id: String,
    pub chunks: Vec<RunGenerationMissingChunk>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizeRunGenerationRequest {
    pub schema: String,
    pub generation_sha256: String,
}

/// Successful publication result. `local_snapshot_id` is computed by opening
/// the atomically installed destination through `rw-query`; it intentionally
/// differs from `source_snapshot_id` when file identity changes in transit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishedRunGeneration {
    pub schema: String,
    pub generation_id: String,
    pub generation_sha256: String,
    pub source_snapshot_id: String,
    pub local_snapshot_id: String,
    pub grid_hash: String,
    pub model: String,
    pub run: String,
    pub published_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeRunGenerationRequest {
    pub schema: String,
    pub generation_sha256: String,
    pub rights_withdrawn: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationTombstone {
    pub schema: String,
    pub generation_id: String,
    pub generation_sha256: String,
    pub owner_principal_sha256: String,
    pub revoked_unix: i64,
    pub rights_withdrawn: bool,
    pub reason: String,
}

impl RunGenerationReplicationManifest {
    pub fn validate(&self, limits: &RunGenerationLimits) -> Result<(), ProtocolError> {
        limits.validate()?;
        if self.schema != RUN_GENERATION_REPLICATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("generation_id", &self.generation_id, 128, false)?;
        validate_token("model", &self.model, 96, true)?;
        validate_plain_text("run", &self.run, 128)?;
        validate_sha256("source_snapshot_id", &self.source_snapshot_id)?;
        validate_sha256("grid_hash", &self.grid_hash)?;
        validate_sha256("owner_principal_sha256", &self.owner_principal_sha256)?;
        validate_sha256("generation_sha256", &self.generation_sha256)?;
        validate_grant(&self.publication)?;
        validate_provenance(&self.source_provenance, limits)?;
        validate_notices(self, limits)?;

        if self.files.len() < 3 || self.files.len() > limits.max_files {
            return invalid(
                "files",
                "generation requires run.json, grid.rwg, and at least one hour",
            );
        }
        if self
            .files
            .windows(2)
            .any(|pair| pair[0].kind >= pair[1].kind)
        {
            return Err(ProtocolError::NonCanonical("generation files"));
        }
        let mut names = BTreeSet::new();
        let mut total_bytes = 0_u64;
        let mut total_chunks = 0_usize;
        let mut saw_manifest = false;
        let mut saw_grid = false;
        let mut saw_hour = false;
        for file in &self.files {
            if file.schema != RUN_GENERATION_FILE_SCHEMA {
                return Err(ProtocolError::UnsupportedSchema(file.schema.clone()));
            }
            validate_file_name(file)?;
            if !names.insert(&file.file_name) {
                return invalid("file_name", "duplicate generation filename");
            }
            match file.kind {
                RunGenerationFileKind::RunManifest => saw_manifest = true,
                RunGenerationFileKind::Grid => saw_grid = true,
                RunGenerationFileKind::Hour { valid_unix, .. } => {
                    if valid_unix < 0 {
                        return invalid("valid_unix", "hour time must be non-negative");
                    }
                    saw_hour = true;
                }
            }
            validate_sha256("file_sha256", &file.file_sha256)?;
            if file.byte_size == 0 {
                return invalid("file.byte_size", "file must not be empty");
            }
            let mut offset = 0_u64;
            for (ordinal, chunk) in file.chunks.iter().enumerate() {
                if chunk.schema != RUN_GENERATION_CHUNK_SCHEMA_V1 {
                    return Err(ProtocolError::UnsupportedSchema(chunk.schema.clone()));
                }
                if chunk.ordinal != ordinal as u32 || chunk.file_offset != offset {
                    return invalid(
                        "chunks",
                        "chunks must be contiguous and ordered from offset zero",
                    );
                }
                validate_sha256("chunk.object_sha256", &chunk.object_sha256)?;
                if chunk.byte_size == 0 || chunk.byte_size > limits.max_chunk_bytes {
                    return invalid(
                        "chunk.byte_size",
                        "chunk is empty or exceeds configured bound",
                    );
                }
                offset = offset
                    .checked_add(chunk.byte_size)
                    .ok_or(ProtocolError::EncodedSizeLimit)?;
                total_chunks = total_chunks
                    .checked_add(1)
                    .ok_or(ProtocolError::EncodedSizeLimit)?;
            }
            if file.chunks.is_empty() || offset != file.byte_size {
                return invalid("chunks", "chunk coverage must equal the exact file size");
            }
            total_bytes = total_bytes
                .checked_add(file.byte_size)
                .ok_or(ProtocolError::EncodedSizeLimit)?;
        }
        if !saw_manifest || !saw_grid || !saw_hour {
            return invalid("files", "closed generation file families are incomplete");
        }
        if total_chunks > limits.max_chunks
            || total_bytes > limits.max_generation_bytes
            || total_bytes != self.total_bytes
        {
            return invalid(
                "total_bytes",
                "inventory totals exceed bounds or do not match",
            );
        }
        if self.published_unix < 0
            || self.retain_until_unix <= self.published_unix
            || self.retain_until_unix - self.published_unix > limits.max_retention_seconds
        {
            return invalid(
                "retention",
                "publication retention is invalid or exceeds policy",
            );
        }
        if generation_content_sha256(self)? != self.generation_sha256 {
            return invalid(
                "generation_sha256",
                "canonical generation identity does not match",
            );
        }
        Ok(())
    }
}

impl BeginRunGenerationRequest {
    pub fn validate(&self, limits: &RunGenerationLimits) -> Result<(), ProtocolError> {
        if self.schema != BEGIN_RUN_GENERATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        self.manifest.validate(limits)
    }
}

impl RunGenerationUploadStatus {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != RUN_GENERATION_UPLOAD_STATUS_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("generation_id", &self.generation_id, 128, false)?;
        validate_sha256("generation_sha256", &self.generation_sha256)?;
        if self.total_chunks == 0
            || self.missing_chunks > self.total_chunks
            || self.upload_expires_unix < 0
        {
            return invalid("upload status", "invalid chunk count or expiration");
        }
        Ok(())
    }
}

impl RunGenerationMissingPage {
    pub fn validate(&self, limits: &RunGenerationLimits) -> Result<(), ProtocolError> {
        if self.schema != RUN_GENERATION_MISSING_PAGE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("generation_id", &self.generation_id, 128, false)?;
        if self.chunks.len() > MAX_RUN_GENERATION_MISSING_PAGE {
            return invalid("chunks", "missing-object page exceeds protocol bound");
        }
        if self
            .chunks
            .windows(2)
            .any(|pair| pair[0].object_sha256 >= pair[1].object_sha256)
        {
            return Err(ProtocolError::NonCanonical("missing chunks"));
        }
        for chunk in &self.chunks {
            validate_sha256("object_sha256", &chunk.object_sha256)?;
            if chunk.byte_size == 0 || chunk.byte_size > limits.max_chunk_bytes {
                return invalid("chunk.byte_size", "missing chunk exceeds configured bound");
            }
        }
        if let Some(cursor) = &self.next_after {
            validate_sha256("next_after", cursor)?;
            if self
                .chunks
                .last()
                .is_none_or(|chunk| chunk.object_sha256 != *cursor)
            {
                return invalid(
                    "next_after",
                    "cursor must equal the last returned object hash",
                );
            }
        }
        Ok(())
    }
}

impl FinalizeRunGenerationRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != FINALIZE_RUN_GENERATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_sha256("generation_sha256", &self.generation_sha256)
    }
}

impl PublishedRunGeneration {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != PUBLISHED_RUN_GENERATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("generation_id", &self.generation_id, 128, false)?;
        validate_sha256("generation_sha256", &self.generation_sha256)?;
        validate_sha256("source_snapshot_id", &self.source_snapshot_id)?;
        validate_sha256("local_snapshot_id", &self.local_snapshot_id)?;
        validate_sha256("grid_hash", &self.grid_hash)?;
        validate_token("model", &self.model, 96, true)?;
        validate_plain_text("run", &self.run, 128)?;
        if self.published_unix < 0 {
            return invalid("published_unix", "publication time must be non-negative");
        }
        Ok(())
    }
}

impl RevokeRunGenerationRequest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != REVOKE_RUN_GENERATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_sha256("generation_sha256", &self.generation_sha256)?;
        if !self.rights_withdrawn {
            return invalid(
                "rights_withdrawn",
                "revocation must be explicitly confirmed",
            );
        }
        validate_plain_text("reason", &self.reason, 1024)
    }
}

impl RunGenerationTombstone {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.schema != RUN_GENERATION_TOMBSTONE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("generation_id", &self.generation_id, 128, false)?;
        validate_sha256("generation_sha256", &self.generation_sha256)?;
        validate_sha256("owner_principal_sha256", &self.owner_principal_sha256)?;
        if self.revoked_unix < 0 || !self.rights_withdrawn {
            return invalid("tombstone", "invalid revocation time or confirmation");
        }
        validate_plain_text("reason", &self.reason, 1024)
    }
}

pub fn parse_signed_run_generation_bounded(
    bytes: &[u8],
    limits: &RunGenerationLimits,
) -> Result<SignedRunGenerationManifest, ProtocolError> {
    limits.validate()?;
    if bytes.is_empty() || bytes.len() > limits.max_manifest_bytes {
        return Err(ProtocolError::ManifestSizeLimit);
    }
    let signed: SignedRunGenerationManifest =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    signed.manifest.validate(limits)?;
    Ok(signed)
}

pub fn generation_content_sha256(
    manifest: &RunGenerationReplicationManifest,
) -> Result<String, ProtocolError> {
    let mut out = Vec::with_capacity(4096);
    out.extend_from_slice(CONTENT_ID_DOMAIN);
    put_str(&mut out, &manifest.model);
    put_str(&mut out, &manifest.run);
    put_str(&mut out, &manifest.source_snapshot_id);
    put_str(&mut out, &manifest.grid_hash);
    put_u32(&mut out, checked_len(manifest.files.len())?);
    for file in &manifest.files {
        encode_file(&mut out, file)?;
    }
    Ok(hex_sha256(&out))
}

pub fn canonical_run_generation_bytes(
    manifest: &RunGenerationReplicationManifest,
    signing_key_id: &str,
    limits: &RunGenerationLimits,
) -> Result<Vec<u8>, ProtocolError> {
    manifest.validate(limits)?;
    validate_token("signing_key_id", signing_key_id, 128, false)?;
    let mut out = Vec::with_capacity(8192);
    out.extend_from_slice(SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    put_str(&mut out, &manifest.schema);
    put_str(&mut out, &manifest.generation_id);
    put_str(&mut out, &manifest.model);
    put_str(&mut out, &manifest.run);
    put_str(&mut out, &manifest.source_snapshot_id);
    put_str(&mut out, &manifest.grid_hash);
    put_str(&mut out, &manifest.owner_principal_sha256);
    put_u8(&mut out, data_origin_tag(manifest.publication.data_origin));
    put_bool(&mut out, manifest.publication.explicit_owner_publication);
    put_bool(
        &mut out,
        manifest.publication.redistribution_rights_confirmed,
    );
    put_u32(&mut out, checked_len(manifest.source_provenance.len())?);
    for source in &manifest.source_provenance {
        put_str(&mut out, &source.provider);
        put_strings(&mut out, &source.roles)?;
        put_strings(&mut out, &source.products)?;
    }
    put_u32(&mut out, checked_len(manifest.files.len())?);
    for file in &manifest.files {
        encode_file(&mut out, file)?;
    }
    put_u64(&mut out, manifest.total_bytes);
    put_str(&mut out, &manifest.generation_sha256);
    put_i64(&mut out, manifest.published_unix);
    put_i64(&mut out, manifest.retain_until_unix);
    put_u32(&mut out, checked_len(manifest.attributions.len())?);
    for notice in &manifest.attributions {
        put_str(&mut out, &notice.provider);
        put_str(&mut out, &notice.notice);
        put_str(&mut out, &notice.source_url);
        put_str(&mut out, &notice.license);
        put_str(&mut out, &notice.license_url);
        put_str(&mut out, &notice.terms_url);
        put_str(&mut out, &notice.disclaimer);
    }
    put_strings(&mut out, &manifest.modification_notices)?;
    Ok(out)
}

pub fn sign_run_generation(
    manifest: RunGenerationReplicationManifest,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
    limits: &RunGenerationLimits,
) -> Result<SignedRunGenerationManifest, ProtocolError> {
    let signing_key_id = signing_key_id.into();
    let preimage = canonical_run_generation_bytes(&manifest, &signing_key_id, limits)?;
    let signature = signing_key.sign(&preimage);
    Ok(SignedRunGenerationManifest {
        manifest,
        signature: SignatureBlock {
            algorithm: SignatureAlgorithm::Ed25519,
            signing_key_id,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    })
}

pub fn verify_signed_run_generation(
    signed: &SignedRunGenerationManifest,
    now_unix: i64,
    trusted_keys: &TrustedSigningKeys,
    limits: &RunGenerationLimits,
) -> Result<(), ProtocolError> {
    signed.manifest.validate(limits)?;
    if now_unix < signed.manifest.published_unix.saturating_sub(300)
        || now_unix >= signed.manifest.retain_until_unix
    {
        return Err(ProtocolError::ManifestExpired);
    }
    let key = trusted_keys
        .get(&signed.signature.signing_key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(signed.signature.signing_key_id.clone()))?;
    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature.signature_base64)
        .map_err(|_| ProtocolError::MalformedSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ProtocolError::MalformedSignature)?;
    let preimage =
        canonical_run_generation_bytes(&signed.manifest, &signed.signature.signing_key_id, limits)?;
    key.verify(&preimage, &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

pub fn verify_run_generation_chunk(
    chunk: &RunGenerationFileChunk,
    bytes: &[u8],
    limits: &RunGenerationLimits,
) -> Result<(), ProtocolError> {
    limits.validate()?;
    if bytes.len() as u64 != chunk.byte_size {
        return Err(ProtocolError::EncodedSizeMismatch);
    }
    if chunk.byte_size == 0 || chunk.byte_size > limits.max_chunk_bytes {
        return Err(ProtocolError::EncodedSizeLimit);
    }
    if hex_sha256(bytes) != chunk.object_sha256 {
        return Err(ProtocolError::ObjectHashMismatch);
    }
    Ok(())
}

fn validate_file_name(file: &RunGenerationFile) -> Result<(), ProtocolError> {
    let expected = match file.kind {
        RunGenerationFileKind::RunManifest => Some("run.json"),
        RunGenerationFileKind::Grid => Some("grid.rwg"),
        RunGenerationFileKind::Hour { .. } => None,
    };
    if let Some(expected) = expected {
        if file.file_name != expected {
            return invalid(
                "file_name",
                "metadata filename does not match its closed kind",
            );
        }
    } else if file.file_name.len() > 128
        || !file.file_name.ends_with(".rws")
        || file.file_name.starts_with('.')
        || !file
            .file_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return invalid("file_name", "hour filename must be one safe .rws component");
    }
    Ok(())
}

fn validate_grant(grant: &PublicationGrant) -> Result<(), ProtocolError> {
    if !grant.redistribution_rights_confirmed {
        return Err(ProtocolError::RedistributionRightsUnconfirmed);
    }
    // Every replication upload is client-authored. The authoritative signer
    // can attest the exact owner-bound immutable generation it validated, but
    // it cannot promote free-form client provenance into a native
    // public-provider identity. Public NOAA/ECMWF lineage remains in notices
    // and provenance under an explicit UserProvided owner publication.
    if grant.data_origin == DataOrigin::PublicProvider || !grant.explicit_owner_publication {
        return Err(ProtocolError::PrivatePublicationDenied);
    }
    Ok(())
}

fn validate_provenance(
    provenance: &[SourceProvenance],
    limits: &RunGenerationLimits,
) -> Result<(), ProtocolError> {
    if provenance.is_empty() || provenance.len() > limits.max_provenance_entries {
        return invalid("source_provenance", "must be bounded and non-empty");
    }
    for source in provenance {
        validate_token("source.provider", &source.provider, 96, true)?;
        if source.roles.is_empty() || source.roles.len() > 64 || source.products.len() > 128 {
            return invalid("source_provenance", "role or product collection is invalid");
        }
        for role in &source.roles {
            validate_token("source.role", role, 96, true)?;
        }
        for product in &source.products {
            validate_token("source.product", product, 128, true)?;
        }
        if source.roles.windows(2).any(|pair| pair[0] >= pair[1])
            || source.products.windows(2).any(|pair| pair[0] >= pair[1])
        {
            return Err(ProtocolError::NonCanonical("source labels"));
        }
    }
    if provenance.windows(2).any(|pair| {
        (&pair[0].provider, &pair[0].roles, &pair[0].products)
            >= (&pair[1].provider, &pair[1].roles, &pair[1].products)
    }) {
        return Err(ProtocolError::NonCanonical("source provenance"));
    }
    Ok(())
}

fn validate_notices(
    manifest: &RunGenerationReplicationManifest,
    limits: &RunGenerationLimits,
) -> Result<(), ProtocolError> {
    if manifest.attributions.len() > limits.max_attributions
        || manifest.modification_notices.len() > 32
    {
        return invalid("notices", "notice collection exceeds configured bounds");
    }
    for notice in &manifest.attributions {
        validate_token("attribution.provider", &notice.provider, 96, true)?;
        validate_plain_text("attribution.notice", &notice.notice, 2048)?;
        validate_plain_text("attribution.license", &notice.license, 512)?;
        validate_plain_text("attribution.disclaimer", &notice.disclaimer, 2048)?;
        for url in [&notice.source_url, &notice.license_url, &notice.terms_url] {
            if !url.starts_with("https://") || url.len() > 2048 || url.chars().any(char::is_control)
            {
                return invalid("attribution_url", "must be bounded HTTPS");
            }
        }
    }
    for notice in &manifest.modification_notices {
        validate_plain_text("modification_notice", notice, 2048)?;
    }
    if manifest.publication.data_origin != DataOrigin::PublicProvider
        && manifest.attributions.is_empty()
    {
        return invalid(
            "attributions",
            "owner-published generations require license attribution",
        );
    }
    if manifest
        .source_provenance
        .iter()
        .any(|source| source.provider == "ecmwf-open-data")
        && (manifest.attributions.iter().all(|notice| {
            notice.provider != "ecmwf-open-data" || !notice.license.contains("CC BY 4.0")
        }) || manifest
            .modification_notices
            .iter()
            .all(|notice| notice.trim().is_empty()))
    {
        return Err(ProtocolError::MissingEcmwfNotice);
    }
    Ok(())
}

fn encode_file(out: &mut Vec<u8>, file: &RunGenerationFile) -> Result<(), ProtocolError> {
    put_str(out, &file.schema);
    match file.kind {
        RunGenerationFileKind::RunManifest => put_u8(out, 0),
        RunGenerationFileKind::Grid => put_u8(out, 1),
        RunGenerationFileKind::Hour {
            storage_slot,
            valid_unix,
        } => {
            put_u8(out, 2);
            out.extend_from_slice(&storage_slot.to_be_bytes());
            put_i64(out, valid_unix);
        }
    }
    put_str(out, &file.file_name);
    put_u64(out, file.byte_size);
    put_str(out, &file.file_sha256);
    put_u32(out, checked_len(file.chunks.len())?);
    for chunk in &file.chunks {
        put_str(out, &chunk.schema);
        put_u32(out, chunk.ordinal);
        put_u64(out, chunk.file_offset);
        put_str(out, &chunk.object_sha256);
        put_u64(out, chunk.byte_size);
    }
    Ok(())
}

fn data_origin_tag(value: DataOrigin) -> u8 {
    match value {
        DataOrigin::PublicProvider => 0,
        DataOrigin::PrivateWrf => 1,
        DataOrigin::PrivateArwen => 2,
        DataOrigin::UserProvided => 3,
    }
}

fn validate_plain_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    let trimmed = value.trim();
    if trimmed.is_empty()
        || trimmed != value
        || value.len() > maximum
        || value.chars().any(|ch| ch == '\0' || ch.is_control())
        || value.contains(['<', '>'])
    {
        return invalid(field, "must be bounded plain text");
    }
    Ok(())
}

fn validate_token(
    field: &'static str,
    value: &str,
    maximum: usize,
    allow_dot: bool,
) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_')
                || (allow_dot && byte == b'.')
        })
    {
        return invalid(field, "contains invalid or unbounded identifier text");
    }
    Ok(())
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        invalid(field, "must be lowercase SHA-256 hex")
    }
}

fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}

fn put_bool(out: &mut Vec<u8>, value: bool) {
    put_u8(out, u8::from(value));
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}

fn put_strings(out: &mut Vec<u8>, values: &[String]) -> Result<(), ProtocolError> {
    put_u32(out, checked_len(values.len())?);
    for value in values {
        put_str(out, value);
    }
    Ok(())
}

fn checked_len(value: usize) -> Result<u32, ProtocolError> {
    u32::try_from(value).map_err(|_| ProtocolError::ManifestSizeLimit)
}

fn hex_sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn invalid<T>(field: &'static str, reason: impl Into<String>) -> Result<T, ProtocolError> {
    Err(ProtocolError::InvalidField {
        field,
        reason: reason.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::VerifyingKey;

    fn hash(byte: u8) -> String {
        format!("{byte:02x}").repeat(32)
    }

    fn limits() -> RunGenerationLimits {
        RunGenerationLimits {
            max_generation_bytes: 1024 * 1024,
            max_files: 32,
            max_chunks: 128,
            max_chunk_bytes: 1024,
            max_manifest_bytes: 1024 * 1024,
            max_retention_seconds: 86_400,
            max_provenance_entries: 8,
            max_attributions: 8,
        }
    }

    fn chunk(ordinal: u32, offset: u64, bytes: &[u8]) -> RunGenerationFileChunk {
        RunGenerationFileChunk {
            schema: RUN_GENERATION_CHUNK_SCHEMA_V1.into(),
            ordinal,
            file_offset: offset,
            object_sha256: hex_sha256(bytes),
            byte_size: bytes.len() as u64,
        }
    }

    fn file(kind: RunGenerationFileKind, name: &str, bytes: &[u8]) -> RunGenerationFile {
        RunGenerationFile {
            schema: RUN_GENERATION_FILE_SCHEMA.into(),
            kind,
            file_name: name.into(),
            byte_size: bytes.len() as u64,
            file_sha256: hex_sha256(bytes),
            chunks: vec![chunk(0, 0, bytes)],
        }
    }

    fn manifest(origin: DataOrigin) -> RunGenerationReplicationManifest {
        let files = vec![
            file(RunGenerationFileKind::RunManifest, "run.json", b"run"),
            file(RunGenerationFileKind::Grid, "grid.rwg", b"grid"),
            file(
                RunGenerationFileKind::Hour {
                    storage_slot: 0,
                    valid_unix: 1_800_000_000,
                },
                "f000.rws",
                b"hour",
            ),
        ];
        let mut manifest = RunGenerationReplicationManifest {
            schema: RUN_GENERATION_REPLICATION_SCHEMA.into(),
            generation_id: "wrf-case-a-generation-1".into(),
            model: "wrf".into(),
            run: "20260812_00z".into(),
            source_snapshot_id: hash(0xaa),
            grid_hash: hash(0xbb),
            owner_principal_sha256: hash(0xcc),
            publication: PublicationGrant {
                data_origin: origin,
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
            generation_sha256: hash(0xdd),
            published_unix: 1_800_000_000,
            retain_until_unix: 1_800_086_400,
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
        manifest
    }

    #[test]
    fn signed_inventory_is_canonical_and_tamper_evident() {
        let limits = limits();
        let key = SigningKey::from_bytes(&[17; 32]);
        let signed =
            sign_run_generation(manifest(DataOrigin::PrivateWrf), "origin-v1", &key, &limits)
                .unwrap();
        let mut trusted = TrustedSigningKeys::new();
        trusted.insert(
            "origin-v1".into(),
            VerifyingKey::from_bytes(&key.verifying_key().to_bytes()).unwrap(),
        );
        verify_signed_run_generation(&signed, 1_800_000_100, &trusted, &limits).unwrap();
        verify_signed_run_generation(&signed, 1_800_086_399, &trusted, &limits).unwrap();
        assert_eq!(
            verify_signed_run_generation(&signed, 1_800_086_400, &trusted, &limits),
            Err(ProtocolError::ManifestExpired)
        );
        let encoded = serde_json::to_vec(&signed).unwrap();
        let parsed = parse_signed_run_generation_bounded(&encoded, &limits).unwrap();
        assert_eq!(parsed, signed);

        let mut tampered = signed;
        tampered.manifest.files[2].chunks[0].object_sha256 = hash(0xee);
        assert!(verify_signed_run_generation(&tampered, 1_800_000_100, &trusted, &limits).is_err());
    }

    #[test]
    fn closed_files_chunks_rights_and_retention_fail_closed() {
        let limits = limits();
        let valid = manifest(DataOrigin::PrivateArwen);
        valid.validate(&limits).unwrap();
        verify_run_generation_chunk(&valid.files[2].chunks[0], b"hour", &limits).unwrap();

        let mut arbitrary = valid.clone();
        arbitrary.files[2].file_name = "private/wrfout_d01".into();
        assert!(arbitrary.validate(&limits).is_err());

        let mut no_rights = valid.clone();
        no_rights.publication.redistribution_rights_confirmed = false;
        assert_eq!(
            no_rights.validate(&limits),
            Err(ProtocolError::RedistributionRightsUnconfirmed)
        );

        let mut no_owner_action = valid.clone();
        no_owner_action.publication.explicit_owner_publication = false;
        assert_eq!(
            no_owner_action.validate(&limits),
            Err(ProtocolError::PrivatePublicationDenied)
        );

        let mut gap = valid.clone();
        gap.files[2].chunks[0].file_offset = 1;
        assert!(gap.validate(&limits).is_err());

        let mut too_long = valid;
        too_long.retain_until_unix += 1;
        assert!(too_long.validate(&limits).is_err());

        let mut relabeled_public = manifest(DataOrigin::PublicProvider);
        relabeled_public.model = "hrrr".into();
        relabeled_public.source_provenance = vec![SourceProvenance {
            provider: "noaa-aws-public-data".into(),
            roles: vec!["surface".into()],
            products: vec!["wrfsfcf".into()],
        }];
        assert_eq!(
            relabeled_public.validate(&limits),
            Err(ProtocolError::PrivatePublicationDenied)
        );
    }

    #[test]
    fn ecmwf_replication_requires_attribution_and_modification_notice() {
        let mut value = manifest(DataOrigin::UserProvided);
        value.source_provenance = vec![SourceProvenance {
            provider: "ecmwf-open-data".into(),
            roles: vec!["source".into()],
            products: vec!["ifs".into()],
        }];
        value.attributions = vec![AttributionNotice {
            provider: "not-ecmwf".into(),
            notice: "Owner publication attribution remains present.".into(),
            source_url: "https://example.invalid/source".into(),
            license: "Owner-authorized redistribution".into(),
            license_url: "https://example.invalid/license".into(),
            terms_url: "https://example.invalid/terms".into(),
            disclaimer: "Experimental publication.".into(),
        }];
        assert_eq!(
            value.validate(&limits()),
            Err(ProtocolError::MissingEcmwfNotice)
        );
    }

    #[test]
    fn upload_control_contracts_are_bounded_and_exact() {
        let limits = limits();
        let manifest = manifest(DataOrigin::PrivateWrf);
        BeginRunGenerationRequest {
            schema: BEGIN_RUN_GENERATION_SCHEMA.into(),
            manifest: manifest.clone(),
        }
        .validate(&limits)
        .unwrap();
        let mut missing: Vec<_> = manifest
            .files
            .iter()
            .flat_map(|file| file.chunks.iter())
            .map(|chunk| RunGenerationMissingChunk {
                object_sha256: chunk.object_sha256.clone(),
                byte_size: chunk.byte_size,
            })
            .collect();
        missing.sort_by(|left, right| left.object_sha256.cmp(&right.object_sha256));
        missing.dedup_by(|left, right| left.object_sha256 == right.object_sha256);
        RunGenerationMissingPage {
            schema: RUN_GENERATION_MISSING_PAGE_SCHEMA.into(),
            generation_id: manifest.generation_id.clone(),
            next_after: missing.last().map(|chunk| chunk.object_sha256.clone()),
            chunks: missing,
        }
        .validate(&limits)
        .unwrap();
        RunGenerationUploadStatus {
            schema: RUN_GENERATION_UPLOAD_STATUS_SCHEMA.into(),
            generation_id: manifest.generation_id.clone(),
            generation_sha256: manifest.generation_sha256.clone(),
            total_chunks: 3,
            missing_chunks: 3,
            upload_expires_unix: 1_800_001_000,
        }
        .validate()
        .unwrap();
        FinalizeRunGenerationRequest {
            schema: FINALIZE_RUN_GENERATION_SCHEMA.into(),
            generation_sha256: manifest.generation_sha256.clone(),
        }
        .validate()
        .unwrap();
        PublishedRunGeneration {
            schema: PUBLISHED_RUN_GENERATION_SCHEMA.into(),
            generation_id: manifest.generation_id.clone(),
            generation_sha256: manifest.generation_sha256.clone(),
            source_snapshot_id: manifest.source_snapshot_id.clone(),
            local_snapshot_id: hash(0xee),
            grid_hash: manifest.grid_hash.clone(),
            model: manifest.model.clone(),
            run: manifest.run.clone(),
            published_unix: 1_800_000_100,
        }
        .validate()
        .unwrap();
        let mut revoke = RevokeRunGenerationRequest {
            schema: REVOKE_RUN_GENERATION_SCHEMA.into(),
            generation_sha256: manifest.generation_sha256,
            rights_withdrawn: true,
            reason: "Owner withdrew redistribution rights.".into(),
        };
        revoke.validate().unwrap();
        revoke.rights_withdrawn = false;
        assert!(revoke.validate().is_err());
    }
}

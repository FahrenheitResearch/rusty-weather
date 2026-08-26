//! Relay control-plane policy and rendezvous implementation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use base64::Engine as _;
use ed25519_dalek::{Signature, SigningKey, Verifier};
use rand_core::{OsRng, RngCore};
use rw_community_protocol::{
    CaseArtifactType, ProtocolError, ProtocolLimits, RelayCandidate, RelayCandidateKind,
    RelayCredentialClaims, RelayDirection, ShareQuery, SignedObjectManifest, SignedRelayCredential,
    TrustedSigningKeys, canonical_object_manifest_bytes, request_sha256, sign_relay_credential,
    validate_object_manifest, verify_signed_relay_credential,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    EphemeralPublicOffer, FallbackTarget, ProviderCredentialLease, ProviderCredentialRequest,
    ProviderRelayAccess, PublicRelayFailure, RelayError, RelayProvider, RelayRole, SecretText,
    SignedSessionBinding, build_session_binding, credential_fingerprint, sign_session_binding,
    valid_opaque_id, valid_sha256,
};

pub const ADVERTISEMENT_SCHEMA: &str = "rw.community.relay-advertisement.v1";
pub const AUDIT_EVENT_SCHEMA: &str = "rw.community.relay-audit.v1";
pub const PROMOTION_SCHEMA: &str = "rw.community.relay-promotion.v1";
const PERSISTENCE_SCHEMA: &str = "rw.community.relay-state.v2";
const MAX_PERSISTENCE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PERSISTED_SUBJECTS: usize = 100_000;
const MAX_PERSISTED_ADVERTISEMENTS: usize = 250_000;
const MAX_PERSISTED_SESSIONS: usize = 100_000;
const MAX_PERSISTED_REVOCATIONS: usize = 250_000;
const MAX_PERSISTED_POPULARITY: usize = 250_000;
/// Stop-and-wait v1 is deliberately limited to small profile/point products.
/// Native windows, temporal grids, and case artifacts fall through to
/// archival HTTPS until a bounded authenticated sliding-window transport is
/// implemented and release-tested.
pub const INITIAL_RELAY_OBJECT_BYTES: u64 = 64 * 1024;

/// Closed v1 categories. There is intentionally no blob, filename, directory,
/// raw WRF output, or full-run category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayObjectCategory {
    Profile,
    PointSeries,
    NativeWindow,
    TemporalGrid,
    CaseArtifact,
}

impl RelayObjectCategory {
    fn from_query(query: &ShareQuery) -> Self {
        match query {
            ShareQuery::Profile { .. } => Self::Profile,
            ShareQuery::PointSeries { .. } => Self::PointSeries,
            ShareQuery::NativeWindow { .. } | ShareQuery::GeographicWindow { .. } => {
                Self::NativeWindow
            }
            ShareQuery::TemporalGrid { .. } => Self::TemporalGrid,
            ShareQuery::CaseArtifact { .. } => Self::CaseArtifact,
        }
    }
}

/// Origin-authenticated identity that may be advertised. The object bytes are
/// still untrusted until the downloader performs the protocol's complete body,
/// hash, bounded-decode, schema, and attribution verification.
#[derive(Clone, PartialEq, Eq)]
pub struct VerifiedRelayObject {
    object_sha256: String,
    request_sha256: String,
    encoded_size: u64,
    expires_unix: i64,
    category: RelayObjectCategory,
}

impl fmt::Debug for VerifiedRelayObject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VerifiedRelayObject")
            .field("object_sha256", &self.object_sha256)
            .field("request_sha256", &self.request_sha256)
            .field("encoded_size", &self.encoded_size)
            .field("expires_unix", &self.expires_unix)
            .field("category", &self.category)
            .finish()
    }
}

impl VerifiedRelayObject {
    pub fn object_sha256(&self) -> &str {
        &self.object_sha256
    }

    pub fn request_sha256(&self) -> &str {
        &self.request_sha256
    }

    pub const fn encoded_size(&self) -> u64 {
        self.encoded_size
    }

    pub const fn category(&self) -> RelayObjectCategory {
        self.category
    }
}

/// Verify just the origin-signed immutable identity for rendezvous admission.
/// A seed need not upload bytes to advertise availability; the receiver later
/// verifies those hostile bytes against the same manifest.
pub fn verify_origin_signed_identity(
    signed: &SignedObjectManifest,
    now_unix: i64,
    trusted_origin_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<VerifiedRelayObject, RelayError> {
    let manifest = &signed.manifest;
    validate_object_manifest(manifest, limits).map_err(|_| RelayError::UntrustedObject)?;
    if now_unix < manifest.created_unix.saturating_sub(300) || now_unix >= manifest.expires_unix {
        return Err(RelayError::UntrustedObject);
    }
    if request_sha256(&manifest.request).map_err(|_| RelayError::UntrustedObject)?
        != manifest.request_sha256
    {
        return Err(RelayError::UntrustedObject);
    }
    if manifest.content_type == "image/png"
        && !matches!(
            manifest.request.query,
            ShareQuery::CaseArtifact {
                artifact_type: CaseArtifactType::RenderedImage,
                ..
            }
        )
    {
        return Err(RelayError::UntrustedObject);
    }
    let verifying_key = trusted_origin_keys
        .get(&signed.signature.signing_key_id)
        .ok_or(RelayError::UntrustedObject)?;
    let signature = base64::engine::general_purpose::STANDARD
        .decode(&signed.signature.signature_base64)
        .ok()
        .and_then(|bytes| Signature::from_slice(&bytes).ok())
        .ok_or(RelayError::UntrustedObject)?;
    let bytes = canonical_object_manifest_bytes(manifest, &signed.signature.signing_key_id)
        .map_err(|_| RelayError::UntrustedObject)?;
    verifying_key
        .verify(&bytes, &signature)
        .map_err(|_| RelayError::UntrustedObject)?;
    Ok(VerifiedRelayObject {
        object_sha256: manifest.object_sha256.clone(),
        request_sha256: manifest.request_sha256.clone(),
        encoded_size: manifest.encoded_size,
        expires_unix: manifest.expires_unix,
        category: RelayObjectCategory::from_query(&manifest.request.query),
    })
}

/// Backend-only authenticated subject. It is never Serialize and Debug is
/// redacted, so app-visible state cannot accidentally reveal an account name.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AuthenticatedSubject(String);

impl AuthenticatedSubject {
    /// Derive the durable relay-accounting identity from an authenticated
    /// backend principal. The caller may pass the server's stable token digest
    /// (or another private principal), but the raw value is never retained.
    pub fn new(internal_id: impl Into<String>) -> Result<Self, RelayError> {
        let internal_id = internal_id.into();
        if internal_id.is_empty()
            || internal_id.len() > 256
            || internal_id.chars().any(char::is_control)
        {
            return Err(RelayError::UnsafeIdentifier);
        }
        let mut digest = Sha256::new();
        digest.update(b"rw-community-relay-authenticated-subject-v1\0");
        digest.update(internal_id.as_bytes());
        Ok(Self(format!("{:x}", digest.finalize())))
    }

    fn from_persisted_digest(value: String) -> Result<Self, RelayError> {
        if !valid_sha256(&value) {
            return Err(RelayError::PersistenceRejected);
        }
        Ok(Self(value))
    }

    /// Server-side dispatch/persistence only. Never copy this value into a DTO,
    /// audit event, provider custom identifier, error, or credential.
    pub fn expose_for_backend_dispatch(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AuthenticatedSubject {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthenticatedSubject([redacted])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BillingPeriod {
    pub year: u16,
    pub month: u8,
}

impl BillingPeriod {
    pub fn new(year: u16, month: u8) -> Result<Self, RelayError> {
        if !(2020..=9999).contains(&year) || !(1..=12).contains(&month) {
            return Err(RelayError::PolicyDenied);
        }
        Ok(Self { year, month })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSeedingPolicy {
    pub opted_in: bool,
    pub categories: BTreeSet<RelayObjectCategory>,
    pub disk_allowance_bytes: u64,
    pub upload_allowance_bytes: u64,
    pub metered_network: bool,
    pub allow_metered_seeding: bool,
}

impl ClientSeedingPolicy {
    fn permits(&self, object: &VerifiedRelayObject) -> Result<(), RelayError> {
        if !self.opted_in
            || !self.categories.contains(&object.category)
            || !matches!(
                object.category,
                RelayObjectCategory::Profile | RelayObjectCategory::PointSeries
            )
            || object.encoded_size > INITIAL_RELAY_OBJECT_BYTES
            || object.encoded_size > self.disk_allowance_bytes
            || object.encoded_size > self.upload_allowance_bytes
        {
            return Err(RelayError::PolicyDenied);
        }
        if self.metered_network && !self.allow_metered_seeding {
            return Err(RelayError::MeteredNetworkPaused);
        }
        Ok(())
    }
}

/// Per-requester opt-in and local download allowance. The default is off, so
/// a passive model query can never become a community lookup implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClientRetrievalPolicy {
    pub opted_in: bool,
    pub download_allowance_bytes: u64,
}

impl ClientRetrievalPolicy {
    fn permits(self, encoded_size: u64) -> Result<(), RelayError> {
        if !self.opted_in
            || self.download_allowance_bytes == 0
            || encoded_size > self.download_allowance_bytes
        {
            return Err(RelayError::PolicyDenied);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayQuotaPolicy {
    pub per_user_upload_bytes_per_month: u64,
    pub per_user_download_bytes_per_month: u64,
    pub per_user_advertised_storage_bytes: u64,
    pub per_user_concurrency: u32,
    pub global_concurrency: u32,
    pub global_relay_bytes_per_month: u64,
    /// Operator-supplied traffic/cost stop. This is bytes rather than compiled
    /// provider pricing and may be lower than the global quota.
    pub cost_stop_after_bytes_per_month: u64,
}

impl RelayQuotaPolicy {
    fn validate(self) -> Result<(), RelayError> {
        if self.per_user_upload_bytes_per_month == 0
            || self.per_user_download_bytes_per_month == 0
            || self.per_user_advertised_storage_bytes == 0
            || self.per_user_concurrency == 0
            || self.global_concurrency == 0
            || self.global_relay_bytes_per_month == 0
            || self.cost_stop_after_bytes_per_month == 0
            || self.cost_stop_after_bytes_per_month > self.global_relay_bytes_per_month
        {
            return Err(RelayError::PolicyDenied);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromotionPolicy {
    pub successful_recoveries: u64,
    pub relayed_bytes: u64,
}

impl PromotionPolicy {
    fn validate(self) -> Result<(), RelayError> {
        if self.successful_recoveries == 0 || self.relayed_bytes == 0 {
            return Err(RelayError::PolicyDenied);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayControlConfig {
    pub phase2_enabled: bool,
    pub security_tests_passed: bool,
    pub capacity_audit_complete: bool,
    pub provider_pricing_verified: bool,
    pub relay_id: String,
    pub signing_key_id: String,
    pub credential_lifetime_seconds: i64,
    pub max_chunk_plaintext_bytes: u32,
    pub quotas: RelayQuotaPolicy,
    pub promotion: PromotionPolicy,
}

impl RelayControlConfig {
    fn validate(&self, limits: &ProtocolLimits) -> Result<(), RelayError> {
        if !valid_opaque_id(&self.relay_id)
            || !valid_opaque_id(&self.signing_key_id)
            || !(1..=15 * 60).contains(&self.credential_lifetime_seconds)
            || self.max_chunk_plaintext_bytes != crate::RELAY_PLAINTEXT_CHUNK_BYTES
            || u64::from(self.max_chunk_plaintext_bytes) > limits.max_encoded_bytes
        {
            return Err(RelayError::PolicyDenied);
        }
        self.quotas.validate()?;
        self.promotion.validate()
    }

    fn enabled(&self) -> Result<(), RelayError> {
        if !self.phase2_enabled {
            return Err(RelayError::Disabled);
        }
        if !self.security_tests_passed
            || !self.capacity_audit_complete
            || !self.provider_pricing_verified
        {
            return Err(RelayError::SecurityGate);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpaqueIdKind {
    Advertisement,
    Session,
    Ticket,
    ParticipantAlias,
}

pub trait OpaqueIdSource {
    fn next_id(&mut self, kind: OpaqueIdKind) -> Result<String, RelayError>;
}

#[derive(Debug, Default)]
pub struct OsOpaqueIdSource;

impl OpaqueIdSource for OsOpaqueIdSource {
    fn next_id(&mut self, kind: OpaqueIdKind) -> Result<String, RelayError> {
        let prefix = match kind {
            OpaqueIdKind::Advertisement => "advert",
            OpaqueIdKind::Session => "session",
            OpaqueIdKind::Ticket => "ticket",
            OpaqueIdKind::ParticipantAlias => "subject",
        };
        let mut random = [0_u8; 24];
        OsRng.fill_bytes(&mut random);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);
        let id = format!("{prefix}-{encoded}");
        if !valid_opaque_id(&id) {
            return Err(RelayError::UnsafeIdentifier);
        }
        Ok(id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvertisementReceipt {
    pub schema: String,
    pub advertisement_id: String,
    pub object_sha256: String,
    pub expires_unix: i64,
}

pub struct ParticipantRelayGrant {
    pub candidate: RelayCandidate,
    pub credential: SignedRelayCredential,
    pub provider_access: ProviderRelayAccess,
}

impl fmt::Debug for ParticipantRelayGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ParticipantRelayGrant([redacted])")
    }
}

/// Backend dispatch result. It cannot be serialized. The authenticated server
/// sends each participant only its own grant and never the other subject.
pub struct RelaySessionGrant {
    pub session_id: String,
    pub object_sha256: String,
    pub encoded_size: u64,
    pub upload: ParticipantRelayGrant,
    pub download: ParticipantRelayGrant,
    seed_dispatch_subject: AuthenticatedSubject,
}

impl fmt::Debug for RelaySessionGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelaySessionGrant([opaque relay session])")
    }
}

impl RelaySessionGrant {
    pub fn seed_dispatch_subject(&self) -> &AuthenticatedSubject {
        &self.seed_dispatch_subject
    }
}

/// Backend-only proof that an authenticated principal owns one role in an
/// active session. It is intentionally non-serializable and redacted so route
/// registration and participant-specific grant delivery cannot be authorized
/// by a session ID or signed credential alone.
pub struct AuthorizedRelayParticipant {
    pub(crate) session_id: String,
    pub(crate) object_sha256: String,
    pub(crate) role: RelayRole,
    pub(crate) credential_fingerprint: String,
    pub(crate) expires_unix: i64,
}

impl fmt::Debug for AuthorizedRelayParticipant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizedRelayParticipant([redacted])")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayAuditKind {
    AdvertisementAccepted,
    SessionIssued,
    SessionCompleted,
    SessionFailed,
    SessionRevoked,
    PromotionRequested,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAuditEvent {
    pub schema: String,
    pub kind: RelayAuditKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromotionSignal {
    pub schema: String,
    pub object_sha256: String,
    pub successful_recoveries: u64,
    pub relayed_bytes: u64,
}

#[derive(Debug)]
pub struct CompletionResult {
    pub audit: RelayAuditEvent,
    pub promotion: Option<PromotionSignal>,
}

#[derive(Debug)]
pub enum ParticipantCompletionResult {
    AwaitingCounterpart,
    Complete(CompletionResult),
}

pub enum ColdLookupOutcome {
    Relay(Box<RelaySessionGrant>),
    Fallback(PublicRelayFailure),
}

impl fmt::Debug for ColdLookupOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Relay(_) => formatter.write_str("ColdLookupOutcome::Relay([redacted])"),
            Self::Fallback(value) => formatter.debug_tuple("Fallback").field(value).finish(),
        }
    }
}

#[derive(Clone)]
struct SeedEntry {
    subject: AuthenticatedSubject,
    advertisement_id: String,
    object: VerifiedRelayObject,
    policy: ClientSeedingPolicy,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubjectUsage {
    uploaded: u64,
    downloaded: u64,
    reserved_upload: u64,
    reserved_download: u64,
    advertised_storage: u64,
    active_uploads: u32,
    active_downloads: u32,
}

struct PendingSession {
    object_sha256: String,
    expected_bytes: u64,
    upload_subject: AuthenticatedSubject,
    download_subject: AuthenticatedSubject,
    upload_revocation_id: SecretText,
    download_revocation_id: SecretText,
    upload_credential_fingerprint: String,
    download_credential_fingerprint: String,
    archival_origin_available: bool,
    expires_unix: i64,
    observed_upload_bytes: u64,
    observed_download_bytes: u64,
    observed_upload_chunks: u32,
    observed_download_chunks: u32,
    upload_completion_bytes: Option<u64>,
    download_completion_bytes: Option<u64>,
}

impl fmt::Debug for PendingSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PendingSession([redacted])")
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectPopularity {
    successful_recoveries: u64,
    relayed_bytes: u64,
    promotion_emitted: bool,
}

/// Versioned backend persistence representation. It is deliberately private:
/// this is operator state, never an HTTP DTO. Provider access secrets and TURN
/// usernames/passwords are structurally absent. Authenticated subjects are
/// already domain-separated bearer-token digests supplied by the server.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RelayPersistenceSnapshot {
    schema: String,
    configuration_sha256: String,
    period: BillingPeriod,
    kill_switch: bool,
    usage: BTreeMap<String, SubjectUsage>,
    global_relayed: u64,
    global_reserved: u64,
    advertisements: Vec<PersistedSeedEntry>,
    pending_sessions: Vec<PersistedPendingSession>,
    revoked_credentials: BTreeMap<String, i64>,
    popularity: BTreeMap<String, ObjectPopularity>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedSeedEntry {
    subject: String,
    advertisement_id: String,
    object_sha256: String,
    request_sha256: String,
    encoded_size: u64,
    expires_unix: i64,
    category: RelayObjectCategory,
    policy: ClientSeedingPolicy,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedPendingSession {
    session_id: String,
    object_sha256: String,
    expected_bytes: u64,
    upload_subject: String,
    download_subject: String,
    upload_credential_fingerprint: String,
    download_credential_fingerprint: String,
    archival_origin_available: bool,
    expires_unix: i64,
    observed_upload_bytes: u64,
    observed_download_bytes: u64,
    observed_upload_chunks: u32,
    observed_download_chunks: u32,
    upload_completion_bytes: Option<u64>,
    download_completion_bytes: Option<u64>,
}

/// In-memory deterministic core. A server integration should place it behind a
/// mutex and persist quota/session transitions atomically before enabling the
/// feature. The APIs intentionally expose no search or seed-list operation.
pub struct RelayCoordinator<P, I> {
    config: RelayControlConfig,
    limits: ProtocolLimits,
    trusted_origin_keys: TrustedSigningKeys,
    trusted_relay_keys: TrustedSigningKeys,
    relay_signing_key: SigningKey,
    provider: P,
    ids: I,
    kill_switch: bool,
    period: BillingPeriod,
    usage: BTreeMap<AuthenticatedSubject, SubjectUsage>,
    global_relayed: u64,
    global_reserved: u64,
    seeds: BTreeMap<String, Vec<SeedEntry>>,
    pending: BTreeMap<String, PendingSession>,
    revoked_credentials: BTreeMap<String, i64>,
    popularity: BTreeMap<String, ObjectPopularity>,
    persistence_maximum_bytes: usize,
}

impl<P: RelayProvider, I: OpaqueIdSource> RelayCoordinator<P, I> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: RelayControlConfig,
        limits: ProtocolLimits,
        trusted_origin_keys: TrustedSigningKeys,
        relay_signing_key: SigningKey,
        provider: P,
        ids: I,
        period: BillingPeriod,
    ) -> Result<Self, RelayError> {
        config.validate(&limits)?;
        if trusted_origin_keys.is_empty() {
            return Err(RelayError::UntrustedObject);
        }
        let trusted_relay_keys = BTreeMap::from([(
            config.signing_key_id.clone(),
            relay_signing_key.verifying_key(),
        )]);
        Ok(Self {
            config,
            limits,
            trusted_origin_keys,
            trusted_relay_keys,
            relay_signing_key,
            provider,
            ids,
            kill_switch: false,
            period,
            usage: BTreeMap::new(),
            global_relayed: 0,
            global_reserved: 0,
            seeds: BTreeMap::new(),
            pending: BTreeMap::new(),
            revoked_credentials: BTreeMap::new(),
            popularity: BTreeMap::new(),
            persistence_maximum_bytes: MAX_PERSISTENCE_BYTES,
        })
    }

    /// Serialize the complete durable control-plane state. The returned bytes
    /// contain no provider credential, TURN username/password, signing key, IP
    /// address, hostname, or peer-facing identifier. Callers must atomically
    /// replace their state file before returning any newly issued grant or
    /// externally acknowledging a terminal transition.
    pub fn export_persistence_json(&self) -> Result<Vec<u8>, RelayError> {
        let advertisements = self
            .seeds
            .values()
            .flatten()
            .map(|entry| PersistedSeedEntry {
                subject: entry.subject.expose_for_backend_dispatch().to_string(),
                advertisement_id: entry.advertisement_id.clone(),
                object_sha256: entry.object.object_sha256.clone(),
                request_sha256: entry.object.request_sha256.clone(),
                encoded_size: entry.object.encoded_size,
                expires_unix: entry.object.expires_unix,
                category: entry.object.category,
                policy: entry.policy.clone(),
            })
            .collect();
        let pending_sessions = self
            .pending
            .iter()
            .map(|(session_id, session)| PersistedPendingSession {
                session_id: session_id.clone(),
                object_sha256: session.object_sha256.clone(),
                expected_bytes: session.expected_bytes,
                upload_subject: session
                    .upload_subject
                    .expose_for_backend_dispatch()
                    .to_string(),
                download_subject: session
                    .download_subject
                    .expose_for_backend_dispatch()
                    .to_string(),
                upload_credential_fingerprint: session.upload_credential_fingerprint.clone(),
                download_credential_fingerprint: session.download_credential_fingerprint.clone(),
                archival_origin_available: session.archival_origin_available,
                expires_unix: session.expires_unix,
                observed_upload_bytes: session.observed_upload_bytes,
                observed_download_bytes: session.observed_download_bytes,
                observed_upload_chunks: session.observed_upload_chunks,
                observed_download_chunks: session.observed_download_chunks,
                upload_completion_bytes: session.upload_completion_bytes,
                download_completion_bytes: session.download_completion_bytes,
            })
            .collect();
        let usage = self
            .usage
            .iter()
            .map(|(subject, usage)| (subject.expose_for_backend_dispatch().to_string(), *usage))
            .collect();
        let snapshot = RelayPersistenceSnapshot {
            schema: PERSISTENCE_SCHEMA.into(),
            configuration_sha256: self.configuration_sha256(),
            period: self.period,
            kill_switch: self.kill_switch,
            usage,
            global_relayed: self.global_relayed,
            global_reserved: self.global_reserved,
            advertisements,
            pending_sessions,
            revoked_credentials: self.revoked_credentials.clone(),
            popularity: self.popularity.clone(),
        };
        if snapshot.usage.len() > MAX_PERSISTED_SUBJECTS
            || snapshot.advertisements.len() > MAX_PERSISTED_ADVERTISEMENTS
            || snapshot.pending_sessions.len() > MAX_PERSISTED_SESSIONS
            || snapshot.revoked_credentials.len() > MAX_PERSISTED_REVOCATIONS
            || snapshot.popularity.len() > MAX_PERSISTED_POPULARITY
        {
            return Err(RelayError::PersistenceRejected);
        }
        let bytes = serde_json::to_vec(&snapshot).map_err(|_| RelayError::PersistenceRejected)?;
        if bytes.is_empty() || bytes.len() > self.persistence_maximum_bytes {
            return Err(RelayError::PersistenceRejected);
        }
        Ok(bytes)
    }

    #[cfg(test)]
    fn set_persistence_maximum_bytes_for_test(&mut self, value: usize) {
        self.persistence_maximum_bytes = value;
    }

    /// Restore a bounded snapshot before accepting traffic. Sessions that were
    /// live when the process stopped are never resumed: their complete byte
    /// reservations are conservatively charged and both signed credential
    /// fingerprints become revoked until expiry. Provider credentials are
    /// intentionally unavailable after restart and simply expire at their
    /// short TTL; no TURN username/password was persisted.
    pub fn restore_persistence_json(
        &mut self,
        bytes: &[u8],
        now_unix: i64,
        current_period: BillingPeriod,
    ) -> Result<(), RelayError> {
        if bytes.is_empty() || bytes.len() > MAX_PERSISTENCE_BYTES {
            return Err(RelayError::PersistenceRejected);
        }
        let snapshot: RelayPersistenceSnapshot =
            serde_json::from_slice(bytes).map_err(|_| RelayError::PersistenceRejected)?;
        if snapshot.schema != PERSISTENCE_SCHEMA
            || snapshot.configuration_sha256 != self.configuration_sha256()
            || snapshot.period > current_period
            || snapshot.usage.len() > MAX_PERSISTED_SUBJECTS
            || snapshot.advertisements.len() > MAX_PERSISTED_ADVERTISEMENTS
            || snapshot.pending_sessions.len() > MAX_PERSISTED_SESSIONS
            || snapshot.pending_sessions.len() > self.config.quotas.global_concurrency as usize
            || snapshot.revoked_credentials.len() > MAX_PERSISTED_REVOCATIONS
            || snapshot.popularity.len() > MAX_PERSISTED_POPULARITY
        {
            return Err(RelayError::PersistenceRejected);
        }

        let mut usage = BTreeMap::new();
        for (raw_subject, value) in snapshot.usage {
            let subject = AuthenticatedSubject::from_persisted_digest(raw_subject)?;
            if value.uploaded > self.config.quotas.per_user_upload_bytes_per_month
                || value.downloaded > self.config.quotas.per_user_download_bytes_per_month
                || value.advertised_storage > self.config.quotas.per_user_advertised_storage_bytes
                || value.active_uploads > self.config.quotas.per_user_concurrency
                || value.active_downloads > self.config.quotas.per_user_concurrency
                || usage.insert(subject, value).is_some()
            {
                return Err(RelayError::PersistenceRejected);
            }
        }

        let mut seeds: BTreeMap<String, Vec<SeedEntry>> = BTreeMap::new();
        let mut advertisement_ids = BTreeSet::new();
        let mut advertised_by_subject: BTreeMap<AuthenticatedSubject, u64> = BTreeMap::new();
        let mut advertised_pairs = BTreeSet::new();
        for persisted in snapshot.advertisements {
            let subject = AuthenticatedSubject::from_persisted_digest(persisted.subject)?;
            let object = VerifiedRelayObject {
                object_sha256: persisted.object_sha256,
                request_sha256: persisted.request_sha256,
                encoded_size: persisted.encoded_size,
                expires_unix: persisted.expires_unix,
                category: persisted.category,
            };
            if !valid_opaque_id(&persisted.advertisement_id)
                || !advertisement_ids.insert(persisted.advertisement_id.clone())
                || !valid_sha256(&object.object_sha256)
                || !valid_sha256(&object.request_sha256)
                || object.encoded_size == 0
                || object.encoded_size > self.limits.max_encoded_bytes
                || persisted.policy.permits(&object).is_err()
                || !advertised_pairs.insert((subject.clone(), object.object_sha256.clone()))
            {
                return Err(RelayError::PersistenceRejected);
            }
            let storage = advertised_by_subject.entry(subject.clone()).or_default();
            *storage = storage
                .checked_add(object.encoded_size)
                .ok_or(RelayError::PersistenceRejected)?;
            seeds
                .entry(object.object_sha256.clone())
                .or_default()
                .push(SeedEntry {
                    subject,
                    advertisement_id: persisted.advertisement_id,
                    object,
                    policy: persisted.policy,
                });
        }
        for entries in seeds.values_mut() {
            entries.sort_by(|left, right| left.advertisement_id.cmp(&right.advertisement_id));
        }

        let mut pending_ids = BTreeSet::new();
        let mut expected_upload_reserved: BTreeMap<AuthenticatedSubject, (u64, u32)> =
            BTreeMap::new();
        let mut expected_download_reserved: BTreeMap<AuthenticatedSubject, (u64, u32)> =
            BTreeMap::new();
        let mut expected_global_reserved = 0_u64;
        let mut recovered_pending = Vec::new();
        for pending in snapshot.pending_sessions {
            let upload_subject =
                AuthenticatedSubject::from_persisted_digest(pending.upload_subject.clone())?;
            let download_subject =
                AuthenticatedSubject::from_persisted_digest(pending.download_subject.clone())?;
            if upload_subject == download_subject
                || !valid_opaque_id(&pending.session_id)
                || !pending_ids.insert(pending.session_id.clone())
                || !valid_sha256(&pending.object_sha256)
                || !valid_sha256(&pending.upload_credential_fingerprint)
                || !valid_sha256(&pending.download_credential_fingerprint)
                || pending.upload_credential_fingerprint == pending.download_credential_fingerprint
                || pending.expected_bytes == 0
                || pending.expected_bytes > self.limits.max_encoded_bytes
                || pending.expires_unix <= 0
                || pending.observed_upload_bytes > pending.expected_bytes
                || pending.observed_download_bytes > pending.expected_bytes
                || pending.observed_upload_chunks > self.limits.max_relay_chunks
                || pending.observed_download_chunks > self.limits.max_relay_chunks
                || pending
                    .upload_completion_bytes
                    .is_some_and(|bytes| bytes != pending.expected_bytes)
                || pending
                    .download_completion_bytes
                    .is_some_and(|bytes| bytes != pending.expected_bytes)
            {
                return Err(RelayError::PersistenceRejected);
            }
            add_reservation(
                &mut expected_upload_reserved,
                upload_subject.clone(),
                pending.expected_bytes,
            )?;
            add_reservation(
                &mut expected_download_reserved,
                download_subject.clone(),
                pending.expected_bytes,
            )?;
            expected_global_reserved = expected_global_reserved
                .checked_add(pending.expected_bytes)
                .ok_or(RelayError::PersistenceRejected)?;
            recovered_pending.push((pending, upload_subject, download_subject));
        }

        if expected_global_reserved != snapshot.global_reserved
            || snapshot
                .global_relayed
                .checked_add(snapshot.global_reserved)
                .is_none_or(|value| {
                    value > self.config.quotas.global_relay_bytes_per_month
                        || value > self.config.quotas.cost_stop_after_bytes_per_month
                })
        {
            return Err(RelayError::PersistenceRejected);
        }
        for (subject, value) in &usage {
            let (upload_bytes, upload_count) = expected_upload_reserved
                .get(subject)
                .copied()
                .unwrap_or_default();
            let (download_bytes, download_count) = expected_download_reserved
                .get(subject)
                .copied()
                .unwrap_or_default();
            let advertised = advertised_by_subject.get(subject).copied().unwrap_or(0);
            if value.reserved_upload != upload_bytes
                || value.active_uploads != upload_count
                || value.reserved_download != download_bytes
                || value.active_downloads != download_count
                || value.advertised_storage != advertised
                || value
                    .uploaded
                    .checked_add(value.reserved_upload)
                    .is_none_or(|total| total > self.config.quotas.per_user_upload_bytes_per_month)
                || value
                    .downloaded
                    .checked_add(value.reserved_download)
                    .is_none_or(|total| {
                        total > self.config.quotas.per_user_download_bytes_per_month
                    })
            {
                return Err(RelayError::PersistenceRejected);
            }
        }
        if expected_upload_reserved
            .keys()
            .chain(expected_download_reserved.keys())
            .chain(advertised_by_subject.keys())
            .any(|subject| !usage.contains_key(subject))
        {
            return Err(RelayError::PersistenceRejected);
        }

        let mut revoked_credentials = snapshot.revoked_credentials;
        if revoked_credentials
            .iter()
            .any(|(fingerprint, expires)| !valid_sha256(fingerprint) || *expires <= 0)
            || snapshot.popularity.iter().any(|(object, stats)| {
                !valid_sha256(object)
                    || stats.successful_recoveries == 0 && stats.relayed_bytes == 0
            })
        {
            return Err(RelayError::PersistenceRejected);
        }

        let crossed_period = snapshot.period < current_period;
        let mut global_relayed = if crossed_period {
            0
        } else {
            snapshot.global_relayed
        };
        if crossed_period {
            for value in usage.values_mut() {
                value.uploaded = 0;
                value.downloaded = 0;
            }
        }
        // Never resume a pre-crash session. Release each durable reservation,
        // charge the full signed maximum in the active period, and remember
        // both credential fingerprints through their original expiry.
        for (pending, upload_subject, download_subject) in recovered_pending {
            let upload = usage
                .get_mut(&upload_subject)
                .ok_or(RelayError::PersistenceRejected)?;
            upload.reserved_upload = upload
                .reserved_upload
                .checked_sub(pending.expected_bytes)
                .ok_or(RelayError::PersistenceRejected)?;
            upload.active_uploads = upload
                .active_uploads
                .checked_sub(1)
                .ok_or(RelayError::PersistenceRejected)?;
            upload.uploaded = upload
                .uploaded
                .checked_add(pending.expected_bytes)
                .filter(|total| *total <= self.config.quotas.per_user_upload_bytes_per_month)
                .ok_or(RelayError::PersistenceRejected)?;
            let download = usage
                .get_mut(&download_subject)
                .ok_or(RelayError::PersistenceRejected)?;
            download.reserved_download = download
                .reserved_download
                .checked_sub(pending.expected_bytes)
                .ok_or(RelayError::PersistenceRejected)?;
            download.active_downloads = download
                .active_downloads
                .checked_sub(1)
                .ok_or(RelayError::PersistenceRejected)?;
            download.downloaded = download
                .downloaded
                .checked_add(pending.expected_bytes)
                .filter(|total| *total <= self.config.quotas.per_user_download_bytes_per_month)
                .ok_or(RelayError::PersistenceRejected)?;
            global_relayed = global_relayed
                .checked_add(pending.expected_bytes)
                .filter(|total| {
                    *total <= self.config.quotas.global_relay_bytes_per_month
                        && *total <= self.config.quotas.cost_stop_after_bytes_per_month
                })
                .ok_or(RelayError::PersistenceRejected)?;
            revoked_credentials.insert(pending.upload_credential_fingerprint, pending.expires_unix);
            revoked_credentials.insert(
                pending.download_credential_fingerprint,
                pending.expires_unix,
            );
        }
        if revoked_credentials.len() > MAX_PERSISTED_REVOCATIONS {
            return Err(RelayError::PersistenceRejected);
        }
        revoked_credentials.retain(|_, expires| *expires > now_unix);

        self.period = current_period;
        self.kill_switch = snapshot.kill_switch;
        self.usage = usage;
        self.global_relayed = global_relayed;
        self.global_reserved = 0;
        self.seeds = seeds;
        self.pending.clear();
        self.revoked_credentials = revoked_credentials;
        self.popularity = snapshot.popularity;
        self.remove_expired(now_unix);
        Ok(())
    }

    fn configuration_sha256(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"rw-community-relay-persistence-configuration-v1\0");
        digest.update(self.config.relay_id.as_bytes());
        digest.update([0]);
        digest.update(self.config.signing_key_id.as_bytes());
        for value in [
            self.config.credential_lifetime_seconds as u64,
            self.config.provider_pricing_verified as u64,
            u64::from(self.config.max_chunk_plaintext_bytes),
            self.config.quotas.per_user_upload_bytes_per_month,
            self.config.quotas.per_user_download_bytes_per_month,
            self.config.quotas.per_user_advertised_storage_bytes,
            u64::from(self.config.quotas.per_user_concurrency),
            u64::from(self.config.quotas.global_concurrency),
            self.config.quotas.global_relay_bytes_per_month,
            self.config.quotas.cost_stop_after_bytes_per_month,
            self.config.promotion.successful_recoveries,
            self.config.promotion.relayed_bytes,
            self.limits.max_manifest_bytes,
            self.limits.max_encoded_bytes,
            self.limits.max_decoded_bytes,
            self.limits.max_decompression_ratio,
            self.limits.max_variables as u64,
            self.limits.max_provenance_entries as u64,
            self.limits.max_attributions as u64,
            self.limits.max_case_artifacts as u64,
            self.limits.max_relay_chunks.into(),
        ] {
            digest.update(value.to_be_bytes());
        }
        for (key_id, key) in &self.trusted_origin_keys {
            digest.update((key_id.len() as u64).to_be_bytes());
            digest.update(key_id.as_bytes());
            digest.update(key.to_bytes());
        }
        format!("{:x}", digest.finalize())
    }

    pub fn advertise(
        &mut self,
        subject: AuthenticatedSubject,
        signed_manifest: &SignedObjectManifest,
        policy: ClientSeedingPolicy,
        now_unix: i64,
        period: BillingPeriod,
    ) -> Result<(AdvertisementReceipt, RelayAuditEvent), RelayError> {
        self.reap_expired_sessions(now_unix);
        self.admission_gate(period)?;
        let object = verify_origin_signed_identity(
            signed_manifest,
            now_unix,
            &self.trusted_origin_keys,
            &self.limits,
        )?;
        policy.permits(&object)?;

        let already_advertised = self
            .seeds
            .get(object.object_sha256())
            .is_some_and(|entries| entries.iter().any(|entry| entry.subject == subject));
        let advertisement_count = self.seeds.values().map(Vec::len).sum::<usize>();
        if !already_advertised && advertisement_count >= MAX_PERSISTED_ADVERTISEMENTS {
            return Err(RelayError::PersistenceRejected);
        }
        if !self.usage.contains_key(&subject) && self.usage.len() >= MAX_PERSISTED_SUBJECTS {
            return Err(RelayError::PersistenceRejected);
        }
        // Advertisement admission is transactional with respect to the exact
        // durable snapshot. This prevents one otherwise-valid advertisement
        // from pushing state beyond its JSON/count bound and causing the
        // server's subsequent persist-or-kill path to fail on an unpersistable
        // in-memory state.
        let previous_usage = self.usage.get(&subject).copied();
        let object_key = object.object_sha256.clone();
        let previous_entries = self.seeds.get(&object_key).cloned();
        let usage = self.usage.entry(subject.clone()).or_default();
        if !already_advertised
            && usage.advertised_storage.saturating_add(object.encoded_size)
                > self.config.quotas.per_user_advertised_storage_bytes
        {
            return Err(RelayError::QuotaReached);
        }
        if !already_advertised {
            usage.advertised_storage += object.encoded_size;
        }
        let advertisement_id = self.ids.next_id(OpaqueIdKind::Advertisement)?;
        if !valid_opaque_id(&advertisement_id) {
            return Err(RelayError::UnsafeIdentifier);
        }
        if self
            .seeds
            .values()
            .flatten()
            .any(|entry| entry.advertisement_id == advertisement_id)
        {
            return Err(RelayError::UnsafeIdentifier);
        }
        let entry = SeedEntry {
            subject: subject.clone(),
            advertisement_id: advertisement_id.clone(),
            object: object.clone(),
            policy,
        };
        let entries = self.seeds.entry(object.object_sha256.clone()).or_default();
        entries.retain(|existing| existing.subject != subject);
        entries.push(entry);
        entries.sort_by(|left, right| left.advertisement_id.cmp(&right.advertisement_id));
        if let Err(error) = self.export_persistence_json() {
            match previous_usage {
                Some(usage) => {
                    self.usage.insert(subject.clone(), usage);
                }
                None => {
                    self.usage.remove(&subject);
                }
            }
            match previous_entries {
                Some(entries) => {
                    self.seeds.insert(object_key, entries);
                }
                None => {
                    self.seeds.remove(&object_key);
                }
            }
            return Err(error);
        }
        let receipt = AdvertisementReceipt {
            schema: ADVERTISEMENT_SCHEMA.into(),
            advertisement_id,
            object_sha256: object.object_sha256.clone(),
            expires_unix: object.expires_unix,
        };
        let audit = RelayAuditEvent {
            schema: AUDIT_EVENT_SCHEMA.into(),
            kind: RelayAuditKind::AdvertisementAccepted,
            session_id: None,
            object_sha256: Some(object.object_sha256),
            failure_code: None,
        };
        Ok((receipt, audit))
    }

    pub fn update_seed_policy(
        &mut self,
        subject: &AuthenticatedSubject,
        policy: ClientSeedingPolicy,
    ) {
        for entries in self.seeds.values_mut() {
            for entry in entries.iter_mut().filter(|entry| &entry.subject == subject) {
                entry.policy = policy.clone();
            }
        }
    }

    pub fn begin_cold_lookup(
        &mut self,
        requester: AuthenticatedSubject,
        requester_policy: ClientRetrievalPolicy,
        object_sha256: &str,
        archival_origin_available: bool,
        now_unix: i64,
        period: BillingPeriod,
    ) -> ColdLookupOutcome {
        if !valid_sha256(object_sha256) {
            return self.fallback(archival_origin_available, RelayError::NotAvailable);
        }
        self.reap_expired_sessions(now_unix);
        if let Err(error) = self.admission_gate(period) {
            return self.fallback(archival_origin_available, error);
        }
        self.remove_expired(now_unix);
        let Some(seed) = self.select_seed(&requester, object_sha256).cloned() else {
            return self.fallback(archival_origin_available, RelayError::NotAvailable);
        };
        if let Err(error) = seed.policy.permits(&seed.object) {
            return self.fallback(archival_origin_available, error);
        }
        if let Err(error) = requester_policy.permits(seed.object.encoded_size) {
            return self.fallback(archival_origin_available, error);
        }
        if let Err(error) = self.reserve(&seed.subject, &requester, seed.object.encoded_size) {
            return self.fallback(archival_origin_available, error);
        }

        match self.issue_session(seed, requester, archival_origin_available, now_unix) {
            Ok(grant) => ColdLookupOutcome::Relay(Box::new(grant)),
            Err(error) => {
                // `issue_session` releases reservations on every failed path.
                self.fallback(archival_origin_available, error)
            }
        }
    }

    pub fn verify_active_credential(
        &self,
        signed: &SignedRelayCredential,
        now_unix: i64,
    ) -> Result<(), RelayError> {
        match verify_signed_relay_credential(
            signed,
            now_unix,
            &self.trusted_relay_keys,
            &self.limits,
        ) {
            Ok(()) => {}
            Err(ProtocolError::RelayCredentialExpired) => {
                return Err(RelayError::CredentialExpired);
            }
            Err(_) => return Err(RelayError::CredentialInvalid),
        }
        let fingerprint = credential_fingerprint(signed, now_unix, &self.limits)?;
        if self.revoked_credentials.contains_key(&fingerprint) {
            return Err(RelayError::CredentialRevoked);
        }
        let pending = self
            .pending
            .get(&signed.claims.session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        if fingerprint != pending.upload_credential_fingerprint
            && fingerprint != pending.download_credential_fingerprint
        {
            return Err(RelayError::CredentialInvalid);
        }
        Ok(())
    }

    pub fn authorize_participant(
        &self,
        subject: &AuthenticatedSubject,
        signed: &SignedRelayCredential,
        role: RelayRole,
        now_unix: i64,
    ) -> Result<AuthorizedRelayParticipant, RelayError> {
        self.verify_active_credential(signed, now_unix)?;
        let pending = self
            .pending
            .get(&signed.claims.session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        let (expected_direction, expected_subject, expected_fingerprint) = match role {
            RelayRole::Uploader => (
                RelayDirection::Upload,
                &pending.upload_subject,
                &pending.upload_credential_fingerprint,
            ),
            RelayRole::Downloader => (
                RelayDirection::Download,
                &pending.download_subject,
                &pending.download_credential_fingerprint,
            ),
        };
        let fingerprint = credential_fingerprint(signed, now_unix, &self.limits)?;
        if signed.claims.direction != expected_direction
            || expected_subject != subject
            || &fingerprint != expected_fingerprint
            || signed.claims.object_sha256 != pending.object_sha256
        {
            return Err(RelayError::CredentialInvalid);
        }
        Ok(AuthorizedRelayParticipant {
            session_id: signed.claims.session_id.clone(),
            object_sha256: pending.object_sha256.clone(),
            role,
            credential_fingerprint: fingerprint,
            expires_unix: pending.expires_unix.min(signed.claims.expires_unix),
        })
    }

    pub(crate) fn sign_transport_binding(
        &self,
        uploader: &EphemeralPublicOffer,
        downloader: &EphemeralPublicOffer,
        now_unix: i64,
    ) -> Result<SignedSessionBinding, RelayError> {
        let pending = self
            .pending
            .get(&uploader.session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        if uploader.session_id != downloader.session_id
            || uploader.object_sha256 != pending.object_sha256
            || downloader.object_sha256 != pending.object_sha256
            || uploader.credential_fingerprint != pending.upload_credential_fingerprint
            || downloader.credential_fingerprint != pending.download_credential_fingerprint
            || now_unix >= pending.expires_unix
        {
            return Err(RelayError::KeyAgreementRejected);
        }
        let binding = build_session_binding(uploader, downloader, pending.expires_unix)?;
        sign_session_binding(
            binding,
            self.config.signing_key_id.clone(),
            &self.relay_signing_key,
        )
    }

    /// Admit one encrypted chunk before provider routing. This continuously
    /// enforces revocation and aggregate byte/chunk scope; issuance limits are
    /// never treated as sufficient on their own.
    pub fn admit_envelope(
        &mut self,
        signed: &SignedRelayCredential,
        envelope: &rw_community_protocol::EncryptedRelayEnvelope,
        now_unix: i64,
    ) -> Result<(), RelayError> {
        self.reap_expired_sessions(now_unix);
        self.verify_active_credential(signed, now_unix)?;
        envelope
            .validate(&signed.claims, &self.limits)
            .map_err(|_| RelayError::EnvelopeRejected)?;
        let pending = self
            .pending
            .get_mut(&signed.claims.session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        let (observed_bytes, observed_chunks) = match signed.claims.direction {
            RelayDirection::Upload => (
                &mut pending.observed_upload_bytes,
                &mut pending.observed_upload_chunks,
            ),
            RelayDirection::Download => (
                &mut pending.observed_download_bytes,
                &mut pending.observed_download_chunks,
            ),
        };
        let new_bytes = observed_bytes
            .checked_add(u64::from(envelope.plaintext_size))
            .ok_or(RelayError::QuotaReached)?;
        let new_chunks = observed_chunks
            .checked_add(1)
            .ok_or(RelayError::QuotaReached)?;
        if new_bytes > pending.expected_bytes
            || new_bytes > signed.claims.max_bytes
            || new_chunks > signed.claims.max_chunks
        {
            return Err(RelayError::QuotaReached);
        }
        *observed_bytes = new_bytes;
        *observed_chunks = new_chunks;
        Ok(())
    }

    /// Record one authenticated participant's exact-byte completion. Success
    /// and popularity are committed only after both opposite signed roles
    /// report the full identical object size. A lost final data-plane message
    /// therefore leaves the session pending until expiry/failure and cannot be
    /// counted as a successful recovery.
    pub fn report_participant_completion(
        &mut self,
        session_id: &str,
        role: RelayRole,
        transferred_bytes: u64,
        period: BillingPeriod,
    ) -> Result<ParticipantCompletionResult, RelayError> {
        self.roll_period(period);
        let ready = {
            let pending = self
                .pending
                .get_mut(session_id)
                .ok_or(RelayError::CredentialRevoked)?;
            if transferred_bytes != pending.expected_bytes {
                return Err(RelayError::ObjectMismatch);
            }
            let report = match role {
                RelayRole::Uploader => &mut pending.upload_completion_bytes,
                RelayRole::Downloader => &mut pending.download_completion_bytes,
            };
            if report.is_some_and(|bytes| bytes != transferred_bytes) {
                return Err(RelayError::Replay);
            }
            *report = Some(transferred_bytes);
            pending.upload_completion_bytes == Some(pending.expected_bytes)
                && pending.download_completion_bytes == Some(pending.expected_bytes)
        };
        if !ready {
            return Ok(ParticipantCompletionResult::AwaitingCounterpart);
        }
        let pending = self
            .pending
            .remove(session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        self.finish_removed_session(session_id, pending, transferred_bytes, true)
            .map(ParticipantCompletionResult::Complete)
    }

    #[cfg(test)]
    pub fn complete_session(
        &mut self,
        session_id: &str,
        transferred_bytes: u64,
        successful: bool,
        period: BillingPeriod,
    ) -> Result<CompletionResult, RelayError> {
        self.roll_period(period);
        let pending = self
            .pending
            .remove(session_id)
            .ok_or(RelayError::CredentialRevoked)?;
        self.finish_removed_session(session_id, pending, transferred_bytes, successful)
    }

    fn finish_removed_session(
        &mut self,
        session_id: &str,
        pending: PendingSession,
        transferred_bytes: u64,
        successful: bool,
    ) -> Result<CompletionResult, RelayError> {
        if transferred_bytes > pending.expected_bytes {
            // The signed credential and provider lease cap the transfer at the
            // expected object size. If an integration reports more, account
            // the entire reserved maximum, revoke, and fail closed.
            self.commit_reserved_session(&pending);
            self.revoke_provider_credentials(&pending);
            self.mark_revoked(&pending);
            return Err(RelayError::QuotaReached);
        }
        self.commit_reserved_session(&pending);
        self.revoke_provider_credentials(&pending);
        self.mark_revoked(&pending);

        let complete_success = successful && transferred_bytes == pending.expected_bytes;
        let promotion = if complete_success {
            let stats = self
                .popularity
                .entry(pending.object_sha256.clone())
                .or_default();
            stats.successful_recoveries = stats.successful_recoveries.saturating_add(1);
            stats.relayed_bytes = stats.relayed_bytes.saturating_add(transferred_bytes);
            if !stats.promotion_emitted
                && (stats.successful_recoveries >= self.config.promotion.successful_recoveries
                    || stats.relayed_bytes >= self.config.promotion.relayed_bytes)
            {
                stats.promotion_emitted = true;
                Some(PromotionSignal {
                    schema: PROMOTION_SCHEMA.into(),
                    object_sha256: pending.object_sha256.clone(),
                    successful_recoveries: stats.successful_recoveries,
                    relayed_bytes: stats.relayed_bytes,
                })
            } else {
                None
            }
        } else {
            None
        };
        Ok(CompletionResult {
            audit: RelayAuditEvent {
                schema: AUDIT_EVENT_SCHEMA.into(),
                kind: if complete_success {
                    RelayAuditKind::SessionCompleted
                } else {
                    RelayAuditKind::SessionFailed
                },
                session_id: Some(session_id.to_string()),
                object_sha256: Some(pending.object_sha256),
                failure_code: (!complete_success).then_some(
                    if successful {
                        RelayError::ObjectMismatch
                    } else {
                        RelayError::ProviderUnavailable
                    }
                    .public_code()
                    .to_string(),
                ),
            },
            promotion,
        })
    }

    pub fn fail_and_fallback(
        &mut self,
        session_id: &str,
        transferred_bytes: u64,
        period: BillingPeriod,
    ) -> PublicRelayFailure {
        self.roll_period(period);
        let Some(pending) = self.pending.remove(session_id) else {
            return PublicRelayFailure::new(
                RelayError::ProviderUnavailable,
                FallbackTarget::Unavailable,
            );
        };
        // TURN payload bytes bypass the backend. Client-reported bytes are
        // useful only for diagnostics; the cost ledger conservatively commits
        // the full reserved maximum once provider credentials existed.
        let _ = transferred_bytes;
        self.commit_reserved_session(&pending);
        self.revoke_provider_credentials(&pending);
        self.mark_revoked(&pending);
        PublicRelayFailure::new(
            RelayError::ProviderUnavailable,
            fallback_target(pending.archival_origin_available),
        )
    }

    /// Activating the global switch revokes every pending relay credential and
    /// prevents new advertisements/sessions. HTTPS origin operation is outside
    /// this crate and remains unaffected.
    pub fn set_kill_switch(&mut self, enabled: bool) {
        self.kill_switch = enabled;
        if !enabled {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for session in pending.into_values() {
            self.commit_reserved_session(&session);
            self.revoke_provider_credentials(&session);
            self.mark_revoked(&session);
        }
    }

    pub const fn kill_switch(&self) -> bool {
        self.kill_switch
    }

    fn issue_session(
        &mut self,
        seed: SeedEntry,
        requester: AuthenticatedSubject,
        archival_origin_available: bool,
        now_unix: i64,
    ) -> Result<RelaySessionGrant, RelayError> {
        let result = self.build_session_grant(&seed, &requester, now_unix);
        let (
            session_id,
            upload_credential_fingerprint,
            download_credential_fingerprint,
            upload_revocation_id,
            download_revocation_id,
            grant,
        ) = match result {
            Ok(result) => result,
            Err(error) => {
                self.release_reservation(&seed.subject, &requester, seed.object.encoded_size);
                return Err(error);
            }
        };
        let expires_unix = grant.upload.credential.claims.expires_unix;
        self.pending.insert(
            session_id,
            PendingSession {
                object_sha256: seed.object.object_sha256,
                expected_bytes: seed.object.encoded_size,
                upload_subject: seed.subject,
                download_subject: requester,
                upload_revocation_id,
                download_revocation_id,
                upload_credential_fingerprint,
                download_credential_fingerprint,
                archival_origin_available,
                expires_unix,
                observed_upload_bytes: 0,
                observed_download_bytes: 0,
                observed_upload_chunks: 0,
                observed_download_chunks: 0,
                upload_completion_bytes: None,
                download_completion_bytes: None,
            },
        );
        Ok(grant)
    }

    #[allow(clippy::type_complexity)]
    fn build_session_grant(
        &mut self,
        seed: &SeedEntry,
        requester: &AuthenticatedSubject,
        now_unix: i64,
    ) -> Result<
        (
            String,
            String,
            String,
            SecretText,
            SecretText,
            RelaySessionGrant,
        ),
        RelayError,
    > {
        let session_id = self.ids.next_id(OpaqueIdKind::Session)?;
        let upload_ticket = self.ids.next_id(OpaqueIdKind::Ticket)?;
        let download_ticket = self.ids.next_id(OpaqueIdKind::Ticket)?;
        let upload_alias = self.ids.next_id(OpaqueIdKind::ParticipantAlias)?;
        let download_alias = self.ids.next_id(OpaqueIdKind::ParticipantAlias)?;
        for value in [
            &session_id,
            &upload_ticket,
            &download_ticket,
            &upload_alias,
            &download_alias,
        ] {
            if !valid_opaque_id(value) {
                return Err(RelayError::UnsafeIdentifier);
            }
        }
        if BTreeSet::from([
            session_id.as_str(),
            upload_ticket.as_str(),
            download_ticket.as_str(),
            upload_alias.as_str(),
            download_alias.as_str(),
        ])
        .len()
            != 5
            || self.pending.contains_key(&session_id)
        {
            return Err(RelayError::UnsafeIdentifier);
        }
        let expires_unix = now_unix.saturating_add(self.config.credential_lifetime_seconds);
        let max_chunks = crate::bounded_relay_chunk_count(seed.object.encoded_size, &self.limits)
            .map_err(|_| RelayError::QuotaReached)?;
        let upload_credential = self.sign_credential(
            &session_id,
            &upload_alias,
            &seed.object,
            RelayDirection::Upload,
            now_unix,
            expires_unix,
            max_chunks,
        )?;
        let download_credential = self.sign_credential(
            &session_id,
            &download_alias,
            &seed.object,
            RelayDirection::Download,
            now_unix,
            expires_unix,
            max_chunks,
        )?;
        // Finish every locally fallible operation before asking the provider
        // to mint credentials, so no later validation error can orphan a
        // provider lease.
        let upload_fingerprint =
            credential_fingerprint(&upload_credential, now_unix, &self.limits)?;
        let download_fingerprint =
            credential_fingerprint(&download_credential, now_unix, &self.limits)?;
        let upload_candidate = RelayCandidate {
            kind: RelayCandidateKind::Relay,
            relay_id: self.config.relay_id.clone(),
            ticket_id: upload_ticket,
            expires_unix,
        };
        let download_candidate = RelayCandidate {
            kind: RelayCandidateKind::Relay,
            relay_id: self.config.relay_id.clone(),
            ticket_id: download_ticket,
            expires_unix,
        };
        upload_candidate
            .validate(now_unix)
            .map_err(|_| RelayError::UnsafeIdentifier)?;
        download_candidate
            .validate(now_unix)
            .map_err(|_| RelayError::UnsafeIdentifier)?;

        let upload_request = ProviderCredentialRequest {
            relay_id: self.config.relay_id.clone(),
            session_id: session_id.clone(),
            object_sha256: seed.object.object_sha256.clone(),
            participant_alias: upload_alias,
            expires_unix,
            max_bytes: seed.object.encoded_size,
        };
        let download_request = ProviderCredentialRequest {
            relay_id: self.config.relay_id.clone(),
            session_id: session_id.clone(),
            object_sha256: seed.object.object_sha256.clone(),
            participant_alias: download_alias,
            expires_unix,
            max_bytes: seed.object.encoded_size,
        };
        upload_request.validate(now_unix)?;
        download_request.validate(now_unix)?;
        let upload_lease = self
            .provider
            .issue(&upload_request, now_unix)
            .map_err(|_| RelayError::ProviderUnavailable)?;
        let download_lease = match self.provider.issue(&download_request, now_unix) {
            Ok(lease) => lease,
            Err(_) => {
                let _ = self.provider.revoke(&upload_lease.revocation_id);
                return Err(RelayError::ProviderUnavailable);
            }
        };
        if upload_lease.access.expires_unix() < expires_unix
            || download_lease.access.expires_unix() < expires_unix
        {
            let _ = self.provider.revoke(&upload_lease.revocation_id);
            let _ = self.provider.revoke(&download_lease.revocation_id);
            return Err(RelayError::ProviderRejected);
        }

        let ProviderCredentialLease {
            access: upload_access,
            revocation_id: upload_revocation_id,
        } = upload_lease;
        let ProviderCredentialLease {
            access: download_access,
            revocation_id: download_revocation_id,
        } = download_lease;
        let grant = RelaySessionGrant {
            session_id: session_id.clone(),
            object_sha256: seed.object.object_sha256.clone(),
            encoded_size: seed.object.encoded_size,
            upload: ParticipantRelayGrant {
                candidate: upload_candidate,
                credential: upload_credential,
                provider_access: upload_access,
            },
            download: ParticipantRelayGrant {
                candidate: download_candidate,
                credential: download_credential,
                provider_access: download_access,
            },
            seed_dispatch_subject: seed.subject.clone(),
        };
        let _ = requester; // requester's account ID never enters the grant.
        Ok((
            session_id,
            upload_fingerprint,
            download_fingerprint,
            upload_revocation_id,
            download_revocation_id,
            grant,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn sign_credential(
        &self,
        session_id: &str,
        alias: &str,
        object: &VerifiedRelayObject,
        direction: RelayDirection,
        now_unix: i64,
        expires_unix: i64,
        max_chunks: u32,
    ) -> Result<SignedRelayCredential, RelayError> {
        sign_relay_credential(
            RelayCredentialClaims {
                schema: rw_community_protocol::RELAY_CREDENTIAL_SCHEMA.into(),
                relay_id: self.config.relay_id.clone(),
                session_id: session_id.into(),
                subject_id: alias.into(),
                object_sha256: object.object_sha256.clone(),
                direction,
                issued_unix: now_unix,
                not_before_unix: now_unix,
                expires_unix,
                max_bytes: object.encoded_size,
                max_chunks,
            },
            self.config.signing_key_id.clone(),
            &self.relay_signing_key,
            now_unix,
            &self.limits,
        )
        .map_err(|_| RelayError::CredentialInvalid)
    }

    fn admission_gate(&mut self, period: BillingPeriod) -> Result<(), RelayError> {
        self.roll_period(period);
        self.config.enabled()?;
        if self.kill_switch {
            return Err(RelayError::Disabled);
        }
        if self.global_relayed.saturating_add(self.global_reserved)
            >= self.config.quotas.cost_stop_after_bytes_per_month
        {
            return Err(RelayError::CostThresholdReached);
        }
        Ok(())
    }

    fn select_seed(
        &self,
        requester: &AuthenticatedSubject,
        object_sha256: &str,
    ) -> Option<&SeedEntry> {
        self.seeds.get(object_sha256)?.iter().find(|entry| {
            &entry.subject != requester
                && entry.policy.permits(&entry.object).is_ok()
                && self.seed_has_quota(&entry.subject, entry.object.encoded_size)
        })
    }

    fn seed_has_quota(&self, subject: &AuthenticatedSubject, bytes: u64) -> bool {
        let usage = self.usage.get(subject).copied().unwrap_or_default();
        usage.active_uploads < self.config.quotas.per_user_concurrency
            && usage
                .uploaded
                .saturating_add(usage.reserved_upload)
                .saturating_add(bytes)
                <= self.config.quotas.per_user_upload_bytes_per_month
    }

    fn reserve(
        &mut self,
        upload: &AuthenticatedSubject,
        download: &AuthenticatedSubject,
        bytes: u64,
    ) -> Result<(), RelayError> {
        let upload_usage = self.usage.get(upload).copied().unwrap_or_default();
        let download_usage = self.usage.get(download).copied().unwrap_or_default();
        if upload_usage.active_uploads >= self.config.quotas.per_user_concurrency
            || download_usage.active_downloads >= self.config.quotas.per_user_concurrency
            || self.pending.len() >= self.config.quotas.global_concurrency as usize
            || upload_usage
                .uploaded
                .saturating_add(upload_usage.reserved_upload)
                .saturating_add(bytes)
                > self.config.quotas.per_user_upload_bytes_per_month
            || download_usage
                .downloaded
                .saturating_add(download_usage.reserved_download)
                .saturating_add(bytes)
                > self.config.quotas.per_user_download_bytes_per_month
            || self
                .global_relayed
                .saturating_add(self.global_reserved)
                .saturating_add(bytes)
                > self.config.quotas.global_relay_bytes_per_month
        {
            return Err(RelayError::QuotaReached);
        }
        if self
            .global_relayed
            .saturating_add(self.global_reserved)
            .saturating_add(bytes)
            > self.config.quotas.cost_stop_after_bytes_per_month
        {
            return Err(RelayError::CostThresholdReached);
        }
        let upload_usage = self.usage.entry(upload.clone()).or_default();
        upload_usage.reserved_upload += bytes;
        upload_usage.active_uploads += 1;
        let download_usage = self.usage.entry(download.clone()).or_default();
        download_usage.reserved_download += bytes;
        download_usage.active_downloads += 1;
        self.global_reserved += bytes;
        Ok(())
    }

    fn release_reservation(
        &mut self,
        upload: &AuthenticatedSubject,
        download: &AuthenticatedSubject,
        bytes: u64,
    ) {
        let upload_usage = self.usage.entry(upload.clone()).or_default();
        upload_usage.reserved_upload = upload_usage.reserved_upload.saturating_sub(bytes);
        upload_usage.active_uploads = upload_usage.active_uploads.saturating_sub(1);
        let download_usage = self.usage.entry(download.clone()).or_default();
        download_usage.reserved_download = download_usage.reserved_download.saturating_sub(bytes);
        download_usage.active_downloads = download_usage.active_downloads.saturating_sub(1);
        self.global_reserved = self.global_reserved.saturating_sub(bytes);
    }

    fn commit_reserved_session(&mut self, pending: &PendingSession) {
        self.release_reservation(
            &pending.upload_subject,
            &pending.download_subject,
            pending.expected_bytes,
        );
        // The backend cannot observe authoritative TURN payload byte counts.
        // Charge the entire signed reservation on every terminal path
        // (success, failure, expiry, or kill switch) until provider analytics
        // can reconcile downward in a future durable integration. A malicious
        // pair therefore gains nothing by reporting zero bytes or expiring.
        let upload = self
            .usage
            .entry(pending.upload_subject.clone())
            .or_default();
        upload.uploaded = upload.uploaded.saturating_add(pending.expected_bytes);
        let download = self
            .usage
            .entry(pending.download_subject.clone())
            .or_default();
        download.downloaded = download.downloaded.saturating_add(pending.expected_bytes);
        self.global_relayed = self.global_relayed.saturating_add(pending.expected_bytes);
    }

    fn revoke_provider_credentials(&mut self, pending: &PendingSession) {
        let _ = self.provider.revoke(&pending.upload_revocation_id);
        let _ = self.provider.revoke(&pending.download_revocation_id);
    }

    fn mark_revoked(&mut self, pending: &PendingSession) {
        self.revoked_credentials.insert(
            pending.upload_credential_fingerprint.clone(),
            pending.expires_unix,
        );
        self.revoked_credentials.insert(
            pending.download_credential_fingerprint.clone(),
            pending.expires_unix,
        );
    }

    fn reap_expired_sessions(&mut self, now_unix: i64) {
        self.revoked_credentials
            .retain(|_, expires_unix| *expires_unix > now_unix);
        let expired = self
            .pending
            .iter()
            .filter_map(|(session_id, session)| {
                (session.expires_unix <= now_unix).then_some(session_id.clone())
            })
            .collect::<Vec<_>>();
        for session_id in expired {
            if let Some(session) = self.pending.remove(&session_id) {
                self.commit_reserved_session(&session);
                self.revoke_provider_credentials(&session);
                self.mark_revoked(&session);
            }
        }
    }

    fn remove_expired(&mut self, now_unix: i64) {
        let mut storage_releases = Vec::new();
        self.seeds.retain(|_, entries| {
            entries.retain(|entry| {
                let keep = entry.object.expires_unix > now_unix;
                if !keep {
                    storage_releases.push((entry.subject.clone(), entry.object.encoded_size));
                }
                keep
            });
            !entries.is_empty()
        });
        for (subject, bytes) in storage_releases {
            let usage = self.usage.entry(subject).or_default();
            usage.advertised_storage = usage.advertised_storage.saturating_sub(bytes);
        }
    }

    fn roll_period(&mut self, period: BillingPeriod) {
        if period <= self.period {
            return;
        }
        self.period = period;
        self.global_relayed = 0;
        for usage in self.usage.values_mut() {
            usage.uploaded = 0;
            usage.downloaded = 0;
        }
        // Reservations and active counts span a month boundary until their
        // sessions close; they are never erased by rollover.
    }

    fn fallback(&self, archival_available: bool, error: RelayError) -> ColdLookupOutcome {
        ColdLookupOutcome::Fallback(PublicRelayFailure::new(
            error,
            fallback_target(archival_available),
        ))
    }
}

fn fallback_target(archival_available: bool) -> FallbackTarget {
    if archival_available {
        FallbackTarget::ArchivalHttpsOrigin
    } else {
        FallbackTarget::Unavailable
    }
}

fn add_reservation(
    reservations: &mut BTreeMap<AuthenticatedSubject, (u64, u32)>,
    subject: AuthenticatedSubject,
    bytes: u64,
) -> Result<(), RelayError> {
    let value = reservations.entry(subject).or_default();
    value.0 = value
        .0
        .checked_add(bytes)
        .ok_or(RelayError::PersistenceRejected)?;
    value.1 = value
        .1
        .checked_add(1)
        .ok_or(RelayError::PersistenceRejected)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rw_community_protocol::{
        Compression, DataOrigin, EncryptedRelayEnvelope, EndToEndCipher, MissingPolicy,
        OBJECT_SCHEMA, ObjectManifest, PublicationGrant, REQUEST_SCHEMA, RecipeIdentity,
        ShareRequest, SourceProvenance, TimeWindow, object_sha256, sign_object_manifest,
    };

    use super::*;
    use crate::CloudflareTurnAdapter;

    #[derive(Default)]
    struct DeterministicIds(u64);

    impl OpaqueIdSource for DeterministicIds {
        fn next_id(&mut self, kind: OpaqueIdKind) -> Result<String, RelayError> {
            self.0 += 1;
            let prefix = match kind {
                OpaqueIdKind::Advertisement => "advert",
                OpaqueIdKind::Session => "session",
                OpaqueIdKind::Ticket => "ticket",
                OpaqueIdKind::ParticipantAlias => "alias",
            };
            Ok(format!("{prefix}-{:08}", self.0))
        }
    }

    struct FakeProvider {
        fail: bool,
        issued: u64,
        revoked: u64,
    }

    impl RelayProvider for FakeProvider {
        fn issue(
            &mut self,
            request: &ProviderCredentialRequest,
            now_unix: i64,
        ) -> Result<ProviderCredentialLease, RelayError> {
            request.validate(now_unix)?;
            if self.fail {
                return Err(RelayError::ProviderUnavailable);
            }
            self.issued += 1;
            let json = br#"{"iceServers":[
                {"urls":"stun:stun.cloudflare.com:3478"},
                {"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"}
            ]}"#;
            CloudflareTurnAdapter::default().parse_and_sanitize(
                json,
                now_unix,
                request.expires_unix,
            )
        }

        fn revoke(&mut self, _revocation_id: &SecretText) -> Result<(), RelayError> {
            self.revoked += 1;
            Ok(())
        }
    }

    fn hash(fill: char) -> String {
        std::iter::repeat_n(fill, 64).collect()
    }

    fn signed_object(
        origin_key: &SigningKey,
        data_origin: DataOrigin,
        owner_published: bool,
        rights: bool,
        bytes: &[u8],
    ) -> SignedObjectManifest {
        let request = ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812T00Z".into(),
            snapshot_id: hash('a'),
            grid_hash: hash('b'),
            variables: vec!["temperature_2m".into()],
            query: ShareQuery::PointSeries {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                window: TimeWindow::Utc {
                    start_unix: 100,
                    end_unix: 200,
                },
                missing_policy: MissingPolicy::Strict,
            },
            recipe: RecipeIdentity {
                recipe_id: "point-series".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-public".into(),
                forecast_producer: None,
                licensing_publisher: None,
                transport_provider: None,
                transport_is_mirror: false,
                roles: vec!["surface".into()],
                products: vec!["hrrr-sfc".into()],
            }],
            publication: PublicationGrant {
                data_origin,
                explicit_owner_publication: owner_published,
                redistribution_rights_confirmed: rights,
            },
        };
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request_sha256: request_sha256(&request).unwrap(),
            request,
            object_sha256: object_sha256(bytes),
            content_type: "application/json".into(),
            compression: Compression::None,
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            attributions: Vec::new(),
            modification_notices: vec!["Subset by Rusty Weather".into()],
            created_unix: 100,
            expires_unix: 10_000,
        };
        sign_object_manifest(manifest, "origin-signing", origin_key).unwrap()
    }

    fn config(limit: u64) -> RelayControlConfig {
        RelayControlConfig {
            phase2_enabled: true,
            security_tests_passed: true,
            capacity_audit_complete: true,
            provider_pricing_verified: true,
            relay_id: "cloudflare-turn".into(),
            signing_key_id: "relay-signing".into(),
            credential_lifetime_seconds: 600,
            max_chunk_plaintext_bytes: crate::RELAY_PLAINTEXT_CHUNK_BYTES,
            quotas: RelayQuotaPolicy {
                per_user_upload_bytes_per_month: limit,
                per_user_download_bytes_per_month: limit,
                per_user_advertised_storage_bytes: limit,
                per_user_concurrency: 1,
                global_concurrency: 16,
                global_relay_bytes_per_month: limit,
                cost_stop_after_bytes_per_month: limit,
            },
            promotion: PromotionPolicy {
                successful_recoveries: 2,
                relayed_bytes: limit,
            },
        }
    }

    fn policy(limit: u64) -> ClientSeedingPolicy {
        ClientSeedingPolicy {
            opted_in: true,
            categories: BTreeSet::from([RelayObjectCategory::PointSeries]),
            disk_allowance_bytes: limit,
            upload_allowance_bytes: limit,
            metered_network: false,
            allow_metered_seeding: false,
        }
    }

    fn retrieval(limit: u64) -> ClientRetrievalPolicy {
        ClientRetrievalPolicy {
            opted_in: true,
            download_allowance_bytes: limit,
        }
    }

    fn coordinator(
        limit: u64,
        provider_fail: bool,
    ) -> (RelayCoordinator<FakeProvider, DeterministicIds>, SigningKey) {
        let origin_key = SigningKey::from_bytes(&[1; 32]);
        let origin_keys = BTreeMap::from([("origin-signing".into(), origin_key.verifying_key())]);
        let relay_key = SigningKey::from_bytes(&[2; 32]);
        let coordinator = RelayCoordinator::new(
            config(limit),
            ProtocolLimits::default(),
            origin_keys,
            relay_key,
            FakeProvider {
                fail: provider_fail,
                issued: 0,
                revoked: 0,
            },
            DeterministicIds::default(),
            BillingPeriod::new(2026, 8).unwrap(),
        )
        .unwrap();
        (coordinator, origin_key)
    }

    #[test]
    fn availability_requires_exact_origin_signed_hash_and_private_grants() {
        let bytes = b"point-series";
        let (mut coordinator, origin_key) = coordinator(1024, false);
        let seed = AuthenticatedSubject::new("seed-account@private.invalid").unwrap();
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                seed.clone(),
                &signed,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();

        let mut tampered = signed.clone();
        tampered.manifest.object_sha256 = hash('f');
        assert_eq!(
            coordinator.advertise(
                seed.clone(),
                &tampered,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::UntrustedObject)
        );

        // A private run is eligible only when those same facts are present in
        // the origin-signed canonical request. This is an explicit publication
        // grant, not a local cache or opt-in side effect.
        let published_private = signed_object(
            &origin_key,
            DataOrigin::PrivateArwen,
            true,
            true,
            b"explicit-private-publication",
        );
        coordinator
            .advertise(
                AuthenticatedSubject::new("private-owner").unwrap(),
                &published_private,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();

        let mut private = signed.clone();
        private.manifest.request.publication = PublicationGrant {
            data_origin: DataOrigin::PrivateWrf,
            explicit_owner_publication: false,
            redistribution_rights_confirmed: true,
        };
        assert_eq!(
            coordinator.advertise(
                seed,
                &private,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::UntrustedObject)
        );
    }

    #[test]
    fn advertisement_over_persistence_budget_is_rejected_without_mutation() {
        let bytes = b"point-series";
        let (mut coordinator, origin_key) = coordinator(1024, false);
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        let empty_bytes = coordinator.export_persistence_json().unwrap();
        coordinator.set_persistence_maximum_bytes_for_test(empty_bytes.len());

        assert_eq!(
            coordinator.advertise(
                AuthenticatedSubject::new("bounded-seed").unwrap(),
                &signed,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::PersistenceRejected)
        );
        assert!(coordinator.seeds.is_empty());
        assert!(coordinator.usage.is_empty());
        assert!(!coordinator.kill_switch());
        assert_eq!(coordinator.export_persistence_json().unwrap(), empty_bytes);

        coordinator.set_persistence_maximum_bytes_for_test(MAX_PERSISTENCE_BYTES);
        coordinator
            .advertise(
                AuthenticatedSubject::new("bounded-seed").unwrap(),
                &signed,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
    }

    #[test]
    fn relay_grants_expose_only_opaque_candidates_and_scoped_credentials() {
        let bytes = b"point-series";
        let (mut coordinator, origin_key) = coordinator(1024, false);
        let seed = AuthenticatedSubject::new("192.0.2.10 seed internal").unwrap();
        let requester = AuthenticatedSubject::new("198.51.100.20 requester internal").unwrap();
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                seed,
                &signed,
                policy(1024),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        let grant = match coordinator.begin_cold_lookup(
            requester,
            retrieval(1024),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            ColdLookupOutcome::Fallback(failure) => panic!("unexpected fallback: {failure:?}"),
        };
        assert_eq!(grant.upload.candidate.kind, RelayCandidateKind::Relay);
        assert_eq!(grant.download.candidate.kind, RelayCandidateKind::Relay);
        assert_eq!(
            grant.upload.credential.claims.direction,
            RelayDirection::Upload
        );
        assert_eq!(
            grant.download.credential.claims.direction,
            RelayDirection::Download
        );
        assert_eq!(
            grant.upload.credential.claims.object_sha256,
            object_sha256(bytes)
        );
        assert_eq!(
            grant.download.credential.claims.object_sha256,
            object_sha256(bytes)
        );
        let visible = format!(
            "{}{}{}{}",
            serde_json::to_string(&grant.upload.candidate).unwrap(),
            serde_json::to_string(&grant.download.candidate).unwrap(),
            serde_json::to_string(&grant.upload.credential).unwrap(),
            serde_json::to_string(&grant.download.credential).unwrap(),
        );
        assert!(!visible.contains("192.0.2.10"));
        assert!(!visible.contains("198.51.100.20"));
        assert!(!visible.contains("address"));
        assert!(!visible.contains("hostname"));
        let debug = format!("{grant:?}");
        assert!(!debug.contains("192.0.2.10"));
        assert!(!debug.contains("198.51.100.20"));
        coordinator
            .verify_active_credential(&grant.download.credential, 200)
            .unwrap();
        let envelope = EncryptedRelayEnvelope {
            schema: rw_community_protocol::RELAY_ENVELOPE_SCHEMA.into(),
            session_id: grant.session_id.clone(),
            object_sha256: object_sha256(bytes),
            cipher: EndToEndCipher::XChaCha20Poly1305,
            chunk_index: 0,
            chunk_count: 1,
            plaintext_size: bytes.len() as u32,
            nonce_base64: base64::engine::general_purpose::STANDARD.encode([1_u8; 24]),
            ciphertext_base64: base64::engine::general_purpose::STANDARD.encode(vec![
                2_u8;
                bytes.len()
                    + 16
            ]),
        };
        coordinator
            .admit_envelope(&grant.download.credential, &envelope, 200)
            .unwrap();
        let mut overflow = envelope.clone();
        overflow.plaintext_size = bytes.len() as u32;
        assert_eq!(
            coordinator.admit_envelope(&grant.download.credential, &overflow, 200),
            Err(RelayError::QuotaReached)
        );
        coordinator
            .complete_session(
                &grant.session_id,
                bytes.len() as u64,
                true,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        assert_eq!(
            coordinator.verify_active_credential(&grant.download.credential, 201),
            Err(RelayError::CredentialRevoked)
        );
        assert_eq!(
            coordinator.verify_active_credential(&grant.download.credential, 800),
            Err(RelayError::CredentialExpired)
        );
    }

    #[test]
    fn metered_quota_kill_switch_and_monthly_rollover_are_fail_closed() {
        let bytes = b"12345678";
        let (mut coordinator, origin_key) = coordinator(8, false);
        let seed = AuthenticatedSubject::new("seed").unwrap();
        let requester = AuthenticatedSubject::new("requester").unwrap();
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        let mut metered = policy(8);
        metered.metered_network = true;
        assert_eq!(
            coordinator.advertise(
                seed.clone(),
                &signed,
                metered,
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::MeteredNetworkPaused)
        );
        coordinator
            .advertise(
                seed,
                &signed,
                policy(8),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        match coordinator.begin_cold_lookup(
            requester.clone(),
            ClientRetrievalPolicy::default(),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Fallback(failure) => {
                assert_eq!(failure.code, "relay_policy_denied");
            }
            _ => panic!("passive/default-off lookup must not enter the relay"),
        }
        let session = match coordinator.begin_cold_lookup(
            requester.clone(),
            retrieval(8),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected relay"),
        };
        coordinator
            .complete_session(
                &session.session_id,
                bytes.len() as u64,
                true,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        match coordinator.begin_cold_lookup(
            requester.clone(),
            retrieval(8),
            &object_sha256(bytes),
            true,
            300,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Fallback(failure) => {
                assert!(matches!(
                    failure.code.as_str(),
                    "relay_cost_threshold" | "relay_not_available"
                ));
                assert_eq!(failure.fallback, FallbackTarget::ArchivalHttpsOrigin);
            }
            _ => panic!("monthly quota must fall back"),
        }
        assert!(matches!(
            coordinator.begin_cold_lookup(
                requester,
                retrieval(8),
                &object_sha256(bytes),
                true,
                400,
                BillingPeriod::new(2026, 9).unwrap(),
            ),
            ColdLookupOutcome::Relay(_)
        ));
        coordinator.set_kill_switch(true);
        assert!(coordinator.kill_switch());
        // Provider credentials were issued in the new billing period. The
        // backend cannot prove that zero TURN bytes moved before revocation,
        // so the kill switch commits the complete reservation.
        assert_eq!(coordinator.global_relayed, bytes.len() as u64);
        match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("someone-else").unwrap(),
            retrieval(8),
            &object_sha256(bytes),
            true,
            500,
            BillingPeriod::new(2026, 9).unwrap(),
        ) {
            ColdLookupOutcome::Fallback(failure) => {
                assert_eq!(failure.code, "relay_disabled");
                assert_eq!(failure.fallback, FallbackTarget::ArchivalHttpsOrigin);
            }
            _ => panic!("kill switch must force HTTPS fallback"),
        }
    }

    #[test]
    fn success_requires_matching_completion_from_both_signed_roles() {
        let bytes = b"two-role-completion";
        let (mut coordinator, origin_key) = coordinator(4096, false);
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                AuthenticatedSubject::new("seed").unwrap(),
                &signed,
                policy(4096),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        let session = match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("requester").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected relay"),
        };
        assert!(matches!(
            coordinator
                .report_participant_completion(
                    &session.session_id,
                    RelayRole::Downloader,
                    bytes.len() as u64,
                    BillingPeriod::new(2026, 8).unwrap(),
                )
                .unwrap(),
            ParticipantCompletionResult::AwaitingCounterpart
        ));
        assert!(coordinator.pending.contains_key(&session.session_id));
        assert!(coordinator.popularity.is_empty());
        assert!(matches!(
            coordinator.report_participant_completion(
                &session.session_id,
                RelayRole::Uploader,
                bytes.len() as u64 - 1,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::ObjectMismatch)
        ));
        let result = coordinator
            .report_participant_completion(
                &session.session_id,
                RelayRole::Uploader,
                bytes.len() as u64,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        assert!(matches!(result, ParticipantCompletionResult::Complete(_)));
        assert!(!coordinator.pending.contains_key(&session.session_id));
        assert_eq!(
            coordinator
                .popularity
                .get(&object_sha256(bytes))
                .unwrap()
                .successful_recoveries,
            1
        );

        let lost_counterpart = match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("requester-two").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            true,
            300,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected second relay"),
        };
        assert!(matches!(
            coordinator
                .report_participant_completion(
                    &lost_counterpart.session_id,
                    RelayRole::Uploader,
                    bytes.len() as u64,
                    BillingPeriod::new(2026, 8).unwrap(),
                )
                .unwrap(),
            ParticipantCompletionResult::AwaitingCounterpart
        ));
        coordinator.fail_and_fallback(
            &lost_counterpart.session_id,
            0,
            BillingPeriod::new(2026, 8).unwrap(),
        );
        assert_eq!(
            coordinator
                .popularity
                .get(&object_sha256(bytes))
                .unwrap()
                .successful_recoveries,
            1,
            "a lost counterpart completion must never count success"
        );
    }

    #[test]
    fn relay_failure_immediately_falls_back_and_popularity_promotes_to_r2() {
        let bytes = b"popular";
        let (mut coordinator, origin_key) = coordinator(4096, true);
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                AuthenticatedSubject::new("seed").unwrap(),
                &signed,
                policy(4096),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("requester").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Fallback(failure) => {
                assert_eq!(failure.code, "relay_provider_unavailable");
                assert_eq!(failure.fallback, FallbackTarget::ArchivalHttpsOrigin);
            }
            _ => panic!("provider failure must immediately fall back"),
        }

        coordinator.provider.fail = false;
        let failed = match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("failed-requester").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            true,
            250,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected relay before injected transfer failure"),
        };
        let failure = coordinator.fail_and_fallback(
            &failed.session_id,
            // A malicious pair can claim zero because TURN payloads bypass
            // the backend. Accounting must still charge the signed maximum.
            0,
            BillingPeriod::new(2026, 8).unwrap(),
        );
        assert_eq!(failure.fallback, FallbackTarget::ArchivalHttpsOrigin);
        assert_eq!(coordinator.global_relayed, bytes.len() as u64);

        let first = match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("requester-1").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            false,
            300,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected first relay"),
        };
        let first_result = coordinator
            .complete_session(
                &first.session_id,
                bytes.len() as u64,
                true,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        assert!(first_result.promotion.is_none());
        let second = match coordinator.begin_cold_lookup(
            AuthenticatedSubject::new("requester-2").unwrap(),
            retrieval(4096),
            &object_sha256(bytes),
            false,
            400,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected second relay"),
        };
        let second_result = coordinator
            .complete_session(
                &second.session_id,
                bytes.len() as u64,
                true,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        let promotion = second_result.promotion.unwrap();
        assert_eq!(promotion.object_sha256, object_sha256(bytes));
        assert_eq!(promotion.successful_recoveries, 2);
    }

    #[test]
    fn expired_provider_credentials_commit_the_full_reserved_maximum() {
        let bytes = b"expired-session";
        let (mut coordinator, origin_key) = coordinator(4096, false);
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                AuthenticatedSubject::new("seed").unwrap(),
                &signed,
                policy(4096),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        assert!(matches!(
            coordinator.begin_cold_lookup(
                AuthenticatedSubject::new("requester").unwrap(),
                retrieval(4096),
                &object_sha256(bytes),
                true,
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            ColdLookupOutcome::Relay(_)
        ));
        assert_eq!(coordinator.global_relayed, 0);
        assert_eq!(coordinator.global_reserved, bytes.len() as u64);

        // Any subsequent control-plane operation reaps expired sessions. No
        // completion report is required (or trusted) to make usage durable.
        coordinator
            .advertise(
                AuthenticatedSubject::new("second-seed").unwrap(),
                &signed,
                policy(4096),
                801,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        assert_eq!(coordinator.global_reserved, 0);
        assert_eq!(coordinator.global_relayed, bytes.len() as u64);
        assert_eq!(coordinator.provider.revoked, 2);
    }

    #[test]
    fn persistence_restores_advertisements_and_terminally_charges_live_sessions() {
        let bytes = b"restart-object";
        let (mut active, origin_key) = coordinator(4096, false);
        let seed = AuthenticatedSubject::new("seed-principal-hash").unwrap();
        let requester = AuthenticatedSubject::new("requester-principal-hash").unwrap();
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        active
            .advertise(
                seed.clone(),
                &signed,
                policy(4096),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        let grant = match active.begin_cold_lookup(
            requester,
            retrieval(4096),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected relay"),
        };
        let interrupted_credential = grant.download.credential.clone();
        let snapshot = active.export_persistence_json().unwrap();
        let persisted = String::from_utf8(snapshot.clone()).unwrap();
        for forbidden in [
            "turn.cloudflare.com",
            "iceServers",
            "provider_access",
            "username",
            "password",
            "short-lived-secret",
            "seed-principal-hash",
            "requester-principal-hash",
        ] {
            assert!(!persisted.contains(forbidden));
        }

        let (mut restored, _) = coordinator(4096, false);
        restored
            .restore_persistence_json(&snapshot, 201, BillingPeriod::new(2026, 8).unwrap())
            .unwrap();
        assert!(restored.pending.is_empty());
        assert_eq!(restored.global_reserved, 0);
        assert_eq!(restored.global_relayed, bytes.len() as u64);
        assert_eq!(
            restored.verify_active_credential(&interrupted_credential, 201),
            Err(RelayError::CredentialRevoked)
        );
        // The signed advertisement remains useful after restart, but the seed
        // starts from its conservatively charged monthly upload usage.
        assert!(restored.seeds.contains_key(&object_sha256(bytes)));
        assert!(matches!(
            restored.begin_cold_lookup(
                AuthenticatedSubject::new("another-requester").unwrap(),
                retrieval(4096),
                &object_sha256(bytes),
                false,
                202,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            ColdLookupOutcome::Relay(_)
        ));

        let mut corrupt: serde_json::Value = serde_json::from_slice(&snapshot).unwrap();
        corrupt["global_reserved"] = serde_json::json!(0);
        let (mut rejected, _) = coordinator(4096, false);
        assert_eq!(
            rejected.restore_persistence_json(
                &serde_json::to_vec(&corrupt).unwrap(),
                201,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::PersistenceRejected)
        );
        let (mut incompatible, _) = coordinator(8192, false);
        assert_eq!(
            incompatible.restore_persistence_json(
                &snapshot,
                201,
                BillingPeriod::new(2026, 8).unwrap(),
            ),
            Err(RelayError::PersistenceRejected)
        );
    }

    #[test]
    fn transport_routes_are_role_subject_offer_and_provider_range_bound() {
        let bytes = b"route-object";
        let (mut coordinator, origin_key) = coordinator(4096, false);
        let seed = AuthenticatedSubject::new("seed-principal-hash").unwrap();
        let requester = AuthenticatedSubject::new("requester-principal-hash").unwrap();
        let signed = signed_object(&origin_key, DataOrigin::PublicProvider, false, true, bytes);
        coordinator
            .advertise(
                seed.clone(),
                &signed,
                policy(4096),
                200,
                BillingPeriod::new(2026, 8).unwrap(),
            )
            .unwrap();
        let grant = match coordinator.begin_cold_lookup(
            requester.clone(),
            retrieval(4096),
            &object_sha256(bytes),
            true,
            200,
            BillingPeriod::new(2026, 8).unwrap(),
        ) {
            ColdLookupOutcome::Relay(grant) => grant,
            _ => panic!("expected relay"),
        };
        let upload_key = crate::EphemeralKeyPair::generate();
        let download_key = crate::EphemeralKeyPair::generate();
        let upload_offer = upload_key
            .offer(
                &grant.upload.credential,
                RelayRole::Uploader,
                200,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let download_offer = download_key
            .offer(
                &grant.download.credential,
                RelayRole::Downloader,
                200,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let route_policy = crate::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let mut routes = crate::RelayRouteRegistry::new(route_policy.clone());

        assert_eq!(
            routes.register(
                &coordinator,
                &requester,
                &grant.upload.credential,
                upload_offer.clone(),
                "104.16.0.7:49152",
                200,
            ),
            Err(RelayError::CredentialInvalid)
        );
        assert_eq!(
            routes.register(
                &coordinator,
                &seed,
                &grant.upload.credential,
                upload_offer.clone(),
                "198.51.100.7:49152",
                200,
            ),
            Err(RelayError::PolicyDenied)
        );
        let first = routes
            .register(
                &coordinator,
                &seed,
                &grant.upload.credential,
                upload_offer.clone(),
                "104.16.0.7:49152",
                200,
            )
            .unwrap();
        assert!(!first.binding_ready);
        assert_eq!(
            routes.register(
                &coordinator,
                &seed,
                &grant.upload.credential,
                upload_offer,
                "104.16.0.9:49152",
                200,
            ),
            Err(RelayError::Replay)
        );
        assert_eq!(
            routes.register(
                &coordinator,
                &seed,
                &grant.download.credential,
                download_offer.clone(),
                "104.16.0.8:49152",
                200,
            ),
            Err(RelayError::CredentialInvalid)
        );
        assert_eq!(
            routes.register(
                &coordinator,
                &requester,
                &grant.download.credential,
                download_offer.clone(),
                // Reusing the uploader's provider allocation is a route
                // substitution and must not poison the valid retry below.
                "104.16.0.7:49152",
                200,
            ),
            Err(RelayError::KeyAgreementRejected)
        );
        let second = routes
            .register(
                &coordinator,
                &requester,
                &grant.download.credential,
                download_offer,
                "104.16.0.8:49152",
                200,
            )
            .unwrap();
        assert!(second.binding_ready);

        let upload_transport = routes
            .participant_grant(
                &coordinator,
                &seed,
                &grant.upload.credential,
                RelayRole::Uploader,
                200,
            )
            .unwrap();
        let upload_bytes = upload_transport.transport_json().unwrap();
        let upload_json = String::from_utf8(upload_bytes.clone()).unwrap();
        assert!(upload_json.contains("104.16.0.8:49152"));
        assert!(!upload_json.contains("104.16.0.7:49152"));
        assert!(!upload_json.contains("seed-principal-hash"));
        assert!(!upload_json.contains("requester-principal-hash"));
        let relay_keys = BTreeMap::from([(
            "relay-signing".into(),
            SigningKey::from_bytes(&[2; 32]).verifying_key(),
        )]);
        crate::verify_signed_session_binding(
            upload_transport.signed_binding(),
            &grant.upload.credential,
            &grant.download.credential,
            200,
            &relay_keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        let (wire, peer_route, _verified) = crate::parse_transport_route_bounded(
            &upload_bytes,
            crate::TransportRouteExpectation {
                session_id: &grant.session_id,
                role: RelayRole::Uploader,
                own_credential: &grant.upload.credential,
                object_sha256: &object_sha256(bytes),
                encoded_size: bytes.len() as u64,
                now_unix: 200,
                trusted_relay_keys: &relay_keys,
                limits: &ProtocolLimits::default(),
                policy: &route_policy,
            },
        )
        .unwrap();
        assert_eq!(wire.peer_credential, grant.download.credential);
        assert!(!format!("{peer_route:?} {wire:?}").contains("104.16"));

        let mut cross_session: serde_json::Value = serde_json::from_slice(&upload_bytes).unwrap();
        cross_session["peer_credential"]["claims"]["session_id"] =
            serde_json::json!("substituted-session");
        assert!(
            crate::parse_transport_route_bounded(
                &serde_json::to_vec(&cross_session).unwrap(),
                crate::TransportRouteExpectation {
                    session_id: &grant.session_id,
                    role: RelayRole::Uploader,
                    own_credential: &grant.upload.credential,
                    object_sha256: &object_sha256(bytes),
                    encoded_size: bytes.len() as u64,
                    now_unix: 200,
                    trusted_relay_keys: &relay_keys,
                    limits: &ProtocolLimits::default(),
                    policy: &route_policy,
                },
            )
            .is_err()
        );
        assert!(matches!(
            routes.participant_grant(
                &coordinator,
                &requester,
                &grant.upload.credential,
                RelayRole::Uploader,
                200,
            ),
            Err(RelayError::CredentialInvalid)
        ));
    }

    #[test]
    fn operational_path_has_no_relay_and_public_state_has_no_peer_fields() {
        assert_eq!(
            crate::after_operational_r2_miss(),
            crate::OperationalFallback::HetznerHttpsOrigin
        );
        let failure = PublicRelayFailure::new(
            RelayError::ProviderUnavailable,
            FallbackTarget::ArchivalHttpsOrigin,
        );
        let audit = RelayAuditEvent {
            schema: AUDIT_EVENT_SCHEMA.into(),
            kind: RelayAuditKind::SessionFailed,
            session_id: Some("session-opaque".into()),
            object_sha256: Some(hash('a')),
            failure_code: Some("relay_provider_unavailable".into()),
        };
        let visible = format!(
            "{}{}",
            serde_json::to_string(&failure).unwrap(),
            serde_json::to_string(&audit).unwrap()
        );
        for forbidden in [
            "peer_ip",
            "peer_host",
            "address",
            "candidate_ip",
            "srflx",
            "prflx",
            "direct",
        ] {
            assert!(!visible.contains(forbidden));
        }
        for candidate_kind in ["host", "srflx", "prflx", "direct"] {
            let candidate = format!(
                r#"{{"kind":"{candidate_kind}","relay_id":"relay-one","ticket_id":"ticket-one","expires_unix":900}}"#
            );
            assert!(serde_json::from_str::<RelayCandidate>(&candidate).is_err());
        }
        let address_bearing = r#"{"kind":"relay","relay_id":"relay-one","ticket_id":"ticket-one","expires_unix":900,"address":"203.0.113.9:3478"}"#;
        assert!(serde_json::from_str::<RelayCandidate>(address_bearing).is_err());
    }
}

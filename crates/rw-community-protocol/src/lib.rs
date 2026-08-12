//! Transport-neutral contract for BowEcho's opt-in Community Cache.
//!
//! This crate deliberately contains no networking, discovery, signaling,
//! socket, STUN, ICE, or relay implementation.  Its public DTOs cannot carry
//! peer addresses.  A future transport may exchange only [`RelayCandidate`]
//! values and encrypted [`EncryptedRelayEnvelope`] payloads through a relay.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::str::FromStr;

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

mod federation;
mod publication;

pub use federation::*;
pub use publication::*;

pub const REQUEST_SCHEMA: &str = "rw.community.request.v1";
pub const OBJECT_SCHEMA: &str = "rw.community.object.v1";
pub const RESOLVE_SCHEMA: &str = "rw.community.resolve.v1";
pub const CASE_SCHEMA: &str = "rw.community.case.v1";
pub const RELAY_CREDENTIAL_SCHEMA: &str = "rw.community.relay-credential.v1";
pub const RELAY_ENVELOPE_SCHEMA: &str = "rw.community.relay-envelope.v1";
pub const PROFILE_PAYLOAD_SCHEMA: &str = "rw.community.profile-payload.v1";
pub const POINT_SERIES_PAYLOAD_SCHEMA: &str = "rw.community.point-series-payload.v1";
pub const NATIVE_WINDOW_PAYLOAD_SCHEMA: &str = "rw.community.native-window-payload.v1";
pub const GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA: &str = "rw.community.geographic-window-payload.v1";
pub const TEMPORAL_GRID_PAYLOAD_SCHEMA: &str = "rw.community.temporal-grid-payload.v1";
pub const CASE_ARTIFACT_PAYLOAD_SCHEMA: &str = "rw.community.case-artifact-payload.v1";

pub const RESOLVE_OBJECT_PATH: &str = "/v1/community/objects/resolve";
pub const OBJECT_PATH_TEMPLATE: &str = "/v1/community/objects/{sha256}";
pub const CREATE_CASE_PATH: &str = "/v1/community/cases";
pub const CASE_PATH_TEMPLATE: &str = "/v1/community/cases/{case_id}";
pub const R2_MANIFEST_KEY_TEMPLATE: &str = "v1/manifests/{request_sha256}.json";
pub const R2_OBJECT_KEY_TEMPLATE: &str = "v1/objects/{object_sha256}";

const OBJECT_SIGNATURE_DOMAIN: &[u8] = b"rw-community-object-signature-v1\0";
const CASE_SIGNATURE_DOMAIN: &[u8] = b"rw-community-case-signature-v1\0";
const REQUEST_HASH_DOMAIN: &[u8] = b"rw-community-request-identity-v1\0";
const RELAY_CREDENTIAL_SIGNATURE_DOMAIN: &[u8] = b"rw-community-relay-credential-signature-v1\0";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported schema '{0}'")]
    UnsupportedSchema(String),
    #[error("manifest or request exceeds the configured byte limit")]
    ManifestSizeLimit,
    #[error("malformed protocol JSON")]
    MalformedJson,
    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("non-canonical {0}")]
    NonCanonical(&'static str),
    #[error("object exceeds the configured encoded byte limit")]
    EncodedSizeLimit,
    #[error("object exceeds the configured decoded byte limit")]
    DecodedSizeLimit,
    #[error("object exceeds the configured decompression ratio")]
    DecompressionRatioLimit,
    #[error("encoded object size does not match its signed manifest")]
    EncodedSizeMismatch,
    #[error("decoded object size does not match its signed manifest")]
    DecodedSizeMismatch,
    #[error("object SHA-256 does not match its signed manifest")]
    ObjectHashMismatch,
    #[error("request SHA-256 does not match its signed manifest")]
    RequestHashMismatch,
    #[error("signing key '{0}' is not trusted")]
    UnknownSigningKey(String),
    #[error("signature is malformed")]
    MalformedSignature,
    #[error("signature verification failed")]
    InvalidSignature,
    #[error("private or user-provided data lacks an explicit owner publication grant")]
    PrivatePublicationDenied,
    #[error("redistribution rights were not confirmed")]
    RedistributionRightsUnconfirmed,
    #[error("required ECMWF attribution or modification notice is absent")]
    MissingEcmwfNotice,
    #[error("only relay-mediated candidates are accepted")]
    DirectCandidateForbidden,
    #[error("app-visible peer address information is forbidden")]
    PeerAddressForbidden,
    #[error("relay credential is expired or outside its validity interval")]
    RelayCredentialExpired,
    #[error("object or case manifest is expired or not yet valid")]
    ManifestExpired,
    #[error("relay envelope exceeds its credential or protocol limit")]
    RelayEnvelopeLimit,
    #[error("federation descriptor or catalog is expired or outside its validity interval")]
    FederationExpired,
    #[error("federation origin '{0}' is not operator-approved")]
    UntrustedFederationOrigin(String),
    #[error("federation origin or signing key '{0}' is revoked")]
    RevokedFederationIdentity(String),
    #[error("federation URL is not a canonical public HTTPS endpoint")]
    UnsafeFederationUrl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolLimits {
    pub max_manifest_bytes: u64,
    pub max_encoded_bytes: u64,
    pub max_decoded_bytes: u64,
    pub max_decompression_ratio: u64,
    pub max_variables: usize,
    pub max_provenance_entries: usize,
    pub max_attributions: usize,
    pub max_case_artifacts: usize,
    pub max_relay_chunks: u32,
}

impl Default for ProtocolLimits {
    fn default() -> Self {
        Self {
            max_manifest_bytes: 256 * 1024,
            max_encoded_bytes: 64 * 1024 * 1024,
            max_decoded_bytes: 256 * 1024 * 1024,
            max_decompression_ratio: 64,
            max_variables: 128,
            max_provenance_entries: 16,
            max_attributions: 16,
            max_case_artifacts: 256,
            max_relay_chunks: 65_536,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPolicy {
    Strict,
    Partial,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compression {
    None,
    Gzip,
    Zstd,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DataOrigin {
    PublicProvider,
    PrivateWrf,
    PrivateArwen,
    #[default]
    UserProvided,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct PublicationGrant {
    #[serde(default)]
    pub data_origin: DataOrigin,
    #[serde(default)]
    pub explicit_owner_publication: bool,
    #[serde(default)]
    pub redistribution_rights_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceProvenance {
    pub provider: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub products: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeIdentity {
    pub recipe_id: String,
    pub recipe_version: String,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TimeWindow {
    Utc {
        start_unix: i64,
        end_unix: i64,
    },
    LocalDay {
        date: String,
        timezone: String,
        resolved_start_unix: i64,
        resolved_end_unix: i64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseArtifactType {
    Annotation,
    DerivedTable,
    Overlay,
    RenderedImage,
}

/// The only object-producing query categories admitted to Community Cache.
/// Numeric coordinates are fixed-point to avoid cross-language float identity
/// disagreements.  No variant contains a path, URL, host, or arbitrary file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ShareQuery {
    Profile {
        latitude_e7: i32,
        longitude_e7: i32,
        storage_slot: u16,
        valid_unix: i64,
        /// Pressure-volume variables carried in the profile result.
        pressure_variables: Vec<String>,
        /// Nearest-gridpoint surface variables bundled with the profile.
        /// This is explicit so a surface-value change cannot collide with a
        /// pressure-only cache identity.
        surface_variables: Vec<String>,
        #[serde(default)]
        pressure_levels_hpa: Vec<u16>,
    },
    PointSeries {
        latitude_e7: i32,
        longitude_e7: i32,
        window: TimeWindow,
        missing_policy: MissingPolicy,
    },
    NativeWindow {
        storage_slot: u16,
        valid_unix: i64,
        x0: u32,
        y0: u32,
        x1: u32,
        y1: u32,
        #[serde(default)]
        pressure_levels_hpa: Vec<u16>,
    },
    /// One geographically selected, self-describing native-grid envelope.
    /// The eastward longitude arc crosses the antimeridian when west > east;
    /// -180..180 denotes the full globe. Bounds use exact 1e-7 degree units.
    GeographicWindow {
        storage_slot: u16,
        valid_unix: i64,
        west_longitude_e7: i32,
        south_latitude_e7: i32,
        east_longitude_e7: i32,
        north_latitude_e7: i32,
        #[serde(default)]
        pressure_levels_hpa: Vec<u16>,
    },
    TemporalGrid {
        window: TimeWindow,
        reducer: String,
        semantics: String,
        missing_policy: MissingPolicy,
        #[serde(default)]
        pressure_levels_hpa: Vec<u16>,
    },
    CaseArtifact {
        case_id: String,
        artifact_id: String,
        artifact_type: CaseArtifactType,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareRequest {
    pub schema: String,
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub variables: Vec<String>,
    pub query: ShareQuery,
    pub recipe: RecipeIdentity,
    #[serde(default)]
    pub source_provenance: Vec<SourceProvenance>,
    #[serde(default)]
    pub publication: PublicationGrant,
}

impl ShareRequest {
    /// Normalize order-insensitive collections before computing cache identity.
    pub fn normalize(&mut self) {
        self.schema = self.schema.trim().to_string();
        self.model = self.model.trim().to_ascii_lowercase();
        self.run = self.run.trim().to_string();
        self.snapshot_id = self.snapshot_id.trim().to_ascii_lowercase();
        self.grid_hash = self.grid_hash.trim().to_ascii_lowercase();
        normalize_tokens(&mut self.variables, false);
        self.recipe.recipe_id = self.recipe.recipe_id.trim().to_ascii_lowercase();
        self.recipe.recipe_version = self.recipe.recipe_version.trim().to_string();
        for source in &mut self.source_provenance {
            source.provider = source.provider.trim().to_ascii_lowercase();
            normalize_tokens(&mut source.roles, true);
            normalize_tokens(&mut source.products, true);
        }
        self.source_provenance.sort_by(|a, b| {
            (&a.provider, &a.roles, &a.products).cmp(&(&b.provider, &b.roles, &b.products))
        });
        self.source_provenance.dedup();
        normalize_query(&mut self.query);
    }

    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
        let mut normalized = self.clone();
        normalized.normalize();
        if &normalized != self {
            return Err(ProtocolError::NonCanonical("share request"));
        }
        if self.schema != REQUEST_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_token("model", &self.model, 96, true)?;
        validate_text("run", &self.run, 128)?;
        validate_sha256("snapshot_id", &self.snapshot_id)?;
        validate_sha256("grid_hash", &self.grid_hash)?;
        validate_variables(&self.variables, limits)?;
        validate_token("recipe_id", &self.recipe.recipe_id, 128, true)?;
        validate_text("recipe_version", &self.recipe.recipe_version, 64)?;
        if self.recipe.parameters.len() > 64 {
            return invalid("recipe.parameters", "contains more than 64 entries");
        }
        for (key, value) in &self.recipe.parameters {
            validate_token("recipe parameter key", key, 96, true)?;
            validate_text("recipe parameter value", value, 512)?;
        }
        validate_provenance(&self.source_provenance, limits)?;
        validate_query(&self.query, limits)?;
        if let ShareQuery::Profile {
            pressure_variables,
            surface_variables,
            ..
        } = &self.query
        {
            let mut combined = pressure_variables.clone();
            combined.extend(surface_variables.iter().cloned());
            normalize_tokens(&mut combined, false);
            if combined != self.variables {
                return invalid(
                    "variables",
                    "profile variables must equal the union of pressure_variables and surface_variables",
                );
            }
        }
        validate_publication(&self.publication)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttributionNotice {
    pub provider: String,
    pub notice: String,
    pub source_url: String,
    pub license: String,
    pub license_url: String,
    pub terms_url: String,
    pub disclaimer: String,
}

impl AttributionNotice {
    pub fn ecmwf_open_data() -> Self {
        Self {
            provider: "ecmwf-open-data".into(),
            notice: "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF).".into(),
            source_url: "https://www.ecmwf.int/".into(),
            license: "Creative Commons Attribution 4.0 International (CC BY 4.0)".into(),
            license_url: "https://creativecommons.org/licenses/by/4.0/".into(),
            terms_url: "https://apps.ecmwf.int/datasets/licences/general/".into(),
            disclaimer: "ECMWF does not accept liability for errors, omissions, availability, loss, or damage arising from use of these data.".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectManifest {
    pub schema: String,
    /// Full, self-describing cache identity. `request_sha256` is checked
    /// against the canonical binary encoding of this value.
    pub request: ShareRequest,
    pub request_sha256: String,
    pub object_sha256: String,
    pub content_type: String,
    pub compression: Compression,
    pub encoded_size: u64,
    pub decoded_size: u64,
    #[serde(default)]
    pub attributions: Vec<AttributionNotice>,
    #[serde(default)]
    pub modification_notices: Vec<String>,
    pub created_unix: i64,
    pub expires_unix: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignatureAlgorithm {
    Ed25519,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignatureBlock {
    pub algorithm: SignatureAlgorithm,
    pub signing_key_id: String,
    pub signature_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedObjectManifest {
    pub manifest: ObjectManifest,
    pub signature: SignatureBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveObjectRequest {
    pub schema: String,
    pub request: ShareRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliverySource {
    R2HotObject,
    CommunityRelay,
    Origin,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveObjectResponse {
    pub schema: String,
    pub request_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_manifest: Option<SignedObjectManifest>,
    #[serde(default)]
    pub delivery_order: Vec<DeliverySource>,
}

/// Payload wrapper used by point-series, native-window, temporal-grid, and
/// case-artifact objects. `data` is the corresponding existing query/result
/// DTO. The category-specific schema constant and request hash prevent a
/// correctly signed body from being decoded as the wrong response family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedObjectPayload<T> {
    pub schema: String,
    pub request_sha256: String,
    pub data: T,
}

pub type PointSeriesObjectPayload<T> = TypedObjectPayload<T>;
pub type NativeWindowObjectPayload<T> = TypedObjectPayload<T>;
pub type GeographicWindowObjectPayload<T> = TypedObjectPayload<T>;
pub type TemporalGridObjectPayload<T> = TypedObjectPayload<T>;
pub type CaseArtifactObjectPayload<T> = TypedObjectPayload<T>;

pub fn validate_typed_payload_identity<T>(
    payload: &TypedObjectPayload<T>,
    expected_schema: &'static str,
    request: &ShareRequest,
) -> Result<(), ProtocolError> {
    if payload.schema != expected_schema {
        return Err(ProtocolError::UnsupportedSchema(payload.schema.clone()));
    }
    if payload.request_sha256 != request_sha256(request)? {
        return Err(ProtocolError::RequestHashMismatch);
    }
    let schema_matches_kind = matches!(
        (&request.query, expected_schema),
        (ShareQuery::PointSeries { .. }, POINT_SERIES_PAYLOAD_SCHEMA)
            | (
                ShareQuery::NativeWindow { .. },
                NATIVE_WINDOW_PAYLOAD_SCHEMA
            )
            | (
                ShareQuery::GeographicWindow { .. },
                GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA
            )
            | (
                ShareQuery::TemporalGrid { .. },
                TEMPORAL_GRID_PAYLOAD_SCHEMA
            )
            | (
                ShareQuery::CaseArtifact { .. },
                CASE_ARTIFACT_PAYLOAD_SCHEMA
            )
    );
    if !schema_matches_kind {
        return invalid(
            "payload.schema",
            "payload schema does not match signed query kind",
        );
    }
    Ok(())
}

/// Standard payload wrapper for a sounding/profile object. At integration
/// sites `P` is `rw_query::ProfileResult`. The signed request separately binds
/// the exact surface variables sampled at the same nearest native-grid point.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileObjectPayload<P> {
    pub schema: String,
    pub request_sha256: String,
    pub profile: P,
    pub surface_samples: Vec<SurfaceSample>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SurfaceSample {
    pub variable: String,
    pub units: String,
    pub value: Option<f32>,
}

pub fn validate_profile_payload_identity<P>(
    payload: &ProfileObjectPayload<P>,
    request: &ShareRequest,
) -> Result<(), ProtocolError> {
    if payload.schema != PROFILE_PAYLOAD_SCHEMA {
        return Err(ProtocolError::UnsupportedSchema(payload.schema.clone()));
    }
    if payload.request_sha256 != request_sha256(request)? {
        return Err(ProtocolError::RequestHashMismatch);
    }
    let ShareQuery::Profile {
        surface_variables, ..
    } = &request.query
    else {
        return invalid("query.kind", "profile payload requires a profile request");
    };
    let sample_names = payload
        .surface_samples
        .iter()
        .map(|sample| sample.variable.clone())
        .collect::<Vec<_>>();
    if &sample_names != surface_variables
        || payload.surface_samples.iter().any(|sample| {
            validate_token("surface sample variable", &sample.variable, 128, false).is_err()
                || validate_text("surface sample units", &sample.units, 64).is_err()
                || sample.value.is_some_and(|value| !value.is_finite())
        })
    {
        return invalid(
            "surface_samples",
            "samples must exactly match the signed sorted surface variable identity",
        );
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseArtifactRef {
    pub artifact_id: String,
    pub artifact_type: CaseArtifactType,
    pub request_sha256: String,
    pub object_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseModelSource {
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub source_provenance: Vec<SourceProvenance>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseRoomManifest {
    pub schema: String,
    pub case_id: String,
    pub title: String,
    pub event_start_unix: i64,
    pub event_end_unix: i64,
    pub published_unix: i64,
    pub retain_until_unix: i64,
    pub publication: PublicationGrant,
    pub sources: Vec<CaseModelSource>,
    pub artifacts: Vec<CaseArtifactRef>,
    #[serde(default)]
    pub attributions: Vec<AttributionNotice>,
    #[serde(default)]
    pub modification_notices: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCaseRoomManifest {
    pub manifest: CaseRoomManifest,
    pub signature: SignatureBlock,
}

/// The enum intentionally has one representable value. Serde therefore fails
/// closed for `host`, `srflx`, `prflx`, `direct`, or future unknown values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayCandidateKind {
    Relay,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayCandidate {
    pub kind: RelayCandidateKind,
    /// Opaque backend identifier, never a hostname or IP literal.
    pub relay_id: String,
    pub ticket_id: String,
    pub expires_unix: i64,
}

impl RelayCandidate {
    pub fn validate(&self, now_unix: i64) -> Result<(), ProtocolError> {
        if self.kind != RelayCandidateKind::Relay {
            return Err(ProtocolError::DirectCandidateForbidden);
        }
        validate_opaque_id("relay_id", &self.relay_id)?;
        validate_opaque_id("ticket_id", &self.ticket_id)?;
        if self.expires_unix <= now_unix {
            return Err(ProtocolError::RelayCredentialExpired);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelayDirection {
    Upload,
    Download,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayCredentialClaims {
    pub schema: String,
    pub relay_id: String,
    pub session_id: String,
    pub subject_id: String,
    pub object_sha256: String,
    pub direction: RelayDirection,
    pub issued_unix: i64,
    pub not_before_unix: i64,
    pub expires_unix: i64,
    pub max_bytes: u64,
    pub max_chunks: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedRelayCredential {
    pub claims: RelayCredentialClaims,
    pub signature: SignatureBlock,
}

impl RelayCredentialClaims {
    pub fn validate(&self, now_unix: i64, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
        if self.schema != RELAY_CREDENTIAL_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_opaque_id("relay_id", &self.relay_id)?;
        validate_opaque_id("session_id", &self.session_id)?;
        validate_opaque_id("subject_id", &self.subject_id)?;
        validate_sha256("object_sha256", &self.object_sha256)?;
        if self.issued_unix > self.not_before_unix
            || now_unix < self.not_before_unix
            || now_unix >= self.expires_unix
            || self.expires_unix.saturating_sub(self.issued_unix) > 15 * 60
        {
            return Err(ProtocolError::RelayCredentialExpired);
        }
        if self.max_bytes == 0
            || self.max_bytes > limits.max_encoded_bytes
            || self.max_chunks == 0
            || self.max_chunks > limits.max_relay_chunks
        {
            return Err(ProtocolError::RelayEnvelopeLimit);
        }
        Ok(())
    }
}

pub fn canonical_relay_credential_bytes(
    claims: &RelayCredentialClaims,
    signing_key_id: &str,
    now_unix: i64,
    limits: &ProtocolLimits,
) -> Result<Vec<u8>, ProtocolError> {
    claims.validate(now_unix, limits)?;
    validate_opaque_id("signing_key_id", signing_key_id)?;
    let mut out = Vec::with_capacity(384);
    out.extend_from_slice(RELAY_CREDENTIAL_SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    encode_relay_credential_claims(&mut out, claims);
    Ok(out)
}

pub fn sign_relay_credential(
    claims: RelayCredentialClaims,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
    now_unix: i64,
    limits: &ProtocolLimits,
) -> Result<SignedRelayCredential, ProtocolError> {
    let signing_key_id = signing_key_id.into();
    let preimage = canonical_relay_credential_bytes(&claims, &signing_key_id, now_unix, limits)?;
    let signature = signing_key.sign(&preimage);
    Ok(SignedRelayCredential {
        claims,
        signature: SignatureBlock {
            algorithm: SignatureAlgorithm::Ed25519,
            signing_key_id,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    })
}

pub fn verify_signed_relay_credential(
    signed: &SignedRelayCredential,
    now_unix: i64,
    trusted_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    let key = trusted_keys
        .get(&signed.signature.signing_key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(signed.signature.signing_key_id.clone()))?;
    let signature_bytes = decode_base64(&signed.signature.signature_base64)
        .map_err(|_| ProtocolError::MalformedSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ProtocolError::MalformedSignature)?;
    let preimage = canonical_relay_credential_bytes(
        &signed.claims,
        &signed.signature.signing_key_id,
        now_unix,
        limits,
    )?;
    key.verify(&preimage, &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndToEndCipher {
    XChaCha20Poly1305,
}

/// Opaque ciphertext envelope. The relay routes this structure but receives no
/// content key. `nonce_base64` must decode to 24 bytes; the ciphertext remains
/// authenticated end-to-end by the clients in a future Phase 2 implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptedRelayEnvelope {
    pub schema: String,
    pub session_id: String,
    pub object_sha256: String,
    pub cipher: EndToEndCipher,
    pub chunk_index: u32,
    pub chunk_count: u32,
    pub plaintext_size: u32,
    pub nonce_base64: String,
    pub ciphertext_base64: String,
}

impl EncryptedRelayEnvelope {
    pub fn validate(
        &self,
        credential: &RelayCredentialClaims,
        limits: &ProtocolLimits,
    ) -> Result<(), ProtocolError> {
        if self.schema != RELAY_ENVELOPE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_opaque_id("session_id", &self.session_id)?;
        validate_sha256("object_sha256", &self.object_sha256)?;
        if self.session_id != credential.session_id
            || self.object_sha256 != credential.object_sha256
            || self.chunk_count == 0
            || self.chunk_count > credential.max_chunks
            || self.chunk_count > limits.max_relay_chunks
            || self.chunk_index >= self.chunk_count
            || u64::from(self.plaintext_size) > credential.max_bytes
        {
            return Err(ProtocolError::RelayEnvelopeLimit);
        }
        let nonce =
            decode_base64(&self.nonce_base64).map_err(|_| ProtocolError::RelayEnvelopeLimit)?;
        let ciphertext = decode_base64(&self.ciphertext_base64)
            .map_err(|_| ProtocolError::RelayEnvelopeLimit)?;
        if nonce.len() != 24
            || ciphertext.len() < 16
            || ciphertext.len() as u64 > credential.max_bytes.saturating_add(16)
        {
            return Err(ProtocolError::RelayEnvelopeLimit);
        }
        Ok(())
    }
}

/// Streaming decompression accounting. Call `observe` for every decoded chunk
/// before retaining it and `finish` before accepting the object.
#[derive(Debug, Clone)]
pub struct DecodedSizeGuard {
    expected: u64,
    maximum: u64,
    observed: u64,
}

impl DecodedSizeGuard {
    pub fn new(manifest: &ObjectManifest, limits: &ProtocolLimits) -> Result<Self, ProtocolError> {
        validate_object_manifest(manifest, limits)?;
        Ok(Self {
            expected: manifest.decoded_size,
            maximum: limits.max_decoded_bytes,
            observed: 0,
        })
    }

    pub fn observe(&mut self, bytes: usize) -> Result<(), ProtocolError> {
        self.observed = self
            .observed
            .checked_add(bytes as u64)
            .ok_or(ProtocolError::DecodedSizeLimit)?;
        if self.observed > self.maximum || self.observed > self.expected {
            return Err(ProtocolError::DecodedSizeLimit);
        }
        Ok(())
    }

    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.observed != self.expected {
            return Err(ProtocolError::DecodedSizeMismatch);
        }
        Ok(())
    }
}

pub type TrustedSigningKeys = BTreeMap<String, VerifyingKey>;

pub fn parse_share_request_bounded(
    bytes: &[u8],
    limits: &ProtocolLimits,
) -> Result<ShareRequest, ProtocolError> {
    check_manifest_size(bytes, limits)?;
    let request: ShareRequest =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    request.validate(limits)?;
    Ok(request)
}

pub fn parse_signed_object_manifest_bounded(
    bytes: &[u8],
    limits: &ProtocolLimits,
) -> Result<SignedObjectManifest, ProtocolError> {
    check_manifest_size(bytes, limits)?;
    let signed: SignedObjectManifest =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    validate_opaque_id("signing_key_id", &signed.signature.signing_key_id)?;
    validate_object_manifest(&signed.manifest, limits)?;
    Ok(signed)
}

pub fn parse_signed_case_manifest_bounded(
    bytes: &[u8],
    limits: &ProtocolLimits,
) -> Result<SignedCaseRoomManifest, ProtocolError> {
    check_manifest_size(bytes, limits)?;
    let signed: SignedCaseRoomManifest =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    validate_opaque_id("signing_key_id", &signed.signature.signing_key_id)?;
    validate_case_manifest(&signed.manifest, limits)?;
    Ok(signed)
}

fn check_manifest_size(bytes: &[u8], limits: &ProtocolLimits) -> Result<(), ProtocolError> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_manifest_bytes {
        Err(ProtocolError::ManifestSizeLimit)
    } else {
        Ok(())
    }
}

pub fn parse_verifying_key_base64(value: &str) -> Result<VerifyingKey, ProtocolError> {
    let bytes = decode_base64(value).map_err(|_| ProtocolError::MalformedSignature)?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| ProtocolError::MalformedSignature)?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| ProtocolError::MalformedSignature)
}

pub fn trusted_signing_keys_from_base64<I, K, V>(
    entries: I,
) -> Result<TrustedSigningKeys, ProtocolError>
where
    I: IntoIterator<Item = (K, V)>,
    K: Into<String>,
    V: AsRef<str>,
{
    let mut trusted = BTreeMap::new();
    for (key_id, value) in entries {
        let key_id = key_id.into();
        validate_opaque_id("signing_key_id", &key_id)?;
        let key = parse_verifying_key_base64(value.as_ref())?;
        if trusted.insert(key_id.clone(), key).is_some() {
            return invalid(
                "signing_key_id",
                format!("duplicate signing key '{key_id}'"),
            );
        }
    }
    if trusted.is_empty() {
        return invalid("trusted_signing_keys", "at least one key is required");
    }
    Ok(trusted)
}

pub fn canonical_request_bytes(request: &ShareRequest) -> Result<Vec<u8>, ProtocolError> {
    let mut normalized = request.clone();
    normalized.normalize();
    normalized.validate(&ProtocolLimits::default())?;
    let mut out = Vec::with_capacity(512);
    out.extend_from_slice(REQUEST_HASH_DOMAIN);
    encode_share_request(&mut out, &normalized);
    Ok(out)
}

pub fn request_sha256(request: &ShareRequest) -> Result<String, ProtocolError> {
    Ok(sha256_hex(&canonical_request_bytes(request)?))
}

pub fn object_sha256(bytes: &[u8]) -> String {
    sha256_hex(bytes)
}

pub fn canonical_object_manifest_bytes(
    manifest: &ObjectManifest,
    signing_key_id: &str,
) -> Result<Vec<u8>, ProtocolError> {
    validate_object_manifest(manifest, &ProtocolLimits::default())?;
    validate_opaque_id("signing_key_id", signing_key_id)?;
    let mut out = Vec::with_capacity(768);
    out.extend_from_slice(OBJECT_SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    encode_object_manifest(&mut out, manifest);
    Ok(out)
}

pub fn sign_object_manifest(
    manifest: ObjectManifest,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
) -> Result<SignedObjectManifest, ProtocolError> {
    let signing_key_id = signing_key_id.into();
    let preimage = canonical_object_manifest_bytes(&manifest, &signing_key_id)?;
    let signature = signing_key.sign(&preimage);
    Ok(SignedObjectManifest {
        manifest,
        signature: SignatureBlock {
            algorithm: SignatureAlgorithm::Ed25519,
            signing_key_id,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    })
}

pub fn verify_signed_object(
    signed: &SignedObjectManifest,
    request: &ShareRequest,
    encoded_object: &[u8],
    now_unix: i64,
    trusted_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    validate_object_manifest(&signed.manifest, limits)?;
    if now_unix < signed.manifest.created_unix.saturating_sub(300)
        || now_unix >= signed.manifest.expires_unix
    {
        return Err(ProtocolError::ManifestExpired);
    }
    let expected_request_hash = request_sha256(request)?;
    if signed.manifest.request_sha256 != expected_request_hash {
        return Err(ProtocolError::RequestHashMismatch);
    }
    if signed.manifest.encoded_size != encoded_object.len() as u64 {
        return Err(ProtocolError::EncodedSizeMismatch);
    }
    if signed.manifest.object_sha256 != object_sha256(encoded_object) {
        return Err(ProtocolError::ObjectHashMismatch);
    }
    let key = trusted_keys
        .get(&signed.signature.signing_key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(signed.signature.signing_key_id.clone()))?;
    let signature_bytes = decode_base64(&signed.signature.signature_base64)
        .map_err(|_| ProtocolError::MalformedSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ProtocolError::MalformedSignature)?;
    let preimage =
        canonical_object_manifest_bytes(&signed.manifest, &signed.signature.signing_key_id)?;
    key.verify(&preimage, &signature)
        .map_err(|_| ProtocolError::InvalidSignature)?;
    enforce_request_attributions(request, &signed.manifest)
}

pub fn canonical_case_manifest_bytes(
    manifest: &CaseRoomManifest,
    signing_key_id: &str,
) -> Result<Vec<u8>, ProtocolError> {
    validate_case_manifest(manifest, &ProtocolLimits::default())?;
    validate_opaque_id("signing_key_id", signing_key_id)?;
    let mut out = Vec::with_capacity(1024);
    out.extend_from_slice(CASE_SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    encode_case_manifest(&mut out, manifest);
    Ok(out)
}

pub fn sign_case_manifest(
    manifest: CaseRoomManifest,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
) -> Result<SignedCaseRoomManifest, ProtocolError> {
    let signing_key_id = signing_key_id.into();
    let preimage = canonical_case_manifest_bytes(&manifest, &signing_key_id)?;
    let signature = signing_key.sign(&preimage);
    Ok(SignedCaseRoomManifest {
        manifest,
        signature: SignatureBlock {
            algorithm: SignatureAlgorithm::Ed25519,
            signing_key_id,
            signature_base64: base64::engine::general_purpose::STANDARD
                .encode(signature.to_bytes()),
        },
    })
}

pub fn verify_signed_case(
    signed: &SignedCaseRoomManifest,
    now_unix: i64,
    trusted_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    validate_case_manifest(&signed.manifest, limits)?;
    if now_unix < signed.manifest.published_unix.saturating_sub(300)
        || now_unix >= signed.manifest.retain_until_unix
    {
        return Err(ProtocolError::ManifestExpired);
    }
    let key = trusted_keys
        .get(&signed.signature.signing_key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(signed.signature.signing_key_id.clone()))?;
    let signature_bytes = decode_base64(&signed.signature.signature_base64)
        .map_err(|_| ProtocolError::MalformedSignature)?;
    let signature =
        Signature::from_slice(&signature_bytes).map_err(|_| ProtocolError::MalformedSignature)?;
    let preimage =
        canonical_case_manifest_bytes(&signed.manifest, &signed.signature.signing_key_id)?;
    key.verify(&preimage, &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

pub fn validate_object_manifest(
    manifest: &ObjectManifest,
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    if manifest.schema != OBJECT_SCHEMA {
        return Err(ProtocolError::UnsupportedSchema(manifest.schema.clone()));
    }
    manifest.request.validate(limits)?;
    if request_sha256(&manifest.request)? != manifest.request_sha256 {
        return Err(ProtocolError::RequestHashMismatch);
    }
    validate_sha256("request_sha256", &manifest.request_sha256)?;
    validate_sha256("object_sha256", &manifest.object_sha256)?;
    validate_content_type(&manifest.content_type)?;
    if manifest.encoded_size == 0 || manifest.encoded_size > limits.max_encoded_bytes {
        return Err(ProtocolError::EncodedSizeLimit);
    }
    if manifest.decoded_size == 0 || manifest.decoded_size > limits.max_decoded_bytes {
        return Err(ProtocolError::DecodedSizeLimit);
    }
    if manifest.compression == Compression::None && manifest.encoded_size != manifest.decoded_size {
        return Err(ProtocolError::DecodedSizeMismatch);
    }
    let permitted = manifest
        .encoded_size
        .saturating_mul(limits.max_decompression_ratio);
    if manifest.decoded_size > permitted {
        return Err(ProtocolError::DecompressionRatioLimit);
    }
    if manifest.created_unix < 0 || manifest.expires_unix <= manifest.created_unix {
        return invalid(
            "timestamps",
            "expiration must be after a non-negative creation time",
        );
    }
    validate_notices(
        &manifest.attributions,
        &manifest.modification_notices,
        limits,
    )?;
    enforce_request_attributions(&manifest.request, manifest)
}

pub fn validate_case_manifest(
    manifest: &CaseRoomManifest,
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    if manifest.schema != CASE_SCHEMA {
        return Err(ProtocolError::UnsupportedSchema(manifest.schema.clone()));
    }
    validate_opaque_id("case_id", &manifest.case_id)?;
    validate_text("title", &manifest.title, 160)?;
    if manifest.event_start_unix >= manifest.event_end_unix
        || manifest.published_unix < 0
        || manifest.retain_until_unix <= manifest.published_unix
    {
        return invalid("case timestamps", "invalid event or retention interval");
    }
    validate_publication(&manifest.publication)?;
    if !manifest.publication.explicit_owner_publication {
        return Err(ProtocolError::PrivatePublicationDenied);
    }
    if manifest.artifacts.is_empty() || manifest.artifacts.len() > limits.max_case_artifacts {
        return invalid(
            "artifacts",
            "case must contain a bounded non-empty artifact list",
        );
    }
    let mut ids = BTreeSet::new();
    for artifact in &manifest.artifacts {
        validate_opaque_id("artifact_id", &artifact.artifact_id)?;
        validate_sha256("request_sha256", &artifact.request_sha256)?;
        validate_sha256("object_sha256", &artifact.object_sha256)?;
        if !ids.insert(&artifact.artifact_id) {
            return invalid("artifact_id", "duplicate case artifact id");
        }
    }
    if manifest.sources.is_empty() || manifest.sources.len() > 64 {
        return invalid(
            "sources",
            "case must contain a bounded non-empty source list",
        );
    }
    for source in &manifest.sources {
        validate_token("model", &source.model, 96, true)?;
        validate_text("run", &source.run, 128)?;
        validate_sha256("snapshot_id", &source.snapshot_id)?;
        validate_sha256("grid_hash", &source.grid_hash)?;
        validate_provenance(&source.source_provenance, limits)?;
    }
    validate_notices(
        &manifest.attributions,
        &manifest.modification_notices,
        limits,
    )?;
    let provenance = manifest
        .sources
        .iter()
        .flat_map(|source| source.source_provenance.iter().cloned())
        .collect::<Vec<_>>();
    enforce_ecmwf_notice(
        &provenance,
        &manifest.attributions,
        &manifest.modification_notices,
    )
}

pub fn enforce_request_attributions(
    request: &ShareRequest,
    manifest: &ObjectManifest,
) -> Result<(), ProtocolError> {
    enforce_ecmwf_notice(
        &request.source_provenance,
        &manifest.attributions,
        &manifest.modification_notices,
    )
}

fn enforce_ecmwf_notice(
    provenance: &[SourceProvenance],
    attributions: &[AttributionNotice],
    modifications: &[String],
) -> Result<(), ProtocolError> {
    let uses_ecmwf = provenance
        .iter()
        .any(|source| source.provider == "ecmwf-open-data");
    if !uses_ecmwf {
        return Ok(());
    }
    let has_attribution = attributions.iter().any(|notice| {
        notice.provider == "ecmwf-open-data"
            && notice.source_url == "https://www.ecmwf.int/"
            && notice.license.contains("CC BY 4.0")
            && notice.license_url == "https://creativecommons.org/licenses/by/4.0/"
            && notice.terms_url == "https://apps.ecmwf.int/datasets/licences/general/"
            && !notice.notice.trim().is_empty()
            && !notice.disclaimer.trim().is_empty()
    });
    if !has_attribution || modifications.iter().all(|notice| notice.trim().is_empty()) {
        return Err(ProtocolError::MissingEcmwfNotice);
    }
    Ok(())
}

fn validate_notices(
    attributions: &[AttributionNotice],
    modifications: &[String],
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    if attributions.len() > limits.max_attributions || modifications.len() > 32 {
        return invalid("notices", "too many attribution or modification notices");
    }
    for item in attributions {
        validate_token("attribution.provider", &item.provider, 96, true)?;
        validate_text("attribution.notice", &item.notice, 2048)?;
        validate_https_url("attribution.source_url", &item.source_url)?;
        validate_text("attribution.license", &item.license, 512)?;
        validate_https_url("attribution.license_url", &item.license_url)?;
        validate_https_url("attribution.terms_url", &item.terms_url)?;
        validate_text("attribution.disclaimer", &item.disclaimer, 2048)?;
    }
    for item in modifications {
        validate_text("modification_notice", item, 2048)?;
    }
    Ok(())
}

fn validate_publication(grant: &PublicationGrant) -> Result<(), ProtocolError> {
    match grant.data_origin {
        DataOrigin::PublicProvider => {
            if !grant.redistribution_rights_confirmed {
                return Err(ProtocolError::RedistributionRightsUnconfirmed);
            }
        }
        DataOrigin::PrivateWrf | DataOrigin::PrivateArwen | DataOrigin::UserProvided => {
            if !grant.explicit_owner_publication {
                return Err(ProtocolError::PrivatePublicationDenied);
            }
            if !grant.redistribution_rights_confirmed {
                return Err(ProtocolError::RedistributionRightsUnconfirmed);
            }
        }
    }
    Ok(())
}

fn validate_query(query: &ShareQuery, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
    match query {
        ShareQuery::Profile {
            latitude_e7,
            longitude_e7,
            valid_unix,
            pressure_variables,
            surface_variables,
            pressure_levels_hpa,
            ..
        } => {
            validate_coordinates(*latitude_e7, *longitude_e7)?;
            if *valid_unix < 0 {
                return invalid("valid_unix", "must be a non-negative UTC Unix timestamp");
            }
            validate_variables(pressure_variables, limits)?;
            validate_variables(surface_variables, limits)?;
            validate_levels(pressure_levels_hpa)?;
        }
        ShareQuery::NativeWindow {
            valid_unix,
            pressure_levels_hpa,
            ..
        } => {
            if *valid_unix < 0 {
                return invalid("valid_unix", "must be a non-negative UTC Unix timestamp");
            }
            validate_levels(pressure_levels_hpa)?;
        }
        ShareQuery::GeographicWindow {
            valid_unix,
            west_longitude_e7,
            south_latitude_e7,
            east_longitude_e7,
            north_latitude_e7,
            pressure_levels_hpa,
            ..
        } => {
            if *valid_unix < 0 {
                return invalid("valid_unix", "must be a non-negative UTC Unix timestamp");
            }
            validate_geographic_bbox(
                *west_longitude_e7,
                *south_latitude_e7,
                *east_longitude_e7,
                *north_latitude_e7,
            )?;
            validate_levels(pressure_levels_hpa)?;
        }
        ShareQuery::PointSeries {
            latitude_e7,
            longitude_e7,
            window,
            ..
        } => {
            validate_coordinates(*latitude_e7, *longitude_e7)?;
            validate_window(window)?;
        }
        ShareQuery::TemporalGrid {
            window,
            reducer,
            semantics,
            pressure_levels_hpa,
            ..
        } => {
            validate_window(window)?;
            validate_token("reducer", reducer, 96, true)?;
            validate_token("semantics", semantics, 96, true)?;
            validate_levels(pressure_levels_hpa)?;
        }
        ShareQuery::CaseArtifact {
            case_id,
            artifact_id,
            ..
        } => {
            validate_opaque_id("case_id", case_id)?;
            validate_opaque_id("artifact_id", artifact_id)?;
        }
    }
    match query {
        ShareQuery::NativeWindow { x0, y0, x1, y1, .. } if x0 >= x1 || y0 >= y1 => {
            return invalid(
                "native_window",
                "window bounds must be non-empty and ordered",
            );
        }
        _ => {}
    }
    Ok(())
}

fn validate_geographic_bbox(
    west_longitude_e7: i32,
    south_latitude_e7: i32,
    east_longitude_e7: i32,
    north_latitude_e7: i32,
) -> Result<(), ProtocolError> {
    validate_coordinates(south_latitude_e7, west_longitude_e7)?;
    validate_coordinates(north_latitude_e7, east_longitude_e7)?;
    if south_latitude_e7 >= north_latitude_e7 {
        return invalid(
            "geographic_bbox",
            "south latitude must be strictly less than north latitude",
        );
    }
    if west_longitude_e7 == east_longitude_e7 {
        return invalid(
            "geographic_bbox",
            "longitude bounds must select a non-empty eastward arc",
        );
    }
    Ok(())
}

fn validate_window(window: &TimeWindow) -> Result<(), ProtocolError> {
    match window {
        TimeWindow::Utc {
            start_unix,
            end_unix,
        } if start_unix < end_unix => Ok(()),
        TimeWindow::LocalDay {
            date,
            timezone,
            resolved_start_unix,
            resolved_end_unix,
        } if date.len() == 10
            && date.bytes().all(|b| b.is_ascii_digit() || b == b'-')
            && !timezone.is_empty()
            && timezone.len() <= 64
            && resolved_start_unix < resolved_end_unix =>
        {
            Ok(())
        }
        _ => invalid("window", "invalid or empty time interval"),
    }
}

fn validate_coordinates(latitude_e7: i32, longitude_e7: i32) -> Result<(), ProtocolError> {
    if !(-900_000_000..=900_000_000).contains(&latitude_e7)
        || !(-1_800_000_000..=1_800_000_000).contains(&longitude_e7)
    {
        return invalid(
            "coordinates",
            "fixed-point coordinate is outside Earth bounds",
        );
    }
    Ok(())
}

fn validate_levels(levels: &[u16]) -> Result<(), ProtocolError> {
    if levels.len() > 256
        || levels.windows(2).any(|window| window[0] >= window[1])
        || levels.iter().any(|level| *level == 0 || *level > 1_200)
    {
        return invalid(
            "pressure_levels_hpa",
            "levels must be unique ascending values in 1..=1200",
        );
    }
    Ok(())
}

fn validate_variables(variables: &[String], limits: &ProtocolLimits) -> Result<(), ProtocolError> {
    if variables.is_empty() || variables.len() > limits.max_variables {
        return invalid("variables", "must be a bounded non-empty list");
    }
    for variable in variables {
        validate_token("variable", variable, 128, false)?;
    }
    if variables.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err(ProtocolError::NonCanonical("variables"));
    }
    Ok(())
}

fn validate_provenance(
    provenance: &[SourceProvenance],
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    if provenance.is_empty() || provenance.len() > limits.max_provenance_entries {
        return invalid("source_provenance", "must be a bounded non-empty list");
    }
    for source in provenance {
        validate_token("provider", &source.provider, 96, true)?;
        if source.roles.len() > 16 || source.products.len() > 32 {
            return invalid("source_provenance", "too many role or product labels");
        }
        for role in &source.roles {
            validate_token("source role", role, 96, true)?;
        }
        for product in &source.products {
            validate_token("source product", product, 96, true)?;
        }
    }
    let mut normalized = provenance.to_vec();
    for source in &mut normalized {
        source.provider = source.provider.trim().to_ascii_lowercase();
        normalize_tokens(&mut source.roles, true);
        normalize_tokens(&mut source.products, true);
    }
    normalized.sort_by(|a, b| {
        (&a.provider, &a.roles, &a.products).cmp(&(&b.provider, &b.roles, &b.products))
    });
    normalized.dedup();
    if normalized != provenance {
        return Err(ProtocolError::NonCanonical("source provenance"));
    }
    Ok(())
}

fn validate_content_type(value: &str) -> Result<(), ProtocolError> {
    const ALLOWED: &[&str] = &[
        "application/json",
        "application/vnd.apache.arrow.file",
        "application/vnd.rusty-weather.window+zstd",
        "image/png",
    ];
    if ALLOWED.contains(&value) {
        Ok(())
    } else {
        invalid(
            "content_type",
            "content type is not in the protocol allowlist",
        )
    }
}

fn validate_sha256(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        Ok(())
    } else {
        invalid(field, "must be 64 lowercase hexadecimal characters")
    }
}

fn validate_opaque_id(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if IpAddr::from_str(value).is_ok()
        || value.contains('.')
        || value.contains(':')
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(ProtocolError::PeerAddressForbidden);
    }
    validate_token(field, value, 128, false)?;
    Ok(())
}

fn validate_token(
    field: &'static str,
    value: &str,
    max: usize,
    allow_dot: bool,
) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > max
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

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), ProtocolError> {
    if value.trim() != value
        || value.is_empty()
        || value.len() > max
        || value.chars().any(char::is_control)
    {
        return invalid(field, "must be bounded, trimmed, non-control text");
    }
    Ok(())
}

fn validate_https_url(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.starts_with("https://") && value.len() <= 512 && !value.chars().any(char::is_control) {
        Ok(())
    } else {
        invalid(field, "must be a bounded HTTPS URL")
    }
}

fn invalid<T>(field: &'static str, reason: impl Into<String>) -> Result<T, ProtocolError> {
    Err(ProtocolError::InvalidField {
        field,
        reason: reason.into(),
    })
}

fn normalize_tokens(values: &mut Vec<String>, lowercase: bool) {
    for value in values.iter_mut() {
        *value = if lowercase {
            value.trim().to_ascii_lowercase()
        } else {
            value.trim().to_string()
        };
    }
    values.sort();
    values.dedup();
}

fn normalize_query(query: &mut ShareQuery) {
    match query {
        ShareQuery::Profile {
            pressure_variables,
            surface_variables,
            pressure_levels_hpa,
            ..
        } => {
            normalize_tokens(pressure_variables, false);
            normalize_tokens(surface_variables, false);
            pressure_levels_hpa.sort();
            pressure_levels_hpa.dedup();
        }
        ShareQuery::NativeWindow {
            pressure_levels_hpa,
            ..
        }
        | ShareQuery::GeographicWindow {
            pressure_levels_hpa,
            ..
        }
        | ShareQuery::TemporalGrid {
            pressure_levels_hpa,
            ..
        } => {
            pressure_levels_hpa.sort();
            pressure_levels_hpa.dedup();
        }
        _ => {}
    }
    if let ShareQuery::TemporalGrid {
        reducer, semantics, ..
    } = query
    {
        *reducer = reducer.trim().to_ascii_lowercase();
        *semantics = semantics.trim().to_ascii_lowercase();
    }
}

fn decode_base64(value: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(value)
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

// Canonical encoding uses fixed field order, one-byte enum tags, big-endian
// integers, and u32 big-endian byte lengths/counts. Strings are raw UTF-8.
// New schema versions must use a new domain and a separate encoder.
fn put_u8(out: &mut Vec<u8>, value: u8) {
    out.push(value);
}
fn put_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn put_bool(out: &mut Vec<u8>, value: bool) {
    put_u8(out, u8::from(value));
}
fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, value.len() as u32);
    out.extend_from_slice(value.as_bytes());
}
fn put_strings(out: &mut Vec<u8>, values: &[String]) {
    put_u32(out, values.len() as u32);
    for value in values {
        put_str(out, value);
    }
}
fn put_u16s(out: &mut Vec<u8>, values: &[u16]) {
    put_u32(out, values.len() as u32);
    for value in values {
        put_u16(out, *value);
    }
}

fn encode_share_request(out: &mut Vec<u8>, value: &ShareRequest) {
    put_str(out, &value.schema);
    put_str(out, &value.model);
    put_str(out, &value.run);
    put_str(out, &value.snapshot_id);
    put_str(out, &value.grid_hash);
    put_strings(out, &value.variables);
    encode_query(out, &value.query);
    put_str(out, &value.recipe.recipe_id);
    put_str(out, &value.recipe.recipe_version);
    put_u32(out, value.recipe.parameters.len() as u32);
    for (key, parameter) in &value.recipe.parameters {
        put_str(out, key);
        put_str(out, parameter);
    }
    encode_provenance(out, &value.source_provenance);
    encode_publication(out, &value.publication);
}

fn encode_query(out: &mut Vec<u8>, value: &ShareQuery) {
    match value {
        ShareQuery::Profile {
            latitude_e7,
            longitude_e7,
            storage_slot,
            valid_unix,
            pressure_variables,
            surface_variables,
            pressure_levels_hpa,
        } => {
            put_u8(out, 0);
            put_i32(out, *latitude_e7);
            put_i32(out, *longitude_e7);
            put_u16(out, *storage_slot);
            put_i64(out, *valid_unix);
            put_strings(out, pressure_variables);
            put_strings(out, surface_variables);
            put_u16s(out, pressure_levels_hpa);
        }
        ShareQuery::PointSeries {
            latitude_e7,
            longitude_e7,
            window,
            missing_policy,
        } => {
            put_u8(out, 1);
            put_i32(out, *latitude_e7);
            put_i32(out, *longitude_e7);
            encode_window(out, window);
            put_u8(out, missing_policy_tag(*missing_policy));
        }
        ShareQuery::NativeWindow {
            storage_slot,
            valid_unix,
            x0,
            y0,
            x1,
            y1,
            pressure_levels_hpa,
        } => {
            put_u8(out, 2);
            put_u16(out, *storage_slot);
            put_i64(out, *valid_unix);
            put_u32(out, *x0);
            put_u32(out, *y0);
            put_u32(out, *x1);
            put_u32(out, *y1);
            put_u16s(out, pressure_levels_hpa);
        }
        ShareQuery::GeographicWindow {
            storage_slot,
            valid_unix,
            west_longitude_e7,
            south_latitude_e7,
            east_longitude_e7,
            north_latitude_e7,
            pressure_levels_hpa,
        } => {
            // Appended tag: existing v1 query identities remain byte-stable.
            put_u8(out, 5);
            put_u16(out, *storage_slot);
            put_i64(out, *valid_unix);
            put_i32(out, *west_longitude_e7);
            put_i32(out, *south_latitude_e7);
            put_i32(out, *east_longitude_e7);
            put_i32(out, *north_latitude_e7);
            put_u16s(out, pressure_levels_hpa);
        }
        ShareQuery::TemporalGrid {
            window,
            reducer,
            semantics,
            missing_policy,
            pressure_levels_hpa,
        } => {
            put_u8(out, 3);
            encode_window(out, window);
            put_str(out, reducer);
            put_str(out, semantics);
            put_u8(out, missing_policy_tag(*missing_policy));
            put_u16s(out, pressure_levels_hpa);
        }
        ShareQuery::CaseArtifact {
            case_id,
            artifact_id,
            artifact_type,
        } => {
            put_u8(out, 4);
            put_str(out, case_id);
            put_str(out, artifact_id);
            put_u8(out, case_artifact_tag(*artifact_type));
        }
    }
}

fn encode_window(out: &mut Vec<u8>, value: &TimeWindow) {
    match value {
        TimeWindow::Utc {
            start_unix,
            end_unix,
        } => {
            put_u8(out, 0);
            put_i64(out, *start_unix);
            put_i64(out, *end_unix);
        }
        TimeWindow::LocalDay {
            date,
            timezone,
            resolved_start_unix,
            resolved_end_unix,
        } => {
            put_u8(out, 1);
            put_str(out, date);
            put_str(out, timezone);
            put_i64(out, *resolved_start_unix);
            put_i64(out, *resolved_end_unix);
        }
    }
}

fn encode_provenance(out: &mut Vec<u8>, values: &[SourceProvenance]) {
    put_u32(out, values.len() as u32);
    for value in values {
        put_str(out, &value.provider);
        put_strings(out, &value.roles);
        put_strings(out, &value.products);
    }
}

fn encode_publication(out: &mut Vec<u8>, value: &PublicationGrant) {
    let tag = match value.data_origin {
        DataOrigin::PublicProvider => 0,
        DataOrigin::PrivateWrf => 1,
        DataOrigin::PrivateArwen => 2,
        DataOrigin::UserProvided => 3,
    };
    put_u8(out, tag);
    put_bool(out, value.explicit_owner_publication);
    put_bool(out, value.redistribution_rights_confirmed);
}

fn encode_object_manifest(out: &mut Vec<u8>, value: &ObjectManifest) {
    put_str(out, &value.schema);
    encode_share_request(out, &value.request);
    put_str(out, &value.request_sha256);
    put_str(out, &value.object_sha256);
    put_str(out, &value.content_type);
    put_u8(out, compression_tag(value.compression));
    put_u64(out, value.encoded_size);
    put_u64(out, value.decoded_size);
    encode_attributions(out, &value.attributions);
    put_strings(out, &value.modification_notices);
    put_i64(out, value.created_unix);
    put_i64(out, value.expires_unix);
}

fn encode_attributions(out: &mut Vec<u8>, values: &[AttributionNotice]) {
    put_u32(out, values.len() as u32);
    for value in values {
        put_str(out, &value.provider);
        put_str(out, &value.notice);
        put_str(out, &value.source_url);
        put_str(out, &value.license);
        put_str(out, &value.license_url);
        put_str(out, &value.terms_url);
        put_str(out, &value.disclaimer);
    }
}

fn encode_case_manifest(out: &mut Vec<u8>, value: &CaseRoomManifest) {
    put_str(out, &value.schema);
    put_str(out, &value.case_id);
    put_str(out, &value.title);
    put_i64(out, value.event_start_unix);
    put_i64(out, value.event_end_unix);
    put_i64(out, value.published_unix);
    put_i64(out, value.retain_until_unix);
    encode_publication(out, &value.publication);
    put_u32(out, value.sources.len() as u32);
    for source in &value.sources {
        put_str(out, &source.model);
        put_str(out, &source.run);
        put_str(out, &source.snapshot_id);
        put_str(out, &source.grid_hash);
        encode_provenance(out, &source.source_provenance);
    }
    put_u32(out, value.artifacts.len() as u32);
    for artifact in &value.artifacts {
        put_str(out, &artifact.artifact_id);
        put_u8(out, case_artifact_tag(artifact.artifact_type));
        put_str(out, &artifact.request_sha256);
        put_str(out, &artifact.object_sha256);
    }
    encode_attributions(out, &value.attributions);
    put_strings(out, &value.modification_notices);
}

fn encode_relay_credential_claims(out: &mut Vec<u8>, value: &RelayCredentialClaims) {
    put_str(out, &value.schema);
    put_str(out, &value.relay_id);
    put_str(out, &value.session_id);
    put_str(out, &value.subject_id);
    put_str(out, &value.object_sha256);
    put_u8(
        out,
        match value.direction {
            RelayDirection::Upload => 0,
            RelayDirection::Download => 1,
        },
    );
    put_i64(out, value.issued_unix);
    put_i64(out, value.not_before_unix);
    put_i64(out, value.expires_unix);
    put_u64(out, value.max_bytes);
    put_u32(out, value.max_chunks);
}

fn missing_policy_tag(value: MissingPolicy) -> u8 {
    match value {
        MissingPolicy::Strict => 0,
        MissingPolicy::Partial => 1,
    }
}
fn compression_tag(value: Compression) -> u8 {
    match value {
        Compression::None => 0,
        Compression::Gzip => 1,
        Compression::Zstd => 2,
    }
}
fn case_artifact_tag(value: CaseArtifactType) -> u8 {
    match value {
        CaseArtifactType::Annotation => 0,
        CaseArtifactType::DerivedTable => 1,
        CaseArtifactType::Overlay => 2,
        CaseArtifactType::RenderedImage => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(fill: char) -> String {
        std::iter::repeat_n(fill, 64).collect()
    }

    fn sample_request() -> ShareRequest {
        ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812T00Z".into(),
            snapshot_id: hash('a'),
            grid_hash: hash('b'),
            variables: vec![
                "dewpoint".into(),
                "dewpoint_2m".into(),
                "mslp".into(),
                "orography".into(),
                "surface_pressure".into(),
                "temperature".into(),
                "temperature_2m".into(),
                "u_10m".into(),
                "v_10m".into(),
            ],
            query: ShareQuery::Profile {
                latitude_e7: 389_000_000,
                longitude_e7: -1_045_000_000,
                storage_slot: 1,
                valid_unix: 1_786_496_400,
                pressure_variables: vec!["dewpoint".into(), "temperature".into()],
                surface_variables: vec![
                    "dewpoint_2m".into(),
                    "mslp".into(),
                    "orography".into(),
                    "surface_pressure".into(),
                    "temperature_2m".into(),
                    "u_10m".into(),
                    "v_10m".into(),
                ],
                pressure_levels_hpa: vec![500, 700, 850],
            },
            recipe: RecipeIdentity {
                recipe_id: "native-profile".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                roles: vec!["pressure".into()],
                products: vec!["wrfprs".into()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
    }

    fn sample_manifest(request: &ShareRequest, bytes: &[u8]) -> ObjectManifest {
        ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request: request.clone(),
            request_sha256: request_sha256(request).unwrap(),
            object_sha256: object_sha256(bytes),
            content_type: "application/json".into(),
            compression: Compression::None,
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            attributions: vec![],
            modification_notices: vec!["Derived by Rusty Weather.".into()],
            created_unix: 1_786_492_800,
            expires_unix: 1_786_579_200,
        }
    }

    fn sample_case(request: &ShareRequest) -> CaseRoomManifest {
        CaseRoomManifest {
            schema: CASE_SCHEMA.into(),
            case_id: "case-20260812-plains".into(),
            title: "Central Plains severe-weather analysis".into(),
            event_start_unix: 1_786_492_800,
            event_end_unix: 1_786_579_200,
            published_unix: 1_786_500_000,
            retain_until_unix: 1_789_092_800,
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            sources: vec![CaseModelSource {
                model: request.model.clone(),
                run: request.run.clone(),
                snapshot_id: request.snapshot_id.clone(),
                grid_hash: request.grid_hash.clone(),
                source_provenance: request.source_provenance.clone(),
            }],
            artifacts: vec![CaseArtifactRef {
                artifact_id: "sounding-001".into(),
                artifact_type: CaseArtifactType::DerivedTable,
                request_sha256: request_sha256(request).unwrap(),
                object_sha256: hash('d'),
            }],
            attributions: vec![],
            modification_notices: vec!["Derived by Rusty Weather.".into()],
        }
    }

    #[test]
    fn canonical_request_identity_is_order_independent_for_sets_and_run_specific() {
        let first = sample_request();
        let mut reordered = first.clone();
        reordered.variables.reverse();
        reordered.source_provenance[0].roles = vec!["surface".into(), "pressure".into()];
        let mut matching = first.clone();
        matching.source_provenance[0].roles.push("surface".into());
        assert_eq!(
            request_sha256(&reordered).unwrap(),
            request_sha256(&matching).unwrap()
        );
        let mut different_run = matching;
        different_run.run = "20260812T01Z".into();
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_run).unwrap()
        );
        let mut different_grid = first.clone();
        different_grid.grid_hash = hash('c');
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_grid).unwrap()
        );
        let mut different_recipe = first.clone();
        different_recipe.recipe.recipe_version = "2".into();
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_recipe).unwrap()
        );
        let mut different_snapshot = first.clone();
        different_snapshot.snapshot_id = hash('d');
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_snapshot).unwrap()
        );
        let mut different_valid_time = first.clone();
        if let ShareQuery::Profile { valid_unix, .. } = &mut different_valid_time.query {
            *valid_unix += 3600;
        }
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_valid_time).unwrap()
        );
        let mut different_variables = first.clone();
        if let ShareQuery::Profile {
            surface_variables, ..
        } = &mut different_variables.query
        {
            surface_variables.retain(|name| name != "mslp");
        }
        different_variables.variables.retain(|name| name != "mslp");
        assert_ne!(
            request_sha256(&first).unwrap(),
            request_sha256(&different_variables).unwrap()
        );
    }

    #[test]
    fn geographic_window_identity_binds_bbox_slot_grid_variables_and_levels() {
        let mut request = sample_request();
        request.variables = vec!["temperature".into()];
        request.query = ShareQuery::GeographicWindow {
            storage_slot: 2,
            valid_unix: 1_786_500_000,
            west_longitude_e7: 1_700_000_000,
            south_latitude_e7: -200_000_000,
            east_longitude_e7: -1_700_000_000,
            north_latitude_e7: 200_000_000,
            pressure_levels_hpa: vec![850, 500],
        };
        request.normalize();
        request.validate(&ProtocolLimits::default()).unwrap();
        let identity = request_sha256(&request).unwrap();
        let mut changed = request.clone();
        if let ShareQuery::GeographicWindow {
            west_longitude_e7, ..
        } = &mut changed.query
        {
            *west_longitude_e7 += 1;
        }
        assert_ne!(identity, request_sha256(&changed).unwrap());
        let mut changed = request.clone();
        changed.grid_hash = hash('f');
        assert_ne!(identity, request_sha256(&changed).unwrap());
        let mut changed = request.clone();
        if let ShareQuery::GeographicWindow {
            pressure_levels_hpa,
            ..
        } = &mut changed.query
        {
            pressure_levels_hpa.pop();
        }
        assert_ne!(identity, request_sha256(&changed).unwrap());
        let payload = TypedObjectPayload {
            schema: GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA.into(),
            request_sha256: identity,
            data: serde_json::json!({"schema": "rw.query.geographic-window.v1"}),
        };
        validate_typed_payload_identity(&payload, GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA, &request)
            .unwrap();
    }

    #[test]
    fn golden_canonical_request_and_signature_are_stable() {
        let request = sample_request();
        let canonical = canonical_request_bytes(&request).unwrap();
        // Length plus SHA-256 pins the exact fixed-field bytes without
        // embedding an unreadable base64 blob in this source file.
        assert_eq!(canonical.len(), 587);
        assert_eq!(
            sha256_hex(&canonical),
            "ccec1c8ed074fa35ded0c1af0b827843393eebaf683a506d229fe1e5acfdbcc1"
        );
        let bytes = br#"{"profile":"tiny"}"#;
        let key = SigningKey::from_bytes(&[7; 32]);
        let signed =
            sign_object_manifest(sample_manifest(&request, bytes), "origin-2026-a", &key).unwrap();
        assert_eq!(
            signed.signature.signature_base64,
            "bkeEK8oZWWa6mSTO/aEWYajc4eYe+6JhbrJ/KVL9sKwtN0t8Nsa2Mjob2EnK1a+SAa9xDg8QhpQSvwG4Q+jJAw=="
        );
    }

    #[test]
    fn signed_object_round_trip_and_tampering_fail_closed() {
        let request = sample_request();
        let bytes = br#"{"profile":"tiny"}"#;
        let key = SigningKey::from_bytes(&[9; 32]);
        let signed =
            sign_object_manifest(sample_manifest(&request, bytes), "origin-a", &key).unwrap();
        let keys = BTreeMap::from([("origin-a".into(), key.verifying_key())]);
        let now = signed.manifest.created_unix + 1;
        verify_signed_object(
            &signed,
            &request,
            bytes,
            now,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(
            verify_signed_object(
                &signed,
                &request,
                b"tampered",
                now,
                &keys,
                &ProtocolLimits::default()
            ),
            Err(ProtocolError::EncodedSizeMismatch)
        );
        let mut changed = signed.clone();
        changed.manifest.content_type = "image/png".into();
        assert_eq!(
            verify_signed_object(
                &changed,
                &request,
                bytes,
                now,
                &keys,
                &ProtocolLimits::default()
            ),
            Err(ProtocolError::InvalidSignature)
        );
        let mut changed_request = request.clone();
        changed_request.run = "20260812T06Z".into();
        assert_eq!(
            verify_signed_object(
                &signed,
                &changed_request,
                bytes,
                now,
                &keys,
                &ProtocolLimits::default()
            ),
            Err(ProtocolError::RequestHashMismatch)
        );
        assert_eq!(
            verify_signed_object(
                &signed,
                &request,
                bytes,
                signed.manifest.expires_unix,
                &keys,
                &ProtocolLimits::default()
            ),
            Err(ProtocolError::ManifestExpired)
        );
    }

    #[test]
    fn decompression_bomb_is_rejected_before_or_during_decode() {
        let request = sample_request();
        let mut manifest = sample_manifest(&request, &[1]);
        manifest.compression = Compression::Zstd;
        manifest.encoded_size = 1;
        manifest.decoded_size = 65;
        let limits = ProtocolLimits {
            max_decompression_ratio: 64,
            ..ProtocolLimits::default()
        };
        assert_eq!(
            validate_object_manifest(&manifest, &limits),
            Err(ProtocolError::DecompressionRatioLimit)
        );
        manifest.decoded_size = 64;
        let mut guard = DecodedSizeGuard::new(&manifest, &limits).unwrap();
        assert_eq!(guard.observe(65), Err(ProtocolError::DecodedSizeLimit));
    }

    #[test]
    fn private_wrf_and_arwen_are_default_deny() {
        for origin in [DataOrigin::PrivateWrf, DataOrigin::PrivateArwen] {
            let mut request = sample_request();
            request.publication = PublicationGrant {
                data_origin: origin,
                ..PublicationGrant::default()
            };
            assert_eq!(
                request.validate(&ProtocolLimits::default()),
                Err(ProtocolError::PrivatePublicationDenied)
            );
            request.publication.explicit_owner_publication = true;
            assert_eq!(
                request.validate(&ProtocolLimits::default()),
                Err(ProtocolError::RedistributionRightsUnconfirmed)
            );
            request.publication.redistribution_rights_confirmed = true;
            request.validate(&ProtocolLimits::default()).unwrap();
        }
    }

    #[test]
    fn candidate_contract_rejects_every_direct_ice_candidate_kind() {
        for kind in ["host", "srflx", "prflx", "direct"] {
            let json = format!(
                r#"{{"kind":"{kind}","relay_id":"relay-a","ticket_id":"ticket-a","expires_unix":99}}"#
            );
            assert!(serde_json::from_str::<RelayCandidate>(&json).is_err());
        }
        let unknown_field = r#"{"kind":"relay","relay_id":"relay-a","ticket_id":"ticket-a","expires_unix":99,"address":"203.0.113.2"}"#;
        assert!(serde_json::from_str::<RelayCandidate>(unknown_field).is_err());
    }

    #[test]
    fn app_visible_relay_state_cannot_encode_peer_ip() {
        let candidate = RelayCandidate {
            kind: RelayCandidateKind::Relay,
            relay_id: "203.0.113.2".into(),
            ticket_id: "ticket-a".into(),
            expires_unix: 99,
        };
        assert_eq!(
            candidate.validate(1),
            Err(ProtocolError::PeerAddressForbidden)
        );
        let safe = RelayCandidate {
            kind: RelayCandidateKind::Relay,
            relay_id: "cf-relay-west".into(),
            ticket_id: "ticket-a".into(),
            expires_unix: 99,
        };
        safe.validate(1).unwrap();
        let json = serde_json::to_string(&safe).unwrap();
        assert!(!json.contains("address"));
        assert!(!json.contains("203.0.113"));
    }

    #[test]
    fn ecmwf_objects_and_cases_require_attribution_and_modification_notice() {
        let mut request = sample_request();
        request.source_provenance[0].provider = "ecmwf-open-data".into();
        let mut manifest = sample_manifest(&request, b"x");
        assert_eq!(
            enforce_request_attributions(&request, &manifest),
            Err(ProtocolError::MissingEcmwfNotice)
        );
        manifest
            .attributions
            .push(AttributionNotice::ecmwf_open_data());
        enforce_request_attributions(&request, &manifest).unwrap();

        let mut case = sample_case(&request);
        assert_eq!(
            validate_case_manifest(&case, &ProtocolLimits::default()),
            Err(ProtocolError::MissingEcmwfNotice)
        );
        case.attributions.push(AttributionNotice::ecmwf_open_data());
        validate_case_manifest(&case, &ProtocolLimits::default()).unwrap();
        let key = SigningKey::from_bytes(&[11; 32]);
        let signed = sign_case_manifest(case, "origin-case-a", &key).unwrap();
        let keys = BTreeMap::from([("origin-case-a".into(), key.verifying_key())]);
        verify_signed_case(
            &signed,
            signed.manifest.published_unix + 1,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        let mut tampered = signed.clone();
        tampered.manifest.title.push('!');
        assert_eq!(
            verify_signed_case(
                &tampered,
                tampered.manifest.published_unix + 1,
                &keys,
                &ProtocolLimits::default(),
            ),
            Err(ProtocolError::InvalidSignature)
        );
    }

    #[test]
    fn unknown_versions_and_content_types_fail_closed() {
        let mut request = sample_request();
        request.schema = "rw.community.request.v2".into();
        assert!(matches!(
            request.validate(&ProtocolLimits::default()),
            Err(ProtocolError::UnsupportedSchema(_))
        ));
        let original = sample_request();
        let mut manifest = sample_manifest(&original, b"x");
        manifest.content_type = "application/octet-stream".into();
        assert!(validate_object_manifest(&manifest, &ProtocolLimits::default()).is_err());
    }

    #[test]
    fn bounded_strict_json_rejects_unknown_fields_and_oversize_manifests() {
        let request = sample_request();
        let bytes = b"x";
        let key = SigningKey::from_bytes(&[13; 32]);
        let signed =
            sign_object_manifest(sample_manifest(&request, bytes), "origin-a", &key).unwrap();
        let mut json = serde_json::to_value(&signed).unwrap();
        json["manifest"]["local_path"] = serde_json::json!("C:\\private\\wrfout");
        let json = serde_json::to_vec(&json).unwrap();
        assert_eq!(
            parse_signed_object_manifest_bounded(&json, &ProtocolLimits::default()),
            Err(ProtocolError::MalformedJson)
        );
        let small = ProtocolLimits {
            max_manifest_bytes: 2,
            ..ProtocolLimits::default()
        };
        assert_eq!(
            parse_signed_object_manifest_bounded(&serde_json::to_vec(&signed).unwrap(), &small),
            Err(ProtocolError::ManifestSizeLimit)
        );
    }

    #[test]
    fn shared_key_parser_rejects_malformed_and_duplicate_keys() {
        let key = SigningKey::from_bytes(&[17; 32]).verifying_key();
        let encoded = base64::engine::general_purpose::STANDARD.encode(key.to_bytes());
        assert_eq!(parse_verifying_key_base64(&encoded).unwrap(), key);
        assert_eq!(
            parse_verifying_key_base64("not-base64"),
            Err(ProtocolError::MalformedSignature)
        );
        assert!(matches!(
            trusted_signing_keys_from_base64([
                ("origin-a", encoded.as_str()),
                ("origin-a", encoded.as_str()),
            ]),
            Err(ProtocolError::InvalidField {
                field: "signing_key_id",
                ..
            })
        ));
    }

    #[test]
    fn profile_payload_surface_samples_must_match_signed_identity() {
        let request = sample_request();
        let ShareQuery::Profile {
            surface_variables, ..
        } = &request.query
        else {
            unreachable!()
        };
        let payload = ProfileObjectPayload {
            schema: PROFILE_PAYLOAD_SCHEMA.into(),
            request_sha256: request_sha256(&request).unwrap(),
            profile: serde_json::json!({"profile": "fixture"}),
            surface_samples: surface_variables
                .iter()
                .map(|name| SurfaceSample {
                    variable: name.clone(),
                    units: "fixture".into(),
                    value: Some(1.0),
                })
                .collect(),
        };
        validate_profile_payload_identity(&payload, &request).unwrap();
        let mut missing = payload;
        missing.surface_samples.pop();
        assert!(validate_profile_payload_identity(&missing, &request).is_err());
    }

    #[test]
    fn relay_credentials_are_signed_short_lived_and_object_scoped() {
        let limits = ProtocolLimits::default();
        let claims = RelayCredentialClaims {
            schema: RELAY_CREDENTIAL_SCHEMA.into(),
            relay_id: "cf-relay-west".into(),
            session_id: "session-a".into(),
            subject_id: "subject-a".into(),
            object_sha256: hash('e'),
            direction: RelayDirection::Download,
            issued_unix: 100,
            not_before_unix: 100,
            expires_unix: 700,
            max_bytes: 1024,
            max_chunks: 4,
        };
        let key = SigningKey::from_bytes(&[19; 32]);
        let signed = sign_relay_credential(claims, "relay-issuer-a", &key, 100, &limits).unwrap();
        let keys = BTreeMap::from([("relay-issuer-a".into(), key.verifying_key())]);
        verify_signed_relay_credential(&signed, 101, &keys, &limits).unwrap();
        assert_eq!(
            verify_signed_relay_credential(&signed, 700, &keys, &limits),
            Err(ProtocolError::RelayCredentialExpired)
        );
        let mut tampered = signed.clone();
        tampered.claims.object_sha256 = hash('f');
        assert_eq!(
            verify_signed_relay_credential(&tampered, 101, &keys, &limits),
            Err(ProtocolError::InvalidSignature)
        );
    }
}

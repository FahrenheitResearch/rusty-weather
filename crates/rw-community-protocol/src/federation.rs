//! Signed, transport-neutral discovery for deliberately public Rusty Weather
//! origins. This is separate from Community Cache: descriptors contain only
//! operator-approved institutional HTTPS endpoints and cannot carry ordinary
//! client, relay, ICE, STUN, or socket state.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use super::{
    ProtocolError, SignatureAlgorithm, SignatureBlock, TrustedSigningKeys, decode_base64, invalid,
    parse_verifying_key_base64, put_bool, put_i32, put_i64, put_str, put_strings, put_u8, put_u16,
    put_u16s, put_u32, put_u64,
};

pub const FEDERATION_ORIGIN_SCHEMA: &str = "rw.federation.origin.v1";
pub const FEDERATION_CATALOG_SCHEMA: &str = "rw.federation.catalog.v1";
pub const FEDERATION_CATALOG_PATH: &str = "/v1/federation/origins";
pub const FEDERATION_ORIGIN_PATH_TEMPLATE: &str = "/v1/federation/origins/{origin_id}";

const ORIGIN_SIGNATURE_DOMAIN: &[u8] = b"rw-federation-origin-signature-v1\0";
const CATALOG_SIGNATURE_DOMAIN: &[u8] = b"rw-federation-catalog-signature-v1\0";
const CLOCK_SKEW_SECONDS: i64 = 300;
const MAX_ADVERTISED_OBJECT_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_ADVERTISED_MONTHLY_BYTES: u64 = 1_000 * 1024 * 1024 * 1024 * 1024;
const MAX_RETENTION_HOURS: u32 = 20 * 366 * 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FederationLimits {
    pub max_descriptor_bytes: u64,
    pub max_catalog_bytes: u64,
    pub max_origins: usize,
    pub max_keys_per_usage: usize,
    pub max_models: usize,
    pub max_products_per_model: usize,
    pub max_total_products: usize,
    pub max_queries_per_product: usize,
    pub max_total_query_capabilities: usize,
    pub max_total_pressure_levels: usize,
    pub max_coverage_areas: usize,
    pub max_descriptor_lifetime_seconds: i64,
    pub max_catalog_lifetime_seconds: i64,
}

impl Default for FederationLimits {
    fn default() -> Self {
        Self {
            max_descriptor_bytes: 256 * 1024,
            max_catalog_bytes: 2 * 1024 * 1024,
            max_origins: 128,
            max_keys_per_usage: 8,
            max_models: 128,
            max_products_per_model: 512,
            max_total_products: 4_096,
            max_queries_per_product: 16,
            max_total_query_capabilities: 16_384,
            max_total_pressure_levels: 32_768,
            max_coverage_areas: 64,
            max_descriptor_lifetime_seconds: 7 * 24 * 60 * 60,
            max_catalog_lifetime_seconds: 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationQueryCapability {
    ModelCatalog,
    RunCatalog,
    PointSeries,
    Sounding,
    NativeWindow,
    ArbitraryDomainMap,
    TemporalGrid,
    Diurnal,
    Ensemble,
    CaseArtifact,
    ImmutableObject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationPublicKey {
    pub algorithm: SignatureAlgorithm,
    pub key_id: String,
    pub public_key_base64: String,
    pub not_before_unix: i64,
    pub expires_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationProductCapability {
    pub product: String,
    pub queries: Vec<FederationQueryCapability>,
    #[serde(default)]
    pub pressure_levels_hpa: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationModelCapability {
    pub model: String,
    pub products: Vec<FederationProductCapability>,
}

/// One rectangular advertised service area in fixed-point degrees. The
/// antimeridian is represented as two areas so containment remains exact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationCoverageArea {
    pub coverage_id: String,
    pub west_longitude_e7: i32,
    pub south_latitude_e7: i32,
    pub east_longitude_e7: i32,
    pub north_latitude_e7: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationRetentionSummary {
    pub queryable_run_hours: u32,
    pub immutable_object_hours: u32,
    pub published_case_hours: u32,
    pub previous_generations: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationQuotaSummary {
    pub maximum_request_bytes: u64,
    pub maximum_response_bytes: u64,
    pub requests_per_minute: u32,
    pub concurrent_requests: u16,
    pub monthly_egress_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationReplicationPolicy {
    pub accepts_replication: bool,
    pub maximum_object_bytes: u64,
    pub monthly_ingress_bytes: u64,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationPolicyLinks {
    pub attribution_url: String,
    pub acceptable_use_url: String,
    pub privacy_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicOriginDescriptor {
    pub schema: String,
    pub origin_id: String,
    pub display_name: String,
    /// Intentionally public conventional HTTPS origin root. It is never an
    /// ordinary Community Cache participant address.
    pub https_base_url: String,
    /// Same-origin relative path. A descriptor cannot redirect health probes
    /// to a different host.
    pub health_path: String,
    pub descriptor_signing_keys: Vec<FederationPublicKey>,
    pub object_signing_keys: Vec<FederationPublicKey>,
    pub models: Vec<FederationModelCapability>,
    pub geographic_coverage: Vec<FederationCoverageArea>,
    pub retention: FederationRetentionSummary,
    pub api_schema_version: String,
    pub build_version: String,
    pub issued_unix: i64,
    pub expires_unix: i64,
    pub policy_links: FederationPolicyLinks,
    pub replication: FederationReplicationPolicy,
    pub quotas: FederationQuotaSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedPublicOriginDescriptor {
    pub descriptor: PublicOriginDescriptor,
    pub signature: SignatureBlock,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationCatalog {
    pub schema: String,
    pub catalog_id: String,
    pub generated_unix: i64,
    pub expires_unix: i64,
    pub origins: Vec<SignedPublicOriginDescriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedFederationCatalog {
    pub catalog: FederationCatalog,
    pub signature: SignatureBlock,
}

/// Operator-controlled trust roots. Origins cannot add themselves: a
/// descriptor is accepted only when its id and signing key are already in
/// `approved_origins`.
#[derive(Debug, Clone, Default)]
pub struct FederationTrustStore {
    pub catalog_keys: TrustedSigningKeys,
    pub approved_origins: BTreeMap<String, TrustedSigningKeys>,
    pub revoked_origin_ids: BTreeSet<String>,
    pub revoked_key_ids: BTreeSet<String>,
}

impl PublicOriginDescriptor {
    pub fn normalize(&mut self) {
        self.schema = self.schema.trim().to_string();
        self.origin_id = self.origin_id.trim().to_ascii_lowercase();
        self.display_name = self.display_name.trim().to_string();
        self.api_schema_version = self.api_schema_version.trim().to_ascii_lowercase();
        self.build_version = self.build_version.trim().to_string();
        normalize_keys(&mut self.descriptor_signing_keys);
        normalize_keys(&mut self.object_signing_keys);
        for model in &mut self.models {
            model.model = model.model.trim().to_ascii_lowercase();
            for product in &mut model.products {
                product.product = product.product.trim().to_ascii_lowercase();
                product.queries.sort();
                product.queries.dedup();
                product.pressure_levels_hpa.sort_unstable();
                product.pressure_levels_hpa.dedup();
            }
            model.products.sort_by(|a, b| a.product.cmp(&b.product));
            model.products.dedup_by(|a, b| a.product == b.product);
        }
        self.models.sort_by(|a, b| a.model.cmp(&b.model));
        self.models.dedup_by(|a, b| a.model == b.model);
        self.geographic_coverage
            .sort_by(|a, b| a.coverage_id.cmp(&b.coverage_id));
        self.geographic_coverage
            .dedup_by(|a, b| a.coverage_id == b.coverage_id);
        for model in &mut self.replication.models {
            *model = model.trim().to_ascii_lowercase();
        }
        self.replication.models.sort();
        self.replication.models.dedup();
    }

    pub fn normalized(mut self) -> Self {
        self.normalize();
        self
    }

    pub fn validate(&self, limits: &FederationLimits) -> Result<(), ProtocolError> {
        if self.descriptor_signing_keys.is_empty()
            || self.descriptor_signing_keys.len() > limits.max_keys_per_usage
            || self.object_signing_keys.is_empty()
            || self.object_signing_keys.len() > limits.max_keys_per_usage
            || self.models.is_empty()
            || self.models.len() > limits.max_models
            || self.geographic_coverage.is_empty()
            || self.geographic_coverage.len() > limits.max_coverage_areas
        {
            return invalid(
                "federation descriptor",
                "contains an invalid top-level capability count",
            );
        }
        let mut total_products = 0usize;
        let mut total_queries = 0usize;
        let mut total_levels = 0usize;
        for model in &self.models {
            if model.products.is_empty() || model.products.len() > limits.max_products_per_model {
                return invalid(
                    "products",
                    "contains an invalid number of product capabilities",
                );
            }
            total_products = total_products
                .checked_add(model.products.len())
                .ok_or(ProtocolError::ManifestSizeLimit)?;
            for product in &model.products {
                if product.queries.is_empty()
                    || product.queries.len() > limits.max_queries_per_product
                {
                    return invalid(
                        "queries",
                        "contains an invalid number of query capabilities",
                    );
                }
                total_queries = total_queries
                    .checked_add(product.queries.len())
                    .ok_or(ProtocolError::ManifestSizeLimit)?;
                total_levels = total_levels
                    .checked_add(product.pressure_levels_hpa.len())
                    .ok_or(ProtocolError::ManifestSizeLimit)?;
            }
        }
        if total_products > limits.max_total_products
            || total_queries > limits.max_total_query_capabilities
            || total_levels > limits.max_total_pressure_levels
            || serde_json::to_vec(self)
                .map_err(|_| ProtocolError::MalformedJson)?
                .len() as u64
                > limits.max_descriptor_bytes
        {
            return Err(ProtocolError::ManifestSizeLimit);
        }
        let mut normalized = self.clone();
        normalized.normalize();
        if &normalized != self {
            return Err(ProtocolError::NonCanonical("federation origin descriptor"));
        }
        if self.schema != FEDERATION_ORIGIN_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_federation_id("origin_id", &self.origin_id, 96)?;
        validate_bounded_text("display_name", &self.display_name, 160)?;
        validate_public_https_url("https_base_url", &self.https_base_url, true)?;
        validate_health_path(&self.health_path)?;
        validate_keys(&self.descriptor_signing_keys, limits)?;
        validate_keys(&self.object_signing_keys, limits)?;
        if self.models.is_empty() || self.models.len() > limits.max_models {
            return invalid("models", "contains an invalid number of model capabilities");
        }
        for model in &self.models {
            validate_federation_id("model", &model.model, 96)?;
            if model.products.is_empty() || model.products.len() > limits.max_products_per_model {
                return invalid(
                    "products",
                    "contains an invalid number of product capabilities",
                );
            }
            for product in &model.products {
                validate_federation_id("product", &product.product, 128)?;
                if product.queries.is_empty()
                    || product.queries.len() > limits.max_queries_per_product
                {
                    return invalid(
                        "queries",
                        "contains an invalid number of query capabilities",
                    );
                }
                if product.pressure_levels_hpa.len() > 256
                    || product
                        .pressure_levels_hpa
                        .iter()
                        .any(|level| *level == 0 || *level > 1_200)
                {
                    return invalid("pressure_levels_hpa", "contains invalid pressure levels");
                }
            }
        }
        if self.geographic_coverage.is_empty()
            || self.geographic_coverage.len() > limits.max_coverage_areas
        {
            return invalid(
                "geographic_coverage",
                "contains an invalid number of coverage areas",
            );
        }
        for area in &self.geographic_coverage {
            validate_federation_id("coverage_id", &area.coverage_id, 96)?;
            if !(-1_800_000_000..=1_800_000_000).contains(&area.west_longitude_e7)
                || !(-1_800_000_000..=1_800_000_000).contains(&area.east_longitude_e7)
                || !(-900_000_000..=900_000_000).contains(&area.south_latitude_e7)
                || !(-900_000_000..=900_000_000).contains(&area.north_latitude_e7)
                || area.west_longitude_e7 >= area.east_longitude_e7
                || area.south_latitude_e7 >= area.north_latitude_e7
            {
                return invalid("geographic_coverage", "contains invalid fixed-point bounds");
            }
        }
        if self.retention.queryable_run_hours == 0
            || self.retention.immutable_object_hours == 0
            || self.retention.published_case_hours == 0
            || self.retention.queryable_run_hours > MAX_RETENTION_HOURS
            || self.retention.immutable_object_hours > MAX_RETENTION_HOURS
            || self.retention.published_case_hours > MAX_RETENTION_HOURS
            || self.retention.previous_generations > 1_024
        {
            return invalid(
                "retention",
                "contains invalid or unbounded retention values",
            );
        }
        validate_federation_id("api_schema_version", &self.api_schema_version, 64)?;
        validate_bounded_text("build_version", &self.build_version, 128)?;
        if self.issued_unix >= self.expires_unix
            || self.expires_unix.saturating_sub(self.issued_unix)
                > limits.max_descriptor_lifetime_seconds
        {
            return Err(ProtocolError::FederationExpired);
        }
        validate_public_https_url(
            "policy_links.attribution_url",
            &self.policy_links.attribution_url,
            false,
        )?;
        validate_public_https_url(
            "policy_links.acceptable_use_url",
            &self.policy_links.acceptable_use_url,
            false,
        )?;
        validate_public_https_url(
            "policy_links.privacy_url",
            &self.policy_links.privacy_url,
            false,
        )?;
        if self.replication.accepts_replication {
            if self.replication.maximum_object_bytes == 0
                || self.replication.maximum_object_bytes > MAX_ADVERTISED_OBJECT_BYTES
                || self.replication.monthly_ingress_bytes == 0
                || self.replication.monthly_ingress_bytes > MAX_ADVERTISED_MONTHLY_BYTES
                || self.replication.models.is_empty()
                || self.replication.models.len() > limits.max_models
            {
                return invalid(
                    "replication",
                    "enabled replication requires bounded limits and models",
                );
            }
            for model in &self.replication.models {
                validate_federation_id("replication.model", model, 96)?;
                if !self.models.iter().any(|item| &item.model == model) {
                    return invalid(
                        "replication.model",
                        "replication model is not an advertised capability",
                    );
                }
            }
        } else if self.replication.maximum_object_bytes != 0
            || self.replication.monthly_ingress_bytes != 0
            || !self.replication.models.is_empty()
        {
            return invalid(
                "replication",
                "disabled replication must not advertise capacity or models",
            );
        }
        if self.quotas.maximum_request_bytes == 0
            || self.quotas.maximum_request_bytes > 64 * 1024 * 1024
            || self.quotas.maximum_response_bytes == 0
            || self.quotas.maximum_response_bytes > MAX_ADVERTISED_OBJECT_BYTES
            || self.quotas.requests_per_minute == 0
            || self.quotas.requests_per_minute > 1_000_000
            || self.quotas.concurrent_requests == 0
            || self.quotas.concurrent_requests > 10_000
            || self.quotas.monthly_egress_bytes == 0
            || self.quotas.monthly_egress_bytes > MAX_ADVERTISED_MONTHLY_BYTES
        {
            return invalid("quotas", "all advertised quota limits must be non-zero");
        }
        Ok(())
    }
}

impl FederationCatalog {
    pub fn normalize(&mut self) {
        self.schema = self.schema.trim().to_string();
        self.catalog_id = self.catalog_id.trim().to_ascii_lowercase();
        self.origins.sort_by(|a, b| {
            a.descriptor
                .origin_id
                .cmp(&b.descriptor.origin_id)
                .then_with(|| a.signature.signing_key_id.cmp(&b.signature.signing_key_id))
        });
    }

    pub fn validate(&self, limits: &FederationLimits) -> Result<(), ProtocolError> {
        if self.origins.len() > limits.max_origins
            || serde_json::to_vec(self)
                .map_err(|_| ProtocolError::MalformedJson)?
                .len() as u64
                > limits.max_catalog_bytes
        {
            return Err(ProtocolError::ManifestSizeLimit);
        }
        let mut normalized = self.clone();
        normalized.normalize();
        if &normalized != self {
            return Err(ProtocolError::NonCanonical("federation catalog"));
        }
        if self.schema != FEDERATION_CATALOG_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_federation_id("catalog_id", &self.catalog_id, 96)?;
        if self.generated_unix >= self.expires_unix
            || self.expires_unix.saturating_sub(self.generated_unix)
                > limits.max_catalog_lifetime_seconds
            || self.origins.len() > limits.max_origins
        {
            return Err(ProtocolError::FederationExpired);
        }
        let mut ids = BTreeSet::new();
        for origin in &self.origins {
            origin.descriptor.validate(limits)?;
            if !ids.insert(&origin.descriptor.origin_id) {
                return invalid("origins", "contains a duplicate origin id");
            }
        }
        Ok(())
    }
}

pub fn canonical_public_origin_descriptor_bytes(
    descriptor: &PublicOriginDescriptor,
    signing_key_id: &str,
    limits: &FederationLimits,
) -> Result<Vec<u8>, ProtocolError> {
    descriptor.validate(limits)?;
    validate_federation_id("signing_key_id", signing_key_id, 128)?;
    let mut out = Vec::with_capacity(2_048);
    out.extend_from_slice(ORIGIN_SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    encode_origin_descriptor(&mut out, descriptor);
    Ok(out)
}

pub fn sign_public_origin_descriptor(
    mut descriptor: PublicOriginDescriptor,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
    limits: &FederationLimits,
) -> Result<SignedPublicOriginDescriptor, ProtocolError> {
    descriptor.normalize();
    let signing_key_id = signing_key_id.into();
    let listed = descriptor
        .descriptor_signing_keys
        .iter()
        .find(|key| key.key_id == signing_key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(signing_key_id.clone()))?;
    if parse_verifying_key_base64(&listed.public_key_base64)? != signing_key.verifying_key() {
        return Err(ProtocolError::UnknownSigningKey(signing_key_id));
    }
    let preimage = canonical_public_origin_descriptor_bytes(&descriptor, &signing_key_id, limits)?;
    let signature = signing_key.sign(&preimage);
    let signed = SignedPublicOriginDescriptor {
        descriptor,
        signature: signature_block(signing_key_id, signature),
    };
    if serde_json::to_vec(&signed)
        .map_err(|_| ProtocolError::MalformedJson)?
        .len() as u64
        > limits.max_descriptor_bytes
    {
        return Err(ProtocolError::ManifestSizeLimit);
    }
    Ok(signed)
}

pub fn verify_signed_public_origin_descriptor(
    signed: &SignedPublicOriginDescriptor,
    now_unix: i64,
    trust: &FederationTrustStore,
    limits: &FederationLimits,
) -> Result<(), ProtocolError> {
    signed.descriptor.validate(limits)?;
    if now_unix
        < signed
            .descriptor
            .issued_unix
            .saturating_sub(CLOCK_SKEW_SECONDS)
        || now_unix >= signed.descriptor.expires_unix
    {
        return Err(ProtocolError::FederationExpired);
    }
    let origin_id = &signed.descriptor.origin_id;
    if trust.revoked_origin_ids.contains(origin_id) {
        return Err(ProtocolError::RevokedFederationIdentity(origin_id.clone()));
    }
    let key_id = &signed.signature.signing_key_id;
    if trust.revoked_key_ids.contains(key_id) {
        return Err(ProtocolError::RevokedFederationIdentity(key_id.clone()));
    }
    let approved = trust
        .approved_origins
        .get(origin_id)
        .ok_or_else(|| ProtocolError::UntrustedFederationOrigin(origin_id.clone()))?;
    let key = approved
        .get(key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(key_id.clone()))?;
    let listed = signed
        .descriptor
        .descriptor_signing_keys
        .iter()
        .find(|item| &item.key_id == key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(key_id.clone()))?;
    if signed.descriptor.issued_unix < listed.not_before_unix
        || signed.descriptor.issued_unix >= listed.expires_unix
        || now_unix < listed.not_before_unix
        || now_unix >= listed.expires_unix
    {
        return Err(ProtocolError::FederationExpired);
    }
    if &parse_verifying_key_base64(&listed.public_key_base64)? != key {
        return Err(ProtocolError::UnknownSigningKey(key_id.clone()));
    }
    verify_signature(
        &signed.signature,
        key,
        &canonical_public_origin_descriptor_bytes(&signed.descriptor, key_id, limits)?,
    )
}

pub fn canonical_federation_catalog_bytes(
    catalog: &FederationCatalog,
    signing_key_id: &str,
    limits: &FederationLimits,
) -> Result<Vec<u8>, ProtocolError> {
    catalog.validate(limits)?;
    validate_federation_id("signing_key_id", signing_key_id, 128)?;
    let mut out = Vec::with_capacity(4_096);
    out.extend_from_slice(CATALOG_SIGNATURE_DOMAIN);
    put_str(&mut out, signing_key_id);
    encode_catalog(&mut out, catalog, limits)?;
    Ok(out)
}

pub fn sign_federation_catalog(
    mut catalog: FederationCatalog,
    signing_key_id: impl Into<String>,
    signing_key: &SigningKey,
    limits: &FederationLimits,
) -> Result<SignedFederationCatalog, ProtocolError> {
    catalog.normalize();
    let signing_key_id = signing_key_id.into();
    let preimage = canonical_federation_catalog_bytes(&catalog, &signing_key_id, limits)?;
    let signature = signing_key.sign(&preimage);
    let signed = SignedFederationCatalog {
        catalog,
        signature: signature_block(signing_key_id, signature),
    };
    if serde_json::to_vec(&signed)
        .map_err(|_| ProtocolError::MalformedJson)?
        .len() as u64
        > limits.max_catalog_bytes
    {
        return Err(ProtocolError::ManifestSizeLimit);
    }
    Ok(signed)
}

pub fn verify_signed_federation_catalog(
    signed: &SignedFederationCatalog,
    now_unix: i64,
    trust: &FederationTrustStore,
    limits: &FederationLimits,
) -> Result<(), ProtocolError> {
    signed.catalog.validate(limits)?;
    if now_unix
        < signed
            .catalog
            .generated_unix
            .saturating_sub(CLOCK_SKEW_SECONDS)
        || now_unix >= signed.catalog.expires_unix
    {
        return Err(ProtocolError::FederationExpired);
    }
    let key_id = &signed.signature.signing_key_id;
    if trust.revoked_key_ids.contains(key_id) {
        return Err(ProtocolError::RevokedFederationIdentity(key_id.clone()));
    }
    let key = trust
        .catalog_keys
        .get(key_id)
        .ok_or_else(|| ProtocolError::UnknownSigningKey(key_id.clone()))?;
    verify_signature(
        &signed.signature,
        key,
        &canonical_federation_catalog_bytes(&signed.catalog, key_id, limits)?,
    )?;
    for origin in &signed.catalog.origins {
        verify_signed_public_origin_descriptor(origin, now_unix, trust, limits)?;
    }
    Ok(())
}

pub fn parse_signed_public_origin_descriptor_bounded(
    bytes: &[u8],
    limits: &FederationLimits,
) -> Result<SignedPublicOriginDescriptor, ProtocolError> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_descriptor_bytes {
        return Err(ProtocolError::ManifestSizeLimit);
    }
    let signed: SignedPublicOriginDescriptor =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    signed.descriptor.validate(limits)?;
    validate_federation_id("signing_key_id", &signed.signature.signing_key_id, 128)?;
    Ok(signed)
}

pub fn parse_signed_federation_catalog_bounded(
    bytes: &[u8],
    limits: &FederationLimits,
) -> Result<SignedFederationCatalog, ProtocolError> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_catalog_bytes {
        return Err(ProtocolError::ManifestSizeLimit);
    }
    let signed: SignedFederationCatalog =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    signed.catalog.validate(limits)?;
    validate_federation_id("signing_key_id", &signed.signature.signing_key_id, 128)?;
    Ok(signed)
}

fn normalize_keys(keys: &mut Vec<FederationPublicKey>) {
    for key in keys.iter_mut() {
        key.key_id = key.key_id.trim().to_ascii_lowercase();
        key.public_key_base64 = key.public_key_base64.trim().to_string();
    }
    keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    keys.dedup_by(|a, b| a.key_id == b.key_id);
}

fn validate_keys(
    keys: &[FederationPublicKey],
    limits: &FederationLimits,
) -> Result<(), ProtocolError> {
    if keys.is_empty() || keys.len() > limits.max_keys_per_usage {
        return invalid("signing_keys", "contains an invalid number of keys");
    }
    for key in keys {
        validate_federation_id("key_id", &key.key_id, 128)?;
        parse_verifying_key_base64(&key.public_key_base64)?;
        if key.not_before_unix >= key.expires_unix
            || key.expires_unix.saturating_sub(key.not_before_unix)
                > 366_i64.saturating_mul(24 * 60 * 60)
        {
            return Err(ProtocolError::FederationExpired);
        }
    }
    Ok(())
}

fn signature_block(signing_key_id: String, signature: Signature) -> SignatureBlock {
    SignatureBlock {
        algorithm: SignatureAlgorithm::Ed25519,
        signing_key_id,
        signature_base64: base64::engine::general_purpose::STANDARD.encode(signature.to_bytes()),
    }
}

fn verify_signature(
    block: &SignatureBlock,
    key: &VerifyingKey,
    preimage: &[u8],
) -> Result<(), ProtocolError> {
    let bytes =
        decode_base64(&block.signature_base64).map_err(|_| ProtocolError::MalformedSignature)?;
    let signature = Signature::from_slice(&bytes).map_err(|_| ProtocolError::MalformedSignature)?;
    key.verify(preimage, &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

fn validate_federation_id(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || value.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.'))
        })
        || value.starts_with(['-', '_', '.'])
        || value.ends_with(['-', '_', '.'])
    {
        return invalid(field, "must be a bounded canonical lowercase identifier");
    }
    Ok(())
}

fn validate_bounded_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return invalid(field, "must be bounded, trimmed, non-control text");
    }
    Ok(())
}

/// Intentionally conservative URL grammar. Origin endpoints must use DNS
/// names (not IP literals), standard HTTPS, no credentials, and no local or
/// special-use suffix. Callers that connect MUST additionally resolve DNS and
/// reject non-global answers to close DNS-rebinding/TOCTOU gaps.
fn validate_public_https_url(
    _field: &'static str,
    value: &str,
    origin_root_only: bool,
) -> Result<(), ProtocolError> {
    if value.len() > 512
        || !value.is_ascii()
        || !value.starts_with("https://")
        || value
            .chars()
            .any(|ch| ch.is_ascii_control() || ch.is_ascii_whitespace())
        || value.contains(['\\', '@', '#'])
    {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    let remainder = &value[8..];
    let authority_end = remainder.find(['/', '?']).unwrap_or(remainder.len());
    let authority = &remainder[..authority_end];
    let suffix = &remainder[authority_end..];
    if authority.is_empty() || authority.len() > 253 || authority.contains(':') {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    if authority != authority.to_ascii_lowercase()
        || !authority.contains('.')
        || authority
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    let forbidden_suffixes = [
        "localhost",
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home",
        ".test",
        ".invalid",
        ".example",
        ".onion",
    ];
    if forbidden_suffixes
        .iter()
        .any(|suffix| authority == suffix.trim_start_matches('.') || authority.ends_with(suffix))
        || authority.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    if origin_root_only {
        if !suffix.is_empty() {
            return Err(ProtocolError::UnsafeFederationUrl);
        }
    } else if suffix.is_empty()
        || !suffix.starts_with('/')
        || suffix.contains('?')
        || suffix.contains("//")
        || suffix.to_ascii_lowercase().contains("%2e")
        || suffix.split('/').any(|part| matches!(part, "." | ".."))
    {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    Ok(())
}

fn validate_health_path(value: &str) -> Result<(), ProtocolError> {
    if value.len() > 160
        || !value.is_ascii()
        || !value.starts_with("/v1/")
        || value.contains(['\\', '?', '#'])
        || value.contains("//")
        || value.to_ascii_lowercase().contains("%2e")
        || value.split('/').any(|part| matches!(part, "." | ".."))
        || value
            .chars()
            .any(|ch| ch.is_ascii_control() || ch.is_ascii_whitespace())
    {
        return Err(ProtocolError::UnsafeFederationUrl);
    }
    Ok(())
}

fn encode_origin_descriptor(out: &mut Vec<u8>, value: &PublicOriginDescriptor) {
    put_str(out, &value.schema);
    put_str(out, &value.origin_id);
    put_str(out, &value.display_name);
    put_str(out, &value.https_base_url);
    put_str(out, &value.health_path);
    encode_keys(out, &value.descriptor_signing_keys);
    encode_keys(out, &value.object_signing_keys);
    put_u32(out, value.models.len() as u32);
    for model in &value.models {
        put_str(out, &model.model);
        put_u32(out, model.products.len() as u32);
        for product in &model.products {
            put_str(out, &product.product);
            put_u32(out, product.queries.len() as u32);
            for query in &product.queries {
                put_u8(out, query_tag(*query));
            }
            put_u16s(out, &product.pressure_levels_hpa);
        }
    }
    put_u32(out, value.geographic_coverage.len() as u32);
    for area in &value.geographic_coverage {
        put_str(out, &area.coverage_id);
        put_i32(out, area.west_longitude_e7);
        put_i32(out, area.south_latitude_e7);
        put_i32(out, area.east_longitude_e7);
        put_i32(out, area.north_latitude_e7);
    }
    put_u32(out, value.retention.queryable_run_hours);
    put_u32(out, value.retention.immutable_object_hours);
    put_u32(out, value.retention.published_case_hours);
    put_u16(out, value.retention.previous_generations);
    put_str(out, &value.api_schema_version);
    put_str(out, &value.build_version);
    put_i64(out, value.issued_unix);
    put_i64(out, value.expires_unix);
    put_str(out, &value.policy_links.attribution_url);
    put_str(out, &value.policy_links.acceptable_use_url);
    put_str(out, &value.policy_links.privacy_url);
    put_bool(out, value.replication.accepts_replication);
    put_u64(out, value.replication.maximum_object_bytes);
    put_u64(out, value.replication.monthly_ingress_bytes);
    put_strings(out, &value.replication.models);
    put_u64(out, value.quotas.maximum_request_bytes);
    put_u64(out, value.quotas.maximum_response_bytes);
    put_u32(out, value.quotas.requests_per_minute);
    put_u16(out, value.quotas.concurrent_requests);
    put_u64(out, value.quotas.monthly_egress_bytes);
}

fn encode_keys(out: &mut Vec<u8>, keys: &[FederationPublicKey]) {
    put_u32(out, keys.len() as u32);
    for key in keys {
        put_u8(out, 0); // Ed25519
        put_str(out, &key.key_id);
        put_str(out, &key.public_key_base64);
        put_i64(out, key.not_before_unix);
        put_i64(out, key.expires_unix);
    }
}

fn encode_catalog(
    out: &mut Vec<u8>,
    value: &FederationCatalog,
    limits: &FederationLimits,
) -> Result<(), ProtocolError> {
    put_str(out, &value.schema);
    put_str(out, &value.catalog_id);
    put_i64(out, value.generated_unix);
    put_i64(out, value.expires_unix);
    put_u32(out, value.origins.len() as u32);
    for origin in &value.origins {
        let bytes = canonical_public_origin_descriptor_bytes(
            &origin.descriptor,
            &origin.signature.signing_key_id,
            limits,
        )?;
        put_u32(out, bytes.len() as u32);
        out.extend_from_slice(&bytes);
        put_u8(out, 0); // Ed25519
        put_str(out, &origin.signature.signing_key_id);
        put_str(out, &origin.signature.signature_base64);
    }
    Ok(())
}

fn query_tag(value: FederationQueryCapability) -> u8 {
    match value {
        FederationQueryCapability::ModelCatalog => 0,
        FederationQueryCapability::RunCatalog => 1,
        FederationQueryCapability::PointSeries => 2,
        FederationQueryCapability::Sounding => 3,
        FederationQueryCapability::NativeWindow => 4,
        FederationQueryCapability::ArbitraryDomainMap => 5,
        FederationQueryCapability::TemporalGrid => 6,
        FederationQueryCapability::Diurnal => 7,
        FederationQueryCapability::Ensemble => 8,
        FederationQueryCapability::CaseArtifact => 9,
        FederationQueryCapability::ImmutableObject => 10,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_786_500_000;

    fn key(id: &str, seed: u8) -> (FederationPublicKey, SigningKey) {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        (
            FederationPublicKey {
                algorithm: SignatureAlgorithm::Ed25519,
                key_id: id.into(),
                public_key_base64: base64::engine::general_purpose::STANDARD
                    .encode(signing.verifying_key().to_bytes()),
                not_before_unix: NOW - 60,
                expires_unix: NOW + 86_400,
            },
            signing,
        )
    }

    fn descriptor(keys: Vec<FederationPublicKey>) -> PublicOriginDescriptor {
        PublicOriginDescriptor {
            schema: FEDERATION_ORIGIN_SCHEMA.into(),
            origin_id: "university-weather-lab".into(),
            display_name: "University Weather Lab".into(),
            https_base_url: "https://weather.example.edu".into(),
            health_path: "/v1/health/ready".into(),
            descriptor_signing_keys: keys.clone(),
            object_signing_keys: keys,
            models: vec![FederationModelCapability {
                model: "hrrr".into(),
                products: vec![FederationProductCapability {
                    product: "native".into(),
                    queries: vec![
                        FederationQueryCapability::Sounding,
                        FederationQueryCapability::ArbitraryDomainMap,
                    ],
                    pressure_levels_hpa: vec![500, 700, 850],
                }],
            }],
            geographic_coverage: vec![FederationCoverageArea {
                coverage_id: "conus".into(),
                west_longitude_e7: -1_300_000_000,
                south_latitude_e7: 200_000_000,
                east_longitude_e7: -600_000_000,
                north_latitude_e7: 550_000_000,
            }],
            retention: FederationRetentionSummary {
                queryable_run_hours: 72,
                immutable_object_hours: 720,
                published_case_hours: 8_760,
                previous_generations: 1,
            },
            api_schema_version: "rw-api-v1".into(),
            build_version: "0.5.0+abc123".into(),
            issued_unix: NOW - 30,
            expires_unix: NOW + 3_600,
            policy_links: FederationPolicyLinks {
                attribution_url: "https://weather.example.edu/attribution".into(),
                acceptable_use_url: "https://weather.example.edu/policy".into(),
                privacy_url: "https://weather.example.edu/privacy".into(),
            },
            replication: FederationReplicationPolicy {
                accepts_replication: true,
                maximum_object_bytes: 64 * 1024 * 1024,
                monthly_ingress_bytes: 1024 * 1024 * 1024,
                models: vec!["hrrr".into()],
            },
            quotas: FederationQuotaSummary {
                maximum_request_bytes: 1024 * 1024,
                maximum_response_bytes: 64 * 1024 * 1024,
                requests_per_minute: 120,
                concurrent_requests: 8,
                monthly_egress_bytes: 10 * 1024 * 1024 * 1024,
            },
        }
    }

    fn trust(
        origin_keys: &[(&str, VerifyingKey)],
        catalog: (&str, VerifyingKey),
    ) -> FederationTrustStore {
        FederationTrustStore {
            catalog_keys: BTreeMap::from([(catalog.0.into(), catalog.1)]),
            approved_origins: BTreeMap::from([(
                "university-weather-lab".into(),
                origin_keys
                    .iter()
                    .map(|(id, key)| ((*id).into(), *key))
                    .collect(),
            )]),
            ..FederationTrustStore::default()
        }
    }

    #[test]
    fn signed_descriptor_and_catalog_fail_on_tamper_expiry_and_untrusted_origin() {
        let (public, origin_key) = key("lab-2026-a", 7);
        let signed = sign_public_origin_descriptor(
            descriptor(vec![public]),
            "lab-2026-a",
            &origin_key,
            &FederationLimits::default(),
        )
        .unwrap();
        let catalog_key = SigningKey::from_bytes(&[9; 32]);
        let trust = trust(
            &[("lab-2026-a", origin_key.verifying_key())],
            ("catalog-2026-a", catalog_key.verifying_key()),
        );
        verify_signed_public_origin_descriptor(&signed, NOW, &trust, &FederationLimits::default())
            .unwrap();
        let catalog = sign_federation_catalog(
            FederationCatalog {
                schema: FEDERATION_CATALOG_SCHEMA.into(),
                catalog_id: "fahrenheit-public-origins".into(),
                generated_unix: NOW,
                expires_unix: NOW + 300,
                origins: vec![signed.clone()],
            },
            "catalog-2026-a",
            &catalog_key,
            &FederationLimits::default(),
        )
        .unwrap();
        verify_signed_federation_catalog(&catalog, NOW, &trust, &FederationLimits::default())
            .unwrap();
        let mut tampered_catalog = catalog.clone();
        tampered_catalog.catalog.catalog_id = "different-approved-catalog".into();
        assert_eq!(
            verify_signed_federation_catalog(
                &tampered_catalog,
                NOW,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_signed_federation_catalog(
                &catalog,
                catalog.catalog.expires_unix,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::FederationExpired)
        );

        let mut tampered = signed.clone();
        tampered.descriptor.quotas.concurrent_requests += 1;
        assert_eq!(
            verify_signed_public_origin_descriptor(
                &tampered,
                NOW,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::InvalidSignature)
        );
        assert_eq!(
            verify_signed_public_origin_descriptor(
                &signed,
                signed.descriptor.expires_unix,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::FederationExpired)
        );
        let mut untrusted = trust.clone();
        untrusted.approved_origins.clear();
        assert_eq!(
            verify_signed_public_origin_descriptor(
                &signed,
                NOW,
                &untrusted,
                &FederationLimits::default()
            ),
            Err(ProtocolError::UntrustedFederationOrigin(
                "university-weather-lab".into()
            ))
        );
    }

    #[test]
    fn canonical_descriptor_and_catalog_signatures_are_deterministic() {
        let (public, origin_key) = key("lab-2026-a", 7);
        let mut value = descriptor(vec![public]);
        value.models[0].products[0].queries.reverse();
        value.replication.models.push("hrrr".into());
        let first = sign_public_origin_descriptor(
            value.clone(),
            "lab-2026-a",
            &origin_key,
            &FederationLimits::default(),
        )
        .unwrap();
        let second = sign_public_origin_descriptor(
            value,
            "lab-2026-a",
            &origin_key,
            &FederationLimits::default(),
        )
        .unwrap();
        assert_eq!(first, second);

        let catalog_key = SigningKey::from_bytes(&[9; 32]);
        let catalog = FederationCatalog {
            schema: FEDERATION_CATALOG_SCHEMA.into(),
            catalog_id: "fahrenheit-public-origins".into(),
            generated_unix: NOW,
            expires_unix: NOW + 300,
            origins: vec![first],
        };
        assert_eq!(
            sign_federation_catalog(
                catalog.clone(),
                "catalog-2026-a",
                &catalog_key,
                &FederationLimits::default()
            )
            .unwrap(),
            sign_federation_catalog(
                catalog,
                "catalog-2026-a",
                &catalog_key,
                &FederationLimits::default()
            )
            .unwrap()
        );
    }

    #[test]
    fn private_local_and_ambiguous_urls_fail_ssrf_policy() {
        let (public, _) = key("lab-2026-a", 7);
        for url in [
            "http://weather.example.edu",
            "https://127.0.0.1",
            "https://10.0.0.1",
            "https://[::1]",
            "https://localhost",
            "https://weather.local",
            "https://user@weather.example.edu",
            "https://weather.example.edu/redirect",
        ] {
            let mut value = descriptor(vec![public.clone()]);
            value.https_base_url = url.into();
            assert_eq!(
                value.validate(&FederationLimits::default()),
                Err(ProtocolError::UnsafeFederationUrl),
                "{url} must fail closed"
            );
        }
        let mut value = descriptor(vec![public]);
        value.health_path = "https://attacker.example.org/health".into();
        assert_eq!(
            value.validate(&FederationLimits::default()),
            Err(ProtocolError::UnsafeFederationUrl)
        );
    }

    #[test]
    fn key_rotation_and_revocation_require_operator_approval() {
        let (old_public, old) = key("lab-2026-a", 7);
        let (new_public, new) = key("lab-2026-b", 8);
        let signed = sign_public_origin_descriptor(
            descriptor(vec![old_public, new_public]),
            "lab-2026-b",
            &new,
            &FederationLimits::default(),
        )
        .unwrap();
        let catalog = SigningKey::from_bytes(&[9; 32]);
        let mut trust = trust(
            &[
                ("lab-2026-a", old.verifying_key()),
                ("lab-2026-b", new.verifying_key()),
            ],
            ("catalog-2026-a", catalog.verifying_key()),
        );
        trust.revoked_key_ids.insert("lab-2026-a".into());
        verify_signed_public_origin_descriptor(&signed, NOW, &trust, &FederationLimits::default())
            .unwrap();
        trust.revoked_key_ids.insert("lab-2026-b".into());
        assert_eq!(
            verify_signed_public_origin_descriptor(
                &signed,
                NOW,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::RevokedFederationIdentity(
                "lab-2026-b".into()
            ))
        );
        trust.revoked_key_ids.remove("lab-2026-b");
        trust
            .revoked_origin_ids
            .insert("university-weather-lab".into());
        assert_eq!(
            verify_signed_public_origin_descriptor(
                &signed,
                NOW,
                &trust,
                &FederationLimits::default()
            ),
            Err(ProtocolError::RevokedFederationIdentity(
                "university-weather-lab".into()
            ))
        );
    }

    #[test]
    fn malicious_capability_counts_and_unknown_address_fields_fail_closed() {
        let (public, _) = key("lab-2026-a", 7);
        let mut value = descriptor(vec![public]);
        value.models[0].products[0].queries = vec![FederationQueryCapability::Sounding; 17];
        assert!(value.validate(&FederationLimits::default()).is_err());

        let (public, origin) = key("lab-2026-a", 7);
        let signed = sign_public_origin_descriptor(
            descriptor(vec![public]),
            "lab-2026-a",
            &origin,
            &FederationLimits::default(),
        )
        .unwrap();
        let mut json = serde_json::to_value(&signed).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("peer_ip".into(), serde_json::json!("203.0.113.9"));
        assert_eq!(
            parse_signed_public_origin_descriptor_bounded(
                &serde_json::to_vec(&json).unwrap(),
                &FederationLimits::default()
            ),
            Err(ProtocolError::MalformedJson)
        );
        let safe_json = serde_json::to_string(&signed).unwrap().to_ascii_lowercase();
        for forbidden in ["peer_ip", "relay_id", "ice_candidate", "stun"] {
            assert!(!safe_json.contains(forbidden));
        }
    }
}

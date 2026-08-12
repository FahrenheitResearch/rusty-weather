//! Operator-approved discovery and bounded failover for deliberately public
//! Rusty Weather origins. This module never discovers or represents ordinary
//! Community Cache clients.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rw_community_protocol::{
    FEDERATION_CATALOG_SCHEMA, FederationCatalog, FederationCoverageArea, FederationLimits,
    FederationQueryCapability, FederationTrustStore, ProtocolError, SignedFederationCatalog,
    SignedPublicOriginDescriptor, parse_signed_public_origin_descriptor_bounded,
    parse_verifying_key_base64, sign_federation_catalog, verify_signed_federation_catalog,
    verify_signed_public_origin_descriptor,
};
use thiserror::Error;

use crate::config::FederationConfig;

const MAX_SECRET_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum FederationError {
    #[error("public-origin federation is disabled")]
    Disabled,
    #[error("federated origin was not found")]
    NotFound,
    #[error("invalid federation configuration or state: {0}")]
    Invalid(String),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FederationHealthObservation {
    Healthy,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationSelectionBounds {
    pub west_longitude_e7: i32,
    pub south_latitude_e7: i32,
    pub east_longitude_e7: i32,
    pub north_latitude_e7: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationSelectionRequest {
    pub model: String,
    pub product: String,
    pub query: FederationQueryCapability,
    pub bounds: Option<FederationSelectionBounds>,
    pub minimum_response_bytes: u64,
    pub require_replication: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFederatedOrigin {
    pub origin_id: String,
    pub https_base_url: String,
    pub health_url: String,
    pub consecutive_failures: u32,
}

#[derive(Debug, Clone, Default)]
struct HealthRecord {
    consecutive_failures: u32,
    quarantine_until_unix: i64,
}

#[derive(Clone, Default)]
pub struct FederationService {
    inner: Option<Arc<FederationInner>>,
}

struct FederationInner {
    catalog_id: String,
    catalog_signing_key_id: String,
    catalog_signing_key: SigningKey,
    descriptors: BTreeMap<String, SignedPublicOriginDescriptor>,
    trust: FederationTrustStore,
    limits: FederationLimits,
    catalog_ttl_seconds: i64,
    health_failure_threshold: u32,
    health_quarantine_seconds: i64,
    maximum_selection_results: usize,
    health: Mutex<BTreeMap<String, HealthRecord>>,
}

impl std::fmt::Debug for FederationService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FederationService")
            .field("enabled", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

impl FederationService {
    pub fn open(config: &FederationConfig) -> Result<Self, FederationError> {
        if !config.enabled {
            return Ok(Self::default());
        }
        let limits = FederationLimits::default();
        let catalog_signing_key_file = config
            .catalog_signing_key_file
            .as_deref()
            .ok_or_else(|| FederationError::Invalid("catalog signing key is absent".into()))?;
        let catalog_signing_key = load_signing_key(catalog_signing_key_file)?;
        let mut trust = FederationTrustStore {
            catalog_keys: BTreeMap::from([(
                config.catalog_signing_key_id.clone(),
                catalog_signing_key.verifying_key(),
            )]),
            revoked_origin_ids: config.revoked_origin_ids.iter().cloned().collect(),
            revoked_key_ids: config.revoked_key_ids.iter().cloned().collect(),
            ..FederationTrustStore::default()
        };
        if trust
            .revoked_key_ids
            .contains(&config.catalog_signing_key_id)
        {
            return Err(FederationError::Invalid(
                "active catalog signing key is revoked".into(),
            ));
        }
        for approved in &config.approved_origins {
            let mut keys = BTreeMap::new();
            for item in &approved.descriptor_signing_keys {
                let key = parse_verifying_key_base64(&item.public_key_base64)?;
                if keys.insert(item.key_id.clone(), key).is_some() {
                    return Err(FederationError::Invalid(format!(
                        "duplicate key for approved origin '{}'",
                        approved.origin_id
                    )));
                }
            }
            if trust
                .approved_origins
                .insert(approved.origin_id.clone(), keys)
                .is_some()
            {
                return Err(FederationError::Invalid(format!(
                    "duplicate approved origin '{}'",
                    approved.origin_id
                )));
            }
        }

        let now = now_unix();
        let mut descriptors = BTreeMap::new();
        for path in &config.descriptor_files {
            let bytes = read_bounded_regular_file(path, limits.max_descriptor_bytes)?;
            let signed = parse_signed_public_origin_descriptor_bounded(&bytes, &limits)?;
            let origin_id = signed.descriptor.origin_id.clone();
            if !trust.approved_origins.contains_key(&origin_id) {
                return Err(ProtocolError::UntrustedFederationOrigin(origin_id).into());
            }
            // Revocation is an emergency exclusion mechanism, not a reason to
            // prevent the conventional origin service from starting.
            if trust.revoked_origin_ids.contains(&origin_id)
                || trust
                    .revoked_key_ids
                    .contains(&signed.signature.signing_key_id)
            {
                continue;
            }
            verify_signed_public_origin_descriptor(&signed, now, &trust, &limits)?;
            if descriptors.insert(origin_id.clone(), signed).is_some() {
                return Err(FederationError::Invalid(format!(
                    "duplicate descriptor for origin '{origin_id}'"
                )));
            }
        }
        let approved_ids = trust
            .approved_origins
            .iter()
            .filter(|(origin_id, keys)| {
                !trust.revoked_origin_ids.contains(*origin_id)
                    && keys
                        .keys()
                        .any(|key_id| !trust.revoked_key_ids.contains(key_id))
            })
            .map(|(origin_id, _)| origin_id.clone())
            .collect::<BTreeSet<_>>();
        let descriptor_ids = descriptors.keys().cloned().collect::<BTreeSet<_>>();
        if approved_ids != descriptor_ids {
            return Err(FederationError::Invalid(
                "approved origin ids and provisioned descriptor ids must match exactly".into(),
            ));
        }

        let inner = FederationInner {
            catalog_id: config.catalog_id.clone(),
            catalog_signing_key_id: config.catalog_signing_key_id.clone(),
            catalog_signing_key,
            descriptors,
            trust,
            limits,
            catalog_ttl_seconds: i64::try_from(config.catalog_ttl_seconds)
                .map_err(|_| FederationError::Invalid("catalog TTL is too large".into()))?,
            health_failure_threshold: config.health_failure_threshold,
            health_quarantine_seconds: i64::try_from(config.health_quarantine_seconds)
                .map_err(|_| FederationError::Invalid("health quarantine is too large".into()))?,
            maximum_selection_results: config.maximum_selection_results,
            health: Mutex::new(BTreeMap::new()),
        };
        let service = Self {
            inner: Some(Arc::new(inner)),
        };
        // Exercise the complete catalog signature chain at startup.
        service.catalog_at(now)?;
        Ok(service)
    }

    pub fn catalog(&self) -> Result<SignedFederationCatalog, FederationError> {
        self.catalog_at(now_unix())
    }

    pub fn descriptor(
        &self,
        origin_id: &str,
    ) -> Result<SignedPublicOriginDescriptor, FederationError> {
        self.descriptor_at(origin_id, now_unix())
    }

    pub fn select(
        &self,
        request: &FederationSelectionRequest,
    ) -> Result<Vec<SelectedFederatedOrigin>, FederationError> {
        self.select_at(request, now_unix())
    }

    pub fn record_health(
        &self,
        origin_id: &str,
        observation: FederationHealthObservation,
    ) -> Result<(), FederationError> {
        self.record_health_at(origin_id, observation, now_unix())
    }

    fn catalog_at(&self, now: i64) -> Result<SignedFederationCatalog, FederationError> {
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        let mut origins = Vec::with_capacity(inner.descriptors.len());
        for descriptor in inner.descriptors.values() {
            match verify_signed_public_origin_descriptor(
                descriptor,
                now,
                &inner.trust,
                &inner.limits,
            ) {
                Ok(()) => origins.push(descriptor.clone()),
                Err(ProtocolError::FederationExpired) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        let descriptor_expiry = origins
            .iter()
            .map(|item| item.descriptor.expires_unix)
            .min()
            .unwrap_or_else(|| now.saturating_add(inner.catalog_ttl_seconds));
        let expires_unix = now
            .saturating_add(inner.catalog_ttl_seconds)
            .min(descriptor_expiry);
        let signed = sign_federation_catalog(
            FederationCatalog {
                schema: FEDERATION_CATALOG_SCHEMA.into(),
                catalog_id: inner.catalog_id.clone(),
                generated_unix: now,
                expires_unix,
                origins,
            },
            inner.catalog_signing_key_id.clone(),
            &inner.catalog_signing_key,
            &inner.limits,
        )?;
        verify_signed_federation_catalog(&signed, now, &inner.trust, &inner.limits)?;
        Ok(signed)
    }

    fn descriptor_at(
        &self,
        origin_id: &str,
        now: i64,
    ) -> Result<SignedPublicOriginDescriptor, FederationError> {
        validate_selection_id(origin_id)?;
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        let signed = inner
            .descriptors
            .get(origin_id)
            .ok_or(FederationError::NotFound)?;
        verify_signed_public_origin_descriptor(signed, now, &inner.trust, &inner.limits)?;
        Ok(signed.clone())
    }

    fn select_at(
        &self,
        request: &FederationSelectionRequest,
        now: i64,
    ) -> Result<Vec<SelectedFederatedOrigin>, FederationError> {
        validate_selection_id(&request.model)?;
        validate_selection_id(&request.product)?;
        if request.minimum_response_bytes == 0 {
            return Err(FederationError::Invalid(
                "minimum response bytes must be non-zero".into(),
            ));
        }
        if let Some(bounds) = &request.bounds {
            validate_selection_bounds(bounds)?;
        }
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        let health = inner
            .health
            .lock()
            .map_err(|_| FederationError::Invalid("health lock is poisoned".into()))?;
        let mut selected = Vec::new();
        for signed in inner.descriptors.values() {
            if verify_signed_public_origin_descriptor(signed, now, &inner.trust, &inner.limits)
                .is_err()
            {
                continue;
            }
            let descriptor = &signed.descriptor;
            let record = health
                .get(&descriptor.origin_id)
                .cloned()
                .unwrap_or_default();
            if now < record.quarantine_until_unix
                || descriptor.quotas.maximum_response_bytes < request.minimum_response_bytes
                || request.require_replication && !descriptor.replication.accepts_replication
                || request.require_replication
                    && !descriptor.replication.models.contains(&request.model)
                || !descriptor.models.iter().any(|model| {
                    model.model == request.model
                        && model.products.iter().any(|product| {
                            product.product == request.product
                                && product.queries.contains(&request.query)
                        })
                })
                || request.bounds.as_ref().is_some_and(|bounds| {
                    !descriptor
                        .geographic_coverage
                        .iter()
                        .any(|area| coverage_contains(area, bounds))
                })
            {
                continue;
            }
            selected.push(SelectedFederatedOrigin {
                origin_id: descriptor.origin_id.clone(),
                https_base_url: descriptor.https_base_url.clone(),
                health_url: format!("{}{}", descriptor.https_base_url, descriptor.health_path),
                consecutive_failures: record.consecutive_failures,
            });
        }
        selected.sort_by(|a, b| {
            a.consecutive_failures
                .cmp(&b.consecutive_failures)
                .then_with(|| a.origin_id.cmp(&b.origin_id))
        });
        selected.truncate(inner.maximum_selection_results);
        Ok(selected)
    }

    fn record_health_at(
        &self,
        origin_id: &str,
        observation: FederationHealthObservation,
        now: i64,
    ) -> Result<(), FederationError> {
        validate_selection_id(origin_id)?;
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        if !inner.descriptors.contains_key(origin_id) {
            return Err(FederationError::NotFound);
        }
        let mut health = inner
            .health
            .lock()
            .map_err(|_| FederationError::Invalid("health lock is poisoned".into()))?;
        let record = health.entry(origin_id.into()).or_default();
        match observation {
            FederationHealthObservation::Healthy => *record = HealthRecord::default(),
            FederationHealthObservation::Failed => {
                record.consecutive_failures = record.consecutive_failures.saturating_add(1);
                if record.consecutive_failures >= inner.health_failure_threshold {
                    record.quarantine_until_unix =
                        now.saturating_add(inner.health_quarantine_seconds);
                }
            }
        }
        Ok(())
    }
}

fn coverage_contains(area: &FederationCoverageArea, requested: &FederationSelectionBounds) -> bool {
    area.west_longitude_e7 <= requested.west_longitude_e7
        && area.south_latitude_e7 <= requested.south_latitude_e7
        && area.east_longitude_e7 >= requested.east_longitude_e7
        && area.north_latitude_e7 >= requested.north_latitude_e7
}

fn validate_selection_bounds(value: &FederationSelectionBounds) -> Result<(), FederationError> {
    if !(-1_800_000_000..=1_800_000_000).contains(&value.west_longitude_e7)
        || !(-1_800_000_000..=1_800_000_000).contains(&value.east_longitude_e7)
        || !(-900_000_000..=900_000_000).contains(&value.south_latitude_e7)
        || !(-900_000_000..=900_000_000).contains(&value.north_latitude_e7)
        || value.west_longitude_e7 >= value.east_longitude_e7
        || value.south_latitude_e7 >= value.north_latitude_e7
    {
        return Err(FederationError::Invalid(
            "selection bounds are invalid".into(),
        ));
    }
    Ok(())
}

fn validate_selection_id(value: &str) -> Result<(), FederationError> {
    if value.is_empty()
        || value.len() > 128
        || value.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.'))
        })
        || value.starts_with(['-', '_', '.'])
        || value.ends_with(['-', '_', '.'])
    {
        return Err(FederationError::Invalid(
            "selection identifier is not canonical".into(),
        ));
    }
    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey, FederationError> {
    let secret = read_secret(path)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .map_err(|_| FederationError::Invalid("catalog signing key must be base64".into()))?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
        FederationError::Invalid("catalog signing key must contain 32 bytes".into())
    })?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn read_secret(path: &Path) -> Result<String, FederationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > MAX_SECRET_BYTES
    {
        return Err(FederationError::Invalid(
            "secret must be a bounded regular file".into(),
        ));
    }
    validate_secret_permissions(&metadata)?;
    let value = fs::read_to_string(path)?.trim().to_string();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(FederationError::Invalid(
            "secret file is empty or malformed".into(),
        ));
    }
    Ok(value)
}

fn read_bounded_regular_file(path: &Path, maximum: u64) -> Result<Vec<u8>, FederationError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > maximum
    {
        return Err(FederationError::Invalid(
            "descriptor must be a bounded regular file".into(),
        ));
    }
    Ok(fs::read(path)?)
}

#[cfg(unix)]
fn validate_secret_permissions(metadata: &fs::Metadata) -> Result<(), FederationError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(FederationError::Invalid(
            "federation secret file must not be accessible by group or other users".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secret_permissions(_metadata: &fs::Metadata) -> Result<(), FederationError> {
    // The Windows deployment doctor must restrict the ACL to the service
    // identity and SYSTEM; std does not expose a portable ACL evaluator.
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_community_protocol::{
        FEDERATION_ORIGIN_SCHEMA, FederationModelCapability, FederationPolicyLinks,
        FederationProductCapability, FederationPublicKey, FederationQuotaSummary,
        FederationReplicationPolicy, FederationRetentionSummary, PublicOriginDescriptor,
        SignatureAlgorithm, sign_public_origin_descriptor,
    };

    fn write_service(now: i64) -> (tempfile::TempDir, FederationService) {
        let directory = tempfile::tempdir().unwrap();
        let origin_key = SigningKey::from_bytes(&[7; 32]);
        let catalog_key = SigningKey::from_bytes(&[9; 32]);
        let encoded_origin_key =
            base64::engine::general_purpose::STANDARD.encode(origin_key.verifying_key().to_bytes());
        let public = FederationPublicKey {
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: "lab-2026-a".into(),
            public_key_base64: encoded_origin_key.clone(),
            not_before_unix: now - 60,
            expires_unix: now + 86_400,
        };
        let signed = sign_public_origin_descriptor(
            PublicOriginDescriptor {
                schema: FEDERATION_ORIGIN_SCHEMA.into(),
                origin_id: "university-weather-lab".into(),
                display_name: "University Weather Lab".into(),
                https_base_url: "https://weather.example.edu".into(),
                health_path: "/v1/health/ready".into(),
                descriptor_signing_keys: vec![public.clone()],
                object_signing_keys: vec![public],
                models: vec![FederationModelCapability {
                    model: "hrrr".into(),
                    products: vec![FederationProductCapability {
                        product: "native".into(),
                        queries: vec![FederationQueryCapability::ArbitraryDomainMap],
                        pressure_levels_hpa: vec![],
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
                build_version: "test".into(),
                issued_unix: now - 30,
                expires_unix: now + 3_600,
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
            },
            "lab-2026-a",
            &origin_key,
            &FederationLimits::default(),
        )
        .unwrap();
        let descriptor_path = directory.path().join("lab.json");
        fs::write(&descriptor_path, serde_json::to_vec(&signed).unwrap()).unwrap();
        let key_path = directory.path().join("catalog.key");
        fs::write(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode(catalog_key.to_bytes()),
        )
        .unwrap();
        let config = FederationConfig {
            enabled: true,
            catalog_signing_key_file: Some(key_path),
            descriptor_files: vec![descriptor_path],
            approved_origins: vec![crate::config::ApprovedFederationOriginConfig {
                origin_id: "university-weather-lab".into(),
                descriptor_signing_keys: vec![crate::config::FederationTrustedKeyConfig {
                    key_id: "lab-2026-a".into(),
                    public_key_base64: encoded_origin_key,
                }],
            }],
            ..FederationConfig::default()
        };
        (directory, FederationService::open(&config).unwrap())
    }

    #[test]
    fn selection_is_bounded_capability_aware_and_health_failover_safe() {
        let now = now_unix();
        let (_directory, service) = write_service(now);
        let request = FederationSelectionRequest {
            model: "hrrr".into(),
            product: "native".into(),
            query: FederationQueryCapability::ArbitraryDomainMap,
            bounds: Some(FederationSelectionBounds {
                west_longitude_e7: -1_050_000_000,
                south_latitude_e7: 300_000_000,
                east_longitude_e7: -900_000_000,
                north_latitude_e7: 450_000_000,
            }),
            minimum_response_bytes: 1024,
            require_replication: false,
        };
        assert_eq!(service.select_at(&request, now).unwrap().len(), 1);
        for _ in 0..3 {
            service
                .record_health_at(
                    "university-weather-lab",
                    FederationHealthObservation::Failed,
                    now,
                )
                .unwrap();
        }
        assert!(service.select_at(&request, now).unwrap().is_empty());
        assert_eq!(service.select_at(&request, now + 60).unwrap().len(), 1);
        service
            .record_health_at(
                "university-weather-lab",
                FederationHealthObservation::Healthy,
                now + 60,
            )
            .unwrap();
        assert_eq!(
            service.select_at(&request, now + 60).unwrap()[0].consecutive_failures,
            0
        );
    }

    #[test]
    fn startup_rejects_unapproved_or_mismatched_descriptor_identity() {
        let now = now_unix();
        let (directory, _) = write_service(now);
        let catalog_key = directory.path().join("catalog.key");
        let descriptor = directory.path().join("lab.json");
        let config = FederationConfig {
            enabled: true,
            catalog_signing_key_file: Some(catalog_key),
            descriptor_files: vec![descriptor],
            approved_origins: vec![crate::config::ApprovedFederationOriginConfig {
                origin_id: "unapproved-lab".into(),
                descriptor_signing_keys: vec![crate::config::FederationTrustedKeyConfig {
                    key_id: "lab-2026-a".into(),
                    public_key_base64: base64::engine::general_purpose::STANDARD
                        .encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes()),
                }],
            }],
            ..FederationConfig::default()
        };
        assert!(matches!(
            FederationService::open(&config),
            Err(FederationError::Protocol(
                ProtocolError::UntrustedFederationOrigin(_)
            ))
        ));
    }

    #[test]
    fn revoked_origin_is_excluded_without_preventing_service_startup() {
        let now = now_unix();
        let (directory, _) = write_service(now);
        let config = FederationConfig {
            enabled: true,
            catalog_signing_key_file: Some(directory.path().join("catalog.key")),
            descriptor_files: vec![directory.path().join("lab.json")],
            approved_origins: vec![crate::config::ApprovedFederationOriginConfig {
                origin_id: "university-weather-lab".into(),
                descriptor_signing_keys: vec![crate::config::FederationTrustedKeyConfig {
                    key_id: "lab-2026-a".into(),
                    public_key_base64: base64::engine::general_purpose::STANDARD
                        .encode(SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes()),
                }],
            }],
            revoked_origin_ids: vec!["university-weather-lab".into()],
            ..FederationConfig::default()
        };
        let service = FederationService::open(&config).unwrap();
        assert!(service.catalog_at(now).unwrap().catalog.origins.is_empty());
        assert!(matches!(
            service.descriptor_at("university-weather-lab", now),
            Err(FederationError::NotFound)
        ));
    }
}

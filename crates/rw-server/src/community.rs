//! Phase-one Community Cache application service.
//!
//! Delivery is local CAS -> hot immutable provider -> HTTPS origin. A cache
//! miss is ordinary and never depends on another user being online.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use chrono::{Datelike, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rw_community_protocol::{
    AttributionNotice, CASE_ARTIFACT_REVOCATION_SCHEMA, CASE_REVOCATION_SCHEMA, CASE_SCHEMA,
    CaseRoomManifest, Compression, DataOrigin, DeliverySource, NATIVE_WINDOW_PAYLOAD_SCHEMA,
    OBJECT_SCHEMA, ObjectManifest, POINT_SERIES_PAYLOAD_SCHEMA, PROFILE_PAYLOAD_SCHEMA,
    PUBLICATION_AUDIT_SCHEMA, PUBLICATION_TOMBSTONE_SCHEMA, ProfileObjectPayload, ProtocolError,
    ProtocolLimits, PublicationAuditRecord, PublicationTombstone, PublishCaseArtifactRequest,
    RESOLVE_SCHEMA, ResolveObjectRequest, ResolveObjectResponse, RevokePublicationRequest,
    ShareQuery, SignedCaseRoomManifest, SignedObjectManifest, SurfaceSample,
    TEMPORAL_GRID_PAYLOAD_SCHEMA, TrustedSigningKeys, TypedObjectPayload,
    case_artifact_payload_bytes, enforce_request_attributions, object_sha256, request_sha256,
    sign_case_manifest, sign_object_manifest, validate_case_artifact_payload_bytes,
    validate_profile_payload_identity, verify_signed_case, verify_signed_object,
};
use rw_query::{
    IndexWindow2DRequest, IndexWindow3DRequest, IntervalSupport, MissingPolicy, PointSeriesRequest,
    ProfileRequest, QueryError, StoreCatalog, TemporalGridRequest, TemporalReducer,
    TemporalSemantics, TemporalVerticalSelection, TemporalWindow, TimeExpectation, TimeRange,
    query_point_series, query_profile, query_window_2d, query_window_3d, reduce_temporal_grid,
};
use thiserror::Error;

use crate::community_store::{
    AccountingLimits, CasLimits, CaseLimits, CommunityCas, CommunityStoreError, QuotaLedger,
};
use crate::config::{CommunityConfig, HotStoreConfig};

const ORIGIN_SIGNING_KEY_ID: &str = "rw-origin-v1";
const MAX_SECRET_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum CommunityError {
    #[error("Community Cache is disabled")]
    Disabled,
    #[error("Community Cache is disabled by the server kill switch")]
    Killed,
    #[error("community object was not found")]
    NotFound,
    #[error("community object type is not implemented in phase one")]
    Unsupported,
    #[error("community upstream failed: {0}")]
    Upstream(String),
    #[error("invalid Community Cache configuration or state: {0}")]
    Invalid(String),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Cas(#[from] CommunityStoreError),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub trait ImmutableObjectProvider: Send + Sync {
    fn get(&self, key: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>, CommunityError>;
    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CommunityError>;
}

pub trait OriginResolver: Send + Sync {
    fn resolve(
        &self,
        request: &ResolveObjectRequest,
        limits: &ProtocolLimits,
    ) -> Result<Option<(SignedObjectManifest, Vec<u8>)>, CommunityError>;
}

#[derive(Debug, Clone)]
pub struct FilesystemObjectProvider {
    root: Arc<PathBuf>,
}

impl FilesystemObjectProvider {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CommunityError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        let metadata = fs::symlink_metadata(&root)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CommunityError::Invalid(
                "hot-object filesystem root must be a real directory".into(),
            ));
        }
        Ok(Self {
            root: Arc::new(root),
        })
    }

    fn path(&self, key: &str) -> Result<PathBuf, CommunityError> {
        if key.is_empty()
            || key.starts_with('/')
            || key.contains(['\\', '\0'])
            || key
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
        {
            return Err(CommunityError::Invalid(
                "invalid immutable object key".into(),
            ));
        }
        Ok(key
            .split('/')
            .fold((*self.root).clone(), |path, part| path.join(part)))
    }
}

impl ImmutableObjectProvider for FilesystemObjectProvider {
    fn get(&self, key: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>, CommunityError> {
        let path = self.path(key)?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.len() > maximum_bytes
        {
            return Err(CommunityError::Invalid(
                "hot object is not a bounded regular file".into(),
            ));
        }
        Ok(Some(fs::read(path)?))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CommunityError> {
        let path = self.path(key)?;
        let parent = path
            .parent()
            .ok_or_else(|| CommunityError::Invalid("immutable key has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let metadata = fs::symlink_metadata(parent)?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(CommunityError::Invalid(
                "hot-object key parent must be a real directory".into(),
            ));
        }
        if path.exists() {
            if fs::read(&path)? != bytes {
                return Err(CommunityError::Invalid(
                    "immutable hot-object key collision".into(),
                ));
            }
        } else {
            rw_store::atomic::atomic_write_bytes(&path, bytes)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct R2GatewayProvider {
    base_url: String,
    bucket: String,
    bearer_token: Arc<String>,
    agent: ureq::Agent,
}

impl R2GatewayProvider {
    pub fn new(
        base_url: String,
        bucket: String,
        token_file: &Path,
    ) -> Result<Self, CommunityError> {
        let bearer_token = read_secret(token_file)?;
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_per_call(Some(Duration::from_secs(20)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::WebPki)
                    .build(),
            )
            .build();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').into(),
            bucket,
            bearer_token: Arc::new(bearer_token),
            agent: config.into(),
        })
    }

    fn url(&self, key: &str) -> Result<String, CommunityError> {
        validate_remote_key(key)?;
        Ok(format!("{}/{}/{}", self.base_url, self.bucket, key))
    }
}

impl ImmutableObjectProvider for R2GatewayProvider {
    fn get(&self, key: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>, CommunityError> {
        let response = match self
            .agent
            .get(&self.url(key)?)
            .header("Authorization", &format!("Bearer {}", self.bearer_token))
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(CommunityError::Upstream(error.to_string())),
        };
        let mut body = response.into_body().into_reader();
        let mut limited = (&mut body).take(maximum_bytes.saturating_add(1));
        let mut bytes = Vec::new();
        limited.read_to_end(&mut bytes)?;
        if bytes.len() as u64 > maximum_bytes {
            return Err(CommunityError::Invalid(
                "hot object exceeds size limit".into(),
            ));
        }
        Ok(Some(bytes))
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CommunityError> {
        match self
            .agent
            .put(&self.url(key)?)
            .header("Authorization", &format!("Bearer {}", self.bearer_token))
            .header("Content-Type", "application/octet-stream")
            .header("If-None-Match", "*")
            .send(bytes)
        {
            Ok(_) | Err(ureq::Error::StatusCode(412)) => Ok(()),
            Err(error) => Err(CommunityError::Upstream(error.to_string())),
        }
    }
}

#[derive(Clone)]
pub struct CommunityService {
    enabled: bool,
    killed: Arc<AtomicBool>,
    cas: Option<CommunityCas>,
    signing_key: Option<Arc<SigningKey>>,
    trusted_keys: Arc<TrustedSigningKeys>,
    limits: ProtocolLimits,
    hot: Option<Arc<dyn ImmutableObjectProvider>>,
    origin: Option<Arc<dyn OriginResolver>>,
    promotion_enabled: bool,
    promotion_hits: u64,
    promotion_window_seconds: u64,
    promotion_maximum_bytes: u64,
    hits: Arc<Mutex<BTreeMap<String, (i64, u64)>>>,
    cases_enabled: bool,
    artifact_publication_enabled: bool,
    maximum_case_retention_seconds: u64,
    quota: QuotaLedger,
}

impl std::fmt::Debug for CommunityService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CommunityService")
            .field("enabled", &self.enabled)
            .field("killed", &self.killed.load(Ordering::Acquire))
            .field("hot_configured", &self.hot.is_some())
            .field("origin_configured", &self.origin.is_some())
            .finish_non_exhaustive()
    }
}

impl CommunityService {
    pub fn open(config: &CommunityConfig) -> Result<Self, CommunityError> {
        let quotas = &config.quotas;
        let cas = config
            .enabled
            .then(|| {
                CommunityCas::open(
                    &config.root,
                    CasLimits {
                        maximum_object_bytes: quotas.maximum_object_bytes,
                        maximum_manifest_bytes: quotas.maximum_manifest_bytes,
                        storage_bytes: quotas.storage_bytes,
                        maximum_objects: quotas.maximum_objects,
                    },
                    CaseLimits {
                        maximum_manifest_bytes: config.cases.maximum_manifest_bytes,
                        storage_bytes: config.cases.storage_bytes,
                        maximum_cases: config.cases.maximum_cases,
                    },
                )
            })
            .transpose()?;
        let signing_key = if config.enabled {
            config
                .signing_key_file
                .as_deref()
                .map(load_signing_key)
                .transpose()?
                .map(Arc::new)
        } else {
            None
        };
        let mut trusted_keys = parse_trusted_keys(&config.trusted_public_keys)?;
        if let Some(key) = &signing_key {
            trusted_keys.insert(ORIGIN_SIGNING_KEY_ID.into(), key.verifying_key());
        }
        let hot: Option<Arc<dyn ImmutableObjectProvider>> = if !config.enabled {
            None
        } else {
            match &config.hot_store {
                HotStoreConfig::Disabled => None,
                HotStoreConfig::Filesystem { root } => {
                    Some(Arc::new(FilesystemObjectProvider::open(root)?))
                }
                HotStoreConfig::R2 {
                    base_url,
                    bucket,
                    token_file,
                } => Some(Arc::new(R2GatewayProvider::new(
                    base_url.clone(),
                    bucket.clone(),
                    token_file,
                )?)),
            }
        };
        let origin = if !config.enabled {
            None
        } else {
            match (&config.origin_base_url, &config.origin_token_file) {
                (Some(base_url), token_file) => Some(Arc::new(HttpsOriginClient::new(
                    base_url.clone(),
                    token_file.as_deref(),
                )?)
                    as Arc<dyn OriginResolver>),
                (None, _) => None,
            }
        };
        let quota = if config.enabled {
            QuotaLedger::open(
                config.root.join("accounting.json"),
                AccountingLimits {
                    upload_bytes_per_month: quotas.upload_bytes_per_month,
                    download_bytes_per_month: quotas.download_bytes_per_month,
                    promoted_bytes_per_month: quotas.promoted_bytes_per_month,
                    concurrent_transfers: quotas.concurrent_transfers,
                    maximum_principals: quotas.maximum_principals,
                },
                current_month(),
            )?
        } else {
            QuotaLedger::memory(
                AccountingLimits {
                    upload_bytes_per_month: quotas.upload_bytes_per_month,
                    download_bytes_per_month: quotas.download_bytes_per_month,
                    promoted_bytes_per_month: quotas.promoted_bytes_per_month,
                    concurrent_transfers: quotas.concurrent_transfers,
                    maximum_principals: quotas.maximum_principals,
                },
                current_month(),
            )?
        };
        Ok(Self {
            enabled: config.enabled,
            killed: Arc::new(AtomicBool::new(config.kill_switch)),
            cas,
            signing_key,
            trusted_keys: Arc::new(trusted_keys),
            limits: ProtocolLimits {
                max_manifest_bytes: quotas.maximum_manifest_bytes,
                max_encoded_bytes: quotas.maximum_object_bytes,
                max_decoded_bytes: quotas.maximum_decompressed_bytes,
                max_case_artifacts: config.cases.maximum_objects_per_case,
                ..ProtocolLimits::default()
            },
            hot,
            origin,
            promotion_enabled: config.promotion.enabled,
            promotion_hits: config.promotion.minimum_hits,
            promotion_window_seconds: config.promotion.window_seconds,
            promotion_maximum_bytes: config.promotion.maximum_object_bytes,
            hits: Arc::new(Mutex::new(BTreeMap::new())),
            cases_enabled: config.cases.enabled,
            artifact_publication_enabled: config.cases.artifact_publication_enabled,
            maximum_case_retention_seconds: config.cases.default_retention_seconds,
            quota,
        })
    }

    #[cfg(test)]
    pub fn with_providers(
        config: &CommunityConfig,
        hot: Option<Arc<dyn ImmutableObjectProvider>>,
        origin: Option<Arc<dyn OriginResolver>>,
    ) -> Result<Self, CommunityError> {
        let mut service = Self::open(config)?;
        service.hot = hot;
        service.origin = origin;
        Ok(service)
    }

    pub fn set_kill_switch(&self, killed: bool) {
        self.killed.store(killed, Ordering::Release);
    }

    fn ensure_available(&self) -> Result<(), CommunityError> {
        if !self.enabled {
            return Err(CommunityError::Disabled);
        }
        Ok(())
    }

    fn ensure_assist_available(&self) -> Result<(), CommunityError> {
        self.ensure_available()?;
        if self.killed.load(Ordering::Acquire) {
            return Err(CommunityError::Killed);
        }
        Ok(())
    }

    fn cas(&self) -> Result<&CommunityCas, CommunityError> {
        self.cas.as_ref().ok_or(CommunityError::Disabled)
    }

    pub fn resolve(
        &self,
        principal: &str,
        request: &ResolveObjectRequest,
        catalog: &StoreCatalog,
    ) -> Result<(ResolveObjectResponse, Option<Bytes>), CommunityError> {
        self.ensure_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if request.schema != RESOLVE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(request.schema.clone()).into());
        }
        request.request.validate(&self.limits)?;
        let identity = request_sha256(&request.request)?;
        if matches!(request.request.query, ShareQuery::CaseArtifact { .. })
            && self.cas()?.publication_request_tombstoned(&identity)?
        {
            return Err(CommunityError::NotFound);
        }

        // The kill switch disables community-assisted delivery and all hot
        // promotion, but never strands a normal signed HTTPS-origin request.
        if self.killed.load(Ordering::Acquire) {
            if let Some(origin) = &self.origin {
                match origin.resolve(request, &self.limits) {
                    Ok(Some((signed, object))) => {
                        verify_signed_object(
                            &signed,
                            &request.request,
                            &object,
                            now_unix(),
                            &self.trusted_keys,
                            &self.limits,
                        )?;
                        enforce_request_attributions(&request.request, &signed.manifest)?;
                        let signed_bytes = serde_json::to_vec(&signed)?;
                        self.cas()?.put(
                            &identity,
                            &signed.manifest.object_sha256,
                            &object,
                            &signed_bytes,
                        )?;
                        return Ok((
                            origin_only_response(identity, signed),
                            Some(Bytes::from(object)),
                        ));
                    }
                    Ok(None)
                    | Err(CommunityError::Upstream(_))
                    | Err(CommunityError::Protocol(_))
                    | Err(CommunityError::Json(_))
                    | Err(CommunityError::Invalid(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            let (signed, object) = self.compute_origin_object(&request.request, catalog)?;
            let signed_bytes = serde_json::to_vec(&signed)?;
            self.cas()?.put(
                &identity,
                &signed.manifest.object_sha256,
                &object,
                &signed_bytes,
            )?;
            return Ok((
                origin_only_response(identity, signed),
                Some(Bytes::from(object)),
            ));
        }

        match self.cas()?.get(&identity) {
            Ok(Some((manifest_bytes, object))) => {
                let verified = serde_json::from_slice::<SignedObjectManifest>(&manifest_bytes)
                    .map_err(CommunityError::from)
                    .and_then(|signed| {
                        verify_signed_object(
                            &signed,
                            &request.request,
                            &object,
                            now_unix(),
                            &self.trusted_keys,
                            &self.limits,
                        )?;
                        Ok(signed)
                    });
                match verified {
                    Ok(signed) => {
                        return Ok((
                            resolved_response(identity, signed),
                            Some(Bytes::from(object)),
                        ));
                    }
                    Err(_) => self.cas()?.invalidate_request(&identity)?,
                }
            }
            Ok(None) => {}
            Err(CommunityStoreError::HashMismatch)
            | Err(CommunityStoreError::Invalid(_))
            | Err(CommunityStoreError::Json(_)) => {
                self.cas()?.invalidate_request(&identity)?;
            }
            Err(error) => return Err(error.into()),
        }

        if let Some(provider) = &self.hot {
            match self.fetch_provider(provider.as_ref(), &request.request, &identity) {
                Ok(Some((signed, object))) => {
                    let signed_bytes = serde_json::to_vec(&signed)?;
                    self.cas()?.put(
                        &identity,
                        &signed.manifest.object_sha256,
                        &object,
                        &signed_bytes,
                    )?;
                    return Ok((
                        resolved_response(identity, signed),
                        Some(Bytes::from(object)),
                    ));
                }
                Ok(None)
                | Err(CommunityError::Upstream(_))
                | Err(CommunityError::Protocol(_))
                | Err(CommunityError::Json(_))
                | Err(CommunityError::Invalid(_)) => {}
                Err(error) => return Err(error),
            }
        }
        if let Some(origin) = &self.origin {
            match origin.resolve(request, &self.limits) {
                Ok(Some((signed, object))) => {
                    verify_signed_object(
                        &signed,
                        &request.request,
                        &object,
                        now_unix(),
                        &self.trusted_keys,
                        &self.limits,
                    )?;
                    enforce_request_attributions(&request.request, &signed.manifest)?;
                    let signed_bytes = serde_json::to_vec(&signed)?;
                    self.cas()?.put(
                        &identity,
                        &signed.manifest.object_sha256,
                        &object,
                        &signed_bytes,
                    )?;
                    // Promotion is a best-effort cost optimization. R2 failure
                    // cannot invalidate a verified HTTPS-origin response.
                    let _ = self.note_popularity_and_promote(&identity, &signed, &object);
                    return Ok((
                        resolved_response(identity, signed),
                        Some(Bytes::from(object)),
                    ));
                }
                Ok(None)
                | Err(CommunityError::Upstream(_))
                | Err(CommunityError::Protocol(_))
                | Err(CommunityError::Json(_))
                | Err(CommunityError::Invalid(_)) => {}
                Err(error) => return Err(error),
            }
        }

        let (signed, object) = self.compute_origin_object(&request.request, catalog)?;
        let signed_bytes = serde_json::to_vec(&signed)?;
        self.cas()?.put(
            &identity,
            &signed.manifest.object_sha256,
            &object,
            &signed_bytes,
        )?;
        // Promotion is a best-effort cost optimization. R2 failure cannot
        // invalidate a successful authoritative-origin computation.
        let _ = self.note_popularity_and_promote(&identity, &signed, &object);
        Ok((
            resolved_response(identity, signed),
            Some(Bytes::from(object)),
        ))
    }

    pub fn object(&self, principal: &str, sha256: &str) -> Result<Bytes, CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if self.cas()?.publication_tombstone(sha256)?.is_some() {
            return Err(CommunityError::NotFound);
        }
        let expired_audit = self
            .cas()?
            .publication_audit(sha256)?
            .filter(|audit| audit.retain_until_unix <= now_unix());
        if let Some(audit) = expired_audit {
            self.cas()?.invalidate_request(&audit.request_sha256)?;
            return Err(CommunityError::NotFound);
        }
        let (manifest_bytes, object) = self
            .cas()?
            .get_object_reference(sha256)?
            .ok_or(CommunityError::NotFound)?;
        let signed: SignedObjectManifest = serde_json::from_slice(&manifest_bytes)?;
        verify_signed_object(
            &signed,
            &signed.manifest.request,
            &object,
            now_unix(),
            &self.trusted_keys,
            &self.limits,
        )?;
        if signed.manifest.object_sha256 != sha256 {
            return Err(CommunityError::Invalid(
                "signed manifest references a different object".into(),
            ));
        }
        if matches!(
            signed.manifest.request.query,
            ShareQuery::CaseArtifact { .. }
        ) {
            validate_case_artifact_payload_bytes(&object, &signed.manifest.request, &self.limits)?;
        }
        let object = Bytes::from(object);
        self.quota.charge_download(principal, object.len() as u64)?;
        Ok(object)
    }

    /// Deliberately publish one strictly typed case artifact. The authenticated
    /// bearer principal is embedded in the canonical request recipe and may
    /// never be supplied on behalf of another owner.
    pub fn publish_case_artifact(
        &self,
        principal: &str,
        publication: PublishCaseArtifactRequest,
    ) -> Result<SignedObjectManifest, CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if !self.cases_enabled || !self.artifact_publication_enabled {
            return Err(CommunityError::Disabled);
        }
        if publication.owner_principal_sha256 != principal {
            return Err(CommunityError::Invalid(
                "authenticated principal cannot publish for another owner".into(),
            ));
        }
        publication.validate(&self.limits)?;
        let private_origin_mismatch = private_origin_for_model(&publication.request.model)
            .is_some_and(|required| publication.request.publication.data_origin != required);
        if private_origin_mismatch {
            return Err(CommunityError::Invalid(
                "private WRF/ArWen artifact cannot be relabeled as public-provider data".into(),
            ));
        }
        let retention_seconds = publication
            .retain_until_unix
            .checked_sub(publication.published_unix)
            .ok_or_else(|| CommunityError::Invalid("artifact retention overflowed".into()))?;
        if retention_seconds <= 0
            || u64::try_from(retention_seconds).unwrap_or(u64::MAX)
                > self.maximum_case_retention_seconds
        {
            return Err(CommunityError::Invalid(
                "artifact retention exceeds the configured maximum".into(),
            ));
        }
        let now = now_unix();
        if publication.published_unix > now.saturating_add(300)
            || publication.retain_until_unix <= now
        {
            return Err(CommunityError::Invalid(
                "artifact publication is expired or too far in the future".into(),
            ));
        }
        let bytes = case_artifact_payload_bytes(&publication)?;
        if bytes.len() as u64 > self.limits.max_encoded_bytes {
            return Err(ProtocolError::EncodedSizeLimit.into());
        }
        validate_case_artifact_payload_bytes(&bytes, &publication.request, &self.limits)?;
        let ShareQuery::CaseArtifact {
            case_id,
            artifact_id,
            artifact_type,
        } = &publication.request.query
        else {
            return Err(CommunityError::Invalid(
                "artifact endpoint requires a case_artifact request".into(),
            ));
        };
        let key = self
            .signing_key
            .as_ref()
            .ok_or_else(|| CommunityError::Invalid("origin signing key is unavailable".into()))?;
        let request_hash = request_sha256(&publication.request)?;
        let object_hash = object_sha256(&bytes);
        if self.cas()?.publication_tombstone(&object_hash)?.is_some() {
            return Err(CommunityError::Invalid(
                "a revoked content identity cannot be republished".into(),
            ));
        }
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request: publication.request.clone(),
            request_sha256: request_hash.clone(),
            object_sha256: object_hash.clone(),
            content_type: "application/json".into(),
            compression: Compression::None,
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            attributions: publication.attributions.clone(),
            modification_notices: publication.modification_notices.clone(),
            created_unix: publication.published_unix,
            expires_unix: publication.retain_until_unix,
        };
        enforce_request_attributions(&publication.request, &manifest)?;
        let signed = sign_object_manifest(manifest, ORIGIN_SIGNING_KEY_ID, key)?;
        verify_signed_object(
            &signed,
            &publication.request,
            &bytes,
            now,
            &self.trusted_keys,
            &self.limits,
        )?;
        let signed_bytes = serde_json::to_vec(&signed)?;
        let charged_bytes = (bytes.len() as u64).saturating_add(signed_bytes.len() as u64);
        self.quota.charge_upload(principal, charged_bytes)?;
        self.cas()?.put_publication_audit(&PublicationAuditRecord {
            schema: PUBLICATION_AUDIT_SCHEMA.into(),
            owner_principal_sha256: principal.into(),
            request_sha256: request_hash.clone(),
            object_sha256: object_hash.clone(),
            case_id: case_id.clone(),
            artifact_id: artifact_id.clone(),
            artifact_type: *artifact_type,
            data_origin: publication.request.publication.data_origin,
            published_unix: publication.published_unix,
            retain_until_unix: publication.retain_until_unix,
            source_snapshot_id: publication.request.snapshot_id.clone(),
            source_grid_hash: publication.request.grid_hash.clone(),
        })?;
        self.cas()?
            .put(&request_hash, &object_hash, &bytes, &signed_bytes)?;
        Ok(signed)
    }

    pub fn revoke_case_artifact(
        &self,
        principal: &str,
        object_sha256: &str,
        request: RevokePublicationRequest,
    ) -> Result<PublicationTombstone, CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if !self.cases_enabled || !self.artifact_publication_enabled {
            return Err(CommunityError::Disabled);
        }
        request.validate(CASE_ARTIFACT_REVOCATION_SCHEMA)?;
        let audit = self
            .cas()?
            .publication_audit(object_sha256)?
            .ok_or(CommunityError::NotFound)?;
        if audit.owner_principal_sha256 != principal {
            return Err(CommunityError::Invalid(
                "authenticated principal cannot revoke another owner's artifact".into(),
            ));
        }
        let tombstone = PublicationTombstone {
            schema: PUBLICATION_TOMBSTONE_SCHEMA.into(),
            owner_principal_sha256: principal.into(),
            request_sha256: audit.request_sha256.clone(),
            object_sha256: object_sha256.into(),
            revoked_unix: now_unix(),
            rights_withdrawn: request.rights_withdrawn,
            reason: request.reason,
        };
        let bytes = serde_json::to_vec(&tombstone)?;
        self.quota.charge_upload(principal, bytes.len() as u64)?;
        self.cas()?.revoke_publication(&audit, &tombstone)?;
        Ok(tombstone)
    }

    pub fn revoke_case(
        &self,
        principal: &str,
        case_id: &str,
        request: RevokePublicationRequest,
    ) -> Result<(), CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if !self.cases_enabled {
            return Err(CommunityError::Disabled);
        }
        request.validate(CASE_REVOCATION_SCHEMA)?;
        let bytes = self
            .cas()?
            .get_case(case_id)?
            .ok_or(CommunityError::NotFound)?;
        let signed: SignedCaseRoomManifest = serde_json::from_slice(&bytes)?;
        for artifact in &signed.manifest.artifacts {
            let audit = self
                .cas()?
                .publication_audit(&artifact.object_sha256)?
                .ok_or(CommunityError::NotFound)?;
            if audit.owner_principal_sha256 != principal {
                return Err(CommunityError::Invalid(
                    "authenticated principal does not own every case artifact".into(),
                ));
            }
        }
        #[derive(serde::Serialize)]
        struct CaseTombstone<'a> {
            schema: &'static str,
            case_id: &'a str,
            owner_principal_sha256: &'a str,
            revoked_unix: i64,
            rights_withdrawn: bool,
            reason: &'a str,
        }
        let tombstone = serde_json::to_vec(&CaseTombstone {
            schema: "rw.community.case-tombstone.v1",
            case_id,
            owner_principal_sha256: principal,
            revoked_unix: now_unix(),
            rights_withdrawn: request.rights_withdrawn,
            reason: &request.reason,
        })?;
        self.quota
            .charge_upload(principal, tombstone.len() as u64)?;
        self.cas()?.revoke_case(case_id, &tombstone)?;
        Ok(())
    }

    pub fn publish_case(
        &self,
        principal: &str,
        manifest: CaseRoomManifest,
    ) -> Result<SignedCaseRoomManifest, CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        if !self.cases_enabled {
            return Err(CommunityError::Disabled);
        }
        if manifest.schema != CASE_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(manifest.schema.clone()).into());
        }
        let retention_seconds = manifest
            .retain_until_unix
            .checked_sub(manifest.published_unix)
            .ok_or_else(|| CommunityError::Invalid("case retention interval overflowed".into()))?;
        if retention_seconds <= 0
            || u64::try_from(retention_seconds).unwrap_or(u64::MAX)
                > self.maximum_case_retention_seconds
        {
            return Err(CommunityError::Invalid(
                "case retention exceeds the configured maximum".into(),
            ));
        }
        for artifact in &manifest.artifacts {
            let audit = self
                .cas()?
                .publication_audit(&artifact.object_sha256)?
                .ok_or(CommunityError::NotFound)?;
            if audit.owner_principal_sha256 != principal
                || audit.case_id != manifest.case_id
                || audit.artifact_id != artifact.artifact_id
                || audit.artifact_type != artifact.artifact_type
                || audit.request_sha256 != artifact.request_sha256
                || audit.retain_until_unix < manifest.retain_until_unix
            {
                return Err(CommunityError::Invalid(
                    "case artifact is not an exact live publication owned by this principal".into(),
                ));
            }
            if self
                .cas()?
                .publication_tombstone(&artifact.object_sha256)?
                .is_some()
                || audit.retain_until_unix <= now_unix()
            {
                return Err(CommunityError::NotFound);
            }
            let (signed_bytes, object) = self
                .cas()?
                .get(&artifact.request_sha256)?
                .ok_or(CommunityError::NotFound)?;
            let signed: SignedObjectManifest = serde_json::from_slice(&signed_bytes)?;
            verify_signed_object(
                &signed,
                &signed.manifest.request,
                &object,
                now_unix(),
                &self.trusted_keys,
                &self.limits,
            )?;
            if signed.manifest.object_sha256 != artifact.object_sha256 {
                return Err(CommunityError::Invalid(
                    "case artifact request and object identities do not match".into(),
                ));
            }
            let ShareQuery::CaseArtifact {
                case_id,
                artifact_id,
                artifact_type,
            } = &signed.manifest.request.query
            else {
                return Err(CommunityError::Invalid(
                    "case reference points to a non-case object".into(),
                ));
            };
            if case_id != &manifest.case_id
                || artifact_id != &artifact.artifact_id
                || artifact_type != &artifact.artifact_type
                || signed.manifest.request_sha256 != artifact.request_sha256
            {
                return Err(CommunityError::Invalid(
                    "case reference does not match the signed artifact identity".into(),
                ));
            }
            validate_case_artifact_payload_bytes(&object, &signed.manifest.request, &self.limits)?;
            if signed.manifest.request.publication.data_origin != DataOrigin::PublicProvider
                && manifest.publication.data_origin == DataOrigin::PublicProvider
            {
                return Err(CommunityError::Invalid(
                    "a case containing owner-provided data cannot be labeled public-provider"
                        .into(),
                ));
            }
            if signed
                .manifest
                .attributions
                .iter()
                .any(|notice| !manifest.attributions.contains(notice))
                || signed
                    .manifest
                    .modification_notices
                    .iter()
                    .any(|notice| !manifest.modification_notices.contains(notice))
            {
                return Err(CommunityError::Invalid(
                    "case manifest did not propagate every artifact attribution and modification notice"
                        .into(),
                ));
            }
            let source_matches = manifest.sources.iter().any(|source| {
                source.model == signed.manifest.request.model
                    && source.run == signed.manifest.request.run
                    && source.snapshot_id == signed.manifest.request.snapshot_id
                    && source.grid_hash == signed.manifest.request.grid_hash
                    && source.source_provenance == signed.manifest.request.source_provenance
            });
            if !source_matches {
                return Err(CommunityError::Invalid(
                    "case sources do not contain the exact signed artifact source".into(),
                ));
            }
        }
        let key = self
            .signing_key
            .as_ref()
            .ok_or_else(|| CommunityError::Invalid("origin signing key is unavailable".into()))?;
        let signed = sign_case_manifest(manifest, ORIGIN_SIGNING_KEY_ID, key)?;
        verify_signed_case(&signed, now_unix(), &self.trusted_keys, &self.limits)?;
        let bytes = serde_json::to_vec(&signed)?;
        self.quota.charge_upload(principal, bytes.len() as u64)?;
        self.cas()?.put_case(&signed.manifest.case_id, &bytes)?;
        Ok(signed)
    }

    pub fn case(
        &self,
        principal: &str,
        case_id: &str,
    ) -> Result<SignedCaseRoomManifest, CommunityError> {
        self.ensure_assist_available()?;
        let _permit = self.quota.begin(principal, current_month())?;
        let bytes = self
            .cas()?
            .get_case(case_id)?
            .ok_or(CommunityError::NotFound)?;
        let signed: SignedCaseRoomManifest = serde_json::from_slice(&bytes)?;
        verify_signed_case(&signed, now_unix(), &self.trusted_keys, &self.limits)?;
        self.validate_live_case_artifacts(&signed)?;
        self.quota.charge_download(principal, bytes.len() as u64)?;
        Ok(signed)
    }

    fn validate_live_case_artifacts(
        &self,
        signed: &SignedCaseRoomManifest,
    ) -> Result<(), CommunityError> {
        for artifact in &signed.manifest.artifacts {
            if self
                .cas()?
                .publication_tombstone(&artifact.object_sha256)?
                .is_some()
            {
                return Err(CommunityError::NotFound);
            }
            let audit = self
                .cas()?
                .publication_audit(&artifact.object_sha256)?
                .ok_or(CommunityError::NotFound)?;
            if audit.request_sha256 != artifact.request_sha256
                || audit.case_id != signed.manifest.case_id
                || audit.artifact_id != artifact.artifact_id
                || audit.artifact_type != artifact.artifact_type
                || audit.retain_until_unix <= now_unix()
            {
                return Err(CommunityError::NotFound);
            }
            let (manifest_bytes, object) = self
                .cas()?
                .get(&artifact.request_sha256)?
                .ok_or(CommunityError::NotFound)?;
            let object_manifest: SignedObjectManifest = serde_json::from_slice(&manifest_bytes)?;
            verify_signed_object(
                &object_manifest,
                &object_manifest.manifest.request,
                &object,
                now_unix(),
                &self.trusted_keys,
                &self.limits,
            )?;
            if object_manifest.manifest.object_sha256 != artifact.object_sha256 {
                return Err(CommunityError::NotFound);
            }
            validate_case_artifact_payload_bytes(
                &object,
                &object_manifest.manifest.request,
                &self.limits,
            )?;
        }
        Ok(())
    }

    fn fetch_provider(
        &self,
        provider: &dyn ImmutableObjectProvider,
        request: &rw_community_protocol::ShareRequest,
        request_hash: &str,
    ) -> Result<Option<(SignedObjectManifest, Vec<u8>)>, CommunityError> {
        let manifest_key = format!("v1/manifests/{request_hash}.json");
        let Some(manifest_bytes) = provider.get(&manifest_key, self.limits.max_manifest_bytes)?
        else {
            return Ok(None);
        };
        let signed: SignedObjectManifest = serde_json::from_slice(&manifest_bytes)?;
        let object_key = format!("v1/objects/{}", signed.manifest.object_sha256);
        let Some(object) = provider.get(&object_key, self.limits.max_encoded_bytes)? else {
            return Ok(None);
        };
        verify_signed_object(
            &signed,
            request,
            &object,
            now_unix(),
            &self.trusted_keys,
            &self.limits,
        )?;
        enforce_request_attributions(request, &signed.manifest)?;
        Ok(Some((signed, object)))
    }

    fn compute_origin_object(
        &self,
        request: &rw_community_protocol::ShareRequest,
        catalog: &StoreCatalog,
    ) -> Result<(SignedObjectManifest, Vec<u8>), CommunityError> {
        let key = self
            .signing_key
            .as_ref()
            .ok_or_else(|| CommunityError::Invalid("origin signing key is unavailable".into()))?;
        let snapshot = catalog.snapshot(&request.model, &request.run)?;
        let descriptor = snapshot.descriptor();
        if descriptor.snapshot_id != request.snapshot_id
            || descriptor.grid_hash != request.grid_hash
        {
            return Err(CommunityError::Invalid(
                "request snapshot or grid does not match the origin store".into(),
            ));
        }
        validate_request_source(request, descriptor)?;
        let bytes = match &request.query {
            ShareQuery::Profile {
                latitude_e7,
                longitude_e7,
                storage_slot,
                valid_unix,
                pressure_variables,
                surface_variables,
                pressure_levels_hpa,
            } => {
                let latitude = f64::from(*latitude_e7) / 10_000_000.0;
                let longitude = f64::from(*longitude_e7) / 10_000_000.0;
                let mut profile = query_profile(
                    &snapshot,
                    &ProfileRequest {
                        latitude,
                        longitude,
                        storage_slot: *storage_slot,
                        variables: pressure_variables.clone(),
                    },
                )?;
                if profile.time.valid_unix != *valid_unix {
                    return Err(CommunityError::Invalid(
                        "profile storage slot does not match the request valid time".into(),
                    ));
                }
                if !pressure_levels_hpa.is_empty() {
                    for variable in &mut profile.variables {
                        let selected = variable
                            .levels_hpa
                            .iter()
                            .copied()
                            .zip(variable.values.iter().copied())
                            .filter(|(level, _)| pressure_levels_hpa.binary_search(level).is_ok())
                            .collect::<Vec<_>>();
                        if selected.len() != pressure_levels_hpa.len() {
                            return Err(CommunityError::Invalid(format!(
                                "profile variable '{}' does not contain every signed pressure level",
                                variable.name
                            )));
                        }
                        variable.levels_hpa = selected.iter().map(|(level, _)| *level).collect();
                        variable.values = selected.into_iter().map(|(_, value)| value).collect();
                        variable.expected_levels = variable.values.len();
                        variable.available_levels = variable.values.iter().flatten().count();
                        variable.coverage = if variable.expected_levels == 0 {
                            0.0
                        } else {
                            variable.available_levels as f64 / variable.expected_levels as f64
                        };
                    }
                }
                let surface = query_point_series(
                    &snapshot,
                    &PointSeriesRequest {
                        latitude,
                        longitude,
                        variables: surface_variables.clone(),
                        time: TimeRange {
                            start_unix: Some(profile.time.valid_unix),
                            end_unix: Some(profile.time.valid_unix.saturating_add(1)),
                        },
                        missing_policy: MissingPolicy::Partial,
                    },
                )?;
                if surface.axis.len() != 1
                    || surface.axis[0].storage_slot != profile.time.storage_slot
                    || surface.axis[0].valid_unix != profile.time.valid_unix
                {
                    return Err(CommunityError::Invalid(
                        "profile surface bundle did not resolve to the exact profile time".into(),
                    ));
                }
                let surface_samples = surface
                    .variables
                    .iter()
                    .map(|variable| SurfaceSample {
                        variable: variable.name.clone(),
                        units: variable.units.clone(),
                        value: variable.values.first().copied().flatten(),
                    })
                    .collect::<Vec<_>>();
                let payload = ProfileObjectPayload {
                    schema: PROFILE_PAYLOAD_SCHEMA.into(),
                    request_sha256: request_sha256(request)?,
                    profile,
                    surface_samples,
                };
                validate_profile_payload_identity(&payload, request)?;
                serde_json::to_vec(&payload)?
            }
            ShareQuery::PointSeries {
                latitude_e7,
                longitude_e7,
                window,
                missing_policy,
            } => {
                let result = query_point_series(
                    &snapshot,
                    &PointSeriesRequest {
                        latitude: f64::from(*latitude_e7) / 10_000_000.0,
                        longitude: f64::from(*longitude_e7) / 10_000_000.0,
                        variables: request.variables.clone(),
                        time: protocol_time_range(window),
                        missing_policy: protocol_missing_policy(*missing_policy),
                    },
                )?;
                serde_json::to_vec(&TypedObjectPayload {
                    schema: POINT_SERIES_PAYLOAD_SCHEMA.into(),
                    request_sha256: request_sha256(request)?,
                    data: result,
                })?
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
                let time = snapshot.timepoint(*storage_slot)?;
                if time.valid_unix != *valid_unix {
                    return Err(CommunityError::Invalid(
                        "native-window storage slot does not match request valid time".into(),
                    ));
                }
                let bounds = (
                    usize::try_from(*x0).map_err(|_| CommunityError::Unsupported)?,
                    usize::try_from(*y0).map_err(|_| CommunityError::Unsupported)?,
                    usize::try_from(*x1).map_err(|_| CommunityError::Unsupported)?,
                    usize::try_from(*y1).map_err(|_| CommunityError::Unsupported)?,
                );
                let windows = if pressure_levels_hpa.is_empty() {
                    request
                        .variables
                        .iter()
                        .map(|variable| {
                            query_window_2d(
                                &snapshot,
                                &IndexWindow2DRequest {
                                    storage_slot: *storage_slot,
                                    variable: variable.clone(),
                                    x0: bounds.0,
                                    y0: bounds.1,
                                    x1: bounds.2,
                                    y1: bounds.3,
                                },
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(serde_json::to_value)??
                } else {
                    request
                        .variables
                        .iter()
                        .map(|variable| {
                            query_window_3d(
                                &snapshot,
                                &IndexWindow3DRequest {
                                    storage_slot: *storage_slot,
                                    variable: variable.clone(),
                                    levels_hpa: pressure_levels_hpa.clone(),
                                    x0: bounds.0,
                                    y0: bounds.1,
                                    x1: bounds.2,
                                    y1: bounds.3,
                                },
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()
                        .map(serde_json::to_value)??
                };
                serde_json::to_vec(&TypedObjectPayload {
                    schema: NATIVE_WINDOW_PAYLOAD_SCHEMA.into(),
                    request_sha256: request_sha256(request)?,
                    data: windows,
                })?
            }
            ShareQuery::TemporalGrid {
                window,
                reducer,
                semantics,
                missing_policy,
                pressure_levels_hpa,
            } => {
                let result = reduce_temporal_grid(
                    &snapshot,
                    &TemporalGridRequest {
                        variables: request.variables.clone(),
                        semantics: parse_temporal_semantics(semantics, &request.recipe.parameters)?,
                        reducer: parse_temporal_reducer(reducer)?,
                        window: protocol_temporal_window(window)?,
                        expectation: parse_time_expectation(&request.recipe.parameters)?,
                        missing_policy: protocol_missing_policy(*missing_policy),
                        vertical: (!pressure_levels_hpa.is_empty()).then(|| {
                            TemporalVerticalSelection::PressureLevels {
                                levels_hpa: pressure_levels_hpa.clone(),
                            }
                        }),
                    },
                )?;
                serde_json::to_vec(&TypedObjectPayload {
                    schema: TEMPORAL_GRID_PAYLOAD_SCHEMA.into(),
                    request_sha256: request_sha256(request)?,
                    data: result,
                })?
            }
            ShareQuery::CaseArtifact { .. } => {
                // Case artifacts are never synthesized from an arbitrary
                // query or private directory. They enter the CAS only through
                // the authenticated, typed, rights-confirmed publication
                // endpoint above.
                return Err(CommunityError::NotFound);
            }
        };
        if bytes.is_empty() || bytes.len() as u64 > self.limits.max_encoded_bytes {
            return Err(CommunityError::Invalid(
                "origin result is empty or exceeds the object limit".into(),
            ));
        }
        let request_hash = request_sha256(request)?;
        let now = now_unix();
        let attributions = protocol_attributions(descriptor);
        let mut modification_notices = descriptor
            .provider_attributions
            .iter()
            .map(|attribution| attribution.modification_notice.clone())
            .filter(|notice| !notice.is_empty())
            .collect::<Vec<_>>();
        modification_notices.sort();
        modification_notices.dedup();
        if modification_notices.is_empty() {
            modification_notices
                .push("Rusty Weather selected, sampled, and re-encoded the source data.".into());
        }
        let manifest = ObjectManifest {
            schema: rw_community_protocol::OBJECT_SCHEMA.into(),
            request: request.clone(),
            request_sha256: request_hash,
            object_sha256: object_sha256(&bytes),
            content_type: "application/json".into(),
            compression: Compression::None,
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            attributions,
            modification_notices,
            created_unix: now,
            expires_unix: now.saturating_add(7 * 24 * 60 * 60),
        };
        enforce_request_attributions(request, &manifest)?;
        let signed = sign_object_manifest(manifest, ORIGIN_SIGNING_KEY_ID, key)?;
        verify_signed_object(
            &signed,
            request,
            &bytes,
            now_unix(),
            &self.trusted_keys,
            &self.limits,
        )?;
        Ok((signed, bytes))
    }

    fn note_popularity_and_promote(
        &self,
        request_hash: &str,
        signed: &SignedObjectManifest,
        object: &[u8],
    ) -> Result<(), CommunityError> {
        if !self.promotion_enabled
            || object.len() as u64 > self.promotion_maximum_bytes
            || self.hot.is_none()
        {
            return Ok(());
        }
        let hits = {
            let mut hits = self
                .hits
                .lock()
                .expect("community promotion mutex poisoned");
            let now = now_unix();
            let (started, count) = hits.entry(request_hash.into()).or_insert((now, 0));
            if now.saturating_sub(*started)
                >= i64::try_from(self.promotion_window_seconds).unwrap_or(i64::MAX)
            {
                *started = now;
                *count = 0;
            }
            *count = count.saturating_add(1);
            *count
        };
        if hits < self.promotion_hits {
            return Ok(());
        }
        let hot = self.hot.as_ref().expect("checked above");
        let manifest_bytes = serde_json::to_vec(signed)?;
        let promotion_bytes = (object.len() as u64)
            .checked_add(manifest_bytes.len() as u64)
            .ok_or_else(|| CommunityError::Invalid("promotion byte count overflowed".into()))?;
        match self
            .quota
            .reserve_promotion(current_month(), promotion_bytes)
        {
            Ok(()) => {}
            Err(CommunityStoreError::Quota) => {
                // Cost threshold deliberately degrades to local/origin rather
                // than failing an otherwise successful query.
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
        hot.put(
            &format!("v1/objects/{}", signed.manifest.object_sha256),
            object,
        )?;
        hot.put(
            &format!("v1/manifests/{request_hash}.json"),
            &manifest_bytes,
        )?;
        Ok(())
    }
}

fn resolved_response(
    request_sha256: String,
    signed_manifest: SignedObjectManifest,
) -> ResolveObjectResponse {
    ResolveObjectResponse {
        schema: RESOLVE_SCHEMA.into(),
        request_sha256,
        signed_manifest: Some(signed_manifest),
        delivery_order: vec![DeliverySource::R2HotObject, DeliverySource::Origin],
    }
}

fn origin_only_response(
    request_sha256: String,
    signed_manifest: SignedObjectManifest,
) -> ResolveObjectResponse {
    ResolveObjectResponse {
        schema: RESOLVE_SCHEMA.into(),
        request_sha256,
        signed_manifest: Some(signed_manifest),
        delivery_order: vec![DeliverySource::Origin],
    }
}

#[derive(Debug, Clone)]
struct HttpsOriginClient {
    base_url: String,
    bearer_token: Option<Arc<String>>,
    agent: ureq::Agent,
}

impl HttpsOriginClient {
    fn new(base_url: String, token_file: Option<&Path>) -> Result<Self, CommunityError> {
        let token = token_file.map(read_secret).transpose()?.map(Arc::new);
        let config = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(5)))
            .timeout_per_call(Some(Duration::from_secs(30)))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::WebPki)
                    .build(),
            )
            .build();
        Ok(Self {
            base_url: base_url.trim_end_matches('/').into(),
            bearer_token: token,
            agent: config.into(),
        })
    }
}

impl OriginResolver for HttpsOriginClient {
    fn resolve(
        &self,
        resolve_request: &ResolveObjectRequest,
        limits: &ProtocolLimits,
    ) -> Result<Option<(SignedObjectManifest, Vec<u8>)>, CommunityError> {
        let body = serde_json::to_vec(resolve_request)?;
        if body.len() as u64 > limits.max_manifest_bytes {
            return Err(CommunityError::Invalid(
                "origin resolve request exceeds limit".into(),
            ));
        }
        let mut request = self
            .agent
            .post(&format!(
                "{}{}",
                self.base_url,
                rw_community_protocol::RESOLVE_OBJECT_PATH
            ))
            .header("Content-Type", "application/json");
        if let Some(token) = &self.bearer_token {
            request = request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = match request.send(&body) {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(CommunityError::Upstream(error.to_string())),
        };
        let mut reader = response.into_body().into_reader();
        let mut response_bytes = Vec::new();
        (&mut reader)
            .take(limits.max_manifest_bytes.saturating_add(1))
            .read_to_end(&mut response_bytes)?;
        if response_bytes.len() as u64 > limits.max_manifest_bytes {
            return Err(CommunityError::Invalid(
                "origin resolve response exceeds limit".into(),
            ));
        }
        let resolved: ResolveObjectResponse = serde_json::from_slice(&response_bytes)?;
        if resolved.schema != RESOLVE_SCHEMA
            || resolved.request_sha256 != request_sha256(&resolve_request.request)?
        {
            return Err(CommunityError::Invalid(
                "origin resolved the wrong request".into(),
            ));
        }
        let Some(signed) = resolved.signed_manifest else {
            return Ok(None);
        };
        let mut object_request = self.agent.get(&format!(
            "{}/v1/community/objects/{}",
            self.base_url, signed.manifest.object_sha256
        ));
        if let Some(token) = &self.bearer_token {
            object_request = object_request.header("Authorization", &format!("Bearer {token}"));
        }
        let response = match object_request.call() {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(CommunityError::Upstream(error.to_string())),
        };
        let mut reader = response.into_body().into_reader();
        let mut object = Vec::new();
        (&mut reader)
            .take(limits.max_encoded_bytes.saturating_add(1))
            .read_to_end(&mut object)?;
        if object.len() as u64 > limits.max_encoded_bytes {
            return Err(CommunityError::Invalid(
                "origin object exceeds size limit".into(),
            ));
        }
        Ok(Some((signed, object)))
    }
}

fn protocol_attributions(descriptor: &rw_query::RunDescriptor) -> Vec<AttributionNotice> {
    descriptor
        .provider_attributions
        .iter()
        .map(|attribution| AttributionNotice {
            provider: descriptor
                .source_provenance
                .iter()
                .find(|source| {
                    (attribution.provider.contains("ECMWF") && source.provider == "ecmwf-open-data")
                        || (attribution.provider.contains("NOAA")
                            && source.provider.starts_with("noaa-"))
                })
                .map(|source| source.provider.clone())
                .unwrap_or_else(|| attribution.provider.clone()),
            notice: attribution.notice.clone(),
            source_url: attribution.source_url.clone(),
            license: attribution.license.clone(),
            license_url: attribution.license_url.clone(),
            terms_url: attribution.terms_url.clone(),
            disclaimer: attribution.disclaimer.clone(),
        })
        .collect()
}

fn validate_request_source(
    request: &rw_community_protocol::ShareRequest,
    descriptor: &rw_query::RunDescriptor,
) -> Result<(), CommunityError> {
    let private_origin_mismatch = private_origin_for_model(&descriptor.model)
        .is_some_and(|required| request.publication.data_origin != required);
    if private_origin_mismatch {
        return Err(CommunityError::Invalid(
            "private WRF/ArWen runs cannot be relabeled as public-provider data".into(),
        ));
    }
    if descriptor.source_provenance.is_empty() {
        if matches!(request.publication.data_origin, DataOrigin::PublicProvider) {
            return Err(CommunityError::Invalid(
                "a run without persisted provider provenance cannot be published as public-provider data"
                    .into(),
            ));
        }
        // Protocol validation already requires explicit owner publication and
        // redistribution rights for all private/user-provided origins.
        return Ok(());
    }
    let mut expected = descriptor
        .source_provenance
        .iter()
        .map(|source| rw_community_protocol::SourceProvenance {
            provider: source.provider.clone(),
            roles: source.roles.clone(),
            products: source.products.clone(),
        })
        .collect::<Vec<_>>();
    for source in &mut expected {
        source.provider = source.provider.trim().to_ascii_lowercase();
        source.roles.sort();
        source.roles.dedup();
        source.products.sort();
        source.products.dedup();
    }
    expected.sort_by(|left, right| {
        (&left.provider, &left.roles, &left.products).cmp(&(
            &right.provider,
            &right.roles,
            &right.products,
        ))
    });
    expected.dedup();
    if request.source_provenance != expected {
        return Err(CommunityError::Invalid(
            "signed source provenance does not match the immutable run snapshot".into(),
        ));
    }
    Ok(())
}

fn private_origin_for_model(model: &str) -> Option<DataOrigin> {
    let normalized = model.trim().to_ascii_lowercase();
    if normalized.contains("arwen") {
        Some(DataOrigin::PrivateArwen)
    } else if normalized.contains("wrf") {
        Some(DataOrigin::PrivateWrf)
    } else {
        None
    }
}

fn protocol_missing_policy(value: rw_community_protocol::MissingPolicy) -> MissingPolicy {
    match value {
        rw_community_protocol::MissingPolicy::Strict => MissingPolicy::Strict,
        rw_community_protocol::MissingPolicy::Partial => MissingPolicy::Partial,
    }
}

fn protocol_time_range(window: &rw_community_protocol::TimeWindow) -> TimeRange {
    match window {
        rw_community_protocol::TimeWindow::Utc {
            start_unix,
            end_unix,
        } => TimeRange {
            start_unix: Some(*start_unix),
            end_unix: Some(*end_unix),
        },
        rw_community_protocol::TimeWindow::LocalDay {
            resolved_start_unix,
            resolved_end_unix,
            ..
        } => TimeRange {
            start_unix: Some(*resolved_start_unix),
            end_unix: Some(*resolved_end_unix),
        },
    }
}

fn protocol_temporal_window(
    window: &rw_community_protocol::TimeWindow,
) -> Result<TemporalWindow, CommunityError> {
    let converted = match window {
        rw_community_protocol::TimeWindow::Utc {
            start_unix,
            end_unix,
        } => TemporalWindow::Utc {
            start_unix: *start_unix,
            end_unix: *end_unix,
        },
        rw_community_protocol::TimeWindow::LocalDay {
            date,
            timezone,
            resolved_start_unix,
            resolved_end_unix,
        } => {
            let converted = TemporalWindow::LocalDay {
                date: date.clone(),
                timezone: timezone.clone(),
            };
            let resolved = rw_query::resolve_temporal_window(&converted)?;
            if resolved.start_unix != *resolved_start_unix
                || resolved.end_unix != *resolved_end_unix
            {
                return Err(CommunityError::Invalid(
                    "signed local-day UTC bounds do not match timezone resolution".into(),
                ));
            }
            converted
        }
    };
    Ok(converted)
}

fn parse_temporal_reducer(value: &str) -> Result<TemporalReducer, CommunityError> {
    match value {
        "scalar_summary" => Ok(TemporalReducer::ScalarSummary),
        "interval_summary" => Ok(TemporalReducer::IntervalSummary),
        "interval_maximum_summary" => Ok(TemporalReducer::IntervalMaximumSummary),
        "cumulative_summary" => Ok(TemporalReducer::CumulativeSummary),
        "rate_summary" => Ok(TemporalReducer::RateSummary),
        "vector_summary" => Ok(TemporalReducer::VectorSummary),
        "circular_mean" => Ok(TemporalReducer::CircularMean),
        "categorical_summary" => Ok(TemporalReducer::CategoricalSummary),
        _ => Err(CommunityError::Invalid(
            "unknown signed temporal reducer".into(),
        )),
    }
}

fn parse_temporal_semantics(
    value: &str,
    parameters: &BTreeMap<String, String>,
) -> Result<TemporalSemantics, CommunityError> {
    match value {
        "instantaneous_scalar" => Ok(TemporalSemantics::InstantaneousScalar),
        "vector_components" => Ok(TemporalSemantics::VectorComponents),
        "circular_degrees" => Ok(TemporalSemantics::CircularDegrees),
        "categorical" => Ok(TemporalSemantics::Categorical),
        "cumulative_from_origin" => Ok(TemporalSemantics::CumulativeFromOrigin {
            include_first_value: parse_optional(parameters, "include_first_value", false)?,
            reset_tolerance: parse_optional(parameters, "reset_tolerance", 0.0)?,
        }),
        "interval_accumulation" => Ok(TemporalSemantics::IntervalAccumulation {
            support: parse_interval_support(parameters)?,
        }),
        "interval_maximum" => Ok(TemporalSemantics::IntervalMaximum {
            support: parse_interval_support(parameters)?,
        }),
        "interval_rate" => Ok(TemporalSemantics::IntervalRate {
            support: parse_interval_support(parameters)?,
            seconds_per_rate_unit: parse_required(parameters, "seconds_per_rate_unit")?,
            integral_units: parameters
                .get("integral_units")
                .filter(|value| !value.is_empty() && value.len() <= 64)
                .cloned()
                .ok_or_else(|| {
                    CommunityError::Invalid("interval_rate requires bounded integral_units".into())
                })?,
        }),
        _ => Err(CommunityError::Invalid(
            "unknown signed temporal semantics".into(),
        )),
    }
}

fn parse_interval_support(
    parameters: &BTreeMap<String, String>,
) -> Result<IntervalSupport, CommunityError> {
    match parameters.get("support").map(String::as_str) {
        Some("starts_at_valid_time") => Ok(IntervalSupport::StartsAtValidTime {
            seconds: parse_required(parameters, "support_seconds")?,
        }),
        Some("ends_at_valid_time") => Ok(IntervalSupport::EndsAtValidTime {
            seconds: parse_required(parameters, "support_seconds")?,
        }),
        Some("until_next_expected_time") => Ok(IntervalSupport::UntilNextExpectedTime),
        Some("since_previous_expected_time") => Ok(IntervalSupport::SincePreviousExpectedTime),
        _ => Err(CommunityError::Invalid(
            "interval semantics require a supported signed support parameter".into(),
        )),
    }
}

fn parse_time_expectation(
    parameters: &BTreeMap<String, String>,
) -> Result<TimeExpectation, CommunityError> {
    match parameters.get("expectation").map(String::as_str) {
        None | Some("manifest_axis") => Ok(TimeExpectation::ManifestAxis),
        Some("fixed_cadence") => Ok(TimeExpectation::FixedCadence {
            step_seconds: parse_required(parameters, "expectation_step_seconds")?,
            anchor_unix: parameters
                .get("expectation_anchor_unix")
                .map(|value| {
                    value.parse::<i64>().map_err(|_| {
                        CommunityError::Invalid("expectation_anchor_unix is not an integer".into())
                    })
                })
                .transpose()?,
        }),
        Some(_) => Err(CommunityError::Invalid(
            "unknown signed time expectation".into(),
        )),
    }
}

fn parse_required<T>(
    parameters: &BTreeMap<String, String>,
    name: &'static str,
) -> Result<T, CommunityError>
where
    T: std::str::FromStr,
{
    parameters
        .get(name)
        .ok_or_else(|| CommunityError::Invalid(format!("missing signed parameter '{name}'")))?
        .parse()
        .map_err(|_| CommunityError::Invalid(format!("signed parameter '{name}' is invalid")))
}

fn parse_optional<T>(
    parameters: &BTreeMap<String, String>,
    name: &'static str,
    default: T,
) -> Result<T, CommunityError>
where
    T: std::str::FromStr,
{
    parameters
        .get(name)
        .map(|value| {
            value.parse().map_err(|_| {
                CommunityError::Invalid(format!("signed parameter '{name}' is invalid"))
            })
        })
        .transpose()
        .map(|value| value.unwrap_or(default))
}

fn parse_trusted_keys(values: &[String]) -> Result<TrustedSigningKeys, CommunityError> {
    use base64::Engine as _;
    let mut keys = TrustedSigningKeys::new();
    for value in values {
        let (id, encoded) = value
            .split_once(':')
            .ok_or_else(|| CommunityError::Invalid("trusted key must be 'key-id:base64'".into()))?;
        if id.is_empty() || keys.contains_key(id) {
            return Err(CommunityError::Invalid(
                "trusted key id is empty or duplicated".into(),
            ));
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| CommunityError::Invalid("trusted public key is malformed".into()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| CommunityError::Invalid("trusted public key is not 32 bytes".into()))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| CommunityError::Invalid("trusted public key is invalid".into()))?;
        keys.insert(id.into(), key);
    }
    Ok(keys)
}

fn load_signing_key(path: &Path) -> Result<SigningKey, CommunityError> {
    use base64::Engine as _;
    let secret = read_secret(path)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .map_err(|_| CommunityError::Invalid("signing key must be base64".into()))?;
    let bytes: [u8; 32] = bytes
        .try_into()
        .map_err(|_| CommunityError::Invalid("signing key must contain 32 bytes".into()))?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn read_secret(path: &Path) -> Result<String, CommunityError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > MAX_SECRET_BYTES
    {
        return Err(CommunityError::Invalid(
            "secret must be a bounded regular file".into(),
        ));
    }
    validate_secret_permissions(&metadata)?;
    let value = fs::read_to_string(path)?.trim().to_string();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(CommunityError::Invalid(
            "secret file is empty or malformed".into(),
        ));
    }
    Ok(value)
}

#[cfg(unix)]
fn validate_secret_permissions(metadata: &fs::Metadata) -> Result<(), CommunityError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CommunityError::Invalid(
            "community secret file must not be accessible by group or other users".into(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secret_permissions(_metadata: &fs::Metadata) -> Result<(), CommunityError> {
    // The Windows installer/doctor must restrict the ACL to the service
    // identity and SYSTEM; std does not expose a portable ACL evaluator.
    Ok(())
}

fn validate_remote_key(key: &str) -> Result<(), CommunityError> {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains(['\\', '?', '#', '\0'])
        || key
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        return Err(CommunityError::Invalid("invalid remote object key".into()));
    }
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn current_month() -> u32 {
    let now = Utc::now();
    u32::try_from(now.year()).unwrap_or(0).saturating_mul(100) + now.month()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CaseRoomConfig, CommunityQuotasConfig, PromotionConfig};

    #[derive(Debug, Default)]
    struct MemoryProvider(Mutex<BTreeMap<String, Vec<u8>>>);

    impl ImmutableObjectProvider for MemoryProvider {
        fn get(&self, key: &str, maximum_bytes: u64) -> Result<Option<Vec<u8>>, CommunityError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .get(key)
                .filter(|value| value.len() as u64 <= maximum_bytes)
                .cloned())
        }

        fn put(&self, key: &str, bytes: &[u8]) -> Result<(), CommunityError> {
            self.0.lock().unwrap().insert(key.into(), bytes.into());
            Ok(())
        }
    }

    #[derive(Debug)]
    struct UnavailableProvider;

    impl ImmutableObjectProvider for UnavailableProvider {
        fn get(&self, _key: &str, _maximum_bytes: u64) -> Result<Option<Vec<u8>>, CommunityError> {
            Err(CommunityError::Upstream("simulated R2 outage".into()))
        }

        fn put(&self, _key: &str, _bytes: &[u8]) -> Result<(), CommunityError> {
            Err(CommunityError::Upstream("simulated R2 outage".into()))
        }
    }

    #[derive(Debug)]
    struct StaticOrigin {
        signing_key: SigningKey,
    }

    impl OriginResolver for StaticOrigin {
        fn resolve(
            &self,
            request: &ResolveObjectRequest,
            _limits: &ProtocolLimits,
        ) -> Result<Option<(SignedObjectManifest, Vec<u8>)>, CommunityError> {
            let object = br#"{"source":"hetzner-origin"}"#.to_vec();
            let created_unix = now_unix();
            let manifest = ObjectManifest {
                schema: rw_community_protocol::OBJECT_SCHEMA.into(),
                request: request.request.clone(),
                request_sha256: request_sha256(&request.request)?,
                object_sha256: object_sha256(&object),
                content_type: "application/json".into(),
                compression: Compression::None,
                encoded_size: object.len() as u64,
                decoded_size: object.len() as u64,
                attributions: vec![],
                modification_notices: vec![],
                created_unix,
                expires_unix: created_unix + 3600,
            };
            Ok(Some((
                sign_object_manifest(manifest, ORIGIN_SIGNING_KEY_ID, &self.signing_key)?,
                object,
            )))
        }
    }

    fn config(directory: &tempfile::TempDir) -> CommunityConfig {
        use base64::Engine as _;
        let key_path = directory.path().join("signing.key");
        fs::write(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode([7u8; 32]),
        )
        .unwrap();
        CommunityConfig {
            enabled: true,
            root: directory.path().join("cas"),
            signing_key_file: Some(key_path),
            hot_store: HotStoreConfig::Disabled,
            promotion: PromotionConfig {
                enabled: true,
                minimum_hits: 1,
                ..PromotionConfig::default()
            },
            quotas: CommunityQuotasConfig {
                maximum_object_bytes: 1024,
                maximum_decompressed_bytes: 4096,
                storage_bytes: 4096,
                ..CommunityQuotasConfig::default()
            },
            cases: CaseRoomConfig::default(),
            ..CommunityConfig::default()
        }
    }

    fn publication_config(directory: &tempfile::TempDir) -> CommunityConfig {
        let mut value = config(directory);
        value.quotas.maximum_object_bytes = 256 * 1024;
        value.quotas.maximum_decompressed_bytes = 1024 * 1024;
        value.quotas.storage_bytes = 4 * 1024 * 1024;
        value.cases = CaseRoomConfig {
            enabled: true,
            artifact_publication_enabled: true,
            maximum_manifest_bytes: 256 * 1024,
            maximum_objects_per_case: 16,
            maximum_cases: 16,
            storage_bytes: 1024 * 1024,
            default_retention_seconds: 7 * 24 * 60 * 60,
            ..CaseRoomConfig::default()
        };
        value
    }

    fn artifact_publication(principal: &str, origin: DataOrigin) -> PublishCaseArtifactRequest {
        let now = now_unix();
        PublishCaseArtifactRequest {
            schema: rw_community_protocol::CASE_ARTIFACT_PUBLICATION_SCHEMA.into(),
            owner_principal_sha256: principal.into(),
            request: rw_community_protocol::ShareRequest {
                schema: rw_community_protocol::REQUEST_SCHEMA.into(),
                model: "owner-wrf".into(),
                run: "20260812T00Z".into(),
                snapshot_id: "a".repeat(64),
                grid_hash: "b".repeat(64),
                variables: vec!["annotation".into()],
                query: ShareQuery::CaseArtifact {
                    case_id: "case-published".into(),
                    artifact_id: "note-a".into(),
                    artifact_type: rw_community_protocol::CaseArtifactType::Annotation,
                },
                recipe: rw_community_protocol::RecipeIdentity {
                    recipe_id: "case-annotation".into(),
                    recipe_version: "1".into(),
                    parameters: BTreeMap::from([(
                        rw_community_protocol::PUBLICATION_OWNER_PARAMETER.into(),
                        principal.into(),
                    )]),
                },
                source_provenance: vec![rw_community_protocol::SourceProvenance {
                    provider: "simulation-owner".into(),
                    roles: vec!["simulation".into()],
                    products: vec!["wrf".into()],
                }],
                publication: rw_community_protocol::PublicationGrant {
                    data_origin: origin,
                    explicit_owner_publication: true,
                    redistribution_rights_confirmed: true,
                },
            },
            payload: rw_community_protocol::CaseArtifactPayload::Annotation(
                rw_community_protocol::AnnotationArtifact {
                    title: "Owner analysis".into(),
                    text: "Simulated circulation reached peak intensity.".into(),
                    event_unix: Some(now),
                },
            ),
            published_unix: now,
            retain_until_unix: now + 3600,
            attributions: vec![AttributionNotice {
                provider: "simulation-owner".into(),
                notice: "Published by the simulation owner.".into(),
                source_url: "https://example.invalid/source".into(),
                license: "Owner-authorized redistribution".into(),
                license_url: "https://example.invalid/license".into(),
                terms_url: "https://example.invalid/terms".into(),
                disclaimer: "Experimental simulation.".into(),
            }],
            modification_notices: vec!["Encoded as a typed annotation.".into()],
        }
    }

    fn case_for_publication(
        publication: &PublishCaseArtifactRequest,
        signed: &SignedObjectManifest,
    ) -> CaseRoomManifest {
        let ShareQuery::CaseArtifact {
            case_id,
            artifact_id,
            artifact_type,
        } = &publication.request.query
        else {
            unreachable!()
        };
        CaseRoomManifest {
            schema: CASE_SCHEMA.into(),
            case_id: case_id.clone(),
            title: "Rights-confirmed simulation case".into(),
            event_start_unix: publication.published_unix - 3600,
            event_end_unix: publication.published_unix,
            published_unix: publication.published_unix,
            retain_until_unix: publication.retain_until_unix,
            publication: publication.request.publication.clone(),
            sources: vec![rw_community_protocol::CaseModelSource {
                model: publication.request.model.clone(),
                run: publication.request.run.clone(),
                snapshot_id: publication.request.snapshot_id.clone(),
                grid_hash: publication.request.grid_hash.clone(),
                source_provenance: publication.request.source_provenance.clone(),
            }],
            artifacts: vec![rw_community_protocol::CaseArtifactRef {
                artifact_id: artifact_id.clone(),
                artifact_type: *artifact_type,
                request_sha256: signed.manifest.request_sha256.clone(),
                object_sha256: signed.manifest.object_sha256.clone(),
            }],
            attributions: publication.attributions.clone(),
            modification_notices: publication.modification_notices.clone(),
        }
    }

    #[test]
    fn filesystem_provider_is_immutable_and_path_safe() {
        let directory = tempfile::tempdir().unwrap();
        let provider = FilesystemObjectProvider::open(directory.path()).unwrap();
        provider.put("v1/objects/abc", b"one").unwrap();
        provider.put("v1/objects/abc", b"one").unwrap();
        assert!(provider.put("v1/objects/abc", b"two").is_err());
        assert!(provider.get("../secret", 100).is_err());
    }

    #[test]
    fn typed_private_artifact_is_owner_bound_signed_and_exactly_retrievable() {
        let directory = tempfile::tempdir().unwrap();
        let service = CommunityService::open(&publication_config(&directory)).unwrap();
        let principal = "f".repeat(64);
        let publication = artifact_publication(&principal, DataOrigin::PrivateWrf);
        let signed = service
            .publish_case_artifact(&principal, publication.clone())
            .unwrap();
        assert_eq!(
            signed.manifest.request.publication.data_origin,
            DataOrigin::PrivateWrf
        );
        assert_eq!(
            signed
                .manifest
                .request
                .recipe
                .parameters
                .get(rw_community_protocol::PUBLICATION_OWNER_PARAMETER),
            Some(&principal)
        );
        let bytes = service
            .object(&principal, &signed.manifest.object_sha256)
            .unwrap();
        assert_eq!(object_sha256(&bytes), signed.manifest.object_sha256);
        validate_case_artifact_payload_bytes(&bytes, &publication.request, &service.limits)
            .unwrap();

        let case = case_for_publication(&publication, &signed);
        let signed_case = service.publish_case(&principal, case).unwrap();
        assert_eq!(
            service.case(&principal, "case-published").unwrap(),
            signed_case
        );
    }

    #[test]
    fn artifact_publication_rejects_impersonation_missing_rights_and_kill_switch() {
        let directory = tempfile::tempdir().unwrap();
        let service = CommunityService::open(&publication_config(&directory)).unwrap();
        let principal = "f".repeat(64);
        let publication = artifact_publication(&principal, DataOrigin::PrivateArwen);
        assert!(matches!(
            service.publish_case_artifact(&"e".repeat(64), publication.clone()),
            Err(CommunityError::Invalid(_))
        ));
        let mut no_rights = publication.clone();
        no_rights
            .request
            .publication
            .redistribution_rights_confirmed = false;
        assert!(matches!(
            service.publish_case_artifact(&principal, no_rights),
            Err(CommunityError::Protocol(
                ProtocolError::RedistributionRightsUnconfirmed
            ))
        ));
        let mut relabeled = artifact_publication(&principal, DataOrigin::PublicProvider);
        relabeled.request.model = "wrf".into();
        assert!(matches!(
            service.publish_case_artifact(&principal, relabeled),
            Err(CommunityError::Invalid(_))
        ));
        service.set_kill_switch(true);
        assert!(matches!(
            service.publish_case_artifact(&principal, publication),
            Err(CommunityError::Killed)
        ));
    }

    #[test]
    fn rights_withdrawal_tombstones_artifact_and_case() {
        let directory = tempfile::tempdir().unwrap();
        let service = CommunityService::open(&publication_config(&directory)).unwrap();
        let principal = "f".repeat(64);
        let publication = artifact_publication(&principal, DataOrigin::PrivateWrf);
        let signed = service
            .publish_case_artifact(&principal, publication.clone())
            .unwrap();
        service
            .publish_case(&principal, case_for_publication(&publication, &signed))
            .unwrap();
        service
            .revoke_case(
                &principal,
                "case-published",
                RevokePublicationRequest {
                    schema: CASE_REVOCATION_SCHEMA.into(),
                    rights_withdrawn: true,
                    reason: "Owner withdrew case publication rights.".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            service.case(&principal, "case-published"),
            Err(CommunityError::NotFound)
        ));
        service
            .revoke_case_artifact(
                &principal,
                &signed.manifest.object_sha256,
                RevokePublicationRequest {
                    schema: CASE_ARTIFACT_REVOCATION_SCHEMA.into(),
                    rights_withdrawn: true,
                    reason: "Owner withdrew artifact redistribution rights.".into(),
                },
            )
            .unwrap();
        assert!(matches!(
            service.object(&principal, &signed.manifest.object_sha256),
            Err(CommunityError::NotFound)
        ));
        assert!(
            service
                .cas()
                .unwrap()
                .publication_tombstone(&signed.manifest.object_sha256)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn ecmwf_artifact_notices_must_propagate_into_case() {
        let directory = tempfile::tempdir().unwrap();
        let service = CommunityService::open(&publication_config(&directory)).unwrap();
        let principal = "f".repeat(64);
        let mut publication = artifact_publication(&principal, DataOrigin::PublicProvider);
        publication.request.model = "ifs".into();
        publication.request.source_provenance = vec![rw_community_protocol::SourceProvenance {
            provider: "ecmwf-open-data".into(),
            roles: vec!["pressure".into()],
            products: vec!["ifs".into()],
        }];
        publication.attributions.clear();
        assert!(matches!(
            service.publish_case_artifact(&principal, publication.clone()),
            Err(CommunityError::Protocol(ProtocolError::MissingEcmwfNotice))
        ));

        publication.attributions = vec![AttributionNotice::ecmwf_open_data()];
        publication.modification_notices =
            vec!["Rusty Weather encoded a typed case annotation.".into()];
        let signed = service
            .publish_case_artifact(&principal, publication.clone())
            .unwrap();
        assert_eq!(signed.manifest.attributions, publication.attributions);

        let mut case = case_for_publication(&publication, &signed);
        case.attributions.clear();
        assert!(matches!(
            service.publish_case(&principal, case),
            Err(CommunityError::Invalid(_))
                | Err(CommunityError::Protocol(ProtocolError::MissingEcmwfNotice))
        ));
        service
            .publish_case(&principal, case_for_publication(&publication, &signed))
            .unwrap();
    }

    #[test]
    fn kill_switch_stops_assisted_reads_and_case_publication() {
        let directory = tempfile::tempdir().unwrap();
        let service = CommunityService::open(&config(&directory)).unwrap();
        let request = test_share_request();
        let identity = request_sha256(&request).unwrap();
        let object = br#"{"cached":true}"#;
        let created_unix = now_unix();
        let signed = sign_object_manifest(
            ObjectManifest {
                schema: rw_community_protocol::OBJECT_SCHEMA.into(),
                request: request.clone(),
                request_sha256: identity.clone(),
                object_sha256: object_sha256(object),
                content_type: "application/json".into(),
                compression: Compression::None,
                encoded_size: object.len() as u64,
                decoded_size: object.len() as u64,
                attributions: vec![],
                modification_notices: vec![],
                created_unix,
                expires_unix: created_unix + 3600,
            },
            ORIGIN_SIGNING_KEY_ID,
            &SigningKey::from_bytes(&[7u8; 32]),
        )
        .unwrap();
        service
            .cas()
            .unwrap()
            .put(
                &identity,
                &signed.manifest.object_sha256,
                object,
                &serde_json::to_vec(&signed).unwrap(),
            )
            .unwrap();
        assert_eq!(
            service
                .object("test", &signed.manifest.object_sha256)
                .unwrap(),
            Bytes::from_static(object)
        );
        let case = test_case_for_object(&request, &signed);
        let signed_case = sign_case_manifest(
            case,
            ORIGIN_SIGNING_KEY_ID,
            &SigningKey::from_bytes(&[7u8; 32]),
        )
        .unwrap();
        service
            .cas()
            .unwrap()
            .put_case(
                &signed_case.manifest.case_id,
                &serde_json::to_vec(&signed_case).unwrap(),
            )
            .unwrap();
        assert!(service.cas().unwrap().get_case("case-a").unwrap().is_some());
        service.set_kill_switch(true);
        assert!(matches!(
            service.object("test", &signed.manifest.object_sha256),
            Err(CommunityError::Killed)
        ));
        assert!(matches!(
            service.publish_case("test", test_case()),
            Err(CommunityError::Killed)
        ));
        assert!(matches!(
            service.case("test", "case-a"),
            Err(CommunityError::Killed)
        ));
    }

    #[test]
    fn kill_switch_preserves_signed_https_origin_resolution() {
        let directory = tempfile::tempdir().unwrap();
        let origin = Arc::new(StaticOrigin {
            signing_key: SigningKey::from_bytes(&[7u8; 32]),
        });
        let service =
            CommunityService::with_providers(&config(&directory), None, Some(origin)).unwrap();
        service.set_kill_switch(true);
        let catalog = StoreCatalog::new(directory.path().join("empty-store"));

        let (response, object) = service
            .resolve(
                "test",
                &ResolveObjectRequest {
                    schema: RESOLVE_SCHEMA.into(),
                    request: test_share_request(),
                },
                &catalog,
            )
            .unwrap();

        assert!(object.is_some());
        assert_eq!(response.delivery_order, vec![DeliverySource::Origin]);
    }

    #[test]
    fn malformed_hot_manifest_falls_through_to_https_origin() {
        let directory = tempfile::tempdir().unwrap();
        let hot = Arc::new(MemoryProvider::default());
        let request = test_share_request();
        let identity = request_sha256(&request).unwrap();
        hot.put(&format!("v1/manifests/{identity}.json"), b"not-json")
            .unwrap();
        let origin = Arc::new(StaticOrigin {
            signing_key: SigningKey::from_bytes(&[7u8; 32]),
        });
        let service =
            CommunityService::with_providers(&config(&directory), Some(hot), Some(origin)).unwrap();
        let catalog = StoreCatalog::new(directory.path().join("empty-store"));
        let (response, object) = service
            .resolve(
                "test",
                &ResolveObjectRequest {
                    schema: RESOLVE_SCHEMA.into(),
                    request,
                },
                &catalog,
            )
            .unwrap();

        assert_eq!(
            object.unwrap(),
            Bytes::from_static(br#"{"source":"hetzner-origin"}"#)
        );
        assert_eq!(
            response.delivery_order,
            vec![DeliverySource::R2HotObject, DeliverySource::Origin]
        );
        assert_eq!(
            response.signed_manifest.unwrap().signature.signing_key_id,
            ORIGIN_SIGNING_KEY_ID
        );
    }

    #[test]
    fn r2_outage_and_failed_promotion_do_not_break_https_origin() {
        let directory = tempfile::tempdir().unwrap();
        let request = test_share_request();
        let origin = Arc::new(StaticOrigin {
            signing_key: SigningKey::from_bytes(&[7u8; 32]),
        });
        let service = CommunityService::with_providers(
            &config(&directory),
            Some(Arc::new(UnavailableProvider)),
            Some(origin),
        )
        .unwrap();
        let catalog = StoreCatalog::new(directory.path().join("empty-store"));

        let (response, object) = service
            .resolve(
                "test",
                &ResolveObjectRequest {
                    schema: RESOLVE_SCHEMA.into(),
                    request,
                },
                &catalog,
            )
            .unwrap();

        assert!(object.is_some());
        assert_eq!(
            response.delivery_order,
            vec![DeliverySource::R2HotObject, DeliverySource::Origin]
        );
    }

    #[test]
    fn private_wrf_and_arwen_models_cannot_be_relabelled_public() {
        fn descriptor(model: &str) -> rw_query::RunDescriptor {
            rw_query::RunDescriptor {
                model: model.into(),
                run: "20260812T00Z".into(),
                schema: "rw-store.run.v2".into(),
                snapshot_id: "a".repeat(64),
                grid_hash: "b".repeat(64),
                nx: 1,
                ny: 1,
                exact_time_axis: true,
                origin_unix: Some(0),
                sample_count: 1,
                first_valid_unix: Some(0),
                last_valid_unix: Some(0),
                source_provenance: vec![rw_query::SourceProvenance {
                    provider: "owner-local".into(),
                    roles: vec!["pressure".into()],
                    products: vec!["wrfout".into()],
                }],
                provider_attributions: vec![],
            }
        }

        for (model, required_origin) in [
            ("wrf", DataOrigin::PrivateWrf),
            ("raw-wrf-d02", DataOrigin::PrivateWrf),
            ("arwen", DataOrigin::PrivateArwen),
            ("private-arwen-d03", DataOrigin::PrivateArwen),
        ] {
            let mut request = test_share_request();
            request.model = model.into();
            request.source_provenance = vec![rw_community_protocol::SourceProvenance {
                provider: "owner-local".into(),
                roles: vec!["pressure".into()],
                products: vec!["wrfout".into()],
            }];
            assert!(validate_request_source(&request, &descriptor(model)).is_err());

            request.publication = rw_community_protocol::PublicationGrant {
                data_origin: required_origin,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            };
            request.validate(&ProtocolLimits::default()).unwrap();
            validate_request_source(&request, &descriptor(model)).unwrap();
        }
    }

    fn test_share_request() -> rw_community_protocol::ShareRequest {
        rw_community_protocol::ShareRequest {
            schema: rw_community_protocol::REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812T00Z".into(),
            snapshot_id: "a".repeat(64),
            grid_hash: "b".repeat(64),
            variables: vec!["temperature".into(), "temperature_2m".into()],
            query: ShareQuery::Profile {
                latitude_e7: 0,
                longitude_e7: 0,
                storage_slot: 0,
                valid_unix: 0,
                pressure_variables: vec!["temperature".into()],
                surface_variables: vec!["temperature_2m".into()],
                pressure_levels_hpa: vec![],
            },
            recipe: rw_community_protocol::RecipeIdentity {
                recipe_id: "native-profile".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![rw_community_protocol::SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                roles: vec!["pressure".into()],
                products: vec!["wrfprs".into()],
            }],
            publication: rw_community_protocol::PublicationGrant {
                data_origin: rw_community_protocol::DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
    }

    fn test_case() -> CaseRoomManifest {
        CaseRoomManifest {
            schema: CASE_SCHEMA.into(),
            case_id: "case-a".into(),
            title: "Test case".into(),
            event_start_unix: 1,
            event_end_unix: 2,
            published_unix: 3,
            retain_until_unix: 4,
            publication: rw_community_protocol::PublicationGrant {
                data_origin: rw_community_protocol::DataOrigin::UserProvided,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            sources: vec![],
            artifacts: vec![],
            attributions: vec![],
            modification_notices: vec![],
        }
    }

    fn test_case_for_object(
        request: &rw_community_protocol::ShareRequest,
        signed: &SignedObjectManifest,
    ) -> CaseRoomManifest {
        let now = now_unix();
        CaseRoomManifest {
            schema: CASE_SCHEMA.into(),
            case_id: "case-a".into(),
            title: "Cached test case".into(),
            event_start_unix: now - 3600,
            event_end_unix: now,
            published_unix: now,
            retain_until_unix: now + 3600,
            publication: rw_community_protocol::PublicationGrant {
                data_origin: rw_community_protocol::DataOrigin::PublicProvider,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            sources: vec![rw_community_protocol::CaseModelSource {
                model: request.model.clone(),
                run: request.run.clone(),
                snapshot_id: request.snapshot_id.clone(),
                grid_hash: request.grid_hash.clone(),
                source_provenance: request.source_provenance.clone(),
            }],
            artifacts: vec![rw_community_protocol::CaseArtifactRef {
                artifact_id: "artifact-a".into(),
                artifact_type: rw_community_protocol::CaseArtifactType::DerivedTable,
                request_sha256: signed.manifest.request_sha256.clone(),
                object_sha256: signed.manifest.object_sha256.clone(),
            }],
            attributions: vec![],
            modification_notices: vec!["Derived by Rusty Weather.".into()],
        }
    }
}

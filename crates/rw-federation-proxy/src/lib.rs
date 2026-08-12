//! Authoritative, server-side failover to deliberately public Rusty Weather
//! origins.
//!
//! This crate is deliberately independent of BowEcho and Community Cache's
//! relay data plane. A trusted authority selects an already verified public
//! descriptor, uses an origin-scoped server credential, verifies the returned
//! immutable object against the descriptor's object keys and the caller's
//! exact canonical request, then re-signs the same manifest for its normal
//! HTTPS client contract.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{SigningKey, VerifyingKey};
use rw_community_protocol::{
    CASE_ARTIFACT_PAYLOAD_SCHEMA, Compression, DeliverySource, FederationQueryCapability,
    GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA, NATIVE_WINDOW_PAYLOAD_SCHEMA, POINT_SERIES_PAYLOAD_SCHEMA,
    ProfileObjectPayload, ProtocolError, ProtocolLimits, PublicOriginDescriptor, RESOLVE_SCHEMA,
    ResolveObjectRequest, ResolveObjectResponse, ShareQuery, ShareRequest, SignedObjectManifest,
    TEMPORAL_GRID_PAYLOAD_SCHEMA, TrustedSigningKeys, TypedObjectPayload,
    enforce_request_attributions, parse_verifying_key_base64, request_sha256, sign_object_manifest,
    validate_case_artifact_payload_bytes, validate_profile_payload_identity,
    validate_typed_payload_identity, verify_signed_object,
};
use rw_query::{
    GeographicFieldValues, GeographicWindowResult, IndexWindow2DResult, IndexWindow3DResult,
    PointSeriesResult, ProfileResult, RunDescriptor, TemporalGridMetadata, TemporalGridResult,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector as _, RustlsConnector, TcpConnector};

pub const FEDERATION_PROXY_SCHEMA: &str = "rw.federation.proxy-resolve.v1";
pub const FEDERATION_PROXY_PATH: &str = "/v1/federation/objects/resolve";
/// Dedicated one-hop upstream route. Its server handler must consult only
/// local CAS, local R2 and the node's published store; it must never invoke
/// FederationProxy again.
pub const FEDERATION_LOCAL_RESOLVE_PATH: &str = "/v1/federation/objects/resolve-local";
/// Dedicated one-hop object route protected by the same origin-scoped token.
/// It is intentionally separate from BowEcho's normal Community object route.
pub const FEDERATION_LOCAL_OBJECT_PATH_PREFIX: &str = "/v1/federation/objects";
pub const FEDERATION_HOP_HEADER: &str = "x-rusty-federation-hop";

const MAX_ORIGIN_ID_BYTES: usize = 96;
const MAX_PRODUCT_BYTES: usize = 128;
const MAX_SECRET_BYTES: usize = 8 * 1024;
const MAX_DNS_ANSWERS: usize = 16;
const MAX_SCOPED_ORIGINS: usize = 128;
const HTTP_READ_CHUNK: usize = 64 * 1024;
const MAX_AUTHORITY_RETENTION_SECONDS: u64 = 5 * 366 * 24 * 60 * 60;

/// Exact proxy request. `request` is the same canonical Community Cache
/// identity used for local/R2/Hetzner delivery. The optional origin id is only
/// a preference among candidates already admitted by the authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FederationProxyRequest {
    pub schema: String,
    pub request: ShareRequest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_origin_id: Option<String>,
}

impl FederationProxyRequest {
    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), FederationProxyError> {
        if self.schema != FEDERATION_PROXY_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()).into());
        }
        self.request.validate(limits)?;
        if let Some(origin_id) = &self.preferred_origin_id {
            validate_id(origin_id, MAX_ORIGIN_ID_BYTES)?;
        }
        Ok(())
    }
}

/// One candidate returned by rw-server's already verified FederationService.
/// `matched_product` is the exact signed product capability that the adapter
/// used during selection; the core rechecks it before performing network I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyCandidate {
    pub descriptor: PublicOriginDescriptor,
    pub matched_product: String,
    pub consecutive_failures: u32,
}

/// Coarse feedback only. Neither raw transport errors nor resolved addresses
/// cross this boundary into federation health state or application logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyHealthObservation {
    Healthy,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("federated public-origin directory is unavailable")]
pub struct DirectoryUnavailable;

/// Adapter implemented by rw-server around FederationService. Selection must
/// return only currently verified, non-quarantined, operator-approved origins.
pub trait VerifiedFederationDirectory: Send + Sync {
    fn candidates(
        &self,
        request: &ShareRequest,
        minimum_response_bytes: u64,
    ) -> Result<Vec<ProxyCandidate>, DirectoryUnavailable>;

    fn record_health(
        &self,
        origin_id: &str,
        observation: ProxyHealthObservation,
    ) -> Result<(), DirectoryUnavailable>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamObject {
    pub resolve: ResolveObjectResponse,
    pub encoded_object: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum UpstreamFailure {
    #[error("upstream object was not found")]
    NotFound,
    #[error("upstream request failed")]
    Unavailable,
    #[error("upstream response was redirected")]
    RedirectRejected,
    #[error("upstream response was malformed or exceeded its bound")]
    InvalidResponse,
    #[error("upstream DNS answer was rejected")]
    DnsRejected,
}

/// Transport owns credential lookup. The proxy core passes only an approved
/// origin id and signed descriptor, never a bearer token or socket address.
pub trait FederatedOriginTransport: Send + Sync {
    fn fetch(
        &self,
        candidate: &ProxyCandidate,
        request: &ResolveObjectRequest,
        limits: &ProtocolLimits,
    ) -> Result<UpstreamObject, UpstreamFailure>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("verified object staging failed")]
pub struct StageFailure;

/// Server adapter for local CAS and optional R2 promotion. It receives only an
/// authority-signed manifest after the upstream signature, hash, identity,
/// schema, decompression and attribution checks have all succeeded.
pub trait VerifiedObjectSink: Send + Sync {
    fn stage(
        &self,
        request_sha256: &str,
        manifest: &SignedObjectManifest,
        encoded_object: &[u8],
    ) -> Result<(), StageFailure>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("federation proxy quota was exhausted")]
pub struct QuotaUnavailable;

pub trait FederationProxyQuota: Send + Sync {
    type Permit;

    /// Atomically acquire concurrency and durably consume the complete
    /// conservative upstream byte bound before any origin transport starts.
    /// The byte reservation is intentionally not refunded when an attempt
    /// fails: an origin may already have emitted the bounded response.
    fn reserve(
        &self,
        principal: &str,
        maximum_upstream_bytes: u64,
    ) -> Result<Self::Permit, QuotaUnavailable>;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopQuota;

impl FederationProxyQuota for NoopQuota {
    type Permit = ();

    fn reserve(
        &self,
        _principal: &str,
        _maximum_upstream_bytes: u64,
    ) -> Result<Self::Permit, QuotaUnavailable> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct NoopSink;

impl VerifiedObjectSink for NoopSink {
    fn stage(
        &self,
        _request_sha256: &str,
        _manifest: &SignedObjectManifest,
        _encoded_object: &[u8],
    ) -> Result<(), StageFailure> {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum FederationProxyError {
    #[error("federation proxy is disabled")]
    Disabled,
    #[error("federation proxy request is invalid")]
    InvalidRequest,
    #[error("preferred public origin is not an approved candidate")]
    UnapprovedOriginHint,
    #[error("no approved public origin can satisfy the exact request")]
    NoCandidate,
    #[error("approved public-origin fallback is unavailable after {attempts} bounded attempts")]
    Unavailable { attempts: usize },
    #[error("verified upstream object could not be staged")]
    Stage,
    #[error("federation proxy quota is exhausted")]
    Quota,
    #[error("federation proxy configuration is invalid")]
    InvalidConfiguration,
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Result served through the normal authoritative HTTPS object contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationProxyResult {
    pub response: ResolveObjectResponse,
    pub encoded_object: Vec<u8>,
    /// Public operator-approved id, safe for coarse metrics. No endpoint,
    /// credential, DNS answer or transport diagnostic is retained.
    pub public_origin_id: String,
}

pub struct FederationProxy<D, T, S, Q> {
    enabled: bool,
    killed: AtomicBool,
    authority_origin_id: String,
    authority_https_root: CanonicalPublicHttpsRoot,
    authority_signing_key_id: String,
    authority_signing_key: SigningKey,
    revoked_key_ids: BTreeSet<String>,
    maximum_attempts: usize,
    authority_retention_seconds: i64,
    limits: ProtocolLimits,
    directory: D,
    transport: T,
    sink: S,
    quota: Q,
}

impl<D, T, S, Q> fmt::Debug for FederationProxy<D, T, S, Q> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FederationProxy")
            .field("enabled", &self.enabled)
            .field("killed", &self.killed.load(Ordering::Acquire))
            .field("authority_origin_id", &self.authority_origin_id)
            .field("authority_https_root", &self.authority_https_root.raw)
            .field("authority_signing_key_id", &self.authority_signing_key_id)
            .field("maximum_attempts", &self.maximum_attempts)
            .finish_non_exhaustive()
    }
}

pub struct FederationProxyConfig {
    pub enabled: bool,
    pub kill_switch: bool,
    pub authority_origin_id: String,
    pub authority_https_root: String,
    pub authority_signing_key_id: String,
    pub authority_signing_key: SigningKey,
    pub revoked_key_ids: BTreeSet<String>,
    pub maximum_attempts: usize,
    /// Same bounded policy as the authority's normal Community objects. The
    /// rw-server adapter must pass `community.object_manifest_retention_seconds`.
    pub authority_retention_seconds: u64,
    pub limits: ProtocolLimits,
}

impl fmt::Debug for FederationProxyConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FederationProxyConfig")
            .field("enabled", &self.enabled)
            .field("kill_switch", &self.kill_switch)
            .field("authority_origin_id", &self.authority_origin_id)
            .field("authority_https_root", &self.authority_https_root)
            .field("authority_signing_key_id", &self.authority_signing_key_id)
            .field("authority_signing_key", &"[REDACTED]")
            .field("maximum_attempts", &self.maximum_attempts)
            .field(
                "authority_retention_seconds",
                &self.authority_retention_seconds,
            )
            .finish_non_exhaustive()
    }
}

impl<D, T, S, Q> FederationProxy<D, T, S, Q>
where
    D: VerifiedFederationDirectory,
    T: FederatedOriginTransport,
    S: VerifiedObjectSink,
    Q: FederationProxyQuota,
{
    pub fn new(
        config: FederationProxyConfig,
        directory: D,
        transport: T,
        sink: S,
        quota: Q,
    ) -> Result<Self, FederationProxyError> {
        validate_id(&config.authority_origin_id, MAX_ORIGIN_ID_BYTES)?;
        validate_id(&config.authority_signing_key_id, 128)?;
        if config.maximum_attempts == 0
            || config.maximum_attempts > MAX_SCOPED_ORIGINS
            || config.authority_retention_seconds == 0
            || config.authority_retention_seconds > MAX_AUTHORITY_RETENTION_SECONDS
            || config
                .revoked_key_ids
                .contains(&config.authority_signing_key_id)
        {
            return Err(FederationProxyError::InvalidConfiguration);
        }
        let authority_retention_seconds = i64::try_from(config.authority_retention_seconds)
            .map_err(|_| FederationProxyError::InvalidConfiguration)?;
        Ok(Self {
            enabled: config.enabled,
            killed: AtomicBool::new(config.kill_switch),
            authority_origin_id: config.authority_origin_id,
            authority_https_root: CanonicalPublicHttpsRoot::parse(&config.authority_https_root)?,
            authority_signing_key_id: config.authority_signing_key_id,
            authority_signing_key: config.authority_signing_key,
            revoked_key_ids: config.revoked_key_ids,
            maximum_attempts: config.maximum_attempts,
            authority_retention_seconds,
            limits: config.limits,
            directory,
            transport,
            sink,
            quota,
        })
    }

    pub fn resolve(
        &self,
        principal: &str,
        proxy_request: &FederationProxyRequest,
    ) -> Result<FederationProxyResult, FederationProxyError> {
        if !self.enabled || self.killed.load(Ordering::Acquire) {
            return Err(FederationProxyError::Disabled);
        }
        validate_principal(principal)?;
        proxy_request.validate(&self.limits)?;
        let request_bytes = serde_json::to_vec(&proxy_request.request)
            .map_err(|_| FederationProxyError::InvalidRequest)?;
        if request_bytes.is_empty() || request_bytes.len() as u64 > self.limits.max_manifest_bytes {
            return Err(FederationProxyError::InvalidRequest);
        }
        let now = now_unix();
        let minimum_response_bytes = 1;
        let mut candidates = self
            .directory
            .candidates(&proxy_request.request, minimum_response_bytes)
            .map_err(|_| FederationProxyError::NoCandidate)?;
        normalize_candidates(
            &mut candidates,
            &proxy_request.request,
            &self.authority_origin_id,
            &self.authority_https_root,
            now,
            &self.limits,
        )?;
        if let Some(preferred) = &proxy_request.preferred_origin_id {
            let Some(index) = candidates
                .iter()
                .position(|candidate| &candidate.descriptor.origin_id == preferred)
            else {
                return Err(FederationProxyError::UnapprovedOriginHint);
            };
            candidates[..=index].rotate_right(1);
        }
        candidates.truncate(self.maximum_attempts);
        if candidates.is_empty() {
            return Err(FederationProxyError::NoCandidate);
        }

        // Reserve the entire bounded cost of every permitted attempt before
        // the first transport call. This is deliberately conservative: DNS,
        // timeout, malformed-response, signature, and staging failures do not
        // refund bytes because an origin may already have emitted them. A
        // durable adapter therefore cannot be bypassed by retrying or restart.
        let maximum_upstream_bytes =
            maximum_upstream_reservation(&candidates, request_bytes.len() as u64, &self.limits)?;
        if maximum_upstream_bytes == 0 {
            return Err(FederationProxyError::InvalidConfiguration);
        }
        let _permit = self
            .quota
            .reserve(principal, maximum_upstream_bytes)
            .map_err(|_| FederationProxyError::Quota)?;

        let resolve_request = ResolveObjectRequest {
            schema: RESOLVE_SCHEMA.to_owned(),
            request: proxy_request.request.clone(),
        };
        let identity = request_sha256(&proxy_request.request)?;
        let mut attempts = 0usize;
        for candidate in candidates {
            attempts += 1;
            let fetched = match self
                .transport
                .fetch(&candidate, &resolve_request, &self.limits)
            {
                Ok(fetched) => fetched,
                Err(_) => {
                    let _ = self.directory.record_health(
                        &candidate.descriptor.origin_id,
                        ProxyHealthObservation::Failed,
                    );
                    continue;
                }
            };
            let verified = verify_upstream_object(
                &candidate,
                &proxy_request.request,
                fetched,
                now,
                &self.revoked_key_ids,
                &self.limits,
            );
            let Ok((mut upstream_manifest, encoded_object, upstream_key_expires_unix)) = verified
            else {
                let _ = self.directory.record_health(
                    &candidate.descriptor.origin_id,
                    ProxyHealthObservation::Failed,
                );
                continue;
            };

            let authority_policy_expiry = upstream_manifest
                .manifest
                .created_unix
                .checked_add(self.authority_retention_seconds)
                .ok_or(FederationProxyError::InvalidConfiguration)?;
            upstream_manifest.manifest.expires_unix = upstream_manifest
                .manifest
                .expires_unix
                .min(candidate.descriptor.expires_unix)
                .min(upstream_key_expires_unix)
                .min(authority_policy_expiry);
            if upstream_manifest.manifest.expires_unix <= now
                || upstream_manifest.manifest.expires_unix
                    <= upstream_manifest.manifest.created_unix
            {
                let _ = self.directory.record_health(
                    &candidate.descriptor.origin_id,
                    ProxyHealthObservation::Failed,
                );
                continue;
            }
            let authority_manifest = sign_object_manifest(
                upstream_manifest.manifest,
                self.authority_signing_key_id.clone(),
                &self.authority_signing_key,
            )?;
            self.sink
                .stage(&identity, &authority_manifest, &encoded_object)
                .map_err(|_| FederationProxyError::Stage)?;
            // Health feedback is deliberately best-effort. A durable health
            // state write failure must not discard a fully verified, staged
            // object or make operational fallback less reliable.
            let _ = self.directory.record_health(
                &candidate.descriptor.origin_id,
                ProxyHealthObservation::Healthy,
            );
            return Ok(FederationProxyResult {
                response: ResolveObjectResponse {
                    schema: RESOLVE_SCHEMA.to_owned(),
                    request_sha256: identity,
                    signed_manifest: Some(authority_manifest),
                    delivery_order: vec![DeliverySource::Origin],
                },
                encoded_object,
                public_origin_id: candidate.descriptor.origin_id,
            });
        }
        Err(FederationProxyError::Unavailable { attempts })
    }

    /// Immediate operator stop for all federated upstream transfers. Signed
    /// catalog discovery and the authority's ordinary local/R2/origin path
    /// remain unaffected.
    pub fn set_kill_switch(&self, killed: bool) {
        self.killed.store(killed, Ordering::Release);
    }

    pub fn kill_switch_enabled(&self) -> bool {
        self.killed.load(Ordering::Acquire)
    }
}

fn maximum_upstream_reservation(
    candidates: &[ProxyCandidate],
    request_bytes: u64,
    limits: &ProtocolLimits,
) -> Result<u64, FederationProxyError> {
    candidates.iter().try_fold(0u64, |total, candidate| {
        let maximum_object_bytes = candidate
            .descriptor
            .quotas
            .maximum_response_bytes
            .min(limits.max_encoded_bytes);
        total
            .checked_add(request_bytes)
            .and_then(|value| value.checked_add(limits.max_manifest_bytes))
            .and_then(|value| value.checked_add(maximum_object_bytes))
            .ok_or(FederationProxyError::Quota)
    })
}

fn normalize_candidates(
    candidates: &mut Vec<ProxyCandidate>,
    request: &ShareRequest,
    authority_origin_id: &str,
    authority_root: &CanonicalPublicHttpsRoot,
    now: i64,
    limits: &ProtocolLimits,
) -> Result<(), FederationProxyError> {
    let mut ids = BTreeSet::new();
    let mut roots = BTreeSet::new();
    candidates.retain(|candidate| {
        candidate_is_compatible(
            candidate,
            request,
            authority_origin_id,
            authority_root,
            now,
            limits,
        ) && ids.insert(candidate.descriptor.origin_id.clone())
            && roots.insert(candidate.descriptor.https_base_url.clone())
    });
    candidates.sort_by(|a, b| {
        a.consecutive_failures
            .cmp(&b.consecutive_failures)
            .then_with(|| a.descriptor.origin_id.cmp(&b.descriptor.origin_id))
    });
    Ok(())
}

fn candidate_is_compatible(
    candidate: &ProxyCandidate,
    request: &ShareRequest,
    authority_origin_id: &str,
    authority_root: &CanonicalPublicHttpsRoot,
    now: i64,
    limits: &ProtocolLimits,
) -> bool {
    let descriptor = &candidate.descriptor;
    let Ok(root) = CanonicalPublicHttpsRoot::parse(&descriptor.https_base_url) else {
        return false;
    };
    if descriptor
        .validate(&rw_community_protocol::FederationLimits::default())
        .is_err()
        || descriptor.issued_unix > now
        || descriptor.expires_unix <= now
        || descriptor.origin_id == authority_origin_id
        || root == *authority_root
        || validate_id(&candidate.matched_product, MAX_PRODUCT_BYTES).is_err()
        || descriptor.quotas.maximum_response_bytes == 0
        || serde_json::to_vec(request).map_or(true, |encoded| {
            encoded.is_empty()
                || encoded.len() as u64 > descriptor.quotas.maximum_request_bytes
                || encoded.len() as u64 > limits.max_manifest_bytes
        })
    {
        return false;
    }
    let capability = request_query_capability(&request.query);
    let requested_levels = query_pressure_levels(&request.query);
    let Some(product) = descriptor
        .models
        .iter()
        .find(|model| model.model == request.model)
        .and_then(|model| {
            model
                .products
                .iter()
                .find(|product| product.product == candidate.matched_product)
        })
    else {
        return false;
    };
    if !product.queries.contains(&capability)
        || (!requested_levels.is_empty()
            && !requested_levels
                .iter()
                .all(|level| product.pressure_levels_hpa.contains(level)))
    {
        return false;
    }
    query_is_in_coverage(&request.query, descriptor)
}

fn request_query_capability(query: &ShareQuery) -> FederationQueryCapability {
    match query {
        ShareQuery::Profile { .. } => FederationQueryCapability::Sounding,
        ShareQuery::PointSeries { .. } => FederationQueryCapability::PointSeries,
        ShareQuery::NativeWindow { .. } => FederationQueryCapability::NativeWindow,
        ShareQuery::GeographicWindow { .. } => FederationQueryCapability::ArbitraryDomainMap,
        ShareQuery::TemporalGrid { .. } => FederationQueryCapability::TemporalGrid,
        ShareQuery::CaseArtifact { .. } => FederationQueryCapability::CaseArtifact,
    }
}

fn query_pressure_levels(query: &ShareQuery) -> &[u16] {
    match query {
        ShareQuery::Profile {
            pressure_levels_hpa,
            ..
        }
        | ShareQuery::NativeWindow {
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
        } => pressure_levels_hpa,
        ShareQuery::PointSeries { .. } | ShareQuery::CaseArtifact { .. } => &[],
    }
}

fn query_is_in_coverage(query: &ShareQuery, descriptor: &PublicOriginDescriptor) -> bool {
    match query {
        ShareQuery::Profile {
            latitude_e7,
            longitude_e7,
            ..
        }
        | ShareQuery::PointSeries {
            latitude_e7,
            longitude_e7,
            ..
        } => descriptor.geographic_coverage.iter().any(|area| {
            area.west_longitude_e7 <= *longitude_e7
                && area.east_longitude_e7 >= *longitude_e7
                && area.south_latitude_e7 <= *latitude_e7
                && area.north_latitude_e7 >= *latitude_e7
        }),
        ShareQuery::GeographicWindow {
            west_longitude_e7,
            south_latitude_e7,
            east_longitude_e7,
            north_latitude_e7,
            ..
        } => {
            let segments = if west_longitude_e7 <= east_longitude_e7 {
                vec![(*west_longitude_e7, *east_longitude_e7)]
            } else {
                vec![
                    (*west_longitude_e7, 1_800_000_000),
                    (-1_800_000_000, *east_longitude_e7),
                ]
            };
            segments.into_iter().all(|(west, east)| {
                descriptor.geographic_coverage.iter().any(|area| {
                    area.west_longitude_e7 <= west
                        && area.east_longitude_e7 >= east
                        && area.south_latitude_e7 <= *south_latitude_e7
                        && area.north_latitude_e7 >= *north_latitude_e7
                })
            })
        }
        ShareQuery::NativeWindow { .. }
        | ShareQuery::TemporalGrid { .. }
        | ShareQuery::CaseArtifact { .. } => true,
    }
}

fn verify_upstream_object(
    candidate: &ProxyCandidate,
    request: &ShareRequest,
    upstream: UpstreamObject,
    now: i64,
    revoked_key_ids: &BTreeSet<String>,
    limits: &ProtocolLimits,
) -> Result<(SignedObjectManifest, Vec<u8>, i64), FederationProxyError> {
    let expected_request_sha256 = request_sha256(request)?;
    if upstream.resolve.schema != RESOLVE_SCHEMA
        || upstream.resolve.request_sha256 != expected_request_sha256
        || upstream
            .resolve
            .delivery_order
            .contains(&DeliverySource::CommunityRelay)
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    let signed = upstream
        .resolve
        .signed_manifest
        .ok_or(FederationProxyError::InvalidRequest)?;
    if signed.manifest.request != *request
        || signed.manifest.request_sha256 != expected_request_sha256
        || signed.manifest.encoded_size > candidate.descriptor.quotas.maximum_response_bytes
        || revoked_key_ids.contains(&signed.signature.signing_key_id)
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    let (trusted, upstream_key_expires_unix) = active_object_keys(
        &candidate.descriptor,
        now,
        revoked_key_ids,
        &signed.signature.signing_key_id,
    )?;
    verify_signed_object(
        &signed,
        request,
        &upstream.encoded_object,
        now,
        &trusted,
        limits,
    )?;
    enforce_request_attributions(request, &signed.manifest)?;
    let decoded = decode_object_bounded(&signed, &upstream.encoded_object, limits)?;
    validate_payload_identity(&decoded, request, limits)?;
    Ok((signed, upstream.encoded_object, upstream_key_expires_unix))
}

fn active_object_keys(
    descriptor: &PublicOriginDescriptor,
    now: i64,
    revoked_key_ids: &BTreeSet<String>,
    selected_key_id: &str,
) -> Result<(TrustedSigningKeys, i64), FederationProxyError> {
    let mut trusted = BTreeMap::<String, VerifyingKey>::new();
    let mut selected_expiry = None;
    for key in &descriptor.object_signing_keys {
        if !revoked_key_ids.contains(&key.key_id)
            && key.not_before_unix <= now
            && now < key.expires_unix
        {
            let parsed = parse_verifying_key_base64(&key.public_key_base64)?;
            if trusted.insert(key.key_id.clone(), parsed).is_some() {
                return Err(FederationProxyError::InvalidConfiguration);
            }
            if key.key_id == selected_key_id {
                selected_expiry = Some(key.expires_unix);
            }
        }
    }
    if trusted.is_empty() {
        return Err(FederationProxyError::InvalidConfiguration);
    }
    Ok((
        trusted,
        selected_expiry.ok_or(FederationProxyError::InvalidConfiguration)?,
    ))
}

fn decode_object_bounded(
    signed: &SignedObjectManifest,
    encoded: &[u8],
    limits: &ProtocolLimits,
) -> Result<Vec<u8>, FederationProxyError> {
    let expected = signed.manifest.decoded_size;
    if expected == 0 || expected > limits.max_decoded_bytes {
        return Err(ProtocolError::DecodedSizeLimit.into());
    }
    let reader: Box<dyn Read> = match signed.manifest.compression {
        Compression::None => Box::new(encoded),
        Compression::Gzip => Box::new(flate2::read::GzDecoder::new(encoded)),
        Compression::Zstd => Box::new(
            zstd::stream::read::Decoder::new(encoded)
                .map_err(|_| ProtocolError::DecodedSizeMismatch)?,
        ),
    };
    let mut decoded = Vec::new();
    decoded
        .try_reserve_exact(usize::try_from(expected).map_err(|_| ProtocolError::DecodedSizeLimit)?)
        .map_err(|_| ProtocolError::DecodedSizeLimit)?;
    reader
        .take(expected.saturating_add(1))
        .read_to_end(&mut decoded)
        .map_err(|_| ProtocolError::DecodedSizeMismatch)?;
    if decoded.len() as u64 != expected {
        return Err(ProtocolError::DecodedSizeMismatch.into());
    }
    Ok(decoded)
}

fn validate_payload_identity(
    decoded: &[u8],
    request: &ShareRequest,
    limits: &ProtocolLimits,
) -> Result<(), FederationProxyError> {
    match &request.query {
        ShareQuery::Profile { .. } => {
            let payload: ProfileObjectPayload<ProfileResult> =
                serde_json::from_slice(decoded).map_err(|_| ProtocolError::MalformedJson)?;
            validate_profile_payload_identity(&payload, request)?;
            validate_profile_result(&payload, request)?;
        }
        ShareQuery::PointSeries { .. } => {
            let payload: TypedObjectPayload<PointSeriesResult> =
                parse_typed_payload(decoded, POINT_SERIES_PAYLOAD_SCHEMA, request)?;
            validate_point_series_result(&payload.data, request)?;
        }
        ShareQuery::NativeWindow {
            pressure_levels_hpa,
            ..
        } => {
            if pressure_levels_hpa.is_empty() {
                let payload: TypedObjectPayload<Vec<IndexWindow2DResult>> =
                    parse_typed_payload(decoded, NATIVE_WINDOW_PAYLOAD_SCHEMA, request)?;
                validate_native_window_2d(&payload.data, request)?;
            } else {
                let payload: TypedObjectPayload<Vec<IndexWindow3DResult>> =
                    parse_typed_payload(decoded, NATIVE_WINDOW_PAYLOAD_SCHEMA, request)?;
                validate_native_window_3d(&payload.data, request)?;
            }
        }
        ShareQuery::GeographicWindow { .. } => {
            let payload: TypedObjectPayload<GeographicWindowResult> =
                parse_typed_payload(decoded, GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA, request)?;
            validate_geographic_window_result(&payload.data, request)?;
        }
        ShareQuery::TemporalGrid { .. } => {
            let payload: TypedObjectPayload<TemporalGridResult> =
                parse_typed_payload(decoded, TEMPORAL_GRID_PAYLOAD_SCHEMA, request)?;
            validate_temporal_grid_result(&payload.data, request)?;
        }
        ShareQuery::CaseArtifact { .. } => {
            validate_case_artifact_payload_bytes(decoded, request, limits)?;
            let payload: TypedObjectPayload<serde_json::Value> =
                serde_json::from_slice(decoded).map_err(|_| ProtocolError::MalformedJson)?;
            validate_typed_payload_identity(&payload, CASE_ARTIFACT_PAYLOAD_SCHEMA, request)?;
        }
    }
    Ok(())
}

fn parse_typed_payload<T>(
    decoded: &[u8],
    expected_schema: &'static str,
    request: &ShareRequest,
) -> Result<TypedObjectPayload<T>, FederationProxyError>
where
    T: serde::de::DeserializeOwned,
{
    let payload: TypedObjectPayload<T> =
        serde_json::from_slice(decoded).map_err(|_| ProtocolError::MalformedJson)?;
    validate_typed_payload_identity(&payload, expected_schema, request)?;
    Ok(payload)
}

fn validate_run_identity(
    run: &RunDescriptor,
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let provenance_matches = run.source_provenance.len() == request.source_provenance.len()
        && run
            .source_provenance
            .iter()
            .zip(&request.source_provenance)
            .all(|(actual, expected)| {
                actual.provider == expected.provider
                    && actual.roles == expected.roles
                    && actual.products == expected.products
            });
    if run.model != request.model
        || run.run != request.run
        || run.snapshot_id != request.snapshot_id
        || run.grid_hash != request.grid_hash
        || !provenance_matches
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    Ok(())
}

fn fixed_coordinate_matches(actual: f64, expected_e7: i32) -> bool {
    actual.is_finite() && (actual * 10_000_000.0 - f64::from(expected_e7)).abs() <= 0.5
}

fn validate_profile_result(
    payload: &ProfileObjectPayload<ProfileResult>,
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::Profile {
        latitude_e7,
        longitude_e7,
        storage_slot,
        valid_unix,
        pressure_variables,
        pressure_levels_hpa,
        ..
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    validate_run_identity(&payload.profile.run, request)?;
    if payload.profile.time.storage_slot != *storage_slot
        || payload.profile.time.valid_unix != *valid_unix
        || !fixed_coordinate_matches(payload.profile.point.requested_latitude, *latitude_e7)
        || !fixed_coordinate_matches(payload.profile.point.requested_longitude, *longitude_e7)
        || payload.profile.variables.len() != pressure_variables.len()
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    for (variable, expected_name) in payload.profile.variables.iter().zip(pressure_variables) {
        if variable.name != *expected_name
            || variable.values.len() != variable.levels_hpa.len()
            || variable.expected_levels != variable.levels_hpa.len()
            || variable.available_levels != variable.values.iter().flatten().count()
            || (!pressure_levels_hpa.is_empty() && variable.levels_hpa != *pressure_levels_hpa)
        {
            return Err(FederationProxyError::InvalidRequest);
        }
    }
    Ok(())
}

fn validate_point_series_result(
    result: &PointSeriesResult,
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::PointSeries {
        latitude_e7,
        longitude_e7,
        window,
        ..
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    validate_run_identity(&result.run, request)?;
    let (start, end) = time_window_bounds(window);
    if !fixed_coordinate_matches(result.point.requested_latitude, *latitude_e7)
        || !fixed_coordinate_matches(result.point.requested_longitude, *longitude_e7)
        || result.variables.len() != request.variables.len()
        || result
            .variables
            .iter()
            .zip(&request.variables)
            .any(|(variable, expected)| {
                variable.name != *expected
                    || variable.values.len() != result.axis.len()
                    || variable.expected_samples != result.axis.len()
                    || variable.available_samples != variable.values.iter().flatten().count()
            })
        || result.axis.iter().any(|time| {
            time.valid_unix < start
                || time.valid_unix >= end
                || time.storage_slot as usize >= result.run.sample_count
        })
        || result
            .axis
            .windows(2)
            .any(|pair| pair[0].valid_unix >= pair[1].valid_unix)
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    Ok(())
}

fn validate_native_window_2d(
    windows: &[IndexWindow2DResult],
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::NativeWindow {
        storage_slot,
        valid_unix,
        x0,
        y0,
        x1,
        y1,
        pressure_levels_hpa,
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    if !pressure_levels_hpa.is_empty() || windows.len() != request.variables.len() {
        return Err(FederationProxyError::InvalidRequest);
    }
    let nx = usize::try_from(x1 - x0).map_err(|_| FederationProxyError::InvalidRequest)?;
    let ny = usize::try_from(y1 - y0).map_err(|_| FederationProxyError::InvalidRequest)?;
    let cells = nx
        .checked_mul(ny)
        .ok_or(FederationProxyError::InvalidRequest)?;
    for (window, expected_variable) in windows.iter().zip(&request.variables) {
        validate_run_identity(&window.run, request)?;
        if window.variable != *expected_variable
            || window.time.storage_slot != *storage_slot
            || window.time.valid_unix != *valid_unix
            || window.x0 != *x0 as usize
            || window.y0 != *y0 as usize
            || window.nx != nx
            || window.ny != ny
            || window.values.len() != cells
        {
            return Err(FederationProxyError::InvalidRequest);
        }
    }
    Ok(())
}

fn validate_native_window_3d(
    windows: &[IndexWindow3DResult],
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::NativeWindow {
        storage_slot,
        valid_unix,
        x0,
        y0,
        x1,
        y1,
        pressure_levels_hpa,
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    if pressure_levels_hpa.is_empty() || windows.len() != request.variables.len() {
        return Err(FederationProxyError::InvalidRequest);
    }
    let nx = usize::try_from(x1 - x0).map_err(|_| FederationProxyError::InvalidRequest)?;
    let ny = usize::try_from(y1 - y0).map_err(|_| FederationProxyError::InvalidRequest)?;
    let values = nx
        .checked_mul(ny)
        .and_then(|cells| cells.checked_mul(pressure_levels_hpa.len()))
        .ok_or(FederationProxyError::InvalidRequest)?;
    for (window, expected_variable) in windows.iter().zip(&request.variables) {
        validate_run_identity(&window.run, request)?;
        if window.variable != *expected_variable
            || window.time.storage_slot != *storage_slot
            || window.time.valid_unix != *valid_unix
            || window.levels_hpa != *pressure_levels_hpa
            || window.x0 != *x0 as usize
            || window.y0 != *y0 as usize
            || window.nx != nx
            || window.ny != ny
            || window.values.len() != values
        {
            return Err(FederationProxyError::InvalidRequest);
        }
    }
    Ok(())
}

fn validate_geographic_window_result(
    result: &GeographicWindowResult,
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::GeographicWindow {
        storage_slot,
        valid_unix,
        west_longitude_e7,
        south_latitude_e7,
        east_longitude_e7,
        north_latitude_e7,
        pressure_levels_hpa,
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    validate_run_identity(&result.run, request)?;
    let cells = result
        .envelope
        .nx
        .checked_mul(result.envelope.ny)
        .ok_or(FederationProxyError::InvalidRequest)?;
    if result.time.storage_slot != *storage_slot
        || result.time.valid_unix != *valid_unix
        || !fixed_coordinate_matches(result.requested_bbox.west_longitude, *west_longitude_e7)
        || !fixed_coordinate_matches(result.requested_bbox.south_latitude, *south_latitude_e7)
        || !fixed_coordinate_matches(result.requested_bbox.east_longitude, *east_longitude_e7)
        || !fixed_coordinate_matches(result.requested_bbox.north_latitude, *north_latitude_e7)
        || result.latitudes.len() != cells
        || result.longitudes.len() != cells
        || result.cell_mask.len() != cells
        || result.fields.len() != request.variables.len()
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    for (field, expected_variable) in result.fields.iter().zip(&request.variables) {
        if field.variable != *expected_variable {
            return Err(FederationProxyError::InvalidRequest);
        }
        match &field.data {
            GeographicFieldValues::Surface2d { values }
                if pressure_levels_hpa.is_empty() && values.len() == cells => {}
            GeographicFieldValues::PressureLevels { levels_hpa, values }
                if !pressure_levels_hpa.is_empty()
                    && levels_hpa == pressure_levels_hpa
                    && values.len()
                        == cells
                            .checked_mul(pressure_levels_hpa.len())
                            .ok_or(FederationProxyError::InvalidRequest)? => {}
            _ => return Err(FederationProxyError::InvalidRequest),
        }
    }
    Ok(())
}

fn temporal_metadata(result: &TemporalGridResult) -> &TemporalGridMetadata {
    match result {
        TemporalGridResult::Scalar(value) => &value.metadata,
        TemporalGridResult::Interval(value) => &value.metadata,
        TemporalGridResult::IntervalMaximum(value) => &value.metadata,
        TemporalGridResult::Cumulative(value) => &value.metadata,
        TemporalGridResult::Rate(value) => &value.metadata,
        TemporalGridResult::Vector(value) => &value.metadata,
        TemporalGridResult::Circular(value) => &value.metadata,
        TemporalGridResult::Categorical(value) => &value.metadata,
    }
}

fn validate_temporal_grid_result(
    result: &TemporalGridResult,
    request: &ShareRequest,
) -> Result<(), FederationProxyError> {
    let ShareQuery::TemporalGrid {
        window,
        pressure_levels_hpa,
        ..
    } = &request.query
    else {
        return Err(FederationProxyError::InvalidRequest);
    };
    let metadata = temporal_metadata(result);
    validate_run_identity(&metadata.run, request)?;
    let (start, end) = time_window_bounds(window);
    let cells = metadata
        .nx
        .checked_mul(metadata.ny)
        .and_then(|cells| {
            cells.checked_mul(if pressure_levels_hpa.is_empty() {
                1
            } else {
                pressure_levels_hpa.len()
            })
        })
        .ok_or(FederationProxyError::InvalidRequest)?;
    if metadata.variables != request.variables
        || metadata.levels_hpa != *pressure_levels_hpa
        || metadata.window.start_unix != start
        || metadata.window.end_unix != end
        || metadata.axis.iter().any(|time| {
            time.valid_unix < start
                || time.valid_unix >= end
                || time.storage_slot as usize >= metadata.run.sample_count
        })
        || !temporal_result_lengths_match(result, cells)
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    Ok(())
}

fn temporal_result_lengths_match(result: &TemporalGridResult, cells: usize) -> bool {
    match result {
        TemporalGridResult::Scalar(value) => {
            value.minimum.len() == cells
                && value.maximum.len() == cells
                && value.range.len() == cells
                && value.time_weighted_mean.len() == cells
                && value.argmin_time_index.len() == cells
                && value.argmax_time_index.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Interval(value) => {
            value.total.len() == cells
                && value.minimum_interval.len() == cells
                && value.maximum_interval.len() == cells
                && value.range_interval.len() == cells
                && value.argmin_time_index.len() == cells
                && value.argmax_time_index.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::IntervalMaximum(value) => {
            value.minimum_of_interval_maxima.len() == cells
                && value.maximum_of_interval_maxima.len() == cells
                && value.range_of_interval_maxima.len() == cells
                && value.argmin_interval_maximum_time_index.len() == cells
                && value.argmax_interval_maximum_time_index.len() == cells
                && value.finite_interval_maximum_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Cumulative(value) => {
            value.total_increment.len() == cells
                && value.minimum_increment.len() == cells
                && value.maximum_increment.len() == cells
                && value.range_increment.len() == cells
                && value.argmin_time_index.len() == cells
                && value.argmax_time_index.len() == cells
                && value.finite_increment_count.len() == cells
                && value.reset_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Rate(value) => {
            value.minimum_rate.len() == cells
                && value.maximum_rate.len() == cells
                && value.range_rate.len() == cells
                && value.duration_weighted_mean.len() == cells
                && value.integral.len() == cells
                && value.argmin_time_index.len() == cells
                && value.argmax_time_index.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Vector(value) => {
            value.minimum_speed.len() == cells
                && value.maximum_speed.len() == cells
                && value.range_speed.len() == cells
                && value.time_weighted_mean_speed.len() == cells
                && value.vector_mean_u.len() == cells
                && value.vector_mean_v.len() == cells
                && value.vector_mean_speed.len() == cells
                && value.vector_mean_direction_toward_degrees.len() == cells
                && value.argmin_time_index.len() == cells
                && value.argmax_time_index.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Circular(value) => {
            value.mean_degrees.len() == cells
                && value.resultant_length.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
        TemporalGridResult::Categorical(value) => {
            value.mode.len() == cells
                && value.mode_duration_seconds.len() == cells
                && value.category_durations.len() == cells
                && value.transitions.len() == cells
                && value.finite_count.len() == cells
                && value.covered_duration_seconds.len() == cells
                && value.duration_coverage.len() == cells
        }
    }
}

fn time_window_bounds(window: &rw_community_protocol::TimeWindow) -> (i64, i64) {
    match window {
        rw_community_protocol::TimeWindow::Utc {
            start_unix,
            end_unix,
        } => (*start_unix, *end_unix),
        rw_community_protocol::TimeWindow::LocalDay {
            resolved_start_unix,
            resolved_end_unix,
            ..
        } => (*resolved_start_unix, *resolved_end_unix),
    }
}

fn validate_id(value: &str, maximum: usize) -> Result<(), FederationProxyError> {
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
        return Err(FederationProxyError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_principal(value: &str) -> Result<(), FederationProxyError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return Err(FederationProxyError::InvalidRequest);
    }
    Ok(())
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

// -------------------------------------------------------------------------
// Hardened conventional HTTPS transport

#[derive(Clone, PartialEq, Eq)]
struct CanonicalPublicHttpsRoot {
    raw: String,
    host: String,
    base_path: String,
}

impl fmt::Debug for CanonicalPublicHttpsRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("CanonicalPublicHttpsRoot")
            .field(&self.raw)
            .finish()
    }
}

impl CanonicalPublicHttpsRoot {
    fn parse(value: &str) -> Result<Self, FederationProxyError> {
        parse_https_root(value, false)
    }

    fn endpoint(&self, path: &str) -> Result<String, UpstreamFailure> {
        if path.len() > 512
            || !path.starts_with("/v1/")
            || path.contains(['\\', '?', '#'])
            || path.contains("//")
            || path.to_ascii_lowercase().contains("%2e")
            || path.split('/').any(|part| matches!(part, "." | ".."))
        {
            return Err(UpstreamFailure::InvalidResponse);
        }
        Ok(format!("{}{}", self.raw, path))
    }
}

/// Strict HTTPS root used by the public federation proxy. Private operator
/// gateways are intentionally not admitted here; they require a separate,
/// explicit private-network policy in the ordinary origin/R2 connector.
fn parse_https_root(
    value: &str,
    allow_bounded_base_path: bool,
) -> Result<CanonicalPublicHttpsRoot, FederationProxyError> {
    if value.len() > 512
        || !value.is_ascii()
        || !value.starts_with("https://")
        || value
            .chars()
            .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
        || value.contains(['\\', '@', '?', '#'])
    {
        return Err(FederationProxyError::InvalidConfiguration);
    }
    let remainder = &value[8..];
    let authority_end = remainder.find('/').unwrap_or(remainder.len());
    let host = &remainder[..authority_end];
    let base_path = &remainder[authority_end..];
    if host.is_empty()
        || host.len() > 253
        || host.contains(':')
        || host != host.to_ascii_lowercase()
        || !host.contains('.')
        || host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
        || forbidden_host(host)
        || host.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        })
        || (!allow_bounded_base_path && !base_path.is_empty())
        || (allow_bounded_base_path
            && !base_path.is_empty()
            && (base_path.len() > 160
                || !base_path.starts_with('/')
                || base_path.ends_with('/')
                || base_path.contains("//")
                || base_path.to_ascii_lowercase().contains("%2e")
                || base_path.split('/').any(|part| matches!(part, "." | ".."))))
    {
        return Err(FederationProxyError::InvalidConfiguration);
    }
    Ok(CanonicalPublicHttpsRoot {
        raw: value.to_owned(),
        host: host.to_owned(),
        base_path: base_path.to_owned(),
    })
}

fn forbidden_host(host: &str) -> bool {
    [
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
    ]
    .iter()
    .any(|suffix| host == suffix.trim_start_matches('.') || host.ends_with(suffix))
}

struct BearerSecret(String);

impl BearerSecret {
    fn parse(value: String) -> Result<Self, FederationProxyError> {
        if value.is_empty()
            || value.len() > MAX_SECRET_BYTES
            || value.trim() != value
            || !value.is_ascii()
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        {
            return Err(FederationProxyError::InvalidConfiguration);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for BearerSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

pub struct ScopedOriginAccess {
    origin_id: String,
    root: CanonicalPublicHttpsRoot,
    bearer: Option<BearerSecret>,
}

impl ScopedOriginAccess {
    pub fn new(
        origin_id: impl Into<String>,
        https_root: impl AsRef<str>,
        bearer: Option<String>,
    ) -> Result<Self, FederationProxyError> {
        let origin_id = origin_id.into();
        validate_id(&origin_id, MAX_ORIGIN_ID_BYTES)?;
        Ok(Self {
            origin_id,
            root: CanonicalPublicHttpsRoot::parse(https_root.as_ref())?,
            bearer: bearer.map(BearerSecret::parse).transpose()?,
        })
    }

    /// Load one origin-scoped data credential from a permission-restricted
    /// regular file. This deliberately does not reuse federation health
    /// credentials and never accepts inline config secret material.
    pub fn from_token_file(
        origin_id: impl Into<String>,
        https_root: impl AsRef<str>,
        bearer_token_file: Option<&Path>,
    ) -> Result<Self, FederationProxyError> {
        Self::new(
            origin_id,
            https_root,
            bearer_token_file.map(read_secret_file).transpose()?,
        )
    }
}

impl fmt::Debug for ScopedOriginAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScopedOriginAccess")
            .field("origin_id", &self.origin_id)
            .field("root", &self.root)
            .field("bearer", &self.bearer)
            .finish()
    }
}

fn read_secret_file(path: &Path) -> Result<String, FederationProxyError> {
    let metadata =
        std::fs::symlink_metadata(path).map_err(|_| FederationProxyError::InvalidConfiguration)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_SECRET_BYTES as u64
    {
        return Err(FederationProxyError::InvalidConfiguration);
    }
    validate_secret_permissions(&metadata)?;
    let value =
        std::fs::read_to_string(path).map_err(|_| FederationProxyError::InvalidConfiguration)?;
    let value = value.trim().to_owned();
    BearerSecret::parse(value.clone())?;
    Ok(value)
}

#[cfg(unix)]
fn validate_secret_permissions(metadata: &std::fs::Metadata) -> Result<(), FederationProxyError> {
    use std::os::unix::fs::PermissionsExt as _;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(FederationProxyError::InvalidConfiguration);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_secret_permissions(_metadata: &std::fs::Metadata) -> Result<(), FederationProxyError> {
    // The Windows installer/doctor enforces the service-identity + SYSTEM ACL;
    // std has no portable ACL inspection API.
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct HttpsTransportTimeouts {
    pub resolve: Duration,
    pub connect: Duration,
    pub send: Duration,
    pub receive: Duration,
    pub global: Duration,
}

impl Default for HttpsTransportTimeouts {
    fn default() -> Self {
        Self {
            resolve: Duration::from_secs(2),
            connect: Duration::from_secs(4),
            send: Duration::from_secs(5),
            receive: Duration::from_secs(20),
            global: Duration::from_secs(30),
        }
    }
}

pub struct HardenedHttpsTransport {
    origins: BTreeMap<String, ScopedOriginAccess>,
    timeouts: HttpsTransportTimeouts,
    dns: BoundedDnsPool,
}

impl fmt::Debug for HardenedHttpsTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HardenedHttpsTransport")
            .field("origin_ids", &self.origins.keys().collect::<Vec<_>>())
            .field("timeouts", &self.timeouts)
            .finish_non_exhaustive()
    }
}

impl HardenedHttpsTransport {
    pub fn new(
        origins: Vec<ScopedOriginAccess>,
        timeouts: HttpsTransportTimeouts,
    ) -> Result<Self, FederationProxyError> {
        if origins.len() > MAX_SCOPED_ORIGINS
            || timeouts.resolve.is_zero()
            || timeouts.connect.is_zero()
            || timeouts.send.is_zero()
            || timeouts.receive.is_zero()
            || timeouts.global.is_zero()
            || timeouts.global < timeouts.resolve
            || timeouts.global < timeouts.connect
            || timeouts.global < timeouts.send
            || timeouts.global < timeouts.receive
        {
            return Err(FederationProxyError::InvalidConfiguration);
        }
        let mut by_id = BTreeMap::new();
        for origin in origins {
            if by_id.insert(origin.origin_id.clone(), origin).is_some() {
                return Err(FederationProxyError::InvalidConfiguration);
            }
        }
        Ok(Self {
            origins: by_id,
            timeouts,
            dns: BoundedDnsPool::new(4),
        })
    }

    fn access_for<'a>(
        &'a self,
        candidate: &ProxyCandidate,
    ) -> Result<&'a ScopedOriginAccess, UpstreamFailure> {
        let access = self
            .origins
            .get(&candidate.descriptor.origin_id)
            .ok_or(UpstreamFailure::Unavailable)?;
        if access.root.raw != candidate.descriptor.https_base_url {
            return Err(UpstreamFailure::Unavailable);
        }
        Ok(access)
    }

    fn agent(&self) -> (ureq::Agent, SafePinnedResolver) {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let resolver = SafePinnedResolver {
            rejected: Arc::new(Mutex::new(false)),
            dns: self.dns.clone(),
        };
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .proxy(None)
            .max_redirects(0)
            .max_idle_connections(0)
            .timeout_global(Some(self.timeouts.global))
            .timeout_per_call(Some(self.timeouts.global))
            .timeout_resolve(Some(self.timeouts.resolve))
            .timeout_connect(Some(self.timeouts.connect))
            .timeout_send_request(Some(self.timeouts.send))
            .timeout_send_body(Some(self.timeouts.send))
            .timeout_recv_response(Some(self.timeouts.receive))
            .timeout_recv_body(Some(self.timeouts.receive))
            .tls_config(
                ureq::tls::TlsConfig::builder()
                    .provider(ureq::tls::TlsProvider::Rustls)
                    .root_certs(ureq::tls::RootCerts::WebPki)
                    .unversioned_rustls_crypto_provider(Arc::new(
                        rustls::crypto::ring::default_provider(),
                    ))
                    .build(),
            )
            .build();
        let connector = ().chain(TcpConnector::default()).chain(RustlsConnector::default());
        (
            ureq::Agent::with_parts(config, connector, resolver.clone()),
            resolver,
        )
    }

    fn classify_response_status(
        status: u16,
        location_present: bool,
    ) -> Result<(), UpstreamFailure> {
        if (300..400).contains(&status) || location_present {
            return Err(UpstreamFailure::RedirectRejected);
        }
        match status {
            200..=299 => Ok(()),
            404 => Err(UpstreamFailure::NotFound),
            _ => Err(UpstreamFailure::Unavailable),
        }
    }

    fn send_resolve(
        &self,
        access: &ScopedOriginAccess,
        request: &ResolveObjectRequest,
        limits: &ProtocolLimits,
    ) -> Result<ResolveObjectResponse, UpstreamFailure> {
        let bytes = serde_json::to_vec(request).map_err(|_| UpstreamFailure::InvalidResponse)?;
        if bytes.is_empty() || bytes.len() as u64 > limits.max_manifest_bytes {
            return Err(UpstreamFailure::InvalidResponse);
        }
        let (agent, resolver) = self.agent();
        let mut call = agent
            .post(&access.root.endpoint(FEDERATION_LOCAL_RESOLVE_PATH)?)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header(FEDERATION_HOP_HEADER, "1")
            .header("user-agent", "rusty-weather-federation-proxy/1");
        if let Some(bearer) = &access.bearer {
            call = call.header("authorization", format!("Bearer {}", bearer.0));
        }
        let mut response = call
            .send(&bytes)
            .map_err(|error| classify_ureq_error(error, &resolver))?;
        Self::classify_response_status(
            response.status().as_u16(),
            response.headers().contains_key("location"),
        )?;
        require_content_type(&response, "application/json")?;
        let response_bytes = read_ureq_body_bounded(&mut response, limits.max_manifest_bytes)?;
        serde_json::from_slice(&response_bytes).map_err(|_| UpstreamFailure::InvalidResponse)
    }

    fn get_object(
        &self,
        access: &ScopedOriginAccess,
        object_sha256: &str,
        maximum: u64,
    ) -> Result<Vec<u8>, UpstreamFailure> {
        if !is_sha256(object_sha256) || maximum == 0 {
            return Err(UpstreamFailure::InvalidResponse);
        }
        let path = format!("{FEDERATION_LOCAL_OBJECT_PATH_PREFIX}/{object_sha256}");
        let (agent, resolver) = self.agent();
        let mut call = agent
            .get(&access.root.endpoint(&path)?)
            .header("accept", "application/octet-stream")
            .header("user-agent", "rusty-weather-federation-proxy/1");
        if let Some(bearer) = &access.bearer {
            call = call.header("authorization", format!("Bearer {}", bearer.0));
        }
        let mut response = call
            .call()
            .map_err(|error| classify_ureq_error(error, &resolver))?;
        Self::classify_response_status(
            response.status().as_u16(),
            response.headers().contains_key("location"),
        )?;
        require_content_type(&response, "application/octet-stream")?;
        read_ureq_body_bounded(&mut response, maximum)
    }
}

impl FederatedOriginTransport for HardenedHttpsTransport {
    fn fetch(
        &self,
        candidate: &ProxyCandidate,
        request: &ResolveObjectRequest,
        limits: &ProtocolLimits,
    ) -> Result<UpstreamObject, UpstreamFailure> {
        let access = self.access_for(candidate)?;
        let resolve = self.send_resolve(access, request, limits)?;
        let manifest = resolve
            .signed_manifest
            .as_ref()
            .ok_or(UpstreamFailure::NotFound)?;
        let maximum = manifest
            .manifest
            .encoded_size
            .min(candidate.descriptor.quotas.maximum_response_bytes)
            .min(limits.max_encoded_bytes);
        if maximum == 0 || manifest.manifest.encoded_size != maximum {
            return Err(UpstreamFailure::InvalidResponse);
        }
        let encoded_object = self.get_object(access, &manifest.manifest.object_sha256, maximum)?;
        Ok(UpstreamObject {
            resolve,
            encoded_object,
        })
    }
}

fn require_content_type(
    response: &ureq::http::Response<ureq::Body>,
    expected: &str,
) -> Result<(), UpstreamFailure> {
    if response.headers().contains_key("content-encoding") {
        return Err(UpstreamFailure::InvalidResponse);
    }
    let actual = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .trim();
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(UpstreamFailure::InvalidResponse)
    }
}

fn read_ureq_body_bounded(
    response: &mut ureq::http::Response<ureq::Body>,
    maximum: u64,
) -> Result<Vec<u8>, UpstreamFailure> {
    if maximum == 0 {
        return Err(UpstreamFailure::InvalidResponse);
    }
    if let Some(value) = response.headers().get("content-length") {
        let length = value
            .to_str()
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .ok_or(UpstreamFailure::InvalidResponse)?;
        if length == 0 || length > maximum {
            return Err(UpstreamFailure::InvalidResponse);
        }
    }
    let mut reader = response.body_mut().as_reader();
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; HTTP_READ_CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|_| UpstreamFailure::Unavailable)?;
        if read == 0 {
            break;
        }
        bytes
            .try_reserve(read)
            .map_err(|_| UpstreamFailure::InvalidResponse)?;
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > maximum {
            return Err(UpstreamFailure::InvalidResponse);
        }
    }
    if bytes.is_empty() {
        Err(UpstreamFailure::InvalidResponse)
    } else {
        Ok(bytes)
    }
}

fn classify_ureq_error(error: ureq::Error, resolver: &SafePinnedResolver) -> UpstreamFailure {
    if resolver.rejected() {
        return UpstreamFailure::DnsRejected;
    }
    match error {
        ureq::Error::TooManyRedirects => UpstreamFailure::RedirectRejected,
        ureq::Error::StatusCode(404) => UpstreamFailure::NotFound,
        _ => UpstreamFailure::Unavailable,
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Clone)]
struct BoundedDnsPool {
    senders: Arc<Vec<mpsc::SyncSender<DnsJob>>>,
    cursor: Arc<AtomicUsize>,
}

impl fmt::Debug for BoundedDnsPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BoundedDnsPool")
            .field("workers", &self.senders.len())
            .finish()
    }
}

struct DnsJob {
    lookup: String,
    response: mpsc::SyncSender<std::io::Result<Vec<SocketAddr>>>,
}

impl BoundedDnsPool {
    fn new(workers: usize) -> Self {
        let mut senders = Vec::new();
        for index in 0..workers {
            let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
            let spawned = thread::Builder::new()
                .name(format!("rw-federation-proxy-dns-{index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let result = job.lookup.to_socket_addrs().map(|answers| {
                            answers
                                .take(MAX_DNS_ANSWERS.saturating_add(1))
                                .collect::<Vec<_>>()
                        });
                        let _ = job.response.send(result);
                    }
                });
            if spawned.is_ok() {
                senders.push(sender);
            }
        }
        Self {
            senders: Arc::new(senders),
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn resolve(
        &self,
        lookup: String,
        timeout: Duration,
    ) -> Result<Vec<SocketAddr>, UpstreamFailure> {
        if self.senders.is_empty() {
            return Err(UpstreamFailure::DnsRejected);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        let mut job = Some(DnsJob { lookup, response });
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        for offset in 0..self.senders.len() {
            let index = (start + offset) % self.senders.len();
            match self.senders[index].try_send(job.take().expect("DNS job retained")) {
                Ok(()) => {
                    return receiver
                        .recv_timeout(timeout)
                        .ok()
                        .and_then(Result::ok)
                        .ok_or(UpstreamFailure::DnsRejected);
                }
                Err(mpsc::TrySendError::Full(returned))
                | Err(mpsc::TrySendError::Disconnected(returned)) => job = Some(returned),
            }
        }
        Err(UpstreamFailure::DnsRejected)
    }
}

#[derive(Debug, Clone)]
struct SafePinnedResolver {
    rejected: Arc<Mutex<bool>>,
    dns: BoundedDnsPool,
}

impl SafePinnedResolver {
    fn rejected(&self) -> bool {
        self.rejected.lock().map(|value| *value).unwrap_or(true)
    }

    fn reject(&self) -> ureq::Error {
        if let Ok(mut rejected) = self.rejected.lock() {
            *rejected = true;
        }
        ureq::Error::HostNotFound
    }
}

impl Resolver for SafePinnedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if let Ok(mut rejected) = self.rejected.lock() {
            *rejected = false;
        }
        if uri.scheme_str() != Some("https") {
            return Err(self.reject());
        }
        let host = uri.host().ok_or_else(|| self.reject())?;
        let port = uri.port_u16().unwrap_or(443);
        let answers = self
            .dns
            .resolve(format!("{host}:{port}"), *timeout.after)
            .map_err(|_| self.reject())?;
        let selected = validate_and_pin_dns_answers(answers).map_err(|_| self.reject())?;
        let mut result = self.empty();
        result.push(selected);
        Ok(result)
    }
}

fn validate_and_pin_dns_answers(
    mut addresses: Vec<SocketAddr>,
) -> Result<SocketAddr, UpstreamFailure> {
    if addresses.is_empty()
        || addresses.len() > MAX_DNS_ANSWERS
        || addresses.iter().any(|address| !is_global_ip(address.ip()))
    {
        return Err(UpstreamFailure::DnsRejected);
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses
        .into_iter()
        .next()
        .ok_or(UpstreamFailure::DnsRejected)
}

fn is_global_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_global_ipv4(address),
        IpAddr::V6(address) => is_global_ipv6(address),
    }
}

fn is_global_ipv4(address: Ipv4Addr) -> bool {
    let value = u32::from(address);
    ![
        (u32::from(Ipv4Addr::new(0, 0, 0, 0)), 8),
        (u32::from(Ipv4Addr::new(10, 0, 0, 0)), 8),
        (u32::from(Ipv4Addr::new(100, 64, 0, 0)), 10),
        (u32::from(Ipv4Addr::new(127, 0, 0, 0)), 8),
        (u32::from(Ipv4Addr::new(169, 254, 0, 0)), 16),
        (u32::from(Ipv4Addr::new(172, 16, 0, 0)), 12),
        (u32::from(Ipv4Addr::new(192, 0, 0, 0)), 24),
        (u32::from(Ipv4Addr::new(192, 0, 2, 0)), 24),
        (u32::from(Ipv4Addr::new(192, 88, 99, 0)), 24),
        (u32::from(Ipv4Addr::new(192, 168, 0, 0)), 16),
        (u32::from(Ipv4Addr::new(198, 18, 0, 0)), 15),
        (u32::from(Ipv4Addr::new(198, 51, 100, 0)), 24),
        (u32::from(Ipv4Addr::new(203, 0, 113, 0)), 24),
        (u32::from(Ipv4Addr::new(224, 0, 0, 0)), 3),
    ]
    .iter()
    .any(|(network, prefix)| in_ipv4_prefix(value, *network, *prefix))
}

fn in_ipv4_prefix(value: u32, network: u32, prefix: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    value & mask == network & mask
}

fn is_global_ipv6(address: Ipv6Addr) -> bool {
    let value = u128::from(address);
    in_ipv6_prefix(
        value,
        u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
        3,
    ) && ![
        (u128::from(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0)), 23),
        (u128::from(Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0)), 48),
        (
            u128::from(Ipv6Addr::new(0x2001, 0x10, 0, 0, 0, 0, 0, 0)),
            28,
        ),
        (
            u128::from(Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0)),
            28,
        ),
        (
            u128::from(Ipv6Addr::new(0x2001, 0x0db8, 0, 0, 0, 0, 0, 0)),
            32,
        ),
        (u128::from(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0)), 16),
        (u128::from(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0)), 20),
    ]
    .iter()
    .any(|(network, prefix)| in_ipv6_prefix(value, *network, *prefix))
}

fn in_ipv6_prefix(value: u128, network: u128, prefix: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
    value & mask == network & mask
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use rw_community_protocol::{
        DataOrigin, FederationCoverageArea, FederationModelCapability, FederationPolicyLinks,
        FederationProductCapability, FederationPublicKey, FederationQuotaSummary,
        FederationReplicationPolicy, FederationRetentionSummary, GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA,
        OBJECT_SCHEMA, ObjectManifest, PublicationGrant, REQUEST_SCHEMA, RecipeIdentity,
        SignatureAlgorithm, SourceProvenance, object_sha256,
    };
    use serde_json::json;

    fn hash(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn federation_key(key_id: &str, signing_key: &SigningKey, now: i64) -> FederationPublicKey {
        FederationPublicKey {
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: key_id.to_owned(),
            public_key_base64: base64::engine::general_purpose::STANDARD
                .encode(signing_key.verifying_key().as_bytes()),
            not_before_unix: now - 60,
            expires_unix: now + 3_600,
        }
    }

    fn request() -> ShareRequest {
        ShareRequest {
            schema: REQUEST_SCHEMA.to_owned(),
            model: "hrrr".to_owned(),
            run: "20260812T00Z".to_owned(),
            snapshot_id: hash('a'),
            grid_hash: hash('b'),
            variables: vec!["temperature".to_owned()],
            query: ShareQuery::GeographicWindow {
                storage_slot: 1,
                valid_unix: 1_786_496_400,
                west_longitude_e7: -1_200_000_000,
                south_latitude_e7: 300_000_000,
                east_longitude_e7: -800_000_000,
                north_latitude_e7: 500_000_000,
                pressure_levels_hpa: vec![500],
            },
            recipe: RecipeIdentity {
                recipe_id: "native-geographic-window".to_owned(),
                recipe_version: "1".to_owned(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".to_owned(),
                roles: vec!["analysis".to_owned()],
                products: vec!["hrrr".to_owned()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
    }

    fn descriptor(
        origin_id: &str,
        root: &str,
        object_keys: Vec<FederationPublicKey>,
        now: i64,
    ) -> PublicOriginDescriptor {
        let mut descriptor = PublicOriginDescriptor {
            schema: rw_community_protocol::FEDERATION_ORIGIN_SCHEMA.to_owned(),
            origin_id: origin_id.to_owned(),
            display_name: format!("{origin_id} public weather lab"),
            https_base_url: root.to_owned(),
            health_path: "/v1/health/ready".to_owned(),
            descriptor_signing_keys: object_keys.clone(),
            object_signing_keys: object_keys,
            models: vec![FederationModelCapability {
                model: "hrrr".to_owned(),
                products: vec![FederationProductCapability {
                    product: "native".to_owned(),
                    queries: vec![FederationQueryCapability::ArbitraryDomainMap],
                    pressure_levels_hpa: vec![500, 700, 850],
                }],
            }],
            geographic_coverage: vec![FederationCoverageArea {
                coverage_id: "conus".to_owned(),
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
            api_schema_version: "rw-api-v1".to_owned(),
            build_version: "test".to_owned(),
            issued_unix: now - 30,
            expires_unix: now + 1_800,
            policy_links: FederationPolicyLinks {
                attribution_url: format!("{root}/attribution"),
                acceptable_use_url: format!("{root}/acceptable-use"),
                privacy_url: format!("{root}/privacy"),
            },
            replication: FederationReplicationPolicy {
                accepts_replication: false,
                maximum_object_bytes: 0,
                monthly_ingress_bytes: 0,
                models: vec![],
            },
            quotas: FederationQuotaSummary {
                maximum_request_bytes: 1024 * 1024,
                maximum_response_bytes: 64 * 1024 * 1024,
                requests_per_minute: 120,
                concurrent_requests: 8,
                monthly_egress_bytes: 10 * 1024 * 1024 * 1024,
            },
        };
        descriptor.normalize();
        descriptor
    }

    fn candidate(
        origin_id: &str,
        root: &str,
        object_keys: Vec<FederationPublicKey>,
        failures: u32,
        now: i64,
    ) -> ProxyCandidate {
        ProxyCandidate {
            descriptor: descriptor(origin_id, root, object_keys, now),
            matched_product: "native".to_owned(),
            consecutive_failures: failures,
        }
    }

    fn payload(request: &ShareRequest) -> Vec<u8> {
        let run = json!({
            "model": request.model,
            "run": request.run,
            "schema": "rw.store.run.v1",
            "snapshot_id": request.snapshot_id,
            "grid_hash": request.grid_hash,
            "nx": 2,
            "ny": 2,
            "exact_time_axis": true,
            "origin_unix": 1_786_492_800_i64,
            "sample_count": 2,
            "first_valid_unix": 1_786_492_800_i64,
            "last_valid_unix": 1_786_496_400_i64,
            "source_provenance": request.source_provenance,
            "provider_attributions": []
        });
        serde_json::to_vec(&TypedObjectPayload {
            schema: GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA.to_owned(),
            request_sha256: request_sha256(request).unwrap(),
            data: json!({
                "schema": "rw.query.geographic-window.v1",
                "run": run,
                "time": {
                    "storage_slot": 1,
                    "lead_seconds": 3_600,
                    "valid_unix": 1_786_496_400_i64
                },
                "requested_bbox": {
                    "west_longitude": -120.0,
                    "south_latitude": 30.0,
                    "east_longitude": -80.0,
                    "north_latitude": 50.0
                },
                "longitude_arc": "ordinary",
                "envelope_semantics": "minimal_native_rectangular_envelope",
                "cell_inclusion_semantics": "grid_point_center_within_closed_bbox",
                "envelope": {"x0": 0, "y0": 0, "nx": 2, "ny": 2},
                "latitudes": [30.0, 30.0, 31.0, 31.0],
                "longitudes": [-100.0, -99.0, -100.0, -99.0],
                "cell_mask": [true, true, true, true],
                "mask_required": false,
                "projection": null,
                "fields": [{
                    "variable": "temperature",
                    "units": "K",
                    "selector": {},
                    "data": {
                        "kind": "pressure_levels",
                        "levels_hpa": [500],
                        "values": [1.0, 2.0, 3.0, 4.0]
                    }
                }]
            }),
        })
        .unwrap()
    }

    fn upstream(
        request: &ShareRequest,
        signing_key_id: &str,
        signing_key: &SigningKey,
        now: i64,
    ) -> UpstreamObject {
        let bytes = payload(request);
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.to_owned(),
            request: request.clone(),
            request_sha256: request_sha256(request).unwrap(),
            object_sha256: object_sha256(&bytes),
            content_type: "application/json".to_owned(),
            compression: Compression::None,
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            attributions: vec![],
            modification_notices: vec!["Verified federated public-origin result.".to_owned()],
            created_unix: now - 10,
            expires_unix: now + 600,
        };
        let signed = sign_object_manifest(manifest, signing_key_id, signing_key).unwrap();
        UpstreamObject {
            resolve: ResolveObjectResponse {
                schema: RESOLVE_SCHEMA.to_owned(),
                request_sha256: request_sha256(request).unwrap(),
                signed_manifest: Some(signed),
                delivery_order: vec![DeliverySource::Origin],
            },
            encoded_object: bytes,
        }
    }

    #[derive(Clone)]
    struct MockDirectory {
        candidates: Arc<Vec<ProxyCandidate>>,
        observations: Arc<Mutex<Vec<(String, ProxyHealthObservation)>>>,
    }

    impl MockDirectory {
        fn new(candidates: Vec<ProxyCandidate>) -> Self {
            Self {
                candidates: Arc::new(candidates),
                observations: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl VerifiedFederationDirectory for MockDirectory {
        fn candidates(
            &self,
            _request: &ShareRequest,
            _minimum_response_bytes: u64,
        ) -> Result<Vec<ProxyCandidate>, DirectoryUnavailable> {
            Ok(self.candidates.as_ref().clone())
        }

        fn record_health(
            &self,
            origin_id: &str,
            observation: ProxyHealthObservation,
        ) -> Result<(), DirectoryUnavailable> {
            self.observations
                .lock()
                .unwrap()
                .push((origin_id.to_owned(), observation));
            Ok(())
        }
    }

    #[derive(Clone)]
    struct MockTransport {
        responses: Arc<Mutex<BTreeMap<String, Result<UpstreamObject, UpstreamFailure>>>>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl MockTransport {
        fn new(
            responses: impl IntoIterator<Item = (String, Result<UpstreamObject, UpstreamFailure>)>,
        ) -> Self {
            Self {
                responses: Arc::new(Mutex::new(responses.into_iter().collect())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl FederatedOriginTransport for MockTransport {
        fn fetch(
            &self,
            candidate: &ProxyCandidate,
            _request: &ResolveObjectRequest,
            _limits: &ProtocolLimits,
        ) -> Result<UpstreamObject, UpstreamFailure> {
            self.calls
                .lock()
                .unwrap()
                .push(candidate.descriptor.origin_id.clone());
            self.responses
                .lock()
                .unwrap()
                .remove(&candidate.descriptor.origin_id)
                .unwrap_or(Err(UpstreamFailure::Unavailable))
        }
    }

    type StagedObject = (String, SignedObjectManifest, Vec<u8>);

    #[derive(Clone, Default)]
    struct MockSink(Arc<Mutex<Vec<StagedObject>>>);

    impl VerifiedObjectSink for MockSink {
        fn stage(
            &self,
            identity: &str,
            manifest: &SignedObjectManifest,
            encoded_object: &[u8],
        ) -> Result<(), StageFailure> {
            self.0.lock().unwrap().push((
                identity.to_owned(),
                manifest.clone(),
                encoded_object.to_vec(),
            ));
            Ok(())
        }
    }

    #[derive(Debug, Default)]
    struct MockQuotaState {
        used: u64,
        active: usize,
        reservations: Vec<u64>,
    }

    #[derive(Clone)]
    struct MockQuota {
        maximum: u64,
        state: Arc<Mutex<MockQuotaState>>,
    }

    struct MockQuotaPermit(Arc<Mutex<MockQuotaState>>);

    impl Drop for MockQuotaPermit {
        fn drop(&mut self) {
            let mut state = self.0.lock().unwrap();
            state.active = state.active.saturating_sub(1);
        }
    }

    impl MockQuota {
        fn new(maximum: u64) -> Self {
            Self {
                maximum,
                state: Arc::new(Mutex::new(MockQuotaState::default())),
            }
        }
    }

    impl FederationProxyQuota for MockQuota {
        type Permit = MockQuotaPermit;

        fn reserve(
            &self,
            _principal: &str,
            maximum_upstream_bytes: u64,
        ) -> Result<Self::Permit, QuotaUnavailable> {
            let mut state = self.state.lock().unwrap();
            let next = state
                .used
                .checked_add(maximum_upstream_bytes)
                .ok_or(QuotaUnavailable)?;
            if next > self.maximum || state.active != 0 {
                return Err(QuotaUnavailable);
            }
            state.used = next;
            state.active += 1;
            state.reservations.push(maximum_upstream_bytes);
            drop(state);
            Ok(MockQuotaPermit(self.state.clone()))
        }
    }

    fn proxy<T: FederatedOriginTransport>(
        directory: MockDirectory,
        transport: T,
        sink: MockSink,
        revoked_key_ids: BTreeSet<String>,
        authority_key: SigningKey,
    ) -> FederationProxy<MockDirectory, T, MockSink, NoopQuota> {
        proxy_with_retention(
            directory,
            transport,
            sink,
            revoked_key_ids,
            authority_key,
            7 * 24 * 60 * 60,
        )
    }

    fn proxy_with_retention<T: FederatedOriginTransport>(
        directory: MockDirectory,
        transport: T,
        sink: MockSink,
        revoked_key_ids: BTreeSet<String>,
        authority_key: SigningKey,
        authority_retention_seconds: u64,
    ) -> FederationProxy<MockDirectory, T, MockSink, NoopQuota> {
        FederationProxy::new(
            FederationProxyConfig {
                enabled: true,
                kill_switch: false,
                authority_origin_id: "hetzner-authority".to_owned(),
                authority_https_root: "https://weather.fahrenheitresearch.com".to_owned(),
                authority_signing_key_id: "authority-object-v1".to_owned(),
                authority_signing_key: authority_key,
                revoked_key_ids,
                maximum_attempts: 8,
                authority_retention_seconds,
                limits: ProtocolLimits::default(),
            },
            directory,
            transport,
            sink,
            NoopQuota,
        )
        .unwrap()
    }

    fn proxy_request(request: ShareRequest) -> FederationProxyRequest {
        FederationProxyRequest {
            schema: FEDERATION_PROXY_SCHEMA.to_owned(),
            request,
            preferred_origin_id: None,
        }
    }

    #[test]
    fn two_origin_failover_rejects_wrong_key_then_authority_signs_and_stages() {
        let now = now_unix();
        let alpha_key = key(1);
        let beta_key = key(2);
        let attacker_key = key(3);
        let authority_key = key(4);
        let request = request();
        let alpha = candidate(
            "alpha-lab",
            "https://alpha.weather.edu",
            vec![federation_key("alpha-object", &alpha_key, now)],
            0,
            now,
        );
        let beta = candidate(
            "beta-lab",
            "https://beta.weather.edu",
            vec![federation_key("beta-object", &beta_key, now)],
            0,
            now,
        );
        let directory = MockDirectory::new(vec![beta, alpha]);
        let transport = MockTransport::new([
            (
                "alpha-lab".to_owned(),
                Ok(upstream(&request, "attacker-object", &attacker_key, now)),
            ),
            (
                "beta-lab".to_owned(),
                Ok(upstream(&request, "beta-object", &beta_key, now)),
            ),
        ]);
        let sink = MockSink::default();
        let service = proxy(
            directory.clone(),
            transport.clone(),
            sink.clone(),
            BTreeSet::new(),
            authority_key.clone(),
        );
        let result = service
            .resolve(&hash('f'), &proxy_request(request.clone()))
            .unwrap();
        assert_eq!(result.public_origin_id, "beta-lab");
        assert_eq!(
            transport.calls.lock().unwrap().as_slice(),
            ["alpha-lab", "beta-lab"]
        );
        assert_eq!(
            directory.observations.lock().unwrap().as_slice(),
            [
                ("alpha-lab".to_owned(), ProxyHealthObservation::Failed),
                ("beta-lab".to_owned(), ProxyHealthObservation::Healthy),
            ]
        );
        let authority_manifest = result.response.signed_manifest.as_ref().unwrap();
        let authority_keys = BTreeMap::from([(
            "authority-object-v1".to_owned(),
            authority_key.verifying_key(),
        )]);
        verify_signed_object(
            authority_manifest,
            &request,
            &result.encoded_object,
            now,
            &authority_keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn quota_is_reserved_before_transport_and_failed_attempts_stay_charged() {
        let now = now_unix();
        let origin_key = key(5);
        let request = request();
        let candidate = candidate(
            "origin-lab",
            "https://origin.weather.edu",
            vec![federation_key("origin-object", &origin_key, now)],
            0,
            now,
        );
        let limits = ProtocolLimits::default();
        let request_bytes = serde_json::to_vec(&request).unwrap().len() as u64;
        let reserved =
            maximum_upstream_reservation(std::slice::from_ref(&candidate), request_bytes, &limits)
                .unwrap();

        let blocked_transport =
            MockTransport::new([("origin-lab".to_owned(), Err(UpstreamFailure::Unavailable))]);
        let blocked = FederationProxy::new(
            FederationProxyConfig {
                enabled: true,
                kill_switch: false,
                authority_origin_id: "hetzner-authority".to_owned(),
                authority_https_root: "https://weather.fahrenheitresearch.com".to_owned(),
                authority_signing_key_id: "authority-object-v1".to_owned(),
                authority_signing_key: key(6),
                revoked_key_ids: BTreeSet::new(),
                maximum_attempts: 1,
                authority_retention_seconds: 60,
                limits,
            },
            MockDirectory::new(vec![candidate.clone()]),
            blocked_transport.clone(),
            MockSink::default(),
            MockQuota::new(reserved - 1),
        )
        .unwrap();
        assert!(matches!(
            blocked.resolve(&hash('f'), &proxy_request(request.clone())),
            Err(FederationProxyError::Quota)
        ));
        assert!(blocked_transport.calls.lock().unwrap().is_empty());

        let failed_transport =
            MockTransport::new([("origin-lab".to_owned(), Err(UpstreamFailure::Unavailable))]);
        let quota = MockQuota::new(reserved);
        let service = FederationProxy::new(
            FederationProxyConfig {
                enabled: true,
                kill_switch: false,
                authority_origin_id: "hetzner-authority".to_owned(),
                authority_https_root: "https://weather.fahrenheitresearch.com".to_owned(),
                authority_signing_key_id: "authority-object-v1".to_owned(),
                authority_signing_key: key(7),
                revoked_key_ids: BTreeSet::new(),
                maximum_attempts: 1,
                authority_retention_seconds: 60,
                limits,
            },
            MockDirectory::new(vec![candidate]),
            failed_transport.clone(),
            MockSink::default(),
            quota.clone(),
        )
        .unwrap();
        assert!(matches!(
            service.resolve(&hash('f'), &proxy_request(request.clone())),
            Err(FederationProxyError::Unavailable { attempts: 1 })
        ));
        assert_eq!(
            failed_transport.calls.lock().unwrap().as_slice(),
            ["origin-lab"]
        );
        {
            let state = quota.state.lock().unwrap();
            assert_eq!(state.used, reserved);
            assert_eq!(state.reservations, [reserved]);
            assert_eq!(state.active, 0);
        }
        assert!(matches!(
            service.resolve(&hash('f'), &proxy_request(request)),
            Err(FederationProxyError::Quota)
        ));
        assert_eq!(
            failed_transport.calls.lock().unwrap().as_slice(),
            ["origin-lab"]
        );
    }

    #[test]
    fn wrong_request_identity_payload_or_relay_delivery_fails_closed() {
        let now = now_unix();
        let origin_key = key(11);
        let request = request();
        let candidate = candidate(
            "origin-lab",
            "https://origin.weather.edu",
            vec![federation_key("origin-object", &origin_key, now)],
            0,
            now,
        );

        let mut wrong_response = upstream(&request, "origin-object", &origin_key, now);
        wrong_response.resolve.request_sha256 = hash('9');
        assert!(
            verify_upstream_object(
                &candidate,
                &request,
                wrong_response,
                now,
                &BTreeSet::new(),
                &ProtocolLimits::default()
            )
            .is_err()
        );

        let mut relay_response = upstream(&request, "origin-object", &origin_key, now);
        relay_response
            .resolve
            .delivery_order
            .push(DeliverySource::CommunityRelay);
        assert!(
            verify_upstream_object(
                &candidate,
                &request,
                relay_response,
                now,
                &BTreeSet::new(),
                &ProtocolLimits::default()
            )
            .is_err()
        );

        let mut wrong_payload = upstream(&request, "origin-object", &origin_key, now);
        let bytes = serde_json::to_vec(&TypedObjectPayload {
            schema: GEOGRAPHIC_WINDOW_PAYLOAD_SCHEMA.to_owned(),
            request_sha256: hash('8'),
            data: json!({}),
        })
        .unwrap();
        let manifest = ObjectManifest {
            object_sha256: object_sha256(&bytes),
            encoded_size: bytes.len() as u64,
            decoded_size: bytes.len() as u64,
            ..wrong_payload.resolve.signed_manifest.unwrap().manifest
        };
        wrong_payload.resolve.signed_manifest =
            Some(sign_object_manifest(manifest, "origin-object", &origin_key).unwrap());
        wrong_payload.encoded_object = bytes;
        assert!(
            verify_upstream_object(
                &candidate,
                &request,
                wrong_payload,
                now,
                &BTreeSet::new(),
                &ProtocolLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn object_key_rotation_accepts_new_key_and_revocation_rejects_old_key() {
        let now = now_unix();
        let old_key = key(21);
        let new_key = key(22);
        let request = request();
        let candidate = candidate(
            "rotation-lab",
            "https://rotation.weather.edu",
            vec![
                federation_key("object-new", &new_key, now),
                federation_key("object-old", &old_key, now),
            ],
            0,
            now,
        );
        let revoked = BTreeSet::from(["object-old".to_owned()]);
        assert!(
            verify_upstream_object(
                &candidate,
                &request,
                upstream(&request, "object-new", &new_key, now),
                now,
                &revoked,
                &ProtocolLimits::default()
            )
            .is_ok()
        );
        assert!(
            verify_upstream_object(
                &candidate,
                &request,
                upstream(&request, "object-old", &old_key, now),
                now,
                &revoked,
                &ProtocolLimits::default()
            )
            .is_err()
        );
    }

    #[test]
    fn authority_retention_caps_expiry_and_cannot_revive_expired_or_revoked_data() {
        let now = now_unix();
        let origin_key = key(25);
        let authority_key = key(26);
        let request = request();
        let candidate = candidate(
            "retention-lab",
            "https://retention.weather.edu",
            vec![federation_key("origin-object", &origin_key, now)],
            0,
            now,
        );
        let sink = MockSink::default();
        let service = proxy_with_retention(
            MockDirectory::new(vec![candidate.clone()]),
            MockTransport::new([(
                "retention-lab".to_owned(),
                Ok(upstream(&request, "origin-object", &origin_key, now)),
            )]),
            sink.clone(),
            BTreeSet::new(),
            authority_key,
            30,
        );
        let result = service
            .resolve(&hash('f'), &proxy_request(request.clone()))
            .unwrap();
        let manifest = &result.response.signed_manifest.unwrap().manifest;
        assert_eq!(manifest.created_unix, now - 10);
        assert_eq!(manifest.expires_unix, now + 20);
        assert_eq!(sink.0.lock().unwrap().len(), 1);

        let mut expired = upstream(&request, "origin-object", &origin_key, now);
        let mut expired_manifest = expired.resolve.signed_manifest.take().unwrap().manifest;
        expired_manifest.created_unix = now - 100;
        expired_manifest.expires_unix = now - 1;
        expired.resolve.signed_manifest =
            Some(sign_object_manifest(expired_manifest, "origin-object", &origin_key).unwrap());
        let expired_sink = MockSink::default();
        let expired_service = proxy(
            MockDirectory::new(vec![candidate.clone()]),
            MockTransport::new([("retention-lab".to_owned(), Ok(expired))]),
            expired_sink.clone(),
            BTreeSet::new(),
            key(27),
        );
        assert!(matches!(
            expired_service.resolve(&hash('f'), &proxy_request(request.clone())),
            Err(FederationProxyError::Unavailable { attempts: 1 })
        ));
        assert!(expired_sink.0.lock().unwrap().is_empty());

        let revoked_sink = MockSink::default();
        let revoked_service = proxy(
            MockDirectory::new(vec![candidate]),
            MockTransport::new([(
                "retention-lab".to_owned(),
                Ok(upstream(&request, "origin-object", &origin_key, now)),
            )]),
            revoked_sink.clone(),
            BTreeSet::from(["origin-object".to_owned()]),
            key(28),
        );
        assert!(matches!(
            revoked_service.resolve(&hash('f'), &proxy_request(request)),
            Err(FederationProxyError::Unavailable { attempts: 1 })
        ));
        assert!(revoked_sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn health_order_hint_and_one_hop_attempt_bound_are_deterministic() {
        let now = now_unix();
        let origin_key = key(31);
        let request = request();
        let alpha = candidate(
            "alpha-lab",
            "https://alpha.weather.edu",
            vec![federation_key("alpha-object", &origin_key, now)],
            2,
            now,
        );
        let beta = candidate(
            "beta-lab",
            "https://beta.weather.edu",
            vec![federation_key("beta-object", &origin_key, now)],
            0,
            now,
        );
        let directory = MockDirectory::new(vec![alpha, beta]);
        let transport = MockTransport::new([
            ("alpha-lab".to_owned(), Err(UpstreamFailure::Unavailable)),
            ("beta-lab".to_owned(), Err(UpstreamFailure::Unavailable)),
        ]);
        let service = proxy(
            directory,
            transport.clone(),
            MockSink::default(),
            BTreeSet::new(),
            key(32),
        );
        let mut input = proxy_request(request.clone());
        input.preferred_origin_id = Some("alpha-lab".to_owned());
        assert!(matches!(
            service.resolve(&hash('f'), &input),
            Err(FederationProxyError::Unavailable { attempts: 2 })
        ));
        assert_eq!(
            transport.calls.lock().unwrap().as_slice(),
            ["alpha-lab", "beta-lab"]
        );

        let invalid_hint_service = proxy(
            MockDirectory::new(vec![candidate(
                "beta-lab",
                "https://beta.weather.edu",
                vec![federation_key("beta-object", &origin_key, now)],
                0,
                now,
            )]),
            MockTransport::new([]),
            MockSink::default(),
            BTreeSet::new(),
            key(33),
        );
        let mut invalid = proxy_request(request);
        invalid.preferred_origin_id = Some("unapproved-lab".to_owned());
        assert!(matches!(
            invalid_hint_service.resolve(&hash('f'), &invalid),
            Err(FederationProxyError::UnapprovedOriginHint)
        ));
    }

    #[test]
    fn cyclic_self_origin_and_duplicate_roots_are_removed_before_transport() {
        let now = now_unix();
        let origin_key = key(41);
        let request = request();
        let candidates = vec![
            candidate(
                "hetzner-authority",
                "https://weather.fahrenheitresearch.com",
                vec![federation_key("authority-object", &origin_key, now)],
                0,
                now,
            ),
            candidate(
                "alias-cycle",
                "https://weather.fahrenheitresearch.com",
                vec![federation_key("alias-object", &origin_key, now)],
                0,
                now,
            ),
        ];
        let transport = MockTransport::new([]);
        let service = proxy(
            MockDirectory::new(candidates),
            transport.clone(),
            MockSink::default(),
            BTreeSet::new(),
            key(42),
        );
        assert!(matches!(
            service.resolve(&hash('f'), &proxy_request(request)),
            Err(FederationProxyError::NoCandidate)
        ));
        assert!(transport.calls.lock().unwrap().is_empty());
        assert_eq!(
            FEDERATION_LOCAL_RESOLVE_PATH,
            "/v1/federation/objects/resolve-local"
        );
        assert_eq!(FEDERATION_HOP_HEADER, "x-rusty-federation-hop");
    }

    #[test]
    fn dns_rebinding_private_answers_redirects_and_malicious_urls_are_rejected() {
        let public: SocketAddr = "8.8.8.8:443".parse().unwrap();
        let private: SocketAddr = "127.0.0.1:443".parse().unwrap();
        assert!(validate_and_pin_dns_answers(vec![public]).is_ok());
        assert!(validate_and_pin_dns_answers(vec![public, private]).is_err());
        assert!(validate_and_pin_dns_answers(vec![private]).is_err());
        assert!(!is_global_ip("169.254.169.254".parse().unwrap()));
        assert!(!is_global_ip("::1".parse().unwrap()));
        assert!(HardenedHttpsTransport::classify_response_status(302, true).is_err());
        assert!(HardenedHttpsTransport::classify_response_status(200, true).is_err());
        for invalid in [
            "http://weather.edu",
            "https://user:secret@weather.edu",
            "https://weather.edu?next=https://evil.net",
            "https://weather.edu/#fragment",
            "https://127.0.0.1",
            "https://weather.local",
            "https://weather.edu/path",
            "https://weather.edu:8443",
        ] {
            assert!(
                CanonicalPublicHttpsRoot::parse(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn origin_credentials_are_scoped_redacted_and_never_cross_core_trait() {
        let access = ScopedOriginAccess::new(
            "alpha-lab",
            "https://alpha.weather.edu",
            Some("alpha-super-secret".to_owned()),
        )
        .unwrap();
        let debug = format!("{access:?}");
        assert!(!debug.contains("alpha-super-secret"));
        assert!(debug.contains("[REDACTED]"));
        let transport =
            HardenedHttpsTransport::new(vec![access], HttpsTransportTimeouts::default()).unwrap();
        let transport_debug = format!("{transport:?}");
        assert!(!transport_debug.contains("alpha-super-secret"));
        assert!(!format!("{}", UpstreamFailure::Unavailable).contains("alpha.weather.edu"));
        assert!(!format!("{}", UpstreamFailure::DnsRejected).contains("127.0.0.1"));

        let now = now_unix();
        let origin_key = key(51);
        let wrong_root = candidate(
            "alpha-lab",
            "https://different.weather.edu",
            vec![federation_key("alpha-object", &origin_key, now)],
            0,
            now,
        );
        assert!(matches!(
            transport.access_for(&wrong_root),
            Err(UpstreamFailure::Unavailable)
        ));
    }

    #[test]
    fn capability_pressure_geography_size_and_loop_filters_fail_closed() {
        let now = now_unix();
        let origin_key = key(61);
        let request = request();
        let authority_root =
            CanonicalPublicHttpsRoot::parse("https://weather.fahrenheitresearch.com").unwrap();
        let compatible = candidate(
            "origin-lab",
            "https://origin.weather.edu",
            vec![federation_key("origin-object", &origin_key, now)],
            0,
            now,
        );
        assert!(candidate_is_compatible(
            &compatible,
            &request,
            "hetzner-authority",
            &authority_root,
            now,
            &ProtocolLimits::default()
        ));
        let mut wrong_level = request.clone();
        let ShareQuery::GeographicWindow {
            pressure_levels_hpa,
            ..
        } = &mut wrong_level.query
        else {
            unreachable!()
        };
        *pressure_levels_hpa = vec![925];
        assert!(!candidate_is_compatible(
            &compatible,
            &wrong_level,
            "hetzner-authority",
            &authority_root,
            now,
            &ProtocolLimits::default()
        ));
        let mut out_of_bounds = request;
        let ShareQuery::GeographicWindow {
            west_longitude_e7,
            east_longitude_e7,
            ..
        } = &mut out_of_bounds.query
        else {
            unreachable!()
        };
        *west_longitude_e7 = 1_000_000_000;
        *east_longitude_e7 = 1_100_000_000;
        assert!(!candidate_is_compatible(
            &compatible,
            &out_of_bounds,
            "hetzner-authority",
            &authority_root,
            now,
            &ProtocolLimits::default()
        ));

        let mut expired = compatible.clone();
        expired.descriptor.expires_unix = now;
        assert!(!candidate_is_compatible(
            &expired,
            &out_of_bounds,
            "hetzner-authority",
            &authority_root,
            now,
            &ProtocolLimits::default()
        ));

        let mut request_limited = compatible;
        request_limited.descriptor.quotas.maximum_request_bytes = 1;
        assert!(!candidate_is_compatible(
            &request_limited,
            &out_of_bounds,
            "hetzner-authority",
            &authority_root,
            now,
            &ProtocolLimits::default()
        ));
    }

    #[test]
    fn active_authority_signing_key_cannot_be_configured_as_revoked() {
        let now = now_unix();
        let origin_key = key(71);
        let result = FederationProxy::new(
            FederationProxyConfig {
                enabled: true,
                kill_switch: false,
                authority_origin_id: "hetzner-authority".to_owned(),
                authority_https_root: "https://weather.fahrenheitresearch.com".to_owned(),
                authority_signing_key_id: "authority-object-v1".to_owned(),
                authority_signing_key: key(72),
                revoked_key_ids: BTreeSet::from(["authority-object-v1".to_owned()]),
                maximum_attempts: 1,
                authority_retention_seconds: 60,
                limits: ProtocolLimits::default(),
            },
            MockDirectory::new(vec![candidate(
                "origin-lab",
                "https://origin.weather.edu",
                vec![federation_key("origin-object", &origin_key, now)],
                0,
                now,
            )]),
            MockTransport::new([]),
            MockSink::default(),
            NoopQuota,
        );
        assert!(matches!(
            result,
            Err(FederationProxyError::InvalidConfiguration)
        ));
    }

    #[test]
    fn operator_kill_switch_stops_only_proxy_transfers() {
        let now = now_unix();
        let origin_key = key(81);
        let request = request();
        let transport = MockTransport::new([(
            "origin-lab".to_owned(),
            Ok(upstream(&request, "origin-object", &origin_key, now)),
        )]);
        let service = proxy(
            MockDirectory::new(vec![candidate(
                "origin-lab",
                "https://origin.weather.edu",
                vec![federation_key("origin-object", &origin_key, now)],
                0,
                now,
            )]),
            transport.clone(),
            MockSink::default(),
            BTreeSet::new(),
            key(82),
        );
        service.set_kill_switch(true);
        assert!(service.kill_switch_enabled());
        assert!(matches!(
            service.resolve(&hash('f'), &proxy_request(request)),
            Err(FederationProxyError::Disabled)
        ));
        assert!(transport.calls.lock().unwrap().is_empty());
    }
}

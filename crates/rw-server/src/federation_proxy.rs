//! rw-server adapters for the hardened public-origin federation proxy.
//!
//! This module is kept outside the shared HTTP/config/state files so its
//! directory, quota, signer, and staging boundaries can be tested before the
//! feature-gated routes are mounted.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rw_community_protocol::{FederationQueryCapability, ShareQuery, ShareRequest};
use rw_federation_proxy::{
    DirectoryUnavailable, FederationProxy, FederationProxyConfig, FederationProxyQuota,
    HardenedHttpsTransport, HttpsTransportTimeouts, ProxyCandidate, ProxyHealthObservation,
    QuotaUnavailable, ScopedOriginAccess, StageFailure, VerifiedFederationDirectory,
    VerifiedObjectSink,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

use crate::TokenSet;
use crate::community::CommunityService;
use crate::community_store::{AccountingLimits, QuotaLedger, TransferPermit};
use crate::config::AppConfig;
use crate::federation::{
    FederationHealthObservation, FederationSelectionBounds, FederationSelectionRequest,
    FederationService,
};

pub(crate) const FEDERATION_PRODUCT_RECIPE_PARAMETER: &str = "federation_product";

type InnerFederationProxy = FederationProxy<
    FederationDirectoryAdapter,
    HardenedHttpsTransport,
    CommunityFederationSink,
    CommunityFederationQuota,
>;

pub const FEDERATION_PROXY_KILL_SWITCH_SCHEMA: &str = "rw.server.federation-proxy-kill-switch.v1";
pub const FEDERATION_PROXY_STATUS_SCHEMA: &str = "rw.server.federation-proxy-status.v1";
const FEDERATION_PROXY_CONTROL_SCHEMA: &str = "rw.server.federation-proxy-control.v1";
const MAX_CONTROL_STATE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct FederationProxyKillSwitchRequest {
    pub schema: String,
    pub engaged: bool,
}

#[derive(Debug, Clone, Serialize, JsonSchema, ToSchema)]
pub struct FederationProxyStatusResponse {
    pub schema: String,
    pub enabled: bool,
    pub kill_switch: bool,
    pub persistence_healthy: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FederationProxyControlError {
    #[error("federation proxy control is disabled")]
    Disabled,
    #[error("federation proxy operator authorization was rejected")]
    Forbidden,
    #[error("federation proxy control request is invalid")]
    Invalid,
    #[error("federation proxy control state could not be persisted")]
    Persistence,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableControlState {
    schema: String,
    kill_switch: bool,
}

#[derive(Clone)]
struct DurableFederationProxyControl {
    path: Arc<PathBuf>,
    state: Arc<Mutex<DurableControlState>>,
    healthy: Arc<AtomicBool>,
}

impl DurableFederationProxyControl {
    fn open(path: &Path, configured_kill_switch: bool) -> Result<Self, String> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| "federation proxy control path has no parent".to_owned())?;
        fs::create_dir_all(parent)
            .map_err(|_| "federation proxy control directory could not be created".to_owned())?;
        let parent_metadata = fs::symlink_metadata(parent)
            .map_err(|_| "federation proxy control directory is unavailable".to_owned())?;
        if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
            return Err("federation proxy control directory is unsafe".into());
        }
        let loaded = match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || !metadata.is_file()
                    || metadata.len() == 0
                    || metadata.len() > MAX_CONTROL_STATE_BYTES
                {
                    return Err("federation proxy control state is unsafe".into());
                }
                let bytes = fs::read(path)
                    .map_err(|_| "federation proxy control state could not be read".to_owned())?;
                let state: DurableControlState = serde_json::from_slice(&bytes)
                    .map_err(|_| "federation proxy control state is malformed".to_owned())?;
                if state.schema != FEDERATION_PROXY_CONTROL_SCHEMA {
                    return Err("federation proxy control state version is unsupported".into());
                }
                state
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => DurableControlState {
                schema: FEDERATION_PROXY_CONTROL_SCHEMA.into(),
                kill_switch: configured_kill_switch,
            },
            Err(_) => return Err("federation proxy control state is unavailable".into()),
        };
        let state = DurableControlState {
            schema: FEDERATION_PROXY_CONTROL_SCHEMA.into(),
            kill_switch: loaded.kill_switch || configured_kill_switch,
        };
        persist_control_state(path, &state)
            .map_err(|_| "federation proxy control state could not be persisted".to_owned())?;
        Ok(Self {
            path: Arc::new(path.to_path_buf()),
            state: Arc::new(Mutex::new(state)),
            healthy: Arc::new(AtomicBool::new(true)),
        })
    }

    fn kill_switch(&self) -> bool {
        self.state
            .lock()
            .expect("federation proxy control mutex poisoned")
            .kill_switch
    }

    fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Acquire)
    }

    fn persist(&self, engaged: bool) -> Result<(), FederationProxyControlError> {
        let mut state = self
            .state
            .lock()
            .expect("federation proxy control mutex poisoned");
        let candidate = DurableControlState {
            schema: FEDERATION_PROXY_CONTROL_SCHEMA.into(),
            kill_switch: engaged,
        };
        if persist_control_state(&self.path, &candidate).is_err() {
            self.healthy.store(false, Ordering::Release);
            return Err(FederationProxyControlError::Persistence);
        }
        *state = candidate;
        self.healthy.store(true, Ordering::Release);
        Ok(())
    }
}

fn persist_control_state(path: &Path, state: &DurableControlState) -> Result<(), ()> {
    let bytes = serde_json::to_vec(state).map_err(|_| ())?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_CONTROL_STATE_BYTES {
        return Err(());
    }
    rw_store::atomic::atomic_write_bytes(path, &bytes).map_err(|_| ())
}

pub(crate) struct ServerFederationProxy {
    inner: InnerFederationProxy,
    control: DurableFederationProxyControl,
    operator_principals: BTreeSet<String>,
    update_lock: Mutex<()>,
}

impl std::fmt::Debug for ServerFederationProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerFederationProxy")
            .field("enabled", &true)
            .field("kill_switch", &self.inner.kill_switch_enabled())
            .field("persistence_healthy", &self.control.healthy())
            .field("operator_count", &self.operator_principals.len())
            .finish()
    }
}

impl ServerFederationProxy {
    pub(crate) fn resolve(
        &self,
        principal: &str,
        request: &rw_federation_proxy::FederationProxyRequest,
    ) -> Result<rw_federation_proxy::FederationProxyResult, rw_federation_proxy::FederationProxyError>
    {
        self.inner.resolve(principal, request)
    }

    pub(crate) fn startup_status(&self) -> FederationProxyStatusResponse {
        self.status_response()
    }

    pub(crate) fn operator_status(
        &self,
        principal: &str,
    ) -> Result<FederationProxyStatusResponse, FederationProxyControlError> {
        self.require_operator(principal)?;
        let _update = self
            .update_lock
            .lock()
            .expect("federation proxy update mutex poisoned");
        Ok(self.status_response())
    }

    pub(crate) fn set_kill_switch(
        &self,
        principal: &str,
        request: FederationProxyKillSwitchRequest,
    ) -> Result<FederationProxyStatusResponse, FederationProxyControlError> {
        self.require_operator(principal)?;
        if request.schema != FEDERATION_PROXY_KILL_SWITCH_SCHEMA {
            return Err(FederationProxyControlError::Invalid);
        }
        let _update = self
            .update_lock
            .lock()
            .expect("federation proxy update mutex poisoned");
        if request.engaged {
            // Stop transport first. A persistence failure leaves this process
            // safely killed and marks all proxy admissions unhealthy.
            self.inner.set_kill_switch(true);
            self.control.persist(true)?;
        } else {
            // Never reopen transport until the disengaged state is durable.
            if let Err(error) = self.control.persist(false) {
                self.inner.set_kill_switch(true);
                return Err(error);
            }
            self.inner.set_kill_switch(false);
        }
        Ok(self.status_response())
    }

    fn require_operator(&self, principal: &str) -> Result<(), FederationProxyControlError> {
        if self.operator_principals.contains(principal) {
            Ok(())
        } else {
            Err(FederationProxyControlError::Forbidden)
        }
    }

    fn status_response(&self) -> FederationProxyStatusResponse {
        FederationProxyStatusResponse {
            schema: FEDERATION_PROXY_STATUS_SCHEMA.into(),
            enabled: true,
            kill_switch: self.inner.kill_switch_enabled(),
            persistence_healthy: self.control.healthy(),
        }
    }
}

/// Coarse startup evidence for `rw-server doctor`. Deliberately omits every
/// URL, path, credential, address, DNS answer, principal, and transport error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FederationProxyDoctorStatus {
    pub approved_origins: usize,
    pub authority_signing_key_id: String,
    pub kill_switch: bool,
    pub durable_accounting_opened: bool,
    pub local_resolve_enabled: bool,
    pub local_resolve_credential_loaded: bool,
}

pub fn doctor_status(
    config: &AppConfig,
    api_tokens: &TokenSet,
) -> Result<FederationProxyDoctorStatus, String> {
    if !config.federation.proxy.enabled && !config.federation.proxy.accept_local_resolve {
        return Err("federation proxy data paths are not enabled".into());
    }
    let community = CommunityService::open(&config.community)
        .map_err(|_| "canonical Community authority could not be opened".to_owned())?;
    let authority = community
        .federation_authority_signing_material()
        .map_err(|_| "canonical Community authority signer is unavailable".to_owned())?;
    let local_resolve_credential_loaded =
        !load_federation_origin_tokens(config, api_tokens)?.is_empty();
    let (durable_accounting_opened, kill_switch) = if config.federation.proxy.enabled {
        let federation = FederationService::open(&config.federation)
            .map_err(|_| "verified federation directory could not be opened".to_owned())?;
        let opened = open_server_federation_proxy(config, community, federation)?;
        let Some(opened) = opened else {
            return Err("federation proxy did not initialize".into());
        };
        (true, opened.startup_status().kill_switch)
    } else {
        (false, config.federation.proxy.kill_switch)
    };
    Ok(FederationProxyDoctorStatus {
        approved_origins: config.federation.approved_origins.len(),
        authority_signing_key_id: authority.signing_key_id,
        kill_switch,
        durable_accounting_opened,
        local_resolve_enabled: config.federation.proxy.accept_local_resolve,
        local_resolve_credential_loaded,
    })
}

/// Load the one-hop origin credential domain and prove it is disjoint from
/// ordinary BowEcho API credentials before routes or upstream transports are
/// exposed. Errors are deliberately value-free.
pub(crate) fn load_federation_origin_tokens(
    config: &AppConfig,
    api_tokens: &TokenSet,
) -> Result<TokenSet, String> {
    let mut domains = vec![("ordinary API", api_tokens.clone())];
    let origin_tokens = if config.federation.proxy.accept_local_resolve {
        let path = config
            .federation
            .proxy
            .local_resolve_token_file
            .as_deref()
            .ok_or_else(|| "dedicated federation origin credential is not configured".to_owned())?;
        let tokens = load_credential_set(path, "dedicated federation origin", false)?;
        add_disjoint_credential_domain(&mut domains, "dedicated federation origin", &tokens)?;
        tokens
    } else {
        TokenSet::default()
    };

    // Every outbound origin data bearer and health-probe bearer is a separate
    // privilege domain. File paths alone are not identities: two different
    // files containing the same secret must also fail closed.
    for approved in &config.federation.approved_origins {
        if config.federation.proxy.enabled
            && let Some(path) = approved.data_bearer_token_file.as_deref()
        {
            let tokens = load_credential_set(path, "federation origin data", true)?;
            add_disjoint_credential_domain(&mut domains, "federation origin data", &tokens)?;
        }
        if config.federation.health_monitor_enabled
            && let Some(path) = approved.health_bearer_token_file.as_deref()
        {
            let tokens = load_credential_set(path, "federation origin health", true)?;
            add_disjoint_credential_domain(&mut domains, "federation origin health", &tokens)?;
        }
    }
    Ok(origin_tokens)
}

/// Value-safe startup/doctor validation for every active federation bearer
/// domain. Callers receive no digest or secret material.
pub fn validate_credential_isolation(
    config: &AppConfig,
    api_tokens: &TokenSet,
) -> Result<(), String> {
    load_federation_origin_tokens(config, api_tokens).map(|_| ())
}

fn load_credential_set(
    path: &std::path::Path,
    domain: &str,
    require_single: bool,
) -> Result<TokenSet, String> {
    let tokens = TokenSet::load_file(path)
        .map_err(|_| format!("{domain} credential could not be loaded"))?;
    if tokens.is_empty() {
        return Err(format!("{domain} credential file is empty"));
    }
    if require_single && tokens.len() != 1 {
        return Err(format!(
            "{domain} credential file must contain exactly one token"
        ));
    }
    Ok(tokens)
}

fn add_disjoint_credential_domain(
    domains: &mut Vec<(&'static str, TokenSet)>,
    name: &'static str,
    tokens: &TokenSet,
) -> Result<(), String> {
    if domains
        .iter()
        .any(|(_, existing)| tokens.overlaps(existing))
    {
        return Err(format!(
            "{name} credentials overlap another federation or ordinary API credential domain"
        ));
    }
    domains.push((name, tokens.clone()));
    Ok(())
}

/// Construct the authority proxy only after both CommunityService and the
/// verified descriptor directory are open. Origin credentials remain scoped
/// to their signed origin id and HTTPS root inside HardenedHttpsTransport.
pub(crate) fn open_server_federation_proxy(
    config: &AppConfig,
    community: CommunityService,
    federation: FederationService,
) -> Result<Option<Arc<ServerFederationProxy>>, String> {
    let proxy = &config.federation.proxy;
    if !proxy.enabled {
        return Ok(None);
    }
    let authority = community
        .federation_authority_signing_material()
        .map_err(|_| "canonical Community authority signer is unavailable".to_owned())?;
    let mut scoped_origins = Vec::with_capacity(config.federation.approved_origins.len());
    for approved in &config.federation.approved_origins {
        let signed = federation
            .descriptor(&approved.origin_id)
            .map_err(|_| "an approved federation descriptor is unavailable".to_owned())?;
        scoped_origins.push(
            ScopedOriginAccess::from_token_file(
                approved.origin_id.clone(),
                &signed.descriptor.https_base_url,
                approved.data_bearer_token_file.as_deref(),
            )
            .map_err(|_| "an origin-scoped federation credential is invalid".to_owned())?,
        );
    }
    let transport = HardenedHttpsTransport::new(
        scoped_origins,
        HttpsTransportTimeouts {
            resolve: Duration::from_secs(proxy.resolve_timeout_seconds),
            connect: Duration::from_secs(proxy.connect_timeout_seconds),
            send: Duration::from_secs(proxy.send_timeout_seconds),
            receive: Duration::from_secs(proxy.receive_timeout_seconds),
            global: Duration::from_secs(proxy.global_timeout_seconds),
        },
    )
    .map_err(|_| "federation HTTPS transport configuration is invalid".to_owned())?;
    let quota = QuotaLedger::open(
        &proxy.accounting_state_file,
        AccountingLimits {
            upload_bytes_per_month: proxy.monthly_download_bytes_per_principal,
            download_bytes_per_month: proxy.monthly_download_bytes_per_principal,
            promoted_bytes_per_month: proxy.monthly_download_bytes_per_principal,
            concurrent_transfers: proxy.concurrent_requests_per_principal,
            maximum_principals: proxy.maximum_principals,
        },
        current_month(),
    )
    .map_err(|_| "federation quota state is unavailable".to_owned())?;
    let control =
        DurableFederationProxyControl::open(&proxy.control_state_file, proxy.kill_switch)?;
    let service = FederationProxy::new(
        FederationProxyConfig {
            enabled: true,
            kill_switch: control.kill_switch(),
            authority_origin_id: proxy.authority_origin_id.clone(),
            authority_https_root: proxy.authority_https_root.clone(),
            authority_signing_key_id: authority.signing_key_id,
            authority_signing_key: authority.signing_key,
            revoked_key_ids: config
                .federation
                .revoked_key_ids
                .iter()
                .cloned()
                .collect::<BTreeSet<_>>(),
            maximum_attempts: proxy.maximum_attempts,
            authority_retention_seconds: authority.object_manifest_retention_seconds,
            limits: authority.limits,
        },
        FederationDirectoryAdapter::new(federation),
        transport,
        CommunityFederationSink::new(community),
        CommunityFederationQuota::new(quota),
    )
    .map_err(|_| "federation proxy configuration is invalid".to_owned())?;
    Ok(Some(Arc::new(ServerFederationProxy {
        inner: service,
        control,
        operator_principals: proxy.operator_principals.iter().cloned().collect(),
        update_lock: Mutex::new(()),
    })))
}

/// Maps one canonical ShareRequest to the existing signed descriptor catalog.
/// If a request names `recipe.parameters.federation_product`, only that exact
/// signed product is eligible. Otherwise all products whose signed query
/// capability and requested pressure levels match are tried deterministically.
#[derive(Debug, Clone)]
pub(crate) struct FederationDirectoryAdapter {
    service: FederationService,
}

impl FederationDirectoryAdapter {
    pub(crate) fn new(service: FederationService) -> Self {
        Self { service }
    }
}

impl VerifiedFederationDirectory for FederationDirectoryAdapter {
    fn candidates(
        &self,
        request: &ShareRequest,
        minimum_response_bytes: u64,
    ) -> Result<Vec<ProxyCandidate>, DirectoryUnavailable> {
        let query = request_query_capability(&request.query);
        let requested_levels = request_pressure_levels(&request.query);
        let requested_product = request
            .recipe
            .parameters
            .get(FEDERATION_PRODUCT_RECIPE_PARAMETER);
        let catalog = self.service.catalog().map_err(|_| DirectoryUnavailable)?;
        let mut by_origin = BTreeMap::<String, ProxyCandidate>::new();
        for signed in catalog.catalog.origins {
            let descriptor = signed.descriptor;
            let Some(model) = descriptor
                .models
                .iter()
                .find(|model| model.model == request.model)
            else {
                continue;
            };
            for product in &model.products {
                if requested_product.is_some_and(|requested| requested != &product.product)
                    || !product.queries.contains(&query)
                    || (!requested_levels.is_empty()
                        && !requested_levels
                            .iter()
                            .all(|level| product.pressure_levels_hpa.contains(level)))
                {
                    continue;
                }
                let bounds = selection_bounds(&request.query);
                let selected = self
                    .service
                    .select(&FederationSelectionRequest {
                        model: request.model.clone(),
                        product: product.product.clone(),
                        query,
                        bounds,
                        minimum_response_bytes,
                        require_replication: false,
                    })
                    .map_err(|_| DirectoryUnavailable)?;
                for selected in selected {
                    let signed = self
                        .service
                        .descriptor(&selected.origin_id)
                        .map_err(|_| DirectoryUnavailable)?;
                    let candidate = ProxyCandidate {
                        descriptor: signed.descriptor,
                        matched_product: product.product.clone(),
                        consecutive_failures: selected.consecutive_failures,
                    };
                    match by_origin.get(&selected.origin_id) {
                        Some(existing) if existing.matched_product <= candidate.matched_product => {
                        }
                        _ => {
                            by_origin.insert(selected.origin_id, candidate);
                        }
                    }
                }
            }
        }
        Ok(by_origin.into_values().collect())
    }

    fn record_health(
        &self,
        origin_id: &str,
        observation: ProxyHealthObservation,
    ) -> Result<(), DirectoryUnavailable> {
        let observation = match observation {
            ProxyHealthObservation::Healthy => FederationHealthObservation::Healthy,
            ProxyHealthObservation::Failed => FederationHealthObservation::Failed,
        };
        self.service
            .record_health(origin_id, observation)
            .map_err(|_| DirectoryUnavailable)
    }
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

fn request_pressure_levels(query: &ShareQuery) -> &[u16] {
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

fn selection_bounds(query: &ShareQuery) -> Option<FederationSelectionBounds> {
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
        } => Some(point_selection_bounds(*latitude_e7, *longitude_e7)),
        ShareQuery::GeographicWindow {
            west_longitude_e7,
            south_latitude_e7,
            east_longitude_e7,
            north_latitude_e7,
            ..
        } => Some(FederationSelectionBounds {
            west_longitude_e7: *west_longitude_e7,
            south_latitude_e7: *south_latitude_e7,
            east_longitude_e7: *east_longitude_e7,
            north_latitude_e7: *north_latitude_e7,
        }),
        ShareQuery::NativeWindow { .. }
        | ShareQuery::TemporalGrid { .. }
        | ShareQuery::CaseArtifact { .. } => None,
    }
}

fn point_selection_bounds(latitude_e7: i32, longitude_e7: i32) -> FederationSelectionBounds {
    // FederationService's rectangle contract is intentionally non-empty.
    // Form the smallest representable rectangle containing the point while
    // staying inside the signed fixed-point coordinate domain.
    let (west, east) = if longitude_e7 < 1_800_000_000 {
        (longitude_e7, longitude_e7 + 1)
    } else {
        (longitude_e7 - 1, longitude_e7)
    };
    let (south, north) = if latitude_e7 < 900_000_000 {
        (latitude_e7, latitude_e7 + 1)
    } else {
        (latitude_e7 - 1, latitude_e7)
    };
    FederationSelectionBounds {
        west_longitude_e7: west,
        south_latitude_e7: south,
        east_longitude_e7: east,
        north_latitude_e7: north,
    }
}

#[derive(Debug, Clone)]
pub(crate) struct CommunityFederationSink {
    community: CommunityService,
}

impl CommunityFederationSink {
    pub(crate) fn new(community: CommunityService) -> Self {
        Self { community }
    }
}

impl VerifiedObjectSink for CommunityFederationSink {
    fn stage(
        &self,
        request_sha256: &str,
        manifest: &rw_community_protocol::SignedObjectManifest,
        encoded_object: &[u8],
    ) -> Result<(), StageFailure> {
        self.community
            .stage_verified_federated_object(request_sha256, manifest, encoded_object)
            .map_err(|_| StageFailure)
    }
}

#[derive(Clone)]
pub(crate) struct CommunityFederationQuota {
    ledger: Arc<QuotaLedger>,
    month: Arc<dyn Fn() -> u32 + Send + Sync>,
}

impl CommunityFederationQuota {
    pub(crate) fn new(ledger: QuotaLedger) -> Self {
        Self {
            ledger: Arc::new(ledger),
            month: Arc::new(current_month),
        }
    }

    #[cfg(test)]
    fn with_month_source(
        ledger: QuotaLedger,
        month: impl Fn() -> u32 + Send + Sync + 'static,
    ) -> Self {
        Self {
            ledger: Arc::new(ledger),
            month: Arc::new(month),
        }
    }
}

impl FederationProxyQuota for CommunityFederationQuota {
    type Permit = TransferPermit;

    fn reserve(
        &self,
        principal: &str,
        maximum_upstream_bytes: u64,
    ) -> Result<Self::Permit, QuotaUnavailable> {
        self.ledger
            .begin_reserved_download(principal, (self.month)(), maximum_upstream_bytes)
            .map_err(|_| QuotaUnavailable)
    }
}

fn current_month() -> u32 {
    use chrono::Datelike as _;
    let now = chrono::Utc::now();
    u32::try_from(now.year()).unwrap_or(0).saturating_mul(100) + now.month()
}

#[cfg(test)]
pub(crate) fn test_server_federation_proxy(
    control_state_file: &Path,
    configured_kill_switch: bool,
    operator_principal: String,
) -> ServerFederationProxy {
    let limits = rw_community_protocol::ProtocolLimits::default();
    let control =
        DurableFederationProxyControl::open(control_state_file, configured_kill_switch).unwrap();
    let inner = FederationProxy::new(
        FederationProxyConfig {
            enabled: true,
            kill_switch: control.kill_switch(),
            authority_origin_id: "test-authority".into(),
            authority_https_root: "https://weather.example.com".into(),
            authority_signing_key_id: "test-authority-key".into(),
            authority_signing_key: ed25519_dalek::SigningKey::from_bytes(&[73; 32]),
            revoked_key_ids: BTreeSet::new(),
            maximum_attempts: 1,
            authority_retention_seconds: 60,
            limits,
        },
        FederationDirectoryAdapter::new(
            FederationService::open(&crate::config::FederationConfig::default()).unwrap(),
        ),
        HardenedHttpsTransport::new(vec![], HttpsTransportTimeouts::default()).unwrap(),
        CommunityFederationSink::new(
            CommunityService::open(&crate::config::CommunityConfig::default()).unwrap(),
        ),
        CommunityFederationQuota::new(
            QuotaLedger::memory(
                AccountingLimits {
                    upload_bytes_per_month: 1024 * 1024,
                    download_bytes_per_month: 1024 * 1024,
                    promoted_bytes_per_month: 1024 * 1024,
                    concurrent_transfers: 1,
                    maximum_principals: 4,
                },
                202608,
            )
            .unwrap(),
        ),
    )
    .unwrap();
    ServerFederationProxy {
        inner,
        control,
        operator_principals: BTreeSet::from([operator_principal]),
        update_lock: Mutex::new(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use base64::Engine as _;
    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        DataOrigin, FederationCoverageArea, FederationModelCapability, FederationPolicyLinks,
        FederationProductCapability, FederationPublicKey, FederationQuotaSummary,
        FederationReplicationPolicy, FederationRetentionSummary, ProtocolLimits, PublicationGrant,
        REQUEST_SCHEMA, RecipeIdentity, ResolveObjectRequest, SignatureAlgorithm, SourceProvenance,
    };
    use rw_federation_proxy::{
        FederatedOriginTransport, FederationProxyError, NoopSink, UpstreamFailure, UpstreamObject,
    };

    const API_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const ORIGIN_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn write_private_token(path: &std::path::Path, token: &str) {
        std::fs::write(path, format!("{token}\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn hash(character: char) -> String {
        std::iter::repeat_n(character, 64).collect()
    }

    fn proxy_test_request() -> ShareRequest {
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

    fn proxy_test_candidate(now: i64) -> ProxyCandidate {
        let origin_key = SigningKey::from_bytes(&[41; 32]);
        let public_key = FederationPublicKey {
            algorithm: SignatureAlgorithm::Ed25519,
            key_id: "origin-object-v1".to_owned(),
            public_key_base64: base64::engine::general_purpose::STANDARD
                .encode(origin_key.verifying_key().as_bytes()),
            not_before_unix: now - 60,
            expires_unix: now + 3_600,
        };
        let root = "https://origin.weather.edu";
        let mut descriptor = rw_community_protocol::PublicOriginDescriptor {
            schema: rw_community_protocol::FEDERATION_ORIGIN_SCHEMA.to_owned(),
            origin_id: "origin-lab".to_owned(),
            display_name: "Origin weather lab".to_owned(),
            https_base_url: root.to_owned(),
            health_path: "/v1/health/ready".to_owned(),
            descriptor_signing_keys: vec![public_key.clone()],
            object_signing_keys: vec![public_key],
            models: vec![FederationModelCapability {
                model: "hrrr".to_owned(),
                products: vec![FederationProductCapability {
                    product: "native".to_owned(),
                    queries: vec![FederationQueryCapability::ArbitraryDomainMap],
                    pressure_levels_hpa: vec![500],
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
                maximum_response_bytes: 1024,
                requests_per_minute: 120,
                concurrent_requests: 8,
                monthly_egress_bytes: 1024 * 1024,
            },
        };
        descriptor.normalize();
        ProxyCandidate {
            descriptor,
            matched_product: "native".to_owned(),
            consecutive_failures: 0,
        }
    }

    #[derive(Clone)]
    struct ProxyTestDirectory(ProxyCandidate);

    impl VerifiedFederationDirectory for ProxyTestDirectory {
        fn candidates(
            &self,
            _request: &ShareRequest,
            _minimum_response_bytes: u64,
        ) -> Result<Vec<ProxyCandidate>, DirectoryUnavailable> {
            Ok(vec![self.0.clone()])
        }

        fn record_health(
            &self,
            _origin_id: &str,
            _observation: ProxyHealthObservation,
        ) -> Result<(), DirectoryUnavailable> {
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct AlwaysFailTransport(Arc<AtomicUsize>);

    impl FederatedOriginTransport for AlwaysFailTransport {
        fn fetch(
            &self,
            _candidate: &ProxyCandidate,
            _request: &ResolveObjectRequest,
            _limits: &ProtocolLimits,
        ) -> Result<UpstreamObject, UpstreamFailure> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Err(UpstreamFailure::Unavailable)
        }
    }

    fn proxy_with_production_quota(
        candidate: ProxyCandidate,
        transport: AlwaysFailTransport,
        quota: CommunityFederationQuota,
        limits: ProtocolLimits,
    ) -> FederationProxy<ProxyTestDirectory, AlwaysFailTransport, NoopSink, CommunityFederationQuota>
    {
        FederationProxy::new(
            FederationProxyConfig {
                enabled: true,
                kill_switch: false,
                authority_origin_id: "hetzner-authority".to_owned(),
                authority_https_root: "https://weather.fahrenheitresearch.com".to_owned(),
                authority_signing_key_id: "authority-object-v1".to_owned(),
                authority_signing_key: SigningKey::from_bytes(&[42; 32]),
                revoked_key_ids: BTreeSet::new(),
                maximum_attempts: 1,
                authority_retention_seconds: 60,
                limits,
            },
            ProxyTestDirectory(candidate),
            transport,
            NoopSink,
            quota,
        )
        .unwrap()
    }

    #[test]
    fn exact_product_hint_is_canonical_recipe_state() {
        assert_eq!(FEDERATION_PRODUCT_RECIPE_PARAMETER, "federation_product");
    }

    #[test]
    fn query_capability_and_geography_are_derived_without_client_urls() {
        let query = ShareQuery::GeographicWindow {
            storage_slot: 1,
            valid_unix: 2,
            west_longitude_e7: -1_200_000_000,
            south_latitude_e7: 300_000_000,
            east_longitude_e7: -800_000_000,
            north_latitude_e7: 500_000_000,
            pressure_levels_hpa: vec![500],
        };
        assert_eq!(
            request_query_capability(&query),
            FederationQueryCapability::ArbitraryDomainMap
        );
        assert_eq!(request_pressure_levels(&query), [500]);
        let bounds = selection_bounds(&query).unwrap();
        assert_eq!(bounds.west_longitude_e7, -1_200_000_000);
        assert_eq!(bounds.east_longitude_e7, -800_000_000);
    }

    #[test]
    fn point_selection_uses_a_minimal_bounded_rectangle() {
        let ordinary = point_selection_bounds(0, 0);
        assert_eq!(ordinary.west_longitude_e7, 0);
        assert_eq!(ordinary.east_longitude_e7, 1);
        assert_eq!(ordinary.south_latitude_e7, 0);
        assert_eq!(ordinary.north_latitude_e7, 1);

        let edge = point_selection_bounds(900_000_000, 1_800_000_000);
        assert_eq!(edge.west_longitude_e7, 1_799_999_999);
        assert_eq!(edge.east_longitude_e7, 1_800_000_000);
        assert_eq!(edge.south_latitude_e7, 899_999_999);
        assert_eq!(edge.north_latitude_e7, 900_000_000);
    }

    #[test]
    fn startup_origin_credentials_must_be_nonempty_and_disjoint_from_api_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("federation-origin.token");
        let api_tokens = TokenSet::from_tokens([API_TOKEN]).unwrap();
        let mut config = AppConfig::default();
        config.federation.proxy.accept_local_resolve = true;
        config.federation.proxy.local_resolve_token_file = Some(path.clone());

        write_private_token(&path, ORIGIN_TOKEN);
        let loaded = load_federation_origin_tokens(&config, &api_tokens).unwrap();
        assert_eq!(loaded.len(), 1);
        assert!(!loaded.overlaps(&api_tokens));

        write_private_token(&path, API_TOKEN);
        let error = load_federation_origin_tokens(&config, &api_tokens).unwrap_err();
        assert!(error.contains("overlap"));
        assert!(!error.contains(API_TOKEN));

        write_private_token(&path, "# no credential");
        let error = load_federation_origin_tokens(&config, &api_tokens).unwrap_err();
        assert!(error.contains("empty"));
    }

    #[test]
    fn origin_only_doctor_validates_credential_without_opening_outbound_proxy() {
        let directory = tempfile::tempdir().unwrap();
        let token_path = directory.path().join("local-resolve.token");
        write_private_token(&token_path, ORIGIN_TOKEN);
        let key_path = directory.path().join("community.key");
        std::fs::write(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode([9u8; 32]),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = AppConfig::default();
        config.community.enabled = true;
        config.community.root = directory.path().join("community");
        config.community.signing_key_file = Some(key_path);
        config.federation.proxy.security_tests_passed = true;
        config.federation.proxy.enabled = false;
        config.federation.proxy.accept_local_resolve = true;
        config.federation.proxy.local_resolve_token_file = Some(token_path);
        let api_tokens = TokenSet::from_tokens([API_TOKEN]).unwrap();
        let status = doctor_status(&config, &api_tokens).unwrap();
        assert!(status.local_resolve_enabled);
        assert!(status.local_resolve_credential_loaded);
        assert!(!status.durable_accounting_opened);
    }

    #[test]
    fn every_configured_federation_credential_value_is_a_disjoint_domain() {
        let directory = tempfile::tempdir().unwrap();
        let local = directory.path().join("local.token");
        let data_a = directory.path().join("data-a.token");
        let health_a = directory.path().join("health-a.token");
        let data_b = directory.path().join("data-b.token");
        for (path, token) in [
            (&local, "llllllllllllllllllllllllllllllll"),
            (&data_a, "dddddddddddddddddddddddddddddddd"),
            (&health_a, "hhhhhhhhhhhhhhhhhhhhhhhhhhhhhhhh"),
            (&data_b, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"),
        ] {
            write_private_token(path, token);
        }
        let api_tokens = TokenSet::from_tokens([API_TOKEN]).unwrap();
        let mut config = AppConfig::default();
        config.federation.proxy.enabled = true;
        config.federation.proxy.accept_local_resolve = true;
        config.federation.health_monitor_enabled = true;
        config.federation.proxy.local_resolve_token_file = Some(local);
        config.federation.approved_origins = vec![
            crate::config::ApprovedFederationOriginConfig {
                origin_id: "origin-a".to_owned(),
                descriptor_signing_keys: vec![],
                health_bearer_token_file: Some(health_a.clone()),
                data_bearer_token_file: Some(data_a.clone()),
            },
            crate::config::ApprovedFederationOriginConfig {
                origin_id: "origin-b".to_owned(),
                descriptor_signing_keys: vec![],
                health_bearer_token_file: None,
                data_bearer_token_file: Some(data_b.clone()),
            },
        ];
        assert!(load_federation_origin_tokens(&config, &api_tokens).is_ok());

        write_private_token(&data_b, "dddddddddddddddddddddddddddddddd");
        let error = load_federation_origin_tokens(&config, &api_tokens).unwrap_err();
        assert!(error.contains("overlap"));
        assert!(!error.contains("dddddddddddddddddddddddddddddddd"));

        write_private_token(&data_b, "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        write_private_token(&health_a, API_TOKEN);
        let error = load_federation_origin_tokens(&config, &api_tokens).unwrap_err();
        assert!(error.contains("overlap"));
        assert!(!error.contains(API_TOKEN));
    }

    #[test]
    fn production_quota_adapter_blocks_restart_retries_until_month_rollover() {
        let now = chrono::Utc::now().timestamp();
        let candidate = proxy_test_candidate(now);
        let request = proxy_test_request();
        let proxy_request = rw_federation_proxy::FederationProxyRequest {
            schema: rw_federation_proxy::FEDERATION_PROXY_SCHEMA.to_owned(),
            request: request.clone(),
            preferred_origin_id: None,
        };
        let limits = ProtocolLimits::default();
        let reserved = serde_json::to_vec(&request).unwrap().len() as u64
            + limits.max_manifest_bytes
            + candidate.descriptor.quotas.maximum_response_bytes;
        let directory = tempfile::tempdir().unwrap();
        let state_path = directory.path().join("federation-accounting.json");
        let accounting_limits = AccountingLimits {
            upload_bytes_per_month: reserved,
            download_bytes_per_month: reserved,
            promoted_bytes_per_month: reserved,
            concurrent_transfers: 1,
            maximum_principals: 2,
        };
        let month = Arc::new(AtomicU32::new(202608));
        let first_transport = AlwaysFailTransport::default();
        let first_ledger = QuotaLedger::open(&state_path, accounting_limits, 202608).unwrap();
        let first_quota = CommunityFederationQuota::with_month_source(first_ledger, {
            let month = month.clone();
            move || month.load(Ordering::SeqCst)
        });
        let first = proxy_with_production_quota(
            candidate.clone(),
            first_transport.clone(),
            first_quota,
            limits,
        );
        assert!(matches!(
            first.resolve(&hash('f'), &proxy_request),
            Err(FederationProxyError::Unavailable { attempts: 1 })
        ));
        assert_eq!(first_transport.0.load(Ordering::SeqCst), 1);
        drop(first);

        let restarted_transport = AlwaysFailTransport::default();
        let restarted_ledger = QuotaLedger::open(&state_path, accounting_limits, 202608).unwrap();
        let restarted_quota = CommunityFederationQuota::with_month_source(restarted_ledger, {
            let month = month.clone();
            move || month.load(Ordering::SeqCst)
        });
        let restarted = proxy_with_production_quota(
            candidate,
            restarted_transport.clone(),
            restarted_quota,
            limits,
        );
        assert!(matches!(
            restarted.resolve(&hash('f'), &proxy_request),
            Err(FederationProxyError::Quota)
        ));
        assert_eq!(restarted_transport.0.load(Ordering::SeqCst), 0);

        month.store(202609, Ordering::SeqCst);
        assert!(matches!(
            restarted.resolve(&hash('f'), &proxy_request),
            Err(FederationProxyError::Unavailable { attempts: 1 })
        ));
        assert_eq!(restarted_transport.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn runtime_kill_switch_is_authorized_atomic_durable_and_restart_safe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("federation-control.json");
        let operator = "a".repeat(64);
        let outsider = "b".repeat(64);
        let service = test_server_federation_proxy(&path, true, operator.clone());
        assert!(matches!(
            service.operator_status(&outsider),
            Err(FederationProxyControlError::Forbidden)
        ));
        assert!(service.operator_status(&operator).unwrap().kill_switch);
        assert!(matches!(
            service.set_kill_switch(
                &operator,
                FederationProxyKillSwitchRequest {
                    schema: "wrong".into(),
                    engaged: false,
                }
            ),
            Err(FederationProxyControlError::Invalid)
        ));
        let status = service
            .set_kill_switch(
                &operator,
                FederationProxyKillSwitchRequest {
                    schema: FEDERATION_PROXY_KILL_SWITCH_SCHEMA.into(),
                    engaged: false,
                },
            )
            .unwrap();
        assert!(!status.kill_switch);
        assert!(status.persistence_healthy);
        drop(service);

        let restarted = test_server_federation_proxy(&path, false, operator.clone());
        assert!(!restarted.startup_status().kill_switch);
        restarted
            .set_kill_switch(
                &operator,
                FederationProxyKillSwitchRequest {
                    schema: FEDERATION_PROXY_KILL_SWITCH_SCHEMA.into(),
                    engaged: true,
                },
            )
            .unwrap();
        drop(restarted);
        assert!(
            test_server_federation_proxy(&path, false, operator.clone())
                .startup_status()
                .kill_switch
        );

        // A safe configured kill always overrides a previously persisted
        // disengaged value at startup.
        let override_path = directory.path().join("override-control.json");
        let override_service =
            test_server_federation_proxy(&override_path, false, operator.clone());
        assert!(!override_service.startup_status().kill_switch);
        drop(override_service);
        assert!(
            test_server_federation_proxy(&override_path, true, operator)
                .startup_status()
                .kill_switch
        );
    }

    #[test]
    fn control_persistence_failure_and_tampered_state_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("federation-control.json");
        let operator = "a".repeat(64);
        let service = test_server_federation_proxy(&path, false, operator.clone());
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(matches!(
            service.set_kill_switch(
                &operator,
                FederationProxyKillSwitchRequest {
                    schema: FEDERATION_PROXY_KILL_SWITCH_SCHEMA.into(),
                    engaged: true,
                }
            ),
            Err(FederationProxyControlError::Persistence)
        ));
        let status = service.operator_status(&operator).unwrap();
        assert!(status.kill_switch);
        assert!(!status.persistence_healthy);

        let tampered = directory.path().join("tampered-control.json");
        fs::write(
            &tampered,
            br#"{"schema":"rw.server.federation-proxy-control.v1","kill_switch":false,"secret":"leak"}"#,
        )
        .unwrap();
        assert!(DurableFederationProxyControl::open(&tampered, false).is_err());
    }
}

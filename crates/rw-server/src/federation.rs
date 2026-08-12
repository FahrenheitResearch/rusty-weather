//! Operator-approved discovery and bounded failover for deliberately public
//! Rusty Weather origins. This module never discovers or represents ordinary
//! Community Cache clients.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use ed25519_dalek::SigningKey;
use rw_community_protocol::{
    FEDERATION_CATALOG_SCHEMA, FederationCatalog, FederationCoverageArea, FederationLimits,
    FederationQueryCapability, FederationTrustStore, ProtocolError, SignedFederationCatalog,
    SignedPublicOriginDescriptor, parse_signed_public_origin_descriptor_bounded,
    parse_verifying_key_base64, sign_federation_catalog, verify_signed_federation_catalog,
    verify_signed_public_origin_descriptor,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector as _, RustlsConnector, TcpConnector};

use crate::Metrics;
use crate::config::{ApprovedFederationOriginConfig, FederationConfig};

const MAX_SECRET_BYTES: u64 = 64 * 1024;
const MAX_HEALTH_STATE_BYTES: u64 = 256 * 1024;
const MAX_HEALTH_BODY_BYTES: u64 = 4 * 1024;
const MAX_DNS_ANSWERS: usize = 16;
const HEALTH_STATE_SCHEMA: &str = "rw.federation.health-state.v1";
const HEALTH_STATUS_SCHEMA: &str = "rw.federation.health-status.v1";

#[derive(Debug, Error)]
pub enum FederationError {
    #[error("public-origin federation is disabled")]
    Disabled,
    #[error("federated origin was not found")]
    NotFound,
    #[error("federation health state could not be persisted")]
    Persistence,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ProbeFailureKind {
    DnsRejected,
    Timeout,
    TlsOrNetwork,
    HttpStatus,
    WorkerFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeOutcome {
    Healthy,
    Failed(ProbeFailureKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FederationOriginHealthState {
    Unknown,
    Healthy,
    Degraded,
    Quarantined,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FederationOriginHealthStatus {
    pub origin_id: String,
    pub state: FederationOriginHealthState,
    pub consecutive_failures: u32,
    pub quarantine_until_unix: Option<i64>,
    pub last_probe_unix: Option<i64>,
    pub last_success_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FederationHealthStatus {
    pub schema: String,
    pub monitor_enabled: bool,
    pub total_origins: usize,
    pub healthy_origins: usize,
    pub degraded_origins: usize,
    pub quarantined_origins: usize,
    pub unknown_origins: usize,
    pub last_round_unix: Option<i64>,
    pub origins: Vec<FederationOriginHealthStatus>,
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct HealthRecord {
    consecutive_failures: u32,
    quarantine_until_unix: i64,
    last_probe_unix: Option<i64>,
    last_success_unix: Option<i64>,
    last_failure: Option<ProbeFailureKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DurableHealthState {
    schema: String,
    last_round_unix: Option<i64>,
    records: BTreeMap<String, HealthRecord>,
}

#[derive(Debug, Clone)]
struct HealthProbeTarget {
    origin_id: String,
    health_url: String,
    bearer_token: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct HealthProbeTimeouts {
    resolve: Duration,
    connect: Duration,
    send: Duration,
    receive: Duration,
    global: Duration,
}

#[derive(Debug, Clone)]
struct HealthMonitorSettings {
    enabled: bool,
    interval: Duration,
    concurrency: usize,
    state_file: Option<PathBuf>,
    timeouts: HealthProbeTimeouts,
}

trait HealthProbe: std::fmt::Debug + Send + Sync + 'static {
    fn probe(&self, target: &HealthProbeTarget, timeouts: HealthProbeTimeouts) -> ProbeOutcome;
}

#[derive(Debug)]
struct SystemHealthProbe {
    dns: BoundedDnsPool,
}

impl SystemHealthProbe {
    fn new(workers: usize) -> Self {
        Self {
            dns: BoundedDnsPool::new(workers),
        }
    }
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
    monitor: HealthMonitorSettings,
    probe_targets: BTreeMap<String, HealthProbeTarget>,
    health: Mutex<DurableHealthState>,
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

        let monitor = HealthMonitorSettings {
            enabled: config.health_monitor_enabled,
            interval: Duration::from_secs(config.health_probe_interval_seconds),
            concurrency: config.health_probe_concurrency,
            state_file: config.health_state_file.clone(),
            timeouts: HealthProbeTimeouts {
                resolve: Duration::from_secs(config.health_resolve_timeout_seconds),
                connect: Duration::from_secs(config.health_connect_timeout_seconds),
                send: Duration::from_secs(config.health_send_timeout_seconds),
                receive: Duration::from_secs(config.health_receive_timeout_seconds),
                global: Duration::from_secs(config.health_global_timeout_seconds),
            },
        };
        let mut probe_targets = BTreeMap::new();
        for (origin_id, signed) in &descriptors {
            let approved = config
                .approved_origins
                .iter()
                .find(|approved| approved.origin_id == *origin_id)
                .ok_or_else(|| {
                    FederationError::Invalid("approved origin mapping disappeared".into())
                })?;
            let bearer_token = if monitor.enabled {
                load_health_token(approved)?
            } else {
                None
            };
            probe_targets.insert(
                origin_id.clone(),
                HealthProbeTarget {
                    origin_id: origin_id.clone(),
                    health_url: format!(
                        "{}{}",
                        signed.descriptor.https_base_url, signed.descriptor.health_path
                    ),
                    bearer_token,
                },
            );
        }
        let health = load_health_state(monitor.state_file.as_deref(), &descriptor_ids)?;

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
            monitor,
            probe_targets,
            health: Mutex::new(health),
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

    pub fn health_status(&self) -> Result<FederationHealthStatus, FederationError> {
        self.health_status_at(now_unix())
    }

    /// Start the active public-origin monitor. Catalog federation and passive
    /// failover selection remain usable when this separately gated task is off.
    pub fn start_health_monitor(&self, metrics: Arc<Metrics>) -> Option<JoinHandle<()>> {
        let inner = self.inner.as_ref()?;
        let initial = self.health_status().ok()?;
        metrics.set_federation_health(&initial);
        if !inner.monitor.enabled {
            return None;
        }
        let service = self.clone();
        Some(tokio::spawn(async move {
            let workers = service
                .inner
                .as_ref()
                .map(|inner| inner.monitor.concurrency)
                .unwrap_or(1);
            let probe: Arc<dyn HealthProbe> = Arc::new(SystemHealthProbe::new(workers));
            let mut interval = tokio::time::interval(service.monitor_interval());
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                service
                    .run_health_round(probe.clone(), metrics.clone())
                    .await;
            }
        }))
    }

    fn monitor_interval(&self) -> Duration {
        self.inner
            .as_ref()
            .map(|inner| inner.monitor.interval)
            .unwrap_or(Duration::from_secs(60))
    }

    async fn run_health_round(&self, probe: Arc<dyn HealthProbe>, metrics: Arc<Metrics>) {
        let Some(inner) = self.inner.as_ref() else {
            return;
        };
        if !inner.monitor.enabled {
            return;
        }
        let semaphore = Arc::new(tokio::sync::Semaphore::new(inner.monitor.concurrency));
        let timeouts = inner.monitor.timeouts;
        let round_started = now_unix();
        let targets = inner
            .probe_targets
            .iter()
            .filter_map(|(origin_id, target)| {
                let descriptor = inner.descriptors.get(origin_id)?;
                verify_signed_public_origin_descriptor(
                    descriptor,
                    round_started,
                    &inner.trust,
                    &inner.limits,
                )
                .ok()
                .map(|()| target.clone())
            })
            .collect::<Vec<_>>();
        let mut tasks = tokio::task::JoinSet::new();
        for target in targets {
            let Ok(permit) = semaphore.clone().acquire_owned().await else {
                break;
            };
            let probe = probe.clone();
            tasks.spawn(async move {
                let origin_id = target.origin_id.clone();
                let outcome = match tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    probe.probe(&target, timeouts)
                })
                .await
                {
                    Ok(outcome) => outcome,
                    Err(_) => ProbeOutcome::Failed(ProbeFailureKind::WorkerFailure),
                };
                (origin_id, outcome)
            });
        }
        let observed_at = now_unix();
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok((origin_id, outcome)) => {
                    metrics.record_federation_probe(matches!(outcome, ProbeOutcome::Healthy));
                    if let Err(error) =
                        self.record_probe_outcome_at(&origin_id, outcome, observed_at)
                    {
                        warn!(origin_id, %error, "public-origin health observation was not retained");
                    }
                }
                Err(error) => warn!(%error, "public-origin health worker did not complete"),
            }
        }
        if let Err(error) = self.record_round_completed_at(observed_at) {
            warn!(%error, "public-origin health round could not be persisted");
        }
        if let Ok(status) = self.health_status_at(observed_at) {
            metrics.set_federation_health(&status);
            info!(
                healthy = status.healthy_origins,
                degraded = status.degraded_origins,
                quarantined = status.quarantined_origins,
                unknown = status.unknown_origins,
                "public-origin health round completed"
            );
        }
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
                .records
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
        order_and_bound_selected(&mut selected, inner.maximum_selection_results);
        Ok(selected)
    }

    fn record_health_at(
        &self,
        origin_id: &str,
        observation: FederationHealthObservation,
        now: i64,
    ) -> Result<(), FederationError> {
        let outcome = match observation {
            FederationHealthObservation::Healthy => ProbeOutcome::Healthy,
            FederationHealthObservation::Failed => {
                ProbeOutcome::Failed(ProbeFailureKind::TlsOrNetwork)
            }
        };
        self.record_probe_outcome_at(origin_id, outcome, now)
    }

    fn record_probe_outcome_at(
        &self,
        origin_id: &str,
        outcome: ProbeOutcome,
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
        let mut next = health.clone();
        let record = next.records.entry(origin_id.into()).or_default();
        record.last_probe_unix = Some(now);
        match outcome {
            ProbeOutcome::Healthy => {
                record.consecutive_failures = 0;
                record.quarantine_until_unix = 0;
                record.last_success_unix = Some(now);
                record.last_failure = None;
            }
            ProbeOutcome::Failed(kind) => {
                record.consecutive_failures = record.consecutive_failures.saturating_add(1);
                record.last_failure = Some(kind);
                if record.consecutive_failures >= inner.health_failure_threshold {
                    record.quarantine_until_unix =
                        now.saturating_add(inner.health_quarantine_seconds);
                }
            }
        }
        persist_health_state(inner.monitor.state_file.as_deref(), &next)?;
        *health = next;
        Ok(())
    }

    fn record_round_completed_at(&self, now: i64) -> Result<(), FederationError> {
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        let mut health = inner
            .health
            .lock()
            .map_err(|_| FederationError::Invalid("health lock is poisoned".into()))?;
        let mut next = health.clone();
        next.last_round_unix = Some(now);
        persist_health_state(inner.monitor.state_file.as_deref(), &next)?;
        *health = next;
        Ok(())
    }

    fn health_status_at(&self, now: i64) -> Result<FederationHealthStatus, FederationError> {
        let inner = self.inner.as_ref().ok_or(FederationError::Disabled)?;
        let health = inner
            .health
            .lock()
            .map_err(|_| FederationError::Invalid("health lock is poisoned".into()))?;
        let mut origins = Vec::with_capacity(inner.descriptors.len());
        let mut healthy_origins = 0usize;
        let mut degraded_origins = 0usize;
        let mut quarantined_origins = 0usize;
        let mut unknown_origins = 0usize;
        for origin_id in inner.descriptors.keys() {
            let record = health.records.get(origin_id).cloned().unwrap_or_default();
            let state = if now < record.quarantine_until_unix {
                quarantined_origins += 1;
                FederationOriginHealthState::Quarantined
            } else if record.last_probe_unix.is_none() {
                unknown_origins += 1;
                FederationOriginHealthState::Unknown
            } else if record.consecutive_failures == 0 {
                healthy_origins += 1;
                FederationOriginHealthState::Healthy
            } else {
                degraded_origins += 1;
                FederationOriginHealthState::Degraded
            };
            origins.push(FederationOriginHealthStatus {
                origin_id: origin_id.clone(),
                state,
                consecutive_failures: record.consecutive_failures,
                quarantine_until_unix: (record.quarantine_until_unix > now)
                    .then_some(record.quarantine_until_unix),
                last_probe_unix: record.last_probe_unix,
                last_success_unix: record.last_success_unix,
            });
        }
        Ok(FederationHealthStatus {
            schema: HEALTH_STATUS_SCHEMA.into(),
            monitor_enabled: inner.monitor.enabled,
            total_origins: origins.len(),
            healthy_origins,
            degraded_origins,
            quarantined_origins,
            unknown_origins,
            last_round_unix: health.last_round_unix,
            origins,
        })
    }
}

fn order_and_bound_selected(selected: &mut Vec<SelectedFederatedOrigin>, maximum: usize) {
    selected.sort_by(|a, b| {
        a.consecutive_failures
            .cmp(&b.consecutive_failures)
            .then_with(|| a.origin_id.cmp(&b.origin_id))
    });
    selected.truncate(maximum);
}

#[derive(Clone)]
struct BoundedDnsPool {
    senders: Arc<Vec<mpsc::SyncSender<DnsJob>>>,
    cursor: Arc<AtomicUsize>,
}

struct DnsJob {
    lookup: String,
    response: mpsc::SyncSender<std::io::Result<Vec<SocketAddr>>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DnsPoolError {
    Busy,
    Timeout,
    Disconnected,
    Io,
}

impl std::fmt::Debug for BoundedDnsPool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BoundedDnsPool")
            .field("workers", &self.senders.len())
            .finish_non_exhaustive()
    }
}

impl BoundedDnsPool {
    fn new(maximum_workers: usize) -> Self {
        let mut senders = Vec::with_capacity(maximum_workers);
        for worker in 0..maximum_workers {
            // One queued lookup per fixed worker avoids startup races while
            // preserving a strict pool-wide upper bound of 2 * workers.
            let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
            let spawned = thread::Builder::new()
                .name(format!("rw-federation-dns-{worker}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let result = job.lookup.to_socket_addrs().map(|addresses| {
                            addresses
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

    fn resolve(&self, lookup: String, timeout: Duration) -> Result<Vec<SocketAddr>, DnsPoolError> {
        if self.senders.is_empty() {
            return Err(DnsPoolError::Disconnected);
        }
        let (response, receiver) = mpsc::sync_channel(1);
        let mut job = Some(DnsJob { lookup, response });
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        let mut submitted = false;
        for offset in 0..self.senders.len() {
            let index = (start + offset) % self.senders.len();
            match self.senders[index].try_send(job.take().expect("DNS job retained")) {
                Ok(()) => {
                    submitted = true;
                    break;
                }
                Err(mpsc::TrySendError::Full(returned))
                | Err(mpsc::TrySendError::Disconnected(returned)) => job = Some(returned),
            }
        }
        if !submitted {
            return Err(DnsPoolError::Busy);
        }
        match receiver.recv_timeout(timeout) {
            Ok(Ok(addresses)) => Ok(addresses),
            Ok(Err(_)) => Err(DnsPoolError::Io),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DnsPoolError::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DnsPoolError::Disconnected),
        }
    }
}

#[derive(Debug, Clone)]
struct SafePinnedResolver {
    rejected_answer: Arc<Mutex<bool>>,
    dns: BoundedDnsPool,
}

impl SafePinnedResolver {
    fn rejected_answer(&self) -> bool {
        self.rejected_answer
            .lock()
            .map(|value| *value)
            .unwrap_or(true)
    }

    fn reject(&self) -> ureq::Error {
        if let Ok(mut rejected) = self.rejected_answer.lock() {
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
        if let Ok(mut rejected) = self.rejected_answer.lock() {
            *rejected = false;
        }
        if uri.scheme_str() != Some("https") {
            return Err(self.reject());
        }
        let host = uri.host().ok_or_else(|| self.reject())?.to_string();
        let port = uri.port_u16().unwrap_or(443);
        let lookup = format!("{host}:{port}");
        let addresses = self
            .dns
            .resolve(lookup, *timeout.after)
            .map_err(|error| match error {
                DnsPoolError::Timeout => ureq::Error::Timeout(timeout.reason),
                DnsPoolError::Busy | DnsPoolError::Disconnected | DnsPoolError::Io => {
                    ureq::Error::HostNotFound
                }
            })?;
        let selected = validate_and_pin_dns_answers(addresses).map_err(|()| self.reject())?;
        let mut result = self.empty();
        result.push(selected);
        Ok(result)
    }
}

impl HealthProbe for SystemHealthProbe {
    fn probe(&self, target: &HealthProbeTarget, timeouts: HealthProbeTimeouts) -> ProbeOutcome {
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let resolver = SafePinnedResolver {
            rejected_answer: Arc::new(Mutex::new(false)),
            dns: self.dns.clone(),
        };
        let resolver_status = resolver.clone();
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .https_only(true)
            .proxy(None)
            .max_redirects(0)
            .max_idle_connections(0)
            .timeout_global(Some(timeouts.global))
            .timeout_per_call(Some(timeouts.global))
            .timeout_resolve(Some(timeouts.resolve))
            .timeout_connect(Some(timeouts.connect))
            .timeout_send_request(Some(timeouts.send))
            .timeout_send_body(Some(timeouts.send))
            .timeout_recv_response(Some(timeouts.receive))
            .timeout_recv_body(Some(timeouts.receive))
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
        // A fresh agent is intentional: no pooled connection may bypass the
        // immediately preceding DNS policy check or its single pinned socket.
        let agent = ureq::Agent::with_parts(config, connector, resolver);
        let mut request = agent
            .get(&target.health_url)
            .header("accept", "application/json")
            .header("user-agent", "rusty-weather-federation-health/1");
        if let Some(token) = &target.bearer_token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let result = request.call().and_then(|mut response| {
            if !response.status().is_success() {
                return Err(ureq::Error::StatusCode(response.status().as_u16()));
            }
            response
                .body_mut()
                .with_config()
                .limit(MAX_HEALTH_BODY_BYTES)
                .read_to_vec()?;
            Ok(())
        });
        match result {
            Ok(()) => ProbeOutcome::Healthy,
            Err(_) if resolver_status.rejected_answer() => {
                ProbeOutcome::Failed(ProbeFailureKind::DnsRejected)
            }
            Err(ureq::Error::Timeout(_)) => ProbeOutcome::Failed(ProbeFailureKind::Timeout),
            Err(ureq::Error::StatusCode(_)) | Err(ureq::Error::TooManyRedirects) => {
                ProbeOutcome::Failed(ProbeFailureKind::HttpStatus)
            }
            Err(_) => ProbeOutcome::Failed(ProbeFailureKind::TlsOrNetwork),
        }
    }
}

fn validate_and_pin_dns_answers(mut addresses: Vec<SocketAddr>) -> Result<SocketAddr, ()> {
    if addresses.is_empty()
        || addresses.len() > MAX_DNS_ANSWERS
        || addresses.iter().any(|address| !is_global_ip(address.ip()))
    {
        return Err(());
    }
    addresses.sort_unstable();
    addresses.dedup();
    addresses.into_iter().next().ok_or(())
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
    let globally_routed = in_ipv6_prefix(
        value,
        u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
        3,
    );
    globally_routed
        && ![
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

fn load_health_token(
    approved: &ApprovedFederationOriginConfig,
) -> Result<Option<String>, FederationError> {
    let Some(path) = approved.health_bearer_token_file.as_deref() else {
        return Ok(None);
    };
    let token = read_secret(path)?;
    if token.len() > 8 * 1024
        || !token.is_ascii()
        || token
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(FederationError::Invalid(
            "federation health bearer token is malformed".into(),
        ));
    }
    Ok(Some(token))
}

fn load_health_state(
    path: Option<&Path>,
    expected_origins: &BTreeSet<String>,
) -> Result<DurableHealthState, FederationError> {
    let Some(path) = path else {
        return Ok(DurableHealthState {
            schema: HEALTH_STATE_SCHEMA.into(),
            last_round_unix: None,
            records: BTreeMap::new(),
        });
    };
    let bytes = match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink()
                || !metadata.file_type().is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_HEALTH_STATE_BYTES
            {
                return Err(FederationError::Invalid(
                    "federation health state must be a bounded regular file".into(),
                ));
            }
            fs::read(path)?
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(DurableHealthState {
                schema: HEALTH_STATE_SCHEMA.into(),
                last_round_unix: None,
                records: BTreeMap::new(),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let state: DurableHealthState = serde_json::from_slice(&bytes)?;
    if state.schema != HEALTH_STATE_SCHEMA
        || state.records.len() > expected_origins.len()
        || state.records.iter().any(|(origin_id, record)| {
            !expected_origins.contains(origin_id)
                || record.consecutive_failures > 1_000_000
                || record.quarantine_until_unix < 0
                || record.last_probe_unix.is_some_and(|time| time < 0)
                || record.last_success_unix.is_some_and(|time| time < 0)
        })
    {
        return Err(FederationError::Invalid(
            "federation health state is incompatible or malformed".into(),
        ));
    }
    Ok(state)
}

fn persist_health_state(
    path: Option<&Path>,
    state: &DurableHealthState,
) -> Result<(), FederationError> {
    let Some(path) = path else {
        return Ok(());
    };
    let bytes = serde_json::to_vec(state)?;
    if bytes.len() as u64 > MAX_HEALTH_STATE_BYTES {
        return Err(FederationError::Persistence);
    }
    rw_store::atomic::atomic_write_bytes(path, &bytes).map_err(|_| FederationError::Persistence)
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
        write_service_with(now, |_directory, _config| {})
    }

    fn write_service_with(
        now: i64,
        mutate: impl FnOnce(&tempfile::TempDir, &mut FederationConfig),
    ) -> (tempfile::TempDir, FederationService) {
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut config = FederationConfig {
            enabled: true,
            catalog_signing_key_file: Some(key_path),
            descriptor_files: vec![descriptor_path],
            approved_origins: vec![crate::config::ApprovedFederationOriginConfig {
                origin_id: "university-weather-lab".into(),
                descriptor_signing_keys: vec![crate::config::FederationTrustedKeyConfig {
                    key_id: "lab-2026-a".into(),
                    public_key_base64: encoded_origin_key,
                }],
                health_bearer_token_file: None,
                data_bearer_token_file: None,
            }],
            ..FederationConfig::default()
        };
        mutate(&directory, &mut config);
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
                health_bearer_token_file: None,
                data_bearer_token_file: None,
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
                health_bearer_token_file: None,
                data_bearer_token_file: None,
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

    #[test]
    fn dns_policy_rejects_rebinding_mixed_private_and_non_global_answers() {
        for answers in [
            vec![],
            vec![
                "8.8.8.8:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ],
            vec![
                "1.1.1.1:443".parse().unwrap(),
                "10.0.0.7:443".parse().unwrap(),
            ],
            vec!["192.0.2.1:443".parse().unwrap()],
            vec!["[2001:db8::1]:443".parse().unwrap()],
            vec!["[fc00::1]:443".parse().unwrap()],
            vec!["[fe80::1]:443".parse().unwrap()],
        ] {
            assert!(validate_and_pin_dns_answers(answers).is_err());
        }
        let pinned = validate_and_pin_dns_answers(vec![
            "[2606:4700:4700::1111]:443".parse().unwrap(),
            "1.1.1.1:443".parse().unwrap(),
        ])
        .unwrap();
        assert_eq!(pinned, "1.1.1.1:443".parse().unwrap());
    }

    #[test]
    fn all_special_use_address_classes_fail_closed() {
        for address in [
            "0.0.0.0",
            "10.2.3.4",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "64:ff9b::1",
            "2001:db8::1",
            "2001:20::1",
            "2002::1",
            "3fff::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
        ] {
            assert!(!is_global_ip(address.parse().unwrap()), "{address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_global_ip(address.parse().unwrap()), "{address}");
        }
    }

    #[test]
    fn timeout_failure_quarantines_and_a_success_recovers_immediately() {
        let now = now_unix();
        let (_directory, service) = write_service(now);
        for _ in 0..3 {
            service
                .record_probe_outcome_at(
                    "university-weather-lab",
                    ProbeOutcome::Failed(ProbeFailureKind::Timeout),
                    now,
                )
                .unwrap();
        }
        let status = service.health_status_at(now).unwrap();
        assert_eq!(status.quarantined_origins, 1);
        assert_eq!(
            status.origins[0].state,
            FederationOriginHealthState::Quarantined
        );
        service
            .record_probe_outcome_at("university-weather-lab", ProbeOutcome::Healthy, now + 1)
            .unwrap();
        let recovered = service.health_status_at(now + 1).unwrap();
        assert_eq!(recovered.healthy_origins, 1);
        assert_eq!(recovered.origins[0].consecutive_failures, 0);
        assert_eq!(recovered.origins[0].quarantine_until_unix, None);
    }

    #[test]
    fn durable_quarantine_survives_restart_without_endpoint_or_address_leakage() {
        let now = now_unix();
        let (directory, service) = write_service_with(now, |directory, config| {
            config.health_state_file = Some(directory.path().join("health.json"));
        });
        for _ in 0..3 {
            service
                .record_probe_outcome_at(
                    "university-weather-lab",
                    ProbeOutcome::Failed(ProbeFailureKind::DnsRejected),
                    now,
                )
                .unwrap();
        }
        let bytes = fs::read(directory.path().join("health.json")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("weather.example.edu"));
        assert!(!text.contains("https://"));
        assert!(!text.contains("127.0.0.1"));

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
                health_bearer_token_file: None,
                data_bearer_token_file: None,
            }],
            health_state_file: Some(directory.path().join("health.json")),
            ..FederationConfig::default()
        };
        let restarted = FederationService::open(&config).unwrap();
        assert_eq!(
            restarted.health_status_at(now).unwrap().quarantined_origins,
            1
        );
    }

    #[test]
    fn failover_order_is_exact_and_quarantine_advances_to_next_origin() {
        let now = now_unix();
        let (_directory, service) = write_service_with(now, |directory, config| {
            let bytes = fs::read(directory.path().join("lab.json")).unwrap();
            let signed =
                parse_signed_public_origin_descriptor_bounded(&bytes, &FederationLimits::default())
                    .unwrap();
            let mut descriptor = signed.descriptor;
            descriptor.origin_id = "alpha-lab".into();
            descriptor.display_name = "Alpha Lab".into();
            descriptor.https_base_url = "https://alpha.example.edu".into();
            descriptor.policy_links.attribution_url =
                "https://alpha.example.edu/attribution".into();
            descriptor.policy_links.acceptable_use_url = "https://alpha.example.edu/policy".into();
            descriptor.policy_links.privacy_url = "https://alpha.example.edu/privacy".into();
            let key = SigningKey::from_bytes(&[7; 32]);
            let alpha = sign_public_origin_descriptor(
                descriptor,
                "lab-2026-a",
                &key,
                &FederationLimits::default(),
            )
            .unwrap();
            let alpha_path = directory.path().join("alpha.json");
            fs::write(&alpha_path, serde_json::to_vec(&alpha).unwrap()).unwrap();
            config.descriptor_files.push(alpha_path);
            config
                .approved_origins
                .push(crate::config::ApprovedFederationOriginConfig {
                    origin_id: "alpha-lab".into(),
                    descriptor_signing_keys: vec![crate::config::FederationTrustedKeyConfig {
                        key_id: "lab-2026-a".into(),
                        public_key_base64: base64::engine::general_purpose::STANDARD
                            .encode(key.verifying_key().to_bytes()),
                    }],
                    health_bearer_token_file: None,
                    data_bearer_token_file: None,
                });
        });
        let request = FederationSelectionRequest {
            model: "hrrr".into(),
            product: "native".into(),
            query: FederationQueryCapability::ArbitraryDomainMap,
            bounds: None,
            minimum_response_bytes: 1024,
            require_replication: false,
        };
        let ordered = service.select_at(&request, now).unwrap();
        assert_eq!(
            ordered
                .iter()
                .map(|item| item.origin_id.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha-lab", "university-weather-lab"]
        );
        service
            .record_probe_outcome_at(
                "alpha-lab",
                ProbeOutcome::Failed(ProbeFailureKind::Timeout),
                now,
            )
            .unwrap();
        assert_eq!(
            service.select_at(&request, now).unwrap()[0].origin_id,
            "university-weather-lab"
        );
        for _ in 1..3 {
            service
                .record_probe_outcome_at(
                    "alpha-lab",
                    ProbeOutcome::Failed(ProbeFailureKind::Timeout),
                    now,
                )
                .unwrap();
        }
        let failover = service.select_at(&request, now).unwrap();
        assert_eq!(failover.len(), 1);
        assert_eq!(failover[0].origin_id, "university-weather-lab");
        service
            .record_probe_outcome_at("alpha-lab", ProbeOutcome::Healthy, now + 1)
            .unwrap();
        assert_eq!(
            service.select_at(&request, now + 1).unwrap()[0].origin_id,
            "alpha-lab"
        );
    }

    #[test]
    fn timed_out_dns_worker_pool_has_fixed_capacity_and_never_queues_more_work() {
        let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
        let (ready, ready_receiver) = mpsc::sync_channel(1);
        let (started, started_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            ready.send(()).unwrap();
            let job = receiver.recv().unwrap();
            started.send(()).unwrap();
            release_receiver.recv().unwrap();
            let _ = job.response.send(Ok(vec!["8.8.8.8:443".parse().unwrap()]));
        });
        let pool = BoundedDnsPool {
            senders: Arc::new(vec![sender]),
            cursor: Arc::new(AtomicUsize::new(0)),
        };
        ready_receiver.recv().unwrap();
        let first_pool = pool.clone();
        let first = thread::spawn(move || {
            first_pool.resolve("blocked.example:443".into(), Duration::from_millis(10))
        });
        started_receiver.recv().unwrap();
        assert_eq!(first.join().unwrap(), Err(DnsPoolError::Timeout));
        // The one bounded queue slot can accept one more lookup while the
        // fixed worker is stuck. A third cannot enqueue or spawn a replacement.
        let (queued_response, _queued_receiver) = mpsc::sync_channel(1);
        assert!(
            pool.senders[0]
                .try_send(DnsJob {
                    lookup: "127.0.0.1:443".into(),
                    response: queued_response,
                })
                .is_ok(),
            "the fixed worker's single bounded queue slot must accept exactly one waiting job"
        );
        assert_eq!(
            pool.resolve("third.example:443".into(), Duration::from_millis(10)),
            Err(DnsPoolError::Busy)
        );
        release.send(()).unwrap();
        worker.join().unwrap();
    }
}

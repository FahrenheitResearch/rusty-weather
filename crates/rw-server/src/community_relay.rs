//! Durable, authenticated Phase 2 broker for cold Community Cache objects.
//!
//! This service is intentionally absent from the operational local -> R2 ->
//! Hetzner path. It offers only exact-hash historical rendezvous and never a
//! seed directory, passive search feed, user identity, or direct candidate.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use chrono::{Datelike, Utc};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rw_community_protocol::{ProtocolLimits, RelayDirection, TrustedSigningKeys};
use rw_community_relay::{
    AdvertisementReceipt, AuthenticatedSubject, BillingPeriod, ClientRetrievalPolicy,
    ClientSeedingPolicy, ColdLookupOutcome, FallbackTarget, OsOpaqueIdSource,
    ParticipantCompletionResult, ParticipantRelayGrant, ParticipantRelayGrantWire, PromotionPolicy,
    PublicRelayFailure, RELAY_ADVERTISE_REQUEST_SCHEMA, RELAY_GRANT_POLL_SCHEMA,
    RELAY_HISTORICAL_LOOKUP_SCHEMA, RELAY_KILL_SWITCH_SCHEMA, RELAY_LOOKUP_RESPONSE_SCHEMA,
    RELAY_ROUTE_REGISTRATION_SCHEMA, RELAY_SESSION_COMPLETION_SCHEMA, RELAY_SESSION_FAILURE_SCHEMA,
    RELAY_SESSION_REVOCATION_SCHEMA, RELAY_STATUS_SCHEMA, RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA,
    RelayControlConfig, RelayCoordinator, RelayError, RelayProvider, RelayQuotaPolicy, RelayRole,
    RelayRoutePolicy, RelayRouteRegistrationReceipt, RelayRouteRegistry,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::community_relay_provider::{CloudflareProviderTimeouts, CloudflareRelayProvider};
use crate::config::{CommunityConfig, CommunityRelayConfig};

// Keep the HTTP route/OpenAPI surface source-compatible while making the
// relay crate the sole JSON wire authority shared by server and clients.
pub use rw_community_relay::{
    HistoricalRelayLookupRequest, HistoricalRelayLookupResponse, RelayAdvertiseRequest,
    RelayGrantPollRequest, RelayKillSwitchRequest, RelayRouteRegistrationRequest,
    RelaySessionCompletionRequest, RelaySessionFailureRequest, RelayStatusResponse,
    RelayTerminalResponse, RelayTransportGrantRequest,
};

const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_SECRET_BYTES: u64 = 64 * 1024;

#[derive(Debug, Error)]
pub enum CommunityRelayError {
    #[error("community relay is disabled")]
    Disabled,
    #[error("community relay request was not found")]
    NotFound,
    #[error("community relay authorization was rejected")]
    Forbidden,
    #[error("community relay request is invalid")]
    Invalid,
    #[error("community relay durable state failed")]
    Persistence,
    #[error(transparent)]
    Relay(#[from] RelayError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

struct BoxedRelayProvider(Box<dyn RelayProvider + Send>);

impl fmt::Debug for BoxedRelayProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BoxedRelayProvider([redacted])")
    }
}

impl RelayProvider for BoxedRelayProvider {
    fn issue(
        &mut self,
        request: &rw_community_relay::ProviderCredentialRequest,
        now_unix: i64,
    ) -> Result<rw_community_relay::ProviderCredentialLease, RelayError> {
        self.0.issue(request, now_unix)
    }

    fn revoke(&mut self, revocation_id: &rw_community_relay::SecretText) -> Result<(), RelayError> {
        self.0.revoke(revocation_id)
    }
}

type Coordinator = RelayCoordinator<BoxedRelayProvider, OsOpaqueIdSource>;

#[derive(Clone, Default)]
pub struct CommunityRelayService {
    inner: Option<Arc<Mutex<BrokerInner>>>,
}

impl fmt::Debug for CommunityRelayService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommunityRelayService")
            .field("enabled", &self.inner.is_some())
            .finish_non_exhaustive()
    }
}

struct BrokerInner {
    coordinator: Coordinator,
    routes: RelayRouteRegistry,
    route_policy: RelayRoutePolicy,
    state_file: PathBuf,
    archival_origin_available: bool,
    operator_principals: BTreeSet<String>,
    dispatches: BTreeMap<DispatchKey, DispatchGrant>,
    queues: BTreeMap<String, VecDeque<DispatchKey>>,
    persistence_healthy: bool,
    relay_sessions_issued: u64,
    relay_sessions_completed: u64,
    relay_sessions_failed: u64,
    promotion_signals: u64,
}

impl fmt::Debug for BrokerInner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BrokerInner")
            .field("persistence_healthy", &self.persistence_healthy)
            .field("active_dispatch_count", &self.dispatches.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct DispatchKey {
    session_id: String,
    role: u8,
}

impl DispatchKey {
    fn new(session_id: String, role: RelayRole) -> Self {
        Self {
            session_id,
            role: match role {
                RelayRole::Uploader => 1,
                RelayRole::Downloader => 2,
            },
        }
    }
}

struct DispatchGrant {
    subject: String,
    object_sha256: String,
    encoded_size: u64,
    grant: ParticipantRelayGrant,
}

impl fmt::Debug for DispatchGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DispatchGrant([redacted])")
    }
}

impl CommunityRelayService {
    pub fn open(
        community: &CommunityConfig,
        limits: ProtocolLimits,
    ) -> Result<Self, CommunityRelayError> {
        let config = &community.relay;
        if !config.enabled {
            return Ok(Self::default());
        }
        let origin_keys = load_origin_keys(community)?;
        let relay_signing_key = load_signing_key(
            config
                .signing_key_file
                .as_deref()
                .ok_or(CommunityRelayError::Invalid)?,
        )?;
        let provider_config = &config.cloudflare;
        let provider = CloudflareRelayProvider::open(
            provider_config.turn_key_id.clone(),
            provider_config
                .api_token_file
                .as_deref()
                .ok_or(CommunityRelayError::Invalid)?,
            &provider_config.allowed_turn_hosts,
            CloudflareProviderTimeouts {
                resolve: Duration::from_secs(provider_config.resolve_timeout_seconds),
                connect: Duration::from_secs(provider_config.connect_timeout_seconds),
                send: Duration::from_secs(provider_config.send_timeout_seconds),
                receive: Duration::from_secs(provider_config.receive_timeout_seconds),
                global: Duration::from_secs(provider_config.global_timeout_seconds),
            },
        )?;
        Self::with_provider(
            config,
            limits,
            origin_keys,
            relay_signing_key,
            BoxedRelayProvider(Box::new(provider)),
        )
    }

    fn with_provider(
        config: &CommunityRelayConfig,
        limits: ProtocolLimits,
        origin_keys: TrustedSigningKeys,
        relay_signing_key: SigningKey,
        provider: BoxedRelayProvider,
    ) -> Result<Self, CommunityRelayError> {
        let period = current_billing_period()?;
        let mut coordinator = RelayCoordinator::new(
            RelayControlConfig {
                phase2_enabled: config.enabled,
                security_tests_passed: config.security_tests_passed,
                capacity_audit_complete: config.capacity_audit_completed,
                provider_pricing_verified: config.provider_pricing_verified,
                relay_id: config.relay_id.clone(),
                signing_key_id: config.signing_key_id.clone(),
                credential_lifetime_seconds: i64::try_from(config.credential_lifetime_seconds)
                    .map_err(|_| CommunityRelayError::Invalid)?,
                max_chunk_plaintext_bytes: config.max_chunk_plaintext_bytes,
                quotas: RelayQuotaPolicy {
                    per_user_upload_bytes_per_month: config.quotas.per_user_upload_bytes_per_month,
                    per_user_download_bytes_per_month: config
                        .quotas
                        .per_user_download_bytes_per_month,
                    per_user_advertised_storage_bytes: config
                        .quotas
                        .per_user_advertised_storage_bytes,
                    per_user_concurrency: config.quotas.per_user_concurrency,
                    global_concurrency: config.quotas.global_concurrency,
                    global_relay_bytes_per_month: config.quotas.global_relay_bytes_per_month,
                    cost_stop_after_bytes_per_month: config.quotas.cost_stop_after_bytes_per_month,
                },
                promotion: PromotionPolicy {
                    successful_recoveries: config.promotion.successful_recoveries,
                    relayed_bytes: config.promotion.relayed_bytes,
                },
            },
            limits,
            origin_keys,
            relay_signing_key,
            provider,
            OsOpaqueIdSource,
            period,
        )?;
        prepare_state_file(&config.state_file)?;
        if config.state_file.exists() {
            let bytes = read_bounded(&config.state_file, MAX_STATE_BYTES)?;
            coordinator.restore_persistence_json(&bytes, now_unix(), period)?;
        }
        if config.kill_switch {
            coordinator.set_kill_switch(true);
        }
        let route_policy =
            RelayRoutePolicy::from_audited_cidrs(&config.cloudflare.audited_relay_cidrs)?;
        let routes = RelayRouteRegistry::new(route_policy.clone());
        let mut inner = BrokerInner {
            coordinator,
            routes,
            route_policy,
            state_file: config.state_file.clone(),
            archival_origin_available: config.archival_origin_available,
            operator_principals: config.operator_principals.iter().cloned().collect(),
            dispatches: BTreeMap::new(),
            queues: BTreeMap::new(),
            persistence_healthy: true,
            relay_sessions_issued: 0,
            relay_sessions_completed: 0,
            relay_sessions_failed: 0,
            promotion_signals: 0,
        };
        inner.persist()?;
        Ok(Self {
            inner: Some(Arc::new(Mutex::new(inner))),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_provider(
        config: &CommunityRelayConfig,
        limits: ProtocolLimits,
        origin_key_id: &str,
        origin_key: VerifyingKey,
        relay_signing_key: SigningKey,
        provider: Box<dyn RelayProvider + Send>,
    ) -> Result<Self, CommunityRelayError> {
        let origin_keys = TrustedSigningKeys::from([(origin_key_id.to_string(), origin_key)]);
        Self::with_provider(
            config,
            limits,
            origin_keys,
            relay_signing_key,
            BoxedRelayProvider(provider),
        )
    }

    pub fn advertise(
        &self,
        principal: &str,
        request: RelayAdvertiseRequest,
    ) -> Result<AdvertisementReceipt, CommunityRelayError> {
        if request.schema != RELAY_ADVERTISE_REQUEST_SCHEMA {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let policy = ClientSeedingPolicy {
            opted_in: request.opted_in,
            categories: request.categories,
            disk_allowance_bytes: request.disk_allowance_bytes,
            upload_allowance_bytes: request.upload_allowance_bytes,
            metered_network: request.metered_network,
            allow_metered_seeding: request.allow_metered_seeding,
        };
        let mut inner = self.lock()?;
        let (receipt, _audit) = inner.coordinator.advertise(
            subject,
            &request.signed_manifest,
            policy,
            now_unix(),
            current_billing_period()?,
        )?;
        inner.persist_or_kill()?;
        Ok(receipt)
    }

    pub fn historical_lookup_json(
        &self,
        principal: &str,
        request: HistoricalRelayLookupRequest,
    ) -> Result<(Vec<u8>, bool), CommunityRelayError> {
        if request.schema != RELAY_HISTORICAL_LOOKUP_SCHEMA || !request.historical {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let mut inner = self.lock()?;
        let archival = inner.archival_origin_available;
        let result = inner.coordinator.begin_cold_lookup(
            subject.clone(),
            ClientRetrievalPolicy {
                opted_in: request.opted_in,
                download_allowance_bytes: request.download_allowance_bytes,
            },
            &request.object_sha256,
            archival,
            now_unix(),
            current_billing_period()?,
        );
        match result {
            ColdLookupOutcome::Fallback(fallback) => {
                serde_json::to_vec(&HistoricalRelayLookupResponse {
                    schema: RELAY_LOOKUP_RESPONSE_SCHEMA.into(),
                    participant_grant: None,
                    fallback: Some(fallback),
                    fallback_after_relay_failure: None,
                })
                .map(|bytes| (bytes, false))
                .map_err(|_| CommunityRelayError::Invalid)
            }
            ColdLookupOutcome::Relay(grant) => {
                // Persist the new reservation/session before any credential is
                // placed in participant dispatch state or returned over HTTP.
                inner.persist_or_kill()?;
                inner.relay_sessions_issued = inner.relay_sessions_issued.saturating_add(1);
                let seed_subject = grant
                    .seed_dispatch_subject()
                    .expose_for_backend_dispatch()
                    .to_string();
                let session_id = grant.session_id.clone();
                let object_sha256 = grant.object_sha256.clone();
                let encoded_size = grant.encoded_size;
                let rw_community_relay::RelaySessionGrant {
                    upload, download, ..
                } = *grant;
                inner.insert_dispatch(
                    seed_subject,
                    session_id.clone(),
                    object_sha256.clone(),
                    encoded_size,
                    RelayRole::Uploader,
                    upload,
                );
                inner.insert_dispatch(
                    subject.expose_for_backend_dispatch().to_string(),
                    session_id.clone(),
                    object_sha256,
                    encoded_size,
                    RelayRole::Downloader,
                    download,
                );
                let key = DispatchKey::new(session_id, RelayRole::Downloader);
                let value = inner
                    .dispatches
                    .get(&key)
                    .ok_or(CommunityRelayError::Persistence)?
                    .grant_value()?;
                serde_json::to_vec(&HistoricalRelayLookupResponse {
                    schema: RELAY_LOOKUP_RESPONSE_SCHEMA.into(),
                    participant_grant: Some(value),
                    fallback: None,
                    fallback_after_relay_failure: Some(if archival {
                        FallbackTarget::ArchivalHttpsOrigin
                    } else {
                        FallbackTarget::Unavailable
                    }),
                })
                .map(|bytes| (bytes, true))
                .map_err(|_| CommunityRelayError::Invalid)
            }
        }
    }

    /// Return only the caller's oldest still-unregistered participant grant.
    /// There is no session list, seed list, requester identity, or peer state.
    pub fn next_grant_json(
        &self,
        principal: &str,
        request: RelayGrantPollRequest,
    ) -> Result<Vec<u8>, CommunityRelayError> {
        if request.schema != RELAY_GRANT_POLL_SCHEMA {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let relay_principal = subject.expose_for_backend_dispatch();
        let mut inner = self.lock()?;
        inner.compact_queue(relay_principal);
        let key = inner
            .queues
            .get(relay_principal)
            .and_then(|queue| queue.front())
            .cloned()
            .ok_or(CommunityRelayError::NotFound)?;
        let dispatch = inner
            .dispatches
            .get(&key)
            .filter(|dispatch| dispatch.subject == relay_principal)
            .ok_or(CommunityRelayError::NotFound)?;
        dispatch.grant_json()
    }

    pub fn grant_for_session_json(
        &self,
        principal: &str,
        session_id: &str,
        role: RelayRole,
    ) -> Result<Vec<u8>, CommunityRelayError> {
        let subject = authenticated_subject(principal)?;
        let relay_principal = subject.expose_for_backend_dispatch();
        let inner = self.lock()?;
        let key = DispatchKey::new(session_id.to_string(), role);
        let dispatch = inner
            .dispatches
            .get(&key)
            .filter(|dispatch| dispatch.subject == relay_principal)
            .ok_or(CommunityRelayError::NotFound)?;
        dispatch.grant_json()
    }

    pub fn register_route(
        &self,
        principal: &str,
        request: RelayRouteRegistrationRequest,
    ) -> Result<RelayRouteRegistrationReceipt, CommunityRelayError> {
        if request.schema != RELAY_ROUTE_REGISTRATION_SCHEMA {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let role = request.offer.role;
        let session_id = request.offer.session_id.clone();
        let mut inner = self.lock()?;
        let receipt = {
            let BrokerInner {
                coordinator,
                routes,
                ..
            } = &mut *inner;
            routes.register(
                coordinator,
                &subject,
                &request.credential,
                request.offer,
                &request.turn_local_addr,
                now_unix(),
            )?
        };
        inner.remove_dispatch(&DispatchKey::new(session_id, role));
        Ok(receipt)
    }

    pub fn transport_grant_json(
        &self,
        principal: &str,
        request: RelayTransportGrantRequest,
    ) -> Result<Vec<u8>, CommunityRelayError> {
        if request.schema != RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let mut inner = self.lock()?;
        let grant = {
            let BrokerInner {
                coordinator,
                routes,
                ..
            } = &mut *inner;
            routes.participant_grant(
                coordinator,
                &subject,
                &request.credential,
                request.role,
                now_unix(),
            )?
        };
        grant.transport_json().map_err(Into::into)
    }

    pub fn complete(
        &self,
        principal: &str,
        request: RelaySessionCompletionRequest,
    ) -> Result<(RelayTerminalResponse, Option<String>), CommunityRelayError> {
        let expected_direction = match request.role {
            RelayRole::Uploader => RelayDirection::Upload,
            RelayRole::Downloader => RelayDirection::Download,
        };
        if request.schema != RELAY_SESSION_COMPLETION_SCHEMA
            || request.credential.claims.direction != expected_direction
        {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let mut inner = self.lock()?;
        inner.coordinator.authorize_participant(
            &subject,
            &request.credential,
            request.role,
            now_unix(),
        )?;
        let session_id = request.credential.claims.session_id.clone();
        let result = inner.coordinator.report_participant_completion(
            &session_id,
            request.role,
            request.transferred_bytes,
            current_billing_period()?,
        )?;
        inner.persist_or_kill()?;
        let ParticipantCompletionResult::Complete(result) = result else {
            return Ok((
                RelayTerminalResponse {
                    fallback: None,
                    promotion_requested: false,
                    session_complete: false,
                },
                None,
            ));
        };
        inner.cleanup_session(&session_id);
        inner.relay_sessions_completed = inner.relay_sessions_completed.saturating_add(1);
        let promoted = result
            .promotion
            .as_ref()
            .map(|value| value.object_sha256.clone());
        if promoted.is_some() {
            inner.promotion_signals = inner.promotion_signals.saturating_add(1);
        }
        Ok((
            RelayTerminalResponse {
                fallback: None,
                promotion_requested: promoted.is_some(),
                session_complete: true,
            },
            promoted,
        ))
    }

    pub fn fail_or_revoke(
        &self,
        principal: &str,
        request: RelaySessionFailureRequest,
        revoke: bool,
    ) -> Result<RelayTerminalResponse, CommunityRelayError> {
        let expected_schema = if revoke {
            RELAY_SESSION_REVOCATION_SCHEMA
        } else {
            RELAY_SESSION_FAILURE_SCHEMA
        };
        if request.schema != expected_schema {
            return Err(CommunityRelayError::Invalid);
        }
        let subject = authenticated_subject(principal)?;
        let mut inner = self.lock()?;
        inner.coordinator.authorize_participant(
            &subject,
            &request.credential,
            request.role,
            now_unix(),
        )?;
        let session_id = request.credential.claims.session_id.clone();
        let fallback =
            inner
                .coordinator
                .fail_and_fallback(&session_id, 0, current_billing_period()?);
        inner.persist_or_kill()?;
        inner.cleanup_session(&session_id);
        inner.relay_sessions_failed = inner.relay_sessions_failed.saturating_add(1);
        Ok(RelayTerminalResponse {
            fallback: Some(fallback),
            promotion_requested: false,
            session_complete: false,
        })
    }

    pub fn set_kill_switch(
        &self,
        principal: &str,
        request: RelayKillSwitchRequest,
    ) -> Result<RelayStatusResponse, CommunityRelayError> {
        if request.schema != RELAY_KILL_SWITCH_SCHEMA {
            return Err(CommunityRelayError::Invalid);
        }
        authenticated_subject(principal)?;
        let mut inner = self.lock()?;
        if !inner.operator_principals.contains(principal) {
            return Err(CommunityRelayError::Forbidden);
        }
        inner.coordinator.set_kill_switch(request.enabled);
        if request.enabled {
            inner.routes = RelayRouteRegistry::new(RelayRoutePolicy::default());
            inner.dispatches.clear();
            inner.queues.clear();
        } else {
            inner.routes = RelayRouteRegistry::new(inner.route_policy.clone());
        }
        inner.persist_or_kill()?;
        Ok(inner.status())
    }

    pub fn status(&self, principal: &str) -> Result<RelayStatusResponse, CommunityRelayError> {
        authenticated_subject(principal)?;
        let inner = self.lock()?;
        if !inner.operator_principals.contains(principal) {
            return Err(CommunityRelayError::Forbidden);
        }
        Ok(inner.status())
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BrokerInner>, CommunityRelayError> {
        self.inner
            .as_ref()
            .ok_or(CommunityRelayError::Disabled)?
            .lock()
            .map_err(|_| CommunityRelayError::Persistence)
    }
}

impl BrokerInner {
    fn persist(&mut self) -> Result<(), CommunityRelayError> {
        let bytes = self.coordinator.export_persistence_json()?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_STATE_BYTES {
            self.persistence_healthy = false;
            return Err(CommunityRelayError::Persistence);
        }
        match rw_store::atomic::atomic_write_bytes(&self.state_file, &bytes) {
            Ok(()) => {
                self.persistence_healthy = true;
                Ok(())
            }
            Err(_) => {
                self.persistence_healthy = false;
                Err(CommunityRelayError::Persistence)
            }
        }
    }

    fn persist_or_kill(&mut self) -> Result<(), CommunityRelayError> {
        if self.persist().is_ok() {
            return Ok(());
        }
        // A grant is never returned from an uncommitted transition. Revoke all
        // live credentials, clear secret dispatch state, and make one best-
        // effort attempt to durably record the emergency stop.
        self.coordinator.set_kill_switch(true);
        self.routes = RelayRouteRegistry::new(RelayRoutePolicy::default());
        self.dispatches.clear();
        self.queues.clear();
        let _ = self.persist();
        Err(CommunityRelayError::Persistence)
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_dispatch(
        &mut self,
        subject: String,
        session_id: String,
        object_sha256: String,
        encoded_size: u64,
        role: RelayRole,
        grant: ParticipantRelayGrant,
    ) {
        let key = DispatchKey::new(session_id, role);
        self.dispatches.insert(
            key.clone(),
            DispatchGrant {
                subject: subject.clone(),
                object_sha256,
                encoded_size,
                grant,
            },
        );
        self.queues.entry(subject).or_default().push_back(key);
    }

    fn remove_dispatch(&mut self, key: &DispatchKey) {
        if let Some(dispatch) = self.dispatches.remove(key)
            && let Some(queue) = self.queues.get_mut(&dispatch.subject)
        {
            queue.retain(|candidate| candidate != key);
            if queue.is_empty() {
                self.queues.remove(&dispatch.subject);
            }
        }
    }

    fn compact_queue(&mut self, subject: &str) {
        if let Some(queue) = self.queues.get_mut(subject) {
            queue.retain(|key| {
                self.dispatches
                    .get(key)
                    .is_some_and(|dispatch| dispatch.subject == subject)
            });
            if queue.is_empty() {
                self.queues.remove(subject);
            }
        }
    }

    fn cleanup_session(&mut self, session_id: &str) {
        self.routes.remove_session(session_id);
        let keys = self
            .dispatches
            .keys()
            .filter(|key| key.session_id == session_id)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.remove_dispatch(&key);
        }
    }

    fn status(&self) -> RelayStatusResponse {
        RelayStatusResponse {
            schema: RELAY_STATUS_SCHEMA.into(),
            enabled: true,
            kill_switch: self.coordinator.kill_switch(),
            persistence_healthy: self.persistence_healthy,
            transport_route_gate_configured: !self.route_policy.is_empty(),
            sessions_issued: self.relay_sessions_issued,
            sessions_completed: self.relay_sessions_completed,
            sessions_failed: self.relay_sessions_failed,
            promotion_signals: self.promotion_signals,
        }
    }
}

impl DispatchGrant {
    fn grant_value(&self) -> Result<ParticipantRelayGrantWire, CommunityRelayError> {
        ParticipantRelayGrantWire::from_server_grant(
            self.object_sha256.clone(),
            self.encoded_size,
            &self.grant,
        )
        .map_err(Into::into)
    }

    fn grant_json(&self) -> Result<Vec<u8>, CommunityRelayError> {
        serde_json::to_vec(&self.grant_value()?).map_err(|_| CommunityRelayError::Invalid)
    }
}

fn authenticated_subject(principal: &str) -> Result<AuthenticatedSubject, CommunityRelayError> {
    if principal.len() != 64
        || !principal
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(CommunityRelayError::Forbidden);
    }
    let mut digest = Sha256::new();
    digest.update(b"rw-community-relay-principal-v1\0");
    digest.update(principal.as_bytes());
    AuthenticatedSubject::new(format!("{:x}", digest.finalize())).map_err(Into::into)
}

fn load_origin_keys(
    community: &CommunityConfig,
) -> Result<TrustedSigningKeys, CommunityRelayError> {
    let mut keys = TrustedSigningKeys::new();
    for value in &community.trusted_public_keys {
        let (key_id, encoded) = value.split_once(':').ok_or(CommunityRelayError::Invalid)?;
        if key_id.is_empty() || keys.contains_key(key_id) {
            return Err(CommunityRelayError::Invalid);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| CommunityRelayError::Invalid)?;
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| CommunityRelayError::Invalid)?;
        let key = VerifyingKey::from_bytes(&bytes).map_err(|_| CommunityRelayError::Invalid)?;
        if keys.values().any(|existing| existing == &key) {
            return Err(CommunityRelayError::Invalid);
        }
        keys.insert(key_id.to_string(), key);
    }
    if let Some(path) = community.signing_key_file.as_deref() {
        let key = load_signing_key(path)?;
        if keys.values().any(|trusted| trusted == &key.verifying_key()) {
            return Err(CommunityRelayError::Invalid);
        }
        if keys
            .insert(community.signing_key_id.clone(), key.verifying_key())
            .is_some()
        {
            return Err(CommunityRelayError::Invalid);
        }
    }
    if keys.is_empty() {
        return Err(CommunityRelayError::Invalid);
    }
    Ok(keys)
}

fn load_signing_key(path: &Path) -> Result<SigningKey, CommunityRelayError> {
    let secret = read_secret(path)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(secret)
        .map_err(|_| CommunityRelayError::Invalid)?;
    let bytes: [u8; 32] = bytes.try_into().map_err(|_| CommunityRelayError::Invalid)?;
    Ok(SigningKey::from_bytes(&bytes))
}

fn read_secret(path: &Path) -> Result<String, CommunityRelayError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_SECRET_BYTES
    {
        return Err(CommunityRelayError::Invalid);
    }
    validate_private_permissions(&metadata)?;
    let value = fs::read_to_string(path)?;
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(CommunityRelayError::Invalid);
    }
    Ok(value.to_string())
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &fs::Metadata) -> Result<(), CommunityRelayError> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(CommunityRelayError::Invalid);
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(_metadata: &fs::Metadata) -> Result<(), CommunityRelayError> {
    Ok(())
}

fn prepare_state_file(path: &Path) -> Result<(), CommunityRelayError> {
    let parent = path.parent().ok_or(CommunityRelayError::Invalid)?;
    fs::create_dir_all(parent)?;
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(CommunityRelayError::Invalid);
    }
    if path.exists() {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink()
            || !metadata.file_type().is_file()
            || metadata.len() > MAX_STATE_BYTES
        {
            return Err(CommunityRelayError::Invalid);
        }
    }
    Ok(())
}

fn read_bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, CommunityRelayError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() > maximum
    {
        return Err(CommunityRelayError::Invalid);
    }
    Ok(fs::read(path)?)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn current_billing_period() -> Result<BillingPeriod, CommunityRelayError> {
    let now = Utc::now();
    BillingPeriod::new(
        u16::try_from(now.year()).map_err(|_| CommunityRelayError::Invalid)?,
        u8::try_from(now.month()).map_err(|_| CommunityRelayError::Invalid)?,
    )
    .map_err(Into::into)
}

pub fn fallback_for_unavailable(archival: bool) -> PublicRelayFailure {
    PublicRelayFailure::new(
        RelayError::NotAvailable,
        if archival {
            FallbackTarget::ArchivalHttpsOrigin
        } else {
            FallbackTarget::Unavailable
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_origin_keyring_rejects_id_and_key_material_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let current_key = SigningKey::from_bytes(&[71; 32]);
        let key_path = directory.path().join("origin.key");
        fs::write(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode(current_key.to_bytes()),
        )
        .unwrap();
        let historical_key = SigningKey::from_bytes(&[72; 32]).verifying_key();
        let encoded_historical =
            base64::engine::general_purpose::STANDARD.encode(historical_key.to_bytes());
        let mut config = CommunityConfig {
            signing_key_file: Some(key_path),
            signing_key_id: "origin-v2".into(),
            trusted_public_keys: vec![format!("origin-v1:{encoded_historical}")],
            ..CommunityConfig::default()
        };
        assert_eq!(load_origin_keys(&config).unwrap().len(), 2);

        config.trusted_public_keys = vec![
            format!("old-a:{encoded_historical}"),
            format!("old-b:{encoded_historical}"),
        ];
        assert!(matches!(
            load_origin_keys(&config),
            Err(CommunityRelayError::Invalid)
        ));

        config.trusted_public_keys = vec![format!(
            "current-alias:{}",
            base64::engine::general_purpose::STANDARD
                .encode(current_key.verifying_key().to_bytes())
        )];
        assert!(matches!(
            load_origin_keys(&config),
            Err(CommunityRelayError::Invalid)
        ));
    }
}

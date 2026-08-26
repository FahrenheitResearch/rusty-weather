//! Closed client orchestration for explicit cold/historical Community Cache.
//!
//! Operational local -> R2 -> HTTPS origin delivery is structurally absent
//! from this module. Product code must invoke this client only after local and
//! R2 miss with an exact still-valid origin-signed manifest. Every public
//! failure is address/credential-free and returns only the ordered archival
//! HTTPS/unavailable decision.

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use rw_community_protocol::{
    ProtocolLimits, ShareRequest, SignedObjectManifest, TrustedSigningKeys, verify_signed_object,
};
use tokio::time::{sleep, timeout};

use crate::{
    AddressFamily, AdvertisementReceipt, EphemeralKeyPair, FallbackTarget,
    HistoricalRelayLookupRequest, INITIAL_RELAY_OBJECT_BYTES, NoopPacketEventSink,
    ParticipantRelayGrantWire, ProviderRelayAccess, RELAY_ADVERTISE_REQUEST_SCHEMA,
    RELAY_GRANT_POLL_SCHEMA, RELAY_HISTORICAL_LOOKUP_SCHEMA, RELAY_SESSION_COMPLETION_SCHEMA,
    RELAY_SESSION_FAILURE_SCHEMA, RELAY_SESSION_REVOCATION_SCHEMA,
    RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA, RelayAdvertiseRequest, RelayChunkPolicy, RelayError,
    RelayGrantPollRequest, RelayObjectCategory, RelayOnlyAllocation, RelayOnlyTurnClient,
    RelayReliabilityPolicy, RelayRole, RelayRoutePolicy, RelayRouteRegistrationReceipt,
    RelayRouteRegistrationRequest, RelaySender, RelaySessionCompletionRequest,
    RelaySessionFailureRequest, RelayTerminalResponse, RelayTransportGrantRequest,
    RelayTurnAccessWire, TokioRelayDnsResolver, after_operational_r2_miss,
    parse_historical_lookup_response_bounded, parse_polled_uploader_grant_bounded,
    parse_transport_route_bounded, resolve_supported_udp_endpoint, verify_origin_signed_identity,
};

const MAX_CONTROL_RESPONSE_BYTES: usize = 256 * 1024;

/// Injectable authenticated HTTPS control plane. Implementations must use
/// fixed method/path mappings, bounded bodies, no redirects, and no-store
/// semantics; raw provider errors and response bodies must never reach UI or
/// logs. Typed methods prevent callers from creating an arbitrary relay URL.
#[async_trait]
pub trait RelayBrokerHttp: Send + Sync {
    async fn historical_lookup(
        &self,
        request: RelayHistoricalLookupRequest,
    ) -> Result<Vec<u8>, RelayError>;

    async fn advertise(
        &self,
        request: RelayAdvertiseRequest,
    ) -> Result<AdvertisementReceipt, RelayError>;

    async fn next_grant(&self, request: RelayGrantPollRequest) -> Result<Vec<u8>, RelayError>;

    async fn register_route(
        &self,
        request: RelayRouteRegistrationRequest,
    ) -> Result<RelayRouteRegistrationReceipt, RelayError>;

    async fn transport_grant(
        &self,
        request: RelayTransportGrantRequest,
    ) -> Result<Vec<u8>, RelayError>;

    async fn complete(
        &self,
        request: RelaySessionCompletionRequest,
    ) -> Result<RelayTerminalResponse, RelayError>;

    async fn fail(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError>;

    async fn revoke(
        &self,
        request: RelaySessionFailureRequest,
    ) -> Result<RelayTerminalResponse, RelayError>;
}

/// Alias retained in method signatures to make the historical-only invariant
/// explicit at the call boundary.
pub type RelayHistoricalLookupRequest = HistoricalRelayLookupRequest;

#[async_trait]
pub trait RelayAllocationFactory: Send + Sync {
    async fn allocate(
        &self,
        turn: RelayTurnAccessWire,
        now_unix: i64,
        credential_expires_unix: i64,
    ) -> Result<RelayOnlyAllocation, RelayError>;
}

/// Production TURN/UDP allocator. It performs no ICE or STUN gathering,
/// resolves only the allowlisted TURN hostname from the signed grant, pins one
/// global server address, and exposes only the closed allocation wrapper.
#[derive(Debug, Clone, Copy)]
pub struct ProviderTurnAllocationFactory {
    pub family: AddressFamily,
}

#[async_trait]
impl RelayAllocationFactory for ProviderTurnAllocationFactory {
    async fn allocate(
        &self,
        turn: RelayTurnAccessWire,
        now_unix: i64,
        credential_expires_unix: i64,
    ) -> Result<RelayOnlyAllocation, RelayError> {
        let access =
            ProviderRelayAccess::from_broker_wire(turn, now_unix, credential_expires_unix)?;
        let pinned =
            resolve_supported_udp_endpoint(&access, &TokioRelayDnsResolver, self.family).await?;
        RelayOnlyTurnClient::new_udp(&access, pinned, Arc::new(NoopPacketEventSink))
            .await?
            .allocate()
            .await
    }
}

#[derive(Clone)]
pub struct HistoricalRelaySecurity {
    pub trusted_origin_keys: TrustedSigningKeys,
    pub trusted_relay_keys: TrustedSigningKeys,
    pub route_policy: RelayRoutePolicy,
    pub limits: ProtocolLimits,
}

impl fmt::Debug for HistoricalRelaySecurity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoricalRelaySecurity")
            .field("origin_key_count", &self.trusted_origin_keys.len())
            .field("relay_key_count", &self.trusted_relay_keys.len())
            .field("route_policy", &self.route_policy)
            .field("limits", &self.limits)
            .finish()
    }
}

#[derive(Debug, Clone, Copy)]
pub struct HistoricalRelayPolicy {
    pub opted_in: bool,
    pub seeding_opted_in: bool,
    pub metered_network: bool,
    pub allow_metered_seeding: bool,
    pub disk_allowance_bytes: u64,
    pub upload_allowance_bytes: u64,
    pub download_allowance_bytes: u64,
    pub route_poll_attempts: u8,
    pub route_poll_interval: Duration,
    pub session_timeout: Duration,
    pub reliability: RelayReliabilityPolicy,
}

impl Default for HistoricalRelayPolicy {
    fn default() -> Self {
        Self {
            opted_in: false,
            seeding_opted_in: false,
            metered_network: false,
            allow_metered_seeding: false,
            disk_allowance_bytes: 0,
            upload_allowance_bytes: 0,
            download_allowance_bytes: 0,
            route_poll_attempts: 30,
            route_poll_interval: Duration::from_millis(250),
            session_timeout: Duration::from_secs(60),
            reliability: RelayReliabilityPolicy::default(),
        }
    }
}

impl HistoricalRelayPolicy {
    fn retrieval_ready(self) -> Result<Self, RelayError> {
        if !self.opted_in
            || self.download_allowance_bytes == 0
            || !(1..=120).contains(&self.route_poll_attempts)
            || self.route_poll_interval.is_zero()
            || self.route_poll_interval > Duration::from_secs(2)
            || self.session_timeout < Duration::from_secs(5)
            || self.session_timeout > Duration::from_secs(15 * 60)
        {
            return Err(RelayError::PolicyDenied);
        }
        Ok(self)
    }

    fn seeding_ready(self) -> Result<Self, RelayError> {
        self.retrieval_ready()?;
        if !self.seeding_opted_in
            || self.disk_allowance_bytes == 0
            || self.upload_allowance_bytes == 0
            || self.metered_network && !self.allow_metered_seeding
        {
            return Err(if self.metered_network && !self.allow_metered_seeding {
                RelayError::MeteredNetworkPaused
            } else {
                RelayError::PolicyDenied
            });
        }
        Ok(self)
    }
}

pub trait RelayCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Debug, Default)]
pub struct NeverCancelled;

impl RelayCancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoricalRelayOutcome {
    Recovered(Vec<u8>),
    Fallback(FallbackTarget),
}

#[derive(Debug, Clone)]
pub struct VerifiedSeedObject {
    pub manifest: SignedObjectManifest,
    pub encoded: Vec<u8>,
}

pub trait VerifiedRelaySeedStore: Send + Sync {
    /// Exact CAS lookup only. Implementations must never interpret the hash as
    /// a filesystem path and must return only previously origin-verified cache
    /// objects, not private/arbitrary files.
    fn load_exact(&self, object_sha256: &str) -> Result<Option<VerifiedSeedObject>, RelayError>;
}

pub struct HistoricalRelayClient<B, A> {
    broker: B,
    allocations: A,
    security: HistoricalRelaySecurity,
    policy: HistoricalRelayPolicy,
}

impl<B, A> fmt::Debug for HistoricalRelayClient<B, A> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HistoricalRelayClient")
            .field("security", &self.security)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl<B, A> HistoricalRelayClient<B, A>
where
    B: RelayBrokerHttp,
    A: RelayAllocationFactory,
{
    pub fn new(
        broker: B,
        allocations: A,
        security: HistoricalRelaySecurity,
        policy: HistoricalRelayPolicy,
    ) -> Result<Self, RelayError> {
        if security.trusted_origin_keys.is_empty()
            || security.trusted_relay_keys.is_empty()
            || security.route_policy.is_empty()
        {
            return Err(RelayError::SecurityGate);
        }
        Ok(Self {
            broker,
            allocations,
            security,
            policy,
        })
    }

    /// Explicit cold-only recovery. The caller supplies the exact signed
    /// manifest retained after local/R2 miss; this method never resolves a
    /// current query, never invokes operational origin delivery, and never
    /// attempts direct connectivity.
    pub async fn recover_historical<C: RelayCancellation>(
        &self,
        request: &ShareRequest,
        signed_manifest: &SignedObjectManifest,
        now_unix: i64,
        cancellation: &C,
    ) -> Result<HistoricalRelayOutcome, RelayError> {
        let policy = self.policy.retrieval_ready()?;
        let verified = verify_origin_signed_identity(
            signed_manifest,
            now_unix,
            &self.security.trusted_origin_keys,
            &self.security.limits,
        )?;
        require_initial_transport_object(verified.category(), verified.encoded_size())?;
        if &signed_manifest.manifest.request != request {
            return Err(RelayError::UntrustedObject);
        }
        if cancellation.is_cancelled() {
            return Ok(HistoricalRelayOutcome::Fallback(
                FallbackTarget::Unavailable,
            ));
        }
        let lookup_bytes = self
            .broker
            .historical_lookup(HistoricalRelayLookupRequest {
                schema: RELAY_HISTORICAL_LOOKUP_SCHEMA.into(),
                historical: true,
                object_sha256: verified.object_sha256().into(),
                opted_in: true,
                download_allowance_bytes: policy.download_allowance_bytes,
            })
            .await?;
        require_control_size(&lookup_bytes)?;
        let response = parse_historical_lookup_response_bounded(&lookup_bytes)?;
        if let Some(fallback) = response.fallback {
            return Ok(HistoricalRelayOutcome::Fallback(fallback.fallback));
        }
        let fallback = response
            .fallback_after_relay_failure
            .ok_or(RelayError::CredentialInvalid)?;
        let grant = response
            .participant_grant
            .ok_or(RelayError::CredentialInvalid)?;
        if grant.encoded_size != signed_manifest.manifest.encoded_size {
            return Ok(HistoricalRelayOutcome::Fallback(fallback));
        }
        grant.validate(
            verified.object_sha256(),
            RelayRole::Downloader,
            now_unix,
            &self.security.trusted_relay_keys,
            &self.security.limits,
        )?;
        self.run_download_session(
            request,
            signed_manifest,
            grant,
            fallback,
            now_unix,
            cancellation,
        )
        .await
    }

    async fn run_download_session<C: RelayCancellation>(
        &self,
        request: &ShareRequest,
        signed_manifest: &SignedObjectManifest,
        grant: ParticipantRelayGrantWire,
        fallback: FallbackTarget,
        now_unix: i64,
        cancellation: &C,
    ) -> Result<HistoricalRelayOutcome, RelayError> {
        let credential = grant.credential.clone();
        let session_id = grant.session_id.clone();
        let object_sha256 = grant.object_sha256.clone();
        let encoded_size = grant.encoded_size;
        let key_pair = EphemeralKeyPair::generate();
        let offer = key_pair.offer(
            &credential,
            RelayRole::Downloader,
            now_unix,
            &self.security.limits,
        )?;
        let mut allocation = match self
            .allocations
            .allocate(grant.turn, now_unix, credential.claims.expires_unix)
            .await
        {
            Ok(value) => value,
            Err(_) => return Ok(HistoricalRelayOutcome::Fallback(fallback)),
        };
        let flow = async {
            let registration = allocation.route_registration_request(&credential, &offer)?;
            self.broker.register_route(registration).await?;
            let (route_wire, peer_route, binding) = self
                .wait_for_route(
                    &session_id,
                    RelayRole::Downloader,
                    &credential,
                    &object_sha256,
                    encoded_size,
                    now_unix,
                    cancellation,
                )
                .await?;
            allocation.bind_peer_route(peer_route)?;
            let key = key_pair.derive_session_key(&binding, RelayRole::Downloader)?;
            let receiver = crate::RelayReceiver::new(
                key,
                &binding,
                &credential.claims,
                encoded_size,
                RelayChunkPolicy::default(),
                self.security.limits,
            )?;
            // `turn` creates peer permissions lazily on `send_to`. Prime the
            // downloader allocation before waiting so the uploader's first
            // encrypted chunk can pass the TURN server's inbound permission
            // check. The E2E-authenticated marker may itself be discarded by
            // the reverse allocation; no direct address or fallback exists.
            allocation.prime_receive_permission(&receiver).await?;
            let broker = &self.broker;
            let origin_keys = &self.security.trusted_origin_keys;
            let limits = self.security.limits;
            let credential_for_completion = credential.clone();
            let encoded = allocation
                .receive_object_reliably_with_confirmation(
                    receiver,
                    self.policy.reliability,
                    |bytes| {
                        // Confirmation crosses an await at the broker. Keep a
                        // bounded owned copy rather than borrowing the
                        // receiver's internal buffer across that suspension.
                        let bytes = bytes.to_vec();
                        async move {
                            verify_signed_object(
                                signed_manifest,
                                request,
                                &bytes,
                                now_unix,
                                origin_keys,
                                &limits,
                            )
                            .map_err(|_| RelayError::UntrustedObject)?;
                            let terminal = broker
                                .complete(RelaySessionCompletionRequest {
                                    schema: RELAY_SESSION_COMPLETION_SCHEMA.into(),
                                    role: RelayRole::Downloader,
                                    credential: credential_for_completion,
                                    transferred_bytes: encoded_size,
                                })
                                .await?;
                            if terminal.fallback.is_some() {
                                return Err(RelayError::TransportUnavailable);
                            }
                            Ok(())
                        }
                    },
                )
                .await?;
            // The transport-private peer credential never crosses this task.
            drop(route_wire);
            Ok::<_, RelayError>(encoded)
        };
        let result = timeout(self.policy.session_timeout, flow).await;
        let _ = allocation.close().await;
        match result {
            Ok(Ok(encoded)) => Ok(HistoricalRelayOutcome::Recovered(encoded)),
            Ok(Err(_)) | Err(_) => {
                let _ = self
                    .broker
                    .fail(RelaySessionFailureRequest {
                        schema: RELAY_SESSION_FAILURE_SCHEMA.into(),
                        role: RelayRole::Downloader,
                        credential,
                    })
                    .await;
                Ok(HistoricalRelayOutcome::Fallback(fallback))
            }
        }
    }

    pub async fn advertise_verified(
        &self,
        object: &VerifiedSeedObject,
        now_unix: i64,
    ) -> Result<AdvertisementReceipt, RelayError> {
        let policy = self.policy.seeding_ready()?;
        let verified = verify_origin_signed_identity(
            &object.manifest,
            now_unix,
            &self.security.trusted_origin_keys,
            &self.security.limits,
        )?;
        require_initial_transport_object(verified.category(), verified.encoded_size())?;
        verify_signed_object(
            &object.manifest,
            &object.manifest.manifest.request,
            &object.encoded,
            now_unix,
            &self.security.trusted_origin_keys,
            &self.security.limits,
        )
        .map_err(|_| RelayError::UntrustedObject)?;
        self.broker
            .advertise(RelayAdvertiseRequest {
                schema: RELAY_ADVERTISE_REQUEST_SCHEMA.into(),
                signed_manifest: object.manifest.clone(),
                opted_in: true,
                categories: BTreeSet::from([verified.category()]),
                disk_allowance_bytes: policy.disk_allowance_bytes,
                upload_allowance_bytes: policy.upload_allowance_bytes,
                metered_network: policy.metered_network,
                allow_metered_seeding: policy.allow_metered_seeding,
            })
            .await
    }

    /// Poll and serve one caller-specific grant from an exact verified CAS.
    /// No seed directory, requester identity, arbitrary path, or peer address
    /// becomes visible to the application.
    pub async fn serve_one<S: VerifiedRelaySeedStore, C: RelayCancellation>(
        &self,
        store: &S,
        now_unix: i64,
        cancellation: &C,
    ) -> Result<bool, RelayError> {
        self.policy.seeding_ready()?;
        if cancellation.is_cancelled() {
            return Ok(false);
        }
        let grant_bytes = match self
            .broker
            .next_grant(RelayGrantPollRequest {
                schema: RELAY_GRANT_POLL_SCHEMA.into(),
            })
            .await
        {
            Ok(value) => value,
            Err(RelayError::NotAvailable) => return Ok(false),
            Err(error) => return Err(error),
        };
        require_control_size(&grant_bytes)?;
        let grant = parse_polled_uploader_grant_bounded(
            &grant_bytes,
            now_unix,
            &self.security.trusted_relay_keys,
            &self.security.limits,
        )?;
        let Some(object) = store.load_exact(&grant.object_sha256)? else {
            let _ = self
                .broker
                .revoke(RelaySessionFailureRequest {
                    schema: RELAY_SESSION_REVOCATION_SCHEMA.into(),
                    role: RelayRole::Uploader,
                    credential: grant.credential,
                })
                .await;
            return Ok(false);
        };
        // This re-verification is what removes expiry/key-removal eligibility:
        // no stale cached object can remain advertised or be uploaded.
        let verified = verify_origin_signed_identity(
            &object.manifest,
            now_unix,
            &self.security.trusted_origin_keys,
            &self.security.limits,
        )?;
        require_initial_transport_object(verified.category(), verified.encoded_size())?;
        verify_signed_object(
            &object.manifest,
            &object.manifest.manifest.request,
            &object.encoded,
            now_unix,
            &self.security.trusted_origin_keys,
            &self.security.limits,
        )
        .map_err(|_| RelayError::UntrustedObject)?;
        if object.manifest.manifest.object_sha256 != grant.object_sha256
            || object.encoded.len() as u64 != grant.encoded_size
        {
            return Err(RelayError::ObjectMismatch);
        }
        self.run_upload_session(grant, object.encoded, now_unix, cancellation)
            .await
    }

    async fn run_upload_session<C: RelayCancellation>(
        &self,
        grant: ParticipantRelayGrantWire,
        encoded: Vec<u8>,
        now_unix: i64,
        cancellation: &C,
    ) -> Result<bool, RelayError> {
        let credential = grant.credential.clone();
        let key_pair = EphemeralKeyPair::generate();
        let offer = key_pair.offer(
            &credential,
            RelayRole::Uploader,
            now_unix,
            &self.security.limits,
        )?;
        let mut allocation = match self
            .allocations
            .allocate(grant.turn, now_unix, credential.claims.expires_unix)
            .await
        {
            Ok(value) => value,
            Err(error) => return Err(error),
        };
        let flow = async {
            self.broker
                .register_route(allocation.route_registration_request(&credential, &offer)?)
                .await?;
            let (_wire, peer_route, binding) = self
                .wait_for_route(
                    &grant.session_id,
                    RelayRole::Uploader,
                    &credential,
                    &grant.object_sha256,
                    grant.encoded_size,
                    now_unix,
                    cancellation,
                )
                .await?;
            allocation.bind_peer_route(peer_route)?;
            let sender = RelaySender::new(
                key_pair.derive_session_key(&binding, RelayRole::Uploader)?,
                &binding,
                &credential.claims,
                grant.encoded_size,
                RelayChunkPolicy::default(),
                self.security.limits,
            )?;
            allocation
                .send_object_reliably(sender, &encoded, self.policy.reliability)
                .await?;
            let terminal = self
                .broker
                .complete(RelaySessionCompletionRequest {
                    schema: RELAY_SESSION_COMPLETION_SCHEMA.into(),
                    role: RelayRole::Uploader,
                    credential: credential.clone(),
                    transferred_bytes: grant.encoded_size,
                })
                .await?;
            if !terminal.session_complete || terminal.fallback.is_some() {
                return Err(RelayError::TransportUnavailable);
            }
            Ok::<_, RelayError>(())
        };
        let result = timeout(self.policy.session_timeout, flow).await;
        let _ = allocation.close().await;
        match result {
            Ok(Ok(())) => Ok(true),
            Ok(Err(error)) => {
                let _ = self
                    .broker
                    .fail(RelaySessionFailureRequest {
                        schema: RELAY_SESSION_FAILURE_SCHEMA.into(),
                        role: RelayRole::Uploader,
                        credential,
                    })
                    .await;
                Err(error)
            }
            Err(_) => {
                let _ = self
                    .broker
                    .fail(RelaySessionFailureRequest {
                        schema: RELAY_SESSION_FAILURE_SCHEMA.into(),
                        role: RelayRole::Uploader,
                        credential,
                    })
                    .await;
                Err(RelayError::TransportUnavailable)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn wait_for_route<C: RelayCancellation>(
        &self,
        session_id: &str,
        role: RelayRole,
        credential: &rw_community_protocol::SignedRelayCredential,
        object_sha256: &str,
        encoded_size: u64,
        now_unix: i64,
        cancellation: &C,
    ) -> Result<
        (
            crate::ParticipantTransportRouteGrantWire,
            crate::RelayAllocationRoute,
            crate::VerifiedSessionBinding,
        ),
        RelayError,
    > {
        for _ in 0..self.policy.route_poll_attempts {
            if cancellation.is_cancelled() {
                return Err(RelayError::TransportUnavailable);
            }
            match self
                .broker
                .transport_grant(RelayTransportGrantRequest {
                    schema: RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA.into(),
                    role,
                    credential: credential.clone(),
                })
                .await
            {
                Ok(bytes) => {
                    require_control_size(&bytes)?;
                    return parse_transport_route_bounded(
                        &bytes,
                        crate::TransportRouteExpectation {
                            session_id,
                            role,
                            own_credential: credential,
                            object_sha256,
                            encoded_size,
                            now_unix,
                            trusted_relay_keys: &self.security.trusted_relay_keys,
                            limits: &self.security.limits,
                            policy: &self.security.route_policy,
                        },
                    );
                }
                Err(RelayError::NotAvailable) => sleep(self.policy.route_poll_interval).await,
                Err(error) => return Err(error),
            }
        }
        Err(RelayError::TransportUnavailable)
    }
}

fn require_initial_transport_object(
    category: RelayObjectCategory,
    encoded_size: u64,
) -> Result<(), RelayError> {
    if !matches!(
        category,
        RelayObjectCategory::Profile | RelayObjectCategory::PointSeries
    ) || encoded_size == 0
        || encoded_size > INITIAL_RELAY_OBJECT_BYTES
    {
        return Err(RelayError::PolicyDenied);
    }
    Ok(())
}

fn require_control_size(bytes: &[u8]) -> Result<(), RelayError> {
    if bytes.is_empty() || bytes.len() > MAX_CONTROL_RESPONSE_BYTES {
        Err(RelayError::CredentialInvalid)
    } else {
        Ok(())
    }
}

// Compile-time guard: operational ordering remains a different enum and has
// no relay variant regardless of this module's availability.
const _: crate::OperationalFallback = after_operational_r2_miss();

#[cfg(test)]
mod tests {
    use std::any::Any;
    use std::collections::{BTreeMap, VecDeque};
    use std::io;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        Compression, DataOrigin, MissingPolicy, OBJECT_SCHEMA, ObjectManifest, PublicationGrant,
        REQUEST_SCHEMA, RecipeIdentity, ShareQuery, SourceProvenance, TimeWindow, object_sha256,
        request_sha256, sign_object_manifest,
    };
    use tokio::sync::{Mutex as TokioMutex, Notify};
    use webrtc_util::Conn;

    use super::*;
    use crate::{
        AuthenticatedSubject, BillingPeriod, ClientRetrievalPolicy, ClientSeedingPolicy,
        CloudflareTurnAdapter, ColdLookupOutcome, HistoricalRelayLookupResponse, OpaqueIdKind,
        OpaqueIdSource, ParticipantCompletionResult, PromotionPolicy, ProviderCredentialLease,
        ProviderCredentialRequest, RelayControlConfig, RelayCoordinator, RelayProvider,
        RelayQuotaPolicy, RelayRouteRegistry, SecretText,
    };

    const NOW: i64 = 1_800_000_000;

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

    struct TestProvider;

    impl RelayProvider for TestProvider {
        fn issue(
            &mut self,
            request: &ProviderCredentialRequest,
            now_unix: i64,
        ) -> Result<ProviderCredentialLease, RelayError> {
            request.validate(now_unix)?;
            let response = format!(
                r#"{{"iceServers":[{{"urls":"stun:stun.cloudflare.com:3478"}},{{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"{}","credential":"test-secret"}}]}}"#,
                request.participant_alias
            );
            CloudflareTurnAdapter::default().parse_and_sanitize(
                response.as_bytes(),
                now_unix,
                request.expires_unix,
            )
        }

        fn revoke(&mut self, _revocation_id: &SecretText) -> Result<(), RelayError> {
            Ok(())
        }
    }

    fn control_config() -> RelayControlConfig {
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
                per_user_upload_bytes_per_month: 1024 * 1024,
                per_user_download_bytes_per_month: 1024 * 1024,
                per_user_advertised_storage_bytes: 1024 * 1024,
                per_user_concurrency: 2,
                global_concurrency: 4,
                global_relay_bytes_per_month: 4 * 1024 * 1024,
                cost_stop_after_bytes_per_month: 4 * 1024 * 1024,
            },
            promotion: PromotionPolicy {
                successful_recoveries: 2,
                relayed_bytes: 1024 * 1024,
            },
        }
    }

    struct BrokerState {
        coordinator: RelayCoordinator<TestProvider, DeterministicIds>,
        routes: RelayRouteRegistry,
        uploader_grants: VecDeque<ParticipantRelayGrantWire>,
        advertisement_calls: usize,
    }

    struct TestBroker {
        state: Mutex<BrokerState>,
        seed_subject: AuthenticatedSubject,
        download_subject: AuthenticatedSubject,
        period: BillingPeriod,
        grant_ready: Notify,
    }

    impl TestBroker {
        fn new(
            origin_keys: TrustedSigningKeys,
            relay_signing_key: SigningKey,
            route_policy: RelayRoutePolicy,
        ) -> Self {
            let period = BillingPeriod::new(2027, 1).unwrap();
            let coordinator = RelayCoordinator::new(
                control_config(),
                ProtocolLimits::default(),
                origin_keys,
                relay_signing_key,
                TestProvider,
                DeterministicIds::default(),
                period,
            )
            .unwrap();
            Self {
                state: Mutex::new(BrokerState {
                    coordinator,
                    routes: RelayRouteRegistry::new(route_policy),
                    uploader_grants: VecDeque::new(),
                    advertisement_calls: 0,
                }),
                seed_subject: AuthenticatedSubject::new("seed-principal").unwrap(),
                download_subject: AuthenticatedSubject::new("download-principal").unwrap(),
                period,
                grant_ready: Notify::new(),
            }
        }

        fn subject_for(&self, role: RelayRole) -> &AuthenticatedSubject {
            match role {
                RelayRole::Uploader => &self.seed_subject,
                RelayRole::Downloader => &self.download_subject,
            }
        }
    }

    #[async_trait]
    impl RelayBrokerHttp for Arc<TestBroker> {
        async fn historical_lookup(
            &self,
            request: RelayHistoricalLookupRequest,
        ) -> Result<Vec<u8>, RelayError> {
            let outcome = {
                let mut state = self.state.lock().unwrap();
                state.coordinator.begin_cold_lookup(
                    self.download_subject.clone(),
                    ClientRetrievalPolicy {
                        opted_in: request.opted_in,
                        download_allowance_bytes: request.download_allowance_bytes,
                    },
                    &request.object_sha256,
                    true,
                    NOW,
                    self.period,
                )
            };
            let response = match outcome {
                ColdLookupOutcome::Fallback(fallback) => HistoricalRelayLookupResponse {
                    schema: crate::RELAY_LOOKUP_RESPONSE_SCHEMA.into(),
                    participant_grant: None,
                    fallback: Some(fallback),
                    fallback_after_relay_failure: None,
                },
                ColdLookupOutcome::Relay(grant) => {
                    let downloader = ParticipantRelayGrantWire::from_server_grant(
                        grant.object_sha256.clone(),
                        grant.encoded_size,
                        &grant.download,
                    )?;
                    let uploader = ParticipantRelayGrantWire::from_server_grant(
                        grant.object_sha256.clone(),
                        grant.encoded_size,
                        &grant.upload,
                    )?;
                    self.state
                        .lock()
                        .unwrap()
                        .uploader_grants
                        .push_back(uploader);
                    self.grant_ready.notify_waiters();
                    HistoricalRelayLookupResponse {
                        schema: crate::RELAY_LOOKUP_RESPONSE_SCHEMA.into(),
                        participant_grant: Some(downloader),
                        fallback: None,
                        fallback_after_relay_failure: Some(FallbackTarget::ArchivalHttpsOrigin),
                    }
                }
            };
            serde_json::to_vec(&response).map_err(|_| RelayError::CredentialInvalid)
        }

        async fn advertise(
            &self,
            request: RelayAdvertiseRequest,
        ) -> Result<AdvertisementReceipt, RelayError> {
            let mut state = self.state.lock().unwrap();
            state.advertisement_calls += 1;
            let (receipt, _) = state.coordinator.advertise(
                self.seed_subject.clone(),
                &request.signed_manifest,
                ClientSeedingPolicy {
                    opted_in: request.opted_in,
                    categories: request.categories,
                    disk_allowance_bytes: request.disk_allowance_bytes,
                    upload_allowance_bytes: request.upload_allowance_bytes,
                    metered_network: request.metered_network,
                    allow_metered_seeding: request.allow_metered_seeding,
                },
                NOW,
                self.period,
            )?;
            Ok(receipt)
        }

        async fn next_grant(&self, _request: RelayGrantPollRequest) -> Result<Vec<u8>, RelayError> {
            loop {
                let notified = self.grant_ready.notified();
                if let Some(grant) = self.state.lock().unwrap().uploader_grants.pop_front() {
                    return serde_json::to_vec(&grant).map_err(|_| RelayError::CredentialInvalid);
                }
                notified.await;
            }
        }

        async fn register_route(
            &self,
            request: RelayRouteRegistrationRequest,
        ) -> Result<RelayRouteRegistrationReceipt, RelayError> {
            let role = request.offer.role;
            let subject = self.subject_for(role).clone();
            let mut state = self.state.lock().unwrap();
            let BrokerState {
                coordinator,
                routes,
                ..
            } = &mut *state;
            routes.register(
                coordinator,
                &subject,
                &request.credential,
                request.offer,
                &request.turn_local_addr,
                NOW,
            )
        }

        async fn transport_grant(
            &self,
            request: RelayTransportGrantRequest,
        ) -> Result<Vec<u8>, RelayError> {
            let subject = self.subject_for(request.role).clone();
            let mut state = self.state.lock().unwrap();
            let BrokerState {
                coordinator,
                routes,
                ..
            } = &mut *state;
            routes
                .participant_grant(
                    coordinator,
                    &subject,
                    &request.credential,
                    request.role,
                    NOW,
                )?
                .transport_json()
        }

        async fn complete(
            &self,
            request: RelaySessionCompletionRequest,
        ) -> Result<RelayTerminalResponse, RelayError> {
            let subject = self.subject_for(request.role).clone();
            let mut state = self.state.lock().unwrap();
            state.coordinator.authorize_participant(
                &subject,
                &request.credential,
                request.role,
                NOW,
            )?;
            let result = state.coordinator.report_participant_completion(
                &request.credential.claims.session_id,
                request.role,
                request.transferred_bytes,
                self.period,
            )?;
            let (session_complete, promotion_requested) = match result {
                ParticipantCompletionResult::AwaitingCounterpart => (false, false),
                ParticipantCompletionResult::Complete(result) => (true, result.promotion.is_some()),
            };
            Ok(RelayTerminalResponse {
                fallback: None,
                promotion_requested,
                session_complete,
            })
        }

        async fn fail(
            &self,
            request: RelaySessionFailureRequest,
        ) -> Result<RelayTerminalResponse, RelayError> {
            let fallback = self.state.lock().unwrap().coordinator.fail_and_fallback(
                &request.credential.claims.session_id,
                0,
                self.period,
            );
            Ok(RelayTerminalResponse {
                fallback: Some(fallback),
                promotion_requested: false,
                session_complete: false,
            })
        }

        async fn revoke(
            &self,
            request: RelaySessionFailureRequest,
        ) -> Result<RelayTerminalResponse, RelayError> {
            self.fail(request).await
        }
    }

    struct LinkState {
        uploader: SocketAddr,
        downloader: SocketAddr,
        to_uploader: TokioMutex<VecDeque<(Vec<u8>, SocketAddr)>>,
        to_downloader: TokioMutex<VecDeque<(Vec<u8>, SocketAddr)>>,
        uploader_ready: Notify,
        downloader_ready: Notify,
    }

    #[derive(Clone)]
    struct LinkConn {
        local: SocketAddr,
        state: Arc<LinkState>,
    }

    impl fmt::Debug for LinkConn {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("LinkConn([redacted TURN test route])")
        }
    }

    #[async_trait]
    impl Conn for LinkConn {
        async fn connect(&self, _addr: SocketAddr) -> Result<(), webrtc_util::Error> {
            Err(io::Error::other("closed route").into())
        }

        async fn recv(&self, _buf: &mut [u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("closed route").into())
        }

        async fn recv_from(
            &self,
            buf: &mut [u8],
        ) -> Result<(usize, SocketAddr), webrtc_util::Error> {
            loop {
                let (queue, ready) = if self.local == self.state.uploader {
                    (&self.state.to_uploader, &self.state.uploader_ready)
                } else {
                    (&self.state.to_downloader, &self.state.downloader_ready)
                };
                let notified = ready.notified();
                if let Some((bytes, source)) = queue.lock().await.pop_front() {
                    if bytes.len() > buf.len() {
                        return Err(io::Error::other("oversize relay datagram").into());
                    }
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    return Ok((bytes.len(), source));
                }
                notified.await;
            }
        }

        async fn send(&self, _buf: &[u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("closed route").into())
        }

        async fn send_to(
            &self,
            buf: &[u8],
            target: SocketAddr,
        ) -> Result<usize, webrtc_util::Error> {
            let expected = if self.local == self.state.uploader {
                self.state.downloader
            } else {
                self.state.uploader
            };
            if target != expected {
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "closed route").into());
            }
            let (queue, ready) = if target == self.state.uploader {
                (&self.state.to_uploader, &self.state.uploader_ready)
            } else {
                (&self.state.to_downloader, &self.state.downloader_ready)
            };
            queue.lock().await.push_back((buf.to_vec(), self.local));
            ready.notify_one();
            Ok(buf.len())
        }

        fn local_addr(&self) -> Result<SocketAddr, webrtc_util::Error> {
            Ok(self.local)
        }

        fn remote_addr(&self) -> Option<SocketAddr> {
            None
        }

        async fn close(&self) -> Result<(), webrtc_util::Error> {
            Ok(())
        }

        fn as_any(&self) -> &(dyn Any + Send + Sync) {
            self
        }
    }

    fn allocation_pair() -> (RelayOnlyAllocation, RelayOnlyAllocation) {
        let uploader: SocketAddr = "104.16.0.10:50010".parse().unwrap();
        let downloader: SocketAddr = "104.16.0.11:50011".parse().unwrap();
        let state = Arc::new(LinkState {
            uploader,
            downloader,
            to_uploader: TokioMutex::new(VecDeque::new()),
            to_downloader: TokioMutex::new(VecDeque::new()),
            uploader_ready: Notify::new(),
            downloader_ready: Notify::new(),
        });
        (
            RelayOnlyAllocation::from_test_connection(
                LinkConn {
                    local: uploader,
                    state: Arc::clone(&state),
                },
                uploader,
            ),
            RelayOnlyAllocation::from_test_connection(
                LinkConn {
                    local: downloader,
                    state,
                },
                downloader,
            ),
        )
    }

    struct FixedAllocationFactory(Mutex<Option<RelayOnlyAllocation>>);

    #[async_trait]
    impl RelayAllocationFactory for FixedAllocationFactory {
        async fn allocate(
            &self,
            turn: RelayTurnAccessWire,
            now_unix: i64,
            credential_expires_unix: i64,
        ) -> Result<RelayOnlyAllocation, RelayError> {
            // Exercise the same strict broker-wire sanitation as production;
            // the test only replaces the provider socket implementation.
            drop(ProviderRelayAccess::from_broker_wire(
                turn,
                now_unix,
                credential_expires_unix,
            )?);
            self.0
                .lock()
                .unwrap()
                .take()
                .ok_or(RelayError::TransportUnavailable)
        }
    }

    #[derive(Clone)]
    struct TestStore(VerifiedSeedObject);

    impl VerifiedRelaySeedStore for TestStore {
        fn load_exact(
            &self,
            object_sha256: &str,
        ) -> Result<Option<VerifiedSeedObject>, RelayError> {
            Ok((self.0.manifest.manifest.object_sha256 == object_sha256).then(|| self.0.clone()))
        }
    }

    fn signed_point_object(signing: &SigningKey, expires_unix: i64) -> VerifiedSeedObject {
        let encoded = br#"{"temperature_k":[299.0,300.0]}"#.to_vec();
        let request = ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20270115T00Z".into(),
            snapshot_id: "a".repeat(64),
            grid_hash: "b".repeat(64),
            variables: vec!["temperature_2m".into()],
            query: ShareQuery::PointSeries {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                window: TimeWindow::Utc {
                    start_unix: NOW - 86_400,
                    end_unix: NOW - 82_800,
                },
                missing_policy: MissingPolicy::Strict,
            },
            recipe: RecipeIdentity {
                recipe_id: "native-point-series".into(),
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
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        }
        .normalized();
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request_sha256: request_sha256(&request).unwrap(),
            request,
            object_sha256: object_sha256(&encoded),
            content_type: "application/json".into(),
            compression: Compression::None,
            encoded_size: encoded.len() as u64,
            decoded_size: encoded.len() as u64,
            attributions: Vec::new(),
            modification_notices: vec!["Subset by Rusty Weather".into()],
            created_unix: NOW - 1_000,
            expires_unix,
        };
        VerifiedSeedObject {
            manifest: sign_object_manifest(manifest, "origin-signing", signing).unwrap(),
            encoded,
        }
    }

    fn client_policy() -> HistoricalRelayPolicy {
        HistoricalRelayPolicy {
            opted_in: true,
            seeding_opted_in: true,
            metered_network: false,
            allow_metered_seeding: false,
            disk_allowance_bytes: 1024 * 1024,
            upload_allowance_bytes: 1024 * 1024,
            download_allowance_bytes: 1024 * 1024,
            route_poll_attempts: 120,
            route_poll_interval: Duration::from_millis(1),
            session_timeout: Duration::from_secs(5),
            reliability: RelayReliabilityPolicy {
                max_data_attempts: 4,
                receive_timeout: Duration::from_millis(100),
                completion_repetitions: 3,
            },
        }
    }

    #[tokio::test]
    async fn cold_client_orchestration_is_exact_verified_and_broker_terminal() {
        let origin_signing = SigningKey::from_bytes(&[51; 32]);
        let relay_signing = SigningKey::from_bytes(&[52; 32]);
        let origin_keys =
            BTreeMap::from([("origin-signing".into(), origin_signing.verifying_key())]);
        let relay_keys = BTreeMap::from([("relay-signing".into(), relay_signing.verifying_key())]);
        let route_policy = RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let broker = Arc::new(TestBroker::new(
            origin_keys.clone(),
            relay_signing,
            route_policy.clone(),
        ));
        let object = signed_point_object(&origin_signing, NOW + 86_400);
        let store = TestStore(object.clone());
        let (upload_allocation, download_allocation) = allocation_pair();
        let security = HistoricalRelaySecurity {
            trusted_origin_keys: origin_keys,
            trusted_relay_keys: relay_keys,
            route_policy,
            limits: ProtocolLimits::default(),
        };
        let uploader = HistoricalRelayClient::new(
            Arc::clone(&broker),
            FixedAllocationFactory(Mutex::new(Some(upload_allocation))),
            security.clone(),
            client_policy(),
        )
        .unwrap();
        let downloader = HistoricalRelayClient::new(
            Arc::clone(&broker),
            FixedAllocationFactory(Mutex::new(Some(download_allocation))),
            security,
            client_policy(),
        )
        .unwrap();
        uploader.advertise_verified(&object, NOW).await.unwrap();

        let request = object.manifest.manifest.request.clone();
        let (download_result, upload_result) = tokio::join!(
            downloader.recover_historical(&request, &object.manifest, NOW, &NeverCancelled),
            uploader.serve_one(&store, NOW, &NeverCancelled),
        );
        assert_eq!(
            download_result.unwrap(),
            HistoricalRelayOutcome::Recovered(object.encoded.clone())
        );
        assert!(upload_result.unwrap());
        assert_eq!(broker.state.lock().unwrap().advertisement_calls, 1);
        let debug = format!("{downloader:?}");
        for forbidden in [
            "104.16.0",
            "test-secret",
            "seed-principal",
            "download-principal",
        ] {
            assert!(!debug.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn expired_seed_is_rejected_before_advertisement_or_transport() {
        let origin_signing = SigningKey::from_bytes(&[61; 32]);
        let relay_signing = SigningKey::from_bytes(&[62; 32]);
        let origin_keys =
            BTreeMap::from([("origin-signing".into(), origin_signing.verifying_key())]);
        let relay_keys = BTreeMap::from([("relay-signing".into(), relay_signing.verifying_key())]);
        let route_policy = RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let broker = Arc::new(TestBroker::new(
            origin_keys.clone(),
            relay_signing,
            route_policy.clone(),
        ));
        let (_, unused) = allocation_pair();
        let client = HistoricalRelayClient::new(
            Arc::clone(&broker),
            FixedAllocationFactory(Mutex::new(Some(unused))),
            HistoricalRelaySecurity {
                trusted_origin_keys: origin_keys,
                trusted_relay_keys: relay_keys,
                route_policy,
                limits: ProtocolLimits::default(),
            },
            client_policy(),
        )
        .unwrap();
        let expired = signed_point_object(&origin_signing, NOW);
        assert_eq!(
            client.advertise_verified(&expired, NOW).await,
            Err(RelayError::UntrustedObject)
        );
        assert_eq!(broker.state.lock().unwrap().advertisement_calls, 0);
    }
}

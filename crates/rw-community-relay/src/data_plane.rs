//! Concrete relay-only TURN data-plane seam.
//!
//! The selected `turn` client is used directly, without an ICE agent. Its
//! optional STUN address is left empty and its public Binding Request method is
//! never exposed. A destination-enforcing `Conn` wrapper rejects every socket
//! write that is not addressed to the DNS-pinned, operator-approved TURN
//! server. This prevents retransmission/background tasks inside the dependency
//! from escaping the same boundary.
//!
//! `turn` 0.17.2 supports UDP transport only: it creates UDP allocations and
//! has no TCP or TLS client transport. Consequently `turns:` and TURN/TCP
//! endpoints are not silently downgraded. They return
//! [`RelayError::TransportUnavailable`]. Cloudflare's official UDP 3478
//! endpoint is supported; TLS 5349/443 requires a separately audited client.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::net::UdpSocket;
use tokio::time::{Duration, timeout};
use turn::client::{Client, ClientConfig};
use webrtc_util::Conn;

use rand_core::OsRng;
use rw_community_protocol::{EncryptedRelayEnvelope, SignedRelayCredential, object_sha256};

use crate::{
    AuthenticatedRelayAck, EphemeralPublicOffer, ProviderRelayAccess, RELAY_ACK_SCHEMA,
    RELAY_ROUTE_REGISTRATION_SCHEMA, RelayAckKind, RelayAllocationRoute, RelayError, RelayReceiver,
    RelayRouteRegistrationRequest, RelaySender, TurnScheme, TurnTransport,
    validate_route_registration_request,
};

const MAX_RESOLVED_ADDRESSES: usize = 32;
pub const RELAY_DNS_TIMEOUT: Duration = Duration::from_secs(3);
pub const RELAY_CLIENT_START_TIMEOUT: Duration = Duration::from_secs(5);
pub const RELAY_ALLOCATION_TIMEOUT: Duration = Duration::from_secs(8);
pub const RELAY_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
pub const RELAY_DATAGRAM_TIMEOUT: Duration = Duration::from_secs(5);
/// Fits inside one UDP datagram after TURN channel framing. E2E plaintext
/// chunk policy must be chosen so its JSON/base64 envelope stays below this
/// bound; oversize payloads fail instead of IP-fragmenting unbounded data.
pub const MAX_RELAY_ENVELOPE_DATAGRAM_BYTES: usize = crate::MAX_RELAY_WIRE_DATAGRAM_BYTES;

/// `turn` 0.17.2 forwards the complete UDP datagram but returns the size of
/// its serialized TURN frame instead of the caller payload size. Accept only
/// the three exact complete-frame lengths that this pinned implementation can
/// produce: ChannelData, an IPv4 Send Indication, or an IPv6 Send Indication.
/// Every short, oversized, or otherwise unexplained report still fails closed.
fn turn_write_report_is_complete(payload_bytes: usize, reported_bytes: usize) -> bool {
    if reported_bytes == payload_bytes {
        return true;
    }
    let Some(padded_payload) = payload_bytes.checked_add(3).map(|value| value & !3) else {
        return false;
    };
    [
        // 4-byte ChannelData header.
        padded_payload.checked_add(4),
        // 20-byte STUN header + DATA attribute header + IPv4 XOR-PEER-ADDRESS
        // attribute + FINGERPRINT attribute.
        padded_payload.checked_add(44),
        // The same Send Indication with an IPv6 XOR-PEER-ADDRESS attribute.
        padded_payload.checked_add(56),
    ]
    .into_iter()
    .flatten()
    .any(|complete_frame| reported_bytes == complete_frame)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RelayReliabilityPolicy {
    pub max_data_attempts: u8,
    pub receive_timeout: Duration,
    pub completion_repetitions: u8,
}

impl Default for RelayReliabilityPolicy {
    fn default() -> Self {
        Self {
            max_data_attempts: 4,
            receive_timeout: RELAY_DATAGRAM_TIMEOUT,
            completion_repetitions: 3,
        }
    }
}

impl RelayReliabilityPolicy {
    fn validate(self) -> Result<Self, RelayError> {
        if !(1..=8).contains(&self.max_data_attempts)
            || self.receive_timeout.is_zero()
            || self.receive_timeout > RELAY_DATAGRAM_TIMEOUT
            || !(1..=8).contains(&self.completion_repetitions)
        {
            return Err(RelayError::PolicyDenied);
        }
        Ok(self)
    }
}

enum RelayInboundDatagram {
    Data(EncryptedRelayEnvelope),
    Acknowledgement(AuthenticatedRelayAck),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    Ipv4,
    Ipv6,
}

/// Injected DNS boundary. Production implementations may use a hardened
/// resolver; tests provide deterministic answers. Every resolved address is
/// validated and one exact socket is pinned before any network I/O.
#[async_trait]
pub trait RelayDnsResolver: Send + Sync {
    async fn resolve(
        &self,
        host: &str,
        port: u16,
        family: AddressFamily,
    ) -> Result<Vec<SocketAddr>, RelayError>;
}

#[derive(Debug, Default)]
pub struct TokioRelayDnsResolver;

#[async_trait]
impl RelayDnsResolver for TokioRelayDnsResolver {
    async fn resolve(
        &self,
        host: &str,
        port: u16,
        family: AddressFamily,
    ) -> Result<Vec<SocketAddr>, RelayError> {
        let mut addresses = tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| RelayError::DnsRejected)?
            .filter(|address| match family {
                AddressFamily::Ipv4 => address.is_ipv4(),
                AddressFamily::Ipv6 => address.is_ipv6(),
            })
            .take(MAX_RESOLVED_ADDRESSES + 1)
            .collect::<Vec<_>>();
        addresses.sort();
        addresses.dedup();
        if addresses.is_empty() || addresses.len() > MAX_RESOLVED_ADDRESSES {
            return Err(RelayError::DnsRejected);
        }
        Ok(addresses)
    }
}

/// Resolved, supported, operator-approved TURN endpoint. It is backend-local
/// transport state, not a signaling or app-visible DTO.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PinnedTurnEndpoint {
    socket_addr: SocketAddr,
}

impl fmt::Debug for PinnedTurnEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PinnedTurnEndpoint([operator TURN server])")
    }
}

impl PinnedTurnEndpoint {
    pub const fn socket_addr_for_dialer(self) -> SocketAddr {
        self.socket_addr
    }
}

pub async fn resolve_supported_udp_endpoint<R: RelayDnsResolver>(
    access: &ProviderRelayAccess,
    resolver: &R,
    family: AddressFamily,
) -> Result<PinnedTurnEndpoint, RelayError> {
    let endpoint = access
        .explicit_udp_endpoint()
        .ok_or(RelayError::TransportUnavailable)?;
    if endpoint.scheme() != TurnScheme::Turn || endpoint.transport() != Some(TurnTransport::Udp) {
        return Err(RelayError::TransportUnavailable);
    }
    let addresses = timeout(
        RELAY_DNS_TIMEOUT,
        resolver.resolve(endpoint.host(), endpoint.port(), family),
    )
    .await
    .map_err(|_| RelayError::TransportUnavailable)??;
    if addresses.is_empty()
        || addresses.len() > MAX_RESOLVED_ADDRESSES
        || addresses.iter().any(|address| {
            address.port() != endpoint.port()
                || match family {
                    AddressFamily::Ipv4 => !address.is_ipv4(),
                    AddressFamily::Ipv6 => !address.is_ipv6(),
                }
                || !is_global_relay_address(address.ip())
        })
    {
        return Err(RelayError::DnsRejected);
    }
    // Pin one deterministic answer for the lifetime of this allocation. We
    // reject the entire set if even one answer is unsafe rather than selecting
    // around a mixed/rebinding response.
    let socket_addr = *addresses.iter().min().ok_or(RelayError::DnsRejected)?;
    Ok(PinnedTurnEndpoint { socket_addr })
}

pub(crate) fn is_global_relay_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            !(address.is_unspecified()
                || address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_broadcast()
                || address.is_documentation()
                || address.is_multicast()
                || octets[0] >= 240
                || octets[0] == 0
                || octets[0] == 100 && (64..=127).contains(&octets[1])
                || octets[0] == 192 && octets[1] == 0 && octets[2] == 0
                || octets[0] == 192 && octets[1] == 88 && octets[2] == 99
                || octets[0] == 198 && matches!(octets[1], 18 | 19))
        }
        IpAddr::V6(address) => {
            let segments = address.segments();
            let mapped_is_global = address
                .to_ipv4_mapped()
                .is_none_or(|mapped| is_global_relay_address(IpAddr::V4(mapped)));
            // Conservatively admit only IPv6 global unicast (2000::/3), then
            // exclude current IANA documentation, protocol-assignment, and
            // deprecated transition ranges. Managed TURN anycast does not
            // need those special-purpose blocks.
            segments[0] & 0xe000 == 0x2000
                && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && segments[0] != 0x2002
                && segments[0] != 0x3fff
                && mapped_is_global
        }
    }
}

/// Test/observability event for a base-socket operation. This sink stays inside
/// the transport implementation; events must not be emitted to UI or ordinary
/// application logs because they contain the operator TURN server address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketEvent {
    Attempted { destination: SocketAddr },
    Rejected { destination: SocketAddr },
}

pub trait PacketEventSink: Send + Sync {
    fn observe(&self, event: PacketEvent);
}

#[derive(Debug, Default)]
pub struct NoopPacketEventSink;

impl PacketEventSink for NoopPacketEventSink {
    fn observe(&self, _event: PacketEvent) {}
}

/// Base UDP socket accepted by `turn` 0.17.2. All `send_to`, `connect`, and
/// connected `send` operations are pinned to exactly one operator TURN socket.
/// Any dependency bug or misuse that tries another destination fails locally.
pub struct DestinationPinnedUdpConn<S> {
    socket: UdpSocket,
    allowed: PinnedTurnEndpoint,
    sink: Arc<S>,
}

impl<S> fmt::Debug for DestinationPinnedUdpConn<S> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DestinationPinnedUdpConn([operator TURN only])")
    }
}

impl<S: PacketEventSink> DestinationPinnedUdpConn<S> {
    pub async fn bind(allowed: PinnedTurnEndpoint, sink: Arc<S>) -> Result<Self, RelayError> {
        let bind_address = if allowed.socket_addr.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED), 0)
        };
        let socket = UdpSocket::bind(bind_address)
            .await
            .map_err(|_| RelayError::TransportUnavailable)?;
        socket
            .connect(allowed.socket_addr)
            .await
            .map_err(|_| RelayError::TransportUnavailable)?;
        Ok(Self {
            socket,
            allowed,
            sink,
        })
    }

    fn require_allowed(&self, destination: SocketAddr) -> Result<(), webrtc_util::Error> {
        self.sink.observe(PacketEvent::Attempted { destination });
        if destination != self.allowed.socket_addr {
            self.sink.observe(PacketEvent::Rejected { destination });
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "relay destination rejected",
            )
            .into());
        }
        Ok(())
    }
}

#[async_trait]
impl<S: PacketEventSink + 'static> Conn for DestinationPinnedUdpConn<S> {
    async fn connect(&self, address: SocketAddr) -> Result<(), webrtc_util::Error> {
        self.require_allowed(address)?;
        self.socket.connect(address).await.map_err(Into::into)
    }

    async fn recv(&self, buffer: &mut [u8]) -> Result<usize, webrtc_util::Error> {
        self.socket.recv(buffer).await.map_err(Into::into)
    }

    async fn recv_from(
        &self,
        buffer: &mut [u8],
    ) -> Result<(usize, SocketAddr), webrtc_util::Error> {
        let (size, source) = self.socket.recv_from(buffer).await?;
        if source != self.allowed.socket_addr {
            return Err(
                io::Error::new(io::ErrorKind::PermissionDenied, "relay source rejected").into(),
            );
        }
        Ok((size, source))
    }

    async fn send(&self, buffer: &[u8]) -> Result<usize, webrtc_util::Error> {
        self.require_allowed(self.allowed.socket_addr)?;
        self.socket.send(buffer).await.map_err(Into::into)
    }

    async fn send_to(
        &self,
        buffer: &[u8],
        destination: SocketAddr,
    ) -> Result<usize, webrtc_util::Error> {
        self.require_allowed(destination)?;
        self.socket.send(buffer).await.map_err(Into::into)
    }

    fn local_addr(&self) -> Result<SocketAddr, webrtc_util::Error> {
        self.socket.local_addr().map_err(Into::into)
    }

    fn remote_addr(&self) -> Option<SocketAddr> {
        Some(self.allowed.socket_addr)
    }

    async fn close(&self) -> Result<(), webrtc_util::Error> {
        Ok(())
    }

    fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
        self
    }
}

/// Concrete `turn` 0.17.2 UDP allocation wrapper. It exposes no raw allocation
/// or packet connection publicly: application code must not be able to create
/// a TURN permission/channel bind to an arbitrary caller-supplied address. A
/// later backend-authorized opaque rendezvous transport can use the
/// crate-private allocator only after it binds the other participant's relay
/// route without exposing that socket through app state. The dependency's
/// Binding Request API is likewise never exposed.
pub struct RelayOnlyTurnClient {
    client: Client,
}

impl fmt::Debug for RelayOnlyTurnClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayOnlyTurnClient([redacted])")
    }
}

impl RelayOnlyTurnClient {
    pub async fn new_udp<S: PacketEventSink + 'static>(
        access: &ProviderRelayAccess,
        pinned: PinnedTurnEndpoint,
        sink: Arc<S>,
    ) -> Result<Self, RelayError> {
        let start = async {
            let connection = Arc::new(DestinationPinnedUdpConn::bind(pinned, sink).await?);
            let client = Client::new(ClientConfig {
                // Empty is deliberate. The dependency does not resolve or use
                // a STUN server and callers cannot invoke its Binding API
                // through this wrapper.
                stun_serv_addr: String::new(),
                // Pre-resolved literal prevents the dependency from performing
                // a second DNS lookup after global-address validation/pinning.
                turn_serv_addr: pinned.socket_addr.to_string(),
                username: access.username().expose().to_string(),
                password: access.credential().expose().to_string(),
                realm: String::new(),
                software: "BowEcho Community Cache".into(),
                rto_in_ms: 200,
                conn: connection,
                vnet: None,
            })
            .await
            .map_err(|_| RelayError::TransportUnavailable)?;
            client
                .listen()
                .await
                .map_err(|_| RelayError::TransportUnavailable)?;
            Ok::<_, RelayError>(client)
        };
        let client = timeout(RELAY_CLIENT_START_TIMEOUT, start)
            .await
            .map_err(|_| RelayError::TransportUnavailable)??;
        Ok(Self { client })
    }

    /// Allocate one TURN-only route and move the underlying client into a
    /// closed session wrapper. The wrapper exposes neither `Conn` nor an
    /// arbitrary destination API.
    pub async fn allocate(self) -> Result<RelayOnlyAllocation, RelayError> {
        let connection = timeout(RELAY_ALLOCATION_TIMEOUT, self.client.allocate())
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable)?;
        let own_allocation = connection
            .local_addr()
            .map_err(|_| RelayError::TransportUnavailable)?;
        if own_allocation.port() == 0 || !is_global_relay_address(own_allocation.ip()) {
            let _ = connection.close().await;
            let _ = self.client.close().await;
            return Err(RelayError::TransportUnavailable);
        }
        Ok(RelayOnlyAllocation {
            client: Some(self.client),
            connection: Box::new(connection),
            own_allocation,
            peer: None,
        })
    }

    pub async fn close(&self) -> Result<(), RelayError> {
        timeout(RELAY_CLOSE_TIMEOUT, self.client.close())
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable)
    }
}

/// One live provider TURN allocation. The only permitted destination is a
/// redacted [`RelayAllocationRoute`] returned by the participant-authenticated
/// broker after its session/role checks. No socket address, generic connection
/// or arbitrary byte-send API is exposed to product code.
pub struct RelayOnlyAllocation {
    client: Option<Client>,
    connection: Box<dyn Conn + Send + Sync>,
    own_allocation: SocketAddr,
    peer: Option<RelayAllocationRoute>,
}

impl fmt::Debug for RelayOnlyAllocation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayOnlyAllocation([redacted provider routes])")
    }
}

impl RelayOnlyAllocation {
    /// Construct the only request allowed to serialize this participant's own
    /// provider allocation. The request has redacted `Debug` and should be
    /// posted immediately with `Cache-Control: no-store` semantics.
    pub fn route_registration_request(
        &self,
        credential: &SignedRelayCredential,
        offer: &EphemeralPublicOffer,
    ) -> Result<RelayRouteRegistrationRequest, RelayError> {
        let request = RelayRouteRegistrationRequest {
            schema: RELAY_ROUTE_REGISTRATION_SCHEMA.into(),
            credential: credential.clone(),
            offer: offer.clone(),
            turn_local_addr: self.own_allocation.to_string(),
        };
        validate_route_registration_request(&request)?;
        Ok(request)
    }

    /// Bind the other participant's provider allocation exactly once. The
    /// route came from `parse_transport_route_bounded`; it cannot contain a
    /// host/srflx/prflx/direct candidate and has no public address accessor.
    pub fn bind_peer_route(&mut self, route: RelayAllocationRoute) -> Result<(), RelayError> {
        if self.peer.is_some() || route.socket_addr_for_relay_transport() == self.own_allocation {
            return Err(RelayError::Replay);
        }
        self.peer = Some(route);
        Ok(())
    }

    pub async fn send_envelope(&self, envelope: &EncryptedRelayEnvelope) -> Result<(), RelayError> {
        self.send_serialized(envelope).await
    }

    async fn send_ack(&self, acknowledgement: &AuthenticatedRelayAck) -> Result<(), RelayError> {
        self.send_serialized(acknowledgement).await
    }

    /// Establish the downloader allocation's lazy TURN permission before it
    /// waits for uploader traffic. The datagram is authenticated end to end
    /// and binds the opaque session and exact object. It may be discarded by
    /// the uploader's TURN allocation if the reverse permission does not yet
    /// exist; its required effect is the local CreatePermission side effect.
    pub async fn prime_receive_permission(
        &self,
        receiver: &RelayReceiver,
    ) -> Result<(), RelayError> {
        self.send_ack(&receiver.receiver_ready()?).await
    }

    async fn send_serialized<T: serde::Serialize>(&self, value: &T) -> Result<(), RelayError> {
        let destination = self
            .peer
            .ok_or(RelayError::KeyAgreementRejected)?
            .socket_addr_for_relay_transport();
        let bytes = serde_json::to_vec(value).map_err(|_| RelayError::EnvelopeRejected)?;
        if bytes.is_empty() || bytes.len() > MAX_RELAY_ENVELOPE_DATAGRAM_BYTES {
            return Err(RelayError::EnvelopeRejected);
        }
        let sent = timeout(
            RELAY_DATAGRAM_TIMEOUT,
            self.connection.send_to(&bytes, destination),
        )
        .await
        .map_err(|_| RelayError::TransportUnavailable)?
        .map_err(|_| RelayError::TransportUnavailable)?;
        if !turn_write_report_is_complete(bytes.len(), sent) {
            return Err(RelayError::TransportUnavailable);
        }
        Ok(())
    }

    pub async fn receive_envelope(&self) -> Result<EncryptedRelayEnvelope, RelayError> {
        match self
            .receive_datagram_with_timeout(RELAY_DATAGRAM_TIMEOUT)
            .await?
        {
            RelayInboundDatagram::Data(envelope) => Ok(envelope),
            RelayInboundDatagram::Acknowledgement(_) => Err(RelayError::EnvelopeRejected),
        }
    }

    async fn receive_datagram_with_timeout(
        &self,
        receive_timeout: Duration,
    ) -> Result<RelayInboundDatagram, RelayError> {
        let source = self
            .peer
            .ok_or(RelayError::KeyAgreementRejected)?
            .socket_addr_for_relay_transport();
        let mut bytes = vec![0_u8; MAX_RELAY_ENVELOPE_DATAGRAM_BYTES + 1];
        let (read, actual_source) = timeout(receive_timeout, self.connection.recv_from(&mut bytes))
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable)?;
        if actual_source != source || read == 0 || read > MAX_RELAY_ENVELOPE_DATAGRAM_BYTES {
            return Err(RelayError::TransportUnavailable);
        }
        bytes.truncate(read);
        if let Ok(envelope) = serde_json::from_slice::<EncryptedRelayEnvelope>(&bytes) {
            return Ok(RelayInboundDatagram::Data(envelope));
        }
        let acknowledgement: AuthenticatedRelayAck =
            serde_json::from_slice(&bytes).map_err(|_| RelayError::EnvelopeRejected)?;
        if acknowledgement.schema != RELAY_ACK_SCHEMA {
            return Err(RelayError::EnvelopeRejected);
        }
        Ok(RelayInboundDatagram::Acknowledgement(acknowledgement))
    }

    /// Send one exact object with bounded stop-and-wait retransmission. Each
    /// chunk is encrypted once and retransmitted byte-for-byte until its
    /// session/object/chunk-bound ACK authenticates. No alternate destination
    /// or transport can be selected by this algorithm.
    pub async fn send_object_reliably(
        &self,
        mut sender: RelaySender,
        object: &[u8],
        policy: RelayReliabilityPolicy,
    ) -> Result<(), RelayError> {
        let policy = policy.validate()?;
        if object_sha256(object) != sender_object_hash(&sender) {
            return Err(RelayError::ObjectMismatch);
        }
        let expected_chunks = sender.expected_chunk_count();
        let mut offset = 0_usize;
        let mut chunk_index = 0_u32;
        let mut receiver_ready_datagrams = 0_u8;
        while offset < object.len() {
            let chunk_size = usize::try_from(sender.next_plaintext_size()?)
                .map_err(|_| RelayError::EnvelopeRejected)?;
            let end = offset
                .checked_add(chunk_size)
                .filter(|end| *end <= object.len())
                .ok_or(RelayError::EnvelopeRejected)?;
            let envelope = sender.encrypt_next(&object[offset..end], &mut OsRng)?;
            let mut acknowledged = false;
            for _ in 0..policy.max_data_attempts {
                self.send_envelope(&envelope).await?;
                loop {
                    match self
                        .receive_datagram_with_timeout(policy.receive_timeout)
                        .await
                    {
                        Ok(RelayInboundDatagram::Acknowledgement(ack)) => {
                            if consume_receiver_ready(
                                &sender,
                                &ack,
                                &mut receiver_ready_datagrams,
                                policy.max_data_attempts,
                            )? {
                                continue;
                            }
                            if ack.chunk_index == chunk_index {
                                sender.verify_ack(&ack, chunk_index)?;
                                acknowledged = true;
                                break;
                            }
                            if chunk_index > 0 && ack.chunk_index == chunk_index - 1 {
                                sender.verify_ack(&ack, chunk_index - 1)?;
                                break;
                            }
                            return Err(RelayError::OutOfOrder);
                        }
                        Ok(RelayInboundDatagram::Data(_)) => return Err(RelayError::OutOfOrder),
                        Err(RelayError::TransportUnavailable) => break,
                        Err(error) => return Err(error),
                    }
                }
                if acknowledged {
                    break;
                }
            }
            if !acknowledged {
                return Err(RelayError::TransportUnavailable);
            }
            offset = end;
            chunk_index = chunk_index
                .checked_add(1)
                .ok_or(RelayError::EnvelopeRejected)?;
        }
        if chunk_index != expected_chunks {
            return Err(RelayError::ObjectMismatch);
        }
        let completion = sender.completion_confirmation()?;
        let mut receipt_verified = false;
        for _ in 0..policy.max_data_attempts {
            self.send_ack(&completion).await?;
            loop {
                match self
                    .receive_datagram_with_timeout(policy.receive_timeout)
                    .await
                {
                    Ok(RelayInboundDatagram::Acknowledgement(receipt)) => {
                        if consume_receiver_ready(
                            &sender,
                            &receipt,
                            &mut receiver_ready_datagrams,
                            policy.max_data_attempts,
                        )? {
                            continue;
                        }
                        sender.verify_transfer_receipt(&receipt)?;
                        receipt_verified = true;
                        break;
                    }
                    Ok(RelayInboundDatagram::Data(_)) => return Err(RelayError::OutOfOrder),
                    Err(RelayError::TransportUnavailable) => break,
                    Err(error) => return Err(error),
                }
            }
            if receipt_verified {
                break;
            }
        }
        if !receipt_verified {
            return Err(RelayError::TransportUnavailable);
        }
        sender.finish()
    }

    /// Receive one exact object. Only the immediately preceding identical
    /// authenticated ciphertext may repeat, solely to recover a lost ACK; it
    /// is re-authenticated and re-ACKed without appending twice. Completion is
    /// accepted only after exact byte count and SHA-256 validation.
    pub async fn receive_object_reliably(
        &self,
        receiver: RelayReceiver,
        policy: RelayReliabilityPolicy,
    ) -> Result<Vec<u8>, RelayError> {
        self.receive_object_reliably_with_confirmation(receiver, policy, |_| async { Ok(()) })
            .await
    }

    /// Variant used by the product orchestration: after exact byte count and
    /// SHA-256 validation but before the final data-plane receipt, run the
    /// downloader's bounded broker completion and normal origin-manifest
    /// verification. Thus receipt proves the downloader reached those gates,
    /// and the uploader can safely submit the broker's second-role report.
    pub async fn receive_object_reliably_with_confirmation<F, Fut>(
        &self,
        mut receiver: RelayReceiver,
        policy: RelayReliabilityPolicy,
        confirm: F,
    ) -> Result<Vec<u8>, RelayError>
    where
        F: FnOnce(&[u8]) -> Fut,
        Fut: std::future::Future<Output = Result<(), RelayError>>,
    {
        let policy = policy.validate()?;
        let mut idle_attempts = 0_u8;
        let max_datagrams = u64::from(receiver.expected_chunk_count())
            .saturating_mul(u64::from(policy.max_data_attempts))
            .saturating_add(u64::from(policy.completion_repetitions));
        let mut observed_datagrams = 0_u64;
        loop {
            let incoming = match self
                .receive_datagram_with_timeout(policy.receive_timeout)
                .await
            {
                Ok(incoming) => {
                    idle_attempts = 0;
                    incoming
                }
                Err(RelayError::TransportUnavailable)
                    if idle_attempts + 1 < policy.max_data_attempts =>
                {
                    idle_attempts += 1;
                    continue;
                }
                Err(error) => return Err(error),
            };
            observed_datagrams = observed_datagrams.saturating_add(1);
            if observed_datagrams > max_datagrams {
                return Err(RelayError::QuotaReached);
            }
            match incoming {
                RelayInboundDatagram::Data(envelope) => {
                    receiver.accept_reliable(&envelope)?;
                    let acknowledgement = receiver.acknowledgement(envelope.chunk_index)?;
                    self.send_ack(&acknowledgement).await?;
                }
                RelayInboundDatagram::Acknowledgement(completion) => {
                    receiver.verify_completion(&completion)?;
                    confirm(receiver.verified_bytes()?).await?;
                    let receipt = receiver.transfer_receipt()?;
                    for _ in 0..policy.completion_repetitions {
                        self.send_ack(&receipt).await?;
                    }
                    return receiver.finish();
                }
            }
        }
    }

    pub async fn close(self) -> Result<(), RelayError> {
        let allocation_result = timeout(RELAY_CLOSE_TIMEOUT, self.connection.close())
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable);
        let client_result = if let Some(client) = self.client {
            timeout(RELAY_CLOSE_TIMEOUT, client.close())
                .await
                .map_err(|_| RelayError::TransportUnavailable)?
                .map_err(|_| RelayError::TransportUnavailable)
        } else {
            Ok(())
        };
        allocation_result?;
        client_result
    }

    #[cfg(test)]
    pub(crate) fn from_test_connection<C>(connection: C, own_allocation: SocketAddr) -> Self
    where
        C: Conn + Send + Sync + 'static,
    {
        Self {
            client: None,
            connection: Box::new(connection),
            own_allocation,
            peer: None,
        }
    }
}

fn sender_object_hash(sender: &RelaySender) -> String {
    // The credential-scoped hash is intentionally available only as a copied
    // value, never a credential or session reference.
    sender.object_sha256().to_owned()
}

/// Verify and account for an idempotent downloader readiness marker while the
/// uploader awaits an ACK or receipt. A bounded number may race with data;
/// unauthenticated, cross-session, cross-object, wrong-kind, or excessive
/// markers fail closed.
fn consume_receiver_ready(
    sender: &RelaySender,
    acknowledgement: &AuthenticatedRelayAck,
    observed: &mut u8,
    maximum: u8,
) -> Result<bool, RelayError> {
    if acknowledgement.kind != RelayAckKind::ReceiverReady {
        return Ok(false);
    }
    sender.verify_receiver_ready(acknowledgement)?;
    *observed = observed.checked_add(1).ok_or(RelayError::QuotaReached)?;
    if *observed > maximum {
        return Err(RelayError::QuotaReached);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudflareTurnAdapter;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        EndToEndCipher, ProtocolLimits, RELAY_CREDENTIAL_SCHEMA, RELAY_ENVELOPE_SCHEMA,
        RelayCredentialClaims, RelayDirection, TrustedSigningKeys, object_sha256,
        sign_relay_credential,
    };
    use tokio::sync::{Mutex as TokioMutex, Notify};

    #[test]
    fn pinned_turn_frame_length_compatibility_remains_exact_and_bounded() {
        for payload in [1_usize, 512, 1_044, MAX_RELAY_ENVELOPE_DATAGRAM_BYTES] {
            let padded = (payload + 3) & !3;
            for reported in [payload, padded + 4, padded + 44, padded + 56] {
                assert!(
                    turn_write_report_is_complete(payload, reported),
                    "complete TURN report {reported} for {payload} payload bytes"
                );
            }
            for rejected in [
                0,
                payload.saturating_sub(1),
                padded + 3,
                padded + 5,
                padded + 43,
                padded + 45,
                padded + 55,
                padded + 57,
            ] {
                if ![payload, padded + 4, padded + 44, padded + 56].contains(&rejected) {
                    assert!(
                        !turn_write_report_is_complete(payload, rejected),
                        "unexplained TURN report {rejected} for {payload} payload bytes"
                    );
                }
            }
        }
        assert!(!turn_write_report_is_complete(usize::MAX, usize::MAX - 1));
    }

    #[derive(Debug)]
    struct FixedResolver(Vec<SocketAddr>);

    #[async_trait]
    impl RelayDnsResolver for FixedResolver {
        async fn resolve(
            &self,
            _host: &str,
            _port: u16,
            _family: AddressFamily,
        ) -> Result<Vec<SocketAddr>, RelayError> {
            Ok(self.0.clone())
        }
    }

    #[derive(Debug)]
    struct HangingResolver;

    #[async_trait]
    impl RelayDnsResolver for HangingResolver {
        async fn resolve(
            &self,
            _host: &str,
            _port: u16,
            _family: AddressFamily,
        ) -> Result<Vec<SocketAddr>, RelayError> {
            std::future::pending().await
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSink(StdMutex<Vec<PacketEvent>>);

    impl PacketEventSink for RecordingSink {
        fn observe(&self, event: PacketEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[derive(Clone)]
    struct FakeRelayConn {
        state: Arc<FakeRelayState>,
    }

    struct FakeRelayState {
        local: SocketAddr,
        sent: StdMutex<Vec<(Vec<u8>, SocketAddr)>>,
        incoming: StdMutex<VecDeque<(Vec<u8>, SocketAddr)>>,
    }

    #[derive(Clone)]
    struct FaultyLinkConn {
        local: SocketAddr,
        state: Arc<FaultyLinkState>,
    }

    struct FaultyLinkState {
        uploader: SocketAddr,
        downloader: SocketAddr,
        to_uploader: TokioMutex<VecDeque<(Vec<u8>, SocketAddr)>>,
        to_downloader: TokioMutex<VecDeque<(Vec<u8>, SocketAddr)>>,
        uploader_notify: Notify,
        downloader_notify: Notify,
        drop_first_data: AtomicBool,
        drop_first_ack: AtomicBool,
        drop_first_receipt: AtomicBool,
        uploader_permission: AtomicBool,
        downloader_permission: AtomicBool,
        dropped_for_missing_permission: AtomicUsize,
        delayed_receiver_ready: StdMutex<Option<Vec<u8>>>,
        flows: StdMutex<Vec<(SocketAddr, SocketAddr, String)>>,
    }

    impl FaultyLinkState {
        fn pair(uploader: SocketAddr, downloader: SocketAddr) -> (FaultyLinkConn, FaultyLinkConn) {
            let state = Arc::new(Self {
                uploader,
                downloader,
                to_uploader: TokioMutex::new(VecDeque::new()),
                to_downloader: TokioMutex::new(VecDeque::new()),
                uploader_notify: Notify::new(),
                downloader_notify: Notify::new(),
                drop_first_data: AtomicBool::new(true),
                drop_first_ack: AtomicBool::new(true),
                drop_first_receipt: AtomicBool::new(true),
                uploader_permission: AtomicBool::new(false),
                downloader_permission: AtomicBool::new(false),
                dropped_for_missing_permission: AtomicUsize::new(0),
                delayed_receiver_ready: StdMutex::new(None),
                flows: StdMutex::new(Vec::new()),
            });
            (
                FaultyLinkConn {
                    local: uploader,
                    state: Arc::clone(&state),
                },
                FaultyLinkConn {
                    local: downloader,
                    state,
                },
            )
        }
    }

    impl fmt::Debug for FaultyLinkConn {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("FaultyLinkConn([redacted TURN-only test link])")
        }
    }

    #[async_trait]
    impl Conn for FaultyLinkConn {
        async fn connect(&self, _addr: SocketAddr) -> Result<(), webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
        }

        async fn recv(&self, _buf: &mut [u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
        }

        async fn recv_from(
            &self,
            buf: &mut [u8],
        ) -> Result<(usize, SocketAddr), webrtc_util::Error> {
            loop {
                let (queue, notify) = if self.local == self.state.uploader {
                    (&self.state.to_uploader, &self.state.uploader_notify)
                } else {
                    (&self.state.to_downloader, &self.state.downloader_notify)
                };
                if let Some((bytes, source)) = queue.lock().await.pop_front() {
                    if bytes.len() > buf.len() {
                        return Err(io::Error::new(io::ErrorKind::InvalidData, "oversize").into());
                    }
                    buf[..bytes.len()].copy_from_slice(&bytes);
                    return Ok((bytes.len(), source));
                }
                notify.notified().await;
            }
        }

        async fn send(&self, _buf: &[u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
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
            let value: serde_json::Value = serde_json::from_slice(buf)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid frame"))?;
            let schema = value
                .get("schema")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("invalid")
                .to_owned();
            self.state
                .flows
                .lock()
                .unwrap()
                .push((self.local, target, schema.clone()));
            let source_permission = if self.local == self.state.uploader {
                &self.state.uploader_permission
            } else {
                &self.state.downloader_permission
            };
            source_permission.store(true, Ordering::SeqCst);
            let target_has_permission = if target == self.state.uploader {
                self.state.uploader_permission.load(Ordering::SeqCst)
            } else {
                self.state.downloader_permission.load(Ordering::SeqCst)
            };
            let is_receiver_ready = schema == RELAY_ACK_SCHEMA
                && value.get("kind").and_then(serde_json::Value::as_str) == Some("receiver_ready");
            if !target_has_permission {
                self.state
                    .dropped_for_missing_permission
                    .fetch_add(1, Ordering::SeqCst);
                if self.local == self.state.downloader && is_receiver_ready {
                    // Preserve one network duplicate so it can race the first
                    // data ACK after the uploader establishes its own
                    // permission. Production correctness does not depend on
                    // this duplicate being delivered.
                    *self.state.delayed_receiver_ready.lock().unwrap() = Some(buf.to_vec());
                }
                return Ok(buf.len());
            }
            if self.local == self.state.uploader {
                let delayed_ready = self.state.delayed_receiver_ready.lock().unwrap().take();
                if let Some(ready) = delayed_ready {
                    self.state
                        .to_uploader
                        .lock()
                        .await
                        .push_back((ready, self.state.downloader));
                    self.state.uploader_notify.notify_one();
                }
            }
            if self.local == self.state.uploader
                && schema == RELAY_ENVELOPE_SCHEMA
                && self.state.drop_first_data.swap(false, Ordering::SeqCst)
            {
                return Ok(buf.len());
            }
            let is_chunk_ack = schema == RELAY_ACK_SCHEMA
                && value.get("kind").and_then(serde_json::Value::as_str) == Some("chunk");
            if self.local == self.state.downloader
                && is_chunk_ack
                && self.state.drop_first_ack.swap(false, Ordering::SeqCst)
            {
                return Ok(buf.len());
            }
            let is_receipt = schema == RELAY_ACK_SCHEMA
                && value.get("kind").and_then(serde_json::Value::as_str)
                    == Some("transfer_receipt");
            if self.local == self.state.downloader
                && is_receipt
                && self.state.drop_first_receipt.swap(false, Ordering::SeqCst)
            {
                return Ok(buf.len());
            }
            if target == self.state.uploader {
                self.state
                    .to_uploader
                    .lock()
                    .await
                    .push_back((buf.to_vec(), self.local));
                self.state.uploader_notify.notify_one();
            } else {
                self.state
                    .to_downloader
                    .lock()
                    .await
                    .push_back((buf.to_vec(), self.local));
                self.state.downloader_notify.notify_one();
            }
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

        fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
            self
        }
    }

    impl FakeRelayConn {
        fn new(local: SocketAddr) -> Self {
            Self {
                state: Arc::new(FakeRelayState {
                    local,
                    sent: StdMutex::new(Vec::new()),
                    incoming: StdMutex::new(VecDeque::new()),
                }),
            }
        }

        fn push_incoming(&self, bytes: Vec<u8>, source: SocketAddr) {
            self.state
                .incoming
                .lock()
                .unwrap()
                .push_back((bytes, source));
        }
    }

    impl fmt::Debug for FakeRelayConn {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("FakeRelayConn([redacted])")
        }
    }

    #[async_trait]
    impl Conn for FakeRelayConn {
        async fn connect(&self, _addr: SocketAddr) -> Result<(), webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
        }

        async fn recv(&self, _buf: &mut [u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
        }

        async fn recv_from(
            &self,
            buf: &mut [u8],
        ) -> Result<(usize, SocketAddr), webrtc_util::Error> {
            let (bytes, source) = self
                .state
                .incoming
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "no packet"))?;
            if bytes.len() > buf.len() {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "oversize").into());
            }
            buf[..bytes.len()].copy_from_slice(&bytes);
            Ok((bytes.len(), source))
        }

        async fn send(&self, _buf: &[u8]) -> Result<usize, webrtc_util::Error> {
            Err(io::Error::other("not supported").into())
        }

        async fn send_to(
            &self,
            buf: &[u8],
            target: SocketAddr,
        ) -> Result<usize, webrtc_util::Error> {
            self.state.sent.lock().unwrap().push((buf.to_vec(), target));
            Ok(buf.len())
        }

        fn local_addr(&self) -> Result<SocketAddr, webrtc_util::Error> {
            Ok(self.state.local)
        }

        fn remote_addr(&self) -> Option<SocketAddr> {
            None
        }

        async fn close(&self) -> Result<(), webrtc_util::Error> {
            Ok(())
        }

        fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) {
            self
        }
    }

    fn access(json: &[u8]) -> ProviderRelayAccess {
        CloudflareTurnAdapter::default()
            .parse_and_sanitize(json, 100, 700)
            .unwrap()
            .access
    }

    #[tokio::test]
    async fn endpoint_resolution_rejects_private_special_and_documentation_addresses() {
        let access = access(
            br#"{"iceServers":[{"urls":"turn:turn.cloudflare.com:3478?transport=udp","username":"u","credential":"c"}]}"#,
        );
        for address in [
            "127.0.0.1:3478",
            "10.0.0.1:3478",
            "169.254.1.1:3478",
            "192.0.2.1:3478",
            "198.51.100.1:3478",
            "203.0.113.1:3478",
            "224.0.0.1:3478",
        ] {
            let resolver = FixedResolver(vec![address.parse().unwrap()]);
            assert_eq!(
                resolve_supported_udp_endpoint(&access, &resolver, AddressFamily::Ipv4).await,
                Err(RelayError::DnsRejected)
            );
        }
    }

    #[tokio::test]
    async fn endpoint_resolution_rejects_mixed_wrong_port_and_wrong_family_sets() {
        let access = access(
            br#"{"iceServers":[{"urls":"turn:turn.cloudflare.com:3478?transport=udp","username":"u","credential":"c"}]}"#,
        );
        for addresses in [
            vec![
                "1.1.1.1:3478".parse().unwrap(),
                "10.0.0.1:3478".parse().unwrap(),
            ],
            vec!["1.1.1.1:9999".parse().unwrap()],
            vec!["[2606:4700:4700::1111]:3478".parse().unwrap()],
        ] {
            assert_eq!(
                resolve_supported_udp_endpoint(
                    &access,
                    &FixedResolver(addresses),
                    AddressFamily::Ipv4,
                )
                .await,
                Err(RelayError::DnsRejected)
            );
        }
        assert_eq!(
            resolve_supported_udp_endpoint(
                &access,
                &FixedResolver(vec!["1.1.1.1:3478".parse().unwrap()]),
                AddressFamily::Ipv6,
            )
            .await,
            Err(RelayError::DnsRejected)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_dns_is_bounded_and_fails_for_immediate_https_fallback() {
        let access = access(
            br#"{"iceServers":[{"urls":"turn:turn.cloudflare.com:3478?transport=udp","username":"u","credential":"c"}]}"#,
        );
        let result =
            resolve_supported_udp_endpoint(&access, &HangingResolver, AddressFamily::Ipv4).await;
        assert_eq!(result, Err(RelayError::TransportUnavailable));
    }

    #[tokio::test]
    async fn unsupported_tcp_and_tls_are_rejected_without_downgrade() {
        for endpoint in [
            "turn:turn.cloudflare.com:3478?transport=tcp",
            "turns:turn.cloudflare.com:5349?transport=tcp",
        ] {
            let json = format!(
                r#"{{"iceServers":[{{"urls":"{endpoint}","username":"u","credential":"c"}}]}}"#
            );
            let access = access(json.as_bytes());
            assert_eq!(
                resolve_supported_udp_endpoint(
                    &access,
                    &FixedResolver(vec!["1.1.1.1:3478".parse().unwrap()]),
                    AddressFamily::Ipv4,
                )
                .await,
                Err(RelayError::TransportUnavailable)
            );
        }
    }

    #[tokio::test]
    async fn packet_boundary_rejects_every_non_turn_destination_before_io() {
        let allowed = PinnedTurnEndpoint {
            socket_addr: "1.1.1.1:3478".parse().unwrap(),
        };
        let sink = Arc::new(RecordingSink::default());
        let connection = DestinationPinnedUdpConn::bind(allowed, Arc::clone(&sink))
            .await
            .unwrap();
        let forbidden = "8.8.8.8:3478".parse().unwrap();
        assert!(connection.send_to(b"not sent", forbidden).await.is_err());
        assert!(connection.connect(forbidden).await.is_err());
        let events = sink.0.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                PacketEvent::Attempted {
                    destination: forbidden
                },
                PacketEvent::Rejected {
                    destination: forbidden
                },
                PacketEvent::Attempted {
                    destination: forbidden
                },
                PacketEvent::Rejected {
                    destination: forbidden
                },
            ]
        );
        assert!(events.iter().all(|event| match event {
            PacketEvent::Attempted { destination } | PacketEvent::Rejected { destination } =>
                *destination != allowed.socket_addr,
        }));
    }

    #[tokio::test]
    async fn turn_dependency_is_constructed_with_only_a_pinned_udp_destination() {
        let access = access(
            br#"{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478","turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"}]}"#,
        );
        let pinned = resolve_supported_udp_endpoint(
            &access,
            &FixedResolver(vec!["1.1.1.1:3478".parse().unwrap()]),
            AddressFamily::Ipv4,
        )
        .await
        .unwrap();
        let sink = Arc::new(RecordingSink::default());
        let client = RelayOnlyTurnClient::new_udp(&access, pinned, Arc::clone(&sink))
            .await
            .unwrap();
        // Construction/listening itself sends nothing and performs no Binding
        // request. An allocation would send only to the pinned address through
        // the same enforcing connection (covered above without live network).
        assert!(sink.0.lock().unwrap().is_empty());
        client.close().await.unwrap();
    }

    #[tokio::test]
    async fn allocated_session_exposes_only_redacted_registration_and_bound_envelopes() {
        let local: SocketAddr = "104.16.0.7:49152".parse().unwrap();
        let peer_text = "104.16.0.8:49153";
        let peer: SocketAddr = peer_text.parse().unwrap();
        let policy = crate::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let route = RelayAllocationRoute::parse_from_turn_local_addr(peer_text, &policy).unwrap();
        let fake = FakeRelayConn::new(local);
        let mut allocation = RelayOnlyAllocation::from_test_connection(fake.clone(), local);

        let signing = SigningKey::from_bytes(&[31; 32]);
        let credential = sign_relay_credential(
            RelayCredentialClaims {
                schema: RELAY_CREDENTIAL_SCHEMA.into(),
                relay_id: "relay-a".into(),
                session_id: "session-a".into(),
                subject_id: "subject-a".into(),
                object_sha256: "a".repeat(64),
                direction: RelayDirection::Download,
                issued_unix: 100,
                not_before_unix: 100,
                expires_unix: 700,
                max_bytes: 1024,
                max_chunks: 4,
            },
            "relay-key-a",
            &signing,
            100,
            &ProtocolLimits::default(),
        )
        .unwrap();
        let key_pair = crate::EphemeralKeyPair::generate();
        let offer = key_pair
            .offer(
                &credential,
                crate::RelayRole::Downloader,
                101,
                &ProtocolLimits::default(),
            )
            .unwrap();
        let registration = allocation
            .route_registration_request(&credential, &offer)
            .unwrap();
        assert_eq!(registration.turn_local_addr, local.to_string());
        let rendered = format!("{registration:?} {allocation:?} {route:?}");
        assert!(!rendered.contains("104.16"));
        assert!(!rendered.contains("49152"));
        assert!(allocation.send_envelope(&envelope()).await.is_err());

        allocation.bind_peer_route(route).unwrap();
        assert_eq!(allocation.bind_peer_route(route), Err(RelayError::Replay));
        let envelope = envelope();
        allocation.send_envelope(&envelope).await.unwrap();
        {
            let sent = fake.state.sent.lock().unwrap();
            assert_eq!(sent.len(), 1);
            assert_eq!(sent[0].1, peer);
        }

        let encoded = serde_json::to_vec(&envelope).unwrap();
        fake.push_incoming(encoded.clone(), local);
        assert_eq!(
            allocation.receive_envelope().await,
            Err(RelayError::TransportUnavailable),
            "even a valid envelope from a non-bound route must be rejected"
        );
        fake.push_incoming(encoded, peer);
        assert_eq!(allocation.receive_envelope().await.unwrap(), envelope);
        allocation.close().await.unwrap();
    }

    #[tokio::test]
    async fn allocated_session_rejects_oversize_envelope_before_transport() {
        let local: SocketAddr = "104.16.0.7:49152".parse().unwrap();
        let policy = crate::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let route =
            RelayAllocationRoute::parse_from_turn_local_addr("104.16.0.8:49153", &policy).unwrap();
        let fake = FakeRelayConn::new(local);
        let mut allocation = RelayOnlyAllocation::from_test_connection(fake.clone(), local);
        allocation.bind_peer_route(route).unwrap();
        let mut envelope = envelope();
        envelope.ciphertext_base64 = "A".repeat(MAX_RELAY_ENVELOPE_DATAGRAM_BYTES);
        assert_eq!(
            allocation.send_envelope(&envelope).await,
            Err(RelayError::EnvelopeRejected)
        );
        assert!(fake.state.sent.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn permission_required_transport_drops_data_until_downloader_sends_first() {
        let upload_addr: SocketAddr = "104.16.0.7:49152".parse().unwrap();
        let download_addr: SocketAddr = "104.16.0.8:49153".parse().unwrap();
        let (upload_conn, _download_conn) = FaultyLinkState::pair(upload_addr, download_addr);
        let link = Arc::clone(&upload_conn.state);
        let route_policy = crate::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let mut upload_allocation =
            RelayOnlyAllocation::from_test_connection(upload_conn, upload_addr);
        upload_allocation
            .bind_peer_route(
                RelayAllocationRoute::parse_from_turn_local_addr(
                    &download_addr.to_string(),
                    &route_policy,
                )
                .unwrap(),
            )
            .unwrap();

        upload_allocation.send_envelope(&envelope()).await.unwrap();

        assert!(link.uploader_permission.load(Ordering::SeqCst));
        assert!(!link.downloader_permission.load(Ordering::SeqCst));
        assert!(link.to_downloader.lock().await.is_empty());
        assert_eq!(
            link.dropped_for_missing_permission.load(Ordering::SeqCst),
            1
        );
    }

    #[tokio::test]
    async fn reliable_turn_only_transfer_recovers_loss_and_duplicate_and_hashes_exactly() {
        let upload_addr: SocketAddr = "104.16.0.7:49152".parse().unwrap();
        let download_addr: SocketAddr = "104.16.0.8:49153".parse().unwrap();
        let (upload_conn, download_conn) = FaultyLinkState::pair(upload_addr, download_addr);
        let link = Arc::clone(&upload_conn.state);
        let route_policy = crate::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let mut upload_allocation =
            RelayOnlyAllocation::from_test_connection(upload_conn, upload_addr);
        upload_allocation
            .bind_peer_route(
                RelayAllocationRoute::parse_from_turn_local_addr(
                    &download_addr.to_string(),
                    &route_policy,
                )
                .unwrap(),
            )
            .unwrap();
        let mut download_allocation =
            RelayOnlyAllocation::from_test_connection(download_conn, download_addr);
        download_allocation
            .bind_peer_route(
                RelayAllocationRoute::parse_from_turn_local_addr(
                    &upload_addr.to_string(),
                    &route_policy,
                )
                .unwrap(),
            )
            .unwrap();

        let object = (0..1_300)
            .map(|value| (value % 251) as u8)
            .collect::<Vec<_>>();
        let object_hash = object_sha256(&object);
        let limits = ProtocolLimits::default();
        let signing = SigningKey::from_bytes(&[43; 32]);
        let common = RelayCredentialClaims {
            schema: RELAY_CREDENTIAL_SCHEMA.into(),
            relay_id: "relay-a".into(),
            session_id: "session-reliable".into(),
            subject_id: "opaque-uploader".into(),
            object_sha256: object_hash,
            direction: RelayDirection::Upload,
            issued_unix: 100,
            not_before_unix: 100,
            expires_unix: 700,
            max_bytes: object.len() as u64,
            max_chunks: crate::bounded_relay_chunk_count(object.len() as u64, &limits).unwrap(),
        };
        let upload_credential =
            sign_relay_credential(common.clone(), "relay-key", &signing, 100, &limits).unwrap();
        let download_credential = sign_relay_credential(
            RelayCredentialClaims {
                subject_id: "opaque-downloader".into(),
                direction: RelayDirection::Download,
                ..common
            },
            "relay-key",
            &signing,
            100,
            &limits,
        )
        .unwrap();
        let uploader_keys = crate::EphemeralKeyPair::generate();
        let downloader_keys = crate::EphemeralKeyPair::generate();
        let upload_offer = uploader_keys
            .offer(&upload_credential, crate::RelayRole::Uploader, 100, &limits)
            .unwrap();
        let download_offer = downloader_keys
            .offer(
                &download_credential,
                crate::RelayRole::Downloader,
                100,
                &limits,
            )
            .unwrap();
        let binding = crate::sign_session_binding(
            crate::build_session_binding(&upload_offer, &download_offer, 700).unwrap(),
            "relay-key",
            &signing,
        )
        .unwrap();
        let trusted = TrustedSigningKeys::from([("relay-key".into(), signing.verifying_key())]);
        let verified = crate::verify_signed_session_binding(
            &binding,
            &upload_credential,
            &download_credential,
            100,
            &trusted,
            &limits,
        )
        .unwrap();
        let sender = RelaySender::new(
            uploader_keys
                .derive_session_key(&verified, crate::RelayRole::Uploader)
                .unwrap(),
            &verified,
            &upload_credential.claims,
            object.len() as u64,
            crate::RelayChunkPolicy::default(),
            limits,
        )
        .unwrap();
        let receiver = RelayReceiver::new(
            downloader_keys
                .derive_session_key(&verified, crate::RelayRole::Downloader)
                .unwrap(),
            &verified,
            &download_credential.claims,
            object.len() as u64,
            crate::RelayChunkPolicy::default(),
            limits,
        )
        .unwrap();
        let reliability = RelayReliabilityPolicy {
            max_data_attempts: 6,
            receive_timeout: Duration::from_millis(20),
            completion_repetitions: 3,
        };
        download_allocation
            .prime_receive_permission(&receiver)
            .await
            .unwrap();
        assert!(link.downloader_permission.load(Ordering::SeqCst));
        assert!(!link.uploader_permission.load(Ordering::SeqCst));
        assert_eq!(
            link.dropped_for_missing_permission.load(Ordering::SeqCst),
            1,
            "the readiness marker creates the downloader permission even when the reverse allocation drops it"
        );
        let send = upload_allocation.send_object_reliably(sender, &object, reliability);
        let receive = download_allocation.receive_object_reliably(receiver, reliability);
        let (send_result, receive_result) = tokio::join!(send, receive);
        send_result.unwrap();
        assert_eq!(receive_result.unwrap(), object);
        assert!(!link.drop_first_data.load(Ordering::SeqCst));
        assert!(!link.drop_first_ack.load(Ordering::SeqCst));
        assert!(!link.drop_first_receipt.load(Ordering::SeqCst));
        assert!(link.uploader_permission.load(Ordering::SeqCst));
        assert!(link.downloader_permission.load(Ordering::SeqCst));
        assert_eq!(
            link.dropped_for_missing_permission.load(Ordering::SeqCst),
            1,
            "no encrypted data may be lost to an unprimed downloader allocation"
        );
        let flows = link.flows.lock().unwrap();
        assert!(
            flows.len() > 8,
            "loss and ACK loss must force bounded retransmission"
        );
        assert!(flows.iter().all(|(source, target, _)| {
            (*source == upload_addr && *target == download_addr)
                || (*source == download_addr && *target == upload_addr)
        }));
        assert!(flows.iter().all(|(_, _, schema)| {
            schema == RELAY_ENVELOPE_SCHEMA || schema == RELAY_ACK_SCHEMA
        }));
    }

    fn envelope() -> EncryptedRelayEnvelope {
        EncryptedRelayEnvelope {
            schema: RELAY_ENVELOPE_SCHEMA.into(),
            session_id: "session-a".into(),
            object_sha256: "a".repeat(64),
            cipher: EndToEndCipher::XChaCha20Poly1305,
            chunk_index: 0,
            chunk_count: 1,
            plaintext_size: 1,
            nonce_base64: "A".repeat(32),
            ciphertext_base64: "AQ==".into(),
        }
    }
}

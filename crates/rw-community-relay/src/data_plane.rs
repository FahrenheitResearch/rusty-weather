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

use crate::{ProviderRelayAccess, RelayError, TurnScheme, TurnTransport};

const MAX_RESOLVED_ADDRESSES: usize = 32;
pub const RELAY_DNS_TIMEOUT: Duration = Duration::from_secs(3);
pub const RELAY_CLIENT_START_TIMEOUT: Duration = Duration::from_secs(5);
pub const RELAY_ALLOCATION_TIMEOUT: Duration = Duration::from_secs(8);
pub const RELAY_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

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

    /// Allocate an operator relay address internally. This is a TURN Allocate
    /// exchange, not candidate gathering. It remains crate-private until an
    /// opaque backend-authorized route wrapper owns the returned connection.
    #[allow(dead_code)]
    pub(crate) async fn allocate_internal(&self) -> Result<impl Conn, RelayError> {
        timeout(RELAY_ALLOCATION_TIMEOUT, self.client.allocate())
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable)
    }

    pub async fn close(&self) -> Result<(), RelayError> {
        timeout(RELAY_CLOSE_TIMEOUT, self.client.close())
            .await
            .map_err(|_| RelayError::TransportUnavailable)?
            .map_err(|_| RelayError::TransportUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CloudflareTurnAdapter;
    use std::sync::Mutex as StdMutex;

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
}

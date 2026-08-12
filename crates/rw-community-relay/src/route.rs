//! Closed exchange for routes returned by a participant's own TURN Allocate.
//!
//! A route is accepted only when it is globally routable and belongs to an
//! explicit operator-audited relay-provider CIDR. The default allowlist is
//! empty. These values are transport-private Cloudflare allocation addresses,
//! never host/srflx/prflx candidates and never app-visible discovery state.

use std::collections::BTreeMap;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::str::FromStr;

use rw_community_protocol::SignedRelayCredential;
use serde::{Deserialize, Serialize};

use crate::control::{AuthenticatedSubject, OpaqueIdSource, RelayCoordinator};
use crate::data_plane::is_global_relay_address;
use crate::{
    EphemeralPublicOffer, RelayError, RelayProvider, RelayRole, SignedSessionBinding,
    valid_opaque_id,
};

pub const TRANSPORT_ROUTE_GRANT_SCHEMA: &str = "rw.community.relay-transport-route.v1";
const MAX_ROUTE_ALLOWLIST_ENTRIES: usize = 128;
const MAX_TRANSPORT_ROUTE_JSON_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, PartialEq, Eq)]
enum RelayNetwork {
    V4 { network: u32, prefix: u8 },
    V6 { network: u128, prefix: u8 },
}

/// Exact operator-managed relay allocation ranges. There are deliberately no
/// compiled Cloudflare ranges: provider address ownership changes over time
/// and must be audited at deployment. Empty/default policy rejects everything.
#[derive(Clone, Default)]
pub struct RelayRoutePolicy {
    networks: Vec<RelayNetwork>,
}

impl fmt::Debug for RelayRoutePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayRoutePolicy")
            .field("allowlist_entries", &self.networks.len())
            .finish()
    }
}

impl RelayRoutePolicy {
    pub fn from_audited_cidrs<I, S>(cidrs: I) -> Result<Self, RelayError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut networks = Vec::new();
        for raw in cidrs {
            if networks.len() >= MAX_ROUTE_ALLOWLIST_ENTRIES {
                return Err(RelayError::PolicyDenied);
            }
            let network = parse_network(raw.as_ref())?;
            if !networks.contains(&network) {
                networks.push(network);
            }
        }
        Ok(Self { networks })
    }

    pub fn is_empty(&self) -> bool {
        self.networks.is_empty()
    }

    fn permits(&self, address: SocketAddr) -> bool {
        address.port() != 0
            && is_global_relay_address(address.ip())
            && self
                .networks
                .iter()
                .any(|network| network_contains(*network, address.ip()))
    }
}

/// A provider relay allocation address. Debug is redacted and the address has
/// no general serialization implementation. The only wire encoding is the
/// participant-authenticated transport grant below.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct RelayAllocationRoute(SocketAddr);

impl fmt::Debug for RelayAllocationRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayAllocationRoute([provider relay])")
    }
}

impl RelayAllocationRoute {
    pub fn parse_from_turn_local_addr(
        value: &str,
        policy: &RelayRoutePolicy,
    ) -> Result<Self, RelayError> {
        if value.is_empty() || value.len() > 96 || value.trim() != value {
            return Err(RelayError::PolicyDenied);
        }
        let address = SocketAddr::from_str(value).map_err(|_| RelayError::PolicyDenied)?;
        if !policy.permits(address) {
            return Err(RelayError::PolicyDenied);
        }
        Ok(Self(address))
    }

    /// Deliberate data-plane access. This is the TURN provider's allocation,
    /// not the participant's host/server-reflexive address.
    pub(crate) const fn socket_addr_for_relay_transport(self) -> SocketAddr {
        self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayRouteRegistrationReceipt {
    pub schema: String,
    pub session_id: String,
    pub role: RelayRole,
    pub binding_ready: bool,
}

struct RegisteredParticipantRoute {
    route: RelayAllocationRoute,
    offer: EphemeralPublicOffer,
    credential: SignedRelayCredential,
}

impl fmt::Debug for RegisteredParticipantRoute {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RegisteredParticipantRoute([redacted])")
    }
}

#[derive(Default)]
struct RegisteredRouteSession {
    uploader: Option<RegisteredParticipantRoute>,
    downloader: Option<RegisteredParticipantRoute>,
    binding: Option<SignedSessionBinding>,
    expires_unix: i64,
}

impl fmt::Debug for RegisteredRouteSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RegisteredRouteSession([redacted])")
    }
}

/// In-memory routes belong to live TURN allocations and therefore must never
/// survive process restart. The durable coordinator treats every pre-crash
/// session as failed/revoked and charges its complete reservation.
#[derive(Default)]
pub struct RelayRouteRegistry {
    policy: RelayRoutePolicy,
    sessions: BTreeMap<String, RegisteredRouteSession>,
}

impl fmt::Debug for RelayRouteRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RelayRouteRegistry")
            .field("active_sessions", &self.sessions.len())
            .field("policy", &self.policy)
            .finish()
    }
}

impl RelayRouteRegistry {
    pub fn new(policy: RelayRoutePolicy) -> Self {
        Self {
            policy,
            sessions: BTreeMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn register<P: RelayProvider, I: OpaqueIdSource>(
        &mut self,
        coordinator: &RelayCoordinator<P, I>,
        subject: &AuthenticatedSubject,
        credential: &SignedRelayCredential,
        offer: EphemeralPublicOffer,
        turn_local_addr: &str,
        now_unix: i64,
    ) -> Result<RelayRouteRegistrationReceipt, RelayError> {
        self.reap(now_unix);
        if self.policy.is_empty() {
            return Err(RelayError::SecurityGate);
        }
        offer.validate()?;
        let authorized =
            coordinator.authorize_participant(subject, credential, offer.role, now_unix)?;
        if offer.session_id != authorized.session_id
            || offer.object_sha256 != authorized.object_sha256
            || offer.credential_fingerprint != authorized.credential_fingerprint
            || !valid_opaque_id(&offer.session_id)
        {
            return Err(RelayError::KeyAgreementRejected);
        }
        let route =
            RelayAllocationRoute::parse_from_turn_local_addr(turn_local_addr, &self.policy)?;
        let session = self
            .sessions
            .entry(authorized.session_id.clone())
            .or_default();
        if session.expires_unix != 0 && session.expires_unix != authorized.expires_unix {
            return Err(RelayError::KeyAgreementRejected);
        }
        session.expires_unix = authorized.expires_unix;
        let (slot, counterpart) = match authorized.role {
            RelayRole::Uploader => (&mut session.uploader, session.downloader.as_ref()),
            RelayRole::Downloader => (&mut session.downloader, session.uploader.as_ref()),
        };
        // Registration is one-shot. Replaying even an identical route/offer
        // cannot replace or extend a live allocation binding.
        if slot.is_some() {
            return Err(RelayError::Replay);
        }
        let binding = if let Some(counterpart) = counterpart {
            if counterpart.route == route {
                return Err(RelayError::KeyAgreementRejected);
            }
            let (uploader, downloader) = match authorized.role {
                RelayRole::Uploader => (&offer, &counterpart.offer),
                RelayRole::Downloader => (&counterpart.offer, &offer),
            };
            Some(coordinator.sign_transport_binding(uploader, downloader, now_unix)?)
        } else {
            None
        };
        *slot = Some(RegisteredParticipantRoute {
            route,
            offer,
            credential: credential.clone(),
        });
        session.binding = binding;
        Ok(RelayRouteRegistrationReceipt {
            schema: TRANSPORT_ROUTE_GRANT_SCHEMA.into(),
            session_id: authorized.session_id,
            role: authorized.role,
            binding_ready: session.binding.is_some(),
        })
    }

    pub fn participant_grant<P: RelayProvider, I: OpaqueIdSource>(
        &mut self,
        coordinator: &RelayCoordinator<P, I>,
        subject: &AuthenticatedSubject,
        credential: &SignedRelayCredential,
        role: RelayRole,
        now_unix: i64,
    ) -> Result<ParticipantTransportRouteGrant, RelayError> {
        self.reap(now_unix);
        let authorized = coordinator.authorize_participant(subject, credential, role, now_unix)?;
        let session = self
            .sessions
            .get(&authorized.session_id)
            .ok_or(RelayError::NotAvailable)?;
        let binding = session.binding.clone().ok_or(RelayError::NotAvailable)?;
        let peer = match role {
            RelayRole::Uploader => session.downloader.as_ref(),
            RelayRole::Downloader => session.uploader.as_ref(),
        }
        .ok_or(RelayError::NotAvailable)?;
        Ok(ParticipantTransportRouteGrant {
            session_id: authorized.session_id,
            role,
            peer_relay_route: peer.route,
            peer_credential: peer.credential.clone(),
            binding,
        })
    }

    pub fn remove_session(&mut self, session_id: &str) {
        if valid_opaque_id(session_id) {
            self.sessions.remove(session_id);
        }
    }

    fn reap(&mut self, now_unix: i64) {
        self.sessions
            .retain(|_, session| session.expires_unix > now_unix);
    }
}

/// Participant-specific transport result. It contains exactly the other
/// participant's provider relay allocation and the signed E2E transcript—no
/// account identity, host/srflx candidate, or combined pair of grants.
pub struct ParticipantTransportRouteGrant {
    session_id: String,
    role: RelayRole,
    peer_relay_route: RelayAllocationRoute,
    peer_credential: SignedRelayCredential,
    binding: SignedSessionBinding,
}

impl fmt::Debug for ParticipantTransportRouteGrant {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ParticipantTransportRouteGrant([redacted])")
    }
}

impl ParticipantTransportRouteGrant {
    pub const fn peer_relay_route(&self) -> RelayAllocationRoute {
        self.peer_relay_route
    }

    pub fn signed_binding(&self) -> &SignedSessionBinding {
        &self.binding
    }

    pub fn peer_credential(&self) -> &SignedRelayCredential {
        &self.peer_credential
    }

    /// The sole address-bearing wire seam. A server returns these bounded JSON
    /// bytes only from a participant-authenticated transport endpoint and must
    /// mark the response non-cacheable and exclude its body from logs/traces.
    pub fn transport_json(&self) -> Result<Vec<u8>, RelayError> {
        #[derive(Serialize)]
        #[serde(deny_unknown_fields)]
        struct Wire<'a> {
            schema: &'static str,
            session_id: &'a str,
            role: RelayRole,
            peer_relay_allocation: String,
            peer_credential: &'a SignedRelayCredential,
            signed_binding: &'a SignedSessionBinding,
        }
        let bytes = serde_json::to_vec(&Wire {
            schema: TRANSPORT_ROUTE_GRANT_SCHEMA,
            session_id: &self.session_id,
            role: self.role,
            peer_relay_allocation: self.peer_relay_route.0.to_string(),
            peer_credential: &self.peer_credential,
            signed_binding: &self.binding,
        })
        .map_err(|_| RelayError::KeyAgreementRejected)?;
        if bytes.len() > MAX_TRANSPORT_ROUTE_JSON_BYTES {
            return Err(RelayError::KeyAgreementRejected);
        }
        Ok(bytes)
    }
}

fn parse_network(value: &str) -> Result<RelayNetwork, RelayError> {
    if value.is_empty() || value.len() > 96 || value.trim() != value {
        return Err(RelayError::PolicyDenied);
    }
    let (raw_address, raw_prefix) = value.split_once('/').unwrap_or((value, ""));
    let address = IpAddr::from_str(raw_address).map_err(|_| RelayError::PolicyDenied)?;
    match address {
        IpAddr::V4(address) => {
            let prefix = if raw_prefix.is_empty() {
                32
            } else {
                raw_prefix
                    .parse::<u8>()
                    .ok()
                    .filter(|prefix| (16..=32).contains(prefix))
                    .ok_or(RelayError::PolicyDenied)?
            };
            let raw = u32::from(address);
            let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
            let network = raw & mask;
            if raw != network
                || !is_global_relay_address(IpAddr::V4(Ipv4Addr::from(network)))
                || !is_global_relay_address(IpAddr::V4(Ipv4Addr::from(network | !mask)))
            {
                return Err(RelayError::PolicyDenied);
            }
            Ok(RelayNetwork::V4 { network, prefix })
        }
        IpAddr::V6(address) => {
            let prefix = if raw_prefix.is_empty() {
                128
            } else {
                raw_prefix
                    .parse::<u8>()
                    .ok()
                    .filter(|prefix| (32..=128).contains(prefix))
                    .ok_or(RelayError::PolicyDenied)?
            };
            let raw = u128::from(address);
            let mask = u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0);
            let network = raw & mask;
            if raw != network
                || !is_global_relay_address(IpAddr::V6(Ipv6Addr::from(network)))
                || !is_global_relay_address(IpAddr::V6(Ipv6Addr::from(network | !mask)))
            {
                return Err(RelayError::PolicyDenied);
            }
            Ok(RelayNetwork::V6 { network, prefix })
        }
    }
}

fn network_contains(network: RelayNetwork, address: IpAddr) -> bool {
    match (network, address) {
        (RelayNetwork::V4 { network, prefix }, IpAddr::V4(address)) => {
            let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
            u32::from(address) & mask == network
        }
        (RelayNetwork::V6 { network, prefix }, IpAddr::V6(address)) => {
            let mask = u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0);
            u128::from(address) & mask == network
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_unsafe_allowlists_fail_closed() {
        let empty = RelayRoutePolicy::default();
        assert!(matches!(
            RelayAllocationRoute::parse_from_turn_local_addr("104.16.1.2:50000", &empty),
            Err(RelayError::PolicyDenied)
        ));
        for cidr in [
            "0.0.0.0/0",
            "10.0.0.0/24",
            "192.0.2.0/24",
            "100.64.0.0/16",
            "2001:db8::/48",
            "2606:4700::1/48",
        ] {
            assert!(RelayRoutePolicy::from_audited_cidrs([cidr]).is_err());
        }
    }

    #[test]
    fn exact_audited_range_accepts_only_global_addresses_inside_it() {
        let policy =
            RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24", "2606:4700:100::/48"]).unwrap();
        assert!(
            RelayAllocationRoute::parse_from_turn_local_addr("104.16.0.7:49152", &policy).is_ok()
        );
        assert!(
            RelayAllocationRoute::parse_from_turn_local_addr("[2606:4700:100::9]:49152", &policy,)
                .is_ok()
        );
        for value in [
            "104.16.1.7:49152",
            "104.16.0.7:0",
            "10.0.0.7:49152",
            "203.0.113.7:49152",
        ] {
            assert!(RelayAllocationRoute::parse_from_turn_local_addr(value, &policy).is_err());
        }
    }

    #[test]
    fn routes_have_redacted_debug_and_no_general_serde_contract() {
        let policy = RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let route =
            RelayAllocationRoute::parse_from_turn_local_addr("104.16.0.7:49152", &policy).unwrap();
        assert!(!format!("{route:?}").contains("104.16"));
        assert_eq!(
            route.socket_addr_for_relay_transport(),
            "104.16.0.7:49152".parse().unwrap()
        );
    }
}

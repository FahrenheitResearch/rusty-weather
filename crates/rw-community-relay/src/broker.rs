//! Exact HTTP wire contract between a Community Cache client and the relay
//! broker. Address-bearing values occur only in participant-authenticated,
//! `Cache-Control: no-store` responses and name TURN provider allocations,
//! never another participant's host or server-reflexive address.

use std::collections::BTreeSet;
use std::fmt;

use rw_community_protocol::{
    ProtocolLimits, RelayCandidate, RelayDirection, SignedObjectManifest, SignedRelayCredential,
    TrustedSigningKeys, verify_signed_relay_credential,
};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

use crate::{
    AdvertisementReceipt, EphemeralPublicOffer, FallbackTarget, PublicRelayFailure, RelayError,
    RelayObjectCategory, RelayRole, RelayRoutePolicy, RelayRouteRegistrationReceipt,
    SignedSessionBinding,
};

pub const RELAY_ADVERTISE_REQUEST_SCHEMA: &str = "rw.community.relay-advertise-request.v1";
pub const RELAY_HISTORICAL_LOOKUP_SCHEMA: &str = "rw.community.relay-historical-lookup.v1";
pub const RELAY_LOOKUP_RESPONSE_SCHEMA: &str = "rw.community.relay-historical-lookup-response.v1";
pub const RELAY_PARTICIPANT_GRANT_SCHEMA: &str = "rw.community.relay-participant-grant.v1";
pub const RELAY_GRANT_POLL_SCHEMA: &str = "rw.community.relay-grant-poll.v1";
pub const RELAY_ROUTE_REGISTRATION_SCHEMA: &str = "rw.community.relay-route-registration.v1";
pub const RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA: &str =
    "rw.community.relay-transport-grant-request.v1";
pub const RELAY_SESSION_COMPLETION_SCHEMA: &str = "rw.community.relay-session-completion.v1";
pub const RELAY_SESSION_FAILURE_SCHEMA: &str = "rw.community.relay-session-failure.v1";
pub const RELAY_SESSION_REVOCATION_SCHEMA: &str = "rw.community.relay-session-revocation.v1";
pub const RELAY_KILL_SWITCH_SCHEMA: &str = "rw.community.relay-kill-switch.v1";
pub const RELAY_STATUS_SCHEMA: &str = "rw.community.relay-status.v1";
pub const RELAY_TERMINAL_SCHEMA: &str = "rw.community.relay-terminal.v1";

pub const RELAY_ADVERTISE_PATH: &str = "/v1/community/relay/advertisements";
pub const RELAY_HISTORICAL_LOOKUP_PATH: &str = "/v1/community/relay/historical/lookups";
pub const RELAY_NEXT_GRANT_PATH: &str = "/v1/community/relay/grants/next";
pub const RELAY_SESSION_GRANT_PATH_TEMPLATE: &str =
    "/v1/community/relay/sessions/{session_id}/grants/{role}";
pub const RELAY_ROUTE_REGISTRATION_PATH: &str = "/v1/community/relay/routes";
pub const RELAY_TRANSPORT_GRANT_PATH: &str = "/v1/community/relay/transport";
pub const RELAY_SESSION_COMPLETE_PATH: &str = "/v1/community/relay/sessions/complete";
pub const RELAY_SESSION_FAIL_PATH: &str = "/v1/community/relay/sessions/fail";
pub const RELAY_SESSION_REVOKE_PATH: &str = "/v1/community/relay/sessions/revoke";
pub const RELAY_OPERATOR_KILL_SWITCH_PATH: &str = "/v1/community/relay/operator/kill-switch";
pub const RELAY_OPERATOR_STATUS_PATH: &str = "/v1/community/relay/operator/status";

const MAX_BROKER_JSON_BYTES: usize = 256 * 1024;
const MAX_TURN_URLS: usize = 32;
const MAX_SECRET_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayAdvertiseRequest {
    pub schema: String,
    pub signed_manifest: SignedObjectManifest,
    pub opted_in: bool,
    pub categories: BTreeSet<RelayObjectCategory>,
    pub disk_allowance_bytes: u64,
    pub upload_allowance_bytes: u64,
    pub metered_network: bool,
    pub allow_metered_seeding: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalRelayLookupRequest {
    pub schema: String,
    /// Must be true so an operational miss cannot enter this path.
    pub historical: bool,
    pub object_sha256: String,
    pub opted_in: bool,
    pub download_allowance_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayGrantPollRequest {
    pub schema: String,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayRouteRegistrationRequest {
    pub schema: String,
    pub credential: SignedRelayCredential,
    pub offer: EphemeralPublicOffer,
    /// Exact provider allocation returned by this participant's own TURN
    /// Allocate. A server must check an operator-audited provider CIDR.
    pub turn_local_addr: String,
}

impl fmt::Debug for RelayRouteRegistrationRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayRouteRegistrationRequest([redacted provider route])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayTransportGrantRequest {
    pub schema: String,
    pub role: RelayRole,
    pub credential: SignedRelayCredential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySessionCompletionRequest {
    pub schema: String,
    pub role: RelayRole,
    pub credential: SignedRelayCredential,
    pub transferred_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelaySessionFailureRequest {
    pub schema: String,
    pub role: RelayRole,
    pub credential: SignedRelayCredential,
}

pub type RelaySessionRevocationRequest = RelaySessionFailureRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayKillSwitchRequest {
    pub schema: String,
    pub enabled: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayTurnAccessWire {
    pub urls: Vec<String>,
    pub username: String,
    pub credential: String,
    pub expires_unix: i64,
}

impl fmt::Debug for RelayTurnAccessWire {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RelayTurnAccessWire([redacted TURN access])")
    }
}

impl Drop for RelayTurnAccessWire {
    fn drop(&mut self) {
        self.username.zeroize();
        self.credential.zeroize();
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantRelayGrantWire {
    pub schema: String,
    pub session_id: String,
    pub object_sha256: String,
    pub encoded_size: u64,
    pub role: RelayRole,
    pub candidate: RelayCandidate,
    pub credential: SignedRelayCredential,
    pub turn: RelayTurnAccessWire,
}

impl fmt::Debug for ParticipantRelayGrantWire {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ParticipantRelayGrantWire([redacted participant grant])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoricalRelayLookupResponse {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participant_grant: Option<ParticipantRelayGrantWire>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<PublicRelayFailure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback_after_relay_failure: Option<FallbackTarget>,
}

/// Parsed only inside the relay transport task. Never retain this value in
/// app-visible state or include it in a diagnostic message.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParticipantTransportRouteGrantWire {
    pub schema: String,
    pub session_id: String,
    pub role: RelayRole,
    pub peer_relay_allocation: String,
    pub peer_credential: SignedRelayCredential,
    pub signed_binding: SignedSessionBinding,
}

impl fmt::Debug for ParticipantTransportRouteGrantWire {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ParticipantTransportRouteGrantWire([redacted provider route])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayStatusResponse {
    pub schema: String,
    pub enabled: bool,
    pub kill_switch: bool,
    pub persistence_healthy: bool,
    pub transport_route_gate_configured: bool,
    pub sessions_issued: u64,
    pub sessions_completed: u64,
    pub sessions_failed: u64,
    pub promotion_signals: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelayTerminalResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<PublicRelayFailure>,
    pub promotion_requested: bool,
    /// True only after the broker authenticated matching exact-byte reports
    /// from both uploader and downloader. One-sided/lost completion never
    /// counts as a successful recovery.
    pub session_complete: bool,
}

pub fn parse_participant_grant_bounded(
    bytes: &[u8],
    expected_object_sha256: &str,
    expected_role: RelayRole,
    now_unix: i64,
    trusted_relay_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<ParticipantRelayGrantWire, RelayError> {
    check_json_size(bytes)?;
    let grant: ParticipantRelayGrantWire =
        serde_json::from_slice(bytes).map_err(|_| RelayError::CredentialInvalid)?;
    grant.validate(
        expected_object_sha256,
        expected_role,
        now_unix,
        trusted_relay_keys,
        limits,
    )?;
    Ok(grant)
}

/// Parse an uploader's caller-specific grant poll without trusting any field
/// as an input selector. The exact hash/size are authenticated by the signed
/// credential and are then used only to look up an already verified local CAS
/// entry; there is no arbitrary file or path seam.
pub fn parse_polled_uploader_grant_bounded(
    bytes: &[u8],
    now_unix: i64,
    trusted_relay_keys: &TrustedSigningKeys,
    limits: &ProtocolLimits,
) -> Result<ParticipantRelayGrantWire, RelayError> {
    check_json_size(bytes)?;
    let grant: ParticipantRelayGrantWire =
        serde_json::from_slice(bytes).map_err(|_| RelayError::CredentialInvalid)?;
    let expected_hash = grant.object_sha256.clone();
    grant.validate(
        &expected_hash,
        RelayRole::Uploader,
        now_unix,
        trusted_relay_keys,
        limits,
    )?;
    Ok(grant)
}

impl ParticipantRelayGrantWire {
    pub fn validate(
        &self,
        expected_object_sha256: &str,
        expected_role: RelayRole,
        now_unix: i64,
        trusted_relay_keys: &TrustedSigningKeys,
        limits: &ProtocolLimits,
    ) -> Result<(), RelayError> {
        if self.schema != RELAY_PARTICIPANT_GRANT_SCHEMA
            || self.role != expected_role
            || self.object_sha256 != expected_object_sha256
            || self.session_id != self.credential.claims.session_id
            || self.object_sha256 != self.credential.claims.object_sha256
            || self.encoded_size == 0
            || self.encoded_size > self.credential.claims.max_bytes
            || self.candidate.relay_id != self.credential.claims.relay_id
            || self.candidate.expires_unix != self.credential.claims.expires_unix
        {
            return Err(RelayError::CredentialInvalid);
        }
        let expected_direction = match expected_role {
            RelayRole::Uploader => RelayDirection::Upload,
            RelayRole::Downloader => RelayDirection::Download,
        };
        if self.credential.claims.direction != expected_direction {
            return Err(RelayError::CredentialInvalid);
        }
        self.candidate
            .validate(now_unix)
            .map_err(|_| RelayError::CredentialInvalid)?;
        verify_signed_relay_credential(&self.credential, now_unix, trusted_relay_keys, limits)
            .map_err(|_| RelayError::CredentialInvalid)?;
        self.turn
            .validate(now_unix, self.credential.claims.expires_unix)
    }
}

impl RelayTurnAccessWire {
    pub fn validate(&self, now_unix: i64, credential_expires_unix: i64) -> Result<(), RelayError> {
        if self.urls.is_empty()
            || self.urls.len() > MAX_TURN_URLS
            || self.urls.windows(2).any(|pair| pair[0] >= pair[1])
            || self.username.is_empty()
            || self.username.len() > MAX_SECRET_BYTES
            || self.credential.is_empty()
            || self.credential.len() > MAX_SECRET_BYTES
            || self
                .username
                .chars()
                .chain(self.credential.chars())
                .any(char::is_control)
            || self.expires_unix <= now_unix
            || self.expires_unix != credential_expires_unix
            || self.urls.iter().any(|url| {
                url.len() > 512
                    || url.chars().any(char::is_whitespace)
                    || url.chars().any(char::is_control)
                    || !matches!(
                        url.split_once(':').map(|(scheme, _)| scheme),
                        Some("turn" | "turns")
                    )
            })
            || !self
                .urls
                .iter()
                .any(|url| url.starts_with("turn:") && url.ends_with("?transport=udp"))
        {
            return Err(RelayError::ProviderRejected);
        }
        Ok(())
    }

    pub(crate) fn from_provider_access(access: &crate::ProviderRelayAccess) -> Self {
        Self {
            urls: access
                .endpoints()
                .iter()
                .map(|endpoint| endpoint.canonical().to_owned())
                .collect(),
            username: access.username().expose().to_owned(),
            credential: access.credential().expose().to_owned(),
            expires_unix: access.expires_unix(),
        }
    }
}

impl ParticipantRelayGrantWire {
    pub fn from_server_grant(
        object_sha256: String,
        encoded_size: u64,
        grant: &crate::ParticipantRelayGrant,
    ) -> Result<Self, RelayError> {
        let role = match grant.credential.claims.direction {
            RelayDirection::Upload => RelayRole::Uploader,
            RelayDirection::Download => RelayRole::Downloader,
        };
        let value = Self {
            schema: RELAY_PARTICIPANT_GRANT_SCHEMA.into(),
            session_id: grant.credential.claims.session_id.clone(),
            object_sha256,
            encoded_size,
            role,
            candidate: grant.candidate.clone(),
            credential: grant.credential.clone(),
            turn: RelayTurnAccessWire::from_provider_access(&grant.provider_access),
        };
        // The broker already owns trusted signing state and verifies active
        // credentials before dispatch. Retain all transport-independent shape
        // invariants here without requiring that key map a second time.
        if value.schema != RELAY_PARTICIPANT_GRANT_SCHEMA
            || value.object_sha256 != value.credential.claims.object_sha256
            || value.session_id != value.credential.claims.session_id
            || value.encoded_size == 0
            || value.encoded_size > value.credential.claims.max_bytes
            || value.candidate.relay_id != value.credential.claims.relay_id
            || value.candidate.expires_unix != value.credential.claims.expires_unix
        {
            return Err(RelayError::CredentialInvalid);
        }
        Ok(value)
    }
}

/// Exact participant-side expectations for one transport-route response.
/// Keeping these bindings in one typed value makes it harder for callers to
/// transpose a session, object, credential, keyring, or route policy.
pub struct TransportRouteExpectation<'a> {
    pub session_id: &'a str,
    pub role: RelayRole,
    pub own_credential: &'a SignedRelayCredential,
    pub object_sha256: &'a str,
    pub encoded_size: u64,
    pub now_unix: i64,
    pub trusted_relay_keys: &'a TrustedSigningKeys,
    pub limits: &'a ProtocolLimits,
    pub policy: &'a RelayRoutePolicy,
}

pub fn parse_transport_route_bounded(
    bytes: &[u8],
    expected: TransportRouteExpectation<'_>,
) -> Result<
    (
        ParticipantTransportRouteGrantWire,
        crate::RelayAllocationRoute,
        crate::VerifiedSessionBinding,
    ),
    RelayError,
> {
    check_json_size(bytes)?;
    let route: ParticipantTransportRouteGrantWire =
        serde_json::from_slice(bytes).map_err(|_| RelayError::KeyAgreementRejected)?;
    let expected_peer_direction = match expected.role {
        RelayRole::Uploader => RelayDirection::Download,
        RelayRole::Downloader => RelayDirection::Upload,
    };
    if route.schema != crate::TRANSPORT_ROUTE_GRANT_SCHEMA
        || route.session_id != expected.session_id
        || route.role != expected.role
        || route.signed_binding.binding.session_id != expected.session_id
        || route.signed_binding.binding.object_sha256 != expected.object_sha256
        || expected.own_credential.claims.session_id != expected.session_id
        || expected.own_credential.claims.object_sha256 != expected.object_sha256
        || expected.own_credential.claims.max_bytes != expected.encoded_size
        || route.peer_credential.claims.session_id != expected.session_id
        || route.peer_credential.claims.object_sha256 != expected.object_sha256
        || route.peer_credential.claims.direction != expected_peer_direction
        || route.peer_credential.claims.max_bytes != expected.encoded_size
        || route.peer_credential.claims.expires_unix != expected.own_credential.claims.expires_unix
        || route.peer_credential.claims.subject_id == expected.own_credential.claims.subject_id
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    verify_signed_relay_credential(
        expected.own_credential,
        expected.now_unix,
        expected.trusted_relay_keys,
        expected.limits,
    )
    .map_err(|_| RelayError::CredentialInvalid)?;
    verify_signed_relay_credential(
        &route.peer_credential,
        expected.now_unix,
        expected.trusted_relay_keys,
        expected.limits,
    )
    .map_err(|_| RelayError::CredentialInvalid)?;
    let (uploader_credential, downloader_credential) = match expected.role {
        RelayRole::Uploader => (expected.own_credential, &route.peer_credential),
        RelayRole::Downloader => (&route.peer_credential, expected.own_credential),
    };
    let verified_binding = crate::verify_signed_session_binding(
        &route.signed_binding,
        uploader_credential,
        downloader_credential,
        expected.now_unix,
        expected.trusted_relay_keys,
        expected.limits,
    )?;
    let allocation = crate::RelayAllocationRoute::parse_from_turn_local_addr(
        &route.peer_relay_allocation,
        expected.policy,
    )?;
    Ok((route, allocation, verified_binding))
}

pub fn parse_historical_lookup_response_bounded(
    bytes: &[u8],
) -> Result<HistoricalRelayLookupResponse, RelayError> {
    check_json_size(bytes)?;
    let response: HistoricalRelayLookupResponse =
        serde_json::from_slice(bytes).map_err(|_| RelayError::CredentialInvalid)?;
    let immediate_fallback = response.participant_grant.is_none()
        && response.fallback.is_some()
        && response.fallback_after_relay_failure.is_none();
    let relay_attempt = response.participant_grant.is_some()
        && response.fallback.is_none()
        && response.fallback_after_relay_failure.is_some();
    if response.schema != RELAY_LOOKUP_RESPONSE_SCHEMA || !(immediate_fallback || relay_attempt) {
        return Err(RelayError::CredentialInvalid);
    }
    Ok(response)
}

pub fn validate_advertise_request(request: &RelayAdvertiseRequest) -> Result<(), RelayError> {
    if request.schema != RELAY_ADVERTISE_REQUEST_SCHEMA
        || request.categories.is_empty()
        || request.disk_allowance_bytes == 0
        || request.upload_allowance_bytes == 0
    {
        return Err(RelayError::PolicyDenied);
    }
    Ok(())
}

pub fn validate_historical_lookup_request(
    request: &HistoricalRelayLookupRequest,
) -> Result<(), RelayError> {
    if request.schema != RELAY_HISTORICAL_LOOKUP_SCHEMA
        || !request.historical
        || !crate::valid_sha256(&request.object_sha256)
        || request.download_allowance_bytes == 0
    {
        return Err(RelayError::PolicyDenied);
    }
    Ok(())
}

pub fn validate_route_registration_request(
    request: &RelayRouteRegistrationRequest,
) -> Result<(), RelayError> {
    if request.schema != RELAY_ROUTE_REGISTRATION_SCHEMA
        || request.turn_local_addr.is_empty()
        || request.turn_local_addr.len() > 96
        || request.turn_local_addr.trim() != request.turn_local_addr
        || request.offer.role
            != match request.credential.claims.direction {
                RelayDirection::Upload => RelayRole::Uploader,
                RelayDirection::Download => RelayRole::Downloader,
            }
        || request.offer.session_id != request.credential.claims.session_id
        || request.offer.object_sha256 != request.credential.claims.object_sha256
    {
        return Err(RelayError::KeyAgreementRejected);
    }
    Ok(())
}

fn check_json_size(bytes: &[u8]) -> Result<(), RelayError> {
    if bytes.is_empty() || bytes.len() > MAX_BROKER_JSON_BYTES {
        Err(RelayError::CredentialInvalid)
    } else {
        Ok(())
    }
}

// Compile-time assurance that the public success types used by the server
// remain available beside the shared request DTOs.
const _: fn(AdvertisementReceipt, RelayRouteRegistrationReceipt) = |_, _| {};

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rw_community_protocol::{
        RELAY_CREDENTIAL_SCHEMA, RelayCandidateKind, RelayCredentialClaims, sign_relay_credential,
    };

    fn hash(byte: char) -> String {
        byte.to_string().repeat(64)
    }

    fn credential(role: RelayRole) -> (SignedRelayCredential, TrustedSigningKeys) {
        let key = SigningKey::from_bytes(&[29; 32]);
        let direction = match role {
            RelayRole::Uploader => RelayDirection::Upload,
            RelayRole::Downloader => RelayDirection::Download,
        };
        let signed = sign_relay_credential(
            RelayCredentialClaims {
                schema: RELAY_CREDENTIAL_SCHEMA.into(),
                relay_id: "relay-a".into(),
                session_id: "session-a".into(),
                subject_id: "subject-a".into(),
                object_sha256: hash('a'),
                direction,
                issued_unix: 100,
                not_before_unix: 100,
                expires_unix: 700,
                max_bytes: 1024,
                max_chunks: 4,
            },
            "relay-key-a",
            &key,
            100,
            &ProtocolLimits::default(),
        )
        .unwrap();
        (
            signed,
            TrustedSigningKeys::from([("relay-key-a".into(), key.verifying_key())]),
        )
    }

    #[test]
    fn participant_grant_is_strict_signed_and_redacted() {
        let (credential, keys) = credential(RelayRole::Downloader);
        let grant = ParticipantRelayGrantWire {
            schema: RELAY_PARTICIPANT_GRANT_SCHEMA.into(),
            session_id: "session-a".into(),
            object_sha256: hash('a'),
            encoded_size: 1024,
            role: RelayRole::Downloader,
            candidate: RelayCandidate {
                kind: RelayCandidateKind::Relay,
                relay_id: "relay-a".into(),
                ticket_id: "ticket-a".into(),
                expires_unix: 700,
            },
            credential,
            turn: RelayTurnAccessWire {
                urls: vec!["turn:turn.cloudflare.com:3478?transport=udp".into()],
                username: "secret-user".into(),
                credential: "secret-password".into(),
                expires_unix: 700,
            },
        };
        let bytes = serde_json::to_vec(&grant).unwrap();
        let parsed = parse_participant_grant_bounded(
            &bytes,
            &hash('a'),
            RelayRole::Downloader,
            101,
            &keys,
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(parsed, grant);
        let debug = format!("{parsed:?}");
        assert!(!debug.contains("secret-user"));
        assert!(!debug.contains("secret-password"));
    }

    #[test]
    fn direct_candidates_stun_and_cross_role_fail_closed() {
        let (credential, keys) = credential(RelayRole::Downloader);
        let mut grant = ParticipantRelayGrantWire {
            schema: RELAY_PARTICIPANT_GRANT_SCHEMA.into(),
            session_id: "session-a".into(),
            object_sha256: hash('a'),
            encoded_size: 1024,
            role: RelayRole::Downloader,
            candidate: RelayCandidate {
                kind: RelayCandidateKind::Relay,
                relay_id: "relay-a".into(),
                ticket_id: "ticket-a".into(),
                expires_unix: 700,
            },
            credential,
            turn: RelayTurnAccessWire {
                urls: vec!["stun:stun.cloudflare.com:3478".into()],
                username: "u".into(),
                credential: "c".into(),
                expires_unix: 700,
            },
        };
        assert!(
            grant
                .validate(
                    &hash('a'),
                    RelayRole::Downloader,
                    101,
                    &keys,
                    &ProtocolLimits::default(),
                )
                .is_err()
        );
        grant.turn.urls = vec!["turn:turn.cloudflare.com:3478?transport=udp".into()];
        assert!(
            grant
                .validate(
                    &hash('a'),
                    RelayRole::Uploader,
                    101,
                    &keys,
                    &ProtocolLimits::default(),
                )
                .is_err()
        );
        let hostile =
            br#"{"schema":"rw.community.relay-participant-grant.v1","peer_ip":"198.51.100.4"}"#;
        assert!(
            parse_participant_grant_bounded(
                hostile,
                &hash('a'),
                RelayRole::Downloader,
                101,
                &keys,
                &ProtocolLimits::default(),
            )
            .is_err()
        );
    }
}

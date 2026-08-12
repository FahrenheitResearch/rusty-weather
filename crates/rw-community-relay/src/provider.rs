use std::collections::BTreeSet;
use std::fmt;
use std::net::IpAddr;

use serde::Deserialize;
use zeroize::{Zeroize, Zeroizing};

use crate::{RelayError, valid_opaque_id, valid_sha256};

const MAX_PROVIDER_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_PROVIDER_URLS: usize = 32;
const MAX_PROVIDER_SECRET_BYTES: usize = 4096;

/// A provider credential request contains opaque aliases only. An integration
/// must never put an account name, email address, IP, hostname, or socket into
/// any identifier.
pub struct ProviderCredentialRequest {
    pub relay_id: String,
    pub session_id: String,
    pub object_sha256: String,
    pub participant_alias: String,
    pub expires_unix: i64,
    pub max_bytes: u64,
}

impl fmt::Debug for ProviderCredentialRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredentialRequest([redacted])")
    }
}

impl ProviderCredentialRequest {
    pub fn validate(&self, now_unix: i64) -> Result<(), RelayError> {
        if !valid_opaque_id(&self.relay_id)
            || !valid_opaque_id(&self.session_id)
            || !valid_opaque_id(&self.participant_alias)
            || !valid_sha256(&self.object_sha256)
            || self.expires_unix <= now_unix
            || self.expires_unix.saturating_sub(now_unix) > 15 * 60
            || self.max_bytes == 0
        {
            return Err(RelayError::ProviderRejected);
        }
        Ok(())
    }
}

/// Injected provider boundary. Implementations may call a provider API, but
/// this core has no network client and never receives account secrets.
pub trait RelayProvider {
    fn issue(
        &mut self,
        request: &ProviderCredentialRequest,
        now_unix: i64,
    ) -> Result<ProviderCredentialLease, RelayError>;

    fn revoke(&mut self, revocation_id: &SecretText) -> Result<(), RelayError>;
}

/// Secret provider string with redacted Debug and zeroization on drop.
#[derive(Clone)]
pub struct SecretText(Zeroizing<String>);

impl SecretText {
    pub fn new(value: String) -> Result<Self, RelayError> {
        if value.is_empty()
            || value.len() > MAX_PROVIDER_SECRET_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(RelayError::ProviderRejected);
        }
        Ok(Self(Zeroizing::new(value)))
    }

    /// Deliberate secret access for the TURN client/provider adapter only.
    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for SecretText {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretText([redacted])")
    }
}

impl Drop for SecretText {
    fn drop(&mut self) {
        // `Zeroizing` already wipes its allocation; this also protects the
        // wrapper if that implementation detail changes.
        self.0.zeroize();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnScheme {
    Turn,
    Turns,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnTransport {
    Udp,
    Tcp,
}

/// Sanitized provider endpoint. The hostname is the configured relay
/// provider's address, never another community client's address.
#[derive(Clone, PartialEq, Eq)]
pub struct TurnEndpoint {
    canonical: String,
    scheme: TurnScheme,
    transport: Option<TurnTransport>,
    host: String,
    port: u16,
}

impl fmt::Debug for TurnEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TurnEndpoint([provider relay])")
    }
}

impl TurnEndpoint {
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    pub const fn scheme(&self) -> TurnScheme {
        self.scheme
    }

    pub const fn transport(&self) -> Option<TurnTransport> {
        self.transport
    }

    pub(crate) fn host(&self) -> &str {
        &self.host
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }
}

/// Sanitized TURN-only credential material delivered to one participant.
/// This type is intentionally neither Serialize nor Deserialize.
pub struct ProviderRelayAccess {
    endpoints: Vec<TurnEndpoint>,
    username: SecretText,
    credential: SecretText,
    expires_unix: i64,
}

impl fmt::Debug for ProviderRelayAccess {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderRelayAccess([redacted TURN credential])")
    }
}

impl ProviderRelayAccess {
    /// Consume the already bounded broker wire value into the transport-only
    /// credential type. The initial deployment deliberately accepts only the
    /// audited Cloudflare TURN host/port matrix; STUN, literal-IP, unknown
    /// hosts, arbitrary ports and unsupported schemes fail closed.
    pub fn from_broker_wire(
        mut wire: crate::RelayTurnAccessWire,
        now_unix: i64,
        credential_expires_unix: i64,
    ) -> Result<Self, RelayError> {
        wire.validate(now_unix, credential_expires_unix)?;
        let adapter = CloudflareTurnAdapter::default();
        let mut endpoints = wire
            .urls
            .iter()
            .map(|url| parse_turn_url(url, &adapter.allowed_hosts))
            .collect::<Result<Vec<_>, _>>()?;
        endpoints.sort_by(|left, right| left.canonical.cmp(&right.canonical));
        endpoints.dedup_by(|left, right| left.canonical == right.canonical);
        if endpoints.is_empty()
            || !endpoints.iter().any(|endpoint| {
                endpoint.scheme == TurnScheme::Turn
                    && endpoint.transport == Some(TurnTransport::Udp)
            })
        {
            return Err(RelayError::TransportUnavailable);
        }
        Ok(Self {
            endpoints,
            username: SecretText::new(std::mem::take(&mut wire.username))?,
            credential: SecretText::new(std::mem::take(&mut wire.credential))?,
            expires_unix: wire.expires_unix,
        })
    }

    pub fn endpoints(&self) -> &[TurnEndpoint] {
        &self.endpoints
    }

    pub fn username(&self) -> &SecretText {
        &self.username
    }

    pub fn credential(&self) -> &SecretText {
        &self.credential
    }

    pub const fn expires_unix(&self) -> i64 {
        self.expires_unix
    }

    pub(crate) fn explicit_udp_endpoint(&self) -> Option<&TurnEndpoint> {
        let udp = self.endpoints.iter().filter(|endpoint| {
            endpoint.scheme == TurnScheme::Turn && endpoint.transport == Some(TurnTransport::Udp)
        });
        // Prefer Cloudflare's primary UDP port. Port 53 remains a possible
        // explicit later fallback because it is blocked on many networks.
        udp.clone()
            .find(|endpoint| endpoint.port == 3478)
            .or_else(|| udp.into_iter().next())
    }
}

pub struct ProviderCredentialLease {
    pub access: ProviderRelayAccess,
    pub revocation_id: SecretText,
}

impl fmt::Debug for ProviderCredentialLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ProviderCredentialLease([redacted])")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloudflareCredentialResponse {
    #[serde(rename = "iceServers")]
    ice_servers: Vec<CloudflareIceServer>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloudflareIceServer {
    urls: OneOrManyUrls,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    credential: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum OneOrManyUrls {
    One(String),
    Many(Vec<String>),
}

impl OneOrManyUrls {
    fn values(&self) -> Box<dyn Iterator<Item = &str> + '_> {
        match self {
            Self::One(value) => Box::new(std::iter::once(value.as_str())),
            Self::Many(values) => Box::new(values.iter().map(String::as_str)),
        }
    }
}

/// Cloudflare's standard credential response includes STUN and TURN entries.
/// This adapter permanently drops every STUN/STUNS value and admits only
/// TURN/TURNS URLs on an operator-configured provider-host allowlist.
pub struct CloudflareTurnAdapter {
    allowed_hosts: BTreeSet<String>,
}

impl Default for CloudflareTurnAdapter {
    fn default() -> Self {
        Self {
            allowed_hosts: BTreeSet::from(["turn.cloudflare.com".into()]),
        }
    }
}

impl CloudflareTurnAdapter {
    pub fn new<I, S>(hosts: I) -> Result<Self, RelayError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut allowed_hosts = BTreeSet::new();
        for host in hosts {
            let host = host.as_ref().to_ascii_lowercase();
            if !valid_dns_name(&host) || host.parse::<IpAddr>().is_ok() {
                return Err(RelayError::ProviderRejected);
            }
            allowed_hosts.insert(host);
        }
        if allowed_hosts.is_empty() || allowed_hosts.len() > 16 {
            return Err(RelayError::ProviderRejected);
        }
        // Custom Cloudflare domains retain the same documented service port
        // matrix. Another provider needs its own adapter and explicit policy;
        // this API never silently enables arbitrary ports.
        Ok(Self { allowed_hosts })
    }

    pub fn parse_and_sanitize(
        &self,
        response_json: &[u8],
        now_unix: i64,
        expires_unix: i64,
    ) -> Result<ProviderCredentialLease, RelayError> {
        if response_json.len() > MAX_PROVIDER_RESPONSE_BYTES
            || expires_unix <= now_unix
            || expires_unix.saturating_sub(now_unix) > 15 * 60
        {
            return Err(RelayError::ProviderRejected);
        }
        let response: CloudflareCredentialResponse =
            serde_json::from_slice(response_json).map_err(|_| RelayError::ProviderRejected)?;
        self.sanitize(response, expires_unix)
    }

    fn sanitize(
        &self,
        response: CloudflareCredentialResponse,
        expires_unix: i64,
    ) -> Result<ProviderCredentialLease, RelayError> {
        let mut endpoints = Vec::new();
        let mut selected_username: Option<String> = None;
        let mut selected_credential: Option<String> = None;
        let mut seen_urls = 0usize;

        for server in response.ice_servers {
            for raw_url in server.urls.values() {
                seen_urls = seen_urls.saturating_add(1);
                if seen_urls > MAX_PROVIDER_URLS {
                    return Err(RelayError::ProviderRejected);
                }
                let scheme = raw_url
                    .split_once(':')
                    .map(|(scheme, _)| scheme.to_ascii_lowercase())
                    .ok_or(RelayError::ProviderRejected)?;
                if matches!(scheme.as_str(), "stun" | "stuns") {
                    continue;
                }
                if !matches!(scheme.as_str(), "turn" | "turns") {
                    return Err(RelayError::ProviderRejected);
                }
                let endpoint = parse_turn_url(raw_url, &self.allowed_hosts)?;
                let username = server
                    .username
                    .as_ref()
                    .ok_or(RelayError::ProviderRejected)?;
                let credential = server
                    .credential
                    .as_ref()
                    .ok_or(RelayError::ProviderRejected)?;
                if let Some(selected) = &selected_username
                    && selected != username
                {
                    return Err(RelayError::ProviderRejected);
                }
                if let Some(selected) = &selected_credential
                    && selected != credential
                {
                    return Err(RelayError::ProviderRejected);
                }
                selected_username = Some(username.clone());
                selected_credential = Some(credential.clone());
                endpoints.push(endpoint);
            }
        }

        endpoints.sort_by(|left, right| left.canonical.cmp(&right.canonical));
        endpoints.dedup_by(|left, right| left.canonical == right.canonical);
        if endpoints.is_empty() {
            return Err(RelayError::ProviderRejected);
        }

        let username = selected_username.ok_or(RelayError::ProviderRejected)?;
        Ok(ProviderCredentialLease {
            access: ProviderRelayAccess {
                endpoints,
                username: SecretText::new(username.clone())?,
                credential: SecretText::new(
                    selected_credential.ok_or(RelayError::ProviderRejected)?,
                )?,
                expires_unix,
            },
            // Cloudflare's revocation endpoint is keyed by the generated TURN
            // username. Deriving it here prevents a caller from revoking an
            // unrelated credential identifier.
            revocation_id: SecretText::new(username)?,
        })
    }
}

fn parse_turn_url(
    raw_url: &str,
    allowed_hosts: &BTreeSet<String>,
) -> Result<TurnEndpoint, RelayError> {
    if raw_url.len() > 512
        || raw_url.chars().any(char::is_whitespace)
        || raw_url.chars().any(char::is_control)
        || raw_url.contains(['/', '\\', '@', '#'])
    {
        return Err(RelayError::ProviderRejected);
    }
    let (raw_scheme, rest) = raw_url
        .split_once(':')
        .ok_or(RelayError::ProviderRejected)?;
    let (scheme, scheme_text) = match raw_scheme.to_ascii_lowercase().as_str() {
        "turn" => (TurnScheme::Turn, "turn"),
        "turns" => (TurnScheme::Turns, "turns"),
        _ => return Err(RelayError::ProviderRejected),
    };
    let (authority, raw_query) = match rest.split_once('?') {
        Some((authority, query)) => (authority, Some(query)),
        None => (rest, None),
    };
    if authority.is_empty() || authority.contains('?') || authority.contains('[') {
        return Err(RelayError::ProviderRejected);
    }
    let (host, port) = authority
        .rsplit_once(':')
        .ok_or(RelayError::ProviderRejected)?;
    let host = host.to_ascii_lowercase();
    let port = port
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or(RelayError::ProviderRejected)?;
    if !valid_dns_name(&host) || host.parse::<IpAddr>().is_ok() || !allowed_hosts.contains(&host) {
        return Err(RelayError::ProviderRejected);
    }
    let transport = match raw_query {
        None => None,
        Some("transport=udp") => Some(TurnTransport::Udp),
        Some("transport=tcp") => Some(TurnTransport::Tcp),
        Some(_) => return Err(RelayError::ProviderRejected),
    };
    if !matches!(
        (scheme, transport, port),
        (TurnScheme::Turn, Some(TurnTransport::Udp), 3478 | 53)
            | (TurnScheme::Turn, Some(TurnTransport::Tcp), 3478 | 80)
            | (TurnScheme::Turns, Some(TurnTransport::Tcp), 5349 | 443)
    ) {
        return Err(RelayError::ProviderRejected);
    }
    let canonical = match transport {
        None => format!("{scheme_text}:{host}:{port}"),
        Some(TurnTransport::Udp) => format!("{scheme_text}:{host}:{port}?transport=udp"),
        Some(TurnTransport::Tcp) => format!("{scheme_text}:{host}:{port}?transport=tcp"),
    };
    Ok(TurnEndpoint {
        canonical,
        scheme,
        transport,
        host,
        port,
    })
}

fn valid_dns_name(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host.contains('.')
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .as_bytes()
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloudflare_adapter_strips_stun_and_keeps_only_allowlisted_turn() {
        let json = br#"{
          "iceServers": [
            {"urls": ["stun:stun.cloudflare.com:3478"]},
            {"urls": [
              "turn:turn.cloudflare.com:3478?transport=udp",
              "turn:turn.cloudflare.com:3478?transport=tcp",
              "turns:turn.cloudflare.com:5349?transport=tcp"
            ], "username": "short-lived-user", "credential": "short-lived-secret"}
          ]
        }"#;
        let lease = CloudflareTurnAdapter::default()
            .parse_and_sanitize(json, 100, 700)
            .unwrap();
        assert_eq!(lease.access.endpoints().len(), 3);
        assert!(
            lease
                .access
                .endpoints()
                .iter()
                .all(|endpoint| matches!(endpoint.scheme(), TurnScheme::Turn | TurnScheme::Turns))
        );
        assert!(
            lease
                .access
                .endpoints()
                .iter()
                .all(|endpoint| !endpoint.canonical().starts_with("stun:"))
        );
        assert_eq!(lease.access.username().expose(), "short-lived-user");
        assert!(!format!("{lease:?}").contains("short-lived"));
    }

    #[test]
    fn cloudflare_adapter_rejects_unknown_direct_and_address_endpoints() {
        for raw in [
            "https:turn.cloudflare.com:443",
            "turn:203.0.113.8:3478?transport=udp",
            "turn:peer.example:3478?transport=udp",
            "turn:turn.cloudflare.com:3478?transport=sctp",
            "turn:user@turn.cloudflare.com:3478",
            "turn:turn.cloudflare.com:22?transport=udp",
            "turns:turn.cloudflare.com:3478?transport=tcp",
            "turns:turn.cloudflare.com:5349?transport=udp",
        ] {
            let json = format!(
                r#"{{"iceServers":[{{"urls":["{raw}"],"username":"u","credential":"c"}}]}}"#
            );
            assert!(matches!(
                CloudflareTurnAdapter::default().parse_and_sanitize(json.as_bytes(), 100, 700),
                Err(RelayError::ProviderRejected)
            ));
        }
    }

    #[test]
    fn broker_wire_becomes_only_redacted_sanitized_turn_access() {
        let wire = crate::RelayTurnAccessWire {
            urls: vec![
                "turn:turn.cloudflare.com:3478?transport=udp".into(),
                "turns:turn.cloudflare.com:5349?transport=tcp".into(),
            ],
            username: "short-lived-user".into(),
            credential: "short-lived-secret".into(),
            expires_unix: 700,
        };
        let access = ProviderRelayAccess::from_broker_wire(wire, 100, 700).unwrap();
        assert_eq!(access.endpoints().len(), 2);
        assert_eq!(access.username().expose(), "short-lived-user");
        let rendered = format!("{access:?}");
        assert!(!rendered.contains("short-lived"));
        assert!(!rendered.contains("turn.cloudflare.com"));

        let unsafe_wire = crate::RelayTurnAccessWire {
            urls: vec!["turn:peer.example:3478?transport=udp".into()],
            username: "secret-user".into(),
            credential: "secret-password".into(),
            expires_unix: 700,
        };
        let error = ProviderRelayAccess::from_broker_wire(unsafe_wire, 100, 700).unwrap_err();
        assert_eq!(error, RelayError::ProviderRejected);
        assert!(!error.to_string().contains("peer.example"));
    }

    #[test]
    fn provider_errors_and_debug_never_echo_raw_addresses_or_secrets() {
        let json = br#"{"iceServers":[{"urls":"turn:192.0.2.55:3478","username":"secret-user","credential":"secret-password"}]}"#;
        let error = CloudflareTurnAdapter::default()
            .parse_and_sanitize(json, 1, 2)
            .unwrap_err();
        let rendered = error.to_string();
        assert!(!rendered.contains("192.0.2.55"));
        assert!(!rendered.contains("secret-user"));
        assert!(!rendered.contains("secret-password"));
    }
}

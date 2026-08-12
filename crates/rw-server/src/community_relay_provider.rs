//! Hardened Cloudflare TURN credential boundary for Community Cache.
//!
//! The long-lived TURN key remains in a permission-restricted server file.
//! Every call uses fresh DNS resolution, pins one globally routable answer,
//! preserves the configured hostname for TLS verification, disables redirects
//! and proxies, and returns only the relay crate's sanitized TURN-only lease.

use std::fmt;
use std::fs;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use rw_community_relay::{
    CloudflareTurnAdapter, ProviderCredentialLease, ProviderCredentialRequest, RelayError,
    RelayProvider, SecretText,
};
use serde::{Deserialize, Serialize};
use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector as _, RustlsConnector, TcpConnector};

const CLOUDFLARE_API_HOST: &str = "rtc.live.cloudflare.com";
const CLOUDFLARE_API_ORIGIN: &str = "https://rtc.live.cloudflare.com";
const MAX_SECRET_BYTES: u64 = 64 * 1024;
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;
const MAX_DNS_ANSWERS: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct CloudflareProviderTimeouts {
    pub resolve: Duration,
    pub connect: Duration,
    pub send: Duration,
    pub receive: Duration,
    pub global: Duration,
}

impl Default for CloudflareProviderTimeouts {
    fn default() -> Self {
        Self {
            resolve: Duration::from_secs(3),
            connect: Duration::from_secs(5),
            send: Duration::from_secs(5),
            receive: Duration::from_secs(10),
            global: Duration::from_secs(15),
        }
    }
}

impl CloudflareProviderTimeouts {
    pub fn validate(self) -> Result<(), RelayError> {
        let values = [
            self.resolve,
            self.connect,
            self.send,
            self.receive,
            self.global,
        ];
        if values
            .iter()
            .any(|value| value.is_zero() || *value > Duration::from_secs(60))
            || self.global < self.resolve
            || self.global < self.connect
            || self.global < self.send
            || self.global < self.receive
        {
            return Err(RelayError::PolicyDenied);
        }
        Ok(())
    }
}

pub struct CloudflareRelayProvider {
    turn_key_id: String,
    api_token: SecretText,
    adapter: CloudflareTurnAdapter,
    transport: Box<dyn CloudflareTransport>,
}

impl fmt::Debug for CloudflareRelayProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CloudflareRelayProvider([redacted])")
    }
}

impl CloudflareRelayProvider {
    pub fn open(
        turn_key_id: String,
        api_token_file: &Path,
        allowed_turn_hosts: &[String],
        timeouts: CloudflareProviderTimeouts,
    ) -> Result<Self, RelayError> {
        validate_turn_key_id(&turn_key_id)?;
        timeouts.validate()?;
        let api_token = SecretText::new(
            read_secret(api_token_file).map_err(|_| RelayError::ProviderUnavailable)?,
        )?;
        let adapter = CloudflareTurnAdapter::new(allowed_turn_hosts)?;
        Ok(Self {
            turn_key_id,
            api_token,
            adapter,
            transport: Box::new(PinnedCloudflareTransport::new(timeouts)),
        })
    }

    #[cfg(test)]
    fn with_transport(
        turn_key_id: String,
        api_token: String,
        allowed_turn_hosts: &[String],
        transport: Box<dyn CloudflareTransport>,
    ) -> Result<Self, RelayError> {
        validate_turn_key_id(&turn_key_id)?;
        Ok(Self {
            turn_key_id,
            api_token: SecretText::new(api_token)?,
            adapter: CloudflareTurnAdapter::new(allowed_turn_hosts)?,
            transport,
        })
    }
}

impl RelayProvider for CloudflareRelayProvider {
    fn issue(
        &mut self,
        request: &ProviderCredentialRequest,
        now_unix: i64,
    ) -> Result<ProviderCredentialLease, RelayError> {
        request.validate(now_unix)?;
        let ttl = request.expires_unix.saturating_sub(now_unix);
        if !(1..=15 * 60).contains(&ttl) {
            return Err(RelayError::ProviderRejected);
        }
        let path = format!("/v1/turn/keys/{}/credentials/generate", self.turn_key_id);
        let body = serde_json::to_vec(&GenerateCredentialRequest {
            ttl,
            custom_identifier: &request.participant_alias,
        })
        .map_err(|_| RelayError::ProviderRejected)?;
        let response = self
            .transport
            .post(&path, &self.api_token, Some(&body), ExpectedStatus::Created)
            .map_err(|_| RelayError::ProviderUnavailable)?;
        // `/credentials/generate` is Cloudflare's documented endpoint for
        // attaching `customIdentifier`. Its response uses one ICE server
        // object, while `/generate-ice-servers` returns an array. Normalize
        // only that strict shape before applying the relay crate's TURN-only
        // host/scheme/port parser.
        let response = normalize_generate_response(&response)?;
        let lease = self
            .adapter
            .parse_and_sanitize(&response, now_unix, request.expires_unix)?;
        if !valid_path_segment(lease.revocation_id.expose()) {
            return Err(RelayError::ProviderRejected);
        }
        Ok(lease)
    }

    fn revoke(&mut self, revocation_id: &SecretText) -> Result<(), RelayError> {
        if !valid_path_segment(revocation_id.expose()) {
            return Err(RelayError::ProviderRejected);
        }
        let path = format!(
            "/v1/turn/keys/{}/credentials/{}/revoke",
            self.turn_key_id,
            revocation_id.expose()
        );
        self.transport
            .post(&path, &self.api_token, None, ExpectedStatus::NoContent)
            .map(|_| ())
            .map_err(|_| RelayError::ProviderUnavailable)
    }
}

#[derive(Serialize)]
struct GenerateCredentialRequest<'a> {
    ttl: i64,
    #[serde(rename = "customIdentifier")]
    custom_identifier: &'a str,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerateCredentialResponse {
    #[serde(rename = "iceServers")]
    ice_servers: GenerateIceServer,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GenerateIceServer {
    urls: OneOrManyString,
    username: String,
    credential: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum OneOrManyString {
    One(String),
    Many(Vec<String>),
}

fn normalize_generate_response(response: &[u8]) -> Result<Vec<u8>, RelayError> {
    if response.len() > MAX_RESPONSE_BYTES as usize {
        return Err(RelayError::ProviderRejected);
    }
    let response: GenerateCredentialResponse =
        serde_json::from_slice(response).map_err(|_| RelayError::ProviderRejected)?;
    serde_json::to_vec(&serde_json::json!({ "iceServers": [response.ice_servers] }))
        .map_err(|_| RelayError::ProviderRejected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedStatus {
    Created,
    NoContent,
}

trait CloudflareTransport: fmt::Debug + Send {
    fn post(
        &self,
        path: &str,
        api_token: &SecretText,
        body: Option<&[u8]>,
        expected: ExpectedStatus,
    ) -> Result<Vec<u8>, RelayError>;
}

#[derive(Debug)]
struct PinnedCloudflareTransport {
    timeouts: CloudflareProviderTimeouts,
    dns: CloudflareDnsPool,
}

impl PinnedCloudflareTransport {
    fn new(timeouts: CloudflareProviderTimeouts) -> Self {
        Self {
            timeouts,
            dns: CloudflareDnsPool::new(),
        }
    }
}

impl CloudflareTransport for PinnedCloudflareTransport {
    fn post(
        &self,
        path: &str,
        api_token: &SecretText,
        body: Option<&[u8]>,
        expected: ExpectedStatus,
    ) -> Result<Vec<u8>, RelayError> {
        if !valid_provider_path(path) {
            return Err(RelayError::ProviderRejected);
        }
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();
        let resolver = CloudflarePinnedResolver {
            dns: self.dns.clone(),
            rejected: Arc::new(Mutex::new(false)),
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
        let agent = ureq::Agent::with_parts(config, connector, resolver);
        let url = format!("{CLOUDFLARE_API_ORIGIN}{path}");
        let request = agent
            .post(&url)
            .header("authorization", format!("Bearer {}", api_token.expose()))
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("user-agent", "rusty-weather-community-relay/1");
        let result = match body {
            Some(body) => request.send(body),
            None => request.send_empty(),
        };
        let mut response = result.map_err(|_| RelayError::ProviderUnavailable)?;
        let accepted = match expected {
            ExpectedStatus::Created => response.status().as_u16() == 201,
            ExpectedStatus::NoContent => response.status().as_u16() == 204,
        };
        if !accepted {
            return Err(RelayError::ProviderUnavailable);
        }
        if expected == ExpectedStatus::NoContent {
            return Ok(Vec::new());
        }
        response
            .body_mut()
            .with_config()
            .limit(MAX_RESPONSE_BYTES)
            .read_to_vec()
            .map_err(|_| RelayError::ProviderUnavailable)
    }
}

#[derive(Debug)]
enum DnsFailure {
    Busy,
    Timeout,
    Io,
}

struct DnsJob {
    response: mpsc::SyncSender<Result<Vec<SocketAddr>, io::Error>>,
}

#[derive(Clone)]
struct CloudflareDnsPool {
    sender: Arc<mpsc::SyncSender<DnsJob>>,
}

impl fmt::Debug for CloudflareDnsPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CloudflareDnsPool")
    }
}

impl CloudflareDnsPool {
    fn new() -> Self {
        let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
        let _ = thread::Builder::new()
            .name("rw-community-relay-dns".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    let result = (CLOUDFLARE_API_HOST, 443)
                        .to_socket_addrs()
                        .map(|addresses| {
                            addresses.take(MAX_DNS_ANSWERS.saturating_add(1)).collect()
                        });
                    let _ = job.response.send(result);
                }
            });
        Self {
            sender: Arc::new(sender),
        }
    }

    fn resolve(&self, timeout: Duration) -> Result<Vec<SocketAddr>, DnsFailure> {
        let (response, receiver) = mpsc::sync_channel(1);
        self.sender
            .try_send(DnsJob { response })
            .map_err(|_| DnsFailure::Busy)?;
        match receiver.recv_timeout(timeout) {
            Ok(Ok(addresses)) => Ok(addresses),
            Ok(Err(_)) => Err(DnsFailure::Io),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(DnsFailure::Timeout),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(DnsFailure::Io),
        }
    }
}

#[derive(Clone)]
struct CloudflarePinnedResolver {
    dns: CloudflareDnsPool,
    rejected: Arc<Mutex<bool>>,
}

impl fmt::Debug for CloudflarePinnedResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CloudflarePinnedResolver")
    }
}

impl CloudflarePinnedResolver {
    fn reject(&self) -> ureq::Error {
        if let Ok(mut rejected) = self.rejected.lock() {
            *rejected = true;
        }
        ureq::Error::HostNotFound
    }
}

impl Resolver for CloudflarePinnedResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if uri.scheme_str() != Some("https")
            || uri.host() != Some(CLOUDFLARE_API_HOST)
            || uri.port_u16().unwrap_or(443) != 443
        {
            return Err(self.reject());
        }
        let mut addresses = self
            .dns
            .resolve(*timeout.after)
            .map_err(|error| match error {
                DnsFailure::Timeout => ureq::Error::Timeout(timeout.reason),
                DnsFailure::Busy | DnsFailure::Io => ureq::Error::HostNotFound,
            })?;
        if addresses.is_empty()
            || addresses.len() > MAX_DNS_ANSWERS
            || addresses
                .iter()
                .any(|address| address.port() != 443 || !is_global_ip(address.ip()))
        {
            return Err(self.reject());
        }
        addresses.sort_unstable();
        addresses.dedup();
        let selected = addresses.into_iter().next().ok_or_else(|| self.reject())?;
        let mut result = self.empty();
        result.push(selected);
        Ok(result)
    }
}

fn validate_turn_key_id(value: &str) -> Result<(), RelayError> {
    if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RelayError::ProviderRejected);
    }
    Ok(())
}

fn valid_path_segment(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_provider_path(path: &str) -> bool {
    path.starts_with("/v1/turn/keys/")
        && path.len() <= 512
        && !path.contains(['?', '#', '\\', '\0'])
        && path
            .split('/')
            .skip(1)
            .all(|segment| !segment.is_empty() && valid_path_segment(segment))
}

fn read_secret(path: &Path) -> Result<String, io::Error> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_SECRET_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe relay provider secret file",
        ));
    }
    validate_private_permissions(&metadata)?;
    let value = fs::read_to_string(path)?;
    let value = value.trim();
    if value.len() < 32 || value.chars().any(char::is_control) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid relay provider secret",
        ));
    }
    Ok(value.to_string())
}

#[cfg(unix)]
fn validate_private_permissions(metadata: &fs::Metadata) -> Result<(), io::Error> {
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "relay provider secret permissions are too broad",
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_permissions(_metadata: &fs::Metadata) -> Result<(), io::Error> {
    Ok(())
}

fn is_global_ip(address: IpAddr) -> bool {
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
            let mapped_global = address
                .to_ipv4_mapped()
                .is_none_or(|mapped| is_global_ip(IpAddr::V4(mapped)));
            segments[0] & 0xe000 == 0x2000
                && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
                && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
                && segments[0] != 0x2002
                && segments[0] != 0x3fff
                && mapped_global
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Debug, Clone)]
    struct RecordedRequest {
        path: String,
        body: Option<Vec<u8>>,
        expected: ExpectedStatus,
    }

    #[derive(Debug)]
    struct FakeTransport {
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        replies: Mutex<VecDeque<Result<Vec<u8>, RelayError>>>,
    }

    impl CloudflareTransport for FakeTransport {
        fn post(
            &self,
            path: &str,
            _api_token: &SecretText,
            body: Option<&[u8]>,
            expected: ExpectedStatus,
        ) -> Result<Vec<u8>, RelayError> {
            self.requests.lock().unwrap().push(RecordedRequest {
                path: path.into(),
                body: body.map(ToOwned::to_owned),
                expected,
            });
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(RelayError::ProviderUnavailable))
        }
    }

    fn provider_with(
        replies: Vec<Result<Vec<u8>, RelayError>>,
    ) -> (CloudflareRelayProvider, Arc<Mutex<Vec<RecordedRequest>>>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let transport = FakeTransport {
            requests: requests.clone(),
            replies: Mutex::new(replies.into()),
        };
        (
            CloudflareRelayProvider::with_transport(
                "0123456789abcdef0123456789abcdef".into(),
                "server-only-cloudflare-token-value".into(),
                &["turn.cloudflare.com".into()],
                Box::new(transport),
            )
            .unwrap(),
            requests,
        )
    }

    fn request() -> ProviderCredentialRequest {
        ProviderCredentialRequest {
            relay_id: "cloudflare-turn".into(),
            session_id: "session-opaque".into(),
            object_sha256: "a".repeat(64),
            participant_alias: "subject-session-alias".into(),
            expires_unix: 700,
            max_bytes: 1024,
        }
    }

    #[test]
    fn issue_uses_exact_path_ttl_alias_and_strips_stun() {
        // Official `/credentials/generate` fixture: unlike
        // `/generate-ice-servers`, `iceServers` is one credential object.
        let response = br#"{"iceServers":{
          "urls":["stun:stun.cloudflare.com:3478","turn:turn.cloudflare.com:3478?transport=udp"],
          "username":"generated-user","credential":"generated-secret"
        }}"#
        .to_vec();
        let (mut provider, requests) = provider_with(vec![Ok(response)]);
        let lease = provider.issue(&request(), 100).unwrap();
        assert_eq!(lease.access.endpoints().len(), 1);
        assert!(lease.access.endpoints()[0].canonical().starts_with("turn:"));
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[0].path,
            "/v1/turn/keys/0123456789abcdef0123456789abcdef/credentials/generate"
        );
        assert_eq!(requests[0].expected, ExpectedStatus::Created);
        let body: serde_json::Value =
            serde_json::from_slice(requests[0].body.as_deref().unwrap()).unwrap();
        assert_eq!(body["ttl"], 600);
        assert_eq!(body["customIdentifier"], "subject-session-alias");
        let rendered = format!("{provider:?}");
        assert!(!rendered.contains("server-only"));
        assert!(!rendered.contains("generated"));
    }

    #[test]
    fn revoke_uses_only_the_sanitized_generated_username() {
        let response = br#"{"iceServers":{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"generated-user","credential":"generated-secret"}}"#.to_vec();
        let (mut provider, requests) = provider_with(vec![Ok(response), Ok(Vec::new())]);
        let lease = provider.issue(&request(), 100).unwrap();
        provider.revoke(&lease.revocation_id).unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests[1].path,
            "/v1/turn/keys/0123456789abcdef0123456789abcdef/credentials/generated-user/revoke"
        );
        assert_eq!(requests[1].expected, ExpectedStatus::NoContent);
        assert!(requests[1].body.is_none());
    }

    #[test]
    fn provider_failures_and_malformed_identity_are_coarse() {
        let (mut provider, _) = provider_with(vec![Err(RelayError::ProviderUnavailable)]);
        let error = provider.issue(&request(), 100).unwrap_err();
        assert_eq!(error, RelayError::ProviderUnavailable);
        let rendered = error.to_string();
        assert!(!rendered.contains(CLOUDFLARE_API_HOST));
        assert!(!rendered.contains("server-only"));
        assert!(
            CloudflareRelayProvider::with_transport(
                "../not-a-key".into(),
                "server-only-cloudflare-token-value".into(),
                &["turn.cloudflare.com".into()],
                Box::new(FakeTransport {
                    requests: Arc::new(Mutex::new(Vec::new())),
                    replies: Mutex::new(VecDeque::new()),
                }),
            )
            .is_err()
        );
    }

    #[test]
    fn special_address_policy_is_conservative() {
        for address in [
            "10.0.0.1",
            "100.64.0.1",
            "192.0.2.1",
            "198.18.0.1",
            "224.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_global_ip(address.parse().unwrap()));
        }
        assert!(is_global_ip("104.16.0.1".parse().unwrap()));
        assert!(is_global_ip("2606:4700::1".parse().unwrap()));
    }
}

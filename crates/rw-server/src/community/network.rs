//! Hardened conventional HTTPS networking for the phase-one Community
//! origin and hot-object gateway. This is intentionally separate from the
//! relay transport and never discovers or connects to another BowEcho user.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs as _};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use ureq::unversioned::resolver::{ResolvedSocketAddrs, Resolver};
use ureq::unversioned::transport::{Connector as _, RustlsConnector, TcpConnector};

use super::CommunityError;

const MAX_REMOTE_BASE_URL_BYTES: usize = 512;
const MAX_REMOTE_BASE_PATH_BYTES: usize = 160;
const MAX_DNS_ANSWERS: usize = 16;

/// Network scope is explicit. Normal public R2/Hetzner endpoints must use
/// `PublicInternetOnly`; an operator who truly needs an RFC1918 gateway must
/// separately opt into `ExplicitPrivateOperatorNetwork` in server config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteNetworkPolicy {
    PublicInternetOnly,
    ExplicitPrivateOperatorNetwork,
}

/// Canonical HTTPS endpoint root. A bounded base path is permitted because
/// operator gateways are commonly mounted below `/api`; authority and host
/// remain exact and credentials can never be redirected or embedded in it.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct StrictHttpsBaseUrl {
    value: String,
}

impl fmt::Debug for StrictHttpsBaseUrl {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("StrictHttpsBaseUrl")
            .field(&self.value)
            .finish()
    }
}

impl StrictHttpsBaseUrl {
    pub(crate) fn parse(
        value: &str,
        allow_base_path: bool,
        network_policy: RemoteNetworkPolicy,
    ) -> Result<Self, CommunityError> {
        if value.is_empty()
            || value.len() > MAX_REMOTE_BASE_URL_BYTES
            || !value.is_ascii()
            || !value.starts_with("https://")
            || value
                .chars()
                .any(|character| character.is_ascii_control() || character.is_ascii_whitespace())
            || value.contains(['\\', '@', '?', '#'])
        {
            return Err(invalid_base_url());
        }
        let remainder = &value[8..];
        let authority_end = remainder.find('/').unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        let path = &remainder[authority_end..];
        let (host, port) = split_authority(authority)?;
        if host.is_empty()
            || host.len() > 253
            || host != host.to_ascii_lowercase()
            || host.ends_with('.')
            || !canonical_host(host, network_policy)
            || port.is_some_and(|port| port == 0)
            || (!allow_base_path && !path.is_empty())
            || (allow_base_path && !canonical_base_path(path))
        {
            return Err(invalid_base_url());
        }
        if network_policy == RemoteNetworkPolicy::PublicInternetOnly && port.is_some() {
            // Public services use canonical HTTPS/443. Non-default ports are
            // allowed only by the separate private-operator policy.
            return Err(invalid_base_url());
        }
        Ok(Self {
            value: value.to_owned(),
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.value
    }

    pub(crate) fn endpoint(&self, path: &str) -> Result<String, CommunityError> {
        if path.len() > 512
            || !path.starts_with("/v1/")
            || path.contains(['\\', '?', '#', '%'])
            || path.contains("//")
            || path.split('/').any(|part| matches!(part, "." | ".."))
            || !path.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.')
            })
        {
            return Err(CommunityError::Invalid("remote API path is invalid".into()));
        }
        Ok(format!("{}{}", self.value, path))
    }
}

fn invalid_base_url() -> CommunityError {
    CommunityError::Invalid("remote endpoint must be a canonical HTTPS base URL".into())
}

fn split_authority(authority: &str) -> Result<(&str, Option<u16>), CommunityError> {
    if authority.is_empty()
        || authority.starts_with('[')
        || authority.ends_with(']')
        || authority.matches(':').count() > 1
    {
        return Err(invalid_base_url());
    }
    match authority.split_once(':') {
        Some((host, raw_port)) => {
            if raw_port.is_empty()
                || (raw_port.len() > 1 && raw_port.starts_with('0'))
                || !raw_port.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(invalid_base_url());
            }
            let port = raw_port.parse::<u16>().map_err(|_| invalid_base_url())?;
            Ok((host, Some(port)))
        }
        None => Ok((authority, None)),
    }
}

fn canonical_host(host: &str, network_policy: RemoteNetworkPolicy) -> bool {
    if host.parse::<IpAddr>().is_ok() {
        return network_policy == RemoteNetworkPolicy::ExplicitPrivateOperatorNetwork;
    }
    let labels_are_canonical = host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    });
    if !labels_are_canonical {
        return false;
    }
    match network_policy {
        RemoteNetworkPolicy::PublicInternetOnly => host.contains('.') && !is_private_dns_name(host),
        RemoteNetworkPolicy::ExplicitPrivateOperatorNetwork => true,
    }
}

fn is_private_dns_name(host: &str) -> bool {
    [
        "localhost",
        ".localhost",
        ".local",
        ".internal",
        ".lan",
        ".home",
        ".test",
        ".invalid",
        ".example",
        ".onion",
    ]
    .iter()
    .any(|suffix| host == suffix.trim_start_matches('.') || host.ends_with(suffix))
}

fn canonical_base_path(path: &str) -> bool {
    path.is_empty()
        || (path.len() <= MAX_REMOTE_BASE_PATH_BYTES
            && path.starts_with('/')
            && !path.ends_with('/')
            && !path.contains("//")
            && !path.contains('%')
            && !path.split('/').any(|part| matches!(part, "." | ".."))
            && path.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~')
            }))
}

/// Builds a no-proxy, no-redirect agent with a fresh DNS resolution for each
/// connection. Public mode rejects the entire answer set if any address is
/// non-global, then pins one approved socket while rustls still verifies the
/// original hostname. This closes redirect, DNS-rebinding, and mixed-answer
/// bearer-token exfiltration paths.
pub(crate) fn hardened_https_agent(
    network_policy: RemoteNetworkPolicy,
    connect_timeout: Duration,
    call_timeout: Duration,
) -> Result<ureq::Agent, CommunityError> {
    if connect_timeout.is_zero() || call_timeout.is_zero() || call_timeout < connect_timeout {
        return Err(CommunityError::Invalid(
            "remote HTTPS timeouts are invalid".into(),
        ));
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let resolver = PolicyResolver {
        policy: network_policy,
        dns: BoundedDnsPool::new(2),
    };
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .https_only(true)
        .proxy(None)
        .max_redirects(0)
        .max_idle_connections(0)
        .timeout_connect(Some(connect_timeout))
        .timeout_per_call(Some(call_timeout))
        .timeout_resolve(Some(connect_timeout))
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
    Ok(ureq::Agent::with_parts(config, connector, resolver))
}

#[derive(Debug, Clone)]
struct BoundedDnsPool {
    senders: Arc<Vec<mpsc::SyncSender<DnsJob>>>,
    cursor: Arc<AtomicUsize>,
}

struct DnsJob {
    lookup: String,
    response: mpsc::SyncSender<std::io::Result<Vec<SocketAddr>>>,
}

impl BoundedDnsPool {
    fn new(workers: usize) -> Self {
        let mut senders = Vec::new();
        for index in 0..workers {
            let (sender, receiver) = mpsc::sync_channel::<DnsJob>(1);
            if thread::Builder::new()
                .name(format!("rw-community-dns-{index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv() {
                        let result = job.lookup.to_socket_addrs().map(|answers| {
                            answers
                                .take(MAX_DNS_ANSWERS.saturating_add(1))
                                .collect::<Vec<_>>()
                        });
                        let _ = job.response.send(result);
                    }
                })
                .is_ok()
            {
                senders.push(sender);
            }
        }
        Self {
            senders: Arc::new(senders),
            cursor: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn resolve(&self, lookup: String, timeout: Duration) -> Result<Vec<SocketAddr>, ()> {
        if self.senders.is_empty() {
            return Err(());
        }
        let (response, receiver) = mpsc::sync_channel(1);
        let mut job = Some(DnsJob { lookup, response });
        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.senders.len();
        for offset in 0..self.senders.len() {
            let index = (start + offset) % self.senders.len();
            match self.senders[index].try_send(job.take().expect("DNS job retained")) {
                Ok(()) => {
                    return receiver
                        .recv_timeout(timeout)
                        .ok()
                        .and_then(Result::ok)
                        .ok_or(());
                }
                Err(mpsc::TrySendError::Full(returned))
                | Err(mpsc::TrySendError::Disconnected(returned)) => job = Some(returned),
            }
        }
        Err(())
    }
}

#[derive(Debug, Clone)]
struct PolicyResolver {
    policy: RemoteNetworkPolicy,
    dns: BoundedDnsPool,
}

impl Resolver for PolicyResolver {
    fn resolve(
        &self,
        uri: &ureq::http::Uri,
        _config: &ureq::config::Config,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<ResolvedSocketAddrs, ureq::Error> {
        if uri.scheme_str() != Some("https") {
            return Err(ureq::Error::HostNotFound);
        }
        let host = uri.host().ok_or(ureq::Error::HostNotFound)?;
        let port = uri.port_u16().unwrap_or(443);
        let answers = self
            .dns
            .resolve(format!("{host}:{port}"), *timeout.after)
            .map_err(|()| ureq::Error::HostNotFound)?;
        let selected = validate_and_pin_dns_answers(answers, self.policy)
            .map_err(|()| ureq::Error::HostNotFound)?;
        let mut result = self.empty();
        result.push(selected);
        Ok(result)
    }
}

fn validate_and_pin_dns_answers(
    mut addresses: Vec<SocketAddr>,
    policy: RemoteNetworkPolicy,
) -> Result<SocketAddr, ()> {
    if addresses.is_empty()
        || addresses.len() > MAX_DNS_ANSWERS
        || (policy == RemoteNetworkPolicy::PublicInternetOnly
            && addresses.iter().any(|address| !is_global_ip(address.ip())))
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
    in_ipv6_prefix(
        value,
        u128::from(Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0)),
        3,
    ) && ![
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_public_urls_and_explicit_private_policy_are_distinct() {
        let public = RemoteNetworkPolicy::PublicInternetOnly;
        let private = RemoteNetworkPolicy::ExplicitPrivateOperatorNetwork;
        assert!(StrictHttpsBaseUrl::parse("https://weather.example.org/api", true, public).is_ok());
        assert!(StrictHttpsBaseUrl::parse("https://localhost:8443/api", true, public).is_err());
        assert!(StrictHttpsBaseUrl::parse("https://localhost:8443/api", true, private).is_ok());
        assert!(StrictHttpsBaseUrl::parse("https://127.0.0.1:8443", true, public).is_err());
        assert!(StrictHttpsBaseUrl::parse("https://127.0.0.1:8443", true, private).is_ok());
    }

    #[test]
    fn mixed_or_private_public_dns_answers_fail_closed() {
        let public = RemoteNetworkPolicy::PublicInternetOnly;
        let private = RemoteNetworkPolicy::ExplicitPrivateOperatorNetwork;
        let mixed = vec![
            SocketAddr::from(([1, 1, 1, 1], 443)),
            SocketAddr::from(([127, 0, 0, 1], 443)),
        ];
        assert!(validate_and_pin_dns_answers(mixed.clone(), public).is_err());
        assert!(validate_and_pin_dns_answers(mixed, private).is_ok());
        assert!(
            validate_and_pin_dns_answers(vec![SocketAddr::from(([198, 51, 100, 2], 443))], public)
                .is_err()
        );
    }
}

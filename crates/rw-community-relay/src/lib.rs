//! Relay-only Community Cache security and data-plane primitives.
//!
//! This crate does **not** implement ICE, STUN, direct sockets, LAN discovery,
//! or a fallback transport. Hetzner and R2 remain ordinary HTTPS tiers. The
//! only job of this crate is cold-object recovery through an operator-selected
//! privacy relay after local and R2 misses.
//!
//! Provider responses, rendezvous state, public failures, and audit events use
//! closed structures. They never contain a peer address or a provider error
//! string. A transport integration must remain feature-disabled until its
//! packet-level test proves that each client connects only to the configured
//! relay.

#![forbid(unsafe_code)]

mod control;
mod crypto;
mod data_plane;
mod provider;
mod route;

pub use control::*;
pub use crypto::*;
pub use data_plane::*;
pub use provider::*;
pub use route::*;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Closed, non-sensitive failure categories. No variant carries provider
/// text, a URL, a socket address, a subject identifier, or credentials.
#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum RelayError {
    #[error("community relay is disabled")]
    Disabled,
    #[error("community relay security gates are incomplete")]
    SecurityGate,
    #[error("relay provider response was rejected")]
    ProviderRejected,
    #[error("relay provider is unavailable")]
    ProviderUnavailable,
    #[error("origin-signed object identity was rejected")]
    UntrustedObject,
    #[error("relay identifier was rejected")]
    UnsafeIdentifier,
    #[error("relay credential was rejected")]
    CredentialInvalid,
    #[error("relay credential is expired")]
    CredentialExpired,
    #[error("relay credential was revoked")]
    CredentialRevoked,
    #[error("community sharing policy denied this operation")]
    PolicyDenied,
    #[error("seeding is paused on the current metered network")]
    MeteredNetworkPaused,
    #[error("community relay quota was reached")]
    QuotaReached,
    #[error("community relay cost threshold was reached")]
    CostThresholdReached,
    #[error("no eligible community copy is available")]
    NotAvailable,
    #[error("relay envelope was rejected")]
    EnvelopeRejected,
    #[error("relay chunk authentication failed")]
    AuthenticationFailed,
    #[error("relay chunk was replayed")]
    Replay,
    #[error("relay chunks arrived out of order")]
    OutOfOrder,
    #[error("relay object did not match its signed identity")]
    ObjectMismatch,
    #[error("relay key agreement was rejected")]
    KeyAgreementRejected,
    #[error("community relay persistence state was rejected")]
    PersistenceRejected,
    #[error("relay DNS result was rejected")]
    DnsRejected,
    #[error("no supported relay transport is available")]
    TransportUnavailable,
}

impl RelayError {
    /// Stable public code suitable for app-visible state and coarse logs.
    pub const fn public_code(self) -> &'static str {
        match self {
            Self::Disabled => "relay_disabled",
            Self::SecurityGate => "relay_security_gate",
            Self::ProviderRejected => "relay_provider_rejected",
            Self::ProviderUnavailable => "relay_provider_unavailable",
            Self::UntrustedObject => "relay_untrusted_object",
            Self::UnsafeIdentifier => "relay_unsafe_identifier",
            Self::CredentialInvalid => "relay_credential_invalid",
            Self::CredentialExpired => "relay_credential_expired",
            Self::CredentialRevoked => "relay_credential_revoked",
            Self::PolicyDenied => "relay_policy_denied",
            Self::MeteredNetworkPaused => "relay_metered_pause",
            Self::QuotaReached => "relay_quota_reached",
            Self::CostThresholdReached => "relay_cost_threshold",
            Self::NotAvailable => "relay_not_available",
            Self::EnvelopeRejected => "relay_envelope_rejected",
            Self::AuthenticationFailed => "relay_authentication_failed",
            Self::Replay => "relay_replay",
            Self::OutOfOrder => "relay_out_of_order",
            Self::ObjectMismatch => "relay_object_mismatch",
            Self::KeyAgreementRejected => "relay_key_agreement_rejected",
            Self::PersistenceRejected => "relay_persistence_rejected",
            Self::DnsRejected => "relay_dns_rejected",
            Self::TransportUnavailable => "relay_transport_unavailable",
        }
    }
}

pub const PUBLIC_FAILURE_SCHEMA: &str = "rw.community.relay-failure.v1";

/// App-visible relay failure. The ordered HTTPS fallback is a decision, not a
/// raw endpoint, so another client or provider address cannot leak here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicRelayFailure {
    pub schema: String,
    pub code: String,
    pub fallback: FallbackTarget,
}

impl PublicRelayFailure {
    pub fn new(error: RelayError, fallback: FallbackTarget) -> Self {
        Self {
            schema: PUBLIC_FAILURE_SCHEMA.into(),
            code: error.public_code().into(),
            fallback,
        }
    }
}

/// The only post-relay choices in the historical path. Actual origin URLs are
/// resolved by the normal authenticated HTTPS client, outside this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackTarget {
    ArchivalHttpsOrigin,
    Unavailable,
}

/// The current-data path is deliberately separate and has no relay variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationalFallback {
    HetznerHttpsOrigin,
}

pub const fn after_operational_r2_miss() -> OperationalFallback {
    OperationalFallback::HetznerHttpsOrigin
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}

pub(crate) fn valid_opaque_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.parse::<std::net::IpAddr>().is_err()
        && !value.contains(['.', ':', '/', '\\'])
        && value
            .as_bytes()
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

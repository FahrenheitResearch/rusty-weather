use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::origin_catalog::{OriginCatalogConfig, PublicationSourceMode};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS: u64 = 5 * 366 * 24 * 60 * 60;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to inspect configuration file: {0}")]
    Inspect(#[source] std::io::Error),
    #[error("configuration file exceeds the 1 MiB safety limit")]
    TooLarge,
    #[error("failed to read configuration file: {0}")]
    Read(#[source] std::io::Error),
    #[error("invalid configuration: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("invalid environment value for {name}: {value}")]
    Environment { name: &'static str, value: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub limits: LimitsConfig,
    pub catalog: CatalogConfig,
    /// Optional scheduler-controlled publication boundary for the conventional
    /// HTTPS origin. When enabled, only active and previous generations from
    /// the validated origin catalog are visible.
    pub origin_catalog: OriginCatalogConfig,
    /// Advanced, owner-published replication of complete immutable rw-store
    /// generations. This is separate from operational HTTPS delivery and from
    /// relay-mediated Community Cache objects.
    pub generation_replication: GenerationReplicationConfig,
    /// Optional, privacy-preserving Community Cache. This is disabled unless
    /// an operator explicitly enables it and supplies signing material.
    pub community: CommunityConfig,
    /// Operator-approved public university/lab/service origins. This is a
    /// conventional HTTPS federation, not ordinary-client peer discovery.
    pub federation: FederationConfig,
    pub logging: LoggingConfig,
}

impl AppConfig {
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let mut config = match path {
            Some(path) => {
                let metadata = fs::metadata(path).map_err(ConfigError::Inspect)?;
                if metadata.len() > MAX_CONFIG_BYTES {
                    return Err(ConfigError::TooLarge);
                }
                let text = fs::read_to_string(path).map_err(ConfigError::Read)?;
                toml::from_str(&text).map_err(ConfigError::Parse)?
            }
            None => Self::default(),
        };
        config.apply_environment()?;
        Ok(config)
    }

    fn apply_environment(&mut self) -> Result<(), ConfigError> {
        if let Some(value) = env_nonempty("RW_LISTEN") {
            self.server.listen = parse_env("RW_LISTEN", &value)?;
        }
        if let Some(value) = env_nonempty("RW_STORE_ROOT") {
            self.server.store_root = PathBuf::from(value);
        }
        if let Some(value) = env_nonempty("RW_ARTIFACT_ROOT") {
            self.server.artifact_root = PathBuf::from(value);
        }
        if let Some(value) = env_nonempty("RW_API_TOKEN_FILE") {
            self.auth.token_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_ALLOW_UNAUTHENTICATED_PUBLIC_BIND") {
            self.server.allow_unauthenticated_public_bind =
                parse_env("RW_ALLOW_UNAUTHENTICATED_PUBLIC_BIND", &value)?;
        }
        if let Some(value) = env_nonempty("RW_LOG") {
            self.logging.filter = value;
        }
        if let Some(value) = env_nonempty("RW_ORIGIN_CATALOG_ENABLED") {
            self.origin_catalog.enabled = parse_env("RW_ORIGIN_CATALOG_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_ORIGIN_CATALOG_PUBLICATION_SOURCES") {
            self.origin_catalog.publication_sources = match value.as_str() {
                "scheduler" => PublicationSourceMode::Scheduler,
                "replication" => PublicationSourceMode::Replication,
                "union" => PublicationSourceMode::Union,
                _ => {
                    return Err(ConfigError::Environment {
                        name: "RW_ORIGIN_CATALOG_PUBLICATION_SOURCES",
                        value,
                    });
                }
            };
        }
        if let Some(value) = env_nonempty("RW_ORIGIN_CATALOG_REFRESH_SECONDS") {
            self.origin_catalog.refresh_seconds =
                parse_env("RW_ORIGIN_CATALOG_REFRESH_SECONDS", &value)?;
        }
        if let Some(value) = env_nonempty("RW_ORIGIN_CATALOG_MAX_AGE_SECONDS") {
            self.origin_catalog.max_age_seconds =
                parse_env("RW_ORIGIN_CATALOG_MAX_AGE_SECONDS", &value)?;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_ENABLED") {
            self.generation_replication.enabled =
                parse_env("RW_GENERATION_REPLICATION_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_SECURITY_TESTS_PASSED") {
            self.generation_replication.security_tests_passed =
                parse_env("RW_GENERATION_REPLICATION_SECURITY_TESTS_PASSED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_CAPACITY_AUDIT_COMPLETED") {
            self.generation_replication.capacity_audit_completed =
                parse_env("RW_GENERATION_REPLICATION_CAPACITY_AUDIT_COMPLETED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_KILL_SWITCH") {
            self.generation_replication.kill_switch =
                parse_env("RW_GENERATION_REPLICATION_KILL_SWITCH", &value)?;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_ROOT") {
            self.generation_replication.control_root = PathBuf::from(value);
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_SIGNING_KEY_FILE") {
            self.generation_replication.signing_key_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_SIGNING_KEY_ID") {
            self.generation_replication.signing_key_id = value;
        }
        if let Some(value) = env_nonempty("RW_GENERATION_REPLICATION_OPERATOR_PRINCIPALS") {
            self.generation_replication.operator_principals = comma_separated_values(&value);
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_ENABLED") {
            self.community.enabled = parse_env("RW_COMMUNITY_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_CAPACITY_AUDIT_COMPLETED") {
            self.community.capacity_audit_completed =
                parse_env("RW_COMMUNITY_CAPACITY_AUDIT_COMPLETED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_KILL_SWITCH") {
            self.community.kill_switch = parse_env("RW_COMMUNITY_KILL_SWITCH", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_ROOT") {
            self.community.root = PathBuf::from(value);
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_SIGNING_KEY_FILE") {
            self.community.signing_key_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_SIGNING_KEY_ID") {
            self.community.signing_key_id = value;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS") {
            self.community.object_manifest_retention_seconds =
                parse_env("RW_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_ORIGIN_BASE_URL") {
            self.community.origin_base_url = Some(value);
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_RELAY_ENABLED") {
            self.community.relay.enabled = parse_env("RW_COMMUNITY_RELAY_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_RELAY_KILL_SWITCH") {
            self.community.relay.kill_switch = parse_env("RW_COMMUNITY_RELAY_KILL_SWITCH", &value)?;
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_RELAY_SIGNING_KEY_FILE") {
            self.community.relay.signing_key_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_COMMUNITY_RELAY_CLOUDFLARE_API_TOKEN_FILE") {
            self.community.relay.cloudflare.api_token_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_ENABLED") {
            self.federation.enabled = parse_env("RW_FEDERATION_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_HEALTH_MONITOR_ENABLED") {
            self.federation.health_monitor_enabled =
                parse_env("RW_FEDERATION_HEALTH_MONITOR_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_SIGNING_KEY_FILE") {
            self.federation.catalog_signing_key_file = Some(PathBuf::from(value));
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_PROXY_ENABLED") {
            self.federation.proxy.enabled = parse_env("RW_FEDERATION_PROXY_ENABLED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_PROXY_SECURITY_TESTS_PASSED") {
            self.federation.proxy.security_tests_passed =
                parse_env("RW_FEDERATION_PROXY_SECURITY_TESTS_PASSED", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_PROXY_KILL_SWITCH") {
            self.federation.proxy.kill_switch =
                parse_env("RW_FEDERATION_PROXY_KILL_SWITCH", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_PROXY_CONTROL_STATE_FILE") {
            self.federation.proxy.control_state_file = PathBuf::from(value);
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_PROXY_OPERATOR_PRINCIPALS") {
            self.federation.proxy.operator_principals = comma_separated_values(&value);
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_ACCEPT_LOCAL_RESOLVE") {
            self.federation.proxy.accept_local_resolve =
                parse_env("RW_FEDERATION_ACCEPT_LOCAL_RESOLVE", &value)?;
        }
        if let Some(value) = env_nonempty("RW_FEDERATION_LOCAL_RESOLVE_TOKEN_FILE") {
            self.federation.proxy.local_resolve_token_file = Some(PathBuf::from(value));
        }
        Ok(())
    }

    pub fn validate(&self, has_tokens: bool) -> Result<(), ConfigError> {
        if is_public_bind(self.server.listen.ip())
            && !has_tokens
            && !self.server.allow_unauthenticated_public_bind
        {
            return Err(ConfigError::Invalid(
                "a non-loopback listen address requires authentication; configure a token or explicitly enable allow_unauthenticated_public_bind"
                    .to_string(),
            ));
        }
        if self.server.store_root.as_os_str().is_empty() {
            return Err(ConfigError::Invalid("store_root must not be empty".into()));
        }
        if self.server.artifact_root.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "artifact_root must not be empty".into(),
            ));
        }
        self.limits.validate()?;
        if self.catalog.response_cache_seconds == 0 {
            return Err(ConfigError::Invalid(
                "catalog.response_cache_seconds must be greater than zero".into(),
            ));
        }
        self.origin_catalog
            .validate()
            .map_err(ConfigError::Invalid)?;
        self.generation_replication
            .validate(has_tokens, self.origin_catalog.enabled)?;
        if self.origin_catalog.enabled
            && self
                .origin_catalog
                .publication_sources
                .requires_replication()
            && !self.generation_replication.enabled
        {
            return Err(ConfigError::Invalid(
                "origin_catalog publication_sources requires generation_replication.enabled".into(),
            ));
        }
        if self.generation_replication.enabled
            && !self
                .origin_catalog
                .publication_sources
                .requires_replication()
        {
            return Err(ConfigError::Invalid(
                "enabled generation_replication requires origin_catalog.publication_sources = replication or union"
                    .into(),
            ));
        }
        self.community.validate()?;
        if self.community.relay.enabled && !has_tokens {
            return Err(ConfigError::Invalid(
                "community.relay requires authenticated API tokens so each participant has an isolated quota identity"
                    .into(),
            ));
        }
        self.federation.validate()?;
        if (self.federation.proxy.enabled || self.federation.proxy.accept_local_resolve)
            && !self.community.enabled
        {
            return Err(ConfigError::Invalid(
                "federation proxy data paths require community.enabled for the canonical signer and immutable object store"
                    .into(),
            ));
        }
        if self.federation.proxy.enabled && !has_tokens {
            return Err(ConfigError::Invalid(
                "federation proxy requires authenticated BowEcho API tokens for per-user quotas"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationReplicationConfig {
    pub enabled: bool,
    /// Deployment has passed the replication security and recovery suite for
    /// this exact build and filesystem topology.
    pub security_tests_passed: bool,
    /// Disk, inode, concurrency, bandwidth, retention, and recovery capacity
    /// values below were selected from a deployment-specific audit.
    pub capacity_audit_completed: bool,
    pub kill_switch: bool,
    /// Durable authenticated control state, immutable chunks, and signed
    /// manifests. It must be a separate persistent root, never `store_root`.
    pub control_root: PathBuf,
    /// Separate Ed25519 key used for replicated-generation manifests/state.
    pub signing_key_file: Option<PathBuf>,
    pub signing_key_id: String,
    /// Auth-domain principal digests allowed to use coarse operator routes.
    /// Owner manifests use a further replication-domain-derived identity.
    pub operator_principals: Vec<String>,
    pub limits: GenerationReplicationLimitsConfig,
    pub quotas: GenerationReplicationQuotasConfig,
}

impl Default for GenerationReplicationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            security_tests_passed: false,
            capacity_audit_completed: false,
            kill_switch: true,
            control_root: PathBuf::from("./generation-replication"),
            signing_key_file: None,
            signing_key_id: "rw-generation-replication-v1".into(),
            operator_principals: Vec::new(),
            limits: GenerationReplicationLimitsConfig::default(),
            quotas: GenerationReplicationQuotasConfig::default(),
        }
    }
}

impl GenerationReplicationConfig {
    fn validate(&self, has_tokens: bool, origin_catalog_enabled: bool) -> Result<(), ConfigError> {
        self.protocol_limits().validate().map_err(|error| {
            ConfigError::Invalid(format!("generation_replication limits: {error}"))
        })?;
        self.replication_policy()
            .validate(&self.protocol_limits())
            .map_err(|error| {
                ConfigError::Invalid(format!("generation_replication quotas: {error}"))
            })?;
        if self.control_root.as_os_str().is_empty()
            || !valid_config_token(&self.signing_key_id, 128)
        {
            return Err(ConfigError::Invalid(
                "generation_replication control_root or signing_key_id is invalid".into(),
            ));
        }
        if self.operator_principals.len() > 64
            || self
                .operator_principals
                .iter()
                .any(|principal| !is_lower_sha256(principal))
        {
            return Err(ConfigError::Invalid(
                "generation_replication.operator_principals must contain lowercase auth-domain SHA-256 identities"
                    .into(),
            ));
        }
        if self.enabled {
            if !has_tokens {
                return Err(ConfigError::Invalid(
                    "generation_replication requires authenticated API tokens".into(),
                ));
            }
            if !origin_catalog_enabled {
                return Err(ConfigError::Invalid(
                    "generation_replication requires origin_catalog.enabled so replicated runs cannot bypass PublishedStoreCatalog"
                        .into(),
                ));
            }
            if !self.security_tests_passed || !self.capacity_audit_completed {
                return Err(ConfigError::Invalid(
                    "generation_replication security_tests_passed and capacity_audit_completed must both be true before enablement"
                        .into(),
                ));
            }
            if self.signing_key_file.is_none() || self.operator_principals.is_empty() {
                return Err(ConfigError::Invalid(
                    "generation_replication signing_key_file and operator_principals are required when enabled"
                        .into(),
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn protocol_limits(&self) -> rw_community_protocol::RunGenerationLimits {
        rw_community_protocol::RunGenerationLimits {
            max_generation_bytes: self.limits.maximum_generation_bytes,
            max_files: self.limits.maximum_files,
            max_chunks: self.limits.maximum_chunks,
            max_chunk_bytes: self.limits.maximum_chunk_bytes,
            max_manifest_bytes: self.limits.maximum_manifest_bytes,
            max_retention_seconds: self.limits.maximum_retention_seconds,
            max_provenance_entries: self.limits.maximum_provenance_entries,
            max_attributions: self.limits.maximum_attributions,
        }
    }

    pub(crate) fn replication_policy(&self) -> rw_generation_replication::ReplicationPolicy {
        rw_generation_replication::ReplicationPolicy {
            max_owner_storage_bytes: self.quotas.per_owner_storage_bytes,
            max_total_storage_bytes: self.quotas.total_storage_bytes,
            max_owner_generations: self.quotas.per_owner_generations,
            max_total_generations: self.quotas.total_generations,
            max_owner_concurrent_uploads: self.quotas.per_owner_concurrent_uploads,
            max_total_concurrent_uploads: self.quotas.total_concurrent_uploads,
            max_owner_monthly_upload_bytes: self.quotas.per_owner_upload_bytes_per_month,
            max_total_monthly_upload_bytes: self.quotas.total_upload_bytes_per_month,
            upload_ttl_seconds: self.quotas.upload_ttl_seconds,
            max_state_bytes: self.quotas.maximum_state_bytes,
            max_gc_entries: self.quotas.maximum_gc_entries,
            max_gc_deletions: self.quotas.maximum_gc_deletions,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationReplicationLimitsConfig {
    pub maximum_generation_bytes: u64,
    pub maximum_files: usize,
    pub maximum_chunks: usize,
    pub maximum_chunk_bytes: u64,
    pub maximum_manifest_bytes: usize,
    pub maximum_retention_seconds: i64,
    pub maximum_provenance_entries: usize,
    pub maximum_attributions: usize,
}

impl Default for GenerationReplicationLimitsConfig {
    fn default() -> Self {
        Self {
            maximum_generation_bytes: 274_877_906_944,
            maximum_files: 65_538,
            maximum_chunks: 262_144,
            maximum_chunk_bytes: 8 * 1024 * 1024,
            maximum_manifest_bytes: 8 * 1024 * 1024,
            maximum_retention_seconds: 90 * 24 * 60 * 60,
            maximum_provenance_entries: 32,
            maximum_attributions: 32,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GenerationReplicationQuotasConfig {
    pub per_owner_storage_bytes: u64,
    pub total_storage_bytes: u64,
    pub per_owner_generations: usize,
    pub total_generations: usize,
    pub per_owner_concurrent_uploads: usize,
    pub total_concurrent_uploads: usize,
    pub per_owner_upload_bytes_per_month: u64,
    pub total_upload_bytes_per_month: u64,
    pub upload_ttl_seconds: i64,
    pub maximum_state_bytes: u64,
    pub maximum_gc_entries: usize,
    pub maximum_gc_deletions: usize,
}

impl Default for GenerationReplicationQuotasConfig {
    fn default() -> Self {
        Self {
            per_owner_storage_bytes: 549_755_813_888,
            total_storage_bytes: 2_199_023_255_552,
            per_owner_generations: 8,
            total_generations: 64,
            per_owner_concurrent_uploads: 2,
            total_concurrent_uploads: 8,
            per_owner_upload_bytes_per_month: 1_099_511_627_776,
            total_upload_bytes_per_month: 4_398_046_511_104,
            upload_ttl_seconds: 24 * 60 * 60,
            maximum_state_bytes: 64 * 1024 * 1024,
            maximum_gc_entries: 250_000,
            maximum_gc_deletions: 25_000,
        }
    }
}

/// Phase-one Community Cache configuration. Capacity values intentionally
/// remain conservative and operator-configurable until the deployment host is
/// audited. Enabling this does not enable any peer transport.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityConfig {
    pub enabled: bool,
    /// Explicit acknowledgement that deployment-specific disk/concurrency
    /// capacity values came from the completed origin-host audit.
    pub capacity_audit_completed: bool,
    /// Immediate server-side stop for assisted cache reads, publication, and
    /// promotion. Signed conventional HTTPS-origin resolution remains live.
    pub kill_switch: bool,
    pub root: PathBuf,
    /// File containing a private manifest-signing key. Never accepted inline.
    pub signing_key_file: Option<PathBuf>,
    /// Canonical identifier for the current object/case signing key. Rotate by
    /// changing this id/key and retaining old public keys below.
    pub signing_key_id: String,
    /// Lifetime of exact immutable query-object manifests. Long retention is
    /// safe because identities bind concrete run/snapshot/grid/recipe values.
    pub object_manifest_retention_seconds: u64,
    /// Public verification keys accepted for origin and hot-store manifests.
    pub trusted_public_keys: Vec<String>,
    /// Normal HTTPS Hetzner/origin fallback. Mutable aliases are not allowed.
    pub origin_base_url: Option<String>,
    pub origin_token_file: Option<PathBuf>,
    pub hot_store: HotStoreConfig,
    pub promotion: PromotionConfig,
    pub quotas: CommunityQuotasConfig,
    pub cases: CaseRoomConfig,
    /// Separately gated Phase 2 control plane for cold historical objects.
    /// Operational local/R2/Hetzner delivery never consults this service.
    pub relay: CommunityRelayConfig,
}

impl Default for CommunityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capacity_audit_completed: false,
            kill_switch: false,
            root: PathBuf::from("./community-cache"),
            signing_key_file: None,
            signing_key_id: "rw-origin-v1".into(),
            object_manifest_retention_seconds: 365 * 24 * 60 * 60,
            trusted_public_keys: Vec::new(),
            origin_base_url: None,
            origin_token_file: None,
            hot_store: HotStoreConfig::default(),
            promotion: PromotionConfig::default(),
            quotas: CommunityQuotasConfig::default(),
            cases: CaseRoomConfig::default(),
            relay: CommunityRelayConfig::default(),
        }
    }
}

impl CommunityConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.root.as_os_str().is_empty() {
            return Err(ConfigError::Invalid(
                "community.root must not be empty".into(),
            ));
        }
        if !valid_config_token(&self.signing_key_id, 128) {
            return Err(ConfigError::Invalid(
                "community.signing_key_id must be a bounded canonical identifier".into(),
            ));
        }
        if self.object_manifest_retention_seconds == 0
            || self.object_manifest_retention_seconds
                > MAX_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS
        {
            return Err(ConfigError::Invalid(format!(
                "community.object_manifest_retention_seconds must be between 1 and {MAX_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS}"
            )));
        }
        self.quotas.validate()?;
        self.promotion.validate()?;
        self.cases.validate()?;
        self.relay.validate(self.enabled)?;
        if self.enabled {
            if !self.capacity_audit_completed {
                return Err(ConfigError::Invalid(
                    "community.capacity_audit_completed must be true before enabling Community Cache"
                        .into(),
                ));
            }
            if self.signing_key_file.is_none() {
                return Err(ConfigError::Invalid(
                    "community.signing_key_file is required when Community Cache is enabled".into(),
                ));
            }
            if let Some(url) = &self.origin_base_url {
                validate_https_url("community.origin_base_url", url)?;
            }
            self.hot_store.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityRelayConfig {
    pub enabled: bool,
    pub security_tests_passed: bool,
    pub capacity_audit_completed: bool,
    /// Operator confirmed the current provider plan and derived byte/cost
    /// stops from its actual billed terms. This is intentionally false after
    /// every fresh install or config regeneration.
    pub provider_pricing_verified: bool,
    pub kill_switch: bool,
    pub state_file: PathBuf,
    /// Separate Ed25519 key used only for short-lived relay credentials and
    /// session transcripts. Secret material is loaded from this file.
    pub signing_key_file: Option<PathBuf>,
    pub signing_key_id: String,
    pub relay_id: String,
    pub credential_lifetime_seconds: u64,
    pub max_chunk_plaintext_bytes: u32,
    /// Honest post-relay fallback advertised by this deployment. False means
    /// an unavailable result when hosted historical retention has expired.
    pub archival_origin_available: bool,
    /// Domain-separated authenticated principal digests allowed to change the
    /// runtime relay kill switch. These are identifiers, never bearer tokens.
    pub operator_principals: Vec<String>,
    pub cloudflare: CloudflareRelayConfig,
    pub quotas: CommunityRelayQuotasConfig,
    pub promotion: CommunityRelayPromotionConfig,
}

impl Default for CommunityRelayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            security_tests_passed: false,
            capacity_audit_completed: false,
            provider_pricing_verified: false,
            kill_switch: true,
            state_file: PathBuf::from("./community-cache/relay-state.json"),
            signing_key_file: None,
            signing_key_id: "rw-relay-v1".into(),
            relay_id: "cloudflare-turn".into(),
            credential_lifetime_seconds: 600,
            max_chunk_plaintext_bytes: rw_community_relay::RELAY_PLAINTEXT_CHUNK_BYTES,
            archival_origin_available: false,
            operator_principals: Vec::new(),
            cloudflare: CloudflareRelayConfig::default(),
            quotas: CommunityRelayQuotasConfig::default(),
            promotion: CommunityRelayPromotionConfig::default(),
        }
    }
}

impl CommunityRelayConfig {
    fn validate(&self, phase_one_enabled: bool) -> Result<(), ConfigError> {
        self.quotas.validate()?;
        self.promotion.validate()?;
        self.cloudflare.validate(self.enabled)?;
        if self.state_file.as_os_str().is_empty()
            || self.signing_key_id.is_empty()
            || self.signing_key_id.len() > 128
            || self.relay_id.is_empty()
            || self.relay_id.len() > 128
            || self.credential_lifetime_seconds == 0
            || self.credential_lifetime_seconds > 15 * 60
            || self.max_chunk_plaintext_bytes != rw_community_relay::RELAY_PLAINTEXT_CHUNK_BYTES
        {
            return Err(ConfigError::Invalid(
                "community.relay identifiers, state path, lifetime, or chunk size are invalid"
                    .into(),
            ));
        }
        if self.enabled {
            if !phase_one_enabled {
                return Err(ConfigError::Invalid(
                    "community.enabled is required before community.relay can be enabled".into(),
                ));
            }
            if !self.security_tests_passed
                || !self.capacity_audit_completed
                || !self.provider_pricing_verified
            {
                return Err(ConfigError::Invalid(
                    "community.relay security_tests_passed, capacity_audit_completed, and provider_pricing_verified must all be true before enablement"
                        .into(),
                ));
            }
            if self.signing_key_file.is_none() || self.operator_principals.is_empty() {
                return Err(ConfigError::Invalid(
                    "community.relay signing_key_file and operator_principals are required when enabled"
                        .into(),
                ));
            }
            if self.cloudflare.audited_relay_cidrs.is_empty() {
                return Err(ConfigError::Invalid(
                    "community.relay.cloudflare.audited_relay_cidrs must contain a current operator-audited provider allocation range before enablement"
                        .into(),
                ));
            }
        }
        if self.operator_principals.len() > 64
            || self.operator_principals.iter().any(|principal| {
                principal.len() != 64
                    || !principal
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        {
            return Err(ConfigError::Invalid(
                "community.relay.operator_principals must contain lowercase SHA-256 identities"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CloudflareRelayConfig {
    /// Cloudflare-generated 32-character TURN key ID; it is an identifier, not
    /// the long-lived API token.
    pub turn_key_id: String,
    pub api_token_file: Option<PathBuf>,
    pub allowed_turn_hosts: Vec<String>,
    /// Exact provider allocation ranges audited by the operator. Empty is the
    /// safe default; no Cloudflare address range is compiled as trust.
    pub audited_relay_cidrs: Vec<String>,
    pub resolve_timeout_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub send_timeout_seconds: u64,
    pub receive_timeout_seconds: u64,
    pub global_timeout_seconds: u64,
}

impl Default for CloudflareRelayConfig {
    fn default() -> Self {
        Self {
            turn_key_id: String::new(),
            api_token_file: None,
            allowed_turn_hosts: vec!["turn.cloudflare.com".into()],
            audited_relay_cidrs: Vec::new(),
            resolve_timeout_seconds: 3,
            connect_timeout_seconds: 5,
            send_timeout_seconds: 5,
            receive_timeout_seconds: 10,
            global_timeout_seconds: 15,
        }
    }
}

impl CloudflareRelayConfig {
    fn validate(&self, enabled: bool) -> Result<(), ConfigError> {
        let timeouts = [
            self.resolve_timeout_seconds,
            self.connect_timeout_seconds,
            self.send_timeout_seconds,
            self.receive_timeout_seconds,
            self.global_timeout_seconds,
        ];
        if timeouts.iter().any(|value| *value == 0 || *value > 60)
            || self.global_timeout_seconds < self.resolve_timeout_seconds
            || self.global_timeout_seconds < self.connect_timeout_seconds
            || self.global_timeout_seconds < self.send_timeout_seconds
            || self.global_timeout_seconds < self.receive_timeout_seconds
            || self.allowed_turn_hosts.is_empty()
            || self.allowed_turn_hosts.len() > 16
        {
            return Err(ConfigError::Invalid(
                "community.relay.cloudflare timeout or host policy is invalid".into(),
            ));
        }
        if enabled
            && (self.turn_key_id.len() != 32
                || !self
                    .turn_key_id
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || self.api_token_file.is_none())
        {
            return Err(ConfigError::Invalid(
                "community.relay.cloudflare requires a canonical turn_key_id and api_token_file"
                    .into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityRelayQuotasConfig {
    pub per_user_upload_bytes_per_month: u64,
    pub per_user_download_bytes_per_month: u64,
    pub per_user_advertised_storage_bytes: u64,
    pub per_user_concurrency: u32,
    pub global_concurrency: u32,
    pub global_relay_bytes_per_month: u64,
    pub cost_stop_after_bytes_per_month: u64,
}

impl Default for CommunityRelayQuotasConfig {
    fn default() -> Self {
        Self {
            per_user_upload_bytes_per_month: 10 * 1024 * 1024 * 1024,
            per_user_download_bytes_per_month: 10 * 1024 * 1024 * 1024,
            per_user_advertised_storage_bytes: 25 * 1024 * 1024 * 1024,
            per_user_concurrency: 2,
            global_concurrency: 32,
            global_relay_bytes_per_month: 1024 * 1024 * 1024 * 1024,
            cost_stop_after_bytes_per_month: 900 * 1024 * 1024 * 1024,
        }
    }
}

impl CommunityRelayQuotasConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.per_user_upload_bytes_per_month == 0
            || self.per_user_download_bytes_per_month == 0
            || self.per_user_advertised_storage_bytes == 0
            || self.per_user_concurrency == 0
            || self.global_concurrency == 0
            || self.global_relay_bytes_per_month == 0
            || self.cost_stop_after_bytes_per_month == 0
            || self.cost_stop_after_bytes_per_month > self.global_relay_bytes_per_month
        {
            return Err(ConfigError::Invalid(
                "community.relay quotas are zero or internally inconsistent".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityRelayPromotionConfig {
    pub successful_recoveries: u64,
    pub relayed_bytes: u64,
}

impl Default for CommunityRelayPromotionConfig {
    fn default() -> Self {
        Self {
            successful_recoveries: 3,
            relayed_bytes: 256 * 1024 * 1024,
        }
    }
}

impl CommunityRelayPromotionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.successful_recoveries == 0 || self.relayed_bytes == 0 {
            return Err(ConfigError::Invalid(
                "community.relay promotion thresholds must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum HotStoreConfig {
    #[default]
    Disabled,
    /// Test/local provider using the same immutable key layout as R2.
    Filesystem { root: PathBuf },
    /// R2-compatible HTTPS object gateway. Credentials are loaded from a file,
    /// never serialized in configuration or logs.
    R2 {
        base_url: String,
        bucket: String,
        token_file: PathBuf,
    },
}

impl HotStoreConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::Disabled => Ok(()),
            Self::Filesystem { root } if root.as_os_str().is_empty() => Err(ConfigError::Invalid(
                "community.hot_store filesystem root must not be empty".into(),
            )),
            Self::Filesystem { .. } => Ok(()),
            Self::R2 {
                base_url,
                bucket,
                token_file,
            } => {
                validate_https_url("community.hot_store.base_url", base_url)?;
                if bucket.is_empty()
                    || bucket.len() > 128
                    || !bucket.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
                {
                    return Err(ConfigError::Invalid(
                        "community.hot_store.bucket is invalid".into(),
                    ));
                }
                if token_file.as_os_str().is_empty() {
                    return Err(ConfigError::Invalid(
                        "community.hot_store.token_file must not be empty".into(),
                    ));
                }
                Ok(())
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PromotionConfig {
    pub enabled: bool,
    pub minimum_hits: u64,
    pub window_seconds: u64,
    pub maximum_object_bytes: u64,
}

impl Default for PromotionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            minimum_hits: 3,
            window_seconds: 300,
            maximum_object_bytes: 64 * 1024 * 1024,
        }
    }
}

impl PromotionConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.minimum_hits == 0 || self.window_seconds == 0 || self.maximum_object_bytes == 0 {
            return Err(ConfigError::Invalid(
                "community promotion limits must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommunityQuotasConfig {
    pub maximum_object_bytes: u64,
    pub maximum_decompressed_bytes: u64,
    pub maximum_manifest_bytes: u64,
    pub storage_bytes: u64,
    pub upload_bytes_per_month: u64,
    pub download_bytes_per_month: u64,
    /// Global cost ceiling for bytes promoted to the hot-object provider.
    pub promoted_bytes_per_month: u64,
    pub concurrent_transfers: usize,
    /// Maximum distinct authenticated principals retained in the durable
    /// monthly accounting file. A new principal fails closed at this bound.
    pub maximum_principals: usize,
    pub maximum_objects: usize,
}

impl Default for CommunityQuotasConfig {
    fn default() -> Self {
        Self {
            maximum_object_bytes: 64 * 1024 * 1024,
            maximum_decompressed_bytes: 256 * 1024 * 1024,
            maximum_manifest_bytes: 256 * 1024,
            storage_bytes: 10 * 1024 * 1024 * 1024,
            upload_bytes_per_month: 100 * 1024 * 1024 * 1024,
            download_bytes_per_month: 100 * 1024 * 1024 * 1024,
            promoted_bytes_per_month: 100 * 1024 * 1024 * 1024,
            concurrent_transfers: 4,
            maximum_principals: 10_000,
            maximum_objects: 100_000,
        }
    }
}

impl CommunityQuotasConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.maximum_object_bytes == 0
            || self.maximum_decompressed_bytes < self.maximum_object_bytes
            || self.maximum_manifest_bytes == 0
            || self.storage_bytes < self.maximum_object_bytes
            || self.upload_bytes_per_month == 0
            || self.download_bytes_per_month == 0
            || self.promoted_bytes_per_month == 0
            || self.concurrent_transfers == 0
            || self.maximum_principals == 0
            || self.maximum_objects == 0
        {
            return Err(ConfigError::Invalid(
                "community quotas are zero or internally inconsistent".into(),
            ));
        }
        if self.maximum_principals > 100_000 {
            return Err(ConfigError::Invalid(
                "community.quotas.maximum_principals must not exceed 100000".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CaseRoomConfig {
    pub enabled: bool,
    /// Separate opt-in gate for authenticated typed artifact publication.
    /// Raw files, directories, wrfout, and complete-run uploads remain outside
    /// this endpoint regardless of this setting.
    pub artifact_publication_enabled: bool,
    pub maximum_manifest_bytes: u64,
    pub maximum_objects_per_case: usize,
    /// Total number of unexpired case manifests retained locally.
    pub maximum_cases: usize,
    /// Total encoded bytes available to unexpired case manifests.
    pub storage_bytes: u64,
    /// Maximum requested retention interval for a published case.
    pub default_retention_seconds: u64,
}

impl Default for CaseRoomConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            artifact_publication_enabled: false,
            maximum_manifest_bytes: 256 * 1024,
            maximum_objects_per_case: 512,
            maximum_cases: 1_000,
            storage_bytes: 256 * 1024 * 1024,
            default_retention_seconds: 30 * 24 * 60 * 60,
        }
    }
}

impl CaseRoomConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        if self.artifact_publication_enabled && !self.enabled {
            return Err(ConfigError::Invalid(
                "community.cases.enabled is required for artifact publication".into(),
            ));
        }
        if self.maximum_manifest_bytes == 0
            || self.maximum_objects_per_case == 0
            || self.maximum_cases == 0
            || self.storage_bytes < self.maximum_manifest_bytes
            || self.default_retention_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "community case-room limits must be greater than zero".into(),
            ));
        }
        if self.maximum_cases > 100_000 {
            return Err(ConfigError::Invalid(
                "community.cases.maximum_cases must not exceed 100000".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FederationConfig {
    pub enabled: bool,
    /// Actively probe the signed, deliberately public origin health endpoints.
    /// This remains separately disabled even when catalog federation is on.
    pub health_monitor_enabled: bool,
    pub catalog_id: String,
    pub catalog_signing_key_id: String,
    /// Private Ed25519 catalog key. It is loaded from a permission-restricted
    /// file and is never accepted inline or emitted by configuration APIs.
    pub catalog_signing_key_file: Option<PathBuf>,
    /// Signed descriptors provisioned by an operator. There is deliberately
    /// no HTTP self-registration endpoint.
    pub descriptor_files: Vec<PathBuf>,
    /// Explicit identity/key allowlist established out of band.
    pub approved_origins: Vec<ApprovedFederationOriginConfig>,
    pub revoked_origin_ids: Vec<String>,
    pub revoked_key_ids: Vec<String>,
    pub catalog_ttl_seconds: u64,
    pub health_failure_threshold: u32,
    pub health_quarantine_seconds: u64,
    /// Durable, address-free monitor state. Required when active monitoring is
    /// enabled so quarantine survives process restarts.
    pub health_state_file: Option<PathBuf>,
    pub health_probe_interval_seconds: u64,
    pub health_probe_concurrency: usize,
    pub health_resolve_timeout_seconds: u64,
    pub health_connect_timeout_seconds: u64,
    pub health_send_timeout_seconds: u64,
    pub health_receive_timeout_seconds: u64,
    pub health_global_timeout_seconds: u64,
    pub maximum_selection_results: usize,
    /// Server-mediated data failover. Public descriptor URLs remain visible,
    /// but only the authority holds origin-scoped data credentials.
    pub proxy: FederationProxyServerConfig,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            health_monitor_enabled: false,
            catalog_id: "fahrenheit-public-origins".into(),
            catalog_signing_key_id: "federation-catalog-v1".into(),
            catalog_signing_key_file: None,
            descriptor_files: Vec::new(),
            approved_origins: Vec::new(),
            revoked_origin_ids: Vec::new(),
            revoked_key_ids: Vec::new(),
            catalog_ttl_seconds: 300,
            health_failure_threshold: 3,
            health_quarantine_seconds: 60,
            health_state_file: None,
            health_probe_interval_seconds: 30,
            health_probe_concurrency: 4,
            health_resolve_timeout_seconds: 2,
            health_connect_timeout_seconds: 3,
            health_send_timeout_seconds: 2,
            health_receive_timeout_seconds: 4,
            health_global_timeout_seconds: 8,
            maximum_selection_results: 8,
            proxy: FederationProxyServerConfig::default(),
        }
    }
}

impl FederationConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        validate_config_id("federation.catalog_id", &self.catalog_id, 96)?;
        validate_config_id(
            "federation.catalog_signing_key_id",
            &self.catalog_signing_key_id,
            128,
        )?;
        if self.catalog_ttl_seconds == 0
            || self.catalog_ttl_seconds > 60 * 60
            || self.health_failure_threshold == 0
            || self.health_failure_threshold > 100
            || self.health_quarantine_seconds == 0
            || self.health_quarantine_seconds > 24 * 60 * 60
            || self.health_probe_interval_seconds == 0
            || self.health_probe_interval_seconds > 60 * 60
            || self.health_probe_concurrency == 0
            || self.health_probe_concurrency > 32
            || self.health_resolve_timeout_seconds == 0
            || self.health_resolve_timeout_seconds > 60
            || self.health_connect_timeout_seconds == 0
            || self.health_connect_timeout_seconds > 60
            || self.health_send_timeout_seconds == 0
            || self.health_send_timeout_seconds > 60
            || self.health_receive_timeout_seconds == 0
            || self.health_receive_timeout_seconds > 60
            || self.health_global_timeout_seconds == 0
            || self.health_global_timeout_seconds > 120
            || self.health_global_timeout_seconds < self.health_resolve_timeout_seconds
            || self.health_global_timeout_seconds < self.health_connect_timeout_seconds
            || self.health_global_timeout_seconds < self.health_send_timeout_seconds
            || self.health_global_timeout_seconds < self.health_receive_timeout_seconds
            || self.maximum_selection_results == 0
            || self.maximum_selection_results > 128
        {
            return Err(ConfigError::Invalid(
                "federation catalog, health, or selection limits are invalid".into(),
            ));
        }
        if self.descriptor_files.len() > 128
            || self.approved_origins.len() > 128
            || self.revoked_origin_ids.len() > 128
            || self.revoked_key_ids.len() > 1_024
        {
            return Err(ConfigError::Invalid(
                "federation list exceeds its configured safety bound".into(),
            ));
        }
        let mut origins = std::collections::BTreeSet::new();
        for origin in &self.approved_origins {
            validate_config_id(
                "federation.approved_origins.origin_id",
                &origin.origin_id,
                96,
            )?;
            if !origins.insert(&origin.origin_id) {
                return Err(ConfigError::Invalid(
                    "federation approved origin ids must be unique".into(),
                ));
            }
            if origin.descriptor_signing_keys.is_empty() || origin.descriptor_signing_keys.len() > 8
            {
                return Err(ConfigError::Invalid(
                    "each federation origin requires between one and eight trusted descriptor keys"
                        .into(),
                ));
            }
            if origin
                .health_bearer_token_file
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(ConfigError::Invalid(
                    "federation health bearer-token paths must not be empty".into(),
                ));
            }
            let mut key_ids = std::collections::BTreeSet::new();
            for key in &origin.descriptor_signing_keys {
                validate_config_id("federation key_id", &key.key_id, 128)?;
                if !key_ids.insert(&key.key_id)
                    || rw_community_protocol::parse_verifying_key_base64(&key.public_key_base64)
                        .is_err()
                {
                    return Err(ConfigError::Invalid(
                        "federation trusted descriptor keys are duplicate or malformed".into(),
                    ));
                }
            }
        }
        for value in &self.revoked_origin_ids {
            validate_config_id("federation.revoked_origin_ids", value, 96)?;
        }
        for value in &self.revoked_key_ids {
            validate_config_id("federation.revoked_key_ids", value, 128)?;
        }
        if self
            .descriptor_files
            .iter()
            .any(|path| path.as_os_str().is_empty())
        {
            return Err(ConfigError::Invalid(
                "federation descriptor paths must not be empty".into(),
            ));
        }
        if self
            .health_state_file
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ConfigError::Invalid(
                "federation.health_state_file must not be empty".into(),
            ));
        }
        if self.health_monitor_enabled && !self.enabled {
            return Err(ConfigError::Invalid(
                "federation health monitoring requires federation.enabled = true".into(),
            ));
        }
        if self.health_monitor_enabled && self.health_state_file.is_none() {
            return Err(ConfigError::Invalid(
                "federation.health_state_file is required when active health monitoring is enabled"
                    .into(),
            ));
        }
        if self.enabled {
            if self.catalog_signing_key_file.is_none() {
                return Err(ConfigError::Invalid(
                    "federation.catalog_signing_key_file is required when federation is enabled"
                        .into(),
                ));
            }
            if self.descriptor_files.is_empty() || self.approved_origins.is_empty() {
                return Err(ConfigError::Invalid(
                    "federation requires operator-provisioned descriptors and an explicit origin allowlist"
                        .into(),
                ));
            }
        }
        self.proxy.validate(self.enabled, &self.approved_origins)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovedFederationOriginConfig {
    pub origin_id: String,
    pub descriptor_signing_keys: Vec<FederationTrustedKeyConfig>,
    /// Optional permission-restricted bearer token file for this public
    /// origin's same-origin health endpoint. Tokens are never accepted inline.
    pub health_bearer_token_file: Option<PathBuf>,
    /// Dedicated server-side credential for this origin's one-hop local-only
    /// resolver. It is never returned by catalog, status, UI, logs, or errors.
    pub data_bearer_token_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FederationProxyServerConfig {
    pub enabled: bool,
    /// Operator attestation that the release-gate suite was run for this build.
    pub security_tests_passed: bool,
    pub kill_switch: bool,
    /// Atomic durable runtime kill-switch state. A persisted engaged switch
    /// survives restart; `kill_switch = true` always overrides it safely.
    pub control_state_file: PathBuf,
    /// Auth-domain principal digests authorized for the runtime status and
    /// kill-switch endpoints. These are identifiers, never bearer tokens.
    pub operator_principals: Vec<String>,
    /// Enable the token-isolated, one-hop local resolver on a public origin.
    pub accept_local_resolve: bool,
    pub local_resolve_token_file: Option<PathBuf>,
    pub authority_origin_id: String,
    pub authority_https_root: String,
    pub maximum_attempts: usize,
    pub accounting_state_file: PathBuf,
    pub monthly_download_bytes_per_principal: u64,
    pub concurrent_requests_per_principal: usize,
    pub maximum_principals: usize,
    pub resolve_timeout_seconds: u64,
    pub connect_timeout_seconds: u64,
    pub send_timeout_seconds: u64,
    pub receive_timeout_seconds: u64,
    pub global_timeout_seconds: u64,
}

impl Default for FederationProxyServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            security_tests_passed: false,
            kill_switch: true,
            control_state_file: PathBuf::from("./community/federation-control.json"),
            operator_principals: Vec::new(),
            accept_local_resolve: false,
            local_resolve_token_file: None,
            authority_origin_id: "hetzner-authority".into(),
            authority_https_root: "https://weather.fahrenheitresearch.com".into(),
            maximum_attempts: 2,
            accounting_state_file: PathBuf::from("./community/federation-accounting.json"),
            monthly_download_bytes_per_principal: 100 * 1024 * 1024 * 1024,
            concurrent_requests_per_principal: 2,
            maximum_principals: 100_000,
            resolve_timeout_seconds: 2,
            connect_timeout_seconds: 4,
            send_timeout_seconds: 5,
            receive_timeout_seconds: 20,
            global_timeout_seconds: 30,
        }
    }
}

impl FederationProxyServerConfig {
    fn validate(
        &self,
        federation_enabled: bool,
        origins: &[ApprovedFederationOriginConfig],
    ) -> Result<(), ConfigError> {
        validate_config_id(
            "federation.proxy.authority_origin_id",
            &self.authority_origin_id,
            96,
        )?;
        validate_https_url(
            "federation.proxy.authority_https_root",
            &self.authority_https_root,
        )?;
        if self.maximum_attempts == 0
            || self.maximum_attempts > 128
            || self.monthly_download_bytes_per_principal == 0
            || self.concurrent_requests_per_principal == 0
            || self.concurrent_requests_per_principal > 64
            || self.maximum_principals == 0
            || self.maximum_principals > 10_000_000
            || self.resolve_timeout_seconds == 0
            || self.resolve_timeout_seconds > 60
            || self.connect_timeout_seconds == 0
            || self.connect_timeout_seconds > 60
            || self.send_timeout_seconds == 0
            || self.send_timeout_seconds > 60
            || self.receive_timeout_seconds == 0
            || self.receive_timeout_seconds > 120
            || self.global_timeout_seconds == 0
            || self.global_timeout_seconds > 180
            || self.global_timeout_seconds < self.resolve_timeout_seconds
            || self.global_timeout_seconds < self.connect_timeout_seconds
            || self.global_timeout_seconds < self.send_timeout_seconds
            || self.global_timeout_seconds < self.receive_timeout_seconds
        {
            return Err(ConfigError::Invalid(
                "federation proxy quota, attempt, or timeout limits are invalid".into(),
            ));
        }
        if self.accounting_state_file.as_os_str().is_empty()
            || self.control_state_file.as_os_str().is_empty()
            || self.accounting_state_file == self.control_state_file
        {
            return Err(ConfigError::Invalid(
                "federation proxy accounting and control state files must be nonempty and distinct"
                    .into(),
            ));
        }
        let operator_principals = self
            .operator_principals
            .iter()
            .collect::<std::collections::BTreeSet<_>>();
        if self.operator_principals.len() > 64
            || operator_principals.len() != self.operator_principals.len()
            || self.operator_principals.iter().any(|principal| {
                principal.len() != 64
                    || !principal
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
            })
        {
            return Err(ConfigError::Invalid(
                "federation.proxy.operator_principals must contain unique lowercase auth-domain SHA-256 identities"
                    .into(),
            ));
        }
        if (self.enabled || self.accept_local_resolve) && !self.security_tests_passed {
            return Err(ConfigError::Invalid(
                "federation proxy data paths require security_tests_passed = true".into(),
            ));
        }
        if self.enabled {
            if !federation_enabled {
                return Err(ConfigError::Invalid(
                    "federation.proxy.enabled requires federation.enabled = true".into(),
                ));
            }
            if origins
                .iter()
                .any(|origin| origin.data_bearer_token_file.is_none())
            {
                return Err(ConfigError::Invalid(
                    "every approved federation origin requires a dedicated data_bearer_token_file when proxy failover is enabled"
                        .into(),
                ));
            }
            if self.operator_principals.is_empty() {
                return Err(ConfigError::Invalid(
                    "federation.proxy.operator_principals is required when proxy failover is enabled"
                        .into(),
                ));
            }
        }
        if self.accept_local_resolve && self.local_resolve_token_file.is_none() {
            return Err(ConfigError::Invalid(
                "federation.proxy.local_resolve_token_file is required for one-hop origin access"
                    .into(),
            ));
        }
        for origin in origins {
            if origin
                .data_bearer_token_file
                .as_ref()
                .is_some_and(|path| path.as_os_str().is_empty())
            {
                return Err(ConfigError::Invalid(
                    "federation data bearer-token paths must not be empty".into(),
                ));
            }
            if origin.data_bearer_token_file.is_some()
                && origin.data_bearer_token_file == origin.health_bearer_token_file
            {
                return Err(ConfigError::Invalid(
                    "federation health and data credentials must use distinct token files".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FederationTrustedKeyConfig {
    pub key_id: String,
    pub public_key_base64: String,
}

fn validate_config_id(name: &'static str, value: &str, maximum: usize) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.len() > maximum
        || value.bytes().any(|byte| {
            !(byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.'))
        })
        || value.starts_with(['-', '_', '.'])
        || value.ends_with(['-', '_', '.'])
    {
        return Err(ConfigError::Invalid(format!(
            "{name} must be a canonical lowercase identifier"
        )));
    }
    Ok(())
}

fn valid_config_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn validate_https_url(name: &'static str, value: &str) -> Result<(), ConfigError> {
    let parsed = value.parse::<http::Uri>().ok();
    let valid = parsed.as_ref().is_some_and(|uri| {
        let Some(authority) = uri.authority() else {
            return false;
        };
        let host = authority.host();
        let path = uri.path();
        uri.scheme_str() == Some("https")
            && !value.contains(['\r', '\n', '\\', '#'])
            && value.len() <= 512
            && value.is_ascii()
            && !value.bytes().any(|byte| byte.is_ascii_whitespace())
            && !authority.as_str().contains('@')
            && authority.port_u16().is_none()
            && !host.is_empty()
            && host.len() <= 253
            && host == host.to_ascii_lowercase()
            && !host.ends_with('.')
            && host.parse::<IpAddr>().is_err()
            && host != "localhost"
            && !host.ends_with(".localhost")
            && !host.ends_with(".local")
            && uri.query().is_none()
            && ((path == "/" && !value.ends_with('/'))
                || (!path.is_empty()
                    && path.len() <= 160
                    && path.starts_with('/')
                    && !path.ends_with('/')
                    && !path.contains(['%', ':'])
                    && !path.contains("//")
                    && !path.split('/').any(|part| part == "." || part == "..")))
    });
    if !valid {
        return Err(ConfigError::Invalid(format!(
            "{name} must be a canonical HTTPS base URL without userinfo, query, fragment, redirect tricks, or ambiguous path segments"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub listen: SocketAddr,
    pub store_root: PathBuf,
    pub artifact_root: PathBuf,
    pub allow_unauthenticated_public_bind: bool,
    pub cors_origins: Vec<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8788),
            store_root: PathBuf::from("./store"),
            artifact_root: PathBuf::from("./artifacts"),
            allow_unauthenticated_public_bind: false,
            cors_origins: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub token_file: Option<PathBuf>,
    pub protect_metrics: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            token_file: None,
            protect_metrics: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    pub request_body_bytes: usize,
    pub variables_per_query: usize,
    pub points_per_query: usize,
    pub sync_result_values: usize,
    pub json_grid_values: usize,
    /// Native cells in one synchronous arbitrary geographic-domain envelope.
    pub geographic_window_cells: usize,
    /// Coordinates, mask entries, and field/level values serialized by one
    /// synchronous arbitrary geographic-domain response.
    pub geographic_window_output_values: usize,
    /// Native-domain cells allowed for an asynchronous temporal reduction.
    pub temporal_reduction_cells: usize,
    /// Fixed and dynamic values allowed in an asynchronous temporal result.
    pub temporal_output_values: usize,
    pub catalog_time_points: usize,
    pub temporal_frames: usize,
    pub light_concurrency: usize,
    pub heavy_concurrency: usize,
    pub queued_jobs: usize,
    pub job_history_records: usize,
    pub sync_timeout_seconds: u64,
    pub job_timeout_seconds: u64,
    pub job_retention_seconds: u64,
    pub job_result_bytes: u64,
    pub reader_cache_bytes: u64,
    pub response_cache_bytes: u64,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            request_body_bytes: 2 * 1024 * 1024,
            variables_per_query: 32,
            points_per_query: 100,
            sync_result_values: 2_000_000,
            json_grid_values: 250_000,
            geographic_window_cells: 250_000,
            geographic_window_output_values: 2_000_000,
            temporal_reduction_cells: 4_000_000,
            temporal_output_values: 32_000_000,
            catalog_time_points: 10_000,
            temporal_frames: 120,
            light_concurrency: 32,
            heavy_concurrency: 2,
            queued_jobs: 64,
            job_history_records: 10_000,
            sync_timeout_seconds: 10,
            job_timeout_seconds: 300,
            job_retention_seconds: 7 * 24 * 60 * 60,
            job_result_bytes: 512 * 1024 * 1024,
            reader_cache_bytes: 256 * 1024 * 1024,
            response_cache_bytes: 128 * 1024 * 1024,
        }
    }
}

impl LimitsConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        let positive = [
            ("request_body_bytes", self.request_body_bytes),
            ("variables_per_query", self.variables_per_query),
            ("points_per_query", self.points_per_query),
            ("sync_result_values", self.sync_result_values),
            ("json_grid_values", self.json_grid_values),
            ("geographic_window_cells", self.geographic_window_cells),
            (
                "geographic_window_output_values",
                self.geographic_window_output_values,
            ),
            ("temporal_reduction_cells", self.temporal_reduction_cells),
            ("temporal_output_values", self.temporal_output_values),
            ("catalog_time_points", self.catalog_time_points),
            ("temporal_frames", self.temporal_frames),
            ("light_concurrency", self.light_concurrency),
            ("heavy_concurrency", self.heavy_concurrency),
            ("queued_jobs", self.queued_jobs),
            ("job_history_records", self.job_history_records),
        ];
        if let Some((name, _)) = positive.into_iter().find(|(_, value)| *value == 0) {
            return Err(ConfigError::Invalid(format!(
                "limits.{name} must be greater than zero"
            )));
        }
        if self.sync_timeout_seconds == 0
            || self.job_timeout_seconds == 0
            || self.job_retention_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "timeout and retention limits must be greater than zero".into(),
            ));
        }
        if self.job_history_records > 100_000 {
            return Err(ConfigError::Invalid(
                "limits.job_history_records may not exceed 100000".into(),
            ));
        }
        if self.job_result_bytes == 0 || self.job_result_bytes > 16 * 1024 * 1024 * 1024 {
            return Err(ConfigError::Invalid(
                "limits.job_result_bytes must be between 1 byte and 16 GiB".into(),
            ));
        }
        if self.request_body_bytes > 64 * 1024 * 1024 {
            return Err(ConfigError::Invalid(
                "limits.request_body_bytes may not exceed 64 MiB".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CatalogConfig {
    pub response_cache_seconds: u64,
}

impl Default for CatalogConfig {
    fn default() -> Self {
        Self {
            response_cache_seconds: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    pub filter: String,
    pub format: LogFormat,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "rw_server=info,tower_http=info".into(),
            format: LogFormat::Json,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
    Json,
    Pretty,
}

fn env_nonempty(name: &'static str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn comma_separated_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect()
}

fn parse_env<T>(name: &'static str, value: &str) -> Result<T, ConfigError>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| ConfigError::Environment {
        name,
        value: value.to_string(),
    })
}

fn is_public_bind(ip: IpAddr) -> bool {
    ip.is_unspecified() || !ip.is_loopback()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    #[test]
    fn loopback_default_is_safe_without_tokens() {
        let config = AppConfig::default();
        assert!(!config.origin_catalog.enabled);
        config.validate(false).unwrap();
    }

    #[test]
    fn enabled_origin_catalog_requires_bounded_nonzero_freshness() {
        let mut config = AppConfig::default();
        config.origin_catalog.enabled = true;
        config.origin_catalog.max_age_seconds = 0;
        assert!(matches!(
            config.validate(false),
            Err(ConfigError::Invalid(detail)) if detail.contains("max_age_seconds")
        ));
        config.origin_catalog.max_age_seconds = 86_401;
        assert!(config.validate(false).is_err());
        config.origin_catalog.max_age_seconds = 7_200;
        config.origin_catalog.refresh_seconds = 0;
        assert!(matches!(
            config.validate(false),
            Err(ConfigError::Invalid(detail)) if detail.contains("refresh_seconds")
        ));
    }

    #[test]
    fn generation_replication_is_default_off_and_requires_every_explicit_gate() {
        let mut config = AppConfig::default();
        assert!(!config.generation_replication.enabled);
        assert!(config.generation_replication.kill_switch);

        config.origin_catalog.enabled = true;
        config.origin_catalog.publication_sources = PublicationSourceMode::Replication;
        config.generation_replication.enabled = true;
        config.generation_replication.signing_key_file = Some(PathBuf::from("replication.key"));
        config.generation_replication.operator_principals = vec!["a".repeat(64)];
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("security_tests_passed")
        ));
        config.generation_replication.security_tests_passed = true;
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("capacity_audit_completed")
        ));
        config.generation_replication.capacity_audit_completed = true;
        config.validate(true).unwrap();

        assert!(matches!(
            config.validate(false),
            Err(ConfigError::Invalid(detail)) if detail.contains("authenticated API tokens")
        ));
        config.origin_catalog.publication_sources = PublicationSourceMode::Scheduler;
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("publication_sources")
        ));
    }

    #[test]
    fn replication_publication_mode_cannot_run_without_the_engine() {
        let mut config = AppConfig::default();
        config.origin_catalog.enabled = true;
        config.origin_catalog.publication_sources = PublicationSourceMode::Replication;
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("requires generation_replication.enabled")
        ));
    }

    #[test]
    fn immutable_object_manifest_retention_is_nonzero_and_hard_bounded() {
        let mut config = AppConfig::default();
        config.community.object_manifest_retention_seconds = 0;
        assert!(matches!(
            config.validate(false),
            Err(ConfigError::Invalid(detail)) if detail.contains("object_manifest_retention_seconds")
        ));
        config.community.object_manifest_retention_seconds =
            MAX_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS + 1;
        assert!(config.validate(false).is_err());
        config.community.object_manifest_retention_seconds =
            MAX_COMMUNITY_OBJECT_MANIFEST_RETENTION_SECONDS;
        config.validate(false).unwrap();
    }

    #[test]
    fn public_bind_requires_authentication_or_explicit_override() {
        let mut config = AppConfig::default();
        config.server.listen = "0.0.0.0:8788".parse().unwrap();
        assert!(config.validate(false).is_err());
        config.validate(true).unwrap();
        config.server.allow_unauthenticated_public_bind = true;
        config.validate(false).unwrap();
    }

    #[test]
    fn unknown_fields_and_zero_limits_are_rejected() {
        assert!(toml::from_str::<AppConfig>("unknown = true").is_err());
        let mut config = AppConfig::default();
        config.limits.heavy_concurrency = 0;
        assert!(config.validate(true).is_err());
    }

    #[test]
    fn community_enablement_requires_completed_capacity_audit() {
        let mut config = AppConfig::default();
        config.community.enabled = true;
        config.community.signing_key_file = Some(PathBuf::from("signing.key"));
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("capacity_audit_completed")
        ));
        config.community.capacity_audit_completed = true;
        config.validate(true).unwrap();
    }

    #[test]
    fn federation_enablement_requires_operator_provisioning() {
        let mut config = AppConfig::default();
        config.federation.enabled = true;
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("catalog_signing_key_file")
        ));
        config.federation.catalog_signing_key_file = Some(PathBuf::from("catalog.key"));
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("operator-provisioned")
        ));
    }

    #[test]
    fn federation_health_monitor_is_separately_gated_and_requires_durable_state() {
        let mut config = AppConfig::default();
        config.federation.health_monitor_enabled = true;
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("requires federation.enabled")
        ));
        config.federation.enabled = true;
        config.federation.catalog_signing_key_file = Some(PathBuf::from("catalog.key"));
        config.federation.descriptor_files = vec![PathBuf::from("lab.json")];
        config.federation.approved_origins = vec![ApprovedFederationOriginConfig {
            origin_id: "lab".into(),
            descriptor_signing_keys: vec![FederationTrustedKeyConfig {
                key_id: "lab-key".into(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(
                    ed25519_dalek::SigningKey::from_bytes(&[4; 32])
                        .verifying_key()
                        .to_bytes(),
                ),
            }],
            health_bearer_token_file: None,
            data_bearer_token_file: None,
        }];
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("health_state_file")
        ));
        config.federation.health_state_file = Some(PathBuf::from("health.json"));
        config.validate(true).unwrap();
    }

    #[test]
    fn federation_proxy_accepts_safe_enabled_kill_on_staging() {
        use base64::Engine as _;

        let mut config = AppConfig::default();
        config.community.enabled = true;
        config.community.capacity_audit_completed = true;
        config.community.signing_key_file = Some(PathBuf::from("community.key"));
        config.federation.enabled = true;
        config.federation.catalog_signing_key_file = Some(PathBuf::from("catalog.key"));
        config.federation.descriptor_files = vec![PathBuf::from("lab.json")];
        config.federation.approved_origins = vec![ApprovedFederationOriginConfig {
            origin_id: "lab".into(),
            descriptor_signing_keys: vec![FederationTrustedKeyConfig {
                key_id: "lab-key".into(),
                public_key_base64: base64::engine::general_purpose::STANDARD.encode(
                    ed25519_dalek::SigningKey::from_bytes(&[4; 32])
                        .verifying_key()
                        .to_bytes(),
                ),
            }],
            health_bearer_token_file: Some(PathBuf::from("lab-health.token")),
            data_bearer_token_file: Some(PathBuf::from("lab-data.token")),
        }];
        config.federation.proxy.enabled = true;
        config.federation.proxy.security_tests_passed = true;
        config.federation.proxy.kill_switch = true;
        config.federation.proxy.operator_principals = vec!["a".repeat(64)];
        config.validate(true).unwrap();
    }

    #[test]
    fn remote_base_url_validation_rejects_ambiguous_authorities_and_paths() {
        for value in [
            "http://example.com",
            "https://user@example.com",
            "https://example.com:8443",
            "https://127.0.0.1",
            "https://LOCALHOST",
            "https://example.com/",
            "https://example.com/api/../secret",
            "https://example.com/api%2fsecret",
            "https://example.com/api?redirect=https://evil.example",
            "https://example.com/api#fragment",
        ] {
            assert!(
                validate_https_url("test", value).is_err(),
                "accepted {value}"
            );
        }
        validate_https_url("test", "https://weather.example.edu").unwrap();
        validate_https_url("test", "https://gateway.example.edu/api").unwrap();
    }

    #[test]
    fn async_temporal_defaults_cover_full_hrrr_vector_results() {
        let limits = LimitsConfig::default();
        let hrrr_cells = 1_799usize * 1_059usize;
        assert!(limits.temporal_reduction_cells >= hrrr_cells);
        assert!(limits.temporal_output_values >= hrrr_cells * 13);
        assert!(limits.json_grid_values < hrrr_cells);
        assert!(limits.sync_result_values < hrrr_cells * 13);
    }
}

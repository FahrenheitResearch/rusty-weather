use std::fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use thiserror::Error;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

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
        if let Some(value) = env_nonempty("RW_COMMUNITY_ORIGIN_BASE_URL") {
            self.community.origin_base_url = Some(value);
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
        self.community.validate()?;
        self.federation.validate()?;
        Ok(())
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
    /// Public verification keys accepted for origin and hot-store manifests.
    pub trusted_public_keys: Vec<String>,
    /// Normal HTTPS Hetzner/origin fallback. Mutable aliases are not allowed.
    pub origin_base_url: Option<String>,
    pub origin_token_file: Option<PathBuf>,
    pub hot_store: HotStoreConfig,
    pub promotion: PromotionConfig,
    pub quotas: CommunityQuotasConfig,
    pub cases: CaseRoomConfig,
}

impl Default for CommunityConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            capacity_audit_completed: false,
            kill_switch: false,
            root: PathBuf::from("./community-cache"),
            signing_key_file: None,
            trusted_public_keys: Vec::new(),
            origin_base_url: None,
            origin_token_file: None,
            hot_store: HotStoreConfig::default(),
            promotion: PromotionConfig::default(),
            quotas: CommunityQuotasConfig::default(),
            cases: CaseRoomConfig::default(),
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
        self.quotas.validate()?;
        self.promotion.validate()?;
        self.cases.validate()?;
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
    /// Reserved contract gate for a later typed `.rws` generation replication
    /// service. No current HTTP route consumes it and it must remain false.
    pub full_run_replication_enabled: bool,
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
            full_run_replication_enabled: false,
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
        if self.full_run_replication_enabled {
            return Err(ConfigError::Invalid(
                "community.cases.full_run_replication_enabled cannot be enabled before the separate replication service is implemented"
                    .into(),
            ));
        }
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

fn validate_https_url(name: &'static str, value: &str) -> Result<(), ConfigError> {
    if !value.starts_with("https://")
        || value.contains(['\r', '\n'])
        || value.trim_end_matches('/').len() <= "https://".len()
    {
        return Err(ConfigError::Invalid(format!(
            "{name} must be an absolute HTTPS URL"
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
        AppConfig::default().validate(false).unwrap();
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
        }];
        assert!(matches!(
            config.validate(true),
            Err(ConfigError::Invalid(detail)) if detail.contains("health_state_file")
        ));
        config.federation.health_state_file = Some(PathBuf::from("health.json"));
        config.validate(true).unwrap();
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

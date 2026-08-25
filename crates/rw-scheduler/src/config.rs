use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use rustwx_core::{ModelId, SourceId};
use rw_ingest::ingest_profile::IngestProfile;
use rw_ingest::{
    IngestCapabilityLimitation, IngestSupportStatus, model_ingest_capabilities,
    model_ingest_capability, model_source_ingest_supported,
};
use serde::{Deserialize, Serialize};

use crate::error::{SchedulerError, SchedulerResult};
use crate::limits::SchedulerLimits;
use crate::origin::OriginCatalogPlanConfig;
use crate::state::RetryPolicy;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    pub store_root: PathBuf,
    pub cache_root: PathBuf,
    pub state_root: PathBuf,
    /// Explicit model slugs, or the sole token `all_ready`.
    pub models: Vec<String>,
    pub source: Option<String>,
    pub model_sources: BTreeMap<String, String>,
    /// `auto`, `full`, `view`, `view_profiles`, `sounding`, `surface`, or
    /// `analysis`.
    pub profile: String,
    pub model_profiles: BTreeMap<String, String>,
    pub use_cache: bool,
    pub verify: bool,
    pub rollback_days: u16,
    pub poll_seconds: u64,
    /// Hard wall-clock budget for one model's provider discovery pass.
    pub discovery_timeout_seconds: u64,
    /// Maximum wall-clock wait for one metadata-only provider probe.
    pub discovery_probe_timeout_seconds: u64,
    pub max_concurrent_jobs: usize,
    pub max_concurrent_hours: usize,
    pub max_queued_jobs: usize,
    pub free_space_reserve_bytes: u64,
    pub retry: RetryConfig,
    pub retention: RetentionConfig,
    /// Capability-driven public-origin lane policy. Once its capacity audit is
    /// complete, the executor discovers, ingests, validates, aliases, and
    /// retention-protects the independent lane generations.
    pub origin_catalog_plan: Option<OriginCatalogPlanConfig>,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            store_root: PathBuf::new(),
            cache_root: PathBuf::new(),
            state_root: PathBuf::new(),
            models: Vec::new(),
            source: None,
            model_sources: BTreeMap::new(),
            profile: "auto".to_string(),
            model_profiles: BTreeMap::new(),
            // The ingest cache has no scheduler-owned size/age eviction yet.
            // Keep daemon operation bounded by default; operators may opt in
            // only when an external cache-retention policy is in place.
            use_cache: false,
            verify: true,
            rollback_days: 2,
            poll_seconds: 300,
            discovery_timeout_seconds: 30,
            discovery_probe_timeout_seconds: 6,
            max_concurrent_jobs: 2,
            max_concurrent_hours: 2,
            max_queued_jobs: 128,
            free_space_reserve_bytes: 10 * 1024 * 1024 * 1024,
            retry: RetryConfig::default(),
            retention: RetentionConfig::default(),
            origin_catalog_plan: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetryConfig {
    pub max_attempts: u32,
    pub initial_backoff_seconds: u64,
    pub max_backoff_seconds: u64,
    pub jitter_percent: u8,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            initial_backoff_seconds: 60,
            max_backoff_seconds: 3_600,
            jitter_percent: 20,
        }
    }
}

impl RetryConfig {
    pub fn policy(self) -> SchedulerResult<RetryPolicy> {
        if self.jitter_percent > 50 {
            return Err(SchedulerError::InvalidConfig(
                "retry jitter_percent must be in 0..=50".to_string(),
            ));
        }
        RetryPolicy::new(
            self.max_attempts,
            self.initial_backoff_seconds,
            self.max_backoff_seconds,
        )
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    pub enabled: bool,
    /// Defaults true. Deletion requires both `enabled = true` and
    /// `dry_run = false`.
    pub dry_run: bool,
    pub keep_latest_per_model: usize,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            dry_run: true,
            keep_latest_per_model: 3,
        }
    }
}

impl SchedulerConfig {
    pub fn load(path: &Path) -> SchedulerResult<Self> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(SchedulerError::InvalidConfig(format!(
                "config '{}' must be a real regular file",
                path.display()
            )));
        }
        if metadata.len() > MAX_CONFIG_BYTES {
            return Err(SchedulerError::InvalidConfig(format!(
                "config '{}' exceeds {MAX_CONFIG_BYTES} bytes",
                path.display()
            )));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        fs::File::open(path)?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            return Err(SchedulerError::InvalidConfig(
                "config grew beyond its size limit while reading".to_string(),
            ));
        }
        let text = std::str::from_utf8(&bytes).map_err(|error| {
            SchedulerError::InvalidConfig(format!("config is not UTF-8: {error}"))
        })?;
        let config = if path.extension().is_some_and(|ext| ext == "json") {
            serde_json::from_str(text)?
        } else {
            toml::from_str(text).map_err(|error| {
                SchedulerError::InvalidConfig(format!("invalid TOML config: {error}"))
            })?
        };
        Ok(config)
    }

    pub fn validate(&self) -> SchedulerResult<()> {
        for (label, path) in [
            ("store_root", &self.store_root),
            ("cache_root", &self.cache_root),
            ("state_root", &self.state_root),
        ] {
            validate_root_path(label, path)?;
        }
        let roots = [&self.store_root, &self.cache_root, &self.state_root];
        for (index, left) in roots.iter().enumerate() {
            for right in roots.iter().skip(index + 1) {
                if left.starts_with(right) || right.starts_with(left) {
                    return Err(SchedulerError::InvalidConfig(format!(
                        "scheduler roots must be distinct and non-nested: '{}' and '{}'",
                        left.display(),
                        right.display()
                    )));
                }
            }
        }
        let _ = self.expanded_models()?;
        let _ = self.retry.policy()?;
        SchedulerLimits::new(self.max_concurrent_jobs, self.max_queued_jobs)?;
        if self.max_concurrent_hours == 0 {
            return Err(SchedulerError::InvalidConfig(
                "max_concurrent_hours must be greater than zero".to_string(),
            ));
        }
        if self.poll_seconds == 0 {
            return Err(SchedulerError::InvalidConfig(
                "poll_seconds must be greater than zero".to_string(),
            ));
        }
        if self.discovery_timeout_seconds == 0 || self.discovery_timeout_seconds > 3_600 {
            return Err(SchedulerError::InvalidConfig(
                "discovery_timeout_seconds must be in 1..=3600".to_string(),
            ));
        }
        if self.discovery_probe_timeout_seconds == 0
            || self.discovery_probe_timeout_seconds > self.discovery_timeout_seconds
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "discovery_probe_timeout_seconds must be in 1..={} (the total discovery budget)",
                self.discovery_timeout_seconds
            )));
        }
        if self.rollback_days > 14 {
            return Err(SchedulerError::InvalidConfig(
                "rollback_days must be in 0..=14".to_string(),
            ));
        }
        if self.retention.keep_latest_per_model == 0 {
            return Err(SchedulerError::InvalidConfig(
                "retention.keep_latest_per_model must be greater than zero".to_string(),
            ));
        }
        let expanded = self.expanded_models()?;
        let capacity = self
            .max_concurrent_jobs
            .checked_add(self.max_queued_jobs)
            .ok_or_else(|| {
                SchedulerError::InvalidConfig(
                    "max_concurrent_jobs + max_queued_jobs overflows usize".to_string(),
                )
            })?;
        if expanded.len() > capacity {
            return Err(SchedulerError::InvalidConfig(format!(
                "{} configured models exceed max_concurrent_jobs + max_queued_jobs ({})",
                expanded.len(),
                capacity
            )));
        }
        let allowed = expanded.iter().copied().collect::<BTreeSet<_>>();
        if let Some(origin) = &self.origin_catalog_plan {
            origin.validate_for_models(&allowed)?;
            let required_state_slots = origin
                .lanes()
                .len()
                .checked_mul(usize::from(origin.previous_generations) + 1)
                .ok_or_else(|| {
                    SchedulerError::InvalidConfig(
                        "origin catalog durable-state capacity overflows usize".to_string(),
                    )
                })?;
            if capacity < required_state_slots {
                return Err(SchedulerError::InvalidConfig(format!(
                    "origin catalog requires max_concurrent_jobs + max_queued_jobs >= {required_state_slots} for active and rollback generations"
                )));
            }
            for lane in origin.lanes() {
                lane.validate_profile(&self.profile_for(lane.model)?)?;
            }
        }
        for key in self.model_profiles.keys().chain(self.model_sources.keys()) {
            let model = key.parse::<ModelId>()?;
            if key != model.as_str() || !allowed.contains(&model) {
                return Err(SchedulerError::InvalidConfig(format!(
                    "per-model key '{key}' must be a canonical slug in the configured allowlist"
                )));
            }
        }
        for model in expanded {
            let _ = self.profile_for(model)?;
            let _ = self.source_for(model)?;
        }
        Ok(())
    }

    pub fn prepare_roots(&self) -> SchedulerResult<()> {
        self.validate()?;
        for path in [&self.store_root, &self.cache_root, &self.state_root] {
            fs::create_dir_all(path)?;
            let metadata = fs::symlink_metadata(path)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(SchedulerError::InvalidConfig(format!(
                    "root '{}' must be a real directory",
                    path.display()
                )));
            }
        }
        let canonical = [
            fs::canonicalize(&self.store_root)?,
            fs::canonicalize(&self.cache_root)?,
            fs::canonicalize(&self.state_root)?,
        ];
        for (index, left) in canonical.iter().enumerate() {
            for right in canonical.iter().skip(index + 1) {
                if left.starts_with(right) || right.starts_with(left) {
                    return Err(SchedulerError::InvalidConfig(format!(
                        "scheduler roots resolve to equal or nested directories: '{}' and '{}'",
                        left.display(),
                        right.display()
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn expanded_models(&self) -> SchedulerResult<Vec<ModelId>> {
        if self.models.len() == 1 && self.models[0].eq_ignore_ascii_case("all_ready") {
            return Ok(model_ingest_capabilities()
                .into_iter()
                .filter(|capability| capability.status == IngestSupportStatus::Ready)
                .map(|capability| capability.model)
                .collect());
        }
        if self.models.is_empty()
            || self
                .models
                .iter()
                .any(|model| model.eq_ignore_ascii_case("all_ready"))
        {
            return Err(SchedulerError::InvalidConfig(
                "models must be an explicit non-empty allowlist, or exactly ['all_ready']"
                    .to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        let mut models = Vec::new();
        for slug in &self.models {
            let model = slug.parse::<ModelId>()?;
            if model_ingest_capability(model).status != IngestSupportStatus::Ready {
                return Err(SchedulerError::UnsupportedModel(model));
            }
            if !seen.insert(model) {
                return Err(SchedulerError::InvalidConfig(format!(
                    "duplicate model '{model}' in allowlist"
                )));
            }
            models.push(model);
        }
        Ok(models)
    }

    pub fn source_for(&self, model: ModelId) -> SchedulerResult<Option<SourceId>> {
        let value = self
            .model_sources
            .get(model.as_str())
            .or(self.source.as_ref());
        let source = value
            .map(|source| source.parse::<SourceId>().map_err(SchedulerError::from))
            .transpose()?;
        if let Some(source) = source
            && !model_source_ingest_supported(model, source)
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "source '{source}' is not supported by remote ingest for model '{model}'"
            )));
        }
        Ok(source)
    }

    pub fn profile_for(&self, model: ModelId) -> SchedulerResult<IngestProfile> {
        let capability = model_ingest_capability(model);
        let requested = self
            .model_profiles
            .get(model.as_str())
            .map(String::as_str)
            .unwrap_or(&self.profile);
        let profile = if requested == "auto" {
            if model == ModelId::GdpsGeml {
                // GEML's complete native contract is the sounding-shaped
                // six-volume/4-surface inventory; it has no inputs for the
                // render-grade full-2D or derived stages.
                IngestProfile::sounding()
            } else if capability
                .limitations
                .contains(&IngestCapabilityLimitation::AnalysisOnly)
            {
                IngestProfile::analysis()
            } else if capability
                .limitations
                .contains(&IngestCapabilityLimitation::ProviderStatisticsOnly)
            {
                IngestProfile::surface_for_model(model)
            } else if capability.limitations.iter().any(|limitation| {
                matches!(
                    limitation,
                    IngestCapabilityLimitation::SurfaceOnly
                        | IngestCapabilityLimitation::TwoDimensionalStatisticsOnly
                )
            }) {
                IngestProfile::surface()
            } else if capability
                .limitations
                .contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
            {
                let mut profile = IngestProfile::full();
                profile.derived = false;
                profile.heavy = false;
                profile
            } else {
                IngestProfile::view()
            }
        } else if requested == "surface" {
            IngestProfile::surface_for_model(model)
        } else {
            IngestProfile::preset(requested).map_err(SchedulerError::InvalidConfig)?
        };
        // JobPlan is the single compatibility validator (pressure availability
        // and typed product limitations); use a real supported cycle only in
        // the executor, so duplicate the profile-local checks here.
        profile.validate().map_err(SchedulerError::InvalidConfig)?;
        let surface_only = capability.limitations.iter().any(|limitation| {
            matches!(
                limitation,
                IngestCapabilityLimitation::AnalysisOnly
                    | IngestCapabilityLimitation::SurfaceOnly
                    | IngestCapabilityLimitation::ProviderStatisticsOnly
                    | IngestCapabilityLimitation::TwoDimensionalStatisticsOnly
            )
        });
        if surface_only && profile.needs_prs() {
            return Err(SchedulerError::InvalidConfig(format!(
                "profile '{requested}' requires pressure data but model '{model}' is surface-only"
            )));
        }
        if capability
            .limitations
            .contains(&IngestCapabilityLimitation::TwoDimensionalStatisticsOnly)
            && profile != IngestProfile::surface()
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "profile '{requested}' is incompatible with model '{model}', which requires the complete surface profile for its typed 2-D statistics collection"
            )));
        }
        if capability
            .limitations
            .contains(&IngestCapabilityLimitation::ProviderStatisticsOnly)
            && profile != IngestProfile::surface_for_model(model)
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "profile '{requested}' is incompatible with model '{model}', which requires its complete typed provider-statistics profile"
            )));
        }
        if capability
            .limitations
            .contains(&IngestCapabilityLimitation::DerivedProductsDisabled)
            && (profile.derived || profile.heavy)
        {
            return Err(SchedulerError::InvalidConfig(format!(
                "profile '{requested}' enables derived diagnostics forbidden for model '{model}'"
            )));
        }
        Ok(profile)
    }
}

fn validate_root_path(label: &str, path: &Path) -> SchedulerResult<()> {
    if !path.is_absolute() || path.as_os_str().is_empty() {
        return Err(SchedulerError::InvalidConfig(format!(
            "{label} must be an absolute path"
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SchedulerError::InvalidConfig(format!(
            "{label} must not contain '.' or '..' components"
        )));
    }
    Ok(())
}

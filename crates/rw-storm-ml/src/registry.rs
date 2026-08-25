use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};

use rw_ops_protocol::{MAX_MODEL_INPUTS, ProtocolError, StormModelManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{DistributionAudience, ModelUsePolicy};

const REGISTRY_STATE_SCHEMA: &str = "rw.storm-model-registry-state.v1";
const MODELS_DIR: &str = "models";
const STATE_FILE: &str = "registry-state.json";
const MANIFEST_FILE: &str = "manifest.json";
const POLICY_FILE: &str = "use-policy.json";
const ARTIFACT_FILE: &str = "artifact.bin";
const INSTALL_PREFIX: &str = ".install-";

pub type RegistryResult<T> = Result<T, RegistryError>;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("registry root must be an absolute path without '.' or '..': {0}")]
    UnsafeRoot(PathBuf),
    #[error("filesystem link or Windows reparse point is forbidden in the model registry: {0}")]
    FilesystemLink(PathBuf),
    #[error("unexpected filesystem entry in the model registry: {0}")]
    UnexpectedEntry(PathBuf),
    #[error("model identifier is not a portable path component: {0}")]
    UnsafeIdentifier(String),
    #[error("model {0} is already installed and immutable")]
    AlreadyInstalled(ModelKey),
    #[error("model {0} is not installed")]
    NotInstalled(ModelKey),
    #[error("model {0} is disabled")]
    Disabled(ModelKey),
    #[error("model '{0}' has no active version")]
    NoActiveVersion(String),
    #[error("model '{0}' has no enabled activation to roll back to")]
    NoRollback(String),
    #[error("registry state refers to unavailable model {0}")]
    InvalidStateReference(ModelKey),
    #[error("registry state schema is unsupported: {0}")]
    InvalidStateSchema(String),
    #[error("artifact for {key} has SHA-256 {actual}, expected {expected}")]
    DigestMismatch {
        key: ModelKey,
        expected: String,
        actual: String,
    },
    #[error("{resource} size {actual} exceeds configured maximum {limit}")]
    SizeLimit {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("model artifact must not be empty")]
    EmptyArtifact,
    #[error("invalid model metadata: {0}")]
    InvalidMetadata(String),
    #[error("invalid model-use policy field '{field}': {reason}")]
    InvalidPolicy {
        field: &'static str,
        reason: &'static str,
    },
    #[error("distribution of {subject} to {audience:?} is not permitted")]
    DistributionDenied {
        subject: &'static str,
        audience: DistributionAudience,
    },
    #[error("model input is incompatible: {0}")]
    IncompatibleInput(String),
    #[error("model output is invalid: {0}")]
    InvalidOutput(String),
    #[error("native Rust backend is not registered for {0}")]
    NativeBackendMissing(ModelKey),
    #[error("a native Rust backend is already registered for {0}")]
    NativeBackendDuplicate(ModelKey),
    #[error("backend {0} cannot be executed by this build")]
    BackendUnavailable(&'static str),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error(transparent)]
    Storm(#[from] rw_storm::StormError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Store(#[from] rw_store::RwStoreError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Explicit resource limits for untrusted or externally supplied model data.
///
/// Limits are configuration, not hidden downsampling. An over-limit request is
/// rejected before allocation and can be raised by a node operator with a
/// deliberate memory budget.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelLimits {
    pub max_installed_versions: usize,
    pub max_activation_history: usize,
    pub max_artifact_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_grid_width: usize,
    pub max_grid_height: usize,
    pub max_grid_points: usize,
    pub max_input_planes: usize,
    /// Sum of cropped label windows accepted by supplied-label conversion.
    pub max_label_work_points: usize,
}

impl Default for ModelLimits {
    fn default() -> Self {
        Self {
            // Cardinality and scientific grid geometry are limited by checked
            // `usize` arithmetic and actual allocation/filesystem capacity,
            // not by arbitrary product ceilings. Library embedders may still
            // choose smaller explicit policies through `ModelLimits`.
            max_installed_versions: usize::MAX,
            max_activation_history: usize::MAX,
            max_artifact_bytes: 4 * 1024 * 1024 * 1024,
            max_manifest_bytes: 1024 * 1024,
            max_grid_width: usize::MAX,
            max_grid_height: usize::MAX,
            max_grid_points: usize::MAX,
            max_input_planes: MAX_MODEL_INPUTS,
            max_label_work_points: usize::MAX,
        }
    }
}

impl ModelLimits {
    pub fn validate(self) -> RegistryResult<()> {
        if self.max_installed_versions == 0
            || self.max_activation_history == 0
            || self.max_artifact_bytes == 0
            || self.max_manifest_bytes == 0
            || self.max_grid_width < 2
            || self.max_grid_height < 2
            || self.max_grid_points < 4
            || self.max_input_planes == 0
            || self.max_input_planes > MAX_MODEL_INPUTS
            || self.max_label_work_points < 4
        {
            return Err(RegistryError::InvalidMetadata(
                "ModelLimits must be positive and input planes must remain within wire-protocol bounds".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelKey {
    pub model_id: String,
    pub model_version: String,
}

impl ModelKey {
    pub fn new(
        model_id: impl Into<String>,
        model_version: impl Into<String>,
    ) -> RegistryResult<Self> {
        let key = Self {
            model_id: model_id.into(),
            model_version: model_version.into(),
        };
        key.validate()?;
        Ok(key)
    }

    pub(crate) fn validate(&self) -> RegistryResult<()> {
        validate_portable_component(&self.model_id)?;
        validate_portable_component(&self.model_version)
    }
}

impl fmt::Display for ModelKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}@{}", self.model_id, self.model_version)
    }
}

#[derive(Clone, Debug)]
pub struct InstalledModel {
    pub key: ModelKey,
    pub manifest: StormModelManifest,
    pub policy: ModelUsePolicy,
    artifact_path: PathBuf,
}

impl InstalledModel {
    pub fn artifact_path(&self) -> &Path {
        &self.artifact_path
    }

    pub fn authorize_artifact(&self, audience: DistributionAudience) -> RegistryResult<()> {
        self.policy.authorize_artifact(audience)
    }

    pub fn authorize_derived_output(&self, audience: DistributionAudience) -> RegistryResult<()> {
        self.policy.authorize_derived_output(audience)
    }

    /// Open the artifact only after checking its type, configured byte limit,
    /// and exact digest. Future inference backends should consume this handle
    /// rather than reopening the path after verification.
    pub fn open_verified_artifact(&self, limits: ModelLimits) -> RegistryResult<File> {
        limits.validate()?;
        reject_link(&self.artifact_path)?;
        let metadata = fs::metadata(&self.artifact_path)?;
        if !metadata.is_file() {
            return Err(RegistryError::UnexpectedEntry(self.artifact_path.clone()));
        }
        if metadata.len() > limits.max_artifact_bytes {
            return Err(RegistryError::SizeLimit {
                resource: "model artifact",
                actual: metadata.len(),
                limit: limits.max_artifact_bytes,
            });
        }
        let mut file = File::open(&self.artifact_path)?;
        let actual = digest_reader(&mut file, limits.max_artifact_bytes)?;
        if actual != self.manifest.artifact_sha256 {
            return Err(RegistryError::DigestMismatch {
                key: self.key.clone(),
                expected: self.manifest.artifact_sha256.clone(),
                actual,
            });
        }
        use std::io::Seek;
        file.rewind()?;
        Ok(file)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RegistryState {
    schema: String,
    generation: u64,
    enabled: BTreeSet<ModelKey>,
    active: BTreeMap<String, String>,
    activation_history: BTreeMap<String, Vec<String>>,
}

impl RegistryState {
    fn empty() -> Self {
        Self {
            schema: REGISTRY_STATE_SCHEMA.into(),
            ..Self::default()
        }
    }
}

pub struct ModelRegistry {
    root: PathBuf,
    limits: ModelLimits,
    installed: BTreeMap<ModelKey, InstalledModel>,
    state: RegistryState,
}

impl ModelRegistry {
    pub fn open(root: impl AsRef<Path>, limits: ModelLimits) -> RegistryResult<Self> {
        limits.validate()?;
        let root = validate_root(root.as_ref())?;
        reject_link_components(&root)?;
        fs::create_dir_all(root.join(MODELS_DIR))?;
        reject_link_components(&root)?;
        reject_link_components(&root.join(MODELS_DIR))?;

        let installed = scan_installed(&root, limits)?;
        let state_path = root.join(STATE_FILE);
        let state = if state_path.exists() {
            reject_link(&state_path)?;
            let bytes = read_bounded(&state_path, limits.max_manifest_bytes, "registry state")?;
            serde_json::from_slice(&bytes)?
        } else {
            RegistryState::empty()
        };
        validate_state(&state, &installed, limits)?;

        let registry = Self {
            root,
            limits,
            installed,
            state,
        };
        if !state_path.exists() {
            registry.persist_state_value(&registry.state)?;
        }
        Ok(registry)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn limits(&self) -> ModelLimits {
        self.limits
    }

    pub fn installed(&self) -> impl ExactSizeIterator<Item = &InstalledModel> {
        self.installed.values()
    }

    pub fn get(&self, key: &ModelKey) -> RegistryResult<&InstalledModel> {
        self.installed
            .get(key)
            .ok_or_else(|| RegistryError::NotInstalled(key.clone()))
    }

    pub fn is_enabled(&self, key: &ModelKey) -> bool {
        self.state.enabled.contains(key)
    }

    pub fn enabled_for_execution(&self, key: &ModelKey) -> RegistryResult<&InstalledModel> {
        let model = self.get(key)?;
        if !self.is_enabled(key) {
            return Err(RegistryError::Disabled(key.clone()));
        }
        model.open_verified_artifact(self.limits)?;
        Ok(model)
    }

    pub fn active(&self, model_id: &str) -> RegistryResult<&InstalledModel> {
        let version = self
            .state
            .active
            .get(model_id)
            .ok_or_else(|| RegistryError::NoActiveVersion(model_id.to_owned()))?;
        let key = ModelKey::new(model_id, version)?;
        let model = self.get(&key)?;
        if !self.is_enabled(&key) {
            return Err(RegistryError::Disabled(key));
        }
        Ok(model)
    }

    pub fn active_for_execution(&self, model_id: &str) -> RegistryResult<&InstalledModel> {
        let model = self.active(model_id)?;
        model.open_verified_artifact(self.limits)?;
        Ok(model)
    }

    /// Install one immutable model version. Installation never enables or
    /// activates a model; those are separate auditable operations.
    pub fn install<R: Read>(
        &mut self,
        manifest: StormModelManifest,
        policy: ModelUsePolicy,
        artifact: R,
    ) -> RegistryResult<&InstalledModel> {
        validate_manifest(&manifest)?;
        policy.validate()?;
        let key = ModelKey::new(&manifest.model_id, &manifest.model_version)?;
        if self.installed.contains_key(&key) {
            return Err(RegistryError::AlreadyInstalled(key));
        }
        if self.installed.len() >= self.limits.max_installed_versions {
            return Err(RegistryError::SizeLimit {
                resource: "installed model versions",
                actual: self.installed.len() as u64 + 1,
                limit: self.limits.max_installed_versions as u64,
            });
        }

        let model_parent = self.root.join(MODELS_DIR).join(&key.model_id);
        reject_link_components(&self.root)?;
        reject_link_components(&model_parent)?;
        fs::create_dir_all(&model_parent)?;
        reject_link_components(&model_parent)?;
        let final_dir = model_parent.join(&key.model_version);
        match fs::symlink_metadata(&final_dir) {
            Ok(_) => return Err(RegistryError::AlreadyInstalled(key)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }

        static INSTALL_SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let stage = model_parent.join(format!(
            "{INSTALL_PREFIX}{}-{}",
            process::id(),
            INSTALL_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&stage)?;
        let install_result = self.write_staged_install(&stage, &key, &manifest, &policy, artifact);
        if let Err(error) = install_result {
            let _ = fs::remove_dir_all(&stage);
            return Err(error);
        }
        if let Err(error) = fs::rename(&stage, &final_dir) {
            let _ = fs::remove_dir_all(&stage);
            if final_dir.exists() {
                return Err(RegistryError::AlreadyInstalled(key));
            }
            return Err(error.into());
        }
        sync_directory(&model_parent)?;

        let installed = InstalledModel {
            key: key.clone(),
            manifest,
            policy,
            artifact_path: final_dir.join(ARTIFACT_FILE),
        };
        self.installed.insert(key.clone(), installed);
        Ok(self.installed.get(&key).expect("inserted model must exist"))
    }

    pub fn enable(&mut self, key: &ModelKey) -> RegistryResult<()> {
        self.get(key)?;
        if !self.state.enabled.contains(key) {
            let mut next = self.state.clone();
            next.enabled.insert(key.clone());
            self.commit_state(next)?;
        }
        Ok(())
    }

    pub fn disable(&mut self, key: &ModelKey) -> RegistryResult<()> {
        self.get(key)?;
        if self.state.enabled.contains(key) {
            let mut next = self.state.clone();
            next.enabled.remove(key);
            if next.active.get(&key.model_id) == Some(&key.model_version) {
                next.active.remove(&key.model_id);
            }
            self.commit_state(next)?;
        }
        Ok(())
    }

    pub fn activate(&mut self, key: &ModelKey) -> RegistryResult<()> {
        self.get(key)?;
        if !self.is_enabled(key) {
            return Err(RegistryError::Disabled(key.clone()));
        }
        if self.state.active.get(&key.model_id) == Some(&key.model_version) {
            return Ok(());
        }
        let mut next = self.state.clone();
        if let Some(previous) = next
            .active
            .insert(key.model_id.clone(), key.model_version.clone())
        {
            let history = next
                .activation_history
                .entry(key.model_id.clone())
                .or_default();
            history.push(previous);
            if history.len() > self.limits.max_activation_history {
                history.remove(0);
            }
        }
        self.commit_state(next)
    }

    /// Atomically select the most recently active version that remains both
    /// installed and enabled. The consumed history is persisted.
    pub fn rollback(&mut self, model_id: &str) -> RegistryResult<&InstalledModel> {
        validate_portable_component(model_id)?;
        let mut next = self.state.clone();
        let history = next
            .activation_history
            .get_mut(model_id)
            .ok_or_else(|| RegistryError::NoRollback(model_id.to_owned()))?;
        let mut selected = None;
        while let Some(version) = history.pop() {
            let key = ModelKey::new(model_id, &version)?;
            if self.installed.contains_key(&key) && next.enabled.contains(&key) {
                selected = Some(key);
                break;
            }
        }
        let key = selected.ok_or_else(|| RegistryError::NoRollback(model_id.to_owned()))?;
        next.active
            .insert(model_id.to_owned(), key.model_version.clone());
        self.commit_state(next)?;
        self.get(&key)
    }

    fn write_staged_install<R: Read>(
        &self,
        stage: &Path,
        key: &ModelKey,
        manifest: &StormModelManifest,
        policy: &ModelUsePolicy,
        mut artifact: R,
    ) -> RegistryResult<()> {
        let artifact_path = stage.join(ARTIFACT_FILE);
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&artifact_path)?;
        let mut writer = BufWriter::with_capacity(1024 * 1024, file);
        let mut hasher = Sha256::new();
        let mut total = 0_u64;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            let count = artifact.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or(RegistryError::SizeLimit {
                    resource: "model artifact",
                    actual: u64::MAX,
                    limit: self.limits.max_artifact_bytes,
                })?;
            if total > self.limits.max_artifact_bytes {
                return Err(RegistryError::SizeLimit {
                    resource: "model artifact",
                    actual: total,
                    limit: self.limits.max_artifact_bytes,
                });
            }
            hasher.update(&buffer[..count]);
            writer.write_all(&buffer[..count])?;
        }
        if total == 0 {
            return Err(RegistryError::EmptyArtifact);
        }
        writer.flush()?;
        writer
            .into_inner()
            .map_err(|error| error.into_error())?
            .sync_all()?;
        let actual = hex_digest(hasher.finalize().as_slice());
        if actual != manifest.artifact_sha256 {
            return Err(RegistryError::DigestMismatch {
                key: key.clone(),
                expected: manifest.artifact_sha256.clone(),
                actual,
            });
        }

        write_new_json(&stage.join(MANIFEST_FILE), manifest)?;
        write_new_json(&stage.join(POLICY_FILE), policy)?;
        sync_directory(stage)
    }

    fn commit_state(&mut self, mut next: RegistryState) -> RegistryResult<()> {
        next.generation =
            self.state.generation.checked_add(1).ok_or_else(|| {
                RegistryError::InvalidMetadata("registry generation overflow".into())
            })?;
        self.persist_state_value(&next)?;
        self.state = next;
        Ok(())
    }

    fn persist_state_value(&self, state: &RegistryState) -> RegistryResult<()> {
        validate_state(state, &self.installed, self.limits)?;
        let bytes = serde_json::to_vec_pretty(state)?;
        if bytes.len() as u64 > self.limits.max_manifest_bytes {
            return Err(RegistryError::SizeLimit {
                resource: "registry state",
                actual: bytes.len() as u64,
                limit: self.limits.max_manifest_bytes,
            });
        }
        let path = self.root.join(STATE_FILE);
        if path.exists() {
            reject_link(&path)?;
        }
        rw_store::atomic::atomic_write_bytes(&path, &bytes)?;
        Ok(())
    }
}

fn validate_manifest(manifest: &StormModelManifest) -> RegistryResult<()> {
    manifest.validate()?;
    let key = ModelKey::new(&manifest.model_id, &manifest.model_version)?;
    if manifest.artifact_sha256 != manifest.artifact_sha256.to_ascii_lowercase() {
        return Err(RegistryError::InvalidMetadata(format!(
            "artifact SHA-256 for {key} must use canonical lowercase hexadecimal"
        )));
    }
    if manifest.probability_threshold <= 0.0 {
        return Err(RegistryError::InvalidMetadata(
            "probability_threshold must be greater than zero".into(),
        ));
    }
    let license = manifest
        .license
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| RegistryError::InvalidMetadata("license must be explicit".into()))?;
    let training = manifest
        .training_provenance
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            RegistryError::InvalidMetadata("training_provenance must be explicit".into())
        })?;
    if license.len() > 512 || training.len() > 8192 {
        return Err(RegistryError::InvalidMetadata(
            "license or training provenance exceeds protocol limits".into(),
        ));
    }
    if manifest.description.len() > 2048 {
        return Err(RegistryError::InvalidMetadata(
            "description exceeds the canonical storm-method limit of 2048 bytes".into(),
        ));
    }
    let mut names = BTreeSet::new();
    if manifest
        .inputs
        .iter()
        .any(|input| !names.insert(input.name.as_str()))
    {
        return Err(RegistryError::InvalidMetadata(
            "model input names must be unique".into(),
        ));
    }
    Ok(())
}

fn validate_state(
    state: &RegistryState,
    installed: &BTreeMap<ModelKey, InstalledModel>,
    limits: ModelLimits,
) -> RegistryResult<()> {
    if state.schema != REGISTRY_STATE_SCHEMA {
        return Err(RegistryError::InvalidStateSchema(state.schema.clone()));
    }
    for key in &state.enabled {
        key.validate()?;
        if !installed.contains_key(key) {
            return Err(RegistryError::InvalidStateReference(key.clone()));
        }
    }
    for (model_id, version) in &state.active {
        let key = ModelKey::new(model_id, version)?;
        if !installed.contains_key(&key) || !state.enabled.contains(&key) {
            return Err(RegistryError::InvalidStateReference(key));
        }
    }
    for (model_id, history) in &state.activation_history {
        validate_portable_component(model_id)?;
        if history.len() > limits.max_activation_history {
            return Err(RegistryError::SizeLimit {
                resource: "activation history",
                actual: history.len() as u64,
                limit: limits.max_activation_history as u64,
            });
        }
        for version in history {
            ModelKey::new(model_id, version)?;
        }
    }
    Ok(())
}

fn scan_installed(
    root: &Path,
    limits: ModelLimits,
) -> RegistryResult<BTreeMap<ModelKey, InstalledModel>> {
    let mut installed = BTreeMap::new();
    let models = root.join(MODELS_DIR);
    for model_entry in fs::read_dir(&models)? {
        let model_entry = model_entry?;
        let model_path = model_entry.path();
        reject_link(&model_path)?;
        if !model_entry.file_type()?.is_dir() {
            return Err(RegistryError::UnexpectedEntry(model_path));
        }
        let model_id = model_entry.file_name().to_string_lossy().into_owned();
        validate_portable_component(&model_id)?;
        for version_entry in fs::read_dir(&model_path)? {
            let version_entry = version_entry?;
            let version_path = version_entry.path();
            reject_link(&version_path)?;
            let version = version_entry.file_name().to_string_lossy().into_owned();
            if version.starts_with(INSTALL_PREFIX) {
                if !version_entry.file_type()?.is_dir() {
                    return Err(RegistryError::UnexpectedEntry(version_path));
                }
                continue;
            }
            if !version_entry.file_type()?.is_dir() {
                return Err(RegistryError::UnexpectedEntry(version_path));
            }
            let key = ModelKey::new(&model_id, &version)?;
            if installed.len() >= limits.max_installed_versions {
                return Err(RegistryError::SizeLimit {
                    resource: "installed model versions",
                    actual: installed.len() as u64 + 1,
                    limit: limits.max_installed_versions as u64,
                });
            }
            let manifest_path = version_path.join(MANIFEST_FILE);
            let policy_path = version_path.join(POLICY_FILE);
            let artifact_path = version_path.join(ARTIFACT_FILE);
            for required in [&manifest_path, &policy_path, &artifact_path] {
                reject_link(required)?;
            }
            reject_extra_version_entries(&version_path)?;
            let manifest: StormModelManifest = serde_json::from_slice(&read_bounded(
                &manifest_path,
                limits.max_manifest_bytes,
                "model manifest",
            )?)?;
            validate_manifest(&manifest)?;
            if manifest.model_id != model_id || manifest.model_version != version {
                return Err(RegistryError::InvalidMetadata(format!(
                    "manifest identity does not match directory {key}"
                )));
            }
            let policy: ModelUsePolicy = serde_json::from_slice(&read_bounded(
                &policy_path,
                limits.max_manifest_bytes,
                "model use policy",
            )?)?;
            policy.validate()?;
            let model = InstalledModel {
                key: key.clone(),
                manifest,
                policy,
                artifact_path,
            };
            model.open_verified_artifact(limits)?;
            installed.insert(key, model);
        }
    }
    Ok(installed)
}

fn reject_extra_version_entries(path: &Path) -> RegistryResult<()> {
    let expected = BTreeSet::from([MANIFEST_FILE, POLICY_FILE, ARTIFACT_FILE]);
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        reject_link(&entry.path())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !expected.contains(name.as_str()) || !entry.file_type()?.is_file() {
            return Err(RegistryError::UnexpectedEntry(entry.path()));
        }
    }
    Ok(())
}

fn validate_portable_component(value: &str) -> RegistryResult<()> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err(RegistryError::UnsafeIdentifier(value.to_owned()))
    } else {
        Ok(())
    }
}

fn validate_root(path: &Path) -> RegistryResult<PathBuf> {
    if !path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(RegistryError::UnsafeRoot(path.to_owned()));
    }
    Ok(path.to_owned())
}

fn reject_link_components(path: &Path) -> RegistryResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata_is_link(&metadata) => {
                return Err(RegistryError::FilesystemLink(current));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn reject_link(path: &Path) -> RegistryResult<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata_is_link(&metadata) {
        Err(RegistryError::FilesystemLink(path.to_owned()))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

#[cfg(windows)]
fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

fn read_bounded(path: &Path, limit: u64, resource: &'static str) -> RegistryResult<Vec<u8>> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(RegistryError::UnexpectedEntry(path.to_owned()));
    }
    if metadata.len() > limit {
        return Err(RegistryError::SizeLimit {
            resource,
            actual: metadata.len(),
            limit,
        });
    }
    let capacity = usize::try_from(metadata.len()).map_err(|_| RegistryError::SizeLimit {
        resource,
        actual: metadata.len(),
        limit,
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    BufReader::new(File::open(path)?).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn digest_reader(reader: &mut impl Read, limit: u64) -> RegistryResult<String> {
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total = total
            .checked_add(count as u64)
            .ok_or(RegistryError::SizeLimit {
                resource: "model artifact",
                actual: u64::MAX,
                limit,
            })?;
        if total > limit {
            return Err(RegistryError::SizeLimit {
                resource: "model artifact",
                actual: total,
                limit,
            });
        }
        digest.update(&buffer[..count]);
    }
    Ok(hex_digest(digest.finalize().as_slice()))
}

fn hex_digest(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn write_new_json(path: &Path, value: &impl Serialize) -> RegistryResult<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    let mut writer = BufWriter::new(file);
    writer.write_all(&bytes)?;
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|error| error.into_error())?
        .sync_all()?;
    Ok(())
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> RegistryResult<()> {
    File::open(path)?.sync_all()?;
    Ok(())
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> RegistryResult<()> {
    // Each staged file is fsynced and the same-volume directory rename is the
    // visibility boundary. Windows does not permit opening directories as
    // ordinary File handles without platform-specific backup semantics.
    Ok(())
}

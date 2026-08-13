//! Fail-closed consumption of the scheduler's bounded public-origin aliases.
//!
//! The scheduler owns mutable lane selection and retention. The HTTPS server
//! consumes only the closed, versioned document at
//! `<store_root>/.rw-origin-catalog.json`, then independently reopens every
//! referenced rw-store generation before exposing it. When this gate is
//! enabled, catalog listing never broad-scans the store and direct queries can
//! resolve only the active and one previous generation named by the validated
//! scheduler document.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, RwLock, TryLockError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rw_community_protocol::PublishedRunGeneration;
use rw_query::{
    ModelCatalogEntry, QueryError, QueryResult, RunCatalogEntry, RunSnapshot, StoreCatalog,
};
use rw_scheduler::{
    ORIGIN_CATALOG_FILE, OriginCatalogPlanConfig, OriginCatalogState, OriginPublishedGeneration,
    cycle_origin_unix,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::generation_replication::{GenerationReplicationError, ServerGenerationReplication};

const MAX_REFRESH_SECONDS: u64 = 300;
const MAX_FRESHNESS_SECONDS: u64 = 86_400;
const MAX_FUTURE_SKEW_SECONDS: i64 = 300;
const MAX_ORIGIN_CATALOG_BYTES: u64 = 1024 * 1024;

/// Conventional HTTPS-origin publication gate. It is deliberately disabled
/// by default. An enabled gate always enforces bounded freshness.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OriginCatalogConfig {
    pub enabled: bool,
    pub publication_sources: PublicationSourceMode,
    pub refresh_seconds: u64,
    pub max_age_seconds: u64,
}

impl Default for OriginCatalogConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            publication_sources: PublicationSourceMode::Scheduler,
            refresh_seconds: 5,
            // The scheduler intentionally preserves the prior timestamp when
            // lane membership is unchanged. This spans a delayed HRRR cycle
            // while still failing readiness if publication stops for hours.
            max_age_seconds: 7_200,
        }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum PublicationSourceMode {
    /// Hetzner-style operational origin: a fresh scheduler active/previous
    /// catalog is mandatory and replication publications are not exposed.
    #[default]
    Scheduler,
    /// Institutional archive: only durable engine-authorized replications are
    /// visible; every unrelated raw rw-store directory remains hidden.
    Replication,
    /// Both authorities are mandatory. Their exact model/run namespaces must
    /// be disjoint or the complete publication view fails closed.
    Union,
}

impl PublicationSourceMode {
    pub const fn requires_scheduler(self) -> bool {
        matches!(self, Self::Scheduler | Self::Union)
    }

    pub const fn requires_replication(self) -> bool {
        matches!(self, Self::Replication | Self::Union)
    }
}

impl OriginCatalogConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.refresh_seconds == 0 || self.refresh_seconds > MAX_REFRESH_SECONDS {
            return Err(format!(
                "origin_catalog.refresh_seconds must be between 1 and {MAX_REFRESH_SECONDS}"
            ));
        }
        if self.max_age_seconds == 0 || self.max_age_seconds > MAX_FRESHNESS_SECONDS {
            return Err(format!(
                "origin_catalog.max_age_seconds must be between 1 and {MAX_FRESHNESS_SECONDS}"
            ));
        }
        Ok(())
    }
}

/// Coarse, address- and identity-free state suitable for readiness/operator
/// status. It intentionally omits paths, model names, run IDs, and errors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, utoipa::ToSchema)]
pub struct OriginCatalogHealthStatus {
    pub schema: String,
    pub enabled: bool,
    pub ready: bool,
    pub state: String,
    pub publication_sources: PublicationSourceMode,
    pub scheduler_ready: bool,
    pub replication_ready: bool,
    pub published_models: usize,
    pub published_runs: usize,
    pub catalog_updated_unix: Option<i64>,
    pub last_reload_unix: Option<i64>,
}

/// Clone-shared store view enforcing the scheduler publication boundary.
#[derive(Clone)]
pub struct PublishedStoreCatalog {
    store: Arc<StoreCatalog>,
    gate: Arc<PublicationGate>,
    replication: Arc<dyn ReplicationPublicationAuthority>,
}

trait ReplicationPublicationAuthority: Send + Sync {
    fn is_enabled(&self) -> bool;
    fn authorized_publications(
        &self,
    ) -> Result<Vec<PublishedRunGeneration>, GenerationReplicationError>;
}

impl ReplicationPublicationAuthority for ServerGenerationReplication {
    fn is_enabled(&self) -> bool {
        self.is_enabled()
    }

    fn authorized_publications(
        &self,
    ) -> Result<Vec<PublishedRunGeneration>, GenerationReplicationError> {
        self.authorized_publications()
    }
}

impl std::fmt::Debug for PublishedStoreCatalog {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PublishedStoreCatalog")
            .field("enabled", &self.gate.config.enabled)
            .finish_non_exhaustive()
    }
}

struct PublicationGate {
    config: OriginCatalogConfig,
    store: Arc<StoreCatalog>,
    reload: Mutex<ReloadState>,
    view: RwLock<PublishedView>,
}

#[derive(Debug, Default)]
struct ReloadState {
    last_attempt: Option<Instant>,
    last_reload_unix: Option<i64>,
    was_ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewState {
    Disabled,
    Pending,
    Ready,
    Unavailable,
}

#[derive(Debug, Clone)]
struct PublishedView {
    state: ViewState,
    updated_unix: Option<i64>,
    runs: BTreeMap<String, BTreeMap<String, RunCatalogEntry>>,
}

impl PublishedView {
    fn disabled() -> Self {
        Self {
            state: ViewState::Disabled,
            updated_unix: None,
            runs: BTreeMap::new(),
        }
    }

    fn pending() -> Self {
        Self {
            state: ViewState::Pending,
            updated_unix: None,
            runs: BTreeMap::new(),
        }
    }

    fn unavailable() -> Self {
        Self {
            state: ViewState::Unavailable,
            updated_unix: None,
            runs: BTreeMap::new(),
        }
    }

    fn is_fresh(&self, now_unix: i64, max_age_seconds: u64) -> bool {
        if self.state != ViewState::Ready {
            return false;
        }
        let Some(updated_unix) = self.updated_unix else {
            return false;
        };
        now_unix.saturating_sub(updated_unix) <= max_age_seconds as i64
    }
}

impl PublishedStoreCatalog {
    pub fn new(store: StoreCatalog, config: OriginCatalogConfig) -> Self {
        Self::from_arc(Arc::new(store), config)
    }

    fn from_arc(store: Arc<StoreCatalog>, config: OriginCatalogConfig) -> Self {
        let enabled = config.enabled;
        let catalog = Self {
            store: store.clone(),
            gate: Arc::new(PublicationGate {
                config,
                store,
                reload: Mutex::new(ReloadState::default()),
                view: RwLock::new(if enabled {
                    PublishedView::pending()
                } else {
                    PublishedView::disabled()
                }),
            }),
            replication: Arc::new(ServerGenerationReplication::default()),
        };
        if enabled && catalog.gate.config.publication_sources.requires_scheduler() {
            catalog.force_reload_at(now_unix());
        }
        catalog
    }

    /// Attach the separately gated full-generation publication authority.
    /// Scheduler lanes and replicated generations share one collision-checked
    /// view, but retain independent retention/lifecycle state.
    pub fn with_generation_replication(mut self, replication: ServerGenerationReplication) -> Self {
        self.replication = Arc::new(replication);
        self
    }

    #[cfg(test)]
    fn with_replication_authority(
        mut self,
        replication: Arc<dyn ReplicationPublicationAuthority>,
    ) -> Self {
        self.replication = replication;
        self
    }

    fn authorize(&self, model: &str, run: &str) -> QueryResult<()> {
        if !self.gate.config.enabled {
            return Ok(());
        }
        let runs = self.authorized_runs()?;
        match runs.get(model) {
            Some(runs) if runs.contains_key(run) => Ok(()),
            Some(_) => Err(QueryError::UnknownRun {
                model: model.to_string(),
                run: run.to_string(),
            }),
            None => Err(QueryError::UnknownModel(model.to_string())),
        }
    }

    pub fn probe_readable(&self) -> QueryResult<()> {
        self.store.probe_readable()?;
        if !self.gate.config.enabled {
            return Ok(());
        }
        self.authorized_runs().map(|_| ())
    }

    pub fn publication_gate_enabled(&self) -> bool {
        self.gate.config.enabled
    }

    pub fn publication_ready(&self) -> bool {
        self.health_status().ready
    }

    pub fn unavailable(&self) -> bool {
        self.gate.config.enabled && !self.publication_ready()
    }

    pub fn list_models(&self) -> QueryResult<Vec<ModelCatalogEntry>> {
        if !self.gate.config.enabled {
            return self.store.list_models();
        }
        let runs = self.authorized_runs()?;
        Ok(runs
            .into_iter()
            .map(|(model, runs)| ModelCatalogEntry {
                model,
                run_count: runs.len(),
            })
            .collect())
    }

    pub fn list_runs(&self, model: &str) -> QueryResult<Vec<RunCatalogEntry>> {
        if !self.gate.config.enabled {
            return self.store.list_runs(model);
        }
        let mut runs = self.authorized_runs()?;
        let Some(runs) = runs.remove(model) else {
            return Err(QueryError::UnknownModel(model.to_string()));
        };
        Ok(runs.into_values().collect())
    }

    /// Resolve the newest visible run for one model by physical cycle origin.
    ///
    /// This deliberately selects from the same publication-gated view as the
    /// model/run catalog. It never scans around an enabled gate, and it reopens
    /// the selected immutable snapshot before returning it. Run identity is a
    /// deterministic tie-break only; a missing physical origin fails closed
    /// instead of treating a lexical run slug as scientific time.
    pub fn latest_run(&self, model: &str) -> QueryResult<RunSnapshot> {
        let runs = self.list_runs(model)?;
        let run = select_latest_run_id(runs.iter())?.ok_or_else(|| {
            // An existing but empty model directory is not a useful public
            // identity. Keep it indistinguishable from an unknown model.
            QueryError::UnknownModel(model.to_string())
        })?;
        self.snapshot(model, &run)
    }

    pub fn snapshot(&self, model: &str, run: &str) -> QueryResult<RunSnapshot> {
        self.authorize(model, run)?;
        self.store.snapshot(model, run)
    }

    pub fn health_status(&self) -> OriginCatalogHealthStatus {
        self.refresh_if_due();
        let now = now_unix();
        // Preserve the reload -> view lock order used by reload_locked. Copy
        // the scalar before taking the view lock so status can never deadlock
        // a concurrent refresh.
        let last_reload_unix = match self.gate.reload.try_lock() {
            Ok(reload) => reload.last_reload_unix,
            Err(TryLockError::Poisoned(error)) => error.into_inner().last_reload_unix,
            Err(TryLockError::WouldBlock) => None,
        };
        let view = self
            .gate
            .view
            .read()
            .unwrap_or_else(|error| error.into_inner());
        let scheduler_ready = view.is_fresh(now, self.gate.config.max_age_seconds);
        let scheduler_runs = if scheduler_ready {
            view.runs.clone()
        } else {
            BTreeMap::new()
        };
        let catalog_updated_unix = scheduler_ready.then_some(view.updated_unix).flatten();
        drop(view);
        let replication_runs = if self.gate.config.publication_sources.requires_replication() {
            self.replicated_runs().ok()
        } else {
            Some(BTreeMap::new())
        };
        let replication_ready = replication_runs.is_some()
            && (!self.gate.config.publication_sources.requires_replication()
                || self.replication.is_enabled());
        let ready = !self.gate.config.enabled
            || (!self.gate.config.publication_sources.requires_scheduler() || scheduler_ready)
                && (!self.gate.config.publication_sources.requires_replication()
                    || replication_ready)
                && {
                    let mut combined = scheduler_runs.clone();
                    replication_runs
                        .clone()
                        .is_some_and(|runs| merge_replicated_runs(&mut combined, runs).is_ok())
                };
        let state = if !self.gate.config.enabled {
            "disabled"
        } else if ready {
            "ready"
        } else if self.gate.config.publication_sources.requires_scheduler()
            && scheduler_runs.is_empty()
            && matches!(
                self.gate
                    .view
                    .read()
                    .unwrap_or_else(|error| error.into_inner())
                    .state,
                ViewState::Pending
            )
        {
            "pending"
        } else {
            "unavailable"
        };
        let mut combined =
            if self.gate.config.publication_sources.requires_scheduler() && scheduler_ready {
                scheduler_runs
            } else {
                BTreeMap::new()
            };
        if let Some(replication_runs) = replication_runs
            && self.gate.config.publication_sources.requires_replication()
        {
            let _ = merge_replicated_runs(&mut combined, replication_runs);
        }
        OriginCatalogHealthStatus {
            schema: "rw-server.origin-catalog-health.v1".to_string(),
            enabled: self.gate.config.enabled,
            ready,
            state: state.to_string(),
            publication_sources: self.gate.config.publication_sources,
            scheduler_ready: !self.gate.config.publication_sources.requires_scheduler()
                || scheduler_ready,
            replication_ready,
            published_models: if ready { combined.len() } else { 0 },
            published_runs: if ready {
                combined.values().map(BTreeMap::len).sum()
            } else {
                0
            },
            catalog_updated_unix,
            last_reload_unix,
        }
    }

    fn authorized_runs(&self) -> QueryResult<BTreeMap<String, BTreeMap<String, RunCatalogEntry>>> {
        self.refresh_if_due();
        let mut runs = BTreeMap::new();
        if self.gate.config.publication_sources.requires_scheduler() {
            let view = self
                .gate
                .view
                .read()
                .unwrap_or_else(|error| error.into_inner());
            if !view.is_fresh(now_unix(), self.gate.config.max_age_seconds) {
                return Err(unavailable_query_error());
            }
            runs = view.runs.clone();
        }
        if self.gate.config.publication_sources.requires_replication() {
            if !self.replication.is_enabled() {
                return Err(replication_unavailable_error());
            }
            merge_replicated_runs(&mut runs, self.replicated_runs()?)?;
        }
        Ok(runs)
    }

    fn replicated_runs(&self) -> QueryResult<BTreeMap<String, BTreeMap<String, RunCatalogEntry>>> {
        if !self.replication.is_enabled() {
            return Ok(BTreeMap::new());
        }
        let publications = match self.replication.authorized_publications() {
            Ok(publications) => publications,
            Err(_) => return Err(replication_unavailable_error()),
        };
        let mut runs: BTreeMap<String, BTreeMap<String, RunCatalogEntry>> = BTreeMap::new();
        for publication in publications {
            let snapshot = self
                .store
                .snapshot(&publication.model, &publication.run)
                .map_err(|_| replication_unavailable_error())?;
            if snapshot.descriptor().snapshot_id != publication.local_snapshot_id
                || snapshot.descriptor().grid_hash != publication.grid_hash
            {
                return Err(replication_unavailable_error());
            }
            let variable_count = snapshot
                .variable_capabilities()
                .map_err(|_| replication_unavailable_error())?
                .len();
            if variable_count == 0 {
                return Err(replication_unavailable_error());
            }
            let previous = runs.entry(publication.model).or_default().insert(
                publication.run,
                RunCatalogEntry {
                    run: snapshot.descriptor().clone(),
                    variable_count,
                },
            );
            if previous.is_some() {
                return Err(replication_collision_error());
            }
        }
        Ok(runs)
    }

    fn refresh_if_due(&self) {
        if !self.gate.config.enabled || !self.gate.config.publication_sources.requires_scheduler() {
            return;
        }
        let mut reload = match self.gate.reload.try_lock() {
            Ok(reload) => reload,
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
            // Another request is already validating a complete replacement.
            // Continue serving the last atomic fresh view instead of queuing
            // unbounded waiters behind filesystem work.
            Err(TryLockError::WouldBlock) => return,
        };
        let due = reload.last_attempt.is_none_or(|last| {
            last.elapsed() >= Duration::from_secs(self.gate.config.refresh_seconds)
        });
        if due {
            self.reload_locked(&mut reload, now_unix());
        }
    }

    /// Force a synchronous reload for deterministic no-network tests; request
    /// handling uses the bounded cadence.
    fn force_reload_at(&self, current_unix: i64) {
        if !self.gate.config.enabled || !self.gate.config.publication_sources.requires_scheduler() {
            return;
        }
        let mut reload = self
            .gate
            .reload
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        self.reload_locked(&mut reload, current_unix);
    }

    fn reload_locked(&self, reload: &mut ReloadState, current_unix: i64) {
        reload.last_attempt = Some(Instant::now());
        reload.last_reload_unix = Some(current_unix);
        let next = match load_catalog_document(self.gate.store.root()) {
            Ok(Some(state)) => match validate_catalog_state(
                &self.gate.store,
                &self.gate.config,
                current_unix,
                state,
            ) {
                Ok(Some(view)) => {
                    reload.was_ready = true;
                    view
                }
                Ok(None) if !reload.was_ready => PublishedView::pending(),
                Ok(None) | Err(_) => PublishedView::unavailable(),
            },
            Ok(None) if !reload.was_ready => PublishedView::pending(),
            Ok(None) | Err(_) => PublishedView::unavailable(),
        };
        *self
            .gate
            .view
            .write()
            .unwrap_or_else(|error| error.into_inner()) = next;
    }
}

fn select_latest_run_id<'a>(
    runs: impl IntoIterator<Item = &'a RunCatalogEntry>,
) -> QueryResult<Option<String>> {
    let mut latest: Option<(i64, &str)> = None;
    for entry in runs {
        let origin_unix = entry.run.origin_unix.ok_or_else(|| {
            QueryError::InvalidRequest(
                "a visible run lacks a physical cycle origin and cannot be ordered".to_string(),
            )
        })?;
        let candidate = (origin_unix, entry.run.run.as_str());
        if latest.is_none_or(|current| candidate > current) {
            latest = Some(candidate);
        }
    }
    Ok(latest.map(|(_, run)| run.to_string()))
}

fn load_catalog_document(root: &Path) -> Result<Option<OriginCatalogState>, String> {
    let root_metadata = fs::symlink_metadata(root)
        .map_err(|error| format!("failed to inspect origin store root: {error}"))?;
    if root_metadata.file_type().is_symlink() || !root_metadata.file_type().is_dir() {
        return Err("origin store root is not a real directory".to_string());
    }
    let path = root.join(ORIGIN_CATALOG_FILE);
    let before = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to inspect origin catalog: {error}")),
    };
    if before.file_type().is_symlink() || !before.file_type().is_file() {
        return Err("origin catalog is not a real regular file".to_string());
    }
    if before.len() > MAX_ORIGIN_CATALOG_BYTES {
        return Err("origin catalog exceeds its size limit".to_string());
    }

    let file =
        File::open(&path).map_err(|error| format!("failed to open origin catalog: {error}"))?;
    if !file
        .metadata()
        .map_err(|error| format!("failed to inspect open origin catalog: {error}"))?
        .is_file()
    {
        return Err("opened origin catalog is not a regular file".to_string());
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    file.take(MAX_ORIGIN_CATALOG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read origin catalog: {error}"))?;
    if bytes.len() as u64 > MAX_ORIGIN_CATALOG_BYTES {
        return Err("origin catalog grew beyond its size limit".to_string());
    }

    let after = fs::symlink_metadata(&path)
        .map_err(|error| format!("origin catalog changed while reading: {error}"))?;
    if after.file_type().is_symlink()
        || !after.file_type().is_file()
        || before.len() != after.len()
        || before.len() != bytes.len() as u64
        || before.modified().ok() != after.modified().ok()
    {
        return Err("origin catalog changed while reading".to_string());
    }

    let state: OriginCatalogState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("origin catalog JSON is invalid: {error}"))?;
    state
        .validate(&OriginCatalogPlanConfig::default())
        .map_err(|error| format!("origin catalog validation failed: {error}"))?;
    Ok(Some(state))
}

fn validate_catalog_state(
    store: &StoreCatalog,
    config: &OriginCatalogConfig,
    current_unix: i64,
    state: OriginCatalogState,
) -> Result<Option<PublishedView>, String> {
    let mut generations: BTreeMap<(String, String), OriginPublishedGeneration> = BTreeMap::new();
    for lane in &state.lanes {
        for generation in [lane.active.as_ref(), lane.previous.as_ref()]
            .into_iter()
            .flatten()
        {
            let key = (generation.model.to_string(), generation.run_id.clone());
            if let Some(existing) = generations.insert(key, generation.clone())
                && existing != *generation
            {
                return Err("one origin generation has conflicting lane metadata".to_string());
            }
        }
    }
    if generations.is_empty() {
        return Ok(None);
    }
    if !catalog_timestamp_is_fresh(state.updated_unix, current_unix, config.max_age_seconds) {
        return Err("origin catalog is stale".to_string());
    }

    let mut runs: BTreeMap<String, BTreeMap<String, RunCatalogEntry>> = BTreeMap::new();
    for ((model, run), generation) in generations {
        let snapshot = validate_generation(store, &generation)?;
        let variable_count = snapshot
            .variable_capabilities()
            .map_err(|error| format!("published generation is not queryable: {error}"))?
            .len();
        if variable_count == 0 {
            return Err("published generation has no queryable variables".to_string());
        }
        runs.entry(model).or_default().insert(
            run,
            RunCatalogEntry {
                run: snapshot.descriptor().clone(),
                variable_count,
            },
        );
    }

    Ok(Some(PublishedView {
        state: ViewState::Ready,
        updated_unix: Some(state.updated_unix),
        runs,
    }))
}

fn catalog_timestamp_is_fresh(updated_unix: i64, current_unix: i64, max_age_seconds: u64) -> bool {
    if updated_unix > current_unix.saturating_add(MAX_FUTURE_SKEW_SECONDS) {
        return false;
    }
    max_age_seconds != 0 && current_unix.saturating_sub(updated_unix) <= max_age_seconds as i64
}

fn validate_generation(
    store: &StoreCatalog,
    generation: &OriginPublishedGeneration,
) -> Result<RunSnapshot, String> {
    let model = generation.model.to_string();
    let snapshot = store
        .snapshot(&model, &generation.run_id)
        .map_err(|error| format!("published generation failed snapshot validation: {error}"))?;
    let descriptor = snapshot.descriptor();
    if descriptor.model != model || descriptor.run != generation.run_id {
        return Err("published generation identity does not match its snapshot".to_string());
    }
    let expected_origin = cycle_origin_unix(&generation.cycle)
        .map_err(|error| format!("published generation cycle is invalid: {error}"))?;
    if descriptor.origin_unix != Some(expected_origin) {
        return Err("published generation origin does not match its cycle".to_string());
    }
    let actual_valid: BTreeSet<_> = snapshot
        .time_axis()
        .iter()
        .map(|time| time.valid_unix)
        .collect();
    if actual_valid != generation.available_valid_unix {
        return Err("published generation valid-time inventory does not match storage".to_string());
    }
    Ok(snapshot)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn unavailable_query_error() -> QueryError {
    QueryError::InvalidRequest("the enabled origin publication catalog is unavailable".to_string())
}

fn replication_unavailable_error() -> QueryError {
    QueryError::InvalidRequest(
        "the replicated-generation publication authority is unavailable".to_string(),
    )
}

fn replication_collision_error() -> QueryError {
    QueryError::InvalidRequest(
        "a replicated generation collides with an authoritative scheduler publication".to_string(),
    )
}

fn merge_replicated_runs(
    target: &mut BTreeMap<String, BTreeMap<String, RunCatalogEntry>>,
    replicated: BTreeMap<String, BTreeMap<String, RunCatalogEntry>>,
) -> QueryResult<()> {
    for (model, runs) in replicated {
        let target_runs = target.entry(model).or_default();
        for (run, entry) in runs {
            if target_runs.insert(run, entry).is_some() {
                return Err(replication_collision_error());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{CycleSpec, GridShape, LatLonGrid, ModelId};
    use rw_query::QueryLimits;
    use rw_scheduler::{
        OriginCatalogState, OriginCatalogStateStore, OriginPublishedGeneration, OriginPublishedLane,
    };
    use rw_store::RwsExactTime;
    use rw_store::ingest::{DerivedFieldInput, write_hour_from_grid_with_derived_exact};
    use tempfile::TempDir;

    const NOW: i64 = 1_786_665_600;

    #[derive(Debug)]
    struct MockReplicationAuthority {
        enabled: bool,
        publications: Vec<PublishedRunGeneration>,
        unavailable: bool,
    }

    impl ReplicationPublicationAuthority for MockReplicationAuthority {
        fn is_enabled(&self) -> bool {
            self.enabled
        }

        fn authorized_publications(
            &self,
        ) -> Result<Vec<PublishedRunGeneration>, GenerationReplicationError> {
            if self.unavailable {
                Err(GenerationReplicationError::Disabled)
            } else {
                Ok(self.publications.clone())
            }
        }
    }

    fn config() -> OriginCatalogConfig {
        OriginCatalogConfig {
            enabled: true,
            refresh_seconds: 1,
            max_age_seconds: 600,
            ..OriginCatalogConfig::default()
        }
    }

    fn config_for(publication_sources: PublicationSourceMode) -> OriginCatalogConfig {
        OriginCatalogConfig {
            publication_sources,
            ..config()
        }
    }

    fn run_id(date: &str, hour: u8) -> String {
        format!("{date}_{hour:02}z")
    }

    fn write_run(root: &Path, date: &str, hour: u8) -> OriginPublishedGeneration {
        let cycle = CycleSpec::new(date, hour).unwrap();
        let origin = cycle_origin_unix(&cycle).unwrap();
        let run = run_id(date, hour);
        let grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![40.0, 40.0, 41.0, 41.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap();
        let mut available_valid_unix = BTreeSet::new();
        for (slot, lead) in [(0_u16, 0_u64), (1, 3_600)] {
            let valid = origin + lead as i64;
            available_valid_unix.insert(valid);
            let values = [slot as f32, 2.0, 3.0, 4.0];
            write_hour_from_grid_with_derived_exact(
                root,
                "hrrr",
                &run,
                slot,
                RwsExactTime::new(lead, valid),
                &grid,
                None,
                &[],
                &[DerivedFieldInput {
                    name: "temperature_2m",
                    units: "K",
                    values: &values,
                }],
                &[],
                "origin-catalog-test",
                1_800_000_000 + u64::from(slot),
            )
            .unwrap();
        }
        OriginPublishedGeneration {
            model: ModelId::Hrrr,
            cycle,
            run_id: run,
            coverage_complete: false,
            available_valid_unix,
        }
    }

    fn catalog_state(
        active: OriginPublishedGeneration,
        previous: Option<OriginPublishedGeneration>,
        updated_unix: i64,
    ) -> OriginCatalogState {
        let mut state = OriginCatalogState::empty(&OriginCatalogPlanConfig::default());
        state.updated_unix = updated_unix;
        state.lanes[0] = OriginPublishedLane {
            id: "hrrr-hourly".to_string(),
            active: Some(active),
            previous,
        };
        state
    }

    fn save(root: &Path, state: &OriginCatalogState) {
        OriginCatalogStateStore::new(root)
            .save(&OriginCatalogPlanConfig::default(), state)
            .unwrap();
    }

    fn replication_publication(
        root: &Path,
        generation: &OriginPublishedGeneration,
    ) -> PublishedRunGeneration {
        let snapshot = StoreCatalog::new(root)
            .snapshot("hrrr", &generation.run_id)
            .unwrap();
        PublishedRunGeneration {
            schema: "rw.published-run-generation.v1".to_string(),
            generation_id: format!("generation-{}", generation.run_id),
            generation_sha256: "11".repeat(32),
            source_snapshot_id: "22".repeat(32),
            local_snapshot_id: snapshot.descriptor().snapshot_id.clone(),
            grid_hash: snapshot.descriptor().grid_hash.clone(),
            model: "hrrr".to_string(),
            run: generation.run_id.clone(),
            published_unix: NOW,
        }
    }

    fn fixture() -> (TempDir, PublishedStoreCatalog, OriginPublishedGeneration) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(&root).unwrap();
        let active = write_run(&root, "20260812", 0);
        save(&root, &catalog_state(active.clone(), None, NOW));
        let store = StoreCatalog::with_limits(&root, QueryLimits::default());
        let catalog = PublishedStoreCatalog::new(store, config());
        catalog.force_reload_at(NOW);
        (directory, catalog, active)
    }

    #[test]
    fn filters_to_active_and_previous_and_denies_hidden_run() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(&root).unwrap();
        let previous = write_run(&root, "20260811", 23);
        // Deliberately newer than the published active run. A broad store scan
        // would select it and leak an unpublished run through the pointer.
        let hidden = write_run(&root, "20260812", 1);
        let active = write_run(&root, "20260812", 0);
        save(
            &root,
            &catalog_state(active.clone(), Some(previous.clone()), NOW),
        );
        let catalog = PublishedStoreCatalog::new(StoreCatalog::new(&root), config());
        catalog.force_reload_at(NOW);

        assert_eq!(catalog.list_models().unwrap()[0].run_count, 2);
        assert_eq!(catalog.list_runs("hrrr").unwrap().len(), 2);
        assert_eq!(
            catalog.latest_run("hrrr").unwrap().descriptor().run,
            active.run_id
        );
        assert!(catalog.snapshot("hrrr", &active.run_id).is_ok());
        assert!(catalog.snapshot("hrrr", &previous.run_id).is_ok());
        assert!(matches!(
            catalog.snapshot("hrrr", &hidden.run_id),
            Err(QueryError::UnknownRun { .. })
        ));
    }

    #[test]
    fn hot_atomic_replacement_swaps_the_complete_view() {
        let (directory, catalog, old) = fixture();
        let root = directory.path().join("store");
        let new = write_run(&root, "20260812", 1);
        save(
            &root,
            &catalog_state(new.clone(), Some(old.clone()), NOW + 60),
        );
        catalog.force_reload_at(NOW + 60);

        let runs = catalog.list_runs("hrrr").unwrap();
        assert_eq!(runs.len(), 2);
        assert!(catalog.snapshot("hrrr", &new.run_id).is_ok());
        assert!(catalog.snapshot("hrrr", &old.run_id).is_ok());
        assert_eq!(catalog.health_status().state, "ready");
    }

    #[test]
    fn stale_tampered_deleted_or_unqueryable_catalog_fails_closed() {
        let (directory, catalog, active) = fixture();
        let root = directory.path().join("store");

        save(&root, &catalog_state(active.clone(), None, NOW - 601));
        catalog.force_reload_at(NOW);
        assert!(matches!(
            catalog.list_models(),
            Err(QueryError::InvalidRequest(_))
        ));
        assert!(!catalog.health_status().ready);

        fs::write(root.join(ORIGIN_CATALOG_FILE), b"{\"schema\":\"tampered\"}").unwrap();
        catalog.force_reload_at(NOW);
        assert!(catalog.list_models().is_err());

        fs::remove_file(root.join(ORIGIN_CATALOG_FILE)).unwrap();
        catalog.force_reload_at(NOW);
        assert!(catalog.list_models().is_err());
        assert_eq!(catalog.health_status().state, "unavailable");

        save(&root, &catalog_state(active.clone(), None, NOW));
        let hour = root.join("hrrr").join(&active.run_id).join("f000.rws");
        fs::remove_file(hour).unwrap();
        catalog.force_reload_at(NOW);
        assert!(catalog.list_models().is_err());
    }

    #[test]
    fn mismatched_valid_inventory_fails_closed() {
        let (directory, catalog, mut active) = fixture();
        let root = directory.path().join("store");
        active.available_valid_unix.pop_first();
        // The scheduler validates that this is a legal partial inventory; the
        // server additionally proves it exactly matches the rw-store axis.
        save(&root, &catalog_state(active, None, NOW));
        catalog.force_reload_at(NOW);
        assert!(catalog.list_models().is_err());
    }

    #[test]
    fn missing_startup_is_pending_and_never_broad_scans() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(root.join("INVALID NAME THAT WOULD BREAK A SCAN")).unwrap();
        let catalog = PublishedStoreCatalog::new(StoreCatalog::new(&root), config());
        catalog.force_reload_at(NOW);

        assert!(catalog.list_models().is_err());
        let status = catalog.health_status();
        assert_eq!(status.state, "pending");
        assert!(!status.ready);
        let encoded = serde_json::to_string(&status).unwrap();
        assert!(!encoded.contains(root.to_string_lossy().as_ref()));
        assert!(!encoded.contains("hrrr"));
    }

    #[test]
    fn empty_scheduler_document_is_pending_until_a_generation_is_published() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(&root).unwrap();
        save(
            &root,
            &OriginCatalogState::empty(&OriginCatalogPlanConfig::default()),
        );
        let catalog = PublishedStoreCatalog::new(StoreCatalog::new(&root), config());
        catalog.force_reload_at(NOW);

        assert_eq!(catalog.health_status().state, "pending");
        assert!(catalog.list_models().is_err());
        assert!(catalog.latest_run("hrrr").is_err());

        let active = write_run(&root, "20260812", 0);
        save(&root, &catalog_state(active, None, NOW));
        catalog.force_reload_at(NOW);
        assert_eq!(catalog.health_status().state, "ready");
    }

    #[test]
    fn latest_run_order_is_physical_and_ties_are_deterministic() {
        fn entry(run: &str, origin_unix: Option<i64>) -> RunCatalogEntry {
            RunCatalogEntry {
                run: rw_query::RunDescriptor {
                    model: "hrrr".to_string(),
                    run: run.to_string(),
                    schema: "rws-run-v1".to_string(),
                    snapshot_id: format!("snapshot-{run}"),
                    grid_hash: "grid".to_string(),
                    nx: 1,
                    ny: 1,
                    exact_time_axis: true,
                    origin_unix,
                    sample_count: 1,
                    first_valid_unix: origin_unix,
                    last_valid_unix: origin_unix,
                    source_provenance: Vec::new(),
                    provider_attributions: Vec::new(),
                },
                variable_count: 1,
            }
        }

        let older_with_later_slug = entry("zz-older", Some(NOW - 3_600));
        let tied_a = entry("cycle-a", Some(NOW));
        let tied_b = entry("cycle-b", Some(NOW));
        let forward = [&older_with_later_slug, &tied_a, &tied_b];
        let reverse = [&tied_b, &tied_a, &older_with_later_slug];
        assert_eq!(
            select_latest_run_id(forward).unwrap().as_deref(),
            Some("cycle-b")
        );
        assert_eq!(
            select_latest_run_id(reverse).unwrap().as_deref(),
            Some("cycle-b")
        );
        assert_eq!(
            select_latest_run_id(std::iter::empty::<&RunCatalogEntry>()).unwrap(),
            None
        );

        let unordered = entry("unknown-cycle", None);
        assert!(matches!(
            select_latest_run_id([&tied_a, &unordered]),
            Err(QueryError::InvalidRequest(_))
        ));
    }

    #[test]
    fn non_regular_and_oversized_catalogs_fail_closed_without_scanning() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(root.join(ORIGIN_CATALOG_FILE)).unwrap();
        let catalog = PublishedStoreCatalog::new(StoreCatalog::new(&root), config());
        catalog.force_reload_at(NOW);
        assert_eq!(catalog.health_status().state, "unavailable");
        assert!(catalog.list_models().is_err());

        fs::remove_dir(root.join(ORIGIN_CATALOG_FILE)).unwrap();
        let oversized = File::create(root.join(ORIGIN_CATALOG_FILE)).unwrap();
        oversized.set_len(MAX_ORIGIN_CATALOG_BYTES + 1).unwrap();
        drop(oversized);
        catalog.force_reload_at(NOW);
        assert_eq!(catalog.health_status().state, "unavailable");
        assert!(catalog.list_models().is_err());
    }

    #[test]
    fn config_limits_reload_work_and_future_catalogs_fail_closed() {
        let mut invalid = config();
        invalid.refresh_seconds = 0;
        assert!(invalid.validate().is_err());
        invalid.refresh_seconds = MAX_REFRESH_SECONDS + 1;
        assert!(invalid.validate().is_err());
        invalid.refresh_seconds = 1;
        invalid.max_age_seconds = 0;
        assert!(invalid.validate().is_err());
        invalid.max_age_seconds = MAX_FRESHNESS_SECONDS + 1;
        assert!(invalid.validate().is_err());

        let (directory, catalog, active) = fixture();
        let root = directory.path().join("store");
        save(
            &root,
            &catalog_state(active, None, NOW + MAX_FUTURE_SKEW_SECONDS + 1),
        );
        catalog.force_reload_at(NOW);
        assert!(catalog.list_models().is_err());
        assert!(!catalog.health_status().ready);
    }

    #[test]
    fn replication_only_serves_exact_engine_publications_without_scheduler_catalog() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(&root).unwrap();
        let published = write_run(&root, "20260812", 0);
        let stray = write_run(&root, "20260812", 1);
        let publication = replication_publication(&root, &published);
        let catalog = PublishedStoreCatalog::new(
            StoreCatalog::new(&root),
            config_for(PublicationSourceMode::Replication),
        )
        .with_replication_authority(Arc::new(MockReplicationAuthority {
            enabled: true,
            publications: vec![publication],
            unavailable: false,
        }));

        assert_eq!(catalog.list_runs("hrrr").unwrap().len(), 1);
        assert!(catalog.snapshot("hrrr", &published.run_id).is_ok());
        assert!(matches!(
            catalog.snapshot("hrrr", &stray.run_id),
            Err(QueryError::UnknownRun { .. })
        ));
        let status = catalog.health_status();
        assert!(status.ready);
        assert!(status.scheduler_ready);
        assert!(status.replication_ready);
    }

    #[test]
    fn union_requires_both_sources_and_rejects_model_run_collisions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("store");
        fs::create_dir_all(&root).unwrap();
        let active = write_run(&root, "20260812", 0);
        save(&root, &catalog_state(active.clone(), None, NOW));
        let publication = replication_publication(&root, &active);
        let catalog = PublishedStoreCatalog::new(
            StoreCatalog::new(&root),
            config_for(PublicationSourceMode::Union),
        )
        .with_replication_authority(Arc::new(MockReplicationAuthority {
            enabled: true,
            publications: vec![publication],
            unavailable: false,
        }));
        catalog.force_reload_at(NOW);

        assert!(catalog.list_models().is_err());
        assert!(!catalog.health_status().ready);

        let missing_replication = PublishedStoreCatalog::new(
            StoreCatalog::new(&root),
            config_for(PublicationSourceMode::Union),
        )
        .with_replication_authority(Arc::new(MockReplicationAuthority {
            enabled: true,
            publications: Vec::new(),
            unavailable: true,
        }));
        missing_replication.force_reload_at(NOW);
        assert!(missing_replication.list_models().is_err());
        assert!(!missing_replication.health_status().ready);
    }
}

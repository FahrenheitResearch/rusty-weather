//! Read-only view of an rw-store root: enumerate models / runs / timesteps from
//! the on-disk layout (`<root>/<model>/<run>/run.json`) and open timestep files
//! and grid files for the panels.
//!
//! Enumeration is deliberately forgiving: unreadable directories or
//! malformed manifests become warnings on the returned [`StoreTree`] instead
//! of errors, so one broken run never blanks the whole browser.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, Metadata};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::{RwsRunManifest, validate_store_component};
use rw_store::{RwResult, RwStoreError, RwsExactTime};

use crate::worker::format_lead_seconds;

/// Decoded 2D tile budget assigned to each retained hour reader by default.
///
/// A sounding normally touches several surface variables in the same tile,
/// so this is large enough for useful reuse while staying much smaller than
/// [`rw_store::reader::DEFAULT_TILE_CACHE_BYTES`].
pub const DEFAULT_POOLED_READER_TILE_CACHE_BYTES: usize = 1024 * 1024;
/// Maximum decoded-tile capacity reserved by entries owned by one
/// clone-shared [`StoreView`] pool.
///
/// Caller-held readers can outlive eviction, and independently constructed
/// pools have independent limits, so this is not a process-wide memory cap.
pub const MAX_READER_POOL_TILE_CACHE_BYTES: usize = 64 * 1024 * 1024;
/// Maximum number of readers owned by one clone-shared pool. Caller-held
/// `Arc`s can outlive eviction and independent pools retain independently.
pub const MAX_READER_POOL_READERS: usize = 64;

/// Explicit bounds for the application-level hour-reader pool.
///
/// Values are normalized conservatively: pool-owned entry count and reserved
/// tile-cache capacity are clamped to the public maxima, and a per-reader
/// reservation larger than the pool total is clamped to the total. Set
/// `max_readers` to zero (or use
/// [`Self::disabled`]) to bypass retention entirely. A zero per-reader/total
/// tile budget with a nonzero reader count retains parsed readers/mmaps but
/// disables decoded-tile caching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReaderPoolLimits {
    pub max_readers: usize,
    pub per_reader_tile_cache_bytes: usize,
    pub total_tile_cache_bytes: usize,
}

impl ReaderPoolLimits {
    pub const fn disabled() -> Self {
        Self {
            max_readers: 0,
            per_reader_tile_cache_bytes: 0,
            total_tile_cache_bytes: 0,
        }
    }

    fn normalized(self) -> Self {
        let total_tile_cache_bytes = self
            .total_tile_cache_bytes
            .min(MAX_READER_POOL_TILE_CACHE_BYTES);
        let per_reader_tile_cache_bytes =
            self.per_reader_tile_cache_bytes.min(total_tile_cache_bytes);
        let mut max_readers = self.max_readers.min(MAX_READER_POOL_READERS);
        if per_reader_tile_cache_bytes != 0 {
            max_readers = max_readers.min(total_tile_cache_bytes / per_reader_tile_cache_bytes);
        }
        Self {
            max_readers,
            per_reader_tile_cache_bytes,
            total_tile_cache_bytes,
        }
    }
}

impl Default for ReaderPoolLimits {
    fn default() -> Self {
        Self {
            max_readers: MAX_READER_POOL_READERS,
            per_reader_tile_cache_bytes: DEFAULT_POOLED_READER_TILE_CACHE_BYTES,
            total_tile_cache_bytes: MAX_READER_POOL_TILE_CACHE_BYTES,
        }
    }
}

/// Counters and reserved capacity for entries currently owned by one pool.
///
/// An [`Arc<HourReader>`] handed to a caller may outlive its LRU entry, and a
/// separately constructed [`StoreView`] owns a separate pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReaderPoolStats {
    pub hits: u64,
    pub misses: u64,
    pub insertions: u64,
    pub evictions: u64,
    pub invalidations: u64,
    pub entries: usize,
    pub retained_tile_cache_budget_bytes: usize,
}

/// Handle to a store root directory. Cheap to create; all IO happens in
/// [`StoreView::enumerate`] and the `open_*` calls (run them off the UI
/// thread — see [`crate::StoreWorker`]). Clones share one bounded reader
/// pool, including decoded-tile caches and LRU state.
#[derive(Debug, Clone)]
pub struct StoreView {
    root: PathBuf,
    reader_pool: Arc<ReaderPool>,
}

#[derive(Debug)]
struct ReaderPool {
    limits: ReaderPoolLimits,
    state: Mutex<ReaderPoolState>,
}

#[derive(Debug, Default)]
struct ReaderPoolState {
    entries: HashMap<ReaderPoolKey, PooledReader>,
    retained_tile_cache_budget_bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    insertions: u64,
    evictions: u64,
    invalidations: u64,
}

#[derive(Debug)]
struct PooledReader {
    reader: Arc<HourReader>,
    tile_cache_budget_bytes: usize,
    last_used: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReaderPoolKey {
    canonical_path: PathBuf,
    file_generation: FileGeneration,
    manifest_generation: FileGeneration,
    manifest_written_unix: u64,
    exact_time: Option<RwsExactTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileGeneration {
    len: u64,
    modified: Option<SystemTimeFingerprint>,
    created: Option<SystemTimeFingerprint>,
    identity_a: u64,
    identity_b: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct SystemTimeFingerprint {
    before_epoch: bool,
    seconds: u64,
    nanos: u32,
}

#[derive(Debug)]
struct ResolvedHour {
    manifest: RwsRunManifest,
    manifest_path: PathBuf,
    key: ReaderPoolKey,
}

const STABLE_OPEN_ATTEMPTS: usize = 3;

impl ReaderPoolState {
    fn next_stamp(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn get(&mut self, key: &ReaderPoolKey) -> Option<Arc<HourReader>> {
        let stamp = self.next_stamp();
        let reader = self.entries.get_mut(key).map(|entry| {
            entry.last_used = stamp;
            Arc::clone(&entry.reader)
        });
        if reader.is_some() {
            self.hits = self.hits.saturating_add(1);
        } else {
            self.misses = self.misses.saturating_add(1);
        }
        reader
    }

    fn invalidate_other_generations(&mut self, current: &ReaderPoolKey) {
        let stale: Vec<ReaderPoolKey> = self
            .entries
            .keys()
            .filter(|key| key.canonical_path == current.canonical_path && *key != current)
            .cloned()
            .collect();
        for key in stale {
            if let Some(entry) = self.entries.remove(&key) {
                self.retained_tile_cache_budget_bytes = self
                    .retained_tile_cache_budget_bytes
                    .saturating_sub(entry.tile_cache_budget_bytes);
                self.invalidations = self.invalidations.saturating_add(1);
            }
        }
    }

    fn invalidate(&mut self, key: &ReaderPoolKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.retained_tile_cache_budget_bytes = self
                .retained_tile_cache_budget_bytes
                .saturating_sub(entry.tile_cache_budget_bytes);
            self.invalidations = self.invalidations.saturating_add(1);
        }
    }

    fn invalidate_if_reader(&mut self, key: &ReaderPoolKey, reader: &Arc<HourReader>) {
        let matches = self
            .entries
            .get(key)
            .is_some_and(|entry| Arc::ptr_eq(&entry.reader, reader));
        if matches {
            self.invalidate(key);
        }
    }

    fn evict_lru(&mut self) -> bool {
        let Some(key) = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_used)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        let entry = self.entries.remove(&key).expect("LRU key came from map");
        self.retained_tile_cache_budget_bytes = self
            .retained_tile_cache_budget_bytes
            .saturating_sub(entry.tile_cache_budget_bytes);
        self.evictions = self.evictions.saturating_add(1);
        true
    }

    /// Atomically retain `reader` only when `key` is still vacant. If an
    /// unlocked concurrent opener won the race, return its reader instead.
    fn insert_if_absent(
        &mut self,
        key: ReaderPoolKey,
        reader: Arc<HourReader>,
        limits: ReaderPoolLimits,
    ) -> Arc<HourReader> {
        let stamp = self.next_stamp();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = stamp;
            self.hits = self.hits.saturating_add(1);
            return Arc::clone(&entry.reader);
        }
        if limits.max_readers == 0 {
            return reader;
        }
        let budget = limits.per_reader_tile_cache_bytes;
        while self.entries.len() >= limits.max_readers
            || self.retained_tile_cache_budget_bytes.saturating_add(budget)
                > limits.total_tile_cache_bytes
        {
            if !self.evict_lru() {
                return reader;
            }
        }
        self.entries.insert(
            key,
            PooledReader {
                reader: Arc::clone(&reader),
                tile_cache_budget_bytes: budget,
                last_used: stamp,
            },
        );
        self.retained_tile_cache_budget_bytes =
            self.retained_tile_cache_budget_bytes.saturating_add(budget);
        self.insertions = self.insertions.saturating_add(1);
        reader
    }

    fn stats(&self) -> ReaderPoolStats {
        ReaderPoolStats {
            hits: self.hits,
            misses: self.misses,
            insertions: self.insertions,
            evictions: self.evictions,
            invalidations: self.invalidations,
            entries: self.entries.len(),
            retained_tile_cache_budget_bytes: self.retained_tile_cache_budget_bytes,
        }
    }
}

/// Everything the run browser needs, in render order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StoreTree {
    /// Models sorted ascending by name.
    pub models: Vec<ModelEntry>,
    /// Human-readable problems encountered while scanning (broken
    /// manifests, unreadable dirs). The scan itself never fails.
    pub warnings: Vec<String>,
}

impl StoreTree {
    /// Find one enumerated run without reopening its manifest.
    pub fn run(&self, model: &str, run: &str) -> Option<&RunEntry> {
        self.models
            .iter()
            .find(|entry| entry.model == model)?
            .runs
            .iter()
            .find(|entry| entry.run == run)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelEntry {
    pub model: String,
    /// Runs sorted descending by name (newest run first for the usual
    /// `YYYYMMDD_HHz` naming).
    pub runs: Vec<RunEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RunEntry {
    pub run: String,
    /// Writer build stamp from `run.json`.
    pub build: String,
    pub writer_version: String,
    pub nx: usize,
    pub ny: usize,
    /// True only for a validated rw-store v2 exact-time axis. In that case
    /// every `hours` entry carries `exact_time`, and `hour` is an ordinal slot.
    pub exact_time_axis: bool,
    /// Timesteps sorted by their manifest key. That key is a forecast hour in
    /// v1 and an ordinal storage slot in exact-time v2.
    pub hours: Vec<HourEntry>,
}

impl RunEntry {
    /// Complete exact axis keyed by storage slot. A partial axis is never
    /// returned: temporal consumers must either receive every verified time or
    /// stay disabled.
    pub fn exact_times(&self) -> Option<BTreeMap<u16, RwsExactTime>> {
        if !self.exact_time_axis {
            return None;
        }
        self.hours
            .iter()
            .map(|entry| entry.exact_time.map(|exact| (entry.hour, exact)))
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HourEntry {
    /// Manifest storage slot. This is the forecast hour only for legacy v1.
    pub hour: u16,
    /// Timestep file name inside the run directory. Exact-time v2 retains the
    /// `f###.rws` physical naming, but the number is only a storage slot.
    pub file: String,
    pub variable_count: usize,
    pub written_unix: u64,
    pub exact_time: Option<RwsExactTime>,
}

impl StoreView {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_reader_pool_limits(root, ReaderPoolLimits::default())
    }

    /// Construct a view with explicit, clone-shared reader-pool bounds.
    pub fn with_reader_pool_limits(root: impl Into<PathBuf>, limits: ReaderPoolLimits) -> Self {
        Self {
            root: root.into(),
            reader_pool: Arc::new(ReaderPool {
                limits: limits.normalized(),
                state: Mutex::new(ReaderPoolState::default()),
            }),
        }
    }

    /// The normalized limits shared by this view and all of its clones.
    pub fn reader_pool_limits(&self) -> ReaderPoolLimits {
        self.reader_pool.limits
    }

    /// Current counters and reserved capacity for pool-owned entries.
    pub fn reader_pool_stats(&self) -> ReaderPoolStats {
        self.reader_pool
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stats()
    }

    /// Drop all pool-owned readers. Existing caller-held `Arc`s remain valid;
    /// subsequent opens will resolve current filesystem generations afresh.
    pub fn clear_reader_pool(&self) {
        let mut state = self
            .reader_pool
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.entries.clear();
        state.retained_tile_cache_budget_bytes = 0;
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory of one model run: `<root>/<model>/<run>`.
    pub fn run_dir(&self, model: &str, run: &str) -> PathBuf {
        self.root.join(model).join(run)
    }

    /// Scan the store root. A missing root yields an empty tree (the UI
    /// shows its empty state), not an error.
    pub fn enumerate(&self) -> StoreTree {
        let mut tree = StoreTree::default();
        let model_dirs = match read_subdirs(&self.root) {
            Ok(dirs) => dirs,
            Err(err) => {
                if self.root.exists() {
                    tree.warnings.push(format!(
                        "cannot read store root {}: {err}",
                        self.root.display()
                    ));
                }
                return tree;
            }
        };

        for model_dir in model_dirs {
            let model = dir_name(&model_dir);
            let run_dirs = match read_subdirs(&model_dir) {
                Ok(dirs) => dirs,
                Err(err) => {
                    tree.warnings.push(format!(
                        "cannot read model dir {}: {err}",
                        model_dir.display()
                    ));
                    continue;
                }
            };
            let mut runs = Vec::new();
            for run_dir in run_dirs {
                let run = dir_name(&run_dir);
                let manifest_path = run_dir.join("run.json");
                if !manifest_path.is_file() {
                    continue; // not a run directory; skip silently
                }
                match self.load_run_manifest(&model, &run) {
                    Ok((_, manifest)) => runs.push(run_entry(run, manifest)),
                    Err(err) => tree
                        .warnings
                        .push(format!("{}: {err}", manifest_path.display())),
                }
            }
            if runs.is_empty() {
                continue;
            }
            runs.sort_by(|a, b| b.run.cmp(&a.run)); // newest first
            tree.models.push(ModelEntry { model, runs });
        }
        tree.models.sort_by(|a, b| a.model.cmp(&b.model));
        tree
    }

    /// Open one timestep without retaining it in the application-level pool
    /// and with decoded-tile caching disabled. This is the one-shot
    /// path for broad/full-field consumers; use [`Self::open_hour_shared`] for
    /// repeated point/window reads.
    pub fn open_hour(&self, model: &str, run: &str, hour: u16) -> RwResult<HourReader> {
        self.open_hour_with_tile_cache_bytes(model, run, hour, 0)
    }

    /// One-shot/broad-read escape hatch with an explicit reader-local tile
    /// cache. The returned reader is never inserted into the shared pool.
    pub fn open_hour_with_tile_cache_bytes(
        &self,
        model: &str,
        run: &str,
        hour: u16,
        tile_cache_bytes: usize,
    ) -> RwResult<HourReader> {
        for _ in 0..STABLE_OPEN_ATTEMPTS {
            let resolved = self.resolve_hour(model, run, hour)?;
            let reader = HourReader::open_with_tile_cache_bytes(
                &resolved.key.canonical_path,
                tile_cache_bytes,
            )?;
            resolved.manifest.validate_hour_meta(hour, reader.meta())?;
            if reader_matches_snapshot(&reader, &resolved) {
                return Ok(reader);
            }
        }
        Err(hour_changed_error(model, run, hour))
    }

    /// Open or reuse a generation-validated, clone-shared hour reader.
    ///
    /// The key's canonical path and metadata generation are only a fast
    /// lookup. Before any cached or newly opened reader is returned, its
    /// retained source handle is compared with the file currently at that
    /// path, so a same-name replacement cannot accept the prior mmap.
    pub fn open_hour_shared(&self, model: &str, run: &str, hour: u16) -> RwResult<Arc<HourReader>> {
        let limits = self.reader_pool.limits;
        for _ in 0..STABLE_OPEN_ATTEMPTS {
            let resolved = self.resolve_hour(model, run, hour)?;
            let cached = {
                let mut pool = self
                    .reader_pool
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pool.invalidate_other_generations(&resolved.key);
                pool.get(&resolved.key)
            };
            if let Some(reader) = cached {
                if reader_matches_snapshot(&reader, &resolved) {
                    return Ok(reader);
                }
                self.reader_pool
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .invalidate_if_reader(&resolved.key, &reader);
                continue;
            }

            // Opening validates and mmaps the file, so do it without holding
            // the pool mutex. Atomic insertion below resolves a concurrent
            // opener without overwriting or double-accounting its reader.
            let reader = Arc::new(HourReader::open_with_tile_cache_bytes(
                &resolved.key.canonical_path,
                limits.per_reader_tile_cache_bytes,
            )?);
            resolved.manifest.validate_hour_meta(hour, reader.meta())?;
            if !reader_matches_snapshot(&reader, &resolved) {
                continue;
            }

            let reader = {
                let mut pool = self
                    .reader_pool
                    .state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                pool.invalidate_other_generations(&resolved.key);
                pool.insert_if_absent(resolved.key.clone(), reader, limits)
            };
            if reader_matches_snapshot(&reader, &resolved) {
                return Ok(reader);
            }
            self.reader_pool
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .invalidate_if_reader(&resolved.key, &reader);
        }
        Err(hour_changed_error(model, run, hour))
    }

    fn resolve_hour(&self, model: &str, run: &str, hour: u16) -> RwResult<ResolvedHour> {
        for _ in 0..STABLE_OPEN_ATTEMPTS {
            let run_dir = self.canonical_run_dir(model, run)?;
            let manifest_path =
                canonical_contained_path(&run_dir, &run_dir.join("run.json"), "run manifest")?;
            let manifest_generation = file_generation(&manifest_path)?;
            let manifest = RwsRunManifest::load_for_run(&manifest_path, model, run)?;
            if file_generation(&manifest_path)? != manifest_generation {
                continue;
            }
            let entry = manifest.hours.get(&hour).ok_or_else(|| {
                RwStoreError::Meta(format!("run {model}/{run} has no storage slot {hour}"))
            })?;
            let entry_time = entry.exact_time();
            let entry_label = entry_time.map_or_else(
                || format!("forecast hour F{hour:03}"),
                |exact| {
                    format!(
                        "exact-time slot {hour} ({})",
                        format_lead_seconds(exact.lead_seconds)
                    )
                },
            );
            let hour_path = canonical_contained_path(
                &run_dir,
                &run_dir.join(&entry.file),
                &format!("{entry_label} file"),
            )?;
            let key = ReaderPoolKey {
                file_generation: file_generation(&hour_path)?,
                canonical_path: hour_path,
                manifest_generation,
                manifest_written_unix: entry.written_unix,
                exact_time: entry_time,
            };
            if file_generation(&manifest_path)? == manifest_generation {
                return Ok(ResolvedHour {
                    manifest,
                    manifest_path,
                    key,
                });
            }
        }
        Err(hour_changed_error(model, run, hour))
    }

    /// Open the run's grid file (`grid.rwg`).
    pub fn open_grid(&self, model: &str, run: &str) -> RwResult<GridFile> {
        let (run_dir, manifest) = self.load_run_manifest(model, run)?;
        let grid_path = canonical_contained_path(&run_dir, &run_dir.join("grid.rwg"), "grid file")?;
        let grid = GridFile::open(&grid_path)?;
        manifest.validate_grid(&grid.hash, grid.nx, grid.ny)?;
        Ok(grid)
    }

    fn load_run_manifest(&self, model: &str, run: &str) -> RwResult<(PathBuf, RwsRunManifest)> {
        let run_dir = self.canonical_run_dir(model, run)?;
        let manifest_path =
            canonical_contained_path(&run_dir, &run_dir.join("run.json"), "run manifest")?;
        let manifest = RwsRunManifest::load_for_run(&manifest_path, model, run)?;
        Ok((run_dir, manifest))
    }

    fn canonical_run_dir(&self, model: &str, run: &str) -> RwResult<PathBuf> {
        validate_store_component("model", model)?;
        validate_store_component("run", run)?;
        let root = fs::canonicalize(&self.root).map_err(|err| {
            RwStoreError::Meta(format!(
                "cannot resolve store root {}: {err}",
                self.root.display()
            ))
        })?;
        let requested = self.run_dir(model, run);
        let run_dir = fs::canonicalize(&requested).map_err(|err| {
            RwStoreError::Meta(format!(
                "cannot resolve run directory {}: {err}",
                requested.display()
            ))
        })?;
        if !run_dir.starts_with(&root) {
            return Err(RwStoreError::Meta(format!(
                "run directory {} resolves outside store root {}",
                requested.display(),
                root.display()
            )));
        }
        Ok(run_dir)
    }
}

fn hour_changed_error(model: &str, run: &str, hour: u16) -> RwStoreError {
    RwStoreError::Meta(format!(
        "run {model}/{run} storage slot {hour} changed repeatedly while it was being opened"
    ))
}

fn reader_matches_snapshot(reader: &HourReader, resolved: &ResolvedHour) -> bool {
    // Call source identity even when the metadata fast path has changed. This
    // keeps every candidate reader checked against the exact current file.
    let source_matches = reader
        .source_matches_path(&resolved.key.canonical_path)
        .unwrap_or(false);
    source_matches && snapshot_is_current(resolved)
}

fn snapshot_is_current(resolved: &ResolvedHour) -> bool {
    file_generation(&resolved.key.canonical_path)
        .is_ok_and(|generation| generation == resolved.key.file_generation)
        && file_generation(&resolved.manifest_path)
            .is_ok_and(|generation| generation == resolved.key.manifest_generation)
}

fn file_generation(path: &Path) -> RwResult<FileGeneration> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(RwStoreError::Meta(format!(
            "reader-pool path {} is not a regular file",
            path.display()
        )));
    }
    let (identity_a, identity_b) = platform_file_identity(&metadata);
    Ok(FileGeneration {
        len: metadata.len(),
        modified: system_time_fingerprint(metadata.modified()),
        created: system_time_fingerprint(metadata.created()),
        identity_a,
        identity_b,
    })
}

fn system_time_fingerprint(time: std::io::Result<SystemTime>) -> Option<SystemTimeFingerprint> {
    let time = time.ok()?;
    Some(match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => SystemTimeFingerprint {
            before_epoch: false,
            seconds: duration.as_secs(),
            nanos: duration.subsec_nanos(),
        },
        Err(error) => {
            let duration = error.duration();
            SystemTimeFingerprint {
                before_epoch: true,
                seconds: duration.as_secs(),
                nanos: duration.subsec_nanos(),
            }
        }
    })
}

#[cfg(windows)]
fn platform_file_identity(metadata: &Metadata) -> (u64, u64) {
    use std::os::windows::fs::MetadataExt;

    // Stable raw 100-ns Windows timestamps. Atomic ingest writes through a
    // newly created sibling temp, so replacement changes this generation
    // even when the final path and payload length are unchanged.
    (metadata.creation_time(), metadata.last_write_time())
}

#[cfg(unix)]
fn platform_file_identity(metadata: &Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;

    (metadata.dev(), metadata.ino())
}

#[cfg(not(any(windows, unix)))]
fn platform_file_identity(_metadata: &Metadata) -> (u64, u64) {
    (0, 0)
}

fn run_entry(run: String, manifest: RwsRunManifest) -> RunEntry {
    let exact_time_axis = manifest.is_exact_time_axis();
    let hours = manifest
        .hours
        .iter() // BTreeMap: already ascending by hour
        .map(|(&hour, entry)| HourEntry {
            hour,
            file: entry.file.clone(),
            variable_count: entry.variables.len(),
            written_unix: entry.written_unix,
            exact_time: entry.exact_time(),
        })
        .collect();
    RunEntry {
        run,
        build: manifest.writer.build,
        writer_version: manifest.writer.version,
        nx: manifest.nx,
        ny: manifest.ny,
        exact_time_axis,
        hours,
    }
}

fn canonical_contained_path(run_dir: &Path, path: &Path, label: &str) -> RwResult<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|err| {
        RwStoreError::Meta(format!("cannot resolve {label} {}: {err}", path.display()))
    })?;
    if !canonical.starts_with(run_dir) {
        return Err(RwStoreError::Meta(format!(
            "{label} {} resolves outside run directory {}",
            path.display(),
            run_dir.display()
        )));
    }
    Ok(canonical)
}

/// Subdirectories of `dir`, sorted by name for deterministic scans.
fn read_subdirs(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    Ok(dirs)
}

fn dir_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

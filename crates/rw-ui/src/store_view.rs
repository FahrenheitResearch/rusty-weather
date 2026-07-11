//! Read-only view of an rw-store root: enumerate models / runs / timesteps from
//! the on-disk layout (`<root>/<model>/<run>/run.json`) and open timestep files
//! and grid files for the panels.
//!
//! Enumeration is deliberately forgiving: unreadable directories or
//! malformed manifests become warnings on the returned [`StoreTree`] instead
//! of errors, so one broken run never blanks the whole browser.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::{RwsRunManifest, validate_store_component};
use rw_store::{RwResult, RwStoreError, RwsExactTime};

use crate::worker::format_lead_seconds;

/// Handle to a store root directory. Cheap to create; all IO happens in
/// [`StoreView::enumerate`] and the `open_*` calls (run them off the UI
/// thread — see [`crate::StoreWorker`]).
#[derive(Debug, Clone)]
pub struct StoreView {
    root: PathBuf,
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
        Self { root: root.into() }
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

    /// Open the timestep file for (`model`, `run`, `hour`), resolving the file
    /// name through `run.json` (the manifest is the source of truth). `hour`
    /// is an opaque ordinal slot for an exact-time v2 run.
    pub fn open_hour(&self, model: &str, run: &str, hour: u16) -> RwResult<HourReader> {
        let (run_dir, manifest) = self.load_run_manifest(model, run)?;
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
        let reader = HourReader::open(&hour_path)?;
        let meta = reader.meta();
        manifest.validate_hour_meta(hour, meta)?;
        Ok(reader)
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

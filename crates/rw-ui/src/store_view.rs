//! Read-only view of an rw-store root: enumerate models / runs / hours from
//! the on-disk layout (`<root>/<model>/<run>/run.json`) and open hour files
//! and grid files for the panels.
//!
//! Enumeration is deliberately forgiving: unreadable directories or
//! malformed manifests become warnings on the returned [`StoreTree`] instead
//! of errors, so one broken run never blanks the whole browser.

use std::fs;
use std::path::{Path, PathBuf};

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::run::{RwsRunManifest, SCHEMA_RUN};
use rw_store::{RwResult, RwStoreError};

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
    /// Hours sorted ascending by forecast hour.
    pub hours: Vec<HourEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HourEntry {
    pub hour: u16,
    /// Hour file name inside the run directory (e.g. `f006.rws`).
    pub file: String,
    pub variable_count: usize,
    pub written_unix: u64,
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
                    tree.warnings
                        .push(format!("cannot read store root {}: {err}", self.root.display()));
                }
                return tree;
            }
        };

        for model_dir in model_dirs {
            let model = dir_name(&model_dir);
            let run_dirs = match read_subdirs(&model_dir) {
                Ok(dirs) => dirs,
                Err(err) => {
                    tree.warnings
                        .push(format!("cannot read model dir {}: {err}", model_dir.display()));
                    continue;
                }
            };
            let mut runs = Vec::new();
            for run_dir in run_dirs {
                let manifest_path = run_dir.join("run.json");
                if !manifest_path.is_file() {
                    continue; // not a run directory; skip silently
                }
                match load_manifest(&manifest_path) {
                    Ok(manifest) => runs.push(run_entry(&run_dir, manifest)),
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

    /// Open the hour file for (`model`, `run`, `hour`), resolving the file
    /// name through `run.json` (the manifest is the source of truth).
    pub fn open_hour(&self, model: &str, run: &str, hour: u16) -> RwResult<HourReader> {
        let run_dir = self.run_dir(model, run);
        let manifest = load_manifest(&run_dir.join("run.json"))?;
        let entry = manifest.hours.get(&hour).ok_or_else(|| {
            RwStoreError::Meta(format!("run {model}/{run} has no forecast hour {hour}"))
        })?;
        HourReader::open(&run_dir.join(&entry.file))
    }

    /// Open the run's grid file (`grid.rwg`).
    pub fn open_grid(&self, model: &str, run: &str) -> RwResult<GridFile> {
        GridFile::open(&self.run_dir(model, run).join("grid.rwg"))
    }
}

fn run_entry(run_dir: &Path, manifest: RwsRunManifest) -> RunEntry {
    let hours = manifest
        .hours
        .iter() // BTreeMap: already ascending by hour
        .map(|(&hour, entry)| HourEntry {
            hour,
            file: entry.file.clone(),
            variable_count: entry.variables.len(),
            written_unix: entry.written_unix,
        })
        .collect();
    RunEntry {
        // Prefer the manifest's run name; the directory name should match.
        run: if manifest.run.is_empty() { dir_name(run_dir) } else { manifest.run },
        build: manifest.writer.build,
        writer_version: manifest.writer.version,
        nx: manifest.nx,
        ny: manifest.ny,
        hours,
    }
}

/// Load and schema-check a `run.json` manifest.
fn load_manifest(path: &Path) -> RwResult<RwsRunManifest> {
    let bytes = fs::read(path)?;
    let manifest: RwsRunManifest = serde_json::from_slice(&bytes)
        .map_err(|err| RwStoreError::Meta(format!("run manifest JSON: {err}")))?;
    if manifest.schema != SCHEMA_RUN {
        return Err(RwStoreError::Meta(format!(
            "unexpected schema '{}' (expected '{SCHEMA_RUN}')",
            manifest.schema
        )));
    }
    Ok(manifest)
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

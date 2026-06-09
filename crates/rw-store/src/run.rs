//! Run-level manifest (`run.json`): which hour files exist for a model run,
//! keyed by forecast hour, plus the grid identity they were written against.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write_bytes;
use crate::error::{RwResult, RwStoreError};
use crate::format::RwsWriterInfo;

/// Schema identifier embedded in run manifests.
pub const SCHEMA_RUN: &str = "rw-store.run.v1";

/// One registered forecast hour: the hour file plus write provenance.
/// `written_unix` is supplied by the caller (the library never reads the
/// clock), so tests and replays stay deterministic.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsHourEntry {
    pub file: String,
    pub written_unix: u64,
    pub encode_ms: u64,
    pub variables: Vec<String>,
}

/// Run manifest: identity of the run (model, run, grid) and the map of
/// forecast hours written so far. Hours are a BTreeMap so the JSON is
/// stable-ordered and re-registering an hour overwrites in place.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsRunManifest {
    pub schema: String,
    pub model: String,
    pub run: String,
    pub grid_hash: String,
    pub nx: usize,
    pub ny: usize,
    pub hours: BTreeMap<u16, RwsHourEntry>,
    pub writer: RwsWriterInfo,
}

impl RwsRunManifest {
    /// Load the manifest at `path`, or create a fresh empty one if the file
    /// does not exist. An existing manifest must match `model`, `run`, and
    /// `grid_hash` exactly ([`RwStoreError::Meta`] otherwise) — a mismatch
    /// means the directory holds a different run's data.
    pub fn load_or_new(
        path: &Path,
        model: &str,
        run: &str,
        grid_hash: &str,
        nx: usize,
        ny: usize,
        writer: RwsWriterInfo,
    ) -> RwResult<Self> {
        if path.exists() {
            let bytes = fs::read(path)?;
            let manifest: Self = serde_json::from_slice(&bytes)
                .map_err(|err| RwStoreError::Meta(format!("run manifest JSON: {err}")))?;
            if manifest.schema != SCHEMA_RUN {
                return Err(RwStoreError::Meta(format!(
                    "unexpected schema '{}' (expected '{SCHEMA_RUN}')",
                    manifest.schema
                )));
            }
            if manifest.model != model || manifest.run != run || manifest.grid_hash != grid_hash {
                return Err(RwStoreError::Meta(format!(
                    "existing manifest is for model='{}' run='{}' grid_hash='{}'; \
                     requested model='{model}' run='{run}' grid_hash='{grid_hash}'",
                    manifest.model, manifest.run, manifest.grid_hash
                )));
            }
            return Ok(manifest);
        }
        Ok(Self {
            schema: SCHEMA_RUN.to_string(),
            model: model.to_string(),
            run: run.to_string(),
            grid_hash: grid_hash.to_string(),
            nx,
            ny,
            hours: BTreeMap::new(),
            writer,
        })
    }

    /// Insert or overwrite the entry for `hour`.
    pub fn register_hour(&mut self, hour: u16, entry: RwsHourEntry) {
        self.hours.insert(hour, entry);
    }

    /// Atomically write the manifest as pretty JSON.
    pub fn save(&self, path: &Path) -> RwResult<()> {
        let mut bytes = serde_json::to_vec_pretty(self)
            .map_err(|err| RwStoreError::Meta(err.to_string()))?;
        bytes.push(b'\n');
        atomic_write_bytes(path, &bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RwStoreError;
    use crate::format::RwsWriterInfo;
    use std::fs;
    use std::path::PathBuf;

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rw-store-run-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn writer_info() -> RwsWriterInfo {
        RwsWriterInfo {
            name: "rw-store".to_string(),
            version: "0.1.0".to_string(),
            build: "test-build".to_string(),
        }
    }

    fn entry(file: &str, written_unix: u64, encode_ms: u64, variables: &[&str]) -> RwsHourEntry {
        RwsHourEntry {
            file: file.to_string(),
            written_unix,
            encode_ms,
            variables: variables.iter().map(|v| v.to_string()).collect(),
        }
    }

    #[test]
    fn run_manifest_round_trips_and_registers_hours() {
        let dir = test_dir("round-trip");
        let path = dir.join("run.json");

        let mut manifest = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "2026-06-09T12:00:00Z",
            "gridhash-test",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        assert_eq!(manifest.schema, "rw-store.run.v1");
        assert_eq!(manifest.model, "hrrr");
        assert_eq!(manifest.run, "2026-06-09T12:00:00Z");
        assert_eq!(manifest.grid_hash, "gridhash-test");
        assert_eq!((manifest.nx, manifest.ny), (600, 500));
        assert!(manifest.hours.is_empty(), "new manifest starts empty");

        manifest.register_hour(
            0,
            entry("f000.rws", 1_770_000_000, 850, &["temp_2m", "dewpoint_2m"]),
        );
        manifest.register_hour(6, entry("f006.rws", 1_770_000_600, 912, &["temp_2m"]));
        manifest.save(&path).unwrap();

        let loaded = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "2026-06-09T12:00:00Z",
            "gridhash-test",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        assert_eq!(loaded, manifest, "save -> load must round-trip exactly");
        assert_eq!(loaded.hours.len(), 2);
        assert_eq!(loaded.hours[&0].file, "f000.rws");
        assert_eq!(loaded.hours[&0].written_unix, 1_770_000_000);
        assert_eq!(loaded.hours[&0].variables, vec!["temp_2m", "dewpoint_2m"]);
        assert_eq!(loaded.hours[&6].encode_ms, 912);

        // Re-registering an hour overwrites in place; the map does not grow.
        let mut manifest = loaded;
        manifest.register_hour(0, entry("f000-v2.rws", 1_770_001_000, 700, &["temp_2m"]));
        assert_eq!(manifest.hours.len(), 2, "overwrite must not add an entry");
        assert_eq!(manifest.hours[&0].file, "f000-v2.rws");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_new_rejects_mismatched_existing_manifest() {
        let dir = test_dir("mismatch");
        let path = dir.join("run.json");
        let manifest = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "2026-06-09T12:00:00Z",
            "gridhash-a",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        manifest.save(&path).unwrap();

        let err = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "2026-06-09T12:00:00Z",
            "gridhash-b",
            600,
            500,
            writer_info(),
        )
        .unwrap_err();
        assert!(
            matches!(err, RwStoreError::Meta(_)),
            "expected Meta error for grid_hash mismatch, got {err:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

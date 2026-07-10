//! Run-level manifest (`run.json`): which hour files exist for a model run,
//! keyed by forecast hour, plus the grid identity they were written against.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

use crate::atomic::atomic_write_bytes;
use crate::error::{RwResult, RwStoreError};
use crate::format::RwsWriterInfo;

/// Schema identifier embedded in run manifests.
pub const SCHEMA_RUN: &str = "rw-store.run.v1";

/// Maximum accepted `run.json` size. Manifests are metadata, not payloads;
/// bounding the read prevents a corrupt or hostile store from forcing an
/// unbounded allocation before JSON parsing begins.
pub const MAX_RUN_MANIFEST_BYTES: u64 = 16 * 1024 * 1024;

/// The grid reader applies the same ceiling to each decompressed f32
/// coordinate array. Reject impossible manifest geometry before a consumer
/// uses it to size work or compares it with an hour/grid file.
const MAX_GRID_COORD_RAW_BYTES: u64 = 1 << 31;

/// Require a store identity or persisted child filename to be exactly one
/// relative path component. The explicit backslash check keeps serialized
/// manifests safe when moved between Unix and Windows, where path separator
/// rules differ.
pub fn validate_store_component(label: &str, value: &str) -> RwResult<()> {
    let mut components = Path::new(value).components();
    let first_is_exact_normal = matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new(value)
    );
    if !first_is_exact_normal
        || components.next().is_some()
        || value.contains('\\')
        || value.contains('\0')
        || value.contains(':')
        || value
            .chars()
            .any(|ch| matches!(ch, '<' | '>' | '"' | '|' | '?' | '*'))
        || value.ends_with('.')
        || value.ends_with(' ')
        || value.chars().any(|ch| ch.is_control())
        || is_windows_device_name(value)
    {
        return Err(RwStoreError::Meta(format!(
            "{label} '{value}' must be exactly one normal path component"
        )));
    }
    Ok(())
}

fn is_windows_device_name(value: &str) -> bool {
    let stem = value
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(|ch| ch == ' ' || ch == '.')
        .to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" | "CLOCK$"
    )
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
            })
}

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
    /// Read and parse one persisted manifest without trusting its contents.
    /// The read is capped even if the file grows after its metadata is
    /// inspected. Diagnostic tools may use this to retain a parsed value and
    /// report several problems; normal consumers should use [`Self::load`].
    pub fn load_bounded(path: &Path) -> RwResult<Self> {
        let initial_meta = path.metadata()?;
        if !initial_meta.is_file() {
            return Err(RwStoreError::Meta(format!(
                "run manifest {} is not a regular file",
                path.display()
            )));
        }
        let file = File::open(path)?;
        let open_meta = file.metadata()?;
        if !open_meta.is_file() {
            return Err(RwStoreError::Meta(format!(
                "run manifest {} changed to a non-file before it was opened",
                path.display()
            )));
        }
        let file_len = open_meta.len();
        if file_len > MAX_RUN_MANIFEST_BYTES {
            return Err(RwStoreError::Meta(format!(
                "run manifest {} is {file_len} bytes; limit is {MAX_RUN_MANIFEST_BYTES} bytes",
                path.display()
            )));
        }

        let mut bytes = Vec::with_capacity(file_len as usize);
        file.take(MAX_RUN_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_RUN_MANIFEST_BYTES {
            return Err(RwStoreError::Meta(format!(
                "run manifest {} grew beyond the {MAX_RUN_MANIFEST_BYTES}-byte limit while reading",
                path.display()
            )));
        }

        let manifest: Self = serde_json::from_slice(&bytes)
            .map_err(|err| RwStoreError::Meta(format!("run manifest JSON: {err}")))?;
        Ok(manifest)
    }

    /// Read, parse, and fully validate one persisted manifest.
    pub fn load(path: &Path) -> RwResult<Self> {
        let manifest = Self::load_bounded(path)?;
        manifest.validate_contents()?;
        Ok(manifest)
    }

    /// Load a manifest and require its persisted model/run identity to match
    /// the directory identity selected by the caller. Expected components are
    /// checked before the file is opened, so `..` cannot redirect the read.
    pub fn load_for_run(path: &Path, model: &str, run: &str) -> RwResult<Self> {
        validate_store_component("model", model)?;
        validate_store_component("run", run)?;
        let manifest = Self::load(path)?;
        manifest.validate_identity(model, run)?;
        Ok(manifest)
    }

    /// Validate the manifest's model/run identity against its containing
    /// directory (or another trusted identity source).
    pub fn validate_identity(&self, model: &str, run: &str) -> RwResult<()> {
        validate_store_component("model", model)?;
        validate_store_component("run", run)?;
        if self.model != model || self.run != run {
            return Err(RwStoreError::Meta(format!(
                "manifest identity is model='{}' run='{}'; expected model='{model}' run='{run}'",
                self.model, self.run
            )));
        }
        Ok(())
    }

    /// Validate the grid identity and geometry recorded by another trusted
    /// store object (`grid.rwg` or an opened hour file).
    pub fn validate_grid(&self, grid_hash: &str, nx: usize, ny: usize) -> RwResult<()> {
        if self.grid_hash != grid_hash || self.nx != nx || self.ny != ny {
            return Err(RwStoreError::Meta(format!(
                "manifest grid is {}x{} hash='{}'; expected {nx}x{ny} hash='{grid_hash}'",
                self.nx, self.ny, self.grid_hash
            )));
        }
        Ok(())
    }

    /// Validate a parsed manifest's schema, safe identities, geometry, and
    /// registered hour filenames. Primarily public for diagnostic tooling;
    /// regular readers get this automatically through [`Self::load`].
    pub fn validate_contents(&self) -> RwResult<()> {
        if self.schema != SCHEMA_RUN {
            return Err(RwStoreError::Meta(format!(
                "unexpected schema '{}' (expected '{SCHEMA_RUN}')",
                self.schema
            )));
        }
        validate_store_component("manifest model", &self.model)?;
        validate_store_component("manifest run", &self.run)?;
        if self.grid_hash.trim().is_empty() {
            return Err(RwStoreError::Meta(
                "manifest grid_hash must not be empty".to_string(),
            ));
        }
        if self.nx == 0 || self.ny == 0 {
            return Err(RwStoreError::Meta(format!(
                "degenerate manifest grid {}x{} (nx and ny must be nonzero)",
                self.nx, self.ny
            )));
        }
        let raw_bytes = self
            .nx
            .checked_mul(self.ny)
            .and_then(|cells| u64::try_from(cells).ok())
            .and_then(|cells| cells.checked_mul(4))
            .filter(|&bytes| bytes <= MAX_GRID_COORD_RAW_BYTES)
            .ok_or_else(|| {
                RwStoreError::Meta(format!(
                    "manifest grid {}x{} exceeds the supported coordinate array size",
                    self.nx, self.ny
                ))
            })?;
        debug_assert!(raw_bytes > 0);

        let mut files = BTreeSet::new();
        for (&hour, entry) in &self.hours {
            validate_store_component(&format!("manifest hour F{hour:03} file"), &entry.file)?;
            if !files.insert(entry.file.as_str()) {
                return Err(RwStoreError::Meta(format!(
                    "manifest hour F{hour:03} reuses file '{}' already registered to another hour",
                    entry.file
                )));
            }
        }
        Ok(())
    }

    /// Load the manifest at `path`, or create a fresh empty one if the file
    /// does not exist. An existing manifest must match `model`, `run`, grid
    /// identity, and geometry exactly ([`RwStoreError::Meta`] otherwise).
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
            let manifest = Self::load_for_run(path, model, run)?;
            manifest.validate_grid(grid_hash, nx, ny)?;
            return Ok(manifest);
        }
        let manifest = Self {
            schema: SCHEMA_RUN.to_string(),
            model: model.to_string(),
            run: run.to_string(),
            grid_hash: grid_hash.to_string(),
            nx,
            ny,
            hours: BTreeMap::new(),
            writer,
        };
        manifest.validate_contents()?;
        Ok(manifest)
    }

    /// Insert or overwrite the entry for `hour`.
    pub fn register_hour(&mut self, hour: u16, entry: RwsHourEntry) {
        self.hours.insert(hour, entry);
    }

    /// Atomically write the manifest as pretty JSON.
    pub fn save(&self, path: &Path) -> RwResult<()> {
        self.validate_contents()?;
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|err| RwStoreError::Meta(err.to_string()))?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_RUN_MANIFEST_BYTES {
            return Err(RwStoreError::Meta(format!(
                "serialized run manifest is {} bytes; limit is {MAX_RUN_MANIFEST_BYTES} bytes",
                bytes.len()
            )));
        }
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
        let dir =
            std::env::temp_dir().join(format!("rw-store-run-{}-{}", std::process::id(), name));
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
            "20260609_12z",
            "gridhash-test",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        assert_eq!(manifest.schema, "rw-store.run.v1");
        assert_eq!(manifest.model, "hrrr");
        assert_eq!(manifest.run, "20260609_12z");
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
            "20260609_12z",
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
            "20260609_12z",
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
            "20260609_12z",
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

    #[test]
    fn bounded_loader_rejects_oversized_manifest_before_json_parse() {
        let dir = test_dir("oversized");
        let path = dir.join("run.json");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_RUN_MANIFEST_BYTES + 1).unwrap();

        let err = RwsRunManifest::load(&path).unwrap_err();
        assert!(matches!(err, RwStoreError::Meta(_)));
        assert!(err.to_string().contains("limit"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn loader_rejects_directory_identity_mismatch_and_unsafe_components() {
        let dir = test_dir("identity");
        let path = dir.join("run.json");
        let mut manifest = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "20260609_12z",
            "gridhash-test",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        manifest.register_hour(0, entry("f000.rws", 1_770_000_000, 850, &["temp_2m"]));
        manifest.save(&path).unwrap();

        let err = RwsRunManifest::load_for_run(&path, "hrrr", "20260609_00z").unwrap_err();
        assert!(err.to_string().contains("manifest identity"), "{err}");

        // Expected identities are checked before any attempted read.
        let err = RwsRunManifest::load_for_run(
            &dir.join("missing.json"),
            "../hrrr",
            "20260609_12z",
        )
        .unwrap_err();
        assert!(matches!(err, RwStoreError::Meta(_)));

        for (unsafe_model, unsafe_run) in [
            ("../hrrr", "20260609_12z"),
            ("hrrr", "../20260609_12z"),
        ] {
            let mut tampered = manifest.clone();
            tampered.model = unsafe_model.to_string();
            tampered.run = unsafe_run.to_string();
            fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
            let err = RwsRunManifest::load(&path).unwrap_err();
            assert!(
                matches!(err, RwStoreError::Meta(_)),
                "unsafe manifest identity must be rejected: {err:?}"
            );
        }

        for unsafe_file in [
            "",
            ".",
            "..",
            "../f000.rws",
            "nested/f000.rws",
            "nested\\f000.rws",
            "f000.rws:stream",
            "f000?.rws",
            "f000*.rws",
            "f000<old>.rws",
            "f000|old.rws",
            "f000\"old.rws",
            "f000.rws.",
            "NUL",
            "con.txt",
            "CONIN$",
            "conout$.log",
            "CLOCK$",
            "COM1",
        ] {
            manifest.hours.get_mut(&0).unwrap().file = unsafe_file.to_string();
            fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
            let err = RwsRunManifest::load_for_run(&path, "hrrr", "20260609_12z")
                .unwrap_err();
            assert!(
                matches!(err, RwStoreError::Meta(_)),
                "'{unsafe_file}' must be rejected: {err:?}"
            );
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_or_new_rejects_existing_geometry_mismatch() {
        let dir = test_dir("geometry-mismatch");
        let path = dir.join("run.json");
        let manifest = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "20260609_12z",
            "gridhash-test",
            600,
            500,
            writer_info(),
        )
        .unwrap();
        manifest.save(&path).unwrap();

        let err = RwsRunManifest::load_or_new(
            &path,
            "hrrr",
            "20260609_12z",
            "gridhash-test",
            601,
            500,
            writer_info(),
        )
        .unwrap_err();
        assert!(matches!(err, RwStoreError::Meta(_)));
        assert!(err.to_string().contains("600x500"), "{err}");

        let mut degenerate = manifest;
        degenerate.nx = 0;
        fs::write(&path, serde_json::to_vec(&degenerate).unwrap()).unwrap();
        let err = RwsRunManifest::load(&path).unwrap_err();
        assert!(err.to_string().contains("degenerate manifest grid"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }
}

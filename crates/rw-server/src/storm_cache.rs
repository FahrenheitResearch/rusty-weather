//! Durable exact-frame storm-object cache.
//!
//! One cache hit is a directory installed by a single same-parent rename. The
//! directory contains canonical JSON, GeoJSON, and a manifest binding both
//! byte streams to the exact stored source and deterministic method identity.
//! A crash can therefore leave only an unreachable staging directory; clients
//! never observe half of a canonical/GeoJSON pair.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use rw_ops_protocol::{StormCellFrame, StormMethodIdentity, StormSource};
use serde::{Deserialize, Serialize};

use crate::config::StormCacheRetention;

pub(crate) const STORM_CACHE_REVISION: &str = "rw-server.storm-frame-cache.v2";
const CACHE_DIRECTORY: &str = ".rw-storm-frame-cache";
const CACHE_LAYOUT: &str = "v2";
const MANIFEST_SCHEMA: &str = "rw-server.storm-frame-cache-entry.v2";
const MANIFEST_FILE: &str = "manifest.json";
const CANONICAL_FILE: &str = "canonical.json";
const GEOJSON_FILE: &str = "geojson.json";

#[derive(Clone, Debug)]
pub(crate) struct StormFrameDiskCache {
    inner: Arc<DiskCacheInner>,
}

#[derive(Debug)]
struct DiskCacheInner {
    root: PathBuf,
    retention: StormCacheRetention,
    mutation: Mutex<()>,
    health: Mutex<StormDiskCacheHealth>,
}

#[derive(Clone, Debug)]
pub(crate) struct CachedStormFrame {
    pub(crate) frame: Arc<StormCellFrame>,
    pub(crate) canonical: Bytes,
    pub(crate) geojson: Bytes,
}

impl CachedStormFrame {
    pub(crate) fn estimated_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.canonical.len())
            .saturating_add(self.geojson.len())
            .saturating_add(crate::storms::estimated_frame_bytes(&self.frame))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct StormCacheIdentity {
    pub(crate) key: String,
    pub(crate) model: String,
    pub(crate) run: String,
    pub(crate) snapshot_id: String,
    pub(crate) grid_hash: String,
    pub(crate) storage_slot: u16,
    pub(crate) variable: String,
    pub(crate) source: StormSource,
}

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct StormDiskCacheHealth {
    pub(crate) ready: bool,
    pub(crate) cache_revision: &'static str,
    pub(crate) entries: u64,
    pub(crate) bytes: u64,
    pub(crate) recovered_staging_entries: u64,
    pub(crate) recovered_invalid_entries: u64,
    pub(crate) disk_hits: u64,
    pub(crate) atomic_store_writes: u64,
    pub(crate) last_hit_unix_ms: Option<i64>,
    pub(crate) last_store_unix_ms: Option<i64>,
    pub(crate) last_error_unix_ms: Option<i64>,
    pub(crate) last_error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct EntryManifest {
    schema: String,
    cache_revision: String,
    cache_key: String,
    created_at_unix_ms: i64,
    model: String,
    run: String,
    snapshot_id: String,
    grid_hash: String,
    storage_slot: u16,
    variable: String,
    source: StormSource,
    method: StormMethodIdentity,
    canonical_blake3: String,
    canonical_bytes: u64,
    geojson_blake3: String,
    geojson_bytes: u64,
}

impl StormFrameDiskCache {
    pub(crate) fn open(cache_root: &Path, retention: StormCacheRetention) -> io::Result<Self> {
        let root = cache_root.join(CACHE_DIRECTORY).join(CACHE_LAYOUT);
        fs::create_dir_all(&root)?;
        ensure_real_directory(&root)?;
        let (recovered_staging_entries, recovered_invalid_entries) = recover_layout(&root)?;
        let (entries, bytes) = scan_totals(&root)?;
        let cache = Self {
            inner: Arc::new(DiskCacheInner {
                root,
                retention,
                mutation: Mutex::new(()),
                health: Mutex::new(StormDiskCacheHealth {
                    ready: true,
                    cache_revision: STORM_CACHE_REVISION,
                    entries,
                    bytes,
                    recovered_staging_entries,
                    recovered_invalid_entries,
                    ..StormDiskCacheHealth::default()
                }),
            }),
        };
        cache.prune()?;
        Ok(cache)
    }

    pub(crate) fn health(&self) -> StormDiskCacheHealth {
        self.lock_health().clone()
    }

    /// Load one complete exact entry. Invalid derived data is discarded and
    /// returned as a miss; scientific source data is never changed.
    pub(crate) fn load(
        &self,
        identity: &StormCacheIdentity,
    ) -> io::Result<Option<Arc<CachedStormFrame>>> {
        validate_key(&identity.key)?;
        let path = entry_path(&self.inner.root, &identity.key);
        match load_entry(&path, identity) {
            Ok(Some(entry)) => {
                let mut health = self.lock_health();
                health.ready = true;
                health.disk_hits = health.disk_hits.saturating_add(1);
                health.last_hit_unix_ms = Some(now_unix_ms());
                health.last_error = None;
                Ok(Some(Arc::new(entry)))
            }
            Ok(None) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                let _guard = self.lock_mutation();
                discard_directory(&path)?;
                let (entries, bytes) = scan_totals(&self.inner.root)?;
                let mut health = self.lock_health();
                health.entries = entries;
                health.bytes = bytes;
                health.recovered_invalid_entries =
                    health.recovered_invalid_entries.saturating_add(1);
                health.last_error_unix_ms = Some(now_unix_ms());
                health.last_error = Some(bound_error(error.to_string()));
                Ok(None)
            }
            Err(error) => {
                self.record_error(&error);
                Err(error)
            }
        }
    }

    /// Install canonical and GeoJSON bytes as one atomic cache entry.
    pub(crate) fn store(
        &self,
        identity: &StormCacheIdentity,
        entry: &CachedStormFrame,
    ) -> io::Result<()> {
        let result = self.store_inner(identity, entry);
        if let Err(error) = &result {
            self.record_error(error);
        }
        result
    }

    fn store_inner(
        &self,
        identity: &StormCacheIdentity,
        entry: &CachedStormFrame,
    ) -> io::Result<()> {
        validate_key(&identity.key)?;
        if entry.frame.source != identity.source {
            return Err(invalid_input(
                "storm frame source differs from its exact cache identity",
            ));
        }
        entry
            .frame
            .validate()
            .map_err(|error| invalid_input(format!("invalid storm frame: {error}")))?;
        validate_geojson_identity(&entry.geojson, &entry.frame)?;

        let canonical_blake3 = blake3::hash(&entry.canonical).to_hex().to_string();
        let geojson_blake3 = blake3::hash(&entry.geojson).to_hex().to_string();
        let manifest = EntryManifest {
            schema: MANIFEST_SCHEMA.into(),
            cache_revision: STORM_CACHE_REVISION.into(),
            cache_key: identity.key.clone(),
            created_at_unix_ms: now_unix_ms(),
            model: identity.model.clone(),
            run: identity.run.clone(),
            snapshot_id: identity.snapshot_id.clone(),
            grid_hash: identity.grid_hash.clone(),
            storage_slot: identity.storage_slot,
            variable: identity.variable.clone(),
            source: identity.source.clone(),
            method: entry.frame.method.clone(),
            canonical_blake3,
            canonical_bytes: u64::try_from(entry.canonical.len())
                .map_err(|_| invalid_input("canonical JSON length does not fit in u64"))?,
            geojson_blake3,
            geojson_bytes: u64::try_from(entry.geojson.len())
                .map_err(|_| invalid_input("GeoJSON length does not fit in u64"))?,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(invalid_json)?;
        let target = entry_path(&self.inner.root, &identity.key);
        let parent = target
            .parent()
            .ok_or_else(|| invalid_input("storm cache entry has no parent"))?;
        fs::create_dir_all(parent)?;
        ensure_real_directory(parent)?;

        let _guard = self.lock_mutation();
        match load_entry(&target, identity) {
            Ok(Some(existing)) => {
                if existing.canonical != entry.canonical || existing.geojson != entry.geojson {
                    return Err(invalid_data(
                        "one exact storm cache identity produced different output bytes",
                    ));
                }
                let mut health = self.lock_health();
                health.ready = true;
                health.last_error = None;
                return Ok(());
            }
            Ok(None) => {}
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                discard_directory(&target)?;
                let mut health = self.lock_health();
                health.recovered_invalid_entries =
                    health.recovered_invalid_entries.saturating_add(1);
            }
            Err(error) => return Err(error),
        }

        let stage = parent.join(format!(
            ".{}.stage-{}",
            identity.key,
            uuid::Uuid::new_v4().as_simple()
        ));
        let write_result = (|| -> io::Result<()> {
            fs::create_dir(&stage)?;
            write_synced(&stage.join(CANONICAL_FILE), &entry.canonical)?;
            write_synced(&stage.join(GEOJSON_FILE), &entry.geojson)?;
            // Manifest is written last inside the unreachable staging
            // directory, then the whole complete set becomes visible at once.
            write_synced(&stage.join(MANIFEST_FILE), &manifest_bytes)?;
            sync_directory(&stage)?;
            fs::rename(&stage, &target)?;
            sync_directory(parent)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = discard_directory(&stage);
            if let Some(existing) = load_entry(&target, identity)?
                && existing.canonical == entry.canonical
                && existing.geojson == entry.geojson
            {
                return Ok(());
            }
            return Err(error);
        }

        self.prune_locked()?;
        let (entries, bytes) = scan_totals(&self.inner.root)?;
        let mut health = self.lock_health();
        health.ready = true;
        health.entries = entries;
        health.bytes = bytes;
        health.atomic_store_writes = health.atomic_store_writes.saturating_add(1);
        health.last_store_unix_ms = Some(now_unix_ms());
        health.last_error = None;
        Ok(())
    }

    pub(crate) fn prune(&self) -> io::Result<()> {
        let _guard = self.lock_mutation();
        self.prune_locked()
    }

    fn prune_locked(&self) -> io::Result<()> {
        let StormCacheRetention::Bounded { frames_per_source } = self.inner.retention else {
            return Ok(());
        };
        let mut by_source: BTreeMap<String, Vec<(i64, PathBuf)>> = BTreeMap::new();
        for path in entry_directories(&self.inner.root)? {
            let Some(manifest) = read_manifest(&path)? else {
                continue;
            };
            let source_key = source_retention_key(&manifest.source)?;
            let valid_at = source_valid_at_unix_ms(&manifest.source);
            by_source
                .entry(source_key)
                .or_default()
                .push((valid_at, path));
        }
        for entries in by_source.values_mut() {
            entries.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
            for (_, path) in entries.iter().skip(frames_per_source) {
                discard_directory(path)?;
            }
        }
        let (entries, bytes) = scan_totals(&self.inner.root)?;
        let mut health = self.lock_health();
        health.entries = entries;
        health.bytes = bytes;
        Ok(())
    }

    fn record_error(&self, error: &io::Error) {
        let mut health = self.lock_health();
        health.ready = false;
        health.last_error_unix_ms = Some(now_unix_ms());
        health.last_error = Some(bound_error(error.to_string()));
    }

    fn lock_mutation(&self) -> MutexGuard<'_, ()> {
        self.inner
            .mutation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_health(&self) -> MutexGuard<'_, StormDiskCacheHealth> {
        self.inner
            .health
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    pub(crate) fn root_for_test(&self) -> &Path {
        &self.inner.root
    }
}

fn load_entry(path: &Path, identity: &StormCacheIdentity) -> io::Result<Option<CachedStormFrame>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data("storm cache entry is not a real directory"));
    }
    let manifest = read_manifest(path)?
        .ok_or_else(|| invalid_data("storm cache entry has no complete manifest"))?;
    if manifest.schema != MANIFEST_SCHEMA
        || manifest.cache_revision != STORM_CACHE_REVISION
        || manifest.cache_key != identity.key
        || manifest.model != identity.model
        || manifest.run != identity.run
        || manifest.snapshot_id != identity.snapshot_id
        || manifest.grid_hash != identity.grid_hash
        || manifest.storage_slot != identity.storage_slot
        || manifest.variable != identity.variable
        || manifest.source != identity.source
    {
        return Err(invalid_data(
            "storm cache manifest differs from the exact request identity",
        ));
    }
    let canonical = read_regular_file(&path.join(CANONICAL_FILE))?;
    let geojson = read_regular_file(&path.join(GEOJSON_FILE))?;
    if canonical.len() as u64 != manifest.canonical_bytes
        || geojson.len() as u64 != manifest.geojson_bytes
        || blake3::hash(&canonical).to_hex().as_str() != manifest.canonical_blake3
        || blake3::hash(&geojson).to_hex().as_str() != manifest.geojson_blake3
    {
        return Err(invalid_data("storm cache payload digest mismatch"));
    }
    let frame: StormCellFrame =
        serde_json::from_slice(&canonical).map_err(|error| invalid_data(error.to_string()))?;
    frame
        .validate()
        .map_err(|error| invalid_data(format!("cached storm frame is invalid: {error}")))?;
    if frame.source != manifest.source || frame.method != manifest.method {
        return Err(invalid_data(
            "cached storm frame source or method differs from its manifest",
        ));
    }
    validate_geojson_identity(&geojson, &frame)?;
    Ok(Some(CachedStormFrame {
        frame: Arc::new(frame),
        canonical: Bytes::from(canonical),
        geojson: Bytes::from(geojson),
    }))
}

fn validate_geojson_identity(bytes: &[u8], frame: &StormCellFrame) -> io::Result<()> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|error| invalid_data(error.to_string()))?;
    if value.get("type").and_then(serde_json::Value::as_str) != Some("FeatureCollection")
        || value.get("source") != serde_json::to_value(&frame.source).ok().as_ref()
        || value.get("method") != serde_json::to_value(&frame.method).ok().as_ref()
        || value
            .get("generated_at_unix_ms")
            .and_then(serde_json::Value::as_i64)
            != Some(frame.generated_at_unix_ms)
    {
        return Err(invalid_data(
            "cached GeoJSON identity differs from the canonical storm frame",
        ));
    }
    Ok(())
}

fn read_manifest(path: &Path) -> io::Result<Option<EntryManifest>> {
    let manifest_path = path.join(MANIFEST_FILE);
    let bytes = match read_regular_file(&manifest_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| invalid_data(format!("invalid storm cache manifest: {error}")))
}

fn read_regular_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid_data(format!(
            "storm cache payload is not a real file: {}",
            path.display()
        )));
    }
    fs::read(path)
}

fn recover_layout(root: &Path) -> io::Result<(u64, u64)> {
    let mut staging = 0_u64;
    let mut invalid = 0_u64;
    for shard in fs::read_dir(root)? {
        let shard = shard?;
        let shard_meta = fs::symlink_metadata(shard.path())?;
        if shard_meta.file_type().is_symlink() || !shard_meta.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.contains(".stage-") {
                discard_directory(&entry.path())?;
                staging = staging.saturating_add(1);
            } else if !valid_key(&name) {
                // Ignore operator-owned/unrecognized files. Only names in the
                // cache's exact internal namespace are ever removed.
                continue;
            } else {
                let metadata = fs::symlink_metadata(entry.path())?;
                let valid = if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    false
                } else {
                    match read_manifest(&entry.path()) {
                        Ok(Some(manifest)) => {
                            let identity = StormCacheIdentity {
                                key: manifest.cache_key.clone(),
                                model: manifest.model.clone(),
                                run: manifest.run.clone(),
                                snapshot_id: manifest.snapshot_id.clone(),
                                grid_hash: manifest.grid_hash.clone(),
                                storage_slot: manifest.storage_slot,
                                variable: manifest.variable.clone(),
                                source: manifest.source.clone(),
                            };
                            load_entry(&entry.path(), &identity).is_ok_and(|value| value.is_some())
                        }
                        Ok(None) | Err(_) => false,
                    }
                };
                if !valid {
                    discard_directory(&entry.path())?;
                    invalid = invalid.saturating_add(1);
                }
            }
        }
    }
    Ok((staging, invalid))
}

fn scan_totals(root: &Path) -> io::Result<(u64, u64)> {
    let mut count = 0_u64;
    let mut bytes = 0_u64;
    for path in entry_directories(root)? {
        count = count.saturating_add(1);
        bytes = bytes.saturating_add(directory_bytes(&path)?);
    }
    Ok((count, bytes))
}

fn entry_directories(root: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for shard in fs::read_dir(root)? {
        let shard = shard?;
        let shard_path = shard.path();
        let shard_meta = fs::symlink_metadata(&shard_path)?;
        if shard_meta.file_type().is_symlink() || !shard_meta.is_dir() {
            continue;
        }
        for entry in fs::read_dir(shard_path)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let metadata = fs::symlink_metadata(entry.path())?;
            if valid_key(&name) && metadata.is_dir() && !metadata.file_type().is_symlink() {
                paths.push(entry.path());
            }
        }
    }
    Ok(paths)
}

fn directory_bytes(path: &Path) -> io::Result<u64> {
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            bytes = bytes.saturating_add(metadata.len());
        }
    }
    Ok(bytes)
}

fn source_retention_key(source: &StormSource) -> io::Result<String> {
    let key = match source {
        StormSource::Mrms { product, .. } => format!("mrms:{product}"),
        StormSource::NexradLevel2 { site, moment, .. } => {
            format!("nexrad_level2:{site}:{moment}")
        }
    };
    Ok(key)
}

fn source_valid_at_unix_ms(source: &StormSource) -> i64 {
    match source {
        StormSource::Mrms {
            valid_at_unix_ms, ..
        } => *valid_at_unix_ms,
        StormSource::NexradLevel2 {
            volume_at_unix_ms, ..
        } => *volume_at_unix_ms,
    }
}

fn entry_path(root: &Path, key: &str) -> PathBuf {
    root.join(&key[..2]).join(key)
}

fn validate_key(key: &str) -> io::Result<()> {
    if valid_key(key) {
        Ok(())
    } else {
        Err(invalid_input(
            "storm cache key must be 64 lowercase hexadecimal digits",
        ))
    }
}

fn valid_key(key: &str) -> bool {
    key.len() == 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn discard_directory(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => Err(
            invalid_data("refusing to remove a non-directory cache entry"),
        ),
        Ok(_) => fs::remove_dir_all(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_input(format!(
            "storm cache path is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn bound_error(mut error: String) -> String {
    if error.len() > 2_048 {
        let mut end = 2_048;
        while !error.is_char_boundary(end) {
            end -= 1;
        }
        error.truncate(end);
    }
    error
}

fn invalid_input(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail.into())
}

fn invalid_data(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid_data(format!("failed to encode storm cache metadata: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rw_ops_protocol::{
        ContourRing, GeoPoint, STORM_CELL_FRAME_SCHEMA, StormCell, StormMethodKind,
    };

    fn fixture(valid_at_unix_ms: i64, seed: &[u8]) -> (StormCacheIdentity, CachedStormFrame) {
        let source = StormSource::Mrms {
            product: "ReflectivityAtLowestAltitude".into(),
            valid_at_unix_ms,
            grid_hash: "a".repeat(64),
        };
        let method = StormMethodIdentity {
            method_id: "rw-storm-deterministic".into(),
            method_version: "1".into(),
            kind: StormMethodKind::Deterministic,
            display_name: "Deterministic fixture".into(),
            description: "Synthetic cache fixture; no observational claims.".into(),
            upstream_product: None,
            model_id: None,
            model_version: None,
            parameters: BTreeMap::new(),
        };
        let frame = StormCellFrame {
            schema: STORM_CELL_FRAME_SCHEMA.into(),
            generated_at_unix_ms: 1_700_000_001_000,
            source: source.clone(),
            method: method.clone(),
            cells: vec![StormCell {
                cell_id: "fixture-cell".into(),
                track_id: None,
                centroid: GeoPoint {
                    latitude: 35.05,
                    longitude: -97.05,
                },
                rings: vec![ContourRing {
                    hole: false,
                    points: vec![
                        GeoPoint {
                            latitude: 35.0,
                            longitude: -97.1,
                        },
                        GeoPoint {
                            latitude: 35.1,
                            longitude: -97.1,
                        },
                        GeoPoint {
                            latitude: 35.1,
                            longitude: -97.0,
                        },
                        GeoPoint {
                            latitude: 35.0,
                            longitude: -97.1,
                        },
                    ],
                }],
                area_km2: 1.0,
                maximum_reflectivity_dbz: Some(50.0),
                echo_top_m: None,
                confidence: None,
                attributes: BTreeMap::new(),
            }],
            partial: false,
            warnings: Vec::new(),
        };
        frame.validate().unwrap();
        let canonical = serde_json::to_vec(&frame).unwrap();
        let geojson = serde_json::to_vec(&serde_json::json!({
            "type": "FeatureCollection",
            "schema": "rw.ops.storm-cell-geojson.v1",
            "generated_at_unix_ms": frame.generated_at_unix_ms,
            "source": source,
            "method": method,
            "partial": false,
            "warnings": [],
            "features": []
        }))
        .unwrap();
        let key = blake3::hash(seed).to_hex().to_string();
        (
            StormCacheIdentity {
                key,
                model: "obs-mrms".into(),
                run: "conus-reflectivity-20260823".into(),
                snapshot_id: "b".repeat(64),
                grid_hash: "a".repeat(64),
                storage_slot: 1,
                variable: "mrms_reflectivity_lowest_altitude".into(),
                source: frame.source.clone(),
            },
            CachedStormFrame {
                frame: Arc::new(frame),
                canonical: canonical.into(),
                geojson: geojson.into(),
            },
        )
    }

    #[test]
    fn restart_reuses_verified_canonical_and_geojson_pair() {
        let directory = tempfile::tempdir().unwrap();
        let (identity, frame) = fixture(1_700_000_000_000, b"restart-hit");
        let cache =
            StormFrameDiskCache::open(directory.path(), StormCacheRetention::Unlimited).unwrap();
        cache.store(&identity, &frame).unwrap();
        drop(cache);

        let reopened =
            StormFrameDiskCache::open(directory.path(), StormCacheRetention::Unlimited).unwrap();
        let loaded = reopened.load(&identity).unwrap().unwrap();
        assert_eq!(loaded.canonical, frame.canonical);
        assert_eq!(loaded.geojson, frame.geojson);
        assert_eq!(reopened.health().disk_hits, 1);
        assert_eq!(reopened.health().atomic_store_writes, 0);
    }

    #[test]
    fn restart_discards_incomplete_atomic_stage_and_corrupt_target() {
        let directory = tempfile::tempdir().unwrap();
        let (identity, frame) = fixture(1_700_000_000_000, b"atomic-recovery");
        let cache =
            StormFrameDiskCache::open(directory.path(), StormCacheRetention::Unlimited).unwrap();
        cache.store(&identity, &frame).unwrap();
        let target = entry_path(cache.root_for_test(), &identity.key);
        fs::write(target.join(CANONICAL_FILE), b"truncated").unwrap();
        let stage = target
            .parent()
            .unwrap()
            .join(format!(".{}.stage-interrupted", identity.key));
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join(CANONICAL_FILE), b"partial").unwrap();
        drop(cache);

        let recovered =
            StormFrameDiskCache::open(directory.path(), StormCacheRetention::Unlimited).unwrap();
        let health = recovered.health();
        assert_eq!(health.recovered_staging_entries, 1);
        assert_eq!(health.recovered_invalid_entries, 1);
        assert!(recovered.load(&identity).unwrap().is_none());
        assert!(!stage.exists());
        assert!(!target.exists());
    }

    #[test]
    fn changed_source_identity_cannot_hit_an_older_frame() {
        let directory = tempfile::tempdir().unwrap();
        let (old_identity, frame) = fixture(1_700_000_000_000, b"old-source");
        let cache =
            StormFrameDiskCache::open(directory.path(), StormCacheRetention::Unlimited).unwrap();
        cache.store(&old_identity, &frame).unwrap();

        let (new_identity, _) = fixture(1_700_000_300_000, b"new-source");
        assert!(cache.load(&new_identity).unwrap().is_none());
        assert!(cache.load(&old_identity).unwrap().is_some());
    }
}

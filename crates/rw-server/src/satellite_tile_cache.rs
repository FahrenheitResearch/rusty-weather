//! Restart-reusable native satellite tile storage.
//!
//! Entries are exact-identity, self-validating envelopes installed with one
//! same-directory rename. A request can therefore observe either no entry or
//! one complete entry; `.tmp` files are never addressable as cache hits.

use std::collections::{BTreeSet, HashMap};
use std::fs::{self, FileTimes, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::state::{CachedSatelliteTile, SatelliteTileCacheKey};

const CACHE_DIRECTORY: &str = ".rw-satellite-tile-cache";
const CACHE_LAYOUT: &str = "v1";
const ENTRY_EXTENSION: &str = "rwtile";
const ENTRY_MAGIC: &[u8; 8] = b"RWSATTL1";
const ENTRY_SCHEMA: &str = "rw-server.satellite-tile-cache.v1";
const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const STALE_TEMP_AGE: Duration = Duration::from_secs(60 * 60);

#[derive(Clone, Debug)]
pub(crate) struct SatelliteTileDiskCache {
    inner: Arc<DiskCacheInner>,
}

#[derive(Debug)]
struct DiskCacheInner {
    root: PathBuf,
    maximum_bytes: u64,
    index: Mutex<CacheIndex>,
}

#[derive(Debug, Default)]
struct CacheIndex {
    total_bytes: u64,
    entries: HashMap<PathBuf, IndexedEntry>,
    least_recent: BTreeSet<(SystemTime, PathBuf)>,
}

#[derive(Clone, Copy, Debug)]
struct IndexedEntry {
    bytes: u64,
    last_used: SystemTime,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TileEntryHeader {
    schema: String,
    key: SatelliteTileCacheKey,
    key_blake3: String,
    png_blake3: String,
    png_bytes: u64,
    frame_id: String,
    valid_unix: i64,
}

impl SatelliteTileDiskCache {
    pub(crate) fn open(cache_root: &Path, maximum_bytes: u64) -> io::Result<Self> {
        if maximum_bytes == 0 {
            return Err(invalid_input(
                "satellite tile disk-cache capacity must be greater than zero",
            ));
        }
        let root = cache_root.join(CACHE_DIRECTORY).join(CACHE_LAYOUT);
        fs::create_dir_all(&root)?;
        ensure_real_directory(&root)?;
        let index = scan_index(&root)?;
        let cache = Self {
            inner: Arc::new(DiskCacheInner {
                root,
                maximum_bytes,
                index: Mutex::new(index),
            }),
        };
        cache.prune_to(maximum_bytes)?;
        Ok(cache)
    }

    /// Load and validate one exact immutable entry. Corrupt entries are
    /// removed and reported as misses; they are never returned to the caller.
    pub(crate) fn load(
        &self,
        key: &SatelliteTileCacheKey,
    ) -> io::Result<Option<Arc<CachedSatelliteTile>>> {
        let digest = key_digest(key);
        let path = entry_path(&self.inner.root, &digest);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.lock_index().remove(&path);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            self.lock_index().remove(&path);
            return Ok(None);
        }
        if metadata.len() > MAX_ENTRY_BYTES || metadata.len() < minimum_entry_bytes() {
            self.discard(&path)?;
            return Ok(None);
        }
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.lock_index().remove(&path);
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let Some(tile) = decode_entry(bytes, key, &digest) else {
            self.discard(&path)?;
            return Ok(None);
        };

        let now = SystemTime::now();
        if let Ok(file) = OpenOptions::new().read(true).write(true).open(&path) {
            let _ = file.set_times(FileTimes::new().set_modified(now));
        }
        // A concurrent prune may have removed the file after it was read.
        // The already-validated bytes remain safe for this response, but only
        // index an entry that still exists with the expected envelope length.
        if fs::symlink_metadata(&path).is_ok_and(|current| {
            current.file_type().is_file()
                && !current.file_type().is_symlink()
                && current.len() == metadata.len()
        }) {
            self.lock_index().upsert(path, metadata.len(), now);
        }
        Ok(Some(Arc::new(tile)))
    }

    /// The prewarm path can use this as an exact completeness check. It reads
    /// and hashes the envelope rather than trusting a filename's existence.
    #[allow(dead_code)] // Consumed by the follow-up bounded prewarm worker.
    pub(crate) fn contains_valid(&self, key: &SatelliteTileCacheKey) -> io::Result<bool> {
        self.load(key).map(|entry| entry.is_some())
    }

    /// Atomically retain one rendered tile. An entry larger than the configured
    /// total capacity is served from memory but intentionally not persisted.
    pub(crate) fn store(
        &self,
        key: &SatelliteTileCacheKey,
        tile: &CachedSatelliteTile,
    ) -> io::Result<bool> {
        if tile.frame_id != key.frame {
            return Err(invalid_input(
                "satellite tile frame does not match its exact cache identity",
            ));
        }
        if tile.source_revision != key.source_revision {
            return Err(invalid_input(
                "satellite tile source revision does not match its exact cache identity",
            ));
        }
        if !tile.png.starts_with(PNG_SIGNATURE) {
            return Err(invalid_input(
                "satellite tile cache accepts only complete PNG payloads",
            ));
        }

        let key_blake3 = key_digest(key);
        let png_blake3 = blake3::hash(&tile.png).to_hex().to_string();
        let expected_etag = format!("\"{png_blake3}\"");
        if tile.etag != expected_etag {
            return Err(invalid_input(
                "satellite tile ETag does not match its PNG payload",
            ));
        }
        let header = TileEntryHeader {
            schema: ENTRY_SCHEMA.to_string(),
            key: key.clone(),
            key_blake3: key_blake3.clone(),
            png_blake3,
            png_bytes: u64::try_from(tile.png.len())
                .map_err(|_| invalid_input("satellite tile PNG length does not fit in u64"))?,
            frame_id: tile.frame_id.clone(),
            valid_unix: tile.valid_unix,
        };
        let header = serde_json::to_vec(&header).map_err(invalid_json)?;
        if header.len() > MAX_HEADER_BYTES {
            return Err(invalid_input(
                "satellite tile cache header exceeds its format bound",
            ));
        }
        let entry_bytes = entry_size(header.len(), tile.png.len())?;
        if entry_bytes > self.inner.maximum_bytes || entry_bytes > MAX_ENTRY_BYTES {
            return Ok(false);
        }

        let target = entry_path(&self.inner.root, &key_blake3);
        let parent = target
            .parent()
            .ok_or_else(|| invalid_input("satellite tile cache path has no parent"))?;
        fs::create_dir_all(parent)?;
        ensure_real_directory(parent)?;

        if let Some(existing) = self.load(key)? {
            if existing.etag != tile.etag {
                return Err(invalid_data(
                    "one exact satellite tile identity produced different PNG bytes",
                ));
            }
            return Ok(true);
        }

        // Pruning holds only the compact index lock. Different exact tiles can
        // still read, encode, fsync, and rename in parallel.
        self.prune_to(self.inner.maximum_bytes.saturating_sub(entry_bytes))?;

        let temporary = parent.join(format!(
            ".{key_blake3}.{}.tmp",
            uuid::Uuid::new_v4().as_simple()
        ));
        let write_result = (|| -> io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(ENTRY_MAGIC)?;
            file.write_all(
                &u32::try_from(header.len())
                    .map_err(|_| invalid_input("satellite tile header length does not fit u32"))?
                    .to_le_bytes(),
            )?;
            file.write_all(&header)?;
            file.write_all(&tile.png)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, &target)?;
            sync_parent_directory(parent)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary);
            if target.exists()
                && let Some(existing) = self.load(key)?
                && existing.etag == tile.etag
            {
                return Ok(true);
            }
            return Err(error);
        }

        {
            let mut index = self.lock_index();
            index.upsert(target, entry_bytes, SystemTime::now());
            self.prune_locked(&mut index, self.inner.maximum_bytes)?;
        }
        Ok(true)
    }

    fn discard(&self, path: &Path) -> io::Result<()> {
        self.lock_index().remove(path);
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn prune_to(&self, bytes: u64) -> io::Result<()> {
        let mut index = self.lock_index();
        self.prune_locked(&mut index, bytes)
    }

    fn prune_locked(&self, index: &mut CacheIndex, bytes: u64) -> io::Result<()> {
        while index.total_bytes > bytes {
            let Some((_, path)) = index.least_recent.iter().next().cloned() else {
                index.total_bytes = 0;
                break;
            };
            match fs::remove_file(&path) {
                Ok(()) => {
                    index.remove(&path);
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    index.remove(&path);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn lock_index(&self) -> MutexGuard<'_, CacheIndex> {
        self.inner
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[cfg(test)]
    fn entry_path_for_test(&self, key: &SatelliteTileCacheKey) -> PathBuf {
        entry_path(&self.inner.root, &key_digest(key))
    }

    #[cfg(test)]
    fn total_bytes_for_test(&self) -> u64 {
        self.lock_index().total_bytes
    }
}

impl CacheIndex {
    fn upsert(&mut self, path: PathBuf, bytes: u64, last_used: SystemTime) {
        self.remove(&path);
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.entries
            .insert(path.clone(), IndexedEntry { bytes, last_used });
        self.least_recent.insert((last_used, path));
    }

    fn remove(&mut self, path: &Path) {
        if let Some(previous) = self.entries.remove(path) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.bytes);
            self.least_recent
                .remove(&(previous.last_used, path.to_path_buf()));
        }
    }
}

fn scan_index(root: &Path) -> io::Result<CacheIndex> {
    let mut index = CacheIndex::default();
    for shard in fs::read_dir(root)? {
        let shard = shard?;
        let file_type = shard.file_type()?;
        if !file_type.is_dir()
            || file_type.is_symlink()
            || !valid_shard_name(&shard.file_name().to_string_lossy())
        {
            continue;
        }
        for entry in fs::read_dir(shard.path())? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if !file_type.is_file() || file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let metadata = entry.metadata()?;
            if valid_entry_name(&name) {
                let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
                index.upsert(path, metadata.len(), modified);
            } else if name.ends_with(".tmp") && stale(&metadata) {
                let _ = fs::remove_file(path);
            }
        }
    }
    Ok(index)
}

fn stale(metadata: &fs::Metadata) -> bool {
    metadata
        .modified()
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= STALE_TEMP_AGE)
}

fn decode_entry(
    bytes: Vec<u8>,
    expected_key: &SatelliteTileCacheKey,
    expected_key_blake3: &str,
) -> Option<CachedSatelliteTile> {
    if bytes.len() < minimum_entry_bytes_usize() || bytes.get(..ENTRY_MAGIC.len())? != ENTRY_MAGIC {
        return None;
    }
    let length_offset = ENTRY_MAGIC.len();
    let header_bytes = bytes.get(length_offset..length_offset + 4)?;
    let header_length = usize::try_from(u32::from_le_bytes(header_bytes.try_into().ok()?)).ok()?;
    if header_length == 0 || header_length > MAX_HEADER_BYTES {
        return None;
    }
    let header_start = length_offset + 4;
    let header_end = header_start.checked_add(header_length)?;
    let header: TileEntryHeader =
        serde_json::from_slice(bytes.get(header_start..header_end)?).ok()?;
    let png = bytes.get(header_end..)?;
    if header.schema != ENTRY_SCHEMA
        || header.key != *expected_key
        || header.key_blake3 != expected_key_blake3
        || header.frame_id != expected_key.frame
        || header.png_bytes != u64::try_from(png.len()).ok()?
        || !png.starts_with(PNG_SIGNATURE)
    {
        return None;
    }
    let png_blake3 = blake3::hash(png).to_hex().to_string();
    if header.png_blake3 != png_blake3 {
        return None;
    }
    let bytes = Bytes::from(bytes);
    let png = bytes.slice(header_end..);
    Some(CachedSatelliteTile {
        png,
        etag: format!("\"{png_blake3}\""),
        frame_id: header.frame_id,
        source_revision: header.key.source_revision,
        valid_unix: header.valid_unix,
    })
}

fn key_digest(key: &SatelliteTileCacheKey) -> String {
    let mut hash = blake3::Hasher::new();
    hash.update(b"rw-server:satellite-tile-cache-key:v1\0");
    update_string(&mut hash, &key.recipe);
    update_string(&mut hash, &key.source_revision);
    update_string(&mut hash, &key.platform);
    update_string(&mut hash, &key.sector);
    update_string(&mut hash, &key.product);
    update_string(&mut hash, &key.frame);
    hash.update(&[key.zoom]);
    hash.update(&key.x.to_le_bytes());
    hash.update(&key.y.to_le_bytes());
    hash.update(&key.tile_size.to_le_bytes());
    hash.finalize().to_hex().to_string()
}

fn update_string(hash: &mut blake3::Hasher, value: &str) {
    hash.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hash.update(value.as_bytes());
}

fn entry_path(root: &Path, digest: &str) -> PathBuf {
    root.join(&digest[..2])
        .join(format!("{digest}.{ENTRY_EXTENSION}"))
}

fn entry_size(header_bytes: usize, png_bytes: usize) -> io::Result<u64> {
    u64::try_from(
        ENTRY_MAGIC
            .len()
            .checked_add(4)
            .and_then(|value| value.checked_add(header_bytes))
            .and_then(|value| value.checked_add(png_bytes))
            .ok_or_else(|| invalid_input("satellite tile cache entry length overflow"))?,
    )
    .map_err(|_| invalid_input("satellite tile cache entry length does not fit u64"))
}

const fn minimum_entry_bytes() -> u64 {
    (ENTRY_MAGIC.len() + 4 + 1 + PNG_SIGNATURE.len()) as u64
}

const fn minimum_entry_bytes_usize() -> usize {
    ENTRY_MAGIC.len() + 4 + 1 + PNG_SIGNATURE.len()
}

fn ensure_real_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_input(format!(
            "satellite tile cache path is not a real directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn valid_shard_name(name: &str) -> bool {
    name.len() == 2
        && name
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_entry_name(name: &str) -> bool {
    let Some(digest) = name.strip_suffix(&format!(".{ENTRY_EXTENSION}")) else {
        return false;
    };
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn invalid_input(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, detail.into())
}

fn invalid_data(detail: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, detail.into())
}

fn invalid_json(error: serde_json::Error) -> io::Error {
    invalid_data(format!(
        "failed to encode satellite tile cache header: {error}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60,
        0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xa5, 0xf6, 0x45, 0x40, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn exact_entry_survives_reopen_and_corruption_is_never_served() {
        let directory = tempfile::tempdir().unwrap();
        let key = fixture_key("20260822T1951", 19);
        let tile = fixture_tile(&key.frame);

        let cache = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        assert!(cache.store(&key, &tile).unwrap());
        let entry_path = cache.entry_path_for_test(&key);
        drop(cache);

        let reopened = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        let loaded = reopened.load(&key).unwrap().unwrap();
        assert_eq!(loaded.png.as_ref(), TINY_PNG);
        assert_eq!(loaded.etag, tile.etag);

        fs::write(&entry_path, b"partial final entry").unwrap();
        assert!(reopened.load(&key).unwrap().is_none());
        assert!(!entry_path.exists());
    }

    #[test]
    fn key_identity_separates_every_pixel_input_and_prunes_oldest_safely() {
        let directory = tempfile::tempdir().unwrap();
        let first = fixture_key("20260822T1941", 19);
        let second = fixture_key("20260822T1951", 20);
        let large = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        large.store(&first, &fixture_tile(&first.frame)).unwrap();
        large.store(&second, &fixture_tile(&second.frame)).unwrap();
        let first_path = large.entry_path_for_test(&first);
        let second_path = large.entry_path_for_test(&second);
        let first_bytes = fs::metadata(&first_path).unwrap().len();
        let second_bytes = fs::metadata(&second_path).unwrap().len();
        assert_ne!(first_path, second_path);
        drop(large);

        let first_file = OpenOptions::new().write(true).open(&first_path).unwrap();
        first_file
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
            .unwrap();
        let second_file = OpenOptions::new().write(true).open(&second_path).unwrap();
        second_file
            .set_times(FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(2)))
            .unwrap();

        let bounded = SatelliteTileDiskCache::open(directory.path(), second_bytes).unwrap();
        assert!(bounded.total_bytes_for_test() <= second_bytes);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        assert!(first_bytes > 0);
        assert!(!bounded.contains_valid(&first).unwrap());
        assert!(bounded.contains_valid(&second).unwrap());
    }

    fn fixture_key(frame: &str, x: u32) -> SatelliteTileCacheKey {
        SatelliteTileCacheKey {
            recipe: "rw-sat-native-v2".into(),
            source_revision: "1".repeat(64),
            platform: "g18".into(),
            sector: "fulldisk".into(),
            product: "open_geocolor_v1".into(),
            frame: frame.into(),
            zoom: 7,
            x,
            y: 41,
            tile_size: 256,
        }
    }

    fn fixture_tile(frame: &str) -> CachedSatelliteTile {
        let png = Bytes::from_static(TINY_PNG);
        CachedSatelliteTile {
            etag: format!("\"{}\"", blake3::hash(&png).to_hex()),
            png,
            frame_id: frame.into(),
            source_revision: "1".repeat(64),
            valid_unix: 1_777_000_000,
        }
    }
}

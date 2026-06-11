//! Writer side of the `.rwl` flash store: [`BucketWriter`] accumulates flashes
//! across granules, sorts them, and rewrites the affected 10-minute bucket
//! files atomically while holding the per-satellite writer lock. It also
//! maintains the `window.json` manifest.
//!
//! ## Layout
//!
//! ```text
//! <root>/glm/<satellite>/window.json
//! <root>/glm/<satellite>/<YYYYMMDD>/tHHMM.rwl
//! ```
//!
//! The date-dir level (`<YYYYMMDD>`) is *not* in the original spec, which wrote
//! `<root>/glm/<satellite>/tHHMM.rwl`. It was added during the build because a
//! flat `tHHMM` namespace collides across days whenever the rolling window
//! straddles UTC midnight (a >24 h window, or any window crossing 00:00 with
//! activity in the same `tHHMM` slot on two days) and because per-day
//! directories make rolling-window pruning a cheap directory drop. The spec
//! doc carries an amendment noting this change.
//!
//! ## Concurrency
//!
//! Exactly one writer per satellite store at a time. The lock is the
//! satellite-directory `.rw-lock` taken via [`rw_store::lock::RunLock`] — the
//! same advisory-lock contract as rw-store (auto-released on process exit,
//! never deleted on drop, readers never take it). [`BucketWriter::open`]
//! acquires the lock in its constructor and the guard lives for the writer's
//! whole lifetime, so the lock is held from open until the `BucketWriter` is
//! dropped.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rw_store::RunLock;
use rw_store::atomic::atomic_write_bytes;
use serde::{Deserialize, Serialize};

use crate::error::RwlResult;
use crate::format::{self, FlashRecord, HEADER_LEN, RECORD_LEN, RwlHeader};

/// Default time to wait for the satellite-store lock before giving up.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

/// Schema string for `window.json`.
pub const WINDOW_SCHEMA: &str = "rw-glm.window.v1";

/// The `window.json` manifest sitting at `<root>/glm/<satellite>/window.json`.
/// v1 records only enough to make the store self-describing; the follow engine
/// (Task 3) will extend it with seen-granule keys and prune bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WindowManifest {
    /// Exact schema string; readers reject a mismatch.
    pub schema: String,
    /// Satellite id, e.g. `"goes19"`.
    pub satellite: String,
    /// Earliest flash time currently stored, Unix ms (None if empty).
    #[serde(default)]
    pub time_min_unix_ms: Option<i64>,
    /// Latest flash time currently stored, Unix ms (None if empty).
    #[serde(default)]
    pub time_max_unix_ms: Option<i64>,
}

impl WindowManifest {
    fn new(satellite: &str) -> Self {
        Self {
            schema: WINDOW_SCHEMA.to_string(),
            satellite: satellite.to_string(),
            time_min_unix_ms: None,
            time_max_unix_ms: None,
        }
    }
}

/// A writer for one satellite's flash store. Holds the satellite-directory
/// writer lock for its whole lifetime.
#[derive(Debug)]
pub struct BucketWriter {
    sat_dir: PathBuf,
    satellite: String,
    /// Held for the writer's lifetime; released on drop. Never read directly —
    /// its existence *is* the single-writer guarantee.
    _lock: RunLock,
}

impl BucketWriter {
    /// Open (creating directories as needed) the store for `satellite` under
    /// `root` and acquire the per-satellite writer lock.
    ///
    /// Blocks up to [`DEFAULT_LOCK_TIMEOUT`] for the lock; returns
    /// [`crate::RwlError::Locked`] on timeout. The satellite directory is
    /// created before locking because `RunLock` needs an existing directory to
    /// open `.rw-lock` in.
    pub fn open(root: &Path, satellite: &str) -> RwlResult<Self> {
        Self::open_with_timeout(root, satellite, DEFAULT_LOCK_TIMEOUT)
    }

    /// [`open`](Self::open) with an explicit lock-acquisition timeout.
    pub fn open_with_timeout(root: &Path, satellite: &str, timeout: Duration) -> RwlResult<Self> {
        let sat_dir = root.join("glm").join(satellite);
        std::fs::create_dir_all(&sat_dir)?;
        let lock = RunLock::acquire(&sat_dir, timeout)?;
        Ok(Self {
            sat_dir,
            satellite: satellite.to_string(),
            _lock: lock,
        })
    }

    /// Insert `flashes` (in any granule order) into the store. Flashes are
    /// grouped by their destination bucket; each affected bucket is merged with
    /// whatever it already holds, re-sorted ascending by time (stable on ties),
    /// and rewritten atomically. The `window.json` manifest is refreshed from
    /// the resulting on-disk extent.
    ///
    /// `source_granule_count` is the number of granules these flashes came from
    /// in this call; it is *added* to each touched bucket's header count so the
    /// header records provenance breadth. (Restart-safe granule-key dedup lives
    /// in the follow engine, Task 3 — this is just the count field.)
    pub fn insert_flashes(
        &mut self,
        flashes: &[FlashRecord],
        source_granule_count: u32,
    ) -> RwlResult<()> {
        if flashes.is_empty() {
            return Ok(());
        }

        // Group incoming flashes by (date_dir, bucket_name) key, preserving
        // arrival order within a group so the later stable sort keeps ties
        // ordered by insertion.
        let mut by_bucket: BTreeMap<(String, String), Vec<FlashRecord>> = BTreeMap::new();
        for f in flashes {
            let key = (
                format::date_dir(f.time_unix_ms),
                format::bucket_name(f.time_unix_ms),
            );
            by_bucket.entry(key).or_default().push(*f);
        }

        for ((date, name), incoming) in by_bucket {
            let path = self.sat_dir.join(&date).join(&name);
            self.rewrite_bucket(&path, incoming, source_granule_count)?;
        }

        self.refresh_window()?;
        Ok(())
    }

    /// Merge `incoming` into the bucket at `path` (reading any existing
    /// records), sort, and atomically rewrite.
    fn rewrite_bucket(
        &self,
        path: &Path,
        incoming: Vec<FlashRecord>,
        added_granules: u32,
    ) -> RwlResult<()> {
        let mut existing = read_bucket_records(path)?;
        let prior_granules = read_bucket_granule_count(path)?;

        existing.extend(incoming);
        // Stable sort by time so ties keep their relative order (existing
        // records first, then this call's arrivals).
        existing.sort_by_key(|r| r.time_unix_ms);

        let bytes = pack_bucket(&existing, prior_granules.saturating_add(added_granules));
        atomic_write_bytes(path, &bytes)?;
        Ok(())
    }

    /// Recompute `window.json` from the min/max flash time across all buckets
    /// on disk and write it atomically.
    fn refresh_window(&self) -> RwlResult<()> {
        let (mut min, mut max): (Option<i64>, Option<i64>) = (None, None);
        for path in self.bucket_files()? {
            let data = match std::fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            if let Ok(h) = RwlHeader::parse(&data) {
                if h.record_count > 0 {
                    min = Some(min.map_or(h.time_min_unix_ms, |m: i64| m.min(h.time_min_unix_ms)));
                    max = Some(max.map_or(h.time_max_unix_ms, |m: i64| m.max(h.time_max_unix_ms)));
                }
            }
        }
        let mut manifest = WindowManifest::new(&self.satellite);
        manifest.time_min_unix_ms = min;
        manifest.time_max_unix_ms = max;
        let json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| crate::RwlError::Format(format!("window.json serialize: {e}")))?;
        atomic_write_bytes(&self.sat_dir.join("window.json"), &json)?;
        Ok(())
    }

    /// Enumerate every `tHHMM.rwl` file under this satellite's date dirs.
    fn bucket_files(&self) -> RwlResult<Vec<PathBuf>> {
        let mut out = Vec::new();
        let day_dirs = match std::fs::read_dir(&self.sat_dir) {
            Ok(it) => it,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(e.into()),
        };
        for day in day_dirs.flatten() {
            let day_path = day.path();
            if !day_path.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&day_path)?.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("rwl") {
                    out.push(p);
                }
            }
        }
        Ok(out)
    }
}

/// Pack a sorted slice of records into a complete `.rwl` file image (header +
/// records). The caller guarantees `records` is sorted ascending by time.
pub fn pack_bucket(records: &[FlashRecord], source_granule_count: u32) -> Vec<u8> {
    let (time_min, time_max) = match (records.first(), records.last()) {
        (Some(first), Some(last)) => (first.time_unix_ms, last.time_unix_ms),
        _ => (0, 0),
    };
    let header = RwlHeader {
        version: format::VERSION,
        record_count: records.len() as u32,
        time_min_unix_ms: time_min,
        time_max_unix_ms: time_max,
        source_granule_count,
    };
    let mut buf = Vec::with_capacity(HEADER_LEN + records.len() * RECORD_LEN);
    header.pack_into(&mut buf);
    for r in records {
        r.pack_into(&mut buf);
    }
    buf
}

/// Read the records out of an existing bucket file, or `vec![]` if it does not
/// exist. Returns an error for a structurally broken file (so we never blindly
/// overwrite a corrupt bucket having lost its contents to a parse slip).
fn read_bucket_records(path: &Path) -> RwlResult<Vec<FlashRecord>> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let header = RwlHeader::parse(&data)?;
    let count = header.record_count as usize;
    let expected = HEADER_LEN + count * RECORD_LEN;
    if data.len() != expected {
        return Err(crate::RwlError::Format(format!(
            "{}: size {} != expected {}",
            path.display(),
            data.len(),
            expected
        )));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let start = HEADER_LEN + i * RECORD_LEN;
        out.push(FlashRecord::unpack(&data[start..start + RECORD_LEN])?);
    }
    Ok(out)
}

/// Read just the `source_granule_count` from an existing bucket header (0 if
/// the file does not exist or cannot be parsed).
fn read_bucket_granule_count(path: &Path) -> RwlResult<u32> {
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    match RwlHeader::parse(&data) {
        Ok(h) => Ok(h.source_granule_count),
        Err(_) => Ok(0),
    }
}

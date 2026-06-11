//! Rolling time-window management for the `.rwl` flash store: cap a
//! satellite's footprint by bucket age and total bytes, deleting the oldest
//! bucket files and dropping emptied date directories — the point-event analog
//! of `rw-sat`'s `window.rs`.
//!
//! ## Lock contract (mirrors rw-sat, adapted to the per-satellite lock)
//!
//! In rw-sat the writer lock is per *run dir* and the pruner walks many run
//! dirs, locking each in turn. Here the lock is the single **satellite-dir**
//! `.rw-lock` (the same one [`crate::store::BucketWriter`] holds for its
//! lifetime), and a satellite has many *date sub-directories* under it. So the
//! pruner takes the one satellite lock with [`RunLock::try_acquire`]; if the
//! engine's own writer (or any other process) still holds it the whole pass is
//! skipped and recorded in [`PruneReport::skipped_locked`], to be retried next
//! cycle.
//!
//! **Lock lifetimes never overlap with the writer.** The follow loop is single
//! threaded: it opens the `BucketWriter` (acquiring the lock), writes the
//! granule, drops the writer (releasing the lock), and only *then* calls
//! [`enforce_window`], which re-acquires the same lock. Ingest-then-enforce,
//! sequentially, exactly like rw-sat — the two lock holders never coexist.
//!
//! Because the lock file lives at the satellite-dir level (which a prune never
//! deletes — only its date sub-dirs), the teardown is simpler than rw-sat's:
//! all mutation (bucket-file deletes, then empty date-dir `remove_dir`s) happens
//! *while the satellite lock is held*, and the `.rw-lock` itself is left in
//! place. We still follow rw-sat's reviewed-safe ordering — content first, then
//! the now-empty directory — and tolerate a still-non-empty date dir (a racing
//! writer wrote a fresh bucket) by leaving it for the next cycle.

use std::fs;
use std::path::{Path, PathBuf};

use rw_store::RunLock;

use crate::error::RwlResult;
use crate::format::{self, RwlHeader};

/// Rolling-window retention policy. `None` disables that limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowConfig {
    /// Evict buckets whose newest flash is older than `now - max_age_ms`.
    pub max_age_ms: Option<i64>,
    /// After the age sweep, evict the oldest buckets until the total on-disk
    /// byte size fits this budget.
    pub max_bytes: Option<u64>,
}

impl WindowConfig {
    pub fn is_unbounded(&self) -> bool {
        self.max_age_ms.is_none() && self.max_bytes.is_none()
    }
}

/// What one [`enforce_window`] pass removed. Field names mirror rw-sat's
/// `EvictionReport` (`removed_*`, `skipped_locked`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneReport {
    pub removed_buckets: usize,
    pub removed_bytes: u64,
    /// Date directories left empty and removed, sorted.
    pub removed_date_dirs: Vec<String>,
    /// Set when the satellite-dir lock was held by another writer and the whole
    /// pass was skipped; the date-dir name(s) that *would* have been pruned are
    /// not enumerated (we never read under the held lock) — this carries the
    /// satellite name so the engine can log/retry. Mirrors rw-sat's
    /// `skipped_locked` semantics (skip-if-locked, retry next cycle).
    pub skipped_locked: Vec<String>,
}

/// One bucket file's identity for the eviction decision.
#[derive(Debug)]
struct BucketRef {
    date_dir: String,
    path: PathBuf,
    /// Newest flash time in the bucket (header `time_max`), the age key.
    newest_ms: i64,
    bytes: u64,
}

/// Enforce the rolling window over one satellite store under `root`. `now_ms`
/// is supplied by the caller (Unix ms) so tests stay deterministic.
///
/// Acquires the satellite-dir lock without blocking; if another writer holds it
/// the pass is skipped (`report.skipped_locked = [satellite]`) and retried next
/// cycle. A missing store, or an unbounded config, is a clean no-op.
pub fn enforce_window(
    root: &Path,
    satellite: &str,
    now_ms: i64,
    config: &WindowConfig,
) -> RwlResult<PruneReport> {
    let mut report = PruneReport::default();
    if config.is_unbounded() {
        return Ok(report);
    }
    let sat_dir = root.join("glm").join(satellite);
    if !sat_dir.is_dir() {
        return Ok(report);
    }

    // Take the satellite lock without blocking. The engine's own writer is
    // already dropped by the time we get here (ingest-then-enforce), so this
    // normally succeeds; a held lock means a competing writer — skip this pass.
    let _lock = match RunLock::try_acquire(&sat_dir)? {
        Some(lock) => lock,
        None => {
            report.skipped_locked.push(satellite.to_string());
            return Ok(report);
        }
    };

    // Inventory every bucket file across the date sub-dirs.
    let mut buckets = inventory_buckets(&sat_dir)?;
    buckets.sort_by_key(|b| b.newest_ms);

    // Decide the evict set: everything past max_age, then oldest-first until the
    // remainder fits max_bytes.
    let mut evict = vec![false; buckets.len()];
    if let Some(max_age_ms) = config.max_age_ms {
        let cutoff = now_ms - max_age_ms;
        for (index, bucket) in buckets.iter().enumerate() {
            if bucket.newest_ms < cutoff {
                evict[index] = true;
            }
        }
    }
    if let Some(max_bytes) = config.max_bytes {
        let mut kept_bytes: u64 = buckets
            .iter()
            .enumerate()
            .filter(|(index, _)| !evict[*index])
            .map(|(_, b)| b.bytes)
            .sum();
        for (index, bucket) in buckets.iter().enumerate() {
            if kept_bytes <= max_bytes {
                break;
            }
            if !evict[index] {
                evict[index] = true;
                kept_bytes -= bucket.bytes;
            }
        }
    }

    // Apply: delete the evicted bucket files (all under the held satellite
    // lock), then drop any date dir left empty. Content first, directory after
    // — rw-sat's reviewed-safe ordering.
    for (index, bucket) in buckets.iter().enumerate() {
        if !evict[index] {
            continue;
        }
        if bucket.path.is_file() {
            fs::remove_file(&bucket.path)?;
            report.removed_buckets += 1;
            report.removed_bytes += bucket.bytes;
        }
    }

    // Sweep date dirs that may now be empty. `remove_dir` refuses a non-empty
    // dir (a racing writer's fresh bucket), so it is self-guarding — leave such
    // a dir for the next cycle. We only consider date dirs we actually touched.
    let mut touched: Vec<String> = buckets
        .iter()
        .enumerate()
        .filter(|(index, _)| evict[*index])
        .map(|(_, b)| b.date_dir.clone())
        .collect();
    touched.sort();
    touched.dedup();
    for date in touched {
        let date_path = sat_dir.join(&date);
        if dir_is_empty(&date_path) && fs::remove_dir(&date_path).is_ok() {
            report.removed_date_dirs.push(date);
        }
    }
    report.removed_date_dirs.sort();
    Ok(report)
}

/// Inventory every `tHHMM.rwl` bucket under a satellite's date sub-dirs. Each
/// bucket's age key is its header `time_max` (the newest flash it holds); a
/// file that cannot be parsed is skipped (the validator handles diagnosis — the
/// pruner must never abort the poll loop on one bad file).
fn inventory_buckets(sat_dir: &Path) -> RwlResult<Vec<BucketRef>> {
    let mut out = Vec::new();
    let day_dirs = match fs::read_dir(sat_dir) {
        Ok(it) => it,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for day in day_dirs.flatten() {
        let day_path = day.path();
        if !day_path.is_dir() {
            continue;
        }
        let date_dir = day.file_name().to_string_lossy().into_owned();
        for f in fs::read_dir(&day_path)?.flatten() {
            let path = f.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rwl") {
                continue;
            }
            let data = match fs::read(&path) {
                Ok(d) => d,
                Err(_) => continue,
            };
            let Ok(header) = RwlHeader::parse(&data) else {
                continue;
            };
            // An empty bucket (0 records) carries no flashes; age it by its
            // bucket-start time derived from the filename so it can still be
            // pruned. With records, time_max is exact.
            let newest_ms = if header.record_count > 0 {
                header.time_max_unix_ms
            } else {
                bucket_start_from_path(&date_dir, &path).unwrap_or(i64::MIN)
            };
            let bytes = data.len() as u64;
            out.push(BucketRef {
                date_dir: date_dir.clone(),
                path,
                newest_ms,
                bytes,
            });
        }
    }
    Ok(out)
}

/// Reconstruct a bucket's start time (Unix ms) from its `YYYYMMDD` date dir and
/// `tHHMM.rwl` filename, for aging an empty bucket. Returns `None` if either
/// component does not parse.
fn bucket_start_from_path(date_dir: &str, path: &Path) -> Option<i64> {
    let stem = path.file_stem()?.to_str()?; // "tHHMM"
    let hhmm = stem.strip_prefix('t')?;
    if hhmm.len() != 4 {
        return None;
    }
    let hh: i64 = hhmm[0..2].parse().ok()?;
    let mm: i64 = hhmm[2..4].parse().ok()?;
    if date_dir.len() != 8 {
        return None;
    }
    let y: i64 = date_dir[0..4].parse().ok()?;
    let mo: u32 = date_dir[4..6].parse().ok()?;
    let d: u32 = date_dir[6..8].parse().ok()?;
    let day_ms = format::days_from_civil(y, mo, d)? * 86_400_000;
    Some(day_ms + (hh * 3600 + mm * 60) * 1000)
}

/// Whether a directory has no entries (treats an unreadable dir as non-empty so
/// we never try to remove it).
fn dir_is_empty(dir: &Path) -> bool {
    match fs::read_dir(dir) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlashRecord;
    use crate::store::BucketWriter;

    /// 2026-01-01 00:00:00 UTC in Unix ms.
    const BASE: i64 = 1_767_225_600_000;

    fn test_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-glm-window-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn flash(time: i64) -> FlashRecord {
        FlashRecord {
            time_unix_ms: time,
            lat: 30.0,
            lon: -95.0,
            energy: 1.0e-15,
            area: 25.0,
            flash_id: 1,
            flags: 0,
            duration_ms: 400,
        }
    }

    #[test]
    fn unbounded_is_a_no_op() {
        let root = test_root("noop");
        let mut w = BucketWriter::open(&root, "goes19").unwrap();
        w.insert_flashes(&[flash(BASE)], 1).unwrap();
        drop(w);
        let report =
            enforce_window(&root, "goes19", BASE + 10_000_000, &WindowConfig::default()).unwrap();
        assert_eq!(report, PruneReport::default());
        assert!(root.join("glm/goes19/20260101/t0000.rwl").is_file());
    }

    #[test]
    fn max_age_evicts_old_buckets() {
        let root = test_root("age");
        let mut w = BucketWriter::open(&root, "goes19").unwrap();
        // Buckets at t0000, t0010, t0020, t0030.
        w.insert_flashes(
            &[
                flash(BASE),             // t0000
                flash(BASE + 600_000),   // t0010
                flash(BASE + 1_200_000), // t0020
                flash(BASE + 1_800_000), // t0030
            ],
            1,
        )
        .unwrap();
        drop(w);

        // now = t0030 + 1 min; window = 20 min. Cutoff = t0011. t0000 and t0010
        // are older; t0020 and t0030 stay.
        let now = BASE + 1_800_000 + 60_000;
        let report = enforce_window(
            &root,
            "goes19",
            now,
            &WindowConfig {
                max_age_ms: Some(20 * 60_000),
                max_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(report.removed_buckets, 2);
        assert!(report.removed_bytes > 0);
        let day = root.join("glm/goes19/20260101");
        assert!(!day.join("t0000.rwl").exists());
        assert!(!day.join("t0010.rwl").exists());
        assert!(day.join("t0020.rwl").is_file());
        assert!(day.join("t0030.rwl").is_file());
    }

    #[test]
    fn max_age_eviction_across_a_date_dir_boundary() {
        let root = test_root("age-date");
        let mut w = BucketWriter::open(&root, "goes19").unwrap();
        // One flash the day before, one this day.
        w.insert_flashes(&[flash(BASE - 600_000)], 1).unwrap(); // 2025-12-31 t2350
        w.insert_flashes(&[flash(BASE + 600_000)], 1).unwrap(); // 2026-01-01 t0010
        drop(w);

        assert!(root.join("glm/goes19/20251231/t2350.rwl").is_file());
        assert!(root.join("glm/goes19/20260101/t0010.rwl").is_file());

        // now = t0010 + 1 min (00:11). Window 15 min => cutoff 23:56: the
        // prior-day 23:50 bucket drops (leaving 20251231 empty), the 00:10
        // bucket (1 min old) stays.
        let now = BASE + 600_000 + 60_000;
        let report = enforce_window(
            &root,
            "goes19",
            now,
            &WindowConfig {
                max_age_ms: Some(15 * 60_000),
                max_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(report.removed_buckets, 1);
        assert_eq!(report.removed_date_dirs, vec!["20251231".to_string()]);
        assert!(
            !root.join("glm/goes19/20251231").exists(),
            "emptied date dir is removed"
        );
        assert!(root.join("glm/goes19/20260101/t0010.rwl").is_file());
    }

    #[test]
    fn max_bytes_evicts_oldest_first() {
        let root = test_root("bytes");
        let mut w = BucketWriter::open(&root, "goes19").unwrap();
        w.insert_flashes(
            &[flash(BASE), flash(BASE + 600_000), flash(BASE + 1_200_000)],
            1,
        )
        .unwrap();
        drop(w);

        let day = root.join("glm/goes19/20260101");
        let per_bucket = fs::metadata(day.join("t0000.rwl")).unwrap().len();
        // Budget for exactly two buckets.
        let report = enforce_window(
            &root,
            "goes19",
            BASE + 10_000_000,
            &WindowConfig {
                max_age_ms: None,
                max_bytes: Some(per_bucket * 2),
            },
        )
        .unwrap();
        assert_eq!(report.removed_buckets, 1);
        assert!(!day.join("t0000.rwl").exists(), "oldest goes first");
        assert!(day.join("t0010.rwl").is_file());
        assert!(day.join("t0020.rwl").is_file());
    }

    #[test]
    fn locked_satellite_dir_is_skipped_then_pruned_after_release() {
        let root = test_root("locked");
        let mut w = BucketWriter::open(&root, "goes19").unwrap();
        w.insert_flashes(&[flash(BASE)], 1).unwrap();
        // Keep the writer (and its satellite lock) alive across the prune.

        let config = WindowConfig {
            max_age_ms: Some(60_000),
            max_bytes: None,
        };
        let now = BASE + 10_000_000;
        let skipped = enforce_window(&root, "goes19", now, &config).unwrap();
        assert_eq!(skipped.removed_buckets, 0, "locked store not pruned");
        assert_eq!(skipped.skipped_locked, vec!["goes19".to_string()]);
        assert!(
            root.join("glm/goes19/20260101/t0000.rwl").is_file(),
            "bucket survives while the store is locked"
        );

        // Release the writer lock; the next pass prunes normally.
        drop(w);
        let pruned = enforce_window(&root, "goes19", now, &config).unwrap();
        assert_eq!(pruned.removed_buckets, 1);
        assert!(pruned.skipped_locked.is_empty());
        assert_eq!(pruned.removed_date_dirs, vec!["20260101".to_string()]);
        assert!(!root.join("glm/goes19/20260101").exists());
    }

    #[test]
    fn missing_store_is_a_clean_no_op() {
        let root = test_root("missing");
        let report = enforce_window(
            &root,
            "goes18",
            BASE,
            &WindowConfig {
                max_age_ms: Some(60_000),
                max_bytes: None,
            },
        )
        .unwrap();
        assert_eq!(report, PruneReport::default());
    }
}

//! Lock-free reader for the `.rwl` flash store.
//!
//! [`read_flashes`] selects candidate bucket files by date-dir + bucket-name
//! math (never scanning unrelated days), scans their fixed records, and
//! filters by time and optional bounding box. Because every bucket is written
//! by an atomic temp+fsync+rename (see [`crate::store`]), a reader observes
//! either the old complete file or the new complete file and so needs no lock.

use std::path::{Path, PathBuf};

use crate::error::{RwlError, RwlResult};
use crate::format::{self, FlashRecord, HEADER_LEN, RECORD_LEN, RwlHeader};

/// A single decoded flash event — a plain value mirror of [`FlashRecord`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flash {
    /// First-event time of the flash, Unix milliseconds.
    pub time_unix_ms: i64,
    /// Latitude, degrees north.
    pub lat: f32,
    /// Longitude, degrees east.
    pub lon: f32,
    /// Radiant energy, raw SI joules (no normalization).
    pub energy: f32,
    /// Flash area, km^2.
    pub area: f32,
    /// GLM flash id (granule-scoped).
    pub flash_id: u32,
    /// Quality bitfield; bit 0 = degraded quality.
    pub flags: u16,
    /// Flash duration in ms (saturating at 65535).
    pub duration_ms: u16,
}

impl Flash {
    fn from_record(r: FlashRecord) -> Self {
        Self {
            time_unix_ms: r.time_unix_ms,
            lat: r.lat,
            lon: r.lon,
            energy: r.energy,
            area: r.area,
            flash_id: r.flash_id,
            flags: r.flags,
            duration_ms: r.duration_ms,
        }
    }

    /// True if the quality bit is set (consumers QC-filter on this).
    pub fn is_degraded(&self) -> bool {
        self.flags & format::FLAG_DEGRADED_QUALITY != 0
    }
}

/// An inclusive geographic bounding box (degrees). A flash matches when its
/// latitude is in `[lat_min, lat_max]` and longitude in `[lon_min, lon_max]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BBox {
    pub lat_min: f32,
    pub lat_max: f32,
    pub lon_min: f32,
    pub lon_max: f32,
}

impl BBox {
    pub fn new(lat_min: f32, lat_max: f32, lon_min: f32, lon_max: f32) -> Self {
        Self {
            lat_min,
            lat_max,
            lon_min,
            lon_max,
        }
    }

    fn contains(&self, lat: f32, lon: f32) -> bool {
        lat >= self.lat_min && lat <= self.lat_max && lon >= self.lon_min && lon <= self.lon_max
    }
}

/// Read all flashes for `satellite` whose time falls in the **half-open**
/// range `[t0, t1)` (Unix ms), optionally clipped to `bbox`.
///
/// Semantics:
/// - **Half-open `[t0, t1)`**: a flash at exactly `t0` is included; a flash at
///   exactly `t1` is excluded. An empty or inverted range (`t1 <= t0`) returns
///   `Ok(vec![])` without touching the filesystem.
/// - **File selection** walks every 10-minute bucket whose start lies in
///   `[bucket_start(t0), t1)` and reads only those files (by their
///   `<root>/glm/<satellite>/<YYYYMMDD>/tHHMM.rwl` path). Unrelated days are
///   never enumerated.
/// - A **missing satellite directory** (or any missing bucket file) is not an
///   error — it contributes no flashes. `Ok(vec![])` for an absent store.
/// - Results are returned in ascending time order (buckets are visited in
///   ascending order and each bucket's records are already sorted).
///
/// `Err` is reserved for real I/O failures and for a bucket file that exists
/// but is structurally malformed (bad magic / version / size). Use
/// [`crate::validate`] to diagnose a store without aborting on the first bad
/// file.
pub fn read_flashes(
    root: &Path,
    satellite: &str,
    t0: i64,
    t1: i64,
    bbox: Option<BBox>,
) -> RwlResult<Vec<Flash>> {
    let mut out = Vec::new();
    if t1 <= t0 {
        return Ok(out);
    }

    let sat_dir = root.join("glm").join(satellite);
    if !sat_dir.is_dir() {
        return Ok(out);
    }

    // Enumerate candidate buckets: every 10-minute boundary from the bucket
    // containing t0 up to (but not including) t1. The first bucket may begin
    // before t0; its in-range records are still filtered below.
    let mut cursor = format::bucket_start_ms(t0);
    while cursor < t1 {
        let path = bucket_path(&sat_dir, cursor);
        if path.is_file() {
            read_bucket_into(&path, t0, t1, bbox.as_ref(), &mut out)?;
        }
        // Step to the next bucket. The store never spans more than a few hours,
        // so this loop is tiny; saturating_add guards a pathological t1.
        cursor = cursor.saturating_add(format::BUCKET_SPAN_MS);
    }

    Ok(out)
}

/// Build the `<sat_dir>/<YYYYMMDD>/tHHMM.rwl` path for a bucket-start time.
fn bucket_path(sat_dir: &Path, bucket_start_ms: i64) -> PathBuf {
    sat_dir
        .join(format::date_dir(bucket_start_ms))
        .join(format::bucket_name(bucket_start_ms))
}

/// Read one bucket file, appending matching flashes to `out`. Returns
/// [`RwlError::Format`] only for a structurally broken file.
fn read_bucket_into(
    path: &Path,
    t0: i64,
    t1: i64,
    bbox: Option<&BBox>,
    out: &mut Vec<Flash>,
) -> RwlResult<()> {
    let data = std::fs::read(path)?;
    let header = RwlHeader::parse(&data)
        .map_err(|e| RwlError::Format(format!("{}: {e}", path.display())))?;

    // Bounds: file size must be exactly header + record_count * 32.
    let record_count = header.record_count as usize;
    let expected_len = HEADER_LEN
        .checked_add(record_count.checked_mul(RECORD_LEN).ok_or_else(|| {
            RwlError::Format(format!("{}: record_count overflows", path.display()))
        })?)
        .ok_or_else(|| RwlError::Format(format!("{}: file length overflows", path.display())))?;
    if data.len() != expected_len {
        return Err(RwlError::Format(format!(
            "{}: size {} != expected {} (64 + 32*{})",
            path.display(),
            data.len(),
            expected_len,
            record_count
        )));
    }

    for i in 0..record_count {
        let start = HEADER_LEN + i * RECORD_LEN;
        // Bounds guaranteed by the size check above.
        let rec = FlashRecord::unpack(&data[start..start + RECORD_LEN])?;
        if rec.time_unix_ms < t0 || rec.time_unix_ms >= t1 {
            continue;
        }
        if let Some(b) = bbox {
            if !b.contains(rec.lat, rec.lon) {
                continue;
            }
        }
        out.push(Flash::from_record(rec));
    }
    Ok(())
}

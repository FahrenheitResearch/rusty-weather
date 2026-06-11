//! Validation for `.rwl` flash bucket files.
//!
//! Same contract as `rw_store::validate`: [`validate_bucket_file`] returns
//! `Ok(report)` for any file that *opens* — every format problem lands in
//! [`ValidationReport::errors`] so a CLI can print them all instead of
//! stopping at the first. `Err(_)` is reserved for I/O failures. The checker
//! never panics on hostile bytes: every slice access is bounds-checked and
//! every length/count derived from the file goes through checked arithmetic.
//!
//! Two depths:
//! - [`ValidateDepth::Structural`] — magic, version, exact file size vs
//!   `64 + 32*record_count`, header `time_min <= time_max`, strict-ish
//!   ascending record sort (non-decreasing, ties allowed), and each record's
//!   header-extent agreement (record times within `[time_min, time_max]`).
//! - [`ValidateDepth::Deep`] — everything structural plus per-record value
//!   sanity: finite lat/lon/energy/area, lat in `[-90, 90]`, lon in
//!   `[-180, 180]`, and `flags` carrying no bits outside the v1 known set.

use std::path::Path;

use crate::error::RwlResult;
use crate::format::{FlashRecord, HEADER_LEN, KNOWN_FLAGS, MAGIC, RECORD_LEN, SUPPORTED_VERSIONS};

/// How deeply to validate a bucket file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateDepth {
    /// Header + layout + sort + header-extent agreement. No per-value checks.
    Structural,
    /// Structural + per-record value sanity (finite, in-range, known flags).
    Deep,
}

/// Aggregate result of validating one or more bucket files.
#[derive(Debug, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    pub stats: ValidationStats,
}

/// Counts collected during validation.
#[derive(Debug, Default)]
pub struct ValidationStats {
    /// Number of flash records seen.
    pub records: u64,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool {
        self.errors.is_empty()
    }

    fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    fn warn(&mut self, msg: impl Into<String>) {
        self.warnings.push(msg.into());
    }
}

/// Validate one `.rwl` bucket file at the requested depth.
///
/// Returns `Err` only for an I/O failure opening the file; all format problems
/// are reported in `report.errors`.
pub fn validate_bucket_file(path: &Path, depth: ValidateDepth) -> RwlResult<ValidationReport> {
    let data = std::fs::read(path)?;
    let mut report = ValidationReport::default();
    check_bucket(&data, depth, &mut report);
    Ok(report)
}

/// Core checker operating on in-memory bytes. Never panics on hostile input.
fn check_bucket(data: &[u8], depth: ValidateDepth, report: &mut ValidationReport) {
    // Check 1: at least a header's worth of bytes.
    if data.len() < HEADER_LEN {
        report.error(format!(
            "file shorter than header: {} bytes < {HEADER_LEN}",
            data.len()
        ));
        return;
    }

    // Check 2: magic.
    if &data[0..8] != MAGIC.as_slice() {
        report.error(format!(
            "bad magic: expected {:?}, got {:?}",
            MAGIC,
            &data[0..8]
        ));
        return;
    }

    // Check 3: version in whitelist.
    let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
    if !SUPPORTED_VERSIONS.contains(&version) {
        report.error(format!(
            "unsupported version {version} (supported: {SUPPORTED_VERSIONS:?})"
        ));
        return;
    }

    let record_count = u32::from_le_bytes(data[12..16].try_into().unwrap());
    let header_time_min = i64::from_le_bytes(data[16..24].try_into().unwrap());
    let header_time_max = i64::from_le_bytes(data[24..32].try_into().unwrap());
    // source_granule_count [32..36] is informational; no constraint to check.

    // Check 4: reserved bytes [36..64] should be zero (warn, not error).
    if data[36..64].iter().any(|&b| b != 0) {
        report.warn("reserved header bytes [36..64] are non-zero".to_string());
    }

    // Check 5: exact file size == 64 + 32*record_count, with checked
    // arithmetic so a hostile record_count cannot wrap the multiply.
    let payload_bytes = match (record_count as usize).checked_mul(RECORD_LEN) {
        Some(v) => v,
        None => {
            report.error(format!(
                "record_count {record_count} * {RECORD_LEN} overflows usize"
            ));
            return;
        }
    };
    let expected_len = match HEADER_LEN.checked_add(payload_bytes) {
        Some(v) => v,
        None => {
            report.error("header + payload length overflows usize".to_string());
            return;
        }
    };
    if data.len() != expected_len {
        if data.len() < expected_len {
            report.error(format!(
                "file truncated: {} bytes < expected {expected_len} (64 + 32*{record_count})",
                data.len()
            ));
        } else {
            report.error(format!(
                "trailing bytes: {} bytes > expected {expected_len} (trailing {} bytes)",
                data.len(),
                data.len() - expected_len
            ));
        }
        // The size is wrong, but we can still scan whatever whole records fit
        // to surface more diagnostics — bounded by what is actually present.
    }

    // Check 6: header time range self-consistency.
    if header_time_min > header_time_max {
        report.error(format!(
            "header time_min {header_time_min} > time_max {header_time_max}"
        ));
    }

    // Scan only records that fully fit in the buffer (defends a truncated or
    // count-lying file from out-of-bounds slicing).
    let max_whole_records = data.len().saturating_sub(HEADER_LEN) / RECORD_LEN;
    let scan_count = (record_count as usize).min(max_whole_records);
    report.stats.records = scan_count as u64;

    let mut prev_time: Option<i64> = None;
    let mut seen_min = i64::MAX;
    let mut seen_max = i64::MIN;
    let mut any_record = false;

    for i in 0..scan_count {
        let start = HEADER_LEN + i * RECORD_LEN;
        let end = start + RECORD_LEN;
        // end <= data.len() guaranteed by scan_count derivation.
        let rec = match FlashRecord::unpack(&data[start..end]) {
            Ok(r) => r,
            Err(err) => {
                report.error(format!("record {i}: {err}"));
                continue;
            }
        };
        any_record = true;

        // Check 7: non-decreasing time (ascending sort, ties allowed/stable).
        if let Some(prev) = prev_time {
            if rec.time_unix_ms < prev {
                report.error(format!(
                    "sort order violated at record {i}: time {} < previous {}",
                    rec.time_unix_ms, prev
                ));
            }
        }
        prev_time = Some(rec.time_unix_ms);

        // Check 8: each record's time within the header-declared extent.
        if rec.time_unix_ms < header_time_min || rec.time_unix_ms > header_time_max {
            report.error(format!(
                "record {i} time {} outside header extent [{header_time_min}, {header_time_max}]",
                rec.time_unix_ms
            ));
        }

        seen_min = seen_min.min(rec.time_unix_ms);
        seen_max = seen_max.max(rec.time_unix_ms);

        if depth == ValidateDepth::Deep {
            check_record_values(i, &rec, report);
        }
    }

    // Check 9: header extent matches the actual record min/max (only when the
    // file is otherwise the declared size and non-empty — a count-lying or
    // truncated file already errored above).
    if any_record && data.len() == expected_len {
        if header_time_min != seen_min {
            report.error(format!(
                "header time_min {header_time_min} != actual min record time {seen_min}"
            ));
        }
        if header_time_max != seen_max {
            report.error(format!(
                "header time_max {header_time_max} != actual max record time {seen_max}"
            ));
        }
    }
    // An empty file (record_count == 0) must have a zeroed extent.
    if record_count == 0 && (header_time_min != 0 || header_time_max != 0) {
        report.warn(format!(
            "empty bucket has non-zero header extent [{header_time_min}, {header_time_max}]"
        ));
    }
}

/// Deep per-record value sanity.
fn check_record_values(i: usize, rec: &FlashRecord, report: &mut ValidationReport) {
    if !rec.lat.is_finite() || rec.lat < -90.0 || rec.lat > 90.0 {
        report.error(format!(
            "record {i}: lat {} not a finite value in [-90, 90]",
            rec.lat
        ));
    }
    if !rec.lon.is_finite() || rec.lon < -180.0 || rec.lon > 180.0 {
        report.error(format!(
            "record {i}: lon {} not a finite value in [-180, 180]",
            rec.lon
        ));
    }
    if !rec.energy.is_finite() {
        report.error(format!("record {i}: energy {} not finite", rec.energy));
    }
    if !rec.area.is_finite() {
        report.error(format!("record {i}: area {} not finite", rec.area));
    }
    if rec.flags & !KNOWN_FLAGS != 0 {
        report.error(format!(
            "record {i}: flags 0x{:04x} has bits outside the v1 known set 0x{KNOWN_FLAGS:04x}",
            rec.flags
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{RwlHeader, VERSION};
    use crate::store::pack_bucket;

    fn rec(time: i64, lat: f32, lon: f32) -> FlashRecord {
        FlashRecord {
            time_unix_ms: time,
            lat,
            lon,
            energy: 1.0e-15,
            area: 25.0,
            flash_id: 1,
            flags: 0,
            duration_ms: 400,
        }
    }

    fn valid_bytes() -> Vec<u8> {
        let records = vec![
            rec(1000, 30.0, -95.0),
            rec(2000, 31.0, -94.0),
            rec(2000, 31.5, -93.5), // tie on time — allowed
            rec(3000, 32.0, -93.0),
        ];
        pack_bucket(&records, 2)
    }

    fn check(bytes: &[u8], depth: ValidateDepth) -> ValidationReport {
        let mut r = ValidationReport::default();
        check_bucket(bytes, depth, &mut r);
        r
    }

    #[test]
    fn valid_bucket_passes_structural_and_deep() {
        let bytes = valid_bytes();
        let s = check(&bytes, ValidateDepth::Structural);
        assert!(s.is_ok(), "structural errors: {:?}", s.errors);
        assert_eq!(s.stats.records, 4);
        let d = check(&bytes, ValidateDepth::Deep);
        assert!(d.is_ok(), "deep errors: {:?}", d.errors);
    }

    #[test]
    fn empty_bucket_is_ok() {
        let bytes = pack_bucket(&[], 0);
        let r = check(&bytes, ValidateDepth::Deep);
        assert!(r.is_ok(), "empty errors: {:?}", r.errors);
        assert_eq!(r.stats.records, 0);
    }

    #[test]
    fn bad_magic_reports_error() {
        let mut bytes = valid_bytes();
        bytes[0] = b'X';
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("magic")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn bad_version_reports_error() {
        let mut bytes = valid_bytes();
        bytes[8..12].copy_from_slice(&7u32.to_le_bytes());
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("version")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn truncation_reports_error_without_panic() {
        let mut bytes = valid_bytes();
        bytes.truncate(bytes.len() - 5);
        let r = check(&bytes, ValidateDepth::Deep);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("truncated")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn trailing_bytes_report_error() {
        let mut bytes = valid_bytes();
        bytes.extend_from_slice(&[0u8; 8]);
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("trailing")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn count_mismatch_lying_header_does_not_panic() {
        // Claim a huge record_count but provide a tiny file.
        let mut bytes = valid_bytes();
        bytes[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let r = check(&bytes, ValidateDepth::Deep);
        assert!(!r.is_ok());
        // Must mention truncation/size, must not panic (we got here).
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("truncated") || e.contains("byte")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn unsorted_records_report_error() {
        // Hand-build a header that's internally consistent on extent but whose
        // records are out of order.
        let r0 = rec(3000, 30.0, -95.0);
        let r1 = rec(1000, 31.0, -94.0);
        let header = RwlHeader {
            version: VERSION,
            record_count: 2,
            time_min_unix_ms: 1000,
            time_max_unix_ms: 3000,
            source_granule_count: 1,
        };
        let mut bytes = Vec::new();
        header.pack_into(&mut bytes);
        r0.pack_into(&mut bytes);
        r1.pack_into(&mut bytes);
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("sort")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn header_extent_lie_reports_error() {
        let mut bytes = valid_bytes();
        // Overwrite header time_max to a value larger than any record.
        bytes[24..32].copy_from_slice(&9_999_999i64.to_le_bytes());
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors
                .iter()
                .any(|e| e.contains("time_max") || e.contains("extent")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn header_min_greater_than_max_reports_error() {
        let header = RwlHeader {
            version: VERSION,
            record_count: 0,
            time_min_unix_ms: 5000,
            time_max_unix_ms: 1000,
            source_granule_count: 0,
        };
        let mut bytes = Vec::new();
        header.pack_into(&mut bytes);
        let r = check(&bytes, ValidateDepth::Structural);
        assert!(!r.is_ok());
        assert!(
            r.errors.iter().any(|e| e.contains("time_min")),
            "{:?}",
            r.errors
        );
    }

    #[test]
    fn deep_catches_out_of_range_lat_and_bad_flags() {
        let mut records = vec![rec(1000, 30.0, -95.0)];
        records[0].lat = 200.0; // impossible
        records[0].flags = 0x8000; // reserved bit set
        let bytes = pack_bucket(&records, 1);
        // Structural passes (layout is fine), deep flags the values.
        let s = check(&bytes, ValidateDepth::Structural);
        assert!(s.is_ok(), "structural should pass: {:?}", s.errors);
        let d = check(&bytes, ValidateDepth::Deep);
        assert!(!d.is_ok());
        assert!(d.errors.iter().any(|e| e.contains("lat")), "{:?}", d.errors);
        assert!(
            d.errors.iter().any(|e| e.contains("flags")),
            "{:?}",
            d.errors
        );
    }

    #[test]
    fn deep_catches_nonfinite_energy() {
        let mut records = vec![rec(1000, 30.0, -95.0)];
        records[0].energy = f32::NAN;
        let bytes = pack_bucket(&records, 1);
        let d = check(&bytes, ValidateDepth::Deep);
        assert!(!d.is_ok());
        assert!(
            d.errors.iter().any(|e| e.contains("energy")),
            "{:?}",
            d.errors
        );
    }
}

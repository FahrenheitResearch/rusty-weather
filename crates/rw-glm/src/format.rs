//! On-disk layout of the `.rwl` flash store: the 64-byte file header, the
//! fixed 32-byte flash record, and the `tHHMM` bucket-name math.
//!
//! All multi-byte integers and floats are little-endian, matching rw-store.
//! This module is pure layout — no I/O, no allocation beyond the pack buffer —
//! so it can be unit-tested against exact byte offsets the way
//! `rw_store::index` is.

use crate::error::{RwlError, RwlResult};

/// File magic, bytes `[0..8]` of every `.rwl` file.
pub const MAGIC: &[u8; 8] = b"RWLIGHT1";

/// Current format version (header bytes `[8..12]`).
pub const VERSION: u32 = 1;

/// Versions this build can read. Writers always emit [`VERSION`].
pub const SUPPORTED_VERSIONS: &[u32] = &[1];

/// Byte length of the fixed file header.
pub const HEADER_LEN: usize = 64;

/// Byte length of one fixed flash record.
pub const RECORD_LEN: usize = 32;

/// `flags` bit 0: the flash's quality flag was anything but its nominal/good
/// value in the source granule. Consumers QC-filter on this bit. Bits 1..15
/// are reserved, written zero, and ignored by readers.
pub const FLAG_DEGRADED_QUALITY: u16 = 1 << 0;

/// Mask of every `flags` bit defined in v1.
pub const KNOWN_FLAGS: u16 = FLAG_DEGRADED_QUALITY;

/// The 64-byte `.rwl` file header.
///
/// Byte layout (little-endian, 64 bytes):
/// ```text
///  0- 7  magic                 b"RWLIGHT1"
///  8-11  version               u32  (= 1)
/// 12-15  record_count          u32
/// 16-23  time_min_unix_ms      i64
/// 24-31  time_max_unix_ms      i64
/// 32-35  source_granule_count  u32
/// 36-63  reserved              (zeros)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RwlHeader {
    pub version: u32,
    pub record_count: u32,
    pub time_min_unix_ms: i64,
    pub time_max_unix_ms: i64,
    pub source_granule_count: u32,
}

impl RwlHeader {
    /// Append exactly 64 bytes to `out`.
    pub fn pack_into(&self, out: &mut Vec<u8>) {
        out.reserve(HEADER_LEN);
        out.extend_from_slice(MAGIC); // 0..8
        out.extend_from_slice(&self.version.to_le_bytes()); // 8..12
        out.extend_from_slice(&self.record_count.to_le_bytes()); // 12..16
        out.extend_from_slice(&self.time_min_unix_ms.to_le_bytes()); // 16..24
        out.extend_from_slice(&self.time_max_unix_ms.to_le_bytes()); // 24..32
        out.extend_from_slice(&self.source_granule_count.to_le_bytes()); // 32..36
        out.extend_from_slice(&[0u8; 28]); // 36..64 reserved
    }

    /// Parse and *validate* a header from the first 64 bytes of `bytes`.
    ///
    /// Returns [`RwlError::Format`] for a short buffer or bad magic,
    /// [`RwlError::UnsupportedVersion`] for an out-of-whitelist version. The
    /// caller is responsible for the count-vs-filesize and `min <= max` checks
    /// that need the whole file (see [`crate::reader`] / [`crate::validate`]).
    pub fn parse(bytes: &[u8]) -> RwlResult<Self> {
        if bytes.len() < HEADER_LEN {
            return Err(RwlError::Format(format!(
                "header requires {HEADER_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        if &bytes[0..8] != MAGIC.as_slice() {
            return Err(RwlError::Format(format!(
                "bad magic: expected {:?}, got {:?}",
                MAGIC,
                &bytes[0..8]
            )));
        }
        let version = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        if !SUPPORTED_VERSIONS.contains(&version) {
            return Err(RwlError::UnsupportedVersion {
                found: version,
                supported: SUPPORTED_VERSIONS,
            });
        }
        let record_count = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let time_min_unix_ms = i64::from_le_bytes(bytes[16..24].try_into().unwrap());
        let time_max_unix_ms = i64::from_le_bytes(bytes[24..32].try_into().unwrap());
        let source_granule_count = u32::from_le_bytes(bytes[32..36].try_into().unwrap());
        Ok(Self {
            version,
            record_count,
            time_min_unix_ms,
            time_max_unix_ms,
            source_granule_count,
        })
    }
}

/// One fixed 32-byte flash record.
///
/// `time_unix_ms` is a signed i64 so the format can represent any instant,
/// including negative (pre-1970) times. Live GLM data is always well after
/// 1970, so a negative or pre-epoch flash time is unexpected in practice; the
/// validator nonetheless *accepts* it (it is a representable, internally
/// consistent value, not a structural defect) and leaves any such filtering to
/// the consumer.
///
/// Byte layout (little-endian, 32 bytes):
/// ```text
///  0- 7  time_unix_ms  i64   first-event time of the flash
///  8-11  lat           f32   degrees north
/// 12-15  lon           f32   degrees east
/// 16-19  energy        f32   raw SI joules (no normalization)
/// 20-23  area          f32   km^2
/// 24-27  flash_id      u32   GLM flash id (granule-scoped)
/// 28-29  flags         u16   bit 0 = degraded_quality; bits 1..15 reserved
/// 30-31  duration_ms   u16   flash duration, saturating at 65535
/// ```
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlashRecord {
    pub time_unix_ms: i64,
    pub lat: f32,
    pub lon: f32,
    pub energy: f32,
    pub area: f32,
    pub flash_id: u32,
    pub flags: u16,
    pub duration_ms: u16,
}

impl FlashRecord {
    /// Append exactly 32 bytes to `out`.
    pub fn pack_into(&self, out: &mut Vec<u8>) {
        out.reserve(RECORD_LEN);
        out.extend_from_slice(&self.time_unix_ms.to_le_bytes()); // 0..8
        out.extend_from_slice(&self.lat.to_le_bytes()); // 8..12
        out.extend_from_slice(&self.lon.to_le_bytes()); // 12..16
        out.extend_from_slice(&self.energy.to_le_bytes()); // 16..20
        out.extend_from_slice(&self.area.to_le_bytes()); // 20..24
        out.extend_from_slice(&self.flash_id.to_le_bytes()); // 24..28
        out.extend_from_slice(&self.flags.to_le_bytes()); // 28..30
        out.extend_from_slice(&self.duration_ms.to_le_bytes()); // 30..32
    }

    /// Parse a record from the first 32 bytes of `bytes`.
    pub fn unpack(bytes: &[u8]) -> RwlResult<Self> {
        if bytes.len() < RECORD_LEN {
            return Err(RwlError::Format(format!(
                "flash record requires {RECORD_LEN} bytes, got {}",
                bytes.len()
            )));
        }
        let time_unix_ms = i64::from_le_bytes(bytes[0..8].try_into().unwrap());
        let lat = f32::from_le_bytes(bytes[8..12].try_into().unwrap());
        let lon = f32::from_le_bytes(bytes[12..16].try_into().unwrap());
        let energy = f32::from_le_bytes(bytes[16..20].try_into().unwrap());
        let area = f32::from_le_bytes(bytes[20..24].try_into().unwrap());
        let flash_id = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let flags = u16::from_le_bytes(bytes[28..30].try_into().unwrap());
        let duration_ms = u16::from_le_bytes(bytes[30..32].try_into().unwrap());
        Ok(Self {
            time_unix_ms,
            lat,
            lon,
            energy,
            area,
            flash_id,
            flags,
            duration_ms,
        })
    }
}

/// Saturate a flash duration in milliseconds into the `u16` record field. A
/// negative duration (last-event before first-event — a malformed granule)
/// clamps to 0; anything `>= 65535` clamps to `u16::MAX`.
pub fn saturate_duration_ms(duration_ms: i64) -> u16 {
    duration_ms.clamp(0, u16::MAX as i64) as u16
}

/// Number of milliseconds in one UTC day.
const MS_PER_DAY: i64 = 86_400_000;
/// Number of milliseconds in one 10-minute bucket.
const MS_PER_BUCKET: i64 = 600_000;

/// Floor a `time_unix_ms` to the start of its UTC day, returning the
/// millisecond-since-epoch value of `00:00:00` that day. Uses Euclidean
/// division so pre-epoch (negative) times floor *down* correctly.
pub fn day_floor_ms(time_unix_ms: i64) -> i64 {
    time_unix_ms.div_euclid(MS_PER_DAY) * MS_PER_DAY
}

/// The UTC date-directory name `YYYYMMDD` for a flash time.
///
/// Implemented with a self-contained civil-from-days algorithm (Howard
/// Hinnant's `days_from_civil` inverse) so the crate needs no chrono/time
/// dependency. Correct for any in-range `i64` millisecond timestamp.
pub fn date_dir(time_unix_ms: i64) -> String {
    let days = time_unix_ms.div_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}{m:02}{d:02}")
}

/// The bucket filename `tHHMM.rwl` for a flash time, where `HHMM` is the UTC
/// hour and minute floored to a 10-minute boundary (`t0000`, `t0010`, …,
/// `t2350`).
pub fn bucket_name(time_unix_ms: i64) -> String {
    let day_start = day_floor_ms(time_unix_ms);
    let into_day = time_unix_ms - day_start; // 0 .. MS_PER_DAY, always >= 0
    let bucket_idx = into_day / MS_PER_BUCKET; // 0 .. 143
    let bucket_start_min = bucket_idx * 10; // minutes into the day
    let hh = bucket_start_min / 60;
    let mm = bucket_start_min % 60;
    format!("t{hh:02}{mm:02}.rwl")
}

/// The millisecond timestamp at which the bucket containing `time_unix_ms`
/// begins (its 10-minute floor). Used by the reader to enumerate candidate
/// buckets across a time range.
pub fn bucket_start_ms(time_unix_ms: i64) -> i64 {
    time_unix_ms.div_euclid(MS_PER_BUCKET) * MS_PER_BUCKET
}

/// Step from one bucket's start to the next (10 minutes in ms).
pub const BUCKET_SPAN_MS: i64 = MS_PER_BUCKET;

/// Convert a day count since the Unix epoch (1970-01-01 = 0) into a
/// `(year, month, day)` civil date. From Howard Hinnant's `civil_from_days`.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_record() -> FlashRecord {
        FlashRecord {
            time_unix_ms: 0x0102_0304_0506_0708,
            lat: 30.5_f32,
            lon: -95.25_f32,
            energy: 1.5e-15_f32,
            area: 42.0_f32,
            flash_id: 0x1314_1516,
            flags: 0x0001,
            duration_ms: 0x1718,
        }
    }

    #[test]
    fn record_round_trips_through_32_bytes() {
        let r = sample_record();
        let mut buf = Vec::new();
        r.pack_into(&mut buf);
        assert_eq!(buf.len(), RECORD_LEN);
        let r2 = FlashRecord::unpack(&buf).unwrap();
        assert_eq!(r2, r);
    }

    #[test]
    fn record_pack_layout_is_exact() {
        let r = FlashRecord {
            time_unix_ms: 0x0102_0304_0506_0708,
            lat: 0.0_f32,    // bits 0x00000000
            lon: 1.0_f32,    // bits 0x3f800000
            energy: 2.0_f32, // bits 0x40000000
            area: -1.0_f32,  // bits 0xbf800000
            flash_id: 0x191A_1B1C,
            flags: 0x1D1E,
            duration_ms: 0x1F20,
        };
        let mut buf = Vec::new();
        r.pack_into(&mut buf);
        assert_eq!(buf.len(), 32);
        // time_unix_ms [0..8] LE
        assert_eq!(
            &buf[0..8],
            &[0x08, 0x07, 0x06, 0x05, 0x04, 0x03, 0x02, 0x01],
            "time_unix_ms LE bytes"
        );
        // lat [8..12] = 0.0_f32
        assert_eq!(&buf[8..12], &[0x00, 0x00, 0x00, 0x00], "lat LE bytes");
        // lon [12..16] = 1.0_f32
        assert_eq!(&buf[12..16], &[0x00, 0x00, 0x80, 0x3f], "lon LE bytes");
        // energy [16..20] = 2.0_f32
        assert_eq!(&buf[16..20], &[0x00, 0x00, 0x00, 0x40], "energy LE bytes");
        // area [20..24] = -1.0_f32
        assert_eq!(&buf[20..24], &[0x00, 0x00, 0x80, 0xbf], "area LE bytes");
        // flash_id [24..28] LE
        assert_eq!(&buf[24..28], &[0x1C, 0x1B, 0x1A, 0x19], "flash_id LE bytes");
        // flags [28..30] LE
        assert_eq!(&buf[28..30], &[0x1E, 0x1D], "flags LE bytes");
        // duration_ms [30..32] LE
        assert_eq!(&buf[30..32], &[0x20, 0x1F], "duration_ms LE bytes");
    }

    #[test]
    fn header_round_trips_and_layout_is_exact() {
        let h = RwlHeader {
            version: 1,
            record_count: 0x0405_0607,
            time_min_unix_ms: 0x0809_0A0B_0C0D_0E0F,
            time_max_unix_ms: 0x1011_1213_1415_1617,
            source_granule_count: 0x1819_1A1B,
        };
        let mut buf = Vec::new();
        h.pack_into(&mut buf);
        assert_eq!(buf.len(), HEADER_LEN);
        assert_eq!(&buf[0..8], MAGIC.as_slice(), "magic");
        assert_eq!(&buf[8..12], &[0x01, 0x00, 0x00, 0x00], "version LE");
        assert_eq!(&buf[12..16], &[0x07, 0x06, 0x05, 0x04], "record_count LE");
        assert_eq!(
            &buf[16..24],
            &[0x0F, 0x0E, 0x0D, 0x0C, 0x0B, 0x0A, 0x09, 0x08],
            "time_min LE"
        );
        assert_eq!(
            &buf[24..32],
            &[0x17, 0x16, 0x15, 0x14, 0x13, 0x12, 0x11, 0x10],
            "time_max LE"
        );
        assert_eq!(
            &buf[32..36],
            &[0x1B, 0x1A, 0x19, 0x18],
            "source_granule_count LE"
        );
        assert_eq!(&buf[36..64], &[0u8; 28], "reserved zeros");

        let h2 = RwlHeader::parse(&buf).unwrap();
        assert_eq!(h2, h);
    }

    #[test]
    fn header_parse_rejects_short_and_bad_magic_and_version() {
        // short
        assert!(matches!(
            RwlHeader::parse(&[0u8; 10]),
            Err(RwlError::Format(_))
        ));
        // bad magic
        let mut bad = vec![0u8; HEADER_LEN];
        bad[0..8].copy_from_slice(b"NOTLIGHT");
        assert!(matches!(RwlHeader::parse(&bad), Err(RwlError::Format(_))));
        // unsupported version
        let mut badver = vec![0u8; HEADER_LEN];
        badver[0..8].copy_from_slice(MAGIC);
        badver[8..12].copy_from_slice(&99u32.to_le_bytes());
        assert!(matches!(
            RwlHeader::parse(&badver),
            Err(RwlError::UnsupportedVersion { found: 99, .. })
        ));
    }

    #[test]
    fn bucket_name_floors_to_ten_minutes() {
        // 2026-01-01 00:00:00 UTC
        let base: i64 = 1_767_225_600_000;
        assert_eq!(bucket_name(base), "t0000.rwl");
        assert_eq!(date_dir(base), "20260101");
        // +9 min 59 s still in t0000
        assert_eq!(bucket_name(base + 9 * 60_000 + 59_000), "t0000.rwl");
        // +10 min exactly -> t0010
        assert_eq!(bucket_name(base + 10 * 60_000), "t0010.rwl");
        // +19 min -> t0010
        assert_eq!(bucket_name(base + 19 * 60_000), "t0010.rwl");
        // +20 min -> t0020
        assert_eq!(bucket_name(base + 20 * 60_000), "t0020.rwl");
    }

    #[test]
    fn bucket_name_last_bucket_of_day_is_t2350() {
        let base: i64 = 1_767_225_600_000; // 2026-01-01 00:00:00 UTC
        // 23:50:00
        let t = base + (23 * 3600 + 50 * 60) * 1000;
        assert_eq!(bucket_name(t), "t2350.rwl");
        assert_eq!(date_dir(t), "20260101");
        // 23:59:59 still t2350
        assert_eq!(bucket_name(t + 9 * 60_000 + 59_000), "t2350.rwl");
    }

    #[test]
    fn day_boundary_rolls_date_and_bucket() {
        let base: i64 = 1_767_225_600_000; // 2026-01-01 00:00:00 UTC
        let next_midnight = base + 86_400_000;
        assert_eq!(date_dir(next_midnight), "20260102");
        assert_eq!(bucket_name(next_midnight), "t0000.rwl");
        // one ms before midnight is the previous day's t2350
        assert_eq!(date_dir(next_midnight - 1), "20260101");
        assert_eq!(bucket_name(next_midnight - 1), "t2350.rwl");
    }

    #[test]
    fn date_dir_known_dates() {
        assert_eq!(date_dir(0), "19700101"); // epoch
        assert_eq!(date_dir(1_767_225_600_000), "20260101");
        // 2024-02-29 (leap day) 12:00:00 UTC = 1709208000 s
        assert_eq!(date_dir(1_709_208_000_000), "20240229");
    }

    #[test]
    fn saturation_clamps() {
        assert_eq!(saturate_duration_ms(400), 400);
        assert_eq!(saturate_duration_ms(65_535), 65_535);
        assert_eq!(saturate_duration_ms(70_000), 65_535);
        assert_eq!(saturate_duration_ms(-5), 0);
    }

    #[test]
    fn bucket_start_aligns() {
        let base: i64 = 1_767_225_600_000;
        assert_eq!(bucket_start_ms(base), base);
        assert_eq!(bucket_start_ms(base + 5 * 60_000), base);
        assert_eq!(bucket_start_ms(base + 10 * 60_000), base + 600_000);
    }
}

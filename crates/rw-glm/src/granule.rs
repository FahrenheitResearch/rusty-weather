//! Decode one GOES GLM **L2 LCFA** NetCDF4 granule into [`Flash`] records.
//!
//! [`decode_granule`] opens a `.nc` granule with the vendored netcrust stack
//! (same pattern as `rw-sat`'s `open_goes_netcdf_lossy`), reads the per-flash
//! variables, and maps each flash into the crate's [`Flash`] value (the same
//! shape that [`crate::FlashRecord`] persists). The mapping was pinned against a
//! real G19 granule in `tests/fixtures/` — see `tests/granule_decode.rs`.
//!
//! ## Product layout (verified against
//! `OR_GLM-L2-LCFA_G19_s20261620805000_…`)
//!
//! - Flash count is the `number_of_flashes` dimension.
//! - `product_time` (F64 scalar) is the granule reference instant in **J2000**
//!   seconds (`units = "seconds since 2000-01-01 12:00:00"`). The J2000 epoch is
//!   `946_728_000` Unix seconds, so `product_unix_ms = round((946_728_000 +
//!   product_time) * 1000)`.
//! - `flash_time_offset_of_first_event` / `…_of_last_event` are scaled i16
//!   *seconds* relative to `product_time` (`scale_factor`/`add_offset` applied
//!   in f64; the encoded values can be slightly negative — the first event may
//!   precede the labelled granule start by a fraction of a second).
//! - `flash_lat` / `flash_lon` are f32 degrees.
//! - `flash_energy` is a scaled i16 in **joules**. Its `scale_factor` is on the
//!   order of `1e-15`, so scale/offset are applied in **f64** and only the final
//!   joule value is narrowed to f32 (the record field) — never the raw int×scale
//!   product in f32, which would lose precision.
//! - `flash_area` is a scaled i16 in **m²**, converted to **km²** (÷ 1e6).
//! - `flash_quality_flag` is an i16 enum; `0` is good, anything else sets the
//!   record's degraded-quality bit.
//!
//! `_FillValue`-flagged flashes (raw value equal to the variable's `_FillValue`,
//! e.g. `-1` for the scaled shorts) are dropped: a fill flash has no real
//! position/energy and must not pollute the store.

use std::path::Path;

use netcrust::File as NcFile;

use crate::error::{RwlError, RwlResult};
use crate::format::{FLAG_DEGRADED_QUALITY, saturate_duration_ms};
use crate::reader::Flash;

/// J2000 epoch (2000-01-01 12:00:00 UTC) expressed in Unix seconds. GLM
/// `product_time` is measured from this instant.
pub const J2000_EPOCH_UNIX_S: f64 = 946_728_000.0;

/// The dimension that gives a granule's flash count.
const DIM_FLASHES: &str = "number_of_flashes";

/// One decoded GLM granule: the flashes plus enough provenance for the follow
/// engine (Task 3) to dedup and route them.
#[derive(Debug, Clone)]
pub struct DecodedGranule {
    /// Satellite hint parsed from the granule's `platform_ID` global attribute
    /// (e.g. `"G19"`), if present. `None` when the attribute is absent.
    pub satellite: Option<String>,
    /// Dedup key for Task 3: the granule filename stem (no directory, no
    /// extension), e.g. `OR_GLM-L2-LCFA_G19_s…_e…_c…`.
    pub granule_key: String,
    /// Every flash decoded from the granule, in file order.
    pub flashes: Vec<Flash>,
}

/// Decode a GLM L2 LCFA granule at `path` into a [`DecodedGranule`].
///
/// Returns [`RwlError::Format`] for any structural problem — a file that is not
/// a readable NetCDF, a missing required variable/dimension, or a length
/// mismatch between the per-flash arrays. Never panics on a malformed or
/// non-GLM file.
pub fn decode_granule(path: &Path) -> RwlResult<DecodedGranule> {
    let granule_key = granule_key_from_path(path)?;

    let options = netcrust::NcOpenOptions {
        metadata_mode: netcrust::NcMetadataMode::Lossy,
        ..Default::default()
    };
    let file = NcFile::open_with_options(path, options)
        .map_err(|e| RwlError::Format(format!("{}: not a readable NetCDF: {e}", path.display())))?;

    let satellite = file
        .attribute("platform_ID")
        .and_then(|a| a.as_string().map(str::to_string));

    // Flash count from the dimension. Its absence means this is not a GLM LCFA
    // granule (or an empty/garbage file) — a clean Format error, not a panic.
    let n = file
        .dimension(DIM_FLASHES)
        .ok_or_else(|| {
            RwlError::Format(format!(
                "{}: missing `{DIM_FLASHES}` dimension (not a GLM L2 LCFA granule?)",
                path.display()
            ))
        })?
        .len();

    // Zero flashes is valid — a quiet granule.
    if n == 0 {
        return Ok(DecodedGranule {
            satellite,
            granule_key,
            flashes: Vec::new(),
        });
    }

    // Reference instant: product_time (J2000 seconds) -> Unix ms.
    let product_time = read_scalar_f64(&file, "product_time", path)?;
    let product_unix_ms = ((J2000_EPOCH_UNIX_S + product_time) * 1000.0).round() as i64;

    // Per-flash arrays. Each must have exactly `n` values.
    let flash_id = read_raw_f64(&file, "flash_id", n, path)?;
    let lat = read_raw_f64(&file, "flash_lat", n, path)?;
    let lon = read_raw_f64(&file, "flash_lon", n, path)?;
    let quality = read_raw_f64(&file, "flash_quality_flag", n, path)?;
    let first_off = read_scaled(&file, "flash_time_offset_of_first_event", n, path)?;
    let last_off = read_scaled(&file, "flash_time_offset_of_last_event", n, path)?;
    let energy = read_scaled(&file, "flash_energy", n, path)?;
    let area = read_scaled(&file, "flash_area", n, path)?;

    let mut flashes = Vec::with_capacity(n);
    for i in 0..n {
        // Drop fill-valued flashes: a scaled field decodes to NaN at a
        // _FillValue, so any NaN in a required field means "no real flash here".
        let first_s = first_off[i];
        let last_s = last_off[i];
        let energy_j = energy[i];
        let area_m2 = area[i];
        if !first_s.is_finite() || !energy_j.is_finite() || !area_m2.is_finite() {
            continue;
        }

        // Absolute first-event time, Unix ms. The offsets are seconds relative
        // to product_time.
        let time_unix_ms = product_unix_ms + (first_s * 1000.0).round() as i64;

        // Duration = (last - first) seconds -> ms, saturating into u16. A
        // non-finite last offset (fill) clamps to 0 via the i64 floor below.
        let duration_ms = if last_s.is_finite() {
            saturate_duration_ms(((last_s - first_s) * 1000.0).round() as i64)
        } else {
            0
        };

        // Degraded-quality bit: anything but the nominal value (0) is degraded.
        let flags = if quality[i] != 0.0 {
            FLAG_DEGRADED_QUALITY
        } else {
            0
        };

        flashes.push(Flash {
            time_unix_ms,
            lat: lat[i] as f32,
            lon: lon[i] as f32,
            // energy_j is already a precise f64 joule value; narrow once.
            energy: energy_j as f32,
            // m² -> km².
            area: (area_m2 / 1.0e6) as f32,
            flash_id: flash_id[i] as u32,
            flags,
            duration_ms,
        });
    }

    Ok(DecodedGranule {
        satellite,
        granule_key,
        flashes,
    })
}

/// The granule filename stem (no directory, no extension) — Task 3's dedup key.
fn granule_key_from_path(path: &Path) -> RwlResult<String> {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| RwlError::Format(format!("{}: path has no file name", path.display())))
}

/// Read a required scalar f64 variable (e.g. `product_time`).
fn read_scalar_f64(file: &NcFile, name: &str, path: &Path) -> RwlResult<f64> {
    let values = file
        .read_f64(name)
        .map_err(|e| RwlError::Format(format!("{}: reading `{name}`: {e}", path.display())))?;
    values
        .first()
        .copied()
        .ok_or_else(|| RwlError::Format(format!("{}: `{name}` is empty", path.display())))
}

/// Read a required per-flash variable as **raw** f64 values (no scale/offset),
/// asserting it has exactly `n` elements. Used for the unscaled fields
/// (`flash_id`, `flash_lat`, `flash_lon`, `flash_quality_flag`).
fn read_raw_f64(file: &NcFile, name: &str, n: usize, path: &Path) -> RwlResult<Vec<f64>> {
    let values = file
        .read_f64(name)
        .map_err(|e| RwlError::Format(format!("{}: reading `{name}`: {e}", path.display())))?;
    if values.len() != n {
        return Err(RwlError::Format(format!(
            "{}: `{name}` has {} values, expected {n}",
            path.display(),
            values.len()
        )));
    }
    Ok(values)
}

/// Read a required per-flash **scaled** variable and apply `scale_factor` /
/// `add_offset` in **f64**, returning `n` decoded f64 values. A value equal to
/// the variable's `_FillValue` (compared on the *raw* integer, before scaling)
/// decodes to `NaN` so the caller can drop fill flashes.
///
/// Applying scale/offset in f64 (not via rw-sat's `read_scaled_f32`, which
/// narrows mid-computation) preserves the precision of `flash_energy`, whose
/// `scale_factor` is ~`1e-15`.
fn read_scaled(file: &NcFile, name: &str, n: usize, path: &Path) -> RwlResult<Vec<f64>> {
    let variable = file.variable(name).ok_or_else(|| {
        RwlError::Format(format!("{}: missing variable `{name}`", path.display()))
    })?;
    let scale = variable
        .attribute("scale_factor")
        .and_then(|a| a.as_f64())
        .unwrap_or(1.0);
    let offset = variable
        .attribute("add_offset")
        .and_then(|a| a.as_f64())
        .unwrap_or(0.0);
    let fill = variable.attribute("_FillValue").and_then(|a| a.as_f64());

    let raw = variable
        .values_f64()
        .map_err(|e| RwlError::Format(format!("{}: reading `{name}`: {e}", path.display())))?;
    if raw.len() != n {
        return Err(RwlError::Format(format!(
            "{}: `{name}` has {} values, expected {n}",
            path.display(),
            raw.len()
        )));
    }

    Ok(raw
        .into_iter()
        .map(|r| {
            // Compare against _FillValue on the raw integer (exact for the
            // small i16 fills GLM uses). NaN signals "drop this flash".
            if !r.is_finite() || fill.is_some_and(|f| (r - f).abs() < 0.5) {
                f64::NAN
            } else {
                r * scale + offset
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_saturation_logic() {
        // (last - first) in ms, saturating into u16.
        // Normal sub-second flash.
        assert_eq!(saturate_duration_ms(34), 34);
        // Exactly the u16 ceiling.
        assert_eq!(saturate_duration_ms(65_535), 65_535);
        // A long flash beyond u16 saturates, not wraps.
        assert_eq!(saturate_duration_ms(120_000), 65_535);
        // last before first (malformed) clamps to 0, not a huge unsigned wrap.
        assert_eq!(saturate_duration_ms(-50), 0);
    }

    #[test]
    fn j2000_epoch_is_the_known_constant() {
        // 2000-01-01 12:00:00 UTC == 946_728_000 Unix seconds.
        assert_eq!(J2000_EPOCH_UNIX_S, 946_728_000.0);
    }
}

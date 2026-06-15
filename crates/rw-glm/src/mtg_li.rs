//! Decode EUMETSAT MTG Lightning Imager L2 flash body NetCDF files into the
//! shared `.rwl` flash-event shape.
//!
//! The LI L2 flash product is not byte-for-byte GOES GLM, but its body chunk
//! has the point-event fields the store needs for density products:
//! `flash_time`, scaled `latitude` / `longitude`, `radiance`,
//! `flash_footprint`, `flash_duration`, and `flash_id`.

use std::path::Path;

use netcrust::File as NcFile;

use crate::error::{RwlError, RwlResult};
use crate::format::saturate_duration_ms;
use crate::reader::Flash;

/// MTG LI `flash_time` is measured from 2000-01-01 00:00:00 UTC.
const LI_EPOCH_UNIX_S: f64 = 946_684_800.0;

#[derive(Debug, Clone)]
pub struct DecodedMtgLiProduct {
    pub product_key: String,
    pub satellite: Option<String>,
    pub time_coverage_start: Option<String>,
    pub time_coverage_end: Option<String>,
    pub flashes: Vec<Flash>,
}

pub fn decode_mtg_li_flashes(path: &Path) -> RwlResult<DecodedMtgLiProduct> {
    let product_key = product_key_from_path(path)?;
    let options = netcrust::NcOpenOptions {
        metadata_mode: netcrust::NcMetadataMode::Lossy,
        ..Default::default()
    };
    let file = NcFile::open_with_options(path, options)
        .map_err(|e| RwlError::Format(format!("{}: not a readable NetCDF: {e}", path.display())))?;

    let flash_id = read_raw(&file, "flash_id", path)?;
    let n = flash_id.len();
    let flash_time = read_raw_exact(&file, "flash_time", n, path)?;
    let latitude = read_scaled_exact(&file, "latitude", n, path)?;
    let longitude = read_scaled_exact(&file, "longitude", n, path)?;
    let radiance = read_scaled_exact(&file, "radiance", n, path)?;
    let footprint = read_raw_exact(&file, "flash_footprint", n, path)?;
    let duration = read_raw_exact(&file, "flash_duration", n, path)?;

    let mut flashes = Vec::with_capacity(n);
    for i in 0..n {
        let time_s = flash_time[i];
        let lat = latitude[i];
        let lon = longitude[i];
        if !(time_s.is_finite() && lat.is_finite() && lon.is_finite()) {
            continue;
        }
        flashes.push(Flash {
            time_unix_ms: ((LI_EPOCH_UNIX_S + time_s) * 1000.0).round() as i64,
            lat: lat as f32,
            lon: lon as f32,
            // `.rwl` has a provider-neutral f32 payload slot named energy.
            // For MTG LI this stores the product's flash radiance proxy.
            energy: radiance[i] as f32,
            area: footprint[i] as f32,
            flash_id: flash_id[i] as u32,
            flags: 0,
            duration_ms: saturate_duration_ms(duration[i].round() as i64),
        });
    }

    Ok(DecodedMtgLiProduct {
        product_key,
        satellite: infer_satellite(&file),
        time_coverage_start: file
            .attribute("time_coverage_start")
            .and_then(|attr| attr.as_string().map(str::to_string)),
        time_coverage_end: file
            .attribute("time_coverage_end")
            .and_then(|attr| attr.as_string().map(str::to_string)),
        flashes,
    })
}

fn product_key_from_path(path: &Path) -> RwlResult<String> {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .ok_or_else(|| RwlError::Format(format!("{}: path has no file name", path.display())))
}

fn infer_satellite(file: &NcFile) -> Option<String> {
    file.attribute("platform")
        .or_else(|| file.attribute("platform_ID"))
        .or_else(|| file.attribute("spacecraft"))
        .and_then(|attr| attr.as_string().map(str::to_string))
}

fn read_raw(file: &NcFile, name: &str, path: &Path) -> RwlResult<Vec<f64>> {
    let variable = file.variable(name).ok_or_else(|| {
        RwlError::Format(format!("{}: missing variable `{name}`", path.display()))
    })?;
    let fill = variable
        .attribute("_FillValue")
        .and_then(|attr| attr.as_f64());
    let raw = variable
        .values_f64()
        .map_err(|e| RwlError::Format(format!("{}: reading `{name}`: {e}", path.display())))?;
    Ok(raw
        .into_iter()
        .map(|value| {
            if !value.is_finite() || fill.is_some_and(|fill| (value - fill).abs() < 0.5) {
                f64::NAN
            } else {
                value
            }
        })
        .collect())
}

fn read_raw_exact(file: &NcFile, name: &str, n: usize, path: &Path) -> RwlResult<Vec<f64>> {
    let values = read_raw(file, name, path)?;
    if values.len() != n {
        return Err(RwlError::Format(format!(
            "{}: `{name}` has {} values, expected {n}",
            path.display(),
            values.len()
        )));
    }
    Ok(values)
}

fn read_scaled_exact(file: &NcFile, name: &str, n: usize, path: &Path) -> RwlResult<Vec<f64>> {
    let variable = file.variable(name).ok_or_else(|| {
        RwlError::Format(format!("{}: missing variable `{name}`", path.display()))
    })?;
    let scale = variable
        .attribute("scale_factor")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(1.0);
    let offset = variable
        .attribute("add_offset")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(0.0);
    let fill = variable
        .attribute("_FillValue")
        .and_then(|attr| attr.as_f64());
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
        .map(|value| {
            if !value.is_finite() || fill.is_some_and(|fill| (value - fill).abs() < 0.5) {
                f64::NAN
            } else {
                value * scale + offset
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn li_epoch_is_midnight_2000() {
        assert_eq!(LI_EPOCH_UNIX_S, 946_684_800.0);
    }
}

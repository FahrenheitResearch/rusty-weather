//! Build isobaric sounding volumes from a WRF file.
//!
//! WRF is on native (eta) levels, but the skew-T builder
//! ([`rw_ui::skewt::build_sounding_column`]) needs the same `*_iso` isobaric
//! 3D variables the model ingest writes for HRRR/GFS: `temperature_iso`,
//! `dewpoint_iso`, `u_iso`, `v_iso`, `height_iso`. This module reads WRF's 3D
//! fields through `wrf-core`'s `getvar` (which already handles destaggering,
//! theta -> T, geopotential -> height, and QVAPOR -> Td) and log-pressure
//! interpolates each column onto the canonical isobaric levels, so imported
//! WRF runs produce soundings exactly like the downloaded models do.

use rw_store::PressureVolumeInput;
use wrf_core::{ComputeOpts, VarOutput, WrfFile, getvar};

/// Canonical isobaric levels (hPa), matching the model-ingest convention
/// (`100..=1000` step 25 -> 37 levels). Levels outside a column's model range
/// are left NaN and pruned by the sounding column builder.
fn standard_levels() -> Vec<u16> {
    (100..=1000u16).step_by(25).collect()
}

/// One isobaric volume ready for the store writer: owned row-major planes.
pub struct IsoVolume {
    pub name: String,
    pub units: String,
    /// `(level_hpa, plane)` where each plane holds `ny * nx` row-major values.
    pub levels: Vec<(u16, Vec<f32>)>,
}

impl IsoVolume {
    /// Borrowed view for the store writer's [`PressureVolumeInput`].
    pub fn as_input(&self) -> PressureVolumeInput<'_> {
        PressureVolumeInput {
            name: &self.name,
            units: &self.units,
            selector_template: serde_json::json!({
                "source": "wrf",
                "field": self.name,
                "vertical": "isobaric",
            }),
            levels: self
                .levels
                .iter()
                .map(|(hpa, plane)| (*hpa, plane.as_slice()))
                .collect(),
        }
    }
}

/// Read WRF 3D fields for `timeidx` and interpolate them to the canonical
/// isobaric levels, returning the five `*_iso` volumes the skew-T needs.
///
/// `cells` is the horizontal grid size (`ny * nx`) of the hour being written;
/// every returned plane matches it. Fails (leaving the caller to skip volumes
/// and still write the 2D fields) if the required 3D fields are unreadable.
pub fn build_iso_volumes(
    file: &WrfFile,
    timeidx: usize,
    cells: usize,
) -> Result<Vec<IsoVolume>, String> {
    if cells == 0 {
        return Err("WRF grid has zero cells".to_string());
    }
    let read = |name: &str| -> Result<VarOutput, String> {
        getvar(file, name, Some(timeidx), &ComputeOpts::default())
            .map_err(|err| format!("read WRF {name}: {err}"))
    };

    let pressure = read("pressure")?; // hPa, [nz, ny, nx]
    let nz = pressure.data.len() / cells;
    if nz < 2 || nz * cells != pressure.data.len() {
        return Err(format!(
            "WRF pressure field has {} values, not a whole number of {cells}-cell levels",
            pressure.data.len()
        ));
    }

    let temp = read("temp")?; // K
    let td = read("td")?; // degC
    let height = read("height")?; // m MSL
    check_len(&temp, nz * cells, "temp")?;
    check_len(&td, nz * cells, "td")?;
    check_len(&height, nz * cells, "height")?;

    // Earth-relative winds. `uvmet` returns [u_earth.., v_earth..]
    // (2 * nz * cells); fall back to grid-relative ua/va if it is unavailable
    // or the interleaved layout is unexpected.
    let (u_wind, v_wind) = match read("uvmet") {
        Ok(uvmet) if uvmet.data.len() == 2 * nz * cells => {
            let (u, v) = uvmet.data.split_at(nz * cells);
            (u.to_vec(), v.to_vec())
        }
        _ => {
            let ua = read("ua")?;
            let va = read("va")?;
            check_len(&ua, nz * cells, "ua")?;
            check_len(&va, nz * cells, "va")?;
            (ua.data, va.data)
        }
    };

    let levels = standard_levels();
    let mut temp_iso = init_planes(levels.len(), cells);
    let mut dewp_iso = init_planes(levels.len(), cells);
    let mut u_iso = init_planes(levels.len(), cells);
    let mut v_iso = init_planes(levels.len(), cells);
    let mut hgt_iso = init_planes(levels.len(), cells);

    let mut col_p = vec![0f64; nz];
    for c in 0..cells {
        for k in 0..nz {
            col_p[k] = pressure.data[k * cells + c];
        }
        for (li, &lev) in levels.iter().enumerate() {
            let Some((k, t)) = bracket(&col_p, f64::from(lev)) else {
                continue;
            };
            let (i0, i1) = (k * cells + c, (k + 1) * cells + c);
            if let Some(value) = lerp(temp.data[i0], temp.data[i1], t) {
                temp_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(td.data[i0], td.data[i1], t) {
                dewp_iso[li][c] = (value + 273.15) as f32;
            }
            if let Some(value) = lerp(u_wind[i0], u_wind[i1], t) {
                u_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(v_wind[i0], v_wind[i1], t) {
                v_iso[li][c] = value as f32;
            }
            if let Some(value) = lerp(height.data[i0], height.data[i1], t) {
                hgt_iso[li][c] = value as f32;
            }
        }
    }

    Ok(vec![
        IsoVolume {
            name: "temperature_iso".to_string(),
            units: "K".to_string(),
            levels: pack(&levels, temp_iso),
        },
        IsoVolume {
            name: "dewpoint_iso".to_string(),
            units: "K".to_string(),
            levels: pack(&levels, dewp_iso),
        },
        IsoVolume {
            name: "u_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, u_iso),
        },
        IsoVolume {
            name: "v_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(&levels, v_iso),
        },
        IsoVolume {
            name: "height_iso".to_string(),
            units: "gpm".to_string(),
            levels: pack(&levels, hgt_iso),
        },
    ])
}

fn check_len(out: &VarOutput, expected: usize, name: &str) -> Result<(), String> {
    if out.data.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "WRF {name} has {} values, expected {expected}",
            out.data.len()
        ))
    }
}

fn init_planes(levels: usize, cells: usize) -> Vec<Vec<f32>> {
    vec![vec![f32::NAN; cells]; levels]
}

fn pack(levels: &[u16], planes: Vec<Vec<f32>>) -> Vec<(u16, Vec<f32>)> {
    levels.iter().copied().zip(planes).collect()
}

/// Locate the native levels bracketing `target` hPa in a WRF column (pressure
/// decreasing with index, level 0 nearest the surface) and return the lower
/// level index plus the log-pressure interpolation weight. `None` when the
/// target sits below the lowest level or above the model top.
fn bracket(col_p: &[f64], target: f64) -> Option<(usize, f64)> {
    for k in 0..col_p.len().saturating_sub(1) {
        let (pk, pk1) = (col_p[k], col_p[k + 1]);
        if !pk.is_finite() || !pk1.is_finite() || pk == pk1 {
            continue;
        }
        let (hi, lo) = if pk >= pk1 { (pk, pk1) } else { (pk1, pk) };
        if target <= hi && target >= lo {
            let t = (target.ln() - pk.ln()) / (pk1.ln() - pk.ln());
            return Some((k, t));
        }
    }
    None
}

fn lerp(a: f64, b: f64, t: f64) -> Option<f64> {
    (a.is_finite() && b.is_finite()).then(|| a + t * (b - a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_levels_span_the_isobaric_ladder() {
        let levels = standard_levels();
        assert_eq!(levels.len(), 37);
        assert_eq!(*levels.first().unwrap(), 100);
        assert_eq!(*levels.last().unwrap(), 1000);
    }

    #[test]
    fn bracket_interpolates_in_log_pressure_and_clamps_to_range() {
        // Decreasing pressure with index (level 0 nearest the surface).
        let col = [1000.0, 850.0, 700.0, 500.0];
        // Midway between 1000 and 850 in ln-p.
        let (k, t) = bracket(&col, 925.0).expect("in range");
        assert_eq!(k, 0);
        let expected = (925f64.ln() - 1000f64.ln()) / (850f64.ln() - 1000f64.ln());
        assert!((t - expected).abs() < 1e-9);
        // Below the lowest level and above the top are both out of range.
        assert!(bracket(&col, 1013.0).is_none());
        assert!(bracket(&col, 300.0).is_none());
    }

    #[test]
    fn lerp_skips_non_finite_endpoints() {
        assert_eq!(lerp(0.0, 10.0, 0.5), Some(5.0));
        assert_eq!(lerp(f64::NAN, 10.0, 0.5), None);
        assert_eq!(lerp(0.0, f64::NAN, 0.5), None);
    }
}

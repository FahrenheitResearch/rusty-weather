//! Bridge from rw-store sounding data to the production sounding renderer.
//!
//! [`build_native_sounding`] turns a [`SoundingData`] (per-level profiles +
//! surface point samples from the [`crate::StoreWorker`]) into a
//! `rustwx_sounding::NativeSounding` — the rustwx-owned skew-T / hodograph /
//! parameter-table renderer riding on sharprs, with ecape-rs-verified
//! values. [`render_sounding_image`] then rasterizes it to an
//! [`egui::ColorImage`] ready for texture upload.
//!
//! Column construction mirrors the production convention
//! (`wxprofile_sounding_render`): the column starts at the MODEL SURFACE
//! (psfc / orography / 2 m T+Td / 10 m wind), and isobaric levels are
//! pruned when they sit at or below it — `pressure >= psfc - 0.1 hPa` or
//! `height <= orography + 1 m` — so below-ground HRRR levels never reach
//! the renderer. Pressure must then decrease (and height increase) strictly
//! level over level; offending levels are skipped, dewpoint is clamped to
//! the temperature, and non-finite levels are dropped.

use egui::ColorImage;
use rustwx_sounding::{NativeSounding, SoundingColumn, SoundingMetadata};

use crate::worker::{ProfileVar, SoundingData};

/// 3D store variables the skew-T needs, in the units the ingest writes
/// (K, K, m/s, m/s, gpm).
const PROFILE_VARS: [&str; 5] = [
    "temperature_iso",
    "dewpoint_iso",
    "u_iso",
    "v_iso",
    "height_iso",
];

/// Surface samples the skew-T needs (K, K, m/s, m/s, Pa, gpm).
const SURFACE_VARS: [&str; 6] = [
    "temperature_2m",
    "dewpoint_2m",
    "u_10m",
    "v_10m",
    "surface_pressure",
    "orography",
];

/// Below-ground pruning epsilons — same values as the production
/// `wxprofile_sounding_render` column builder.
const PRESSURE_EPSILON_HPA: f64 = 0.1;
const HEIGHT_EPSILON_M: f64 = 1.0;

/// Build the production sounding (sharprs profile + computed parameter
/// table + verified ECAPE) from one worker [`SoundingData`].
pub fn build_native_sounding(data: &SoundingData) -> Result<NativeSounding, String> {
    let column = build_sounding_column(data)?;
    NativeSounding::from_column(&column).map_err(|err| err.to_string())
}

/// Render `native` to an RGBA [`ColorImage`] via the bridge's PNG renderer.
pub fn render_sounding_image(native: &NativeSounding) -> Result<ColorImage, String> {
    png_to_color_image(&native.render_full_png())
}

/// Assemble the `SoundingColumn` (the `rustwx-sounding` bridge input
/// contract: surface-first descending pressure, ascending height, °C, m/s)
/// from store-native profile + surface data. See the module docs for the
/// below-ground convention.
pub fn build_sounding_column(data: &SoundingData) -> Result<SoundingColumn, String> {
    let missing: Vec<&str> = PROFILE_VARS
        .iter()
        .copied()
        .filter(|name| !data.vars.iter().any(|var| var.name == *name))
        .chain(
            SURFACE_VARS
                .iter()
                .copied()
                .filter(|name| data.surface_value(name).is_none()),
        )
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "store hour lacks skew-T inputs: {}",
            missing.join(", ")
        ));
    }

    let surface_value = |name: &str| -> Result<f64, String> {
        let sample = data.surface_value(name).expect("checked above");
        let value = f64::from(sample.value);
        if !value.is_finite() {
            return Err(format!("surface sample {name} is missing at this point"));
        }
        Ok(match (name, sample.units.as_str()) {
            (_, "K") => value - 273.15,
            ("surface_pressure", "Pa") => value / 100.0,
            _ => value,
        })
    };
    let t2_c = surface_value("temperature_2m")?;
    let td2_c = surface_value("dewpoint_2m")?;
    let u10_ms = surface_value("u_10m")?;
    let v10_ms = surface_value("v_10m")?;
    let psfc_hpa = surface_value("surface_pressure")?;
    let orog_m = surface_value("orography")?;

    let profile = |name: &str| data.vars.iter().find(|var| var.name == name).unwrap();
    let temperature = profile("temperature_iso");
    let dewpoint = profile("dewpoint_iso");
    let u_wind = profile("u_iso");
    let v_wind = profile("v_iso");
    let height = profile("height_iso");

    let mut column = SoundingColumn {
        pressure_hpa: Vec::new(),
        height_m_msl: Vec::new(),
        temperature_c: Vec::new(),
        dewpoint_c: Vec::new(),
        u_ms: Vec::new(),
        v_ms: Vec::new(),
        omega_pa_s: Vec::new(),
        metadata: metadata_for(data, orog_m),
    };

    // Level 0: the model surface.
    push_level(&mut column, psfc_hpa, orog_m, t2_c, td2_c, u10_ms, v10_ms);

    // Isobaric levels, low to high (the store's levels are descending).
    for &level_hpa in &temperature.levels_hpa {
        let pressure_hpa = f64::from(level_hpa);
        let Some(t_c) = level_value(temperature, level_hpa) else {
            continue;
        };
        let Some(td_c) = level_value(dewpoint, level_hpa) else {
            continue;
        };
        let Some(u_ms) = level_value(u_wind, level_hpa) else {
            continue;
        };
        let Some(v_ms) = level_value(v_wind, level_hpa) else {
            continue;
        };
        let Some(height_m) = level_value(height, level_hpa) else {
            continue;
        };
        // Below-ground pruning (production convention, see module docs).
        if pressure_hpa >= psfc_hpa - PRESSURE_EPSILON_HPA || height_m <= orog_m + HEIGHT_EPSILON_M
        {
            continue;
        }
        push_level(&mut column, pressure_hpa, height_m, t_c, td_c, u_ms, v_ms);
    }

    if column.len() < 2 {
        return Err(format!(
            "only {} usable level(s) above the surface ({psfc_hpa:.0} hPa)",
            column.len()
        ));
    }
    column.validate().map_err(|err| err.to_string())?;
    Ok(column)
}

/// One profile value, converted to bridge units; `None` for absent levels
/// and non-finite values (both are skipped, mirroring production).
fn level_value(var: &ProfileVar, level_hpa: u16) -> Option<f64> {
    let index = var.levels_hpa.iter().position(|&have| have == level_hpa)?;
    let value = f64::from(*var.values.get(index)?);
    if !value.is_finite() {
        return None;
    }
    Some(if var.units == "K" {
        value - 273.15
    } else {
        value
    })
}

/// Append a level iff it keeps pressure strictly decreasing and height
/// strictly increasing (the bridge's monotonicity contract); dewpoint is
/// clamped to the temperature. Same guard as the production builder.
fn push_level(
    column: &mut SoundingColumn,
    pressure_hpa: f64,
    height_m_msl: f64,
    temperature_c: f64,
    dewpoint_c: f64,
    u_ms: f64,
    v_ms: f64,
) {
    if !(pressure_hpa.is_finite()
        && height_m_msl.is_finite()
        && temperature_c.is_finite()
        && dewpoint_c.is_finite()
        && u_ms.is_finite()
        && v_ms.is_finite())
    {
        return;
    }
    if let (Some(&last_p), Some(&last_z)) = (column.pressure_hpa.last(), column.height_m_msl.last())
    {
        if pressure_hpa >= last_p - 1.0e-6 || height_m_msl <= last_z + 1.0e-6 {
            return;
        }
    }
    column.pressure_hpa.push(pressure_hpa);
    column.height_m_msl.push(height_m_msl);
    column.temperature_c.push(temperature_c);
    column.dewpoint_c.push(dewpoint_c.min(temperature_c));
    column.u_ms.push(u_ms);
    column.v_ms.push(v_ms);
}

/// Title metadata: station = "HRRR 30.34N 89.79W", valid = "20260608 00z
/// F006" — the renderer's title bar shows
/// "rustwx Sounding Analysis - {station} - {valid}".
fn metadata_for(data: &SoundingData, orog_m: f64) -> SoundingMetadata {
    let model = data.hour.model.to_uppercase();
    let station_id = match (data.lat, data.lon) {
        (Some(lat), Some(lon)) => format!("{model} {}", format_latlon(lat, lon)),
        _ => format!("{model} grid {:.1},{:.1}", data.fx, data.fy),
    };
    SoundingMetadata {
        station_id,
        valid_time: format!("{} F{:03}", data.hour.run.replace('_', " "), data.hour.hour),
        latitude_deg: data.lat.map(f64::from),
        longitude_deg: data.lon.map(f64::from).map(normalize_lon),
        elevation_m: Some(orog_m),
        sample_method: Some("rw_store_point".to_string()),
        box_radius_lat_deg: None,
        box_radius_lon_deg: None,
    }
}

/// "30.34N 89.79W"-style location label.
fn format_latlon(lat: f32, lon: f32) -> String {
    let lon = normalize_lon(f64::from(lon));
    let ns = if lat >= 0.0 { 'N' } else { 'S' };
    let ew = if lon >= 0.0 { 'E' } else { 'W' };
    format!("{:.2}{ns} {:.2}{ew}", lat.abs(), lon.abs())
}

fn normalize_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}

fn png_to_color_image(png: &[u8]) -> Result<ColorImage, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|err| format!("decode sounding png: {err}"))?
        .to_rgba8();
    let size = [image.width() as usize, image.height() as usize];
    Ok(ColorImage::from_rgba_unmultiplied(size, image.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::{HourKey, SurfaceSample};

    /// Synthetic Gulf-coast-ish sounding whose two lowest isobaric levels
    /// (1000 and 975 hPa) sit below the model surface (psfc 962 hPa).
    fn sample_data() -> SoundingData {
        let levels: Vec<u16> = vec![1000, 975, 950, 925, 850, 700, 500, 300, 250];
        let surface = |name: &str, units: &str, value: f32| SurfaceSample {
            name: name.to_string(),
            units: units.to_string(),
            value,
        };
        let var = |name: &str, units: &str, values: &[f32]| ProfileVar {
            name: name.to_string(),
            units: units.to_string(),
            levels_hpa: levels.clone(),
            values: values.to_vec(),
        };
        SoundingData {
            hour: HourKey {
                model: "hrrr".to_string(),
                run: "20260608_00z".to_string(),
                hour: 6,
            },
            fx: 100.0,
            fy: 200.0,
            lat: Some(30.34),
            lon: Some(-89.79),
            vars: vec![
                var(
                    "temperature_iso",
                    "K",
                    &[
                        303.15, 301.15, 299.15, 297.15, 290.15, 277.15, 258.15, 233.15, 225.15,
                    ],
                ),
                var(
                    "dewpoint_iso",
                    "K",
                    &[
                        296.15, 295.15, 294.15, 293.15, 285.15, 268.15, 243.15, 213.15, 205.15,
                    ],
                ),
                var(
                    "u_iso",
                    "m/s",
                    &[2.0, 3.0, 4.0, 5.0, 8.0, 14.0, 25.0, 35.0, 38.0],
                ),
                var(
                    "v_iso",
                    "m/s",
                    &[5.0, 6.0, 7.0, 7.5, 8.0, 6.0, 2.0, -4.0, -6.0],
                ),
                var(
                    "height_iso",
                    "gpm",
                    &[
                        110.0, 330.0, 550.0, 780.0, 1500.0, 3100.0, 5800.0, 9600.0, 10900.0,
                    ],
                ),
            ],
            surface: vec![
                surface("temperature_2m", "K", 302.15),
                surface("dewpoint_2m", "K", 297.15),
                surface("u_10m", "m/s", 1.5),
                surface("v_10m", "m/s", 4.0),
                surface("surface_pressure", "Pa", 96_200.0),
                surface("orography", "gpm", 420.0),
            ],
            read_ms: 0.0,
        }
    }

    #[test]
    fn column_prunes_below_ground_levels_and_starts_at_the_surface() {
        let column = build_sounding_column(&sample_data()).expect("column should build");

        // Surface first; 1000 hPa (>= psfc) and 975 hPa (height 330 m below
        // orography 420 m) are below ground and pruned.
        assert_eq!(
            column.pressure_hpa,
            vec![962.0, 950.0, 925.0, 850.0, 700.0, 500.0, 300.0, 250.0]
        );
        assert_eq!(column.height_m_msl[0], 420.0, "surface at the orography");
        assert!(
            column.height_m_msl.windows(2).all(|w| w[0] < w[1]),
            "heights strictly increasing: {:?}",
            column.height_m_msl
        );

        // Unit conversions: K -> C, Pa -> hPa; winds pass through in m/s.
        // (f32 store values: compare at f32 precision.)
        assert!((column.temperature_c[0] - 29.0).abs() < 1e-3, "2 m T in C");
        assert!((column.dewpoint_c[0] - 24.0).abs() < 1e-3, "2 m Td in C");
        assert!(
            (column.temperature_c[1] - 26.0).abs() < 1e-3,
            "950 hPa T in C"
        );
        assert_eq!(column.u_ms[0], 1.5);
        assert_eq!(column.v_ms[0], 4.0);

        // Metadata feeds the renderer's title + locator.
        assert_eq!(column.metadata.station_id, "HRRR 30.34N 89.79W");
        assert_eq!(column.metadata.valid_time, "20260608 00z F006");
        assert_eq!(column.metadata.elevation_m, Some(420.0));

        // And the bridge accepts it.
        build_native_sounding(&sample_data()).expect("native sounding should build");
    }

    #[test]
    fn saturated_levels_clamp_dewpoint_to_temperature() {
        let mut data = sample_data();
        // Push 2 m dewpoint above 2 m temperature (supersaturated sample).
        data.surface[1].value = data.surface[0].value + 1.0;
        let column = build_sounding_column(&data).expect("clamped column should build");
        assert_eq!(column.dewpoint_c[0], column.temperature_c[0]);
    }

    #[test]
    fn missing_inputs_are_reported_by_name() {
        let mut data = sample_data();
        data.vars.retain(|var| var.name != "height_iso");
        data.surface
            .retain(|sample| sample.name != "surface_pressure");
        let error = build_sounding_column(&data).expect_err("missing inputs must fail");
        assert!(error.contains("height_iso"), "got: {error}");
        assert!(error.contains("surface_pressure"), "got: {error}");
    }

    #[test]
    fn nan_levels_are_skipped_not_fatal() {
        let mut data = sample_data();
        // 850 hPa temperature missing at this point: skip that level only.
        data.vars[0].values[4] = f32::NAN;
        let column = build_sounding_column(&data).expect("column should build");
        assert!(!column.pressure_hpa.contains(&850.0));
        assert!(column.pressure_hpa.contains(&700.0));
    }

    /// Full render through sharprs + the native parameter table. Slowish
    /// (parcel + ECAPE math and a 2400x1800 raster) but a real guard that
    /// the bridge output draws a non-trivial image.
    #[test]
    fn renders_non_trivial_skewt_image() {
        let native = build_native_sounding(&sample_data()).expect("native sounding");
        let image = render_sounding_image(&native).expect("render should succeed");
        assert!(
            image.width() >= 1000 && image.height() >= 800,
            "{:?}",
            image.size
        );
        let background = image.pixels[0];
        let non_background = image
            .pixels
            .iter()
            .filter(|pixel| **pixel != background)
            .count();
        assert!(
            non_background > 100_000,
            "skew-T looks blank: {non_background} non-background pixels"
        );
    }
}

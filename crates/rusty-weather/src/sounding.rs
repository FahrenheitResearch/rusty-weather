//! Store-served CWT skew-T sounding at a point.
//!
//! Builds a full vertical profile for the nearest grid cell from the run's
//! stored isobaric volumes (`temperature_iso`, `dewpoint_iso`, `u_iso`,
//! `v_iso`, `height_iso`; 100–1000 hPa, descending pressure, index 0 =
//! 1000 hPa) plus the 2 m/10 m surface fields (`surface_pressure`,
//! `temperature_2m`, `dewpoint_2m`, `u_10m`, `v_10m`, `orography`).
//! Below-ground levels (p > psfc) are dropped and the surface sample is
//! prepended, then the vendored sharprs machinery (via `rustwx-sounding`)
//! computes parcels/shear/indices — including single-column ECAPE through
//! ecape-rs — and renders the CWT-styled composed image: skew-T with barb
//! stave, hodograph, ECAPE (entrainment) block, locator map, and the full
//! parameter table. Served as `GET /api/sounding` next to `/api/xsection`,
//! sharing its query parsing, error shape (422 for volume-less hours),
//! and run alias resolution.

use std::path::Path;

use rustwx_products::places::nearest_place;
use rustwx_sounding::{CwtHeader, NativeSounding, SoundingColumn, SoundingMetadata};
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;

use super::meteogram::{nearest_cell, valid_label};
use super::xsection::NO_VOLUME_MESSAGE;

/// Isobaric volumes the profile needs; absent on hours ingested before the
/// `view-volumes` profile deployed (the shared 422).
const VOLUME_VARS: &[&str] = &[
    "temperature_iso",
    "dewpoint_iso",
    "u_iso",
    "v_iso",
    "height_iso",
];

const M_TO_FT: f64 = 3.280_84;

#[derive(Debug, Clone)]
pub struct SoundingRequest {
    pub lat: f64,
    pub lon: f64,
    /// Forecast hour; `None` picks the newest stored hour carrying volumes.
    pub hour: Option<u16>,
    /// Hours to add to UTC for the local-time note (PDT = -7).
    pub utc_offset_hours: f64,
    /// Credit drawn on the image. Defaults to the CAFire house credit; the
    /// unbranded lab passes its own, and an empty string draws none — the same
    /// rule the outlook cards follow, so one sounding can be shared from either
    /// site without the wrong name on it.
    pub brand: String,
    /// Draw the vendored SHARPpy compositor's own panel arrangement instead of
    /// the CWT composite (house header, locator map, ECAPE block, rustwx table).
    /// The CAFire Lab keeps the composite; the unbranded lab asks for this one.
    pub sharppy_layout: bool,
}

#[derive(Debug)]
pub struct SoundingOutput {
    /// The composed CWT sounding image.
    pub png: Vec<u8>,
    /// The profile arrays + computed indices (`format=json` on the API);
    /// carries the forecast hour actually rendered.
    pub data: serde_json::Value,
}

/// One surface sample for the profile floor.
#[derive(Debug, Clone, Copy)]
struct SurfaceSample {
    psfc_hpa: f64,
    height_m: f64,
    t_c: f64,
    td_c: f64,
    u_ms: f64,
    v_ms: f64,
}

/// Assembled point profile, descending pressure, surface level first.
#[derive(Debug, Clone, Default)]
struct AssembledProfile {
    pressure_hpa: Vec<f64>,
    height_m: Vec<f64>,
    temperature_c: Vec<f64>,
    dewpoint_c: Vec<f64>,
    u_ms: Vec<f64>,
    v_ms: Vec<f64>,
}

/// Build the sounding arrays: drop below-ground isobaric levels
/// (p >= psfc), prepend the 2 m/10 m surface level, keep the store's
/// descending-pressure order, clamp dewpoint to temperature, and skip
/// levels with any non-finite member (the bridge validates hard).
fn assemble_profile(
    levels_hpa: &[f64],
    t_k: &[f32],
    td_k: &[f32],
    u_ms: &[f32],
    v_ms: &[f32],
    h_m: &[f32],
    sfc: &SurfaceSample,
) -> Result<AssembledProfile, String> {
    if !sfc.psfc_hpa.is_finite() || sfc.psfc_hpa <= 0.0 {
        return Err("surface pressure is missing at this cell".to_string());
    }
    if ![sfc.height_m, sfc.t_c, sfc.td_c, sfc.u_ms, sfc.v_ms]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err("surface sample is incomplete at this cell".to_string());
    }

    let mut profile = AssembledProfile::default();
    let sfc_height = sfc.height_m + 2.0; // 2 m sensor height above ground
    profile.pressure_hpa.push(sfc.psfc_hpa);
    profile.height_m.push(sfc_height);
    profile.temperature_c.push(sfc.t_c);
    profile.dewpoint_c.push(sfc.td_c.min(sfc.t_c));
    profile.u_ms.push(sfc.u_ms);
    profile.v_ms.push(sfc.v_ms);

    for (index, &p) in levels_hpa.iter().enumerate() {
        // Below-ground (and surface-duplicate) levels: p at or under psfc.
        if !(p > 0.0) || p >= sfc.psfc_hpa - 0.1 {
            continue;
        }
        let t = f64::from(t_k[index]) - 273.15;
        let td = f64::from(td_k[index]) - 273.15;
        let u = f64::from(u_ms[index]);
        let v = f64::from(v_ms[index]);
        let h = f64::from(h_m[index]);
        if ![t, td, u, v, h].iter().all(|value| value.is_finite()) {
            continue;
        }
        // Keep heights strictly increasing over the prepended surface.
        if h <= profile.height_m.last().copied().unwrap_or(f64::NEG_INFINITY) {
            continue;
        }
        profile.pressure_hpa.push(p);
        profile.height_m.push(h);
        profile.temperature_c.push(t);
        profile.dewpoint_c.push(td.min(t));
        profile.u_ms.push(u);
        profile.v_ms.push(v);
    }

    if profile.pressure_hpa.len() < 4 {
        return Err("too few above-ground levels at this cell to build a sounding".to_string());
    }
    Ok(profile)
}

/// Kelvin-aware conversion to °C (store carries 2 m temps in K).
fn to_c(value: f64, units: &str) -> f64 {
    let lower = units.trim().to_ascii_lowercase();
    if lower == "k" || lower.contains("kelvin") {
        value - 273.15
    } else {
        value
    }
}

fn finite_json(value: f64, scale: f64) -> serde_json::Value {
    if value.is_finite() {
        serde_json::json!((value * scale).round() / scale)
    } else {
        serde_json::Value::Null
    }
}

pub fn render_sounding(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    request: &SoundingRequest,
) -> Result<SoundingOutput, String> {
    if !(request.lat.is_finite() && request.lon.is_finite()) {
        return Err("lat/lon must be finite".to_string());
    }

    let run_dir = store_root.join(model_slug).join(run_slug);
    let grid =
        GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| format!("open grid: {err}"))?;
    let (cell, d2) = nearest_cell(&grid.lat, &grid.lon, request.lat, request.lon);
    if d2.sqrt() > 0.5 {
        return Err("the point is more than ~50 km outside the model grid".to_string());
    }
    let (cx, cy) = (cell % grid.nx, cell / grid.nx);
    let (cell_lat, cell_lon) = (f64::from(grid.lat[cell]), f64::from(grid.lon[cell]));

    // Stored hours in the run (for the hour default and honest errors).
    let mut hours: Vec<u16> = std::fs::read_dir(&run_dir)
        .map_err(|err| format!("read run dir: {err}"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix('f')?.strip_suffix(".rws")?.parse::<u16>().ok()
        })
        .collect();
    hours.sort_unstable();
    hours.dedup();
    if hours.is_empty() {
        return Err("run has no stored hours".to_string());
    }

    let has_volumes = |reader: &HourReader| {
        VOLUME_VARS.iter().all(|name| {
            reader.variable(name).is_some_and(|var| var.kind == "pressure3d")
        })
    };
    // Resolve the hour: explicit request (with honest errors), or the
    // newest stored hour that actually carries the volumes — the same
    // contract as /api/xsection.
    let (hour, reader) = match request.hour {
        Some(hour) => {
            if !hours.contains(&hour) {
                return Err(format!(
                    "hour F{hour:03} is not stored for this run (stored: F{:03}-F{:03})",
                    hours[0],
                    hours[hours.len() - 1]
                ));
            }
            let reader = HourReader::open(&run_dir.join(format!("f{hour:03}.rws")))
                .map_err(|err| format!("open hour F{hour:03}: {err}"))?;
            if !has_volumes(&reader) {
                return Err(NO_VOLUME_MESSAGE.to_string());
            }
            (hour, reader)
        }
        None => hours
            .iter()
            .rev()
            .find_map(|&hour| {
                let reader = HourReader::open(&run_dir.join(format!("f{hour:03}.rws"))).ok()?;
                has_volumes(&reader).then_some((hour, reader))
            })
            .ok_or_else(|| format!("no stored hour carries isobaric volumes — {NO_VOLUME_MESSAGE}"))?,
    };
    if reader.meta().grid_hash != grid.hash {
        return Err(format!("hour F{hour:03} was stored on a different grid"));
    }

    // ---- read the column at the nearest cell ----
    let column_var = |name: &str| -> Result<(Vec<f64>, Vec<f32>), String> {
        let var = reader
            .variable(name)
            .ok_or_else(|| NO_VOLUME_MESSAGE.to_string())?;
        let levels: Vec<f64> = var.levels_hpa.iter().map(|&l| f64::from(l)).collect();
        let values = reader
            .read_column_3d(name, cx, cy)
            .map_err(|err| format!("read {name}: {err}"))?;
        Ok((levels, values))
    };
    let (levels, t_k) = column_var("temperature_iso")?;
    let (levels_td, td_k) = column_var("dewpoint_iso")?;
    let (levels_u, u_iso) = column_var("u_iso")?;
    let (levels_v, v_iso) = column_var("v_iso")?;
    let (levels_h, h_iso) = column_var("height_iso")?;
    if levels_td != levels || levels_u != levels || levels_v != levels || levels_h != levels {
        return Err("isobaric volumes carry different levels in this hour".to_string());
    }

    // ---- surface sample (one tile decompress per variable) ----
    let surface = |name: &str| -> Result<(f64, String), String> {
        let var = reader
            .variable(name)
            .ok_or_else(|| format!("store hour lacks {name}"))?;
        let units = var.units.clone();
        let window = reader
            .read_window_2d(name, cx, cy, cx + 1, cy + 1)
            .map_err(|err| format!("read {name}: {err}"))?;
        Ok((f64::from(window.values[0]), units))
    };
    let (psfc_raw, psfc_units) = surface("surface_pressure")?;
    let to_hpa = if psfc_units.trim().eq_ignore_ascii_case("pa") { 0.01 } else { 1.0 };
    let (t2m_raw, t2m_units) = surface("temperature_2m")?;
    let (td2m_raw, td2m_units) = surface("dewpoint_2m")?;
    let (u10, _) = surface("u_10m")?;
    let (v10, _) = surface("v_10m")?;
    let (orog_m, _) = surface("orography")?;
    let sfc = SurfaceSample {
        psfc_hpa: psfc_raw * to_hpa,
        height_m: orog_m,
        t_c: to_c(t2m_raw, &t2m_units),
        td_c: to_c(td2m_raw, &td2m_units),
        u_ms: u10,
        v_ms: v10,
    };

    let assembled = assemble_profile(&levels, &t_k, &td_k, &u_iso, &v_iso, &h_iso, &sfc)?;

    // ---- bridge into the vendored sounding machinery ----
    let near = nearest_place(request.lat, request.lon);
    let place = match &near {
        Some(near) => near.describe(),
        None => format!("{:.2}, {:.2}", request.lat, request.lon),
    };
    let (hod, day_label) = valid_label(date_yyyymmdd, cycle_utc, hour);
    let local = (f64::from(hod) + request.utc_offset_hours).rem_euclid(24.0) as u32;
    let elevation_ft = (orog_m * M_TO_FT).round() as i64;

    let column = SoundingColumn {
        pressure_hpa: assembled.pressure_hpa.clone(),
        height_m_msl: assembled.height_m.clone(),
        temperature_c: assembled.temperature_c.clone(),
        dewpoint_c: assembled.dewpoint_c.clone(),
        u_ms: assembled.u_ms.clone(),
        v_ms: assembled.v_ms.clone(),
        omega_pa_s: Vec::new(),
        metadata: SoundingMetadata {
            station_id: place.clone(),
            valid_time: format!("{day_label} {hod:02}Z"),
            latitude_deg: Some(request.lat),
            longitude_deg: Some(request.lon),
            elevation_m: Some(orog_m),
            sample_method: Some("point".to_string()),
            box_radius_lat_deg: None,
            box_radius_lon_deg: None,
        },
    };
    let native = NativeSounding::from_column(&column)
        .map_err(|err| format!("profile rejected: {err}"))?;

    let header = CwtHeader {
        title: format!("SOUNDING — {place}"),
        subtitle: format!(
            "{model} {date} {cycle:02}Z | F{hour:03} | valid {day_label} {hod:02}Z ({local:02} local) | \
             point {lat:.3}, {lon:.3} | cell {cell_lat:.3}, {cell_lon:.3} | {elevation_ft} ft | \
             ECAPE = entrainment-adjusted (experimental)",
            model = model_slug.to_uppercase(),
            date = date_yyyymmdd,
            cycle = cycle_utc,
            lat = request.lat,
            lon = request.lon,
        ),
        brand: request.brand.clone(),
    };
    let png = if request.sharppy_layout {
        // The SPC-style window from sharppyrs, rendered offscreen. It takes the
        // raw profile rather than our computed params: sharppyrs runs the same
        // sharprs engine (one copy in the graph, patched in the workspace root),
        // so handing it the arrays keeps a single source of truth for the numbers
        // on the plot.
        let data = crate::sounding_sharppy::sounding_data(
            &assembled.pressure_hpa,
            &assembled.height_m,
            &assembled.temperature_c,
            &assembled.dewpoint_c,
            &assembled.u_ms,
            &assembled.v_ms,
            cell_lat,
            cell_lon,
        );
        let title = format!(
            "{model} {date} {cycle:02}z F{hour:03}  Valid: {day_label} {hod:02}z @{place}",
            model = model_slug.to_uppercase(),
            date = date_yyyymmdd,
            cycle = cycle_utc,
        );
        crate::sounding_sharppy::render_png(data, &title, &request.brand)?
    } else {
        native
            .render_cwt_png(&header)
            .map_err(|err| format!("render failed: {err}"))?
    };

    // ---- format=json payload: profile arrays + flat indices ----
    let params = &native.params;
    let ecape = &native.verified_ecape;
    let mag = |uv: (f64, f64)| uv.0.hypot(uv.1);
    let indices = serde_json::json!({
        "sbcape_j_kg": finite_json(params.sfcpcl.bplus, 1.0),
        "sbcin_j_kg": finite_json(params.sfcpcl.bminus, 1.0),
        "mlcape_j_kg": finite_json(params.mlpcl.bplus, 1.0),
        "mlcin_j_kg": finite_json(params.mlpcl.bminus, 1.0),
        "mucape_j_kg": finite_json(params.mupcl.bplus, 1.0),
        "mucin_j_kg": finite_json(params.mupcl.bminus, 1.0),
        "sb_ecape_j_kg": finite_json(ecape.surface_based.ecape, 1.0),
        "ml_ecape_j_kg": finite_json(ecape.mixed_layer.ecape, 1.0),
        "mu_ecape_j_kg": finite_json(ecape.most_unstable.ecape, 1.0),
        "sb_ncape": finite_json(ecape.surface_based.ncape, 100.0),
        "ml_ncape": finite_json(ecape.mixed_layer.ncape, 100.0),
        "mu_ncape": finite_json(ecape.most_unstable.ncape, 100.0),
        "lcl_m_agl": finite_json(params.sfcpcl.lclhght, 1.0),
        "lfc_m_agl": finite_json(params.sfcpcl.lfchght, 1.0),
        "el_m_agl": finite_json(params.sfcpcl.elhght, 1.0),
        "shear_0_1km_kt": finite_json(mag(params.shr01), 10.0),
        "shear_0_3km_kt": finite_json(mag(params.shr03), 10.0),
        "shear_0_6km_kt": finite_json(mag(params.shr06), 10.0),
        "srh_0_1km_m2_s2": finite_json(params.srh01.0, 10.0),
        "srh_0_3km_m2_s2": finite_json(params.srh03.0, 10.0),
        "pwat_in": finite_json(params.precip_water.unwrap_or(f64::NAN), 100.0),
        "lapse_0_3km_c_km": finite_json(params.lr03.unwrap_or(f64::NAN), 10.0),
        "lapse_3_6km_c_km": finite_json(params.lr36.unwrap_or(f64::NAN), 10.0),
        "lapse_700_500_c_km": finite_json(params.lr75.unwrap_or(f64::NAN), 10.0),
        "lapse_850_500_c_km": finite_json(params.lr85.unwrap_or(f64::NAN), 10.0),
        "k_index": finite_json(params.k_index.unwrap_or(f64::NAN), 10.0),
        "total_totals": finite_json(params.t_totals.unwrap_or(f64::NAN), 10.0),
        "dcape_j_kg": finite_json(params.dcape.dcape, 1.0),
        "freezing_level_m_agl": finite_json(params.frz_lvl.unwrap_or(f64::NAN), 1.0),
        "wet_bulb_zero_m_agl": finite_json(params.wb_zero.unwrap_or(f64::NAN), 1.0),
    });
    let round = |values: &[f64], scale: f64| -> Vec<serde_json::Value> {
        values.iter().map(|&v| finite_json(v, scale)).collect()
    };
    let data = serde_json::json!({
        "model": model_slug,
        "run": run_slug,
        "hour": hour,
        "valid": format!("{day_label} {hod:02}Z"),
        "point": { "lat": request.lat, "lon": request.lon },
        "nearest_place": near.map_or(serde_json::Value::Null, |n| serde_json::json!({
            "label": n.label,
            "distance_mi": (n.distance_mi() * 10.0).round() / 10.0,
            "bearing": n.compass(),
            "description": n.describe(),
        })),
        "cell": { "lat": cell_lat, "lon": cell_lon, "elevation_ft": elevation_ft },
        "profile": {
            "pressure_hpa": round(&assembled.pressure_hpa, 10.0),
            "height_m_msl": round(&assembled.height_m, 10.0),
            "temperature_c": round(&assembled.temperature_c, 100.0),
            "dewpoint_c": round(&assembled.dewpoint_c, 100.0),
            "u_ms": round(&assembled.u_ms, 100.0),
            "v_ms": round(&assembled.v_ms, 100.0),
        },
        "indices": indices,
        "ecape_note": "ECAPE/NCAPE are entrainment-adjusted via ecape-rs (experimental)",
    });

    Ok(SoundingOutput { png, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustwx_core::{
        CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
    };
    use rw_store::{PressureVolumeInput, write_hour_from_fields};

    // ---- profile assembly (pure) ----

    fn sfc() -> SurfaceSample {
        SurfaceSample {
            psfc_hpa: 903.0, // ~900 hPa station: 1000/925 hPa are below ground
            height_m: 1000.0,
            t_c: 31.0,
            td_c: 8.0,
            u_ms: 2.0,
            v_ms: 1.0,
        }
    }

    const LEVELS: [f64; 6] = [1000.0, 925.0, 850.0, 700.0, 500.0, 300.0];

    fn iso_columns() -> ([f32; 6], [f32; 6], [f32; 6], [f32; 6], [f32; 6]) {
        let t_k = [308.0, 304.0, 299.0, 288.0, 262.0, 232.0];
        let td_k = [300.0, 296.0, 280.0, 270.0, 240.0, 210.0];
        let u = [1.0, 2.0, 4.0, 8.0, 15.0, 25.0];
        let v = [0.5, 1.0, 2.0, 4.0, 8.0, 12.0];
        let h = [110.0, 780.0, 1500.0, 3100.0, 5800.0, 9600.0];
        (t_k, td_k, u, v, h)
    }

    #[test]
    fn assembly_drops_below_ground_and_prepends_surface() {
        let (t_k, td_k, u, v, h) = iso_columns();
        let profile = assemble_profile(&LEVELS, &t_k, &td_k, &u, &v, &h, &sfc())
            .expect("assembly succeeds");

        // 1000 and 925 hPa sit below the 903 hPa surface: dropped.
        assert_eq!(profile.pressure_hpa, vec![903.0, 850.0, 700.0, 500.0, 300.0]);
        // Surface level prepended with the 2 m sample.
        assert_eq!(profile.temperature_c[0], 31.0);
        assert_eq!(profile.u_ms[0], 2.0);
        assert_eq!(profile.height_m[0], 1002.0);
        // Descending pressure, ascending height — the bridge's contract.
        for pair in profile.pressure_hpa.windows(2) {
            assert!(pair[0] > pair[1], "pressure must descend: {pair:?}");
        }
        for pair in profile.height_m.windows(2) {
            assert!(pair[0] < pair[1], "height must ascend: {pair:?}");
        }
        // Dewpoint never exceeds temperature.
        for (t, td) in profile.temperature_c.iter().zip(&profile.dewpoint_c) {
            assert!(td <= t);
        }
    }

    #[test]
    fn assembly_skips_nan_levels_and_clamps_dewpoint() {
        let (mut t_k, mut td_k, u, v, h) = iso_columns();
        t_k[3] = f32::NAN; // 700 hPa hole
        td_k[4] = 280.0; // dewpoint above temperature at 500 hPa
        let profile = assemble_profile(&LEVELS, &t_k, &td_k, &u, &v, &h, &sfc())
            .expect("assembly succeeds");
        assert!(!profile.pressure_hpa.contains(&700.0), "NaN level dropped");
        let i500 = profile.pressure_hpa.iter().position(|&p| p == 500.0).unwrap();
        assert_eq!(
            profile.dewpoint_c[i500], profile.temperature_c[i500],
            "supersaturated dewpoint clamps to temperature"
        );
    }

    #[test]
    fn assembly_requires_a_real_surface_and_enough_levels() {
        let (t_k, td_k, u, v, h) = iso_columns();
        let mut bad = sfc();
        bad.psfc_hpa = f64::NAN;
        assert!(assemble_profile(&LEVELS, &t_k, &td_k, &u, &v, &h, &bad).is_err());

        let mut high = sfc();
        high.psfc_hpa = 320.0; // absurd surface: nearly everything below ground
        assert!(assemble_profile(&LEVELS, &t_k, &td_k, &u, &v, &h, &high).is_err());
    }

    // ---- synthetic-store integration (mirrors the xsection harness) ----

    const NX: usize = 60;
    const NY: usize = 50;
    const VOL_LEVELS: [u16; 9] = [1000, 925, 850, 700, 600, 500, 400, 300, 250];

    fn grid() -> LatLonGrid {
        let mut lat = Vec::with_capacity(NX * NY);
        let mut lon = Vec::with_capacity(NX * NY);
        for y in 0..NY {
            for x in 0..NX {
                lat.push((36.0 + 0.1 * y as f64) as f32);
                lon.push((-123.0 + 0.1 * x as f64) as f32);
            }
        }
        LatLonGrid::new(GridShape::new(NX, NY).expect("dims"), lat, lon).expect("grid")
    }

    fn surface_field(
        selector: FieldSelector,
        units: &str,
        value: impl Fn(usize, usize) -> f32,
    ) -> SelectedField2D {
        let values: Vec<f32> = (0..NY)
            .flat_map(|y| (0..NX).map(|x| value(x, y)).collect::<Vec<_>>())
            .collect();
        SelectedField2D::new(selector, units, grid(), values)
            .expect("sized values")
            .with_projection(GridProjection::Geographic)
    }

    /// A warm, moist, sheared July afternoon column: unstable enough that
    /// CAPE must come out positive.
    fn volume_plane(level_hpa: u16, kind: &str) -> Vec<f32> {
        let p = f32::from(level_hpa);
        let value = match kind {
            // Roughly standard-atmosphere heights.
            "h" => 44_330.0 * (1.0 - (p / 1013.25).powf(0.190_3)),
            // Warm surface, ~7 C/km lapse aloft in pressure space.
            "t" => 273.15 + 34.0 - 0.075 * (1000.0 - p),
            // Moist boundary layer drying aloft.
            "td" => 273.15 + 19.0 - 0.11 * (1000.0 - p),
            "u" => 2.0 + 0.035 * (1000.0 - p),
            _ => 1.0 + 0.012 * (1000.0 - p),
        };
        vec![value; NX * NY]
    }

    fn test_store(tag: &str, with_volumes: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-sounding-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let t2m = surface_field(
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            |_, _| 273.15 + 34.0,
        );
        let td2m = surface_field(
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            |_, _| 273.15 + 19.0,
        );
        let u10 = surface_field(
            FieldSelector::height_agl(CanonicalField::UWind, 10),
            "m s-1",
            |_, _| 2.5,
        );
        let v10 = surface_field(
            FieldSelector::height_agl(CanonicalField::VWind, 10),
            "m s-1",
            |_, _| 1.5,
        );
        let psfc = surface_field(
            FieldSelector::surface(CanonicalField::Pressure),
            "Pa",
            |_, _| 97_000.0, // ~370 m terrain: 1000 hPa is below ground
        );
        let orog = surface_field(
            FieldSelector::surface(CanonicalField::GeopotentialHeight),
            "m",
            |_, _| 370.0,
        );
        let planes: Vec<(&str, Vec<Vec<f32>>)> = [
            ("temperature_iso", "t"),
            ("dewpoint_iso", "td"),
            ("u_iso", "u"),
            ("v_iso", "v"),
            ("height_iso", "h"),
        ]
        .iter()
        .map(|&(name, kind)| {
            (
                name,
                VOL_LEVELS.iter().map(|&l| volume_plane(l, kind)).collect(),
            )
        })
        .collect();
        let volumes: Vec<PressureVolumeInput> = if with_volumes {
            planes
                .iter()
                .map(|(name, data)| PressureVolumeInput {
                    name,
                    units: if *name == "height_iso" {
                        "gpm"
                    } else if name.ends_with("_iso") && name.starts_with(['u', 'v']) {
                        "m s-1"
                    } else {
                        "K"
                    },
                    selector_template: serde_json::json!({ "synthetic": name }),
                    levels: VOL_LEVELS
                        .iter()
                        .zip(data)
                        .map(|(&l, p)| (l, p.as_slice()))
                        .collect(),
                })
                .collect()
        } else {
            Vec::new()
        };
        write_hour_from_fields(
            &dir,
            "synthetic",
            "20260702_00z",
            6,
            &[
                ("temperature_2m", &t2m),
                ("dewpoint_2m", &td2m),
                ("u_10m", &u10),
                ("v_10m", &v10),
                ("surface_pressure", &psfc),
                ("orography", &orog),
            ],
            &volumes,
            "sounding-test",
            1_780_000_000,
        )
        .expect("hour write");
        dir
    }

    fn request() -> SoundingRequest {
        SoundingRequest {
            lat: 38.5,
            lon: -121.5,
            hour: Some(6),
            utc_offset_hours: -7.0,
            brand: "cafire.org/weather".to_string(),
            sharppy_layout: false,
        }
    }

    /// The SPC-style window (sharppyrs, rendered offscreen through egui) must
    /// actually produce an image from a real store column, not just compile.
    /// This is the whole point of the port, and it exercises the wgpu path — so
    /// if the box has no usable Vulkan driver, this is the test that says so.
    #[test]
    fn the_sharppy_layout_renders_from_the_store() {
        let root = test_store("sharppy", true);
        let mut req = request();
        req.sharppy_layout = true;
        req.brand = "wxsection.com".to_string();
        let out = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect("sharppy layout renders");
        assert_eq!(&out.png[..4], &[0x89, 0x50, 0x4E, 0x47], "not a PNG");
        // The window is 1630x1100; anything much smaller means the harness drew
        // an empty frame rather than the sounding.
        assert!(
            out.png.len() > 40_000,
            "suspiciously small render: {} bytes",
            out.png.len()
        );
        let decoded = image::load_from_memory(&out.png).expect("decodes as an image");
        assert_eq!(
            (decoded.width(), decoded.height()),
            (1630, 1100),
            "window size changed"
        );
    }

    /// The credit is per-request so one renderer can serve both sites: the
    /// branded Lab and the unbranded one. Both must produce a real image.
    #[test]
    fn the_credit_is_per_request() {
        let root = test_store("brand", true);
        for brand in ["cafire.org/weather", "wxsection.com", ""] {
            let mut req = request();
            req.brand = brand.to_string();
            let out = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
                .unwrap_or_else(|err| panic!("render with brand {brand:?}: {err}"));
            assert_eq!(&out.png[..4], &[0x89, 0x50, 0x4E, 0x47], "brand {brand:?}");
        }
    }

    #[test]
    fn renders_a_sounding_with_sane_indices() {
        let root = test_store("happy", true);
        let out = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &request())
            .expect("render succeeds");
        assert_eq!(out.data["hour"], 6);
        // PNG magic bytes.
        assert_eq!(&out.png[..4], &[0x89, 0x50, 0x4E, 0x47]);

        let profile = &out.data["profile"];
        let pres: Vec<f64> = profile["pressure_hpa"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        // 1000 hPa is below the 970 hPa surface: dropped; surface first.
        assert!(pres[0] > 960.0 && pres[0] < 980.0, "surface first, got {}", pres[0]);
        assert!(!pres.contains(&1000.0), "below-ground level dropped");
        assert!(pres.windows(2).all(|w| w[0] > w[1]), "descending pressure");

        // Index sanity on the synthetic unstable column.
        let indices = &out.data["indices"];
        let sbcape = indices["sbcape_j_kg"].as_f64().expect("finite SBCAPE");
        assert!(sbcape > 0.0, "warm/moist July column must have CAPE, got {sbcape}");
        let shear6 = indices["shear_0_6km_kt"].as_f64().expect("finite shear");
        assert!(shear6 > 10.0, "sheared column, got {shear6} kt");
        if let Some(ecape) = indices["sb_ecape_j_kg"].as_f64() {
            let cape_for_ratio = sbcape.max(1.0);
            assert!(
                ecape <= cape_for_ratio * 1.35,
                "ECAPE ({ecape}) should not wildly exceed CAPE ({sbcape})"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_volumes_yield_the_honest_message() {
        let root = test_store("novol", false);
        let err = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &request())
            .expect_err("must refuse without volumes");
        assert!(
            err.contains("predates vertical-volume storage"),
            "honest message, got: {err}"
        );
        // The hour=None path reports the same cause.
        let mut req = request();
        req.hour = None;
        let err = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("must refuse without volumes");
        assert!(err.contains("predates vertical-volume storage"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_requests_are_named() {
        let root = test_store("bad", true);
        let mut req = request();
        req.hour = Some(13);
        let err = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("unknown hour");
        assert!(err.contains("F013 is not stored"), "got: {err}");

        let mut req = request();
        (req.lat, req.lon) = (12.0, -60.0);
        let err = render_sounding(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("far outside the grid");
        assert!(err.contains("outside the model grid"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

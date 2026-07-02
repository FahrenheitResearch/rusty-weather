//! Store-native IMET point meteogram.
//!
//! Samples one grid cell across every stored hour of a run via
//! `HourReader::read_window_2d` (one tile decompress per var-hour — no
//! full-plane reads) and renders a customizable multi-panel SVG: text stays
//! vector, the output scales to any screen, and generation is milliseconds.
//!
//! Panels (`panels` request field, default all): `temp` (T/Td °F), `rh`
//! (RH% with 15/20% critical lines), `vpd` (VPD kPa + surface HDW dual
//! axis), `wind` (sustained + gust mph), `fuels` (ERC + 10-h dead fuel
//! moisture), `smoke` (8 m smoke). Hours meeting the joint critical
//! thresholds (RH<=20% & wind>=20 mph, or RH<=15% & gust>=25 mph) are
//! shaded across every panel. Thermo follows the frozen atlas formulas
//! (Magnus/Bolton e_s; VPD = (e_s(T)-e_s(Td)) * 0.1 kPa).

use std::collections::BTreeMap;
use std::path::Path;

use rustwx_products::places::{nearest_place, NearestPlace};
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;

const MS_TO_MPH: f64 = 2.236_936;

/// Human headline form of a store variable name: underscores to spaces,
/// words capitalized, known acronyms uppercased ("rh_2m" → "RH 2m").
fn prettify_var_name(var: &str) -> String {
    var.split('_')
        .map(|word| match word {
            "rh" | "vpd" | "hdw" | "erc" | "kbdi" | "mslp" | "pwat" | "apcp" | "qpf" | "uh" => {
                word.to_uppercase()
            }
            _ if word.chars().next().is_some_and(|c| c.is_ascii_digit()) => word.to_string(),
            _ => {
                let mut chars = word.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                    None => String::new(),
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Client-sent titles that are just coordinates (the Lab sends
/// "39.150, -123.210" for map picks) get the nearest-town treatment as
/// if no title were given.
fn title_is_bare_coords(title: &str) -> bool {
    let trimmed = title.trim();
    !trimmed.is_empty()
        && trimmed.chars().any(|c| c.is_ascii_digit())
        && trimmed
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-' | '+' | ' ' | '°'))
}

/// Display title for a point product: absent/bare-coords titles become
/// "<coords> · 12 mi NE of Town, ST"; custom titles keep their text and
/// gain the town phrase only when it is non-redundant and fits.
fn point_title(
    title: Option<&str>,
    fallback_coords: &str,
    near: Option<&NearestPlace>,
    max_chars: usize,
) -> String {
    let custom = title
        .map(str::trim)
        .filter(|t| !t.is_empty() && !title_is_bare_coords(t));
    let mut composed = custom.map_or_else(|| fallback_coords.to_string(), str::to_string);
    if let Some(near) = near {
        let town = near.label.split(',').next().unwrap_or(&near.label);
        let redundant = composed
            .to_ascii_lowercase()
            .contains(&town.to_ascii_lowercase());
        let phrase = near.describe();
        if !redundant && composed.chars().count() + phrase.chars().count() + 3 <= max_chars {
            composed.push_str(&format!(" · {phrase}"));
        }
    }
    composed
}

pub const METEOGRAM_PANELS: &[&str] =
    &["temp", "rh", "vpd", "wind", "precip", "fuels", "smoke"];

#[derive(Debug, Clone)]
pub struct MeteogramRequest {
    pub lat: f64,
    pub lon: f64,
    pub panels: Vec<String>,
    /// Arbitrary stored variables, one auto-scaled panel each (overrides
    /// `panels` when non-empty) — the "chart anything" mode. Any 2D
    /// variable in the run's store works.
    pub vars: Vec<String>,
    pub title: Option<String>,
    /// Hours to add to UTC for the local-time axis row (PDT = -7).
    pub utc_offset_hours: f64,
}

struct HourSample {
    hour: u16,
    values: BTreeMap<String, f64>,
}

const SAMPLED_VARS: &[&str] = &[
    "temperature_2m",
    "dewpoint_2m",
    "rh_2m",
    "u_10m",
    "v_10m",
    "wind_gust_10m",
    "erc",
    "dead_fuel_moisture_10h",
    "kbdi",
    "smoke_8m",
    "apcp_run_total",
];

/// Nearest grid cell by squared equirectangular distance.
fn nearest_cell(lat: &[f32], lon: &[f32], point_lat: f64, point_lon: f64) -> (usize, f64) {
    let coslat = point_lat.to_radians().cos();
    let mut best = (0usize, f64::INFINITY);
    for (index, (&la, &lo)) in lat.iter().zip(lon.iter()).enumerate() {
        let dlat = f64::from(la) - point_lat;
        let dlon = (f64::from(lo) - point_lon) * coslat;
        let d2 = dlat * dlat + dlon * dlon;
        if d2 < best.1 {
            best = (index, d2);
        }
    }
    best
}

fn saturation_vapor_pressure_hpa(t_c: f64) -> f64 {
    6.112 * ((17.67 * t_c) / (t_c + 243.5)).exp()
}

fn vpd_kpa(t_c: f64, td_c: f64) -> f64 {
    ((saturation_vapor_pressure_hpa(t_c) - saturation_vapor_pressure_hpa(td_c)).max(0.0)) * 0.1
}

fn to_c(value: f64, units: &str) -> f64 {
    let lower = units.trim().to_ascii_lowercase();
    if lower == "k" || lower.contains("kelvin") {
        value - 273.15
    } else {
        value
    }
}

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

// ---- civil-date math (Hinnant) for valid-time axis labels ----

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = ((m + 9) % 12) as u64;
    let doy = (153 * mp + 2) / 5 + u64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (y + i64::from(m <= 2), m, d)
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTH_ABBREV: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// No-leap DOY (Feb 29 folds onto 59) for a (month, day).
fn no_leap_doy(month: u32, day: u32) -> u16 {
    const CUM: [u16; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let day = if month == 2 && day == 29 { 28 } else { day };
    CUM[(month as usize - 1).min(11)] + day as u16
}

/// NOAA sunrise/sunset in UTC hours for a lat/lon and no-leap DOY.
/// None above/below the polar circles when the sun never crosses.
fn sunrise_sunset_utc(lat: f64, lon: f64, doy: u16) -> Option<(f64, f64)> {
    let gamma = 2.0 * std::f64::consts::PI / 365.0 * (f64::from(doy) - 1.0 + 0.5);
    let eqtime = 229.18
        * (0.000075 + 0.001868 * gamma.cos()
            - 0.032077 * gamma.sin()
            - 0.014615 * (2.0 * gamma).cos()
            - 0.040849 * (2.0 * gamma).sin());
    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos()
        + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos()
        + 0.00148 * (3.0 * gamma).sin();
    let lat_rad = lat.to_radians();
    let cos_ha = (90.833f64.to_radians().cos() / (lat_rad.cos() * decl.cos()))
        - lat_rad.tan() * decl.tan();
    if !(-1.0..=1.0).contains(&cos_ha) {
        return None;
    }
    let ha_deg = cos_ha.acos().to_degrees();
    let sunrise = (720.0 - 4.0 * (lon + ha_deg) - eqtime) / 60.0;
    let sunset = (720.0 - 4.0 * (lon - ha_deg) - eqtime) / 60.0;
    Some((sunrise.rem_euclid(24.0), sunset.rem_euclid(24.0)))
}

/// Climatological day-window reference values for this cell (from the
/// seasonal RTMA store, when present): dashed "what's normal" lines.
struct ClimoDayRefs {
    /// (value in display units, short label, color) per panel key.
    refs: BTreeMap<&'static str, Vec<(f64, String, &'static str)>>,
}

fn climo_day_refs(
    store_root: &Path,
    forecast_grid_hash: &str,
    cx: usize,
    cy: usize,
    doy: u16,
) -> Option<ClimoDayRefs> {
    let climo_dir = store_root.join("rtma_climo").join("seasonal_v2026_05_24");
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(climo_dir.join("climo_grid_meta.json")).ok()?)
            .ok()?;
    if meta.get("hrrr_grid_hash")?.as_str()? != forecast_grid_hash {
        return None;
    }
    let row0 = meta.get("hrrr_row0")?.as_u64()? as usize;
    let col0 = meta.get("hrrr_col0")?.as_u64()? as usize;
    let ny = meta.get("ny")?.as_u64()? as usize;
    let nx = meta.get("nx")?.as_u64()? as usize;
    let (sx, sy) = (cx.checked_sub(col0)?, cy.checked_sub(row0)?);
    if sx >= nx || sy >= ny {
        return None;
    }
    let reader = HourReader::open(&climo_dir.join(format!("f{doy:03}.rws"))).ok()?;
    let read = |name: &str| -> Option<f64> {
        let window = reader.read_window_2d(name, sx, sy, sx + 1, sy + 1).ok()?;
        let value = f64::from(window.values[0]);
        value.is_finite().then_some(value)
    };
    let mut refs: BTreeMap<&'static str, Vec<(f64, String, &'static str)>> = BTreeMap::new();
    let mut push = |panel: &'static str, value: Option<f64>, label: &str, color: &'static str| {
        if let Some(value) = value {
            refs.entry(panel).or_default().push((value, label.to_string(), color));
        }
    };
    let vpd = |stat: &str| read(&format!("climo__utc_00_23__max_vpd_2m_kpa__{stat}"));
    push("vpd", vpd("p50"), "climo p50", "#6f6455");
    push("vpd", vpd("p90"), "climo p90", "#b0763d");
    push("vpd", vpd("p99"), "climo p99", "#c24a3d");
    let gust = |stat: &str| {
        read(&format!("climo__utc_00_23__max_gust_10m_ms__{stat}")).map(|v| v * MS_TO_MPH)
    };
    push("wind", gust("p90"), "gust p90", "#b0763d");
    push("wind", gust("p99"), "gust p99", "#c24a3d");
    let rh = |stat: &str| read(&format!("climo__utc_00_23__min_rh_2m_pct__{stat}"));
    push("rh", rh("p50"), "min-RH p50", "#6f6455");
    push("rh", rh("p10"), "min-RH p10", "#b0763d");
    Some(ClimoDayRefs { refs })
}

/// (utc-hour-of-day, "Wed 7/2") for run date + cycle + forecast hour.
fn valid_label(date_yyyymmdd: &str, cycle_utc: u8, forecast_hour: u16) -> (u32, String) {
    let year: i64 = date_yyyymmdd[0..4].parse().unwrap_or(2000);
    let month: u32 = date_yyyymmdd[4..6].parse().unwrap_or(1);
    let day: u32 = date_yyyymmdd[6..8].parse().unwrap_or(1);
    let total_hours = i64::from(cycle_utc) + i64::from(forecast_hour);
    let days = days_from_civil(year, month, day) + total_hours.div_euclid(24);
    let hod = total_hours.rem_euclid(24) as u32;
    let (_, m, d) = civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    (hod, format!("{weekday} {m}/{d}"))
}

/// Round tick step to a 1/2/2.5/5 decade multiple covering the range.
fn nice_step(range: f64, target: usize) -> f64 {
    if !(range.is_finite()) || range <= 0.0 {
        return 1.0;
    }
    let raw = range / target.max(1) as f64;
    let mag = 10f64.powf(raw.log10().floor());
    for mult in [1.0, 2.0, 2.5, 5.0, 10.0] {
        if mag * mult >= raw {
            return mag * mult;
        }
    }
    mag * 10.0
}

fn axis_bounds(values: &[f64]) -> Option<(f64, f64)> {
    let finite: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return None;
    }
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in finite {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if (hi - lo).abs() < 1e-9 {
        lo -= 1.0;
        hi += 1.0;
    }
    Some((lo, hi))
}

fn fmt(v: f64) -> String {
    if v.abs() >= 100.0 || (v - v.round()).abs() < 1e-6 {
        format!("{}", v.round() as i64)
    } else {
        format!("{v:.1}")
    }
}

/// Step-aware tick label: prints enough digits that adjacent ticks
/// always differ (tiny-range panels like raw smoke or vorticity would
/// otherwise print "0 0 0 0"). Sub-1e-4 steps switch to compact
/// exponent form ("2.5e-7") instead of a wall of zeros.
fn fmt_tick(v: f64, step: f64) -> String {
    if step >= 1.0 {
        return fmt(v);
    }
    if step < 1e-4 {
        return if v == 0.0 {
            "0".to_string()
        } else {
            format!("{v:.1e}")
        };
    }
    let decimals = ((-step.log10().floor()) as usize).clamp(1, 6);
    let s = format!("{v:.decimals$}");
    // IEEE -0.0 (and small negatives that round to it) print as 0.
    if s.trim_start_matches('-').chars().all(|c| c == '0' || c == '.') {
        s.trim_start_matches('-').to_string()
    } else {
        s
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Sunset→next-sunrise night bands in forecast-hour coordinates. In UTC
/// hours-of-day the night stays inside one UTC day when sunrise > sunset
/// (the Americas); it straddles the day boundary otherwise (Europe-like
/// longitudes) — pairing the wrong sunrise makes every night ~33 h long
/// and shades the whole plot.
fn night_spans_fh(sunrise: f64, sunset: f64, cycle_utc: u8, span_days: i64) -> Vec<(f64, f64)> {
    let mut spans = Vec::new();
    for day in -1..=span_days {
        let set_fh = day as f64 * 24.0 + sunset - f64::from(cycle_utc);
        let rise_day = if sunrise > sunset { day } else { day + 1 };
        let rise_fh = rise_day as f64 * 24.0 + sunrise - f64::from(cycle_utc);
        if rise_fh > set_fh {
            spans.push((set_fh, rise_fh));
        }
    }
    spans
}

/// One data series drawn in a panel.
#[derive(Clone)]
struct Series {
    key: String,
    label: String,
    color: &'static str,
    dashed: bool,
    right_axis: bool,
}

fn series(key: &str, label: &str, color: &'static str, dashed: bool, right_axis: bool) -> Series {
    Series {
        key: key.to_string(),
        label: label.to_string(),
        color,
        dashed,
        right_axis,
    }
}

/// A renderable panel: identity kind (drives rh lines / wind arrows /
/// climo refs), display title, series list.
struct PanelDef {
    kind: String,
    title: String,
    series: Vec<Series>,
}

const CUSTOM_PALETTE: [&str; 8] = [
    "#ffb454", "#7fb8e6", "#58c98f", "#c77dff", "#ff6d4d", "#ffd456", "#4fa3d1", "#e05545",
];

pub struct MeteogramOutput {
    pub svg: String,
    pub cell_lat: f64,
    pub cell_lon: f64,
    pub hours: usize,
    /// The sampled data (`format=json` on the API): hour list, per-series
    /// values in display units, critical-hour flags, cell metadata.
    pub data: serde_json::Value,
}

#[allow(clippy::too_many_arguments)]
pub fn render_meteogram_svg(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    request: &MeteogramRequest,
) -> Result<MeteogramOutput, String> {
    let run_dir = store_root.join(model_slug).join(run_slug);
    let grid = GridFile::open(&run_dir.join("grid.rwg"))
        .map_err(|err| format!("open grid: {err}"))?;
    if !(request.lat.is_finite() && request.lon.is_finite()) {
        return Err("lat/lon must be finite".to_string());
    }
    let (cell, d2) = nearest_cell(&grid.lat, &grid.lon, request.lat, request.lon);
    if d2.sqrt() > 0.5 {
        return Err(format!(
            "point ({:.3}, {:.3}) is more than ~50 km outside the model grid",
            request.lat, request.lon
        ));
    }
    let (cx, cy) = (cell % grid.nx, cell / grid.nx);
    let (cell_lat, cell_lon) = (f64::from(grid.lat[cell]), f64::from(grid.lon[cell]));

    // Stored hours: every f###.rws in the run dir.
    let mut hours: Vec<u16> = std::fs::read_dir(&run_dir)
        .map_err(|err| format!("read run dir: {err}"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let digits = name.strip_prefix('f')?.strip_suffix(".rws")?;
            digits.parse::<u16>().ok()
        })
        .collect();
    hours.sort_unstable();
    hours.dedup();
    if hours.len() < 2 {
        return Err(format!("run has {} stored hours; need at least 2", hours.len()));
    }

    // Custom mode: chart any stored 2D variables, one panel each.
    let custom_mode = !request.vars.is_empty();
    let custom_vars: Vec<String> = request.vars.iter().take(8).cloned().collect();
    let mut var_units: BTreeMap<String, String> = BTreeMap::new();

    // Sample the cell across hours: one tile decompress per var-hour.
    let mut samples: Vec<HourSample> = Vec::with_capacity(hours.len());
    let mut elevation_ft: Option<f64> = None;
    for &hour in &hours {
        let path = run_dir.join(format!("f{hour:03}.rws"));
        let reader = match HourReader::open(&path) {
            Ok(reader) => reader,
            Err(_) => continue,
        };
        if reader.meta().grid_hash != grid.hash {
            continue;
        }
        let mut values = BTreeMap::new();
        let names: Vec<String> = if custom_mode {
            custom_vars.clone()
        } else {
            SAMPLED_VARS.iter().map(|s| s.to_string()).collect()
        };
        for name in &names {
            let Some(var) = reader.variable(name) else { continue };
            let units = var.units.clone();
            let Ok(window) = reader.read_window_2d(name, cx, cy, cx + 1, cy + 1) else {
                continue;
            };
            let raw = f64::from(window.values[0]);
            if !raw.is_finite() {
                continue;
            }
            let is_kelvin = units.trim().eq_ignore_ascii_case("k")
                || units.to_ascii_lowercase().contains("kelvin");
            let value = if custom_mode {
                // Generic: Kelvin reads as °F, everything else native.
                if is_kelvin { c_to_f(raw - 273.15) } else { raw }
            } else {
                match name.as_str() {
                    "temperature_2m" | "dewpoint_2m" => c_to_f(to_c(raw, &units)),
                    "u_10m" | "v_10m" | "wind_gust_10m" => raw * MS_TO_MPH,
                    // Store carries kg/m^3; the curated panel is ug/m3.
                    "smoke_8m" => raw * 1.0e9,
                    _ => raw,
                }
            };
            var_units
                .entry(name.clone())
                .or_insert_with(|| if is_kelvin { "F".to_string() } else { units });
            values.insert(name.clone(), value);
        }
        if elevation_ft.is_none() {
            if let Some(_var) = reader.variable("orography") {
                if let Ok(window) = reader.read_window_2d("orography", cx, cy, cx + 1, cy + 1) {
                    let meters = f64::from(window.values[0]);
                    if meters.is_finite() {
                        elevation_ft = Some(meters * 3.280_84);
                    }
                }
            }
        }
        // Derived series (frozen formulas, in native units before display).
        if let (Some(&t_f), Some(&td_f)) = (values.get("temperature_2m"), values.get("dewpoint_2m"))
        {
            let (t_c, td_c) = ((t_f - 32.0) * 5.0 / 9.0, (td_f - 32.0) * 5.0 / 9.0);
            let vpd = vpd_kpa(t_c, td_c);
            values.insert("vpd".to_string(), vpd);
            if let (Some(&u), Some(&v)) = (values.get("u_10m"), values.get("v_10m")) {
                let wind_mph = (u * u + v * v).sqrt();
                values.insert("wind".to_string(), wind_mph);
                values.insert("hdw_wind".to_string(), vpd * wind_mph / MS_TO_MPH);
            }
            if let Some(&gust) = values.get("wind_gust_10m") {
                values.insert("hdw_gust".to_string(), vpd * gust / MS_TO_MPH);
            }
        }
        samples.push(HourSample { hour, values });
    }
    if samples.len() < 2 {
        return Err("fewer than 2 hours sampled".to_string());
    }

    // Hourly precip (inches) from the run-total accumulation differences.
    let totals: Vec<Option<f64>> = samples
        .iter()
        .map(|s| s.values.get("apcp_run_total").copied())
        .collect();
    for index in 1..samples.len() {
        if let (Some(prev), Some(now)) = (totals[index - 1], totals[index]) {
            samples[index]
                .values
                .insert("precip_in".to_string(), ((now - prev).max(0.0)) / 25.4);
        }
    }

    // Run-date DOY drives the climatology reference lines; night bands come
    // from NOAA sunrise/sunset at the cell.
    let run_month: u32 = date_yyyymmdd[4..6].parse().unwrap_or(1);
    let run_day: u32 = date_yyyymmdd[6..8].parse().unwrap_or(1);
    let doy = no_leap_doy(run_month, run_day);
    let climo_refs = climo_day_refs(store_root, &grid.hash, cx, cy, doy);
    // Night spans in forecast-hour coordinates, covering the run span.
    let last_hour = samples.last().expect("nonempty").hour;
    let night_spans: Vec<(f64, f64)> =
        if let Some((sunrise, sunset)) = sunrise_sunset_utc(cell_lat, cell_lon, doy) {
            // Day offsets relative to the run date, generously covering the span.
            let span_days = (i64::from(last_hour) + i64::from(cycle_utc)) / 24 + 1;
            night_spans_fh(sunrise, sunset, cycle_utc, span_days)
        } else {
            Vec::new()
        };

    // Critical joint-threshold hours (atlas thresholds, mph).
    let critical: Vec<bool> = samples
        .iter()
        .map(|sample| {
            let rh = sample.values.get("rh_2m").copied().unwrap_or(f64::NAN);
            let wind = sample.values.get("wind").copied().unwrap_or(f64::NAN);
            let gust = sample.values.get("wind_gust_10m").copied().unwrap_or(f64::NAN);
            (rh <= 20.0 && wind >= 20.0) || (rh <= 15.0 && gust >= 25.0)
        })
        .collect();

    // ---- layout ----
    let panels: Vec<&str> = if request.panels.is_empty() {
        METEOGRAM_PANELS.to_vec()
    } else {
        METEOGRAM_PANELS
            .iter()
            .copied()
            .filter(|panel| request.panels.iter().any(|p| p == panel))
            .collect()
    };
    if panels.is_empty() && !custom_mode {
        return Err(format!(
            "no valid panels requested; choose from {}",
            METEOGRAM_PANELS.join(",")
        ));
    }
    let panel_count = if custom_mode { custom_vars.len().max(1) } else { panels.len() };

    const W: f64 = 1180.0;
    const ML: f64 = 58.0;
    const MR: f64 = 58.0;
    const HEADER: f64 = 66.0;
    const PANEL_H: f64 = 148.0;
    /// Dedicated title strip above each plot area.
    const PANEL_BAND: f64 = 24.0;
    const PANEL_GAP: f64 = 18.0;
    const PANEL_UNIT: f64 = PANEL_BAND + PANEL_H + PANEL_GAP;
    const AXIS_H: f64 = 54.0;
    let plot_w = W - ML - MR;
    let total_h = HEADER + panel_count as f64 * PANEL_UNIT - PANEL_GAP + AXIS_H;

    let h0 = f64::from(samples.first().expect("nonempty").hour);
    let h1 = f64::from(samples.last().expect("nonempty").hour);
    let x_for = |hour: f64| ML + (hour - h0) / (h1 - h0).max(1.0) * plot_w;

    let get = |key: &str| -> Vec<f64> {
        samples
            .iter()
            .map(|s| s.values.get(key).copied().unwrap_or(f64::NAN))
            .collect()
    };

    let mut svg = String::with_capacity(64 * 1024);
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{total_h}" viewBox="0 0 {W} {total_h}" font-family="'IBM Plex Mono','Consolas',monospace">"##
    ));
    svg.push_str(&format!(
        r##"<rect width="{W}" height="{total_h}" fill="#14100c"/>"##
    ));

    // Header.
    let near = nearest_place(request.lat, request.lon);
    let title = point_title(
        request.title.as_deref(),
        &format!("{:.4}, {:.4}", request.lat, request.lon),
        near.as_ref(),
        75,
    );
    let elev = elevation_ft
        .map(|ft| format!(" | {} ft", ft.round() as i64))
        .unwrap_or_default();
    svg.push_str(&format!(
        r##"<text x="{ML}" y="30" fill="#f2e7d5" font-size="19" font-weight="700">POINT METEOGRAM — {}</text>"##,
        xml_escape(&title)
    ));
    // Branding parity with the outlook card.
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="30" fill="#8d8171" font-size="11" text-anchor="end">cafire.wxsection.com/lab</text>"##,
        W - MR
    ));
    svg.push_str(&format!(
        r##"<text x="{ML}" y="50" fill="#8d8171" font-size="12">{model} {date} {cycle:02}Z | grid cell {clat:.4}, {clon:.4}{elev} | F{f0:03}-F{f1:03} | RTMA-formula VPD/HDW | shaded = RH&lt;=20% &amp; wind&gt;=20mph (or RH&lt;=15 &amp; gust&gt;=25)</text>"##,
        model = model_slug.to_uppercase(),
        date = date_yyyymmdd,
        cycle = cycle_utc,
        clat = cell_lat,
        clon = cell_lon,
        f0 = samples.first().expect("nonempty").hour,
        f1 = samples.last().expect("nonempty").hour,
    ));

    // Panel definitions: fixed catalog for the curated mode, or one
    // auto-scaled panel per requested variable in custom mode.
    let panel_defs: Vec<PanelDef> = if custom_mode {
        custom_vars
            .iter()
            .enumerate()
            .map(|(index, name)| {
                let units = var_units.get(name).cloned().unwrap_or_default();
                let pretty = name.replace('_', " ");
                PanelDef {
                    kind: "custom".to_string(),
                    title: if units.is_empty() { pretty.clone() } else { format!("{pretty} ({units})") },
                    series: vec![series(name, &pretty, CUSTOM_PALETTE[index % CUSTOM_PALETTE.len()], false, false)],
                }
            })
            .collect()
    } else {
        // Model cadence from the actual stored spacing (1 h HRRR, 6 h
        // GFS/NBM) — the precip panel shows per-step totals, so its
        // title must name the step.
        let sample_gap_h = samples
            .windows(2)
            .map(|w| w[1].hour - w[0].hour)
            .min()
            .unwrap_or(1)
            .max(1);
        let precip_title = if sample_gap_h == 1 {
            "Hourly Precip (in)".to_string()
        } else {
            format!("{sample_gap_h}-h Precip (in)")
        };
        let catalog: Vec<(&str, &str, Vec<Series>)> = vec![
            ("temp", "Temperature / Dewpoint (°F)", vec![
                series("temperature_2m", "T", "#ff6d4d", false, false),
                series("dewpoint_2m", "Td", "#58c98f", false, false),
            ]),
            ("rh", "Relative Humidity (%)", vec![series("rh_2m", "RH", "#58c98f", false, false)]),
            ("vpd", "VPD (kPa) / Surface HDW", vec![
                series("vpd", "VPD", "#ffb454", false, false),
                series("hdw_wind", "HDW-w", "#c77dff", true, true),
                series("hdw_gust", "HDW-g", "#8a5fd0", true, true),
            ]),
            ("wind", "Wind / Gust (mph)", vec![
                series("wind", "sustained", "#7fb8e6", false, false),
                series("wind_gust_10m", "gust", "#ff8a00", false, false),
            ]),
            ("precip", precip_title.as_str(), vec![series("precip_in", "precip", "#4fa3d1", false, false)]),
            ("fuels", "ERC / 10-h Fuel Moisture (%)", vec![
                series("erc", "ERC", "#e05545", false, false),
                series("dead_fuel_moisture_10h", "10-h FM", "#58c98f", false, true),
            ]),
            ("smoke", "Near-Surface Smoke (ug/m3)", vec![series("smoke_8m", "smoke 8m", "#b0a695", false, false)]),
        ];
        catalog
            .into_iter()
            .filter(|(kind, _, _)| panels.iter().any(|p| p == kind))
            .map(|(kind, title, series)| PanelDef {
                kind: kind.to_string(),
                title: title.to_string(),
                series,
            })
            .collect()
    };

    for (panel_index, def) in panel_defs.iter().enumerate() {
        let panel_title = &def.title;
        let series_list = &def.series;
        let panel: &str = &def.kind;
        let band_top = HEADER + panel_index as f64 * PANEL_UNIT;
        let top = band_top + PANEL_BAND;
        // Title strip with an accent underline in the lead series color.
        let accent = series_list.first().map(|s| s.color).unwrap_or("#8d8171");
        svg.push_str(&format!(
            r##"<rect x="{ML}" y="{band_top}" width="{plot_w}" height="{PANEL_BAND}" fill="#241c13"/>"##
        ));
        svg.push_str(&format!(
            r##"<rect x="{ML}" y="{:.1}" width="{plot_w}" height="2" fill="{accent}" fill-opacity="0.55"/>"##,
            band_top + PANEL_BAND - 2.0
        ));
        svg.push_str(&format!(
            r##"<rect x="{ML}" y="{top}" width="{plot_w}" height="{PANEL_H}" fill="#1b1611" stroke="#3a3128"/>"##
        ));
        // Night shading (sunset -> sunrise) under everything.
        for &(start, end) in &night_spans {
            let lo = start.max(h0);
            let hi = end.min(h1);
            if hi <= lo {
                continue;
            }
            let x_lo = x_for(lo);
            svg.push_str(&format!(
                r##"<rect x="{x_lo:.1}" y="{top}" width="{:.1}" height="{PANEL_H}" fill="#000000" fill-opacity="0.30"/>"##,
                x_for(hi) - x_lo
            ));
        }
        // 00Z day boundaries inside the plot area.
        for sample in &samples {
            let (hod, _) = valid_label(date_yyyymmdd, cycle_utc, sample.hour);
            if hod == 0 {
                let x = x_for(f64::from(sample.hour));
                svg.push_str(&format!(
                    r##"<line x1="{x:.1}" y1="{top}" x2="{x:.1}" y2="{:.1}" stroke="#4a4034" stroke-width="1.2"/>"##,
                    top + PANEL_H
                ));
            }
        }
        // Critical-hour shading.
        for (index, sample) in samples.iter().enumerate() {
            if !critical[index] {
                continue;
            }
            let half = (h1 - h0) / (samples.len() as f64 - 1.0) / 2.0;
            let x_lo = x_for((f64::from(sample.hour) - half).max(h0));
            let x_hi = x_for((f64::from(sample.hour) + half).min(h1));
            svg.push_str(&format!(
                r##"<rect x="{x_lo:.1}" y="{top}" width="{:.1}" height="{PANEL_H}" fill="#dc1f1f" fill-opacity="0.13"/>"##,
                x_hi - x_lo
            ));
        }

        // Axis scaling: left axis from non-right series, right from the rest.
        // Climatology reference values are folded in so their dashed lines
        // always land inside the axis.
        let panel_refs: &[(f64, String, &'static str)] = climo_refs
            .as_ref()
            .and_then(|refs| refs.refs.get(panel).map(Vec::as_slice))
            .unwrap_or(&[]);
        let mut left_values: Vec<f64> = series_list
            .iter()
            .filter(|s| !s.right_axis)
            .flat_map(|s| get(&s.key))
            .collect();
        left_values.extend(panel_refs.iter().map(|(value, _, _)| *value));
        let right_values: Vec<f64> = series_list
            .iter()
            .filter(|s| s.right_axis)
            .flat_map(|s| get(&s.key))
            .collect();
        let left_bounds = if panel == "precip" {
            // Bars hang from zero; keep a sane minimum top for dry runs.
            axis_bounds(&left_values).map(|(_, hi)| (0.0, hi.max(0.10)))
        } else {
            axis_bounds(&left_values)
        };
        let right_bounds = axis_bounds(&right_values);
        let pad_bounds = |bounds: (f64, f64)| {
            let pad = (bounds.1 - bounds.0) * 0.12;
            (bounds.0 - pad, bounds.1 + pad)
        };

        // A panel with no plottable data names its missing variables
        // instead of rendering a silent blank box (typos self-diagnose;
        // NBM has no fuels/smoke, short runs may lack ERC).
        let panel_empty = left_bounds.is_none() && right_bounds.is_none();
        if panel_empty {
            let missing: Vec<&str> = series_list.iter().map(|s| s.key.as_str()).collect();
            svg.push_str(&format!(
                r##"<text x="{:.1}" y="{:.1}" fill="#8d8171" font-size="12" text-anchor="middle">no data for '{}' in this run</text>"##,
                ML + plot_w / 2.0,
                top + PANEL_H / 2.0 + 4.0,
                xml_escape(&missing.join("', '"))
            ));
        }

        // Gridlines + left ticks.
        if let Some(bounds) = left_bounds {
            let (lo, hi) = pad_bounds(bounds);
            let step = nice_step(hi - lo, 4);
            let mut tick = (lo / step).ceil() * step;
            while tick <= hi {
                let y = top + PANEL_H - (tick - lo) / (hi - lo) * PANEL_H;
                svg.push_str(&format!(
                    r##"<line x1="{ML}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#2a231b" stroke-width="1"/>"##,
                    ML + plot_w
                ));
                svg.push_str(&format!(
                    r##"<text x="{:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="end">{}</text>"##,
                    ML - 6.0,
                    y + 3.0,
                    fmt_tick(tick, step)
                ));
                tick += step;
            }
            // RH critical reference lines.
            if panel == "rh" {
                for (threshold, color) in [(20.0, "#f8823c"), (15.0, "#dc1f1f")] {
                    if threshold > lo && threshold < hi {
                        let y = top + PANEL_H - (threshold - lo) / (hi - lo) * PANEL_H;
                        svg.push_str(&format!(
                            r##"<line x1="{ML}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="{color}" stroke-width="1" stroke-dasharray="5,4" stroke-opacity="0.7"/>"##,
                            ML + plot_w
                        ));
                    }
                }
            }
            // Climatology day-window references ("what's normal here today").
            // Labels stagger left/right so near-equal values stay readable.
            for (ref_index, (value, label, color)) in panel_refs.iter().enumerate() {
                if *value <= lo || *value >= hi {
                    continue;
                }
                let y = top + PANEL_H - (value - lo) / (hi - lo) * PANEL_H;
                svg.push_str(&format!(
                    r##"<line x1="{ML}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="{color}" stroke-width="1" stroke-dasharray="2,4" stroke-opacity="0.9"/>"##,
                    ML + plot_w
                ));
                let label_x = if ref_index % 2 == 0 {
                    ML + plot_w - 4.0
                } else {
                    ML + plot_w - 80.0
                };
                // Near the panel top the usual above-the-line spot collides
                // with the wind-arrow row — drop below the line instead.
                let label_y = if y - top < 26.0 { y + 10.0 } else { y - 3.0 };
                svg.push_str(&format!(
                    r##"<text x="{label_x:.1}" y="{label_y:.1}" fill="{color}" font-size="9" text-anchor="end">{}</text>"##,
                    xml_escape(label)
                ));
            }
        }
        if let Some(bounds) = right_bounds {
            let (lo, hi) = pad_bounds(bounds);
            let step = nice_step(hi - lo, 4);
            let mut tick = (lo / step).ceil() * step;
            while tick <= hi {
                let y = top + PANEL_H - (tick - lo) / (hi - lo) * PANEL_H;
                svg.push_str(&format!(
                    r##"<text x="{:.1}" y="{:.1}" fill="#6f6455" font-size="10">{}</text>"##,
                    ML + plot_w + 6.0,
                    y + 3.0,
                    fmt_tick(tick, step)
                ));
                tick += step;
            }
        }

        // Series polylines (NaN breaks the path); precip draws as bars.
        for series in series_list {
            let bounds = if series.right_axis { right_bounds } else { left_bounds };
            let Some(bounds) = bounds else { continue };
            let (lo, hi) = pad_bounds(bounds);
            let values = get(&series.key);
            if series.key == "precip_in" {
                let bar_half = (plot_w / samples.len() as f64 * 0.36).clamp(2.0, 14.0);
                let y0 = top + PANEL_H - (0.0 - lo) / (hi - lo) * PANEL_H;
                for (sample, &value) in samples.iter().zip(values.iter()) {
                    if !value.is_finite() || value <= 0.0 {
                        continue;
                    }
                    let x = x_for(f64::from(sample.hour));
                    let y = top + PANEL_H - (value - lo) / (hi - lo) * PANEL_H;
                    // Edge samples: keep the bar inside the plot frame.
                    let x_lo = (x - bar_half).max(ML);
                    let x_hi = (x + bar_half).min(ML + plot_w);
                    svg.push_str(&format!(
                        r##"<rect x="{x_lo:.1}" y="{y:.1}" width="{:.1}" height="{:.1}" fill="{}" fill-opacity="0.85"/>"##,
                        (x_hi - x_lo).max(0.5),
                        (y0 - y).max(0.5),
                        series.color
                    ));
                }
                continue;
            }
            let mut segments: Vec<Vec<(f64, f64)>> = vec![Vec::new()];
            for (sample, &value) in samples.iter().zip(values.iter()) {
                if value.is_finite() {
                    let x = x_for(f64::from(sample.hour));
                    let y = top + PANEL_H - (value - lo) / (hi - lo) * PANEL_H;
                    segments.last_mut().expect("segment").push((x, y));
                } else if !segments.last().expect("segment").is_empty() {
                    segments.push(Vec::new());
                }
            }
            let dash = if series.dashed { r##" stroke-dasharray="6,4""## } else { "" };
            for segment in segments.iter().filter(|s| s.len() >= 2) {
                let points: Vec<String> =
                    segment.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
                svg.push_str(&format!(
                    r##"<polyline points="{}" fill="none" stroke="{}" stroke-width="1.8"{dash}/>"##,
                    points.join(" "),
                    series.color
                ));
            }
        }

        // Wind-direction arrows along the top of the wind panel (arrow
        // points downwind; met convention "from" drives the rotation).
        if panel == "wind" {
            let stride = (samples.len() / 26).max(1);
            for (index, sample) in samples.iter().enumerate() {
                if index % stride != 0 {
                    continue;
                }
                let (Some(&u), Some(&v)) =
                    (sample.values.get("u_10m"), sample.values.get("v_10m"))
                else {
                    continue;
                };
                if u * u + v * v < 1.0 {
                    continue; // calm: no meaningful direction
                }
                let from_deg = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
                // Edge samples: keep the rotated glyph inside the frame.
                let x = x_for(f64::from(sample.hour)).clamp(ML + 6.0, ML + plot_w - 6.0);
                svg.push_str(&format!(
                    r##"<g transform="translate({x:.1},{:.1}) rotate({from_deg:.0})"><line x1="0" y1="-5.5" x2="0" y2="5.5" stroke="#9fb6c9" stroke-width="1.3"/><path d="M0,5.5 L-2.6,1.8 M0,5.5 L2.6,1.8" stroke="#9fb6c9" stroke-width="1.3" fill="none"/></g>"##,
                    top + 13.0
                ));
            }
        }

        // Panel title + legend live in the title strip.
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#e8ddc9" font-size="12" font-weight="700">{}</text>"##,
            ML + 8.0,
            band_top + 16.0,
            xml_escape(panel_title)
        ));
        // No legend swatches over an empty panel — the in-panel note
        // already names what's missing.
        if !panel_empty {
            let mut legend_x = ML + plot_w - 8.0;
            for series in series_list.iter().rev() {
                let label = format!("{} —", series.label);
                legend_x -= 7.0 * label.len() as f64 + 12.0;
                svg.push_str(&format!(
                    r##"<text x="{legend_x:.1}" y="{:.1}" fill="{}" font-size="11">{}</text>"##,
                    band_top + 16.0,
                    series.color,
                    xml_escape(&label)
                ));
            }
        }
    }

    // Time axis: UTC row, local-time row, date labels at 00Z boundaries.
    let axis_y = HEADER + panel_count as f64 * PANEL_UNIT - PANEL_GAP;
    let offset = request.utc_offset_hours;
    for sample in &samples {
        let (hod, day_label) = valid_label(date_yyyymmdd, cycle_utc, sample.hour);
        let x = x_for(f64::from(sample.hour));
        if hod % 3 == 0 {
            svg.push_str(&format!(
                r##"<text x="{x:.1}" y="{:.1}" fill="#a99c88" font-size="10" text-anchor="middle">{hod:02}Z</text>"##,
                axis_y + 14.0
            ));
            let local = (f64::from(hod) + offset).rem_euclid(24.0) as u32;
            svg.push_str(&format!(
                r##"<text x="{x:.1}" y="{:.1}" fill="#7a8f6f" font-size="10" text-anchor="middle">{local:02}</text>"##,
                axis_y + 27.0
            ));
        }
        if hod == 0 || sample.hour == samples[0].hour {
            svg.push_str(&format!(
                r##"<text x="{x:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="middle">{day_label}</text>"##,
                axis_y + 41.0
            ));
        }
    }
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="{:.1}" fill="#6f6455" font-size="9" text-anchor="end">UTC / local (UTC{offset:+.0}) | dashed = climo day refs | dark = night</text>"##,
        ML + plot_w,
        axis_y + 52.0
    ));

    svg.push_str("</svg>");

    let mut series_keys: Vec<String> = Vec::new();
    for sample in &samples {
        for key in sample.values.keys() {
            if !series_keys.contains(key) {
                series_keys.push(key.clone());
            }
        }
    }
    let series: serde_json::Map<String, serde_json::Value> = series_keys
        .iter()
        .map(|key| {
            let values: Vec<serde_json::Value> = samples
                .iter()
                .map(|s| match s.values.get(key) {
                    Some(&v) if v.is_finite() => serde_json::json!(v),
                    _ => serde_json::Value::Null,
                })
                .collect();
            (key.to_string(), serde_json::Value::Array(values))
        })
        .collect();
    let data = serde_json::json!({
        "model": model_slug,
        "run": run_slug,
        "point": { "lat": request.lat, "lon": request.lon },
        "nearest_place": near.map_or(serde_json::Value::Null, |n| serde_json::json!({
            "label": n.label,
            "distance_mi": (n.distance_mi() * 10.0).round() / 10.0,
            "bearing": n.compass(),
            "description": n.describe(),
        })),
        "cell": { "lat": cell_lat, "lon": cell_lon, "elevation_ft": elevation_ft },
        "units": {
            "temperature_2m": "F", "dewpoint_2m": "F", "rh_2m": "%",
            "u_10m": "mph", "v_10m": "mph", "wind": "mph", "wind_gust_10m": "mph",
            "vpd": "kPa", "hdw_wind": "kPa*m/s", "hdw_gust": "kPa*m/s",
            "precip_in": "in", "erc": "index", "dead_fuel_moisture_10h": "%",
            "kbdi": "index", "smoke_8m": "ug/m3", "apcp_run_total": "mm",
        },
        "forecast_hours": samples.iter().map(|s| s.hour).collect::<Vec<u16>>(),
        "critical": critical,
        "series": series,
    });

    Ok(MeteogramOutput {
        svg,
        cell_lat,
        cell_lon,
        hours: samples.len(),
        data,
    })
}

// ===================== daily outlook card =====================
// The weathermodels.com-style shareable graphic: one column per local
// calendar day, HI/LO colored number strips, grouped bars, weekday labels.
// Works for any stored 2D variable on any model (temperature gets the
// classic absolute color scale; everything else auto-normalizes).

#[derive(Debug, Clone)]
pub struct DailyRequest {
    pub lat: f64,
    pub lon: f64,
    pub var: String,
    pub title: Option<String>,
    pub utc_offset_hours: f64,
    /// Column bucket: None = local calendar days (the classic card);
    /// Some(1/3/6) = fixed-hour buckets (the HRRR-friendly hourly strip).
    pub step_hours: Option<u16>,
}

struct DayAgg {
    label_date: String,
    label_dow: &'static str,
    hi: f64,
    lo: f64,
    /// Bucket-mean wind components (m/s) for the direction arrow and
    /// bucket-max sustained speed (mph), when wind is stored.
    wind_dir_from_deg: Option<f64>,
    wind_max_mph: Option<f64>,
    /// Bucket precipitation (inches), from run-total differences.
    precip_in: Option<f64>,
}

/// Classic absolute temperature (°F) cell color.
fn temp_color(f: f64) -> &'static str {
    match f {
        f if f < 0.0 => "#b7a6f0",
        f if f < 15.0 => "#8f9fe8",
        f if f < 32.0 => "#6f9bd1",
        f if f < 45.0 => "#7fc4d8",
        f if f < 55.0 => "#9cd6a4",
        f if f < 65.0 => "#d3e07f",
        f if f < 72.0 => "#f5e05c",
        f if f < 80.0 => "#f5b53c",
        f if f < 88.0 => "#ef8633",
        f if f < 95.0 => "#e0512e",
        f if f < 103.0 => "#b31f1f",
        f if f < 110.0 => "#7a2f1d",
        _ => "#8b3a94",
    }
}

/// Generic ramp for non-temperature variables, normalized over the card.
fn ramp_color(t: f64) -> &'static str {
    const RAMP: [&str; 8] = [
        "#6f9bd1", "#7fc4d8", "#9cd6a4", "#d3e07f", "#f5e05c", "#f5b53c", "#ef8633", "#e0512e",
    ];
    RAMP[((t.clamp(0.0, 1.0)) * (RAMP.len() as f64 - 1.0)).round() as usize]
}

/// Black or white text for a hex cell background.
fn text_on(hex: &str) -> &'static str {
    let parse = |s: &str| u32::from_str_radix(s, 16).unwrap_or(128) as f64;
    let (r, g, b) = (parse(&hex[1..3]), parse(&hex[3..5]), parse(&hex[5..7]));
    if 0.299 * r + 0.587 * g + 0.114 * b > 150.0 { "#141414" } else { "#ffffff" }
}

pub fn render_daily_svg(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    request: &DailyRequest,
) -> Result<String, String> {
    let run_dir = store_root.join(model_slug).join(run_slug);
    let grid = GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| format!("open grid: {err}"))?;
    let (cell, d2) = nearest_cell(&grid.lat, &grid.lon, request.lat, request.lon);
    if d2.sqrt() > 0.5 {
        return Err("point is outside the model grid".to_string());
    }
    let (cx, cy) = (cell % grid.nx, cell / grid.nx);

    let mut hours: Vec<u16> = std::fs::read_dir(&run_dir)
        .map_err(|err| format!("read run dir: {err}"))?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_prefix('f')?.strip_suffix(".rws")?.parse::<u16>().ok()
        })
        .collect();
    hours.sort_unstable();

    // Sample + group by LOCAL calendar day.
    let year: i64 = date_yyyymmdd[0..4].parse().unwrap_or(2000);
    let month: u32 = date_yyyymmdd[4..6].parse().unwrap_or(1);
    let day: u32 = date_yyyymmdd[6..8].parse().unwrap_or(1);
    let run_days = days_from_civil(year, month, day);
    let mut units = String::new();
    let mut per_day: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
    // Default context rows: wind (u,v) and precip run-total per bucket.
    let mut per_day_wind: BTreeMap<i64, Vec<(f64, f64)>> = BTreeMap::new();
    let mut per_day_apcp: BTreeMap<i64, (f64, f64)> = BTreeMap::new(); // (first,last) run-total
    // First sampled local-hour per bucket: single-value columns are
    // labeled with the sample's actual valid hour, not the bucket start.
    let mut per_day_first_lh: BTreeMap<i64, i64> = BTreeMap::new();
    for &hour in &hours {
        let Ok(reader) = HourReader::open(&run_dir.join(format!("f{hour:03}.rws"))) else {
            continue;
        };
        if reader.meta().grid_hash != grid.hash {
            continue;
        }
        let Some(var) = reader.variable(&request.var) else { continue };
        if units.is_empty() {
            units = var.units.clone();
        }
        let point = |name: &str| -> Option<f64> {
            let window = reader.read_window_2d(name, cx, cy, cx + 1, cy + 1).ok()?;
            let value = f64::from(window.values[0]);
            value.is_finite().then_some(value)
        };
        let Some(raw) = point(&request.var) else { continue };
        let local_hours =
            run_days * 24 + i64::from(cycle_utc) + i64::from(hour) + request.utc_offset_hours as i64;
        let bucket = match request.step_hours {
            Some(step) => local_hours.div_euclid(i64::from(step.max(1))),
            None => local_hours.div_euclid(24),
        };
        per_day.entry(bucket).or_default().push(raw);
        per_day_first_lh.entry(bucket).or_insert(local_hours);
        if let (Some(u), Some(v)) = (point("u_10m"), point("v_10m")) {
            per_day_wind.entry(bucket).or_default().push((u, v));
        }
        if let Some(total) = point("apcp_run_total") {
            // Hours iterate ascending: keep the first total, overwrite
            // the last.
            let entry = per_day_apcp.entry(bucket).or_insert((total, total));
            entry.1 = total;
        }
    }
    if per_day.is_empty() {
        return Err(format!("variable '{}' has no data in this run", request.var));
    }

    // Chain run-total diffs ACROSS buckets so accumulation between the
    // last sample of one bucket and the first of the next isn't dropped
    // (an hourly bucket holds one sample — its in-bucket spread is zero
    // by construction, which zeroed the whole PCPN row on hourly cards).
    let mut per_day_precip: BTreeMap<i64, f64> = BTreeMap::new();
    let mut prev_total: Option<f64> = None;
    for (&bucket, &(first, last)) in &per_day_apcp {
        let base = prev_total.unwrap_or(first);
        per_day_precip.insert(bucket, (last - base).max(0.0));
        prev_total = Some(last);
    }

    // Unit conversions for readability.
    let lower_units = units.to_ascii_lowercase();
    let is_kelvin = lower_units == "k" || lower_units.contains("kelvin");
    let to_mph = lower_units.contains("m/s")
        && (request.var.contains("wind") || request.var.contains("gust"));
    let convert = |v: f64| {
        if is_kelvin { c_to_f(v - 273.15) } else if to_mph { v * MS_TO_MPH } else { v }
    };
    let display_units = if is_kelvin {
        "°F".to_string()
    } else if to_mph {
        "mph".to_string()
    } else {
        units.clone()
    };

    // Drop partial buckets: require ~3/4 of the samples a full bucket
    // would carry at this model's cadence — otherwise an evening-only
    // stub masquerades as the day's HI.
    let cadence = hours
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .min()
        .unwrap_or(1)
        .max(1);
    let bucket_hours = i64::from(request.step_hours.unwrap_or(24).max(1));
    let expected = (bucket_hours as usize / cadence as usize).max(1);
    let min_samples = ((expected * 3) / 4).max(1);
    // A bucket no finer than the model cadence holds one value: single-
    // value mode (one strip row, no inner LO bar).
    let single = bucket_hours <= i64::from(cadence);
    let mut last_day: i64 = i64::MIN;
    let days: Vec<DayAgg> = per_day
        .iter()
        .filter(|(_, values)| values.len() >= min_samples)
        .map(|(&bucket, values)| {
            let start_hours = bucket * bucket_hours;
            let local_day = start_hours.div_euclid(24);
            let (y, m, d) = civil_from_days(local_day);
            let _ = y;
            let dow = WEEKDAYS[(local_day + 4).rem_euclid(7) as usize];
            let hi = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let lo = values.iter().copied().fold(f64::INFINITY, f64::min);
            let (label_date, label_dow) = if request.step_hours.is_some() {
                // Hourly strip: hour-of-day on top, date only at day
                // breaks. Single-value columns label the sample's actual
                // valid hour — the bucket start can sit hours earlier.
                let hod = if single {
                    per_day_first_lh
                        .get(&bucket)
                        .map_or(start_hours, |lh| *lh)
                        .rem_euclid(24)
                } else {
                    start_hours.rem_euclid(24)
                };
                let day_label: &'static str = if local_day != last_day { dow } else { "" };
                last_day = local_day;
                (format!("{hod:02}"), day_label)
            } else {
                last_day = local_day;
                (
                    format!("{}.{:02}", MONTH_ABBREV[m as usize - 1].to_uppercase(), d),
                    dow,
                )
            };
            let wind = per_day_wind.get(&bucket).filter(|w| !w.is_empty()).map(|w| {
                let (su, sv) = w.iter().fold((0.0, 0.0), |(a, b), (u, v)| (a + u, b + v));
                let (mu, mv) = (su / w.len() as f64, sv / w.len() as f64);
                let dir_from = (-mu).atan2(-mv).to_degrees().rem_euclid(360.0);
                let max_mph = w
                    .iter()
                    .map(|(u, v)| (u * u + v * v).sqrt() * MS_TO_MPH)
                    .fold(f64::NEG_INFINITY, f64::max);
                (dir_from, max_mph)
            });
            let precip_in = per_day_precip.get(&bucket).map(|mm| mm / 25.4);
            DayAgg {
                label_date,
                label_dow,
                hi: convert(hi),
                lo: convert(lo),
                wind_dir_from_deg: wind.map(|(d, _)| d),
                wind_max_mph: wind.map(|(_, s)| s),
                precip_in,
            }
        })
        .collect();
    if days.is_empty() {
        return Err("no bucket has enough samples".to_string());
    }

    let is_temp = is_kelvin || display_units == "°F" || lower_units.contains("degf");

    // Header text first: the canvas widens to fit it, so it must exist
    // before layout.
    let var_pretty = prettify_var_name(&request.var);
    let span_word = match request.step_hours {
        None => "Daily HI/LO".to_string(),
        Some(step) if i64::from(step) <= i64::from(cadence) => {
            // Single-value mode: surviving columns land on the model
            // cadence, not the requested step — say what's rendered.
            if cadence == 1 {
                "Hourly".to_string()
            } else {
                format!("{cadence}-h")
            }
        }
        Some(step) => format!("{step}-h HI/LO"),
    };
    let heading = if is_temp && request.var.starts_with("temperature") {
        format!("{span_word} Temperature [{display_units}]")
    } else {
        format!("{span_word} {var_pretty} [{display_units}]")
    };
    let near = nearest_place(request.lat, request.lon);
    let custom_title = request
        .title
        .as_deref()
        .map(str::trim)
        .filter(|t| !t.is_empty() && !title_is_bare_coords(t));
    // Big orange slot: custom titles verbatim; map picks become the
    // community, distance-honest ("near" only genuinely nearby, the
    // full offset beyond that).
    let place = match (custom_title, &near) {
        (Some(t), _) => t.to_string(),
        (None, Some(n)) if n.distance_mi() < 3.0 => n.label.clone(),
        (None, Some(n)) if n.distance_mi() <= 12.0 => format!("near {}", n.label),
        (None, Some(n)) => format!(
            "{} mi {} of {}",
            n.distance_mi().round() as i64,
            n.compass(),
            n.label
        ),
        (None, None) => format!("{:.3}, {:.3}", request.lat, request.lon),
    };
    // Coord readout keeps the precise town offset (skipped only when a
    // custom title already names the town).
    let mut point_line = format!("{:.2}°, {:.2}°", request.lat, request.lon);
    if let Some(n) = &near {
        let town = n.label.split(',').next().unwrap_or(&n.label);
        let redundant = custom_title
            .map(|t| t.to_ascii_lowercase().contains(&town.to_ascii_lowercase()))
            .unwrap_or(false);
        if !redundant {
            point_line.push_str(&format!(" · {}", n.describe()));
        }
    }

    // ---- layout ----
    let n = days.len();
    let mut ml = 64.0;
    let base_col = if n <= 4 { 150.0 } else { (1080.0 / n as f64).clamp(84.0, 150.0) };
    // Width: room for the columns, the 880px header floor, AND the
    // actual header text — a long fire title must never overlap the
    // heading (both are anchored to opposite edges).
    let place_chars = place.chars().count();
    let place_font: f64 = if place_chars > 30 { 19.0 } else { 24.0 };
    let heading_w = (6 + heading.chars().count()) as f64 * 21.0 * 0.60;
    let place_w = place_chars as f64 * place_font * 0.68;
    let text_need = 24.0 + heading_w + 24.0 + place_w + 26.0;
    let width = (ml + base_col * n as f64 + 28.0)
        .max(880.0)
        .max(text_need.ceil());
    // Stretch columns into the floored width (capped so a 1-day bar
    // doesn't balloon) and center the block in the leftover slack.
    let col_w = ((width - ml - 28.0) / n as f64)
        .clamp(84.0, if n <= 4 { 240.0 } else { 150.0 });
    ml += ((width - 28.0 - ml) - col_w * n as f64) / 2.0;
    let strip_h = 46.0;
    let n_strips: f64 = if single { 1.0 } else { 2.0 };
    let wind_row_h = 46.0;
    let pcpn_row_h = 38.0;
    let chart_top = 86.0 + n_strips * (strip_h + 6.0) + wind_row_h + pcpn_row_h + 14.0;
    let chart_h = 320.0;
    let height = chart_top + chart_h + 74.0;

    let all_hi = days.iter().map(|d| d.hi).fold(f64::NEG_INFINITY, f64::max);
    let all_lo = days.iter().map(|d| d.lo).fold(f64::INFINITY, f64::min);
    let (all_lo, all_hi) = if !is_temp && all_hi - all_lo < 1e-9 {
        // Constant field (zero precip, per-run fuels): widen so the axis
        // is real instead of ±1e-7 noise. Non-negative data sits at the
        // axis floor, not mid-chart.
        let lo = if all_lo >= 0.0 { (all_lo - 1.0).max(0.0) } else { all_lo - 1.0 };
        (lo, all_hi + 1.0)
    } else {
        (all_lo, all_hi)
    };
    let span = (all_hi - all_lo).max(1e-6);
    let cell_color = |v: f64| if is_temp { temp_color(v) } else { ramp_color((v - all_lo) / span) };
    // Bar axis: pad below the min and above the max to nice steps.
    let axis_lo = if is_temp { (all_lo / 5.0).floor() * 5.0 - 5.0 } else { all_lo - span * 0.15 };
    let axis_hi = if is_temp { (all_hi / 5.0).ceil() * 5.0 + 5.0 } else { all_hi + span * 0.15 };
    let y_of = |v: f64| chart_top + chart_h - (v - axis_lo) / (axis_hi - axis_lo) * chart_h;
    let fmt_val = |v: f64| {
        if (axis_hi - axis_lo) > 20.0 || v.abs() >= 100.0 {
            format!("{}", v.round() as i64)
        } else {
            let s = format!("{v:.1}");
            if s == "-0.0" { "0.0".to_string() } else { s }
        }
    };

    let mut svg = String::with_capacity(32 * 1024);
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" font-family="Inter,'Segoe UI',Helvetica,Arial,sans-serif">"##
    ));
    svg.push_str(&format!(r##"<rect width="{width}" height="{height}" fill="#0d1112"/>"##));
    svg.push_str(&format!(
        r##"<rect x="0" y="0" width="{width}" height="4" fill="#ff5b24"/>"##
    ));
    // Header.
    svg.push_str(&format!(
        r##"<text x="24" y="40" fill="#f6f5f1" font-size="21" font-weight="800">CWT | {}</text>"##,
        xml_escape(&heading)
    ));
    svg.push_str(&format!(
        r##"<text x="24" y="64" fill="#9ea6a5" font-size="13">{} {} {:02}Z | {}</text>"##,
        model_slug.to_uppercase(),
        date_yyyymmdd,
        cycle_utc,
        xml_escape(&point_line),
    ));
    svg.push_str(&format!(
        r##"<text x="{:.0}" y="44" fill="#ff6a36" font-size="{place_font:.0}" font-weight="800" text-anchor="end">{}</text>"##,
        width - 26.0,
        xml_escape(&place.to_uppercase())
    ));
    svg.push_str(&format!(
        r##"<text x="{:.0}" y="64" fill="#9ea6a5" font-size="12" text-anchor="end">cafire.wxsection.com/lab</text>"##,
        width - 26.0
    ));

    // Value strips: HI/LO pair, or one row for single-value buckets.
    let strip_rows: &[(&str, bool)] = if single { &[("VAL", true)] } else { &[("HI", true), ("LO", false)] };
    let value_font = if col_w < 100.0 { 19.0 } else { 26.0 };
    for (row, (label, is_hi)) in strip_rows.iter().enumerate() {
        let y = 86.0 + row as f64 * (strip_h + 6.0);
        svg.push_str(&format!(
            r##"<text x="{:.0}" y="{:.0}" fill="#9ea6a5" font-size="13" font-weight="700" text-anchor="end">{label}</text>"##,
            ml - 10.0,
            y + strip_h / 2.0 + 5.0,
        ));
        for (index, day) in days.iter().enumerate() {
            let v = if *is_hi { day.hi } else { day.lo };
            let color = cell_color(v);
            let x = ml + index as f64 * col_w;
            svg.push_str(&format!(
                r##"<rect x="{x:.0}" y="{y:.0}" width="{:.0}" height="{strip_h}" fill="{color}" stroke="#0d1112" stroke-width="2"/>"##,
                col_w
            ));
            svg.push_str(&format!(
                r##"<text x="{:.0}" y="{:.0}" fill="{}" font-size="{value_font}" font-weight="800" text-anchor="middle">{}</text>"##,
                x + col_w / 2.0,
                y + strip_h / 2.0 + 8.0,
                text_on(color),
                fmt_val(v)
            ));
        }
    }

    // WIND row: direction arrow (pointing downwind) + bucket-max speed.
    let wind_y = 86.0 + n_strips * (strip_h + 6.0);
    svg.push_str(&format!(
        r##"<text x="{:.0}" y="{:.0}" fill="#9ea6a5" font-size="13" font-weight="700" text-anchor="end">WIND</text>"##,
        ml - 10.0,
        wind_y + wind_row_h / 2.0 + 5.0,
    ));
    for (index, day) in days.iter().enumerate() {
        let x_center = ml + index as f64 * col_w + col_w / 2.0;
        match (day.wind_dir_from_deg, day.wind_max_mph) {
            (Some(dir), Some(speed)) => {
                let speed_color = if speed >= 30.0 { "#ff5b24" } else if speed >= 15.0 { "#f5b53c" } else { "#9ea6a5" };
                svg.push_str(&format!(
                    r##"<g transform="translate({x_center:.1},{:.1}) rotate({dir:.0})"><line x1="0" y1="-8" x2="0" y2="8" stroke="{speed_color}" stroke-width="2"/><path d="M0,8 L-3.6,2.6 M0,8 L3.6,2.6" stroke="{speed_color}" stroke-width="2" fill="none"/></g>"##,
                    wind_y + 15.0
                ));
                svg.push_str(&format!(
                    r##"<text x="{x_center:.1}" y="{:.1}" fill="{speed_color}" font-size="13" font-weight="700" text-anchor="middle">{}</text>"##,
                    wind_y + wind_row_h - 4.0,
                    speed.round() as i64
                ));
            }
            _ => svg.push_str(&format!(
                r##"<text x="{x_center:.1}" y="{:.1}" fill="#4a4f50" font-size="13" text-anchor="middle">—</text>"##,
                wind_y + wind_row_h / 2.0 + 4.0
            )),
        }
    }

    // PCPN row: bucket precipitation in inches, blue-scaled.
    let pcpn_y = wind_y + wind_row_h + 4.0;
    svg.push_str(&format!(
        r##"<text x="{:.0}" y="{:.0}" fill="#9ea6a5" font-size="13" font-weight="700" text-anchor="end">PCPN</text>"##,
        ml - 10.0,
        pcpn_y + pcpn_row_h / 2.0 + 5.0,
    ));
    for (index, day) in days.iter().enumerate() {
        let x = ml + index as f64 * col_w;
        let (cell, text, text_color) = match day.precip_in {
            Some(p) if p >= 0.005 => {
                let cell = if p >= 1.0 { "#b3e0ff" } else if p >= 0.5 { "#7cc0e8" } else if p >= 0.25 { "#4fa3d1" } else if p >= 0.1 { "#3b82b8" } else { "#2b5f7e" };
                (cell, format!("{p:.2}"), text_on(cell))
            }
            Some(_) => ("#161c1d", "0".to_string(), "#4a4f50"),
            None => ("#161c1d", "—".to_string(), "#4a4f50"),
        };
        svg.push_str(&format!(
            r##"<rect x="{x:.0}" y="{pcpn_y:.0}" width="{:.0}" height="{pcpn_row_h}" fill="{cell}" stroke="#0d1112" stroke-width="2"/>"##,
            col_w
        ));
        svg.push_str(&format!(
            r##"<text x="{:.0}" y="{:.0}" fill="{text_color}" font-size="14" font-weight="700" text-anchor="middle">{text}</text>"##,
            x + col_w / 2.0,
            pcpn_y + pcpn_row_h / 2.0 + 5.0,
        ));
    }

    // Chart area + gridlines.
    svg.push_str(&format!(
        r##"<rect x="{ml:.0}" y="{chart_top:.0}" width="{:.0}" height="{chart_h:.0}" fill="#111617" stroke="#2a2f30"/>"##,
        col_w * n as f64
    ));
    let step = nice_step(axis_hi - axis_lo, 7);
    let mut tick = (axis_lo / step).ceil() * step;
    while tick <= axis_hi {
        let y = y_of(tick);
        svg.push_str(&format!(
            r##"<line x1="{ml:.0}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#22282a" stroke-width="1" stroke-dasharray="4,5"/>"##,
            ml + col_w * n as f64
        ));
        svg.push_str(&format!(
            r##"<text x="{:.0}" y="{:.1}" fill="#737c7b" font-size="12" text-anchor="end">{}</text>"##,
            ml - 8.0,
            y + 4.0,
            fmt_val(tick)
        ));
        tick += step;
    }

    // Bars: wide HI bar + inner LO bar, both on the strip color scale.
    for (index, day) in days.iter().enumerate() {
        let x = ml + index as f64 * col_w;
        let bar_w = col_w * 0.62;
        let bar_x = x + (col_w - bar_w) / 2.0;
        let hi_y = y_of(day.hi);
        let base = chart_top + chart_h;
        svg.push_str(&format!(
            r##"<rect x="{bar_x:.1}" y="{hi_y:.1}" width="{bar_w:.1}" height="{:.1}" fill="{}" rx="3"/>"##,
            (base - hi_y).max(1.0),
            cell_color(day.hi)
        ));
        if !single {
            let lo_w = bar_w * 0.52;
            let lo_x = x + (col_w - lo_w) / 2.0;
            let lo_y = y_of(day.lo);
            svg.push_str(&format!(
                r##"<rect x="{lo_x:.1}" y="{lo_y:.1}" width="{lo_w:.1}" height="{:.1}" fill="{}" rx="3" stroke="#0d1112" stroke-width="1.5"/>"##,
                (base - lo_y).max(1.0),
                cell_color(day.lo)
            ));
            svg.push_str(&format!(
                r##"<text x="{:.1}" y="{:.1}" fill="#0d1112" font-size="14" font-weight="800" text-anchor="middle">{}</text>"##,
                x + col_w / 2.0,
                (lo_y + 17.0).min(base - 6.0),
                fmt_val(day.lo)
            ));
        }
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#f6f5f1" font-size="{}" font-weight="800" text-anchor="middle">{}</text>"##,
            x + col_w / 2.0,
            hi_y - 8.0,
            if col_w < 100.0 { 13.5 } else { 17.0 },
            fmt_val(day.hi)
        ));
        // Day labels.
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#f6f5f1" font-size="13.5" font-weight="700" text-anchor="middle">{}</text>"##,
            x + col_w / 2.0,
            chart_top + chart_h + 24.0,
            day.label_date
        ));
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#9ea6a5" font-size="12.5" text-anchor="middle">{}</text>"##,
            x + col_w / 2.0,
            chart_top + chart_h + 42.0,
            day.label_dow
        ));
    }
    let window_note = match request.step_hours {
        None => "HI/LO over each calendar day".to_string(),
        // Single-value mode: columns land on the model cadence.
        Some(_) if single && cadence == 1 => "hourly values".to_string(),
        Some(_) if single => format!("one value per {cadence}-h step"),
        Some(step) => format!("HI/LO over each {step}-h bucket"),
    };
    let axis_note = if request.step_hours.is_none() {
        "local days"
    } else {
        "local time"
    };
    svg.push_str(&format!(
        r##"<text x="24" y="{:.0}" fill="#737c7b" font-size="11">{axis_note} (UTC{:+.0}) | {window_note} | California Wildfire Tracking — Weather Lab</text>"##,
        height - 14.0,
        request.utc_offset_hours
    ));
    svg.push_str("</svg>");
    Ok(svg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_cell_picks_the_closest_point() {
        // 2x3 grid.
        let lat = [30.0f32, 30.0, 30.0, 31.0, 31.0, 31.0];
        let lon = [-120.0f32, -119.0, -118.0, -120.0, -119.0, -118.0];
        let (cell, _) = nearest_cell(&lat, &lon, 30.9, -118.1);
        assert_eq!(cell, 5, "closest to (31, -118)");
        let (cell, _) = nearest_cell(&lat, &lon, 30.1, -120.2);
        assert_eq!(cell, 0);
    }

    #[test]
    fn bare_coords_titles_are_detected() {
        assert!(title_is_bare_coords("39.150, -123.210"));
        assert!(title_is_bare_coords(" 39.15°, -123.21° "));
        assert!(title_is_bare_coords("38.58,-121.49"));
        assert!(!title_is_bare_coords("Grapevine Fire"));
        assert!(!title_is_bare_coords("Station 12"));
        assert!(!title_is_bare_coords(""));
        assert!(!title_is_bare_coords("   "));
        assert!(!title_is_bare_coords("- - -"), "no digits at all");
    }

    #[test]
    fn point_titles_get_the_nearest_town() {
        // On Sacramento's Census internal point.
        let near = nearest_place(38.5677, -121.4682).expect("gazetteer is nonempty");
        assert_eq!(near.label, "Sacramento, CA");

        // The Lab's bare-coords title becomes coords + town phrase.
        let title = point_title(
            Some("38.568, -121.468"),
            "38.5677, -121.4682",
            Some(&near),
            75,
        );
        assert!(title.starts_with("38.5677, -121.4682 · "), "{title}");
        assert!(title.contains("Sacramento, CA"), "{title}");

        // Custom titles keep their text; a redundant town suffix is skipped.
        let title = point_title(Some("Sacramento Exec Airport"), "x", Some(&near), 75);
        assert_eq!(title, "Sacramento Exec Airport");

        // Fire names gain the phrase when it fits.
        let title = point_title(Some("Grapevine Fire"), "x", Some(&near), 75);
        assert!(title.starts_with("Grapevine Fire · "), "{title}");
        // ...but not when the strip would overflow.
        let title = point_title(Some("Grapevine Fire"), "x", Some(&near), 20);
        assert_eq!(title, "Grapevine Fire");

        // No gazetteer hit falls back to plain coords.
        let title = point_title(None, "38.6000, -121.3000", None, 75);
        assert_eq!(title, "38.6000, -121.3000");
    }

    #[test]
    fn valid_labels_roll_dates_and_weekdays() {
        // 2026-07-01 is a Wednesday; 00Z run + F30 = 06Z Thu Jul 2.
        let (hod, label) = valid_label("20260701", 0, 30);
        assert_eq!(hod, 6);
        assert_eq!(label, "Thu 7/2");
        let (hod, label) = valid_label("20261231", 12, 13);
        assert_eq!(hod, 1);
        assert_eq!(label, "Fri 1/1");
    }

    #[test]
    fn nice_steps_are_decade_multiples() {
        assert_eq!(nice_step(10.0, 5), 2.0);
        assert_eq!(nice_step(100.0, 4), 25.0);
        assert!((nice_step(3.7, 4) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn night_spans_pair_sunset_with_next_sunrise() {
        // Sacramento July (UTC): sunset 03:34, sunrise 12:45 — the night
        // stays inside ONE UTC day. Every span must be shorter than 24 h
        // and consecutive spans must not overlap (the old pairing made
        // 33-h overlapping spans that shaded the entire plot).
        let spans = night_spans_fh(12.75, 3.55, 0, 2);
        assert!(!spans.is_empty());
        for (start, end) in &spans {
            assert!(end - start < 24.0, "span {start}..{end} too long");
            assert!((end - start - 9.2).abs() < 0.5, "span {start}..{end}");
        }
        for pair in spans.windows(2) {
            assert!(pair[1].0 >= pair[0].1, "spans overlap: {pair:?}");
        }
        // Europe-like longitudes (sunset 20:00, sunrise 04:00 UTC):
        // night straddles the day boundary; behavior unchanged.
        let spans = night_spans_fh(4.0, 20.0, 0, 2);
        for (start, end) in &spans {
            assert!((end - start - 8.0).abs() < 1e-9, "span {start}..{end}");
        }
    }

    #[test]
    fn sunrise_sunset_bracket_sacramento_summer() {
        // Sacramento, July 1 (DOY 182): sunrise ~12:45 UTC, sunset ~03:34 UTC.
        let (sunrise, sunset) = sunrise_sunset_utc(38.58, -121.49, 182).expect("sun crosses");
        assert!((sunrise - 12.75).abs() < 0.35, "sunrise {sunrise}");
        assert!((sunset - 3.55).abs() < 0.35, "sunset {sunset}");
        assert_eq!(no_leap_doy(7, 1), 182);
        assert_eq!(no_leap_doy(2, 29), 59, "leap day folds");
    }

    #[test]
    fn thermo_matches_frozen_formulas() {
        assert!((vpd_kpa(30.0, 10.0) - 3.02).abs() < 0.02);
        assert_eq!(c_to_f(0.0), 32.0);
        assert!((to_c(300.15, "K") - 27.0).abs() < 1e-9);
    }
}

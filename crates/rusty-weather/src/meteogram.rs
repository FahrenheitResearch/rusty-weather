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

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;

const MS_TO_MPH: f64 = 2.236_936;

pub const METEOGRAM_PANELS: &[&str] = &["temp", "rh", "vpd", "wind", "fuels", "smoke"];

#[derive(Debug, Clone)]
pub struct MeteogramRequest {
    pub lat: f64,
    pub lon: f64,
    pub panels: Vec<String>,
    pub title: Option<String>,
}

struct HourSample {
    hour: u16,
    values: BTreeMap<&'static str, f64>,
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

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// One data series drawn in a panel.
struct Series<'a> {
    key: &'a str,
    label: &'a str,
    color: &'a str,
    dashed: bool,
    right_axis: bool,
}

pub struct MeteogramOutput {
    pub svg: String,
    pub cell_lat: f64,
    pub cell_lon: f64,
    pub hours: usize,
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
        for &name in SAMPLED_VARS {
            let Some(var) = reader.variable(name) else { continue };
            let units = var.units.clone();
            let Ok(window) = reader.read_window_2d(name, cx, cy, cx + 1, cy + 1) else {
                continue;
            };
            let raw = f64::from(window.values[0]);
            if !raw.is_finite() {
                continue;
            }
            let value = match name {
                "temperature_2m" | "dewpoint_2m" => c_to_f(to_c(raw, &units)),
                "u_10m" | "v_10m" | "wind_gust_10m" => raw * MS_TO_MPH,
                _ => raw,
            };
            values.insert(name, value);
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
            values.insert("vpd", vpd);
            if let (Some(&u), Some(&v)) = (values.get("u_10m"), values.get("v_10m")) {
                let wind_mph = (u * u + v * v).sqrt();
                values.insert("wind", wind_mph);
                values.insert("hdw_wind", vpd * wind_mph / MS_TO_MPH);
            }
            if let Some(&gust) = values.get("wind_gust_10m") {
                values.insert("hdw_gust", vpd * gust / MS_TO_MPH);
            }
        }
        samples.push(HourSample { hour, values });
    }
    if samples.len() < 2 {
        return Err("fewer than 2 hours sampled".to_string());
    }

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
    if panels.is_empty() {
        return Err(format!(
            "no valid panels requested; choose from {}",
            METEOGRAM_PANELS.join(",")
        ));
    }

    const W: f64 = 1180.0;
    const ML: f64 = 58.0;
    const MR: f64 = 58.0;
    const HEADER: f64 = 66.0;
    const PANEL_H: f64 = 148.0;
    const PANEL_GAP: f64 = 10.0;
    const AXIS_H: f64 = 40.0;
    let plot_w = W - ML - MR;
    let total_h = HEADER + panels.len() as f64 * (PANEL_H + PANEL_GAP) + AXIS_H;

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
    let title = request
        .title
        .clone()
        .unwrap_or_else(|| format!("{:.4}, {:.4}", request.lat, request.lon));
    let elev = elevation_ft
        .map(|ft| format!(" | {} ft", ft.round() as i64))
        .unwrap_or_default();
    svg.push_str(&format!(
        r##"<text x="{ML}" y="30" fill="#f2e7d5" font-size="19" font-weight="700">POINT METEOGRAM — {}</text>"##,
        xml_escape(&title)
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

    // Panel definitions.
    let panel_series: BTreeMap<&str, (&str, Vec<Series>)> = BTreeMap::from([
        (
            "temp",
            ("Temperature / Dewpoint (F)", vec![
                Series { key: "temperature_2m", label: "T", color: "#ff6d4d", dashed: false, right_axis: false },
                Series { key: "dewpoint_2m", label: "Td", color: "#58c98f", dashed: false, right_axis: false },
            ]),
        ),
        (
            "rh",
            ("Relative Humidity (%)", vec![Series {
                key: "rh_2m", label: "RH", color: "#58c98f", dashed: false, right_axis: false,
            }]),
        ),
        (
            "vpd",
            ("VPD (kPa) / Surface HDW", vec![
                Series { key: "vpd", label: "VPD", color: "#ffb454", dashed: false, right_axis: false },
                Series { key: "hdw_wind", label: "HDW-w", color: "#c77dff", dashed: true, right_axis: true },
                Series { key: "hdw_gust", label: "HDW-g", color: "#8a5fd0", dashed: true, right_axis: true },
            ]),
        ),
        (
            "wind",
            ("Wind / Gust (mph)", vec![
                Series { key: "wind", label: "sustained", color: "#7fb8e6", dashed: false, right_axis: false },
                Series { key: "wind_gust_10m", label: "gust", color: "#ff8a00", dashed: false, right_axis: false },
            ]),
        ),
        (
            "fuels",
            ("ERC / 10-h Fuel Moisture (%)", vec![
                Series { key: "erc", label: "ERC", color: "#e05545", dashed: false, right_axis: false },
                Series { key: "dead_fuel_moisture_10h", label: "10-h FM", color: "#58c98f", dashed: false, right_axis: true },
            ]),
        ),
        (
            "smoke",
            ("Near-Surface Smoke (ug/m3)", vec![Series {
                key: "smoke_8m", label: "smoke 8m", color: "#b0a695", dashed: false, right_axis: false,
            }]),
        ),
    ]);

    for (panel_index, panel) in panels.iter().enumerate() {
        let Some((panel_title, series_list)) = panel_series.get(panel) else { continue };
        let top = HEADER + panel_index as f64 * (PANEL_H + PANEL_GAP);
        svg.push_str(&format!(
            r##"<rect x="{ML}" y="{top}" width="{plot_w}" height="{PANEL_H}" fill="#1b1611" stroke="#2a231b"/>"##
        ));
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
        let left_values: Vec<f64> = series_list
            .iter()
            .filter(|s| !s.right_axis)
            .flat_map(|s| get(s.key))
            .collect();
        let right_values: Vec<f64> = series_list
            .iter()
            .filter(|s| s.right_axis)
            .flat_map(|s| get(s.key))
            .collect();
        let left_bounds = axis_bounds(&left_values);
        let right_bounds = axis_bounds(&right_values);
        let pad_bounds = |bounds: (f64, f64)| {
            let pad = (bounds.1 - bounds.0) * 0.12;
            (bounds.0 - pad, bounds.1 + pad)
        };

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
                    fmt(tick)
                ));
                tick += step;
            }
            // RH critical reference lines.
            if *panel == "rh" {
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
                    fmt(tick)
                ));
                tick += step;
            }
        }

        // Series polylines (NaN breaks the path).
        for series in series_list {
            let bounds = if series.right_axis { right_bounds } else { left_bounds };
            let Some(bounds) = bounds else { continue };
            let (lo, hi) = pad_bounds(bounds);
            let values = get(series.key);
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

        // Panel title + legend.
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#d8cdbd" font-size="12" font-weight="700">{}</text>"##,
            ML + 8.0,
            top + 16.0,
            xml_escape(panel_title)
        ));
        let mut legend_x = ML + plot_w - 8.0;
        for series in series_list.iter().rev() {
            let label = format!("{} —", series.label);
            legend_x -= 7.0 * label.len() as f64 + 12.0;
            svg.push_str(&format!(
                r##"<text x="{legend_x:.1}" y="{:.1}" fill="{}" font-size="11">{}</text>"##,
                top + 16.0,
                series.color,
                xml_escape(&label)
            ));
        }
    }

    // Time axis: tick every 3 stored hours, heavier at 00Z with date label.
    let axis_y = HEADER + panels.len() as f64 * (PANEL_H + PANEL_GAP);
    for sample in &samples {
        let (hod, day_label) = valid_label(date_yyyymmdd, cycle_utc, sample.hour);
        let x = x_for(f64::from(sample.hour));
        let is_midnight = hod == 0;
        if is_midnight {
            svg.push_str(&format!(
                r##"<line x1="{x:.1}" y1="{HEADER}" x2="{x:.1}" y2="{axis_y:.1}" stroke="#40372c" stroke-width="1.2"/>"##
            ));
        }
        if hod % 3 == 0 {
            svg.push_str(&format!(
                r##"<text x="{x:.1}" y="{:.1}" fill="#a99c88" font-size="10" text-anchor="middle">{hod:02}Z</text>"##,
                axis_y + 14.0
            ));
        }
        if is_midnight || sample.hour == samples[0].hour {
            svg.push_str(&format!(
                r##"<text x="{x:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="middle">{day_label}</text>"##,
                axis_y + 28.0
            ));
        }
    }

    svg.push_str("</svg>");
    Ok(MeteogramOutput {
        svg,
        cell_lat,
        cell_lon,
        hours: samples.len(),
    })
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
    fn thermo_matches_frozen_formulas() {
        assert!((vpd_kpa(30.0, 10.0) - 3.02).abs() < 0.02);
        assert_eq!(c_to_f(0.0), 32.0);
        assert!((to_c(300.15, "K") - 27.0).abs() < 1e-9);
    }
}

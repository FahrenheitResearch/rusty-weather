//! Store-native vertical cross section along a user-drawn line.
//!
//! Slices the run's isobaric volumes (`temperature_iso`, `dewpoint_iso`,
//! `u_iso`/`v_iso`; 100–1000 hPa, 25 hPa step) along the line A→B and
//! renders one SVG panel: x = distance along the path (miles), y = log-p
//! pressure (1000 hPa at the bottom, 150 hPa at the top), a smooth color
//! fill of the chosen field (temperature °F / RH % / wind mph), a solid
//! terrain silhouette from `surface_pressure`, and true wind barbs from
//! `u_iso`/`v_iso`. Served as `GET /api/xsection` next to `/api/meteogram`,
//! sharing its query parsing, error shape, and dark IBM Plex Mono style.
//!
//! Path approximation (deliberate, documented in the plot footer): sample
//! points interpolate lat/lon AND fractional grid indices linearly between
//! the endpoint cells (`nearest_cell` at both ends). The HRRR Lambert grid
//! is near-uniform over CONUS, so mid-path drift is at most ~1–2 cells
//! (~3–6 km) — invisible at this product's ~200-column resolution. Each
//! column is sampled with `HourReader::read_profile_3d` (bilinear over the
//! four surrounding columns, chunk decodes deduped per call).

use std::path::Path;

use rustwx_products::places::nearest_place;
use rw_store::grid::GridFile;
use rw_store::reader::HourReader;

use super::meteogram::{
    MS_TO_MPH, nearest_cell, nice_step, saturation_vapor_pressure_hpa, valid_label, wind_barb_svg,
    xml_escape,
};

/// Columns sampled along the path.
const COLUMNS: usize = 200;
/// Log-p pressure axis: 1000 hPa at the bottom, 150 hPa at the top.
const P_BOTTOM_HPA: f64 = 1000.0;
const P_TOP_HPA: f64 = 150.0;
/// Barb decimation: every Nth column, every Mth level.
const BARB_COLUMN_STRIDE: usize = 14;
const BARB_LEVEL_STRIDE: usize = 3;

/// The honest 422 message when a stored hour has no isobaric volumes
/// (hours ingested before the `view-volumes` profile deployed).
const NO_VOLUME_MESSAGE: &str =
    "this hour predates vertical-volume storage — pick a newer run/hour";

pub const XSECTION_FIELDS: &[&str] = &["temperature", "rh", "wind"];

#[derive(Debug, Clone)]
pub struct XsectionRequest {
    pub lat0: f64,
    pub lon0: f64,
    pub lat1: f64,
    pub lon1: f64,
    /// "temperature" | "rh" | "wind" (see [`XSECTION_FIELDS`]).
    pub field: String,
    /// Forecast hour; `None` picks the newest stored hour carrying volumes.
    pub hour: Option<u16>,
    /// Hours to add to UTC for the local-time note (PDT = -7).
    pub utc_offset_hours: f64,
}

#[derive(Debug)]
pub struct XsectionOutput {
    pub svg: String,
    /// The sampled data (`format=json` on the API); carries the forecast
    /// hour actually rendered (resolving a `None` request hour).
    pub data: serde_json::Value,
}

// ---- pure geometry helpers ----

/// Great-circle distance in statute miles (haversine).
fn haversine_mi(lat0: f64, lon0: f64, lat1: f64, lon1: f64) -> f64 {
    const R_MI: f64 = 3958.7613;
    let (p0, p1) = (lat0.to_radians(), lat1.to_radians());
    let dp = (lat1 - lat0).to_radians();
    let dl = (lon1 - lon0).to_radians();
    let a = (dp / 2.0).sin().powi(2) + p0.cos() * p1.cos() * (dl / 2.0).sin().powi(2);
    2.0 * R_MI * a.sqrt().asin()
}

/// One sampled column along the A→B line.
#[derive(Debug, Clone, Copy)]
struct PathSample {
    /// Fractional grid coordinates (linear between the endpoint cells).
    fx: f64,
    fy: f64,
    lat: f64,
    lon: f64,
    /// Cumulative great-circle distance from A, statute miles.
    dist_mi: f64,
}

/// `n` samples from A to B: lat/lon and fractional cell indices both
/// interpolate linearly (see the module docs for why that is accurate
/// enough on the HRRR grid). Endpoints land exactly on the endpoint
/// cells; distances are cumulative and strictly monotone for distinct
/// endpoints.
fn sample_path(
    a: (f64, f64),
    b: (f64, f64),
    cell_a: (f64, f64),
    cell_b: (f64, f64),
    n: usize,
) -> Vec<PathSample> {
    let n = n.max(2);
    let mut samples = Vec::with_capacity(n);
    let mut dist = 0.0;
    let (mut prev_lat, mut prev_lon) = a;
    for i in 0..n {
        let t = i as f64 / (n - 1) as f64;
        let lat = a.0 + t * (b.0 - a.0);
        let lon = a.1 + t * (b.1 - a.1);
        dist += haversine_mi(prev_lat, prev_lon, lat, lon);
        (prev_lat, prev_lon) = (lat, lon);
        samples.push(PathSample {
            fx: cell_a.0 + t * (cell_b.0 - cell_a.0),
            fy: cell_a.1 + t * (cell_b.1 - cell_a.1),
            lat,
            lon,
            dist_mi: dist,
        });
    }
    samples
}

/// Log-p vertical mapping for the panel: `y(p_bottom)` is the panel
/// bottom, `y(p_top)` the panel top, pressures clamped into the axis.
#[derive(Debug, Clone, Copy)]
struct PressureAxis {
    top: f64,
    height: f64,
    p_bottom: f64,
    p_top: f64,
}

impl PressureAxis {
    fn y(&self, p_hpa: f64) -> f64 {
        let p = if p_hpa.is_finite() {
            p_hpa.clamp(self.p_top, self.p_bottom)
        } else {
            self.p_bottom
        };
        self.top + self.height * (p.ln() - self.p_top.ln()) / (self.p_bottom.ln() - self.p_top.ln())
    }

    fn bottom(&self) -> f64 {
        self.top + self.height
    }
}

/// Terrain silhouette vertices: panel-bottom corners bracketing one
/// ground-line point per column (surface pressure on the log-p axis;
/// NaN/off-axis pressures clamp to the panel bottom so coastal columns
/// sit flat on the axis).
fn terrain_polygon(xs: &[f64], psfc_hpa: &[f64], axis: &PressureAxis) -> Vec<(f64, f64)> {
    let mut points = Vec::with_capacity(xs.len() + 2);
    let bottom = axis.bottom();
    if let (Some(&x_first), Some(&x_last)) = (xs.first(), xs.last()) {
        points.push((x_first, bottom));
        for (&x, &p) in xs.iter().zip(psfc_hpa.iter()) {
            points.push((x, axis.y(p)));
        }
        points.push((x_last, bottom));
    }
    points
}

// ---- color ramps ----
// Anchored absolute ramps that keep the site's temperature-palette look
// (the warm-side hexes are the outlook card's `temp_color` stops) and
// extend it smoothly into upper-air cold. Linear RGB interpolation
// between anchors.

const TEMP_RAMP_F: &[(f64, [u8; 3])] = &[
    (-110.0, [0x1c, 0x10, 0x3c]),
    (-80.0, [0x2f, 0x1f, 0x63]),
    (-55.0, [0x4a, 0x37, 0x96]),
    (-30.0, [0x7c, 0x66, 0xc9]),
    (-10.0, [0xb7, 0xa6, 0xf0]),
    (7.0, [0x8f, 0x9f, 0xe8]),
    (23.0, [0x6f, 0x9b, 0xd1]),
    (38.0, [0x7f, 0xc4, 0xd8]),
    (50.0, [0x9c, 0xd6, 0xa4]),
    (60.0, [0xd3, 0xe0, 0x7f]),
    (68.0, [0xf5, 0xe0, 0x5c]),
    (76.0, [0xf5, 0xb5, 0x3c]),
    (84.0, [0xef, 0x86, 0x33]),
    (91.0, [0xe0, 0x51, 0x2e]),
    (99.0, [0xb3, 0x1f, 0x1f]),
    (106.0, [0x7a, 0x2f, 0x1d]),
    (115.0, [0x8b, 0x3a, 0x94]),
];

/// Dry brown → site green → moist teal.
const RH_RAMP_PCT: &[(f64, [u8; 3])] = &[
    (0.0, [0x4a, 0x30, 0x18]),
    (10.0, [0x6b, 0x44, 0x23]),
    (25.0, [0x9c, 0x7a, 0x44]),
    (40.0, [0xc2, 0xb2, 0x80]),
    (55.0, [0x9a, 0xc5, 0x75]),
    (70.0, [0x58, 0xc9, 0x8f]),
    (85.0, [0x37, 0xb3, 0xa5]),
    (100.0, [0x2f, 0xa3, 0xc7]),
];

/// Calm panel-dark → site wind blue → warning warm tones.
const WIND_RAMP_MPH: &[(f64, [u8; 3])] = &[
    (0.0, [0x1c, 0x27, 0x33]),
    (10.0, [0x2e, 0x55, 0x77]),
    (20.0, [0x4f, 0x8c, 0xc0]),
    (30.0, [0x7f, 0xb8, 0xe6]),
    (40.0, [0x9c, 0xd6, 0xa4]),
    (50.0, [0xf5, 0xe0, 0x5c]),
    (65.0, [0xf5, 0xb5, 0x3c]),
    (80.0, [0xef, 0x86, 0x33]),
    (100.0, [0xe0, 0x51, 0x2e]),
    (125.0, [0x8b, 0x3a, 0x94]),
];

/// Piecewise-linear RGB between ramp anchors, clamped at the ends.
fn ramp_rgb(stops: &[(f64, [u8; 3])], v: f64) -> [u8; 3] {
    if !v.is_finite() || v <= stops[0].0 {
        return stops[0].1;
    }
    for pair in stops.windows(2) {
        let (v0, c0) = pair[0];
        let (v1, c1) = pair[1];
        if v <= v1 {
            let t = ((v - v0) / (v1 - v0)).clamp(0.0, 1.0);
            let lerp = |a: u8, b: u8| (f64::from(a) + t * (f64::from(b) - f64::from(a))).round() as u8;
            return [lerp(c0[0], c1[0]), lerp(c0[1], c1[1]), lerp(c0[2], c1[2])];
        }
    }
    stops[stops.len() - 1].1
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

/// Dark or cream overlay ink for a fill color (same luminance split the
/// outlook card uses for its number strips).
fn ink_on(rgb: [u8; 3]) -> &'static str {
    let lum = 0.299 * f64::from(rgb[0]) + 0.587 * f64::from(rgb[1]) + 0.114 * f64::from(rgb[2]);
    if lum > 150.0 { "#141414" } else { "#f2e7d5" }
}

// ---- 2D window sampling (surface pressure along the path) ----

/// Bilinear sample of a `Window2D` at GLOBAL fractional grid coordinates,
/// with the same finite-corner weight renormalization `read_profile_3d`
/// applies. Returns NaN outside the window or over all-NaN corners.
fn window_bilinear(win: &rw_store::reader::Window2D, fx: f64, fy: f64) -> f64 {
    if win.nx == 0 || win.ny == 0 {
        return f64::NAN;
    }
    let lx = (fx - win.x0 as f64).clamp(0.0, (win.nx - 1) as f64);
    let ly = (fy - win.y0 as f64).clamp(0.0, (win.ny - 1) as f64);
    let (x0, x1) = (lx.floor() as usize, lx.ceil() as usize);
    let (y0, y1) = (ly.floor() as usize, ly.ceil() as usize);
    let wx = lx - x0 as f64;
    let wy = ly - y0 as f64;
    let corners = [
        (x0, y0, (1.0 - wx) * (1.0 - wy)),
        (x1, y0, wx * (1.0 - wy)),
        (x0, y1, (1.0 - wx) * wy),
        (x1, y1, wx * wy),
    ];
    let mut weight_sum = 0.0;
    let mut value_sum = 0.0;
    for (ix, iy, weight) in corners {
        let value = f64::from(win.values[iy * win.nx + ix]);
        if value.is_finite() {
            weight_sum += weight;
            value_sum += weight * value;
        }
    }
    if weight_sum > 0.0 { value_sum / weight_sum } else { f64::NAN }
}

// ---- field catalog ----

struct FieldSpec {
    key: &'static str,
    /// Header phrase, e.g. "TEMPERATURE (°F)".
    title: &'static str,
    units: &'static str,
    ramp: &'static [(f64, [u8; 3])],
    /// 3D store variables the fill needs (barbs want u/v separately).
    vars: &'static [&'static str],
}

const FIELD_SPECS: &[FieldSpec] = &[
    FieldSpec {
        key: "temperature",
        title: "TEMPERATURE",
        units: "°F",
        ramp: TEMP_RAMP_F,
        vars: &["temperature_iso"],
    },
    FieldSpec {
        key: "rh",
        title: "RELATIVE HUMIDITY",
        units: "%",
        ramp: RH_RAMP_PCT,
        vars: &["temperature_iso", "dewpoint_iso"],
    },
    FieldSpec {
        key: "wind",
        title: "WIND SPEED",
        units: "mph",
        ramp: WIND_RAMP_MPH,
        vars: &["u_iso", "v_iso"],
    },
];

/// Endpoint header phrase: gazetteer place when one is near, coordinates
/// otherwise. `short` drops the distance/bearing prefix when the full
/// title would overflow the strip.
fn endpoint_phrase(lat: f64, lon: f64, short: bool) -> String {
    match nearest_place(lat, lon) {
        Some(near) if short => near.label,
        Some(near) => near.describe(),
        None => format!("{lat:.2}, {lon:.2}"),
    }
}

/// RH% from temperature/dewpoint in °C (Magnus e_s, the frozen atlas
/// formula), clamped to 0–100.
fn rh_percent(t_c: f64, td_c: f64) -> f64 {
    (100.0 * saturation_vapor_pressure_hpa(td_c) / saturation_vapor_pressure_hpa(t_c))
        .clamp(0.0, 100.0)
}

#[allow(clippy::too_many_arguments)]
pub fn render_xsection_svg(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    request: &XsectionRequest,
) -> Result<XsectionOutput, String> {
    let spec = FIELD_SPECS
        .iter()
        .find(|spec| spec.key == request.field)
        .ok_or_else(|| format!("field must be one of {}", XSECTION_FIELDS.join(", ")))?;

    let run_dir = store_root.join(model_slug).join(run_slug);
    let grid =
        GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| format!("open grid: {err}"))?;
    for (label, lat, lon) in [
        ("A", request.lat0, request.lon0),
        ("B", request.lat1, request.lon1),
    ] {
        if !(lat.is_finite() && lon.is_finite()) {
            return Err(format!("endpoint {label} lat/lon must be finite"));
        }
    }
    let (cell_a, d2_a) = nearest_cell(&grid.lat, &grid.lon, request.lat0, request.lon0);
    let (cell_b, d2_b) = nearest_cell(&grid.lat, &grid.lon, request.lat1, request.lon1);
    if d2_a.sqrt() > 0.5 || d2_b.sqrt() > 0.5 {
        return Err("an endpoint is more than ~50 km outside the model grid".to_string());
    }
    let total_mi = haversine_mi(request.lat0, request.lon0, request.lat1, request.lon1);
    if total_mi < 2.0 {
        return Err("endpoints are (nearly) the same point — draw a longer line".to_string());
    }
    let cell_af = ((cell_a % grid.nx) as f64, (cell_a / grid.nx) as f64);
    let cell_bf = ((cell_b % grid.nx) as f64, (cell_b / grid.nx) as f64);
    let samples = sample_path(
        (request.lat0, request.lon0),
        (request.lat1, request.lon1),
        cell_af,
        cell_bf,
        COLUMNS,
    );

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
        spec.vars.iter().all(|name| {
            reader.variable(name).is_some_and(|var| var.kind == "pressure3d")
        })
    };
    // Resolve the hour: explicit request (with honest errors), or the
    // newest stored hour that actually carries the needed volumes.
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

    // ---- sample the volumes along the path ----
    // One bilinear profile per column per variable; ~200 columns × 25 hPa
    // levels. Wind is drawn (barbs) on every field when u/v volumes exist.
    let profile_var = |name: &str| -> Result<(Vec<f64>, Vec<Vec<f32>>), String> {
        let var = reader
            .variable(name)
            .ok_or_else(|| NO_VOLUME_MESSAGE.to_string())?;
        let levels: Vec<f64> = var.levels_hpa.iter().map(|&l| f64::from(l)).collect();
        let columns = samples
            .iter()
            .map(|s| reader.read_profile_3d(name, s.fx, s.fy))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| format!("read {name}: {err}"))?;
        Ok((levels, columns))
    };

    // Barb wind: read u/v once — the overlay on every field, and the fill
    // itself for field=wind. None when a volume is absent or levels differ.
    let barb_wind: Option<(Vec<f64>, Vec<Vec<f32>>, Vec<Vec<f32>>)> = (|| {
        let (levels_u, u) = profile_var("u_iso").ok()?;
        let (levels_v, v) = profile_var("v_iso").ok()?;
        (levels_u == levels_v).then_some((levels_u, u, v))
    })();

    let (levels_hpa, values): (Vec<f64>, Vec<Vec<f64>>) = match spec.key {
        "temperature" => {
            let (levels, t) = profile_var("temperature_iso")?;
            let cols = t
                .iter()
                .map(|col| col.iter().map(|&k| f64::from(k) * 9.0 / 5.0 - 459.67).collect())
                .collect();
            (levels, cols)
        }
        "rh" => {
            let (levels, t) = profile_var("temperature_iso")?;
            let (levels_td, td) = profile_var("dewpoint_iso")?;
            if levels != levels_td {
                return Err(
                    "temperature_iso and dewpoint_iso carry different levels in this hour".into(),
                );
            }
            let cols = t
                .iter()
                .zip(td.iter())
                .map(|(tc, tdc)| {
                    tc.iter()
                        .zip(tdc.iter())
                        .map(|(&tk, &tdk)| {
                            let (t_c, td_c) = (f64::from(tk) - 273.15, f64::from(tdk) - 273.15);
                            if t_c.is_finite() && td_c.is_finite() {
                                rh_percent(t_c, td_c)
                            } else {
                                f64::NAN
                            }
                        })
                        .collect()
                })
                .collect();
            (levels, cols)
        }
        _ => {
            let (levels, u, v) = barb_wind
                .as_ref()
                .ok_or("u_iso and v_iso are unavailable or carry different levels in this hour")?;
            let cols = u
                .iter()
                .zip(v.iter())
                .map(|(uc, vc)| {
                    uc.iter()
                        .zip(vc.iter())
                        .map(|(&u, &v)| {
                            f64::from(u).hypot(f64::from(v)) * MS_TO_MPH
                        })
                        .collect()
                })
                .collect();
            (levels.clone(), cols)
        }
    };

    // ---- surface pressure along the path (terrain) ----
    // One windowed 2D read over the path's bounding box, then bilinear
    // per column. Pa in the store → hPa here (skew-T convention).
    let psfc_var = reader
        .variable("surface_pressure")
        .ok_or_else(|| "store hour lacks surface_pressure (terrain)".to_string())?;
    let psfc_units = psfc_var.units.clone();
    let (min_fx, max_fx) = samples
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| (lo.min(s.fx), hi.max(s.fx)));
    let (min_fy, max_fy) = samples
        .iter()
        .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), s| (lo.min(s.fy), hi.max(s.fy)));
    let x0 = (min_fx.floor() as usize).saturating_sub(1);
    let y0 = (min_fy.floor() as usize).saturating_sub(1);
    let x1 = ((max_fx.ceil() as usize) + 2).min(grid.nx);
    let y1 = ((max_fy.ceil() as usize) + 2).min(grid.ny);
    let psfc_window = reader
        .read_window_2d("surface_pressure", x0, y0, x1, y1)
        .map_err(|err| format!("read surface_pressure: {err}"))?;
    let to_hpa = if psfc_units.trim().eq_ignore_ascii_case("pa") { 0.01 } else { 1.0 };
    let psfc_hpa: Vec<f64> = samples
        .iter()
        .map(|s| window_bilinear(&psfc_window, s.fx, s.fy) * to_hpa)
        .collect();

    // ---- layout ----
    const W: f64 = 1180.0;
    const ML: f64 = 58.0;
    const MR: f64 = 58.0;
    const HEADER: f64 = 66.0;
    const PANEL_H: f64 = 470.0;
    const LEGEND_GAP: f64 = 40.0;
    const LEGEND_H: f64 = 14.0;
    const FOOTER_H: f64 = 64.0;
    let plot_w = W - ML - MR;
    let panel_top = HEADER;
    let panel_bottom = panel_top + PANEL_H;
    let legend_y = panel_bottom + LEGEND_GAP;
    let total_h = legend_y + LEGEND_H + FOOTER_H;
    let axis = PressureAxis {
        top: panel_top,
        height: PANEL_H,
        p_bottom: P_BOTTOM_HPA,
        p_top: P_TOP_HPA,
    };
    let x_for = |dist: f64| ML + dist / total_mi * plot_w;

    let mut svg = String::with_capacity(256 * 1024);
    svg.push_str(&format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{total_h}" viewBox="0 0 {W} {total_h}" font-family="'IBM Plex Mono','Consolas',monospace">"##
    ));
    svg.push_str(&format!(r##"<rect width="{W}" height="{total_h}" fill="#14100c"/>"##));
    svg.push_str(&format!(
        r##"<clipPath id="panel"><rect x="{ML}" y="{panel_top}" width="{plot_w}" height="{PANEL_H}"/></clipPath>"##
    ));

    // Header: A → B phrases from the gazetteer, shortened if they overflow.
    let title_places = |short: bool| {
        format!(
            "{} → {}",
            endpoint_phrase(request.lat0, request.lon0, short),
            endpoint_phrase(request.lat1, request.lon1, short)
        )
    };
    let mut places = title_places(false);
    if places.chars().count() > 62 {
        places = title_places(true);
    }
    let title = format!("VERTICAL CROSS SECTION — {places}");
    svg.push_str(&format!(
        r##"<text x="{ML}" y="30" fill="#f2e7d5" font-size="19" font-weight="700">{}</text>"##,
        xml_escape(&title)
    ));
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="30" fill="#8d8171" font-size="11" text-anchor="end">cafire.org/weather</text>"##,
        W - MR
    ));
    let (hod, day_label) = valid_label(date_yyyymmdd, cycle_utc, hour);
    let local = (f64::from(hod) + request.utc_offset_hours).rem_euclid(24.0) as u32;
    svg.push_str(&format!(
        r##"<text x="{ML}" y="50" fill="#8d8171" font-size="12">{model} {date} {cycle:02}Z | F{hour:03} | valid {day_label} {hod:02}Z ({local:02} local) | {field} ({units}) fill | barbs kt | {total_mi:.0} mi path</text>"##,
        model = model_slug.to_uppercase(),
        date = date_yyyymmdd,
        cycle = cycle_utc,
        field = spec.title,
        units = spec.units,
    ));

    // Panel background.
    svg.push_str(&format!(
        r##"<rect x="{ML}" y="{panel_top}" width="{plot_w}" height="{PANEL_H}" fill="#1b1611" stroke="#3a3128"/>"##
    ));
    svg.push_str(r##"<g clip-path="url(#panel)">"##);

    // Column x edges: midpoints between neighbors, panel edges outside.
    let xs: Vec<f64> = samples.iter().map(|s| x_for(s.dist_mi)).collect();
    let x_edge = |i: usize| -> (f64, f64) {
        let left = if i == 0 { ML } else { (xs[i - 1] + xs[i]) / 2.0 };
        let right = if i + 1 == xs.len() { ML + plot_w } else { (xs[i] + xs[i + 1]) / 2.0 };
        (left, right)
    };
    // Level y edges: geometric-mean (log-p midpoint) boundaries; the
    // bottom band ends at the axis floor, the top band at its own level.
    let level_edges = |k: usize| -> (f64, f64) {
        let p = levels_hpa[k];
        let below = if k == 0 { P_BOTTOM_HPA.max(p) } else { (p * levels_hpa[k - 1]).sqrt() };
        let above = if k + 1 == levels_hpa.len() { p } else { (p * levels_hpa[k + 1]).sqrt() };
        (axis.y(below), axis.y(above))
    };

    // FILL: per-(column, level) cells, quantized to 1 display unit and
    // run-length merged along each level row (a smooth gradient collapses
    // to a few dozen rects per row instead of 200). Cells overlap 0.3 px
    // so anti-aliasing can't draw hairline seams on the dark background.
    for k in 0..levels_hpa.len() {
        if levels_hpa[k] < P_TOP_HPA - 25.0 {
            continue; // fully above the axis top: nothing visible
        }
        let (y_lo, y_hi) = level_edges(k);
        let mut run_start = 0;
        let mut run_rgb: Option<[u8; 3]> = None;
        let flush = |start: usize, end: usize, rgb: Option<[u8; 3]>, svg: &mut String| {
            let Some(rgb) = rgb else { return };
            let (x_left, _) = x_edge(start);
            let (_, x_right) = x_edge(end);
            svg.push_str(&format!(
                r##"<rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" fill="{}"/>"##,
                x_left - 0.3,
                y_hi - 0.3,
                x_right - x_left + 0.6,
                y_lo - y_hi + 0.6,
                hex(rgb)
            ));
        };
        for (i, column) in values.iter().enumerate() {
            let value = column[k];
            let rgb = value.is_finite().then(|| ramp_rgb(spec.ramp, value.round()));
            if rgb != run_rgb {
                if i > run_start {
                    flush(run_start, i - 1, run_rgb, &mut svg);
                }
                run_start = i;
                run_rgb = rgb;
            }
        }
        flush(run_start, values.len() - 1, run_rgb, &mut svg);
    }

    // TERRAIN: solid silhouette from surface pressure, with a ground line.
    let terrain = terrain_polygon(&xs, &psfc_hpa, &axis);
    let terrain_points: Vec<String> =
        terrain.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect();
    svg.push_str(&format!(
        r##"<polygon points="{}" fill="#0b0806"/>"##,
        terrain_points.join(" ")
    ));
    let ground_points: Vec<String> = terrain[1..terrain.len() - 1]
        .iter()
        .map(|(x, y)| format!("{x:.1},{y:.1}"))
        .collect();
    svg.push_str(&format!(
        r##"<polyline points="{}" fill="none" stroke="#7a6a55" stroke-width="1.4"/>"##,
        ground_points.join(" ")
    ));

    // Gridlines over fill + terrain (subtle).
    const P_TICKS: [f64; 11] = [1000.0, 925.0, 850.0, 700.0, 600.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0];
    for p in P_TICKS {
        let y = axis.y(p);
        svg.push_str(&format!(
            r##"<line x1="{ML}" y1="{y:.1}" x2="{:.1}" y2="{y:.1}" stroke="#f2e7d5" stroke-width="1" stroke-opacity="0.10"/>"##,
            ML + plot_w
        ));
    }
    let mi_step = nice_step(total_mi, 8);
    let mut tick = mi_step;
    while tick < total_mi {
        let x = x_for(tick);
        svg.push_str(&format!(
            r##"<line x1="{x:.1}" y1="{panel_top}" x2="{x:.1}" y2="{panel_bottom:.1}" stroke="#f2e7d5" stroke-width="1" stroke-opacity="0.07"/>"##
        ));
        tick += mi_step;
    }

    // WIND BARBS: decimated columns × levels, in knots via the shared
    // meteogram glyph (it converts mph → kt). Below-ground and off-axis
    // levels are skipped; ink flips dark/cream per the fill under it.
    if let Some((barb_levels, u_cols, v_cols)) = &barb_wind {
        for (i, sample) in samples.iter().enumerate().step_by(BARB_COLUMN_STRIDE) {
            let x = x_for(sample.dist_mi);
            if x < ML + 10.0 || x > ML + plot_w - 10.0 {
                continue; // keep rotated glyphs inside the frame
            }
            for (k, &p) in barb_levels.iter().enumerate() {
                if k % BARB_LEVEL_STRIDE != 0 || !(P_TOP_HPA..=P_BOTTOM_HPA).contains(&p) {
                    continue;
                }
                if psfc_hpa[i].is_finite() && p >= psfc_hpa[i] {
                    continue; // below ground
                }
                let (u, v) = (f64::from(u_cols[i][k]), f64::from(v_cols[i][k]));
                if !(u.is_finite() && v.is_finite()) {
                    continue;
                }
                let speed_mph = u.hypot(v) * MS_TO_MPH;
                let dir_from = (-u).atan2(-v).to_degrees().rem_euclid(360.0);
                let fill_value = values[i][k.min(values[i].len() - 1)];
                let ink = if fill_value.is_finite() {
                    ink_on(ramp_rgb(spec.ramp, fill_value.round()))
                } else {
                    "#f2e7d5"
                };
                svg.push_str(&wind_barb_svg(x, axis.y(p), dir_from, speed_mph, ink));
            }
        }
    }

    // Endpoint tags inside the panel's top corners.
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="{:.1}" fill="#f2e7d5" font-size="13" font-weight="700">A</text>"##,
        ML + 8.0,
        panel_top + 18.0
    ));
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="{:.1}" fill="#f2e7d5" font-size="13" font-weight="700" text-anchor="end">B</text>"##,
        ML + plot_w - 8.0,
        panel_top + 18.0
    ));
    svg.push_str("</g>");

    // Pressure tick labels (left, outside the clip).
    for p in P_TICKS {
        let y = axis.y(p);
        svg.push_str(&format!(
            r##"<text x="{:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="end">{}</text>"##,
            ML - 6.0,
            y + 3.0,
            p as i64
        ));
    }
    // Distance tick labels + endpoint letters under the x axis.
    let mut tick = 0.0;
    while tick <= total_mi + 1e-9 {
        let x = x_for(tick.min(total_mi));
        svg.push_str(&format!(
            r##"<text x="{x:.1}" y="{:.1}" fill="#a99c88" font-size="10" text-anchor="middle">{}</text>"##,
            panel_bottom + 16.0,
            tick.round() as i64
        ));
        tick += mi_step;
    }
    svg.push_str(&format!(
        r##"<text x="{ML}" y="{:.1}" fill="#8d8171" font-size="10">A: {}</text>"##,
        panel_bottom + 30.0,
        xml_escape(&format!("{:.3}, {:.3}", request.lat0, request.lon0))
    ));
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="end">B: {}</text>"##,
        ML + plot_w,
        panel_bottom + 30.0,
        xml_escape(&format!("{:.3}, {:.3}", request.lat1, request.lon1))
    ));

    // Legend: continuous ramp bar over the section's own value range
    // (RH pins to 0–100).
    let finite: Vec<f64> = values
        .iter()
        .flat_map(|col| col.iter().copied())
        .filter(|v| v.is_finite())
        .collect();
    let (legend_lo, legend_hi) = match spec.key {
        "rh" => (0.0, 100.0),
        _ => {
            let lo = finite.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = finite.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            if lo.is_finite() && hi > lo {
                ((lo / 10.0).floor() * 10.0, (hi / 10.0).ceil() * 10.0)
            } else {
                (0.0, 100.0)
            }
        }
    };
    const LEGEND_W: f64 = 320.0;
    const LEGEND_SEGMENTS: usize = 64;
    for seg in 0..LEGEND_SEGMENTS {
        let t = seg as f64 / (LEGEND_SEGMENTS - 1) as f64;
        let value = legend_lo + t * (legend_hi - legend_lo);
        let seg_w = LEGEND_W / LEGEND_SEGMENTS as f64;
        svg.push_str(&format!(
            r##"<rect x="{:.1}" y="{legend_y:.1}" width="{:.1}" height="{LEGEND_H}" fill="{}"/>"##,
            ML + seg as f64 * seg_w,
            seg_w + 0.4,
            hex(ramp_rgb(spec.ramp, value))
        ));
    }
    svg.push_str(&format!(
        r##"<rect x="{ML}" y="{legend_y:.1}" width="{LEGEND_W}" height="{LEGEND_H}" fill="none" stroke="#3a3128"/>"##
    ));
    let legend_step = nice_step(legend_hi - legend_lo, 5);
    let mut tick = (legend_lo / legend_step).ceil() * legend_step;
    while tick <= legend_hi + 1e-9 {
        let x = ML + (tick - legend_lo) / (legend_hi - legend_lo) * LEGEND_W;
        svg.push_str(&format!(
            r##"<text x="{x:.1}" y="{:.1}" fill="#8d8171" font-size="10" text-anchor="middle">{}</text>"##,
            legend_y + LEGEND_H + 13.0,
            tick.round() as i64
        ));
        tick += legend_step;
    }
    svg.push_str(&format!(
        r##"<text x="{:.1}" y="{:.1}" fill="#a99c88" font-size="11">{} ({})</text>"##,
        ML + LEGEND_W + 14.0,
        legend_y + LEGEND_H - 2.0,
        spec.title,
        spec.units
    ));

    // Footer: axis note (honest about the path approximation) + branding.
    svg.push_str(&format!(
        r##"<text x="{ML}" y="{:.1}" fill="#6f6455" font-size="9.5">x: miles along A→B (straight lat/lon path, ≤2-cell grid drift mid-path) | y: pressure (hPa, log) | terrain: model surface pressure | California Wildfire Tracking — Weather Lab</text>"##,
        total_h - 14.0
    ));
    svg.push_str("</svg>");

    // ---- format=json payload (raw sampled arrays for CAFire) ----
    let json_columns: Vec<serde_json::Value> = values
        .iter()
        .map(|col| {
            col.iter()
                .map(|&v| {
                    if v.is_finite() {
                        serde_json::json!((v * 100.0).round() / 100.0)
                    } else {
                        serde_json::Value::Null
                    }
                })
                .collect()
        })
        .collect();
    let json_num = |v: f64, scale: f64| -> serde_json::Value {
        if v.is_finite() {
            serde_json::json!((v * scale).round() / scale)
        } else {
            serde_json::Value::Null
        }
    };
    let data = serde_json::json!({
        "model": model_slug,
        "run": run_slug,
        "hour": hour,
        "valid": format!("{day_label} {hod:02}Z"),
        "field": spec.key,
        "units": spec.units,
        "a": {
            "lat": request.lat0, "lon": request.lon0,
            "place": nearest_place(request.lat0, request.lon0).map(|n| n.describe()),
        },
        "b": {
            "lat": request.lat1, "lon": request.lon1,
            "place": nearest_place(request.lat1, request.lon1).map(|n| n.describe()),
        },
        "path_note": "columns interpolate lat/lon and grid indices linearly between the endpoint cells (<=2-cell drift mid-path on the near-uniform HRRR grid)",
        "levels_hpa": levels_hpa,
        "distance_mi": samples.iter().map(|s| json_num(s.dist_mi, 100.0)).collect::<Vec<_>>(),
        "lat": samples.iter().map(|s| json_num(s.lat, 10000.0)).collect::<Vec<_>>(),
        "lon": samples.iter().map(|s| json_num(s.lon, 10000.0)).collect::<Vec<_>>(),
        "terrain_psfc_hpa": psfc_hpa.iter().map(|&p| json_num(p, 10.0)).collect::<Vec<_>>(),
        "values": json_columns,
    });

    Ok(XsectionOutput { svg, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rustwx_core::{
        CanonicalField, FieldSelector, GridProjection, GridShape, LatLonGrid, SelectedField2D,
    };
    use rw_store::{PressureVolumeInput, write_hour_from_fields};

    // ---- pure helpers ----

    #[test]
    fn path_sampling_honors_endpoints_and_monotone_distance() {
        let samples = sample_path(
            (38.0, -122.5),
            (39.0, -118.5),
            (100.0, 40.0),
            (350.0, 160.0),
            200,
        );
        assert_eq!(samples.len(), 200);
        let first = samples.first().unwrap();
        let last = samples.last().unwrap();
        // Endpoint cells honored exactly.
        assert_eq!((first.fx, first.fy), (100.0, 40.0));
        assert_eq!((last.fx, last.fy), (350.0, 160.0));
        assert_eq!((first.lat, first.lon), (38.0, -122.5));
        assert_eq!((last.lat, last.lon), (39.0, -118.5));
        assert_eq!(first.dist_mi, 0.0);
        // Distances strictly monotone and summing to the endpoint distance.
        for pair in samples.windows(2) {
            assert!(pair[1].dist_mi > pair[0].dist_mi, "distance must increase");
        }
        let direct = haversine_mi(38.0, -122.5, 39.0, -118.5);
        assert!(
            (last.dist_mi - direct).abs() < 0.5,
            "cumulative {} vs direct {direct}",
            last.dist_mi
        );
        // Sanity: the SF→Sierra line is ~225 miles.
        assert!((200.0..260.0).contains(&direct), "got {direct}");
    }

    #[test]
    fn pressure_axis_is_log_p_with_1000_bottom_150_top() {
        let axis = PressureAxis { top: 100.0, height: 400.0, p_bottom: 1000.0, p_top: 150.0 };
        assert!((axis.y(1000.0) - 500.0).abs() < 1e-9, "1000 hPa at the bottom");
        assert!((axis.y(150.0) - 100.0).abs() < 1e-9, "150 hPa at the top");
        // The log-p midpoint is the geometric mean, not the arithmetic one.
        let geometric_mean = (1000.0f64 * 150.0).sqrt();
        assert!((axis.y(geometric_mean) - 300.0).abs() < 1e-6);
        // The arithmetic midpoint (575 hPa) is a higher pressure than the
        // geometric mean, so on log-p it sits in the LOWER half (y > center).
        assert!(axis.y(575.0) > 300.0, "arithmetic midpoint sits below center on log-p");
        // Off-axis pressures clamp instead of escaping the panel.
        assert_eq!(axis.y(1013.0), axis.y(1000.0));
        assert_eq!(axis.y(100.0), axis.y(150.0));
        assert_eq!(axis.y(f64::NAN), axis.y(1000.0), "NaN clamps to the bottom");
    }

    #[test]
    fn terrain_polygon_follows_surface_pressure() {
        let axis = PressureAxis { top: 0.0, height: 400.0, p_bottom: 1000.0, p_top: 150.0 };
        let xs = [10.0, 20.0, 30.0];
        // Sea level (>=1000 hPa clamps to the floor), a 700 hPa peak, a
        // mid slope.
        let points = terrain_polygon(&xs, &[1013.2, 700.0, 850.0], &axis);
        assert_eq!(points.len(), 5, "bottom corners + one vertex per column");
        assert_eq!(points[0], (10.0, 400.0), "closes at the bottom-left");
        assert_eq!(points[4], (30.0, 400.0), "closes at the bottom-right");
        assert_eq!(points[1].1, 400.0, "sea-level column sits on the axis floor");
        assert!(points[2].1 < points[3].1, "the 700 hPa peak rises above the 850 hPa slope");
        assert!(points[3].1 < 400.0);
    }

    #[test]
    fn ramps_interpolate_and_clamp() {
        assert_eq!(hex(ramp_rgb(TEMP_RAMP_F, -200.0)), "#1c103c", "clamps below");
        assert_eq!(hex(ramp_rgb(TEMP_RAMP_F, 200.0)), "#8b3a94", "clamps above");
        assert_eq!(hex(ramp_rgb(RH_RAMP_PCT, 70.0)), "#58c98f", "anchor hits exactly");
        // Between anchors the value moves smoothly (not a step function).
        let a = ramp_rgb(WIND_RAMP_MPH, 22.0);
        let b = ramp_rgb(WIND_RAMP_MPH, 28.0);
        assert_ne!(a, b);
        // Dark fills take cream ink, bright fills take dark ink.
        assert_eq!(ink_on([0x1c, 0x10, 0x3c]), "#f2e7d5");
        assert_eq!(ink_on([0xf5, 0xe0, 0x5c]), "#141414");
    }

    // ---- synthetic-store integration (mirrors rw-ui's synthetic store) ----

    const NX: usize = 90;
    const NY: usize = 70;
    const LEVELS: [u16; 7] = [1000, 925, 850, 700, 500, 300, 250];

    /// Regular lat/lon grid: lat 30..36.9, lon -105..-96.1 (0.1° step).
    fn grid() -> LatLonGrid {
        let mut lat = Vec::with_capacity(NX * NY);
        let mut lon = Vec::with_capacity(NX * NY);
        for y in 0..NY {
            for x in 0..NX {
                lat.push((30.0 + 0.1 * y as f64) as f32);
                lon.push((-105.0 + 0.1 * x as f64) as f32);
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

    /// Surface pressure with a mountain ridge in the middle of the grid
    /// (drops to ~750 hPa) so the terrain silhouette has real shape.
    fn psfc_pa(x: usize, _y: usize) -> f32 {
        let center = (x as f32 - NX as f32 / 2.0).abs() / (NX as f32 / 2.0);
        101_300.0 - 26_000.0 * (1.0 - center).max(0.0)
    }

    fn volume_plane(level_hpa: u16, offset: f32) -> Vec<f32> {
        let base = 288.15 * (f32::from(level_hpa) / 1000.0).powf(0.190_3);
        (0..NY)
            .flat_map(|y| {
                (0..NX)
                    .map(|x| base + 0.02 * x as f32 - 0.015 * y as f32 + offset)
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn volume_input<'a>(name: &'a str, units: &'a str, planes: &'a [Vec<f32>]) -> PressureVolumeInput<'a> {
        PressureVolumeInput {
            name,
            units,
            selector_template: serde_json::json!({ "synthetic": name }),
            levels: LEVELS.iter().zip(planes).map(|(&l, p)| (l, p.as_slice())).collect(),
        }
    }

    fn test_store(tag: &str, with_volumes: bool) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rw-xsection-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let temperature = surface_field(
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            |x, y| 288.0 + 0.05 * x as f32 - 0.04 * y as f32,
        );
        let psfc = surface_field(
            FieldSelector::surface(CanonicalField::Pressure),
            "Pa",
            psfc_pa,
        );
        let temp_planes: Vec<Vec<f32>> =
            LEVELS.iter().map(|&l| volume_plane(l, 0.0)).collect();
        let dew_planes: Vec<Vec<f32>> = LEVELS
            .iter()
            .map(|&l| volume_plane(l, -(3.0 + 0.035 * (1000.0 - f32::from(l)))))
            .collect();
        let u_planes: Vec<Vec<f32>> = LEVELS
            .iter()
            .map(|&l| vec![3.0 + 0.02 * (1000.0 - f32::from(l)); NX * NY])
            .collect();
        let v_planes: Vec<Vec<f32>> = LEVELS.iter().map(|_| vec![1.5; NX * NY]).collect();
        let volumes = if with_volumes {
            vec![
                volume_input("temperature_iso", "K", &temp_planes),
                volume_input("dewpoint_iso", "K", &dew_planes),
                volume_input("u_iso", "m s-1", &u_planes),
                volume_input("v_iso", "m s-1", &v_planes),
            ]
        } else {
            Vec::new()
        };
        write_hour_from_fields(
            &dir,
            "synthetic",
            "20260702_00z",
            6,
            &[("temperature_2m", &temperature), ("surface_pressure", &psfc)],
            &volumes,
            "xsection-test",
            1_780_000_000,
        )
        .expect("hour write");
        dir
    }

    fn request(field: &str) -> XsectionRequest {
        XsectionRequest {
            lat0: 33.0,
            lon0: -104.5,
            lat1: 33.5,
            lon1: -96.6,
            field: field.to_string(),
            hour: Some(6),
            utc_offset_hours: -7.0,
        }
    }

    #[test]
    fn renders_a_section_with_terrain_and_cooling_aloft() {
        let root = test_store("happy", true);
        let out = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &request("temperature"))
            .expect("render succeeds");
        assert_eq!(out.data["hour"], 6);
        assert!(out.svg.starts_with("<svg "));
        assert!(out.svg.contains("VERTICAL CROSS SECTION"));
        assert!(out.svg.contains("<polygon "), "terrain silhouette present");
        assert!(out.svg.contains("California Wildfire Tracking"));

        // JSON: 200 columns, monotone distances, terrain ridge visible,
        // temperature cools with height.
        let cols = out.data["values"].as_array().unwrap();
        assert_eq!(cols.len(), COLUMNS);
        let dist = out.data["distance_mi"].as_array().unwrap();
        assert!(dist.windows(2).all(|w| w[0].as_f64() < w[1].as_f64()));
        let terrain: Vec<f64> = out.data["terrain_psfc_hpa"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_f64().unwrap())
            .collect();
        let mid = terrain[terrain.len() / 2];
        assert!(mid < 800.0, "ridge crest ~750 hPa, got {mid}");
        // The path starts near the ramp foot (grid x≈5 of the synthetic
        // ridge), well below the crest.
        assert!(terrain[0] > 950.0, "path start is near the ridge foot, got {}", terrain[0]);
        assert!(terrain[0] > mid + 100.0, "endpoints sit far below the crest");
        let column = cols[10].as_array().unwrap();
        let sfc = column[0].as_f64().unwrap();
        let top = column[column.len() - 1].as_f64().unwrap();
        assert!(sfc > top, "temperature must cool with height ({sfc} vs {top})");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rh_and_wind_fields_render() {
        let root = test_store("fields", true);
        for field in ["rh", "wind"] {
            let out = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &request(field))
                .unwrap_or_else(|err| panic!("{field}: {err}"));
            let cols = out.data["values"].as_array().unwrap();
            let column = cols[100].as_array().unwrap();
            let value = column[2].as_f64().unwrap();
            match field {
                "rh" => assert!((0.0..=100.0).contains(&value), "RH {value}"),
                _ => assert!(value > 0.0, "wind speed {value}"),
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_volumes_yield_the_honest_message() {
        let root = test_store("novol", false);
        let err = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &request("temperature"))
            .expect_err("must refuse without volumes");
        assert!(
            err.contains("predates vertical-volume storage"),
            "honest message, got: {err}"
        );
        // The hour=None path reports the same cause.
        let mut req = request("temperature");
        req.hour = None;
        let err = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("must refuse without volumes");
        assert!(err.contains("predates vertical-volume storage"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bad_requests_are_named() {
        let root = test_store("bad", true);
        let mut req = request("temperature");
        req.field = "vorticity".to_string();
        let err = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("unknown field");
        assert!(err.contains("field must be one of"), "got: {err}");

        let mut req = request("temperature");
        (req.lat1, req.lon1) = (req.lat0, req.lon0);
        let err = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("degenerate line");
        assert!(err.contains("draw a longer line"), "got: {err}");

        let mut req = request("temperature");
        req.hour = Some(13);
        let err = render_xsection_svg(&root, "synthetic", "20260702_00z", "20260702", 0, &req)
            .expect_err("unknown hour");
        assert!(err.contains("F013 is not stored"), "got: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }
}

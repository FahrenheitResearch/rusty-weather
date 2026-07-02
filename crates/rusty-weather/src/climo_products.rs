#![allow(dead_code)]

//! RTMA-climatology anomaly lane: HRRR forecast day-window fields ranked
//! against the FireWxAtlas ±7-day day-of-year percentile store
//! (`rtma_climo` model, imported by `rw_climo_import`).
//!
//! Formula parity is BY CONSTRUCTION with the atlas's frozen
//! `rtma_surface_formula_manifest_v2026_05_24`: hourly RH/VPD are computed
//! from stored 2 m T/Td with the same Magnus/Bolton saturation vapor
//! pressure, wind is `sqrt(u^2+v^2)`, the surface-HDW proxies are
//! `vpd_kpa * wind` and `vpd_kpa * gust`, and the joint threshold-hours
//! products count hours meeting RH<=15/20 % with gust>=25 mph or
//! wind>=20 mph. Windows are true UTC calendar windows (`utc_00_23` and
//! `utc_12_06_next_day`, the latter assigned to the 12Z start date), then
//! each cell is ranked against the eight stored percentile anchors for
//! that day-of-year. Missing climatology or insufficient window coverage
//! blocks the product with a reason — never a weather-only substitute.

use std::path::Path;
use std::time::Instant;

use rustwx_core::ModelId;
use rustwx_products::places;
use rustwx_products::plot_design::StaticPlotDesign;
use rustwx_products::shared_context::{DomainSpec, model_time_subtitle};
use rustwx_render::{
    ChromeScale, Color, ColorScale, DiscreteColorScale, ExtendMode, Field2D, GridShape,
    LatLonGrid, MapRenderRequest, PngWriteOptions, ProductKey, ProductVisualMode,
    ProjectedDomain, map_frame_aspect_ratio, save_png_profile_with_options,
};
use rw_store::error::RwStoreError;
use serde::Deserialize;

use super::climo_rank::{dryness_rank, no_leap_doy, percentile_rank, valid_civil_date};
use super::{RenderedProduct, StoreFieldSource, StoreRenderConfig, StoreRenderSkip};

/// Store model the importer writes and this lane reads.
pub const CLIMO_MODEL: &str = "rtma_climo";
const CLIMO_RUN_ENV: &str = "RUSTWX_CLIMO_RUN";
const DEFAULT_CLIMO_RUN: &str = "seasonal_v2026_05_24";
const ANCHOR_STATS: [&str; 8] = ["p05", "p10", "p25", "p50", "p75", "p90", "p95", "p99"];
const MPH_TO_MS: f32 = 0.447_04;
/// Minimum hourly samples for a usable day window (of 24) and overnight
/// window (of 19) — mirrors the atlas's n_valid_hours tolerance.
const MIN_DAY_HOURS: usize = 20;
const MIN_OVERNIGHT_HOURS: usize = 16;

/// Magnus/Bolton saturation vapor pressure in hPa (thermo_v1).
fn saturation_vapor_pressure_hpa(t_c: f32) -> f32 {
    6.112 * ((17.67 * t_c) / (t_c + 243.5)).exp()
}

/// RH % from T/Td, clipped to 0..100 (thermo_v1).
fn relative_humidity_pct(t_c: f32, td_c: f32) -> f32 {
    (100.0 * saturation_vapor_pressure_hpa(td_c) / saturation_vapor_pressure_hpa(t_c))
        .clamp(0.0, 100.0)
}

/// VPD kPa from T/Td (thermo_v1).
fn vapor_pressure_deficit_kpa(t_c: f32, td_c: f32) -> f32 {
    ((saturation_vapor_pressure_hpa(t_c) - saturation_vapor_pressure_hpa(td_c)).max(0.0)) * 0.1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClimoWindow {
    /// 00-23Z of the target UTC date.
    UtcDay,
    /// 12Z target date through 06Z the next date, assigned to the start date.
    Utc1206NextDay,
}

impl ClimoWindow {
    fn store_name(self) -> &'static str {
        match self {
            Self::UtcDay => "utc_00_23",
            Self::Utc1206NextDay => "utc_12_06_next_day",
        }
    }

    fn min_hours(self) -> usize {
        match self {
            Self::UtcDay => MIN_DAY_HOURS,
            Self::Utc1206NextDay => MIN_OVERNIGHT_HOURS,
        }
    }

    fn full_hours(self) -> usize {
        match self {
            Self::UtcDay => 24,
            Self::Utc1206NextDay => 19,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClimoProduct {
    VpdDayMax,
    MinRhDay,
    WindDayMax,
    GustDayMax,
    HdwWindDay,
    HdwGustDay,
    HoursRh15Gust25,
    HoursRh20Gust25,
    HoursRh20Wind20,
    OvernightRhRecovery,
    /// Weighted blend of six ingredient severity percentiles
    /// (surface_fire_weather_potential_v1: RH .22, VPD .22, wind .16,
    /// gust .16, HDW-wind .12, HDW-gust .12; needs >=4 ingredients and
    /// >=0.70 weight fraction). Weather-only by contract.
    SurfacePotential,
}

pub const CLIMO_PRODUCTS: &[&str] = &[
    "vpd_day_max_percentile",
    "min_rh_day_percentile",
    "wind_day_max_percentile",
    "gust_day_max_percentile",
    "hdw_wind_day_percentile",
    "hdw_gust_day_percentile",
    "hours_rh15_gust25_percentile",
    "hours_rh20_gust25_percentile",
    "hours_rh20_wind20_percentile",
    "overnight_rh_recovery_percentile",
    "surface_fire_weather_potential",
];

const ALL_PRODUCTS: [ClimoProduct; 11] = [
    ClimoProduct::VpdDayMax,
    ClimoProduct::MinRhDay,
    ClimoProduct::WindDayMax,
    ClimoProduct::GustDayMax,
    ClimoProduct::HdwWindDay,
    ClimoProduct::HdwGustDay,
    ClimoProduct::HoursRh15Gust25,
    ClimoProduct::HoursRh20Gust25,
    ClimoProduct::HoursRh20Wind20,
    ClimoProduct::OvernightRhRecovery,
    ClimoProduct::SurfacePotential,
];

impl ClimoProduct {
    pub fn parse(slug: &str) -> Option<Self> {
        ALL_PRODUCTS
            .iter()
            .copied()
            .find(|product| product.slug() == slug)
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::VpdDayMax => "vpd_day_max_percentile",
            Self::MinRhDay => "min_rh_day_percentile",
            Self::WindDayMax => "wind_day_max_percentile",
            Self::GustDayMax => "gust_day_max_percentile",
            Self::HdwWindDay => "hdw_wind_day_percentile",
            Self::HdwGustDay => "hdw_gust_day_percentile",
            Self::HoursRh15Gust25 => "hours_rh15_gust25_percentile",
            Self::HoursRh20Gust25 => "hours_rh20_gust25_percentile",
            Self::HoursRh20Wind20 => "hours_rh20_wind20_percentile",
            Self::OvernightRhRecovery => "overnight_rh_recovery_percentile",
            Self::SurfacePotential => "surface_fire_weather_potential",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::VpdDayMax => "Day-Max VPD Percentile vs 2019-2026 Climatology",
            Self::MinRhDay => "Day-Min RH Dryness Percentile vs 2019-2026 Climatology",
            Self::WindDayMax => "Day-Max Wind Percentile vs 2019-2026 Climatology",
            Self::GustDayMax => "Day-Max Gust Percentile vs 2019-2026 Climatology",
            Self::HdwWindDay => "Day-Max Surface HDW (Wind) Percentile vs Climatology",
            Self::HdwGustDay => "Day-Max Surface HDW (Gust) Percentile vs Climatology",
            Self::HoursRh15Gust25 => "Hours RH<=15% & Gust>=25mph Percentile vs Climatology",
            Self::HoursRh20Gust25 => "Hours RH<=20% & Gust>=25mph Percentile vs Climatology",
            Self::HoursRh20Wind20 => "Hours RH<=20% & Wind>=20mph Percentile vs Climatology",
            Self::OvernightRhRecovery => "Overnight RH Recovery Percentile (12Z-06Z) vs Climatology",
            Self::SurfacePotential => "Surface Fire Weather Potential (Weather-Only, 0-100)",
        }
    }

    fn window(self) -> ClimoWindow {
        match self {
            Self::OvernightRhRecovery => ClimoWindow::Utc1206NextDay,
            _ => ClimoWindow::UtcDay,
        }
    }

    /// Climatology store product name inside the window group.
    fn climo_product(self) -> &'static str {
        match self {
            Self::VpdDayMax => "max_vpd_2m_kpa",
            Self::MinRhDay | Self::OvernightRhRecovery => "min_rh_2m_pct",
            Self::WindDayMax => "max_wind_10m_ms",
            Self::GustDayMax => "max_gust_10m_ms",
            Self::HdwWindDay => "max_surface_hdw_wind",
            Self::HdwGustDay => "max_surface_hdw_gust",
            Self::HoursRh15Gust25 => "hours_joint_rh15_gust25",
            Self::HoursRh20Gust25 => "hours_joint_rh20_gust25",
            Self::HoursRh20Wind20 => "hours_joint_rh20_wind20",
            Self::SurfacePotential => "max_vpd_2m_kpa",
        }
    }

    /// Low tail is the dangerous tail (minimum RH products).
    fn dryness(self) -> bool {
        matches!(self, Self::MinRhDay | Self::OvernightRhRecovery)
    }

    /// Threshold-hours products: zero hours where zero is also the
    /// climatological norm renders as no-data, not a mid-rank fill.
    fn hours_product(self) -> bool {
        matches!(
            self,
            Self::HoursRh15Gust25 | Self::HoursRh20Gust25 | Self::HoursRh20Wind20
        )
    }

    fn scale(self) -> DiscreteColorScale {
        // Few, operationally-meaningful bins: quiet blues below median, pale
        // neutrals through p75, then thick hot bins where IMETs look.
        DiscreteColorScale {
            levels: vec![0.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0, 100.5],
            colors: hex_colors(&[
                "#5b93c3", "#c8d9e4", "#f2ede0", "#fdd08a", "#f8823c", "#dc1f1f", "#7a0177",
            ]),
            extend: ExtendMode::Neither,
            mask_below: None,
        }
    }
}

/// Importer sidecar recording where the climatology subgrid sits in the
/// HRRR grid — the lane's alignment contract.
#[derive(Debug, Deserialize)]
struct ClimoGridMeta {
    schema: String,
    hrrr_grid_hash: String,
    hrrr_row0: usize,
    hrrr_col0: usize,
    ny: usize,
    nx: usize,
    doy_start: u16,
    doy_end: u16,
}

pub struct ClimoRenderOutcome {
    pub rendered: Vec<RenderedProduct>,
    pub skipped: Vec<StoreRenderSkip>,
    pub anchor_hour: u16,
    pub doy: u16,
}

fn climo_run_slug() -> String {
    std::env::var(CLIMO_RUN_ENV)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_CLIMO_RUN.to_string())
}

/// Everything one UTC-day fold produces, on the climatology subgrid.
struct DayFold {
    min_rh: Vec<f32>,
    max_vpd: Vec<f32>,
    max_wind: Vec<f32>,
    max_gust: Vec<f32>,
    max_hdw_wind: Vec<f32>,
    max_hdw_gust: Vec<f32>,
    hours_rh15_gust25: Vec<f32>,
    hours_rh20_gust25: Vec<f32>,
    hours_rh20_wind20: Vec<f32>,
    hours_used: Vec<u16>,
}

impl DayFold {
    fn values_for(&self, product: ClimoProduct) -> &[f32] {
        match product {
            ClimoProduct::VpdDayMax => &self.max_vpd,
            ClimoProduct::MinRhDay | ClimoProduct::OvernightRhRecovery => &self.min_rh,
            ClimoProduct::WindDayMax => &self.max_wind,
            ClimoProduct::GustDayMax => &self.max_gust,
            ClimoProduct::HdwWindDay => &self.max_hdw_wind,
            ClimoProduct::HdwGustDay => &self.max_hdw_gust,
            ClimoProduct::HoursRh15Gust25 => &self.hours_rh15_gust25,
            ClimoProduct::HoursRh20Gust25 => &self.hours_rh20_gust25,
            ClimoProduct::HoursRh20Wind20 => &self.hours_rh20_wind20,
            ClimoProduct::SurfacePotential => &self.max_vpd,
        }
    }
}

/// Temperatures stored in Kelvin convert to Celsius; Celsius passes through.
fn to_celsius(units: &str, values: &mut [f32]) {
    let is_kelvin = units.trim().eq_ignore_ascii_case("k")
        || units.to_ascii_lowercase().contains("kelvin");
    if is_kelvin {
        for value in values.iter_mut() {
            *value -= 273.15;
        }
    }
}

/// The forecast hours of `stored_hours` whose valid times fall inside the
/// window anchored at UTC date `(year, month, day)`.
fn window_hours(
    stored_hours: &[u16],
    date_yyyymmdd: &str,
    cycle_utc: u8,
    target: (i32, u32, u32),
    window: ClimoWindow,
) -> Vec<u16> {
    let mut hours = Vec::new();
    for &hour in stored_hours {
        let Ok(valid_date) = valid_civil_date(date_yyyymmdd, cycle_utc, hour) else {
            continue;
        };
        let hod = (u32::from(cycle_utc) + u32::from(hour)) % 24;
        let is_member = match window {
            ClimoWindow::UtcDay => valid_date == target,
            ClimoWindow::Utc1206NextDay => {
                let next = next_civil_date(target);
                (valid_date == target && hod >= 12) || (valid_date == next && hod <= 6)
            }
        };
        if is_member {
            hours.push(hour);
        }
    }
    hours.sort_unstable();
    hours
}

fn next_civil_date(date: (i32, u32, u32)) -> (i32, u32, u32) {
    // Reuse the run-relative hour arithmetic: date + 24 hours.
    let stamp = format!("{:04}{:02}{:02}", date.0, date.1, date.2);
    valid_civil_date(&stamp, 0, 24).unwrap_or(date)
}

/// Fold one window's hourly fields (cropped to the subgrid) into the
/// atlas-formula day products.
#[allow(clippy::too_many_arguments)]
fn fold_window(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    hours: &[u16],
    meta: &ClimoGridMeta,
    hrrr_nx: usize,
) -> Result<DayFold, String> {
    let plane = meta.ny * meta.nx;
    let gust25 = 25.0 * MPH_TO_MS;
    let wind20 = 20.0 * MPH_TO_MS;
    let mut fold = DayFold {
        min_rh: vec![f32::NAN; plane],
        max_vpd: vec![f32::NAN; plane],
        max_wind: vec![f32::NAN; plane],
        max_gust: vec![f32::NAN; plane],
        max_hdw_wind: vec![f32::NAN; plane],
        max_hdw_gust: vec![f32::NAN; plane],
        hours_rh15_gust25: vec![0.0; plane],
        hours_rh20_gust25: vec![0.0; plane],
        hours_rh20_wind20: vec![0.0; plane],
        hours_used: Vec::new(),
    };
    let crop = |field: &rustwx_core::SelectedField2D| -> Vec<f32> {
        let mut out = Vec::with_capacity(plane);
        for row in 0..meta.ny {
            let base = (meta.hrrr_row0 + row) * hrrr_nx + meta.hrrr_col0;
            out.extend_from_slice(&field.values[base..base + meta.nx]);
        }
        out
    };
    for &hour in hours {
        let source = StoreFieldSource::open(store_root, model_slug, run_slug, hour)
            .map_err(|err| format!("open hour {hour}: {err}"))?;
        let mut fetch = |name: &str| -> Result<(Vec<f32>, String), String> {
            let field = source
                .fetch_variable(name)
                .map_err(|err| format!("hour {hour} variable {name}: {err}"))?;
            Ok((crop(&field), field.units))
        };
        let (mut t, t_units) = fetch("temperature_2m")?;
        let (mut td, td_units) = fetch("dewpoint_2m")?;
        let (u, _) = fetch("u_10m")?;
        let (v, _) = fetch("v_10m")?;
        let (gust, _) = fetch("wind_gust_10m")?;
        to_celsius(&t_units, &mut t);
        to_celsius(&td_units, &mut td);
        for cell in 0..plane {
            let (t_c, td_c) = (t[cell], td[cell]);
            if !t_c.is_finite() || !td_c.is_finite() {
                continue;
            }
            let rh = relative_humidity_pct(t_c, td_c);
            let vpd = vapor_pressure_deficit_kpa(t_c, td_c);
            let wind = (u[cell] * u[cell] + v[cell] * v[cell]).sqrt();
            let gust_ms = gust[cell];
            let update_max = |slot: &mut f32, value: f32| {
                if value.is_finite() && (!slot.is_finite() || value > *slot) {
                    *slot = value;
                }
            };
            let update_min = |slot: &mut f32, value: f32| {
                if value.is_finite() && (!slot.is_finite() || value < *slot) {
                    *slot = value;
                }
            };
            update_min(&mut fold.min_rh[cell], rh);
            update_max(&mut fold.max_vpd[cell], vpd);
            update_max(&mut fold.max_wind[cell], wind);
            update_max(&mut fold.max_gust[cell], gust_ms);
            if wind.is_finite() {
                update_max(&mut fold.max_hdw_wind[cell], vpd * wind);
            }
            if gust_ms.is_finite() {
                update_max(&mut fold.max_hdw_gust[cell], vpd * gust_ms);
            }
            if gust_ms.is_finite() && gust_ms >= gust25 {
                if rh <= 15.0 {
                    fold.hours_rh15_gust25[cell] += 1.0;
                }
                if rh <= 20.0 {
                    fold.hours_rh20_gust25[cell] += 1.0;
                }
            }
            if wind.is_finite() && wind >= wind20 && rh <= 20.0 {
                fold.hours_rh20_wind20[cell] += 1.0;
            }
        }
        fold.hours_used.push(hour);
    }
    Ok(fold)
}

/// Render the requested climatology-anomaly products for one run.
pub fn render_climo_products_from_store(
    config: &StoreRenderConfig,
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    requested: &[String],
) -> Result<ClimoRenderOutcome, Box<dyn std::error::Error>> {
    let mut skipped = Vec::new();
    let mut products = Vec::new();
    for slug in requested {
        match ClimoProduct::parse(slug) {
            Some(product) => products.push(product),
            None => skipped.push(StoreRenderSkip {
                slug: slug.clone(),
                reason: "unknown climo product".to_string(),
            }),
        }
    }

    let stored_hours =
        super::windowed_store::stored_run_hours(store_root, model_slug, run_slug)?;
    let anchor_hour = stored_hours.iter().copied().max().unwrap_or(0);

    // Target day: the latest UTC date with a usable 00-23Z window.
    let mut target: Option<(i32, u32, u32)> = None;
    let mut candidates: Vec<(i32, u32, u32)> = stored_hours
        .iter()
        .filter_map(|&hour| valid_civil_date(&config.date_yyyymmdd, config.cycle_utc, hour).ok())
        .collect();
    candidates.sort_unstable();
    candidates.dedup();
    for &candidate in candidates.iter().rev() {
        let hours = window_hours(
            &stored_hours,
            &config.date_yyyymmdd,
            config.cycle_utc,
            candidate,
            ClimoWindow::UtcDay,
        );
        if hours.len() >= MIN_DAY_HOURS {
            target = Some(candidate);
            break;
        }
    }
    let Some(target) = target else {
        return Ok(ClimoRenderOutcome {
            rendered: Vec::new(),
            skipped: products
                .iter()
                .map(|product| StoreRenderSkip {
                    slug: product.slug().to_string(),
                    reason: format!(
                        "no UTC day has >= {MIN_DAY_HOURS} stored hours (run has {} hours)",
                        stored_hours.len()
                    ),
                })
                .chain(skipped)
                .collect(),
            anchor_hour,
            doy: 0,
        });
    };
    let doy = no_leap_doy(target.0, target.1, target.2);

    let climo_run = climo_run_slug();
    let climo_dir = store_root.join(CLIMO_MODEL).join(&climo_run);
    let block_all = |products: &[ClimoProduct],
                     skipped: Vec<StoreRenderSkip>,
                     reason: String| ClimoRenderOutcome {
        rendered: Vec::new(),
        skipped: products
            .iter()
            .map(|product| StoreRenderSkip {
                slug: product.slug().to_string(),
                reason: reason.clone(),
            })
            .chain(skipped)
            .collect(),
        anchor_hour,
        doy,
    };
    if products.is_empty() {
        return Ok(ClimoRenderOutcome {
            rendered: Vec::new(),
            skipped,
            anchor_hour,
            doy,
        });
    }
    let meta_text = match std::fs::read_to_string(climo_dir.join("climo_grid_meta.json")) {
        Ok(text) => text,
        Err(err) => {
            return Ok(block_all(
                &products,
                skipped,
                format!("climatology store not available at {}: {err}", climo_dir.display()),
            ));
        }
    };
    let meta: ClimoGridMeta = serde_json::from_str(&meta_text)
        .map_err(|err| format!("climo_grid_meta.json: {err}"))?;
    if meta.schema != "cafire.rtma_climo_grid_meta.v1" {
        return Ok(block_all(
            &products,
            skipped,
            format!("unsupported climo grid meta schema {}", meta.schema),
        ));
    }
    let hrrr_grid = rw_store::grid::GridFile::open(
        &store_root.join(model_slug).join(run_slug).join("grid.rwg"),
    )?;
    if hrrr_grid.hash != meta.hrrr_grid_hash {
        return Ok(block_all(
            &products,
            skipped,
            "climatology was imported against a different HRRR grid".to_string(),
        ));
    }
    if doy < meta.doy_start || doy > meta.doy_end {
        return Ok(block_all(
            &products,
            skipped,
            format!(
                "DOY {doy} outside imported climatology range {}..{}",
                meta.doy_start, meta.doy_end
            ),
        ));
    }
    let climo = match StoreFieldSource::open(store_root, CLIMO_MODEL, &climo_run, doy) {
        Ok(source) => source,
        Err(err) => {
            return Ok(block_all(&products, skipped, format!("open climo DOY {doy}: {err}")));
        }
    };

    // Fold each needed window once.
    let mut day_fold: Option<DayFold> = None;
    let mut night_fold: Option<DayFold> = None;
    for window in [ClimoWindow::UtcDay, ClimoWindow::Utc1206NextDay] {
        if !products.iter().any(|product| product.window() == window) {
            continue;
        }
        let hours = window_hours(
            &stored_hours,
            &config.date_yyyymmdd,
            config.cycle_utc,
            target,
            window,
        );
        if hours.len() < window.min_hours() {
            let reason = format!(
                "{} window has {}/{} stored hours",
                window.store_name(),
                hours.len(),
                window.full_hours()
            );
            let blocked: Vec<ClimoProduct> = products
                .iter()
                .copied()
                .filter(|product| product.window() == window)
                .collect();
            for product in &blocked {
                skipped.push(StoreRenderSkip {
                    slug: product.slug().to_string(),
                    reason: reason.clone(),
                });
            }
            products.retain(|product| product.window() != window);
            continue;
        }
        let fold = fold_window(store_root, model_slug, run_slug, &hours, &meta, hrrr_grid.nx)?;
        match window {
            ClimoWindow::UtcDay => day_fold = Some(fold),
            ClimoWindow::Utc1206NextDay => night_fold = Some(fold),
        }
    }

    // Shared render context on the climatology subgrid.
    let subgrid = climo.full_grid();
    let (ny, nx) = (subgrid.shape.ny, subgrid.shape.nx);
    if (ny, nx) != (meta.ny, meta.nx) {
        return Err(format!(
            "climo grid is {ny}x{nx} but meta records {}x{}",
            meta.ny, meta.nx
        )
        .into());
    }
    let projected = rustwx_products::direct::build_projected_map_with_projection(
        &subgrid.lat_deg,
        &subgrid.lon_deg,
        climo.projection(),
        config.domain.bounds,
        map_frame_aspect_ratio(config.output_width, config.output_height, true, true),
    )?;

    let mut rendered = Vec::new();
    for product in products {
        let started = Instant::now();
        let fold = match product.window() {
            ClimoWindow::UtcDay => day_fold.as_ref(),
            ClimoWindow::Utc1206NextDay => night_fold.as_ref(),
        };
        let Some(fold) = fold else { continue };

        let mut anchors = Vec::with_capacity(ANCHOR_STATS.len());
        let mut anchor_error = None;
        for stat in ANCHOR_STATS {
            let name = format!(
                "climo__{}__{}__{stat}",
                product.window().store_name(),
                product.climo_product()
            );
            match climo.derived_grid(&name) {
                Ok(stored) => anchors.push(stored.values),
                Err(RwStoreError::UnknownVariable(_)) => {
                    anchor_error = Some(format!("climatology grid '{name}' not stored"));
                    break;
                }
                Err(err) => return Err(format!("read {name}: {err}").into()),
            }
        }
        if let Some(reason) = anchor_error {
            skipped.push(StoreRenderSkip {
                slug: product.slug().to_string(),
                reason,
            });
            continue;
        }
        let sample_n = climo
            .derived_grid(&format!(
                "climo__{}__{}__sample_count",
                product.window().store_name(),
                product.climo_product()
            ))
            .ok()
            .map(|stored| median_finite(&stored.values))
            .unwrap_or(f32::NAN);

        if product == ClimoProduct::SurfacePotential {
            match compute_surface_potential(&climo, fold, ny * nx) {
                Ok(ranks) => {
                    let output_path = render_rank_map(
                        config, &subgrid, &projected, climo.projection().cloned(),
                        product, ranks, anchor_hour, fold, doy, f32::NAN,
                    )?;
                    rendered.push(RenderedProduct {
                        slug: product.slug().to_string(),
                        total_ms: started.elapsed().as_millis(),
                        output_path,
                    });
                }
                Err(reason) => skipped.push(StoreRenderSkip {
                    slug: product.slug().to_string(),
                    reason,
                }),
            }
            continue;
        }
        let source_values = fold.values_for(product);
        let mut ranks = vec![f32::NAN; ny * nx];
        for cell in 0..ny * nx {
            let value = source_values[cell];
            if !value.is_finite() {
                continue;
            }
            // Joint-hours: no critical hours forecast where none are
            // climatologically expected (p75 == 0) is "no signal", not a
            // mid-percentile fill — leave the basemap visible.
            if product.hours_product() && value <= 0.0 && anchors[4][cell] <= 0.0 {
                continue;
            }
            let anchor_values = [
                anchors[0][cell],
                anchors[1][cell],
                anchors[2][cell],
                anchors[3][cell],
                anchors[4][cell],
                anchors[5][cell],
                anchors[6][cell],
                anchors[7][cell],
            ];
            ranks[cell] = if product.dryness() {
                dryness_rank(value, &anchor_values)
            } else {
                percentile_rank(value, &anchor_values)
            };
        }

        let output_path = render_rank_map(
            config,
            &subgrid,
            &projected,
            climo.projection().cloned(),
            product,
            ranks,
            anchor_hour,
            fold,
            doy,
            sample_n,
        )?;
        rendered.push(RenderedProduct {
            slug: product.slug().to_string(),
            total_ms: started.elapsed().as_millis(),
            output_path,
        });
    }

    Ok(ClimoRenderOutcome {
        rendered,
        skipped,
        anchor_hour,
        doy,
    })
}

#[allow(clippy::too_many_arguments)]
fn render_rank_map(
    config: &StoreRenderConfig,
    subgrid: &rustwx_core::LatLonGrid,
    projected: &rustwx_render::ProjectedMap,
    projection: Option<rustwx_core::GridProjection>,
    product: ClimoProduct,
    ranks: Vec<f32>,
    anchor_hour: u16,
    fold: &DayFold,
    doy: u16,
    sample_n: f32,
) -> Result<std::path::PathBuf, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&config.out_dir)?;
    let render_grid = LatLonGrid::new(
        GridShape::new(subgrid.shape.nx, subgrid.shape.ny)?,
        subgrid.lat_deg.clone(),
        subgrid.lon_deg.clone(),
    )?;
    let field = Field2D::new(
        ProductKey::named(product.slug()),
        "percentile".to_string(),
        render_grid,
        ranks,
    )?;
    let mut request = MapRenderRequest::new(field, ColorScale::Discrete(product.scale()));
    request.title = Some(product.title().to_string());
    request.subtitle_left = Some(model_time_subtitle(
        config.model,
        &config.date_yyyymmdd,
        config.cycle_utc,
        anchor_hour,
    ));
    let n_label = if sample_n.is_finite() {
        format!(" | n~{}", sample_n.round() as i64)
    } else {
        String::new()
    };
    request.subtitle_right = Some(format!(
        "+/-7d climo 19-26 | DOY {doy}{n_label} | {}h {}",
        fold.hours_used.len(),
        product.window().store_name().replace("utc_", "").replace("_next_day", "z+"),
    ));
    request.cbar_tick_step = None;
    request.width = config.output_width;
    request.height = config.output_height;
    request.chrome_scale = static_chrome_scale();
    request.supersample_factor = static_supersample_factor();
    request.supersample_sharpen = static_supersample_sharpen();
    StaticPlotDesign::new(config.domain.bounds, ProductVisualMode::SevereDiagnostic)
        .apply_to_request(&mut request);
    request.projected_domain = Some(ProjectedDomain {
        x: projected.projected_x.clone(),
        y: projected.projected_y.clone(),
        extent: projected.extent.clone(),
    });
    request.projected_lines = projected.lines.clone();
    request.projected_polygons = projected.polygons.clone();
    request.inverse_raster_projection = projected.inverse_raster_projection.clone();
    if let Some(overlay) = config.place_label_overlay.as_ref() {
        places::apply_place_label_overlay(
            &mut request,
            overlay,
            &config.domain,
            &subgrid.lat_deg,
            &subgrid.lon_deg,
            projection.as_ref(),
        )?;
    }
    let output_path = config.out_dir.join(format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_{}.png",
        config.model.as_str().replace('-', "_"),
        config.date_yyyymmdd,
        config.cycle_utc,
        anchor_hour,
        config.domain.slug,
        product.slug(),
    ));
    save_png_profile_with_options(
        &request,
        &output_path,
        &PngWriteOptions {
            compression: config.png_compression,
        },
    )?;
    Ok(output_path)
}

/// surface_fire_weather_potential_v1: weighted severity-percentile blend.
fn compute_surface_potential(
    climo: &StoreFieldSource,
    fold: &DayFold,
    plane: usize,
) -> Result<Vec<f32>, String> {
    // (fold values, climo product, weight, dryness orientation)
    let ingredients: [(&[f32], &str, f64, bool); 6] = [
        (&fold.min_rh, "min_rh_2m_pct", 0.22, true),
        (&fold.max_vpd, "max_vpd_2m_kpa", 0.22, false),
        (&fold.max_wind, "max_wind_10m_ms", 0.16, false),
        (&fold.max_gust, "max_gust_10m_ms", 0.16, false),
        (&fold.max_hdw_wind, "max_surface_hdw_wind", 0.12, false),
        (&fold.max_hdw_gust, "max_surface_hdw_gust", 0.12, false),
    ];
    let mut anchor_sets = Vec::with_capacity(6);
    for (_, name, _, _) in &ingredients {
        let mut set = Vec::with_capacity(ANCHOR_STATS.len());
        for stat in ANCHOR_STATS {
            let full = format!("climo__utc_00_23__{name}__{stat}");
            match climo.derived_grid(&full) {
                Ok(stored) => set.push(stored.values),
                Err(err) => return Err(format!("potential ingredient {full}: {err}")),
            }
        }
        anchor_sets.push(set);
    }
    let mut out = vec![f32::NAN; plane];
    for cell in 0..plane {
        let mut weighted = 0.0f64;
        let mut weight_sum = 0.0f64;
        let mut count = 0usize;
        for (index, (values, _, weight, dryness)) in ingredients.iter().enumerate() {
            let value = values[cell];
            if !value.is_finite() {
                continue;
            }
            let set = &anchor_sets[index];
            let anchors = [
                set[0][cell], set[1][cell], set[2][cell], set[3][cell],
                set[4][cell], set[5][cell], set[6][cell], set[7][cell],
            ];
            let rank = if *dryness {
                dryness_rank(value, &anchors)
            } else {
                percentile_rank(value, &anchors)
            };
            if rank.is_finite() {
                weighted += f64::from(rank) * weight;
                weight_sum += weight;
                count += 1;
            }
        }
        if count >= 4 && weight_sum >= 0.70 {
            out[cell] = (weighted / weight_sum) as f32;
        }
    }
    Ok(out)
}

fn median_finite(values: &[f32]) -> f32 {
    let mut finite: Vec<f32> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.is_empty() {
        return f32::NAN;
    }
    finite.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    finite[finite.len() / 2]
}

fn hex_colors(values: &[&str]) -> Vec<Color> {
    values
        .iter()
        .map(|hex| {
            let hex = hex.trim_start_matches('#');
            let r = u8::from_str_radix(&hex[0..2], 16).unwrap_or(0);
            let g = u8::from_str_radix(&hex[2..4], 16).unwrap_or(0);
            let b = u8::from_str_radix(&hex[4..6], 16).unwrap_or(0);
            Color::rgba(r, g, b, 255)
        })
        .collect()
}

fn static_supersample_factor() -> u32 {
    std::env::var("RUSTWX_SUPERSAMPLE_FACTOR")
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|&value| value > 0)
        .unwrap_or(1)
}

fn static_supersample_sharpen() -> bool {
    std::env::var("RUSTWX_SUPERSAMPLE_SHARPEN")
        .ok()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        })
        .unwrap_or(false)
}

fn static_chrome_scale() -> ChromeScale {
    let scale = std::env::var("RUSTWX_CHROME_SCALE")
        .ok()
        .and_then(|value| value.parse::<f32>().ok())
        .unwrap_or(0.9)
        .clamp(0.75, 2.0);
    ChromeScale::Fixed(scale)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thermo_matches_the_frozen_atlas_formulas() {
        // Magnus/Bolton spot values, hand-computed.
        assert!((saturation_vapor_pressure_hpa(20.0) - 23.37).abs() < 0.05);
        assert!((saturation_vapor_pressure_hpa(0.0) - 6.112).abs() < 1e-3);
        // T=30C, Td=10C: e_s=42.4x hPa, e_d=12.27 hPa -> VPD ~3.02 kPa.
        let vpd = vapor_pressure_deficit_kpa(30.0, 10.0);
        assert!((vpd - 3.02).abs() < 0.02, "vpd {vpd}");
        let rh = relative_humidity_pct(30.0, 10.0);
        assert!((rh - 28.9).abs() < 0.5, "rh {rh}");
        // Saturated inputs sit at ~100 (f32 rounding); supersaturated clip exactly.
        assert!(relative_humidity_pct(10.0, 10.0) > 99.99);
        assert_eq!(relative_humidity_pct(10.0, 12.0), 100.0);
        assert_eq!(vapor_pressure_deficit_kpa(10.0, 12.0), 0.0);
    }

    #[test]
    fn slugs_round_trip_for_all_ten_products() {
        assert_eq!(CLIMO_PRODUCTS.len(), 11);
        for slug in CLIMO_PRODUCTS {
            let product = ClimoProduct::parse(slug).expect(slug);
            assert_eq!(product.slug(), *slug);
        }
        assert!(ClimoProduct::parse("not_a_product").is_none());
    }

    #[test]
    fn day_window_selects_the_utc_calendar_day() {
        let stored: Vec<u16> = (0..=30).collect();
        // 00z run on 2026-07-01: the Jul 1 day window is F00..F23.
        let hours = window_hours(&stored, "20260701", 0, (2026, 7, 1), ClimoWindow::UtcDay);
        assert_eq!(hours, (0..=23).collect::<Vec<u16>>());
        // 12z run: Jul 1 window is F00..F11 (12Z-23Z of Jul 1).
        let hours = window_hours(&stored, "20260701", 12, (2026, 7, 1), ClimoWindow::UtcDay);
        assert_eq!(hours, (0..=11).collect::<Vec<u16>>());
    }

    #[test]
    fn overnight_window_spans_12z_to_06z_next_day() {
        let stored: Vec<u16> = (0..=30).collect();
        let hours = window_hours(
            &stored,
            "20260701",
            0,
            (2026, 7, 1),
            ClimoWindow::Utc1206NextDay,
        );
        assert_eq!(hours, (12..=30).collect::<Vec<u16>>(), "F12..F30 = 12Z Jul1 .. 06Z Jul2");
        // Month rollover: overnight window anchored on Jul 31.
        let hours = window_hours(
            &(0..=48).collect::<Vec<u16>>(),
            "20260731",
            0,
            (2026, 7, 31),
            ClimoWindow::Utc1206NextDay,
        );
        assert_eq!(hours.first(), Some(&12));
        assert_eq!(hours.last(), Some(&30));
    }

    #[test]
    fn kelvin_fields_convert_to_celsius() {
        let mut values = vec![300.15, f32::NAN];
        to_celsius("K", &mut values);
        assert!((values[0] - 27.0).abs() < 1e-3);
        let mut celsius = vec![27.0];
        to_celsius("degC", &mut celsius);
        assert_eq!(celsius[0], 27.0);
    }

    #[test]
    fn joint_thresholds_use_mph_converted_to_ms() {
        assert!((25.0 * MPH_TO_MS - 11.176).abs() < 1e-3);
        assert!((20.0 * MPH_TO_MS - 8.9408).abs() < 1e-3);
    }
}

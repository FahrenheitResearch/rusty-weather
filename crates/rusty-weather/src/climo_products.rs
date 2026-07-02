#![allow(dead_code)]

//! RTMA-climatology anomaly lane: HRRR forecast day-window fields ranked
//! against the FireWxAtlas ±7-day day-of-year percentile store
//! (`rtma_climo` model, imported by `rw_climo_import`).
//!
//! Each product folds the run's stored hours into a day window (via the
//! existing windowed lane), crops onto the climatology subgrid using the
//! importer's recorded HRRR offsets, ranks every cell against the eight
//! stored percentile anchors, and renders a [5..99] percentile-rank map.
//! Missing climatology (wrong grid hash, absent DOY, absent store) blocks
//! the product with a reason — never a silent weather-only substitute.

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

use super::climo_rank::{
    ANCHOR_LEVELS, dryness_rank, no_leap_doy, percentile_rank, valid_civil_date,
};
use super::windowed_store;
use super::{RenderedProduct, StoreFieldSource, StoreRenderConfig, StoreRenderSkip};

/// Store model the importer writes and this lane reads.
pub const CLIMO_MODEL: &str = "rtma_climo";
const CLIMO_RUN_ENV: &str = "RUSTWX_CLIMO_RUN";
const DEFAULT_CLIMO_RUN: &str = "seasonal_v2026_05_24";
const ANCHOR_STATS: [&str; 8] = ["p05", "p10", "p25", "p50", "p75", "p90", "p95", "p99"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClimoProduct {
    /// Day-max 2 m VPD vs `utc_00_23/max_vpd_2m_kpa`.
    VpdDayMaxPercentile,
    /// Day-min 2 m RH vs `utc_00_23/min_rh_2m_pct` (dryness rank: 99 = driest).
    MinRhDayPercentile,
    /// Day-max 10 m wind vs `utc_00_23/max_wind_10m_ms`.
    WindDayMaxPercentile,
}

pub const CLIMO_PRODUCTS: &[&str] = &[
    "vpd_day_max_percentile",
    "min_rh_day_percentile",
    "wind_day_max_percentile",
];

impl ClimoProduct {
    pub fn parse(slug: &str) -> Option<Self> {
        match slug {
            "vpd_day_max_percentile" => Some(Self::VpdDayMaxPercentile),
            "min_rh_day_percentile" => Some(Self::MinRhDayPercentile),
            "wind_day_max_percentile" => Some(Self::WindDayMaxPercentile),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::VpdDayMaxPercentile => "vpd_day_max_percentile",
            Self::MinRhDayPercentile => "min_rh_day_percentile",
            Self::WindDayMaxPercentile => "wind_day_max_percentile",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::VpdDayMaxPercentile => "Day-Max VPD Percentile vs 2019-2026 Climatology",
            Self::MinRhDayPercentile => "Day-Min RH Dryness Percentile vs 2019-2026 Climatology",
            Self::WindDayMaxPercentile => "Day-Max Wind Percentile vs 2019-2026 Climatology",
        }
    }

    /// Windowed HRRR source slug feeding the rank.
    fn windowed_slug(self) -> &'static str {
        match self {
            Self::VpdDayMaxPercentile => "2m_vpd_0_24h_max",
            Self::MinRhDayPercentile => "2m_rh_0_24h_min",
            Self::WindDayMaxPercentile => "10m_wind_0_24h_max",
        }
    }

    /// Climatology store product name (window fixed at utc_00_23 for v1).
    fn climo_product(self) -> &'static str {
        match self {
            Self::VpdDayMaxPercentile => "max_vpd_2m_kpa",
            Self::MinRhDayPercentile => "min_rh_2m_pct",
            Self::WindDayMaxPercentile => "max_wind_10m_ms",
        }
    }

    /// Low tail is the dangerous tail (minimum RH).
    fn dryness(self) -> bool {
        matches!(self, Self::MinRhDayPercentile)
    }

    /// Convert the windowed lane's display units to climatology units.
    fn normalize(self, units: &str, value: f64) -> Result<f64, String> {
        match (self, units) {
            (Self::VpdDayMaxPercentile, "kPa") => Ok(value),
            (Self::VpdDayMaxPercentile, "hPa") => Ok(value / 10.0),
            (Self::MinRhDayPercentile, "%") => Ok(value),
            (Self::WindDayMaxPercentile, "m/s") => Ok(value),
            (Self::WindDayMaxPercentile, "kt" | "kts" | "knots") => Ok(value * 0.514_444),
            (Self::WindDayMaxPercentile, "mph") => Ok(value * 0.447_04),
            _ => Err(format!(
                "windowed source units '{units}' are not convertible for {}",
                self.slug()
            )),
        }
    }

    fn scale(self) -> DiscreteColorScale {
        DiscreteColorScale {
            levels: vec![0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 90.0, 95.0, 99.0, 100.5],
            colors: hex_colors(&[
                "#2c7bb6", "#74add1", "#c6dbef", "#edf2f7", "#fdf6c3", "#fdcc8a", "#fc8d59",
                "#e31a1c", "#7a0177",
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

/// Render the requested climatology-anomaly products for one run. `store`
/// is the already-open HRRR hour (grid + projection provenance only).
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

    // Window fold across the run's stored hours (anchored at the max hour).
    let stored_hours = windowed_store::stored_run_hours(store_root, model_slug, run_slug)?;
    let anchor_hour = stored_hours.iter().copied().max().unwrap_or(0);
    let (year, month, day) =
        valid_civil_date(&config.date_yyyymmdd, config.cycle_utc, anchor_hour)
            .map_err(|err| format!("valid date: {err}"))?;
    let doy = no_leap_doy(year, month, day);

    let climo_run = climo_run_slug();
    let climo_dir = store_root.join(CLIMO_MODEL).join(&climo_run);
    let block_all = |products: &[ClimoProduct], reason: String| ClimoRenderOutcome {
        rendered: Vec::new(),
        skipped: products
            .iter()
            .map(|product| StoreRenderSkip {
                slug: product.slug().to_string(),
                reason: reason.clone(),
            })
            .chain(skipped.iter().cloned())
            .collect(),
        anchor_hour,
        doy,
    };
    if products.is_empty() {
        return Ok(block_all(&[], String::new()));
    }
    let meta_text = match std::fs::read_to_string(climo_dir.join("climo_grid_meta.json")) {
        Ok(text) => text,
        Err(err) => {
            return Ok(block_all(
                &products,
                format!(
                    "climatology store not available at {}: {err}",
                    climo_dir.display()
                ),
            ));
        }
    };
    let meta: ClimoGridMeta = serde_json::from_str(&meta_text)
        .map_err(|err| format!("climo_grid_meta.json: {err}"))?;
    if meta.schema != "cafire.rtma_climo_grid_meta.v1" {
        return Ok(block_all(
            &products,
            format!("unsupported climo grid meta schema {}", meta.schema),
        ));
    }
    let hrrr_grid = rw_store::grid::GridFile::open(
        &store_root.join(model_slug).join(run_slug).join("grid.rwg"),
    )?;
    if hrrr_grid.hash != meta.hrrr_grid_hash {
        return Ok(block_all(
            &products,
            "climatology was imported against a different HRRR grid".to_string(),
        ));
    }
    if doy < meta.doy_start || doy > meta.doy_end {
        return Ok(block_all(
            &products,
            format!(
                "DOY {doy} outside imported climatology range {}..{}",
                meta.doy_start, meta.doy_end
            ),
        ));
    }
    let climo = match StoreFieldSource::open(store_root, CLIMO_MODEL, &climo_run, doy) {
        Ok(source) => source,
        Err(err) => return Ok(block_all(&products, format!("open climo DOY {doy}: {err}"))),
    };

    let requested_windowed: Vec<String> = products
        .iter()
        .map(|product| product.windowed_slug().to_string())
        .collect();
    let outcome = windowed_store::compute_windowed_products(
        store_root,
        model_slug,
        run_slug,
        &stored_hours,
        &requested_windowed,
    )?;
    let blockers: Vec<(String, String)> = outcome.blockers;

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
    let hrrr_nx = hrrr_grid.nx;

    let mut rendered = Vec::new();
    for product in products {
        let started = Instant::now();
        let source_grid = match outcome
            .grids
            .iter()
            .find(|grid| grid.slug == product.windowed_slug())
        {
            Some(grid) => grid,
            None => {
                let reason = blockers
                    .iter()
                    .find(|(slug, _)| slug == product.windowed_slug())
                    .map(|(_, reason)| reason.clone())
                    .unwrap_or_else(|| {
                        format!("windowed source {} unavailable", product.windowed_slug())
                    });
                skipped.push(StoreRenderSkip {
                    slug: product.slug().to_string(),
                    reason,
                });
                continue;
            }
        };

        // Anchor grids + sample count from the climatology hour.
        let mut anchors = Vec::with_capacity(ANCHOR_STATS.len());
        let mut anchor_error = None;
        for stat in ANCHOR_STATS {
            let name = format!("climo__utc_00_23__{}__{stat}", product.climo_product());
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
                "climo__utc_00_23__{}__sample_count",
                product.climo_product()
            ))
            .ok()
            .map(|stored| median_finite(&stored.values))
            .unwrap_or(f32::NAN);

        // Crop the full-grid windowed values onto the subgrid and rank.
        let mut ranks = vec![f32::NAN; ny * nx];
        let mut unit_error = None;
        for row in 0..ny {
            let hrrr_base = (meta.hrrr_row0 + row) * hrrr_nx + meta.hrrr_col0;
            for col in 0..nx {
                let value = source_grid.values[hrrr_base + col];
                if !value.is_finite() {
                    continue;
                }
                let value = match product.normalize(&source_grid.units, value) {
                    Ok(value) => value as f32,
                    Err(err) => {
                        unit_error = Some(err);
                        break;
                    }
                };
                let cell = row * nx + col;
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
            if unit_error.is_some() {
                break;
            }
        }
        if let Some(reason) = unit_error {
            skipped.push(StoreRenderSkip {
                slug: product.slug().to_string(),
                reason,
            });
            continue;
        }

        let output_path = render_rank_map(
            config,
            &subgrid,
            &projected,
            climo.projection().cloned(),
            product,
            ranks,
            anchor_hour,
            source_grid,
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
    source_grid: &windowed_store::WindowedGrid,
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
        "+/-7d climo 19-26 | DOY {doy}{n_label} | F{:02}-F{:02}",
        source_grid.hours_used.first().copied().unwrap_or(0),
        source_grid.hours_used.last().copied().unwrap_or(anchor_hour),
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
    let output_path = config.out_dir.join(climo_output_filename(
        config.model,
        &config.date_yyyymmdd,
        config.cycle_utc,
        anchor_hour,
        &config.domain,
        product,
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

fn climo_output_filename(
    model: ModelId,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    hour: u16,
    domain: &DomainSpec,
    product: ClimoProduct,
) -> String {
    format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_{}.png",
        model.as_str().replace('-', "_"),
        date_yyyymmdd,
        cycle_utc,
        hour,
        domain.slug,
        product.slug(),
    )
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
    fn slugs_round_trip() {
        for slug in CLIMO_PRODUCTS {
            let product = ClimoProduct::parse(slug).expect(slug);
            assert_eq!(product.slug(), *slug);
        }
        assert!(ClimoProduct::parse("not_a_product").is_none());
    }

    #[test]
    fn unit_normalization_covers_the_windowed_display_units() {
        let vpd = ClimoProduct::VpdDayMaxPercentile;
        assert_eq!(vpd.normalize("kPa", 2.0).unwrap(), 2.0);
        assert!((vpd.normalize("hPa", 20.0).unwrap() - 2.0).abs() < 1e-12);
        assert!(vpd.normalize("%", 20.0).is_err());
        let wind = ClimoProduct::WindDayMaxPercentile;
        assert!((wind.normalize("kt", 10.0).unwrap() - 5.14444).abs() < 1e-4);
        assert_eq!(wind.normalize("m/s", 12.0).unwrap(), 12.0);
        let rh = ClimoProduct::MinRhDayPercentile;
        assert_eq!(rh.normalize("%", 15.0).unwrap(), 15.0);
    }

    #[test]
    fn percentile_scale_bins_cover_the_full_rank_range() {
        for product in [
            ClimoProduct::VpdDayMaxPercentile,
            ClimoProduct::MinRhDayPercentile,
            ClimoProduct::WindDayMaxPercentile,
        ] {
            let scale = product.scale();
            assert_eq!(scale.levels.len(), scale.colors.len() + 1);
            assert!(scale.levels.first().copied().unwrap() <= ANCHOR_LEVELS[0] as f64);
            assert!(scale.levels.last().copied().unwrap() > ANCHOR_LEVELS[7] as f64);
        }
    }
}

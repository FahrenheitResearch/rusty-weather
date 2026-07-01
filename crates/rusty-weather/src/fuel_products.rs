#![allow(dead_code)]

//! Native CAFire fuel-product lane.
//!
//! Fuel products are intentionally not HRRR-derived recipes: they consume
//! store-backed fuel grids (gridMET/NFDRS/LANDFIRE/etc. once ingested) and
//! optionally join them with already-stored HRRR weather diagnostics. Missing
//! fuel data blocks fuel-aware products instead of silently falling back to a
//! weather-only substitute.

use std::path::PathBuf;
use std::time::Instant;

use rayon::prelude::*;
use rustwx_core::{ModelId, SelectedField2D, SourceId};
use rustwx_products::places;
use rustwx_products::plot_design::StaticPlotDesign;
use rustwx_products::shared_context::{DomainSpec, model_time_subtitle, source_subtitle};
use rustwx_render::{
    ChromeScale, Color, ColorScale, DiscreteColorScale, ExtendMode, Field2D, GridShape, LatLonGrid,
    MapRenderRequest, PngWriteOptions, ProductKey, ProductVisualMode, ProjectedDomain,
    ProjectedMap, map_frame_aspect_ratio, save_png_profile_with_options,
};
use rw_store::error::RwStoreError;

use crate::render_all::{RenderedProduct, StoreFieldSource, StoreRenderConfig, StoreRenderSkip};

const WEATHER_FIRE_GRID: &str = "fire_weather_composite";
const WEATHER_HDW_GRID: &str = "hdw";
const WEATHER_VPD_GRID: &str = "vpd_2m";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FuelProduct {
    Kbdi,
    Erc,
    BurningIndex,
    DeadFuelMoisture1h,
    DeadFuelMoisture10h,
    DeadFuelMoisture100h,
    DeadFuelMoisture1000h,
    DailyPrecipFuelContext,
    LandfireFuelModel,
    LandfireFuelLoading,
    FuelReceptiveness,
    FirePotentialComposite,
    HdwFuelReceptive,
    VpdFuelReceptive,
    ErcHdwComposite,
}

impl FuelProduct {
    pub fn parse(slug: &str) -> Option<Self> {
        match normalize_slug(slug).as_str() {
            "kbdi" | "keetch_byram_drought_index" => Some(Self::Kbdi),
            "erc" | "energy_release_component" => Some(Self::Erc),
            "burning_index" | "bi" => Some(Self::BurningIndex),
            "dead_fuel_moisture_1h" | "dfm_1h" | "fm_1h" | "1h_dead_fuel_moisture" => {
                Some(Self::DeadFuelMoisture1h)
            }
            "dead_fuel_moisture_10h" | "dfm_10h" | "fm_10h" | "10h_dead_fuel_moisture" => {
                Some(Self::DeadFuelMoisture10h)
            }
            "dead_fuel_moisture_100h" | "dfm_100h" | "fm_100h" | "100h_dead_fuel_moisture" => {
                Some(Self::DeadFuelMoisture100h)
            }
            "dead_fuel_moisture_1000h" | "dfm_1000h" | "fm_1000h" | "1000h_dead_fuel_moisture" => {
                Some(Self::DeadFuelMoisture1000h)
            }
            "daily_precip_fuel_context" | "fuel_wetting_precip" | "daily_precip_fuel" => {
                Some(Self::DailyPrecipFuelContext)
            }
            "landfire_fuel_model" | "fuel_model" => Some(Self::LandfireFuelModel),
            "landfire_fuel_loading" | "fuel_loading" => Some(Self::LandfireFuelLoading),
            "fuel_receptiveness" | "fuel_receptivity" => Some(Self::FuelReceptiveness),
            "fire_potential_composite" | "weather_fuel_composite" | "fire_potential" => {
                Some(Self::FirePotentialComposite)
            }
            "hdw_fuel_receptive" | "hdw_fuels" => Some(Self::HdwFuelReceptive),
            "vpd_fuel_receptive" | "vpd_fuels" => Some(Self::VpdFuelReceptive),
            "erc_hdw_composite" | "hdw_erc_composite" => Some(Self::ErcHdwComposite),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Kbdi => "kbdi",
            Self::Erc => "erc",
            Self::BurningIndex => "burning_index",
            Self::DeadFuelMoisture1h => "dead_fuel_moisture_1h",
            Self::DeadFuelMoisture10h => "dead_fuel_moisture_10h",
            Self::DeadFuelMoisture100h => "dead_fuel_moisture_100h",
            Self::DeadFuelMoisture1000h => "dead_fuel_moisture_1000h",
            Self::DailyPrecipFuelContext => "daily_precip_fuel_context",
            Self::LandfireFuelModel => "landfire_fuel_model",
            Self::LandfireFuelLoading => "landfire_fuel_loading",
            Self::FuelReceptiveness => "fuel_receptiveness",
            Self::FirePotentialComposite => "fire_potential_composite",
            Self::HdwFuelReceptive => "hdw_fuel_receptive",
            Self::VpdFuelReceptive => "vpd_fuel_receptive",
            Self::ErcHdwComposite => "erc_hdw_composite",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Kbdi => "KBDI",
            Self::Erc => "Energy Release Component",
            Self::BurningIndex => "Burning Index",
            Self::DeadFuelMoisture1h => "1 h Dead Fuel Moisture",
            Self::DeadFuelMoisture10h => "10 h Dead Fuel Moisture",
            Self::DeadFuelMoisture100h => "100 h Dead Fuel Moisture",
            Self::DeadFuelMoisture1000h => "1000 h Dead Fuel Moisture",
            Self::DailyPrecipFuelContext => "Daily Precip Fuel Context",
            Self::LandfireFuelModel => "LANDFIRE Fuel Model",
            Self::LandfireFuelLoading => "LANDFIRE Fuel Loading",
            Self::FuelReceptiveness => "Fuel Receptiveness",
            Self::FirePotentialComposite => "Fire Potential Composite",
            Self::HdwFuelReceptive => "HDW x Fuel Receptiveness",
            Self::VpdFuelReceptive => "VPD x Fuel Receptiveness",
            Self::ErcHdwComposite => "ERC x HDW Composite",
        }
    }

    fn native_grid_slug(self) -> Option<&'static str> {
        match self {
            Self::Kbdi
            | Self::Erc
            | Self::BurningIndex
            | Self::DeadFuelMoisture1h
            | Self::DeadFuelMoisture10h
            | Self::DeadFuelMoisture100h
            | Self::DeadFuelMoisture1000h
            | Self::DailyPrecipFuelContext
            | Self::LandfireFuelModel
            | Self::LandfireFuelLoading => Some(self.slug()),
            Self::FuelReceptiveness
            | Self::FirePotentialComposite
            | Self::HdwFuelReceptive
            | Self::VpdFuelReceptive
            | Self::ErcHdwComposite => None,
        }
    }

    fn scale(self) -> DiscreteColorScale {
        match self {
            Self::Kbdi => DiscreteColorScale {
                levels: range_step(0.0, 801.0, 100.0),
                colors: hex_colors(&[
                    "#f7fbef", "#d7efb1", "#addd8e", "#78c679", "#fed976", "#fd8d3c", "#f03b20",
                    "#bd0026",
                ]),
                extend: ExtendMode::Max,
                mask_below: None,
            },
            Self::Erc
            | Self::FuelReceptiveness
            | Self::FirePotentialComposite
            | Self::HdwFuelReceptive
            | Self::VpdFuelReceptive
            | Self::ErcHdwComposite => DiscreteColorScale {
                levels: range_step(0.0, 101.0, 10.0),
                colors: hex_colors(&[
                    "#f7fbef", "#d9f0a3", "#addd8e", "#78c679", "#fee08b", "#fdae61", "#f46d43",
                    "#d73027", "#a50026", "#542788",
                ]),
                extend: ExtendMode::Neither,
                mask_below: None,
            },
            Self::BurningIndex => DiscreteColorScale {
                levels: range_step(0.0, 151.0, 15.0),
                colors: hex_colors(&[
                    "#f7fbef", "#d9f0a3", "#addd8e", "#78c679", "#fee08b", "#fdae61", "#f46d43",
                    "#d73027", "#a50026", "#542788",
                ]),
                extend: ExtendMode::Max,
                mask_below: None,
            },
            Self::DeadFuelMoisture1h
            | Self::DeadFuelMoisture10h
            | Self::DeadFuelMoisture100h
            | Self::DeadFuelMoisture1000h => DiscreteColorScale {
                levels: range_step(0.0, 41.0, 4.0),
                // Low fuel moisture is dangerous: red/orange at the low end,
                // green/blue at the high/wet end.
                colors: hex_colors(&[
                    "#7f0000", "#b30000", "#e34a33", "#fc8d59", "#fdbb84", "#fee8c8", "#d9f0a3",
                    "#78c679", "#31a354", "#2b8cbe",
                ]),
                extend: ExtendMode::Both,
                mask_below: None,
            },
            Self::DailyPrecipFuelContext => DiscreteColorScale {
                levels: vec![0.0, 0.01, 0.05, 0.10, 0.25, 0.50, 1.0, 2.0],
                colors: hex_colors(&[
                    "#f7fbff", "#deebf7", "#c6dbef", "#9ecae1", "#6baed6", "#3182bd", "#08519c",
                ]),
                extend: ExtendMode::Max,
                mask_below: None,
            },
            Self::LandfireFuelModel => DiscreteColorScale {
                levels: range_step(0.0, 15.0, 1.0),
                colors: hex_colors(&[
                    "#f7f7f7", "#1b9e77", "#66a61e", "#a6d854", "#ffd92f", "#e6ab02", "#d95f02",
                    "#a6761d", "#7570b3", "#1f78b4", "#6a3d9a", "#e7298a", "#b15928", "#666666",
                ]),
                extend: ExtendMode::Max,
                mask_below: None,
            },
            Self::LandfireFuelLoading => DiscreteColorScale {
                levels: vec![0.0, 1.0, 2.5, 5.0, 10.0, 20.0, 40.0, 80.0],
                colors: hex_colors(&[
                    "#f7fcf5", "#e5f5e0", "#c7e9c0", "#a1d99b", "#74c476", "#31a354", "#006d2c",
                ]),
                extend: ExtendMode::Max,
                mask_below: None,
            },
        }
    }

    fn tick_step(self) -> Option<f64> {
        match self {
            Self::Kbdi => Some(100.0),
            Self::Erc
            | Self::FuelReceptiveness
            | Self::FirePotentialComposite
            | Self::HdwFuelReceptive
            | Self::VpdFuelReceptive
            | Self::ErcHdwComposite => Some(20.0),
            Self::BurningIndex => Some(30.0),
            Self::DeadFuelMoisture1h
            | Self::DeadFuelMoisture10h
            | Self::DeadFuelMoisture100h
            | Self::DeadFuelMoisture1000h => Some(8.0),
            Self::DailyPrecipFuelContext => Some(0.25),
            Self::LandfireFuelModel => Some(1.0),
            Self::LandfireFuelLoading => Some(10.0),
        }
    }
}

pub const CAFIRE_FUEL_PRODUCTS: &[&str] = &[
    "kbdi",
    "erc",
    "burning_index",
    "dead_fuel_moisture_1h",
    "dead_fuel_moisture_10h",
    "dead_fuel_moisture_100h",
    "dead_fuel_moisture_1000h",
    "daily_precip_fuel_context",
    "landfire_fuel_model",
    "landfire_fuel_loading",
];

pub const CAFIRE_FUEL_COMPOSITE_PRODUCTS: &[&str] = &[
    "fuel_receptiveness",
    "fire_potential_composite",
    "hdw_fuel_receptive",
    "vpd_fuel_receptive",
    "erc_hdw_composite",
];

pub fn supported_fuel_product_slugs() -> Vec<String> {
    CAFIRE_FUEL_PRODUCTS
        .iter()
        .chain(CAFIRE_FUEL_COMPOSITE_PRODUCTS)
        .map(|slug| (*slug).to_string())
        .collect()
}

pub struct FuelRenderOutcome {
    pub rendered: Vec<RenderedProduct>,
    pub skipped: Vec<StoreRenderSkip>,
}

#[derive(Clone)]
struct FuelPlane {
    units: String,
    values: Vec<f32>,
}

struct PreparedFuelProduct {
    product: FuelProduct,
    plane: FuelPlane,
    resolve_ms: u128,
}

struct FuelRenderContext {
    nx: usize,
    ny: usize,
    lat_deg: Vec<f32>,
    lon_deg: Vec<f32>,
    projection: Option<rustwx_core::GridProjection>,
    projected: ProjectedMap,
    orography: Option<SelectedField2D>,
}

pub fn render_fuel_products_from_store(
    config: &StoreRenderConfig,
    store: &StoreFieldSource,
    hour: u16,
    requested: &[String],
) -> Result<FuelRenderOutcome, Box<dyn std::error::Error>> {
    let mut prepared = Vec::new();
    let mut skipped = Vec::new();
    for slug in requested {
        let Some(product) = FuelProduct::parse(slug) else {
            skipped.push(StoreRenderSkip {
                slug: slug.clone(),
                reason: "unknown fuel product".to_string(),
            });
            continue;
        };
        let started = Instant::now();
        match resolve_fuel_product(product, store) {
            Ok(plane) => prepared.push(PreparedFuelProduct {
                product,
                plane,
                resolve_ms: started.elapsed().as_millis(),
            }),
            Err(reason) => skipped.push(StoreRenderSkip {
                slug: product.slug().to_string(),
                reason,
            }),
        }
    }
    if prepared.is_empty() {
        return Ok(FuelRenderOutcome {
            rendered: Vec::new(),
            skipped,
        });
    }
    let context = build_fuel_render_context(config, store)?;
    let rendered = prepared
        .into_par_iter()
        .map(|prepared| {
            let started = Instant::now();
            render_one_fuel_product(config, &context, hour, prepared.product, prepared.plane)
                .map(|output_path| RenderedProduct {
                    slug: prepared.product.slug().to_string(),
                    total_ms: prepared.resolve_ms + started.elapsed().as_millis(),
                    output_path,
                })
                .map_err(|err| err.to_string())
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::Other, err))?;
    Ok(FuelRenderOutcome { rendered, skipped })
}

fn resolve_fuel_product(
    product: FuelProduct,
    store: &StoreFieldSource,
) -> Result<FuelPlane, String> {
    if let Some(slug) = product.native_grid_slug() {
        return read_plane(store, slug);
    }
    match product {
        FuelProduct::FuelReceptiveness => compute_fuel_receptiveness(store),
        FuelProduct::FirePotentialComposite => {
            let weather = read_plane(store, WEATHER_FIRE_GRID)?;
            let fuels = compute_fuel_receptiveness(store)?;
            Ok(FuelPlane {
                units: "index".to_string(),
                values: combine_weather_and_fuels(&weather.values, &fuels.values, 0.58),
            })
        }
        FuelProduct::HdwFuelReceptive => {
            let hdw = read_plane(store, WEATHER_HDW_GRID)?;
            let fuels = compute_fuel_receptiveness(store)?;
            Ok(FuelPlane {
                units: "index".to_string(),
                values: combine_normalized(&normalize_hdw(&hdw.values), &fuels.values),
            })
        }
        FuelProduct::VpdFuelReceptive => {
            let vpd = read_plane(store, WEATHER_VPD_GRID)?;
            let fuels = compute_fuel_receptiveness(store)?;
            Ok(FuelPlane {
                units: "index".to_string(),
                values: combine_normalized(&normalize_vpd(&vpd.values), &fuels.values),
            })
        }
        FuelProduct::ErcHdwComposite => {
            let erc = read_plane(store, "erc")?;
            let hdw = read_plane(store, WEATHER_HDW_GRID)?;
            let erc_score = normalize_percent_like(&erc.values);
            let hdw_score = normalize_hdw(&hdw.values);
            Ok(FuelPlane {
                units: "index".to_string(),
                values: combine_normalized(&erc_score, &hdw_score),
            })
        }
        _ => unreachable!("native fuel product handled above"),
    }
}

fn read_plane(store: &StoreFieldSource, slug: &str) -> Result<FuelPlane, String> {
    match store.derived_grid(slug) {
        Ok(stored) => Ok(FuelPlane {
            units: stored.units,
            values: stored.values,
        }),
        Err(RwStoreError::UnknownVariable(_)) => Err(format!(
            "not stored: fuel product '{slug}' needs a same-grid rw-store variable named '{slug}'"
        )),
        Err(err) => Err(format!("read fuel grid '{slug}': {err}")),
    }
}

fn compute_fuel_receptiveness(store: &StoreFieldSource) -> Result<FuelPlane, String> {
    let mut components: Vec<Vec<f32>> = Vec::new();
    let mut component_names = Vec::new();
    for (slug, kind) in [
        ("erc", FuelScoreKind::PercentLike),
        ("kbdi", FuelScoreKind::Kbdi),
        ("burning_index", FuelScoreKind::BurningIndex),
        ("dead_fuel_moisture_1h", FuelScoreKind::DeadFuelMoisture1h),
        ("dead_fuel_moisture_10h", FuelScoreKind::DeadFuelMoisture10h),
        (
            "dead_fuel_moisture_100h",
            FuelScoreKind::DeadFuelMoisture100h,
        ),
        (
            "dead_fuel_moisture_1000h",
            FuelScoreKind::DeadFuelMoisture1000h,
        ),
    ] {
        if let Ok(plane) = read_plane(store, slug) {
            components.push(score_fuel_component(&plane.values, kind));
            component_names.push(slug);
        }
    }
    if components.is_empty() {
        return Err(
            "not stored: fuel_receptiveness needs at least one fuel grid (erc, kbdi, burning_index, or dead fuel moisture)"
                .to_string(),
        );
    }
    let len = components[0].len();
    if components.iter().any(|values| values.len() != len) {
        return Err("fuel component grids do not have matching lengths".to_string());
    }
    let mut values = vec![f32::NAN; len];
    for i in 0..len {
        let mut sum = 0.0f32;
        let mut count = 0usize;
        for component in &components {
            let value = component[i];
            if value.is_finite() {
                sum += value;
                count += 1;
            }
        }
        if count > 0 {
            values[i] = (sum / count as f32).clamp(0.0, 100.0);
        }
    }
    Ok(FuelPlane {
        units: format!("index from {}", component_names.join("+")),
        values,
    })
}

#[derive(Debug, Clone, Copy)]
enum FuelScoreKind {
    PercentLike,
    Kbdi,
    BurningIndex,
    DeadFuelMoisture1h,
    DeadFuelMoisture10h,
    DeadFuelMoisture100h,
    DeadFuelMoisture1000h,
}

fn score_fuel_component(values: &[f32], kind: FuelScoreKind) -> Vec<f32> {
    match kind {
        FuelScoreKind::PercentLike => normalize_percent_like(values),
        FuelScoreKind::Kbdi => values
            .iter()
            .map(|&value| score_linear_high_bad(value, 0.0, 800.0))
            .collect(),
        FuelScoreKind::BurningIndex => values
            .iter()
            .map(|&value| score_linear_high_bad(value, 0.0, 150.0))
            .collect(),
        FuelScoreKind::DeadFuelMoisture1h => values
            .iter()
            .map(|&value| score_dead_fuel_moisture(value, 2.0, 25.0))
            .collect(),
        FuelScoreKind::DeadFuelMoisture10h => values
            .iter()
            .map(|&value| score_dead_fuel_moisture(value, 3.0, 30.0))
            .collect(),
        FuelScoreKind::DeadFuelMoisture100h => values
            .iter()
            .map(|&value| score_dead_fuel_moisture(value, 5.0, 35.0))
            .collect(),
        FuelScoreKind::DeadFuelMoisture1000h => values
            .iter()
            .map(|&value| score_dead_fuel_moisture(value, 6.0, 40.0))
            .collect(),
    }
}

fn normalize_percent_like(values: &[f32]) -> Vec<f32> {
    values
        .iter()
        .map(|&value| {
            if value.is_finite() {
                value.clamp(0.0, 100.0)
            } else {
                f32::NAN
            }
        })
        .collect()
}

fn normalize_hdw(values: &[f32]) -> Vec<f32> {
    values
        .iter()
        .map(|&value| score_linear_high_bad(value, 0.0, 800.0))
        .collect()
}

fn normalize_vpd(values: &[f32]) -> Vec<f32> {
    values
        .iter()
        .map(|&value| score_linear_high_bad(value, 0.0, 60.0))
        .collect()
}

fn score_linear_high_bad(value: f32, low: f32, high: f32) -> f32 {
    if !value.is_finite() || high <= low {
        return f32::NAN;
    }
    (((value - low) / (high - low)) * 100.0).clamp(0.0, 100.0)
}

fn score_dead_fuel_moisture(value: f32, critical: f32, wet: f32) -> f32 {
    if !value.is_finite() || wet <= critical {
        return f32::NAN;
    }
    (((wet - value) / (wet - critical)) * 100.0).clamp(0.0, 100.0)
}

fn combine_weather_and_fuels(weather: &[f32], fuels: &[f32], weather_weight: f32) -> Vec<f32> {
    let fuel_weight = 1.0 - weather_weight;
    weather
        .iter()
        .zip(fuels.iter())
        .map(|(&weather, &fuel)| {
            if weather.is_finite() && fuel.is_finite() {
                (weather.clamp(0.0, 100.0) * weather_weight + fuel.clamp(0.0, 100.0) * fuel_weight)
                    .clamp(0.0, 100.0)
            } else {
                f32::NAN
            }
        })
        .collect()
}

fn combine_normalized(a: &[f32], b: &[f32]) -> Vec<f32> {
    a.iter()
        .zip(b.iter())
        .map(|(&a, &b)| {
            if a.is_finite() && b.is_finite() {
                ((a.clamp(0.0, 100.0) / 100.0 * b.clamp(0.0, 100.0) / 100.0).sqrt() * 100.0)
                    .clamp(0.0, 100.0)
            } else {
                f32::NAN
            }
        })
        .collect()
}

fn build_fuel_render_context(
    config: &StoreRenderConfig,
    store: &StoreFieldSource,
) -> Result<FuelRenderContext, Box<dyn std::error::Error>> {
    let full_grid = store.full_grid();
    let projected = rustwx_products::direct::build_projected_map_with_projection(
        &full_grid.lat_deg,
        &full_grid.lon_deg,
        store.projection(),
        config.domain.bounds,
        map_frame_aspect_ratio(config.output_width, config.output_height, true, true),
    )?;
    Ok(FuelRenderContext {
        nx: full_grid.shape.nx,
        ny: full_grid.shape.ny,
        lat_deg: full_grid.lat_deg,
        lon_deg: full_grid.lon_deg,
        projection: store.projection().cloned(),
        projected,
        orography: if rustwx_products::topo::basemap_style_env_is_topo() {
            store.fetch_variable("orography").ok()
        } else {
            None
        },
    })
}

fn render_one_fuel_product(
    config: &StoreRenderConfig,
    context: &FuelRenderContext,
    hour: u16,
    product: FuelProduct,
    plane: FuelPlane,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&config.out_dir)?;
    let render_grid = LatLonGrid::new(
        GridShape::new(context.nx, context.ny)?,
        context.lat_deg.clone(),
        context.lon_deg.clone(),
    )?;
    let field = Field2D::new(
        ProductKey::named(product.slug()),
        plane.units,
        render_grid,
        plane.values,
    )?;
    let mut request = MapRenderRequest::new(field, ColorScale::Discrete(product.scale()));
    request.title = Some(product.title().to_string());
    request.subtitle_left = Some(model_time_subtitle(
        config.model,
        &config.date_yyyymmdd,
        config.cycle_utc,
        hour,
    ));
    request.subtitle_right = Some(fuel_subtitle_right(product, config.source));
    request.cbar_tick_step = product.tick_step();
    request.width = config.output_width;
    request.height = config.output_height;
    request.chrome_scale = static_chrome_scale();
    request.supersample_factor = static_supersample_factor();
    request.supersample_sharpen = static_supersample_sharpen();
    StaticPlotDesign::new(config.domain.bounds, ProductVisualMode::SevereDiagnostic)
        .apply_to_request(&mut request);
    request.projected_domain = Some(ProjectedDomain {
        x: context.projected.projected_x.clone(),
        y: context.projected.projected_y.clone(),
        extent: context.projected.extent.clone(),
    });
    request.projected_lines = context.projected.lines.clone();
    request.projected_polygons = context.projected.polygons.clone();
    request.inverse_raster_projection = context.projected.inverse_raster_projection.clone();
    if let Some(orography) = context.orography.as_ref() {
        rustwx_products::topo::apply_orography_topo_overlay(&mut request, orography)?;
    }
    if let Some(overlay) = config.place_label_overlay.as_ref() {
        places::apply_place_label_overlay(
            &mut request,
            overlay,
            &config.domain,
            &context.lat_deg,
            &context.lon_deg,
            context.projection.as_ref(),
        )?;
    }
    let output_path = config.out_dir.join(fuel_output_filename(
        config.model,
        &config.date_yyyymmdd,
        config.cycle_utc,
        hour,
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

fn fuel_subtitle_right(product: FuelProduct, source: SourceId) -> String {
    let source_label = source_subtitle(source);
    match product.native_grid_slug() {
        Some(_) => format!("{source_label} | fuel layer"),
        None => format!("{source_label} | wx+fuels"),
    }
}

fn fuel_output_filename(
    model: ModelId,
    date_yyyymmdd: &str,
    cycle_utc: u8,
    hour: u16,
    domain: &DomainSpec,
    product: FuelProduct,
) -> String {
    format!(
        "rustwx_{}_{}_{}z_f{:03}_{}_{}.png",
        model.as_str().replace('-', "_"),
        date_yyyymmdd,
        cycle_utc,
        hour,
        domain.slug,
        product.slug()
    )
}

fn normalize_slug(slug: &str) -> String {
    slug.trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn range_step(start: f64, end: f64, step: f64) -> Vec<f64> {
    let mut values = Vec::new();
    let mut current = start;
    while current < end {
        values.push(current);
        current += step;
    }
    values
}

fn hex_colors(values: &[&str]) -> Vec<Color> {
    values.iter().map(|value| hex_color(value)).collect()
}

fn hex_color(value: &str) -> Color {
    let hex = value.trim().trim_start_matches('#');
    let parse = |start| u8::from_str_radix(&hex[start..start + 2], 16).unwrap_or(0);
    Color::rgba(parse(0), parse(2), parse(4), 255)
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
    fn fuel_product_aliases_parse() {
        assert_eq!(
            FuelProduct::parse("100h_dead_fuel_moisture"),
            Some(FuelProduct::DeadFuelMoisture100h)
        );
        assert_eq!(
            FuelProduct::parse("weather-fuel-composite"),
            Some(FuelProduct::FirePotentialComposite)
        );
        assert_eq!(
            FuelProduct::parse("keetch_byram_drought_index"),
            Some(FuelProduct::Kbdi)
        );
    }

    #[test]
    fn dead_fuel_moisture_scores_drier_as_worse() {
        let dry = score_dead_fuel_moisture(4.0, 2.0, 25.0);
        let wet = score_dead_fuel_moisture(20.0, 2.0, 25.0);
        assert!(dry > wet, "dry fuel must score higher than wet fuel");
        assert!((0.0..=100.0).contains(&dry));
        assert!((0.0..=100.0).contains(&wet));
    }

    #[test]
    fn weather_fuel_composite_requires_both_finite_inputs() {
        let weather = vec![100.0, 50.0, f32::NAN];
        let fuels = vec![100.0, 0.0, 100.0];
        let combined = combine_weather_and_fuels(&weather, &fuels, 0.5);
        assert_eq!(combined[0], 100.0);
        assert_eq!(combined[1], 25.0);
        assert!(combined[2].is_nan());
    }

    #[test]
    fn supported_fuel_products_include_raw_and_composites() {
        let slugs = supported_fuel_product_slugs();
        for slug in [
            "kbdi",
            "erc",
            "burning_index",
            "dead_fuel_moisture_1000h",
            "landfire_fuel_model",
            "fuel_receptiveness",
            "fire_potential_composite",
            "erc_hdw_composite",
        ] {
            assert!(slugs.iter().any(|item| item == slug), "missing {slug}");
        }
    }

    #[test]
    fn output_filename_matches_existing_static_map_pattern() {
        let domain = DomainSpec::new("cafire_california", (-126.0, -113.8, 31.9, 42.5));
        assert_eq!(
            fuel_output_filename(
                ModelId::Hrrr,
                "20260629",
                3,
                6,
                &domain,
                FuelProduct::FirePotentialComposite,
            ),
            "rustwx_hrrr_20260629_3z_f006_cafire_california_fire_potential_composite.png"
        );
    }
}

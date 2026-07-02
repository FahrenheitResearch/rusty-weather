#![allow(dead_code)]

//! Shared "render every stored product" flow, used by `rw_render` (one
//! hour per invocation) and `rw_batch` (per pipelined hour) via `#[path]`
//! inclusion. This module owns the product-request partitioning (catalog
//! keywords vs strict slug lists), the per-hour direct + derived/heavy
//! render pass over [`store_render`], and the windowed compute + render
//! pass over [`windowed_store`] + the products crate's windowed render
//! seam. No render logic lives here — everything feeds the EXACT render
//! paths the GRIB-lane smoke bins use (pixel-parity proven in Task 4).

use std::path::{Path, PathBuf};
use std::time::Instant;

use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::{LatestRun, plot_recipe};
use rustwx_products::derived::{
    DerivedBatchRequest, NativeContourRenderMode, is_heavy_derived_recipe_slug,
    store_derived_recipe_slugs, store_heavy_recipe_slugs,
};
use rustwx_products::direct::{DirectBatchRequest, supported_direct_recipe_slugs};
use rustwx_products::places::PlaceLabelOverlay;
use rustwx_products::shared_context::DomainSpec;
use rustwx_products::source::ProductSourceMode;
use rustwx_products::windowed::{
    HrrrWindowedBatchRequest, HrrrWindowedProduct, StoreWindowedGrid,
    render_windowed_products_from_store_grids,
};
use rustwx_render::PngCompressionMode;

#[path = "climo_products.rs"]
pub mod climo_products;
#[path = "climo_rank.rs"]
pub mod climo_rank;
#[path = "fuel_products.rs"]
pub mod fuel_products;
#[path = "store_render.rs"]
pub mod store_render;
#[path = "windowed_store.rs"]
pub mod windowed_store;

pub use store_render::{StoreFieldSource, StoreRenderSkip};

pub const CAFIRE_CORE_HOUR_PRODUCTS: &[&str] = &["vpd_2m", "hdw", "fire_weather_composite"];
pub const CAFIRE_CORE_WINDOWED_PRODUCTS: &[&str] = &["10m_wind_1h_max", "10m_wind_run_max"];
pub const CAFIRE_DIRECT_PRODUCTS: &[&str] = &[
    "2m_temperature_10m_winds",
    "2m_relative_humidity_10m_winds",
    "2m_dewpoint_10m_winds",
    "10m_wind_gusts",
    "visibility",
    "smoke_pm25_native",
    "smoke_column",
];
pub const CAFIRE_WINDOWED_PRODUCTS: &[&str] = &[
    "qpf_1h",
    "10m_wind_1h_max",
    "10m_wind_run_max",
    "2m_temp_0_24h_range",
    "2m_temp_24_48h_range",
    "2m_temp_0_48h_range",
];

/// Which products were asked for, and whether unresolvable ones fail the
/// run (only explicit slug lists are strict; the catalog keywords render
/// what exists and report the rest).
pub struct ProductRequest {
    pub direct: Vec<String>,
    pub derived: Vec<String>,
    pub fuel: Vec<String>,
    pub climo: Vec<String>,
    pub windowed: Vec<String>,
    /// The windowed list came from the "all" keyword: render it only when
    /// the run has more than one stored hour (a single hour realizes only
    /// the degenerate 1 h windows, which the per-hour lanes already cover).
    pub windowed_auto: bool,
    pub strict: bool,
}

impl ProductRequest {
    /// Drop the heavy recipe slugs from a non-strict request — for runs
    /// whose ingest skipped the heavy stage, where the 16 heavy grids are
    /// EXPECTED absent rather than blocked. Returns how many were dropped.
    /// Strict (explicit slug list) requests are left alone: asking for a
    /// heavy product by name against a no-heavy store should fail loudly.
    pub fn drop_heavy_unless_strict(&mut self) -> usize {
        if self.strict {
            return 0;
        }
        let before = self.derived.len();
        self.derived
            .retain(|slug| !is_heavy_derived_recipe_slug(slug));
        before - self.derived.len()
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_string()).collect()
}

fn cafire_hour_products() -> Vec<String> {
    CAFIRE_DIRECT_PRODUCTS
        .iter()
        .chain(CAFIRE_CORE_HOUR_PRODUCTS)
        .map(|item| (*item).to_string())
        .collect()
}

fn product_request(
    direct: Vec<String>,
    derived: Vec<String>,
    windowed: Vec<String>,
    windowed_auto: bool,
) -> Result<ProductRequest, Box<dyn std::error::Error>> {
    product_request_with_fuel(direct, derived, Vec::new(), windowed, windowed_auto)
}

fn product_request_with_fuel(
    direct: Vec<String>,
    derived: Vec<String>,
    fuel: Vec<String>,
    windowed: Vec<String>,
    windowed_auto: bool,
) -> Result<ProductRequest, Box<dyn std::error::Error>> {
    Ok(ProductRequest {
        direct,
        derived,
        fuel,
        climo: Vec::new(),
        windowed,
        windowed_auto,
        strict: false,
    })
}

pub fn partition_products(
    spec: &str,
    model: ModelId,
) -> Result<ProductRequest, Box<dyn std::error::Error>> {
    let derived_catalog = || {
        store_derived_recipe_slugs()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let heavy_catalog = || {
        store_heavy_recipe_slugs()
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let windowed_catalog = || {
        HrrrWindowedProduct::supported_products()
            .iter()
            .map(|product| product.slug().to_string())
            .collect::<Vec<_>>()
    };
    let fuel_catalog = fuel_products::supported_fuel_product_slugs;
    match spec.trim() {
        "none" | "store-only" | "ingest-only" => {
            product_request(Vec::new(), Vec::new(), Vec::new(), false)
        }
        "cafire-core" => product_request(
            Vec::new(),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            strings(CAFIRE_CORE_WINDOWED_PRODUCTS),
            false,
        ),
        "cafire-core-hour" => product_request(
            Vec::new(),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            Vec::new(),
            false,
        ),
        "cafire-core-windowed" => product_request(
            Vec::new(),
            Vec::new(),
            strings(CAFIRE_CORE_WINDOWED_PRODUCTS),
            false,
        ),
        "cafire-hour" => product_request(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            Vec::new(),
            false,
        ),
        "cafire-windowed" => product_request(
            Vec::new(),
            Vec::new(),
            strings(CAFIRE_WINDOWED_PRODUCTS),
            false,
        ),
        "cafire-windowed-expanded" => {
            product_request(Vec::new(), Vec::new(), windowed_catalog(), false)
        }
        "cafire-all" | "cafire-current" | "cafire-ops" => product_request(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            strings(CAFIRE_WINDOWED_PRODUCTS),
            false,
        ),
        "cafire-expanded" | "cafire-store-all" => product_request(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            windowed_catalog(),
            false,
        ),
        "cafire-with-fuels" | "cafire-all-fuels" => product_request_with_fuel(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            fuel_catalog(),
            strings(CAFIRE_WINDOWED_PRODUCTS),
            false,
        ),
        "cafire-expanded-with-fuels" | "cafire-store-all-fuels" => product_request_with_fuel(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            fuel_catalog(),
            windowed_catalog(),
            false,
        ),
        "cafire-hour-with-fuels" => product_request_with_fuel(
            strings(CAFIRE_DIRECT_PRODUCTS),
            strings(CAFIRE_CORE_HOUR_PRODUCTS),
            fuel_catalog(),
            Vec::new(),
            false,
        ),
        "cafire-fuels" | "cafire-fuel" => {
            product_request_with_fuel(Vec::new(), Vec::new(), fuel_catalog(), Vec::new(), false)
        }
        "cafire-fuel-layers" => product_request_with_fuel(
            Vec::new(),
            Vec::new(),
            strings(fuel_products::CAFIRE_FUEL_PRODUCTS),
            Vec::new(),
            false,
        ),
        "cafire-fuel-composites" => product_request_with_fuel(
            Vec::new(),
            Vec::new(),
            strings(fuel_products::CAFIRE_FUEL_COMPOSITE_PRODUCTS),
            Vec::new(),
            false,
        ),
        "cafire-anomaly" | "cafire-climo" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: Vec::new(),
            fuel: Vec::new(),
            climo: strings(climo_products::CLIMO_PRODUCTS),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "cafire-record" | "cafire-vs-record" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: Vec::new(),
            fuel: Vec::new(),
            climo: strings(climo_products::CLIMO_RECORD_PRODUCTS),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "all" => Ok(ProductRequest {
            direct: supported_direct_recipe_slugs(model),
            derived: derived_catalog()
                .into_iter()
                .chain(heavy_catalog())
                .collect(),
            fuel: fuel_catalog(),
            climo: Vec::new(),
            windowed: windowed_catalog(),
            windowed_auto: true,
            strict: false,
        }),
        "direct" => Ok(ProductRequest {
            direct: supported_direct_recipe_slugs(model),
            derived: Vec::new(),
            fuel: Vec::new(),
            climo: Vec::new(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "derived" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: derived_catalog(),
            fuel: Vec::new(),
            climo: Vec::new(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "heavy" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: heavy_catalog(),
            fuel: Vec::new(),
            climo: Vec::new(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "windowed" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: Vec::new(),
            fuel: Vec::new(),
            climo: Vec::new(),
            windowed: windowed_catalog(),
            windowed_auto: false,
            strict: false,
        }),
        "fuel" | "fuels" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: Vec::new(),
            fuel: fuel_catalog(),
            climo: Vec::new(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        list => {
            let mut direct = Vec::new();
            let mut derived = Vec::new();
            let mut fuel = Vec::new();
            let mut climo = Vec::new();
            let mut windowed = Vec::new();
            for slug in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let is_derived = store_derived_recipe_slugs().contains(&slug)
                    || store_heavy_recipe_slugs().contains(&slug)
                    || is_heavy_derived_recipe_slug(slug);
                if HrrrWindowedProduct::from_slug(slug).is_some() {
                    windowed.push(slug.to_string());
                } else if is_derived {
                    derived.push(slug.to_string());
                } else if fuel_products::FuelProduct::parse(slug).is_some() {
                    fuel.push(slug.to_string());
                } else if climo_products::parse_climo_request(slug).is_some()
                    || climo_products::parse_climo_ref(slug).is_some()
                {
                    climo.push(slug.to_string());
                } else if plot_recipe(slug).is_some() {
                    direct.push(slug.to_string());
                } else {
                    return Err(format!(
                        "unknown product '{slug}': neither a direct plot recipe, a \
                         derived/heavy recipe slug, a fuel product slug, a climo anomaly \
                         slug, nor a windowed product slug"
                    )
                    .into());
                }
            }
            if direct.is_empty()
                && derived.is_empty()
                && fuel.is_empty()
                && climo.is_empty()
                && windowed.is_empty()
            {
                return Err("pass at least one product slug via --products".into());
            }
            Ok(ProductRequest {
                direct,
                derived,
                fuel,
                climo,
                windowed,
                windowed_auto: false,
                strict: true,
            })
        }
    }
}

/// Everything the render passes need to know, independent of any bin's CLI.
#[derive(Clone)]
pub struct StoreRenderConfig {
    pub model: ModelId,
    pub date_yyyymmdd: String,
    pub cycle_utc: u8,
    /// Source stamped into provenance subtitles (the store does not record
    /// the fetch source).
    pub source: SourceId,
    pub domain: DomainSpec,
    pub out_dir: PathBuf,
    pub contour_mode: NativeContourRenderMode,
    pub native_fill_level_multiplier: usize,
    pub output_width: u32,
    pub output_height: u32,
    pub png_compression: PngCompressionMode,
    pub place_label_overlay: Option<PlaceLabelOverlay>,
}

impl StoreRenderConfig {
    fn latest_run(&self) -> Result<LatestRun, Box<dyn std::error::Error>> {
        Ok(LatestRun {
            model: self.model,
            cycle: CycleSpec::new(self.date_yyyymmdd.clone(), self.cycle_utc)?,
            source: self.source,
        })
    }
}

/// One rendered product (any lane), with its render wall and output path.
pub struct RenderedProduct {
    pub slug: String,
    pub total_ms: u128,
    pub output_path: PathBuf,
}

/// Outcome of one hour's direct + derived/heavy render pass.
pub struct HourRenderOutcome {
    pub rendered: Vec<RenderedProduct>,
    pub skipped: Vec<StoreRenderSkip>,
}

/// Render the requested direct and derived/heavy products from one stored
/// hour through the existing render paths. Products whose inputs are not
/// in the store come back in `skipped` with the missing selector/grid —
/// the caller decides whether that fails the run (strict requests).
pub fn render_hour_products(
    config: &StoreRenderConfig,
    store: &StoreFieldSource,
    hour: u16,
    direct_slugs: &[String],
    derived_slugs: &[String],
    fuel_slugs: &[String],
    // Optional pacing hook for the direct lane's chunked render: called
    // before each chunk loads its fields. `rw_batch` passes its memory
    // gate (defer chunks inside high-memory ingest windows); `rw_render`
    // passes None. Timing-only — pixels are gate-independent.
    direct_chunk_gate: Option<&dyn Fn()>,
) -> Result<HourRenderOutcome, Box<dyn std::error::Error>> {
    let mut rendered = Vec::new();
    let mut skipped = Vec::new();

    if !direct_slugs.is_empty() {
        let topo_orography = if rustwx_products::topo::basemap_style_env_is_topo() {
            store.fetch_variable("orography").ok()
        } else {
            None
        };
        let direct_request = DirectBatchRequest {
            model: config.model,
            date_yyyymmdd: config.date_yyyymmdd.clone(),
            cycle_override_utc: Some(config.cycle_utc),
            forecast_hour: hour,
            source: config.source,
            domain: config.domain.clone(),
            out_dir: config.out_dir.clone(),
            cache_root: config.out_dir.join("cache"),
            use_cache: false,
            recipe_slugs: direct_slugs.to_vec(),
            product_overrides: std::collections::HashMap::new(),
            contour_mode: config.contour_mode,
            native_fill_level_multiplier: config.native_fill_level_multiplier.max(1),
            output_width: config.output_width,
            output_height: config.output_height,
            png_compression: config.png_compression,
            place_label_overlay: config.place_label_overlay.clone(),
            output_suffix: None,
            subtitle_left_override: None,
            subtitle_right_override: None,
            topo_orography,
        };
        let outcome = store_render::render_direct_recipes_from_store(
            store,
            &direct_request,
            &config.latest_run()?,
            direct_slugs,
            direct_chunk_gate,
        )?;
        rendered.extend(outcome.rendered.into_iter().map(|recipe| RenderedProduct {
            slug: recipe.recipe_slug,
            total_ms: recipe.timing.total_ms,
            output_path: recipe.output_path,
        }));
        skipped.extend(outcome.skipped);
    }

    if !derived_slugs.is_empty() {
        // The derived/heavy store-render pass loads every requested grid
        // as f64 up front (~0.5-0.7 GB at HRRR size); defer its START out
        // of high-memory ingest windows the same way direct chunks defer.
        if let Some(gate) = direct_chunk_gate {
            gate();
        }
        let derived_request = DerivedBatchRequest {
            model: config.model,
            date_yyyymmdd: config.date_yyyymmdd.clone(),
            cycle_override_utc: Some(config.cycle_utc),
            forecast_hour: hour,
            source: config.source,
            domain: config.domain.clone(),
            out_dir: config.out_dir.clone(),
            cache_root: config.out_dir.join("cache"),
            use_cache: false,
            recipe_slugs: derived_slugs.to_vec(),
            surface_product_override: None,
            pressure_product_override: None,
            source_mode: ProductSourceMode::Canonical,
            allow_large_heavy_domain: false,
            contour_mode: config.contour_mode,
            native_fill_level_multiplier: config.native_fill_level_multiplier.max(1),
            output_width: config.output_width,
            output_height: config.output_height,
            png_compression: config.png_compression,
            place_label_overlay: config.place_label_overlay.clone(),
            topo_orography: if rustwx_products::topo::basemap_style_env_is_topo() {
                store.fetch_variable("orography").ok()
            } else {
                None
            },
        };
        let outcome = store_render::render_derived_recipes_from_store(
            store,
            &derived_request,
            config.cycle_utc,
            derived_slugs,
        )?;
        rendered.extend(outcome.rendered.into_iter().map(|recipe| RenderedProduct {
            slug: recipe.recipe_slug,
            total_ms: recipe.timing.total_ms,
            output_path: recipe.output_path,
        }));
        skipped.extend(outcome.skipped);
    }

    if !fuel_slugs.is_empty() {
        if let Some(gate) = direct_chunk_gate {
            gate();
        }
        let outcome =
            fuel_products::render_fuel_products_from_store(config, store, hour, fuel_slugs)?;
        rendered.extend(outcome.rendered);
        skipped.extend(outcome.skipped);
    }

    Ok(HourRenderOutcome { rendered, skipped })
}

/// Outcome of the windowed compute + render pass over the run's stored
/// hours, anchored at the max stored hour.
pub struct WindowedRenderOutcome {
    pub rendered: Vec<RenderedProduct>,
    pub blocked: Vec<StoreRenderSkip>,
    pub anchor_hour: u16,
    pub stored_hours: usize,
    pub compute_ms: u128,
}

/// Compute and render the requested windowed products across the run's
/// stored hours. `auto` is the "all"-keyword gate: with it set, a run with
/// at most one stored hour skips the lane entirely (returns `None`).
/// `store` only carries the run grid + projection for the render half.
pub fn render_windowed_products(
    config: &StoreRenderConfig,
    store: &StoreFieldSource,
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    requested: &[String],
    auto: bool,
) -> Result<Option<WindowedRenderOutcome>, Box<dyn std::error::Error>> {
    let stored_hours = windowed_store::stored_run_hours(store_root, model_slug, run_slug)?;
    if auto && stored_hours.len() <= 1 {
        return Ok(None);
    }
    let compute_started = Instant::now();
    let outcome = windowed_store::compute_windowed_products(
        store_root,
        model_slug,
        run_slug,
        &stored_hours,
        requested,
    )?;
    let compute_ms = compute_started.elapsed().as_millis();
    let windowed_request = HrrrWindowedBatchRequest {
        model: config.model,
        date_yyyymmdd: config.date_yyyymmdd.clone(),
        cycle_override_utc: Some(config.cycle_utc),
        forecast_hour: outcome.anchor_hour,
        source: config.source,
        domain: config.domain.clone(),
        out_dir: config.out_dir.clone(),
        cache_root: config.out_dir.join("cache"),
        use_cache: false,
        products: Vec::new(),
        output_width: config.output_width,
        output_height: config.output_height,
        png_compression: config.png_compression,
        place_label_overlay: config.place_label_overlay.clone(),
        topo_orography: if rustwx_products::topo::basemap_style_env_is_topo() {
            store.fetch_variable("orography").ok()
        } else {
            None
        },
    };
    let grids: Vec<StoreWindowedGrid> = outcome
        .grids
        .into_iter()
        .map(|grid| StoreWindowedGrid {
            slug: grid.slug,
            units: grid.units,
            values: grid.values,
            hours_used: grid.hours_used,
            window_hours: grid.window_hours,
            strategy: grid.strategy,
        })
        .collect();
    let rendered = render_windowed_products_from_store_grids(
        &windowed_request,
        config.cycle_utc,
        &store.full_grid(),
        store.projection(),
        &grids,
    )?;
    Ok(Some(WindowedRenderOutcome {
        rendered: rendered
            .into_iter()
            .map(|product| RenderedProduct {
                slug: product.product.slug().to_string(),
                total_ms: product.timing.total_ms,
                output_path: product.output_path,
            })
            .collect(),
        blocked: outcome
            .blockers
            .into_iter()
            .map(|(slug, reason)| StoreRenderSkip { slug, reason })
            .collect(),
        anchor_hour: outcome.anchor_hour,
        stored_hours: stored_hours.len(),
        compute_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The public preview site must expose every renderable product as an
    /// option. Compares the site's HTML against the live catalogs so a new
    /// recipe slug fails this test until it is either added to the page or
    /// pinned in the exclusion list with a reason.
    #[test]
    fn preview_site_exposes_the_full_catalog() {
        let html = include_str!("cafire_preview.html");
        // Intentionally not on the preview site (reason pinned):
        let excluded: &[(&str, &str)] = &[
            ("total_qpf", "alias of qpf_total"),
            ("10m_wind_1h_max", "trailing 1h window; run-max + 0-24h cover the use"),
            ("uh_2to5km_1h_max", "trailing 1h window; run-max covers the use"),
            ("uh_2to5km_3h_max", "trailing 3h window; run-max covers the use"),
            ("sblcl", "sounding-adjacent; meteogram territory"),
            ("mucin", "CIN pair covered by mlcin"),
            ("sbcin", "CIN pair covered by mlcin"),
            ("ehi_0_1km", "0-3km EHI shown; 0-1km niche"),
            ("scp_mu_0_3km_0_6km_proxy", "proxy composite; STP shown"),
            ("2m_temperature", "barbless duplicate of 2m_temperature_10m_winds"),
            ("2m_dewpoint", "barbless duplicate of 2m_dewpoint_10m_winds"),
            ("2m_relative_humidity", "barbless duplicate of RH + winds"),
            ("low_cloud_cover", "cloud_cover_levels panel shows all three"),
            ("middle_cloud_cover", "cloud_cover_levels panel shows all three"),
            ("high_cloud_cover", "cloud_cover_levels panel shows all three"),
        ];
        let is_excluded = |slug: &str| {
            excluded.iter().any(|(excluded_slug, _)| *excluded_slug == slug)
                // The ECAPE/heavy lane stays off the operational site by
                // Drew's directive (view-profile stores don't carry it).
                || store_heavy_recipe_slugs().contains(&slug)
        };
        let mut missing: Vec<String> = Vec::new();
        let mut check = |slug: &str| {
            if !is_excluded(slug) && !html.contains(&format!("\"{slug}\"")) {
                missing.push(slug.to_string());
            }
        };
        for slug in supported_direct_recipe_slugs(ModelId::Hrrr) {
            check(&slug);
        }
        for slug in store_derived_recipe_slugs() {
            check(slug);
        }
        for product in HrrrWindowedProduct::supported_products() {
            check(product.slug());
        }
        for slug in fuel_products::supported_fuel_product_slugs() {
            check(&slug);
        }
        // Anomaly slugs appear literally; the _vs_record twins are derived
        // in page JS from the same array, so the base list is the contract.
        for slug in climo_products::CLIMO_PRODUCTS {
            check(slug);
        }
        assert!(
            missing.is_empty(),
            "preview site is missing {} product option(s): {missing:#?}",
            missing.len()
        );
    }

    #[test]
    fn products_keywords_pull_the_catalogs() {
        let all = partition_products("all", ModelId::Hrrr).unwrap();
        assert!(!all.strict);
        assert_eq!(all.direct, supported_direct_recipe_slugs(ModelId::Hrrr));
        assert_eq!(
            all.derived.len(),
            store_derived_recipe_slugs().len() + store_heavy_recipe_slugs().len()
        );
        assert_eq!(
            all.fuel.len(),
            fuel_products::supported_fuel_product_slugs().len()
        );
        assert_eq!(
            all.windowed.len(),
            HrrrWindowedProduct::supported_products().len()
        );
        assert!(
            all.windowed_auto,
            "'all' must gate windowed on multi-hour stores"
        );

        let heavy = partition_products("heavy", ModelId::Hrrr).unwrap();
        assert!(heavy.direct.is_empty());
        assert_eq!(heavy.derived.len(), store_heavy_recipe_slugs().len());
        assert!(heavy.fuel.is_empty());
        assert!(heavy.windowed.is_empty());

        let windowed = partition_products("windowed", ModelId::Hrrr).unwrap();
        assert!(windowed.direct.is_empty() && windowed.derived.is_empty());
        assert!(windowed.fuel.is_empty());
        assert_eq!(
            windowed.windowed.len(),
            HrrrWindowedProduct::supported_products().len()
        );
        assert!(
            !windowed.windowed_auto,
            "explicit 'windowed' keyword must render even single-hour stores"
        );
        assert!(!windowed.strict);
    }

    #[test]
    fn none_keyword_builds_store_without_render_requests() {
        for keyword in ["none", "store-only", "ingest-only"] {
            let request = partition_products(keyword, ModelId::Hrrr).unwrap();
            assert!(request.direct.is_empty(), "{keyword} direct");
            assert!(request.derived.is_empty(), "{keyword} derived");
            assert!(request.fuel.is_empty(), "{keyword} fuel");
            assert!(request.windowed.is_empty(), "{keyword} windowed");
            assert!(!request.strict);
            assert!(!request.windowed_auto);
        }
    }

    #[test]
    fn cafire_all_matches_current_store_backed_product_table() {
        let request = partition_products("cafire-all", ModelId::Hrrr).unwrap();
        assert!(!request.strict);
        assert_eq!(request.direct, strings(CAFIRE_DIRECT_PRODUCTS));
        assert_eq!(request.derived, strings(CAFIRE_CORE_HOUR_PRODUCTS));
        assert!(request.fuel.is_empty());
        assert_eq!(request.windowed, strings(CAFIRE_WINDOWED_PRODUCTS));
        for slug in [
            "2m_temperature_10m_winds",
            "2m_relative_humidity_10m_winds",
            "2m_dewpoint_10m_winds",
            "10m_wind_gusts",
            "visibility",
            "smoke_pm25_native",
            "smoke_column",
            "vpd_2m",
            "hdw",
            "fire_weather_composite",
            "qpf_1h",
            "10m_wind_1h_max",
            "10m_wind_run_max",
            "2m_temp_0_24h_range",
            "2m_temp_24_48h_range",
            "2m_temp_0_48h_range",
        ] {
            assert!(
                request.direct.iter().any(|item| item == slug)
                    || request.derived.iter().any(|item| item == slug)
                    || request.fuel.iter().any(|item| item == slug)
                    || request.windowed.iter().any(|item| item == slug),
                "missing {slug}"
            );
        }
    }

    #[test]
    fn cafire_expanded_adds_every_current_windowed_store_product() {
        let request = partition_products("cafire-expanded", ModelId::Hrrr).unwrap();
        assert_eq!(request.direct, strings(CAFIRE_DIRECT_PRODUCTS));
        assert_eq!(request.derived, strings(CAFIRE_CORE_HOUR_PRODUCTS));
        assert!(request.fuel.is_empty());
        assert_eq!(
            request.windowed.len(),
            HrrrWindowedProduct::supported_products().len()
        );
        assert!(
            request
                .windowed
                .iter()
                .any(|slug| slug == "2m_rh_0_24h_min")
        );
        assert!(
            request
                .windowed
                .iter()
                .any(|slug| slug == "2m_vpd_0_48h_max")
        );
        assert!(request.windowed.iter().any(|slug| slug == "qpf_24h"));
        assert!(
            request
                .windowed
                .iter()
                .any(|slug| slug == "10m_wind_0_48h_max")
        );
        assert!(!request.strict);
    }

    #[test]
    fn cafire_fuel_presets_are_native_hour_products() {
        let request = partition_products("cafire-with-fuels", ModelId::Hrrr).unwrap();
        assert_eq!(request.direct, strings(CAFIRE_DIRECT_PRODUCTS));
        assert_eq!(request.derived, strings(CAFIRE_CORE_HOUR_PRODUCTS));
        assert_eq!(request.fuel, fuel_products::supported_fuel_product_slugs());
        assert_eq!(request.windowed, strings(CAFIRE_WINDOWED_PRODUCTS));
        assert!(!request.strict);

        let fuels = partition_products("cafire-fuels", ModelId::Hrrr).unwrap();
        assert!(fuels.direct.is_empty());
        assert!(fuels.derived.is_empty());
        assert_eq!(fuels.fuel, fuel_products::supported_fuel_product_slugs());
        assert!(fuels.windowed.is_empty());
    }

    #[test]
    fn product_lists_classify_into_lanes_and_are_strict() {
        let picked = partition_products(
            "2m_temperature,sbcape,ecape_stp,kbdi,fire_potential_composite,qpf_6h,uh_2to5km_run_max",
            ModelId::Hrrr,
        )
        .unwrap();
        assert!(picked.strict);
        assert_eq!(picked.direct, vec!["2m_temperature".to_string()]);
        assert_eq!(
            picked.derived,
            vec!["sbcape".to_string(), "ecape_stp".to_string()]
        );
        assert_eq!(
            picked.fuel,
            vec!["kbdi".to_string(), "fire_potential_composite".to_string()]
        );
        assert_eq!(
            picked.windowed,
            vec!["qpf_6h".to_string(), "uh_2to5km_run_max".to_string()]
        );
        assert!(!picked.windowed_auto);
        assert!(partition_products("definitely_not_a_product", ModelId::Hrrr).is_err());
    }

    #[test]
    fn drop_heavy_strips_only_heavy_slugs_and_respects_strict() {
        let mut all = partition_products("all", ModelId::Hrrr).unwrap();
        let dropped = all.drop_heavy_unless_strict();
        assert_eq!(dropped, store_heavy_recipe_slugs().len());
        assert_eq!(all.derived.len(), store_derived_recipe_slugs().len());
        assert!(
            all.derived
                .iter()
                .all(|slug| !is_heavy_derived_recipe_slug(slug))
        );

        let mut strict = partition_products("sbcape,ecape_stp", ModelId::Hrrr).unwrap();
        assert_eq!(strict.drop_heavy_unless_strict(), 0);
        assert_eq!(
            strict.derived,
            vec!["sbcape".to_string(), "ecape_stp".to_string()],
            "strict requests must keep explicitly named heavy slugs"
        );
    }
}

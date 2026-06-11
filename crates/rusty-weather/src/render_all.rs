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

#[path = "store_render.rs"]
pub mod store_render;
#[path = "windowed_store.rs"]
pub mod windowed_store;

pub use store_render::{StoreFieldSource, StoreRenderSkip};

/// Which products were asked for, and whether unresolvable ones fail the
/// run (only explicit slug lists are strict; the catalog keywords render
/// what exists and report the rest).
pub struct ProductRequest {
    pub direct: Vec<String>,
    pub derived: Vec<String>,
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
    match spec.trim() {
        "all" => Ok(ProductRequest {
            direct: supported_direct_recipe_slugs(model),
            derived: derived_catalog()
                .into_iter()
                .chain(heavy_catalog())
                .collect(),
            windowed: windowed_catalog(),
            windowed_auto: true,
            strict: false,
        }),
        "direct" => Ok(ProductRequest {
            direct: supported_direct_recipe_slugs(model),
            derived: Vec::new(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "derived" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: derived_catalog(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "heavy" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: heavy_catalog(),
            windowed: Vec::new(),
            windowed_auto: false,
            strict: false,
        }),
        "windowed" => Ok(ProductRequest {
            direct: Vec::new(),
            derived: Vec::new(),
            windowed: windowed_catalog(),
            windowed_auto: false,
            strict: false,
        }),
        list => {
            let mut direct = Vec::new();
            let mut derived = Vec::new();
            let mut windowed = Vec::new();
            for slug in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                let is_derived = store_derived_recipe_slugs().contains(&slug)
                    || store_heavy_recipe_slugs().contains(&slug)
                    || is_heavy_derived_recipe_slug(slug);
                if HrrrWindowedProduct::from_slug(slug).is_some() {
                    windowed.push(slug.to_string());
                } else if is_derived {
                    derived.push(slug.to_string());
                } else if plot_recipe(slug).is_some() {
                    direct.push(slug.to_string());
                } else {
                    return Err(format!(
                        "unknown product '{slug}': neither a direct plot recipe, a \
                         derived/heavy recipe slug, nor a windowed product slug"
                    )
                    .into());
                }
            }
            if direct.is_empty() && derived.is_empty() && windowed.is_empty() {
                return Err("pass at least one product slug via --products".into());
            }
            Ok(ProductRequest {
                direct,
                derived,
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
    // Optional pacing hook for the direct lane's chunked render: called
    // before each chunk loads its fields. `rw_batch` passes its memory
    // gate (defer chunks inside high-memory ingest windows); `rw_render`
    // passes None. Timing-only — pixels are gate-independent.
    direct_chunk_gate: Option<&dyn Fn()>,
) -> Result<HourRenderOutcome, Box<dyn std::error::Error>> {
    let mut rendered = Vec::new();
    let mut skipped = Vec::new();

    if !direct_slugs.is_empty() {
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
        assert!(heavy.windowed.is_empty());

        let windowed = partition_products("windowed", ModelId::Hrrr).unwrap();
        assert!(windowed.direct.is_empty() && windowed.derived.is_empty());
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
    fn product_lists_classify_into_lanes_and_are_strict() {
        let picked = partition_products(
            "2m_temperature,sbcape,ecape_stp,qpf_6h,uh_2to5km_run_max",
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

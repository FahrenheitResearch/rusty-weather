//! Render every stored product from one rw-store hour to PNG, through the
//! EXACT render paths the GRIB-lane smoke bins use (proven pixel-identical
//! for representative direct and derived products — see the Task 4 parity
//! matrix in the commit). Direct recipes resolve their fetch-plan
//! `FieldSelector`s against the stored variable metadata; derived/heavy
//! recipes read their precomputed slug-named grids. Products whose inputs
//! are not in the store are reported as unresolvable with the missing
//! selector — a failure only when the product was requested explicitly.

use std::path::PathBuf;
use std::time::Instant;

#[path = "../contour_mode.rs"]
mod contour_mode;
#[path = "../domain.rs"]
mod domain;
#[path = "../region.rs"]
mod region;
#[path = "../store_render.rs"]
mod store_render;
#[path = "../throttle.rs"]
mod throttle;
#[path = "../windowed_store.rs"]
mod windowed_store;

use clap::{Parser, ValueEnum};
use contour_mode::ContourModeArg;
use region::RegionPreset;
use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_models::{LatestRun, plot_recipe};
use rustwx_products::derived::{
    DerivedBatchRequest, is_heavy_derived_recipe_slug, store_derived_recipe_slugs,
    store_heavy_recipe_slugs,
};
use rustwx_products::direct::{DirectBatchRequest, supported_direct_recipe_slugs};
use rustwx_products::places::{PlaceLabelDensityTier, default_place_label_overlay_for_domain};
use rustwx_products::shared_context::DomainSpec;
use rustwx_products::source::ProductSourceMode;
use rustwx_products::windowed::{
    HrrrWindowedBatchRequest, HrrrWindowedProduct, StoreWindowedGrid,
    render_windowed_products_from_store_grids,
};
use store_render::{StoreFieldSource, StoreRenderSkip};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum PngCompressionArg {
    Default,
    Fast,
    Fastest,
}

impl From<PngCompressionArg> for rustwx_render::PngCompressionMode {
    fn from(value: PngCompressionArg) -> Self {
        match value {
            PngCompressionArg::Default => Self::Default,
            PngCompressionArg::Fast => Self::Fast,
            PngCompressionArg::Fastest => Self::Fastest,
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "rw-render",
    about = "Render stored rw-store products to PNG through the existing render paths"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run slug as stored, e.g. 20260608_00z")]
    run: String,
    #[arg(long, help = "Forecast hour of the stored .rws file")]
    hour: u16,
    #[arg(
        long,
        default_value = "all",
        help = "all | direct | derived | heavy | windowed | comma-separated product slugs \
                (windowed products span the run's stored hours, anchored at the max hour; \
                'all' includes them only when more than one hour is stored)"
    )]
    products: String,
    #[arg(long, value_enum, default_value_t = RegionPreset::Midwest)]
    region: RegionPreset,
    #[arg(long, default_value = "out/rw_render")]
    out_dir: PathBuf,
    #[arg(
        long,
        help = "Source stamped into provenance subtitles; defaults to the model's primary source \
                (the store does not record the fetch source)"
    )]
    source: Option<SourceId>,
    #[arg(long, value_enum, default_value_t = ContourModeArg::Automatic)]
    contour_mode: ContourModeArg,
    #[arg(long, default_value_t = 1)]
    native_fill_level_multiplier: usize,
    #[arg(long = "png-compression", value_enum, default_value_t = PngCompressionArg::Fast)]
    png_compression: PngCompressionArg,
    #[arg(long = "place-label-density", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=3))]
    place_label_density: u8,
    #[arg(
        long,
        help = "Rayon thread count (default: all cores minus 2 in polite mode, all cores with --full-throttle)"
    )]
    threads: Option<usize>,
    #[arg(
        long,
        default_value_t = false,
        help = "Dedicated-node mode: keep normal process priority and use every core"
    )]
    full_throttle: bool,
}

/// Which products were asked for, and whether unresolvable ones fail the
/// run (only explicit slug lists are strict; the catalog keywords render
/// what exists and report the rest).
struct ProductRequest {
    direct: Vec<String>,
    derived: Vec<String>,
    windowed: Vec<String>,
    /// The windowed list came from the "all" keyword: render it only when
    /// the run has more than one stored hour (a single hour realizes only
    /// the degenerate 1 h windows, which the per-hour lanes already cover).
    windowed_auto: bool,
    strict: bool,
}

fn partition_products(
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

/// `YYYYMMDD_CCz` -> (date, cycle).
fn parse_run_slug(run: &str) -> Result<(String, u8), Box<dyn std::error::Error>> {
    let (date, cycle) = run
        .split_once('_')
        .ok_or_else(|| format!("--run '{run}' is not of the form YYYYMMDD_CCz"))?;
    if date.len() != 8 || !date.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("--run '{run}': '{date}' is not a YYYYMMDD date").into());
    }
    let cycle = cycle
        .strip_suffix('z')
        .and_then(|value| value.parse::<u8>().ok())
        .filter(|value| *value < 24)
        .ok_or_else(|| format!("--run '{run}': '{cycle}' is not a cycle of the form CCz"))?;
    Ok((date.to_string(), cycle))
}

fn static_output_dimension(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 320)
        .unwrap_or(fallback)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Scheduling policy must land before anything touches rayon (the
    // global pool is built lazily on first use and cannot be resized).
    throttle::apply(args.threads, args.full_throttle);
    run(&args)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let total_started = Instant::now();
    let (date, cycle) = parse_run_slug(&args.run)?;
    let request = partition_products(&args.products, args.model)?;
    let model_slug = args.model.as_str().replace('-', "_");
    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let latest = LatestRun {
        model: args.model,
        cycle: CycleSpec::new(date.clone(), cycle)?,
        source,
    };
    let domain = DomainSpec::new(args.region.slug(), args.region.bounds());
    let place_label_overlay = default_place_label_overlay_for_domain(
        &domain,
        PlaceLabelDensityTier::from_numeric(args.place_label_density),
    );
    let output_width = static_output_dimension("RUSTWX_STATIC_OUTPUT_WIDTH", 1600);
    let output_height = static_output_dimension("RUSTWX_STATIC_OUTPUT_HEIGHT", 900);

    let open_started = Instant::now();
    let store = StoreFieldSource::open(&args.store_root, &model_slug, &args.run, args.hour)?;
    let open_ms = open_started.elapsed().as_millis();
    println!(
        "rw_render build {} | store {} | {} products requested ({} direct, {} derived/heavy, {} windowed) | open {} ms",
        env!("RW_BUILD_SHA"),
        store.hour_path().display(),
        request.direct.len() + request.derived.len() + request.windowed.len(),
        request.direct.len(),
        request.derived.len(),
        request.windowed.len(),
        open_ms,
    );

    let mut timings: Vec<(String, u128)> = Vec::new();
    let mut skipped: Vec<StoreRenderSkip> = Vec::new();

    if !request.direct.is_empty() {
        let direct_request = DirectBatchRequest {
            model: args.model,
            date_yyyymmdd: date.clone(),
            cycle_override_utc: Some(cycle),
            forecast_hour: args.hour,
            source,
            domain: domain.clone(),
            out_dir: args.out_dir.clone(),
            cache_root: args.out_dir.join("cache"),
            use_cache: false,
            recipe_slugs: request.direct.clone(),
            product_overrides: std::collections::HashMap::new(),
            contour_mode: args.contour_mode.into(),
            native_fill_level_multiplier: args.native_fill_level_multiplier.max(1),
            output_width,
            output_height,
            png_compression: args.png_compression.into(),
            place_label_overlay: place_label_overlay.clone(),
            output_suffix: None,
            subtitle_left_override: None,
            subtitle_right_override: None,
        };
        let outcome = store_render::render_direct_recipes_from_store(
            &store,
            &direct_request,
            &latest,
            &request.direct,
        )?;
        for recipe in &outcome.rendered {
            println!(
                "{:>8} ms  {}  {}",
                recipe.timing.total_ms,
                recipe.recipe_slug,
                recipe.output_path.display()
            );
            timings.push((recipe.recipe_slug.clone(), recipe.timing.total_ms));
        }
        skipped.extend(outcome.skipped);
    }

    if !request.derived.is_empty() {
        let derived_request = DerivedBatchRequest {
            model: args.model,
            date_yyyymmdd: date.clone(),
            cycle_override_utc: Some(cycle),
            forecast_hour: args.hour,
            source,
            domain: domain.clone(),
            out_dir: args.out_dir.clone(),
            cache_root: args.out_dir.join("cache"),
            use_cache: false,
            recipe_slugs: request.derived.clone(),
            surface_product_override: None,
            pressure_product_override: None,
            source_mode: ProductSourceMode::Canonical,
            allow_large_heavy_domain: false,
            contour_mode: args.contour_mode.into(),
            native_fill_level_multiplier: args.native_fill_level_multiplier.max(1),
            output_width,
            output_height,
            png_compression: args.png_compression.into(),
            place_label_overlay: place_label_overlay.clone(),
        };
        let outcome = store_render::render_derived_recipes_from_store(
            &store,
            &derived_request,
            cycle,
            &request.derived,
        )?;
        for recipe in &outcome.rendered {
            println!(
                "{:>8} ms  {}  {}",
                recipe.timing.total_ms,
                recipe.recipe_slug,
                recipe.output_path.display()
            );
            timings.push((recipe.recipe_slug.clone(), recipe.timing.total_ms));
        }
        skipped.extend(outcome.skipped);
    }

    if !request.windowed.is_empty() {
        let stored_hours =
            windowed_store::stored_run_hours(&args.store_root, &model_slug, &args.run)?;
        if request.windowed_auto && stored_hours.len() <= 1 {
            println!(
                "windowed: skipped ({} stored hour(s); 'all' includes windowed products \
                 only when more than one hour is stored)",
                stored_hours.len(),
            );
        } else {
            let compute_started = Instant::now();
            let outcome = windowed_store::compute_windowed_products(
                &args.store_root,
                &model_slug,
                &args.run,
                &stored_hours,
                &request.windowed,
            )?;
            let compute_ms = compute_started.elapsed().as_millis();
            let windowed_request = HrrrWindowedBatchRequest {
                model: args.model,
                date_yyyymmdd: date.clone(),
                cycle_override_utc: Some(cycle),
                forecast_hour: outcome.anchor_hour,
                source,
                domain: domain.clone(),
                out_dir: args.out_dir.clone(),
                cache_root: args.out_dir.join("cache"),
                use_cache: false,
                products: Vec::new(),
                output_width,
                output_height,
                png_compression: args.png_compression.into(),
                place_label_overlay: place_label_overlay.clone(),
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
                cycle,
                &store.full_grid(),
                store.projection(),
                &grids,
            )?;
            println!(
                "windowed: {} realized, {} blocked | anchor F{:03} over {} stored hour(s) | compute {} ms",
                rendered.len(),
                outcome.blockers.len(),
                outcome.anchor_hour,
                stored_hours.len(),
                compute_ms,
            );
            for product in &rendered {
                println!(
                    "{:>8} ms  {}  {}",
                    product.timing.total_ms,
                    product.product.slug(),
                    product.output_path.display()
                );
                timings.push((product.product.slug().to_string(), product.timing.total_ms));
            }
            skipped.extend(
                outcome
                    .blockers
                    .into_iter()
                    .map(|(slug, reason)| StoreRenderSkip { slug, reason }),
            );
        }
    }

    let total_ms = total_started.elapsed().as_millis();
    if !timings.is_empty() {
        let mut sorted: Vec<u128> = timings.iter().map(|(_, ms)| *ms).collect();
        sorted.sort_unstable();
        println!(
            "rendered {} products | per-product ms min {} / median {} / max {} | total wall {} ms",
            timings.len(),
            sorted[0],
            sorted[sorted.len() / 2],
            sorted[sorted.len() - 1],
            total_ms,
        );
    } else {
        println!("rendered 0 products | total wall {total_ms} ms");
    }
    if !skipped.is_empty() {
        eprintln!("unresolvable products ({}):", skipped.len());
        for skip in &skipped {
            eprintln!("  {}: {}", skip.slug, skip.reason);
        }
        if request.strict {
            return Err(format!(
                "{} explicitly requested product(s) could not render from the store",
                skipped.len()
            )
            .into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_slug_parses_date_and_cycle() {
        assert_eq!(
            parse_run_slug("20260608_00z").unwrap(),
            ("20260608".to_string(), 0)
        );
        assert_eq!(
            parse_run_slug("20251231_23z").unwrap(),
            ("20251231".to_string(), 23)
        );
    }

    #[test]
    fn run_slug_rejects_malformed_inputs_naming_the_flag() {
        for bad in ["20260608", "2026060_00z", "20260608_24z", "20260608_0", ""] {
            let err = parse_run_slug(bad).unwrap_err().to_string();
            assert!(
                err.contains("--run"),
                "error for '{bad}' must name --run: {err}"
            );
        }
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
}

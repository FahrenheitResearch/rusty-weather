//! Render every stored product from one rw-store hour to PNG, through the
//! EXACT render paths the GRIB-lane smoke bins use (proven pixel-identical
//! for representative direct and derived products — see the Task 4 parity
//! matrix in the commit). The flow lives in the shared `render_all` module
//! (also driven per-hour by `rw_batch`): direct recipes resolve their
//! fetch-plan `FieldSelector`s against the stored variable metadata;
//! derived/heavy recipes read their precomputed slug-named grids; windowed
//! products accumulate across the run's stored hours. Products whose
//! inputs are not in the store are reported as unresolvable with the
//! missing selector — a failure only when requested explicitly.

use std::path::PathBuf;
use std::time::Instant;

#[path = "../contour_mode.rs"]
mod contour_mode;
#[path = "../region.rs"]
mod region;
#[path = "../render_all.rs"]
mod render_all;
use rw_ingest::throttle;

use clap::{Parser, ValueEnum};
use contour_mode::ContourModeArg;
use region::RegionPreset;
use render_all::{StoreFieldSource, StoreRenderConfig, StoreRenderSkip};
use rustwx_core::{ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::places::{PlaceLabelDensityTier, default_place_label_overlay_for_domain};
use rustwx_products::shared_context::DomainSpec;

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
    let request = render_all::partition_products(&args.products, args.model)?;
    let model_slug = args.model.as_str().replace('-', "_");
    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let domain = DomainSpec::new(args.region.slug(), args.region.bounds());
    let config = StoreRenderConfig {
        model: args.model,
        date_yyyymmdd: date,
        cycle_utc: cycle,
        source,
        domain: domain.clone(),
        out_dir: args.out_dir.clone(),
        contour_mode: args.contour_mode.into(),
        native_fill_level_multiplier: args.native_fill_level_multiplier.max(1),
        output_width: static_output_dimension("RUSTWX_STATIC_OUTPUT_WIDTH", 1600),
        output_height: static_output_dimension("RUSTWX_STATIC_OUTPUT_HEIGHT", 900),
        png_compression: args.png_compression.into(),
        place_label_overlay: default_place_label_overlay_for_domain(
            &domain,
            PlaceLabelDensityTier::from_numeric(args.place_label_density),
        ),
    };

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

    let hour_outcome = render_all::render_hour_products(
        &config,
        &store,
        args.hour,
        &request.direct,
        &request.derived,
        // Solo render: nothing else competes for memory, no chunk gate.
        None,
    )?;
    for product in &hour_outcome.rendered {
        println!(
            "{:>8} ms  {}  {}",
            product.total_ms,
            product.slug,
            product.output_path.display()
        );
        timings.push((product.slug.clone(), product.total_ms));
    }
    skipped.extend(hour_outcome.skipped);

    if !request.windowed.is_empty() {
        match render_all::render_windowed_products(
            &config,
            &store,
            &args.store_root,
            &model_slug,
            &args.run,
            &request.windowed,
            request.windowed_auto,
        )? {
            None => println!(
                "windowed: skipped (single stored hour; 'all' includes windowed products \
                 only when more than one hour is stored)"
            ),
            Some(outcome) => {
                println!(
                    "windowed: {} realized, {} blocked | anchor F{:03} over {} stored hour(s) | compute {} ms",
                    outcome.rendered.len(),
                    outcome.blocked.len(),
                    outcome.anchor_hour,
                    outcome.stored_hours,
                    outcome.compute_ms,
                );
                for product in &outcome.rendered {
                    println!(
                        "{:>8} ms  {}  {}",
                        product.total_ms,
                        product.slug,
                        product.output_path.display()
                    );
                    timings.push((product.slug.clone(), product.total_ms));
                }
                skipped.extend(outcome.blocked);
            }
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
}

//! Live GRIB -> `.rws` store ingest for HRRR-class models: the serial
//! per-hour CLI over the shared [`ingest_hour`] flow (see
//! `src/ingest_hour.rs` for the full fetch/extract/derive/write story —
//! `rw_batch` pipelines the same flow across hours).

use std::path::PathBuf;

use clap::Parser;
use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::cache::{default_proof_cache_dir, ensure_dir};

#[path = "../ingest_hour.rs"]
mod ingest_hour;
#[path = "../throttle.rs"]
mod throttle;
use ingest_hour::{IngestConfig, cache_state, parse_hours};

/// The derived CAPE kernels allocate per-column scratch across every rayon
/// thread; mimalloc handles that churn better than the default Windows heap
/// (measured ~10% on the derived stage and ~15% on GRIB extraction).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Debug, Parser)]
#[command(
    name = "rw-ingest",
    about = "Ingest live model GRIB output into the per-hour .rws store"
)]
struct Args {
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run date as YYYYMMDD")]
    date: String,
    #[arg(long, help = "Run cycle hour UTC (0-23)")]
    cycle: u8,
    #[arg(long, help = "Forecast hours: \"0\", \"0,6,12\", or \"0-6\"")]
    hours: String,
    #[arg(long)]
    source: Option<SourceId>,
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_cache: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "After each write, re-open the hour and verify a 2D round-trip and one profile per 3D variable"
    )]
    verify: bool,
    #[arg(
        long,
        default_value_t = false,
        conflicts_with = "no_heavy",
        help = "Run the heavy ECAPE ingest stage (the default; present so callers can be explicit)"
    )]
    heavy: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Skip the heavy ECAPE ingest stage: the 16 heavy grids are not stored (derived 29 still are)"
    )]
    no_heavy: bool,
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

/// Resolve the `--heavy` / `--no-heavy` pair: heavy is ON unless
/// `--no-heavy` is passed (the flags conflict, so both set is unreachable).
fn heavy_enabled(args: &Args) -> bool {
    !args.no_heavy
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Scheduling policy must land before anything touches rayon (the
    // global pool is built lazily on first use and cannot be resized).
    throttle::apply(args.threads, args.full_throttle);
    run(&args)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let hours = parse_hours(&args.hours)?;
    let cache_root = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| default_proof_cache_dir(std::path::Path::new("out")));
    if !args.no_cache {
        ensure_dir(&cache_root)?;
    }
    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let cycle = CycleSpec::new(args.date.clone(), args.cycle)?;
    let run_slug = format!("{}_{:02}z", args.date, args.cycle);
    let model_slug = args.model.as_str().replace('-', "_");

    println!(
        "rw_ingest build {} | model {} run {} | source {} | store {} | cache {}",
        env!("RW_BUILD_SHA"),
        model_slug,
        run_slug,
        source,
        args.store_root.display(),
        cache_root.display(),
    );
    let config = IngestConfig {
        model: args.model,
        cycle: &cycle,
        source_override: Some(source),
        cache_root: &cache_root,
        use_cache: !args.no_cache,
        store_root: &args.store_root,
        model_slug: &model_slug,
        run_slug: &run_slug,
        heavy: heavy_enabled(args),
        verify: args.verify,
    };
    for &hour in &hours {
        let ingested = ingest_hour::ingest_hour(&config, hour)?;
        println!(
            "f{hour:03}: prs fetch {} ms ({}, {:.1} MB) | sfc fetch {} ms ({}, {:.1} MB) | extract prs {} ms, sfc {} ms | thermo decode {} ms | derived {} ms | heavy {} ms | encode {} ms | total {} ms | {} {:.1} MB | 2d {}/{} | derived {}/{} | heavy {}/{} | 3d {}",
            ingested.prs_fetch_ms,
            cache_state(ingested.prs_cache_hit),
            ingested.prs_mb,
            ingested.sfc_fetch_ms,
            cache_state(ingested.sfc_cache_hit),
            ingested.sfc_mb,
            ingested.prs_extract_ms,
            ingested.sfc_extract_ms,
            ingested.thermo_decode_ms,
            ingested.derived_ms,
            ingested.heavy_ms,
            ingested.encode_ms,
            ingested.total_ms(),
            ingested.store_path.display(),
            ingested.store_mb,
            ingested.fields_2d,
            ingested.planned_2d,
            ingested.derived,
            ingested.planned_derived,
            ingested.heavy,
            ingested.planned_heavy,
            ingested
                .volumes
                .iter()
                .map(|volume| format!("{}:{}", volume.name, volume.levels.len()))
                .collect::<Vec<_>>()
                .join(" "),
        );
        for volume in &ingested.volumes {
            println!("  {} levels (hPa): {:?}", volume.name, volume.levels);
        }
    }
    println!(
        "{}",
        args.store_root
            .join(&model_slug)
            .join(&run_slug)
            .join("run.json")
            .display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heavy_flag_defaults_on_and_no_heavy_turns_it_off() {
        let base = [
            "rw-ingest",
            "--date",
            "20260608",
            "--cycle",
            "0",
            "--hours",
            "6",
        ];
        let args = Args::try_parse_from(base).expect("default args parse");
        assert!(heavy_enabled(&args), "heavy must default ON");

        let explicit =
            Args::try_parse_from(base.iter().copied().chain(["--heavy"])).expect("--heavy parses");
        assert!(heavy_enabled(&explicit));

        let off = Args::try_parse_from(base.iter().copied().chain(["--no-heavy"]))
            .expect("--no-heavy parses");
        assert!(!heavy_enabled(&off), "--no-heavy must gate the stage off");

        assert!(
            Args::try_parse_from(base.iter().copied().chain(["--heavy", "--no-heavy"])).is_err(),
            "--heavy and --no-heavy must conflict"
        );
    }
}

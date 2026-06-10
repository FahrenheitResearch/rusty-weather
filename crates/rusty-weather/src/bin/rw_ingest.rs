//! Live GRIB -> `.rws` store ingest for HRRR-class models: the serial
//! per-hour CLI over the shared [`ingest_hour`] flow (see
//! `src/ingest_hour.rs` for the full fetch/extract/derive/write story —
//! `rw_batch` pipelines the same flow across hours).
//!
//! Profiles (`--profile full|sounding|view` plus the composable
//! `--level-step/--no-derived/--heavy/--no-heavy` overrides) choose what
//! each hour fetches, extracts, computes, and stores; `--estimate` prices
//! the planned ingest (per-variable breakdown + store/download totals)
//! without fetching or writing anything.

use std::path::PathBuf;

use clap::Parser;
use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::cache::{default_proof_cache_dir, ensure_dir};

#[path = "../ingest_hour.rs"]
mod ingest_hour;
#[path = "../throttle.rs"]
mod throttle;
use ingest_hour::ingest_profile::{IngestProfile, ProfileOverrides, resolve_profile};
use ingest_hour::size_estimate::{Calibration, estimate};
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
        default_value = "full",
        help = "Ingest profile: full (everything, today's default), sounding (5 volumes + 7 \
                surface fields, no compute stages), view (all 2D incl. derived, no volumes)"
    )]
    profile: String,
    #[arg(
        long,
        help = "Override the isobaric level step in hPa: 25 (37 levels) or 50 (19 levels)"
    )]
    level_step: Option<u16>,
    #[arg(
        long,
        default_value_t = false,
        help = "Skip the derived compute stage (requires --no-heavy: the heavy stage builds on it)"
    )]
    no_derived: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Print the size estimate for the planned ingest (per-variable breakdown + \
                store/download totals) and exit without fetching or writing"
    )]
    estimate: bool,
    #[arg(
        long,
        help = "Calibrate the estimate from this hour file or run directory (default: the \
                newest stored hours of the same model under --store-root, else built-in \
                defaults measured from the 20260608 00z HRRR store)"
    )]
    calibrate_from: Option<PathBuf>,
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
        help = "Run the heavy ECAPE ingest stage (the full-profile default; present so callers can be explicit)"
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

/// Resolve `--profile` + the override flags into a validated profile.
fn profile_from_args(args: &Args) -> Result<IngestProfile, String> {
    let overrides = ProfileOverrides {
        level_step_hpa: args.level_step,
        no_derived: args.no_derived,
        heavy: if args.heavy {
            Some(true)
        } else if args.no_heavy {
            Some(false)
        } else {
            None
        },
    };
    resolve_profile(&args.profile, &overrides)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    // Scheduling policy must land before anything touches rayon (the
    // global pool is built lazily on first use and cannot be resized).
    throttle::apply(args.threads, args.full_throttle);
    run(&args)
}

/// Calibration hour files for `--estimate`: an explicit `--calibrate-from`
/// hour file or run directory, else the newest stored hours (up to 3) of
/// the same model under the store root, else none (built-in defaults).
fn calibration_paths(args: &Args, model_slug: &str) -> Vec<PathBuf> {
    let mut hour_files: Vec<PathBuf> = Vec::new();
    if let Some(from) = &args.calibrate_from {
        if from.is_file() {
            return vec![from.clone()];
        }
        if let Ok(entries) = std::fs::read_dir(from) {
            hour_files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                path.extension().is_some_and(|ext| ext == "rws")
            }));
        }
        hour_files.sort();
        return hour_files;
    }
    let model_dir = args.store_root.join(model_slug);
    let Ok(runs) = std::fs::read_dir(&model_dir) else {
        return Vec::new();
    };
    for run in runs.flatten() {
        if let Ok(entries) = std::fs::read_dir(run.path()) {
            hour_files.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
                path.extension().is_some_and(|ext| ext == "rws")
            }));
        }
    }
    // Newest first by modification time; the 3 newest bound the walk cost.
    hour_files.sort_by_key(|path| {
        std::cmp::Reverse(
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok(),
        )
    });
    hour_files.truncate(3);
    hour_files
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

/// `--estimate`: price the planned ingest and exit. No fetch, no write.
fn print_estimate(
    args: &Args,
    profile: &IngestProfile,
    hour_count: u16,
    model_slug: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let paths = calibration_paths(args, model_slug);
    let calibration = if paths.is_empty() {
        Calibration::builtin_default()
    } else {
        match Calibration::from_hour_files(&paths) {
            Ok(calibration) => calibration,
            Err(err) => {
                eprintln!("calibration from stored hours failed ({err}); using built-in defaults");
                Calibration::builtin_default()
            }
        }
    };
    let estimate = estimate(profile, args.model, hour_count, &calibration);

    println!("estimate: profile {} ({})", args.profile, profile.describe());
    println!("calibration: {}", calibration.source);
    println!();
    println!("{:<36} {:>12}", "variable (per hour)", "compressed");
    for (name, bytes) in &estimate.breakdown {
        println!("{name:<36} {:>9.2} MB", mb(*bytes));
    }
    println!();
    println!(
        "per hour: store {:.1} MB | download {:.1} MB ({})",
        mb(estimate.per_hour_store_bytes),
        mb(estimate.per_hour_download_bytes),
        if profile.needs_prs() {
            format!(
                "prs {:.1} MB + sfc {:.1} MB, full files",
                mb(calibration.prs_file_bytes),
                mb(calibration.sfc_file_bytes)
            )
        } else {
            format!("sfc {:.1} MB full file only", mb(calibration.sfc_file_bytes))
        },
    );
    println!(
        "total for {hour_count} hour(s): store {:.1} MB (incl. grid.rwg {:.1} MB, once per run) \
         | download {:.1} MB",
        mb(estimate.store_bytes),
        mb(estimate.grid_file_bytes),
        mb(estimate.download_bytes),
    );
    println!(
        "note: downloads price the full prs/sfc family files (cache-cold); an .idx-driven \
         byte-range subset fetch would shrink small profiles and is a future refinement"
    );
    Ok(())
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let profile = profile_from_args(args)?;
    let hours = parse_hours(&args.hours)?;
    let model_slug = args.model.as_str().replace('-', "_");
    if args.estimate {
        return print_estimate(args, &profile, hours.len() as u16, &model_slug);
    }
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

    println!(
        "rw_ingest build {} | model {} run {} | profile {} ({}) | source {} | store {} | cache {}",
        env!("RW_BUILD_SHA"),
        model_slug,
        run_slug,
        args.profile,
        profile.describe(),
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
        profile: &profile,
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

    const BASE: [&str; 7] = [
        "rw-ingest",
        "--date",
        "20260608",
        "--cycle",
        "0",
        "--hours",
        "6",
    ];

    #[test]
    fn default_profile_is_full_with_heavy_on_and_no_heavy_turns_it_off() {
        let args = Args::try_parse_from(BASE).expect("default args parse");
        let profile = profile_from_args(&args).expect("default profile resolves");
        assert_eq!(profile, IngestProfile::full(), "default must be today's behavior");

        let explicit = Args::try_parse_from(BASE.iter().copied().chain(["--heavy"]))
            .expect("--heavy parses");
        assert!(profile_from_args(&explicit).expect("resolves").heavy);

        let off = Args::try_parse_from(BASE.iter().copied().chain(["--no-heavy"]))
            .expect("--no-heavy parses");
        let profile = profile_from_args(&off).expect("resolves");
        assert!(!profile.heavy, "--no-heavy must gate the stage off");
        assert!(profile.derived, "--no-heavy must leave derived on");

        assert!(
            Args::try_parse_from(BASE.iter().copied().chain(["--heavy", "--no-heavy"])).is_err(),
            "--heavy and --no-heavy must conflict"
        );
    }

    #[test]
    fn profile_flags_compose() {
        let args = Args::try_parse_from(
            BASE.iter()
                .copied()
                .chain(["--profile", "sounding", "--level-step", "50"]),
        )
        .expect("sounding @ 50 parses");
        let profile = profile_from_args(&args).expect("resolves");
        assert_eq!(profile.level_step_hpa, 50);
        assert!(!profile.derived && !profile.heavy);

        let args = Args::try_parse_from(
            BASE.iter()
                .copied()
                .chain(["--profile", "sounding", "--heavy"]),
        )
        .expect("parses; validation rejects");
        let message = profile_from_args(&args).unwrap_err();
        assert!(message.contains("named surface subset"), "got: {message}");

        let args = Args::try_parse_from(BASE.iter().copied().chain(["--profile", "view"]))
            .expect("view parses");
        let profile = profile_from_args(&args).expect("resolves");
        assert!(profile.volumes.is_empty() && profile.derived && !profile.heavy);

        let args = Args::try_parse_from(BASE.iter().copied().chain(["--profile", "bogus"]))
            .expect("parses; validation rejects");
        assert!(profile_from_args(&args).unwrap_err().contains("unknown preset"));
    }

    #[test]
    fn estimate_flag_parses_with_profile_flags() {
        let args = Args::try_parse_from(BASE.iter().copied().chain([
            "--estimate",
            "--profile",
            "view",
        ]))
        .expect("--estimate parses");
        assert!(args.estimate);
        assert_eq!(args.profile, "view");
    }
}

//! `rw_batch` — the one-command orchestrated pipeline: live GRIB -> `.rws`
//! store -> every product PNG, for a range of forecast hours, in ONE
//! invocation. This is Plan 3's acceptance bin: its batch manifest carries
//! THE 3-hour all-products wall-clock.
//!
//! Pipeline shape (small std::thread pipeline, NOT per-stage rayon pools):
//!
//! ```text
//! [fetch thread]      hour N+1: prs+sfc download / cache read   (network)
//!       | sync_channel(1) of FetchedHour (raw bytes)
//! [ingest thread]     hour N:   extract -> derived -> heavy -> encode/write
//!       | channel of IngestedHour (store stats + stage walls)
//! [main thread]       hour N-1: render all per-hour products from the store
//! ```
//!
//! The fetch thread overlaps network with compute; the two CPU stages
//! (ingest-side extraction/derive and main-side render) both submit work to
//! the ONE global rayon pool and self-schedule against each other.
//! Windowed products run after the last hour lands (they need every hour),
//! through the same shared `render_all`/`windowed_store` flow `rw_render`
//! uses. Per-stage wall + CPU timings per hour land in
//! `<out-dir>/batch_manifest.json` (schema `rw-batch-manifest-v1`).
//!
//! CPU accounting honesty: `*_cpu_ms` values are process-wide CPU-time
//! deltas (GetProcessTimes) over each stage's wall interval. Pipeline
//! stages overlap across threads, so per-stage CPU attribution is
//! approximate; `totals.process_cpu_ms` is exact.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

#[path = "../contour_mode.rs"]
mod contour_mode;
#[path = "../region.rs"]
mod region;
#[path = "../render_all.rs"]
mod render_all;

use clap::{Parser, ValueEnum};
use contour_mode::ContourModeArg;
use region::RegionPreset;
use render_all::{StoreFieldSource, StoreRenderConfig, StoreRenderSkip};
use rustwx_core::{CycleSpec, ModelId, SourceId};
use rustwx_models::model_summary;
use rustwx_products::cache::{default_proof_cache_dir, ensure_dir};
use rustwx_products::places::{PlaceLabelDensityTier, default_place_label_overlay_for_domain};
use rustwx_products::shared_context::DomainSpec;
use rw_ingest::ingest_profile::{IngestProfile, ProfileOverrides, resolve_profile};
use rw_ingest::throttle;
use rw_ingest::{
    FetchedHour, IngestConfig, IngestedHour, NEVER_CANCEL, cache_state, parse_hours, print_event,
};

/// The derived CAPE kernels allocate per-column scratch across every rayon
/// thread; mimalloc handles that churn better than the default Windows heap
/// (measured ~10% on the derived stage and ~15% on GRIB extraction).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Decommit freed segments immediately instead of after mimalloc's default
/// 10 ms batched purge delay — see the matching helper in `rw_ingest.rs`:
/// purge lag inflated the measured ingest peak working set ~1.3 GB above
/// the live set at identical wall time.
fn disable_mimalloc_purge_delay() {
    /// `mi_option_purge_delay` in mimalloc's option enum (libmimalloc-sys
    /// 0.1.49 exports the neighbors: eager_commit_delay = 14,
    /// use_numa_nodes = 16).
    const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
}

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
    name = "rw-batch",
    about = "One command: fetch -> ingest -> render every product for a range of forecast hours, pipelined"
)]
struct Args {
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run date as YYYYMMDD")]
    date: String,
    #[arg(long, help = "Run cycle hour UTC (0-23)")]
    cycle: u8,
    #[arg(long, help = "Forecast hours: \"4\", \"4,5,6\", or \"4-6\"")]
    hours: String,
    #[arg(
        long,
        help = "Pin one fetch source; default tries every configured source in catalog order \
                (which also lets a warm raw-byte cache hit regardless of original source)"
    )]
    source: Option<SourceId>,
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_cache: bool,
    #[arg(long, default_value = "out/rw_batch")]
    out_dir: PathBuf,
    #[arg(
        long,
        default_value = "all",
        help = "all | direct | derived | heavy | windowed | comma-separated product slugs"
    )]
    products: String,
    #[arg(long, value_enum, default_value_t = RegionPreset::Midwest)]
    region: RegionPreset,
    #[arg(
        long,
        default_value = "full",
        help = "Ingest profile: full (everything, today's default), sounding (5 volumes + 7 \
                surface fields, no compute stages), view (all 2D incl. derived, no volumes). \
                Products whose store variables a profile excludes skip at render"
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
        conflicts_with = "no_heavy",
        help = "Run the heavy ECAPE ingest stage (the full-profile default; present so callers can be explicit)"
    )]
    heavy: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Skip the heavy ECAPE ingest stage; non-strict product requests drop the 16 heavy slugs"
    )]
    no_heavy: bool,
    #[arg(long, value_enum, default_value_t = ContourModeArg::Automatic)]
    contour_mode: ContourModeArg,
    #[arg(long = "png-compression", value_enum, default_value_t = PngCompressionArg::Fast)]
    png_compression: PngCompressionArg,
    #[arg(long = "place-label-density", default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=3))]
    place_label_density: u8,
    #[arg(
        long,
        default_value_t = false,
        help = "Print one line per rendered product (default: compact per-hour summaries)"
    )]
    list_products: bool,
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

/// Process-wide CPU time (kernel + user) in milliseconds.
#[cfg(windows)]
fn process_cpu_ms() -> u128 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: GetCurrentProcess returns the process pseudo-handle (never
    // fails, never needs closing); GetProcessTimes only fills the four
    // out-params for the calling process.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return 0;
    }
    let hundred_ns =
        |ft: FILETIME| (u128::from(ft.dwHighDateTime) << 32) | u128::from(ft.dwLowDateTime);
    (hundred_ns(kernel) + hundred_ns(user)) / 10_000
}

/// No portable std process-CPU API; the benchmark box is Windows. Off
/// Windows the manifest's cpu_ms fields read 0 (walls stay exact).
#[cfg(not(windows))]
fn process_cpu_ms() -> u128 {
    0
}

fn static_output_dimension(name: &str, fallback: u32) -> u32 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .filter(|value| *value >= 320)
        .unwrap_or(fallback)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    disable_mimalloc_purge_delay();
    let args = Args::parse();
    // Scheduling policy must land before anything touches rayon (the
    // global pool is built lazily on first use and cannot be resized).
    throttle::apply(args.threads, args.full_throttle);
    run(&args)
}

/// One hour's full pipeline record for the manifest.
struct HourReport {
    ingested: IngestedHour,
    fetch_cpu_ms: u128,
    ingest_cpu_ms: u128,
    open_ms: u128,
    render_ms: u128,
    render_cpu_ms: u128,
    rendered: Vec<(String, u128)>,
    skipped: Vec<StoreRenderSkip>,
}

fn ms_distribution(timings: &[u128]) -> (u128, u128, u128) {
    if timings.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = timings.to_vec();
    sorted.sort_unstable();
    (
        sorted[0],
        sorted[sorted.len() / 2],
        sorted[sorted.len() - 1],
    )
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let total_started = Instant::now();
    let total_cpu_started = process_cpu_ms();

    let profile = profile_from_args(args)?;
    let hours = parse_hours(&args.hours)?;
    let mut request = render_all::partition_products(&args.products, args.model)?;
    if !profile.heavy {
        let dropped = request.drop_heavy_unless_strict();
        if dropped > 0 {
            println!(
                "products: dropped {dropped} heavy recipe slug(s) (this profile's ingest stores \
                 no heavy grids; pass them explicitly to force the blocked-product error instead)"
            );
        }
    }
    let cache_root = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| default_proof_cache_dir(std::path::Path::new("out")));
    if !args.no_cache {
        ensure_dir(&cache_root)?;
    }
    ensure_dir(&args.out_dir)?;
    let cycle = CycleSpec::new(args.date.clone(), args.cycle)?;
    let run_slug = format!("{}_{:02}z", args.date, args.cycle);
    let model_slug = args.model.as_str().replace('-', "_");
    // Provenance source for subtitles; the FETCH keeps args.source verbatim
    // (None = try all catalog sources, hitting warm caches from any of them).
    let provenance_source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let domain = DomainSpec::new(args.region.slug(), args.region.bounds());

    println!(
        "rw_batch build {} | model {} run {} | hours {:?} | profile {} ({}) | source {} | store {} | cache {} | out {}",
        env!("RW_BUILD_SHA"),
        model_slug,
        run_slug,
        hours,
        args.profile,
        profile.describe(),
        args.source
            .map(|source| source.to_string())
            .unwrap_or_else(|| "any (catalog order)".to_string()),
        args.store_root.display(),
        cache_root.display(),
        args.out_dir.display(),
    );
    println!(
        "products: {} direct, {} derived/heavy, {} windowed requested",
        request.direct.len(),
        request.derived.len(),
        request.windowed.len(),
    );

    let ingest_config = IngestConfig {
        model: args.model,
        cycle: &cycle,
        source_override: args.source,
        cache_root: &cache_root,
        use_cache: !args.no_cache,
        store_root: &args.store_root,
        model_slug: &model_slug,
        run_slug: &run_slug,
        profile: &profile,
        verify: false,
        progress: &print_event,
        cancel: &NEVER_CANCEL,
    };
    let render_config = StoreRenderConfig {
        model: args.model,
        date_yyyymmdd: args.date.clone(),
        cycle_utc: args.cycle,
        source: provenance_source,
        domain: domain.clone(),
        out_dir: args.out_dir.clone(),
        contour_mode: args.contour_mode.into(),
        native_fill_level_multiplier: 1,
        output_width: static_output_dimension("RUSTWX_STATIC_OUTPUT_WIDTH", 1600),
        output_height: static_output_dimension("RUSTWX_STATIC_OUTPUT_HEIGHT", 900),
        png_compression: args.png_compression.into(),
        place_label_overlay: default_place_label_overlay_for_domain(
            &domain,
            PlaceLabelDensityTier::from_numeric(args.place_label_density),
        ),
    };

    // --- the pipeline: fetch thread -> ingest thread -> render (main) ---
    let pipeline: Result<(Vec<HourReport>, Option<StoreFieldSource>), String> = std::thread::scope(
        |scope| {
            // Raw bytes are ~575 MB/hour warm; capacity 1 bounds resident
            // raw-byte sets to <= 3 (fetching + queued + ingesting).
            let (fetched_tx, fetched_rx) =
                mpsc::sync_channel::<Result<(FetchedHour, u128), String>>(1);
            let (ingested_tx, ingested_rx) =
                mpsc::channel::<Result<(IngestedHour, u128, u128), String>>();

            let fetch_hours = hours.clone();
            let fetch_config = &ingest_config;
            scope.spawn(move || {
                for &hour in &fetch_hours {
                    let cpu_started = process_cpu_ms();
                    match rw_ingest::fetch_hour(fetch_config, hour) {
                        Ok(fetched) => {
                            let fetch_cpu = process_cpu_ms().saturating_sub(cpu_started);
                            // Receiver gone => downstream failed; just stop.
                            if fetched_tx.send(Ok((fetched, fetch_cpu))).is_err() {
                                return;
                            }
                        }
                        Err(err) => {
                            let _ = fetched_tx.send(Err(format!("f{hour:03}: fetch: {err}")));
                            return;
                        }
                    }
                }
            });

            let process_config = &ingest_config;
            scope.spawn(move || {
                while let Ok(message) = fetched_rx.recv() {
                    match message {
                        Ok((fetched, fetch_cpu)) => {
                            let hour = fetched.hour;
                            let cpu_started = process_cpu_ms();
                            match rw_ingest::process_fetched_hour(process_config, fetched) {
                                Ok(ingested) => {
                                    let ingest_cpu = process_cpu_ms().saturating_sub(cpu_started);
                                    if ingested_tx
                                        .send(Ok((ingested, fetch_cpu, ingest_cpu)))
                                        .is_err()
                                    {
                                        return;
                                    }
                                }
                                Err(err) => {
                                    let _ =
                                        ingested_tx.send(Err(format!("f{hour:03}: ingest: {err}")));
                                    return;
                                }
                            }
                        }
                        Err(err) => {
                            let _ = ingested_tx.send(Err(err));
                            return;
                        }
                    }
                }
            });

            // Render on the main thread: hour N renders while hour N+1
            // extracts/derives (ingest thread) and hour N+2 fetches.
            let mut reports = Vec::with_capacity(hours.len());
            let mut last_store: Option<StoreFieldSource> = None;
            for _ in 0..hours.len() {
                let (ingested, fetch_cpu_ms, ingest_cpu_ms) = ingested_rx
                    .recv()
                    .map_err(|_| "pipeline: ingest thread exited without a result".to_string())??;
                let hour = ingested.hour;
                println!(
                    "f{hour:03} ingest: prs fetch {} ms ({}, {:.1} MB) | sfc fetch {} ms ({}, {:.1} MB) | extract prs {} ms, sfc {} ms | thermo decode {} ms | derived {} ms | heavy {} ms | encode {} ms | store {:.1} MB | 2d {}/{} | derived {}/{} | heavy {}/{}",
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
                    ingested.write_ms,
                    ingested.store_mb,
                    ingested.fields_2d,
                    ingested.planned_2d,
                    ingested.derived,
                    ingested.planned_derived,
                    ingested.heavy,
                    ingested.planned_heavy,
                );

                let open_started = Instant::now();
                let store = StoreFieldSource::open(&args.store_root, &model_slug, &run_slug, hour)
                    .map_err(|err| format!("f{hour:03}: open store hour: {err}"))?;
                let open_ms = open_started.elapsed().as_millis();
                let render_started = Instant::now();
                let render_cpu_started = process_cpu_ms();
                let outcome = render_all::render_hour_products(
                    &render_config,
                    &store,
                    hour,
                    &request.direct,
                    &request.derived,
                )
                .map_err(|err| format!("f{hour:03}: render: {err}"))?;
                let render_ms = render_started.elapsed().as_millis();
                let render_cpu_ms = process_cpu_ms().saturating_sub(render_cpu_started);

                if args.list_products {
                    for product in &outcome.rendered {
                        println!(
                            "{:>8} ms  {}  {}",
                            product.total_ms,
                            product.slug,
                            product.output_path.display()
                        );
                    }
                }
                let per_product: Vec<u128> = outcome
                    .rendered
                    .iter()
                    .map(|product| product.total_ms)
                    .collect();
                let (min_ms, median_ms, max_ms) = ms_distribution(&per_product);
                println!(
                    "f{hour:03} render: {} rendered, {} skipped | open {} ms | per-product ms min {} / median {} / max {} | wall {} ms",
                    outcome.rendered.len(),
                    outcome.skipped.len(),
                    open_ms,
                    min_ms,
                    median_ms,
                    max_ms,
                    render_ms,
                );

                reports.push(HourReport {
                    ingested,
                    fetch_cpu_ms,
                    ingest_cpu_ms,
                    open_ms,
                    render_ms,
                    render_cpu_ms,
                    rendered: outcome
                        .rendered
                        .into_iter()
                        .map(|product| (product.slug, product.total_ms))
                        .collect(),
                    skipped: outcome.skipped,
                });
                last_store = Some(store);
            }
            Ok((reports, last_store))
        },
    );
    let (reports, last_store) = pipeline?;

    // --- windowed products: after the last hour lands (they need all) ---
    let mut windowed_summary = serde_json::Value::Null;
    let mut windowed_rendered = 0usize;
    let mut windowed_blocked: Vec<StoreRenderSkip> = Vec::new();
    let mut windowed_ms = 0u128;
    if !request.windowed.is_empty() {
        let store = last_store
            .as_ref()
            .ok_or("windowed render needs at least one ingested hour")?;
        let windowed_started = Instant::now();
        let windowed_cpu_started = process_cpu_ms();
        match render_all::render_windowed_products(
            &render_config,
            store,
            &args.store_root,
            &model_slug,
            &run_slug,
            &request.windowed,
            request.windowed_auto,
        )? {
            None => println!(
                "windowed: skipped (single stored hour; 'all' includes windowed products \
                 only when more than one hour is stored)"
            ),
            Some(outcome) => {
                windowed_ms = windowed_started.elapsed().as_millis();
                let windowed_cpu_ms = process_cpu_ms().saturating_sub(windowed_cpu_started);
                println!(
                    "windowed: {} realized, {} blocked | anchor F{:03} over {} stored hour(s) | compute {} ms | wall {} ms",
                    outcome.rendered.len(),
                    outcome.blocked.len(),
                    outcome.anchor_hour,
                    outcome.stored_hours,
                    outcome.compute_ms,
                    windowed_ms,
                );
                if args.list_products {
                    for product in &outcome.rendered {
                        println!(
                            "{:>8} ms  {}  {}",
                            product.total_ms,
                            product.slug,
                            product.output_path.display()
                        );
                    }
                }
                windowed_rendered = outcome.rendered.len();
                windowed_summary = serde_json::json!({
                    "anchor_hour": outcome.anchor_hour,
                    "stored_hours": outcome.stored_hours,
                    "compute_ms": outcome.compute_ms,
                    "wall_ms": windowed_ms,
                    "cpu_ms": windowed_cpu_ms,
                    "rendered": outcome.rendered.iter().map(|product| serde_json::json!({
                        "slug": product.slug,
                        "ms": product.total_ms,
                    })).collect::<Vec<_>>(),
                    "blocked": outcome.blocked.iter().map(|skip| serde_json::json!({
                        "slug": skip.slug,
                        "reason": skip.reason,
                    })).collect::<Vec<_>>(),
                });
                windowed_blocked = outcome.blocked;
            }
        }
    }

    // --- totals + manifest ---
    let total_wall_ms = total_started.elapsed().as_millis();
    let total_cpu_ms = process_cpu_ms().saturating_sub(total_cpu_started);
    let sum = |field: fn(&HourReport) -> u128| -> u128 { reports.iter().map(field).sum() };
    let fetch_total = sum(|report| report.ingested.prs_fetch_ms + report.ingested.sfc_fetch_ms);
    let extract_total =
        sum(|report| report.ingested.prs_extract_ms + report.ingested.sfc_extract_ms);
    let thermo_total = sum(|report| report.ingested.thermo_decode_ms);
    let derived_total = sum(|report| report.ingested.derived_ms);
    let heavy_total = sum(|report| report.ingested.heavy_ms);
    let encode_total = sum(|report| report.ingested.write_ms);
    let render_total = sum(|report| report.render_ms);
    let rendered_total: usize = reports
        .iter()
        .map(|report| report.rendered.len())
        .sum::<usize>()
        + windowed_rendered;
    let skipped_total: usize = reports
        .iter()
        .map(|report| report.skipped.len())
        .sum::<usize>()
        + windowed_blocked.len();

    println!("per-hour stage walls (ms):");
    println!("  hour   fetch  extract  thermo  derived   heavy  encode  render");
    for report in &reports {
        println!(
            "  f{:03} {:>7} {:>8} {:>7} {:>8} {:>7} {:>7} {:>7}",
            report.ingested.hour,
            report.ingested.prs_fetch_ms + report.ingested.sfc_fetch_ms,
            report.ingested.prs_extract_ms + report.ingested.sfc_extract_ms,
            report.ingested.thermo_decode_ms,
            report.ingested.derived_ms,
            report.ingested.heavy_ms,
            report.ingested.write_ms,
            report.render_ms,
        );
    }
    println!(
        "stage totals (ms): fetch {fetch_total} | extract {extract_total} | thermo {thermo_total} | derived {derived_total} | heavy {heavy_total} | encode {encode_total} | render {render_total} | windowed {windowed_ms}"
    );
    println!(
        "TOTAL: {rendered_total} products rendered ({skipped_total} skipped/blocked) | wall {total_wall_ms} ms | process cpu {total_cpu_ms} ms"
    );

    let manifest = serde_json::json!({
        "schema": "rw-batch-manifest-v1",
        "build": env!("RW_BUILD_SHA"),
        "model": model_slug,
        "run": run_slug,
        "hours": hours,
        "profile": args.profile,
        "profile_detail": profile.describe(),
        "heavy": profile.heavy,
        "products_spec": args.products,
        "region": domain.slug,
        "full_throttle": args.full_throttle,
        "scheduling_note": if args.full_throttle {
            "full throttle: normal priority, every core"
        } else {
            "polite default: below-normal process priority and a cores-2 rayon pool; \
             wall-clock slightly overestimates a dedicated-node run"
        },
        "cpu_attribution_note": "cpu_ms values are process-wide CPU-time deltas over each \
            stage's wall interval; pipeline stages overlap across threads, so per-stage CPU \
            is approximate while totals.process_cpu_ms is exact",
        "per_hour": reports.iter().map(|report| {
            let ingested = &report.ingested;
            let per_product: Vec<u128> =
                report.rendered.iter().map(|(_, ms)| *ms).collect();
            let (min_ms, median_ms, max_ms) = ms_distribution(&per_product);
            serde_json::json!({
                "hour": ingested.hour,
                "fetch": {
                    "wall_ms": ingested.prs_fetch_ms + ingested.sfc_fetch_ms,
                    "cpu_ms": report.fetch_cpu_ms,
                    "prs_ms": ingested.prs_fetch_ms,
                    "sfc_ms": ingested.sfc_fetch_ms,
                    "prs_cache_hit": ingested.prs_cache_hit,
                    "sfc_cache_hit": ingested.sfc_cache_hit,
                    "prs_mb": ingested.prs_mb,
                    "sfc_mb": ingested.sfc_mb,
                },
                "extract": {
                    "wall_ms": ingested.prs_extract_ms + ingested.sfc_extract_ms,
                    "prs_ms": ingested.prs_extract_ms,
                    "sfc_ms": ingested.sfc_extract_ms,
                },
                "thermo_decode_ms": ingested.thermo_decode_ms,
                "derived_ms": ingested.derived_ms,
                "heavy_ms": ingested.heavy_ms,
                "encode": {
                    "wall_ms": ingested.write_ms,
                    "codec_ms": ingested.encode_ms,
                },
                "ingest_cpu_ms": report.ingest_cpu_ms,
                "store_mb": ingested.store_mb,
                "counts": {
                    "fields_2d": format!("{}/{}", ingested.fields_2d, ingested.planned_2d),
                    "derived": format!("{}/{}", ingested.derived, ingested.planned_derived),
                    "heavy": format!("{}/{}", ingested.heavy, ingested.planned_heavy),
                },
                "render": {
                    "wall_ms": report.render_ms,
                    "cpu_ms": report.render_cpu_ms,
                    "open_ms": report.open_ms,
                    "rendered": report.rendered.len(),
                    "skipped": report.skipped.iter().map(|skip| serde_json::json!({
                        "slug": skip.slug,
                        "reason": skip.reason,
                    })).collect::<Vec<_>>(),
                    "per_product_ms": {
                        "min": min_ms, "median": median_ms, "max": max_ms,
                    },
                },
            })
        }).collect::<Vec<_>>(),
        "windowed": windowed_summary,
        "totals": {
            "wall_ms": total_wall_ms,
            "process_cpu_ms": total_cpu_ms,
            "fetch_ms": fetch_total,
            "extract_ms": extract_total,
            "thermo_decode_ms": thermo_total,
            "derived_ms": derived_total,
            "heavy_ms": heavy_total,
            "encode_ms": encode_total,
            "render_ms": render_total,
            "windowed_ms": windowed_ms,
            "products_rendered": rendered_total,
            "products_skipped_or_blocked": skipped_total,
        },
    });
    let manifest_path = args.out_dir.join("batch_manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .map_err(|err| format!("write {}: {err}", manifest_path.display()))?;
    println!("manifest: {}", manifest_path.display());

    // No silent caps: every per-hour skip and windowed blocker is listed.
    let mut all_skips: Vec<(String, &StoreRenderSkip)> = Vec::new();
    for report in &reports {
        for skip in &report.skipped {
            all_skips.push((format!("f{:03}", report.ingested.hour), skip));
        }
    }
    for skip in &windowed_blocked {
        all_skips.push(("windowed".to_string(), skip));
    }
    if !all_skips.is_empty() {
        eprintln!("products not rendered ({}):", all_skips.len());
        for (scope_label, skip) in &all_skips {
            eprintln!("  [{scope_label}] {}: {}", skip.slug, skip.reason);
        }
        if request.strict {
            return Err(format!(
                "{} explicitly requested product(s) could not render",
                all_skips.len()
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
    fn default_profile_is_full_and_heavy_flags_map_onto_it() {
        let base = [
            "rw-batch", "--date", "20260608", "--cycle", "0", "--hours", "4-6",
        ];
        let args = Args::try_parse_from(base).expect("default args parse");
        let profile = profile_from_args(&args).expect("default profile resolves");
        assert_eq!(
            profile,
            IngestProfile::full(),
            "default must be today's behavior (heavy ON)"
        );
        let off = Args::try_parse_from(base.iter().copied().chain(["--no-heavy"]))
            .expect("--no-heavy parses");
        assert!(!profile_from_args(&off).expect("resolves").heavy);
        assert!(
            Args::try_parse_from(base.iter().copied().chain(["--heavy", "--no-heavy"])).is_err(),
            "--heavy and --no-heavy must conflict"
        );
        let sounding = Args::try_parse_from(base.iter().copied().chain([
            "--profile",
            "sounding",
            "--level-step",
            "50",
        ]))
        .expect("sounding @ 50 parses");
        let profile = profile_from_args(&sounding).expect("resolves");
        assert_eq!(profile.level_step_hpa, 50);
        assert!(!profile.derived && !profile.heavy);
    }

    #[test]
    fn ms_distribution_handles_empty_and_orders() {
        assert_eq!(ms_distribution(&[]), (0, 0, 0));
        assert_eq!(ms_distribution(&[5]), (5, 5, 5));
        assert_eq!(ms_distribution(&[9, 1, 5]), (1, 5, 9));
    }

    #[test]
    fn process_cpu_time_is_monotonic() {
        let before = process_cpu_ms();
        // Burn a little CPU so the counter can only move forward.
        let mut acc = 0u64;
        for i in 0..2_000_000u64 {
            acc = acc.wrapping_add(i.wrapping_mul(31));
        }
        assert!(acc != 1, "keep the loop alive");
        assert!(process_cpu_ms() >= before);
    }
}

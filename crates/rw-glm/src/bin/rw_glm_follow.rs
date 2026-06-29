//! `rw_glm_follow` — the thin ops CLI for the GLM lightning follow engine.
//!
//! Three subcommands:
//!
//! - `follow`: poll the live GOES GLM-L2-LCFA bucket for `--satellite`,
//!   ingesting flashes into the rolling `.rwl` store under `--store-root`,
//!   then (unless `--no-validate`) run a post-run report — Deep-validate every
//!   bucket, scan the whole window with [`rw_glm::read_flashes`], and run a
//!   CONUS bbox query — printing live numbers (granules, flashes/min, bytes per
//!   bucket, holdbacks/skips). `--duration-mins` bounds the run by wall time;
//!   `--window-mins` and `--byte-budget-mb` set the rolling window so eviction
//!   can be observed live.
//!
//! - `list`: a one-shot listing sanity check — print the first N keys under the
//!   current (or `--hour-back`) GLM hour prefix for a satellite. Used for the
//!   G18 cross-check (the follow engine only targets one satellite at a time).
//!
//! - `density`: derive a regular lat/lon flash-density grid from the `.rwl`
//!   store for a recent or explicit time window.
//!
//! - `mtg-li-ingest`: ingest an unpacked MTG Lightning Imager L2 flash body
//!   NetCDF into the same `.rwl` store.
//!
//! This is a deliberately thin front end: all behaviour lives in the library
//! ([`rw_glm::follow_live`], [`rw_glm::read_flashes`],
//! [`rw_glm::validate_bucket_file`]). It mirrors `rw-sat`'s `rw_sat` bin.

use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};

use rw_glm::follow::{GlmEvent, SkipReason};
use rw_glm::s3::{
    bucket_for_satellite, build_agent, glm_hour_prefix, list_s3_objects, object_filename,
};
use rw_glm::{
    BBox, BucketWriter, DensityBounds, FlashDensityRequest, FlashRecord, GlmFollowSpec,
    ValidateDepth, WindowManifest, decode_mtg_li_flashes, flash_density, follow_live, read_flashes,
    validate_bucket_file,
};

#[derive(Parser)]
#[command(
    name = "rw_glm_follow",
    about = "GOES GLM lightning live follow into the rw-glm .rwl rolling store"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Poll the live bucket continuously and ingest flashes as they land.
    Follow(FollowArgs),
    /// List the first few granule keys under a satellite's current GLM hour
    /// prefix (a one-shot bucket sanity check, e.g. the G18 cross-check).
    List(ListArgs),
    /// Derive a flash-density grid from an existing `.rwl` store.
    Density(DensityArgs),
    /// Ingest unpacked MTG Lightning Imager flash body NetCDF into `.rwl`.
    MtgLiIngest(MtgLiIngestArgs),
}

#[derive(Args)]
struct FollowArgs {
    /// Satellite: goes19 (East), goes18 (West), goes16 (legacy).
    #[arg(long, default_value = "goes19")]
    satellite: String,
    /// Store root; buckets land under `<root>/glm/<satellite>/<YYYYMMDD>/`.
    #[arg(long, default_value = "out/glm_store")]
    store_root: PathBuf,
    /// Rolling window in minutes (age eviction). Default 120 (2 h).
    #[arg(long, default_value_t = 120)]
    window_mins: u64,
    /// Poll cadence in seconds. Default 20 (GLM granule cadence).
    #[arg(long, default_value_t = 20)]
    poll_secs: u64,
    /// Stop after this many minutes of wall time (omit to run until killed).
    #[arg(long)]
    duration_mins: Option<u64>,
    /// Optional total byte budget (MB) for the rolling window (oldest-first
    /// eviction once exceeded).
    #[arg(long)]
    byte_budget_mb: Option<u64>,
    /// Skip the post-run validation/reader report.
    #[arg(long, default_value_t = false)]
    no_validate: bool,
}

#[derive(Args)]
struct ListArgs {
    /// Satellite to list (goes19 / goes18 / ...).
    #[arg(long, default_value = "goes18")]
    satellite: String,
    /// How many keys to print.
    #[arg(long, default_value_t = 3)]
    count: usize,
    /// List this many hours before now (0 = current hour). A just-rolled hour
    /// can be briefly empty; 1 falls back to the previous hour.
    #[arg(long, default_value_t = 0)]
    hour_back: i64,
}

#[derive(Args)]
struct DensityArgs {
    /// Satellite store to read (goes19 / goes18 / future MTG LI slug).
    #[arg(long, default_value = "goes19")]
    satellite: String,
    /// Store root; reads `<root>/glm/<satellite>/<YYYYMMDD>/tHHMM.rwl`.
    #[arg(long, default_value = "out/glm_store")]
    store_root: PathBuf,
    /// Window end, Unix milliseconds. Defaults to now.
    #[arg(long)]
    end_unix_ms: Option<i64>,
    /// Window start, Unix milliseconds. Defaults to `end - --minutes`.
    #[arg(long)]
    start_unix_ms: Option<i64>,
    /// Lookback window in minutes when `--start-unix-ms` is omitted.
    #[arg(long, default_value_t = 60)]
    minutes: u64,
    /// Southern latitude bound.
    #[arg(long, default_value_t = 24.0)]
    south: f32,
    /// Northern latitude bound.
    #[arg(long, default_value_t = 50.0)]
    north: f32,
    /// Western longitude bound.
    #[arg(long, default_value_t = -125.0)]
    west: f32,
    /// Eastern longitude bound.
    #[arg(long, default_value_t = -66.0)]
    east: f32,
    /// Grid columns.
    #[arg(long, default_value_t = 118)]
    nx: usize,
    /// Grid rows.
    #[arg(long, default_value_t = 52)]
    ny: usize,
    /// Include flashes marked degraded-quality.
    #[arg(long, default_value_t = false)]
    include_degraded: bool,
    /// Print the whole grid as pretty JSON.
    #[arg(long, default_value_t = false)]
    json: bool,
    /// Write the whole grid as pretty JSON.
    #[arg(long)]
    out_json: Option<PathBuf>,
}

#[derive(Args)]
struct MtgLiIngestArgs {
    /// Unpacked MTG LI L2 flash body NetCDF path(s), usually `...CHK-BODY...nc`.
    #[arg(long)]
    path: Vec<PathBuf>,
    /// Store root; buckets land under `<root>/glm/<satellite>/<YYYYMMDD>/`.
    #[arg(long, default_value = "out/glm_store")]
    store_root: PathBuf,
    /// Satellite/store slug for MTG LI.
    #[arg(long, default_value = "mtg-li")]
    satellite: String,
    /// Reingest even when the product key is already present in window.json.
    #[arg(long, default_value_t = false)]
    force: bool,
    /// Skip structural validation of written buckets.
    #[arg(long, default_value_t = false)]
    no_validate: bool,
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Follow(args) => run_follow(&args),
        Command::List(args) => run_list(&args),
        Command::Density(args) => run_density(&args),
        Command::MtgLiIngest(args) => run_mtg_li_ingest(&args),
    };
    if let Err(err) = result {
        eprintln!("rw_glm_follow: {err}");
        std::process::exit(1);
    }
}

fn run_density(args: &DensityArgs) -> Result<(), Box<dyn Error>> {
    let end = args.end_unix_ms.unwrap_or_else(now_unix_ms);
    let start = args
        .start_unix_ms
        .unwrap_or_else(|| end.saturating_sub(args.minutes.saturating_mul(60_000) as i64));
    let mut request = FlashDensityRequest::new(
        start,
        end,
        DensityBounds::new(args.south, args.north, args.west, args.east),
        args.nx,
        args.ny,
    );
    request.include_degraded = args.include_degraded;

    let grid = flash_density(&args.store_root, &args.satellite, &request)?;
    if let Some(path) = &args.out_json {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&grid)?)?;
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&grid)?);
        return Ok(());
    }

    let hours = (end - start) as f64 / 3_600_000.0;
    let max = grid.max_cell();
    println!(
        "rw_glm_follow density | satellite {} | store {} | window [{start}, {end}) ({hours:.2} h) | grid {}x{} | domain lat {}..{} lon {}..{}",
        args.satellite,
        args.store_root.display(),
        grid.nx,
        grid.ny,
        grid.bounds.south,
        grid.bounds.north,
        grid.bounds.west,
        grid.bounds.east,
    );
    println!(
        "flashes counted {} | units {} | degraded included: {}",
        grid.flash_count, grid.units, args.include_degraded
    );
    if let Some(cell) = max {
        println!(
            "max cell x={} y={} center=({:.2}, {:.2}) count={} value={:.3}",
            cell.x, cell.y, cell.lat_center, cell.lon_center, cell.count, cell.value
        );
    }
    if let Some(path) = &args.out_json {
        println!("wrote {}", path.display());
    }
    Ok(())
}

fn run_mtg_li_ingest(args: &MtgLiIngestArgs) -> Result<(), Box<dyn Error>> {
    if args.path.is_empty() {
        return Err("pass at least one --path pointing at an MTG LI body NetCDF".into());
    }

    let mut writer = BucketWriter::open(&args.store_root, &args.satellite)?;
    let mut seen = writer.load_manifest().seen_granule_keys;
    let mut total_products = 0_usize;
    let mut skipped = 0_usize;
    let mut total_flashes = 0_usize;
    let mut affected = Vec::new();
    let mut time_min = i64::MAX;
    let mut time_max = i64::MIN;

    for path in &args.path {
        let decoded = decode_mtg_li_flashes(path)?;
        if !args.force && seen.iter().any(|key| key == &decoded.product_key) {
            skipped += 1;
            println!(
                "mtg-li-ingest skip {} | already in window.json",
                decoded.product_key
            );
            continue;
        }
        let records: Vec<FlashRecord> = decoded
            .flashes
            .iter()
            .map(|flash| FlashRecord {
                time_unix_ms: flash.time_unix_ms,
                lat: flash.lat,
                lon: flash.lon,
                energy: flash.energy,
                area: flash.area,
                flash_id: flash.flash_id,
                flags: flash.flags,
                duration_ms: flash.duration_ms,
            })
            .collect();
        for record in &records {
            time_min = time_min.min(record.time_unix_ms);
            time_max = time_max.max(record.time_unix_ms);
        }
        affected.extend(writer.affected_bucket_paths(&records));
        writer.insert_flashes(&records, 1)?;
        writer.record_seen_granule(&decoded.product_key)?;
        seen.push(decoded.product_key.clone());
        total_products += 1;
        total_flashes += records.len();
        println!(
            "mtg-li-ingest {} | {} flash(es) | coverage {:?}..{:?}",
            path.display(),
            records.len(),
            decoded.time_coverage_start,
            decoded.time_coverage_end
        );
    }

    affected.sort();
    affected.dedup();
    if !args.no_validate {
        for path in &affected {
            let report = validate_bucket_file(path, ValidateDepth::Structural)?;
            if !report.errors.is_empty() {
                return Err(
                    format!("{} failed validation: {:?}", path.display(), report.errors).into(),
                );
            }
        }
    }

    println!(
        "mtg-li-ingest summary | satellite {} | store {} | products {} skipped {} | flashes {} | buckets {} | time [{}..{}]",
        args.satellite,
        args.store_root.display(),
        total_products,
        skipped,
        total_flashes,
        affected.len(),
        if time_min == i64::MAX { 0 } else { time_min },
        if time_max == i64::MIN { 0 } else { time_max },
    );
    Ok(())
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Counters accumulated from the event stream during a follow run, so the
/// final report can quote live numbers without re-walking the store — and so
/// the cancelled (duration-reached) path, where `follow_live` returns no
/// `FollowSummary`, can still report polls and prunes.
#[derive(Default)]
struct RunStats {
    polls: u32,
    fetched: usize,
    decoded_flashes: usize,
    bucket_writes: usize,
    pruned_buckets: usize,
    holdbacks: usize,
    skips_already_seen: usize,
    skips_permanent: usize,
    skips_exhausted: usize,
    warnings: usize,
}

fn run_follow(args: &FollowArgs) -> Result<(), Box<dyn Error>> {
    let mut spec = GlmFollowSpec::new(&args.satellite, args.store_root.clone());
    spec.poll_secs = args.poll_secs;
    spec.window = Duration::from_secs(args.window_mins.saturating_mul(60));
    spec.byte_budget = args.byte_budget_mb.map(|mb| mb.saturating_mul(1024 * 1024));

    println!(
        "rw_glm_follow follow | satellite {} | store {} | window {} min | poll {} s | byte_budget {} | duration {}",
        args.satellite,
        args.store_root.display(),
        args.window_mins,
        args.poll_secs,
        args.byte_budget_mb
            .map(|mb| format!("{mb} MB"))
            .unwrap_or_else(|| "none".to_string()),
        args.duration_mins
            .map(|m| format!("{m} min"))
            .unwrap_or_else(|| "until killed".to_string()),
    );

    // Bound the run by wall time via a background timer that flips the cancel
    // flag. follow_live observes it at the next poll boundary and returns
    // Cancelled (a clean stop, not an error for us).
    let cancel = Arc::new(AtomicBool::new(false));
    if let Some(mins) = args.duration_mins {
        let deadline = Duration::from_secs(mins.saturating_mul(60));
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(deadline);
            flag.store(true, Ordering::Relaxed);
        });
    }

    let mut stats = RunStats::default();
    let started = Instant::now();
    let mut sink = |event: GlmEvent| {
        accumulate(&mut stats, &event);
        rw_glm::follow::print_event(&event);
    };

    let summary = match follow_live(&spec, &mut sink, &cancel) {
        Ok(summary) => summary,
        Err(err) if err.is_cancelled() => {
            // The duration timer fired (or a Ctrl-C path set the flag): a clean
            // stop. follow_live returns the in-progress work durably written, so
            // we still report. Re-derive the summary fields from our counters.
            println!("(duration reached — stopping)");
            // follow_live returns no summary on cancel; rebuild it from the
            // event counters so the report still quotes real polls/granules/
            // prunes rather than zeros.
            rw_glm::FollowSummary {
                polls: stats.polls,
                ingested_granules: stats.fetched,
                ingested_flashes: stats.decoded_flashes,
                skipped_granules: stats.skips_permanent + stats.skips_exhausted,
                pruned_buckets: stats.pruned_buckets,
            }
        }
        Err(err) => return Err(err.to_string().into()),
    };

    let elapsed = started.elapsed();
    print_run_summary(args, &summary, &stats, elapsed);

    if !args.no_validate {
        validate_pass(&args.store_root, &args.satellite)?;
    }
    Ok(())
}

/// Fold one event into the running counters for the final report.
fn accumulate(stats: &mut RunStats, event: &GlmEvent) {
    match event {
        // One Listing event per poll cycle (the loop emits it at the top).
        GlmEvent::Listing { .. } => stats.polls += 1,
        GlmEvent::GranuleFetched { .. } => stats.fetched += 1,
        GlmEvent::GranuleDecoded { flashes, .. } => stats.decoded_flashes += flashes,
        GlmEvent::BucketWritten { .. } => stats.bucket_writes += 1,
        GlmEvent::Pruned { report } => stats.pruned_buckets += report.removed_buckets,
        GlmEvent::GranuleSkipped { reason, .. } => match reason {
            SkipReason::Holdback { .. } => stats.holdbacks += 1,
            SkipReason::AlreadySeen => stats.skips_already_seen += 1,
            SkipReason::PermanentDecodeError => stats.skips_permanent += 1,
            SkipReason::RetriesExhausted => stats.skips_exhausted += 1,
        },
        GlmEvent::Warning { .. } => stats.warnings += 1,
        _ => {}
    }
}

fn print_run_summary(
    args: &FollowArgs,
    summary: &rw_glm::FollowSummary,
    stats: &RunStats,
    elapsed: Duration,
) {
    let mins = elapsed.as_secs_f64() / 60.0;
    let flashes_per_min = if mins > 0.0 {
        summary.ingested_flashes as f64 / mins
    } else {
        0.0
    };
    println!("\n=== run summary ===");
    println!(
        "elapsed {:.1} min | polls {} | granules ingested {} | flashes ingested {} | flashes/min {:.1}",
        mins, summary.polls, summary.ingested_granules, summary.ingested_flashes, flashes_per_min,
    );
    println!(
        "events: fetched {} | decoded flashes {} | bucket writes {} | holdback waits {} | skips: already-seen {}, permanent {}, exhausted {} | warnings {}",
        stats.fetched,
        stats.decoded_flashes,
        stats.bucket_writes,
        stats.holdbacks,
        stats.skips_already_seen,
        stats.skips_permanent,
        stats.skips_exhausted,
        stats.warnings,
    );
    println!(
        "prune: {} bucket(s) evicted over the run (window {} min{})",
        summary.pruned_buckets,
        args.window_mins,
        args.byte_budget_mb
            .map(|mb| format!(", {mb} MB budget"))
            .unwrap_or_default(),
    );
}

/// Enumerate `<root>/glm/<sat>/<YYYYMMDD>/tHHMM.rwl`, sorted (date, name).
fn bucket_files(sat_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(days) = std::fs::read_dir(sat_dir) else {
        return out;
    };
    for day in days.flatten() {
        let day_path = day.path();
        if !day_path.is_dir() {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(&day_path) {
            for f in files.flatten() {
                let p = f.path();
                if p.extension().and_then(|e| e.to_str()) == Some("rwl") {
                    out.push(p);
                }
            }
        }
    }
    out.sort();
    out
}

/// The post-run validation/reader report: Deep-validate every bucket, scan the
/// whole window, cross-check the count, and run a CONUS bbox query.
fn validate_pass(store_root: &Path, satellite: &str) -> Result<(), Box<dyn Error>> {
    println!("\n=== validation pass ===");
    let sat_dir = store_root.join("glm").join(satellite);
    let buckets = bucket_files(&sat_dir);
    if buckets.is_empty() {
        println!("no buckets on disk (quiet run — nothing to validate)");
        return Ok(());
    }

    // 1) Deep-validate every bucket; report per-bucket ok + records + bytes.
    let mut all_ok = true;
    let mut total_records: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut bucket_min = i64::MAX;
    let mut bucket_max = i64::MIN;
    for path in &buckets {
        let report = validate_bucket_file(path, ValidateDepth::Deep)?;
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        total_bytes += bytes;
        total_records += report.stats.records;
        // Track the on-disk time span from the bucket headers for the reader
        // scan below (covers the whole window with a margin).
        if let Ok(data) = std::fs::read(path) {
            if let Ok(h) = rw_glm::RwlHeader::parse(&data) {
                if h.record_count > 0 {
                    bucket_min = bucket_min.min(h.time_min_unix_ms);
                    bucket_max = bucket_max.max(h.time_max_unix_ms);
                }
            }
        }
        let name = path
            .strip_prefix(&sat_dir)
            .unwrap_or(path)
            .to_string_lossy();
        if report.is_ok() {
            println!(
                "  ok   {name}  ({} records, {} bytes)",
                report.stats.records, bytes
            );
        } else {
            all_ok = false;
            println!("  FAIL {name}  errors: {:?}", report.errors);
        }
        for w in &report.warnings {
            println!("       warn: {w}");
        }
    }
    println!(
        "buckets: {} | total records {} | total bytes {} | every bucket Deep-valid: {}",
        buckets.len(),
        total_records,
        total_bytes,
        if all_ok { "YES" } else { "NO" },
    );

    if bucket_min == i64::MAX {
        println!("all buckets empty (no flashes) — reader checks skipped");
        return Ok(());
    }

    // 2) read_flashes over the full window: count must equal the sum of bucket
    // records (every record falls inside [bucket_min, bucket_max] by
    // construction, so a full-span read returns all of them), and the result
    // must be ascending by time.
    let scan = read_flashes(store_root, satellite, bucket_min, bucket_max + 1, None)?;
    let sorted = scan
        .windows(2)
        .all(|w| w[0].time_unix_ms <= w[1].time_unix_ms);
    println!(
        "read_flashes [{bucket_min}, {}] -> {} flashes | sorted ascending: {} | == sum of bucket records ({}): {}",
        bucket_max + 1,
        scan.len(),
        if sorted { "YES" } else { "NO" },
        total_records,
        if scan.len() as u64 == total_records {
            "YES"
        } else {
            "NO"
        },
    );

    // 3) CONUS-ish bbox query: lat 24..50, lon -125..-66.
    let conus = BBox::new(24.0, 50.0, -125.0, -66.0);
    let in_box = read_flashes(
        store_root,
        satellite,
        bucket_min,
        bucket_max + 1,
        Some(conus),
    )?;
    let pct = if !scan.is_empty() {
        100.0 * in_box.len() as f64 / scan.len() as f64
    } else {
        0.0
    };
    println!(
        "CONUS bbox (lat 24..50, lon -125..-66) -> {} flashes ({:.0}% of the window)",
        in_box.len(),
        pct,
    );

    // 4) window.json manifest snapshot (seen-key dedup state + extent).
    let manifest_path = sat_dir.join("window.json");
    if let Ok(bytes) = std::fs::read(&manifest_path) {
        if let Ok(m) = serde_json::from_slice::<WindowManifest>(&bytes) {
            println!(
                "window.json: extent [{:?}, {:?}] | {} seen granule key(s) persisted",
                m.time_min_unix_ms,
                m.time_max_unix_ms,
                m.seen_granule_keys.len(),
            );
        }
    }

    if !all_ok {
        return Err("one or more buckets failed Deep validation".into());
    }
    Ok(())
}

/// `list`: print the first `count` keys under a satellite's GLM hour prefix.
fn run_list(args: &ListArgs) -> Result<(), Box<dyn Error>> {
    let bucket = bucket_for_satellite(&args.satellite)?;
    let when = now_unix_ms() - args.hour_back.saturating_mul(3_600_000);
    let (year, doy, hour) = utc_year_doy_hour(when);
    let prefix = glm_hour_prefix(year, doy, hour);
    println!(
        "listing s3://{bucket}/{prefix} (satellite {}, {} key(s)):",
        args.satellite, args.count
    );
    let agent = build_agent();
    let objects = list_s3_objects(&agent, &bucket, &prefix, None)?;
    let nc: Vec<_> = objects
        .into_iter()
        .filter(|o| o.key.ends_with(".nc"))
        .collect();
    if nc.is_empty() {
        println!("  (no .nc granules under this prefix — try --hour-back 1)");
        return Ok(());
    }
    for o in nc.iter().take(args.count) {
        println!("  {} ({} bytes)", object_filename(&o.key), o.size_bytes);
    }
    println!("  ... {} granule(s) total under the prefix", nc.len());
    Ok(())
}

/// Civil `(year, day_of_year, hour)` for a Unix-ms instant (no chrono; reuses
/// the format module's date math via the public `date_dir`/`days_from_civil`).
fn utc_year_doy_hour(unix_ms: i64) -> (i64, u32, u32) {
    let day = unix_ms.div_euclid(86_400_000);
    let into_day_ms = unix_ms - day * 86_400_000;
    let hour = (into_day_ms / 3_600_000) as u32;
    let date = rw_glm::date_dir(unix_ms); // "YYYYMMDD"
    let year: i64 = date[0..4].parse().unwrap_or(1970);
    let jan1 = rw_glm::format::days_from_civil(year, 1, 1).unwrap_or(day);
    let doy = (day - jan1 + 1).clamp(1, 366) as u32;
    (year, doy, hour)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cli_parses_follow_and_list() {
        let cli = Cli::try_parse_from([
            "rw_glm_follow",
            "follow",
            "--satellite",
            "goes18",
            "--store-root",
            "out/x",
            "--window-mins",
            "6",
            "--poll-secs",
            "10",
            "--duration-mins",
            "2",
            "--byte-budget-mb",
            "8",
            "--no-validate",
        ])
        .expect("follow args parse");
        match cli.command {
            Command::Follow(a) => {
                assert_eq!(a.satellite, "goes18");
                assert_eq!(a.store_root, PathBuf::from("out/x"));
                assert_eq!(a.window_mins, 6);
                assert_eq!(a.poll_secs, 10);
                assert_eq!(a.duration_mins, Some(2));
                assert_eq!(a.byte_budget_mb, Some(8));
                assert!(a.no_validate);
            }
            _ => panic!("expected follow"),
        }

        let cli = Cli::try_parse_from(["rw_glm_follow", "list", "--satellite", "goes18"])
            .expect("list args parse");
        match cli.command {
            Command::List(a) => {
                assert_eq!(a.satellite, "goes18");
                assert_eq!(a.count, 3);
                assert_eq!(a.hour_back, 0);
            }
            _ => panic!("expected list"),
        }

        let cli = Cli::try_parse_from([
            "rw_glm_follow",
            "density",
            "--satellite",
            "goes19",
            "--minutes",
            "30",
            "--nx",
            "20",
            "--ny",
            "10",
            "--json",
        ])
        .expect("density args parse");
        match cli.command {
            Command::Density(a) => {
                assert_eq!(a.satellite, "goes19");
                assert_eq!(a.minutes, 30);
                assert_eq!(a.nx, 20);
                assert_eq!(a.ny, 10);
                assert!(a.json);
            }
            _ => panic!("expected density"),
        }
    }

    #[test]
    fn follow_defaults_match_the_spec() {
        let cli = Cli::try_parse_from(["rw_glm_follow", "follow"]).expect("defaults parse");
        match cli.command {
            Command::Follow(a) => {
                assert_eq!(a.satellite, "goes19");
                assert_eq!(a.window_mins, 120, "2 h default window");
                assert_eq!(a.poll_secs, 20, "20 s default cadence");
                assert!(a.duration_mins.is_none(), "runs until killed by default");
                assert!(!a.no_validate, "validation on by default");
            }
            _ => panic!("expected follow"),
        }
    }

    #[test]
    fn utc_year_doy_hour_matches_known_instants() {
        // 2026-01-01 00:00:00 UTC -> day-of-year 1, hour 0.
        let base: i64 = 1_767_225_600_000;
        assert_eq!(utc_year_doy_hour(base), (2026, 1, 0));
        // 2026-06-11 10:00 UTC -> day-of-year 162, hour 10 (the live-run hour).
        let jun11_10z = base + ((161 * 86_400) + (10 * 3600)) * 1000;
        assert_eq!(utc_year_doy_hour(jun11_10z), (2026, 162, 10));
    }

    #[test]
    fn accumulate_folds_events_into_counters() {
        let mut s = RunStats::default();
        accumulate(
            &mut s,
            &GlmEvent::GranuleFetched {
                key: "k".into(),
                bytes: 1,
            },
        );
        accumulate(
            &mut s,
            &GlmEvent::GranuleDecoded {
                key: "k".into(),
                flashes: 42,
            },
        );
        accumulate(
            &mut s,
            &GlmEvent::GranuleSkipped {
                key: "k".into(),
                reason: SkipReason::Holdback { retry_in_secs: 20 },
            },
        );
        accumulate(
            &mut s,
            &GlmEvent::GranuleSkipped {
                key: "k".into(),
                reason: SkipReason::AlreadySeen,
            },
        );
        accumulate(
            &mut s,
            &GlmEvent::Warning {
                message: "w".into(),
            },
        );
        assert_eq!(s.fetched, 1);
        assert_eq!(s.decoded_flashes, 42);
        assert_eq!(s.holdbacks, 1);
        assert_eq!(s.skips_already_seen, 1);
        assert_eq!(s.warnings, 1);
    }
}

//! rw_bench — read-path benchmark for the rw-store hour format.
//!
//! Methodology: every metric runs 1 untimed warmup iteration followed by
//! `--samples` timed iterations (std::time::Instant) and reports the MEDIAN
//! plus min/max. `locate_warm` is the exception: a single locate() is too
//! short to time individually, so each timed iteration is 1000 back-to-back
//! calls and the reported value is that iteration's per-call mean. All
//! results pass through std::hint::black_box so the optimizer cannot elide
//! the measured work.
//!
//! Metrics:
//!   open            HourReader::open (mmap + header/meta/index parse + sort
//!                   verify), fresh per iteration
//!   grid_open       GridFile::open (read + sha256 + coord decompress)
//!   locator_build   GridLocator::build from a pre-opened GridFile
//!   locate_warm     locate() on a built locator at mid-CONUS (39.0N 97.5W)
//!   read_full_2d    full-domain decode, per 2D variable, pre-opened reader
//!   window_quarter  read_window_2d over (0..nx/2, 0..ny/2) — 1/4 the area
//!   window_64       read_window_2d over a 64x64 window mid-grid
//!   sounding_cold   GridFile::open + GridLocator::build + HourReader::open
//!                   + locate + read_profile_3d x all 3D vars (worst-case
//!                   first click)
//!   sounding_warm   locate + read_profile_3d x all 3D vars on pre-opened
//!                   handles (steady-state click; the headline number)
//!
//! Gates (Plan 2 spec) checked after the table:
//!   sounding_warm  <= 100 ms hard (<= 25 ms expected)
//!   read_full_2d   <= 150 ms per 2D variable
//!   window_quarter <= 0.35 x the same variable's full-read time + 0.5 ms
//!                  (loose area-scaling check: 1/4 the area must cost about
//!                  1/4 the decode; the absolute allowance covers fixed
//!                  per-call overheads — output allocation, rayon fork-join,
//!                  placement copies — that stop being negligible when the
//!                  full read itself is only a few ms)
//!   locator_build  <= 50 ms (informational target, not a hard gate)

use std::fs;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser;
use rw_store::grid::{GridFile, GridLocator};
use rw_store::reader::HourReader;

#[path = "../ingest_hour.rs"]
mod ingest_hour;
use ingest_hour::size_estimate::walk_hour_sizes;

/// Mid-CONUS sounding click point (central Kansas).
const SOUNDING_LAT: f64 = 39.0;
const SOUNDING_LON: f64 = -97.5;
/// Inner locate() calls per timed iteration of `locate_warm`.
const LOCATE_INNER_CALLS: usize = 1000;

/// Gate thresholds, in seconds.
const GATE_SOUNDING_HARD_S: f64 = 0.100;
const GATE_SOUNDING_EXPECTED_S: f64 = 0.025;
const GATE_FULL_2D_S: f64 = 0.150;
const GATE_WINDOW_QUARTER_RATIO: f64 = 0.35;
/// Absolute allowance for fixed per-call window-read overheads (alloc,
/// rayon fork-join, placement copies), in seconds.
const GATE_WINDOW_QUARTER_OVERHEAD_S: f64 = 0.0005;
const TARGET_LOCATOR_BUILD_S: f64 = 0.050;

#[derive(Debug, Parser)]
#[command(
    name = "rw-bench",
    about = "Benchmark the rw-store read path against one stored hour"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: String,
    #[arg(long, help = "Run slug, e.g. 20260608_00z")]
    run: String,
    #[arg(long, default_value_t = 6)]
    hour: u16,
    #[arg(long, default_value_t = 5, help = "Timed iterations per metric (after 1 warmup)")]
    samples: usize,
}

/// Median/min/max over the timed iterations of one metric, in seconds.
#[derive(Debug, Clone, Copy)]
struct Stats {
    median: f64,
    min: f64,
    max: f64,
}

impl Stats {
    fn from_times(mut times: Vec<f64>) -> Self {
        assert!(!times.is_empty(), "stats need at least one sample");
        times.sort_by(|a, b| a.total_cmp(b));
        let mid = times.len() / 2;
        let median = if times.len() % 2 == 1 {
            times[mid]
        } else {
            (times[mid - 1] + times[mid]) / 2.0
        };
        Self {
            median,
            min: times[0],
            max: *times.last().expect("non-empty"),
        }
    }
}

/// 1 warmup + `samples` timed iterations of `op`; results are black_boxed.
fn bench<T>(samples: usize, mut op: impl FnMut() -> T) -> Stats {
    black_box(op());
    let mut times = Vec::with_capacity(samples);
    for _ in 0..samples {
        let started = Instant::now();
        black_box(op());
        times.push(started.elapsed().as_secs_f64());
    }
    Stats::from_times(times)
}

/// Display unit for one table row.
#[derive(Debug, Clone, Copy)]
enum Unit {
    Millis,
    Micros,
}

fn fmt_time(seconds: f64, unit: Unit) -> String {
    match unit {
        Unit::Millis => format!("{:.2} ms", seconds * 1e3),
        Unit::Micros => format!("{:.1} us", seconds * 1e6),
    }
}

fn mb(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.samples == 0 {
        return Err("--samples must be >= 1".into());
    }
    let run_dir = args.store_root.join(&args.model).join(&args.run);
    let hour_path = run_dir.join(format!("f{:03}.rws", args.hour));
    let grid_path = run_dir.join("grid.rwg");
    let file_bytes = fs::metadata(&hour_path)?.len();
    let grid_bytes = fs::metadata(&grid_path)?.len();

    // Pre-opened handles for the warm-path metrics.
    let reader = HourReader::open(&hour_path)?;
    let grid = GridFile::open(&grid_path)?;
    let locator = GridLocator::build(&grid);
    let meta = reader.meta().clone();
    let (nx, ny) = (meta.nx, meta.ny);

    let vars_2d: Vec<String> = meta
        .variables
        .iter()
        .filter(|var| var.kind == "surface2d")
        .map(|var| var.name.clone())
        .collect();
    let vars_3d: Vec<String> = meta
        .variables
        .iter()
        .filter(|var| var.kind == "pressure3d")
        .map(|var| var.name.clone())
        .collect();
    if vars_2d.is_empty() || vars_3d.is_empty() {
        return Err(format!(
            "hour has {} 2D and {} 3D variables; the bench needs at least one of each",
            vars_2d.len(),
            vars_3d.len()
        )
        .into());
    }
    let window_var = vars_2d
        .iter()
        .find(|name| *name == "temperature_2m")
        .unwrap_or(&vars_2d[0])
        .clone();

    let (fx, fy) = locator
        .locate(SOUNDING_LAT, SOUNDING_LON)
        .ok_or(format!("locate({SOUNDING_LAT}, {SOUNDING_LON}) is off-grid"))?;

    println!(
        "rw_bench build {} | {} {} f{:03} | grid {} x {} | samples {} (median, 1 warmup)",
        env!("RW_BUILD_SHA"),
        args.model,
        args.run,
        args.hour,
        nx,
        ny,
        args.samples,
    );
    println!(
        "hour file {} ({:.1} MB) | grid file {:.1} MB | 2D vars {} | 3D vars {} ({} levels)",
        hour_path.display(),
        mb(file_bytes),
        mb(grid_bytes),
        vars_2d.len(),
        vars_3d.len(),
        meta.variables
            .iter()
            .find(|var| var.kind == "pressure3d")
            .map(|var| var.levels_hpa.len())
            .unwrap_or(0),
    );
    println!(
        "sounding point {SOUNDING_LAT:.1}N {:.1}W -> grid ({fx:.2}, {fy:.2})",
        -SOUNDING_LON
    );
    println!();

    let mut rows: Vec<(String, Stats, Unit)> = Vec::new();

    // 1. open: fresh HourReader per iteration.
    rows.push((
        "open (HourReader)".to_string(),
        bench(args.samples, || {
            HourReader::open(&hour_path).expect("open hour file")
        }),
        Unit::Millis,
    ));

    // 2. grid_open: fresh GridFile per iteration.
    rows.push((
        "grid_open (GridFile)".to_string(),
        bench(args.samples, || {
            GridFile::open(&grid_path).expect("open grid file")
        }),
        Unit::Millis,
    ));

    // 3. locator_build: fresh locator from the pre-opened grid.
    rows.push((
        "locator_build (cold)".to_string(),
        bench(args.samples, || GridLocator::build(&grid)),
        Unit::Millis,
    ));

    // 4. locate_warm: per-call mean over LOCATE_INNER_CALLS, median across
    //    iterations.
    let locate_stats = {
        for _ in 0..LOCATE_INNER_CALLS {
            black_box(locator.locate(SOUNDING_LAT, SOUNDING_LON));
        }
        let mut times = Vec::with_capacity(args.samples);
        for _ in 0..args.samples {
            let started = Instant::now();
            for _ in 0..LOCATE_INNER_CALLS {
                black_box(locator.locate(SOUNDING_LAT, SOUNDING_LON));
            }
            times.push(started.elapsed().as_secs_f64() / LOCATE_INNER_CALLS as f64);
        }
        Stats::from_times(times)
    };
    rows.push((
        format!("locate_warm (per call, {LOCATE_INNER_CALLS}x/iter)"),
        locate_stats,
        Unit::Micros,
    ));

    // 5. read_full_2d per 2D variable on the pre-opened reader.
    let mut full_2d: Vec<(String, Stats)> = Vec::new();
    for name in &vars_2d {
        let stats = bench(args.samples, || {
            reader.read_full_2d(name).expect("full 2D read")
        });
        full_2d.push((name.clone(), stats));
        rows.push((format!("read_full_2d {name}"), stats, Unit::Millis));
    }

    // 6. windows on one representative 2D variable.
    let (qx1, qy1) = (nx / 2, ny / 2);
    let quarter_stats = bench(args.samples, || {
        reader
            .read_window_2d(&window_var, 0, 0, qx1, qy1)
            .expect("quarter window read")
    });
    rows.push((
        format!("window_quarter {window_var} ({qx1}x{qy1})"),
        quarter_stats,
        Unit::Millis,
    ));
    let (wx0, wy0) = (nx / 2 - 32, ny / 2 - 32);
    rows.push((
        format!("window_64 {window_var} (64x64 mid-grid)"),
        bench(args.samples, || {
            reader
                .read_window_2d(&window_var, wx0, wy0, wx0 + 64, wy0 + 64)
                .expect("64x64 window read")
        }),
        Unit::Millis,
    ));

    // 7. sounding_cold: everything from scratch, the worst-case first click.
    rows.push((
        format!("sounding_cold (open+build+locate+{} profiles)", vars_3d.len()),
        bench(args.samples, || {
            let grid = GridFile::open(&grid_path).expect("open grid file");
            let locator = GridLocator::build(&grid);
            let reader = HourReader::open(&hour_path).expect("open hour file");
            let (fx, fy) = locator
                .locate(SOUNDING_LAT, SOUNDING_LON)
                .expect("sounding point on grid");
            let mut values = 0usize;
            for name in &vars_3d {
                values += reader.read_profile_3d(name, fx, fy).expect("profile").len();
            }
            values
        }),
        Unit::Millis,
    ));

    // 8. sounding_warm: pre-opened handles, the steady-state click.
    let sounding_warm = bench(args.samples, || {
        let (fx, fy) = locator
            .locate(SOUNDING_LAT, SOUNDING_LON)
            .expect("sounding point on grid");
        let mut values = 0usize;
        for name in &vars_3d {
            values += reader.read_profile_3d(name, fx, fy).expect("profile").len();
        }
        values
    });
    rows.push((
        format!("sounding_warm (locate+{} profiles)", vars_3d.len()),
        sounding_warm,
        Unit::Millis,
    ));

    // --- timing table ---
    let label_width = rows.iter().map(|(label, ..)| label.len()).max().unwrap_or(0);
    println!(
        "{:<label_width$}  {:>12}  {:>12}  {:>12}",
        "metric", "median", "min", "max"
    );
    for (label, stats, unit) in &rows {
        println!(
            "{label:<label_width$}  {:>12}  {:>12}  {:>12}",
            fmt_time(stats.median, *unit),
            fmt_time(stats.min, *unit),
            fmt_time(stats.max, *unit),
        );
    }

    // 9. file size + per-variable compressed payload bytes from the index
    //    (the shared EXACT size walk; the payload is never read).
    let sizes = walk_hour_sizes(&hour_path)?;
    println!();
    println!(
        "{:<24} {:>10} {:>8} {:>12}",
        "variable", "kind", "chunks", "compressed"
    );
    for var in &sizes.vars {
        println!(
            "{:<24} {:>10} {:>8} {:>9.1} MB",
            var.name,
            var.kind,
            var.chunks,
            mb(var.bytes)
        );
    }
    println!(
        "{:<24} {:>10} {:>8} {:>9.1} MB   (file {:.1} MB incl. header/meta/index)",
        "total payload",
        "",
        sizes.vars.iter().map(|var| var.chunks).sum::<usize>(),
        mb(sizes.payload_bytes),
        mb(sizes.file_bytes),
    );

    // --- gates ---
    println!();
    println!("gates:");
    let mut failed = false;
    let mut gate = |pass: bool, line: String| {
        if !pass {
            failed = true;
        }
        println!("  {} {line}", if pass { "PASS" } else { "FAIL" });
    };

    let warm = sounding_warm.median;
    gate(
        warm <= GATE_SOUNDING_HARD_S,
        format!(
            "sounding_warm {:.2} ms <= {:.0} ms hard gate ({} {:.0} ms expected)",
            warm * 1e3,
            GATE_SOUNDING_HARD_S * 1e3,
            if warm <= GATE_SOUNDING_EXPECTED_S {
                "also within"
            } else {
                "above"
            },
            GATE_SOUNDING_EXPECTED_S * 1e3,
        ),
    );

    let (worst_name, worst_stats) = full_2d
        .iter()
        .max_by(|a, b| a.1.median.total_cmp(&b.1.median))
        .expect("at least one 2D var");
    gate(
        full_2d.iter().all(|(_, stats)| stats.median <= GATE_FULL_2D_S),
        format!(
            "read_full_2d worst {:.2} ms ({worst_name}) <= {:.0} ms per var",
            worst_stats.median * 1e3,
            GATE_FULL_2D_S * 1e3,
        ),
    );

    let full_window_var = full_2d
        .iter()
        .find(|(name, _)| *name == window_var)
        .expect("window var was benched")
        .1
        .median;
    let quarter_budget =
        GATE_WINDOW_QUARTER_RATIO * full_window_var + GATE_WINDOW_QUARTER_OVERHEAD_S;
    gate(
        quarter_stats.median <= quarter_budget,
        format!(
            "window_quarter {:.2} ms <= {GATE_WINDOW_QUARTER_RATIO} x full {:.2} ms + {:.1} ms overhead = {:.2} ms (loose area-scaling check)",
            quarter_stats.median * 1e3,
            full_window_var * 1e3,
            GATE_WINDOW_QUARTER_OVERHEAD_S * 1e3,
            quarter_budget * 1e3,
        ),
    );

    let build = rows
        .iter()
        .find(|(label, ..)| label.starts_with("locator_build"))
        .expect("locator_build row")
        .1
        .median;
    println!(
        "  INFO locator_build {:.2} ms (target <= {:.0} ms, informational)",
        build * 1e3,
        TARGET_LOCATOR_BUILD_S * 1e3,
    );

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

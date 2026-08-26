//! Resource-lifetime and output-equivalence proof for `rw_sharpmod --serve`.
//!
//! The serve loop keeps one `GridFile` / `GridLocator` / `HourReader` set open
//! across requests instead of reopening the store per export. Two properties
//! have to hold for that to be a safe swap:
//!
//!   * **Identical output.** A point exported through the persistent server
//!     must be byte-for-byte the same JSON the one-shot CLI writes for the
//!     same point. `serve_output_is_byte_identical_to_open_per_call` asserts
//!     that over a spread of points and also reports median wall time for
//!     both paths.
//!   * **Real reuse, correctly keyed.** The handles must actually survive
//!     across requests, and must be dropped and reopened when the requested
//!     hour changes. Both are observed without any timing heuristic: the
//!     run's `grid.rwg` is fully read into memory and closed by
//!     `GridFile::open`, so deleting it after a warm-up leaves the cached
//!     hour serviceable while any reopen fails. Exports for the warmed key
//!     keep succeeding; an export for a different hour fails.
//!
//! The store is synthesized here rather than taken from a fixture: these
//! tests care about handle lifetime and byte equality, not about any
//! particular model's numbers.

use std::fs;
use std::io::{BufRead, BufReader, Lines, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use rustwx_core::{GridShape, LatLonGrid};
use rw_store::grid::write_grid;
use rw_store::writer::HourWriter;

const NX: usize = 48;
const NY: usize = 32;
const MODEL: &str = "hrrr";
const RUN: &str = "20260715_12z";
const BUILD: &str = "rw-sharpmod-serve-test";
const GRID_HASH_TAG: &str = "sharpmod-serve-grid";
/// Descending pressure, matching how the store writes isobaric volumes.
const LEVELS: [u16; 10] = [1000, 950, 900, 850, 800, 700, 600, 500, 400, 300];
/// Surface pressure in Pa; the exporter divides by 100 to reach hPa, so the
/// 1000 hPa isobaric row is below ground and gets replaced by the surface.
const SURFACE_PRESSURE_PA: f32 = 97_500.0;

/// Rounds used for the in-process timing series (median of these).
const REPEATS: usize = 9;

/// Points spread across the synthetic domain, all comfortably inside it.
const POINTS: [(f64, f64); 5] = [
    (42.5, -101.3),
    (39.75, -97.0),
    (36.0, -94.25),
    (33.25, -90.5),
    (31.5, -87.0),
];

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

fn test_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-sharpmod-serve-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_dir(store_root: &Path) -> PathBuf {
    store_root.join(MODEL).join(RUN)
}

fn grid_path(store_root: &Path) -> PathBuf {
    run_dir(store_root).join("grid.rwg")
}

fn hour_path(store_root: &Path, fxx: u16) -> PathBuf {
    run_dir(store_root).join(format!("f{fxx:03}.rws"))
}

/// Regular lat/lon mesh, north-to-south so row 0 is the northernmost.
fn lat_lon_arrays() -> (Vec<f32>, Vec<f32>) {
    let mut lat = Vec::with_capacity(NX * NY);
    let mut lon = Vec::with_capacity(NX * NY);
    for y in 0..NY {
        let row_lat = 45.0 - 15.0 * (y as f32) / ((NY - 1) as f32);
        for x in 0..NX {
            lat.push(row_lat);
            lon.push(-105.0 + 20.0 * (x as f32) / ((NX - 1) as f32));
        }
    }
    (lat, lon)
}

/// Standard-atmosphere geopotential height for a pressure level, in metres.
fn level_height_m(pressure_hpa: u16) -> f32 {
    44_330.0 * (1.0 - (f32::from(pressure_hpa) / 1013.25).powf(0.190_3))
}

/// Small, smooth spatial signal so bilinear interpolation has something to
/// interpolate and neighbouring points differ.
fn spatial(x: usize, y: usize) -> f32 {
    0.5 * (x as f32) + 0.3 * (y as f32)
}

/// One isobaric plane. `fxx` shifts the whole hour so a stale cache entry
/// would surface as visibly wrong numbers rather than as a silent pass.
fn iso_plane(name: &str, pressure_hpa: u16, fxx: u16) -> Vec<f32> {
    let z = level_height_m(pressure_hpa);
    let hour = f32::from(fxx);
    (0..NX * NY)
        .map(|index| {
            let (x, y) = (index % NX, index / NX);
            let jitter = spatial(x, y);
            match name {
                "height_iso" => z + jitter,
                "temperature_iso" => 288.15 - 0.0065 * z + 0.02 * jitter + 0.75 * hour,
                "dewpoint_iso" => 283.15 - 0.0070 * z + 0.02 * jitter + 0.75 * hour,
                "u_iso" => 5.0 + 0.002 * z + 0.01 * jitter + 0.5 * hour,
                "v_iso" => -3.0 + 0.001 * z + 0.01 * jitter - 0.25 * hour,
                other => panic!("unexpected isobaric variable '{other}'"),
            }
        })
        .collect()
}

fn surface_plane(name: &str, fxx: u16) -> Vec<f32> {
    let hour = f32::from(fxx);
    (0..NX * NY)
        .map(|index| {
            let (x, y) = (index % NX, index / NX);
            let jitter = spatial(x, y);
            match name {
                "surface_pressure" => SURFACE_PRESSURE_PA,
                "orography" => 300.0 + jitter,
                "temperature_2m" => 295.0 + 0.01 * jitter + 0.75 * hour,
                "dewpoint_2m" => 289.0 + 0.01 * jitter + 0.75 * hour,
                "u_10m" => 3.0 + 0.01 * jitter + 0.5 * hour,
                "v_10m" => -1.0 + 0.01 * jitter - 0.25 * hour,
                other => panic!("unexpected surface variable '{other}'"),
            }
        })
        .collect()
}

fn write_hour(store_root: &Path, fxx: u16, grid_hash: &str) {
    let mut writer = HourWriter::new(MODEL, RUN, fxx, NX, NY, grid_hash, BUILD);

    for name in [
        "temperature_iso",
        "dewpoint_iso",
        "height_iso",
        "u_iso",
        "v_iso",
    ] {
        let units = match name {
            "temperature_iso" | "dewpoint_iso" => "K",
            "height_iso" => "gpm",
            _ => "m s-1",
        };
        let planes: Vec<Vec<f32>> = LEVELS
            .iter()
            .map(|&level| iso_plane(name, level, fxx))
            .collect();
        let plane_refs: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
        writer
            .add_pressure3d(
                name,
                units,
                serde_json::json!({"level_type": "isobaric", "store_name": name}),
                &LEVELS,
                &plane_refs,
            )
            .unwrap();
    }

    for name in [
        "surface_pressure",
        "orography",
        "temperature_2m",
        "dewpoint_2m",
        "u_10m",
        "v_10m",
    ] {
        let units = match name {
            "surface_pressure" => "Pa",
            "orography" => "m",
            "temperature_2m" | "dewpoint_2m" => "K",
            _ => "m s-1",
        };
        writer
            .add_surface2d(
                name,
                units,
                serde_json::json!({"level_type": "surface", "store_name": name}),
                &surface_plane(name, fxx),
            )
            .unwrap();
    }

    writer.finish(&hour_path(store_root, fxx)).unwrap();
}

/// Build a store root holding one run with hours f000 and f001.
fn fixture_store(tag: &str) -> PathBuf {
    let store_root = test_dir(tag);
    fs::create_dir_all(run_dir(&store_root)).unwrap();

    let (lat, lon) = lat_lon_arrays();
    let grid = LatLonGrid::new(GridShape::new(NX, NY).unwrap(), lat, lon).unwrap();
    let grid_hash = write_grid(&grid_path(&store_root), &grid, None).unwrap();
    assert!(!grid_hash.is_empty(), "{GRID_HASH_TAG} produced no hash");

    write_hour(&store_root, 0, &grid_hash);
    write_hour(&store_root, 1, &grid_hash);
    store_root
}

// ---------------------------------------------------------------------------
// Driving the binary
// ---------------------------------------------------------------------------

const SHARPMOD: &str = env!("CARGO_BIN_EXE_rw_sharpmod");

/// One `rw_sharpmod --serve` child, spoken to over stdin/stdout JSON lines.
struct Server {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Server {
    fn spawn() -> Self {
        let mut child = Command::new(SHARPMOD)
            .arg("--serve")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn rw_sharpmod --serve");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        Self {
            child,
            stdin,
            stdout,
        }
    }

    /// Send one already-encoded request line and read its response object.
    fn send_raw(&mut self, line: &str) -> serde_json::Value {
        writeln!(self.stdin, "{line}").expect("write serve request");
        self.stdin.flush().expect("flush serve request");
        let response = self
            .stdout
            .next()
            .expect("serve loop closed stdout early")
            .expect("read serve response");
        serde_json::from_str(&response).expect("serve response is JSON")
    }

    fn send(&mut self, request: serde_json::Value) -> serde_json::Value {
        self.send_raw(&request.to_string())
    }

    /// Close stdin so the serve loop drains, then require a clean exit.
    fn shutdown(mut self) {
        drop(self.stdin);
        let status = self.child.wait().expect("wait for rw_sharpmod --serve");
        assert!(status.success(), "serve loop exited with {status}");
    }
}

fn warm_request(id: u64, store_root: &Path, fxx: u16) -> serde_json::Value {
    serde_json::json!({
        "request_id": id,
        "action": "warm",
        "store_root": store_root,
        "model": MODEL,
        "run": RUN,
        "forecast_hour": fxx,
    })
}

fn export_request(
    id: u64,
    store_root: &Path,
    fxx: u16,
    lat: f64,
    lon: f64,
    output: &Path,
) -> serde_json::Value {
    serde_json::json!({
        "request_id": id,
        "action": "export",
        "store_root": store_root,
        "model": MODEL,
        "run": RUN,
        "forecast_hour": fxx,
        "lat": lat,
        "lon": lon,
        "output": output,
    })
}

fn assert_ok(response: &serde_json::Value, id: u64) -> usize {
    assert_eq!(response["request_id"].as_u64(), Some(id), "{response}");
    assert_eq!(response["ok"].as_bool(), Some(true), "{response}");
    assert!(response.get("error").is_none(), "{response}");
    response["levels"].as_u64().unwrap_or(0) as usize
}

fn assert_err(response: &serde_json::Value, id: u64) -> String {
    assert_eq!(response["request_id"].as_u64(), Some(id), "{response}");
    assert_eq!(response["ok"].as_bool(), Some(false), "{response}");
    response["error"]
        .as_str()
        .unwrap_or_else(|| panic!("failed response carries no error text: {response}"))
        .to_string()
}

/// One-shot CLI export: opens the store, writes the file, exits.
fn export_once(store_root: &Path, fxx: u16, lat: f64, lon: f64, output: &Path) {
    let out = Command::new(SHARPMOD)
        .arg("--store-root")
        .arg(store_root)
        .arg("--model")
        .arg(MODEL)
        .arg("--run")
        .arg(RUN)
        .arg("--forecast-hour")
        .arg(fxx.to_string())
        .arg("--lat")
        .arg(lat.to_string())
        .arg("--lon")
        .arg(lon.to_string())
        .arg("--output")
        .arg(output)
        .output()
        .expect("run one-shot rw_sharpmod");
    assert!(
        out.status.success(),
        "one-shot export failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn median(mut samples: Vec<Duration>) -> Duration {
    assert!(!samples.is_empty());
    samples.sort_unstable();
    samples[samples.len() / 2]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The persistent store must not change a single output byte, and the
/// per-request cost of reusing it is reported alongside the open-per-call
/// baseline. Timing is printed, never asserted: it is context for the port,
/// not a property this suite can hold stable on shared CI hardware.
#[test]
fn serve_output_is_byte_identical_to_open_per_call() {
    let store_root = fixture_store("identical");
    let out_dir = store_root.join("out");
    fs::create_dir_all(&out_dir).unwrap();

    // Baseline: a fresh process (and a fresh open) per point.
    let mut cold_times = Vec::new();
    let mut cold_files = Vec::new();
    for (index, (lat, lon)) in POINTS.iter().enumerate() {
        let output = out_dir.join(format!("cold-{index}.json"));
        let started = Instant::now();
        export_once(&store_root, 0, *lat, *lon, &output);
        cold_times.push(started.elapsed());
        cold_files.push(output);
    }

    // Persistent: one process, one open, every point served from it.
    let mut server = Server::spawn();
    let warm = server.send(warm_request(1, &store_root, 0));
    assert_ok(&warm, 1);

    let mut warm_times = Vec::new();
    let mut warm_files = Vec::new();
    for (index, (lat, lon)) in POINTS.iter().enumerate() {
        let output = out_dir.join(format!("warm-{index}.json"));
        let id = 100 + index as u64;
        let started = Instant::now();
        let response = server.send(export_request(id, &store_root, 0, *lat, *lon, &output));
        warm_times.push(started.elapsed());
        let levels = assert_ok(&response, id);
        assert!(
            levels >= 8,
            "point {index} exported only {levels} levels; the fixture should yield a full profile"
        );
        warm_files.push(output);
    }
    server.shutdown();

    for (index, (cold, warm)) in cold_files.iter().zip(&warm_files).enumerate() {
        let cold_bytes = fs::read(cold).unwrap();
        let warm_bytes = fs::read(warm).unwrap();
        assert_eq!(
            cold_bytes, warm_bytes,
            "point {index} at {:?} differs between open-per-call and the persistent store",
            POINTS[index]
        );
        assert!(!cold_bytes.is_empty(), "point {index} exported no bytes");
    }

    // Distinct points must not be collapsing to one answer, or byte equality
    // above would be trivially satisfiable by a constant.
    assert_ne!(
        fs::read(&warm_files[0]).unwrap(),
        fs::read(&warm_files[POINTS.len() - 1]).unwrap(),
        "distinct points produced identical soundings; the fixture is degenerate"
    );

    // The medians above bracket the whole one-shot path, process spawn
    // included. To price the store open on its own, run two more request
    // series inside a single server: one pinned to a hour (pure reuse) and
    // one alternating hours so every request is forced to reopen. Same
    // export work either way, so the gap is the open.
    let mut server = Server::spawn();
    let mut reuse_times = Vec::new();
    let mut reopen_times = Vec::new();
    let scratch = out_dir.join("timing.json");
    assert_ok(&server.send(warm_request(300, &store_root, 0)), 300);
    for round in 0..REPEATS {
        let id = 400 + round as u64;
        let started = Instant::now();
        assert_ok(
            &server.send(export_request(
                id,
                &store_root,
                0,
                POINTS[0].0,
                POINTS[0].1,
                &scratch,
            )),
            id,
        );
        reuse_times.push(started.elapsed());

        let id = 500 + round as u64;
        let fxx = u16::from(round % 2 == 0);
        let started = Instant::now();
        assert_ok(
            &server.send(export_request(
                id,
                &store_root,
                fxx,
                POINTS[0].0,
                POINTS[0].1,
                &scratch,
            )),
            id,
        );
        reopen_times.push(started.elapsed());
    }
    server.shutdown();

    println!(
        "rw_sharpmod export over {} points: one-shot process median {:?}, persistent-store median {:?}",
        POINTS.len(),
        median(cold_times),
        median(warm_times),
    );
    println!(
        "rw_sharpmod in-process export over {REPEATS} rounds: retained-store median {:?}, forced-reopen median {:?}",
        median(reuse_times),
        median(reopen_times),
    );
}

/// Resource lifetime, observed without timing: `GridFile::open` reads
/// `grid.rwg` fully and closes it, so removing that file after a warm-up
/// leaves the already-open hour fully serviceable and makes every reopen
/// fail. Exports on the warmed key must keep succeeding.
#[test]
fn serve_keeps_the_store_open_across_requests() {
    let store_root = fixture_store("lifetime");
    let out_dir = store_root.join("out");
    fs::create_dir_all(&out_dir).unwrap();

    let mut server = Server::spawn();
    assert_ok(&server.send(warm_request(1, &store_root, 0)), 1);

    // Pull the run's grid out from under the server. Only a reopen needs it.
    fs::remove_file(grid_path(&store_root)).expect("grid.rwg is closed after open");
    assert!(!grid_path(&store_root).exists());

    for (index, (lat, lon)) in POINTS.iter().enumerate() {
        let id = 200 + index as u64;
        let output = out_dir.join(format!("held-{index}.json"));
        let response = server.send(export_request(id, &store_root, 0, *lat, *lon, &output));
        assert_ok(&response, id);
        assert!(
            output.exists(),
            "export {index} reported success but wrote no file"
        );
    }

    // The one-shot path has no cached handles, so it must fail on the same
    // store — proof the successes above came from the retained resources
    // rather than from the grid still being readable somehow.
    let doomed = out_dir.join("one-shot-after-removal.json");
    let out = Command::new(SHARPMOD)
        .arg("--store-root")
        .arg(&store_root)
        .arg("--model")
        .arg(MODEL)
        .arg("--run")
        .arg(RUN)
        .arg("--forecast-hour")
        .arg("0")
        .arg("--lat")
        .arg(POINTS[0].0.to_string())
        .arg("--lon")
        .arg(POINTS[0].1.to_string())
        .arg("--output")
        .arg(&doomed)
        .output()
        .expect("run one-shot rw_sharpmod");
    assert!(
        !out.status.success(),
        "one-shot export succeeded without grid.rwg; the lifetime probe proves nothing"
    );

    server.shutdown();
}

/// The cache is keyed by (store root, model, run, hour): a different hour has
/// to drop the retained handles and reopen. With `grid.rwg` removed that
/// reopen is guaranteed to fail, which is exactly how the swap is observed.
#[test]
fn serve_reopens_the_store_when_the_requested_hour_changes() {
    let store_root = fixture_store("rekey");
    let out_dir = store_root.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let (lat, lon) = POINTS[1];

    let mut server = Server::spawn();

    // While the grid is present, both hours export and must differ: the
    // fixture offsets every hour, so a cache that ignored the key would be
    // caught here.
    let hour0 = out_dir.join("hour0.json");
    let hour1 = out_dir.join("hour1.json");
    assert_ok(
        &server.send(export_request(1, &store_root, 0, lat, lon, &hour0)),
        1,
    );
    assert_ok(
        &server.send(export_request(2, &store_root, 1, lat, lon, &hour1)),
        2,
    );
    assert_ne!(
        fs::read(&hour0).unwrap(),
        fs::read(&hour1).unwrap(),
        "f000 and f001 exported identical soundings; the hour key is being ignored"
    );

    // Re-request f001, the currently cached key, then remove the grid.
    let hour1_again = out_dir.join("hour1-again.json");
    assert_ok(
        &server.send(export_request(3, &store_root, 1, lat, lon, &hour1_again)),
        3,
    );
    fs::remove_file(grid_path(&store_root)).expect("grid.rwg is closed after open");

    // Cached key: still served from the retained handles.
    let cached = out_dir.join("cached.json");
    assert_ok(
        &server.send(export_request(4, &store_root, 1, lat, lon, &cached)),
        4,
    );
    assert_eq!(
        fs::read(&hour1).unwrap(),
        fs::read(&cached).unwrap(),
        "the retained hour changed its answer across requests"
    );

    // Different key: forced reopen, which now cannot find the grid.
    let missed = out_dir.join("missed.json");
    let response = server.send(export_request(5, &store_root, 0, lat, lon, &missed));
    let error = assert_err(&response, 5);
    assert!(!error.is_empty(), "reopen failure carried no message");
    assert!(
        !missed.exists(),
        "a failed reopen still wrote an output file"
    );

    server.shutdown();
}

/// A bad request must be answered and survived, not fatal: the loop reports
/// the failure against the right `request_id` and keeps serving.
#[test]
fn serve_reports_request_failures_and_keeps_serving() {
    let store_root = fixture_store("errors");
    let out_dir = store_root.join("out");
    fs::create_dir_all(&out_dir).unwrap();
    let (lat, lon) = POINTS[2];

    let mut server = Server::spawn();

    // Malformed JSON: answered under request_id 0 because none was parsed.
    let malformed = server.send_raw("{not json at all");
    assert!(
        assert_err(&malformed, 0).contains("invalid request JSON"),
        "{malformed}"
    );

    // Unknown action.
    let mut unknown = warm_request(11, &store_root, 0);
    unknown["action"] = serde_json::Value::String("teleport".into());
    let response = server.send(unknown);
    assert!(assert_err(&response, 11).contains("teleport"), "{response}");

    // Export missing its coordinates.
    let mut incomplete = warm_request(12, &store_root, 0);
    incomplete["action"] = serde_json::Value::String("export".into());
    incomplete["output"] = serde_json::json!(out_dir.join("incomplete.json"));
    let response = server.send(incomplete);
    assert!(!assert_err(&response, 12).is_empty(), "{response}");

    // Point outside the synthetic domain.
    let far = out_dir.join("far.json");
    let response = server.send(export_request(13, &store_root, 0, 12.0, 40.0, &far));
    assert!(!assert_err(&response, 13).is_empty(), "{response}");
    assert!(!far.exists(), "a rejected point still wrote an output file");

    // Missing hour.
    let absent = out_dir.join("absent.json");
    let response = server.send(export_request(14, &store_root, 99, lat, lon, &absent));
    assert!(!assert_err(&response, 14).is_empty(), "{response}");

    // Still healthy afterwards.
    let good = out_dir.join("good.json");
    let response = server.send(export_request(15, &store_root, 0, lat, lon, &good));
    assert!(assert_ok(&response, 15) >= 8, "{response}");
    assert!(good.exists());

    server.shutdown();
}

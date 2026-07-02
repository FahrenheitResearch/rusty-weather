//! Local CAFire render API prototype.
//!
//! This intentionally stays small and dependency-free: it is a local
//! `TcpListener` that serves a demo page, accepts draw-a-box render jobs,
//! shells out to the sibling `rw_render` binary, and serves generated PNGs.
//! The request shape is the server contract CAFire.org would eventually call.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use serde::{Deserialize, Serialize};

#[path = "../perimeter.rs"]
mod perimeter;

#[path = "../meteogram.rs"]
mod meteogram;

const MIN_RENDER_WIDTH: u32 = 1200;
const MIN_RENDER_HEIGHT: u32 = 900;
const MAX_RENDER_DIMENSION: u32 = 2400;
/// Request bodies above this are rejected before buffering; large enough
/// for multi-thousand-point perimeter GeoJSON, far below memory-pressure
/// territory on burst days.
const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "rw-fire-api",
    about = "Local CAFire draw-a-box render API over rw-store/.rws data"
)]
struct Args {
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8787)]
    port: u16,
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "out/fire_api")]
    out_root: PathBuf,
    #[arg(long, help = "Path to rw_render; default is sibling executable")]
    rw_render: Option<PathBuf>,
    #[arg(
        long,
        default_value_t = 2,
        help = "Maximum simultaneous rw_render child processes"
    )]
    max_render_jobs: usize,
    #[arg(
        long,
        help = "Thread count forwarded to rw_render; tune with --max-render-jobs to avoid oversubscription"
    )]
    render_threads: Option<usize>,
    #[arg(
        long,
        default_value_t = false,
        help = "Forward --full-throttle to rw_render for dedicated server nodes"
    )]
    full_throttle_render: bool,
    #[arg(
        long,
        default_value_t = 300,
        help = "Kill rw_render children that run longer than this many seconds"
    )]
    render_timeout_secs: u64,
}

#[derive(Clone)]
struct AppState {
    store_root: PathBuf,
    out_root: PathBuf,
    rw_render: PathBuf,
    render_threads: Option<usize>,
    full_throttle_render: bool,
    render_timeout: Duration,
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    render_cache: Arc<Mutex<HashMap<String, String>>>,
    counter: Arc<AtomicU64>,
    render_gate: Arc<RenderGate>,
    /// Cached /api/fires response body and when it was fetched.
    fires_cache: Arc<Mutex<Option<(Instant, Vec<u8>)>>>,
}

struct RenderGate {
    max_active: usize,
    state: Mutex<RenderGateState>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct RenderGateState {
    active: usize,
    waiting: usize,
}

struct RenderPermit {
    gate: Arc<RenderGate>,
}

impl RenderGate {
    fn new(max_active: usize) -> Self {
        Self {
            max_active: max_active.max(1),
            state: Mutex::new(RenderGateState::default()),
            changed: Condvar::new(),
        }
    }

    fn acquire(self: &Arc<Self>) -> RenderPermit {
        let mut state = self.state.lock().expect("render gate mutex");
        state.waiting += 1;
        while state.active >= self.max_active {
            state = self.changed.wait(state).expect("render gate mutex");
        }
        state.waiting -= 1;
        state.active += 1;
        RenderPermit { gate: self.clone() }
    }

    fn snapshot(&self) -> serde_json::Value {
        let state = self.state.lock().expect("render gate mutex");
        serde_json::json!({
            "max_active": self.max_active,
            "active": state.active,
            "waiting": state.waiting,
        })
    }
}

impl Drop for RenderPermit {
    fn drop(&mut self) {
        let mut state = self.gate.state.lock().expect("render gate mutex");
        state.active = state.active.saturating_sub(1);
        self.gate.changed.notify_one();
    }
}

#[derive(Debug, Clone, Serialize)]
struct Job {
    id: String,
    state: JobState,
    request: RenderJobRequest,
    output_dir: String,
    message: String,
    stdout_tail: String,
    stderr_tail: String,
    files: Vec<RenderedFile>,
    created_unix_ms: u128,
    started_unix_ms: Option<u128>,
    finished_unix_ms: Option<u128>,
    wall_ms: Option<u128>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum JobState {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RenderJobRequest {
    #[serde(default = "default_model")]
    model: String,
    #[serde(default = "default_run")]
    run: String,
    #[serde(default = "default_hour")]
    hour: u16,
    #[serde(default = "default_products")]
    products: String,
    #[serde(default = "default_output_format")]
    output_format: String,
    #[serde(default = "default_plot_style")]
    plot_style: String,
    #[serde(default = "default_basemap_style")]
    basemap_style: String,
    #[serde(default = "default_county_linework")]
    county_linework: bool,
    #[serde(default = "default_place_label_density")]
    place_label_density: u8,
    #[serde(default = "default_place_label_size")]
    place_label_size: u8,
    #[serde(default = "default_domain_slug")]
    domain_slug: String,
    /// Render bounds; either given directly (draw-a-box) or computed by
    /// validation from `perimeter`. Always `Some` after validation.
    #[serde(default)]
    bounds: Option<[f64; 4]>,
    /// Fire perimeter ring as `[lon, lat]` pairs. When present, bounds are
    /// computed around it and the ring is overlaid on the rendered maps.
    #[serde(default)]
    perimeter: Option<Vec<[f64; 2]>>,
    /// Kilometers of padding around the perimeter (default 50).
    #[serde(default)]
    padding_km: Option<f64>,
    /// One-sided extension toward an expected spread bearing.
    #[serde(default)]
    extend: Option<ExtendRequest>,
    /// Draw the perimeter ring on the maps (default true).
    #[serde(default)]
    overlay_perimeter: Option<bool>,
    /// Free-text context appended to every plot title as " (note)" —
    /// e.g. "Aspen Acres Fire". Sanitized and capped at validation.
    #[serde(default)]
    title_note: Option<String>,
    #[serde(default)]
    output_width: Option<u32>,
    #[serde(default)]
    output_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ExtendRequest {
    direction_deg: f64,
    distance_km: f64,
}

impl RenderJobRequest {
    /// Bounds after successful validation.
    fn resolved_bounds(&self) -> [f64; 4] {
        self.bounds
            .expect("request was validated: bounds are resolved")
    }

    fn overlay_perimeter_enabled(&self) -> bool {
        self.perimeter.is_some() && self.overlay_perimeter.unwrap_or(true)
    }
}

#[derive(Debug, Clone, Serialize)]
struct RenderedFile {
    name: String,
    url: String,
    bytes: u64,
}

#[derive(Debug)]
struct HttpRequest {
    method: String,
    path: String,
    /// Raw query string (after '?'), empty when absent.
    query: String,
    body: Vec<u8>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.max_render_jobs == 0 {
        return Err("--max-render-jobs must be at least 1".into());
    }
    fs::create_dir_all(&args.out_root)?;
    let rw_render = args.rw_render.unwrap_or_else(sibling_rw_render_path);
    let state = AppState {
        store_root: args.store_root,
        out_root: args.out_root,
        rw_render,
        render_threads: args.render_threads,
        full_throttle_render: args.full_throttle_render,
        render_timeout: Duration::from_secs(args.render_timeout_secs.max(1)),
        jobs: Arc::new(Mutex::new(HashMap::new())),
        render_cache: Arc::new(Mutex::new(HashMap::new())),
        counter: Arc::new(AtomicU64::new(1)),
        render_gate: Arc::new(RenderGate::new(args.max_render_jobs)),
        fires_cache: Arc::new(Mutex::new(None)),
    };
    let addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&addr)?;
    println!("rw_fire_api listening on http://{addr}");
    println!("store_root: {}", state.store_root.display());
    println!("out_root: {}", state.out_root.display());
    println!("rw_render: {}", state.rw_render.display());
    println!("max_render_jobs: {}", state.render_gate.max_active);
    if let Some(threads) = state.render_threads {
        println!("render_threads: {threads}");
    }
    println!("full_throttle_render: {}", state.full_throttle_render);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let state = state.clone();
                std::thread::spawn(move || {
                    if let Err(err) = handle_stream(stream, state) {
                        eprintln!("request failed: {err}");
                    }
                });
            }
            Err(err) => eprintln!("accept failed: {err}"),
        }
    }
    Ok(())
}

fn handle_stream(mut stream: TcpStream, state: AppState) -> Result<(), Box<dyn std::error::Error>> {
    let request = read_request(&mut stream)?;
    let response = route(request, state);
    stream.write_all(&response)?;
    Ok(())
}

fn route(request: HttpRequest, state: AppState) -> Vec<u8> {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => html_response(PREVIEW_HTML),
        ("GET", "/ops") => html_response(SITE_HTML),
        ("GET", "/legacy") => html_response(DEMO_HTML),
        ("GET", "/api/health") => json_response(&serde_json::json!({
            "ok": true,
            "service": "rw-fire-api",
            "store_root": state.store_root.display().to_string(),
            "out_root": state.out_root.display().to_string(),
            "rw_render": state.rw_render.display().to_string(),
            "render_threads": state.render_threads,
            "full_throttle_render": state.full_throttle_render,
            "render_timeout_secs": state.render_timeout.as_secs(),
            "render_gate": state.render_gate.snapshot(),
            "render_cache_entries": state.render_cache.lock().expect("render cache mutex").len(),
        })),
        ("POST", "/api/render") => start_render_job(request.body, state),
        ("OPTIONS", _) => empty_response(204),
        _ if request.method == "GET" && request.path.starts_with("/api/meteogram") => {
            meteogram_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/runs") => {
            runs_response(&request.query, &state)
        }
        ("GET", "/api/fires") => fires_response(&state),
        _ if request.method == "GET" && request.path.starts_with("/api/jobs/") => {
            let id = request.path.trim_start_matches("/api/jobs/");
            job_response(id, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/outputs/") => {
            output_file_response(&request.path, &state)
        }
        _ => text_response(404, "not found"),
    }
}

fn start_render_job(body: Vec<u8>, state: AppState) -> Vec<u8> {
    let parsed = serde_json::from_slice::<RenderJobRequest>(&body)
        .map_err(|err| format!("invalid JSON body: {err}"))
        .and_then(validate_render_request);
    let mut request = match parsed {
        Ok(request) => request,
        Err(message) => return json_status_response(400, &serde_json::json!({ "error": message })),
    };
    // Resolve `latest` BEFORE the cache key: alias entries must never
    // outlive the run they pointed at.
    let alias = request.run.trim().to_ascii_lowercase();
    if alias == "latest" || alias == "latest-day" {
        request.run =
            match resolve_latest_run(&state.store_root, &request.model, alias == "latest-day") {
                Ok(run) => run,
                Err(message) => {
                    return json_status_response(422, &serde_json::json!({ "error": message }));
                }
            };
    }

    let id = next_job_id(&state);
    let cache_key = render_cache_key(&request);
    if let Some(cached_id) = cached_render_job_id(&state, &cache_key) {
        return json_status_response(
            202,
            &serde_json::json!({
                "id": cached_id,
                "status_url": format!("/api/jobs/{cached_id}"),
                "cache": "hit",
            }),
        );
    }

    state
        .render_cache
        .lock()
        .expect("render cache mutex")
        .insert(cache_key.clone(), id.clone());
    let output_dir = state.out_root.join(&id);
    let job = Job {
        id: id.clone(),
        state: JobState::Queued,
        request: request.clone(),
        output_dir: output_dir.display().to_string(),
        message: "queued".to_string(),
        stdout_tail: String::new(),
        stderr_tail: String::new(),
        files: Vec::new(),
        created_unix_ms: unix_ms_now(),
        started_unix_ms: None,
        finished_unix_ms: None,
        wall_ms: None,
    };
    state
        .jobs
        .lock()
        .expect("job mutex")
        .insert(id.clone(), job);

    let worker_state = state.clone();
    let worker_id = id.clone();
    std::thread::spawn(move || run_job(worker_state, worker_id, request, output_dir, cache_key));

    json_status_response(
        202,
        &serde_json::json!({
            "id": id,
            "status_url": format!("/api/jobs/{id}"),
            "cache": "miss",
        }),
    )
}

fn cached_render_job_id(state: &AppState, cache_key: &str) -> Option<String> {
    let cached_id = state
        .render_cache
        .lock()
        .expect("render cache mutex")
        .get(cache_key)
        .cloned()?;
    let usable = {
        let jobs = state.jobs.lock().expect("job mutex");
        match jobs.get(&cached_id) {
            Some(job) if matches!(job.state, JobState::Queued | JobState::Running) => true,
            Some(job) if job.state == JobState::Succeeded => job
                .files
                .iter()
                .all(|file| state.out_root.join(&cached_id).join(&file.name).is_file()),
            _ => false,
        }
    };
    if usable {
        Some(cached_id)
    } else {
        state
            .render_cache
            .lock()
            .expect("render cache mutex")
            .remove(cache_key);
        None
    }
}

fn job_response(id: &str, state: &AppState) -> Vec<u8> {
    let jobs = state.jobs.lock().expect("job mutex");
    match jobs.get(id) {
        Some(job) => json_response(job),
        None => json_status_response(404, &serde_json::json!({ "error": "job not found" })),
    }
}

fn output_file_response(path: &str, state: &AppState) -> Vec<u8> {
    let parts = path
        .trim_start_matches("/outputs/")
        .split('/')
        .collect::<Vec<_>>();
    if parts.len() != 2 || !safe_path_component(parts[0]) || !safe_path_component(parts[1]) {
        return text_response(400, "bad output path");
    }
    let job_id = parts[0];
    let file_name = parts[1];
    let jobs = state.jobs.lock().expect("job mutex");
    let Some(job) = jobs.get(job_id) else {
        return text_response(404, "job not found");
    };
    if !job.files.iter().any(|file| file.name == file_name) {
        return text_response(404, "file not found");
    }
    drop(jobs);
    let path = state.out_root.join(job_id).join(file_name);
    let content_type = match path.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext.eq_ignore_ascii_case("webp") => "image/webp",
        _ => "image/png",
    };
    match fs::read(&path) {
        Ok(bytes) => binary_response(200, content_type, bytes),
        Err(err) => text_response(404, &format!("read {}: {err}", path.display())),
    }
}

fn run_job(
    state: AppState,
    id: String,
    request: RenderJobRequest,
    output_dir: PathBuf,
    cache_key: String,
) {
    update_job(&state, &id, |job| {
        job.message = "waiting for render slot".to_string();
    });
    let _permit = state.render_gate.acquire();
    let started = Instant::now();
    update_job(&state, &id, |job| {
        job.state = JobState::Running;
        job.message = "running rw_render".to_string();
        job.started_unix_ms = Some(unix_ms_now());
    });
    let result = run_rw_render(&state, &request, &output_dir, &id);
    let wall_ms = started.elapsed().as_millis();
    update_job(&state, &id, |job| {
        job.finished_unix_ms = Some(unix_ms_now());
        job.wall_ms = Some(wall_ms);
        match result {
            Ok((files, stdout_tail, stderr_tail)) => {
                job.state = JobState::Succeeded;
                job.message = format!("rendered {} file(s) in {wall_ms} ms", files.len());
                job.files = files;
                job.stdout_tail = stdout_tail;
                job.stderr_tail = stderr_tail;
            }
            Err((message, stdout_tail, stderr_tail)) => {
                job.state = JobState::Failed;
                job.message = message;
                job.stdout_tail = stdout_tail;
                job.stderr_tail = stderr_tail;
                state
                    .render_cache
                    .lock()
                    .expect("render cache mutex")
                    .remove(&cache_key);
            }
        }
    });
}

fn run_rw_render(
    state: &AppState,
    request: &RenderJobRequest,
    output_dir: &Path,
    job_id: &str,
) -> Result<(Vec<RenderedFile>, String, String), (String, String, String)> {
    fs::create_dir_all(output_dir).map_err(|err| {
        (
            format!("create {}: {err}", output_dir.display()),
            String::new(),
            String::new(),
        )
    })?;
    let (width, height) = output_size(request);
    let place_label_density = request.place_label_density.to_string();
    let (place_label_size_factor, place_label_alpha_factor) =
        place_label_render_env(request.place_label_size);
    let mut command = Command::new(&state.rw_render);
    command.args([
        "--model",
        &request.model,
        "--run",
        &request.run,
        "--hour",
        &request.hour.to_string(),
        "--store-root",
        &state.store_root.display().to_string(),
        "--out-dir",
        &output_dir.display().to_string(),
        "--products",
        &request.products,
        "--output-format",
        &request.output_format,
        &format!("--domain-bounds={}", format_bounds(request.resolved_bounds())),
        "--domain-slug",
        &safe_slug(&request.domain_slug),
        "--png-compression",
        "fastest",
        "--place-label-density",
        &place_label_density,
    ]);
    if let Some(threads) = state.render_threads {
        let threads = threads.to_string();
        command.args(["--threads", &threads]);
    }
    if state.full_throttle_render {
        command.arg("--full-throttle");
    }
    command.env("RUSTWX_PROJECTED_FRAME_SOURCE", "requested");
    command.env("RUSTWX_PROJECTION_VARIANT", "mercator");
    command.env("RUSTWX_PLOT_STYLE", &request.plot_style);
    command.env("RUSTWX_BASEMAP_STYLE", &request.basemap_style);
    command.env(
        "RUSTWX_COUNTY_LINEWORK",
        if request.county_linework {
            "true"
        } else {
            "false"
        },
    );
    command.env("RUSTWX_STATIC_OUTPUT_WIDTH", width.to_string());
    command.env("RUSTWX_STATIC_OUTPUT_HEIGHT", height.to_string());
    command.env("RUSTWX_PLACE_LABEL_SIZE_FACTOR", place_label_size_factor);
    command.env("RUSTWX_PLACE_LABEL_ALPHA_FACTOR", place_label_alpha_factor);
    if request.overlay_perimeter_enabled() {
        let overlay_path = output_dir.join("perimeter_overlay.json");
        write_perimeter_overlay_spec(&overlay_path, request).map_err(|err| {
            (
                format!("write {}: {err}", overlay_path.display()),
                String::new(),
                String::new(),
            )
        })?;
        command.env(
            "RUSTWX_OVERLAY_POLYLINE_FILE",
            overlay_path.display().to_string(),
        );
    }
    if let Some(note) = &request.title_note {
        // Every render lane appends this to its plot title as " (note)".
        command.env("RUSTWX_TITLE_SUFFIX", note);
    }

    // Child output goes to per-job log files instead of in-memory pipes so
    // the deadline loop below never deadlocks on a full pipe and the API
    // never buffers unbounded child output.
    let stdout_path = output_dir.join("render_stdout.log");
    let stderr_path = output_dir.join("render_stderr.log");
    for (stream, path) in [("stdout", &stdout_path), ("stderr", &stderr_path)] {
        let file = fs::File::create(path).map_err(|err| {
            (
                format!("create {stream} log {}: {err}", path.display()),
                String::new(),
                String::new(),
            )
        })?;
        if stream == "stdout" {
            command.stdout(file);
        } else {
            command.stderr(file);
        }
    }
    let status = run_command_with_deadline(&mut command, state.render_timeout).map_err(|err| {
        (
            format!("rw_render {err}"),
            read_log_tail(&stdout_path),
            read_log_tail(&stderr_path),
        )
    })?;
    let stdout_tail = read_log_tail(&stdout_path);
    let stderr_tail = read_log_tail(&stderr_path);
    if !status.success() {
        return Err((
            format!("rw_render exited with {status}"),
            stdout_tail,
            stderr_tail,
        ));
    }
    let files = collect_rendered_files(output_dir, job_id).map_err(|err| {
        (
            format!("collect rendered files: {err}"),
            stdout_tail.clone(),
            stderr_tail.clone(),
        )
    })?;
    if files.is_empty() {
        // Exit 0 with zero outputs means every requested product was
        // blocked/skipped (e.g. windowed sources not yet ingested). Caching
        // that as a success would poison the request key forever.
        return Err((
            "rw_render produced no output files (all requested products were blocked or skipped)"
                .to_string(),
            stdout_tail,
            stderr_tail,
        ));
    }
    Ok((files, stdout_tail, stderr_tail))
}

/// Write the render-side overlay spec (`{"rings": [[[lon, lat], ...]]}`)
/// consumed through `RUSTWX_OVERLAY_POLYLINE_FILE`.
fn write_perimeter_overlay_spec(
    path: &Path,
    request: &RenderJobRequest,
) -> std::io::Result<()> {
    let Some(points) = &request.perimeter else {
        return Ok(());
    };
    let spec = serde_json::json!({ "rings": [points] });
    fs::write(path, serde_json::to_vec(&spec)?)
}

fn collect_rendered_files(output_dir: &Path, job_id: &str) -> std::io::Result<Vec<RenderedFile>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(output_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !is_served_image_extension(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let bytes = entry.metadata()?.len();
        files.push(RenderedFile {
            name: name.to_string(),
            url: format!("/outputs/{job_id}/{name}"),
            bytes,
        });
    }
    files.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(files)
}

fn is_served_image_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase()),
        Some(ext) if matches!(ext.as_str(), "png" | "webp")
    )
}

fn update_job(state: &AppState, id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(job) = state.jobs.lock().expect("job mutex").get_mut(id) {
        f(job);
    }
}

/// Live active-fire perimeters from the public WFIGS ArcGIS feed (key-free),
/// simplified for the perimeter-domain API and the site's fire picker.
/// Served from a 10-minute in-memory cache so testers never hammer NIFC.
// National, ordered by size (HRRR covers all of CONUS; the picker labels
// carry the state). No state filter — a big Colorado fire matters too.
const WFIGS_URL: &str = "https://services3.arcgis.com/T4QMspbfLg3qTGWY/arcgis/rest/services/WFIGS_Interagency_Perimeters_Current/FeatureServer/0/query?where=poly_GISAcres%3E300&outFields=poly_IncidentName,poly_GISAcres,attr_POOState,poly_DateCurrent&orderByFields=poly_GISAcres+DESC&resultRecordCount=60&geometryPrecision=4&outSR=4326&f=geojson";
const FIRES_CACHE_SECS: u64 = 600;
const FIRE_RING_MAX_POINTS: usize = 240;

fn fires_agent() -> ureq::Agent {
    static CRYPTO_PROVIDER: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    CRYPTO_PROVIDER.get_or_init(|| {
        rustls::crypto::CryptoProvider::install_default(rustls_rustcrypto::provider()).ok();
    });
    let crypto = std::sync::Arc::new(rustls_rustcrypto::provider());
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(15)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .tls_config(
            ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::WebPki)
                .unversioned_rustls_crypto_provider(crypto)
                .build(),
        )
        .build()
        .new_agent()
}

/// Largest outer ring of a Polygon/MultiPolygon, decimated to a point cap —
/// plenty for domain framing and the drawn overlay.
fn largest_ring(geometry: &serde_json::Value) -> Option<Vec<[f64; 2]>> {
    let coords = geometry.get("coordinates")?;
    let outer_rings: Vec<&serde_json::Value> = match geometry.get("type")?.as_str()? {
        "Polygon" => coords.as_array()?.first().into_iter().collect(),
        "MultiPolygon" => coords
            .as_array()?
            .iter()
            .filter_map(|poly| poly.as_array()?.first())
            .collect(),
        _ => return None,
    };
    let ring = outer_rings
        .into_iter()
        .filter_map(|ring| ring.as_array())
        .max_by_key(|ring| ring.len())?;
    let step = (ring.len() / FIRE_RING_MAX_POINTS).max(1);
    let points: Vec<[f64; 2]> = ring
        .iter()
        .step_by(step)
        .filter_map(|point| {
            let pair = point.as_array()?;
            Some([pair.first()?.as_f64()?, pair.get(1)?.as_f64()?])
        })
        .collect();
    (points.len() >= 4).then_some(points)
}

fn fetch_fires() -> Result<Vec<u8>, String> {
    let mut response = fires_agent()
        .get(WFIGS_URL)
        .call()
        .map_err(|err| format!("WFIGS fetch: {err}"))?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|err| format!("WFIGS body: {err}"))?;
    let geojson: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("WFIGS parse: {err}"))?;
    let fires: Vec<serde_json::Value> = geojson
        .get("features")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|feature| {
            let props = feature.get("properties")?;
            let ring = largest_ring(feature.get("geometry")?)?;
            Some(serde_json::json!({
                "name": props.get("poly_IncidentName").and_then(|v| v.as_str()).unwrap_or("unnamed").trim(),
                "acres": props.get("poly_GISAcres").and_then(|v| v.as_f64()).unwrap_or(0.0).round(),
                "state": props.get("attr_POOState").and_then(|v| v.as_str()).unwrap_or(""),
                "updated_ms": props.get("poly_DateCurrent").and_then(|v| v.as_i64()),
                "ring": ring,
            }))
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "source": "WFIGS current perimeters", "fires": fires }))
        .map_err(|err| err.to_string())
}

fn fires_response(state: &AppState) -> Vec<u8> {
    {
        let cache = state.fires_cache.lock().expect("fires cache mutex");
        if let Some((fetched, body)) = cache.as_ref() {
            if fetched.elapsed().as_secs() < FIRES_CACHE_SECS {
                return response(200, "application/json; charset=utf-8", body.clone());
            }
        }
    }
    match fetch_fires() {
        Ok(body) => {
            *state.fires_cache.lock().expect("fires cache mutex") =
                Some((Instant::now(), body.clone()));
            response(200, "application/json; charset=utf-8", body)
        }
        Err(message) => {
            // Serve stale data over an error when the upstream hiccups.
            let cache = state.fires_cache.lock().expect("fires cache mutex");
            if let Some((_, body)) = cache.as_ref() {
                return response(200, "application/json; charset=utf-8", body.clone());
            }
            json_status_response(502, &serde_json::json!({ "error": message }))
        }
    }
}

/// GET /api/runs[?model=hrrr] — stored runs plus the daemon's latest manifest.
fn runs_response(query: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(query);
    let model = query
        .get("model")
        .map(String::as_str)
        .unwrap_or("hrrr")
        .to_string();
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return json_status_response(400, &serde_json::json!({ "error": "model slug is not valid" }));
    }
    let model_dir = state.store_root.join(&model);
    let mut runs: Vec<String> = std::fs::read_dir(&model_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains('_') && name.ends_with('z'))
        .collect();
    runs.sort();
    runs.reverse();
    let latest: Option<serde_json::Value> = std::fs::read_to_string(model_dir.join("latest.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    json_response(&serde_json::json!({ "model": model, "runs": runs, "latest": latest }))
}

/// Resolve the `latest` / `latest-day` run aliases via the daemon's atomic
/// manifest. `latest` = newest fully-stored run; `latest-day` = newest run
/// covering a full UTC day (what the anomaly/day-window lanes need).
fn resolve_latest_run(store_root: &Path, model: &str, day: bool) -> Result<String, String> {
    let path = store_root.join(model).join("latest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("no latest-run manifest for model '{model}' (daemon not running?)"))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("latest.json: {err}"))?;
    let field = |name: &str| manifest.get(name).and_then(|value| value.as_str());
    let run = if day {
        field("day_run").or_else(|| field("complete_run")).or_else(|| field("run"))
    } else {
        field("complete_run").or_else(|| field("run"))
    }
    .ok_or("latest.json has no run field")?;
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return Err("latest.json run slug is not valid".to_string());
    }
    Ok(run.to_string())
}

fn validate_render_request(mut request: RenderJobRequest) -> Result<RenderJobRequest, String> {
    request.model = safe_model_slug(&request.model);
    // Title note: printable text only, capped — it lands verbatim in the
    // plot chrome via RUSTWX_TITLE_SUFFIX.
    request.title_note = request.title_note.take().and_then(|note| {
        let cleaned: String = note
            .chars()
            .filter(|c| !c.is_control())
            .take(60)
            .collect::<String>()
            .trim()
            .to_string();
        (!cleaned.is_empty()).then_some(cleaned)
    });
    request.output_format = request.output_format.trim().to_ascii_lowercase();
    request.plot_style = normalize_plot_style(&request.plot_style)?;
    request.basemap_style = normalize_basemap_style(&request.basemap_style)?;
    request.domain_slug = safe_slug(&request.domain_slug);
    if request.model.is_empty() {
        return Err("model is required".to_string());
    }
    if request.run.trim().is_empty() {
        return Err("run is required".to_string());
    }
    if request.products.trim().is_empty() {
        return Err("products is required".to_string());
    }
    if !matches!(request.output_format.as_str(), "png" | "webp" | "png-webp") {
        return Err("output_format must be png, webp, or png-webp".to_string());
    }
    if request.place_label_density > 4 {
        return Err("place_label_density must be 0, 1, 2, 3, or 4".to_string());
    }
    if request.place_label_size > 3 {
        return Err("place_label_size must be 0, 1, 2, or 3".to_string());
    }
    if let Some(points) = &request.perimeter {
        let ring: Vec<(f64, f64)> = points.iter().map(|point| (point[0], point[1])).collect();
        let options = perimeter::PerimeterDomainOptions {
            padding_km: request.padding_km.unwrap_or(50.0),
            extend: request.extend.map(|extend| perimeter::PerimeterExtension {
                direction_deg: extend.direction_deg,
                distance_km: extend.distance_km,
            }),
            // Only an explicit width x height request pins the aspect; a
            // width-only preview keeps the natural padded box and derives
            // its height from the computed bounds in output_size.
            aspect: match (request.output_width, request.output_height) {
                (Some(width), Some(height)) if width > 0 && height > 0 => {
                    Some(f64::from(width.clamp(MIN_RENDER_WIDTH, MAX_RENDER_DIMENSION))
                        / f64::from(height.clamp(MIN_RENDER_HEIGHT, MAX_RENDER_DIMENSION)))
                }
                _ => None,
            },
            ..perimeter::PerimeterDomainOptions::default()
        };
        request.bounds = Some(perimeter::perimeter_domain_bounds(&ring, &options)?);
    }
    let Some([west, east, south, north]) = request.bounds else {
        return Err("provide bounds [west,east,south,north] or a perimeter".to_string());
    };
    if !west.is_finite() || !east.is_finite() || !south.is_finite() || !north.is_finite() {
        return Err("bounds must be finite west,east,south,north values".to_string());
    }
    if west < -360.0 || east > 360.0 || south < -90.0 || north > 90.0 || west >= east
        || south >= north
    {
        return Err(format!(
            "bounds out of range: got [{west},{east},{south},{north}]"
        ));
    }
    let lon_span = (east - west).abs();
    let lat_span = (north - south).abs();
    if lon_span < 0.02 || lat_span < 0.02 {
        return Err("drawn box is too small".to_string());
    }
    if lon_span > 80.0 || lat_span > 50.0 {
        return Err("drawn box is too large for the local fire render API".to_string());
    }
    Ok(request)
}

fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];
    let header_end = loop {
        let n = stream.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            return Err("client closed before headers".to_string());
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(pos) = find_header_end(&bytes) {
            break pos;
        }
        if bytes.len() > 64 * 1024 {
            return Err("headers too large".to_string());
        }
    };
    let header_text = std::str::from_utf8(&bytes[..header_end])
        .map_err(|err| format!("headers are not utf-8: {err}"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines.next().ok_or("missing request line")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or_default().to_string();
    let raw_path = request_parts.next().unwrap_or("/").to_string();
    let (path, query) = match raw_path.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (raw_path, String::new()),
    };
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = checked_content_length(value)?;
            }
        }
    }
    let body_start = header_end + 4;
    while bytes.len() < body_start + content_length {
        let n = stream.read(&mut buf).map_err(|err| err.to_string())?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let body = bytes
        .get(body_start..body_start + content_length)
        .unwrap_or_default()
        .to_vec();
    Ok(HttpRequest {
        method,
        path,
        query,
        body,
    })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn json_response(value: &impl Serialize) -> Vec<u8> {
    json_status_response(200, value)
}

/// Minimal query-string parse for the meteogram GET (handles %XX and '+').
fn parse_query(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        out.insert(url_decode(key), url_decode(value));
    }
    out
}

fn url_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => out.push(b' '),
            b'%' => {
                if let Some(decoded) = bytes
                    .get(index + 1..index + 3)
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    out.push(decoded);
                    index += 2;
                } else {
                    out.push(b'%');
                }
            }
            other => out.push(other),
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// GET /api/meteogram?lat=..&lon=..&run=20260701_00z[&model=hrrr]
/// [&panels=temp,rh,vpd,wind,fuels,smoke][&title=...] -> inline SVG.
fn meteogram_response(path: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(path);
    let bad = |message: &str| json_status_response(400, &serde_json::json!({ "error": message }));
    let Some(lat) = query.get("lat").and_then(|v| v.parse::<f64>().ok()) else {
        return bad("lat is required");
    };
    let Some(lon) = query.get("lon").and_then(|v| v.parse::<f64>().ok()) else {
        return bad("lon is required");
    };
    let Some(run) = query.get("run").map(String::as_str) else {
        return bad("run is required (e.g. 20260701_00z or latest)");
    };
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return bad("run slug is not valid");
    }
    let model = query.get("model").map(String::as_str).unwrap_or("hrrr");
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return bad("model slug is not valid");
    }
    let resolved_run;
    let alias = run.to_ascii_lowercase();
    let run = if alias == "latest" || alias == "latest-day" {
        match resolve_latest_run(&state.store_root, model, alias == "latest-day") {
            Ok(resolved) => {
                resolved_run = resolved;
                resolved_run.as_str()
            }
            Err(message) => return json_status_response(422, &serde_json::json!({ "error": message })),
        }
    } else {
        run
    };
    let Some((date, cycle)) = run.split_once('_').and_then(|(date, cycle)| {
        let hour: u8 = cycle.strip_suffix('z').or_else(|| cycle.strip_suffix('Z'))?.parse().ok()?;
        (date.len() == 8 && date.chars().all(|c| c.is_ascii_digit()) && hour <= 23)
            .then(|| (date.to_string(), hour))
    }) else {
        return bad("run must look like 20260701_00z");
    };
    let panels: Vec<String> = query
        .get("panels")
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let request = meteogram::MeteogramRequest {
        lat,
        lon,
        panels,
        title: query.get("title").cloned().filter(|t| !t.trim().is_empty()),
        utc_offset_hours: query
            .get("utc_offset")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-14.0..=14.0).contains(v))
            .unwrap_or(-7.0),
    };
    match meteogram::render_meteogram_svg(&state.store_root, model, run, &date, cycle, &request) {
        Ok(output) => {
            if query.get("format").map(String::as_str) == Some("json") {
                json_response(&output.data)
            } else {
                response_with_extra_headers(
                    200,
                    "image/svg+xml; charset=utf-8",
                    output.svg.into_bytes(),
                    "Cache-Control: no-store\r\n",
                )
            }
        }
        Err(message) => json_status_response(422, &serde_json::json!({ "error": message })),
    }
}

fn json_status_response(status: u16, value: &impl Serialize) -> Vec<u8> {
    let body =
        serde_json::to_vec_pretty(value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec());
    response(status, "application/json; charset=utf-8", body)
}

fn html_response(body: &str) -> Vec<u8> {
    response_with_extra_headers(
        200,
        "text/html; charset=utf-8",
        body.as_bytes().to_vec(),
        "Cache-Control: no-store\r\n",
    )
}

fn text_response(status: u16, body: &str) -> Vec<u8> {
    response(
        status,
        "text/plain; charset=utf-8",
        body.as_bytes().to_vec(),
    )
}

fn binary_response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    response(status, content_type, body)
}

fn empty_response(status: u16) -> Vec<u8> {
    response(status, "text/plain; charset=utf-8", Vec::new())
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> Vec<u8> {
    response_with_extra_headers(status, content_type, body, "")
}

fn response_with_extra_headers(
    status: u16,
    content_type: &str,
    body: Vec<u8>,
    extra_headers: &str,
) -> Vec<u8> {
    let status_text = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET,POST,OPTIONS\r\n{extra_headers}Connection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend(body);
    out
}

fn next_job_id(state: &AppState) -> String {
    let count = state.counter.fetch_add(1, Ordering::Relaxed);
    format!("job-{}-{count}", unix_ms_now())
}

fn unix_ms_now() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn sibling_rw_render_path() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from(format!("rw_render{}", std::env::consts::EXE_SUFFIX)));
    path.set_file_name(format!("rw_render{}", std::env::consts::EXE_SUFFIX));
    path
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains("..")
        && !value.contains('/')
        && !value.contains('\\')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn safe_slug(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn safe_model_slug(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .collect()
}

fn output_size(request: &RenderJobRequest) -> (u32, u32) {
    let [west, east, south, north] = request.resolved_bounds();
    let aspect = ((east - west).abs().max(0.1) / (north - south).abs().max(0.1)).clamp(0.45, 2.2);
    match (
        request
            .output_width
            .map(|width| width.clamp(MIN_RENDER_WIDTH, MAX_RENDER_DIMENSION)),
        request
            .output_height
            .map(|height| height.clamp(MIN_RENDER_HEIGHT, MAX_RENDER_DIMENSION)),
    ) {
        (Some(width), Some(height)) => return (width, height),
        (Some(width), None) => {
            let height = (f64::from(width) / aspect).round().clamp(
                f64::from(MIN_RENDER_HEIGHT),
                f64::from(MAX_RENDER_DIMENSION),
            ) as u32;
            return (width, height);
        }
        (None, Some(height)) => {
            let width = (f64::from(height) * aspect)
                .round()
                .clamp(f64::from(MIN_RENDER_WIDTH), f64::from(MAX_RENDER_DIMENSION))
                as u32;
            return (width, height);
        }
        (None, None) => {}
    }
    if aspect >= 1.0 {
        let width = 1600u32;
        let height = (f64::from(width) / aspect).round().clamp(900.0, 1600.0) as u32;
        (width, height)
    } else {
        let height = 1500u32;
        let width = (f64::from(height) * aspect).round().clamp(900.0, 1600.0) as u32;
        (width, height)
    }
}

fn render_cache_key(request: &RenderJobRequest) -> String {
    let (width, height) = output_size(request);
    let perimeter_part = match &request.perimeter {
        Some(points) => {
            let ring: Vec<(f64, f64)> = points.iter().map(|point| (point[0], point[1])).collect();
            let extend_part = request
                .extend
                .map(|extend| format!("{:.2}@{:.2}", extend.distance_km, extend.direction_deg))
                .unwrap_or_else(|| "-".to_string());
            format!(
                "{}+{:.2}+{}+{}",
                perimeter::perimeter_hash(&ring),
                request.padding_km.unwrap_or(50.0),
                extend_part,
                request.overlay_perimeter_enabled(),
            )
        }
        None => "-".to_string(),
    };
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}x{}",
        request.model,
        request.run,
        request.hour,
        request.products.trim(),
        request.output_format,
        request.plot_style,
        request.basemap_style,
        request.county_linework,
        request.place_label_density,
        request.place_label_size,
        request.domain_slug,
        format_bounds(request.resolved_bounds()),
        perimeter_part,
        request.title_note.as_deref().unwrap_or("-"),
        width,
        height,
    )
}

fn format_bounds(bounds: [f64; 4]) -> String {
    format!(
        "{:.6},{:.6},{:.6},{:.6}",
        bounds[0], bounds[1], bounds[2], bounds[3]
    )
}

/// Parse a Content-Length header value, rejecting bodies the service must
/// not buffer (the render API's biggest legitimate body is perimeter
/// GeoJSON, well under [`MAX_REQUEST_BODY_BYTES`]).
fn checked_content_length(value: &str) -> Result<usize, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    let length = trimmed
        .parse::<usize>()
        .map_err(|_| format!("invalid content-length '{trimmed}'"))?;
    if length > MAX_REQUEST_BODY_BYTES {
        return Err(format!(
            "request body of {length} bytes exceeds the {MAX_REQUEST_BODY_BYTES} byte limit"
        ));
    }
    Ok(length)
}

/// Run a child process with a hard deadline: a child that outlives it is
/// killed so a hung renderer can never pin a render permit forever.
fn run_command_with_deadline(
    command: &mut Command,
    deadline: Duration,
) -> Result<std::process::ExitStatus, String> {
    let mut child = command
        .spawn()
        .map_err(|err| format!("spawn failed: {err}"))?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {
                if started.elapsed() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "timed out after {:.0}s and was killed",
                        deadline.as_secs_f64()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(err) => return Err(format!("wait failed: {err}")),
        }
    }
}

/// Last lines of a child log file, for job status reporting.
fn read_log_tail(path: &Path) -> String {
    fs::read_to_string(path)
        .map(|content| tail_lines(&content, 24))
        .unwrap_or_default()
}

fn tail_lines(value: &str, max_lines: usize) -> String {
    let lines = value.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

fn default_model() -> String {
    "hrrr".to_string()
}

fn default_run() -> String {
    "20260629_03z".to_string()
}

fn default_hour() -> u16 {
    3
}

fn default_products() -> String {
    "cafire-with-fuels".to_string()
}

fn default_output_format() -> String {
    "png".to_string()
}

fn default_plot_style() -> String {
    "operational-fast".to_string()
}

fn default_basemap_style() -> String {
    "topo".to_string()
}

fn default_county_linework() -> bool {
    true
}

fn default_place_label_density() -> u8 {
    4
}

fn default_place_label_size() -> u8 {
    2
}

fn default_domain_slug() -> String {
    "drawn_box".to_string()
}

fn normalize_plot_style(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match normalized.as_str() {
        "" | "default" | "operational-fast" | "ops-fast" | "weathermodels-fast"
        | "pivotal-fast" => "operational-fast",
        "operational" | "ops" | "weathermodels" | "reference" => "operational",
        "operational-quality"
        | "operational-quality-2x"
        | "ops-quality"
        | "weathermodels-quality"
        | "pivotal-quality" => "operational-quality-2x",
        "operational-budget-30s"
        | "operational-budget"
        | "ops-budget"
        | "budget-30s"
        | "quality-budget"
        | "operational-best"
        | "ops-best"
        | "weathermodels-best"
        | "max-quality" => "operational-budget-30s",
        "clean" | "atlas" | "clean-atlas" | "pivotal" => "clean-atlas",
        "fast" | "clean-fast" | "atlas-fast" | "clean-atlas-fast" | "production"
        | "rusty-weather" | "rusty-weather-fast" => "clean-atlas-fast",
        "quality"
        | "quality-2x"
        | "beauty"
        | "export"
        | "clean-quality"
        | "clean-quality-2x"
        | "clean-atlas-quality"
        | "clean-atlas-quality-2x" => "clean-atlas-quality-2x",
        "combined"
        | "clean-combined"
        | "atlas-combined"
        | "clean-atlas-combined"
        | "presentation"
        | "best" => "clean-atlas-combined",
        other => {
            return Err(format!(
                "plot_style must be operational-fast, clean-atlas-fast, clean-atlas, clean-atlas-combined, or another supported static plot style; got {other}"
            ));
        }
    };
    Ok(canonical.to_string())
}

fn normalize_basemap_style(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase().replace('_', "-");
    let canonical = match normalized.as_str() {
        "" | "default" | "filled" | "fill" | "color" | "colored" | "land-ocean"
        | "rusty-weather" | "clean-atlas" => "filled",
        "white" | "nws" | "plain" | "outline" => "white",
        "topo" | "topographic" | "terrain" | "terrain-tint" | "relief" => "topo",
        other => {
            return Err(format!(
                "basemap_style must be filled, white, or topo; got {other}"
            ));
        }
    };
    Ok(canonical.to_string())
}

fn place_label_render_env(size: u8) -> (&'static str, &'static str) {
    match size {
        0 => ("0.90", "1.00"),
        1 => ("1.00", "1.05"),
        2 => ("1.28", "1.18"),
        _ => ("1.55", "1.30"),
    }
}

/// The CAFire Weather Ops console — the upgraded weather page.
const SITE_HTML: &str = include_str!("../fire_site.html");
/// CAFire-styled public preview page (tester-facing, served at `/`).
const PREVIEW_HTML: &str = include_str!("../cafire_preview.html");

const DEMO_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Rusty Fire Weather Local Demo</title>
<style>
  :root { color-scheme: light; font-family: ui-sans-serif, system-ui, -apple-system, Segoe UI, sans-serif; }
  body { margin: 0; background: #f6f7f4; color: #17202a; }
  main { display: grid; grid-template-columns: minmax(360px, 520px) 1fr; gap: 18px; padding: 18px; }
  section { background: #fff; border: 1px solid #d9ded6; border-radius: 8px; padding: 14px; }
  h1 { margin: 0 0 10px; font-size: 22px; }
  h2 { margin: 0 0 10px; font-size: 15px; }
  label { display: grid; gap: 4px; margin: 9px 0; font-size: 13px; font-weight: 650; }
  input, select, button { font: inherit; }
  input, select { padding: 8px; border: 1px solid #c7cec4; border-radius: 6px; }
  label.check { display: flex; align-items: center; gap: 8px; }
  label.check input { width: 18px; height: 18px; }
  button { border: 0; border-radius: 6px; padding: 9px 11px; background: #ba2430; color: white; font-weight: 700; cursor: pointer; }
  button.secondary { background: #315b6f; }
  button:disabled { background: #a9b0a6; cursor: wait; }
  canvas { width: 100%; max-width: 900px; aspect-ratio: 1.45; background: #edf1ea; border: 1px solid #9aa89a; border-radius: 6px; touch-action: none; }
  .row { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
  .actions { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 10px; }
  .status { white-space: pre-wrap; background: #182025; color: #e9f0ea; border-radius: 6px; padding: 10px; min-height: 90px; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; font-size: 12px; }
  .gallery { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 12px; margin-top: 12px; }
  .gallery img { width: 100%; border: 1px solid #d0d7ce; border-radius: 6px; background: white; }
  .muted { color: #5d6871; font-size: 12px; }
  @media (max-width: 900px) { main { grid-template-columns: 1fr; } }
</style>
</head>
<body>
<main>
  <section>
    <h1>Rusty Fire Weather</h1>
    <p class="muted">Local-only draw-a-box CAFire render demo. This calls the Rust API on this machine and renders from rw-store .rws files.</p>
    <div class="row">
      <label>Model <input id="model" value="hrrr"></label>
      <label>Run <input id="run" value="20260629_03z"></label>
    </div>
    <div class="row">
      <label>Hour <input id="hour" type="number" min="0" max="384" value="3"></label>
      <label>Preview width <input id="outputWidth" type="number" min="1200" max="2400" value="1400"></label>
    </div>
    <div class="row">
      <label>Products
        <select id="products">
          <option value="cafire-with-fuels" selected>cafire-with-fuels</option>
          <option value="cafire-expanded-with-fuels">cafire-expanded-with-fuels</option>
          <option value="cafire-core">cafire-core</option>
          <option value="cafire-all">cafire-all</option>
          <option value="cafire-expanded">cafire-expanded</option>
          <option value="cafire-fuels">cafire-fuels</option>
          <option value="cafire-fuel-layers">cafire-fuel-layers</option>
          <option value="cafire-fuel-composites">cafire-fuel-composites</option>
        </select>
      </label>
      <label>Format
        <select id="outputFormat">
          <option value="webp">webp</option>
          <option value="png">png</option>
          <option value="png-webp">png + webp</option>
        </select>
      </label>
    </div>
    <div class="row">
      <label>Basemap / plot style
        <select id="plotStyle">
          <option value="operational-fast" selected>CAFire operational fast</option>
          <option value="clean-atlas-fast">Rusty Weather clean atlas fast</option>
          <option value="clean-atlas">Rusty Weather clean atlas</option>
          <option value="clean-atlas-combined">Rusty Weather clean atlas best</option>
          <option value="operational-quality-2x">CAFire operational quality 2x</option>
        </select>
      </label>
      <label>Map fill
        <select id="basemapStyle">
          <option value="topo" selected>topo terrain tint</option>
          <option value="filled">filled land/ocean</option>
          <option value="white">white / NWS-style</option>
        </select>
      </label>
    </div>
    <div class="row">
      <label>Place labels
        <select id="placeLabelDensity">
          <option value="0">off</option>
          <option value="1">major cities</option>
          <option value="2">major + regional cities</option>
          <option value="3">dense local / tiny places</option>
          <option value="4" selected>max tiny towns</option>
        </select>
      </label>
      <label>Label size
        <select id="placeLabelSize">
          <option value="0">small</option>
          <option value="1">normal</option>
          <option value="2" selected>large</option>
          <option value="3">huge</option>
        </select>
      </label>
    </div>
    <label class="check"><input id="countyLinework" type="checkbox" checked> Show county lines</label>
    <label>Domain name <input id="domainSlug" value="drawn_box"></label>
    <div class="row">
      <label>West <input id="west" type="number" step="0.01" value="-123.50"></label>
      <label>East <input id="east" type="number" step="0.01" value="-120.25"></label>
      <label>South <input id="south" type="number" step="0.01" value="37.00"></label>
      <label>North <input id="north" type="number" step="0.01" value="39.50"></label>
    </div>
    <div class="actions">
      <button class="secondary" id="cali">CAFire California</button>
      <button class="secondary" id="wide">CAFire Wide West</button>
      <button id="render">Render Box</button>
    </div>
  </section>
  <section>
    <h2>Draw A Box</h2>
    <canvas id="map" width="900" height="620"></canvas>
    <p class="muted">Drag on the canvas. The sketch is coordinate-accurate enough for a job payload; the Rust renderer does the real map projection.</p>
  </section>
  <section style="grid-column: 1 / -1;">
    <h2>Job</h2>
    <div class="status" id="status">ready</div>
    <div class="gallery" id="gallery"></div>
  </section>
</main>
<script>
const world = {west: -126.5, east: -103.5, south: 31.0, north: 47.0};
const canvas = document.getElementById('map');
const ctx = canvas.getContext('2d');
let dragStart = null;

function input(id) { return document.getElementById(id); }
function bounds() {
  return {
    west: Number(input('west').value),
    east: Number(input('east').value),
    south: Number(input('south').value),
    north: Number(input('north').value)
  };
}
function setBounds(b, name) {
  input('west').value = b.west.toFixed(2);
  input('east').value = b.east.toFixed(2);
  input('south').value = b.south.toFixed(2);
  input('north').value = b.north.toFixed(2);
  if (name) input('domainSlug').value = name;
  draw();
}
function xForLon(lon) { return (lon - world.west) / (world.east - world.west) * canvas.width; }
function yForLat(lat) { return (world.north - lat) / (world.north - world.south) * canvas.height; }
function lonForX(x) { return world.west + x / canvas.width * (world.east - world.west); }
function latForY(y) { return world.north - y / canvas.height * (world.north - world.south); }
function draw() {
  ctx.clearRect(0, 0, canvas.width, canvas.height);
  ctx.fillStyle = '#eef3ec';
  ctx.fillRect(0, 0, canvas.width, canvas.height);
  ctx.strokeStyle = '#c7d0c3';
  ctx.lineWidth = 1;
  for (let lon = -126; lon <= -104; lon += 2) {
    const x = xForLon(lon);
    ctx.beginPath(); ctx.moveTo(x, 0); ctx.lineTo(x, canvas.height); ctx.stroke();
  }
  for (let lat = 32; lat <= 46; lat += 2) {
    const y = yForLat(lat);
    ctx.beginPath(); ctx.moveTo(0, y); ctx.lineTo(canvas.width, y); ctx.stroke();
  }
  ctx.strokeStyle = '#516252';
  ctx.lineWidth = 3;
  roughPolyline([[-124.4,42],[-123.7,40.5],[-122.7,38.8],[-122.4,37.8],[-121.9,36.7],[-121.2,35.6],[-120.4,34.6],[-118.6,34.0],[-117.1,32.7]]);
  roughPolyline([[-120.0,42.0],[-119.8,40],[-119.9,38.5],[-119.5,36.8],[-118.5,35.2],[-117.3,34.2],[-114.7,32.7]]);
  ctx.fillStyle = '#394640';
  ctx.font = '15px system-ui';
  ctx.fillText('California / Wide West sketch', 14, 24);
  const b = bounds();
  const x1 = xForLon(b.west), x2 = xForLon(b.east);
  const y1 = yForLat(b.north), y2 = yForLat(b.south);
  ctx.fillStyle = 'rgba(186,36,48,0.14)';
  ctx.strokeStyle = '#ba2430';
  ctx.lineWidth = 3;
  ctx.fillRect(x1, y1, x2 - x1, y2 - y1);
  ctx.strokeRect(x1, y1, x2 - x1, y2 - y1);
}
function roughPolyline(points) {
  ctx.beginPath();
  points.forEach(([lon, lat], i) => {
    const x = xForLon(lon), y = yForLat(lat);
    if (i === 0) ctx.moveTo(x, y); else ctx.lineTo(x, y);
  });
  ctx.stroke();
}
function pointerPos(event) {
  const r = canvas.getBoundingClientRect();
  return {x: (event.clientX - r.left) * canvas.width / r.width, y: (event.clientY - r.top) * canvas.height / r.height};
}
canvas.addEventListener('pointerdown', event => { dragStart = pointerPos(event); canvas.setPointerCapture(event.pointerId); });
canvas.addEventListener('pointermove', event => {
  if (!dragStart) return;
  const p = pointerPos(event);
  const west = Math.min(lonForX(dragStart.x), lonForX(p.x));
  const east = Math.max(lonForX(dragStart.x), lonForX(p.x));
  const south = Math.min(latForY(dragStart.y), latForY(p.y));
  const north = Math.max(latForY(dragStart.y), latForY(p.y));
  setBounds({west, east, south, north}, input('domainSlug').value || 'drawn_box');
});
canvas.addEventListener('pointerup', () => { dragStart = null; });
document.getElementById('cali').onclick = () => setBounds({west:-126.0,east:-113.8,south:31.9,north:42.5}, 'cafire_california');
document.getElementById('wide').onclick = () => setBounds({west:-125.7,east:-103.8,south:31.9,north:46.5}, 'cafire_wide_west');
document.getElementById('render').onclick = async () => {
  const button = document.getElementById('render');
  button.disabled = true;
  document.getElementById('gallery').innerHTML = '';
  const b = bounds();
  const payload = {
    model: input('model').value,
    run: input('run').value,
    hour: Number(input('hour').value),
    products: input('products').value,
    output_format: input('outputFormat').value,
    plot_style: input('plotStyle').value,
    basemap_style: input('basemapStyle').value,
    county_linework: input('countyLinework').checked,
    place_label_density: Number(input('placeLabelDensity').value),
    place_label_size: Number(input('placeLabelSize').value),
    domain_slug: input('domainSlug').value,
    bounds: [b.west, b.east, b.south, b.north]
  };
  const outputWidth = Number(input('outputWidth').value);
  if (Number.isFinite(outputWidth) && outputWidth > 0) payload.output_width = outputWidth;
  setStatus('submitting job\\n' + JSON.stringify(payload, null, 2));
  try {
    const started = await fetch('/api/render', {method:'POST', headers:{'content-type':'application/json'}, body: JSON.stringify(payload)}).then(r => r.json());
    await pollJob(started.id);
  } catch (err) {
    setStatus('request failed: ' + err);
  } finally {
    button.disabled = false;
  }
};
async function pollJob(id) {
  while (true) {
    const job = await fetch('/api/jobs/' + encodeURIComponent(id)).then(r => r.json());
    setStatus(JSON.stringify(job, null, 2));
    if (job.state === 'succeeded' || job.state === 'failed') {
      showFiles(job.files || []);
      return;
    }
    await new Promise(resolve => setTimeout(resolve, 1000));
  }
}
function showFiles(files) {
  const gallery = document.getElementById('gallery');
  gallery.innerHTML = '';
  files.forEach(file => {
    const a = document.createElement('a');
    a.href = file.url;
    a.target = '_blank';
    const img = document.createElement('img');
    img.src = file.url;
    img.alt = file.name;
    a.appendChild(img);
    gallery.appendChild(a);
  });
}
function setStatus(text) { document.getElementById('status').textContent = text; }
draw();
</script>
</body>
</html>
"#;

#[cfg(test)]
mod tests {
    use super::*;

    /// A validating draw-a-box request; tests override what they probe.
    fn base_request() -> RenderJobRequest {
        RenderJobRequest {
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-core".to_string(),
            output_format: "webp".to_string(),
            plot_style: default_plot_style(),
            basemap_style: default_basemap_style(),
            county_linework: default_county_linework(),
            place_label_density: default_place_label_density(),
            place_label_size: default_place_label_size(),
            domain_slug: "box".to_string(),
            bounds: Some([-123.21, -119.67, 37.13, 41.14]),
            perimeter: None,
            padding_km: None,
            extend: None,
            overlay_perimeter: None,
            title_note: None,
            output_width: None,
            output_height: None,
        }
    }

    /// A ~20 km wide synthetic fire perimeter near Paradise, CA.
    fn sample_perimeter() -> Vec<[f64; 2]> {
        vec![
            [-121.7, 39.6],
            [-121.5, 39.7],
            [-121.4, 39.5],
            [-121.6, 39.4],
        ]
    }

    #[test]
    fn safe_slug_removes_path_punctuation() {
        assert_eq!(safe_slug("Napa Box / 01"), "napa_box_01");
    }

    #[test]
    fn safe_model_slug_preserves_model_hyphens() {
        assert_eq!(safe_model_slug("ecmwf-open-data"), "ecmwf-open-data");
        assert_eq!(safe_model_slug("hrrr-ak"), "hrrr-ak");
    }

    #[test]
    fn format_bounds_is_stable_for_renderer_cli() {
        assert_eq!(
            format_bounds([-123.5, -120.25, 37.0, 39.5]),
            "-123.500000,-120.250000,37.000000,39.500000"
        );
    }

    #[test]
    fn request_validation_rejects_inverted_boxes() {
        let request = RenderJobRequest {
            domain_slug: "bad".to_string(),
            bounds: Some([-123.0, -120.0, 40.0, 37.0]),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn request_validation_rejects_reversed_west_east() {
        let request = RenderJobRequest {
            bounds: Some([-119.67, -123.21, 37.13, 41.14]),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn request_validation_rejects_unknown_output_format() {
        let request = RenderJobRequest {
            output_format: "jpeg".to_string(),
            output_width: Some(800),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn request_validation_requires_bounds_or_perimeter() {
        let request = RenderJobRequest {
            bounds: None,
            ..base_request()
        };
        let message = validate_render_request(request).unwrap_err();
        assert!(message.contains("bounds"), "unexpected error: {message}");
        assert!(message.contains("perimeter"), "unexpected error: {message}");
    }

    #[test]
    fn perimeter_requests_compute_padded_bounds() {
        let request = RenderJobRequest {
            bounds: None,
            perimeter: Some(sample_perimeter()),
            padding_km: Some(50.0),
            ..base_request()
        };
        let validated = validate_render_request(request).expect("perimeter request validates");
        let [west, east, south, north] = validated.resolved_bounds();
        for point in sample_perimeter() {
            assert!(point[0] > west && point[0] < east, "lon {}", point[0]);
            assert!(point[1] > south && point[1] < north, "lat {}", point[1]);
        }
        // 50 km padding is roughly 0.45 degrees latitude beyond the ring.
        assert!(north - 39.7 > 0.40, "north edge too close: {north}");
        assert!(39.4 - south > 0.40, "south edge too close: {south}");
    }

    #[test]
    fn perimeter_requests_reject_bad_rings() {
        let request = RenderJobRequest {
            bounds: None,
            perimeter: Some(vec![[-121.7, 39.6], [-121.5, 39.7]]),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
        let request = RenderJobRequest {
            bounds: None,
            perimeter: Some(sample_perimeter()),
            padding_km: Some(9000.0),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn perimeter_overlay_defaults_on_and_can_be_disabled() {
        let on = RenderJobRequest {
            bounds: None,
            perimeter: Some(sample_perimeter()),
            ..base_request()
        };
        assert!(on.overlay_perimeter_enabled());
        let off = RenderJobRequest {
            overlay_perimeter: Some(false),
            ..on.clone()
        };
        assert!(!off.overlay_perimeter_enabled());
        let boxed = base_request();
        assert!(!boxed.overlay_perimeter_enabled());
    }

    #[test]
    fn perimeter_options_change_the_cache_key() {
        let base = validate_render_request(RenderJobRequest {
            bounds: None,
            perimeter: Some(sample_perimeter()),
            padding_km: Some(50.0),
            ..base_request()
        })
        .unwrap();
        let padded = validate_render_request(RenderJobRequest {
            padding_km: Some(100.0),
            ..base.clone()
        })
        .unwrap();
        let extended = validate_render_request(RenderJobRequest {
            extend: Some(ExtendRequest {
                direction_deg: 65.0,
                distance_km: 80.0,
            }),
            ..base.clone()
        })
        .unwrap();
        let no_overlay = validate_render_request(RenderJobRequest {
            overlay_perimeter: Some(false),
            ..base.clone()
        })
        .unwrap();
        let same = validate_render_request(base.clone()).unwrap();
        assert_eq!(render_cache_key(&base), render_cache_key(&same));
        assert_ne!(render_cache_key(&base), render_cache_key(&padded));
        assert_ne!(render_cache_key(&base), render_cache_key(&extended));
        assert_ne!(render_cache_key(&base), render_cache_key(&no_overlay));
    }

    #[test]
    fn output_size_derives_height_from_width_only_preview() {
        let request = RenderJobRequest {
            output_width: Some(1000),
            ..base_request()
        };
        assert_eq!(output_size(&request), (1200, 1359));
    }

    #[test]
    fn output_size_keeps_explicit_dimensions() {
        let request = RenderJobRequest {
            output_width: Some(1000),
            output_height: Some(700),
            ..base_request()
        };
        assert_eq!(output_size(&request), (1200, 900));
    }

    #[test]
    fn render_cache_key_uses_clamped_output_size() {
        let small = RenderJobRequest {
            products: "cafire-with-fuels".to_string(),
            output_width: Some(800),
            ..base_request()
        };
        let clamped = RenderJobRequest {
            output_width: Some(1200),
            ..small.clone()
        };
        assert_eq!(render_cache_key(&small), render_cache_key(&clamped));
    }

    #[test]
    fn request_validation_normalizes_map_style_options() {
        let request = RenderJobRequest {
            plot_style: "rusty_weather".to_string(),
            basemap_style: "NWS".to_string(),
            county_linework: false,
            place_label_density: 3,
            place_label_size: 3,
            output_width: Some(1400),
            ..base_request()
        };
        let validated = validate_render_request(request).expect("request should validate");
        assert_eq!(validated.plot_style, "clean-atlas-fast");
        assert_eq!(validated.basemap_style, "white");
        assert!(!validated.county_linework);
        assert_eq!(validated.place_label_density, 3);
        assert_eq!(validated.place_label_size, 3);
    }

    #[test]
    fn request_validation_normalizes_topo_basemap_aliases() {
        let request = RenderJobRequest {
            basemap_style: "terrain".to_string(),
            output_width: Some(1400),
            ..base_request()
        };
        let validated = validate_render_request(request).expect("request should validate");
        assert_eq!(validated.basemap_style, "topo");
    }

    #[test]
    fn render_cache_key_changes_for_map_style_options() {
        let base = RenderJobRequest {
            products: "cafire-with-fuels".to_string(),
            plot_style: "operational-fast".to_string(),
            basemap_style: "filled".to_string(),
            place_label_density: 1,
            place_label_size: 1,
            output_width: Some(1200),
            ..base_request()
        };
        let clean = RenderJobRequest {
            plot_style: "clean-atlas-fast".to_string(),
            ..base.clone()
        };
        let white = RenderJobRequest {
            basemap_style: "white".to_string(),
            ..base.clone()
        };
        let no_counties = RenderJobRequest {
            county_linework: false,
            ..base.clone()
        };
        let dense_places = RenderJobRequest {
            place_label_density: 3,
            ..base.clone()
        };
        let huge_places = RenderJobRequest {
            place_label_size: 3,
            ..base.clone()
        };
        assert_ne!(render_cache_key(&base), render_cache_key(&clean));
        assert_ne!(render_cache_key(&base), render_cache_key(&white));
        assert_ne!(render_cache_key(&base), render_cache_key(&no_counties));
        assert_ne!(render_cache_key(&base), render_cache_key(&dense_places));
        assert_ne!(render_cache_key(&base), render_cache_key(&huge_places));
    }

    #[test]
    fn request_validation_rejects_unknown_place_label_density() {
        let request = RenderJobRequest {
            place_label_density: 5,
            output_width: Some(1400),
            ..base_request()
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn render_gate_tracks_active_permits() {
        let gate = Arc::new(RenderGate::new(1));
        assert_eq!(gate.snapshot()["active"], 0);
        let permit = gate.acquire();
        assert_eq!(gate.snapshot()["active"], 1);
        drop(permit);
        assert_eq!(gate.snapshot()["active"], 0);
    }

    #[test]
    fn content_length_is_capped_before_buffering() {
        assert_eq!(checked_content_length("128"), Ok(128));
        assert_eq!(checked_content_length(" 128 "), Ok(128));
        assert_eq!(checked_content_length(""), Ok(0));
        assert_eq!(
            checked_content_length(&MAX_REQUEST_BODY_BYTES.to_string()),
            Ok(MAX_REQUEST_BODY_BYTES)
        );
        assert!(checked_content_length(&(MAX_REQUEST_BODY_BYTES + 1).to_string()).is_err());
        assert!(checked_content_length("8000000000").is_err());
        assert!(checked_content_length("not-a-number").is_err());
        assert!(checked_content_length("-5").is_err());
    }

    #[test]
    fn fast_children_finish_within_the_deadline() {
        let mut command = Command::new("cmd");
        command.args(["/C", "exit", "0"]);
        let status = run_command_with_deadline(&mut command, Duration::from_secs(30))
            .expect("fast child should complete");
        assert!(status.success());
    }

    #[test]
    fn hung_children_are_killed_at_the_deadline() {
        // `ping -n 60` idles for ~59 seconds; the 500 ms deadline must kill it.
        let mut command = Command::new("cmd");
        command.args(["/C", "ping -n 60 127.0.0.1 > NUL"]);
        let started = Instant::now();
        let result = run_command_with_deadline(&mut command, Duration::from_millis(500));
        let waited = started.elapsed();
        let message = result.expect_err("hung child must be killed, not awaited");
        assert!(
            message.contains("timed out"),
            "unexpected error: {message}"
        );
        assert!(
            waited < Duration::from_secs(10),
            "kill took too long: {waited:?}"
        );
    }
}

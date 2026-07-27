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

#[path = "../xsection.rs"]
mod xsection;

#[path = "../sounding.rs"]
mod sounding;

#[path = "../svg_raster.rs"]
mod svg_raster;

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
    loops: Arc<Mutex<HashMap<String, LoopJob>>>,
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
    /// Pivotal-style value labels: stamp the plotted field's value at the
    /// city label points instead of the city name. Off by default.
    #[serde(default)]
    value_labels: bool,
    /// Optional plot-banner branding (title / credit / logo). When absent the
    /// render keeps the built-in CAFire banner, so existing callers are
    /// unchanged. See `BrandRequest`.
    #[serde(default)]
    brand: Option<BrandRequest>,
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
    /// Temperature display units for the surface temperature family
    /// (2 m temperature/dewpoint, wet-bulb, heat index, ...): "f"
    /// (default) or "c". Upper-air temperature maps always render °C.
    /// Normalized at validation so an omitted field and an explicit "f"
    /// share one cache key.
    #[serde(default = "default_temp_units")]
    temp_units: String,
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

/// Plot-banner branding. A named `preset` picks a base look; `title`/`credit`
/// override individual strings; `logo_b64` is a base64 PNG drawn left-aligned
/// in the banner strip.
///
/// Presets:
/// - `cafire` (or absent) — the built-in CAFire banner (unchanged behavior).
/// - `generic` — no left title, right credit `wxsection.com`.
/// - `none` — blank strip (no title/credit; logo still drawn if supplied).
/// - `custom` — start blank; use `title`/`credit`/`logo_b64` verbatim.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct BrandRequest {
    #[serde(default)]
    preset: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    credit: Option<String>,
    #[serde(default)]
    logo_b64: Option<String>,
}

/// Branding resolved to the env values the render child reads. `None` for a
/// text field means "leave the render's built-in CAFire default"; `Some("")`
/// means "omit that element".
struct ResolvedBrand {
    title: Option<String>,
    credit: Option<String>,
    logo_png: Option<Vec<u8>>,
}

impl BrandRequest {
    fn resolve(&self) -> Result<ResolvedBrand, String> {
        let preset = self
            .preset
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        // Base strings from the preset. `None` = keep the render default.
        let (mut title, mut credit) = match preset.as_str() {
            "" | "cafire" => (None, None),
            "generic" => (Some(String::new()), Some("wxsection.com".to_string())),
            "none" => (Some(String::new()), Some(String::new())),
            "custom" => (Some(String::new()), Some(String::new())),
            other => return Err(format!("unknown brand preset: {other}")),
        };
        // Explicit strings override the preset base.
        if let Some(explicit) = &self.title {
            title = Some(sanitize_brand_text(explicit));
        }
        if let Some(explicit) = &self.credit {
            credit = Some(sanitize_brand_text(explicit));
        }
        let logo_png = match &self.logo_b64 {
            Some(encoded) if !encoded.trim().is_empty() => {
                Some(decode_brand_logo(encoded.trim())?)
            }
            _ => None,
        };
        Ok(ResolvedBrand {
            title,
            credit,
            logo_png,
        })
    }
}

/// Keep brand strings to a single sane line: no control chars, capped length.
fn sanitize_brand_text(raw: &str) -> String {
    raw.chars()
        .filter(|ch| !ch.is_control())
        .take(80)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Decode a base64 PNG logo, accepting an optional `data:image/...;base64,`
/// prefix. Caps the decoded size so a caller can't push a huge asset.
fn decode_brand_logo(encoded: &str) -> Result<Vec<u8>, String> {
    use base64::Engine;
    let payload = encoded
        .rsplit_once(";base64,")
        .map(|(_, data)| data)
        .unwrap_or(encoded);
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|err| format!("brand logo is not valid base64: {err}"))?;
    if bytes.len() > 2 * 1024 * 1024 {
        return Err("brand logo exceeds 2 MB".to_string());
    }
    if bytes.len() < 8 || bytes[..8] != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        return Err("brand logo must be a PNG".to_string());
    }
    Ok(bytes)
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
        loops: Arc::new(Mutex::new(HashMap::new())),
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
        // Public URL is `/lab/generic`: Caddy's `handle_path /lab*` strips the
        // `/lab` prefix, so it arrives here as `/generic`. `/lab-generic` is
        // kept for direct access to the API port (bypassing Caddy).
        ("GET", "/generic") | ("GET", "/lab-generic") => html_response(GENERIC_LAB_HTML),
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
        ("POST", "/api/loop") => start_loop_job(request.body, state),
        ("POST", "/api/card-logo") => store_card_logo(request.body, &state),
        _ if request.method == "GET" && request.path.starts_with("/api/loops/") => {
            loop_response(request.path.trim_start_matches("/api/loops/"), &state)
        }
        ("OPTIONS", _) => empty_response(204),
        _ if request.method == "GET" && request.path.starts_with("/api/meteogram") => {
            meteogram_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/xsection") => {
            xsection_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/sounding") => {
            sounding_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/runs") => {
            runs_response(&request.query, &state)
        }
        ("GET", "/api/fires") => fires_response(&state),
        _ if request.method == "GET" && request.path.starts_with("/api/vars") => {
            vars_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/daily") => {
            daily_response(&request.query, &state)
        }
        _ if request.method == "GET" && request.path.starts_with("/api/ecape/") => {
            ecape_file_response(&request.path, &state)
        }
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

/// Enqueue ONE render and return its job id, or `(status, message)`.
///
/// Extracted from the POST handler so loops queue frames through the exact same
/// path: same validation, same alias resolution, same render cache (an hour
/// already rendered returns its existing job id instead of re-rendering) and the
/// same gate. A loop that duplicated this would drift from it.
fn enqueue_render_job(
    request: RenderJobRequest,
    state: &AppState,
) -> Result<String, (u16, String)> {
    let mut request = validate_render_request(request).map_err(|message| (400u16, message))?;
    // Resolve `latest` BEFORE the cache key: alias entries must never
    // outlive the run they pointed at.
    let alias = request.run.trim().to_ascii_lowercase();
    if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        request.run = resolve_latest_run_for_hour(
            &state.store_root,
            &request.model,
            &alias,
            Some(request.hour),
        )
        .map_err(|message| (422u16, message))?;
    }

    let cache_key = render_cache_key(&request);
    if let Some(cached_id) = cached_render_job_id(state, &cache_key) {
        return Ok(cached_id);
    }
    let id = next_job_id(state);
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
    Ok(id)
}

fn start_render_job(body: Vec<u8>, state: AppState) -> Vec<u8> {
    let request = match serde_json::from_slice::<RenderJobRequest>(&body) {
        Ok(request) => request,
        Err(err) => {
            return json_status_response(
                400,
                &serde_json::json!({ "error": format!("invalid JSON body: {err}") }),
            );
        }
    };
    // Report cache hit/miss the way callers already expect: a hit reuses an
    // existing job id, so compare what we get back against a fresh id.
    let existing: std::collections::HashSet<String> = state
        .jobs
        .lock()
        .expect("job mutex")
        .keys()
        .cloned()
        .collect();
    match enqueue_render_job(request, &state) {
        Ok(id) => {
            let cache = if existing.contains(&id) { "hit" } else { "miss" };
            json_status_response(
                202,
                &serde_json::json!({
                    "id": id,
                    "status_url": format!("/api/jobs/{id}"),
                    "cache": cache,
                }),
            )
        }
        Err((status, message)) => {
            json_status_response(status, &serde_json::json!({ "error": message }))
        }
    }
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
        // Rendered outputs are job-id-keyed and never change, so let browsers
        // and the CDN cache them hard — makes loop/prefetch frame re-loads
        // instant instead of a fresh fetch each cycle.
        Ok(bytes) => response_with_extra_headers(
            200,
            content_type,
            bytes,
            "Cache-Control: public, max-age=31536000, immutable\r\n",
        ),
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
    command.env(
        "RUSTWX_PROJECTION_VARIANT",
        projection_variant_for_bounds(request.resolved_bounds()),
    );
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
    // Surface temperature family display units ("f" default | "c"); every
    // map lane reads this at request-finalization time.
    command.env("RUSTWX_TEMP_UNITS", &request.temp_units);
    // Pivotal-style city value labels: the place-label overlay stamps the
    // plotted value (in display units) at each city instead of its name.
    if request.value_labels {
        command.env("RUSTWX_VALUE_LABELS", "1");
    }
    // Plot-banner branding. Absent env vars => the render keeps its built-in
    // CAFire banner (unchanged). A present-but-empty title/credit omits that
    // element; the logo is written to the job dir and passed by path.
    if let Some(brand) = &request.brand {
        let resolved = brand
            .resolve()
            .map_err(|message| (message, String::new(), String::new()))?;
        if let Some(title) = resolved.title {
            command.env("RUSTWX_BRAND_TITLE", title);
        }
        if let Some(credit) = resolved.credit {
            command.env("RUSTWX_BRAND_CREDIT", credit);
        }
        if let Some(logo_png) = resolved.logo_png {
            let logo_path = output_dir.join(BRAND_LOGO_FILENAME);
            fs::write(&logo_path, &logo_png).map_err(|err| {
                (
                    format!("write {}: {err}", logo_path.display()),
                    String::new(),
                    String::new(),
                )
            })?;
            command.env("RUSTWX_BRAND_LOGO", logo_path.display().to_string());
        }
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

/// Sidecar input written into the job dir for the render child (a PNG logo).
/// It is NOT a render artifact, so it must be excluded from the served files.
const BRAND_LOGO_FILENAME: &str = "brand_logo.png";

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
        // The brand logo is an input we wrote, not a rendered output.
        if name == BRAND_LOGO_FILENAME {
            continue;
        }
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

/// Serve a rendered SVG card as vector SVG, or rasterized to PNG when the
/// client passes `?format=png` — a raw SVG document can't be copied/pasted as a
/// picture (a drag-select grabs its `<text>`), so shares / "open image in new
/// tab" need a raster. The in-page `<img>` should keep requesting SVG for
/// crispness.
fn svg_card_response(svg: String, want_png: bool) -> Vec<u8> {
    if want_png {
        match svg_raster::svg_to_png(&svg, 2.0) {
            Ok(png) => {
                response_with_extra_headers(200, "image/png", png, "Cache-Control: no-store\r\n")
            }
            Err(message) => json_status_response(
                500,
                &serde_json::json!({ "error": format!("rasterize: {message}") }),
            ),
        }
    } else {
        response_with_extra_headers(
            200,
            "image/svg+xml; charset=utf-8",
            svg.into_bytes(),
            "Cache-Control: no-store\r\n",
        )
    }
}

/// GET /api/daily?lat&lon&var=temperature_2m[&model][&run][&title][&utc_offset]
/// — the shareable daily HI/LO outlook card (weathermodels-style) for any
/// stored variable, one column per local calendar day.
fn daily_response(query: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(query);
    let bad = |message: &str| json_status_response(400, &serde_json::json!({ "error": message }));
    let Some(lat) = query.get("lat").and_then(|v| v.parse::<f64>().ok()) else {
        return bad("lat is required");
    };
    let Some(lon) = query.get("lon").and_then(|v| v.parse::<f64>().ok()).map(wrap_longitude) else {
        return bad("lon is required");
    };
    let Some(var) = query.get("var").cloned().filter(|v| {
        !v.is_empty() && v.len() <= 48 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    }) else {
        return bad("var is required (see /api/vars)");
    };
    let model = query.get("model").map(String::as_str).unwrap_or("hrrr");
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return bad("model slug is not valid");
    }
    let mut run = query.get("run").cloned().unwrap_or_else(|| "latest".to_string());
    let alias = run.to_ascii_lowercase();
    if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        run = match resolve_latest_run(&state.store_root, model, &alias) {
            Ok(resolved) => resolved,
            Err(message) => return json_status_response(422, &serde_json::json!({ "error": message })),
        };
    }
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return bad("run slug is not valid");
    }
    let Some((date, cycle)) = run.split_once('_').and_then(|(date, cycle)| {
        let hour: u8 = cycle.strip_suffix('z')?.parse().ok()?;
        (date.len() == 8 && hour <= 23).then(|| (date.to_string(), hour))
    }) else {
        return bad("run must look like 20260702_00z");
    };
    let request = meteogram::DailyRequest {
        lat,
        lon,
        var,
        title: query.get("title").cloned().filter(|t| !t.trim().is_empty()),
        utc_offset_hours: query
            .get("utc_offset")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-14.0..=14.0).contains(v))
            .unwrap_or(-7.0),
        // step=day (default) | 1 | 3 | 6 — hourly/bucketed columns.
        step_hours: query
            .get("step")
            .and_then(|v| v.parse::<u16>().ok())
            .filter(|v| matches!(v, 1 | 3 | 6 | 12)),
        fahrenheit: query_wants_fahrenheit(&query),
        theme: {
            let mut theme = card_theme_from_query(&query);
            // An unknown id draws the card without a logo rather than failing:
            // a stale shared link should still show the forecast.
            theme.logo = query.get("logo").and_then(|id| load_card_logo(id, state));
            theme
        },
    };
    // `?format=png` returns a rasterized copy so the card can be shared /
    // "open image in new tab" / copied as a picture. The in-page display stays SVG.
    let want_png = query
        .get("format")
        .map(|f| f.eq_ignore_ascii_case("png"))
        .unwrap_or(false);
    match meteogram::render_daily_svg(&state.store_root, model, &run, &date, cycle, &request) {
        Ok(svg) => svg_card_response(svg, want_png),
        // `latest` resolves to the newest complete run, but a short hourly run
        // that initializes mid-local-day (e.g. HRRR 14z–17z = morning PDT,
        // F0–18) can't put the required ~3/4 of a day's samples in ANY bucket,
        // so the card 422s purely from cycle timing. The day_run pointer is the
        // newest extended run and always spans full local days — retry with it
        // rather than erroring. Explicit run requests still surface the error.
        Err(message)
            if alias == "latest" && message.contains("enough samples") =>
        {
            let fallback = resolve_latest_run(&state.store_root, model, "latest-day")
                .ok()
                .filter(|day_run| *day_run != run)
                .and_then(|day_run| {
                    let (date, cycle) = day_run.split_once('_').and_then(|(date, cycle)| {
                        let hour: u8 = cycle.strip_suffix('z')?.parse().ok()?;
                        (date.len() == 8 && hour <= 23).then(|| (date.to_string(), hour))
                    })?;
                    meteogram::render_daily_svg(
                        &state.store_root,
                        model,
                        &day_run,
                        &date,
                        cycle,
                        &request,
                    )
                    .ok()
                });
            match fallback {
                Some(svg) => svg_card_response(svg, want_png),
                None => json_status_response(422, &serde_json::json!({ "error": message })),
            }
        }
        Err(message) => json_status_response(422, &serde_json::json!({ "error": message })),
    }
}

/// GET /api/vars[?model=hrrr][&run=latest] — every 2D variable stored in the
/// run's newest hour (name + units): the catalog behind the chart-anything
/// custom meteogram mode.
fn vars_response(query: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(query);
    let model = query.get("model").map(String::as_str).unwrap_or("hrrr").to_string();
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return json_status_response(400, &serde_json::json!({ "error": "model slug is not valid" }));
    }
    let mut run = query.get("run").cloned().unwrap_or_else(|| "latest".to_string());
    let alias = run.to_ascii_lowercase();
    if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        run = match resolve_latest_run(&state.store_root, &model, &alias) {
            Ok(resolved) => resolved,
            Err(message) => {
                return json_status_response(422, &serde_json::json!({ "error": message }));
            }
        };
    }
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return json_status_response(400, &serde_json::json!({ "error": "run slug is not valid" }));
    }
    let run_dir = state.store_root.join(&model).join(&run);
    let newest = std::fs::read_dir(&run_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            let hour: u16 = name.strip_prefix('f')?.strip_suffix(".rws")?.parse().ok()?;
            Some((hour, entry.path()))
        })
        .max_by_key(|(hour, _)| *hour);
    let Some((hour, path)) = newest else {
        return json_status_response(404, &serde_json::json!({ "error": "run has no stored hours" }));
    };
    match rw_store::reader::HourReader::open(&path) {
        Ok(reader) => {
            let vars: Vec<serde_json::Value> = reader
                .meta()
                .variables
                .iter()
                .filter(|var| var.kind == "surface2d")
                .map(|var| serde_json::json!({ "name": var.name, "units": var.units }))
                .collect();
            json_response(&serde_json::json!({ "model": model, "run": run, "hour": hour, "vars": vars }))
        }
        Err(err) => json_status_response(500, &serde_json::json!({ "error": err.to_string() })),
    }
}

/// GET /api/ecape/<run>/<file> or /api/ecape/latest.json — static products
/// pushed up by the ECAPE compute node (outbound-only rsync into
/// out_root/ecape/). Path segments are strictly sanitized.
fn ecape_file_response(path: &str, state: &AppState) -> Vec<u8> {
    let rel = path.trim_start_matches("/api/ecape/");
    let safe = !rel.is_empty()
        && rel.len() < 160
        && rel
            .split('/')
            .all(|part| {
                !part.is_empty()
                    && part != ".."
                    && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            });
    if !safe {
        return json_status_response(400, &serde_json::json!({ "error": "bad path" }));
    }
    let full = state.out_root.join("ecape").join(rel);
    match std::fs::read(&full) {
        Ok(bytes) => {
            let content_type = if rel.ends_with(".json") {
                "application/json; charset=utf-8"
            } else if rel.ends_with(".webp") {
                "image/webp"
            } else if rel.ends_with(".png") {
                "image/png"
            } else {
                "application/octet-stream"
            };
            response(200, content_type, bytes)
        }
        Err(_) => json_status_response(404, &serde_json::json!({ "error": "not found" })),
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
    // Per-run stored hours so clients can build honest hour ladders for
    // EXPLICIT run picks (the latest manifest only covers the alias).
    // Optional ?var=<store variable> narrows each list to hours whose
    // file actually carries that variable — so a client can offer only
    // renderable hours for store-grid products (e.g. PFT exists only on
    // hours ingested after its lane deployed).
    let want_var = query
        .get("var")
        .map(String::as_str)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    let mut hours_by_run = serde_json::Map::new();
    for run in &runs {
        let mut hours: Vec<u16> = std::fs::read_dir(model_dir.join(run))
            .into_iter()
            .flatten()
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let hour = name.strip_prefix('f')?.strip_suffix(".rws")?.parse::<u16>().ok()?;
                if let Some(var) = want_var {
                    let reader =
                        rw_store::reader::HourReader::open(&entry.path()).ok()?;
                    reader.variable(var)?;
                }
                Some(hour)
            })
            .collect();
        hours.sort_unstable();
        hours_by_run.insert(run.clone(), serde_json::json!(hours));
    }
    let latest: Option<serde_json::Value> = std::fs::read_to_string(model_dir.join("latest.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    json_response(
        &serde_json::json!({ "model": model, "runs": runs, "hours": hours_by_run, "latest": latest }),
    )
}

/// Resolve the `latest` / `latest-day` / `fuel-run` run aliases via the
/// daemon's atomic manifest. `latest` = newest fully-stored run (freshest
/// weather); `latest-day` = newest complete run covering a full UTC day
/// (anomaly/day-window lanes); `fuel-run` = newest complete run whose fuels
/// are imported (fuel products, so they never error on `latest` during a
/// fresh run's gridMET-import lag). Unknown aliases resolve as `latest`.
fn resolve_latest_run(store_root: &Path, model: &str, alias: &str) -> Result<String, String> {
    resolve_latest_run_for_hour(store_root, model, alias, None)
}

/// Whether a stored run actually holds the hour a request needs.
fn run_has_hour(store_root: &Path, model: &str, run: &str, hour: u16) -> bool {
    store_root
        .join(model)
        .join(run)
        .join(format!("f{hour:03}.rws"))
        .is_file()
}

/// Stored run slugs for a model, newest first (slugs sort chronologically).
fn stored_runs_newest_first(store_root: &Path, model: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(store_root.join(model)) else {
        return Vec::new();
    };
    let mut runs: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.len() == 12 && name.as_bytes()[8] == b'_' && name.ends_with('z'))
        .collect();
    runs.sort_unstable();
    runs.reverse();
    runs
}

/// Resolve a run alias, requiring the chosen run to actually contain
/// `required_hour` when one is given.
///
/// The pointers in `latest.json` advance as soon as a cycle STARTS ingesting. An
/// extended HRRR cycle takes a while to walk F000 -> F048, so `day_run` pointed
/// at a run holding only F040 while every 0-48 h window product asks for F048 --
/// the render died on `f048.rws: No such file or directory`, which reached the
/// Lab as a bare "rw_render exited with exit status: 1". That broke all 227
/// window products for an hour-plus, four times a day.
///
/// The alias now takes its preferred pointer only if that run can serve the
/// request, else walks back to the newest stored run that can. With no
/// `required_hour` the behavior is exactly as before.
fn resolve_latest_run_for_hour(
    store_root: &Path,
    model: &str,
    alias: &str,
    required_hour: Option<u16>,
) -> Result<String, String> {
    let path = store_root.join(model).join("latest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|_| format!("no latest-run manifest for model '{model}' (daemon not running?)"))?;
    let manifest: serde_json::Value =
        serde_json::from_str(&text).map_err(|err| format!("latest.json: {err}"))?;
    let field = |name: &str| manifest.get(name).and_then(|value| value.as_str());
    let preferred: &[&str] = match alias {
        "latest-day" => &["day_run", "complete_run", "run"],
        "fuel-run" => &["fuel_run", "complete_run", "run"],
        _ => &["complete_run", "run"],
    };
    let mut candidates: Vec<String> = preferred
        .iter()
        .filter_map(|name| field(name))
        .map(str::to_string)
        .collect();
    if candidates.is_empty() {
        return Err("latest.json has no run field".to_string());
    }
    if let Some(hour) = required_hour {
        for run in stored_runs_newest_first(store_root, model) {
            if !candidates.contains(&run) {
                candidates.push(run);
            }
        }
        if let Some(run) = candidates
            .iter()
            .find(|run| run_has_hour(store_root, model, run, hour))
        {
            return validated_run_slug(run);
        }
        return Err(format!(
            "no stored {model} run holds F{hour:03} yet (the newest extended cycle is still ingesting)"
        ));
    }
    validated_run_slug(&candidates[0])
}

fn validated_run_slug(run: &str) -> Result<String, String> {
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
    request.temp_units = normalize_temp_units(&request.temp_units)?;
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
    let Some(lon) = query.get("lon").and_then(|v| v.parse::<f64>().ok()).map(wrap_longitude) else {
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
    let run = if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        match resolve_latest_run(&state.store_root, model, &alias) {
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
    let vars: Vec<String> = query
        .get("vars")
        .map(|list| {
            list.split(',')
                .map(str::trim)
                .filter(|v| !v.is_empty() && v.len() <= 48)
                .filter(|v| v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .take(8)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let request = meteogram::MeteogramRequest {
        lat,
        lon,
        panels,
        vars,
        title: query.get("title").cloned().filter(|t| !t.trim().is_empty()),
        utc_offset_hours: query
            .get("utc_offset")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-14.0..=14.0).contains(v))
            .unwrap_or(-7.0),
        fahrenheit: query_wants_fahrenheit(&query),
    };
    match meteogram::render_meteogram_svg(&state.store_root, model, run, &date, cycle, &request) {
        Ok(output) => {
            if query.get("format").map(String::as_str) == Some("json") {
                json_response(&output.data)
            } else {
                let want_png = query
                    .get("format")
                    .map(|f| f.eq_ignore_ascii_case("png"))
                    .unwrap_or(false);
                svg_card_response(output.svg, want_png)
            }
        }
        Err(message) => json_status_response(422, &serde_json::json!({ "error": message })),
    }
}

/// GET /api/xsection?lat0=..&lon0=..&lat1=..&lon1=..&run=latest[&model=hrrr]
/// [&hour=12][&field=temperature|rh|wind][&utc_offset=-7][&format=json]
/// — a vertical slice through the stored isobaric volumes along the A→B
/// line, as inline SVG (or the raw sampled arrays with format=json).
fn xsection_response(path: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(path);
    let bad = |message: &str| json_status_response(400, &serde_json::json!({ "error": message }));
    let mut coords = [0.0f64; 4];
    for (slot, key) in coords.iter_mut().zip(["lat0", "lon0", "lat1", "lon1"]) {
        match query.get(key).and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()) {
            Some(value) => *slot = value,
            None => return bad(&format!("{key} is required (endpoint coordinates)")),
        }
    }
    let [lat0, lon0, lat1, lon1] = coords;
    if !((-90.0..=90.0).contains(&lat0) && (-90.0..=90.0).contains(&lat1)) {
        return bad("latitudes must be within -90..90");
    }
    if !((-360.0..=360.0).contains(&lon0) && (-360.0..=360.0).contains(&lon1)) {
        return bad("longitudes must be within -360..360");
    }
    // A panned web map hands back unwrapped longitudes; the grids are -180..180.
    let (lon0, lon1) = (wrap_longitude(lon0), wrap_longitude(lon1));
    let Some(run) = query.get("run").map(String::as_str) else {
        return bad("run is required (e.g. 20260702_22z or latest)");
    };
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return bad("run slug is not valid");
    }
    let model = query.get("model").map(String::as_str).unwrap_or("hrrr");
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return bad("model slug is not valid");
    }
    let field = query
        .get("field")
        .map(String::as_str)
        .unwrap_or("temperature")
        .to_ascii_lowercase();
    if !xsection::XSECTION_FIELDS.contains(&field.as_str()) {
        return bad(&format!(
            "field must be one of {}",
            xsection::XSECTION_FIELDS.join(", ")
        ));
    }
    let hour = match query.get("hour") {
        None => None,
        Some(value) => match value.parse::<u16>().ok().filter(|h| *h <= 384) {
            Some(hour) => Some(hour),
            None => return bad("hour must be an integer forecast hour (0-384)"),
        },
    };
    let resolved_run;
    let alias = run.to_ascii_lowercase();
    let run = if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        match resolve_latest_run(&state.store_root, model, &alias) {
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
        return bad("run must look like 20260702_22z");
    };
    let request = xsection::XsectionRequest {
        lat0,
        lon0,
        lat1,
        lon1,
        field,
        hour,
        utc_offset_hours: query
            .get("utc_offset")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-14.0..=14.0).contains(v))
            .unwrap_or(-7.0),
    };
    match xsection::render_xsection_svg(&state.store_root, model, run, &date, cycle, &request) {
        Ok(output) => {
            if query.get("format").map(String::as_str) == Some("json") {
                json_response(&output.data)
            } else {
                let want_png = query
                    .get("format")
                    .map(|f| f.eq_ignore_ascii_case("png"))
                    .unwrap_or(false);
                svg_card_response(output.svg, want_png)
            }
        }
        Err(message) => json_status_response(422, &serde_json::json!({ "error": message })),
    }
}

/// GET /api/sounding?lat=..&lon=..&run=latest[&model=hrrr][&hour=12]
/// [&utc_offset=-7][&format=json] — a CWT-styled skew-T/hodograph/ECAPE
/// sounding for the nearest grid cell, composed from the stored isobaric
/// volumes + surface fields, as PNG (or the profile arrays and computed
/// indices with format=json).
fn sounding_response(path: &str, state: &AppState) -> Vec<u8> {
    let query = parse_query(path);
    let bad = |message: &str| json_status_response(400, &serde_json::json!({ "error": message }));
    let Some(lat) = query.get("lat").and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite())
    else {
        return bad("lat is required");
    };
    let Some(lon) = query
        .get("lon")
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(wrap_longitude)
    else {
        return bad("lon is required");
    };
    if !(-90.0..=90.0).contains(&lat) {
        return bad("lat must be within -90..90");
    }
    if !(-360.0..=360.0).contains(&lon) {
        return bad("lon must be within -360..360");
    }
    let Some(run) = query.get("run").map(String::as_str) else {
        return bad("run is required (e.g. 20260702_22z or latest)");
    };
    if run.len() > 40 || run.contains(['/', '\\', '.']) {
        return bad("run slug is not valid");
    }
    let model = query.get("model").map(String::as_str).unwrap_or("hrrr");
    if model.len() > 24 || model.contains(['/', '\\', '.']) {
        return bad("model slug is not valid");
    }
    let hour = match query.get("hour") {
        None => None,
        Some(value) => match value.parse::<u16>().ok().filter(|h| *h <= 384) {
            Some(hour) => Some(hour),
            None => return bad("hour must be an integer forecast hour (0-384)"),
        },
    };
    let resolved_run;
    let alias = run.to_ascii_lowercase();
    let run = if alias == "latest" || alias == "latest-day" || alias == "fuel-run" {
        match resolve_latest_run(&state.store_root, model, &alias) {
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
        return bad("run must look like 20260702_22z");
    };
    let request = sounding::SoundingRequest {
        lat,
        lon,
        hour,
        utc_offset_hours: query
            .get("utc_offset")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| (-14.0..=14.0).contains(v))
            .unwrap_or(-7.0),
        // Same contract as the outlook cards: an ABSENT `brand` keeps the house
        // credit, a present-but-EMPTY one draws none.
        brand: card_text_override(&query, "brand")
            .unwrap_or_else(|| "cafire.org/weather".to_string()),
    };
    match sounding::render_sounding(&state.store_root, model, run, &date, cycle, &request) {
        Ok(output) => {
            if query.get("format").map(String::as_str) == Some("json") {
                json_response(&output.data)
            } else {
                response_with_extra_headers(
                    200,
                    "image/png",
                    output.png,
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

/// Longitude/latitude spans past which a domain is "continental" rather than
/// regional. Mirrors the renderer's own wide-domain threshold
/// (`full_domain_projected_frame_default`) so both agree on what CONUS-scale
/// means.
const CONTINENTAL_LAT_SPAN_DEG: f64 = 25.0;
const CONTINENTAL_LON_SPAN_DEG: f64 = 45.0;

/// Which presentation projection the render child should use.
///
/// Regional boxes get Mercator: north stays up, the aspect is honest at a
/// single reference latitude, and every one of the Lab's state/region crops
/// falls in this class. A CONUS-scale box does NOT — a regional Mercator's
/// single reference latitude cannot describe 26 degrees of latitude, and the
/// frame it produces disagreed with the projected grid badly enough that the
/// raster was clipped: CONUS lost Texas, the Gulf and all of Florida below
/// ~34 N and left a third of the canvas empty. `adaptive` picks the conic
/// presentation (Lambert/Albers) that a continental map actually wants.
fn projection_variant_for_bounds(bounds: [f64; 4]) -> &'static str {
    let [west, east, south, north] = bounds;
    let lat_span = (north - south).abs();
    let lon_span = (east - west).abs();
    if lat_span >= CONTINENTAL_LAT_SPAN_DEG || lon_span >= CONTINENTAL_LON_SPAN_DEG {
        "adaptive"
    } else {
        "mercator"
    }
}

/// On-screen aspect (width:height) of a lat/lon box as it is PROJECTED, not as
/// raw degrees: a degree of longitude spans only `cos(latitude)` as much
/// distance as a degree of latitude, so CONUS draws about 1.8:1, not the 2.25:1
/// its degree extents suggest. Sizing the canvas from raw degrees made the
/// canvas disagree with the map on every domain, and the renderer then
/// letterboxed the map inside it — the dead bands above and below the plot.
fn domain_display_aspect(bounds: [f64; 4]) -> f64 {
    let [west, east, south, north] = bounds;
    let lat_span = (north - south).abs().max(0.05);
    let lon_span = (east - west).abs().max(0.05);
    let mid_lat_cos = (f64::midpoint(south, north)).to_radians().cos().max(0.15);
    ((lon_span * mid_lat_cos) / lat_span).clamp(0.3, 3.6)
}

/// Scale a canvas so both edges satisfy the minimums (and neither exceeds the
/// maximum) WITHOUT changing its aspect. Clamping a single axis is what
/// produced the "CONUS is a half strip" canvases: a 1200x545 CONUS frame had
/// its height forced up to 900, leaving a 1.33 canvas around a 1.80 map.
fn fit_render_dimensions(width: f64, height: f64) -> (u32, u32) {
    let width = width.max(1.0);
    let height = height.max(1.0);
    let up = (f64::from(MIN_RENDER_WIDTH) / width)
        .max(f64::from(MIN_RENDER_HEIGHT) / height)
        .max(1.0);
    let (width, height) = (width * up, height * up);
    let down = (f64::from(MAX_RENDER_DIMENSION) / width.max(height)).min(1.0);
    (
        (width * down).round().max(1.0) as u32,
        (height * down).round().max(1.0) as u32,
    )
}

fn output_size(request: &RenderJobRequest) -> (u32, u32) {
    let aspect = domain_display_aspect(request.resolved_bounds());
    match (request.output_width, request.output_height) {
        // Both given: the caller owns the frame exactly.
        (Some(width), Some(height)) => (
            width.clamp(MIN_RENDER_WIDTH, MAX_RENDER_DIMENSION),
            height.clamp(MIN_RENDER_HEIGHT, MAX_RENDER_DIMENSION),
        ),
        (Some(width), None) => {
            let width = f64::from(width);
            fit_render_dimensions(width, width / aspect)
        }
        (None, Some(height)) => {
            let height = f64::from(height);
            fit_render_dimensions(height * aspect, height)
        }
        // Default frame: hold the long edge near 1600 and let the domain's
        // aspect set the other.
        (None, None) => {
            if aspect >= 1.0 {
                fit_render_dimensions(1600.0, 1600.0 / aspect)
            } else {
                fit_render_dimensions(1500.0 * aspect, 1500.0)
            }
        }
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
    // Fingerprint the branding so a re-brand never returns a cached image of a
    // different brand (the logo is hashed, not embedded, to keep the key small).
    let brand_part = match &request.brand {
        None => "-".to_string(),
        Some(brand) => {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            brand.preset.as_deref().unwrap_or("").hash(&mut hasher);
            brand.title.as_deref().unwrap_or("").hash(&mut hasher);
            brand.credit.as_deref().unwrap_or("").hash(&mut hasher);
            brand.logo_b64.as_deref().unwrap_or("").hash(&mut hasher);
            format!("{:x}", hasher.finish())
        }
    };
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}x{}",
        request.model,
        request.run,
        request.hour,
        request.products.trim(),
        request.output_format,
        request.plot_style,
        request.basemap_style,
        request.temp_units,
        request.county_linework,
        request.place_label_density,
        request.place_label_size,
        request.value_labels,
        request.domain_slug,
        format_bounds(request.resolved_bounds()),
        perimeter_part,
        request.title_note.as_deref().unwrap_or("-"),
        brand_part,
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

/// `temp_units=c` opt-out for the SVG card/chart endpoints, matching the map
/// lane's `temp_units` body field (°F default, `c`/`celsius` for Celsius).
/// Anything else — including the explicit `f` the Lab may send — keeps °F.
fn query_wants_fahrenheit(query: &HashMap<String, String>) -> bool {
    !query
        .get("temp_units")
        .map(|value| {
            let value = value.trim().to_ascii_lowercase();
            value == "c" || value == "celsius"
        })
        .unwrap_or(false)
}

/// Wrap a longitude into the [-180, 180] range every stored grid is indexed on.
///
/// A web map's longitude is unbounded: Leaflet's `mouseEventToLatLng` hands back
/// -186 or +200 once the user pans past the antimeridian, and it keeps counting
/// on each further wrap. Our grids — including the GLOBAL GFS grid — run
/// -180..180, so an unwrapped click missed every cell and the point products
/// answered "point is outside the model grid" for a place the model plainly
/// covers. Wrapping is the honest fix: the click names a real location either
/// way, and rejecting it would only move the confusion.
fn wrap_longitude(lon: f64) -> f64 {
    if !lon.is_finite() {
        return lon;
    }
    let wrapped = (lon + 180.0).rem_euclid(360.0) - 180.0;
    // rem_euclid sends an exact +180 to -180; the antimeridian is the same
    // meridian either way, so keep the sign the caller asked for.
    if wrapped == -180.0 && lon > 0.0 {
        180.0
    } else {
        wrapped
    }
}

/// A short piece of caller-supplied card text (brand, credit, footer).
///
/// Present-but-empty is meaningful and must survive: `?credit=` is how a caller
/// says "no attribution line", which is different from omitting the parameter
/// and inheriting the theme's own. Control characters are dropped because the
/// value is interpolated into SVG text.
fn card_text_override(query: &HashMap<String, String>, key: &str) -> Option<String> {
    query.get(key).map(|raw| {
        raw.trim()
            .chars()
            .filter(|c| !c.is_control())
            .take(48)
            .collect::<String>()
    })
}

/// Where uploaded card logos live. Content-addressed, so the same logo uploaded
/// twice is one file and one stable URL.
fn card_logo_dir(state: &AppState) -> PathBuf {
    state.out_root.join("card-logos")
}

/// Content address for an uploaded logo: FNV-1a over the re-encoded PNG.
///
/// A cache key, not a security boundary — nothing is authenticated by it. What
/// it buys is stability: the same logo always yields the same id, so a shared
/// card link keeps working and repeated uploads don't pile up files.
fn logo_content_id(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Largest edge an embedded logo keeps. A card header shows it 44 px tall, so
/// 512 px is generous for 2x rasterization and still small enough that the
/// base64 copy inside every SVG stays a few tens of KB.
const CARD_LOGO_MAX_EDGE: u32 = 512;
/// Upload ceiling. Well above any real logo, far below memory pressure.
const CARD_LOGO_MAX_BYTES: usize = 1024 * 1024;

/// POST /api/card-logo — store a logo for `/api/daily?...&logo=<id>`.
///
/// Cards are GETs, which is what makes them shareable, downloadable and
/// copyable by URL; a base64 logo in the query string would blow past request
/// line limits. So the bytes are uploaded once and referenced by id.
///
/// Anything that decodes as an image is re-encoded to PNG, which both
/// normalizes the format and drops whatever else was in the container.
fn store_card_logo(body: Vec<u8>, state: &AppState) -> Vec<u8> {
    let bad = |message: &str| json_status_response(400, &serde_json::json!({ "error": message }));
    // Accept either raw bytes or the `data:image/png;base64,...` URL a browser
    // canvas produces, since the labs already build the latter for map banners.
    let bytes = if body.starts_with(b"data:") {
        let text = String::from_utf8_lossy(&body);
        let Some((_, encoded)) = text.split_once("base64,") else {
            return bad("data URL must be base64 (data:image/png;base64,...)");
        };
        use base64::Engine;
        match base64::engine::general_purpose::STANDARD.decode(encoded.trim()) {
            Ok(bytes) => bytes,
            Err(err) => return bad(&format!("data URL is not valid base64: {err}")),
        }
    } else {
        body
    };
    if bytes.is_empty() {
        return bad("empty upload; POST the image bytes or a data: URL");
    }
    if bytes.len() > CARD_LOGO_MAX_BYTES {
        return bad("logo is larger than 1 MB; downscale it first");
    }

    // Bounded decode: a small highly-compressed PNG can declare enormous
    // dimensions, and this endpoint is unauthenticated.
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    let reader = match image::ImageReader::new(std::io::Cursor::new(&bytes)).with_guessed_format() {
        Ok(reader) => reader,
        Err(err) => return bad(&format!("could not read the upload: {err}")),
    };
    let mut reader = reader;
    reader.limits(limits);
    let decoded = match reader.decode() {
        Ok(image) => image,
        Err(err) => {
            return bad(&format!(
                "could not decode the image ({err}); PNG, WebP and GIF are supported"
            ));
        }
    };

    let scaled = if decoded.width().max(decoded.height()) > CARD_LOGO_MAX_EDGE {
        decoded.resize(
            CARD_LOGO_MAX_EDGE,
            CARD_LOGO_MAX_EDGE,
            image::imageops::FilterType::Lanczos3,
        )
    } else {
        decoded
    };
    let mut png: Vec<u8> = Vec::new();
    if let Err(err) = scaled.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png) {
        return json_status_response(
            500,
            &serde_json::json!({ "error": format!("could not re-encode the logo: {err}") }),
        );
    }

    let id = logo_content_id(&png);
    let dir = card_logo_dir(state);
    if let Err(err) = fs::create_dir_all(&dir) {
        return json_status_response(
            500,
            &serde_json::json!({ "error": format!("logo dir: {err}") }),
        );
    }
    let path = dir.join(format!("{id}.png"));
    if !path.exists() {
        if let Err(err) = fs::write(&path, &png) {
            return json_status_response(
                500,
                &serde_json::json!({ "error": format!("store logo: {err}") }),
            );
        }
    }
    json_response(&serde_json::json!({
        "id": id,
        "width": scaled.width(),
        "height": scaled.height(),
        "bytes": png.len(),
        "usage": format!("/api/daily?...&logo={id}"),
    }))
}

/// Resolve a logo id to its file, rejecting anything that is not one of our own
/// hex hashes.
///
/// This is the only thing standing between a query parameter and the
/// filesystem, so it is deliberately a separate, strict, testable function
/// rather than a check buried in the I/O path.
fn card_logo_path(id: &str, dir: &Path) -> Option<PathBuf> {
    if id.is_empty() || id.len() > 32 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(dir.join(format!("{id}.png")))
}

/// Load an uploaded logo for embedding, or None when the id is unknown.
fn load_card_logo(id: &str, state: &AppState) -> Option<meteogram::CardLogo> {
    let path = card_logo_path(id, &card_logo_dir(state))?;
    let bytes = fs::read(&path).ok()?;
    let (width, height) = image::image_dimensions(&path).ok()?;
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Some(meteogram::CardLogo {
        data_uri: format!("data:image/png;base64,{encoded}"),
        width,
        height,
    })
}

/// Card chrome from the query: a named theme, then optional per-request
/// overrides. Unknown theme names fall back to the branded default inside
/// `CardTheme::named`, so a stale shared link still renders.
fn card_theme_from_query(query: &HashMap<String, String>) -> meteogram::CardTheme {
    let mut theme = meteogram::CardTheme::named(query.get("theme").map(String::as_str).unwrap_or(""));
    if let Some(brand) = card_text_override(query, "brand") {
        theme.brand = brand;
    }
    if let Some(credit) = card_text_override(query, "credit") {
        theme.credit = credit;
    }
    if let Some(footer) = card_text_override(query, "footer") {
        theme.footer = footer;
    }
    // A single accent drives both the place name and the extended-range marker;
    // only `#rrggbb` is accepted so nothing can inject markup into a fill.
    if let Some(accent) = query.get("accent").map(|raw| raw.trim()).filter(|raw| {
        raw.len() == 7
            && raw.starts_with('#')
            && raw[1..].chars().all(|c| c.is_ascii_hexdigit())
    }) {
        theme.accent = accent.to_string();
        theme.accent_soft = accent.to_string();
    }
    theme
}

fn default_temp_units() -> String {
    "f".to_string()
}

/// Normalize the surface temperature display units to the canonical "f" /
/// "c" the cache key and the render child use: an omitted field, "f", and
/// "fahrenheit" must all produce the SAME cache key.
fn normalize_temp_units(value: &str) -> Result<String, String> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "f" | "degf" | "fahrenheit" => Ok("f".to_string()),
        "c" | "degc" | "celsius" => Ok("c".to_string()),
        other => Err(format!(
            "temp_units must be 'f' (Fahrenheit, default) or 'c' (Celsius), got '{other}'"
        )),
    }
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
/// Neutral (unbranded) generic weather-plot lab, served at `/lab-generic`.
/// Reuses the same render API; adds a branding panel (generic / custom / none)
/// to exercise the `brand` request field end-to-end.
const GENERIC_LAB_HTML: &str = include_str!("../generic_lab.html");

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
            value_labels: false,
            brand: None,
            domain_slug: "box".to_string(),
            bounds: Some([-123.21, -119.67, 37.13, 41.14]),
            perimeter: None,
            padding_km: None,
            extend: None,
            overlay_perimeter: None,
            title_note: None,
            temp_units: default_temp_units(),
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

    /// A regional Mercator cannot describe a continental box: forcing it on
    /// CONUS produced a frame the projected grid overflowed, and the raster was
    /// clipped at ~34 N — no Texas, no Gulf, no Florida — over a third of the
    /// canvas left empty. Continental boxes must get the conic presentation.
    #[test]
    fn continental_domains_use_the_adaptive_conic_presentation() {
        let conus = [-125.0, -66.5, 24.0, 50.0];
        assert_eq!(projection_variant_for_bounds(conus), "adaptive");
        // A tall-but-narrow box counts as continental on latitude alone.
        assert_eq!(
            projection_variant_for_bounds([-100.0, -80.0, 24.0, 50.0]),
            "adaptive"
        );
    }

    /// Every state/region crop stays on Mercator — the fix above must not
    /// re-project the 70-odd regional domains that already render correctly.
    #[test]
    fn regional_domains_keep_mercator() {
        for bounds in [
            [-124.5, -114.0, 32.4, 42.1],   // california
            [-107.0, -89.0, 40.0, 49.5],    // northern plains
            [-125.0, -116.0, 41.9, 49.1],   // pacific northwest
            [-106.7, -93.5, 25.8, 36.6],    // texas
            [-85.7, -80.0, 24.3, 31.1],     // florida
        ] {
            assert_eq!(
                projection_variant_for_bounds(bounds),
                "mercator",
                "regional domain {bounds:?} must stay on Mercator"
            );
        }
    }

    #[test]
    fn output_size_derives_height_from_width_only_preview() {
        let request = RenderJobRequest {
            output_width: Some(1000),
            ..base_request()
        };
        // Height follows the PROJECTED aspect (cos-corrected), and the 1200
        // minimum width scales the whole frame rather than squashing one axis.
        assert_eq!(output_size(&request), (1200, 1752));
    }

    /// A wide domain must keep its projected aspect instead of being padded out
    /// to the minimum height: CONUS used to come back 1200x900 (1.33) around a
    /// 1.80 map, which rendered as a strip floating in dead space.
    #[test]
    fn output_size_keeps_wide_domain_aspect_instead_of_padding_height() {
        let conus = [-125.0, -66.5, 24.0, 50.0];
        let aspect = domain_display_aspect(conus);
        assert!(
            (1.7..1.9).contains(&aspect),
            "CONUS projects near 1.8:1, got {aspect}"
        );
        for requested in [900u32, 1800u32] {
            let request = RenderJobRequest {
                output_width: Some(requested),
                bounds: Some(conus),
                ..base_request()
            };
            let (width, height) = output_size(&request);
            assert!(
                width >= MIN_RENDER_WIDTH && height >= MIN_RENDER_HEIGHT,
                "{requested}: {width}x{height} under the minimum frame"
            );
            let rendered = f64::from(width) / f64::from(height);
            assert!(
                (rendered - aspect).abs() < 0.02,
                "{requested}: canvas {width}x{height} is {rendered:.2}, domain is {aspect:.2}"
            );
        }
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
    fn temp_units_normalize_and_default_fahrenheit_shares_the_cache_key() {
        // Omitted field (serde default), explicit "f", and "Fahrenheit"
        // must all normalize to the same cache key; "c" must differ.
        let omitted = validate_render_request(base_request()).unwrap();
        assert_eq!(omitted.temp_units, "f");
        let explicit_f = validate_render_request(RenderJobRequest {
            temp_units: "f".to_string(),
            ..base_request()
        })
        .unwrap();
        let spelled_out = validate_render_request(RenderJobRequest {
            temp_units: " Fahrenheit ".to_string(),
            ..base_request()
        })
        .unwrap();
        let celsius = validate_render_request(RenderJobRequest {
            temp_units: "C".to_string(),
            ..base_request()
        })
        .unwrap();
        assert_eq!(celsius.temp_units, "c");
        assert_eq!(render_cache_key(&omitted), render_cache_key(&explicit_f));
        assert_eq!(render_cache_key(&omitted), render_cache_key(&spelled_out));
        assert_ne!(render_cache_key(&omitted), render_cache_key(&celsius));
    }

    #[test]
    fn temp_units_reject_unknown_values() {
        let request = RenderJobRequest {
            temp_units: "kelvin".to_string(),
            ..base_request()
        };
        let message = validate_render_request(request).unwrap_err();
        assert!(message.contains("temp_units"), "unexpected error: {message}");
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

#[cfg(test)]
mod run_alias_tests {
    use super::*;

    /// Same pattern the windowed-store tests use: a pid-scoped temp dir, no
    /// extra dev-dependency.
    fn store_with(name: &str, runs: &[(&str, &[u16])]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-run-alias-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let model_dir = dir.join("hrrr");
        std::fs::create_dir_all(&model_dir).unwrap();
        for (run, hours) in runs {
            let run_dir = model_dir.join(run);
            std::fs::create_dir_all(&run_dir).unwrap();
            for hour in *hours {
                std::fs::write(run_dir.join(format!("f{hour:03}.rws")), b"x").unwrap();
            }
        }
        dir
    }

    fn write_manifest(dir: &std::path::Path, day_run: &str, complete_run: &str, run: &str) {
        std::fs::write(
            dir.join("hrrr").join("latest.json"),
            serde_json::json!({
                "day_run": day_run,
                "complete_run": complete_run,
                "run": run,
            })
            .to_string(),
        )
        .unwrap();
    }

    /// The reported outage: `day_run` advanced to an extended cycle still
    /// mid-ingest (F040), so every 0-48 h window product asked for F048 and got
    /// `f048.rws: No such file or directory` -> "rw_render exited with exit
    /// status: 1". Resolution must fall back to the newest run that HAS F048.
    #[test]
    fn latest_day_falls_back_when_the_pointer_run_lacks_the_hour() {
        let hours_48: Vec<u16> = (0..=48).collect();
        let hours_40: Vec<u16> = (0..=40).collect();
        let store = store_with(
            "fallback",
            &[("20260725_12z", &hours_48), ("20260725_18z", &hours_40)],
        );
        write_manifest(&store, "20260725_18z", "20260725_17z", "20260725_18z");

        // F048 cannot come from the mid-ingest 18z run.
        assert_eq!(
            resolve_latest_run_for_hour(&store, "hrrr", "latest-day", Some(48)).unwrap(),
            "20260725_12z"
        );
        // A low hour the pointer CAN serve still uses the pointer.
        assert_eq!(
            resolve_latest_run_for_hour(&store, "hrrr", "latest-day", Some(6)).unwrap(),
            "20260725_18z"
        );
        // No required hour = unchanged historical behavior.
        assert_eq!(
            resolve_latest_run(&store, "hrrr", "latest-day").unwrap(),
            "20260725_18z"
        );
    }

    /// When nothing on disk holds the hour, say so instead of letting rw_render
    /// die on a missing file.
    #[test]
    fn missing_hour_everywhere_is_an_honest_error() {
        let hours_18: Vec<u16> = (0..=18).collect();
        let store = store_with("missing", &[("20260725_18z", &hours_18)]);
        write_manifest(&store, "20260725_18z", "20260725_18z", "20260725_18z");
        let err = resolve_latest_run_for_hour(&store, "hrrr", "latest-day", Some(48))
            .expect_err("F048 is not stored anywhere");
        assert!(err.contains("F048"), "unhelpful message: {err}");
    }
}

#[cfg(test)]
mod loop_range_tests {
    use super::*;

    fn request(hours: Vec<u16>, start: Option<u16>, end: Option<u16>, step: Option<u16>) -> LoopJobRequest {
        let base: RenderJobRequest = serde_json::from_str(
            r#"{"run":"20260726_00z","hour":0,"products":"2m_temperature_10m_winds",
                "bounds":[-125.0,-66.5,24.0,50.0]}"#,
        )
        .expect("a minimal render request parses");
        LoopJobRequest {
            base,
            hours,
            hour_start: start,
            hour_end: end,
            hour_step: step,
            frame_ms: None,
            gif_width: None,
            video_width: None,
            video_crf: None,
        }
    }

    /// Custom loop length: the labs send an explicit hour list (they know which
    /// hours the run actually holds), and API callers can give a range instead.
    #[test]
    fn a_range_expands_at_the_requested_stride() {
        assert_eq!(
            loop_hours(&request(vec![], Some(6), Some(18), Some(3))).unwrap(),
            vec![6, 9, 12, 15, 18]
        );
        // A stride that overshoots the end still yields the start.
        assert_eq!(
            loop_hours(&request(vec![], Some(12), Some(14), Some(9))).unwrap(),
            vec![12]
        );
        // Default stride is hourly.
        assert_eq!(
            loop_hours(&request(vec![], Some(0), Some(3), None)).unwrap(),
            vec![0, 1, 2, 3]
        );
        // Zero is treated as 1 rather than dividing by zero.
        assert_eq!(
            loop_hours(&request(vec![], Some(0), Some(2), Some(0))).unwrap(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn an_explicit_list_wins_and_is_normalized() {
        // Out of order and duplicated: sorted and deduped, since frames are
        // assembled in hour order and a repeat would stutter the animation.
        assert_eq!(
            loop_hours(&request(vec![12, 6, 6, 0], None, None, None)).unwrap(),
            vec![0, 6, 12]
        );
        // The list takes precedence over any range fields.
        assert_eq!(
            loop_hours(&request(vec![7], Some(0), Some(48), Some(1))).unwrap(),
            vec![7]
        );
    }

    #[test]
    fn bad_ranges_are_named_not_guessed() {
        let err = loop_hours(&request(vec![], Some(12), Some(6), None)).expect_err("inverted");
        assert!(err.contains("hour_end"), "{err}");
        let err = loop_hours(&request(vec![], Some(6), None, None)).expect_err("no end");
        assert!(err.contains("hour_start"), "{err}");
        let err = loop_hours(&request(vec![], None, None, None)).expect_err("nothing at all");
        assert!(err.contains("hours"), "{err}");
    }

    /// The cap protects the render gate: one request must not be able to queue
    /// unbounded work. The labs mirror this number so the UI never asks for a
    /// job the API will refuse.
    #[test]
    fn the_frame_cap_is_enforced_and_reported() {
        let at_cap: Vec<u16> = (0..LOOP_MAX_FRAMES as u16).collect();
        assert_eq!(
            loop_hours(&request(at_cap, None, None, None)).unwrap().len(),
            LOOP_MAX_FRAMES
        );
        let over: Vec<u16> = (0..=LOOP_MAX_FRAMES as u16).collect();
        let err = loop_hours(&request(over, None, None, None)).expect_err("over the cap");
        assert!(err.contains(&LOOP_MAX_FRAMES.to_string()), "{err}");
        // A wide range at a coarse stride stays legal: 0-384 every 6 h is 65.
        assert_eq!(
            loop_hours(&request(vec![], Some(0), Some(384), Some(6))).unwrap().len(),
            65
        );
    }
}

#[cfg(test)]
mod card_theme_tests {
    use super::*;

    fn query(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn an_absent_theme_keeps_the_house_style() {
        let theme = card_theme_from_query(&query(&[]));
        assert_eq!(theme.brand, "CWT");
        assert_eq!(theme.credit, "cafire.org/weather");
        assert_eq!(theme.paper, "#0d1112");
    }

    #[test]
    fn a_named_theme_replaces_the_palette_and_the_branding() {
        let theme = card_theme_from_query(&query(&[("theme", "paper")]));
        assert_eq!(theme.paper, "#f7f5f0");
        assert!(theme.brand.is_empty(), "an unbranded theme carries no prefix");
        assert!(theme.credit.is_empty());
    }

    /// Present-but-empty is how a caller says "drop this line"; omitting the
    /// parameter has to keep inheriting the theme's own text, so the two cases
    /// must not collapse into one.
    #[test]
    fn empty_overrides_clear_and_absent_ones_inherit() {
        let cleared = card_theme_from_query(&query(&[("credit", ""), ("brand", "")]));
        assert!(cleared.credit.is_empty() && cleared.brand.is_empty());
        let inherited = card_theme_from_query(&query(&[("theme", "cafire")]));
        assert_eq!(inherited.credit, "cafire.org/weather");
        let replaced = card_theme_from_query(&query(&[
            ("theme", "slate"),
            ("brand", "ACME FIRE OPS"),
            ("credit", "acme.example.com"),
        ]));
        assert_eq!(replaced.brand, "ACME FIRE OPS");
        assert_eq!(replaced.credit, "acme.example.com");
    }

    #[test]
    fn only_a_real_hex_accent_is_accepted() {
        let good = card_theme_from_query(&query(&[("theme", "slate"), ("accent", "#ff00aa")]));
        assert_eq!(good.accent, "#ff00aa");
        assert_eq!(good.accent_soft, "#ff00aa");
        for junk in ["red", "#ff0", "#gggggg", "\"/><script>", "#ff00aa11"] {
            let theme = card_theme_from_query(&query(&[("theme", "slate"), ("accent", junk)]));
            assert_eq!(
                theme.accent,
                meteogram::CardTheme::named("slate").accent,
                "{junk} must not reach a fill attribute"
            );
        }
    }

    /// The reported bug: GFS is GLOBAL, yet a point picked after panning a web
    /// map past the antimeridian answered "point is outside the model grid",
    /// because Leaflet keeps counting longitude (-186, +200, +560) while every
    /// stored grid is indexed -180..180.
    #[test]
    fn longitudes_from_a_panned_map_wrap_onto_the_grid() {
        for (input, expected) in [
            (-73.97, -73.97),
            (-186.03, 173.97),
            (-433.97, -73.97),
            (200.5, -159.5),
            (560.5, -159.5),
            (0.0, 0.0),
            (-180.0, -180.0),
            (179.9, 179.9),
        ] {
            let got = wrap_longitude(input);
            assert!(
                (got - expected).abs() < 1e-9,
                "wrap_longitude({input}) = {got}, expected {expected}"
            );
        }
        // The antimeridian is one meridian; keep the sign the caller used.
        assert_eq!(wrap_longitude(180.0), 180.0);
        assert!(wrap_longitude(f64::NAN).is_nan());
    }

    #[test]
    fn a_logo_id_can_never_escape_the_logo_directory() {
        let dir = std::path::Path::new("C:/rw/out/card-logos");
        for hostile in [
            "../../etc/passwd",
            "..",
            "a/b",
            "a\\b",
            "zz",
            "",
            "%2e%2e",
            &"f".repeat(64),
        ] {
            assert!(
                card_logo_path(hostile, dir).is_none(),
                "{hostile:?} must not resolve to a file"
            );
        }
        // A real id resolves, and only inside the logo directory.
        let ok = card_logo_path("b79ab8698c6a29f9", dir).expect("valid hex id");
        assert_eq!(ok, dir.join("b79ab8698c6a29f9.png"));
    }

    /// Content addressing has to be stable (same bytes → same id) and it has to
    /// separate different logos, or one upload would serve another's card.
    #[test]
    fn logo_ids_are_stable_and_distinct() {
        assert_eq!(logo_content_id(b"one"), logo_content_id(b"one"));
        assert_ne!(logo_content_id(b"one"), logo_content_id(b"two"));
        assert_eq!(logo_content_id(b"one").len(), 16);
        assert!(logo_content_id(b"one").chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// Card text lands inside SVG `<text>`, so control characters are dropped
    /// and the length is bounded.
    #[test]
    fn card_text_is_sanitized_and_bounded() {
        let theme = card_theme_from_query(&query(&[
            ("brand", "  AC\nME\t  "),
            ("credit", &"x".repeat(200)),
        ]));
        assert_eq!(theme.brand, "ACME");
        assert_eq!(theme.credit.chars().count(), 48);
    }
}

// ---------------------------------------------------------------------------
// Loops: render an hour range as ONE job, then serve it back as frames or GIF.
//
// The Lab animated by firing one /api/render per hour from the browser. With a
// 3-slot render gate a 48-hour loop is 48 racing requests: no shared progress,
// no ordering, and no way to tell "still rendering" from "stuck". A loop is one
// server-side unit instead. It reuses the SAME per-frame render cache (hours
// already on disk are instant) and the SAME gate (a loop cannot starve
// interactive renders), and reports done/total while it works.
// ---------------------------------------------------------------------------

/// Frames one loop may span. 48 h hourly is the HRRR ceiling; the cap stops a
/// single request queueing unbounded render work.
const LOOP_MAX_FRAMES: usize = 120;

#[derive(Debug, Clone, Deserialize)]
struct LoopJobRequest {
    /// The per-frame render request; its `hour` is replaced by each frame's.
    #[serde(flatten)]
    base: RenderJobRequest,
    /// Explicit hour list; takes precedence over start/end/step when non-empty.
    #[serde(default)]
    hours: Vec<u16>,
    #[serde(default)]
    hour_start: Option<u16>,
    #[serde(default)]
    hour_end: Option<u16>,
    #[serde(default)]
    hour_step: Option<u16>,
    /// Milliseconds per frame in the exported GIF.
    #[serde(default)]
    frame_ms: Option<u16>,
    /// Width of the exported GIF. GIF is palette-limited and grows with area, so
    /// the export downscales by default instead of shipping a ~100 MB file.
    #[serde(default)]
    gif_width: Option<u32>,
    /// Width of the exported VIDEO. Defaults to 0 = keep the rendered frame's
    /// native size: H.264 handles full-resolution maps in a few MB, so there is
    /// no reason to throw away detail the way GIF forces us to.
    #[serde(default)]
    video_width: Option<u32>,
    /// x264/VP9 quality (CRF). Lower is better; 18 is visually near-lossless for
    /// flat map fills, 23 left visible mush in the colorbar and thin linework.
    #[serde(default)]
    video_crf: Option<u8>,
}

#[derive(Debug, Clone)]
struct LoopFrame {
    hour: u16,
    job_id: String,
}

#[derive(Debug, Clone)]
struct LoopJob {
    id: String,
    frames: Vec<LoopFrame>,
    frame_ms: u16,
    gif_width: u32,
    /// 0 = native frame size.
    video_width: u32,
    video_crf: u8,
    created_unix_ms: u128,
}

fn loop_hours(request: &LoopJobRequest) -> Result<Vec<u16>, String> {
    let mut hours: Vec<u16> = if !request.hours.is_empty() {
        request.hours.clone()
    } else {
        let start = request.hour_start.unwrap_or(0);
        let end = request
            .hour_end
            .ok_or("a loop needs `hours` or `hour_start`/`hour_end`")?;
        if end < start {
            return Err("hour_end must be >= hour_start".to_string());
        }
        let step = request.hour_step.unwrap_or(1).max(1);
        (start..=end).step_by(usize::from(step)).collect()
    };
    hours.sort_unstable();
    hours.dedup();
    if hours.is_empty() {
        return Err("a loop needs at least one hour".to_string());
    }
    if hours.len() > LOOP_MAX_FRAMES {
        return Err(format!(
            "a loop is capped at {LOOP_MAX_FRAMES} frames; asked for {}",
            hours.len()
        ));
    }
    Ok(hours)
}

fn start_loop_job(body: Vec<u8>, state: AppState) -> Vec<u8> {
    let parsed = match serde_json::from_slice::<LoopJobRequest>(&body) {
        Ok(parsed) => parsed,
        Err(err) => {
            return json_status_response(
                400,
                &serde_json::json!({ "error": format!("invalid JSON body: {err}") }),
            );
        }
    };
    let hours = match loop_hours(&parsed) {
        Ok(hours) => hours,
        Err(message) => return json_status_response(400, &serde_json::json!({ "error": message })),
    };

    // Each frame goes through the ordinary render path, so a loop inherits
    // validation, alias resolution, the render cache and the gate for free.
    let mut frames = Vec::with_capacity(hours.len());
    for hour in hours {
        let mut per_frame = parsed.base.clone();
        per_frame.hour = hour;
        match enqueue_render_job(per_frame, &state) {
            Ok(job_id) => frames.push(LoopFrame { hour, job_id }),
            Err((status, message)) => {
                return json_status_response(status, &serde_json::json!({ "error": message }));
            }
        }
    }

    let id = format!("loop-{}", next_job_id(&state));
    let frame_count = frames.len();
    let job = LoopJob {
        id: id.clone(),
        frames,
        frame_ms: parsed.frame_ms.unwrap_or(220).clamp(20, 2000),
        gif_width: parsed.gif_width.unwrap_or(1000).clamp(200, 2000),
        video_width: parsed.video_width.map(|w| w.clamp(200, 4096)).unwrap_or(0),
        video_crf: parsed.video_crf.unwrap_or(18).clamp(0, 51),
        created_unix_ms: unix_ms_now(),
    };
    state
        .loops
        .lock()
        .expect("loop mutex")
        .insert(id.clone(), job);

    json_status_response(
        202,
        &serde_json::json!({
            "id": id,
            "frames": frame_count,
            "status_url": format!("/api/loops/{id}"),
            "gif_url": format!("/api/loops/{id}/animation.gif"),
            "mp4_url": format!("/api/loops/{id}/animation.mp4"),
            "webm_url": format!("/api/loops/{id}/animation.webm"),
        }),
    )
}

fn loop_response(tail: &str, state: &AppState) -> Vec<u8> {
    enum Export {
        Status,
        Gif,
        Mp4,
        Webm,
    }
    let (id, export) = if let Some(id) = tail.strip_suffix("/animation.gif") {
        (id, Export::Gif)
    } else if let Some(id) = tail.strip_suffix("/animation.mp4") {
        (id, Export::Mp4)
    } else if let Some(id) = tail.strip_suffix("/animation.webm") {
        (id, Export::Webm)
    } else {
        (tail.trim_end_matches('/'), Export::Status)
    };
    let job = {
        let loops = state.loops.lock().expect("loop mutex");
        match loops.get(id) {
            Some(job) => job.clone(),
            None => {
                return json_status_response(
                    404,
                    &serde_json::json!({ "error": "unknown loop id" }),
                );
            }
        }
    };
    match export {
        Export::Gif => return loop_gif_response(&job, state),
        Export::Mp4 => return loop_video_response(&job, state, false),
        Export::Webm => return loop_video_response(&job, state, true),
        Export::Status => {}
    }

    let jobs = state.jobs.lock().expect("job mutex");
    let mut done = 0usize;
    let mut failed = 0usize;
    let frames: Vec<serde_json::Value> = job
        .frames
        .iter()
        .map(|frame| {
            let child = jobs.get(&frame.job_id);
            let child_state = child.map(|job| job.state.clone()).unwrap_or(JobState::Queued);
            let url = child
                .and_then(|job| job.files.first())
                .map(|file| file.url.clone());
            match child_state {
                JobState::Succeeded => done += 1,
                JobState::Failed => failed += 1,
                _ => {}
            }
            serde_json::json!({
                "hour": frame.hour,
                "state": child_state,
                "job_id": frame.job_id,
                "url": url,
            })
        })
        .collect();
    let total = job.frames.len();
    let state_word = if failed > 0 && done + failed == total {
        "failed"
    } else if done == total {
        "succeeded"
    } else {
        "running"
    };
    json_response(&serde_json::json!({
        "id": job.id,
        "state": state_word,
        "done": done,
        "failed": failed,
        "total": total,
        "frame_ms": job.frame_ms,
        "gif_url": format!("/api/loops/{}/animation.gif", job.id),
        "mp4_url": format!("/api/loops/{}/animation.mp4", job.id),
        "webm_url": format!("/api/loops/{}/animation.webm", job.id),
        "created_unix_ms": job.created_unix_ms,
        "frames": frames,
    }))
}

/// Assemble the loop's finished frames into an animated GIF.
///
/// Only rendered frames are included, in hour order, so a partially finished
/// loop still previews rather than 404ing.
/// Rendered frame files in hour order. Unfinished frames are skipped, so a
/// partially complete loop still exports what exists rather than failing.
fn loop_frame_paths(job: &LoopJob, state: &AppState) -> Vec<PathBuf> {
    let jobs = state.jobs.lock().expect("job mutex");
    job.frames
        .iter()
        .filter_map(|frame| {
            let child = jobs.get(&frame.job_id)?;
            if child.state != JobState::Succeeded {
                return None;
            }
            let file = child.files.first()?;
            Some(PathBuf::from(&child.output_dir).join(&file.name))
        })
        .collect()
}

fn loop_gif_response(job: &LoopJob, state: &AppState) -> Vec<u8> {
    let paths = loop_frame_paths(job, state);
    if paths.is_empty() {
        return json_status_response(
            409,
            &serde_json::json!({ "error": "no frames rendered yet; poll the status url first" }),
        );
    }

    let mut buffer: Vec<u8> = Vec::new();
    {
        use image::codecs::gif::{GifEncoder, Repeat};
        use image::{Delay, Frame};
        let mut encoder = GifEncoder::new_with_speed(std::io::Cursor::new(&mut buffer), 12);
        if encoder.set_repeat(Repeat::Infinite).is_err() {
            return json_status_response(
                500,
                &serde_json::json!({ "error": "gif encoder rejected the repeat setting" }),
            );
        }
        let delay = Delay::from_numer_denom_ms(u32::from(job.frame_ms), 1);
        for path in &paths {
            let Ok(image) = image::open(path) else {
                continue;
            };
            let scaled = if image.width() > job.gif_width {
                let height = (u64::from(image.height()) * u64::from(job.gif_width)
                    / u64::from(image.width().max(1))) as u32;
                image.resize_exact(
                    job.gif_width,
                    height.max(1),
                    image::imageops::FilterType::Triangle,
                )
            } else {
                image
            };
            if encoder
                .encode_frame(Frame::from_parts(scaled.to_rgba8(), 0, 0, delay))
                .is_err()
            {
                return json_status_response(
                    500,
                    &serde_json::json!({ "error": "gif frame encode failed" }),
                );
            }
        }
    }

    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: image/gif\r\nContent-Length: {}\r\nCache-Control: public, max-age=300\r\nContent-Disposition: attachment; filename=\"{}.gif\"\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        buffer.len(),
        job.id
    );
    let mut response = header.into_bytes();
    response.extend_from_slice(&buffer);
    response
}

/// Assemble the loop's finished frames into an H.264 MP4 via ffmpeg.
///
/// GIF is 256 colors and grows with area — a 48-frame CONUS loop is tens of MB
/// and visibly bands the smooth temperature ramps. H.264 is a few MB for the
/// same loop and keeps the gradients, so MP4 is the default export and GIF is
/// the fallback for places that will only take an image.
///
/// Frames are symlink-free copies into a per-request temp dir named `%05d.png`
/// because ffmpeg's image2 demuxer wants a numeric sequence, and the render job
/// dirs are neither contiguous nor ordered.
fn loop_video_response(job: &LoopJob, state: &AppState, webm: bool) -> Vec<u8> {
    let paths = loop_frame_paths(job, state);
    if paths.is_empty() {
        return json_status_response(
            409,
            &serde_json::json!({ "error": "no frames rendered yet; poll the status url first" }),
        );
    }

    let work = state.out_root.join(format!("{}-video", job.id));
    let _ = std::fs::remove_dir_all(&work);
    if let Err(err) = std::fs::create_dir_all(&work) {
        return json_status_response(
            500,
            &serde_json::json!({ "error": format!("video workdir: {err}") }),
        );
    }
    for (index, path) in paths.iter().enumerate() {
        let target = work.join(format!("{index:05}.png"));
        // Re-encode to PNG rather than copying: frames may be WebP, which the
        // image2 demuxer will not read from a .png name.
        match image::open(path) {
            Ok(image) => {
                if image.save(&target).is_err() {
                    let _ = std::fs::remove_dir_all(&work);
                    return json_status_response(
                        500,
                        &serde_json::json!({ "error": "could not stage a frame for encoding" }),
                    );
                }
            }
            Err(_) => continue,
        }
    }

    let fps = (1000.0 / f64::from(job.frame_ms.max(20))).clamp(1.0, 30.0);
    let out_name = if webm { "animation.webm" } else { "animation.mp4" };
    let out_path = work.join(out_name);
    // yuv420p needs even dimensions; the scale filter floors both axes to even.
    let mut command = std::process::Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-framerate")
        .arg(format!("{fps:.4}"))
        .arg("-i")
        .arg(work.join("%05d.png"))
        .arg("-vf")
        .arg(if job.video_width == 0 {
            // Native size; only force even dimensions for yuv420p.
            "scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string()
        } else {
            format!(
                "scale='min({width},iw)':-2:flags=lanczos,pad=ceil(iw/2)*2:ceil(ih/2)*2",
                width = job.video_width
            )
        })
        .arg("-pix_fmt")
        .arg("yuv420p");
    if webm {
        command
            .arg("-c:v")
            .arg("libvpx-vp9")
            .arg("-b:v")
            .arg("0")
            .arg("-crf")
            .arg(job.video_crf.to_string())
            .arg("-row-mt")
            .arg("1");
    } else {
        command
            .arg("-c:v")
            .arg("libx264")
            // `slow` over `veryfast`: these are a handful of frames, the encode
            // is not the bottleneck, and it buys real quality at the same CRF.
            .arg("-preset")
            .arg("slow")
            .arg("-crf")
            .arg(job.video_crf.to_string())
            // 4:2:0 chroma subsampling smears the saturated colorbar edges; the
            // high profile keeps 8x8 transforms which helps thin linework.
            .arg("-profile:v")
            .arg("high")
            .arg("-tune")
            .arg("stillimage")
            .arg("-movflags")
            .arg("+faststart");
    }
    command.arg(&out_path);

    let output = match command.output() {
        Ok(output) => output,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&work);
            return json_status_response(
                500,
                &serde_json::json!({
                    "error": format!("ffmpeg could not be started ({err}); is ffmpeg installed?")
                }),
            );
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.chars().rev().take(400).collect::<String>().chars().rev().collect();
        let _ = std::fs::remove_dir_all(&work);
        return json_status_response(
            500,
            &serde_json::json!({ "error": format!("ffmpeg failed: {tail}") }),
        );
    }
    let bytes = match std::fs::read(&out_path) {
        Ok(bytes) => bytes,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&work);
            return json_status_response(
                500,
                &serde_json::json!({ "error": format!("read encoded video: {err}") }),
            );
        }
    };
    let _ = std::fs::remove_dir_all(&work);

    let mime = if webm { "video/webm" } else { "video/mp4" };
    let ext = if webm { "webm" } else { "mp4" };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: public, max-age=300\r\nContent-Disposition: attachment; filename=\"{}.{ext}\"\r\nAccess-Control-Allow-Origin: *\r\n\r\n",
        bytes.len(),
        job.id
    );
    let mut response = header.into_bytes();
    response.extend_from_slice(&bytes);
    response
}

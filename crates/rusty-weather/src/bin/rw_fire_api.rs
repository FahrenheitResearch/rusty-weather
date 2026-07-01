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

const MIN_RENDER_WIDTH: u32 = 1200;
const MIN_RENDER_HEIGHT: u32 = 900;
const MAX_RENDER_DIMENSION: u32 = 2400;

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
}

#[derive(Clone)]
struct AppState {
    store_root: PathBuf,
    out_root: PathBuf,
    rw_render: PathBuf,
    render_threads: Option<usize>,
    full_throttle_render: bool,
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    render_cache: Arc<Mutex<HashMap<String, String>>>,
    counter: Arc<AtomicU64>,
    render_gate: Arc<RenderGate>,
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
    #[serde(default = "default_domain_slug")]
    domain_slug: String,
    bounds: [f64; 4],
    #[serde(default)]
    output_width: Option<u32>,
    #[serde(default)]
    output_height: Option<u32>,
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
        jobs: Arc::new(Mutex::new(HashMap::new())),
        render_cache: Arc::new(Mutex::new(HashMap::new())),
        counter: Arc::new(AtomicU64::new(1)),
        render_gate: Arc::new(RenderGate::new(args.max_render_jobs)),
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
        ("GET", "/") => html_response(DEMO_HTML),
        ("GET", "/api/health") => json_response(&serde_json::json!({
            "ok": true,
            "service": "rw-fire-api",
            "store_root": state.store_root.display().to_string(),
            "out_root": state.out_root.display().to_string(),
            "rw_render": state.rw_render.display().to_string(),
            "render_threads": state.render_threads,
            "full_throttle_render": state.full_throttle_render,
            "render_gate": state.render_gate.snapshot(),
            "render_cache_entries": state.render_cache.lock().expect("render cache mutex").len(),
        })),
        ("POST", "/api/render") => start_render_job(request.body, state),
        ("OPTIONS", _) => empty_response(204),
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
    let request = match parsed {
        Ok(request) => request,
        Err(message) => return json_status_response(400, &serde_json::json!({ "error": message })),
    };

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
        &format!("--domain-bounds={}", format_bounds(request.bounds)),
        "--domain-slug",
        &safe_slug(&request.domain_slug),
        "--png-compression",
        "fastest",
        "--place-label-density",
        "1",
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
    command.env("RUSTWX_PLOT_STYLE", "operational-fast");
    command.env("RUSTWX_STATIC_OUTPUT_WIDTH", width.to_string());
    command.env("RUSTWX_STATIC_OUTPUT_HEIGHT", height.to_string());

    let output = command.output().map_err(|err| {
        (
            format!("launch {}: {err}", state.rw_render.display()),
            String::new(),
            String::new(),
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout_tail = tail_lines(&stdout, 24);
    let stderr_tail = tail_lines(&stderr, 24);
    if !output.status.success() {
        return Err((
            format!("rw_render exited with {}", output.status),
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
    Ok((files, stdout_tail, stderr_tail))
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

fn validate_render_request(mut request: RenderJobRequest) -> Result<RenderJobRequest, String> {
    request.model = safe_model_slug(&request.model);
    request.output_format = request.output_format.trim().to_ascii_lowercase();
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
    let [west, east, south, north] = request.bounds;
    if !west.is_finite() || !east.is_finite() || !south.is_finite() || !north.is_finite() {
        return Err("bounds must be finite west,east,south,north values".to_string());
    }
    if west < -360.0 || east > 360.0 || south < -90.0 || north > 90.0 || south >= north {
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
    let path = raw_path.split('?').next().unwrap_or("/").to_string();
    let mut content_length = 0usize;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = value.trim().parse::<usize>().unwrap_or(0);
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
    Ok(HttpRequest { method, path, body })
}

fn find_header_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

fn json_response(value: &impl Serialize) -> Vec<u8> {
    json_status_response(200, value)
}

fn json_status_response(status: u16, value: &impl Serialize) -> Vec<u8> {
    let body =
        serde_json::to_vec_pretty(value).unwrap_or_else(|_| b"{\"error\":\"json\"}".to_vec());
    response(status, "application/json; charset=utf-8", body)
}

fn html_response(body: &str) -> Vec<u8> {
    response(200, "text/html; charset=utf-8", body.as_bytes().to_vec())
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
    let status_text = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        _ => "OK",
    };
    let mut out = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Headers: content-type\r\nAccess-Control-Allow-Methods: GET,POST,OPTIONS\r\nConnection: close\r\n\r\n",
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
    let [west, east, south, north] = request.bounds;
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
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}x{}",
        request.model,
        request.run,
        request.hour,
        request.products.trim(),
        request.output_format,
        request.domain_slug,
        format_bounds(request.bounds),
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

fn default_domain_slug() -> String {
    "drawn_box".to_string()
}

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
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-core".to_string(),
            output_format: "webp".to_string(),
            domain_slug: "bad".to_string(),
            bounds: [-123.0, -120.0, 40.0, 37.0],
            output_width: None,
            output_height: None,
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn request_validation_rejects_unknown_output_format() {
        let request = RenderJobRequest {
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-core".to_string(),
            output_format: "jpeg".to_string(),
            domain_slug: "box".to_string(),
            bounds: [-123.21, -119.67, 37.13, 41.14],
            output_width: Some(800),
            output_height: None,
        };
        assert!(validate_render_request(request).is_err());
    }

    #[test]
    fn output_size_derives_height_from_width_only_preview() {
        let request = RenderJobRequest {
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-core".to_string(),
            output_format: "webp".to_string(),
            domain_slug: "box".to_string(),
            bounds: [-123.21, -119.67, 37.13, 41.14],
            output_width: Some(1000),
            output_height: None,
        };
        assert_eq!(output_size(&request), (1200, 1359));
    }

    #[test]
    fn output_size_keeps_explicit_dimensions() {
        let request = RenderJobRequest {
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-core".to_string(),
            output_format: "webp".to_string(),
            domain_slug: "box".to_string(),
            bounds: [-123.21, -119.67, 37.13, 41.14],
            output_width: Some(1000),
            output_height: Some(700),
        };
        assert_eq!(output_size(&request), (1200, 900));
    }

    #[test]
    fn render_cache_key_uses_clamped_output_size() {
        let small = RenderJobRequest {
            model: "hrrr".to_string(),
            run: "20260629_03z".to_string(),
            hour: 3,
            products: "cafire-with-fuels".to_string(),
            output_format: "webp".to_string(),
            domain_slug: "box".to_string(),
            bounds: [-123.21, -119.67, 37.13, 41.14],
            output_width: Some(800),
            output_height: None,
        };
        let clamped = RenderJobRequest {
            output_width: Some(1200),
            ..small.clone()
        };
        assert_eq!(render_cache_key(&small), render_cache_key(&clamped));
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
}

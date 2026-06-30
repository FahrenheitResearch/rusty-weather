//! Background static-map renderer for CAFire-style store-backed products.
//!
//! The request shape intentionally mirrors the future web/API job: a stored
//! model run, a forecast hour, a product preset, and a named geographic box.

use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;

#[derive(Debug, Clone)]
pub struct FireWeatherRenderRequest {
    pub model: String,
    pub run: String,
    pub hour: u16,
    pub store_root: PathBuf,
    pub out_dir: PathBuf,
    pub products: String,
    pub domain_slug: String,
    pub bounds: (f64, f64, f64, f64),
    pub output_width: u32,
    pub output_height: u32,
}

#[derive(Debug, Clone)]
pub enum FireWeatherRenderResponse {
    Started(FireWeatherRenderRequest),
    Finished {
        request: FireWeatherRenderRequest,
        stdout_tail: String,
        stderr_tail: String,
    },
    Failed {
        request: FireWeatherRenderRequest,
        message: String,
        stdout_tail: String,
        stderr_tail: String,
    },
}

pub struct FireWeatherWorker {
    tx: Sender<FireWeatherRenderRequest>,
    rx: Receiver<FireWeatherRenderResponse>,
    _thread: JoinHandle<()>,
}

impl FireWeatherWorker {
    pub fn spawn(notify: impl Fn() + Send + Sync + 'static) -> Self {
        let (req_tx, req_rx) = channel::<FireWeatherRenderRequest>();
        let (resp_tx, resp_rx) = channel::<FireWeatherRenderResponse>();
        let thread = std::thread::Builder::new()
            .name("rw-fire-weather-render".to_string())
            .spawn(move || worker_loop(&req_rx, &resp_tx, &notify))
            .expect("spawn fire weather render worker");
        Self {
            tx: req_tx,
            rx: resp_rx,
            _thread: thread,
        }
    }

    pub fn send(&self, request: FireWeatherRenderRequest) {
        let _ = self.tx.send(request);
    }

    pub fn try_recv(&self) -> Option<FireWeatherRenderResponse> {
        self.rx.try_recv().ok()
    }
}

fn worker_loop(
    requests: &Receiver<FireWeatherRenderRequest>,
    responses: &Sender<FireWeatherRenderResponse>,
    notify: &(impl Fn() + Send + Sync + 'static),
) {
    while let Ok(request) = requests.recv() {
        let _ = responses.send(FireWeatherRenderResponse::Started(request.clone()));
        notify();
        let output = run_render(&request);
        let response = match output {
            Ok((stdout_tail, stderr_tail)) => FireWeatherRenderResponse::Finished {
                request,
                stdout_tail,
                stderr_tail,
            },
            Err((message, stdout_tail, stderr_tail)) => FireWeatherRenderResponse::Failed {
                request,
                message,
                stdout_tail,
                stderr_tail,
            },
        };
        let _ = responses.send(response);
        notify();
    }
}

fn run_render(
    request: &FireWeatherRenderRequest,
) -> Result<(String, String), (String, String, String)> {
    if let Err(err) = std::fs::create_dir_all(&request.out_dir) {
        return Err((
            format!("create {}: {err}", request.out_dir.display()),
            String::new(),
            String::new(),
        ));
    }

    let args = render_args(request);
    let mut command = if let Some(exe) = sibling_rw_render_path().filter(|path| path.exists()) {
        let mut command = Command::new(exe);
        command.args(&args);
        command
    } else {
        let mut cargo_args = vec![
            "run".to_string(),
            "-p".to_string(),
            "rusty-weather".to_string(),
            "--bin".to_string(),
            "rw_render".to_string(),
            "--".to_string(),
        ];
        cargo_args.extend(args);
        let mut command = Command::new("cargo");
        command.args(cargo_args);
        command
    };

    command.env("RUSTWX_PROJECTED_FRAME_SOURCE", "requested");
    command.env("RUSTWX_PROJECTION_VARIANT", "mercator");
    command.env("RUSTWX_PLOT_STYLE", "operational-fast");
    command.env(
        "RUSTWX_STATIC_OUTPUT_WIDTH",
        request.output_width.to_string(),
    );
    command.env(
        "RUSTWX_STATIC_OUTPUT_HEIGHT",
        request.output_height.to_string(),
    );

    let output = command.output().map_err(|err| {
        (
            format!("launch rw_render: {err}"),
            String::new(),
            String::new(),
        )
    })?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout_tail = tail_lines(&stdout, 12);
    let stderr_tail = tail_lines(&stderr, 12);
    if output.status.success() {
        Ok((stdout_tail, stderr_tail))
    } else {
        Err((
            format!("rw_render exited with {}", output.status),
            stdout_tail,
            stderr_tail,
        ))
    }
}

fn render_args(request: &FireWeatherRenderRequest) -> Vec<String> {
    vec![
        "--model".to_string(),
        request.model.clone(),
        "--run".to_string(),
        request.run.clone(),
        "--hour".to_string(),
        request.hour.to_string(),
        "--store-root".to_string(),
        request.store_root.display().to_string(),
        "--out-dir".to_string(),
        request.out_dir.display().to_string(),
        "--products".to_string(),
        request.products.clone(),
        "--domain-bounds".to_string(),
        format_bounds(request.bounds),
        "--domain-slug".to_string(),
        request.domain_slug.clone(),
        "--png-compression".to_string(),
        "fastest".to_string(),
        "--place-label-density".to_string(),
        "1".to_string(),
    ]
}

fn sibling_rw_render_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    #[cfg(windows)]
    let name = "rw_render.exe";
    #[cfg(not(windows))]
    let name = "rw_render";
    Some(dir.join(name))
}

fn format_bounds(bounds: (f64, f64, f64, f64)) -> String {
    format!(
        "{:.6},{:.6},{:.6},{:.6}",
        bounds.0, bounds.1, bounds.2, bounds.3
    )
}

fn tail_lines(value: &str, max_lines: usize) -> String {
    let lines = value.lines().collect::<Vec<_>>();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_args_include_custom_domain_bounds() {
        let request = FireWeatherRenderRequest {
            model: "hrrr".to_string(),
            run: "20260629_00z".to_string(),
            hour: 3,
            store_root: PathBuf::from("store"),
            out_dir: PathBuf::from("out/fire"),
            products: "cafire-all".to_string(),
            domain_slug: "napa_box".to_string(),
            bounds: (-123.5, -120.25, 37.0, 39.5),
            output_width: 1400,
            output_height: 1000,
        };
        let args = render_args(&request);
        assert!(has_arg_pair(&args, "--domain-slug", "napa_box"));
        assert!(has_arg_pair(
            &args,
            "--domain-bounds",
            "-123.500000,-120.250000,37.000000,39.500000"
        ));
    }

    fn has_arg_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2)
            .any(|pair| pair[0] == flag && pair[1] == value)
    }
}

//! Compact benchmark harness for the store-to-PNG renderer.
//!
//! This intentionally shells out to the sibling `rw_render` binary instead of
//! linking the render flow directly. Each run gets isolated environment knobs
//! (`RUSTWX_RENDER_THREADS`, output dimensions) while benchmarking the exact
//! executable a model-map graphics worker would run.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(
    name = "rw-render-bench",
    about = "Benchmark rw_render over one stored hour with conservative defaults"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: String,
    #[arg(long, help = "Run slug as stored, e.g. 20260629_05z")]
    run: String,
    #[arg(long, help = "Forecast hour of the stored .rws file")]
    hour: u16,
    #[arg(long, value_delimiter = ',', default_value = "all")]
    products: Vec<String>,
    #[arg(
        long = "render-workers",
        value_delimiter = ',',
        default_value = "8",
        help = "Comma-separated sweep, e.g. 1,4,8,16. Defaults to one safe sample."
    )]
    render_workers: Vec<usize>,
    #[arg(long, value_delimiter = ',', default_value = "1280x720")]
    sizes: Vec<OutputSize>,
    #[arg(long, default_value = "conus")]
    region: String,
    #[arg(long, default_value = "out/rw_render_bench")]
    out_dir: PathBuf,
    #[arg(long = "png-compression", default_value = "fastest")]
    png_compression: String,
    #[arg(
        long,
        help = "Rayon thread count passed through to rw_render. Omit for rw_render's polite default."
    )]
    threads: Option<usize>,
    #[arg(
        long,
        default_value_t = false,
        help = "Pass --full-throttle through to rw_render. Leave off on an interactive desktop."
    )]
    full_throttle: bool,
    #[arg(long, default_value_t = 1)]
    iterations: usize,
}

#[derive(Debug, Clone, Copy)]
struct OutputSize {
    width: u32,
    height: u32,
}

impl std::str::FromStr for OutputSize {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (width, height) = value
            .split_once('x')
            .or_else(|| value.split_once('X'))
            .ok_or_else(|| format!("size '{value}' must be WIDTHxHEIGHT"))?;
        let width = width
            .parse::<u32>()
            .map_err(|err| format!("size '{value}' width: {err}"))?;
        let height = height
            .parse::<u32>()
            .map_err(|err| format!("size '{value}' height: {err}"))?;
        if width < 320 || height < 180 {
            return Err(format!("size '{value}' is too small"));
        }
        Ok(Self { width, height })
    }
}

impl std::fmt::Display for OutputSize {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let renderer = sibling_renderer_exe()?;
    if !renderer.is_file() {
        return Err(format!(
            "{} is missing; build it with `cargo build --profile release-fast -p rusty-weather --bin rw_render`",
            renderer.display()
        )
        .into());
    }

    println!(
        "renderer {} | store {} | run {} f{:03} | iterations {}",
        renderer.display(),
        args.store_root.display(),
        args.run,
        args.hour,
        args.iterations.max(1),
    );
    println!(
        "{:<10} {:<9} {:>7} {:>10} {:>10} {:>8} {:>8}  summary",
        "products", "size", "workers", "wall_ms", "rw_ms", "rendered", "skipped"
    );

    for size in &args.sizes {
        for products in &args.products {
            for &workers in &args.render_workers {
                let mut samples = Vec::new();
                let mut last = None;
                for iteration in 0..args.iterations.max(1) {
                    let run_out = args.out_dir.join(format!(
                        "{}_{}_t{}_i{}",
                        products.replace(',', "+"),
                        size,
                        workers,
                        iteration + 1
                    ));
                    let result = run_once(&renderer, &args, *size, products, workers, run_out)?;
                    samples.push(result.wall_ms);
                    last = Some(result);
                }
                let result = last.expect("at least one sample");
                println!(
                    "{:<10} {:<9} {:>7} {:>10} {:>10} {:>8} {:>8}  {}",
                    products,
                    size,
                    workers,
                    median(&mut samples),
                    result
                        .renderer_wall_ms
                        .map_or("-".to_string(), |ms| ms.to_string()),
                    result
                        .rendered
                        .map_or("-".to_string(), |count| count.to_string()),
                    result
                        .skipped
                        .map_or("-".to_string(), |count| count.to_string()),
                    result.summary
                );
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
struct BenchRun {
    wall_ms: u128,
    renderer_wall_ms: Option<u128>,
    rendered: Option<usize>,
    skipped: Option<usize>,
    summary: String,
}

fn run_once(
    renderer: &std::path::Path,
    args: &Args,
    size: OutputSize,
    products: &str,
    workers: usize,
    out_dir: PathBuf,
) -> Result<BenchRun, Box<dyn std::error::Error>> {
    let _ = std::fs::remove_dir_all(&out_dir);
    let started = Instant::now();
    let mut command = Command::new(renderer);
    command
        .arg("--store-root")
        .arg(&args.store_root)
        .arg("--model")
        .arg(&args.model)
        .arg("--run")
        .arg(&args.run)
        .arg("--hour")
        .arg(args.hour.to_string())
        .arg("--products")
        .arg(products)
        .arg("--region")
        .arg(&args.region)
        .arg("--out-dir")
        .arg(out_dir)
        .arg("--png-compression")
        .arg(&args.png_compression);
    if args.full_throttle {
        command.arg("--full-throttle");
    }
    if let Some(threads) = args.threads {
        command.arg("--threads").arg(threads.to_string());
    }
    let output = command
        .env("RUSTWX_RENDER_THREADS", workers.to_string())
        .env("RUSTWX_STATIC_OUTPUT_WIDTH", size.width.to_string())
        .env("RUSTWX_STATIC_OUTPUT_HEIGHT", size.height.to_string())
        .output()?;
    let wall_ms = started.elapsed().as_millis();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "rw_render failed for products={products}, size={size}, workers={workers}\nstdout:\n{stdout}\nstderr:\n{stderr}"
        )
        .into());
    }

    let summary = stdout
        .lines()
        .find(|line| line.starts_with("rendered "))
        .unwrap_or("rendered ? products")
        .to_string();
    Ok(BenchRun {
        wall_ms,
        renderer_wall_ms: parse_after(&summary, "total wall ", " ms"),
        rendered: parse_between(&summary, "rendered ", " products"),
        skipped: stderr
            .lines()
            .find_map(|line| parse_between(line, "unresolvable products (", "):")),
        summary,
    })
}

fn sibling_renderer_exe() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let suffix = std::env::consts::EXE_SUFFIX;
    Ok(std::env::current_exe()?.with_file_name(format!("rw_render{suffix}")))
}

fn parse_between<T>(text: &str, before: &str, after: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    let start = text.find(before)? + before.len();
    let tail = &text[start..];
    let end = tail.find(after)?;
    tail[..end].trim().parse().ok()
}

fn parse_after<T>(text: &str, before: &str, after: &str) -> Option<T>
where
    T: std::str::FromStr,
{
    parse_between(text, before, after)
}

fn median(values: &mut [u128]) -> u128 {
    values.sort_unstable();
    values[values.len() / 2]
}

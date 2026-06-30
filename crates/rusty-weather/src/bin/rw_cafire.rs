//! CAFire-oriented Rust runner over the `.rws` pipeline.
//!
//! This binary is intentionally a thin operator layer around `rw_batch`:
//! it chooses CAFire defaults (domains + products), resolves a run, launches
//! the Rust ingest/store/render pipeline, and writes a CAFire manifest that
//! points at the static-map PNGs. WxStore and the old Python wheel path stay
//! out of the loop.

use std::path::PathBuf;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[path = "../region.rs"]
mod region;
#[path = "../render_all.rs"]
mod render_all;

use clap::{Parser, ValueEnum};
use region::RegionPreset;
use rustwx_core::{ModelId, SourceId};
use rustwx_models::latest_available_run_at_forecast_hour;
use rw_ingest::parse_hours;

const DEFAULT_PRODUCTS: &str = "cafire-all";
const INGEST_PRODUCTS: &str = "none";
const CAFIRE_PROJECTED_FRAME_SOURCE: &str = "requested";
const CAFIRE_PROJECTION_VARIANT: &str = "mercator";
const CAFIRE_PLOT_STYLE: &str = "operational-fast";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum CafireDomainArg {
    California,
    WideWest,
}

impl CafireDomainArg {
    fn slug(self) -> &'static str {
        match self {
            Self::California => "california",
            Self::WideWest => "wide_west",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::California => "California",
            Self::WideWest => "Wide West",
        }
    }

    fn region(self) -> RegionPreset {
        match self {
            Self::California => RegionPreset::CafireCalifornia,
            Self::WideWest => RegionPreset::CafireWideWest,
        }
    }

    fn region_arg(self) -> &'static str {
        match self {
            Self::California => "cafire-california",
            Self::WideWest => "cafire-wide-west",
        }
    }

    fn output_size(self) -> (u32, u32) {
        match self {
            Self::California => (1400, 1696),
            Self::WideWest => (1800, 1200),
        }
    }

    fn render_env(self) -> Vec<(&'static str, String)> {
        let (width, height) = self.output_size();
        vec![
            (
                "RUSTWX_PROJECTED_FRAME_SOURCE",
                CAFIRE_PROJECTED_FRAME_SOURCE.to_string(),
            ),
            (
                "RUSTWX_PROJECTION_VARIANT",
                CAFIRE_PROJECTION_VARIANT.to_string(),
            ),
            ("RUSTWX_PLOT_STYLE", CAFIRE_PLOT_STYLE.to_string()),
            ("RUSTWX_STATIC_OUTPUT_WIDTH", width.to_string()),
            ("RUSTWX_STATIC_OUTPUT_HEIGHT", height.to_string()),
        ]
    }

    fn bounds(self) -> (f64, f64, f64, f64) {
        self.region().bounds()
    }

    fn internal_slug(self) -> &'static str {
        match self {
            Self::California => "cafire_california",
            Self::WideWest => "cafire_wide_west",
        }
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "rw-cafire",
    about = "CAFire runner: HRRR ingest -> .rws stores -> RustWX-quality static maps"
)]
struct Args {
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run date as YYYYMMDD; omit with --cycle to probe latest")]
    date: Option<String>,
    #[arg(long, help = "Run cycle hour UTC; omit with --date to probe latest")]
    cycle: Option<u8>,
    #[arg(
        long,
        help = "Forecast hours: \"3\", \"1,2,3\", or \"1-3\"",
        default_value = "1-3"
    )]
    hours: String,
    #[arg(long, help = "Pin one fetch source; default probes/uses catalog order")]
    source: Option<SourceId>,
    #[arg(
        long = "domain",
        value_enum,
        value_delimiter = ',',
        default_value = "california,wide-west",
        help = "CAFire domain(s): california,wide-west"
    )]
    domains: Vec<CafireDomainArg>,
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value = "out/cafire")]
    out_dir: PathBuf,
    #[arg(
        long,
        default_value = DEFAULT_PRODUCTS,
        help = "Product preset or slug list. CAFire presets: cafire-core, cafire-all, cafire-expanded"
    )]
    products: String,
    #[arg(
        long,
        default_value = "view",
        help = "rw_batch ingest profile; view keeps all 2D/derived grids without ECAPE volumes"
    )]
    profile: String,
    #[arg(long, default_value_t = true)]
    no_heavy: bool,
    #[arg(long, default_value = "automatic")]
    contour_mode: String,
    #[arg(long = "png-compression", default_value = "fast")]
    png_compression: String,
    #[arg(long = "place-label-density", default_value_t = 1)]
    place_label_density: u8,
    #[arg(long)]
    threads: Option<usize>,
    #[arg(long, default_value_t = false)]
    full_throttle: bool,
    #[arg(long, default_value_t = false)]
    list_products: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Print rw_batch commands without running them"
    )]
    dry_run: bool,
}

#[derive(Debug, Clone)]
struct ResolvedRun {
    date_yyyymmdd: String,
    cycle_utc: u8,
    source: Option<SourceId>,
    auto_resolved: bool,
}

#[derive(Debug, Clone)]
struct ProductGroups {
    hour: Vec<String>,
    windowed: Vec<String>,
    hour_render_spec: Option<String>,
    windowed_render_spec: Option<String>,
}

impl ProductGroups {
    fn expanded_slugs(&self) -> Vec<String> {
        self.hour.iter().chain(&self.windowed).cloned().collect()
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    run(&args)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.date.is_some() != args.cycle.is_some() {
        return Err("pass both --date and --cycle, or omit both to probe latest".into());
    }
    if args.domains.is_empty() {
        return Err("pass at least one --domain".into());
    }

    let hours = parse_hours(&args.hours)?;
    let max_hour = hours.iter().copied().max().unwrap_or(0);
    let run = resolve_run(args, max_hour)?;
    let product_groups = product_groups(args)?;
    let product_slugs = product_groups.expanded_slugs();
    let batch_exe = sibling_rw_batch_path();
    let render_exe = sibling_rw_render_path();
    let ingest_out_dir = args.out_dir.join("ingest");
    let ingest_domain = args.domains[0];
    let mut domain_summaries = Vec::new();

    println!(
        "rw_cafire: ingest {} {:02}z hours {} -> {}",
        run.date_yyyymmdd,
        run.cycle_utc,
        args.hours,
        ingest_out_dir.display()
    );
    let batch_args = build_batch_args(args, &run, ingest_domain, &ingest_out_dir);
    if args.dry_run {
        println!(
            "dry-run: {}",
            command_preview(&batch_exe, &batch_args, ingest_domain.render_env())
        );
    } else {
        let status = if batch_exe.exists() {
            let mut command = Command::new(&batch_exe);
            command.args(&batch_args);
            apply_render_env(&mut command, ingest_domain);
            command.status()
        } else {
            let mut cargo_args = vec![
                "run".to_string(),
                "-p".to_string(),
                "rusty-weather".to_string(),
                "--bin".to_string(),
                "rw_batch".to_string(),
                "--".to_string(),
            ];
            cargo_args.extend(batch_args.clone());
            let mut command = Command::new("cargo");
            command.args(cargo_args);
            apply_render_env(&mut command, ingest_domain);
            command.status()
        }
        .map_err(|err| format!("launch rw_batch ingest: {err}"))?;
        if !status.success() {
            return Err(format!("rw_batch ingest failed with status {status}").into());
        }
    }

    let batch_manifest_path = ingest_out_dir.join("batch_manifest.json");
    let batch_manifest = if args.dry_run {
        serde_json::Value::Null
    } else {
        batch_manifest_path.display().to_string().into()
    };

    for domain in &args.domains {
        let domain_out_dir = args.out_dir.join(domain.slug());
        println!(
            "rw_cafire: render {} {} {:02}z {} -> {}",
            domain.label(),
            run.date_yyyymmdd,
            run.cycle_utc,
            args.products,
            domain_out_dir.display()
        );

        if args.dry_run {
            for command_args in build_render_args_for_domain(
                args,
                &run,
                *domain,
                &domain_out_dir,
                &hours,
                &product_groups,
            ) {
                println!(
                    "dry-run: {}",
                    command_preview(&render_exe, &command_args, domain.render_env())
                );
            }
            domain_summaries.push(domain_manifest(
                *domain,
                &domain_out_dir,
                "store-render",
                serde_json::Value::Null,
                serde_json::Value::Null,
                Vec::new(),
            ));
        } else {
            let (render_manifest_path, rendered_products) = render_domain_from_store(
                args,
                &run,
                *domain,
                &domain_out_dir,
                &hours,
                &product_groups,
                &render_exe,
            )?;
            domain_summaries.push(domain_manifest(
                *domain,
                &domain_out_dir,
                "store-render",
                serde_json::Value::Null,
                render_manifest_path.display().to_string().into(),
                rendered_products,
            ));
        }
    }

    if args.dry_run {
        println!("dry-run: no cafire manifest written");
        return Ok(());
    }

    std::fs::create_dir_all(&args.out_dir)
        .map_err(|err| format!("create {}: {err}", args.out_dir.display()))?;
    let manifest = serde_json::json!({
        "schema": "rw-cafire-manifest-v1",
        "runner": "rw_cafire",
        "storage": {
            "canonical": "rw-store/.rws",
            "wxstore": false,
            "python_wheel_path": false,
        },
        "pipeline": {
            "ingest_once": true,
            "render_domains_from_store": true,
            "note": "rw_batch builds full-grid .rws stores once with no PNG render; every CAFire domain renders afterward as a crop from those same stores with rw_render.",
        },
        "ingest": {
            "products": INGEST_PRODUCTS,
            "out_dir": ingest_out_dir.display().to_string(),
            "batch_manifest": batch_manifest,
        },
        "rendering": {
            "engine": "rustwx-render/rustwx-products",
            "style_note": "RustWX-style static map chrome, scales, labels, basemap overlays, colorbars, and wind/windowed products are rendered from .rws-backed Rusty Weather stores.",
            "projected_frame_source": CAFIRE_PROJECTED_FRAME_SOURCE,
            "projection_variant": CAFIRE_PROJECTION_VARIANT,
            "plot_style": CAFIRE_PLOT_STYLE,
        },
        "run": {
            "model": args.model.as_str(),
            "date": run.date_yyyymmdd,
            "cycle": run.cycle_utc,
            "source": run.source.map(|source| source.to_string()),
            "auto_resolved": run.auto_resolved,
        },
        "hours": hours,
        "products": product_slugs,
        "store_root": args.store_root.display().to_string(),
        "cache_dir": args.cache_dir.as_ref().map(|path| path.display().to_string()),
        "domains": domain_summaries,
    });
    let manifest_path = args.out_dir.join("cafire_manifest.json");
    std::fs::write(&manifest_path, serde_json::to_string_pretty(&manifest)?)
        .map_err(|err| format!("write {}: {err}", manifest_path.display()))?;
    println!("cafire manifest: {}", manifest_path.display());
    Ok(())
}

fn product_groups(args: &Args) -> Result<ProductGroups, Box<dyn std::error::Error>> {
    let request = render_all::partition_products(&args.products, args.model)?;
    let hour = request
        .direct
        .into_iter()
        .chain(request.derived)
        .collect::<Vec<_>>();
    let (hour_render_spec, windowed_render_spec) =
        split_render_specs(args.products.trim(), &hour, &request.windowed);
    Ok(ProductGroups {
        hour,
        windowed: request.windowed,
        hour_render_spec,
        windowed_render_spec,
    })
}

fn split_render_specs(
    product_spec: &str,
    hour: &[String],
    windowed: &[String],
) -> (Option<String>, Option<String>) {
    let normalized = product_spec.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "cafire-core" => (
            Some("cafire-core-hour".to_string()),
            Some("cafire-core-windowed".to_string()),
        ),
        "cafire-all" | "cafire-current" | "cafire-ops" => (
            Some("cafire-hour".to_string()),
            Some("cafire-windowed".to_string()),
        ),
        "cafire-expanded" | "cafire-store-all" => (
            Some("cafire-hour".to_string()),
            Some("cafire-windowed-expanded".to_string()),
        ),
        "cafire-hour" => (Some("cafire-hour".to_string()), None),
        "cafire-windowed" => (None, Some("cafire-windowed".to_string())),
        "cafire-windowed-expanded" => (None, Some("cafire-windowed-expanded".to_string())),
        "cafire-core-hour" => (Some("cafire-core-hour".to_string()), None),
        "cafire-core-windowed" => (None, Some("cafire-core-windowed".to_string())),
        _ => (join_product_spec(hour), join_product_spec(windowed)),
    }
}

fn join_product_spec(slugs: &[String]) -> Option<String> {
    if slugs.is_empty() {
        None
    } else {
        Some(slugs.join(","))
    }
}

fn render_domain_from_store(
    args: &Args,
    run: &ResolvedRun,
    domain: CafireDomainArg,
    domain_out_dir: &std::path::Path,
    hours: &[u16],
    product_groups: &ProductGroups,
    render_exe: &std::path::Path,
) -> Result<(PathBuf, Vec<serde_json::Value>), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(domain_out_dir)
        .map_err(|err| format!("create {}: {err}", domain_out_dir.display()))?;
    let mut commands = Vec::new();
    for command_args in
        build_render_args_for_domain(args, run, domain, domain_out_dir, hours, product_groups)
    {
        let started = Instant::now();
        let status = if render_exe.exists() {
            let mut command = Command::new(render_exe);
            command.args(&command_args);
            apply_render_env(&mut command, domain);
            command.status()
        } else {
            let mut cargo_args = vec![
                "run".to_string(),
                "-p".to_string(),
                "rusty-weather".to_string(),
                "--bin".to_string(),
                "rw_render".to_string(),
                "--".to_string(),
            ];
            cargo_args.extend(command_args.clone());
            let mut command = Command::new("cargo");
            command.args(cargo_args);
            apply_render_env(&mut command, domain);
            command.status()
        }
        .map_err(|err| format!("launch rw_render for {}: {err}", domain.slug()))?;
        let wall_ms = started.elapsed().as_millis();
        if !status.success() {
            return Err(format!(
                "rw_render failed for {} with status {status}",
                domain.slug()
            )
            .into());
        }
        commands.push(serde_json::json!({
            "args": command_args,
            "wall_ms": wall_ms,
        }));
    }

    let rendered_products =
        collect_expected_png_products(args, run, domain, domain_out_dir, hours, product_groups);
    let render_manifest = serde_json::json!({
        "schema": "rw-cafire-domain-render-manifest-v1",
        "mode": "store-render",
        "run": format!("{}_{:02}z", run.date_yyyymmdd, run.cycle_utc),
        "domain": domain.internal_slug(),
        "hours": hours,
        "commands": commands,
        "rendered_products": rendered_products,
    });
    let manifest_path = domain_out_dir.join("render_manifest.json");
    std::fs::write(
        &manifest_path,
        serde_json::to_string_pretty(&render_manifest)?,
    )
    .map_err(|err| format!("write {}: {err}", manifest_path.display()))?;
    Ok((manifest_path, rendered_products))
}

fn build_render_args_for_domain(
    args: &Args,
    run: &ResolvedRun,
    domain: CafireDomainArg,
    domain_out_dir: &std::path::Path,
    hours: &[u16],
    product_groups: &ProductGroups,
) -> Vec<Vec<String>> {
    let mut commands = Vec::new();
    let run_slug = format!("{}_{:02}z", run.date_yyyymmdd, run.cycle_utc);
    if let Some(products) = &product_groups.hour_render_spec {
        for hour in hours {
            commands.push(build_render_args(
                args,
                run,
                domain,
                domain_out_dir,
                &run_slug,
                *hour,
                products,
            ));
        }
    }
    if let Some(products) = &product_groups.windowed_render_spec {
        if let Some(anchor_hour) = hours.iter().copied().max() {
            commands.push(build_render_args(
                args,
                run,
                domain,
                domain_out_dir,
                &run_slug,
                anchor_hour,
                products,
            ));
        }
    }
    commands
}

fn build_render_args(
    args: &Args,
    run: &ResolvedRun,
    domain: CafireDomainArg,
    domain_out_dir: &std::path::Path,
    run_slug: &str,
    hour: u16,
    products: &str,
) -> Vec<String> {
    let mut render_args = vec![
        "--model".to_string(),
        args.model.as_str().to_string(),
        "--run".to_string(),
        run_slug.to_string(),
        "--hour".to_string(),
        hour.to_string(),
        "--store-root".to_string(),
        args.store_root.display().to_string(),
        "--out-dir".to_string(),
        domain_out_dir.display().to_string(),
        "--products".to_string(),
        products.to_string(),
        "--region".to_string(),
        domain.region_arg().to_string(),
        "--contour-mode".to_string(),
        args.contour_mode.clone(),
        "--png-compression".to_string(),
        args.png_compression.clone(),
        "--place-label-density".to_string(),
        args.place_label_density.to_string(),
    ];
    if let Some(source) = run.source.or(args.source) {
        render_args.push("--source".to_string());
        render_args.push(source.to_string());
    }
    if let Some(threads) = args.threads {
        render_args.push("--threads".to_string());
        render_args.push(threads.to_string());
    }
    if args.full_throttle {
        render_args.push("--full-throttle".to_string());
    }
    render_args
}

fn collect_expected_png_products(
    args: &Args,
    run: &ResolvedRun,
    domain: CafireDomainArg,
    domain_out_dir: &std::path::Path,
    hours: &[u16],
    product_groups: &ProductGroups,
) -> Vec<serde_json::Value> {
    let model_slug = args.model.as_str().replace('-', "_");
    let mut rendered = Vec::new();
    for &hour in hours {
        for slug in &product_groups.hour {
            let path = domain_out_dir.join(format!(
                "rustwx_{}_{}_{:01}z_f{:03}_{}_{}.png",
                model_slug,
                run.date_yyyymmdd,
                run.cycle_utc,
                hour,
                domain.internal_slug(),
                slug
            ));
            if path.exists() {
                rendered.push(serde_json::json!({
                    "slug": slug,
                    "hour": hour,
                    "lane": "hour",
                    "path": path.display().to_string(),
                }));
            }
        }
    }
    if let Some(anchor_hour) = hours.iter().copied().max() {
        for slug in &product_groups.windowed {
            let path = domain_out_dir.join(format!(
                "rustwx_{}_{}_{:01}z_f{:03}_{}_{}.png",
                model_slug,
                run.date_yyyymmdd,
                run.cycle_utc,
                anchor_hour,
                domain.internal_slug(),
                slug
            ));
            if path.exists() {
                rendered.push(serde_json::json!({
                    "slug": slug,
                    "lane": "windowed",
                    "path": path.display().to_string(),
                }));
            }
        }
    }
    rendered
}

fn resolve_run(args: &Args, max_hour: u16) -> Result<ResolvedRun, Box<dyn std::error::Error>> {
    match (&args.date, args.cycle) {
        (Some(date), Some(cycle)) => Ok(ResolvedRun {
            date_yyyymmdd: date.clone(),
            cycle_utc: cycle,
            source: args.source,
            auto_resolved: false,
        }),
        (None, None) => {
            let today = utc_today_yyyymmdd()?;
            let latest =
                latest_available_run_at_forecast_hour(args.model, args.source, &today, max_hour)?;
            Ok(ResolvedRun {
                date_yyyymmdd: latest.cycle.date_yyyymmdd,
                cycle_utc: latest.cycle.hour_utc,
                source: Some(latest.source),
                auto_resolved: true,
            })
        }
        _ => unreachable!("date/cycle pairing checked by run"),
    }
}

fn build_batch_args(
    args: &Args,
    run: &ResolvedRun,
    domain: CafireDomainArg,
    domain_out_dir: &std::path::Path,
) -> Vec<String> {
    let mut batch_args = vec![
        "--model".to_string(),
        args.model.as_str().to_string(),
        "--date".to_string(),
        run.date_yyyymmdd.clone(),
        "--cycle".to_string(),
        run.cycle_utc.to_string(),
        "--hours".to_string(),
        args.hours.clone(),
        "--store-root".to_string(),
        args.store_root.display().to_string(),
        "--out-dir".to_string(),
        domain_out_dir.display().to_string(),
        "--products".to_string(),
        INGEST_PRODUCTS.to_string(),
        "--region".to_string(),
        domain.region_arg().to_string(),
        "--profile".to_string(),
        args.profile.clone(),
        "--contour-mode".to_string(),
        args.contour_mode.clone(),
        "--png-compression".to_string(),
        args.png_compression.clone(),
        "--place-label-density".to_string(),
        args.place_label_density.to_string(),
    ];
    if let Some(source) = run.source.or(args.source) {
        batch_args.push("--source".to_string());
        batch_args.push(source.to_string());
    }
    if let Some(cache_dir) = &args.cache_dir {
        batch_args.push("--cache-dir".to_string());
        batch_args.push(cache_dir.display().to_string());
    }
    if args.no_heavy {
        batch_args.push("--no-heavy".to_string());
    }
    if let Some(threads) = args.threads {
        batch_args.push("--threads".to_string());
        batch_args.push(threads.to_string());
    }
    if args.full_throttle {
        batch_args.push("--full-throttle".to_string());
    }
    if args.list_products {
        batch_args.push("--list-products".to_string());
    }
    batch_args
}

fn sibling_rw_batch_path() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from(format!("rw_batch{}", std::env::consts::EXE_SUFFIX)));
    path.set_file_name(format!("rw_batch{}", std::env::consts::EXE_SUFFIX));
    path
}

fn sibling_rw_render_path() -> PathBuf {
    let mut path = std::env::current_exe()
        .unwrap_or_else(|_| PathBuf::from(format!("rw_render{}", std::env::consts::EXE_SUFFIX)));
    path.set_file_name(format!("rw_render{}", std::env::consts::EXE_SUFFIX));
    path
}

fn apply_render_env(command: &mut Command, domain: CafireDomainArg) {
    for (key, value) in domain.render_env() {
        command.env(key, value);
    }
}

fn command_preview(
    exe: &std::path::Path,
    args: &[String],
    env: Vec<(&'static str, String)>,
) -> String {
    let mut parts = Vec::with_capacity(args.len() + 1);
    parts.extend(env.into_iter().map(|(key, value)| format!("{key}={value}")));
    parts.push(exe.display().to_string());
    parts.extend(args.iter().cloned());
    parts.join(" ")
}

fn domain_manifest(
    domain: CafireDomainArg,
    out_dir: &std::path::Path,
    execution_mode: &'static str,
    batch_manifest: serde_json::Value,
    render_manifest: serde_json::Value,
    rendered_products: Vec<serde_json::Value>,
) -> serde_json::Value {
    let region = domain.region();
    let (west, east, south, north) = domain.bounds();
    let (width, height) = domain.output_size();
    serde_json::json!({
        "slug": domain.slug(),
        "internal_slug": domain.internal_slug(),
        "label": domain.label(),
        "execution_mode": execution_mode,
        "region": region.slug(),
        "bounds": {
            "west": west,
            "east": east,
            "south": south,
            "north": north,
        },
        "render": {
            "width": width,
            "height": height,
            "projected_frame_source": CAFIRE_PROJECTED_FRAME_SOURCE,
            "projection_variant": CAFIRE_PROJECTION_VARIANT,
            "plot_style": CAFIRE_PLOT_STYLE,
        },
        "out_dir": out_dir.display().to_string(),
        "batch_manifest": batch_manifest,
        "render_manifest": render_manifest,
        "rendered_products": rendered_products,
    })
}

fn utc_today_yyyymmdd() -> Result<String, Box<dyn std::error::Error>> {
    let seconds = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let days = seconds / 86_400;
    let (year, month, day) = civil_from_days(days);
    Ok(format!("{year:04}{month:02}{day:02}"))
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + i64::from(m <= 2);
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pinned_args() -> Args {
        Args::try_parse_from([
            "rw-cafire",
            "--date",
            "20260608",
            "--cycle",
            "18",
            "--hours",
            "1-3",
        ])
        .expect("default args parse")
    }

    #[test]
    fn default_products_keep_hdw_and_windowed_wind_family() {
        let args = pinned_args();
        assert_eq!(args.products, "cafire-all");
        let products = product_groups(&args).expect("default product groups");
        let slugs = products.expanded_slugs();
        for slug in [
            "2m_temperature_10m_winds",
            "2m_relative_humidity_10m_winds",
            "2m_dewpoint_10m_winds",
            "10m_wind_gusts",
            "visibility",
            "smoke_pm25_native",
            "smoke_column",
            "vpd_2m",
            "hdw",
            "fire_weather_composite",
            "qpf_1h",
            "10m_wind_1h_max",
            "10m_wind_run_max",
            "2m_temp_0_24h_range",
            "2m_temp_24_48h_range",
            "2m_temp_0_48h_range",
        ] {
            assert!(slugs.iter().any(|item| item == slug), "missing {slug}");
        }
    }

    #[test]
    fn default_domains_are_california_and_wide_west() {
        let args = pinned_args();
        assert_eq!(
            args.domains,
            vec![CafireDomainArg::California, CafireDomainArg::WideWest]
        );
        assert_eq!(
            CafireDomainArg::WideWest.region(),
            RegionPreset::CafireWideWest
        );
        assert_eq!(CafireDomainArg::WideWest.region_arg(), "cafire-wide-west");
        assert_eq!(
            CafireDomainArg::California.bounds(),
            (-126.0, -113.8, 31.9, 42.5)
        );
        assert_eq!(
            CafireDomainArg::WideWest.bounds(),
            (-125.7, -103.8, 31.9, 46.5)
        );
    }

    #[test]
    fn batch_args_build_store_without_rendering() {
        let args = pinned_args();
        let run = ResolvedRun {
            date_yyyymmdd: "20260608".to_string(),
            cycle_utc: 18,
            source: None,
            auto_resolved: false,
        };
        let batch_args = build_batch_args(
            &args,
            &run,
            CafireDomainArg::California,
            std::path::Path::new("out/cafire/california"),
        );
        assert!(
            batch_args
                .windows(2)
                .any(|pair| pair == ["--products", INGEST_PRODUCTS])
        );
        assert!(
            batch_args
                .windows(2)
                .any(|pair| pair == ["--region", "cafire-california"])
        );
        assert!(
            batch_args
                .windows(2)
                .any(|pair| pair == ["--profile", "view"])
        );
        assert!(batch_args.iter().any(|arg| arg == "--no-heavy"));
    }

    #[test]
    fn product_groups_split_hour_and_windowed_products() {
        let args = pinned_args();
        let groups = product_groups(&args).expect("default product groups");
        assert_eq!(
            groups.hour.iter().map(String::as_str).collect::<Vec<_>>(),
            vec![
                "2m_temperature_10m_winds",
                "2m_relative_humidity_10m_winds",
                "2m_dewpoint_10m_winds",
                "10m_wind_gusts",
                "visibility",
                "smoke_pm25_native",
                "smoke_column",
                "vpd_2m",
                "hdw",
                "fire_weather_composite",
            ]
        );
        assert_eq!(
            groups
                .windowed
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "qpf_1h",
                "10m_wind_1h_max",
                "10m_wind_run_max",
                "2m_temp_0_24h_range",
                "2m_temp_24_48h_range",
                "2m_temp_0_48h_range",
            ]
        );
        assert_eq!(groups.hour_render_spec.as_deref(), Some("cafire-hour"));
        assert_eq!(
            groups.windowed_render_spec.as_deref(),
            Some("cafire-windowed")
        );
    }

    #[test]
    fn render_args_for_store_domain_split_hour_and_windowed_commands() {
        let args = pinned_args();
        let run = ResolvedRun {
            date_yyyymmdd: "20260608".to_string(),
            cycle_utc: 18,
            source: None,
            auto_resolved: false,
        };
        let groups = product_groups(&args).expect("default product groups");
        let commands = build_render_args_for_domain(
            &args,
            &run,
            CafireDomainArg::WideWest,
            std::path::Path::new("out/cafire/wide_west"),
            &[1, 2, 3],
            &groups,
        );

        assert_eq!(commands.len(), 4);
        for command in &commands {
            assert!(
                command
                    .windows(2)
                    .any(|pair| pair == ["--region", "cafire-wide-west"])
            );
        }
        assert_eq!(
            commands[0]
                .windows(2)
                .find(|pair| pair[0] == "--products")
                .map(|pair| pair[1].as_str()),
            Some("cafire-hour")
        );
        assert_eq!(
            commands[3]
                .windows(2)
                .find(|pair| pair[0] == "--products")
                .map(|pair| pair[1].as_str()),
            Some("cafire-windowed")
        );
        assert!(commands[3].windows(2).any(|pair| pair == ["--hour", "3"]));
    }

    #[test]
    fn cafire_domain_render_env_sets_projection_and_dimensions() {
        let california_env = CafireDomainArg::California.render_env();
        assert!(
            california_env
                .iter()
                .any(|(key, value)| { *key == "RUSTWX_STATIC_OUTPUT_WIDTH" && value == "1400" })
        );
        assert!(
            california_env
                .iter()
                .any(|(key, value)| { *key == "RUSTWX_STATIC_OUTPUT_HEIGHT" && value == "1696" })
        );
        assert!(california_env.iter().any(|(key, value)| {
            *key == "RUSTWX_PROJECTED_FRAME_SOURCE" && value == CAFIRE_PROJECTED_FRAME_SOURCE
        }));
        assert!(california_env.iter().any(|(key, value)| {
            *key == "RUSTWX_PROJECTION_VARIANT" && value == CAFIRE_PROJECTION_VARIANT
        }));
    }

    #[test]
    fn civil_date_conversion_handles_unix_epoch() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(10_957), (2000, 1, 1));
    }
}

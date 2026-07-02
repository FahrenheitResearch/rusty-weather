//! Fetch/process public fuel data and write same-grid fuel layers into `.rws`.
//!
//! The first production provider is gridMET: daily ERC, BI, dead-fuel
//! moisture, precip, and Tmax NetCDF files from the Northwest Knowledge
//! Network public archive. This binary downloads/caches the annual files,
//! extracts the requested daily slice, computes KBDI from Tmax/precip when
//! requested, regrids everything onto the forecast grid, and appends the fuel
//! layers to existing `.rws` hours.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use rustwx_core::GridShape;
use rustwx_regrid::{
    CurvilinearLatLonGrid, GridGeometry, MissingPolicy, RegridMethod, RegridOptions, RegridPlan,
    RegularLatLonGrid,
};
use rw_sat::netcdf::{ScaledVariable, open_goes_netcdf_lossy, read_scaled_f32};
use rw_store::grid::GridFile;
use serde::Serialize;

#[path = "../fuel_import.rs"]
mod fuel_import;
#[path = "../render_all.rs"]
mod render_all;

use fuel_import::{FuelAugmentOptions, FuelAugmentSummary, FuelLayer};

const DEFAULT_GRIDMET_BASE_URL: &str = "https://www.northwestknowledge.net/metdata/data";

#[derive(Debug, Parser)]
#[command(
    name = "rw-fuel-fetch",
    about = "Download/process public fuel datasets and import them into rw-store hours"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long, default_value = "hrrr")]
    model: String,
    #[arg(long, help = "Run slug, e.g. 20260629_03z")]
    run: String,
    #[arg(long, help = "Forecast hours to rewrite, e.g. 3 or 0-48")]
    hours: String,
    #[arg(long, help = "Fuel valid date as YYYY-MM-DD")]
    date: String,
    #[arg(long, default_value = "cache/fuel")]
    cache_dir: PathBuf,
    #[arg(long, default_value = DEFAULT_GRIDMET_BASE_URL)]
    gridmet_base_url: String,
    #[arg(long, value_enum, default_value_t = FuelRegridMethodArg::Bilinear)]
    method: FuelRegridMethodArg,
    #[arg(long, default_value_t = false, help = "Allow bilinear extrapolation")]
    extrapolate: bool,
    #[arg(
        long,
        default_value_t = 75.0,
        help = "Max distance for nearest-neighbor imports, in km"
    )]
    nearest_max_distance_km: f64,
    #[arg(long, value_enum, default_value_t = FuelMissingPolicyArg::Renormalize)]
    missing_policy: FuelMissingPolicyArg,
    #[arg(long, default_value_t = true)]
    overwrite: bool,
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "Only download/cache gridMET files"
    )]
    download_only: bool,
    #[arg(
        long,
        default_value_t = true,
        help = "Compute KBDI from gridMET Tmax + precip"
    )]
    kbdi: bool,
    #[arg(
        long,
        default_value_t = 180,
        help = "KBDI spin-up days ending at --date; clipped by available year data"
    )]
    kbdi_spinup_days: usize,
    #[arg(
        long,
        default_value_t = 20.0,
        help = "Mean annual rainfall in inches for KBDI formula until a climatology layer is wired"
    )]
    kbdi_annual_rain_in: f32,
    #[arg(
        long,
        help = "Manifest path; default writes fuel_fetch_manifest.json in the run dir"
    )]
    manifest_out: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum FuelRegridMethodArg {
    Bilinear,
    Nearest,
    InverseDistance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "kebab-case")]
enum FuelMissingPolicyArg {
    Propagate,
    Renormalize,
}

#[derive(Debug, Clone, Copy)]
struct GridmetLayerDef {
    slug: &'static str,
    file_stem: &'static str,
    variable: &'static str,
    output_units: &'static str,
    value_scale: f32,
}

const GRIDMET_DIRECT_LAYERS: &[GridmetLayerDef] = &[
    GridmetLayerDef {
        slug: "erc",
        file_stem: "erc",
        variable: "energy_release_component-g",
        output_units: "index",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "burning_index",
        file_stem: "bi",
        variable: "burning_index_g",
        output_units: "index",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "dead_fuel_moisture_1h",
        file_stem: "fm1",
        variable: "dead_fuel_moisture_1hr",
        output_units: "%",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "dead_fuel_moisture_10h",
        file_stem: "fm10",
        variable: "dead_fuel_moisture_10hr",
        output_units: "%",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "dead_fuel_moisture_100h",
        file_stem: "fm100",
        variable: "dead_fuel_moisture_100hr",
        output_units: "%",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "dead_fuel_moisture_1000h",
        file_stem: "fm1000",
        variable: "dead_fuel_moisture_1000hr",
        output_units: "%",
        value_scale: 1.0,
    },
    GridmetLayerDef {
        slug: "daily_precip_fuel_context",
        file_stem: "pr",
        variable: "precipitation_amount",
        output_units: "in",
        value_scale: 0.039_370_08,
    },
];

const KBDI_TMMX: GridmetLayerDef = GridmetLayerDef {
    slug: "kbdi_tmmx",
    file_stem: "tmmx",
    variable: "air_temperature",
    output_units: "K",
    value_scale: 1.0,
};

const KBDI_PR: GridmetLayerDef = GridmetLayerDef {
    slug: "kbdi_pr",
    file_stem: "pr",
    variable: "precipitation_amount",
    output_units: "mm",
    value_scale: 1.0,
};

#[derive(Debug, Clone, Copy)]
struct GridmetDate {
    year: i32,
    month: u8,
    day: u8,
    day_index: usize,
}

#[derive(Debug, Clone)]
enum SourceGeometry {
    Regular(RegularLatLonGrid),
    Curvilinear(CurvilinearLatLonGrid),
}

impl SourceGeometry {
    fn as_geometry(&self) -> &dyn GridGeometry {
        match self {
            Self::Regular(grid) => grid,
            Self::Curvilinear(grid) => grid,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Regular(_) => "regular-lat-lon",
            Self::Curvilinear(_) => "curvilinear-lat-lon",
        }
    }
}

#[derive(Debug, Clone)]
struct SourceLayer {
    slug: String,
    units: String,
    shape: GridShape,
    values: Vec<f32>,
    geometry: SourceGeometry,
}

#[derive(Debug, Serialize)]
struct DownloadManifest {
    file_stem: String,
    url: String,
    path: String,
    cache_hit: bool,
    bytes: u64,
    download_ms: u128,
}

#[derive(Debug, Serialize)]
struct PreparedLayerManifest {
    slug: String,
    source_file: String,
    source_variable: String,
    source_units: String,
    output_units: String,
    source_shape: [usize; 2],
    source_geometry: String,
    day_index: usize,
    read_ms: u128,
    regrid_ms: u128,
    finite_count: usize,
    min: Option<f32>,
    max: Option<f32>,
}

#[derive(Debug)]
struct PreparedLayer {
    layer: FuelLayer,
    manifest: PreparedLayerManifest,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let total_started = Instant::now();
    let date = parse_gridmet_date(&args.date)?;
    let hours = rw_ingest::parse_hours(&args.hours)?;
    if hours.is_empty() {
        return Err("pass at least one hour via --hours".into());
    }
    if args.kbdi && args.kbdi_annual_rain_in <= 0.0 {
        return Err("--kbdi-annual-rain-in must be positive".into());
    }

    let mut required = GRIDMET_DIRECT_LAYERS
        .iter()
        .copied()
        .collect::<Vec<GridmetLayerDef>>();
    if args.kbdi {
        if !required
            .iter()
            .any(|def| def.file_stem == KBDI_TMMX.file_stem)
        {
            required.push(KBDI_TMMX);
        }
        if !required
            .iter()
            .any(|def| def.file_stem == KBDI_PR.file_stem)
        {
            required.push(KBDI_PR);
        }
    }
    let mut files = fetch_gridmet_files(&args, date.year, &required)?;
    ensure_gridmet_day_coverage(&mut files, &required, date.day_index)?;
    if args.download_only {
        let manifest_path =
            default_manifest_path(&args, &model_slug(&args), "fuel_fetch_manifest.json");
        write_manifest(
            &manifest_path,
            &args,
            &date,
            &files,
            &[],
            &[],
            0,
            0,
            total_started.elapsed().as_millis(),
        )?;
        println!("download-only manifest: {}", manifest_path.display());
        return Ok(());
    }

    let model_slug = model_slug(&args);
    let run_dir = args.store_root.join(&model_slug).join(&args.run);
    let grid = GridFile::open(&run_dir.join("grid.rwg"))?;
    let target = target_geometry_from_grid(&grid)?;
    let regrid_options = regrid_options(&args)?;

    println!(
        "rw_fuel_fetch build {} | gridMET {}-{:02}-{:02} day_index {} | run {} {} | hours {:?} | target {}x{}",
        env!("RW_BUILD_SHA"),
        date.year,
        date.month,
        date.day,
        date.day_index,
        model_slug,
        args.run,
        hours,
        grid.nx,
        grid.ny,
    );

    let mut prepared = Vec::new();
    for def in GRIDMET_DIRECT_LAYERS {
        let path = file_path_for(&files, def.file_stem)?;
        let started = Instant::now();
        let mut source = read_gridmet_day_layer(path, *def, date.day_index)?;
        for value in &mut source.values {
            if value.is_finite() {
                *value *= def.value_scale;
            }
        }
        source.units = def.output_units.to_string();
        let read_ms = started.elapsed().as_millis();
        prepared.push(regrid_source_layer(
            source,
            path,
            def.variable,
            date.day_index,
            &target,
            regrid_options.clone(),
            read_ms,
        )?);
    }

    if args.kbdi {
        let tmmx_path = file_path_for(&files, KBDI_TMMX.file_stem)?;
        let pr_path = file_path_for(&files, KBDI_PR.file_stem)?;
        let started = Instant::now();
        let source = compute_kbdi_layer(
            tmmx_path,
            pr_path,
            date.day_index,
            args.kbdi_spinup_days,
            args.kbdi_annual_rain_in,
        )?;
        let read_ms = started.elapsed().as_millis();
        prepared.push(regrid_source_layer(
            source,
            tmmx_path,
            "kbdi(tmmx,pr)",
            date.day_index,
            &target,
            regrid_options,
            read_ms,
        )?);
    }

    let layers = prepared
        .iter()
        .map(|prepared| prepared.layer.clone())
        .collect::<Vec<_>>();
    let mut hour_summaries = Vec::new();
    if args.dry_run {
        println!("dry-run: prepared layers but did not rewrite .rws hour files");
    } else {
        let written_unix = current_unix()?;
        for &hour in &hours {
            let started = Instant::now();
            let summary = fuel_import::augment_hour_with_fuel_layers(
                &FuelAugmentOptions {
                    store_root: args.store_root.clone(),
                    model_slug: model_slug.clone(),
                    run_slug: args.run.clone(),
                    hour,
                    overwrite: args.overwrite,
                    written_unix,
                    writer_build: env!("RW_BUILD_SHA").to_string(),
                },
                &layers,
            )?;
            println!(
                "f{hour:03} fuel fetch/import: {} -> {} vars | added {} | replaced {} | encode {} ms | wall {} ms | {}",
                summary.variables_before,
                summary.variables_after,
                summary.added.len(),
                summary.replaced.len(),
                summary.encode_ms,
                started.elapsed().as_millis(),
                summary.hour_path.display(),
            );
            hour_summaries.push(summary);
        }
    }

    let manifest_path = default_manifest_path(&args, &model_slug, "fuel_fetch_manifest.json");
    write_manifest(
        &manifest_path,
        &args,
        &date,
        &files,
        &prepared,
        &hour_summaries,
        grid.nx,
        grid.ny,
        total_started.elapsed().as_millis(),
    )?;
    println!("fuel fetch manifest: {}", manifest_path.display());
    Ok(())
}

fn fetch_gridmet_files(
    args: &Args,
    year: i32,
    defs: &[GridmetLayerDef],
) -> Result<Vec<DownloadManifest>, Box<dyn std::error::Error>> {
    let agent = rw_sat::s3::build_agent();
    let mut unique = BTreeMap::new();
    for def in defs {
        unique.insert(def.file_stem, *def);
    }
    let mut manifests = Vec::new();
    for (file_stem, _) in unique {
        let filename = format!("{file_stem}_{year}.nc");
        let url = format!(
            "{}/{}",
            args.gridmet_base_url.trim_end_matches('/'),
            filename
        );
        let path = args
            .cache_dir
            .join("gridmet")
            .join(year.to_string())
            .join(&filename);
        let started = Instant::now();
        let (cache_hit, bytes) = download_if_needed(&agent, &url, &path)?;
        println!(
            "gridMET {:<8} {} | {} bytes | {}",
            file_stem,
            if cache_hit { "cache" } else { "download" },
            bytes,
            path.display()
        );
        manifests.push(DownloadManifest {
            file_stem: file_stem.to_string(),
            url,
            path: path.display().to_string(),
            cache_hit,
            bytes,
            download_ms: started.elapsed().as_millis(),
        });
    }
    Ok(manifests)
}

/// A cached gridMET annual file only covers the days published when it was
/// downloaded. When the requested day index is beyond the cached time
/// dimension, refresh the file once and re-check; fail with a clear message
/// only if the freshly downloaded file still lacks the day (gridMET
/// publishes with roughly a 1-2 day lag).
fn ensure_gridmet_day_coverage(
    files: &mut [DownloadManifest],
    defs: &[GridmetLayerDef],
    day_index: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let agent = rw_sat::s3::build_agent();
    for manifest in files.iter_mut() {
        let Some(def) = defs.iter().find(|def| def.file_stem == manifest.file_stem) else {
            continue;
        };
        let path = PathBuf::from(&manifest.path);
        let days = gridmet_time_len(&path, def.variable)?;
        if days > day_index {
            continue;
        }
        if !manifest.cache_hit {
            return Err(format!(
                "gridMET {} covers {days} day(s); day index {day_index} is not published yet",
                manifest.file_stem
            )
            .into());
        }
        println!(
            "gridMET {:<8} cached file covers {days} day(s), need index {day_index}; refreshing",
            manifest.file_stem
        );
        fs::remove_file(&path)?;
        let started = Instant::now();
        let (_, bytes) = download_if_needed(&agent, &manifest.url, &path)?;
        manifest.cache_hit = false;
        manifest.bytes = bytes;
        manifest.download_ms = started.elapsed().as_millis();
        let refreshed = gridmet_time_len(&path, def.variable)?;
        if refreshed <= day_index {
            return Err(format!(
                "gridMET {} still covers only {refreshed} day(s) after refresh; day index \
                 {day_index} is not published yet (gridMET lags ~1-2 days)",
                manifest.file_stem
            )
            .into());
        }
    }
    Ok(())
}

fn gridmet_time_len(path: &Path, variable: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let file = open_goes_netcdf_lossy(path)?;
    let Some(var) = file.variable(variable) else {
        return Err(format!("variable not found: {variable} in {}", path.display()).into());
    };
    let shape = var.shape().to_vec();
    match shape.as_slice() {
        [nt, _, _] => Ok(*nt),
        other => Err(format!("{variable} is not 3D [time,lat,lon]: {other:?}").into()),
    }
}

fn download_if_needed(
    agent: &ureq::Agent,
    url: &str,
    path: &Path,
) -> Result<(bool, u64), Box<dyn std::error::Error>> {
    if path.exists() {
        let bytes = path.metadata()?.len();
        if bytes > 0 {
            return Ok((true, bytes));
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut response = agent.get(url).call()?;
    let expected = response
        .headers()
        .get("Content-Length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let partial = path.with_extension("partial");
    let mut file = fs::File::create(&partial)?;
    let bytes = io::copy(&mut response.body_mut().as_reader(), &mut file)?;
    file.sync_all()?;
    drop(file);
    if let Some(expected) = expected {
        if expected != bytes {
            let _ = fs::remove_file(&partial);
            return Err(format!(
                "downloaded byte count mismatch for {url}: expected {expected}, got {bytes}"
            )
            .into());
        }
    }
    fs::rename(&partial, path)?;
    Ok((false, bytes))
}

fn read_gridmet_day_layer(
    path: &Path,
    def: GridmetLayerDef,
    day_index: usize,
) -> Result<SourceLayer, Box<dyn std::error::Error>> {
    let file = open_goes_netcdf_lossy(path)?;
    let data = read_scaled_f32_time_index(&file, def.variable, day_index).or_else(|_| {
        read_scaled_f32(&file, def.variable).and_then(|full| extract_time_index(&full, day_index))
    })?;
    let shape = GridShape::new(data.shape[1], data.shape[0])?;
    let lat = read_coordinate(&file, &["lat", "latitude", "y"], "latitude")?;
    let lon = read_coordinate(&file, &["lon", "longitude", "x"], "longitude")?;
    let geometry = source_geometry_from_coordinates(shape, &lat, &lon)?;
    Ok(SourceLayer {
        slug: def.slug.to_string(),
        units: data.units.unwrap_or_else(|| def.output_units.to_string()),
        shape,
        values: data.values,
        geometry,
    })
}

fn compute_kbdi_layer(
    tmmx_path: &Path,
    pr_path: &Path,
    day_index: usize,
    spinup_days: usize,
    annual_rain_in: f32,
) -> Result<SourceLayer, Box<dyn std::error::Error>> {
    let tmmx_file = open_goes_netcdf_lossy(tmmx_path)?;
    let pr_file = open_goes_netcdf_lossy(pr_path)?;
    let first_tmmx = read_scaled_f32_time_index(&tmmx_file, KBDI_TMMX.variable, day_index)
        .or_else(|_| {
            read_scaled_f32(&tmmx_file, KBDI_TMMX.variable)
                .and_then(|full| extract_time_index(&full, day_index))
        })?;
    let shape = GridShape::new(first_tmmx.shape[1], first_tmmx.shape[0])?;
    let lat = read_coordinate(&tmmx_file, &["lat", "latitude", "y"], "latitude")?;
    let lon = read_coordinate(&tmmx_file, &["lon", "longitude", "x"], "longitude")?;
    let geometry = source_geometry_from_coordinates(shape, &lat, &lon)?;
    let start_day = day_index
        .saturating_add(1)
        .saturating_sub(spinup_days.max(1));
    let mut kbdi = vec![0.0f32; shape.len()];
    for idx in start_day..=day_index {
        let tmmx = if idx == day_index {
            first_tmmx.clone()
        } else {
            read_scaled_f32_time_index(&tmmx_file, KBDI_TMMX.variable, idx).or_else(|_| {
                read_scaled_f32(&tmmx_file, KBDI_TMMX.variable)
                    .and_then(|full| extract_time_index(&full, idx))
            })?
        };
        let pr = read_scaled_f32_time_index(&pr_file, KBDI_PR.variable, idx).or_else(|_| {
            read_scaled_f32(&pr_file, KBDI_PR.variable)
                .and_then(|full| extract_time_index(&full, idx))
        })?;
        for i in 0..shape.len() {
            let t_k = tmmx.values[i];
            let pr_mm = pr.values[i];
            if !t_k.is_finite() || !pr_mm.is_finite() {
                kbdi[i] = f32::NAN;
                continue;
            }
            let rain_in = pr_mm * 0.039_370_08;
            if rain_in > 0.20 {
                kbdi[i] = (kbdi[i] - (rain_in - 0.20) * 100.0).max(0.0);
            }
            let t_f = (t_k - 273.15) * 9.0 / 5.0 + 32.0;
            let drought_factor = kbdi_daily_increment(kbdi[i], t_f, annual_rain_in);
            kbdi[i] = (kbdi[i] + drought_factor).clamp(0.0, 800.0);
        }
    }
    Ok(SourceLayer {
        slug: "kbdi".to_string(),
        units: "index".to_string(),
        shape,
        values: kbdi,
        geometry,
    })
}

fn kbdi_daily_increment(kbdi: f32, tmax_f: f32, annual_rain_in: f32) -> f32 {
    if !kbdi.is_finite() || !tmax_f.is_finite() || tmax_f <= 50.0 {
        return 0.0;
    }
    let numerator = (800.0 - kbdi) * (0.9676 * (0.0486 * tmax_f).exp() - 8.299).max(0.0);
    let denominator = 1.0 + 10.88 * (-0.0441 * annual_rain_in).exp();
    (numerator / denominator * 0.001).max(0.0)
}

fn read_scaled_f32_time_index(
    file: &netcrust::File,
    name: &str,
    time_index: usize,
) -> Result<ScaledVariable, Box<dyn std::error::Error>> {
    let Some(variable) = file.variable(name) else {
        return Err(format!("variable not found: {name}").into());
    };
    let shape = variable.shape().to_vec();
    let [nt, ny, nx] = shape.as_slice() else {
        return Err(format!("variable {name} is not 3D [time,lat,lon], got {shape:?}").into());
    };
    if time_index >= *nt {
        return Err(format!(
            "time index {time_index} out of range for {name}; file has {nt} day(s)"
        )
        .into());
    }
    let scale = variable
        .attribute("scale_factor")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(1.0);
    let offset = variable
        .attribute("add_offset")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(0.0);
    let fill = variable
        .attribute("_FillValue")
        .and_then(|attr| attr.as_f64());
    let valid_range = variable
        .attribute("valid_range")
        .and_then(|attr| attr.value().as_f64_vec())
        .and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    let units = variable
        .attribute("units")
        .and_then(|attr| attr.as_string())
        .map(str::to_string);
    let selection = netcrust::NcSliceInfo {
        selections: vec![
            netcrust::NcSliceInfoElem::Index(time_index as u64),
            netcrust::NcSliceInfoElem::Slice {
                start: 0,
                end: *ny as u64,
                step: 1,
            },
            netcrust::NcSliceInfoElem::Slice {
                start: 0,
                end: *nx as u64,
                step: 1,
            },
        ],
    };
    let array = variable.array_f64_slice(&selection)?;
    Ok(ScaledVariable {
        name: name.to_string(),
        shape: vec![*ny, *nx],
        units,
        values: scale_values(array.into_values(), scale, offset, fill, valid_range),
    })
}

fn extract_time_index(
    full: &ScaledVariable,
    time_index: usize,
) -> Result<ScaledVariable, Box<dyn std::error::Error>> {
    let [nt, ny, nx] = full.shape.as_slice() else {
        return Err(format!(
            "variable '{}' must be 3D [time,lat,lon], got {:?}",
            full.name, full.shape
        )
        .into());
    };
    if time_index >= *nt {
        return Err(format!(
            "time index {time_index} out of range for {}; file has {} day(s)",
            full.name, nt
        )
        .into());
    }
    let cells = ny * nx;
    let start = time_index * cells;
    Ok(ScaledVariable {
        name: full.name.clone(),
        shape: vec![*ny, *nx],
        units: full.units.clone(),
        values: full.values[start..start + cells].to_vec(),
    })
}

fn scale_values(
    values: Vec<f64>,
    scale: f64,
    offset: f64,
    fill: Option<f64>,
    valid_range: Option<(f64, f64)>,
) -> Vec<f32> {
    values
        .into_iter()
        .map(|value| {
            if !value.is_finite()
                || fill.is_some_and(|fill| (value - fill).abs() < 0.5)
                || valid_range.is_some_and(|(min, max)| value < min || value > max)
            {
                f32::NAN
            } else {
                (value * scale + offset) as f32
            }
        })
        .collect()
}

fn regrid_source_layer(
    source: SourceLayer,
    source_path: &Path,
    source_variable: &str,
    day_index: usize,
    target: &CurvilinearLatLonGrid,
    options: RegridOptions,
    read_ms: u128,
) -> Result<PreparedLayer, Box<dyn std::error::Error>> {
    let started = Instant::now();
    let plan = RegridPlan::build(source.geometry.as_geometry(), target, options)?;
    let values = plan.apply_f32(&source.values)?;
    let regrid_ms = started.elapsed().as_millis();
    let (finite_count, min, max) = stats(&values);
    println!(
        "layer {:<28} {}:{} source {}x{} {} | read {} ms | regrid {} ms | finite {}",
        source.slug,
        source_path.display(),
        source_variable,
        source.shape.nx,
        source.shape.ny,
        source.geometry.kind(),
        read_ms,
        regrid_ms,
        finite_count,
    );
    Ok(PreparedLayer {
        layer: FuelLayer {
            slug: source.slug.clone(),
            units: source.units.clone(),
            values,
        },
        manifest: PreparedLayerManifest {
            slug: source.slug,
            source_file: source_path.display().to_string(),
            source_variable: source_variable.to_string(),
            source_units: source.units.clone(),
            output_units: source.units,
            source_shape: [source.shape.nx, source.shape.ny],
            source_geometry: source.geometry.kind().to_string(),
            day_index,
            read_ms,
            regrid_ms,
            finite_count,
            min,
            max,
        },
    })
}

fn read_coordinate(
    file: &netcrust::File,
    defaults: &[&str],
    label: &str,
) -> Result<ScaledVariable, Box<dyn std::error::Error>> {
    let mut errors = Vec::new();
    for name in defaults {
        match read_scaled_f32(file, name) {
            Ok(var) => return Ok(var),
            Err(err) => errors.push(format!("{name}: {err}")),
        }
    }
    Err(format!(
        "could not find {label} coordinate variable; tried {} ({})",
        defaults.join(", "),
        errors.join("; ")
    )
    .into())
}

fn source_geometry_from_coordinates(
    shape: GridShape,
    lat: &ScaledVariable,
    lon: &ScaledVariable,
) -> Result<SourceGeometry, Box<dyn std::error::Error>> {
    if lat.values.len() == shape.ny && lon.values.len() == shape.nx {
        let lat_axis = lat
            .values
            .iter()
            .map(|&value| f64::from(value))
            .collect::<Vec<_>>();
        let lon_axis = lon
            .values
            .iter()
            .map(|&value| f64::from(value))
            .collect::<Vec<_>>();
        let dlat = regular_step(&lat_axis, "latitude")?;
        let dlon = regular_step(&lon_axis, "longitude")?;
        let global_lon_wrap = (dlon.abs() * shape.nx as f64 - 360.0).abs() <= dlon.abs().max(1e-6);
        return Ok(SourceGeometry::Regular(RegularLatLonGrid::new(
            shape,
            lat_axis[0],
            lon_axis[0],
            dlat,
            dlon,
            global_lon_wrap,
        )?));
    }
    if lat.values.len() == shape.len() && lon.values.len() == shape.len() {
        return Ok(SourceGeometry::Curvilinear(CurvilinearLatLonGrid::new(
            shape,
            lat.values.iter().map(|&value| f64::from(value)).collect(),
            lon.values.iter().map(|&value| f64::from(value)).collect(),
            None,
        )?));
    }
    Err(format!(
        "coordinate shapes do not match data grid {}x{}: lat {:?} ({} values), lon {:?} ({} values)",
        shape.nx,
        shape.ny,
        lat.shape,
        lat.values.len(),
        lon.shape,
        lon.values.len()
    )
    .into())
}

fn target_geometry_from_grid(
    grid: &GridFile,
) -> Result<CurvilinearLatLonGrid, Box<dyn std::error::Error>> {
    Ok(CurvilinearLatLonGrid::new(
        GridShape::new(grid.nx, grid.ny)?,
        grid.lat.iter().map(|&value| f64::from(value)).collect(),
        grid.lon.iter().map(|&value| f64::from(value)).collect(),
        None,
    )?)
}

fn regular_step(values: &[f64], label: &str) -> Result<f64, Box<dyn std::error::Error>> {
    if values.len() < 2 {
        return Err(format!("{label} axis needs at least two values").into());
    }
    let step = values[1] - values[0];
    if !step.is_finite() || step == 0.0 {
        return Err(format!("{label} axis has invalid first step {step}").into());
    }
    for (idx, window) in values.windows(2).enumerate() {
        let have = window[1] - window[0];
        if (have - step).abs() > 1e-4 {
            return Err(format!(
                "{label} axis is not regular at segment {}: first step {}, here {}",
                idx, step, have
            )
            .into());
        }
    }
    Ok(step)
}

fn regrid_options(args: &Args) -> Result<RegridOptions, Box<dyn std::error::Error>> {
    let method = match args.method {
        FuelRegridMethodArg::Bilinear => RegridMethod::Bilinear,
        FuelRegridMethodArg::Nearest => RegridMethod::Nearest {
            max_distance_km: Some(args.nearest_max_distance_km),
        },
        FuelRegridMethodArg::InverseDistance => RegridMethod::InverseDistance {
            k: 4,
            power: 2.0,
            radius_km: Some(args.nearest_max_distance_km),
        },
    };
    let mut options = RegridOptions::new(method);
    options.extrapolate = args.extrapolate;
    options.missing_policy = match args.missing_policy {
        FuelMissingPolicyArg::Propagate => MissingPolicy::Propagate,
        FuelMissingPolicyArg::Renormalize => MissingPolicy::RenormalizeValid,
    };
    Ok(options)
}

fn file_path_for<'a>(
    files: &'a [DownloadManifest],
    file_stem: &str,
) -> Result<&'a Path, Box<dyn std::error::Error>> {
    let manifest = files
        .iter()
        .find(|manifest| manifest.file_stem == file_stem)
        .ok_or_else(|| format!("missing downloaded gridMET file for '{file_stem}'"))?;
    Ok(Path::new(&manifest.path))
}

fn parse_gridmet_date(value: &str) -> Result<GridmetDate, Box<dyn std::error::Error>> {
    let parts = value.split('-').collect::<Vec<_>>();
    if parts.len() != 3 {
        return Err(format!("date must be YYYY-MM-DD, got '{value}'").into());
    }
    let year = parts[0].parse::<i32>()?;
    let month = parts[1].parse::<u8>()?;
    let day = parts[2].parse::<u8>()?;
    let day_index = day_of_year_zero_based(year, month, day)?;
    Ok(GridmetDate {
        year,
        month,
        day,
        day_index,
    })
}

fn day_of_year_zero_based(
    year: i32,
    month: u8,
    day: u8,
) -> Result<usize, Box<dyn std::error::Error>> {
    if !(1..=12).contains(&month) {
        return Err(format!("month out of range: {month}").into());
    }
    let month_lengths = [
        31,
        if is_leap_year(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let max_day = month_lengths[usize::from(month - 1)];
    if day == 0 || usize::from(day) > max_day {
        return Err(format!("day out of range for {year}-{month:02}: {day}").into());
    }
    Ok(month_lengths[..usize::from(month - 1)]
        .iter()
        .sum::<usize>()
        + usize::from(day - 1))
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn model_slug(args: &Args) -> String {
    args.model.replace('-', "_")
}

fn default_manifest_path(args: &Args, model_slug: &str, filename: &str) -> PathBuf {
    args.manifest_out.clone().unwrap_or_else(|| {
        args.store_root
            .join(model_slug)
            .join(&args.run)
            .join(filename)
    })
}

fn stats(values: &[f32]) -> (usize, Option<f32>, Option<f32>) {
    let mut count = 0usize;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &value in values {
        if value.is_finite() {
            count += 1;
            min = min.min(value);
            max = max.max(value);
        }
    }
    if count == 0 {
        (0, None, None)
    } else {
        (count, Some(min), Some(max))
    }
}

fn write_manifest(
    path: &Path,
    args: &Args,
    date: &GridmetDate,
    downloads: &[DownloadManifest],
    prepared: &[PreparedLayer],
    hour_summaries: &[FuelAugmentSummary],
    target_nx: usize,
    target_ny: usize,
    total_ms: u128,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let manifest = serde_json::json!({
        "schema": "rw-fuel-fetch-manifest-v1",
        "build": env!("RW_BUILD_SHA"),
        "provider": "gridmet",
        "gridmet_base_url": args.gridmet_base_url,
        "date": format!("{}-{:02}-{:02}", date.year, date.month, date.day),
        "day_index": date.day_index,
        "model": model_slug(args),
        "run": args.run,
        "hours": args.hours,
        "target_grid": {"nx": target_nx, "ny": target_ny},
        "method": format!("{:?}", args.method),
        "missing_policy": format!("{:?}", args.missing_policy),
        "extrapolate": args.extrapolate,
        "overwrite": args.overwrite,
        "dry_run": args.dry_run,
        "kbdi": {
            "enabled": args.kbdi,
            "spinup_days": args.kbdi_spinup_days,
            "annual_rain_in": args.kbdi_annual_rain_in,
            "note": "KBDI is computed from gridMET Tmax and precip with a scalar annual-rainfall parameter; replace with gridded climatology when available."
        },
        "downloads": downloads,
        "layers": prepared.iter().map(|item| &item.manifest).collect::<Vec<_>>(),
        "hours_rewritten": hour_summaries.iter().map(|summary| serde_json::json!({
            "hour_path": summary.hour_path.display().to_string(),
            "variables_before": summary.variables_before,
            "variables_after": summary.variables_after,
            "added": summary.added,
            "replaced": summary.replaced,
            "encode_ms": summary.encode_ms,
            "bytes": summary.bytes,
            "wall_ms": summary.wall_ms,
        })).collect::<Vec<_>>(),
        "total_wall_ms": total_ms,
    });
    fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

fn current_unix() -> Result<u64, Box<dyn std::error::Error>> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_parser_returns_zero_based_day_index() {
        let date = parse_gridmet_date("2026-06-29").unwrap();
        assert_eq!(date.year, 2026);
        assert_eq!(date.day_index, 179);
    }

    #[test]
    fn leap_day_index_is_valid() {
        assert_eq!(day_of_year_zero_based(2024, 2, 29).unwrap(), 59);
    }

    #[test]
    fn kbdi_daily_increment_is_nonnegative() {
        assert!(kbdi_daily_increment(200.0, 95.0, 20.0) > 0.0);
        assert_eq!(kbdi_daily_increment(200.0, 40.0, 20.0), 0.0);
    }
}

//! Import real fuel layers into existing `.rws` forecast hours.
//!
//! Expected production flow:
//! 1. `rw_batch` / `rw_cafire` builds weather `.rws` hours.
//! 2. `rw_fuel_import` reads gridMET/LANDFIRE/NFDRS-style NetCDF layers,
//!    regrids them onto the model grid, and rewrites each hour with fuel
//!    variables named by the native CAFire fuel product slugs.
//! 3. `rw_render --products cafire-with-fuels` renders the weather+fuel maps.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Parser, ValueEnum};
use serde::Serialize;

#[path = "../fuel_import.rs"]
mod fuel_import;
#[path = "../render_all.rs"]
mod render_all;

use fuel_import::{FuelAugmentOptions, FuelAugmentSummary, FuelLayer};
use render_all::fuel_products::FuelProduct;
use rustwx_core::GridShape;
use rustwx_regrid::{
    CurvilinearLatLonGrid, GridGeometry, MissingPolicy, RegridMethod, RegridOptions, RegridPlan,
    RegularLatLonGrid,
};
use rw_ingest::parse_hours;
use rw_sat::netcdf::{ScaledVariable, open_goes_netcdf_lossy, read_scaled_f32};
use rw_store::grid::GridFile;

#[derive(Debug, Parser)]
#[command(
    name = "rw-fuel-import",
    about = "Import/regrid fuel layers into rw-store .rws hours"
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
    #[arg(
        long = "layer",
        required = true,
        help = "Fuel layer spec slug=path.nc:variable; repeat for multiple layers"
    )]
    layers: Vec<String>,
    #[arg(
        long,
        help = "Latitude coordinate variable; default tries lat,latitude,y"
    )]
    lat_var: Option<String>,
    #[arg(
        long,
        help = "Longitude coordinate variable; default tries lon,longitude,x"
    )]
    lon_var: Option<String>,
    #[arg(
        long,
        default_value_t = 0,
        help = "Leading time index when the data variable is 3D [time,y,x]"
    )]
    time_index: usize,
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
    #[arg(long, default_value_t = false)]
    overwrite: bool,
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    #[arg(
        long,
        help = "Manifest path; default writes fuel_import_manifest.json in the run dir"
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

#[derive(Debug, Clone)]
struct LayerSpec {
    slug: String,
    path: PathBuf,
    variable: String,
}

#[derive(Debug, Clone)]
struct NetcdfFuelLayer {
    slug: String,
    units: String,
    shape: GridShape,
    values: Vec<f32>,
    geometry: SourceGeometry,
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

#[derive(Debug, Serialize)]
struct PreparedLayerManifest {
    slug: String,
    source_path: String,
    source_variable: String,
    source_shape: [usize; 2],
    source_geometry: String,
    source_units: String,
    output_units: String,
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
    let hours = parse_hours(&args.hours)?;
    if hours.is_empty() {
        return Err("pass at least one hour via --hours".into());
    }
    let layer_specs = args
        .layers
        .iter()
        .map(|spec| parse_layer_spec(spec))
        .collect::<Result<Vec<_>, _>>()?;
    let model_slug = args.model.replace('-', "_");
    let run_dir = args.store_root.join(&model_slug).join(&args.run);
    let grid = GridFile::open(&run_dir.join("grid.rwg"))?;
    let target = target_geometry_from_grid(&grid)?;
    let regrid_options = regrid_options(&args)?;

    println!(
        "rw_fuel_import build {} | run {} {} | hours {:?} | {} layer(s) | target {}x{} | method {:?}",
        env!("RW_BUILD_SHA"),
        model_slug,
        args.run,
        hours,
        layer_specs.len(),
        grid.nx,
        grid.ny,
        args.method,
    );

    let mut prepared = Vec::new();
    for spec in &layer_specs {
        let layer_started = Instant::now();
        let source = read_netcdf_fuel_layer(
            spec,
            args.lat_var.as_deref(),
            args.lon_var.as_deref(),
            args.time_index,
        )?;
        let read_ms = layer_started.elapsed().as_millis();
        let regrid_started = Instant::now();
        let plan = RegridPlan::build(
            source.geometry.as_geometry(),
            &target,
            regrid_options.clone(),
        )?;
        let regridded = plan.apply_f32(&source.values)?;
        let regrid_ms = regrid_started.elapsed().as_millis();
        let (finite_count, min, max) = stats(&regridded);
        println!(
            "layer {:<28} {}:{} source {}x{} {} | read {} ms | regrid {} ms | finite {}",
            source.slug,
            spec.path.display(),
            spec.variable,
            source.shape.nx,
            source.shape.ny,
            source.geometry.kind(),
            read_ms,
            regrid_ms,
            finite_count,
        );
        prepared.push(PreparedLayer {
            layer: FuelLayer {
                slug: source.slug.clone(),
                units: source.units.clone(),
                values: regridded,
            },
            manifest: PreparedLayerManifest {
                slug: source.slug,
                source_path: spec.path.display().to_string(),
                source_variable: spec.variable.clone(),
                source_shape: [source.shape.nx, source.shape.ny],
                source_geometry: source.geometry.kind().to_string(),
                source_units: source.units.clone(),
                output_units: source.units,
                read_ms,
                regrid_ms,
                finite_count,
                min,
                max,
            },
        });
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
                "f{hour:03} import: {} -> {} vars | added {} | replaced {} | encode {} ms | wall {} ms | {}",
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

    let manifest_path = args
        .manifest_out
        .clone()
        .unwrap_or_else(|| run_dir.join("fuel_import_manifest.json"));
    write_manifest(
        &manifest_path,
        &args,
        &model_slug,
        &hours,
        grid.nx,
        grid.ny,
        &prepared,
        &hour_summaries,
        total_started.elapsed().as_millis(),
    )?;
    println!("fuel import manifest: {}", manifest_path.display());
    Ok(())
}

fn parse_layer_spec(spec: &str) -> Result<LayerSpec, Box<dyn std::error::Error>> {
    let (slug_raw, source) = spec
        .split_once('=')
        .ok_or_else(|| format!("layer spec must look like slug=path.nc:variable, got '{spec}'"))?;
    let product = FuelProduct::parse(slug_raw)
        .ok_or_else(|| format!("unknown native fuel product slug '{slug_raw}'"))?;
    let (path_raw, variable) = source.rsplit_once(':').ok_or_else(|| {
        format!("layer source must end with :variable (slug=path.nc:variable), got '{spec}'")
    })?;
    if path_raw.len() == 1 && path_raw.as_bytes()[0].is_ascii_alphabetic() {
        return Err(format!(
            "layer source appears to be missing :variable after Windows path in '{spec}'"
        )
        .into());
    }
    if variable.trim().is_empty() {
        return Err(format!("layer source variable is empty in '{spec}'").into());
    }
    Ok(LayerSpec {
        slug: product.slug().to_string(),
        path: PathBuf::from(path_raw),
        variable: variable.trim().to_string(),
    })
}

fn read_netcdf_fuel_layer(
    spec: &LayerSpec,
    lat_var: Option<&str>,
    lon_var: Option<&str>,
    time_index: usize,
) -> Result<NetcdfFuelLayer, Box<dyn std::error::Error>> {
    let file = open_goes_netcdf_lossy(&spec.path)?;
    let data = read_scaled_f32(&file, &spec.variable)
        .map_err(|err| format!("read {}:{}: {err}", spec.path.display(), spec.variable))?;
    let (shape, values) = extract_2d_values(&data, time_index)?;
    let lat = read_coordinate(&file, lat_var, &["lat", "latitude", "y"], "latitude")?;
    let lon = read_coordinate(&file, lon_var, &["lon", "longitude", "x"], "longitude")?;
    let geometry = source_geometry_from_coordinates(shape, &lat, &lon)?;
    Ok(NetcdfFuelLayer {
        slug: spec.slug.clone(),
        units: data
            .units
            .clone()
            .unwrap_or_else(|| default_units_for_slug(&spec.slug).to_string()),
        shape,
        values,
        geometry,
    })
}

fn read_coordinate(
    file: &netcrust::File,
    explicit: Option<&str>,
    defaults: &[&str],
    label: &str,
) -> Result<ScaledVariable, Box<dyn std::error::Error>> {
    if let Some(name) = explicit {
        return read_scaled_f32(file, name)
            .map_err(|err| format!("read {label} var '{name}': {err}").into());
    }
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

fn extract_2d_values(
    var: &ScaledVariable,
    time_index: usize,
) -> Result<(GridShape, Vec<f32>), Box<dyn std::error::Error>> {
    match var.shape.as_slice() {
        [ny, nx] => Ok((GridShape::new(*nx, *ny)?, var.values.clone())),
        [nt, ny, nx] => {
            if time_index >= *nt {
                return Err(format!(
                    "variable '{}' time_index {} out of range for {} time steps",
                    var.name, time_index, nt
                )
                .into());
            }
            let cells = nx * ny;
            let start = time_index * cells;
            Ok((
                GridShape::new(*nx, *ny)?,
                var.values[start..start + cells].to_vec(),
            ))
        }
        other => Err(format!(
            "variable '{}' must be 2D [y,x] or 3D [time,y,x], got shape {:?}",
            var.name, other
        )
        .into()),
    }
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

fn default_units_for_slug(slug: &str) -> &'static str {
    match slug {
        "dead_fuel_moisture_1h"
        | "dead_fuel_moisture_10h"
        | "dead_fuel_moisture_100h"
        | "dead_fuel_moisture_1000h" => "%",
        "daily_precip_fuel_context" => "in",
        "landfire_fuel_loading" => "tons/ac",
        _ => "index",
    }
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
    model_slug: &str,
    hours: &[u16],
    target_nx: usize,
    target_ny: usize,
    prepared: &[PreparedLayer],
    hour_summaries: &[FuelAugmentSummary],
    total_ms: u128,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let manifest = serde_json::json!({
        "schema": "rw-fuel-import-manifest-v1",
        "build": env!("RW_BUILD_SHA"),
        "model": model_slug,
        "run": args.run,
        "hours": hours,
        "target_grid": {"nx": target_nx, "ny": target_ny},
        "method": format!("{:?}", args.method),
        "missing_policy": format!("{:?}", args.missing_policy),
        "extrapolate": args.extrapolate,
        "overwrite": args.overwrite,
        "dry_run": args.dry_run,
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
    std::fs::write(path, serde_json::to_vec_pretty(&manifest)?)?;
    Ok(())
}

fn current_unix() -> Result<u64, Box<dyn std::error::Error>> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layer_spec_parses_windows_paths_with_variable_suffix() {
        let spec = parse_layer_spec(r"kbdi=C:\data\gridmet.nc:pet").unwrap();
        assert_eq!(spec.slug, "kbdi");
        assert_eq!(spec.path, PathBuf::from(r"C:\data\gridmet.nc"));
        assert_eq!(spec.variable, "pet");
    }

    #[test]
    fn layer_spec_rejects_missing_variable_suffix() {
        assert!(parse_layer_spec(r"kbdi=C:\data\gridmet.nc").is_err());
    }

    #[test]
    fn extract_2d_values_takes_requested_time_slice() {
        let var = ScaledVariable {
            name: "erc".to_string(),
            shape: vec![2, 2, 3],
            units: Some("index".to_string()),
            values: (0..12).map(|value| value as f32).collect(),
        };
        let (shape, values) = extract_2d_values(&var, 1).unwrap();
        assert_eq!((shape.nx, shape.ny), (3, 2));
        assert_eq!(values, vec![6.0, 7.0, 8.0, 9.0, 10.0, 11.0]);
    }

    #[test]
    fn regular_coordinate_geometry_accepts_descending_latitude() {
        let lat = ScaledVariable {
            name: "lat".to_string(),
            shape: vec![2],
            units: None,
            values: vec![40.0, 39.0],
        };
        let lon = ScaledVariable {
            name: "lon".to_string(),
            shape: vec![3],
            units: None,
            values: vec![-123.0, -122.0, -121.0],
        };
        let shape = GridShape::new(3, 2).unwrap();
        let geometry = source_geometry_from_coordinates(shape, &lat, &lon).unwrap();
        match geometry {
            SourceGeometry::Regular(grid) => {
                assert_eq!(grid.lat0_deg, 40.0);
                assert_eq!(grid.dlat_deg, -1.0);
                assert_eq!(grid.dlon_deg, 1.0);
            }
            SourceGeometry::Curvilinear(_) => panic!("expected regular grid"),
        }
    }
}

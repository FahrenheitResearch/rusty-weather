//! Export one cached `.rws` model hour as a GUI-neutral point sounding.
//!
//! The JSON schema is intentionally small and stable so SHARPpy Reimagined
//! can use Rusty Weather for acquisition/storage without coupling its Qt UI
//! to Rust internals.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use rw_store::grid::{GridFile, GridLocator};
use rw_store::reader::HourReader;
use serde::Serialize;

#[derive(Debug, Parser)]
#[command(
    name = "rw-sharpmod",
    about = "Export a Rusty Weather store hour as a SHARPpy point-sounding JSON file"
)]
struct Args {
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long, help = "Run directory, for example 20260713_22z")]
    run: String,
    #[arg(long)]
    forecast_hour: u16,
    #[arg(long, allow_hyphen_values = true)]
    lat: f64,
    #[arg(long, allow_hyphen_values = true)]
    lon: f64,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Debug, Serialize)]
struct Coordinate {
    lat: f64,
    lon: f64,
}

#[derive(Debug, Serialize)]
struct PointSounding {
    schema: &'static str,
    model: String,
    run: String,
    forecast_hour: u16,
    valid_unix: Option<i64>,
    requested: Coordinate,
    selected: Coordinate,
    pressure_hpa: Vec<f64>,
    height_m_msl: Vec<f64>,
    temperature_c: Vec<f64>,
    dewpoint_c: Vec<f64>,
    u_ms: Vec<f64>,
    v_ms: Vec<f64>,
}

fn profile_map(
    reader: &HourReader,
    name: &str,
    fx: f64,
    fy: f64,
) -> Result<BTreeMap<u16, f64>, Box<dyn std::error::Error>> {
    let meta = reader
        .variable(name)
        .ok_or_else(|| format!("required pressure variable '{name}' is absent"))?;
    let levels = meta.levels_hpa.clone();
    let units = meta.units.clone();
    let values = reader.read_profile_3d(name, fx, fy)?;
    if values.len() != levels.len() {
        return Err(format!(
            "variable '{name}' returned {} values for {} pressure levels",
            values.len(),
            levels.len()
        )
        .into());
    }
    Ok(levels
        .into_iter()
        .zip(values)
        .map(|(level, value)| (level, convert_value(name, &units, f64::from(value))))
        .collect())
}

fn convert_value(name: &str, units: &str, value: f64) -> f64 {
    let normalized = units.to_ascii_lowercase().replace([' ', '_'], "");
    if name.contains("temperature") || name.contains("dewpoint") {
        if normalized == "k" || normalized.contains("kelvin") {
            return value - 273.15;
        }
    }
    if (name.starts_with("u_") || name.starts_with("v_"))
        && (normalized.contains("knot") || normalized == "kt")
    {
        return value / 1.943_844_492_440_6;
    }
    value
}

fn surface_value(
    reader: &HourReader,
    name: &str,
    x: usize,
    y: usize,
) -> Result<Option<f64>, Box<dyn std::error::Error>> {
    let Some(meta) = reader.variable(name) else {
        return Ok(None);
    };
    let units = meta.units.clone();
    let window = reader.read_window_2d(name, x, y, x + 1, y + 1)?;
    Ok(window
        .values
        .first()
        .map(|value| convert_value(name, &units, f64::from(*value)))
        .filter(|value| value.is_finite()))
}

fn store_paths(root: &Path, model: &str, run: &str, fxx: u16) -> (PathBuf, PathBuf) {
    let run_dir = root.join(model.replace('-', "_")).join(run);
    (
        run_dir.join("grid.rwg"),
        run_dir.join(format!("f{fxx:03}.rws")),
    )
}

fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let (grid_path, hour_path) = store_paths(
        &args.store_root,
        &args.model.to_ascii_lowercase(),
        &args.run,
        args.forecast_hour,
    );
    let grid = GridFile::open(&grid_path)?;
    let locator = GridLocator::build(&grid);
    let (fx, fy) = locator.locate(args.lat, args.lon).ok_or_else(|| {
        format!(
            "requested point ({:.4}, {:.4}) is outside the stored model grid",
            args.lat, args.lon
        )
    })?;
    let x = (fx.round() as usize).min(grid.nx - 1);
    let y = (fy.round() as usize).min(grid.ny - 1);
    let grid_index = y * grid.nx + x;

    let reader = HourReader::open(&hour_path)?;
    let temperature = profile_map(&reader, "temperature_iso", fx, fy)?;
    let dewpoint = profile_map(&reader, "dewpoint_iso", fx, fy)?;
    let height = profile_map(&reader, "height_iso", fx, fy)?;
    let u = profile_map(&reader, "u_iso", fx, fy)?;
    let v = profile_map(&reader, "v_iso", fx, fy)?;

    let mut rows: Vec<(f64, f64, f64, f64, f64, f64)> = temperature
        .iter()
        .filter_map(|(&pressure, &temp)| {
            let values = (
                f64::from(pressure),
                *height.get(&pressure)?,
                temp,
                *dewpoint.get(&pressure)?,
                *u.get(&pressure)?,
                *v.get(&pressure)?,
            );
            [values.0, values.1, values.2, values.3, values.4, values.5]
                .iter()
                .all(|value| value.is_finite())
                .then_some(values)
        })
        .collect();

    // Add the native surface fields when they are complete and below the
    // lowest isobaric level. This preserves the near-ground detail expected
    // by the existing skew-T and parcel calculations.
    let surface = (
        surface_value(&reader, "surface_pressure", x, y)?.map(|value| {
            if value > 2_000.0 {
                value / 100.0
            } else {
                value
            }
        }),
        surface_value(&reader, "orography", x, y)?,
        surface_value(&reader, "temperature_2m", x, y)?,
        surface_value(&reader, "dewpoint_2m", x, y)?,
        surface_value(&reader, "u_10m", x, y)?,
        surface_value(&reader, "v_10m", x, y)?,
    );
    if let (Some(p), Some(z), Some(t), Some(td), Some(su), Some(sv)) = surface {
        // Pressure surfaces at or below the terrain may still contain
        // extrapolated finite values. They are not atmospheric levels for a
        // point sounding, so replace all of them with the native surface.
        rows.retain(|row| row.0 < p - 0.1);
        rows.push((p, z, t, td.min(t), su, sv));
    }
    rows.sort_by(|left, right| right.0.total_cmp(&left.0));
    let mut last_height = f64::NEG_INFINITY;
    rows.retain(|row| {
        if row.1 > last_height {
            last_height = row.1;
            true
        } else {
            false
        }
    });
    if rows.len() < 8 {
        return Err(format!(
            "only {} complete sounding levels were available",
            rows.len()
        )
        .into());
    }

    let meta = reader.meta();
    let mut sounding = PointSounding {
        schema: "sharpmod.point-sounding.v1",
        model: meta.model.clone(),
        run: meta.run.clone(),
        forecast_hour: meta.forecast_hour,
        valid_unix: meta.valid_unix,
        requested: Coordinate {
            lat: args.lat,
            lon: args.lon,
        },
        selected: Coordinate {
            lat: f64::from(grid.lat[grid_index]),
            lon: f64::from(grid.lon[grid_index]),
        },
        pressure_hpa: Vec::with_capacity(rows.len()),
        height_m_msl: Vec::with_capacity(rows.len()),
        temperature_c: Vec::with_capacity(rows.len()),
        dewpoint_c: Vec::with_capacity(rows.len()),
        u_ms: Vec::with_capacity(rows.len()),
        v_ms: Vec::with_capacity(rows.len()),
    };
    for (p, z, t, td, su, sv) in rows {
        sounding.pressure_hpa.push(p);
        sounding.height_m_msl.push(z);
        sounding.temperature_c.push(t);
        sounding.dewpoint_c.push(td.min(t));
        sounding.u_ms.push(su);
        sounding.v_ms.push(sv);
    }

    if let Some(parent) = args.output.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&args.output, serde_json::to_vec_pretty(&sounding)?)?;
    println!(
        "exported {} levels at ({:.4}, {:.4}) to {}",
        sounding.pressure_hpa.len(),
        sounding.selected.lat,
        sounding.selected.lon,
        args.output.display()
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    run(Args::parse())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_store_units() {
        assert!((convert_value("temperature_iso", "K", 273.15) - 0.0).abs() < 1e-9);
        assert!((convert_value("u_iso", "knots", 10.0) - 5.144_44).abs() < 1e-4);
        assert_eq!(convert_value("height_iso", "m", 1234.0), 1234.0);
    }

    #[test]
    fn constructs_expected_store_paths() {
        let (grid, hour) = store_paths(Path::new("cache"), "rrfs-a", "20260713_22z", 6);
        assert_eq!(grid, Path::new("cache/rrfs_a/20260713_22z/grid.rwg"));
        assert_eq!(hour, Path::new("cache/rrfs_a/20260713_22z/f006.rws"));
    }
}

//! Public ingest seam between the extraction layer and the store:
//! [`SelectedField2D`] in, `.rws` + `grid.rwg` + `run.json` out, plus the
//! read-back helpers — [`read_field_2d`] reconstructs a `SelectedField2D`
//! for extracted GRIB fields, [`read_grid_2d`] reads any 2D variable
//! (including ingest-computed derived grids, whose selector is the
//! [`derived_selector`] marker rather than a `FieldSelector`).
//!
//! v1 requires at least one 2D field per hour write: volume planes are bare
//! slices and cannot carry the grid, so the first 2D field's grid (and
//! projection) is the one written to `grid.rwg`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use rustwx_core::{GridProjection, GridShape, LatLonGrid, SelectedField2D};

use crate::error::{RwResult, RwStoreError};
use crate::format::RwsWriterInfo;
use crate::grid::{GridFile, write_grid};
use crate::reader::HourReader;
use crate::run::{RwsHourEntry, RwsRunManifest};
use crate::writer::HourWriter;

/// One 3D pressure volume to ingest: a selector template plus one full-grid
/// row-major plane per level. Levels may arrive in any order; they are
/// sorted descending (1000 hPa first) internally, planes following their
/// levels.
pub struct PressureVolumeInput<'a> {
    pub name: &'a str,
    pub units: &'a str,
    pub selector_template: serde_json::Value,
    /// `(level_hpa, plane)` pairs; each plane holds `ny * nx` values.
    pub levels: Vec<(u16, &'a [f32])>,
}

/// One derived 2D variable to ingest: a grid computed at ingest time (not
/// extracted from a model file), so it has no GRIB `FieldSelector`. Its
/// stored selector is the [`derived_selector`] marker object instead, and it
/// is read back through [`read_grid_2d`] (NOT [`read_field_2d`], which
/// requires a real selector).
pub struct DerivedFieldInput<'a> {
    pub name: &'a str,
    pub units: &'a str,
    /// Row-major `ny * nx` values on the hour grid.
    pub values: &'a [f32],
}

/// The selector marker stored for derived variables: `{"derived": "<slug>"}`.
/// Derived grids are computed at ingest, so there is no GRIB selector to
/// round-trip; the marker keeps the `selector` meta slot self-describing.
pub fn derived_selector(slug: &str) -> serde_json::Value {
    serde_json::json!({ "derived": slug })
}

/// If `selector` is the [`derived_selector`] marker, return its slug.
pub fn derived_selector_slug(selector: &serde_json::Value) -> Option<&str> {
    selector.get("derived")?.as_str()
}

/// What [`write_hour_from_fields`] produced: where the hour file landed, how
/// long encoding took, its final size, and every variable written (2D fields
/// first, then derived 2D variables, then volumes, in input order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenHour {
    pub path: PathBuf,
    pub encode_ms: u64,
    pub bytes: u64,
    pub vars: Vec<String>,
}

/// Write one forecast hour into `<store_root>/<model>/<run>/`: the hour file
/// `f{hour:03}.rws`, the run grid `grid.rwg` (written from the first 2D
/// field's grid on first use, bit-verified against it afterwards), and the
/// updated `run.json` manifest.
///
/// `written_unix` is supplied by the caller — the library never reads the
/// wall clock, so replays and tests stay deterministic. `encode_ms` is a
/// duration measured internally.
#[allow(clippy::too_many_arguments)]
pub fn write_hour_from_fields(
    store_root: &Path,
    model: &str,
    run: &str,
    forecast_hour: u16,
    fields_2d: &[(&str, &SelectedField2D)],
    volumes: &[PressureVolumeInput<'_>],
    writer_build: &str,
    written_unix: u64,
) -> RwResult<WrittenHour> {
    write_hour_from_fields_with_derived(
        store_root,
        model,
        run,
        forecast_hour,
        fields_2d,
        &[],
        volumes,
        writer_build,
        written_unix,
    )
}

/// [`write_hour_from_fields`] plus ingest-computed derived 2D variables,
/// written after the extracted 2D fields (the grid still rides on the first
/// extracted field) and before the volumes. Derived planes are bare slices
/// validated against the hour grid; their stored selector is the
/// [`derived_selector`] marker.
#[allow(clippy::too_many_arguments)]
pub fn write_hour_from_fields_with_derived(
    store_root: &Path,
    model: &str,
    run: &str,
    forecast_hour: u16,
    fields_2d: &[(&str, &SelectedField2D)],
    derived_2d: &[DerivedFieldInput<'_>],
    volumes: &[PressureVolumeInput<'_>],
    writer_build: &str,
    written_unix: u64,
) -> RwResult<WrittenHour> {
    // v1: the grid rides on the first 2D field — volume planes are bare
    // slices with no (nx, ny) of their own. This also covers "no inputs".
    let Some((_, first)) = fields_2d.first() else {
        return Err(RwStoreError::Format(format!(
            "write_hour_from_fields requires at least one extracted 2D field to carry the \
             grid (got 0 2D fields, {} derived, {} volumes)",
            derived_2d.len(),
            volumes.len()
        )));
    };
    let reference = &first.grid;
    let (nx, ny) = (reference.shape.nx, reference.shape.ny);
    let cells = reference.shape.len();
    if reference.lat_deg.len() != cells || reference.lon_deg.len() != cells {
        return Err(RwStoreError::Format(format!(
            "2D field '{}': coordinate arrays must hold {cells} values ({ny} x {nx}), \
             got lat {} / lon {}",
            fields_2d[0].0,
            reference.lat_deg.len(),
            reference.lon_deg.len()
        )));
    }

    // Every input must agree on one (nx, ny) before anything touches disk,
    // and every 2D field after the first must sit on bit-identical
    // coordinates: two same-dims fields from different grids must error,
    // not store silently under the first field's coordinates.
    for (index, (name, field)) in fields_2d.iter().enumerate() {
        let shape = field.grid.shape;
        if (shape.nx, shape.ny) != (nx, ny) {
            return Err(RwStoreError::Format(format!(
                "2D field '{name}': grid {}x{} does not match the hour grid {nx}x{ny}",
                shape.nx, shape.ny
            )));
        }
        if index == 0 {
            continue;
        }
        let coords_match = field.grid.lat_deg.len() == cells
            && field.grid.lon_deg.len() == cells
            && field
                .grid
                .lat_deg
                .iter()
                .zip(&reference.lat_deg)
                .all(|(a, b)| a.to_bits() == b.to_bits())
            && field
                .grid
                .lon_deg
                .iter()
                .zip(&reference.lon_deg)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        if !coords_match {
            return Err(RwStoreError::Format(format!(
                "2D field '{name}': same {nx}x{ny} dims as the first field '{}' \
                 but different coordinates",
                fields_2d[0].0
            )));
        }
    }
    // Derived planes are bare slices on the hour grid; size-check them
    // before anything touches disk, like the volume planes below.
    for derived in derived_2d {
        if derived.values.len() != cells {
            return Err(RwStoreError::Format(format!(
                "derived 2D field '{}': plane holds {} values, expected {cells} ({ny} x {nx})",
                derived.name,
                derived.values.len()
            )));
        }
    }
    // Sort each volume's levels descending (planes follow their levels) and
    // reject duplicates and wrong-sized planes up front.
    let mut sorted_volumes = Vec::with_capacity(volumes.len());
    for volume in volumes {
        let mut levels = volume.levels.clone();
        levels.sort_by(|a, b| b.0.cmp(&a.0));
        if let Some(pair) = levels.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(RwStoreError::Format(format!(
                "volume '{}': duplicate level {} hPa",
                volume.name, pair[0].0
            )));
        }
        for (level, plane) in &levels {
            if plane.len() != cells {
                return Err(RwStoreError::Format(format!(
                    "volume '{}' level {level} hPa: plane holds {} values, \
                     expected {cells} ({ny} x {nx})",
                    volume.name,
                    plane.len()
                )));
            }
        }
        sorted_volumes.push((volume, levels));
    }

    let run_dir = store_root.join(model).join(run);
    fs::create_dir_all(&run_dir)?;

    // grid.rwg: write once from the first 2D field; afterwards every hour
    // must match it bit-for-bit (full coordinate compare, once per write).
    let grid_path = run_dir.join("grid.rwg");
    let grid_hash = if grid_path.exists() {
        let existing = GridFile::open(&grid_path)?;
        if (existing.nx, existing.ny) != (nx, ny) {
            return Err(RwStoreError::Meta(format!(
                "existing grid.rwg holds a {}x{} grid, but the input fields are on {nx}x{ny}",
                existing.nx, existing.ny
            )));
        }
        let coords_match = existing
            .lat
            .iter()
            .zip(&reference.lat_deg)
            .all(|(a, b)| a.to_bits() == b.to_bits())
            && existing
                .lon
                .iter()
                .zip(&reference.lon_deg)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        if !coords_match {
            return Err(RwStoreError::Meta(format!(
                "existing grid.rwg ({}) and the input grid have the same {nx}x{ny} dims \
                 but different coordinates",
                existing.hash
            )));
        }
        existing.hash
    } else {
        write_grid(&grid_path, reference, first.projection.as_ref())?
    };

    // Encode and write the hour file; encode_ms is a duration (Instant), not
    // a stored wall timestamp, so the no-clock rule does not apply to it.
    let started = Instant::now();
    let mut writer = HourWriter::new(model, run, forecast_hour, nx, ny, &grid_hash, writer_build);
    let mut vars = Vec::with_capacity(fields_2d.len() + derived_2d.len() + volumes.len());
    for (name, field) in fields_2d {
        let selector = serde_json::to_value(field.selector).map_err(|err| {
            RwStoreError::Meta(format!("2D field '{name}': selector JSON: {err}"))
        })?;
        writer.add_surface2d(name, &field.units, selector, &field.values)?;
        vars.push((*name).to_string());
    }
    for derived in derived_2d {
        writer.add_surface2d(
            derived.name,
            derived.units,
            derived_selector(derived.name),
            derived.values,
        )?;
        vars.push(derived.name.to_string());
    }
    for (volume, levels) in &sorted_volumes {
        let levels_hpa: Vec<u16> = levels.iter().map(|(level, _)| *level).collect();
        let planes: Vec<&[f32]> = levels.iter().map(|(_, plane)| *plane).collect();
        writer.add_pressure3d(
            volume.name,
            volume.units,
            volume.selector_template.clone(),
            &levels_hpa,
            &planes,
        )?;
        vars.push(volume.name.to_string());
    }
    let file_name = format!("f{forecast_hour:03}.rws");
    let hour_path = run_dir.join(&file_name);
    writer.finish(&hour_path)?;
    let encode_ms = started.elapsed().as_millis() as u64;
    let bytes = fs::metadata(&hour_path)?.len();

    // Register the hour in run.json last, so a failed write never appears
    // in the manifest.
    let manifest_path = run_dir.join("run.json");
    let writer_info = RwsWriterInfo {
        name: "rw-store".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        build: writer_build.to_string(),
    };
    let mut manifest =
        RwsRunManifest::load_or_new(&manifest_path, model, run, &grid_hash, nx, ny, writer_info)?;
    manifest.register_hour(
        forecast_hour,
        RwsHourEntry {
            file: file_name,
            written_unix,
            encode_ms,
            variables: vars.clone(),
        },
    );
    manifest.save(&manifest_path)?;

    Ok(WrittenHour {
        path: hour_path,
        encode_ms,
        bytes,
        vars,
    })
}

/// A stored 2D variable read back without interpreting its selector: the
/// selector stays raw JSON, so this works for extracted GRIB fields and for
/// ingest-computed derived variables (whose selector is the
/// [`derived_selector`] marker, not a `FieldSelector`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredField2D {
    pub units: String,
    /// Raw selector meta: a `FieldSelector` JSON object for extracted
    /// fields, or the `{"derived": "<slug>"}` marker for derived variables.
    pub selector: serde_json::Value,
    pub grid: LatLonGrid,
    pub values: Vec<f32>,
    pub projection: Option<GridProjection>,
}

/// Read any 2D variable from a store hour — derived or extracted — without
/// deserializing the selector: variable meta gives units and the raw
/// selector JSON, the hour file gives the values, and the grid file gives
/// the coordinates and projection. The grid file must be the one the hour
/// was written against (`grid_hash` match).
pub fn read_grid_2d(reader: &HourReader, grid: &GridFile, name: &str) -> RwResult<StoredField2D> {
    let var = reader
        .variable(name)
        .ok_or_else(|| RwStoreError::UnknownVariable(name.to_string()))?;
    if var.kind != "surface2d" {
        return Err(RwStoreError::Format(format!(
            "variable '{name}' has kind '{}', expected 'surface2d'",
            var.kind
        )));
    }
    if reader.meta().grid_hash != grid.hash {
        return Err(RwStoreError::Grid(format!(
            "hour file was written against grid {} but the supplied grid file is {}",
            reader.meta().grid_hash,
            grid.hash
        )));
    }
    let values = reader.read_full_2d(name)?;
    Ok(StoredField2D {
        units: var.units.clone(),
        selector: var.selector.clone(),
        grid: LatLonGrid {
            shape: GridShape {
                nx: grid.nx,
                ny: grid.ny,
            },
            lat_deg: grid.lat.clone(),
            lon_deg: grid.lon.clone(),
        },
        values,
        projection: grid.projection.clone(),
    })
}

/// Reconstruct a [`SelectedField2D`] from a store hour: [`read_grid_2d`]
/// plus selector deserialization. Only valid for extracted GRIB fields —
/// derived variables carry the [`derived_selector`] marker instead of a
/// `FieldSelector` and must be read through [`read_grid_2d`].
pub fn read_field_2d(
    reader: &HourReader,
    grid: &GridFile,
    name: &str,
) -> RwResult<SelectedField2D> {
    let stored = read_grid_2d(reader, grid, name)?;
    let selector = serde_json::from_value(stored.selector.clone()).map_err(|err| {
        let hint = derived_selector_slug(&stored.selector)
            .map(|slug| format!(" ('{slug}' is a derived variable; use read_grid_2d)"))
            .unwrap_or_default();
        RwStoreError::Meta(format!("variable '{name}': selector JSON: {err}{hint}"))
    })?;
    Ok(SelectedField2D {
        selector,
        units: stored.units,
        grid: stored.grid,
        values: stored.values,
        projection: stored.projection,
    })
}

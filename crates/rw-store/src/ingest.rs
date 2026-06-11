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
use std::time::{Duration, Instant};

use rustwx_core::{GridProjection, GridShape, LatLonGrid, SelectedField2D};

use crate::atomic::atomic_write_bytes;
use crate::error::{RwResult, RwStoreError};
use crate::format::RwsWriterInfo;
use crate::grid::{GridFile, encode_grid_bytes};
use crate::lock::RunLock;
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
    // Volume validation (level sort, duplicate levels, plane sizes) happens
    // in HourIngestWriter::add_volume below, with the same error messages.

    let mut writer = HourIngestWriter::begin(
        store_root,
        model,
        run,
        forecast_hour,
        reference,
        first.projection.as_ref(),
        writer_build,
    )?;
    for (name, field) in fields_2d {
        let selector = serde_json::to_value(field.selector).map_err(|err| {
            RwStoreError::Meta(format!("2D field '{name}': selector JSON: {err}"))
        })?;
        writer.add_field_2d(name, &field.units, selector, &field.values)?;
    }
    for derived in derived_2d {
        writer.add_derived_2d(derived.name, derived.units, derived.values)?;
    }
    for volume in volumes {
        writer.add_volume(
            volume.name,
            volume.units,
            volume.selector_template.clone(),
            &volume.levels,
        )?;
    }
    writer.finish(written_unix)
}

/// Incremental per-hour ingest writer: the staged seam behind
/// [`write_hour_from_fields_with_derived`], also driven directly by the
/// store-ingest flow so extracted planes can be encoded (and their raw f32
/// buffers freed) BEFORE the derived/heavy compute stages run.
///
/// Output bytes are identical to the historical one-shot write:
/// * 2D fields and derived grids take ids in add order;
/// * volumes added through [`Self::add_volume`] are encoded immediately but
///   numbered AFTER every 2D variable at finish (deferred ids), preserving
///   the historical `fields, derived, heavy, volumes` order no matter when
///   they are added;
/// * encoded chunk payloads spill to a temp file next to the hour file, and
///   `finish()` streams them into the atomic temp instead of assembling the
///   whole hour in memory;
/// * `grid.rwg` is validated against any existing file at `begin()` (same
///   errors as before) and, when absent, its byte image is precomputed at
///   `begin()` and written at `finish()` just before the hour file.
/// How long [`HourIngestWriter::begin`] waits for the run-dir advisory lock
/// before giving up with [`RwStoreError::Locked`]. 60s because the normal
/// contention is a competing hour encode finishing (seconds), which we want
/// to wait out rather than fail on.
const LOCK_TIMEOUT: Duration = Duration::from_secs(60);

pub struct HourIngestWriter {
    run_dir: PathBuf,
    grid_hash: String,
    /// The `.rwg` image to write at finish when no grid.rwg exists yet.
    pending_grid: Option<Vec<u8>>,
    nx: usize,
    ny: usize,
    cells: usize,
    forecast_hour: u16,
    model: String,
    run: String,
    writer_build: String,
    writer: HourWriter,
    vars_normal: Vec<String>,
    vars_deferred: Vec<String>,
    encode_elapsed: Duration,
    /// Exclusive advisory lock on this run dir, held for the whole
    /// begin→finish critical section (grid validation + hour write +
    /// manifest update). Released when this writer drops (after `finish`,
    /// or early on an aborted write). Underscore-prefixed: it is a drop
    /// guard, never read directly.
    _lock: RunLock,
}

impl HourIngestWriter {
    /// Open the run directory, settle the grid identity (validate an
    /// existing `grid.rwg` bit-for-bit or stage a new one), and start the
    /// hour writer in spill mode.
    pub fn begin(
        store_root: &Path,
        model: &str,
        run: &str,
        forecast_hour: u16,
        grid: &LatLonGrid,
        projection: Option<&GridProjection>,
        writer_build: &str,
    ) -> RwResult<Self> {
        let (nx, ny) = (grid.shape.nx, grid.shape.ny);
        let cells = grid.shape.len();
        if grid.lat_deg.len() != cells || grid.lon_deg.len() != cells {
            return Err(RwStoreError::Grid(format!(
                "hour grid coordinate arrays must hold {cells} values ({ny} x {nx}), \
                 got lat {} / lon {}",
                grid.lat_deg.len(),
                grid.lon_deg.len()
            )));
        }
        let run_dir = store_root.join(model).join(run);
        fs::create_dir_all(&run_dir)?;

        // Single-writer-per-run-dir (FORMAT.md §7). Take the exclusive
        // advisory lock before reading or writing grid.rwg / f*.rws /
        // run.json — the whole begin→finish span is the critical section.
        // 60s: the normal contention is another process finishing an hour
        // encode on this same run dir (seconds, not minutes); waiting that
        // out is correct, and on a true overrun the `Locked` error names
        // the contended path so the operator can find the other writer.
        let lock = RunLock::acquire(&run_dir, LOCK_TIMEOUT)?;

        // grid.rwg: written once from the hour grid; afterwards every hour
        // must match it bit-for-bit (full coordinate compare, once per
        // write). When absent, the byte image is staged here and written at
        // finish() so a failed hour leaves no grid file behind.
        let grid_path = run_dir.join("grid.rwg");
        let (grid_hash, pending_grid) = if grid_path.exists() {
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
                .zip(&grid.lat_deg)
                .all(|(a, b)| a.to_bits() == b.to_bits())
                && existing
                    .lon
                    .iter()
                    .zip(&grid.lon_deg)
                    .all(|(a, b)| a.to_bits() == b.to_bits());
            if !coords_match {
                return Err(RwStoreError::Meta(format!(
                    "existing grid.rwg ({}) and the input grid have the same {nx}x{ny} dims \
                     but different coordinates",
                    existing.hash
                )));
            }
            (existing.hash, None)
        } else {
            let (bytes, hash) = encode_grid_bytes(grid, projection)?;
            (hash, Some(bytes))
        };

        let writer = HourWriter::new(model, run, forecast_hour, nx, ny, &grid_hash, writer_build)
            .with_spill_dir(&run_dir);
        Ok(Self {
            run_dir,
            grid_hash,
            pending_grid,
            nx,
            ny,
            cells,
            forecast_hour,
            model: model.to_string(),
            run: run.to_string(),
            writer_build: writer_build.to_string(),
            writer,
            vars_normal: Vec::new(),
            vars_deferred: Vec::new(),
            encode_elapsed: Duration::ZERO,
            _lock: lock,
        })
    }

    /// Add one extracted 2D field (raw selector JSON), id in add order.
    pub fn add_field_2d(
        &mut self,
        name: &str,
        units: &str,
        selector: serde_json::Value,
        values: &[f32],
    ) -> RwResult<()> {
        let started = Instant::now();
        self.writer.add_surface2d(name, units, selector, values)?;
        self.encode_elapsed += started.elapsed();
        self.vars_normal.push(name.to_string());
        Ok(())
    }

    /// Add one ingest-computed derived 2D grid (selector = the
    /// [`derived_selector`] marker), id in add order.
    pub fn add_derived_2d(&mut self, name: &str, units: &str, values: &[f32]) -> RwResult<()> {
        if values.len() != self.cells {
            return Err(RwStoreError::Format(format!(
                "derived 2D field '{name}': plane holds {} values, expected {} ({} x {})",
                values.len(),
                self.cells,
                self.ny,
                self.nx
            )));
        }
        let started = Instant::now();
        self.writer
            .add_surface2d(name, units, derived_selector(name), values)?;
        self.encode_elapsed += started.elapsed();
        self.vars_normal.push(name.to_string());
        Ok(())
    }

    /// Add one 3D pressure volume: encoded (and spilled) now, numbered
    /// after every 2D variable at finish. Levels may arrive in any order
    /// and are sorted descending, planes following their levels.
    pub fn add_volume(
        &mut self,
        name: &str,
        units: &str,
        selector_template: serde_json::Value,
        levels: &[(u16, &[f32])],
    ) -> RwResult<()> {
        let mut sorted: Vec<(u16, &[f32])> = levels.to_vec();
        sorted.sort_by(|a, b| b.0.cmp(&a.0));
        if let Some(pair) = sorted.windows(2).find(|pair| pair[0].0 == pair[1].0) {
            return Err(RwStoreError::Format(format!(
                "volume '{name}': duplicate level {} hPa",
                pair[0].0
            )));
        }
        for (level, plane) in &sorted {
            if plane.len() != self.cells {
                return Err(RwStoreError::Format(format!(
                    "volume '{name}' level {level} hPa: plane holds {} values, \
                     expected {} ({} x {})",
                    plane.len(),
                    self.cells,
                    self.ny,
                    self.nx
                )));
            }
        }
        let levels_hpa: Vec<u16> = sorted.iter().map(|(level, _)| *level).collect();
        let planes: Vec<&[f32]> = sorted.iter().map(|(_, plane)| *plane).collect();
        let started = Instant::now();
        self.writer.add_pressure3d_deferred(
            name,
            units,
            selector_template,
            &levels_hpa,
            &planes,
        )?;
        self.encode_elapsed += started.elapsed();
        self.vars_deferred.push(name.to_string());
        Ok(())
    }

    /// Write `grid.rwg` (when newly staged), stream-assemble the hour file
    /// atomically, and register the hour in `run.json` last so a failed
    /// write never appears in the manifest.
    pub fn finish(mut self, written_unix: u64) -> RwResult<WrittenHour> {
        let file_name = format!("f{:03}.rws", self.forecast_hour);
        let hour_path = self.run_dir.join(&file_name);

        if let Some(bytes) = self.pending_grid.take() {
            atomic_write_bytes(&self.run_dir.join("grid.rwg"), &bytes)?;
        }

        let started = Instant::now();
        self.writer.finish(&hour_path)?;
        self.encode_elapsed += started.elapsed();
        let encode_ms = self.encode_elapsed.as_millis() as u64;
        let bytes = fs::metadata(&hour_path)?.len();

        let mut vars = self.vars_normal;
        vars.extend(self.vars_deferred);

        let manifest_path = self.run_dir.join("run.json");
        let writer_info = RwsWriterInfo {
            name: "rw-store".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: self.writer_build.clone(),
        };
        let mut manifest = RwsRunManifest::load_or_new(
            &manifest_path,
            &self.model,
            &self.run,
            &self.grid_hash,
            self.nx,
            self.ny,
            writer_info,
        )?;
        manifest.register_hour(
            self.forecast_hour,
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

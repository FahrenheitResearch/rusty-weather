//! Store-side fuel layer import/augmentation.
//!
//! The renderer treats fuel products as ordinary same-grid `.rws` variables
//! with `{"derived": "<slug>"}` selectors. This module owns the safe rewrite
//! path that preserves an existing hour and appends/replaces those fuel grids.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use rustwx_core::{GridShape, LatLonGrid};

use rw_store::grid::GridFile;
use rw_store::reader::HourReader;
use rw_store::{HourIngestWriter, RwStoreError};

#[derive(Debug, Clone)]
pub struct FuelLayer {
    pub slug: String,
    pub units: String,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone)]
pub struct FuelAugmentOptions {
    pub store_root: PathBuf,
    pub model_slug: String,
    pub run_slug: String,
    pub hour: u16,
    pub overwrite: bool,
    pub written_unix: u64,
    pub writer_build: String,
}

#[derive(Debug, Clone)]
pub struct FuelAugmentSummary {
    pub hour_path: PathBuf,
    pub variables_before: usize,
    pub variables_after: usize,
    pub added: Vec<String>,
    pub replaced: Vec<String>,
    pub encode_ms: u64,
    pub bytes: u64,
    pub wall_ms: u128,
}

pub fn augment_hour_with_fuel_layers(
    options: &FuelAugmentOptions,
    layers: &[FuelLayer],
) -> Result<FuelAugmentSummary, Box<dyn std::error::Error>> {
    if layers.is_empty() {
        return Err("pass at least one fuel layer to import".into());
    }
    let run_dir = options
        .store_root
        .join(&options.model_slug)
        .join(&options.run_slug);
    let grid_path = run_dir.join("grid.rwg");
    let hour_path = run_dir.join(format!("f{:03}.rws", options.hour));
    let grid = GridFile::open(&grid_path)?;
    let shape = GridShape::new(grid.nx, grid.ny)?;
    let cells = shape.len();
    for layer in layers {
        if layer.values.len() != cells {
            return Err(format!(
                "fuel layer '{}' has {} values, expected {} for store grid {}x{}",
                layer.slug,
                layer.values.len(),
                cells,
                grid.nx,
                grid.ny
            )
            .into());
        }
    }
    reject_duplicate_layers(layers)?;

    let started = Instant::now();
    let reader = HourReader::open(&hour_path)?;
    let variables = reader.meta().variables.clone();
    let existing_names: HashSet<String> = variables.iter().map(|var| var.name.clone()).collect();
    let import_names: HashSet<&str> = layers.iter().map(|layer| layer.slug.as_str()).collect();
    let conflicts: Vec<String> = layers
        .iter()
        .filter(|layer| existing_names.contains(&layer.slug))
        .map(|layer| layer.slug.clone())
        .collect();
    if !options.overwrite && !conflicts.is_empty() {
        return Err(format!(
            "fuel layer(s) already exist in {}: {} (pass --overwrite to replace)",
            hour_path.display(),
            conflicts.join(", ")
        )
        .into());
    }

    let target_grid = LatLonGrid::new(shape, grid.lat.clone(), grid.lon.clone())?;
    let mut writer = HourIngestWriter::begin(
        &options.store_root,
        &options.model_slug,
        &options.run_slug,
        options.hour,
        &target_grid,
        grid.projection.as_ref(),
        &options.writer_build,
    )?;

    let mut replaced = Vec::new();
    for var in &variables {
        if import_names.contains(var.name.as_str()) && options.overwrite {
            replaced.push(var.name.clone());
            continue;
        }
        match var.kind.as_str() {
            "surface2d" => {
                let values = reader.read_full_2d(&var.name)?;
                writer.add_field_2d(&var.name, &var.units, var.selector.clone(), &values)?;
            }
            "pressure3d" => {
                copy_pressure_volume(&reader, &mut writer, var, cells)?;
            }
            other => {
                return Err(format!(
                    "cannot preserve variable '{}' with unsupported kind '{}'",
                    var.name, other
                )
                .into());
            }
        }
    }

    for layer in layers {
        writer.add_derived_2d(&layer.slug, &layer.units, &layer.values)?;
    }

    drop(reader);
    let written = writer.finish(options.written_unix)?;
    Ok(FuelAugmentSummary {
        hour_path: written.path,
        variables_before: variables.len(),
        variables_after: written.vars.len(),
        added: layers.iter().map(|layer| layer.slug.clone()).collect(),
        replaced,
        encode_ms: written.encode_ms,
        bytes: written.bytes,
        wall_ms: started.elapsed().as_millis(),
    })
}

fn copy_pressure_volume(
    reader: &HourReader,
    writer: &mut HourIngestWriter,
    var: &rw_store::format::RwsVariableMeta,
    cells: usize,
) -> Result<(), RwStoreError> {
    let values = reader.read_full_3d(&var.name)?;
    let levels = var
        .levels_hpa
        .iter()
        .enumerate()
        .map(|(idx, level)| {
            let start = idx * cells;
            (*level, &values[start..start + cells])
        })
        .collect::<Vec<_>>();
    writer.add_volume(&var.name, &var.units, var.selector.clone(), &levels)
}

fn reject_duplicate_layers(layers: &[FuelLayer]) -> Result<(), Box<dyn std::error::Error>> {
    let mut seen = HashSet::new();
    for layer in layers {
        if !seen.insert(layer.slug.as_str()) {
            return Err(format!("duplicate fuel layer '{}'", layer.slug).into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_core::{GridProjection, GridShape, LatLonGrid};
    use rw_store::HourIngestWriter;
    use std::path::Path;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rw-fuel-import-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid() -> LatLonGrid {
        LatLonGrid::new(
            GridShape::new(3, 2).unwrap(),
            vec![40.0, 40.0, 40.0, 39.0, 39.0, 39.0],
            vec![-123.0, -122.0, -121.0, -123.0, -122.0, -121.0],
        )
        .unwrap()
    }

    fn write_fixture(store_root: &Path) {
        let grid = grid();
        let mut writer = HourIngestWriter::begin(
            store_root,
            "hrrr",
            "20260629_03z",
            3,
            &grid,
            Some(&GridProjection::Geographic),
            "test-build",
        )
        .unwrap();
        writer
            .add_field_2d(
                "temp_2m",
                "K",
                serde_json::json!({"discipline": 0, "parameter": "TMP"}),
                &[290.0, 291.0, 292.0, 293.0, 294.0, 295.0],
            )
            .unwrap();
        writer
            .add_derived_2d("vpd_2m", "hPa", &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
            .unwrap();
        let level_1000 = [10.0, 11.0, 12.0, 13.0, 14.0, 15.0];
        let level_900 = [20.0, 21.0, 22.0, 23.0, 24.0, 25.0];
        writer
            .add_volume(
                "temp_iso",
                "K",
                serde_json::json!({"volume": "temp_iso"}),
                &[(1000, &level_1000), (900, &level_900)],
            )
            .unwrap();
        writer.finish(1_783_000_000).unwrap();
    }

    #[test]
    fn augment_preserves_existing_variables_and_adds_fuel() {
        let dir = test_dir("preserve-add");
        write_fixture(&dir);
        let layer = FuelLayer {
            slug: "kbdi".to_string(),
            units: "index".to_string(),
            values: vec![0.0, 100.0, 200.0, 300.0, 400.0, 500.0],
        };
        let summary = augment_hour_with_fuel_layers(
            &FuelAugmentOptions {
                store_root: dir.clone(),
                model_slug: "hrrr".to_string(),
                run_slug: "20260629_03z".to_string(),
                hour: 3,
                overwrite: false,
                written_unix: 1_783_000_100,
                writer_build: "test-build".to_string(),
            },
            &[layer.clone()],
        )
        .unwrap();
        assert_eq!(summary.variables_before, 3);
        assert_eq!(summary.variables_after, 4);
        assert_eq!(summary.added, vec!["kbdi"]);
        assert!(summary.replaced.is_empty());

        let hour = HourReader::open(&summary.hour_path).unwrap();
        assert_eq!(hour.read_full_2d("kbdi").unwrap(), layer.values);
        assert_eq!(
            hour.variable("kbdi")
                .unwrap()
                .selector
                .get("derived")
                .and_then(|value| value.as_str()),
            Some("kbdi")
        );
        assert_eq!(
            hour.read_full_2d("temp_2m").unwrap(),
            vec![290.0, 291.0, 292.0, 293.0, 294.0, 295.0]
        );
        assert_eq!(hour.read_full_3d("temp_iso").unwrap().len(), 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn augment_requires_overwrite_for_existing_layer() {
        let dir = test_dir("overwrite");
        write_fixture(&dir);
        let first = FuelLayer {
            slug: "erc".to_string(),
            units: "index".to_string(),
            values: vec![1.0; 6],
        };
        let mut options = FuelAugmentOptions {
            store_root: dir.clone(),
            model_slug: "hrrr".to_string(),
            run_slug: "20260629_03z".to_string(),
            hour: 3,
            overwrite: false,
            written_unix: 1_783_000_100,
            writer_build: "test-build".to_string(),
        };
        augment_hour_with_fuel_layers(&options, &[first]).unwrap();
        let second = FuelLayer {
            slug: "erc".to_string(),
            units: "index".to_string(),
            values: vec![42.0; 6],
        };
        assert!(augment_hour_with_fuel_layers(&options, &[second.clone()]).is_err());
        options.overwrite = true;
        let summary = augment_hour_with_fuel_layers(&options, &[second.clone()]).unwrap();
        assert_eq!(summary.replaced, vec!["erc"]);
        let hour = HourReader::open(&summary.hour_path).unwrap();
        assert_eq!(hour.read_full_2d("erc").unwrap(), second.values);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

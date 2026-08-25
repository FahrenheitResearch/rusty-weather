use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Utc};
use rw_query::{RunSnapshot, ensure_variable_metadata_compatible};
use rw_store::atomic::atomic_write_bytes;
use rw_store::format::{CODEC_2D, RwsExactTime, RwsVariableMeta, RwsWriterInfo};
use rw_store::grid::{GridFile, encode_grid_bytes};
use rw_store::lock::RunLock;
use rw_store::reader::HourReader;
use rw_store::run::{RwsHourEntry, RwsRunManifest, RwsSourceProvenance};
use rw_store::writer::HourWriter;

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GridPlane, ObservationError, ObservationFrame, ObservationResult,
    ObservationValueSemantics, StoredFrameRef, StoredPlaneRef, observation_display_hint,
    observation_display_hint_from_selector,
};

const WRITER_BUILD: &str = concat!("rw-observations ", env!("CARGO_PKG_VERSION"));
const FRAME_SCHEMA: &str = "rw-observations.stored-frame.v1";
const RUN_LOCK_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct OpenedStoredPlane {
    pub model: String,
    pub run: String,
    pub storage_slot: u16,
    pub valid_unix: i64,
    pub variable: String,
    pub units: String,
    pub selector: serde_json::Value,
    pub grid: GridFile,
    pub values: Vec<f32>,
}

pub fn sanitize_token(value: &str) -> String {
    let mut output = String::with_capacity(value.len().min(96));
    let mut separator = false;
    for character in value.chars().take(256) {
        let mapped = if character.is_ascii_alphanumeric() {
            separator = false;
            Some(character.to_ascii_lowercase())
        } else if matches!(character, '-' | '_' | '.') {
            if separator {
                None
            } else {
                separator = true;
                Some('-')
            }
        } else if separator {
            None
        } else {
            separator = true;
            Some('-')
        };
        if let Some(character) = mapped {
            output.push(character);
            if output.len() >= 96 {
                break;
            }
        }
    }
    let output = output.trim_matches(['-', '_', '.']).to_string();
    if output.is_empty() {
        "unknown".to_string()
    } else {
        output
    }
}

pub fn write_observation_frame(
    store_root: &Path,
    frame: &ObservationFrame,
) -> ObservationResult<StoredFrameRef> {
    write_observation_frame_with_limit(store_root, frame, DEFAULT_MAXIMUM_GRID_CELLS)
}

pub fn write_observation_frame_with_limit(
    store_root: &Path,
    frame: &ObservationFrame,
    maximum_cells: usize,
) -> ObservationResult<StoredFrameRef> {
    frame.validate(maximum_cells)?;
    let model = frame.family.model_slug().to_string();
    let date = DateTime::<Utc>::from_timestamp(frame.valid_unix, 0)
        .ok_or_else(|| ObservationError::Invalid("valid_unix is outside UTC range".into()))?;
    let origin = date
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .expect("midnight is valid")
        .and_utc()
        .timestamp();
    let lead_seconds = u64::try_from(frame.valid_unix.saturating_sub(origin))
        .map_err(|_| ObservationError::Invalid("valid time precedes its UTC-day origin".into()))?;

    fs::create_dir_all(store_root)?;
    let model_dir = store_root.join(&model);
    fs::create_dir_all(&model_dir)?;

    let (grid_bytes, grid_hash) = encode_grid_bytes(&frame.grid, frame.projection.as_ref())?;
    let hash_prefix = &grid_hash[..12.min(grid_hash.len())];
    let collection = sanitize_token(&frame.collection);
    let product = sanitize_token(&frame.product);
    let day = format!("{:04}{:02}{:02}", date.year(), date.month(), date.day());
    let base = format!("{collection}-{product}-{day}-{hash_prefix}");
    let variables = frame
        .planes
        .iter()
        .map(|plane| plane.name.clone())
        .collect::<Vec<_>>();
    let source_provenance = source_provenance(frame)?;

    for variant in 0u64..=u64::MAX {
        let run = if variant == 0 {
            base.clone()
        } else {
            format!("{base}-v{variant}")
        };
        let run_dir = model_dir.join(&run);
        fs::create_dir_all(&run_dir)?;
        let lock = RunLock::acquire(&run_dir, RUN_LOCK_TIMEOUT)?;
        let grid_path = run_dir.join("grid.rwg");
        if grid_path.exists() {
            let existing = GridFile::open(&grid_path)?;
            if existing.hash != grid_hash {
                drop(lock);
                continue;
            }
        } else {
            atomic_write_bytes(&grid_path, &grid_bytes)?;
        }

        let manifest_path = run_dir.join("run.json");
        let writer_info = RwsWriterInfo {
            name: "rw-observations".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            build: WRITER_BUILD.to_string(),
        };
        let mut manifest = RwsRunManifest::load_or_new_exact(
            &manifest_path,
            &model,
            &run,
            &grid_hash,
            frame.grid.shape.nx,
            frame.grid.shape.ny,
            writer_info,
        )?;

        if let Some((&slot, entry)) = manifest.hours.iter().find(|(_, entry)| {
            entry.valid_unix == Some(frame.valid_unix) && entry.variables == variables
        }) {
            let frame_path = run_dir.join(&entry.file);
            if let Some(bytes) =
                stored_frame_matches(&frame_path, entry, frame, &grid_hash, &source_provenance)?
            {
                return Ok(StoredFrameRef {
                    schema: FRAME_SCHEMA.to_string(),
                    model,
                    run,
                    storage_slot: slot,
                    valid_unix: frame.valid_unix,
                    variables,
                    grid_hash,
                    frame_file: entry.file.clone(),
                    bytes,
                    duplicate: true,
                });
            }
            // The append-only run already owns this exact time. Preserve it
            // and place the revised scientific payload in a new run variant.
            drop(lock);
            continue;
        }

        if !frame_metadata_is_compatible(&run_dir, &manifest, frame)? {
            // Scientific metadata is part of run identity. A legitimate
            // upstream change starts a new run instead of poisoning every
            // query that spans the transition.
            drop(lock);
            continue;
        }

        let can_append = manifest
            .hours
            .iter()
            .next_back()
            .and_then(|(_, entry)| entry.valid_unix)
            .is_none_or(|last| frame.valid_unix > last);
        if !can_append {
            drop(lock);
            continue;
        }
        let storage_slot = match manifest.hours.keys().next_back().copied() {
            Some(last) => match last.checked_add(1) {
                Some(slot) => slot,
                None => {
                    drop(lock);
                    continue;
                }
            },
            None => 0,
        };
        let frame_file = format!("f{storage_slot:03}.rws");
        let frame_path = run_dir.join(&frame_file);
        let started = Instant::now();
        let mut writer = HourWriter::new_exact(
            &model,
            &run,
            storage_slot,
            RwsExactTime::new(lead_seconds, frame.valid_unix),
            frame.grid.shape.nx,
            frame.grid.shape.ny,
            &grid_hash,
            WRITER_BUILD,
        );
        for plane in &frame.planes {
            let selector = observation_selector(frame, plane);
            writer.add_surface2d(&plane.name, &plane.units, selector, &plane.values)?;
        }
        writer.finish(&frame_path)?;
        let encode_ms = started.elapsed().as_millis() as u64;
        let bytes = match validate_written_frame(
            &frame_path,
            frame,
            &model,
            &run,
            storage_slot,
            lead_seconds,
            &grid_hash,
        ) {
            Ok(bytes) => bytes,
            Err(error) => {
                let _ = fs::remove_file(&frame_path);
                return Err(error);
            }
        };
        let written_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        manifest.register_hour(
            storage_slot,
            RwsHourEntry {
                file: frame_file.clone(),
                lead_seconds: Some(lead_seconds),
                valid_unix: Some(frame.valid_unix),
                written_unix,
                encode_ms,
                variables: variables.clone(),
                source_provenance: source_provenance.clone(),
            },
        );
        manifest.save(&manifest_path)?;
        return Ok(StoredFrameRef {
            schema: FRAME_SCHEMA.to_string(),
            model,
            run,
            storage_slot,
            valid_unix: frame.valid_unix,
            variables,
            grid_hash,
            frame_file,
            bytes,
            duplicate: false,
        });
    }

    Err(ObservationError::Invalid(
        "could not allocate a compatible append-only observation run".into(),
    ))
}

fn observation_selector(frame: &ObservationFrame, plane: &GridPlane) -> serde_json::Value {
    let source_display =
        observation_display_hint_from_selector(&plane.name, &plane.units, &plane.selector);
    let display = if source_display.semantics == ObservationValueSemantics::GenericScalar {
        observation_display_hint(frame.family, &plane.name, &plane.units)
    } else {
        source_display
    };
    serde_json::json!({
        "observation": {
            "family": frame.family,
            "collection": frame.collection,
            "product": frame.product,
        },
        "grid_display": {
            "geometry": "structured_curvilinear_lat_lon",
            "sample_location": "cell_center",
            "mask": "non_finite_values",
            "bbox_texture_safe": false,
        },
        "display": display,
        "source_selector": plane.selector,
    })
}

fn source_provenance(frame: &ObservationFrame) -> ObservationResult<Vec<RwsSourceProvenance>> {
    let provider = if frame.provenance_provider.trim().is_empty() {
        frame.family.model_slug().to_string()
    } else {
        sanitize_token(&frame.provenance_provider)
    };
    let roles = if frame.provenance_roles.is_empty() {
        vec!["observation".to_string()]
    } else {
        frame
            .provenance_roles
            .iter()
            .map(|value| sanitize_token(value))
            .collect()
    };
    let products = if frame.provenance_products.is_empty() {
        vec![sanitize_token(&frame.product)]
    } else {
        frame
            .provenance_products
            .iter()
            .map(|value| sanitize_token(value))
            .collect()
    };
    Ok(vec![RwsSourceProvenance::new(provider, roles, products)?])
}

fn planned_variable_meta(frame: &ObservationFrame, plane: &GridPlane) -> RwsVariableMeta {
    RwsVariableMeta {
        // Variable ids are storage-local ordinals and are intentionally not
        // part of rw-query's scientific compatibility contract.
        id: 0,
        name: plane.name.clone(),
        units: plane.units.clone(),
        kind: "surface2d".to_string(),
        codec: CODEC_2D.to_string(),
        levels_hpa: Vec::new(),
        selector: observation_selector(frame, plane),
    }
}

fn metadata_matches(
    expected: &RwsVariableMeta,
    actual: &RwsVariableMeta,
) -> ObservationResult<bool> {
    match ensure_variable_metadata_compatible(expected, actual) {
        Ok(()) => Ok(true),
        Err(rw_query::QueryError::InconsistentVariable { .. }) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn frame_metadata_is_compatible(
    run_dir: &Path,
    manifest: &RwsRunManifest,
    frame: &ObservationFrame,
) -> ObservationResult<bool> {
    for plane in &frame.planes {
        let Some(entry) = manifest
            .hours
            .values()
            .find(|entry| entry.variables.contains(&plane.name))
        else {
            continue;
        };
        let reader = HourReader::open(&run_dir.join(&entry.file))?;
        let stored = reader.variable(&plane.name).ok_or_else(|| {
            ObservationError::Invalid(format!(
                "run manifest inventories '{}' but {} does not contain it",
                plane.name, entry.file
            ))
        })?;
        if !metadata_matches(stored, &planned_variable_meta(frame, plane))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn stored_frame_matches(
    frame_path: &Path,
    entry: &RwsHourEntry,
    frame: &ObservationFrame,
    grid_hash: &str,
    source_provenance: &[RwsSourceProvenance],
) -> ObservationResult<Option<u64>> {
    let metadata = fs::symlink_metadata(frame_path)?;
    if !metadata.file_type().is_file() {
        return Err(ObservationError::Invalid(format!(
            "stored observation frame is not a regular file: {}",
            frame_path.display()
        )));
    }
    if entry.source_provenance != source_provenance {
        return Ok(None);
    }

    let reader = HourReader::open(frame_path)?;
    let meta = reader.meta();
    let stored_variables = meta
        .variables
        .iter()
        .map(|variable| variable.name.as_str())
        .collect::<Vec<_>>();
    let requested_variables = frame
        .planes
        .iter()
        .map(|plane| plane.name.as_str())
        .collect::<Vec<_>>();
    if meta.valid_unix != Some(frame.valid_unix)
        || meta.nx != frame.grid.shape.nx
        || meta.ny != frame.grid.shape.ny
        || meta.grid_hash != grid_hash
        || stored_variables != requested_variables
    {
        return Ok(None);
    }

    for plane in &frame.planes {
        let Some(stored) = reader.variable(&plane.name) else {
            return Ok(None);
        };
        if !metadata_matches(stored, &planned_variable_meta(frame, plane))? {
            return Ok(None);
        }
        let values = reader.read_full_2d(&plane.name)?;
        if values.len() != plane.values.len()
            || !values
                .iter()
                .zip(&plane.values)
                .all(|(&stored, &incoming)| {
                    stored == incoming || (stored.is_nan() && incoming.is_nan())
                })
        {
            return Ok(None);
        }
    }

    Ok(Some(metadata.len()))
}

#[allow(clippy::too_many_arguments)]
fn validate_written_frame(
    frame_path: &Path,
    frame: &ObservationFrame,
    model: &str,
    run: &str,
    storage_slot: u16,
    lead_seconds: u64,
    grid_hash: &str,
) -> ObservationResult<u64> {
    let metadata = fs::symlink_metadata(frame_path)?;
    if !metadata.file_type().is_file() {
        return Err(ObservationError::Invalid(format!(
            "new observation frame is not a regular file: {}",
            frame_path.display()
        )));
    }
    let reader = HourReader::open(frame_path)?;
    let meta = reader.meta();
    let stored_variables = meta
        .variables
        .iter()
        .map(|variable| variable.name.as_str())
        .collect::<Vec<_>>();
    let requested_variables = frame
        .planes
        .iter()
        .map(|plane| plane.name.as_str())
        .collect::<Vec<_>>();
    if meta.model != model
        || meta.run != run
        || meta.forecast_hour != storage_slot
        || meta.lead_seconds != Some(lead_seconds)
        || meta.valid_unix != Some(frame.valid_unix)
        || meta.nx != frame.grid.shape.nx
        || meta.ny != frame.grid.shape.ny
        || meta.grid_hash != grid_hash
        || stored_variables != requested_variables
    {
        return Err(ObservationError::Invalid(format!(
            "new observation frame identity does not match its requested run at {}",
            frame_path.display()
        )));
    }
    for plane in &frame.planes {
        let stored = reader.variable(&plane.name).ok_or_else(|| {
            ObservationError::Invalid(format!(
                "new observation frame omitted variable '{}'",
                plane.name
            ))
        })?;
        if !metadata_matches(stored, &planned_variable_meta(frame, plane))? {
            return Err(ObservationError::Invalid(format!(
                "new observation frame changed metadata for '{}' while writing",
                plane.name
            )));
        }
    }
    Ok(metadata.len())
}

pub fn read_stored_plane(
    store_root: &Path,
    reference: &StoredPlaneRef,
) -> ObservationResult<OpenedStoredPlane> {
    let snapshot = RunSnapshot::open(store_root, &reference.model, &reference.run)?;
    let time = snapshot.timepoint(reference.storage_slot)?;
    let entry = snapshot
        .manifest()
        .hours
        .get(&reference.storage_slot)
        .ok_or_else(|| ObservationError::Invalid("storage slot disappeared".into()))?;
    let frame_path = snapshot
        .store_root()
        .join(&reference.model)
        .join(&reference.run)
        .join(&entry.file);
    let reader = HourReader::open(&frame_path)?;
    let variable = reader.variable(&reference.variable).ok_or_else(|| {
        ObservationError::Invalid(format!(
            "variable '{}' is absent from {}/{} slot {}",
            reference.variable, reference.model, reference.run, reference.storage_slot
        ))
    })?;
    if variable.kind != "surface2d" {
        return Err(ObservationError::Invalid(format!(
            "variable '{}' has kind '{}'; a 2-D plane is required",
            reference.variable, variable.kind
        )));
    }
    let values = reader.read_full_2d(&reference.variable)?;
    Ok(OpenedStoredPlane {
        model: reference.model.clone(),
        run: reference.run.clone(),
        storage_slot: reference.storage_slot,
        valid_unix: time.valid_unix,
        variable: reference.variable.clone(),
        units: variable.units.clone(),
        selector: variable.selector.clone(),
        grid: snapshot.grid().clone(),
        values,
    })
}

pub fn frame_path(
    store_root: &Path,
    model: &str,
    run: &str,
    storage_slot: u16,
) -> ObservationResult<(PathBuf, i64)> {
    let snapshot = RunSnapshot::open(store_root, model, run)?;
    let time = snapshot.timepoint(storage_slot)?;
    let entry = snapshot
        .manifest()
        .hours
        .get(&storage_slot)
        .ok_or_else(|| ObservationError::Invalid("storage slot disappeared".into()))?;
    Ok((
        snapshot
            .store_root()
            .join(model)
            .join(run)
            .join(&entry.file),
        time.valid_unix,
    ))
}

pub fn encode_grid_blob(grid: &GridFile) -> Vec<u8> {
    let cells = grid.nx.saturating_mul(grid.ny);
    let mut bytes = Vec::with_capacity(32 + cells.saturating_mul(8));
    bytes.extend_from_slice(b"RWOBGRID");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&(grid.nx as u32).to_le_bytes());
    bytes.extend_from_slice(&(grid.ny as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for value in &grid.lat {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    for value in &grid.lon {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub fn encode_plane_blob(
    variable: &str,
    units: &str,
    valid_unix: i64,
    nx: usize,
    ny: usize,
    values: &[f32],
) -> ObservationResult<Vec<u8>> {
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| ObservationError::Invalid("plane shape overflows".into()))?;
    if values.len() != cells {
        return Err(ObservationError::Invalid(format!(
            "plane has {} values; expected {cells}",
            values.len()
        )));
    }
    let variable_bytes = variable.as_bytes();
    let unit_bytes = units.as_bytes();
    let variable_len = u16::try_from(variable_bytes.len())
        .map_err(|_| ObservationError::Invalid("variable name is too long".into()))?;
    let unit_len = u16::try_from(unit_bytes.len())
        .map_err(|_| ObservationError::Invalid("unit string is too long".into()))?;
    let mut bytes = Vec::with_capacity(40 + variable_bytes.len() + unit_bytes.len() + cells * 4);
    bytes.extend_from_slice(b"RWOBF32\0");
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&(nx as u32).to_le_bytes());
    bytes.extend_from_slice(&(ny as u32).to_le_bytes());
    bytes.extend_from_slice(&valid_unix.to_le_bytes());
    bytes.extend_from_slice(&variable_len.to_le_bytes());
    bytes.extend_from_slice(&unit_len.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(variable_bytes);
    bytes.extend_from_slice(unit_bytes);
    for value in values {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_frame(
        valid_unix: i64,
        units: &str,
        values: [f32; 2],
        selector: serde_json::Value,
    ) -> ObservationFrame {
        ObservationFrame::from_regular_grid(
            crate::ObservationFamily::Mrms,
            "mrms",
            "reflectivity",
            valid_unix,
            2,
            1,
            vec![35.0, 35.0],
            vec![-100.0, -99.0],
            Some(rustwx_core::GridProjection::Geographic),
            vec![GridPlane {
                name: "mrms_reflectivity".to_string(),
                units: units.to_string(),
                selector,
                values: values.to_vec(),
            }],
        )
        .unwrap()
    }

    #[test]
    fn token_sanitizer_is_stable_and_path_safe() {
        assert_eq!(sanitize_token("MRMS / Composite REF"), "mrms-composite-ref");
        assert_eq!(sanitize_token("../../"), "unknown");
        assert_eq!(sanitize_token("A___B"), "a-b");
    }

    #[test]
    fn binary_plane_header_has_expected_magic_and_shape() {
        let bytes = encode_plane_blob("ref", "dBZ", 123, 2, 1, &[1.0, 2.0]).unwrap();
        assert_eq!(&bytes[..8], b"RWOBF32\0");
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 2);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1);
    }

    #[test]
    fn stored_selector_carries_palette_mask_and_mesh_contract() {
        let plane = GridPlane {
            name: "radar_velocity".to_string(),
            units: "m/s".to_string(),
            selector: serde_json::json!({"radar": {"site_id": "KRTX"}}),
            values: vec![1.0],
        };
        let frame = ObservationFrame::from_regular_grid(
            crate::ObservationFamily::Radar,
            "krtx",
            "velocity-lowest",
            123,
            1,
            1,
            vec![45.0],
            vec![-122.0],
            Some(rustwx_core::GridProjection::Geographic),
            vec![plane.clone()],
        )
        .unwrap();
        let selector = observation_selector(&frame, &plane);
        assert_eq!(
            selector
                .pointer("/display/semantics")
                .and_then(|value| value.as_str()),
            Some("radial_velocity")
        );
        assert_eq!(
            selector
                .pointer("/display/palette")
                .and_then(|value| value.as_str()),
            Some("velocity")
        );
        assert_eq!(
            selector
                .pointer("/grid_display/bbox_texture_safe")
                .and_then(|value| value.as_bool()),
            Some(false)
        );
        assert_eq!(
            selector
                .pointer("/grid_display/mask")
                .and_then(|value| value.as_str()),
            Some("non_finite_values")
        );
    }

    #[test]
    fn duplicate_requires_the_same_scientific_payload_and_a_real_file() {
        let directory = tempfile::tempdir().unwrap();
        let frame = test_frame(
            1_700_000_000,
            "dBZ",
            [12.0, f32::NAN],
            serde_json::json!({}),
        );

        let first = write_observation_frame(directory.path(), &frame).unwrap();
        assert!(!first.duplicate);
        let duplicate = write_observation_frame(directory.path(), &frame).unwrap();
        assert!(duplicate.duplicate);
        assert_eq!(duplicate.run, first.run);
        assert_eq!(duplicate.bytes, first.bytes);

        fs::remove_file(
            directory
                .path()
                .join(&first.model)
                .join(&first.run)
                .join(&first.frame_file),
        )
        .unwrap();
        assert!(write_observation_frame(directory.path(), &frame).is_err());
    }

    #[test]
    fn infinite_values_are_rejected_before_store_mutation() {
        let directory = tempfile::tempdir().unwrap();
        let frame = test_frame(
            1_700_000_000,
            "dBZ",
            [f32::INFINITY, f32::NAN],
            serde_json::json!({}),
        );
        assert!(write_observation_frame(directory.path(), &frame).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    }

    #[test]
    fn revised_same_time_is_preserved_in_a_new_run_variant() {
        let directory = tempfile::tempdir().unwrap();
        let first_frame = test_frame(
            1_700_000_000,
            "dBZ",
            [12.0, f32::NAN],
            serde_json::json!({}),
        );
        let mut revised_frame = first_frame.clone();
        revised_frame.planes[0].values[0] = 18.0;

        let first = write_observation_frame(directory.path(), &first_frame).unwrap();
        let revised = write_observation_frame(directory.path(), &revised_frame).unwrap();
        assert!(!revised.duplicate);
        assert_ne!(revised.run, first.run);

        let first_reader = HourReader::open(
            &directory
                .path()
                .join(&first.model)
                .join(&first.run)
                .join(&first.frame_file),
        )
        .unwrap();
        assert_eq!(
            first_reader.read_full_2d("mrms_reflectivity").unwrap()[0],
            12.0
        );
        let revised_reader = HourReader::open(
            &directory
                .path()
                .join(&revised.model)
                .join(&revised.run)
                .join(&revised.frame_file),
        )
        .unwrap();
        assert_eq!(
            revised_reader.read_full_2d("mrms_reflectivity").unwrap()[0],
            18.0
        );
    }

    #[test]
    fn scientific_metadata_change_starts_a_new_queryable_run() {
        let directory = tempfile::tempdir().unwrap();
        let first_frame = test_frame(
            1_700_000_000,
            "dBZ",
            [12.0, f32::NAN],
            serde_json::json!({}),
        );
        let changed_units =
            test_frame(1_700_000_060, "K", [285.0, f32::NAN], serde_json::json!({}));

        let first = write_observation_frame(directory.path(), &first_frame).unwrap();
        let changed = write_observation_frame(directory.path(), &changed_units).unwrap();
        assert_ne!(changed.run, first.run);
        assert_eq!(
            RunSnapshot::open(directory.path(), &first.model, &first.run)
                .unwrap()
                .variable_capabilities()
                .unwrap()[0]
                .units,
            "dBZ"
        );
        assert_eq!(
            RunSnapshot::open(directory.path(), &changed.model, &changed.run)
                .unwrap()
                .variable_capabilities()
                .unwrap()[0]
                .units,
            "K"
        );
    }

    /// A frame shaped like the NEXRAD Level II follower's output for one
    /// archive volume. `volume_time_ms` and the optional exact acquisition
    /// identity vary per volume; the f32-promoted sweep floats are the exact
    /// kind of value the production selector carries (`nyquist_velocity` is
    /// decoded as `raw_u16 as f32 / 100.0` in wx-radar).
    fn nexrad_level2_frame(
        valid_unix: i64,
        volume_time_ms: u32,
        nyquist_m_s: f32,
        source_identity: Option<serde_json::Value>,
    ) -> ObservationFrame {
        let mut radar = serde_json::json!({
            "provider": "nexrad-level2",
            "site_id": "KTLX",
            "site_latitude": 35.3331,
            "site_longitude": -97.2778,
            "site_elevation_m": 370.0,
            "moment": "reflectivity",
            "mode": {"kind": "lowest"},
            "resolution_m": 250.0,
            "radius_km": 230.0,
            "volume_date": 20690u32,
            "volume_time_ms": volume_time_ms,
            "sweep_count": 14u32,
            "selected_sweeps": [{
                "sweep_index": 0u32,
                "elevation_angle_deg": 0.4833984f32,
                "nyquist_velocity": nyquist_m_s,
                "radial_count": 720u32,
            }],
        });
        // Frames written before the archive follower existed have no
        // "source_identity" key at all; follower frames always carry it.
        if let Some(identity) = source_identity {
            radar["source_identity"] = identity;
        }
        ObservationFrame::from_regular_grid(
            crate::ObservationFamily::Radar,
            "ktlx",
            "ref-lowest",
            valid_unix,
            2,
            1,
            vec![35.0, 35.0],
            vec![-97.3, -97.2],
            Some(rustwx_core::GridProjection::Geographic),
            vec![GridPlane {
                name: "radar_reflectivity".to_string(),
                units: "dBZ".to_string(),
                selector: serde_json::json!({ "radar": radar }),
                values: vec![12.0, f32::NAN],
            }],
        )
        .unwrap()
    }

    /// The exact production nyquist shape: wx-radar decodes the radial-block
    /// nyquist as `raw_u16 as f32 / 100.0`; 9.15 m/s is a real surveillance
    /// cut value whose f64 promotion has a 17-significant-digit shortest JSON
    /// representation.
    fn surveillance_nyquist() -> f32 {
        f32::from(915u16) / 100.0
    }

    /// Reproduces a production follower's NEXRAD Level II wedge: the follower
    /// failed every cycle with "<object> decode/store failed: invalid
    /// observation request: new observation frame changed metadata for
    /// 'radar_reflectivity' while writing" and the cursor could never
    /// advance. Storing a volume must be a deterministic function of its
    /// bytes: the same selector floats must survive the write/read metadata
    /// contract, and redelivering the identical volume must be reported as a
    /// duplicate rather than an error.
    #[test]
    fn nexrad_f32_selector_floats_do_not_wedge_the_writer() {
        let nyquist = surveillance_nyquist();
        let directory = tempfile::tempdir().unwrap();
        let frame = nexrad_level2_frame(
            1_787_899_385,
            24_185_000,
            nyquist,
            Some(serde_json::json!({
                "provider_id": "unidata-nexrad-level2",
                "object_key": "2026/08/24/KTLX/KTLX20260824_064305_V06",
                "object_bytes": 4_642_133u64,
                "sha256": "a".repeat(64),
            })),
        );
        let stored = write_observation_frame(directory.path(), &frame)
            .expect("a volume with f32-promoted selector floats must store");
        assert!(!stored.duplicate);

        // The stored selector is byte-faithful: the source selector reads
        // back exactly as the decoder produced it, ULPs included.
        let opened = read_stored_plane(
            directory.path(),
            &StoredPlaneRef {
                model: stored.model.clone(),
                run: stored.run.clone(),
                storage_slot: stored.storage_slot,
                variable: "radar_reflectivity".to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            opened.selector.pointer("/source_selector"),
            Some(&frame.planes[0].selector),
            "stored selector must equal the decoded selector exactly"
        );

        // The follower retries the same object after any failure; the retry
        // must converge as a duplicate instead of failing forever.
        let retried = write_observation_frame(directory.path(), &frame).unwrap();
        assert!(retried.duplicate);
        assert_eq!(retried.run, stored.run);

        // The property the store's post-write validation depends on: the
        // shortest JSON text of every stored float re-parses to the
        // identical f64.
        let text = serde_json::to_string(&f64::from(nyquist)).unwrap();
        assert_eq!(
            serde_json::from_str::<f64>(&text).unwrap(),
            f64::from(nyquist),
            "selector float {text} must survive a JSON round trip exactly"
        );
    }

    #[test]
    fn same_contract_appends_with_f32_selector_floats_share_one_run() {
        let directory = tempfile::tempdir().unwrap();
        let selector = serde_json::json!({
            "radar_mosaic": {
                "source_semantics": "reflectivity",
                "calibration_gain": surveillance_nyquist(),
            }
        });
        let first_frame = test_frame(1_700_000_000, "dBZ", [12.0, f32::NAN], selector.clone());
        let second_frame = test_frame(1_700_000_060, "dBZ", [18.0, f32::NAN], selector);

        let first = write_observation_frame(directory.path(), &first_frame).unwrap();
        let second = write_observation_frame(directory.path(), &second_frame).unwrap();
        assert_eq!(
            second.run, first.run,
            "an unchanged scientific contract must keep appending to one run"
        );
        assert_eq!(second.storage_slot, 1);
    }

    /// A release that changes the stored selector contract mid-day (the
    /// previous release wrote no "source_identity"; the follower release
    /// does) must never wedge on the old-contract run: the new frame starts
    /// a new run with an honest identity while the old run stays untouched
    /// and servable.
    #[test]
    fn selector_contract_change_mid_day_starts_a_new_run_and_preserves_the_old() {
        let directory = tempfile::tempdir().unwrap();
        // Benign nyquist (26.65 survives serde_json's default parser) so this
        // pin holds with or without the float fix.
        let old_release_frame = nexrad_level2_frame(1_787_890_000, 21_000_000, 26.65, None);
        let follower_frame = nexrad_level2_frame(
            1_787_890_300,
            21_300_000,
            26.65,
            Some(serde_json::json!({
                "provider_id": "unidata-nexrad-level2",
                "object_key": "2026/08/24/KTLX/KTLX20260824_040500_V06",
                "object_bytes": 4_000_111u64,
                "sha256": "b".repeat(64),
            })),
        );

        let old_run = write_observation_frame(directory.path(), &old_release_frame).unwrap();
        let old_manifest_bytes = fs::read(
            directory
                .path()
                .join(&old_run.model)
                .join(&old_run.run)
                .join("run.json"),
        )
        .unwrap();

        let new_run = write_observation_frame(directory.path(), &follower_frame).unwrap();
        assert_ne!(
            new_run.run, old_run.run,
            "a changed selector contract must start a new run, not rewrite history"
        );
        assert!(
            new_run.run.starts_with(&old_run.run),
            "the new run keeps the same honest day/grid identity plus a variant"
        );

        // The old-contract run is untouched and still servable.
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(&old_run.model)
                    .join(&old_run.run)
                    .join("run.json"),
            )
            .unwrap(),
            old_manifest_bytes,
            "appending under a new contract must not modify the old run"
        );
        for reference in [&old_run, &new_run] {
            let opened = read_stored_plane(
                directory.path(),
                &StoredPlaneRef {
                    model: reference.model.clone(),
                    run: reference.run.clone(),
                    storage_slot: reference.storage_slot,
                    variable: "radar_reflectivity".to_string(),
                },
            )
            .unwrap();
            assert_eq!(opened.values[0], 12.0);
        }
    }

    #[test]
    fn volatile_missing_cell_count_remains_in_one_run() {
        let directory = tempfile::tempdir().unwrap();
        let selector = |normalized_cells| {
            serde_json::json!({
                "mrms": {
                    "parameter": "MergedReflectivityQC",
                    "missing_value_contract": {
                        "normalized_cells": normalized_cells,
                    }
                }
            })
        };
        let first_frame = test_frame(1_700_000_000, "dBZ", [12.0, f32::NAN], selector(1));
        let second_frame = test_frame(1_700_000_060, "dBZ", [18.0, f32::NAN], selector(2));

        let first = write_observation_frame(directory.path(), &first_frame).unwrap();
        let second = write_observation_frame(directory.path(), &second_frame).unwrap();
        assert_eq!(second.run, first.run);
        assert_eq!(second.storage_slot, 1);
        assert_eq!(
            RunSnapshot::open(directory.path(), &first.model, &first.run)
                .unwrap()
                .variable_capabilities()
                .unwrap()[0]
                .available_samples,
            2
        );
    }
}

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Utc};
use rw_query::RunSnapshot;
use rw_store::atomic::atomic_write_bytes;
use rw_store::format::{RwsExactTime, RwsWriterInfo};
use rw_store::grid::{GridFile, encode_grid_bytes};
use rw_store::lock::RunLock;
use rw_store::reader::HourReader;
use rw_store::run::{RwsHourEntry, RwsRunManifest, RwsSourceProvenance};
use rw_store::writer::HourWriter;

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GridPlane, ObservationError, ObservationFrame, ObservationResult,
    StoredFrameRef, StoredPlaneRef,
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

    for variant in 0..1_000u16 {
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
            let bytes = fs::metadata(&frame_path)
                .map(|metadata| metadata.len())
                .unwrap_or(0);
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
        let bytes = fs::metadata(&frame_path)?.len();
        let written_unix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let source_provenance = source_provenance(frame)?;
        manifest.register_hour(
            storage_slot,
            RwsHourEntry {
                file: frame_file.clone(),
                lead_seconds: Some(lead_seconds),
                valid_unix: Some(frame.valid_unix),
                written_unix,
                encode_ms,
                variables: variables.clone(),
                source_provenance,
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
    serde_json::json!({
        "observation": {
            "family": frame.family,
            "collection": frame.collection,
            "product": frame.product,
            "valid_unix": frame.valid_unix,
        },
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
}

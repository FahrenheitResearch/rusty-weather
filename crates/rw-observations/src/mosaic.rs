use std::path::Path;

use rayon::prelude::*;
use rustwx_core::GridProjection;
use rw_store::grid::GridLocator;
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GeographicGridSpec, GridPlane, ObservationError, ObservationFamily,
    ObservationFrame, ObservationResult, StoredFrameRef, StoredPlaneRef, read_stored_plane,
    sanitize_token, write_observation_frame_with_limit,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MosaicMethod {
    Maximum,
    Mean,
    Latest,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RadarMosaicRequest {
    pub inputs: Vec<StoredPlaneRef>,
    pub target: GeographicGridSpec,
    pub method: MosaicMethod,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub variable: Option<String>,
    #[serde(default)]
    pub units: Option<String>,
}

pub fn build_radar_mosaic(
    store_root: &Path,
    request: &RadarMosaicRequest,
    maximum_cells: usize,
    maximum_inputs: usize,
) -> ObservationResult<ObservationFrame> {
    if request.inputs.is_empty() || request.inputs.len() > maximum_inputs {
        return Err(ObservationError::Invalid(format!(
            "radar mosaic requires 1..={maximum_inputs} inputs"
        )));
    }
    let opened = request
        .inputs
        .iter()
        .map(|reference| read_stored_plane(store_root, reference))
        .collect::<ObservationResult<Vec<_>>>()?;
    let target = request.target.build(maximum_cells)?;
    let valid_unix = opened
        .iter()
        .map(|plane| plane.valid_unix)
        .max()
        .ok_or_else(|| ObservationError::Transform("mosaic input list is empty".into()))?;
    let default_units = opened[0].units.clone();
    if request.units.is_none() && opened.iter().any(|plane| plane.units != default_units) {
        return Err(ObservationError::Invalid(
            "mosaic inputs have different units; specify output units explicitly only after converting them"
                .into(),
        ));
    }
    let sources = opened
        .iter()
        .map(|plane| MosaicSource {
            locator: GridLocator::build(&plane.grid),
            nx: plane.grid.nx,
            values: &plane.values,
            valid_unix: plane.valid_unix,
        })
        .collect::<Vec<_>>();
    let values = (0..target.shape.len())
        .into_par_iter()
        .map(|index| {
            let latitude = f64::from(target.lat_deg[index]);
            let longitude = f64::from(target.lon_deg[index]);
            let mut maximum: Option<f32> = None;
            let mut sum = 0.0f64;
            let mut count = 0u32;
            let mut latest: Option<(i64, f32)> = None;
            for source in &sources {
                let Some((fx, fy)) = source.locator.locate(latitude, longitude) else {
                    continue;
                };
                let x = (fx.round() as usize).min(source.nx.saturating_sub(1));
                let source_ny = source.values.len() / source.nx.max(1);
                let y = (fy.round() as usize).min(source_ny.saturating_sub(1));
                let value = source.values[y * source.nx + x];
                if !value.is_finite() {
                    continue;
                }
                match request.method {
                    MosaicMethod::Maximum => {
                        maximum = Some(maximum.map_or(value, |old| old.max(value)));
                    }
                    MosaicMethod::Mean => {
                        sum += f64::from(value);
                        count += 1;
                    }
                    MosaicMethod::Latest => {
                        if latest.is_none_or(|(time, _)| source.valid_unix >= time) {
                            latest = Some((source.valid_unix, value));
                        }
                    }
                }
            }
            match request.method {
                MosaicMethod::Maximum => maximum.unwrap_or(f32::NAN),
                MosaicMethod::Mean if count > 0 => (sum / f64::from(count)) as f32,
                MosaicMethod::Mean => f32::NAN,
                MosaicMethod::Latest => latest.map(|(_, value)| value).unwrap_or(f32::NAN),
            }
        })
        .collect::<Vec<_>>();
    let input_metadata = opened
        .iter()
        .map(|plane| {
            serde_json::json!({
                "model": &plane.model,
                "run": &plane.run,
                "storage_slot": plane.storage_slot,
                "variable": &plane.variable,
                "valid_unix": plane.valid_unix,
                "grid_hash": &plane.grid.hash,
            })
        })
        .collect::<Vec<_>>();
    let variable = request
        .variable
        .clone()
        .unwrap_or_else(|| "radar_mosaic".to_string());
    let product = request
        .product
        .clone()
        .unwrap_or_else(|| format!("{:?}", request.method).to_ascii_lowercase());
    Ok(ObservationFrame {
        family: ObservationFamily::RadarMosaic,
        collection: request
            .collection
            .clone()
            .unwrap_or_else(|| "custom".to_string()),
        product,
        valid_unix,
        grid: target,
        projection: Some(GridProjection::Geographic),
        planes: vec![GridPlane {
            name: variable,
            units: request.units.clone().unwrap_or(default_units),
            selector: serde_json::json!({
                "radar_mosaic": {
                    "method": request.method,
                    "inputs": input_metadata,
                    "target": request.target,
                }
            }),
            values,
        }],
        provenance_provider: "rw-observations-mosaic".to_string(),
        provenance_roles: vec!["radar".to_string(), "mosaic".to_string()],
        provenance_products: opened
            .iter()
            .map(|plane| sanitize_token(&plane.variable))
            .take(16)
            .collect(),
    })
}

pub fn build_and_store_radar_mosaic(
    store_root: &Path,
    request: &RadarMosaicRequest,
    maximum_cells: usize,
    maximum_inputs: usize,
) -> ObservationResult<StoredFrameRef> {
    let frame = build_radar_mosaic(store_root, request, maximum_cells, maximum_inputs)?;
    write_observation_frame_with_limit(store_root, &frame, maximum_cells)
}

pub fn build_and_store_radar_mosaic_default_limits(
    store_root: &Path,
    request: &RadarMosaicRequest,
) -> ObservationResult<StoredFrameRef> {
    build_and_store_radar_mosaic(store_root, request, DEFAULT_MAXIMUM_GRID_CELLS, 32)
}

struct MosaicSource<'a> {
    locator: GridLocator,
    nx: usize,
    values: &'a [f32],
    valid_unix: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_grid_rejects_unbounded_allocations() {
        let spec = GeographicGridSpec {
            west_longitude: -130.0,
            south_latitude: 20.0,
            east_longitude: -60.0,
            north_latitude: 55.0,
            resolution_km: 0.05,
        };
        assert!(spec.build(10_000).is_err());
    }
}

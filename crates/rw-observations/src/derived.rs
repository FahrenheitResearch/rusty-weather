use std::path::Path;

use rayon::prelude::*;
use rustwx_core::{GridShape, LatLonGrid};
use rw_query::RunSnapshot;
use rw_store::reader::HourReader;
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GridPlane, ObservationError, ObservationFamily, ObservationFrame,
    ObservationResult, StoredFrameRef, sanitize_token, write_observation_frame_with_limit,
};

const EARTH_RADIUS_M: f64 = 6_371_000.0;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredVariableRef {
    pub model: String,
    pub run: String,
    pub storage_slot: u16,
    pub variable: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeamAggregation {
    #[default]
    Center,
    Maximum,
    Mean,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SimulatedRadarOperation {
    PassThrough,
    CompositeMax,
    PressureLevel {
        level_hpa: u16,
    },
    EchoTop {
        threshold_dbz: f32,
        height_variable: String,
    },
    Vil {
        height_variable: String,
    },
    BeamPpi {
        height_variable: String,
        radar_latitude: f64,
        radar_longitude: f64,
        radar_elevation_m: f64,
        tilt_deg: f64,
        #[serde(default = "default_beam_width_deg")]
        beam_width_deg: f64,
        #[serde(default = "default_earth_radius_factor")]
        earth_radius_factor: f64,
        #[serde(default = "default_max_range_km")]
        max_range_km: f64,
        #[serde(default)]
        aggregation: BeamAggregation,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        minimum_dbz: Option<f32>,
    },
}

const fn default_beam_width_deg() -> f64 {
    1.0
}

const fn default_earth_radius_factor() -> f64 {
    4.0 / 3.0
}

const fn default_max_range_km() -> f64 {
    230.0
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SimulatedRadarRequest {
    pub source: StoredVariableRef,
    pub operation: SimulatedRadarOperation,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub product: Option<String>,
    #[serde(default)]
    pub variable: Option<String>,
}

pub fn derive_simulated_radar(
    store_root: &Path,
    request: &SimulatedRadarRequest,
    maximum_cells: usize,
) -> ObservationResult<ObservationFrame> {
    validate_request(request)?;
    let snapshot = RunSnapshot::open(store_root, &request.source.model, &request.source.run)?;
    let time = snapshot.timepoint(request.source.storage_slot)?;
    let entry = snapshot
        .manifest()
        .hours
        .get(&request.source.storage_slot)
        .ok_or_else(|| ObservationError::Transform("source storage slot disappeared".into()))?;
    let frame_path = snapshot
        .store_root()
        .join(&request.source.model)
        .join(&request.source.run)
        .join(&entry.file);
    let reader = HourReader::open(&frame_path)?;
    let source_meta = reader
        .variable(&request.source.variable)
        .cloned()
        .ok_or_else(|| {
            ObservationError::Invalid(format!(
                "source variable '{}' does not exist",
                request.source.variable
            ))
        })?;
    let nx = snapshot.grid().nx;
    let ny = snapshot.grid().ny;
    let cells = nx
        .checked_mul(ny)
        .ok_or_else(|| ObservationError::Transform("source grid size overflows".into()))?;
    if cells > maximum_cells {
        return Err(ObservationError::Invalid(format!(
            "source grid has {cells} cells; maximum is {maximum_cells}"
        )));
    }

    let (values, units, default_variable, default_product) = match source_meta.kind.as_str() {
        "surface2d" => derive_from_surface(&reader, &source_meta, request)?,
        "pressure3d" => derive_from_volume(
            &reader,
            &source_meta,
            snapshot.grid().lat.as_slice(),
            snapshot.grid().lon.as_slice(),
            cells,
            request,
        )?,
        other => {
            return Err(ObservationError::Invalid(format!(
                "source variable has unsupported kind '{other}'"
            )));
        }
    };
    let grid = LatLonGrid::new(
        GridShape::new(nx, ny)?,
        snapshot.grid().lat.clone(),
        snapshot.grid().lon.clone(),
    )?;
    let variable = request.variable.clone().unwrap_or(default_variable);
    let product = request.product.clone().unwrap_or(default_product);
    Ok(ObservationFrame {
        family: ObservationFamily::SimulatedRadar,
        collection: request
            .collection
            .clone()
            .unwrap_or_else(|| sanitize_token(&request.source.run)),
        product,
        valid_unix: time.valid_unix,
        grid,
        projection: snapshot.grid().projection.clone(),
        planes: vec![GridPlane {
            name: variable,
            units,
            selector: serde_json::json!({
                "simulated_radar": {
                    "source": &request.source,
                    "operation": &request.operation,
                    "source_selector": &source_meta.selector,
                }
            }),
            values,
        }],
        provenance_provider: sanitize_token(&request.source.model),
        provenance_roles: vec!["model".to_string(), "simulated-radar".to_string()],
        provenance_products: vec![sanitize_token(&request.source.variable)],
    })
}

pub fn derive_and_store_simulated_radar(
    store_root: &Path,
    request: &SimulatedRadarRequest,
    maximum_cells: usize,
) -> ObservationResult<StoredFrameRef> {
    let frame = derive_simulated_radar(store_root, request, maximum_cells)?;
    write_observation_frame_with_limit(store_root, &frame, maximum_cells)
}

pub fn derive_and_store_simulated_radar_default_limit(
    store_root: &Path,
    request: &SimulatedRadarRequest,
) -> ObservationResult<StoredFrameRef> {
    derive_and_store_simulated_radar(store_root, request, DEFAULT_MAXIMUM_GRID_CELLS)
}

fn derive_from_surface(
    reader: &HourReader,
    source_meta: &rw_store::format::RwsVariableMeta,
    request: &SimulatedRadarRequest,
) -> ObservationResult<(Vec<f32>, String, String, String)> {
    if !matches!(
        &request.operation,
        SimulatedRadarOperation::PassThrough | SimulatedRadarOperation::CompositeMax
    ) {
        return Err(ObservationError::Invalid(
            "the selected simulated-radar operation requires a pressure3d source variable".into(),
        ));
    }
    let values = reader.read_full_2d(&source_meta.name)?;
    Ok((
        values,
        source_meta.units.clone(),
        "simulated_radar".to_string(),
        "surface-reflectivity".to_string(),
    ))
}

fn derive_from_volume(
    reader: &HourReader,
    source_meta: &rw_store::format::RwsVariableMeta,
    latitudes: &[f32],
    longitudes: &[f32],
    cells: usize,
    request: &SimulatedRadarRequest,
) -> ObservationResult<(Vec<f32>, String, String, String)> {
    let levels = source_meta.levels_hpa.len();
    if levels == 0 {
        return Err(ObservationError::Transform(
            "pressure3d source has no levels".into(),
        ));
    }
    let reflectivity = reader.read_full_3d(&source_meta.name)?;
    match &request.operation {
        SimulatedRadarOperation::PassThrough => Err(ObservationError::Invalid(
            "pass_through is only valid for a surface2d source".into(),
        )),
        SimulatedRadarOperation::CompositeMax => Ok((
            column_maximum(&reflectivity, levels, cells),
            source_meta.units.clone(),
            "simulated_composite_reflectivity".to_string(),
            "composite-reflectivity".to_string(),
        )),
        SimulatedRadarOperation::PressureLevel { level_hpa } => {
            let level_index = source_meta
                .levels_hpa
                .iter()
                .enumerate()
                .min_by_key(|(_, level)| (**level).abs_diff(*level_hpa))
                .map(|(index, _)| index)
                .ok_or_else(|| ObservationError::Transform("source has no levels".into()))?;
            let start = level_index * cells;
            Ok((
                reflectivity[start..start + cells].to_vec(),
                source_meta.units.clone(),
                format!("simulated_reflectivity_{level_hpa}hpa"),
                format!("reflectivity-{level_hpa}hpa"),
            ))
        }
        SimulatedRadarOperation::EchoTop {
            threshold_dbz,
            height_variable,
        } => {
            let height = matching_height_volume(reader, source_meta, height_variable)?;
            Ok((
                echo_top(&reflectivity, &height, levels, cells, *threshold_dbz),
                "km MSL".to_string(),
                format!("simulated_echo_top_{threshold_dbz:.0}dbz"),
                format!("echo-top-{threshold_dbz:.0}dbz"),
            ))
        }
        SimulatedRadarOperation::Vil { height_variable } => {
            let height = matching_height_volume(reader, source_meta, height_variable)?;
            Ok((
                vil(&reflectivity, &height, levels, cells),
                "kg/m^2".to_string(),
                "simulated_vil".to_string(),
                "vil".to_string(),
            ))
        }
        SimulatedRadarOperation::BeamPpi {
            height_variable,
            radar_latitude,
            radar_longitude,
            radar_elevation_m,
            tilt_deg,
            beam_width_deg,
            earth_radius_factor,
            max_range_km,
            aggregation,
            minimum_dbz,
        } => {
            let height = matching_height_volume(reader, source_meta, height_variable)?;
            let settings = BeamSettings {
                latitude: *radar_latitude,
                longitude: *radar_longitude,
                elevation_m: *radar_elevation_m,
                tilt_deg: *tilt_deg,
                beam_width_deg: *beam_width_deg,
                earth_radius_factor: *earth_radius_factor,
                max_range_m: *max_range_km * 1_000.0,
                aggregation: *aggregation,
                minimum_dbz: *minimum_dbz,
            };
            let values = beam_ppi(
                &reflectivity,
                &height,
                levels,
                cells,
                latitudes,
                longitudes,
                settings,
            );
            Ok((
                values,
                source_meta.units.clone(),
                format!("simulated_ppi_{tilt_deg:.2}deg"),
                format!("ppi-{tilt_deg:.2}deg"),
            ))
        }
    }
}

fn matching_height_volume(
    reader: &HourReader,
    source_meta: &rw_store::format::RwsVariableMeta,
    height_variable: &str,
) -> ObservationResult<Vec<f32>> {
    let height_meta = reader.variable(height_variable).ok_or_else(|| {
        ObservationError::Invalid(format!("height variable '{height_variable}' is absent"))
    })?;
    if height_meta.kind != "pressure3d" || height_meta.levels_hpa != source_meta.levels_hpa {
        return Err(ObservationError::Invalid(format!(
            "height variable '{height_variable}' must be pressure3d on the same levels as '{}'",
            source_meta.name
        )));
    }
    Ok(reader.read_full_3d(height_variable)?)
}

fn column_maximum(values: &[f32], levels: usize, cells: usize) -> Vec<f32> {
    (0..cells)
        .into_par_iter()
        .map(|cell| {
            (0..levels)
                .map(|level| values[level * cells + cell])
                .filter(|value| value.is_finite())
                .reduce(f32::max)
                .unwrap_or(f32::NAN)
        })
        .collect()
}

fn echo_top(
    reflectivity: &[f32],
    height: &[f32],
    levels: usize,
    cells: usize,
    threshold_dbz: f32,
) -> Vec<f32> {
    (0..cells)
        .into_par_iter()
        .map(|cell| {
            let mut top: Option<f32> = None;
            for level in 0..levels {
                let dbz = reflectivity[level * cells + cell];
                let meters = height[level * cells + cell];
                if dbz.is_finite() && meters.is_finite() && dbz >= threshold_dbz {
                    top = Some(top.map_or(meters, |old| old.max(meters)));
                }
            }
            top.map(|meters| meters / 1_000.0).unwrap_or(f32::NAN)
        })
        .collect()
}

fn vil(reflectivity: &[f32], height: &[f32], levels: usize, cells: usize) -> Vec<f32> {
    (0..cells)
        .into_par_iter()
        .map(|cell| {
            let mut total = 0.0f64;
            let mut used = false;
            for level in 0..levels.saturating_sub(1) {
                let z0 = reflectivity[level * cells + cell];
                let z1 = reflectivity[(level + 1) * cells + cell];
                let h0 = height[level * cells + cell];
                let h1 = height[(level + 1) * cells + cell];
                if !z0.is_finite() || !z1.is_finite() || !h0.is_finite() || !h1.is_finite() {
                    continue;
                }
                let dz = f64::from((h1 - h0).abs());
                if dz <= 0.0 {
                    continue;
                }
                let density0 = vil_density(z0);
                let density1 = vil_density(z1);
                total += 0.5 * (density0 + density1) * dz;
                used = true;
            }
            if used { total as f32 } else { f32::NAN }
        })
        .collect()
}

fn vil_density(dbz: f32) -> f64 {
    let capped = f64::from(dbz.min(56.0));
    let z_linear = 10.0f64.powf(capped / 10.0);
    3.44e-6 * z_linear.powf(4.0 / 7.0)
}

#[derive(Debug, Clone, Copy)]
struct BeamSettings {
    latitude: f64,
    longitude: f64,
    elevation_m: f64,
    tilt_deg: f64,
    beam_width_deg: f64,
    earth_radius_factor: f64,
    max_range_m: f64,
    aggregation: BeamAggregation,
    minimum_dbz: Option<f32>,
}

fn beam_ppi(
    reflectivity: &[f32],
    height: &[f32],
    levels: usize,
    cells: usize,
    latitudes: &[f32],
    longitudes: &[f32],
    settings: BeamSettings,
) -> Vec<f32> {
    (0..cells)
        .into_par_iter()
        .map(|cell| {
            let range = haversine_m(
                settings.latitude,
                settings.longitude,
                f64::from(latitudes[cell]),
                f64::from(longitudes[cell]),
            );
            if !range.is_finite() || range > settings.max_range_m {
                return f32::NAN;
            }
            let center_height = beam_height_m(
                range,
                settings.tilt_deg,
                settings.elevation_m,
                settings.earth_radius_factor,
            );
            let half_width = settings.beam_width_deg * 0.5;
            let lower_height = beam_height_m(
                range,
                settings.tilt_deg - half_width,
                settings.elevation_m,
                settings.earth_radius_factor,
            );
            let upper_height = beam_height_m(
                range,
                settings.tilt_deg + half_width,
                settings.elevation_m,
                settings.earth_radius_factor,
            );
            let value = match settings.aggregation {
                BeamAggregation::Center => interpolate_vertical(
                    reflectivity,
                    height,
                    levels,
                    cells,
                    cell,
                    center_height as f32,
                ),
                BeamAggregation::Maximum => aggregate_beam(
                    reflectivity,
                    height,
                    levels,
                    cells,
                    cell,
                    lower_height.min(upper_height) as f32,
                    lower_height.max(upper_height) as f32,
                    true,
                )
                .or_else(|| {
                    interpolate_vertical(
                        reflectivity,
                        height,
                        levels,
                        cells,
                        cell,
                        center_height as f32,
                    )
                }),
                BeamAggregation::Mean => aggregate_beam(
                    reflectivity,
                    height,
                    levels,
                    cells,
                    cell,
                    lower_height.min(upper_height) as f32,
                    lower_height.max(upper_height) as f32,
                    false,
                )
                .or_else(|| {
                    interpolate_vertical(
                        reflectivity,
                        height,
                        levels,
                        cells,
                        cell,
                        center_height as f32,
                    )
                }),
            }
            .unwrap_or(f32::NAN);
            if settings.minimum_dbz.is_some_and(|minimum| value < minimum) {
                f32::NAN
            } else {
                value
            }
        })
        .collect()
}

fn interpolate_vertical(
    values: &[f32],
    height: &[f32],
    levels: usize,
    cells: usize,
    cell: usize,
    target_height: f32,
) -> Option<f32> {
    for level in 0..levels.saturating_sub(1) {
        let h0 = height[level * cells + cell];
        let h1 = height[(level + 1) * cells + cell];
        let v0 = values[level * cells + cell];
        let v1 = values[(level + 1) * cells + cell];
        if !h0.is_finite() || !h1.is_finite() || !v0.is_finite() || !v1.is_finite() {
            continue;
        }
        let within = (h0 <= target_height && target_height <= h1)
            || (h1 <= target_height && target_height <= h0);
        if !within || h0 == h1 {
            continue;
        }
        let fraction = (target_height - h0) / (h1 - h0);
        return Some(v0 + fraction * (v1 - v0));
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn aggregate_beam(
    values: &[f32],
    height: &[f32],
    levels: usize,
    cells: usize,
    cell: usize,
    lower_height: f32,
    upper_height: f32,
    maximum: bool,
) -> Option<f32> {
    let mut result: Option<f32> = None;
    let mut sum = 0.0f64;
    let mut count = 0u32;
    for level in 0..levels {
        let h = height[level * cells + cell];
        let value = values[level * cells + cell];
        if !h.is_finite() || !value.is_finite() || h < lower_height || h > upper_height {
            continue;
        }
        if maximum {
            result = Some(result.map_or(value, |old| old.max(value)));
        } else {
            sum += f64::from(value);
            count += 1;
        }
    }
    if maximum {
        result
    } else if count > 0 {
        Some((sum / f64::from(count)) as f32)
    } else {
        None
    }
}

fn beam_height_m(
    range_m: f64,
    tilt_deg: f64,
    radar_elevation_m: f64,
    earth_radius_factor: f64,
) -> f64 {
    let effective_radius = EARTH_RADIUS_M * earth_radius_factor;
    let tilt = tilt_deg.to_radians();
    (range_m * range_m
        + effective_radius * effective_radius
        + 2.0 * range_m * effective_radius * tilt.sin())
    .sqrt()
        - effective_radius
        + radar_elevation_m
}

fn haversine_m(lat1: f64, lon1: f64, lat2: f64, lon2: f64) -> f64 {
    let lat1 = lat1.to_radians();
    let lat2 = lat2.to_radians();
    let dlat = lat2 - lat1;
    let dlon = (lon2 - lon1).to_radians();
    let a = (dlat * 0.5).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon * 0.5).sin().powi(2);
    2.0 * EARTH_RADIUS_M * a.sqrt().atan2((1.0 - a).sqrt())
}

fn validate_request(request: &SimulatedRadarRequest) -> ObservationResult<()> {
    if request.source.model.trim().is_empty()
        || request.source.run.trim().is_empty()
        || request.source.variable.trim().is_empty()
    {
        return Err(ObservationError::Invalid(
            "simulated-radar source identifiers must be non-empty".into(),
        ));
    }
    if let SimulatedRadarOperation::BeamPpi {
        radar_latitude,
        radar_longitude,
        radar_elevation_m,
        tilt_deg,
        beam_width_deg,
        earth_radius_factor,
        max_range_km,
        ..
    } = &request.operation
        && (!radar_latitude.is_finite()
            || !radar_longitude.is_finite()
            || !radar_elevation_m.is_finite()
            || !tilt_deg.is_finite()
            || !beam_width_deg.is_finite()
            || !earth_radius_factor.is_finite()
            || !max_range_km.is_finite()
            || !(-90.0..=90.0).contains(radar_latitude)
            || !(-180.0..=180.0).contains(radar_longitude)
            || !(-1.0..=90.0).contains(tilt_deg)
            || !(0.05..=10.0).contains(beam_width_deg)
            || !(1.0..=2.0).contains(earth_radius_factor)
            || !(1.0..=1_000.0).contains(max_range_km))
    {
        return Err(ObservationError::Invalid(
            "invalid virtual-radar beam settings".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beam_height_is_positive_and_increases_with_range() {
        let near = beam_height_m(10_000.0, 0.5, 300.0, 4.0 / 3.0);
        let far = beam_height_m(200_000.0, 0.5, 300.0, 4.0 / 3.0);
        assert!(near > 300.0);
        assert!(far > near);
    }

    #[test]
    fn vertical_interpolation_uses_level_major_layout() {
        let values = vec![0.0, 10.0, 20.0, 30.0];
        let heights = vec![1_000.0, 1_000.0, 3_000.0, 3_000.0];
        assert_eq!(
            interpolate_vertical(&values, &heights, 2, 2, 1, 2_000.0),
            Some(20.0)
        );
    }

    #[test]
    fn vil_is_finite_for_two_valid_levels() {
        let result = vil(&[30.0, 40.0], &[1_000.0, 2_000.0], 2, 1);
        assert!(result[0].is_finite());
        assert!(result[0] > 0.0);
    }
}

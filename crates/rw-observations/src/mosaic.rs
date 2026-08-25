use std::path::Path;

use rayon::prelude::*;
use rustwx_core::GridProjection;
use rw_store::grid::GridLocator;
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GeographicGridSpec, GridPlane, ObservationDisplayHint,
    ObservationError, ObservationFamily, ObservationFrame, ObservationInterpolation,
    ObservationResult, ObservationValueSemantics, StoredFrameRef, StoredPlaneRef,
    observation_display_hint_from_selector, read_stored_plane, sanitize_token,
    write_observation_frame_with_limit,
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
    validate_mosaic_input_count(request.inputs.len(), maximum_inputs)?;
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
    let displays = opened
        .iter()
        .map(|plane| {
            observation_display_hint_from_selector(&plane.variable, &plane.units, &plane.selector)
        })
        .collect::<Vec<_>>();
    let semantics = displays[0].semantics;
    if displays
        .iter()
        .any(|display| display.semantics != semantics)
    {
        return Err(ObservationError::Invalid(
            "radar mosaic inputs have different scientific semantics".into(),
        ));
    }
    validate_mosaic_method(request.method, semantics)?;
    let sources = opened
        .iter()
        .zip(displays)
        .map(|(plane, display)| MosaicSource {
            locator: GridLocator::build(&plane.grid),
            nx: plane.grid.nx,
            ny: plane.grid.ny,
            values: &plane.values,
            valid_unix: plane.valid_unix,
            display,
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
                let Some(value) = source.sample(fx, fy) else {
                    continue;
                };
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
        .unwrap_or_else(|| opened[0].variable.clone());
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
                    "sampling": "semantic_validity_aware_bilinear",
                    "source_semantics": semantics,
                    "velocity_reference": if semantics == ObservationValueSemantics::RadialVelocity {
                        Some("radial_to_source_radars")
                    } else {
                        None
                    },
                    "earth_relative_wind": if semantics == ObservationValueSemantics::RadialVelocity {
                        Some(false)
                    } else {
                        None
                    },
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
            .collect(),
    })
}

fn validate_mosaic_input_count(input_count: usize, maximum_inputs: usize) -> ObservationResult<()> {
    if input_count == 0 || input_count > maximum_inputs {
        return Err(ObservationError::Invalid(format!(
            "radar mosaic requires 1..={maximum_inputs} inputs"
        )));
    }
    Ok(())
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
    build_and_store_radar_mosaic(store_root, request, DEFAULT_MAXIMUM_GRID_CELLS, usize::MAX)
}

struct MosaicSource<'a> {
    locator: GridLocator,
    nx: usize,
    ny: usize,
    values: &'a [f32],
    valid_unix: i64,
    display: ObservationDisplayHint,
}

impl MosaicSource<'_> {
    fn sample(&self, fx: f64, fy: f64) -> Option<f32> {
        if !(fx.is_finite() && fy.is_finite()) || self.nx == 0 || self.ny == 0 {
            return None;
        }
        let x = fx.clamp(0.0, self.nx.saturating_sub(1) as f64);
        let y = fy.clamp(0.0, self.ny.saturating_sub(1) as f64);
        match self.display.interpolation {
            ObservationInterpolation::Nearest => {
                sample_nearest(self.values, self.nx, self.ny, x, y)
            }
            ObservationInterpolation::CircularDegrees => {
                sample_bilinear_circular(self.values, self.nx, self.ny, x, y)
            }
            ObservationInterpolation::VelocityFoldAware => sample_bilinear_fold_aware(
                self.values,
                self.nx,
                self.ny,
                x,
                y,
                self.display.discontinuity_threshold.unwrap_or(30.0),
            ),
            ObservationInterpolation::Linear => {
                sample_bilinear_linear(self.values, self.nx, self.ny, x, y)
            }
        }
    }
}

fn validate_mosaic_method(
    method: MosaicMethod,
    semantics: ObservationValueSemantics,
) -> ObservationResult<()> {
    if !matches!(method, MosaicMethod::Latest)
        && matches!(
            semantics,
            ObservationValueSemantics::RadialVelocity
                | ObservationValueSemantics::DifferentialPhase
                | ObservationValueSemantics::HydrometeorClassification
                | ObservationValueSemantics::Rgba
        )
    {
        return Err(ObservationError::Invalid(format!(
            "mosaic method {method:?} is not scientifically valid for {} values; use latest or produce a retrieved earth-relative field",
            semantics.slug()
        )));
    }
    Ok(())
}

fn sample_nearest(values: &[f32], nx: usize, ny: usize, x: f64, y: f64) -> Option<f32> {
    let x = (x.round() as usize).min(nx.saturating_sub(1));
    let y = (y.round() as usize).min(ny.saturating_sub(1));
    values
        .get(y.checked_mul(nx)?.checked_add(x)?)
        .copied()
        .filter(|value| value.is_finite())
}

fn bilinear_samples(
    values: &[f32],
    nx: usize,
    ny: usize,
    x: f64,
    y: f64,
) -> Option<[(f32, f32); 4]> {
    if nx == 0 || ny == 0 || values.len() != nx.checked_mul(ny)? {
        return None;
    }
    let x0 = (x.floor() as usize).min(nx - 1);
    let y0 = (y.floor() as usize).min(ny - 1);
    let x1 = (x0 + 1).min(nx - 1);
    let y1 = (y0 + 1).min(ny - 1);
    let tx = (x - x0 as f64).clamp(0.0, 1.0) as f32;
    let ty = (y - y0 as f64).clamp(0.0, 1.0) as f32;
    let index = |yy: usize, xx: usize| yy * nx + xx;
    Some([
        (values[index(y0, x0)], (1.0 - tx) * (1.0 - ty)),
        (values[index(y0, x1)], tx * (1.0 - ty)),
        (values[index(y1, x0)], (1.0 - tx) * ty),
        (values[index(y1, x1)], tx * ty),
    ])
}

fn sample_bilinear_linear(values: &[f32], nx: usize, ny: usize, x: f64, y: f64) -> Option<f32> {
    weighted_finite_mean(&bilinear_samples(values, nx, ny, x, y)?)
}

fn sample_bilinear_fold_aware(
    values: &[f32],
    nx: usize,
    ny: usize,
    x: f64,
    y: f64,
    threshold: f32,
) -> Option<f32> {
    let samples = bilinear_samples(values, nx, ny, x, y)?;
    let mut minimum = f32::INFINITY;
    let mut maximum = f32::NEG_INFINITY;
    for &(value, weight) in &samples {
        if value.is_finite() && weight > 0.0 {
            minimum = minimum.min(value);
            maximum = maximum.max(value);
        }
    }
    if minimum.is_finite() && maximum.is_finite() && maximum - minimum > threshold.max(0.0) {
        nearest_finite_sample(&samples)
    } else {
        weighted_finite_mean(&samples)
    }
}

fn sample_bilinear_circular(values: &[f32], nx: usize, ny: usize, x: f64, y: f64) -> Option<f32> {
    let samples = bilinear_samples(values, nx, ny, x, y)?;
    let mut sine = 0.0f32;
    let mut cosine = 0.0f32;
    let mut total_weight = 0.0f32;
    for &(value, weight) in &samples {
        if value.is_finite() && weight > 0.0 {
            let radians = value.to_radians();
            sine += radians.sin() * weight;
            cosine += radians.cos() * weight;
            total_weight += weight;
        }
    }
    if total_weight < 0.5 || sine.hypot(cosine) <= 1.0e-6 {
        return nearest_finite_sample(&samples);
    }
    Some(sine.atan2(cosine).to_degrees().rem_euclid(360.0))
}

fn nearest_finite_sample(samples: &[(f32, f32); 4]) -> Option<f32> {
    samples
        .iter()
        .filter(|(value, _)| value.is_finite())
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .map(|(value, _)| *value)
}

fn weighted_finite_mean(samples: &[(f32, f32); 4]) -> Option<f32> {
    let mut weighted = 0.0f32;
    let mut total_weight = 0.0f32;
    for &(value, weight) in samples {
        if value.is_finite() && weight > 0.0 {
            weighted += value * weight;
            total_weight += weight;
        }
    }
    if total_weight >= 0.5 {
        Some(weighted / total_weight)
    } else {
        None
    }
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

    #[test]
    fn default_mosaic_input_policy_has_no_product_count_ceiling() {
        assert!(validate_mosaic_input_count(33, usize::MAX).is_ok());
        assert!(validate_mosaic_input_count(33, 32).is_err());
    }

    #[test]
    fn bilinear_sampling_respects_nan_coverage_edges() {
        let values = [10.0, f32::NAN, 20.0, f32::NAN];
        assert_eq!(sample_bilinear_linear(&values, 2, 2, 0.0, 0.5), Some(15.0));
        assert_eq!(sample_bilinear_linear(&values, 2, 2, 0.75, 0.5), None);
    }

    #[test]
    fn velocity_sampling_does_not_create_zero_at_a_fold_boundary() {
        let values = [-25.0, 25.0, -25.0, 25.0];
        let sampled = sample_bilinear_fold_aware(&values, 2, 2, 0.25, 0.5, 30.0);
        assert_eq!(sampled, Some(-25.0));
    }

    #[test]
    fn phase_sampling_uses_the_short_arc() {
        let values = [350.0, 10.0, 350.0, 10.0];
        let sampled = sample_bilinear_circular(&values, 2, 2, 0.5, 0.5).unwrap();
        assert!(sampled.is_finite(), "got {sampled}");
        assert!(!(1.0..=359.0).contains(&sampled), "got {sampled}");
    }

    #[test]
    fn unsafe_radial_velocity_arithmetic_is_rejected() {
        assert!(
            validate_mosaic_method(
                MosaicMethod::Mean,
                ObservationValueSemantics::RadialVelocity,
            )
            .is_err()
        );
        assert!(
            validate_mosaic_method(
                MosaicMethod::Latest,
                ObservationValueSemantics::RadialVelocity,
            )
            .is_ok()
        );
    }
}

use rustwx_core::{LatLonGrid as CoreLatLonGrid, SelectedField2D};

use crate::bilinear::build_bilinear_weights;
use crate::conservative::build_conservative_weights;
use crate::error::RegridError;
use crate::grid::{GridFingerprint, GridGeometry, GridProjection};
use crate::idw::build_idw_weights;
use crate::method::{MissingPolicy, RegridMethod, RegridOptions};
use crate::nearest::build_nearest_weights;
use crate::weights::SparseWeights;

#[derive(Clone, Debug, PartialEq)]
pub struct RegridPlan {
    pub source_grid_id: GridFingerprint,
    pub target_grid_id: GridFingerprint,
    pub method: RegridMethod,
    pub weights: SparseWeights,
    pub missing_policy: MissingPolicy,
    pub extrapolate: bool,
}

impl RegridPlan {
    pub fn build(
        source: &dyn GridGeometry,
        target: &dyn GridGeometry,
        options: RegridOptions,
    ) -> Result<Self, RegridError> {
        let weights = match &options.method {
            RegridMethod::Nearest { max_distance_km } => {
                build_nearest_weights(source, target, *max_distance_km)?
            }
            RegridMethod::Bilinear => build_bilinear_weights(source, target, options.extrapolate)?,
            RegridMethod::InverseDistance {
                k,
                power,
                radius_km,
            } => build_idw_weights(source, target, *k, *power, *radius_km)?,
            RegridMethod::Conservative { normalization } => {
                build_conservative_weights(source, target, *normalization)?
            }
        };
        Ok(Self {
            source_grid_id: source.fingerprint(),
            target_grid_id: target.fingerprint(),
            method: options.method,
            weights,
            missing_policy: options.missing_policy,
            extrapolate: options.extrapolate,
        })
    }

    pub fn apply_f32(&self, source_values: &[f32]) -> Result<Vec<f32>, RegridError> {
        let mut target_values = vec![f32::NAN; self.weights.target_len];
        self.apply_into_f32(source_values, &mut target_values)?;
        Ok(target_values)
    }

    pub fn apply_f64(&self, source_values: &[f64]) -> Result<Vec<f64>, RegridError> {
        let mut target_values = vec![f64::NAN; self.weights.target_len];
        self.apply_into_f64(source_values, &mut target_values)?;
        Ok(target_values)
    }

    pub fn apply_into_f32(
        &self,
        source_values: &[f32],
        target_values: &mut [f32],
    ) -> Result<(), RegridError> {
        if source_values.len() != self.weights.source_len {
            return Err(RegridError::ShapeMismatch {
                expected: self.weights.source_len,
                actual: source_values.len(),
            });
        }
        if target_values.len() != self.weights.target_len {
            return Err(RegridError::ShapeMismatch {
                expected: self.weights.target_len,
                actual: target_values.len(),
            });
        }
        for (target_index, target_value) in target_values.iter_mut().enumerate() {
            *target_value = apply_row_f32(
                self.weights.row(target_index),
                source_values,
                self.missing_policy,
            );
        }
        Ok(())
    }

    pub fn apply_into_f64(
        &self,
        source_values: &[f64],
        target_values: &mut [f64],
    ) -> Result<(), RegridError> {
        if source_values.len() != self.weights.source_len {
            return Err(RegridError::ShapeMismatch {
                expected: self.weights.source_len,
                actual: source_values.len(),
            });
        }
        if target_values.len() != self.weights.target_len {
            return Err(RegridError::ShapeMismatch {
                expected: self.weights.target_len,
                actual: target_values.len(),
            });
        }
        for (target_index, target_value) in target_values.iter_mut().enumerate() {
            *target_value = apply_row_f64(
                self.weights.row(target_index),
                source_values,
                self.missing_policy,
            );
        }
        Ok(())
    }
}

pub fn regrid_selected_field_f32(
    field: &SelectedField2D,
    source_grid: &dyn GridGeometry,
    target_grid: &dyn GridGeometry,
    options: RegridOptions,
) -> Result<SelectedField2D, RegridError> {
    let plan = RegridPlan::build(source_grid, target_grid, options)?;
    let values = plan.apply_f32(&field.values)?;
    let target_core_grid = core_grid_from_geometry(target_grid)?;
    let mut output = SelectedField2D::new(
        field.selector,
        field.units.clone(),
        target_core_grid,
        values,
    )
    .map_err(|err| RegridError::InvalidGrid(err.to_string()))?;
    output.projection = target_core_projection(target_grid);
    Ok(output)
}

fn core_grid_from_geometry(grid: &dyn GridGeometry) -> Result<CoreLatLonGrid, RegridError> {
    let mut lat = Vec::with_capacity(grid.len());
    let mut lon = Vec::with_capacity(grid.len());
    for index in 0..grid.len() {
        let point = grid.center_lat_lon(index).ok_or_else(|| {
            RegridError::UnsupportedGeometry(format!(
                "{} does not expose center lat/lon for target index {index}",
                grid.geometry_name()
            ))
        })?;
        lat.push(point.lat_deg as f32);
        lon.push(point.lon_deg as f32);
    }
    CoreLatLonGrid::new(grid.shape(), lat, lon)
        .map_err(|err| RegridError::InvalidGrid(err.to_string()))
}

fn target_core_projection(grid: &dyn GridGeometry) -> Option<rustwx_core::GridProjection> {
    if grid.regular_lat_lon().is_some() {
        return Some(rustwx_core::GridProjection::Geographic);
    }
    match grid.projection()? {
        GridProjection::LatLon => Some(rustwx_core::GridProjection::Geographic),
        GridProjection::LambertConformal {
            standard_parallel_1_deg,
            standard_parallel_2_deg,
            longitude_of_origin_deg,
            ..
        } => Some(rustwx_core::GridProjection::LambertConformal {
            standard_parallel_1_deg: *standard_parallel_1_deg,
            standard_parallel_2_deg: standard_parallel_2_deg.unwrap_or(*standard_parallel_1_deg),
            central_meridian_deg: *longitude_of_origin_deg,
        }),
        GridProjection::PolarStereographic {
            latitude_of_projection_origin_deg,
            straight_vertical_longitude_from_pole_deg,
            standard_parallel_deg,
            ..
        } => Some(rustwx_core::GridProjection::PolarStereographic {
            true_latitude_deg: standard_parallel_deg.unwrap_or(*latitude_of_projection_origin_deg),
            central_meridian_deg: *straight_vertical_longitude_from_pole_deg,
            south_pole_on_projection_plane: *latitude_of_projection_origin_deg < 0.0,
        }),
        GridProjection::Mercator {
            longitude_of_projection_origin_deg,
            standard_parallel_deg,
            ..
        } => Some(rustwx_core::GridProjection::Mercator {
            latitude_of_true_scale_deg: standard_parallel_deg.unwrap_or(0.0),
            central_meridian_deg: *longitude_of_projection_origin_deg,
        }),
        GridProjection::RotatedLatLon { .. } | GridProjection::Geostationary { .. } => None,
    }
}

fn apply_row_f32(
    row: impl Iterator<Item = (usize, f64)>,
    source_values: &[f32],
    missing_policy: MissingPolicy,
) -> f32 {
    let mut sum = 0.0f64;
    let mut valid_weight_sum = 0.0f64;
    let mut saw_weight = false;
    let mut saw_nan = false;
    for (source_index, weight) in row {
        saw_weight = true;
        let value = source_values[source_index];
        if value.is_nan() {
            saw_nan = true;
            if matches!(missing_policy, MissingPolicy::RenormalizeValid) {
                continue;
            }
        } else {
            sum += weight * f64::from(value);
            valid_weight_sum += weight;
        }
    }
    let missing = !saw_weight
        || match missing_policy {
            MissingPolicy::Propagate
            | MissingPolicy::FillValueF32(_)
            | MissingPolicy::FillValueF64(_) => saw_nan,
            MissingPolicy::RenormalizeValid => valid_weight_sum == 0.0,
        };
    if missing {
        return missing_f32(missing_policy);
    }
    match missing_policy {
        MissingPolicy::RenormalizeValid if saw_nan => (sum / valid_weight_sum) as f32,
        _ => sum as f32,
    }
}

fn apply_row_f64(
    row: impl Iterator<Item = (usize, f64)>,
    source_values: &[f64],
    missing_policy: MissingPolicy,
) -> f64 {
    let mut sum = 0.0f64;
    let mut valid_weight_sum = 0.0f64;
    let mut saw_weight = false;
    let mut saw_nan = false;
    for (source_index, weight) in row {
        saw_weight = true;
        let value = source_values[source_index];
        if value.is_nan() {
            saw_nan = true;
            if matches!(missing_policy, MissingPolicy::RenormalizeValid) {
                continue;
            }
        } else {
            sum += weight * value;
            valid_weight_sum += weight;
        }
    }
    let missing = !saw_weight
        || match missing_policy {
            MissingPolicy::Propagate
            | MissingPolicy::FillValueF32(_)
            | MissingPolicy::FillValueF64(_) => saw_nan,
            MissingPolicy::RenormalizeValid => valid_weight_sum == 0.0,
        };
    if missing {
        return missing_f64(missing_policy);
    }
    match missing_policy {
        MissingPolicy::RenormalizeValid if saw_nan => sum / valid_weight_sum,
        _ => sum,
    }
}

fn missing_f32(policy: MissingPolicy) -> f32 {
    match policy {
        MissingPolicy::FillValueF32(value) => value,
        MissingPolicy::FillValueF64(value) => value as f32,
        MissingPolicy::Propagate | MissingPolicy::RenormalizeValid => f32::NAN,
    }
}

fn missing_f64(policy: MissingPolicy) -> f64 {
    match policy {
        MissingPolicy::FillValueF32(value) => f64::from(value),
        MissingPolicy::FillValueF64(value) => value,
        MissingPolicy::Propagate | MissingPolicy::RenormalizeValid => f64::NAN,
    }
}

#[cfg(test)]
mod tests {
    use rustwx_core::{FieldSelector, GridShape, LatLonGrid};

    use crate::{
        GridGeometry, MissingPolicy, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid,
        regrid_selected_field_f32,
    };

    #[test]
    fn missing_policy_propagates_and_renormalizes() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.5, 0.5, 1.0, 1.0, false)
                .unwrap();
        let mut options = RegridOptions::new(RegridMethod::Bilinear);
        options.missing_policy = MissingPolicy::Propagate;
        let plan = RegridPlan::build(&source, &target, options.clone()).unwrap();
        let output = plan.apply_f32(&[1.0, f32::NAN, 3.0, 5.0]).unwrap();
        assert!(output[0].is_nan());

        options.missing_policy = MissingPolicy::RenormalizeValid;
        let plan = RegridPlan::build(&source, &target, options).unwrap();
        let output = plan.apply_f32(&[1.0, f32::NAN, 3.0, 5.0]).unwrap();
        assert!((output[0] - 3.0).abs() < 1.0e-6);
    }

    #[test]
    fn apply_shape_mismatch_is_error() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target = source.clone();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        let err = plan.apply_f32(&[1.0, 2.0]).unwrap_err();
        assert!(matches!(err, crate::RegridError::ShapeMismatch { .. }));
    }

    #[test]
    fn core_lat_lon_adapter_exposes_shape_len_fingerprint_and_centers() {
        let grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![10.0, 10.0, 11.0, 11.0],
            vec![20.0, 21.0, 20.0, 21.0],
        )
        .unwrap();
        assert_eq!(GridGeometry::shape(&grid), GridShape::new(2, 2).unwrap());
        assert_eq!(GridGeometry::len(&grid), 4);
        assert_eq!(grid.center_lat_lon(3).unwrap().lat_deg, 11.0);
        assert_eq!(grid.fingerprint(), grid.fingerprint());
        assert!(grid.regular_lat_lon().is_some());
    }

    #[test]
    fn selected_field_helper_regrids_to_target_core_grid() {
        let source_grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![0.0, 0.0, 1.0, 1.0],
            vec![0.0, 1.0, 0.0, 1.0],
        )
        .unwrap();
        let field = rustwx_core::SelectedField2D::new(
            FieldSelector::surface(rustwx_core::CanonicalField::Temperature),
            "K",
            source_grid.clone(),
            vec![1.0, 2.0, 3.0, 4.0],
        )
        .unwrap();
        let target_grid =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let output = regrid_selected_field_f32(
            &field,
            &source_grid,
            &target_grid,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        assert_eq!(output.values, vec![1.0]);
        assert_eq!(output.grid.shape, GridShape::new(1, 1).unwrap());
    }
}

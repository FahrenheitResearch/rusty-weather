use crate::error::RegridError;
use crate::grid::GridGeometry;
use crate::nearest::{collect_source_points, haversine_distance_km, validate_point};
use crate::weights::{SparseWeightBuilder, SparseWeights};

const COLOCATED_DISTANCE_KM: f64 = 1.0e-9;

pub(crate) fn build_idw_weights(
    source: &dyn GridGeometry,
    target: &dyn GridGeometry,
    k: usize,
    power: f64,
    radius_km: Option<f64>,
) -> Result<SparseWeights, RegridError> {
    if k == 0 {
        return Err(RegridError::InvalidOptions(
            "IDW requires k > 0".to_string(),
        ));
    }
    if !power.is_finite() || power <= 0.0 {
        return Err(RegridError::InvalidOptions(format!(
            "IDW requires finite positive power, got {power}"
        )));
    }
    if let Some(radius) = radius_km {
        if !radius.is_finite() || radius < 0.0 {
            return Err(RegridError::InvalidOptions(format!(
                "IDW radius_km must be finite and non-negative, got {radius}"
            )));
        }
    }
    let source_points = collect_source_points(source)?;
    let mut builder = SparseWeightBuilder::new(target.len(), source.len());
    for target_index in 0..target.len() {
        let Some(target_point) = target.center_lat_lon(target_index) else {
            builder.push_row(&[]);
            continue;
        };
        validate_point(target_point)?;
        let mut distances = Vec::with_capacity(source_points.len());
        let mut colocated = None;
        for &(source_index, source_point) in &source_points {
            let distance = haversine_distance_km(source_point, target_point);
            if distance <= COLOCATED_DISTANCE_KM {
                colocated = Some(source_index);
                break;
            }
            if radius_km.is_none_or(|radius| distance <= radius) {
                distances.push((source_index, distance));
            }
        }
        if let Some(source_index) = colocated {
            builder.push_row(&[(source_index, 1.0)]);
            continue;
        }
        distances.sort_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        distances.truncate(k);
        if distances.is_empty() {
            builder.push_row(&[]);
            continue;
        }
        let raw = distances
            .iter()
            .map(|&(idx, distance)| (idx, 1.0 / distance.powf(power)))
            .collect::<Vec<_>>();
        let sum = raw.iter().map(|(_, weight)| weight).sum::<f64>();
        let row = raw
            .iter()
            .map(|&(idx, weight)| (idx, weight / sum))
            .collect::<Vec<_>>();
        builder.push_row(&row);
    }
    builder.finish()
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use crate::{CurvilinearLatLonGrid, RegridMethod, RegridOptions, RegridPlan};

    #[test]
    fn idw_uses_colocated_point_as_exact_match() {
        let source = CurvilinearLatLonGrid::new(
            GridShape::new(2, 1).unwrap(),
            vec![30.0, 30.0],
            vec![-100.0, -99.0],
            None,
        )
        .unwrap();
        let target = CurvilinearLatLonGrid::new(
            GridShape::new(1, 1).unwrap(),
            vec![30.0],
            vec![-99.0],
            None,
        )
        .unwrap();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::InverseDistance {
                k: 2,
                power: 2.0,
                radius_km: None,
            }),
        )
        .unwrap();
        assert_eq!(plan.apply_f32(&[1.0, 5.0]).unwrap(), vec![5.0]);
    }

    #[test]
    fn idw_radius_can_leave_empty_rows() {
        let source = CurvilinearLatLonGrid::new(
            GridShape::new(1, 1).unwrap(),
            vec![30.0],
            vec![-100.0],
            None,
        )
        .unwrap();
        let target = CurvilinearLatLonGrid::new(
            GridShape::new(1, 1).unwrap(),
            vec![40.0],
            vec![-100.0],
            None,
        )
        .unwrap();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::InverseDistance {
                k: 1,
                power: 2.0,
                radius_km: Some(1.0),
            }),
        )
        .unwrap();
        assert!(plan.apply_f32(&[1.0]).unwrap()[0].is_nan());
    }
}

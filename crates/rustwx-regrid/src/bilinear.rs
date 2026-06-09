use crate::error::RegridError;
use crate::grid::{GridGeometry, RegularLatLonSpec};
use crate::method::RegridMethod;
use crate::nearest::{
    distance_to_valid_axis, longitude_fraction_wrapped, nearest_non_wrapped_fraction,
    validate_point,
};
use crate::weights::{SparseWeightBuilder, SparseWeights};

pub(crate) fn build_bilinear_weights(
    source: &dyn GridGeometry,
    target: &dyn GridGeometry,
    extrapolate: bool,
) -> Result<SparseWeights, RegridError> {
    let Some(spec) = source.regular_lat_lon() else {
        return Err(RegridError::UnsupportedMethodForGeometry {
            method: format!("{:?}", RegridMethod::Bilinear),
            geometry: source.geometry_name().to_string(),
        });
    };
    validate_regular_for_bilinear(spec)?;
    let mut builder = SparseWeightBuilder::new(target.len(), source.len());
    for target_index in 0..target.len() {
        let Some(point) = target.center_lat_lon(target_index) else {
            builder.push_row(&[]);
            continue;
        };
        validate_point(point)?;
        let Some((x0, x1, fx)) = bracket_lon(spec, point.lon_deg, extrapolate) else {
            builder.push_row(&[]);
            continue;
        };
        let Some((y0, y1, fy)) = bracket_axis(
            (point.lat_deg - spec.lat0_deg) / spec.dlat_deg,
            spec.shape.ny,
            extrapolate,
        ) else {
            builder.push_row(&[]);
            continue;
        };
        let nx = spec.shape.nx;
        let w00 = (1.0 - fx) * (1.0 - fy);
        let w10 = fx * (1.0 - fy);
        let w01 = (1.0 - fx) * fy;
        let w11 = fx * fy;
        builder.push_row(&[
            (y0 * nx + x0, w00),
            (y0 * nx + x1, w10),
            (y1 * nx + x0, w01),
            (y1 * nx + x1, w11),
        ]);
    }
    builder.finish()
}

fn validate_regular_for_bilinear(spec: RegularLatLonSpec) -> Result<(), RegridError> {
    if spec.shape.nx < 2 || spec.shape.ny < 2 {
        return Err(RegridError::InvalidGrid(
            "bilinear regridding requires at least 2x2 source points".to_string(),
        ));
    }
    if spec.dlat_deg == 0.0 || spec.dlon_deg == 0.0 {
        return Err(RegridError::InvalidGrid(
            "bilinear regridding requires non-zero source spacing".to_string(),
        ));
    }
    Ok(())
}

fn bracket_axis(value: f64, len: usize, extrapolate: bool) -> Option<(usize, usize, f64)> {
    if len < 2 || !value.is_finite() {
        return None;
    }
    let max = (len - 1) as f64;
    let value = if extrapolate {
        value.clamp(0.0, max)
    } else if !(0.0..=max).contains(&value) {
        return None;
    } else {
        value
    };
    if value >= max {
        return Some((len - 2, len - 1, 1.0));
    }
    let lower = value.floor().max(0.0) as usize;
    Some((lower, lower + 1, value - lower as f64))
}

fn bracket_lon(
    spec: RegularLatLonSpec,
    target_lon_deg: f64,
    extrapolate: bool,
) -> Option<(usize, usize, f64)> {
    if spec.global_lon_wrap {
        let value = longitude_fraction_wrapped(target_lon_deg, spec.lon0_deg, spec.dlon_deg)
            .rem_euclid(spec.shape.nx as f64);
        let x0 = value.floor() as usize % spec.shape.nx;
        let x1 = (x0 + 1) % spec.shape.nx;
        Some((x0, x1, value - value.floor()))
    } else {
        let mut best = nearest_non_wrapped_fraction(
            target_lon_deg,
            spec.lon0_deg,
            spec.dlon_deg,
            spec.shape.nx,
        );
        let mut best_score = distance_to_valid_axis(best, 0.0, (spec.shape.nx - 1) as f64);
        for shift in -2..=2 {
            let shifted = target_lon_deg + 360.0 * f64::from(shift);
            let value = (shifted - spec.lon0_deg) / spec.dlon_deg;
            let score = distance_to_valid_axis(value, 0.0, (spec.shape.nx - 1) as f64);
            if score < best_score {
                best = value;
                best_score = score;
            }
        }
        bracket_axis(best, spec.shape.nx, extrapolate)
    }
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use crate::{MissingPolicy, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid};

    #[test]
    fn bilinear_preserves_constant_values() {
        let source =
            RegularLatLonGrid::new(GridShape::new(3, 3).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(3, 3).unwrap(), 0.25, 0.25, 0.5, 0.5, false)
                .unwrap();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions {
                method: RegridMethod::Bilinear,
                missing_policy: MissingPolicy::Propagate,
                extrapolate: false,
            },
        )
        .unwrap();
        let output = plan.apply_f32(&vec![5.0; source.shape.len()]).unwrap();
        assert!(output.iter().all(|value| (*value - 5.0).abs() < 1.0e-6));
    }

    #[test]
    fn bilinear_interpolates_linear_ramp() {
        let source =
            RegularLatLonGrid::new(GridShape::new(3, 3).unwrap(), 0.0, 0.0, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.5, 0.5, 1.0, 1.0, false)
                .unwrap();
        let values = (0..source.shape.len())
            .map(|idx| {
                let y = idx / source.shape.nx;
                let x = idx % source.shape.nx;
                source.lat_at_y(y) + 2.0 * source.lon_at_x(x)
            })
            .collect::<Vec<_>>();
        let plan = RegridPlan::build(&source, &target, RegridOptions::new(RegridMethod::Bilinear))
            .unwrap();
        let output = plan.apply_f64(&values).unwrap();
        assert!((output[0] - 1.5).abs() < 1.0e-10);
    }

    #[test]
    fn bilinear_wraps_longitude() {
        let source =
            RegularLatLonGrid::new(GridShape::new(4, 2).unwrap(), 0.0, 0.0, 1.0, 90.0, true)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.5, -45.0, 1.0, 1.0, false)
                .unwrap();
        let values = vec![0.0, 90.0, 180.0, 270.0, 10.0, 100.0, 190.0, 280.0];
        let plan = RegridPlan::build(&source, &target, RegridOptions::new(RegridMethod::Bilinear))
            .unwrap();
        let output = plan.apply_f64(&values).unwrap();
        assert!((output[0] - 140.0).abs() < 1.0e-10);
    }
}

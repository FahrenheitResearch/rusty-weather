use crate::error::RegridError;
use crate::grid::{GridGeometry, LatLon, RegularLatLonSpec, normalize_lon_delta};
use crate::weights::{SparseWeightBuilder, SparseWeights};

const EARTH_RADIUS_KM: f64 = 6_371.0;

pub(crate) fn build_nearest_weights(
    source: &dyn GridGeometry,
    target: &dyn GridGeometry,
    max_distance_km: Option<f64>,
) -> Result<SparseWeights, RegridError> {
    if let Some(radius) = max_distance_km {
        if !radius.is_finite() || radius < 0.0 {
            return Err(RegridError::InvalidOptions(format!(
                "nearest max_distance_km must be finite and non-negative, got {radius}"
            )));
        }
    }
    let source_points = collect_source_points(source)?;
    let mut builder = SparseWeightBuilder::new(target.len(), source.len());
    let regular = source.regular_lat_lon();
    for target_index in 0..target.len() {
        let Some(target_point) = target.center_lat_lon(target_index) else {
            builder.push_row(&[]);
            continue;
        };
        validate_point(target_point)?;
        let nearest = if let Some(spec) = regular {
            nearest_regular(spec, target_point)
                .or_else(|| nearest_bruteforce(&source_points, target_point))
        } else {
            nearest_bruteforce(&source_points, target_point)
        };
        let Some((source_index, distance_km)) = nearest else {
            builder.push_row(&[]);
            continue;
        };
        if max_distance_km.is_some_and(|max| distance_km > max) {
            builder.push_row(&[]);
        } else {
            builder.push_row(&[(source_index, 1.0)]);
        }
    }
    builder.finish()
}

pub(crate) fn haversine_distance_km(a: LatLon, b: LatLon) -> f64 {
    let lat1 = a.lat_deg.to_radians();
    let lat2 = b.lat_deg.to_radians();
    let dlat = (b.lat_deg - a.lat_deg).to_radians();
    let dlon = normalize_lon_delta(b.lon_deg - a.lon_deg).to_radians();
    let sin_dlat = (dlat * 0.5).sin();
    let sin_dlon = (dlon * 0.5).sin();
    let h = sin_dlat * sin_dlat + lat1.cos() * lat2.cos() * sin_dlon * sin_dlon;
    2.0 * EARTH_RADIUS_KM * h.sqrt().min(1.0).asin()
}

pub(crate) fn collect_source_points(
    source: &dyn GridGeometry,
) -> Result<Vec<(usize, LatLon)>, RegridError> {
    let mut points = Vec::with_capacity(source.len());
    for index in 0..source.len() {
        let Some(point) = source.center_lat_lon(index) else {
            return Err(RegridError::UnsupportedGeometry(format!(
                "{} does not expose center lat/lon for index {index}",
                source.geometry_name()
            )));
        };
        validate_point(point)?;
        points.push((index, point));
    }
    Ok(points)
}

pub(crate) fn validate_point(point: LatLon) -> Result<(), RegridError> {
    if !point.lat_deg.is_finite()
        || !point.lon_deg.is_finite()
        || !(-90.0..=90.0).contains(&point.lat_deg)
    {
        return Err(RegridError::InvalidGrid(format!(
            "invalid lat/lon point lat={} lon={}",
            point.lat_deg, point.lon_deg
        )));
    }
    Ok(())
}

fn nearest_bruteforce(points: &[(usize, LatLon)], target: LatLon) -> Option<(usize, f64)> {
    points
        .iter()
        .map(|&(idx, point)| (idx, haversine_distance_km(point, target)))
        .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
}

fn nearest_regular(spec: RegularLatLonSpec, target: LatLon) -> Option<(usize, f64)> {
    if spec.shape.nx == 0 || spec.shape.ny == 0 {
        return None;
    }
    let fy = ((target.lat_deg - spec.lat0_deg) / spec.dlat_deg)
        .round()
        .clamp(0.0, (spec.shape.ny - 1) as f64);
    let y = fy as usize;
    let x = if spec.global_lon_wrap {
        let fx = longitude_fraction_wrapped(target.lon_deg, spec.lon0_deg, spec.dlon_deg);
        (fx.round() as isize).rem_euclid(spec.shape.nx as isize) as usize
    } else {
        let fx = nearest_non_wrapped_fraction(
            target.lon_deg,
            spec.lon0_deg,
            spec.dlon_deg,
            spec.shape.nx,
        )
        .round()
        .clamp(0.0, (spec.shape.nx - 1) as f64);
        fx as usize
    };
    let source_point = LatLon::new(
        spec.lat0_deg + y as f64 * spec.dlat_deg,
        spec.lon0_deg + x as f64 * spec.dlon_deg,
    );
    let index = y * spec.shape.nx + x;
    Some((index, haversine_distance_km(source_point, target)))
}

pub(crate) fn longitude_fraction_wrapped(lon_deg: f64, lon0_deg: f64, dlon_deg: f64) -> f64 {
    if dlon_deg > 0.0 {
        super::grid::normalize_lon_positive(lon_deg - lon0_deg) / dlon_deg
    } else {
        -super::grid::normalize_lon_positive(lon0_deg - lon_deg) / dlon_deg
    }
}

pub(crate) fn nearest_non_wrapped_fraction(
    lon_deg: f64,
    lon0_deg: f64,
    dlon_deg: f64,
    len: usize,
) -> f64 {
    let mut best = (lon_deg - lon0_deg) / dlon_deg;
    let mut best_score = distance_to_valid_axis(best, 0.0, (len.saturating_sub(1)) as f64);
    for shift in -2..=2 {
        let shifted = lon_deg + 360.0 * f64::from(shift);
        let fx = (shifted - lon0_deg) / dlon_deg;
        let score = distance_to_valid_axis(fx, 0.0, (len.saturating_sub(1)) as f64);
        if score < best_score {
            best = fx;
            best_score = score;
        }
    }
    best
}

pub(crate) fn distance_to_valid_axis(value: f64, min: f64, max: f64) -> f64 {
    if value < min {
        min - value
    } else if value > max {
        value - max
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use crate::{MissingPolicy, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid};

    #[test]
    fn nearest_identity_matches_input() {
        let grid =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), 30.0, -100.0, 1.0, 1.0, false)
                .unwrap();
        let plan = RegridPlan::build(
            &grid,
            &grid,
            RegridOptions {
                method: RegridMethod::Nearest {
                    max_distance_km: None,
                },
                missing_policy: MissingPolicy::Propagate,
                extrapolate: false,
            },
        )
        .unwrap();
        assert_eq!(
            plan.apply_f32(&[1.0, 2.0, 3.0, 4.0]).unwrap(),
            vec![1.0, 2.0, 3.0, 4.0]
        );
    }

    #[test]
    fn nearest_offset_and_max_distance_work() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 1).unwrap(), 30.0, -100.0, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 30.0, -99.1, 1.0, 1.0, false)
                .unwrap();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Nearest {
                max_distance_km: None,
            }),
        )
        .unwrap();
        assert_eq!(plan.apply_f32(&[1.0, 2.0]).unwrap(), vec![2.0]);

        let mut options = RegridOptions::new(RegridMethod::Nearest {
            max_distance_km: Some(1.0),
        });
        options.missing_policy = MissingPolicy::FillValueF32(-9999.0);
        let plan = RegridPlan::build(&source, &target, options).unwrap();
        assert_eq!(plan.apply_f32(&[1.0, 2.0]).unwrap(), vec![-9999.0]);
    }
}

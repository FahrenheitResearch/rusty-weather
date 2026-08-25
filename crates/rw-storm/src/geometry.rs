use std::cmp::Ordering;

use rw_ops_protocol::{ContourRing, GeoPoint};
use weather_contours::{ContourLimits, contour_levels_with_limits};

use crate::StormError;
use crate::components::{Component, Run, try_push};

const EARTH_MEAN_RADIUS_M: f64 = 6_371_008.8;
const MAX_EXACT_F32_INTEGER: usize = 1 << 24;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Projection {
    Geographic,
    LocalCartesian { origin: GeoPoint },
}

impl Projection {
    pub(crate) fn parameter_value(self) -> &'static str {
        match self {
            Self::Geographic => "geographic_rectilinear_lon_lat",
            Self::LocalCartesian { .. } => "level2_local_cartesian_east_north",
        }
    }

    pub(crate) fn to_geo(self, x: f64, y: f64) -> GeoPoint {
        match self {
            Self::Geographic => GeoPoint {
                latitude: y,
                longitude: x,
            },
            Self::LocalCartesian { origin } => destination_from_local(origin, x, y),
        }
    }
}

pub(crate) struct CellGeometry {
    pub rings: Vec<ContourRing>,
    pub area_km2: f64,
}

struct PlanarRing {
    points: Vec<(f64, f64)>,
    signed_area_twice: f64,
    hole: bool,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn component_geometry(
    values: &[f32],
    nx: usize,
    x_axis: &[f64],
    y_axis: &[f64],
    threshold: f32,
    minimum_valid_dbz: f32,
    maximum_valid_dbz: f32,
    component: &Component,
    runs: &[Run],
    projection: Projection,
) -> Result<CellGeometry, StormError> {
    let x_coordinates = extended_axis(x_axis, component.min_x, component.max_x)?;
    let y_coordinates = extended_axis(y_axis, component.min_y, component.max_y)?;
    let local_nx = x_coordinates.len();
    let local_ny = y_coordinates.len();
    if local_nx > MAX_EXACT_F32_INTEGER {
        return Err(StormError::ContourDimensionPrecision {
            dimension: local_nx,
        });
    }
    if local_ny > MAX_EXACT_F32_INTEGER {
        return Err(StormError::ContourDimensionPrecision {
            dimension: local_ny,
        });
    }

    let local_count = local_nx
        .checked_mul(local_ny)
        .ok_or(StormError::GridSizeOverflow)?;
    let below = threshold - threshold.abs().mul_add(0.001, 1.0);
    let mut local_values = filled_vec(local_count, below, "component contour values")?;

    for local_y in 0..local_ny {
        let Some(global_y) =
            halo_global_index(local_y, component.min_y, component.max_y, y_axis.len())
        else {
            continue;
        };
        let source_row = global_y * nx;
        let destination_row = local_y * local_nx;
        for local_x in 0..local_nx {
            let Some(global_x) =
                halo_global_index(local_x, component.min_x, component.max_x, x_axis.len())
            else {
                continue;
            };
            let value = values[source_row + global_x];
            if value.is_finite()
                && (minimum_valid_dbz..=maximum_valid_dbz).contains(&value)
                && value < threshold
            {
                local_values[destination_row + local_x] = value;
            }
        }
    }

    for &run_index in &component.run_indices {
        let run = runs
            .get(run_index)
            .ok_or(StormError::Invariant("component references a missing run"))?;
        let destination_row = (run.row - component.min_y + 1) * local_nx + 1;
        for global_x in run.start..=run.end {
            let value = values[run.row * nx + global_x];
            local_values[destination_row + global_x - component.min_x] = if value == threshold {
                next_up(value)
            } else {
                value
            };
        }
    }

    let local_x = index_axis(local_nx)?;
    let local_y = index_axis(local_ny)?;
    let limits = dimension_limits(local_nx, local_ny)?;
    let packed = contour_levels_with_limits(
        &local_values,
        local_nx,
        local_ny,
        &local_x,
        &local_y,
        &[threshold],
        limits,
    )?;

    if packed.path_count() == 0 {
        return Err(StormError::MissingComponentBoundary);
    }
    let mut planar_rings = Vec::new();
    planar_rings
        .try_reserve_exact(packed.path_count())
        .map_err(|_| StormError::Allocation {
            resource: "component planar rings",
            requested: packed.path_count(),
        })?;

    for path in 0..packed.path_count() {
        if !packed.path_is_closed(path) {
            return Err(StormError::OpenComponentBoundary);
        }
        let begin = packed.path_offsets[path] as usize;
        let end = packed.path_offsets[path + 1] as usize;
        let packed_points = &packed.vertices[begin * 2..end * 2];
        let mut points = Vec::new();
        points
            .try_reserve_exact(end - begin)
            .map_err(|_| StormError::Allocation {
                resource: "component ring points",
                requested: end - begin,
            })?;
        for point in packed_points.as_chunks::<2>().0 {
            let x = interpolate_axis(&x_coordinates, f64::from(point[0]))?;
            let y = interpolate_axis(&y_coordinates, f64::from(point[1]))?;
            if points.last().copied() != Some((x, y)) {
                points.push((x, y));
            }
        }
        if points.len() < 3 {
            return Err(StormError::Invariant(
                "closed contour has fewer than three distinct points",
            ));
        }
        let signed_area_twice = planar_signed_area_twice(&points);
        if !signed_area_twice.is_finite() || signed_area_twice == 0.0 {
            return Err(StormError::InvalidPolygonArea);
        }
        planar_rings.push(PlanarRing {
            points,
            signed_area_twice,
            hole: false,
        });
    }

    classify_and_canonicalize(&mut planar_rings);
    planar_rings.sort_by(canonical_ring_order);

    let mut rings = Vec::new();
    rings
        .try_reserve_exact(planar_rings.len())
        .map_err(|_| StormError::Allocation {
            resource: "protocol contour rings",
            requested: planar_rings.len(),
        })?;
    let mut area_km2 = 0.0_f64;
    for planar in planar_rings {
        let point_count = planar.points.len().saturating_add(1);
        let mut points = Vec::new();
        points
            .try_reserve_exact(point_count)
            .map_err(|_| StormError::Allocation {
                resource: "geographic contour points",
                requested: point_count,
            })?;
        for (x, y) in planar.points {
            try_push(
                &mut points,
                projection.to_geo(x, y),
                "geographic contour points",
            )?;
        }
        points.push(points[0]);
        let ring_area = spherical_ring_area_km2(&points);
        if planar.hole {
            area_km2 -= ring_area;
        } else {
            area_km2 += ring_area;
        }
        rings.push(ContourRing {
            hole: planar.hole,
            points,
        });
    }

    if !area_km2.is_finite() || area_km2 <= 0.0 {
        return Err(StormError::InvalidPolygonArea);
    }
    Ok(CellGeometry { rings, area_km2 })
}

pub(crate) fn dimension_limits(nx: usize, ny: usize) -> Result<ContourLimits, StormError> {
    if nx < 2 || ny < 2 {
        return Err(StormError::Invariant(
            "component contour window must be at least two by two",
        ));
    }
    let points = nx.checked_mul(ny).ok_or(StormError::GridSizeOverflow)?;
    let horizontal = (nx - 1)
        .checked_mul(ny)
        .ok_or(StormError::GridSizeOverflow)?;
    let vertical = nx.checked_mul(ny - 1).ok_or(StormError::GridSizeOverflow)?;
    let edges = horizontal
        .checked_add(vertical)
        .ok_or(StormError::GridSizeOverflow)?;
    let cells = (nx - 1)
        .checked_mul(ny - 1)
        .ok_or(StormError::GridSizeOverflow)?;
    let band_vertices = edges.checked_mul(2).ok_or(StormError::GridSizeOverflow)?;
    let band_triangles = cells.checked_mul(8).ok_or(StormError::GridSizeOverflow)?;
    let band_clip_operations = cells.checked_mul(16).ok_or(StormError::GridSizeOverflow)?;

    Ok(ContourLimits {
        max_grid_points: points,
        max_grid_edges: edges,
        max_levels: 1,
        max_edge_crossings: edges.min(u32::MAX as usize),
        max_band_vertices: band_vertices,
        max_band_triangles: band_triangles,
        max_band_clip_operations: band_clip_operations,
    })
}

fn extended_axis(axis: &[f64], minimum: usize, maximum: usize) -> Result<Vec<f64>, StormError> {
    let length = maximum
        .checked_sub(minimum)
        .and_then(|span| span.checked_add(3))
        .ok_or(StormError::GridSizeOverflow)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| StormError::Allocation {
            resource: "component extended axis",
            requested: length,
        })?;
    let left = if minimum > 0 {
        axis[minimum - 1]
    } else {
        axis[0] - (axis[1] - axis[0])
    };
    output.push(left);
    output.extend_from_slice(&axis[minimum..=maximum]);
    let right = if maximum + 1 < axis.len() {
        axis[maximum + 1]
    } else {
        axis[maximum] + (axis[maximum] - axis[maximum - 1])
    };
    output.push(right);
    Ok(output)
}

fn halo_global_index(
    local: usize,
    minimum: usize,
    maximum: usize,
    axis_length: usize,
) -> Option<usize> {
    let component_length = maximum - minimum + 1;
    match local {
        0 if minimum > 0 => Some(minimum - 1),
        0 => None,
        value if value <= component_length => Some(minimum + value - 1),
        value if value == component_length + 1 && maximum + 1 < axis_length => Some(maximum + 1),
        _ => None,
    }
}

fn index_axis(length: usize) -> Result<Vec<f32>, StormError> {
    if length > MAX_EXACT_F32_INTEGER {
        return Err(StormError::ContourDimensionPrecision { dimension: length });
    }
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| StormError::Allocation {
            resource: "component contour index axis",
            requested: length,
        })?;
    output.extend((0..length).map(|index| index as f32));
    Ok(output)
}

fn interpolate_axis(axis: &[f64], coordinate: f64) -> Result<f64, StormError> {
    let maximum = (axis.len() - 1) as f64;
    if !coordinate.is_finite() || coordinate < -1.0e-6 || coordinate > maximum + 1.0e-6 {
        return Err(StormError::Invariant(
            "contour coordinate lies outside its component axis",
        ));
    }
    let coordinate = coordinate.clamp(0.0, maximum);
    let left = (coordinate.floor() as usize).min(axis.len() - 1);
    if left + 1 == axis.len() {
        return Ok(axis[left]);
    }
    let fraction = coordinate - left as f64;
    Ok(axis[left] + fraction * (axis[left + 1] - axis[left]))
}

fn classify_and_canonicalize(rings: &mut [PlanarRing]) {
    for index in 0..rings.len() {
        let sample = rings[index].points[0];
        let area = rings[index].signed_area_twice.abs();
        let depth = rings
            .iter()
            .enumerate()
            .filter(|(candidate, ring)| {
                *candidate != index
                    && ring.signed_area_twice.abs() > area
                    && point_in_polygon(sample, &ring.points)
            })
            .count();
        rings[index].hole = depth % 2 == 1;
    }

    for ring in rings {
        let is_counter_clockwise = ring.signed_area_twice > 0.0;
        let wants_counter_clockwise = !ring.hole;
        if is_counter_clockwise != wants_counter_clockwise {
            ring.points.reverse();
            ring.signed_area_twice = -ring.signed_area_twice;
        }
        let first = ring
            .points
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| point_order(**a, **b))
            .map(|(index, _)| index)
            .unwrap_or(0);
        ring.points.rotate_left(first);
    }
}

fn canonical_ring_order(a: &PlanarRing, b: &PlanarRing) -> Ordering {
    a.hole
        .cmp(&b.hole)
        .then_with(|| {
            b.signed_area_twice
                .abs()
                .total_cmp(&a.signed_area_twice.abs())
        })
        .then_with(|| point_order(a.points[0], b.points[0]))
}

fn point_order(a: (f64, f64), b: (f64, f64)) -> Ordering {
    a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1))
}

fn point_in_polygon(point: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    let mut inside = false;
    let mut previous = polygon[polygon.len() - 1];
    for &current in polygon {
        let crosses = (current.1 > point.1) != (previous.1 > point.1);
        if crosses {
            let x_at_y = (previous.0 - current.0) * (point.1 - current.1)
                / (previous.1 - current.1)
                + current.0;
            if point.0 < x_at_y {
                inside = !inside;
            }
        }
        previous = current;
    }
    inside
}

fn planar_signed_area_twice(points: &[(f64, f64)]) -> f64 {
    let mut area = 0.0_f64;
    let mut previous = points[points.len() - 1];
    for &current in points {
        area += previous.0 * current.1 - current.0 * previous.1;
        previous = current;
    }
    area
}

fn spherical_ring_area_km2(points: &[GeoPoint]) -> f64 {
    let mut accumulator = 0.0_f64;
    for edge in points.windows(2) {
        let latitude_a = edge[0].latitude.to_radians();
        let latitude_b = edge[1].latitude.to_radians();
        let mut longitude_delta = (edge[1].longitude - edge[0].longitude).to_radians();
        if longitude_delta > std::f64::consts::PI {
            longitude_delta -= std::f64::consts::TAU;
        } else if longitude_delta < -std::f64::consts::PI {
            longitude_delta += std::f64::consts::TAU;
        }
        accumulator += longitude_delta * (latitude_a.sin() + latitude_b.sin());
    }
    (accumulator.abs() * EARTH_MEAN_RADIUS_M * EARTH_MEAN_RADIUS_M * 0.5) / 1_000_000.0
}

fn destination_from_local(origin: GeoPoint, east_m: f64, north_m: f64) -> GeoPoint {
    let distance = east_m.hypot(north_m);
    if distance == 0.0 {
        return origin;
    }
    let angular_distance = distance / EARTH_MEAN_RADIUS_M;
    let bearing = east_m.atan2(north_m);
    let latitude_1 = origin.latitude.to_radians();
    let longitude_1 = origin.longitude.to_radians();
    let latitude_2 = (latitude_1.sin() * angular_distance.cos()
        + latitude_1.cos() * angular_distance.sin() * bearing.cos())
    .asin();
    let longitude_2 = longitude_1
        + (bearing.sin() * angular_distance.sin() * latitude_1.cos())
            .atan2(angular_distance.cos() - latitude_1.sin() * latitude_2.sin());
    let longitude = (longitude_2.to_degrees() + 180.0).rem_euclid(360.0) - 180.0;
    GeoPoint {
        latitude: latitude_2.to_degrees(),
        longitude,
    }
}

fn filled_vec<T: Clone>(
    length: usize,
    value: T,
    resource: &'static str,
) -> Result<Vec<T>, StormError> {
    let mut output = Vec::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| StormError::Allocation {
            resource,
            requested: length,
        })?;
    output.resize(length, value);
    Ok(output)
}

fn next_up(value: f32) -> f32 {
    if value.is_nan() || value == f32::INFINITY {
        value
    } else if value == -0.0 {
        f32::from_bits(1)
    } else if value >= 0.0 {
        f32::from_bits(value.to_bits() + 1)
    } else {
        f32::from_bits(value.to_bits() - 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_mrms_dimensions_exceed_vendor_default_without_a_new_ceiling() {
        let limits = dimension_limits(7_002, 3_502).unwrap();
        assert_eq!(limits.max_grid_points, 7_002 * 3_502);
        assert!(limits.max_grid_points > ContourLimits::DEFAULT.max_grid_points);
        assert_eq!(limits.max_levels, 1);
    }

    #[test]
    fn cartesian_destination_preserves_origin_and_directions() {
        let origin = GeoPoint {
            latitude: 35.0,
            longitude: -97.0,
        };
        assert_eq!(destination_from_local(origin, 0.0, 0.0), origin);
        assert!(destination_from_local(origin, 1_000.0, 0.0).longitude > origin.longitude);
        assert!(destination_from_local(origin, 0.0, 1_000.0).latitude > origin.latitude);
    }
}

use std::collections::BTreeMap;

use crate::error::RegridError;
use crate::grid::{GridGeometry, RegularLatLonSpec, regular_lat_bounds, regular_lon_bounds};
use crate::method::{ConservativeNormalization, RegridMethod};
use crate::weights::{SparseWeightBuilder, SparseWeights};

const EARTH_RADIUS_M: f64 = 6_371_000.0;

pub(crate) fn build_conservative_weights(
    source: &dyn GridGeometry,
    target: &dyn GridGeometry,
    normalization: ConservativeNormalization,
) -> Result<SparseWeights, RegridError> {
    let Some(source_spec) = source.regular_lat_lon() else {
        return Err(RegridError::UnsupportedMethodForGeometry {
            method: format!("{:?}", RegridMethod::Conservative { normalization }),
            geometry: source.geometry_name().to_string(),
        });
    };
    let Some(target_spec) = target.regular_lat_lon() else {
        return Err(RegridError::UnsupportedMethodForGeometry {
            method: format!("{:?}", RegridMethod::Conservative { normalization }),
            geometry: target.geometry_name().to_string(),
        });
    };
    validate_regular_for_conservative(source_spec, "source")?;
    validate_regular_for_conservative(target_spec, "target")?;

    let source_cells = regular_cells(source_spec);
    let target_cells = regular_cells(target_spec);
    let mut builder = SparseWeightBuilder::new(target.len(), source.len());
    for target_cell in &target_cells {
        let mut covered_area = 0.0;
        let mut overlaps = BTreeMap::<usize, f64>::new();
        for source_cell in &source_cells {
            let lat_south = target_cell.south.max(source_cell.south);
            let lat_north = target_cell.north.min(source_cell.north);
            if lat_north <= lat_south {
                continue;
            }
            let lon_overlap_deg = periodic_lon_overlap_deg(
                (target_cell.west, target_cell.east),
                (source_cell.west, source_cell.east),
            );
            if lon_overlap_deg <= 0.0 {
                continue;
            }
            let overlap_area = spherical_rect_area_m2(lat_south, lat_north, lon_overlap_deg);
            if overlap_area <= 0.0 {
                continue;
            }
            covered_area += overlap_area;
            *overlaps.entry(source_cell.index).or_insert(0.0) += overlap_area;
        }
        if overlaps.is_empty() || covered_area == 0.0 {
            builder.push_row(&[]);
            continue;
        }
        let denom = match normalization {
            ConservativeNormalization::TargetArea => target_cell.area_m2,
            ConservativeNormalization::CoveredArea => covered_area,
        };
        if denom <= 0.0 {
            builder.push_row(&[]);
            continue;
        }
        let row = overlaps
            .iter()
            .map(|(&source_index, &area)| (source_index, area / denom))
            .collect::<Vec<_>>();
        builder.push_row(&row);
    }
    builder.finish()
}

#[derive(Clone, Debug)]
struct Cell {
    index: usize,
    south: f64,
    north: f64,
    west: f64,
    east: f64,
    area_m2: f64,
}

fn validate_regular_for_conservative(
    spec: RegularLatLonSpec,
    role: &str,
) -> Result<(), RegridError> {
    if spec.shape.nx == 0 || spec.shape.ny == 0 {
        return Err(RegridError::InvalidGrid(format!(
            "conservative {role} grid must not be empty"
        )));
    }
    if spec.dlat_deg == 0.0 || spec.dlon_deg == 0.0 {
        return Err(RegridError::InvalidGrid(format!(
            "conservative {role} grid requires non-zero spacing"
        )));
    }
    Ok(())
}

fn regular_cells(spec: RegularLatLonSpec) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(spec.shape.len());
    for y in 0..spec.shape.ny {
        let center_lat = spec.lat0_deg + y as f64 * spec.dlat_deg;
        let (south, north) = regular_lat_bounds(center_lat, spec.dlat_deg);
        for x in 0..spec.shape.nx {
            let center_lon = spec.lon0_deg + x as f64 * spec.dlon_deg;
            let (west, east) = regular_lon_bounds(center_lon, spec.dlon_deg);
            let lon_width = (east - west).abs();
            let area_m2 = spherical_rect_area_m2(south, north, lon_width);
            cells.push(Cell {
                index: y * spec.shape.nx + x,
                south,
                north,
                west,
                east,
                area_m2,
            });
        }
    }
    cells
}

fn periodic_lon_overlap_deg(target: (f64, f64), source: (f64, f64)) -> f64 {
    let (target_west, target_east) = target;
    let (source_west, source_east) = source;
    let mut overlap = 0.0;
    for shift in -2..=2 {
        let shift = 360.0 * f64::from(shift);
        overlap += interval_overlap(
            target_west,
            target_east,
            source_west + shift,
            source_east + shift,
        );
    }
    overlap
}

fn interval_overlap(a0: f64, a1: f64, b0: f64, b1: f64) -> f64 {
    let west = a0.max(b0);
    let east = a1.min(b1);
    (east - west).max(0.0)
}

fn spherical_rect_area_m2(south_deg: f64, north_deg: f64, delta_lon_deg: f64) -> f64 {
    let south = south_deg.to_radians();
    let north = north_deg.to_radians();
    let dlon = delta_lon_deg.abs().to_radians();
    EARTH_RADIUS_M * EARTH_RADIUS_M * dlon * (north.sin() - south.sin()).abs()
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use crate::{
        ConservativeNormalization, RegridMethod, RegridOptions, RegridPlan, RegularLatLonGrid,
    };

    #[test]
    fn conservative_two_by_two_to_one_by_one_area_average() {
        let source =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), -0.5, 0.5, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(1, 1).unwrap(), 0.0, 1.0, 2.0, 2.0, false)
                .unwrap();
        let values = vec![1.0, 2.0, 3.0, 4.0];
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Conservative {
                normalization: ConservativeNormalization::CoveredArea,
            }),
        )
        .unwrap();
        let output = plan.apply_f64(&values).unwrap();
        let expected = plan
            .weights
            .row(0)
            .map(|(idx, weight)| weight * values[idx])
            .sum::<f64>();
        assert!((output[0] - expected).abs() < 1.0e-10);
        assert!((output[0] - 2.5).abs() < 1.0e-10);
    }

    #[test]
    fn conservative_preserves_constant_values() {
        let source =
            RegularLatLonGrid::new(GridShape::new(4, 4).unwrap(), -1.5, 0.5, 1.0, 1.0, false)
                .unwrap();
        let target =
            RegularLatLonGrid::new(GridShape::new(2, 2).unwrap(), -1.0, 1.0, 2.0, 2.0, false)
                .unwrap();
        let plan = RegridPlan::build(
            &source,
            &target,
            RegridOptions::new(RegridMethod::Conservative {
                normalization: ConservativeNormalization::CoveredArea,
            }),
        )
        .unwrap();
        let output = plan.apply_f64(&vec![7.0; source.shape.len()]).unwrap();
        assert!(output.iter().all(|value| (*value - 7.0).abs() < 1.0e-10));
    }
}

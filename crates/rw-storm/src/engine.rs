use std::collections::{BTreeMap, BTreeSet};

use rw_ops_protocol::{
    GeoPoint, STORM_CELL_FRAME_SCHEMA, StormCell, StormCellFrame, StormMethodIdentity,
    StormMethodKind, StormSource,
};

use crate::components::{Component, Components, Run, label_components, try_push};
pub(crate) use crate::geometry::Projection;
use crate::geometry::component_geometry;
use crate::{DETERMINISTIC_METHOD_ID, DETERMINISTIC_METHOD_VERSION, DetectionConfig, StormError};

#[allow(clippy::too_many_arguments)]
pub(crate) fn detect(
    source: StormSource,
    generated_at_unix_ms: i64,
    values: &[f32],
    x_axis: &[f64],
    y_axis: &[f64],
    projection: Projection,
    config: DetectionConfig,
) -> Result<StormCellFrame, StormError> {
    let (nx, ny) = validate_grid(values, x_axis, y_axis, projection)?;
    let grid_point_count = nx.checked_mul(ny).ok_or(StormError::GridSizeOverflow)?;
    let Components {
        runs,
        components,
        missing_sample_count,
    } = label_components(
        values,
        nx,
        ny,
        x_axis,
        y_axis,
        config.threshold_dbz,
        config.minimum_valid_dbz,
        config.maximum_valid_dbz,
        config.connectivity,
    )?;

    let detected_component_count = components.len();
    let mut rejected_by_gate_count = 0_usize;
    let mut rejected_by_area = 0_usize;
    let mut cells = Vec::new();
    let mut identifiers = BTreeSet::new();

    for component in components {
        if component.gate_count < config.minimum_gate_count {
            rejected_by_gate_count += 1;
            continue;
        }
        let geometry = component_geometry(
            values,
            nx,
            x_axis,
            y_axis,
            config.threshold_dbz,
            config.minimum_valid_dbz,
            config.maximum_valid_dbz,
            &component,
            &runs,
            projection,
        )?;
        if geometry.area_km2 < config.minimum_area_km2 {
            rejected_by_area += 1;
            continue;
        }
        let centroid = component_centroid(&component, projection)?;
        let cell_id = stable_cell_id(&source, config, &component, &runs, x_axis, y_axis);
        if !identifiers.insert(cell_id.clone()) {
            return Err(StormError::IdentifierCollision);
        }

        let hole_count = geometry.rings.iter().filter(|ring| ring.hole).count();
        let mut attributes = BTreeMap::new();
        attributes.insert("gate_count".into(), component.gate_count.to_string());
        attributes.insert(
            "minimum_linear_gate_index".into(),
            component.minimum_linear_index.to_string(),
        );
        attributes.insert("grid_column_min".into(), component.min_x.to_string());
        attributes.insert("grid_column_max".into(), component.max_x.to_string());
        attributes.insert("grid_row_min".into(), component.min_y.to_string());
        attributes.insert("grid_row_max".into(), component.max_y.to_string());
        attributes.insert("hole_count".into(), hole_count.to_string());
        attributes.insert(
            "geometry_provenance".into(),
            "derived_reflectivity_threshold_contour".into(),
        );

        try_push(
            &mut cells,
            StormCell {
                cell_id,
                track_id: None,
                centroid,
                rings: geometry.rings,
                area_km2: geometry.area_km2,
                maximum_reflectivity_dbz: Some(f64::from(component.maximum_dbz)),
                echo_top_m: None,
                confidence: None,
                attributes,
            },
            "storm-cell output",
        )?;
    }

    let mut parameters = BTreeMap::new();
    parameters.insert("threshold_dbz".into(), config.threshold_dbz.to_string());
    parameters.insert(
        "minimum_valid_dbz".into(),
        config.minimum_valid_dbz.to_string(),
    );
    parameters.insert(
        "maximum_valid_dbz".into(),
        config.maximum_valid_dbz.to_string(),
    );
    parameters.insert(
        "minimum_gate_count".into(),
        config.minimum_gate_count.to_string(),
    );
    parameters.insert(
        "minimum_area_km2".into(),
        config.minimum_area_km2.to_string(),
    );
    parameters.insert("connectivity".into(), config.connectivity.as_str().into());
    parameters.insert(
        "membership_rule".into(),
        "valid_value_greater_than_or_equal_to_threshold".into(),
    );
    parameters.insert("component_engine".into(), "row_run_union_find_v1".into());
    parameters.insert(
        "contour_engine".into(),
        "weather_contours_0.2.0_oirt".into(),
    );
    parameters.insert(
        "contour_interpolation".into(),
        "bilinear_with_asymptotic_saddle_decider".into(),
    );
    parameters.insert(
        "domain_edge_policy".into(),
        "one_axis_step_extrapolated_below_threshold".into(),
    );
    parameters.insert(
        "missing_data_policy".into(),
        "non_finite_or_out_of_range_excluded_and_contoured_below_threshold".into(),
    );
    parameters.insert(
        "coordinate_geometry".into(),
        projection.parameter_value().into(),
    );
    parameters.insert("grid_point_count".into(), grid_point_count.to_string());
    parameters.insert(
        "detected_component_count".into(),
        detected_component_count.to_string(),
    );
    parameters.insert(
        "rejected_by_gate_count".into(),
        rejected_by_gate_count.to_string(),
    );
    parameters.insert("rejected_by_area".into(), rejected_by_area.to_string());

    let method = StormMethodIdentity {
        method_id: DETERMINISTIC_METHOD_ID.into(),
        method_version: DETERMINISTIC_METHOD_VERSION.into(),
        kind: StormMethodKind::Deterministic,
        display_name: "Deterministic reflectivity cells".into(),
        description: "Connected finite reflectivity gates with separately derived threshold-contour geometry; this is not an authoritative NOAA/NCEI polygon product.".into(),
        upstream_product: None,
        model_id: None,
        model_version: None,
        parameters,
    };

    let mut warnings = Vec::new();
    if missing_sample_count > 0 {
        warnings.push(format!(
            "{missing_sample_count} non-finite or out-of-range grid samples were excluded; enclosed missing regions are represented as contour holes"
        ));
    }
    let frame = StormCellFrame {
        schema: STORM_CELL_FRAME_SCHEMA.into(),
        generated_at_unix_ms,
        source,
        method,
        cells,
        partial: false,
        warnings,
    };
    frame.validate()?;
    Ok(frame)
}

fn validate_grid(
    values: &[f32],
    x_axis: &[f64],
    y_axis: &[f64],
    projection: Projection,
) -> Result<(usize, usize), StormError> {
    validate_axis("x", x_axis)?;
    validate_axis("y", y_axis)?;
    if matches!(projection, Projection::Geographic) {
        validate_axis_range("longitude", x_axis, -180.0, 180.0)?;
        validate_axis_range("latitude", y_axis, -90.0, 90.0)?;
    }
    let expected = x_axis
        .len()
        .checked_mul(y_axis.len())
        .ok_or(StormError::GridSizeOverflow)?;
    if values.len() != expected {
        return Err(StormError::DataLength {
            expected,
            actual: values.len(),
        });
    }
    Ok((x_axis.len(), y_axis.len()))
}

fn validate_axis(name: &'static str, axis: &[f64]) -> Result<(), StormError> {
    if axis.len() < 2 {
        return Err(StormError::AxisTooShort {
            axis: name,
            actual: axis.len(),
        });
    }
    if let Some(index) = axis.iter().position(|coordinate| !coordinate.is_finite()) {
        return Err(StormError::NonFiniteAxis { axis: name, index });
    }
    let ascending = axis[1] > axis[0];
    if axis[1] == axis[0] {
        return Err(StormError::NonMonotonicAxis {
            axis: name,
            left: 0,
            right: 1,
        });
    }
    if let Some((left, _)) = axis
        .windows(2)
        .enumerate()
        .find(|(_, pair)| pair[0] == pair[1] || (pair[1] > pair[0]) != ascending)
    {
        return Err(StormError::NonMonotonicAxis {
            axis: name,
            left,
            right: left + 1,
        });
    }
    Ok(())
}

fn validate_axis_range(
    name: &'static str,
    axis: &[f64],
    minimum: f64,
    maximum: f64,
) -> Result<(), StormError> {
    if let Some(index) = axis
        .iter()
        .position(|coordinate| !(minimum..=maximum).contains(coordinate))
    {
        return Err(StormError::AxisOutOfRange {
            axis: name,
            index,
            minimum,
            maximum,
        });
    }
    Ok(())
}

fn component_centroid(
    component: &Component,
    projection: Projection,
) -> Result<GeoPoint, StormError> {
    let divisor = component.gate_count as f64;
    let centroid = projection.to_geo(component.sum_x / divisor, component.sum_y / divisor);
    centroid.validate()?;
    Ok(centroid)
}

fn stable_cell_id(
    source: &StormSource,
    config: DetectionConfig,
    component: &Component,
    runs: &[Run],
    x_axis: &[f64],
    y_axis: &[f64],
) -> String {
    let mut hash = StableHash128::new();
    hash.write_bytes(DETERMINISTIC_METHOD_ID.as_bytes());
    hash.write_bytes(DETERMINISTIC_METHOD_VERSION.as_bytes());
    match source {
        StormSource::Mrms {
            product,
            valid_at_unix_ms,
            grid_hash,
        } => {
            hash.write_bytes(b"mrms");
            hash.write_bytes(product.as_bytes());
            hash.write_i64(*valid_at_unix_ms);
            hash.write_bytes(grid_hash.as_bytes());
        }
        StormSource::NexradLevel2 {
            site,
            volume_at_unix_ms,
            elevation_degrees_milli,
            moment,
        } => {
            hash.write_bytes(b"nexrad_level2");
            hash.write_bytes(site.as_bytes());
            hash.write_i64(*volume_at_unix_ms);
            hash.write_i64(i64::from(*elevation_degrees_milli));
            hash.write_bytes(moment.as_bytes());
        }
    }
    hash.write_u64(u64::from(config.threshold_dbz.to_bits()));
    hash.write_bytes(config.connectivity.as_str().as_bytes());
    hash.write_u64(component.minimum_linear_index as u64);
    hash.write_u64(component.gate_count as u64);
    hash.write_u64(x_axis[component.min_x].to_bits());
    hash.write_u64(x_axis[component.max_x].to_bits());
    hash.write_u64(y_axis[component.min_y].to_bits());
    hash.write_u64(y_axis[component.max_y].to_bits());
    for &run_index in &component.run_indices {
        let run = &runs[run_index];
        hash.write_u64(run.row as u64);
        hash.write_u64(run.start as u64);
        hash.write_u64(run.end as u64);
    }
    let (first, second) = hash.finish();
    format!("cell:rwdet1:{first:016x}{second:016x}")
}

struct StableHash128 {
    first: u64,
    second: u64,
}

impl StableHash128 {
    const FIRST_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const SECOND_OFFSET: u64 = 0x8422_2325_cbf2_9ce4;
    const FIRST_PRIME: u64 = 0x0000_0100_0000_01b3;
    const SECOND_PRIME: u64 = 0x9e37_79b1_85eb_ca87;

    fn new() -> Self {
        Self {
            first: Self::FIRST_OFFSET,
            second: Self::SECOND_OFFSET,
        }
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        self.write_u64(bytes.len() as u64);
        for &byte in bytes {
            self.first ^= u64::from(byte);
            self.first = self.first.wrapping_mul(Self::FIRST_PRIME);
            self.second ^= u64::from(byte).rotate_left(1);
            self.second = self.second.wrapping_mul(Self::SECOND_PRIME);
            self.second ^= self.second >> 29;
        }
    }

    fn write_i64(&mut self, value: i64) {
        self.write_u64(value as u64);
    }

    fn write_u64(&mut self, value: u64) {
        for byte in value.to_le_bytes() {
            self.first ^= u64::from(byte);
            self.first = self.first.wrapping_mul(Self::FIRST_PRIME);
            self.second ^= u64::from(byte).rotate_left(1);
            self.second = self.second.wrapping_mul(Self::SECOND_PRIME);
            self.second ^= self.second >> 29;
        }
    }

    fn finish(self) -> (u64, u64) {
        (self.first, self.second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GeographicGrid, Level2CartesianGrid, detect_geographic, detect_level2_cartesian};

    fn mrms_source(hash: &str) -> StormSource {
        StormSource::Mrms {
            product: "reflectivity_at_lowest_altitude".into(),
            valid_at_unix_ms: 1_787_000_000_000,
            grid_hash: hash.into(),
        }
    }

    fn geographic_axes(nx: usize, ny: usize) -> (Vec<f64>, Vec<f64>) {
        let x = (0..nx).map(|index| -100.0 + index as f64 * 0.01).collect();
        let y = (0..ny).map(|index| 35.0 + index as f64 * 0.01).collect();
        (x, y)
    }

    fn permissive_config() -> DetectionConfig {
        DetectionConfig {
            minimum_gate_count: 1,
            minimum_area_km2: 0.0,
            ..DetectionConfig::default()
        }
    }

    #[test]
    fn simple_component_emits_valid_derived_polygon() {
        let nx = 7;
        let ny = 7;
        let (x, y) = geographic_axes(nx, ny);
        let mut values = vec![0.0; nx * ny];
        for row in 2..=4 {
            for column in 2..=4 {
                values[row * nx + column] = 50.0;
            }
        }
        let frame = detect_geographic(
            mrms_source("simple-grid"),
            1_787_000_000_100,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            permissive_config(),
        )
        .unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.cells.len(), 1);
        assert_eq!(frame.cells[0].attributes["gate_count"], "9");
        assert_eq!(frame.cells[0].rings.len(), 1);
        assert!(!frame.cells[0].rings[0].hole);
        assert_eq!(frame.cells[0].maximum_reflectivity_dbz, Some(50.0));
        assert_eq!(frame.method.kind, StormMethodKind::Deterministic);
        assert!(frame.method.description.contains("not an authoritative"));
    }

    #[test]
    fn enclosed_missing_data_becomes_an_explicit_hole() {
        let nx = 11;
        let ny = 11;
        let (x, y) = geographic_axes(nx, ny);
        let mut values = vec![0.0; nx * ny];
        for row in 2..=8 {
            for column in 2..=8 {
                values[row * nx + column] = 55.0;
            }
        }
        for row in 4..=6 {
            for column in 4..=6 {
                values[row * nx + column] = f32::NAN;
            }
        }
        let frame = detect_geographic(
            mrms_source("missing-hole"),
            1_787_000_000_100,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            permissive_config(),
        )
        .unwrap();
        assert_eq!(frame.cells.len(), 1);
        assert_eq!(frame.cells[0].rings.len(), 2);
        assert_eq!(
            frame.cells[0].rings.iter().filter(|ring| ring.hole).count(),
            1
        );
        assert!(frame.warnings[0].starts_with("9 non-finite"));
    }

    #[test]
    fn component_with_more_than_64_rings_is_preserved() {
        let nx = 35;
        let ny = 35;
        let (x, y) = geographic_axes(nx, ny);
        let mut values = vec![0.0; nx * ny];
        for row in 2..=32 {
            for column in 2..=32 {
                values[row * nx + column] = 55.0;
            }
        }
        let mut expected_holes = 0;
        for row in (4..=30).step_by(3) {
            for column in (4..=30).step_by(3) {
                values[row * nx + column] = 0.0;
                expected_holes += 1;
            }
        }

        let frame = detect_geographic(
            mrms_source("many-holes"),
            1_787_000_000_100,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            permissive_config(),
        )
        .unwrap();

        frame.validate().unwrap();
        assert_eq!(frame.cells.len(), 1);
        assert_eq!(
            frame.cells[0].rings.iter().filter(|ring| ring.hole).count(),
            expected_holes
        );
        assert!(frame.cells[0].rings.len() > 64);
    }

    #[test]
    fn identifiers_are_stable_across_generation_time() {
        let nx = 5;
        let ny = 5;
        let (x, y) = geographic_axes(nx, ny);
        let mut values = vec![0.0; nx * ny];
        values[2 * nx + 2] = 50.0;
        let make = |generated_at_unix_ms| {
            detect_geographic(
                mrms_source("stable-grid"),
                generated_at_unix_ms,
                GeographicGrid {
                    values_dbz: &values,
                    longitudes: &x,
                    latitudes: &y,
                },
                permissive_config(),
            )
            .unwrap()
        };
        assert_eq!(make(1).cells[0].cell_id, make(2).cells[0].cell_id);
    }

    #[test]
    fn minimum_gate_and_area_filters_are_applied_in_order() {
        let nx = 7;
        let ny = 7;
        let (x, y) = geographic_axes(nx, ny);
        let mut values = vec![0.0; nx * ny];
        values[2 * nx + 2] = 50.0;
        values[4 * nx + 3] = 50.0;
        values[4 * nx + 4] = 50.0;

        let gate_filtered = detect_geographic(
            mrms_source("filter-grid"),
            1,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            DetectionConfig {
                minimum_gate_count: 2,
                minimum_area_km2: 0.0,
                ..DetectionConfig::default()
            },
        )
        .unwrap();
        assert_eq!(gate_filtered.cells.len(), 1);
        assert_eq!(gate_filtered.cells[0].attributes["gate_count"], "2");
        assert_eq!(
            gate_filtered.method.parameters["rejected_by_gate_count"],
            "1"
        );

        let area_filtered = detect_geographic(
            mrms_source("filter-grid"),
            2,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            DetectionConfig {
                minimum_gate_count: 1,
                minimum_area_km2: 1_000_000.0,
                ..DetectionConfig::default()
            },
        )
        .unwrap();
        assert!(area_filtered.cells.is_empty());
        assert_eq!(area_filtered.method.parameters["rejected_by_area"], "2");
    }

    #[test]
    fn invalid_axes_fail_before_geometry() {
        let values = [40.0; 6];
        let error = detect_geographic(
            mrms_source("bad-axis"),
            1,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &[-100.0, -99.0, -99.0],
                latitudes: &[35.0, 36.0],
            },
            permissive_config(),
        )
        .unwrap_err();
        assert!(matches!(error, StormError::NonMonotonicAxis { .. }));
    }

    #[test]
    fn cartesian_entry_point_requires_level2_and_geolocates_output() {
        let axis = [-1_000.0, 0.0, 1_000.0];
        let mut values = [0.0; 9];
        values[4] = 50.0;
        let source = StormSource::NexradLevel2 {
            site: "KTLX".into(),
            volume_at_unix_ms: 1_787_000_000_000,
            elevation_degrees_milli: 500,
            moment: "REF".into(),
        };
        let frame = detect_level2_cartesian(
            source,
            1,
            Level2CartesianGrid {
                values_dbz: &values,
                east_m: &axis,
                north_m: &axis,
                radar_location: GeoPoint {
                    latitude: 35.333,
                    longitude: -97.278,
                },
            },
            permissive_config(),
        )
        .unwrap();
        assert_eq!(frame.cells.len(), 1);
        assert!((frame.cells[0].centroid.latitude - 35.333).abs() < 1.0e-9);
        assert!((frame.cells[0].centroid.longitude + 97.278).abs() < 1.0e-9);
        assert_eq!(
            frame.method.parameters["coordinate_geometry"],
            "level2_local_cartesian_east_north"
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn full_7000_by_3500_grid_has_no_vendor_default_ceiling() {
        let nx = 7_000;
        let ny = 3_500;
        let mut values = vec![0.0_f32; nx * ny];
        for row in 1_748..=1_752 {
            for column in 3_498..=3_502 {
                values[row * nx + column] = 50.0;
            }
        }
        let x: Vec<_> = (0..nx).map(|index| -130.0 + index as f64 * 0.01).collect();
        let y: Vec<_> = (0..ny).map(|index| 20.0 + index as f64 * 0.01).collect();
        let frame = detect_geographic(
            mrms_source("full-mrms-shape"),
            1,
            GeographicGrid {
                values_dbz: &values,
                longitudes: &x,
                latitudes: &y,
            },
            permissive_config(),
        )
        .unwrap();
        assert_eq!(frame.cells.len(), 1);
        assert_eq!(frame.cells[0].attributes["gate_count"], "25");
        assert_eq!(frame.method.parameters["grid_point_count"], "24500000");
    }
}

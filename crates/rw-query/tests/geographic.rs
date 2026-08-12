use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustwx_core::{GridProjection, GridShape, LatLonGrid};
use rw_query::{
    GEOGRAPHIC_WINDOW_RESULT_SCHEMA, GeographicBoundingBox, GeographicFieldValues,
    GeographicVerticalSelection, GeographicWindowLimits, GeographicWindowRequest,
    LongitudeArcSemantics, QueryError, RunSnapshot, query_geographic_window,
    query_geographic_window_with_cancel,
};
use rw_store::RwsExactTime;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, write_hour_from_grid_with_derived_exact,
};

const MODEL: &str = "geographic-model";
const RUN: &str = "20260812T00Z";
const VALID_UNIX: i64 = 1_786_492_800;
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rw-query-geographic-{label}-{}-{serial}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn store(
    label: &str,
    nx: usize,
    ny: usize,
    latitudes: Vec<f32>,
    longitudes: Vec<f32>,
    projection: Option<GridProjection>,
) -> (TestDir, RunSnapshot) {
    let dir = TestDir::new(label);
    let root = dir.0.join("store");
    let grid = LatLonGrid::new(GridShape::new(nx, ny).unwrap(), latitudes, longitudes).unwrap();
    let cells = nx * ny;
    let surface = (0..cells).map(|value| value as f32).collect::<Vec<_>>();
    let pressure_850 = (0..cells)
        .map(|value| 850.0 + value as f32)
        .collect::<Vec<_>>();
    let pressure_500 = (0..cells)
        .map(|value| 500.0 + value as f32)
        .collect::<Vec<_>>();
    write_hour_from_grid_with_derived_exact(
        &root,
        MODEL,
        RUN,
        0,
        RwsExactTime::new(0, VALID_UNIX),
        &grid,
        projection.as_ref(),
        &[],
        &[DerivedFieldInput {
            name: "temperature_2m",
            units: "K",
            values: &surface,
        }],
        &[PressureVolumeInput {
            name: "temperature",
            units: "K",
            selector_template: serde_json::json!({"parameter": "temperature"}),
            levels: vec![(850, &pressure_850), (500, &pressure_500)],
        }],
        "rw-query-geographic-test",
        1_800_000_000,
    )
    .unwrap();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    (dir, snapshot)
}

fn request(
    snapshot: &RunSnapshot,
    variables: &[&str],
    bbox: GeographicBoundingBox,
    vertical: GeographicVerticalSelection,
) -> GeographicWindowRequest {
    GeographicWindowRequest {
        expected_snapshot_id: snapshot.descriptor().snapshot_id.clone(),
        expected_grid_hash: snapshot.descriptor().grid_hash.clone(),
        storage_slot: 0,
        variables: variables.iter().map(|value| (*value).to_string()).collect(),
        bbox,
        vertical,
    }
}

#[test]
fn rectilinear_window_returns_only_the_minimal_cropped_grid_and_projection() {
    let projection = GridProjection::LambertConformal {
        standard_parallel_1_deg: 30.0,
        standard_parallel_2_deg: 60.0,
        central_meridian_deg: -100.0,
    };
    let (_dir, snapshot) = store(
        "rectilinear",
        4,
        3,
        vec![
            40.0, 40.0, 40.0, 40.0, 41.0, 41.0, 41.0, 41.0, 42.0, 42.0, 42.0, 42.0,
        ],
        vec![
            -102.0, -101.0, -100.0, -99.0, -102.0, -101.0, -100.0, -99.0, -102.0, -101.0, -100.0,
            -99.0,
        ],
        Some(projection.clone()),
    );
    let result = query_geographic_window(
        &snapshot,
        &request(
            &snapshot,
            &["temperature_2m"],
            GeographicBoundingBox {
                west_longitude: -101.1,
                south_latitude: 39.9,
                east_longitude: -99.9,
                north_latitude: 41.1,
            },
            GeographicVerticalSelection::Surface2d,
        ),
    )
    .unwrap();

    assert_eq!(result.schema, GEOGRAPHIC_WINDOW_RESULT_SCHEMA);
    assert_eq!(result.run.snapshot_id, snapshot.descriptor().snapshot_id);
    assert_eq!(result.run.grid_hash, snapshot.descriptor().grid_hash);
    assert_eq!((result.envelope.x0, result.envelope.y0), (1, 0));
    assert_eq!((result.envelope.nx, result.envelope.ny), (2, 2));
    assert_eq!(result.latitudes.len(), 4);
    assert_eq!(result.longitudes.len(), 4);
    assert_eq!(result.cell_mask, vec![true; 4]);
    assert!(!result.mask_required);
    assert_eq!(result.projection, Some(projection));
    let GeographicFieldValues::Surface2d { values } = &result.fields[0].data else {
        panic!("surface result");
    };
    assert_eq!(values, &vec![Some(1.0), Some(2.0), Some(5.0), Some(6.0)]);
}

#[test]
fn curvilinear_envelope_masks_cells_outside_the_requested_bbox() {
    let (_dir, snapshot) = store(
        "curvilinear-mask",
        3,
        3,
        vec![0.0, 0.0, 0.0, 1.0, 1.0, 1.0, 2.0, 2.0, 2.0],
        vec![0.0, 1.0, 2.0, 0.6, 1.6, 2.6, 0.0, 1.0, 2.0],
        Some(GridProjection::Geographic),
    );
    let result = query_geographic_window(
        &snapshot,
        &request(
            &snapshot,
            &["temperature_2m"],
            GeographicBoundingBox {
                west_longitude: 0.5,
                south_latitude: -0.5,
                east_longitude: 1.5,
                north_latitude: 2.5,
            },
            GeographicVerticalSelection::Surface2d,
        ),
    )
    .unwrap();
    assert_eq!(
        (result.envelope.x0, result.envelope.nx, result.envelope.ny),
        (0, 2, 3)
    );
    assert_eq!(
        result.cell_mask,
        vec![false, true, true, false, false, true]
    );
    assert!(result.mask_required);
    let GeographicFieldValues::Surface2d { values } = &result.fields[0].data else {
        panic!("surface result");
    };
    assert_eq!(
        values,
        &vec![None, Some(1.0), Some(3.0), None, None, Some(7.0)]
    );
}

#[test]
fn antimeridian_arc_selects_both_sides_without_selecting_greenwich() {
    let (_dir, snapshot) = store(
        "antimeridian",
        4,
        2,
        vec![-1.0, -1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0],
        vec![160.0, 175.0, -175.0, -160.0, 160.0, 175.0, -175.0, -160.0],
        None,
    );
    let result = query_geographic_window(
        &snapshot,
        &request(
            &snapshot,
            &["temperature_2m"],
            GeographicBoundingBox {
                west_longitude: 170.0,
                south_latitude: -2.0,
                east_longitude: -170.0,
                north_latitude: 2.0,
            },
            GeographicVerticalSelection::Surface2d,
        ),
    )
    .unwrap();
    assert_eq!(
        result.longitude_arc,
        LongitudeArcSemantics::CrossesAntimeridian
    );
    assert_eq!((result.envelope.x0, result.envelope.nx), (1, 2));
    assert_eq!(result.cell_mask, vec![true; 4]);
    assert_eq!(
        result.longitudes,
        vec![Some(175.0), Some(-175.0), Some(175.0), Some(-175.0)]
    );
}

#[test]
fn pressure_selection_preserves_explicit_levels_without_reduction() {
    let (_dir, snapshot) = store(
        "pressure",
        2,
        2,
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
        None,
    );
    let result = query_geographic_window(
        &snapshot,
        &request(
            &snapshot,
            &["temperature"],
            GeographicBoundingBox {
                west_longitude: -100.1,
                south_latitude: 39.9,
                east_longitude: -98.9,
                north_latitude: 41.1,
            },
            GeographicVerticalSelection::PressureLevels {
                levels_hpa: vec![500, 850],
            },
        ),
    )
    .unwrap();
    let GeographicFieldValues::PressureLevels { levels_hpa, values } = &result.fields[0].data
    else {
        panic!("pressure result");
    };
    assert_eq!(levels_hpa, &vec![500, 850]);
    assert_eq!(values.len(), 8);
    for (actual, expected) in values[..4].iter().zip([500.0, 501.0, 502.0, 503.0]) {
        assert!((actual.unwrap() - expected).abs() < 0.01);
    }
    for (actual, expected) in values[4..].iter().zip([850.0, 851.0, 852.0, 853.0]) {
        assert!((actual.unwrap() - expected).abs() < 0.01);
    }
}

#[test]
fn edge_no_overlap_caps_identity_and_cancellation_fail_closed() {
    let (_dir, snapshot) = store(
        "fail-closed",
        2,
        2,
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
        None,
    );
    let selected = request(
        &snapshot,
        &["temperature_2m"],
        GeographicBoundingBox {
            west_longitude: -100.1,
            south_latitude: 39.9,
            east_longitude: -98.9,
            north_latitude: 41.1,
        },
        GeographicVerticalSelection::Surface2d,
    );

    let mut wrong_identity = selected.clone();
    wrong_identity.expected_snapshot_id = "0".repeat(64);
    assert!(matches!(
        query_geographic_window(&snapshot, &wrong_identity),
        Err(QueryError::InvalidRequest(_))
    ));
    assert!(matches!(
        query_geographic_window_with_cancel(
            &snapshot,
            &selected,
            GeographicWindowLimits {
                max_native_cells: 3,
                max_output_values: 100,
            },
            || false,
        ),
        Err(QueryError::LimitExceeded {
            what: "geographic envelope cells",
            ..
        })
    ));
    assert!(matches!(
        query_geographic_window_with_cancel(
            &snapshot,
            &selected,
            GeographicWindowLimits {
                max_native_cells: 4,
                max_output_values: 15,
            },
            || false,
        ),
        Err(QueryError::LimitExceeded {
            what: "geographic output values",
            ..
        })
    ));
    assert!(matches!(
        query_geographic_window_with_cancel(
            &snapshot,
            &selected,
            GeographicWindowLimits::default(),
            || true,
        ),
        Err(QueryError::Cancelled)
    ));

    let no_overlap = request(
        &snapshot,
        &["temperature_2m"],
        GeographicBoundingBox {
            west_longitude: 10.0,
            south_latitude: -10.0,
            east_longitude: 20.0,
            north_latitude: 10.0,
        },
        GeographicVerticalSelection::Surface2d,
    );
    assert!(matches!(
        query_geographic_window(&snapshot, &no_overlap),
        Err(QueryError::InvalidRequest(_))
    ));
}

#[test]
fn serialized_wire_contract_is_explicit_and_versioned() {
    let (_dir, snapshot) = store("serialization", 1, 1, vec![0.0], vec![180.0], None);
    let result = query_geographic_window(
        &snapshot,
        &request(
            &snapshot,
            &["temperature_2m"],
            GeographicBoundingBox {
                west_longitude: -180.0,
                south_latitude: -1.0,
                east_longitude: 180.0,
                north_latitude: 1.0,
            },
            GeographicVerticalSelection::Surface2d,
        ),
    )
    .unwrap();
    let json = serde_json::to_value(&result).unwrap();
    assert_eq!(json["schema"], GEOGRAPHIC_WINDOW_RESULT_SCHEMA);
    assert_eq!(json["longitude_arc"], "full_globe");
    assert_eq!(
        json["envelope_semantics"],
        "minimal_native_rectangular_envelope"
    );
    assert_eq!(
        json["cell_inclusion_semantics"],
        "grid_point_center_within_closed_bbox"
    );
    assert_eq!(json["fields"][0]["data"]["kind"], "surface2d");
    let decoded = serde_json::from_value::<rw_query::GeographicWindowResult>(json).unwrap();
    assert_eq!(decoded.schema, result.schema);
    assert_eq!(decoded.run, result.run);
    assert_eq!(decoded.envelope, result.envelope);
    assert_eq!(decoded.fields, result.fields);
}

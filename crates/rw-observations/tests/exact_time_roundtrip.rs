use rustwx_core::{GridProjection, GridShape, LatLonGrid};
use rw_observations::{
    GridPlane, ObservationFamily, ObservationFrame, StoredPlaneRef, read_stored_plane,
    write_observation_frame,
};
use rw_query::RunSnapshot;

fn frame(valid_unix: i64, offset: f32) -> ObservationFrame {
    let grid = LatLonGrid::new(
        GridShape::new(2, 2).unwrap(),
        vec![46.0, 46.0, 45.0, 45.0],
        vec![-123.0, -122.0, -123.0, -122.0],
    )
    .unwrap();
    ObservationFrame {
        family: ObservationFamily::Mrms,
        collection: "test".into(),
        product: "reflectivity".into(),
        valid_unix,
        grid,
        projection: Some(GridProjection::Geographic),
        planes: vec![GridPlane {
            name: "reflectivity".into(),
            units: "dBZ".into(),
            selector: serde_json::json!({"test": true}),
            values: vec![offset, offset + 1.0, offset + 2.0, offset + 3.0],
        }],
        provenance_provider: "test-provider".into(),
        provenance_roles: vec!["radar".into()],
        provenance_products: vec!["reflectivity".into()],
    }
}

#[test]
fn exact_time_observation_frames_append_and_round_trip() {
    let directory = tempfile::tempdir().unwrap();
    let first = write_observation_frame(directory.path(), &frame(1_776_816_000, 10.0)).unwrap();
    let second = write_observation_frame(directory.path(), &frame(1_776_816_300, 20.0)).unwrap();
    assert_eq!(first.run, second.run);
    assert_eq!(first.storage_slot, 0);
    assert_eq!(second.storage_slot, 1);

    let duplicate = write_observation_frame(directory.path(), &frame(1_776_816_300, 20.0)).unwrap();
    assert!(duplicate.duplicate);
    assert_eq!(duplicate.storage_slot, 1);

    let snapshot = RunSnapshot::open(directory.path(), &first.model, &first.run).unwrap();
    assert!(snapshot.descriptor().exact_time_axis);
    assert_eq!(snapshot.time_axis().len(), 2);
    assert_eq!(snapshot.time_axis()[1].valid_unix, 1_776_816_300);

    let plane = read_stored_plane(
        directory.path(),
        &StoredPlaneRef {
            model: second.model,
            run: second.run,
            storage_slot: second.storage_slot,
            variable: "reflectivity".into(),
        },
    )
    .unwrap();
    assert_eq!(plane.values, vec![20.0, 21.0, 22.0, 23.0]);
}

fn mrms_reflectivity_frame(valid_unix: i64, normalized_cells: usize) -> ObservationFrame {
    let mut frame = frame(valid_unix, 10.0);
    frame.collection = "conus".into();
    frame.product = "ReflectivityAtLowestAltitude".into();
    frame.planes[0].name = "mrms_reflectivity_lowest_altitude".into();
    frame.planes[0].selector = serde_json::json!({
        "mrms": {
            "product": "ReflectivityAtLowestAltitude",
            "discipline": 209,
            "parameter_category": 3,
            "parameter_number": 57,
            "parameter_name": "Unknown",
            "level_type": 102,
            "level_value": 500.0,
            "grib_template": 0,
            "missing_value_contract": {
                "missing": -99.0,
                "no_coverage": -999.0,
                "normalized_to": "NaN",
                "normalized_cells": normalized_cells,
            },
        },
    });
    frame
}

#[test]
fn mrms_hours_accept_volatile_provenance_and_keep_reflectivity_display() {
    let directory = tempfile::tempdir().unwrap();
    let first = write_observation_frame(
        directory.path(),
        &mrms_reflectivity_frame(1_776_816_000, 23_539_883),
    )
    .unwrap();
    let second = write_observation_frame(
        directory.path(),
        &mrms_reflectivity_frame(1_776_816_300, 23_541_914),
    )
    .unwrap();
    assert_eq!(first.run, second.run);

    let snapshot = RunSnapshot::open(directory.path(), &first.model, &first.run).unwrap();
    let capabilities = snapshot.variable_capabilities().unwrap();
    let reflectivity = capabilities
        .iter()
        .find(|capability| capability.name == "mrms_reflectivity_lowest_altitude")
        .unwrap();
    assert_eq!(reflectivity.available_samples, 2);
    assert_eq!(reflectivity.units, "dBZ");
    assert_eq!(
        reflectivity
            .selector
            .pointer("/display/palette")
            .and_then(serde_json::Value::as_str),
        Some("reflectivity")
    );
    assert_eq!(
        reflectivity.selector.pointer("/display/preferred_range"),
        Some(&serde_json::json!([-32.0, 95.0]))
    );
}

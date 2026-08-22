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

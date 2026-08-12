use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustwx_core::{GridShape, LatLonGrid};
use rw_query::{
    MissingPolicy, PointSeriesRequest, ProfileRequest, QueryError, QueryLimits, RunSnapshot,
    ScalarTemporalRequest, StoreCatalog, TimeRange, query_point_series, query_profile,
    reduce_scalar_temporal,
};
use rw_store::RwsExactTime;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, write_hour_from_grid_with_derived,
    write_hour_from_grid_with_derived_exact,
};
use rw_store::run::RwsRunManifest;

const MODEL: &str = "fixture-model";
const RUN: &str = "exact-fixture";
const ORIGIN: i64 = 1_700_000_000;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("rw-query-{label}-{}-{serial}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn grid() -> LatLonGrid {
    LatLonGrid::new(
        GridShape::new(2, 2).unwrap(),
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
    )
    .unwrap()
}

fn exact_store() -> (TestDir, PathBuf) {
    let dir = TestDir::new("exact");
    let root = dir.0.join("store");
    let grid = grid();
    let scalar_hours = [
        [5.0, f32::NAN, 1.0, 2.0],
        [3.0, 4.0, 1.0, f32::NAN],
        [3.0, 6.0, 2.0, 8.0],
    ];
    for (slot, lead_seconds) in [(0u16, 0u64), (1, 900), (2, 2_700)] {
        let index = slot as usize;
        let optional = [9.0, 10.0, 11.0, 12.0];
        let mut derived = vec![DerivedFieldInput {
            name: "scalar",
            units: "K",
            values: &scalar_hours[index],
        }];
        if slot == 1 {
            derived.push(DerivedFieldInput {
                name: "optional",
                units: "1",
                values: &optional,
            });
        }
        let pressure_1000 = [280.0 + index as f32, 281.0, 282.0, 283.0];
        let pressure_900 = [270.0 + index as f32, 271.0, 272.0, 273.0];
        let volumes = [PressureVolumeInput {
            name: "temperature",
            units: "K",
            selector_template: serde_json::json!({"parameter": "temperature"}),
            levels: vec![(1000, &pressure_1000), (900, &pressure_900)],
        }];
        write_hour_from_grid_with_derived_exact(
            &root,
            MODEL,
            RUN,
            slot,
            RwsExactTime::new(lead_seconds, ORIGIN + lead_seconds as i64),
            &grid,
            None,
            &[],
            &derived,
            &volumes,
            "rw-query-test",
            1_800_000_000 + u64::from(slot),
        )
        .unwrap();
    }
    (dir, root)
}

fn partial_request(variable: &str) -> ScalarTemporalRequest {
    ScalarTemporalRequest {
        variable: variable.to_string(),
        time: TimeRange::default(),
        missing_policy: MissingPolicy::Partial,
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1.0e-6,
        "got {actual}, expected {expected}"
    );
}

#[test]
fn exact_snapshot_catalog_capabilities_point_and_profile_conform() {
    let (_dir, root) = exact_store();
    let catalog = StoreCatalog::new(&root);
    assert_eq!(catalog.list_models().unwrap()[0].model, MODEL);
    let runs = catalog.list_runs(MODEL).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run.run, RUN);
    assert_eq!(runs[0].variable_count, 3);

    let snapshot = catalog.snapshot(MODEL, RUN).unwrap();
    assert!(snapshot.descriptor().exact_time_axis);
    assert_eq!(snapshot.descriptor().origin_unix, Some(ORIGIN));
    assert_eq!(snapshot.descriptor().sample_count, 3);
    assert_eq!(
        snapshot
            .time_axis()
            .iter()
            .map(|time| (time.storage_slot, time.lead_seconds, time.valid_unix))
            .collect::<Vec<_>>(),
        vec![
            (0, 0, ORIGIN),
            (1, 900, ORIGIN + 900),
            (2, 2_700, ORIGIN + 2_700),
        ]
    );
    let public_time = serde_json::to_value(&snapshot.time_axis()[0]).unwrap();
    assert!(public_time.get("file").is_none());

    let capabilities = snapshot.variable_capabilities().unwrap();
    let optional = capabilities
        .iter()
        .find(|capability| capability.name == "optional")
        .unwrap();
    assert_eq!(optional.available_slots, vec![1]);
    assert_eq!(optional.available_samples, 1);
    assert_close(optional.coverage, 1.0 / 3.0);
    assert_eq!(
        optional.temporal.value_class,
        rw_query::TemporalValueClass::Unknown
    );
    assert!(optional.temporal.requires_manual_semantics);
    let pressure = capabilities
        .iter()
        .find(|capability| capability.name == "temperature")
        .unwrap();
    assert!(pressure.pressure_profile);
    assert!(!pressure.scalar_temporal_reduction);
    assert_eq!(
        pressure.temporal.basis,
        rw_query::TemporalCapabilityBasis::ManualRequired
    );
    assert_eq!(pressure.levels_hpa, vec![1000, 900]);

    let point = query_point_series(
        &snapshot,
        &PointSeriesRequest {
            latitude: 40.0,
            longitude: -99.0,
            variables: vec!["scalar".to_string(), "optional".to_string()],
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap();
    assert_eq!(point.point.x, 1);
    assert_eq!(point.variables[0].values, vec![None, Some(4.0), Some(6.0)]);
    assert_eq!(point.variables[0].available_samples, 2);
    assert_eq!(point.variables[1].values, vec![None, Some(10.0), None]);

    let strict = query_point_series(
        &snapshot,
        &PointSeriesRequest {
            latitude: 40.0,
            longitude: -99.0,
            variables: vec!["scalar".to_string()],
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Strict,
        },
    )
    .unwrap_err();
    assert!(matches!(strict, QueryError::MissingValue { slot: 0, .. }));

    let profile = query_profile(
        &snapshot,
        &ProfileRequest {
            latitude: 40.0,
            longitude: -100.0,
            storage_slot: 1,
            variables: vec!["temperature".to_string()],
        },
    )
    .unwrap();
    assert_eq!(profile.time.valid_unix, ORIGIN + 900);
    assert_eq!(profile.variables[0].levels_hpa, vec![1000, 900]);
    assert_eq!(profile.variables[0].available_levels, 2);
    assert!((profile.variables[0].values[0].unwrap() - 281.0).abs() < 0.05);
    assert!((profile.variables[0].values[1].unwrap() - 271.0).abs() < 0.05);
}

#[test]
fn snapshot_identity_changes_when_same_manifest_content_is_atomically_replaced() {
    let (_dir, root) = exact_store();
    let first = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let first_id = first.descriptor().snapshot_id.clone();
    let manifest_path = root.join(MODEL).join(RUN).join("run.json");
    let original_manifest = RwsRunManifest::load(&manifest_path).unwrap();

    let replacement = [50.0, 51.0, 52.0, 53.0];
    let pressure_1000 = [290.0, 291.0, 292.0, 293.0];
    let pressure_900 = [280.0, 281.0, 282.0, 283.0];
    let volumes = [PressureVolumeInput {
        name: "temperature",
        units: "K",
        selector_template: serde_json::json!({"parameter": "temperature"}),
        levels: vec![(1000, &pressure_1000), (900, &pressure_900)],
    }];
    write_hour_from_grid_with_derived_exact(
        &root,
        MODEL,
        RUN,
        0,
        RwsExactTime::new(0, ORIGIN),
        &grid(),
        None,
        &[],
        &[DerivedFieldInput {
            name: "scalar",
            units: "K",
            values: &replacement,
        }],
        &volumes,
        "rw-query-test",
        1_800_000_000,
    )
    .unwrap();

    // Restore identical manifest JSON. Cache identity must still change
    // because the atomically published file object changed.
    original_manifest.save(&manifest_path).unwrap();

    let second = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    assert_ne!(first_id, second.descriptor().snapshot_id);
    assert!(matches!(
        first.variable_capabilities(),
        Err(QueryError::ManifestInvalidated)
    ));
}

#[test]
fn scalar_reducer_handles_irregular_times_nans_missing_hours_and_ties() {
    let (_dir, root) = exact_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let reduced = reduce_scalar_temporal(&snapshot, &partial_request("scalar")).unwrap();
    assert_eq!(reduced.expected_samples, 3);
    assert_eq!(
        reduced.minimum,
        vec![Some(3.0), Some(4.0), Some(1.0), Some(2.0)]
    );
    assert_eq!(
        reduced.maximum,
        vec![Some(5.0), Some(6.0), Some(2.0), Some(8.0)]
    );
    assert_eq!(
        reduced.range,
        vec![Some(2.0), Some(2.0), Some(1.0), Some(6.0)]
    );
    assert_eq!(reduced.finite_count, vec![3, 2, 3, 2]);
    assert_close(reduced.sample_mean[0].unwrap(), 11.0 / 3.0);
    assert_close(reduced.sample_mean[1].unwrap(), 5.0);
    assert_close(reduced.coverage[1], 2.0 / 3.0);
    assert_eq!(
        reduced.argmin_time_index,
        vec![Some(1), Some(1), Some(0), Some(0),]
    );
    assert_eq!(
        reduced.argmax_time_index,
        vec![Some(0), Some(2), Some(2), Some(2),]
    );

    let strict_error = reduce_scalar_temporal(
        &snapshot,
        &ScalarTemporalRequest {
            variable: "scalar".to_string(),
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Strict,
        },
    )
    .unwrap_err();
    assert!(matches!(
        strict_error,
        QueryError::MissingValue {
            slot: 0,
            x: 1,
            y: 0,
            ..
        }
    ));

    let optional = reduce_scalar_temporal(&snapshot, &partial_request("optional")).unwrap();
    assert_eq!(optional.missing_variable_slots, vec![0, 2]);
    assert_eq!(optional.finite_count, vec![1, 1, 1, 1]);
    assert_close(optional.coverage[0], 1.0 / 3.0);

    let window = reduce_scalar_temporal(
        &snapshot,
        &ScalarTemporalRequest {
            variable: "scalar".to_string(),
            time: TimeRange {
                start_unix: Some(ORIGIN + 900),
                end_unix: Some(ORIGIN + 2_701),
            },
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap();
    assert_eq!(window.axis.len(), 2);
    assert_eq!(window.axis[0].lead_seconds, 900);
    assert_eq!(window.axis[1].lead_seconds, 2_700);
    assert_eq!(window.argmin_time_index[0], Some(0));
    assert_eq!(window.argmax_time_index[0], Some(0));
}

#[test]
fn legacy_axes_are_explicit_utc_and_requests_are_bounded() {
    let dir = TestDir::new("legacy");
    let root = dir.0.join("store");
    let grid = grid();
    let values = [1.0, 2.0, 3.0, 4.0];
    let derived = [DerivedFieldInput {
        name: "scalar",
        units: "1",
        values: &values,
    }];
    write_hour_from_grid_with_derived(
        &root,
        "legacy-model",
        "20240229_06z",
        3,
        &grid,
        None,
        &[],
        &derived,
        &[],
        "rw-query-test",
        1,
    )
    .unwrap();
    let snapshot = RunSnapshot::open(&root, "legacy-model", "20240229_06z").unwrap();
    assert!(!snapshot.descriptor().exact_time_axis);
    assert_eq!(snapshot.descriptor().origin_unix, Some(1_709_186_400));
    assert_eq!(snapshot.time_axis()[0].lead_seconds, 10_800);
    assert_eq!(snapshot.time_axis()[0].valid_unix, 1_709_197_200);

    let limited = RunSnapshot::open_with_limits(
        &root,
        "legacy-model",
        "20240229_06z",
        QueryLimits {
            max_time_points: 0,
            ..QueryLimits::default()
        },
    )
    .err()
    .expect("time-point bound must reject the run");
    assert!(matches!(limited, QueryError::LimitExceeded { .. }));

    let selected_limited = RunSnapshot::open_with_limits(
        &root,
        "legacy-model",
        "20240229_06z",
        QueryLimits {
            max_selected_time_points: 0,
            ..QueryLimits::default()
        },
    )
    .unwrap();
    assert!(matches!(
        selected_limited.select_timepoints(TimeRange::default()),
        Err(QueryError::LimitExceeded { .. })
    ));
    assert!(RunSnapshot::open(&root, "..", "20240229_06z").is_err());

    let invalid_coordinate = query_point_series(
        &snapshot,
        &PointSeriesRequest {
            latitude: f64::NAN,
            longitude: -100.0,
            variables: vec!["scalar".to_string()],
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap_err();
    assert!(matches!(invalid_coordinate, QueryError::InvalidRequest(_)));
    let out_of_range = query_point_series(
        &snapshot,
        &PointSeriesRequest {
            latitude: 91.0,
            longitude: -100.0,
            variables: vec!["scalar".to_string()],
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap_err();
    assert!(matches!(out_of_range, QueryError::InvalidRequest(_)));
    assert_eq!(MissingPolicy::default(), MissingPolicy::Strict);

    write_hour_from_grid_with_derived(
        &root,
        "invalid-model",
        "research-run",
        0,
        &grid,
        None,
        &[],
        &derived,
        &[],
        "rw-query-test",
        1,
    )
    .unwrap();
    let invalid = RunSnapshot::open(&root, "invalid-model", "research-run")
        .err()
        .expect("noncanonical legacy run must be rejected");
    assert!(matches!(invalid, QueryError::InvalidLegacyRunSlug { .. }));
}

#[test]
fn a_manifest_replaced_after_snapshot_creation_is_rejected() {
    let (_dir, root) = exact_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let manifest_path = root.join(MODEL).join(RUN).join("run.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["writer"]["build"] = serde_json::json!("replacement-build");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let error = query_point_series(
        &snapshot,
        &PointSeriesRequest {
            latitude: 40.0,
            longitude: -100.0,
            variables: vec!["scalar".to_string()],
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap_err();
    assert!(matches!(error, QueryError::ManifestInvalidated));
}

#[test]
fn catalog_inventory_is_not_limited_by_per_request_variable_count() {
    let dir = TestDir::new("wide-catalog");
    let root = dir.0.join("store");
    let grid = grid();
    let names = (0..40)
        .map(|index| format!("field_{index:02}"))
        .collect::<Vec<_>>();
    let values = (0..40).map(|index| [index as f32; 4]).collect::<Vec<_>>();
    let fields = names
        .iter()
        .zip(&values)
        .map(|(name, values)| DerivedFieldInput {
            name,
            units: "1",
            values,
        })
        .collect::<Vec<_>>();
    write_hour_from_grid_with_derived_exact(
        &root,
        MODEL,
        "wide-fixture",
        0,
        RwsExactTime::new(0, ORIGIN),
        &grid,
        None,
        &[],
        &fields,
        &[],
        "rw-query-test",
        1_800_000_000,
    )
    .unwrap();

    let snapshot = RunSnapshot::open_with_limits(
        &root,
        MODEL,
        "wide-fixture",
        QueryLimits {
            max_variables: 1,
            ..QueryLimits::default()
        },
    )
    .unwrap();
    assert_eq!(snapshot.variable_capabilities().unwrap().len(), 40);
    let catalog = StoreCatalog::with_limits(
        &root,
        QueryLimits {
            max_variables: 1,
            ..QueryLimits::default()
        },
    );
    assert_eq!(catalog.list_runs(MODEL).unwrap()[0].variable_count, 40);
}

#[test]
fn legacy_scalar_reducer_counts_all_eight_output_grids() {
    let (_dir, root) = exact_store();
    let snapshot = RunSnapshot::open_with_limits(
        &root,
        MODEL,
        RUN,
        QueryLimits {
            max_point_values: 31,
            ..QueryLimits::default()
        },
    )
    .unwrap();

    let error = reduce_scalar_temporal(&snapshot, &partial_request("scalar")).unwrap_err();
    assert!(matches!(
        error,
        QueryError::LimitExceeded {
            what: "scalar reduction output values",
            requested: 32,
            limit: 31,
        }
    ));
}

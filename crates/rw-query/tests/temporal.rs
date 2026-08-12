use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustwx_core::{GridShape, LatLonGrid};
use rw_query::{
    IndexWindow2DRequest, IntervalSupport, MissingPolicy, QueryError, QueryLimits, RunSnapshot,
    SpatialStatsSeriesRequest, TemporalGridRequest, TemporalGridResult, TemporalReducer,
    TemporalReductionLimits, TemporalSemantics, TemporalVerticalSelection, TemporalWindow,
    TimeExpectation, TimeRange, query_spatial_stats_series, query_window_2d, reduce_temporal_grid,
    reduce_temporal_grid_with_cancel, reduce_temporal_grid_with_cancel_and_limits,
    resolve_temporal_window, temporal_semantics_capability,
};
use rw_store::RwsExactTime;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, write_hour_from_grid_with_derived_exact,
};

const MODEL: &str = "temporal-model";
const RUN: &str = "temporal-run";
const ORIGIN: i64 = 1_700_000_000;

const DAY_MODEL: &str = "boundary-model";
const DAY_RUN: &str = "boundary-run";
const DAY_ORIGIN: i64 = 1_704_067_200; // 2024-01-01T00:00:00Z
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rw-query-temporal-{label}-{}-{serial}",
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

fn small_grid() -> LatLonGrid {
    LatLonGrid::new(
        GridShape::new(2, 2).unwrap(),
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
    )
    .unwrap()
}

fn temporal_store() -> (TestDir, PathBuf) {
    let dir = TestDir::new("fixture");
    let root = dir.0.join("store");
    let grid = small_grid();
    for (slot, lead) in [(0u16, 0u64), (1, 900), (2, 2_700)] {
        let index = slot as usize;
        let scalar = [
            [5.0, f32::NAN, 2.0, 2.0],
            [3.0, 4.0, 2.0, 2.0],
            [3.0, 6.0, 2.0, 2.0],
        ][index];
        let amount = [[1.0; 4], [2.0; 4], [3.0; 4]][index];
        let cumulative = [[1.0; 4], [3.0; 4], [0.5; 4]][index];
        let rate = [[10.0; 4], [20.0; 4], [30.0; 4]][index];
        let u = [[0.0; 4], [3.0; 4], [0.0; 4]][index];
        let v = [[1.0; 4], [4.0; 4], [-1.0; 4]][index];
        let angle = [[350.0; 4], [10.0; 4], [10.0; 4]][index];
        let category = [[1.0; 4], [2.0; 4], [2.0; 4]][index];
        let invalid_category = [[1.0; 4], [1.5; 4], [2.0; 4]][index];
        let interval_maximum = [[4.0; 4], [8.0; 4], [6.0; 4]][index];
        let fields = [
            DerivedFieldInput {
                name: "scalar",
                units: "K",
                values: &scalar,
            },
            DerivedFieldInput {
                name: "amount",
                units: "mm",
                values: &amount,
            },
            DerivedFieldInput {
                name: "cumulative",
                units: "mm",
                values: &cumulative,
            },
            DerivedFieldInput {
                name: "rate",
                units: "mm h-1",
                values: &rate,
            },
            DerivedFieldInput {
                name: "u",
                units: "m s-1",
                values: &u,
            },
            DerivedFieldInput {
                name: "v",
                units: "m s-1",
                values: &v,
            },
            DerivedFieldInput {
                name: "angle",
                units: "degree",
                values: &angle,
            },
            DerivedFieldInput {
                name: "category",
                units: "1",
                values: &category,
            },
            DerivedFieldInput {
                name: "invalid_category",
                units: "1",
                values: &invalid_category,
            },
            DerivedFieldInput {
                name: "apcp_1h",
                units: "mm",
                values: &amount,
            },
            DerivedFieldInput {
                name: "wind_direction_10m",
                units: "degrees",
                values: &angle,
            },
            DerivedFieldInput {
                name: "categorical_snow",
                units: "0/1",
                values: &category,
            },
            DerivedFieldInput {
                name: "wind_speed_10m_max_1h",
                units: "m/s",
                values: &interval_maximum,
            },
        ];
        write_hour_from_grid_with_derived_exact(
            &root,
            MODEL,
            RUN,
            slot,
            RwsExactTime::new(lead, ORIGIN + lead as i64),
            &grid,
            None,
            &[],
            &fields,
            &[],
            "rw-query-temporal-test",
            1_800_000_000 + u64::from(slot),
        )
        .unwrap();
    }
    (dir, root)
}

fn pressure_store() -> (TestDir, PathBuf) {
    let dir = TestDir::new("pressure-fixture");
    let root = dir.0.join("store");
    let grid = small_grid();
    for (slot, lead) in [(0u16, 0u64), (1, 3_600)] {
        let dummy = [0.0; 4];
        let t850 = if slot == 0 { [10.0; 4] } else { [14.0; 4] };
        let t500 = if slot == 0 { [1.0; 4] } else { [5.0; 4] };
        let u850 = if slot == 0 { [0.0; 4] } else { [6.0; 4] };
        let v850 = if slot == 0 { [2.0; 4] } else { [8.0; 4] };
        let u500 = if slot == 0 { [3.0; 4] } else { [0.0; 4] };
        let v500 = if slot == 0 { [4.0; 4] } else { [0.0; 4] };
        let fields = [DerivedFieldInput {
            name: "dummy",
            units: "1",
            values: &dummy,
        }];
        let volumes = [
            PressureVolumeInput {
                name: "temperature_iso",
                units: "K",
                selector_template: serde_json::json!({"field":"Temperature"}),
                levels: vec![(850, &t850), (500, &t500)],
            },
            PressureVolumeInput {
                name: "u_iso",
                units: "m/s",
                selector_template: serde_json::json!({"field":"UWind"}),
                levels: vec![(850, &u850), (500, &u500)],
            },
            PressureVolumeInput {
                name: "v_iso",
                units: "m/s",
                selector_template: serde_json::json!({"field":"VWind"}),
                levels: vec![(850, &v850), (500, &v500)],
            },
        ];
        write_hour_from_grid_with_derived_exact(
            &root,
            "pressure-model",
            "pressure-run",
            slot,
            RwsExactTime::new(lead, ORIGIN + lead as i64),
            &grid,
            None,
            &[],
            &fields,
            &volumes,
            "rw-query-pressure-test",
            1_800_200_000 + u64::from(slot),
        )
        .unwrap();
    }
    (dir, root)
}

fn full_day_accumulation_store() -> (TestDir, PathBuf) {
    let dir = TestDir::new("full-day-boundary");
    let root = dir.0.join("store");
    let grid = small_grid();
    for slot in 0u16..=24 {
        let lead_seconds = u64::from(slot) * 3_600;
        let amount = [1.0; 4];
        let cumulative = [f32::from(slot); 4];
        let fields = [
            DerivedFieldInput {
                name: "apcp_1h",
                units: "mm",
                values: &amount,
            },
            DerivedFieldInput {
                name: "cumulative",
                units: "mm",
                values: &cumulative,
            },
        ];
        write_hour_from_grid_with_derived_exact(
            &root,
            DAY_MODEL,
            DAY_RUN,
            slot,
            RwsExactTime::new(lead_seconds, DAY_ORIGIN + lead_seconds as i64),
            &grid,
            None,
            &[],
            &fields,
            &[],
            "rw-query-temporal-boundary-test",
            1_800_100_000 + u64::from(slot),
        )
        .unwrap();
    }
    (dir, root)
}
fn utc_window() -> TemporalWindow {
    TemporalWindow::Utc {
        start_unix: ORIGIN,
        end_unix: ORIGIN + 3_600,
    }
}

fn request(
    variables: &[&str],
    semantics: TemporalSemantics,
    reducer: TemporalReducer,
) -> TemporalGridRequest {
    TemporalGridRequest {
        variables: variables.iter().map(|value| (*value).to_string()).collect(),
        semantics,
        reducer,
        window: utc_window(),
        expectation: TimeExpectation::ManifestAxis,
        missing_policy: MissingPolicy::Partial,
        vertical: None,
    }
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1.0e-6,
        "got {actual}, expected {expected}"
    );
}

#[test]
fn local_days_resolve_to_23_24_and_25_hours() {
    let spring = resolve_temporal_window(&TemporalWindow::LocalDay {
        date: "2024-03-10".to_string(),
        timezone: "America/New_York".to_string(),
    })
    .unwrap();
    let ordinary = resolve_temporal_window(&TemporalWindow::LocalDay {
        date: "2024-02-10".to_string(),
        timezone: "America/New_York".to_string(),
    })
    .unwrap();
    let fall = resolve_temporal_window(&TemporalWindow::LocalDay {
        date: "2024-11-03".to_string(),
        timezone: "America/New_York".to_string(),
    })
    .unwrap();
    assert_eq!(spring.duration_seconds, 23 * 3_600);
    assert_eq!(ordinary.duration_seconds, 24 * 3_600);
    assert_eq!(fall.duration_seconds, 25 * 3_600);
    assert!(
        resolve_temporal_window(&TemporalWindow::LocalDay {
            date: "2024-02-30".to_string(),
            timezone: "America/New_York".to_string(),
        })
        .is_err()
    );
    assert!(
        resolve_temporal_window(&TemporalWindow::LocalDay {
            date: "2024-02-10".to_string(),
            timezone: "Not/A_Zone".to_string(),
        })
        .is_err()
    );
}

#[test]
fn end_stamped_and_cumulative_local_day_include_all_24_intervals() {
    let (_dir, root) = full_day_accumulation_store();
    let snapshot = RunSnapshot::open(&root, DAY_MODEL, DAY_RUN).unwrap();
    let window = TemporalWindow::LocalDay {
        date: "2024-01-01".to_string(),
        timezone: "UTC".to_string(),
    };
    let expectation = TimeExpectation::FixedCadence {
        step_seconds: 3_600,
        anchor_unix: Some(DAY_ORIGIN),
    };

    let interval_request = TemporalGridRequest {
        variables: vec!["apcp_1h".to_string()],
        semantics: TemporalSemantics::IntervalAccumulation {
            support: IntervalSupport::EndsAtValidTime { seconds: 3_600 },
        },
        reducer: TemporalReducer::IntervalSummary,
        window: window.clone(),
        expectation: expectation.clone(),
        missing_policy: MissingPolicy::Strict,
        vertical: None,
    };
    let TemporalGridResult::Interval(interval) =
        reduce_temporal_grid(&snapshot, &interval_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(interval.total[0], Some(24.0));
    assert_eq!(interval.covered_duration_seconds[0], 86_400);
    assert_eq!(interval.metadata.axis.len(), 24);
    assert_eq!(interval.metadata.axis[0].valid_unix, DAY_ORIGIN + 3_600);
    assert_eq!(interval.metadata.axis[23].valid_unix, DAY_ORIGIN + 86_400);
    assert_eq!(interval.metadata.completeness.duration_coverage, 1.0);

    let cumulative_request = TemporalGridRequest {
        variables: vec!["cumulative".to_string()],
        semantics: TemporalSemantics::CumulativeFromOrigin {
            include_first_value: false,
            reset_tolerance: 0.0,
        },
        reducer: TemporalReducer::CumulativeSummary,
        window,
        expectation,
        missing_policy: MissingPolicy::Strict,
        vertical: None,
    };
    let TemporalGridResult::Cumulative(cumulative) =
        reduce_temporal_grid(&snapshot, &cumulative_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(cumulative.total_increment[0], Some(24.0));
    assert_eq!(cumulative.minimum_increment[0], Some(1.0));
    assert_eq!(cumulative.maximum_increment[0], Some(1.0));
    assert_eq!(cumulative.range_increment[0], Some(0.0));
    assert_eq!(cumulative.covered_duration_seconds[0], 86_400);
    assert_eq!(cumulative.metadata.axis.len(), 24);
    assert_eq!(cumulative.metadata.axis[0].valid_unix, DAY_ORIGIN + 3_600);
    assert_eq!(cumulative.metadata.axis[23].valid_unix, DAY_ORIGIN + 86_400);
    assert_eq!(cumulative.metadata.completeness.duration_coverage, 1.0);
}
#[test]
fn instantaneous_reduction_is_exact_time_weighted_and_reports_fixed_cadence_gaps() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let base = request(
        &["scalar"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::ScalarSummary,
    );
    let TemporalGridResult::Scalar(result) = reduce_temporal_grid(&snapshot, &base).unwrap() else {
        panic!("wrong result type")
    };
    assert_eq!(result.minimum[0], Some(3.0));
    assert_eq!(result.maximum[0], Some(5.0));
    assert_eq!(result.argmin_time_index[0], Some(1));
    assert_eq!(result.argmax_time_index[0], Some(0));
    assert_close(result.time_weighted_mean[0].unwrap(), 3.5);
    assert_close(result.time_weighted_mean[1].unwrap(), 14.0 / 3.0);
    assert_eq!(result.covered_duration_seconds[1], 2_700);
    assert_close(result.duration_coverage[1], 0.75);
    assert_eq!(result.metadata.completeness.largest_gap_seconds, 0);
    assert_eq!(
        result.metadata.semantics,
        TemporalSemantics::InstantaneousScalar
    );

    let mut fixed = base.clone();
    fixed.expectation = TimeExpectation::FixedCadence {
        step_seconds: 900,
        anchor_unix: None,
    };
    let TemporalGridResult::Scalar(fixed_result) = reduce_temporal_grid(&snapshot, &fixed).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(fixed_result.metadata.completeness.expected_samples, 4);
    assert_eq!(fixed_result.metadata.completeness.available_samples, 3);
    assert_eq!(
        fixed_result.metadata.completeness.missing_valid_unix,
        vec![ORIGIN + 1_800]
    );
    assert_eq!(
        fixed_result.metadata.completeness.covered_duration_seconds,
        2_700
    );
    assert_eq!(fixed_result.metadata.completeness.largest_gap_seconds, 900);
    assert_close(fixed_result.time_weighted_mean[0].unwrap(), 11.0 / 3.0);

    fixed.missing_policy = MissingPolicy::Strict;
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &fixed),
        Err(QueryError::MissingExpectedTime { valid_unix }) if valid_unix == ORIGIN + 1_800
    ));
}

#[test]
fn interval_cumulative_and_rate_reducers_preserve_physical_meaning() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let support = IntervalSupport::StartsAtValidTime { seconds: 900 };

    let interval_request = request(
        &["amount"],
        TemporalSemantics::IntervalAccumulation { support },
        TemporalReducer::IntervalSummary,
    );
    let TemporalGridResult::Interval(interval) =
        reduce_temporal_grid(&snapshot, &interval_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(interval.total[0], Some(6.0));
    assert_eq!(interval.minimum_interval[0], Some(1.0));
    assert_eq!(interval.maximum_interval[0], Some(3.0));
    assert_eq!(interval.range_interval[0], Some(2.0));
    assert_eq!(interval.covered_duration_seconds[0], 2_700);
    assert_eq!(interval.metadata.completeness.largest_gap_seconds, 900);

    let cumulative_request = request(
        &["cumulative"],
        TemporalSemantics::CumulativeFromOrigin {
            include_first_value: true,
            reset_tolerance: 0.0,
        },
        TemporalReducer::CumulativeSummary,
    );
    let TemporalGridResult::Cumulative(cumulative) =
        reduce_temporal_grid(&snapshot, &cumulative_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(cumulative.total_increment[0], Some(2.5));
    assert_eq!(cumulative.minimum_increment[0], Some(0.5));
    assert_eq!(cumulative.maximum_increment[0], Some(2.0));
    assert_eq!(cumulative.range_increment[0], Some(1.5));
    assert_eq!(cumulative.argmin_time_index[0], Some(1));
    assert_eq!(cumulative.argmax_time_index[0], Some(0));
    assert_eq!(cumulative.reset_count[0], 1);
    assert_eq!(cumulative.covered_duration_seconds[0], 2_700);

    let rate_request = request(
        &["rate"],
        TemporalSemantics::IntervalRate {
            support,
            seconds_per_rate_unit: 3_600.0,
            integral_units: "mm".to_string(),
        },
        TemporalReducer::RateSummary,
    );
    let TemporalGridResult::Rate(rate) = reduce_temporal_grid(&snapshot, &rate_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(rate.integral_units, "mm");
    assert_eq!(rate.minimum_rate[0], Some(10.0));
    assert_eq!(rate.maximum_rate[0], Some(30.0));
    assert_eq!(rate.range_rate[0], Some(20.0));
    assert_close(rate.duration_weighted_mean[0].unwrap(), 20.0);
    assert_close(rate.integral[0].unwrap(), 15.0);
}

#[test]
fn vector_circular_and_categorical_reducers_are_duration_weighted() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();

    let vector_request = request(
        &["u", "v"],
        TemporalSemantics::VectorComponents,
        TemporalReducer::VectorSummary,
    );
    let TemporalGridResult::Vector(vector) =
        reduce_temporal_grid(&snapshot, &vector_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(vector.minimum_speed[0], Some(1.0));
    assert_eq!(vector.maximum_speed[0], Some(5.0));
    assert_eq!(vector.range_speed[0], Some(4.0));
    assert_close(vector.time_weighted_mean_speed[0].unwrap(), 3.0);
    assert_close(vector.vector_mean_u[0].unwrap(), 1.5);
    assert_close(vector.vector_mean_v[0].unwrap(), 2.0);
    assert_close(vector.vector_mean_speed[0].unwrap(), 2.5);
    assert_close(
        vector.vector_mean_direction_toward_degrees[0].unwrap(),
        36.869_897_645_844_02,
    );
    assert_eq!(vector.argmin_time_index[0], Some(0));
    assert_eq!(vector.argmax_time_index[0], Some(1));

    let circular_request = request(
        &["angle"],
        TemporalSemantics::CircularDegrees,
        TemporalReducer::CircularMean,
    );
    let TemporalGridResult::Circular(circular) =
        reduce_temporal_grid(&snapshot, &circular_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert!((circular.mean_degrees[0].unwrap() - 5.038_368_773).abs() < 1.0e-6);
    assert!(circular.resultant_length[0].unwrap() > 0.98);

    let categorical_request = request(
        &["category"],
        TemporalSemantics::Categorical,
        TemporalReducer::CategoricalSummary,
    );
    let TemporalGridResult::Categorical(categorical) =
        reduce_temporal_grid(&snapshot, &categorical_request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(categorical.mode[0], Some(2));
    assert_eq!(categorical.mode_duration_seconds[0], 2_700);
    assert_eq!(categorical.transitions[0], 1);
    assert_eq!(
        categorical.category_durations[0]
            .iter()
            .map(|entry| (entry.category, entry.duration_seconds))
            .collect::<Vec<_>>(),
        vec![(1, 900), (2, 2_700)]
    );
}

#[test]
fn invalid_semantics_categories_and_cancellation_are_rejected() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let mismatch = request(
        &["scalar"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::CircularMean,
    );
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &mismatch),
        Err(QueryError::InvalidRequest(_))
    ));
    let unknown = request(
        &["scalar"],
        TemporalSemantics::Unknown,
        TemporalReducer::ScalarSummary,
    );
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &unknown),
        Err(QueryError::InvalidRequest(_))
    ));
    let capability = temporal_semantics_capability(TemporalSemantics::Unknown);
    assert!(!capability.reducible);
    assert!(capability.supported_reducers.is_empty());

    let invalid_category = request(
        &["invalid_category"],
        TemporalSemantics::Categorical,
        TemporalReducer::CategoricalSummary,
    );
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &invalid_category),
        Err(QueryError::InvalidCategory { slot: 1, .. })
    ));

    let scalar = request(
        &["scalar"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::ScalarSummary,
    );
    assert!(matches!(
        reduce_temporal_grid_with_cancel(&snapshot, &scalar, || true),
        Err(QueryError::Cancelled)
    ));
}

#[test]
fn trusted_capabilities_reject_mislabeled_scalar_reducers() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    for variable in [
        "apcp_1h",
        "wind_direction_10m",
        "categorical_snow",
        "wind_speed_10m_max_1h",
    ] {
        let mislabeled = request(
            &[variable],
            TemporalSemantics::InstantaneousScalar,
            TemporalReducer::ScalarSummary,
        );
        assert!(matches!(
            reduce_temporal_grid(&snapshot, &mislabeled),
            Err(QueryError::InvalidRequest(message)) if message.contains("trusted")
        ));
    }
}

#[test]
fn fixed_window_maxima_use_explicit_extremum_semantics_and_union_coverage() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let request = TemporalGridRequest {
        variables: vec!["wind_speed_10m_max_1h".to_string()],
        semantics: TemporalSemantics::IntervalMaximum {
            support: IntervalSupport::EndsAtValidTime { seconds: 3_600 },
        },
        reducer: TemporalReducer::IntervalMaximumSummary,
        // All three trailing one-hour supports are wholly contained. They
        // overlap, so coverage must be their union rather than 3 * 3600 s.
        window: TemporalWindow::Utc {
            start_unix: ORIGIN - 3_600,
            end_unix: ORIGIN + 2_700,
        },
        expectation: TimeExpectation::ManifestAxis,
        missing_policy: MissingPolicy::Partial,
        vertical: None,
    };
    let TemporalGridResult::IntervalMaximum(result) =
        reduce_temporal_grid(&snapshot, &request).unwrap()
    else {
        panic!("wrong reducer result")
    };
    assert_eq!(result.minimum_of_interval_maxima, vec![Some(4.0); 4]);
    assert_eq!(result.maximum_of_interval_maxima, vec![Some(8.0); 4]);
    assert_eq!(result.range_of_interval_maxima, vec![Some(4.0); 4]);
    assert_eq!(result.argmin_interval_maximum_time_index, vec![Some(0); 4]);
    assert_eq!(result.argmax_interval_maximum_time_index, vec![Some(1); 4]);
    assert_eq!(result.finite_interval_maximum_count, vec![3; 4]);
    assert_eq!(result.covered_duration_seconds, vec![6_300; 4]);
    assert_eq!(result.duration_coverage, vec![1.0; 4]);
}

#[test]
fn temporal_output_budget_counts_all_fixed_and_dynamic_values() {
    let (_dir, root) = temporal_store();
    let scalar_limits = QueryLimits {
        max_temporal_output_values: 35,
        ..QueryLimits::default()
    };
    let scalar_snapshot = RunSnapshot::open_with_limits(&root, MODEL, RUN, scalar_limits).unwrap();
    let scalar = request(
        &["scalar"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::ScalarSummary,
    );
    assert!(matches!(
        reduce_temporal_grid(&scalar_snapshot, &scalar),
        Err(QueryError::LimitExceeded {
            what: "temporal output values",
            requested: 36,
            limit: 35,
        })
    ));

    let vector_limits = QueryLimits {
        max_temporal_output_values: 51,
        ..QueryLimits::default()
    };
    let vector_snapshot = RunSnapshot::open_with_limits(&root, MODEL, RUN, vector_limits).unwrap();
    let vector = request(
        &["u", "v"],
        TemporalSemantics::VectorComponents,
        TemporalReducer::VectorSummary,
    );
    assert!(matches!(
        reduce_temporal_grid(&vector_snapshot, &vector),
        Err(QueryError::LimitExceeded {
            what: "temporal output values",
            requested: 52,
            limit: 51,
        })
    ));

    let ten_array_limits = QueryLimits {
        max_temporal_output_values: 39,
        ..QueryLimits::default()
    };
    for request in [
        request(
            &["amount"],
            TemporalSemantics::IntervalAccumulation {
                support: IntervalSupport::StartsAtValidTime { seconds: 900 },
            },
            TemporalReducer::IntervalSummary,
        ),
        request(
            &["cumulative"],
            TemporalSemantics::CumulativeFromOrigin {
                include_first_value: true,
                reset_tolerance: 0.0,
            },
            TemporalReducer::CumulativeSummary,
        ),
        request(
            &["rate"],
            TemporalSemantics::IntervalRate {
                support: IntervalSupport::StartsAtValidTime { seconds: 900 },
                seconds_per_rate_unit: 3_600.0,
                integral_units: "mm".to_string(),
            },
            TemporalReducer::RateSummary,
        ),
    ] {
        let snapshot =
            RunSnapshot::open_with_limits(&root, MODEL, RUN, ten_array_limits.clone()).unwrap();
        assert!(matches!(
            reduce_temporal_grid(&snapshot, &request),
            Err(QueryError::LimitExceeded {
                what: "temporal output values",
                requested: 40,
                limit: 39,
            })
        ));
    }

    let categorical_limits = QueryLimits {
        max_temporal_output_values: 31,
        ..QueryLimits::default()
    };
    let categorical_snapshot =
        RunSnapshot::open_with_limits(&root, MODEL, RUN, categorical_limits).unwrap();
    let categorical = request(
        &["category"],
        TemporalSemantics::Categorical,
        TemporalReducer::CategoricalSummary,
    );
    assert!(matches!(
        reduce_temporal_grid(&categorical_snapshot, &categorical),
        Err(QueryError::LimitExceeded {
            what: "categorical result entries",
            requested: 4,
            limit: 3,
        })
    ));
}
#[test]
fn spatial_stats_and_native_index_windows_are_bounded_and_exact() {
    let (_dir, root) = temporal_store();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    let stats = query_spatial_stats_series(
        &snapshot,
        &SpatialStatsSeriesRequest {
            variable: "scalar".to_string(),
            time: TimeRange::default(),
            missing_policy: MissingPolicy::Partial,
        },
    )
    .unwrap();
    assert_eq!(stats.samples.len(), 3);
    assert_eq!(stats.samples[0].minimum, Some(2.0));
    assert_eq!(stats.samples[0].maximum, Some(5.0));
    assert_eq!(stats.samples[0].finite_count, 3);
    assert_eq!(stats.samples[0].missing_count, 1);

    let window = query_window_2d(
        &snapshot,
        &IndexWindow2DRequest {
            storage_slot: 1,
            variable: "scalar".to_string(),
            x0: 1,
            y0: 0,
            x1: 2,
            y1: 2,
        },
    )
    .unwrap();
    assert_eq!((window.x0, window.y0, window.nx, window.ny), (1, 0, 1, 2));
    assert_eq!(window.values, vec![Some(4.0), Some(2.0)]);

    let missing_window = query_window_2d(
        &snapshot,
        &IndexWindow2DRequest {
            storage_slot: 0,
            variable: "scalar".to_string(),
            x0: 1,
            y0: 0,
            x1: 2,
            y1: 1,
        },
    )
    .unwrap();
    assert_eq!(missing_window.values, vec![None]);
    assert_eq!(
        serde_json::to_value(&missing_window).unwrap()["values"],
        serde_json::json!([null])
    );
    assert!(
        query_window_2d(
            &snapshot,
            &IndexWindow2DRequest {
                storage_slot: 1,
                variable: "scalar".to_string(),
                x0: 0,
                y0: 0,
                x1: 3,
                y1: 1,
            },
        )
        .is_err()
    );
}

#[test]
fn temporal_reduction_crosses_store_tile_edges_without_reordering_cells() {
    let dir = TestDir::new("tile-seam");
    let root = dir.0.join("store");
    let nx = 257usize;
    let ny = 2usize;
    let mut lat = Vec::with_capacity(nx * ny);
    let mut lon = Vec::with_capacity(nx * ny);
    for y in 0..ny {
        for x in 0..nx {
            lat.push(30.0 + y as f32 * 0.1);
            lon.push(-110.0 + x as f32 * 0.01);
        }
    }
    let grid = LatLonGrid::new(GridShape::new(nx, ny).unwrap(), lat, lon).unwrap();
    for (slot, lead, seam) in [(0u16, 0u64, 10.0f32), (1, 60, -5.0)] {
        let mut values = vec![slot as f32; nx * ny];
        values[256] = seam;
        let fields = [DerivedFieldInput {
            name: "seam",
            units: "1",
            values: &values,
        }];
        write_hour_from_grid_with_derived_exact(
            &root,
            "tile-model",
            "tile-run",
            slot,
            RwsExactTime::new(lead, ORIGIN + lead as i64),
            &grid,
            None,
            &[],
            &fields,
            &[],
            "rw-query-temporal-test",
            1_800_000_000 + u64::from(slot),
        )
        .unwrap();
    }
    let snapshot = RunSnapshot::open(&root, "tile-model", "tile-run").unwrap();
    let request = TemporalGridRequest {
        variables: vec!["seam".to_string()],
        semantics: TemporalSemantics::InstantaneousScalar,
        reducer: TemporalReducer::ScalarSummary,
        window: TemporalWindow::Utc {
            start_unix: ORIGIN,
            end_unix: ORIGIN + 120,
        },
        expectation: TimeExpectation::ManifestAxis,
        missing_policy: MissingPolicy::Partial,
        vertical: None,
    };
    let TemporalGridResult::Scalar(result) = reduce_temporal_grid(&snapshot, &request).unwrap()
    else {
        panic!("wrong result type")
    };
    assert_eq!(result.minimum.len(), nx * ny);
    assert_eq!(result.minimum[256], Some(-5.0));
    assert_eq!(result.maximum[256], Some(10.0));
    assert_close(result.time_weighted_mean[256].unwrap(), 2.5);
    assert_eq!(result.minimum[257], Some(0.0));
    assert_eq!(result.maximum[257], Some(1.0));
}

#[test]
fn pressure_scalar_and_vector_reduce_each_requested_level_in_level_y_x_order() {
    let (_dir, root) = pressure_store();
    let snapshot = RunSnapshot::open(&root, "pressure-model", "pressure-run").unwrap();
    let mut scalar = request(
        &["temperature_iso"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::ScalarSummary,
    );
    scalar.window = TemporalWindow::Utc {
        start_unix: ORIGIN,
        end_unix: ORIGIN + 7_200,
    };
    scalar.vertical = Some(TemporalVerticalSelection::PressureLevels {
        levels_hpa: vec![500, 850],
    });
    let TemporalGridResult::Scalar(result) = reduce_temporal_grid(&snapshot, &scalar).unwrap()
    else {
        panic!("wrong scalar result")
    };
    assert_eq!(result.metadata.levels_hpa, [500, 850]);
    assert_eq!(result.metadata.shape, Some([2, 2, 2]));
    assert_eq!(
        serde_json::to_value(&result.metadata).unwrap()["layout"],
        "level_y_x"
    );
    assert_eq!(
        result.minimum,
        [vec![Some(1.0); 4], vec![Some(10.0); 4]].concat()
    );
    assert_eq!(
        result.maximum,
        [vec![Some(5.0); 4], vec![Some(14.0); 4]].concat()
    );

    let mut vector = scalar.clone();
    vector.variables = vec!["u_iso".into(), "v_iso".into()];
    vector.semantics = TemporalSemantics::VectorComponents;
    vector.reducer = TemporalReducer::VectorSummary;
    let TemporalGridResult::Vector(result) = reduce_temporal_grid(&snapshot, &vector).unwrap()
    else {
        panic!("wrong vector result")
    };
    assert_eq!(
        result.minimum_speed,
        [vec![Some(0.0); 4], vec![Some(2.0); 4]].concat()
    );
    assert_eq!(
        result.maximum_speed,
        [vec![Some(5.0); 4], vec![Some(10.0); 4]].concat()
    );
    assert_eq!(
        result.range_speed,
        [vec![Some(5.0); 4], vec![Some(8.0); 4]].concat()
    );
}

#[test]
fn pressure_selection_is_explicit_unique_present_and_budgeted_by_level() {
    let (_dir, root) = pressure_store();
    let snapshot = RunSnapshot::open(&root, "pressure-model", "pressure-run").unwrap();
    let mut request = request(
        &["temperature_iso"],
        TemporalSemantics::InstantaneousScalar,
        TemporalReducer::ScalarSummary,
    );
    request.vertical = Some(TemporalVerticalSelection::PressureLevels {
        levels_hpa: vec![850, 850],
    });
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &request),
        Err(QueryError::InvalidRequest(message)) if message.contains("unique")
    ));
    request.vertical = Some(TemporalVerticalSelection::PressureLevels {
        levels_hpa: vec![700],
    });
    assert!(matches!(
        reduce_temporal_grid(&snapshot, &request),
        Err(QueryError::InvalidRequest(message)) if message.contains("700")
    ));
    request.vertical = Some(TemporalVerticalSelection::PressureLevels {
        levels_hpa: vec![850, 500],
    });
    assert!(matches!(
        reduce_temporal_grid_with_cancel_and_limits(
            &snapshot,
            &request,
            TemporalReductionLimits {
                max_reduction_cells: 7,
                max_output_values: usize::MAX,
            },
            || false,
        ),
        Err(QueryError::LimitExceeded {
            what: "temporal reduction cells",
            requested: 8,
            limit: 7
        })
    ));
}

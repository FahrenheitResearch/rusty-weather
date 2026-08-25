//! Full-plane reads used by the binary plane routes.
//!
//! These exercise `RunSnapshot`'s snapshot-revalidated plane readers rather
//! than the raw `HourReader`, because the routes must never reconstruct hour
//! paths themselves: doing so reopens the time-of-check/time-of-use gap around
//! atomically replaced runs that `RunSnapshot` exists to close.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use rustwx_core::{GridShape, LatLonGrid};
use rw_query::{QueryError, RunSnapshot};
use rw_store::RwsExactTime;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, write_hour_from_grid_with_derived_exact,
};

const MODEL: &str = "plane-model";
const RUN: &str = "20260812T00Z";
const VALID_UNIX: i64 = 1_786_492_800;
static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let serial = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "rw-query-plane-{label}-{}-{serial}",
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

fn store(label: &str) -> (TestDir, RunSnapshot) {
    let dir = TestDir::new(label);
    let root = dir.0.join("store");
    let grid = LatLonGrid::new(
        GridShape::new(2, 2).unwrap(),
        vec![40.0, 40.0, 41.0, 41.0],
        vec![-100.0, -99.0, -100.0, -99.0],
    )
    .unwrap();
    let surface = [1.0_f32, 2.0, 3.0, 4.0];
    let pressure_850 = [850.0_f32, 851.0, 852.0, 853.0];
    let pressure_500 = [500.0_f32, 501.0, 502.0, 503.0];
    write_hour_from_grid_with_derived_exact(
        &root,
        MODEL,
        RUN,
        0,
        RwsExactTime::new(0, VALID_UNIX),
        &grid,
        None,
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
        "rw-query-plane-test",
        1_800_000_000,
    )
    .unwrap();
    let snapshot = RunSnapshot::open(&root, MODEL, RUN).unwrap();
    (dir, snapshot)
}

/// Pressure volumes are stored as `zstd1_affine_i16`, so a decoded level is a
/// dequantized approximation of the ingested field, not a bit-exact copy. The
/// reader must not pretend otherwise, and it must carry the exact stored codec
/// name so callers can publish that fact rather than imply lossless f32.
#[test]
fn one_exact_pressure_level_reads_as_a_full_native_dequantized_plane() {
    let (_dir, snapshot) = store("level");

    let plane = snapshot
        .read_pressure_level_2d(0, "temperature", 850)
        .unwrap();
    assert_eq!(plane.level_hpa, 850);
    assert_eq!(plane.time.valid_unix, VALID_UNIX);
    assert_eq!(plane.metadata.name, "temperature");
    assert_eq!(plane.metadata.units, "K");
    assert_eq!(plane.metadata.kind, "pressure3d");
    assert_eq!(plane.metadata.codec, "zstd1_affine_i16");
    assert_eq!(plane.metadata.levels_hpa, vec![850, 500]);
    assert_eq!(plane.values.len(), 4);
    for (value, expected) in plane.values.iter().zip([850.0, 851.0, 852.0, 853.0]) {
        assert!(
            (value - expected).abs() < 0.01,
            "{value} is not within affine-i16 quantization of {expected}"
        );
    }

    let upper = snapshot
        .read_pressure_level_2d(0, "temperature", 500)
        .unwrap();
    for (value, expected) in upper.values.iter().zip([500.0, 501.0, 502.0, 503.0]) {
        assert!(
            (value - expected).abs() < 0.01,
            "{value} is not within affine-i16 quantization of {expected}"
        );
    }

    // Surface fields are the lossless half of the same contract.
    let surface = snapshot.read_surface_2d(0, "temperature_2m").unwrap();
    assert_eq!(surface.metadata.codec, "zstd1_f32");
    assert_eq!(surface.values, vec![1.0, 2.0, 3.0, 4.0]);
}

/// A level the run does not store is a missing sub-resource, not an internal
/// failure: it must stay in the same explicit `Unknown*` family as an unknown
/// slot or variable so the HTTP layer can answer 404 instead of 500.
#[test]
fn an_unstored_pressure_level_fails_explicitly_rather_than_as_a_store_error() {
    let (_dir, snapshot) = store("missing-level");

    let error = snapshot
        .read_pressure_level_2d(0, "temperature", 700)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            QueryError::UnknownPressureLevel { variable, level_hpa }
                if variable == "temperature" && *level_hpa == 700
        ),
        "unexpected error: {error:?}"
    );
    assert!(error.to_string().contains("700"));
}

#[test]
fn plane_readers_reject_the_wrong_variable_kind_in_both_directions() {
    let (_dir, snapshot) = store("kind");

    let surface_as_pressure = snapshot
        .read_pressure_level_2d(0, "temperature_2m", 850)
        .unwrap_err();
    assert!(
        matches!(
            &surface_as_pressure,
            QueryError::WrongVariableKind { expected, actual, .. }
                if *expected == "pressure3d" && actual == "surface2d"
        ),
        "unexpected error: {surface_as_pressure:?}"
    );

    let pressure_as_surface = snapshot.read_surface_2d(0, "temperature").unwrap_err();
    assert!(
        matches!(
            &pressure_as_surface,
            QueryError::WrongVariableKind { expected, actual, .. }
                if *expected == "surface2d" && actual == "pressure3d"
        ),
        "unexpected error: {pressure_as_surface:?}"
    );
}

#[test]
fn unknown_slots_and_variables_stay_distinguishable_on_the_level_reader() {
    let (_dir, snapshot) = store("unknown");

    assert!(matches!(
        snapshot.read_pressure_level_2d(7, "temperature", 850),
        Err(QueryError::UnknownStorageSlot(7))
    ));
    assert!(matches!(
        snapshot.read_pressure_level_2d(0, "absent", 850),
        Err(QueryError::UnknownVariable(name)) if name == "absent"
    ));
}

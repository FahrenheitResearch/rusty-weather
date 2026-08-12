use std::fs;
use std::path::{Path, PathBuf};

use rw_store::FieldStats2D;
use rw_store::error::RwStoreError;
use rw_store::reader::HourReader;
use rw_store::writer::HourWriter;

const NX: usize = 4;
const NY: usize = 3;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-store-stats-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_stats_hour(path: &Path) {
    let finite = vec![2.0, -4.0, 9.0, 0.0, 3.0, 5.0, 1.0, 8.0, 7.0, 6.0, -1.0, 4.0];
    let mixed = vec![
        f32::NAN,
        -7.0,
        2.0,
        f32::NAN,
        11.0,
        4.0,
        f32::NAN,
        8.0,
        -2.0,
        1.0,
        f32::NAN,
        3.0,
    ];
    let all_missing = vec![f32::NAN; NX * NY];
    let pressure_1000 = vec![280.0; NX * NY];
    let pressure_850 = vec![270.0; NX * NY];
    let pressure_planes: [&[f32]; 2] = [&pressure_1000, &pressure_850];

    let mut writer = HourWriter::new("wrf", "run", 0, NX, NY, "grid", "test");
    writer
        .add_surface2d("finite", "1", serde_json::Value::Null, &finite)
        .unwrap();
    writer
        .add_surface2d("mixed", "1", serde_json::Value::Null, &mixed)
        .unwrap();
    writer
        .add_surface2d("all_missing", "1", serde_json::Value::Null, &all_missing)
        .unwrap();
    writer
        .add_pressure3d(
            "temperature",
            "K",
            serde_json::Value::Null,
            &[1000, 850],
            &pressure_planes,
        )
        .unwrap();
    writer.finish(path).unwrap();
}

#[test]
fn stats_2d_reports_finite_mixed_and_all_missing_fields() {
    let dir = test_dir("values");
    let path = dir.join("f000.rws");
    write_stats_hour(&path);
    let reader = HourReader::open(&path).unwrap();

    assert_eq!(
        reader.stats_2d("finite").unwrap(),
        FieldStats2D {
            finite_min: Some(-4.0),
            finite_max: Some(9.0),
            finite_count: 12,
            missing_count: 0,
        }
    );
    assert_eq!(
        reader.stats_2d("mixed").unwrap(),
        FieldStats2D {
            finite_min: Some(-7.0),
            finite_max: Some(11.0),
            finite_count: 8,
            missing_count: 4,
        }
    );
    assert_eq!(
        reader.stats_2d("all_missing").unwrap(),
        FieldStats2D {
            finite_min: None,
            finite_max: None,
            finite_count: 0,
            missing_count: 12,
        }
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn stats_2d_rejects_unknown_and_non_2d_variables() {
    let dir = test_dir("errors");
    let path = dir.join("f000.rws");
    write_stats_hour(&path);
    let reader = HourReader::open(&path).unwrap();

    let unknown = reader.stats_2d("not_present").unwrap_err();
    assert!(
        matches!(&unknown, RwStoreError::UnknownVariable(name) if name == "not_present"),
        "expected UnknownVariable, got {unknown:?}"
    );

    let pressure = reader.stats_2d("temperature").unwrap_err();
    match pressure {
        RwStoreError::Format(message) => assert!(
            message.contains("pressure3d") && message.contains("surface2d"),
            "unexpected error: {message}"
        ),
        other => panic!("expected Format error, got {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}

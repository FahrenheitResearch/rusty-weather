//! StoreView enumeration + worker round-trip against the synthetic store.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use rw_ui::synthetic::{
    SYNTHETIC_BUILD, SYNTHETIC_HOURS, SYNTHETIC_LEVELS, SYNTHETIC_MODEL, SYNTHETIC_RUN,
    write_synthetic_store,
};
use rw_ui::{
    FieldKey, HourKey, StoreRequest, StoreResponse, StoreView, StoreWorker, VarKind,
};

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-ui-{}-{}", std::process::id(), name));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn enumerate_synthetic_store() {
    let dir = test_dir("enumerate");
    let root = dir.join("store");
    write_synthetic_store(&root).unwrap();

    let tree = StoreView::new(&root).enumerate();
    assert!(tree.warnings.is_empty(), "clean store: {:?}", tree.warnings);
    assert_eq!(tree.models.len(), 1);
    let model = &tree.models[0];
    assert_eq!(model.model, SYNTHETIC_MODEL);
    assert_eq!(model.runs.len(), 1);

    let run = &model.runs[0];
    assert_eq!(run.run, SYNTHETIC_RUN);
    assert_eq!(run.build, SYNTHETIC_BUILD, "build stamp from run.json");
    assert!(run.nx > 0 && run.ny > 0);
    let hours: Vec<u16> = run.hours.iter().map(|h| h.hour).collect();
    assert_eq!(hours, SYNTHETIC_HOURS.to_vec(), "hours ascending");
    for hour in &run.hours {
        assert_eq!(hour.file, format!("f{:03}.rws", hour.hour));
        // 3 surface fields + 2 volumes.
        assert_eq!(hour.variable_count, 5, "variable count from run.json");
        assert!(hour.written_unix > 0);
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn enumerate_missing_root_is_empty_not_error() {
    let dir = test_dir("missing-root");
    let tree = StoreView::new(dir.join("does-not-exist")).enumerate();
    assert!(tree.models.is_empty());
    assert!(tree.warnings.is_empty(), "missing root is a clean empty state");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn enumerate_reports_broken_manifest_as_warning() {
    let dir = test_dir("broken-manifest");
    let root = dir.join("store");
    write_synthetic_store(&root).unwrap();

    // A second run with a corrupt run.json must not blank the good one.
    let bad_run = root.join(SYNTHETIC_MODEL).join("20990101_00z");
    fs::create_dir_all(&bad_run).unwrap();
    fs::write(bad_run.join("run.json"), b"{ not json").unwrap();
    // A directory without run.json is skipped silently.
    fs::create_dir_all(root.join(SYNTHETIC_MODEL).join("scratch")).unwrap();

    let tree = StoreView::new(&root).enumerate();
    assert_eq!(tree.models.len(), 1);
    assert_eq!(
        tree.models[0].runs.len(),
        1,
        "only the valid run is listed"
    );
    assert_eq!(tree.models[0].runs[0].run, SYNTHETIC_RUN);
    assert_eq!(tree.warnings.len(), 1, "broken manifest becomes a warning");
    assert!(tree.warnings[0].contains("run.json"));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn runs_sort_newest_first() {
    let dir = test_dir("run-order");
    let root = dir.join("store");
    write_synthetic_store(&root).unwrap();
    // Clone the run directory under an older name.
    let model_dir = root.join(SYNTHETIC_MODEL);
    let newer = model_dir.join(SYNTHETIC_RUN);
    let older = model_dir.join("20260601_12z");
    fs::create_dir_all(&older).unwrap();
    for entry in fs::read_dir(&newer).unwrap() {
        let entry = entry.unwrap();
        fs::copy(entry.path(), older.join(entry.file_name())).unwrap();
    }
    // Fix the manifest's run name so it stays self-consistent.
    let manifest = fs::read_to_string(older.join("run.json"))
        .unwrap()
        .replace(SYNTHETIC_RUN, "20260601_12z");
    fs::write(older.join("run.json"), manifest).unwrap();

    let tree = StoreView::new(&root).enumerate();
    let runs: Vec<&str> = tree.models[0].runs.iter().map(|r| r.run.as_str()).collect();
    assert_eq!(runs, [SYNTHETIC_RUN, "20260601_12z"], "newest run first");

    let _ = fs::remove_dir_all(&dir);
}

/// Full worker round-trip: enumerate -> hour vars -> field -> sounding.
#[test]
fn worker_round_trip_on_synthetic_store() {
    let dir = test_dir("worker");
    let root = dir.join("store");
    write_synthetic_store(&root).unwrap();

    let worker = StoreWorker::spawn(StoreView::new(&root), || {});
    let timeout = Duration::from_secs(20);
    let hour_key = HourKey {
        model: SYNTHETIC_MODEL.to_string(),
        run: SYNTHETIC_RUN.to_string(),
        hour: SYNTHETIC_HOURS[1],
    };

    worker.send(StoreRequest::Enumerate);
    match worker.recv_timeout(timeout) {
        Some(StoreResponse::Tree(tree)) => {
            assert_eq!(tree.models.len(), 1);
        }
        other => panic!("expected Tree response, got {other:?}"),
    }

    worker.send(StoreRequest::LoadHour(hour_key.clone()));
    let vars = match worker.recv_timeout(timeout) {
        Some(StoreResponse::HourVars(key, Ok(vars))) => {
            assert_eq!(key, hour_key);
            vars
        }
        other => panic!("expected HourVars response, got {other:?}"),
    };
    let names: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "temperature_2m",
            "dewpoint_2m",
            "wind_gust_10m",
            "temperature_iso",
            "dewpoint_iso"
        ]
    );
    assert_eq!(vars[0].kind, VarKind::Surface2D);
    assert_eq!(vars[3].kind, VarKind::Pressure3D);
    assert_eq!(vars[3].levels_hpa, SYNTHETIC_LEVELS.to_vec());

    let field_key = FieldKey {
        hour: hour_key.clone(),
        var: "temperature_2m".to_string(),
    };
    worker.send(StoreRequest::LoadField(field_key.clone()));
    match worker.recv_timeout(timeout) {
        Some(StoreResponse::Field(key, Ok(field))) => {
            assert_eq!(key, field_key);
            assert_eq!(field.values.len(), field.nx * field.ny);
            assert_eq!(field.units, "K");
            let (lo, hi) = field.range.expect("finite values exist");
            assert!(lo < hi, "temperature field has spread: {lo}..{hi}");
            assert!((250.0..320.0).contains(&lo), "plausible Kelvin: {lo}");
        }
        other => panic!("expected Field response, got {other:?}"),
    }

    // Unknown variable surfaces as a string error, not a worker death.
    worker.send(StoreRequest::LoadField(FieldKey {
        hour: hour_key.clone(),
        var: "no_such_var".to_string(),
    }));
    match worker.recv_timeout(timeout) {
        Some(StoreResponse::Field(_, Err(message))) => {
            assert!(message.contains("no_such_var"), "got: {message}");
        }
        other => panic!("expected Field error response, got {other:?}"),
    }

    worker.send(StoreRequest::LoadSounding {
        hour: hour_key.clone(),
        fx: 10.5,
        fy: 20.5,
    });
    match worker.recv_timeout(timeout) {
        Some(StoreResponse::Sounding(key, Ok(sounding))) => {
            assert_eq!(key, hour_key);
            assert_eq!(sounding.vars.len(), 2, "both 3D variables profiled");
            let temp = &sounding.vars[0];
            assert_eq!(temp.name, "temperature_iso");
            assert_eq!(temp.levels_hpa, SYNTHETIC_LEVELS.to_vec());
            assert_eq!(temp.values.len(), SYNTHETIC_LEVELS.len());
            // Plausible: warm at 1000 hPa, cold at 250 hPa, monotonic-ish.
            assert!((270.0..300.0).contains(&temp.values[0]), "{:?}", temp.values);
            assert!(temp.values.last().unwrap() < &240.0, "{:?}", temp.values);
            // Dewpoint below temperature everywhere.
            let dew = &sounding.vars[1];
            for (t, td) in temp.values.iter().zip(&dew.values) {
                assert!(td < t, "dewpoint must sit below temperature");
            }
            // Grid coordinates resolved to lat/lon via grid.rwg.
            let lat = sounding.lat.expect("grid file readable");
            let lon = sounding.lon.expect("grid file readable");
            assert!((30.0..37.0).contains(&lat), "lat {lat}");
            assert!((-105.0..-96.0).contains(&lon), "lon {lon}");
        }
        other => panic!("expected Sounding response, got {other:?}"),
    }

    drop(worker);
    let _ = fs::remove_dir_all(&dir);
}

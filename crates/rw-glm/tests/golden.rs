//! Golden-fixture tests that pin the rw-glm `.rwl` v1 on-disk format FOREVER.
//!
//! The committed fixture files in `tests/golden/v1/` are the ground truth.
//! Any change to the bytes the writer emits — header layout, record layout,
//! bucket-split math — makes `golden_v1_bytes_are_stable` fail. Any change to
//! how the reader decodes them makes `golden_v1_reader_values_match_expected`
//! fail. Regenerating the fixtures is, by definition, a format change and a
//! version bump discussion.
//!
//! Fixture definition (all literal — never derived from the current writer):
//!   satellite = "goes19", date dir 20260101
//!   40 flashes, i in 0..40:
//!     time_unix_ms = 1_767_225_600_000 + i*37_000      (2026-01-01 00:00 UTC base)
//!     lat          = 30.0 + 0.1*i
//!     lon          = -95.0 - 0.05*i
//!     energy       = 1.0e-15 * (1 + i)
//!     area         = 25.0 + i
//!     flash_id     = 1000 + i
//!     flags        = bit0 set iff i % 7 == 0  (degraded quality)
//!     duration_ms  = saturate(400 + i*10), except i == 13 forced to 70_000
//!                    (pins the u16 saturation at 65535)
//!   The 37 s step over 10-minute buckets splits the 40 flashes 17 / 16 / 7
//!   across t0000 / t0010 / t0020 — asserted exactly below.

use std::fs;
use std::path::{Path, PathBuf};

use rw_glm::format::{HEADER_LEN, RECORD_LEN, RwlHeader};
use rw_glm::{
    BBox, BucketWriter, FlashRecord, ValidateDepth, read_flashes, saturate_duration_ms,
    validate_bucket_file,
};

// ---------------------------------------------------------------------------
// Fixture constants — the single source of truth for every test
// ---------------------------------------------------------------------------

const SATELLITE: &str = "goes19";
const DATE_DIR: &str = "20260101";
const BASE_MS: i64 = 1_767_225_600_000;
const N_FLASHES: u32 = 40;
const SOURCE_GRANULES: u32 = 3;

/// Build the i-th flash from the literal formulas.
fn golden_flash(i: u32) -> FlashRecord {
    let fi = i as f32;
    let flags = if i % 7 == 0 { 1u16 } else { 0u16 };
    let duration_raw: i64 = if i == 13 { 70_000 } else { 400 + i as i64 * 10 };
    FlashRecord {
        time_unix_ms: BASE_MS + i as i64 * 37_000,
        lat: 30.0 + 0.1 * fi,
        lon: -95.0 - 0.05 * fi,
        energy: 1.0e-15 * (1.0 + fi),
        area: 25.0 + fi,
        flash_id: 1000 + i,
        flags,
        duration_ms: saturate_duration_ms(duration_raw),
    }
}

/// All 40 flashes in formula order.
fn golden_flashes() -> Vec<FlashRecord> {
    (0..N_FLASHES).map(golden_flash).collect()
}

/// Write the golden store under `root` and return the satellite dir.
/// The single authoritative builder used by both regen and the writer-pin test.
fn build_golden_store(root: &Path) -> PathBuf {
    let mut w = BucketWriter::open(root, SATELLITE).expect("BucketWriter::open");
    // Insert in a deliberately scrambled order to prove the writer sorts; the
    // bytes-stable output must be identical regardless of arrival order.
    let mut flashes = golden_flashes();
    flashes.reverse();
    w.insert_flashes(&flashes, SOURCE_GRANULES)
        .expect("insert_flashes");
    drop(w);
    root.join("glm").join(SATELLITE)
}

/// The three committed bucket filenames in ascending order.
const BUCKET_FILES: [&str; 3] = ["t0000.rwl", "t0010.rwl", "t0020.rwl"];

/// Path to the committed golden fixture directory.
fn committed_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/v1"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-glm-golden-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

// ---------------------------------------------------------------------------
// Bucket-split assertion — the exact 17 / 16 / 7 partition
// ---------------------------------------------------------------------------

#[test]
fn golden_bucket_split_is_17_16_7() {
    let tmp = temp_dir("split");
    let store_root = tmp.join("store");
    let sat_dir = build_golden_store(&store_root);
    let day = sat_dir.join(DATE_DIR);

    let counts: Vec<u32> = BUCKET_FILES
        .iter()
        .map(|name| {
            let bytes = fs::read(day.join(name)).unwrap_or_else(|_| panic!("read {name}"));
            RwlHeader::parse(&bytes).unwrap().record_count
        })
        .collect();
    assert_eq!(
        counts,
        vec![17, 16, 7],
        "37s step over 10-min buckets must split 40 flashes 17/16/7"
    );
    assert_eq!(counts.iter().sum::<u32>(), N_FLASHES);

    let _ = fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Test 1: Writer pin — byte equality between rebuilt and committed fixtures
// ---------------------------------------------------------------------------

#[test]
fn golden_v1_bytes_are_stable() {
    let tmp = temp_dir("writer-pin");
    let store_root = tmp.join("store");
    let sat_dir = build_golden_store(&store_root);
    let rebuilt_day = sat_dir.join(DATE_DIR);
    let committed = committed_dir();

    for name in BUCKET_FILES {
        let rebuilt = fs::read(rebuilt_day.join(name)).unwrap_or_else(|_| panic!("rebuilt {name}"));
        let pinned = fs::read(committed.join(name)).unwrap_or_else(|_| panic!("committed {name}"));
        assert_eq!(
            rebuilt.len(),
            pinned.len(),
            "{name} length changed: committed={} rebuilt={} — FORMAT CHANGE requiring a version bump discussion",
            pinned.len(),
            rebuilt.len()
        );
        if rebuilt != pinned {
            let offset = rebuilt
                .iter()
                .zip(pinned.iter())
                .position(|(a, b)| a != b)
                .unwrap_or(0);
            panic!(
                "{name} first divergence at byte offset {offset} — \
                 FORMAT CHANGE requiring a version bump discussion"
            );
        }
    }

    let _ = fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Test 2: Reader pin — committed fixtures read back exactly as expected
// ---------------------------------------------------------------------------

#[test]
fn golden_v1_reader_values_match_expected() {
    let committed = committed_dir();
    // The committed fixture is a flat dir of bucket files; copy it into a
    // proper <root>/glm/<sat>/<date>/ tree so the reader's path math applies.
    let tmp = temp_dir("reader-pin");
    let day = tmp.join("glm").join(SATELLITE).join(DATE_DIR);
    fs::create_dir_all(&day).unwrap();
    for name in BUCKET_FILES {
        fs::copy(committed.join(name), day.join(name)).unwrap();
    }

    let expected: serde_json::Value =
        serde_json::from_slice(&fs::read(committed.join("expected.json")).expect("expected.json"))
            .expect("parse expected.json");

    // ---- whole-window read, all 40, ascending ----
    let all = read_flashes(&tmp, SATELLITE, BASE_MS, BASE_MS + 40 * 37_000, None).unwrap();
    assert_eq!(all.len(), N_FLASHES as usize, "whole window must return 40");
    for w in all.windows(2) {
        assert!(w[0].time_unix_ms <= w[1].time_unix_ms, "must be ascending");
    }

    // ---- spot values for flash i = 13 (the saturated-duration one) ----
    let f13 = all.iter().find(|f| f.flash_id == 1013).expect("flash 1013");
    let exp13 = &expected["flash_13"];
    assert_eq!(f13.time_unix_ms, exp13["time_unix_ms"].as_i64().unwrap());
    assert_eq!(f13.duration_ms, 65535, "duration must saturate at 65535");
    assert_eq!(
        f13.duration_ms,
        exp13["duration_ms"].as_u64().unwrap() as u16
    );
    assert_eq!(
        f13.lat.to_bits(),
        (exp13["lat"].as_f64().unwrap() as f32).to_bits(),
        "flash 13 lat"
    );
    assert_eq!(
        f13.lon.to_bits(),
        (exp13["lon"].as_f64().unwrap() as f32).to_bits(),
        "flash 13 lon"
    );
    assert_eq!(
        f13.energy.to_bits(),
        (exp13["energy"].as_f64().unwrap() as f32).to_bits(),
        "flash 13 energy"
    );

    // ---- degraded-quality bit on every 7th flash (i=0,7,14,21,28,35) ----
    let degraded: Vec<u32> = all
        .iter()
        .filter(|f| f.is_degraded())
        .map(|f| f.flash_id)
        .collect();
    let expected_degraded: Vec<u32> = expected["degraded_ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert_eq!(degraded, expected_degraded, "degraded-quality ids");

    // ---- bucket split ----
    let split: Vec<u32> = expected["bucket_split"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_u64().unwrap() as u32)
        .collect();
    assert_eq!(split, vec![17, 16, 7]);

    // ---- range + bbox count and checksum ----
    let bbox = BBox::new(
        expected["bbox"]["lat_min"].as_f64().unwrap() as f32,
        expected["bbox"]["lat_max"].as_f64().unwrap() as f32,
        expected["bbox"]["lon_min"].as_f64().unwrap() as f32,
        expected["bbox"]["lon_max"].as_f64().unwrap() as f32,
    );
    let t0 = expected["range"]["t0"].as_i64().unwrap();
    let t1 = expected["range"]["t1"].as_i64().unwrap();
    let sel = read_flashes(&tmp, SATELLITE, t0, t1, Some(bbox)).unwrap();
    assert_eq!(
        sel.len(),
        expected["range_bbox_count"].as_u64().unwrap() as usize,
        "range+bbox count"
    );
    let checksum: f64 = sel.iter().map(|f| f.energy as f64).sum();
    let checksum_str = format!("{checksum:.18}");
    assert_eq!(
        checksum_str,
        expected["range_bbox_energy_checksum"].as_str().unwrap(),
        "range+bbox energy checksum"
    );

    // ---- every committed bucket Deep-validates ----
    // Validate the nested copies under `day` (<root>/glm/<sat>/<date>/tHHMM.rwl),
    // not the flat committed fixtures: Deep validation now includes the
    // bucket-membership check, which compares each record against the file's own
    // name and parent-dir (`YYYYMMDD`) name. The committed fixtures live in a
    // flat `v1/` dir, so only the properly-nested layout exercises that check.
    for name in BUCKET_FILES {
        let report = validate_bucket_file(&day.join(name), ValidateDepth::Deep).unwrap();
        assert!(
            report.is_ok(),
            "committed {name} must deep-validate; errors: {:?}",
            report.errors
        );
    }

    let _ = fs::remove_dir_all(&tmp);
}

// ---------------------------------------------------------------------------
// Test 3: Regen (ignored) — writes the fixture bucket files + expected.json
// ---------------------------------------------------------------------------

#[test]
#[ignore = "regen overwrites committed fixtures — only run deliberately"]
fn regen_golden_v1() {
    eprintln!();
    eprintln!("==========================================================================");
    eprintln!("WARNING: regen_golden_v1 is about to OVERWRITE committed fixture files.");
    eprintln!("Committing these changes constitutes a FORMAT CHANGE and REQUIRES a");
    eprintln!("version bump discussion before merging.");
    eprintln!("==========================================================================");
    eprintln!();

    // Build and self-verify before pinning — never pin garbage.
    let tmp = temp_dir("regen");
    let store_root = tmp.join("store");
    let sat_dir = build_golden_store(&store_root);
    let day = sat_dir.join(DATE_DIR);

    // Read back every flash and confirm it matches the formula bit-exactly.
    let all = read_flashes(&store_root, SATELLITE, BASE_MS, BASE_MS + 40 * 37_000, None).unwrap();
    assert_eq!(all.len(), N_FLASHES as usize);
    let want = golden_flashes();
    let by_id: std::collections::HashMap<u32, &FlashRecord> =
        want.iter().map(|r| (r.flash_id, r)).collect();
    for f in &all {
        let w = by_id[&f.flash_id];
        assert_eq!(f.time_unix_ms, w.time_unix_ms);
        assert_eq!(f.lat.to_bits(), w.lat.to_bits());
        assert_eq!(f.lon.to_bits(), w.lon.to_bits());
        assert_eq!(f.energy.to_bits(), w.energy.to_bits());
        assert_eq!(f.area.to_bits(), w.area.to_bits());
        assert_eq!(f.flags, w.flags);
        assert_eq!(f.duration_ms, w.duration_ms);
    }

    // Bucket split.
    let split: Vec<u32> = BUCKET_FILES
        .iter()
        .map(|name| {
            RwlHeader::parse(&fs::read(day.join(name)).unwrap())
                .unwrap()
                .record_count
        })
        .collect();
    assert_eq!(split, vec![17, 16, 7], "regen bucket split");

    // Each bucket must be exactly 64 + 32*count and deep-validate.
    for name in BUCKET_FILES {
        let bytes = fs::read(day.join(name)).unwrap();
        let count = RwlHeader::parse(&bytes).unwrap().record_count as usize;
        assert_eq!(bytes.len(), HEADER_LEN + count * RECORD_LEN);
        let report = validate_bucket_file(&day.join(name), ValidateDepth::Deep).unwrap();
        assert!(report.is_ok(), "regen {name}: {:?}", report.errors);
    }

    // ---- compute expected.json ----
    let f13 = golden_flash(13);
    let degraded_ids: Vec<u32> = (0..N_FLASHES)
        .filter(|i| i % 7 == 0)
        .map(|i| 1000 + i)
        .collect();

    // Range+bbox: choose a window that lands inside t0000+t0010 and a bbox that
    // clips out the higher-index (further south/west) flashes.
    let t0 = BASE_MS;
    let t1 = BASE_MS + 900_000; // excludes t0020 entirely and part of t0010
    let bbox = BBox::new(29.0, 32.0, -97.0, -95.0);
    let sel = read_flashes(&store_root, SATELLITE, t0, t1, Some(bbox)).unwrap();
    let checksum: f64 = sel.iter().map(|f| f.energy as f64).sum();
    let checksum_str = format!("{checksum:.18}");

    let expected = serde_json::json!({
        "flash_13": {
            "time_unix_ms": f13.time_unix_ms,
            "lat": f13.lat as f64,
            "lon": f13.lon as f64,
            "energy": f13.energy as f64,
            "duration_ms": f13.duration_ms,
        },
        "degraded_ids": degraded_ids,
        "bucket_split": [17, 16, 7],
        "range": { "t0": t0, "t1": t1 },
        "bbox": {
            "lat_min": 29.0, "lat_max": 32.0, "lon_min": -97.0, "lon_max": -95.0,
        },
        "range_bbox_count": sel.len(),
        "range_bbox_energy_checksum": checksum_str,
    });

    // ---- write fixture files ----
    let fixture_dir = committed_dir();
    fs::create_dir_all(&fixture_dir).expect("create fixture dir");
    for name in BUCKET_FILES {
        fs::copy(day.join(name), fixture_dir.join(name)).expect("copy bucket");
    }
    let bytes = serde_json::to_vec_pretty(&expected).expect("serialize expected.json");
    fs::write(fixture_dir.join("expected.json"), &bytes).expect("write expected.json");

    eprintln!("Fixture files written to: {}", fixture_dir.display());
    for name in BUCKET_FILES {
        eprintln!(
            "  {name}  ({} bytes)",
            fs::metadata(fixture_dir.join(name)).unwrap().len()
        );
    }
    eprintln!(
        "  expected.json  ({} bytes)",
        fs::metadata(fixture_dir.join("expected.json"))
            .unwrap()
            .len()
    );

    let _ = fs::remove_dir_all(&tmp);
}

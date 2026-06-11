//! Integration tests for the `.rwl` store writer and the `read_flashes` API:
//! sort-on-insert, atomic rewrite visibility, time-range file selection,
//! half-open semantics, bbox filtering, empty range, and missing-store.

use std::path::PathBuf;

use rw_glm::{BBox, BucketWriter, FlashRecord, ValidateDepth, read_flashes, validate_bucket_file};

/// 2026-01-01 00:00:00 UTC in Unix ms.
const BASE: i64 = 1_767_225_600_000;

fn temp_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-glm-it-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn flash(time: i64, lat: f32, lon: f32, id: u32) -> FlashRecord {
    FlashRecord {
        time_unix_ms: time,
        lat,
        lon,
        energy: 1.0e-15,
        area: 25.0,
        flash_id: id,
        flags: 0,
        duration_ms: 400,
    }
}

#[test]
fn writer_sorts_arbitrary_granule_order_within_a_bucket() {
    let root = temp_root("sort");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    // Insert out of order, all inside t0000.
    let records = vec![
        flash(BASE + 300_000, 31.0, -94.0, 3),
        flash(BASE + 100_000, 30.0, -95.0, 1),
        flash(BASE + 200_000, 30.5, -94.5, 2),
    ];
    w.insert_flashes(&records, 1).unwrap();
    drop(w);

    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    let times: Vec<i64> = got.iter().map(|f| f.time_unix_ms).collect();
    assert_eq!(times, vec![BASE + 100_000, BASE + 200_000, BASE + 300_000]);

    // And the on-disk bucket validates deep.
    let bucket = root
        .join("glm")
        .join("goes19")
        .join("20260101")
        .join("t0000.rwl");
    let report = validate_bucket_file(&bucket, ValidateDepth::Deep).unwrap();
    assert!(report.is_ok(), "validate errors: {:?}", report.errors);
}

#[test]
fn multiple_inserts_merge_and_keep_sorted() {
    let root = temp_root("merge");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    w.insert_flashes(&[flash(BASE + 200_000, 30.0, -95.0, 2)], 1)
        .unwrap();
    // Second granule lands an earlier flash into the same bucket.
    w.insert_flashes(&[flash(BASE + 100_000, 30.0, -95.0, 1)], 1)
        .unwrap();
    drop(w);

    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    let times: Vec<i64> = got.iter().map(|f| f.time_unix_ms).collect();
    assert_eq!(times, vec![BASE + 100_000, BASE + 200_000]);

    // source_granule_count accumulates across inserts touching the bucket.
    let bytes = std::fs::read(
        root.join("glm")
            .join("goes19")
            .join("20260101")
            .join("t0000.rwl"),
    )
    .unwrap();
    let header = rw_glm::RwlHeader::parse(&bytes).unwrap();
    assert_eq!(header.source_granule_count, 2);
    assert_eq!(header.record_count, 2);
}

#[test]
fn atomic_rewrite_is_visible_to_a_fresh_reader() {
    let root = temp_root("atomic");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    w.insert_flashes(&[flash(BASE + 100_000, 30.0, -95.0, 1)], 1)
        .unwrap();

    // Mid-stream read (writer still alive and holding the lock) — the reader is
    // lock-free and must see the first atomic write.
    let mid = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert_eq!(
        mid.len(),
        1,
        "first write must be visible while writer lives"
    );

    // Append a second flash; a new read sees both.
    w.insert_flashes(&[flash(BASE + 200_000, 31.0, -94.0, 2)], 1)
        .unwrap();
    let after = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert_eq!(after.len(), 2, "second write must be visible too");
    drop(w);
}

#[test]
fn time_range_selection_spans_three_buckets_and_filters_half_open() {
    let root = temp_root("range");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    // Flashes across t0000, t0010, t0020.
    let records = vec![
        flash(BASE, 30.0, -95.0, 0),             // t0000, at exactly t0
        flash(BASE + 300_000, 30.1, -95.1, 1),   // t0000
        flash(BASE + 600_000, 30.2, -95.2, 2),   // t0010 boundary
        flash(BASE + 900_000, 30.3, -95.3, 3),   // t0010
        flash(BASE + 1_200_000, 30.4, -95.4, 4), // t0020 boundary
        flash(BASE + 1_500_000, 30.5, -95.5, 5), // t0020
    ];
    w.insert_flashes(&records, 1).unwrap();
    drop(w);

    // Confirm three bucket files exist (file selection correctness).
    let day = root.join("glm").join("goes19").join("20260101");
    assert!(day.join("t0000.rwl").is_file());
    assert!(day.join("t0010.rwl").is_file());
    assert!(day.join("t0020.rwl").is_file());

    // Half-open [t0, t1): start inclusive, end exclusive.
    // Range [BASE, BASE+1_200_000) must include ids 0..=3, exclude 4 (== t1) and 5.
    let got = read_flashes(&root, "goes19", BASE, BASE + 1_200_000, None).unwrap();
    let ids: Vec<u32> = got.iter().map(|f| f.flash_id).collect();
    assert_eq!(
        ids,
        vec![0, 1, 2, 3],
        "half-open end must exclude id 4 at t1"
    );

    // A flash exactly at t0 is included.
    let got2 = read_flashes(&root, "goes19", BASE, BASE + 1, None).unwrap();
    assert_eq!(got2.len(), 1);
    assert_eq!(got2[0].flash_id, 0);

    // Whole-window read returns all six, sorted.
    let all = read_flashes(&root, "goes19", BASE, BASE + 2_000_000, None).unwrap();
    let all_ids: Vec<u32> = all.iter().map(|f| f.flash_id).collect();
    assert_eq!(all_ids, vec![0, 1, 2, 3, 4, 5]);
}

#[test]
fn bbox_filter_clips_to_region() {
    let root = temp_root("bbox");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    let records = vec![
        flash(BASE + 10_000, 30.0, -95.0, 0), // inside
        flash(BASE + 20_000, 45.0, -95.0, 1), // lat too high
        flash(BASE + 30_000, 30.0, -80.0, 2), // lon too high
        flash(BASE + 40_000, 31.0, -96.0, 3), // inside
    ];
    w.insert_flashes(&records, 1).unwrap();
    drop(w);

    let bbox = BBox::new(29.0, 32.0, -97.0, -94.0);
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, Some(bbox)).unwrap();
    let ids: Vec<u32> = got.iter().map(|f| f.flash_id).collect();
    assert_eq!(ids, vec![0, 3], "bbox keeps only the two in-region flashes");
}

#[test]
fn empty_and_inverted_ranges_return_empty_without_io() {
    let root = temp_root("empty-range");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    w.insert_flashes(&[flash(BASE + 100_000, 30.0, -95.0, 1)], 1)
        .unwrap();
    drop(w);

    assert!(
        read_flashes(&root, "goes19", BASE, BASE, None)
            .unwrap()
            .is_empty()
    );
    assert!(
        read_flashes(&root, "goes19", BASE + 100, BASE, None)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn missing_satellite_dir_returns_empty() {
    let root = temp_root("missing-sat");
    // Nothing written. Reading any satellite is a clean empty.
    let got = read_flashes(&root, "goes18", BASE, BASE + 600_000, None).unwrap();
    assert!(got.is_empty());

    // Even a missing root entirely.
    let absent = root.join("does-not-exist");
    let got2 = read_flashes(&absent, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert!(got2.is_empty());
}

#[test]
fn window_manifest_tracks_extent() {
    let root = temp_root("window");
    let mut w = BucketWriter::open(&root, "goes19").unwrap();
    w.insert_flashes(
        &[
            flash(BASE + 100_000, 30.0, -95.0, 1),
            flash(BASE + 700_000, 31.0, -94.0, 2),
        ],
        1,
    )
    .unwrap();
    drop(w);

    let manifest_bytes =
        std::fs::read(root.join("glm").join("goes19").join("window.json")).unwrap();
    let manifest: rw_glm::WindowManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest.satellite, "goes19");
    assert_eq!(manifest.time_min_unix_ms, Some(BASE + 100_000));
    assert_eq!(manifest.time_max_unix_ms, Some(BASE + 700_000));
}

#[test]
fn second_writer_is_locked_out_while_first_lives() {
    let root = temp_root("lock");
    let w1 = BucketWriter::open(&root, "goes19").unwrap();
    // A second writer on the same satellite must time out quickly.
    let err =
        BucketWriter::open_with_timeout(&root, "goes19", std::time::Duration::from_millis(200))
            .unwrap_err();
    assert!(
        matches!(err, rw_glm::RwlError::Locked(_)),
        "expected Locked, got {err:?}"
    );
    drop(w1);
    // Once released, a new writer can open.
    let _w2 = BucketWriter::open(&root, "goes19").unwrap();
}

//! Structural equivalence comparison for `.rws` hour files.
//!
//! Two hour files compare **equivalent** when their header `version` /
//! `index_count`, meta JSON (with `writer.build` masked out), index records
//! (with `offset` compared relative to each file's `payload_offset`), and
//! payload bytes all match.  This lets independent builds of the same inputs
//! be verified as deterministically equivalent even when the writer-build sha
//! differs (which shifts every absolute offset).
//!
//! The `assert-build` helpers ([`read_writer_build`] / [`build_matches`]) guard
//! against mislabeled baselines: verify the producer's build stamp BEFORE
//! trusting any comparison made with its output.

use std::fs;
use std::path::Path;

use crate::header::RwsHeader;
use crate::index::ChunkRecord;

/// A difference found between two files (or an I/O error while comparing).
#[derive(Debug)]
pub enum Difference {
    /// An I/O or format error prevented the comparison.
    Io(String),
    /// The files differ; the message describes the first difference found.
    Found(String),
}

/// Compare two `.rws` hour files structurally:
///
/// - header: `version`, `index_count`
/// - meta JSON: all fields with `writer.build` masked to null
/// - index records: all fields, with `offset` normalised to payload-relative
///   (so a different-length `writer.build` that shifts absolute offsets does
///   not register as a difference)
/// - payload: byte-for-byte from each file's `payload_offset` onward
///
/// Returns `Ok(())` when equivalent.  The `Ok` path prints a summary line to
/// stdout; the `Err(Difference::Found(_))` path prints nothing — the caller
/// prints the difference.
pub fn compare(path_a: &Path, path_b: &Path) -> Result<(), Difference> {
    let bytes_a = fs::read(path_a)
        .map_err(|err| Difference::Io(format!("read {}: {err}", path_a.display())))?;
    let bytes_b = fs::read(path_b)
        .map_err(|err| Difference::Io(format!("read {}: {err}", path_b.display())))?;
    let header_a = RwsHeader::parse(&bytes_a)
        .map_err(|err| Difference::Io(format!("header {}: {err}", path_a.display())))?;
    let header_b = RwsHeader::parse(&bytes_b)
        .map_err(|err| Difference::Io(format!("header {}: {err}", path_b.display())))?;

    if header_a.version != header_b.version {
        return Err(Difference::Found(format!(
            "header version {} vs {}",
            header_a.version, header_b.version
        )));
    }
    if header_a.index_count != header_b.index_count {
        return Err(Difference::Found(format!(
            "index_count {} vs {}",
            header_a.index_count, header_b.index_count
        )));
    }

    // Meta JSON with writer.build masked out.
    let meta_a = meta_without_build(&bytes_a, &header_a, path_a)?;
    let meta_b = meta_without_build(&bytes_b, &header_b, path_b)?;
    if meta_a != meta_b {
        return Err(Difference::Found(
            "meta JSON differs beyond writer.build (variables/levels/selectors/grid_hash)"
                .to_string(),
        ));
    }

    // Index records, offsets normalized to the payload base.
    for index in 0..header_a.index_count as usize {
        let record_a = record_at(&bytes_a, &header_a, index, path_a)?;
        let record_b = record_at(&bytes_b, &header_b, index, path_b)?;
        let rel_a = record_a.offset.wrapping_sub(header_a.payload_offset);
        let rel_b = record_b.offset.wrapping_sub(header_b.payload_offset);
        let fields_equal = record_a.var_id == record_b.var_id
            && record_a.kind == record_b.kind
            && record_a.flags == record_b.flags
            && record_a.tile_y == record_b.tile_y
            && record_a.tile_x == record_b.tile_x
            && rel_a == rel_b
            && record_a.len == record_b.len
            && record_a.raw_len == record_b.raw_len
            && record_a.center.to_bits() == record_b.center.to_bits()
            && record_a.scale.to_bits() == record_b.scale.to_bits()
            && record_a.min.to_bits() == record_b.min.to_bits()
            && record_a.max.to_bits() == record_b.max.to_bits()
            && record_a.valid_count == record_b.valid_count;
        if !fields_equal {
            return Err(Difference::Found(format!(
                "index record {index}: {record_a:?} (rel offset {rel_a}) vs {record_b:?} \
                 (rel offset {rel_b})"
            )));
        }
    }

    // Payload regions, byte for byte.
    let payload_a = &bytes_a[header_a.payload_offset as usize..];
    let payload_b = &bytes_b[header_b.payload_offset as usize..];
    if payload_a.len() != payload_b.len() {
        return Err(Difference::Found(format!(
            "payload length {} vs {}",
            payload_a.len(),
            payload_b.len()
        )));
    }
    if let Some(position) = payload_a
        .iter()
        .zip(payload_b.iter())
        .position(|(a, b)| a != b)
    {
        return Err(Difference::Found(format!(
            "payload bytes differ at payload offset {position} (of {})",
            payload_a.len()
        )));
    }
    println!(
        "compared: {} index records, {} payload bytes, meta keys minus writer.build",
        header_a.index_count,
        payload_a.len()
    );
    Ok(())
}

/// Parse the meta JSON region and set `writer.build` to null so two files
/// that differ only in their build stamp compare equal.
pub fn meta_without_build(
    bytes: &[u8],
    header: &RwsHeader,
    path: &Path,
) -> Result<serde_json::Value, Difference> {
    let start = 64usize;
    let end = start + header.meta_len as usize;
    let mut meta: serde_json::Value =
        serde_json::from_slice(bytes.get(start..end).ok_or_else(|| {
            Difference::Io(format!("{}: meta region out of range", path.display()))
        })?)
        .map_err(|err| Difference::Io(format!("{}: meta JSON: {err}", path.display())))?;
    if let Some(writer) = meta.get_mut("writer") {
        if let Some(build) = writer.get_mut("build") {
            *build = serde_json::Value::Null;
        }
    }
    Ok(meta)
}

/// Read one 64-byte index record at position `index`.
pub fn record_at(
    bytes: &[u8],
    header: &RwsHeader,
    index: usize,
    path: &Path,
) -> Result<ChunkRecord, Difference> {
    let start = header.index_offset as usize + index * 64;
    let slice = bytes.get(start..start + 64).ok_or_else(|| {
        Difference::Io(format!(
            "{}: index record {index} out of range",
            path.display()
        ))
    })?;
    ChunkRecord::unpack(slice)
        .map_err(|err| Difference::Io(format!("{}: index record {index}: {err}", path.display())))
}

/// Read the `writer.build` stamp out of a store artifact: `run.json` (any
/// `.json` file with `writer.build`) or an `.rws` hour file's meta JSON.
pub fn read_writer_build(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let meta: serde_json::Value = if path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
    {
        serde_json::from_slice(&bytes).map_err(|err| format!("{}: JSON: {err}", path.display()))?
    } else {
        let header =
            RwsHeader::parse(&bytes).map_err(|err| format!("header {}: {err}", path.display()))?;
        let start = 64usize;
        let end = start + header.meta_len as usize;
        serde_json::from_slice(
            bytes
                .get(start..end)
                .ok_or_else(|| format!("{}: meta region out of range", path.display()))?,
        )
        .map_err(|err| format!("{}: meta JSON: {err}", path.display()))?
    };
    meta.get("writer")
        .and_then(|writer| writer.get("build"))
        .and_then(|build| build.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{}: no writer.build in meta", path.display()))
}

/// `expected` is a sha prefix (short or full). The recorded build matches
/// when it starts with the prefix and carries nothing beyond more sha hex
/// digits — so `290cf4b` matches `290cf4b2fce8` but NOT `290cf4b2fce8-dirty`:
/// a dirty build is not the claimed commit. To accept a dirty stamp
/// deliberately, pass the full stamp including the `-dirty` suffix.
pub fn build_matches(expected: &str, build: &str) -> bool {
    let Some(rest) = build.strip_prefix(expected) else {
        return false;
    };
    rest.chars().all(|c| c.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rustwx_core::{GridShape, LatLonGrid};

    use crate::ingest::HourIngestWriter;

    use super::*;

    // ── build_matches unit tests (verbatim from old bin) ──────────────────────

    #[test]
    fn build_prefix_matches_longer_sha() {
        assert!(build_matches("290cf4b", "290cf4b2fce8"));
        assert!(build_matches("290cf4b2fce8", "290cf4b2fce8"));
    }

    #[test]
    fn build_prefix_rejects_dirty_stamp() {
        assert!(!build_matches("290cf4b", "290cf4b2fce8-dirty"));
        assert!(!build_matches("290cf4b2fce8", "290cf4b2fce8-dirty"));
    }

    #[test]
    fn explicit_dirty_expectation_is_accepted() {
        assert!(build_matches("290cf4b2fce8-dirty", "290cf4b2fce8-dirty"));
    }

    #[test]
    fn build_prefix_rejects_unrelated_sha() {
        assert!(!build_matches("290cf4b", "a7bf0c7171ee"));
        assert!(!build_matches("290cf4b", "unknown"));
    }

    #[test]
    fn run_json_build_extraction() {
        let dir = std::env::temp_dir().join("rw_store_diff_test_run_json");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.json");
        std::fs::write(
            &path,
            r#"{"schema":"rw-store.run.v1","writer":{"name":"rw-store","version":"0.1.0","build":"290cf4b2fce8"}}"#,
        )
        .unwrap();
        assert_eq!(read_writer_build(&path).unwrap(), "290cf4b2fce8");
        std::fs::remove_file(&path).ok();
    }

    // ── compare() correctness tests ───────────────────────────────────────────

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rw-store-diff-{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn tiny_grid() -> LatLonGrid {
        let nx = 20usize;
        let ny = 10usize;
        let lat: Vec<f32> = (0..ny)
            .flat_map(|gy| (0..nx).map(move |_gx| 30.0_f32 + 0.01 * gy as f32))
            .collect();
        let lon: Vec<f32> = (0..ny)
            .flat_map(|_gy| (0..nx).map(move |gx| -100.0_f32 + 0.05 * gx as f32))
            .collect();
        LatLonGrid::new(GridShape::new(nx, ny).unwrap(), lat, lon).unwrap()
    }

    fn write_tiny_hour(store_root: &std::path::Path, writer_build: &str) -> PathBuf {
        let grid = tiny_grid();
        let nx = 20usize;
        let ny = 10usize;
        let values: Vec<f32> = (0..ny * nx).map(|i| i as f32).collect();

        let mut writer = HourIngestWriter::begin(
            store_root,
            "test",
            "20260101_00z",
            0,
            &grid,
            None,
            writer_build,
        )
        .expect("HourIngestWriter::begin");

        writer
            .add_field_2d("t2m", "K", serde_json::json!({"var": "TMP"}), &values)
            .expect("add t2m");

        writer.finish(0).expect("finish");
        store_root.join("test").join("20260101_00z")
    }

    /// Two byte-identical files compare Ok.
    #[test]
    fn identical_files_compare_ok() {
        let dir = test_dir("identical");
        let run_dir = write_tiny_hour(&dir, "build-abc");
        let hour = run_dir.join("f000.rws");

        assert!(
            compare(&hour, &hour).is_ok(),
            "identical file should compare Ok"
        );
    }

    /// Flipping one payload byte produces a Found difference.
    #[test]
    fn flipped_payload_byte_detected() {
        let dir = test_dir("flip");
        let run_dir = write_tiny_hour(&dir, "build-abc");
        let hour = run_dir.join("f000.rws");

        let mut bytes = std::fs::read(&hour).unwrap();
        // Parse the header to locate the payload region.
        let header = RwsHeader::parse(&bytes).unwrap();
        let payload_start = header.payload_offset as usize;
        // Flip the first payload byte.
        bytes[payload_start] ^= 0xFF;
        let corrupted = dir.join("f000_corrupt.rws");
        std::fs::write(&corrupted, &bytes).unwrap();

        match compare(&hour, &corrupted) {
            Err(Difference::Found(msg)) => {
                assert!(
                    msg.contains("payload bytes differ"),
                    "expected payload-diff message, got: {msg}"
                );
            }
            Ok(()) => panic!("expected Difference::Found, got Ok"),
            Err(Difference::Io(msg)) => panic!("expected Difference::Found, got Io: {msg}"),
        }
    }

    /// Two files differing only in writer.build still compare Ok (build masked).
    #[test]
    fn different_writer_builds_compare_ok() {
        let dir = test_dir("build-masked");
        let run_a = dir.join("store_a");
        let run_b = dir.join("store_b");
        let dir_a = write_tiny_hour(&run_a, "aaaaaaaaaaaaa");
        let dir_b = write_tiny_hour(&run_b, "bbbbbbbbbbbbb");
        let hour_a = dir_a.join("f000.rws");
        let hour_b = dir_b.join("f000.rws");

        assert!(
            compare(&hour_a, &hour_b).is_ok(),
            "files differing only in writer.build should compare Ok"
        );
    }
}

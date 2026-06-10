//! Determinism comparator for `.rws` hour files: proves two (or more) hour
//! files are byte-identical EXCEPT for the writer-provenance meta
//! (`writer.build`) and the offset shift that a different-length meta JSON
//! induces.
//!
//! Two independently produced hour files (e.g. a baseline build and a
//! refactored build ingesting the same GRIB inputs) cannot be compared
//! with a flat byte diff: the meta JSON embeds the writer's build sha, and
//! a build-sha length difference shifts every absolute payload offset in
//! the index. This tool compares the regions structurally:
//!
//! * header: version, index_count (meta_len/offsets are derived);
//! * meta JSON: every field, with `writer.build` excluded;
//! * index: every record field, with `offset` compared relative to the
//!   file's payload base (`record.offset - payload_offset`);
//! * payload: the byte regions `[payload_offset..]` of both files.
//!
//! With more than two files the tool runs the N-run self-consistency
//! check (see `docs/DETERMINISM.md`): every file is grouped into
//! structural-equivalence classes and the majority/minority split is
//! reported, so a scheduling-dependent flake in one run is visible
//! instead of silently passing or failing the gate.
//!
//! The `assert-build` verb guards against mislabeled baselines: it
//! verifies that the writer build sha recorded inside an artifact
//! (`run.json` or `.rws` meta) matches the sha the caller claims the
//! producing binary was built from. Verification flows must assert the
//! baseline's stamp BEFORE trusting any comparison made with it.
//!
//! Exit code 0 = equivalent / assertion held, 1 = different / assertion
//! failed (first difference printed), 2 = usage/IO error.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

use rw_store::header::RwsHeader;
use rw_store::index::ChunkRecord;

const USAGE: &str = "usage: rw_store_diff <hour_a.rws> <hour_b.rws> [hour_c.rws ...]\n       rw_store_diff assert-build <expected-sha> <run.json|hour.rws> [...]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("assert-build") => {
            let [_, expected, artifacts @ ..] = args.as_slice() else {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            };
            if artifacts.is_empty() {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
            assert_build(expected, artifacts)
        }
        _ => {
            if args.len() < 2 {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
            compare_files(&args)
        }
    }
}

fn compare_files(paths: &[String]) -> ExitCode {
    if paths.len() == 2 {
        return match compare(Path::new(&paths[0]), Path::new(&paths[1])) {
            Ok(()) => {
                println!("equivalent: payload + index + meta (writer.build excluded) match");
                ExitCode::SUCCESS
            }
            Err(Difference::Io(message)) => {
                eprintln!("error: {message}");
                ExitCode::from(2)
            }
            Err(Difference::Found(message)) => {
                eprintln!("DIFFERENT: {message}");
                ExitCode::FAILURE
            }
        };
    }

    // N-run self-consistency: group the files into structural-equivalence
    // classes (each class representative is the first file seen in it).
    let mut classes: Vec<Vec<usize>> = Vec::new();
    for (index, path) in paths.iter().enumerate() {
        let mut placed = false;
        for class in classes.iter_mut() {
            let representative = &paths[class[0]];
            match compare(Path::new(representative), Path::new(path)) {
                Ok(()) => {
                    class.push(index);
                    placed = true;
                    break;
                }
                Err(Difference::Found(_)) => {}
                Err(Difference::Io(message)) => {
                    eprintln!("error: {message}");
                    return ExitCode::from(2);
                }
            }
        }
        if !placed {
            classes.push(vec![index]);
        }
    }

    if classes.len() == 1 {
        println!(
            "self-consistent: all {} hour files structurally equivalent (writer.build excluded)",
            paths.len()
        );
        return ExitCode::SUCCESS;
    }

    classes.sort_by_key(|class| std::cmp::Reverse(class.len()));
    let majority_size = classes[0].len();
    let has_majority = classes
        .get(1)
        .is_none_or(|runner_up| runner_up.len() < majority_size);
    eprintln!(
        "DIFFERENT: {} equivalence classes across {} runs{}",
        classes.len(),
        paths.len(),
        if has_majority {
            format!(" (majority: {majority_size} runs)")
        } else {
            " (NO majority)".to_string()
        }
    );
    for (rank, class) in classes.iter().enumerate() {
        let members = class
            .iter()
            .map(|&index| paths[index].as_str())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("  class {} ({} runs): {members}", rank + 1, class.len());
    }
    // Print the first concrete difference (majority representative vs the
    // first minority representative) so the flake is actionable.
    if let Err(Difference::Found(message)) = compare(
        Path::new(&paths[classes[0][0]]),
        Path::new(&paths[classes[1][0]]),
    ) {
        eprintln!("  first difference: {message}");
    }
    ExitCode::FAILURE
}

fn assert_build(expected: &str, artifacts: &[String]) -> ExitCode {
    if expected.is_empty() {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    }
    let mut failed = false;
    for artifact in artifacts {
        let path = Path::new(artifact);
        match read_writer_build(path) {
            Ok(build) => {
                if build_matches(expected, &build) {
                    println!("{artifact}: writer build {build} matches {expected}");
                } else {
                    eprintln!(
                        "MISMATCH: {artifact}: writer build {build} does not match expected \
                         {expected}"
                    );
                    failed = true;
                }
            }
            Err(message) => {
                eprintln!("error: {message}");
                return ExitCode::from(2);
            }
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// `expected` is a sha prefix (short or full). The recorded build matches
/// when it starts with the prefix and carries nothing beyond more sha
/// hex digits — so `290cf4b` matches `290cf4b2fce8` but NOT
/// `290cf4b2fce8-dirty`: a dirty build is not the claimed commit. To
/// accept a dirty stamp deliberately, pass the full stamp including the
/// `-dirty` suffix.
fn build_matches(expected: &str, build: &str) -> bool {
    let Some(rest) = build.strip_prefix(expected) else {
        return false;
    };
    rest.chars().all(|c| c.is_ascii_hexdigit())
}

/// Read the writer build stamp out of a store artifact: `run.json` (any
/// `.json` file with `writer.build`) or an `.rws` hour file's meta JSON.
fn read_writer_build(path: &Path) -> Result<String, String> {
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

enum Difference {
    Io(String),
    Found(String),
}

fn compare(path_a: &Path, path_b: &Path) -> Result<(), Difference> {
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

fn meta_without_build(
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

fn record_at(
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

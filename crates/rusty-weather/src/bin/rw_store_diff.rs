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

use std::path::Path;
use std::process::ExitCode;

use rw_store::diff::{Difference, build_matches, compare, read_writer_build};

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

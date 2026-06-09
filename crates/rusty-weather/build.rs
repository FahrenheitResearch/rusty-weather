//! Compile-time build stamp: RW_BUILD_SHA carries the short git SHA (plus
//! `-dirty` when the tree has local changes) into every store written by
//! rw_ingest, so run.json records exactly which build produced it.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let sha = build_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=RW_BUILD_SHA={sha}");
}

fn build_sha() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let rev = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .current_dir(&manifest_dir)
        .output()
        .ok()?;
    if !rev.status.success() {
        return None;
    }
    let mut sha = String::from_utf8(rev.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&manifest_dir)
        .output()
        .ok()?;
    if !status.status.success() {
        return None;
    }
    if status.stdout.iter().any(|byte| !byte.is_ascii_whitespace()) {
        sha.push_str("-dirty");
    }
    Some(sha)
}

use std::fs;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("..")
}

fn read_repo_file(relative_path: &str) -> String {
    fs::read_to_string(repo_root().join(relative_path))
        .unwrap_or_else(|err| panic!("failed to read {relative_path}: {err}"))
}

#[test]
fn repo_docs_describe_contour_and_proof_boundaries() {
    let readme = read_repo_file("README.md");
    let architecture = read_repo_file("ARCHITECTURE.md");

    assert!(
        readme.contains("current map contour overlays are still the existing `rustwx-render` path"),
        "README should keep the current contour implementation explicit"
    );
    assert!(
        readme.contains("it is not yet the shared live map contour backend"),
        "README should keep rustwx-contour integration marked as future work"
    );
    assert!(
        architecture.contains("current plotted map contours still come from `rustwx-render`"),
        "ARCHITECTURE should distinguish current contour rendering from future contour integration"
    );
}

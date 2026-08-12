use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn checked_in_json(relative_path: &str) -> Value {
    let path = repository_root().join(relative_path);
    let bytes = fs::read(&path)
        .unwrap_or_else(|error| panic!("failed to read checked-in {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("checked-in {} is not valid JSON: {error}", path.display()))
}

fn assert_contract_current(
    name: &str,
    relative_path: &str,
    generated: Value,
    regenerate_command: &str,
) {
    let checked_in = checked_in_json(relative_path);
    assert!(
        checked_in == generated,
        "{name} contract is stale: {relative_path} differs from rw-server's generator.\n\
         Regenerate it with:\n  {regenerate_command}"
    );
}

#[test]
fn checked_in_config_and_openapi_contracts_match_generators() {
    assert_contract_current(
        "configuration schema",
        "config/rusty-weather.schema.json",
        serde_json::to_value(rw_server::config_schema_document())
            .expect("configuration schema must serialize"),
        "cargo run --locked -p rw-server -- config-schema > config/rusty-weather.schema.json",
    );
    assert_contract_current(
        "OpenAPI",
        "config/rusty-weather.openapi.json",
        serde_json::to_value(rw_server::openapi::document())
            .expect("OpenAPI document must serialize"),
        "cargo run --locked -p rw-server -- openapi > config/rusty-weather.openapi.json",
    );
}

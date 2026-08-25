use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

const HTTP_METHODS: &[&str] = &["get", "post", "put", "delete", "patch"];

fn document() -> Value {
    serde_json::to_value(rw_server::openapi::document()).expect("OpenAPI must serialize")
}

fn documented_operations(value: &Value) -> BTreeSet<(String, String)> {
    let mut operations = BTreeSet::new();
    for (path, item) in value["paths"]
        .as_object()
        .expect("OpenAPI paths must be an object")
    {
        for method in HTTP_METHODS {
            if item.get(*method).is_some() {
                operations.insert((method.to_ascii_uppercase(), path.clone()));
            }
        }
    }
    operations
}

fn operation<'a>(value: &'a Value, method: &str, path: &str) -> &'a Value {
    &value["paths"][path][method.to_ascii_lowercase()]
}

fn manifest() -> BTreeSet<(String, String)> {
    rw_server::routes::PRODUCTION_ROUTE_MANIFEST
        .iter()
        .map(|(method, path)| ((*method).to_owned(), (*path).to_owned()))
        .collect()
}

#[test]
fn generated_openapi_exactly_covers_the_explicit_axum_production_manifest() {
    let expected = manifest();
    assert_eq!(
        expected.len(),
        rw_server::routes::PRODUCTION_ROUTE_MANIFEST.len(),
        "production route manifest contains duplicate method/path pairs"
    );
    // 90 before the model binary plane route (GET
    // /v1/models/{model}/runs/{run}/planes/{storage_slot}/{variable}) was added.
    assert_eq!(expected.len(), 91, "unexpected production route count");

    let actual = documented_operations(&document());
    let missing = expected.difference(&actual).cloned().collect::<Vec<_>>();
    let spec_only = actual.difference(&expected).cloned().collect::<Vec<_>>();
    assert!(
        missing.is_empty() && spec_only.is_empty(),
        "OpenAPI/Axum manifest drift\nmissing from OpenAPI: {missing:#?}\nnot registered by Axum: {spec_only:#?}"
    );
}

#[test]
fn every_template_variable_is_a_required_path_parameter() {
    let value = document();
    for (method, path) in manifest() {
        let expected = path
            .split('/')
            .filter_map(|segment| segment.strip_prefix('{')?.strip_suffix('}'))
            .collect::<BTreeSet<_>>();
        let documented = operation(&value, &method, &path)["parameters"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|parameter| parameter["in"] == "path")
            .map(|parameter| {
                assert_eq!(
                    parameter["required"], true,
                    "{method} {path} has a non-required path parameter"
                );
                parameter["name"]
                    .as_str()
                    .expect("path parameter name must be a string")
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            documented, expected,
            "{method} {path} path parameters do not match the Axum template"
        );
    }
}

#[test]
fn observation_and_satellite_contracts_pin_auth_media_cache_and_provenance() {
    let value = document();
    let observation_operations = [
        ("get", "/v1/observations/capabilities"),
        ("get", "/v1/observations"),
        ("get", "/v1/observations/{model}/{run}/frames"),
        ("get", "/v1/observations/{model}/{run}/grid.bin"),
        (
            "get",
            "/v1/observations/{model}/{run}/frames/{storage_slot}/{variable}",
        ),
        ("post", "/v1/observations/mrms/latest"),
        ("post", "/v1/observations/nexrad/level2"),
        ("post", "/v1/observations/radar/mosaic"),
        ("post", "/v1/observations/wrf-radar/derive"),
        ("post", "/v1/observations/generated"),
    ];
    let satellite_operations = [
        ("get", "/v1/satellite/catalog"),
        ("get", "/v1/satellite/prewarm/status"),
        ("get", "/v1/satellite/{platform}/{sector}/{product}/frames"),
        (
            "get",
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json",
        ),
        (
            "get",
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}",
        ),
        (
            "get",
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{z}/{x}/{y}",
        ),
        (
            "get",
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{source_revision}/{z}/{x}/{y}",
        ),
    ];
    for (method, path) in observation_operations
        .into_iter()
        .chain(satellite_operations)
    {
        assert_eq!(
            operation(&value, method, path)["security"][0]["bearer_auth"],
            serde_json::json!([]),
            "{method} {path} must document data bearer authentication"
        );
    }

    assert_eq!(
        operation(&value, "get", "/v1/observations/{model}/{run}/grid.bin")["responses"]["200"]["content"]
            ["application/vnd.rusty-weather.observation-grid+f32"]["schema"]["$ref"],
        "#/components/schemas/ObservationGridBinaryDoc"
    );
    let plane = &operation(
        &value,
        "get",
        "/v1/observations/{model}/{run}/frames/{storage_slot}/{variable}",
    )["responses"]["200"];
    assert_eq!(
        plane["content"]["application/vnd.rusty-weather.observation-plane+f32"]["schema"]["$ref"],
        "#/components/schemas/ObservationPlaneBinaryDoc"
    );
    for header in [
        "Cache-Control",
        "ETag",
        "x-rw-observation-semantics",
        "x-rw-observation-interpolation",
        "x-rw-observation-palette",
        "x-rw-nodata",
    ] {
        assert!(plane["headers"][header].is_object(), "plane omits {header}");
    }

    let writes = [
        ("/v1/observations/mrms/latest", "application/json"),
        ("/v1/observations/nexrad/level2", "application/octet-stream"),
        ("/v1/observations/radar/mosaic", "application/json"),
        ("/v1/observations/wrf-radar/derive", "application/json"),
        ("/v1/observations/generated", "application/json"),
    ];
    for (path, media_type) in writes {
        let write = operation(&value, "post", path);
        assert!(write["requestBody"]["content"][media_type].is_object());
        assert_eq!(
            write["responses"]["202"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/JobView"
        );
    }
    for schema in [
        "GeneratedObservationFrameRequestDoc",
        "GeneratedObservationPlaneRequestDoc",
    ] {
        assert_eq!(
            value["components"]["schemas"][schema]["additionalProperties"], false,
            "{schema} must reject fields that the runtime DTO rejects"
        );
    }

    let frame_schema = &value["components"]["schemas"]["SatelliteFrameDescriptorDoc"];
    assert!(frame_schema["properties"]["source_revision"].is_object());
    let tilejson = operation(
        &value,
        "get",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json",
    );
    assert!(tilejson["responses"]["304"].is_object());
    for field in ["rendererRecipe", "frame", "sourceRevision", "attribution"] {
        assert!(
            value["components"]["schemas"]["SatelliteTileJsonResponseDoc"]["properties"][field]
                .is_object(),
            "TileJSON schema omits {field}"
        );
    }
    for header in [
        "Cache-Control",
        "ETag",
        "Vary",
        "x-rw-satellite-frame",
        "x-rw-satellite-recipe",
        "x-rw-satellite-source-revision",
    ] {
        assert!(
            tilejson["responses"]["200"]["headers"][header].is_object(),
            "TileJSON omits {header}"
        );
    }

    let model_plane = operation(
        &value,
        "get",
        "/v1/models/{model}/runs/{run}/planes/{storage_slot}/{variable}",
    );
    assert_eq!(
        model_plane["security"][0]["bearer_auth"],
        serde_json::json!([]),
        "the model plane route must document the same data bearer authentication as the observation plane route"
    );
    assert_eq!(
        model_plane["responses"]["200"]["content"]["application/vnd.rusty-weather.model-plane+f32"]
            ["schema"]["$ref"],
        "#/components/schemas/ModelPlaneBinaryDoc"
    );
    for header in [
        "Cache-Control",
        "ETag",
        "x-rw-model-variable",
        "x-rw-model-units",
        "x-rw-model-codec",
        "x-rw-valid-unix",
        "x-rw-model-level-hpa",
        "x-rw-nodata",
    ] {
        assert!(
            model_plane["responses"]["200"]["headers"][header].is_object(),
            "model plane omits {header}"
        );
    }
    // The immutable directive is only honest because the URL pins one run
    // generation, so the identity guards must be documented as required.
    let query_parameters = model_plane["parameters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|parameter| parameter["in"] == "query")
        .map(|parameter| {
            (
                parameter["name"].as_str().expect("query parameter name"),
                parameter["required"] == true,
            )
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        query_parameters,
        BTreeMap::from([
            ("expected_grid_hash", true),
            ("expected_snapshot_id", true),
            ("level_hpa", false),
        ])
    );
    assert!(
        model_plane["responses"]["200"]["headers"]["Cache-Control"]["description"]
            .as_str()
            .expect("cache policy must be described")
            .contains("immutable")
    );
    // The route exists to serve whole forecast fields; it must not advertise a
    // cell ceiling the way the JSON window routes do.
    let model_plane_description = model_plane["description"]
        .as_str()
        .expect("the model plane operation must describe its contract");
    assert!(model_plane_description.contains("no cell ceiling"));
    assert!(model_plane_description.contains("zstd1_affine_i16"));

    let mut cache_descriptions = BTreeMap::new();
    for path in [
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{z}/{x}/{y}",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{source_revision}/{z}/{x}/{y}",
    ] {
        let tile = operation(&value, "get", path);
        assert_eq!(
            tile["responses"]["200"]["content"]["image/png"]["schema"]["$ref"],
            "#/components/schemas/SatellitePngTileDoc"
        );
        assert!(tile["responses"]["200"]["headers"]["x-rw-satellite-source-revision"].is_object());
        for status in ["200", "304"] {
            for header in [
                "Cache-Control",
                "ETag",
                "Vary",
                "x-rw-satellite-frame",
                "x-rw-valid-unix",
                "x-rw-satellite-recipe",
                "x-rw-satellite-source-revision",
            ] {
                assert!(
                    tile["responses"][status]["headers"][header].is_object(),
                    "tile {path} {status} omits {header}"
                );
            }
        }
        cache_descriptions.insert(
            path,
            tile["responses"]["200"]["headers"]["Cache-Control"]["description"]
                .as_str()
                .expect("cache policy must be described"),
        );
    }
    assert!(
        cache_descriptions
            .values()
            .any(|text| text.contains("immutable"))
    );
    assert!(
        cache_descriptions
            .values()
            .any(|text| text.contains("must-revalidate"))
    );
    assert!(
        cache_descriptions
            .values()
            .all(|text| text.contains("no-store"))
    );
}

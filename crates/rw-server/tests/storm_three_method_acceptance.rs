use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rustwx_core::{GridShape, LatLonGrid};
use rw_nexrad_storm::{
    DecodeOptions, DerivedGeometryProvenance, Level2DerivedGeometryRef, NexradStormProduct,
    PairingOptions, SuppliedGeometry, decode_with_options, pair_geometry,
};
use rw_observations::{
    GridPlane, ObservationFamily, ObservationFrame, StoredFrameRef,
    write_observation_frame_with_limit,
};
use rw_ops_protocol::{
    ModelInputSource, NEXRAD_LEVEL3_STORM_DECODE_PATH, STORM_CELL_FRAME_SCHEMA, STORM_CELLS_PATH,
    STORM_MODEL_MANIFEST_SCHEMA, STORM_MODELS_PATH, StormCellFrame, StormMethodKind,
    StormModelBackend, StormModelInput, StormModelManifest, StormSource,
};
use rw_server::{AppConfig, AppState, TokenSet, build_router};
use rw_storm::{DetectionConfig, GeographicGrid, detect_geographic};
use rw_storm_ml::{
    DistributionAudience, GridGeometry, MaskOutput, ModelInputBatch, ModelInputPlane, ModelKey,
    ModelLimits, ModelRegistry, ModelUsePolicy, RegistryError, canonicalize_supplied_mask,
    validate_model_inputs,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tower::ServiceExt as _;

const READ_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";
const PRODUCT: &str = "MergedReflectivityQCComposite";
const VALID_UNIX: i64 = 1_700_000_000;
const STORM_REQUEST_SCHEMA: &str = "rw.server.storm-cells-request.v1";
const LEVEL3_REQUEST_SCHEMA: &str = "rw.server.nexrad-level3-storm-decode-request.v1";
const STORM_GEOJSON_SCHEMA: &str = "rw.ops.storm-cell-geojson.v1";
const PROOF_MODEL_ID: &str = "proof-only-supplied-mask";
const PROOF_V1: &str = "acceptance-v1";
const PROOF_V2: &str = "acceptance-v2";
const P58_SHA256: &str = "5b580c1664f5b49ab4f09832655430155f751486e2c1ff398a5d0c74cbbc2c5e";
const P58_BASE64: &str = include_str!("fixtures/noaa-kgld-p58-20260823t0845.sn.b64");

const ARTIFACT_V1: &[u8] =
    b"proof-only supplied mask acceptance artifact v1; inert bytes, never executable";
const ARTIFACT_V2: &[u8] =
    b"proof-only supplied mask acceptance artifact v2; inert bytes, never executable";

struct HttpFixture {
    _directory: TempDir,
    app: Router,
    request: Value,
}

#[test]
fn deterministic_native_grid_is_canonical_missing_honest_and_oirt_provenanced() {
    let width = 256_usize;
    let height = 192_usize;
    let longitudes = (0..width)
        .map(|column| -100.0 + column as f64 * 0.01)
        .collect::<Vec<_>>();
    let latitudes = (0..height)
        .map(|row| 32.0 + row as f64 * 0.01)
        .collect::<Vec<_>>();
    let mut reflectivity = vec![5.0_f32; width * height];
    for row in 32..160 {
        for column in 48..208 {
            reflectivity[row * width + column] = 52.0;
        }
    }
    for row in 80..112 {
        for column in 112..144 {
            reflectivity[row * width + column] = f32::NAN;
        }
    }

    let source = mrms_source("acceptance-native-grid");
    let config = DetectionConfig {
        minimum_gate_count: 1,
        minimum_area_km2: 0.0,
        ..DetectionConfig::default()
    };
    let started = Instant::now();
    let first = detect_geographic(
        source.clone(),
        VALID_UNIX * 1_000,
        GeographicGrid {
            values_dbz: &reflectivity,
            longitudes: &longitudes,
            latitudes: &latitudes,
        },
        config,
    )
    .expect("native-resolution deterministic detection");
    let elapsed = started.elapsed();
    let second = detect_geographic(
        source.clone(),
        VALID_UNIX * 1_000,
        GeographicGrid {
            values_dbz: &reflectivity,
            longitudes: &longitudes,
            latitudes: &latitudes,
        },
        config,
    )
    .expect("repeat deterministic detection");

    first.validate().expect("canonical storm frame");
    assert_eq!(first.schema, STORM_CELL_FRAME_SCHEMA);
    assert_eq!(first.method.kind, StormMethodKind::Deterministic);
    assert_eq!(
        first.method.parameters["contour_engine"],
        "weather_contours_0.2.0_oirt"
    );
    assert_eq!(
        first.method.parameters["grid_point_count"],
        (width * height).to_string(),
        "every supplied native-grid sample must be considered"
    );
    assert_eq!(
        first.method.parameters["missing_data_policy"],
        "non_finite_or_out_of_range_excluded_and_contoured_below_threshold"
    );
    assert_eq!(first.cells.len(), 1);
    assert!(first.cells[0].rings.iter().any(|ring| ring.hole));
    assert_eq!(first.warnings.len(), 1);
    assert!(first.warnings[0].starts_with("1024 non-finite"));
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap(),
        "same source, timestamp, grid, and policy must produce byte-stable canonical JSON"
    );

    let contour_points = first.cells[0]
        .rings
        .iter()
        .map(|ring| ring.points.len())
        .sum::<usize>();
    eprintln!(
        "storm acceptance contour evidence: native_points={}, cells={}, contour_points={}, elapsed_ms={} (observational only; no flaky timing threshold)",
        width * height,
        first.cells.len(),
        contour_points,
        elapsed.as_millis()
    );
}

#[test]
fn proof_only_supplied_mask_lifecycle_executes_rolls_back_and_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let registry_root = absolute(directory.path().join("storm-models"));
    let mut registry = ModelRegistry::open(&registry_root, ModelLimits::default()).unwrap();
    let key_v1 = ModelKey::new(PROOF_MODEL_ID, PROOF_V1).unwrap();
    let key_v2 = ModelKey::new(PROOF_MODEL_ID, PROOF_V2).unwrap();

    registry
        .install(
            proof_manifest(PROOF_V1, ARTIFACT_V1),
            proof_policy(),
            ARTIFACT_V1,
        )
        .unwrap();
    assert!(matches!(
        registry.enabled_for_execution(&key_v1),
        Err(RegistryError::Disabled(key)) if key == key_v1
    ));
    registry.enable(&key_v1).unwrap();
    registry.activate(&key_v1).unwrap();
    assert_eq!(
        registry.active_for_execution(PROOF_MODEL_ID).unwrap().key,
        key_v1
    );

    registry
        .install(
            proof_manifest(PROOF_V2, ARTIFACT_V2),
            proof_policy(),
            ARTIFACT_V2,
        )
        .unwrap();
    registry.enable(&key_v2).unwrap();
    registry.activate(&key_v2).unwrap();
    assert_eq!(
        registry.active_for_execution(PROOF_MODEL_ID).unwrap().key,
        key_v2
    );
    assert_eq!(registry.rollback(PROOF_MODEL_ID).unwrap().key, key_v1);
    drop(registry);

    let registry = ModelRegistry::open(&registry_root, ModelLimits::default()).unwrap();
    let installed = registry
        .active_for_execution(PROOF_MODEL_ID)
        .expect("restart must verify the immutable artifact digest");
    assert_eq!(installed.key, key_v1);
    assert!(
        installed
            .manifest
            .description
            .contains("not a trained or production model")
    );
    assert!(
        installed
            .manifest
            .training_provenance
            .as_deref()
            .unwrap()
            .contains("No training occurred")
    );

    let longitudes = [-98.0, -97.9, -97.8, -97.7, -97.6];
    let latitudes = [35.0, 35.1, 35.2, 35.3, 35.4];
    let reflectivity = vec![45.0_f32; 25];
    let source = mrms_source("acceptance-model-grid");
    let correct_plane = ModelInputPlane {
        name: "reflectivity",
        source: ModelInputSource::MrmsProduct,
        field: "mrms_reflectivity",
        units: "dBZ",
        values: &reflectivity,
    };
    let correct_batch = ModelInputBatch {
        source: &source,
        geometry: GridGeometry::Geographic {
            longitudes: &longitudes,
            latitudes: &latitudes,
        },
        planes: std::slice::from_ref(&correct_plane),
    };
    validate_model_inputs(installed, correct_batch, registry.limits()).unwrap();

    let wrong_units = ModelInputPlane {
        units: "m/s",
        ..correct_plane.clone()
    };
    let wrong_input = validate_model_inputs(
        installed,
        ModelInputBatch {
            planes: std::slice::from_ref(&wrong_units),
            ..correct_batch
        },
        registry.limits(),
    );
    assert!(matches!(
        wrong_input,
        Err(RegistryError::IncompatibleInput(_))
    ));

    let level2_source = StormSource::NexradLevel2 {
        site: "KTLX".into(),
        volume_at_unix_ms: VALID_UNIX * 1_000,
        elevation_degrees_milli: 500,
        moment: "REF".into(),
    };
    let wrong_source = validate_model_inputs(
        installed,
        ModelInputBatch {
            source: &level2_source,
            ..correct_batch
        },
        registry.limits(),
    );
    assert!(matches!(
        wrong_source,
        Err(RegistryError::IncompatibleInput(_))
    ));

    let probabilities = [
        0.0, 0.0, 0.0, 0.0, 0.0, //
        0.0, 0.9, 0.9, 0.9, 0.0, //
        0.0, 0.9, 0.9, 0.9, 0.0, //
        0.0, 0.9, 0.9, 0.9, 0.0, //
        0.0, 0.0, 0.0, 0.0, 0.0,
    ];
    let frame = canonicalize_supplied_mask(
        &registry,
        &key_v1,
        source.clone(),
        VALID_UNIX * 1_000,
        correct_batch.geometry,
        MaskOutput::Probabilities {
            width: 5,
            height: 5,
            values: &probabilities,
        },
        DistributionAudience::CompanyCoworker,
    )
    .unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.method.kind, StormMethodKind::MachineLearning);
    assert_eq!(frame.method.model_id.as_deref(), Some(PROOF_MODEL_ID));
    assert_eq!(frame.method.model_version.as_deref(), Some(PROOF_V1));
    assert_eq!(frame.method.parameters["backend"], "supplied_mask");
    assert_eq!(
        frame.method.parameters["contour_engine"],
        "rw-storm_weather-contours_oirt"
    );
    assert_eq!(frame.cells.len(), 1);
    assert_eq!(frame.cells[0].maximum_reflectivity_dbz, None);
    assert_eq!(
        frame.cells[0].attributes["geometry_provenance"],
        "model_probability_threshold_contour"
    );

    let mismatched_grid = canonicalize_supplied_mask(
        &registry,
        &key_v1,
        source.clone(),
        VALID_UNIX * 1_000,
        correct_batch.geometry,
        MaskOutput::Probabilities {
            width: 4,
            height: 5,
            values: &probabilities[..20],
        },
        DistributionAudience::CompanyCoworker,
    );
    assert!(matches!(
        mismatched_grid,
        Err(RegistryError::InvalidOutput(_))
    ));
}

#[test]
fn real_noaa_p58_tracks_remain_authoritative_points_and_never_polygons() {
    let bytes = p58_bytes();
    assert_eq!(sha256(&bytes), P58_SHA256);
    let product = decode_with_options(
        &bytes,
        &DecodeOptions {
            site_hint: Some("KGLD".into()),
            ..DecodeOptions::default()
        },
    )
    .expect("real NOAA/NCEI Level III message 58 must decode");
    let NexradStormProduct::StormTracking(tracking) = &product else {
        panic!("fixture must remain Level III message 58")
    };

    assert_eq!(tracking.identity.message_code, 58);
    assert_eq!(
        tracking.identity.radar_site.site_id.as_deref(),
        Some("KGLD")
    );
    assert_eq!(tracking.cells.len(), 32);
    assert_eq!(
        tracking.identity.provenance.supplied_geometry,
        SuppliedGeometry::CentroidPointsAndTracks
    );
    assert!(
        tracking
            .identity
            .provenance
            .geometry_statement
            .contains("does not supply storm polygons")
    );
    assert!(
        tracking
            .cells
            .iter()
            .any(|cell| !cell.history_in_packet_order.is_empty())
    );
    assert!(tracking.cells.iter().any(|cell| !cell.forecasts.is_empty()));
    assert!(
        tracking
            .cells
            .iter()
            .flat_map(|cell| &cell.history_in_packet_order)
            .all(|point| point.valid_at_unix_ms.is_none()),
        "message 58 has no exact timestamp per historical point; none may be invented"
    );
    assert!(
        tracking
            .cells
            .iter()
            .all(|cell| cell.current.valid_at_unix_ms.is_some())
    );

    let json_once = serde_json::to_vec(&product).unwrap();
    let round_trip: NexradStormProduct = serde_json::from_slice(&json_once).unwrap();
    assert_eq!(round_trip.identity().message_code, 58);
    let NexradStormProduct::StormTracking(round_trip) = round_trip else {
        panic!("typed JSON round trip changed the product variant")
    };
    assert_eq!(round_trip.cells.len(), tracking.cells.len());
    let value = serde_json::to_value(&product).unwrap();
    for cell in value["cells"].as_array().unwrap() {
        assert!(cell.get("rings").is_none());
        assert!(cell.get("polygon").is_none());
        assert!(cell.get("geometry").is_none());
    }

    let authoritative = &tracking.cells[0];
    let derived = Level2DerivedGeometryRef {
        geometry_id: "derived-level2-cell-proof".into(),
        site_id: "KGLD".into(),
        volume_scan_at_unix_ms: tracking.identity.volume_scan_at_unix_ms,
        centroid: authoritative.current.geographic,
        provenance: DerivedGeometryProvenance {
            source_kind: "nexrad_level_ii".into(),
            source_id: "KGLD-exact-volume-proof".into(),
            method_id: "rw-deterministic-reflectivity-components".into(),
            method_version: "1".into(),
            moment: "REF".into(),
        },
    };
    let paired = pair_geometry(
        tracking,
        std::slice::from_ref(&derived),
        PairingOptions::default(),
    )
    .unwrap();
    assert_eq!(paired.associations.len(), 1);
    assert_eq!(
        paired.associations[0].derived_geometry.provenance,
        derived.provenance
    );
    assert!(
        paired.associations[0]
            .provenance_statement
            .contains("not a NOAA/RPG polygon")
    );
}

#[tokio::test]
async fn production_http_keeps_all_three_methods_separate_and_json_geojson_stable() {
    let fixture = http_fixture();

    let mut deterministic_request = fixture.request.clone();
    deterministic_request["method"] = json!({"kind": "deterministic"});
    let deterministic_a = post_json(
        &fixture.app,
        STORM_CELLS_PATH,
        Some(READ_TOKEN),
        &deterministic_request,
    )
    .await;
    let deterministic_b = post_json(
        &fixture.app,
        STORM_CELLS_PATH,
        Some(READ_TOKEN),
        &deterministic_request,
    )
    .await;
    assert_eq!(deterministic_a.0, StatusCode::OK);
    assert_eq!(deterministic_a.1, deterministic_b.1);
    let deterministic: StormCellFrame = serde_json::from_slice(&deterministic_a.1).unwrap();
    deterministic.validate().unwrap();
    assert_eq!(deterministic.method.kind, StormMethodKind::Deterministic);
    assert_eq!(
        deterministic.method.parameters["contour_engine"],
        "weather_contours_0.2.0_oirt"
    );

    let geojson_uri = format!("{STORM_CELLS_PATH}?format=geojson");
    let geojson_a = post_json(
        &fixture.app,
        &geojson_uri,
        Some(READ_TOKEN),
        &deterministic_request,
    )
    .await;
    let geojson_b = post_json(
        &fixture.app,
        &geojson_uri,
        Some(READ_TOKEN),
        &deterministic_request,
    )
    .await;
    assert_eq!(geojson_a.0, StatusCode::OK);
    assert_eq!(geojson_a.1, geojson_b.1);
    let geojson: Value = serde_json::from_slice(&geojson_a.1).unwrap();
    assert_eq!(geojson["schema"], STORM_GEOJSON_SCHEMA);
    assert_eq!(geojson["type"], "FeatureCollection");
    assert_eq!(
        geojson["source"],
        serde_json::to_value(&deterministic.source).unwrap()
    );
    assert_eq!(
        geojson["method"],
        serde_json::to_value(&deterministic.method).unwrap()
    );
    assert_eq!(
        geojson["generated_at_unix_ms"],
        deterministic.generated_at_unix_ms
    );
    assert_eq!(geojson["features"][0]["id"], deterministic.cells[0].cell_id);

    let mut ml_request = fixture.request.clone();
    ml_request["method"] = json!({
        "kind": "machine_learning",
        "model_id": PROOF_MODEL_ID,
        "model_version": PROOF_V1,
        "supplied_mask_variable": "storm_probability"
    });
    let ml_response = post_json(
        &fixture.app,
        STORM_CELLS_PATH,
        Some(READ_TOKEN),
        &ml_request,
    )
    .await;
    assert_eq!(ml_response.0, StatusCode::OK);
    let ml: StormCellFrame = serde_json::from_slice(&ml_response.1).unwrap();
    ml.validate().unwrap();
    assert_eq!(ml.method.kind, StormMethodKind::MachineLearning);
    assert_eq!(ml.method.model_id.as_deref(), Some(PROOF_MODEL_ID));
    assert_eq!(ml.method.model_version.as_deref(), Some(PROOF_V1));
    assert!(
        ml.method
            .description
            .contains("not a trained or production model")
    );

    let models = get(&fixture.app, STORM_MODELS_PATH, Some(READ_TOKEN)).await;
    assert_eq!(models.0, StatusCode::OK);
    let models: Value = serde_json::from_slice(&models.1).unwrap();
    let proof_model = models["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["manifest"]["model_version"] == PROOF_V1)
        .unwrap();
    assert_eq!(proof_model["enabled"], true);
    assert_eq!(proof_model["active"], true);
    assert!(
        proof_model["manifest"]["training_provenance"]
            .as_str()
            .unwrap()
            .contains("No training occurred")
    );

    let level3_request = json!({
        "schema": LEVEL3_REQUEST_SCHEMA,
        "site_hint": "KGLD",
        "product_base64": BASE64_STANDARD.encode(p58_bytes())
    });
    let authoritative_response = post_json(
        &fixture.app,
        NEXRAD_LEVEL3_STORM_DECODE_PATH,
        Some(READ_TOKEN),
        &level3_request,
    )
    .await;
    assert_eq!(authoritative_response.0, StatusCode::OK);
    let authoritative: Value = serde_json::from_slice(&authoritative_response.1).unwrap();
    assert_eq!(authoritative["method"]["kind"], "authoritative");
    assert_eq!(
        authoritative["method"]["method_id"],
        "noaa-nexrad-level3-nst-sti"
    );
    assert_eq!(
        authoritative["method"]["parameters"]["polygon_geometry"],
        "not_supplied_by_level3_product"
    );
    assert!(
        authoritative["geometry_statement"]
            .as_str()
            .unwrap()
            .contains("does not supply storm polygons")
    );
    assert_eq!(
        authoritative["product"]["cells"].as_array().unwrap().len(),
        32
    );

    let method_ids = [
        deterministic.method.method_id.as_str(),
        ml.method.method_id.as_str(),
        authoritative["method"]["method_id"].as_str().unwrap(),
    ];
    assert_ne!(method_ids[0], method_ids[1]);
    assert_ne!(method_ids[0], method_ids[2]);
    assert_ne!(method_ids[1], method_ids[2]);
}

fn http_fixture() -> HttpFixture {
    let directory = tempfile::tempdir().unwrap();
    let store_root = directory.path().join("store");
    let artifact_root = directory.path().join("artifacts");
    let operations_root = directory.path().join("operations");
    fs::create_dir_all(&store_root).unwrap();
    fs::create_dir_all(&artifact_root).unwrap();
    fs::create_dir_all(&operations_root).unwrap();

    let stored = write_stored_frame(&store_root);
    install_http_proof_models(&operations_root);
    let token_path = directory.path().join("ops-read.tokens");
    write_private_file(&token_path, READ_TOKEN.as_bytes());

    let mut config = AppConfig::default();
    config.server.store_root = store_root;
    config.server.artifact_root = artifact_root;
    config.server.cache_root = directory.path().join("cache");
    config.operations.enabled = true;
    config.operations.root = operations_root;
    config.auth.ops_read_token_file = Some(token_path);
    config.validate(false).unwrap();
    let state = AppState::new(config, TokenSet::default()).unwrap();
    let descriptor = state
        .catalog
        .snapshot(&stored.model, &stored.run)
        .unwrap()
        .descriptor()
        .clone();
    let request = json!({
        "schema": STORM_REQUEST_SCHEMA,
        "grid": {
            "model": stored.model,
            "run": stored.run,
            "expected_snapshot_id": descriptor.snapshot_id,
            "expected_grid_hash": descriptor.grid_hash,
            "storage_slot": stored.storage_slot,
            "variable": "mrms_reflectivity"
        },
        "source": {
            "kind": "mrms",
            "product": PRODUCT,
            "valid_at_unix_ms": VALID_UNIX * 1_000,
            "grid_hash": stored.grid_hash
        },
        "method": {"kind": "deterministic"}
    });
    HttpFixture {
        _directory: directory,
        app: build_router(state).unwrap(),
        request,
    }
}

fn write_stored_frame(store_root: &Path) -> StoredFrameRef {
    let width = 7_usize;
    let height = 7_usize;
    let mut latitudes = Vec::with_capacity(width * height);
    let mut longitudes = Vec::with_capacity(width * height);
    for row in 0..height {
        for column in 0..width {
            latitudes.push(35.0 + row as f32 * 0.1);
            longitudes.push(-98.0 + column as f32 * 0.1);
        }
    }
    let grid = LatLonGrid::new(
        GridShape::new(width, height).unwrap(),
        latitudes,
        longitudes,
    )
    .unwrap();
    let mut reflectivity = vec![10.0_f32; width * height];
    let mut probability = vec![0.0_f32; width * height];
    for row in 1..6 {
        for column in 1..6 {
            reflectivity[row * width + column] = 50.0;
            probability[row * width + column] = 0.9;
        }
    }
    reflectivity[3 * width + 3] = f32::NAN;
    probability[3 * width + 3] = f32::NAN;
    let frame = ObservationFrame {
        family: ObservationFamily::Mrms,
        collection: "conus".into(),
        product: PRODUCT.into(),
        valid_unix: VALID_UNIX,
        grid,
        projection: None,
        planes: vec![
            GridPlane {
                name: "mrms_reflectivity".into(),
                units: "dBZ".into(),
                selector: json!({
                    "mrms": {
                        "product": PRODUCT,
                        "parameter_name": "ReflectivityAtLowestAltitude"
                    }
                }),
                values: reflectivity,
            },
            GridPlane {
                name: "storm_probability".into(),
                units: "1".into(),
                selector: json!({"derived": {"field": "storm_probability"}}),
                values: probability,
            },
        ],
        provenance_provider: "noaa-mrms".into(),
        provenance_roles: vec!["radar".into(), "mosaic".into()],
        provenance_products: vec!["merged-reflectivity-qc-composite".into()],
    };
    write_observation_frame_with_limit(store_root, &frame, width * height).unwrap()
}

fn install_http_proof_models(operations_root: &Path) {
    let root = absolute(operations_root.join("storm-models"));
    let mut registry = ModelRegistry::open(root, ModelLimits::default()).unwrap();
    let key_v1 = ModelKey::new(PROOF_MODEL_ID, PROOF_V1).unwrap();
    let key_v2 = ModelKey::new(PROOF_MODEL_ID, PROOF_V2).unwrap();
    registry
        .install(
            proof_manifest(PROOF_V1, ARTIFACT_V1),
            proof_policy(),
            ARTIFACT_V1,
        )
        .unwrap();
    registry.enable(&key_v1).unwrap();
    registry.activate(&key_v1).unwrap();
    registry
        .install(
            proof_manifest(PROOF_V2, ARTIFACT_V2),
            proof_policy(),
            ARTIFACT_V2,
        )
        .unwrap();
    registry.enable(&key_v2).unwrap();
    registry.activate(&key_v2).unwrap();
    assert_eq!(registry.rollback(PROOF_MODEL_ID).unwrap().key, key_v1);
}

fn proof_manifest(version: &str, artifact: &[u8]) -> StormModelManifest {
    StormModelManifest {
        schema: STORM_MODEL_MANIFEST_SCHEMA.into(),
        model_id: PROOF_MODEL_ID.into(),
        model_version: version.into(),
        backend: StormModelBackend::SuppliedMask,
        artifact_sha256: sha256(artifact),
        display_name: "Proof-only supplied storm mask".into(),
        description: "Proof-only hand-authored mask used by release acceptance; this is not a trained or production model.".into(),
        inputs: vec![StormModelInput {
            name: "reflectivity".into(),
            source: ModelInputSource::MrmsProduct,
            field: "mrms_reflectivity".into(),
            units: "dBZ".into(),
            minimum: Some(-20.0),
            maximum: Some(90.0),
            missing_value: None,
        }],
        output_name: "storm_probability".into(),
        probability_threshold: 0.5,
        minimum_area_km2: Some(0.0),
        producer: "Fahrenheit Research acceptance test only".into(),
        license: Some("private internal acceptance fixture; no production claim".into()),
        training_provenance: Some(
            "No training occurred; hand-authored synthetic probability mask for test/proof only."
                .into(),
        ),
    }
}

fn proof_policy() -> ModelUsePolicy {
    ModelUsePolicy::private_company(
        "Fahrenheit Research proof-only supplied-mask acceptance fixture",
        "internal-test-proof-only-v1",
    )
}

fn mrms_source(grid_hash: &str) -> StormSource {
    StormSource::Mrms {
        product: PRODUCT.into(),
        valid_at_unix_ms: VALID_UNIX * 1_000,
        grid_hash: grid_hash.into(),
    }
}

fn p58_bytes() -> Vec<u8> {
    let compact = P58_BASE64.lines().collect::<String>();
    BASE64_STANDARD
        .decode(compact)
        .expect("checked-in P58 base64 fixture")
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(path).unwrap()
}

fn write_private_file(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
}

async fn post_json(
    app: &Router,
    uri: &str,
    token: Option<&str>,
    value: &Value,
) -> (StatusCode, Vec<u8>) {
    request(app, Method::POST, uri, token, Some(value)).await
}

async fn get(app: &Router, uri: &str, token: Option<&str>) -> (StatusCode, Vec<u8>) {
    request(app, Method::GET, uri, token, None).await
}

async fn request(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    value: Option<&Value>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let body = if let Some(value) = value {
        builder = builder.header(header::CONTENT_TYPE, "application/json");
        Body::from(serde_json::to_vec(value).unwrap())
    } else {
        Body::empty()
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store, private"
    );
    let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

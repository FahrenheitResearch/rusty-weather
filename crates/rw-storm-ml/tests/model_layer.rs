use std::fs;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rw_ops_protocol::{
    GeoPoint, ModelInputSource, STORM_MODEL_MANIFEST_SCHEMA, StormModelBackend, StormModelInput,
    StormModelManifest, StormSource,
};
use rw_storm_ml::{
    DistributionAudience, DistributionGrant, GridGeometry, MaskOutput, ModelInputBatch,
    ModelInputPlane, ModelKey, ModelLimits, ModelRegistry, ModelUsePolicy, NativeBackendRegistry,
    NativeStormModel, OwnedMask, RegistryError, RegistryResult, canonicalize_supplied_mask,
};
use sha2::{Digest, Sha256};

const FIXTURE_ARTIFACT: &[u8] = include_bytes!("fixtures/supplied-mask-v1.artifact");

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new(name: &str) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rw-storm-ml-{name}-{}-{}",
            process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn manifest(version: &str, backend: StormModelBackend, artifact: &[u8]) -> StormModelManifest {
    StormModelManifest {
        schema: STORM_MODEL_MANIFEST_SCHEMA.into(),
        model_id: "fixture-cell-model".into(),
        model_version: version.into(),
        backend,
        artifact_sha256: sha256(artifact),
        display_name: "Fixture cell model".into(),
        description: "Fixture model used to prove immutable installation and canonical geometry."
            .into(),
        inputs: vec![StormModelInput {
            name: "reflectivity".into(),
            source: ModelInputSource::MrmsProduct,
            field: "mrms_reflectivity_lowest_altitude".into(),
            units: "dBZ".into(),
            minimum: Some(-20.0),
            maximum: Some(90.0),
            missing_value: Some(-999.0),
        }],
        output_name: "storm_probability".into(),
        probability_threshold: 0.5,
        minimum_area_km2: Some(0.0),
        producer: "Fahrenheit Research test fixture".into(),
        license: Some("private test fixture; internal derived use allowed".into()),
        training_provenance: Some(
            "synthetic squares; no observational or personal data; fixture revision 1".into(),
        ),
    }
}

fn internal_policy() -> ModelUsePolicy {
    ModelUsePolicy::private_company(
        "Fahrenheit Research fixture model",
        "internal-test-policy-v1",
    )
}

fn public_output_policy() -> ModelUsePolicy {
    ModelUsePolicy {
        artifact_distribution: DistributionGrant::NodeOnly,
        derived_output_distribution: DistributionGrant::Public,
        required_attribution: "Fahrenheit Research fixture model".into(),
        rights_reference: "internal-test-policy-v1-public-output".into(),
    }
}

#[test]
fn default_registry_policy_has_no_arbitrary_catalog_or_grid_ceiling() {
    let limits = ModelLimits::default();
    assert_eq!(limits.max_installed_versions, usize::MAX);
    assert_eq!(limits.max_activation_history, usize::MAX);
    assert_eq!(limits.max_grid_width, usize::MAX);
    assert_eq!(limits.max_grid_height, usize::MAX);
    assert_eq!(limits.max_grid_points, usize::MAX);
    assert_eq!(limits.max_label_work_points, usize::MAX);

    let longitudes = (0..=32_768)
        .map(|index| -180.0 + 360.0 * index as f64 / 32_768.0)
        .collect::<Vec<_>>();
    let latitudes = [-1.0, 1.0];
    assert_eq!(
        GridGeometry::Geographic {
            longitudes: &longitudes,
            latitudes: &latitudes,
        }
        .shape(limits)
        .unwrap(),
        (32_769, 2)
    );

    let explicit = ModelLimits {
        max_grid_width: 32_768,
        ..limits
    };
    assert!(
        GridGeometry::Geographic {
            longitudes: &longitudes,
            latitudes: &latitudes,
        }
        .shape(explicit)
        .is_err()
    );
}

fn source() -> StormSource {
    StormSource::Mrms {
        product: "mrms_reflectivity_lowest_altitude".into(),
        valid_at_unix_ms: 1_788_000_000_000,
        grid_hash: "a".repeat(64),
    }
}

fn key(version: &str) -> ModelKey {
    ModelKey::new("fixture-cell-model", version).unwrap()
}

fn install_enabled(
    root: &Path,
    version: &str,
    backend: StormModelBackend,
    policy: ModelUsePolicy,
) -> ModelRegistry {
    let mut registry = ModelRegistry::open(root, ModelLimits::default()).unwrap();
    registry
        .install(
            manifest(version, backend, FIXTURE_ARTIFACT),
            policy,
            FIXTURE_ARTIFACT,
        )
        .unwrap();
    registry.enable(&key(version)).unwrap();
    registry.activate(&key(version)).unwrap();
    registry
}

#[test]
fn default_model_output_limits_do_not_cap_native_geometry_cardinality() {
    let limits = ModelLimits::default();
    limits.validate().unwrap();
}

#[test]
fn fixture_install_survives_restart_with_exact_active_identity() {
    let directory = TestDirectory::new("restart");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        internal_policy(),
    );
    assert_eq!(registry.installed().len(), 1);
    drop(registry);

    let reopened = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    let active = reopened.active_for_execution("fixture-cell-model").unwrap();
    assert_eq!(active.key, key("1.0.0"));
    assert_eq!(active.manifest.artifact_sha256, sha256(FIXTURE_ARTIFACT));
    assert_eq!(fs::read(active.artifact_path()).unwrap(), FIXTURE_ARTIFACT);
}

#[test]
fn wrong_digest_never_publishes_a_partial_version() {
    let directory = TestDirectory::new("digest-reject");
    let mut registry = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    let mut wrong = manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT);
    wrong.artifact_sha256 = "0".repeat(64);
    assert!(matches!(
        registry.install(wrong, internal_policy(), FIXTURE_ARTIFACT),
        Err(RegistryError::DigestMismatch { .. })
    ));
    assert!(
        !directory
            .path()
            .join("models/fixture-cell-model/1.0.0")
            .exists()
    );
    registry
        .install(
            manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT),
            internal_policy(),
            FIXTURE_ARTIFACT,
        )
        .unwrap();
}

#[test]
fn artifact_corruption_is_detected_on_restart_and_before_execution() {
    let directory = TestDirectory::new("artifact-corrupt");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        internal_policy(),
    );
    let artifact = registry
        .get(&key("1.0.0"))
        .unwrap()
        .artifact_path()
        .to_owned();
    fs::write(&artifact, b"tampered artifact").unwrap();
    assert!(matches!(
        registry.enabled_for_execution(&key("1.0.0")),
        Err(RegistryError::DigestMismatch { .. })
    ));
    drop(registry);
    assert!(matches!(
        ModelRegistry::open(directory.path(), ModelLimits::default()),
        Err(RegistryError::DigestMismatch { .. })
    ));
}

#[test]
fn corrupt_registry_state_fails_closed() {
    let directory = TestDirectory::new("state-corrupt");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        internal_policy(),
    );
    drop(registry);
    fs::write(directory.path().join("registry-state.json"), b"not json").unwrap();
    assert!(matches!(
        ModelRegistry::open(directory.path(), ModelLimits::default()),
        Err(RegistryError::Json(_))
    ));
}

#[test]
fn activation_rollback_is_persistent_and_versions_are_immutable() {
    let directory = TestDirectory::new("rollback");
    let mut registry = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    for version in ["1.0.0", "2.0.0"] {
        let artifact = format!("model-version-{version}");
        registry
            .install(
                manifest(
                    version,
                    StormModelBackend::SuppliedMask,
                    artifact.as_bytes(),
                ),
                internal_policy(),
                artifact.as_bytes(),
            )
            .unwrap();
        registry.enable(&key(version)).unwrap();
        registry.activate(&key(version)).unwrap();
    }
    assert_eq!(
        registry.active("fixture-cell-model").unwrap().key,
        key("2.0.0")
    );
    assert!(matches!(
        registry.install(
            manifest("1.0.0", StormModelBackend::SuppliedMask, b"replacement"),
            internal_policy(),
            b"replacement".as_slice()
        ),
        Err(RegistryError::AlreadyInstalled(_))
    ));
    assert_eq!(
        registry.rollback("fixture-cell-model").unwrap().key,
        key("1.0.0")
    );
    drop(registry);
    let mut reopened = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    assert_eq!(
        reopened.active("fixture-cell-model").unwrap().key,
        key("1.0.0")
    );
    reopened.disable(&key("1.0.0")).unwrap();
    assert!(matches!(
        reopened.active("fixture-cell-model"),
        Err(RegistryError::NoActiveVersion(_))
    ));
    drop(reopened);
    let disabled = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    assert!(!disabled.is_enabled(&key("1.0.0")));
}

#[test]
fn probability_mask_becomes_valid_canonical_geometry() {
    let directory = TestDirectory::new("probability-geometry");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        public_output_policy(),
    );
    let axes = [0.0, 0.1, 0.2, 0.3, 0.4];
    let mut values = vec![0.0_f32; 25];
    for y in 1..=3 {
        for x in 1..=3 {
            values[y * 5 + x] = 0.8;
        }
    }
    let frame = canonicalize_supplied_mask(
        &registry,
        &key("1.0.0"),
        source(),
        1_788_000_000_001,
        GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes,
        },
        MaskOutput::Probabilities {
            width: 5,
            height: 5,
            values: &values,
        },
        DistributionAudience::PublicWebsite,
    )
    .unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.cells.len(), 1);
    assert_eq!(frame.method.model_id.as_deref(), Some("fixture-cell-model"));
    assert_eq!(frame.cells[0].confidence, Some(0.8_f32 as f64));
    assert!(frame.cells[0].maximum_reflectivity_dbz.is_none());
    assert_eq!(
        frame.cells[0].rings[0].points.first(),
        frame.cells[0].rings[0].points.last()
    );
}

#[test]
fn touching_integer_labels_remain_distinct_canonical_cells() {
    let directory = TestDirectory::new("label-geometry");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        internal_policy(),
    );
    let axes = [0.0, 0.1, 0.2, 0.3, 0.4];
    let mut labels = vec![0_u32; 25];
    labels[6] = 10;
    labels[2 * 5 + 1] = 10;
    labels[7] = 20;
    labels[2 * 5 + 2] = 20;
    let frame = canonicalize_supplied_mask(
        &registry,
        &key("1.0.0"),
        source(),
        1_788_000_000_001,
        GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes,
        },
        MaskOutput::Labels {
            width: 5,
            height: 5,
            values: &labels,
        },
        DistributionAudience::CompanyCoworker,
    )
    .unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.cells.len(), 2);
    let labels = frame
        .cells
        .iter()
        .map(|cell| cell.attributes["supplied_label"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(labels, std::collections::BTreeSet::from(["10", "20"]));
}

#[test]
fn level2_cartesian_mask_uses_radar_aware_canonical_geometry() {
    let directory = TestDirectory::new("level2-geometry");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::SuppliedMask,
        internal_policy(),
    );
    let axes = [-1_000.0, 0.0, 1_000.0];
    let values = [0.0, 0.0, 0.0, 0.0, 0.9, 0.0, 0.0, 0.0, 0.0];
    let level2_source = StormSource::NexradLevel2 {
        site: "KTLX".into(),
        volume_at_unix_ms: 1_788_000_000_000,
        elevation_degrees_milli: 500,
        moment: "REF".into(),
    };
    let frame = canonicalize_supplied_mask(
        &registry,
        &key("1.0.0"),
        level2_source,
        1_788_000_000_001,
        GridGeometry::Level2Cartesian {
            east_m: &axes,
            north_m: &axes,
            radar_location: GeoPoint {
                latitude: 35.333,
                longitude: -97.278,
            },
        },
        MaskOutput::Probabilities {
            width: 3,
            height: 3,
            values: &values,
        },
        DistributionAudience::CompanyCoworker,
    )
    .unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.cells.len(), 1);
    assert!(frame.cells[0].centroid.latitude > 35.32);
    assert!(frame.cells[0].centroid.longitude < -97.26);
}

struct ThresholdBackend;

impl NativeStormModel for ThresholdBackend {
    fn infer(
        &self,
        inputs: ModelInputBatch<'_>,
        _limits: ModelLimits,
    ) -> RegistryResult<OwnedMask> {
        let (width, height) = inputs.geometry.shape(ModelLimits::default())?;
        Ok(OwnedMask::Probabilities {
            width,
            height,
            values: inputs.planes[0]
                .values
                .iter()
                .map(|value| if *value >= 35.0 { 0.9 } else { 0.0 })
                .collect(),
        })
    }
}

#[test]
fn compiled_native_backend_is_explicit_and_input_compatible() {
    let directory = TestDirectory::new("native");
    let registry = install_enabled(
        directory.path(),
        "1.0.0",
        StormModelBackend::NativeRust,
        internal_policy(),
    );
    let mut native = NativeBackendRegistry::new();
    native
        .register(&registry, key("1.0.0"), Arc::new(ThresholdBackend))
        .unwrap();
    let axes = [0.0, 0.1, 0.2, 0.3];
    let values = [
        0.0, 0.0, 0.0, 0.0, 0.0, 40.0, 40.0, 0.0, 0.0, 40.0, 40.0, 0.0, 0.0, 0.0, 0.0, 0.0,
    ];
    let planes = [ModelInputPlane {
        name: "reflectivity",
        source: ModelInputSource::MrmsProduct,
        field: "mrms_reflectivity_lowest_altitude",
        units: "dBZ",
        values: &values,
    }];
    let storm_source = source();
    let batch = ModelInputBatch {
        source: &storm_source,
        geometry: GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes,
        },
        planes: &planes,
    };
    let frame = native
        .infer_canonical(
            &registry,
            &key("1.0.0"),
            1_788_000_000_001,
            batch,
            DistributionAudience::CompanyCoworker,
        )
        .unwrap();
    frame.validate().unwrap();
    assert_eq!(frame.cells.len(), 1);

    let incompatible = [ModelInputPlane {
        units: "kelvin",
        ..planes[0].clone()
    }];
    let bad_batch = ModelInputBatch {
        planes: &incompatible,
        ..batch
    };
    assert!(matches!(
        native.infer(
            &registry,
            &key("1.0.0"),
            bad_batch,
            DistributionAudience::CompanyCoworker
        ),
        Err(RegistryError::IncompatibleInput(_))
    ));
}

#[test]
fn path_dimension_probability_and_publication_rejections_fail_closed() {
    let directory = TestDirectory::new("rejections");
    let mut registry = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    let mut unsafe_manifest = manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT);
    unsafe_manifest.model_id = "..".into();
    assert!(matches!(
        registry.install(unsafe_manifest, internal_policy(), FIXTURE_ARTIFACT),
        Err(RegistryError::UnsafeIdentifier(_))
    ));

    registry
        .install(
            manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT),
            internal_policy(),
            FIXTURE_ARTIFACT,
        )
        .unwrap();
    registry.enable(&key("1.0.0")).unwrap();
    let axes = [0.0, 0.1, 0.2];
    let bad_values = [0.0, 0.0, 0.0, 0.0, 1.2, 0.0, 0.0, 0.0, 0.0];
    let result = canonicalize_supplied_mask(
        &registry,
        &key("1.0.0"),
        source(),
        1_788_000_000_001,
        GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes,
        },
        MaskOutput::Probabilities {
            width: 3,
            height: 3,
            values: &bad_values,
        },
        DistributionAudience::CompanyCoworker,
    );
    assert!(matches!(result, Err(RegistryError::InvalidOutput(_))));

    let zeros = [0.0_f32; 9];
    let denied = canonicalize_supplied_mask(
        &registry,
        &key("1.0.0"),
        source(),
        1_788_000_000_001,
        GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes,
        },
        MaskOutput::Probabilities {
            width: 3,
            height: 3,
            values: &zeros,
        },
        DistributionAudience::PublicWebsite,
    );
    assert!(matches!(
        denied,
        Err(RegistryError::DistributionDenied { .. })
    ));
    assert!(matches!(
        registry
            .get(&key("1.0.0"))
            .unwrap()
            .authorize_artifact(DistributionAudience::CompanyCoworker),
        Err(RegistryError::DistributionDenied { .. })
    ));

    let invalid_axes = [0.0, f64::NAN, 0.2];
    assert!(matches!(
        GridGeometry::Geographic {
            longitudes: &invalid_axes,
            latitudes: &axes
        }
        .shape(registry.limits()),
        Err(RegistryError::IncompatibleInput(_))
    ));

    let small_limits = ModelLimits {
        max_grid_points: 4,
        ..ModelLimits::default()
    };
    let separate = TestDirectory::new("dimension-limit");
    let limited = ModelRegistry::open(separate.path(), small_limits).unwrap();
    assert!(matches!(
        GridGeometry::Geographic {
            longitudes: &axes,
            latitudes: &axes
        }
        .shape(limited.limits()),
        Err(RegistryError::IncompatibleInput(_))
    ));
}

#[cfg(unix)]
#[test]
fn registry_rejects_symlinked_model_tree() {
    use std::os::unix::fs::symlink;

    let directory = TestDirectory::new("symlink");
    let outside = TestDirectory::new("symlink-outside");
    fs::create_dir_all(directory.path().join("models")).unwrap();
    symlink(outside.path(), directory.path().join("models/linked-model")).unwrap();
    assert!(matches!(
        ModelRegistry::open(directory.path(), ModelLimits::default()),
        Err(RegistryError::FilesystemLink(_))
    ));
}

#[test]
fn license_and_training_provenance_are_mandatory_at_install() {
    let directory = TestDirectory::new("provenance");
    let mut registry = ModelRegistry::open(directory.path(), ModelLimits::default()).unwrap();
    let mut no_license = manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT);
    no_license.license = None;
    assert!(matches!(
        registry.install(no_license, internal_policy(), FIXTURE_ARTIFACT),
        Err(RegistryError::InvalidMetadata(_))
    ));
    let mut no_training = manifest("1.0.0", StormModelBackend::SuppliedMask, FIXTURE_ARTIFACT);
    no_training.training_provenance = None;
    assert!(matches!(
        registry.install(no_training, internal_policy(), FIXTURE_ARTIFACT),
        Err(RegistryError::InvalidMetadata(_))
    ));
}

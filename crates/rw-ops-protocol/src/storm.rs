use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{
    GeoPoint, ProtocolError, finite_in_range, invalid, validate_identifier, validate_schema,
    validate_text,
};

pub const STORM_CELL_FRAME_SCHEMA: &str = "rw.ops.storm-cell-frame.v1";
pub const STORM_METHOD_CATALOG_SCHEMA: &str = "rw.ops.storm-method-catalog.v1";
pub const STORM_MODEL_MANIFEST_SCHEMA: &str = "rw.ops.storm-model-manifest.v1";

/// A model inference request fans out validation and field reads once per
/// input plane, so this is part of the executable wire contract rather than a
/// grid-resolution or catalog-cardinality policy. It is advertised by the
/// storm-model catalog and may be revised with a future protocol version.
pub const MAX_MODEL_INPUTS: usize = 64;

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StormSource {
    Mrms {
        product: String,
        valid_at_unix_ms: i64,
        grid_hash: String,
    },
    NexradLevel2 {
        site: String,
        volume_at_unix_ms: i64,
        elevation_degrees_milli: i32,
        moment: String,
    },
}

impl StormSource {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Mrms {
                product,
                valid_at_unix_ms,
                grid_hash,
            } => {
                validate_identifier("product", product, 128)?;
                validate_identifier("grid_hash", grid_hash, 128)?;
                if *valid_at_unix_ms <= 0 {
                    return Err(invalid("valid_at_unix_ms", "must be positive"));
                }
            }
            Self::NexradLevel2 {
                site,
                volume_at_unix_ms,
                elevation_degrees_milli,
                moment,
            } => {
                validate_identifier("site", site, 16)?;
                validate_identifier("moment", moment, 32)?;
                if *volume_at_unix_ms <= 0 {
                    return Err(invalid("volume_at_unix_ms", "must be positive"));
                }
                if !(-2_000..=90_000).contains(elevation_degrees_milli) {
                    return Err(invalid(
                        "elevation_degrees_milli",
                        "outside physical bounds",
                    ));
                }
            }
        }
        Ok(())
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StormMethodKind {
    Authoritative,
    Deterministic,
    MachineLearning,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormMethodIdentity {
    pub method_id: String,
    pub method_version: String,
    pub kind: StormMethodKind,
    pub display_name: String,
    pub description: String,
    #[serde(default)]
    pub upstream_product: Option<String>,
    #[serde(default)]
    pub model_id: Option<String>,
    #[serde(default)]
    pub model_version: Option<String>,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

impl StormMethodIdentity {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier("method_id", &self.method_id, 96)?;
        validate_identifier("method_version", &self.method_version, 48)?;
        validate_text("display_name", &self.display_name, 128)?;
        validate_text("description", &self.description, 2048)?;
        if self.parameters.len() > 128 {
            return Err(invalid("parameters", "too many method parameters"));
        }
        if self.kind == StormMethodKind::MachineLearning
            && (self.model_id.is_none() || self.model_version.is_none())
        {
            return Err(invalid(
                "model_identity",
                "machine-learning methods require model_id and model_version",
            ));
        }
        for (key, value) in &self.parameters {
            validate_identifier("parameter_key", key, 64)?;
            validate_text("parameter_value", value, 256)?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContourRing {
    pub hole: bool,
    pub points: Vec<GeoPoint>,
}

impl ContourRing {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        if self.points.len() < 4 {
            return Err(invalid(
                "contour_ring",
                "requires at least four closed points",
            ));
        }
        self.points.iter().try_for_each(GeoPoint::validate)?;
        let first = self.points.first().unwrap();
        let last = self.points.last().unwrap();
        if (first.latitude - last.latitude).abs() > 1.0e-9
            || (first.longitude - last.longitude).abs() > 1.0e-9
        {
            return Err(invalid("contour_ring", "must be explicitly closed"));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormCell {
    pub cell_id: String,
    #[serde(default)]
    pub track_id: Option<String>,
    pub centroid: GeoPoint,
    pub rings: Vec<ContourRing>,
    pub area_km2: f64,
    #[serde(default)]
    pub maximum_reflectivity_dbz: Option<f64>,
    #[serde(default)]
    pub echo_top_m: Option<f64>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

impl StormCell {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier("cell_id", &self.cell_id, 96)?;
        if let Some(track_id) = &self.track_id {
            validate_identifier("track_id", track_id, 96)?;
        }
        self.centroid.validate()?;
        if self.rings.is_empty() {
            return Err(invalid("rings", "requires a non-empty contour set"));
        }
        self.rings.iter().try_for_each(ContourRing::validate)?;
        finite_in_range("area_km2", self.area_km2, 0.0, 100_000_000.0)?;
        if self.area_km2 == 0.0 {
            return Err(invalid("area_km2", "must be greater than zero"));
        }
        if let Some(value) = self.maximum_reflectivity_dbz {
            finite_in_range("maximum_reflectivity_dbz", value, -100.0, 200.0)?;
        }
        if let Some(value) = self.echo_top_m {
            finite_in_range("echo_top_m", value, 0.0, 100_000.0)?;
        }
        if let Some(value) = self.confidence {
            finite_in_range("confidence", value, 0.0, 1.0)?;
        }
        if self.attributes.len() > 128 {
            return Err(invalid("attributes", "too many cell attributes"));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormCellFrame {
    pub schema: String,
    pub generated_at_unix_ms: i64,
    pub source: StormSource,
    pub method: StormMethodIdentity,
    pub cells: Vec<StormCell>,
    pub partial: bool,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl StormCellFrame {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_schema(&self.schema, STORM_CELL_FRAME_SCHEMA)?;
        if self.generated_at_unix_ms <= 0 {
            return Err(invalid("generated_at_unix_ms", "must be positive"));
        }
        self.source.validate()?;
        self.method.validate()?;
        self.cells.iter().try_for_each(StormCell::validate)?;
        self.warnings
            .iter()
            .try_for_each(|warning| validate_text("warning", warning, 2048))
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormMethodCatalog {
    pub schema: String,
    pub generated_at_unix_ms: i64,
    pub methods: Vec<StormMethodIdentity>,
}

impl StormMethodCatalog {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_schema(&self.schema, STORM_METHOD_CATALOG_SCHEMA)?;
        self.methods
            .iter()
            .try_for_each(StormMethodIdentity::validate)
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StormModelBackend {
    NativeRust,
    TractOnnx,
    SuppliedMask,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInputSource {
    MrmsProduct,
    NexradMoment,
    DerivedField,
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormModelInput {
    pub name: String,
    pub source: ModelInputSource,
    pub field: String,
    pub units: String,
    #[serde(default)]
    pub minimum: Option<f64>,
    #[serde(default)]
    pub maximum: Option<f64>,
    #[serde(default)]
    pub missing_value: Option<f64>,
}

impl StormModelInput {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_identifier("input_name", &self.name, 96)?;
        validate_identifier("input_field", &self.field, 128)?;
        validate_text("input_units", &self.units, 64)?;
        if let (Some(minimum), Some(maximum)) = (self.minimum, self.maximum)
            && !(minimum.is_finite() && maximum.is_finite() && minimum < maximum)
        {
            return Err(invalid("input_range", "minimum must be below maximum"));
        }
        if self.missing_value.is_some_and(|value| !value.is_finite()) {
            return Err(invalid("missing_value", "must be finite when supplied"));
        }
        Ok(())
    }
}

#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StormModelManifest {
    pub schema: String,
    pub model_id: String,
    pub model_version: String,
    pub backend: StormModelBackend,
    pub artifact_sha256: String,
    pub display_name: String,
    pub description: String,
    pub inputs: Vec<StormModelInput>,
    pub output_name: String,
    pub probability_threshold: f64,
    #[serde(default)]
    pub minimum_area_km2: Option<f64>,
    pub producer: String,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub training_provenance: Option<String>,
}

impl StormModelManifest {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        validate_schema(&self.schema, STORM_MODEL_MANIFEST_SCHEMA)?;
        validate_identifier("model_id", &self.model_id, 96)?;
        validate_identifier("model_version", &self.model_version, 48)?;
        if self.artifact_sha256.len() != 64
            || !self
                .artifact_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid("artifact_sha256", "must be 64 hexadecimal digits"));
        }
        validate_text("display_name", &self.display_name, 128)?;
        validate_text("description", &self.description, 4096)?;
        validate_text("producer", &self.producer, 256)?;
        validate_identifier("output_name", &self.output_name, 96)?;
        if self.inputs.is_empty() || self.inputs.len() > MAX_MODEL_INPUTS {
            return Err(invalid("inputs", "requires a bounded non-empty input list"));
        }
        self.inputs.iter().try_for_each(StormModelInput::validate)?;
        finite_in_range(
            "probability_threshold",
            self.probability_threshold,
            0.0,
            1.0,
        )?;
        if let Some(value) = self.minimum_area_km2 {
            finite_in_range("minimum_area_km2", value, 0.0, 100_000_000.0)?;
        }
        if let Some(value) = &self.license {
            validate_text("license", value, 512)?;
        }
        if let Some(value) = &self.training_provenance {
            validate_text("training_provenance", value, 8192)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ml_manifest_requires_exact_artifact_identity_and_provenance() {
        let manifest = StormModelManifest {
            schema: STORM_MODEL_MANIFEST_SCHEMA.into(),
            model_id: "cell-segmentation".into(),
            model_version: "2026.08.1".into(),
            backend: StormModelBackend::TractOnnx,
            artifact_sha256: "a".repeat(64),
            display_name: "Cell segmentation".into(),
            description: "Produces a calibrated storm-cell probability mask.".into(),
            inputs: vec![StormModelInput {
                name: "reflectivity".into(),
                source: ModelInputSource::MrmsProduct,
                field: "reflectivity_at_lowest_altitude".into(),
                units: "dBZ".into(),
                minimum: Some(-20.0),
                maximum: Some(80.0),
                missing_value: Some(-999.0),
            }],
            output_name: "cell_probability".into(),
            probability_threshold: 0.55,
            minimum_area_km2: Some(4.0),
            producer: "Fahrenheit Research".into(),
            license: Some("private-company-use".into()),
            training_provenance: Some("training-set:v1".into()),
        };
        manifest.validate().unwrap();
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let decoded: StormModelManifest = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(decoded, manifest);
    }

    #[test]
    fn method_catalog_has_no_arbitrary_entry_count_ceiling() {
        let method = StormMethodIdentity {
            method_id: "deterministic".into(),
            method_version: "1".into(),
            kind: StormMethodKind::Deterministic,
            display_name: "Deterministic cells".into(),
            description: "Validated catalog fixture".into(),
            upstream_product: None,
            model_id: None,
            model_version: None,
            parameters: BTreeMap::new(),
        };
        let catalog = StormMethodCatalog {
            schema: STORM_METHOD_CATALOG_SCHEMA.into(),
            generated_at_unix_ms: 1,
            methods: vec![method; 257],
        };
        catalog.validate().unwrap();
    }

    #[test]
    fn contour_must_be_closed() {
        let ring = ContourRing {
            hole: false,
            points: vec![
                GeoPoint {
                    latitude: 0.0,
                    longitude: 0.0,
                },
                GeoPoint {
                    latitude: 0.0,
                    longitude: 1.0,
                },
                GeoPoint {
                    latitude: 1.0,
                    longitude: 1.0,
                },
                GeoPoint {
                    latitude: 1.0,
                    longitude: 0.0,
                },
            ],
        };
        assert!(ring.validate().is_err());
    }
}

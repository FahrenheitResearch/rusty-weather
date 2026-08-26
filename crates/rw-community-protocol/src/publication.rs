//! Closed contracts for deliberate case-artifact publication.
//!
//! These DTOs deliberately cannot name a local path, remote URL, directory,
//! or arbitrary file. Raw `wrfout` and full-run transfer are not accepted by
//! the artifact endpoint. The generation inventory at the bottom of this file
//! is a contract for a later, separately gated replication service only.

use std::collections::{BTreeMap, BTreeSet};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{
    AttributionNotice, CaseArtifactType, DataOrigin, ProtocolError, ProtocolLimits,
    PublicationGrant, ShareQuery, ShareRequest, SourceProvenance, request_sha256,
};

pub const CASE_ARTIFACT_PUBLICATION_SCHEMA: &str = "rw.community.case-artifact-publication.v1";
pub const CASE_ARTIFACT_REVOCATION_SCHEMA: &str = "rw.community.case-artifact-revocation.v1";
pub const CASE_REVOCATION_SCHEMA: &str = "rw.community.case-revocation.v1";
pub const PUBLICATION_AUDIT_SCHEMA: &str = "rw.community.publication-audit.v1";
pub const PUBLICATION_TOMBSTONE_SCHEMA: &str = "rw.community.publication-tombstone.v1";
pub const RUN_GENERATION_PUBLICATION_SCHEMA: &str = "rw.community.run-generation-publication.v1";
pub const RUN_GENERATION_CHUNK_SCHEMA: &str = "rw.community.rws-generation-chunk.v1";
pub const PUBLICATION_OWNER_PARAMETER: &str = "publication_owner_principal_sha256";

pub const PUBLISH_CASE_ARTIFACT_PATH: &str = "/v1/community/artifacts";
pub const REVOKE_CASE_ARTIFACT_PATH_TEMPLATE: &str = "/v1/community/artifacts/{sha256}/revoke";
pub const REVOKE_CASE_PATH_TEMPLATE: &str = "/v1/community/cases/{case_id}/revoke";

const MAX_PUBLICATION_RETENTION_SECONDS: i64 = 366 * 24 * 60 * 60;
const MAX_ANNOTATION_BYTES: usize = 64 * 1024;
const MAX_TABLE_COLUMNS: usize = 256;
const MAX_TABLE_ROWS: usize = 100_000;
const MAX_TABLE_CELLS: usize = 2_000_000;
const MAX_OVERLAY_FEATURES: usize = 100_000;
const MAX_OVERLAY_COORDINATES: usize = 2_000_000;
const MAX_IMAGE_PIXELS: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnotationArtifact {
    pub title: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_unix: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TableCell {
    Text { value: String },
    Integer { value: i64 },
    FixedDecimal { value_e6: i64 },
    Boolean { value: bool },
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DerivedTableArtifact {
    pub title: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<TableCell>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixedCoordinate {
    pub longitude_e7: i32,
    pub latitude_e7: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OverlayGeometry {
    Point { coordinate: FixedCoordinate },
    Polyline { coordinates: Vec<FixedCoordinate> },
    Polygon { ring: Vec<FixedCoordinate> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayFeature {
    pub feature_id: String,
    pub geometry: OverlayGeometry,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayArtifact {
    pub title: String,
    pub features: Vec<OverlayFeature>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderedImageFormat {
    Png,
    Webp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenderedImageArtifact {
    pub format: RenderedImageFormat,
    pub width: u32,
    pub height: u32,
    pub alt_text: String,
    pub bytes_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CaseArtifactPayload {
    Annotation(AnnotationArtifact),
    DerivedTable(DerivedTableArtifact),
    Overlay(OverlayArtifact),
    RenderedImage(RenderedImageArtifact),
}

impl CaseArtifactPayload {
    pub fn artifact_type(&self) -> CaseArtifactType {
        match self {
            Self::Annotation(_) => CaseArtifactType::Annotation,
            Self::DerivedTable(_) => CaseArtifactType::DerivedTable,
            Self::Overlay(_) => CaseArtifactType::Overlay,
            Self::RenderedImage(_) => CaseArtifactType::RenderedImage,
        }
    }

    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
        match self {
            Self::Annotation(value) => {
                validate_safe_text("annotation.title", &value.title, 160)?;
                validate_safe_text("annotation.text", &value.text, MAX_ANNOTATION_BYTES)?;
                if value.event_unix.is_some_and(|time| time < 0) {
                    return invalid("annotation.event_unix", "must be non-negative");
                }
            }
            Self::DerivedTable(value) => {
                validate_safe_text("table.title", &value.title, 160)?;
                if value.columns.is_empty() || value.columns.len() > MAX_TABLE_COLUMNS {
                    return invalid("table.columns", "must be a bounded non-empty list");
                }
                let mut columns = BTreeSet::new();
                for column in &value.columns {
                    validate_safe_text("table.column", column, 128)?;
                    if !columns.insert(column) {
                        return invalid("table.columns", "contains duplicate names");
                    }
                }
                if value.rows.len() > MAX_TABLE_ROWS
                    || value
                        .rows
                        .len()
                        .checked_mul(value.columns.len())
                        .is_none_or(|cells| cells > MAX_TABLE_CELLS)
                {
                    return invalid("table.rows", "table exceeds the row or cell limit");
                }
                for row in &value.rows {
                    if row.len() != value.columns.len() {
                        return invalid("table.rows", "row width does not match columns");
                    }
                    for cell in row {
                        if let TableCell::Text { value } = cell {
                            validate_safe_text("table.text", value, 4096)?;
                        }
                    }
                }
            }
            Self::Overlay(value) => {
                validate_safe_text("overlay.title", &value.title, 160)?;
                if value.features.is_empty() || value.features.len() > MAX_OVERLAY_FEATURES {
                    return invalid("overlay.features", "must be a bounded non-empty list");
                }
                let mut coordinate_count = 0usize;
                let mut feature_ids = BTreeSet::new();
                for feature in &value.features {
                    validate_id("overlay.feature_id", &feature.feature_id, 96)?;
                    if !feature_ids.insert(&feature.feature_id) {
                        return invalid("overlay.feature_id", "duplicate feature id");
                    }
                    let coordinates: &[FixedCoordinate] = match &feature.geometry {
                        OverlayGeometry::Point { coordinate } => std::slice::from_ref(coordinate),
                        OverlayGeometry::Polyline { coordinates } if coordinates.len() >= 2 => {
                            coordinates
                        }
                        OverlayGeometry::Polygon { ring } if ring.len() >= 4 => {
                            if ring.first() != ring.last() {
                                return invalid("overlay.polygon", "ring must be closed");
                            }
                            ring
                        }
                        _ => return invalid("overlay.geometry", "geometry is empty or malformed"),
                    };
                    coordinate_count = coordinate_count
                        .checked_add(coordinates.len())
                        .ok_or_else(|| invalid_error("overlay", "coordinate count overflow"))?;
                    if coordinate_count > MAX_OVERLAY_COORDINATES {
                        return invalid("overlay", "too many coordinates");
                    }
                    for coordinate in coordinates {
                        validate_coordinate(*coordinate)?;
                    }
                    if feature.properties.len() > 64 {
                        return invalid("overlay.properties", "too many properties");
                    }
                    for (key, value) in &feature.properties {
                        validate_id("overlay.property", key, 64)?;
                        validate_safe_text("overlay.property", value, 1024)?;
                    }
                }
            }
            Self::RenderedImage(value) => {
                if value.width == 0
                    || value.height == 0
                    || u64::from(value.width)
                        .checked_mul(u64::from(value.height))
                        .is_none_or(|pixels| pixels > MAX_IMAGE_PIXELS)
                {
                    return invalid("image.dimensions", "image pixel bounds are invalid");
                }
                validate_safe_text("image.alt_text", &value.alt_text, 2048)?;
                let maximum = usize::try_from(limits.max_encoded_bytes).unwrap_or(usize::MAX);
                if value.bytes_base64.len() > maximum.saturating_mul(2) {
                    return Err(ProtocolError::EncodedSizeLimit);
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&value.bytes_base64)
                    .map_err(|_| invalid_error("image.bytes_base64", "malformed base64"))?;
                if bytes.is_empty() || bytes.len() as u64 > limits.max_encoded_bytes {
                    return Err(ProtocolError::EncodedSizeLimit);
                }
                let valid_signature = match value.format {
                    RenderedImageFormat::Png => bytes.starts_with(b"\x89PNG\r\n\x1a\n"),
                    RenderedImageFormat::Webp => {
                        bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP"
                    }
                };
                if !valid_signature {
                    return invalid(
                        "image.bytes_base64",
                        "bytes do not match the declared image format",
                    );
                }
                let parsed_dimensions =
                    image_dimensions(value.format, &bytes).ok_or_else(|| {
                        invalid_error(
                            "image.bytes_base64",
                            "image header is truncated or malformed",
                        )
                    })?;
                if parsed_dimensions != (value.width, value.height) {
                    return invalid(
                        "image.dimensions",
                        "declared dimensions do not match the encoded image header",
                    );
                }
                let decoded_surface_bytes = u64::from(value.width)
                    .saturating_mul(u64::from(value.height))
                    .saturating_mul(4);
                if decoded_surface_bytes > limits.max_decoded_bytes {
                    return Err(ProtocolError::DecodedSizeLimit);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishCaseArtifactRequest {
    pub schema: String,
    /// Stable SHA-256 principal supplied by the authenticated client. The
    /// server requires exact equality with its bearer-token-derived principal.
    pub owner_principal_sha256: String,
    pub request: ShareRequest,
    pub payload: CaseArtifactPayload,
    pub published_unix: i64,
    pub retain_until_unix: i64,
    #[serde(default)]
    pub attributions: Vec<AttributionNotice>,
    #[serde(default)]
    pub modification_notices: Vec<String>,
}

impl PublishCaseArtifactRequest {
    pub fn validate(&self, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
        if self.schema != CASE_ARTIFACT_PUBLICATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        validate_sha256_local("owner_principal_sha256", &self.owner_principal_sha256)?;
        self.request.validate(limits)?;
        if self.request.publication.data_origin == DataOrigin::PublicProvider
            || !self.request.publication.explicit_owner_publication
        {
            return invalid(
                "request.publication",
                "client-authored case artifacts must be explicit owner publications, not public-provider objects",
            );
        }
        let ShareQuery::CaseArtifact { artifact_type, .. } = self.request.query else {
            return invalid(
                "request.query",
                "artifact publication requires case_artifact",
            );
        };
        if artifact_type != self.payload.artifact_type() {
            return invalid("payload.kind", "does not match the canonical artifact type");
        }
        if self
            .request
            .recipe
            .parameters
            .get(PUBLICATION_OWNER_PARAMETER)
            != Some(&self.owner_principal_sha256)
        {
            return invalid("publication owner", "canonical request is not owner-bound");
        }
        if self.published_unix < 0
            || self.retain_until_unix <= self.published_unix
            || self.retain_until_unix.saturating_sub(self.published_unix)
                > MAX_PUBLICATION_RETENTION_SECONDS
        {
            return invalid("publication timestamps", "invalid or excessive retention");
        }
        self.payload.validate(limits)?;
        validate_notices_local(&self.attributions, &self.modification_notices, limits)?;
        if self.attributions.is_empty() {
            return invalid(
                "attributions",
                "owner-published data requires attribution and license fields",
            );
        }
        if self.request.source_provenance.iter().any(|source| {
            matches!(
                source.licensing_publisher_identity(),
                "ecmwf" | "ecmwf-open-data"
            )
        }) {
            let has_ecmwf = self.attributions.iter().any(|notice| {
                matches!(notice.provider.as_str(), "ecmwf" | "ecmwf-open-data")
                    && notice.license.contains("CC BY 4.0")
                    && !notice.notice.trim().is_empty()
            });
            if !has_ecmwf
                || self
                    .modification_notices
                    .iter()
                    .all(|value| value.trim().is_empty())
            {
                return Err(ProtocolError::MissingEcmwfNotice);
            }
        }
        Ok(())
    }

    pub fn request_sha256(&self) -> Result<String, ProtocolError> {
        self.validate(&ProtocolLimits::default())?;
        request_sha256(&self.request)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokePublicationRequest {
    pub schema: String,
    pub rights_withdrawn: bool,
    pub reason: String,
}

impl RevokePublicationRequest {
    pub fn validate(&self, expected_schema: &str) -> Result<(), ProtocolError> {
        if self.schema != expected_schema {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        if !self.rights_withdrawn {
            return invalid(
                "rights_withdrawn",
                "revocation requires an explicit confirmation",
            );
        }
        validate_safe_text("revocation.reason", &self.reason, 1024)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationAuditRecord {
    pub schema: String,
    pub owner_principal_sha256: String,
    pub request_sha256: String,
    pub object_sha256: String,
    pub case_id: String,
    pub artifact_id: String,
    pub artifact_type: CaseArtifactType,
    pub data_origin: DataOrigin,
    pub published_unix: i64,
    pub retain_until_unix: i64,
    pub source_snapshot_id: String,
    pub source_grid_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationTombstone {
    pub schema: String,
    pub owner_principal_sha256: String,
    pub request_sha256: String,
    pub object_sha256: String,
    pub revoked_unix: i64,
    pub rights_withdrawn: bool,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationChunk {
    pub schema: String,
    pub chunk_id: String,
    pub object_sha256: String,
    pub encoded_size: u64,
    pub ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunGenerationPublicationManifest {
    pub schema: String,
    /// Must remain false until the separate full-run service is enabled.
    #[serde(default)]
    pub publication_enabled: bool,
    pub generation_id: String,
    pub model: String,
    pub run: String,
    pub snapshot_id: String,
    pub grid_hash: String,
    pub owner_principal_sha256: String,
    pub publication: PublicationGrant,
    pub source_provenance: Vec<SourceProvenance>,
    pub chunks: Vec<RunGenerationChunk>,
    pub generation_sha256: String,
    pub published_unix: i64,
    pub retain_until_unix: i64,
    #[serde(default)]
    pub attributions: Vec<AttributionNotice>,
    #[serde(default)]
    pub modification_notices: Vec<String>,
}

impl RunGenerationPublicationManifest {
    /// Validate the inventory shape. No server route consumes this contract;
    /// enabling publication still fails closed until that service exists.
    pub fn validate_inventory(&self, limits: &ProtocolLimits) -> Result<(), ProtocolError> {
        if self.schema != RUN_GENERATION_PUBLICATION_SCHEMA {
            return Err(ProtocolError::UnsupportedSchema(self.schema.clone()));
        }
        if self.publication_enabled {
            return invalid(
                "publication_enabled",
                "full-run replication is not implemented",
            );
        }
        validate_id("generation_id", &self.generation_id, 128)?;
        validate_id("model", &self.model, 96)?;
        validate_safe_text("run", &self.run, 128)?;
        validate_sha256_local("snapshot_id", &self.snapshot_id)?;
        validate_sha256_local("grid_hash", &self.grid_hash)?;
        validate_sha256_local("owner_principal_sha256", &self.owner_principal_sha256)?;
        validate_sha256_local("generation_sha256", &self.generation_sha256)?;
        validate_grant(&self.publication)?;
        if self.source_provenance.is_empty()
            || self.source_provenance.len() > limits.max_provenance_entries
        {
            return invalid(
                "source_provenance",
                "generation inventory requires bounded source provenance",
            );
        }
        for source in &self.source_provenance {
            validate_id("source.provider", &source.provider, 96)?;
            source.validate_identity()?;
            if source.roles.is_empty() || source.roles.len() > 64 || source.products.len() > 128 {
                return invalid(
                    "source_provenance",
                    "roles and products exceed generation inventory bounds",
                );
            }
            for role in &source.roles {
                validate_id("source.role", role, 96)?;
            }
            for product in &source.products {
                validate_id("source.product", product, 128)?;
            }
        }
        if self.chunks.is_empty() || self.chunks.len() > 1_000_000 {
            return invalid("chunks", "must be a bounded non-empty inventory");
        }
        let mut ids = BTreeSet::new();
        let mut ordinals = BTreeSet::new();
        for chunk in &self.chunks {
            if chunk.schema != RUN_GENERATION_CHUNK_SCHEMA {
                return Err(ProtocolError::UnsupportedSchema(chunk.schema.clone()));
            }
            validate_id("chunk_id", &chunk.chunk_id, 128)?;
            validate_sha256_local("chunk.object_sha256", &chunk.object_sha256)?;
            if chunk.encoded_size == 0 || chunk.encoded_size > limits.max_encoded_bytes {
                return Err(ProtocolError::EncodedSizeLimit);
            }
            if !ids.insert(&chunk.chunk_id) || !ordinals.insert(chunk.ordinal) {
                return invalid("chunks", "duplicate chunk id or ordinal");
            }
        }
        if ordinals.iter().copied().ne(0..self.chunks.len() as u32) {
            return invalid("chunks", "chunk ordinals must be contiguous from zero");
        }
        if self.published_unix < 0 || self.retain_until_unix <= self.published_unix {
            return invalid("timestamps", "invalid publication retention");
        }
        validate_notices_local(&self.attributions, &self.modification_notices, limits)?;
        if self.publication.data_origin != DataOrigin::PublicProvider
            && self.attributions.is_empty()
        {
            return invalid(
                "attributions",
                "owner-published generation requires attribution and license fields",
            );
        }
        if self.source_provenance.iter().any(|source| {
            matches!(
                source.licensing_publisher_identity(),
                "ecmwf" | "ecmwf-open-data"
            )
        }) && (self.attributions.iter().all(|notice| {
            !matches!(notice.provider.as_str(), "ecmwf" | "ecmwf-open-data")
                || !notice.license.contains("CC BY 4.0")
        }) || self
            .modification_notices
            .iter()
            .all(|notice| notice.trim().is_empty()))
        {
            return Err(ProtocolError::MissingEcmwfNotice);
        }
        Ok(())
    }
}

pub fn validate_publication_audit(record: &PublicationAuditRecord) -> Result<(), ProtocolError> {
    if record.schema != PUBLICATION_AUDIT_SCHEMA {
        return Err(ProtocolError::UnsupportedSchema(record.schema.clone()));
    }
    validate_sha256_local("owner_principal_sha256", &record.owner_principal_sha256)?;
    validate_sha256_local("request_sha256", &record.request_sha256)?;
    validate_sha256_local("object_sha256", &record.object_sha256)?;
    validate_id("case_id", &record.case_id, 96)?;
    validate_id("artifact_id", &record.artifact_id, 96)?;
    validate_sha256_local("source_snapshot_id", &record.source_snapshot_id)?;
    validate_sha256_local("source_grid_hash", &record.source_grid_hash)?;
    if record.published_unix < 0 || record.retain_until_unix <= record.published_unix {
        return invalid("audit timestamps", "invalid retention");
    }
    Ok(())
}

pub fn validate_publication_tombstone(
    tombstone: &PublicationTombstone,
) -> Result<(), ProtocolError> {
    if tombstone.schema != PUBLICATION_TOMBSTONE_SCHEMA {
        return Err(ProtocolError::UnsupportedSchema(tombstone.schema.clone()));
    }
    validate_sha256_local("owner_principal_sha256", &tombstone.owner_principal_sha256)?;
    validate_sha256_local("request_sha256", &tombstone.request_sha256)?;
    validate_sha256_local("object_sha256", &tombstone.object_sha256)?;
    if tombstone.revoked_unix < 0 || !tombstone.rights_withdrawn {
        return invalid("tombstone", "revocation is not explicitly confirmed");
    }
    validate_safe_text("tombstone.reason", &tombstone.reason, 1024)
}

pub fn case_artifact_payload_bytes(
    publication: &PublishCaseArtifactRequest,
) -> Result<Vec<u8>, ProtocolError> {
    publication.validate(&ProtocolLimits::default())?;
    let payload = crate::TypedObjectPayload {
        schema: crate::CASE_ARTIFACT_PAYLOAD_SCHEMA.into(),
        request_sha256: request_sha256(&publication.request)?,
        data: &publication.payload,
    };
    let bytes = serde_json::to_vec(&payload).map_err(|_| ProtocolError::MalformedJson)?;
    if bytes.is_empty() {
        return Err(ProtocolError::EncodedSizeLimit);
    }
    Ok(bytes)
}

pub fn validate_case_artifact_payload_bytes(
    bytes: &[u8],
    request: &ShareRequest,
    limits: &ProtocolLimits,
) -> Result<CaseArtifactPayload, ProtocolError> {
    if bytes.is_empty() || bytes.len() as u64 > limits.max_encoded_bytes {
        return Err(ProtocolError::EncodedSizeLimit);
    }
    let payload: crate::TypedObjectPayload<CaseArtifactPayload> =
        serde_json::from_slice(bytes).map_err(|_| ProtocolError::MalformedJson)?;
    crate::validate_typed_payload_identity(&payload, crate::CASE_ARTIFACT_PAYLOAD_SCHEMA, request)?;
    payload.data.validate(limits)?;
    let ShareQuery::CaseArtifact { artifact_type, .. } = request.query else {
        return invalid("request.query", "expected case_artifact");
    };
    if artifact_type != payload.data.artifact_type() {
        return invalid("payload.kind", "does not match signed request");
    }
    Ok(payload.data)
}

fn validate_grant(grant: &PublicationGrant) -> Result<(), ProtocolError> {
    if !grant.redistribution_rights_confirmed {
        return Err(ProtocolError::RedistributionRightsUnconfirmed);
    }
    if grant.data_origin != DataOrigin::PublicProvider && !grant.explicit_owner_publication {
        return Err(ProtocolError::PrivatePublicationDenied);
    }
    Ok(())
}

fn validate_notices_local(
    attributions: &[AttributionNotice],
    modifications: &[String],
    limits: &ProtocolLimits,
) -> Result<(), ProtocolError> {
    if attributions.len() > limits.max_attributions || modifications.len() > 32 {
        return invalid("notices", "too many attribution or modification notices");
    }
    for notice in attributions {
        validate_id("attribution.provider", &notice.provider, 96)?;
        validate_safe_text("attribution.notice", &notice.notice, 2048)?;
        validate_safe_text("attribution.license", &notice.license, 512)?;
        validate_safe_text("attribution.disclaimer", &notice.disclaimer, 2048)?;
        for url in [&notice.source_url, &notice.license_url, &notice.terms_url] {
            if !url.starts_with("https://") || url.len() > 2048 || url.chars().any(char::is_control)
            {
                return invalid("attribution URL", "must be bounded HTTPS");
            }
        }
    }
    for notice in modifications {
        validate_safe_text("modification_notice", notice, 2048)?;
    }
    Ok(())
}

fn validate_coordinate(value: FixedCoordinate) -> Result<(), ProtocolError> {
    if !(-900_000_000..=900_000_000).contains(&value.latitude_e7)
        || !(-1_800_000_000..=1_800_000_000).contains(&value.longitude_e7)
    {
        return invalid("coordinate", "outside Earth bounds");
    }
    Ok(())
}

fn image_dimensions(format: RenderedImageFormat, bytes: &[u8]) -> Option<(u32, u32)> {
    match format {
        RenderedImageFormat::Png => {
            if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
                return None;
            }
            let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
            let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
            (width > 0 && height > 0).then_some((width, height))
        }
        RenderedImageFormat::Webp => {
            if bytes.len() < 30 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
                return None;
            }
            match &bytes[12..16] {
                b"VP8X" => {
                    // Animated WebP is deliberately excluded from the rendered
                    // image artifact contract.
                    if bytes[20] & 0x02 != 0 {
                        return None;
                    }
                    let width = 1
                        + u32::from(bytes[24])
                        + (u32::from(bytes[25]) << 8)
                        + (u32::from(bytes[26]) << 16);
                    let height = 1
                        + u32::from(bytes[27])
                        + (u32::from(bytes[28]) << 8)
                        + (u32::from(bytes[29]) << 16);
                    Some((width, height))
                }
                b"VP8L" if bytes.len() >= 25 && bytes[20] == 0x2f => {
                    let bits = u32::from_le_bytes(bytes[21..25].try_into().ok()?);
                    let width = (bits & 0x3fff) + 1;
                    let height = ((bits >> 14) & 0x3fff) + 1;
                    Some((width, height))
                }
                b"VP8 " if bytes.len() >= 30 && &bytes[23..26] == b"\x9d\x01\x2a" => {
                    let width = u16::from_le_bytes(bytes[26..28].try_into().ok()?) & 0x3fff;
                    let height = u16::from_le_bytes(bytes[28..30].try_into().ok()?) & 0x3fff;
                    (width > 0 && height > 0).then_some((u32::from(width), u32::from(height)))
                }
                _ => None,
            }
        }
    }
}

fn validate_safe_text(
    field: &'static str,
    value: &str,
    maximum: usize,
) -> Result<(), ProtocolError> {
    let trimmed = value.trim();
    let lowercase = trimmed.to_ascii_lowercase();
    let path_like = trimmed.starts_with('/')
        || trimmed.starts_with("\\\\")
        || (trimmed.len() >= 3
            && trimmed.as_bytes()[1] == b':'
            && matches!(trimmed.as_bytes()[2], b'\\' | b'/'));
    if trimmed.is_empty()
        || trimmed != value
        || value.len() > maximum
        || value.chars().any(|character| {
            character == '\0' || character.is_control() && character != '\n' && character != '\t'
        })
        || value.contains(['<', '>'])
        || lowercase.contains("javascript:")
        || lowercase.contains("file:")
        || lowercase.contains("http://")
        || lowercase.contains("https://")
        || lowercase.contains("<script")
        || path_like
    {
        return invalid(
            field,
            "must be bounded plain text without paths, URLs, HTML, or script",
        );
    }
    Ok(())
}

fn validate_id(field: &'static str, value: &str, maximum: usize) -> Result<(), ProtocolError> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return invalid(field, "must be an opaque token");
    }
    Ok(())
}

fn validate_sha256_local(field: &'static str, value: &str) -> Result<(), ProtocolError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return invalid(field, "must be lowercase SHA-256 hex");
    }
    Ok(())
}

fn invalid<T>(field: &'static str, reason: impl Into<String>) -> Result<T, ProtocolError> {
    Err(invalid_error(field, reason))
}

fn invalid_error(field: &'static str, reason: impl Into<String>) -> ProtocolError {
    ProtocolError::InvalidField {
        field,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{REQUEST_SCHEMA, RecipeIdentity};

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn request(origin: DataOrigin) -> PublishCaseArtifactRequest {
        let owner = hash('a');
        PublishCaseArtifactRequest {
            schema: CASE_ARTIFACT_PUBLICATION_SCHEMA.into(),
            owner_principal_sha256: owner.clone(),
            request: ShareRequest {
                schema: REQUEST_SCHEMA.into(),
                model: "owner-wrf".into(),
                run: "2026-08-12T00:00:00Z".into(),
                snapshot_id: hash('b'),
                grid_hash: hash('c'),
                variables: vec!["annotation".into()],
                query: ShareQuery::CaseArtifact {
                    case_id: "case-a".into(),
                    artifact_id: "note-a".into(),
                    artifact_type: CaseArtifactType::Annotation,
                },
                recipe: RecipeIdentity {
                    recipe_id: "case-annotation".into(),
                    recipe_version: "1".into(),
                    parameters: BTreeMap::from([(PUBLICATION_OWNER_PARAMETER.into(), owner)]),
                },
                source_provenance: vec![SourceProvenance {
                    provider: "owner".into(),
                    forecast_producer: None,
                    licensing_publisher: None,
                    transport_provider: None,
                    transport_is_mirror: false,
                    roles: vec!["simulation".into()],
                    products: vec!["wrf".into()],
                }],
                publication: PublicationGrant {
                    data_origin: origin,
                    explicit_owner_publication: true,
                    redistribution_rights_confirmed: true,
                },
            },
            payload: CaseArtifactPayload::Annotation(AnnotationArtifact {
                title: "Damage survey note".into(),
                text: "Observed circulation crossed the county line.".into(),
                event_unix: Some(1_800_000_000),
            }),
            published_unix: 1_800_000_000,
            retain_until_unix: 1_800_086_400,
            attributions: vec![AttributionNotice {
                provider: "owner".into(),
                notice: "Published by the simulation owner.".into(),
                source_url: "https://example.invalid/source".into(),
                license: "Owner-authorized redistribution".into(),
                license_url: "https://example.invalid/license".into(),
                terms_url: "https://example.invalid/terms".into(),
                disclaimer: "Experimental simulation.".into(),
            }],
            modification_notices: vec!["Encoded as a typed case annotation.".into()],
        }
    }

    #[test]
    fn private_publication_requires_owner_rights_and_owner_binding() {
        let limits = ProtocolLimits::default();
        let mut value = request(DataOrigin::PrivateWrf);
        assert!(value.validate(&limits).is_ok());
        assert!(request(DataOrigin::UserProvided).validate(&limits).is_ok());
        value.request.publication.explicit_owner_publication = false;
        assert_eq!(
            value.validate(&limits),
            Err(ProtocolError::PrivatePublicationDenied)
        );
        value.request.publication.explicit_owner_publication = true;
        value.request.publication.redistribution_rights_confirmed = false;
        assert_eq!(
            value.validate(&limits),
            Err(ProtocolError::RedistributionRightsUnconfirmed)
        );
        value.request.publication.redistribution_rights_confirmed = true;
        value.owner_principal_sha256 = hash('d');
        assert!(value.validate(&limits).is_err());
    }

    #[test]
    fn client_authored_artifact_cannot_claim_public_provider_identity() {
        let limits = ProtocolLimits::default();
        let mut value = request(DataOrigin::PublicProvider);
        value.request.model = "hrrr".into();
        value.request.source_provenance = vec![SourceProvenance {
            provider: "noaa-aws-public-data".into(),
            forecast_producer: None,
            licensing_publisher: None,
            transport_provider: None,
            transport_is_mirror: false,
            roles: vec!["surface".into()],
            products: vec!["wrfsfcf".into()],
        }];
        assert!(matches!(
            value.validate(&limits),
            Err(ProtocolError::InvalidField {
                field: "request.publication",
                ..
            })
        ));
    }

    #[test]
    fn artifact_types_reject_paths_urls_html_scripts_and_wrong_kinds() {
        let limits = ProtocolLimits::default();
        for text in [
            "C:\\private\\wrfout",
            "https://host/object",
            "<script>alert(1)</script>",
        ] {
            let mut value = request(DataOrigin::PrivateArwen);
            let CaseArtifactPayload::Annotation(annotation) = &mut value.payload else {
                unreachable!()
            };
            annotation.text = text.into();
            assert!(value.validate(&limits).is_err(), "accepted {text}");
        }
        let mut value = request(DataOrigin::PrivateWrf);
        value.payload = CaseArtifactPayload::DerivedTable(DerivedTableArtifact {
            title: "Values".into(),
            columns: vec!["value".into()],
            rows: vec![vec![TableCell::Integer { value: 1 }]],
        });
        assert!(value.validate(&limits).is_err());

        let mut malformed_image = request(DataOrigin::PrivateWrf);
        let ShareQuery::CaseArtifact { artifact_type, .. } = &mut malformed_image.request.query
        else {
            unreachable!()
        };
        *artifact_type = CaseArtifactType::RenderedImage;
        malformed_image.payload = CaseArtifactPayload::RenderedImage(RenderedImageArtifact {
            format: RenderedImageFormat::Png,
            width: 1,
            height: 1,
            alt_text: "Rendered model field".into(),
            bytes_base64: base64::engine::general_purpose::STANDARD.encode(b"not-a-png"),
        });
        assert!(malformed_image.validate(&limits).is_err());
    }

    #[test]
    fn typed_payload_round_trip_is_exact_and_hash_bound() {
        let value = request(DataOrigin::PrivateWrf);
        let bytes = case_artifact_payload_bytes(&value).unwrap();
        let decoded = validate_case_artifact_payload_bytes(
            &bytes,
            &value.request,
            &ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(decoded, value.payload);
        assert_eq!(crate::object_sha256(&bytes).len(), 64);
        let mut wrong = value.request.clone();
        wrong.run = "different".into();
        assert!(
            validate_case_artifact_payload_bytes(&bytes, &wrong, &ProtocolLimits::default())
                .is_err()
        );
    }

    #[test]
    fn raw_generation_inventory_is_contract_only_and_disabled() {
        let manifest = RunGenerationPublicationManifest {
            schema: RUN_GENERATION_PUBLICATION_SCHEMA.into(),
            publication_enabled: false,
            generation_id: "wrf-generation-a".into(),
            model: "wrf".into(),
            run: "2026-08-12T00:00:00Z".into(),
            snapshot_id: hash('b'),
            grid_hash: hash('c'),
            owner_principal_sha256: hash('a'),
            publication: PublicationGrant {
                data_origin: DataOrigin::PrivateWrf,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            source_provenance: vec![SourceProvenance {
                provider: "simulation-owner".into(),
                forecast_producer: None,
                licensing_publisher: None,
                transport_provider: None,
                transport_is_mirror: false,
                roles: vec!["generation".into()],
                products: vec!["rws".into()],
            }],
            chunks: vec![RunGenerationChunk {
                schema: RUN_GENERATION_CHUNK_SCHEMA.into(),
                chunk_id: "rws-chunk-0000".into(),
                object_sha256: hash('d'),
                encoded_size: 1024,
                ordinal: 0,
            }],
            generation_sha256: hash('e'),
            published_unix: 1_800_000_000,
            retain_until_unix: 1_800_086_400,
            attributions: vec![AttributionNotice {
                provider: "simulation-owner".into(),
                notice: "Published by the simulation owner.".into(),
                source_url: "https://example.invalid/source".into(),
                license: "Owner-authorized redistribution".into(),
                license_url: "https://example.invalid/license".into(),
                terms_url: "https://example.invalid/terms".into(),
                disclaimer: "Experimental simulation.".into(),
            }],
            modification_notices: vec!["Inventoried as immutable rws chunks.".into()],
        };
        assert!(
            manifest
                .validate_inventory(&ProtocolLimits::default())
                .is_ok()
        );
        let mut enabled = manifest;
        enabled.publication_enabled = true;
        assert!(
            enabled
                .validate_inventory(&ProtocolLimits::default())
                .is_err()
        );
    }
}

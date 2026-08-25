//! Stable, transport-neutral storm-object contracts shared by RW Server,
//! desktop clients, and browser dashboards.
//!
//! This crate deliberately performs no networking, persistence, radar
//! decoding, or ML inference. A standalone workstation and a central server
//! therefore serialize the same honest identities, provenance, geometry, and
//! missing-data decisions without depending on one another's runtime.

mod storm;

pub use storm::*;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Geographic position shared by every contract in this crate.
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

impl GeoPoint {
    pub fn validate(&self) -> Result<(), ProtocolError> {
        finite_in_range("latitude", self.latitude, -90.0, 90.0)?;
        finite_in_range("longitude", self.longitude, -180.0, 180.0)
    }
}

pub const STORM_METHODS_PATH: &str = "/v1/ops/storms/methods";
pub const STORM_CELLS_PATH: &str = "/v1/ops/storms/cells";
pub const STORM_MODELS_PATH: &str = "/v1/ops/storms/models";
pub const NEXRAD_LEVEL3_STORM_DECODE_PATH: &str =
    "/v1/ops/storms/authoritative/nexrad-level3/decode";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("unsupported schema '{0}'")]
    UnsupportedSchema(String),
    #[error("invalid {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
    #[error("{field} exceeds the protocol limit of {limit}")]
    LimitExceeded { field: &'static str, limit: usize },
    #[error("timestamps are not monotonic")]
    NonMonotonicTime,
}

pub(crate) fn invalid(field: &'static str, reason: impl Into<String>) -> ProtocolError {
    ProtocolError::InvalidField {
        field,
        reason: reason.into(),
    }
}

pub(crate) fn validate_schema(actual: &str, expected: &str) -> Result<(), ProtocolError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProtocolError::UnsupportedSchema(actual.to_owned()))
    }
}

pub(crate) fn validate_identifier(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), ProtocolError> {
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.len() > maximum_bytes {
        return Err(ProtocolError::LimitExceeded {
            field,
            limit: maximum_bytes,
        });
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(invalid(
            field,
            "must contain only ASCII letters, digits, '-', '_', '.', or ':'",
        ));
    }
    Ok(())
}

pub(crate) fn validate_text(
    field: &'static str,
    value: &str,
    maximum_bytes: usize,
) -> Result<(), ProtocolError> {
    if value.trim().is_empty() {
        return Err(invalid(field, "must not be blank"));
    }
    if value.len() > maximum_bytes {
        return Err(ProtocolError::LimitExceeded {
            field,
            limit: maximum_bytes,
        });
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(field, "must not contain control characters"));
    }
    Ok(())
}

pub(crate) fn finite_in_range(
    field: &'static str,
    value: f64,
    minimum: f64,
    maximum: f64,
) -> Result<(), ProtocolError> {
    if value.is_finite() && (minimum..=maximum).contains(&value) {
        Ok(())
    } else {
        Err(invalid(
            field,
            format!("must be finite and within [{minimum}, {maximum}]"),
        ))
    }
}

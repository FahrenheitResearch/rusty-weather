//! Deterministic, UI-independent storm-cell geometry.
//!
//! This crate derives polygons from reflectivity grids. It does not represent
//! NEXRAD NST/STI centroids or tracks as authoritative polygons.

mod components;
mod engine;
mod geometry;

use rw_ops_protocol::{GeoPoint, ProtocolError, StormCellFrame, StormSource};
use thiserror::Error;

pub const DETERMINISTIC_METHOD_ID: &str = "rw-deterministic-reflectivity-components";
pub const DETERMINISTIC_METHOD_VERSION: &str = "1";

/// Gate adjacency used before contour extraction.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Connectivity {
    /// Only gates sharing a horizontal or vertical edge are connected.
    #[default]
    Four,
    /// Diagonally touching gates are also connected.
    Eight,
}

impl Connectivity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Four => "four",
            Self::Eight => "eight",
        }
    }
}

/// Controls the deterministic reflectivity segmentation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DetectionConfig {
    /// A finite gate is a member when its dBZ value is greater than or equal
    /// to this threshold.
    pub threshold_dbz: f32,
    /// Finite samples outside this inclusive range are treated as missing.
    /// This excludes common finite sentinels such as -999 and protects the
    /// protocol's physical reflectivity range.
    pub minimum_valid_dbz: f32,
    pub maximum_valid_dbz: f32,
    /// Components with fewer gates are rejected before contour extraction.
    pub minimum_gate_count: usize,
    /// Components with less derived spherical polygon area are rejected.
    pub minimum_area_km2: f64,
    pub connectivity: Connectivity,
}

impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            threshold_dbz: 35.0,
            minimum_valid_dbz: -100.0,
            maximum_valid_dbz: 200.0,
            minimum_gate_count: 4,
            minimum_area_km2: 1.0,
            connectivity: Connectivity::Four,
        }
    }
}

impl DetectionConfig {
    fn validate(self) -> Result<(), StormError> {
        if !self.minimum_valid_dbz.is_finite()
            || !self.maximum_valid_dbz.is_finite()
            || self.minimum_valid_dbz >= self.maximum_valid_dbz
            || self.minimum_valid_dbz < -100.0
            || self.maximum_valid_dbz > 200.0
        {
            return Err(StormError::InvalidConfig(
                "valid dBZ bounds must be finite, increasing, and within [-100, 200]",
            ));
        }
        if !self.threshold_dbz.is_finite()
            || !(self.minimum_valid_dbz..=self.maximum_valid_dbz).contains(&self.threshold_dbz)
        {
            return Err(StormError::InvalidConfig(
                "threshold_dbz must be finite and within the configured valid dBZ bounds",
            ));
        }
        if self.minimum_gate_count == 0 {
            return Err(StormError::InvalidConfig(
                "minimum_gate_count must be at least one",
            ));
        }
        if !self.minimum_area_km2.is_finite()
            || !(0.0..=100_000_000.0).contains(&self.minimum_area_km2)
        {
            return Err(StormError::InvalidConfig(
                "minimum_area_km2 must be finite and within protocol bounds",
            ));
        }
        Ok(())
    }
}

/// A row-major scalar field on rectilinear longitude/latitude sample axes.
///
/// Axes may be strictly increasing or strictly decreasing. Dateline-wrapped
/// axes are intentionally rejected in this first slice because a discontinuous
/// longitude array is not rectilinear in geographic coordinates.
#[derive(Clone, Copy, Debug)]
pub struct GeographicGrid<'a> {
    pub values_dbz: &'a [f32],
    pub longitudes: &'a [f64],
    pub latitudes: &'a [f64],
}

/// A row-major scalar field already gridded in a local Cartesian radar plane.
///
/// `east_m` and `north_m` are offsets from `radar_location`. They are not
/// azimuth, slant range, radial index, or gate index. Level-II polar data must
/// be resampled with radar-aware beam geometry before calling this API.
#[derive(Clone, Copy, Debug)]
pub struct Level2CartesianGrid<'a> {
    pub values_dbz: &'a [f32],
    pub east_m: &'a [f64],
    pub north_m: &'a [f64],
    pub radar_location: GeoPoint,
}

#[derive(Debug, Error)]
pub enum StormError {
    #[error("invalid storm-detection configuration: {0}")]
    InvalidConfig(&'static str),
    #[error("invalid generated timestamp: it must be positive")]
    InvalidGeneratedTime,
    #[error("{axis} axis requires at least two coordinates, got {actual}")]
    AxisTooShort { axis: &'static str, actual: usize },
    #[error("{axis} axis coordinate {index} is non-finite")]
    NonFiniteAxis { axis: &'static str, index: usize },
    #[error("{axis} axis coordinate {index} is outside [{minimum}, {maximum}]")]
    AxisOutOfRange {
        axis: &'static str,
        index: usize,
        minimum: f64,
        maximum: f64,
    },
    #[error("{axis} axis changes direction or collapses between indices {left} and {right}")]
    NonMonotonicAxis {
        axis: &'static str,
        left: usize,
        right: usize,
    },
    #[error("grid dimensions overflow the platform address space")]
    GridSizeOverflow,
    #[error("grid data length mismatch: expected {expected}, got {actual}")]
    DataLength { expected: usize, actual: usize },
    #[error("component contour dimension {dimension} exceeds packed f32 index precision")]
    ContourDimensionPrecision { dimension: usize },
    #[error("could not allocate {requested} elements for {resource}")]
    Allocation {
        resource: &'static str,
        requested: usize,
    },
    #[error("Level-II Cartesian detection requires a NexradLevel2 StormSource")]
    Level2SourceRequired,
    #[error("contour engine emitted an open component boundary")]
    OpenComponentBoundary,
    #[error("contour engine emitted no polygon for a non-empty component")]
    MissingComponentBoundary,
    #[error("component polygon has no positive finite area")]
    InvalidPolygonArea,
    #[error("stable cell identifier collision")]
    IdentifierCollision,
    #[error("internal storm geometry invariant failed: {0}")]
    Invariant(&'static str),
    #[error(transparent)]
    Contour(#[from] weather_contours::ContourError),
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
}

/// Derive storm-cell polygons from an MRMS or already-georeferenced radar
/// reflectivity grid.
pub fn detect_geographic(
    source: StormSource,
    generated_at_unix_ms: i64,
    grid: GeographicGrid<'_>,
    config: DetectionConfig,
) -> Result<StormCellFrame, StormError> {
    validate_request(&source, generated_at_unix_ms, config)?;
    engine::detect(
        source,
        generated_at_unix_ms,
        grid.values_dbz,
        grid.longitudes,
        grid.latitudes,
        engine::Projection::Geographic,
        config,
    )
}

/// Derive storm-cell polygons from an explicitly Cartesian Level-II grid.
///
/// This API does no polar-to-Cartesian conversion and therefore cannot be
/// accidentally fed raw radial/gate indices under an ambiguous name.
pub fn detect_level2_cartesian(
    source: StormSource,
    generated_at_unix_ms: i64,
    grid: Level2CartesianGrid<'_>,
    config: DetectionConfig,
) -> Result<StormCellFrame, StormError> {
    if !matches!(source, StormSource::NexradLevel2 { .. }) {
        return Err(StormError::Level2SourceRequired);
    }
    grid.radar_location.validate()?;
    validate_request(&source, generated_at_unix_ms, config)?;
    engine::detect(
        source,
        generated_at_unix_ms,
        grid.values_dbz,
        grid.east_m,
        grid.north_m,
        engine::Projection::LocalCartesian {
            origin: grid.radar_location,
        },
        config,
    )
}

fn validate_request(
    source: &StormSource,
    generated_at_unix_ms: i64,
    config: DetectionConfig,
) -> Result<(), StormError> {
    source.validate()?;
    if generated_at_unix_ms <= 0 {
        return Err(StormError::InvalidGeneratedTime);
    }
    config.validate()
}

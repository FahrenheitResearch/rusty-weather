//! Bounded geographic-domain extraction from one immutable run snapshot.
//!
//! A request selects grid-point centres inside an eastward longitude arc and
//! a closed latitude interval. The result is the smallest native rectangular
//! envelope containing those centres. Cropped latitude/longitude arrays and a
//! mask travel with the values, so curvilinear grids and envelope cells outside
//! the requested geographic box are never mistaken for selected data.

use std::collections::BTreeSet;

use rustwx_core::GridProjection;
use serde::{Deserialize, Serialize};

use crate::{
    IndexWindow2DRequest, IndexWindow3DRequest, QueryError, QueryResult, RunDescriptor,
    RunSnapshot, TimePoint, query_window_2d, query_window_3d,
};

/// Versioned JSON data schema used both by the direct API and the signed
/// Community Cache payload wrapper.
pub const GEOGRAPHIC_WINDOW_RESULT_SCHEMA: &str = "rw.query.geographic-window.v1";

/// Geographic bounds in degrees. Latitude bounds are closed and ordered.
/// Longitude follows the eastward arc from `west_longitude` to
/// `east_longitude`: west < east is ordinary, west > east crosses the
/// antimeridian, and exactly -180..180 is the full globe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeographicBoundingBox {
    pub west_longitude: f64,
    pub south_latitude: f64,
    pub east_longitude: f64,
    pub north_latitude: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LongitudeArcSemantics {
    Ordinary,
    CrossesAntimeridian,
    FullGlobe,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GeographicVerticalSelection {
    Surface2d,
    PressureLevels { levels_hpa: Vec<u16> },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeographicWindowRequest {
    /// Required optimistic identity. The request fails instead of silently
    /// following an atomic replacement of the named run.
    pub expected_snapshot_id: String,
    pub expected_grid_hash: String,
    pub storage_slot: u16,
    pub variables: Vec<String>,
    pub bbox: GeographicBoundingBox,
    pub vertical: GeographicVerticalSelection,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GeographicWindowLimits {
    /// Maximum cells in the returned minimal native rectangular envelope.
    pub max_native_cells: usize,
    /// Maximum scalar entries across latitude, longitude, mask, and all
    /// returned field/level arrays.
    pub max_output_values: usize,
}

impl Default for GeographicWindowLimits {
    fn default() -> Self {
        Self {
            max_native_cells: 250_000,
            max_output_values: 2_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeGridEnvelope {
    pub x0: usize,
    pub y0: usize,
    pub nx: usize,
    pub ny: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GeographicFieldValues {
    Surface2d {
        /// Flat row-major `[y][x]` values. Cells outside the requested bbox
        /// are `None`, even when the rectangular native envelope contains a
        /// finite stored value there.
        values: Vec<Option<f32>>,
    },
    PressureLevels {
        /// Exact explicit request order; no vertical reduction is performed.
        levels_hpa: Vec<u16>,
        /// Flat `[level][y][x]` values. Masked envelope cells are `None` at
        /// every level.
        values: Vec<Option<f32>>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeographicField {
    pub variable: String,
    pub units: String,
    /// Exact stored selector metadata used by clients to recover production
    /// styling and scientific product identity.
    pub selector: serde_json::Value,
    pub data: GeographicFieldValues,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeographicWindowResult {
    pub schema: String,
    pub run: RunDescriptor,
    pub time: TimePoint,
    pub requested_bbox: GeographicBoundingBox,
    pub longitude_arc: LongitudeArcSemantics,
    /// Always `minimal_native_rectangular_envelope` in schema v1.
    pub envelope_semantics: String,
    /// Always `grid_point_center_within_closed_bbox` in schema v1.
    pub cell_inclusion_semantics: String,
    pub envelope: NativeGridEnvelope,
    /// Cropped row-major coordinates for exactly `envelope`, never the full
    /// run grid unless the selected envelope itself is the full grid.
    pub latitudes: Vec<Option<f32>>,
    pub longitudes: Vec<Option<f32>>,
    /// True only where the finite grid-point centre is inside `requested_bbox`.
    pub cell_mask: Vec<bool>,
    pub mask_required: bool,
    pub projection: Option<GridProjection>,
    pub fields: Vec<GeographicField>,
}

#[derive(Debug, Clone, Copy)]
struct ValidatedBounds {
    bbox: GeographicBoundingBox,
    longitude_arc: LongitudeArcSemantics,
    longitude_span: f64,
}

/// Extract a geographic domain with conservative default output budgets.
pub fn query_geographic_window(
    snapshot: &RunSnapshot,
    request: &GeographicWindowRequest,
) -> QueryResult<GeographicWindowResult> {
    query_geographic_window_with_cancel(
        snapshot,
        request,
        GeographicWindowLimits::default(),
        || false,
    )
}

/// Extract a geographic domain with explicit server budgets and cooperative
/// cancellation during full-grid coordinate discovery and field reads.
pub fn query_geographic_window_with_cancel<F>(
    snapshot: &RunSnapshot,
    request: &GeographicWindowRequest,
    limits: GeographicWindowLimits,
    mut is_cancelled: F,
) -> QueryResult<GeographicWindowResult>
where
    F: FnMut() -> bool,
{
    validate_identity(snapshot, request)?;
    validate_variables(snapshot, &request.variables)?;
    let bounds = validate_bbox(request.bbox)?;
    let levels = validate_vertical(&request.vertical)?;
    if limits.max_native_cells == 0 || limits.max_output_values == 0 {
        return Err(QueryError::InvalidRequest(
            "geographic-window limits must be greater than zero".into(),
        ));
    }

    let grid = snapshot.grid();
    let mut min_x = usize::MAX;
    let mut min_y = usize::MAX;
    let mut max_x = 0usize;
    let mut max_y = 0usize;
    let mut matched = 0usize;
    for index in 0..grid.lat.len() {
        if index % 16_384 == 0 && is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        if bounds.contains(grid.lat[index], grid.lon[index]) {
            let x = index % grid.nx;
            let y = index / grid.nx;
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
            matched += 1;
        }
    }
    if matched == 0 {
        return Err(QueryError::InvalidRequest(
            "geographic bounding box contains no finite grid-point centres in this snapshot".into(),
        ));
    }

    let nx = max_x - min_x + 1;
    let ny = max_y - min_y + 1;
    let cells = nx.checked_mul(ny).ok_or(QueryError::LimitExceeded {
        what: "geographic envelope cells",
        requested: usize::MAX,
        limit: limits.max_native_cells,
    })?;
    if cells > limits.max_native_cells {
        return Err(QueryError::LimitExceeded {
            what: "geographic envelope cells",
            requested: cells,
            limit: limits.max_native_cells,
        });
    }
    let level_count = levels.as_ref().map_or(1, Vec::len);
    let field_values = cells
        .checked_mul(level_count)
        .and_then(|count| count.checked_mul(request.variables.len()))
        .ok_or(QueryError::LimitExceeded {
            what: "geographic output values",
            requested: usize::MAX,
            limit: limits.max_output_values,
        })?;
    let output_values = cells
        .checked_mul(3)
        .and_then(|coordinate_values| coordinate_values.checked_add(field_values))
        .ok_or(QueryError::LimitExceeded {
            what: "geographic output values",
            requested: usize::MAX,
            limit: limits.max_output_values,
        })?;
    if output_values > limits.max_output_values {
        return Err(QueryError::LimitExceeded {
            what: "geographic output values",
            requested: output_values,
            limit: limits.max_output_values,
        });
    }

    let mut latitudes = try_vec(cells, "geographic latitude window")?;
    let mut longitudes = try_vec(cells, "geographic longitude window")?;
    let mut cell_mask = Vec::new();
    cell_mask
        .try_reserve_exact(cells)
        .map_err(|error| QueryError::Allocation {
            what: "geographic cell mask",
            detail: error.to_string(),
        })?;
    for y in min_y..=max_y {
        if (y - min_y) % 64 == 0 && is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        for x in min_x..=max_x {
            let source = y * grid.nx + x;
            let lat = grid.lat[source];
            let lon = grid.lon[source];
            latitudes.push(lat.is_finite().then_some(lat));
            longitudes.push(lon.is_finite().then_some(lon));
            cell_mask.push(bounds.contains(lat, lon));
        }
    }
    let mask_required = cell_mask.iter().any(|included| !included);

    let time = snapshot.timepoint(request.storage_slot)?;
    let (metadata_reader, metadata_path) = snapshot.open_reader(&time)?;
    let mut fields = Vec::new();
    fields
        .try_reserve_exact(request.variables.len())
        .map_err(|error| QueryError::Allocation {
            what: "geographic fields",
            detail: error.to_string(),
        })?;
    for variable in &request.variables {
        if is_cancelled() {
            return Err(QueryError::Cancelled);
        }
        let selector = metadata_reader
            .variable(variable)
            .ok_or_else(|| QueryError::UnknownVariable(variable.clone()))?
            .selector
            .clone();
        let field = match &levels {
            None => {
                let mut window = query_window_2d(
                    snapshot,
                    &IndexWindow2DRequest {
                        storage_slot: request.storage_slot,
                        variable: variable.clone(),
                        x0: min_x,
                        y0: min_y,
                        x1: max_x + 1,
                        y1: max_y + 1,
                    },
                )?;
                apply_mask(&mut window.values, &cell_mask, 1)?;
                GeographicField {
                    variable: window.variable,
                    units: window.units,
                    selector,
                    data: GeographicFieldValues::Surface2d {
                        values: window.values,
                    },
                }
            }
            Some(levels_hpa) => {
                let mut window = query_window_3d(
                    snapshot,
                    &IndexWindow3DRequest {
                        storage_slot: request.storage_slot,
                        variable: variable.clone(),
                        levels_hpa: levels_hpa.clone(),
                        x0: min_x,
                        y0: min_y,
                        x1: max_x + 1,
                        y1: max_y + 1,
                    },
                )?;
                apply_mask(&mut window.values, &cell_mask, levels_hpa.len())?;
                GeographicField {
                    variable: window.variable,
                    units: window.units,
                    selector,
                    data: GeographicFieldValues::PressureLevels {
                        levels_hpa: window.levels_hpa,
                        values: window.values,
                    },
                }
            }
        };
        fields.push(field);
    }
    snapshot.ensure_source(&metadata_reader, &metadata_path, time.storage_slot)?;
    snapshot.ensure_manifest_current()?;

    Ok(GeographicWindowResult {
        schema: GEOGRAPHIC_WINDOW_RESULT_SCHEMA.into(),
        run: snapshot.descriptor().clone(),
        time,
        requested_bbox: bounds.bbox,
        longitude_arc: bounds.longitude_arc,
        envelope_semantics: "minimal_native_rectangular_envelope".into(),
        cell_inclusion_semantics: "grid_point_center_within_closed_bbox".into(),
        envelope: NativeGridEnvelope {
            x0: min_x,
            y0: min_y,
            nx,
            ny,
        },
        latitudes,
        longitudes,
        cell_mask,
        mask_required,
        projection: grid.projection.clone(),
        fields,
    })
}

fn validate_identity(snapshot: &RunSnapshot, request: &GeographicWindowRequest) -> QueryResult<()> {
    if request.expected_snapshot_id != snapshot.descriptor().snapshot_id
        || request.expected_grid_hash != snapshot.descriptor().grid_hash
    {
        return Err(QueryError::InvalidRequest(
            "geographic request snapshot_id/grid_hash does not match the resolved immutable run"
                .into(),
        ));
    }
    Ok(())
}

fn validate_variables(snapshot: &RunSnapshot, variables: &[String]) -> QueryResult<()> {
    if variables.is_empty() || variables.len() > snapshot.limits().max_variables {
        return Err(QueryError::LimitExceeded {
            what: "geographic variables",
            requested: variables.len(),
            limit: snapshot.limits().max_variables,
        });
    }
    let mut unique = BTreeSet::new();
    for variable in variables {
        if variable.is_empty() || variable.trim() != variable || !unique.insert(variable) {
            return Err(QueryError::InvalidRequest(
                "geographic variables must be non-empty, trimmed, and unique".into(),
            ));
        }
    }
    Ok(())
}

fn validate_vertical(vertical: &GeographicVerticalSelection) -> QueryResult<Option<Vec<u16>>> {
    match vertical {
        GeographicVerticalSelection::Surface2d => Ok(None),
        GeographicVerticalSelection::PressureLevels { levels_hpa } => {
            if levels_hpa.is_empty() || levels_hpa.len() > 256 {
                return Err(QueryError::InvalidRequest(
                    "pressure geographic windows require 1..=256 explicit levels".into(),
                ));
            }
            let mut unique = BTreeSet::new();
            if levels_hpa
                .iter()
                .any(|level| *level == 0 || *level > 1_200 || !unique.insert(*level))
            {
                return Err(QueryError::InvalidRequest(
                    "pressure levels must be unique values in 1..=1200 hPa".into(),
                ));
            }
            Ok(Some(levels_hpa.clone()))
        }
    }
}

fn validate_bbox(bbox: GeographicBoundingBox) -> QueryResult<ValidatedBounds> {
    let coordinates = [
        bbox.west_longitude,
        bbox.south_latitude,
        bbox.east_longitude,
        bbox.north_latitude,
    ];
    if coordinates.iter().any(|value| !value.is_finite()) {
        return Err(QueryError::InvalidRequest(
            "geographic bounding-box coordinates must be finite".into(),
        ));
    }
    if !(-90.0..=90.0).contains(&bbox.south_latitude)
        || !(-90.0..=90.0).contains(&bbox.north_latitude)
        || bbox.south_latitude >= bbox.north_latitude
    {
        return Err(QueryError::InvalidRequest(
            "latitude bounds must satisfy -90 <= south < north <= 90".into(),
        ));
    }
    if !(-180.0..=180.0).contains(&bbox.west_longitude)
        || !(-180.0..=180.0).contains(&bbox.east_longitude)
    {
        return Err(QueryError::InvalidRequest(
            "longitude bounds must each be in -180..=180 degrees".into(),
        ));
    }
    let delta = bbox.east_longitude - bbox.west_longitude;
    if delta == 0.0 {
        return Err(QueryError::InvalidRequest(
            "longitude bounds must select a non-empty eastward arc".into(),
        ));
    }
    let (longitude_arc, longitude_span) = if delta.abs() == 360.0 {
        (LongitudeArcSemantics::FullGlobe, 360.0)
    } else if delta > 0.0 {
        (LongitudeArcSemantics::Ordinary, delta)
    } else {
        (
            LongitudeArcSemantics::CrossesAntimeridian,
            delta.rem_euclid(360.0),
        )
    };
    Ok(ValidatedBounds {
        bbox,
        longitude_arc,
        longitude_span,
    })
}

impl ValidatedBounds {
    fn contains(&self, latitude: f32, longitude: f32) -> bool {
        if !latitude.is_finite() || !longitude.is_finite() {
            return false;
        }
        let latitude = f64::from(latitude);
        if latitude < self.bbox.south_latitude || latitude > self.bbox.north_latitude {
            return false;
        }
        if self.longitude_arc == LongitudeArcSemantics::FullGlobe {
            return true;
        }
        let offset = (f64::from(longitude) - self.bbox.west_longitude).rem_euclid(360.0);
        offset <= self.longitude_span
    }
}

fn apply_mask(values: &mut [Option<f32>], mask: &[bool], planes: usize) -> QueryResult<()> {
    if values.len() != mask.len().saturating_mul(planes) {
        return Err(QueryError::InvalidRequest(
            "geographic field shape does not match its coordinate mask".into(),
        ));
    }
    for plane in values.chunks_exact_mut(mask.len()) {
        for (value, included) in plane.iter_mut().zip(mask) {
            if !included {
                *value = None;
            }
        }
    }
    Ok(())
}

fn try_vec<T>(capacity: usize, what: &'static str) -> QueryResult<Vec<T>> {
    let mut values = Vec::new();
    values
        .try_reserve_exact(capacity)
        .map_err(|error| QueryError::Allocation {
            what,
            detail: error.to_string(),
        })?;
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longitude_arcs_are_explicit_and_antimeridian_safe() {
        let ordinary = validate_bbox(GeographicBoundingBox {
            west_longitude: -110.0,
            south_latitude: 20.0,
            east_longitude: -90.0,
            north_latitude: 50.0,
        })
        .unwrap();
        assert_eq!(ordinary.longitude_arc, LongitudeArcSemantics::Ordinary);
        assert!(ordinary.contains(40.0, -100.0));
        assert!(!ordinary.contains(40.0, 179.0));

        let crossing = validate_bbox(GeographicBoundingBox {
            west_longitude: 170.0,
            south_latitude: -20.0,
            east_longitude: -170.0,
            north_latitude: 20.0,
        })
        .unwrap();
        assert_eq!(
            crossing.longitude_arc,
            LongitudeArcSemantics::CrossesAntimeridian
        );
        for longitude in [175.0, -175.0, 185.0] {
            assert!(crossing.contains(0.0, longitude));
        }
        assert!(!crossing.contains(0.0, 0.0));
    }

    #[test]
    fn non_finite_and_unordered_boxes_fail_closed() {
        for bbox in [
            GeographicBoundingBox {
                west_longitude: f64::NAN,
                south_latitude: 0.0,
                east_longitude: 10.0,
                north_latitude: 20.0,
            },
            GeographicBoundingBox {
                west_longitude: -10.0,
                south_latitude: 20.0,
                east_longitude: 10.0,
                north_latitude: 20.0,
            },
            GeographicBoundingBox {
                west_longitude: 10.0,
                south_latitude: 0.0,
                east_longitude: 10.0,
                north_latitude: 20.0,
            },
        ] {
            assert!(validate_bbox(bbox).is_err());
        }
    }
}

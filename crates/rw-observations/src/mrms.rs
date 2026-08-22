use std::path::Path;

use chrono::{Duration, NaiveDateTime};
use grib_core::grib2::{
    Grib2File, Grib2Message, flip_rows, grid_latlon, parameter_name, parameter_units,
    unpack_message_normalized,
};
use rustwx_core::{GridShape, LatLonGrid};
use serde::{Deserialize, Serialize};

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GridPlane, ObservationError, ObservationFamily, ObservationFrame,
    ObservationResult, StoredFrameRef, sanitize_token, write_observation_frame_with_limit,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MrmsMessageSelector {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discipline: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_category: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameter_number: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level_type: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_index: Option<usize>,
}

impl MrmsMessageSelector {
    fn matches(&self, message: &Grib2Message) -> bool {
        self.discipline
            .is_none_or(|value| message.discipline == value)
            && self
                .parameter_category
                .is_none_or(|value| message.product.parameter_category == value)
            && self
                .parameter_number
                .is_none_or(|value| message.product.parameter_number == value)
            && self
                .level_type
                .is_none_or(|value| message.product.level_type == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MrmsIngestRequest {
    pub product: String,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub variable: Option<String>,
    #[serde(default)]
    pub units: Option<String>,
    #[serde(default)]
    pub selector: MrmsMessageSelector,
}

impl MrmsIngestRequest {
    pub fn reflectivity_at_lowest_altitude() -> Self {
        Self {
            product: "ReflectivityAtLowestAltitude".to_string(),
            collection: Some("conus".to_string()),
            variable: Some("mrms_reflectivity_lowest_altitude".to_string()),
            units: Some("dBZ".to_string()),
            selector: MrmsMessageSelector::default(),
        }
    }

    pub fn composite_reflectivity() -> Self {
        Self {
            product: "MergedReflectivityQCComposite".to_string(),
            collection: Some("conus".to_string()),
            variable: Some("mrms_composite_reflectivity".to_string()),
            units: Some("dBZ".to_string()),
            selector: MrmsMessageSelector::default(),
        }
    }
}

pub fn fetch_mrms_frame(request: &MrmsIngestRequest) -> ObservationResult<ObservationFrame> {
    let bytes = rustwx_io::fetch_mrms_latest_product(&request.product)?;
    decode_mrms_grib(&bytes, request)
}

pub fn ingest_mrms_latest(
    store_root: &Path,
    request: &MrmsIngestRequest,
    maximum_cells: usize,
) -> ObservationResult<StoredFrameRef> {
    let frame = fetch_mrms_frame(request)?;
    write_observation_frame_with_limit(store_root, &frame, maximum_cells)
}

pub fn ingest_mrms_latest_default_limit(
    store_root: &Path,
    request: &MrmsIngestRequest,
) -> ObservationResult<StoredFrameRef> {
    ingest_mrms_latest(store_root, request, DEFAULT_MAXIMUM_GRID_CELLS)
}

pub fn decode_mrms_grib(
    bytes: &[u8],
    request: &MrmsIngestRequest,
) -> ObservationResult<ObservationFrame> {
    let file =
        Grib2File::from_bytes(bytes).map_err(|error| ObservationError::Mrms(error.to_string()))?;
    let matches = file
        .messages
        .iter()
        .filter(|message| request.selector.matches(message))
        .collect::<Vec<_>>();
    let index = request.selector.message_index.unwrap_or(0);
    let message = matches.get(index).copied().ok_or_else(|| {
        ObservationError::Mrms(format!(
            "product '{}' has no GRIB message matching selector {:?} at index {index}",
            request.product, request.selector
        ))
    })?;
    if message.grid.is_reduced {
        return Err(ObservationError::Mrms(
            "reduced GRIB grids are not supported for MRMS delivery".into(),
        ));
    }
    let nx = message.grid.nx as usize;
    let ny = message.grid.ny as usize;
    let shape = GridShape::new(nx, ny)?;
    let cells = shape.checked_len()?;
    let mut values = unpack_message_normalized(message)
        .map_err(|error| ObservationError::Mrms(error.to_string()))?;
    let (mut latitudes, mut longitudes) = grid_latlon(&message.grid);
    if values.len() != cells || latitudes.len() != cells || longitudes.len() != cells {
        return Err(ObservationError::Mrms(format!(
            "decoded MRMS grid/value length mismatch: values={}, lat={}, lon={}, expected={cells}",
            values.len(),
            latitudes.len(),
            longitudes.len()
        )));
    }
    if message.grid.scan_mode & 0x40 != 0 {
        flip_rows(&mut latitudes, nx, ny);
        flip_rows(&mut longitudes, nx, ny);
    }
    normalize_alternating_rows(&mut values, nx, ny, message.grid.scan_mode);
    normalize_alternating_rows(&mut latitudes, nx, ny, message.grid.scan_mode);
    normalize_alternating_rows(&mut longitudes, nx, ny, message.grid.scan_mode);

    let latitudes = latitudes.into_iter().map(|value| value as f32).collect();
    let longitudes = longitudes
        .into_iter()
        .map(|value| normalize_longitude(value) as f32)
        .collect();
    let grid = LatLonGrid::new(shape, latitudes, longitudes)?;
    let values = values.into_iter().map(|value| value as f32).collect();
    let parameter = parameter_name(
        message.discipline,
        message.product.parameter_category,
        message.product.parameter_number,
    );
    let variable = request.variable.clone().unwrap_or_else(|| {
        let name = sanitize_token(parameter);
        if name == "unknown" {
            format!("mrms_{}", sanitize_token(&request.product))
        } else {
            format!("mrms_{name}")
        }
    });
    let units = request.units.clone().unwrap_or_else(|| {
        parameter_units(
            message.discipline,
            message.product.parameter_category,
            message.product.parameter_number,
        )
        .to_string()
    });
    let valid_unix = message_valid_time(message).and_utc().timestamp();
    let selector = serde_json::json!({
        "mrms": {
            "product": request.product,
            "discipline": message.discipline,
            "parameter_category": message.product.parameter_category,
            "parameter_number": message.product.parameter_number,
            "parameter_name": parameter,
            "level_type": message.product.level_type,
            "level_value": message.product.level_value,
            "grib_template": message.product.template,
        }
    });
    Ok(ObservationFrame {
        family: ObservationFamily::Mrms,
        collection: request
            .collection
            .clone()
            .unwrap_or_else(|| "conus".to_string()),
        product: request.product.clone(),
        valid_unix,
        grid,
        projection: rustwx_io::grid_projection_from_grib2_grid(&message.grid),
        planes: vec![GridPlane {
            name: variable,
            units,
            selector,
            values,
        }],
        provenance_provider: "noaa-mrms".to_string(),
        provenance_roles: vec!["radar".to_string(), "mosaic".to_string()],
        provenance_products: vec![sanitize_token(&request.product)],
    })
}

fn message_valid_time(message: &Grib2Message) -> NaiveDateTime {
    if let Some(end) = message.product.end_of_interval {
        return end;
    }
    let amount = i64::from(message.product.forecast_time);
    let duration = match message.product.time_range_unit {
        0 => Duration::minutes(amount),
        1 => Duration::hours(amount),
        2 => Duration::days(amount),
        10 => Duration::hours(amount.saturating_mul(3)),
        11 => Duration::hours(amount.saturating_mul(6)),
        12 => Duration::hours(amount.saturating_mul(12)),
        13 => Duration::seconds(amount),
        _ => Duration::zero(),
    };
    message.reference_time + duration
}

fn normalize_longitude(mut longitude: f64) -> f64 {
    while longitude > 180.0 {
        longitude -= 360.0;
    }
    while longitude < -180.0 {
        longitude += 360.0;
    }
    longitude
}

fn normalize_alternating_rows(values: &mut [f64], nx: usize, ny: usize, scan_mode: u8) {
    // GRIB2 scan-mode bit 4 means adjacent rows scan in opposite directions.
    if scan_mode & 0x10 == 0 || values.len() != nx.saturating_mul(ny) {
        return;
    }
    for y in (1..ny).step_by(2) {
        values[y * nx..(y + 1) * nx].reverse();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alternating_scan_normalization_reverses_odd_rows() {
        let mut values = vec![1.0, 2.0, 4.0, 3.0];
        normalize_alternating_rows(&mut values, 2, 2, 0x10);
        assert_eq!(values, vec![1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn built_in_requests_have_stable_variables() {
        assert_eq!(
            MrmsIngestRequest::composite_reflectivity()
                .variable
                .as_deref(),
            Some("mrms_composite_reflectivity")
        );
    }
}

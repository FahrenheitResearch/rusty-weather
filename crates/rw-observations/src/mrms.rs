use std::path::Path;
use std::time::Duration as StdDuration;

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

const MRMS_DISCIPLINE: u8 = 209;

/// Officially published GRIB2 identity, unit string, and finite missing-data
/// codes for one MRMS product family this workspace follows.
///
/// Source of truth (never guessed): NOAA/NSSL "MRMS GRIB2 Tables" for
/// operational MRMS v12.2, `UserTable_MRMS_v12.2.csv` (columns
/// `Discipline,Category,Parameter,Name,Frequency,Unit,Missing,Range Folded,
/// No Coverage,Description,Notes`), NOAA National Severe Storms Laboratory
/// `mrms-support` repository, retrieved 2026-08-23:
/// https://raw.githubusercontent.com/NOAA-National-Severe-Storms-Laboratory/mrms-support/main/GRIB2_TABLES/UserTable_MRMS_v12.2.csv
#[derive(Debug, Clone, Copy, PartialEq)]
struct MrmsParameterContract {
    parameter_category: u8,
    parameter_number: u8,
    /// Exact `Unit` column value from the official table.
    units: &'static str,
    /// The table's finite `Missing` code: inside coverage, not measurable.
    missing: f64,
    /// The table's finite `No Coverage` code: outside the mosaic domain.
    no_coverage: f64,
}

/// Explicit per-identity contract table covering every MRMS product the
/// reference deployment follows (all discipline 209). Identities absent from
/// this table keep their upstream values untouched — sentinels and units are
/// never inferred from ranges, product-name heuristics, or recollection.
///
/// Rows transcribed verbatim from `UserTable_MRMS_v12.2.csv` (see the
/// [`MrmsParameterContract`] doc comment for the authoritative URL;
/// re-verified against the same file 2026-08-24):
///
/// ```text
/// 209,3,15,RotationTrackML60min,2-min,0.001/s,0,n/a,0,...
/// 209,3,27,POSH,2-min,%,-1,n/a,-3,...
/// 209,3,28,MESH,2-min,mm,-1,n/a,-3,...
/// 209,3,41,VIL,2-min,kg/m^2,-1,n/a,-3,...
/// 209,3,44,EchoTop_18,2-min,km MSL,-1,n/a,-3,...
/// 209,3,45,EchoTop_30,2-min,km MSL,-1,n/a,-3,...
/// 209,3,57,ReflectivityAtLowestAltitude,2-min,dBZ,-99,n/a,-999,...
/// 209,6,1,PrecipRate,2-min,mm/hr,-1,n/a,-3,...
/// 209,6,37,MultiSensor_QPE_01H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,6,39,MultiSensor_QPE_06H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,6,40,MultiSensor_QPE_12H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,6,41,MultiSensor_QPE_24H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,6,42,MultiSensor_QPE_48H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,6,43,MultiSensor_QPE_72H_Pass2,60-min,mm,-1,n/a,-3,...
/// 209,8,8,SeamlessHSR,2-min,dBZ,-99,n/a,-999,...
/// 209,10,0,MergedReflectivityQCComposite,2-min,dBZ,-99,n/a,-999,...
/// ```
///
/// RotationTrackML60min is the one identity whose official Missing and
/// No Coverage codes are the same finite value, `0`: NOAA publishes the
/// rotation-track swath with `0` as its only fill, so a zero cell is
/// upstream's "no rotation detected / no coverage" and is normalized to
/// transparent no-data exactly as the table prescribes. Positive azimuthal
/// shear is never touched.
const MRMS_PARAMETER_CONTRACTS: &[MrmsParameterContract] = &[
    // RotationTrackML60min
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 15,
        units: "0.001/s",
        missing: 0.0,
        no_coverage: 0.0,
    },
    // POSH
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 27,
        units: "%",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MESH
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 28,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // VIL
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 41,
        units: "kg/m^2",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // EchoTop_18
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 44,
        units: "km MSL",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // EchoTop_30
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 45,
        units: "km MSL",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // ReflectivityAtLowestAltitude
    MrmsParameterContract {
        parameter_category: 3,
        parameter_number: 57,
        units: "dBZ",
        missing: -99.0,
        no_coverage: -999.0,
    },
    // PrecipRate
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 1,
        units: "mm/hr",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_01H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 37,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_06H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 39,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_12H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 40,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_24H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 41,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_48H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 42,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // MultiSensor_QPE_72H_Pass2
    MrmsParameterContract {
        parameter_category: 6,
        parameter_number: 43,
        units: "mm",
        missing: -1.0,
        no_coverage: -3.0,
    },
    // SeamlessHSR
    MrmsParameterContract {
        parameter_category: 8,
        parameter_number: 8,
        units: "dBZ",
        missing: -99.0,
        no_coverage: -999.0,
    },
    // MergedReflectivityQCComposite
    MrmsParameterContract {
        parameter_category: 10,
        parameter_number: 0,
        units: "dBZ",
        missing: -99.0,
        no_coverage: -999.0,
    },
];

/// Look up the official contract for a decoded GRIB2 identity, or `None` for
/// any identity the deployment has not explicitly confirmed against the
/// NOAA/NSSL table.
fn mrms_parameter_contract(
    discipline: u8,
    parameter_category: u8,
    parameter_number: u8,
) -> Option<&'static MrmsParameterContract> {
    if discipline != MRMS_DISCIPLINE {
        return None;
    }
    MRMS_PARAMETER_CONTRACTS.iter().find(|contract| {
        contract.parameter_category == parameter_category
            && contract.parameter_number == parameter_number
    })
}

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

pub fn fetch_mrms_frame_with_policy(
    request: &MrmsIngestRequest,
    timeout: StdDuration,
    max_retries: u32,
) -> ObservationResult<ObservationFrame> {
    let bytes =
        rustwx_io::fetch_mrms_latest_product_with_policy(&request.product, timeout, max_retries)?;
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
    let sentinel_contract = mrms_parameter_contract(
        message.discipline,
        message.product.parameter_category,
        message.product.parameter_number,
    );
    if sentinel_contract.is_some() {
        normalize_mrms_sentinels(
            &mut values,
            message.discipline,
            message.product.parameter_category,
            message.product.parameter_number,
        );
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
    let units = resolve_mrms_units(
        request.units.as_deref(),
        message.discipline,
        message.product.parameter_category,
        message.product.parameter_number,
    );
    let valid_unix = message_valid_time(message).and_utc().timestamp();
    let mut mrms_selector = serde_json::json!({
        "product": request.product,
        "discipline": message.discipline,
        "parameter_category": message.product.parameter_category,
        "parameter_number": message.product.parameter_number,
        "parameter_name": parameter,
        "level_type": message.product.level_type,
        "level_value": message.product.level_value,
        "grib_template": message.product.template,
    });
    if let Some(contract) = sentinel_contract {
        mrms_selector["missing_value_contract"] = serde_json::json!({
            "missing": contract.missing,
            "no_coverage": contract.no_coverage,
            "normalized_to": "NaN",
        });
    }
    let selector = serde_json::json!({ "mrms": mrms_selector });
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

/// Convert the officially published finite Missing / No Coverage codes of a
/// confirmed MRMS identity into the store's non-finite missing
/// representation, returning the number of normalized cells.
///
/// The per-identity codes come from [`MRMS_PARAMETER_CONTRACTS`] — NOAA/NSSL
/// `UserTable_MRMS_v12.2.csv` — so a reflectivity plane only ever normalizes
/// `-99`/`-999` and a precipitation plane only ever normalizes `-1`/`-3`.
/// Nothing else is replaced: negative dBZ is valid reflectivity, and zero or
/// trace precipitation is valid data. Identities without a contract return
/// `None` and keep every upstream value untouched.
fn normalize_mrms_sentinels(
    values: &mut [f64],
    discipline: u8,
    parameter_category: u8,
    parameter_number: u8,
) -> Option<usize> {
    let contract = mrms_parameter_contract(discipline, parameter_category, parameter_number)?;
    let mut normalized = 0usize;
    for value in values {
        if *value == contract.missing || *value == contract.no_coverage {
            *value = f64::NAN;
            normalized += 1;
        }
    }
    Some(normalized)
}

/// Resolve the units string stored beside a decoded MRMS plane.
///
/// An explicit request value always wins. Otherwise identities confirmed in
/// [`MRMS_PARAMETER_CONTRACTS`] ship the exact `Unit` column value from the
/// official NOAA/NSSL v12.2 table (`dBZ`, `mm/hr`, `mm`), and everything else
/// falls back to the generic WMO/NCEP lookup with its explicit `?` unknown
/// marker rather than an invented value.
fn resolve_mrms_units(
    explicit_units: Option<&str>,
    discipline: u8,
    parameter_category: u8,
    parameter_number: u8,
) -> String {
    if let Some(units) = explicit_units {
        return units.to_owned();
    }
    if let Some(contract) =
        mrms_parameter_contract(discipline, parameter_category, parameter_number)
    {
        return contract.units.to_owned();
    }
    parameter_units(discipline, parameter_category, parameter_number).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ObservationFamily, ObservationInterpolation, ObservationValueSemantics,
        observation_display_hint,
    };

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

    // Reflectivity-family expectations pinned to the same official table
    // (see the authoritative-source comment below):
    //   209,3,57  ReflectivityAtLowestAltitude   dBZ  Missing -99  No Coverage -999
    //   209,8,8   SeamlessHSR                    dBZ  Missing -99  No Coverage -999
    //   209,10,0  MergedReflectivityQCComposite  dBZ  Missing -99  No Coverage -999
    #[test]
    fn official_reflectivity_sentinels_become_missing_without_masking_negative_dbz() {
        for (category, parameter) in [(3, 57), (8, 8), (10, 0)] {
            let mut values = vec![-999.0, -99.0, -20.0, -0.5, 0.0, 65.0, f64::NAN];
            let normalized =
                normalize_mrms_sentinels(&mut values, MRMS_DISCIPLINE, category, parameter);
            assert_eq!(normalized, Some(2), "identity 209/{category}/{parameter}");
            assert!(values[0].is_nan());
            assert!(values[1].is_nan());
            assert_eq!(&values[2..6], &[-20.0, -0.5, 0.0, 65.0]);
            assert!(values[6].is_nan());
        }
    }

    // Identity, unit, and sentinel expectations below are pinned to the
    // official NOAA/NSSL MRMS GRIB2 user table for operational MRMS v12.2,
    // "UserTable_MRMS_v12.2.csv" (columns Discipline,Category,Parameter,Name,
    // Frequency,Unit,Missing,Range Folded,No Coverage,...), NOAA National
    // Severe Storms Laboratory mrms-support repository, retrieved 2026-08-23:
    // https://raw.githubusercontent.com/NOAA-National-Severe-Storms-Laboratory/mrms-support/main/GRIB2_TABLES/UserTable_MRMS_v12.2.csv
    //   209,6,1   PrecipRate                 mm/hr  Missing -1  No Coverage -3
    //   209,6,37  MultiSensor_QPE_01H_Pass2  mm     Missing -1  No Coverage -3
    //   209,6,39  MultiSensor_QPE_06H_Pass2  mm     Missing -1  No Coverage -3
    //   209,6,40  MultiSensor_QPE_12H_Pass2  mm     Missing -1  No Coverage -3
    //   209,6,41  MultiSensor_QPE_24H_Pass2  mm     Missing -1  No Coverage -3
    //   209,6,42  MultiSensor_QPE_48H_Pass2  mm     Missing -1  No Coverage -3
    //   209,6,43  MultiSensor_QPE_72H_Pass2  mm     Missing -1  No Coverage -3
    #[test]
    fn official_precip_sentinels_become_missing_without_masking_zero_or_trace() {
        for (category, parameter) in [(6, 1), (6, 37), (6, 39), (6, 40), (6, 41), (6, 42), (6, 43)]
        {
            let mut values = vec![-3.0, -1.0, 0.0, 0.02, 12.5, 250.0, f64::NAN];
            let normalized =
                normalize_mrms_sentinels(&mut values, MRMS_DISCIPLINE, category, parameter);
            assert_eq!(normalized, Some(2), "identity 209/{category}/{parameter}");
            assert!(values[0].is_nan());
            assert!(values[1].is_nan());
            assert_eq!(&values[2..6], &[0.0, 0.02, 12.5, 250.0]);
            assert!(values[6].is_nan());
        }
    }

    // Severe-weather diagnostics pinned to the same official table:
    //   209,3,27  POSH        %        Missing -1  No Coverage -3
    //   209,3,28  MESH        mm       Missing -1  No Coverage -3
    //   209,3,41  VIL         kg/m^2   Missing -1  No Coverage -3
    //   209,3,44  EchoTop_18  km MSL   Missing -1  No Coverage -3
    //   209,3,45  EchoTop_30  km MSL   Missing -1  No Coverage -3
    #[test]
    fn official_severe_weather_sentinels_become_missing_without_masking_zero() {
        for (category, parameter) in [(3, 27), (3, 28), (3, 41), (3, 44), (3, 45)] {
            let mut values = vec![-3.0, -1.0, 0.0, 0.5, 12.5, 70.0, f64::NAN];
            let normalized =
                normalize_mrms_sentinels(&mut values, MRMS_DISCIPLINE, category, parameter);
            assert_eq!(normalized, Some(2), "identity 209/{category}/{parameter}");
            assert!(values[0].is_nan());
            assert!(values[1].is_nan());
            assert_eq!(&values[2..6], &[0.0, 0.5, 12.5, 70.0]);
            assert!(values[6].is_nan());
        }
    }

    // 209,3,15 RotationTrackML60min, 0.001/s, is the one followed identity
    // whose official Missing and No Coverage codes are both the finite value
    // 0: NOAA fills the swath with 0 wherever no rotation was detected or no
    // coverage exists, so 0 normalizes to transparent no-data while every
    // positive azimuthal-shear value survives. The other families' codes
    // (-1/-3, -99/-999) are NOT sentinels for this identity and pass through.
    #[test]
    fn rotation_track_zero_fill_becomes_missing_without_masking_shear() {
        let mut values = vec![0.0, -0.0, 0.001, 2.0, 12.0, -1.0, -3.0, -99.0, -999.0];
        let normalized = normalize_mrms_sentinels(&mut values, MRMS_DISCIPLINE, 3, 15);
        assert_eq!(normalized, Some(2));
        assert!(values[0].is_nan());
        assert!(values[1].is_nan());
        assert_eq!(&values[2..], &[0.001, 2.0, 12.0, -1.0, -3.0, -99.0, -999.0]);
    }

    #[test]
    fn precip_identities_do_not_borrow_the_reflectivity_sentinel_codes() {
        // -99/-999 are the reflectivity-family codes (209/3/57, 209/10/0).
        // The precipitation identities use -1/-3 per the official table, so
        // -99/-999 must pass through as (implausible but explicit) data.
        let mut values = vec![-999.0, -99.0];
        assert_eq!(
            normalize_mrms_sentinels(&mut values, MRMS_DISCIPLINE, 6, 1),
            Some(0)
        );
        assert_eq!(values, vec![-999.0, -99.0]);
    }

    #[test]
    fn authoritative_precip_units_resolve_from_the_official_table() {
        // Unit column of UserTable_MRMS_v12.2.csv (see module comment above).
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 6, 1), "mm/hr");
        for parameter in [37, 39, 40, 41, 42, 43] {
            assert_eq!(
                resolve_mrms_units(None, MRMS_DISCIPLINE, 6, parameter),
                "mm"
            );
        }
        assert_eq!(
            resolve_mrms_units(Some("explicit-test-units"), MRMS_DISCIPLINE, 6, 1),
            "explicit-test-units"
        );
        // 209/6/2 (RadarOnly_QPE_01H) exists upstream but is not a configured
        // deployment product: it must keep the generic unknown-units marker
        // rather than a guessed value.
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 6, 2), "?");
    }

    #[test]
    fn authoritative_severe_weather_units_resolve_from_the_official_table() {
        // Unit column of UserTable_MRMS_v12.2.csv (see module comment above).
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 15), "0.001/s");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 27), "%");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 28), "mm");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 41), "kg/m^2");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 44), "km MSL");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 3, 45), "km MSL");
        assert_eq!(resolve_mrms_units(None, MRMS_DISCIPLINE, 8, 8), "dBZ");
    }

    #[test]
    fn finite_codes_are_untouched_for_identities_outside_the_contract_table() {
        // 209/6/2 (RadarOnly_QPE_01H) is published upstream but is not a
        // configured deployment product, and discipline 0 is ordinary WMO
        // meteorological data. Neither may be normalized: absent an explicit
        // per-identity contract the upstream values pass through unchanged.
        for (discipline, category, parameter) in
            [(MRMS_DISCIPLINE, 6, 2), (0, 6, 1), (MRMS_DISCIPLINE, 6, 0)]
        {
            let mut values = vec![-999.0, -99.0, -3.0, -1.0];
            assert_eq!(
                normalize_mrms_sentinels(&mut values, discipline, category, parameter),
                None,
                "identity {discipline}/{category}/{parameter}"
            );
            assert_eq!(values, vec![-999.0, -99.0, -3.0, -1.0]);
        }
    }

    #[test]
    fn configured_reflectivity_variables_resolve_the_reflectivity_display_contract() {
        // Deployed variable names carry "reflectivity", and the official
        // units are dBZ, so the client display contract must resolve to the
        // reflectivity palette family, never generic grayscale.
        for variable in [
            "mrms_reflectivity_lowest_altitude",
            "mrms_composite_reflectivity",
            "mrms_seamless_hybrid_scan_reflectivity",
        ] {
            let hint = observation_display_hint(ObservationFamily::Mrms, variable, "dBZ");
            assert_eq!(
                hint.semantics,
                ObservationValueSemantics::Reflectivity,
                "{variable}"
            );
            assert_eq!(hint.palette, "reflectivity");
            assert_eq!(hint.interpolation, ObservationInterpolation::Linear);
            assert!(hint.transparent_non_finite);
            assert_eq!(hint.preferred_range, Some([-32.0, 95.0]));
        }
    }

    #[test]
    fn configured_precip_rate_variable_resolves_the_rate_display_contract() {
        let hint = observation_display_hint(ObservationFamily::Mrms, "mrms_precip_rate", "mm/hr");
        assert_eq!(hint.semantics, ObservationValueSemantics::Precipitation);
        assert_eq!(hint.palette, "precipitation");
        assert_eq!(hint.interpolation, ObservationInterpolation::Linear);
        assert!(hint.transparent_non_finite);
        // Rate units (per hour) select the 0..=100 mm/hr presentation range.
        assert_eq!(hint.preferred_range, Some([0.0, 100.0]));
    }

    #[test]
    fn configured_qpe_accumulation_variables_resolve_the_accumulation_display_contract() {
        for variable in [
            "mrms_precip_accum_1h",
            "mrms_precip_accum_6h",
            "mrms_precip_accum_12h",
            "mrms_precip_accum_24h",
            "mrms_precip_accum_48h",
            "mrms_precip_accum_72h",
        ] {
            let hint = observation_display_hint(ObservationFamily::Mrms, variable, "mm");
            assert_eq!(
                hint.semantics,
                ObservationValueSemantics::Precipitation,
                "{variable}"
            );
            assert_eq!(hint.palette, "precipitation");
            assert_eq!(hint.interpolation, ObservationInterpolation::Linear);
            assert!(hint.transparent_non_finite);
            // Accumulation units (plain mm) select the 0..=250 mm range.
            assert_eq!(hint.preferred_range, Some([0.0, 250.0]));
        }
    }

    #[test]
    fn configured_vil_variable_resolves_the_vil_display_contract() {
        let hint = observation_display_hint(ObservationFamily::Mrms, "mrms_vil", "kg/m^2");
        assert_eq!(
            hint.semantics,
            ObservationValueSemantics::VerticallyIntegratedLiquid
        );
        assert_eq!(hint.palette, "vil");
        assert_eq!(hint.interpolation, ObservationInterpolation::Linear);
        assert!(hint.transparent_non_finite);
        assert_eq!(hint.preferred_range, Some([0.0, 80.0]));
    }

    #[test]
    fn configured_echo_top_variables_resolve_the_echo_top_display_contract() {
        for variable in ["mrms_echo_top_18", "mrms_echo_top_30"] {
            let hint = observation_display_hint(ObservationFamily::Mrms, variable, "km MSL");
            assert_eq!(
                hint.semantics,
                ObservationValueSemantics::EchoTop,
                "{variable}"
            );
            assert_eq!(hint.palette, "echo_top");
            assert_eq!(hint.interpolation, ObservationInterpolation::Linear);
            assert!(hint.transparent_non_finite);
            // "km MSL" units select the kilometre presentation range.
            assert_eq!(hint.preferred_range, Some([0.0, 20.0]));
        }
    }

    #[test]
    fn hail_and_rotation_variables_stay_generic_rather_than_borrowing_a_palette() {
        // MESH ("mm"), POSH ("%"), and the rotation-track swath ("0.001/s")
        // have no dedicated palette family yet. They must resolve the honest
        // generic-scalar contract — in particular MESH's plain "mm" must NOT
        // borrow the precipitation-accumulation palette, and none of them may
        // read as reflectivity.
        for (variable, units) in [
            ("mrms_mesh", "mm"),
            ("mrms_posh", "%"),
            ("mrms_rotation_track_ml_60min", "0.001/s"),
        ] {
            let hint = observation_display_hint(ObservationFamily::Mrms, variable, units);
            assert_eq!(
                hint.semantics,
                ObservationValueSemantics::GenericScalar,
                "{variable}"
            );
            assert_eq!(hint.palette, "generic_scalar");
        }
    }

    #[test]
    fn authoritative_reflectivity_units_fill_unknown_table_entries_without_overriding_requests() {
        for (category, parameter) in [(3, 57), (8, 8), (10, 0)] {
            assert_eq!(
                resolve_mrms_units(None, MRMS_DISCIPLINE, category, parameter),
                "dBZ"
            );
            assert_eq!(
                resolve_mrms_units(
                    Some("explicit-test-units"),
                    MRMS_DISCIPLINE,
                    category,
                    parameter,
                ),
                "explicit-test-units"
            );
        }
        assert_ne!(resolve_mrms_units(None, MRMS_DISCIPLINE, 6, 1), "dBZ");
    }
}

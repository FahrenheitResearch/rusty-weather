//! Decode + compute kernel for windowed products.
//!
//! This module owns the GRIB2 message decode for APCP, native UH, native
//! 10 m wind-max fields, and 2 m surface snapshots as well as the
//! per-product window-compute kernels. It is deliberately separated from the batch orchestration in
//! [`crate::windowed`] so non-HRRR windowed products can plug in later
//! without dragging the HRRR-specific runner along.
//!
//! The orchestrator in `windowed.rs` fetches bytes through the planner
//! + runtime and then hands them here. Everything in this module is
//! pure given bytes (plus the cache path when the caller opts in) - it
//! does no I/O of its own beyond the optional bincode cache.
use crate::cache::{load_bincode, store_bincode};
use crate::windowed::{HrrrWindowedProduct, HrrrWindowedProductMetadata};
use grib_core::grib2::{Grib2File, Grib2Message, unpack_message_normalized};
use rustwx_calc::{max_window_fields, sum_window_fields};
use rustwx_core::{Field2D, ProductKey};
use rustwx_render::{
    Color, ColorScale, DiscreteColorScale, ExtendMode, WeatherPalette, WeatherProduct,
    palette_scale,
    weather::{dewpoint_palette_celsius_for_levels, temperature_palette_cropped_f},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

const MM_PER_INCH: f64 = 25.4;
const MS_TO_KT: f64 = 1.943_844_5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct WindowedFieldRecord {
    pub(crate) hours: u16,
    pub(crate) values: Vec<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HrrrApcpDecode {
    pub(crate) windows: Vec<WindowedFieldRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HrrrUhDecode {
    pub(crate) windows: Vec<WindowedFieldRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HrrrWind10mMaxDecode {
    pub(crate) windows: Vec<WindowedFieldRecord>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct HrrrSurfaceSnapshotDecode {
    pub(crate) temp2m_k: Option<Vec<f64>>,
    pub(crate) rh2m_pct: Option<Vec<f64>>,
    pub(crate) dewpoint2m_k: Option<Vec<f64>>,
}

#[derive(Debug, Clone)]
pub(crate) struct ComputedWindowedField {
    pub(crate) field: Field2D,
    pub(crate) title: String,
    pub(crate) metadata: HrrrWindowedProductMetadata,
    pub(crate) scale: ColorScale,
}

pub(crate) fn load_or_decode_apcp(
    path: &Path,
    bytes: &[u8],
    use_cache: bool,
    fallback_window_hours: Option<u16>,
) -> Result<HrrrApcpDecode, Box<dyn std::error::Error>> {
    if use_cache {
        if let Some(cached) = load_bincode::<HrrrApcpDecode>(path)? {
            return Ok(cached);
        }
    }
    let decoded = decode_apcp(bytes, fallback_window_hours)?;
    if use_cache {
        store_bincode(path, &decoded)?;
    }
    Ok(decoded)
}

pub(crate) fn load_or_decode_uh25(
    path: &Path,
    bytes: &[u8],
    use_cache: bool,
) -> Result<HrrrUhDecode, Box<dyn std::error::Error>> {
    if use_cache {
        if let Some(cached) = load_bincode::<HrrrUhDecode>(path)? {
            return Ok(cached);
        }
    }
    let decoded = decode_uh25(bytes)?;
    if use_cache {
        store_bincode(path, &decoded)?;
    }
    Ok(decoded)
}

pub(crate) fn load_or_decode_wind10m_max(
    path: &Path,
    bytes: &[u8],
    use_cache: bool,
) -> Result<HrrrWind10mMaxDecode, Box<dyn std::error::Error>> {
    if use_cache {
        if let Some(cached) = load_bincode::<HrrrWind10mMaxDecode>(path)? {
            return Ok(cached);
        }
    }
    let decoded = decode_wind10m_max(bytes)?;
    if use_cache {
        store_bincode(path, &decoded)?;
    }
    Ok(decoded)
}

pub(crate) fn load_or_decode_surface_snapshot(
    path: &Path,
    bytes: &[u8],
    use_cache: bool,
) -> Result<HrrrSurfaceSnapshotDecode, Box<dyn std::error::Error>> {
    if use_cache {
        if let Some(cached) = load_bincode::<HrrrSurfaceSnapshotDecode>(path)? {
            return Ok(cached);
        }
    }
    let decoded = decode_surface_snapshot(bytes)?;
    if use_cache {
        store_bincode(path, &decoded)?;
    }
    Ok(decoded)
}

pub(crate) fn decode_apcp(
    bytes: &[u8],
    fallback_window_hours: Option<u16>,
) -> Result<HrrrApcpDecode, Box<dyn std::error::Error>> {
    let grib = Grib2File::from_bytes(bytes)?;
    let mut windows = Vec::new();
    for message in &grib.messages {
        if message.discipline == 0
            && message.product.parameter_category == 1
            && message.product.parameter_number == 8
            && message.product.level_type == 1
        {
            let hours = time_range_hours(message)
                .or(fallback_window_hours)
                .ok_or("APCP message missing hourly time-range metadata")?;
            if windows
                .iter()
                .any(|record: &WindowedFieldRecord| record.hours == hours)
            {
                continue;
            }
            windows.push(WindowedFieldRecord {
                hours,
                values: unpack_message_normalized(message)?,
            });
        }
    }
    if windows.is_empty() {
        return Err("no APCP surface accumulation fields were found in subset".into());
    }
    windows.sort_by_key(|record| record.hours);
    Ok(HrrrApcpDecode { windows })
}

pub(crate) fn decode_uh25(bytes: &[u8]) -> Result<HrrrUhDecode, Box<dyn std::error::Error>> {
    let grib = Grib2File::from_bytes(bytes)?;
    let mut windows = Vec::new();
    for message in &grib.messages {
        if is_uh25_message(message) {
            let hours = time_range_hours(message)
                .ok_or("native UH message missing hourly max-window metadata")?;
            if windows
                .iter()
                .any(|record: &WindowedFieldRecord| record.hours == hours)
            {
                continue;
            }
            windows.push(WindowedFieldRecord {
                hours,
                values: unpack_message_normalized(message)?,
            });
        }
    }
    if windows.is_empty() {
        return Err("no native 2-5 km UH max fields were found in subset".into());
    }
    windows.sort_by_key(|record| record.hours);
    Ok(HrrrUhDecode { windows })
}

pub(crate) fn decode_wind10m_max(
    bytes: &[u8],
) -> Result<HrrrWind10mMaxDecode, Box<dyn std::error::Error>> {
    let grib = Grib2File::from_bytes(bytes)?;
    let mut windows = Vec::new();
    for message in &grib.messages {
        if is_wind10m_max_message(message) {
            let hours = time_range_hours(message)
                .ok_or("native 10 m wind max message missing hourly max-window metadata")?;
            if windows
                .iter()
                .any(|record: &WindowedFieldRecord| record.hours == hours)
            {
                continue;
            }
            windows.push(WindowedFieldRecord {
                hours,
                values: unpack_message_normalized(message)?,
            });
        }
    }
    if windows.is_empty() {
        return Err("no native 10 m wind max fields were found in subset".into());
    }
    windows.sort_by_key(|record| record.hours);
    Ok(HrrrWind10mMaxDecode { windows })
}

pub(crate) fn decode_surface_snapshot(
    bytes: &[u8],
) -> Result<HrrrSurfaceSnapshotDecode, Box<dyn std::error::Error>> {
    let grib = Grib2File::from_bytes(bytes)?;
    let mut decoded = HrrrSurfaceSnapshotDecode::default();
    for message in &grib.messages {
        if is_temp2m_message(message) {
            decoded.temp2m_k = Some(unpack_message_normalized(message)?);
        } else if is_rh2m_message(message) {
            decoded.rh2m_pct = Some(unpack_message_normalized(message)?);
        } else if is_dewpoint2m_message(message) {
            decoded.dewpoint2m_k = Some(unpack_message_normalized(message)?);
        }
    }
    if decoded.temp2m_k.is_none() && decoded.rh2m_pct.is_none() && decoded.dewpoint2m_k.is_none() {
        return Err("no native 2 m temperature/RH/dewpoint fields were found in subset".into());
    }
    Ok(decoded)
}

pub(crate) fn is_uh25_message(message: &Grib2Message) -> bool {
    matches!(
        (
            message.product.parameter_category,
            message.product.parameter_number
        ),
        (7, 199) | (7, 15)
    ) && matches!(message.product.level_type, 103 | 118)
        && (message.product.level_value - 5000.0).abs() < 0.25
}

pub(crate) fn is_wind10m_max_message(message: &Grib2Message) -> bool {
    message.discipline == 0
        && message.product.parameter_category == 2
        && message.product.parameter_number == 1
        && message.product.level_type == 103
        && (message.product.level_value - 10.0).abs() < 0.25
        && time_range_hours(message).is_some()
}

pub(crate) fn is_temp2m_message(message: &Grib2Message) -> bool {
    message.discipline == 0
        && message.product.parameter_category == 0
        && message.product.parameter_number == 0
        && message.product.level_type == 103
        && (message.product.level_value - 2.0).abs() < 0.25
}

pub(crate) fn is_rh2m_message(message: &Grib2Message) -> bool {
    message.discipline == 0
        && message.product.parameter_category == 1
        && message.product.parameter_number == 1
        && message.product.level_type == 103
        && (message.product.level_value - 2.0).abs() < 0.25
}

pub(crate) fn is_dewpoint2m_message(message: &Grib2Message) -> bool {
    message.discipline == 0
        && message.product.parameter_category == 0
        && message.product.parameter_number == 6
        && message.product.level_type == 103
        && (message.product.level_value - 2.0).abs() < 0.25
}

pub(crate) fn time_range_hours(message: &Grib2Message) -> Option<u16> {
    message.product.statistical_time_range_hours()
}

pub(crate) fn compute_qpf_product(
    product: HrrrWindowedProduct,
    forecast_hour: u16,
    grid: &rustwx_core::LatLonGrid,
    apcp_by_hour: &BTreeMap<u16, Result<HrrrApcpDecode, String>>,
) -> Result<ComputedWindowedField, String> {
    let (window_hours, title) = match product {
        HrrrWindowedProduct::Qpf1h => (Some(1), "1-h QPF"),
        HrrrWindowedProduct::Qpf6h => (Some(6), "6-h QPF"),
        HrrrWindowedProduct::Qpf12h => (Some(12), "12-h QPF"),
        HrrrWindowedProduct::Qpf24h => (Some(24), "24-h QPF"),
        HrrrWindowedProduct::QpfTotal => (None, "Total QPF"),
        _ => return Err(format!("{} is not a QPF product", product.slug())),
    };

    let (values_mm, strategy, contributing_hours) = match window_hours {
        Some(window) => {
            let end = apcp_by_hour
                .get(&forecast_hour)
                .ok_or_else(|| format!("missing APCP fetch for F{:03}", forecast_hour))?
                .as_ref()
                .map_err(Clone::clone)?;
            if let Some(direct) = select_window(&end.windows, window) {
                (
                    direct.to_vec(),
                    format!("direct APCP {}h accumulation", window),
                    vec![forecast_hour],
                )
            } else {
                let start_hour = forecast_hour + 1 - window;
                let hours = (start_hour..=forecast_hour).collect::<Vec<_>>();
                let increments = collect_apcp_windows(apcp_by_hour, &hours, 1)?;
                (
                    sum_window_fields(grid.shape, &increments).map_err(|err| err.to_string())?,
                    format!("sum of {} hourly APCP increments", window),
                    hours,
                )
            }
        }
        None => {
            let end = apcp_by_hour
                .get(&forecast_hour)
                .ok_or_else(|| format!("missing APCP fetch for F{:03}", forecast_hour))?
                .as_ref()
                .map_err(Clone::clone)?;
            if let Some(direct) = select_window(&end.windows, forecast_hour) {
                (
                    direct.to_vec(),
                    format!("direct APCP {}h accumulation", forecast_hour),
                    vec![forecast_hour],
                )
            } else {
                let hours = (1..=forecast_hour).collect::<Vec<_>>();
                let increments = collect_apcp_windows(apcp_by_hour, &hours, 1)?;
                (
                    sum_window_fields(grid.shape, &increments).map_err(|err| err.to_string())?,
                    "sum of all available hourly APCP increments".to_string(),
                    hours,
                )
            }
        }
    };

    let values_in = values_mm
        .into_iter()
        .map(|value| value / MM_PER_INCH)
        .collect::<Vec<_>>();
    let field = Field2D::new(
        ProductKey::named(product.slug()),
        "in",
        grid.clone(),
        values_in.iter().map(|&value| value as f32).collect(),
    )
    .map_err(|err| err.to_string())?;

    Ok(ComputedWindowedField {
        field,
        title: title.to_string(),
        metadata: HrrrWindowedProductMetadata {
            strategy,
            contributing_forecast_hours: contributing_hours,
            window_hours,
        },
        scale: ColorScale::Discrete(qpf_scale()),
    })
}

pub(crate) fn qpf_fallback_hours_if_direct_missing(
    product: HrrrWindowedProduct,
    forecast_hour: u16,
    apcp_by_hour: &BTreeMap<u16, Result<HrrrApcpDecode, String>>,
) -> Option<Vec<u16>> {
    let direct_window = match product {
        HrrrWindowedProduct::Qpf1h => 1,
        HrrrWindowedProduct::Qpf6h => 6,
        HrrrWindowedProduct::Qpf12h => 12,
        HrrrWindowedProduct::Qpf24h => 24,
        HrrrWindowedProduct::QpfTotal => forecast_hour,
        _ => return None,
    };
    let end = apcp_by_hour.get(&forecast_hour)?.as_ref().ok()?;
    if select_window(&end.windows, direct_window).is_some() {
        return None;
    }
    match product {
        HrrrWindowedProduct::Qpf1h => Some(vec![forecast_hour]),
        HrrrWindowedProduct::Qpf6h if forecast_hour >= 6 => {
            Some(((forecast_hour - 5)..=forecast_hour).collect())
        }
        HrrrWindowedProduct::Qpf12h if forecast_hour >= 12 => {
            Some(((forecast_hour - 11)..=forecast_hour).collect())
        }
        HrrrWindowedProduct::Qpf24h if forecast_hour >= 24 => {
            Some(((forecast_hour - 23)..=forecast_hour).collect())
        }
        HrrrWindowedProduct::QpfTotal if forecast_hour >= 1 => Some((1..=forecast_hour).collect()),
        _ => None,
    }
}

pub(crate) fn compute_uh_product(
    product: HrrrWindowedProduct,
    forecast_hour: u16,
    grid: &rustwx_core::LatLonGrid,
    uh_by_hour: &BTreeMap<u16, Result<HrrrUhDecode, String>>,
) -> Result<ComputedWindowedField, String> {
    let (values, strategy, contributing_hours, window_hours) = match product {
        HrrrWindowedProduct::Uh25km1h => {
            let decoded = uh_by_hour
                .get(&forecast_hour)
                .ok_or_else(|| format!("missing native UH fetch for F{:03}", forecast_hour))?
                .as_ref()
                .map_err(Clone::clone)?;
            let values = select_window(&decoded.windows, 1)
                .ok_or_else(|| format!("native UH F{:03} missing 1-hour max field", forecast_hour))?
                .to_vec();
            (
                values,
                "direct native 1-hour UH max".to_string(),
                vec![forecast_hour],
                Some(1),
            )
        }
        HrrrWindowedProduct::Uh25km3h => {
            let hours = ((forecast_hour - 2)..=forecast_hour).collect::<Vec<_>>();
            let windows = collect_uh_windows(uh_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "max of native hourly UH maxima across trailing 3 hours".to_string(),
                hours,
                Some(3),
            )
        }
        HrrrWindowedProduct::Uh25kmRunMax => {
            let hours = (1..=forecast_hour).collect::<Vec<_>>();
            let windows = collect_uh_windows(uh_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "run max of native hourly UH maxima".to_string(),
                hours,
                None,
            )
        }
        _ => return Err(format!("{} is not a UH product", product.slug())),
    };

    let field = Field2D::new(
        ProductKey::named(product.slug()),
        "m^2/s^2",
        grid.clone(),
        values.iter().map(|&value| value as f32).collect(),
    )
    .map_err(|err| err.to_string())?;

    Ok(ComputedWindowedField {
        field,
        title: product.title().to_string(),
        metadata: HrrrWindowedProductMetadata {
            strategy,
            contributing_forecast_hours: contributing_hours,
            window_hours,
        },
        scale: ColorScale::Weather(WeatherProduct::Uh.scale_preset()),
    })
}

pub(crate) fn compute_wind10m_product(
    product: HrrrWindowedProduct,
    forecast_hour: u16,
    grid: &rustwx_core::LatLonGrid,
    wind_by_hour: &BTreeMap<u16, Result<HrrrWind10mMaxDecode, String>>,
) -> Result<ComputedWindowedField, String> {
    let (values_ms, strategy, contributing_hours, window_hours) = match product {
        HrrrWindowedProduct::Wind10m1hMax => {
            let decoded = wind_by_hour
                .get(&forecast_hour)
                .ok_or_else(|| {
                    format!(
                        "missing native 10 m wind max fetch for F{:03}",
                        forecast_hour
                    )
                })?
                .as_ref()
                .map_err(Clone::clone)?;
            let values = select_window(&decoded.windows, 1)
                .ok_or_else(|| {
                    format!(
                        "native 10 m wind F{:03} missing 1-hour max field",
                        forecast_hour
                    )
                })?
                .to_vec();
            (
                values,
                "direct native 1-hour 10 m wind max".to_string(),
                vec![forecast_hour],
                Some(1),
            )
        }
        HrrrWindowedProduct::Wind10mRunMax => {
            let hours = (1..=forecast_hour).collect::<Vec<_>>();
            let windows = collect_wind10m_windows(wind_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "run max of native hourly 10 m wind maxima".to_string(),
                hours,
                None,
            )
        }
        HrrrWindowedProduct::Wind10m0to24hMax => {
            let hours = (1..=24).collect::<Vec<_>>();
            let windows = collect_wind10m_windows(wind_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "max of native hourly 10 m wind maxima across F001-F024".to_string(),
                hours,
                Some(24),
            )
        }
        HrrrWindowedProduct::Wind10m24to48hMax => {
            let hours = (25..=48).collect::<Vec<_>>();
            let windows = collect_wind10m_windows(wind_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "max of native hourly 10 m wind maxima across F025-F048".to_string(),
                hours,
                Some(24),
            )
        }
        HrrrWindowedProduct::Wind10m0to48hMax => {
            let hours = (1..=48).collect::<Vec<_>>();
            let windows = collect_wind10m_windows(wind_by_hour, &hours, 1)?;
            (
                max_window_fields(grid.shape, &windows).map_err(|err| err.to_string())?,
                "max of native hourly 10 m wind maxima across F001-F048".to_string(),
                hours,
                Some(48),
            )
        }
        _ => return Err(format!("{} is not a 10 m wind product", product.slug())),
    };

    let values_kt = values_ms
        .into_iter()
        .map(|value| value * MS_TO_KT)
        .collect::<Vec<_>>();
    let field = Field2D::new(
        ProductKey::named(product.slug()),
        "kt",
        grid.clone(),
        values_kt.iter().map(|&value| value as f32).collect(),
    )
    .map_err(|err| err.to_string())?;

    Ok(ComputedWindowedField {
        field,
        title: product.title().to_string(),
        metadata: HrrrWindowedProductMetadata {
            strategy,
            contributing_forecast_hours: contributing_hours,
            window_hours,
        },
        scale: ColorScale::Discrete(wind10m_scale()),
    })
}

pub(crate) fn compute_surface_snapshot_product(
    product: HrrrWindowedProduct,
    grid: &rustwx_core::LatLonGrid,
    snapshot_by_hour: &BTreeMap<u16, Result<HrrrSurfaceSnapshotDecode, String>>,
) -> Result<ComputedWindowedField, String> {
    let spec = surface_snapshot_window_spec(product).ok_or_else(|| {
        format!(
            "{} is not a 2 m surface snapshot window product",
            product.slug()
        )
    })?;
    let windows = collect_surface_snapshot_values(snapshot_by_hour, &spec.hours, spec.field)?;
    let window_refs = windows.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let values = match spec.operation {
        SurfaceSnapshotWindowOp::Max => {
            max_window_fields(grid.shape, &window_refs).map_err(|err| err.to_string())?
        }
        SurfaceSnapshotWindowOp::Min => min_window_fields(grid.shape, &window_refs)?,
        SurfaceSnapshotWindowOp::Range => {
            let max_values =
                max_window_fields(grid.shape, &window_refs).map_err(|err| err.to_string())?;
            let min_values = min_window_fields(grid.shape, &window_refs)?;
            max_values
                .into_iter()
                .zip(min_values)
                .map(|(max_value, min_value)| max_value - min_value)
                .collect::<Vec<_>>()
        }
    };

    let field = Field2D::new(
        ProductKey::named(product.slug()),
        spec.field.units(),
        grid.clone(),
        values.iter().map(|&value| value as f32).collect(),
    )
    .map_err(|err| err.to_string())?;
    let operation_label = spec.operation.label();

    Ok(ComputedWindowedField {
        field,
        title: product.title().to_string(),
        metadata: HrrrWindowedProductMetadata {
            strategy: format!(
                "pointwise {operation_label} of hourly {} snapshots across {}",
                spec.field.label(),
                spec.window_label
            ),
            contributing_forecast_hours: spec.hours,
            window_hours: spec.window_hours,
        },
        scale: ColorScale::Discrete(spec.field.scale(spec.operation)),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceSnapshotField {
    Temp2m,
    Rh2m,
    Dewpoint2m,
    Vpd2m,
}

impl SurfaceSnapshotField {
    fn label(self) -> &'static str {
        match self {
            Self::Temp2m => "2 m temperature",
            Self::Rh2m => "2 m relative humidity",
            Self::Dewpoint2m => "2 m dewpoint",
            Self::Vpd2m => "2 m vapor pressure deficit",
        }
    }

    fn units(self) -> &'static str {
        match self {
            Self::Temp2m | Self::Dewpoint2m => "degC",
            Self::Rh2m => "%",
            Self::Vpd2m => "hPa",
        }
    }

    fn scale(self, operation: SurfaceSnapshotWindowOp) -> DiscreteColorScale {
        match self {
            Self::Temp2m => {
                if operation == SurfaceSnapshotWindowOp::Range {
                    temp2m_range_scale()
                } else {
                    temp2m_scale()
                }
            }
            Self::Rh2m => rh2m_scale(operation == SurfaceSnapshotWindowOp::Range),
            Self::Dewpoint2m => {
                if operation == SurfaceSnapshotWindowOp::Range {
                    temp2m_range_scale()
                } else {
                    dewpoint2m_scale()
                }
            }
            Self::Vpd2m => vpd2m_scale(operation == SurfaceSnapshotWindowOp::Range),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SurfaceSnapshotWindowOp {
    Max,
    Min,
    Range,
}

impl SurfaceSnapshotWindowOp {
    fn label(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Min => "min",
            Self::Range => "max-min range",
        }
    }
}

#[derive(Debug, Clone)]
struct SurfaceSnapshotWindowSpec {
    field: SurfaceSnapshotField,
    operation: SurfaceSnapshotWindowOp,
    hours: Vec<u16>,
    window_hours: Option<u16>,
    window_label: &'static str,
}

fn surface_snapshot_window_spec(product: HrrrWindowedProduct) -> Option<SurfaceSnapshotWindowSpec> {
    use HrrrWindowedProduct::*;
    let (field, operation, start, end, window_hours, window_label) = match product {
        Temp2m0to24hMax => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Temp2m24to48hMax => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Max,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Temp2m0to48hMax => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Temp2m0to24hMin => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Temp2m24to48hMin => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Min,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Temp2m0to48hMin => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Temp2m0to24hRange => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Temp2m24to48hRange => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Range,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Temp2m0to48hRange => (
            SurfaceSnapshotField::Temp2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Rh2m0to24hMax => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Rh2m24to48hMax => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Max,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Rh2m0to48hMax => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Rh2m0to24hMin => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Rh2m24to48hMin => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Min,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Rh2m0to48hMin => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Rh2m0to24hRange => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Rh2m24to48hRange => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Range,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Rh2m0to48hRange => (
            SurfaceSnapshotField::Rh2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Dewpoint2m0to24hMax => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Dewpoint2m24to48hMax => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Max,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Dewpoint2m0to48hMax => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Dewpoint2m0to24hMin => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Dewpoint2m24to48hMin => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Min,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Dewpoint2m0to48hMin => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Dewpoint2m0to24hRange => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Dewpoint2m24to48hRange => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Range,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Dewpoint2m0to48hRange => (
            SurfaceSnapshotField::Dewpoint2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Vpd2m0to24hMax => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Vpd2m24to48hMax => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Max,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Vpd2m0to48hMax => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Max,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Vpd2m0to24hMin => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Vpd2m24to48hMin => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Min,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Vpd2m0to48hMin => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Min,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        Vpd2m0to24hRange => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            24,
            Some(24),
            "F001-F024",
        ),
        Vpd2m24to48hRange => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Range,
            25,
            48,
            Some(24),
            "F025-F048",
        ),
        Vpd2m0to48hRange => (
            SurfaceSnapshotField::Vpd2m,
            SurfaceSnapshotWindowOp::Range,
            1,
            48,
            Some(48),
            "F001-F048",
        ),
        _ => return None,
    };
    Some(SurfaceSnapshotWindowSpec {
        field,
        operation,
        hours: (start..=end).collect(),
        window_hours,
        window_label,
    })
}

pub(crate) fn collect_apcp_windows<'a>(
    apcp_by_hour: &'a BTreeMap<u16, Result<HrrrApcpDecode, String>>,
    hours: &[u16],
    window_hours: u16,
) -> Result<Vec<&'a [f64]>, String> {
    let mut out = Vec::with_capacity(hours.len());
    for &hour in hours {
        let decoded = apcp_by_hour
            .get(&hour)
            .ok_or_else(|| format!("missing APCP fetch for F{:03}", hour))?
            .as_ref()
            .map_err(Clone::clone)?;
        let window = select_window(&decoded.windows, window_hours).ok_or_else(|| {
            format!(
                "APCP F{:03} missing {}-hour accumulation field",
                hour, window_hours
            )
        })?;
        out.push(window);
    }
    Ok(out)
}

pub(crate) fn collect_uh_windows<'a>(
    uh_by_hour: &'a BTreeMap<u16, Result<HrrrUhDecode, String>>,
    hours: &[u16],
    window_hours: u16,
) -> Result<Vec<&'a [f64]>, String> {
    let mut out = Vec::with_capacity(hours.len());
    for &hour in hours {
        let decoded = uh_by_hour
            .get(&hour)
            .ok_or_else(|| format!("missing native UH fetch for F{:03}", hour))?
            .as_ref()
            .map_err(Clone::clone)?;
        let window = select_window(&decoded.windows, window_hours).ok_or_else(|| {
            format!(
                "native UH F{:03} missing {}-hour max field",
                hour, window_hours
            )
        })?;
        out.push(window);
    }
    Ok(out)
}

pub(crate) fn collect_wind10m_windows<'a>(
    wind_by_hour: &'a BTreeMap<u16, Result<HrrrWind10mMaxDecode, String>>,
    hours: &[u16],
    window_hours: u16,
) -> Result<Vec<&'a [f64]>, String> {
    let mut out = Vec::with_capacity(hours.len());
    for &hour in hours {
        let decoded = wind_by_hour
            .get(&hour)
            .ok_or_else(|| format!("missing native 10 m wind max fetch for F{:03}", hour))?
            .as_ref()
            .map_err(Clone::clone)?;
        let window = select_window(&decoded.windows, window_hours).ok_or_else(|| {
            format!(
                "native 10 m wind F{:03} missing {}-hour max field",
                hour, window_hours
            )
        })?;
        out.push(window);
    }
    Ok(out)
}

fn collect_surface_snapshot_values(
    snapshot_by_hour: &BTreeMap<u16, Result<HrrrSurfaceSnapshotDecode, String>>,
    hours: &[u16],
    field: SurfaceSnapshotField,
) -> Result<Vec<Vec<f64>>, String> {
    let mut out = Vec::with_capacity(hours.len());
    for &hour in hours {
        let decoded = snapshot_by_hour
            .get(&hour)
            .ok_or_else(|| format!("missing native surface snapshot fetch for F{:03}", hour))?
            .as_ref()
            .map_err(Clone::clone)?;
        out.push(surface_snapshot_values_for_hour(decoded, field, hour)?);
    }
    Ok(out)
}

fn surface_snapshot_values_for_hour(
    decoded: &HrrrSurfaceSnapshotDecode,
    field: SurfaceSnapshotField,
    hour: u16,
) -> Result<Vec<f64>, String> {
    match field {
        SurfaceSnapshotField::Temp2m => {
            let temp = decoded
                .temp2m_k
                .as_deref()
                .ok_or_else(|| format!("native F{hour:03} missing 2 m temperature field"))?;
            Ok(temp.iter().map(|value| *value - 273.15).collect())
        }
        SurfaceSnapshotField::Rh2m => {
            let rh = decoded
                .rh2m_pct
                .as_deref()
                .ok_or_else(|| format!("native F{hour:03} missing 2 m relative humidity field"))?;
            Ok(rh.iter().map(|value| (*value).clamp(0.0, 100.0)).collect())
        }
        SurfaceSnapshotField::Dewpoint2m => {
            let dewpoint = decoded
                .dewpoint2m_k
                .as_deref()
                .ok_or_else(|| format!("native F{hour:03} missing 2 m dewpoint field"))?;
            Ok(dewpoint.iter().map(|value| *value - 273.15).collect())
        }
        SurfaceSnapshotField::Vpd2m => vpd2m_values_for_hour(decoded, hour),
    }
}

fn vpd2m_values_for_hour(
    decoded: &HrrrSurfaceSnapshotDecode,
    hour: u16,
) -> Result<Vec<f64>, String> {
    let temp = decoded
        .temp2m_k
        .as_deref()
        .ok_or_else(|| format!("native F{hour:03} missing 2 m temperature field for VPD"))?;
    if let Some(rh) = decoded.rh2m_pct.as_deref() {
        if rh.len() != temp.len() {
            return Err(format!(
                "native F{hour:03} VPD length mismatch: temperature has {}, RH has {}",
                temp.len(),
                rh.len()
            ));
        }
        return Ok(temp
            .iter()
            .zip(rh.iter())
            .map(|(temp_k, rh_pct)| {
                let temp_c = *temp_k - 273.15;
                let es_hpa = saturation_vapor_pressure_hpa(temp_c);
                let rh_fraction = (*rh_pct / 100.0).clamp(0.0, 1.0);
                es_hpa * (1.0 - rh_fraction)
            })
            .collect());
    }

    let dewpoint = decoded
        .dewpoint2m_k
        .as_deref()
        .ok_or_else(|| format!("native F{hour:03} missing 2 m RH/dewpoint field for VPD"))?;
    if dewpoint.len() != temp.len() {
        return Err(format!(
            "native F{hour:03} VPD length mismatch: temperature has {}, dewpoint has {}",
            temp.len(),
            dewpoint.len()
        ));
    }
    Ok(temp
        .iter()
        .zip(dewpoint.iter())
        .map(|(temp_k, dewpoint_k)| {
            let temp_c = *temp_k - 273.15;
            let dewpoint_c = *dewpoint_k - 273.15;
            (saturation_vapor_pressure_hpa(temp_c) - saturation_vapor_pressure_hpa(dewpoint_c))
                .max(0.0)
        })
        .collect())
}

fn saturation_vapor_pressure_hpa(temp_c: f64) -> f64 {
    6.112 * ((17.67 * temp_c) / (temp_c + 243.5)).exp()
}

fn min_window_fields(grid: rustwx_core::GridShape, fields: &[&[f64]]) -> Result<Vec<f64>, String> {
    if fields.is_empty() {
        return Err("min window requires at least one input field".to_string());
    }
    let expected = grid.len();
    let mut out = vec![f64::INFINITY; expected];
    for values in fields {
        if values.len() != expected {
            return Err(format!(
                "window_field length mismatch: expected {expected}, got {}",
                values.len()
            ));
        }
        for (target, value) in out.iter_mut().zip(values.iter()) {
            *target = target.min(*value);
        }
    }
    Ok(out)
}

/// The color scale each windowed product renders with — the same per-family
/// scales the compute kernels above attach to their `ComputedWindowedField`,
/// factored out so grids computed elsewhere (the rw-store windowed lane)
/// render through identical styling. UH products return the same
/// `WeatherProduct::Uh` preset the kernel does; the render-request builder
/// routes them through `for_core_weather_product` either way.
pub(crate) fn windowed_product_scale(product: HrrrWindowedProduct) -> ColorScale {
    if product.is_qpf() {
        ColorScale::Discrete(qpf_scale())
    } else if product.is_wind10m() {
        ColorScale::Discrete(wind10m_scale())
    } else if let Some(spec) = surface_snapshot_window_spec(product) {
        ColorScale::Discrete(spec.field.scale(spec.operation))
    } else if let Some(scale) = temporal_product_scale(product) {
        scale
    } else if let Some(scale) = day_window_scale(product) {
        scale
    } else {
        ColorScale::Weather(WeatherProduct::Uh.scale_preset())
    }
}

/// Scale for the temporal-reduction family (hour counts, streak lengths and
/// onset/peak hours). Their values are HOURS, not the source variable, so they
/// need their own ramps — the field-matched day-window scales would be
/// meaningless (a "wind gust" ramp over an hour count).
///
/// Counts mask out zero so "never happened" reads as blank rather than as the
/// bottom color, which is the difference between an honest footprint and a map
/// that looks like wall-to-wall coverage.
fn temporal_product_scale(product: HrrrWindowedProduct) -> Option<ColorScale> {
    let slug = product.slug();
    let window_hours = if slug.ends_with("_0_48h") { 48.0 } else { 24.0 };
    // Timing fields: which hour it happened. A sequential ramp across the
    // window reads as a clock; NaN (never) renders blank.
    if slug.contains("_onset_hour") || slug.contains("_peak_hour") || slug.ends_with("_end_hour") {
        let (lo, hi) = if slug.ends_with("_24_48h") {
            (25.0, 48.0)
        } else {
            (1.0, window_hours)
        };
        return Some(ColorScale::Discrete(palette_scale(
            WeatherPalette::Temperature,
            range_step(lo, hi, 1.0),
            ExtendMode::Neither,
            None,
        )));
    }
    // Duration fields: hours, or longest consecutive run of hours.
    if slug.contains("_hours") || slug.ends_with("_longest_run") {
        return Some(ColorScale::Discrete(palette_scale(
            WeatherPalette::Cape,
            range_step(1.0, window_hours, 1.0),
            ExtendMode::Max,
            Some(0.5),
        )));
    }
    None
}

/// The source store variable behind a single-variable day-window product,
/// recovered from its slug (`wetbulb_2m_0_24h_max` -> `wetbulb_2m`).
///
/// Only reached for the day-window family: QPF, 10 m wind and the 2 m surface
/// snapshots are matched earlier in [`windowed_product_scale`], so their slugs
/// never fall through here even though they share the suffix shape.
pub(crate) fn day_window_source_var(product: HrrrWindowedProduct) -> Option<&'static str> {
    let slug = product.slug();
    [
        "_0_24h_max",
        "_24_48h_max",
        "_0_48h_max",
        "_0_24h_min",
        "_24_48h_min",
        "_0_48h_min",
    ]
    .iter()
    .find_map(|suffix| slug.strip_suffix(suffix))
}

/// Fill scale for a day-window product, matched to its FIELD rather than to
/// the windowed family — a peak wet-bulb map has to read like a temperature
/// map, not like updraft helicity. Without this every day-window product fell
/// through to the UH preset and rendered as a flat grey wash.
///
/// Where the field is a derived recipe the derived lane's own scale is reused
/// verbatim, so the window matches its hourly counterpart exactly.
fn day_window_scale(product: HrrrWindowedProduct) -> Option<ColorScale> {
    let var = day_window_source_var(product)?;
    // Prefer the derived lane's own production scale so a window looks exactly
    // like its hourly counterpart — EXCEPT for fields that also go through
    // `windowed_temp_display`. Derived scales are in DISPLAY units, so letting
    // temp_display convert those levels again double-converts them: wet-bulb's
    // °F ramp turned into an 80..240 colorbar over 35..87 °F data. Those
    // fields must use a STORED-units (°C) ramp and convert exactly once.
    if !day_window_scale_needs_stored_units(var) {
        if let Ok(lane) = crate::derived::derived_store_variable_style(var) {
            return Some(lane.scale);
        }
    }
    day_window_scale_explicit(var)
}

/// Fields whose day-window fill is re-scaled by [`crate::temp_display`], so
/// their color ramp must be expressed in the STORED unit (°C), not display °F.
fn day_window_scale_needs_stored_units(var: &str) -> bool {
    matches!(
        var,
        "wetbulb_2m"
            | "heat_index_2m"
            | "apparent_temperature_2m"
            | "wind_chill_2m"
            | "dewpoint_depression_2m"
    )
}

fn day_window_scale_explicit(var: &str) -> Option<ColorScale> {
    let discrete = |scale| Some(ColorScale::Discrete(scale));
    let preset = |p: WeatherProduct| Some(ColorScale::Weather(p.scale_preset()));
    match var {
        // Gusts and shear both read as wind speeds (kt); gusts run well past
        // the sustained-wind ceiling, so the ramp goes higher.
        "wind_gust_10m" => discrete(palette_scale(
            WeatherPalette::Winds,
            (10..=100).map(f64::from).collect(),
            ExtendMode::Both,
            None,
        )),
        "bulk_shear_0_1km" | "bulk_shear_0_6km" => discrete(palette_scale(
            WeatherPalette::Winds,
            (5..=80).map(f64::from).collect(),
            ExtendMode::Both,
            None,
        )),
        // Absolute °C temperature fields share the 2 m temperature ramp so the
        // °F display transform lands on the same colors as the hourly maps.
        "wetbulb_2m" | "heat_index_2m" | "apparent_temperature_2m" | "wind_chill_2m" => {
            discrete(temp2m_scale())
        }
        // A temperature DIFFERENCE, not an absolute — same ramp the 2 m range
        // windows use.
        "dewpoint_depression_2m" => discrete(temp2m_range_scale()),
        "hdw" => discrete(palette_scale(
            WeatherPalette::Cape,
            crate::plot_design::geometric_levels(20.0, 1.10, 1200.0),
            ExtendMode::Max,
            Some(20.0),
        )),
        // Visibility: the RH palette already runs bad(brown) -> good(green),
        // which is exactly how a worst-visibility map should read.
        "visibility" => discrete(palette_scale(
            WeatherPalette::Rh,
            range_step(0.25, 10.0, 0.25),
            ExtendMode::Max,
            None,
        )),
        "composite_reflectivity" | "reflectivity_1km" => {
            discrete(crate::plot_design::reflectivity_dbz_scale())
        }
        "cloud_cover_total" | "cloud_cover_low" | "cloud_cover_mid" | "cloud_cover_high" => {
            discrete(crate::plot_design::cloud_cover_scale())
        }
        "sbcape" => preset(WeatherProduct::Sbcape),
        "mlcape" => preset(WeatherProduct::Mlcape),
        "mucape" => preset(WeatherProduct::Mucape),
        // No dedicated DCAPE preset; downdraft CAPE shares the CAPE ramp.
        "dcape" => preset(WeatherProduct::Sbcape),
        "srh_0_1km" => preset(WeatherProduct::Srh01km),
        "srh_0_3km" => preset(WeatherProduct::Srh03km),
        "stp_fixed" => preset(WeatherProduct::StpFixed),
        "ehi_0_1km" | "ehi_0_3km" => preset(WeatherProduct::Ehi),
        "scp_mu_0_3km_0_6km_proxy" => preset(WeatherProduct::Scp),
        "sblcl" => preset(WeatherProduct::Lcl),
        "pwat" => discrete(palette_scale(
            WeatherPalette::Precip,
            range_step(2.0, 70.0, 2.0),
            ExtendMode::Both,
            None,
        )),
        "theta_e_2m_10m_winds" => discrete(palette_scale(
            WeatherPalette::Temperature,
            range_step(280.0, 350.0, 1.0),
            ExtendMode::Both,
            None,
        )),
        "lapse_rate_0_3km" | "lapse_rate_700_500" => discrete(palette_scale(
            WeatherPalette::LapseRate,
            range_step(3.0, 10.0, 0.25),
            ExtendMode::Both,
            None,
        )),
        // Pressure has no production fill scale; a diverging ramp about ~1013
        // hPa at least reads as low/high rather than as a grey wash.
        "mslp" => discrete(palette_scale(
            WeatherPalette::GeopotAnomaly,
            range_step(976.0, 1044.0, 2.0),
            ExtendMode::Both,
            None,
        )),
        // Smoke: the direct lane's geometric ramps + shared smoke palette.
        "smoke_8m" => discrete(DiscreteColorScale {
            levels: crate::plot_design::geometric_levels(10.0, 1.12, 560.0),
            colors: crate::plot_design::smoke_scale_colors(),
            extend: ExtendMode::Max,
            mask_below: Some(10.0),
        }),
        "smoke_column" => discrete(DiscreteColorScale {
            levels: crate::plot_design::geometric_levels(20.0, 1.12, 720.0),
            colors: crate::plot_design::smoke_scale_colors(),
            extend: ExtendMode::Max,
            mask_below: Some(20.0),
        }),
        // Occurrence maps: one filled class meaning "happened at some hour in
        // the window". mask_below hides the zeros so the map shows footprints.
        "categorical_rain" => discrete(binary_occurrence_scale(Color::rgba(38, 132, 80, 235))),
        "categorical_snow" => discrete(binary_occurrence_scale(Color::rgba(74, 130, 200, 235))),
        "categorical_freezing_rain" => {
            discrete(binary_occurrence_scale(Color::rgba(178, 74, 168, 235)))
        }
        "categorical_ice_pellets" => {
            discrete(binary_occurrence_scale(Color::rgba(206, 126, 62, 235)))
        }
        _ => None,
    }
}

/// A 0/1 occurrence field: everything below 0.5 is masked out, everything at
/// or above it draws in one flat color.
fn binary_occurrence_scale(color: Color) -> DiscreteColorScale {
    DiscreteColorScale {
        levels: vec![0.5, 1.0],
        colors: vec![color, color],
        extend: ExtendMode::Max,
        mask_below: Some(0.5),
    }
}

pub(crate) fn select_window(records: &[WindowedFieldRecord], hours: u16) -> Option<&[f64]> {
    records
        .iter()
        .find(|record| record.hours == hours)
        .map(|record| record.values.as_slice())
}

pub(crate) fn qpf_scale() -> rustwx_render::DiscreteColorScale {
    crate::qpf::qpf_inches_scale()
}

pub(crate) fn wind10m_scale() -> rustwx_render::DiscreteColorScale {
    palette_scale(
        WeatherPalette::Winds,
        (10..=70).map(|value| value as f64).collect(),
        ExtendMode::Both,
        None,
    )
}

pub(crate) fn temp2m_scale() -> DiscreteColorScale {
    let lo = -50.0;
    let hi = 50.5;
    let step = 0.5;
    DiscreteColorScale {
        levels: range_step(lo, hi, step),
        colors: temperature_palette_cropped_f(
            Some((-40.0, 120.0)),
            (((hi - lo) / step).round() as usize).max(2),
        ),
        extend: ExtendMode::Both,
        mask_below: None,
    }
}

pub(crate) fn temp2m_range_scale() -> DiscreteColorScale {
    let lo = 0.0;
    let hi = 40.5;
    let step = 0.5;
    DiscreteColorScale {
        levels: range_step(lo, hi, step),
        colors: temperature_palette_cropped_f(
            Some((32.0, 110.0)),
            (((hi - lo) / step).round() as usize).max(2),
        ),
        extend: ExtendMode::Max,
        mask_below: None,
    }
}

pub(crate) fn rh2m_scale(range: bool) -> DiscreteColorScale {
    palette_scale(
        WeatherPalette::Rh,
        range_step(0.0, 101.0, 1.0),
        if range {
            ExtendMode::Max
        } else {
            ExtendMode::Both
        },
        None,
    )
}

pub(crate) fn dewpoint2m_scale() -> DiscreteColorScale {
    let levels = range_step(-40.0, 31.0, 1.0);
    DiscreteColorScale {
        colors: dewpoint_palette_celsius_for_levels(&levels),
        levels,
        extend: ExtendMode::Both,
        mask_below: None,
    }
}

pub(crate) fn vpd2m_scale(range: bool) -> DiscreteColorScale {
    DiscreteColorScale {
        levels: if range {
            range_step(0.0, 31.0, 2.0)
        } else {
            range_step(0.0, 41.0, 2.0)
        },
        colors: vec![
            Color::rgba(24, 90, 145, 255),
            Color::rgba(39, 129, 172, 255),
            Color::rgba(67, 164, 184, 255),
            Color::rgba(110, 190, 168, 255),
            Color::rgba(154, 211, 142, 255),
            Color::rgba(196, 226, 126, 255),
            Color::rgba(229, 232, 126, 255),
            Color::rgba(247, 219, 118, 255),
            Color::rgba(248, 195, 102, 255),
            Color::rgba(240, 163, 85, 255),
            Color::rgba(226, 130, 72, 255),
            Color::rgba(207, 100, 65, 255),
            Color::rgba(184, 74, 61, 255),
            Color::rgba(157, 53, 60, 255),
            Color::rgba(128, 37, 63, 255),
        ],
        extend: ExtendMode::Max,
        mask_below: None,
    }
}

fn range_step(start: f64, end: f64, step: f64) -> Vec<f64> {
    let mut values = Vec::new();
    let mut value = start;
    while value <= end + step * 0.5 {
        values.push((value * 1000.0).round() / 1000.0);
        value += step;
    }
    values
}

#[cfg(test)]
mod tests;

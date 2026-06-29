//! EUMETSAT MTG FCI L1c NetCDF4 image decode.
//!
//! FCI body chunks are NetCDF4/HDF5 files with one `data/<channel>/measured`
//! group per spectral channel. This module decodes the packed effective
//! radiance, calibrates it to the requested value product, and adapts the
//! result into the generic satellite rw-store frame path.

use std::collections::BTreeSet;
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use hdf5_reader::{Datatype, Hdf5File};

use crate::abi::AbiFixedGrid;
use crate::geostationary::SweepAngleAxis;
use crate::store::{SatelliteGridField, SatelliteGridScene, SatelliteProjection};

const AU_KM: f64 = 149_597_870.7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FciChannel {
    pub name: &'static str,
    pub band: u8,
}

impl FciChannel {
    pub const ALL: &'static [FciChannel] = &[
        FciChannel {
            name: "vis_04",
            band: 1,
        },
        FciChannel {
            name: "vis_05",
            band: 2,
        },
        FciChannel {
            name: "vis_06",
            band: 3,
        },
        FciChannel {
            name: "vis_08",
            band: 4,
        },
        FciChannel {
            name: "vis_09",
            band: 5,
        },
        FciChannel {
            name: "nir_13",
            band: 6,
        },
        FciChannel {
            name: "nir_16",
            band: 7,
        },
        FciChannel {
            name: "nir_22",
            band: 8,
        },
        FciChannel {
            name: "ir_38",
            band: 9,
        },
        FciChannel {
            name: "wv_63",
            band: 10,
        },
        FciChannel {
            name: "wv_73",
            band: 11,
        },
        FciChannel {
            name: "ir_87",
            band: 12,
        },
        FciChannel {
            name: "ir_97",
            band: 13,
        },
        FciChannel {
            name: "ir_105",
            band: 14,
        },
        FciChannel {
            name: "ir_123",
            band: 15,
        },
        FciChannel {
            name: "ir_133",
            band: 16,
        },
    ];

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = normalize_channel_token(value);
        Self::ALL
            .iter()
            .copied()
            .find(|channel| normalize_channel_token(channel.name) == normalized)
            .or_else(|| {
                normalized
                    .strip_prefix('c')
                    .and_then(|raw| raw.parse::<u8>().ok())
                    .and_then(Self::from_band)
            })
    }

    pub fn from_band(band: u8) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|channel| channel.band == band)
    }

    pub fn is_thermal(self) -> bool {
        matches!(
            self.name,
            "ir_38" | "wv_63" | "wv_73" | "ir_87" | "ir_97" | "ir_105" | "ir_123" | "ir_133"
        )
    }

    pub fn choices() -> String {
        Self::ALL
            .iter()
            .map(|channel| channel.name)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FciValueMode {
    Count,
    Radiance,
    BrightnessTemperature,
    Reflectance,
}

impl FciValueMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], "")
            .as_str()
        {
            "count" | "counts" | "rawcount" | "rawcounts" => Some(Self::Count),
            "radiance" | "rad" => Some(Self::Radiance),
            "brightnesstemp" | "brightnesstemperature" | "bt" | "temp" | "temperature" => {
                Some(Self::BrightnessTemperature)
            }
            "reflectance" | "refl" | "albedo" => Some(Self::Reflectance),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Radiance => "radiance",
            Self::BrightnessTemperature => "bt",
            Self::Reflectance => "reflectance",
        }
    }

    pub fn variable_prefix(self) -> &'static str {
        match self {
            Self::Count => "fci_count",
            Self::Radiance => "fci_radiance",
            Self::BrightnessTemperature => "fci_bt",
            Self::Reflectance => "fci_reflectance",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Radiance => "mW m-2 sr-1 (cm-1)-1",
            Self::BrightnessTemperature => "K",
            Self::Reflectance => "%",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FciChunkSummary {
    pub path: PathBuf,
    pub channel: FciChannel,
    pub nx: usize,
    pub ny: usize,
    pub start_row: u32,
    pub end_row: u32,
    pub start_column: u32,
    pub end_column: u32,
    pub finite_count: usize,
}

#[derive(Debug, Clone)]
struct FciDecodedChunk {
    summary: FciChunkSummary,
    x_scan_rad: Vec<f64>,
    y_scan_rad: Vec<f64>,
    projection: SatelliteProjection,
    values: Vec<f32>,
    metadata: FciFileMetadata,
}

#[derive(Debug, Clone, PartialEq)]
struct FciFileMetadata {
    platform: String,
    product: String,
    processing_level: Option<String>,
    subtype: Option<String>,
    repeat_cycle_start_time_utc: Option<DateTime<Utc>>,
    start_time_utc: DateTime<Utc>,
    end_time_utc: DateTime<Utc>,
    full_grid_rows: Option<u32>,
}

pub fn inspect_fci_chunk(
    path: &Path,
    channel: FciChannel,
    mode: FciValueMode,
) -> Result<FciChunkSummary, Box<dyn Error>> {
    Ok(read_fci_chunk(path, channel, mode)?.summary)
}

pub fn assemble_fci_chunks(
    paths: &[PathBuf],
    channel: FciChannel,
    mode: FciValueMode,
    downsample: usize,
) -> Result<SatelliteGridField, Box<dyn Error>> {
    if paths.is_empty() {
        return Err(boxed_error("no MTG FCI NetCDF paths supplied"));
    }
    if mode == FciValueMode::BrightnessTemperature && !channel.is_thermal() {
        return Err(boxed_error(format!(
            "FCI brightness temperature requires an IR/WV channel, got {}",
            channel.name
        )));
    }

    let mut chunks = paths
        .iter()
        .map(|path| read_fci_chunk(path, channel, mode))
        .collect::<Result<Vec<_>, _>>()?;
    chunks.sort_by_key(|chunk| (chunk.summary.start_row, chunk.summary.start_column));

    let first = &chunks[0];
    let nx = first.summary.nx;
    let start_row = first.summary.start_row;
    let start_column = first.summary.start_column;
    let end_column = first.summary.end_column;
    let mut expected_row = start_row;
    let mut y_scan_rad = Vec::new();
    let mut values = Vec::new();
    let mut source_paths = Vec::new();
    let mut row_ranges = Vec::new();
    let mut seen = BTreeSet::new();

    for chunk in &chunks {
        if !seen.insert(chunk.summary.path.clone()) {
            return Err(boxed_error(format!(
                "duplicate FCI path {}",
                chunk.summary.path.display()
            )));
        }
        validate_compatible_chunk(first, chunk)?;
        if chunk.summary.start_column != start_column || chunk.summary.end_column != end_column {
            return Err(boxed_error(format!(
                "FCI chunk {} has column range {}..{}, expected {}..{}",
                chunk.summary.path.display(),
                chunk.summary.start_column,
                chunk.summary.end_column,
                start_column,
                end_column
            )));
        }
        if chunk.summary.start_row != expected_row {
            return Err(boxed_error(format!(
                "FCI chunks are not contiguous: expected row {}, got {} in {}",
                expected_row,
                chunk.summary.start_row,
                chunk.summary.path.display()
            )));
        }
        expected_row = chunk.summary.end_row.saturating_add(1);
        y_scan_rad.extend_from_slice(&chunk.y_scan_rad);
        values.extend_from_slice(&chunk.values);
        source_paths.push(chunk.summary.path.display().to_string());
        row_ranges.push(serde_json::json!({
            "start_row": chunk.summary.start_row,
            "end_row": chunk.summary.end_row,
            "path": chunk.summary.path,
            "finite_count": chunk.summary.finite_count,
        }));
    }

    let ny = y_scan_rad.len();
    if values.len() != nx.saturating_mul(ny) {
        return Err(boxed_error(format!(
            "assembled FCI field length {} does not match {nx}x{ny}",
            values.len()
        )));
    }

    let last_row = chunks
        .last()
        .map(|chunk| chunk.summary.end_row)
        .unwrap_or(start_row);
    let full_rows = first.metadata.full_grid_rows;
    let sector = fci_sector_slug(start_row, last_row, full_rows);
    let metadata = &first.metadata;
    let field = SatelliteGridField {
        scene: SatelliteGridScene {
            model: platform_model_slug(&metadata.platform),
            satellite: platform_display_name(&metadata.platform),
            provider: "eumetsat".to_string(),
            instrument: "fci".to_string(),
            product: metadata.product.clone(),
            sector,
            band: channel.band,
            layer: format!("{}_{}", mode.slug(), channel.name),
            source_variable: format!("data/{}/measured/effective_radiance", channel.name),
            start_time_utc: metadata.start_time_utc,
            end_time_utc: chunks
                .last()
                .map(|chunk| chunk.metadata.end_time_utc)
                .unwrap_or(metadata.end_time_utc),
            projection: first.projection.clone(),
            fixed_grid: AbiFixedGrid {
                nx,
                ny,
                x_scan_rad: first.x_scan_rad.clone(),
                y_scan_rad,
            },
            metadata: serde_json::json!({
                "source_format": "mtg_fci_l1c_netcdf4",
                "value_mode": mode.slug(),
                "channel": channel.name,
                "platform": metadata.platform,
                "processing_level": metadata.processing_level,
                "subtype": metadata.subtype,
                "repeat_cycle_start_time_utc": metadata
                    .repeat_cycle_start_time_utc
                    .map(|time| time.to_rfc3339()),
                "full_grid_rows": full_rows,
                "row_ranges": row_ranges,
                "source_paths": source_paths,
            }),
        },
        variable_name: format!("{}_c{:02}", mode.variable_prefix(), channel.band),
        units: mode.units().to_string(),
        values,
    };

    Ok(downsample_satellite_field(field, downsample))
}

fn read_fci_chunk(
    path: &Path,
    channel: FciChannel,
    mode: FciValueMode,
) -> Result<FciDecodedChunk, Box<dyn Error>> {
    let file = Hdf5File::open(path)?;
    let measured_path = format!("data/{}/measured", channel.name);
    let radiance_dataset = file.dataset(&format!("{measured_path}/effective_radiance"))?;
    if radiance_dataset.ndim() != 2 {
        return Err(boxed_error(format!(
            "FCI effective_radiance must be 2D, got {:?}",
            radiance_dataset.shape()
        )));
    }
    let shape = radiance_dataset
        .shape()
        .iter()
        .map(|value| usize::try_from(*value))
        .collect::<Result<Vec<_>, _>>()?;
    let [ny, nx] = shape.as_slice() else {
        return Err(boxed_error("FCI effective_radiance shape is not rank 2"));
    };

    let start_row = read_scalar_u32(&file, &format!("{measured_path}/start_position_row"))?;
    let end_row = read_scalar_u32(&file, &format!("{measured_path}/end_position_row"))?;
    let start_column = read_scalar_u32(&file, &format!("{measured_path}/start_position_column"))?;
    let end_column = read_scalar_u32(&file, &format!("{measured_path}/end_position_column"))?;
    let x_scan_rad = read_scaled_axis(&file, &format!("{measured_path}/x"))?;
    let y_scan_rad = read_scaled_axis(&file, &format!("{measured_path}/y"))?;
    if x_scan_rad.len() != *nx || y_scan_rad.len() != *ny {
        return Err(boxed_error(format!(
            "FCI axis lengths do not match radiance grid: x={} y={} grid={}x{}",
            x_scan_rad.len(),
            y_scan_rad.len(),
            nx,
            ny
        )));
    }

    let values = read_fci_values(&file, &radiance_dataset, &measured_path, channel, mode)?;
    let finite_count = values.iter().filter(|value| value.is_finite()).count();
    let projection = read_projection(&file)?;
    let metadata = read_metadata(&file, path, &y_scan_rad)?;
    Ok(FciDecodedChunk {
        summary: FciChunkSummary {
            path: path.to_path_buf(),
            channel,
            nx: *nx,
            ny: *ny,
            start_row,
            end_row,
            start_column,
            end_column,
            finite_count,
        },
        x_scan_rad,
        y_scan_rad,
        projection,
        values,
        metadata,
    })
}

fn read_fci_values(
    file: &Hdf5File,
    radiance_dataset: &hdf5_reader::Dataset,
    measured_path: &str,
    channel: FciChannel,
    mode: FciValueMode,
) -> Result<Vec<f32>, Box<dyn Error>> {
    let raw = hdf5_dataset_values_f64(radiance_dataset)?;
    let scale = hdf5_attr_f64(radiance_dataset, "scale_factor").unwrap_or(1.0);
    let offset = hdf5_attr_f64(radiance_dataset, "add_offset").unwrap_or(0.0);
    let fill = hdf5_attr_f64(radiance_dataset, "_FillValue");
    let valid_range = hdf5_attr_f64_vec(radiance_dataset, "valid_range").and_then(|values| {
        match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        }
    });
    let valid_cold_range =
        hdf5_attr_f64_vec(radiance_dataset, "valid_cold_range").and_then(|values| {
            match values.as_slice() {
                [min, max, ..] => Some((*min, *max)),
                _ => None,
            }
        });
    let warm_scale = hdf5_attr_f64(radiance_dataset, "warm_scale_factor");
    let warm_offset = hdf5_attr_f64(radiance_dataset, "warm_add_offset");

    let radiance = raw
        .into_iter()
        .map(|value| {
            if !value.is_finite()
                || fill.is_some_and(|fill| (value - fill).abs() < 0.5)
                || valid_range.is_some_and(|(min, max)| value < min || value > max)
            {
                f32::NAN
            } else if channel.name == "ir_38"
                && valid_cold_range.is_some_and(|(_, max)| value > max)
                && warm_scale.is_some()
                && warm_offset.is_some()
            {
                (value * warm_scale.unwrap() + warm_offset.unwrap()) as f32
            } else {
                (value * scale + offset) as f32
            }
        })
        .collect::<Vec<_>>();

    match mode {
        FciValueMode::Count => Ok(radiance
            .into_iter()
            .map(|value| {
                if value.is_finite() {
                    ((f64::from(value) - offset) / scale) as f32
                } else {
                    f32::NAN
                }
            })
            .collect()),
        FciValueMode::Radiance => Ok(radiance),
        FciValueMode::BrightnessTemperature => {
            let coefficients = read_bt_coefficients(file, measured_path)?;
            Ok(radiance
                .into_iter()
                .map(|value| fci_brightness_temperature(f64::from(value), coefficients) as f32)
                .collect())
        }
        FciValueMode::Reflectance => {
            let irradiance = read_scalar_f64(
                file,
                &format!("{measured_path}/channel_effective_solar_irradiance"),
            )?;
            if !(irradiance.is_finite() && irradiance > 0.0 && irradiance < 1.0e30) {
                return Err(boxed_error(format!(
                    "FCI reflectance requires a finite solar irradiance for {measured_path}"
                )));
            }
            let sun_earth_distance_au = read_sun_earth_distance_au(file).unwrap_or(1.0);
            Ok(radiance
                .into_iter()
                .map(|value| {
                    if value.is_finite() {
                        (100.0
                            * f64::from(value)
                            * std::f64::consts::PI
                            * sun_earth_distance_au.powi(2)
                            / irradiance) as f32
                    } else {
                        f32::NAN
                    }
                })
                .collect())
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct BtCoefficients {
    c1: f64,
    c2: f64,
    a: f64,
    b: f64,
    wavenumber: f64,
}

fn read_bt_coefficients(
    file: &Hdf5File,
    measured_path: &str,
) -> Result<BtCoefficients, Box<dyn Error>> {
    let coefficients = BtCoefficients {
        c1: read_scalar_f64(
            file,
            &format!("{measured_path}/radiance_to_bt_conversion_constant_c1"),
        )?,
        c2: read_scalar_f64(
            file,
            &format!("{measured_path}/radiance_to_bt_conversion_constant_c2"),
        )?,
        a: read_scalar_f64(
            file,
            &format!("{measured_path}/radiance_to_bt_conversion_coefficient_a"),
        )?,
        b: read_scalar_f64(
            file,
            &format!("{measured_path}/radiance_to_bt_conversion_coefficient_b"),
        )?,
        wavenumber: read_scalar_f64(
            file,
            &format!("{measured_path}/radiance_to_bt_conversion_coefficient_wavenumber"),
        )?,
    };
    if [
        coefficients.c1,
        coefficients.c2,
        coefficients.a,
        coefficients.b,
        coefficients.wavenumber,
    ]
    .iter()
    .any(|value| !value.is_finite() || *value >= 1.0e30)
        || coefficients.a == 0.0
        || coefficients.wavenumber <= 0.0
    {
        return Err(boxed_error(format!(
            "FCI brightness temperature coefficients are not valid for {measured_path}"
        )));
    }
    Ok(coefficients)
}

fn fci_brightness_temperature(radiance: f64, coefficients: BtCoefficients) -> f64 {
    if !(radiance.is_finite() && radiance > 0.0) {
        return f64::NAN;
    }
    let numerator = coefficients.c1 * coefficients.wavenumber.powi(3);
    let log_arg = 1.0 + numerator / radiance;
    if !(log_arg.is_finite() && log_arg > 1.0) {
        return f64::NAN;
    }
    let denominator = coefficients.a * log_arg.ln();
    if !(denominator.is_finite() && denominator != 0.0) {
        return f64::NAN;
    }
    let kelvin =
        coefficients.c2 * coefficients.wavenumber / denominator - coefficients.b / coefficients.a;
    if kelvin.is_finite() { kelvin } else { f64::NAN }
}

fn read_scaled_axis(file: &Hdf5File, path: &str) -> Result<Vec<f64>, Box<dyn Error>> {
    let dataset = file.dataset(path)?;
    let scale = hdf5_attr_f64(&dataset, "scale_factor").unwrap_or(1.0);
    let offset = hdf5_attr_f64(&dataset, "add_offset").unwrap_or(0.0);
    let fill = hdf5_attr_f64(&dataset, "_FillValue");
    let valid_range =
        hdf5_attr_f64_vec(&dataset, "valid_range").and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    Ok(hdf5_dataset_values_f64(&dataset)?
        .into_iter()
        .map(|value| {
            if !value.is_finite()
                || fill.is_some_and(|fill| (value - fill).abs() < 0.5)
                || valid_range.is_some_and(|(min, max)| value < min || value > max)
            {
                f64::NAN
            } else {
                value * scale + offset
            }
        })
        .collect())
}

fn read_projection(file: &Hdf5File) -> Result<SatelliteProjection, Box<dyn Error>> {
    let projection = file.dataset("data/mtg_geos_projection")?;
    let semi_major_axis_m = hdf5_attr_f64(&projection, "semi_major_axis")
        .or_else(|| read_scalar_f64(file, "state/processor/earth_equatorial_radius").ok())
        .ok_or_else(|| boxed_error("FCI projection missing semi_major_axis"))?;
    let semi_minor_axis_m = hdf5_attr_f64(&projection, "semi_minor_axis")
        .or_else(|| read_scalar_f64(file, "state/processor/earth_polar_radius").ok())
        .ok_or_else(|| boxed_error("FCI projection missing semi_minor_axis"))?;
    let perspective_point_height_m = hdf5_attr_f64(&projection, "perspective_point_height")
        .or_else(|| read_scalar_f64(file, "state/processor/reference_altitude").ok())
        .ok_or_else(|| boxed_error("FCI projection missing perspective_point_height"))?;
    let longitude_of_projection_origin_deg =
        hdf5_attr_f64(&projection, "longitude_of_projection_origin")
            .or_else(|| read_scalar_f64(file, "state/processor/projection_origin_longitude").ok())
            .ok_or_else(|| boxed_error("FCI projection missing longitude_of_projection_origin"))?;
    let sweep_angle_axis = hdf5_attr_string(&projection, "sweep_angle_axis")
        .map(|value| SweepAngleAxis::parse(&value))
        .unwrap_or(SweepAngleAxis::Y);

    Ok(SatelliteProjection {
        perspective_point_height_m,
        semi_major_axis_m,
        semi_minor_axis_m,
        longitude_of_projection_origin_deg,
        sweep_angle_axis,
    })
}

fn read_metadata(
    file: &Hdf5File,
    path: &Path,
    y_scan_rad: &[f64],
) -> Result<FciFileMetadata, Box<dyn Error>> {
    let (start_time_utc, end_time_utc) = parse_fci_times_from_filename(path)?;
    let platform = root_attr_string(file, "platform").unwrap_or_else(|| "MTI1".to_string());
    let processing_level = root_attr_string(file, "processing_level");
    let subtype = root_attr_string(file, "subtype");
    let product = match (
        root_attr_string(file, "data_source"),
        processing_level.as_deref(),
        root_attr_string(file, "type"),
        subtype.as_deref(),
    ) {
        (Some(source), Some(level), Some(kind), Some(subtype)) => {
            format!("{source}-L{level}-{kind}-{subtype}")
        }
        _ => root_attr_string(file, "product_id").unwrap_or_else(|| "FCI-L1C".to_string()),
    };
    let repeat_cycle_start_time_utc =
        read_cf2000_seconds(file, "state/instrument/repeat_cycle_start_time").ok();
    let full_grid_rows = hdf5_axis_full_rows(file, y_scan_rad);

    Ok(FciFileMetadata {
        platform,
        product,
        processing_level,
        subtype,
        repeat_cycle_start_time_utc,
        start_time_utc,
        end_time_utc,
        full_grid_rows,
    })
}

fn hdf5_axis_full_rows(file: &Hdf5File, y_scan_rad: &[f64]) -> Option<u32> {
    let dataset = file.dataset("data/ir_105/measured/y").ok()?;
    hdf5_attr_f64_vec(&dataset, "valid_range")
        .and_then(|values| values.get(1).copied())
        .and_then(|value| u32::try_from(value as i64).ok())
        .filter(|value| {
            usize::try_from(*value)
                .ok()
                .is_some_and(|rows| rows >= y_scan_rad.len())
        })
}

fn read_cf2000_seconds(file: &Hdf5File, path: &str) -> Result<DateTime<Utc>, Box<dyn Error>> {
    let seconds = read_scalar_f64(file, path)?;
    if !(seconds.is_finite() && seconds < 1.0e30) {
        return Err(boxed_error(format!("{path} is not finite")));
    }
    let epoch = Utc
        .with_ymd_and_hms(2000, 1, 1, 0, 0, 0)
        .single()
        .ok_or_else(|| boxed_error("invalid CF-2000 epoch"))?;
    Ok(epoch + chrono::Duration::milliseconds((seconds * 1000.0).round() as i64))
}

fn parse_fci_times_from_filename(
    path: &Path,
) -> Result<(DateTime<Utc>, DateTime<Utc>), Box<dyn Error>> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .ok_or_else(|| boxed_error(format!("FCI path has no file name: {}", path.display())))?;
    let mut tokens = Vec::new();
    let bytes = name.as_bytes();
    for index in 0..bytes.len().saturating_sub(13) {
        let candidate = &name[index..index + 14];
        if candidate.bytes().all(|byte| byte.is_ascii_digit()) {
            let left_ok = index == 0 || !bytes[index - 1].is_ascii_digit();
            let right_ok = index + 14 == bytes.len() || !bytes[index + 14].is_ascii_digit();
            if left_ok && right_ok {
                tokens.push(candidate.to_string());
            }
        }
    }
    if tokens.len() < 2 {
        return Err(boxed_error(format!(
            "could not parse FCI observation start/end timestamps from {}",
            path.display()
        )));
    }
    let start = parse_fci_timestamp(&tokens[tokens.len() - 2])?;
    let end = parse_fci_timestamp(&tokens[tokens.len() - 1])?;
    Ok((start, end))
}

fn parse_fci_timestamp(value: &str) -> Result<DateTime<Utc>, Box<dyn Error>> {
    let naive = NaiveDateTime::parse_from_str(value, "%Y%m%d%H%M%S")?;
    Ok(Utc.from_utc_datetime(&naive))
}

fn read_sun_earth_distance_au(file: &Hdf5File) -> Option<f64> {
    let dataset = file.dataset("state/celestial/earth_sun_distance").ok()?;
    let fill = hdf5_attr_f64(&dataset, "_FillValue");
    let values = hdf5_dataset_values_f64(&dataset).ok()?;
    let mut count = 0usize;
    let mut sum = 0.0;
    for value in values {
        if value.is_finite()
            && value > 0.0
            && value < 1.0e20
            && !fill.is_some_and(|fill| (value - fill).abs() < 0.5)
        {
            count += 1;
            sum += value;
        }
    }
    (count > 0).then(|| (sum / count as f64) / AU_KM)
}

fn validate_compatible_chunk(
    first: &FciDecodedChunk,
    candidate: &FciDecodedChunk,
) -> Result<(), Box<dyn Error>> {
    if first.summary.channel != candidate.summary.channel
        || first.summary.nx != candidate.summary.nx
        || first.x_scan_rad.len() != candidate.x_scan_rad.len()
        || !coords_bit_identical_f64(&first.x_scan_rad, &candidate.x_scan_rad)
        || first.projection != candidate.projection
        || first.metadata.platform != candidate.metadata.platform
        || first.metadata.product != candidate.metadata.product
    {
        return Err(boxed_error(format!(
            "incompatible FCI chunk {}",
            candidate.summary.path.display()
        )));
    }
    Ok(())
}

fn downsample_satellite_field(mut field: SatelliteGridField, step: usize) -> SatelliteGridField {
    if step <= 1 {
        return field;
    }
    let nx = field.scene.fixed_grid.nx;
    let ny = field.scene.fixed_grid.ny;
    let xs: Vec<usize> = (0..nx).step_by(step).collect();
    let ys: Vec<usize> = (0..ny).step_by(step).collect();
    let mut values = Vec::with_capacity(xs.len() * ys.len());
    for &y in &ys {
        for &x in &xs {
            values.push(field.values[y * nx + x]);
        }
    }
    field.scene.fixed_grid = AbiFixedGrid {
        nx: xs.len(),
        ny: ys.len(),
        x_scan_rad: xs
            .iter()
            .map(|&x| field.scene.fixed_grid.x_scan_rad[x])
            .collect(),
        y_scan_rad: ys
            .iter()
            .map(|&y| field.scene.fixed_grid.y_scan_rad[y])
            .collect(),
    };
    field.values = values;
    field.scene.metadata["downsample"] = serde_json::json!(step);
    field
}

fn fci_sector_slug(start_row: u32, end_row: u32, full_rows: Option<u32>) -> String {
    match full_rows {
        Some(rows) if start_row == 1 && end_row == rows => "fulldisk".to_string(),
        Some(rows) => format!("fulldisk_r{start_row:04}_{end_row:04}of{rows:04}"),
        None => format!("fulldisk_r{start_row:04}_{end_row:04}"),
    }
}

fn platform_model_slug(platform: &str) -> String {
    let normalized = platform.trim().to_ascii_lowercase();
    if let Some(number) = normalized.strip_prefix("mti") {
        format!("mtg-i{number}")
    } else {
        normalized
    }
}

fn platform_display_name(platform: &str) -> String {
    match platform.trim().to_ascii_uppercase().as_str() {
        "MTI1" => "Meteosat-12 / MTI1".to_string(),
        value if !value.is_empty() => value.to_string(),
        _ => "MTI1".to_string(),
    }
}

fn normalize_channel_token(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

fn root_attr_string(file: &Hdf5File, name: &str) -> Option<String> {
    let root = file.root_group().ok()?;
    root.attribute(name)
        .ok()
        .and_then(|attr| attr.read_string().ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty() && value != "null")
}

fn hdf5_attr_f64(dataset: &hdf5_reader::Dataset, name: &str) -> Option<f64> {
    dataset
        .attribute(name)
        .ok()
        .and_then(|attr| attr.read_as_f64().ok())
}

fn hdf5_attr_f64_vec(dataset: &hdf5_reader::Dataset, name: &str) -> Option<Vec<f64>> {
    let attr = dataset.attribute(name).ok()?;
    match &attr.datatype {
        Datatype::FloatingPoint { size: 4, .. } => Some(
            attr.read_1d::<f32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FloatingPoint { size: 8, .. } => attr.read_1d::<f64>().ok(),
        Datatype::FixedPoint {
            size: 1,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i8>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u8>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i16>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u16>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 4,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 4,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 8,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i64>()
                .ok()?
                .into_iter()
                .map(|value| value as f64)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 8,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u64>()
                .ok()?
                .into_iter()
                .map(|value| value as f64)
                .collect(),
        ),
        _ => None,
    }
}

fn hdf5_attr_string(dataset: &hdf5_reader::Dataset, name: &str) -> Option<String> {
    dataset
        .attribute(name)
        .ok()
        .and_then(|attr| attr.read_string().ok())
        .map(|value| {
            value
                .trim()
                .trim_matches(['"', '\u{201c}', '\u{201d}'])
                .to_string()
        })
        .filter(|value| !value.is_empty())
}

fn read_scalar_f64(file: &Hdf5File, path: &str) -> Result<f64, Box<dyn Error>> {
    hdf5_dataset_values_f64(&file.dataset(path)?)?
        .into_iter()
        .next()
        .ok_or_else(|| boxed_error(format!("scalar dataset {path} is empty")))
}

fn read_scalar_u32(file: &Hdf5File, path: &str) -> Result<u32, Box<dyn Error>> {
    let value = read_scalar_f64(file, path)?;
    if !(value.is_finite() && value >= 0.0 && value <= f64::from(u32::MAX)) {
        return Err(boxed_error(format!("{path} is not a valid u32: {value}")));
    }
    Ok(value.round() as u32)
}

fn hdf5_dataset_values_f64(dataset: &hdf5_reader::Dataset) -> Result<Vec<f64>, Box<dyn Error>> {
    match dataset.dtype() {
        Datatype::FloatingPoint { size: 4, .. } => Ok(dataset
            .read_array::<f32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FloatingPoint { size: 8, .. } => {
            Ok(dataset.read_array::<f64>()?.iter().copied().collect())
        }
        Datatype::FixedPoint {
            size: 1,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i8>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u8>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i16>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u16>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i64>()?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u64>()?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        dtype => Err(boxed_error(format!(
            "unsupported HDF5 numeric dataset type for {}: {dtype:?}",
            dataset.name()
        ))),
    }
}

fn coords_bit_identical_f64(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fci_channels() {
        assert_eq!(FciChannel::parse("ir_105").unwrap().band, 14);
        assert_eq!(FciChannel::parse("IR105").unwrap().name, "ir_105");
        assert_eq!(FciChannel::parse("c14").unwrap().name, "ir_105");
        assert!(FciChannel::parse("c99").is_none());
    }

    #[test]
    fn parses_fci_filename_observation_window() {
        let path = Path::new(
            "W_XX-EUMETSAT-Darmstadt,IMG+SAT,MTI1+FCI-1C-RRAD-FDHSI-FD--CHK-BODY---NC4E_C_EUMT_20260615031254_IDPFI_OPE_20260615031003_20260615031935_N__O_0020_0000.nc",
        );
        let (start, end) = parse_fci_times_from_filename(path).unwrap();
        assert_eq!(start.to_rfc3339(), "2026-06-15T03:10:03+00:00");
        assert_eq!(end.to_rfc3339(), "2026-06-15T03:19:35+00:00");
    }

    #[test]
    fn converts_fci_bt_coefficients() {
        let coefficients = BtCoefficients {
            c1: 1.191_042_79e-5,
            c2: 1.438_775_18,
            a: 0.999,
            b: 0.3644,
            wavenumber: 949.973,
        };
        let bt = fci_brightness_temperature(50.0, coefficients);
        assert!(bt > 240.0 && bt < 270.0, "{bt}");
    }
}

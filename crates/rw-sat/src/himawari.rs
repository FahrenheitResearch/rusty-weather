//! JMA Himawari-8/9 AHI discovery over NOAA's public S3 buckets.
//!
//! The live open-data path is different from GOES ABI: AHI full-disk scans are
//! segmented `DAT.bz2` files rather than single NetCDF granules. This module
//! intentionally covers live inventory/URL construction first; native segment
//! decode into `rw-store` is a separate ingest step.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use bzip2::read::MultiBzDecoder;
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::abi::AbiFixedGrid;
use crate::geostationary::SweepAngleAxis;
use crate::s3::{S3Object, list_s3_objects, object_filename};
use crate::store::{SatelliteGridField, SatelliteGridScene, SatelliteProjection};

pub const HIMAWARI8_BUCKET: &str = "noaa-himawari8";
pub const HIMAWARI9_BUCKET: &str = "noaa-himawari9";
pub const HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA: &str = "rusty-weather.himawari-segment-manifest.v1";
pub const HIMAWARI_STAGE_MANIFEST_SCHEMA: &str = "rusty-weather.himawari-stage-manifest.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HimawariSatellite {
    H8,
    H9,
}

impl HimawariSatellite {
    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace(['-', '_'], "");
        match normalized.as_str() {
            "h8" | "himawari8" | "noaahimawari8" => Some(Self::H8),
            "h9" | "himawari9" | "noaahimawari9" => Some(Self::H9),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::H8 => "h8",
            Self::H9 => "h9",
        }
    }

    pub fn platform(self) -> &'static str {
        match self {
            Self::H8 => "H08",
            Self::H9 => "H09",
        }
    }

    pub fn bucket(self) -> &'static str {
        match self {
            Self::H8 => HIMAWARI8_BUCKET,
            Self::H9 => HIMAWARI9_BUCKET,
        }
    }
}

impl fmt::Display for HimawariSatellite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.slug(), self.bucket())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HimawariProduct {
    AhiL1bFldk,
}

impl HimawariProduct {
    pub fn slug(self) -> &'static str {
        match self {
            Self::AhiL1bFldk => "ahi-l1b-fldk",
        }
    }

    pub fn s3_prefix(self) -> &'static str {
        match self {
            Self::AhiL1bFldk => "AHI-L1b-FLDK",
        }
    }

    pub fn cadence_minutes(self) -> i64 {
        match self {
            Self::AhiL1bFldk => 10,
        }
    }

    pub fn scan_prefix(self, scan_time: DateTime<Utc>) -> String {
        format!(
            "{}/{:04}/{:02}/{:02}/{:02}{:02}/",
            self.s3_prefix(),
            scan_time.year(),
            scan_time.month(),
            scan_time.day(),
            scan_time.hour(),
            scan_time.minute()
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HimawariSegmentName {
    pub satellite: HimawariSatellite,
    pub scan_time: DateTime<Utc>,
    pub band: u8,
    pub product: String,
    pub resolution: String,
    pub segment_index: u8,
    pub segment_count: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HimawariSegment {
    pub object: S3Object,
    pub name: HimawariSegmentName,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HimawariLatestRequest {
    pub satellite: HimawariSatellite,
    pub product: HimawariProduct,
    pub band: Option<u8>,
    pub lookback_minutes: i64,
    pub require_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HimawariLatestResult {
    pub satellite: HimawariSatellite,
    pub product: HimawariProduct,
    pub scan_time: DateTime<Utc>,
    pub prefix: String,
    pub segments: Vec<HimawariSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariDownloadManifest {
    pub schema: String,
    pub satellite: String,
    pub platform: String,
    pub bucket: String,
    pub product: String,
    pub scan_time_utc: String,
    pub prefix: String,
    pub band: u8,
    pub segments_downloaded: usize,
    pub segments_available: usize,
    pub source_complete: bool,
    pub allow_partial: bool,
    pub total_downloaded_bytes: u64,
    pub cache_root: String,
    pub segments: Vec<HimawariManifestSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariManifestSegment {
    pub band: u8,
    pub segment_index: u8,
    pub segment_count: u8,
    pub product: String,
    pub resolution: String,
    pub key: String,
    pub url: String,
    pub last_modified: String,
    pub size_bytes: u64,
    pub cache_path: String,
    pub cache_hit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariStageManifest {
    pub schema: String,
    pub source_manifest: String,
    pub satellite: String,
    pub platform: String,
    pub product: String,
    pub scan_time_utc: String,
    pub band: u8,
    pub source_complete: bool,
    pub segments_staged: usize,
    pub total_compressed_bytes: u64,
    pub total_raw_bytes: u64,
    pub out_dir: String,
    pub segments: Vec<HimawariStagedSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariStagedSegment {
    pub band: u8,
    pub segment_index: u8,
    pub segment_count: u8,
    pub key: String,
    pub compressed_path: String,
    pub compressed_bytes: u64,
    pub raw_path: String,
    pub raw_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HimawariByteOrder {
    Little,
    Big,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HimawariHsdHeader {
    pub path: Option<String>,
    pub byte_order: HimawariByteOrder,
    pub total_header_blocks: u16,
    pub block_lengths: Vec<HimawariBlockLength>,
    pub satellite_name: String,
    pub processing_center: String,
    pub observation_area: String,
    pub observation_timeline: u16,
    pub observation_start_mjd: f64,
    pub observation_end_mjd: f64,
    pub file_creation_mjd: f64,
    pub total_header_length: u32,
    pub total_data_length: u32,
    pub quality_flags: [u8; 4],
    pub file_format_version: String,
    pub file_name: String,
    pub data: HimawariDataInfo,
    pub projection: Option<HimawariProjectionInfo>,
    pub calibration: Option<HimawariCalibrationInfo>,
    pub segment: Option<HimawariSegmentInfo>,
    pub file_length: u64,
    pub expected_file_length: u64,
    pub length_matches_header: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariBlockLength {
    pub block_number: u8,
    pub offset: u32,
    pub length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariDataInfo {
    pub bits_per_pixel: u16,
    pub columns: u16,
    pub lines: u16,
    pub compression_flag: u8,
    pub compression: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HimawariProjectionInfo {
    pub sub_lon_degrees: f64,
    pub cfac: u32,
    pub lfac: u32,
    pub coff: f32,
    pub loff: f32,
    pub satellite_distance_km: f64,
    pub equatorial_radius_km: f64,
    pub polar_radius_km: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HimawariCalibrationInfo {
    pub band_number: u16,
    pub central_wavelength_um: f64,
    pub valid_bits_per_pixel: u16,
    pub error_pixel_count: u16,
    pub outside_scan_count: u16,
    pub count_to_radiance_slope: f64,
    pub count_to_radiance_intercept: f64,
    pub planck_or_albedo_coefficients: [f64; 3],
    pub inverse_planck_coefficients: Option<[f64; 3]>,
    pub physical_constants: Option<HimawariPhysicalConstants>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HimawariPhysicalConstants {
    pub speed_of_light_m_s: f64,
    pub planck_constant_j_s: f64,
    pub boltzmann_constant_j_k: f64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HimawariSegmentInfo {
    pub total_segments: u8,
    pub sequence_number: u8,
    pub first_line_number: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HimawariValueMode {
    Count,
    Radiance,
    BrightnessTemperature,
}

impl HimawariValueMode {
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
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Radiance => "radiance",
            Self::BrightnessTemperature => "bt",
        }
    }

    pub fn variable_prefix(self) -> &'static str {
        match self {
            Self::Count => "ahi_count",
            Self::Radiance => "ahi_radiance",
            Self::BrightnessTemperature => "ahi_bt",
        }
    }

    pub fn units(self) -> &'static str {
        match self {
            Self::Count => "count",
            Self::Radiance => "W m-2 sr-1 um-1",
            Self::BrightnessTemperature => "K",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HimawariDecodedSegment {
    pub header: HimawariHsdHeader,
    pub values: Vec<f32>,
}

pub fn list_latest_segments(
    agent: &ureq::Agent,
    request: &HimawariLatestRequest,
) -> Result<HimawariLatestResult, Box<dyn Error>> {
    let lookback_minutes = request
        .lookback_minutes
        .max(request.product.cadence_minutes());
    let cadence = request.product.cadence_minutes();
    let mut scan_time = round_down_scan_time(Utc::now(), cadence);
    let stop = scan_time - chrono::Duration::minutes(lookback_minutes);

    while scan_time >= stop {
        let prefix = request.product.scan_prefix(scan_time);
        let objects = list_s3_objects(agent, request.satellite.bucket(), &prefix, None)?;
        let mut segments = objects
            .into_iter()
            .filter_map(|object| {
                let name = parse_segment_name(object_filename(&object.key))?;
                (name.satellite == request.satellite
                    && name.scan_time == scan_time
                    && request.band.map_or(true, |band| name.band == band))
                .then_some(HimawariSegment { object, name })
            })
            .collect::<Vec<_>>();
        if !segments.is_empty() && (!request.require_complete || is_complete_segment_set(&segments))
        {
            segments.sort_by(|a, b| {
                a.name
                    .band
                    .cmp(&b.name.band)
                    .then(a.name.segment_index.cmp(&b.name.segment_index))
            });
            return Ok(HimawariLatestResult {
                satellite: request.satellite,
                product: request.product,
                scan_time,
                prefix,
                segments,
            });
        }
        scan_time -= chrono::Duration::minutes(cadence);
    }

    Err(format!(
        "no {} {} segment(s) found in the last {} min",
        request.satellite.slug(),
        request.product.slug(),
        lookback_minutes
    )
    .into())
}

pub fn read_download_manifest(path: &Path) -> Result<HimawariDownloadManifest, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let manifest: HimawariDownloadManifest = serde_json::from_slice(&bytes)?;
    if manifest.schema != HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA {
        return Err(format!(
            "unsupported Himawari manifest schema '{}' in {}",
            manifest.schema,
            path.display()
        )
        .into());
    }
    Ok(manifest)
}

pub fn stage_download_manifest(
    manifest_path: &Path,
    out_dir: &Path,
) -> Result<HimawariStageManifest, Box<dyn Error>> {
    let manifest = read_download_manifest(manifest_path)?;
    let mut staged = Vec::with_capacity(manifest.segments.len());
    let mut total_compressed_bytes = 0_u64;
    let mut total_raw_bytes = 0_u64;
    fs::create_dir_all(out_dir)?;

    for segment in &manifest.segments {
        let compressed_path = PathBuf::from(&segment.cache_path);
        let compressed_bytes = fs::metadata(&compressed_path)?.len();
        if compressed_bytes != segment.size_bytes {
            return Err(format!(
                "compressed byte count mismatch for {}: manifest {}, file {}",
                segment.cache_path, segment.size_bytes, compressed_bytes
            )
            .into());
        }
        let relative = raw_segment_relative_path(&segment.key)?;
        let raw_path = out_dir.join(relative);
        if let Some(parent) = raw_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let input = fs::File::open(&compressed_path)?;
        let mut decoder = MultiBzDecoder::new(input);
        let mut output = fs::File::create(&raw_path)?;
        let raw_bytes = io::copy(&mut decoder, &mut output)?;
        output.sync_all()?;
        total_compressed_bytes = total_compressed_bytes.saturating_add(compressed_bytes);
        total_raw_bytes = total_raw_bytes.saturating_add(raw_bytes);
        staged.push(HimawariStagedSegment {
            band: segment.band,
            segment_index: segment.segment_index,
            segment_count: segment.segment_count,
            key: segment.key.clone(),
            compressed_path: compressed_path.display().to_string(),
            compressed_bytes,
            raw_path: raw_path.display().to_string(),
            raw_bytes,
        });
    }

    Ok(HimawariStageManifest {
        schema: HIMAWARI_STAGE_MANIFEST_SCHEMA.to_string(),
        source_manifest: manifest_path.display().to_string(),
        satellite: manifest.satellite,
        platform: manifest.platform,
        product: manifest.product,
        scan_time_utc: manifest.scan_time_utc,
        band: manifest.band,
        source_complete: manifest.source_complete,
        segments_staged: staged.len(),
        total_compressed_bytes,
        total_raw_bytes,
        out_dir: out_dir.display().to_string(),
        segments: staged,
    })
}

pub fn inspect_hsd_file(path: &Path) -> Result<HimawariHsdHeader, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let mut header = parse_hsd_header(&bytes)?;
    header.path = Some(path.display().to_string());
    Ok(header)
}

pub fn parse_hsd_header(bytes: &[u8]) -> Result<HimawariHsdHeader, Box<dyn Error>> {
    require_len(bytes, 282, "HSD basic information block")?;
    if bytes[0] != 1 {
        return Err(format!("expected HSD block #1 at offset 0, got {}", bytes[0]).into());
    }
    let byte_order = match bytes[5] {
        0 => HimawariByteOrder::Little,
        1 => HimawariByteOrder::Big,
        value => return Err(format!("unsupported HSD byte order flag {value}").into()),
    };
    let block1_len = read_u16(bytes, 1, byte_order)?;
    if block1_len < 82 {
        return Err(format!("HSD block #1 is too short: {block1_len}").into());
    }
    let total_header_blocks = read_u16(bytes, 3, byte_order)?;
    let total_header_length = read_u32(bytes, 70, byte_order)?;
    let total_data_length = read_u32(bytes, 74, byte_order)?;
    require_len(
        bytes,
        total_header_length as usize,
        "HSD declared header length",
    )?;

    let mut block_lengths = Vec::new();
    let mut offset = 0_usize;
    let mut data_info = None;
    let mut projection = None;
    let mut calibration = None;
    let mut segment = None;
    for _ in 0..total_header_blocks {
        require_len(bytes, offset + 3, "HSD block prefix")?;
        let block_number = bytes[offset];
        let length = if block_number == 10 {
            require_len(bytes, offset + 5, "HSD block #10 prefix")?;
            read_u32(bytes, offset + 1, byte_order)?
        } else {
            u32::from(read_u16(bytes, offset + 1, byte_order)?)
        };
        if length == 0 {
            return Err(
                format!("HSD block #{block_number} at offset {offset} has zero length").into(),
            );
        }
        let block_end = offset.saturating_add(length as usize);
        require_len(bytes, block_end, "HSD block body")?;
        block_lengths.push(HimawariBlockLength {
            block_number,
            offset: offset as u32,
            length,
        });
        let block = &bytes[offset..block_end];
        match block_number {
            2 => data_info = Some(parse_data_info(block, byte_order)?),
            3 => projection = Some(parse_projection_info(block, byte_order)?),
            5 => calibration = Some(parse_calibration_info(block, byte_order)?),
            7 => segment = Some(parse_segment_info(block, byte_order)?),
            _ => {}
        }
        offset = block_end;
    }
    if offset != total_header_length as usize {
        return Err(format!(
            "HSD header block lengths sum to {offset}, declared header length is {total_header_length}"
        )
        .into());
    }
    let data = data_info.ok_or("HSD header is missing block #2 data information")?;
    let expected_file_length = u64::from(total_header_length) + u64::from(total_data_length);
    Ok(HimawariHsdHeader {
        path: None,
        byte_order,
        total_header_blocks,
        block_lengths,
        satellite_name: read_ascii(bytes, 6, 16)?,
        processing_center: read_ascii(bytes, 22, 16)?,
        observation_area: read_ascii(bytes, 38, 4)?,
        observation_timeline: read_u16(bytes, 44, byte_order)?,
        observation_start_mjd: read_f64(bytes, 46, byte_order)?,
        observation_end_mjd: read_f64(bytes, 54, byte_order)?,
        file_creation_mjd: read_f64(bytes, 62, byte_order)?,
        total_header_length,
        total_data_length,
        quality_flags: [bytes[78], bytes[79], bytes[80], bytes[81]],
        file_format_version: read_ascii(bytes, 82, 32)?,
        file_name: read_ascii(bytes, 114, 128)?,
        data,
        projection,
        calibration,
        segment,
        file_length: bytes.len() as u64,
        expected_file_length,
        length_matches_header: bytes.len() as u64 == expected_file_length,
    })
}

pub fn read_hsd_grid_segment(
    path: &Path,
    mode: HimawariValueMode,
) -> Result<HimawariDecodedSegment, Box<dyn Error>> {
    let bytes = fs::read(path)?;
    let mut header = parse_hsd_header(&bytes)?;
    header.path = Some(path.display().to_string());
    let values = decode_hsd_values(&bytes, &header, mode)?;
    Ok(HimawariDecodedSegment { header, values })
}

pub fn assemble_hsd_segments(
    paths: &[PathBuf],
    mode: HimawariValueMode,
    downsample: usize,
) -> Result<SatelliteGridField, Box<dyn Error>> {
    if paths.is_empty() {
        return Err("no Himawari HSD segment paths supplied".into());
    }
    let mut decoded = paths
        .iter()
        .map(|path| read_hsd_grid_segment(path, mode))
        .collect::<Result<Vec<_>, _>>()?;
    decoded.sort_by_key(|segment| {
        segment
            .header
            .segment
            .as_ref()
            .map(|segment| segment.sequence_number)
            .unwrap_or(0)
    });

    let first = &decoded[0].header;
    let projection = first
        .projection
        .as_ref()
        .ok_or("Himawari HSD header is missing projection block #3")?;
    let calibration = first
        .calibration
        .as_ref()
        .ok_or("Himawari HSD header is missing calibration block #5")?;
    let first_segment = first
        .segment
        .as_ref()
        .ok_or("Himawari HSD header is missing segment block #7")?;
    let band = u8::try_from(calibration.band_number)
        .map_err(|_| format!("unsupported Himawari band {}", calibration.band_number))?;
    let nx = usize::from(first.data.columns);
    let segment_lines = usize::from(first.data.lines);
    let total_segments = first_segment.total_segments;
    let mut expected_first_line = first_segment.first_line_number;
    let mut values = Vec::with_capacity(nx.saturating_mul(segment_lines * decoded.len()));
    let mut sequence_numbers = Vec::with_capacity(decoded.len());

    for segment in &decoded {
        validate_compatible_segment(first, &segment.header)?;
        let info = segment
            .header
            .segment
            .as_ref()
            .ok_or("Himawari HSD header is missing segment block #7")?;
        if info.first_line_number != expected_first_line {
            return Err(format!(
                "Himawari segments are not contiguous: expected first line {}, got {} in S{:02}",
                expected_first_line, info.first_line_number, info.sequence_number
            )
            .into());
        }
        expected_first_line = expected_first_line.saturating_add(segment.header.data.lines);
        sequence_numbers.push(info.sequence_number);
        values.extend_from_slice(&segment.values);
    }

    let ny = values.len() / nx;
    let x_scan_rad = (0..nx)
        .map(|col| himawari_column_scan_rad(projection, col))
        .collect::<Vec<_>>();
    let first_line_number = first_segment.first_line_number;
    let y_scan_rad = (0..ny)
        .map(|row| himawari_line_scan_rad(projection, u32::from(first_line_number) + row as u32))
        .collect::<Vec<_>>();

    let full_disk = decoded.len() == usize::from(total_segments) && first_line_number == 1;
    let sector_base = himawari_sector_slug(&first.observation_area);
    let sector = if full_disk {
        sector_base.clone()
    } else {
        let first_seq = sequence_numbers.first().copied().unwrap_or(0);
        let last_seq = sequence_numbers.last().copied().unwrap_or(first_seq);
        format!("{sector_base}_s{first_seq:02}_{last_seq:02}of{total_segments:02}")
    };
    let model = himawari_model_slug(&first.satellite_name);
    let field = SatelliteGridField {
        scene: SatelliteGridScene {
            model,
            satellite: first.satellite_name.clone(),
            provider: "jma".to_string(),
            instrument: "ahi".to_string(),
            product: format!("AHI-L1b-{}", first.observation_area),
            sector,
            band,
            layer: format!("{}_c{band:02}", mode.slug()),
            source_variable: "HSD count".to_string(),
            start_time_utc: mjd_to_datetime(first.observation_start_mjd)?,
            end_time_utc: mjd_to_datetime(
                decoded
                    .last()
                    .map(|segment| segment.header.observation_end_mjd)
                    .unwrap_or(first.observation_end_mjd),
            )?,
            projection: SatelliteProjection {
                perspective_point_height_m: (projection.satellite_distance_km
                    - projection.equatorial_radius_km)
                    * 1000.0,
                semi_major_axis_m: projection.equatorial_radius_km * 1000.0,
                semi_minor_axis_m: projection.polar_radius_km * 1000.0,
                longitude_of_projection_origin_deg: projection.sub_lon_degrees,
                sweep_angle_axis: SweepAngleAxis::X,
            },
            fixed_grid: AbiFixedGrid {
                nx,
                ny,
                x_scan_rad,
                y_scan_rad,
            },
            metadata: serde_json::json!({
                "source_format": "himawari_standard_data",
                "value_mode": mode.slug(),
                "segments": decoded.iter().map(|segment| {
                    let info = segment.header.segment.as_ref();
                    serde_json::json!({
                        "path": segment.header.path,
                        "sequence_number": info.map(|info| info.sequence_number),
                        "total_segments": info.map(|info| info.total_segments),
                        "first_line_number": info.map(|info| info.first_line_number),
                    })
                }).collect::<Vec<_>>(),
                "calibration": {
                    "central_wavelength_um": calibration.central_wavelength_um,
                    "valid_bits_per_pixel": calibration.valid_bits_per_pixel,
                    "count_to_radiance_slope": calibration.count_to_radiance_slope,
                    "count_to_radiance_intercept": calibration.count_to_radiance_intercept,
                },
            }),
        },
        variable_name: format!("{}_c{band:02}", mode.variable_prefix()),
        units: mode.units().to_string(),
        values,
    };

    Ok(downsample_satellite_field(field, downsample))
}

fn decode_hsd_values(
    bytes: &[u8],
    header: &HimawariHsdHeader,
    mode: HimawariValueMode,
) -> Result<Vec<f32>, Box<dyn Error>> {
    if !header.length_matches_header {
        return Err(format!(
            "Himawari HSD file length {} does not match declared {}",
            header.file_length, header.expected_file_length
        )
        .into());
    }
    if header.data.compression_flag != 0 {
        return Err(format!(
            "Himawari HSD data block compression '{}' is not decoded yet",
            header.data.compression
        )
        .into());
    }
    let calibration = header
        .calibration
        .as_ref()
        .ok_or("Himawari HSD header is missing calibration block #5")?;
    let pixels = usize::from(header.data.columns).saturating_mul(usize::from(header.data.lines));
    let expected_data_bytes = pixels.saturating_mul(2);
    if header.total_data_length as usize != expected_data_bytes {
        return Err(format!(
            "Himawari HSD data length {} does not match {}x{}x2 = {}",
            header.total_data_length, header.data.columns, header.data.lines, expected_data_bytes
        )
        .into());
    }
    let data_start = header.total_header_length as usize;
    require_len(bytes, data_start + expected_data_bytes, "HSD data block")?;

    let mut values = Vec::with_capacity(pixels);
    for index in 0..pixels {
        let raw = read_u16(bytes, data_start + index * 2, header.byte_order)?;
        values.push(hsd_raw_count_to_value(raw, header, calibration, mode)?);
    }
    Ok(values)
}

fn hsd_raw_count_to_value(
    raw: u16,
    header: &HimawariHsdHeader,
    calibration: &HimawariCalibrationInfo,
    mode: HimawariValueMode,
) -> Result<f32, Box<dyn Error>> {
    if raw == calibration.error_pixel_count || raw == calibration.outside_scan_count {
        return Ok(f32::NAN);
    }
    let count = f64::from(valid_hsd_count(
        raw,
        header.data.bits_per_pixel,
        calibration.valid_bits_per_pixel,
    ));
    let radiance =
        calibration.count_to_radiance_slope * count + calibration.count_to_radiance_intercept;
    match mode {
        HimawariValueMode::Count => Ok(count as f32),
        HimawariValueMode::Radiance => Ok(if radiance.is_finite() && radiance > 0.0 {
            radiance as f32
        } else {
            f32::NAN
        }),
        HimawariValueMode::BrightnessTemperature => {
            Ok(brightness_temperature_kelvin(radiance, calibration)? as f32)
        }
    }
}

fn valid_hsd_count(raw: u16, bits_per_pixel: u16, valid_bits_per_pixel: u16) -> u16 {
    let shift = bits_per_pixel.saturating_sub(valid_bits_per_pixel).min(15);
    raw >> shift
}

fn brightness_temperature_kelvin(
    radiance_per_um: f64,
    calibration: &HimawariCalibrationInfo,
) -> Result<f64, Box<dyn Error>> {
    if !(radiance_per_um.is_finite() && radiance_per_um > 0.0) {
        return Ok(f64::NAN);
    }
    let constants = calibration
        .physical_constants
        .as_ref()
        .ok_or("Himawari brightness temperature requires infrared Planck constants")?;
    let correction = calibration.planck_or_albedo_coefficients;
    let wavelength_m = calibration.central_wavelength_um * 1.0e-6;
    let radiance_per_m = radiance_per_um * 1.0e6;
    let numerator = 2.0 * constants.planck_constant_j_s * constants.speed_of_light_m_s.powi(2);
    let denominator = radiance_per_m * wavelength_m.powi(5);
    if !(denominator.is_finite() && denominator > 0.0) {
        return Ok(f64::NAN);
    }
    let log_arg = numerator / denominator + 1.0;
    if !(log_arg.is_finite() && log_arg > 1.0) {
        return Ok(f64::NAN);
    }
    let planck_term = constants.planck_constant_j_s * constants.speed_of_light_m_s
        / constants.boltzmann_constant_j_k;
    let blackbody_kelvin = planck_term / (wavelength_m * log_arg.ln());
    let effective_kelvin =
        correction[0] + correction[1] * blackbody_kelvin + correction[2] * blackbody_kelvin.powi(2);
    Ok(if effective_kelvin.is_finite() {
        effective_kelvin
    } else {
        f64::NAN
    })
}

fn validate_compatible_segment(
    first: &HimawariHsdHeader,
    candidate: &HimawariHsdHeader,
) -> Result<(), Box<dyn Error>> {
    if first.satellite_name != candidate.satellite_name
        || first.observation_area != candidate.observation_area
        || first.data.columns != candidate.data.columns
        || first.data.lines != candidate.data.lines
    {
        return Err(format!(
            "incompatible Himawari segment {}",
            candidate.path.as_deref().unwrap_or("(unknown)")
        )
        .into());
    }
    let first_calibration = first
        .calibration
        .as_ref()
        .ok_or("Himawari HSD header is missing calibration block #5")?;
    let candidate_calibration = candidate
        .calibration
        .as_ref()
        .ok_or("Himawari HSD header is missing calibration block #5")?;
    if first_calibration.band_number != candidate_calibration.band_number {
        return Err(format!(
            "mixed Himawari bands: B{} and B{}",
            first_calibration.band_number, candidate_calibration.band_number
        )
        .into());
    }
    let first_segment = first
        .segment
        .as_ref()
        .ok_or("Himawari HSD header is missing segment block #7")?;
    let candidate_segment = candidate
        .segment
        .as_ref()
        .ok_or("Himawari HSD header is missing segment block #7")?;
    if first_segment.total_segments != candidate_segment.total_segments {
        return Err("mixed Himawari total segment counts".into());
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

fn himawari_column_scan_rad(projection: &HimawariProjectionInfo, zero_based_col: usize) -> f64 {
    let column = zero_based_col as f64 + 1.0;
    ((column - f64::from(projection.coff)) * 65536.0 / f64::from(projection.cfac)).to_radians()
}

fn himawari_line_scan_rad(projection: &HimawariProjectionInfo, one_based_line: u32) -> f64 {
    ((f64::from(projection.loff) - f64::from(one_based_line)) * 65536.0
        / f64::from(projection.lfac))
    .to_radians()
}

fn mjd_to_datetime(mjd: f64) -> Result<DateTime<Utc>, Box<dyn Error>> {
    if !mjd.is_finite() {
        return Err(format!("invalid MJD {mjd}").into());
    }
    let epoch = NaiveDate::from_ymd_opt(1858, 11, 17)
        .and_then(|date| date.and_hms_opt(0, 0, 0))
        .ok_or("failed to construct MJD epoch")?;
    let millis = (mjd * 86_400_000.0).round();
    if millis < i64::MIN as f64 || millis > i64::MAX as f64 {
        return Err(format!("MJD {mjd} is out of supported range").into());
    }
    let naive = epoch
        .checked_add_signed(chrono::Duration::milliseconds(millis as i64))
        .ok_or_else(|| format!("MJD {mjd} is out of supported range"))?;
    Ok(Utc.from_utc_datetime(&naive))
}

fn himawari_sector_slug(area: &str) -> String {
    match area {
        "FLDK" => "fulldisk".to_string(),
        value if value.starts_with("JP") => "japan".to_string(),
        value if value.starts_with("R3") => "target".to_string(),
        value if value.starts_with("R4") => "landmark4".to_string(),
        value if value.starts_with("R5") => "landmark5".to_string(),
        value => value.to_ascii_lowercase(),
    }
}

fn himawari_model_slug(name: &str) -> String {
    match name.trim() {
        "Himawari-8" => "h8".to_string(),
        "Himawari-9" => "h9".to_string(),
        value => value.to_ascii_lowercase().replace([' ', '-'], ""),
    }
}

fn parse_data_info(
    block: &[u8],
    byte_order: HimawariByteOrder,
) -> Result<HimawariDataInfo, Box<dyn Error>> {
    require_len(block, 10, "HSD data information block")?;
    Ok(HimawariDataInfo {
        bits_per_pixel: read_u16(block, 3, byte_order)?,
        columns: read_u16(block, 5, byte_order)?,
        lines: read_u16(block, 7, byte_order)?,
        compression_flag: block[9],
        compression: match block[9] {
            0 => "none",
            1 => "gzip",
            2 => "bzip2",
            _ => "unknown",
        }
        .to_string(),
    })
}

fn parse_projection_info(
    block: &[u8],
    byte_order: HimawariByteOrder,
) -> Result<HimawariProjectionInfo, Box<dyn Error>> {
    require_len(block, 51, "HSD projection information block")?;
    Ok(HimawariProjectionInfo {
        sub_lon_degrees: read_f64(block, 3, byte_order)?,
        cfac: read_u32(block, 11, byte_order)?,
        lfac: read_u32(block, 15, byte_order)?,
        coff: read_f32(block, 19, byte_order)?,
        loff: read_f32(block, 23, byte_order)?,
        satellite_distance_km: read_f64(block, 27, byte_order)?,
        equatorial_radius_km: read_f64(block, 35, byte_order)?,
        polar_radius_km: read_f64(block, 43, byte_order)?,
    })
}

fn parse_calibration_info(
    block: &[u8],
    byte_order: HimawariByteOrder,
) -> Result<HimawariCalibrationInfo, Box<dyn Error>> {
    require_len(block, 59, "HSD calibration information block")?;
    let band_number = read_u16(block, 3, byte_order)?;
    let inverse_planck_coefficients = if band_number >= 7 {
        require_len(block, 107, "HSD infrared calibration block")?;
        Some([
            read_f64(block, 59, byte_order)?,
            read_f64(block, 67, byte_order)?,
            read_f64(block, 75, byte_order)?,
        ])
    } else {
        None
    };
    let physical_constants = if band_number >= 7 {
        Some(HimawariPhysicalConstants {
            speed_of_light_m_s: read_f64(block, 83, byte_order)?,
            planck_constant_j_s: read_f64(block, 91, byte_order)?,
            boltzmann_constant_j_k: read_f64(block, 99, byte_order)?,
        })
    } else {
        None
    };
    Ok(HimawariCalibrationInfo {
        band_number,
        central_wavelength_um: read_f64(block, 5, byte_order)?,
        valid_bits_per_pixel: read_u16(block, 13, byte_order)?,
        error_pixel_count: read_u16(block, 15, byte_order)?,
        outside_scan_count: read_u16(block, 17, byte_order)?,
        count_to_radiance_slope: read_f64(block, 19, byte_order)?,
        count_to_radiance_intercept: read_f64(block, 27, byte_order)?,
        planck_or_albedo_coefficients: [
            read_f64(block, 35, byte_order)?,
            read_f64(block, 43, byte_order)?,
            read_f64(block, 51, byte_order)?,
        ],
        inverse_planck_coefficients,
        physical_constants,
    })
}

fn parse_segment_info(
    block: &[u8],
    byte_order: HimawariByteOrder,
) -> Result<HimawariSegmentInfo, Box<dyn Error>> {
    require_len(block, 7, "HSD segment information block")?;
    Ok(HimawariSegmentInfo {
        total_segments: block[3],
        sequence_number: block[4],
        first_line_number: read_u16(block, 5, byte_order)?,
    })
}

fn require_len(data: &[u8], needed: usize, context: &str) -> Result<(), Box<dyn Error>> {
    if data.len() < needed {
        Err(format!(
            "{context} requires at least {needed} bytes, got {}",
            data.len()
        )
        .into())
    } else {
        Ok(())
    }
}

fn read_u16(
    data: &[u8],
    offset: usize,
    byte_order: HimawariByteOrder,
) -> Result<u16, Box<dyn Error>> {
    require_len(data, offset + 2, "u16 read")?;
    let bytes = [data[offset], data[offset + 1]];
    Ok(match byte_order {
        HimawariByteOrder::Little => u16::from_le_bytes(bytes),
        HimawariByteOrder::Big => u16::from_be_bytes(bytes),
    })
}

fn read_u32(
    data: &[u8],
    offset: usize,
    byte_order: HimawariByteOrder,
) -> Result<u32, Box<dyn Error>> {
    require_len(data, offset + 4, "u32 read")?;
    let bytes = [
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ];
    Ok(match byte_order {
        HimawariByteOrder::Little => u32::from_le_bytes(bytes),
        HimawariByteOrder::Big => u32::from_be_bytes(bytes),
    })
}

fn read_f32(
    data: &[u8],
    offset: usize,
    byte_order: HimawariByteOrder,
) -> Result<f32, Box<dyn Error>> {
    Ok(f32::from_bits(read_u32(data, offset, byte_order)?))
}

fn read_f64(
    data: &[u8],
    offset: usize,
    byte_order: HimawariByteOrder,
) -> Result<f64, Box<dyn Error>> {
    require_len(data, offset + 8, "f64 read")?;
    let bytes = [
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
        data[offset + 4],
        data[offset + 5],
        data[offset + 6],
        data[offset + 7],
    ];
    Ok(match byte_order {
        HimawariByteOrder::Little => f64::from_le_bytes(bytes),
        HimawariByteOrder::Big => f64::from_be_bytes(bytes),
    })
}

fn read_ascii(data: &[u8], offset: usize, len: usize) -> Result<String, Box<dyn Error>> {
    require_len(data, offset + len, "ASCII read")?;
    Ok(String::from_utf8_lossy(&data[offset..offset + len])
        .trim_matches(char::from(0))
        .trim()
        .to_string())
}

pub fn parse_segment_name(filename: &str) -> Option<HimawariSegmentName> {
    let filename = filename.strip_suffix(".DAT.bz2")?;
    let mut parts = filename.split('_');
    let prefix = parts.next()?;
    if prefix != "HS" {
        return None;
    }
    let satellite = match parts.next()? {
        "H08" => HimawariSatellite::H8,
        "H09" => HimawariSatellite::H9,
        _ => return None,
    };
    let date = parts.next()?;
    let time = parts.next()?;
    let band = parts.next()?.strip_prefix('B')?.parse::<u8>().ok()?;
    let product = parts.next()?.to_string();
    let resolution = parts.next()?.to_string();
    let segment = parts.next()?.strip_prefix('S')?;
    if segment.len() != 4 {
        return None;
    }
    let segment_index = segment[0..2].parse::<u8>().ok()?;
    let segment_count = segment[2..4].parse::<u8>().ok()?;
    let scan_time = NaiveDateTime::parse_from_str(&format!("{date}{time}"), "%Y%m%d%H%M").ok()?;
    let scan_time = Utc.from_utc_datetime(&scan_time);
    Some(HimawariSegmentName {
        satellite,
        scan_time,
        band,
        product,
        resolution,
        segment_index,
        segment_count,
    })
}

fn raw_segment_relative_path(key: &str) -> Result<PathBuf, Box<dyn Error>> {
    let mut path = PathBuf::new();
    for part in key.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains(':')
        {
            return Err(format!("unsafe Himawari object key for staging: {key}").into());
        }
        path.push(part);
    }
    let Some(file_name) = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
    else {
        return Err(format!("Himawari object key has no file name: {key}").into());
    };
    if let Some(raw_name) = file_name.strip_suffix(".bz2") {
        path.set_file_name(raw_name);
    }
    Ok(path)
}

pub fn is_complete_segment_set(segments: &[HimawariSegment]) -> bool {
    let mut by_band = BTreeMap::<u8, (u8, Vec<bool>)>::new();
    for segment in segments {
        let count = segment.name.segment_count;
        let index = segment.name.segment_index;
        if count == 0 || index == 0 || index > count {
            return false;
        }
        let entry = by_band
            .entry(segment.name.band)
            .or_insert_with(|| (count, vec![false; usize::from(count)]));
        if entry.0 != count {
            return false;
        }
        entry.1[usize::from(index - 1)] = true;
    }
    !by_band.is_empty()
        && by_band
            .values()
            .all(|(_, present)| present.iter().all(|present| *present))
}

fn round_down_scan_time(time: DateTime<Utc>, cadence_minutes: i64) -> DateTime<Utc> {
    let cadence = cadence_minutes.max(1) as u32;
    let minute = time.minute() - (time.minute() % cadence);
    time.with_second(0)
        .and_then(|time| time.with_nanosecond(0))
        .and_then(|time| time.with_minute(minute))
        .unwrap_or(time)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bzip2::Compression;
    use bzip2::write::BzEncoder;
    use chrono::TimeZone;
    use std::io::Write;

    #[test]
    fn parses_himawari_satellite_aliases() {
        assert_eq!(
            HimawariSatellite::parse("himawari-9"),
            Some(HimawariSatellite::H9)
        );
        assert_eq!(
            HimawariSatellite::parse("noaa-himawari8"),
            Some(HimawariSatellite::H8)
        );
        assert_eq!(HimawariSatellite::parse("goes19"), None);
    }

    #[test]
    fn full_disk_prefix_matches_public_bucket_shape() {
        let time = Utc.with_ymd_and_hms(2026, 6, 15, 0, 30, 0).unwrap();
        assert_eq!(
            HimawariProduct::AhiL1bFldk.scan_prefix(time),
            "AHI-L1b-FLDK/2026/06/15/0030/"
        );
    }

    #[test]
    fn parses_segment_names() {
        let parsed = parse_segment_name("HS_H09_20260615_0030_B13_FLDK_R20_S0410.DAT.bz2").unwrap();
        assert_eq!(parsed.satellite, HimawariSatellite::H9);
        assert_eq!(
            parsed.scan_time,
            Utc.with_ymd_and_hms(2026, 6, 15, 0, 30, 0).unwrap()
        );
        assert_eq!(parsed.band, 13);
        assert_eq!(parsed.product, "FLDK");
        assert_eq!(parsed.resolution, "R20");
        assert_eq!(parsed.segment_index, 4);
        assert_eq!(parsed.segment_count, 10);
    }

    #[test]
    fn detects_complete_segment_sets_per_band() {
        let object = S3Object {
            key: String::new(),
            size_bytes: 0,
            last_modified: String::new(),
        };
        let segment = |index| HimawariSegment {
            object: object.clone(),
            name: HimawariSegmentName {
                satellite: HimawariSatellite::H9,
                scan_time: Utc.with_ymd_and_hms(2026, 6, 15, 0, 30, 0).unwrap(),
                band: 13,
                product: "FLDK".to_string(),
                resolution: "R20".to_string(),
                segment_index: index,
                segment_count: 3,
            },
        };
        assert!(is_complete_segment_set(&[
            segment(1),
            segment(2),
            segment(3)
        ]));
        assert!(!is_complete_segment_set(&[segment(1), segment(3)]));
    }

    #[test]
    fn stages_download_manifest_by_decompressing_segments() {
        let root =
            std::env::temp_dir().join(format!("rw-sat-himawari-stage-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let cache = root.join("cache").join("segment.DAT.bz2");
        fs::create_dir_all(cache.parent().unwrap()).unwrap();
        let mut encoder = BzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(b"raw-").unwrap();
        let mut compressed = encoder.finish().unwrap();
        let mut second_encoder = BzEncoder::new(Vec::new(), Compression::best());
        second_encoder.write_all(b"himawari-segment").unwrap();
        compressed.extend(second_encoder.finish().unwrap());
        fs::write(&cache, &compressed).unwrap();

        let manifest = HimawariDownloadManifest {
            schema: HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA.to_string(),
            satellite: "h9".to_string(),
            platform: "H09".to_string(),
            bucket: HIMAWARI9_BUCKET.to_string(),
            product: "ahi-l1b-fldk".to_string(),
            scan_time_utc: "2026-06-15T00:40:00Z".to_string(),
            prefix: "AHI-L1b-FLDK/2026/06/15/0040/".to_string(),
            band: 13,
            segments_downloaded: 1,
            segments_available: 1,
            source_complete: true,
            allow_partial: false,
            total_downloaded_bytes: compressed.len() as u64,
            cache_root: root.join("cache").display().to_string(),
            segments: vec![HimawariManifestSegment {
                band: 13,
                segment_index: 1,
                segment_count: 1,
                product: "FLDK".to_string(),
                resolution: "R20".to_string(),
                key: "AHI-L1b-FLDK/2026/06/15/0040/HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT.bz2"
                    .to_string(),
                url: "https://example.invalid/segment".to_string(),
                last_modified: "2026-06-15T00:51:57.000Z".to_string(),
                size_bytes: compressed.len() as u64,
                cache_path: cache.display().to_string(),
                cache_hit: false,
            }],
        };
        let manifest_path = root.join("manifest.json");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let out = root.join("raw");
        let staged = stage_download_manifest(&manifest_path, &out).unwrap();
        assert_eq!(staged.segments_staged, 1);
        assert_eq!(staged.total_raw_bytes, 20);
        let raw =
            out.join("AHI-L1b-FLDK/2026/06/15/0040/HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT");
        assert_eq!(fs::read(raw).unwrap(), b"raw-himawari-segment");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_unsafe_stage_keys() {
        assert!(raw_segment_relative_path("../bad.DAT.bz2").is_err());
        assert!(raw_segment_relative_path("safe/../bad.DAT.bz2").is_err());
        assert!(raw_segment_relative_path("safe\\bad.DAT.bz2").is_err());
        assert!(
            raw_segment_relative_path(
                "AHI-L1b-FLDK/2026/06/15/0040/HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT.bz2"
            )
            .unwrap()
            .ends_with("HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT")
        );
    }

    #[test]
    fn parses_hsd_header_metadata() {
        let bytes = synthetic_hsd_file();
        let header = parse_hsd_header(&bytes).unwrap();

        assert_eq!(header.byte_order, HimawariByteOrder::Little);
        assert_eq!(header.total_header_blocks, 11);
        assert_eq!(header.total_header_length, 1463);
        assert_eq!(header.total_data_length, 4);
        assert_eq!(header.satellite_name, "Himawari-9");
        assert_eq!(header.processing_center, "MSC");
        assert_eq!(header.observation_area, "FLDK");
        assert_eq!(header.file_format_version, "1.3");
        assert_eq!(
            header.file_name,
            "HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT"
        );
        assert_eq!(header.data.bits_per_pixel, 16);
        assert_eq!(header.data.columns, 5500);
        assert_eq!(header.data.lines, 550);
        assert_eq!(header.data.compression, "bzip2");
        assert_eq!(
            header.block_lengths.last().map(|block| block.block_number),
            Some(11)
        );
        assert_eq!(
            header.projection.as_ref().map(|projection| {
                (projection.sub_lon_degrees, projection.cfac, projection.lfac)
            }),
            Some((140.7, 81_365_527, 81_365_527))
        );
        assert_eq!(
            header.calibration.as_ref().map(|calibration| {
                (
                    calibration.band_number,
                    calibration.valid_bits_per_pixel,
                    calibration.central_wavelength_um,
                )
            }),
            Some((13, 12, 10.4073))
        );
        assert_eq!(
            header.segment,
            Some(HimawariSegmentInfo {
                total_segments: 10,
                sequence_number: 1,
                first_line_number: 1,
            })
        );
        assert_eq!(header.file_length, 1467);
        assert_eq!(header.expected_file_length, 1467);
        assert!(header.length_matches_header);
    }

    #[test]
    fn decodes_hsd_counts_and_assembles_partial_grid() {
        let root =
            std::env::temp_dir().join(format!("rw-sat-himawari-decode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let path = root.join("HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT");
        let mut bytes = synthetic_hsd_file_with_data(2, 1, &[0x30, 0x12, 0xfe, 0xff]);
        bytes[282 + 9] = 0;
        fs::write(&path, bytes).unwrap();

        let decoded = read_hsd_grid_segment(&path, HimawariValueMode::Count).unwrap();
        assert_eq!(decoded.values[0], 0x123 as f32);
        assert!(decoded.values[1].is_nan());

        let field = assemble_hsd_segments(&[path], HimawariValueMode::Count, 1).unwrap();
        assert_eq!(field.scene.model, "h9");
        assert_eq!(field.scene.provider, "jma");
        assert_eq!(field.scene.instrument, "ahi");
        assert_eq!(field.scene.sector, "fulldisk_s01_01of10");
        assert_eq!(field.scene.fixed_grid.nx, 2);
        assert_eq!(field.scene.fixed_grid.ny, 1);
        assert_eq!(field.variable_name, "ahi_count_c13");
        assert_eq!(field.units, "count");
        assert_eq!(field.values[0], 0x123 as f32);
        assert!(field.values[1].is_nan());

        let _ = fs::remove_dir_all(&root);
    }

    fn synthetic_hsd_file() -> Vec<u8> {
        synthetic_hsd_file_with_data(5500, 550, &[1, 2, 3, 4])
    }

    fn synthetic_hsd_file_with_data(columns: u16, lines: u16, data: &[u8]) -> Vec<u8> {
        let block_lengths = [
            (1_u8, 282_u32),
            (2, 50),
            (3, 127),
            (4, 139),
            (5, 147),
            (6, 259),
            (7, 47),
            (8, 61),
            (9, 45),
            (10, 47),
            (11, 259),
        ];
        let header_len = block_lengths
            .iter()
            .map(|(_, length)| *length as usize)
            .sum::<usize>();
        let mut bytes = vec![0_u8; header_len + data.len()];

        let mut offset = 0_usize;
        let mut offsets = BTreeMap::new();
        for (block_number, length) in block_lengths {
            offsets.insert(block_number, offset);
            bytes[offset] = block_number;
            if block_number == 10 {
                write_u32(&mut bytes, offset + 1, length);
            } else {
                write_u16(&mut bytes, offset + 1, length as u16);
            }
            offset += length as usize;
        }
        bytes[header_len..].copy_from_slice(data);

        let block1 = offsets[&1];
        write_u16(&mut bytes, block1 + 3, 11);
        bytes[block1 + 5] = 0;
        write_ascii(&mut bytes, block1 + 6, 16, "Himawari-9");
        write_ascii(&mut bytes, block1 + 22, 16, "MSC");
        write_ascii(&mut bytes, block1 + 38, 4, "FLDK");
        write_u16(&mut bytes, block1 + 44, 101);
        write_f64(&mut bytes, block1 + 46, 60476.0277777778);
        write_f64(&mut bytes, block1 + 54, 60476.0347222222);
        write_f64(&mut bytes, block1 + 62, 60476.0416666667);
        write_u32(&mut bytes, block1 + 70, header_len as u32);
        write_u32(&mut bytes, block1 + 74, data.len() as u32);
        bytes[block1 + 78..block1 + 82].copy_from_slice(&[0, 1, 0, 1]);
        write_ascii(&mut bytes, block1 + 82, 32, "1.3");
        write_ascii(
            &mut bytes,
            block1 + 114,
            128,
            "HS_H09_20260615_0040_B13_FLDK_R20_S0110.DAT",
        );

        let block2 = offsets[&2];
        write_u16(&mut bytes, block2 + 3, 16);
        write_u16(&mut bytes, block2 + 5, columns);
        write_u16(&mut bytes, block2 + 7, lines);
        bytes[block2 + 9] = 2;

        let block3 = offsets[&3];
        write_f64(&mut bytes, block3 + 3, 140.7);
        write_u32(&mut bytes, block3 + 11, 81_365_527);
        write_u32(&mut bytes, block3 + 15, 81_365_527);
        write_f32(&mut bytes, block3 + 19, 2750.5);
        write_f32(&mut bytes, block3 + 23, 2750.5);
        write_f64(&mut bytes, block3 + 27, 42164.0);
        write_f64(&mut bytes, block3 + 35, 6378.137);
        write_f64(&mut bytes, block3 + 43, 6356.7523);

        let block5 = offsets[&5];
        write_u16(&mut bytes, block5 + 3, 13);
        write_f64(&mut bytes, block5 + 5, 10.4073);
        write_u16(&mut bytes, block5 + 13, 12);
        write_u16(&mut bytes, block5 + 15, 65535);
        write_u16(&mut bytes, block5 + 17, 65534);
        write_f64(&mut bytes, block5 + 19, 0.001);
        write_f64(&mut bytes, block5 + 27, -0.1);
        write_f64(&mut bytes, block5 + 35, 300.0);
        write_f64(&mut bytes, block5 + 43, 0.1);
        write_f64(&mut bytes, block5 + 51, 0.01);

        let block7 = offsets[&7];
        bytes[block7 + 3] = 10;
        bytes[block7 + 4] = 1;
        write_u16(&mut bytes, block7 + 5, 1);

        bytes
    }

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_f32(bytes: &mut [u8], offset: usize, value: f32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_f64(bytes: &mut [u8], offset: usize, value: f64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn write_ascii(bytes: &mut [u8], offset: usize, len: usize, value: &str) {
        let value = value.as_bytes();
        let count = value.len().min(len);
        bytes[offset..offset + count].copy_from_slice(&value[..count]);
    }
}

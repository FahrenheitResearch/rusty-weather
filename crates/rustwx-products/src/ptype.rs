//! Preparation and orchestration for live surface precipitation type.
//!
//! The expensive cached product is a thermodynamic phase prior. Radar is
//! deliberately absent from the cache key and metadata here; callers attach
//! live precipitation occurrence and radar provenance after this analysis.

use crate::gridded::{PressureFields, SurfaceFields, compute_height_agl_3d};
use chrono::{DateTime, Utc};
use rustwx_calc::{PtypeGridInputs, PtypeScores, VolumeShape, compute_modified_bourgouin_ptype};
use rustwx_core::{GridShape, LatLonGrid, RustwxError};
use rustwx_regrid::{MissingPolicy, RegridError, RegridMethod, RegridOptions, RegridPlan};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;

pub use rustwx_calc::{PrecipType, PtypeOptions, PtypeQc};

pub const CURRENT_PTYPE_ALGORITHM_VERSION: PtypeAlgorithmVersion = PtypeAlgorithmVersion(1);
pub const DEFAULT_SURFACE_REPLACEMENT_DISTANCE_KM: f64 = 50.0;

#[derive(Debug, Error)]
pub enum PtypeProductError {
    #[error("invalid {field} length: expected {expected}, got {actual}")]
    InvalidLength {
        field: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("invalid {field} value at index {index}: {value}")]
    InvalidValue {
        field: &'static str,
        index: usize,
        value: f64,
    },
    #[error("invalid precipitation-type configuration: {0}")]
    InvalidConfig(String),
    #[error(transparent)]
    Core(#[from] RustwxError),
    #[error(transparent)]
    Calc(#[from] rustwx_calc::CalcError),
    #[error(transparent)]
    Regrid(#[from] RegridError),
}

/// Version of the scientific/preparation contract used in cache identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Ord, PartialOrd)]
#[serde(transparent)]
pub struct PtypeAlgorithmVersion(pub u16);

impl Default for PtypeAlgorithmVersion {
    fn default() -> Self {
        CURRENT_PTYPE_ALGORITHM_VERSION
    }
}

/// Preparation choices that can change the thermodynamic prior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PtypePreparationOptions {
    pub replace_model_surface: bool,
    pub surface_max_distance_m: u32,
}

impl Default for PtypePreparationOptions {
    fn default() -> Self {
        Self {
            replace_model_surface: false,
            surface_max_distance_m: 50_000,
        }
    }
}

/// Stable identity for the expensive thermodynamic prior.
///
/// Radar scan/source fields are intentionally impossible to put in this key.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PtypeThermodynamicCacheKey {
    pub model_id: String,
    pub model_cycle: DateTime<Utc>,
    pub model_valid: DateTime<Utc>,
    pub domain_grid_id: String,
    pub surface_analysis_id: String,
    pub surface_analysis_valid: DateTime<Utc>,
    pub algorithm_version: PtypeAlgorithmVersion,
    pub preparation_options: PtypePreparationOptions,
}

impl PtypeThermodynamicCacheKey {
    pub fn from_metadata(
        metadata: &LivePtypeMetadata,
        domain_grid_id: impl Into<String>,
        preparation_options: PtypePreparationOptions,
    ) -> Self {
        Self {
            model_id: metadata.model_id.clone(),
            model_cycle: metadata.model_cycle,
            model_valid: metadata.model_valid,
            domain_grid_id: domain_grid_id.into(),
            surface_analysis_id: metadata.surface_analysis_id.clone(),
            surface_analysis_valid: metadata.surface_analysis_valid,
            algorithm_version: CURRENT_PTYPE_ALGORITHM_VERSION,
            preparation_options,
        }
    }
}

/// Thermodynamic provenance persisted with a phase-prior frame.
///
/// Radar timestamps and age are render-time state and intentionally live in
/// the consuming application rather than this cached metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LivePtypeMetadata {
    pub model_id: String,
    pub model_cycle: DateTime<Utc>,
    pub model_valid: DateTime<Utc>,
    pub model_horizontal_resolution_m: f32,
    pub surface_analysis_id: String,
    pub surface_analysis_valid: DateTime<Utc>,
}

impl LivePtypeMetadata {
    pub fn new(
        model_id: impl Into<String>,
        model_cycle: DateTime<Utc>,
        model_valid: DateTime<Utc>,
        model_horizontal_resolution_m: f32,
        surface_analysis_id: impl Into<String>,
        surface_analysis_valid: DateTime<Utc>,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            model_cycle,
            model_valid,
            model_horizontal_resolution_m,
            surface_analysis_id: surface_analysis_id.into(),
            surface_analysis_valid,
        }
    }
}

/// Validated model columns in the units expected by `rustwx-calc`.
#[derive(Debug, Clone)]
pub struct PreparedPtypeColumns {
    pub grid: LatLonGrid,
    pub shape: VolumeShape,
    /// Either one pressure per level (`nz`) or a full `[k][y][x]` volume.
    pub pressure_pa: Vec<f64>,
    pub temperature_c: Vec<f64>,
    /// Water-vapor mixing ratio, not specific humidity.
    pub mixing_ratio_kgkg: Vec<f64>,
    pub height_agl_m: Vec<f64>,
    pub psfc_pa: Vec<f64>,
    pub t2_k: Vec<f64>,
    /// Surface water-vapor mixing ratio, not specific humidity.
    pub q2_mixing_ratio_kgkg: Vec<f64>,
}

impl PreparedPtypeColumns {
    #[allow(clippy::too_many_arguments)]
    pub fn from_wrf_parts(
        grid: LatLonGrid,
        pressure_pa: Vec<f64>,
        temperature_c: Vec<f64>,
        mixing_ratio_kgkg: Vec<f64>,
        height_agl_m: Vec<f64>,
        psfc_pa: Vec<f64>,
        t2_k: Vec<f64>,
        q2_mixing_ratio_kgkg: Vec<f64>,
    ) -> Result<Self, PtypeProductError> {
        let nxy = grid.shape.checked_len()?;
        if temperature_c.is_empty() || !temperature_c.len().is_multiple_of(nxy) {
            return Err(PtypeProductError::InvalidConfig(format!(
                "temperature volume length {} is not a positive multiple of grid length {nxy}",
                temperature_c.len()
            )));
        }
        let shape = VolumeShape::new(grid.shape, temperature_c.len() / nxy)?;
        let columns = Self {
            grid,
            shape,
            pressure_pa,
            temperature_c,
            mixing_ratio_kgkg,
            height_agl_m,
            psfc_pa,
            t2_k,
            q2_mixing_ratio_kgkg,
        };
        columns.validate()?;
        Ok(columns)
    }

    pub fn validate(&self) -> Result<(), PtypeProductError> {
        let nxy = self.shape.grid.checked_len()?;
        if self.grid.shape != self.shape.grid {
            return Err(PtypeProductError::InvalidConfig(
                "prepared grid shape does not match volume grid shape".to_string(),
            ));
        }
        require_len("grid latitude", self.grid.lat_deg.len(), nxy)?;
        require_len("grid longitude", self.grid.lon_deg.len(), nxy)?;
        require_len(
            "temperature_c",
            self.temperature_c.len(),
            self.shape.len3d(),
        )?;
        require_len(
            "mixing_ratio_kgkg",
            self.mixing_ratio_kgkg.len(),
            self.shape.len3d(),
        )?;
        require_len("height_agl_m", self.height_agl_m.len(), self.shape.len3d())?;
        if self.pressure_pa.len() != self.shape.nz && self.pressure_pa.len() != self.shape.len3d() {
            return Err(PtypeProductError::InvalidLength {
                field: "pressure_pa",
                expected: self.shape.nz,
                actual: self.pressure_pa.len(),
            });
        }
        require_len("psfc_pa", self.psfc_pa.len(), nxy)?;
        require_len("t2_k", self.t2_k.len(), nxy)?;
        require_len("q2_mixing_ratio_kgkg", self.q2_mixing_ratio_kgkg.len(), nxy)?;
        Ok(())
    }
}

/// Construct columns from decoded HRRR/RAP pressure-level bundles.
///
/// `SurfaceFields` and `PressureFields` already convert GRIB `SPFH` to mixing
/// ratio. Copying those arrays is intentional: converting them again would be
/// a moisture bias. `HGT` remains MSL and is converted through the shared AGL
/// helper.
pub fn prepare_hrrr_rap_columns(
    surface: &SurfaceFields,
    pressure: &PressureFields,
) -> Result<PreparedPtypeColumns, PtypeProductError> {
    let grid_shape = GridShape::new(surface.nx, surface.ny)?;
    let nxy = grid_shape.len();
    require_len("surface latitude", surface.lat.len(), nxy)?;
    require_len("surface longitude", surface.lon.len(), nxy)?;
    require_len("surface pressure", surface.psfc_pa.len(), nxy)?;
    require_len("surface terrain", surface.orog_m.len(), nxy)?;
    require_len("surface temperature", surface.t2_k.len(), nxy)?;
    require_len("surface mixing ratio", surface.q2_kgkg.len(), nxy)?;

    let shape = VolumeShape::new(grid_shape, pressure.pressure_levels_hpa.len())?;
    require_len(
        "pressure temperature",
        pressure.temperature_c_3d.len(),
        shape.len3d(),
    )?;
    require_len(
        "pressure mixing ratio",
        pressure.qvapor_kgkg_3d.len(),
        shape.len3d(),
    )?;
    require_len("pressure height", pressure.gh_m_3d.len(), shape.len3d())?;
    if let Some(values) = pressure.pressure_3d_pa.as_ref() {
        require_len("pressure_3d_pa", values.len(), shape.len3d())?;
    }

    let grid = surface.core_grid()?;
    let height_agl_m = compute_height_agl_3d(surface, pressure, grid_shape, shape);
    let pressure_pa = pressure.pressure_3d_pa.clone().unwrap_or_else(|| {
        pressure
            .pressure_levels_hpa
            .iter()
            .map(|level| level * 100.0)
            .collect()
    });
    PreparedPtypeColumns::from_wrf_parts(
        grid,
        pressure_pa,
        pressure.temperature_c_3d.clone(),
        pressure.qvapor_kgkg_3d.clone(),
        height_agl_m,
        surface.psfc_pa.clone(),
        surface.t2_k.clone(),
        surface.q2_kgkg.clone(),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn prepare_wrf_columns(
    grid: LatLonGrid,
    pressure_pa: Vec<f64>,
    temperature_c: Vec<f64>,
    mixing_ratio_kgkg: Vec<f64>,
    height_agl_m: Vec<f64>,
    psfc_pa: Vec<f64>,
    t2_k: Vec<f64>,
    q2_mixing_ratio_kgkg: Vec<f64>,
) -> Result<PreparedPtypeColumns, PtypeProductError> {
    PreparedPtypeColumns::from_wrf_parts(
        grid,
        pressure_pa,
        temperature_c,
        mixing_ratio_kgkg,
        height_agl_m,
        psfc_pa,
        t2_k,
        q2_mixing_ratio_kgkg,
    )
}

#[derive(Debug, Clone, Copy)]
pub struct CurrentSurfaceFields<'a> {
    pub psfc_pa: &'a [f64],
    pub t2_k: &'a [f64],
    pub q2_mixing_ratio_kgkg: &'a [f64],
}

pub fn replace_current_surface(
    columns: &mut PreparedPtypeColumns,
    surface: CurrentSurfaceFields<'_>,
) -> Result<(), PtypeProductError> {
    columns.validate()?;
    let nxy = columns.shape.len2d();
    require_len("replacement psfc_pa", surface.psfc_pa.len(), nxy)?;
    require_len("replacement t2_k", surface.t2_k.len(), nxy)?;
    require_len(
        "replacement q2_mixing_ratio_kgkg",
        surface.q2_mixing_ratio_kgkg.len(),
        nxy,
    )?;
    for index in 0..nxy {
        validate_surface_triplet(
            index,
            surface.psfc_pa[index],
            surface.t2_k[index],
            surface.q2_mixing_ratio_kgkg[index],
        )?;
    }
    columns.psfc_pa.copy_from_slice(surface.psfc_pa);
    columns.t2_k.copy_from_slice(surface.t2_k);
    columns
        .q2_mixing_ratio_kgkg
        .copy_from_slice(surface.q2_mixing_ratio_kgkg);
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PtypeAnalysisFrame {
    pub grid: GridShape,
    pub rain_powt_pct: Vec<f32>,
    pub snow_powt_pct: Vec<f32>,
    pub freezing_rain_powt_pct: Vec<f32>,
    pub ice_pellets_powt_pct: Vec<f32>,
    pub qc_bits: Vec<u16>,
    pub metadata: LivePtypeMetadata,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtypeDisplayFields {
    pub display_type_code: Vec<u8>,
    pub confidence: Vec<f32>,
}

pub fn default_ptype_options() -> PtypeOptions {
    PtypeOptions::default()
}

pub fn analyze_prepared_columns(
    columns: &PreparedPtypeColumns,
    active_mask: Option<&[u8]>,
    options: &PtypeOptions,
    metadata: LivePtypeMetadata,
) -> Result<PtypeAnalysisFrame, PtypeProductError> {
    columns.validate()?;
    let output = compute_modified_bourgouin_ptype(
        PtypeGridInputs {
            shape: columns.shape,
            pressure_3d_pa: &columns.pressure_pa,
            temperature_3d_c: &columns.temperature_c,
            qvapor_3d_kgkg: &columns.mixing_ratio_kgkg,
            height_agl_3d_m: &columns.height_agl_m,
            psfc_pa: &columns.psfc_pa,
            t2_k: &columns.t2_k,
            q2_kgkg: &columns.q2_mixing_ratio_kgkg,
            active_mask,
        },
        options,
    )?;
    Ok(PtypeAnalysisFrame {
        grid: columns.shape.grid,
        rain_powt_pct: output.fields.rain_powt_pct,
        snow_powt_pct: output.fields.snow_powt_pct,
        freezing_rain_powt_pct: output.fields.freezing_rain_powt_pct,
        ice_pellets_powt_pct: output.fields.ice_pellets_powt_pct,
        qc_bits: output.fields.qc_bits,
        metadata,
    })
}

pub fn analyze_prepared_columns_default(
    columns: &PreparedPtypeColumns,
    active_mask: Option<&[u8]>,
    mixed_fraction_threshold: f64,
    metadata: LivePtypeMetadata,
) -> Result<PtypeAnalysisFrame, PtypeProductError> {
    analyze_prepared_columns(
        columns,
        active_mask,
        &PtypeOptions {
            mixed_fraction_threshold,
            include_diagnostics: false,
        },
        metadata,
    )
}

pub fn derive_display_fields_after_regrid(
    rain_powt_pct: &[f32],
    snow_powt_pct: &[f32],
    freezing_rain_powt_pct: &[f32],
    ice_pellets_powt_pct: &[f32],
    mixed_fraction_threshold: f64,
) -> Result<PtypeDisplayFields, PtypeProductError> {
    if !mixed_fraction_threshold.is_finite() || !(0.0..=1.0).contains(&mixed_fraction_threshold) {
        return Err(PtypeProductError::InvalidConfig(
            "mixed fraction threshold must be finite and in 0..=1".to_string(),
        ));
    }
    let len = rain_powt_pct.len();
    require_len("snow PoWT", snow_powt_pct.len(), len)?;
    require_len("freezing-rain PoWT", freezing_rain_powt_pct.len(), len)?;
    require_len("ice-pellet PoWT", ice_pellets_powt_pct.len(), len)?;
    let mut display_type_code = Vec::with_capacity(len);
    let mut confidence = Vec::with_capacity(len);
    for index in 0..len {
        let values = [
            rain_powt_pct[index],
            snow_powt_pct[index],
            freezing_rain_powt_pct[index],
            ice_pellets_powt_pct[index],
        ];
        if values.iter().any(|value| !value.is_finite()) {
            display_type_code.push(PrecipType::Unknown.code());
            confidence.push(f32::NAN);
            continue;
        }
        let scores = PtypeScores {
            rain_pct: values[0].clamp(0.0, 100.0) as f64,
            snow_pct: values[1].clamp(0.0, 100.0) as f64,
            freezing_rain_pct: values[2].clamp(0.0, 100.0) as f64,
            ice_pellets_pct: values[3].clamp(0.0, 100.0) as f64,
        };
        let fractions = scores.qpf_fractions();
        display_type_code.push(fractions.display_type(mixed_fraction_threshold).code());
        confidence.push(fractions.confidence() as f32);
    }
    Ok(PtypeDisplayFields {
        display_type_code,
        confidence,
    })
}

fn require_len(
    field: &'static str,
    actual: usize,
    expected: usize,
) -> Result<(), PtypeProductError> {
    if actual == expected {
        Ok(())
    } else {
        Err(PtypeProductError::InvalidLength {
            field,
            expected,
            actual,
        })
    }
}

fn validate_surface_triplet(
    index: usize,
    psfc_pa: f64,
    t2_k: f64,
    q2_mixing_ratio_kgkg: f64,
) -> Result<(), PtypeProductError> {
    for (field, value, valid) in [
        ("psfc_pa", psfc_pa, psfc_pa.is_finite() && psfc_pa > 0.0),
        ("t2_k", t2_k, t2_k.is_finite() && t2_k > 0.0),
        (
            "q2_mixing_ratio_kgkg",
            q2_mixing_ratio_kgkg,
            q2_mixing_ratio_kgkg.is_finite() && (0.0..1.0).contains(&q2_mixing_ratio_kgkg),
        ),
    ] {
        if !valid {
            return Err(PtypeProductError::InvalidValue {
                field,
                index,
                value,
            });
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SurfaceReplacementOptions {
    /// Reject a source grid point farther than this distance from the target.
    pub max_distance_km: f64,
}

impl Default for SurfaceReplacementOptions {
    fn default() -> Self {
        Self {
            max_distance_km: DEFAULT_SURFACE_REPLACEMENT_DISTANCE_KM,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceReplacementReport {
    pub target_cells: usize,
    pub replaced_cells: usize,
    pub outside_source_domain_cells: usize,
    pub too_far_cells: usize,
    pub invalid_source_cells: usize,
}

/// Replace only PSFC/T2/Q2 with a current gridded surface analysis.
///
/// A structured spatial bucket is built once for the analysis grid and reused
/// for every target cell. Targets outside the source footprint or farther than
/// `max_distance_km` retain their model surface values. The decoded Q2 field is
/// already mixing ratio and is copied without another humidity conversion.
pub fn replace_current_surface_from_analysis(
    columns: &mut PreparedPtypeColumns,
    analysis: &SurfaceFields,
    options: SurfaceReplacementOptions,
) -> Result<SurfaceReplacementReport, PtypeProductError> {
    columns.validate()?;
    if !options.max_distance_km.is_finite() || options.max_distance_km <= 0.0 {
        return Err(PtypeProductError::InvalidConfig(
            "surface replacement max distance must be finite and positive".to_string(),
        ));
    }
    let source_shape = GridShape::new(analysis.nx, analysis.ny)?;
    let source_len = source_shape.len();
    require_len("analysis latitude", analysis.lat.len(), source_len)?;
    require_len("analysis longitude", analysis.lon.len(), source_len)?;
    require_len("analysis psfc_pa", analysis.psfc_pa.len(), source_len)?;
    require_len("analysis t2_k", analysis.t2_k.len(), source_len)?;
    require_len("analysis q2_kgkg", analysis.q2_kgkg.len(), source_len)?;

    let locator = SurfaceAnalysisLocator::new(
        source_shape,
        &analysis.lat,
        &analysis.lon,
        options.max_distance_km,
    )?;
    let mut report = SurfaceReplacementReport {
        target_cells: columns.shape.len2d(),
        ..SurfaceReplacementReport::default()
    };
    for target_index in 0..columns.shape.len2d() {
        let target_lat = columns.grid.lat_deg[target_index] as f64;
        let target_lon = columns.grid.lon_deg[target_index] as f64;
        let source_index = match locator.locate(target_lat, target_lon) {
            SurfaceLocation::Found(index) => index,
            SurfaceLocation::Outside => {
                report.outside_source_domain_cells += 1;
                continue;
            }
            SurfaceLocation::TooFar => {
                report.too_far_cells += 1;
                continue;
            }
        };
        let psfc_pa = analysis.psfc_pa[source_index];
        let t2_k = analysis.t2_k[source_index];
        let q2 = analysis.q2_kgkg[source_index];
        if validate_surface_triplet(source_index, psfc_pa, t2_k, q2).is_err() {
            report.invalid_source_cells += 1;
            continue;
        }
        columns.psfc_pa[target_index] = psfc_pa;
        columns.t2_k[target_index] = t2_k;
        columns.q2_mixing_ratio_kgkg[target_index] = q2;
        report.replaced_cells += 1;
    }
    Ok(report)
}

const EARTH_RADIUS_KM: f64 = 6_371.008_8;

#[derive(Debug, Clone, Copy)]
struct LocalPoint {
    x_km: f64,
    y_km: f64,
}

#[derive(Debug, Clone, Copy)]
enum SurfaceLocation {
    Found(usize),
    Outside,
    TooFar,
}

struct SurfaceAnalysisLocator {
    reference_lat_deg: f64,
    reference_lon_deg: f64,
    cos_reference_lat: f64,
    bucket_size_km: f64,
    max_distance_km: f64,
    source_lat: Vec<f64>,
    source_lon: Vec<f64>,
    local_points: Vec<LocalPoint>,
    buckets: HashMap<(i32, i32), Vec<usize>>,
    footprint: Vec<LocalPoint>,
    x_min: f64,
    x_max: f64,
    y_min: f64,
    y_max: f64,
}

impl SurfaceAnalysisLocator {
    fn new(
        shape: GridShape,
        latitude: &[f64],
        longitude: &[f64],
        max_distance_km: f64,
    ) -> Result<Self, PtypeProductError> {
        let reference_lon_deg = longitude
            .first()
            .copied()
            .ok_or_else(|| PtypeProductError::InvalidConfig("empty analysis grid".into()))?;
        let mut latitude_sum = 0.0;
        for (index, (&lat, &lon)) in latitude.iter().zip(longitude.iter()).enumerate() {
            if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
                return Err(PtypeProductError::InvalidValue {
                    field: "analysis latitude",
                    index,
                    value: lat,
                });
            }
            if !lon.is_finite() {
                return Err(PtypeProductError::InvalidValue {
                    field: "analysis longitude",
                    index,
                    value: lon,
                });
            }
            latitude_sum += lat;
        }
        let reference_lat_deg = latitude_sum / latitude.len() as f64;
        let cos_reference_lat = reference_lat_deg.to_radians().cos().abs().max(1.0e-6);
        let bucket_size_km = max_distance_km.max(1.0);
        let to_local = |lat: f64, lon: f64| LocalPoint {
            x_km: EARTH_RADIUS_KM
                * wrapped_longitude_delta_deg(lon, reference_lon_deg).to_radians()
                * cos_reference_lat,
            y_km: EARTH_RADIUS_KM * (lat - reference_lat_deg).to_radians(),
        };
        let local_points = latitude
            .iter()
            .zip(longitude.iter())
            .map(|(&lat, &lon)| to_local(lat, lon))
            .collect::<Vec<_>>();
        let mut buckets = HashMap::<(i32, i32), Vec<usize>>::new();
        for (index, point) in local_points.iter().copied().enumerate() {
            buckets
                .entry(bucket_for(point, bucket_size_km))
                .or_default()
                .push(index);
        }
        let footprint_indices = perimeter_indices(shape);
        let footprint = footprint_indices
            .into_iter()
            .map(|index| local_points[index])
            .collect::<Vec<_>>();
        let (x_min, x_max, y_min, y_max) = local_points.iter().fold(
            (
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
            ),
            |(x_min, x_max, y_min, y_max), point| {
                (
                    x_min.min(point.x_km),
                    x_max.max(point.x_km),
                    y_min.min(point.y_km),
                    y_max.max(point.y_km),
                )
            },
        );
        Ok(Self {
            reference_lat_deg,
            reference_lon_deg,
            cos_reference_lat,
            bucket_size_km,
            max_distance_km,
            source_lat: latitude.to_vec(),
            source_lon: longitude.to_vec(),
            local_points,
            buckets,
            footprint,
            x_min,
            x_max,
            y_min,
            y_max,
        })
    }

    fn local_point(&self, lat_deg: f64, lon_deg: f64) -> Option<LocalPoint> {
        if !lat_deg.is_finite() || !(-90.0..=90.0).contains(&lat_deg) || !lon_deg.is_finite() {
            return None;
        }
        Some(LocalPoint {
            x_km: EARTH_RADIUS_KM
                * wrapped_longitude_delta_deg(lon_deg, self.reference_lon_deg).to_radians()
                * self.cos_reference_lat,
            y_km: EARTH_RADIUS_KM * (lat_deg - self.reference_lat_deg).to_radians(),
        })
    }

    fn locate(&self, lat_deg: f64, lon_deg: f64) -> SurfaceLocation {
        let Some(target) = self.local_point(lat_deg, lon_deg) else {
            return SurfaceLocation::Outside;
        };
        if !self.contains_target(target) {
            return SurfaceLocation::Outside;
        }
        let (bucket_x, bucket_y) = bucket_for(target, self.bucket_size_km);
        let search_radius = (self.max_distance_km / self.bucket_size_km).ceil() as i32 + 1;
        let mut nearest = None::<(usize, f64)>;
        for dy in -search_radius..=search_radius {
            for dx in -search_radius..=search_radius {
                let Some(indices) = self.buckets.get(&(bucket_x + dx, bucket_y + dy)) else {
                    continue;
                };
                for &source_index in indices {
                    let point = self.local_points[source_index];
                    let planar_distance =
                        (point.x_km - target.x_km).hypot(point.y_km - target.y_km);
                    if planar_distance > self.max_distance_km * 1.2 {
                        continue;
                    }
                    let distance = haversine_km(
                        lat_deg,
                        lon_deg,
                        self.source_lat[source_index],
                        self.source_lon[source_index],
                    );
                    if distance <= self.max_distance_km
                        && nearest.is_none_or(|(_, nearest_distance)| distance < nearest_distance)
                    {
                        nearest = Some((source_index, distance));
                    }
                }
            }
        }
        nearest
            .map(|(index, _)| SurfaceLocation::Found(index))
            .unwrap_or(SurfaceLocation::TooFar)
    }

    fn contains_target(&self, point: LocalPoint) -> bool {
        if self.footprint.len() >= 3 && polygon_area(&self.footprint).abs() > 1.0e-6 {
            point_in_polygon_or_boundary(point, &self.footprint)
        } else {
            point.x_km >= self.x_min
                && point.x_km <= self.x_max
                && point.y_km >= self.y_min
                && point.y_km <= self.y_max
        }
    }
}

fn perimeter_indices(shape: GridShape) -> Vec<usize> {
    if shape.nx == 1 || shape.ny == 1 {
        return (0..shape.len()).collect();
    }
    let mut indices = Vec::with_capacity(2 * shape.nx + 2 * shape.ny - 4);
    indices.extend(0..shape.nx);
    for y in 1..shape.ny {
        indices.push(y * shape.nx + shape.nx - 1);
    }
    for x in (0..shape.nx - 1).rev() {
        indices.push((shape.ny - 1) * shape.nx + x);
    }
    for y in (1..shape.ny - 1).rev() {
        indices.push(y * shape.nx);
    }
    indices
}

fn bucket_for(point: LocalPoint, bucket_size_km: f64) -> (i32, i32) {
    (
        (point.x_km / bucket_size_km).floor() as i32,
        (point.y_km / bucket_size_km).floor() as i32,
    )
}

fn wrapped_longitude_delta_deg(lon_deg: f64, reference_lon_deg: f64) -> f64 {
    (lon_deg - reference_lon_deg + 180.0).rem_euclid(360.0) - 180.0
}

fn haversine_km(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    let dlat = (lat_b - lat_a).to_radians();
    let dlon = wrapped_longitude_delta_deg(lon_b, lon_a).to_radians();
    let lat_a = lat_a.to_radians();
    let lat_b = lat_b.to_radians();
    let hav = (dlat * 0.5).sin().powi(2) + lat_a.cos() * lat_b.cos() * (dlon * 0.5).sin().powi(2);
    2.0 * EARTH_RADIUS_KM * hav.clamp(0.0, 1.0).sqrt().asin()
}

fn polygon_area(points: &[LocalPoint]) -> f64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| a.x_km * b.y_km - b.x_km * a.y_km)
        .sum::<f64>()
        * 0.5
}

fn point_in_polygon_or_boundary(point: LocalPoint, polygon: &[LocalPoint]) -> bool {
    let mut inside = false;
    for (a, b) in polygon
        .iter()
        .copied()
        .zip(polygon.iter().copied().cycle().skip(1))
        .take(polygon.len())
    {
        let cross =
            (point.x_km - a.x_km) * (b.y_km - a.y_km) - (point.y_km - a.y_km) * (b.x_km - a.x_km);
        let on_segment = cross.abs() <= 1.0e-6
            && point.x_km >= a.x_km.min(b.x_km) - 1.0e-6
            && point.x_km <= a.x_km.max(b.x_km) + 1.0e-6
            && point.y_km >= a.y_km.min(b.y_km) - 1.0e-6
            && point.y_km <= a.y_km.max(b.y_km) + 1.0e-6;
        if on_segment {
            return true;
        }
        if (a.y_km > point.y_km) != (b.y_km > point.y_km) {
            let x_cross = (b.x_km - a.x_km) * (point.y_km - a.y_km) / (b.y_km - a.y_km) + a.x_km;
            if point.x_km < x_cross {
                inside = !inside;
            }
        }
    }
    inside
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtypeRegridOptions {
    pub score_options: RegridOptions,
    pub qc_max_distance_km: Option<f64>,
    pub mixed_fraction_threshold: f64,
}

impl Default for PtypeRegridOptions {
    fn default() -> Self {
        Self {
            score_options: RegridOptions {
                method: RegridMethod::Bilinear,
                missing_policy: MissingPolicy::RenormalizeValid,
                extrapolate: false,
            },
            qc_max_distance_km: Some(DEFAULT_SURFACE_REPLACEMENT_DISTANCE_KM),
            mixed_fraction_threshold: PtypeOptions::default().mixed_fraction_threshold,
        }
    }
}

/// Regridded scores and display fields.
///
/// The four score planes are interpolated with one reusable plan. QC uses a
/// separate nearest-neighbor plan, and display type/confidence are recreated
/// from the regridded scores. No categorical value is ever interpolated.
#[derive(Debug, Clone, PartialEq)]
pub struct PtypeRegriddedFrame {
    pub analysis: PtypeAnalysisFrame,
    pub display: PtypeDisplayFields,
}

pub fn regrid_analysis_frame(
    frame: &PtypeAnalysisFrame,
    source_grid: &LatLonGrid,
    target_grid: &LatLonGrid,
    options: &PtypeRegridOptions,
) -> Result<PtypeRegriddedFrame, PtypeProductError> {
    validate_analysis_frame(frame)?;
    if source_grid.shape != frame.grid {
        return Err(PtypeProductError::InvalidConfig(
            "source grid shape does not match precipitation-type frame".to_string(),
        ));
    }
    source_grid.shape.checked_len()?;
    target_grid.shape.checked_len()?;
    if !options.mixed_fraction_threshold.is_finite()
        || !(0.0..=1.0).contains(&options.mixed_fraction_threshold)
    {
        return Err(PtypeProductError::InvalidConfig(
            "mixed fraction threshold must be finite and in 0..=1".to_string(),
        ));
    }

    let score_plan = RegridPlan::build(source_grid, target_grid, options.score_options.clone())?;
    let mut rain = score_plan.apply_f32(&frame.rain_powt_pct)?;
    let mut snow = score_plan.apply_f32(&frame.snow_powt_pct)?;
    let mut freezing_rain = score_plan.apply_f32(&frame.freezing_rain_powt_pct)?;
    let mut ice_pellets = score_plan.apply_f32(&frame.ice_pellets_powt_pct)?;
    for plane in [&mut rain, &mut snow, &mut freezing_rain, &mut ice_pellets] {
        for value in plane.iter_mut().filter(|value| value.is_finite()) {
            *value = value.clamp(0.0, 100.0);
        }
    }

    let qc_plan = RegridPlan::build(
        source_grid,
        target_grid,
        RegridOptions {
            method: RegridMethod::Nearest {
                max_distance_km: options.qc_max_distance_km,
            },
            missing_policy: MissingPolicy::Propagate,
            extrapolate: false,
        },
    )?;
    // Every u16 is exactly representable in f32. Missing/outside QC remains 0;
    // its score planes are NaN and therefore derive an Unknown display type.
    let source_qc = frame
        .qc_bits
        .iter()
        .map(|&bits| bits as f32)
        .collect::<Vec<_>>();
    let qc_bits = qc_plan
        .apply_f32(&source_qc)?
        .into_iter()
        .map(|value| {
            if value.is_finite() {
                value.round().clamp(0.0, u16::MAX as f32) as u16
            } else {
                0
            }
        })
        .collect::<Vec<_>>();
    let display = derive_display_fields_after_regrid(
        &rain,
        &snow,
        &freezing_rain,
        &ice_pellets,
        options.mixed_fraction_threshold,
    )?;
    Ok(PtypeRegriddedFrame {
        analysis: PtypeAnalysisFrame {
            grid: target_grid.shape,
            rain_powt_pct: rain,
            snow_powt_pct: snow,
            freezing_rain_powt_pct: freezing_rain,
            ice_pellets_powt_pct: ice_pellets,
            qc_bits,
            metadata: frame.metadata.clone(),
        },
        display,
    })
}

fn validate_analysis_frame(frame: &PtypeAnalysisFrame) -> Result<(), PtypeProductError> {
    let len = frame.grid.checked_len()?;
    require_len("rain PoWT", frame.rain_powt_pct.len(), len)?;
    require_len("snow PoWT", frame.snow_powt_pct.len(), len)?;
    require_len(
        "freezing-rain PoWT",
        frame.freezing_rain_powt_pct.len(),
        len,
    )?;
    require_len("ice-pellet PoWT", frame.ice_pellets_powt_pct.len(), len)?;
    require_len("ptype QC", frame.qc_bits.len(), len)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn metadata() -> LivePtypeMetadata {
        let cycle = Utc.with_ymd_and_hms(2026, 2, 1, 0, 0, 0).unwrap();
        LivePtypeMetadata::new("hrrr", cycle, cycle, 3_000.0, "model", cycle)
    }

    fn surface_fixture(nx: usize, ny: usize) -> SurfaceFields {
        let len = nx * ny;
        let mut lat = Vec::with_capacity(len);
        let mut lon = Vec::with_capacity(len);
        for y in 0..ny {
            for x in 0..nx {
                lat.push(y as f64);
                lon.push(x as f64);
            }
        }
        SurfaceFields {
            lat,
            lon,
            nx,
            ny,
            projection: None,
            psfc_pa: vec![100_000.0; len],
            orog_m: vec![250.0; len],
            orog_is_proxy: false,
            t2_k: vec![275.15; len],
            q2_kgkg: vec![0.01; len],
            u10_ms: vec![0.0; len],
            v10_ms: vec![0.0; len],
            native_sbcape_jkg: None,
            native_mlcape_jkg: None,
            native_mucape_jkg: None,
            native_pblh_m: None,
        }
    }

    fn pressure_fixture(nxy: usize) -> PressureFields {
        PressureFields {
            pressure_levels_hpa: vec![900.0, 700.0, 500.0],
            pressure_3d_pa: None,
            temperature_c_3d: [vec![-2.0; nxy], vec![-10.0; nxy], vec![-20.0; nxy]].concat(),
            qvapor_kgkg_3d: [vec![0.01; nxy], vec![0.004; nxy], vec![0.001; nxy]].concat(),
            u_ms_3d: vec![0.0; 3 * nxy],
            v_ms_3d: vec![0.0; 3 * nxy],
            gh_m_3d: [vec![1_000.0; nxy], vec![2_500.0; nxy], vec![5_000.0; nxy]].concat(),
            omega_pa_s_3d: None,
            absolute_vorticity_s_3d: None,
            cloud_liquid_kgkg_3d: None,
            cloud_ice_kgkg_3d: None,
            rain_kgkg_3d: None,
            snow_kgkg_3d: None,
            graupel_kgkg_3d: None,
        }
    }

    #[test]
    fn prepare_preserves_decoded_mixing_ratio_and_converts_msl_height_to_agl() {
        let surface = surface_fixture(1, 1);
        let pressure = pressure_fixture(1);
        let prepared = prepare_hrrr_rap_columns(&surface, &pressure).unwrap();
        assert_eq!(prepared.q2_mixing_ratio_kgkg, vec![0.01]);
        assert_eq!(prepared.mixing_ratio_kgkg[0], 0.01);
        assert_ne!(
            prepared.mixing_ratio_kgkg[0],
            rustwx_calc::mixing_ratio_from_specific_humidity(0.01)
        );
        assert_eq!(prepared.height_agl_m, vec![750.0, 2_250.0, 4_750.0]);
        assert_eq!(prepared.pressure_pa, vec![90_000.0, 70_000.0, 50_000.0]);
    }

    #[test]
    fn duplicate_zero_height_keeps_replaced_surface_and_sets_qc() {
        let grid = LatLonGrid::new(GridShape::new(1, 1).unwrap(), vec![40.0], vec![-90.0]).unwrap();
        let columns = prepare_wrf_columns(
            grid,
            vec![95000.0, 80000.0, 65000.0],
            vec![-5.0, -8.0, -18.0],
            vec![0.003, 0.002, 0.001],
            vec![0.0, 1_500.0, 3_000.0],
            vec![100_000.0],
            vec![278.15],
            vec![0.005],
        )
        .unwrap();
        let frame =
            analyze_prepared_columns(&columns, None, &PtypeOptions::default(), metadata()).unwrap();
        let qc = PtypeQc::from_bits(frame.qc_bits[0]);
        assert!(qc.contains(PtypeQc::DUPLICATE_HEIGHT_REMOVED));
        assert_eq!(frame.freezing_rain_powt_pct[0], 0.0);
    }

    #[test]
    fn current_surface_analysis_replaces_inside_only_without_double_conversion() {
        let target_grid = LatLonGrid::new(
            GridShape::new(2, 1).unwrap(),
            vec![0.1, 10.0],
            vec![0.1, 10.0],
        )
        .unwrap();
        let mut columns = prepare_wrf_columns(
            target_grid,
            vec![90_000.0],
            vec![-5.0, -5.0],
            vec![0.002, 0.002],
            vec![1_000.0, 1_000.0],
            vec![99_000.0, 99_000.0],
            vec![270.0, 270.0],
            vec![0.001, 0.001],
        )
        .unwrap();
        let mut analysis = surface_fixture(2, 2);
        analysis.psfc_pa = vec![100_100.0, 100_200.0, 100_300.0, 100_400.0];
        analysis.t2_k = vec![280.0, 281.0, 282.0, 283.0];
        analysis.q2_kgkg = vec![0.01, 0.02, 0.03, 0.04];
        let report = replace_current_surface_from_analysis(
            &mut columns,
            &analysis,
            SurfaceReplacementOptions {
                max_distance_km: 200.0,
            },
        )
        .unwrap();
        assert_eq!(report.target_cells, 2);
        assert_eq!(report.replaced_cells, 1);
        assert_eq!(report.outside_source_domain_cells, 1);
        assert_eq!(columns.psfc_pa, vec![100_100.0, 99_000.0]);
        assert_eq!(columns.q2_mixing_ratio_kgkg, vec![0.01, 0.001]);
    }

    #[test]
    fn regrid_derives_mixed_from_scores_and_never_interpolates_category() {
        let source_grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![0.0, 0.0, 1.0, 1.0],
            vec![0.0, 1.0, 0.0, 1.0],
        )
        .unwrap();
        let target_grid =
            LatLonGrid::new(GridShape::new(1, 1).unwrap(), vec![0.5], vec![0.5]).unwrap();
        let frame = PtypeAnalysisFrame {
            grid: source_grid.shape,
            rain_powt_pct: vec![100.0, 0.0, 0.0, 0.0],
            snow_powt_pct: vec![0.0, 100.0, 0.0, 0.0],
            freezing_rain_powt_pct: vec![0.0, 0.0, 100.0, 0.0],
            ice_pellets_powt_pct: vec![0.0, 0.0, 0.0, 100.0],
            qc_bits: vec![1, 2, 4, 8],
            metadata: metadata(),
        };
        let output = regrid_analysis_frame(
            &frame,
            &source_grid,
            &target_grid,
            &PtypeRegridOptions {
                qc_max_distance_km: Some(100.0),
                ..PtypeRegridOptions::default()
            },
        )
        .unwrap();
        assert_eq!(output.analysis.rain_powt_pct, vec![25.0]);
        assert_eq!(output.analysis.snow_powt_pct, vec![25.0]);
        assert_eq!(output.analysis.freezing_rain_powt_pct, vec![25.0]);
        assert_eq!(output.analysis.ice_pellets_powt_pct, vec![25.0]);
        assert_eq!(
            output.display.display_type_code,
            vec![PrecipType::Mixed.code()]
        );
        assert_eq!(output.display.confidence, vec![0.25]);
        assert!(matches!(output.analysis.qc_bits[0], 1 | 2 | 4 | 8));
    }

    #[test]
    fn thermodynamic_cache_key_has_no_radar_identity() {
        let metadata = metadata();
        let key = PtypeThermodynamicCacheKey::from_metadata(
            &metadata,
            "hrrr-conus-native",
            PtypePreparationOptions::default(),
        );
        let json = serde_json::to_value(&key).unwrap();
        let object = json.as_object().unwrap();
        assert!(!object.keys().any(|name| name.contains("radar")));
        assert_eq!(key.algorithm_version, CURRENT_PTYPE_ALGORITHM_VERSION);
    }
}

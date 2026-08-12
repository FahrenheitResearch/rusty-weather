//! Modified Bourgouin precipitation-type diagnostics.
//!
//! The implementation follows Birk et al. (2021), using a wet-bulb
//! temperature profile to calculate melting/refreezing energy and a
//! precipitation-generation-layer test to estimate the probability of ice
//! initiation. The four PoWT values are independent scores and therefore do
//! not necessarily sum to 100. Use [`PtypeScores::qpf_fractions`] when a
//! mutually exclusive split is required for QPF, rendering, or regridding.
//! The thermodynamic method cannot distinguish freezing drizzle from freezing
//! rain; supercooled liquid at a subfreezing surface is reported in the
//! `freezing_rain` field.

use rayon::prelude::*;

use crate::ecape::{VolumeShape, validate_len};
use crate::error::CalcError;

const GRAVITY_MS2: f64 = 9.80665;
const FREEZING_K: f64 = 273.15;
const ICE_RH_THRESHOLD_PCT: f64 = 75.0;
const GENERATION_LAYER_MIN_DEPTH_M: f64 = 1000.0;
const SUBLIMATION_LAYER_MIN_DEPTH_M: f64 = 1500.0;
const HEIGHT_EPSILON_M: f64 = 0.1;
const BELOW_GROUND_REJECT_M: f64 = -50.0;
const BELOW_GROUND_PRESSURE_TOLERANCE_PA: f64 = 100.0;
const MIXING_RATIO_NEGATIVE_TOLERANCE_KGKG: f64 = 1.0e-8;
const GRID_BLOCK_POINTS: usize = 8192;

/// Basic surface precipitation type, plus explicit mixed/quality states.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrecipType {
    NoPrecip = 0,
    Rain = 1,
    Snow = 2,
    FreezingRain = 3,
    IcePellets = 4,
    Mixed = 5,
    Unknown = 255,
}

impl PrecipType {
    pub const fn code(self) -> u8 {
        self as u8
    }
}

/// Pressure/temperature/dewpoint profile input for a single atmospheric column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeThermoLevel {
    pub pressure_hpa: f64,
    pub temperature_c: f64,
    pub dewpoint_c: f64,
    pub height_agl_m: f64,
}

/// Precomputed wet-bulb profile input.
///
/// This form is useful for verification fixtures and for callers that already
/// own wet-bulb temperature and relative humidity with respect to ice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeWetBulbLevel {
    pub temperature_c: f64,
    pub wet_bulb_c: f64,
    pub relative_humidity_ice_pct: f64,
    pub height_agl_m: f64,
}

/// Independent Modified Bourgouin probability-of-weather-type (PoWT) scores.
///
/// These are not a categorical probability distribution: two or more fields
/// may simultaneously equal 100. Normalize with [`Self::qpf_fractions`] only
/// when a mutually exclusive split is needed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeScores {
    pub rain_pct: f64,
    pub snow_pct: f64,
    pub freezing_rain_pct: f64,
    pub ice_pellets_pct: f64,
}

impl PtypeScores {
    pub const fn zero() -> Self {
        Self {
            rain_pct: 0.0,
            snow_pct: 0.0,
            freezing_rain_pct: 0.0,
            ice_pellets_pct: 0.0,
        }
    }

    pub const fn nan() -> Self {
        Self {
            rain_pct: f64::NAN,
            snow_pct: f64::NAN,
            freezing_rain_pct: f64::NAN,
            ice_pellets_pct: f64::NAN,
        }
    }

    pub fn sum(self) -> f64 {
        self.rain_pct + self.snow_pct + self.freezing_rain_pct + self.ice_pellets_pct
    }

    /// Convert independent PoWT scores to fractions that sum to one.
    ///
    /// This is the paper's QPF-splitting interpretation: for example, scores
    /// of 100 snow and 30 ice pellets become 100/130 and 30/130.
    pub fn qpf_fractions(self) -> PtypeFractions {
        let values = [
            self.rain_pct,
            self.snow_pct,
            self.freezing_rain_pct,
            self.ice_pellets_pct,
        ];
        if values.iter().any(|value| !value.is_finite() || *value < 0.0) {
            return PtypeFractions::nan();
        }
        let total = self.sum();
        if total <= 0.0 {
            return PtypeFractions::zero();
        }
        PtypeFractions {
            rain: self.rain_pct / total,
            snow: self.snow_pct / total,
            freezing_rain: self.freezing_rain_pct / total,
            ice_pellets: self.ice_pellets_pct / total,
        }
    }
}

/// Mutually exclusive fractions derived from [`PtypeScores`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeFractions {
    pub rain: f64,
    pub snow: f64,
    pub freezing_rain: f64,
    pub ice_pellets: f64,
}

impl PtypeFractions {
    pub const fn zero() -> Self {
        Self {
            rain: 0.0,
            snow: 0.0,
            freezing_rain: 0.0,
            ice_pellets: 0.0,
        }
    }

    pub const fn nan() -> Self {
        Self {
            rain: f64::NAN,
            snow: f64::NAN,
            freezing_rain: f64::NAN,
            ice_pellets: f64::NAN,
        }
    }

    pub fn confidence(self) -> f64 {
        [self.rain, self.snow, self.freezing_rain, self.ice_pellets]
            .into_iter()
            .fold(f64::NEG_INFINITY, f64::max)
    }

    /// Convert fractions to a display category.
    ///
    /// `mixed_fraction_threshold` is deliberately caller-controlled because
    /// it is a presentation decision, not part of the Modified Bourgouin
    /// equations. A value of 0.60 is a practical default.
    pub fn display_type(self, mixed_fraction_threshold: f64) -> PrecipType {
        let values = [
            (PrecipType::Rain, self.rain),
            (PrecipType::Snow, self.snow),
            (PrecipType::FreezingRain, self.freezing_rain),
            (PrecipType::IcePellets, self.ice_pellets),
        ];
        if values.iter().any(|(_, value)| !value.is_finite()) {
            return PrecipType::Unknown;
        }
        let mut top = values[0];
        for candidate in values.into_iter().skip(1) {
            if candidate.1 > top.1 {
                top = candidate;
            }
        }
        if top.1 <= 0.0 {
            PrecipType::Unknown
        } else if top.1 < mixed_fraction_threshold {
            PrecipType::Mixed
        } else {
            top.0
        }
    }
}

/// Bitwise quality-control metadata for a point or grid cell.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PtypeQc(u16);

impl PtypeQc {
    pub const ACTIVE_MASK_OFF: Self = Self(1 << 0);
    pub const INVALID_INPUT_LEVEL_REMOVED: Self = Self(1 << 1);
    pub const WET_BULB_FAILURE: Self = Self(1 << 2);
    pub const HEIGHTS_REORDERED: Self = Self(1 << 3);
    pub const DUPLICATE_HEIGHT_REMOVED: Self = Self(1 << 4);
    pub const NEGATIVE_HEIGHT_CLAMPED: Self = Self(1 << 5);
    pub const INSUFFICIENT_PROFILE: Self = Self(1 << 6);
    pub const SURFACE_LEVEL_MISSING: Self = Self(1 << 7);
    pub const NO_PRECIP_GENERATION_LAYER: Self = Self(1 << 8);
    pub const UPPER_GENERATION_LAYER_REMOVED: Self = Self(1 << 9);
    pub const ZERO_TOTAL_SCORE: Self = Self(1 << 10);
    pub const BELOW_GROUND_LEVEL_REMOVED: Self = Self(1 << 11);

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, flag: Self) -> bool {
        self.0 & flag.0 == flag.0
    }

    pub const fn is_clean(self) -> bool {
        self.0 == 0
    }

    fn insert(&mut self, flag: Self) {
        self.0 |= flag.0;
    }
}

/// Debug/verification quantities produced by the classifier.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeDiagnostics {
    pub surface_wet_bulb_c: f64,
    pub melting_energy_total_jkg: f64,
    pub melting_energy_aloft_jkg: f64,
    pub refreezing_energy_jkg: f64,
    pub probability_ice_pct: f64,
    pub generation_layer_min_temperature_c: f64,
    pub generation_layer_bottom_agl_m: f64,
    pub generation_layer_top_agl_m: f64,
}

impl PtypeDiagnostics {
    const fn nan() -> Self {
        Self {
            surface_wet_bulb_c: f64::NAN,
            melting_energy_total_jkg: f64::NAN,
            melting_energy_aloft_jkg: f64::NAN,
            refreezing_energy_jkg: f64::NAN,
            probability_ice_pct: f64::NAN,
            generation_layer_min_temperature_c: f64::NAN,
            generation_layer_bottom_agl_m: f64::NAN,
            generation_layer_top_agl_m: f64::NAN,
        }
    }
}

/// Complete result for one vertical profile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypePointResult {
    pub scores: PtypeScores,
    pub qpf_fractions: PtypeFractions,
    pub display_type: PrecipType,
    /// Largest normalized fraction in the range 0-1.
    pub confidence: f64,
    pub diagnostics: PtypeDiagnostics,
    pub qc: PtypeQc,
}

/// Configuration shared by point and grid APIs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PtypeOptions {
    /// A display cell is marked mixed when no normalized fraction reaches this
    /// threshold. This does not change the four scientific PoWT fields.
    pub mixed_fraction_threshold: f64,
    /// Retain energy and ice-generation diagnostics in the grid output.
    pub include_diagnostics: bool,
}

impl Default for PtypeOptions {
    fn default() -> Self {
        Self {
            mixed_fraction_threshold: 0.60,
            include_diagnostics: false,
        }
    }
}

/// Model-grid input. Three-dimensional arrays use `[k][y][x]` ordering.
///
/// `pressure_3d_pa` may be either one pressure value per vertical level (`nz`)
/// or a full three-dimensional field (`nx * ny * nz`). `qvapor_3d_kgkg` and
/// `q2_kgkg` are water-vapor mixing ratios (kg kg-1), matching WRF's QVAPOR/Q2
/// convention rather than specific humidity. `active_mask`, when present,
/// should be nonzero only where precipitation is occurring or forecast.
/// Radar/MRMS belongs in this mask; it should not directly replace the
/// thermodynamic phase calculation.
#[derive(Debug, Clone, Copy)]
pub struct PtypeGridInputs<'a> {
    pub shape: VolumeShape,
    pub pressure_3d_pa: &'a [f64],
    pub temperature_3d_c: &'a [f64],
    pub qvapor_3d_kgkg: &'a [f64],
    pub height_agl_3d_m: &'a [f64],
    pub psfc_pa: &'a [f64],
    pub t2_k: &'a [f64],
    pub q2_kgkg: &'a [f64],
    pub active_mask: Option<&'a [u8]>,
}

/// Compact fields intended for storage/regridding/rendering.
#[derive(Debug, Clone, PartialEq)]
pub struct PtypeGridFields {
    pub rain_powt_pct: Vec<f32>,
    pub snow_powt_pct: Vec<f32>,
    pub freezing_rain_powt_pct: Vec<f32>,
    pub ice_pellets_powt_pct: Vec<f32>,
    pub display_type_code: Vec<u8>,
    pub confidence: Vec<f32>,
    pub qc_bits: Vec<u16>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtypeGridDiagnostics {
    pub surface_wet_bulb_c: Vec<f32>,
    pub melting_energy_total_jkg: Vec<f32>,
    pub melting_energy_aloft_jkg: Vec<f32>,
    pub refreezing_energy_jkg: Vec<f32>,
    pub probability_ice_pct: Vec<f32>,
    pub generation_layer_min_temperature_c: Vec<f32>,
    pub generation_layer_bottom_agl_m: Vec<f32>,
    pub generation_layer_top_agl_m: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PtypeGridOutput {
    pub fields: PtypeGridFields,
    pub diagnostics: Option<PtypeGridDiagnostics>,
}

/// Classify a temperature/dewpoint profile with the Modified Bourgouin method.
pub fn classify_modified_bourgouin_profile(
    levels: &[PtypeThermoLevel],
    precipitating: bool,
    options: &PtypeOptions,
) -> Result<PtypePointResult, CalcError> {
    validate_options(options)?;
    let mut scratch = ColumnScratch::with_capacity(levels.len());
    let mut qc = PtypeQc::default();
    for level in levels {
        append_thermo_level(&mut scratch.levels, *level, &mut qc);
    }
    Ok(classify_computed_profile(
        &mut scratch,
        precipitating,
        options.mixed_fraction_threshold,
        qc,
    ))
}

/// Classify a precomputed wet-bulb/RH-ice profile.
pub fn classify_modified_bourgouin_wet_bulb_profile(
    levels: &[PtypeWetBulbLevel],
    precipitating: bool,
    options: &PtypeOptions,
) -> Result<PtypePointResult, CalcError> {
    validate_options(options)?;
    let mut scratch = ColumnScratch::with_capacity(levels.len());
    let mut qc = PtypeQc::default();
    for level in levels {
        append_wet_bulb_level(&mut scratch.levels, *level, &mut qc);
    }
    Ok(classify_computed_profile(
        &mut scratch,
        precipitating,
        options.mixed_fraction_threshold,
        qc,
    ))
}

/// Compute Modified Bourgouin PoWT fields for every horizontal grid point.
pub fn compute_modified_bourgouin_ptype(
    inputs: PtypeGridInputs<'_>,
    options: &PtypeOptions,
) -> Result<PtypeGridOutput, CalcError> {
    validate_options(options)?;
    validate_grid_inputs(inputs)?;

    let nxy = inputs.shape.len2d();
    let nz = inputs.shape.nz;
    let pressure_is_levels = inputs.pressure_3d_pa.len() == nz;
    let block_count = nxy.div_ceil(GRID_BLOCK_POINTS);

    // Build compact SoA blocks directly instead of collecting a large
    // Vec<PtypePointResult>. This keeps the high-resolution path from carrying
    // a second full grid of f64 diagnostics when the caller only wants fields.
    // Indexed parallel collection preserves block order, so the final append
    // remains in native [y][x] order.
    let blocks: Vec<PtypeGridOutput> = (0..block_count)
        .into_par_iter()
        .map_init(
            || ColumnScratch::with_capacity(nz + 1),
            |scratch, block_index| {
                let start = block_index * GRID_BLOCK_POINTS;
                let end = (start + GRID_BLOCK_POINTS).min(nxy);
                let mut output = empty_grid_output(end - start, options.include_diagnostics);

                for ij in start..end {
                    scratch.clear();
                    let point = classify_grid_point(
                        inputs,
                        ij,
                        pressure_is_levels,
                        scratch,
                        options.mixed_fraction_threshold,
                    );
                    push_grid_point(&mut output, point);
                }
                output
            },
        )
        .collect();

    Ok(merge_grid_blocks(blocks, options.include_diagnostics))
}

fn classify_grid_point(
    inputs: PtypeGridInputs<'_>,
    ij: usize,
    pressure_is_levels: bool,
    scratch: &mut ColumnScratch,
    mixed_fraction_threshold: f64,
) -> PtypePointResult {
    let active = inputs.active_mask.is_none_or(|mask| mask[ij] != 0);
    if !active {
        let mut qc = PtypeQc::default();
        qc.insert(PtypeQc::ACTIVE_MASK_OFF);
        return no_precip_result(qc);
    }

    let mut qc = PtypeQc::default();
    let surface_pressure_hpa = inputs.psfc_pa[ij] / 100.0;
    append_thermo_level(
        &mut scratch.levels,
        PtypeThermoLevel {
            pressure_hpa: surface_pressure_hpa,
            temperature_c: inputs.t2_k[ij] - FREEZING_K,
            dewpoint_c: dewpoint_from_mixing_ratio(surface_pressure_hpa, inputs.q2_kgkg[ij]),
            height_agl_m: 0.0,
        },
        &mut qc,
    );

    let nxy = inputs.shape.len2d();
    for k in 0..inputs.shape.nz {
        let idx = k * nxy + ij;
        let pressure_pa = if pressure_is_levels {
            inputs.pressure_3d_pa[k]
        } else {
            inputs.pressure_3d_pa[idx]
        };
        if pressure_pa
            > inputs.psfc_pa[ij] + BELOW_GROUND_PRESSURE_TOLERANCE_PA
        {
            qc.insert(PtypeQc::BELOW_GROUND_LEVEL_REMOVED);
            continue;
        }
        let pressure_hpa = pressure_pa / 100.0;
        append_thermo_level(
            &mut scratch.levels,
            PtypeThermoLevel {
                pressure_hpa,
                temperature_c: inputs.temperature_3d_c[idx],
                dewpoint_c: dewpoint_from_mixing_ratio(
                    pressure_hpa,
                    inputs.qvapor_3d_kgkg[idx],
                ),
                height_agl_m: inputs.height_agl_3d_m[idx],
            },
            &mut qc,
        );
    }

    classify_computed_profile(scratch, true, mixed_fraction_threshold, qc)
}

fn validate_options(options: &PtypeOptions) -> Result<(), CalcError> {
    if !options.mixed_fraction_threshold.is_finite()
        || !(0.0..=1.0).contains(&options.mixed_fraction_threshold)
    {
        return Err(CalcError::InvalidConfig {
            field: "mixed_fraction_threshold",
            reason: "must be finite and in the range 0-1",
        });
    }
    Ok(())
}

fn validate_grid_inputs(inputs: PtypeGridInputs<'_>) -> Result<(), CalcError> {
    let nxy = inputs.shape.len2d();
    let n3d = inputs.shape.len3d();
    if inputs.pressure_3d_pa.len() != inputs.shape.nz {
        validate_len("pressure_3d_pa", inputs.pressure_3d_pa.len(), n3d)?;
    }
    validate_len(
        "temperature_3d_c",
        inputs.temperature_3d_c.len(),
        n3d,
    )?;
    validate_len(
        "qvapor_3d_kgkg",
        inputs.qvapor_3d_kgkg.len(),
        n3d,
    )?;
    validate_len(
        "height_agl_3d_m",
        inputs.height_agl_3d_m.len(),
        n3d,
    )?;
    validate_len("psfc_pa", inputs.psfc_pa.len(), nxy)?;
    validate_len("t2_k", inputs.t2_k.len(), nxy)?;
    validate_len("q2_kgkg", inputs.q2_kgkg.len(), nxy)?;
    if let Some(mask) = inputs.active_mask {
        validate_len("active_mask", mask.len(), nxy)?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct ComputedLevel {
    temperature_c: f64,
    wet_bulb_c: f64,
    relative_humidity_ice_pct: f64,
    height_agl_m: f64,
}

#[derive(Debug, Clone, Copy)]
struct ThermalLayer {
    warm: bool,
    energy_jkg: f64,
}

#[derive(Debug, Clone, Copy)]
struct ThresholdNode {
    temperature_c: f64,
    relative_humidity_ice_pct: f64,
    height_agl_m: f64,
}

#[derive(Debug, Clone, Copy)]
struct LayerRun {
    bottom_agl_m: f64,
    top_agl_m: f64,
    min_temperature_c: f64,
}

impl LayerRun {
    fn depth_m(self) -> f64 {
        (self.top_agl_m - self.bottom_agl_m).max(0.0)
    }
}

struct ColumnScratch {
    levels: Vec<ComputedLevel>,
    thermal_layers: Vec<ThermalLayer>,
    threshold_nodes: Vec<ThresholdNode>,
    moist_runs: Vec<LayerRun>,
    dry_runs: Vec<LayerRun>,
}

impl ColumnScratch {
    fn with_capacity(levels: usize) -> Self {
        Self {
            levels: Vec::with_capacity(levels),
            thermal_layers: Vec::with_capacity(6),
            threshold_nodes: Vec::with_capacity(levels * 2),
            moist_runs: Vec::with_capacity(4),
            dry_runs: Vec::with_capacity(4),
        }
    }

    fn clear(&mut self) {
        self.levels.clear();
        self.thermal_layers.clear();
        self.threshold_nodes.clear();
        self.moist_runs.clear();
        self.dry_runs.clear();
    }
}

fn append_thermo_level(
    output: &mut Vec<ComputedLevel>,
    level: PtypeThermoLevel,
    qc: &mut PtypeQc,
) {
    if !level.pressure_hpa.is_finite()
        || level.pressure_hpa <= 0.0
        || !level.temperature_c.is_finite()
        || !level.dewpoint_c.is_finite()
        || !level.height_agl_m.is_finite()
    {
        qc.insert(PtypeQc::INVALID_INPUT_LEVEL_REMOVED);
        return;
    }

    let wet_bulb_c = metrust::calc::wet_bulb_temperature(
        level.pressure_hpa,
        level.temperature_c,
        level.dewpoint_c,
    );
    let relative_humidity_ice_pct = relative_humidity_ice_pct(
        level.temperature_c,
        level.dewpoint_c,
    );
    if !wet_bulb_c.is_finite() || !relative_humidity_ice_pct.is_finite() {
        qc.insert(PtypeQc::WET_BULB_FAILURE);
        return;
    }

    let mut height_agl_m = level.height_agl_m;
    if height_agl_m < BELOW_GROUND_REJECT_M {
        qc.insert(PtypeQc::BELOW_GROUND_LEVEL_REMOVED);
        return;
    }
    if height_agl_m < 0.0 {
        height_agl_m = 0.0;
        qc.insert(PtypeQc::NEGATIVE_HEIGHT_CLAMPED);
    }
    output.push(ComputedLevel {
        temperature_c: level.temperature_c,
        wet_bulb_c,
        relative_humidity_ice_pct,
        height_agl_m,
    });
}

fn append_wet_bulb_level(
    output: &mut Vec<ComputedLevel>,
    level: PtypeWetBulbLevel,
    qc: &mut PtypeQc,
) {
    if !level.temperature_c.is_finite()
        || !level.wet_bulb_c.is_finite()
        || !level.relative_humidity_ice_pct.is_finite()
        || !level.height_agl_m.is_finite()
    {
        qc.insert(PtypeQc::INVALID_INPUT_LEVEL_REMOVED);
        return;
    }
    let mut height_agl_m = level.height_agl_m;
    if height_agl_m < BELOW_GROUND_REJECT_M {
        qc.insert(PtypeQc::BELOW_GROUND_LEVEL_REMOVED);
        return;
    }
    if height_agl_m < 0.0 {
        height_agl_m = 0.0;
        qc.insert(PtypeQc::NEGATIVE_HEIGHT_CLAMPED);
    }
    output.push(ComputedLevel {
        temperature_c: level.temperature_c,
        wet_bulb_c: level.wet_bulb_c,
        relative_humidity_ice_pct: level.relative_humidity_ice_pct,
        height_agl_m,
    });
}

fn classify_computed_profile(
    scratch: &mut ColumnScratch,
    precipitating: bool,
    mixed_fraction_threshold: f64,
    mut qc: PtypeQc,
) -> PtypePointResult {
    if !precipitating {
        qc.insert(PtypeQc::ACTIVE_MASK_OFF);
        return no_precip_result(qc);
    }

    normalize_profile(&mut scratch.levels, &mut qc);
    if scratch.levels.len() < 3 {
        qc.insert(PtypeQc::INSUFFICIENT_PROFILE);
        return unknown_result(qc);
    }
    if scratch.levels[0].height_agl_m > 50.0 {
        qc.insert(PtypeQc::SURFACE_LEVEL_MISSING);
    }

    let energies = calculate_energies(&scratch.levels, &mut scratch.thermal_layers);
    let ice = calculate_probability_ice(scratch, &mut qc);

    let snow_initial = clamp_pct(
        1540.0 * (-0.29 * energies.melting_energy_total_jkg).exp(),
    );
    let snow_pct = clamp_pct((ice.probability_pct / 100.0) * snow_initial);

    let ice_pellets_initial = if energies.melting_energy_aloft_jkg > 0.0 {
        clamp_pct(
            2.3 * energies.refreezing_energy_jkg
                - 42.0 * (energies.melting_energy_aloft_jkg + 1.0).ln()
                + 3.0,
        )
    } else {
        0.0
    };
    let ice_pellets_pct =
        clamp_pct((ice.probability_pct / 100.0) * ice_pellets_initial);

    // The liquid equation uses the column melting energy. The separate
    // ``melting_energy_aloft`` diagnostic is the elevated warm-layer METw
    // paired with RETw and is used only by the PL equation.
    let mut liquid_initial = clamp_pct(
        -2.1 * energies.refreezing_energy_jkg
            + 0.2 * energies.melting_energy_total_jkg
            + 458.0,
    );
    if energies.melting_energy_total_jkg < 5.0 {
        liquid_initial *= 0.2 * energies.melting_energy_total_jkg;
    }
    let liquid_pct = clamp_pct(
        (100.0 - ice.probability_pct)
            + (ice.probability_pct / 100.0) * liquid_initial,
    );

    let (rain_pct, freezing_rain_pct) = if energies.surface_wet_bulb_c > 0.0 {
        (liquid_pct, 0.0)
    } else {
        (0.0, liquid_pct)
    };
    let scores = PtypeScores {
        rain_pct,
        snow_pct,
        freezing_rain_pct,
        ice_pellets_pct,
    };
    let qpf_fractions = scores.qpf_fractions();
    if scores.sum() <= 0.0 {
        qc.insert(PtypeQc::ZERO_TOTAL_SCORE);
    }
    let display_type = qpf_fractions.display_type(mixed_fraction_threshold);
    let confidence = qpf_fractions.confidence();

    PtypePointResult {
        scores,
        qpf_fractions,
        display_type,
        confidence,
        diagnostics: PtypeDiagnostics {
            surface_wet_bulb_c: energies.surface_wet_bulb_c,
            melting_energy_total_jkg: energies.melting_energy_total_jkg,
            melting_energy_aloft_jkg: energies.melting_energy_aloft_jkg,
            refreezing_energy_jkg: energies.refreezing_energy_jkg,
            probability_ice_pct: ice.probability_pct,
            generation_layer_min_temperature_c: ice.min_temperature_c,
            generation_layer_bottom_agl_m: ice.bottom_agl_m,
            generation_layer_top_agl_m: ice.top_agl_m,
        },
        qc,
    }
}

fn normalize_profile(levels: &mut Vec<ComputedLevel>, qc: &mut PtypeQc) {
    let reordered = levels
        .windows(2)
        .any(|pair| pair[1].height_agl_m < pair[0].height_agl_m);
    if reordered {
        levels.sort_by(|a, b| a.height_agl_m.total_cmp(&b.height_agl_m));
        qc.insert(PtypeQc::HEIGHTS_REORDERED);
    }

    let mut write = 0usize;
    for read in 0..levels.len() {
        let level = levels[read];
        if write > 0
            && (level.height_agl_m - levels[write - 1].height_agl_m).abs()
                <= HEIGHT_EPSILON_M
        {
            qc.insert(PtypeQc::DUPLICATE_HEIGHT_REMOVED);
            // Keep the first value at an exactly duplicated height. The grid
            // API deliberately prepends the caller's 2-m/surface analysis, so
            // replacing it with a model level at 0 m would silently discard the
            // live surface correction that matters most near the rain/FZRA line.
        } else {
            levels[write] = level;
            write += 1;
        }
    }
    levels.truncate(write);
}

#[derive(Debug, Clone, Copy)]
struct EnergySummary {
    surface_wet_bulb_c: f64,
    melting_energy_total_jkg: f64,
    melting_energy_aloft_jkg: f64,
    refreezing_energy_jkg: f64,
}

fn calculate_energies(
    levels: &[ComputedLevel],
    thermal_layers: &mut Vec<ThermalLayer>,
) -> EnergySummary {
    thermal_layers.clear();

    for pair in levels.windows(2) {
        let lower = pair[0];
        let upper = pair[1];
        let dz = upper.height_agl_m - lower.height_agl_m;
        if dz <= 0.0 {
            continue;
        }
        let tw0 = lower.wet_bulb_c;
        let tw1 = upper.wet_bulb_c;

        if tw0 * tw1 < 0.0 {
            // Split the trapezoid exactly at the linearly interpolated 0 C
            // crossing so warm and cold areas never cancel one another.
            let fraction = -tw0 / (tw1 - tw0);
            let crossing_height = lower.height_agl_m + fraction * dz;
            append_thermal_energy(
                thermal_layers,
                layer_energy(lower.height_agl_m, crossing_height, tw0, 0.0),
            );
            append_thermal_energy(
                thermal_layers,
                layer_energy(crossing_height, upper.height_agl_m, 0.0, tw1),
            );
        } else {
            append_thermal_energy(
                thermal_layers,
                layer_energy(lower.height_agl_m, upper.height_agl_m, tw0, tw1),
            );
        }
    }

    let melting_energy_total_jkg = thermal_layers
        .iter()
        .filter(|layer| layer.warm)
        .map(|layer| layer.energy_jkg)
        .sum();

    // The PL equation pairs an elevated warm layer with the near-surface cold
    // layer directly beneath it. The near-surface cold layer is either the
    // first thermal layer (cold surface) or the second layer (an additional
    // shallow surface melt, Birk et al. Fig. 1d). Restricting the pair this way
    // prevents an unrelated upper-level warm/cold couplet from replacing the
    // layer through which precipitation must actually fall near the ground.
    let near_surface_cold_index = match thermal_layers.first() {
        Some(first) if !first.warm => Some(0),
        Some(_) if thermal_layers.get(1).is_some_and(|layer| !layer.warm) => Some(1),
        _ => None,
    };
    let (melting_energy_aloft_jkg, refreezing_energy_jkg) = near_surface_cold_index
        .and_then(|cold_index| {
            thermal_layers
                .get(cold_index + 1)
                .filter(|layer| layer.warm)
                .map(|warm| (warm.energy_jkg, thermal_layers[cold_index].energy_jkg))
        })
        .unwrap_or((0.0, 0.0));

    EnergySummary {
        surface_wet_bulb_c: levels[0].wet_bulb_c,
        melting_energy_total_jkg,
        melting_energy_aloft_jkg,
        refreezing_energy_jkg,
    }
}

fn append_thermal_energy(layers: &mut Vec<ThermalLayer>, signed_energy_jkg: f64) {
    if !signed_energy_jkg.is_finite() || signed_energy_jkg == 0.0 {
        return;
    }
    let warm = signed_energy_jkg > 0.0;
    let magnitude = signed_energy_jkg.abs();
    if let Some(last) = layers.last_mut() {
        if last.warm == warm {
            last.energy_jkg += magnitude;
            return;
        }
    }
    layers.push(ThermalLayer {
        warm,
        energy_jkg: magnitude,
    });
}

fn layer_energy(z0_m: f64, z1_m: f64, tw0_c: f64, tw1_c: f64) -> f64 {
    GRAVITY_MS2 / FREEZING_K * 0.5 * (tw0_c + tw1_c) * (z1_m - z0_m)
}

#[derive(Debug, Clone, Copy)]
struct IceSummary {
    probability_pct: f64,
    min_temperature_c: f64,
    bottom_agl_m: f64,
    top_agl_m: f64,
}

fn calculate_probability_ice(scratch: &mut ColumnScratch, qc: &mut PtypeQc) -> IceSummary {
    build_humidity_runs(
        &scratch.levels,
        true,
        &mut scratch.threshold_nodes,
        &mut scratch.moist_runs,
    );
    build_humidity_runs(
        &scratch.levels,
        false,
        &mut scratch.threshold_nodes,
        &mut scratch.dry_runs,
    );

    let mut selected: Option<LayerRun> = None;
    let mut candidate_seen = false;
    for generation in scratch
        .moist_runs
        .iter()
        .copied()
        .filter(|run| run.depth_m() > GENERATION_LAYER_MIN_DEPTH_M)
    {
        candidate_seen = true;
        let blocked = scratch.dry_runs.iter().copied().any(|dry| {
            dry.depth_m() > SUBLIMATION_LAYER_MIN_DEPTH_M
                && dry.top_agl_m <= generation.bottom_agl_m + HEIGHT_EPSILON_M
        });
        if blocked {
            qc.insert(PtypeQc::UPPER_GENERATION_LAYER_REMOVED);
            continue;
        }
        match selected {
            Some(current) if current.min_temperature_c <= generation.min_temperature_c => {}
            _ => selected = Some(generation),
        }
    }

    let Some(generation) = selected else {
        if !candidate_seen || qc.contains(PtypeQc::UPPER_GENERATION_LAYER_REMOVED) {
            qc.insert(PtypeQc::NO_PRECIP_GENERATION_LAYER);
        }
        return IceSummary {
            probability_pct: 0.0,
            min_temperature_c: f64::NAN,
            bottom_agl_m: f64::NAN,
            top_agl_m: f64::NAN,
        };
    };

    IceSummary {
        probability_pct: probability_ice_from_temperature(generation.min_temperature_c),
        min_temperature_c: generation.min_temperature_c,
        bottom_agl_m: generation.bottom_agl_m,
        top_agl_m: generation.top_agl_m,
    }
}

fn build_humidity_runs(
    levels: &[ComputedLevel],
    moist: bool,
    nodes: &mut Vec<ThresholdNode>,
    runs: &mut Vec<LayerRun>,
) {
    nodes.clear();
    runs.clear();
    if levels.len() < 2 {
        return;
    }

    nodes.push(ThresholdNode {
        temperature_c: levels[0].temperature_c,
        relative_humidity_ice_pct: levels[0].relative_humidity_ice_pct,
        height_agl_m: levels[0].height_agl_m,
    });
    for pair in levels.windows(2) {
        let lower = pair[0];
        let upper = pair[1];
        let d0 = lower.relative_humidity_ice_pct - ICE_RH_THRESHOLD_PCT;
        let d1 = upper.relative_humidity_ice_pct - ICE_RH_THRESHOLD_PCT;
        if d0 * d1 < 0.0 {
            let fraction = -d0 / (d1 - d0);
            nodes.push(ThresholdNode {
                temperature_c: lower.temperature_c
                    + fraction * (upper.temperature_c - lower.temperature_c),
                relative_humidity_ice_pct: ICE_RH_THRESHOLD_PCT,
                height_agl_m: lower.height_agl_m
                    + fraction * (upper.height_agl_m - lower.height_agl_m),
            });
        }
        nodes.push(ThresholdNode {
            temperature_c: upper.temperature_c,
            relative_humidity_ice_pct: upper.relative_humidity_ice_pct,
            height_agl_m: upper.height_agl_m,
        });
    }

    let mut current: Option<LayerRun> = None;
    for pair in nodes.windows(2) {
        let lower = pair[0];
        let upper = pair[1];
        if upper.height_agl_m - lower.height_agl_m <= HEIGHT_EPSILON_M {
            continue;
        }
        let midpoint_rh =
            0.5 * (lower.relative_humidity_ice_pct + upper.relative_humidity_ice_pct);
        let qualifies = if moist {
            midpoint_rh > ICE_RH_THRESHOLD_PCT
        } else {
            midpoint_rh < ICE_RH_THRESHOLD_PCT
        };

        if qualifies {
            let segment_min_temperature = lower.temperature_c.min(upper.temperature_c);
            if let Some(run) = &mut current {
                if (run.top_agl_m - lower.height_agl_m).abs() <= HEIGHT_EPSILON_M {
                    run.top_agl_m = upper.height_agl_m;
                    run.min_temperature_c =
                        run.min_temperature_c.min(segment_min_temperature);
                } else {
                    runs.push(*run);
                    *run = LayerRun {
                        bottom_agl_m: lower.height_agl_m,
                        top_agl_m: upper.height_agl_m,
                        min_temperature_c: segment_min_temperature,
                    };
                }
            } else {
                current = Some(LayerRun {
                    bottom_agl_m: lower.height_agl_m,
                    top_agl_m: upper.height_agl_m,
                    min_temperature_c: segment_min_temperature,
                });
            }
        } else if let Some(run) = current.take() {
            runs.push(run);
        }
    }
    if let Some(run) = current {
        runs.push(run);
    }
}

/// Convert specific humidity to water-vapor mixing ratio.
///
/// HRRR/RAP pressure-level GRIB fields are commonly exposed as specific
/// humidity (`SPFH`), while [`PtypeGridInputs`] follows WRF's mixing-ratio
/// convention. Invalid values return NaN so the normal profile QC path removes
/// the affected level instead of silently treating it as dry.
pub fn mixing_ratio_from_specific_humidity(specific_humidity_kgkg: f64) -> f64 {
    if !specific_humidity_kgkg.is_finite()
        || specific_humidity_kgkg < 0.0
        || specific_humidity_kgkg >= 1.0
    {
        return f64::NAN;
    }
    specific_humidity_kgkg / (1.0 - specific_humidity_kgkg)
}

/// Ice-presence probability from the minimum environmental temperature in the
/// precipitation-generation layer (Birk et al. 2021, Eq. 2).
pub fn probability_ice_from_temperature(temperature_c: f64) -> f64 {
    if !temperature_c.is_finite() {
        return f64::NAN;
    }
    if temperature_c <= -15.0 {
        100.0
    } else if temperature_c >= -7.0 {
        0.0
    } else {
        clamp_pct(
            -0.065 * temperature_c.powi(4)
                - 3.1544 * temperature_c.powi(3)
                - 56.414 * temperature_c.powi(2)
                - 449.6 * temperature_c
                - 1308.0,
        )
    }
}

fn relative_humidity_ice_pct(temperature_c: f64, dewpoint_c: f64) -> f64 {
    let vapor_pressure_hpa = saturation_vapor_pressure_water_hpa(dewpoint_c);
    let saturation_ice_hpa = saturation_vapor_pressure_ice_hpa(temperature_c);
    100.0 * vapor_pressure_hpa / saturation_ice_hpa
}

fn saturation_vapor_pressure_water_hpa(temperature_c: f64) -> f64 {
    6.1121
        * ((18.678 - temperature_c / 234.5) * temperature_c / (257.14 + temperature_c))
            .exp()
}

fn saturation_vapor_pressure_ice_hpa(temperature_c: f64) -> f64 {
    6.1115
        * ((23.036 - temperature_c / 333.7) * temperature_c / (279.82 + temperature_c))
            .exp()
}

fn dewpoint_from_mixing_ratio(pressure_hpa: f64, mixing_ratio_kgkg: f64) -> f64 {
    if !pressure_hpa.is_finite()
        || pressure_hpa <= 0.0
        || !mixing_ratio_kgkg.is_finite()
        || mixing_ratio_kgkg < -MIXING_RATIO_NEGATIVE_TOLERANCE_KGKG
        || mixing_ratio_kgkg >= 1.0
    {
        return f64::NAN;
    }
    // Tolerate only tiny negative model noise. A broad `max(0)` without this
    // guard would turn common negative fill values (for example -9999) into a
    // valid, extremely dry level and silently contaminate the phase profile.
    let mixing_ratio_kgkg = mixing_ratio_kgkg.max(0.0);
    let vapor_pressure_hpa =
        (mixing_ratio_kgkg * pressure_hpa / (0.622 + mixing_ratio_kgkg)).max(1.0e-10);
    let logarithm = (vapor_pressure_hpa / 6.112).ln();
    243.5 * logarithm / (17.67 - logarithm)
}

fn clamp_pct(value: f64) -> f64 {
    value.clamp(0.0, 100.0)
}

fn no_precip_result(qc: PtypeQc) -> PtypePointResult {
    PtypePointResult {
        scores: PtypeScores::zero(),
        qpf_fractions: PtypeFractions::zero(),
        display_type: PrecipType::NoPrecip,
        confidence: 0.0,
        diagnostics: PtypeDiagnostics::nan(),
        qc,
    }
}

fn unknown_result(qc: PtypeQc) -> PtypePointResult {
    PtypePointResult {
        scores: PtypeScores::nan(),
        qpf_fractions: PtypeFractions::nan(),
        display_type: PrecipType::Unknown,
        confidence: f64::NAN,
        diagnostics: PtypeDiagnostics::nan(),
        qc,
    }
}

fn empty_grid_output(capacity: usize, include_diagnostics: bool) -> PtypeGridOutput {
    PtypeGridOutput {
        fields: PtypeGridFields {
            rain_powt_pct: Vec::with_capacity(capacity),
            snow_powt_pct: Vec::with_capacity(capacity),
            freezing_rain_powt_pct: Vec::with_capacity(capacity),
            ice_pellets_powt_pct: Vec::with_capacity(capacity),
            display_type_code: Vec::with_capacity(capacity),
            confidence: Vec::with_capacity(capacity),
            qc_bits: Vec::with_capacity(capacity),
        },
        diagnostics: include_diagnostics.then(|| PtypeGridDiagnostics {
            surface_wet_bulb_c: Vec::with_capacity(capacity),
            melting_energy_total_jkg: Vec::with_capacity(capacity),
            melting_energy_aloft_jkg: Vec::with_capacity(capacity),
            refreezing_energy_jkg: Vec::with_capacity(capacity),
            probability_ice_pct: Vec::with_capacity(capacity),
            generation_layer_min_temperature_c: Vec::with_capacity(capacity),
            generation_layer_bottom_agl_m: Vec::with_capacity(capacity),
            generation_layer_top_agl_m: Vec::with_capacity(capacity),
        }),
    }
}

fn push_grid_point(output: &mut PtypeGridOutput, point: PtypePointResult) {
    output.fields.rain_powt_pct.push(point.scores.rain_pct as f32);
    output.fields.snow_powt_pct.push(point.scores.snow_pct as f32);
    output
        .fields
        .freezing_rain_powt_pct
        .push(point.scores.freezing_rain_pct as f32);
    output
        .fields
        .ice_pellets_powt_pct
        .push(point.scores.ice_pellets_pct as f32);
    output.fields.display_type_code.push(point.display_type.code());
    output.fields.confidence.push(point.confidence as f32);
    output.fields.qc_bits.push(point.qc.bits());

    if let Some(diagnostics) = &mut output.diagnostics {
        diagnostics
            .surface_wet_bulb_c
            .push(point.diagnostics.surface_wet_bulb_c as f32);
        diagnostics
            .melting_energy_total_jkg
            .push(point.diagnostics.melting_energy_total_jkg as f32);
        diagnostics
            .melting_energy_aloft_jkg
            .push(point.diagnostics.melting_energy_aloft_jkg as f32);
        diagnostics
            .refreezing_energy_jkg
            .push(point.diagnostics.refreezing_energy_jkg as f32);
        diagnostics
            .probability_ice_pct
            .push(point.diagnostics.probability_ice_pct as f32);
        diagnostics
            .generation_layer_min_temperature_c
            .push(point.diagnostics.generation_layer_min_temperature_c as f32);
        diagnostics
            .generation_layer_bottom_agl_m
            .push(point.diagnostics.generation_layer_bottom_agl_m as f32);
        diagnostics
            .generation_layer_top_agl_m
            .push(point.diagnostics.generation_layer_top_agl_m as f32);
    }
}

fn merge_grid_blocks(
    blocks: Vec<PtypeGridOutput>,
    include_diagnostics: bool,
) -> PtypeGridOutput {
    let mut blocks = blocks.into_iter();
    let Some(mut output) = blocks.next() else {
        return empty_grid_output(0, include_diagnostics);
    };

    for mut block in blocks {
        output
            .fields
            .rain_powt_pct
            .append(&mut block.fields.rain_powt_pct);
        output
            .fields
            .snow_powt_pct
            .append(&mut block.fields.snow_powt_pct);
        output
            .fields
            .freezing_rain_powt_pct
            .append(&mut block.fields.freezing_rain_powt_pct);
        output
            .fields
            .ice_pellets_powt_pct
            .append(&mut block.fields.ice_pellets_powt_pct);
        output
            .fields
            .display_type_code
            .append(&mut block.fields.display_type_code);
        output
            .fields
            .confidence
            .append(&mut block.fields.confidence);
        output.fields.qc_bits.append(&mut block.fields.qc_bits);

        if let (Some(target), Some(source)) = (&mut output.diagnostics, &mut block.diagnostics) {
            target
                .surface_wet_bulb_c
                .append(&mut source.surface_wet_bulb_c);
            target
                .melting_energy_total_jkg
                .append(&mut source.melting_energy_total_jkg);
            target
                .melting_energy_aloft_jkg
                .append(&mut source.melting_energy_aloft_jkg);
            target
                .refreezing_energy_jkg
                .append(&mut source.refreezing_energy_jkg);
            target
                .probability_ice_pct
                .append(&mut source.probability_ice_pct);
            target
                .generation_layer_min_temperature_c
                .append(&mut source.generation_layer_min_temperature_c);
            target
                .generation_layer_bottom_agl_m
                .append(&mut source.generation_layer_bottom_agl_m);
            target
                .generation_layer_top_agl_m
                .append(&mut source.generation_layer_top_agl_m);
        }
    }

    output
}

#[cfg(test)]
mod tests {
    use rustwx_core::GridShape;

    use super::*;

    fn wet_level(z: f64, temperature: f64, wet_bulb: f64, rh_ice: f64) -> PtypeWetBulbLevel {
        PtypeWetBulbLevel {
            temperature_c: temperature,
            wet_bulb_c: wet_bulb,
            relative_humidity_ice_pct: rh_ice,
            height_agl_m: z,
        }
    }

    #[test]
    fn specific_humidity_conversion_matches_definition() {
        let q = 0.01;
        assert!((mixing_ratio_from_specific_humidity(q) - q / (1.0 - q)).abs() < 1.0e-15);
        assert!(mixing_ratio_from_specific_humidity(-0.01).is_nan());
        assert!(mixing_ratio_from_specific_humidity(1.0).is_nan());
        assert!(dewpoint_from_mixing_ratio(1000.0, -9999.0).is_nan());
        assert!(dewpoint_from_mixing_ratio(1000.0, 1.0).is_nan());
        assert!(dewpoint_from_mixing_ratio(1000.0, -1.0e-12).is_finite());
    }

    #[test]
    fn probability_ice_polynomial_matches_reference_points() {
        assert_eq!(probability_ice_from_temperature(-15.0), 100.0);
        assert!((probability_ice_from_temperature(-10.0) - 51.0).abs() < 1.0e-9);
        assert_eq!(probability_ice_from_temperature(-7.0), 0.0);
    }

    #[test]
    fn all_cold_saturated_profile_is_snow() {
        let levels = [
            wet_level(0.0, -2.0, -2.0, 95.0),
            wet_level(500.0, -4.0, -4.0, 95.0),
            wet_level(1000.0, -7.0, -7.0, 95.0),
            wet_level(1500.0, -10.0, -10.0, 95.0),
            wet_level(2200.0, -14.0, -14.0, 95.0),
            wet_level(3000.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.display_type, PrecipType::Snow);
        assert!((result.scores.snow_pct - 100.0).abs() < 1.0e-9);
        assert_eq!(result.scores.rain_pct, 0.0);
        assert_eq!(result.scores.freezing_rain_pct, 0.0);
    }

    #[test]
    fn warm_surface_profile_is_rain() {
        let levels = [
            wet_level(0.0, 3.0, 2.0, 95.0),
            wet_level(500.0, 4.0, 3.0, 95.0),
            wet_level(1000.0, 3.0, 2.0, 95.0),
            wet_level(1800.0, -2.0, -2.0, 95.0),
            wet_level(2800.0, -9.0, -9.0, 95.0),
            wet_level(4000.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.display_type, PrecipType::Rain);
        assert!(result.scores.rain_pct > 95.0);
        assert_eq!(result.scores.freezing_rain_pct, 0.0);
        assert_eq!(result.scores.ice_pellets_pct, 0.0);
        assert!(result.diagnostics.melting_energy_total_jkg > 0.0);
        assert_eq!(result.diagnostics.melting_energy_aloft_jkg, 0.0);
    }

    #[test]
    fn surface_melting_energy_still_drives_liquid_probability() {
        // With ice certain and only a very shallow surface warm layer, the
        // low-ME liquid adjustment must use total METw.  Using the PL-only
        // aloft value here would incorrectly force the liquid score to zero.
        let levels = [
            wet_level(0.0, 0.2, 0.2, 95.0),
            wet_level(100.0, 0.2, 0.2, 95.0),
            wet_level(300.0, -1.0, -1.0, 95.0),
            wet_level(1300.0, -10.0, -10.0, 95.0),
            wet_level(2300.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert!(result.diagnostics.melting_energy_total_jkg > 0.0);
        assert!(result.diagnostics.melting_energy_total_jkg < 5.0);
        assert_eq!(result.diagnostics.melting_energy_aloft_jkg, 0.0);
        assert!(result.scores.rain_pct > 0.0);
        assert_eq!(result.scores.ice_pellets_pct, 0.0);
    }

    #[test]
    fn elevated_warm_layer_with_surface_melt_retains_refreezing_pair() {
        // Birk et al. Fig. 1d: an elevated warm layer melts ice, a cold
        // layer can refreeze it, and a second surface warm layer changes
        // liquid precipitation from FZRA to RA without erasing PL potential.
        let levels = [
            wet_level(0.0, 2.0, 2.0, 95.0),
            wet_level(180.0, 0.0, 0.0, 95.0),
            wet_level(600.0, -5.0, -5.0, 95.0),
            wet_level(1200.0, -5.0, -5.0, 95.0),
            wet_level(1600.0, 0.0, 0.0, 95.0),
            wet_level(2200.0, 2.0, 2.0, 95.0),
            wet_level(2800.0, 0.0, 0.0, 95.0),
            wet_level(4000.0, -10.0, -10.0, 95.0),
            wet_level(5200.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert!(result.diagnostics.melting_energy_total_jkg
            > result.diagnostics.melting_energy_aloft_jkg);
        assert!(result.diagnostics.melting_energy_aloft_jkg > 0.0);
        assert!(result.diagnostics.refreezing_energy_jkg > 0.0);
        assert!(result.scores.rain_pct > 0.0);
        assert_eq!(result.scores.freezing_rain_pct, 0.0);
        assert!(result.scores.ice_pellets_pct > 0.0);
    }

    #[test]
    fn warm_nose_over_shallow_cold_layer_is_freezing_rain() {
        let levels = [
            wet_level(0.0, -1.0, -1.0, 95.0),
            wet_level(500.0, -0.5, -0.5, 95.0),
            wet_level(1000.0, 3.0, 3.0, 95.0),
            wet_level(2000.0, 4.0, 4.0, 95.0),
            wet_level(2600.0, 0.0, 0.0, 95.0),
            wet_level(3600.0, -8.0, -8.0, 95.0),
            wet_level(4700.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.display_type, PrecipType::FreezingRain);
        assert!(result.scores.freezing_rain_pct > 90.0);
        assert!(result.scores.ice_pellets_pct < 20.0);
    }

    #[test]
    fn warm_nose_over_deep_cold_layer_is_ice_pellets() {
        let levels = [
            wet_level(0.0, -8.0, -8.0, 95.0),
            wet_level(1000.0, -8.0, -8.0, 95.0),
            wet_level(1500.0, -4.0, -4.0, 95.0),
            wet_level(2200.0, 3.0, 3.0, 95.0),
            wet_level(3200.0, 4.0, 4.0, 95.0),
            wet_level(3700.0, 0.0, 0.0, 95.0),
            wet_level(4700.0, -10.0, -10.0, 95.0),
            wet_level(5700.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.display_type, PrecipType::IcePellets);
        assert!(result.scores.ice_pellets_pct > 90.0);
        assert!(result.scores.freezing_rain_pct < 20.0);
    }

    #[test]
    fn deep_dry_layer_eliminates_upper_generation_layer() {
        let levels = [
            wet_level(0.0, -2.0, -2.0, 40.0),
            wet_level(1000.0, -4.0, -4.0, 40.0),
            wet_level(2000.0, -6.0, -6.0, 40.0),
            wet_level(2600.0, -8.0, -8.0, 40.0),
            wet_level(3200.0, -10.0, -10.0, 95.0),
            wet_level(4300.0, -13.0, -13.0, 95.0),
            wet_level(5500.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.diagnostics.probability_ice_pct, 0.0);
        assert!(result.qc.contains(PtypeQc::UPPER_GENERATION_LAYER_REMOVED));
        assert!(result.qc.contains(PtypeQc::NO_PRECIP_GENERATION_LAYER));
    }

    #[test]
    fn exactly_one_kilometer_is_not_a_generation_layer() {
        // The paper requires a saturated layer to exceed 1 km in depth.
        let levels = [
            wet_level(0.0, -5.0, -5.0, 95.0),
            wet_level(500.0, -10.0, -10.0, 95.0),
            wet_level(1000.0, -15.0, -15.0, 75.0),
            wet_level(1500.0, -18.0, -18.0, 40.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.diagnostics.probability_ice_pct, 0.0);
        assert!(result.qc.contains(PtypeQc::NO_PRECIP_GENERATION_LAYER));
    }

    #[test]
    fn duplicate_surface_height_keeps_first_live_surface_value() {
        let levels = [
            wet_level(0.0, 2.0, 2.0, 95.0),
            wet_level(0.0, -3.0, -3.0, 95.0),
            wet_level(1200.0, -10.0, -10.0, 95.0),
            wet_level(2400.0, -17.0, -17.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(result.diagnostics.surface_wet_bulb_c, 2.0);
        assert!(result.qc.contains(PtypeQc::DUPLICATE_HEIGHT_REMOVED));
        assert_eq!(result.scores.freezing_rain_pct, 0.0);
    }

    #[test]
    fn upper_warm_nose_does_not_replace_near_surface_pl_pair() {
        let levels = [
            wet_level(0.0, -2.0, -2.0, 95.0),
            wet_level(600.0, 0.0, 0.0, 95.0),
            wet_level(1100.0, 2.0, 2.0, 95.0),
            wet_level(1600.0, 0.0, 0.0, 95.0),
            wet_level(2200.0, -6.0, -6.0, 95.0),
            wet_level(2800.0, 0.0, 0.0, 95.0),
            wet_level(3300.0, 5.0, 5.0, 95.0),
            wet_level(3800.0, 0.0, 0.0, 95.0),
            wet_level(5000.0, -15.0, -15.0, 95.0),
        ];
        let result = classify_modified_bourgouin_wet_bulb_profile(
            &levels,
            true,
            &PtypeOptions::default(),
        )
        .unwrap();
        let expected_near_surface_melt = layer_energy(600.0, 1100.0, 0.0, 2.0)
            + layer_energy(1100.0, 1600.0, 2.0, 0.0);
        assert!(
            (result.diagnostics.melting_energy_aloft_jkg - expected_near_surface_melt).abs()
                < 1.0e-12
        );
        assert!(
            result.diagnostics.melting_energy_total_jkg
                > result.diagnostics.melting_energy_aloft_jkg
        );
    }

    #[test]
    fn independent_scores_normalize_for_qpf_split() {
        let fractions = PtypeScores {
            rain_pct: 0.0,
            snow_pct: 100.0,
            freezing_rain_pct: 0.0,
            ice_pellets_pct: 30.0,
        }
        .qpf_fractions();
        assert!((fractions.snow - 100.0 / 130.0).abs() < 1.0e-12);
        assert!((fractions.ice_pellets - 30.0 / 130.0).abs() < 1.0e-12);
    }

    #[test]
    fn grid_wrapper_respects_inactive_mask() {
        let grid = GridShape::new(1, 1).unwrap();
        let shape = VolumeShape::new(grid, 2).unwrap();
        let output = compute_modified_bourgouin_ptype(
            PtypeGridInputs {
                shape,
                pressure_3d_pa: &[90000.0, 80000.0],
                temperature_3d_c: &[-5.0, -10.0],
                qvapor_3d_kgkg: &[0.003, 0.002],
                height_agl_3d_m: &[500.0, 1500.0],
                psfc_pa: &[100000.0],
                t2_k: &[271.15],
                q2_kgkg: &[0.003],
                active_mask: Some(&[0]),
            },
            &PtypeOptions::default(),
        )
        .unwrap();
        assert_eq!(output.fields.display_type_code, vec![PrecipType::NoPrecip.code()]);
        assert_eq!(output.fields.rain_powt_pct, vec![0.0]);
        assert_eq!(output.fields.qc_bits[0], PtypeQc::ACTIVE_MASK_OFF.bits());
    }

    #[test]
    fn grid_wrapper_emits_ordered_compact_diagnostics() {
        let grid = GridShape::new(2, 1).unwrap();
        let shape = VolumeShape::new(grid, 3).unwrap();
        let output = compute_modified_bourgouin_ptype(
            PtypeGridInputs {
                shape,
                pressure_3d_pa: &[90000.0, 80000.0, 70000.0],
                temperature_3d_c: &[-3.0, -3.0, -8.0, -8.0, -16.0, -16.0],
                qvapor_3d_kgkg: &[0.003, 0.003, 0.002, 0.002, 0.001, 0.001],
                height_agl_3d_m: &[500.0, 500.0, 1500.0, 1500.0, 3000.0, 3000.0],
                psfc_pa: &[100000.0, 100000.0],
                t2_k: &[271.15, 271.15],
                q2_kgkg: &[0.003, 0.003],
                active_mask: Some(&[1, 0]),
            },
            &PtypeOptions {
                include_diagnostics: true,
                ..PtypeOptions::default()
            },
        )
        .unwrap();

        assert_eq!(output.fields.rain_powt_pct.len(), 2);
        assert_ne!(output.fields.display_type_code[0], PrecipType::NoPrecip.code());
        assert_eq!(output.fields.display_type_code[1], PrecipType::NoPrecip.code());
        let diagnostics = output.diagnostics.unwrap();
        assert_eq!(diagnostics.surface_wet_bulb_c.len(), 2);
        assert!(diagnostics.surface_wet_bulb_c[0].is_finite());
        assert!(diagnostics.surface_wet_bulb_c[1].is_nan());
    }

    #[test]
    fn grid_wrapper_rejects_bad_pressure_length() {
        let grid = GridShape::new(1, 1).unwrap();
        let shape = VolumeShape::new(grid, 2).unwrap();
        let error = compute_modified_bourgouin_ptype(
            PtypeGridInputs {
                shape,
                pressure_3d_pa: &[90000.0, 80000.0, 70000.0],
                temperature_3d_c: &[-5.0, -10.0],
                qvapor_3d_kgkg: &[0.003, 0.002],
                height_agl_3d_m: &[500.0, 1500.0],
                psfc_pa: &[100000.0],
                t2_k: &[271.15],
                q2_kgkg: &[0.003],
                active_mask: None,
            },
            &PtypeOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            CalcError::LengthMismatch {
                field: "pressure_3d_pa",
                ..
            }
        ));
    }
}

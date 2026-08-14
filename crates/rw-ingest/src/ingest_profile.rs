//! Ingest profiles: WHICH subset of the full per-hour ingest plan a run
//! fetches, extracts, computes, and stores. Shared via `#[path]` inclusion
//! as a child of `ingest_hour` (the single inclusion point, so every bin
//! sees one set of types).
//!
//! A profile is the customization surface for model-data packs: volumes
//! (the 3D isobaric variables), the isobaric level step (25 or 50 hPa),
//! the 2D surface field set (everything, or a named subset), and the two
//! compute stages (derived, heavy). Five named presets exist:
//!
//! * `full` — today's default ingest, unchanged: all 5 volumes at 25 hPa
//!   steps, every 2D field (surface set + trailing 1 h windows + vorticity
//!   planes + direct-recipe isobaric planes), derived AND heavy stages.
//! * `sounding` — the point-sounding pack: all 5 volumes plus the 7 surface
//!   fields a sounding/hodograph view needs; no derived, no heavy, none of
//!   the render-grade 2D planes.
//! * `view` — the 2D map pack: every 2D field including the derived grids,
//!   NO volumes, no heavy.
//! * `surface` - every directly published 2D field, with no pressure-volume
//!   or derived/heavy dependency. This is the complete native pack for a
//!   surface-only forecast product.
//! * `analysis` - the narrow surface-analysis pack: only fields published by
//!   the RTMA/URMA `2dvaranl_ndfd` product, with no pressure volumes or
//!   derived/heavy stages.
//!
//! Validation happens HERE, not mid-ingest: the derived/heavy stages decode
//! their thermo inputs from the full surface + pressure files, so a profile
//! that stores only a named surface subset (and therefore skips the prs
//! 2D planes) excludes their inputs and must be rejected up front.

use rustwx_core::{CanonicalField, FieldProduct, FieldSelector, ModelId, ProbabilitySelection};

/// The two supported isobaric level steps (hPa) over the 100..=1000 range.
pub const LEVEL_STEPS_HPA: [u16; 2] = [25, 50];

/// One 3D isobaric variable choice, mapping to the stable store names the
/// full ingest has always written (`temperature_iso`, `dewpoint_iso`,
/// `u_iso`, `v_iso`, `height_iso`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeChoice {
    Temperature,
    Dewpoint,
    UWind,
    VWind,
    GeopotentialHeight,
}

impl VolumeChoice {
    /// Every volume, in the order the full ingest has always stored them.
    pub const ALL: [VolumeChoice; 5] = [
        VolumeChoice::Temperature,
        VolumeChoice::Dewpoint,
        VolumeChoice::UWind,
        VolumeChoice::VWind,
        VolumeChoice::GeopotentialHeight,
    ];

    /// The GRIB field this volume extracts.
    pub fn field(self) -> CanonicalField {
        match self {
            VolumeChoice::Temperature => CanonicalField::Temperature,
            VolumeChoice::Dewpoint => CanonicalField::Dewpoint,
            VolumeChoice::UWind => CanonicalField::UWind,
            VolumeChoice::VWind => CanonicalField::VWind,
            VolumeChoice::GeopotentialHeight => CanonicalField::GeopotentialHeight,
        }
    }

    /// The stable store variable name (dewpoint may fall back to `rh_iso`
    /// at ingest when the file realizes fewer than two dewpoint levels).
    pub fn store_name(self) -> &'static str {
        match self {
            VolumeChoice::Temperature => "temperature_iso",
            VolumeChoice::Dewpoint => "dewpoint_iso",
            VolumeChoice::UWind => "u_iso",
            VolumeChoice::VWind => "v_iso",
            VolumeChoice::GeopotentialHeight => "height_iso",
        }
    }
}

/// The 2D surface field set a profile stores: everything the full ingest
/// plan carries (surface plan + trailing 1 h windows + vorticity planes +
/// direct-recipe isobaric planes), or a named subset of the surface plan
/// (names from [`surface_plan`]; the prs-sourced planes and trailing
/// windows ride only with `All`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSet {
    All,
    Named(Vec<String>),
}

/// One ingest profile: what to fetch/extract/compute/store per hour.
#[derive(Debug, Clone, PartialEq)]
pub struct IngestProfile {
    pub volumes: Vec<VolumeChoice>,
    /// Isobaric level step over 100..=1000 hPa: 25 (37 levels) or 50 (19).
    pub level_step_hpa: u16,
    pub surface_fields: FieldSet,
    /// Run the non-heavy derived compute stage (29 recipe grids).
    pub derived: bool,
    /// Run the heavy ECAPE compute stage (16 recipe grids).
    pub heavy: bool,
}

/// The 7 surface fields the `sounding` preset stores: the 2 m/10 m state +
/// pressure trio a sounding/hodograph view needs (orography anchors the
/// column AGL heights; mslp labels the chart).
pub const SOUNDING_SURFACE_FIELDS: [&str; 7] = [
    "temperature_2m",
    "dewpoint_2m",
    "u_10m",
    "v_10m",
    "surface_pressure",
    "mslp",
    "orography",
];

/// Surface fields published by the CONUS RTMA/URMA `2dvaranl_ndfd`
/// analysis. This deliberately excludes fields absent from the product
/// (MSLP, precipitation, reflectivity, pressure-level data) so selecting the
/// preset can never manufacture or imply unsupported data.
pub const ANALYSIS_SURFACE_FIELDS: [&str; 9] = [
    "temperature_2m",
    "dewpoint_2m",
    "u_10m",
    "v_10m",
    "wind_gust_10m",
    "surface_pressure",
    "orography",
    "cloud_cover_total",
    "visibility",
];

impl IngestProfile {
    /// Today's default ingest, unchanged: everything, both compute stages.
    pub fn full() -> Self {
        Self {
            volumes: VolumeChoice::ALL.to_vec(),
            level_step_hpa: 25,
            surface_fields: FieldSet::All,
            derived: true,
            heavy: true,
        }
    }

    /// The point-sounding pack: 5 volumes + 7 surface fields, no compute
    /// stages, no render-grade 2D planes.
    pub fn sounding() -> Self {
        Self {
            volumes: VolumeChoice::ALL.to_vec(),
            level_step_hpa: 25,
            surface_fields: FieldSet::Named(
                SOUNDING_SURFACE_FIELDS
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            ),
            derived: false,
            heavy: false,
        }
    }

    /// The 2D map pack: every 2D field including derived grids, no
    /// volumes, no heavy stage.
    pub fn view() -> Self {
        Self {
            volumes: Vec::new(),
            level_step_hpa: 25,
            surface_fields: FieldSet::All,
            derived: true,
            heavy: false,
        }
    }

    /// Complete deterministic direct-field pack for surface-only forecast
    /// products. A named set deliberately avoids the pressure-sourced planes
    /// attached to `FieldSet::All`, while retaining every long-standing base
    /// surface selector (partial extraction skips absent selectors honestly).
    pub fn surface() -> Self {
        Self::surface_with_plan(base_surface_plan())
    }

    /// Complete direct-field pack for one model. Provider-statistics-only
    /// systems use their exact typed inventory; deterministic models retain
    /// the stable base surface preset.
    pub fn surface_for_model(model: ModelId) -> Self {
        Self::surface_with_plan(model_surface_plan(model))
    }

    fn surface_with_plan(plan: Vec<(&'static str, FieldSelector)>) -> Self {
        Self {
            volumes: Vec::new(),
            level_step_hpa: 25,
            surface_fields: FieldSet::Named(
                plan.into_iter().map(|(name, _)| name.to_string()).collect(),
            ),
            derived: false,
            heavy: false,
        }
    }

    /// Surface-only analysis pack for products such as RTMA/URMA
    /// `2dvaranl_ndfd`: no pressure volumes and no compute stages whose
    /// inputs require a pressure product.
    pub fn analysis() -> Self {
        Self {
            volumes: Vec::new(),
            level_step_hpa: 25,
            surface_fields: FieldSet::Named(
                ANALYSIS_SURFACE_FIELDS
                    .iter()
                    .map(|name| (*name).to_string())
                    .collect(),
            ),
            derived: false,
            heavy: false,
        }
    }

    /// Preset lookup by CLI name.
    pub fn preset(name: &str) -> Result<Self, String> {
        match name {
            "full" => Ok(Self::full()),
            "sounding" => Ok(Self::sounding()),
            "view" => Ok(Self::view()),
            "surface" => Ok(Self::surface()),
            "analysis" => Ok(Self::analysis()),
            other => Err(format!(
                "--profile: unknown preset '{other}' (expected full, sounding, view, surface, or analysis)"
            )),
        }
    }

    /// Candidate isobaric levels (hPa) for every volume; absent levels are
    /// simply not stored (same partial-extraction behavior as always).
    pub fn candidate_levels(&self) -> Vec<u16> {
        (100..=1000)
            .step_by(usize::from(self.level_step_hpa))
            .collect()
    }

    /// Whether the profile stores the full 2D plan: the trailing 1 h window
    /// fields, the vorticity planes, and the direct-recipe isobaric planes
    /// ride only with `FieldSet::All`.
    pub fn includes_full_2d(&self) -> bool {
        matches!(self.surface_fields, FieldSet::All)
    }

    /// Whether one named surface-plan field is stored under this profile.
    pub fn includes_surface_field(&self, name: &str) -> bool {
        match &self.surface_fields {
            FieldSet::All => true,
            FieldSet::Named(names) => names.iter().any(|have| have == name),
        }
    }

    /// Whether one volume field is stored under this profile.
    pub fn includes_volume_field(&self, field: CanonicalField) -> bool {
        self.volumes.iter().any(|choice| choice.field() == field)
    }

    /// Whether the ingest needs the pressure ("prs") product file at all:
    /// volumes, the prs-sourced 2D planes, or either compute stage (their
    /// thermo decode reads the raw prs bytes).
    pub fn needs_prs(&self) -> bool {
        !self.volumes.is_empty() || self.includes_full_2d() || self.derived || self.heavy
    }

    /// Validate the profile as a whole. Rules:
    /// 1. `level_step_hpa` must be 25 or 50.
    /// 2. No duplicate volume choices.
    /// 3. A named surface set must be non-empty and name only known
    ///    surface-plan fields.
    /// 4. The derived/heavy stages need the full 2D surface set and the
    ///    prs file (their thermo pair decodes from both family files); a
    ///    named-subset profile excludes those inputs.
    /// 5. The heavy stage builds on the derived stage.
    pub fn validate(&self) -> Result<(), String> {
        if !LEVEL_STEPS_HPA.contains(&self.level_step_hpa) {
            return Err(format!(
                "profile: level step {} hPa is not supported (expected 25 or 50)",
                self.level_step_hpa
            ));
        }
        for (index, choice) in self.volumes.iter().enumerate() {
            if self.volumes[..index].contains(choice) {
                return Err(format!(
                    "profile: duplicate volume '{}'",
                    choice.store_name()
                ));
            }
        }
        if let FieldSet::Named(names) = &self.surface_fields {
            if names.is_empty() {
                return Err(
                    "profile: the named surface field set is empty; every hour needs at \
                     least one surface field to carry the grid"
                        .to_string(),
                );
            }
            for name in names {
                if !surface_plan().iter().any(|(have, _)| have == name) {
                    return Err(format!(
                        "profile: unknown surface field '{name}' (known fields: {})",
                        surface_plan()
                            .iter()
                            .map(|(have, _)| *have)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
        if (self.derived || self.heavy) && !self.includes_full_2d() {
            return Err(
                "profile: the derived/heavy stages need the full 2D surface set and the \
                 prs file as compute inputs, but this profile stores only a named surface \
                 subset; use the full or view preset, or disable the compute stages"
                    .to_string(),
            );
        }
        if self.heavy && !self.derived {
            return Err(
                "profile: the heavy stage builds on the derived stage; enable derived \
                 (drop --no-derived) or disable heavy (--no-heavy)"
                    .to_string(),
            );
        }
        Ok(())
    }

    /// One-line human description for CLI banners.
    pub fn describe(&self) -> String {
        let volumes = if self.volumes.is_empty() {
            "no volumes".to_string()
        } else {
            format!(
                "{} volume(s) @ {} hPa steps ({} levels)",
                self.volumes.len(),
                self.level_step_hpa,
                self.candidate_levels().len()
            )
        };
        let surface = match &self.surface_fields {
            FieldSet::All => "all 2D fields".to_string(),
            FieldSet::Named(names) => format!("{} named surface field(s)", names.len()),
        };
        format!(
            "{volumes}, {surface}, derived {}, heavy {}",
            if self.derived { "on" } else { "off" },
            if self.heavy { "on" } else { "off" },
        )
    }
}

/// CLI override flags applied on top of a preset (the composable surface:
/// `--profile NAME [--level-step N] [--no-derived] [--heavy|--no-heavy]`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ProfileOverrides {
    /// `--level-step N` (25 or 50).
    pub level_step_hpa: Option<u16>,
    /// `--no-derived`: skip the derived compute stage.
    pub no_derived: bool,
    /// `--heavy` (Some(true)) / `--no-heavy` (Some(false)) / neither (None).
    pub heavy: Option<bool>,
}

/// Resolve a preset name + override flags into a validated profile.
pub fn resolve_profile(
    preset: &str,
    overrides: &ProfileOverrides,
) -> Result<IngestProfile, String> {
    apply_profile_overrides(preset, IngestProfile::preset(preset)?, overrides)
}

/// Resolve a preset for one model. Only `surface` is model-specialized; all
/// other named profiles intentionally keep their established semantics.
pub fn resolve_profile_for_model(
    preset: &str,
    overrides: &ProfileOverrides,
    model: ModelId,
) -> Result<IngestProfile, String> {
    let profile = if preset == "surface" {
        IngestProfile::surface_for_model(model)
    } else {
        IngestProfile::preset(preset)?
    };
    apply_profile_overrides(preset, profile, overrides)
}

fn apply_profile_overrides(
    preset: &str,
    mut profile: IngestProfile,
    overrides: &ProfileOverrides,
) -> Result<IngestProfile, String> {
    if let Some(step) = overrides.level_step_hpa {
        if !LEVEL_STEPS_HPA.contains(&step) {
            return Err(format!(
                "--level-step: {step} hPa is not supported (expected 25 or 50)"
            ));
        }
        profile.level_step_hpa = step;
    }
    if overrides.no_derived {
        profile.derived = false;
    }
    if let Some(heavy) = overrides.heavy {
        profile.heavy = heavy;
    }
    profile
        .validate()
        .map_err(|err| format!("--profile {preset}: {err}"))?;
    Ok(profile)
}

/// 2D fields pulled from the surface ("sfc") product file, with their stable
/// store names. These mirror the selector constructors the rustwx-models
/// plot-recipe catalog uses for the same HRRR fields. (Moved here from
/// `ingest_hour` so profile validation and the ingest plan share one list.)
///
/// CAPE has no plan entry: it is sounding-derived here (no CAPE
/// CanonicalField) and ships through the derived precompute stage instead
/// (`sbcape`/`mlcape`/`mucape`... — see `compute_derived_grids`).
///
/// `apcp_run_total` is the plain TotalPrecipitation selection: the sfc file
/// carries two APCP accumulations that both end at hour h (0->h run total
/// and the trailing (h-1)->h hour); they tie on match score and the run
/// total wins as first in file order. The trailing 1 h window is stored
/// separately as `apcp_1h` via a dedicated re-select (see `ingest_hour`).
///
/// Lightning flash density is deliberately absent: rustwx-io has no
/// structured selector for it (HRRR exposes LTNG, a non-dimensional
/// lightning flag, and LTNGSD strike density — not flash density), and the
/// recipe catalog blocks the slug for HRRR for the same mislabeling reason.
fn base_surface_plan() -> Vec<(&'static str, FieldSelector)> {
    vec![
        (
            "temperature_2m",
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
        ),
        (
            "dewpoint_2m",
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
        ),
        (
            "u_10m",
            FieldSelector::height_agl(CanonicalField::UWind, 10),
        ),
        (
            "v_10m",
            FieldSelector::height_agl(CanonicalField::VWind, 10),
        ),
        (
            "composite_reflectivity",
            FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        ),
        (
            "mslp",
            FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel),
        ),
        // --- surface state & moisture (feeds SurfaceInputs-derived products) ---
        (
            "rh_2m",
            FieldSelector::height_agl(CanonicalField::RelativeHumidity, 2),
        ),
        (
            "wind_gust_10m",
            FieldSelector::height_agl(CanonicalField::WindGust, 10),
        ),
        (
            "surface_pressure",
            FieldSelector::surface(CanonicalField::Pressure),
        ),
        (
            "orography",
            FieldSelector::surface(CanonicalField::GeopotentialHeight),
        ),
        // --- precipitation & precip type ---
        (
            "apcp_run_total",
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
        ),
        (
            "categorical_rain",
            FieldSelector::surface(CanonicalField::CategoricalRain),
        ),
        (
            "categorical_freezing_rain",
            FieldSelector::surface(CanonicalField::CategoricalFreezingRain),
        ),
        (
            "categorical_ice_pellets",
            FieldSelector::surface(CanonicalField::CategoricalIcePellets),
        ),
        (
            "categorical_snow",
            FieldSelector::surface(CanonicalField::CategoricalSnow),
        ),
        // --- moisture column, clouds, visibility ---
        (
            "pwat",
            FieldSelector::entire_atmosphere(CanonicalField::PrecipitableWater),
        ),
        (
            "cloud_cover_low",
            FieldSelector::entire_atmosphere(CanonicalField::LowCloudCover),
        ),
        (
            "cloud_cover_mid",
            FieldSelector::entire_atmosphere(CanonicalField::MiddleCloudCover),
        ),
        (
            "cloud_cover_high",
            FieldSelector::entire_atmosphere(CanonicalField::HighCloudCover),
        ),
        (
            "cloud_cover_total",
            FieldSelector::entire_atmosphere(CanonicalField::TotalCloudCover),
        ),
        (
            "visibility",
            FieldSelector::surface(CanonicalField::Visibility),
        ),
        // --- convection, smoke, satellite (also in wrfnat; sfc carries them
        //     too, so they ride this fetch — see the ingest_hour module doc) ---
        (
            "reflectivity_1km",
            FieldSelector::height_agl(CanonicalField::RadarReflectivity, 1000),
        ),
        (
            "uh_2to5km",
            FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
        ),
        (
            "smoke_8m",
            FieldSelector::height_agl(CanonicalField::SmokeMassDensity, 8),
        ),
        (
            "smoke_column",
            FieldSelector::entire_atmosphere(CanonicalField::ColumnIntegratedSmoke),
        ),
        (
            "simulated_ir",
            FieldSelector::nominal_top(CanonicalField::SimulatedInfraredBrightnessTemperature),
        ),
    ]
}

/// Direct 2-D fields normalized for one model. Most deterministic models use
/// the long-standing surface plan above. CMA GRAPES GEPS publishes only
/// provider-computed ensemble statistics, so its plan names and selects every
/// scientifically identified statistic instead of pretending those records
/// are deterministic surface fields or raw ensemble members.
fn cma_geps_statistics_surface_plan() -> Vec<(&'static str, FieldSelector)> {
    let mut plan = vec![
        (
            "height_500hpa_ensemble_mean",
            FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500).with_ensemble_mean(),
        ),
        (
            "height_500hpa_ensemble_spread",
            FieldSelector::isobaric(CanonicalField::GeopotentialHeight, 500).with_ensemble_spread(),
        ),
        (
            "mslp_ensemble_mean",
            FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel)
                .with_ensemble_mean(),
        ),
        (
            "mslp_ensemble_spread",
            FieldSelector::mean_sea_level(CanonicalField::PressureReducedToMeanSeaLevel)
                .with_ensemble_spread(),
        ),
    ];

    const PERCENTILES: [u8; 5] = [0, 25, 50, 75, 100];
    const PERCENTILE_FAMILIES: [(&str, FieldSelector); 7] = [
        (
            "temperature_2m",
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
        ),
        (
            "temperature_850hpa",
            FieldSelector::isobaric(CanonicalField::Temperature, 850),
        ),
        (
            "wind_speed_10m",
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
        ),
        (
            "wind_speed_850hpa",
            FieldSelector::isobaric(CanonicalField::WindSpeed, 850),
        ),
        (
            "wind_speed_250hpa",
            FieldSelector::isobaric(CanonicalField::WindSpeed, 250),
        ),
        (
            "wind_gust_10m",
            FieldSelector::height_agl(CanonicalField::WindGust, 10),
        ),
        (
            "cloud_cover_total",
            // CMA encodes the full-column statistic with GRIB level type 1
            // (surface) even though parameter 0/6/1 remains total cloud
            // cover. Preserve that provider contract explicitly.
            FieldSelector::surface(CanonicalField::TotalCloudCover),
        ),
    ];
    for (family, selector) in PERCENTILE_FAMILIES {
        for percentile in PERCENTILES {
            let name = match (family, percentile) {
                ("temperature_2m", 0) => "temperature_2m_p00",
                ("temperature_2m", 25) => "temperature_2m_p25",
                ("temperature_2m", 50) => "temperature_2m_p50",
                ("temperature_2m", 75) => "temperature_2m_p75",
                ("temperature_2m", 100) => "temperature_2m_p100",
                ("temperature_850hpa", 0) => "temperature_850hpa_p00",
                ("temperature_850hpa", 25) => "temperature_850hpa_p25",
                ("temperature_850hpa", 50) => "temperature_850hpa_p50",
                ("temperature_850hpa", 75) => "temperature_850hpa_p75",
                ("temperature_850hpa", 100) => "temperature_850hpa_p100",
                ("wind_speed_10m", 0) => "wind_speed_10m_p00",
                ("wind_speed_10m", 25) => "wind_speed_10m_p25",
                ("wind_speed_10m", 50) => "wind_speed_10m_p50",
                ("wind_speed_10m", 75) => "wind_speed_10m_p75",
                ("wind_speed_10m", 100) => "wind_speed_10m_p100",
                ("wind_speed_850hpa", 0) => "wind_speed_850hpa_p00",
                ("wind_speed_850hpa", 25) => "wind_speed_850hpa_p25",
                ("wind_speed_850hpa", 50) => "wind_speed_850hpa_p50",
                ("wind_speed_850hpa", 75) => "wind_speed_850hpa_p75",
                ("wind_speed_850hpa", 100) => "wind_speed_850hpa_p100",
                ("wind_speed_250hpa", 0) => "wind_speed_250hpa_p00",
                ("wind_speed_250hpa", 25) => "wind_speed_250hpa_p25",
                ("wind_speed_250hpa", 50) => "wind_speed_250hpa_p50",
                ("wind_speed_250hpa", 75) => "wind_speed_250hpa_p75",
                ("wind_speed_250hpa", 100) => "wind_speed_250hpa_p100",
                ("wind_gust_10m", 0) => "wind_gust_10m_p00",
                ("wind_gust_10m", 25) => "wind_gust_10m_p25",
                ("wind_gust_10m", 50) => "wind_gust_10m_p50",
                ("wind_gust_10m", 75) => "wind_gust_10m_p75",
                ("wind_gust_10m", 100) => "wind_gust_10m_p100",
                ("cloud_cover_total", 0) => "cloud_cover_total_p00",
                ("cloud_cover_total", 25) => "cloud_cover_total_p25",
                ("cloud_cover_total", 50) => "cloud_cover_total_p50",
                ("cloud_cover_total", 75) => "cloud_cover_total_p75",
                ("cloud_cover_total", 100) => "cloud_cover_total_p100",
                _ => unreachable!("complete CMA GEPS percentile name table"),
            };
            plan.push((name, selector.with_percentile(percentile)));
        }
    }

    for (threshold_milli, name) in [
        (10_000, "wind_speed_10m_probability_gt_10ms"),
        (15_000, "wind_speed_10m_probability_gt_15ms"),
        (20_000, "wind_speed_10m_probability_gt_20ms"),
        (25_000, "wind_speed_10m_probability_gt_25ms"),
    ] {
        plan.push((
            name,
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10).with_probability(
                ProbabilitySelection::new(Some(3), Some(threshold_milli), None),
            ),
        ));
    }
    for (threshold_milli, name) in [
        (15_000, "wind_gust_10m_probability_gt_15ms"),
        (25_000, "wind_gust_10m_probability_gt_25ms"),
        (35_000, "wind_gust_10m_probability_gt_35ms"),
    ] {
        plan.push((
            name,
            FieldSelector::height_agl(CanonicalField::WindGust, 10).with_probability(
                ProbabilitySelection::new(Some(3), Some(threshold_milli), None),
            ),
        ));
    }

    for percentile in PERCENTILES {
        let name = match percentile {
            0 => "apcp_run_total_p00",
            25 => "apcp_run_total_p25",
            50 => "apcp_run_total_p50",
            75 => "apcp_run_total_p75",
            100 => "apcp_run_total_p100",
            _ => unreachable!("fixed CMA GEPS percentile list"),
        };
        plan.push((
            name,
            FieldSelector::surface(CanonicalField::TotalPrecipitation).with_percentile(percentile),
        ));
    }
    for (threshold_milli, name) in [
        (1_000, "apcp_run_total_probability_gt_1mm"),
        (5_000, "apcp_run_total_probability_gt_5mm"),
        (10_000, "apcp_run_total_probability_gt_10mm"),
        (25_000, "apcp_run_total_probability_gt_25mm"),
        (50_000, "apcp_run_total_probability_gt_50mm"),
        (100_000, "apcp_run_total_probability_gt_100mm"),
    ] {
        plan.push((
            name,
            FieldSelector::surface(CanonicalField::TotalPrecipitation).with_probability(
                ProbabilitySelection::new(Some(3), Some(threshold_milli), None),
            ),
        ));
    }
    plan
}

fn append_reps_full_statistics(
    plan: &mut Vec<(&'static str, FieldSelector)>,
    names: [&'static str; 9],
    selector: FieldSelector,
) {
    const PRODUCTS: [FieldProduct; 9] = [
        FieldProduct::Percentile(10),
        FieldProduct::Percentile(25),
        FieldProduct::Percentile(50),
        FieldProduct::Percentile(75),
        FieldProduct::Percentile(90),
        FieldProduct::EnsembleSpread,
        FieldProduct::EnsembleMean,
        FieldProduct::EnsembleMinimum,
        FieldProduct::EnsembleMaximum,
    ];
    plan.extend(
        names
            .into_iter()
            .zip(PRODUCTS)
            .map(|(name, product)| (name, selector.with_product(product))),
    );
}

/// Exact scalar statistics inventory admitted from ECCC REPS. Wind is the
/// provider's scalar WIND product, so no grid-relative vector is mislabeled
/// earth-relative. Every selector is explicitly statistical; raw members and
/// deterministic/default selectors cannot enter this plan.
fn reps_statistics_surface_plan() -> Vec<(&'static str, FieldSelector)> {
    let mut plan = Vec::with_capacity(37);
    append_reps_full_statistics(
        &mut plan,
        [
            "temperature_2m_p10",
            "temperature_2m_p25",
            "temperature_2m_p50",
            "temperature_2m_p75",
            "temperature_2m_p90",
            "temperature_2m_ensemble_spread",
            "temperature_2m_ensemble_mean",
            "temperature_2m_ensemble_min",
            "temperature_2m_ensemble_max",
        ],
        FieldSelector::height_agl(CanonicalField::Temperature, 2),
    );
    append_reps_full_statistics(
        &mut plan,
        [
            "wind_speed_10m_p10",
            "wind_speed_10m_p25",
            "wind_speed_10m_p50",
            "wind_speed_10m_p75",
            "wind_speed_10m_p90",
            "wind_speed_10m_ensemble_spread",
            "wind_speed_10m_ensemble_mean",
            "wind_speed_10m_ensemble_min",
            "wind_speed_10m_ensemble_max",
        ],
        FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
    );
    append_reps_full_statistics(
        &mut plan,
        [
            "total_precipitation_3h_p10",
            "total_precipitation_3h_p25",
            "total_precipitation_3h_p50",
            "total_precipitation_3h_p75",
            "total_precipitation_3h_p90",
            "total_precipitation_3h_ensemble_spread",
            "total_precipitation_3h_ensemble_mean",
            "total_precipitation_3h_ensemble_min",
            "total_precipitation_3h_ensemble_max",
        ],
        FieldSelector::surface(CanonicalField::TotalPrecipitation),
    );
    for (threshold_milli, name) in [
        (1_000, "total_precipitation_3h_probability_gt_1mm"),
        (2_500, "total_precipitation_3h_probability_gt_2p5mm"),
        (5_000, "total_precipitation_3h_probability_gt_5mm"),
        (10_000, "total_precipitation_3h_probability_gt_10mm"),
        (15_000, "total_precipitation_3h_probability_gt_15mm"),
        (20_000, "total_precipitation_3h_probability_gt_20mm"),
        (25_000, "total_precipitation_3h_probability_gt_25mm"),
        (30_000, "total_precipitation_3h_probability_gt_30mm"),
        (40_000, "total_precipitation_3h_probability_gt_40mm"),
        (50_000, "total_precipitation_3h_probability_gt_50mm"),
    ] {
        plan.push((
            name,
            FieldSelector::surface(CanonicalField::TotalPrecipitation).with_probability(
                ProbabilitySelection::new(Some(3), Some(threshold_milli), None),
            ),
        ));
    }
    debug_assert_eq!(plan.len(), 37);
    plan
}

/// Union of every stable direct-field name accepted by an ingest profile.
/// Model-specific execution uses [`model_surface_plan`] so adding a provider
/// statistics family does not inflate existing deterministic model stores.
pub fn surface_plan() -> Vec<(&'static str, FieldSelector)> {
    let mut plan = base_surface_plan();
    plan.extend(cma_geps_statistics_surface_plan());
    plan.extend(reps_statistics_surface_plan());
    plan
}

pub fn model_surface_plan(model: ModelId) -> Vec<(&'static str, FieldSelector)> {
    match model {
        ModelId::CmaGeps => cma_geps_statistics_surface_plan(),
        ModelId::Reps => reps_statistics_surface_plan(),
        _ => base_surface_plan(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_preset_is_todays_behavior() {
        let full = IngestProfile::full();
        assert_eq!(full.volumes, VolumeChoice::ALL.to_vec());
        assert_eq!(full.level_step_hpa, 25);
        assert_eq!(full.surface_fields, FieldSet::All);
        assert!(full.derived && full.heavy);
        assert_eq!(full.candidate_levels().len(), 37, "100..=1000 step 25");
        assert_eq!(full.candidate_levels()[0], 100);
        assert_eq!(*full.candidate_levels().last().unwrap(), 1000);
        full.validate().expect("full preset validates");
    }

    #[test]
    fn sounding_preset_stores_volumes_and_seven_surface_fields() {
        let sounding = IngestProfile::sounding();
        assert_eq!(sounding.volumes.len(), 5);
        assert!(!sounding.derived && !sounding.heavy);
        assert!(!sounding.includes_full_2d());
        for name in SOUNDING_SURFACE_FIELDS {
            assert!(
                sounding.includes_surface_field(name),
                "sounding must include '{name}'"
            );
        }
        assert!(!sounding.includes_surface_field("composite_reflectivity"));
        sounding.validate().expect("sounding preset validates");
    }

    #[test]
    fn view_preset_is_all_2d_no_volumes_no_heavy() {
        let view = IngestProfile::view();
        assert!(view.volumes.is_empty());
        assert!(view.includes_full_2d());
        assert!(view.derived && !view.heavy);
        assert!(view.needs_prs(), "view still needs prs for the 2D planes");
        view.validate().expect("view preset validates");
    }

    #[test]
    fn analysis_preset_is_surface_only_and_needs_no_pressure_product() {
        let analysis = IngestProfile::analysis();
        assert!(analysis.volumes.is_empty());
        assert!(!analysis.includes_full_2d());
        assert!(!analysis.derived && !analysis.heavy);
        assert!(!analysis.needs_prs());
        for name in ANALYSIS_SURFACE_FIELDS {
            assert!(
                analysis.includes_surface_field(name),
                "analysis must include '{name}'"
            );
        }
        assert!(!analysis.includes_surface_field("mslp"));
        assert!(!analysis.includes_surface_field("apcp_run_total"));
        analysis.validate().expect("analysis preset validates");
    }

    #[test]
    fn surface_preset_keeps_every_direct_surface_selector_without_pressure() {
        let surface = IngestProfile::surface();
        assert!(surface.volumes.is_empty());
        assert!(!surface.includes_full_2d());
        assert!(!surface.derived && !surface.heavy);
        assert!(!surface.needs_prs());
        assert_eq!(
            surface.surface_fields,
            FieldSet::Named(
                base_surface_plan()
                    .into_iter()
                    .map(|(name, _)| name.to_string())
                    .collect()
            )
        );
        surface.validate().expect("surface preset validates");
    }

    #[test]
    fn cma_geps_plan_is_an_exact_provider_statistics_inventory() {
        use rustwx_core::{CanonicalField, FieldProduct, ProbabilitySelection, VerticalSelector};

        let plan = model_surface_plan(ModelId::CmaGeps);
        assert_eq!(plan.len(), 57);
        let unique_names = plan
            .iter()
            .map(|(name, _)| *name)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(unique_names.len(), plan.len());
        assert!(
            plan.iter()
                .all(|(_, selector)| selector.product != FieldProduct::Default),
            "the CMA lane must never imply deterministic/raw-member fields"
        );
        assert!(plan.contains(&(
            "temperature_2m_p50",
            FieldSelector::height_agl(CanonicalField::Temperature, 2).with_percentile(50),
        )));
        assert!(plan.contains(&(
            "cloud_cover_total_p100",
            FieldSelector::surface(CanonicalField::TotalCloudCover).with_percentile(100),
        )));
        assert!(
            plan.contains(&(
                "wind_speed_10m_probability_gt_15ms",
                FieldSelector::height_agl(CanonicalField::WindSpeed, 10)
                    .with_probability(ProbabilitySelection::new(Some(3), Some(15_000), None)),
            ))
        );
        assert!(plan.iter().any(|(name, selector)| {
            *name == "wind_speed_850hpa_p25"
                && selector.vertical == VerticalSelector::IsobaricHpa(850)
                && selector.product == FieldProduct::Percentile(25)
        }));
        assert_eq!(
            model_surface_plan(ModelId::Hrrr).len(),
            base_surface_plan().len()
        );

        let cma_surface = IngestProfile::surface_for_model(ModelId::CmaGeps);
        assert_eq!(
            cma_surface.surface_fields,
            FieldSet::Named(plan.iter().map(|(name, _)| (*name).to_string()).collect())
        );
        assert!(matches!(
            IngestProfile::surface().surface_fields,
            FieldSet::Named(ref names) if names.len() == 26
        ));
    }

    #[test]
    fn reps_plan_is_scalar_statistics_only_and_preserves_the_three_hour_window() {
        let plan = model_surface_plan(ModelId::Reps);
        assert_eq!(plan.len(), 37);
        assert_eq!(
            plan.iter()
                .map(|(name, _)| *name)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            plan.len()
        );
        assert!(
            plan.iter()
                .all(|(_, selector)| selector.product != FieldProduct::Default)
        );
        assert!(plan.iter().all(|(_, selector)| !matches!(
            selector.field,
            CanonicalField::UWind | CanonicalField::VWind
        )));
        assert!(
            plan.contains(&(
                "temperature_2m_ensemble_spread",
                FieldSelector::height_agl(CanonicalField::Temperature, 2)
                    .with_product(FieldProduct::EnsembleSpread),
            ))
        );
        assert!(plan.contains(&(
            "total_precipitation_3h_p50",
            FieldSelector::surface(CanonicalField::TotalPrecipitation).with_percentile(50),
        )));
        assert!(
            plan.contains(&(
                "total_precipitation_3h_probability_gt_2p5mm",
                FieldSelector::surface(CanonicalField::TotalPrecipitation)
                    .with_probability(ProbabilitySelection::new(Some(3), Some(2_500), None),),
            ))
        );
        assert!(
            plan.iter()
                .filter(|(name, _)| name.starts_with("total_precipitation_3h_"))
                .count()
                == 19
        );
        assert_eq!(
            IngestProfile::surface_for_model(ModelId::Reps).surface_fields,
            FieldSet::Named(plan.iter().map(|(name, _)| (*name).to_string()).collect())
        );
    }

    #[test]
    fn every_sounding_surface_field_is_a_known_plan_name() {
        let plan = surface_plan();
        for name in SOUNDING_SURFACE_FIELDS {
            assert!(
                plan.iter().any(|(have, _)| *have == name),
                "'{name}' must exist in surface_plan()"
            );
        }
    }

    #[test]
    fn level_step_50_yields_19_levels() {
        let mut profile = IngestProfile::sounding();
        profile.level_step_hpa = 50;
        let levels = profile.candidate_levels();
        assert_eq!(levels.len(), 19);
        assert_eq!(levels[0], 100);
        assert_eq!(*levels.last().unwrap(), 1000);
        assert!(levels.windows(2).all(|pair| pair[1] - pair[0] == 50));
    }

    #[test]
    fn validate_rejects_bad_level_step() {
        let mut profile = IngestProfile::full();
        profile.level_step_hpa = 10;
        let message = profile.validate().unwrap_err();
        assert!(message.contains("10 hPa"), "got: {message}");
    }

    #[test]
    fn validate_rejects_duplicate_volumes() {
        let mut profile = IngestProfile::full();
        profile.volumes.push(VolumeChoice::Temperature);
        let message = profile.validate().unwrap_err();
        assert!(message.contains("duplicate volume"), "got: {message}");
        assert!(message.contains("temperature_iso"), "got: {message}");
    }

    #[test]
    fn validate_rejects_empty_named_set_and_unknown_names() {
        let mut profile = IngestProfile::sounding();
        profile.surface_fields = FieldSet::Named(Vec::new());
        let message = profile.validate().unwrap_err();
        assert!(message.contains("empty"), "got: {message}");

        profile.surface_fields = FieldSet::Named(vec!["not_a_field".to_string()]);
        let message = profile.validate().unwrap_err();
        assert!(
            message.contains("unknown surface field 'not_a_field'"),
            "got: {message}"
        );
    }

    #[test]
    fn validate_rejects_derived_or_heavy_on_a_named_subset() {
        let mut profile = IngestProfile::sounding();
        profile.heavy = true;
        let message = profile.validate().unwrap_err();
        assert!(
            message.contains("named surface subset"),
            "heavy on sounding must name the excluded inputs, got: {message}"
        );

        let mut profile = IngestProfile::sounding();
        profile.derived = true;
        assert!(profile.validate().is_err());
    }

    #[test]
    fn validate_rejects_heavy_without_derived() {
        let mut profile = IngestProfile::full();
        profile.derived = false;
        let message = profile.validate().unwrap_err();
        assert!(
            message.contains("heavy stage builds on the derived stage"),
            "got: {message}"
        );
    }

    #[test]
    fn resolve_profile_applies_overrides() {
        let profile = resolve_profile(
            "sounding",
            &ProfileOverrides {
                level_step_hpa: Some(50),
                ..Default::default()
            },
        )
        .expect("sounding @ 50 resolves");
        assert_eq!(profile.level_step_hpa, 50);
        assert_eq!(profile.candidate_levels().len(), 19);

        let profile = resolve_profile(
            "full",
            &ProfileOverrides {
                heavy: Some(false),
                ..Default::default()
            },
        )
        .expect("full --no-heavy resolves");
        assert!(profile.derived && !profile.heavy);

        // --no-derived alone on full leaves heavy dangling: clear error.
        let message = resolve_profile(
            "full",
            &ProfileOverrides {
                no_derived: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(message.contains("--no-heavy"), "got: {message}");

        // --no-derived --no-heavy composes into a plain-extraction full.
        let profile = resolve_profile(
            "full",
            &ProfileOverrides {
                no_derived: true,
                heavy: Some(false),
                ..Default::default()
            },
        )
        .expect("full --no-derived --no-heavy resolves");
        assert!(!profile.derived && !profile.heavy);

        let cma_surface =
            resolve_profile_for_model("surface", &ProfileOverrides::default(), ModelId::CmaGeps)
                .expect("CMA surface profile resolves to its provider statistics");
        assert!(matches!(
            cma_surface.surface_fields,
            FieldSet::Named(ref names) if names.len() == 57
        ));
        let hrrr_surface =
            resolve_profile_for_model("surface", &ProfileOverrides::default(), ModelId::Hrrr)
                .expect("deterministic surface profile keeps the base inventory");
        assert!(matches!(
            hrrr_surface.surface_fields,
            FieldSet::Named(ref names) if names.len() == 26
        ));
    }

    #[test]
    fn resolve_profile_rejects_heavy_on_sounding_with_a_clear_error() {
        let message = resolve_profile(
            "sounding",
            &ProfileOverrides {
                heavy: Some(true),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(message.contains("--profile sounding"), "got: {message}");
        assert!(message.contains("named surface subset"), "got: {message}");
    }

    #[test]
    fn resolve_profile_rejects_unknown_preset_and_bad_step() {
        let message = resolve_profile("everything", &ProfileOverrides::default()).unwrap_err();
        assert!(
            message.contains("unknown preset 'everything'"),
            "got: {message}"
        );

        let message = resolve_profile(
            "full",
            &ProfileOverrides {
                level_step_hpa: Some(30),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(message.contains("--level-step"), "got: {message}");
    }

    #[test]
    fn describe_names_the_shape() {
        assert_eq!(
            IngestProfile::full().describe(),
            "5 volume(s) @ 25 hPa steps (37 levels), all 2D fields, derived on, heavy on"
        );
        assert_eq!(
            IngestProfile::view().describe(),
            "no volumes, all 2D fields, derived on, heavy off"
        );
        assert_eq!(
            IngestProfile::analysis().describe(),
            "no volumes, 9 named surface field(s), derived off, heavy off"
        );
        let mut sounding = IngestProfile::sounding();
        sounding.level_step_hpa = 50;
        assert_eq!(
            sounding.describe(),
            "5 volume(s) @ 50 hPa steps (19 levels), 7 named surface field(s), derived off, heavy off"
        );
    }
}

#![allow(dead_code)]

//! Shared per-hour live-GRIB -> `.rws` ingest flow, used by `rw_ingest`
//! (serial hours) and `rw_batch` (pipelined hours) via `#[path]` inclusion.
//!
//! Per forecast hour: fetch the `prs` product file (cache-aware), extract
//! the full 3D isobaric superset plus the sparse per-level vorticity planes
//! AND every isobaric plane a supported direct plot recipe consumes (stored
//! as bit-exact 2D variables named by selector key — the 3D volume codec is
//! quantized, so render-grade isobaric planes must ride the lossless f32
//! 2D codec) in ONE decode pass, fetch the `sfc` product file, extract the
//! 2D surface set in ONE decode pass (plus one re-select at `hour - 1` for
//! the trailing 1 h window fields: the APCP increment, the native
//! sub-hourly MXUPHL max, and the native sub-hourly WIND 10 m max — the two
//! max fields the GRIB windowed lane consumed), decode the surface +
//! pressure thermo pair through the render lanes' own products decoder, and
//! compute every non-heavy derived recipe grid AND (when `heavy` is set)
//! every heavy ECAPE-class recipe grid from that pair (see `ingest_compute`;
//! stored as ordinary 2D variables named by recipe slug, selector = the
//! `{"derived": ...}` marker), then write the hour into
//! `<store-root>/<model>/<run>/f{hour:03}.rws` plus `grid.rwg` and
//! `run.json` via `rw_store::ingest::write_hour_from_fields_with_derived`.
//!
//! The flow is split at the network/CPU boundary — [`fetch_hour`] (network
//! or cache disk read only) and [`process_fetched_hour`] (extract, derived,
//! heavy, encode — all CPU on the ONE global rayon pool) — so `rw_batch`
//! can run hour N's compute while hour N+1 downloads. [`ingest_hour`]
//! composes the two for serial callers.
//!
//! No `nat` (wrfnat) fetch: every formerly "native-only" 2D field this plan
//! needs (composite reflectivity, 1 km AGL reflectivity, 2-5 km updraft
//! helicity, 8 m AGL smoke, column-integrated smoke, simulated IR) is also
//! carried by the HRRR `sfc` file — verified against the live
//! `hrrr.t00z.wrfsfcf06.grib2.idx` listing on AWS for the 2026-06-08 00z run
//! — so they ride the existing sfc fetch + decode pass for free instead of
//! pulling any of the ~770 MB wrfnat file.

use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rustwx_core::{
    CanonicalField, CycleSpec, FieldSelector, ModelId, ModelRunRequest, SelectedField2D, SourceId,
    VerticalSelector,
};
use rustwx_io::{
    CachedFetchResult, FetchRequest, extract_fields_partial_from_model_bytes_at_forecast_hour,
    fetch_bytes_with_cache,
};
use rustwx_models::plot_recipe_fetch_plan;
use rustwx_products::derived::{store_derived_recipe_slugs, store_heavy_recipe_slugs};
use rustwx_products::direct::supported_direct_recipe_slugs;
use rw_store::grid::GridFile;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, derived_selector, read_field_2d, read_grid_2d,
    write_hour_from_fields_with_derived,
};
use rw_store::reader::HourReader;

#[path = "ingest_compute.rs"]
pub mod ingest_compute;
use ingest_compute::DerivedGrid2D;

/// Candidate isobaric levels (hPa) requested for every 3D variable; absent
/// levels come back in `PartialExtraction.missing` and are simply not stored.
fn candidate_levels() -> Vec<u16> {
    (100..=1000).step_by(25).collect()
}

/// 3D variables pulled from the pressure ("prs") product file, with their
/// stable store names. Dewpoint falls back to RelativeHumidity ("rh_iso")
/// when the file realizes fewer than two dewpoint levels.
///
/// Moisture note: the derived/CAPE compute path (`rustwx_calc::ecape::
/// {SurfaceInputs, EcapeVolumeInputs}`) consumes mixing ratio, which
/// rustwx-products derives from dewpoint (preferred fallback after SPFH,
/// which has no CanonicalField) plus pressure — `dewpoint_iso` here and
/// `dewpoint_2m` + `surface_pressure` on the 2D side cover it, so no
/// dedicated moisture volume is needed.
const VOLUME_PLAN: &[(CanonicalField, &str)] = &[
    (CanonicalField::Temperature, "temperature_iso"),
    (CanonicalField::Dewpoint, "dewpoint_iso"),
    (CanonicalField::UWind, "u_iso"),
    (CanonicalField::VWind, "v_iso"),
    (CanonicalField::GeopotentialHeight, "height_iso"),
];

/// Sparse per-level absolute vorticity planes pulled from the prs file in
/// the same decode pass as the volumes, stored as five 2D variables (the
/// direct plot recipes consume vorticity per-level, and five levels do not
/// justify a 37-level volume slot).
const VORTICITY_PLAN: &[(u16, &str)] = &[
    (200, "absolute_vorticity_200"),
    (300, "absolute_vorticity_300"),
    (500, "absolute_vorticity_500"),
    (700, "absolute_vorticity_700"),
    (850, "absolute_vorticity_850"),
];

/// 2D fields pulled from the surface ("sfc") product file, with their stable
/// store names. These mirror the selector constructors the rustwx-models
/// plot-recipe catalog uses for the same HRRR fields.
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
fn surface_plan() -> Vec<(&'static str, FieldSelector)> {
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
        //     too, so they ride this fetch — see the module doc) ---
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

/// Every isobaric plane a supported direct plot recipe consumes for this
/// model, derived from the recipe catalog's own fetch plans so coverage is
/// provable rather than hand-maintained. These are stored as bit-exact 2D
/// variables named by selector key (e.g. `geopotential_height_500hpa`):
/// the 3D volume codec quantizes to i16 per chunk, so a render-grade plane
/// must ride the lossless f32 2D codec to keep store-rendered plots
/// pixel-identical to the GRIB lane. Absolute vorticity is excluded — it
/// already ships through `VORTICITY_PLAN` under its stable store names.
fn direct_isobaric_plane_selectors(model: ModelId) -> Vec<FieldSelector> {
    let mut selectors = Vec::new();
    for slug in supported_direct_recipe_slugs(model) {
        let Ok(plan) = plot_recipe_fetch_plan(&slug, model) else {
            continue;
        };
        for selector in plan.selectors() {
            if matches!(selector.vertical, VerticalSelector::IsobaricHpa(_))
                && selector.field != CanonicalField::AbsoluteVorticity
                && !selectors.contains(&selector)
            {
                selectors.push(selector);
            }
        }
    }
    selectors
}

/// Everything one ingest pass needs to know, independent of any bin's CLI.
pub struct IngestConfig<'a> {
    pub model: ModelId,
    pub cycle: &'a CycleSpec,
    /// `Some` pins one source; `None` tries every configured source in
    /// catalog order — which also lets a warm raw-byte cache hit no matter
    /// which source originally served the file.
    pub source_override: Option<SourceId>,
    pub cache_root: &'a Path,
    pub use_cache: bool,
    pub store_root: &'a Path,
    pub model_slug: &'a str,
    pub run_slug: &'a str,
    /// Run the heavy ECAPE ingest stage.
    pub heavy: bool,
    /// After the write, re-open the hour and verify a bit-exact 2D
    /// round-trip of every field plus one profile per 3D variable.
    pub verify: bool,
}

/// The network half of one hour: both family files fetched (cache-aware),
/// with per-file fetch walls. Everything here is `Send`, so a fetch thread
/// can hand it across a channel to the CPU half.
pub struct FetchedHour {
    pub hour: u16,
    pub prs: CachedFetchResult,
    pub sfc: CachedFetchResult,
    pub prs_fetch_ms: u128,
    pub sfc_fetch_ms: u128,
}

/// One realized 3D variable summary for reporting.
pub struct VolumeSummary {
    pub name: String,
    pub levels: Vec<u16>,
}

/// One ingested hour: per-stage walls, cache provenance, store stats, and
/// realized/planned counts for honest reporting.
pub struct IngestedHour {
    pub hour: u16,
    pub prs_fetch_ms: u128,
    pub sfc_fetch_ms: u128,
    pub prs_cache_hit: bool,
    pub sfc_cache_hit: bool,
    pub prs_mb: f64,
    pub sfc_mb: f64,
    pub prs_extract_ms: u128,
    pub sfc_extract_ms: u128,
    pub thermo_decode_ms: u128,
    pub derived_ms: u128,
    pub heavy_ms: u128,
    /// Full store-write stage wall (volume assembly + encode + fs write).
    pub write_ms: u128,
    /// The codec's own encode wall, a subset of `write_ms`.
    pub encode_ms: u128,
    /// CPU-half wall: extract through write (and verify when enabled).
    pub process_ms: u128,
    pub store_path: PathBuf,
    pub store_mb: f64,
    pub fields_2d: usize,
    pub planned_2d: usize,
    pub derived: usize,
    pub planned_derived: usize,
    pub heavy: usize,
    pub planned_heavy: usize,
    pub volumes: Vec<VolumeSummary>,
}

impl IngestedHour {
    /// Fetch + process wall (the serial cost of this hour).
    pub fn total_ms(&self) -> u128 {
        self.prs_fetch_ms + self.sfc_fetch_ms + self.process_ms
    }
}

pub fn cache_state(hit: bool) -> &'static str {
    if hit { "cache hit" } else { "cache miss" }
}

/// Fetch both family files for one hour (network or cache disk read; no
/// decode, no rayon).
pub fn fetch_hour(
    config: &IngestConfig<'_>,
    hour: u16,
) -> Result<FetchedHour, Box<dyn std::error::Error>> {
    let prs_fetch = FetchRequest {
        request: ModelRunRequest::new(config.model, config.cycle.clone(), hour, "prs")?,
        source_override: config.source_override,
        variable_patterns: Vec::new(),
    };
    let fetch_started = Instant::now();
    let prs = fetch_bytes_with_cache(&prs_fetch, config.cache_root, config.use_cache)?;
    let prs_fetch_ms = fetch_started.elapsed().as_millis();

    let sfc_fetch = FetchRequest {
        request: ModelRunRequest::new(config.model, config.cycle.clone(), hour, "sfc")?,
        source_override: config.source_override,
        variable_patterns: Vec::new(),
    };
    let fetch_started = Instant::now();
    let sfc = fetch_bytes_with_cache(&sfc_fetch, config.cache_root, config.use_cache)?;
    let sfc_fetch_ms = fetch_started.elapsed().as_millis();

    Ok(FetchedHour {
        hour,
        prs,
        sfc,
        prs_fetch_ms,
        sfc_fetch_ms,
    })
}

/// One owned 3D variable assembled from extraction, before borrowing into
/// `PressureVolumeInput` for the store write.
struct VolumeData {
    name: &'static str,
    field: CanonicalField,
    units: &'static str,
    levels: Vec<(u16, Vec<f32>)>,
}

/// The CPU half of one hour: extract both files, compute derived/heavy
/// grids, write the store hour, and (optionally) verify the round-trip.
/// All parallelism rides the ONE global rayon pool.
pub fn process_fetched_hour(
    config: &IngestConfig<'_>,
    fetched: FetchedHour,
) -> Result<IngestedHour, Box<dyn std::error::Error>> {
    let process_started = Instant::now();
    let FetchedHour {
        hour,
        prs,
        sfc,
        prs_fetch_ms,
        sfc_fetch_ms,
    } = fetched;
    let prs_cache_hit = prs.cache_hit;
    let prs_mb = prs.result.bytes.len() as f64 / (1024.0 * 1024.0);
    let sfc_cache_hit = sfc.cache_hit;
    let sfc_mb = sfc.result.bytes.len() as f64 / (1024.0 * 1024.0);

    // --- prs product file: 3D isobaric superset, one decode pass ---
    let levels = candidate_levels();
    let direct_planes = direct_isobaric_plane_selectors(config.model);
    let mut prs_selectors =
        Vec::with_capacity(VOLUME_PLAN.len() * levels.len() + VORTICITY_PLAN.len());
    for (field, _) in VOLUME_PLAN {
        for &level in &levels {
            prs_selectors.push(FieldSelector::isobaric(*field, level));
        }
    }
    for (level, _) in VORTICITY_PLAN {
        prs_selectors.push(FieldSelector::isobaric(
            CanonicalField::AbsoluteVorticity,
            *level,
        ));
    }
    // Direct-recipe isobaric planes ride the same decode pass; most are
    // already in the volume superset (T/Td/U/V/Z), but e.g. isobaric RH is
    // plane-only and joins the request here.
    for selector in &direct_planes {
        if !prs_selectors.contains(selector) {
            prs_selectors.push(*selector);
        }
    }
    let extract_started = Instant::now();
    let prs_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        config.model,
        &prs.result.bytes,
        Some(&prs.bytes_path),
        &prs_selectors,
        Some(hour),
    )?;
    let mut prs_extract_ms = extract_started.elapsed().as_millis();

    let mut volumes_data: Vec<VolumeData> = VOLUME_PLAN
        .iter()
        .map(|(field, name)| VolumeData {
            name,
            field: *field,
            units: FieldSelector::isobaric(*field, 500).native_units(),
            levels: Vec::new(),
        })
        .collect();
    let mut prs_fields_2d: Vec<(String, SelectedField2D)> = Vec::new();
    for extracted in prs_extraction.extracted {
        let VerticalSelector::IsobaricHpa(level) = extracted.selector.vertical else {
            continue;
        };
        if extracted.selector.field == CanonicalField::AbsoluteVorticity {
            if let Some((_, name)) = VORTICITY_PLAN.iter().find(|(have, _)| *have == level) {
                prs_fields_2d.push((name.to_string(), extracted));
            }
            continue;
        }
        // Direct-recipe planes are stored as bit-exact 2D variables under
        // their selector key, in addition to (not instead of) the volume.
        if direct_planes.contains(&extracted.selector) {
            prs_fields_2d.push((extracted.selector.key(), extracted.clone()));
        }
        if let Some(volume) = volumes_data
            .iter_mut()
            .find(|volume| volume.field == extracted.selector.field)
        {
            // Move the plane out; the per-field grid copy drops here.
            volume.levels.push((level, extracted.values));
        }
    }
    for (level, name) in VORTICITY_PLAN {
        if !prs_fields_2d.iter().any(|(have, _)| have == name) {
            eprintln!(
                "f{hour:03}: 2D field '{name}' (absolute vorticity {level} hPa) \
                 missing from the prs file; skipped"
            );
        }
    }
    for selector in &direct_planes {
        let key = selector.key();
        if !prs_fields_2d.iter().any(|(have, _)| *have == key) {
            eprintln!("f{hour:03}: direct-recipe plane '{key}' missing from the prs file; skipped");
        }
    }

    // Dewpoint fallback: when the prs file realizes < 2 dewpoint levels,
    // re-select RelativeHumidity from the already-fetched bytes (the GRIB
    // index re-parses, but only the RH messages decode).
    let dewpoint_realized = volumes_data
        .iter()
        .find(|volume| volume.field == CanonicalField::Dewpoint)
        .map(|volume| volume.levels.len())
        .unwrap_or(0);
    if dewpoint_realized < 2 {
        eprintln!(
            "f{hour:03}: dewpoint_iso realized only {dewpoint_realized} level(s); \
             falling back to relative humidity (rh_iso)"
        );
        let rh_selectors: Vec<FieldSelector> = levels
            .iter()
            .map(|&level| FieldSelector::isobaric(CanonicalField::RelativeHumidity, level))
            .collect();
        let rh_started = Instant::now();
        let rh_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
            config.model,
            &prs.result.bytes,
            Some(&prs.bytes_path),
            &rh_selectors,
            Some(hour),
        )?;
        prs_extract_ms += rh_started.elapsed().as_millis();
        let dewpoint = volumes_data
            .iter_mut()
            .find(|volume| volume.field == CanonicalField::Dewpoint)
            .expect("dewpoint volume slot exists");
        dewpoint.name = "rh_iso";
        dewpoint.field = CanonicalField::RelativeHumidity;
        dewpoint.units =
            FieldSelector::isobaric(CanonicalField::RelativeHumidity, 500).native_units();
        dewpoint.levels.clear();
        for extracted in rh_extraction.extracted {
            if let VerticalSelector::IsobaricHpa(level) = extracted.selector.vertical {
                dewpoint.levels.push((level, extracted.values));
            }
        }
    }
    // `prs` stays alive: the derived/heavy compute stage decodes the
    // thermo pair from the raw prs + sfc bytes below.

    // --- sfc product file: 2D surface set, one decode pass ---
    let surface_plan = surface_plan();
    let sfc_selectors: Vec<FieldSelector> =
        surface_plan.iter().map(|(_, selector)| *selector).collect();
    let extract_started = Instant::now();
    let mut sfc_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        config.model,
        &sfc.result.bytes,
        Some(&sfc.bytes_path),
        &sfc_selectors,
        Some(hour),
    )?;
    let mut sfc_extract_ms = extract_started.elapsed().as_millis();

    let mut fields_2d_owned: Vec<(String, SelectedField2D)> = Vec::new();
    for (name, selector) in &surface_plan {
        match sfc_extraction
            .extracted
            .iter()
            .position(|field| field.selector == *selector)
        {
            Some(index) => {
                fields_2d_owned.push((
                    name.to_string(),
                    sfc_extraction.extracted.swap_remove(index),
                ));
            }
            None => eprintln!(
                "f{hour:03}: 2D field '{name}' ({}) missing from the sfc file; skipped",
                selector.key()
            ),
        }
    }

    // Trailing (h-1)->h window fields, all selected at `hour - 1` in ONE
    // re-select pass (the GRIB index re-parses, but only these three
    // messages decode):
    //
    // * apcp_1h — both sfc APCP accumulations end at hour h and tie on
    //   match score at hour h, where the run total wins as first in file
    //   order (stored above as `apcp_run_total`). At `hour - 1` only the
    //   trailing window matches: its start hour scores an exact 0 while
    //   the run total's start and end both miss.
    // * uh_2to5km_max_1h — the native sub-hourly MXUPHL max
    //   (`MXUPHL:5000-2000 m above ground:(h-1)-h hour max fcst`), the
    //   exact message the GRIB windowed lane reduced. The start-hour match
    //   pins the statistical message even if a future HRRR build adds an
    //   instantaneous UPHL message (which would win the plain `uh_2to5km`
    //   selection at hour h; in current sfc files MXUPHL is the only 2-5 km
    //   UH message, so `uh_2to5km` is the same plane selected by its
    //   end-hour score).
    // * wind_speed_10m_max_1h — the native sub-hourly
    //   `WIND:10 m above ground:(h-1)-h hour max fcst` field the GRIB
    //   windowed lane consumed; the sfc file carries no instantaneous wind
    //   speed message (only UGRD/VGRD), so this is the only WIND match.
    //
    // LATENT FRAGILITY (review 2026-06-09): apcp_run_total's identity rests
    // on NOAA keeping the 0->h message ahead of the (h-1)->h message in the
    // sfc file. If a future HRRR build reorders them, run_total silently
    // becomes the 1 h window. A windowed-product sanity check (run_total >=
    // 1h sum for h > 1) is the cheap detector if this ever bites.
    if hour >= 1 {
        let trailing_plan = [
            (
                "apcp_1h",
                FieldSelector::surface(CanonicalField::TotalPrecipitation),
            ),
            (
                "uh_2to5km_max_1h",
                FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            ),
            (
                "wind_speed_10m_max_1h",
                FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            ),
        ];
        let trailing_selectors: Vec<FieldSelector> =
            trailing_plan.iter().map(|(_, selector)| *selector).collect();
        let trailing_started = Instant::now();
        let mut trailing_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
            config.model,
            &sfc.result.bytes,
            Some(&sfc.bytes_path),
            &trailing_selectors,
            Some(hour - 1),
        )?;
        sfc_extract_ms += trailing_started.elapsed().as_millis();
        for (name, selector) in &trailing_plan {
            match trailing_extraction
                .extracted
                .iter()
                .position(|field| field.selector == *selector)
            {
                Some(index) => fields_2d_owned.push((
                    name.to_string(),
                    trailing_extraction.extracted.swap_remove(index),
                )),
                None => eprintln!(
                    "f{hour:03}: 2D field '{name}' (trailing 1 h window of {}) \
                     missing from the sfc file; skipped",
                    selector.key()
                ),
            }
        }
    } else {
        eprintln!(
            "f{hour:03}: trailing 1 h window fields (apcp_1h, uh_2to5km_max_1h, \
             wind_speed_10m_max_1h) have no window at analysis; skipped"
        );
    }

    // Sparse prs-sourced planes ride behind the sfc set so the grid carrier
    // stays the first surface-plan field (both files share the HRRR grid;
    // the store write bit-verifies that). The +3 is the trailing 1 h
    // window set (apcp_1h, uh_2to5km_max_1h, wind_speed_10m_max_1h).
    let planned_2d = surface_plan.len() + 3 + VORTICITY_PLAN.len() + direct_planes.len();
    fields_2d_owned.extend(prs_fields_2d);
    if fields_2d_owned.is_empty() {
        return Err(format!(
            "f{hour:03}: no 2D fields realized; cannot write an hour without a grid carrier"
        )
        .into());
    }

    // --- derived + heavy precompute: decode the surface + pressure thermo
    //     pair from the still-resident raw bytes through the render lanes'
    //     own products decoder (same messages, same moisture preference,
    //     same f64 precision — the stored grids are bit-identical to a
    //     render-lane compute over the same files), then run every
    //     non-heavy recipe grid and every heavy ECAPE-class recipe grid ---
    let planned_derived = store_derived_recipe_slugs().len();
    let planned_heavy = store_heavy_recipe_slugs().len();
    let stages = compute_product_grids(&sfc.result.bytes, &prs.result.bytes, hour, config.heavy);
    drop(sfc);
    drop(prs);
    let thermo_decode_ms = stages.decode_ms;
    let derived_ms = stages.derived_ms;
    let heavy_ms = stages.heavy_ms;
    let derived_grids = stages.derived;
    let heavy_grids = stages.heavy;

    // --- assemble + write ---
    let write_started = Instant::now();
    let fields_2d: Vec<(&str, &SelectedField2D)> = fields_2d_owned
        .iter()
        .map(|(name, field)| (name.as_str(), field))
        .collect();
    let mut volumes: Vec<PressureVolumeInput<'_>> = Vec::new();
    for volume in &volumes_data {
        if volume.levels.len() < 2 {
            eprintln!(
                "f{hour:03}: 3D variable '{}' realized {} level(s) (< 2); skipped",
                volume.name,
                volume.levels.len()
            );
            continue;
        }
        volumes.push(PressureVolumeInput {
            name: volume.name,
            units: volume.units,
            selector_template: serde_json::json!({
                "field": volume.field.as_str(),
                "vertical": "isobaric",
            }),
            levels: volume
                .levels
                .iter()
                .map(|(level, plane)| (*level, plane.as_slice()))
                .collect(),
        });
    }

    let derived_inputs: Vec<DerivedFieldInput<'_>> = derived_grids
        .iter()
        .chain(heavy_grids.iter())
        .map(|grid| DerivedFieldInput {
            name: grid.name,
            units: &grid.units,
            values: &grid.values,
        })
        .collect();

    let unix_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| format!("system clock before unix epoch: {err}"))?
        .as_secs();
    let written = write_hour_from_fields_with_derived(
        config.store_root,
        config.model_slug,
        config.run_slug,
        hour,
        &fields_2d,
        &derived_inputs,
        &volumes,
        env!("RW_BUILD_SHA"),
        unix_now,
    )?;
    let write_ms = write_started.elapsed().as_millis();

    if config.verify {
        verify_hour(
            &written.path,
            &fields_2d_owned,
            &derived_grids,
            &heavy_grids,
            &volumes_data,
        )?;
    }

    let volumes_summary = volumes_data
        .iter()
        .filter(|volume| volume.levels.len() >= 2)
        .map(|volume| {
            let mut realized: Vec<u16> = volume.levels.iter().map(|(level, _)| *level).collect();
            realized.sort_unstable();
            VolumeSummary {
                name: volume.name.to_string(),
                levels: realized,
            }
        })
        .collect();

    Ok(IngestedHour {
        hour,
        prs_fetch_ms,
        sfc_fetch_ms,
        prs_cache_hit,
        sfc_cache_hit,
        prs_mb,
        sfc_mb,
        prs_extract_ms,
        sfc_extract_ms,
        thermo_decode_ms,
        derived_ms,
        heavy_ms,
        write_ms,
        encode_ms: u128::from(written.encode_ms),
        process_ms: process_started.elapsed().as_millis(),
        store_path: written.path,
        store_mb: written.bytes as f64 / (1024.0 * 1024.0),
        fields_2d: fields_2d_owned.len(),
        planned_2d,
        derived: derived_grids.len(),
        planned_derived,
        heavy: heavy_grids.len(),
        planned_heavy,
        volumes: volumes_summary,
    })
}

/// Serial fetch + process for one hour (rw_ingest's flow).
pub fn ingest_hour(
    config: &IngestConfig<'_>,
    hour: u16,
) -> Result<IngestedHour, Box<dyn std::error::Error>> {
    let fetched = fetch_hour(config, hour)?;
    process_fetched_hour(config, fetched)
}

/// Both compute stages' outputs for one hour, with per-stage wall times.
/// `decode_ms` is the products-lane thermo pair decode (the stage's input
/// assembly); `derived_ms` is the non-heavy pass; `heavy_ms` is the heavy
/// ECAPE stage alone (height-AGL prep + ECAPE triplet + composites).
#[derive(Default)]
struct ComputedProductGrids {
    derived: Vec<DerivedGrid2D>,
    heavy: Vec<DerivedGrid2D>,
    decode_ms: u128,
    derived_ms: u128,
    heavy_ms: u128,
}

/// Decode the thermo pair from the raw sfc + prs bytes through the render
/// lanes' own products decoder, then run the non-heavy derived pass and
/// (when enabled) the heavy ECAPE pass. Each stage degrades independently
/// to "no variables from this stage", never a failed ingest — the
/// extracted fields still carry the hour.
fn compute_product_grids(
    surface_bytes: &[u8],
    pressure_bytes: &[u8],
    hour: u16,
    heavy_enabled: bool,
) -> ComputedProductGrids {
    let decode_started = Instant::now();
    let inputs = match ingest_compute::decode_products_inputs(surface_bytes, pressure_bytes) {
        Ok(inputs) => inputs,
        Err(err) => {
            eprintln!("f{hour:03}: derived/heavy precompute skipped: thermo decode failed: {err}");
            return ComputedProductGrids::default();
        }
    };
    let decode_ms = decode_started.elapsed().as_millis();

    let derived_started = Instant::now();
    let derived = match ingest_compute::compute_derived_2d_from_inputs(&inputs) {
        Ok(grids) => grids,
        Err(err) => {
            eprintln!("f{hour:03}: derived precompute skipped: {err}");
            Vec::new()
        }
    };
    let derived_ms = derived_started.elapsed().as_millis();

    if !heavy_enabled {
        println!("f{hour:03}: heavy ingest stage skipped (--no-heavy)");
        return ComputedProductGrids {
            derived,
            heavy: Vec::new(),
            decode_ms,
            derived_ms,
            heavy_ms: 0,
        };
    }

    let heavy_started = Instant::now();
    let heavy = match ingest_compute::compute_heavy_2d_from_inputs(&inputs) {
        Ok(heavy) => {
            for (slug, reason) in &heavy.skipped {
                eprintln!("f{hour:03}: heavy recipe '{slug}' skipped: {reason}");
            }
            if heavy.ecape_failure_count > 0 {
                eprintln!(
                    "f{hour:03}: ECAPE triplet failed on {} column(s) (NaN in grids, \
                     same as the render lane)",
                    heavy.ecape_failure_count
                );
            }
            println!(
                "f{hour:03}: heavy breakdown: height-AGL prep {} ms | ECAPE triplet {} ms | \
                 wind diagnostics {} ms | ML classic (STP LCL) {} ms | composites {} ms",
                heavy.timing.prepare_height_agl_ms,
                heavy.timing.kernels.ecape_triplet_ms,
                heavy.timing.kernels.wind_diagnostics_ms,
                heavy.timing.kernels.ml_classic_ms,
                heavy.timing.kernels.composites_ms,
            );
            heavy.grids
        }
        Err(err) => {
            eprintln!("f{hour:03}: heavy precompute skipped: {err}");
            Vec::new()
        }
    };
    let heavy_ms = heavy_started.elapsed().as_millis();

    ComputedProductGrids {
        derived,
        heavy,
        decode_ms,
        derived_ms,
        heavy_ms,
    }
}

/// Re-open the just-written hour: bit-exact round-trip of every 2D field
/// plus the first derived and first heavy variable (via `read_grid_2d`,
/// marker selector checked), plus one center-of-grid profile per 3D
/// variable, each profile value checked against the source plane's center
/// value within the quantization bound.
fn verify_hour(
    hour_path: &Path,
    fields_2d: &[(String, SelectedField2D)],
    derived_grids: &[DerivedGrid2D],
    heavy_grids: &[DerivedGrid2D],
    volumes_data: &[VolumeData],
) -> Result<(), Box<dyn std::error::Error>> {
    let reader = HourReader::open(hour_path)?;
    let grid_path = hour_path
        .parent()
        .ok_or("hour path has no parent directory")?
        .join("grid.rwg");
    let grid = GridFile::open(&grid_path)?;

    // Every extracted 2D field must round-trip bit-exactly — not just the
    // first. (A sub-epsilon constant-tile shortcut in the f32 codec once
    // flattened the 8 m smoke plane while temperature_2m verified clean.)
    for (name, original) in fields_2d {
        let round_trip = read_field_2d(&reader, &grid, name)?;
        let exact = round_trip.values.len() == original.values.len()
            && round_trip
                .values
                .iter()
                .zip(&original.values)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        if !exact {
            return Err(format!("verify: 2D round-trip of '{name}' is not bit-exact").into());
        }
    }
    let name = &fields_2d[0].0;

    let mut derived_note = String::from("no derived vars");
    if let Some(derived) = derived_grids.first() {
        verify_marked_grid(&reader, &grid, derived)?;
        derived_note = format!("derived '{}' bit-exact", derived.name);
    }
    let mut heavy_note = String::from("no heavy vars");
    if let Some(heavy) = heavy_grids.first() {
        verify_marked_grid(&reader, &grid, heavy)?;
        heavy_note = format!("heavy '{}' bit-exact", heavy.name);
    }

    // Integer grid center: at exact integer coordinates the bilinear profile
    // degenerates to the stored column at that point, so each profile value
    // compares directly against the source plane's center value.
    let (cx, cy) = (grid.nx / 2, grid.ny / 2);
    let center = cy * grid.nx + cx;
    let mut profiles = Vec::new();
    for var in &reader.meta().variables {
        if var.kind != "pressure3d" {
            continue;
        }
        let profile = reader.read_profile_3d(&var.name, cx as f64, cy as f64)?;
        if profile.len() != var.levels_hpa.len() {
            return Err(format!(
                "verify: profile of '{}' returned {} values for {} levels",
                var.name,
                profile.len(),
                var.levels_hpa.len()
            )
            .into());
        }
        let source = volumes_data
            .iter()
            .find(|volume| volume.name == var.name)
            .ok_or_else(|| {
                format!(
                    "verify: no source volume for stored variable '{}'",
                    var.name
                )
            })?;
        // Conservative quantization bound: the 3D codec quantizes whole
        // [y][x][z] chunks with one scale per chunk, so every chunk's scale
        // is <= (global variable range) / 65534; half of that is the max
        // rounding error, leaving generous headroom for f32 decode noise.
        let mut vmin = f32::INFINITY;
        let mut vmax = f32::NEG_INFINITY;
        for (_, plane) in &source.levels {
            for value in plane {
                if value.is_finite() {
                    vmin = vmin.min(*value);
                    vmax = vmax.max(*value);
                }
            }
        }
        let bound = if vmax >= vmin {
            (vmax - vmin) / 65534.0 + 1e-3
        } else {
            1e-3 // no finite source values: only NaN-vs-NaN checks remain
        };
        for (k, &level) in var.levels_hpa.iter().enumerate() {
            let (_, plane) = source
                .levels
                .iter()
                .find(|(have, _)| *have == level)
                .ok_or_else(|| {
                    format!(
                        "verify: stored level {level} hPa of '{}' missing from the source volume",
                        var.name
                    )
                })?;
            let expected = plane[center];
            let got = profile[k];
            if expected.is_nan() {
                if !got.is_nan() {
                    return Err(format!(
                        "verify: '{}' {level} hPa at grid center: source is NaN \
                         but profile is {got}",
                        var.name
                    )
                    .into());
                }
                continue;
            }
            let diff = (got - expected).abs();
            if diff.is_nan() || diff > bound {
                return Err(format!(
                    "verify: '{}' {level} hPa at grid center: profile {got} vs source \
                     {expected} exceeds quantization bound {bound}",
                    var.name
                )
                .into());
            }
        }
        profiles.push(format!("{}:{}", var.name, profile.len()));
    }
    println!(
        "  verify ok: all {} 2D fields bit-exact (first '{name}'), {derived_note}, \
         {heavy_note}, profiles at grid center [{}], values within quantization bound",
        fields_2d.len(),
        profiles.join(" ")
    );
    Ok(())
}

/// Bit-exact round-trip of one derived/heavy grid via `read_grid_2d`,
/// including the `{"derived": slug}` marker selector.
fn verify_marked_grid(
    reader: &HourReader,
    grid: &GridFile,
    expected: &DerivedGrid2D,
) -> Result<(), Box<dyn std::error::Error>> {
    let stored = read_grid_2d(reader, grid, expected.name)?;
    if stored.selector != derived_selector(expected.name) {
        return Err(format!(
            "verify: derived '{}' stored selector {} is not the derived marker",
            expected.name, stored.selector
        )
        .into());
    }
    let exact = stored.values.len() == expected.values.len()
        && stored
            .values
            .iter()
            .zip(&expected.values)
            .all(|(a, b)| a.to_bits() == b.to_bits());
    if !exact {
        return Err(format!(
            "verify: derived round-trip of '{}' is not bit-exact",
            expected.name
        )
        .into());
    }
    Ok(())
}

/// Parse the `--hours` spec: a single hour ("6"), an inclusive range
/// ("0-6"), or a comma list of either ("12,0,4-6"). The output is sorted
/// ascending and deduplicated.
pub fn parse_hours(spec: &str) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
    fn parse_token(token: &str) -> Result<u16, Box<dyn std::error::Error>> {
        token.parse().map_err(|_| {
            format!("--hours: invalid token '{token}' (expected N, N-M, or comma list)").into()
        })
    }
    let mut hours = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!(
                "--hours: empty entry in '{spec}' (expected N, N-M, or comma list)"
            )
            .into());
        }
        if let Some((start, end)) = part.split_once('-') {
            let start = parse_token(start.trim())?;
            let end = parse_token(end.trim())?;
            if start > end {
                return Err(format!("--hours: invalid range '{part}': start > end").into());
            }
            hours.extend(start..=end);
        } else {
            hours.push(parse_token(part)?);
        }
    }
    hours.sort_unstable();
    hours.dedup();
    if hours.is_empty() {
        return Err("pass at least one forecast hour via --hours".into());
    }
    Ok(hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hours_single() {
        assert_eq!(parse_hours("6").unwrap(), vec![6]);
    }

    #[test]
    fn parse_hours_degenerate_range() {
        assert_eq!(parse_hours("0-0").unwrap(), vec![0]);
    }

    #[test]
    fn parse_hours_list_sorts_ascending() {
        assert_eq!(parse_hours("12,0,6").unwrap(), vec![0, 6, 12]);
    }

    #[test]
    fn parse_hours_range_is_inclusive() {
        assert_eq!(parse_hours("0-6").unwrap(), (0..=6).collect::<Vec<u16>>());
    }

    #[test]
    fn parse_hours_rejects_junk_naming_the_flag() {
        for spec in ["1x", "6-2", "", ","] {
            let err = parse_hours(spec).unwrap_err();
            let message = err.to_string();
            assert!(
                message.contains("--hours"),
                "error for '{spec}' must name the flag, got: {message}"
            );
        }
        let message = parse_hours("1x").unwrap_err().to_string();
        assert!(
            message.contains("invalid token '1x'"),
            "error must name the offending token, got: {message}"
        );
    }
}

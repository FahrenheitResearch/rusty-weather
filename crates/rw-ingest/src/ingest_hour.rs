//! Shared per-hour live-GRIB -> `.rws` ingest flow, used by `rw_ingest`
//! (serial hours), `rw_batch` (pipelined hours), and the UI shell's
//! ingest worker. Also the parent of the `ingest_profile` (what one run
//! fetches/extracts/computes/stores — the default `full` profile is the
//! behavior described below, unchanged) and `size_estimate` (exact +
//! predictive store/download sizing) child modules.
//!
//! Progress is reported through the [`IngestEvent`] sink on
//! [`IngestConfig`] (stage start/done walls plus the historical
//! stdout/stderr note lines — see [`crate::print_event`]); cancellation is
//! observed at stage boundaries via the config's cancel flag and surfaces
//! as [`IngestError::Cancelled`]. The store write is atomic (temp + rename
//! inside rw-store), so a cancel mid-hour never leaves partial files — at
//! worst the in-flight stage finishes and the hour is dropped before its
//! write.
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
use std::sync::atomic::{AtomicBool, Ordering};
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
#[path = "ingest_profile.rs"]
pub mod ingest_profile;
#[path = "size_estimate.rs"]
pub mod size_estimate;
use crate::events::{IngestError, IngestEvent, IngestStage, other};
use crate::profile_scope;
use ingest_compute::DerivedGrid2D;
use ingest_profile::{IngestProfile, VolumeChoice, surface_plan};

/// The volume plan under one profile: `(field, store name)` pairs in the
/// stable full-ingest order. Dewpoint falls back to RelativeHumidity
/// ("rh_iso") when the file realizes fewer than two dewpoint levels.
///
/// Moisture note: the derived/CAPE compute path (`rustwx_calc::ecape::
/// {SurfaceInputs, EcapeVolumeInputs}`) consumes mixing ratio, which
/// rustwx-products derives from dewpoint (preferred fallback after SPFH,
/// which has no CanonicalField) plus pressure — `dewpoint_iso` here and
/// `dewpoint_2m` + `surface_pressure` on the 2D side cover it, so no
/// dedicated moisture volume is needed.
fn volume_plan(profile: &IngestProfile) -> Vec<(CanonicalField, &'static str)> {
    VolumeChoice::ALL
        .iter()
        .filter(|choice| profile.volumes.contains(choice))
        .map(|choice| (choice.field(), choice.store_name()))
        .collect()
}

/// The trailing (h-1)->h window field names (see the re-select pass in
/// `process_fetched_hour`); stored only under a full-2D profile.
const TRAILING_2D_NAMES: [&str; 3] = ["apcp_1h", "uh_2to5km_max_1h", "wind_speed_10m_max_1h"];

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
    /// What to fetch/extract/compute/store — validated up front (see
    /// `ingest_profile`); `IngestProfile::full()` is today's behavior.
    pub profile: &'a IngestProfile,
    /// After the write, re-open the hour and verify a bit-exact 2D
    /// round-trip of every field plus one profile per 3D variable.
    pub verify: bool,
    /// Progress sink: stage start/done walls plus the historical
    /// stdout/stderr note lines. Bins pass [`crate::print_event`] (which
    /// reproduces the old inline prints byte-for-byte); UI hosts forward
    /// events over a channel. `Sync` because `rw_batch` shares one config
    /// across its fetch/process threads.
    pub progress: &'a (dyn Fn(IngestEvent) + Sync),
    /// Checked at stage boundaries (between the prs and sfc fetches, and
    /// before each CPU stage); when set, the flow returns
    /// [`IngestError::Cancelled`]. Callers without cancellation pass
    /// [`crate::NEVER_CANCEL`].
    pub cancel: &'a AtomicBool,
}

impl IngestConfig<'_> {
    fn emit(&self, event: IngestEvent) {
        (self.progress)(event);
    }

    /// `Err(Cancelled)` once the cancel flag is set; called at every stage
    /// boundary.
    fn check_cancel(&self) -> Result<(), IngestError> {
        if self.cancel.load(Ordering::Relaxed) {
            Err(IngestError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// The network half of one hour: both family files fetched (cache-aware),
/// with per-file fetch walls. Everything here is `Send`, so a fetch thread
/// can hand it across a channel to the CPU half.
#[derive(Debug)]
pub struct FetchedHour {
    pub hour: u16,
    pub prs: CachedFetchResult,
    pub sfc: CachedFetchResult,
    pub prs_fetch_ms: u128,
    pub sfc_fetch_ms: u128,
}

/// One realized 3D variable summary for reporting.
#[derive(Debug, Clone)]
pub struct VolumeSummary {
    pub name: String,
    pub levels: Vec<u16>,
}

/// One ingested hour: per-stage walls, cache provenance, store stats, and
/// realized/planned counts for honest reporting.
#[derive(Debug)]
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
/// decode, no rayon). The cancel flag is observed before each file; a
/// cancel mid-download takes effect once the in-flight file completes
/// (the byte fetch has no abort hook today).
pub fn fetch_hour(config: &IngestConfig<'_>, hour: u16) -> Result<FetchedHour, IngestError> {
    config.check_cancel()?;
    let prs_fetch = FetchRequest {
        request: ModelRunRequest::new(config.model, config.cycle.clone(), hour, "prs")
            .map_err(other)?,
        source_override: config.source_override,
        variable_patterns: Vec::new(),
    };
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::FetchPrs,
    });
    let fetch_started = Instant::now();
    let prs =
        fetch_bytes_with_cache(&prs_fetch, config.cache_root, config.use_cache).map_err(other)?;
    let prs_fetch_ms = fetch_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::FetchPrs,
        ms: prs_fetch_ms,
    });

    config.check_cancel()?;
    let sfc_fetch = FetchRequest {
        request: ModelRunRequest::new(config.model, config.cycle.clone(), hour, "sfc")
            .map_err(other)?,
        source_override: config.source_override,
        variable_patterns: Vec::new(),
    };
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::FetchSfc,
    });
    let fetch_started = Instant::now();
    let sfc =
        fetch_bytes_with_cache(&sfc_fetch, config.cache_root, config.use_cache).map_err(other)?;
    let sfc_fetch_ms = fetch_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::FetchSfc,
        ms: sfc_fetch_ms,
    });

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
/// All parallelism rides the rayon pool of the CALLING thread — the global
/// pool for the bins, or a dedicated below-normal pool when an interactive
/// host runs this inside `ThreadPool::install` (see [`crate::throttle`]).
pub fn process_fetched_hour(
    config: &IngestConfig<'_>,
    fetched: FetchedHour,
) -> Result<IngestedHour, IngestError> {
    // The CLIs validate via resolve_profile; guard programmatic callers too
    // (e.g. derived=false + heavy=true would silently compute derived).
    debug_assert!(
        config.profile.validate().is_ok(),
        "process_fetched_hour called with an unvalidated profile: {:?}",
        config.profile.validate()
    );
    config.check_cancel()?;
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
    // The profile picks the volumes and level step; the render-grade 2D
    // planes (vorticity + direct-recipe) ride only with a full-2D profile.
    let profile = config.profile;
    let levels = profile.candidate_levels();
    let volume_plan = volume_plan(profile);
    let include_full_2d = profile.includes_full_2d();
    let direct_planes = if include_full_2d {
        direct_isobaric_plane_selectors(config.model)
    } else {
        Vec::new()
    };
    let mut prs_selectors =
        Vec::with_capacity(volume_plan.len() * levels.len() + VORTICITY_PLAN.len());
    for (field, _) in &volume_plan {
        for &level in &levels {
            prs_selectors.push(FieldSelector::isobaric(*field, level));
        }
    }
    if include_full_2d {
        for (level, _) in VORTICITY_PLAN {
            prs_selectors.push(FieldSelector::isobaric(
                CanonicalField::AbsoluteVorticity,
                *level,
            ));
        }
    }
    // Direct-recipe isobaric planes ride the same decode pass; most are
    // already in the volume superset (T/Td/U/V/Z), but e.g. isobaric RH is
    // plane-only and joins the request here.
    for selector in &direct_planes {
        if !prs_selectors.contains(selector) {
            prs_selectors.push(*selector);
        }
    }
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::ExtractPrs,
    });
    let extract_started = Instant::now();
    let mut prs_extract_ms = 0u128;
    let mut volumes_data: Vec<VolumeData> = volume_plan
        .iter()
        .map(|(field, name)| VolumeData {
            name,
            field: *field,
            units: FieldSelector::isobaric(*field, 500).native_units(),
            levels: Vec::new(),
        })
        .collect();
    let mut prs_fields_2d: Vec<(String, SelectedField2D)> = Vec::new();
    if !prs_selectors.is_empty() {
        profile_scope!("ingest_extract_prs");
        let prs_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
            config.model,
            &prs.result.bytes,
            Some(&prs.bytes_path),
            &prs_selectors,
            Some(hour),
        )
        .map_err(other)?;
        prs_extract_ms = extract_started.elapsed().as_millis();
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
    }
    if include_full_2d {
        for (level, name) in VORTICITY_PLAN {
            if !prs_fields_2d.iter().any(|(have, _)| have == name) {
                config.emit(IngestEvent::Warning {
                    hour,
                    message: format!(
                        "f{hour:03}: 2D field '{name}' (absolute vorticity {level} hPa) \
                         missing from the prs file; skipped"
                    ),
                });
            }
        }
    }
    for selector in &direct_planes {
        let key = selector.key();
        if !prs_fields_2d.iter().any(|(have, _)| *have == key) {
            config.emit(IngestEvent::Warning {
                hour,
                message: format!(
                    "f{hour:03}: direct-recipe plane '{key}' missing from the prs file; skipped"
                ),
            });
        }
    }

    // Dewpoint fallback: when the profile stores a dewpoint volume but the
    // prs file realizes < 2 dewpoint levels, re-select RelativeHumidity
    // from the already-fetched bytes (the GRIB index re-parses, but only
    // the RH messages decode).
    let dewpoint_planned = volumes_data
        .iter()
        .any(|volume| volume.field == CanonicalField::Dewpoint);
    let dewpoint_realized = volumes_data
        .iter()
        .find(|volume| volume.field == CanonicalField::Dewpoint)
        .map(|volume| volume.levels.len())
        .unwrap_or(0);
    if dewpoint_planned && dewpoint_realized < 2 {
        config.emit(IngestEvent::Warning {
            hour,
            message: format!(
                "f{hour:03}: dewpoint_iso realized only {dewpoint_realized} level(s); \
                 falling back to relative humidity (rh_iso)"
            ),
        });
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
        )
        .map_err(other)?;
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
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::ExtractPrs,
        ms: prs_extract_ms,
    });
    config.check_cancel()?;

    // --- sfc product file: 2D surface set (profile-filtered, plan order
    //     so the grid carrier stays the first surface-plan field), one
    //     decode pass ---
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::ExtractSfc,
    });
    let surface_plan: Vec<(&'static str, FieldSelector)> = surface_plan()
        .into_iter()
        .filter(|(name, _)| profile.includes_surface_field(name))
        .collect();
    let sfc_selectors: Vec<FieldSelector> =
        surface_plan.iter().map(|(_, selector)| *selector).collect();
    let extract_started = Instant::now();
    let mut sfc_extraction = {
        profile_scope!("ingest_extract_sfc");
        extract_fields_partial_from_model_bytes_at_forecast_hour(
            config.model,
            &sfc.result.bytes,
            Some(&sfc.bytes_path),
            &sfc_selectors,
            Some(hour),
        )
        .map_err(other)?
    };
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
            None => config.emit(IngestEvent::Warning {
                hour,
                message: format!(
                    "f{hour:03}: 2D field '{name}' ({}) missing from the sfc file; skipped",
                    selector.key()
                ),
            }),
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
    if hour >= 1 && include_full_2d {
        let trailing_plan = [
            (
                TRAILING_2D_NAMES[0], // apcp_1h
                FieldSelector::surface(CanonicalField::TotalPrecipitation),
            ),
            (
                TRAILING_2D_NAMES[1], // uh_2to5km_max_1h
                FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            ),
            (
                TRAILING_2D_NAMES[2], // wind_speed_10m_max_1h
                FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            ),
        ];
        let trailing_selectors: Vec<FieldSelector> = trailing_plan
            .iter()
            .map(|(_, selector)| *selector)
            .collect();
        let trailing_started = Instant::now();
        let mut trailing_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
            config.model,
            &sfc.result.bytes,
            Some(&sfc.bytes_path),
            &trailing_selectors,
            Some(hour - 1),
        )
        .map_err(other)?;
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
                None => config.emit(IngestEvent::Warning {
                    hour,
                    message: format!(
                        "f{hour:03}: 2D field '{name}' (trailing 1 h window of {}) \
                         missing from the sfc file; skipped",
                        selector.key()
                    ),
                }),
            }
        }
    } else if include_full_2d {
        config.emit(IngestEvent::Warning {
            hour,
            message: format!(
                "f{hour:03}: trailing 1 h window fields (apcp_1h, uh_2to5km_max_1h, \
                 wind_speed_10m_max_1h) have no window at analysis; skipped"
            ),
        });
    }

    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::ExtractSfc,
        ms: sfc_extract_ms,
    });
    config.check_cancel()?;

    // Sparse prs-sourced planes ride behind the sfc set so the grid carrier
    // stays the first surface-plan field (both files share the HRRR grid;
    // the store write bit-verifies that). The +3 is the trailing 1 h
    // window set (apcp_1h, uh_2to5km_max_1h, wind_speed_10m_max_1h);
    // everything past the surface set rides only with a full-2D profile.
    let planned_2d = surface_plan.len()
        + if include_full_2d {
            TRAILING_2D_NAMES.len() + VORTICITY_PLAN.len() + direct_planes.len()
        } else {
            0
        };
    fields_2d_owned.extend(prs_fields_2d);
    if fields_2d_owned.is_empty() {
        return Err(other(format!(
            "f{hour:03}: no 2D fields realized; cannot write an hour without a grid carrier"
        )));
    }

    // --- derived + heavy precompute: decode the surface + pressure thermo
    //     pair from the still-resident raw bytes through the render lanes'
    //     own products decoder (same messages, same moisture preference,
    //     same f64 precision — the stored grids are bit-identical to a
    //     render-lane compute over the same files), then run every
    //     non-heavy recipe grid and every heavy ECAPE-class recipe grid ---
    let planned_derived = if profile.derived {
        store_derived_recipe_slugs().len()
    } else {
        0
    };
    let planned_heavy = if profile.heavy {
        store_heavy_recipe_slugs().len()
    } else {
        0
    };
    // Hand the raw GRIB buffers to the compute stage by value: every
    // earlier consumer (the dewpoint-fallback re-extract and the trailing
    // (h-1) re-select) has already run, and the thermo decode frees each
    // buffer at its last use instead of after both compute stages.
    let stages = compute_product_grids(
        config,
        sfc.result.bytes,
        prs.result.bytes,
        hour,
        profile.derived,
        profile.heavy,
    )?;
    config.check_cancel()?;
    let thermo_decode_ms = stages.decode_ms;
    let derived_ms = stages.derived_ms;
    let heavy_ms = stages.heavy_ms;
    let derived_grids = stages.derived;
    let heavy_grids = stages.heavy;

    // --- assemble + write ---
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::Write,
    });
    let write_started = Instant::now();
    let fields_2d: Vec<(&str, &SelectedField2D)> = fields_2d_owned
        .iter()
        .map(|(name, field)| (name.as_str(), field))
        .collect();
    let mut volumes: Vec<PressureVolumeInput<'_>> = Vec::new();
    for volume in &volumes_data {
        if volume.levels.len() < 2 {
            config.emit(IngestEvent::Warning {
                hour,
                message: format!(
                    "f{hour:03}: 3D variable '{}' realized {} level(s) (< 2); skipped",
                    volume.name,
                    volume.levels.len()
                ),
            });
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
        .map_err(|err| other(format!("system clock before unix epoch: {err}")))?
        .as_secs();
    let written = {
        profile_scope!("ingest_write_hour");
        write_hour_from_fields_with_derived(
            config.store_root,
            config.model_slug,
            config.run_slug,
            hour,
            &fields_2d,
            &derived_inputs,
            &volumes,
            env!("RW_BUILD_SHA"),
            unix_now,
        )
        .map_err(other)?
    };
    let write_ms = write_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::Write,
        ms: write_ms,
    });

    if config.verify {
        config.emit(IngestEvent::StageStarted {
            hour,
            stage: IngestStage::Verify,
        });
        let verify_started = Instant::now();
        verify_hour(
            config,
            hour,
            &written.path,
            &fields_2d_owned,
            &derived_grids,
            &heavy_grids,
            &volumes_data,
        )
        .map_err(other)?;
        config.emit(IngestEvent::StageDone {
            hour,
            stage: IngestStage::Verify,
            ms: verify_started.elapsed().as_millis(),
        });
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
pub fn ingest_hour(config: &IngestConfig<'_>, hour: u16) -> Result<IngestedHour, IngestError> {
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
/// extracted fields still carry the hour. Profiles with both stages off
/// (e.g. `sounding`) skip even the thermo decode. The only error is
/// `Cancelled`: the cancel flag is checked before each compute stage so a
/// cancel never waits out the (long) heavy stage.
fn compute_product_grids(
    config: &IngestConfig<'_>,
    surface_bytes: Vec<u8>,
    pressure_bytes: Vec<u8>,
    hour: u16,
    derived_enabled: bool,
    heavy_enabled: bool,
) -> Result<ComputedProductGrids, IngestError> {
    if !derived_enabled && !heavy_enabled {
        config.emit(IngestEvent::Info {
            hour,
            message: format!("f{hour:03}: derived/heavy compute stages skipped (profile)"),
        });
        return Ok(ComputedProductGrids::default());
    }
    config.check_cancel()?;
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::ThermoDecode,
    });
    let decode_started = Instant::now();
    let inputs = {
        profile_scope!("ingest_thermo_decode");
        match ingest_compute::decode_products_inputs(surface_bytes, pressure_bytes) {
            Ok(inputs) => inputs,
            Err(err) => {
                config.emit(IngestEvent::Warning {
                    hour,
                    message: format!(
                        "f{hour:03}: derived/heavy precompute skipped: thermo decode failed: {err}"
                    ),
                });
                return Ok(ComputedProductGrids::default());
            }
        }
    };
    let decode_ms = decode_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::ThermoDecode,
        ms: decode_ms,
    });

    config.check_cancel()?;
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::Derived,
    });
    let derived_started = Instant::now();
    let derived = {
        profile_scope!("ingest_derived");
        match ingest_compute::compute_derived_2d_from_inputs(&inputs) {
            Ok(grids) => grids,
            Err(err) => {
                config.emit(IngestEvent::Warning {
                    hour,
                    message: format!("f{hour:03}: derived precompute skipped: {err}"),
                });
                Vec::new()
            }
        }
    };
    let derived_ms = derived_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::Derived,
        ms: derived_ms,
    });

    if !heavy_enabled {
        config.emit(IngestEvent::Info {
            hour,
            message: format!("f{hour:03}: heavy ingest stage skipped (profile/--no-heavy)"),
        });
        return Ok(ComputedProductGrids {
            derived,
            heavy: Vec::new(),
            decode_ms,
            derived_ms,
            heavy_ms: 0,
        });
    }

    config.check_cancel()?;
    config.emit(IngestEvent::StageStarted {
        hour,
        stage: IngestStage::Heavy,
    });
    let heavy_started = Instant::now();
    let heavy = {
        profile_scope!("ingest_heavy");
        match ingest_compute::compute_heavy_2d_from_inputs(&inputs) {
            Ok(heavy) => {
                for (slug, reason) in &heavy.skipped {
                    config.emit(IngestEvent::Warning {
                        hour,
                        message: format!("f{hour:03}: heavy recipe '{slug}' skipped: {reason}"),
                    });
                }
                if heavy.ecape_failure_count > 0 {
                    config.emit(IngestEvent::Warning {
                        hour,
                        message: format!(
                            "f{hour:03}: ECAPE triplet failed on {} column(s) (NaN in grids, \
                             same as the render lane)",
                            heavy.ecape_failure_count
                        ),
                    });
                }
                config.emit(IngestEvent::Info {
                    hour,
                    message: format!(
                        "f{hour:03}: heavy breakdown: height-AGL prep {} ms | ECAPE triplet {} ms | \
                         wind diagnostics {} ms | ML classic (STP LCL) {} ms | composites {} ms",
                        heavy.timing.prepare_height_agl_ms,
                        heavy.timing.kernels.ecape_triplet_ms,
                        heavy.timing.kernels.wind_diagnostics_ms,
                        heavy.timing.kernels.ml_classic_ms,
                        heavy.timing.kernels.composites_ms,
                    ),
                });
                heavy.grids
            }
            Err(err) => {
                config.emit(IngestEvent::Warning {
                    hour,
                    message: format!("f{hour:03}: heavy precompute skipped: {err}"),
                });
                Vec::new()
            }
        }
    };
    let heavy_ms = heavy_started.elapsed().as_millis();
    config.emit(IngestEvent::StageDone {
        hour,
        stage: IngestStage::Heavy,
        ms: heavy_ms,
    });

    Ok(ComputedProductGrids {
        derived,
        heavy,
        decode_ms,
        derived_ms,
        heavy_ms,
    })
}

/// Re-open the just-written hour: bit-exact round-trip of every 2D field
/// plus the first derived and first heavy variable (via `read_grid_2d`,
/// marker selector checked), plus one center-of-grid profile per 3D
/// variable, each profile value checked against the source plane's center
/// value within the quantization bound.
#[allow(clippy::too_many_arguments)]
fn verify_hour(
    config: &IngestConfig<'_>,
    hour: u16,
    hour_path: &Path,
    fields_2d: &[(String, SelectedField2D)],
    derived_grids: &[DerivedGrid2D],
    heavy_grids: &[DerivedGrid2D],
    volumes_data: &[VolumeData],
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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
    config.emit(IngestEvent::Info {
        hour,
        message: format!(
            "  verify ok: all {} 2D fields bit-exact (first '{name}'), {derived_note}, \
             {heavy_note}, profiles at grid center [{}], values within quantization bound",
            fields_2d.len(),
            profiles.join(" ")
        ),
    });
    Ok(())
}

/// Bit-exact round-trip of one derived/heavy grid via `read_grid_2d`,
/// including the `{"derived": slug}` marker selector.
fn verify_marked_grid(
    reader: &HourReader,
    grid: &GridFile,
    expected: &DerivedGrid2D,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
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

/// The store variables one profile plans to write per hour, by stable
/// store name — the single source of truth the size estimator prices.
/// `volumes` carries the planned (candidate) level count; 2D names ride in
/// stored order (surface plan, trailing windows, vorticity planes,
/// direct-recipe planes), then the derived and heavy recipe slugs.
pub struct PlannedStoreVariables {
    pub volumes: Vec<(&'static str, usize)>,
    pub fields_2d: Vec<String>,
    pub derived: Vec<&'static str>,
    pub heavy: Vec<&'static str>,
}

/// Resolve one profile into the variables it plans to store for `model`.
/// Predictive by construction: candidate levels are assumed realized (true
/// for HRRR's 25 hPa prs files) and the dewpoint volume keeps its planned
/// `dewpoint_iso` name (the rh_iso fallback is a per-file degradation).
pub fn planned_store_variables(profile: &IngestProfile, model: ModelId) -> PlannedStoreVariables {
    let level_count = profile.candidate_levels().len();
    let volumes = volume_plan(profile)
        .iter()
        .map(|(_, name)| (*name, level_count))
        .collect();
    let mut fields_2d: Vec<String> = surface_plan()
        .iter()
        .filter(|(name, _)| profile.includes_surface_field(name))
        .map(|(name, _)| (*name).to_string())
        .collect();
    if profile.includes_full_2d() {
        fields_2d.extend(TRAILING_2D_NAMES.iter().map(|name| (*name).to_string()));
        fields_2d.extend(VORTICITY_PLAN.iter().map(|(_, name)| (*name).to_string()));
        fields_2d.extend(
            direct_isobaric_plane_selectors(model)
                .iter()
                .map(|selector| selector.key()),
        );
    }
    PlannedStoreVariables {
        volumes,
        fields_2d,
        derived: if profile.derived {
            store_derived_recipe_slugs()
        } else {
            Vec::new()
        },
        heavy: if profile.heavy {
            store_heavy_recipe_slugs()
        } else {
            Vec::new()
        },
    }
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

    /// Every stored 2D variable the FULL ingest plan realizes either
    /// resolves to its production plot styling through the viewer resolver
    /// (`rustwx_products::viewer`) or is a known fallback with no production
    /// fill counterpart (barb inputs, compute inputs, contour-only height
    /// planes, contour-only mslp). The fallback set is pinned exactly: a new
    /// plan entry that silently falls back fails this test.
    #[test]
    fn viewer_style_resolver_covers_the_full_ingest_plan() {
        use rustwx_products::viewer::operational_style_for_store_variable;

        let model = ModelId::Hrrr;
        // (name, selector) for every planned 2D variable, mirroring
        // `process_fetched_hour`'s stored selectors exactly.
        let mut planned: Vec<(String, FieldSelector)> = surface_plan()
            .into_iter()
            .map(|(name, selector)| (name.to_string(), selector))
            .collect();
        planned.extend([
            (
                TRAILING_2D_NAMES[0].to_string(),
                FieldSelector::surface(CanonicalField::TotalPrecipitation),
            ),
            (
                TRAILING_2D_NAMES[1].to_string(),
                FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            ),
            (
                TRAILING_2D_NAMES[2].to_string(),
                FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            ),
        ]);
        for (level, name) in VORTICITY_PLAN {
            planned.push((
                name.to_string(),
                FieldSelector::isobaric(CanonicalField::AbsoluteVorticity, *level),
            ));
        }
        for selector in direct_isobaric_plane_selectors(model) {
            planned.push((selector.key(), selector));
        }

        let mut mapped = Vec::new();
        let mut fallback = Vec::new();
        for (name, selector) in &planned {
            let selector_json = serde_json::to_value(selector).expect("selector json");
            match operational_style_for_store_variable(name, &selector_json, "native", model) {
                Some(style) => {
                    assert!(!style.title.is_empty(), "'{name}' must carry a title");
                    assert!(
                        !style.display_units.is_empty(),
                        "'{name}' must carry display units"
                    );
                    mapped.push(name.clone());
                }
                None => fallback.push(name.clone()),
            }
        }

        // The complete fallback set: u/v barb inputs (surface + isobaric),
        // compute-only inputs, the contour-only geopotential heights, and
        // mslp (production contours mslp; its plot fills the companion
        // 10 m wind speed, so no production colorbar exists for the plane).
        let mut expected_fallback: Vec<String> = vec![
            "u_10m".to_string(),
            "v_10m".to_string(),
            "surface_pressure".to_string(),
            "orography".to_string(),
            "mslp".to_string(),
        ];
        for selector in direct_isobaric_plane_selectors(model) {
            if matches!(
                selector.field,
                CanonicalField::GeopotentialHeight | CanonicalField::UWind | CanonicalField::VWind
            ) {
                expected_fallback.push(selector.key());
            }
        }
        let mut actual_fallback = fallback.clone();
        actual_fallback.sort();
        expected_fallback.sort();
        assert_eq!(
            actual_fallback, expected_fallback,
            "every fallback variable must be a known no-counterpart input"
        );
        assert_eq!(mapped.len() + fallback.len(), planned.len());
        // Pinned inventory counts (2026-06 full HRRR plan): 42 of the 65
        // planned 2D planes carry production styling; the 23 fallbacks are
        // the exact set asserted above.
        assert_eq!(mapped.len(), 42, "mapped 2D plane count");
        assert_eq!(fallback.len(), 23, "fallback 2D plane count");

        // Derived + heavy store grids resolve through their slug markers.
        for slug in store_derived_recipe_slugs()
            .into_iter()
            .chain(store_heavy_recipe_slugs())
        {
            let marker = serde_json::json!({ "derived": slug });
            assert!(
                operational_style_for_store_variable(slug, &marker, "units", model).is_some(),
                "derived/heavy slug '{slug}' must resolve to production styling"
            );
        }

        eprintln!(
            "viewer coverage: {} mapped 2D planes, {} fallback 2D planes, {} derived, {} heavy",
            mapped.len(),
            fallback.len(),
            store_derived_recipe_slugs().len(),
            store_heavy_recipe_slugs().len(),
        );
    }

    #[test]
    fn planned_store_variables_full_is_the_complete_plan() {
        let plan = planned_store_variables(&IngestProfile::full(), ModelId::Hrrr);
        assert_eq!(
            plan.volumes
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>(),
            vec![
                "temperature_iso",
                "dewpoint_iso",
                "u_iso",
                "v_iso",
                "height_iso"
            ],
        );
        assert!(plan.volumes.iter().all(|(_, levels)| *levels == 37));
        // Surface plan + trailing windows + vorticity + direct planes, in
        // stored order with the grid carrier first.
        assert_eq!(plan.fields_2d[0], "temperature_2m");
        assert!(plan.fields_2d.contains(&"apcp_1h".to_string()));
        assert!(
            plan.fields_2d
                .contains(&"absolute_vorticity_500".to_string())
        );
        assert!(
            plan.fields_2d
                .contains(&"geopotential_height_500hpa".to_string()),
            "direct-recipe planes must be planned under their selector keys"
        );
        assert!(plan.derived.contains(&"sbcape"));
        assert!(plan.heavy.contains(&"sbecape"));
    }

    #[test]
    fn planned_store_variables_respect_sounding_and_view() {
        let mut profile = IngestProfile::sounding();
        profile.level_step_hpa = 50;
        let sounding = planned_store_variables(&profile, ModelId::Hrrr);
        assert_eq!(sounding.volumes.len(), 5);
        assert!(sounding.volumes.iter().all(|(_, levels)| *levels == 19));
        assert_eq!(sounding.fields_2d.len(), 7);
        assert_eq!(
            sounding.fields_2d[0], "temperature_2m",
            "grid carrier first"
        );
        assert!(!sounding.fields_2d.contains(&"apcp_1h".to_string()));
        assert!(sounding.derived.is_empty() && sounding.heavy.is_empty());

        let view = planned_store_variables(&IngestProfile::view(), ModelId::Hrrr);
        assert!(view.volumes.is_empty());
        assert!(
            view.fields_2d
                .contains(&"absolute_vorticity_500".to_string())
        );
        assert!(!view.derived.is_empty());
        assert!(view.heavy.is_empty());
    }

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

    /// A pre-set cancel flag stops both halves at their first boundary —
    /// before any network or extraction work — with the distinguishable
    /// `Cancelled` error and no events emitted.
    #[test]
    fn cancel_flag_stops_fetch_and_process_before_any_work() {
        use std::sync::Mutex;

        let cycle = CycleSpec::new("20260608", 0).expect("valid cycle");
        let profile = IngestProfile::sounding();
        let cancel = AtomicBool::new(true);
        let events: Mutex<Vec<IngestEvent>> = Mutex::new(Vec::new());
        let sink = |event: IngestEvent| events.lock().unwrap().push(event);
        let config = IngestConfig {
            model: ModelId::Hrrr,
            cycle: &cycle,
            source_override: None,
            cache_root: Path::new("nonexistent-cache"),
            use_cache: false,
            store_root: Path::new("nonexistent-store"),
            model_slug: "hrrr",
            run_slug: "20260608_00z",
            profile: &profile,
            verify: false,
            progress: &sink,
            cancel: &cancel,
        };

        let err = fetch_hour(&config, 6).expect_err("cancelled fetch must error");
        assert!(err.is_cancelled(), "got: {err}");

        let fetched = FetchedHour {
            hour: 6,
            prs: rustwx_io::CachedFetchResult {
                result: rustwx_io::FetchResult {
                    source: SourceId::Aws,
                    url: String::new(),
                    bytes: Vec::new(),
                },
                cache_hit: true,
                bytes_path: PathBuf::new(),
                metadata_path: PathBuf::new(),
            },
            sfc: rustwx_io::CachedFetchResult {
                result: rustwx_io::FetchResult {
                    source: SourceId::Aws,
                    url: String::new(),
                    bytes: Vec::new(),
                },
                cache_hit: true,
                bytes_path: PathBuf::new(),
                metadata_path: PathBuf::new(),
            },
            prs_fetch_ms: 0,
            sfc_fetch_ms: 0,
        };
        let err = process_fetched_hour(&config, fetched).expect_err("cancelled process");
        assert!(err.is_cancelled(), "got: {err}");
        assert!(
            events.lock().unwrap().is_empty(),
            "a pre-set cancel must stop the flow before any stage event"
        );
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

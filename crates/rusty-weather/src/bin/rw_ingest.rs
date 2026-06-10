//! Live GRIB -> `.rws` store ingest for HRRR-class models.
//!
//! Per forecast hour: fetch the `prs` product file (cache-aware), extract the
//! full 3D isobaric superset plus the sparse per-level vorticity planes in
//! ONE decode pass, fetch the `sfc` product file, extract the 2D surface set
//! in ONE decode pass (plus one re-select for the trailing 1 h APCP window),
//! compute every non-heavy derived recipe grid while the extracted fields
//! are still in RAM (see `ingest_compute`; stored as ordinary 2D variables
//! named by recipe slug, selector = the `{"derived": ...}` marker), then
//! write the hour into `<store-root>/<model>/<run>/f{hour:03}.rws` plus
//! `grid.rwg` and `run.json` via
//! `rw_store::ingest::write_hour_from_fields_with_derived`.
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

use clap::Parser;
use rustwx_core::{
    CanonicalField, CycleSpec, FieldSelector, ModelId, ModelRunRequest, SelectedField2D, SourceId,
    VerticalSelector,
};
use rustwx_io::{
    FetchRequest, extract_fields_partial_from_model_bytes_at_forecast_hour, fetch_bytes_with_cache,
};
use rustwx_models::model_summary;
use rustwx_products::cache::{default_proof_cache_dir, ensure_dir};
use rustwx_products::derived::store_derived_recipe_slugs;
use rw_store::grid::GridFile;
use rw_store::ingest::{
    DerivedFieldInput, PressureVolumeInput, derived_selector, read_field_2d, read_grid_2d,
    write_hour_from_fields_with_derived,
};
use rw_store::reader::HourReader;

#[path = "../ingest_compute.rs"]
mod ingest_compute;
use ingest_compute::{DerivedGrid2D, IngestVolumes, MoistureKind};

/// The derived CAPE kernels allocate per-column scratch across every rayon
/// thread; mimalloc handles that churn better than the default Windows heap
/// (measured ~10% on the derived stage and ~15% on GRIB extraction).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

#[derive(Debug, Parser)]
#[command(
    name = "rw-ingest",
    about = "Ingest live model GRIB output into the per-hour .rws store"
)]
struct Args {
    #[arg(long, default_value = "hrrr")]
    model: ModelId,
    #[arg(long, help = "Run date as YYYYMMDD")]
    date: String,
    #[arg(long, help = "Run cycle hour UTC (0-23)")]
    cycle: u8,
    #[arg(long, help = "Forecast hours: \"0\", \"0,6,12\", or \"0-6\"")]
    hours: String,
    #[arg(long)]
    source: Option<SourceId>,
    #[arg(long, default_value = "store")]
    store_root: PathBuf,
    #[arg(long)]
    cache_dir: Option<PathBuf>,
    #[arg(long, default_value_t = false)]
    no_cache: bool,
    #[arg(
        long,
        default_value_t = false,
        help = "After each write, re-open the hour and verify a 2D round-trip and one profile per 3D variable"
    )]
    verify: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    run(&args)
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    let hours = parse_hours(&args.hours)?;
    let cache_root = args
        .cache_dir
        .clone()
        .unwrap_or_else(|| default_proof_cache_dir(Path::new("out")));
    if !args.no_cache {
        ensure_dir(&cache_root)?;
    }
    let source = args
        .source
        .unwrap_or(model_summary(args.model).sources[0].id);
    let cycle = CycleSpec::new(args.date.clone(), args.cycle)?;
    let run_slug = format!("{}_{:02}z", args.date, args.cycle);
    let model_slug = args.model.as_str().replace('-', "_");

    println!(
        "rw_ingest build {} | model {} run {} | source {} | store {} | cache {}",
        env!("RW_BUILD_SHA"),
        model_slug,
        run_slug,
        source,
        args.store_root.display(),
        cache_root.display(),
    );
    for &hour in &hours {
        ingest_hour(
            args,
            &cycle,
            source,
            &cache_root,
            &model_slug,
            &run_slug,
            hour,
        )?;
    }
    println!(
        "{}",
        args.store_root
            .join(&model_slug)
            .join(&run_slug)
            .join("run.json")
            .display()
    );
    Ok(())
}

/// One owned 3D variable assembled from extraction, before borrowing into
/// `PressureVolumeInput` for the store write.
struct VolumeData {
    name: &'static str,
    field: CanonicalField,
    units: &'static str,
    levels: Vec<(u16, Vec<f32>)>,
}

#[allow(clippy::too_many_arguments)]
fn ingest_hour(
    args: &Args,
    cycle: &CycleSpec,
    source: SourceId,
    cache_root: &Path,
    model_slug: &str,
    run_slug: &str,
    hour: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    let hour_started = Instant::now();
    let use_cache = !args.no_cache;

    // --- prs product file: 3D isobaric superset, one decode pass ---
    let prs_fetch = FetchRequest {
        request: ModelRunRequest::new(args.model, cycle.clone(), hour, "prs")?,
        source_override: Some(source),
        variable_patterns: Vec::new(),
    };
    let fetch_started = Instant::now();
    let prs = fetch_bytes_with_cache(&prs_fetch, cache_root, use_cache)?;
    let prs_fetch_ms = fetch_started.elapsed().as_millis();
    let prs_cache_hit = prs.cache_hit;
    let prs_mb = prs.result.bytes.len() as f64 / (1024.0 * 1024.0);

    let levels = candidate_levels();
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
    let extract_started = Instant::now();
    let prs_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        args.model,
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
    let mut prs_fields_2d: Vec<(&'static str, SelectedField2D)> = Vec::new();
    for extracted in prs_extraction.extracted {
        let VerticalSelector::IsobaricHpa(level) = extracted.selector.vertical else {
            continue;
        };
        if extracted.selector.field == CanonicalField::AbsoluteVorticity {
            if let Some((_, name)) = VORTICITY_PLAN.iter().find(|(have, _)| *have == level) {
                prs_fields_2d.push((name, extracted));
            }
            continue;
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
            args.model,
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
    drop(prs);

    // --- sfc product file: 2D surface set, one decode pass ---
    let sfc_fetch = FetchRequest {
        request: ModelRunRequest::new(args.model, cycle.clone(), hour, "sfc")?,
        source_override: Some(source),
        variable_patterns: Vec::new(),
    };
    let fetch_started = Instant::now();
    let sfc = fetch_bytes_with_cache(&sfc_fetch, cache_root, use_cache)?;
    let sfc_fetch_ms = fetch_started.elapsed().as_millis();
    let sfc_cache_hit = sfc.cache_hit;
    let sfc_mb = sfc.result.bytes.len() as f64 / (1024.0 * 1024.0);

    let surface_plan = surface_plan();
    let sfc_selectors: Vec<FieldSelector> =
        surface_plan.iter().map(|(_, selector)| *selector).collect();
    let extract_started = Instant::now();
    let mut sfc_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
        args.model,
        &sfc.result.bytes,
        Some(&sfc.bytes_path),
        &sfc_selectors,
        Some(hour),
    )?;
    let mut sfc_extract_ms = extract_started.elapsed().as_millis();

    let mut fields_2d_owned: Vec<(&'static str, SelectedField2D)> = Vec::new();
    for (name, selector) in &surface_plan {
        match sfc_extraction
            .extracted
            .iter()
            .position(|field| field.selector == *selector)
        {
            Some(index) => {
                fields_2d_owned.push((name, sfc_extraction.extracted.swap_remove(index)));
            }
            None => eprintln!(
                "f{hour:03}: 2D field '{name}' ({}) missing from the sfc file; skipped",
                selector.key()
            ),
        }
    }

    // apcp_1h: both sfc APCP accumulations end at hour h, tie on match
    // score, and the run total wins as first in file order (stored above as
    // `apcp_run_total`). Re-selecting at `hour - 1` matches only the
    // trailing (h-1)->h window — its start hour scores an exact 0 while the
    // run total's start and end both miss — isolating the 1 h accumulation.
    // The GRIB index re-parses, but only the one APCP message decodes.
    //
    // LATENT FRAGILITY (review 2026-06-09): apcp_run_total's identity rests
    // on NOAA keeping the 0->h message ahead of the (h-1)->h message in the
    // sfc file. If a future HRRR build reorders them, run_total silently
    // becomes the 1 h window. A windowed-product sanity check (run_total >=
    // 1h sum for h > 1) is the cheap detector if this ever bites.
    if hour >= 1 {
        let apcp_selectors = [FieldSelector::surface(CanonicalField::TotalPrecipitation)];
        let apcp_started = Instant::now();
        let apcp_extraction = extract_fields_partial_from_model_bytes_at_forecast_hour(
            args.model,
            &sfc.result.bytes,
            Some(&sfc.bytes_path),
            &apcp_selectors,
            Some(hour - 1),
        )?;
        sfc_extract_ms += apcp_started.elapsed().as_millis();
        match apcp_extraction.extracted.into_iter().next() {
            Some(field) => fields_2d_owned.push(("apcp_1h", field)),
            None => eprintln!(
                "f{hour:03}: 2D field 'apcp_1h' (trailing 1 h APCP window) \
                 missing from the sfc file; skipped"
            ),
        }
    } else {
        eprintln!("f{hour:03}: 2D field 'apcp_1h' has no trailing 1 h window at analysis; skipped");
    }
    drop(sfc);

    // Sparse prs-sourced planes ride behind the sfc set so the grid carrier
    // stays the first surface-plan field (both files share the HRRR grid;
    // the store write bit-verifies that).
    let planned_2d = surface_plan.len() + 1 + VORTICITY_PLAN.len();
    fields_2d_owned.extend(prs_fields_2d);
    if fields_2d_owned.is_empty() {
        return Err(format!(
            "f{hour:03}: no 2D fields realized; cannot write an hour without a grid carrier"
        )
        .into());
    }

    // --- derived precompute: every non-heavy recipe grid, while the
    //     extracted inputs are still in RAM (no refetch, no redecode) ---
    let planned_derived = store_derived_recipe_slugs().len();
    let derived_started = Instant::now();
    let derived_grids = compute_derived_grids(&fields_2d_owned, &volumes_data, hour);
    let derived_ms = derived_started.elapsed().as_millis();

    // --- assemble + write ---
    let fields_2d: Vec<(&str, &SelectedField2D)> = fields_2d_owned
        .iter()
        .map(|(name, field)| (*name, field))
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
        &args.store_root,
        model_slug,
        run_slug,
        hour,
        &fields_2d,
        &derived_inputs,
        &volumes,
        env!("RW_BUILD_SHA"),
        unix_now,
    )?;

    let total_ms = hour_started.elapsed().as_millis();
    let volume_summary = volumes_data
        .iter()
        .filter(|volume| volume.levels.len() >= 2)
        .map(|volume| format!("{}:{}", volume.name, volume.levels.len()))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "f{hour:03}: prs fetch {prs_fetch_ms} ms ({}, {prs_mb:.1} MB) | sfc fetch {sfc_fetch_ms} ms ({}, {sfc_mb:.1} MB) | extract prs {prs_extract_ms} ms, sfc {sfc_extract_ms} ms | derived {derived_ms} ms | encode {} ms | total {total_ms} ms | {} {:.1} MB | 2d {}/{} | derived {}/{planned_derived} | 3d {volume_summary}",
        cache_state(prs_cache_hit),
        cache_state(sfc_cache_hit),
        written.encode_ms,
        written.path.display(),
        written.bytes as f64 / (1024.0 * 1024.0),
        fields_2d.len(),
        planned_2d,
        derived_grids.len(),
    );
    for volume in &volumes_data {
        if volume.levels.len() >= 2 {
            let mut realized: Vec<u16> = volume.levels.iter().map(|(level, _)| *level).collect();
            realized.sort_unstable();
            println!("  {} levels (hPa): {realized:?}", volume.name);
        }
    }

    if args.verify {
        verify_hour(
            &written.path,
            &fields_2d_owned,
            &derived_grids,
            &volumes_data,
        )?;
    }
    Ok(())
}

fn cache_state(hit: bool) -> &'static str {
    if hit { "cache hit" } else { "cache miss" }
}

/// Route the extracted hour into the derived precompute lane: pick each
/// volume slot by canonical field (the moisture slot is dewpoint_iso, or
/// rh_iso after the extraction fallback) and run all non-heavy recipes.
/// Failures degrade to "no derived variables this hour", never a failed
/// ingest — the extracted fields still carry the hour.
fn compute_derived_grids(
    fields_2d: &[(&'static str, SelectedField2D)],
    volumes_data: &[VolumeData],
    hour: u16,
) -> Vec<DerivedGrid2D> {
    let volume_levels = |field: CanonicalField| {
        volumes_data
            .iter()
            .find(|volume| volume.field == field && volume.levels.len() >= 2)
            .map(|volume| volume.levels.as_slice())
    };
    let missing = |name: &str| {
        eprintln!(
            "f{hour:03}: derived precompute skipped: 3D variable '{name}' \
             realized < 2 levels"
        );
        Vec::new()
    };
    let Some(temperature_k) = volume_levels(CanonicalField::Temperature) else {
        return missing("temperature_iso");
    };
    let Some(u_ms) = volume_levels(CanonicalField::UWind) else {
        return missing("u_iso");
    };
    let Some(v_ms) = volume_levels(CanonicalField::VWind) else {
        return missing("v_iso");
    };
    let Some(height_m) = volume_levels(CanonicalField::GeopotentialHeight) else {
        return missing("height_iso");
    };
    let (moisture, moisture_kind) = match volume_levels(CanonicalField::Dewpoint) {
        Some(levels) => (levels, MoistureKind::DewpointK),
        None => match volume_levels(CanonicalField::RelativeHumidity) {
            Some(levels) => (levels, MoistureKind::RelativeHumidityPct),
            None => return missing("dewpoint_iso/rh_iso"),
        },
    };
    match ingest_compute::compute_derived_2d(
        fields_2d,
        &IngestVolumes {
            temperature_k,
            moisture,
            moisture_kind,
            u_ms,
            v_ms,
            height_m,
        },
    ) {
        Ok(grids) => grids,
        Err(err) => {
            eprintln!("f{hour:03}: derived precompute skipped: {err}");
            Vec::new()
        }
    }
}

/// Re-open the just-written hour: bit-exact round-trip of the first 2D field
/// and the first derived variable (via `read_grid_2d`, marker selector
/// checked), plus one center-of-grid profile per 3D variable, each profile
/// value checked against the source plane's center value within the
/// quantization bound.
fn verify_hour(
    hour_path: &Path,
    fields_2d: &[(&'static str, SelectedField2D)],
    derived_grids: &[DerivedGrid2D],
    volumes_data: &[VolumeData],
) -> Result<(), Box<dyn std::error::Error>> {
    let reader = HourReader::open(hour_path)?;
    let grid_path = hour_path
        .parent()
        .ok_or("hour path has no parent directory")?
        .join("grid.rwg");
    let grid = GridFile::open(&grid_path)?;

    let (name, original) = &fields_2d[0];
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

    let mut derived_note = String::from("no derived vars");
    if let Some(derived) = derived_grids.first() {
        let stored = read_grid_2d(&reader, &grid, derived.name)?;
        if stored.selector != derived_selector(derived.name) {
            return Err(format!(
                "verify: derived '{}' stored selector {} is not the derived marker",
                derived.name, stored.selector
            )
            .into());
        }
        let exact = stored.values.len() == derived.values.len()
            && stored
                .values
                .iter()
                .zip(&derived.values)
                .all(|(a, b)| a.to_bits() == b.to_bits());
        if !exact {
            return Err(format!(
                "verify: derived round-trip of '{}' is not bit-exact",
                derived.name
            )
            .into());
        }
        derived_note = format!("derived '{}' bit-exact", derived.name);
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
            if !((got - expected).abs() <= bound) {
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
        "  verify ok: '{name}' bit-exact, {derived_note}, profiles at grid center [{}], \
         values within quantization bound",
        profiles.join(" ")
    );
    Ok(())
}

/// Parse the `--hours` spec: a single hour ("6"), an inclusive range
/// ("0-6"), or a comma list of either ("12,0,4-6"). The output is sorted
/// ascending and deduplicated.
fn parse_hours(spec: &str) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
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

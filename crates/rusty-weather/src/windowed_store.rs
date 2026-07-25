#![allow(dead_code)]

//! Windowed products (multi-hour accumulations and extrema) computed FROM
//! THE STORE across per-hour `.rws` files, mirroring the GRIB windowed
//! lane's semantics (`rustwx_products::windowed` + `windowed_decoder`)
//! product for product:
//!
//! * QPF — `qpf_1h` and `qpf_total` read the trailing 1 h / run-total APCP
//!   accumulations the ingest stored from the anchor hour's sfc file
//!   (`apcp_1h`, `apcp_run_total`): the GRIB lane's "direct" strategy. The
//!   fixed trailing windows (`qpf_6h`/`12h`/`24h`) sum stored hourly
//!   `apcp_1h` increments, exactly the GRIB lane's HRRR path (HRRR never
//!   carries 6/12/24 h APCP messages, so that lane always summed hourly
//!   increments too). Millimeters fold first, inches out — the GRIB lane's
//!   conversion order.
//! * 2-5 km UH — pointwise maxima of the stored sub-hourly 1 h max planes
//!   (`uh_2to5km_max_1h`, the native MXUPHL message selected at its window
//!   start hour), the exact field the GRIB windowed lane reduced. Hours
//!   ingested before the max field existed fall back to the stored hourly
//!   `uh_2to5km` plane, with the fallback hours named in the strategy
//!   note. (In current HRRR sfc files that plane is itself the MXUPHL
//!   message — the file carries no instantaneous UPHL, so plain selection
//!   matched MXUPHL by its end-hour score — but the note stays
//!   conservative: a store written from a file that DOES carry
//!   instantaneous UPHL holds top-of-hour snapshots, a lower bound on the
//!   sub-hourly max.)
//! * 10 m wind — pointwise maxima of `wind_speed_10m_max_1h` (the native
//!   sub-hourly `WIND:10 m above ground` max field the GRIB lane
//!   consumed); m/s folds first, knots out. Hours without the stored max
//!   field fall back to top-of-hour hypot(`u_10m`, `v_10m`) speeds — a
//!   genuine lower bound on the sub-hourly max (the sfc file carries no
//!   instantaneous wind-speed message), named in the strategy note.
//! * 2 m temp/RH/dewpoint/VPD — pointwise max/min/range over the fixed
//!   F001-F024 / F025-F048 / F001-F048 snapshot windows. Temperature and
//!   dewpoint convert K -> degC per hour before the fold and RH clamps to
//!   0..100, mirroring `surface_snapshot_values_for_hour`; VPD reads the
//!   ingest-computed `vpd_2m` derived grid (hPa) instead of recomputing
//!   from temp + RH.
//!
//! Gap handling mirrors the GRIB lane's blocker pattern exactly: a window
//! realizes only when EVERY contributing hour is present — in the store
//! AND carrying the source variable(s) in the expected units. A missing
//! middle hour blocks the product with a reason naming the gap; it is
//! never silently skipped. Window minimums (e.g. 24 h products need F024)
//! reuse the lane's planning blockers verbatim, with the anchor hour = the
//! run's max stored hour.
//!
//! Memory: accumulations stream hour by hour — each hour file is opened
//! once, each needed source plane is read once (`read_full_2d`, ~3.6 ms)
//! and folded into every per-product accumulator that wants it; no
//! per-hour plane outlives its hour iteration.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use rustwx_products::windowed::HrrrWindowedProduct;
use rw_store::error::RwStoreError;
use rw_store::grid::GridFile;
use rw_store::ingest::read_grid_2d;
use rw_store::reader::HourReader;
use rw_store::run::{RwsRunManifest, SCHEMA_RUN};

pub(crate) const MM_PER_INCH: f64 = 25.4;
pub(crate) const MS_TO_KT: f64 = 1.943_844_5;

/// One realized windowed product grid: display values (already in display
/// units) on the full run grid, plus the metadata the windowed render path
/// stamps into subtitles and reports.
#[derive(Debug, Clone)]
pub struct WindowedGrid {
    pub slug: String,
    pub units: String,
    pub title: String,
    pub values: Vec<f64>,
    pub hours_used: Vec<u16>,
    pub window_hours: Option<u16>,
    pub strategy: String,
}

/// Outcome of one windowed compute pass: realized grids in request order,
/// blocked products as `(slug, reason)` (window minimum not met, an hour
/// missing from the store, a source variable missing from an hour file, or
/// unexpected stored units), and the anchor hour trailing windows ended at.
#[derive(Debug)]
pub struct WindowedStoreOutcome {
    pub grids: Vec<WindowedGrid>,
    pub blockers: Vec<(String, String)>,
    pub anchor_hour: u16,
}

/// Forecast hours registered in the run's `run.json` manifest, ascending.
pub fn stored_run_hours(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
) -> Result<Vec<u16>, Box<dyn std::error::Error>> {
    let path = store_root.join(model_slug).join(run_slug).join("run.json");
    let bytes = std::fs::read(&path).map_err(|err| format!("read {}: {err}", path.display()))?;
    let manifest: RwsRunManifest =
        serde_json::from_slice(&bytes).map_err(|err| format!("parse {}: {err}", path.display()))?;
    if manifest.schema != SCHEMA_RUN {
        return Err(format!(
            "{}: unexpected schema '{}' (expected '{SCHEMA_RUN}')",
            path.display(),
            manifest.schema
        )
        .into());
    }
    Ok(manifest.hours.keys().copied().collect())
}

/// Compute the requested windowed products from the stored hour files of
/// `<store_root>/<model_slug>/<run_slug>/`.
///
/// Two anchoring rules, because the windowed family mixes two window shapes:
///
/// * **Trailing windows** (`qpf_1h`/`6h`/`12h`/`24h`, `qpf_total`, the 1 h/3 h
///   UH and wind maxima) mean "the window ENDING at hour N", so they honor
///   `anchor_override` — the interactive path passes the requested forecast
///   hour so a user can scrub precip hour-by-hour.
/// * **Run-scoped windows** — the fixed 0-24/24-48/0-48 h snapshot windows and
///   the `*_run_max` products — describe the RUN, not a requested hour. They
///   always anchor at the max stored hour. Anchoring these to the requested
///   hour is what regression `dbd322f` did: it blocked every 0-24/24-48/0-48 h
///   product unless the caller happened to ask for an hour past the window end,
///   and silently truncated `*_run_max` at the requested hour.
///
/// Unknown slugs are an error (the caller validates requests against
/// `HrrrWindowedProduct::supported_products()`); windows that do not fit the
/// available hours come back as blockers, never as silently shortened windows.
pub fn compute_windowed_products(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    available_hours: &[u16],
    requested: &[String],
    anchor_override: Option<u16>,
) -> Result<WindowedStoreOutcome, Box<dyn std::error::Error>> {
    let available: BTreeSet<u16> = available_hours.iter().copied().collect();
    let Some(&run_anchor) = available.iter().next_back() else {
        return Err("windowed compute needs at least one stored hour".into());
    };
    let trailing_anchor = match anchor_override {
        Some(hour) => {
            if !available.contains(&hour) {
                return Err(format!(
                    "windowed anchor F{hour:03} is not a stored hour of this run"
                )
                .into());
            }
            hour
        }
        None => run_anchor,
    };
    let run_dir = store_root.join(model_slug).join(run_slug);
    let grid_path = run_dir.join("grid.rwg");
    let grid =
        GridFile::open(&grid_path).map_err(|err| format!("open {}: {err}", grid_path.display()))?;

    // Plan: dedupe slugs (mirroring the GRIB lane), block products whose
    // window minimum exceeds the anchor or whose window has store gaps.
    let mut blockers: Vec<(String, String)> = Vec::new();
    let mut accums: Vec<Accum> = Vec::new();
    let mut seen = BTreeSet::new();
    // Effective anchor across realized products, used for the reported
    // `anchor_hour` (output naming / render `forecast_hour`): a lone trailing
    // product names itself after the requested hour, while any run-scoped
    // product pulls it back to the run anchor as before.
    let mut effective_anchor = 0u16;
    for slug in requested {
        if !seen.insert(slug.as_str()) {
            continue;
        }
        let product = HrrrWindowedProduct::from_slug(slug)
            .ok_or_else(|| format!("'{slug}' is not a windowed product slug"))?;
        let anchor_hour = if product_is_run_scoped(product) {
            run_anchor
        } else {
            trailing_anchor
        };
        let spec = match plan_product(product, anchor_hour) {
            Ok(spec) => spec,
            Err(reason) => {
                blockers.push((slug.clone(), reason));
                continue;
            }
        };
        effective_anchor = effective_anchor.max(anchor_hour);
        let missing: Vec<u16> = spec
            .hours
            .iter()
            .copied()
            .filter(|hour| !available.contains(hour))
            .collect();
        if missing.is_empty() {
            accums.push(Accum::new(spec));
        } else {
            blockers.push((
                slug.clone(),
                format!(
                    "missing stored hour(s) {} (window F{:03}-F{:03} needs every hour; \
                     gaps are never skipped)",
                    missing
                        .iter()
                        .map(|hour| format!("F{hour:03}"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    spec.hours.first().copied().unwrap_or(anchor_hour),
                    spec.hours.last().copied().unwrap_or(anchor_hour),
                ),
            ));
        }
    }

    // Which source planes each hour must serve, across live products.
    let mut hours_needed: BTreeMap<u16, BTreeSet<SourceKind>> = BTreeMap::new();
    for accum in &accums {
        for &hour in &accum.spec.hours {
            hours_needed
                .entry(hour)
                .or_default()
                .insert(accum.spec.source);
        }
    }

    // Stream: one HourReader per hour, one read per (hour, source plane),
    // folded into every accumulator that wants it. Ascending hour order is
    // the BTreeMap iteration order, mirroring the GRIB lane's hour order.
    for (&hour, kinds) in &hours_needed {
        let needs = |accum: &Accum, kind: SourceKind| {
            accum.failed.is_none() && accum.spec.source == kind && accum.spec.hours.contains(&hour)
        };
        if !accums
            .iter()
            .any(|accum| kinds.iter().any(|&kind| needs(accum, kind)))
        {
            continue;
        }
        let hour_path = run_dir.join(format!("f{hour:03}.rws"));
        let reader = match HourReader::open(&hour_path) {
            Ok(reader) => reader,
            Err(err) => {
                let reason = format!("open {}: {err}", hour_path.display());
                for accum in accums.iter_mut() {
                    if accum.failed.is_none() && accum.spec.hours.contains(&hour) {
                        accum.failed = Some(reason.clone());
                    }
                }
                continue;
            }
        };
        for &kind in kinds {
            if !accums.iter().any(|accum| needs(accum, kind)) {
                continue;
            }
            match read_source_plane(&reader, &grid, kind, hour) {
                Ok(plane) => {
                    for accum in accums.iter_mut() {
                        if needs(accum, kind) {
                            accum.fold(&plane.values, hour);
                            if plane.instantaneous_fallback {
                                accum.fallback_hours.push(hour);
                            }
                        }
                    }
                }
                Err(reason) => {
                    for accum in accums.iter_mut() {
                        if needs(accum, kind) {
                            accum.failed = Some(reason.clone());
                        }
                    }
                }
            }
        }
    }

    let mut grids = Vec::with_capacity(accums.len());
    for accum in accums {
        let slug = accum.spec.product.slug().to_string();
        match accum.finish() {
            Ok(grid) => grids.push(grid),
            Err(reason) => blockers.push((slug, reason)),
        }
    }
    Ok(WindowedStoreOutcome {
        grids,
        blockers,
        // Nothing realized (every product blocked) -> report the run anchor.
        anchor_hour: if effective_anchor == 0 {
            run_anchor
        } else {
            effective_anchor
        },
    })
}

/// The stored source plane a windowed product reduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SourceKind {
    /// `apcp_1h` (kg/m^2 == mm), the trailing (h-1)->h accumulation.
    Apcp1h,
    /// `apcp_run_total` (kg/m^2 == mm), the 0->h run accumulation.
    ApcpRunTotal,
    /// `uh_2to5km_max_1h` (m^2/s^2), the native sub-hourly MXUPHL max;
    /// falls back to the stored hourly `uh_2to5km` plane when the max
    /// field is absent (stores ingested before it existed).
    Uh2to5km,
    /// `wind_speed_10m_max_1h` (m/s), the native sub-hourly WIND max;
    /// falls back to top-of-hour hypot(`u_10m`, `v_10m`) when absent.
    WindSpeed10m,
    /// `temperature_2m` converted K -> degC per hour.
    Temp2mC,
    /// `rh_2m` clamped to 0..100 %.
    Rh2mPct,
    /// `dewpoint_2m` converted K -> degC per hour.
    Dewpoint2mC,
    /// `vpd_2m` (hPa), the ingest-computed derived grid.
    Vpd2mHpa,
    /// `smoke_8m` (kg/m^3), near-surface smoke mass density.
    Smoke8m,
    /// `smoke_column` (kg/m^2), column-integrated smoke.
    SmokeColumn,
    /// `wind_gust_10m` (m/s) — day-window source plane.
    WinWindGust10m,
    /// `hdw` (hPa*m/s) — day-window source plane.
    WinHdw,
    /// `fire_weather_composite` (index) — day-window source plane.
    WinFireWeatherComposite,
    /// `visibility` (m) — day-window source plane.
    WinVisibility,
    /// `dewpoint_depression_2m` (degC) — day-window source plane.
    WinDewpointDepression2m,
    /// `heat_index_2m` (degC) — day-window source plane.
    WinHeatIndex2m,
    /// `apparent_temperature_2m` (degC) — day-window source plane.
    WinApparentTemperature2m,
    /// `wetbulb_2m` (degC) — day-window source plane.
    WinWetbulb2m,
    /// `wind_chill_2m` (degC) — day-window source plane.
    WinWindChill2m,
    /// `composite_reflectivity` (dBZ) — day-window source plane.
    WinCompositeReflectivity,
    /// `sbcape` (J/kg) — day-window source plane.
    WinSbcape,
    /// `mlcape` (J/kg) — day-window source plane.
    WinMlcape,
    /// `mucape` (J/kg) — day-window source plane.
    WinMucape,
    /// `dcape` (J/kg) — day-window source plane.
    WinDcape,
    /// `pwat` (kg/m^2) — day-window source plane.
    WinPwat,
    /// `theta_e_2m_10m_winds` (K) — day-window source plane.
    WinThetaE2m10mWinds,
    /// `srh_0_1km` (m^2/s^2) — day-window source plane.
    WinSrh01km,
    /// `srh_0_3km` (m^2/s^2) — day-window source plane.
    WinSrh03km,
    /// `stp_fixed` (dimensionless) — day-window source plane.
    WinStpFixed,
    /// `ehi_0_1km` (dimensionless) — day-window source plane.
    WinEhi01km,
    /// `ehi_0_3km` (dimensionless) — day-window source plane.
    WinEhi03km,
    /// `scp_mu_0_3km_0_6km_proxy` (dimensionless) — day-window source plane.
    WinScpMu03km06kmProxy,
    /// `bulk_shear_0_1km` (kt) — day-window source plane.
    WinBulkShear01km,
    /// `bulk_shear_0_6km` (kt) — day-window source plane.
    WinBulkShear06km,
    /// `lapse_rate_0_3km` (degC/km) — day-window source plane.
    WinLapseRate03km,
    /// `lapse_rate_700_500` (degC/km) — day-window source plane.
    WinLapseRate700500,
    /// `sblcl` (m) — day-window source plane.
    WinSblcl,
    /// `cloud_cover_total` (%) — day-window source plane.
    WinCloudCoverTotal,
    /// `mslp` (Pa) — day-window source plane.
    WinMslp,
    /// `categorical_rain` (0/1) — day-window source plane.
    WinCategoricalRain,
    /// `categorical_snow` (0/1) — day-window source plane.
    WinCategoricalSnow,
    /// `categorical_freezing_rain` (0/1) — day-window source plane.
    WinCategoricalFreezingRain,
}

/// How the per-hour planes reduce into the product grid.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Reduce {
    /// Single stored plane (1 h / run-total accumulations, 1 h UH/wind).
    Direct,
    Sum,
    Max,
    Min,
    /// Pointwise max - min over the window.
    Range,
    /// Number of hours in the window that meet a threshold. This is a COUNT of
    /// hours in ONE deterministic run — it is never a probability.
    Count(Threshold),
    /// Longest run of CONSECUTIVE hours meeting a threshold. Separates one long
    /// event from the same number of scattered hours, which a count cannot.
    LongestRun(Threshold),
    /// Earliest hour meeting a threshold (onset); NaN where never met.
    FirstHour(Threshold),
    /// Latest hour meeting a threshold (end); NaN where never met.
    LastHour(Threshold),
    /// Hour at which the window maximum occurs, gated by a floor so noise
    /// near zero does not produce a confetti map. First hour wins ties.
    PeakHour(Threshold),
}

/// A threshold test against the STORED plane values (before `Finish`
/// conversion), so thresholds are expressed in the store's units.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Threshold {
    value: f64,
    /// `true` counts hours at or above `value`; `false` at or below.
    at_or_above: bool,
}

impl Threshold {
    const fn above(value: f64) -> Self {
        Self {
            value,
            at_or_above: true,
        }
    }

    const fn below(value: f64) -> Self {
        Self {
            value,
            at_or_above: false,
        }
    }

    fn met(self, value: f64) -> bool {
        if !value.is_finite() {
            return false;
        }
        if self.at_or_above {
            value >= self.value
        } else {
            value <= self.value
        }
    }
}

/// Display-unit conversion applied AFTER the fold (the GRIB lane's order:
/// QPF sums millimeters then divides; wind maxes m/s then multiplies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    None,
    MmToInches,
    MsToKnots,
    /// Near-surface smoke `kg/m^3 -> ug/m^3` (x1e9), matching the direct
    /// lane's `UnitConvert::KgM3ToUgM3` so the shared smoke palette lines up.
    KgM3ToUgM3,
    /// Column smoke `kg/m^2 -> mg/m^2` (x1e6), matching
    /// `UnitConvert::KgM2ToMgM2`.
    KgM2ToMgM2,
    /// Visibility `m -> miles`, matching `UnitConvert::MetersToMiles`.
    MetersToMiles,
    /// Pressure `Pa -> hPa`, matching `UnitConvert::PaToHpa`.
    PaToHpa,
}

#[derive(Debug, Clone)]
struct ProductSpec {
    product: HrrrWindowedProduct,
    source: SourceKind,
    reduce: Reduce,
    /// Contributing hours, ascending; every one of them is required.
    hours: Vec<u16>,
    window_hours: Option<u16>,
    units: &'static str,
    finish: Finish,
    strategy: String,
}

/// Mirror of the GRIB lane's `plan_windowed_products` + per-kernel window
/// definitions for one product, anchored at the max stored hour. `Err` is
/// the planning blocker reason (same wording as the GRIB lane where the
/// constraint is identical).
fn plan_product(product: HrrrWindowedProduct, end: u16) -> Result<ProductSpec, String> {
    use HrrrWindowedProduct::*;
    if let Some(plan) = snapshot_plan(product) {
        if end < plan.window_end {
            return Err(format!(
                "{} requires forecast hour >= {}; use a HRRR extended cycle for 24-48 h products",
                plan.blocker_label, plan.window_end
            ));
        }
        return Ok(ProductSpec {
            product,
            source: plan.source,
            reduce: plan.reduce,
            hours: (plan.window_start..=plan.window_end).collect(),
            window_hours: Some(plan.window_hours),
            units: plan.units,
            finish: Finish::None,
            strategy: format!(
                "pointwise {} of stored hourly {} snapshots across {}",
                plan.op_label, plan.field_label, plan.window_label
            ),
        });
    }

    let spec = |source, reduce, hours: Vec<u16>, window_hours, units, finish, strategy| {
        Ok(ProductSpec {
            product,
            source,
            reduce,
            hours,
            window_hours,
            units,
            finish,
            strategy,
        })
    };
    let qpf_sum = |window: u16| {
        if end < window {
            return Err(format!("{window}-h QPF requires forecast hour >= {window}"));
        }
        spec(
            SourceKind::Apcp1h,
            Reduce::Sum,
            (end + 1 - window..=end).collect(),
            Some(window),
            "in",
            Finish::MmToInches,
            format!("sum of {window} stored hourly APCP increments (apcp_1h)"),
        )
    };
    match product {
        Qpf1h => {
            if end < 1 {
                return Err(
                    "1-h QPF requires forecast hour >= 1 because HRRR APCP windows start at 0-1 h"
                        .to_string(),
                );
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Direct,
                vec![end],
                Some(1),
                "in",
                Finish::MmToInches,
                format!("stored trailing 1 h APCP accumulation (apcp_1h) at F{end:03}"),
            )
        }
        Qpf6h => qpf_sum(6),
        Qpf12h => qpf_sum(12),
        Qpf24h => qpf_sum(24),
        QpfTotal => {
            if end < 1 {
                return Err("total QPF requires forecast hour >= 1".to_string());
            }
            spec(
                SourceKind::ApcpRunTotal,
                Reduce::Direct,
                vec![end],
                None,
                "in",
                Finish::MmToInches,
                format!(
                    "stored run-total APCP accumulation (apcp_run_total, 0-{end} h) at F{end:03}"
                ),
            )
        }
        Uh25km1h => {
            if end < 1 {
                return Err(
                    "1-h UH max requires forecast hour >= 1 because native UH windows start at 0-1 h"
                        .to_string(),
                );
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Direct,
                vec![end],
                Some(1),
                "m^2/s^2",
                Finish::None,
                format!(
                    "stored sub-hourly 1 h max 2-5 km UH plane (uh_2to5km_max_1h) at F{end:03}"
                ),
            )
        }
        Uh25km3h => {
            if end < 3 {
                return Err("3-h UH max requires forecast hour >= 3".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Max,
                (end - 2..=end).collect(),
                Some(3),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored sub-hourly 1 h max 2-5 km UH planes across \
                 trailing 3 hours"
                    .to_string(),
            )
        }
        Uh25kmRunMax => {
            if end < 1 {
                return Err("run-max UH requires forecast hour >= 1".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Max,
                (1..=end).collect(),
                None,
                "m^2/s^2",
                Finish::None,
                "run max of stored sub-hourly 1 h max 2-5 km UH planes".to_string(),
            )
        }
        Wind10m1hMax => {
            if end < 1 {
                return Err(
                    "1-h 10 m wind max requires forecast hour >= 1 because native wind max windows start at 0-1 h"
                        .to_string(),
                );
            }
            spec(
                SourceKind::WindSpeed10m,
                Reduce::Direct,
                vec![end],
                Some(1),
                "kt",
                Finish::MsToKnots,
                format!(
                    "stored sub-hourly 1 h max 10 m wind speed (wind_speed_10m_max_1h) at F{end:03}"
                ),
            )
        }
        Wind10mRunMax => {
            if end < 1 {
                return Err("run-max 10 m wind requires forecast hour >= 1".to_string());
            }
            spec(
                SourceKind::WindSpeed10m,
                Reduce::Max,
                (1..=end).collect(),
                None,
                "kt",
                Finish::MsToKnots,
                "run max of stored sub-hourly 1 h max 10 m wind speeds".to_string(),
            )
        }
        Wind10m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 10 m wind max requires forecast hour >= 24".to_string());
            }
            spec(
                SourceKind::WindSpeed10m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "kt",
                Finish::MsToKnots,
                "max of stored sub-hourly 1 h max 10 m wind speeds across F001-F024".to_string(),
            )
        }
        Wind10m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 10 m wind max requires forecast hour >= 48".to_string());
            }
            spec(
                SourceKind::WindSpeed10m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "kt",
                Finish::MsToKnots,
                "max of stored sub-hourly 1 h max 10 m wind speeds across F025-F048".to_string(),
            )
        }
        Wind10m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 10 m wind max requires forecast hour >= 48".to_string());
            }
            spec(
                SourceKind::WindSpeed10m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "kt",
                Finish::MsToKnots,
                "max of stored sub-hourly 1 h max 10 m wind speeds across F001-F048".to_string(),
            )
        }
        // Peak smoke over a fixed window. Planned inline rather than through
        // `snapshot_plan` because smoke carries a display-unit conversion
        // (snapshot plans are all Finish::None). Max only — smoke is
        // heavy-tailed and ~0 over most of the domain, so min/range say nothing.
        Smoke8m0to24hMax | Smoke8m24to48hMax | Smoke8m0to48hMax
        | SmokeColumn0to24hMax | SmokeColumn24to48hMax | SmokeColumn0to48hMax => {
            let (start, stop, window_hours) = match product {
                Smoke8m0to24hMax | SmokeColumn0to24hMax => (1u16, 24u16, 24u16),
                Smoke8m24to48hMax | SmokeColumn24to48hMax => (25, 48, 24),
                _ => (1, 48, 48),
            };
            let column = matches!(
                product,
                SmokeColumn0to24hMax | SmokeColumn24to48hMax | SmokeColumn0to48hMax
            );
            let label = if window_hours == 48 {
                "0-48 h"
            } else if start == 1 {
                "0-24 h"
            } else {
                "24-48 h"
            };
            let field = if column {
                "column-integrated smoke"
            } else {
                "near-surface smoke"
            };
            if end < stop {
                return Err(format!(
                    "{label} {field} max requires forecast hour >= {stop}; \
                     use a HRRR extended cycle for 24-48 h products"
                ));
            }
            spec(
                if column {
                    SourceKind::SmokeColumn
                } else {
                    SourceKind::Smoke8m
                },
                Reduce::Max,
                (start..=stop).collect(),
                Some(window_hours),
                if column { "mg/m^2" } else { "ug/m^3" },
                if column {
                    Finish::KgM2ToMgM2
                } else {
                    Finish::KgM3ToUgM3
                },
                format!("pointwise max of stored hourly {field} across F{start:03}-F{stop:03}"),
            )
        }
        Gust10m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 10 m wind gust max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "kt",
                Finish::MsToKnots,
                "pointwise max of stored hourly wind_gust_10m across F001-F024".to_string(),
            )
        }
        Gust10m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 10 m wind gust max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "kt",
                Finish::MsToKnots,
                "pointwise max of stored hourly wind_gust_10m across F025-F048".to_string(),
            )
        }
        Gust10m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 10 m wind gust max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "kt",
                Finish::MsToKnots,
                "pointwise max of stored hourly wind_gust_10m across F001-F048".to_string(),
            )
        }
        Hdw0to24hMax => {
            if end < 24 {
                return Err("0-24 h hot-dry-windy index max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "hPa*m/s",
                Finish::None,
                "pointwise max of stored hourly hdw across F001-F024".to_string(),
            )
        }
        Hdw24to48hMax => {
            if end < 48 {
                return Err("24-48 h hot-dry-windy index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "hPa*m/s",
                Finish::None,
                "pointwise max of stored hourly hdw across F025-F048".to_string(),
            )
        }
        Hdw0to48hMax => {
            if end < 48 {
                return Err("0-48 h hot-dry-windy index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "hPa*m/s",
                Finish::None,
                "pointwise max of stored hourly hdw across F001-F048".to_string(),
            )
        }
        FireWxComposite0to24hMax => {
            if end < 24 {
                return Err("0-24 h fire weather composite max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinFireWeatherComposite,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "index",
                Finish::None,
                "pointwise max of stored hourly fire_weather_composite across F001-F024".to_string(),
            )
        }
        FireWxComposite24to48hMax => {
            if end < 48 {
                return Err("24-48 h fire weather composite max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinFireWeatherComposite,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "index",
                Finish::None,
                "pointwise max of stored hourly fire_weather_composite across F025-F048".to_string(),
            )
        }
        FireWxComposite0to48hMax => {
            if end < 48 {
                return Err("0-48 h fire weather composite max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinFireWeatherComposite,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "index",
                Finish::None,
                "pointwise max of stored hourly fire_weather_composite across F001-F048".to_string(),
            )
        }
        Visibility0to24hMin => {
            if end < 24 {
                return Err("0-24 h visibility min requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Min,
                (1..=24).collect(),
                Some(24),
                "mi",
                Finish::MetersToMiles,
                "pointwise min of stored hourly visibility across F001-F024".to_string(),
            )
        }
        Visibility24to48hMin => {
            if end < 48 {
                return Err("24-48 h visibility min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Min,
                (25..=48).collect(),
                Some(24),
                "mi",
                Finish::MetersToMiles,
                "pointwise min of stored hourly visibility across F025-F048".to_string(),
            )
        }
        Visibility0to48hMin => {
            if end < 48 {
                return Err("0-48 h visibility min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Min,
                (1..=48).collect(),
                Some(48),
                "mi",
                Finish::MetersToMiles,
                "pointwise min of stored hourly visibility across F001-F048".to_string(),
            )
        }
        DewpointDepression2m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 2 m dewpoint depression max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDewpointDepression2m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly dewpoint_depression_2m across F001-F024".to_string(),
            )
        }
        DewpointDepression2m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 2 m dewpoint depression max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDewpointDepression2m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly dewpoint_depression_2m across F025-F048".to_string(),
            )
        }
        DewpointDepression2m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 2 m dewpoint depression max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDewpointDepression2m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC",
                Finish::None,
                "pointwise max of stored hourly dewpoint_depression_2m across F001-F048".to_string(),
            )
        }
        HeatIndex2m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 2 m heat index max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly heat_index_2m across F001-F024".to_string(),
            )
        }
        HeatIndex2m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 2 m heat index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly heat_index_2m across F025-F048".to_string(),
            )
        }
        HeatIndex2m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 2 m heat index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC",
                Finish::None,
                "pointwise max of stored hourly heat_index_2m across F001-F048".to_string(),
            )
        }
        ApparentTemp2m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 2 m apparent temperature max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinApparentTemperature2m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly apparent_temperature_2m across F001-F024".to_string(),
            )
        }
        ApparentTemp2m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 2 m apparent temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinApparentTemperature2m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly apparent_temperature_2m across F025-F048".to_string(),
            )
        }
        ApparentTemp2m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 2 m apparent temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinApparentTemperature2m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC",
                Finish::None,
                "pointwise max of stored hourly apparent_temperature_2m across F001-F048".to_string(),
            )
        }
        Wetbulb2m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 2 m wet-bulb temperature max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly wetbulb_2m across F001-F024".to_string(),
            )
        }
        Wetbulb2m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 2 m wet-bulb temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise max of stored hourly wetbulb_2m across F025-F048".to_string(),
            )
        }
        Wetbulb2m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 2 m wet-bulb temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC",
                Finish::None,
                "pointwise max of stored hourly wetbulb_2m across F001-F048".to_string(),
            )
        }
        WindChill2m0to24hMin => {
            if end < 24 {
                return Err("0-24 h 2 m wind chill min requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindChill2m,
                Reduce::Min,
                (1..=24).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise min of stored hourly wind_chill_2m across F001-F024".to_string(),
            )
        }
        WindChill2m24to48hMin => {
            if end < 48 {
                return Err("24-48 h 2 m wind chill min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindChill2m,
                Reduce::Min,
                (25..=48).collect(),
                Some(24),
                "degC",
                Finish::None,
                "pointwise min of stored hourly wind_chill_2m across F025-F048".to_string(),
            )
        }
        WindChill2m0to48hMin => {
            if end < 48 {
                return Err("0-48 h 2 m wind chill min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindChill2m,
                Reduce::Min,
                (1..=48).collect(),
                Some(48),
                "degC",
                Finish::None,
                "pointwise min of stored hourly wind_chill_2m across F001-F048".to_string(),
            )
        }
        CompositeReflectivity0to24hMax => {
            if end < 24 {
                return Err("0-24 h composite reflectivity max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "dBZ",
                Finish::None,
                "pointwise max of stored hourly composite_reflectivity across F001-F024".to_string(),
            )
        }
        CompositeReflectivity24to48hMax => {
            if end < 48 {
                return Err("24-48 h composite reflectivity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "dBZ",
                Finish::None,
                "pointwise max of stored hourly composite_reflectivity across F025-F048".to_string(),
            )
        }
        CompositeReflectivity0to48hMax => {
            if end < 48 {
                return Err("0-48 h composite reflectivity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "dBZ",
                Finish::None,
                "pointwise max of stored hourly composite_reflectivity across F001-F048".to_string(),
            )
        }
        Sbcape0to24hMax => {
            if end < 24 {
                return Err("0-24 h surface-based cape max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSbcape,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly sbcape across F001-F024".to_string(),
            )
        }
        Sbcape24to48hMax => {
            if end < 48 {
                return Err("24-48 h surface-based cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSbcape,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly sbcape across F025-F048".to_string(),
            )
        }
        Sbcape0to48hMax => {
            if end < 48 {
                return Err("0-48 h surface-based cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSbcape,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly sbcape across F001-F048".to_string(),
            )
        }
        Mlcape0to24hMax => {
            if end < 24 {
                return Err("0-24 h mixed-layer cape max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMlcape,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mlcape across F001-F024".to_string(),
            )
        }
        Mlcape24to48hMax => {
            if end < 48 {
                return Err("24-48 h mixed-layer cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMlcape,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mlcape across F025-F048".to_string(),
            )
        }
        Mlcape0to48hMax => {
            if end < 48 {
                return Err("0-48 h mixed-layer cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMlcape,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mlcape across F001-F048".to_string(),
            )
        }
        Mucape0to24hMax => {
            if end < 24 {
                return Err("0-24 h most-unstable cape max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mucape across F001-F024".to_string(),
            )
        }
        Mucape24to48hMax => {
            if end < 48 {
                return Err("24-48 h most-unstable cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mucape across F025-F048".to_string(),
            )
        }
        Mucape0to48hMax => {
            if end < 48 {
                return Err("0-48 h most-unstable cape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly mucape across F001-F048".to_string(),
            )
        }
        Dcape0to24hMax => {
            if end < 24 {
                return Err("0-24 h dcape max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDcape,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly dcape across F001-F024".to_string(),
            )
        }
        Dcape24to48hMax => {
            if end < 48 {
                return Err("24-48 h dcape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDcape,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly dcape across F025-F048".to_string(),
            )
        }
        Dcape0to48hMax => {
            if end < 48 {
                return Err("0-48 h dcape max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinDcape,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "J/kg",
                Finish::None,
                "pointwise max of stored hourly dcape across F001-F048".to_string(),
            )
        }
        Pwat0to24hMax => {
            if end < 24 {
                return Err("0-24 h precipitable water max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinPwat,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "kg/m^2",
                Finish::None,
                "pointwise max of stored hourly pwat across F001-F024".to_string(),
            )
        }
        Pwat24to48hMax => {
            if end < 48 {
                return Err("24-48 h precipitable water max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinPwat,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "kg/m^2",
                Finish::None,
                "pointwise max of stored hourly pwat across F025-F048".to_string(),
            )
        }
        Pwat0to48hMax => {
            if end < 48 {
                return Err("0-48 h precipitable water max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinPwat,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "kg/m^2",
                Finish::None,
                "pointwise max of stored hourly pwat across F001-F048".to_string(),
            )
        }
        ThetaE2m0to24hMax => {
            if end < 24 {
                return Err("0-24 h 2 m equivalent potential temperature max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinThetaE2m10mWinds,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "K",
                Finish::None,
                "pointwise max of stored hourly theta_e_2m_10m_winds across F001-F024".to_string(),
            )
        }
        ThetaE2m24to48hMax => {
            if end < 48 {
                return Err("24-48 h 2 m equivalent potential temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinThetaE2m10mWinds,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "K",
                Finish::None,
                "pointwise max of stored hourly theta_e_2m_10m_winds across F025-F048".to_string(),
            )
        }
        ThetaE2m0to48hMax => {
            if end < 48 {
                return Err("0-48 h 2 m equivalent potential temperature max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinThetaE2m10mWinds,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "K",
                Finish::None,
                "pointwise max of stored hourly theta_e_2m_10m_winds across F001-F048".to_string(),
            )
        }
        Srh01km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-1 km storm-relative helicity max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh01km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_1km across F001-F024".to_string(),
            )
        }
        Srh01km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-1 km storm-relative helicity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh01km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_1km across F025-F048".to_string(),
            )
        }
        Srh01km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-1 km storm-relative helicity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh01km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_1km across F001-F048".to_string(),
            )
        }
        Srh03km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-3 km storm-relative helicity max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh03km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_3km across F001-F024".to_string(),
            )
        }
        Srh03km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-3 km storm-relative helicity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh03km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_3km across F025-F048".to_string(),
            )
        }
        Srh03km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-3 km storm-relative helicity max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSrh03km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "m^2/s^2",
                Finish::None,
                "pointwise max of stored hourly srh_0_3km across F001-F048".to_string(),
            )
        }
        StpFixed0to24hMax => {
            if end < 24 {
                return Err("0-24 h significant tornado parameter max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly stp_fixed across F001-F024".to_string(),
            )
        }
        StpFixed24to48hMax => {
            if end < 48 {
                return Err("24-48 h significant tornado parameter max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly stp_fixed across F025-F048".to_string(),
            )
        }
        StpFixed0to48hMax => {
            if end < 48 {
                return Err("0-48 h significant tornado parameter max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly stp_fixed across F001-F048".to_string(),
            )
        }
        Ehi01km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-1 km energy-helicity index max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi01km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_1km across F001-F024".to_string(),
            )
        }
        Ehi01km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-1 km energy-helicity index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi01km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_1km across F025-F048".to_string(),
            )
        }
        Ehi01km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-1 km energy-helicity index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi01km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_1km across F001-F048".to_string(),
            )
        }
        Ehi03km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-3 km energy-helicity index max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi03km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_3km across F001-F024".to_string(),
            )
        }
        Ehi03km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-3 km energy-helicity index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi03km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_3km across F025-F048".to_string(),
            )
        }
        Ehi03km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-3 km energy-helicity index max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinEhi03km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly ehi_0_3km across F001-F048".to_string(),
            )
        }
        ScpProxy0to24hMax => {
            if end < 24 {
                return Err("0-24 h supercell composite max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinScpMu03km06kmProxy,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly scp_mu_0_3km_0_6km_proxy across F001-F024".to_string(),
            )
        }
        ScpProxy24to48hMax => {
            if end < 48 {
                return Err("24-48 h supercell composite max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinScpMu03km06kmProxy,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly scp_mu_0_3km_0_6km_proxy across F025-F048".to_string(),
            )
        }
        ScpProxy0to48hMax => {
            if end < 48 {
                return Err("0-48 h supercell composite max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinScpMu03km06kmProxy,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "dimensionless",
                Finish::None,
                "pointwise max of stored hourly scp_mu_0_3km_0_6km_proxy across F001-F048".to_string(),
            )
        }
        BulkShear01km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-1 km bulk shear max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear01km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_1km across F001-F024".to_string(),
            )
        }
        BulkShear01km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-1 km bulk shear max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear01km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_1km across F025-F048".to_string(),
            )
        }
        BulkShear01km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-1 km bulk shear max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear01km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_1km across F001-F048".to_string(),
            )
        }
        BulkShear06km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-6 km bulk shear max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear06km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_6km across F001-F024".to_string(),
            )
        }
        BulkShear06km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-6 km bulk shear max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear06km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_6km across F025-F048".to_string(),
            )
        }
        BulkShear06km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-6 km bulk shear max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinBulkShear06km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "kt",
                Finish::None,
                "pointwise max of stored hourly bulk_shear_0_6km across F001-F048".to_string(),
            )
        }
        LapseRate03km0to24hMax => {
            if end < 24 {
                return Err("0-24 h 0-3 km lapse rate max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate03km,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_0_3km across F001-F024".to_string(),
            )
        }
        LapseRate03km24to48hMax => {
            if end < 48 {
                return Err("24-48 h 0-3 km lapse rate max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate03km,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_0_3km across F025-F048".to_string(),
            )
        }
        LapseRate03km0to48hMax => {
            if end < 48 {
                return Err("0-48 h 0-3 km lapse rate max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate03km,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_0_3km across F001-F048".to_string(),
            )
        }
        LapseRate7005000to24hMax => {
            if end < 24 {
                return Err("0-24 h 700-500 mb lapse rate max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate700500,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_700_500 across F001-F024".to_string(),
            )
        }
        LapseRate70050024to48hMax => {
            if end < 48 {
                return Err("24-48 h 700-500 mb lapse rate max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate700500,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_700_500 across F025-F048".to_string(),
            )
        }
        LapseRate7005000to48hMax => {
            if end < 48 {
                return Err("0-48 h 700-500 mb lapse rate max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinLapseRate700500,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "degC/km",
                Finish::None,
                "pointwise max of stored hourly lapse_rate_700_500 across F001-F048".to_string(),
            )
        }
        Sblcl0to24hMin => {
            if end < 24 {
                return Err("0-24 h surface-based lcl height min requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSblcl,
                Reduce::Min,
                (1..=24).collect(),
                Some(24),
                "m",
                Finish::None,
                "pointwise min of stored hourly sblcl across F001-F024".to_string(),
            )
        }
        Sblcl24to48hMin => {
            if end < 48 {
                return Err("24-48 h surface-based lcl height min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSblcl,
                Reduce::Min,
                (25..=48).collect(),
                Some(24),
                "m",
                Finish::None,
                "pointwise min of stored hourly sblcl across F025-F048".to_string(),
            )
        }
        Sblcl0to48hMin => {
            if end < 48 {
                return Err("0-48 h surface-based lcl height min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinSblcl,
                Reduce::Min,
                (1..=48).collect(),
                Some(48),
                "m",
                Finish::None,
                "pointwise min of stored hourly sblcl across F001-F048".to_string(),
            )
        }
        CloudCoverTotalMaxField0to24hMax => {
            if end < 24 {
                return Err("0-24 h total cloud cover max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "%",
                Finish::None,
                "pointwise max of stored hourly cloud_cover_total across F001-F024".to_string(),
            )
        }
        CloudCoverTotalMaxField24to48hMax => {
            if end < 48 {
                return Err("24-48 h total cloud cover max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "%",
                Finish::None,
                "pointwise max of stored hourly cloud_cover_total across F025-F048".to_string(),
            )
        }
        CloudCoverTotalMaxField0to48hMax => {
            if end < 48 {
                return Err("0-48 h total cloud cover max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "%",
                Finish::None,
                "pointwise max of stored hourly cloud_cover_total across F001-F048".to_string(),
            )
        }
        CloudCoverTotalMinField0to24hMin => {
            if end < 24 {
                return Err("0-24 h total cloud cover min requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Min,
                (1..=24).collect(),
                Some(24),
                "%",
                Finish::None,
                "pointwise min of stored hourly cloud_cover_total across F001-F024".to_string(),
            )
        }
        CloudCoverTotalMinField24to48hMin => {
            if end < 48 {
                return Err("24-48 h total cloud cover min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Min,
                (25..=48).collect(),
                Some(24),
                "%",
                Finish::None,
                "pointwise min of stored hourly cloud_cover_total across F025-F048".to_string(),
            )
        }
        CloudCoverTotalMinField0to48hMin => {
            if end < 48 {
                return Err("0-48 h total cloud cover min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCloudCoverTotal,
                Reduce::Min,
                (1..=48).collect(),
                Some(48),
                "%",
                Finish::None,
                "pointwise min of stored hourly cloud_cover_total across F001-F048".to_string(),
            )
        }
        Mslp0to24hMin => {
            if end < 24 {
                return Err("0-24 h mean sea-level pressure min requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMslp,
                Reduce::Min,
                (1..=24).collect(),
                Some(24),
                "hPa",
                Finish::PaToHpa,
                "pointwise min of stored hourly mslp across F001-F024".to_string(),
            )
        }
        Mslp24to48hMin => {
            if end < 48 {
                return Err("24-48 h mean sea-level pressure min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMslp,
                Reduce::Min,
                (25..=48).collect(),
                Some(24),
                "hPa",
                Finish::PaToHpa,
                "pointwise min of stored hourly mslp across F025-F048".to_string(),
            )
        }
        Mslp0to48hMin => {
            if end < 48 {
                return Err("0-48 h mean sea-level pressure min requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMslp,
                Reduce::Min,
                (1..=48).collect(),
                Some(48),
                "hPa",
                Finish::PaToHpa,
                "pointwise min of stored hourly mslp across F001-F048".to_string(),
            )
        }
        CategoricalRain0to24hMax => {
            if end < 24 {
                return Err("0-24 h categorical rain max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalRain,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_rain across F001-F024".to_string(),
            )
        }
        CategoricalRain24to48hMax => {
            if end < 48 {
                return Err("24-48 h categorical rain max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalRain,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_rain across F025-F048".to_string(),
            )
        }
        CategoricalRain0to48hMax => {
            if end < 48 {
                return Err("0-48 h categorical rain max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalRain,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_rain across F001-F048".to_string(),
            )
        }
        CategoricalSnow0to24hMax => {
            if end < 24 {
                return Err("0-24 h categorical snow max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_snow across F001-F024".to_string(),
            )
        }
        CategoricalSnow24to48hMax => {
            if end < 48 {
                return Err("24-48 h categorical snow max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_snow across F025-F048".to_string(),
            )
        }
        CategoricalSnow0to48hMax => {
            if end < 48 {
                return Err("0-48 h categorical snow max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_snow across F001-F048".to_string(),
            )
        }
        CategoricalFreezingRain0to24hMax => {
            if end < 24 {
                return Err("0-24 h categorical freezing rain max requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Max,
                (1..=24).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_freezing_rain across F001-F024".to_string(),
            )
        }
        CategoricalFreezingRain24to48hMax => {
            if end < 48 {
                return Err("24-48 h categorical freezing rain max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Max,
                (25..=48).collect(),
                Some(24),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_freezing_rain across F025-F048".to_string(),
            )
        }
        CategoricalFreezingRain0to48hMax => {
            if end < 48 {
                return Err("0-48 h categorical freezing rain max requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Max,
                (1..=48).collect(),
                Some(48),
                "0/1",
                Finish::None,
                "pointwise max of stored hourly categorical_freezing_rain across F001-F048".to_string(),
            )
        }
        HeavyRainHours0to24h => {
            if end < 24 {
                return Err("Hours of Heavy Rain (>=0.5 in/h) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(12.7)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Heavy Rain (>=0.5 in/h) [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        HeavyRainHours24to48h => {
            if end < 48 {
                return Err("Hours of Heavy Rain (>=0.5 in/h) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(12.7)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Heavy Rain (>=0.5 in/h) [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        HeavyRainHours0to48h => {
            if end < 48 {
                return Err("Hours of Heavy Rain (>=0.5 in/h) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(12.7)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Heavy Rain (>=0.5 in/h) [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        VeryHeavyRainHours0to24h => {
            if end < 24 {
                return Err("Hours of Very Heavy Rain (>=1 in/h) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(25.4)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Very Heavy Rain (>=1 in/h) [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        VeryHeavyRainHours24to48h => {
            if end < 48 {
                return Err("Hours of Very Heavy Rain (>=1 in/h) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(25.4)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Very Heavy Rain (>=1 in/h) [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        VeryHeavyRainHours0to48h => {
            if end < 48 {
                return Err("Hours of Very Heavy Rain (>=1 in/h) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::Count(Threshold::above(25.4)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Very Heavy Rain (>=1 in/h) [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        HeavyRainLongestRun0to24h => {
            if end < 24 {
                return Err("Longest Run of Heavy Rain [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LongestRun(Threshold::above(12.7)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Heavy Rain [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        HeavyRainLongestRun24to48h => {
            if end < 48 {
                return Err("Longest Run of Heavy Rain [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LongestRun(Threshold::above(12.7)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Heavy Rain [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        HeavyRainLongestRun0to48h => {
            if end < 48 {
                return Err("Longest Run of Heavy Rain [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LongestRun(Threshold::above(12.7)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Longest Run of Heavy Rain [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainOnsetHour0to24h => {
            if end < 24 {
                return Err("Heavy Rain Onset Hour [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::FirstHour(Threshold::above(12.7)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Heavy Rain Onset Hour [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        RainOnsetHour24to48h => {
            if end < 48 {
                return Err("Heavy Rain Onset Hour [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::FirstHour(Threshold::above(12.7)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Heavy Rain Onset Hour [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainOnsetHour0to48h => {
            if end < 48 {
                return Err("Heavy Rain Onset Hour [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::FirstHour(Threshold::above(12.7)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Heavy Rain Onset Hour [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainEndHour0to24h => {
            if end < 24 {
                return Err("Heavy Rain End Hour [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LastHour(Threshold::above(12.7)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Heavy Rain End Hour [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        RainEndHour24to48h => {
            if end < 48 {
                return Err("Heavy Rain End Hour [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LastHour(Threshold::above(12.7)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Heavy Rain End Hour [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainEndHour0to48h => {
            if end < 48 {
                return Err("Heavy Rain End Hour [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::LastHour(Threshold::above(12.7)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Heavy Rain End Hour [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainPeakHour0to24h => {
            if end < 24 {
                return Err("Hour of Heaviest Rain [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::PeakHour(Threshold::above(2.5)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Heaviest Rain [0-24 h] over F001-F024 of stored hourly apcp_1h".to_string(),
            )
        }
        RainPeakHour24to48h => {
            if end < 48 {
                return Err("Hour of Heaviest Rain [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::PeakHour(Threshold::above(2.5)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Heaviest Rain [24-48 h] over F025-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        RainPeakHour0to48h => {
            if end < 48 {
                return Err("Hour of Heaviest Rain [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Apcp1h,
                Reduce::PeakHour(Threshold::above(2.5)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Hour of Heaviest Rain [0-48 h] over F001-F048 of stored hourly apcp_1h".to_string(),
            )
        }
        GustHours34kt0to24h => {
            if end < 24 {
                return Err("Hours of Gusts >=34 kt [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(17.491)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=34 kt [0-24 h] over F001-F024 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours34kt24to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=34 kt [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(17.491)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=34 kt [24-48 h] over F025-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours34kt0to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=34 kt [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(17.491)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Gusts >=34 kt [0-48 h] over F001-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours50kt0to24h => {
            if end < 24 {
                return Err("Hours of Gusts >=50 kt [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(25.722)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=50 kt [0-24 h] over F001-F024 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours50kt24to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=50 kt [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(25.722)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=50 kt [24-48 h] over F025-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours50kt0to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=50 kt [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(25.722)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Gusts >=50 kt [0-48 h] over F001-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours64kt0to24h => {
            if end < 24 {
                return Err("Hours of Gusts >=64 kt [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(32.924)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=64 kt [0-24 h] over F001-F024 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours64kt24to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=64 kt [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(32.924)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Gusts >=64 kt [24-48 h] over F025-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustHours64kt0to48h => {
            if end < 48 {
                return Err("Hours of Gusts >=64 kt [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::Count(Threshold::above(32.924)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Gusts >=64 kt [0-48 h] over F001-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustOnsetHour34kt0to24h => {
            if end < 24 {
                return Err("Onset Hour of 34 kt Gusts [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::FirstHour(Threshold::above(17.491)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Onset Hour of 34 kt Gusts [0-24 h] over F001-F024 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustOnsetHour34kt24to48h => {
            if end < 48 {
                return Err("Onset Hour of 34 kt Gusts [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::FirstHour(Threshold::above(17.491)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Onset Hour of 34 kt Gusts [24-48 h] over F025-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustOnsetHour34kt0to48h => {
            if end < 48 {
                return Err("Onset Hour of 34 kt Gusts [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::FirstHour(Threshold::above(17.491)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Onset Hour of 34 kt Gusts [0-48 h] over F001-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustPeakHour0to24h => {
            if end < 24 {
                return Err("Hour of Peak Gust [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::PeakHour(Threshold::above(10.0)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Gust [0-24 h] over F001-F024 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustPeakHour24to48h => {
            if end < 48 {
                return Err("Hour of Peak Gust [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::PeakHour(Threshold::above(10.0)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Gust [24-48 h] over F025-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        GustPeakHour0to48h => {
            if end < 48 {
                return Err("Hour of Peak Gust [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWindGust10m,
                Reduce::PeakHour(Threshold::above(10.0)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Hour of Peak Gust [0-48 h] over F001-F048 of stored hourly wind_gust_10m".to_string(),
            )
        }
        RotationHours0to24h => {
            if end < 24 {
                return Err("Hours of Storm Rotation (UH >=75) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Count(Threshold::above(75.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Storm Rotation (UH >=75) [0-24 h] over F001-F024 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        RotationHours24to48h => {
            if end < 48 {
                return Err("Hours of Storm Rotation (UH >=75) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Count(Threshold::above(75.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Storm Rotation (UH >=75) [24-48 h] over F025-F048 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        RotationHours0to48h => {
            if end < 48 {
                return Err("Hours of Storm Rotation (UH >=75) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::Count(Threshold::above(75.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Storm Rotation (UH >=75) [0-48 h] over F001-F048 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        RotationPeakHour0to24h => {
            if end < 24 {
                return Err("Hour of Peak Rotation [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::PeakHour(Threshold::above(25.0)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Rotation [0-24 h] over F001-F024 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        RotationPeakHour24to48h => {
            if end < 48 {
                return Err("Hour of Peak Rotation [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::PeakHour(Threshold::above(25.0)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Rotation [24-48 h] over F025-F048 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        RotationPeakHour0to48h => {
            if end < 48 {
                return Err("Hour of Peak Rotation [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Uh2to5km,
                Reduce::PeakHour(Threshold::above(25.0)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Hour of Peak Rotation [0-48 h] over F001-F048 of stored hourly uh_2to5km_max_1h".to_string(),
            )
        }
        StormHours0to24h => {
            if end < 24 {
                return Err("Hours with a Storm Overhead (>=40 dBZ) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Count(Threshold::above(40.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with a Storm Overhead (>=40 dBZ) [0-24 h] over F001-F024 of stored hourly composite_reflectivity".to_string(),
            )
        }
        StormHours24to48h => {
            if end < 48 {
                return Err("Hours with a Storm Overhead (>=40 dBZ) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Count(Threshold::above(40.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with a Storm Overhead (>=40 dBZ) [24-48 h] over F025-F048 of stored hourly composite_reflectivity".to_string(),
            )
        }
        StormHours0to48h => {
            if end < 48 {
                return Err("Hours with a Storm Overhead (>=40 dBZ) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::Count(Threshold::above(40.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours with a Storm Overhead (>=40 dBZ) [0-48 h] over F001-F048 of stored hourly composite_reflectivity".to_string(),
            )
        }
        StormOnsetHour0to24h => {
            if end < 24 {
                return Err("Convective Onset Hour [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::FirstHour(Threshold::above(40.0)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Convective Onset Hour [0-24 h] over F001-F024 of stored hourly composite_reflectivity".to_string(),
            )
        }
        StormOnsetHour24to48h => {
            if end < 48 {
                return Err("Convective Onset Hour [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::FirstHour(Threshold::above(40.0)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Convective Onset Hour [24-48 h] over F025-F048 of stored hourly composite_reflectivity".to_string(),
            )
        }
        StormOnsetHour0to48h => {
            if end < 48 {
                return Err("Convective Onset Hour [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCompositeReflectivity,
                Reduce::FirstHour(Threshold::above(40.0)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Convective Onset Hour [0-48 h] over F001-F048 of stored hourly composite_reflectivity".to_string(),
            )
        }
        SigTorEnvHours0to24h => {
            if end < 24 {
                return Err("Hours in a Significant-Tornado Environment (STP >=1) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Count(Threshold::above(1.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours in a Significant-Tornado Environment (STP >=1) [0-24 h] over F001-F024 of stored hourly stp_fixed".to_string(),
            )
        }
        SigTorEnvHours24to48h => {
            if end < 48 {
                return Err("Hours in a Significant-Tornado Environment (STP >=1) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Count(Threshold::above(1.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours in a Significant-Tornado Environment (STP >=1) [24-48 h] over F025-F048 of stored hourly stp_fixed".to_string(),
            )
        }
        SigTorEnvHours0to48h => {
            if end < 48 {
                return Err("Hours in a Significant-Tornado Environment (STP >=1) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinStpFixed,
                Reduce::Count(Threshold::above(1.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours in a Significant-Tornado Environment (STP >=1) [0-48 h] over F001-F048 of stored hourly stp_fixed".to_string(),
            )
        }
        BigCapeHours0to24h => {
            if end < 24 {
                return Err("Hours with MUCAPE >=1000 J/kg [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Count(Threshold::above(1000.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with MUCAPE >=1000 J/kg [0-24 h] over F001-F024 of stored hourly mucape".to_string(),
            )
        }
        BigCapeHours24to48h => {
            if end < 48 {
                return Err("Hours with MUCAPE >=1000 J/kg [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Count(Threshold::above(1000.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with MUCAPE >=1000 J/kg [24-48 h] over F025-F048 of stored hourly mucape".to_string(),
            )
        }
        BigCapeHours0to48h => {
            if end < 48 {
                return Err("Hours with MUCAPE >=1000 J/kg [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinMucape,
                Reduce::Count(Threshold::above(1000.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours with MUCAPE >=1000 J/kg [0-48 h] over F001-F048 of stored hourly mucape".to_string(),
            )
        }
        CriticalRhHours0to24h => {
            if end < 24 {
                return Err("Hours of Critical Low RH (<=15%) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::Count(Threshold::below(15.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Critical Low RH (<=15%) [0-24 h] over F001-F024 of stored hourly rh_2m".to_string(),
            )
        }
        CriticalRhHours24to48h => {
            if end < 48 {
                return Err("Hours of Critical Low RH (<=15%) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::Count(Threshold::below(15.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Critical Low RH (<=15%) [24-48 h] over F025-F048 of stored hourly rh_2m".to_string(),
            )
        }
        CriticalRhHours0to48h => {
            if end < 48 {
                return Err("Hours of Critical Low RH (<=15%) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::Count(Threshold::below(15.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Critical Low RH (<=15%) [0-48 h] over F001-F048 of stored hourly rh_2m".to_string(),
            )
        }
        CriticalRhLongestRun0to24h => {
            if end < 24 {
                return Err("Longest Run of Critical Low RH [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::LongestRun(Threshold::below(15.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Critical Low RH [0-24 h] over F001-F024 of stored hourly rh_2m".to_string(),
            )
        }
        CriticalRhLongestRun24to48h => {
            if end < 48 {
                return Err("Longest Run of Critical Low RH [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::LongestRun(Threshold::below(15.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Critical Low RH [24-48 h] over F025-F048 of stored hourly rh_2m".to_string(),
            )
        }
        CriticalRhLongestRun0to48h => {
            if end < 48 {
                return Err("Longest Run of Critical Low RH [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::Rh2mPct,
                Reduce::LongestRun(Threshold::below(15.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Longest Run of Critical Low RH [0-48 h] over F001-F048 of stored hourly rh_2m".to_string(),
            )
        }
        HdwPeakHour0to24h => {
            if end < 24 {
                return Err("Hour of Peak Hot-Dry-Windy [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::PeakHour(Threshold::above(50.0)),
                (1..=24).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Hot-Dry-Windy [0-24 h] over F001-F024 of stored hourly hdw".to_string(),
            )
        }
        HdwPeakHour24to48h => {
            if end < 48 {
                return Err("Hour of Peak Hot-Dry-Windy [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::PeakHour(Threshold::above(50.0)),
                (25..=48).collect(),
                Some(24),
                "forecast hour",
                Finish::None,
                "Hour of Peak Hot-Dry-Windy [24-48 h] over F025-F048 of stored hourly hdw".to_string(),
            )
        }
        HdwPeakHour0to48h => {
            if end < 48 {
                return Err("Hour of Peak Hot-Dry-Windy [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHdw,
                Reduce::PeakHour(Threshold::above(50.0)),
                (1..=48).collect(),
                Some(48),
                "forecast hour",
                Finish::None,
                "Hour of Peak Hot-Dry-Windy [0-48 h] over F001-F048 of stored hourly hdw".to_string(),
            )
        }
        DangerHeatHours0to24h => {
            if end < 24 {
                return Err("Hours of Dangerous Heat (HI >=105F) [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Count(Threshold::above(40.56)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Dangerous Heat (HI >=105F) [0-24 h] over F001-F024 of stored hourly heat_index_2m".to_string(),
            )
        }
        DangerHeatHours24to48h => {
            if end < 48 {
                return Err("Hours of Dangerous Heat (HI >=105F) [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Count(Threshold::above(40.56)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Dangerous Heat (HI >=105F) [24-48 h] over F025-F048 of stored hourly heat_index_2m".to_string(),
            )
        }
        DangerHeatHours0to48h => {
            if end < 48 {
                return Err("Hours of Dangerous Heat (HI >=105F) [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::Count(Threshold::above(40.56)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Dangerous Heat (HI >=105F) [0-48 h] over F001-F048 of stored hourly heat_index_2m".to_string(),
            )
        }
        DangerHeatLongestRun0to24h => {
            if end < 24 {
                return Err("Longest Run of Dangerous Heat [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::LongestRun(Threshold::above(40.56)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Dangerous Heat [0-24 h] over F001-F024 of stored hourly heat_index_2m".to_string(),
            )
        }
        DangerHeatLongestRun24to48h => {
            if end < 48 {
                return Err("Longest Run of Dangerous Heat [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::LongestRun(Threshold::above(40.56)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Dangerous Heat [24-48 h] over F025-F048 of stored hourly heat_index_2m".to_string(),
            )
        }
        DangerHeatLongestRun0to48h => {
            if end < 48 {
                return Err("Longest Run of Dangerous Heat [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinHeatIndex2m,
                Reduce::LongestRun(Threshold::above(40.56)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Longest Run of Dangerous Heat [0-48 h] over F001-F048 of stored hourly heat_index_2m".to_string(),
            )
        }
        HighWetbulbHours0to24h => {
            if end < 24 {
                return Err("Hours with Wet-Bulb >=28C [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Count(Threshold::above(28.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with Wet-Bulb >=28C [0-24 h] over F001-F024 of stored hourly wetbulb_2m".to_string(),
            )
        }
        HighWetbulbHours24to48h => {
            if end < 48 {
                return Err("Hours with Wet-Bulb >=28C [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Count(Threshold::above(28.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours with Wet-Bulb >=28C [24-48 h] over F025-F048 of stored hourly wetbulb_2m".to_string(),
            )
        }
        HighWetbulbHours0to48h => {
            if end < 48 {
                return Err("Hours with Wet-Bulb >=28C [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinWetbulb2m,
                Reduce::Count(Threshold::above(28.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours with Wet-Bulb >=28C [0-48 h] over F001-F048 of stored hourly wetbulb_2m".to_string(),
            )
        }
        LowVisHours0to24h => {
            if end < 24 {
                return Err("Hours of Visibility <=1 mile [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Count(Threshold::below(1609.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Visibility <=1 mile [0-24 h] over F001-F024 of stored hourly visibility".to_string(),
            )
        }
        LowVisHours24to48h => {
            if end < 48 {
                return Err("Hours of Visibility <=1 mile [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Count(Threshold::below(1609.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Visibility <=1 mile [24-48 h] over F025-F048 of stored hourly visibility".to_string(),
            )
        }
        LowVisHours0to48h => {
            if end < 48 {
                return Err("Hours of Visibility <=1 mile [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::Count(Threshold::below(1609.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Visibility <=1 mile [0-48 h] over F001-F048 of stored hourly visibility".to_string(),
            )
        }
        LowVisLongestRun0to24h => {
            if end < 24 {
                return Err("Longest Run of Visibility <=1 mile [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::LongestRun(Threshold::below(1609.0)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Visibility <=1 mile [0-24 h] over F001-F024 of stored hourly visibility".to_string(),
            )
        }
        LowVisLongestRun24to48h => {
            if end < 48 {
                return Err("Longest Run of Visibility <=1 mile [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::LongestRun(Threshold::below(1609.0)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Longest Run of Visibility <=1 mile [24-48 h] over F025-F048 of stored hourly visibility".to_string(),
            )
        }
        LowVisLongestRun0to48h => {
            if end < 48 {
                return Err("Longest Run of Visibility <=1 mile [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinVisibility,
                Reduce::LongestRun(Threshold::below(1609.0)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Longest Run of Visibility <=1 mile [0-48 h] over F001-F048 of stored hourly visibility".to_string(),
            )
        }
        SnowHours0to24h => {
            if end < 24 {
                return Err("Hours of Snow [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Count(Threshold::above(0.5)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Snow [0-24 h] over F001-F024 of stored hourly categorical_snow".to_string(),
            )
        }
        SnowHours24to48h => {
            if end < 48 {
                return Err("Hours of Snow [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Count(Threshold::above(0.5)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Snow [24-48 h] over F025-F048 of stored hourly categorical_snow".to_string(),
            )
        }
        SnowHours0to48h => {
            if end < 48 {
                return Err("Hours of Snow [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalSnow,
                Reduce::Count(Threshold::above(0.5)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Snow [0-48 h] over F001-F048 of stored hourly categorical_snow".to_string(),
            )
        }
        FreezingRainHours0to24h => {
            if end < 24 {
                return Err("Hours of Freezing Rain [0-24 h] requires forecast hour >= 24; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Count(Threshold::above(0.5)),
                (1..=24).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Freezing Rain [0-24 h] over F001-F024 of stored hourly categorical_freezing_rain".to_string(),
            )
        }
        FreezingRainHours24to48h => {
            if end < 48 {
                return Err("Hours of Freezing Rain [24-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Count(Threshold::above(0.5)),
                (25..=48).collect(),
                Some(24),
                "hours",
                Finish::None,
                "Hours of Freezing Rain [24-48 h] over F025-F048 of stored hourly categorical_freezing_rain".to_string(),
            )
        }
        FreezingRainHours0to48h => {
            if end < 48 {
                return Err("Hours of Freezing Rain [0-48 h] requires forecast hour >= 48; use a HRRR extended cycle for 24-48 h products".to_string());
            }
            spec(
                SourceKind::WinCategoricalFreezingRain,
                Reduce::Count(Threshold::above(0.5)),
                (1..=48).collect(),
                Some(48),
                "hours",
                Finish::None,
                "Hours of Freezing Rain [0-48 h] over F001-F048 of stored hourly categorical_freezing_rain".to_string(),
            )
        }
        _ => unreachable!("surface snapshot window products are handled before the match"),
    }
}

struct SnapshotPlan {
    source: SourceKind,
    reduce: Reduce,
    window_start: u16,
    window_end: u16,
    window_hours: u16,
    /// e.g. "F001-F024" (strategy text).
    window_label: &'static str,
    /// e.g. "0-24 h 2 m surface snapshot window" (planning blocker text,
    /// mirroring the GRIB lane verbatim).
    blocker_label: &'static str,
    field_label: &'static str,
    op_label: &'static str,
    units: &'static str,
}

/// Decompose a 2 m snapshot-window product into its field, window, and
/// reduction — `None` for QPF/UH/wind products.
/// True when a product describes the whole RUN rather than a window ending at
/// a requested hour: the fixed 0-24/24-48/0-48 h windows and the `*_run_max`
/// products. These always anchor at the max stored hour so they neither block
/// nor truncate when a caller asks for an earlier hour.
///
/// Covers the 2 m snapshot windows (via [`snapshot_plan`]) AND the 10 m wind
/// windows, which are planned inline in [`plan_product`] rather than through
/// `snapshot_plan` but are just as fixed (`(1..=24)`, `(25..=48)`, `(1..=48)`).
fn product_is_run_scoped(product: HrrrWindowedProduct) -> bool {
    use HrrrWindowedProduct::*;
    snapshot_plan(product).is_some()
        || matches!(
            product,
            Uh25kmRunMax
                | Wind10mRunMax
                | Wind10m0to24hMax
                | Wind10m24to48hMax
                | Wind10m0to48hMax
                | Smoke8m0to24hMax
                | Smoke8m24to48hMax
                | Smoke8m0to48hMax
                | SmokeColumn0to24hMax
                | SmokeColumn24to48hMax
                | SmokeColumn0to48hMax
                | Gust10m0to24hMax
                | Gust10m24to48hMax
                | Gust10m0to48hMax
                | Hdw0to24hMax
                | Hdw24to48hMax
                | Hdw0to48hMax
                | FireWxComposite0to24hMax
                | FireWxComposite24to48hMax
                | FireWxComposite0to48hMax
                | Visibility0to24hMin
                | Visibility24to48hMin
                | Visibility0to48hMin
                | DewpointDepression2m0to24hMax
                | DewpointDepression2m24to48hMax
                | DewpointDepression2m0to48hMax
                | HeatIndex2m0to24hMax
                | HeatIndex2m24to48hMax
                | HeatIndex2m0to48hMax
                | ApparentTemp2m0to24hMax
                | ApparentTemp2m24to48hMax
                | ApparentTemp2m0to48hMax
                | Wetbulb2m0to24hMax
                | Wetbulb2m24to48hMax
                | Wetbulb2m0to48hMax
                | WindChill2m0to24hMin
                | WindChill2m24to48hMin
                | WindChill2m0to48hMin
                | CompositeReflectivity0to24hMax
                | CompositeReflectivity24to48hMax
                | CompositeReflectivity0to48hMax
                | Sbcape0to24hMax
                | Sbcape24to48hMax
                | Sbcape0to48hMax
                | Mlcape0to24hMax
                | Mlcape24to48hMax
                | Mlcape0to48hMax
                | Mucape0to24hMax
                | Mucape24to48hMax
                | Mucape0to48hMax
                | Dcape0to24hMax
                | Dcape24to48hMax
                | Dcape0to48hMax
                | Pwat0to24hMax
                | Pwat24to48hMax
                | Pwat0to48hMax
                | ThetaE2m0to24hMax
                | ThetaE2m24to48hMax
                | ThetaE2m0to48hMax
                | Srh01km0to24hMax
                | Srh01km24to48hMax
                | Srh01km0to48hMax
                | Srh03km0to24hMax
                | Srh03km24to48hMax
                | Srh03km0to48hMax
                | StpFixed0to24hMax
                | StpFixed24to48hMax
                | StpFixed0to48hMax
                | Ehi01km0to24hMax
                | Ehi01km24to48hMax
                | Ehi01km0to48hMax
                | Ehi03km0to24hMax
                | Ehi03km24to48hMax
                | Ehi03km0to48hMax
                | ScpProxy0to24hMax
                | ScpProxy24to48hMax
                | ScpProxy0to48hMax
                | BulkShear01km0to24hMax
                | BulkShear01km24to48hMax
                | BulkShear01km0to48hMax
                | BulkShear06km0to24hMax
                | BulkShear06km24to48hMax
                | BulkShear06km0to48hMax
                | LapseRate03km0to24hMax
                | LapseRate03km24to48hMax
                | LapseRate03km0to48hMax
                | LapseRate7005000to24hMax
                | LapseRate70050024to48hMax
                | LapseRate7005000to48hMax
                | Sblcl0to24hMin
                | Sblcl24to48hMin
                | Sblcl0to48hMin
                | CloudCoverTotalMaxField0to24hMax
                | CloudCoverTotalMaxField24to48hMax
                | CloudCoverTotalMaxField0to48hMax
                | CloudCoverTotalMinField0to24hMin
                | CloudCoverTotalMinField24to48hMin
                | CloudCoverTotalMinField0to48hMin
                | Mslp0to24hMin
                | Mslp24to48hMin
                | Mslp0to48hMin
                | CategoricalRain0to24hMax
                | CategoricalRain24to48hMax
                | CategoricalRain0to48hMax
                | CategoricalSnow0to24hMax
                | CategoricalSnow24to48hMax
                | CategoricalSnow0to48hMax
                | CategoricalFreezingRain0to24hMax
                | CategoricalFreezingRain24to48hMax
                | CategoricalFreezingRain0to48hMax
                | HeavyRainHours0to24h
                | HeavyRainHours24to48h
                | HeavyRainHours0to48h
                | VeryHeavyRainHours0to24h
                | VeryHeavyRainHours24to48h
                | VeryHeavyRainHours0to48h
                | HeavyRainLongestRun0to24h
                | HeavyRainLongestRun24to48h
                | HeavyRainLongestRun0to48h
                | RainOnsetHour0to24h
                | RainOnsetHour24to48h
                | RainOnsetHour0to48h
                | RainEndHour0to24h
                | RainEndHour24to48h
                | RainEndHour0to48h
                | RainPeakHour0to24h
                | RainPeakHour24to48h
                | RainPeakHour0to48h
                | GustHours34kt0to24h
                | GustHours34kt24to48h
                | GustHours34kt0to48h
                | GustHours50kt0to24h
                | GustHours50kt24to48h
                | GustHours50kt0to48h
                | GustHours64kt0to24h
                | GustHours64kt24to48h
                | GustHours64kt0to48h
                | GustOnsetHour34kt0to24h
                | GustOnsetHour34kt24to48h
                | GustOnsetHour34kt0to48h
                | GustPeakHour0to24h
                | GustPeakHour24to48h
                | GustPeakHour0to48h
                | RotationHours0to24h
                | RotationHours24to48h
                | RotationHours0to48h
                | RotationPeakHour0to24h
                | RotationPeakHour24to48h
                | RotationPeakHour0to48h
                | StormHours0to24h
                | StormHours24to48h
                | StormHours0to48h
                | StormOnsetHour0to24h
                | StormOnsetHour24to48h
                | StormOnsetHour0to48h
                | SigTorEnvHours0to24h
                | SigTorEnvHours24to48h
                | SigTorEnvHours0to48h
                | BigCapeHours0to24h
                | BigCapeHours24to48h
                | BigCapeHours0to48h
                | CriticalRhHours0to24h
                | CriticalRhHours24to48h
                | CriticalRhHours0to48h
                | CriticalRhLongestRun0to24h
                | CriticalRhLongestRun24to48h
                | CriticalRhLongestRun0to48h
                | HdwPeakHour0to24h
                | HdwPeakHour24to48h
                | HdwPeakHour0to48h
                | DangerHeatHours0to24h
                | DangerHeatHours24to48h
                | DangerHeatHours0to48h
                | DangerHeatLongestRun0to24h
                | DangerHeatLongestRun24to48h
                | DangerHeatLongestRun0to48h
                | HighWetbulbHours0to24h
                | HighWetbulbHours24to48h
                | HighWetbulbHours0to48h
                | LowVisHours0to24h
                | LowVisHours24to48h
                | LowVisHours0to48h
                | LowVisLongestRun0to24h
                | LowVisLongestRun24to48h
                | LowVisLongestRun0to48h
                | SnowHours0to24h
                | SnowHours24to48h
                | SnowHours0to48h
                | FreezingRainHours0to24h
                | FreezingRainHours24to48h
                | FreezingRainHours0to48h
        )
}

fn snapshot_plan(product: HrrrWindowedProduct) -> Option<SnapshotPlan> {
    use HrrrWindowedProduct::*;
    let (source, field_label, units) = match product {
        Temp2m0to24hMax | Temp2m24to48hMax | Temp2m0to48hMax | Temp2m0to24hMin
        | Temp2m24to48hMin | Temp2m0to48hMin | Temp2m0to24hRange | Temp2m24to48hRange
        | Temp2m0to48hRange => (SourceKind::Temp2mC, "2 m temperature", "degC"),
        Rh2m0to24hMax | Rh2m24to48hMax | Rh2m0to48hMax | Rh2m0to24hMin | Rh2m24to48hMin
        | Rh2m0to48hMin | Rh2m0to24hRange | Rh2m24to48hRange | Rh2m0to48hRange => {
            (SourceKind::Rh2mPct, "2 m relative humidity", "%")
        }
        Dewpoint2m0to24hMax
        | Dewpoint2m24to48hMax
        | Dewpoint2m0to48hMax
        | Dewpoint2m0to24hMin
        | Dewpoint2m24to48hMin
        | Dewpoint2m0to48hMin
        | Dewpoint2m0to24hRange
        | Dewpoint2m24to48hRange
        | Dewpoint2m0to48hRange => (SourceKind::Dewpoint2mC, "2 m dewpoint", "degC"),
        Vpd2m0to24hMax | Vpd2m24to48hMax | Vpd2m0to48hMax | Vpd2m0to24hMin | Vpd2m24to48hMin
        | Vpd2m0to48hMin | Vpd2m0to24hRange | Vpd2m24to48hRange | Vpd2m0to48hRange => {
            (SourceKind::Vpd2mHpa, "2 m vapor pressure deficit", "hPa")
        }
        _ => return None,
    };
    let (window_start, window_end, window_hours, window_label, blocker_label) = match product {
        Temp2m0to24hMax
        | Temp2m0to24hMin
        | Temp2m0to24hRange
        | Rh2m0to24hMax
        | Rh2m0to24hMin
        | Rh2m0to24hRange
        | Dewpoint2m0to24hMax
        | Dewpoint2m0to24hMin
        | Dewpoint2m0to24hRange
        | Vpd2m0to24hMax
        | Vpd2m0to24hMin
        | Vpd2m0to24hRange => (1, 24, 24, "F001-F024", "0-24 h 2 m surface snapshot window"),
        Temp2m24to48hMax
        | Temp2m24to48hMin
        | Temp2m24to48hRange
        | Rh2m24to48hMax
        | Rh2m24to48hMin
        | Rh2m24to48hRange
        | Dewpoint2m24to48hMax
        | Dewpoint2m24to48hMin
        | Dewpoint2m24to48hRange
        | Vpd2m24to48hMax
        | Vpd2m24to48hMin
        | Vpd2m24to48hRange => (
            25,
            48,
            24,
            "F025-F048",
            "24-48 h 2 m surface snapshot window",
        ),
        _ => (1, 48, 48, "F001-F048", "0-48 h 2 m surface snapshot window"),
    };
    let (reduce, op_label) = match product {
        Temp2m0to24hMax | Temp2m24to48hMax | Temp2m0to48hMax | Rh2m0to24hMax | Rh2m24to48hMax
        | Rh2m0to48hMax | Dewpoint2m0to24hMax | Dewpoint2m24to48hMax | Dewpoint2m0to48hMax
        | Vpd2m0to24hMax | Vpd2m24to48hMax | Vpd2m0to48hMax => (Reduce::Max, "max"),
        Temp2m0to24hMin | Temp2m24to48hMin | Temp2m0to48hMin | Rh2m0to24hMin | Rh2m24to48hMin
        | Rh2m0to48hMin | Dewpoint2m0to24hMin | Dewpoint2m24to48hMin | Dewpoint2m0to48hMin
        | Vpd2m0to24hMin | Vpd2m24to48hMin | Vpd2m0to48hMin => (Reduce::Min, "min"),
        _ => (Reduce::Range, "max-min range"),
    };
    Some(SnapshotPlan {
        source,
        reduce,
        window_start,
        window_end,
        window_hours,
        window_label,
        blocker_label,
        field_label,
        op_label,
        units,
    })
}

/// One source plane read for one hour: the fold-ready values, plus whether
/// the per-hour instantaneous fallback (not the stored sub-hourly max
/// field) supplied them — recorded so the product's strategy note can name
/// the lower-bound hours honestly.
struct SourcePlane {
    values: Vec<f64>,
    instantaneous_fallback: bool,
}

impl SourcePlane {
    fn exact(values: Vec<f64>) -> Self {
        Self {
            values,
            instantaneous_fallback: false,
        }
    }
}

/// Why one stored-variable read failed: the variable is absent from the
/// hour file (eligible for the documented instantaneous fallback) vs any
/// other failure (unit drift, codec error — always a blocker, never a
/// silent fallback).
enum ReadFailure {
    MissingVariable(String),
    Failed(String),
}

impl ReadFailure {
    fn into_reason(self) -> String {
        match self {
            Self::MissingVariable(reason) | Self::Failed(reason) => reason,
        }
    }
}

/// Read one source plane for one hour, unit-checked and transformed to the
/// per-hour values the fold consumes (the GRIB lane's per-hour transforms:
/// K -> degC, RH clamp; accumulation/UH/wind planes stay raw — their
/// display conversion happens after the fold). UH/wind prefer the stored
/// sub-hourly max fields and fall back to the instantaneous planes ONLY
/// when the max variable is absent (older stores); a max field present
/// with wrong units blocks instead of falling back.
fn read_source_plane(
    reader: &HourReader,
    grid: &GridFile,
    kind: SourceKind,
    hour: u16,
) -> Result<SourcePlane, String> {
    let read = |name: &str, expected_units: &str| -> Result<Vec<f32>, ReadFailure> {
        match read_grid_2d(reader, grid, name) {
            Ok(stored) => {
                if stored.units != expected_units {
                    return Err(ReadFailure::Failed(format!(
                        "stored '{name}' at F{hour:03} has units '{}', expected '{expected_units}'",
                        stored.units
                    )));
                }
                Ok(stored.values)
            }
            Err(RwStoreError::UnknownVariable(_)) => Err(ReadFailure::MissingVariable(format!(
                "stored hour F{hour:03} has no '{name}' variable"
            ))),
            Err(err) => Err(ReadFailure::Failed(format!(
                "read '{name}' from stored hour F{hour:03}: {err}"
            ))),
        }
    };
    let plain = |result: Result<Vec<f32>, ReadFailure>| -> Result<Vec<f32>, String> {
        result.map_err(ReadFailure::into_reason)
    };
    match kind {
        SourceKind::Apcp1h => Ok(SourcePlane::exact(to_f64(plain(read(
            "apcp_1h", "kg/m^2",
        ))?))),
        SourceKind::ApcpRunTotal => Ok(SourcePlane::exact(to_f64(plain(read(
            "apcp_run_total",
            "kg/m^2",
        ))?))),
        SourceKind::Uh2to5km => match read("uh_2to5km_max_1h", "m^2/s^2") {
            Ok(values) => Ok(SourcePlane::exact(to_f64(values))),
            Err(ReadFailure::Failed(reason)) => Err(reason),
            Err(ReadFailure::MissingVariable(missing)) => match read("uh_2to5km", "m^2/s^2") {
                Ok(values) => Ok(SourcePlane {
                    values: to_f64(values),
                    instantaneous_fallback: true,
                }),
                Err(err) => Err(format!(
                    "{missing}; hourly 'uh_2to5km' fallback also unavailable: {}",
                    err.into_reason()
                )),
            },
        },
        SourceKind::WindSpeed10m => match read("wind_speed_10m_max_1h", "m/s") {
            Ok(values) => Ok(SourcePlane::exact(to_f64(values))),
            Err(ReadFailure::Failed(reason)) => Err(reason),
            Err(ReadFailure::MissingVariable(missing)) => {
                let speeds = (|| -> Result<Vec<f64>, ReadFailure> {
                    let u = read("u_10m", "m/s")?;
                    let v = read("v_10m", "m/s")?;
                    Ok(u.iter()
                        .zip(&v)
                        .map(|(&u, &v)| f64::from(u).hypot(f64::from(v)))
                        .collect())
                })();
                match speeds {
                    Ok(values) => Ok(SourcePlane {
                        values,
                        instantaneous_fallback: true,
                    }),
                    Err(err) => Err(format!(
                        "{missing}; hypot(u_10m, v_10m) fallback also unavailable: {}",
                        err.into_reason()
                    )),
                }
            }
        },
        SourceKind::Temp2mC | SourceKind::Dewpoint2mC => {
            let name = if kind == SourceKind::Temp2mC {
                "temperature_2m"
            } else {
                "dewpoint_2m"
            };
            Ok(SourcePlane::exact(
                plain(read(name, "K"))?
                    .iter()
                    .map(|&value| f64::from(value) - 273.15)
                    .collect(),
            ))
        }
        SourceKind::Rh2mPct => Ok(SourcePlane::exact(
            plain(read("rh_2m", "%"))?
                .iter()
                .map(|&value| f64::from(value).clamp(0.0, 100.0))
                .collect(),
        )),
        SourceKind::Vpd2mHpa => Ok(SourcePlane::exact(to_f64(plain(read("vpd_2m", "hPa"))?))),
        SourceKind::Smoke8m => Ok(SourcePlane::exact(to_f64(plain(read(
            "smoke_8m", "kg/m^3",
        ))?))),
        SourceKind::SmokeColumn => Ok(SourcePlane::exact(to_f64(plain(read(
            "smoke_column",
            "kg/m^2",
        ))?))),
        SourceKind::WinWindGust10m => Ok(SourcePlane::exact(to_f64(plain(read(
            "wind_gust_10m",
            "m/s",
        ))?))),
        SourceKind::WinHdw => Ok(SourcePlane::exact(to_f64(plain(read(
            "hdw",
            "hPa*m/s",
        ))?))),
        SourceKind::WinFireWeatherComposite => Ok(SourcePlane::exact(to_f64(plain(read(
            "fire_weather_composite",
            "index",
        ))?))),
        SourceKind::WinVisibility => Ok(SourcePlane::exact(to_f64(plain(read(
            "visibility",
            "m",
        ))?))),
        SourceKind::WinDewpointDepression2m => Ok(SourcePlane::exact(to_f64(plain(read(
            "dewpoint_depression_2m",
            "degC",
        ))?))),
        SourceKind::WinHeatIndex2m => Ok(SourcePlane::exact(to_f64(plain(read(
            "heat_index_2m",
            "degC",
        ))?))),
        SourceKind::WinApparentTemperature2m => Ok(SourcePlane::exact(to_f64(plain(read(
            "apparent_temperature_2m",
            "degC",
        ))?))),
        SourceKind::WinWetbulb2m => Ok(SourcePlane::exact(to_f64(plain(read(
            "wetbulb_2m",
            "degC",
        ))?))),
        SourceKind::WinWindChill2m => Ok(SourcePlane::exact(to_f64(plain(read(
            "wind_chill_2m",
            "degC",
        ))?))),
        SourceKind::WinCompositeReflectivity => Ok(SourcePlane::exact(to_f64(plain(read(
            "composite_reflectivity",
            "dBZ",
        ))?))),
        SourceKind::WinSbcape => Ok(SourcePlane::exact(to_f64(plain(read(
            "sbcape",
            "J/kg",
        ))?))),
        SourceKind::WinMlcape => Ok(SourcePlane::exact(to_f64(plain(read(
            "mlcape",
            "J/kg",
        ))?))),
        SourceKind::WinMucape => Ok(SourcePlane::exact(to_f64(plain(read(
            "mucape",
            "J/kg",
        ))?))),
        SourceKind::WinDcape => Ok(SourcePlane::exact(to_f64(plain(read(
            "dcape",
            "J/kg",
        ))?))),
        SourceKind::WinPwat => Ok(SourcePlane::exact(to_f64(plain(read(
            "pwat",
            "kg/m^2",
        ))?))),
        SourceKind::WinThetaE2m10mWinds => Ok(SourcePlane::exact(to_f64(plain(read(
            "theta_e_2m_10m_winds",
            "K",
        ))?))),
        SourceKind::WinSrh01km => Ok(SourcePlane::exact(to_f64(plain(read(
            "srh_0_1km",
            "m^2/s^2",
        ))?))),
        SourceKind::WinSrh03km => Ok(SourcePlane::exact(to_f64(plain(read(
            "srh_0_3km",
            "m^2/s^2",
        ))?))),
        SourceKind::WinStpFixed => Ok(SourcePlane::exact(to_f64(plain(read(
            "stp_fixed",
            "dimensionless",
        ))?))),
        SourceKind::WinEhi01km => Ok(SourcePlane::exact(to_f64(plain(read(
            "ehi_0_1km",
            "dimensionless",
        ))?))),
        SourceKind::WinEhi03km => Ok(SourcePlane::exact(to_f64(plain(read(
            "ehi_0_3km",
            "dimensionless",
        ))?))),
        SourceKind::WinScpMu03km06kmProxy => Ok(SourcePlane::exact(to_f64(plain(read(
            "scp_mu_0_3km_0_6km_proxy",
            "dimensionless",
        ))?))),
        SourceKind::WinBulkShear01km => Ok(SourcePlane::exact(to_f64(plain(read(
            "bulk_shear_0_1km",
            "kt",
        ))?))),
        SourceKind::WinBulkShear06km => Ok(SourcePlane::exact(to_f64(plain(read(
            "bulk_shear_0_6km",
            "kt",
        ))?))),
        SourceKind::WinLapseRate03km => Ok(SourcePlane::exact(to_f64(plain(read(
            "lapse_rate_0_3km",
            "degC/km",
        ))?))),
        SourceKind::WinLapseRate700500 => Ok(SourcePlane::exact(to_f64(plain(read(
            "lapse_rate_700_500",
            "degC/km",
        ))?))),
        SourceKind::WinSblcl => Ok(SourcePlane::exact(to_f64(plain(read(
            "sblcl",
            "m",
        ))?))),
        SourceKind::WinCloudCoverTotal => Ok(SourcePlane::exact(to_f64(plain(read(
            "cloud_cover_total",
            "%",
        ))?))),
        SourceKind::WinMslp => Ok(SourcePlane::exact(to_f64(plain(read(
            "mslp",
            "Pa",
        ))?))),
        SourceKind::WinCategoricalRain => Ok(SourcePlane::exact(to_f64(plain(read(
            "categorical_rain",
            "0/1",
        ))?))),
        SourceKind::WinCategoricalSnow => Ok(SourcePlane::exact(to_f64(plain(read(
            "categorical_snow",
            "0/1",
        ))?))),
        SourceKind::WinCategoricalFreezingRain => Ok(SourcePlane::exact(to_f64(plain(read(
            "categorical_freezing_rain",
            "0/1",
        ))?))),
    }
}

fn to_f64(values: Vec<f32>) -> Vec<f64> {
    values.into_iter().map(f64::from).collect()
}

/// Per-product streaming accumulator: per-hour planes fold in ascending
/// hour order; `failed` records the first per-hour read failure (the
/// product's blocker reason — once failed, later hours stop folding).
/// `fallback_hours` collects the hours whose plane came from the
/// documented instantaneous fallback (no stored sub-hourly max field) so
/// `finish` can stamp the lower-bound note into the strategy.
struct Accum {
    spec: ProductSpec,
    state: Option<AccumState>,
    failed: Option<String>,
    fallback_hours: Vec<u16>,
}

enum AccumState {
    Sum(Vec<f64>),
    Max(Vec<f64>),
    Min(Vec<f64>),
    Range { max: Vec<f64>, min: Vec<f64> },
    Direct(Vec<f64>),
    /// Hours meeting the threshold (a COUNT, never a probability).
    Count(Vec<f64>),
    /// Longest consecutive streak of hours meeting the threshold.
    LongestRun { best: Vec<f64>, current: Vec<f64> },
    /// First / last hour meeting the threshold; NaN where never met.
    EdgeHour(Vec<f64>),
    /// Hour of the window maximum, with the running max to compare against.
    PeakHour { hour: Vec<f64>, best: Vec<f64> },
}

impl Accum {
    fn new(spec: ProductSpec) -> Self {
        Self {
            spec,
            state: None,
            failed: None,
            fallback_hours: Vec::new(),
        }
    }

    fn fold(&mut self, values: &[f64], hour: u16) {
        let hour_f = f64::from(hour);
        // Threshold reductions start from an empty accumulator and then take
        // the same per-hour path as every later hour, so hour one is not a
        // special case that silently skips the test.
        if let Reduce::Count(_)
        | Reduce::LongestRun(_)
        | Reduce::FirstHour(_)
        | Reduce::LastHour(_)
        | Reduce::PeakHour(_) = self.spec.reduce
        {
            if self.state.is_none() {
                self.state = Some(match self.spec.reduce {
                    Reduce::Count(_) => AccumState::Count(vec![0.0; values.len()]),
                    Reduce::LongestRun(_) => AccumState::LongestRun {
                        best: vec![0.0; values.len()],
                        current: vec![0.0; values.len()],
                    },
                    Reduce::FirstHour(_) | Reduce::LastHour(_) => {
                        AccumState::EdgeHour(vec![f64::NAN; values.len()])
                    }
                    _ => AccumState::PeakHour {
                        hour: vec![f64::NAN; values.len()],
                        best: vec![f64::NEG_INFINITY; values.len()],
                    },
                });
            }
        }
        match &mut self.state {
            None => {
                self.state = Some(match self.spec.reduce {
                    Reduce::Direct => AccumState::Direct(values.to_vec()),
                    Reduce::Sum => AccumState::Sum(values.to_vec()),
                    Reduce::Max => AccumState::Max(values.to_vec()),
                    Reduce::Min => AccumState::Min(values.to_vec()),
                    Reduce::Range => AccumState::Range {
                        max: values.to_vec(),
                        min: values.to_vec(),
                    },
                    _ => unreachable!("threshold reductions are initialized above"),
                });
            }
            Some(AccumState::Count(acc)) => {
                let Reduce::Count(threshold) = self.spec.reduce else {
                    return;
                };
                for (target, value) in acc.iter_mut().zip(values) {
                    if threshold.met(*value) {
                        *target += 1.0;
                    }
                }
            }
            Some(AccumState::LongestRun { best, current }) => {
                let Reduce::LongestRun(threshold) = self.spec.reduce else {
                    return;
                };
                for ((best, current), value) in best.iter_mut().zip(current.iter_mut()).zip(values) {
                    if threshold.met(*value) {
                        *current += 1.0;
                        *best = best.max(*current);
                    } else {
                        *current = 0.0;
                    }
                }
            }
            Some(AccumState::EdgeHour(acc)) => {
                let keep_last = matches!(self.spec.reduce, Reduce::LastHour(_));
                let threshold = match self.spec.reduce {
                    Reduce::FirstHour(threshold) | Reduce::LastHour(threshold) => threshold,
                    _ => return,
                };
                for (target, value) in acc.iter_mut().zip(values) {
                    if threshold.met(*value) && (keep_last || target.is_nan()) {
                        *target = hour_f;
                    }
                }
            }
            Some(AccumState::PeakHour { hour: hours, best }) => {
                let Reduce::PeakHour(threshold) = self.spec.reduce else {
                    return;
                };
                for ((slot, best), value) in hours.iter_mut().zip(best.iter_mut()).zip(values) {
                    // Strictly greater => the FIRST hour reaching the peak wins.
                    if threshold.met(*value) && *value > *best {
                        *best = *value;
                        *slot = hour_f;
                    }
                }
            }
            Some(AccumState::Direct(_)) => {
                unreachable!("direct windowed products fold exactly one hour")
            }
            Some(AccumState::Sum(acc)) => {
                for (target, value) in acc.iter_mut().zip(values) {
                    *target += *value;
                }
            }
            Some(AccumState::Max(acc)) => {
                for (target, value) in acc.iter_mut().zip(values) {
                    *target = target.max(*value);
                }
            }
            Some(AccumState::Min(acc)) => {
                for (target, value) in acc.iter_mut().zip(values) {
                    *target = target.min(*value);
                }
            }
            Some(AccumState::Range { max, min }) => {
                for ((max, min), value) in max.iter_mut().zip(min.iter_mut()).zip(values) {
                    *max = max.max(*value);
                    *min = min.min(*value);
                }
            }
        }
    }

    fn finish(self) -> Result<WindowedGrid, String> {
        if let Some(reason) = self.failed {
            return Err(reason);
        }
        let mut values = match self.state {
            None => {
                return Err("no stored hours folded into this window".to_string());
            }
            Some(AccumState::Direct(values))
            | Some(AccumState::Sum(values))
            | Some(AccumState::Max(values))
            | Some(AccumState::Min(values))
            | Some(AccumState::Count(values))
            | Some(AccumState::EdgeHour(values)) => values,
            Some(AccumState::Range { max, min }) => max
                .into_iter()
                .zip(min)
                .map(|(max, min)| max - min)
                .collect(),
            Some(AccumState::LongestRun { best, .. }) => best,
            Some(AccumState::PeakHour { hour, .. }) => hour,
        };
        // Count / timing reductions output HOURS, not the source variable, so a
        // display-unit conversion would corrupt them (e.g. multiplying an hour
        // count by 1.94 "knots"). Only value-valued reductions convert.
        let converts_units = matches!(
            self.spec.reduce,
            Reduce::Direct | Reduce::Sum | Reduce::Max | Reduce::Min | Reduce::Range
        );
        match if converts_units { self.spec.finish } else { Finish::None } {
            Finish::None => {}
            Finish::MmToInches => {
                for value in values.iter_mut() {
                    *value /= MM_PER_INCH;
                }
            }
            Finish::MsToKnots => {
                for value in values.iter_mut() {
                    *value *= MS_TO_KT;
                }
            }
            Finish::KgM3ToUgM3 => {
                for value in values.iter_mut() {
                    *value *= 1.0e9;
                }
            }
            Finish::KgM2ToMgM2 => {
                for value in values.iter_mut() {
                    *value *= 1.0e6;
                }
            }
            Finish::MetersToMiles => {
                for value in values.iter_mut() {
                    *value *= 0.000_621_371_2;
                }
            }
            Finish::PaToHpa => {
                for value in values.iter_mut() {
                    *value *= 0.01;
                }
            }
        }
        let mut strategy = self.spec.strategy;
        if !self.fallback_hours.is_empty() {
            strategy.push_str(&format!(
                " (top-of-hour instantaneous fallback at {}: no stored sub-hourly max \
                 field — a lower bound on the native sub-hourly max)",
                self.fallback_hours
                    .iter()
                    .map(|hour| format!("F{hour:03}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(WindowedGrid {
            slug: self.spec.product.slug().to_string(),
            units: self.spec.units.to_string(),
            title: self.spec.product.title().to_string(),
            values,
            hours_used: self.spec.hours,
            window_hours: self.spec.window_hours,
            strategy,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    use rustwx_core::{CanonicalField, FieldSelector, GridShape, LatLonGrid, SelectedField2D};
    use rw_store::ingest::{DerivedFieldInput, write_hour_from_fields_with_derived};

    const NX: usize = 2;
    const NY: usize = 2;
    const CELLS: usize = NX * NY;

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rw-windowed-store-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn grid() -> LatLonGrid {
        LatLonGrid::new(
            GridShape::new(NX, NY).unwrap(),
            vec![40.0, 40.0, 41.0, 41.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap()
    }

    fn field(selector: FieldSelector, units: &str, values: Vec<f32>) -> SelectedField2D {
        SelectedField2D {
            selector,
            units: units.to_string(),
            grid: grid(),
            values,
            projection: None,
        }
    }

    // --- deterministic per-(variable, hour, cell) synthetic planes ---

    fn apcp_1h_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| 0.25 * hour as f32 + 0.05 * cell as f32)
            .collect()
    }

    fn apcp_total_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| 10.0 + hour as f32 + 0.5 * cell as f32)
            .collect()
    }

    /// Non-monotonic in hour AND cell so pointwise maxima differ per cell.
    fn uh_plane(hour: u16) -> Vec<f32> {
        let by_hour: &[[f32; 4]] = &[
            [5.0, 50.0, 1.0, 0.0],  // F001
            [60.0, 10.0, 2.0, 0.0], // F002
            [20.0, 30.0, 3.0, 0.0], // F003
            [25.0, 5.0, 4.0, 0.0],  // F004
            [10.0, 45.0, 5.0, 0.0], // F005
            [30.0, 20.0, 6.0, 0.0], // F006
        ];
        by_hour[hour_row(hour, by_hour.len())].to_vec()
    }

    /// Row a forecast hour selects from a cyclic fixture table.
    ///
    /// F000 is a real stored hour (the analysis), and the 0-48 h regression
    /// guard writes `(0..=48)` — so `hour - 1` underflowed on u16 and every
    /// fixture panicked. Hour 0 takes the row cyclically BEFORE hour 1, which
    /// keeps every hour>=1 expectation in this module unchanged.
    fn hour_row(hour: u16, rows: usize) -> usize {
        (hour as usize + rows - 1) % rows
    }

    /// Sub-hourly 1 h max UH planes (`uh_2to5km_max_1h`): the hourly plane
    /// plus a positive sub-hourly excess, so a fold that wrongly read the
    /// instantaneous fallback would miss every expectation.
    fn uh_max_plane(hour: u16) -> Vec<f32> {
        uh_plane(hour).iter().map(|value| value + 6.25).collect()
    }

    /// Exact Pythagorean (u, v) pairs so hypot folds bit-exactly.
    fn wind_uv_planes(hour: u16) -> (Vec<f32>, Vec<f32>) {
        let by_hour: &[([f32; 4], [f32; 4])] = &[
            ([3.0, 0.0, 8.0, 20.0], [4.0, 5.0, 15.0, 21.0]), // speeds 5 5 17 29
            ([6.0, 5.0, 0.0, 3.0], [8.0, 12.0, 2.0, 4.0]),   // speeds 10 13 2 5
            ([0.0, 3.0, 6.0, 5.0], [5.0, 4.0, 8.0, 12.0]),   // speeds 5 5 10 13
            ([8.0, 0.0, 3.0, 0.0], [15.0, 1.0, 4.0, 2.0]),   // speeds 17 1 5 2
            ([20.0, 6.0, 0.0, 8.0], [21.0, 8.0, 5.0, 15.0]), // speeds 29 10 5 17
            ([5.0, 20.0, 3.0, 6.0], [12.0, 21.0, 4.0, 8.0]), // speeds 13 29 5 10
        ];
        let (u, v) = &by_hour[hour_row(hour, by_hour.len())];
        (u.to_vec(), v.to_vec())
    }

    /// Sub-hourly 1 h max wind speed plane (`wind_speed_10m_max_1h`, m/s):
    /// strictly above the hourly hypot(u, v) snapshot, so a fold that
    /// wrongly used the fallback would miss every expectation.
    fn wind_max_plane(hour: u16) -> Vec<f32> {
        let (u, v) = wind_uv_planes(hour);
        u.iter().zip(&v).map(|(&u, &v)| u.hypot(v) + 1.5).collect()
    }

    /// Quadratic in hour (peak at F012) so max/min land mid-window.
    fn temp_k_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| 285.0 + cell as f32 - 0.1 * (hour as f32 - 12.0) * (hour as f32 - 12.0))
            .collect()
    }

    /// Crosses 100 % at later hours to exercise the clamp.
    fn rh_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| -5.0 + 5.0 * hour as f32 + cell as f32)
            .collect()
    }

    fn dewpoint_k_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| 278.0 + 0.5 * cell as f32 + 0.2 * hour as f32)
            .collect()
    }

    fn vpd_plane(hour: u16) -> Vec<f32> {
        (0..CELLS)
            .map(|cell| 0.3 * hour as f32 + 0.1 * cell as f32)
            .collect()
    }

    /// Write one synthetic hour carrying every windowed source variable
    /// except `skip_vars`, mirroring the ingest's store names and native
    /// units (`temperature_2m` always present as the grid carrier).
    fn write_test_hour(store_root: &Path, run: &str, hour: u16, skip_vars: &[&str]) {
        let temp = field(
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            temp_k_plane(hour),
        );
        let dewpoint = field(
            FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
            "K",
            dewpoint_k_plane(hour),
        );
        let rh = field(
            FieldSelector::height_agl(CanonicalField::RelativeHumidity, 2),
            "%",
            rh_plane(hour),
        );
        let (u_values, v_values) = wind_uv_planes(hour);
        let u10 = field(
            FieldSelector::height_agl(CanonicalField::UWind, 10),
            "m/s",
            u_values,
        );
        let v10 = field(
            FieldSelector::height_agl(CanonicalField::VWind, 10),
            "m/s",
            v_values,
        );
        let apcp_1h = field(
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "kg/m^2",
            apcp_1h_plane(hour),
        );
        let apcp_total = field(
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "kg/m^2",
            apcp_total_plane(hour),
        );
        let uh = field(
            FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            "m^2/s^2",
            uh_plane(hour),
        );
        let uh_max = field(
            FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            "m^2/s^2",
            uh_max_plane(hour),
        );
        let wind_max = field(
            FieldSelector::height_agl(CanonicalField::WindSpeed, 10),
            "m/s",
            wind_max_plane(hour),
        );
        let mut fields: Vec<(&str, &SelectedField2D)> = vec![
            ("temperature_2m", &temp),
            ("dewpoint_2m", &dewpoint),
            ("rh_2m", &rh),
            ("u_10m", &u10),
            ("v_10m", &v10),
            ("apcp_run_total", &apcp_total),
            ("apcp_1h", &apcp_1h),
            ("uh_2to5km", &uh),
            ("uh_2to5km_max_1h", &uh_max),
            ("wind_speed_10m_max_1h", &wind_max),
        ];
        fields.retain(|(name, _)| !skip_vars.contains(name));
        let vpd_values = vpd_plane(hour);
        let mut derived = Vec::new();
        if !skip_vars.contains(&"vpd_2m") {
            derived.push(DerivedFieldInput {
                name: "vpd_2m",
                units: "hPa",
                values: &vpd_values,
            });
        }
        write_hour_from_fields_with_derived(
            store_root,
            "hrrr",
            run,
            hour,
            &fields,
            &derived,
            &[],
            "windowed-store-test",
            1_780_000_000 + hour as u64,
        )
        .unwrap();
    }

    fn write_test_run(store_root: &Path, run: &str, hours: &[u16]) {
        for &hour in hours {
            write_test_hour(store_root, run, hour, &[]);
        }
    }

    fn compute(
        store_root: &Path,
        run: &str,
        hours: &[u16],
        slugs: &[&str],
    ) -> WindowedStoreOutcome {
        compute_windowed_products(
            store_root,
            "hrrr",
            run,
            hours,
            &slugs.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            None,
        )
        .unwrap()
    }

    fn grid_named<'a>(outcome: &'a WindowedStoreOutcome, slug: &str) -> &'a WindowedGrid {
        outcome
            .grids
            .iter()
            .find(|grid| grid.slug == slug)
            .unwrap_or_else(|| panic!("'{slug}' must realize; blockers: {:?}", outcome.blockers))
    }

    fn blocker_reason<'a>(outcome: &'a WindowedStoreOutcome, slug: &str) -> &'a str {
        outcome
            .blockers
            .iter()
            .find(|(have, _)| have == slug)
            .map(|(_, reason)| reason.as_str())
            .unwrap_or_else(|| {
                panic!(
                    "'{slug}' must be blocked; realized: {:?}",
                    outcome.grids.iter().map(|g| &g.slug).collect::<Vec<_>>()
                )
            })
    }

    fn assert_values(grid: &WindowedGrid, expected: &[f64]) {
        assert_eq!(grid.values.len(), expected.len(), "{}: length", grid.slug);
        for (cell, (got, want)) in grid.values.iter().zip(expected).enumerate() {
            assert_eq!(
                got.to_bits(),
                want.to_bits(),
                "{} cell {cell}: got {got}, want {want}",
                grid.slug
            );
        }
    }

    #[test]
    fn six_hour_store_realizes_direct_trailing_and_run_windows_exactly() {
        let dir = test_dir("six-hour");
        let hours: Vec<u16> = (1..=6).collect();
        write_test_run(&dir, "20260608_00z", &hours);
        let outcome = compute(
            &dir,
            "20260608_00z",
            &hours,
            &[
                "qpf_1h",
                "qpf_6h",
                "qpf_total",
                "uh_2to5km_1h_max",
                "uh_2to5km_3h_max",
                "uh_2to5km_run_max",
                "10m_wind_1h_max",
                "10m_wind_run_max",
                "qpf_12h",
                "2m_temp_0_24h_max",
            ],
        );
        assert_eq!(outcome.anchor_hour, 6);
        assert_eq!(outcome.grids.len(), 8);
        assert_eq!(outcome.blockers.len(), 2);

        // qpf_1h: the stored trailing 1 h accumulation at F006, mm -> in.
        let qpf_1h = grid_named(&outcome, "qpf_1h");
        let expected: Vec<f64> = apcp_1h_plane(6)
            .iter()
            .map(|&mm| f64::from(mm) / MM_PER_INCH)
            .collect();
        assert_values(qpf_1h, &expected);
        assert_eq!(qpf_1h.units, "in");
        assert_eq!(qpf_1h.hours_used, vec![6]);
        assert_eq!(qpf_1h.window_hours, Some(1));

        // qpf_6h: sum of the six stored hourly increments, THEN mm -> in.
        let qpf_6h = grid_named(&outcome, "qpf_6h");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                let mm: f64 = (1..=6)
                    .map(|hour| f64::from(apcp_1h_plane(hour)[cell]))
                    .sum();
                mm / MM_PER_INCH
            })
            .collect();
        assert_values(qpf_6h, &expected);
        assert_eq!(qpf_6h.hours_used, (1..=6).collect::<Vec<u16>>());
        assert_eq!(qpf_6h.title, "6-h QPF");

        // qpf_total: the stored run-total accumulation at F006 (direct).
        let qpf_total = grid_named(&outcome, "qpf_total");
        let expected: Vec<f64> = apcp_total_plane(6)
            .iter()
            .map(|&mm| f64::from(mm) / MM_PER_INCH)
            .collect();
        assert_values(qpf_total, &expected);
        assert_eq!(qpf_total.hours_used, vec![6]);
        assert_eq!(qpf_total.window_hours, None);

        // UH: direct F006 sub-hourly max plane; trailing-3 and run maxima
        // fold the stored uh_2to5km_max_1h planes (NOT the instantaneous
        // uh_2to5km fallback, whose values sit strictly below).
        let uh_1h = grid_named(&outcome, "uh_2to5km_1h_max");
        assert_values(
            uh_1h,
            &uh_max_plane(6)
                .iter()
                .map(|&v| f64::from(v))
                .collect::<Vec<_>>(),
        );
        assert_eq!(uh_1h.units, "m^2/s^2");
        assert!(
            uh_1h.strategy.contains("uh_2to5km_max_1h") && !uh_1h.strategy.contains("fallback"),
            "strategy must name the stored max field with no fallback note: {}",
            uh_1h.strategy
        );
        let uh_3h = grid_named(&outcome, "uh_2to5km_3h_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (4..=6)
                    .map(|hour| f64::from(uh_max_plane(hour)[cell]))
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect();
        assert_values(uh_3h, &expected);
        assert_eq!(uh_3h.hours_used, vec![4, 5, 6]);
        assert_eq!(uh_3h.window_hours, Some(3));
        let uh_run = grid_named(&outcome, "uh_2to5km_run_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=6)
                    .map(|hour| f64::from(uh_max_plane(hour)[cell]))
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect();
        assert_values(uh_run, &expected);

        // Wind: the stored sub-hourly max speeds (m/s) fold, THEN -> knots.
        let wind_1h = grid_named(&outcome, "10m_wind_1h_max");
        let expected: Vec<f64> = wind_max_plane(6)
            .iter()
            .map(|&speed| f64::from(speed) * MS_TO_KT)
            .collect();
        assert_values(wind_1h, &expected);
        assert_eq!(wind_1h.units, "kt");
        assert!(
            wind_1h.strategy.contains("wind_speed_10m_max_1h")
                && !wind_1h.strategy.contains("fallback"),
            "strategy must name the stored max field with no fallback note: {}",
            wind_1h.strategy
        );
        let wind_run = grid_named(&outcome, "10m_wind_run_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=6)
                    .map(|hour| f64::from(wind_max_plane(hour)[cell]))
                    .fold(f64::NEG_INFINITY, f64::max)
                    * MS_TO_KT
            })
            .collect();
        assert_values(wind_run, &expected);

        // Window minimums block with the GRIB lane's reasons.
        assert!(blocker_reason(&outcome, "qpf_12h").contains(">= 12"));
        assert!(blocker_reason(&outcome, "2m_temp_0_24h_max").contains(">= 24"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_windows_reduce_max_min_range_exactly_over_24_hours() {
        let dir = test_dir("snapshot-24h");
        let hours: Vec<u16> = (1..=24).collect();
        write_test_run(&dir, "20260608_00z", &hours);
        let outcome = compute(
            &dir,
            "20260608_00z",
            &hours,
            &[
                "2m_temp_0_24h_max",
                "2m_temp_0_24h_min",
                "2m_temp_0_24h_range",
                "2m_rh_0_24h_max",
                "2m_vpd_0_24h_min",
                "2m_dewpoint_0_24h_range",
                "qpf_24h",
                "10m_wind_0_24h_max",
                "2m_temp_24_48h_max",
                "2m_temp_0_48h_range",
            ],
        );
        assert_eq!(outcome.anchor_hour, 24);
        assert_eq!(outcome.grids.len(), 8);
        assert_eq!(outcome.blockers.len(), 2);

        // Mirror the fold in f64: K -> degC per hour, then pointwise ops.
        let temp_c = |hour: u16, cell: usize| f64::from(temp_k_plane(hour)[cell]) - 273.15;
        let fold = |cell: usize, op: fn(f64, f64) -> f64, init: f64| {
            (1..=24).map(|hour| temp_c(hour, cell)).fold(init, op)
        };
        let max: Vec<f64> = (0..CELLS)
            .map(|cell| fold(cell, f64::max, f64::NEG_INFINITY))
            .collect();
        let min: Vec<f64> = (0..CELLS)
            .map(|cell| fold(cell, f64::min, f64::INFINITY))
            .collect();
        let range: Vec<f64> = max.iter().zip(&min).map(|(max, min)| max - min).collect();
        let temp_max = grid_named(&outcome, "2m_temp_0_24h_max");
        assert_values(temp_max, &max);
        assert_eq!(temp_max.units, "degC");
        assert_eq!(temp_max.hours_used, (1..=24).collect::<Vec<u16>>());
        assert_eq!(temp_max.window_hours, Some(24));
        assert_values(grid_named(&outcome, "2m_temp_0_24h_min"), &min);
        assert_values(grid_named(&outcome, "2m_temp_0_24h_range"), &range);

        // RH max: raw values cross 100 at late hours; the clamp must hold
        // the fold at exactly 100.
        let rh_max = grid_named(&outcome, "2m_rh_0_24h_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=24)
                    .map(|hour| f64::from(rh_plane(hour)[cell]).clamp(0.0, 100.0))
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect();
        assert_values(rh_max, &expected);
        assert!(rh_max.values.iter().all(|&v| v == 100.0));
        assert_eq!(rh_max.units, "%");

        // VPD min reads the ingest-computed derived grid (hPa, no convert).
        let vpd_min = grid_named(&outcome, "2m_vpd_0_24h_min");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=24)
                    .map(|hour| f64::from(vpd_plane(hour)[cell]))
                    .fold(f64::INFINITY, f64::min)
            })
            .collect();
        assert_values(vpd_min, &expected);
        assert_eq!(vpd_min.units, "hPa");

        // Dewpoint range: K -> degC per hour first (range is invariant to
        // the offset, but the fold path is the converted one).
        let dew_range = grid_named(&outcome, "2m_dewpoint_0_24h_range");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                let values = (1..=24).map(|hour| f64::from(dewpoint_k_plane(hour)[cell]) - 273.15);
                values.clone().fold(f64::NEG_INFINITY, f64::max)
                    - values.fold(f64::INFINITY, f64::min)
            })
            .collect();
        assert_values(dew_range, &expected);

        // qpf_24h sums all 24 stored hourly increments.
        let qpf_24h = grid_named(&outcome, "qpf_24h");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                let mm: f64 = (1..=24)
                    .map(|hour| f64::from(apcp_1h_plane(hour)[cell]))
                    .sum();
                mm / MM_PER_INCH
            })
            .collect();
        assert_values(qpf_24h, &expected);

        // 48 h windows block: only 24 hours are stored.
        assert!(blocker_reason(&outcome, "2m_temp_24_48h_max").contains(">= 48"));
        assert!(blocker_reason(&outcome, "2m_temp_0_48h_range").contains(">= 48"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn gaps_block_windows_instead_of_silently_skipping() {
        let dir = test_dir("gaps");
        let hours: Vec<u16> = vec![1, 2, 4];
        write_test_run(&dir, "20260608_00z", &hours);
        let outcome = compute(
            &dir,
            "20260608_00z",
            &hours,
            &[
                "uh_2to5km_3h_max",
                "uh_2to5km_run_max",
                "10m_wind_run_max",
                "qpf_1h",
                "qpf_total",
                "uh_2to5km_1h_max",
            ],
        );
        assert_eq!(outcome.anchor_hour, 4);

        // The trailing 3 h window F002-F004 is missing F003: blocked, with
        // the gap named — never computed from the two present hours.
        let reason = blocker_reason(&outcome, "uh_2to5km_3h_max");
        assert!(reason.contains("F003"), "gap must be named: {reason}");
        assert!(
            reason.contains("never skipped"),
            "no-silent-gap contract must be stated: {reason}"
        );
        assert!(blocker_reason(&outcome, "uh_2to5km_run_max").contains("F003"));
        assert!(blocker_reason(&outcome, "10m_wind_run_max").contains("F003"));

        // Direct single-hour products at the anchor still realize.
        let qpf_1h = grid_named(&outcome, "qpf_1h");
        let expected: Vec<f64> = apcp_1h_plane(4)
            .iter()
            .map(|&mm| f64::from(mm) / MM_PER_INCH)
            .collect();
        assert_values(qpf_1h, &expected);
        assert_eq!(grid_named(&outcome, "uh_2to5km_1h_max").hours_used, vec![4]);
        assert!(outcome.grids.iter().any(|grid| grid.slug == "qpf_total"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_variables_block_only_the_products_that_need_them() {
        let dir = test_dir("missing-vars");
        write_test_hour(&dir, "20260608_00z", 1, &[]);
        write_test_hour(&dir, "20260608_00z", 2, &["uh_2to5km_max_1h", "uh_2to5km"]);
        write_test_hour(&dir, "20260608_00z", 3, &["wind_speed_10m_max_1h", "v_10m"]);
        let outcome = compute(
            &dir,
            "20260608_00z",
            &[1, 2, 3],
            &[
                "uh_2to5km_3h_max",
                "uh_2to5km_1h_max",
                "10m_wind_1h_max",
                "qpf_1h",
            ],
        );

        // F002 lacks both the max field AND the instantaneous fallback:
        // the 3 h window dies with both variables and the hour named; the
        // 1 h product (F003 only) still realizes.
        let reason = blocker_reason(&outcome, "uh_2to5km_3h_max");
        assert!(
            reason.contains("uh_2to5km_max_1h")
                && reason.contains("uh_2to5km")
                && reason.contains("F002"),
            "reason must name both variables and the hour: {reason}"
        );
        assert!(outcome.grids.iter().any(|g| g.slug == "uh_2to5km_1h_max"));

        // F003 lacks the wind max field and v_10m: the wind speed product
        // blocks naming the failed fallback input.
        let reason = blocker_reason(&outcome, "10m_wind_1h_max");
        assert!(
            reason.contains("wind_speed_10m_max_1h") && reason.contains("v_10m"),
            "{reason}"
        );

        // Unrelated products are untouched.
        assert!(outcome.grids.iter().any(|g| g.slug == "qpf_1h"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_max_fields_fall_back_to_instantaneous_with_lower_bound_note() {
        let dir = test_dir("fallback");
        for hour in 1..=3 {
            write_test_hour(
                &dir,
                "20260608_00z",
                hour,
                &["uh_2to5km_max_1h", "wind_speed_10m_max_1h"],
            );
        }
        let outcome = compute(
            &dir,
            "20260608_00z",
            &[1, 2, 3],
            &["uh_2to5km_3h_max", "10m_wind_run_max", "qpf_1h"],
        );

        // UH folds the instantaneous uh_2to5km planes and says so.
        let uh_3h = grid_named(&outcome, "uh_2to5km_3h_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=3)
                    .map(|hour| f64::from(uh_plane(hour)[cell]))
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect();
        assert_values(uh_3h, &expected);
        assert!(
            uh_3h.strategy.contains("F001, F002, F003") && uh_3h.strategy.contains("lower bound"),
            "strategy must name every fallback hour and the lower-bound caveat: {}",
            uh_3h.strategy
        );

        // Wind folds hypot(u_10m, v_10m) and says so.
        let wind_run = grid_named(&outcome, "10m_wind_run_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=3)
                    .map(|hour| {
                        let (u, v) = wind_uv_planes(hour);
                        f64::from(u[cell]).hypot(f64::from(v[cell]))
                    })
                    .fold(f64::NEG_INFINITY, f64::max)
                    * MS_TO_KT
            })
            .collect();
        assert_values(wind_run, &expected);
        assert!(
            wind_run.strategy.contains("lower bound"),
            "{}",
            wind_run.strategy
        );

        // Products that never touch the max fields carry no note.
        let qpf_1h = grid_named(&outcome, "qpf_1h");
        assert!(!qpf_1h.strategy.contains("fallback"), "{}", qpf_1h.strategy);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mixed_stores_fall_back_only_for_hours_missing_the_max_field() {
        let dir = test_dir("mixed-fallback");
        write_test_hour(&dir, "20260608_00z", 1, &[]);
        write_test_hour(&dir, "20260608_00z", 2, &["uh_2to5km_max_1h"]);
        write_test_hour(&dir, "20260608_00z", 3, &[]);
        let outcome = compute(&dir, "20260608_00z", &[1, 2, 3], &["uh_2to5km_3h_max"]);

        // F001/F003 fold the stored max planes; F002 folds the
        // instantaneous fallback plane.
        let uh_3h = grid_named(&outcome, "uh_2to5km_3h_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                f64::from(uh_max_plane(1)[cell])
                    .max(f64::from(uh_plane(2)[cell]))
                    .max(f64::from(uh_max_plane(3)[cell]))
            })
            .collect();
        assert_values(uh_3h, &expected);
        assert!(
            uh_3h.strategy.contains("F002")
                && !uh_3h.strategy.contains("F001")
                && !uh_3h.strategy.contains("F003"),
            "the note must name exactly the fallback hour: {}",
            uh_3h.strategy
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unexpected_stored_units_block_instead_of_converting_blindly() {
        let dir = test_dir("bad-units");
        // Hand-build an hour whose apcp_1h claims inches and whose UH max
        // field claims knots (beside a perfectly good instantaneous
        // uh_2to5km): the lane must refuse rather than divide by 25.4
        // again, and unit drift on the max field must block — NOT silently
        // fall back to the instantaneous plane.
        let temp = field(
            FieldSelector::height_agl(CanonicalField::Temperature, 2),
            "K",
            temp_k_plane(1),
        );
        let apcp_bad = field(
            FieldSelector::surface(CanonicalField::TotalPrecipitation),
            "in",
            apcp_1h_plane(1),
        );
        let uh = field(
            FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            "m^2/s^2",
            uh_plane(1),
        );
        let uh_max_bad = field(
            FieldSelector::height_layer_agl(CanonicalField::UpdraftHelicity, 2000, 5000),
            "kt",
            uh_max_plane(1),
        );
        write_hour_from_fields_with_derived(
            &dir,
            "hrrr",
            "20260608_00z",
            1,
            &[
                ("temperature_2m", &temp),
                ("apcp_1h", &apcp_bad),
                ("uh_2to5km", &uh),
                ("uh_2to5km_max_1h", &uh_max_bad),
            ],
            &[],
            &[],
            "windowed-store-test",
            1_780_000_001,
        )
        .unwrap();
        let outcome = compute(&dir, "20260608_00z", &[1], &["qpf_1h", "uh_2to5km_1h_max"]);
        let reason = blocker_reason(&outcome, "qpf_1h");
        assert!(
            reason.contains("units 'in'") && reason.contains("kg/m^2"),
            "reason must name actual and expected units: {reason}"
        );
        let reason = blocker_reason(&outcome, "uh_2to5km_1h_max");
        assert!(
            reason.contains("units 'kt'") && reason.contains("m^2/s^2"),
            "unit drift on the max field must block, not fall back: {reason}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// Structural guard for the dbd322f class of bug: any product whose planned
    /// window is the SAME at two different anchors is a fixed window and must be
    /// classified run-scoped, or the interactive anchor override will block it.
    /// This is what caught `10m_wind_0_24h_max` and friends being planned inline
    /// in `plan_product` instead of through `snapshot_plan`.
    #[test]
    fn every_fixed_window_product_is_classified_run_scoped() {
        for &product in HrrrWindowedProduct::supported_products() {
            let (Ok(high), Ok(low)) = (plan_product(product, 48), plan_product(product, 47)) else {
                continue;
            };
            if high.hours == low.hours {
                assert!(
                    product_is_run_scoped(product),
                    "'{}' has an anchor-independent window {:?} but is not run-scoped, so an \
                     interactive request below its window end would block it",
                    product.slug(),
                    high.hours.first().zip(high.hours.last()),
                );
            }
        }
    }

    #[test]
    fn anchor_override_ends_window_at_requested_hour() {
        let dir = test_dir("anchor-override");
        let hours: Vec<u16> = (1..=12).collect();
        write_test_run(&dir, "20260608_00z", &hours);

        // Default (batch/pipeline): anchor at the max stored hour.
        let default = compute(&dir, "20260608_00z", &hours, &["qpf_1h", "qpf_6h"]);
        assert_eq!(default.anchor_hour, 12);
        assert_eq!(grid_named(&default, "qpf_1h").hours_used, vec![12]);

        // Interactive: honor the requested anchor hour (the reported bug —
        // QPF used to always render the max hour regardless of the request).
        let anchored = compute_windowed_products(
            &dir,
            "hrrr",
            "20260608_00z",
            &hours,
            &["qpf_1h".to_string(), "qpf_6h".to_string()],
            Some(6),
        )
        .unwrap();
        assert_eq!(anchored.anchor_hour, 6);
        assert_eq!(grid_named(&anchored, "qpf_1h").hours_used, vec![6]);
        assert_eq!(
            grid_named(&anchored, "qpf_6h").hours_used,
            (1..=6).collect::<Vec<u16>>()
        );

        // Run-scoped products IGNORE the override: the fixed 0-24 h snapshot
        // window and `*_run_max` describe the run, not the requested hour.
        // Regression guard for dbd322f, which blocked every 0-24/24-48/0-48 h
        // product and truncated run-max whenever an earlier hour was asked for.
        let hours48: Vec<u16> = (0..=48).collect();
        write_test_run(&dir, "20260608_06z", &hours48);
        let scoped = compute_windowed_products(
            &dir,
            "hrrr",
            "20260608_06z",
            &hours48,
            &[
                "2m_temp_0_24h_max".to_string(),
                "2m_temp_24_48h_max".to_string(),
                "10m_wind_0_24h_max".to_string(),
                "10m_wind_24_48h_max".to_string(),
                "10m_wind_0_48h_max".to_string(),
                "10m_wind_run_max".to_string(),
                "qpf_1h".to_string(),
            ],
            Some(6),
        )
        .unwrap();
        assert!(
            scoped.blockers.is_empty(),
            "run-scoped windows must not block on an earlier requested hour: {:?}",
            scoped.blockers
        );
        // Snapshot windows are F001-F024 / F025-F048 (hour 0 is the analysis).
        assert_eq!(
            grid_named(&scoped, "2m_temp_0_24h_max").hours_used,
            (1..=24).collect::<Vec<u16>>()
        );
        assert_eq!(
            grid_named(&scoped, "2m_temp_24_48h_max").hours_used,
            (25..=48).collect::<Vec<u16>>()
        );
        // The 10 m wind windows are fixed too (planned inline, not via
        // snapshot_plan) and must survive an earlier requested hour.
        assert_eq!(
            grid_named(&scoped, "10m_wind_0_24h_max").hours_used,
            (1..=24).collect::<Vec<u16>>()
        );
        assert_eq!(
            grid_named(&scoped, "10m_wind_24_48h_max").hours_used,
            (25..=48).collect::<Vec<u16>>()
        );
        // Run-max spans the whole run, not 0..requested.
        assert_eq!(
            *grid_named(&scoped, "10m_wind_run_max")
                .hours_used
                .last()
                .expect("run max has hours"),
            48
        );
        // The trailing product in the same batch still honors the override.
        assert_eq!(grid_named(&scoped, "qpf_1h").hours_used, vec![6]);

        // An anchor that is not a stored hour errors, never silently snaps.
        let err = compute_windowed_products(
            &dir,
            "hrrr",
            "20260608_00z",
            &hours,
            &["qpf_1h".to_string()],
            Some(99),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("F099"), "{err}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_slugs_error_and_duplicates_dedupe() {
        let dir = test_dir("slugs");
        write_test_run(&dir, "20260608_00z", &[1]);
        let err = compute_windowed_products(
            &dir,
            "hrrr",
            "20260608_00z",
            &[1],
            &["not_a_windowed_product".to_string()],
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not_a_windowed_product"), "{err}");

        let outcome = compute(&dir, "20260608_00z", &[1], &["qpf_1h", "qpf_1h", "qpf_1h"]);
        assert_eq!(outcome.grids.len(), 1, "duplicates must dedupe");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stored_run_hours_reads_the_manifest() {
        let dir = test_dir("manifest");
        write_test_run(&dir, "20260608_00z", &[1, 2, 5]);
        let hours = stored_run_hours(&dir, "hrrr", "20260608_00z").unwrap();
        assert_eq!(hours, vec![1, 2, 5]);
        assert!(stored_run_hours(&dir, "hrrr", "20990101_00z").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}

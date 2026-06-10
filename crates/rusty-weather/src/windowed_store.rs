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
//! * 2-5 km UH — pointwise maxima of the stored hourly `uh_2to5km` plane.
//!   DOCUMENTED SEMANTIC SHIFT: the stored plane is the instantaneous UPHL
//!   message (extraction scores a start-hour match (0) above a statistical
//!   end-hour match (1), so UPHL beats MXUPHL), while the GRIB windowed
//!   lane reduced the native sub-hourly MXUPHL maxima. Store-side UH
//!   windows are therefore maxima of top-of-hour snapshots — a lower bound
//!   on the GRIB lane's sub-hourly maxima.
//! * 10 m wind — pointwise maxima of hourly wind speed computed as
//!   hypot(`u_10m`, `v_10m`) (m/s folds first, knots out). Same shift: the
//!   GRIB lane used the native sub-hourly `WIND:10 m above ground` max
//!   field, which the store does not carry; top-of-hour speeds are a lower
//!   bound on it.
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
/// `<store_root>/<model_slug>/<run_slug>/`, anchored at the max hour in
/// `available_hours`. Unknown slugs are an error (the caller validates
/// requests against `HrrrWindowedProduct::supported_products()`); windows
/// that do not fit the available hours come back as blockers, never as
/// silently shortened windows.
pub fn compute_windowed_products(
    store_root: &Path,
    model_slug: &str,
    run_slug: &str,
    available_hours: &[u16],
    requested: &[String],
) -> Result<WindowedStoreOutcome, Box<dyn std::error::Error>> {
    let available: BTreeSet<u16> = available_hours.iter().copied().collect();
    let Some(&anchor_hour) = available.iter().next_back() else {
        return Err("windowed compute needs at least one stored hour".into());
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
    for slug in requested {
        if !seen.insert(slug.as_str()) {
            continue;
        }
        let product = HrrrWindowedProduct::from_slug(slug)
            .ok_or_else(|| format!("'{slug}' is not a windowed product slug"))?;
        let spec = match plan_product(product, anchor_hour) {
            Ok(spec) => spec,
            Err(reason) => {
                blockers.push((slug.clone(), reason));
                continue;
            }
        };
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
                Ok(values) => {
                    for accum in accums.iter_mut() {
                        if needs(accum, kind) {
                            accum.fold(&values);
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
        anchor_hour,
    })
}

/// The stored source plane a windowed product reduces.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum SourceKind {
    /// `apcp_1h` (kg/m^2 == mm), the trailing (h-1)->h accumulation.
    Apcp1h,
    /// `apcp_run_total` (kg/m^2 == mm), the 0->h run accumulation.
    ApcpRunTotal,
    /// `uh_2to5km` (m^2/s^2), the hourly instantaneous UPHL plane.
    Uh2to5km,
    /// hypot(`u_10m`, `v_10m`) in m/s.
    WindSpeed10m,
    /// `temperature_2m` converted K -> degC per hour.
    Temp2mC,
    /// `rh_2m` clamped to 0..100 %.
    Rh2mPct,
    /// `dewpoint_2m` converted K -> degC per hour.
    Dewpoint2mC,
    /// `vpd_2m` (hPa), the ingest-computed derived grid.
    Vpd2mHpa,
}

/// How the per-hour planes reduce into the product grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reduce {
    /// Single stored plane (1 h / run-total accumulations, 1 h UH/wind).
    Direct,
    Sum,
    Max,
    Min,
    /// Pointwise max - min over the window.
    Range,
}

/// Display-unit conversion applied AFTER the fold (the GRIB lane's order:
/// QPF sums millimeters then divides; wind maxes m/s then multiplies).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Finish {
    None,
    MmToInches,
    MsToKnots,
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
                format!("stored hourly 2-5 km UH plane (uh_2to5km) at F{end:03}"),
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
                "pointwise max of stored hourly 2-5 km UH planes across trailing 3 hours"
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
                "run max of stored hourly 2-5 km UH planes".to_string(),
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
                format!("stored 10 m wind speed (hypot of u_10m/v_10m) at F{end:03}"),
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
                "run max of stored hourly 10 m wind speeds (hypot of u_10m/v_10m)".to_string(),
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
                "max of stored hourly 10 m wind speeds across F001-F024".to_string(),
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
                "max of stored hourly 10 m wind speeds across F025-F048".to_string(),
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
                "max of stored hourly 10 m wind speeds across F001-F048".to_string(),
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

/// Read one source plane for one hour, unit-checked and transformed to the
/// per-hour values the fold consumes (the GRIB lane's per-hour transforms:
/// K -> degC, RH clamp; accumulation/UH/wind planes stay raw — their
/// display conversion happens after the fold).
fn read_source_plane(
    reader: &HourReader,
    grid: &GridFile,
    kind: SourceKind,
    hour: u16,
) -> Result<Vec<f64>, String> {
    let read = |name: &str, expected_units: &str| -> Result<Vec<f32>, String> {
        match read_grid_2d(reader, grid, name) {
            Ok(stored) => {
                if stored.units != expected_units {
                    return Err(format!(
                        "stored '{name}' at F{hour:03} has units '{}', expected '{expected_units}'",
                        stored.units
                    ));
                }
                Ok(stored.values)
            }
            Err(RwStoreError::UnknownVariable(_)) => {
                Err(format!("stored hour F{hour:03} has no '{name}' variable"))
            }
            Err(err) => Err(format!("read '{name}' from stored hour F{hour:03}: {err}")),
        }
    };
    match kind {
        SourceKind::Apcp1h => Ok(to_f64(read("apcp_1h", "kg/m^2")?)),
        SourceKind::ApcpRunTotal => Ok(to_f64(read("apcp_run_total", "kg/m^2")?)),
        SourceKind::Uh2to5km => Ok(to_f64(read("uh_2to5km", "m^2/s^2")?)),
        SourceKind::WindSpeed10m => {
            let u = read("u_10m", "m/s")?;
            let v = read("v_10m", "m/s")?;
            Ok(u.iter()
                .zip(&v)
                .map(|(&u, &v)| f64::from(u).hypot(f64::from(v)))
                .collect())
        }
        SourceKind::Temp2mC | SourceKind::Dewpoint2mC => {
            let name = if kind == SourceKind::Temp2mC {
                "temperature_2m"
            } else {
                "dewpoint_2m"
            };
            Ok(read(name, "K")?
                .iter()
                .map(|&value| f64::from(value) - 273.15)
                .collect())
        }
        SourceKind::Rh2mPct => Ok(read("rh_2m", "%")?
            .iter()
            .map(|&value| f64::from(value).clamp(0.0, 100.0))
            .collect()),
        SourceKind::Vpd2mHpa => Ok(to_f64(read("vpd_2m", "hPa")?)),
    }
}

fn to_f64(values: Vec<f32>) -> Vec<f64> {
    values.into_iter().map(f64::from).collect()
}

/// Per-product streaming accumulator: per-hour planes fold in ascending
/// hour order; `failed` records the first per-hour read failure (the
/// product's blocker reason — once failed, later hours stop folding).
struct Accum {
    spec: ProductSpec,
    state: Option<AccumState>,
    failed: Option<String>,
}

enum AccumState {
    Sum(Vec<f64>),
    Max(Vec<f64>),
    Min(Vec<f64>),
    Range { max: Vec<f64>, min: Vec<f64> },
    Direct(Vec<f64>),
}

impl Accum {
    fn new(spec: ProductSpec) -> Self {
        Self {
            spec,
            state: None,
            failed: None,
        }
    }

    fn fold(&mut self, values: &[f64]) {
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
                });
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
            | Some(AccumState::Min(values)) => values,
            Some(AccumState::Range { max, min }) => max
                .into_iter()
                .zip(min)
                .map(|(max, min)| max - min)
                .collect(),
        };
        match self.spec.finish {
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
        }
        Ok(WindowedGrid {
            slug: self.spec.product.slug().to_string(),
            units: self.spec.units.to_string(),
            title: self.spec.product.title().to_string(),
            values,
            hours_used: self.spec.hours,
            window_hours: self.spec.window_hours,
            strategy: self.spec.strategy,
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
        by_hour[(hour as usize - 1) % by_hour.len()].to_vec()
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
        let (u, v) = &by_hour[(hour as usize - 1) % by_hour.len()];
        (u.to_vec(), v.to_vec())
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
        let mut fields: Vec<(&str, &SelectedField2D)> = vec![
            ("temperature_2m", &temp),
            ("dewpoint_2m", &dewpoint),
            ("rh_2m", &rh),
            ("u_10m", &u10),
            ("v_10m", &v10),
            ("apcp_run_total", &apcp_total),
            ("apcp_1h", &apcp_1h),
            ("uh_2to5km", &uh),
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

        // UH: direct F006 plane; trailing-3 and run maxima are pointwise.
        let uh_1h = grid_named(&outcome, "uh_2to5km_1h_max");
        assert_values(
            uh_1h,
            &uh_plane(6)
                .iter()
                .map(|&v| f64::from(v))
                .collect::<Vec<_>>(),
        );
        assert_eq!(uh_1h.units, "m^2/s^2");
        let uh_3h = grid_named(&outcome, "uh_2to5km_3h_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (4..=6)
                    .map(|hour| f64::from(uh_plane(hour)[cell]))
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
                    .map(|hour| f64::from(uh_plane(hour)[cell]))
                    .fold(f64::NEG_INFINITY, f64::max)
            })
            .collect();
        assert_values(uh_run, &expected);

        // Wind: hourly speed = hypot(u, v) in m/s, maxed, THEN -> knots.
        let wind_1h = grid_named(&outcome, "10m_wind_1h_max");
        let (u, v) = wind_uv_planes(6);
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| f64::from(u[cell]).hypot(f64::from(v[cell])) * MS_TO_KT)
            .collect();
        assert_values(wind_1h, &expected);
        assert_eq!(wind_1h.units, "kt");
        let wind_run = grid_named(&outcome, "10m_wind_run_max");
        let expected: Vec<f64> = (0..CELLS)
            .map(|cell| {
                (1..=6)
                    .map(|hour| {
                        let (u, v) = wind_uv_planes(hour);
                        f64::from(u[cell]).hypot(f64::from(v[cell]))
                    })
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
        write_test_hour(&dir, "20260608_00z", 2, &["uh_2to5km"]);
        write_test_hour(&dir, "20260608_00z", 3, &["v_10m"]);
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

        // F002 lacks uh_2to5km: the 3 h window dies with the variable and
        // hour named; the 1 h product (F003 only) still realizes.
        let reason = blocker_reason(&outcome, "uh_2to5km_3h_max");
        assert!(
            reason.contains("uh_2to5km") && reason.contains("F002"),
            "reason must name the variable and hour: {reason}"
        );
        assert!(outcome.grids.iter().any(|g| g.slug == "uh_2to5km_1h_max"));

        // F003 lacks v_10m: the wind speed product blocks naming v_10m.
        let reason = blocker_reason(&outcome, "10m_wind_1h_max");
        assert!(reason.contains("v_10m"), "{reason}");

        // Unrelated products are untouched.
        assert!(outcome.grids.iter().any(|g| g.slug == "qpf_1h"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unexpected_stored_units_block_instead_of_converting_blindly() {
        let dir = test_dir("bad-units");
        // Hand-build an hour whose apcp_1h claims inches: the lane must
        // refuse rather than divide by 25.4 again.
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
        write_hour_from_fields_with_derived(
            &dir,
            "hrrr",
            "20260608_00z",
            1,
            &[("temperature_2m", &temp), ("apcp_1h", &apcp_bad)],
            &[],
            &[],
            "windowed-store-test",
            1_780_000_001,
        )
        .unwrap();
        let outcome = compute(&dir, "20260608_00z", &[1], &["qpf_1h"]);
        let reason = blocker_reason(&outcome, "qpf_1h");
        assert!(
            reason.contains("units 'in'") && reason.contains("kg/m^2"),
            "reason must name actual and expected units: {reason}"
        );
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

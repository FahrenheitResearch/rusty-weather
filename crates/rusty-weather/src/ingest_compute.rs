//! Ingest-time derived + heavy precompute: assemble the rustwx-products
//! compute inputs from the extracted fields ONCE while they sit in RAM,
//! then run all 29 non-heavy derived recipes
//! (`rustwx_products::derived::compute_store_derived_grids`) and all 16
//! heavy ECAPE-class recipes
//! (`rustwx_products::derived::compute_store_heavy_grids` — the exact
//! prep + kernel dispatch the heavy render lane runs) through the existing
//! products compute lanes, handing back f32 grids ready to store as
//! ordinary 2D variables named by recipe slug.
//!
//! No science lives here: moisture comes from the products crate's own
//! mixing-ratio helpers, and every recipe kernel is the rustwx-calc function
//! the render lanes call. This module only converts f32 extraction
//! planes into the f64 `SurfaceFields`/`PressureFields` shape the lanes
//! consume (in parallel — the conversions span ~350M values per HRRR hour).
//! Native CAPE planes (the heavy native-ratio denominators) arrive already
//! decoded as f64 from `rustwx_products::gridded::decode_native_cape_planes`
//! and are passed through untouched.

use rayon::prelude::*;
use rustwx_core::SelectedField2D;
use rustwx_products::derived::{
    StoreHeavyTiming, compute_store_derived_grids, compute_store_heavy_grids,
};
use rustwx_products::gridded::{
    NativeCapePlanes, PressureFields, SurfaceFields, mixing_ratio_from_dewpoint_k,
    mixing_ratio_from_relative_humidity,
};

/// One derived grid ready to store: variable name (the recipe slug), display
/// units, and full-grid row-major values.
pub struct DerivedGrid2D {
    pub name: &'static str,
    pub units: String,
    pub values: Vec<f32>,
}

/// The assembled products-side compute inputs for one extracted hour: the
/// f64 `SurfaceFields`/`PressureFields` pair every store compute stage
/// (derived and heavy) consumes. Built once per hour by
/// [`assemble_products_inputs`] so the ~350M-value f32→f64 conversion is
/// not repeated per stage.
pub struct ProductsComputeInputs {
    pub surface: SurfaceFields,
    pub pressure: PressureFields,
}

/// Output of the heavy (ECAPE-class) compute stage: realized grids,
/// recipes skipped with the products lane's documented reason, the ECAPE
/// triplet's per-column failure count, and the products lane's per-kernel
/// timing breakdown for honest stage reporting.
pub struct HeavyGrids2D {
    pub grids: Vec<DerivedGrid2D>,
    pub skipped: Vec<(&'static str, String)>,
    pub ecape_failure_count: usize,
    pub timing: StoreHeavyTiming,
}

/// How the moisture planes are expressed. Dewpoint is preferred (it is the
/// decode lane's first fallback after specific humidity, which extraction
/// does not carry); relative humidity is the last resort and needs the
/// temperature plane at the same level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoistureKind {
    DewpointK,
    RelativeHumidityPct,
}

/// The five extracted isobaric volumes feeding derived compute, as
/// `(level_hpa, plane)` pairs in any order. Only levels realized in all
/// five volumes are used, in descending-pressure (ground-up) order — the
/// same alignment rule as the decode lane's `common_isobaric_levels`.
pub struct IngestVolumes<'a> {
    pub temperature_k: &'a [(u16, Vec<f32>)],
    pub moisture: &'a [(u16, Vec<f32>)],
    pub moisture_kind: MoistureKind,
    pub u_ms: &'a [(u16, Vec<f32>)],
    pub v_ms: &'a [(u16, Vec<f32>)],
    pub height_m: &'a [(u16, Vec<f32>)],
}

/// Required 2D fields, by the store names `rw_ingest` assigns.
const SURFACE_TEMPERATURE_2M: &str = "temperature_2m";
const SURFACE_DEWPOINT_2M: &str = "dewpoint_2m";
const SURFACE_RH_2M: &str = "rh_2m";
const SURFACE_U_10M: &str = "u_10m";
const SURFACE_V_10M: &str = "v_10m";
const SURFACE_PRESSURE: &str = "surface_pressure";
const SURFACE_OROGRAPHY: &str = "orography";

/// Run the shared non-heavy compute pass on already-assembled inputs and
/// convert the grids back to f32. The expensive recipe kernels are
/// rayon-parallel inside rustwx-calc.
pub fn compute_derived_2d_from_inputs(
    inputs: &ProductsComputeInputs,
) -> Result<Vec<DerivedGrid2D>, Box<dyn std::error::Error>> {
    let grids = compute_store_derived_grids(&inputs.surface, &inputs.pressure)?;
    Ok(grids
        .into_iter()
        .map(|grid| DerivedGrid2D {
            name: grid.slug,
            units: grid.units,
            values: grid.values.par_iter().map(|&value| value as f32).collect(),
        })
        .collect())
}

/// Run the heavy ECAPE-class compute pass (the heavy render lane's exact
/// prep + kernels via `compute_store_heavy_grids`) on already-assembled
/// inputs and convert the grids back to f32. The native-CAPE ratio recipes
/// realize only when [`assemble_products_inputs`] received the matching
/// native plane; otherwise they come back in `skipped` with the products
/// lane's documented reason.
pub fn compute_heavy_2d_from_inputs(
    inputs: &ProductsComputeInputs,
) -> Result<HeavyGrids2D, Box<dyn std::error::Error>> {
    let heavy = compute_store_heavy_grids(&inputs.surface, &inputs.pressure)?;
    Ok(HeavyGrids2D {
        grids: heavy
            .grids
            .into_iter()
            .map(|grid| DerivedGrid2D {
                name: grid.slug,
                units: grid.units,
                values: grid.values.par_iter().map(|&value| value as f32).collect(),
            })
            .collect(),
        skipped: heavy
            .skipped
            .into_iter()
            .map(|skip| (skip.slug, skip.reason))
            .collect(),
        ecape_failure_count: heavy.ecape_failure_count,
        timing: heavy.timing,
    })
}

/// Assemble the products-side compute inputs from the extracted hour: f64
/// conversion, mixing-ratio math, and level alignment, all once. The
/// native CAPE planes (if any) ride on the `SurfaceFields` exactly as the
/// products decode lane carries them; the non-heavy compute ignores them
/// and the heavy stage uses them as the native-ratio denominators.
pub fn assemble_products_inputs(
    fields_2d: &[(&'static str, SelectedField2D)],
    volumes: &IngestVolumes<'_>,
    native_cape: NativeCapePlanes,
) -> Result<ProductsComputeInputs, Box<dyn std::error::Error>> {
    let find = |name: &str| {
        fields_2d
            .iter()
            .find(|(have, _)| *have == name)
            .map(|(_, field)| field)
    };
    let require = |name: &'static str| {
        find(name).ok_or_else(|| format!("derived precompute needs 2D field '{name}'"))
    };

    let t2 = require(SURFACE_TEMPERATURE_2M)?;
    let u10 = require(SURFACE_U_10M)?;
    let v10 = require(SURFACE_V_10M)?;
    let psfc = require(SURFACE_PRESSURE)?;
    let orog = require(SURFACE_OROGRAPHY)?;
    let (moisture_2m, moisture_2m_kind) = find(SURFACE_DEWPOINT_2M)
        .map(|field| (field, MoistureKind::DewpointK))
        .or_else(|| find(SURFACE_RH_2M).map(|field| (field, MoistureKind::RelativeHumidityPct)))
        .ok_or_else(|| {
            format!(
                "derived precompute needs 2D field '{SURFACE_DEWPOINT_2M}' or '{SURFACE_RH_2M}'"
            )
        })?;

    let nx = t2.grid.shape.nx;
    let ny = t2.grid.shape.ny;
    let nxy = nx * ny;
    for (name, field) in [
        (SURFACE_TEMPERATURE_2M, t2),
        (SURFACE_U_10M, u10),
        (SURFACE_V_10M, v10),
        (SURFACE_PRESSURE, psfc),
        (SURFACE_OROGRAPHY, orog),
    ] {
        if field.values.len() != nxy {
            return Err(format!(
                "derived precompute: 2D field '{name}' holds {} values, expected {nxy}",
                field.values.len()
            )
            .into());
        }
    }
    if moisture_2m.values.len() != nxy {
        return Err(format!(
            "derived precompute: 2 m moisture field holds {} values, expected {nxy}",
            moisture_2m.values.len()
        )
        .into());
    }
    for (name, plane) in [
        ("native sbcape", native_cape.sbcape_jkg.as_ref()),
        ("native mlcape", native_cape.mlcape_jkg.as_ref()),
        ("native mucape", native_cape.mucape_jkg.as_ref()),
    ] {
        if let Some(plane) = plane {
            if plane.len() != nxy {
                return Err(format!(
                    "derived precompute: {name} plane holds {} values, expected {nxy} \
                     (surface GRIB grid disagrees with the extraction grid?)",
                    plane.len()
                )
                .into());
            }
        }
    }

    // --- surface inputs: f64 conversion + the decode lane's moisture math ---
    let psfc_pa = to_f64(&psfc.values);
    let t2_k = to_f64(&t2.values);
    let q2_kgkg: Vec<f64> = match moisture_2m_kind {
        MoistureKind::DewpointK => psfc_pa
            .par_iter()
            .zip(moisture_2m.values.par_iter())
            .map(|(&psfc, &td_k)| mixing_ratio_from_dewpoint_k(psfc / 100.0, f64::from(td_k)))
            .collect(),
        MoistureKind::RelativeHumidityPct => psfc_pa
            .par_iter()
            .zip(t2_k.par_iter())
            .zip(moisture_2m.values.par_iter())
            .map(|((&psfc, &t_k), &rh)| {
                mixing_ratio_from_relative_humidity(psfc / 100.0, t_k, f64::from(rh))
            })
            .collect(),
    };
    let surface = SurfaceFields {
        lat: to_f64(&t2.grid.lat_deg),
        lon: to_f64(&t2.grid.lon_deg),
        nx,
        ny,
        projection: t2.projection.clone(),
        psfc_pa,
        orog_m: to_f64(&orog.values),
        orog_is_proxy: false,
        t2_k,
        q2_kgkg,
        u10_ms: to_f64(&u10.values),
        v10_ms: to_f64(&v10.values),
        native_sbcape_jkg: native_cape.sbcape_jkg,
        native_mlcape_jkg: native_cape.mlcape_jkg,
        native_mucape_jkg: native_cape.mucape_jkg,
        native_pblh_m: None,
    };

    // --- pressure inputs: align levels across all five volumes, ground up ---
    let levels = common_levels_descending(volumes);
    if levels.len() < 2 {
        return Err(format!(
            "derived precompute: only {} isobaric level(s) realized across all five volumes \
             (temperature/moisture/u/v/height); need at least 2",
            levels.len()
        )
        .into());
    }
    let temperature_c_3d = flatten_volume_with(
        volumes.temperature_k,
        &levels,
        nxy,
        "temperature_iso",
        |value| f64::from(value) - 273.15,
    )?;
    let qvapor_kgkg_3d = moisture_volume(volumes, &levels, nxy)?;
    let pressure = PressureFields {
        pressure_levels_hpa: levels.iter().map(|&level| f64::from(level)).collect(),
        pressure_3d_pa: None,
        temperature_c_3d,
        qvapor_kgkg_3d,
        u_ms_3d: flatten_volume(volumes.u_ms, &levels, nxy, "u_iso")?,
        v_ms_3d: flatten_volume(volumes.v_ms, &levels, nxy, "v_iso")?,
        gh_m_3d: flatten_volume(volumes.height_m, &levels, nxy, "height_iso")?,
        omega_pa_s_3d: None,
        absolute_vorticity_s_3d: None,
        cloud_liquid_kgkg_3d: None,
        cloud_ice_kgkg_3d: None,
        rain_kgkg_3d: None,
        snow_kgkg_3d: None,
        graupel_kgkg_3d: None,
    };

    Ok(ProductsComputeInputs { surface, pressure })
}

fn to_f64(values: &[f32]) -> Vec<f64> {
    values.par_iter().map(|&value| f64::from(value)).collect()
}

/// Levels realized in all five volumes, descending pressure (1000 hPa
/// first), deduplicated — index 0 of every flattened 3D array is the level
/// nearest the ground, the orientation the compute lane's height-AGL
/// assembly requires.
fn common_levels_descending(volumes: &IngestVolumes<'_>) -> Vec<u16> {
    let has =
        |planes: &[(u16, Vec<f32>)], level: u16| planes.iter().any(|(have, _)| *have == level);
    let mut levels: Vec<u16> = volumes
        .temperature_k
        .iter()
        .map(|(level, _)| *level)
        .filter(|&level| {
            has(volumes.moisture, level)
                && has(volumes.u_ms, level)
                && has(volumes.v_ms, level)
                && has(volumes.height_m, level)
        })
        .collect();
    levels.sort_unstable_by(|a, b| b.cmp(a));
    levels.dedup();
    levels
}

/// Mixing-ratio volume `[level][y][x]` from the moisture planes, using the
/// decode lane's own per-kind formula (dewpoint preferred, RH fallback with
/// the temperature plane at the same level). Converted per level in
/// parallel, straight from the f32 planes — no f64 staging copy.
fn moisture_volume(
    volumes: &IngestVolumes<'_>,
    levels: &[u16],
    nxy: usize,
) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let mut out = vec![0.0f64; levels.len() * nxy];
    out.par_chunks_mut(nxy)
        .zip(levels.par_iter())
        .try_for_each(|(chunk, &level)| -> Result<(), String> {
            let level_hpa = f64::from(level);
            match volumes.moisture_kind {
                MoistureKind::DewpointK => {
                    let dewpoint = plane_for(volumes.moisture, level, nxy, "dewpoint_iso")?;
                    for (dst, &td_k) in chunk.iter_mut().zip(dewpoint.iter()) {
                        *dst = mixing_ratio_from_dewpoint_k(level_hpa, f64::from(td_k));
                    }
                }
                MoistureKind::RelativeHumidityPct => {
                    let rh = plane_for(volumes.moisture, level, nxy, "rh_iso")?;
                    let temperature =
                        plane_for(volumes.temperature_k, level, nxy, "temperature_iso")?;
                    for ((dst, &rh_pct), &t_k) in
                        chunk.iter_mut().zip(rh.iter()).zip(temperature.iter())
                    {
                        *dst = mixing_ratio_from_relative_humidity(
                            level_hpa,
                            f64::from(t_k),
                            f64::from(rh_pct),
                        );
                    }
                }
            }
            Ok(())
        })
        .map_err(std::io::Error::other)?;
    Ok(out)
}

/// Look up one level's plane in a volume, length-checked.
fn plane_for<'p>(
    planes: &'p [(u16, Vec<f32>)],
    level: u16,
    nxy: usize,
    name: &str,
) -> Result<&'p [f32], String> {
    let (_, plane) = planes
        .iter()
        .find(|(have, _)| *have == level)
        .ok_or_else(|| format!("volume '{name}': aligned level {level} hPa missing"))?;
    if plane.len() != nxy {
        return Err(format!(
            "volume '{name}' level {level} hPa: plane holds {} values, expected {nxy}",
            plane.len()
        ));
    }
    Ok(plane.as_slice())
}

/// Flatten one volume to `[level][y][x]` f64 in the given level order,
/// converting per level in parallel.
fn flatten_volume(
    planes: &[(u16, Vec<f32>)],
    levels: &[u16],
    nxy: usize,
    name: &str,
) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    flatten_volume_with(planes, levels, nxy, name, f64::from)
}

/// [`flatten_volume`] with a per-value conversion fused into the single
/// parallel pass (used for the K -> Celsius temperature conversion).
fn flatten_volume_with(
    planes: &[(u16, Vec<f32>)],
    levels: &[u16],
    nxy: usize,
    name: &str,
    convert: impl Fn(f32) -> f64 + Sync,
) -> Result<Vec<f64>, Box<dyn std::error::Error>> {
    let mut out = vec![0.0f64; levels.len() * nxy];
    out.par_chunks_mut(nxy)
        .zip(levels.par_iter())
        .try_for_each(|(chunk, &level)| -> Result<(), String> {
            let plane = plane_for(planes, level, nxy, name)?;
            for (dst, &src) in chunk.iter_mut().zip(plane.iter()) {
                *dst = convert(src);
            }
            Ok(())
        })
        .map_err(std::io::Error::other)?;
    Ok(out)
}

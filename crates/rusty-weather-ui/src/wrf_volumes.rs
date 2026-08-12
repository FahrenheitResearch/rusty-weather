//! Build isobaric sounding volumes from a WRF file.
//!
//! Updated from BowEcho v0.30.5's memory-hardened volume builder so Rusty
//! Weather owns the optimized implementation consumed by desktop hosts.
//!
//! WRF is on native (eta) levels, but the skew-T builder
//! ([`rw_ui::skewt::build_sounding_column`]) needs the same `*_iso` isobaric
//! 3D variables the model ingest writes for HRRR/GFS: `temperature_iso`,
//! `dewpoint_iso`, `u_iso`, `v_iso`, `height_iso`. This module reads WRF's 3D
//! fields through `wrf-core`'s `getvar` (which already handles destaggering,
//! theta -> T, geopotential -> height, and QVAPOR -> Td) and log-pressure
//! interpolates each column onto the canonical isobaric levels, so imported
//! WRF runs produce soundings exactly like the downloaded models do.
#![allow(dead_code)]
// `try_interpolate_iso_volumes` takes the five column fields + shape as separate
// slices by design (the shared raw/post-processed reader contract); factoring
// them into a struct would only obscure the call sites.
#![allow(clippy::too_many_arguments)]

use rayon::{ThreadPool, ThreadPoolBuilder};
use rustwx_core::checked_volume_elements;
use rw_store::PressureVolumeInput;
use std::sync::OnceLock;
use wrf_core::{ComputeOpts, VarOutput, WrfFile, getvar};

const STANDARD_LEVEL_COUNT: usize = 37;
const MAX_NATIVE_F64_COMPONENT_COUNT: u128 = 8;
const ISO_VOLUME_COUNT: u128 = 5;
const SURFACE_F32_PLANE_COUNT: u128 = 5;
const MAX_WRF_ISO_THREADS: usize = 8;
const MIN_PARALLEL_ISO_CELLS: usize = 4_096;
/// Ceiling for buffers owned directly by the volume path. This deliberately
/// excludes wrf-core's memoization cache, whose lifetime is managed separately.
/// The known 800x800x79 workflow needs 3,722,240,000 bytes (~3.47 GiB) by this
/// accounting and therefore remains supported with useful allocation headroom.
const MAX_WRF_VOLUME_OWNED_BYTES: u128 = 4 * 1024 * 1024 * 1024;

/// Canonical isobaric levels (hPa), matching the model-ingest convention
/// (`100..=1000` step 25 -> 37 levels). Levels outside a column's model range
/// are left NaN and pruned by the sounding column builder.
fn standard_levels() -> Vec<u16> {
    (100..=1000u16).step_by(25).collect()
}

/// Preflight the complete known owned working set before any 3-D reads or
/// output allocations. The five `*_iso` products all share the canonical
/// 37-level shape. Raw wrfout owns at most six native-sized f64 components;
/// postprocessed severe processing can retain pressure/QVAPOR alongside its
/// derived hPa/dewpoint arrays and therefore reaches eight. We conservatively
/// budget eight for every caller. Five f32 lowest-level surface planes are also
/// included.
///
/// Callers that receive an error must not begin the 3-D volume read. A caller
/// that already owns independent 2-D products may retain them; a volume-only
/// caller may instead return the error. The returned value is the total known
/// owned byte count. The per-volume store ceiling remains an independent check
/// in addition to the aggregate 4 GiB desktop working-set ceiling.
pub(crate) fn preflight_iso_volume_shape(nz: usize, cells: usize) -> Result<u64, String> {
    if nz < 2 {
        return Err(format!(
            "WRF native pressure volume requires at least two levels, got {nz}"
        ));
    }
    if cells == 0 {
        return Err("WRF grid has zero cells".to_string());
    }
    let iso_elements = checked_volume_elements(STANDARD_LEVEL_COUNT, cells).map_err(|err| {
        format!("canonical {STANDARD_LEVEL_COUNT}-level WRF pressure volume is unsupported: {err}")
    })?;

    let native_bytes = checked_byte_product(
        "native WRF volume buffers",
        &[
            MAX_NATIVE_F64_COMPONENT_COUNT,
            nz as u128,
            cells as u128,
            std::mem::size_of::<f64>() as u128,
        ],
    )?;
    let iso_bytes = checked_byte_product(
        "isobaric WRF output buffers",
        &[
            ISO_VOLUME_COUNT,
            iso_elements as u128,
            std::mem::size_of::<f32>() as u128,
        ],
    )?;
    let surface_bytes = checked_byte_product(
        "WRF surface fallback buffers",
        &[
            SURFACE_F32_PLANE_COUNT,
            cells as u128,
            std::mem::size_of::<f32>() as u128,
        ],
    )?;
    let owned_bytes = native_bytes
        .checked_add(iso_bytes)
        .and_then(|bytes| bytes.checked_add(surface_bytes))
        .ok_or_else(|| "WRF volume owned-byte total overflows u128".to_string())?;
    if owned_bytes > MAX_WRF_VOLUME_OWNED_BYTES {
        return Err(format!(
            "WRF volume path requires {owned_bytes} known owned bytes for {nz} levels x {cells} cells, exceeding the {MAX_WRF_VOLUME_OWNED_BYTES}-byte (4 GiB) desktop ceiling"
        ));
    }
    u64::try_from(owned_bytes)
        .map_err(|_| format!("WRF volume owned-byte total {owned_bytes} does not fit u64"))
}

fn checked_byte_product(name: &str, factors: &[u128]) -> Result<u128, String> {
    factors.iter().try_fold(1u128, |product, &factor| {
        product
            .checked_mul(factor)
            .ok_or_else(|| format!("{name} factors {factors:?} overflow u128"))
    })
}

/// One isobaric volume ready for the store writer: owned row-major planes.
pub struct IsoVolume {
    pub name: String,
    pub units: String,
    /// `(level_hpa, plane)` where each plane holds `ny * nx` row-major values.
    pub levels: Vec<(u16, Vec<f32>)>,
}

impl IsoVolume {
    /// Borrowed view for the store writer's [`PressureVolumeInput`].
    pub fn as_input(&self) -> PressureVolumeInput<'_> {
        PressureVolumeInput {
            name: &self.name,
            units: &self.units,
            selector_template: serde_json::json!({
                "source": "wrf",
                "field": self.name,
                "vertical": "isobaric",
            }),
            levels: self
                .levels
                .iter()
                .map(|(hpa, plane)| (*hpa, plane.as_slice()))
                .collect(),
        }
    }
}

/// Lowest-model-level surface fallbacks, in the units the skew-T expects
/// (Pa, K, K, m/s, m/s). Used to synthesize the 2D surface fields a split
/// `wrf3d` file (CONUS404 / GDEX CONUS-II) omits — chiefly `PSFC` — so the
/// sounding can still start near the surface. Callers expose these substitutes
/// only under explicit `approx_*` names, never as true 2 m/10 m products. Each
/// plane is row-major `ny * nx`.
pub struct SurfaceFallback {
    pub surface_pressure_pa: Vec<f32>,
    pub temperature_2m_k: Vec<f32>,
    pub dewpoint_2m_k: Vec<f32>,
    pub u_10m: Vec<f32>,
    pub v_10m: Vec<f32>,
}

/// Read WRF 3D fields for `timeidx` and interpolate them to the canonical
/// isobaric levels, returning the five `*_iso` volumes the skew-T needs plus
/// the lowest-model-level [`SurfaceFallback`] (so callers can fill in any 2D
/// surface field the file omits).
///
/// `cells` is the horizontal grid size (`ny * nx`) of the hour being written;
/// every returned plane matches it. Fails (leaving the caller to skip volumes
/// and still write the 2D fields) if the required 3D fields are unreadable.
///
/// `progress` receives per-stage messages (which 3D field is being read /
/// getvar'd, then interpolation percentage) — on a 250 m grid each stage is
/// tens of seconds, and both import paths surface these lines in the dock.
pub fn build_iso_volumes(
    file: &WrfFile,
    timeidx: usize,
    cells: usize,
    progress: &mut dyn FnMut(String),
) -> Result<(Vec<IsoVolume>, SurfaceFallback), String> {
    // This must precede the first getvar: an otherwise valid 2-D grid can be
    // too large for either a 37-level dense store volume or the aggregate
    // native/output working set. In that case callers omit these products
    // rather than reading several enormous native fields.
    let (nx, ny, nz) = (file.nx, file.ny, file.nz);
    let file_cells = checked_dimension_product("WRF horizontal grid", &[ny, nx])?;
    if cells != file_cells {
        return Err(format!(
            "WRF caller supplied {cells} horizontal cells, but file dimensions [{ny}, {nx}] describe {file_cells}"
        ));
    }
    preflight_iso_volume_shape(nz, cells)?;
    let read = |name: &str, stage: &str| -> Result<VarOutput, String> {
        getvar(file, name, Some(timeidx), &ComputeOpts::default())
            .map_err(|err| format!("read WRF {name} ({stage}): {err}"))
    };

    progress("reading WRF pressure (sounding field 1/5)".to_string());
    let pressure = read("pressure", "sounding field 1/5")?; // hPa, [nz, ny, nx]
    let expected_3d = check_native_3d_output(&pressure, "pressure", nz, ny, nx)?;

    progress("reading WRF temperature (sounding field 2/5)".to_string());
    let temp = read("temp", "sounding field 2/5")?; // K
    check_native_3d_output(&temp, "temp", nz, ny, nx)?;
    progress("reading WRF dewpoint (sounding field 3/5)".to_string());
    let td = read("td", "sounding field 3/5")?; // degC
    check_native_3d_output(&td, "td", nz, ny, nx)?;
    progress("reading WRF height (sounding field 4/5)".to_string());
    let height = read("height", "sounding field 4/5")?; // m MSL
    check_native_3d_output(&height, "height", nz, ny, nx)?;

    // Earth-relative winds. `uvmet` returns [u_earth.., v_earth..]
    // (2 * nz * cells). There is intentionally NO ua/va fallback: those are
    // grid-relative components, while the store's canonical u_iso/v_iso fields
    // drive geographic sounding wind barbs. Publishing ua/va under those names
    // silently rotates every profile away from true north on projected grids.
    // Split without copying: on a 50 M-cell grid the two halves are ~400 MB
    // each, and `to_vec`-ing them while the 800 MB source was still alive
    // measurably spiked the peak working set of the whole import.
    progress("reading WRF winds (sounding field 5/5)".to_string());
    let uvmet = read("uvmet", "sounding field 5/5")?;
    let wind_data = validate_earth_relative_uvmet(uvmet, nz, ny, nx, expected_3d)?;

    // The hour's LAST `getvar` is behind us, and every input the interpolator
    // needs is owned above — release wrf-core's memoized 3-D f64 intermediates
    // NOW, before the interpolation loop and the store write. `getvar`
    // memoizes every intermediate (full pressure, theta, temperature,
    // geopotential, heights, QVAPOR, destaggered winds, …) inside `WrfFile`
    // and only evicts on a timestep CHANGE; on the 800×800×79 Enderlin grid
    // that cache is ~5 GB of dead weight from here on. Clearing any EARLIER
    // was measured to more than double the peak (every read recomputes its
    // whole dependency chain — see docs/wrf-import-large-grids.md); clearing
    // here costs zero recompute. catch_unwind: a poisoned cache mutex (from a
    // caught diagnostic panic upstream) must not fail the volumes.
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| file.clear_cache()));

    // Dewpoint arrives in degC from wrf-core's `td`; the shared interpolator
    // works in Kelvin like every other field. Convert in place — a separate
    // Kelvin copy is another ~400 MB on large grids.
    let mut dewpoint_k = td.data;
    for value in &mut dewpoint_k {
        *value += 273.15;
    }
    // Borrow the two contiguous components directly. Vec::split_off would
    // allocate a seventh native-sized f64 buffer while retaining the original
    // allocation's two-component capacity. Borrowing keeps raw wrfout at its
    // actual six components and preserves the conservative cap's headroom.
    let (u_wind, v_wind) = wind_data.split_at(expected_3d);
    try_interpolate_iso_volumes(
        &pressure.data,
        &temp.data,
        &dewpoint_k,
        &height.data,
        u_wind,
        v_wind,
        nz,
        cells,
        progress,
    )
}

/// Interpolate pre-read WRF column fields onto the canonical isobaric levels
/// and derive the lowest-level surface fallback. All inputs are row-major
/// `[nz, ny, nx]` (index `k * cells + c`) in skew-T units: pressure hPa,
/// temperature K, dewpoint K, height m, winds m/s. Shared by the raw-wrfout
/// ([`build_iso_volumes`]) and post-processed (`TK`/`Z`/`P`) reader paths.
///
/// File readers must call [`preflight_iso_volume_shape`] with trustworthy
/// metadata before reading their 3-D inputs. This function repeats that guard,
/// validates all input lengths, and uses fallible reservations for the large
/// output, surface, and scratch buffers.
pub(crate) fn try_interpolate_iso_volumes(
    pressure_hpa: &[f64],
    temp_k: &[f64],
    dewpoint_k: &[f64],
    height_m: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    nz: usize,
    cells: usize,
    progress: &mut dyn FnMut(String),
) -> Result<(Vec<IsoVolume>, SurfaceFallback), String> {
    preflight_iso_volume_shape(nz, cells)?;
    validate_interpolation_inputs(
        pressure_hpa,
        temp_k,
        dewpoint_k,
        height_m,
        u_ms,
        v_ms,
        nz,
        cells,
    )?;

    let levels = standard_levels();
    debug_assert_eq!(levels.len(), STANDARD_LEVEL_COUNT);
    let planes = IsoPlanes::try_new(levels.len(), cells)?;
    let surface = try_surface_fallback(pressure_hpa, temp_k, dewpoint_k, u_ms, v_ms, cells)?;
    let mut column_pressure = Vec::new();
    column_pressure
        .try_reserve_exact(nz)
        .map_err(|err| format!("reserve {nz}-level WRF pressure column: {err}"))?;
    column_pressure.resize(nz, 0.0);
    Ok(interpolate_iso_volumes_with_allocations(
        pressure_hpa,
        temp_k,
        dewpoint_k,
        height_m,
        u_ms,
        v_ms,
        nz,
        cells,
        &levels,
        planes,
        surface,
        column_pressure,
        progress,
    ))
}

struct IsoPlanes {
    temperature: Vec<Vec<f32>>,
    dewpoint: Vec<Vec<f32>>,
    u_wind: Vec<Vec<f32>>,
    v_wind: Vec<Vec<f32>>,
    height: Vec<Vec<f32>>,
}

impl IsoPlanes {
    fn try_new(levels: usize, cells: usize) -> Result<Self, String> {
        Ok(Self {
            temperature: try_init_planes("temperature_iso", levels, cells)?,
            dewpoint: try_init_planes("dewpoint_iso", levels, cells)?,
            u_wind: try_init_planes("u_iso", levels, cells)?,
            v_wind: try_init_planes("v_iso", levels, cells)?,
            height: try_init_planes("height_iso", levels, cells)?,
        })
    }
}

#[derive(Clone, Copy)]
struct IsoInterpolationInputs<'a> {
    pressure_hpa: &'a [f64],
    temp_k: &'a [f64],
    dewpoint_k: &'a [f64],
    height_m: &'a [f64],
    u_ms: &'a [f64],
    v_ms: &'a [f64],
    nz: usize,
    cells: usize,
    levels: &'a [u16],
}

/// Matching cell slices from every level plane. Splitting this value splits
/// every output at the same cell boundary, so parallel workers never alias.
struct IsoPlaneSlices<'a> {
    temperature: Vec<&'a mut [f32]>,
    dewpoint: Vec<&'a mut [f32]>,
    u_wind: Vec<&'a mut [f32]>,
    v_wind: Vec<&'a mut [f32]>,
    height: Vec<&'a mut [f32]>,
}

impl<'a> IsoPlaneSlices<'a> {
    fn for_range(planes: &'a mut IsoPlanes, start: usize, end: usize) -> Self {
        fn field_slices<'a>(
            planes: &'a mut [Vec<f32>],
            start: usize,
            end: usize,
        ) -> Vec<&'a mut [f32]> {
            planes
                .iter_mut()
                .map(|plane| &mut plane[start..end])
                .collect()
        }

        Self {
            temperature: field_slices(&mut planes.temperature, start, end),
            dewpoint: field_slices(&mut planes.dewpoint, start, end),
            u_wind: field_slices(&mut planes.u_wind, start, end),
            v_wind: field_slices(&mut planes.v_wind, start, end),
            height: field_slices(&mut planes.height, start, end),
        }
    }

    fn len(&self) -> usize {
        self.temperature.first().map_or(0, |plane| plane.len())
    }

    fn split_at(self, mid: usize) -> (Self, Self) {
        fn split_field<'a>(
            planes: Vec<&'a mut [f32]>,
            mid: usize,
        ) -> (Vec<&'a mut [f32]>, Vec<&'a mut [f32]>) {
            let mut left = Vec::with_capacity(planes.len());
            let mut right = Vec::with_capacity(planes.len());
            for plane in planes {
                let (left_plane, right_plane) = plane.split_at_mut(mid);
                left.push(left_plane);
                right.push(right_plane);
            }
            (left, right)
        }

        let (temperature_left, temperature_right) = split_field(self.temperature, mid);
        let (dewpoint_left, dewpoint_right) = split_field(self.dewpoint, mid);
        let (u_wind_left, u_wind_right) = split_field(self.u_wind, mid);
        let (v_wind_left, v_wind_right) = split_field(self.v_wind, mid);
        let (height_left, height_right) = split_field(self.height, mid);
        (
            Self {
                temperature: temperature_left,
                dewpoint: dewpoint_left,
                u_wind: u_wind_left,
                v_wind: v_wind_left,
                height: height_left,
            },
            Self {
                temperature: temperature_right,
                dewpoint: dewpoint_right,
                u_wind: u_wind_right,
                v_wind: v_wind_right,
                height: height_right,
            },
        )
    }
}

fn wrf_iso_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().saturating_sub(1).clamp(1, MAX_WRF_ISO_THREADS))
        .unwrap_or(1)
}

/// A bounded pool prevents a large WRF import from occupying every process
/// worker. Calls already running on a Rayon worker use the serial path below,
/// avoiding nested pools and oversubscription.
fn wrf_iso_pool() -> Option<&'static ThreadPool> {
    static POOL: OnceLock<Option<ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        let workers = wrf_iso_worker_count();
        if workers < 2 {
            return None;
        }
        ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("wrf-iso-{index}"))
            .build()
            .ok()
    })
    .as_ref()
}

fn try_pressure_columns(count: usize, nz: usize) -> Option<Vec<Vec<f64>>> {
    let mut columns = Vec::new();
    columns.try_reserve_exact(count).ok()?;
    for _ in 0..count {
        let mut column = Vec::new();
        column.try_reserve_exact(nz).ok()?;
        column.resize(nz, 0.0);
        columns.push(column);
    }
    Some(columns)
}

fn interpolate_iso_cells(
    inputs: &IsoInterpolationInputs<'_>,
    mut planes: IsoPlaneSlices<'_>,
    cell_start: usize,
    col_p: &mut [f64],
) {
    for local_cell in 0..planes.len() {
        let cell = cell_start + local_cell;
        for k in 0..inputs.nz {
            col_p[k] = inputs.pressure_hpa[k * inputs.cells + cell];
        }
        for (level_index, &level) in inputs.levels.iter().enumerate() {
            let Some((k, t)) = bracket(col_p, f64::from(level)) else {
                continue;
            };
            let (i0, i1) = (k * inputs.cells + cell, (k + 1) * inputs.cells + cell);
            if let Some(value) = lerp(inputs.temp_k[i0], inputs.temp_k[i1], t) {
                planes.temperature[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.dewpoint_k[i0], inputs.dewpoint_k[i1], t) {
                planes.dewpoint[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.u_ms[i0], inputs.u_ms[i1], t) {
                planes.u_wind[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.v_ms[i0], inputs.v_ms[i1], t) {
                planes.v_wind[level_index][local_cell] = value as f32;
            }
            if let Some(value) = lerp(inputs.height_m[i0], inputs.height_m[i1], t) {
                planes.height[level_index][local_cell] = value as f32;
            }
        }
    }
}

fn interpolate_iso_cells_parallel(
    inputs: &IsoInterpolationInputs<'_>,
    planes: IsoPlaneSlices<'_>,
    cell_start: usize,
    pressure_columns: &mut [Vec<f64>],
) {
    if pressure_columns.len() <= 1 || planes.len() <= 1 {
        interpolate_iso_cells(inputs, planes, cell_start, &mut pressure_columns[0]);
        return;
    }

    let left_workers = pressure_columns.len() / 2;
    let split_cell = planes.len() * left_workers / pressure_columns.len();
    let (left_planes, right_planes) = planes.split_at(split_cell);
    let (left_columns, right_columns) = pressure_columns.split_at_mut(left_workers);
    rayon::join(
        || interpolate_iso_cells_parallel(inputs, left_planes, cell_start, left_columns),
        || {
            interpolate_iso_cells_parallel(
                inputs,
                right_planes,
                cell_start + split_cell,
                right_columns,
            )
        },
    );
}

fn report_iso_progress(
    progress: &mut dyn FnMut(String),
    level_count: usize,
    cell: usize,
    cells: usize,
) {
    progress(format!(
        "interpolating 5 sounding fields to {level_count} isobaric levels — {}%",
        cell * 100 / cells
    ));
}

fn interpolate_iso_volumes_with_allocations(
    pressure_hpa: &[f64],
    temp_k: &[f64],
    dewpoint_k: &[f64],
    height_m: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    nz: usize,
    cells: usize,
    levels: &[u16],
    mut planes: IsoPlanes,
    surface: SurfaceFallback,
    mut col_p: Vec<f64>,
    progress: &mut dyn FnMut(String),
) -> (Vec<IsoVolume>, SurfaceFallback) {
    let inputs = IsoInterpolationInputs {
        pressure_hpa,
        temp_k,
        dewpoint_k,
        height_m,
        u_ms,
        v_ms,
        nz,
        cells,
        levels,
    };
    let progress_step = (cells / 10).max(1);
    let pool = match (
        cells >= MIN_PARALLEL_ISO_CELLS,
        rayon::current_thread_index(),
    ) {
        (true, None) => wrf_iso_pool(),
        _ => None,
    };
    let mut pressure_columns =
        pool.and_then(|pool| try_pressure_columns(pool.current_num_threads(), nz));
    if let (Some(pool), Some(columns)) = (pool, pressure_columns.as_mut()) {
        for start in (0..cells).step_by(progress_step) {
            report_iso_progress(progress, levels.len(), start, cells);
            let end = start.saturating_add(progress_step).min(cells);
            let plane_slices = IsoPlaneSlices::for_range(&mut planes, start, end);
            pool.install(|| {
                interpolate_iso_cells_parallel(&inputs, plane_slices, start, columns.as_mut_slice())
            });
        }
    } else {
        for c in 0..cells {
            if c % progress_step == 0 {
                progress(format!(
                    "interpolating 5 sounding fields to {} isobaric levels — {}%",
                    levels.len(),
                    c * 100 / cells
                ));
            }
            for k in 0..nz {
                col_p[k] = pressure_hpa[k * cells + c];
            }
            for (li, &lev) in levels.iter().enumerate() {
                let Some((k, t)) = bracket(&col_p, f64::from(lev)) else {
                    continue;
                };
                let (i0, i1) = (k * cells + c, (k + 1) * cells + c);
                if let Some(value) = lerp(temp_k[i0], temp_k[i1], t) {
                    planes.temperature[li][c] = value as f32;
                }
                if let Some(value) = lerp(dewpoint_k[i0], dewpoint_k[i1], t) {
                    planes.dewpoint[li][c] = value as f32;
                }
                if let Some(value) = lerp(u_ms[i0], u_ms[i1], t) {
                    planes.u_wind[li][c] = value as f32;
                }
                if let Some(value) = lerp(v_ms[i0], v_ms[i1], t) {
                    planes.v_wind[li][c] = value as f32;
                }
                if let Some(value) = lerp(height_m[i0], height_m[i1], t) {
                    planes.height[li][c] = value as f32;
                }
            }
        }
    }

    let volumes = vec![
        IsoVolume {
            name: "temperature_iso".to_string(),
            units: "K".to_string(),
            levels: pack(levels, planes.temperature),
        },
        IsoVolume {
            name: "dewpoint_iso".to_string(),
            units: "K".to_string(),
            levels: pack(levels, planes.dewpoint),
        },
        IsoVolume {
            name: "u_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(levels, planes.u_wind),
        },
        IsoVolume {
            name: "v_iso".to_string(),
            units: "m/s".to_string(),
            levels: pack(levels, planes.v_wind),
        },
        IsoVolume {
            name: "height_iso".to_string(),
            units: "gpm".to_string(),
            levels: pack(levels, planes.height),
        },
    ];
    (volumes, surface)
}

fn check_native_3d_output(
    out: &VarOutput,
    name: &str,
    nz: usize,
    ny: usize,
    nx: usize,
) -> Result<usize, String> {
    let expected_shape = [nz, ny, nx];
    if out.shape.as_slice() != expected_shape.as_slice() {
        return Err(format!(
            "WRF {name} has shape {:?}, expected exact native shape {expected_shape:?}",
            out.shape
        ));
    }
    let expected = checked_dimension_product("WRF native 3-D field", &expected_shape)?;
    if out.data.len() != expected {
        return Err(format!(
            "WRF {name} shape {expected_shape:?} describes {expected} values, but the output contains {}",
            out.data.len()
        ));
    }
    Ok(expected)
}

fn validate_earth_relative_uvmet(
    uvmet: VarOutput,
    nz: usize,
    ny: usize,
    nx: usize,
    expected_component_values: usize,
) -> Result<Vec<f64>, String> {
    let expected_shape = [2, nz, ny, nx];
    if uvmet.shape.as_slice() != expected_shape.as_slice() {
        return Err(format!(
            "WRF uvmet has shape {:?}, expected exact two-component native shape {expected_shape:?}",
            uvmet.shape,
        ));
    }
    let expected_total = expected_component_values.checked_mul(2).ok_or_else(|| {
        "WRF uvmet two-component length overflows the platform address space".to_string()
    })?;
    if uvmet.data.len() != expected_total {
        return Err(format!(
            "WRF uvmet has {} values, expected {expected_total} for two earth-relative components",
            uvmet.data.len()
        ));
    }
    Ok(uvmet.data)
}

/// Multiply dimensions supplied by an untrusted file without relying on
/// release-mode wrapping (or a debug-mode panic). The caller can then report a
/// malformed shape as an ordinary import error.
fn checked_dimension_product(name: &str, dimensions: &[usize]) -> Result<usize, String> {
    dimensions.iter().try_fold(1usize, |product, &dimension| {
        product.checked_mul(dimension).ok_or_else(|| {
            format!("{name} dimensions {dimensions:?} overflow the platform address space")
        })
    })
}

fn validate_interpolation_inputs(
    pressure_hpa: &[f64],
    temp_k: &[f64],
    dewpoint_k: &[f64],
    height_m: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    nz: usize,
    cells: usize,
) -> Result<(), String> {
    let expected = checked_dimension_product("WRF native 3-D field", &[nz, cells])?;
    if nz < 2 {
        return Err(format!(
            "WRF native 3-D fields require at least two levels, got {nz}"
        ));
    }
    for (name, values) in [
        ("pressure", pressure_hpa),
        ("temperature", temp_k),
        ("dewpoint", dewpoint_k),
        ("height", height_m),
        ("u wind", u_ms),
        ("v wind", v_ms),
    ] {
        if values.len() != expected {
            return Err(format!(
                "WRF {name} has {} values, expected {expected} for {nz} levels x {cells} cells",
                values.len()
            ));
        }
    }
    Ok(())
}

fn try_surface_fallback(
    pressure_hpa: &[f64],
    temp_k: &[f64],
    dewpoint_k: &[f64],
    u_ms: &[f64],
    v_ms: &[f64],
    cells: usize,
) -> Result<SurfaceFallback, String> {
    Ok(SurfaceFallback {
        surface_pressure_pa: try_surface_plane(
            "approx_surface_pressure",
            pressure_hpa,
            cells,
            |value| (value * 100.0) as f32,
        )?,
        temperature_2m_k: try_surface_plane("approx_temperature_2m", temp_k, cells, |value| {
            value as f32
        })?,
        dewpoint_2m_k: try_surface_plane("approx_dewpoint_2m", dewpoint_k, cells, |value| {
            value as f32
        })?,
        u_10m: try_surface_plane("approx_u_10m", u_ms, cells, |value| value as f32)?,
        v_10m: try_surface_plane("approx_v_10m", v_ms, cells, |value| value as f32)?,
    })
}

fn try_surface_plane(
    name: &str,
    source: &[f64],
    cells: usize,
    convert: impl Fn(f64) -> f32,
) -> Result<Vec<f32>, String> {
    let mut plane = Vec::new();
    plane
        .try_reserve_exact(cells)
        .map_err(|err| format!("reserve {cells}-cell WRF surface plane '{name}': {err}"))?;
    plane.extend(source.iter().take(cells).map(|value| convert(*value)));
    Ok(plane)
}

fn try_init_planes(name: &str, levels: usize, cells: usize) -> Result<Vec<Vec<f32>>, String> {
    let mut planes = Vec::new();
    planes
        .try_reserve_exact(levels)
        .map_err(|err| format!("reserve {levels} WRF pressure planes for '{name}': {err}"))?;
    for level_index in 0..levels {
        let mut plane = Vec::new();
        plane.try_reserve_exact(cells).map_err(|err| {
            format!(
                "reserve {cells}-cell WRF pressure plane {level_index}/{levels} for '{name}': {err}"
            )
        })?;
        plane.resize(cells, f32::NAN);
        planes.push(plane);
    }
    Ok(planes)
}

fn pack(levels: &[u16], planes: Vec<Vec<f32>>) -> Vec<(u16, Vec<f32>)> {
    levels.iter().copied().zip(planes).collect()
}

/// Locate the native levels bracketing `target` hPa in a WRF column (pressure
/// decreasing with index, level 0 nearest the surface) and return the lower
/// level index plus the log-pressure interpolation weight. `None` when the
/// target sits below the lowest level or above the model top.
fn bracket(col_p: &[f64], target: f64) -> Option<(usize, f64)> {
    for k in 0..col_p.len().saturating_sub(1) {
        let (pk, pk1) = (col_p[k], col_p[k + 1]);
        if !pk.is_finite() || !pk1.is_finite() || pk == pk1 {
            continue;
        }
        let (hi, lo) = if pk >= pk1 { (pk, pk1) } else { (pk1, pk) };
        if target <= hi && target >= lo {
            let t = (target.ln() - pk.ln()) / (pk1.ln() - pk.ln());
            return Some((k, t));
        }
    }
    None
}

fn lerp(a: f64, b: f64, t: f64) -> Option<f64> {
    (a.is_finite() && b.is_finite()).then_some(a + t * (b - a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_levels_span_the_isobaric_ladder() {
        let levels = standard_levels();
        assert_eq!(levels.len(), 37);
        assert_eq!(*levels.first().unwrap(), 100);
        assert_eq!(*levels.last().unwrap(), 1000);
    }

    #[test]
    fn volume_preflight_checks_store_and_owned_working_set_ceilings_without_allocating() {
        let largest_supported_grid = rustwx_core::MAX_VOLUME_ELEMENTS / STANDARD_LEVEL_COUNT;
        assert!(
            u128::from(preflight_iso_volume_shape(2, largest_supported_grid).unwrap())
                < MAX_WRF_VOLUME_OWNED_BYTES
        );

        let error = preflight_iso_volume_shape(2, largest_supported_grid + 1)
            .expect_err("one cell past the 37-level ceiling must be omitted");
        assert!(error.contains("37-level"), "unexpected error: {error}");
        assert!(
            error.contains(&rustwx_core::MAX_VOLUME_ELEMENTS.to_string()),
            "shared ceiling must be visible in the error: {error}"
        );

        assert_eq!(
            preflight_iso_volume_shape(79, 800 * 800).unwrap(),
            3_722_240_000,
            "known 800x800x79 workflow remains below the 4 GiB owned-buffer cap"
        );
        assert_eq!(
            preflight_iso_volume_shape(92, 800 * 800).unwrap(),
            4_254_720_000,
            "largest accepted level count for this grid remains just under 4 GiB"
        );
        let aggregate_error = preflight_iso_volume_shape(93, 800 * 800)
            .expect_err("one level beyond the aggregate boundary must fail before getvar");
        assert!(
            aggregate_error.contains("4 GiB"),
            "unexpected aggregate error: {aggregate_error}"
        );
        assert!(preflight_iso_volume_shape(usize::MAX, 1).is_err());
        assert!(preflight_iso_volume_shape(1, 1).is_err());
        assert!(preflight_iso_volume_shape(2, 0).is_err());
        assert!(checked_byte_product("test", &[u128::MAX, 2]).is_err());
    }

    #[test]
    fn bracket_interpolates_in_log_pressure_and_clamps_to_range() {
        // Decreasing pressure with index (level 0 nearest the surface).
        let col = [1000.0, 850.0, 700.0, 500.0];
        // Midway between 1000 and 850 in ln-p.
        let (k, t) = bracket(&col, 925.0).expect("in range");
        assert_eq!(k, 0);
        let expected = (925f64.ln() - 1000f64.ln()) / (850f64.ln() - 1000f64.ln());
        assert!((t - expected).abs() < 1e-9);
        // Below the lowest level and above the top are both out of range.
        assert!(bracket(&col, 1013.0).is_none());
        assert!(bracket(&col, 300.0).is_none());
    }

    #[test]
    fn lerp_skips_non_finite_endpoints() {
        assert_eq!(lerp(0.0, 10.0, 0.5), Some(5.0));
        assert_eq!(lerp(f64::NAN, 10.0, 0.5), None);
        assert_eq!(lerp(0.0, f64::NAN, 0.5), None);
    }

    #[test]
    fn malformed_dimension_products_return_errors_instead_of_overflowing() {
        let error = checked_dimension_product("test field", &[2, usize::MAX, 2])
            .expect_err("oversized file dimensions must fail closed");
        assert!(error.contains("overflow"));
        assert_eq!(checked_dimension_product("test field", &[2, 3, 4]), Ok(24));
    }

    #[test]
    fn checked_interpolator_rejects_bad_native_shape_before_output_allocation() {
        let one_value = [1.0];
        let error = try_interpolate_iso_volumes(
            &one_value,
            &one_value,
            &one_value,
            &one_value,
            &one_value,
            &one_value,
            2,
            1,
            &mut |_| {},
        )
        .err()
        .expect("two levels x one cell requires two values per input");
        assert!(error.contains("expected 2"), "unexpected error: {error}");
    }

    #[test]
    fn native_outputs_require_exact_declared_wrf_shapes() {
        let valid = VarOutput {
            data: vec![1.0, 2.0],
            shape: vec![2, 1, 1],
            units: "K".to_string(),
            description: "valid native field".to_string(),
        };
        assert_eq!(check_native_3d_output(&valid, "temp", 2, 1, 1), Ok(2));

        let transposed = VarOutput {
            data: vec![1.0, 2.0],
            shape: vec![1, 1, 2],
            units: "K".to_string(),
            description: "wrongly shaped field".to_string(),
        };
        let error = check_native_3d_output(&transposed, "temp", 2, 1, 1)
            .expect_err("same-length transposed shape must fail closed");
        assert!(
            error.contains("exact native shape"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn uvmet_validation_rejects_grid_relative_or_malformed_fallback_shapes() {
        let uvmet = VarOutput {
            data: vec![1.0, 2.0, 3.0, 4.0],
            shape: vec![2, 2, 1, 1],
            units: "m/s".to_string(),
            description: "earth-relative wind".to_string(),
        };
        let winds =
            validate_earth_relative_uvmet(uvmet, 2, 1, 1, 2).expect("valid uvmet components");
        let (u, v) = winds.split_at(2);
        assert_eq!(u, &[1.0, 2.0]);
        assert_eq!(v, &[3.0, 4.0]);

        let one_component = VarOutput {
            data: vec![1.0, 2.0],
            shape: vec![1, 2, 1, 1],
            units: "m/s".to_string(),
            description: "grid-relative wind".to_string(),
        };
        assert!(
            validate_earth_relative_uvmet(one_component, 2, 1, 1, 2)
                .expect_err("one grid-relative component must not be accepted")
                .contains("two-component native shape")
        );
    }

    #[test]
    fn multicore_interpolation_is_bit_exact_with_serial_cell_order() {
        const CELLS: usize = 4_123;
        const NZ: usize = 9;
        const WORKERS: usize = 4;

        let levels = standard_levels();
        let native_len = NZ * CELLS;
        let mut pressure = Vec::with_capacity(native_len);
        let mut temp = Vec::with_capacity(native_len);
        let mut dewp = Vec::with_capacity(native_len);
        let mut height = Vec::with_capacity(native_len);
        let mut u = Vec::with_capacity(native_len);
        let mut v = Vec::with_capacity(native_len);
        for k in 0..NZ {
            for cell in 0..CELLS {
                let cell_term = cell as f64 * 0.003;
                let level_term = k as f64;
                pressure.push(1_012.75 - f64::from((cell % 17) as u8) * 0.25 - level_term * 112.5);
                temp.push(302.0 - level_term * 5.125 + cell_term);
                dewp.push(296.0 - level_term * 5.375 + cell_term * 0.75);
                height.push(125.0 + level_term * 950.75 + cell_term * 2.0);
                u.push(-12.0 + level_term * 1.75 - cell_term);
                v.push(8.0 - level_term * 0.875 + cell_term * 0.5);
            }
        }

        // Exercise skipped pressure pairs, equal-pressure pairs, and
        // non-finite field endpoints in addition to ordinary columns.
        for cell in (0..CELLS).step_by(257) {
            pressure[3 * CELLS + cell] = f64::NAN;
            temp[5 * CELLS + cell] = f64::NAN;
            dewp[6 * CELLS + cell] = f64::INFINITY;
        }
        for cell in (11..CELLS).step_by(389) {
            pressure[5 * CELLS + cell] = pressure[4 * CELLS + cell];
        }

        let inputs = IsoInterpolationInputs {
            pressure_hpa: &pressure,
            temp_k: &temp,
            dewpoint_k: &dewp,
            height_m: &height,
            u_ms: &u,
            v_ms: &v,
            nz: NZ,
            cells: CELLS,
            levels: &levels,
        };
        let mut serial = IsoPlanes::try_new(levels.len(), CELLS).expect("serial planes");
        let mut parallel = IsoPlanes::try_new(levels.len(), CELLS).expect("parallel planes");
        let mut serial_pressure = vec![0.0; NZ];
        interpolate_iso_cells(
            &inputs,
            IsoPlaneSlices::for_range(&mut serial, 0, CELLS),
            0,
            &mut serial_pressure,
        );

        let pool = ThreadPoolBuilder::new()
            .num_threads(WORKERS)
            .build()
            .expect("test pool");
        let mut parallel_pressure =
            try_pressure_columns(WORKERS, NZ).expect("parallel pressure scratch");
        pool.install(|| {
            interpolate_iso_cells_parallel(
                &inputs,
                IsoPlaneSlices::for_range(&mut parallel, 0, CELLS),
                0,
                &mut parallel_pressure,
            )
        });

        fn assert_field_bits(field: &str, serial: &[Vec<f32>], parallel: &[Vec<f32>]) {
            assert_eq!(serial.len(), parallel.len());
            for (level, (serial_plane, parallel_plane)) in serial.iter().zip(parallel).enumerate() {
                assert_eq!(serial_plane.len(), parallel_plane.len());
                for (cell, (serial_value, parallel_value)) in
                    serial_plane.iter().zip(parallel_plane).enumerate()
                {
                    assert_eq!(
                        serial_value.to_bits(),
                        parallel_value.to_bits(),
                        "{field} differs at level {level}, cell {cell}"
                    );
                }
            }
        }

        assert_field_bits("temperature", &serial.temperature, &parallel.temperature);
        assert_field_bits("dewpoint", &serial.dewpoint, &parallel.dewpoint);
        assert_field_bits("u wind", &serial.u_wind, &parallel.u_wind);
        assert_field_bits("v wind", &serial.v_wind, &parallel.v_wind);
        assert_field_bits("height", &serial.height, &parallel.height);
    }

    /// The shared interpolator must stream progress (both import paths surface
    /// it) and still produce correct planes — guard for the progress plumbing.
    #[test]
    fn interpolate_streams_progress_and_interpolates() {
        // 2 columns × 3 levels, pressure decreasing with index.
        let pressure = vec![1000.0, 1000.0, 850.0, 850.0, 700.0, 700.0];
        let temp = vec![300.0, 301.0, 290.0, 291.0, 280.0, 281.0];
        let dewp = vec![295.0, 296.0, 285.0, 286.0, 275.0, 276.0];
        let height = vec![100.0, 110.0, 1500.0, 1510.0, 3000.0, 3010.0];
        let u = vec![1.0; 6];
        let v = vec![2.0; 6];

        let mut messages = Vec::new();
        let (volumes, surface) = try_interpolate_iso_volumes(
            &pressure,
            &temp,
            &dewp,
            &height,
            &u,
            &v,
            3,
            2,
            &mut |message| messages.push(message),
        )
        .expect("small valid volume");

        assert!(
            messages
                .iter()
                .all(|message| message.contains("isobaric levels")),
            "unexpected progress lines: {messages:?}"
        );
        assert!(!messages.is_empty(), "interpolation must report progress");

        // 850 hPa is an exact native level: temperature lands unchanged.
        let temps = &volumes[0];
        assert_eq!(temps.name, "temperature_iso");
        let (_, plane_850) = temps
            .levels
            .iter()
            .find(|(hpa, _)| *hpa == 850)
            .expect("850 hPa plane");
        assert!((plane_850[0] - 290.0).abs() < 1e-3);
        assert!((plane_850[1] - 291.0).abs() < 1e-3);
        // Surface fallback comes from level 0 in Pa/K.
        assert!((surface.surface_pressure_pa[0] - 100_000.0).abs() < 1e-3);
        assert!((surface.temperature_2m_k[1] - 301.0).abs() < 1e-3);
    }
}

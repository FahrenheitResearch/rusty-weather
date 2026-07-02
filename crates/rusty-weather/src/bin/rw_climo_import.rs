//! `rw_climo_import` — import a `cafire.rtma_climo_pack.v1` directory (from
//! `tools/rw-climo-pack`, node 2) into a native `.rws` climatology store.
//!
//! The pack carries i16-quantized RTMA-grid slices (818x898 Lambert crop).
//! This tool resamples every slice onto the HRRR grid cropped to the pack
//! bounds — so serve-time anomaly math is same-grid arithmetic against HRRR
//! forecasts — and writes `store/rtma_climo/<run_slug>/f{doy:03}.rws` with
//! one derived-marker variable per (window, product, stat), plus a
//! `climo_grid_meta.json` recording the HRRR crop offsets and source grid
//! hash the render lane must validate against.
//!
//! Resampling: the RTMA source is curvilinear (Lambert), so the shared
//! regrid crate's bilinear lane (regular sources only) does not apply. We
//! build a one-time locator by walking the smooth source mesh per target
//! point (scan-line coherent, a few steps per point), then blend the four
//! enclosing quad corners with inverse-distance weights. The map is reused
//! across all slices; NaN contributors renormalize; targets farther than
//! `--max-distance-km` from any source cell become NaN.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use rayon::prelude::*;
use rustwx_core::{GridShape, LatLonGrid};
use rw_store::HourIngestWriter;
use rw_store::grid::GridFile;
use serde::Deserialize;

const NAN_SENTINEL: i16 = i16::MIN;

#[derive(Debug, Parser)]
#[command(
    name = "rw-climo-import",
    about = "Import an RTMA climatology pack into a .rws store on the HRRR subgrid"
)]
struct Args {
    #[arg(long)]
    pack: PathBuf,
    #[arg(long)]
    store_root: PathBuf,
    #[arg(long, help = "Existing HRRR run whose grid.rwg defines the target grid, e.g. 20260629_03z")]
    hrrr_run: String,
    #[arg(long, default_value = "hrrr")]
    hrrr_model: String,
    #[arg(long, default_value = "rtma_climo")]
    climo_model: String,
    #[arg(long, default_value = "seasonal_v2026_05_24")]
    climo_run: String,
    #[arg(long, default_value_t = 4.0)]
    max_distance_km: f64,
    #[arg(long, default_value_t = 16, help = "Random imported slices re-checked against the pack")]
    verify_samples: usize,
    #[arg(long)]
    threads: Option<usize>,
}

// ------------------------------------------------------------ pack reader

#[derive(Debug, Deserialize)]
struct PackManifest {
    schema: String,
    crop: PackCrop,
    nan_sentinel: i16,
    /// `[west, east, south, north]` the pack was asked to cover; the HRRR
    /// target crop uses these, not the rotated source footprint's bbox.
    bounds_requested: [f64; 4],
    tasks: Vec<PackTask>,
    doy_start: u16,
    doy_end: u16,
}

#[derive(Debug, Deserialize)]
struct PackCrop {
    ny: usize,
    nx: usize,
}

#[derive(Debug, Deserialize)]
struct PackTask {
    window: String,
    product: String,
    stat: String,
    file: String,
    slices: Vec<PackSlice>,
}

#[derive(Debug, Deserialize)]
struct PackSlice {
    doy: u16,
    scale: f64,
    offset: f64,
    nan_count: u64,
}

fn read_f32bin(path: &Path, expected: usize) -> Result<Vec<f32>, String> {
    let bytes = fs::read(path).map_err(|err| format!("read {}: {err}", path.display()))?;
    if bytes.len() != expected * 4 {
        return Err(format!(
            "{}: {} bytes, expected {}",
            path.display(),
            bytes.len(),
            expected * 4
        ));
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect())
}

/// Read exactly one slice's bytes from a pack task file (ranged read: the
/// full-year files are ~500 MB and must never be read whole per slice).
fn read_slice_bytes(path: &Path, slice_index: usize, plane: usize) -> Result<Vec<u8>, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let mut file =
        fs::File::open(path).map_err(|err| format!("open {}: {err}", path.display()))?;
    file.seek(SeekFrom::Start((slice_index * plane * 2) as u64))
        .map_err(|err| format!("seek {}: {err}", path.display()))?;
    let mut buffer = vec![0u8; plane * 2];
    file.read_exact(&mut buffer)
        .map_err(|err| format!("read slice {slice_index} of {}: {err}", path.display()))?;
    Ok(buffer)
}

/// Decode one pack slice (raw slice bytes) to f32.
fn decode_slice(
    file_bytes: &[u8],
    slice_index: usize,
    plane: usize,
    meta: &PackSlice,
    is_count: bool,
) -> Result<Vec<f32>, String> {
    let base = slice_index * plane * 2;
    let end = base + plane * 2;
    if file_bytes.len() < end {
        return Err(format!("slice {slice_index} overruns pack file"));
    }
    Ok(file_bytes[base..end]
        .chunks_exact(2)
        .map(|b| {
            let q = i16::from_le_bytes([b[0], b[1]]);
            if q == NAN_SENTINEL && !is_count {
                f32::NAN
            } else if meta.scale == 0.0 {
                if is_count { f32::from(q) } else { meta.offset as f32 }
            } else {
                (meta.offset + f64::from(q) * meta.scale) as f32
            }
        })
        .collect())
}

// ------------------------------------------------- curvilinear resampler

/// Precomputed source->target map in compact CSR form: for each target
/// cell, up to four source indices with normalized inverse-distance
/// weights; an empty row means the target sits beyond the distance cap.
/// Contiguous u32/f32 arrays keep the map small (~8 MB) and cheap to
/// re-checksum, so silent memory corruption during a long import run is
/// detected instead of producing garbage indexes.
struct ResampleMap {
    row_offsets: Vec<u32>,
    source_indices: Vec<u32>,
    weights: Vec<f32>,
}

impl ResampleMap {
    fn checksum(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |value: u64| {
            hash ^= value;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        };
        for &offset in &self.row_offsets {
            mix(u64::from(offset));
        }
        for (&index, &weight) in self.source_indices.iter().zip(&self.weights) {
            mix(u64::from(index));
            mix(u64::from(weight.to_bits()));
        }
        hash
    }

    fn mapped_rows(&self) -> usize {
        self.row_offsets.windows(2).filter(|w| w[1] > w[0]).count()
    }

    fn rows(&self) -> usize {
        self.row_offsets.len() - 1
    }
}

fn haversine_km(lat_a: f64, lon_a: f64, lat_b: f64, lon_b: f64) -> f64 {
    let (la, lb) = (lat_a.to_radians(), lat_b.to_radians());
    let dlat = (lat_b - lat_a).to_radians();
    let dlon = (lon_b - lon_a).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + la.cos() * lb.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * 6371.0 * h.sqrt().asin()
}

/// Walk the smooth source mesh from `start` toward the cell nearest the
/// target point; returns the (row, col) local minimum.
fn walk_to_nearest(
    lat: &[f32],
    lon: &[f32],
    ny: usize,
    nx: usize,
    start: (usize, usize),
    t_lat: f64,
    t_lon: f64,
) -> (usize, usize) {
    let cost = |r: usize, c: usize| -> f64 {
        let i = r * nx + c;
        // Equirectangular squared distance is fine for step decisions.
        let dlat = f64::from(lat[i]) - t_lat;
        let dlon = (f64::from(lon[i]) - t_lon) * t_lat.to_radians().cos();
        dlat * dlat + dlon * dlon
    };
    let (mut r, mut c) = start;
    let mut best = cost(r, c);
    loop {
        let mut improved = false;
        // Adaptive step: long hops first so cross-grid walks stay cheap.
        for step in [64usize, 16, 4, 1] {
            for (dr, dc) in [(0i64, 1i64), (0, -1), (1, 0), (-1, 0), (1, 1), (1, -1), (-1, 1), (-1, -1)] {
                let nr = r as i64 + dr * step as i64;
                let nc = c as i64 + dc * step as i64;
                if nr < 0 || nc < 0 || nr as usize >= ny || nc as usize >= nx {
                    continue;
                }
                let candidate = cost(nr as usize, nc as usize);
                if candidate < best {
                    best = candidate;
                    r = nr as usize;
                    c = nc as usize;
                    improved = true;
                }
            }
            if improved {
                break;
            }
        }
        if !improved {
            return (r, c);
        }
    }
}

fn build_resample_map(
    src_lat: &[f32],
    src_lon: &[f32],
    src_ny: usize,
    src_nx: usize,
    tgt_lat: &[f32],
    tgt_lon: &[f32],
    tgt_ny: usize,
    tgt_nx: usize,
    max_distance_km: f64,
) -> ResampleMap {
    let rows: Vec<Vec<Vec<(usize, f64)>>> = (0..tgt_ny)
        .into_par_iter()
        .map(|tr| {
            let mut row_entries = Vec::with_capacity(tgt_nx);
            let mut cursor = (src_ny / 2, src_nx / 2);
            for tc in 0..tgt_nx {
                let ti = tr * tgt_nx + tc;
                let (t_la, t_lo) = (f64::from(tgt_lat[ti]), f64::from(tgt_lon[ti]));
                let (r, c) = walk_to_nearest(src_lat, src_lon, src_ny, src_nx, cursor, t_la, t_lo);
                cursor = (r, c);
                let nearest_i = r * src_nx + c;
                let nearest_km = haversine_km(
                    f64::from(src_lat[nearest_i]),
                    f64::from(src_lon[nearest_i]),
                    t_la,
                    t_lo,
                );
                if nearest_km > max_distance_km {
                    row_entries.push(Vec::new());
                    continue;
                }
                // Inverse-distance blend over the 2x2 neighborhood around
                // the nearest cell (clamped at edges).
                let r1 = r.min(src_ny - 2);
                let c1 = c.min(src_nx - 2);
                let mut entry = Vec::with_capacity(4);
                let mut total = 0.0f64;
                for (rr, cc) in [(r1, c1), (r1, c1 + 1), (r1 + 1, c1), (r1 + 1, c1 + 1)] {
                    let si = rr * src_nx + cc;
                    let km = haversine_km(
                        f64::from(src_lat[si]),
                        f64::from(src_lon[si]),
                        t_la,
                        t_lo,
                    );
                    let weight = 1.0 / (km * km).max(1.0e-9);
                    entry.push((si, weight));
                    total += weight;
                }
                for item in &mut entry {
                    item.1 /= total;
                }
                row_entries.push(entry);
            }
            row_entries
        })
        .collect();
    let mut row_offsets = Vec::with_capacity(tgt_ny * tgt_nx + 1);
    let mut source_indices = Vec::new();
    let mut weights = Vec::new();
    row_offsets.push(0u32);
    for entry in rows.into_iter().flatten() {
        for (source_index, weight) in entry {
            source_indices.push(source_index as u32);
            weights.push(weight as f32);
        }
        row_offsets.push(source_indices.len() as u32);
    }
    ResampleMap {
        row_offsets,
        source_indices,
        weights,
    }
}

impl ResampleMap {
    /// Apply to one source slice; NaN contributors renormalize, all-NaN or
    /// unmapped targets become NaN. Returns Err on an out-of-range index
    /// (map corruption) rather than panicking or masking it.
    fn apply(&self, source: &[f32], out: &mut [f32]) -> Result<(), String> {
        for target_index in 0..self.rows() {
            let start = self.row_offsets[target_index] as usize;
            let end = self.row_offsets[target_index + 1] as usize;
            let mut sum = 0.0f64;
            let mut weight_sum = 0.0f64;
            for slot in start..end {
                let source_index = self.source_indices[slot] as usize;
                let v = *source.get(source_index).ok_or_else(|| {
                    format!(
                        "resample map corruption: source index {source_index} (plane {}) at target {target_index}",
                        source.len()
                    )
                })?;
                if v.is_finite() {
                    sum += f64::from(v) * f64::from(self.weights[slot]);
                    weight_sum += f64::from(self.weights[slot]);
                }
            }
            out[target_index] = if weight_sum > 0.0 {
                (sum / weight_sum) as f32
            } else {
                f32::NAN
            };
        }
        Ok(())
    }
}

// ------------------------------------------------------------------ main

fn product_units(product: &str, stat: &str) -> &'static str {
    if stat == "sample_count" {
        return "count";
    }
    match product {
        "min_rh_2m_pct" => "%",
        "max_vpd_2m_kpa" => "kPa",
        "max_wind_10m_ms" | "max_gust_10m_ms" => "m/s",
        "max_surface_hdw_wind" | "max_surface_hdw_gust" => "kPa*m/s",
        p if p.starts_with("hours_joint") => "h",
        _ => "",
    }
}

fn main() -> Result<(), String> {
    let args = Args::parse();
    if let Some(threads) = args.threads {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads.max(1))
            .build_global()
            .map_err(|err| err.to_string())?;
    }
    let started = Instant::now();

    // Pack side.
    let manifest: PackManifest = serde_json::from_str(
        &fs::read_to_string(args.pack.join("pack_manifest.json")).map_err(|e| e.to_string())?,
    )
    .map_err(|err| format!("pack manifest: {err}"))?;
    if manifest.schema != "cafire.rtma_climo_pack.v1" {
        return Err(format!("unsupported pack schema {}", manifest.schema));
    }
    if manifest.nan_sentinel != NAN_SENTINEL {
        return Err("pack NaN sentinel mismatch".to_string());
    }
    let src_plane = manifest.crop.ny * manifest.crop.nx;
    let src_lat = read_f32bin(&args.pack.join("latitude_deg.f32bin"), src_plane)?;
    let src_lon = read_f32bin(&args.pack.join("longitude_deg.f32bin"), src_plane)?;

    // Target: HRRR grid cropped to the pack's lon/lat footprint.
    let hrrr_grid_path = args
        .store_root
        .join(&args.hrrr_model)
        .join(&args.hrrr_run)
        .join("grid.rwg");
    let hrrr = GridFile::open(&hrrr_grid_path)
        .map_err(|err| format!("open {}: {err}", hrrr_grid_path.display()))?;
    let [west, east, south, north] = manifest.bounds_requested;
    let (lat_min, lat_max) = (south as f32, north as f32);
    let (lon_min, lon_max) = (west as f32, east as f32);
    let mut row0 = usize::MAX;
    let mut row1 = 0usize;
    let mut col0 = usize::MAX;
    let mut col1 = 0usize;
    for row in 0..hrrr.ny {
        for col in 0..hrrr.nx {
            let i = row * hrrr.nx + col;
            if hrrr.lat[i] >= lat_min
                && hrrr.lat[i] <= lat_max
                && hrrr.lon[i] >= lon_min
                && hrrr.lon[i] <= lon_max
            {
                row0 = row0.min(row);
                row1 = row1.max(row + 1);
                col0 = col0.min(col);
                col1 = col1.max(col + 1);
            }
        }
    }
    if row0 == usize::MAX {
        return Err("HRRR grid does not overlap the pack footprint".to_string());
    }
    let (tgt_ny, tgt_nx) = (row1 - row0, col1 - col0);
    let mut tgt_lat = Vec::with_capacity(tgt_ny * tgt_nx);
    let mut tgt_lon = Vec::with_capacity(tgt_ny * tgt_nx);
    for row in row0..row1 {
        let base = row * hrrr.nx;
        tgt_lat.extend_from_slice(&hrrr.lat[base + col0..base + col1]);
        tgt_lon.extend_from_slice(&hrrr.lon[base + col0..base + col1]);
    }
    println!(
        "target: HRRR {}x{} crop rows {row0}..{row1} cols {col0}..{col1} = {tgt_ny}x{tgt_nx}",
        hrrr.ny, hrrr.nx
    );

    let map_started = Instant::now();
    let map = build_resample_map(
        &src_lat,
        &src_lon,
        manifest.crop.ny,
        manifest.crop.nx,
        &tgt_lat,
        &tgt_lon,
        tgt_ny,
        tgt_nx,
        args.max_distance_km,
    );
    let mapped = map.mapped_rows();
    println!(
        "resample map built in {:.1}s: {mapped}/{} targets mapped ({:.2}%)",
        map_started.elapsed().as_secs_f64(),
        tgt_ny * tgt_nx,
        100.0 * mapped as f64 / (tgt_ny * tgt_nx) as f64
    );
    // Cells inside the requested rectangle but outside the rotated RTMA
    // footprint (offshore corners) are legitimately unmapped and render as
    // no-data; anything below 90% means a real grid mismatch.
    if (mapped as f64) < 0.90 * (tgt_ny * tgt_nx) as f64 {
        return Err("resample map covers under 90% of the target subgrid".to_string());
    }
    let mut unmapped_edge = [0usize; 4]; // W, E, S, N halves
    for index in 0..map.rows() {
        if map.row_offsets[index] == map.row_offsets[index + 1] {
            let (row, col) = (index / tgt_nx, index % tgt_nx);
            if col < tgt_nx / 2 { unmapped_edge[0] += 1 } else { unmapped_edge[1] += 1 }
            if row < tgt_ny / 2 { unmapped_edge[2] += 1 } else { unmapped_edge[3] += 1 }
        }
    }
    println!(
        "unmapped cells by half: west {} east {} south {} north {}",
        unmapped_edge[0], unmapped_edge[1], unmapped_edge[2], unmapped_edge[3]
    );

    let subgrid = LatLonGrid::new(
        GridShape::new(tgt_nx, tgt_ny).map_err(|err| err.to_string())?,
        tgt_lat.clone(),
        tgt_lon.clone(),
    )
    .map_err(|err| err.to_string())?;
    let projection = hrrr.projection.clone();

    // Group tasks by DOY: one hour file per DOY carrying every variable.
    struct SliceRef<'a> {
        task: &'a PackTask,
        slice_index: usize,
        meta: &'a PackSlice,
    }
    let mut by_doy: BTreeMap<u16, Vec<SliceRef>> = BTreeMap::new();
    for task in &manifest.tasks {
        for (slice_index, meta) in task.slices.iter().enumerate() {
            by_doy.entry(meta.doy).or_default().push(SliceRef {
                task,
                slice_index,
                meta,
            });
        }
    }

    // Validate the resample map once, then guard it with a checksum that
    // is re-verified before every DOY batch: silent memory corruption on
    // long imports must abort loudly, never write garbage.
    for &index in &map.source_indices {
        if (index as usize) >= src_plane {
            return Err(format!(
                "resample map is corrupt after build: index {index} (plane {src_plane})"
            ));
        }
    }
    let map_checksum = map.checksum();

    let written_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let build_tag = format!("rw-climo-import-{}", env!("CARGO_PKG_VERSION"));
    let mut hours_written = 0usize;
    let mut vars_written = 0usize;
    for (&doy, slices) in &by_doy {
        if map.checksum() != map_checksum {
            return Err(format!(
                "resample map checksum changed before DOY {doy}: memory corruption detected; aborting import"
            ));
        }
        let mut writer = HourIngestWriter::begin(
            &args.store_root,
            &args.climo_model,
            &args.climo_run,
            doy,
            &subgrid,
            projection.as_ref(),
            &build_tag,
        )
        .map_err(|err| format!("begin doy {doy}: {err}"))?;
        // Resample this DOY's slices in parallel, then append serially.
        let resampled: Vec<(String, &'static str, Vec<f32>)> = slices
            .par_iter()
            .map(|slice| -> Result<_, String> {
                let is_count = slice.task.stat == "sample_count";
                let bytes = read_slice_bytes(
                    &args.pack.join(&slice.task.file),
                    slice.slice_index,
                    src_plane,
                )?;
                let source = decode_slice(&bytes, 0, src_plane, slice.meta, is_count)?;
                let mut out = vec![f32::NAN; tgt_ny * tgt_nx];
                map.apply(&source, &mut out)?;
                let name = format!(
                    "climo__{}__{}__{}",
                    slice.task.window, slice.task.product, slice.task.stat
                );
                Ok((name, product_units(&slice.task.product, &slice.task.stat), out))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (name, units, values) in &resampled {
            writer
                .add_derived_2d(name, units, values)
                .map_err(|err| format!("add {name} doy {doy}: {err}"))?;
            vars_written += 1;
        }
        writer
            .finish(written_unix)
            .map_err(|err| format!("finish doy {doy}: {err}"))?;
        hours_written += 1;
        if hours_written % 30 == 0 {
            println!("... {hours_written}/{} doys written", by_doy.len());
        }
    }

    // Crop-offset sidecar the anomaly render lane validates against.
    let meta_path = args
        .store_root
        .join(&args.climo_model)
        .join(&args.climo_run)
        .join("climo_grid_meta.json");
    fs::write(
        &meta_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "schema": "cafire.rtma_climo_grid_meta.v1",
            "hrrr_model": args.hrrr_model,
            "hrrr_grid_hash": hrrr.hash,
            "hrrr_row0": row0,
            "hrrr_row1": row1,
            "hrrr_col0": col0,
            "hrrr_col1": col1,
            "ny": tgt_ny,
            "nx": tgt_nx,
            "doy_start": manifest.doy_start,
            "doy_end": manifest.doy_end,
            "pack_schema": manifest.schema,
            "resample": "walking-nearest quad IDW",
            "max_distance_km": args.max_distance_km,
        }))
        .map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;

    // Verification: random imported variables re-read from the store and
    // compared against a fresh pack decode + resample.
    let mut verified = 0usize;
    if args.verify_samples > 0 {
        let run_dir = args.store_root.join(&args.climo_model).join(&args.climo_run);
        let grid = GridFile::open(&run_dir.join("grid.rwg")).map_err(|err| err.to_string())?;
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let doys: Vec<u16> = by_doy.keys().copied().collect();
        for _ in 0..args.verify_samples {
            let doy = doys[(next() % doys.len() as u64) as usize];
            let slices = &by_doy[&doy];
            let slice = &slices[(next() % slices.len() as u64) as usize];
            let is_count = slice.task.stat == "sample_count";
            let bytes = read_slice_bytes(
                &args.pack.join(&slice.task.file),
                slice.slice_index,
                src_plane,
            )?;
            let source = decode_slice(&bytes, 0, src_plane, slice.meta, is_count)?;
            let mut expected = vec![f32::NAN; tgt_ny * tgt_nx];
            map.apply(&source, &mut expected)?;
            let reader = rw_store::reader::HourReader::open(
                &run_dir.join(format!("f{doy:03}.rws")),
            )
            .map_err(|err| err.to_string())?;
            let name = format!(
                "climo__{}__{}__{}",
                slice.task.window, slice.task.product, slice.task.stat
            );
            let stored = rw_store::ingest::read_grid_2d(&reader, &grid, &name)
                .map_err(|err| format!("verify read {name}: {err}"))?;
            if stored.values.len() != expected.len() {
                return Err(format!("verify {name} doy {doy}: length mismatch"));
            }
            for (index, (a, b)) in stored.values.iter().zip(&expected).enumerate() {
                let same = (a.is_nan() && b.is_nan()) || a == b;
                if !same {
                    return Err(format!(
                        "verify {name} doy {doy} idx {index}: stored {a} != expected {b}"
                    ));
                }
            }
            verified += 1;
        }
    }

    println!(
        "import complete: {hours_written} doys, {vars_written} variables, verified {verified} slices bit-exact | wall {:.1}s | store {}",
        started.elapsed().as_secs_f64(),
        args.store_root.join(&args.climo_model).join(&args.climo_run).display(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slightly rotated regular mesh: curvilinear like a Lambert grid.
    fn tilted_grid(ny: usize, nx: usize) -> (Vec<f32>, Vec<f32>) {
        let mut lat = Vec::with_capacity(ny * nx);
        let mut lon = Vec::with_capacity(ny * nx);
        for r in 0..ny {
            for c in 0..nx {
                let x = c as f32 * 0.03;
                let y = r as f32 * 0.03;
                lat.push(35.0 + y + 0.004 * x);
                lon.push(-120.0 + x - 0.004 * y);
            }
        }
        (lat, lon)
    }

    #[test]
    fn walking_locator_finds_the_nearest_cell() {
        let (lat, lon) = tilted_grid(120, 150);
        for &(tr, tc) in &[(3usize, 4usize), (60, 75), (110, 140), (0, 149), (119, 0)] {
            let i = tr * 150 + tc;
            let (r, c) = walk_to_nearest(
                &lat,
                &lon,
                120,
                150,
                (60, 75),
                f64::from(lat[i]),
                f64::from(lon[i]),
            );
            assert_eq!((r, c), (tr, tc), "walk missed exact grid point");
        }
    }

    #[test]
    fn resampling_reproduces_a_smooth_field() {
        let (src_lat, src_lon) = tilted_grid(120, 150);
        // Target: interior points offset half a cell.
        let mut tgt_lat = Vec::new();
        let mut tgt_lon = Vec::new();
        for r in 10..110 {
            for c in 10..140 {
                let i = r * 150 + c;
                tgt_lat.push(src_lat[i] + 0.014);
                tgt_lon.push(src_lon[i] + 0.012);
            }
        }
        let map = build_resample_map(
            &src_lat, &src_lon, 120, 150, &tgt_lat, &tgt_lon, 100, 130, 6.0,
        );
        let field: Vec<f32> = src_lat
            .iter()
            .zip(&src_lon)
            .map(|(&la, &lo)| 3.0 * la + 2.0 * lo)
            .collect();
        let mut out = vec![f32::NAN; tgt_lat.len()];
        map.apply(&field, &mut out).unwrap();
        for (index, value) in out.iter().enumerate() {
            let expected = 3.0 * tgt_lat[index] + 2.0 * tgt_lon[index];
            assert!(
                (value - expected).abs() < 0.05,
                "idx {index}: {value} vs {expected}"
            );
        }
    }

    #[test]
    fn nan_contributors_renormalize_and_gaps_stay_nan() {
        let (src_lat, src_lon) = tilted_grid(40, 40);
        let tgt_lat = vec![src_lat[21 * 40 + 21] + 0.01, 60.0];
        let tgt_lon = vec![src_lon[21 * 40 + 21] + 0.01, -80.0];
        let map = build_resample_map(&src_lat, &src_lon, 40, 40, &tgt_lat, &tgt_lon, 1, 2, 6.0);
        let mut field: Vec<f32> = vec![5.0; 40 * 40];
        field[21 * 40 + 21] = f32::NAN;
        let mut out = vec![0.0f32; 2];
        map.apply(&field, &mut out).unwrap();
        assert!((out[0] - 5.0).abs() < 1e-4, "renormalized value {}", out[0]);
        assert!(out[1].is_nan(), "far-away target must be NaN");
    }

    #[test]
    fn pack_slices_decode_with_scale_offset_and_sentinel() {
        let meta = PackSlice {
            doy: 1,
            scale: 0.5,
            offset: 10.0,
            nan_count: 1,
        };
        let mut bytes = Vec::new();
        for q in [-2i16, 0, 4, NAN_SENTINEL] {
            bytes.extend_from_slice(&q.to_le_bytes());
        }
        let values = decode_slice(&bytes, 0, 4, &meta, false).unwrap();
        assert_eq!(values[0], 9.0);
        assert_eq!(values[1], 10.0);
        assert_eq!(values[2], 12.0);
        assert!(values[3].is_nan());
    }
}

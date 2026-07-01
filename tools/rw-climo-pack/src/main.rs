//! `rw_climo_pack` — repack the FireWxAtlas RTMA seasonal ±7-day climatology
//! (zarr v2, full CONUS, 525 GB) into a compact cropped i16 pack for the
//! CAFire serving node.
//!
//! Reads `<atlas>/zarr/rtma2p5/climatology/seasonal_15day/<window>/<product>/
//! doy_XXX/<stat>.zarr/data` arrays (shape [1, 1410, 2335], chunks
//! [1, 256, 256], zlib, `<f4>` or `<u2>`), crops every slice to a lon/lat
//! bounding box, quantizes to i16 with per-slice affine scale/offset
//! (NaN -> -32768), and writes one `[n_doys, ny, nx]` binary per
//! (window, product, stat) plus cropped lat/lon f32 grids and a JSON
//! manifest. `--verify-samples` re-reads random slices from the source and
//! proves the round-trip error bound after writing.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::Parser;
use rayon::prelude::*;
use serde::Serialize;

const NAN_SENTINEL: i16 = i16::MIN;
const QUANT_HALF_RANGE: f64 = 32000.0;

/// Product-window set for pack v1: every product in the daily window, plus
/// the overnight-recovery window for min RH.
const V1_PRODUCT_WINDOWS: &[(&str, &str)] = &[
    ("utc_00_23", "min_rh_2m_pct"),
    ("utc_00_23", "max_vpd_2m_kpa"),
    ("utc_00_23", "max_wind_10m_ms"),
    ("utc_00_23", "max_gust_10m_ms"),
    ("utc_00_23", "max_surface_hdw_wind"),
    ("utc_00_23", "max_surface_hdw_gust"),
    ("utc_00_23", "hours_joint_rh15_gust25"),
    ("utc_00_23", "hours_joint_rh20_gust25"),
    ("utc_00_23", "hours_joint_rh20_wind20"),
    ("utc_12_06_next_day", "min_rh_2m_pct"),
];

#[derive(Debug, Parser)]
#[command(
    name = "rw-climo-pack",
    about = "Repack FireWxAtlas RTMA seasonal climatology zarr into a cropped i16 pack"
)]
struct Args {
    #[arg(long, help = "firewx_atlas_canonical root")]
    atlas_root: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = -126.2)]
    west: f64,
    #[arg(long, default_value_t = -103.3)]
    east: f64,
    #[arg(long, default_value_t = 31.4)]
    south: f64,
    #[arg(long, default_value_t = 47.0)]
    north: f64,
    #[arg(
        long,
        default_value = "v1",
        help = "'v1' for the standard set, or comma list of window:product"
    )]
    products: String,
    #[arg(long, default_value = "p05,p10,p25,p50,p75,p90,p95,p99,sample_count")]
    stats: String,
    #[arg(long, default_value_t = 1)]
    doy_start: u16,
    #[arg(long, default_value_t = 365)]
    doy_end: u16,
    #[arg(long, default_value_t = 24, help = "Random slices to verify after writing")]
    verify_samples: usize,
    #[arg(long)]
    threads: Option<usize>,
}

// ---------------------------------------------------------------- zarr v2

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dtype {
    F4,
    U2,
}

struct ZarrArray {
    dir: PathBuf,
    shape: [usize; 3],
    chunks: [usize; 3],
    dtype: Dtype,
    fill: f32,
}

impl ZarrArray {
    fn open(array_dir: &Path) -> Result<Self, String> {
        let data_dir = array_dir.join("data");
        let meta_path = data_dir.join(".zarray");
        let text = fs::read_to_string(&meta_path)
            .map_err(|err| format!("read {}: {err}", meta_path.display()))?;
        let meta: serde_json::Value = serde_json::from_str(&text)
            .map_err(|err| format!("parse {}: {err}", meta_path.display()))?;
        if meta["zarr_format"] != 2 {
            return Err(format!("{}: not zarr v2", meta_path.display()));
        }
        if meta["order"] != "C" {
            return Err(format!("{}: order must be C", meta_path.display()));
        }
        if meta["compressor"]["id"] != "zlib" {
            return Err(format!("{}: compressor must be zlib", meta_path.display()));
        }
        if !meta["filters"].is_null() {
            return Err(format!("{}: filters unsupported", meta_path.display()));
        }
        let sep = meta["dimension_separator"].as_str().unwrap_or(".");
        if sep != "." {
            return Err(format!("{}: dimension_separator must be '.'", meta_path.display()));
        }
        let dims = |key: &str| -> Result<[usize; 3], String> {
            let list = meta[key]
                .as_array()
                .ok_or_else(|| format!("{}: bad {key}", meta_path.display()))?;
            if list.len() != 3 {
                return Err(format!("{}: {key} must be 3-D", meta_path.display()));
            }
            Ok([
                list[0].as_u64().unwrap_or(0) as usize,
                list[1].as_u64().unwrap_or(0) as usize,
                list[2].as_u64().unwrap_or(0) as usize,
            ])
        };
        let shape = dims("shape")?;
        let chunks = dims("chunks")?;
        let dtype = match meta["dtype"].as_str() {
            Some("<f4") => Dtype::F4,
            Some("<u2") => Dtype::U2,
            other => return Err(format!("{}: unsupported dtype {other:?}", meta_path.display())),
        };
        let fill = match &meta["fill_value"] {
            v if v.is_string() => f32::NAN,
            v => v.as_f64().map(|f| f as f32).unwrap_or(f32::NAN),
        };
        if shape[0] != 1 || chunks[0] != 1 {
            return Err(format!("{}: leading dim must be 1", meta_path.display()));
        }
        Ok(Self {
            dir: data_dir,
            shape,
            chunks,
            dtype,
            fill,
        })
    }

    /// Decode the full [ny, nx] plane to f32 (u2 values converted).
    fn read_full(&self) -> Result<Vec<f32>, String> {
        let (ny, nx) = (self.shape[1], self.shape[2]);
        let (cy, cx) = (self.chunks[1], self.chunks[2]);
        let mut out = vec![self.fill; ny * nx];
        let chunk_rows = ny.div_ceil(cy);
        let chunk_cols = nx.div_ceil(cx);
        let value_size = match self.dtype {
            Dtype::F4 => 4,
            Dtype::U2 => 2,
        };
        for cr in 0..chunk_rows {
            for cc in 0..chunk_cols {
                let path = self.dir.join(format!("0.{cr}.{cc}"));
                let compressed = match fs::read(&path) {
                    Ok(bytes) => bytes,
                    // Absent chunk == entirely fill-valued.
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(err) => return Err(format!("read {}: {err}", path.display())),
                };
                let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&compressed)
                    .map_err(|err| format!("inflate {}: {err:?}", path.display()))?;
                let expected = cy * cx * value_size;
                if raw.len() != expected {
                    return Err(format!(
                        "{}: chunk is {} bytes, expected {expected}",
                        path.display(),
                        raw.len()
                    ));
                }
                let row0 = cr * cy;
                let col0 = cc * cx;
                let rows = cy.min(ny - row0);
                let cols = cx.min(nx - col0);
                for r in 0..rows {
                    let out_base = (row0 + r) * nx + col0;
                    let raw_base = r * cx * value_size;
                    match self.dtype {
                        Dtype::F4 => {
                            for c in 0..cols {
                                let i = raw_base + c * 4;
                                out[out_base + c] = f32::from_le_bytes([
                                    raw[i],
                                    raw[i + 1],
                                    raw[i + 2],
                                    raw[i + 3],
                                ]);
                            }
                        }
                        Dtype::U2 => {
                            for c in 0..cols {
                                let i = raw_base + c * 2;
                                out[out_base + c] =
                                    f32::from(u16::from_le_bytes([raw[i], raw[i + 1]]));
                            }
                        }
                    }
                }
            }
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- pack

#[derive(Debug, Clone, Copy, Serialize)]
struct Crop {
    row0: usize,
    row1: usize,
    col0: usize,
    col1: usize,
    ny: usize,
    nx: usize,
}

impl Crop {
    fn slice<T: Copy>(&self, full: &[T], full_nx: usize) -> Vec<T> {
        let mut out = Vec::with_capacity(self.ny * self.nx);
        for row in self.row0..self.row1 {
            let base = row * full_nx;
            out.extend_from_slice(&full[base + self.col0..base + self.col1]);
        }
        out
    }
}

#[derive(Debug, Clone, Serialize)]
struct SliceMeta {
    doy: u16,
    scale: f64,
    offset: f64,
    min: f64,
    max: f64,
    nan_count: u64,
}

#[derive(Debug, Serialize)]
struct TaskManifest {
    window: String,
    product: String,
    stat: String,
    file: String,
    dtype: &'static str,
    slices: Vec<SliceMeta>,
}

fn quantize(values: &[f32], is_count: bool) -> (Vec<i16>, SliceMeta) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut nan_count = 0u64;
    for &v in values {
        if v.is_finite() {
            min = min.min(f64::from(v));
            max = max.max(f64::from(v));
        } else {
            nan_count += 1;
        }
    }
    if nan_count as usize == values.len() {
        let quantized = vec![NAN_SENTINEL; values.len()];
        let meta = SliceMeta {
            doy: 0,
            scale: 0.0,
            offset: 0.0,
            min: f64::NAN,
            max: f64::NAN,
            nan_count,
        };
        return (quantized, meta);
    }
    let (scale, offset) = if is_count {
        // Counts (n <= a few hundred) are stored exactly.
        (1.0, 0.0)
    } else if max > min {
        ((max - min) / (2.0 * QUANT_HALF_RANGE), (max + min) / 2.0)
    } else {
        (0.0, min)
    };
    let quantized = values
        .iter()
        .map(|&v| {
            if !v.is_finite() {
                return NAN_SENTINEL;
            }
            if scale == 0.0 {
                return if is_count { v as i16 } else { 0 };
            }
            let q = ((f64::from(v) - offset) / scale).round();
            q.clamp(-QUANT_HALF_RANGE, QUANT_HALF_RANGE) as i16
        })
        .collect();
    let meta = SliceMeta {
        doy: 0,
        scale,
        offset,
        min,
        max,
        nan_count,
    };
    (quantized, meta)
}

fn dequantize(q: i16, meta: &SliceMeta, is_count: bool) -> f32 {
    if q == NAN_SENTINEL && !is_count {
        return f32::NAN;
    }
    if meta.scale == 0.0 {
        return if is_count { f32::from(q) } else { meta.offset as f32 };
    }
    (meta.offset + f64::from(q) * meta.scale) as f32
}

fn seasonal_root(atlas_root: &Path) -> PathBuf {
    atlas_root
        .join("zarr")
        .join("rtma2p5")
        .join("climatology")
        .join("seasonal_15day")
}

fn stat_array_dir(root: &Path, window: &str, product: &str, doy: u16, stat: &str) -> PathBuf {
    root.join(window)
        .join(product)
        .join(format!("doy_{doy:03}"))
        .join(format!("{stat}.zarr"))
}

fn write_f32(path: &Path, values: &[f32]) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(values.len() * 4);
    for v in values {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    fs::write(path, bytes).map_err(|err| format!("write {}: {err}", path.display()))
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
    let seasonal = seasonal_root(&args.atlas_root);
    let static_dir = args
        .atlas_root
        .join("firewxatlas_rtma_final")
        .join("static")
        .join("rtma_grid_terrain");

    // Grid + crop window from the checked-in lat/lon arrays.
    let lat_arr = ZarrArray::open(&static_dir.join("latitude_deg.zarr"))?;
    let lon_arr = ZarrArray::open(&static_dir.join("longitude_deg.zarr"))?;
    let (ny_full, nx_full) = (lat_arr.shape[1], lat_arr.shape[2]);
    if lon_arr.shape != lat_arr.shape {
        return Err("latitude/longitude grids disagree".to_string());
    }
    let lat = lat_arr.read_full()?;
    let lon = lon_arr.read_full()?;
    let mut row_min = usize::MAX;
    let mut row_max = 0usize;
    let mut col_min = usize::MAX;
    let mut col_max = 0usize;
    for row in 0..ny_full {
        for col in 0..nx_full {
            let i = row * nx_full + col;
            let (la, lo) = (f64::from(lat[i]), f64::from(lon[i]));
            if la >= args.south && la <= args.north && lo >= args.west && lo <= args.east {
                row_min = row_min.min(row);
                row_max = row_max.max(row);
                col_min = col_min.min(col);
                col_max = col_max.max(col);
            }
        }
    }
    if row_min == usize::MAX {
        return Err("crop bounds select no grid cells".to_string());
    }
    let crop = Crop {
        row0: row_min.saturating_sub(2),
        row1: (row_max + 3).min(ny_full),
        col0: col_min.saturating_sub(2),
        col1: (col_max + 3).min(nx_full),
        ny: 0,
        nx: 0,
    };
    let crop = Crop {
        ny: crop.row1 - crop.row0,
        nx: crop.col1 - crop.col0,
        ..crop
    };
    println!(
        "grid {}x{} -> crop rows {}..{} cols {}..{} = {}x{} ({:.1}% of cells)",
        ny_full,
        nx_full,
        crop.row0,
        crop.row1,
        crop.col0,
        crop.col1,
        crop.ny,
        crop.nx,
        100.0 * (crop.ny * crop.nx) as f64 / (ny_full * nx_full) as f64,
    );

    let product_windows: Vec<(String, String)> = if args.products.trim() == "v1" {
        V1_PRODUCT_WINDOWS
            .iter()
            .map(|(w, p)| ((*w).to_string(), (*p).to_string()))
            .collect()
    } else {
        args.products
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|spec| {
                spec.split_once(':')
                    .map(|(w, p)| (w.to_string(), p.to_string()))
                    .ok_or_else(|| format!("--products entry '{spec}' is not window:product"))
            })
            .collect::<Result<_, _>>()?
    };
    let stats: Vec<String> = args
        .stats
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let doys: Vec<u16> = (args.doy_start..=args.doy_end).collect();

    fs::create_dir_all(&args.out).map_err(|err| err.to_string())?;
    write_f32(&args.out.join("latitude_deg.f32bin"), &crop.slice(&lat, nx_full))?;
    write_f32(&args.out.join("longitude_deg.f32bin"), &crop.slice(&lon, nx_full))?;

    // One task per (window, product, stat): stream DOYs, append slices.
    struct TaskSpec {
        window: String,
        product: String,
        stat: String,
    }
    let tasks: Vec<TaskSpec> = product_windows
        .iter()
        .flat_map(|(w, p)| {
            stats.iter().map(move |s| TaskSpec {
                window: w.clone(),
                product: p.clone(),
                stat: s.clone(),
            })
        })
        .collect();
    println!(
        "{} tasks ({} product-windows x {} stats) x {} doys",
        tasks.len(),
        product_windows.len(),
        stats.len(),
        doys.len()
    );

    let progress = Mutex::new(0usize);
    let manifests: Vec<TaskManifest> = tasks
        .par_iter()
        .map(|task| -> Result<TaskManifest, String> {
            let is_count = task.stat == "sample_count";
            let dir = args.out.join(&task.window).join(&task.product);
            fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
            let file_rel = format!("{}/{}/{}.i16bin", task.window, task.product, task.stat);
            let file_path = args.out.join(&file_rel);
            let mut writer = std::io::BufWriter::new(
                fs::File::create(&file_path).map_err(|err| err.to_string())?,
            );
            let mut slices = Vec::with_capacity(doys.len());
            for &doy in &doys {
                let array = ZarrArray::open(&stat_array_dir(
                    &seasonal,
                    &task.window,
                    &task.product,
                    doy,
                    &task.stat,
                ))?;
                if array.shape[1] != ny_full || array.shape[2] != nx_full {
                    return Err(format!(
                        "{}/{} doy {doy} {}: unexpected shape {:?}",
                        task.window, task.product, task.stat, array.shape
                    ));
                }
                let full = array.read_full()?;
                let cropped = crop.slice(&full, nx_full);
                let (quantized, mut meta) = quantize(&cropped, is_count);
                meta.doy = doy;
                let mut bytes = Vec::with_capacity(quantized.len() * 2);
                for q in &quantized {
                    bytes.extend_from_slice(&q.to_le_bytes());
                }
                writer.write_all(&bytes).map_err(|err| err.to_string())?;
                slices.push(meta);
            }
            writer.flush().map_err(|err| err.to_string())?;
            let mut done = progress.lock().unwrap();
            *done += 1;
            println!(
                "[{}/{}] {} {} {} done",
                *done,
                tasks.len(),
                task.window,
                task.product,
                task.stat
            );
            Ok(TaskManifest {
                window: task.window.clone(),
                product: task.product.clone(),
                stat: task.stat.clone(),
                file: file_rel,
                dtype: "i16le",
                slices,
            })
        })
        .collect::<Result<Vec<_>, _>>()?;

    #[derive(Serialize)]
    struct PackManifest<'a> {
        schema: &'static str,
        generated_unix: u64,
        source_root: String,
        source_dataset: &'static str,
        window_days: u8,
        bounds_requested: [f64; 4],
        source_grid_shape: [usize; 2],
        crop: &'a Crop,
        doy_start: u16,
        doy_end: u16,
        nan_sentinel: i16,
        latitude_file: &'static str,
        longitude_file: &'static str,
        tasks: &'a [TaskManifest],
    }
    let manifest = PackManifest {
        schema: "cafire.rtma_climo_pack.v1",
        generated_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        source_root: args.atlas_root.display().to_string(),
        source_dataset: "firewxatlas_rtma_seasonal_15day",
        window_days: 15,
        bounds_requested: [args.west, args.east, args.south, args.north],
        source_grid_shape: [ny_full, nx_full],
        crop: &crop,
        doy_start: args.doy_start,
        doy_end: args.doy_end,
        nan_sentinel: NAN_SENTINEL,
        latitude_file: "latitude_deg.f32bin",
        longitude_file: "longitude_deg.f32bin",
        tasks: &manifests,
    };
    let manifest_path = args.out.join("pack_manifest.json");
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).map_err(|err| err.to_string())?,
    )
    .map_err(|err| err.to_string())?;

    // ---- verification pass: source zarr vs dequantized pack ----
    let mut verified = 0usize;
    let mut worst_rel = 0.0f64;
    if args.verify_samples > 0 {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..args.verify_samples {
            let task = &manifests[(next() % manifests.len() as u64) as usize];
            let slice_index = (next() % task.slices.len() as u64) as usize;
            let meta = &task.slices[slice_index];
            let is_count = task.stat == "sample_count";
            let array = ZarrArray::open(&stat_array_dir(
                &seasonal,
                &task.window,
                &task.product,
                meta.doy,
                &task.stat,
            ))?;
            let source = crop.slice(&array.read_full()?, nx_full);
            let file = fs::read(args.out.join(&task.file)).map_err(|err| err.to_string())?;
            let plane = crop.ny * crop.nx;
            let base = slice_index * plane * 2;
            // Half a quantum, plus the f32 rounding of the reconstructed
            // value (what the serving reader will actually hold).
            let f32_round = (meta.offset.abs() + QUANT_HALF_RANGE * meta.scale)
                * f64::from(f32::EPSILON);
            let tolerance = if is_count {
                0.0
            } else {
                meta.scale * 0.5 + f32_round + 1e-9
            };
            let mut max_err = 0.0f64;
            for (index, source_value) in source.iter().enumerate() {
                let q = i16::from_le_bytes([file[base + index * 2], file[base + index * 2 + 1]]);
                let unpacked = dequantize(q, meta, is_count);
                if source_value.is_finite() != unpacked.is_finite() {
                    return Err(format!(
                        "{} doy {} idx {index}: NaN mask mismatch ({source_value} vs {unpacked})",
                        task.file, meta.doy
                    ));
                }
                if source_value.is_finite() {
                    let err = (f64::from(*source_value) - f64::from(unpacked)).abs();
                    max_err = max_err.max(err);
                    if err > tolerance {
                        return Err(format!(
                            "{} doy {} idx {index}: error {err} exceeds tolerance {tolerance}",
                            task.file, meta.doy
                        ));
                    }
                }
            }
            let range = (meta.max - meta.min).abs().max(1e-9);
            worst_rel = worst_rel.max(max_err / range);
            verified += 1;
        }
    }

    let total_bytes: u64 = manifests
        .iter()
        .map(|task| fs::metadata(args.out.join(&task.file)).map(|m| m.len()).unwrap_or(0))
        .sum();
    println!(
        "pack complete: {} files, {:.2} GB, {} slices | verified {} random slices, worst relative quantization error {:.3e} | wall {:.1}s | manifest {}",
        manifests.len(),
        total_bytes as f64 / 1_073_741_824.0,
        manifests.iter().map(|t| t.slices.len()).sum::<usize>(),
        verified,
        worst_rel,
        started.elapsed().as_secs_f64(),
        manifest_path.display(),
    );
    Ok(())
}

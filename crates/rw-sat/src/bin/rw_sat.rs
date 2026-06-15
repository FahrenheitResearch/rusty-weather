//! rw_sat — GOES ABI live ingest CLI, plus MTG product discovery.
//!
//! - `latest`: fetch the newest available scan for the requested bands from
//!   the live bucket, ingest into the rolling store, export palette PNGs.
//! - `follow`: poll the bucket continuously (jitter + backoff + dedup),
//!   ingesting frames as they land, with rolling-window eviction.
//! - `export`: re-export a stored frame as a PNG.
//! - `himawari-list`: list recent public NOAA/JMA Himawari AHI segments.
//! - `himawari-download`: download public Himawari AHI segments + manifest.
//! - `himawari-stage`: decompress downloaded Himawari segments for decoders.
//! - `himawari-inspect`: parse staged Himawari Standard Data headers.
//! - `himawari-ingest`: decode staged Himawari data into rw-store frames.
//! - `mtg-list`: list recent public EUMETSAT MTG FCI/LI product IDs.
//! - `mtg-download`: fetch a credential-gated EUMETSAT MTG product ZIP.
//! - `mtg-unpack`: stage a downloaded MTG ZIP/SIP for native decoders.
//! - `mtg-fci-ingest`: decode MTG FCI L1c NetCDF body chunks into rw-store.

use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use clap::{Args, Parser, Subcommand};

use rw_sat::abi::read_goes_abi_field;
use rw_sat::composite::{GoesAbiRgbCompositeStyle, values_on_base_grid};
use rw_sat::events::{NEVER_CANCEL, SatEvent, print_event};
use rw_sat::export::{export_frame_png, render_composite_image};
use rw_sat::fci::{FciChannel, FciValueMode, assemble_fci_chunks};
use rw_sat::follow::{FollowConfig, fetch_and_ingest, follow, poll_prefixes};
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::himawari::{
    HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA, HIMAWARI_STAGE_MANIFEST_SCHEMA, HimawariDownloadManifest,
    HimawariLatestRequest, HimawariManifestSegment, HimawariProduct, HimawariSatellite,
    HimawariStageManifest, HimawariValueMode, assemble_hsd_segments, inspect_hsd_file,
    is_complete_segment_set, list_latest_segments, stage_download_manifest,
};
use rw_sat::mtg::{
    EumetsatCredentials, MtgCollection, MtgSearchRequest, download_product, inspect_package,
    request_access_token, search_products, unpack_package,
};
use rw_sat::s3::{
    S3Object, Sector, abi_filename_product_matches_request, bucket_for_satellite, build_agent,
    download_object, list_s3_objects, object_filename, object_url,
};
use rw_sat::store::{downsample_field, write_satellite_grid_frame};
use rw_sat::window::WindowConfig;

#[derive(Parser)]
#[command(
    name = "rw_sat",
    about = "GOES ABI live satellite ingest into the rw-store rolling window"
)]
struct Cli {
    /// Cap worker threads (defaults to the polite cores-2).
    #[arg(long, global = true)]
    threads: Option<usize>,
    /// Normal process priority and every core (dedicated nodes).
    #[arg(long, global = true)]
    full_throttle: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Args, Clone)]
struct SourceArgs {
    /// Satellite: goes19 (East) or goes18 (West).
    #[arg(long, default_value = "goes19")]
    satellite: String,
    /// Sector: conus, full_disk, meso1, meso2.
    #[arg(long, default_value = "conus")]
    sector: String,
    /// ABI bands, comma separated (e.g. 13 or 13,2).
    #[arg(long, value_delimiter = ',', default_value = "13")]
    bands: Vec<u8>,
    /// ABI scan mode token in filenames (6 = nominal).
    #[arg(long, default_value_t = 6)]
    mode: u8,
    /// Store root directory.
    #[arg(long, default_value = "store")]
    store: PathBuf,
    /// Download cache directory.
    #[arg(long, default_value = "cache")]
    cache: PathBuf,
    /// Stride-decimate frames before storing (1 = native resolution).
    #[arg(long, default_value_t = 1)]
    downsample: usize,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch the newest available scan per band, ingest, export PNGs.
    Latest {
        #[command(flatten)]
        source: SourceArgs,
        /// Directory for exported PNGs.
        #[arg(long, default_value = "out/goes")]
        png_dir: PathBuf,
        /// Also compose an RGB product PNG (geocolor, sandwich, ...) from
        /// the newest scan that has every required channel.
        #[arg(long)]
        composite: Option<String>,
        /// Extra stride decimation applied to the composite base grid.
        #[arg(long, default_value_t = 4)]
        composite_downsample: usize,
    },
    /// Poll the live bucket continuously and ingest frames as they land.
    Follow {
        #[command(flatten)]
        source: SourceArgs,
        /// Stop after this many poll cycles (omit to run until killed).
        #[arg(long)]
        polls: Option<u32>,
        /// Stop after this many ingested frames.
        #[arg(long)]
        max_frames: Option<u32>,
        /// Base poll interval in seconds (default: 15 meso / 30 CONUS / 60 FD).
        #[arg(long)]
        interval_secs: Option<u64>,
        /// Evict frames older than this (rolling window).
        #[arg(long)]
        max_age_minutes: Option<u32>,
        /// Evict oldest frames beyond this total size per followed band.
        #[arg(long)]
        max_bytes_mb: Option<u64>,
    },
    /// Export one stored frame as a PNG.
    Export {
        #[arg(long, default_value = "store")]
        store: PathBuf,
        /// Model (satellite slug), e.g. g19.
        #[arg(long)]
        model: String,
        /// Run dir name, e.g. conus_c13_20260610.
        #[arg(long)]
        run: String,
        /// Frame HHMM, e.g. 1851.
        #[arg(long)]
        hhmm: u16,
        #[arg(long)]
        out: PathBuf,
    },
    /// List recent public JMA Himawari AHI full-disk segments from NOAA S3.
    HimawariList {
        /// Satellite: h9/himawari9 (live default) or h8/himawari8.
        #[arg(long, default_value = "h9")]
        satellite: String,
        /// AHI band to filter, 1-16. Omit to show all bands in the latest scan.
        #[arg(long)]
        band: Option<u8>,
        /// Look back this many minutes from now for a scan with matching segments.
        #[arg(long, default_value_t = 180)]
        minutes: i64,
        /// Segment rows to print from the latest matching scan.
        #[arg(long, default_value_t = 20)]
        count: usize,
    },
    /// Download recent public JMA Himawari AHI full-disk segments from NOAA S3.
    HimawariDownload {
        /// Satellite: h9/himawari9 (live default) or h8/himawari8.
        #[arg(long, default_value = "h9")]
        satellite: String,
        /// AHI band to download, 1-16.
        #[arg(long, default_value_t = 13)]
        band: u8,
        /// Look back this many minutes from now for a scan with matching segments.
        #[arg(long, default_value_t = 180)]
        minutes: i64,
        /// Download cache directory.
        #[arg(long, default_value = "cache")]
        cache: PathBuf,
        /// Directory for the JSON segment manifest.
        #[arg(long, default_value = "out/himawari")]
        manifest_dir: PathBuf,
        /// Redownload even when the cache already has the expected byte size.
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        /// Limit segment count for smoke tests. Omit to fetch the full band.
        #[arg(long)]
        limit: Option<usize>,
        /// Accept the newest partially-uploaded scan instead of waiting for a complete band.
        #[arg(long, default_value_t = false)]
        allow_partial: bool,
    },
    /// Decompress a Himawari download manifest's `.DAT.bz2` segments to raw `.DAT`.
    HimawariStage {
        /// Manifest written by `himawari-download`.
        #[arg(long)]
        manifest: PathBuf,
        /// Output directory for raw `.DAT` segments.
        #[arg(long, default_value = "out/himawari/raw")]
        out_dir: PathBuf,
        /// JSON stage manifest path. Defaults inside --out-dir.
        #[arg(long)]
        stage_manifest: Option<PathBuf>,
    },
    /// Parse Himawari Standard Data headers from staged raw `.DAT` files.
    HimawariInspect {
        /// Raw `.DAT` file(s) to inspect.
        #[arg(long)]
        path: Vec<PathBuf>,
        /// Stage manifest written by `himawari-stage`; all raw paths are inspected.
        #[arg(long)]
        stage_manifest: Option<PathBuf>,
        /// Write full JSON header summaries to this path.
        #[arg(long)]
        out_json: Option<PathBuf>,
    },
    /// Decode staged Himawari Standard Data segments into the rw-store frame layout.
    HimawariIngest {
        /// Raw `.DAT` file(s) to ingest.
        #[arg(long)]
        path: Vec<PathBuf>,
        /// Stage manifest written by `himawari-stage`; all raw paths are ingested.
        #[arg(long)]
        stage_manifest: Option<PathBuf>,
        /// Store root directory.
        #[arg(long, default_value = "store")]
        store: PathBuf,
        /// Value product to store: brightness-temp/bt, radiance, or count.
        #[arg(long, default_value = "brightness-temp")]
        value: String,
        /// Stride-decimate before storing (1 = native staged resolution).
        #[arg(long, default_value_t = 1)]
        downsample: usize,
    },
    /// List recent EUMETSAT MTG FCI/LI products via public OpenSearch.
    MtgList {
        /// Collection slug/id: fci-l1c, fci-l1c-hr, li-flashes, li-events,
        /// li-groups, li-accumulated-flashes, li-accumulated-flash-area, or
        /// li-accumulated-flash-radiance.
        #[arg(long, default_value = "fci-l1c")]
        collection: String,
        /// Look back this many minutes from now.
        #[arg(long, default_value_t = 360)]
        minutes: i64,
        /// Product count, clamped to EUMETSAT's 500-result page limit.
        #[arg(long, default_value_t = 5)]
        count: usize,
    },
    /// Download one EUMETSAT MTG FCI/LI product with EUMETSAT API credentials.
    MtgDownload {
        /// Collection slug/id: fci-l1c, fci-l1c-hr, li-flashes, li-events,
        /// li-groups, li-accumulated-flashes, li-accumulated-flash-area, or
        /// li-accumulated-flash-radiance.
        #[arg(long, default_value = "fci-l1c")]
        collection: String,
        /// Product id. When omitted, the newest product in the lookback window is selected.
        #[arg(long)]
        product_id: Option<String>,
        /// Look back this many minutes from now when --product-id is omitted.
        #[arg(long, default_value_t = 360)]
        minutes: i64,
        /// Output directory for downloaded EUMETSAT ZIP/SIP products.
        #[arg(long, default_value = "out/mtg")]
        out_dir: PathBuf,
        /// EUMETSAT API consumer key. Defaults to EUMETSAT_CONSUMER_KEY.
        #[arg(long)]
        consumer_key: Option<String>,
        /// EUMETSAT API consumer secret. Defaults to EUMETSAT_CONSUMER_SECRET.
        #[arg(long)]
        consumer_secret: Option<String>,
        /// Requested EUMETSAT token validity period in seconds.
        #[arg(long, default_value_t = 86_400)]
        validity_secs: u64,
        /// Resolve the product and print the download URL without requesting a token.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Inspect or extract a downloaded EUMETSAT MTG ZIP/SIP package.
    MtgUnpack {
        /// Downloaded EUMETSAT product ZIP/SIP path.
        #[arg(long)]
        product: PathBuf,
        /// Output directory for extracted package members.
        #[arg(long, default_value = "out/mtg/unpacked")]
        out_dir: PathBuf,
        /// Extract every safe package file instead of only NetCDF members.
        #[arg(long, default_value_t = false)]
        all_files: bool,
        /// Inspect only; write no extracted members.
        #[arg(long, default_value_t = false)]
        inspect_only: bool,
        /// JSON manifest path. Defaults inside --out-dir.
        #[arg(long)]
        manifest: Option<PathBuf>,
    },
    /// Decode MTG FCI L1c NetCDF body chunks into the rw-store frame layout.
    MtgFciIngest {
        /// FCI body NetCDF file(s), normally extracted by `mtg-unpack`.
        #[arg(long)]
        path: Vec<PathBuf>,
        /// FCI channel: vis_04..ir_133 or c01..c16.
        #[arg(long, default_value = "ir_105")]
        channel: String,
        /// Value product to store: brightness-temp/bt, radiance, reflectance, or count.
        #[arg(long, default_value = "brightness-temp")]
        value: String,
        /// Store root directory.
        #[arg(long, default_value = "store")]
        store: PathBuf,
        /// Stride-decimate before storing (1 = native chunk/full-disk resolution).
        #[arg(long, default_value_t = 8)]
        downsample: usize,
        /// Optional PNG quicklook path written after the store frame is saved.
        #[arg(long)]
        png_out: Option<PathBuf>,
    },
}

fn main() {
    let cli = Cli::parse();
    rw_ingest::throttle::apply(cli.threads, cli.full_throttle);
    let result = match cli.command {
        Command::Latest {
            source,
            png_dir,
            composite,
            composite_downsample,
        } => run_latest(
            &source,
            &png_dir,
            composite.as_deref(),
            composite_downsample,
        ),
        Command::Follow {
            source,
            polls,
            max_frames,
            interval_secs,
            max_age_minutes,
            max_bytes_mb,
        } => run_follow(
            &source,
            polls,
            max_frames,
            interval_secs,
            max_age_minutes,
            max_bytes_mb,
        ),
        Command::Export {
            store,
            model,
            run,
            hhmm,
            out,
        } => export_frame_png(&store, &model, &run, hhmm, &out)
            .map(|path| println!("wrote {}", path.display()))
            .map_err(|err| err.to_string().into()),
        Command::HimawariList {
            satellite,
            band,
            minutes,
            count,
        } => run_himawari_list(&satellite, band, minutes, count),
        Command::HimawariDownload {
            satellite,
            band,
            minutes,
            cache,
            manifest_dir,
            no_cache,
            limit,
            allow_partial,
        } => run_himawari_download(
            &satellite,
            band,
            minutes,
            &cache,
            &manifest_dir,
            !no_cache,
            limit,
            allow_partial,
        ),
        Command::HimawariStage {
            manifest,
            out_dir,
            stage_manifest,
        } => run_himawari_stage(&manifest, &out_dir, stage_manifest.as_deref()),
        Command::HimawariInspect {
            path,
            stage_manifest,
            out_json,
        } => run_himawari_inspect(&path, stage_manifest.as_deref(), out_json.as_deref()),
        Command::HimawariIngest {
            path,
            stage_manifest,
            store,
            value,
            downsample,
        } => run_himawari_ingest(&path, stage_manifest.as_deref(), &store, &value, downsample),
        Command::MtgList {
            collection,
            minutes,
            count,
        } => run_mtg_list(&collection, minutes, count),
        Command::MtgDownload {
            collection,
            product_id,
            minutes,
            out_dir,
            consumer_key,
            consumer_secret,
            validity_secs,
            dry_run,
        } => run_mtg_download(
            &collection,
            product_id.as_deref(),
            minutes,
            &out_dir,
            consumer_key.as_deref(),
            consumer_secret.as_deref(),
            validity_secs,
            dry_run,
        ),
        Command::MtgUnpack {
            product,
            out_dir,
            all_files,
            inspect_only,
            manifest,
        } => run_mtg_unpack(
            &product,
            &out_dir,
            !all_files,
            inspect_only,
            manifest.as_deref(),
        ),
        Command::MtgFciIngest {
            path,
            channel,
            value,
            store,
            downsample,
            png_out,
        } => run_mtg_fci_ingest(
            &path,
            &channel,
            &value,
            &store,
            downsample,
            png_out.as_deref(),
        ),
    };
    if let Err(err) = result {
        eprintln!("rw_sat: {err}");
        std::process::exit(1);
    }
}

fn run_himawari_list(
    satellite: &str,
    band: Option<u8>,
    minutes: i64,
    count: usize,
) -> Result<(), Box<dyn Error>> {
    if let Some(band) = band {
        if !(1..=16).contains(&band) {
            return Err(format!("Himawari AHI band must be 1..=16, got {band}").into());
        }
    }
    let satellite = HimawariSatellite::parse(satellite)
        .ok_or_else(|| format!("unknown Himawari satellite '{satellite}' (choices: h9, h8)"))?;
    let request = HimawariLatestRequest {
        satellite,
        product: HimawariProduct::AhiL1bFldk,
        band,
        lookback_minutes: minutes.max(10),
        require_complete: false,
    };
    let agent = build_agent();
    let result = list_latest_segments(&agent, &request)?;
    let limit = count.max(1);
    println!(
        "himawari-list {} | {} | {} | scan {} | {} segment(s), showing {}",
        result.satellite.slug(),
        result.satellite.bucket(),
        result.product.slug(),
        result.scan_time.format("%Y-%m-%dT%H:%M:%SZ"),
        result.segments.len(),
        result.segments.len().min(limit),
    );
    println!("prefix: {}", result.prefix);
    if let Some(band) = band {
        println!("band: B{band:02}");
    }
    for segment in result.segments.iter().take(limit) {
        println!(
            "B{:02} S{:02}/{:02} {} | {} bytes | modified {}",
            segment.name.band,
            segment.name.segment_index,
            segment.name.segment_count,
            object_url(result.satellite.bucket(), &segment.object.key),
            segment.object.size_bytes,
            segment.object.last_modified
        );
    }
    Ok(())
}

fn run_himawari_download(
    satellite: &str,
    band: u8,
    minutes: i64,
    cache: &Path,
    manifest_dir: &Path,
    use_cache: bool,
    limit: Option<usize>,
    allow_partial: bool,
) -> Result<(), Box<dyn Error>> {
    if !(1..=16).contains(&band) {
        return Err(format!("Himawari AHI band must be 1..=16, got {band}").into());
    }
    let satellite = HimawariSatellite::parse(satellite)
        .ok_or_else(|| format!("unknown Himawari satellite '{satellite}' (choices: h9, h8)"))?;
    let request = HimawariLatestRequest {
        satellite,
        product: HimawariProduct::AhiL1bFldk,
        band: Some(band),
        lookback_minutes: minutes.max(10),
        require_complete: !allow_partial,
    };
    let agent = build_agent();
    let result = list_latest_segments(&agent, &request)?;
    let source_complete = is_complete_segment_set(&result.segments);
    let segment_count = limit
        .map(|limit| limit.max(1).min(result.segments.len()))
        .unwrap_or(result.segments.len());
    println!(
        "himawari-download {} | {} | B{band:02} | scan {} | {} segment(s)",
        result.satellite.slug(),
        result.satellite.bucket(),
        result.scan_time.format("%Y-%m-%dT%H:%M:%SZ"),
        segment_count,
    );
    if !source_complete {
        println!("warning: selected scan is partial");
    }

    let mut manifest_segments = Vec::with_capacity(segment_count);
    let mut total_bytes = 0_u64;
    for segment in result.segments.iter().take(segment_count) {
        let downloaded = download_object(
            &agent,
            result.satellite.bucket(),
            cache,
            &segment.object,
            use_cache,
        )?;
        total_bytes = total_bytes.saturating_add(segment.object.size_bytes);
        println!(
            "B{:02} S{:02}/{:02} {} {} bytes {}",
            segment.name.band,
            segment.name.segment_index,
            segment.name.segment_count,
            if downloaded.cache_hit {
                "cache"
            } else {
                "download"
            },
            segment.object.size_bytes,
            downloaded.path.display()
        );
        manifest_segments.push(HimawariManifestSegment {
            band: segment.name.band,
            segment_index: segment.name.segment_index,
            segment_count: segment.name.segment_count,
            product: segment.name.product.clone(),
            resolution: segment.name.resolution.clone(),
            key: segment.object.key.clone(),
            url: object_url(result.satellite.bucket(), &segment.object.key),
            last_modified: segment.object.last_modified.clone(),
            size_bytes: segment.object.size_bytes,
            cache_path: downloaded.path.display().to_string(),
            cache_hit: downloaded.cache_hit,
        });
    }

    std::fs::create_dir_all(manifest_dir)?;
    let manifest_path = manifest_dir.join(format!(
        "{}_{}_b{band:02}_{}.json",
        result.satellite.slug(),
        result.product.slug(),
        result.scan_time.format("%Y%m%dT%H%M%SZ")
    ));
    let manifest = HimawariDownloadManifest {
        schema: HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA.to_string(),
        satellite: result.satellite.slug().to_string(),
        platform: result.satellite.platform().to_string(),
        bucket: result.satellite.bucket().to_string(),
        product: result.product.slug().to_string(),
        scan_time_utc: result.scan_time.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        prefix: result.prefix,
        band,
        segments_downloaded: manifest_segments.len(),
        segments_available: result.segments.len(),
        source_complete,
        allow_partial,
        total_downloaded_bytes: total_bytes,
        cache_root: cache.display().to_string(),
        segments: manifest_segments,
    };
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    std::fs::write(&manifest_path, bytes)?;
    println!("manifest {}", manifest_path.display());
    Ok(())
}

fn run_himawari_stage(
    manifest: &Path,
    out_dir: &Path,
    stage_manifest: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let result = stage_download_manifest(manifest, out_dir)?;
    let stage_manifest = stage_manifest
        .map(PathBuf::from)
        .unwrap_or_else(|| out_dir.join("himawari_stage_manifest.json"));
    println!(
        "himawari-stage {} | {} segment(s) | {} compressed bytes -> {} raw bytes",
        manifest.display(),
        result.segments_staged,
        result.total_compressed_bytes,
        result.total_raw_bytes
    );
    for segment in &result.segments {
        println!(
            "B{:02} S{:02}/{:02} {} bytes {}",
            segment.band,
            segment.segment_index,
            segment.segment_count,
            segment.raw_bytes,
            segment.raw_path
        );
    }
    write_json_manifest(&stage_manifest, &result)?;
    println!("manifest {}", stage_manifest.display());
    Ok(())
}

fn run_himawari_inspect(
    paths: &[PathBuf],
    stage_manifest: Option<&Path>,
    out_json: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let inspect_paths = collect_himawari_paths(paths, stage_manifest)?;
    if inspect_paths.is_empty() {
        return Err("pass at least one --path or a --stage-manifest".into());
    }

    let mut headers = Vec::with_capacity(inspect_paths.len());
    for path in inspect_paths {
        let header = inspect_hsd_file(&path)?;
        let band = header
            .calibration
            .as_ref()
            .map(|calibration| format!("B{:02}", calibration.band_number))
            .unwrap_or_else(|| "B??".to_string());
        let valid_bits = header
            .calibration
            .as_ref()
            .map(|calibration| calibration.valid_bits_per_pixel.to_string())
            .unwrap_or_else(|| "?".to_string());
        let segment = header
            .segment
            .as_ref()
            .map(|segment| {
                format!(
                    "S{:02}/{:02} first_line {}",
                    segment.sequence_number, segment.total_segments, segment.first_line_number
                )
            })
            .unwrap_or_else(|| "S??/??".to_string());
        println!(
            "himawari-inspect {} | {} {} {} | {}x{} | bits {} valid {} | data {} | {} | header {} data {} | length_match {}",
            path.display(),
            header.satellite_name,
            header.observation_area,
            band,
            header.data.columns,
            header.data.lines,
            header.data.bits_per_pixel,
            valid_bits,
            header.data.compression,
            segment,
            header.total_header_length,
            header.total_data_length,
            header.length_matches_header
        );
        headers.push(header);
    }

    if let Some(out_json) = out_json {
        write_json_manifest(out_json, &headers)?;
        println!("manifest {}", out_json.display());
    }
    Ok(())
}

fn run_himawari_ingest(
    paths: &[PathBuf],
    stage_manifest: Option<&Path>,
    store: &Path,
    value: &str,
    downsample: usize,
) -> Result<(), Box<dyn Error>> {
    let ingest_paths = collect_himawari_paths(paths, stage_manifest)?;
    if ingest_paths.is_empty() {
        return Err("pass at least one --path or a --stage-manifest".into());
    }
    let mode = HimawariValueMode::parse(value).ok_or_else(|| {
        format!("unknown Himawari value '{value}' (choices: brightness-temp, bt, radiance, count)")
    })?;
    let field = assemble_hsd_segments(&ingest_paths, mode, downsample.max(1))?;
    let nx = field.scene.fixed_grid.nx;
    let ny = field.scene.fixed_grid.ny;
    let variable = field.variable_name.clone();
    let stats = finite_stats(&field.values);
    let written_unix = Utc::now().timestamp().max(0) as u64;
    let frame = write_satellite_grid_frame(store, &field, written_unix)?;
    println!(
        "himawari-ingest {} segment file(s) | {} {}x{} {} | finite {}/{} min {:.2} max {:.2} | wrote {}/{}/t{:04}.rws ({} bytes, variable {})",
        ingest_paths.len(),
        mode.slug(),
        nx,
        ny,
        field.units,
        stats.finite_count,
        field.values.len(),
        stats.min,
        stats.max,
        frame.model,
        frame.run,
        frame.hhmm,
        frame.bytes,
        variable
    );
    Ok(())
}

struct FiniteStats {
    finite_count: usize,
    min: f32,
    max: f32,
}

fn finite_stats(values: &[f32]) -> FiniteStats {
    let mut finite_count = 0_usize;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &value in values {
        if value.is_finite() {
            finite_count += 1;
            min = min.min(value);
            max = max.max(value);
        }
    }
    if finite_count == 0 {
        min = f32::NAN;
        max = f32::NAN;
    }
    FiniteStats {
        finite_count,
        min,
        max,
    }
}

fn collect_himawari_paths(
    paths: &[PathBuf],
    stage_manifest: Option<&Path>,
) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut collected = paths.to_vec();
    if let Some(stage_manifest) = stage_manifest {
        let manifest = read_himawari_stage_manifest(stage_manifest)?;
        collected.extend(
            manifest
                .segments
                .iter()
                .map(|segment| PathBuf::from(&segment.raw_path)),
        );
    }
    Ok(collected)
}

fn read_himawari_stage_manifest(path: &Path) -> Result<HimawariStageManifest, Box<dyn Error>> {
    let bytes = std::fs::read(path)?;
    let manifest: HimawariStageManifest = serde_json::from_slice(&bytes)?;
    if manifest.schema != HIMAWARI_STAGE_MANIFEST_SCHEMA {
        return Err(format!(
            "unsupported Himawari stage manifest schema '{}' in {}",
            manifest.schema,
            path.display()
        )
        .into());
    }
    Ok(manifest)
}

fn run_mtg_list(collection: &str, minutes: i64, count: usize) -> Result<(), Box<dyn Error>> {
    let collection = parse_mtg_collection(collection)?;
    let end = Utc::now();
    let start = end - chrono::Duration::minutes(minutes.max(1));
    let request = MtgSearchRequest::new(collection, start, end, count);
    let agent = build_agent();
    let result = search_products(&agent, &request)?;
    println!(
        "mtg-list {} | {} | {} product(s) returned of {} in the last {} min",
        collection.slug(),
        collection.collection_id(),
        result.products.len(),
        result.total_results,
        minutes.max(1),
    );
    println!("collection: {}", collection.title());
    println!("browse: {}", collection.collection_browse_url());
    println!("navigator: {}", collection.product_page_url());
    for product in result.products {
        println!(
            "{} | {} | updated {}",
            product.date.as_deref().unwrap_or("(no sensing time)"),
            product.id,
            product.updated.as_deref().unwrap_or("(unknown)")
        );
        for link in product.data_links.iter().take(1) {
            println!("  opensearch link: {}", link.href);
        }
        println!(
            "  download: {}",
            collection.product_download_url(&product.id)
        );
    }
    Ok(())
}

fn run_mtg_download(
    collection: &str,
    product_id: Option<&str>,
    minutes: i64,
    out_dir: &Path,
    consumer_key: Option<&str>,
    consumer_secret: Option<&str>,
    validity_secs: u64,
    dry_run: bool,
) -> Result<(), Box<dyn Error>> {
    let collection = parse_mtg_collection(collection)?;
    let agent = build_agent();
    let product_id = match product_id {
        Some(product_id) => product_id.to_string(),
        None => newest_mtg_product_id(&agent, collection, minutes)?,
    };
    println!(
        "collection: {} | {}",
        collection.slug(),
        collection.collection_id()
    );
    println!("product: {product_id}");
    println!("download: {}", collection.product_download_url(&product_id));
    if dry_run {
        println!("dry-run: no EUMETSAT token requested and no bytes downloaded");
        return Ok(());
    }

    let credentials = credentials_from_args(consumer_key, consumer_secret)?;
    let token = request_access_token(&agent, &credentials, validity_secs)?;
    println!(
        "token: acquired; expires in {}s (unix {})",
        token.expires_in, token.expires_at_unix
    );
    let downloaded = download_product(&agent, collection, &product_id, &token, out_dir)?;
    println!(
        "wrote {} ({} bytes)",
        downloaded.path.display(),
        downloaded.bytes
    );
    Ok(())
}

fn run_mtg_unpack(
    product: &Path,
    out_dir: &Path,
    netcdf_only: bool,
    inspect_only: bool,
    manifest_path: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let manifest_path = manifest_path
        .map(PathBuf::from)
        .unwrap_or_else(|| out_dir.join("mtg_package_manifest.json"));
    if inspect_only {
        let manifest = inspect_package(product)?;
        println!(
            "mtg-unpack inspect {} | {} entries | {} file(s) | {} NetCDF | FCI {} | LI {}",
            product.display(),
            manifest.entry_count,
            manifest.file_count,
            manifest.netcdf_count,
            manifest.fci_count,
            manifest.li_count
        );
        write_json_manifest(&manifest_path, &manifest)?;
        println!("manifest {}", manifest_path.display());
        return Ok(());
    }

    let result = unpack_package(product, out_dir, netcdf_only)?;
    println!(
        "mtg-unpack {} | {} entries | extracted {} file(s) to {}",
        product.display(),
        result.manifest.entry_count,
        result.extracted.len(),
        out_dir.display()
    );
    println!(
        "package members: {} file(s), {} NetCDF, FCI {}, LI {}",
        result.manifest.file_count,
        result.manifest.netcdf_count,
        result.manifest.fci_count,
        result.manifest.li_count
    );
    for entry in &result.extracted {
        println!("  {} bytes {}", entry.bytes, entry.path.display());
    }
    write_json_manifest(&manifest_path, &result)?;
    println!("manifest {}", manifest_path.display());
    Ok(())
}

fn run_mtg_fci_ingest(
    paths: &[PathBuf],
    channel: &str,
    value: &str,
    store: &Path,
    downsample: usize,
    png_out: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    if paths.is_empty() {
        return Err("pass at least one --path to an FCI body NetCDF file".into());
    }
    let channel = FciChannel::parse(channel).ok_or_else(|| {
        format!(
            "unknown FCI channel '{channel}' (choices: {}; or c01..c16)",
            FciChannel::choices()
        )
    })?;
    let mode = FciValueMode::parse(value).ok_or_else(|| {
        "unknown FCI value '{value}' (choices: brightness-temp, bt, radiance, reflectance, count)"
            .to_string()
    })?;
    let field = assemble_fci_chunks(paths, channel, mode, downsample.max(1))?;
    let nx = field.scene.fixed_grid.nx;
    let ny = field.scene.fixed_grid.ny;
    let variable = field.variable_name.clone();
    let stats = finite_stats(&field.values);
    let written_unix = Utc::now().timestamp().max(0) as u64;
    let frame = write_satellite_grid_frame(store, &field, written_unix)?;
    println!(
        "mtg-fci-ingest {} chunk file(s) | {} {} {}x{} {} | finite {}/{} min {:.2} max {:.2} | wrote {}/{}/t{:04}.rws ({} bytes, variable {})",
        paths.len(),
        channel.name,
        mode.slug(),
        nx,
        ny,
        field.units,
        stats.finite_count,
        field.values.len(),
        stats.min,
        stats.max,
        frame.model,
        frame.run,
        frame.hhmm,
        frame.bytes,
        variable
    );
    if let Some(png_out) = png_out {
        let path = export_frame_png(store, &frame.model, &frame.run, frame.hhmm, png_out)?;
        println!("png {}", path.display());
    }
    Ok(())
}

fn write_json_manifest<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let bytes = serde_json::to_vec_pretty(value)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

fn newest_mtg_product_id(
    agent: &ureq::Agent,
    collection: MtgCollection,
    minutes: i64,
) -> Result<String, Box<dyn Error>> {
    let end = Utc::now();
    let start = end - chrono::Duration::minutes(minutes.max(1));
    let request = MtgSearchRequest::new(collection, start, end, 1);
    let result = search_products(agent, &request)?;
    result
        .products
        .into_iter()
        .next()
        .map(|product| product.id)
        .ok_or_else(|| {
            format!(
                "no {} products found in the last {} min",
                collection.slug(),
                minutes.max(1)
            )
            .into()
        })
}

fn parse_mtg_collection(collection: &str) -> Result<MtgCollection, Box<dyn Error>> {
    MtgCollection::parse(collection).ok_or_else(|| {
        let choices = MtgCollection::ALL
            .iter()
            .map(|collection| collection.slug())
            .collect::<Vec<_>>()
            .join(", ");
        format!("unknown MTG collection '{collection}' (choices: {choices})").into()
    })
}

fn credentials_from_args(
    consumer_key: Option<&str>,
    consumer_secret: Option<&str>,
) -> Result<EumetsatCredentials, Box<dyn Error>> {
    match (consumer_key, consumer_secret) {
        (Some(key), Some(secret)) => Ok(EumetsatCredentials::new(key, secret)),
        (None, None) => EumetsatCredentials::from_env(),
        _ => Err("pass both --consumer-key and --consumer-secret, or neither to use EUMETSAT_CONSUMER_KEY/EUMETSAT_CONSUMER_SECRET".into()),
    }
}

fn parse_sector(value: &str) -> Result<Sector, Box<dyn Error>> {
    Sector::parse(value)
        .ok_or_else(|| format!("unknown sector '{value}' (conus, full_disk, meso1, meso2)").into())
}

/// List the newest objects for one band: current hour prefix, then walk
/// back up to `lookback_hours` while empty.
fn newest_band_objects(
    agent: &ureq::Agent,
    bucket: &str,
    sector: Sector,
    satellite: &GoesSatellite,
    mode: u8,
    band: u8,
    lookback_hours: u32,
) -> Result<Vec<S3Object>, Box<dyn Error>> {
    let product = sector.abi_product();
    let mut objects = Vec::new();
    for back in 0..=lookback_hours {
        let when = Utc::now() - chrono::Duration::hours(i64::from(back));
        for prefix in poll_prefixes(product, satellite, mode, band, when) {
            let listed = list_s3_objects(agent, bucket, &prefix, None)?;
            objects.extend(listed);
        }
        if !objects.is_empty() {
            break;
        }
    }
    objects.retain(|object| {
        object.key.ends_with(".nc")
            && parse_goes_abi_filename(object_filename(&object.key)).is_ok_and(|parsed| {
                abi_filename_product_matches_request(&parsed.product, product)
                    && parsed.channel == Some(band)
            })
    });
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    objects.dedup_by(|a, b| a.key == b.key);
    Ok(objects)
}

fn run_latest(
    source: &SourceArgs,
    png_dir: &Path,
    composite: Option<&str>,
    composite_downsample: usize,
) -> Result<(), Box<dyn Error>> {
    let sector = parse_sector(&source.sector)?;
    let bucket = bucket_for_satellite(&source.satellite)?;
    let satellite = GoesSatellite::parse(&source.satellite);
    let agent = build_agent();
    let mut sink = |event: SatEvent| print_event(&event);

    for &band in &source.bands {
        let objects =
            newest_band_objects(&agent, &bucket, sector, &satellite, source.mode, band, 3)?;
        let Some(newest) = objects.last() else {
            eprintln!("no recent C{band:02} objects found in {bucket}");
            continue;
        };
        println!(
            "latest C{band:02}: {} ({} bytes, last-modified {})",
            object_url(&bucket, &newest.key),
            newest.size_bytes,
            newest.last_modified
        );
        let written_unix = Utc::now().timestamp().max(0) as u64;
        let (_download, frame) = fetch_and_ingest(
            &agent,
            &bucket,
            &source.cache,
            &source.store,
            newest,
            source.downsample,
            true,
            written_unix,
            &mut sink,
        )
        .map_err(|err| -> Box<dyn Error> { err.to_string().into() })?;
        let png_path = png_dir.join(format!(
            "{}_{}_t{:04}_{}.png",
            frame.model, frame.run, frame.hhmm, frame.variable
        ));
        let path = export_frame_png(
            &source.store,
            &frame.model,
            &frame.run,
            frame.hhmm,
            &png_path,
        )?;
        println!("png {}", path.display());
    }

    if let Some(style_name) = composite {
        let style = GoesAbiRgbCompositeStyle::parse(style_name)
            .ok_or_else(|| format!("unknown composite style '{style_name}'"))?;
        run_latest_composite(
            &agent,
            &bucket,
            sector,
            &satellite,
            source,
            style,
            composite_downsample,
            png_dir,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_latest_composite(
    agent: &ureq::Agent,
    bucket: &str,
    sector: Sector,
    satellite: &GoesSatellite,
    source: &SourceArgs,
    style: GoesAbiRgbCompositeStyle,
    downsample: usize,
    png_dir: &Path,
) -> Result<(), Box<dyn Error>> {
    let required = style.required_channels();
    let mut sink = |event: SatEvent| print_event(&event);

    // Newest scan start time that has every required channel.
    let mut by_scan: HashMap<chrono::DateTime<Utc>, HashMap<u8, S3Object>> = HashMap::new();
    for &band in required {
        for object in newest_band_objects(agent, bucket, sector, satellite, source.mode, band, 3)? {
            if let Ok(parsed) = parse_goes_abi_filename(object_filename(&object.key)) {
                by_scan
                    .entry(parsed.start_time_utc)
                    .or_default()
                    .insert(band, object);
            }
        }
    }
    let Some((scan_time, channel_objects)) = by_scan
        .into_iter()
        .filter(|(_, channels)| required.iter().all(|band| channels.contains_key(band)))
        .max_by_key(|(time, _)| *time)
    else {
        return Err(format!(
            "no recent scan carries all channels {required:?} for {}",
            style.slug()
        )
        .into());
    };
    println!(
        "composite {} from scan {}",
        style.slug(),
        scan_time.format("%Y-%m-%dT%H:%M:%SZ")
    );

    // Download all channels, decode, resample onto the (decimated) base grid.
    let mut fields = HashMap::new();
    for &band in required {
        let object = &channel_objects[&band];
        println!(
            "  C{band:02}: {} ({} bytes)",
            object_url(bucket, &object.key),
            object.size_bytes
        );
        let download = rw_sat::s3::download_object(agent, bucket, &source.cache, object, true)?;
        let field = read_goes_abi_field(&download.path, "CMI")?;
        fields.insert(band, field);
        let _ = &mut sink; // events reserved for the band path
    }
    let base = fields
        .remove(&style.base_channel())
        .ok_or("missing base channel after download")?;
    let base = downsample_field(base, downsample.max(1));
    let (nx, ny) = (base.scene.fixed_grid.nx, base.scene.fixed_grid.ny);
    let mut bands: HashMap<u8, Vec<f32>> = HashMap::new();
    for (band, field) in &fields {
        bands.insert(*band, values_on_base_grid(field, &base.scene)?);
    }
    bands.insert(style.base_channel(), base.values.clone());
    drop(fields);

    let image = render_composite_image(style, &bands, nx, ny)?;
    let png_path = png_dir.join(format!(
        "{}_{}_{}_{}.png",
        satellite.as_str().to_ascii_lowercase(),
        sector.slug(),
        style.slug(),
        scan_time.format("%Y%m%dT%H%M%SZ")
    ));
    if let Some(parent) = png_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    image.save(&png_path)?;
    println!("png {}", png_path.display());
    Ok(())
}

fn run_follow(
    source: &SourceArgs,
    polls: Option<u32>,
    max_frames: Option<u32>,
    interval_secs: Option<u64>,
    max_age_minutes: Option<u32>,
    max_bytes_mb: Option<u64>,
) -> Result<(), Box<dyn Error>> {
    let sector = parse_sector(&source.sector)?;
    let mut config = FollowConfig::new(&source.satellite, sector, source.bands.clone());
    config.mode = source.mode;
    config.store_root = source.store.clone();
    config.cache_dir = source.cache.clone();
    config.downsample = source.downsample;
    config.poll_interval = interval_secs.map(Duration::from_secs);
    config.max_polls = polls;
    config.max_frames = max_frames;
    config.window = WindowConfig {
        max_age_minutes,
        max_bytes: max_bytes_mb.map(|mb| mb.saturating_mul(1024 * 1024)),
    };

    let mut sink = |event: SatEvent| print_event(&event);
    let summary = follow(&config, &mut sink, &NEVER_CANCEL)
        .map_err(|err| -> Box<dyn Error> { err.to_string().into() })?;
    println!(
        "follow done: {} poll(s), {} frame(s), {} evicted ({} bytes)",
        summary.polls,
        summary.frames.len(),
        summary.evicted_frames,
        summary.evicted_bytes
    );
    for frame in &summary.frames {
        println!(
            "  {}/{}/t{:04} scan {} ({} bytes)",
            frame.model,
            frame.run,
            frame.hhmm,
            frame.scan_time_utc.format("%Y-%m-%dT%H:%M:%SZ"),
            frame.bytes
        );
    }
    Ok(())
}

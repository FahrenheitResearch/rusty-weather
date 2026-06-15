use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use clap::Parser;
use rw_glm::{
    BucketWriter, DensityBounds, FlashDensityRequest, FlashRecord, ValidateDepth,
    decode_mtg_li_flashes, flash_density, validate_bucket_file,
};
use rw_sat::export::export_frame_png;
use rw_sat::fci::{FciChannel, FciValueMode, assemble_fci_chunks};
use rw_sat::follow::poll_prefixes;
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::himawari::{
    HIMAWARI_DOWNLOAD_MANIFEST_SCHEMA, HimawariDownloadManifest, HimawariLatestRequest,
    HimawariManifestSegment, HimawariProduct, HimawariSatellite, HimawariValueMode,
    assemble_hsd_segments, is_complete_segment_set, list_latest_segments, stage_download_manifest,
};
use rw_sat::mtg::{
    EumetsatCredentials, MtgCollection, MtgSearchRequest, download_product, request_access_token,
    search_products, unpack_package,
};
use rw_sat::s3::{
    Sector, abi_filename_product_matches_request, bucket_for_satellite, build_agent,
    download_object, list_s3_objects, object_filename, object_url,
};
use rw_sat::store::write_satellite_grid_frame;

#[derive(Parser, Debug)]
#[command(
    name = "rusty_sat_check",
    about = "One-shot live satellite access check for rusty-weather"
)]
struct Args {
    /// Download/cache root used for NOAA S3 objects.
    #[arg(long, default_value = "cache")]
    cache: PathBuf,
    /// Output root for manifests, MTG packages, quicklook stores, and reports.
    #[arg(long, default_value = "out/sat_check")]
    out_dir: PathBuf,
    /// Store root used for smoke-ingested Himawari and MTG LI products.
    #[arg(long, default_value = "out/sat_check/store")]
    store_root: PathBuf,
    /// EUMETSAT OpenSearch lookback in minutes.
    #[arg(long, default_value_t = 720)]
    mtg_minutes: i64,
    /// Himawari lookback in minutes.
    #[arg(long, default_value_t = 180)]
    himawari_minutes: i64,
    /// Himawari segment count for the smoke ingest.
    #[arg(long, default_value_t = 2)]
    himawari_segments: usize,
    /// Himawari stride decimation for the smoke ingest.
    #[arg(long, default_value_t = 8)]
    himawari_downsample: usize,
    /// Local MTG FCI body NetCDF sample(s) to decode/write/export.
    #[arg(long)]
    fci_sample: Vec<PathBuf>,
    /// FCI channel for sample/live decode: vis_04..ir_133 or c01..c16.
    #[arg(long, default_value = "ir_105")]
    fci_channel: String,
    /// FCI value product: brightness-temp/bt, radiance, reflectance, or count.
    #[arg(long, default_value = "brightness-temp")]
    fci_value: String,
    /// FCI stride decimation for the smoke ingest.
    #[arg(long, default_value_t = 8)]
    fci_downsample: usize,
    /// Also download/unpack the latest FCI L1c package. This can be large.
    #[arg(long, default_value_t = false)]
    download_fci: bool,
    /// EUMETSAT token validity request in seconds.
    #[arg(long, default_value_t = 3600)]
    validity_secs: u64,
}

#[derive(Default)]
struct Report {
    passed: usize,
    failed: usize,
}

impl Report {
    fn pass(&mut self, name: &str, detail: impl AsRef<str>) {
        self.passed += 1;
        println!("[PASS] {name}: {}", detail.as_ref());
    }

    fn fail(&mut self, name: &str, err: &dyn Error) {
        self.failed += 1;
        println!("[FAIL] {name}: {err}");
    }
}

fn main() {
    let args = Args::parse();
    if let Err(err) = fs::create_dir_all(&args.out_dir) {
        eprintln!(
            "rusty_sat_check: failed to create {}: {err}",
            args.out_dir.display()
        );
        std::process::exit(1);
    }
    println!("rusty_sat_check");
    println!("out_dir: {}", args.out_dir.display());
    println!("store_root: {}", args.store_root.display());
    println!("credentials: reading EUMETSAT_CONSUMER_KEY/EUMETSAT_CONSUMER_SECRET from env only");
    println!();

    let agent = build_agent();
    let mut report = Report::default();

    match check_goes_abi(&agent) {
        Ok(detail) => report.pass("GOES ABI public S3", detail),
        Err(err) => report.fail("GOES ABI public S3", err.as_ref()),
    }

    match check_himawari(&agent, &args) {
        Ok(detail) => report.pass("Himawari AHI download/stage/ingest", detail),
        Err(err) => report.fail("Himawari AHI download/stage/ingest", err.as_ref()),
    }

    match check_mtg_discovery(&agent, MtgCollection::FciL1cNormal, args.mtg_minutes) {
        Ok(detail) => report.pass("MTG FCI discovery", detail),
        Err(err) => report.fail("MTG FCI discovery", err.as_ref()),
    }

    if args.fci_sample.is_empty() {
        println!("[SKIP] MTG FCI decode/write/export: pass --fci-sample <body.nc>");
    } else {
        match check_mtg_fci_decode_paths(&args.fci_sample, &args) {
            Ok(detail) => report.pass("MTG FCI decode/write/export", detail),
            Err(err) => report.fail("MTG FCI decode/write/export", err.as_ref()),
        }
    }

    match check_mtg_li(&agent, &args) {
        Ok(detail) => report.pass("MTG LI download/unpack/rwl/density", detail),
        Err(err) => report.fail("MTG LI download/unpack/rwl/density", err.as_ref()),
    }

    if args.download_fci {
        match check_mtg_fci_download(&agent, &args) {
            Ok(detail) => report.pass("MTG FCI download/unpack", detail),
            Err(err) => report.fail("MTG FCI download/unpack", err.as_ref()),
        }
    } else {
        println!(
            "[SKIP] MTG FCI download/unpack: pass --download-fci to pull the large image package"
        );
    }

    println!();
    println!(
        "summary: {} passed, {} failed",
        report.passed, report.failed
    );
    if report.failed > 0 {
        std::process::exit(1);
    }
}

fn check_goes_abi(agent: &ureq::Agent) -> Result<String, Box<dyn Error>> {
    let bucket = bucket_for_satellite("goes19")?;
    let satellite = GoesSatellite::parse("goes19");
    let sector = Sector::Conus;
    let product = sector.abi_product();
    let mut objects = Vec::new();
    for back in 0..=3 {
        let when = Utc::now() - chrono::Duration::hours(back);
        for prefix in poll_prefixes(product, &satellite, 6, 13, when) {
            objects.extend(list_s3_objects(agent, &bucket, &prefix, None)?);
        }
        if !objects.is_empty() {
            break;
        }
    }
    objects.retain(|object| {
        object.key.ends_with(".nc")
            && parse_goes_abi_filename(object_filename(&object.key)).is_ok_and(|parsed| {
                parsed.channel == Some(13)
                    && abi_filename_product_matches_request(&parsed.product, product)
            })
    });
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    let latest = objects
        .last()
        .ok_or("no recent GOES-19 CONUS C13 object found")?;
    Ok(format!(
        "{} object(s), latest {} ({} bytes)",
        objects.len(),
        object_url(&bucket, &latest.key),
        latest.size_bytes
    ))
}

fn check_himawari(agent: &ureq::Agent, args: &Args) -> Result<String, Box<dyn Error>> {
    let request = HimawariLatestRequest {
        satellite: HimawariSatellite::H9,
        product: HimawariProduct::AhiL1bFldk,
        band: Some(13),
        lookback_minutes: args.himawari_minutes.max(10),
        require_complete: true,
    };
    let result = list_latest_segments(agent, &request)?;
    let source_complete = is_complete_segment_set(&result.segments);
    let segment_count = args.himawari_segments.max(1).min(result.segments.len());
    let mut manifest_segments = Vec::with_capacity(segment_count);
    let mut total_bytes = 0_u64;
    for segment in result.segments.iter().take(segment_count) {
        let downloaded = download_object(
            agent,
            result.satellite.bucket(),
            &args.cache,
            &segment.object,
            true,
        )?;
        total_bytes = total_bytes.saturating_add(segment.object.size_bytes);
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

    let himawari_dir = args.out_dir.join("himawari");
    fs::create_dir_all(&himawari_dir)?;
    let manifest_path = himawari_dir.join(format!(
        "h9_ahi_b13_{}.json",
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
        band: 13,
        segments_downloaded: manifest_segments.len(),
        segments_available: result.segments.len(),
        source_complete,
        allow_partial: false,
        total_downloaded_bytes: total_bytes,
        cache_root: args.cache.display().to_string(),
        segments: manifest_segments,
    };
    write_json(&manifest_path, &manifest)?;

    let raw_dir = himawari_dir.join("raw");
    let staged = stage_download_manifest(&manifest_path, &raw_dir)?;
    let stage_manifest = raw_dir.join("himawari_stage_manifest.json");
    write_json(&stage_manifest, &staged)?;

    let paths = staged
        .segments
        .iter()
        .map(|segment| PathBuf::from(&segment.raw_path))
        .collect::<Vec<_>>();
    let field = assemble_hsd_segments(
        &paths,
        HimawariValueMode::BrightnessTemperature,
        args.himawari_downsample.max(1),
    )?;
    let stats = finite_stats(&field.values);
    let frame = write_satellite_grid_frame(
        &args.store_root,
        &field,
        Utc::now().timestamp().max(0) as u64,
    )?;
    Ok(format!(
        "scan {}, staged {} segment(s) to {}, wrote {}/{}/t{:04}.rws {}x{} finite {}/{} min {:.2} max {:.2}",
        result.scan_time.format("%Y-%m-%dT%H:%M:%SZ"),
        staged.segments_staged,
        raw_dir.display(),
        frame.model,
        frame.run,
        frame.hhmm,
        field.scene.fixed_grid.nx,
        field.scene.fixed_grid.ny,
        stats.count,
        field.values.len(),
        stats.min,
        stats.max
    ))
}

fn check_mtg_discovery(
    agent: &ureq::Agent,
    collection: MtgCollection,
    minutes: i64,
) -> Result<String, Box<dyn Error>> {
    let result = newest_mtg_products(agent, collection, minutes, 2)?;
    let first = result
        .products
        .first()
        .ok_or_else(|| format!("no {} products found", collection.slug()))?;
    Ok(format!(
        "{} returned {} of {}; newest {}",
        collection.slug(),
        result.products.len(),
        result.total_results,
        first.date.as_deref().unwrap_or("(no time)")
    ))
}

fn check_mtg_li(agent: &ureq::Agent, args: &Args) -> Result<String, Box<dyn Error>> {
    let products = newest_mtg_products(
        agent,
        MtgCollection::LiLightningFlashes,
        args.mtg_minutes,
        1,
    )?;
    let product = products
        .products
        .first()
        .ok_or("no MTG LI flash products found")?;
    let token = request_eumetsat_token(agent, args.validity_secs)?;
    let li_dir = args.out_dir.join("mtg_li");
    let downloaded = download_product(
        agent,
        MtgCollection::LiLightningFlashes,
        &product.id,
        &token,
        &li_dir,
    )?;
    let unpack_dir = li_dir.join("unpacked");
    let unpacked = unpack_package(&downloaded.path, &unpack_dir, true)?;
    let body = unpacked
        .extracted
        .iter()
        .find(|entry| entry.name.contains("CHK-BODY") && entry.name.ends_with(".nc"))
        .or_else(|| {
            unpacked
                .extracted
                .iter()
                .find(|entry| entry.name.ends_with(".nc"))
        })
        .ok_or("MTG LI package unpacked no NetCDF body file")?;
    let short_body = li_dir.join("li_body.nc");
    fs::copy(&body.path, &short_body)?;
    let decoded = decode_mtg_li_flashes(&short_body)?;
    let records = decoded
        .flashes
        .iter()
        .map(|flash| FlashRecord {
            time_unix_ms: flash.time_unix_ms,
            lat: flash.lat,
            lon: flash.lon,
            energy: flash.energy,
            area: flash.area,
            flash_id: flash.flash_id,
            flags: flash.flags,
            duration_ms: flash.duration_ms,
        })
        .collect::<Vec<_>>();
    if records.is_empty() {
        return Err("MTG LI body decoded zero flashes".into());
    }
    let mut min_time = i64::MAX;
    let mut max_time = i64::MIN;
    for record in &records {
        min_time = min_time.min(record.time_unix_ms);
        max_time = max_time.max(record.time_unix_ms);
    }

    let mut writer = BucketWriter::open(&args.store_root, "mtg-li")?;
    let affected = writer.affected_bucket_paths(&records);
    let seen_key = product.id.as_str();
    let already_seen = writer
        .load_manifest()
        .seen_granule_keys
        .iter()
        .any(|key| key == seen_key);
    if !already_seen {
        writer.insert_flashes(&records, 1)?;
        writer.record_seen_granule(seen_key)?;
    }
    drop(writer);

    let mut affected = affected;
    affected.sort();
    affected.dedup();
    for path in &affected {
        let report = validate_bucket_file(path, ValidateDepth::Structural)?;
        if !report.errors.is_empty() {
            return Err(
                format!("{} failed validation: {:?}", path.display(), report.errors).into(),
            );
        }
    }

    let density_request = FlashDensityRequest::new(
        min_time,
        max_time.saturating_add(1),
        DensityBounds::new(-60.0, 70.0, -80.0, 80.0),
        160,
        130,
    );
    let density = flash_density(&args.store_root, "mtg-li", &density_request)?;
    let density_path = li_dir.join("mtg_li_density.json");
    write_json(&density_path, &density)?;
    let max_cell = density.max_cell();
    Ok(format!(
        "downloaded {} bytes, unpacked {} NetCDF, decoded {} flash(es), wrote {} bucket(s){}; density flashes {} max {} at {:?}; {}",
        downloaded.bytes,
        unpacked.manifest.netcdf_count,
        records.len(),
        affected.len(),
        if already_seen {
            " (already seen, no duplicate insert)"
        } else {
            ""
        },
        density.flash_count,
        max_cell.map(|cell| cell.count).unwrap_or(0),
        max_cell.map(|cell| (cell.lat_center, cell.lon_center)),
        density_path.display()
    ))
}

fn check_mtg_fci_download(agent: &ureq::Agent, args: &Args) -> Result<String, Box<dyn Error>> {
    let products = newest_mtg_products(agent, MtgCollection::FciL1cNormal, args.mtg_minutes, 1)?;
    let product = products
        .products
        .first()
        .ok_or("no MTG FCI L1c products found")?;
    let token = request_eumetsat_token(agent, args.validity_secs)?;
    let fci_dir = args.out_dir.join("mtg_fci");
    let downloaded = download_product(
        agent,
        MtgCollection::FciL1cNormal,
        &product.id,
        &token,
        &fci_dir,
    )?;
    let unpacked = unpack_package(&downloaded.path, &fci_dir.join("unpacked"), true)?;
    let mut fci_paths = unpacked
        .extracted
        .iter()
        .filter(|entry| {
            entry.name.contains("CHK-BODY")
                && entry.name.ends_with(".nc")
                && entry.name.contains("FCI-1C")
        })
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    fci_paths.sort();
    let decoded_detail = if fci_paths.is_empty() {
        "no FCI body NetCDF member decoded".to_string()
    } else {
        check_mtg_fci_decode_paths(&fci_paths, args)?
    };
    Ok(format!(
        "downloaded {} bytes, unpacked {} NetCDF member(s) to {}; {}",
        downloaded.bytes,
        unpacked.manifest.netcdf_count,
        unpacked.out_dir.display(),
        decoded_detail
    ))
}

fn check_mtg_fci_decode_paths(paths: &[PathBuf], args: &Args) -> Result<String, Box<dyn Error>> {
    let channel = FciChannel::parse(&args.fci_channel).ok_or_else(|| {
        format!(
            "unknown FCI channel '{}' (choices: {}; or c01..c16)",
            args.fci_channel,
            FciChannel::choices()
        )
    })?;
    let mode = FciValueMode::parse(&args.fci_value).ok_or_else(|| {
        format!(
            "unknown FCI value '{}' (choices: brightness-temp, bt, radiance, reflectance, count)",
            args.fci_value
        )
    })?;
    let field = assemble_fci_chunks(paths, channel, mode, args.fci_downsample.max(1))?;
    let stats = finite_stats(&field.values);
    let frame = write_satellite_grid_frame(
        &args.store_root,
        &field,
        Utc::now().timestamp().max(0) as u64,
    )?;
    let fci_dir = args.out_dir.join("mtg_fci");
    fs::create_dir_all(&fci_dir)?;
    let png_path = fci_dir.join(format!(
        "{}_{}_t{:04}_{}.png",
        frame.model, frame.run, frame.hhmm, frame.variable
    ));
    let png = export_frame_png(
        &args.store_root,
        &frame.model,
        &frame.run,
        frame.hhmm,
        &png_path,
    )?;
    Ok(format!(
        "{} chunk(s), {} {} wrote {}/{}/t{:04}.rws {}x{} finite {}/{} min {:.2} max {:.2}; png {}",
        paths.len(),
        channel.name,
        mode.slug(),
        frame.model,
        frame.run,
        frame.hhmm,
        field.scene.fixed_grid.nx,
        field.scene.fixed_grid.ny,
        stats.count,
        field.values.len(),
        stats.min,
        stats.max,
        png.display()
    ))
}

fn newest_mtg_products(
    agent: &ureq::Agent,
    collection: MtgCollection,
    minutes: i64,
    count: usize,
) -> Result<rw_sat::mtg::MtgSearchResult, Box<dyn Error>> {
    let end = Utc::now();
    let start = end - chrono::Duration::minutes(minutes.max(1));
    search_products(agent, &MtgSearchRequest::new(collection, start, end, count))
}

fn request_eumetsat_token(
    agent: &ureq::Agent,
    validity_secs: u64,
) -> Result<rw_sat::mtg::EumetsatAccessToken, Box<dyn Error>> {
    let credentials = EumetsatCredentials::from_env()?;
    request_access_token(agent, &credentials, validity_secs)
}

struct FiniteStats {
    count: usize,
    min: f32,
    max: f32,
}

fn finite_stats(values: &[f32]) -> FiniteStats {
    let mut count = 0_usize;
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &value in values {
        if value.is_finite() {
            count += 1;
            min = min.min(value);
            max = max.max(value);
        }
    }
    if count == 0 {
        min = f32::NAN;
        max = f32::NAN;
    }
    FiniteStats { count, min, max }
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), Box<dyn Error>> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}

//! rw_sat — GOES ABI live ingest CLI.
//!
//! - `latest`: fetch the newest available scan for the requested bands from
//!   the live bucket, ingest into the rolling store, export palette PNGs.
//! - `follow`: poll the bucket continuously (jitter + backoff + dedup),
//!   ingesting frames as they land, with rolling-window eviction.
//! - `export`: re-export a stored frame as a PNG.

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
use rw_sat::follow::{FollowConfig, fetch_and_ingest, follow, poll_prefixes};
use rw_sat::goes::{GoesSatellite, parse_goes_abi_filename};
use rw_sat::s3::{
    S3Object, Sector, abi_filename_product_matches_request, bucket_for_satellite, build_agent,
    list_s3_objects, object_filename, object_url,
};
use rw_sat::store::downsample_field;
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
        } => run_latest(&source, &png_dir, composite.as_deref(), composite_downsample),
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
    };
    if let Err(err) = result {
        eprintln!("rw_sat: {err}");
        std::process::exit(1);
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
        let objects = newest_band_objects(&agent, &bucket, sector, &satellite, source.mode, band, 3)?;
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
        let path = export_frame_png(&source.store, &frame.model, &frame.run, frame.hhmm, &png_path)?;
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
        for object in
            newest_band_objects(agent, bucket, sector, satellite, source.mode, band, 3)?
        {
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

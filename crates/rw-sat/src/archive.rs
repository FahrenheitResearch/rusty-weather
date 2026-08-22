//! Native GOES source archive used by desktop product playback and rw-server
//! windowed tile rendering.
//!
//! `.rws` frames remain compact previews.  The calibrated NetCDF object is
//! retained separately so Full Disk and 0.5 km visible data can be sampled at
//! native resolution without forcing hundreds of millions of cells through a
//! whole-grid store allocation.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use rw_store::atomic::{atomic_write_bytes, atomic_write_with};
use rw_store::lock::RunLock;
use serde::{Deserialize, Serialize};

use crate::abi::GoesAbiScene;
use crate::product::GoesAbiProduct;
use crate::store::sector_slug;

pub const NATIVE_SOURCE_ARCHIVE_DIR: &str = ".rw-satellite-sources";
pub const NATIVE_FRAME_SCHEMA: &str = "rw-sat.native-frame.v1";
const FRAME_MANIFEST: &str = "frame.json";
const ARCHIVE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChannelSource {
    pub channel: u8,
    pub object_key: String,
    pub relative_path: String,
    pub byte_size: u64,
    pub scan_start_unix: i64,
    pub scan_end_unix: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeSatelliteFrame {
    pub schema: String,
    pub platform: String,
    pub sector: String,
    pub frame_id: String,
    pub scan_start_unix: i64,
    pub scan_end_unix: i64,
    pub channels: BTreeMap<u8, NativeChannelSource>,
}

impl NativeSatelliteFrame {
    pub fn is_complete_for(&self, product: GoesAbiProduct) -> bool {
        product
            .required_channels()
            .iter()
            .all(|channel| self.channels.contains_key(channel))
    }

    pub fn channel_path(&self, store_root: &Path, channel: u8) -> io::Result<PathBuf> {
        let source = self.channels.get(&channel).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("native frame {} has no ABI C{channel:02}", self.frame_id),
            )
        })?;
        contained_regular_file(store_root, &source.relative_path)
    }
}

pub fn native_archive_root(store_root: &Path) -> PathBuf {
    store_root.join(NATIVE_SOURCE_ARCHIVE_DIR)
}

pub fn archive_goes_source(
    store_root: &Path,
    source_path: &Path,
    scene: &GoesAbiScene,
    object_key: &str,
) -> io::Result<NativeSatelliteFrame> {
    let channel = scene.channel.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "cannot archive a GOES source without a channel",
        )
    })?;
    if !(1..=16).contains(&channel) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("ABI channel is outside 1..=16: {channel}"),
        ));
    }
    let platform = scene.satellite.as_str().to_ascii_lowercase();
    let sector = sector_slug(&scene.sector);
    let frame_id = frame_id(scene.start_time_utc);
    let day = &frame_id[..8];
    let frame_dir = native_archive_root(store_root)
        .join(&platform)
        .join(&sector)
        .join(day)
        .join(&frame_id);
    fs::create_dir_all(&frame_dir)?;
    let _lock = RunLock::acquire(&frame_dir, ARCHIVE_LOCK_TIMEOUT)
        .map_err(|error| io::Error::other(error.to_string()))?;

    let target = frame_dir.join(format!("c{channel:02}.nc"));
    let source_size = fs::metadata(source_path)?.len();
    if !target
        .metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.len() == source_size)
    {
        let mut source_file = fs::File::open(source_path)?;
        atomic_write_with(&target, |writer| {
            io::copy(&mut source_file, writer)?;
            Ok(())
        })
        .map_err(|error| io::Error::other(error.to_string()))?;
    }

    let manifest_path = frame_dir.join(FRAME_MANIFEST);
    let mut manifest = if manifest_path.is_file() {
        load_manifest(&manifest_path)?
    } else {
        NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.to_string(),
            platform: platform.clone(),
            sector: sector.clone(),
            frame_id: frame_id.clone(),
            scan_start_unix: scene.start_time_utc.timestamp(),
            scan_end_unix: scene.end_time_utc.timestamp(),
            channels: BTreeMap::new(),
        }
    };
    if manifest.schema != NATIVE_FRAME_SCHEMA
        || manifest.platform != platform
        || manifest.sector != sector
        || manifest.frame_id != frame_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("native frame identity mismatch at {}", manifest_path.display()),
        ));
    }
    let relative_path = target
        .strip_prefix(store_root)
        .map_err(|_| io::Error::other("native archive target escaped the store root"))?
        .to_string_lossy()
        .replace('\\', "/");
    manifest.scan_start_unix = manifest
        .scan_start_unix
        .min(scene.start_time_utc.timestamp());
    manifest.scan_end_unix = manifest.scan_end_unix.max(scene.end_time_utc.timestamp());
    manifest.channels.insert(
        channel,
        NativeChannelSource {
            channel,
            object_key: object_key.to_string(),
            relative_path,
            byte_size: source_size,
            scan_start_unix: scene.start_time_utc.timestamp(),
            scan_end_unix: scene.end_time_utc.timestamp(),
        },
    );
    save_manifest(&manifest_path, &manifest)?;
    Ok(manifest)
}

pub fn list_native_frames(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: GoesAbiProduct,
    limit: usize,
) -> io::Result<Vec<NativeSatelliteFrame>> {
    let platform = normalize_component(platform)?;
    let sector = normalize_component(sector)?;
    let root = native_archive_root(store_root).join(platform).join(sector);
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let mut manifests = Vec::new();
    let mut days = fs::read_dir(&root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .collect::<Vec<_>>();
    days.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
    let requested = limit.clamp(1, 2_000);
    for day in days {
        let mut frames = fs::read_dir(day.path())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .collect::<Vec<_>>();
        frames.sort_by_key(|entry| std::cmp::Reverse(entry.file_name()));
        for frame in frames {
            let path = frame.path().join(FRAME_MANIFEST);
            let Ok(manifest) = load_manifest(&path) else {
                continue;
            };
            if manifest.is_complete_for(product) {
                manifests.push(manifest);
                if manifests.len() >= requested {
                    return Ok(manifests);
                }
            }
        }
    }
    Ok(manifests)
}

pub fn resolve_native_frame(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: GoesAbiProduct,
    frame: &str,
) -> io::Result<NativeSatelliteFrame> {
    if frame.eq_ignore_ascii_case("latest") {
        return list_native_frames(store_root, platform, sector, product, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no complete satellite frame"));
    }
    if !valid_frame_id(frame) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "satellite frame id must be YYYYMMDDTHHMM",
        ));
    }
    let platform = normalize_component(platform)?;
    let sector = normalize_component(sector)?;
    let path = native_archive_root(store_root)
        .join(platform)
        .join(sector)
        .join(&frame[..8])
        .join(frame)
        .join(FRAME_MANIFEST);
    let manifest = load_manifest(&path)?;
    if !manifest.is_complete_for(product) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("satellite frame {frame} is incomplete for {}", product.slug()),
        ));
    }
    Ok(manifest)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NativeArchivePruneReport {
    pub removed_frames: usize,
    pub removed_bytes: u64,
}

pub fn prune_native_archive(
    store_root: &Path,
    platform: &str,
    sector: &str,
    now: DateTime<Utc>,
    max_age_minutes: Option<u32>,
    max_bytes: Option<u64>,
) -> io::Result<NativeArchivePruneReport> {
    let platform = normalize_component(platform)?;
    let sector = normalize_component(sector)?;
    let root = native_archive_root(store_root).join(platform).join(sector);
    if !root.is_dir() {
        return Ok(NativeArchivePruneReport::default());
    }
    let mut frames = Vec::new();
    for day in fs::read_dir(&root)?.filter_map(Result::ok) {
        if !day.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        for frame in fs::read_dir(day.path())?.filter_map(Result::ok) {
            if !frame.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let manifest_path = frame.path().join(FRAME_MANIFEST);
            let Ok(manifest) = load_manifest(&manifest_path) else {
                continue;
            };
            let bytes = manifest
                .channels
                .values()
                .map(|source| source.byte_size)
                .sum::<u64>();
            frames.push((manifest.scan_start_unix, bytes, frame.path()));
        }
    }
    frames.sort_by_key(|(valid, _, _)| *valid);
    let cutoff = max_age_minutes.map(|minutes| now.timestamp() - i64::from(minutes) * 60);
    let mut remove = vec![false; frames.len()];
    for (index, (valid, _, _)) in frames.iter().enumerate() {
        if cutoff.is_some_and(|cutoff| *valid < cutoff) {
            remove[index] = true;
        }
    }
    if let Some(max_bytes) = max_bytes {
        let mut retained = frames
            .iter()
            .enumerate()
            .filter(|(index, _)| !remove[*index])
            .map(|(_, (_, bytes, _))| *bytes)
            .sum::<u64>();
        for (index, (_, bytes, _)) in frames.iter().enumerate() {
            if retained <= max_bytes {
                break;
            }
            if !remove[index] {
                remove[index] = true;
                retained = retained.saturating_sub(*bytes);
            }
        }
    }
    let mut report = NativeArchivePruneReport::default();
    for ((_, bytes, path), remove) in frames.into_iter().zip(remove) {
        if !remove {
            continue;
        }
        fs::remove_dir_all(&path)?;
        report.removed_frames += 1;
        report.removed_bytes = report.removed_bytes.saturating_add(bytes);
        if let Some(day) = path.parent()
            && day.read_dir().is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(day);
        }
    }
    Ok(report)
}

pub fn automatic_preview_stride(nx: usize, ny: usize, maximum_cells: usize) -> usize {
    let cells = nx.saturating_mul(ny);
    if maximum_cells == 0 || cells <= maximum_cells {
        return 1;
    }
    let ratio = cells.div_ceil(maximum_cells) as f64;
    ratio.sqrt().ceil().max(1.0) as usize
}

fn frame_id(time: DateTime<Utc>) -> String {
    time.format("%Y%m%dT%H%M").to_string()
}

fn valid_frame_id(value: &str) -> bool {
    value.len() == 13
        && value.as_bytes()[8] == b'T'
        && value[..8].bytes().all(|byte| byte.is_ascii_digit())
        && value[9..].bytes().all(|byte| byte.is_ascii_digit())
        && Utc
            .datetime_from_str(value, "%Y%m%dT%H%M")
            .is_ok()
}

fn normalize_component(value: &str) -> io::Result<String> {
    let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    if normalized.is_empty()
        || !normalized
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid satellite archive component",
        ));
    }
    Ok(normalized)
}

fn contained_regular_file(store_root: &Path, relative: &str) -> io::Result<PathBuf> {
    let root = fs::canonicalize(store_root)?;
    let requested = store_root.join(relative);
    let path = fs::canonicalize(&requested)?;
    if !path.starts_with(&root) || !fs::symlink_metadata(&path)?.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "satellite source path escapes the configured store",
        ));
    }
    Ok(path)
}

fn load_manifest(path: &Path) -> io::Result<NativeSatelliteFrame> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid native satellite frame manifest",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)?;
    let manifest: NativeSatelliteFrame = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if manifest.schema != NATIVE_FRAME_SCHEMA
        || !valid_frame_id(&manifest.frame_id)
        || manifest.channels.len() > 16
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid native satellite frame manifest schema",
        ));
    }
    Ok(manifest)
}

fn save_manifest(path: &Path, manifest: &NativeSatelliteFrame) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    atomic_write_bytes(path, &bytes).map_err(|error| io::Error::other(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_stride_hides_internal_fraction_choices() {
        assert_eq!(automatic_preview_stride(2_500, 1_500, 8_000_000), 1);
        assert_eq!(automatic_preview_stride(10_000, 6_000, 8_000_000), 3);
        assert!(automatic_preview_stride(21_696, 21_696, 8_000_000) >= 8);
    }

    #[test]
    fn frame_ids_are_minute_exact() {
        let time = Utc.with_ymd_and_hms(2026, 8, 22, 19, 41, 37).unwrap();
        assert_eq!(frame_id(time), "20260822T1941");
        assert!(valid_frame_id("20260822T1941"));
        assert!(!valid_frame_id("latest"));
    }
}

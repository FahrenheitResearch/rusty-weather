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

#[cfg(test)]
use chrono::TimeZone;
use chrono::{DateTime, Utc};
use rw_store::atomic::{atomic_write_bytes, atomic_write_with};
use rw_store::lock::RunLock;
use serde::{Deserialize, Serialize};

use crate::abi::GoesAbiScene;
use crate::cloud::CloudProduct;
use crate::product::GoesAbiProduct;
use crate::store::sector_slug;

pub const NATIVE_SOURCE_ARCHIVE_DIR: &str = ".rw-satellite-sources";
pub const NATIVE_FRAME_SCHEMA: &str = "rw-sat.native-frame.v1";
const FRAME_MANIFEST: &str = "frame.json";
const ARCHIVE_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
/// ABI Full Disk channels that belong to one provider scan share the exact
/// scan-start timestamp, but NOAA's channel files do not always report an
/// identical scan end.  For example, the operational GOES-18 M6 C01/C02/C03
/// files ending at `...49525` pair with C13 ending at `...49536` (1.1 s
/// later).  Timestamps in the native manifest are whole seconds, so a
/// two-second end tolerance admits that real scan without allowing adjacent
/// ten-minute Full Disk scans to be mixed.
pub const ABI_COMPONENT_END_TOLERANCE_SECONDS: i64 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeChannelSource {
    pub channel: u8,
    pub object_key: String,
    pub relative_path: String,
    pub byte_size: u64,
    /// BLAKE3 of the exact archived NetCDF bytes. Older v1 manifests omit
    /// this and are upgraded under the frame lock on first resolved use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_blake3: Option<String>,
    pub scan_start_unix: i64,
    pub scan_end_unix: i64,
}

/// One archived channel-less ABI L2 granule (the cloud suite).
///
/// The ABI channel imagery of a frame is keyed by channel number; an L2
/// retrieval has no channel, so it is keyed by its store slug
/// ([`CloudProduct::slug`]) instead and lives beside the channels in the
/// same exact-frame directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeL2ProductSource {
    /// [`CloudProduct::slug`]; also this entry's key in the manifest.
    pub product: String,
    /// The NOAA filename product token the granule was published under,
    /// e.g. `ABI-L2-ACHAC`. Sector-bearing, so the archived bytes can be
    /// checked against the frame they were filed under.
    pub abi_product: String,
    pub object_key: String,
    pub relative_path: String,
    pub byte_size: u64,
    /// BLAKE3 of the exact archived NetCDF bytes. Unlike the channel
    /// sources there is no legacy digest-free form to upgrade: an L2
    /// entry has carried its digest since it first existed.
    pub content_blake3: String,
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
    /// Channel-less L2 granules of the same frame, keyed by
    /// [`CloudProduct::slug`]. Skipped when empty, so a channel-only
    /// manifest serializes exactly as it always has.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub l2_products: BTreeMap<String, NativeL2ProductSource>,
}

impl NativeSatelliteFrame {
    /// Whether this frame contains every source required by `product`.
    ///
    /// A minute-granular `frame_id` is not enough to prove that multiple
    /// channels belong to the same scan. For a multichannel product, every
    /// required source must have the provider-identical start time and an end
    /// time within the documented ABI component tolerance. A raw
    /// single-channel product remains complete whenever its one requested
    /// source is present.
    pub fn is_complete_for(&self, product: GoesAbiProduct) -> bool {
        let Some((&reference_channel, remaining_channels)) =
            product.required_channels().split_first()
        else {
            return false;
        };
        let Some(reference) = self.channels.get(&reference_channel) else {
            return false;
        };
        let ends = remaining_channels.iter().try_fold(
            (reference.scan_end_unix, reference.scan_end_unix),
            |(earliest, latest), channel| {
                let source = self.channels.get(channel)?;
                (source.scan_start_unix == reference.scan_start_unix).then_some((
                    earliest.min(source.scan_end_unix),
                    latest.max(source.scan_end_unix),
                ))
            },
        );
        ends.is_some_and(|(earliest, latest)| {
            latest.saturating_sub(earliest) <= ABI_COMPONENT_END_TOLERANCE_SECONDS
        })
    }

    /// Whether every requested L2 cloud product is archived in this frame.
    /// An empty request is not "complete" — it would match every frame.
    pub fn is_complete_for_cloud(&self, products: &[CloudProduct]) -> bool {
        !products.is_empty()
            && products
                .iter()
                .all(|product| self.l2_products.contains_key(product.slug()))
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

    pub fn l2_product_source(&self, product: CloudProduct) -> io::Result<&NativeL2ProductSource> {
        self.l2_products.get(product.slug()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "native frame {} has no {}",
                    self.frame_id,
                    product.catalog_id()
                ),
            )
        })
    }

    pub fn l2_product_path(&self, store_root: &Path, product: CloudProduct) -> io::Result<PathBuf> {
        let source = self.l2_product_source(product)?;
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

    let source_size = fs::metadata(source_path)?.len();
    let content_blake3 = blake3_file(source_path)?;
    let target = content_addressed_channel_path(&frame_dir, channel, &content_blake3);
    install_content_addressed_source(source_path, &target, source_size, &content_blake3)?;

    let manifest_path = frame_dir.join(FRAME_MANIFEST);
    let mut manifest = open_frame_manifest(&manifest_path, &platform, &sector, &frame_id, scene)?;
    let relative_path = store_relative_path(store_root, &target)?;
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
            content_blake3: Some(content_blake3),
            scan_start_unix: scene.start_time_utc.timestamp(),
            scan_end_unix: scene.end_time_utc.timestamp(),
        },
    );
    save_manifest(&manifest_path, &manifest)?;
    Ok(manifest)
}

/// Archive one channel-less ABI L2 cloud granule into the exact frame its
/// scan start belongs to, alongside that frame's channel imagery.
///
/// The granule's own filename product token decides which cloud product
/// this is, so a mislabelled call cannot file a COD granule as ACHA. The
/// bytes are content-addressed exactly as channel sources are, and the
/// frame identity (platform, sector, minute) is enforced against any
/// manifest already there.
pub fn archive_goes_l2_source(
    store_root: &Path,
    source_path: &Path,
    scene: &GoesAbiScene,
    object_key: &str,
) -> io::Result<NativeSatelliteFrame> {
    if scene.channel.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "channel imagery belongs in archive_goes_source, not the L2 archive",
        ));
    }
    let (product, _sector) = CloudProduct::from_abi_product(&scene.product).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an ABI L2 cloud product", scene.product),
        )
    })?;
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

    let source_size = fs::metadata(source_path)?.len();
    let content_blake3 = blake3_file(source_path)?;
    let target = content_addressed_l2_path(&frame_dir, product, &content_blake3);
    install_content_addressed_source(source_path, &target, source_size, &content_blake3)?;

    let manifest_path = frame_dir.join(FRAME_MANIFEST);
    let mut manifest = open_frame_manifest(&manifest_path, &platform, &sector, &frame_id, scene)?;
    let relative_path = store_relative_path(store_root, &target)?;
    manifest.scan_start_unix = manifest
        .scan_start_unix
        .min(scene.start_time_utc.timestamp());
    manifest.scan_end_unix = manifest.scan_end_unix.max(scene.end_time_utc.timestamp());
    manifest.l2_products.insert(
        product.slug().to_string(),
        NativeL2ProductSource {
            product: product.slug().to_string(),
            abi_product: scene.product.trim().to_ascii_uppercase(),
            object_key: object_key.to_string(),
            relative_path,
            byte_size: source_size,
            content_blake3,
            scan_start_unix: scene.start_time_utc.timestamp(),
            scan_end_unix: scene.end_time_utc.timestamp(),
        },
    );
    save_manifest(&manifest_path, &manifest)?;
    Ok(manifest)
}

fn open_frame_manifest(
    manifest_path: &Path,
    platform: &str,
    sector: &str,
    frame_id: &str,
    scene: &GoesAbiScene,
) -> io::Result<NativeSatelliteFrame> {
    let manifest = if manifest_path.is_file() {
        load_manifest(manifest_path)?
    } else {
        NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.to_string(),
            platform: platform.to_string(),
            sector: sector.to_string(),
            frame_id: frame_id.to_string(),
            scan_start_unix: scene.start_time_utc.timestamp(),
            scan_end_unix: scene.end_time_utc.timestamp(),
            channels: BTreeMap::new(),
            l2_products: BTreeMap::new(),
        }
    };
    if manifest.schema != NATIVE_FRAME_SCHEMA
        || manifest.platform != platform
        || manifest.sector != sector
        || manifest.frame_id != frame_id
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "native frame identity mismatch at {}",
                manifest_path.display()
            ),
        ));
    }
    Ok(manifest)
}

fn store_relative_path(store_root: &Path, target: &Path) -> io::Result<String> {
    Ok(target
        .strip_prefix(store_root)
        .map_err(|_| io::Error::other("native archive target escaped the store root"))?
        .to_string_lossy()
        .replace('\\', "/"))
}

pub fn list_native_frames(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: GoesAbiProduct,
    limit: usize,
) -> io::Result<Vec<NativeSatelliteFrame>> {
    list_frames_matching(store_root, platform, sector, limit, |manifest| {
        manifest.is_complete_for(product)
    })
}

/// Newest-first traversal of the retained frames of one platform/sector,
/// returning those `accept` admits. Both the channel and the L2 listings
/// walk the archive this way, so they retire frames in the same order.
fn list_frames_matching(
    store_root: &Path,
    platform: &str,
    sector: &str,
    limit: usize,
    accept: impl Fn(&NativeSatelliteFrame) -> bool,
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
    let requested = requested_frame_limit(limit);
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
            if accept(&manifest) {
                manifests.push(manifest);
                if manifests.len() >= requested {
                    return Ok(manifests);
                }
            }
        }
    }
    Ok(manifests)
}

fn requested_frame_limit(limit: usize) -> usize {
    limit.max(1)
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
    let path = frame_manifest_path(store_root, &platform, &sector, frame);
    let manifest = load_manifest(&path)?;
    if !manifest.is_complete_for(product) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "satellite frame {frame} is incomplete for {}",
                product.slug()
            ),
        ));
    }
    Ok(manifest)
}

/// Exact source identity for a resolved product. The human-readable frame ID
/// is minute-granular; this digest binds only the product's required channels
/// and therefore changes whenever any byte or identity input used to render
/// that product changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedNativeSatelliteFrame {
    pub frame: NativeSatelliteFrame,
    pub source_revision: String,
}

pub fn resolve_native_frame_with_revision(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: GoesAbiProduct,
    frame: &str,
) -> io::Result<ResolvedNativeSatelliteFrame> {
    let mut manifest = resolve_native_frame(store_root, platform, sector, product, frame)?;
    if !required_sources_are_content_addressed(store_root, &manifest, product)? {
        manifest = upgrade_required_sources(store_root, &manifest, product)?;
    }
    let source_revision = native_frame_product_revision(&manifest, product)?;
    Ok(ResolvedNativeSatelliteFrame {
        frame: manifest,
        source_revision,
    })
}

pub fn native_frame_product_revision(
    manifest: &NativeSatelliteFrame,
    product: GoesAbiProduct,
) -> io::Result<String> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"rw-sat:native-product-source-revision:v1\0");
    update_hash_string(&mut hash, &manifest.platform);
    update_hash_string(&mut hash, &manifest.sector);
    update_hash_string(&mut hash, &manifest.frame_id);
    update_hash_string(&mut hash, &product.slug());
    for &channel in product.required_channels() {
        let source = manifest.channels.get(&channel).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "native frame {} has no ABI C{channel:02}",
                    manifest.frame_id
                ),
            )
        })?;
        let digest = source
            .content_blake3
            .as_deref()
            .filter(|value| valid_blake3(value))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "native frame {} ABI C{channel:02} has no validated content digest",
                        manifest.frame_id
                    ),
                )
            })?;
        hash.update(&[channel]);
        update_hash_string(&mut hash, digest);
        update_hash_string(&mut hash, &source.object_key);
        hash.update(&source.byte_size.to_le_bytes());
        hash.update(&source.scan_start_unix.to_le_bytes());
        hash.update(&source.scan_end_unix.to_le_bytes());
    }
    Ok(hash.finalize().to_hex().to_string())
}

/// List retained frames that hold every requested L2 cloud product,
/// newest first. The channel twin of this is [`list_native_frames`]; a
/// frame satisfies both independently.
pub fn list_native_cloud_frames(
    store_root: &Path,
    platform: &str,
    sector: &str,
    products: &[CloudProduct],
    limit: usize,
) -> io::Result<Vec<NativeSatelliteFrame>> {
    list_frames_matching(store_root, platform, sector, limit, |manifest| {
        manifest.is_complete_for_cloud(products)
    })
}

/// Resolve one frame that holds every requested L2 cloud product, by
/// exact `YYYYMMDDTHHMM` id or the `latest` alias.
pub fn resolve_native_cloud_frame(
    store_root: &Path,
    platform: &str,
    sector: &str,
    products: &[CloudProduct],
    frame: &str,
) -> io::Result<NativeSatelliteFrame> {
    if products.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no L2 cloud product requested",
        ));
    }
    if frame.eq_ignore_ascii_case("latest") {
        return list_native_cloud_frames(store_root, platform, sector, products, 1)?
            .into_iter()
            .next()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "no retained satellite frame holds {}",
                        products
                            .iter()
                            .map(|product| product.catalog_id())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                )
            });
    }
    if !valid_frame_id(frame) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "satellite frame id must be YYYYMMDDTHHMM",
        ));
    }
    let platform = normalize_component(platform)?;
    let sector = normalize_component(sector)?;
    let manifest = load_manifest(&frame_manifest_path(store_root, &platform, &sector, frame))?;
    if !manifest.is_complete_for_cloud(products) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "satellite frame {frame} is missing {}",
                missing_cloud_products(&manifest, products).join(", ")
            ),
        ));
    }
    Ok(manifest)
}

/// Exact source identity for a set of L2 cloud products in one frame.
/// The minute-granular frame id is not enough on its own: a republished
/// granule keeps the minute and changes the bytes, and this digest
/// changes with it.
pub fn native_frame_cloud_revision(
    manifest: &NativeSatelliteFrame,
    products: &[CloudProduct],
) -> io::Result<String> {
    if products.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no L2 cloud product requested",
        ));
    }
    let mut hash = blake3::Hasher::new();
    hash.update(b"rw-sat:native-l2-cloud-source-revision:v1\0");
    update_hash_string(&mut hash, &manifest.platform);
    update_hash_string(&mut hash, &manifest.sector);
    update_hash_string(&mut hash, &manifest.frame_id);
    let mut ordered = products.to_vec();
    ordered.sort_unstable();
    ordered.dedup();
    for product in ordered {
        let source = manifest.l2_product_source(product)?;
        if !valid_blake3(&source.content_blake3) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "native frame {} {} has no validated content digest",
                    manifest.frame_id,
                    product.catalog_id()
                ),
            ));
        }
        update_hash_string(&mut hash, product.slug());
        update_hash_string(&mut hash, &source.abi_product);
        update_hash_string(&mut hash, &source.content_blake3);
        update_hash_string(&mut hash, &source.object_key);
        hash.update(&source.byte_size.to_le_bytes());
        hash.update(&source.scan_start_unix.to_le_bytes());
        hash.update(&source.scan_end_unix.to_le_bytes());
    }
    Ok(hash.finalize().to_hex().to_string())
}

/// [`resolve_native_cloud_frame`] plus its exact source revision.
pub fn resolve_native_cloud_frame_with_revision(
    store_root: &Path,
    platform: &str,
    sector: &str,
    products: &[CloudProduct],
    frame: &str,
) -> io::Result<ResolvedNativeSatelliteFrame> {
    let frame = resolve_native_cloud_frame(store_root, platform, sector, products, frame)?;
    let source_revision = native_frame_cloud_revision(&frame, products)?;
    Ok(ResolvedNativeSatelliteFrame {
        frame,
        source_revision,
    })
}

fn missing_cloud_products(
    manifest: &NativeSatelliteFrame,
    products: &[CloudProduct],
) -> Vec<String> {
    products
        .iter()
        .filter(|product| !manifest.l2_products.contains_key(product.slug()))
        .map(|product| product.catalog_id().to_string())
        .collect()
}

fn upgrade_required_sources(
    store_root: &Path,
    observed: &NativeSatelliteFrame,
    product: GoesAbiProduct,
) -> io::Result<NativeSatelliteFrame> {
    let platform = normalize_component(&observed.platform)?;
    let sector = normalize_component(&observed.sector)?;
    let manifest_path = frame_manifest_path(store_root, &platform, &sector, &observed.frame_id);
    let frame_dir = manifest_path
        .parent()
        .ok_or_else(|| io::Error::other("native frame manifest has no parent"))?;
    let _lock = RunLock::acquire(frame_dir, ARCHIVE_LOCK_TIMEOUT)
        .map_err(|error| io::Error::other(error.to_string()))?;
    let mut manifest = load_manifest(&manifest_path)?;
    if !manifest.is_complete_for(product) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "satellite frame {} is incomplete for {}",
                manifest.frame_id,
                product.slug()
            ),
        ));
    }

    let mut changed = false;
    for &channel in product.required_channels() {
        manifest.channels.get(&channel).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "native frame {} has no ABI C{channel:02}",
                    manifest.frame_id
                ),
            )
        })?;
        let source_path = manifest.channel_path(store_root, channel)?;
        let metadata = fs::metadata(&source_path)?;
        let content_blake3 = blake3_file(&source_path)?;
        let target = content_addressed_channel_path(frame_dir, channel, &content_blake3);
        install_content_addressed_source(&source_path, &target, metadata.len(), &content_blake3)?;
        let relative_path = target
            .strip_prefix(store_root)
            .map_err(|_| io::Error::other("native archive target escaped the store root"))?
            .to_string_lossy()
            .replace('\\', "/");
        let updated = manifest.channels.get_mut(&channel).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "native channel disappeared during upgrade",
            )
        })?;
        if updated.relative_path != relative_path
            || updated.byte_size != metadata.len()
            || updated.content_blake3.as_deref() != Some(content_blake3.as_str())
        {
            updated.relative_path = relative_path;
            updated.byte_size = metadata.len();
            updated.content_blake3 = Some(content_blake3);
            changed = true;
        }
    }
    if changed {
        save_manifest(&manifest_path, &manifest)?;
    }
    Ok(manifest)
}

fn required_sources_are_content_addressed(
    store_root: &Path,
    manifest: &NativeSatelliteFrame,
    product: GoesAbiProduct,
) -> io::Result<bool> {
    for &channel in product.required_channels() {
        let Some(source) = manifest.channels.get(&channel) else {
            return Ok(false);
        };
        let Some(digest) = source
            .content_blake3
            .as_deref()
            .filter(|value| valid_blake3(value))
        else {
            return Ok(false);
        };
        let expected_name = format!("c{channel:02}-{digest}.nc");
        if Path::new(&source.relative_path)
            .file_name()
            .and_then(|name| name.to_str())
            != Some(expected_name.as_str())
        {
            return Ok(false);
        }
        let path = manifest.channel_path(store_root, channel)?;
        if fs::metadata(path)?.len() != source.byte_size {
            return Ok(false);
        }
    }
    Ok(true)
}

fn frame_manifest_path(store_root: &Path, platform: &str, sector: &str, frame: &str) -> PathBuf {
    native_archive_root(store_root)
        .join(platform)
        .join(sector)
        .join(&frame[..8])
        .join(frame)
        .join(FRAME_MANIFEST)
}

fn content_addressed_channel_path(frame_dir: &Path, channel: u8, content_blake3: &str) -> PathBuf {
    frame_dir.join(format!("c{channel:02}-{content_blake3}.nc"))
}

fn content_addressed_l2_path(
    frame_dir: &Path,
    product: CloudProduct,
    content_blake3: &str,
) -> PathBuf {
    frame_dir.join(format!("l2-{}-{content_blake3}.nc", product.slug()))
}

fn install_content_addressed_source(
    source_path: &Path,
    target: &Path,
    expected_bytes: u64,
    expected_blake3: &str,
) -> io::Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(target)
        && metadata.file_type().is_file()
        && !metadata.file_type().is_symlink()
        && metadata.len() == expected_bytes
        && blake3_file(target).is_ok_and(|digest| digest == expected_blake3)
    {
        return Ok(());
    }
    let mut source_file = fs::File::open(source_path)?;
    let metadata = source_file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native satellite source changed while it was being archived",
        ));
    }
    atomic_write_with(target, |writer| {
        io::copy(&mut source_file, writer)?;
        Ok(())
    })
    .map_err(|error| io::Error::other(error.to_string()))?;
    if blake3_file(target)? != expected_blake3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "archived native satellite source failed its content digest",
        ));
    }
    Ok(())
}

fn blake3_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native satellite source is not a regular file",
        ));
    }
    let mut hash = blake3::Hasher::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hash.update(&buffer[..read]);
    }
    Ok(hash.finalize().to_hex().to_string())
}

fn update_hash_string(hash: &mut blake3::Hasher, value: &str) {
    hash.update(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_le_bytes());
    hash.update(value.as_bytes());
}

fn valid_blake3(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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
            // Content-addressed source revisions from the same minute remain
            // in this directory so old immutable tiles never point at replaced
            // bytes. Retention must therefore count every regular file here,
            // not only the sources referenced by the newest manifest.
            let Ok(bytes) = frame_directory_regular_bytes(&frame.path()) else {
                continue;
            };
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
            && day
                .read_dir()
                .is_ok_and(|mut entries| entries.next().is_none())
        {
            let _ = fs::remove_dir(day);
        }
    }
    Ok(report)
}

fn frame_directory_regular_bytes(path: &Path) -> io::Result<u64> {
    let mut bytes = 0_u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_file() && !file_type.is_symlink() {
            bytes = bytes.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(bytes)
}

pub fn automatic_preview_stride(nx: usize, ny: usize, maximum_cells: usize) -> usize {
    let cells = nx.saturating_mul(ny);
    if maximum_cells == 0 || cells <= maximum_cells {
        return 1;
    }
    let ratio = cells.div_ceil(maximum_cells) as f64;
    let mut step = ratio.sqrt().ceil().max(1.0) as usize;
    while nx.div_ceil(step).saturating_mul(ny.div_ceil(step)) > maximum_cells {
        step = step.saturating_add(1);
    }
    step
}

fn frame_id(time: DateTime<Utc>) -> String {
    time.format("%Y%m%dT%H%M").to_string()
}

fn valid_frame_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 13
        && bytes[8] == b'T'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..].iter().all(u8::is_ascii_digit)
        && chrono::NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M").is_ok()
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
        || manifest.l2_products.len() > CloudProduct::ALL.len()
        || manifest
            .l2_products
            .iter()
            .any(|(slug, source)| source.product != *slug || CloudProduct::parse(slug).is_none())
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
    use crate::abi::{AbiFixedGrid, AbiSector, GoesImagerProjection};
    use crate::geostationary::SweepAngleAxis;
    use crate::goes::GoesSatellite;

    #[test]
    fn preview_stride_hides_internal_fraction_choices() {
        assert_eq!(automatic_preview_stride(2_500, 1_500, 8_000_000), 1);
        assert_eq!(automatic_preview_stride(10_000, 6_000, 8_000_000), 3);
        assert_eq!(automatic_preview_stride(21_696, 21_696, 8_000_000), 8);
        assert_eq!(automatic_preview_stride(5, 5, 7), 3);
    }

    #[test]
    fn frame_ids_are_minute_exact() {
        let time = Utc.with_ymd_and_hms(2026, 8, 22, 19, 41, 37).unwrap();
        assert_eq!(frame_id(time), "20260822T1941");
        assert!(valid_frame_id("20260822T1941"));
        assert!(!valid_frame_id("latest"));
    }

    #[test]
    fn multichannel_completeness_uses_exact_start_and_provider_end_tolerance() {
        let start = Utc
            .with_ymd_and_hms(2026, 8, 26, 16, 40, 21)
            .unwrap()
            .timestamp();
        let end = start + 571;
        let mut frame = native_test_frame(&[(1, start, end), (2, start, end), (3, start, end)]);

        assert!(frame.is_complete_for(GoesAbiProduct::TrueColor));

        frame.channels.get_mut(&3).unwrap().scan_start_unix += 1;
        assert_eq!(
            DateTime::<Utc>::from_timestamp(frame.channels[&3].scan_start_unix, 0)
                .unwrap()
                .format("%Y%m%dT%H%M")
                .to_string(),
            frame.frame_id,
            "the regression must exercise a seconds mismatch hidden by the same frame minute"
        );
        assert!(!frame.is_complete_for(GoesAbiProduct::TrueColor));
        assert!(
            frame.is_complete_for(GoesAbiProduct::RawChannel(3)),
            "a raw channel does not require cross-channel timestamp agreement"
        );

        frame.channels.get_mut(&3).unwrap().scan_start_unix = start;
        frame.channels.get_mut(&3).unwrap().scan_end_unix += 1;
        assert!(
            frame.is_complete_for(GoesAbiProduct::TrueColor),
            "real ABI component channels can end about one second apart"
        );
        frame.channels.get_mut(&3).unwrap().scan_end_unix += 2;
        assert!(
            !frame.is_complete_for(GoesAbiProduct::TrueColor),
            "an end mismatch beyond the provider tolerance must fail closed"
        );
        assert!(frame.is_complete_for(GoesAbiProduct::RawChannel(3)));

        let spread_around_reference =
            native_test_frame(&[(1, start, end), (2, start, end - 2), (3, start, end + 2)]);
        assert!(
            !spread_around_reference.is_complete_for(GoesAbiProduct::TrueColor),
            "the total component-end spread must stay inside the two-second tolerance"
        );
    }

    #[test]
    fn operational_geocolor_c13_end_offset_is_one_complete_scan() {
        let start = Utc
            .with_ymd_and_hms(2026, 8, 27, 1, 40, 21)
            .unwrap()
            .timestamp();
        // NOAA GOES-18 M6 Full Disk C01/C02/C03 ended at 01:49:52.5Z
        // while C13 ended at 01:49:53.6Z for this real scan. Whole-second
        // manifests retain that as a one-second component-end difference.
        let common_end = start + 571;
        let frame = native_test_frame(&[
            (1, start, common_end),
            (2, start, common_end),
            (3, start, common_end),
            (13, start, common_end + 1),
        ]);

        assert!(frame.is_complete_for(GoesAbiProduct::GeoColor));
    }

    #[test]
    fn resolution_rejects_same_minute_mixed_scan_channels() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let start = Utc
            .with_ymd_and_hms(2026, 8, 26, 16, 40, 21)
            .unwrap()
            .timestamp();
        let end = start + 571;
        let frame = native_test_frame(&[(1, start, end), (2, start + 1, end), (3, start, end)]);
        let frame_dir = native_archive_root(&store_root)
            .join("g19")
            .join("fulldisk")
            .join("20260826")
            .join(&frame.frame_id);
        fs::create_dir_all(&frame_dir).unwrap();
        save_manifest(&frame_dir.join(FRAME_MANIFEST), &frame).unwrap();

        assert!(
            resolve_native_frame(
                &store_root,
                "g19",
                "fulldisk",
                GoesAbiProduct::RawChannel(2),
                &frame.frame_id,
            )
            .is_ok(),
            "the exact C02 source remains individually usable"
        );
        let exact_error = resolve_native_frame(
            &store_root,
            "g19",
            "fulldisk",
            GoesAbiProduct::TrueColor,
            &frame.frame_id,
        )
        .expect_err("same-minute channels from different exact scans are not one product frame");
        assert_eq!(exact_error.kind(), io::ErrorKind::NotFound);
        assert!(exact_error.to_string().contains("incomplete"));

        assert!(
            list_native_frames(&store_root, "g19", "fulldisk", GoesAbiProduct::TrueColor, 8,)
                .unwrap()
                .is_empty()
        );
        let latest_error = resolve_native_frame(
            &store_root,
            "g19",
            "fulldisk",
            GoesAbiProduct::TrueColor,
            "latest",
        )
        .expect_err("latest must not select a mixed-scan composite");
        assert_eq!(latest_error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn native_frame_listing_has_no_legacy_two_thousand_frame_ceiling() {
        assert_eq!(requested_frame_limit(0), 1);
        assert_eq!(requested_frame_limit(2_001), 2_001);
        assert_eq!(requested_frame_limit(usize::MAX), usize::MAX);
    }

    #[test]
    fn same_size_same_minute_replacement_gets_new_revision_and_preserves_old_bytes() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let first_source = directory.path().join("first.nc");
        let second_source = directory.path().join("second.nc");
        fs::write(&first_source, b"AAAA").unwrap();
        fs::write(&second_source, b"BBBB").unwrap();
        let scene = fixture_scene(&first_source);

        let first = archive_goes_source(&store_root, &first_source, &scene, "fixture/first-c13.nc")
            .unwrap();
        let first_path = first.channel_path(&store_root, 13).unwrap();
        let first_resolved = resolve_native_frame_with_revision(
            &store_root,
            "g18",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            &first.frame_id,
        )
        .unwrap();

        let second =
            archive_goes_source(&store_root, &second_source, &scene, "fixture/second-c13.nc")
                .unwrap();
        let second_path = second.channel_path(&store_root, 13).unwrap();
        let second_resolved = resolve_native_frame_with_revision(
            &store_root,
            "g18",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            &second.frame_id,
        )
        .unwrap();

        assert_ne!(first_path, second_path);
        assert_ne!(
            first_resolved.source_revision,
            second_resolved.source_revision
        );
        assert_eq!(fs::read(first_path).unwrap(), b"AAAA");
        assert_eq!(fs::read(&second_path).unwrap(), b"BBBB");

        let frame_dir = second_path.parent().unwrap();
        let actual_frame_bytes = frame_directory_regular_bytes(frame_dir).unwrap();
        assert!(actual_frame_bytes >= 8);
        let report = prune_native_archive(
            &store_root,
            "g18",
            "fulldisk",
            scene.start_time_utc + chrono::Duration::minutes(1),
            Some(0),
            None,
        )
        .unwrap();
        assert_eq!(report.removed_frames, 1);
        assert_eq!(report.removed_bytes, actual_frame_bytes);
    }

    #[test]
    fn legacy_manifest_is_content_hashed_and_upgraded_once_under_lock() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let frame_id = "20260822T1941";
        let frame_dir = native_archive_root(&store_root)
            .join("g18")
            .join("fulldisk")
            .join("20260822")
            .join(frame_id);
        fs::create_dir_all(&frame_dir).unwrap();
        let legacy_path = frame_dir.join("c13.nc");
        fs::write(&legacy_path, b"legacy bytes").unwrap();
        let manifest = NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.into(),
            platform: "g18".into(),
            sector: "fulldisk".into(),
            frame_id: frame_id.into(),
            scan_start_unix: 1_777_000_000,
            scan_end_unix: 1_777_000_600,
            channels: BTreeMap::from([(
                13,
                NativeChannelSource {
                    channel: 13,
                    object_key: "fixture/legacy-c13.nc".into(),
                    relative_path: legacy_path
                        .strip_prefix(&store_root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                    byte_size: 12,
                    content_blake3: None,
                    scan_start_unix: 1_777_000_000,
                    scan_end_unix: 1_777_000_600,
                },
            )]),
            l2_products: BTreeMap::new(),
        };
        save_manifest(&frame_dir.join(FRAME_MANIFEST), &manifest).unwrap();

        let resolved = resolve_native_frame_with_revision(
            &store_root,
            "g18",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            frame_id,
        )
        .unwrap();
        let source = resolved.frame.channels.get(&13).unwrap();
        assert!(valid_blake3(source.content_blake3.as_deref().unwrap()));
        assert!(
            source
                .relative_path
                .contains(source.content_blake3.as_deref().unwrap())
        );
        assert_eq!(
            fs::read(resolved.frame.channel_path(&store_root, 13).unwrap()).unwrap(),
            b"legacy bytes"
        );
        assert!(
            legacy_path.exists(),
            "legacy bytes remain available during migration"
        );
    }

    #[test]
    fn l2_cloud_granules_land_in_the_same_exact_frame_as_the_channel_imagery() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let channel_source = directory.path().join("c13.nc");
        let cloud_source = directory.path().join("acha.nc");
        fs::write(&channel_source, b"channel bytes").unwrap();
        fs::write(&cloud_source, b"cloud bytes").unwrap();

        let channel_scene = conus_scene(&channel_source, "ABI-L2-CMIPC", Some(13));
        let cloud_scene = conus_scene(&cloud_source, "ABI-L2-ACHAC", None);
        archive_goes_source(
            &store_root,
            &channel_source,
            &channel_scene,
            "ABI-L2-CMIPC/2026/216/18/OR_ABI-L2-CMIPC-M6C13_G19_s1_e2_c3.nc",
        )
        .unwrap();
        let manifest = archive_goes_l2_source(
            &store_root,
            &cloud_source,
            &cloud_scene,
            "ABI-L2-ACHAC/2026/216/18/OR_ABI-L2-ACHAC-M6_G19_s1_e2_c3.nc",
        )
        .unwrap();

        // One frame, both kinds of source, neither displacing the other.
        assert!(manifest.channels.contains_key(&13));
        assert!(manifest.is_complete_for_cloud(&[CloudProduct::CloudTopHeight]));
        assert!(!manifest.is_complete_for_cloud(&[CloudProduct::OpticalDepth]));
        assert!(
            !manifest.is_complete_for_cloud(&[]),
            "an empty request must never count as complete"
        );

        let source = manifest
            .l2_product_source(CloudProduct::CloudTopHeight)
            .unwrap();
        assert_eq!(source.product, "acha");
        assert_eq!(source.abi_product, "ABI-L2-ACHAC");
        assert!(valid_blake3(&source.content_blake3));
        assert!(
            source
                .relative_path
                .contains(&format!("l2-acha-{}", source.content_blake3)),
            "L2 sources are content addressed: {}",
            source.relative_path
        );
        let path = manifest
            .l2_product_path(&store_root, CloudProduct::CloudTopHeight)
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"cloud bytes");
        let channel_path = manifest.channel_path(&store_root, 13).unwrap();
        assert_eq!(
            path.parent(),
            channel_path.parent(),
            "the L2 granule and the channel imagery share one frame directory"
        );

        // The manifest survives a round trip through disk unchanged.
        let reloaded = resolve_native_cloud_frame(
            &store_root,
            "g19",
            "conus",
            &[CloudProduct::CloudTopHeight],
            &manifest.frame_id,
        )
        .unwrap();
        assert_eq!(reloaded, manifest);
        assert_eq!(
            list_native_cloud_frames(
                &store_root,
                "g19",
                "conus",
                &[CloudProduct::CloudTopHeight],
                8
            )
            .unwrap(),
            vec![manifest]
        );
    }

    #[test]
    fn republished_cloud_bytes_change_the_cloud_source_revision() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let first = directory.path().join("first.nc");
        let second = directory.path().join("second.nc");
        // Same length, same minute, different bytes: only a content
        // digest can tell these apart.
        fs::write(&first, b"AAAA").unwrap();
        fs::write(&second, b"BBBB").unwrap();

        let key = "ABI-L2-CODC/2026/216/18/OR_ABI-L2-CODC-M6_G19_s1_e2_c3.nc";
        let scene = conus_scene(&first, "ABI-L2-CODC", None);
        let before = archive_goes_l2_source(&store_root, &first, &scene, key).unwrap();
        let before_revision =
            native_frame_cloud_revision(&before, &[CloudProduct::OpticalDepth]).unwrap();
        let before_path = before
            .l2_product_path(&store_root, CloudProduct::OpticalDepth)
            .unwrap();

        let scene = conus_scene(&second, "ABI-L2-CODC", None);
        let resolved = {
            archive_goes_l2_source(&store_root, &second, &scene, key).unwrap();
            resolve_native_cloud_frame_with_revision(
                &store_root,
                "g19",
                "conus",
                &[CloudProduct::OpticalDepth],
                &before.frame_id,
            )
            .unwrap()
        };

        assert_ne!(resolved.source_revision, before_revision);
        assert_eq!(
            fs::read(&before_path).unwrap(),
            b"AAAA",
            "the superseded bytes stay addressable"
        );
        assert_eq!(
            fs::read(
                resolved
                    .frame
                    .l2_product_path(&store_root, CloudProduct::OpticalDepth)
                    .unwrap()
            )
            .unwrap(),
            b"BBBB"
        );
    }

    /// The cloud-water-path derivation needs COD, CPS and ACTP from one
    /// scan. A frame holding two of the three is not a CWP frame.
    #[test]
    fn a_multi_product_request_resolves_only_when_every_product_is_present() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let trio = crate::cwp::CLOUD_WATER_PATH_INPUTS;
        let mut frame_id = String::new();
        for (index, (token, product)) in [
            ("ABI-L2-CODC", CloudProduct::OpticalDepth),
            ("ABI-L2-CPSC", CloudProduct::ParticleSize),
            ("ABI-L2-ACTPC", CloudProduct::CloudTopPhase),
        ]
        .into_iter()
        .enumerate()
        {
            let source = directory.path().join(format!("{}.nc", product.slug()));
            fs::write(&source, format!("bytes for {token}")).unwrap();
            let scene = conus_scene(&source, token, None);
            let manifest = archive_goes_l2_source(
                &store_root,
                &source,
                &scene,
                &format!("{token}/2026/216/18/OR_{token}-M6_G19_s1_e2_c3.nc"),
            )
            .unwrap();
            frame_id = manifest.frame_id.clone();
            let complete = index == 2;
            assert_eq!(
                manifest.is_complete_for_cloud(&trio),
                complete,
                "after {token} the trio is complete={complete}"
            );
        }

        let resolved =
            resolve_native_cloud_frame_with_revision(&store_root, "g19", "conus", &trio, "latest")
                .unwrap();
        assert_eq!(resolved.frame.frame_id, frame_id);
        // Requesting the same three products in another order must name
        // the same revision; requesting fewer must not.
        let reordered = [
            CloudProduct::CloudTopPhase,
            CloudProduct::OpticalDepth,
            CloudProduct::ParticleSize,
        ];
        assert_eq!(
            native_frame_cloud_revision(&resolved.frame, &reordered).unwrap(),
            resolved.source_revision
        );
        assert_ne!(
            native_frame_cloud_revision(&resolved.frame, &[CloudProduct::OpticalDepth]).unwrap(),
            resolved.source_revision
        );

        let missing = resolve_native_cloud_frame(
            &store_root,
            "g19",
            "conus",
            &[CloudProduct::CloudTopHeight],
            &frame_id,
        )
        .expect_err("ACHA was never archived here");
        assert!(
            missing.to_string().contains("l2_cloud_top_height"),
            "{missing}"
        );
    }

    #[test]
    fn the_l2_archive_refuses_channel_imagery_and_unknown_products() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let source = directory.path().join("source.nc");
        fs::write(&source, b"bytes").unwrap();

        let channel_scene = conus_scene(&source, "ABI-L2-CMIPC", Some(13));
        let error = archive_goes_l2_source(&store_root, &source, &channel_scene, "key")
            .expect_err("channel imagery has its own archive door");
        assert!(error.to_string().contains("archive_goes_source"), "{error}");

        let other_l2 = conus_scene(&source, "ABI-L2-TPWC", None);
        let error = archive_goes_l2_source(&store_root, &source, &other_l2, "key")
            .expect_err("total precipitable water is not a cloud product");
        assert!(
            error.to_string().contains("not an ABI L2 cloud product"),
            "{error}"
        );
    }

    /// Channel-only frames — every manifest written before the cloud
    /// suite existed — must load and re-save byte for byte.
    #[test]
    fn channel_only_manifests_round_trip_without_an_l2_field() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let source = directory.path().join("c13.nc");
        fs::write(&source, b"channel bytes").unwrap();
        let scene = conus_scene(&source, "ABI-L2-CMIPC", Some(13));
        let manifest = archive_goes_source(&store_root, &source, &scene, "key").unwrap();
        assert!(manifest.l2_products.is_empty());

        let json = serde_json::to_string(&manifest).unwrap();
        assert!(
            !json.contains("l2_products"),
            "an empty L2 map must not appear on disk: {json}"
        );
        let reloaded: NativeSatelliteFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(reloaded, manifest);
    }

    fn native_test_frame(channel_scans: &[(u8, i64, i64)]) -> NativeSatelliteFrame {
        NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.into(),
            platform: "g19".into(),
            sector: "fulldisk".into(),
            frame_id: "20260826T1640".into(),
            scan_start_unix: channel_scans
                .iter()
                .map(|(_, start, _)| *start)
                .min()
                .unwrap(),
            scan_end_unix: channel_scans
                .iter()
                .map(|(_, _, end)| *end)
                .max()
                .unwrap(),
            channels: channel_scans
                .iter()
                .map(|&(channel, scan_start_unix, scan_end_unix)| {
                    (
                        channel,
                        NativeChannelSource {
                            channel,
                            object_key: format!("fixture/c{channel:02}.nc"),
                            relative_path: format!(
                                ".rw-satellite-sources/g19/fulldisk/20260826/20260826T1640/c{channel:02}.nc"
                            ),
                            byte_size: 1,
                            content_blake3: None,
                            scan_start_unix,
                            scan_end_unix,
                        },
                    )
                })
                .collect(),
            l2_products: BTreeMap::new(),
        }
    }

    fn conus_scene(path: &Path, product: &str, channel: Option<u8>) -> GoesAbiScene {
        let start_time_utc = Utc.with_ymd_and_hms(2026, 8, 4, 18, 1, 17).unwrap();
        GoesAbiScene {
            path: path.to_path_buf(),
            product: product.into(),
            sector: AbiSector::Conus,
            channel,
            satellite: GoesSatellite::G19,
            start_time_utc,
            end_time_utc: start_time_utc + chrono::Duration::seconds(155),
            projection: GoesImagerProjection {
                perspective_point_height_m: 35_786_023.0,
                semi_major_axis_m: 6_378_137.0,
                semi_minor_axis_m: 6_356_752.314_14,
                longitude_of_projection_origin_deg: -75.0,
                sweep_angle_axis: SweepAngleAxis::X,
            },
            fixed_grid: AbiFixedGrid {
                nx: 1,
                ny: 1,
                x_scan_rad: vec![0.0],
                y_scan_rad: vec![0.0],
            },
        }
    }

    fn fixture_scene(path: &Path) -> GoesAbiScene {
        let start_time_utc = Utc.with_ymd_and_hms(2026, 8, 22, 19, 41, 37).unwrap();
        GoesAbiScene {
            path: path.to_path_buf(),
            product: "ABI-L2-CMIPF".into(),
            sector: AbiSector::FullDisk,
            channel: Some(13),
            satellite: GoesSatellite::G18,
            start_time_utc,
            end_time_utc: start_time_utc + chrono::Duration::seconds(571),
            projection: GoesImagerProjection {
                perspective_point_height_m: 35_786_023.0,
                semi_major_axis_m: 6_378_137.0,
                semi_minor_axis_m: 6_356_752.314_14,
                longitude_of_projection_origin_deg: -137.0,
                sweep_angle_axis: SweepAngleAxis::X,
            },
            fixed_grid: AbiFixedGrid {
                nx: 1,
                ny: 1,
                x_scan_rad: vec![0.0],
                y_scan_rad: vec![0.0],
            },
        }
    }
}

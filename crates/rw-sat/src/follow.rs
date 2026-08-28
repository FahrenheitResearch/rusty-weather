//! The follow engine: poll the live GOES bucket per band, fetch new objects
//! as soon as they land, ingest them into the rolling store, and evict old
//! frames — with typed [`SatEvent`] progress, a cancel flag, jittered poll
//! intervals, and exponential backoff on HTTP failures (never on "nothing
//! new yet": an empty diff is the normal idle case).
//!
//! Scheduling per the live-bucket survey: keys are diffed with
//! `start-after={last seen key}` under the band-specific hour prefix; near
//! the top of each UTC hour the previous hour's prefix is polled too so
//! stragglers and local clock skew cannot drop frames. Frame timestamps
//! come from the key's `s` (scan start) time, never the local clock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};

use crate::abi::{read_goes_abi_field_strided_from_scene, read_goes_abi_scene};
use crate::archive::{
    ABI_COMPONENT_END_TOLERANCE_SECONDS, NativeSatelliteFrame, archive_goes_source,
    automatic_preview_stride, list_native_frames, prune_native_archive,
};
use crate::events::{NEVER_CANCEL, SatError, SatEvent, other};
use crate::goes::{GoesSatellite, parse_goes_abi_filename};
use crate::product::GoesAbiProduct;
use crate::s3::{
    DownloadedObject, ObjectDownloadError, S3Object, Sector, abi_filename_product_matches_request,
    band_hour_prefix, bucket_for_satellite, build_agent, cached_object_path,
    download_object_with_control, list_s3_objects, object_filename, prune_object_cache,
};
use crate::store::{WrittenFrame, write_band_frame};
use crate::window::{WindowConfig, enforce_window};

/// Minutes after the top of the hour during which the previous hour's
/// prefix keeps being polled (stragglers + clock skew).
const HOUR_ROLLOVER_GRACE_MINUTES: u32 = 5;
/// A Full Disk scan itself takes roughly ten minutes. Keep the prior hour
/// available long enough that a one-poll "latest" request at HH:05-HH:10 can
/// still find HH-1:50 instead of returning an empty result while HH:00 is
/// incomplete.
const FULL_DISK_ROLLOVER_GRACE_MINUTES: u32 = 15;
/// Backoff cap after consecutive poll errors.
const MAX_BACKOFF_SECS: u64 = 300;
/// Ingest attempts before a repeatedly failing object is skipped for good
/// (with poll backoff in between, this spreads the retries over minutes).
const MAX_INGEST_ATTEMPTS: u32 = 5;
/// Cancel-flag check granularity while sleeping.
const SLEEP_SLICE_MS: u64 = 100;
/// Compact `.rws` previews stay below this many cells even when a caller asks
/// for stride 1. Native NetCDF archival is unaffected.
const MAXIMUM_PREVIEW_CELLS: usize = 8_000_000;

#[derive(Debug, Clone)]
pub struct FollowConfig {
    /// Satellite name (`goes19`, `g18`, ...).
    pub satellite: String,
    pub sector: Sector,
    /// ABI bands to follow (1..=16).
    pub bands: Vec<u8>,
    /// ABI scan mode token in filenames (6 = nominal since 2019).
    pub mode: u8,
    pub store_root: PathBuf,
    pub cache_dir: PathBuf,
    /// Base poll interval; `None` uses the sector default.
    pub poll_interval: Option<Duration>,
    /// +/- jitter fraction applied to every sleep (default 0.2).
    pub jitter_frac: f64,
    /// Preview stride: 0 chooses an automatic bounded preview; native source is always retained.
    pub downsample: usize,
    pub window: WindowConfig,
    /// Stop after this many poll cycles (`None` = run until cancelled).
    pub max_polls: Option<u32>,
    /// Stop once this many frames have been ingested.
    pub max_frames: Option<u32>,
    /// Fill older complete scans from the startup prefixes after securing the
    /// newest scan. When false, startup history is skipped, but every complete
    /// scan that arrives after bootstrap is still followed.
    pub backfill_history: bool,
    pub use_cache: bool,
}

impl FollowConfig {
    pub fn new(satellite: &str, sector: Sector, bands: Vec<u8>) -> Self {
        Self {
            satellite: satellite.to_string(),
            sector,
            bands,
            mode: 6,
            store_root: PathBuf::from("store"),
            cache_dir: PathBuf::from("cache"),
            poll_interval: None,
            jitter_frac: 0.2,
            downsample: 0,
            window: WindowConfig::default(),
            max_polls: None,
            max_frames: None,
            backfill_history: true,
            use_cache: true,
        }
    }

    fn base_interval(&self) -> Duration {
        self.poll_interval
            .unwrap_or_else(|| Duration::from_secs(self.sector.default_poll_secs()))
    }
}

/// What a follow session did.
#[derive(Debug, Default)]
pub struct FollowSummary {
    pub polls: u32,
    pub frames: Vec<WrittenFrame>,
    pub downloaded_keys: Vec<String>,
    pub evicted_frames: usize,
    pub evicted_bytes: u64,
}

#[derive(Debug)]
pub struct IngestedObject {
    pub download: DownloadedObject,
    pub native_frame: NativeSatelliteFrame,
    /// Compact `.rws` preview. Native archival remains a successful ingest
    /// when this optional derivative cannot be decoded or written.
    pub preview_frame: Option<WrittenFrame>,
}

/// The hour prefixes one poll must cover: the current scan hour, preceded
/// by the previous hour during the first [`HOUR_ROLLOVER_GRACE_MINUTES`]
/// of each hour. Pure for testing.
pub fn poll_prefixes(
    abi_product: &str,
    satellite: &GoesSatellite,
    mode: u8,
    band: u8,
    now: DateTime<Utc>,
) -> Vec<String> {
    let mut prefixes = Vec::with_capacity(2);
    let rollover_grace = if abi_product.trim().to_ascii_uppercase().ends_with("CMIPF") {
        FULL_DISK_ROLLOVER_GRACE_MINUTES
    } else {
        HOUR_ROLLOVER_GRACE_MINUTES
    };
    if now.minute() < rollover_grace {
        let previous = now - chrono::Duration::hours(1);
        prefixes.push(band_hour_prefix(
            abi_product,
            satellite,
            mode,
            band,
            previous,
        ));
    }
    prefixes.push(band_hour_prefix(abi_product, satellite, mode, band, now));
    prefixes
}

/// The sleep before the next poll: `base` +/- `jitter_frac` (with
/// `unit_sample` in `[0, 1]` mapping to `[-1, +1]`), doubled per
/// consecutive error and capped at [`MAX_BACKOFF_SECS`]. Pure for testing.
pub fn poll_delay(
    base: Duration,
    jitter_frac: f64,
    unit_sample: f64,
    consecutive_errors: u32,
) -> Duration {
    let jitter = jitter_frac.clamp(0.0, 1.0) * (unit_sample.clamp(0.0, 1.0) * 2.0 - 1.0);
    let jittered = base.as_secs_f64() * (1.0 + jitter);
    let backoff = jittered * f64::from(2u32.saturating_pow(consecutive_errors.min(16)));
    Duration::from_secs_f64(backoff.min(MAX_BACKOFF_SECS as f64).max(0.05))
}

/// Bounded dedup of already-ingested scans, keyed by (band, provider scan
/// start SECOND). Native channel manifests retain that exact source second;
/// minute-only identity is insufficient because distinct or republished scans
/// can share one `tHHMM` store slot.
#[derive(Debug, Default)]
pub struct SeenScans {
    seen: BTreeMap<(u8, DateTime<Utc>), SeenScan>,
}

#[derive(Debug)]
struct SeenScan {
    end_unix: i64,
    /// Exact provider object retained for this channel/scan. `None` is kept
    /// only for the small public/test insertion helpers that predate source
    /// identity; archive priming and live ingest always set this.
    object_key: Option<String>,
}

impl SeenScans {
    /// Record (band, provider start second). Returns `false` when already seen.
    pub fn insert(&mut self, band: u8, start: DateTime<Utc>) -> bool {
        self.insert_window(band, start, start)
    }

    pub fn insert_window(&mut self, band: u8, start: DateTime<Utc>, end: DateTime<Utc>) -> bool {
        self.insert_source_window(band, start, end, None)
    }

    fn insert_object_window(
        &mut self,
        band: u8,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        object_key: &str,
    ) -> bool {
        self.insert_source_window(band, start, end, Some(object_key.to_owned()))
    }

    fn insert_source_window(
        &mut self,
        band: u8,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        object_key: Option<String>,
    ) -> bool {
        let slot = (band, scan_second(start));
        if let Some(previous) = self.seen.get_mut(&slot) {
            // An identity-free compatibility insertion must never erase the
            // exact source identity established by archive priming or ingest.
            if object_key.is_some() || previous.object_key.is_none() {
                *previous = SeenScan {
                    end_unix: end.timestamp(),
                    object_key,
                };
            }
            false
        } else {
            self.seen.insert(
                slot,
                SeenScan {
                    end_unix: end.timestamp(),
                    object_key,
                },
            );
            true
        }
    }

    pub fn contains(&self, band: u8, start: DateTime<Utc>) -> bool {
        self.seen.contains_key(&(band, scan_second(start)))
    }

    fn end_unix(&self, band: u8, start: DateTime<Utc>) -> Option<i64> {
        self.seen
            .get(&(band, scan_second(start)))
            .map(|scan| scan.end_unix)
    }

    /// Whether this exact provider publication is already retained. Entries
    /// created through the legacy identity-free helpers remain wildcard seen
    /// values so their established public behavior does not change.
    fn contains_object(&self, band: u8, start: DateTime<Utc>, object_key: &str) -> bool {
        self.seen
            .get(&(band, scan_second(start)))
            .is_some_and(|scan| {
                scan.object_key
                    .as_deref()
                    .is_none_or(|retained| retained == object_key)
            })
    }

    /// Drop entries older than `cutoff` (call with `now - window`).
    pub fn prune_older_than(&mut self, cutoff: DateTime<Utc>) {
        self.seen.retain(|&(_, start), _| start >= cutoff);
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
}

/// Truncate a scan start to its minute (the store's HHMM granularity).
fn scan_minute(start: DateTime<Utc>) -> DateTime<Utc> {
    start
        .with_second(0)
        .and_then(|time| time.with_nanosecond(0))
        .unwrap_or(start)
}

/// Dedup state rebuilt exclusively from retained native manifests.
///
/// Compact `.rws` previews are derivatives, not proof that the exact native
/// source was retained. In particular, stores created before native archival
/// can contain a legacy preview for a scan that still needs native backfill.
/// Native manifests are therefore the only restart-dedup authority.
pub fn primed_seen_scans(
    store_root: &std::path::Path,
    model: &str,
    sector_slug: &str,
    bands: &[u8],
) -> SeenScans {
    let mut seen = SeenScans::default();
    for &band in bands {
        if let Ok(frames) = list_native_frames(
            store_root,
            model,
            sector_slug,
            GoesAbiProduct::RawChannel(band),
            usize::MAX,
        ) {
            for frame in frames {
                let Some(source) = frame.channels.get(&band) else {
                    continue;
                };
                if let Some(time) = DateTime::<Utc>::from_timestamp(source.scan_start_unix, 0) {
                    if let Some(end) = DateTime::<Utc>::from_timestamp(source.scan_end_unix, 0) {
                        seen.insert_object_window(band, time, end, &source.object_key);
                    }
                }
            }
        }
    }
    seen
}

/// Cheap deterministic xorshift in `[0, 1)` for poll jitter.
#[derive(Debug)]
pub struct JitterRng(u64);

impl JitterRng {
    pub fn new(seed: u64) -> Self {
        Self(seed.max(1))
    }

    pub fn next_unit(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Download one object, retain its native source, and derive a bounded store
/// preview. Shared by the follow loop and the one-shot `latest` CLI flow.
///
/// Native archival is the durable ingest boundary. A preview-only failure is
/// reported as a warning and returns `preview_frame: None`; retrying the same
/// large NOAA object cannot repair a deterministic preview decoder failure.
#[allow(clippy::too_many_arguments)]
pub fn fetch_and_ingest(
    agent: &ureq::Agent,
    bucket: &str,
    cache_dir: &std::path::Path,
    store_root: &std::path::Path,
    object: &S3Object,
    downsample: usize,
    use_cache: bool,
    written_unix: u64,
    sink: &mut dyn FnMut(SatEvent),
) -> Result<IngestedObject, SatError> {
    fetch_and_ingest_with_cancel(
        agent,
        bucket,
        cache_dir,
        store_root,
        object,
        downsample,
        use_cache,
        written_unix,
        &NEVER_CANCEL,
        sink,
    )
}

/// Cancellation-aware form used by live follow sessions. The cancel flag is
/// observed between socket reads, so Stop does not wait for a Full Disk body
/// to finish downloading.
#[allow(clippy::too_many_arguments)]
pub fn fetch_and_ingest_with_cancel(
    agent: &ureq::Agent,
    bucket: &str,
    cache_dir: &std::path::Path,
    store_root: &std::path::Path,
    object: &S3Object,
    downsample: usize,
    use_cache: bool,
    written_unix: u64,
    cancel: &AtomicBool,
    sink: &mut dyn FnMut(SatEvent),
) -> Result<IngestedObject, SatError> {
    sink(SatEvent::DownloadStarted {
        key: object.key.clone(),
        bytes: object.size_bytes,
    });
    let started = Instant::now();
    let download = download_object_with_control(
        agent,
        bucket,
        cache_dir,
        object,
        use_cache,
        cancel,
        &mut |progress| {
            sink(SatEvent::DownloadProgress {
                key: object.key.clone(),
                received_bytes: progress.received_bytes,
                total_bytes: progress.total_bytes,
            });
        },
    )
    .map_err(|error| match error {
        ObjectDownloadError::Cancelled => SatError::Cancelled,
        error => other(error),
    })?;
    sink(SatEvent::DownloadDone {
        key: object.key.clone(),
        bytes: object.size_bytes,
        ms: started.elapsed().as_millis(),
        cache_hit: download.cache_hit,
    });

    // Scene metadata reads only the 1-D fixed-grid axes and projection. In
    // particular, it does not materialize Full Disk C02's 470,716,416-cell
    // CMI plane. Retain the exact NOAA bytes before attempting any derivative.
    let scene = read_goes_abi_scene(&download.path).map_err(to_send_sync)?;
    let channel = scene
        .channel
        .ok_or_else(|| other("cannot ingest a GOES source without a channel"))?;
    let native_frame = archive_goes_source(store_root, &download.path, &scene, &object.key)
        .map_err(|error| other(error.to_string()))?;
    sink(SatEvent::NativeFrameUpdated {
        frame: native_frame.clone(),
        committed_channel: channel,
    });

    let preview_stride =
        bounded_preview_stride(scene.fixed_grid.nx, scene.fixed_grid.ny, downsample);
    if downsample > 0 && preview_stride > downsample {
        sink(SatEvent::Info {
            message: format!(
                "compact C{channel:02} preview stride raised from {downsample} to {preview_stride} to stay within {MAXIMUM_PREVIEW_CELLS} cells; native source remains full resolution"
            ),
        });
    }

    let preview = (|| -> Result<WrittenFrame, Box<dyn std::error::Error>> {
        let mut archived_scene = scene;
        archived_scene.path = native_frame.channel_path(store_root, channel)?;
        let field = read_goes_abi_field_strided_from_scene(&archived_scene, "CMI", preview_stride)?;
        write_band_frame(store_root, &field, written_unix)
    })();
    let preview_frame = match preview {
        Ok(frame) => {
            sink(SatEvent::FrameWritten {
                model: frame.model.clone(),
                run: frame.run.clone(),
                hhmm: frame.hhmm,
                scan_time_utc: frame.scan_time_utc,
                path: frame.path.clone(),
                bytes: frame.bytes,
                encode_ms: frame.encode_ms,
            });
            Some(frame)
        }
        Err(error) => {
            sink(SatEvent::Warning {
                message: format!(
                    "native ABI source retained for {} C{channel:02}; compact preview unavailable and will not trigger a source redownload: {error}",
                    native_frame.frame_id
                ),
            });
            None
        }
    };
    Ok(IngestedObject {
        download,
        native_frame,
        preview_frame,
    })
}

fn bounded_preview_stride(nx: usize, ny: usize, requested: usize) -> usize {
    let automatic = automatic_preview_stride(nx, ny, MAXIMUM_PREVIEW_CELLS);
    if requested == 0 {
        automatic
    } else {
        requested.max(automatic)
    }
}

/// Process one prefix's freshly listed objects in key order, advancing the
/// `start-after` watermark in `last_key` only through objects that were
/// skipped on purpose (not ours, stale, already seen) or successfully
/// ingested. On a retryable ingest failure the watermark stays put and the
/// rest of the prefix is left alone, so the next poll re-lists the failed
/// object and retries it — a transient S3 503 or truncated read no longer
/// leaves a permanent gap in the loop. `attempts` caps the retries per key
/// ([`MAX_INGEST_ATTEMPTS`]) so one poisoned object cannot stall its
/// prefix forever. `seen` is only marked on success.
///
/// Returns already-retained provider scans plus warning messages to emit;
/// every warning also means "this poll failed" for backoff purposes.
#[derive(Debug, Default)]
struct ProcessListedReport {
    warnings: Vec<String>,
    retained: Vec<S3Object>,
    consumed_keys: Vec<String>,
    retry_blocked: bool,
}

/// Native manifests store Unix seconds, while ABI filenames include tenths.
/// Canonicalizing only the subsecond component preserves exact scan identity
/// across the listing/archive boundary without collapsing distinct seconds.
fn scan_second(start: DateTime<Utc>) -> DateTime<Utc> {
    start.with_nanosecond(0).unwrap_or(start)
}

#[derive(Debug, Clone, Copy)]
struct ListedScanIdentity {
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    created: DateTime<Utc>,
}

fn listed_scan_identity(
    object: &S3Object,
    band: u8,
    abi_product: &str,
    stale_cutoff: Option<DateTime<Utc>>,
) -> Option<ListedScanIdentity> {
    parse_goes_abi_filename(object_filename(&object.key))
        .ok()
        .filter(|parsed| {
            object.key.ends_with(".nc")
                && abi_filename_product_matches_request(&parsed.product, abi_product)
                && parsed.channel == Some(band)
                // Never ingest a scan the rolling window would evict on the
                // spot: a restart can re-list a whole hour prefix.
                && stale_cutoff.is_none_or(|cutoff| parsed.start_time_utc >= cutoff)
        })
        .map(|parsed| ListedScanIdentity {
            start: parsed.start_time_utc,
            end: parsed.end_time_utc,
            created: parsed.created_time_utc,
        })
}

#[allow(clippy::too_many_arguments)]
fn process_listed_objects(
    prefix: &str,
    objects: &[S3Object],
    band: u8,
    abi_product: &str,
    stale_cutoff: Option<DateTime<Utc>>,
    seen: &mut SeenScans,
    attempts: &mut HashMap<String, u32>,
    last_key: &mut HashMap<String, String>,
    cancel: &AtomicBool,
    ingest: &mut dyn FnMut(&S3Object) -> Result<(), SatError>,
) -> Result<ProcessListedReport, SatError> {
    let mut report = ProcessListedReport::default();
    for object in objects {
        check_cancel(cancel)?;
        let scan = listed_scan_identity(object, band, abi_product, stale_cutoff);
        let Some(scan) = scan else {
            report.consumed_keys.push(object.key.clone());
            last_key.insert(prefix.to_string(), object.key.clone());
            continue;
        };
        if seen.contains_object(band, scan.start, &object.key) {
            report.retained.push(object.clone());
            report.consumed_keys.push(object.key.clone());
            last_key.insert(prefix.to_string(), object.key.clone());
            continue;
        }
        match ingest(object) {
            Ok(()) => {
                seen.insert_object_window(band, scan.start, scan.end, &object.key);
                attempts.remove(&object.key);
                report.consumed_keys.push(object.key.clone());
                last_key.insert(prefix.to_string(), object.key.clone());
            }
            Err(SatError::Cancelled) => return Err(SatError::Cancelled),
            Err(err) => {
                let tried = {
                    let entry = attempts.entry(object.key.clone()).or_insert(0);
                    *entry += 1;
                    *entry
                };
                if tried >= MAX_INGEST_ATTEMPTS {
                    report.warnings.push(format!(
                        "ingest {}: {err} (attempt {tried}/{MAX_INGEST_ATTEMPTS}, giving up on this object)",
                        object.key
                    ));
                    attempts.remove(&object.key);
                    report.consumed_keys.push(object.key.clone());
                    last_key.insert(prefix.to_string(), object.key.clone());
                } else {
                    report.warnings.push(format!(
                        "ingest {}: {err} (attempt {tried}/{MAX_INGEST_ATTEMPTS}, will retry after re-listing)",
                        object.key
                    ));
                    report.retry_blocked = true;
                    // Hold the watermark before this object: the next
                    // poll re-lists it and everything after it.
                    break;
                }
            }
        }
    }
    Ok(report)
}

#[derive(Debug, Clone)]
struct PolledFollowObject {
    prefix: String,
    band: u8,
    object: S3Object,
    scan_start: Option<DateTime<Utc>>,
    scan_end: Option<DateTime<Utc>>,
    scan_created: Option<DateTime<Utc>>,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ScanMajorPlan {
    newest_complete: Option<DateTime<Utc>>,
    /// Complete scan slots, newest first; entries inside each slot follow the
    /// configured channel order so the product becomes displayable promptly.
    batches: Vec<Vec<usize>>,
    /// Older publications of the same exact channel/scan. These must be
    /// consumed without ingest so an obsolete correction cannot pin the
    /// prefix watermark ahead of the selected newest publication.
    superseded: Vec<usize>,
    /// Older provider objects intentionally consumed when history backfill is
    /// disabled. Newer incomplete scans remain pending for the next poll.
    skipped_history: Vec<usize>,
    /// Incomplete scans older than a newer complete scan cannot contribute a
    /// product frame and must not permanently pin every prefix watermark.
    skipped_incomplete: Vec<usize>,
}

fn exact_scan_slot_complete(
    slot: DateTime<Utc>,
    by_band: &HashMap<u8, usize>,
    objects: &[PolledFollowObject],
    bands: &[u8],
    seen: &SeenScans,
) -> bool {
    let mut earliest_end = i64::MAX;
    let mut latest_end = i64::MIN;
    for &band in bands {
        // A listed correction is the candidate we would actually ingest, so
        // its scan end must outrank stale metadata from the retained object.
        let end = by_band
            .get(&band)
            .and_then(|&index| objects[index].scan_end)
            .map(|end| end.timestamp())
            .or_else(|| seen.end_unix(band, slot));
        let Some(end) = end else {
            return false;
        };
        earliest_end = earliest_end.min(end);
        latest_end = latest_end.max(end);
    }
    latest_end.saturating_sub(earliest_end) <= ABI_COMPONENT_END_TOLERANCE_SECONDS
}

fn scan_major_plan(
    objects: &[PolledFollowObject],
    bands: &[u8],
    seen: &SeenScans,
    backfill_history: bool,
) -> ScanMajorPlan {
    let mut slots: BTreeMap<DateTime<Utc>, HashMap<u8, usize>> = BTreeMap::new();
    let mut superseded = Vec::new();
    for (index, listed) in objects.iter().enumerate() {
        let Some(scan_start) = listed.scan_start else {
            continue;
        };
        let by_band = slots.entry(scan_second(scan_start)).or_default();
        if let Some(selected) = by_band.get_mut(&listed.band) {
            let current = &objects[*selected];
            let replace = listed
                .scan_created
                .cmp(&current.scan_created)
                .then_with(|| listed.object.key.cmp(&current.object.key))
                .is_gt();
            if replace {
                superseded.push(*selected);
                *selected = index;
            } else {
                superseded.push(index);
            }
        } else {
            by_band.insert(listed.band, index);
        }
    }

    let newest_complete = slots.iter().rev().find_map(|(&slot, by_band)| {
        exact_scan_slot_complete(slot, by_band, objects, bands, seen).then_some(slot)
    });
    let mut plan = ScanMajorPlan {
        newest_complete,
        superseded,
        ..ScanMajorPlan::default()
    };
    let Some(newest_complete) = newest_complete else {
        return plan;
    };

    for (&slot, by_band) in slots.iter().rev() {
        let complete = exact_scan_slot_complete(slot, by_band, objects, bands, seen);
        if complete && (backfill_history || slot == newest_complete) {
            let mut batch = Vec::new();
            for &band in bands {
                if let Some(&index) = by_band.get(&band) {
                    batch.push(index);
                }
            }
            plan.batches.push(batch);
        } else if slot < newest_complete {
            let skipped = if complete {
                &mut plan.skipped_history
            } else {
                &mut plan.skipped_incomplete
            };
            for &band in bands {
                if let Some(&index) = by_band.get(&band) {
                    skipped.push(index);
                }
            }
        }
    }
    plan
}

fn backfill_enabled_for_poll(configured: bool, bootstrap_complete: bool) -> bool {
    configured || bootstrap_complete
}

fn advance_consumed_watermarks(
    objects: &[PolledFollowObject],
    consumed_keys: &HashSet<String>,
    last_key: &mut HashMap<String, String>,
) {
    let mut by_prefix: BTreeMap<&str, Vec<&S3Object>> = BTreeMap::new();
    for listed in objects {
        by_prefix
            .entry(&listed.prefix)
            .or_default()
            .push(&listed.object);
    }
    for (prefix, listed) in by_prefix {
        for object in listed {
            if !consumed_keys.contains(&object.key) {
                break;
            }
            last_key.insert(prefix.to_string(), object.key.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn ingest_follow_object(
    agent: &ureq::Agent,
    bucket: &str,
    config: &FollowConfig,
    band: u8,
    object: &S3Object,
    cancel: &AtomicBool,
    summary: &mut FollowSummary,
    sink: &mut dyn FnMut(SatEvent),
) -> Result<(), SatError> {
    let written_unix = Utc::now().timestamp().max(0) as u64;
    let result = fetch_and_ingest_with_cancel(
        agent,
        bucket,
        &config.cache_dir,
        &config.store_root,
        object,
        config.downsample,
        config.use_cache,
        written_unix,
        cancel,
        sink,
    );
    // The retained native source is the artifact of record; the raw staging
    // copy never helps this session again. A failed source ingest must refetch
    // rather than replay a corrupt size-matched body.
    if !matches!(result, Err(SatError::Cancelled)) {
        let _ = std::fs::remove_file(cached_object_path(&config.cache_dir, bucket, &object.key));
    }
    let outcome = result?;
    let model = outcome.native_frame.platform.clone();
    summary.downloaded_keys.push(object.key.clone());
    if let Some(frame) = outcome.preview_frame {
        summary.frames.push(frame.clone());
        let run_prefix = format!("{}_c{band:02}", config.sector.slug());
        match enforce_window(
            &config.store_root,
            &frame.model,
            &run_prefix,
            Utc::now(),
            &config.window,
        ) {
            Ok(report) if report.removed_frames > 0 => {
                summary.evicted_frames += report.removed_frames;
                summary.evicted_bytes += report.removed_bytes;
                sink(SatEvent::Evicted {
                    model: frame.model.clone(),
                    frames: report.removed_frames,
                    bytes: report.removed_bytes,
                });
            }
            Ok(_) => {}
            Err(err) => sink(SatEvent::Warning {
                message: format!("window eviction: {err}"),
            }),
        }
    }
    let archive_max_bytes = config
        .window
        .max_bytes
        .map(|bytes| bytes.saturating_mul(config.bands.len().max(1) as u64));
    match prune_native_archive(
        &config.store_root,
        &model,
        config.sector.slug(),
        Utc::now(),
        config.window.max_age_minutes,
        archive_max_bytes,
    ) {
        Ok(report) if report.removed_frames > 0 => sink(SatEvent::Info {
            message: format!(
                "native archive pruned: {} frame(s), {} bytes",
                report.removed_frames, report.removed_bytes
            ),
        }),
        Ok(_) => {}
        Err(err) => sink(SatEvent::Warning {
            message: format!("native archive eviction: {err}"),
        }),
    }
    Ok(())
}

/// Run a follow session. Returns when `max_polls`/`max_frames` is reached;
/// observing the cancel flag at any boundary returns
/// [`SatError::Cancelled`].
pub fn follow(
    config: &FollowConfig,
    sink: &mut dyn FnMut(SatEvent),
    cancel: &AtomicBool,
) -> Result<FollowSummary, SatError> {
    if config.bands.is_empty() {
        return Err(other("follow requires at least one band"));
    }
    for &band in &config.bands {
        if !(1..=16).contains(&band) {
            return Err(other(format!("ABI band out of range: {band}")));
        }
    }
    let bucket = bucket_for_satellite(&config.satellite).map_err(to_send_sync)?;
    let satellite = GoesSatellite::parse(&config.satellite);
    let abi_product = config.sector.abi_product();
    let agent = build_agent();
    let base_interval = config.base_interval();
    let mut rng = JitterRng::new(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1),
    );

    let mut summary = FollowSummary::default();
    // Dedup survives restarts: every frame the rolling window already
    // holds (per the run manifests) is pre-marked seen, so re-listing the
    // whole current-hour prefix on session start fetches nothing twice.
    let model = satellite.as_str().to_ascii_lowercase();
    let mut seen = primed_seen_scans(
        &config.store_root,
        &model,
        config.sector.slug(),
        &config.bands,
    );
    if !seen.is_empty() {
        sink(SatEvent::Info {
            message: format!(
                "dedup primed from store manifests: {} native channel scan(s) already retained",
                seen.len()
            ),
        });
    }
    // start-after state per (band, hour prefix).
    let mut last_key: HashMap<String, String> = HashMap::new();
    // Failed-ingest retry counts per S3 key (bounded: pruned with `seen`).
    let mut ingest_attempts: HashMap<String, u32> = HashMap::new();
    let mut consecutive_errors: u32 = 0;
    // `backfill_history = false` is a startup policy, not a permanent
    // latest-only mode. Once one exact complete scan is secured, every later
    // complete scan is ingested even when several accumulated between polls.
    let mut bootstrap_complete = config.backfill_history;

    loop {
        check_cancel(cancel)?;
        let mut poll_failed = false;
        let poll_now = Utc::now();
        let stale_cutoff = config
            .window
            .max_age_minutes
            .map(|minutes| poll_now - chrono::Duration::minutes(i64::from(minutes)));
        let mut poll_started_by_band = HashMap::new();
        let mut polled_objects = Vec::new();

        // List every requested channel before downloading anything. The old
        // band-major loop downloaded a whole hour of C01, then C02, then C03;
        // no RGB scan could become complete until the final channel caught
        // up. A single cross-channel inventory lets us schedule by scan.
        for &band in &config.bands {
            check_cancel(cancel)?;
            let prefixes = poll_prefixes(abi_product, &satellite, config.mode, band, poll_now);
            sink(SatEvent::PollStarted {
                band,
                prefixes: prefixes.clone(),
            });
            poll_started_by_band.insert(band, Instant::now());
            for prefix in &prefixes {
                let start_after = last_key.get(prefix).map(String::as_str);
                let objects = match list_s3_objects(&agent, &bucket, prefix, start_after) {
                    Ok(objects) => objects,
                    Err(err) => {
                        poll_failed = true;
                        sink(SatEvent::Warning {
                            message: format!("list {prefix}: {err}"),
                        });
                        continue;
                    }
                };
                for object in objects {
                    let scan = listed_scan_identity(&object, band, abi_product, stale_cutoff);
                    polled_objects.push(PolledFollowObject {
                        scan_start: scan.map(|identity| identity.start),
                        scan_end: scan.map(|identity| identity.end),
                        scan_created: scan.map(|identity| identity.created),
                        prefix: prefix.clone(),
                        band,
                        object,
                    });
                }
            }
        }

        let backfill_this_poll =
            backfill_enabled_for_poll(config.backfill_history, bootstrap_complete);
        let plan = scan_major_plan(&polled_objects, &config.bands, &seen, backfill_this_poll);
        let mut consumed_keys: HashSet<String> = polled_objects
            .iter()
            .filter(|listed| listed.scan_start.is_none())
            .map(|listed| listed.object.key.clone())
            .collect();
        for &index in &plan.skipped_history {
            consumed_keys.insert(polled_objects[index].object.key.clone());
        }
        for &index in &plan.skipped_incomplete {
            consumed_keys.insert(polled_objects[index].object.key.clone());
        }
        for &index in &plan.superseded {
            consumed_keys.insert(polled_objects[index].object.key.clone());
        }
        if !plan.skipped_history.is_empty() {
            sink(SatEvent::Info {
                message: format!(
                    "loop backfill disabled: skipped {} older component object(s)",
                    plan.skipped_history.len()
                ),
            });
        }
        if !plan.skipped_incomplete.is_empty() {
            sink(SatEvent::Info {
                message: format!(
                    "skipped {} component object(s) from incomplete scans older than the newest complete scan",
                    plan.skipped_incomplete.len()
                ),
            });
        }

        let mut execution_order = Vec::new();
        let mut scheduled = HashSet::new();
        for batch in &plan.batches {
            for &index in batch {
                if scheduled.insert(index) {
                    execution_order.push(index);
                }
            }
        }
        // Seen members of an incomplete provider scan are safe to consume and
        // should still surface as retained rows; unseen members remain behind
        // the per-prefix watermark until the missing channels appear.
        for (index, listed) in polled_objects.iter().enumerate() {
            if scheduled.contains(&index)
                || plan.skipped_history.contains(&index)
                || plan.skipped_incomplete.contains(&index)
                || plan.superseded.contains(&index)
                || listed.scan_start.is_none_or(|scan_start| {
                    !seen.contains_object(listed.band, scan_start, &listed.object.key)
                })
            {
                continue;
            }
            execution_order.push(index);
        }

        if let Some(first_batch) = plan.batches.first()
            && let Some(&first_index) = first_batch.first()
            && first_batch.iter().any(|&index| {
                let listed = &polled_objects[index];
                listed.scan_start.is_some_and(|scan_start| {
                    !seen.contains_object(listed.band, scan_start, &listed.object.key)
                })
            })
        {
            let scan = scan_minute(
                polled_objects[first_index]
                    .scan_start
                    .expect("planned objects have scan starts"),
            );
            sink(SatEvent::Info {
                message: if backfill_this_poll {
                    format!(
                        "prioritizing newest complete multichannel scan {} before loop backfill",
                        scan.format("%H:%MZ")
                    )
                } else {
                    format!(
                        "prioritizing newest complete multichannel scan {}; loop backfill disabled",
                        scan.format("%H:%MZ")
                    )
                },
            });
        }

        let mut ingested_by_band: HashMap<u8, usize> = HashMap::new();
        let mut retained_by_band: HashMap<u8, usize> = HashMap::new();
        let mut retry_blocked_slots = HashSet::new();
        for index in execution_order {
            check_cancel(cancel)?;
            let listed = &polled_objects[index];
            let slot = scan_second(
                listed
                    .scan_start
                    .expect("execution objects have scan starts"),
            );
            if retry_blocked_slots.contains(&(listed.band, slot)) {
                continue;
            }
            let mut discarded_watermark = HashMap::new();
            let report = {
                let mut ingest = |object: &S3Object| -> Result<(), SatError> {
                    let result = ingest_follow_object(
                        &agent,
                        &bucket,
                        config,
                        listed.band,
                        object,
                        cancel,
                        &mut summary,
                        &mut *sink,
                    );
                    if result.is_ok() {
                        *ingested_by_band.entry(listed.band).or_default() += 1;
                    }
                    result
                };
                process_listed_objects(
                    &listed.prefix,
                    std::slice::from_ref(&listed.object),
                    listed.band,
                    abi_product,
                    stale_cutoff,
                    &mut seen,
                    &mut ingest_attempts,
                    &mut discarded_watermark,
                    cancel,
                    &mut ingest,
                )?
            };
            consumed_keys.extend(report.consumed_keys);
            *retained_by_band.entry(listed.band).or_default() += report.retained.len();
            for object in report.retained {
                sink(SatEvent::AlreadyRetained {
                    key: object.key,
                    bytes: object.size_bytes,
                });
            }
            if report.retry_blocked {
                retry_blocked_slots.insert((listed.band, slot));
            }
            for message in report.warnings {
                poll_failed = true;
                sink(SatEvent::Warning { message });
            }
        }
        advance_consumed_watermarks(&polled_objects, &consumed_keys, &mut last_key);

        if !bootstrap_complete
            && let Some(slot) = plan.newest_complete
            && config.bands.iter().all(|&band| seen.contains(band, slot))
        {
            let empty = HashMap::new();
            bootstrap_complete =
                exact_scan_slot_complete(slot, &empty, &polled_objects, &config.bands, &seen);
        }

        for &band in &config.bands {
            sink(SatEvent::PollDone {
                band,
                new_keys: ingested_by_band.get(&band).copied().unwrap_or(0),
                retained_keys: retained_by_band.get(&band).copied().unwrap_or(0),
                ms: poll_started_by_band
                    .get(&band)
                    .map(Instant::elapsed)
                    .unwrap_or_default()
                    .as_millis(),
            });
        }
        // Keep the per-prefix bookkeeping bounded: drop start-after
        // watermarks and retry counters for hour prefixes that rotated out
        // of the poll set.
        let now = Utc::now();
        let active: Vec<String> = config
            .bands
            .iter()
            .flat_map(|&band| poll_prefixes(abi_product, &satellite, config.mode, band, now))
            .collect();
        last_key.retain(|prefix, _| active.contains(prefix));
        ingest_attempts
            .retain(|key, _| active.iter().any(|prefix| key.starts_with(prefix.as_str())));
        // Dedup memory stays bounded: anything older than a day is gone
        // from the hour prefixes we poll anyway.
        seen.prune_older_than(Utc::now() - chrono::Duration::days(1));
        // The raw-object cache obeys the rolling window too: each ingest
        // already deletes its own cached object (above); this sweep catches
        // leftovers — interrupted sessions, repeatedly failing objects —
        // so a 24/7 follow keeps a bounded disk footprint even though
        // `enforce_window` only knows the store. Without a max-age the
        // sweep uses the same one-day horizon as the dedup set.
        let cache_cutoff = Utc::now()
            - chrono::Duration::minutes(i64::from(
                config.window.max_age_minutes.unwrap_or(24 * 60),
            ));
        let pruned = prune_object_cache(&config.cache_dir, &bucket, cache_cutoff);
        if pruned.removed_files > 0 {
            sink(SatEvent::Info {
                message: format!(
                    "cache pruned: {} object(s), {} bytes",
                    pruned.removed_files, pruned.removed_bytes
                ),
            });
        }

        consecutive_errors = if poll_failed {
            consecutive_errors.saturating_add(1)
        } else {
            0
        };
        summary.polls += 1;
        if config.max_polls.is_some_and(|max| summary.polls >= max) {
            return Ok(summary);
        }
        if config
            .max_frames
            .is_some_and(|max| summary.downloaded_keys.len() as u32 >= max)
        {
            return Ok(summary);
        }

        let delay = poll_delay(
            base_interval,
            config.jitter_frac,
            rng.next_unit(),
            consecutive_errors,
        );
        sink(SatEvent::Sleeping {
            ms: delay.as_millis() as u64,
        });
        sleep_cancellable(delay, cancel)?;
    }
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), SatError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(SatError::Cancelled);
    }
    Ok(())
}

fn sleep_cancellable(total: Duration, cancel: &AtomicBool) -> Result<(), SatError> {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        check_cancel(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(Duration::from_millis(SLEEP_SLICE_MS)));
    }
    check_cancel(cancel)
}

fn to_send_sync(err: Box<dyn std::error::Error>) -> SatError {
    other(err.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn full_disk_c02_preview_is_bounded_even_when_stride_one_is_requested() {
        let stride = bounded_preview_stride(21_696, 21_696, 1);
        assert_eq!(stride, 8);
        assert!(
            21_696usize.div_ceil(stride) * 21_696usize.div_ceil(stride) <= MAXIMUM_PREVIEW_CELLS
        );
        assert_eq!(bounded_preview_stride(21_696, 21_696, 16), 16);
    }

    #[test]
    fn retained_native_manifest_primes_restart_dedup_without_a_preview() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rw-sat-native-dedup-{}-{nonce}",
            std::process::id()
        ));
        let frame_dir = root
            .join(crate::archive::NATIVE_SOURCE_ARCHIVE_DIR)
            .join("g18")
            .join("fulldisk")
            .join("20260823")
            .join("20260823T0200");
        std::fs::create_dir_all(&frame_dir).unwrap();
        let scan = Utc.with_ymd_and_hms(2026, 8, 23, 2, 0, 21).unwrap();
        let manifest = NativeSatelliteFrame {
            schema: crate::archive::NATIVE_FRAME_SCHEMA.to_string(),
            platform: "g18".to_string(),
            sector: "fulldisk".to_string(),
            frame_id: "20260823T0200".to_string(),
            scan_start_unix: scan.timestamp(),
            scan_end_unix: scan.timestamp() + 571,
            channels: std::collections::BTreeMap::from([(
                2,
                crate::archive::NativeChannelSource {
                    channel: 2,
                    object_key: "ABI-L2-CMIPF/2026/235/02/OR_ABI-L2-CMIPF-M6C02_G18_s20262350200211_e20262350209519_c20262350209578.nc".to_string(),
                    relative_path: ".rw-satellite-sources/g18/fulldisk/20260823/20260823T0200/c02.nc".to_string(),
                    byte_size: 335_834_639,
                    content_blake3: None,
                    scan_start_unix: scan.timestamp(),
                    scan_end_unix: scan.timestamp() + 571,
                },
            )]),
            l2_products: std::collections::BTreeMap::new(),
        };
        std::fs::write(
            frame_dir.join("frame.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();

        let seen = primed_seen_scans(&root, "g18", "fulldisk", &[2]);
        assert!(seen.contains(2, scan));
        assert!(!root.join("g18").exists(), "test must have no .rws preview");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn poll_prefixes_cover_hour_rollover_grace() {
        let satellite = GoesSatellite::G19;
        let mid_hour = Utc.with_ymd_and_hms(2026, 6, 10, 18, 30, 0).unwrap();
        let prefixes = poll_prefixes("ABI-L2-CMIPC", &satellite, 6, 13, mid_hour);
        assert_eq!(
            prefixes,
            vec!["ABI-L2-CMIPC/2026/161/18/OR_ABI-L2-CMIPC-M6C13_G19_"]
        );

        let just_rolled = Utc.with_ymd_and_hms(2026, 6, 10, 19, 2, 0).unwrap();
        let prefixes = poll_prefixes("ABI-L2-CMIPC", &satellite, 6, 13, just_rolled);
        assert_eq!(
            prefixes,
            vec![
                "ABI-L2-CMIPC/2026/161/18/OR_ABI-L2-CMIPC-M6C13_G19_",
                "ABI-L2-CMIPC/2026/161/19/OR_ABI-L2-CMIPC-M6C13_G19_",
            ]
        );

        let full_disk_still_finishing = Utc.with_ymd_and_hms(2026, 6, 10, 19, 10, 0).unwrap();
        let prefixes = poll_prefixes("ABI-L2-CMIPF", &satellite, 6, 13, full_disk_still_finishing);
        assert_eq!(
            prefixes.len(),
            2,
            "Full Disk keeps the prior complete scan available"
        );
        let full_disk_settled = Utc.with_ymd_and_hms(2026, 6, 10, 19, 15, 0).unwrap();
        assert_eq!(
            poll_prefixes("ABI-L2-CMIPF", &satellite, 6, 13, full_disk_settled).len(),
            1
        );

        // Day (and year-prefix) rollover comes for free from chrono.
        let new_day = Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap();
        let prefixes = poll_prefixes("ABI-L2-CMIPC", &satellite, 6, 13, new_day);
        assert_eq!(
            prefixes[0],
            "ABI-L2-CMIPC/2026/161/23/OR_ABI-L2-CMIPC-M6C13_G19_"
        );
        assert_eq!(
            prefixes[1],
            "ABI-L2-CMIPC/2026/162/00/OR_ABI-L2-CMIPC-M6C13_G19_"
        );
    }

    #[test]
    fn poll_delay_jitters_and_backs_off() {
        let base = Duration::from_secs(30);
        // unit_sample 0.5 -> no jitter.
        assert_eq!(poll_delay(base, 0.2, 0.5, 0), Duration::from_secs(30));
        // Extremes stay within +/- 20%.
        let low = poll_delay(base, 0.2, 0.0, 0);
        let high = poll_delay(base, 0.2, 1.0, 0);
        assert_eq!(low, Duration::from_secs(24));
        assert_eq!(high, Duration::from_secs(36));
        // Errors double the delay...
        assert_eq!(poll_delay(base, 0.0, 0.5, 1), Duration::from_secs(60));
        assert_eq!(poll_delay(base, 0.0, 0.5, 2), Duration::from_secs(120));
        // ... capped at 5 minutes.
        assert_eq!(poll_delay(base, 0.0, 0.5, 10), Duration::from_secs(300));
    }

    #[test]
    fn seen_scans_dedup_and_prune() {
        let mut seen = SeenScans::default();
        let t0 = Utc.with_ymd_and_hms(2026, 6, 10, 18, 51, 18).unwrap();
        assert!(seen.insert(13, t0));
        assert!(!seen.insert(13, t0), "second insert is a duplicate");
        assert!(seen.insert(2, t0), "same time, different band is distinct");
        assert!(seen.insert(13, t0 + chrono::Duration::minutes(5)));
        assert_eq!(seen.len(), 3);
        seen.prune_older_than(t0 + chrono::Duration::minutes(1));
        assert_eq!(seen.len(), 1, "old entries pruned");
        assert!(seen.contains(13, t0 + chrono::Duration::minutes(5)));
    }

    #[test]
    fn seen_scans_key_on_the_exact_second_and_ignore_filename_tenths() {
        let mut seen = SeenScans::default();
        let listed = Utc.with_ymd_and_hms(2026, 6, 10, 18, 51, 18).unwrap()
            + chrono::Duration::milliseconds(100);
        let manifest_start = Utc.with_ymd_and_hms(2026, 6, 10, 18, 51, 18).unwrap();
        assert!(seen.insert(13, manifest_start), "primed from a manifest");
        assert!(
            seen.contains(13, listed),
            "filename tenths dedup against the manifest's exact whole second"
        );
        assert!(!seen.insert(13, listed));
        assert!(
            !seen.contains(13, manifest_start + chrono::Duration::seconds(1)),
            "distinct starts in the same HHMM cannot be collapsed"
        );
    }

    #[test]
    fn legacy_preview_without_native_source_does_not_prime_restart_dedup() {
        use crate::store::test_support::{scan_start, synthetic_field};
        use crate::store::write_band_frame;

        let dir = std::env::temp_dir().join(format!("rw-sat-follow-prime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_band_frame(
            &dir,
            &synthetic_field(12, 10, scan_start(18, 51), 13, 0.0),
            1,
        )
        .unwrap();
        write_band_frame(
            &dir,
            &synthetic_field(12, 10, scan_start(18, 56), 13, 0.0),
            2,
        )
        .unwrap();
        write_band_frame(
            &dir,
            &synthetic_field(12, 10, scan_start(18, 51), 8, 0.0),
            3,
        )
        .unwrap();

        let seen = primed_seen_scans(&dir, "g19", "conus", &[13]);
        assert!(
            seen.is_empty(),
            "legacy `.rws` previews must not suppress exact native backfill"
        );
        assert!(!seen.contains(13, scan_start(18, 51)));
        assert!(!seen.contains(13, scan_start(18, 56)));
        assert!(!seen.contains(8, scan_start(18, 51)));

        let both = primed_seen_scans(&dir, "g19", "conus", &[8, 13]);
        assert!(both.is_empty());

        let missing = primed_seen_scans(&dir, "g18", "conus", &[13]);
        assert!(missing.is_empty(), "absent model dir primes nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    const TEST_PREFIX: &str = "ABI-L2-CMIPC/2026/161/18/OR_ABI-L2-CMIPC-M6C13_G19_";

    fn listed_object(key: impl Into<String>) -> S3Object {
        S3Object {
            key: key.into(),
            size_bytes: 1,
            last_modified: String::new(),
            etag: None,
        }
    }

    fn planned_object(band: u8, minute: u32) -> PolledFollowObject {
        planned_object_window(band, minute, 18, 0)
    }

    fn planned_object_window(
        band: u8,
        minute: u32,
        second: u32,
        end_offset_seconds: i64,
    ) -> PolledFollowObject {
        let start = Utc
            .with_ymd_and_hms(2026, 8, 27, 2, minute, second)
            .unwrap();
        PolledFollowObject {
            prefix: format!("band-{band}"),
            band,
            object: listed_object(format!("c{band:02}-{minute:02}")),
            scan_start: Some(start),
            scan_end: Some(start + chrono::Duration::seconds(300 + end_offset_seconds)),
            scan_created: Some(start + chrono::Duration::seconds(360)),
        }
    }

    fn planned_publication(
        band: u8,
        minute: u32,
        created_offset_seconds: i64,
        suffix: &str,
    ) -> PolledFollowObject {
        let mut listed = planned_object(band, minute);
        let start = listed.scan_start.expect("planned scan start");
        listed.object = listed_object(format!("c{band:02}-{minute:02}-{suffix}"));
        listed.scan_created = Some(start + chrono::Duration::seconds(created_offset_seconds));
        listed
    }

    fn planned_keys(plan: &ScanMajorPlan, objects: &[PolledFollowObject]) -> Vec<Vec<String>> {
        plan.batches
            .iter()
            .map(|batch| {
                batch
                    .iter()
                    .map(|&index| objects[index].object.key.clone())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn scan_major_plan_finishes_newest_product_before_history() {
        // Provider listings arrive band-major and oldest-first.
        let objects = vec![
            planned_object(1, 10),
            planned_object(1, 20),
            planned_object(2, 10),
            planned_object(2, 20),
            planned_object(3, 10),
            planned_object(3, 20),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert_eq!(
            planned_keys(&plan, &objects),
            vec![
                vec!["c01-20", "c02-20", "c03-20"],
                vec!["c01-10", "c02-10", "c03-10"],
            ]
        );
        assert!(plan.skipped_history.is_empty());
        assert!(plan.skipped_incomplete.is_empty());
    }

    #[test]
    fn scan_major_plan_selects_newest_publication_and_consumes_superseded_key() {
        let objects = vec![
            planned_publication(1, 20, 330, "original"),
            planned_publication(1, 20, 390, "corrected"),
            planned_publication(2, 20, 360, "only"),
            planned_publication(3, 20, 360, "only"),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert_eq!(
            planned_keys(&plan, &objects),
            vec![vec!["c01-20-corrected", "c02-20-only", "c03-20-only"]]
        );
        assert_eq!(plan.superseded, vec![0]);

        let consumed = plan
            .superseded
            .iter()
            .map(|&index| objects[index].object.key.clone())
            .collect::<HashSet<_>>();
        let mut last_key = HashMap::new();
        advance_consumed_watermarks(&objects, &consumed, &mut last_key);
        assert_eq!(
            last_key.get("band-1"),
            Some(&objects[0].object.key),
            "the obsolete publication advances the watermark up to the selected correction"
        );
    }

    #[test]
    fn selected_correction_end_time_overrides_stale_seen_metadata() {
        let correction = planned_object_window(3, 20, 18, 3);
        let start = correction.scan_start.expect("correction start");
        let retained_end = start + chrono::Duration::seconds(300);
        let mut seen = SeenScans::default();
        for band in [1_u8, 2, 3] {
            seen.insert_object_window(band, start, retained_end, &format!("retained-c{band:02}"));
        }

        let plan = scan_major_plan(&[correction], &[1, 2, 3], &seen, true);
        assert!(
            plan.batches.is_empty(),
            "the selected correction's out-of-tolerance end must not be hidden by stale seen data"
        );
    }

    #[test]
    fn newest_incomplete_scan_waits_while_newest_complete_scan_runs_first() {
        let objects = vec![
            planned_object(1, 20),
            planned_object(2, 20),
            planned_object(3, 20),
            planned_object(1, 30),
            planned_object(2, 30),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert_eq!(
            planned_keys(&plan, &objects),
            vec![vec!["c01-20", "c02-20", "c03-20"]]
        );
        assert!(
            [3_usize, 4].iter().all(|index| !plan
                .batches
                .iter()
                .flatten()
                .any(|item| item == index)),
            "newer partial scan remains pending for a future poll"
        );
    }

    #[test]
    fn same_minute_different_provider_starts_never_form_a_complete_batch() {
        let objects = vec![
            planned_object_window(1, 20, 18, 0),
            planned_object_window(2, 20, 19, 0),
            planned_object_window(3, 20, 18, 0),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert!(plan.batches.is_empty());
        assert!(plan.skipped_history.is_empty());
    }

    #[test]
    fn provider_end_skew_beyond_archive_tolerance_is_not_complete() {
        let objects = vec![
            planned_object_window(1, 20, 18, 0),
            planned_object_window(2, 20, 18, 0),
            planned_object_window(3, 20, 18, 3),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert!(plan.batches.is_empty());

        let spread_around_first = vec![
            planned_object_window(1, 20, 18, 0),
            planned_object_window(2, 20, 18, -2),
            planned_object_window(3, 20, 18, 2),
        ];
        assert!(
            scan_major_plan(
                &spread_around_first,
                &[1, 2, 3],
                &SeenScans::default(),
                true,
            )
            .batches
            .is_empty(),
            "the complete provider-end spread, not only distance from the first band, is bounded"
        );
    }

    #[test]
    fn retained_exact_channels_join_a_listed_remaining_channel() {
        let listed = planned_object_window(3, 20, 18, 1);
        let start = listed.scan_start.unwrap();
        let mut seen = SeenScans::default();
        seen.insert_window(1, start, start + chrono::Duration::seconds(300));
        seen.insert_window(2, start, start + chrono::Duration::seconds(300));
        let objects = vec![listed];

        let plan = scan_major_plan(&objects, &[1, 2, 3], &seen, true);

        assert_eq!(planned_keys(&plan, &objects), vec![vec!["c03-20"]]);
    }

    #[test]
    fn partial_success_retries_remaining_channels_without_restart() {
        let retained = planned_object_window(1, 20, 18, 0);
        let start = retained.scan_start.unwrap();
        let end = retained.scan_end.unwrap();
        let mut seen = SeenScans::default();
        seen.insert_window(1, start, end);
        let remaining = vec![
            planned_object_window(2, 20, 18, 0),
            planned_object_window(3, 20, 18, 1),
        ];

        let next_poll = scan_major_plan(&remaining, &[1, 2, 3], &seen, true);

        assert_eq!(
            planned_keys(&next_poll, &remaining),
            vec![vec!["c02-20", "c03-20"]]
        );
    }

    #[test]
    fn no_backfill_consumes_older_history_but_not_newer_partial_scan() {
        let objects = vec![
            planned_object(1, 10),
            planned_object(2, 10),
            planned_object(3, 10),
            planned_object(1, 20),
            planned_object(2, 20),
            planned_object(3, 20),
            planned_object(1, 30),
            planned_object(2, 30),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), false);

        assert_eq!(
            planned_keys(&plan, &objects),
            vec![vec!["c01-20", "c02-20", "c03-20"]]
        );
        assert_eq!(plan.skipped_history, vec![0, 1, 2]);
        assert!(plan.skipped_incomplete.is_empty());
        assert!(
            [6_usize, 7]
                .iter()
                .all(|index| !plan.skipped_history.contains(index)),
            "newer partial scan cannot be skipped or it would never complete"
        );
    }

    #[test]
    fn future_complete_scans_all_run_after_latest_only_bootstrap() {
        assert!(!backfill_enabled_for_poll(false, false));
        assert!(backfill_enabled_for_poll(false, true));
        let future = vec![
            planned_object(1, 30),
            planned_object(2, 30),
            planned_object(3, 30),
            planned_object(1, 40),
            planned_object(2, 40),
            planned_object(3, 40),
        ];

        let plan = scan_major_plan(
            &future,
            &[1, 2, 3],
            &SeenScans::default(),
            backfill_enabled_for_poll(false, true),
        );

        assert_eq!(
            planned_keys(&plan, &future),
            vec![
                vec!["c01-40", "c02-40", "c03-40"],
                vec!["c01-30", "c02-30", "c03-30"],
            ],
            "a poll gap after bootstrap must not collapse to newest-only"
        );
        assert!(plan.skipped_history.is_empty());
    }

    #[test]
    fn incomplete_scan_older_than_complete_history_does_not_pin_watermark() {
        let objects = vec![
            planned_object(1, 10),
            planned_object(1, 20),
            planned_object(2, 20),
            planned_object(3, 20),
        ];
        let plan = scan_major_plan(&objects, &[1, 2, 3], &SeenScans::default(), true);

        assert_eq!(
            planned_keys(&plan, &objects),
            vec![vec!["c01-20", "c02-20", "c03-20"]]
        );
        assert_eq!(plan.skipped_incomplete, vec![0]);
    }

    #[test]
    fn consumed_watermark_stops_before_retry_barrier() {
        let objects = vec![
            planned_object(1, 10),
            planned_object(1, 20),
            planned_object(1, 30),
        ];
        let consumed =
            HashSet::from([objects[0].object.key.clone(), objects[2].object.key.clone()]);
        let mut last_key = HashMap::new();

        advance_consumed_watermarks(&objects, &consumed, &mut last_key);

        assert_eq!(last_key.get("band-1"), Some(&objects[0].object.key));
    }

    /// A C13 CONUS key under [`TEST_PREFIX`] starting at 18:`minute`.
    fn c13_key(minute: u32) -> String {
        format!(
            "{TEST_PREFIX}s202616118{minute:02}176_e202616118{minute:02}549_c202616118{minute:02}590.nc"
        )
    }

    fn c13_start(minute: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 10, 18, minute, 17).unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn run_process(
        objects: &[S3Object],
        stale_cutoff: Option<DateTime<Utc>>,
        seen: &mut SeenScans,
        attempts: &mut HashMap<String, u32>,
        last_key: &mut HashMap<String, String>,
        ingest: &mut dyn FnMut(&S3Object) -> Result<(), SatError>,
    ) -> ProcessListedReport {
        let cancel = AtomicBool::new(false);
        process_listed_objects(
            TEST_PREFIX,
            objects,
            13,
            "ABI-L2-CMIPC",
            stale_cutoff,
            seen,
            attempts,
            last_key,
            &cancel,
            ingest,
        )
        .unwrap()
    }

    #[test]
    fn failed_ingest_holds_the_watermark_and_is_retried() {
        let objects = vec![listed_object(c13_key(51)), listed_object(c13_key(56))];
        let mut seen = SeenScans::default();
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();

        // First poll: the 18:51 ingest fails transiently.
        let mut fail_first = |object: &S3Object| -> Result<(), SatError> {
            if object.key == objects[0].key {
                Err(other("503 slow down"))
            } else {
                Ok(())
            }
        };
        let report = run_process(
            &objects,
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut fail_first,
        );
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("will retry"),
            "{}",
            report.warnings[0]
        );
        assert!(report.retained.is_empty());
        assert!(
            !last_key.contains_key(TEST_PREFIX),
            "watermark held before the failed key so the next poll re-lists it"
        );
        assert!(seen.is_empty(), "failures are never marked seen");
        assert_eq!(attempts.get(objects[0].key.as_str()), Some(&1));

        // Next poll re-lists both keys (held watermark) and succeeds.
        let mut ok = |_object: &S3Object| Ok(());
        let report = run_process(
            &objects,
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut ok,
        );
        assert!(report.warnings.is_empty());
        assert!(report.retained.is_empty());
        assert_eq!(seen.len(), 2, "both scans ingested after the retry");
        assert_eq!(
            last_key.get(TEST_PREFIX),
            Some(&objects[1].key),
            "watermark advanced through the last success"
        );
        assert!(attempts.is_empty(), "retry counter cleared on success");
    }

    #[test]
    fn poisoned_object_is_dropped_after_the_attempt_cap() {
        let objects = vec![listed_object(c13_key(51)), listed_object(c13_key(56))];
        let mut seen = SeenScans::default();
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();
        let mut ingest = |object: &S3Object| -> Result<(), SatError> {
            if object.key == objects[0].key {
                Err(other("truncated NetCDF"))
            } else {
                Ok(())
            }
        };

        for attempt in 1..MAX_INGEST_ATTEMPTS {
            let report = run_process(
                &objects,
                None,
                &mut seen,
                &mut attempts,
                &mut last_key,
                &mut ingest,
            );
            assert!(
                report.warnings[0].contains("will retry"),
                "{}",
                report.warnings[0]
            );
            assert!(report.retained.is_empty());
            assert!(!last_key.contains_key(TEST_PREFIX));
            assert_eq!(attempts.get(objects[0].key.as_str()), Some(&attempt));
            assert!(
                seen.is_empty(),
                "the good key stays blocked behind the bad one"
            );
        }

        // Final attempt: give up on the bad object, unblock the prefix.
        let report = run_process(
            &objects,
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut ingest,
        );
        assert_eq!(report.warnings.len(), 1);
        assert!(
            report.warnings[0].contains("giving up"),
            "{}",
            report.warnings[0]
        );
        assert!(report.retained.is_empty());
        assert!(attempts.is_empty(), "no counter leak after giving up");
        assert_eq!(seen.len(), 1, "the 18:56 frame finally ingested");
        assert!(seen.contains(13, c13_start(56)));
        assert_eq!(
            last_key.get(TEST_PREFIX),
            Some(&objects[1].key),
            "watermark moved past both keys"
        );
    }

    #[test]
    fn skipped_objects_advance_the_watermark_without_ingest() {
        let stale_cutoff = Utc.with_ymd_and_hms(2026, 6, 10, 18, 45, 0).unwrap();
        let already_seen = c13_key(51);
        let objects = vec![
            // Sidecar / non-NetCDF object.
            listed_object(format!("{TEST_PREFIX}manifest.json")),
            // Wrong band under a sibling prefix page.
            listed_object(
                "ABI-L2-CMIPC/2026/161/18/OR_ABI-L2-CMIPC-M6C08_G19_s20261611846176_e20261611848549_c20261611849020.nc",
            ),
            // Older than the rolling window: churn if ingested.
            listed_object(c13_key(41)),
            // Already in the store (restart priming or an earlier poll).
            listed_object(already_seen.clone()),
        ];
        let mut seen = SeenScans::default();
        seen.insert(13, c13_start(51));
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();
        let mut ingest_calls = 0usize;
        let mut ingest = |_object: &S3Object| -> Result<(), SatError> {
            ingest_calls += 1;
            Ok(())
        };

        let report = run_process(
            &objects,
            Some(stale_cutoff),
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut ingest,
        );
        assert!(report.warnings.is_empty());
        assert_eq!(
            report.retained,
            vec![listed_object(already_seen.clone())],
            "only the filtered, current scan is reported as retained"
        );
        assert_eq!(ingest_calls, 0, "every object was skipped on purpose");
        assert_eq!(
            last_key.get(TEST_PREFIX),
            Some(&already_seen),
            "skips advance the watermark so they are never re-listed"
        );
    }

    #[test]
    fn mixed_listing_reports_retained_scan_and_ingests_only_new_scan() {
        let retained = listed_object(c13_key(51));
        let fresh = listed_object(c13_key(56));
        let objects = vec![retained.clone(), fresh.clone()];
        let mut seen = SeenScans::default();
        seen.insert(13, c13_start(51));
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();
        let mut ingested = Vec::new();
        let mut ingest = |object: &S3Object| -> Result<(), SatError> {
            ingested.push(object.key.clone());
            Ok(())
        };

        let report = run_process(
            &objects,
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut ingest,
        );

        assert!(report.warnings.is_empty());
        assert_eq!(report.retained, vec![retained]);
        assert_eq!(ingested, vec![fresh.key.clone()]);
        assert_eq!(seen.len(), 2);
        assert_eq!(last_key.get(TEST_PREFIX), Some(&fresh.key));
    }

    #[test]
    fn changed_object_key_reingests_same_scan_correction() {
        let retained = listed_object(c13_key(51));
        let corrected = listed_object(retained.key.replace("590.nc", "591.nc"));
        let start = c13_start(51);
        let end = start + chrono::Duration::seconds(37);
        let mut seen = SeenScans::default();
        seen.insert_object_window(13, start, end, &retained.key);
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();
        let mut ingested = Vec::new();
        let mut ingest = |object: &S3Object| -> Result<(), SatError> {
            ingested.push(object.key.clone());
            Ok(())
        };

        let report = run_process(
            &[retained.clone(), corrected.clone()],
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &mut ingest,
        );

        assert_eq!(report.retained, vec![retained.clone()]);
        assert_eq!(ingested, vec![corrected.key.clone()]);
        assert!(seen.contains_object(13, start, &corrected.key));
        assert!(!seen.contains_object(13, start, &retained.key));
        assert_eq!(seen.len(), 1, "the correction replaces the same scan slot");
        assert_eq!(last_key.get(TEST_PREFIX), Some(&corrected.key));
    }

    #[test]
    fn cancel_mid_listing_propagates() {
        let cancel = AtomicBool::new(true);
        let objects = vec![listed_object(c13_key(51))];
        let mut seen = SeenScans::default();
        let mut attempts = HashMap::new();
        let mut last_key = HashMap::new();
        let mut ingest = |_object: &S3Object| -> Result<(), SatError> { Ok(()) };
        let err = process_listed_objects(
            TEST_PREFIX,
            &objects,
            13,
            "ABI-L2-CMIPC",
            None,
            &mut seen,
            &mut attempts,
            &mut last_key,
            &cancel,
            &mut ingest,
        )
        .unwrap_err();
        assert!(err.is_cancelled());
        assert!(last_key.is_empty(), "nothing consumed after cancel");
    }

    #[test]
    fn jitter_rng_is_deterministic_and_in_range() {
        let mut a = JitterRng::new(42);
        let mut b = JitterRng::new(42);
        for _ in 0..100 {
            let sample = a.next_unit();
            assert_eq!(sample, b.next_unit());
            assert!((0.0..1.0).contains(&sample), "sample {sample}");
        }
    }

    #[test]
    fn cancel_flag_stops_sleep_and_follow() {
        let cancel = AtomicBool::new(true);
        let err = sleep_cancellable(Duration::from_secs(5), &cancel).unwrap_err();
        assert!(err.is_cancelled());

        let config = FollowConfig::new("goes19", Sector::Conus, vec![13]);
        let mut events = Vec::new();
        let result = follow(&config, &mut |event| events.push(event), &cancel);
        assert!(result.is_err_and(|err| err.is_cancelled()));
    }

    #[test]
    fn follow_rejects_empty_or_invalid_bands() {
        let cancel = AtomicBool::new(false);
        let mut sink = |_event: SatEvent| {};
        let empty = FollowConfig::new("goes19", Sector::Conus, vec![]);
        assert!(follow(&empty, &mut sink, &cancel).is_err());
        let bad = FollowConfig::new("goes19", Sector::Conus, vec![17]);
        assert!(follow(&bad, &mut sink, &cancel).is_err());
    }
}

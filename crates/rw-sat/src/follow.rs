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

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use chrono::{DateTime, Timelike, Utc};

use crate::events::{SatError, SatEvent, other};
use crate::goes::{GoesSatellite, parse_goes_abi_filename};
use crate::s3::{
    DownloadedObject, S3Object, Sector, abi_filename_product_matches_request, band_hour_prefix,
    bucket_for_satellite, build_agent, download_object, list_s3_objects, object_filename,
};
use crate::store::{WrittenFrame, downsample_field, write_band_frame};
use crate::window::{WindowConfig, enforce_window};
use crate::abi::read_goes_abi_field;

/// Minutes after the top of the hour during which the previous hour's
/// prefix keeps being polled (stragglers + clock skew).
const HOUR_ROLLOVER_GRACE_MINUTES: u32 = 5;
/// Backoff cap after consecutive poll errors.
const MAX_BACKOFF_SECS: u64 = 300;
/// Cancel-flag check granularity while sleeping.
const SLEEP_SLICE_MS: u64 = 100;

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
    /// Per-band stride decimation before storing (1 = native).
    pub downsample: usize,
    pub window: WindowConfig,
    /// Stop after this many poll cycles (`None` = run until cancelled).
    pub max_polls: Option<u32>,
    /// Stop once this many frames have been ingested.
    pub max_frames: Option<u32>,
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
            downsample: 1,
            window: WindowConfig::default(),
            max_polls: None,
            max_frames: None,
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
    if now.minute() < HOUR_ROLLOVER_GRACE_MINUTES {
        let previous = now - chrono::Duration::hours(1);
        prefixes.push(band_hour_prefix(abi_product, satellite, mode, band, previous));
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

/// Bounded dedup of already-ingested scans, keyed by (band, scan start).
/// Re-listed keys (page overlap, prefix rollover) are dropped here even if
/// `start-after` state was lost.
#[derive(Debug, Default)]
pub struct SeenScans {
    seen: BTreeSet<(u8, DateTime<Utc>)>,
}

impl SeenScans {
    /// Record (band, start). Returns `false` when already seen.
    pub fn insert(&mut self, band: u8, start: DateTime<Utc>) -> bool {
        self.seen.insert((band, start))
    }

    pub fn contains(&self, band: u8, start: DateTime<Utc>) -> bool {
        self.seen.contains(&(band, start))
    }

    /// Drop entries older than `cutoff` (call with `now - window`).
    pub fn prune_older_than(&mut self, cutoff: DateTime<Utc>) {
        self.seen.retain(|&(_, start)| start >= cutoff);
    }

    pub fn len(&self) -> usize {
        self.seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.seen.is_empty()
    }
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

/// Download one object and ingest it as a store frame. Shared by the
/// follow loop and the one-shot `latest` CLI flow.
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
) -> Result<(DownloadedObject, WrittenFrame), SatError> {
    sink(SatEvent::DownloadStarted {
        key: object.key.clone(),
        bytes: object.size_bytes,
    });
    let started = Instant::now();
    let download =
        download_object(agent, bucket, cache_dir, object, use_cache).map_err(to_send_sync)?;
    sink(SatEvent::DownloadDone {
        key: object.key.clone(),
        bytes: object.size_bytes,
        ms: started.elapsed().as_millis(),
        cache_hit: download.cache_hit,
    });

    let field = read_goes_abi_field(&download.path, "CMI").map_err(to_send_sync)?;
    let field = downsample_field(field, downsample);
    let frame = write_band_frame(store_root, &field, written_unix).map_err(to_send_sync)?;
    sink(SatEvent::FrameWritten {
        model: frame.model.clone(),
        run: frame.run.clone(),
        hhmm: frame.hhmm,
        scan_time_utc: frame.scan_time_utc,
        path: frame.path.clone(),
        bytes: frame.bytes,
        encode_ms: frame.encode_ms,
    });
    Ok((download, frame))
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
    let mut seen = SeenScans::default();
    // start-after state per (band, hour prefix).
    let mut last_key: HashMap<String, String> = HashMap::new();
    let mut consecutive_errors: u32 = 0;

    loop {
        check_cancel(cancel)?;
        let mut poll_failed = false;
        for &band in &config.bands {
            check_cancel(cancel)?;
            let now = Utc::now();
            let prefixes = poll_prefixes(abi_product, &satellite, config.mode, band, now);
            sink(SatEvent::PollStarted {
                band,
                prefixes: prefixes.clone(),
            });
            let poll_started = Instant::now();
            let mut new_objects: Vec<S3Object> = Vec::new();
            for prefix in &prefixes {
                let start_after = last_key.get(prefix).map(String::as_str);
                match list_s3_objects(&agent, &bucket, prefix, start_after) {
                    Ok(objects) => {
                        if let Some(last) = objects.last() {
                            last_key.insert(prefix.clone(), last.key.clone());
                        }
                        new_objects.extend(objects);
                    }
                    Err(err) => {
                        poll_failed = true;
                        sink(SatEvent::Warning {
                            message: format!("list {prefix}: {err}"),
                        });
                    }
                }
            }
            // Keep the start-after map small: only the prefixes still in
            // rotation matter.
            last_key.retain(|prefix, _| {
                config
                    .bands
                    .iter()
                    .any(|&b| poll_prefixes(abi_product, &satellite, config.mode, b, now).contains(prefix))
            });

            let mut ingested_this_poll = 0usize;
            for object in &new_objects {
                check_cancel(cancel)?;
                if !object.key.ends_with(".nc") {
                    continue;
                }
                let Ok(parsed) = parse_goes_abi_filename(object_filename(&object.key)) else {
                    continue;
                };
                if !abi_filename_product_matches_request(&parsed.product, abi_product)
                    || parsed.channel != Some(band)
                {
                    continue;
                }
                if !seen.insert(band, parsed.start_time_utc) {
                    continue;
                }
                let written_unix = Utc::now().timestamp().max(0) as u64;
                match fetch_and_ingest(
                    &agent,
                    &bucket,
                    &config.cache_dir,
                    &config.store_root,
                    object,
                    config.downsample,
                    config.use_cache,
                    written_unix,
                    sink,
                ) {
                    Ok((_download, frame)) => {
                        summary.downloaded_keys.push(object.key.clone());
                        // Scope eviction to this followed band's runs.
                        let run_prefix = format!("{}_c{band:02}", config.sector.slug());
                        summary.frames.push(frame.clone());
                        ingested_this_poll += 1;
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
                    Err(SatError::Cancelled) => return Err(SatError::Cancelled),
                    Err(err) => {
                        poll_failed = true;
                        sink(SatEvent::Warning {
                            message: format!("ingest {}: {err}", object.key),
                        });
                    }
                }
            }
            sink(SatEvent::PollDone {
                band,
                new_keys: ingested_this_poll,
                ms: poll_started.elapsed().as_millis(),
            });
        }
        // Dedup memory stays bounded: anything older than a day is gone
        // from the hour prefixes we poll anyway.
        seen.prune_older_than(Utc::now() - chrono::Duration::days(1));

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
            .is_some_and(|max| summary.frames.len() as u32 >= max)
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

        // Day (and year-prefix) rollover comes for free from chrono.
        let new_day = Utc.with_ymd_and_hms(2026, 6, 11, 0, 0, 0).unwrap();
        let prefixes = poll_prefixes("ABI-L2-CMIPC", &satellite, 6, 13, new_day);
        assert_eq!(prefixes[0], "ABI-L2-CMIPC/2026/161/23/OR_ABI-L2-CMIPC-M6C13_G19_");
        assert_eq!(prefixes[1], "ABI-L2-CMIPC/2026/162/00/OR_ABI-L2-CMIPC-M6C13_G19_");
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

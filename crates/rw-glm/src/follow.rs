//! The GLM follow engine: poll the live GOES GLM-L2-LCFA bucket, fetch new
//! granules as they land, decode them (Task 2), write their flashes into the
//! rolling `.rwl` store, and prune buckets that age out — with typed
//! [`GlmEvent`] progress, a cancel flag, restart-safe granule-key dedup, and a
//! transient-failure retry holdback.
//!
//! Architecture mirrors `rw-sat`'s `follow.rs`:
//! - **Paginated `start-after` listing** under the hour prefix (and the prior
//!   hour during the first few minutes after the top of the hour, so a
//!   straggler or local clock skew never drops a granule). See [`GranuleSource`].
//! - **Restart-safe dedup**: the set of already-ingested granule keys is
//!   persisted in `window.json` ([`crate::store::WindowManifest::seen_granule_keys`],
//!   capped) and seeded on startup. The header `source_granule_count` is a
//!   provenance count, *not* dedup state.
//! - **Retry holdback**: a granule whose fetch/decode fails *transiently* goes
//!   into a holdback with an attempt count and a next-retry time; a *permanent*
//!   failure (a decode [`Format`](crate::RwlError::Format) error) is recorded as
//!   skipped and never retried.
//! - **Cancel token**: an [`AtomicBool`] checked at every boundary.
//!
//! ## The clock and FORMAT decisions
//!
//! The loop reads the wall clock only for *cadence* (poll sleep), for the
//! current/previous **hour prefix** to list, and for the **window cutoff**
//! (`now - window`). It never derives a bucket name or a record timestamp from
//! the clock — those come from the flashes' own first-event times (Task 2). A
//! live follow engine is, by nature, not a deterministic writer; the
//! determinism that matters (record layout, bucket placement) is clock-free.
//!
//! ## `GranuleSource` and testability
//!
//! The engine is written against the [`GranuleSource`] trait, not the S3 client
//! directly, exactly as rw-sat factors its fetch/ingest behind a closure so its
//! follow logic can be exercised offline. Tests feed an in-memory source of
//! synthetic [`DecodedGranule`]s (no network, no NetCDF files); the live
//! [`S3GranuleSource`] is one implementor that lists S3, downloads to a scratch
//! file, and calls [`crate::decode_granule`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::FlashRecord;
use crate::granule::{DecodedGranule, decode_granule};
use crate::s3::{
    S3Object, ScratchFile, bucket_for_satellite, build_agent, download_object_to, glm_hour_prefix,
    list_s3_objects, object_filename,
};
use crate::store::BucketWriter;
use crate::window::{PruneReport, WindowConfig, enforce_window};

/// Default poll cadence (seconds): GLM granules land roughly every 20 s.
pub const DEFAULT_POLL_SECS: u64 = 20;
/// Default rolling window: 2 hours (matches radar-loop spans / the spec).
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// Minutes after the top of each UTC hour during which the previous hour's
/// prefix is also polled (stragglers + clock skew). Mirrors rw-sat's
/// `HOUR_ROLLOVER_GRACE_MINUTES`.
const HOUR_ROLLOVER_GRACE_MINUTES: u32 = 5;
/// Holdback policy: how long a transiently failed granule waits before its
/// next retry (rw-sat spreads retries over poll backoff; here the holdback is
/// an explicit per-granule timer).
const HOLDBACK_BASE: Duration = Duration::from_secs(20);
/// Cap on the holdback so a long-lived transient never starves a granule.
const HOLDBACK_MAX: Duration = Duration::from_secs(300);
/// Transient-failure attempts before a granule is given up on (recorded
/// skipped). Mirrors rw-sat's `MAX_INGEST_ATTEMPTS`.
const MAX_TRANSIENT_ATTEMPTS: u32 = 5;
/// Cancel-flag check granularity while sleeping.
const SLEEP_SLICE: Duration = Duration::from_millis(100);

/// Why a granule was skipped (carried by [`GlmEvent::GranuleSkipped`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Already ingested (in the persisted/seeded dedup set) — a no-op.
    AlreadySeen,
    /// Permanently un-decodable (a decode Format error); never retried.
    PermanentDecodeError,
    /// Exceeded the transient-retry cap; given up on.
    RetriesExhausted,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::AlreadySeen => "already_seen",
            SkipReason::PermanentDecodeError => "permanent_decode_error",
            SkipReason::RetriesExhausted => "retries_exhausted",
        }
    }
}

/// One progress event from the GLM follow flow. CamelCase variant names
/// consistent with rw-sat's `SatEvent`.
#[derive(Debug, Clone)]
pub enum GlmEvent {
    /// A listing pass started over `prefixes`.
    Listing {
        prefixes: Vec<String>,
    },
    /// A granule's bytes were fetched (`bytes` is the listed S3 size).
    GranuleFetched {
        key: String,
        bytes: u64,
    },
    /// A granule decoded into `flashes` flashes.
    GranuleDecoded {
        key: String,
        flashes: usize,
    },
    /// A bucket was (re)written with `records` total records.
    BucketWritten {
        path: PathBuf,
        records: usize,
    },
    /// A granule was skipped for `reason`.
    GranuleSkipped {
        key: String,
        reason: SkipReason,
    },
    /// The rolling window pruned buckets.
    Pruned {
        report: PruneReport,
    },
    /// The next poll was delayed `secs` seconds (for countdown UIs).
    PollSleep {
        secs: u64,
    },
    Info {
        message: String,
    },
    Warning {
        message: String,
    },
}

/// The bins' sink: human-readable lines, warnings to stderr.
pub fn print_event(event: &GlmEvent) {
    match event {
        GlmEvent::Listing { prefixes } => println!("list: {}", prefixes.join(" + ")),
        GlmEvent::GranuleFetched { key, bytes } => println!("fetched {key} ({bytes} bytes)"),
        GlmEvent::GranuleDecoded { key, flashes } => println!("decoded {key}: {flashes} flash(es)"),
        GlmEvent::BucketWritten { path, records } => {
            println!("bucket {} <- {records} record(s)", path.display())
        }
        GlmEvent::GranuleSkipped { key, reason } => {
            println!("skipped {key} ({})", reason.as_str())
        }
        GlmEvent::Pruned { report } => println!(
            "pruned {} bucket(s) / {} bytes ({} date dir(s) removed)",
            report.removed_buckets,
            report.removed_bytes,
            report.removed_date_dirs.len()
        ),
        GlmEvent::PollSleep { secs } => println!("sleeping {secs} s"),
        GlmEvent::Info { message } => println!("{message}"),
        GlmEvent::Warning { message } => eprintln!("{message}"),
    }
}

/// Errors from the follow flow. `Cancelled` is the variant callers match on
/// (the cancel flag was observed at a boundary). Mirrors rw-sat's `SatError`.
#[derive(Debug, thiserror::Error)]
pub enum GlmError {
    #[error("glm follow cancelled")]
    Cancelled,
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

impl GlmError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, GlmError::Cancelled)
    }
}

fn other(err: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> GlmError {
    GlmError::Other(err.into())
}

/// Classification of a granule-fetch failure, deciding holdback vs record-skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchErrorKind {
    /// A transient failure (network blip, truncated read, S3 5xx): retry after
    /// the holdback elapses.
    Transient,
    /// A permanent failure (the granule decoded to a Format error — corrupt or
    /// non-GLM bytes): never retry; record skipped.
    Permanent,
}

/// A granule-fetch failure carrying its retry classification.
#[derive(Debug)]
pub struct FetchError {
    pub kind: FetchErrorKind,
    pub message: String,
}

impl FetchError {
    pub fn transient(message: impl Into<String>) -> Self {
        Self {
            kind: FetchErrorKind::Transient,
            message: message.into(),
        }
    }
    pub fn permanent(message: impl Into<String>) -> Self {
        Self {
            kind: FetchErrorKind::Permanent,
            message: message.into(),
        }
    }
}

/// The source of granules the follow engine ingests. The live S3 path is one
/// implementor ([`S3GranuleSource`]); tests provide an in-memory source so the
/// engine's dedup/holdback/prune logic runs offline.
pub trait GranuleSource {
    /// List the granule keys available under `prefix`, optionally only those
    /// lexicographically after `start_after` (the incremental primitive). Keys
    /// must sort chronologically (they do for GLM's `s…` token).
    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> Result<Vec<ListedGranule>, FetchError>;

    /// Fetch and decode the granule named by `listed` into a [`DecodedGranule`].
    /// A [`FetchError`] classifies the failure as transient (holdback+retry) or
    /// permanent (record-skip).
    fn fetch(&self, listed: &ListedGranule) -> Result<DecodedGranule, FetchError>;
}

/// A listed-but-not-yet-fetched granule: its key plus the listed byte size (for
/// the `GranuleFetched` event). The key sorts chronologically within a prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedGranule {
    pub key: String,
    pub bytes: u64,
}

impl ListedGranule {
    /// The dedup identity: the filename stem (no dir, no `.nc`), matching
    /// [`DecodedGranule::granule_key`].
    pub fn dedup_key(&self) -> String {
        let name = object_filename(&self.key);
        name.strip_suffix(".nc").unwrap_or(name).to_string()
    }
}

/// The live S3 implementor: list the GLM bucket, download a granule to a scratch
/// file, decode it. A decode `Format` error is classified **permanent**; an S3
/// or I/O error is **transient**.
pub struct S3GranuleSource {
    agent: ureq::Agent,
    bucket: String,
    scratch_dir: PathBuf,
}

impl S3GranuleSource {
    /// Build a source for `satellite`, downloading scratch granules under
    /// `scratch_dir` (created on demand).
    pub fn new(satellite: &str, scratch_dir: PathBuf) -> Result<Self, GlmError> {
        let bucket = bucket_for_satellite(satellite).map_err(to_send_sync)?;
        Ok(Self {
            agent: build_agent(),
            bucket,
            scratch_dir,
        })
    }
}

impl GranuleSource for S3GranuleSource {
    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> Result<Vec<ListedGranule>, FetchError> {
        let objects = list_s3_objects(&self.agent, &self.bucket, prefix, start_after)
            .map_err(|e| FetchError::transient(format!("list {prefix}: {e}")))?;
        Ok(objects
            .into_iter()
            .filter(|o| o.key.ends_with(".nc"))
            .map(
                |S3Object {
                     key, size_bytes, ..
                 }| ListedGranule {
                    key,
                    bytes: size_bytes,
                },
            )
            .collect())
    }

    fn fetch(&self, listed: &ListedGranule) -> Result<DecodedGranule, FetchError> {
        std::fs::create_dir_all(&self.scratch_dir)
            .map_err(|e| FetchError::transient(format!("scratch dir: {e}")))?;
        let scratch = ScratchFile::new(&self.scratch_dir, &listed.key);
        let object = S3Object {
            key: listed.key.clone(),
            size_bytes: listed.bytes,
            last_modified: String::new(),
        };
        download_object_to(&self.agent, &self.bucket, &object, scratch.path())
            .map_err(|e| FetchError::transient(format!("download {}: {e}", listed.key)))?;
        // A decode failure is a Format error = permanent (corrupt/non-GLM bytes);
        // the bytes will not get better on a retry. Other RwlErrors (I/O) are
        // transient.
        decode_granule(scratch.path()).map_err(|e| match e {
            crate::RwlError::Format(msg) => FetchError::permanent(msg),
            other => FetchError::transient(other.to_string()),
        })
    }
}

/// Configuration for a follow session.
#[derive(Debug, Clone)]
pub struct GlmFollowSpec {
    /// Satellite name (`goes19`, `g18`, ...).
    pub satellite: String,
    /// Poll cadence (default [`DEFAULT_POLL_SECS`]).
    pub poll_secs: u64,
    /// Rolling window (default [`DEFAULT_WINDOW`]).
    pub window: Duration,
    /// Optional total-byte budget for the rolling window (oldest-first eviction
    /// once exceeded). `None` = age-only.
    pub byte_budget: Option<u64>,
    /// Store root; buckets land under `<root>/glm/<satellite>/...`.
    pub store_root: PathBuf,
    /// Stop after this many poll cycles (`None` = until cancelled).
    pub max_polls: Option<u32>,
}

impl GlmFollowSpec {
    /// A spec with the documented defaults (20 s cadence, 2 h window, no byte
    /// budget, run until cancelled).
    pub fn new(satellite: &str, store_root: PathBuf) -> Self {
        Self {
            satellite: satellite.to_string(),
            poll_secs: DEFAULT_POLL_SECS,
            window: DEFAULT_WINDOW,
            byte_budget: None,
            store_root,
            max_polls: None,
        }
    }

    fn window_config(&self) -> WindowConfig {
        WindowConfig {
            max_age_ms: Some(self.window.as_millis().min(i64::MAX as u128) as i64),
            max_bytes: self.byte_budget,
        }
    }
}

/// What a follow session did.
#[derive(Debug, Default)]
pub struct FollowSummary {
    pub polls: u32,
    pub ingested_granules: usize,
    pub ingested_flashes: usize,
    pub skipped_granules: usize,
    pub pruned_buckets: usize,
}

/// A granule in the transient-retry holdback: attempts so far and the earliest
/// instant to retry it.
#[derive(Debug, Clone)]
struct Holdback {
    attempts: u32,
    next_retry: Instant,
}

/// Compute the holdback delay for the `attempts`-th transient failure:
/// exponential from [`HOLDBACK_BASE`], capped at [`HOLDBACK_MAX`]. Pure.
fn holdback_delay(attempts: u32) -> Duration {
    let factor = 2u32.saturating_pow(attempts.saturating_sub(1).min(16));
    let secs = HOLDBACK_BASE
        .as_secs()
        .saturating_mul(u64::from(factor))
        .min(HOLDBACK_MAX.as_secs());
    Duration::from_secs(secs.max(1))
}

/// Current Unix time in milliseconds (clock read for cadence + window cutoff
/// only — never for a FORMAT decision).
fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0)
}

/// Civil `(year, day_of_year, hour)` for a Unix-ms instant, for building the
/// GLM hour prefix. Self-contained (no chrono) via the format module's date
/// math.
fn utc_year_doy_hour(unix_ms: i64) -> (i64, u32, u32) {
    let day = unix_ms.div_euclid(86_400_000);
    let into_day_ms = unix_ms - day * 86_400_000;
    let hour = (into_day_ms / 3_600_000) as u32;
    let date = crate::format::date_dir(unix_ms); // "YYYYMMDD"
    let year: i64 = date[0..4].parse().unwrap_or(1970);
    // day-of-year = this day minus Jan-1-of-this-year's day count, +1.
    let jan1 = crate::format::days_from_civil(year, 1, 1).unwrap_or(day);
    let doy = (day - jan1 + 1).clamp(1, 366) as u32;
    (year, doy, hour)
}

/// The hour prefixes one poll must cover: the current hour, preceded by the
/// previous hour during the first [`HOUR_ROLLOVER_GRACE_MINUTES`]. Pure for
/// testing (takes the instant).
pub fn poll_prefixes(now_unix_ms: i64) -> Vec<String> {
    let (year, doy, hour) = utc_year_doy_hour(now_unix_ms);
    let into_hour_min = ((now_unix_ms.div_euclid(60_000)) % 60) as u32;
    let mut prefixes = Vec::with_capacity(2);
    if into_hour_min < HOUR_ROLLOVER_GRACE_MINUTES {
        let prev = now_unix_ms - 3_600_000;
        let (py, pdoy, ph) = utc_year_doy_hour(prev);
        prefixes.push(glm_hour_prefix(py, pdoy, ph));
    }
    prefixes.push(glm_hour_prefix(year, doy, hour));
    prefixes
}

fn check_cancel(cancel: &AtomicBool) -> Result<(), GlmError> {
    if cancel.load(Ordering::Relaxed) {
        return Err(GlmError::Cancelled);
    }
    Ok(())
}

fn sleep_cancellable(total: Duration, cancel: &AtomicBool) -> Result<(), GlmError> {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        check_cancel(cancel)?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(SLEEP_SLICE));
    }
    check_cancel(cancel)
}

fn to_send_sync(err: Box<dyn std::error::Error>) -> GlmError {
    other(err.to_string())
}

/// Run a follow session driven by `source`. Returns when `max_polls` is reached
/// or, with no cap, only on cancel ([`GlmError::Cancelled`]).
///
/// This is the testable core: a synthetic [`GranuleSource`] exercises the whole
/// dedup/holdback/write/prune pipeline offline. [`follow_live`] is the thin
/// wrapper that builds an [`S3GranuleSource`].
pub fn follow_with_source(
    spec: &GlmFollowSpec,
    source: &dyn GranuleSource,
    sink: &mut dyn FnMut(GlmEvent),
    cancel: &AtomicBool,
) -> Result<FollowSummary, GlmError> {
    let mut summary = FollowSummary::default();
    let window = spec.window_config();

    // Restart-safe dedup: seed the seen-set from window.json. Opening a writer
    // briefly to read the manifest also creates the store dir; we drop it right
    // away so nothing holds the lock outside the write phase.
    let mut seen = SeenGranules::default();
    {
        let writer = BucketWriter::open(&spec.store_root, &spec.satellite)
            .map_err(|e| other(e.to_string()))?;
        for key in writer.load_manifest().seen_granule_keys {
            seen.insert(key);
        }
    }
    if !seen.is_empty() {
        sink(GlmEvent::Info {
            message: format!(
                "dedup seeded from window.json: {} granule key(s) already ingested",
                seen.len()
            ),
        });
    }

    // start-after watermark per hour prefix, and the per-granule holdback.
    let mut last_key: HashMap<String, String> = HashMap::new();
    let mut holdbacks: HashMap<String, Holdback> = HashMap::new();

    loop {
        check_cancel(cancel)?;
        let now_ms = now_unix_ms();
        let stale_cutoff = now_ms - window.max_age_ms.unwrap_or(i64::MAX);

        let prefixes = poll_prefixes(now_ms);
        sink(GlmEvent::Listing {
            prefixes: prefixes.clone(),
        });

        // Phase 1: list + fetch + write, holding the writer lock for the whole
        // write phase, then drop it BEFORE pruning (lock lifetimes never
        // overlap — ingest-then-enforce, single threaded).
        {
            let mut writer = BucketWriter::open(&spec.store_root, &spec.satellite)
                .map_err(|e| other(e.to_string()))?;
            for prefix in &prefixes {
                check_cancel(cancel)?;
                let start_after = last_key.get(prefix).cloned();
                let listed = match source.list(prefix, start_after.as_deref()) {
                    Ok(listed) => listed,
                    Err(err) => {
                        sink(GlmEvent::Warning {
                            message: format!("list {prefix}: {}", err.message),
                        });
                        continue;
                    }
                };
                for granule in listed {
                    check_cancel(cancel)?;
                    let flow = process_one_granule(
                        prefix,
                        &granule,
                        stale_cutoff,
                        source,
                        &mut writer,
                        &mut seen,
                        &mut holdbacks,
                        &mut last_key,
                        &mut summary,
                        sink,
                    )?;
                    // A granule in active holdback holds the prefix watermark
                    // before it: stop here so the next poll re-lists it (and
                    // everything after) rather than letting a later success
                    // advance the watermark past the held granule. Mirrors
                    // rw-sat's `break` on a held object.
                    if flow == ProcessFlow::HoldPrefix {
                        break;
                    }
                }
            }
            // writer (and its satellite lock) dropped here.
        }

        // Phase 2: prune. The writer lock is free now; enforce_window
        // re-acquires it (skip-if-locked guards against any other process).
        match enforce_window(&spec.store_root, &spec.satellite, now_ms, &window) {
            Ok(report) => {
                summary.pruned_buckets += report.removed_buckets;
                if report.removed_buckets > 0 || !report.skipped_locked.is_empty() {
                    sink(GlmEvent::Pruned {
                        report: report.clone(),
                    });
                }
            }
            Err(err) => sink(GlmEvent::Warning {
                message: format!("prune: {err}"),
            }),
        }

        // Keep the per-prefix watermark bounded: drop watermarks for hour
        // prefixes no longer in the poll set.
        last_key.retain(|prefix, _| prefixes.contains(prefix));

        summary.polls += 1;
        if spec.max_polls.is_some_and(|max| summary.polls >= max) {
            return Ok(summary);
        }

        sink(GlmEvent::PollSleep {
            secs: spec.poll_secs,
        });
        sleep_cancellable(Duration::from_secs(spec.poll_secs), cancel)?;
    }
}

/// Whether the prefix loop should keep advancing or stop (hold the watermark
/// before a granule still in its retry holdback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessFlow {
    /// This granule reached a terminal state (ingested, or permanently/
    /// exhaustively skipped) and the watermark advanced past it.
    Continue,
    /// This granule is in active holdback; the watermark is held before it so
    /// it is re-listed next cycle. The caller must stop the prefix here.
    HoldPrefix,
}

/// Process one listed granule: dedup, holdback gate, fetch+decode, write, and
/// record-seen. Advances the prefix watermark for granules handled to a
/// terminal state (seen, written, permanently/exhaustively skipped) and returns
/// [`ProcessFlow::Continue`]; a granule in active holdback returns
/// [`ProcessFlow::HoldPrefix`] without advancing the watermark, so it (and
/// everything after it) is re-listed next cycle.
#[allow(clippy::too_many_arguments)]
fn process_one_granule(
    prefix: &str,
    granule: &ListedGranule,
    stale_cutoff: i64,
    source: &dyn GranuleSource,
    writer: &mut BucketWriter,
    seen: &mut SeenGranules,
    holdbacks: &mut HashMap<String, Holdback>,
    last_key: &mut HashMap<String, String>,
    summary: &mut FollowSummary,
    sink: &mut dyn FnMut(GlmEvent),
) -> Result<ProcessFlow, GlmError> {
    let dedup_key = granule.dedup_key();

    // Already ingested (this session or a prior one via the manifest seed).
    if seen.contains(&dedup_key) {
        sink(GlmEvent::GranuleSkipped {
            key: dedup_key,
            reason: SkipReason::AlreadySeen,
        });
        last_key.insert(prefix.to_string(), granule.key.clone());
        return Ok(ProcessFlow::Continue);
    }

    // Holdback gate: a granule waiting out its retry timer is left alone (the
    // watermark is NOT advanced, so it is re-listed and retried next cycle).
    if let Some(hb) = holdbacks.get(&dedup_key) {
        if Instant::now() < hb.next_retry {
            return Ok(ProcessFlow::HoldPrefix);
        }
    }

    match source.fetch(granule) {
        Ok(decoded) => {
            sink(GlmEvent::GranuleFetched {
                key: dedup_key.clone(),
                bytes: granule.bytes,
            });
            sink(GlmEvent::GranuleDecoded {
                key: dedup_key.clone(),
                flashes: decoded.flashes.len(),
            });
            write_decoded(writer, &decoded, stale_cutoff, summary, sink)?;
            // Mark seen, persist it, clear any holdback, advance the watermark.
            seen.insert(dedup_key.clone());
            if let Err(e) = writer.record_seen_granule(&dedup_key) {
                sink(GlmEvent::Warning {
                    message: format!("persist seen {dedup_key}: {e}"),
                });
            }
            holdbacks.remove(&dedup_key);
            summary.ingested_granules += 1;
            last_key.insert(prefix.to_string(), granule.key.clone());
            Ok(ProcessFlow::Continue)
        }
        Err(err) => match err.kind {
            FetchErrorKind::Permanent => {
                sink(GlmEvent::Warning {
                    message: format!("decode {dedup_key}: {} (permanent)", err.message),
                });
                sink(GlmEvent::GranuleSkipped {
                    key: dedup_key.clone(),
                    reason: SkipReason::PermanentDecodeError,
                });
                // A permanent failure is terminal: never retry. Mark it seen so
                // a restart does not re-attempt it, and advance the watermark.
                seen.insert(dedup_key.clone());
                let _ = writer.record_seen_granule(&dedup_key);
                holdbacks.remove(&dedup_key);
                summary.skipped_granules += 1;
                last_key.insert(prefix.to_string(), granule.key.clone());
                Ok(ProcessFlow::Continue)
            }
            FetchErrorKind::Transient => {
                let entry = holdbacks.entry(dedup_key.clone()).or_insert(Holdback {
                    attempts: 0,
                    next_retry: Instant::now(),
                });
                entry.attempts += 1;
                if entry.attempts >= MAX_TRANSIENT_ATTEMPTS {
                    sink(GlmEvent::Warning {
                        message: format!(
                            "fetch {dedup_key}: {} (attempt {}/{MAX_TRANSIENT_ATTEMPTS}, giving up)",
                            err.message, entry.attempts
                        ),
                    });
                    sink(GlmEvent::GranuleSkipped {
                        key: dedup_key.clone(),
                        reason: SkipReason::RetriesExhausted,
                    });
                    seen.insert(dedup_key.clone());
                    let _ = writer.record_seen_granule(&dedup_key);
                    holdbacks.remove(&dedup_key);
                    summary.skipped_granules += 1;
                    last_key.insert(prefix.to_string(), granule.key.clone());
                    // Exhausted is terminal — the prefix may advance past it.
                    Ok(ProcessFlow::Continue)
                } else {
                    let delay = holdback_delay(entry.attempts);
                    entry.next_retry = Instant::now() + delay;
                    sink(GlmEvent::Warning {
                        message: format!(
                            "fetch {dedup_key}: {} (attempt {}/{MAX_TRANSIENT_ATTEMPTS}, retry in {}s)",
                            err.message,
                            entry.attempts,
                            delay.as_secs()
                        ),
                    });
                    // Hold the watermark before this granule: re-list it (and
                    // everything after) next cycle.
                    Ok(ProcessFlow::HoldPrefix)
                }
            }
        },
    }
}

/// Write one decoded granule's flashes into the store, skipping flashes older
/// than `stale_cutoff` (a restart re-lists the whole hour, which can hold
/// granules the rolling window would immediately evict — pure write/prune churn
/// otherwise). Emits a `BucketWritten` event per affected bucket.
fn write_decoded(
    writer: &mut BucketWriter,
    decoded: &DecodedGranule,
    stale_cutoff: i64,
    summary: &mut FollowSummary,
    sink: &mut dyn FnMut(GlmEvent),
) -> Result<(), GlmError> {
    let records: Vec<FlashRecord> = decoded
        .flashes
        .iter()
        .filter(|f| f.time_unix_ms >= stale_cutoff)
        .map(|f| FlashRecord {
            time_unix_ms: f.time_unix_ms,
            lat: f.lat,
            lon: f.lon,
            energy: f.energy,
            area: f.area,
            flash_id: f.flash_id,
            flags: f.flags,
            duration_ms: f.duration_ms,
        })
        .collect();
    summary.ingested_flashes += records.len();
    if records.is_empty() {
        // A quiet (or fully-stale) granule still counts as ingested for dedup;
        // there is just nothing to write.
        return Ok(());
    }
    writer
        .insert_flashes(&records, 1)
        .map_err(|e| other(e.to_string()))?;
    // Report each affected bucket that now exists.
    for path in writer.affected_bucket_paths(&records) {
        let count = bucket_record_count(&path);
        sink(GlmEvent::BucketWritten {
            path,
            records: count,
        });
    }
    Ok(())
}

/// Read a bucket's record count from its header (0 if unreadable).
fn bucket_record_count(path: &Path) -> usize {
    std::fs::read(path)
        .ok()
        .and_then(|d| crate::RwlHeader::parse(&d).ok())
        .map(|h| h.record_count as usize)
        .unwrap_or(0)
}

/// Run a live follow session against the real GLM S3 bucket.
pub fn follow_live(
    spec: &GlmFollowSpec,
    sink: &mut dyn FnMut(GlmEvent),
    cancel: &AtomicBool,
) -> Result<FollowSummary, GlmError> {
    let scratch = spec.store_root.join("glm").join(".scratch");
    let source = S3GranuleSource::new(&spec.satellite, scratch)?;
    follow_with_source(spec, &source, sink, cancel)
}

/// Bounded in-memory dedup set of ingested granule keys. The persisted cap
/// lives in [`crate::store::MAX_SEEN_GRANULE_KEYS`]; the live set is unbounded
/// within a session (re-listing only touches the current/previous hour, so it
/// cannot grow without bound) but is what the manifest is rebuilt from.
#[derive(Debug, Default)]
pub struct SeenGranules {
    set: std::collections::HashSet<String>,
}

impl SeenGranules {
    pub fn insert(&mut self, key: String) -> bool {
        self.set.insert(key)
    }
    pub fn contains(&self, key: &str) -> bool {
        self.set.contains(key)
    }
    pub fn len(&self) -> usize {
        self.set.len()
    }
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::Flash;
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    /// 2026-01-01 00:00:00 UTC in Unix ms.
    const BASE: i64 = 1_767_225_600_000;

    fn flash(time: i64, id: u32) -> Flash {
        Flash {
            time_unix_ms: time,
            lat: 30.0,
            lon: -95.0,
            energy: 1.0e-15,
            area: 25.0,
            flash_id: id,
            flags: 0,
            duration_ms: 400,
        }
    }

    fn granule(key: &str, flashes: Vec<Flash>) -> DecodedGranule {
        DecodedGranule {
            satellite: Some("G19".to_string()),
            granule_key: key.to_string(),
            flashes,
        }
    }

    /// A minimal in-memory granule source for the in-file smoke test: a fixed
    /// listing of always-Ok granules. (The full transient/permanent/holdback
    /// behaviour matrix is exercised in `tests/follow.rs`.)
    struct FakeSource {
        listing: Vec<ListedGranule>,
        responses: RefCell<HashMap<String, DecodedGranule>>,
        fetched: RefCell<Vec<String>>,
    }

    impl FakeSource {
        fn new(items: Vec<(ListedGranule, DecodedGranule)>) -> Self {
            let mut responses = HashMap::new();
            let mut listing = Vec::new();
            for (g, d) in items {
                responses.insert(g.dedup_key(), d);
                listing.push(g);
            }
            Self {
                listing,
                responses: RefCell::new(responses),
                fetched: RefCell::new(Vec::new()),
            }
        }
    }

    impl GranuleSource for FakeSource {
        fn list(
            &self,
            _prefix: &str,
            start_after: Option<&str>,
        ) -> Result<Vec<ListedGranule>, FetchError> {
            Ok(self
                .listing
                .iter()
                .filter(|g| start_after.is_none_or(|after| g.key.as_str() > after))
                .cloned()
                .collect())
        }

        fn fetch(&self, listed: &ListedGranule) -> Result<DecodedGranule, FetchError> {
            self.fetched.borrow_mut().push(listed.dedup_key());
            self.responses
                .borrow()
                .get(&listed.dedup_key())
                .cloned()
                .ok_or_else(|| FetchError::permanent("unknown key"))
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rw-glm-follow-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A key whose `s…` token sorts chronologically and whose stem is the
    /// dedup key. Uses a recent live-shaped name.
    fn key(idx: u32, year: i64, doy: u32, hour: u32) -> String {
        format!(
            "GLM-L2-LCFA/{year:04}/{doy:03}/{hour:02}/OR_GLM-L2-LCFA_G19_s{year:04}{doy:03}{hour:02}00{idx:03}_e{year:04}{doy:03}{hour:02}0020{idx:03}_c{year:04}{doy:03}{hour:02}0021{idx:03}.nc"
        )
    }

    fn spec_for(root: &Path, max_polls: u32) -> GlmFollowSpec {
        let mut spec = GlmFollowSpec::new("goes19", root.to_path_buf());
        spec.poll_secs = 0;
        spec.max_polls = Some(max_polls);
        // Wide window so synthetic BASE-era flashes are never stale-skipped.
        spec.window = Duration::from_secs(10_000 * 24 * 3600);
        spec
    }

    fn run(spec: &GlmFollowSpec, source: &dyn GranuleSource) -> (FollowSummary, Vec<GlmEvent>) {
        let mut events = Vec::new();
        let cancel = AtomicBool::new(false);
        let summary = follow_with_source(spec, source, &mut |e| events.push(e), &cancel).unwrap();
        (summary, events)
    }

    #[test]
    fn holdback_delay_is_exponential_and_capped() {
        assert_eq!(holdback_delay(1), Duration::from_secs(20));
        assert_eq!(holdback_delay(2), Duration::from_secs(40));
        assert_eq!(holdback_delay(3), Duration::from_secs(80));
        // Capped at 300 s.
        assert_eq!(holdback_delay(10), Duration::from_secs(300));
    }

    #[test]
    fn poll_prefixes_cover_hour_and_rollover_grace() {
        // 18:30 -> just the current hour.
        let mid = BASE + (18 * 3600 + 30 * 60) * 1000;
        let p = poll_prefixes(mid);
        assert_eq!(p, vec!["GLM-L2-LCFA/2026/001/18/".to_string()]);

        // 19:02 -> previous hour included.
        let rolled = BASE + (19 * 3600 + 2 * 60) * 1000;
        let p = poll_prefixes(rolled);
        assert_eq!(
            p,
            vec![
                "GLM-L2-LCFA/2026/001/18/".to_string(),
                "GLM-L2-LCFA/2026/001/19/".to_string(),
            ]
        );

        // Day + year rollover: 2026-01-01 00:01 includes 2025-12-31 23:00.
        let new_year = BASE + 60_000;
        let p = poll_prefixes(new_year);
        assert_eq!(p[0], "GLM-L2-LCFA/2025/365/23/".to_string());
        assert_eq!(p[1], "GLM-L2-LCFA/2026/001/00/".to_string());
    }

    #[test]
    fn ingests_a_granule_and_writes_a_bucket() {
        let root = test_root("ingest");
        let k = key(1, 2026, 1, 0);
        let source = FakeSource::new(vec![(
            ListedGranule {
                key: k.clone(),
                bytes: 1000,
            },
            granule(
                k.strip_suffix(".nc")
                    .and_then(|s| s.rsplit('/').next())
                    .unwrap(),
                vec![flash(BASE + 1000, 1), flash(BASE + 2000, 2)],
            ),
        )]);
        let spec = spec_for(&root, 1);
        let (summary, events) = run(&spec, &source);
        assert_eq!(summary.ingested_granules, 1);
        assert_eq!(summary.ingested_flashes, 2);
        assert!(
            events
                .iter()
                .any(|e| matches!(e, GlmEvent::BucketWritten { records, .. } if *records == 2))
        );
        // The flashes are on disk and readable.
        let got = crate::read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
        assert_eq!(got.len(), 2);
    }
}

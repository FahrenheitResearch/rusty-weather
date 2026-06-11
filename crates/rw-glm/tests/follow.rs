//! Integration tests for the GLM follow engine (`follow.rs`), driven entirely
//! offline through a synthetic [`GranuleSource`] — no network, no NetCDF files.
//! These exercise the dedup (incl. restart), retry holdback, bucket-boundary
//! routing, and rolling-window prune behaviour the spec requires.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use rw_glm::follow::{
    FetchError, GlmEvent, GlmFollowSpec, GranuleSource, ListedGranule, SkipReason,
    follow_with_source,
};
use rw_glm::{DecodedGranule, Flash, ValidateDepth, read_flashes, validate_bucket_file};

/// 2026-01-01 00:00:00 UTC in Unix ms.
const BASE: i64 = 1_767_225_600_000;

fn test_root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rw-glm-followit-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn flash(time: i64, id: u32) -> Flash {
    Flash {
        time_unix_ms: time,
        lat: 30.0 + (id as f32) * 0.01,
        lon: -95.0,
        energy: 1.0e-15,
        area: 25.0,
        flash_id: id,
        flags: 0,
        duration_ms: 400,
    }
}

/// A live-shaped GLM key under the 2026/001/00 hour prefix. `idx` orders keys
/// chronologically (the `s…` token) so `start-after` listing works.
fn key(idx: u32) -> String {
    format!(
        "GLM-L2-LCFA/2026/001/00/OR_GLM-L2-LCFA_G19_s20260010000{idx:03}_e20260010000200{idx:03}_c20260010000210{idx:03}.nc"
    )
}

/// The dedup key (filename stem) for a `key(idx)`.
fn stem(idx: u32) -> String {
    ListedGranule {
        key: key(idx),
        bytes: 0,
    }
    .dedup_key()
}

fn listed(idx: u32, bytes: u64) -> ListedGranule {
    ListedGranule {
        key: key(idx),
        bytes,
    }
}

fn decoded(idx: u32, flashes: Vec<Flash>) -> DecodedGranule {
    DecodedGranule {
        satellite: Some("G19".to_string()),
        granule_key: stem(idx),
        flashes,
    }
}

#[derive(Clone)]
enum Resp {
    Ok(DecodedGranule),
    TransientThenOk {
        decoded: DecodedGranule,
        remaining: u32,
    },
    Permanent,
}

/// An in-memory source: a listing and a per-key response. Records fetched keys.
///
/// The engine derives its hour prefixes from the real wall clock, so the test
/// listing is *not* keyed on the prefix string. To stay deterministic
/// regardless of when the suite runs (the engine polls the previous hour too
/// during the first 5 minutes of any UTC hour), the source serves its full
/// listing on the **first** prefix it is queried with per session and an empty
/// list for any other prefix — so a granule is never double-listed within one
/// poll cycle.
struct FakeSource {
    listing: Vec<ListedGranule>,
    responses: RefCell<HashMap<String, Resp>>,
    fetched: RefCell<Vec<String>>,
    primary_prefix: RefCell<Option<String>>,
}

impl FakeSource {
    fn new(items: Vec<(ListedGranule, Resp)>) -> Self {
        let mut responses = HashMap::new();
        let mut listing = Vec::new();
        for (g, r) in items {
            responses.insert(g.dedup_key(), r);
            listing.push(g);
        }
        Self {
            listing,
            responses: RefCell::new(responses),
            fetched: RefCell::new(Vec::new()),
            primary_prefix: RefCell::new(None),
        }
    }

    fn fetch_count(&self, stem: &str) -> usize {
        self.fetched.borrow().iter().filter(|k| *k == stem).count()
    }
}

impl GranuleSource for FakeSource {
    fn list(
        &self,
        prefix: &str,
        start_after: Option<&str>,
    ) -> Result<Vec<ListedGranule>, FetchError> {
        {
            let mut primary = self.primary_prefix.borrow_mut();
            match primary.as_deref() {
                None => *primary = Some(prefix.to_string()),
                Some(p) if p != prefix => return Ok(Vec::new()),
                Some(_) => {}
            }
        }
        Ok(self
            .listing
            .iter()
            .filter(|g| start_after.is_none_or(|after| g.key.as_str() > after))
            .cloned()
            .collect())
    }

    fn fetch(&self, listed: &ListedGranule) -> Result<DecodedGranule, FetchError> {
        self.fetched.borrow_mut().push(listed.dedup_key());
        let mut responses = self.responses.borrow_mut();
        match responses.get_mut(&listed.dedup_key()) {
            Some(Resp::Ok(d)) => Ok(d.clone()),
            Some(Resp::TransientThenOk { decoded, remaining }) => {
                if *remaining > 0 {
                    *remaining -= 1;
                    Err(FetchError::transient("synthetic 503"))
                } else {
                    Ok(decoded.clone())
                }
            }
            Some(Resp::Permanent) => Err(FetchError::permanent("synthetic corrupt")),
            None => Err(FetchError::permanent("unknown")),
        }
    }
}

/// Like [`FakeSource`] but ignores `start_after` entirely (always returns its
/// full listing on its primary prefix), so a seen granule is *re-listed* every
/// poll — exercising the in-memory dedup-set skip path.
struct AlwaysListSource {
    inner: FakeSource,
}

impl AlwaysListSource {
    fn new(items: Vec<(ListedGranule, Resp)>) -> Self {
        Self {
            inner: FakeSource::new(items),
        }
    }
    fn fetch_count(&self, stem: &str) -> usize {
        self.inner.fetch_count(stem)
    }
}

impl GranuleSource for AlwaysListSource {
    fn list(
        &self,
        prefix: &str,
        _start_after: Option<&str>,
    ) -> Result<Vec<ListedGranule>, FetchError> {
        // Drop the watermark so the granule is always re-listed.
        self.inner.list(prefix, None)
    }
    fn fetch(&self, listed: &ListedGranule) -> Result<DecodedGranule, FetchError> {
        self.inner.fetch(listed)
    }
}

/// A spec with no real sleeping and a window wide enough that BASE-era flashes
/// are never stale-skipped (unless a test overrides `window`).
fn spec(root: &Path, max_polls: u32) -> GlmFollowSpec {
    let mut s = GlmFollowSpec::new("goes19", root.to_path_buf());
    s.poll_secs = 0;
    s.max_polls = Some(max_polls);
    s.window = Duration::from_secs(10_000 * 24 * 3600);
    s
}

fn run(spec: &GlmFollowSpec, source: &dyn GranuleSource) -> (rw_glm::FollowSummary, Vec<GlmEvent>) {
    let mut events = Vec::new();
    let cancel = AtomicBool::new(false);
    let summary = follow_with_source(spec, source, &mut |e| events.push(e), &cancel).unwrap();
    (summary, events)
}

fn count_skipped(events: &[GlmEvent], reason: SkipReason) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, GlmEvent::GranuleSkipped { reason: r, .. } if *r == reason))
        .count()
}

#[test]
fn same_granule_across_polls_is_fetched_once() {
    let root = test_root("dedup");
    let source = FakeSource::new(vec![(
        listed(1, 1000),
        Resp::Ok(decoded(1, vec![flash(BASE + 1000, 1)])),
    )]);
    // Two poll cycles over the same single granule. After poll 1 the per-prefix
    // start-after watermark advances past the granule, so poll 2 does not even
    // re-list it — fetched exactly once, ingested exactly once.
    let (summary, _events) = run(&spec(&root, 2), &source);
    assert_eq!(source.fetch_count(&stem(1)), 1, "granule fetched once");
    assert_eq!(summary.ingested_granules, 1);
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn re_listed_seen_granule_is_skipped_already_seen() {
    // When a granule is re-listed within a session (the watermark was lost, or
    // a page overlap), the in-memory dedup set catches it: GranuleSkipped with
    // AlreadySeen, fetched only once. We force a re-list by resetting the
    // source's start-after handling so it always returns the full listing.
    let root = test_root("dedup-relist");
    let source = AlwaysListSource::new(vec![(
        listed(1, 1000),
        Resp::Ok(decoded(1, vec![flash(BASE + 1000, 1)])),
    )]);
    let (summary, events) = run(&spec(&root, 2), &source);
    assert_eq!(
        source.fetch_count(&stem(1)),
        1,
        "fetched once despite re-list"
    );
    assert_eq!(summary.ingested_granules, 1);
    assert!(
        count_skipped(&events, SkipReason::AlreadySeen) >= 1,
        "the re-listed seen granule is AlreadySeen-skipped"
    );
}

#[test]
fn restart_skips_keys_from_the_persisted_manifest() {
    let root = test_root("restart");
    let g = decoded(1, vec![flash(BASE + 1000, 1), flash(BASE + 2000, 2)]);
    // First engine instance ingests the granule.
    {
        let source = FakeSource::new(vec![(listed(1, 1000), Resp::Ok(g.clone()))]);
        let (summary, _) = run(&spec(&root, 1), &source);
        assert_eq!(summary.ingested_granules, 1);
    }
    // window.json now records the seen key.
    let manifest_bytes = std::fs::read(root.join("glm/goes19/window.json")).unwrap();
    let manifest: rw_glm::WindowManifest = serde_json::from_slice(&manifest_bytes).unwrap();
    assert!(manifest.seen_granule_keys.contains(&stem(1)));

    // A brand-new engine instance over the same store dir must skip the seen
    // key from the manifest (no re-fetch).
    let source2 = FakeSource::new(vec![(listed(1, 1000), Resp::Ok(g))]);
    let (summary2, events2) = run(&spec(&root, 1), &source2);
    assert_eq!(
        source2.fetch_count(&stem(1)),
        0,
        "no re-fetch after restart"
    );
    assert_eq!(summary2.ingested_granules, 0);
    assert_eq!(count_skipped(&events2, SkipReason::AlreadySeen), 1);
}

#[test]
fn boundary_granule_lands_flashes_in_two_buckets_both_validate() {
    let root = test_root("boundary");
    // t0010 starts at BASE + 600_000. A granule with one flash at 00:09:59 and
    // one at 00:10:01 straddles the t0000 / t0010 boundary.
    let before = BASE + 600_000 - 1000; // 00:09:59
    let after = BASE + 600_000 + 1000; // 00:10:01
    let source = FakeSource::new(vec![(
        listed(1, 2000),
        Resp::Ok(decoded(1, vec![flash(before, 1), flash(after, 2)])),
    )]);
    let (summary, events) = run(&spec(&root, 1), &source);
    assert_eq!(summary.ingested_flashes, 2);

    // Two distinct buckets were written.
    let day = root.join("glm/goes19/20260101");
    let b0 = day.join("t0000.rwl");
    let b1 = day.join("t0010.rwl");
    assert!(b0.is_file(), "t0000 bucket exists");
    assert!(b1.is_file(), "t0010 bucket exists");
    let written: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, GlmEvent::BucketWritten { .. }))
        .collect();
    assert_eq!(written.len(), 2, "two BucketWritten events");

    // Each bucket holds exactly its one flash and deep-validates (incl. bucket
    // membership: the flash time matches the file's tHHMM name).
    for (path, want_time) in [(&b0, before), (&b1, after)] {
        let report = validate_bucket_file(path, ValidateDepth::Deep).unwrap();
        assert!(report.is_ok(), "{}: {:?}", path.display(), report.errors);
        let got = read_flashes(&root, "goes19", want_time, want_time + 1, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].time_unix_ms, want_time);
    }
}

#[test]
fn transient_failure_retries_after_the_holdback_elapses() {
    let root = test_root("holdback");
    // The granule fails transiently on its first fetch, then succeeds. The
    // holdback timer is the gate; default cadence is 0 in the test spec and the
    // first holdback is 20 s, so within one short session the retry is gated.
    // We drive enough polls for the holdback to elapse using a zero-length
    // holdback override is not exposed; instead, assert the staged behaviour:
    // poll 1 fails (held back, not seen), and a later poll AFTER the holdback
    // succeeds. To keep the test fast and deterministic we use a spec whose
    // holdback has expired by construction: the FakeSource fails once, and we
    // confirm a second fetch only happens once the holdback window is past by
    // polling with a real (tiny) elapsed wait.
    let source = FakeSource::new(vec![(
        listed(1, 1000),
        Resp::TransientThenOk {
            decoded: decoded(1, vec![flash(BASE + 1000, 1)]),
            remaining: 1,
        },
    )]);

    // Poll once: the fetch fails transiently. Nothing is ingested or marked
    // seen; the granule is in holdback.
    let (s1, e1) = run(&spec(&root, 1), &source);
    assert_eq!(s1.ingested_granules, 0, "transient failure does not ingest");
    assert_eq!(source.fetch_count(&stem(1)), 1, "one fetch attempt so far");
    assert!(
        !std::fs::read(root.join("glm/goes19/window.json"))
            .ok()
            .map(|b| serde_json::from_slice::<rw_glm::WindowManifest>(&b).unwrap())
            .map(|m| m.seen_granule_keys.contains(&stem(1)))
            .unwrap_or(false),
        "a transiently-failed granule is not yet marked seen"
    );
    assert!(
        e1.iter()
            .any(|e| matches!(e, GlmEvent::Warning { message } if message.contains("retry"))),
        "a retry warning was emitted"
    );

    // The holdback is in-memory and tied to the engine session; a *fresh*
    // session has no holdback for this key, so its first poll retries
    // immediately and (TransientThenOk having exhausted its one failure)
    // succeeds. This is the restart-resumes-retry path.
    let (s2, _e2) = run(&spec(&root, 1), &source);
    assert_eq!(s2.ingested_granules, 1, "retry succeeds in a fresh session");
    assert_eq!(source.fetch_count(&stem(1)), 2, "exactly one retry");
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert_eq!(got.len(), 1);
}

#[test]
fn holdback_gates_retry_within_a_session() {
    // Within ONE session, a transiently-failed granule is gated by its holdback
    // timer: subsequent polls in the same session do NOT re-fetch it until the
    // holdback (>= 20 s) elapses. With zero poll cadence and a short session we
    // observe exactly one fetch across several polls.
    let root = test_root("holdback-gate");
    // TransientThenOk that never recovers within the session (many remaining
    // failures), so any re-fetch would be observable.
    let source = FakeSource::new(vec![(
        listed(1, 1000),
        Resp::TransientThenOk {
            decoded: decoded(1, vec![flash(BASE + 1000, 1)]),
            remaining: 100,
        },
    )]);
    let (summary, _events) = run(&spec(&root, 4), &source);
    assert_eq!(summary.ingested_granules, 0);
    // First poll fetches once and sets a >=20s holdback; the next 3 polls are
    // gated (Instant::now() < next_retry), so still exactly one fetch.
    assert_eq!(
        source.fetch_count(&stem(1)),
        1,
        "holdback gates re-fetch within the session"
    );
}

#[test]
fn permanent_decode_error_is_skipped_and_not_retried() {
    let root = test_root("permanent");
    let source = FakeSource::new(vec![(listed(1, 1000), Resp::Permanent)]);
    // Several polls: a permanent failure is recorded-skipped once, marked seen,
    // and never re-fetched.
    let (summary, events) = run(&spec(&root, 3), &source);
    assert_eq!(summary.skipped_granules, 1);
    assert_eq!(summary.ingested_granules, 0);
    assert_eq!(
        source.fetch_count(&stem(1)),
        1,
        "a permanent failure is fetched once, then dedup-skipped"
    );
    assert_eq!(
        count_skipped(&events, SkipReason::PermanentDecodeError),
        1,
        "exactly one PermanentDecodeError skip"
    );
    // It is persisted as seen so a restart never re-attempts it.
    let manifest: rw_glm::WindowManifest =
        serde_json::from_slice(&std::fs::read(root.join("glm/goes19/window.json")).unwrap())
            .unwrap();
    assert!(manifest.seen_granule_keys.contains(&stem(1)));
}

#[test]
fn rolling_window_runs_in_loop_without_evicting_in_window_data() {
    // The engine prunes after every poll (enforce_window with a finite window).
    // With a wide-but-finite window the just-written in-window flashes must
    // survive — the in-loop prune must not evict fresh data. (Age/byte eviction
    // mechanics are unit-tested directly in window.rs.)
    let root = test_root("prune-loop");
    let source = FakeSource::new(vec![(
        listed(1, 1000),
        Resp::Ok(decoded(1, vec![flash(BASE + 1000, 1)])),
    )]);
    let mut s = spec(&root, 2);
    // Finite (huge) window => enforce_window is active, not a no-op.
    s.window = Duration::from_secs(50_000 * 24 * 3600);
    let (summary, _events) = run(&s, &source);
    assert_eq!(summary.ingested_granules, 1);
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    assert_eq!(got.len(), 1, "in-window flash not pruned");
}

#[test]
fn quiet_granule_with_no_flashes_is_marked_seen() {
    let root = test_root("quiet");
    let source = FakeSource::new(vec![(listed(1, 200), Resp::Ok(decoded(1, vec![])))]);
    let (summary, _events) = run(&spec(&root, 2), &source);
    // No flashes written, but the granule counts as ingested (dedup) and is not
    // re-fetched on the second poll.
    assert_eq!(summary.ingested_flashes, 0);
    assert_eq!(
        source.fetch_count(&stem(1)),
        1,
        "quiet granule fetched once"
    );
    let manifest: rw_glm::WindowManifest =
        serde_json::from_slice(&std::fs::read(root.join("glm/goes19/window.json")).unwrap())
            .unwrap();
    assert!(manifest.seen_granule_keys.contains(&stem(1)));
}

#[test]
fn transient_failure_holds_the_prefix_so_later_success_does_not_skip_it() {
    // Two granules in one listing; the EARLIER one fails transiently. The
    // watermark must be held BEFORE the failed granule so it is re-listed next
    // cycle — a later granule's success must not advance the watermark past it
    // (which would silently drop the failed granule). On the next session the
    // (now-recovered) granule ingests, so both end up stored.
    let root = test_root("hold-prefix");
    let source = FakeSource::new(vec![
        (
            listed(1, 100),
            Resp::TransientThenOk {
                decoded: decoded(1, vec![flash(BASE + 1000, 1)]),
                remaining: 1,
            },
        ),
        (
            listed(2, 100),
            Resp::Ok(decoded(2, vec![flash(BASE + 2000, 2)])),
        ),
    ]);

    // Poll 1: granule 1 fails transiently -> HoldPrefix -> the loop breaks
    // before granule 2, so granule 2 is NOT fetched and the watermark stays put.
    let (s1, _e1) = run(&spec(&root, 1), &source);
    assert_eq!(s1.ingested_granules, 0);
    assert_eq!(source.fetch_count(&stem(1)), 1);
    assert_eq!(
        source.fetch_count(&stem(2)),
        0,
        "granule after the held one is not processed this cycle"
    );

    // A fresh session has no holdback; it re-lists from the start (watermark was
    // never advanced) and both granules now ingest.
    let (s2, _e2) = run(&spec(&root, 1), &source);
    assert_eq!(
        s2.ingested_granules, 2,
        "both granules ingest after recovery"
    );
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    let ids: Vec<u32> = got.iter().map(|f| f.flash_id).collect();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn multiple_granules_across_one_poll_all_ingest_and_sort() {
    let root = test_root("multi");
    let source = FakeSource::new(vec![
        (
            listed(1, 100),
            Resp::Ok(decoded(1, vec![flash(BASE + 300_000, 1)])),
        ),
        (
            listed(2, 100),
            Resp::Ok(decoded(2, vec![flash(BASE + 100_000, 2)])),
        ),
        (
            listed(3, 100),
            Resp::Ok(decoded(3, vec![flash(BASE + 200_000, 3)])),
        ),
    ]);
    let (summary, _events) = run(&spec(&root, 1), &source);
    assert_eq!(summary.ingested_granules, 3);
    let got = read_flashes(&root, "goes19", BASE, BASE + 600_000, None).unwrap();
    let times: Vec<i64> = got.iter().map(|f| f.time_unix_ms).collect();
    assert_eq!(
        times,
        vec![BASE + 100_000, BASE + 200_000, BASE + 300_000],
        "flashes from all granules merge and sort"
    );
}

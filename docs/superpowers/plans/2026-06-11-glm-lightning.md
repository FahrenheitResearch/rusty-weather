# GLM Lightning Ingest (rw-glm) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** All-Rust GLM flash ingest per the converged spec `docs/superpowers/specs/2026-06-11-glm-lightning-draft.md` (bowecho-approved): `.rwl` rolling-window store + `read_flashes` API + in-process follow engine. Bowecho's layer work starts the day Task 1's reader API is on a pushed branch — push `glm-lightning` to origin after every task.

**Architecture:** New crate `crates/rw-glm` mirroring rw-sat (anonymous S3 poll → netcrust NetCDF decode → rolling-window store), but storing point events in the fixed-record `.rwl` format instead of gridded rw-store. Reuses `rw_store::lock::RunLock` and `rw_store::atomic` (format-agnostic). The spec doc is the normative contract — record layout, flags bits, bucket scheme, defaults are all decided there; do not re-litigate.

**Verification mode:** Task 1 (format) = full adversarial review, same bar as rw-store (golden fixture, hostile-input validator, FORMAT.md section). Tasks 2-4 = lean unless a format change sneaks in.

**Key spec facts (from the converged spec — read it first):**
- 32-byte LE record: time_unix_ms i64 (first event) | lat f32 | lon f32 | energy f32 (raw J) | area f32 (km²) | flash_id u32 | flags u16 (bit0 = degraded_quality, rest write-zero/read-ignore) | duration_ms u16 (saturating).
- File: `<root>/glm/<satellite>/tHHMM.rwl`, HHMM floored to 10 min, records sorted by time; 64-byte header magic `RWLIGHT1`, version u32 (=1), record count, time range (min/max unix_ms), source-granule count; active bucket atomically rewritten per granule; closed buckets immutable. `window.json` manifest + `.rw-lock` via RunLock; rolling window = age + byte budget, default 2 h.
- Reader: `read_flashes(root, satellite, t0..=t1, Option<bbox>) -> Vec<Flash>` — file selection by filename time range, linear scan + filter.
- Source: `s3://noaa-goes19|noaa-goes18/GLM-L2-LCFA/YYYY/DDD/HH/OR_GLM-L2-LCFA_G##_s…_e…_c….nc`, ~20 s granules, NetCDF4 via the vendored netcrust stack (same patterns as `crates/rw-sat/src/netcdf.rs` / `s3.rs`).
- GLM granule variables (verify against a real granule in Task 2 — names from the L2 LCFA product spec): `flash_id`, `flash_time_offset_of_first_event`, `flash_time_offset_of_last_event` (offsets vs the `product_time` epoch attr), `flash_lat`, `flash_lon`, `flash_energy` (J, scaled-int with scale/offset like rw-sat's `read_scaled_f32`), `flash_area` (m² — convert to km²), `flash_quality_flag` (0 = good).

---

### Task 1: `.rwl` format + reader API (full verification) — THE BOWECHO UNBLOCKER

**Files:**
- Create: `crates/rw-glm/` (Cargo.toml: deps rw-store (lock/atomic), serde/serde_json, thiserror — workspace versions; NO s3/netcdf deps yet), `src/lib.rs`, `src/format.rs` (header+record pack/unpack, constants, bucket-name math `tHHMM` floor-10), `src/store.rs` (BucketWriter: insert-sorted append + atomic rewrite; window.json manifest), `src/reader.rs` (`Flash` struct, `read_flashes`), `src/validate.rs` (header/record/sort/range/count checks, Structural+Deep pattern copied from rw-store's contract: Err = I/O only, format problems → report.errors, no panics on hostile bytes — bounds-check everything)
- Modify: root `Cargo.toml` workspace members
- Create: `crates/rw-glm/tests/golden.rs` + committed fixture `tests/golden/v1/` (synthetic, literal-formula flashes incl. duration saturation, degraded-quality bit, bucket-boundary times; same regen-test pattern as rw-store's golden.rs — byte-stability + reader-pin + ignored regen with loud format-change warning) + `.gitattributes` entry
- Modify: `docs/FORMAT.md` — new §10 “.rwl flash files” (header/record tables, flags bits, bucket scheme, concurrency = same RunLock contract, versioning = golden fixtures freeze v1)

**Steps (TDD):**
- [ ] Failing tests: record pack/unpack round-trip with exact byte-offset assertions (mirror rw-store's index_record_pack_layout_is_exact); bucket-name math (t0000/t2350, floor behavior, day boundary); writer insert keeps sort + atomic rewrite visible to a concurrent reader (write, read mid-stream from second handle); `read_flashes` time-range file selection (range spanning 3 buckets returns only in-range flashes, half-open vs inclusive semantics — pick inclusive start/exclusive end and DOCUMENT), bbox filter, empty-range, missing-satellite-dir → clean empty; validator catches truncation/unsorted/bad-magic/count-mismatch/time-range-header-lies without panics.
- [ ] Implement format/store/reader/validate.
- [ ] Golden fixture + regen harness; FORMAT.md §10.
- [ ] Gates: `cargo test -p rw-glm` green, `cargo test --workspace` green, fmt/clippy clean for rw-glm.
- [ ] Commit `feat(rw-glm): .rwl flash store format + reader API` ; **push branch to origin** (bowecho starts here).

### Task 2: Granule decode (lean + one real-granule pin)

**Files:** `crates/rw-glm/src/granule.rs` (+ netcrust dep), fixture `crates/rw-glm/tests/fixtures/<one real small granule>.nc` (~100-400 KB, fetch the most recent G19 granule once via S3 — list bucket anonymously like rw-sat/s3.rs, document provenance in a fixture README)
- [ ] Failing test against the committed real granule: decode_granule(path) → flashes with: count == NetCDF flash dim, every lat within ±66 (GLM disk), lon within disk extent, energy > 0 finite (raw J ~1e-15..1e-10 range sanity), duration_ms saturation logic, flags bit0 set iff quality != 0, time = product epoch + first-event offset (assert one flash's absolute time against a hand-computed value from the granule's raw attrs — print the attrs in the test comment).
- [ ] Implement with netcrust scaled reads (pattern: rw-sat netcdf.rs read_scaled_f32). Area m²→km².
- [ ] Workspace green; commit; push.

### Task 3: Follow engine + window (lean)

**Files:** `crates/rw-glm/src/follow.rs`, `src/window.rs`, `src/s3.rs` (adapt rw-sat's listing — extract-and-share only if trivial; copying the ~paginated-list fn with a comment is acceptable, no premature abstraction), events enum mirroring rw-sat's
- [ ] Tests: granule-key dedup across restarts (state from existing buckets' granule provenance — header granule count is insufficient; persist seen-granule keys in window.json, capped); retry holdback on transient fetch error; window pruning age+bytes with RunLock skip-if-locked (mirror rw-sat window.rs tests incl. lock-held-skip); bucket rewrite per granule is atomic.
- [ ] `GlmFollowSpec { satellite, poll_secs (default 20), window (default 2h), byte_budget }`; in-process runnable (SatWorker pattern — but NO UI work in this plan).
- [ ] Workspace green; commit; push.

### Task 4: Live validation + handoff (lean)

- [ ] Run the follow engine against live G19 for ≥10 min (and one G18 listing sanity check): buckets appear, flash counts plausible (nonzero if any CONUS/SA convection — check both satellites if quiet), `read_flashes` over the live window returns sorted in-range flashes, validator Deep-passes every bucket, prune respects budget. Capture numbers (flashes/min, bytes/bucket).
- [ ] README section (rw-glm usage + the hdf5-reader patch footgun applies — netcrust dep), memory update, bowecho note: reader API shape final, branch/rev to pin.
- [ ] Full gate + merge per finishing-a-development-branch (drew's call on merge timing vs GFS).

## Self-review
- Bowecho dependency is Task 1 only — hence push-per-task and format-first ordering.
- The only cross-track file with GFS is root Cargo.toml (one members line) — trivial merge.
- Granule variable names are the plan's main external assumption; Task 2's real-granule fixture pins them empirically before the follow engine builds on them.
- Flags bit0 mapping ("anything but nominal") is the spec's; if the real granule shows a multi-valued quality enum worth preserving, widen to bits 0-2 in Task 2 WITH a spec-doc amendment, not silently.

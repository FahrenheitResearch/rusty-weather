# GFS End-to-End Support Implementation Plan (Multi-Model Phase A)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `rw_ingest --model gfs` / the Download panel's GFS option work end-to-end: fetch → store → derived → render → soundings → UI, with the same verification bar as HRRR.

**Architecture:** The render/products lane (rustwx-models/products) is already model-agnostic with full GFS metadata (URL builders, cycle tables, per-model selector gating). The new ingest stack has exactly one structural gap: `rw-ingest::fetch_hour` hardcodes HRRR's two-product plan (`"prs"` + `"sfc"`), and GFS's product parser rejects those tokens (`rustwx-models/src/lib.rs:6702-6722` → `UnsupportedProduct`). Fix = a per-model **fetch plan** mapping product files to roles (pressure-source / surface-source), where GFS's single `pgrb2.0p25` serves both. Everything else is gating flips, calibration, fixtures, and live validation.

**Tech Stack:** existing workspace; no new dependencies.

**Verification mode:** Task 1-2 (ingest engine + store path) = full adversarial review (storage lane). Tasks 3-6 = lean (one reviewer).

**Verified facts (file:line, re-verify before relying):**
- `ModelId::Gfs` exists with aliases (`rustwx-core/src/lib.rs:1587,1653`); registry entry: cycles `[0,6,12,18]` (`rustwx-models/src/lib.rs:403`), default product `pgrb2.0p25` (:639), hours 0-120 hourly then 123-384 3-hourly (:5941-5944), sources AWS/NOMADS/Google/NCEI (`build_gfs_url` :6721-6766, filename `gfs.t{HH}z.pgrb2.0p25.f{FFF}`, path `gfs.{DATE}/{HH:02}/atmos/`).
- GFS product tokens accepted: `pgrb2.0p25|pgrb2.0p50|pgrb2.1p00|pgrb2b.0p25|sflux` — **`"prs"`/`"sfc"` are rejected** (:6702-6722).
- `rw-ingest/src/lib.rs:50-52`: `ingest_supported()` = HRRR only. UI greys models off this; `rusty-weather-ui/src/ingest_worker.rs:195-205` rejects with "HRRR only today".
- `rw-ingest/src/ingest_hour.rs:393-415`: `fetch_hour` builds exactly two `FetchRequest`s with literal `"prs"`/`"sfc"`.
- 2D plane selection is already model-aware (`ingest_hour.rs:508` → recipe catalog); profiles/levels dynamic (`ingest_profile.rs` `candidate_levels()`); compute model-neutral (`ingest_compute.rs`).
- Per-model selector gating already blocks GFS from reflectivity/UH/categorical-precip/simulated-IR/smoke (`rustwx-models/src/lib.rs:5760-5923`).
- `size_estimate.rs:186-324`: builtin calibration is HRRR-measured; falls back to store-derived calibration per model when hours exist.
- `rw-store/src/grid.rs:396-401,466-481`: GridLocator documents the lon-periodic seam (0/360 wrap cell falls back to nearest-neighbor, ≤ half-cell error) — acceptable, document only.
- Fixture pattern: `crates/rusty-weather/tests/fixtures/hrrr_mini.grib2` (4.7 MB) used by `tests/rw_store_e2e.rs`.
- **GFS APCP landmine:** GFS `pgrb2` APCP is bucketed accumulation (0-6h resets), NOT hourly like HRRR's `apcp_1h` re-select trick (`ingest_hour.rs`, documented-fragile). GFS v1 must NOT claim an `apcp_1h` field it can't honestly produce.
- GFS 0.25°: grid 1440×721, lat typically 90→-90 (descending), lon 0→359.75 (periodic). Reader orientation is derived per-grid (existing).

---

### Task 1: Per-model fetch plan in rw-ingest (full verification)

**Files:**
- Modify: `crates/rw-ingest/src/ingest_hour.rs` (fetch_hour, FetchedHour, process_fetched_hour roles)
- Modify: `crates/rw-ingest/src/lib.rs` (ingest_supported → `Hrrr | Gfs`; new `fetch_plan` export if placed here)
- Test: in-file + existing rw-ingest tests stay green

**Design:**
```rust
/// One product file to fetch and the roles its messages serve.
pub struct ProductFetch {
    pub product: &'static str,     // token passed to ModelRunRequest
    pub surface_source: bool,      // 2D field extraction reads this file
    pub pressure_source: bool,     // 3D volume extraction reads this file
}

pub fn fetch_plan(model: ModelId) -> Result<Vec<ProductFetch>, IngestError> {
    match model {
        ModelId::Hrrr => Ok(vec![
            ProductFetch { product: "prs", surface_source: false, pressure_source: true },
            ProductFetch { product: "sfc", surface_source: true,  pressure_source: false },
        ]),
        ModelId::Gfs => Ok(vec![
            ProductFetch { product: "pgrb2.0p25", surface_source: true, pressure_source: true },
        ]),
        other => Err(/* not ingest-supported */),
    }
}
```
`FetchedHour` carries `surface_path: PathBuf` + `pressure_path: PathBuf` (same file for GFS — dedupe the download, fetch once, reference twice). `process_fetched_hour` reads roles from those paths instead of the literal prs/sfc fields. The HRRR path must remain **byte-identical in behavior**: same URLs, same extraction order, same store output (the golden e2e + determinism gates prove it).

**Steps:**
- [ ] Failing test: `fetch_plan(Gfs)` returns one entry serving both roles; `fetch_plan(Hrrr)` two entries; `ModelRunRequest::new(Gfs, cycle, hour, "pgrb2.0p25")` resolves a well-formed AWS URL (assert exact URL string for a literal date/cycle/hour, mirroring the existing GFS URL tests at `rustwx-models/src/lib.rs:~11254`).
- [ ] Failing test: `ingest_supported(ModelId::Gfs)` is true; some non-supported model (e.g. `Nbm`) false.
- [ ] Implement plan + FetchedHour refactor; update `ingest_worker.rs:203` error message to derive the supported list dynamically.
- [ ] GFS hour-cadence validation: requesting f121 (not on the 3-hourly grid past 120) errors with a clear message at spec-resolution time (check where hours validate — extend `ingest_profile.rs` validation or the bin's hour parser; HRRR behavior unchanged). Test both directions (f123 ok, f121 rejected, HRRR f018 ok).
- [ ] **APCP honesty:** for GFS, exclude `apcp_1h` (and any HRRR-trick-derived field) from the 2D plan; confirm the recipe catalog gating already does this — if not, gate it in the plan assembly with a comment citing the bucket-reset semantics. Test: GFS plan contains no apcp_1h selector.
- [ ] `cargo test -p rw-ingest -p rusty-weather-ui` green; full `cargo test --workspace` green (HRRR e2e proves no regression).
- [ ] Commit `feat(rw-ingest): per-model fetch plans; GFS ingest-supported`

### Task 2: Live GFS ingest → store → validate (full verification)

**Files:**
- Possibly modify: `crates/rw-ingest/src/ingest_hour.rs` (whatever the live run surfaces)
- Create: `crates/rusty-weather/tests/fixtures/gfs_mini.grib2` (small real subset, see below)
- Modify: `crates/rusty-weather/tests/rw_store_e2e.rs` (parametrize over both fixtures)

**Steps:**
- [ ] Live run from the worktree: `cargo run --release -p rusty-weather --bin rw_ingest -- --model gfs --date <today> --cycle <latest> --hours 0-3 --store-root out/gfs_store --profile sounding --verify`. Iterate until clean. Note wall time + file sizes (expect bigger than HRRR per hour at full level set: 1440×721×levels).
- [ ] `rws validate out/gfs_store/gfs/<run> --deep` → exit 0 (the Wave-1 validator is the conformance gate for the new model's files).
- [ ] `rws export` one GFS hour, open in xarray (Bash python), assert global lat/lon ranges and a spot value vs `read_full_2d`.
- [ ] Sounding spot-check: `read_profile_3d` at a mid-Pacific point AND within 0.3° of the lon seam (grid.rs documented fallback) — values finite, ordered by level.
- [ ] Build the fixture: subset the downloaded pgrb2.0p25 to a handful of messages (use the same technique that produced hrrr_mini.grib2 — check how that fixture was made; wgrib2 unavailable → byte-range slice by .idx offsets is fine) targeting < 6 MB. Parametrize `rw_store_e2e.rs` over hrrr_mini + gfs_mini (shared test body, per-model expected var lists).
- [ ] `cargo test --workspace` green. Commit `feat(rusty-weather): GFS ingest e2e + fixture`

### Task 3: Estimator calibration for GFS (lean)

**Files:**
- Modify: `crates/rw-ingest/src/size_estimate.rs`

**Steps:**
- [ ] From the Task-2 live store, run the calibration walk (the exact-bytes path rw_bench/size_estimate share) and add a GFS builtin table next to the HRRR one (constants + provenance comment with run date). Estimator selects builtin by model; unknown-model fallback unchanged.
- [ ] Test: GFS estimate with empty store uses GFS builtins (provenance string says so); accuracy assertion vs the live store within ±15%.
- [ ] Commit `feat(rw-ingest): GFS builtin size calibration`

### Task 4: Render lane validation (lean)

**Steps:**
- [ ] `rw_render` the GFS store hours: direct + derived products for regions `conus` AND a lon-seam-crossing region if the region table has one (check `rustwx-products` region defs; if none crosses 0°, render a global/full-domain view if supported). Confirm products render, colortables resolve (the resolver is selector-keyed — GFS shares selectors), maps look sane (PrintWindow/off-screen rules don't apply — these are PNG files; eyeball via reading 2-3 PNGs).
- [ ] `rw_batch --model gfs --hours 0-3 --no-heavy` end-to-end; record wall + RAM peak vs HRRR baseline (expect cheaper: 1.04M vs 1.9M cells).
- [ ] Any failures → fix in this task (model-gated product lists are the likely culprits — `supported_direct_recipe_slugs(Gfs)`).
- [ ] Commit `feat: GFS render + batch validated` (with numbers in message)

### Task 5: UI exposure (lean)

**Files:**
- Modify: `crates/rw-ui/src/panels/download.rs` (only if model options are hardcoded rather than derived from `ingest_supported`)
- Modify: `crates/rusty-weather-ui` (whatever the option list builder needs)

**Steps:**
- [ ] Verify the Download panel's GFS entry un-greys via the Task-1 gate flip; hour-picker hint for GFS cadence (3-hourly past f120) in the hours field help text.
- [ ] Run browser/field viewer/sounding panel against the GFS store (orientation, click-to-sounding at several lats incl. southern hemisphere). UI drive must follow the off-screen rules (PrintWindow, never topmost) if window automation is used; prefer the panel unit-test seams.
- [ ] Estimate path: GFS spec → estimate uses GFS builtins (Task 3) — assert in ingest_worker tests.
- [ ] Commit `feat(rw-ui): GFS in the download picker`

### Task 6: Docs + handoff (lean)

- [ ] README: model support matrix (HRRR full / GFS full / others coming), GFS example invocations, APCP caveat note (no apcp_1h for GFS v1; windowed QPF for GFS deferred until bucket-difference logic exists).
- [ ] Memory + bowecho note: GFS available behind the same APIs; download picker un-greyed; their pin bump when merged.
- [ ] Full gate: `cargo test --workspace`, release builds, golden fixtures untouched, `rw_store_diff` self-consistency on a re-ingested GFS hour (determinism on the new model: ingest the same hour twice, files must be equivalent).
- [ ] Commit + merge per finishing-a-development-branch.

## Self-review
- The one structural unknown is how `process_fetched_hour` is internally coupled to two distinct files; Task 1's implementer has license to restructure FetchedHour as long as HRRR output stays byte-equivalent (workspace e2e + determinism gates pin it).
- APCP/QPF: GFS windowed QPF is explicitly out of scope (bucket-difference logic is its own feature); the plan only guards against silently-wrong apcp_1h.
- Seam handling: documented-acceptable; Task 2 spot-checks it near 0° lon.
- RAP/RRFS-A/NBM/REFS are NOT in this plan — each is a follow-on fetch-plan entry + validation pass once GFS proves the pattern.

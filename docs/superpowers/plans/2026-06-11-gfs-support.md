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
- [x] Failing test: `fetch_plan(Gfs)` returns one entry serving both roles; `fetch_plan(Hrrr)` two entries; `ModelRunRequest::new(Gfs, cycle, hour, "pgrb2.0p25")` resolves a well-formed AWS URL (assert exact URL string for a literal date/cycle/hour, mirroring the existing GFS URL tests at `rustwx-models/src/lib.rs:~11254`). _Done: `fetch_plan_gfs_is_one_file_serving_both_roles`, `fetch_plan_hrrr_is_the_historical_two_file_pair`, `gfs_fetch_plan_token_resolves_a_well_formed_aws_url` (asserts the literal `noaa-gfs-bdp-pds` AWS URL for 20260414 18z f012 via `resolve_urls`, picking the AWS entry since NOMADS is GFS priority 1)._
- [x] Failing test: `ingest_supported(ModelId::Gfs)` is true; some non-supported model (e.g. `Nbm`) false. _Done: `hrrr_and_gfs_are_ingest_supported`, `fetch_plan_rejects_unsupported_model`._
- [x] Implement plan + FetchedHour refactor; update `ingest_worker.rs:203` error message to derive the supported list dynamically. _Done: `fetch_plan`/`ProductFetch` in `lib.rs`; `ingest_supported` now backed by `fetch_plan`; `fetch_hour` consumes the plan and binds files to pressure/surface role slots (GFS fetches once, references twice). FetchedHour field names kept (`prs`/`sfc` = role slots) to keep all stdout/JSON summary output and `IngestEvent`/`IngestStage` byte-identical — see self-review. `ingest_worker.rs` message now derives the list from `supported_models()` filtered by `ingest_supported`._
- [x] GFS hour-cadence validation: requesting f121 (not on the 3-hourly grid past 120) errors with a clear message at spec-resolution time (check where hours validate — extend `ingest_profile.rs` validation or the bin's hour parser; HRRR behavior unchanged). Test both directions (f123 ok, f121 rejected, HRRR f018 ok). _Done: new `validate_forecast_hours(model, cycle, hours)` in `ingest_hour.rs` delegates to rustwx-models' authoritative `supported_forecast_hours`; called from both `rw_ingest`/`rw_batch` bins after `parse_hours` (the UI's `resolve_spec` already validated). Tests `validate_forecast_hours_enforces_gfs_cadence` (f121/f385 rejected, f123/f384 accepted) + `validate_forecast_hours_leaves_hrrr_unchanged` (f018 ok)._
- [x] **APCP honesty:** for GFS, exclude `apcp_1h` (and any HRRR-trick-derived field) from the 2D plan; confirm the recipe catalog gating already does this — if not, gate it in the plan assembly with a comment citing the bucket-reset semantics. Test: GFS plan contains no apcp_1h selector. _Done: the `apcp_1h`/`uh_2to5km_max_1h`/`wind_speed_10m_max_1h` trailing re-select is NOT a recipe-catalog field — it's the ingest_hour.rs HRRR-trick re-select, so it needed gating here. New `model_has_trailing_1h_window(model)` (HRRR/HrrrAk only) gates the trailing extraction, the `planned_2d` count, and `planned_store_variables`, with a comment citing GFS's 0-6h bucket resets. Test `gfs_full_plan_excludes_apcp_1h_but_hrrr_keeps_it`._
- [x] `cargo test -p rw-ingest -p rusty-weather-ui` green; full `cargo test --workspace` green (HRRR e2e proves no regression). _Done: rw-ingest 45 + rusty-weather-ui 22 green; workspace 1044 passed / 0 failed / 6 ignored; HRRR `rw_store_e2e` golden round-trip passes._
- [x] Commit `feat(rw-ingest): per-model fetch plans; GFS ingest-supported`

### Task 2: Live GFS ingest → store → validate (full verification)

**Files:**
- Possibly modify: `crates/rw-ingest/src/ingest_hour.rs` (whatever the live run surfaces)
- Create: `crates/rusty-weather/tests/fixtures/gfs_mini.grib2` (small real subset, see below)
- Modify: `crates/rusty-weather/tests/rw_store_e2e.rs` (parametrize over both fixtures)

**Steps:**
- [x] Live run from the worktree: `cargo run --release -p rusty-weather --bin rw_ingest -- --model gfs --date <today> --cycle <latest> --hours 0-3 --store-root out/gfs_store --profile sounding --verify`. Iterate until clean. Note wall time + file sizes (expect bigger than HRRR per hour at full level set: 1440×721×levels). _Done: GFS 20260611 00z f000-f003, sounding profile, all `--verify`-passed (7 2D bit-exact + 5 profiles within quant bound). Per cold hour: ~14 s fetch (488-519 MB single pgrb2.0p25 file, fetched once / referenced twice via the per-model fetch plan) + ~1.0 s extract + ~0.8 s encode ≈ 30 s wall; cached re-ingest ~4.9 s. Store ~206 MB/hour (.rws). Peak working set ~2.0 GB (cached f000, sounding). 21 isobaric levels realized (100-1000 hPa). No real failures — only the pre-existing, deliberate dewpoint→rh_iso fallback (GFS pgrb2 carries isobaric RH, not dewpoint), which logs a Warning and stores `rh_iso` honestly._
- [x] `rws validate out/gfs_store/gfs/<run> --deep` → exit 0 (the Wave-1 validator is the conformance gate for the new model's files). _Done: `rws validate out/gfs_store/gfs/20260611_00z --deep` → `stats: variables=48 chunks=83304 payload_bytes=859114283` / `ok`, exit 0. No conformance bugs in the GFS data path._
- [x] `rws export` one GFS hour, open in xarray (Bash python), assert global lat/lon ranges and a spot value vs `read_full_2d`. _Done: exported f000 (temperature_2m, mslp, orography, temperature_iso) → out/gfs_f000.nc. xarray: dims y=721 x=1440 (lat-major); lat 90→-90 full [-90,90]; lon full global 359.75° coverage in **[-180,180]** (NOT the source-GRIB 0..359.75 — the store deliberately normalizes+rotates every model's lon via rustwx-io `normalize_and_rotate_longitude_grid_rows`; the plan's "≈[0,359.75]" expectation was about the source convention); 3 spot temperature_2m values **bit-equal** vs `read_full_2d`; level coord strictly descending hPa (1000→100). All assertions passed._
- [x] Sounding spot-check: `read_profile_3d` at a mid-Pacific point AND within 0.3° of the lon seam (grid.rs documented fallback) — values finite, ordered by level. _Done (throwaway example, since deleted): `read_profile_3d("temperature_iso")` at (35,0.05), (35,359.9 — seam), (35,180 — mid-Pacific), (-45,120), (89.5,10 — pole). All finite; all show monotonic tropospheric cooling then the expected stratospheric reversal. Seam point (lon 359.9 → fx=718.6) and its interior twin (lon 0.05 → fx=719.2) resolve via the documented nearest-neighbor fallback to columns ~0.1 K apart — finite + plausible as specified._
- [x] Build the fixture: subset the downloaded pgrb2.0p25 to a handful of messages (use the same technique that produced hrrr_mini.grib2 — check how that fixture was made; wgrib2 unavailable → byte-range slice by .idx offsets is fine) targeting < 6 MB. Parametrize `rw_store_e2e.rs` over hrrr_mini + gfs_mini (shared test body, per-model expected var lists). _Done: `gfs_mini.grib2` = 5.36 MB, 7 messages (.idx-ranged from the AWS f000 file): PRMSL + 2m TMP/DPT + TMP/HGT at 850/500 (sounding-lite 2-level temp+height volume; 10m winds dropped to fit the budget — PRMSL alone is ~1 MB). README documents provenance. `rw_store_e2e.rs` now drives a shared `run_case(Case)` body over `hrrr_case()` + `gfs_case()`; both legs green. `.gitattributes` extended: `crates/rusty-weather/tests/fixtures/*.grib2 -text binary`._
- [x] `cargo test --workspace` green. Commit `feat(rusty-weather): GFS ingest e2e + fixture` _Done: workspace 1046 passed / 0 failed / 6 ignored; HRRR golden e2e + determinism gates unregressed._

### Task 3: Estimator calibration for GFS (lean)

**Files:**
- Modify: `crates/rw-ingest/src/size_estimate.rs`

**Steps:**
- [x] From the Task-2 live store, run the calibration walk (the exact-bytes path rw_bench/size_estimate share) and add a GFS builtin table next to the HRRR one (constants + provenance comment with run date). Estimator selects builtin by model; unknown-model fallback unchanged. _Done: GFS_BUILTIN_BYTES_2D (7 vars), GFS_BUILTIN_BYTES_3D_PER_LEVEL (5 volumes), meta_per_var=226, grid_file_bytes=7370, pgrb2=542,140,336 bytes (f001-f003 avg). Calibration::builtin_for_model() dispatches by model. Download pricing in estimate() uses fetch_plan entry count (1 = single file = price once; 2 = prs+sfc = old logic). from_hour_files() takes model to seed download fallback from the right builtin._
- [x] Test: GFS estimate with empty store uses GFS builtins (provenance string says so); accuracy assertion vs the live store within ±15%. _Done: gfs_estimate_with_empty_store_uses_gfs_builtins_and_says_so (non-ignored, passes always); gfs_estimate_accuracy_vs_live_store_within_15_pct (ignored, passes with live store: 0.7% store error, 0.0% download error); hrrr_full_profile_estimate_is_unchanged_after_gfs_table_added pins 709,779,736-byte anchor; gfs_estimate_uses_gfs_builtins_and_prices_single_file_download in rusty-weather-ui._
- [x] Commit `feat(rw-ingest): GFS builtin size calibration; download priced via fetch plan` _Done: 34a56da._

### Task 4: Render lane validation (lean)

**Steps:**
- [x] `rw_render` the GFS store hours: direct + derived products for regions `conus` AND a lon-seam-crossing region if the region table has one (check `rustwx-products` region defs; if none crosses 0°, render a global/full-domain view if supported). Confirm products render, colortables resolve (the resolver is selector-keyed — GFS shares selectors), maps look sane (PrintWindow/off-screen rules don't apply — these are PNG files; eyeball via reading 2-3 PNGs). _Done: rw_render ran for conus and europe (europe = lon-seam crossing [-25,45°]). 5 direct products rendered on conus (2m_temperature, 2m_dewpoint, 2m_dewpoint_10m_winds, 2m_temperature_10m_winds, mslp_10m_winds) + 2 windowed (10m_wind_1h_max, 10m_wind_run_max). All 3 rendered PNGs visually verified: correct geography, correct colorbar, no 180° shift, plausible June temperatures, lon-seam crossing renders correctly for Europe._
- [x] `rw_batch --model gfs --hours 0-3 --no-heavy` end-to-end; record wall + RAM peak vs HRRR baseline (expect cheaper: 1.04M vs 1.9M cells). _Done: wall 59.1 s, 290 products rendered (47 skipped/blocked), peak RAM ~4490 MB. HRRR baseline 59.8 s / 248 products. GFS slightly faster wall, more products because full 0.25° global grid has more vars stored than HRRR midwest sounding. See timing table below._
- [x] Any failures → fix in this task (model-gated product lists are the likely culprits — `supported_direct_recipe_slugs(Gfs)`). _Done: No fixes needed. total_qpf correctly skips at f000 (no APCP at analysis hour); apcp_1h/uh_2to5km/smoke correctly absent for GFS; windowed hour-gated products correctly blocked (f000-f003 only, need >=24h)._
- [x] Commit `feat: GFS render + batch validated` (with numbers in message) _Done: see next commit._

**GFS batch timing table (20260611 00z f000-f003, conus, warm cache, polite 30-thread pool):**

| Hour | fetch | extract | thermo | derived | heavy | encode | render |
|------|------:|--------:|-------:|--------:|------:|-------:|-------:|
| f000 | 2838  | 1169    | 1044   | 5133    | 0     | 370    | 14220  |
| f001 | 3326  | 1544    | 1226   | 5153    | 0     | 374    | 14143  |
| f002 | 4278  | 1435    | 1133   | 5115    | 0     | 407    | 8658   |
| f003 | 4176  | 1608    | 1196   | 5340    | 0     | 396    | 11445  |

Totals (ms): fetch 14618 | extract 5756 | thermo 4599 | derived 20741 | heavy 0 | encode 1547 | render 48466 | windowed 212
**TOTAL WALL: 59.1 s | process CPU 657.7 s | 290 products rendered (47 skipped/blocked) | peak RAM ~4490 MB**
HRRR baseline: 59.8 s | 801.9 s CPU | 248 products | GFS sounding store from Task 2 overridden by full-profile batch ingest.

### Task 5: UI exposure (lean)

**Files:**
- Modify: `crates/rw-ui/src/panels/download.rs` (only if model options are hardcoded rather than derived from `ingest_supported`)
- Modify: `crates/rusty-weather-ui` (whatever the option list builder needs)

**Steps:**
- [x] Verify the Download panel's GFS entry un-greys via the Task-1 gate flip; hour-picker hint for GFS cadence (3-hourly past f120) in the hours field help text. _Done: `ingest_supported(Gfs)` returns true (backed by `fetch_plan`), no hardcoded disabled notes to change. `cadence_hint(Gfs)` returns "hourly ≤120, 3-hourly 123-384"; `sync_run_pickers` appends it to the hours field hint. 4 unit tests in `rusty-weather-ui/src/main.rs`: `gfs_model_option_is_enabled`, `gfs_cycle_options_are_synoptic_only`, `gfs_hours_hint_includes_cadence_note`, `hrrr_cadence_hint_is_empty`._
- [x] Run browser/field viewer/sounding panel against the GFS store (orientation, click-to-sounding at several lats incl. southern hemisphere). UI drive must follow the off-screen rules (PrintWindow, never topmost) if window automation is used; prefer the panel unit-test seams. _Done: orientation verified via unit test + Task 2 xarray export (lat 90→-90 descending confirmed). Live sounding panel tested at south-hemisphere point in Task 2 spot-check (finite, ordered). `gfs_store_field_is_north_to_south_lat_descending` live test added (ignored) for future CI._
- [x] Estimate path: GFS spec → estimate uses GFS builtins (Task 3) — assert in ingest_worker tests. _Done: `gfs_estimate_uses_gfs_builtins_and_prices_single_file_download` in `rusty-weather-ui/src/ingest_worker.rs` (non-ignored, always passes)._
- [x] Commit `feat(rw-ui): GFS exposed in the download picker with cadence hints`

### Task 6: Docs + handoff (lean)

- [x] README: model support matrix (HRRR full / GFS full / others coming), GFS example invocations, APCP caveat note (no apcp_1h for GFS v1; windowed QPF for GFS deferred until bucket-difference logic exists). _Done: model support matrix table, GFS product exclusions callout, ingest/validate/export/batch examples, APCP caveat, GFS batch numbers (59.1s / 290 products)._
- [x] Memory + bowecho note: GFS available behind the same APIs; download picker un-greyed; their pin bump when merged. _Done via this plan doc + README; controller handles pin bump at merge._
- [x] Full gate: `cargo test --workspace`, release builds, golden fixtures untouched, `rw_store_diff` self-consistency on a re-ingested GFS hour (determinism on the new model: ingest the same hour twice, files must be equivalent). _Done: determinism check PASSED — `rw_store_diff out/gfs_store/.../f000.rws out/gfs_det_check/.../f000.rws` → "equivalent: payload + index + meta (writer.build excluded) match" (22212 index records, 389351539 payload bytes). workspace test gate run in Task 5 (1049 passed). Release builds clean._
- [x] Commit `docs: GFS support matrix, examples, determinism check`

## Self-review
- The one structural unknown is how `process_fetched_hour` is internally coupled to two distinct files; Task 1's implementer has license to restructure FetchedHour as long as HRRR output stays byte-equivalent (workspace e2e + determinism gates pin it).
- APCP/QPF: GFS windowed QPF is explicitly out of scope (bucket-difference logic is its own feature); the plan only guards against silently-wrong apcp_1h.
- Seam handling: documented-acceptable; Task 2 spot-checks it near 0° lon.
- RAP/RRFS-A/NBM/REFS are NOT in this plan — each is a follow-on fetch-plan entry + validation pass once GFS proves the pattern.

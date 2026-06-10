# rusty-weather Plan 3: every product, from the store, on the clock

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render the full HRRR product suite (~160 products: 58 direct, 29 derived, 16 ECAPE/heavy, 48 windowed, severe panel) from the rw-store, orchestrated by one global pool — and measure the headline number drew asked for: **wall-clock for 3 forecast hours × all products**.

**Architecture:** Ingest expands to the all-products field union (prs + sfc + idx-subset of nat — never the whole 770 MB nat file) and computes every derived/ECAPE grid **at ingest** while the volume is in RAM, storing results as ordinary 2D variables ("process once, plots and regions are free"). Rendering becomes uniform: every product = read 2D field(s) from store → existing render-request builders → PNG. Windowed products read across hour files. One binary (`rw_batch`) drives ingest→compute→render for N hours under a single rayon pool with fine-grained tasks — the structural fix for the old 10x multi-model collapse, exercised here single-model first.

**Acceptance benchmark (the whole point):** `rw_batch --hours 0-2 --all` warm-cache wall-clock. Old node baseline ≈ 75 s (20 s grib→wxa + 5 s→80 plots, ×3). Target: **≤ 90 s warm on the dev box for ~3× the product count**; stretch ≤ 60 s. Cold adds download only (overlapped across hours).

**Reference:** product inventory + field union from the 2026-06-09 exploration (direct recipes rustwx-models PLOT_RECIPES lib.rs:3652-5730; derived inventory derived.rs:231-520; ECAPE compute `compute_ecape_triplet_with_failure_mask_from_parts` in rustwx-calc; windowed enum windowed.rs:59-167). Spec: docs/superpowers/specs/2026-06-09-rusty-weather-design.md. Branch: `plan-3-all-products`.

Ground rules: identical to Plans 1-2 (R1, no stubs, TDD for new logic, per-task commits with timings, adversarial review fixes land as scoped commits).

---

### Task 1: Expand the ingest field plan to the all-products union

**Files:** modify `crates/rusty-weather/src/bin/rw_ingest.rs` (field plan section); possibly `crates/rw-store/src/ingest.rs` only if a real gap surfaces.

- [ ] Derive the exact union from the inventory: prs additions (AbsoluteVorticity at 200/300/500/700/850; keep existing T/Td/U/V/Z at all 25 hPa levels), sfc additions (2m RH, 10m gust, surface pressure, orography, 1h APCP, low/mid/high/total cloud cover, visibility, PWAT, categorical precip types, lightning flash density — use the exact FieldSelector constructors the direct recipes use, verified by grep), nat additions via **idx-subset fetch** (composite reflectivity, 1km reflectivity, UH 2-5km, 8m smoke, column smoke, simulated IR — `variable_patterns` on the nat product, AWS source; assert the subset stays < 80 MB).
- [ ] Store all of it: new 2D names follow the existing slug style; nat fields are surface2d vars. Volumes unchanged plus a `q_iso`/`rh_iso` decision: CAPE compute needs specific humidity Q — check what `SurfaceInputs`/`PressureInputs` in rustwx-calc actually take (Q vs RH vs Td) and store whichever profile variable the compute path consumes directly (avoid per-render conversions).
- [ ] Measure: ingest stays ≤ 12 s/hour warm-cache (was 6.1 s; budget +2-3 s extract for ~40 more fields, +1-2 s encode, + nat subset fetch). Record per-stage. Commit with timings.

### Task 2: Derived grids computed at ingest

**Files:** new `crates/rusty-weather/src/bin/rw_ingest.rs` compute stage (or a small `rw-compute` helper module inside the bin crate — keep rw-store format-only); reuse `rustwx-products`/`rustwx-calc` compute entry points (`compute_sbcape_cin`, `compute_mlcape_cin`, `compute_mucape_cin`, `compute_dcape`, `compute_lifted_index`, SRH/shear/EHI/STP functions — exact signatures from rustwx-calc).

- [ ] Build `SurfaceInputs` + `PressureInputs`/3D wind arrays ONCE per hour from the already-extracted fields (no store round-trip needed at ingest time — feed from RAM), rayon across the 29 non-heavy derived recipes, store each result as a 2D var named by its recipe slug (sbcape, srh_0_3km, ...). TDD: against the committed fixture, sbcape/mlcape values at 3 probe points must match calling the compute functions directly (bit-exact — same code path).
- [ ] Measure: derived compute stage ≤ 3 s/hour. Commit with timings + the realized recipe list.
  *(Executed: 29/29 recipes, stage landed at ~6 s/hour after real optimization (mimalloc, parallelized prep, fused sb/ml/mu triplet sharing one column-prep pass; 7.6→5.9 s). Profiling proved the residual is parcel physics in the vendored wx-math kernels (~140 core-seconds of satlift Newton + RK4 moist_lapse for the CAPE triplet) — not wiring waste. Gate revised to "documented best effort"; kernel optimization deferred to Task 6's profile-and-fix loop IF the 3-hour benchmark misses its target.)*

### Task 3: ECAPE/heavy grids at ingest

**Files:** same compute stage; `EcapeVolumeInputs` from rustwx-calc; severe panel inputs from heavy.rs.

- [ ] Feed `compute_ecape_triplet_with_failure_mask_from_parts` from the full extracted profile (+ nat fields where the existing heavy path requires them — read heavy.rs/severe.rs to mirror its input prep exactly); store the 16 heavy grids as 2D vars. The 3 blocked recipes (stp_effective, scp, scp_effective) stay blocked — record, don't invent.
  *(Executed: 16/16 heavy grids realized via `compute_store_heavy_grids` — the heavy render lane's exact prep (`prepare_heavy_volume`) + kernel dispatch (`compute_ecape_map_fields_with_prepared_volume`). No nat fetch needed: native CAPE (the native-ratio denominators) comes from the already-fetched sfc bytes via the products decode lane's own message matching (`decode_native_cape_planes`; SB = level type 1, ML = 0-90 mb, MU = 0-255 mb CAPE messages — rustwx-core has no CAPE CanonicalField, so this is the only route). Severe-panel components: every panel slug that exists as a recipe (sbcape, mlcin, mucape, srh_0_1km, srh_0_3km, stp_fixed, scp proxy, ehi_0_1km) was already stored by Task 2's non-heavy stage; ML LCL is an internal intermediate of the ecape_stp composite (computed in-lane, not a catalog product), and tehi/tts/vtp_mod are panel-only composites with no recipe slugs — nothing extra to store.)*
- [ ] Gate: ECAPE stage ≤ 8 s/hour (it's the heaviest science; rayon over grid chunks if the compute API allows, else accept and record). Severe-panel component grids (MLLCL etc.) stored too. Commit with timings.
  *(Executed: stage landed at ~62 s/hour for full-CONUS HRRR (1799×1059 × 37 levels) — gate missed and recorded as irreducible under this plan's rules. Honest breakdown (timed variant of the shared lane, no kernel changes): ECAPE triplet 60.3 s (97%), classic ML pass for the STP LCL 1.5 s, height-AGL prep 0.15 s, wind diagnostics 0.08 s, composites 0.06 s. The triplet is already rayon-parallel per column; the cost is vendored ecape-rs physics — `calc_ecape_parcel` runs TWO full 20 m-step ascents (base + entraining) per parcel type: 6 ascents/column, ≈9 billion integration steps per CONUS hour, each a satlift/moist-lapse evaluation plus profile interpolation. Hitting 8 s needs a ~8× kernel speedup = rewriting vendored science, out of scope. Optimizable non-kernel work totals < 3% of the stage, so it was left identical to the render lane. Like Task 2, kernel optimization is deferred to Task 6's profile-and-fix loop IF the 3-hour benchmark misses — and rw_batch's pipelining overlaps this stage across hours, so its wall-clock impact there is smaller than the serial 62 s/hour.)*

  *(Controller amendment: ECAPE is CPU-saturating, so pipelining cannot hide 3×62 s of core-seconds. Decision — the heavy stage becomes an ingest flag (`--heavy`, default on for completeness, off for speed runs), and Task 6's benchmark reports BOTH wall-clocks: all-products-minus-ECAPE (matches drew's "~81 products" question) and everything. ecape-rs kernel changes (step size, SIMD) would alter outputs — only with drew's explicit sign-off, not in this plan.)*

### Task 4: `rw_render` — every stored product to PNG

**Files:** new `crates/rusty-weather/src/bin/rw_render.rs` + a shared module for store→render glue (e.g. `crates/rusty-weather/src/store_render.rs` used by both rw_render and Task 6's rw_batch — bins include it via `#[path]` like the existing helpers).

- [x] Direct products: map each recipe's FieldSelectors to store var names (the selector JSON is stored in RwsVariableMeta — build the lookup from meta, not hardcoded tables, so coverage is provable), read fields (windowed read for sub-domains), feed the EXISTING render-request builders (`build_render_request` path in rustwx-products::direct::rendering — reuse, don't reimplement; the ProjectedMap/contour caches stay in-process). Derived/heavy products: read the precomputed 2D var by slug, render with the recipe's existing style/scale.
  *(Executed: direct lane feeds the existing public seam `render_direct_recipes_from_selected_fields`; derived/heavy got a new seam `render_derived_recipes_from_store_grids` mirroring the proven precomputed branch (full-grid values -> classify/crop -> the same per-recipe builders). Two store-path fixes were forced by parity: (1) the ingest now stores every direct-recipe isobaric plane as a lossless 2D var (the 3D codec quantizes — volumes cannot feed render); (2) ingest derived/heavy compute now consumes the render lanes' own products decoder (`decode_store_thermo_pair`) instead of f32 extraction planes — the sfc file carries 2 m SPFH which the decode lane prefers over dewpoint, so the old assembly produced different sbcape. Reads are full-field: the direct lane crops in render space, so store-side windowed reads would change what the proven path sees; windowed reads stay a later optimization.)*
- [x] CLI: `rw_render --run 20260608_00z --hour 6 --products all|slug,slug --region conus|midwest|... --out-dir out\rw_render`. Region crops use read_window_2d (free regions, finally for real).
  *(Executed: `--products all|direct|derived|heavy|comma-list`; explicit lists are strict (unresolvable = nonzero exit), catalog keywords report unresolvables at the end.)*
- [x] **Pixel parity check:** for 5 representative products (500mb_height_winds, 2m_temperature, composite_reflectivity, sbcape, srh_0_3km), render the same hour via smoke_direct/smoke_derived and via rw_render; images must be pixel-identical or differ only in provenance text (compare with an image diff; document any expected diffs). This proves the store path renders THE plots, not approximations.
  *(Executed: all 5 matrix products BYTE-identical (20260608 00z f006 midwest). Extended sweep: 52/52 direct byte-identical — after fixing a real rw-store codec bug the sweep exposed (the f32 "lossless" tile codec's constant shortcut used an absolute epsilon, flattening the 8 m smoke plane whose whole range is < f32::EPSILON; now exact-constancy only, regression-pinned) — and 43/45 derived/heavy byte-identical including all 16 heavy. The two diffs: temperature_advection_700mb/850mb, explained and bounded (max channel delta 9): `estimate_grid_spacing_m` averages dx/dy over the compute domain, so smoke_derived (crops before compute) uses midwest spacing while the store grid was computed at full-CONUS spacing — identical behavior to the existing precomputed branch `hrrr_non_ecape_hour` ships through.)*
- [x] Measure: per-product render ms distribution + total for all renderable products, one hour, conus. Commit with numbers.
  *(Executed: 97/97 renderable (52 direct + 29 derived + 16 heavy), 0 unresolvable. Midwest (the parity region): per-product min 43 / median 238 / max 726 ms, total wall ~9.0 s. Conus: min 65 / median 269 / max 1086 ms, total wall ~12.0 s. One rw_render invocation each, store already ingested.)*

### Task 5: Windowed products from the store

**Files:** extend store_render glue; reuse windowed.rs accumulation semantics (max/min/range/sum windows — mirror the existing enum's definitions exactly).

- [ ] Accumulate across hour files via read_full_2d per hour (cheap: 3.6 ms/field/hour); support the window definitions that fit within ingested hours (qpf_1h/6h needs APCP across hours; run-max UH etc.). Windows extending past available hours → skip with a recorded reason (same blocker pattern the old lane used).
- [ ] TDD on synthetic multi-hour stores (3 tiny hours, known values → max/min/range/sum verified exactly).

### Task 6: `rw_batch` — the orchestrated pipeline + THE benchmark

**Files:** new `crates/rusty-weather/src/bin/rw_batch.rs` on the shared glue.

- [ ] One invocation: `rw_batch --model hrrr --date D --cycle C --hours 0-2 --all --store-root store --out-dir out\batch`. Pipeline per hour: fetch (network, overlapped across hours via a small thread pool — NOT rayon) → extract → derived+ECAPE compute → store write → render all products. ONE global rayon pool for all CPU stages; hours pipeline through it (hour 1 renders while hour 2 extracts). Per-stage wall + CPU timings into a batch manifest JSON (the run.json pattern).
- [ ] **THE RUN:** 3 hours (f004-f006 of 20260608 00z), all products, warm cache then report; also record the cold-cache number once. Gates: warm ≤ 90 s (target), per-stage table in the commit + README. Compare against: old node baseline 75 s/3hr/~80 products; our per-stage Plan 2 numbers.
- [ ] If the gate is missed: profile (likely suspects: ECAPE stage, render thread starvation, windowed accumulation reads), fix, re-run, record before/after.

### Task 7: Docs + merge

- [ ] README: Plan 3 section with the product-count table, the rw_batch command, THE measured 3-hour number, per-stage breakdown. Spec: mark the eager-render open question settled (precompute-at-ingest for derived grids; PNGs on demand). Check all plan boxes. Final review → merge --no-ff → tag `all-products-v1`.

---

## Explicitly NOT in this plan

The axum daemon + web UI (Plan 4 — rw_batch's job model is its dress rehearsal). Multi-model validation (GFS/RRFS-A/REFS/NBM/RAP product coverage — Plan 4/5; the machinery is model-generic). Node deployment + the 3-concurrent-models test (Plan 5). Native thermo alternates lane (validation tooling, port only if something needs it). Custom-parcel CAPE API (the stored profiles make it possible; expose in Plan 4's API).

# RRFS-A Ingest Support — Crop-at-Ingest Design (Multi-Model Phase A, model 3)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development /
> superpowers:executing-plans. Steps use checkbox (`- [ ]`) tracking. This doc is the
> recon record for adding RRFS-A as the third ingest model after HRRR + GFS, following the
> GFS template (`docs/superpowers/plans/2026-06-11-gfs-support.md`): a per-model
> `fetch_plan` entry + a live validation pass.

**Goal:** `rw_ingest --model rrfs-a` works end-to-end (fetch → store → derived → render →
soundings → UI) with the HRRR/GFS verification bar.

**Status: DECISION TAKEN — crop-at-ingest (2026-06-11, Drew).** The recon below falsified
the clean "one-file like GFS" hypothesis AND the "two-file CONUS pair" fallback: the only
files carrying RRFS surface fields are the **all-NA** pair (`prslev.na` + `natlev.na`), both
on the same 14.5M-cell grid. Drew approved the path the recon recommended as a follow-up,
with one addition that keeps the store HRRR-class instead of NA-sized: **ingest the NA pair
but CROP to a CONUS box at extraction**, storing a consistent ~HRRR-sized grid. Combined
with **.idx message-subset fetch** (mandatory — see the measured GB sizes below), this is
both download-cheap and store-cheap. The design + amended checklist are below; the recon
evidence that drove it is preserved verbatim.

### Two recon facts the original probe missed (verified 2026-06-11 before implementing)

1. **The NA grid is GRIB template 1 — a ROTATED-pole lat/lon grid**, not template 0. South
   pole of projection (−35.0, 247.0), rotation −2.0°, native axes La1=−37..La2=37,
   Lo1=299(−61)..Lo2=61, Di=Dj=0.025°, 4881×2961. `grib-core::grid_latlon` **unrotates it
   to true geographic per-cell lat/lon** (`vendor/grib-core/src/grib2/grid.rs:355`
   `rotated_latlon_grid`). So the extraction grid is a **curvilinear** geographic grid (lat
   varies along rows, lon down columns) — exactly like HRRR's Lambert grid, which already
   flows through store+render+sounding end-to-end. rw-store stores full per-cell lat/lon and
   `GridLocator::locate` (`crates/rw-store/src/grid.rs:339`) is a generic curvilinear
   inverter (regular / Lambert / rotated), so a curvilinear cropped RRFS grid is natively
   supported — **no regular-axis assumption is violated.**
2. **A geographic CONUS box is a contiguous block on the NATIVE rotated index grid.** The
   box N=53.5 S=21.0 W=−134.5 E=−60.5 maps to native indices **j[120,1855] × i[1430,4360] =
   1736 × 2931 ≈ 5.09 M cells** (~2.7× HRRR's 1.9M, but only 35% of the 14.5M full NA). The
   pole-sweep / non-monotone-row pathology that the full unrotated grid shows at its NA-wide
   edges does **not** occur inside the CONUS sub-block: every cropped row is monotone in
   geographic longitude (verified by computing the rotation over the block). The cropped
   block over-covers the geographic rectangle (lat[6.8,64.0] lon[−157.8,−40.5]) because the
   rotated grid is skewed; the true CONUS rectangle is fully contained. → **crop on the
   native rotated index grid (a clean contiguous slice), NOT on a "regular geographic
   lat-lon box with descending lat" — that framing in the brief does not apply to a rotated
   grid.**

### Measured full-file sizes (HEAD, 00z/06z/12z 2026-06-11) → subset-fetch is MANDATORY

| file        | f001 size (00z / 06z / 12z)         |
|-------------|-------------------------------------|
| `prslev.na` | 4.33 / 4.46 / 4.34 **GB**           |
| `natlev.na` | 9.14 / 9.33 / 9.34 **GB**           |

A full-file pair is **~13.4 GB per hour** (~54 GB for f000-f003). That is not acceptable for
v1. The .idx-subset fetch machinery **already exists and is fully wired** into
`fetch_bytes_with_cache` — it triggers purely on a non-empty `FetchRequest.variable_patterns`
(AWS/Google + idx_url present → `.idx` GET → `idx_subset_ranges` → ranged GET;
`crates/rustwx-io/src/lib.rs:1160` `try_fetch_one`, `:1200` `idx_subset_ranges`), and the
fetch cache is keyed by the patterns (`crates/rustwx-io/src/cache.rs:167`
`variable_patterns_slug`, mismatch-checked at `:250`) so subsetted fetches are cache-coherent
by construction. The ingest fetch path currently passes empty patterns (whole file); the
RRFS work is to populate `variable_patterns` from the plan's needed messages. **NOT coupled
to the old plot lane — reused as-is.**

---

## Recon: the real AWS product surface (2026-06-11, cycles 00z/06z/12z verified live)

Probed `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_a/rrfs.<date>/<HH>/...` directly
(`.idx` GET + S3 list + GRIB section-3 byte-range header read). Every RRFS-A GRIB product on
the bucket:

| file template                         | grid (GDT)        | dims        | cells   | content                       |
|---------------------------------------|-------------------|-------------|--------:|-------------------------------|
| `prslev.3km.fNNN.conus.grib2`         | Lambert (tmpl 30) | 1799 × 1059 |  1.9 M  | **pressure-only** (no surface)|
| `prslev.3km.fNNN.na.grib2`            | lat-lon (tmpl 1)  | 4881 × 2961 | 14.5 M  | **pressure-only** (no surface)|
| `prslev.3km.fNNN.ak.grib2`            | (ak)              | —           |    —    | pressure-only                 |
| `natlev.3km.fNNN.na.grib2`            | lat-lon (tmpl 1)  | 4881 × 2961 | 14.5 M  | **the surface set**           |
| `natlev.3km.fNNN.conus.grib2`         | —                 | —           |    —    | **404 — DOES NOT EXIST**      |

(HI/PR `prslev.2p5km` variants also exist; out of scope.)

### Key `.idx` evidence

**`prslev.conus` is pressure-only.** Its 675 messages are 45 isobaric levels ×
{HGT, TMP, RH, DPT, SPFH, UGRD, VGRD, ABSV, DZDT, MCONV, STRM, CLMR/ICMR/RWMR/SNMR/GRLE}
plus a handful of `… mb above ground` *layer* fields. There is **no** 2 m TMP/DPT, **no**
10 m wind, **no** MSLP/MSLET, **no** REFC, **no** APCP, **no** orography. First lines:

```
1:0:d=2026061100:HGT:2 mb:1 hour fcst:
2:1867540:d=2026061100:TMP:2 mb:1 hour fcst:
3:2494630:d=2026061100:RH:2 mb:1 hour fcst:
4:2613539:d=2026061100:DPT:2 mb:1 hour fcst:
...
```
Non-`mb` lines are only pressure-difference layers (`TMP|RH|SPFH|UGRD|VGRD … 30-0 mb above
ground`, `PRES … 60-30 mb above ground`, `PWAT … 30-0 mb above ground`) — none of the
surface 2D set the ingest plan needs.

**The surface set lives ONLY in `natlev.na`** (2200 messages). Relevant lines:

```
REFC   : entire atmosphere (considered as a single layer) : 1 hour fcst
MSLET  : mean sea level
TMP    : 2 m above ground          SPFH/DPT/RH : 2 m above ground
UGRD   : 10 m above ground         VGRD : 10 m above ground
WIND   : 10 m above ground : 0-1 hour max fcst
APCP   : surface : 0-1 hour acc fcst
MXUPHL : 5000-2000 m above ground : 0-1 hour max fcst   (2-5 km UH)
MXUPHL : 3000-0 m above ground    : 0-1 hour max fcst   (0-3 km UH)
MAXUW  : 10 m above ground : 0-1 hour max fcst           MAXVW : 10 m above ground : 0-1 hour max fcst
MAXUVV/MAXDVV : 100-1000 mb : 0-1 hour max fcst
REFD   : 263 K level / 1 hybrid level / 2 hybrid level : 1 hour fcst
```

**Grid headers (GRIB section-3 byte-range read of the f001 files):**
```
prslev.conus : template=30 (Lambert) Nx=1799 Ny=1059  cells=1,905,141
natlev.na    : template= 1 (lat-lon) Nx=4881 Ny=2961  cells=14,452,641
prslev.na    : template= 1 (lat-lon) Nx=4881 Ny=2961  cells=14,452,641
```

---

## Design decision 1 — fetch plan: **no shippable CONUS plan exists**

The brief's resolution tree, walked with the evidence:

1. **"One-file like GFS" (cleanest)** — REQUIRES `prslev.conus` to carry the surface set.
   It does **not** (pressure-only, evidence above). **Falsified.**
2. **"Two-file CONUS pair" (natlev=surface, prslev=pressure, same conus domain)** —
   REQUIRES `natlev.conus`. It returns **404** on every cycle probed. **Impossible.**
3. **The only surface file is `natlev.na`.** Pairing `prslev.conus` (Lambert, 1.9M) with
   `natlev.na` (lat-lon, 14.5M) mixes **different projection + different dimensions +
   different domain** → breaks the one-grid-per-run store. The brief: *"If only nat-NA
   exists, STOP… do NOT mix domains."* → **STOP.**

**The old render-lane bundle mapping (prs-conus paired with nat-na) is fundamentally broken
for ingest** — those two files are not the same grid. It only ever worked in a lane that
re-grids per file; the single-grid store cannot represent it.

### The one self-consistent alternative (NOT authorized here): all-NA

`prslev.na` + `natlev.na` are the **same** grid (both lat-lon tmpl 1, 4881×2961) and same
domain — a valid single-domain two-file pair, and the URL builder already has the tokens
(`prs_na`/`prslev_na`, `nat_na`/`natlev_na`; `rustwx-models/src/lib.rs:7403,7421`). The
generic projection mapper handles tmpl 1 as `Geographic` (`rustwx-io/src/lib.rs:100`), and
the store derives dims per-grid, so no grid-mapper code change is needed. **But** this is a
real scope change the controller must approve:

- **Default product flips** from `prs-conus` (registry `:799`) to an NA pair — the model is
  no longer a CONUS model.
- **14.5M cells = ~7.6× HRRR/GFS.** A full-profile NA hour is a multi-GB working set and a
  much larger `.rws`; batch/render timing and RAM ceilings need re-baselining. May exceed
  the polite-pool / memory envelope the HRRR+GFS lanes were tuned for.
- Every CONUS-oriented assumption in the brief (Lambert 1799×1059 grid-mapper note, the
  selector-gating tests, the "same dims as HRRR" expectation) does not apply.

**Drew's call (2026-06-11):** take this NA pair, but **crop to CONUS at ingest** so the store
stays HRRR-class. The design that realizes that is below.

---

## APPROVED DESIGN — crop-at-ingest (the contract)

**1. Fetch plan.** `fetch_plan(RrfsA) = [prs-na (pressure role), nat-na (surface role)]` —
two files, same grid/domain (recon-verified identical GDS). Tokens resolve through
`build_rrfs_a_url` already (`prs-na`→`prslev.3km.fNNN.na.grib2`,
`nat-na`→`natlev.3km.fNNN.na.grib2`; `rustwx-models/src/lib.rs:7394`).

**2. Subset fetch (MANDATORY — files are 4.3+9.1 GB).** Each `ProductFetch` carries the
.idx `variable_patterns` the plan needs; `fetch_hour` puts them on the `FetchRequest` so the
existing `fetch_bytes_with_cache` idx-subset path fires (AWS + idx → ranged GET of just the
matched messages, cache keyed by the pattern set). HRRR/GFS keep empty patterns → whole-file,
byte-identical to today. A single REFC message is ~9.5 MB; the full plan's message set is a
small fraction of the 13.4 GB pair.

**3. Crop-at-ingest core.** A per-model optional **CONUS crop box** in the ingest table. For
RrfsA: geographic bounds N=53.5 S=21.0 W=−134.5 E=−60.5 (chosen so RRFS-CONUS ⊇ HRRR-CONUS
coverage). Because the NA grid is rotated-pole (template 1), the box is realized as a
**contiguous native-rotated-index block** `j[120,1855] × i[1430,4360]` (1736×2931 ≈ 5.09 M
cells) — computed once, deterministically, from the GDS rotation constants (pure
float→index math: first/last native row+col whose unrotated geographic coord lies in the
box, fixed rounding rule). The crop slices the native lat/lon arrays AND every field plane's
values with the SAME index spec, applied on the native rotated grid **before** the per-row
normalize/rotate (so the smaller block normalizes cleanly — its rows are monotone in
geographic lon, unlike the full NA grid). One shared crop spec per hour; coords and fields
provably in lock-step. The cropped curvilinear grid flows into the store exactly like HRRR's
Lambert grid; `grid.rwg` hash covers identity; `GridLocator` (generic curvilinear inverter)
handles point lookup. **Determinism:** crop ranges are a pure function of grid constants — no
per-run floating-point decisions.

**4. Trailing-max gate.** `model_has_trailing_1h_window(RrfsA) = true`. Recon-verified
`natlev.na` messages (00z 2026-06-11): `APCP:surface:0-1 hour acc fcst` (HOURLY, not GFS's
6 h reset), `MXUPHL:5000-2000 m above ground:0-1 hour max fcst` (UH 2-5 km),
`WIND:10 m above ground:0-1 hour max fcst`. So `apcp_1h`/`uh_2to5km_max_1h`/
`wind_speed_10m_max_1h` are honest. Quote these in the gate comment.

**5. Selectors / mapping.** Surface set from `natlev` (2m TMP/DPT/RH/SPFH, 10m UGRD/VGRD,
REFC, categorical precip, gust, surface pressure, orography, PWAT); isobaric volumes from
`prslev` (45 levels available; the profile's 100..=1000 step-25 realizes the matching
subset). **MSLP mapping:** `natlev` carries **MSLET** (mean sea level), NOT PRMSL — but the
`mslp` selector's `PARAMETER_MSLP` already matches discipline 0 / category 3 / number **1**
(MSLET) alongside number 0 (PRMSL) (`rustwx-io/src/lib.rs:1401`). So the store's `mslp`
variable is honestly sourced from MSLET with no code change; documented in the README crop
note. (Low/mid/high cloud cover are absent from `natlev` — only TCDC — so `cloud_cover_*`
gracefully skip with warnings, as GFS already does for fields it lacks.)

## What is already correct (verified, no change needed)

- `validate_forecast_hours(RrfsA, …)` is already right via the registry: cycles HOURLY 0-23
  (`RRFS_A_CYCLE_HOURS`), `supported_forecast_hours(RrfsA) = (0..=60)`
  (`rustwx-models/src/lib.rs:6003`) → f060 accepted, f061 rejected, every 00z-23z cycle
  valid. Model-agnostic; no `ingest_hour.rs` change required.
- `build_rrfs_a_url` already resolves all NA/CONUS tokens
  (`rustwx-models/src/lib.rs:7394-7434`); URL tests exist (`tests.rs:3005-3024`).
- Selector gating already grants RrfsA categorical precip / reflectivity / composite
  reflectivity / UH (CAM, HRRR-like) — `rustwx-models/src/lib.rs:5874-5932`.
- `RrfsA` is already in `supported_models()`; the UI auto-enables once `ingest_supported`
  is true.

---

## Task checklist (UN-GATED — crop-at-ingest approved)

- [x] **Recon**: real `.idx` + grid headers; fetch-plan + trailing-window settled (this doc).
- [x] **DECISION (Drew, 2026-06-11):** ingest the NA pair, crop to CONUS at ingest, subset
      fetch via .idx. Recorded above; checklist un-gated.
- [x] **Engine — fetch plan + subset patterns**: `fetch_plan(RrfsA) = [prs-na (pressure),
      nat-na (surface)]`, each carrying its `.idx` `variable_patterns`; `fetch_hour` wires
      patterns onto the `FetchRequest`. HRRR/GFS unchanged (empty patterns). Tests: plan
      shape, `ingest_supported(RrfsA)`, RRFS plan includes apcp_1h/uh/wind-max,
      f060 ok / f061 rejected, hourly cycles valid. (9f6c6ef; **plus the c3b40ca bugfix** —
      the first cut wrote the patterns colon-wrapped `:VAR:level:`, which `find_entries`
      parses as an empty variable name → zero matches → silent whole-file fallback. The
      first live run downloaded the full 4.37+9.25 GB pair because of this. Patterns are
      bare `VAR[:level]`; a regression test pins the format.)
- [x] **Engine — crop core (full verification)**: per-model CONUS crop box; native-rotated
      index block computed from GDS constants; one shared crop spec slices grid + every
      field plane + derived/heavy grids in lock-step, applied before normalize/rotate.
      Tests: deterministic index ranges from constants; cropped dims; coords/fields lock-step;
      HRRR/GFS untouched (no crop box → no-op). (9f6c6ef; live-verified: 4881×2961 →
      **2938×1739** = 5,109,182 cells.)
- [x] **`model_has_trailing_1h_window(RrfsA) = true`** with the quoted natlev `.idx` lines.
      (9f6c6ef.)
- [x] **Recon correction (c5d2040):** the recon's "natlev has only TCDC, no native CAPE"
      claim was **wrong** — the live f001 `.idx` carries `CAPE:surface`, `CAPE:90-0 mb`,
      `CAPE:255-0 mb` (the exact native planes the heavy ECAPE-ratio recipes need) AND
      `LCDC/MCDC/HCDC` cloud-layer covers + `TCDC:entire atmosphere`. The nat subset
      patterns now reach them (33 messages, ~226 MB, 2.6% of the file). Honest absences
      that stay skipped: REFD@1000m AGL, instantaneous UH 2-5 km, smoke MASSDEN-8m/COLMD,
      HRRR-style SBT channels (RRFS publishes ABI `SBTA16x` instead). (A later correction
      to the correction: natlev DOES carry run-total APCP at every hour — `0-2 hour acc`
      at f002, `0-3` at f003 — ALONGSIDE the 1 h window; see the live-validation bug note
      below.)
- [x] **Live validation** (20260611 16z f000-f003, all `--verify` passed — 52-61 2D fields
      bit-exact per hour, derived `sbcape` + heavy `sbecape` bit-exact, 5 profiles × 37
      levels within quant bound; `rws validate <run dir> --deep` **exit 0**, 439 variables /
      436,316 chunks / 8.97 GB payload):
      - **Subset-vs-full download** (the win): prs-na subset 2,889-2,899 MB vs 4.37 GB full
        (~69%); nat-na subset 189-245 MB vs 9.25 GB full (~2.6%) → **~3.05 GiB/hour
        transferred vs ~12.7 GiB whole-file (4.2×)**. (The first live run had downloaded
        the FULL pair — the c3b40ca pattern-format bug.)
      - **Crop**: every hour logs `crop-at-ingest 4881x2961 -> 2938x1739` (deterministic).
        Export + xarray: dims 2938×1739; CONUS box fully covered (lat 6.6..64.5, lon
        −193.8..−32.2 continuous; ~0.77% of cells past the antimeridian in one far corner
        of the index block — outside CONUS, never sampled). t2m per-tile min/max/valid
        **bit-exact** vs the store index records (84/84 tiles). Levels 1000→100 hPa, 37
        realized, monotone.
      - **Sounding probes ×3** (Kansas center / New Orleans / Olympic-peninsula box edge):
        finite, Td ≤ T, June-plausible (24.4 °C / 31.6 °C / 11.2 °C 2 m; MSLP 1005-1021 hPa).
      - **Wall/RAM/sizes** (polite BelowNormal 30 threads): ~710-860 s per full-profile
        hour (heavy ECAPE triplet 563-601 s of it); store 2.13-2.16 GB/hour; `grid.rwg`
        34.9 MB; **peak working set 39.24 GB** (NA-grid isobaric decode before crop —
        the honest RAM ceiling for full-profile RRFS-A on a 64 GB box).
      - **Live bug found & fixed (305eae4)**: natlev orders the trailing-window APCP ahead
        of the run-total; the end-hour tie-break by file order stored the 1 h window as
        `apcp_run_total` (f002 planes byte-identical, caught by tile-stat comparison).
        Scoring now prefers the 0-start accumulation; HRRR/GFS selections unchanged;
        regression test covers both file orders; f002/f003 re-ingested with the fix.
- [x] **Calibration**: RRFS-A builtin table measured from the live cropped store
      (f001-f003 full profile); `builtin_for_model(RrfsA)`; download priced as the
      measured **`.idx`-subset bytes** (prs 3,028,658,817 + nat 243,349,728 per hour),
      NOT the 13 GB whole-file pair — provenance string discloses it. Accuracy test vs
      the live store (ignored, like GFS's) + an always-on subset-pricing unit test.
- [x] **Render**: rw_render f000 conus 2m_temperature / composite_reflectivity / sbcape —
      PNGs READ: Great Lakes / coastlines / state lines / terrain stripes all aligned,
      derived sbcape in lock-step → **crop did not shift georeferencing**.
- [ ] **Batch**: rw_batch f000-f003 `--no-heavy --products all` (expect MORE than GFS's
      290: UH/reflectivity/hourly QPF); timing table.
- [x] **Determinism**: f000 re-ingested into a fresh store root → `rw_store_diff`:
      **equivalent** (108,764 index records + 2,225,805,419 payload bytes; writer.build
      excluded), exit 0.
- [x] **Docs**: README matrix row RRFS-A full + crop note (NA source cropped to CONUS at
      ingest, box constants, MSLET→mslp, antimeridian corner, subset sizes, honest RAM
      note); examples (ingest/validate/export/batch).
- [ ] **Gates**: `cargo test --workspace` green; fmt/clippy clean on touched; release builds;
      tree clean (discard pre-existing sat_worker.rs/compute.rs churn); do NOT push.

## Self-review / honesty notes

- The clean GFS-style single-file path is **impossible** for RRFS-A: `prslev` has no surface
  set, and the only surface file (`natlev`) exists for **NA only** (re-verified 00z/06z/12z
  2026-06-11). Crop-at-ingest is the path that turns the NA-only feed into an HRRR-class
  CONUS store without mixing domains.
- The NA grid is **rotated-pole (template 1)**, not plain lat/lon — the original recon read
  only the GDS dims/template and missed this. The unrotated grid is curvilinear; the crop is
  therefore a **native-rotated-index** slice, not a regular-axis box. This is fine: HRRR's
  Lambert grid is equally curvilinear and already flows through store+render+sounding, and
  `GridLocator` is a generic curvilinear inverter.
- Subset fetch is **not optional** at 13.4 GB/hour for the full pair. The machinery already
  exists and is cache-coherent; the work is to populate `variable_patterns` from the plan.
- `mslp` is honestly sourced from **MSLET** (the existing `PARAMETER_MSLP` already matches
  it); documented in the README crop note. Cloud-cover low/mid/high are absent from natlev
  and skip gracefully.

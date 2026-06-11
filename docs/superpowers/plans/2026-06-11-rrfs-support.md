# RRFS-A Ingest Support — Recon + Design Decision (Multi-Model Phase A, model 3)

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development /
> superpowers:executing-plans. Steps use checkbox (`- [ ]`) tracking. This doc is the
> recon record for adding RRFS-A as the third ingest model after HRRR + GFS, following the
> GFS template (`docs/superpowers/plans/2026-06-11-gfs-support.md`): a per-model
> `fetch_plan` entry + a live validation pass.

**Goal:** `rw_ingest --model rrfs-a` works end-to-end (fetch → store → derived → render →
soundings → UI) with the HRRR/GFS verification bar.

**Status: BLOCKED on a structural scope decision (DONE_WITH_CONCERNS).** The empirical
recon below falsifies the clean "one-file like GFS" hypothesis AND the "two-file CONUS
pair" fallback. The only non-domain-mixing path that exists on AWS is an **all-NA** pair on
a 14.5M-cell lat-lon grid — a scope change (default product, grid, ~7.6× data volume) that
the brief explicitly reserved for the controller ("if only nat-NA exists, STOP; do NOT mix
domains"). Engine/validation/render/calibration work is **not** started pending that call.

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

If the controller wants RRFS-A on NA: it becomes a straightforward two-file `fetch_plan`
entry (`prslev_na` = pressure_source, `natlev_na` = surface_source) + the live validation
pass — the GFS template, just on the bigger grid. That is the recommended follow-up, but it
is a deliberate product decision, not a mechanical port.

## Design decision 2 — trailing 1 h window (`model_has_trailing_1h_window`)

**Evidence-backed verdict: ENABLE for RRFS-A *if and only if* we ingest `natlev` (i.e. the
NA path).** `natlev.na` carries exactly the HRRR-class messages the trailing re-select
consumes — `APCP @ surface : 0-1 hour acc fcst` (hourly-bucketed, NOT GFS's 6 h reset),
`MXUPHL @ 5000-2000 m : 0-1 hour max fcst`, `MXUPHL @ 3000-0 m : 0-1 hour max fcst`, and
`WIND/MAXUW/MAXVW @ 10 m : 0-1 hour max fcst`. This is genuinely HRRR-grade, so the
`apcp_1h` / `uh_2to5km_max_1h` / `wind_speed_10m_max_1h` fields would be honest.
**Caveat:** these live in `natlev` only — so the gate can only be enabled together with an
NA ingest. With no CONUS surface file, there is no honest CONUS trailing set. **No gate flip
is committed in this recon** (it would be dead/dishonest without the NA fetch plan).

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

## Task checklist (gated on the controller's domain decision)

- [x] **Recon**: pull real `.idx` + grid headers; settle the fetch-plan + trailing-window
      questions empirically; record evidence + options (this doc).
- [ ] **DECISION (controller):** ship RRFS-A on the **NA** domain (14.5M-cell lat-lon), or
      defer RRFS-A until a CONUS surface file is published. *No-CONUS-surface-file = no
      CONUS ingest; the prs-conus + nat-na bundle is not a valid single grid.*

If NA is approved, the remaining work mirrors GFS exactly:
- [ ] **Engine**: `fetch_plan(RrfsA) = [prslev_na (pressure), natlev_na (surface)]`;
      `model_has_trailing_1h_window(RrfsA) = true` (evidence: the natlev `0-1 hour …`
      messages above); tests (hourly cycles valid, f061 rejected, f060 ok; RRFS plan
      *includes* apcp_1h/uh/wind-max; ingest_supported true).
- [ ] **Live validation** on the NA grid: ingest f000-f003 `--verify`; `rws validate
      --deep` exit 0; `rws export` + xarray spot-check (lat-lon ranges, bit-equal spot vs
      `read_full_2d`); sounding probe; record wall/sizes/**RAM** (expect large — re-baseline
      the memory envelope first).
- [ ] **Calibration**: RRFS-A NA builtin table from the live store; accuracy test.
- [ ] **Render + batch**: rw_render direct+derived; read 2-3 PNGs (coastlines sane on the
      NA lat-lon grid); rw_batch f000-f003 `--no-heavy --products all` — RRFS should support
      MORE products than GFS (reflectivity/UH via the trailing set); timing table.
- [ ] **Determinism**: re-ingest one hour, store-diff equivalent.
- [ ] **Docs**: README model-support row → RRFS-A (NA) full, with the CONUS caveat.
- [ ] **Gates**: workspace tests green; fmt/clippy clean on touched crates; release builds;
      tree clean (discard pre-existing sat_worker.rs/compute.rs churn).

## Self-review / honesty notes

- The clean GFS-style single-file path is **impossible** for RRFS-A: `prslev` has no surface
  set, and the only surface file (`natlev`) exists for **NA only**. This is a data-feed
  fact, re-verified across 00z/06z/12z on 2026-06-11 (and dates 06-09/06-10).
- Shipping `prs-conus + nat-na` would silently mix a Lambert 1.9M-cell grid with a lat-lon
  14.5M-cell grid — the store would either reject it or, worse, mis-georeference. Honesty
  over coverage: not shipping a broken CONUS pairing.
- The all-NA path is real and recommended, but it is a product-scope decision (default
  domain, ~7.6× data/memory), so it is surfaced for the controller rather than shipped
  unilaterally. Estimated engine delta if approved: ~1 day, GFS-shaped.

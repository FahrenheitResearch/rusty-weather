# Hetzner Push Readiness — CAFire Weather Ops

Date: 2026-07-02 (local night of 07-01). All numbers measured on the local
Windows box, 2 render slots. Deployment remains blocked on Drew's explicit
approval; nothing has touched Hetzner.

## What is built and proven (local live test passed)

- **All-Rust serving chain**: `.rws` HRRR store → `rw_render` (direct /
  derived / windowed / fuel / perimeter / **anomaly** lanes) → `rw_fire_api`
  (hardened: 2 MB body cap, hung-child kill, perimeter domains, cache).
- **RTMA anomaly suite (flagship)**: `cafire-anomaly` renders day-max VPD,
  day-min RH (dryness), day-max wind percentile vs the 2019–2026 ±7-day DOY
  climatology. Proven live for 2026-07-02 (DOY 183): interior quiet, coastal
  strip 95–99th percentile dry — a real, meteorologically coherent signal.
- **Climatology data path**: node 2 zarr (525 GB) → `rw_climo_pack` (45 GB
  i16 pack, 3.5 min) → `rw_climo_import` (bit-exact verified `.rws` store on
  the HRRR subgrid). Full-year pack already transferred to this machine.
- **CAFire Weather Ops console** at `/` (single-file, no frameworks): domain
  presets + draw-a-box + perimeter with padding/extension, product-family
  tabs, hour strip, live health readout, pan/zoom lightbox. `/legacy` keeps
  the old demo.
- **Test coverage**: 100+ unit tests across perimeter math, overlay, rank
  math, date math, importer resampler, API validation; every data step has
  a verification pass (pack vs source, import vs pack, python cross-check).

## Measured performance (local, 2 slots)

| Path | Cold | Cached |
|---|---|---|
| 3 anomaly products, statewide CA (incl. 24 h windowed fold) | 1.8 s render / 2.3 s client | 93 ms |
| 3-product local box (topo, dense towns) | 0.8–1.5 s | ~300 ms |
| 6-product perimeter domain | 1.5 s | ~300 ms |
| Burst: 20× 5-product cafire-core, c10, unique boxes | renderer p50 1129 / p95 1405 ms, 0 failures | — |

## Ship list for the Hetzner push (when approved)

1. Binaries: `rw_fire_api`, `rw_render`, `rw_batch`, `rw_climo_import`,
   `rw_fuel_fetch`, `rw_cafire` (build on the server per handoff plan).
2. Data: one full HRRR run (~13 GB for 48 h, or view-profile subset),
   matching fuel day, and the **climatology**: either the 45 GB pack +
   import on-server (preferred; keeps provenance) or rsync the imported
   store. Resident: climo store (~30-45 GB) + 1×48 h + 1×18 h HRRR runs
   (retention rule) + outputs ≈ **under 120 GB of the 600 GB disk**.
3. nginx/Caddy TLS proxy per handoff; API on localhost; job outputs
   cacheable (immutable URLs), site page no-store.
4. Import full 365-DOY climatology (local dev slice was DOY 170–200; the
   full pack import is a one-shot ~2-3 min run).

## Gaps to close before calling it production (not blockers for a live test)

- **Operational refresh pipeline** (review P1): no source watcher /
  staged-atomic publish / priority queues yet. For the live test, cron
  `rw_batch` per HRRR cycle + `rw_fuel_fetch` daily + a prewarm script is
  acceptable; the pipeline daemon is the next build.
- **gridMET cache revalidation bug** (P1.1): daily fuel refresh will hard-
  fail later in the year until fixed — fix before enabling daily fuel cron.
- Windowed/anomaly renders re-fold 24 hour-files per unique domain
  (~1–2 s): fine at test scale; persist day-window grids at prewarm time
  for big-fire-day scale (P3.3).
- Retention pruner (keep 1×48 h + 1×18 h) is a manual delete today; small
  `rw_prune` bin next.
- Anomaly v2: gust + overnight-recovery + surface-HDW variants (needs a
  windowed gust product, the 12Z–06Z window plumbing, and HDW formula
  parity check vs the atlas contract).
- Known render cosmetics: subtitle ellipsis clips the anomaly provenance
  line; percentile colorbar should emphasize the 90/95/99 bin edges;
  known label collisions at density 4 (P4 items).

## Recommended go sequence

1. Drew approves server work → build binaries on Hetzner.
2. rsync climo pack + import; ingest latest synoptic HRRR run + fuel day.
3. Start API on localhost, run the load-test matrix from the handoff.
4. Wire proxy, smoke-test the console from a phone (ATMU-grade bandwidth).
5. Cron: HRRR ingest per cycle, fuel daily (after revalidation fix),
   prewarm CA + Wide West on new-run completion.
6. Then: pipeline daemon (staging/atomic manifests/priorities) as the
   first post-launch build.

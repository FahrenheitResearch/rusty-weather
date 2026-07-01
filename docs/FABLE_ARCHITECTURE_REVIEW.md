# Fable Architecture Review: rusty-fire-weather for CAFire.org

Date: 2026-07-01. Reviewed at `7dfb664`; changes from this review land in
`0c86715`. Produced by a 23-agent deep review (8 subsystem readers, ranking,
14 adversarially verified claims) plus direct reading of the hot paths and
fresh proof renders. Every claim below survived a verification pass against
the code or is marked as corrected.

## 1. Architecture map

Data plane (solid at the file level):

```text
HRRR GRIB (nomads)
  -> rw_batch (fetch -> decode -> ingest, 3-thread pipeline, memory gating)
  -> .rws hour files (64B header + JSON meta + chunk index + zstd tiles,
     lossless f32 2D; temp+fsync+rename writes; advisory per-run-dir lock;
     grid.rwg + run.json per run)                    crates/rw-store
  -> derived fire grids (vpd_2m, hdw, fire_weather_composite) computed ONCE
     at ingest and persisted as variables             derived/store.rs:185-224
gridMET daily NetCDF -> rw_fuel_fetch (annual-file cache, regrid) ->
  fuel variables appended into the SAME hour files (full-file rewrite)
LANDFIRE-style layers -> rw_fuel_import (manual)
```

Render plane:

```text
rw_render (one process per invocation)
  -> StoreFieldSource::open (hour + grid, selector maps)  store_render.rs:59
  -> partition_products into 4 lanes                      render_all.rs:130
  -> per lane: full-CONUS field reads (~3.6 ms each), projected-map build
     (Natural Earth + counties), rasterize, contours, labels, chrome,
     colorbar, PNG encode; windowed lane re-folds all stored hours
  -> WebP publish decodes the just-written PNG and re-encodes lossless
                                                          rw_render.rs:336-383
```

API plane (prototype-shaped, deliberately dependency-free):

```text
rw_fire_api: raw TcpListener, thread per connection + thread per job,
  Condvar semaphore (--max-render-jobs), in-memory HashMap request cache
  keyed on the full request tuple, shell-out to rw_render with RUSTWX_* env,
  /api/render -> 202 + poll /api/jobs/{id} -> /outputs/{job}/{file}
```

What does NOT exist yet (the mission gap): source watchers, staging paths,
run-completeness markers, atomic public manifests, "latest complete" vs
"latest detected", priority queues, idempotent/resumable jobs, freshness/lag
telemetry. `run.json` records hours written but nothing marks a run publishable.

## 2. Biggest efficiency bottlenecks and architectural risks (verified)

1. **[FIXED 0c86715] Hung render child = dead service.** `command.output()`
   had no timeout; a hung child pinned one of 2 permits forever
   (rw_fire_api.rs:519, permit held at :419). Now children stream to per-job
   logs and are killed at `--render-timeout-secs` (default 300).
2. **gridMET fuel cache deterministically hard-fails later dates.** Any
   existing non-empty annual NetCDF is a permanent cache hit
   (rw_fuel_fetch.rs:494-498, no revalidation, no --force); requested fuel
   dates map to day-of-year indexes (:914-928), so once the cached file's
   time dimension is exceeded every daily refresh errors "time index out of
   range" (:630-635). The daily fuel pipeline breaks mid-season by design.
3. **No operational refresh layer.** Confirmed absent (see map). rw_batch
   aborts on first error with no idempotent skip (rw_batch.rs:631-687);
   rw_cafire is a fail-fast one-shot (rw_cafire.rs:296-299).
4. **Fuel import races live readers.** Fuel augment fully decodes and
   re-encodes every variable of each hour and rewrites in place
   (fuel_import.rs:109-134); the hour snapshot is read BEFORE the lock
   (lost-update window), and atomic replace is delete-then-rename
   (atomic.rs:74-77) — a reader that opens between delete and rename sees a
   missing hour, and a Windows mmap reader blocks the rename outright.
5. **Cold shell-out fixed costs per unique render.** Per job: process spawn,
   TTF parse, county shapefile + Natural Earth OnceLock caches rebuilt
   (features.rs:138-151, 255-304), FOUR projected-map builds per domain (one
   per lane), topo hillshade recomputed per product, orography re-read up to
   3x, and the PNG->disk->decode->lossless-WebP round trip per product.
6. **Windowed products re-fold the run per render.** `10m_wind_run_max` at
   F048 costs 48 HourReader opens + 48 full-plane reads per render, per
   domain, with `use_cache: false` everywhere and nothing persisted
   (windowed_store.rs:118-254, 473-486).
7. **`--threads` does not control render workers.** It only sizes the global
   rayon pool (throttle.rs:126-139); the direct lane spawns
   `available_parallelism()/2` scoped threads (direct.rs:551-630, env
   `RUSTWX_RENDER_THREADS` only) and the derived lane builds its own
   cores/2 rayon pool — so the API's oversubscription guard is ineffective
   exactly under burst.
8. **API backpressure gaps (partially fixed).** Still open: unbounded thread
   per connection/job, cache check-then-insert race that defeats coalescing
   during identical-request bursts, jobs map + cache + output dir grow
   forever, cache hits still cost poll round-trips, `Connection: close`
   everywhere. Fixed: unbounded body buffering (2 MB cap), hung children.
9. **Silent partial success.** Non-strict presets exit 0 with fewer files
   (rw_render.rs:674-686), the API fails jobs only on non-zero exit, and a
   zero-file "success" is cached as permanently good.
10. **Process-env render configuration.** ~10 RUSTWX_* vars are read at
    render time deep in the render crate. Safe with child processes, but it
    blocks any warm-renderer refactor and must become explicit request state
    first.

## 3. Biggest plot-quality gaps (verified + inspected)

1. **Tiny-town label collisions at density 4.** Decluttering is
   geographic-km-only at selection (places.rs:808-831); draw time has no
   pixel-space overlap rejection (render.rs:1331-1461) and edge clamping
   piles labels up (:1440-1443). Confirmed in fresh renders: Reno/Sparks
   overprint, Nevada City/Grass Valley stack, South Lake Tahoe cluster. A
   working occupied-rect placer already exists for contour labels
   (render.rs:2550-2594) — reuse it for places.
2. **Colorbars never show units.** No units on the bar, title, or subtitle
   anywhere. Publication blocker.
3. **Smoke products are illegible over topo.** smoke_column renders in a
   narrow tan band of a 20-320 scale over hillshade texture — reads as a
   terrain map (confirmed in baseline and perimeter proofs). Smoke also
   currently rides a temperature-style palette. Needs a dedicated smoke
   ramp + stronger fill-over-hillshade compositing.
4. **Chrome clipping bug: border polylines strike through header text** and
   overshoot the map frame (visible in fresh proofs, e.g. the CA/NV border
   through "source: nomads"). Global, style-independent.
5. **Subtitle truncation.** Windowed subtitles ellipsize to
   "Init 06/29 03Z | F001-F003 | Val..." (render.rs:768-793); the
   operational subtitle row can also overlap center/side texts.
6. **County linework contrast** is near-invisible over saturated fills.
7. **Ocean/no-data treatment**: jagged raw-grid staircase coasts on fuel
   products; saturated wind fill over open ocean; windowed wind maxima have
   no barbs/vectors. HDW-over-topo rendered near-blank in one proof —
   check masking, not just palette.
8. Minor: colorbar under/over extend indicators missing; tick float
   formatting only supports one decimal; barbs at domain edge dropped
   rather than clipped.

Corrected reader claim: California-statewide and Wide West proofs DO exist
(`outputs/proof_cafire_expanded`, 17+17 PNGs + contact sheets) — but all
proofs are one frozen init/hour (2026-06-29 03Z F003).

## 4. Implementation plan, ranked by production impact

**P0 — landed this session (0c86715):** perimeter->bounds + overlay through
all lanes + cache keys; render-child deadline kill; request-body cap;
west/east validation.

**P1 — operational pipeline (the mission gap; new `rw_pipeline` daemon):**
1. Fix gridMET revalidation (mtime/size/HTTP revalidation or day-window
   epochs + `--force`) — the fuel lane is time-bombed until then.
2. Staged ingest: write runs under `store/.staging/<run>/`, validate, then
   atomically publish a `latest_complete.json` manifest per model (pointer
   swap, never serve half-built runs). Hour-level progressive publish:
   manifest lists complete hours so early hours ship before F48.
3. Source watchers on cadence (HRRR cycles/hours, HRRR-Smoke separately so
   missing smoke never blocks core weather, daily gridMET, hourly RTMA);
   idempotent job keys (source/run/hour/product/domain), per-run-hour locks
   (`.rw-lock` exists — extend), resumable backfill (skip-if-present).
4. Stop rewriting live hours for fuel: write fuel grids to a sidecar
   `fXXX.fuel.rws` (reader merges variable maps) or copy-on-write + manifest
   pointer swap. Kills the reader race and the 49x full-file re-encode.
5. Prewarm CA + Wide West + active incidents per new complete hour into the
   API cache dir; freshness/lag/queue-depth metrics on /api/health.

**P2 — API productionization (keep the process model, fix the semantics):**
bounded accept/job pools with a real FIFO+priority queue (ingest/prewarm vs
urgent perimeter vs ad hoc), 429/503 + Retry-After on queue-full, fix the
coalescing race (single lock, entry API), TTL+LRU eviction for jobs map,
cache map, and job dirs pinned to run retention, cache hits return 200 with
files immediately, keep-alive, structured request logs. Only consider an
axum/hyper swap after these semantics exist — the hand-rolled server is not
the bottleneck yet.

**P3 — renderer efficiency (no quality loss):**
1. Encode WebP directly from the in-memory RGBA (skip PNG write+decode).
2. Build the projected map ONCE per (domain, size) per invocation and share
   across lanes; fetch orography once; cache topo hillshade per domain.
3. Persist windowed grids into the store per (run, anchor-hour) at ingest/
   prewarm time so user renders never re-fold 48 hours.
4. Make `--threads` actually size the direct/derived worker pools.
5. Then measure a persistent warm-renderer worker (env vars must become
   request state first — see risk 10).

**P4 — plot quality:** pixel-space place-label declutter reusing the
contour-label rect placer (keep the dense option, drop only true overlaps);
units on every colorbar; dedicated smoke palette + compositing over topo;
clip chrome/border overshoot; fix subtitle ellipsis (shrink-then-wrap before
truncate); county line contrast pass; ocean masking for fuel/wind products;
per-product proof-render regression pages across 2+ runs and hours.

**P5 — Hetzner (blocked on explicit approval):** deploy plan as in the
handoff; rerun load tests there before capacity claims.

## 5. What was implemented and proven in this session

- `crates/rusty-weather/src/perimeter.rs` — perimeter->bounds (local tangent
  km projection, per-side padding, one-sided bearing extension, aspect fit,
  min/max span, validation, FNV cache hash). 13 tests, TDD.
- `crates/rustwx-render/src/geo_overlay.rs` + hook in
  `build_projected_map_with_options` — lon/lat rings (JSON file via
  `RUSTWX_OVERLAY_POLYLINE_FILE`) densified, projected with the map
  projector, and appended to basemap linework: every lane draws them above
  raster / below labels, immune to the projected_lines clobber hazard.
  6 tests.
- `rw_fire_api`: `perimeter`/`padding_km`/`extend`/`overlay_perimeter`
  request fields, server-side bounds, cache-key coverage, per-job overlay
  spec, body cap, child deadline kill, reversed-bounds rejection. 35 tests.
- Proof renders (job-1782944723896-1): 6 products, 1544 ms, synthetic Park
  Fire perimeter, 50 km pad + 80 km @ 65° extension — bounds verified
  numerically, orange halo ring confirmed on fuel-composite and smoke lanes.
  Identical resubmit returned `cache: hit`.
- Load test (preview-core, 40 req, c10, 2 slots): 0 failures, renderer
  p50 871 ms / p95 1445 ms (baseline 1085/1353 — no regression). NOTE: the
  handoff's 6.77 jobs/s baseline exceeds the 2-slot cold-render ceiling
  (~2/1.085 s) and must have included cache hits or more slots; renderer
  wall time is the comparable metric. The harness's `domain_slug` embeds the
  run label, so every harness request is deliberately cache-unique.

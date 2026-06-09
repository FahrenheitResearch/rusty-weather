# rusty-weather — design

**Date:** 2026-06-09
**Status:** approved direction, pre-implementation
**Source repo:** rustwx, branch `review/grib-wxa-fast-plots-20260605` (worktree at `C:\Users\drew\rustwx-fastplots-wt`)

## Why this exists

rustwx grew into ~333K lines across 16 workspace crates + 10 vendored crates, covering far more than its owner needs (radar, satellite, WRF, lightning, mesoanalysis, a private cloud-seeding engine, an agent platform, ~80 proof binaries). Work diverged across branches; deploy-by-cloning-branches put stale builds on worker nodes; and multi-model production runs are ~10x slower than isolated benchmarks.

Investigation on the fast-plots branch established three facts that shape this design:

1. **The 10x multi-model slowdown is architectural.** Every render job spawns its own `num_cpus/2` thread pool (`direct.rs:448`) and all render threads contend on five shared `Arc<Mutex<HashMap>>` layer caches (`direct.rs:343-346`). Concurrent models oversubscribe the CPU and thrash each other's caches. Each binary assumes it owns the machine; production runs several at once.
2. **The 2D `.wxa` store never delivered its core promise.** It is chunked 256×256 with an index, but the reader always decompresses the entire grid and crops in memory (`wxstore_wxa.rs:208`). "Process once, plot any region free" was the goal; it was not implemented. The 3D volume store is better (chunked, affine-i16 quantization, cheap point soundings) but uncompressed and bloated for curvilinear grids. Both have hardcoded version strings with no compatibility path.
3. **The keep-set is clean.** ~185K lines (9 workspace crates + 7 vendored) form the proven fast path: byte-range GRIB ingest, custom projection math (no PROJ), dialed-in plot rendering, sharprs soundings. The six target models have zero cross-model coupling, so dropping the other models is surgical.

rusty-weather is a curated extraction of that keep-set into a new repo, plus two new pieces: a unified storage format and a local web server with a global scheduler.

## Product goal

Anyone installs one Rust binary, runs `rusty-weather serve`, opens a local webpage, and:

- picks a model (HRRR, GFS, RRFS-A, REFS, NBM, RAP), a cycle, and forecast hours;
- the app fetches the GRIB, builds the store, and renders map products;
- regional plots are cheap because only intersecting tiles are decoded;
- clicking anywhere on a map produces a sounding in well under a second.

## Scope

**v1 in:** the six models above; CONUS-and-native-domain map plots using the existing (ported, unmodified) render styles; point soundings via sharprs; REFS limited to mean/spread products; localhost web UI; Linux nodes + Windows dev as build targets.

**v1 out (explicitly):** radar, satellite, lightning, mesoanalysis, WRF/GDEX, cross-sections, wxmod/cloud-seeding, agent platform, Python bindings, any other model (HRRR-AK, GEFS, AIGFS, ECMWF, AIFS, NAM, HIRESW, SREF, RTMA, URMA, NBM-beyond-core), authentication, multi-machine orchestration. Cross-sections are the most likely v1.5 addition; the crate ports cleanly when wanted.

## Architecture

One binary, three subcommands:

- `rusty-weather serve` — the primary mode: HTTP server + scheduler + pipeline.
- `rusty-weather fetch --model hrrr --cycle ... --hours ...` — headless ingest for scripting.
- `rusty-weather render ...` — headless plot generation from an existing store.

### Concurrency model (the 10x fix)

- **One global rayon pool**, sized once at startup (default: physical cores, configurable). All CPU work — GRIB decode, store encode, tile decode, PNG render — runs as fine-grained tasks on this pool. No other component may create a thread pool.
- **One tokio runtime** (axum's) for HTTP serving and network fetches. Network-bound work never occupies compute threads; downloads hand off decoded bytes to the rayon side through bounded channels.
- **Job model:** a user request ("HRRR 12z f00–f18") becomes a job; jobs decompose into per-hour fetch tasks → per-field encode tasks → per-product render tasks. Fairness across concurrent models emerges from fine task granularity rather than per-model pools.
- **Caches:** projected-map geometry, basemap rasters, and font/palette data live in global compute-once caches keyed by (grid, region, size, style) — `OnceCell`-style entries or a sharded map, not a single `Mutex<HashMap>`. One process means one cache; concurrent models share rather than evict.

### Pipeline

```
probe (idx)  →  fetch (byte-range GRIB)  →  decode (grib-core)  →
encode store (.rws per hour)  →  render products (PNG, on demand or eager)  →
serve (HTTP + frontend)
```

Probe/fetch logic, field selectors, and decode port from `rustwx-io`/`rustwx-models`. The store and server are new.

## Store v2 (`rw-store`)

Replaces both the 2D `.wxa` and the 3D volume store with one format.

### Layout

```
store-root/
  hrrr/
    20260609_12z/
      run.json          # run manifest: model, cycle, grid spec, variables,
                        # levels, hours present, format version, build hash,
                        # per-stage timings
      f000.rws
      f006.rws
      ...
```

A **run is a directory; each forecast hour is one self-contained file.** Appending an hour = writing a new file (atomic temp+rename), so there is no append-rewrite cost, partial runs are natural, and a corrupt hour cannot damage its run.

### Hour-file format (`.rws`)

Header: magic `RWSTORE1`, format version (u32), JSON metadata block (model, run, hour, grid spec, variable table with units, level table, chunk geometry, codec table), then a fixed-record binary chunk index, then payload.

Two chunk classes in the same file:

- **2D fields** (surface + single-level + precomputed derived): 256×256 spatial tiles, zstd-compressed f32. The chunk index is sorted and binary-searchable by (variable, tile_y, tile_x). **Windowed reads are a hard requirement:** a regional plot request computes the intersecting tile set and decodes only those.
- **3D fields** (pressure-level volumes for soundings): column-shaped chunks — small spatial footprint (e.g. 16×16) × all levels — affine-i16 quantized then zstd. A point sounding mmaps the file, binary-searches the index, and decodes the 1–4 chunks covering the point's neighborhood for bilinear interpolation across all levels at once.

Ported ideas from the old stores: empty/constant chunk flags (zero payload), per-chunk min/max/valid-count stats, affine quantization with missing-value sentinel. New requirements the old stores lacked: a codec table instead of one hardcoded codec string; readers accept format version N and N−1 with a defined error (not silence) for older; exact grid-identity checks (not "same type ≈ compatible") before any file is extended or merged.

Quantization is applied only to 3D volumes (plot-grade tolerance); 2D plot fields stay f32+zstd. If a future variable needs lossless 3D, the codec table allows per-variable codec choice without a format break.

### Performance targets

| Operation | Target | Baseline (rustwx, isolated) |
|---|---|---|
| HRRR GRIB → store, one hour | ≤ 20 s | ~20 s |
| Store → 80 PNGs, one hour | ≤ 5 s | ~5 s |
| RRFS-A GRIB → store, one hour | ≤ 60 s | ~60 s |
| Point sounding from store | < 100 ms | n/a (new path) |
| Regional plot vs full-domain plot | decode cost ∝ region area | full-grid decode today |
| 3 models concurrently | ≤ 1.5× each model's isolated time | ~10× today |

The last row is the acceptance test for the architecture; it is measured, not assumed.

## Porting inventory

Ported crates keep their existing names — renaming ~185K lines of imports buys nothing and destroys diffability against rustwx. New crates get new names.

**Workspace crates (from rustwx, pruned at the boundaries):**

| Crate | Action |
|---|---|
| `rustwx-core` | port as-is |
| `rustwx-models` | port whole, then prune in stages: a `supported_models()` catalog surface gates user-facing enumeration to the 6 targets immediately; deep removal of other models' registry/recipe code happens after the daemon exists and dead code is provable (ModelId match arms thread through rustwx-products, so premature enum surgery multiplies extraction risk) |
| `rustwx-io` | port; byte-range fetch, idx parsing, selector extraction, cache layer |
| `rustwx-render` | port as-is (plots are dialed in; custom projections, no PROJ) |
| `rustwx-contour` | port as-is |
| `rustwx-calc` | port as-is (CAPE/ECAPE/severe diagnostics) |
| `rustwx-sounding` | port as-is |
| `rustwx-regrid` | port as-is |
| `rustwx-products` | port **pruned**: keep direct/derived/gridded product planning and recipes, plus their load-bearing infrastructure (`places` city-label overlays are part of the plot look; `publication` provides atomic writes/run manifests); drop satellite, radar, lightning, mesoanalysis, intelligence, agent/orchestrator, custom POI, publication provenance, native datasets, wxstore_*/volume_store (replaced by rw-store) — roughly 40% of its 87K lines stays behind |
| `rustwx-cross-section` | not ported in v1; earmarked for v1.5 |

**New crates:** `rw-store` (format above), `rw-server` (axum app, scheduler, job model), thin `rusty-weather` bin crate.

**Vendored (copied byte-for-byte):** `sharprs`, `metrust`, `ecape-rs`, `grib-core`, `wx-core`, `wx-math`, `wx-field`. Left behind: `wx-radar`, `netcrust` (+ embedded hdf5-reader; WRF-only).

**Assets:** `assets/basemap/` (Natural Earth shapefiles, counties) copied as-is.

**Left behind entirely:** `rustwx-cli`'s ~80 proof binaries, `rustwx-python`, `rustwx-model-maps-launcher`, `rustwx-radar`, `rustwx-wrf`, `rustwx-prep` (WRF lake masking — moot with WRF out of scope), the Python "Studio" UI. rustwx itself stays untouched as the archive and the home of wxmod/agent work; hermes-weather-agent (pinned to an old rustwx) is unaffected.

**External deps:** existing set (image, rayon, serde, zstd, memmap2, shapefile, rusttype, ureq/rustls, …) plus tokio + axum for the server. ureq remains for model fetches in v1 (it works and is proven); moving fetches to reqwest/tokio is an optimization, not a requirement.

## Web server and UI (`rw-server`)

axum, localhost by default (`--bind` to override on trusted LANs — the nodes).

API (JSON unless noted):

- `GET /api/models` — models, cycles, availability (idx probes, cached)
- `POST /api/runs` — request (model, cycle, hours); returns job id
- `GET /api/jobs/{id}/events` — SSE progress (per-hour fetch/encode/render stages)
- `GET /api/runs/{model}/{cycle}` — hours present, products available, timings
- `GET /plot/{model}/{cycle}/{hour}/{product}?region=&width=&style=` — PNG, rendered on demand, disk-cached, build-hash stamped
- `GET /api/sounding?model=&cycle=&hour=&lat=&lon=` — profile JSON (sharprs-derived parameters included)
- `GET /sounding.png?...` — rendered sounding image

Frontend: static vanilla HTML/JS served by the same binary (embedded via `include_dir` so the single-binary install story holds). Model picker → cycle/hour scrubber → product grid of plots → click-on-map → sounding panel. No framework, no build step.

## Observability and reproducibility

- Build hash (git SHA + dirty flag) and store format version are compiled in, stamped into every `run.json`, every PNG's tEXt metadata, and the UI footer. A stale deploy is visible at a glance — the rustwx deploy-lottery cannot recur silently.
- Every pipeline stage records wall time into `run.json` (fetch, decode, encode, render, per hour). "Why is RRFS slow today" becomes a lookup.
- `rusty-weather doctor` prints build hash, pool size, store root, and per-model probe status.

## Error handling

- Layered typed errors (`thiserror`), as in rustwx today.
- Fetch: retry with source fallback (AWS ↔ NOMADS where both exist), ported from `rustwx-io`; an unavailable hour fails that hour's tasks only, never the job.
- Store writes are atomic (temp file + rename); a crash mid-hour leaves no partial `.rws`.
- Server returns structured errors; a bad request or a failed render never takes down the daemon.

## Testing

- Unit tests travel with their ported crates; the `rustwx-render/verify` lane ports too.
- `rw-store`: round-trip property tests (write→read equality within quantization bounds), windowed-read correctness (region read == full read crop, exact), version-mismatch behavior, corrupt-index rejection.
- End-to-end smoke: a small committed GRIB fixture → store → one PNG → one sounding, run in CI.
- Scheduler: a stress test that submits 3 synthetic model jobs and asserts the ≤1.5× concurrency target on fixture data.

## Validation plan

Stand rusty-weather up on node3/node4 (192.168.68.56/.57) beside existing rustwx binaries. For the same live cycle: compare per-stage timings (against the table above) and visually compare PNGs for a fixed product set (styles are ported unchanged, so diffs should be nil-to-trivial). Only after that comparison does rusty-weather become the thing the nodes run.

## Open questions deferred to the implementation plan

- Exact 3D chunk footprint (16×16 vs 32×32 columns) and 2D tile size per model grid — tune with benchmarks, not guessed here.
- Which derived products are precomputed into the store at ingest vs computed at render time (start with rustwx's current derived list for the 6 models).
- REFS mean/spread: computed at ingest (stored as 2D fields) — confirm member-fetch strategy against current rustwx REFS lane.
- Eager-render policy (render all products on ingest vs on first request) — default on-demand with warm-cache option; revisit after timing data.

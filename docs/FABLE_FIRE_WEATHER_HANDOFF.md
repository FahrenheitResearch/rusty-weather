# Claude Fable Handoff: Rusty Fire Weather / CAFire

Last updated: 2026-07-01

Audience: a new high-context model/agent joining this repo. Read this before
spending tokens rediscovering the project.

If you need the shortest pasteable version first, use
`docs/FABLE_BOOT_PROMPT.md`.

## Executive Summary

This repo is the new Rust-first home for the CAFire weather plotting work. The
canonical forecast data format is `.rws` / `rw-store`, not WxStore. RustWX is
valuable as the rendering/style lineage, not as the operational monolith to keep
alive.

The desired product is `rusty-fire-weather`: a production-capable CAFire plotting
service where CAFire.org users can choose California, Wide West, or arbitrary
drawn domains, eventually including domains derived from fire perimeters, and
receive polished RustWX-quality static plots from `.rws` model and fuel data.

Current local demo:

- API: `http://127.0.0.1:8788/`
- Running process: `C:\Users\drew\rusty-fire-weather\target\release\rw_fire_api.exe`
- Current committed head at handoff time: `e6930fc feat: add topo terrain and dense town controls`
- Working tree note: generated `outputs/` is untracked and should not be committed.

Do not "solve" this by going back to WxStore, by rebuilding the RustWX Python
wheel path, or by lowering plot quality. The whole point is `.rws` storage plus
RustWX-quality maps.

## Fable Context

The user expects Claude Fable access on 2026-07-01. I verified current reporting:
Anthropic says the U.S. export controls on Fable 5 and Mythos 5 were lifted on
2026-06-30 and Fable 5 availability resumes 2026-07-01. Useful references:

- Anthropic: https://www.anthropic.com/news/redeploying-fable-5
- Business Insider: https://www.businessinsider.com/anthropic-restores-fable-5-mythos-access-trump-white-house-talks-2026-6
- Al Jazeera: https://www.aljazeera.com/economy/2026/7/1/us-lifts-restrictions-on-powerful-ai-models-fable-mythos-anthropic-says

Treat "AGI" as the user's expectation, not as an engineering assumption. The
engineering goal remains: make the repo shippable, testable, and easy for a very
strong model to continue.

## Non-Negotiables

- `.rws` / `rw-store` is the gridded backend.
- Keep the RustWX look: projected maps, good basemap linework, topo option,
  counties, dense small-town labels, readable titles/subtitles, colorbars,
  wind overlays, fire-weather palettes, clean ocean/no-data treatment.
- Fuel products must use real ingested fuel grids. Do not fake fuel layers with
  weather-only substitutes.
- CAFire products should be Rust-native as much as possible.
- The demo API can be local for now. Do not touch Hetzner until the user
  explicitly approves server work.
- Performance must be measured with load tests, not guessed.
- Do not commit generated render outputs.

## Current Architecture

Data flow:

```text
HRRR / model source
  -> rw_batch / ingest
  -> .rws hour stores
  -> rw_render
  -> PNG or WebP maps
  -> rw_fire_api draw-a-box service
```

Fuel flow:

```text
gridMET / LANDFIRE / public fuel layers
  -> rw_fuel_fetch or rw_fuel_import
  -> same-grid fuel variables inside .rws
  -> rw_render fuel products
  -> rw_fire_api products such as cafire-with-fuels
```

Important binaries:

- `rw_batch`: ingest model hours into `.rws`.
- `rw_fuel_fetch`: download/cache public gridMET fuel datasets and import them.
- `rw_fuel_import`: import/regrid manual fuel layers, including LANDFIRE-style layers.
- `rw_cafire`: one-command CAFire ingest -> fuel fetch/import -> render.
- `rw_render`: render one stored hour/domain/product set.
- `rw_fire_api`: local/web draw-a-box API around `rw_render`.

Primary code surfaces:

- `crates/rusty-weather/src/render_all.rs`: product partitioning and render orchestration.
- `crates/rusty-weather/src/fuel_products.rs`: CAFire fuel products.
- `crates/rusty-weather/src/bin/rw_fire_api.rs`: prototype HTTP API and demo page.
- `crates/rusty-weather/src/bin/rw_render.rs`: CLI render entrypoint.
- `crates/rustwx-products/src/topo.rs`: topo terrain helper from stored orography.
- `crates/rustwx-products/src/places.rs`: city/town label overlays and density tiers.
- `crates/rustwx-render/src`: RustWX-style map renderer.
- `docs/CAFire_Fuel_Production.md`: concise production command notes.

## Recent Commit History

These commits are the CAFire-specific work that matters:

```text
e6930fc feat: add topo terrain and dense town controls
34b7c0c feat: add topo basemap option
4146cc9 feat: add dense place labels for fire api boxes
52d26eb feat: add fire api basemap controls
50f53d2 feat: expose fire api render throttle knobs
65cfa18 perf: cache repeated fire api renders
f58707f perf: parallelize cafire fuel rendering
932c809 feat: add gridmet fuel pipeline for cafire renders
1e0e64c feat: add rusty fire weather cafire rendering prototype
```

## Product Catalog

Core CAFire weather products:

- `vpd_2m`
- `hdw`
- `fire_weather_composite`
- `10m_wind_1h_max`
- `10m_wind_run_max`

Direct stored HRRR products:

- `2m_temperature_10m_winds`
- `2m_relative_humidity_10m_winds`
- `2m_dewpoint_10m_winds`
- `10m_wind_gusts`
- `visibility`
- `smoke_pm25_native`
- `smoke_column`

Windowed products:

- `qpf_1h`
- `10m_wind_1h_max`
- `10m_wind_run_max`
- `2m_temp_0_24h_range`
- `2m_temp_24_48h_range`
- `2m_temp_0_48h_range`

Fuel/native fuel products:

- `kbdi`
- `erc`
- `burning_index`
- `dead_fuel_moisture_1h`
- `dead_fuel_moisture_10h`
- `dead_fuel_moisture_100h`
- `dead_fuel_moisture_1000h`
- `daily_precip_fuel_context`
- `landfire_fuel_model`
- `landfire_fuel_loading`

Fuel/weather composite products:

- `fuel_receptiveness`
- `fire_potential_composite`
- `hdw_fuel_receptive`
- `vpd_fuel_receptive`
- `erc_hdw_composite`

Product presets:

- `cafire-core`: core derived/hour products plus key wind-window products.
- `cafire-hour`: direct HRRR products plus core derived products.
- `cafire-windowed`: windowed products only.
- `cafire-all`: direct + core + standard windowed products.
- `cafire-expanded`: direct + core + full supported windowed catalog.
- `cafire-with-fuels`: direct + core + standard windowed + all fuel products.
- `cafire-expanded-with-fuels`: expanded catalog plus all fuel products.
- `cafire-hour-with-fuels`: direct + core + all fuel products.
- `cafire-fuels`: fuel products only.
- `cafire-fuel-layers`: native fuel layers only.
- `cafire-fuel-composites`: fuel/weather composites only.
- `all`, `direct`, `derived`, `heavy`, `windowed`, `fuel`, `fuels`: broader internal presets.

## Known Domains

Important region presets from `crates/rusty-weather/src/region.rs`:

- `cafire_california`: `[-126.0, -113.8, 31.9, 42.5]`
- `cafire_wide_west`: `[-125.7, -103.8, 31.9, 46.5]`
- `california`: `[-124.9, -113.8, 31.9, 42.5]`
- `california_square`: `[-124.9, -113.7, 31.8, 42.7]`
- `reno_square`: `[-123.1, -116.1, 36.1, 43.1]`
- `sierra_nevada`: `[-122.4, -117.7, 35.0, 40.7]`
- `fire_weather_west`: `[-126.5, -101.0, 28.0, 50.5]`

The API also accepts arbitrary drawn bounds, which is the production direction
for CAFire.org.

## Current API Contract

`rw_fire_api` accepts POST `/api/render` with a JSON body like:

```json
{
  "model": "hrrr",
  "run": "20260629_03z",
  "hour": 3,
  "products": "cafire-with-fuels",
  "output_format": "webp",
  "plot_style": "operational-fast",
  "basemap_style": "topo",
  "county_linework": true,
  "place_label_density": 4,
  "place_label_size": 2,
  "domain_slug": "drawn_box",
  "bounds": [-123.21, -119.67, 37.13, 41.14],
  "output_width": 1400,
  "output_height": 900
}
```

Current controls:

- `basemap_style`: `filled`, `white`, or `topo`.
- `county_linework`: boolean.
- `place_label_density`: `0..4`, where `4` is max local/tiny towns.
- `place_label_size`: currently `1..3`.
- `output_format`: PNG or WebP, depending on current CLI/API normalization.
- `output_width` / `output_height`: clamped for sane render sizes.

The API shells out to `rw_render`, bounds simultaneous child processes with
`--max-render-jobs`, and reports queue state through `/api/health`.

Render cache:

- Repeated identical requests are cached by request key.
- The key includes model/run/hour/products/output format/plot style/basemap
  style/counties/place labels/domain/bounds/output size.
- This is useful for repeated website requests and preview refreshes.

## Local Run Commands

Build:

```powershell
cargo build --release -p rusty-weather --bin rw_fire_api --bin rw_render --bin rw_cafire --bin rw_fuel_fetch --bin rw_fuel_import
```

Start local API:

```powershell
target\release\rw_fire_api.exe `
  --host 127.0.0.1 --port 8788 `
  --store-root C:\Users\drew\rusty-weather\store `
  --out-root C:\Users\drew\Documents\Codex\2026-06-28\so\outputs\rusty_fire_api_demo_jobs `
  --rw-render target\release\rw_render.exe `
  --max-render-jobs 2
```

One-command CAFire run:

```powershell
target\release\rw_cafire.exe `
  --date 20260629 --cycle 3 --hours 0-48 `
  --products cafire-with-fuels `
  --fuel-provider gridmet `
  --fuel-date 2026-06-29 `
  --fuel-cache-dir C:\rw\cache\fuel `
  --fuel-method bilinear `
  --store-root C:\rw\store `
  --out-dir C:\rw\cafire_out
```

Fetch/import fuel only:

```powershell
target\release\rw_fuel_fetch.exe `
  --store-root C:\rw\store `
  --model hrrr --run 20260629_03z --hours 0-48 `
  --date 2026-06-29 `
  --cache-dir C:\rw\cache\fuel `
  --kbdi-spinup-days 180 `
  --kbdi-annual-rain-in 20 `
  --method bilinear
```

Manual LANDFIRE import:

```powershell
target\release\rw_fuel_import.exe `
  --store-root C:\rw\store `
  --model hrrr --run 20260629_03z --hours 0-48 `
  --layer landfire_fuel_model=C:\fuel\landfire_model.nc:fuel_model `
  --layer landfire_fuel_loading=C:\fuel\landfire_loading.nc:fuel_loading `
  --lat-var lat --lon-var lon `
  --method nearest --overwrite
```

## Proof Renders

Known local proof outputs from the demo:

- Smoke topo/towns proof:
  `C:\Users\drew\Documents\Codex\2026-06-28\so\outputs\rusty_fire_api_demo_jobs\job-1782879181721-1\rustwx_hrrr_20260629_3z_f003_reno_sierra_topo_hillshade_towns_smoke_column.webp`
- Fuel/topo/towns proof:
  `C:\Users\drew\Documents\Codex\2026-06-28\so\outputs\rusty_fire_api_demo_jobs\job-1782879209619-2\rustwx_hrrr_20260629_3z_f003_reno_sierra_topo_fuel_towns_vpd_fuel_receptive.webp`

The smoke proof was `smoke_column`, `basemap_style=topo`,
`place_label_density=4`, `place_label_size=3`, bounds
`[-121.73, -116.61, 38.35, 41.53]`.

The fuel proof was `vpd_fuel_receptive`, `basemap_style=topo`,
`place_label_density=4`, `place_label_size=2`, same bounds.

## Performance Baseline

Important: these are local desktop/prototype numbers, not Hetzner numbers.
Rerun on Hetzner before making capacity claims.

Saved local load-test summaries:

- `load_core_c10`: 20 requests, concurrency 10, `preview-core`, 0 failures,
  throughput about 6.77 jobs/sec, client p50 1234 ms, client p95 1504 ms,
  renderer p50 1085 ms, renderer p95 1353 ms.
- `load_core_c20`: 40 requests, concurrency 20, `preview-core`, 0 failures,
  throughput about 8.75 jobs/sec, client p50 2034 ms, client p95 2649 ms,
  renderer p50 1811 ms, renderer p95 2438 ms.
- `load_full_png_core_c08`: 16 requests, concurrency 8, `full-png-core`,
  0 failures, throughput about 5.44 jobs/sec, client p50 1266 ms, client p95
  1664 ms, renderer p50 1060 ms, renderer p95 1419 ms.

The harness is `scripts/fire_api_load_test.py`. Example:

```powershell
python scripts\fire_api_load_test.py `
  --api http://127.0.0.1:8788 `
  --scenario preview-core `
  --requests 40 `
  --concurrency 10 `
  --out-dir C:\rw\load_tests
```

Scenarios:

- `preview-mixed`: randomized single/core requests.
- `preview-core`: WebP, smaller preview width, `cafire-core`.
- `preview-single`: randomized single product.
- `full-png-core`: full PNG core output.

The harness writes CSV samples and JSON summaries with latency, API wall time,
renderer wall time, throughput, bytes, and failures.

## Performance Knobs

`rw_fire_api`:

- `--max-render-jobs`: maximum simultaneous `rw_render` child processes.
- `--render-threads`: thread count forwarded to each renderer process.
- `--full-throttle-render`: forwards full throttle to renderer on dedicated nodes.

Renderer/env knobs currently in use:

- `RUSTWX_BASEMAP_STYLE`: `filled`, `white`, `topo`.
- `RUSTWX_COUNTY_LINEWORK`: toggles counties.
- `RUSTWX_PLACE_LABEL_SIZE_FACTOR`: scales town/city labels.
- `RUSTWX_PLACE_LABEL_ALPHA_FACTOR`: scales label alpha.
- `RUSTWX_STATIC_OUTPUT_WIDTH` / `RUSTWX_STATIC_OUTPUT_HEIGHT`: output size.

Principle:

- Do not lower plot quality as the first performance move.
- Prefer caching, queue control, prewarming latest runs, WebP for web previews,
  output sizing, and avoiding repeated identical child work.
- On a dedicated Hetzner render node, test `--full-throttle-render` and tuned
  `--render-threads`; on shared/dev boxes keep throttling conservative.

## Hetzner Production Target

Known from this repo: Hetzner is the performance/deployment target. Exact
server specs, IP, paths, secrets, and reverse-proxy details are not in this
repo. Ask the user or inspect the real server only after explicit permission.
Do not invent credentials or deployment facts.

Recommended Linux layout:

```text
/srv/rusty-fire-weather        checked-out repo
/srv/rw/store                  .rws model stores
/srv/rw/cache/fuel             fuel cache
/srv/rw/api_jobs               API render outputs
/srv/rw/logs                   service logs
```

Suggested service command shape:

```bash
/srv/rusty-fire-weather/target/release/rw_fire_api \
  --host 127.0.0.1 \
  --port 8788 \
  --store-root /srv/rw/store \
  --out-root /srv/rw/api_jobs \
  --rw-render /srv/rusty-fire-weather/target/release/rw_render \
  --max-render-jobs 2 \
  --render-threads 12
```

Front it with nginx or Caddy:

- TLS at the proxy.
- API path proxied to `127.0.0.1:8788`.
- Output files served either through the API or directly from a static alias.
- Keep the demo HTML/no-store behavior for the control page.
- Make immutable job outputs cacheable once the URL includes the job id.
- Add request-size limits for future GeoJSON perimeter upload.

Initial production targets to validate:

- One local-domain, one-product WebP: p95 under 2 seconds.
- Five-product `cafire-core` local-domain WebP: p95 under 6 seconds.
- Ten concurrent users with mixed preview loads: 0 failures and bounded queue.
- CPU remains saturated but not thrashing; memory does not grow unbounded.
- Repeated identical requests return from cache quickly.

Hetzner load-test plan:

1. Build release binaries on the server.
2. Preload one complete HRRR run into `.rws`.
3. Preload/import the matching fuel day.
4. Start API behind localhost only.
5. Run `preview-single`, `preview-core`, `preview-mixed`, and full-size tests.
6. Tune `--max-render-jobs`, `--render-threads`, and output dimensions.
7. Only then expose through CAFire.org routing.

## Fire Perimeter Plot Plan

This is the dream feature and should be the next major slice:

Users should be able to draw or select a fire perimeter, choose padding such as
25/50/100 km, optionally extend one side toward the expected spread direction,
and receive a polished local-domain plot that is framed perfectly around the
incident.

MVP behavior:

- Accept a polygon/perimeter from the web UI or API.
- Compute bounds around the perimeter with `padding_km`.
- Expand the shorter axis to match the requested output aspect ratio.
- Enforce minimum span for tiny fires and maximum span for accidental huge input.
- Render the existing products using the computed bounds.
- Overlay the perimeter outline on the rendered map.
- Include perimeter hash and padding/extension settings in the cache key.

Proposed API extension:

```json
{
  "model": "hrrr",
  "run": "20260629_03z",
  "hour": 3,
  "products": "cafire-with-fuels",
  "domain_kind": "perimeter",
  "domain_slug": "park_fire_50km",
  "perimeter": [[-121.7, 39.6], [-121.4, 39.7], [-121.3, 39.4], [-121.6, 39.3]],
  "padding_km": 50,
  "extend": {
    "direction_deg": 65,
    "distance_km": 80
  },
  "fit_policy": "natural",
  "overlay_perimeter": true,
  "basemap_style": "topo",
  "county_linework": true,
  "place_label_density": 4,
  "place_label_size": 2
}
```

Perimeter bounds algorithm:

1. Validate all lon/lat points are finite and within sane geographic ranges.
2. Close the ring if needed.
3. Use a local tangent/equirectangular projection centered on the perimeter
   centroid for kilometer math.
4. Convert all points to x/y km.
5. Compute min/max x/y.
6. Pad every side by `padding_km`.
7. If `extend` is set, project the extension vector and expand only the side(s)
   in that direction by `distance_km`.
8. Apply min span, max span, and aspect-ratio fit.
9. Convert final x/y bounds back to lon/lat.
10. Pass those bounds to the existing `.rws` render path.

Overlay implementation path:

- `rustwx-render` already supports projected line overlays in `MapRenderRequest`.
- Add a `PerimeterOverlay` or generic lon/lat polyline overlay to the product
  request layer, not just the API.
- Project perimeter points using the same `ProjectedDomain`/map context used
  for basemap linework.
- Stroke perimeter above raster and below/near labels with a high-contrast
  outline, for example dark halo plus orange/yellow line.
- Ensure overlays work for direct, derived, windowed, and fuel products.

Initial UI behavior:

- Keep existing draw-a-box.
- Add "draw perimeter" or "paste GeoJSON" later.
- Offer padding buttons: 25 km, 50 km, 100 km.
- Offer an optional spread-direction arrow/angle and extension distance.
- Show the computed plot bounds before render.
- Preserve topo/counties/town controls.

First useful implementation sequence:

1. Add perimeter-to-bounds calculation and tests in `rw_fire_api` or a small
   shared module.
2. Allow POST `/api/render` to accept either `bounds` or `perimeter`.
3. Add cache-key fields for perimeter hash, padding, extension, and overlay flag.
4. Thread a perimeter overlay through render requests.
5. Add a UI test/sample perimeter and render one proof plot.
6. Add load tests for perimeter domains. They should perform like drawn boxes
   because the expensive work is still the same crop/render path.

Potential perimeter data sources later:

- User-drawn polygon or uploaded GeoJSON first.
- CAL FIRE incident/perimeter feeds if available and permitted.
- WFIGS / NIFC / IRWIN-style public perimeter sources after source validation.

Do not block the first version on live perimeter ingestion. User-provided
GeoJSON gets the domain/render UX working without external dependency risk.

## Known Gaps / Risks

- The API is a small prototype `TcpListener`, not a hardened web framework.
  This is acceptable locally, but production needs careful proxying, limits,
  logging, and probably a proper async/service layer later.
- Perimeter overlay is not threaded through the product paths yet.
- Dense tiny-town labels can still need smarter collision handling at max
  density. Do not remove the option; improve decluttering.
- LANDFIRE downloader is not complete in the same way gridMET fetch is. Manual
  `rw_fuel_import` exists.
- Fuel-aware products correctly skip when required fuel grids are absent. That
  is good behavior, but production should surface the missing grids cleanly in
  the web UI.
- Exact Hetzner specs are unknown from this repo.
- RTMA anomaly/percentile work is conceptually planned but not fully native in
  this CAFire branch yet.

## HRRR vs RTMA / Anomaly Direction

Remember the intended distinction:

- HRRR products are forecasts.
- RTMA products are analyses/observations.
- Anomaly/percentile products compare current forecast or analysis values
  against a historical/seasonal baseline, likely RTMA-derived.

Planned products from earlier discussions:

- HRRR VPD anomaly/percentile against RTMA seasonal baseline.
- HRRR wind/gust anomaly/percentile against RTMA baseline.
- HRRR surface HDW anomaly/percentile against RTMA-derived surface HDW baseline.
- RTMA observed VPD anomaly/percentile.
- RTMA observed wind/gust anomaly/percentile.
- RTMA min RH / overnight RH recovery.
- RTMA surface HDW anomaly/percentile.
- RTMA pressure fall / dewpoint drop.
- RTMA low visibility / ceiling / cloud duration.
- RTMA threshold-duration products for RH, VPD, wind, gust, and dry+windy hours.

This is not the same as the fuel products. Fuel products should come from
gridMET/LANDFIRE/NFDRS-like fuel data. RTMA anomaly products are weather context.

## Test Commands

Useful checks that have been run successfully in this worktree:

```powershell
cargo check -p rustwx-render -p rustwx-products -p rusty-weather --bin rw_render --bin rw_fire_api
cargo test -p rustwx-products topo::
cargo test -p rustwx-products places::
cargo test -p rusty-weather --bin rw_fire_api
cargo test -p rustwx-render render::tests::projected_place_label_priorities_reduce_auxiliary_and_micro_visual_weight
cargo test -p rustwx-products direct::tests:: -- --skip live
```

For doc-only changes, tests are not necessary. For perimeter/API changes, at
least run:

```powershell
cargo test -p rusty-weather --bin rw_fire_api
cargo check -p rusty-weather --bin rw_fire_api --bin rw_render
```

For renderer overlay changes, add focused tests in `rustwx-render` or
`rustwx-products`, then visually inspect proof outputs.

## What Fable Should Do First

Recommended next actions:

1. Read this file and `docs/CAFire_Fuel_Production.md`.
2. Confirm the worktree is clean except generated `outputs/`.
3. Run the existing local API and one known proof render.
4. Implement perimeter-to-bounds with tests.
5. Thread perimeter overlay through all render lanes.
6. Generate a proof perimeter plot using topo, counties, max local labels, and
   `cafire-with-fuels`.
7. Run a small load test for drawn boxes and perimeter-derived boxes.
8. Write down Hetzner deployment assumptions but do not touch the server until
   the user explicitly says to.

The best next proof would be:

- A synthetic or user-provided California fire perimeter.
- 50 km padding.
- Optional extension downwind or toward a chosen bearing.
- Products: `hdw`, `vpd_2m`, `fire_weather_composite`,
  `vpd_fuel_receptive`, `fire_potential_composite`, `smoke_column`.
- Output: WebP preview and PNG full-res.
- Basemap: `topo`, counties on, `place_label_density=4`.

## What Not To Do

- Do not port RustWX wholesale.
- Do not make WxStore the replacement backend.
- Do not build a Python wheel path as the main integration.
- Do not remove topo/counties/town controls because labels are imperfect.
- Do not fake fuel data.
- Do not benchmark one happy-path render and call it production-ready.
- Do not modify Hetzner without explicit user permission.
- Do not commit `outputs/` or other generated render artifacts.

## Mental Model

This project is now close enough to be useful:

- `.rws` stores are the source of truth.
- RustWX plotting quality is preserved through Rust render crates.
- Drawn local domains already work.
- Fuel-aware product rendering exists when fuel grids are present.
- WebP previews and caching make website-style use plausible.
- The next leap is not a new rendering engine. It is production polish:
  perimeter domains, overlay support, queue/cache hardening, Hetzner load tests,
  and UI paths that expose all the controls without confusing users.

# CAFire Weather API — Integration Contract v1

Paste this whole document to your coding agent. It is the complete,
tested interface of the CAFire weather render service. Everything here is
live today on the staging server and will move to production unchanged —
**only the base URL changes**. Build against `{BASE}` and make it a config
value.

- Content types: requests are `application/json`; plots come back as
  lossless WebP files; meteograms as SVG (or JSON).
- No authentication in v1 (server-side rate limiting may be added; handle
  HTTP 429 gracefully if it appears).
- All times are UTC. Model runs are identified by slug: `YYYYMMDD_CCz`
  (e.g. `20260701_00z`).

---

## 1. Health

`GET {BASE}/api/health` → 200
```json
{ "ok": true, "service": "rw-fire-api", "render_gate": {"active":0,"max_active":2,"waiting":0}, ... }
```
Poll this for a status dot. Anything non-200 = service down.

## 2. Render maps (async job)

`POST {BASE}/api/render` with JSON body:

```json
{
  "model": "hrrr",
  "run": "20260701_00z",
  "hour": 12,
  "products": "cafire-anomaly",
  "output_format": "webp",
  "plot_style": "operational",
  "basemap_style": "topo",
  "county_linework": true,
  "place_label_density": 3,
  "place_label_size": 2,
  "output_width": 1800,
  "domain_slug": "cafire_california",
  "bounds": [-126.0, -113.8, 31.9, 42.5]
}
```

- `bounds` = `[west, east, south, north]` degrees. Alternatively, for
  incident framing, replace `bounds` with:
  - `"perimeter": [[lon,lat], [lon,lat], ...]` (a polygon ring),
  - `"padding_km": 25 | 50 | 100`,
  - optional `"extend": {"direction_deg": 225, "distance_km": 40}`,
  - optional `"overlay_perimeter": true` to draw the perimeter on the map.
- `hour` is the forecast hour. Day-window / anomaly / climatology products
  ignore it (they fold whole windows); hourly products honor it.
- Optional `"title_note": "Aspen Acres Fire"` — appended to every plot
  title as " (Aspen Acres Fire)". Free text, sanitized, max 60 chars.
- `basemap_style`: `topo` | `filled` | `white`. `place_label_density`:
  0–4. `output_width`: up to ~2400.

Response: `202 { "id": "job-...", "status_url": "/api/jobs/job-...", "cache": "hit" | "miss" }`
Identical requests are cache-keyed — a `hit` returns the finished job
immediately.

`GET {BASE}/api/jobs/{id}` → poll every ~700 ms until:
```json
{
  "id": "job-...",
  "state": "queued" | "running" | "succeeded" | "failed",
  "message": "rendered 11 file(s) in 4633 ms",
  "files": [ { "name": "rustwx_hrrr_..._vpd_day_max_percentile.webp",
               "url": "/outputs/job-.../rustwx_..._.webp",
               "bytes": 612345 } ],
  "stderr_tail": "... (only useful when state = failed)"
}
```
Fetch images from `{BASE}{files[i].url}`. On `failed`, show `message`.
Errors: 400 = bad request body (message in `{"error": ...}`).

## 3. Product slugs for `products` (comma list or preset)

**Presets:** `cafire-anomaly` (11 seasonal-anomaly maps),
`cafire-record` (11 vs-all-time-record maps), `cafire-core`,
`cafire-with-fuels`, `fuels`, `windowed`.

**Hourly weather:** `2m_temperature_10m_winds`,
`2m_relative_humidity_10m_winds`, `2m_dewpoint_10m_winds`,
`10m_wind_gusts`, `visibility`, `smoke_pm25_native`, `smoke_column`,
`vpd_2m`, `hdw`, `fire_weather_composite`.

**Day windows:** `2m_temp_0_24h_{max,min,range}`,
`2m_rh_0_24h_{max,min,range}`, `2m_dewpoint_0_24h_{max,min,range}`,
`2m_vpd_0_24h_{max,min,range}` (also `24_48h` and `0_48h` variants on
extended cycles), `10m_wind_1h_max`, `10m_wind_run_max`,
`10m_wind_0_24h_max`, `qpf_1h`, `qpf_6h`, `qpf_24h`.

**Fuels (daily):** `kbdi`, `erc`, `burning_index`,
`dead_fuel_moisture_{1h,10h,100h,1000h}`, `daily_precip_fuel_context`,
`landfire_fuel_model`, `landfire_fuel_loading` — composites:
`fuel_receptiveness`, `fire_potential_composite`, `hdw_fuel_receptive`,
`vpd_fuel_receptive`, `erc_hdw_composite`.

**Anomaly (seasonal ±7-day baseline):** `vpd_day_max_percentile`,
`min_rh_day_percentile`, `wind_day_max_percentile`,
`gust_day_max_percentile`, `hdw_wind_day_percentile`,
`hdw_gust_day_percentile`, `hours_rh15_gust25_percentile`,
`hours_rh20_gust25_percentile`, `hours_rh20_wind20_percentile`,
`overnight_rh_recovery_percentile`, `surface_fire_weather_potential`.
**vs all-time record:** append `_vs_record` to any of the above.

**Climatology reference maps:** `climo_ref:<base>:<stat>:<target>` where
`<base>` = anomaly slug minus `_percentile` (e.g. `vpd_day_max`),
`<stat>` ∈ `p05,p10,p25,p50,p75,p90,p95,p99` (+ `max`, record-only),
`<target>` = `doy196` (day-of-year 1–365, no-leap) or `record`.
Example: `climo_ref:vpd_day_max:p95:doy196`.

## 4. Point meteogram

`GET {BASE}/api/meteogram?lat=38.58&lon=-121.49&run=20260701_00z`
→ `image/svg+xml` (render it in an `<img>` or inline).

Optional params: `panels=temp,rh,vpd,wind,precip,fuels,smoke` (default:
all), `title=Sacramento`, `utc_offset=-7` (local-time axis row),
`format=json` (raw sampled series + units instead of SVG).
Errors: 400/422 JSON `{"error": ...}`.

**Chart anything:** `&vars=composite_reflectivity,pwat,erc` (up to 8)
replaces the curated panels with one auto-scaled panel per stored
variable — any of the ~100 variables in the store works. Discover them
via `GET {BASE}/api/vars[?model=hrrr][&run=latest]` →
`{"vars": [{"name": "...", "units": "..."}]}`. These chart URLs are
stable and shareable — embed or link them directly.

## 5. Domain presets used by the reference console

`cafire_california` `[-126.0,-113.8,31.9,42.5]` ·
`cafire_wide_west` `[-125.7,-103.8,31.9,46.5]` ·
`sierra_front` `[-121.9,-117.6,37.2,41.2]` — but any bounds work.

## 6. Reference client

`GET {BASE}/` serves a complete working console (single-file HTML/JS) that
exercises every endpoint above — read its source for canonical usage.

## 7. Run discovery

Pass `"run": "latest"` (render body) or `run=latest` (meteogram query) —
the server resolves it to the newest ingested run via the refresh
daemon's manifest. For explicit control:

`GET {BASE}/api/runs[?model=hrrr]` → 200
```json
{ "model": "hrrr", "runs": ["20260702_04z", "20260701_00z"],
  "latest": { "run": "20260702_04z", "stored_hours": [0,1,...],
              "target_max_hour": 18, "complete": true, "updated_unix": 0 } }
```
`latest` is `null` until the daemon has published a manifest — fall back
to `runs[0]`. A 422 from `run=latest` means the same thing.

## 8. Stable vs. coming

**Stable now:** everything above — schemas, slugs, presets, the `latest`
alias, `/api/runs`, file URL shape, cache semantics.
**Coming with production:** the production base URL (build against a
config value); automated hourly refresh keeps `latest` fresh with no
client change.

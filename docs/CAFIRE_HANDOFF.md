# CAFire Weather Service — Complete Handoff (July 2026)

**Give this whole document to your coding agent.** It is the complete,
live-tested interface of the CWT weather service, plus an honest map of
what is operational, what is partial, and what is not built yet. It
supersedes the earlier "Integration Contract v1" and "Product Inventory"
docs (both still in `docs/`, both now subsets of this).

**Base URL today:** `https://cafire.wxsection.com` — live now, behind
Cloudflare, self-refreshing around the clock. Build against a `{BASE}`
config value; if the service moves to its own hostname later, only that
value changes.

**Reference implementation:** `{BASE}/lab` is a complete single-file
web client that exercises every endpoint below — product maps, fire
framing, meteograms, custom charts, outlook cards. It is
our opinion of *what works best given the shape of this service*: treat
it as canonical usage (view source; there is no build step), not as a
constraint. Anything it does, your site can do with plain `fetch`.

---

## 0. Operational status at a glance

| Capability | Status | Notes |
|---|---|---|
| HRRR map rendering (hourly, 3 km) | **Live** | New run every hour, self-refreshing |
| RTMA anomaly + vs-record suite | **Live** | 7-year climatology on-server, ocean-masked |
| Climatology reference maps | **Live** | Any DOY, any percentile |
| Day-window maps (0–24 h etc.) | **Live** | Use `run=latest-day` (see §7) |
| Fuels maps (gridMET/NFDRS + LANDFIRE) | **Live** | Daily cadence, 1–3 day upstream lag |
| Fire perimeter framing + WFIGS feed | **Live** | National, >300 acres, top 60 |
| Point meteograms (SVG/JSON) | **Live** | Curated panels or any variables |
| Custom "chart anything" charts | **Live** | ~103 HRRR / ~86 GFS / 9 NBM variables |
| Shareable outlook cards (daily/hourly) | **Live** | HI/LO strips + wind + precip rows |
| GFS lane (8 days, 6-hourly) | **Live** | Charts/cards/meteograms; maps not tuned |
| NBM lane (official blend, 10+ days) | **Live** | 9 surface variables, 6-hourly |
| Advanced instability / pyroCb suite | **In development** | Not part of this integration surface yet |
| Render cache + prewarming | **Live** | Common requests return in ~100 ms |
| Authentication / rate limiting | **Not built** | Open in v1; handle 429 if it appears |
| NDFD proper | **Not built** | NBM covers the "official blend" need |
| RRFS, HREF ensembles | **Not built** | Deliberately deferred |
| Satellite / lightning | **Separate legacy service** | Your existing `/api/v1/*` — untouched |

---

## 1. Architecture in one paragraph (the "shape")

A single Rust API on the server renders everything on demand from a
local hour-by-hour weather store. Three background daemons keep that
store fresh (HRRR every 5 min, GFS every 15, NBM every 20), a pruner
bounds the disk, and after each completed HRRR run the server pre-renders
the common domain × product combinations plus the four largest active
fires — so the requests a public site is likely to make are usually
**cache hits**. Practical consequences for you: (a) prefer the canonical presets
and `latest`/`latest-day` aliases, because those paths are prewarmed;
(b) identical requests are free — don't be shy about re-requesting;
(c) everything is stateless HTTP + JSON/WebP/SVG — no SDK, no keys.

Measured performance: cold map render ~2–3 s end-to-end through
Cloudflare; cached ~100 ms; full 11-map anomaly suite ~6 s cold;
meteograms and cards are synchronous and return in well under a second.

---

## 2. Health

`GET {BASE}/api/health` → 200
```json
{ "ok": true, "service": "rw-fire-api", "render_gate": {"active":0,"max_active":3,"waiting":0} }
```
Poll for a status dot. Non-200 = down.

## 3. Render maps (async job)

`POST {BASE}/api/render` with JSON:

```json
{
  "model": "hrrr",
  "run": "latest",
  "hour": 12,
  "products": "cafire-anomaly",
  "output_format": "webp",
  "plot_style": "operational",
  "basemap_style": "topo",
  "county_linework": true,
  "place_label_density": 3,
  "output_width": 1800,
  "domain_slug": "cafire_california",
  "bounds": [-126.0, -113.8, 31.9, 42.5]
}
```

- `bounds` = `[west, east, south, north]` degrees — any rectangle works.
- **Incident framing:** replace `bounds` with
  `"perimeter": [[lon,lat], ...]` (polygon ring),
  `"padding_km": 25 | 50 | 100`,
  optional `"extend": {"direction_deg": 225, "distance_km": 40}`,
  optional `"overlay_perimeter": true` (draws the perimeter),
  optional `"title_note": "Aspen Acres Fire"` (appended to every plot
  title; sanitized, ≤60 chars).
- `hour` = forecast hour. Day-window / anomaly / climatology products
  ignore it (they fold whole windows); hourly products honor it.
- `basemap_style`: `topo` | `filled` | `white`. `place_label_density`
  0–4. `output_width` up to ~2400.
- `temp_units`: `"c"` for Celsius; surface temperature maps (2 m temp/
  dewpoint, wet-bulb, heat index, ...) default to °F (`"f"`). Upper-air
  (pressure-level) temperature maps always render °C.

Response `202`:
```json
{ "id": "job-...", "status_url": "/api/jobs/job-...", "cache": "hit" }
```
`GET {BASE}/api/jobs/{id}` → poll ~700 ms until `state` is `succeeded`
or `failed`; `files[]` gives `{name, url, bytes}` — fetch images from
`{BASE}{url}`. On `failed`, show `message`. 400 = bad body, message in
`{"error": ...}`.

## 4. Product slugs (comma list or preset)

**Presets:** `cafire-anomaly` (11 seasonal-anomaly maps),
`cafire-record` (11 vs-record maps), `cafire-core`, `cafire-with-fuels`,
`fuels`, `windowed`.

**Hourly weather:** `2m_temperature_10m_winds`,
`2m_relative_humidity_10m_winds`, `2m_dewpoint_10m_winds`,
`10m_wind_gusts`, `visibility`, `smoke_pm25_native`, `smoke_column`,
`vpd_2m`, `hdw`, `fire_weather_composite` (plus radar/precip,
upper-air, severe, and categorical p-type families — the Lab's product
lists enumerate every slug; a build-time test guarantees that list is
complete).

**Day windows:** `2m_temp_0_24h_{max,min,range}`,
`2m_rh_0_24h_{max,min,range}`, `2m_dewpoint_0_24h_{max,min,range}`,
`2m_vpd_0_24h_{max,min,range}` (plus `24_48h`/`0_48h` on extended
cycles), `10m_wind_1h_max`, `10m_wind_run_max`, `10m_wind_0_24h_max`,
`qpf_1h`, `qpf_6h`, `qpf_24h`.

**Fuels (daily):** `kbdi`, `erc`, `burning_index`,
`dead_fuel_moisture_{1h,10h,100h,1000h}`, `daily_precip_fuel_context`,
`landfire_fuel_model`, `landfire_fuel_loading`; composites
`fuel_receptiveness`, `fire_potential_composite`, `hdw_fuel_receptive`,
`vpd_fuel_receptive`, `erc_hdw_composite`.

**Anomaly (seasonal ±7-day baseline, n≈105/cell):**
`vpd_day_max_percentile`, `min_rh_day_percentile`,
`wind_day_max_percentile`, `gust_day_max_percentile`,
`hdw_wind_day_percentile`, `hdw_gust_day_percentile`,
`hours_rh15_gust25_percentile`, `hours_rh20_gust25_percentile`,
`hours_rh20_wind20_percentile`, `overnight_rh_recovery_percentile`,
`surface_fire_weather_potential`.
**vs all-time record (2019–2026, n≈2,695/cell):** append `_vs_record`.
Ocean masked; percentile bins 25/50/75/90/95/99. This suite is the
flagship — nobody else ranks a forecast against seven years of analyzed
reality at each grid cell.

**Climatology reference maps:** `climo_ref:<base>:<stat>:<target>` —
`<base>` = anomaly slug minus `_percentile`; `<stat>` ∈
`p05,p10,p25,p50,p75,p90,p95,p99` (+ `max` for record); `<target>` =
`doyNNN` (1–365) or `record`. Example: `climo_ref:vpd_day_max:p95:doy196`.

## 5. Point meteogram

`GET {BASE}/api/meteogram?lat=38.58&lon=-121.49&run=latest-day`
→ `image/svg+xml` (drop into an `<img>`).

Params: `panels=temp,rh,vpd,wind,precip,fuels,smoke` (default all),
`title=Sacramento`, `utc_offset=-7`, `model=hrrr|gfs|nbm` (default
hrrr), `format=json` (raw series + units). Errors: 400/422 JSON.

**Nearest community, automatic:** if `title` is omitted — or is just a
coordinates string like `"39.250, -123.100"` — the graphic titles
itself with the point's nearest community and offset, e.g.
`39.2500, -123.1000 · 9 mi NE of Ukiah, CA`. A real `title` (fire name,
station) is kept verbatim and gains the community phrase when it fits.
`format=json` includes the same lookup as
`nearest_place: {label, distance_mi, bearing, description}`.

**Chart anything:** `&vars=composite_reflectivity,pwat,erc` (≤8 names,
`[a-z0-9_]`) replaces the panels with one auto-scaled panel per stored
variable — Kelvin auto-converts to °F. Discover names via:

`GET {BASE}/api/vars[?model=hrrr|gfs|nbm][&run=latest]` →
`{"model":"hrrr","run":"...","vars":[{"name":"temperature_2m","units":"K"}, ...]}`
(~103 HRRR, ~86 GFS, 9 NBM variables.)

These chart URLs are stable and shareable — link or embed them directly.

## 6. Shareable outlook cards (the weathermodels-style graphic)

`GET {BASE}/api/daily?lat=38.58&lon=-121.49&model=nbm&run=latest&var=temperature_2m&title=Sacramento`
→ `image/svg+xml`, a self-contained branded card: one column per local
calendar day with HI/LO colored number strips, a **WIND** row (direction
arrow + max speed), a **PCPN** row (bucket inches), and grouped bars.

- `var`: any name from `/api/vars`. Temperature gets the classic
  absolute color scale; other variables get a normalized ramp. Kelvin→°F
  and wind m/s→mph are automatic.
- `model=nbm` → ~10-day card from the official NWS blend (recommended
  default). `gfs` → 8 days. `hrrr` → 1–2 days at high resolution.
- `step=1|3|6` switches from daily columns to hourly/3-h/6-h buckets —
  use with HRRR for an hourly outlook strip. `step=1` gives single-value
  columns; 3/6 keep HI/LO pairs.
- `utc_offset` (default −7) controls the local-day bucketing.
- Partial edge buckets are dropped automatically (no fake daily highs
  from an evening-only stub).
- Nearest-community labeling works exactly as on meteograms: omit
  `title` (or send bare coordinates) and the card's headline becomes the
  nearest community, with the precise offset ("10 mi E of Sacramento,
  CA") next to the coordinates in the header line.

The URL **is** the share link: it always re-renders from the named run
(`latest` stays current). For a frozen snapshot, save the SVG.

## 7. Models, runs, and the two aliases

| Model | Cycles ingested | Hours | Grid | Refresh |
|---|---|---|---|---|
| `hrrr` | every hour | 0–18 (0–48 on 00/06/12/18Z) | 3 km CONUS | ~5 min |
| `gfs` | 00/06/12/18Z | 0–192 by 6 | 0.25° global | ~15 min |
| `nbm` | 00/06/12/18Z | 6–264 by 6 | 2.5 km CONUS | ~20 min |

Run slugs are `YYYYMMDD_CCz`. Two aliases, resolved server-side:

- `latest` — newest **complete** run (never a half-ingested stub).
- `latest-day` — newest run that fully covers a UTC day; **required for
  anomaly / day-window products** (they fold 24 h). Falls back to the
  complete run when no day-covering run exists (e.g. GFS/NBM).

Explicit discovery: `GET {BASE}/api/runs[?model=...]` →
`{"runs":[...], "latest":{"run":"...","stored_hours":[...],"complete":true, ...}}`.
A 422 from an alias means no manifest yet — fall back to `runs[0]`.

**Rule of thumb:** hourly maps and meteograms → `latest`; anything with
"day" in the name → `latest-day`; cards/charts → `latest` per model.

## 8. Active fires (for incident framing)

`GET {BASE}/api/fires` → 10-min-cached WFIGS feed:
```json
{ "fires": [ { "name": "Morrill", "acres": 642029.0,
               "ring": [[lon,lat], ...] } ] }
```
National, >300 acres, top 60 by acreage, rings decimated to ≤240 points.
Feed a `ring` straight into `/api/render`'s `perimeter` with
`padding_km` + `overlay_perimeter` + `title_note` for a labeled,
auto-framed incident map. The top four fires are prewarmed each run.

## 9. Integration recipes (what we'd build in your shoes)

- **Incident page:** `GET /api/fires` → user picks fire → one
  `POST /api/render` with `perimeter`, `padding_km: 50`,
  `overlay_perimeter: true`, `title_note: "<Name> Fire"`,
  `products: "cafire-anomaly"`, `run: "latest-day"` → show the 11 maps.
  The biggest fires are already prewarmed.
- **Daily briefing graphic:** one `<img>` pointed at `/api/daily` with
  `model=nbm`, per station/city. Zero backend needed.
- **"Is today unusual?" banner:** render `surface_fire_weather_potential`
  (seasonal) and its `_vs_record` twin for California once per run.
- **Forecaster deep-dive:** link to `{BASE}/lab` — it's already the
  power-user console (or lift its source into your own page).
- **Point forecast popups:** `/api/meteogram?...&format=json` for
  numbers, or embed the SVG directly.
- **Hour animation:** render hour N, then fire off N±1..3 in the
  background — the shared cache makes subsequent loop hits instant (the
  Lab does exactly this; copy its prefetch pattern).

Style/system notes: everything visual is our house style (dark,
CWT-branded, Cloudflare-cacheable). If you need your own branding on
cards/charts, that's a parameter away — ask.

## 10. Not operational / known limits (read before promising features)

- **No auth, no rate limiting** yet. The render gate allows 3 concurrent
  jobs; heavy bursts queue. If you expect real public traffic spikes,
  tell us and we'll front more aggressive caching.
- **NDFD** is not ingested; **NBM** (the blend NDFD is built from) is,
  and covers the "official forecast" use-case. If you specifically need
  forecaster-edited NDFD grids, that's a scoped future lane.
- **GFS/NBM map rendering** works mechanically but the map styling is
  tuned for HRRR; use GFS/NBM for charts, cards, and meteograms today.
- **NBM carries 9 surface variables** (T/Td/RH/u/v/gust/precip/PWAT/
  visibility) — no smoke, no fuels, no severe fields.
- **Advanced instability / pyroCb products** (entrainment CAPE, PFT,
  fire-modified parcels) are in development on dedicated compute and
  intentionally excluded from this surface for now — don't promise them
  on the site yet; they'll arrive as a documented addition.
- **RRFS / HREF** deliberately deferred.
- **Satellite & lightning** remain on your existing legacy service
  (`/api/v1/*`) — this service does not replace them yet.
- Fuels data lags upstream (gridMET) by 1–3 days; the server walks back
  automatically and stamps products with the fuel date used.
- Retention: ~3 recent runs per model plus the newest extended run.
  Old job outputs are pruned; don't hotlink `/outputs/...` URLs long-term
  — re-request instead (cache makes it free). Card/chart URLs with
  `run=latest` are permanent by construction.

## 11. Stability contract

Everything in §§2–8 — schemas, slugs, presets, aliases, URL shapes,
cache semantics — is what production runs today, and we treat it as
frozen: additions will be backward-compatible; breaking changes get a
versioned path. Build against `{BASE}` as config and you will not need
to touch your integration again.

# CAFire Weather Service — Addendum 1: Vertical Products + Legacy Switchover (July 2026)

**Companion to `CAFIRE_HANDOFF.md`** — give both to your coding agent.
Everything here is additive per the stability contract (handoff §11):
nothing you already built changes. Two new endpoints are live, and this
doc includes the playbook for retiring your last legacy-HRRR
dependencies whenever you're ready.

Same conventions as the main doc: build against `{BASE}`
(`https://cafire.wxsection.com` today), never show that hostname to
users, treat `{BASE}/lab` as the working reference client (view source —
it has a working line-draw UI and point picker for both new products).

---

## 1. Vertical cross sections (NEW)

`GET {BASE}/api/xsection?lat0=38.0&lon0=-122.5&lat1=39.0&lon1=-118.5&run=latest&hour=12`
→ `image/svg+xml` (~150–250 KB; drop into an `<img>`). Warm renders
~200 ms; synchronous — no job polling.

A vertical slice of the atmosphere along the A→B line: log-pressure
axis (1000→150 hPa), field fill, terrain silhouette from the model's
own surface pressure, wind barbs aloft (knots), and the endpoints named
by nearest community in the title ("near Santa Venetia, CA → 18 mi E of
Schurz, NV").

Params:
- `lat0, lon0, lat1, lon1` — the two endpoints (any CONUS pair; a
  ~100–300 mile line reads best).
- `field` = `temperature` (default, °F) | `rh` (%) | `wind` (mph).
- `run` / `hour` — same aliases and semantics as everywhere else.
- `utc_offset` (default −7) for the valid-time label.
- `format=json` — raw arrays instead of the SVG. Top-level keys (as
  served): `a`, `b` (endpoint metadata), `distance_mi`, `lat`, `lon`,
  `levels_hpa`, `values` (per column × level), `terrain_psfc_hpa`,
  `units`, `field`, `model`, `run`, `hour`, `valid`, and `path_note`
  stating the sampling approximation.

Honesty note (also printed on the plot): the path is a straight
lat/lon interpolation between the endpoint grid cells — mid-path drift
vs a true great circle is ≤ 2 grid cells (~6 km). Fine for weather;
stated so nobody assumes survey-grade geometry.

## 2. Point soundings / skew-T (NEW)

`GET {BASE}/api/sounding?lat=38.58&lon=-121.49&run=latest&hour=12`
→ `image/png` (~900 KB). Warm renders ~650 ms; synchronous.

A complete composed sounding in the house dark style: skew-T/log-p with
temperature/dewpoint/wet-bulb/parcel traces and CAPE/CIN shading, wind
stave, hodograph with storm motions (Bunkers/Corfidi), a full parameter
suite (parcels, shear/SRH by layer, lapse rates, PWAT, thermodynamic
indices, composites), a locator map, and an **ECAPE panel** —
entrainment-adjusted parcel energy, which standard sounding sites don't
carry. It is labeled experimental and deliberately shows "--" for the
survival ratio when CAPE is too small for the ratio to mean anything.

Params: `lat`, `lon`, `run`, optional `hour`, `utc_offset`,
`format=json` — top-level keys as served: `point`, `cell`,
`nearest_place`, `profile` (level arrays), `indices` (flat object of
every computed parameter), `ecape_note`, `model`, `run`, `hour`,
`valid`.

Placement suggestions: fire-detail pages (one sounding at the fire
point per run) and anywhere you show our meteogram — they're
complements: meteogram = one point through time, sounding = one point's
full vertical structure at one time.

## 3. Data availability for BOTH new endpoints (read this)

These render from stored 3-D volumes, which the server began keeping on
**2026-07-03 00Z**. Hours ingested before that return
`422 {"error":"this hour predates vertical-volume storage — pick a newer run/hour"}`.
Within a day of that date every retained run has full coverage, but do
not assume — **discover renderable hours**:

`GET {BASE}/api/runs?var=temperature_iso`
→ the usual runs payload plus `hours: {"<run>": [0,1,...]}` filtered to
hours that actually carry the volumes. (The `hours` map without `?var=`
lists all stored hours per run — also new, useful for honest hour
pickers generally.)

Wire your hour picker to that and the 422 becomes unreachable. The Lab
does exactly this — copy its `probeVar` pattern.

## 4. Legacy switchover playbook (when you're ready — no deadline)

**What stays forever (no action):** satellite and lightning on
`/api/v1/*`. Those workers are untouched by any of this.

**What this retires:** the legacy HRRR-derived endpoints — the old
hourly gallery images and the legacy cross sections / soundings /
meteograms. Every one of those capabilities now exists on the new
surface, better and self-refreshing.

Easiest sequence, from your side:

1. Ship your v2 integration (the plan you sent — Lab section, fire
   accordion, location cards). That replaces the gallery + meteogram
   uses.
2. Point any remaining cross-section / sounding UI at the two endpoints
   above (or just deep-link into your Lab section).
3. Grep your codebase for `/api/v1/` references and confirm the only
   survivors are satellite + lightning. If any other `/api/v1` path is
   load-bearing for you, tell us which — we'll either confirm it's
   satellite/lightning-family (survives) or give you the new-surface
   equivalent.
4. Say "go." We stop the legacy HRRR workers server-side. **No URLs
   change, no CSP changes, satellite/lightning unaffected.** Rollback
   is minutes if anything was missed, and nothing is deleted for two
   weeks after cutover.

Why we care: the legacy stack independently downloads ~170 GB of HRRR
per day and holds tens of GB of scratch on the same disk that serves
you. The one degradation event this service has had (a stale-data
window on July 2) was disk-pressure-related. Cutover removes that
entire class of risk.

## 5. Small recent additions you may have missed

- `temp_units` on `POST /api/render` — surface temperature maps now
  default to **°F**; pass `"temp_units": "c"` for Celsius. Upper-air
  temps stay °C by convention. (Main doc §3 updated.)
- Every map plot now carries a **croppable branding strip** at the very
  top — slice off the top band and the plot is still complete and fully
  labeled. Use whichever form fits your layout.
- Outlook cards draw true meteorological wind barbs; point products
  title themselves with the nearest community (~32k-place gazetteer);
  card/meteogram watermarks read `cafire.org/weather`.
- The Lab now shows a hand-written plain-language note for every
  non-obvious product (ECAPE family, anomaly suite, fuels, composites).
  If you want that copy for your own UI, it's the `PRODUCT_NOTES`
  object in the Lab source — take it verbatim, it was written to be
  user-facing.

## 6. Updated status table (delta from the main doc)

| Capability | Status |
|---|---|
| Vertical cross sections (SVG/JSON) | **Live** |
| Point soundings / skew-T (PNG/JSON) | **Live** |
| °F default on surface temperature maps | **Live** |
| Volume-bearing hours discovery (`/api/runs?var=`) | **Live** |
| Legacy HRRR endpoints | **Deprecated — retire at your pace (§4)** |
| Satellite / lightning (`/api/v1/*`) | **Unchanged, staying** |

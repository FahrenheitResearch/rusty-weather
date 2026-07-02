# CAFire Weather — Product Inventory (July 2026)

Everything below is rendered on demand by the CAFire weather API
(`POST /api/render` → WebP plots; `GET /api/meteogram` → SVG/JSON) from a
local HRRR + RTMA-climatology store. All Rust, no external services, no API
keys. Typical render: under 2 s per map cold, ~100 ms cached; full 11-map
anomaly suite ~6 s.

**Forecast model:** HRRR 3 km, new run every hour (00/06/12/18Z reach 48 h,
other cycles 18 h). **Climatology:** RTMA 2.5 km analysis archive,
2019–2026 (~2,695 days), FireWxAtlas frozen formulas.

**Domains:** California, Wide West, Sierra Front presets — plus draw-a-box
(any custom rectangle) and fire-perimeter framing (paste a perimeter, pick
25/50/100 km padding, optionally extend toward expected spread; the
perimeter draws on the map).

---

## 1. Standard weather maps — every forecast hour

| Product | Shows |
|---|---|
| 2 m temperature + 10 m wind | Temperature with wind barbs |
| 2 m dewpoint + 10 m wind | Moisture with wind barbs |
| 2 m relative humidity + 10 m wind | RH with wind barbs |
| 10 m wind gusts | Gust field |
| Visibility | Surface visibility |
| Near-surface smoke (PM2.5) | HRRR-Smoke at 8 m |
| Column smoke | Vertically integrated smoke |
| 2 m VPD | Vapor pressure deficit (dryness stress) |
| HDW | Hot-Dry-Windy index |
| Fire weather composite | Blended fire-weather severity |

## 2. Day-window maps — daily extremes in one map

0–24 h, 24–48 h, and 0–48 h windows (48 h windows need an extended cycle):

| Family | Stats |
|---|---|
| 2 m temperature | max / min / range |
| 2 m RH | max / min / range |
| 2 m dewpoint | max / min / range |
| 2 m VPD | max / min / range |
| 10 m wind | 1 h max, run max, 0–24 h max |
| QPF | 1 h / 6 h / 24 h precipitation |

## 3. Fuels — daily (gridMET/NFDRS + LANDFIRE)

| Layer products (10) | Weather × fuel composites (5) |
|---|---|
| ERC, Burning Index, KBDI | Fuel receptiveness |
| Dead fuel moisture 1 h / 10 h / 100 h / 1000 h | Fire potential composite |
| Daily precip fuel context | HDW × receptive fuels |
| LANDFIRE fuel model + loading | VPD × receptive fuels, ERC × HDW |

## 4. RTMA anomaly suite — the flagship (nobody else has this)

Today's HRRR forecast ranked against 7 years of what actually happened at
that exact grid cell. Two baselines for each of 11 products:

- **Anomaly vs Climo** — vs the same calendar date ±7 days (n≈105/cell):
  *"is this unusual for the date?"*
- **vs All-Time Record** — vs every day 2019–2026 (n≈2,695/cell):
  *"is this severe in absolute terms?"*

Products (each in both baselines): day-max VPD, day-min RH, day-max wind,
day-max gust, surface HDW (wind & gust), critical threshold-hours
(RH≤15%+gust≥25 mph, RH≤20%+gust≥25 mph, RH≤20%+wind≥20 mph), overnight RH
recovery (12Z–06Z), and the weighted Surface Fire Weather Potential
composite. Ocean is masked; percentile bins 25/50/75/90/95/99.

## 5. Climatology browser — the normals themselves

For any calendar date: the P50→P99 ladder of any anomaly product ("what
does p95 VPD normally look like on July 15"), plus all-time P99 and the
all-time record max. Rendered in real units (kPa, mph, %, hours).

## 6. Point meteogram — click a spot, get the whole story

Customizable multi-panel SVG for any lat/lon, every stored hour:
T/Td, RH (15/20% critical lines), VPD + HDW, wind/gust with direction
arrows, hourly precip, ERC + 10-h fuel moisture, smoke. With night
shading, local-time axis, critical-hour highlighting, and dashed
climatology reference lines (normal/big/extreme day for that date at that
point). `format=json` returns the raw numbers.

---

## Delivery to CAFire.org

API-based, same as always: the site calls `POST /api/render` (JSON body:
products, domain or perimeter, quality) → job → WebP URLs, plus
`GET /api/meteogram?lat&lon&run`. The ops console at `/` is a working
reference client for the whole surface.

**Live today** on the local test server. **Before unattended production
(Hetzner):** automated run refresh daemon, disk retention pruner, and
render prewarm — then build on the server, ship the ~35 GB climatology
store, and point CAFire.org routing at it.

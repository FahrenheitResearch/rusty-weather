# Roshydromet ICON-Ru13/6N29 contract

Rusty Weather ingests the public Roshydromet ICON-Ru13/6N29 limited-area
forecast from its WMO WIS2 core-data feed. Roshydromet remains the producer
and licensing publisher. A WIS2 Global Cache is a transport only.

## Authoritative discovery and delivery

- Product description: <https://meteoinfo.ru/en/wis2-srf-products-of-wipps-dc-moscow>
- WIS2 discovery identifier:
  `urn:wmo:md:ru-roshydromet:wipps-dc.forecast.short-range.deterministic.limited-area.icon`
- WIS2 topic:
  `cache/a/wis2/ru-roshydromet/data/core/weather/prediction/forecast/short-range/deterministic/limited-area`
- Canonical listing API: `http://wis2box.mecom.ru/oapi/collections/messages/items`
- Canonical object root:
  `http://wis2box.mecom.ru/data/{YYYY-MM-DD}/wis/{discovery-id}/`
- Preferred replicated transport:
  `https://wis2globalcache.s3.amazonaws.com/data/ru-roshydromet/data/core/weather/prediction/forecast/short-range/deterministic/limited-area/`

The source metadata declares `wmo:dataPolicy=core`. The server reports the
Roshydromet producer, WMO core-data policy, transformation notice, and source
documentation independently of the cache/origin transport used for a run.

## Native schedule, grid, and selected inventory

The normalized lane admits 00Z and 12Z cycles and three-hourly leads f003
through f072. It does not fabricate f000. A complete audited 2026-08-12 00Z
listing contained 1,272 single-message objects: 52 objects for each B-Q lead
group and 55 for each R-Y group.

The decoded regular latitude/longitude grid is 697 by 213 at 0.25 degrees,
from 35.0N, 19.5E through 88.0N, 193.5E. It crosses the antimeridian. Ingest
normalizes and rotates complete longitude rows and every corresponding field
plane together; it does not clip or reorder values independently.

The bounded pressure bundle contains 24 objects per valid time:

- temperature, u wind, v wind, and geopotential height at 250, 500, 700, 850,
  and 925 hPa;
- relative humidity at 500, 700, 850, and 925 hPa.

The bounded surface bundle contains ten objects: 2 m temperature, 2 m
dewpoint, 10 m u/v wind, mean-sea-level pressure, trailing three-hour wind-gust
maximum, total/low/middle cloud cover, and cumulative precipitation. The
precipitation object family changes at its published boundaries: `RUMS`
through f024, `RUMC` from f027 through f048, and `RUWC` from f051 through
f072. Selection is based on decoded statistical end time, not filename-only
relabeling.

The feed also contains relative vorticity, divergence, convective
precipitation, precipitation type, extrema, and CAPE messages. They are not
mapped because the current canonical RWS selectors do not express all of
their native semantics. Derived and heavy diagnostics are disabled for this
sparse pressure profile.

Each object is a WMO bulletin: `SOH`, a bounded sequence/abbreviated-heading
header, one exact GRIB2 message, then `CR CR LF ETX`. Acquisition keeps the raw
object in its cache but admits only an exactly validated wrapper and writes
bare GRIB messages into the deterministic assembled bundle.

## Bounded live evidence

On 2026-08-14, the official 2026-08-12 00Z f003 origin objects were ingested
with the `sounding` profile, writer `--verify`, and four worker threads. The
pressure bundle fetched 24 objects (about 4.2 MB) and the surface bundle ten
objects (about 1.7 MB). The resulting 6.3 MB RWS hour passed bit-exact checks
for all five realized surface variables and quantization-bound checks for all
five pressure volumes. Deep validation reported ten variables, 3,095 chunks,
and 6,382,472 payload bytes with `ok`.

Realized pressure coverage was temperature at five levels, relative humidity
at four levels, and u wind, v wind, and height at five levels each. Surface
pressure and orography are not published in the selected feed and were
explicitly absent rather than synthesized. The manifest recorded provider
`roshydromet-wipps-dc`, roles `pressure` and `surface`, and logical products
`rws-pressure` and `rws-surface`.

Representative exact source evidence:

| Object | Bytes | SHA-256 |
| --- | ---: | --- |
| `A_YTRB85RUMS120000_C_RUMS_20260812000000.grib2` | 185,791 | `F39598CBB8411C9989E446B5D8915951A1496E92B42D81F553F5520B5C5DAB65` |
| `A_YTRB98RUMS120000_C_RUMS_20260812000000.grib2` | 185,791 | `A67B300E7815B5EBD698D3B777219A515FD52431D220DBA9B1E8BAE7124FBFAD` |
| `A_YERD98RUMS120000_C_RUMS_20260812000000.grib2` | 204,372 | `BFE6912C2A79F14485F6EFA008FEF6D421D4C7769D66EA5473CC160A9384A478` |

For all three objects, the source URL is the canonical root above plus the
object name. The GRIB message starts at byte 31; the first two messages have a
declared GRIB length of 185,756 bytes and the precipitation message 204,337
bytes. Each has the observed four-byte `CR CR LF ETX` trailer.

# DWD ICON regular-grid integration

This audit pins the public object layout observed on 2026-08-14 and the
decoder, acquisition, normalization, scheduler, and query contracts now used
by the live-verified ICON-EU and ICON-D2 regular-grid RWS lanes.

## Authoritative source and policy

Both feeds are owned and published by the Deutscher Wetterdienst (DWD) on its
[Open Data server](https://opendata.dwd.de/weather/nwp/). DWD's
[NWP forecast-data page](https://www.dwd.de/EN/ourservices/nwp_forecast_data/nwp_forecast_data.html)
documents ICON-EU and ICON-D2, GRIB2 delivery, regular-grid availability, and
the operational model cadence. DWD's current
[legal notice](https://www.dwd.de/EN/service/legal_notice/legal_notice.html)
licenses open spatial data under CC BY 4.0 with source acknowledgement. Its
[source rules](https://www.dwd.de/EN/service/legal_notice/templates_dwd_as_source.html)
require `Source: Deutscher Wetterdienst` beside unaltered data (including
extracts or format changes); materially processed or redesigned products must
at least identify DWD in a central source list or imprint and should state the
kind of modification. RWS output must therefore retain DWD as data owner,
surface CC BY 4.0 and the source notice, and say that RWS normalized or rendered
the provider data. A future mirror must not replace DWD ownership with the
transport operator.

## Exact Open Data inventory

DWD publishes one externally bzip2-compressed GRIB2 object per
field/level/named forecast time. There is no per-family byte-range index. The
acquisition adapter uses the shared component-bundle lane. The reusable
transport layer recognizes `.bz2` or `BZh`, bounds expansion, and gives the
cache decoded GRIB bytes keyed by the original compressed URL. Logical
`rws-pressure` and `rws-surface` products expand only to strict allowlisted
field/level tokens, in deterministic order, before their GRIB messages are
assembled for canonical extraction.

ICON-EU exposes cycles at 00/03/06/09/12/15/18/21 UTC. On the captured server,
00/06/12/18 UTC had 93 named times: hourly f000-f078, then every three hours
through f120. The four intervening cycles had 34 named times: hourly f000-f030,
then f036/f042/f048. This is the exact public-file inventory even though model
documentation can describe a slightly longer computed short-cycle horizon.

ICON-D2 exposes all eight three-hourly cycles and 49 named times, hourly
f000-f048. Some hourly objects contain four 15-minute messages. Selection must
use GRIB valid/end time, not assume one message per filename.

The canonical surface object directories are `t_2m`, `td_2m`, `relhum_2m`,
`u_10m`, `v_10m`, `pmsl`, `ps`, `hsurf`, and `tot_prec`. ICON-EU uses uppercase
field tokens; ICON-D2 uses lowercase tokens and a `2d_` marker for forecast
surface files. `hsurf` is one time-invariant object per cycle. `tot_prec` is a
run-total accumulation. It must not be relabelled as a native trailing-one-hour
field.

For pressure volumes, both feeds publish `t`, `relhum`, `u`, `v`, and `fi` as
separate objects. Neither publishes native pressure-level dewpoint in this
inventory, so normalized soundings should retain `rh_iso`. ICON-EU publishes
20 levels:

`50, 70, 100, 150, 200, 250, 300, 400, 500, 600, 700, 775, 800, 825, 850, 875, 900, 925, 950, 1000 hPa`.

ICON-D2 publishes 11 levels:

`200, 250, 300, 400, 500, 600, 700, 850, 950, 975, 1000 hPa`.

The exact templates, object counts, official sample URLs, byte lengths, and
compressed SHA-256 values are in
`crates/rustwx-io/tests/fixtures/dwd-icon-regular-latlon-20260814.inventory.txt`.

## Decoder audit

Bounded live samples covered the complete core surface set (2 m temperature,
2 m dewpoint, 2 m RH, 10 m U/V, MSL pressure, surface pressure, and `HSURF`)
plus 500 hPa temperature, RH, U, V, and `FI` from each model. ICON-EU uses
regular latitude/longitude grid template 3.0 at 1377x657 with CCSDS packing
template 5.42. ICON-D2 uses grid template 3.0 at 1215x746, simple packing
template 5.0, and a bitmap that kept 754,862 in-domain cells out of 906,390
grid positions. Parsing, unpacking, grid construction, scan normalization, and
canonical extraction completed for all samples.

DWD `FI` is WMO geopotential parameter 0/3/4 in m2 s-2, not NOAA-style
geopotential-height parameter 0/3/5. The extractor now accepts both and divides
geopotential by standard gravity to emit honest canonical `gpm`.

DWD `HSURF` is WMO geometric-height parameter 0/3/6 at the surface. The
surface-orography selector now accepts that parameter and preserves its metre
values. Geometric height is deliberately accepted only for surface orography;
pressure-level canonical height still requires geopotential or geopotential
height.

## Time semantics and live RWS validation

DWD encodes forecast and statistical durations in WMO unit 0 (minutes).
`tot_prec` f001/f002 proved run-total windows of 0-60 and 0-120 minutes;
ICON-D2 also carried 75/90/105 and 135/150/165-minute messages in those hourly
objects. The shared selector now compares exact seconds and refuses to round
or truncate a quarter-hour endpoint onto the integer-hour RWS axis. Regression
coverage pins all four endpoints in each object and selects only 60 or 120
minutes respectively. The field remains honestly named `apcp_run_total`.
An additional bounded ICON-D2 f001-f002 surface ingest exercised those live
multi-message objects: both hours exact-verified all nine published 2-D
variables, including `apcp_run_total` and the cycle-static `orography` plane,
then deep-validated 270 chunks and 25,474,796 payload bytes. Surface-only
ingest omits the pressure bundle and its provenance instead of claiming an
unfetched role.

On 2026-08-14, bounded official 00z f000 sounding ingests for both models ran
through DWD acquisition, bzip2 decode, canonical extraction, writer exact
verification, and deep RWS validation:

- ICON-EU realized seven surface variables and temperature, RH, U, V, and
  height at all 18 schema-requested native levels from 100-1000 hPa. Deep
  validation passed 12 variables, 18,396 chunks, and 161,516,424 payload bytes.
- ICON-D2 realized the same seven surface variables and five pressure-volume
  families at all 11 native levels from 200-1000 hPa. Deep validation passed
  12 variables, 17,965 chunks, and 62,222,500 payload bytes.

Both manifests persist `dwd-open-data` provenance. Server model and run
responses expose DWD ownership, `Source: Deutscher Wetterdienst`, CC BY 4.0,
and the RWS modification notice. Derived/heavy diagnostics remain disabled
until their complete DWD input contract is separately live-validated; the
native normalized surface and sounding lanes are scheduler-ready.

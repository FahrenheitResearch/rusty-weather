# DWD ICON regular-grid groundwork

This is an implementation audit, not a claim that either model is available
through the RWS scheduler yet. It pins the public object layout observed on
2026-08-14 and the decoder/acquisition work that is safe to reuse when the DWD
model IDs and component-bundle adapter are added.

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
future acquisition adapter therefore needs the shared component-bundle lane;
this branch deliberately does not duplicate it. The reusable transport layer
now recognizes `.bz2` or `BZh`, bounds expansion, and gives the cache decoded
GRIB bytes keyed by the original compressed URL.

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

One blocker remains deliberately unpromoted: DWD encodes forecast and
statistical durations in WMO unit 0 (minutes). `tot_prec` f001/f002 decoded
correctly and proved run-total windows of 0-60 and 0-120 minutes; ICON-D2 also
carried 75/90/105 and 135/150/165-minute messages in those hourly objects. The
current forecast selector only converts minute-valued instantaneous forecast
times, while its statistical-window helper accepts hours only. That shared
time-unit normalization must be fixed and regression-tested before DWD
accumulations are advertised. No scheduler or server capability should be
enabled before the DWD source/model IDs, component inventory, provenance, and
this time-window blocker are integrated and live-validated together.

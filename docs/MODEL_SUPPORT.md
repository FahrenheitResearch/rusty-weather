# Model capability matrix

Rusty Weather reports model capabilities independently. `Catalogued` means the
registry knows the model and its provider layout; it does not imply that every
cycle, member, variable, or derived product has passed live verification.

Status vocabulary:

- **Verified**: download through query/render has passed a real-data validation.
- **Ingest beta**: a fetch plan and focused contract tests exist; broader live
  product/cycle validation remains in progress.
- **Catalogued**: registry and URL/product knowledge exists, but automated
  ingest remains gated.
- **Local import**: user-provided data is normalized without a remote scheduler.

The machine-readable ingest capability also reports an independent typed
verification level: `live_verified`, `fixture_verified`,
`implemented_unverified`, or `unsupported`. A ready fetch plan is therefore
not automatically presented as live-verified.

| Model family | Acquisition | Current status | Important scope |
| --- | --- | --- | --- |
| HRRR CONUS | Remote | Verified | Deterministic surface and pressure products |
| HRRR Alaska | Remote | Ingest beta | Alaska `prs` + `sfc`; hourly f000-f018, with 00/06/12/18z extended through f048; official inventory fixture-verified |
| GFS | Remote | Verified | Global deterministic products |
| ECCC GDPS | Remote | Ingest beta | Global 0.15-degree deterministic 00/12z forecast; hourly f000-f084 then 3-hourly to f240; bounded per-field Datamart acquisition and a 19-level sounding profile are live ingest/store verified |
| CMA GRAPES GEPS | Remote | Ingest beta | Global 0.25-degree provider-produced ensemble statistics; 00/12z, 3-hourly f000-f078 then 6-hourly to f360; mean/spread, percentile, and probability fields only—no raw member or deterministic sounding claim; live ingest/store verified |
| ECCC RDPS | Remote | Ingest beta | North American 10 km rotated-grid deterministic forecast; 00/06/12/18z hourly f000-f084; bounded per-field acquisition and a 19-level sounding profile are live ingest/store verified; canonical U/V are paired and rotated to earth coordinates; derived/heavy disabled |
| ECCC HRDPS continental | Remote | Ingest beta | Pan-Canadian 2.5 km rotated-grid deterministic forecast; 00/06/12/18z hourly f000-f048; bounded per-field acquisition and a 19-level sounding profile are live ingest/store verified; canonical U/V are paired and rotated to earth coordinates; derived/heavy disabled |
| DWD ICON-EU regular | Remote | Ingest beta | European deterministic regular lat/lon feed; eight 3-hourly cycles, main-cycle f000-f120 and short-cycle f000-f048 native cadence; exact bzip2 component bundles and 18 schema-requested native pressure levels are live ingest/store verified; derived/heavy disabled |
| DWD ICON-D2 regular | Remote | Ingest beta | Germany deterministic regular lat/lon feed; eight 3-hourly cycles and hourly f000-f048; exact bzip2 component bundles and all 11 native pressure levels are live ingest/store verified; quarter-hour messages retain exact time semantics; derived/heavy disabled |
| Roshydromet ICON-Ru13/6N29 | Remote | Ingest beta | North Eurasia 697x213 regular lat/lon forecast; 00/12z, three-hourly f003-f072; exact WIS2 bulletin-component acquisition, sparse pressure profile, dateline-safe normalization, and bounded f003 live ingest/store verification |
| RRFS-A | Remote | Verified | Deterministic NA source cropped during ingest |
| RAP | Remote | Ingest beta | Grid-130 `awp130pgrb` (13 km); hourly f000-f021, with 03/09/15/21z extended through f051; live ingest/store verified |
| NAM | Remote | Ingest beta | Ingest is pinned to grid-212 `awip3d` (40 km), not the registry's separate `awip12` plotting product; hourly f000-f036 then 3-hourly through f084; live ingest/store verified |
| GDAS | Remote | Ingest beta | Global 0.25-degree `pgrb2.0p25`; f000-f009 each 00/06/12/18z cycle; official inventory fixture-verified |
| GEFS | Remote | Ingest beta | Control/statistical product route; not full member ingestion |
| NOAA AI-GFS | Remote | Ingest beta | Deterministic pressure and surface products |
| NOAA AI-GEFS | Remote | Ingest beta | Average/statistical products only; not full member ingestion; nonlinear derived/heavy diagnostics disabled |
| NOAA HGEFS | Remote | Ingest beta | Hybrid average/statistical products only; nonlinear derived/heavy diagnostics disabled |
| ECMWF IFS Open Data | Remote | Ingest beta | Open-data deterministic subset |
| NBM | Remote | Ingest beta | Surface-only deterministic core CONUS subset; native cadence varies with lead |
| REFS | Remote | Ingest beta | Preliminary CONUS weighted ensemble mean only; f001-f060; sparse pressure levels; derived/heavy disabled |
| ECMWF AIFS Single v2 | Remote | Ingest beta | ECMWF Open Data `oper` GRIB2 at 0.25 degrees; 00/06/12/18z, 6-hourly f000-f360; sparse published pressure levels; `q` is normalized to dewpoint; derived/heavy stages remain gated pending a verified static-orography join |
| HIRESW | Remote | Ingest beta | CONUS ARW 2.5 km surface/native product only; f000-f048; no pressure-volume claim |
| HREF | Remote | Ingest beta | CONUS weighted ensemble mean only; f001-f048; sparse pressure levels; derived/heavy disabled |
| SREF | Remote | Ingest beta | Grid-212 three-hourly weighted ensemble mean only; f000-f087; sparse pressure levels; derived/heavy disabled |
| RTMA | Remote | Ingest beta | CONUS f000 surface analysis only |
| URMA | Remote | Ingest beta | CONUS f000 surface analysis only |
| RRFS public prototype | Remote | Ingest beta | Preliminary 3 km CONUS deterministic pressure + 2-D feed; 00/06/12/18z hourly f000-f084, 03/09/15/21z hourly f000-f018 |
| WRF / compatible wrfout | User data | Local import | Exact and subhourly valid times supported; no remote scheduler/acquisition lane is advertised |

Stored-data discovery is independent of this table. Any valid rw-store model
directory can be queried even when its model slug is not built into the
registry.

For HREF, SREF, and REFS, the service deliberately ingests the published mean
state fields and their actual sparse pressure levels. It does not compute
non-linear diagnostics from those mean fields and relabel them as ensemble
means. Individual members, spread, probabilities, PMM/LPMM, and alternate
statistics remain catalogued separately rather than being silently substituted.

Native interval fields keep their physical identity in the store. NBM and
HREF deterministic/mean APCP are stored as `apcp_1h`; SREF's three-hourly mean
APCP is `apcp_3h`; and HIRESW's 2-5 km updraft-helicity message is stored as
`uh_2to5km_max_1h`. REFS publishes different APCP aggregation windows at
six-hour boundaries, so its presently selected raw field is conservatively
named `apcp_native_interval` and remains manual-only for temporal reduction
until an explicit fixed-window selector is implemented. This prevents a
native interval from being advertised as a cumulative run total.

The focused ingest fixtures pin inventories captured from the official
[HRRR](https://www.nco.ncep.noaa.gov/pmb/products/hrrr/),
[RAP](https://www.nco.ncep.noaa.gov/pmb/products/rap/),
[NAM](https://www.nco.ncep.noaa.gov/pmb/products/nam/),
[GFS/GDAS](https://www.nco.ncep.noaa.gov/pmb/products/gfs/),
[HIRESW](https://www.nco.ncep.noaa.gov/pmb/products/hiresw/),
[HREF](https://www.nco.ncep.noaa.gov/pmb/products/href/),
[SREF](https://www.nco.ncep.noaa.gov/pmb/products/sref/),
[REFS](https://www.nco.ncep.noaa.gov/pmb/products/refs/), and
[RRFS](https://www.nco.ncep.noaa.gov/pmb/products/rrfs/), and
[ECMWF Open Data](https://www.ecmwf.int/en/forecasts/datasets/open-data),
[ECCC RDPS](https://eccc-msc.github.io/open-data/msc-data/nwp_rdps/readme_rdps-datamart_en/), and
[ECCC HRDPS](https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/readme_hrdps-datamart_en/) feeds.
Fixture source URLs and SHA-256 values are recorded beside the fixtures.

GDPS was additionally exercised against the official
[ECCC MSC Datamart](https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-datamart_en/)
on 2026-08-14. A bounded f000 sounding ingest assembled the selected
per-field objects, realized seven surface variables plus temperature, relative
humidity, u, v, and height at all 19 requested levels from 100-1000 hPa, passed
the writer's exact verification, and passed deep validation at 12 variables,
57,350 chunks, and 477,280,099 payload bytes. The required ECCC attribution is
persisted and exposed by the server; direct legacy whole-file plotting remains
disabled rather than silently using the one-field availability probe.

CMA GRAPES GEPS was exercised against the official WIS2 core-data source on
2026-08-14 using the provider's 2026-08-13 00z f024 object. The bounded
single-lead ingest realized all 57 scientifically identified provider
statistics bit-exactly: ensemble mean/spread fields, five published
percentiles across seven field families, seven wind/gust probabilities, and
eleven precipitation percentile/probability fields. Deep validation passed at
57 variables, 1,026 chunks, and 134,659,235 payload bytes. Unknown local CMA
parameters remain excluded, and the API reports `provider_statistics_only`
rather than implying access to the 31 underlying member forecasts.
RDPS and HRDPS were exercised against the same official Datamart on
2026-08-14 with one bounded 00z f024 sounding ingest each. RDPS realized six
surface variables (the live feed has no surface-orography object) plus five
19-level pressure volumes, passed writer verification, and passed deep store
validation at 11 variables, 23,910 chunks, and 219,953,866 payload bytes.
HRDPS realized all seven sounding surface variables plus the same five
19-level pressure volumes, then passed both gates at 12 variables, 64,815
chunks, and 580,038,302 payload bytes. The fixtures preserve the current RDPS
documentation/payload dimension drift: documentation says 1102x1076, while
the live U/V GRIB objects decode to 1140x1045; live normalized coordinates are
authoritative. HRDPS documentation and payload both report 2540x1290.

Both feeds declare U/V relative to their rotated grid, not geographic east and
north. The regional decoder therefore requires each matching U/V pair, checks
GRIB template 3.1 and its grid-relative component flag, derives the grid-i
tangent from the normalized live coordinates, and rotates both components
before canonical publication. Missing pairs, mismatched grids, or metadata
drift fail closed. Independent comparison with ECCC's separately published
speed/direction objects covered 2,382,600 RDPS and 6,553,200 HRDPS components;
RMS differences were 0.063552 and 0.006831 m/s respectively, within provider
direction quantization. Derived/heavy diagnostics remain disabled until every
diagnostic path is explicitly proven to consume the normalized vectors.
ICON-EU and ICON-D2 regular-grid lanes were exercised against official
[DWD Open Data](https://opendata.dwd.de/weather/nwp/) on 2026-08-14. Bounded
00z f000 sounding ingests passed exact writer verification and deep store
validation with seven 2-D variables plus temperature, RH, U, V, and height at
18 ICON-EU and 11 ICON-D2 levels. DWD ownership, CC BY 4.0 attribution, and the
normalization notice persist through manifests and server responses. Exact
minute-unit regression coverage prevents ICON-D2's 75/90/105 and
135/150/165-minute messages from being rounded onto hourly samples. Live
f001-f002 surface ingests additionally verified all nine available direct
fields, including exact run-total precipitation and cycle-static orography,
without fetching or claiming a pressure bundle.
ICON-Ru13/6N29 was exercised against Roshydromet's official WIS2 source on
2026-08-14. A bounded f003 sounding ingest assembled 24 pressure and ten
surface bulletin objects, validated their exact WMO wrappers, normalized the
697x213 dateline-crossing grid, passed writer verification, and passed deep
RWS validation at ten variables and 3,095 chunks. The lane retains its native
five-level pressure coordinate (four levels for relative humidity), disables
derived/heavy products, and exposes Roshydromet/WMO-core attribution through
the server. Exact inventory, object hashes, and live evidence are recorded in
[ROSHYDROMET_ICON_RU.md](ROSHYDROMET_ICON_RU.md).

For the NOAA deterministic wave, normalization follows the fields actually
published. HRRR Alaska carries native pressure-level temperature, dewpoint,
RH, winds, and height every 25 hPa from 50-1000 hPa. RAP carries the same
100-1000 hPa temperature/RH/wind/height grid but no native isobaric dewpoint,
so the store retains `rh_iso` rather than inventing `dewpoint_iso`. NAM's
`awip3d` carries temperature/RH/wind/height every 25 hPa from 50-1000 hPa,
while dewpoint is native only at 300/400/500/700/850/1000 hPa and 2 m moisture
is RH rather than dewpoint. GDAS likewise publishes pressure-level RH rather
than pressure-level dewpoint. Manifests and query capability report realized
variables and levels per run, so these distinctions remain visible.

## Product availability

Variable availability is evaluated per run and valid time from the store
manifest. The API never claims that every variable exists for every model.
Each variable response reports native units and field kind, actual availability
axis, temporal semantics and confidence, supported reducers, pressure levels,
and supported query shapes. Resolved provider provenance and required provider
attribution are reported once at the run level and are inherited by query
responses through their embedded run descriptor; they are not claimed as
per-variable lineage.

This document is a human-readable summary. The server's `/v1/models` response
is the machine-readable authority for a running deployment.

Potential additional official feeds and the core contracts that gate them are
tracked separately in [PUBLIC_MODEL_BACKLOG.md](PUBLIC_MODEL_BACKLOG.md).

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
| HRRR Alaska | Remote | Ingest beta | Regional deterministic products |
| GFS | Remote | Verified | Global deterministic products |
| RRFS-A | Remote | Verified | Deterministic NA source cropped during ingest |
| RAP | Remote | Ingest beta | North American deterministic products |
| NAM | Remote | Ingest beta | Regional deterministic products |
| GDAS | Remote | Ingest beta | Global analysis/short forecast |
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
[HIRESW](https://www.nco.ncep.noaa.gov/pmb/products/hiresw/),
[HREF](https://www.nco.ncep.noaa.gov/pmb/products/href/),
[SREF](https://www.nco.ncep.noaa.gov/pmb/products/sref/),
[REFS](https://www.nco.ncep.noaa.gov/pmb/products/refs/), and
[RRFS](https://www.nco.ncep.noaa.gov/pmb/products/rrfs/), and
[ECMWF Open Data](https://www.ecmwf.int/en/forecasts/datasets/open-data) feeds.
Fixture source URLs and SHA-256 values are recorded beside the fixtures.

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

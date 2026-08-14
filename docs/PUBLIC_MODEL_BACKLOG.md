# Public numerical-model implementation backlog

Research snapshot: 2026-08-14. This is a discovery and implementation-planning
document, not a claim that the feeds below are supported. Current support is
reported only by [MODEL_SUPPORT.md](MODEL_SUPPORT.md) and the running service's
`/v1/models` response.

This first pass contains 32 deduplicated atmospheric model/domain lanes that
are not represented as working remote lanes in the current capability matrix.
A lane is counted once even when it has both published statistics and raw
members, or several delivery choices. Different resolutions of the same model
are counted separately only when the provider treats them as different grids
with materially different domains or schedules.

## Non-negotiable data contracts

### Producer is not transport

Every run must retain two independent identities:

- **producer/licensor**: the organization that owns or publishes the model data
  and whose attribution and licence apply;
- **transport**: the host or mirror from which bytes were acquired.

For example, a MeteoSwiss object may arrive from CSCS, a Météo-France object
from data.gouv.fr or OVH, and ECMWF data from AWS. CSCS, data.gouv.fr, OVH, and
AWS are transports, not the data producers. `SourceId` and
`RwsSourceProvenance.provider` currently do not express this distinction
reliably. Add stable producer identity, licence identity/URL, required
attribution text, transport identity, and optional mirror identity before
shipping a new provider. Never derive ownership from a hostname.

### Do not flatten ensemble members

`FieldProduct` can identify published means, standard deviations, extrema,
percentiles, and probabilities, but the store/run identity has no explicit
ensemble-member coordinate. Published statistical fields can be added now.
Raw-member feeds require a durable member identifier and member/control
semantics across store, manifest, query, reductions, and API responses. Do not
store an arbitrary member as the deterministic default and do not calculate a
nonlinear diagnostic from a mean state and label it an ensemble mean.

### Do not disguise triangular meshes as rectangular grids

The current `LatLonGrid` and spatial locator describe an `nx` by `ny`
structured grid. Native ICON icosahedral products and the ICON-CH feeds need an
unstructured triangular topology (node coordinates, connectivity, stable grid
identity, and a spatial locator), unless the provider publishes a structured
version. A local regrid may be offered later as an explicitly derived product;
it must not be presented as lossless native ingestion.

### Bound acquisition before decoding

An implementation must discover content length and inventory before fetching
payloads, cap every response and decompression, and use provider indexes,
server-side filters, or one-field objects where available. The size figures
below are observed or provider-documented budgeting examples, not contracts.
Record measured object and selected-range sizes in the fixture manifest.

## Ranked implementation queue

The order favors operational, anonymous, structured GRIB2 feeds that preserve
their native semantics with existing RWS concepts. `P1` is adapter work on an
existing contract, `P2` needs a new bounded acquisition/format adapter, and
`P3` is gated by a core storage or topology extension.

| Rank | Lane | Class | Native schedule and domain | Public data and first implementation slice |
| ---: | --- | --- | --- | --- |
| 1 | ECCC GDPS 15 km | P1 | Global 0.15-degree regular lat/lon, 00/12Z; hourly f000-f084, then 3-hourly to f240 | Anonymous per-variable/level/lead GRIB2. Start surface plus the documented pressure-level sounding set. |
| 2 | ECCC RDPS 10 km | P1 | Canada and adjacent US, rotated lat/lon, 00/06/12/18Z; hourly f000-f084 | Anonymous per-field GRIB2. Preserve the rotated grid and native interval metadata. |
| 3 | ECCC HRDPS continental 2.5 km | P1 | 2540x1290 rotated lat/lon, 00/06/12/18Z; hourly f000-f048 | Anonymous per-field GRIB2; surface plus up to 31 published pressure levels. |
| 4 | DWD ICON-EU regular-grid output | P1 | Europe, 0.0625-degree regular lat/lon; 00/06/12/18Z to f120 and 03/09/15/21Z to f30 | Anonymous per-variable/level/lead bzip2-compressed GRIB2. Use the provider's regular-grid objects. |
| 5 | DWD ICON-D2 regular-grid output | P1 | Germany and neighbors, about 2.2 km; eight cycles/day, hourly f000-f048 | The directory publishes both native icosahedral and regular-lat-lon objects. Select only filenames explicitly marked `regular-lat-lon`. |
| 6 | ECCC GEPS published products | P1 | Global 0.5-degree regular lat/lon, 00/12Z; 3-hourly to f192 then 6-hourly to f384; extended to f936 Monday/Thursday 00Z | Ingest provider-produced mean, standard deviation, percentile, and probability messages first. Raw 20 perturbed members plus control are P3. |
| 7 | ECMWF IFS ENS open subset | P1/P3 | Global 0.25 degree, four cycles/day; 00/12Z to f360 and 06/18Z to f144 | Reuse the existing ECMWF URL/index/range machinery for published `em`, `es`, and `ep` first. Control plus 50 perturbed members need the member contract. |
| 8 | ECMWF AIFS ENS v2 open subset | P1/P3 | Global 0.25 degree, four cycles/day; medium-range ensemble | Same staged approach as IFS ENS: indexed published statistics first, then control plus 50 members after the member contract. Keep it distinct from the supported AIFS Single v2 lane. |
| 9 | Météo-France ARPEGE 0.25 | P2 | Global regular 0.25 degree, 00/06/12/18Z, f000-f102; hourly for some fields and otherwise 3-hourly by lead | GRIB2 packages contain surface, height, and 34-level isobaric groups. Use the account-backed targeted API for the sparse RWS profile; do not fetch multi-gigabyte package groups by default. |
| 10 | Météo-France AROME France 0.025 | P2 | 12W-16E, 37.5N-55.4N regular grid; four cycles/day, hourly f000-f051 | Targeted API or carefully selected package groups. Initial surface and pressure profile; retain provider precipitation-window semantics. |
| 11 | Météo-France AROME France 0.01 | P2 | Same France/near-Europe domain at 0.01 degree; four cycles/day, hourly f000-f051 | Hourly GRIB2 objects are roughly 65-73 MB each in current catalogue observations. Enforce a forecast-window and field budget. |
| 12 | Météo-France ARPEGE 0.1 EURAT | P2 | Europe/Atlantic regular 0.1 degree (32W-42E, 20N-72N), four cycles/day to f102 | Richer regional ARPEGE output, but package objects are hundreds of MB. Prefer targeted API extraction. |
| 13 | Météo-France AROME Antilles 0.025 | P2 | Caribbean regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 25-31 MB. |
| 14 | Météo-France AROME Guyane 0.025 | P2 | French Guiana regional regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 9-11 MB. |
| 15 | Météo-France AROME Réunion-Mayotte 0.025 | P2 | Indian Ocean regional regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 80-87 MB. |
| 16 | Météo-France AROME Polynésie 0.025 | P2 | 157.5W-144.5W, 25.25S-12.6S regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 14-17 MB. |
| 17 | Météo-France AROME Nouvelle-Calédonie 0.025 | P2 | 156E-174E, 30S-10S regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 16-18 MB. |
| 18 | DMI HARMONIE NEA | P2 | North European/Atlantic rotated lat/lon at 2.5 km; deterministic output collected every 3 hours, f000-f054 | GRIB2 surface, 65 model levels, and documented pressure levels. Use EDR parameter/time filtering; rotate grid-relative winds correctly. |
| 19 | DMI HARMONIE Greenland/Iceland | P2 | Greenland/Iceland regional grid, operational rolling forecast | Use the `harmonie_ig_*` EDR collections and keep it distinct from NEA. Surface, pressure, and model-level collections are separate. |
| 20 | MET Norway MEPS | P2 | Nordic 2.5 km, updated hourly, f000-f060 | CF-NetCDF through THREDDS/OPeNDAP. Add a bounded generic CF/OPeNDAP adapter and request only selected variables/times. |
| 21 | MET Norway AROME-Arctic | P2 | Arctic 2.5 km, 00/06/12/18Z, f000-f060 | CF-NetCDF through THREDDS/OPeNDAP; separate grid and field inventory from MEPS. |
| 22 | NOAA CFSv2 operational 9-month forecast | P2/P3 | Global; 00/06/12/18Z, four members per cycle, 6-hourly output to about nine months | Indexed GRIB2 products: `flxf` surface, `pgbf` 0.5-degree pressure, `ipvf` 1-degree isentropic, plus ocean products. Build the indexed adapter now; enable fields only after member identity is durable. |
| 23 | Météo-France PEARP global ensemble | P2/P3 | Global regular 0.25 degree, 00/06/12/18Z; control plus 34 perturbed members | Current lead objects are about 2.2-4.1 GB and have no companion record index. Use the ensemble API or a bounded remote-scan/index plan; never default to whole-object download. |
| 24 | DWD ICON global | P3 | Global native icosahedral mesh, about 13 km; 00/06/12/18Z, to f180 for 00/12Z and f120 for 06/18Z | Per-variable/level/lead bzip2 GRIB2, but no provider-published structured global grid. Requires native triangular topology. A current T2M object is about 3 MB. |
| 25 | DWD ICON-EPS global | P3 | Global native icosahedral ensemble, 00/06/12/18Z | Files carry all members for a field/lead. Requires both triangular topology and member identity. |
| 26 | DWD ICON-EU-EPS | P3 | European native icosahedral ensemble | Requires both triangular topology and member identity; do not substitute the deterministic regular-grid product. |
| 27 | DWD ICON-D2-EPS | P3 | Germany regional 2.2 km, 20 members, f000-f048 | Requires both triangular topology and member identity. |
| 28 | MeteoSwiss ICON-CH1-EPS | P3 | Switzerland 1 km triangular mesh, 11 members, eight cycles/day, f000-f033 (03Z extends to f045), 80 layers | STAC objects are split by parameter, step, and member. Requires triangular topology and member identity. |
| 29 | MeteoSwiss ICON-CH2-EPS | P3 | Switzerland 2.1 km triangular mesh, 21 members, four cycles/day, f000-f120, 80 layers | Same core gates as CH1. Treat CSCS object storage as transport, not producer. |
| 30 | KNMI HARMONIE Netherlands | P3 | Netherlands, about 2 km regular lat/lon, hourly output to f060 | API-key access, tar-packaged GRIB1. Deterministic and rolling-ensemble products are separate datasets. Needs a GRIB1 gate or explicit external normalization. |
| 31 | KNMI HARMONIE Europe | P3 | Europe 5.5 km rotated lat/lon, hourly output to f060 | GRIB1 tar packages. Rolling ensemble is 30 members delivered as six hourly batches of five; preserve each member's reference time and accumulation reset. |
| 32 | KNMI HARMONIE Caribbean | P3 | Caribbean 0.05-degree regular grid, hourly output to f060 | GRIB1 tar packages and API-key access; implement after the GRIB1 decision. |

## Authoritative access contracts

### Environment and Climate Change Canada (ECCC/MSC/CMC)

Producer/licensor: **Environment and Climate Change Canada**, with model output
produced by the Meteorological Service of Canada/Canadian Meteorological
Centre. Transport: `dd.weather.gc.ca`; `hpfx.collab.science.gc.ca` is an
alternate delivery host, not a different producer.

- Catalogue and model documentation:
  <https://eccc-msc.github.io/open-data/msc-data/readme_en/>
- GDPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-datamart_en/>
  and `https://dd.weather.gc.ca/today/model_gdps/15km/{HH}/{hhh}/`
- RDPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_rdps/readme_rdps-datamart_en/>
  and `https://dd.weather.gc.ca/today/model_rdps/10km/{HH}/{hhh}/`
- HRDPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_hrdps/readme_hrdps-datamart_en/>
  and `https://dd.weather.gc.ca/today/model_hrdps/continental/2.5km/{HH}/{hhh}/`
- GEPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_geps/readme_geps-datamart_en/>
  and `https://dd.weather.gc.ca/today/ensemble/geps/grib2/{raw|products}/{HH}/{hhh}/`

Authentication is not required. Dated DataMart roots retain roughly 30 days;
`/today` is the current-day view. No companion record indexes are published,
but files are already split by variable, level, and lead (GEPS groups all
members or all products for that field). Whole-object GET is therefore the
normal path. Enumerate a bounded directory, HEAD selected objects, and sum
their lengths before acquisition. AMQP notification is the preferred follow
mechanism after the initial HTTP adapter.

### Deutscher Wetterdienst (DWD)

Producer/licensor and direct transport operator: **Deutscher Wetterdienst**.

- Product description: <https://www.dwd.de/EN/ourservices/nwp_forecast_data/nwp_forecast_data.html>
- ICON global: `https://opendata.dwd.de/weather/nwp/icon/grib/{HH}/{variable}/`
- ICON-EU: `https://opendata.dwd.de/weather/nwp/icon-eu/grib/{HH}/{variable}/`
- ICON-D2: `https://opendata.dwd.de/weather/nwp/icon-d2/grib/{HH}/{variable}/`
- Ensemble roots: <https://opendata.dwd.de/weather/nwp/icon-eps/grib/>,
  <https://opendata.dwd.de/weather/nwp/icon-eu-eps/grib/>, and
  <https://opendata.dwd.de/weather/nwp/icon-d2-eps/grib/>

Authentication is not required. Objects are `.grib2.bz2`, normally one
variable/level/lead, with no companion record index. Range retrieval does not
avoid decompressing the bzip2 member, so fetch only selected small objects with
strict compressed and decompressed caps. The directory is a rolling current
forecast service, not an archive contract.

### ECMWF

Producer/licensor: **European Centre for Medium-Range Weather Forecasts
(ECMWF)**. Direct transport is `data.ecmwf.int`; AWS, Azure, and Google Cloud
are mirrors and remain transport identities.

- Open-data contract and inventory:
  <https://www.ecmwf.int/en/forecasts/datasets/open-data>
- IFS ENS root:
  `https://data.ecmwf.int/forecasts/{YYYYMMDD}/{HH}z/ifs/0p25/enfo/`
- AIFS ENS root:
  `https://data.ecmwf.int/forecasts/{YYYYMMDD}/{HH}z/aifs-ens/0p25/enfo/`
- Official client/reference implementation:
  <https://github.com/ecmwf/ecmwf-opendata>

Authentication is not required. GRIB2 objects have `.index` companions, so
reuse bounded index parsing and HTTP range selection. The open portal retains
the most recent 12 runs (about two to three days). The open inventory includes
surface fields and pressure levels 1000, 925, 850, 700, 600, 500, 400, 300,
250, 200, 150, 100, 50, and 10 hPa. Capture exact type/number semantics for
`cf`, `pf`, `em`, `es`, and `ep`; do not infer them from filename position.

### Météo-France

Producer/licensor: **Météo-France**. The catalogue is operated by
`data.gouv.fr`; deterministic resource objects may be delivered by
`object.data.gouv.fr`, and PEARP resources by an OVH object store. Those hosts
are transports, not producers.

- Package API documentation:
  <https://confluence-meteofrance.atlassian.net/wiki/spaces/OpenDataMeteoFrance/pages/854818890>
- AROME package discovery begins at
  `https://public-api.meteofrance.fr/previnum/DPPaquetAROME/models/AROME/grids`
- Example bounded AROME package request:
  `https://public-api.meteofrance.fr/previnum/DPPaquetAROME/models/AROME/grids/0.025/packages/SP1/productARO?referencetime={ISO8601}&time=00H06H&format=grib2`
- Targeted ARPEGE service: <https://www.data.gouv.fr/dataservices/api-modele-arpege>
- Deterministic catalogue datasets:
  [ARPEGE 0.25](https://www.data.gouv.fr/datasets/paquets-arpege-resolution-0-25deg),
  [ARPEGE 0.1](https://www.data.gouv.fr/datasets/paquets-arpege-resolution-0-1deg),
  [AROME 0.025](https://www.data.gouv.fr/datasets/paquets-arome-resolution-0-025deg), and
  [AROME 0.01](https://www.data.gouv.fr/datasets/paquets-arome-resolution-0-01deg)
- Overseas datasets:
  [Antilles](https://www.data.gouv.fr/datasets/paquets-arome-outre-mer-antilles-resolution-0-025deg),
  [Guyane](https://www.data.gouv.fr/datasets/paquets-arome-outre-mer-guyane-resolution-0-025deg-1),
  [Réunion-Mayotte](https://www.data.gouv.fr/datasets/paquets-arome-outre-mer-reunion-mayotte-resolution-0-025deg),
  [Polynésie](https://www.data.gouv.fr/datasets/paquets-arome-outre-mer-polynesie-resolution-0-025deg), and
  [Nouvelle-Calédonie](https://www.data.gouv.fr/datasets/paquets-arome-outre-mer-nouvelle-caledonie-resolution-0-025deg)
- PEARP metadata/API discovery:
  `https://www.data.gouv.fr/api/1/datasets/pe-arpege-glob025/`

Anonymous data.gouv package downloads are open, but the targeted model API
requires an account and is limited to 50 requests/minute. Package API data is
real-time with about three days of retention; the targeted PNT service reports
14 days. There are no GRIB record indexes beside the large package objects.
An implementation must discover resources through the API/catalogue, not pin
ephemeral object URLs. Full package sizes range from tens of MB for overseas
domains to hundreds of MB or several GB for ARPEGE/AROME/PEARP.

### Danish Meteorological Institute (DMI)

Producer/licensor: **Danish Meteorological Institute**. API transport is
`opendataapi.dmi.dk`; STAC asset bytes may come from DMI's AWS object storage.

- HARMONIE specification:
  <https://www.dmi.dk/friedata/dokumentation/data/forecast-data-weather-model-harmonie>
- EDR filtering:
  <https://www.dmi.dk/friedata/dokumentation/forecast-data-edr-api>
- EDR template:
  `https://opendataapi.dmi.dk/v1/forecastedr/collections/{collection}/grib?parameter-name={parameters}&datetime={interval}`
- STAC discovery:
  `https://opendataapi.dmi.dk/v1/forecastdata/collections/{collection}/items`

Current collection families are `harmonie_dini_{sf|pl|ml}` and
`harmonie_ig_{sf|pl|ml}`, plus published DINI ensemble means, percentiles, and
probabilities. Authentication is not currently required. Forecast data has
about 48 hours of retention. Whole STAC lead objects observed in the hundreds
of MB, with no provider record index; use EDR's parameter/time filtering. The
documented pressure levels are 1000, 950, 925, 900, 850, 800, 700, 600, 500,
400, 300, 250, 200, 150, 100, and 50 hPa.

### MET Norway

Producer/licensor: **Norwegian Meteorological Institute (MET Norway)**.
Transport is MET Norway THREDDS/OPeNDAP.

- Access documentation: <https://api.met.no/product/THREDDS>
- MEPS latest deterministic endpoint:
  `https://thredds.met.no/thredds/dodsC/meps25files/meps_det_pp_2_5km_latest.nc`
- AROME-Arctic latest endpoint:
  `https://thredds.met.no/thredds/dodsC/aromearcticlatest/arome_arctic_pp_2_5km_latest.nc`

Authentication is not required. Use OPeNDAP projections to request only the
selected CF variables, vertical coordinates, and times; cap decoded array
dimensions before allocation. THREDDS also exposes historical forecast
catalogues. Do not confuse MET Norway's own regional grids with third-party
global model point forecasts exposed through other MET APIs.

### NOAA/NCEP CFSv2

Producer/licensor: **NOAA National Centers for Environmental Prediction**.
Real-time transport is NCEP NOMADS; long-term archive transport is NOAA NCEI.

- Official overview:
  <https://www.ncei.noaa.gov/products/weather-climate-models/climate-forecast-system>
- Real-time products: <https://www.nco.ncep.noaa.gov/pmb/products/cfs/>
- Real-time template:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/cfs/prod/cfs.{YYYYMMDD}/{HH}/6hrly_grib_{01|02|03|04}/`
- NCEI archive:
  <https://www.ncei.noaa.gov/data/climate-forecast-system/access/operational-9-month-forecast/>

Authentication is not required. Each GRIB2 object has an `.idx` companion and
supports range selection. Representative current object sizes are about 4.2
MB for one `flxf` valid time, 23.9 MB for `pgbf`, and 5.2 MB for `ipvf`, before
multiplying by four members and the long 6-hourly horizon. The live NOMADS root
holds roughly a week; NCEI provides the durable archive. Ocean output should be
a later, separate canonical-domain effort.

### MeteoSwiss

Producer/licensor: **Federal Office of Meteorology and Climatology
MeteoSwiss**. Catalogue transport is `data.geo.admin.ch`; signed object bytes
are currently served by CSCS. CSCS is not the model owner.

- ICON-CH1 STAC collection:
  <https://data.geo.admin.ch/api/stac/v1/collections/ch.meteoschweiz.ogd-forecasting-icon-ch1>
- ICON-CH2 STAC collection:
  <https://data.geo.admin.ch/api/stac/v1/collections/ch.meteoschweiz.ogd-forecasting-icon-ch2>
- Model description:
  <https://www.meteoswiss.admin.ch/weather/warning-and-forecasting-systems/icon-forecasting-systems.html>

Authentication is not required. Collection assets include the parameter map
and static horizontal/vertical constants; retain those with the fixture.
Forecast assets expire after roughly 24 hours. Signed object URLs support byte
ranges, but assets are already separated by parameter, lead, and member; one
observed control-field object was about 2.3 MB. Both grids are triangular and
therefore remain gated on topology support.

### Royal Netherlands Meteorological Institute (KNMI)

Producer/licensor: **Royal Netherlands Meteorological Institute (KNMI)**.
Transport is the KNMI Data Platform API.

- Official overview: <https://english.knmidata.nl/open-data/harmonie>
- API template:
  `https://api.dataplatform.knmi.nl/open-data/v1/datasets/{dataset}/versions/1.0/files`
- Deterministic Netherlands dataset example: `harmonie_arome_cy43_p1`

An API key is required. The current cycle-43 products are tar-packaged GRIB1,
not GRIB2, and do not include a GRIB record index. Most rolling products retain
about 72 hours. The Netherlands deterministic archive began in 2026. Ensemble
delivery is a rolling lagged ensemble, so member reference times and
accumulation resets are part of the data model, not incidental metadata.

## Licence and attribution matrix

Open download and permission to redistribute are separate checks. An adapter
is not releasable until both its transport test and the applicable licence test
are recorded.

| Stable producer identity | Redistribution/commercial status | Required attribution record | Official terms |
| --- | --- | --- | --- |
| `eccc` | Worldwide, royalty-free copy, modify, publish, adapt, distribute, including commercial use, under ECCC's server licence | `Data Source: Environment and Climate Change Canada`; preserve any named third-party origin | <https://eccc-msc.github.io/open-data/licence/readme_en/> |
| `dwd` | DWD open data is reusable under CC BY 4.0 / applicable German geodata terms | `Deutscher Wetterdienst (DWD)`, licence link, and modification notice | <https://www.dwd.de/DE/leistungen/opendata/faqs_opendata.html> |
| `ecmwf` | Open subset may be redistributed and used commercially under CC BY 4.0 plus ECMWF terms | ECMWF attribution required by the terms; preserve licence and modification information | <https://www.ecmwf.int/en/forecasts/datasets/open-data> |
| `meteo-france` | Licence Ouverte / Open Licence 2.0 permits reuse, including commercial reuse | `Météo-France`, source link, and indication of modifications | <https://www.data.gouv.fr/datasets/paquets-arpege-resolution-0-25deg> |
| `dmi` | CC BY 4.0 permits sharing and adaptation for any purpose, including commercial | Danish Meteorological Institute/DMI, licence link, and change indication | <https://www.dmi.dk/friedata/dokumentation/terms-of-use> |
| `met-norway` | NLOD 2.0 and CC BY 4.0 are offered for MET Norway data | Credit MET Norway and link the selected licence | <https://docs.api.met.no/doc/License.html> |
| `noaa-ncep` | US federal NOAA data are generally public-domain; retain dataset-specific notices | NOAA/NCEP as producer and NOMADS or NCEI only as transport | <https://www.noaa.gov/disclaimer> |
| `meteoswiss` | STAC collections declare CC BY | Federal Office of Meteorology and Climatology MeteoSwiss, licence link, and change indication | The `license` and provider fields in the [ICON-CH1 collection](https://data.geo.admin.ch/api/stac/v1/collections/ch.meteoschweiz.ogd-forecasting-icon-ch1) |
| `knmi` | HARMONIE open data is CC BY 4.0 | KNMI, licence link, and change indication | <https://english.knmidata.nl/open-data/harmonie> |

## Adapter and fixture acceptance gates

Every lane must land with a provider adapter, canonical mapping, bounded
fixtures, and explicit unsupported semantics. A catalogue entry alone is not
an ingest claim.

1. **Discovery fixture.** Save a bounded official listing, STAC item, THREDDS
   catalogue, API response, or index for one real cycle. Record retrieval time,
   authoritative URL, producer, transport, content length, ETag/last-modified,
   and SHA-256. Strip credentials and expiring query signatures.
2. **Geometry fixture.** Include the smallest legal field that proves scan
   order, longitude convention, projection/rotation, dimensions, and grid
   hash. Assert corner and interior coordinates. DWD regular-grid selection
   must reject the same cycle's icosahedral filename.
3. **Inventory fixture.** Pin the upstream variable/level/product inventory and
   classify every selected field by native units, vertical coordinate,
   instantaneous/interval/accumulation semantics, and ensemble statistic or
   member. Unknown messages stay unknown; they are never guessed into a
   canonical field.
4. **Payload fixture.** Prefer an official index plus a few byte ranges. For
   ECCC and DWD, retain one small complete per-field object and enforce both
   compressed and decompressed limits. For OPeNDAP, retain a tiny selected
   response rather than a whole model file.
5. **Numerical golden.** Compare decoded grid values, missing values, scale,
   offset, and scan order to an independent reference decoder. Include at
   least one surface scalar, vector wind, pressure-level field, and accumulated
   field where the feed provides them.
6. **Cadence contract.** Test allowed cycles, native lead transitions, late or
   missing publications, rolling retention, and latest-cycle fallback. Do not
   advertise a cycle until the required role set is complete.
7. **Acquisition budget.** HEAD or otherwise obtain every selected object's
   length, sum the plan, and reject it before transfer if object, range,
   response, decompression, field-count, grid-cell, or cycle budgets are
   exceeded.
8. **Provenance/licence contract.** Assert producer/licensor independently from
   direct host or mirror, persist exact required attribution, and expose it at
   run/API level. A mirror switch must not change the producer identity.
9. **Round-trip gate.** Ingest the fixture through rw-store, validate the store,
   query points/areas/soundings, and verify capability reporting remains
   `fixture_verified` until a bounded live end-to-end test passes.

Provider-specific minimum fixtures:

- ECCC: GDPS/RDPS/HRDPS directory listing plus surface, isobaric, wind, and
  precipitation objects; GEPS `products` file proving each statistic's GRIB
  metadata, followed later by one `allmbrs` file.
- DWD: `.bz2` surface and pressure objects, decompression ceiling, grid-template
  assertion, and deterministic rejection of native-mesh files in structured
  lanes.
- ECMWF: `.index` plus selected ranges for `em`, `es`, `ep`, `cf`, and one `pf`;
  assert ensemble number/control metadata rather than filename-only routing.
- Météo-France: package/API discovery JSON, one small targeted GRIB2 response,
  representative package metadata without downloading a multi-GB object, and
  a precipitation-window fixture.
- DMI: EDR collection/parameter discovery, a selected GRIB2 response, rotated
  grid coordinates, and a vector-wind rotation golden.
- MET Norway: THREDDS catalogue, NetCDF header/CF coordinate metadata, and a
  tiny OPeNDAP slice containing one surface and one vertical field.
- CFSv2: `.idx` and selected message ranges from `flxf`, `pgbf`, and `ipvf`,
  with member and six-hour valid-time assertions.
- MeteoSwiss: STAC collection and item, parameter CSV, static geometry asset,
  and one bounded signed-asset range with the signature removed from fixtures.
- KNMI: API manifest, safe tar inventory, and the smallest legal GRIB1 sample;
  no support claim until edition-1 decode and rolling-member timing pass.

## Deliberately deferred adjacent feeds

The same official catalogues expose ECCC CAPS/REPS/CanSIPS/NAEFS, ECCC analysis
and air-quality systems, and multiple ocean, ice, wave, and surge products.
They are valuable, but they should be researched as separate canonical-domain
lanes rather than being counted as atmospheric model support. This prevents a
large catalogue number from hiding missing semantics in the store and API.

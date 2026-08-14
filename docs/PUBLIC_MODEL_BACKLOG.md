# Public numerical-model implementation backlog

Research snapshot: 2026-08-14. This is a discovery and implementation-planning
document, not a claim that the feeds below are supported. Current support is
reported only by [MODEL_SUPPORT.md](MODEL_SUPPORT.md) and the running service's
`/v1/models` response.

This inventory contains 45 deduplicated atmospheric model/domain lanes that
are not represented as working remote lanes in the current capability matrix.
A lane is counted once even when it has both published statistics and raw
members, or several delivery choices. Different resolutions of the same model
are counted separately only when the provider treats them as different grids
with materially different domains or schedules.

## Non-negotiable data contracts

### Producer, licensing publisher, and transport are independent

Every run must retain three independent identities:

- **producer**: the organization or consortium responsible for the model output;
- **licensing publisher**: the organization whose terms and attribution apply;
- **transport**: the host or mirror from which bytes were acquired.

For example, a MeteoSwiss object may arrive from CSCS, a Météo-France object
from data.gouv.fr or OVH, and ECMWF data from AWS. CSCS, data.gouv.fr, OVH, and
AWS are transports, not the data producers. The Nordic MEPS feed makes the
third identity necessary: MetCoOp produces the model, FMI or SMHI publishes it
under that distributor's terms, and FMI Open Data or SMHI object storage moves
the bytes. `SourceId` and `RwsSourceProvenance.provider` currently do not
express these distinctions reliably. Add stable producer identity, licensing
publisher identity/URL, required attribution text, transport identity, and
optional mirror identity before shipping a new provider. Never derive
ownership or licensing from a hostname.

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
| 6 | CMA-GEPS published ensemble products | P1/P2 | Global 0.25-degree regular lat/lon, 00/12Z; 3-hourly to f078 then 6-hourly to f360 | Anonymous WIS2-core GRIB2. Current files expose probability, percentile, and ensemble-derived fields for 31 forecasts, not raw members. Add bounded GRIB inventory scanning because no sidecar index is published. |
| 7 | Roshydromet ICON-Ru13/6N29 | P1 | North Eurasia, 697x213 regular lat/lon at 0.25 degree, 00/12Z to f072 | Anonymous WIS2-core single-message GRIB2 bulletins. Strip the WMO bulletin wrapper, preserve the dateline-crossing grid, and ingest the documented surface and pressure fields. |
| 8 | ECCC GEPS published products | P1 | Global 0.5-degree regular lat/lon, 00/12Z; 3-hourly to f192 then 6-hourly to f384; extended to f936 Monday/Thursday 00Z | Ingest provider-produced mean, standard deviation, percentile, and probability messages first. Raw 20 perturbed members plus control are P3. |
| 9 | ECMWF IFS ENS open subset | P1/P3 | Global 0.25 degree, four cycles/day; 00/12Z to f360 and 06/18Z to f144 | Reuse the existing ECMWF URL/index/range machinery for published `em`, `es`, and `ep` first. Control plus 50 perturbed members need the member contract. |
| 10 | ECMWF AIFS ENS v2 open subset | P1/P3 | Global 0.25 degree, four cycles/day; medium-range ensemble | Same staged approach as IFS ENS: indexed published statistics first, then control plus 50 members after the member contract. Keep it distinct from the supported AIFS Single v2 lane. |
| 11 | Met Office global deterministic | P2 | Global 2560x1920 regular lat/lon at 0.09 degree; four cycles/day, to f168 at 00/12Z and f067 at 06/18Z | Anonymous AWS ASDI CF-NetCDF split by variable and valid time. Start a bounded surface/pressure suite; a complete 12Z cycle is currently about 108 GiB and must never be the default plan. |
| 12 | NASA GMAO GEOS-FP | P2 | Global provider grid at 0.3125 by 0.25 degree; hourly 2-D and 3-hourly 3-D near-real-time output | Anonymous NCCS OPeNDAP/portal access. Build the bounded CF/OPeNDAP adapter against surface and pressure-level collections; mark the source experimental/non-operational. |
| 13 | Met Office UKV deterministic | P2/P3 | United Kingdom, native 2 km LAEA; hourly cycles with 12-, 54-, or 120-hour horizons by cycle | Anonymous AWS ASDI CF-NetCDF. Requires a native LAEA grid contract; start one current surface cycle after projection support. |
| 14 | KMA KIM global | P2 | Global postprocessed regular grid, 00/06/12/18Z; hourly to f135 then 3-hourly to f288 | API-key access with server-side variable, level, time, and bbox selection to standardized NetCDF. Fixture-verify the current NE57 grid rather than inheriting an older KIM header. |
| 15 | KMA KIM regional | P2 | East Asia, roughly 3 km Lambert grid | API-key targeted NetCDF or GRIB2. Start with surface and pressure fields and pin the current R030 cycle/horizon from the API response. |
| 16 | KMA KIM local | P2 | Korean Peninsula, roughly 1 km regional grid | API-key targeted standardized NetCDF. Keep it a distinct grid and verify its current L010 schedule before advertising availability. |
| 17 | Met Office MOGREPS-G | P2/P3 | Global 1280x960 regular lat/lon near 20 km, four cycles/day to f246 | Anonymous AWS ASDI CF-NetCDF. Each field file carries 18 raw members; a full cycle is about 1.97 TiB. Requires member identity plus a narrow, budgeted field suite. |
| 18 | Met Office MOGREPS-UK | P3 | United Kingdom 1042x970 native LAEA near 2 km, hourly cycles to f126 | Files carry three members per cycle; the operational 18-member lagged ensemble spans six reference times. Requires LAEA, member identity, and durable reference-time/accumulation semantics. |
| 19 | Météo-France ARPEGE 0.25 | P2 | Global regular 0.25 degree, 00/06/12/18Z, f000-f102; hourly for some fields and otherwise 3-hourly by lead | GRIB2 packages contain surface, height, and 34-level isobaric groups. Use the account-backed targeted API for the sparse RWS profile; do not fetch multi-gigabyte package groups by default. |
| 20 | Météo-France AROME France 0.025 | P2 | 12W-16E, 37.5N-55.4N regular grid; four cycles/day, hourly f000-f051 | Targeted API or carefully selected package groups. Initial surface and pressure profile; retain provider precipitation-window semantics. |
| 21 | Météo-France AROME France 0.01 | P2 | Same France/near-Europe domain at 0.01 degree; four cycles/day, hourly f000-f051 | Hourly GRIB2 objects are roughly 65-73 MB each in current catalogue observations. Enforce a forecast-window and field budget. |
| 22 | Météo-France ARPEGE 0.1 EURAT | P2 | Europe/Atlantic regular 0.1 degree (32W-42E, 20N-72N), four cycles/day to f102 | Richer regional ARPEGE output, but package objects are hundreds of MB. Prefer targeted API extraction. |
| 23 | Météo-France AROME Antilles 0.025 | P2 | Caribbean regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 25-31 MB. |
| 24 | Météo-France AROME Guyane 0.025 | P2 | French Guiana regional regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 9-11 MB. |
| 25 | Météo-France AROME Réunion-Mayotte 0.025 | P2 | Indian Ocean regional regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 80-87 MB. |
| 26 | Météo-France AROME Polynésie 0.025 | P2 | 157.5W-144.5W, 25.25S-12.6S regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 14-17 MB. |
| 27 | Météo-France AROME Nouvelle-Calédonie 0.025 | P2 | 156E-174E, 30S-10S regular grid, hourly output | Separate domain/model slug. Current hourly objects are roughly 16-18 MB. |
| 28 | DMI HARMONIE NEA | P2 | North European/Atlantic rotated lat/lon at 2.5 km; deterministic output collected every 3 hours, f000-f054 | GRIB2 surface, 65 model levels, and documented pressure levels. Use EDR parameter/time filtering; rotate grid-relative winds correctly. |
| 29 | DMI HARMONIE Greenland/Iceland | P2 | Greenland/Iceland regional grid, operational rolling forecast | Use the `harmonie_ig_*` EDR collections and keep it distinct from NEA. Surface, pressure, and model-level collections are separate. |
| 30 | MetCoOp MEPS | P2/P3 | Nordic 2.5 km, updated hourly, f000-f060 to f066 depending publisher/product | Deterministic CF-NetCDF through MET Norway or filtered GRIB2 through FMI is the first slice. SMHI also publishes raw member files; those require member identity and live member discovery. |
| 31 | MET Norway AROME-Arctic | P2 | Arctic 2.5 km, 00/06/12/18Z, f000-f060 | CF-NetCDF through THREDDS/OPeNDAP; separate grid and field inventory from MEPS. |
| 32 | Taiwan CWA WRF 15 km | P2 | East Asia regional domain, 6-hourly cycles, hourly f000-f084 | Anonymous one-file-per-lead GRIB2, about 59 MB at f000. No sidecar index; build a bounded remote inventory before selecting messages. |
| 33 | Taiwan CWA WRF 3 km | P2 | Taiwan and nearby seas, 1158x673, 6-hourly cycles, hourly f000-f084 | Anonymous one-file-per-lead GRIB2, currently about 179-181 MB each. Require range-based inventory and a strict sparse field budget. |
| 34 | Argentina SMN WRF 4 km | P2 | South America, 1249x999 Lambert grid, 00/06/12/18Z to f072 | Anonymous CF-NetCDF on AWS, one file per valid hour. The hourly group is about 2.6 GiB per cycle; select a bounded surface suite and preserve its limited public-variable inventory. |
| 35 | NOAA CFSv2 operational 9-month forecast | P2/P3 | Global; 00/06/12/18Z, four members per cycle, 6-hourly output to about nine months | Indexed GRIB2 products: `flxf` surface, `pgbf` 0.5-degree pressure, `ipvf` 1-degree isentropic, plus ocean products. Build the indexed adapter now; enable fields only after member identity is durable. |
| 36 | Météo-France PEARP global ensemble | P2/P3 | Global regular 0.25 degree, 00/06/12/18Z; control plus 34 perturbed members | Current lead objects are about 2.2-4.1 GB and have no companion record index. Use the ensemble API or a bounded remote-scan/index plan; never default to whole-object download. |
| 37 | DWD ICON global | P3 | Global native icosahedral mesh, about 13 km; 00/06/12/18Z, to f180 for 00/12Z and f120 for 06/18Z | Per-variable/level/lead bzip2 GRIB2, but no provider-published structured global grid. Requires native triangular topology. A current T2M object is about 3 MB. |
| 38 | DWD ICON-EPS global | P3 | Global native icosahedral ensemble, 00/06/12/18Z | Files carry all members for a field/lead. Requires both triangular topology and member identity. |
| 39 | DWD ICON-EU-EPS | P3 | European native icosahedral ensemble | Requires both triangular topology and member identity; do not substitute the deterministic regular-grid product. |
| 40 | DWD ICON-D2-EPS | P3 | Germany regional 2.2 km, 20 members, f000-f048 | Requires both triangular topology and member identity. |
| 41 | MeteoSwiss ICON-CH1-EPS | P3 | Switzerland 1 km triangular mesh, 11 members, eight cycles/day, f000-f033 (03Z extends to f045), 80 layers | STAC objects are split by parameter, step, and member. Requires triangular topology and member identity. |
| 42 | MeteoSwiss ICON-CH2-EPS | P3 | Switzerland 2.1 km triangular mesh, 21 members, four cycles/day, f000-f120, 80 layers | Same core gates as CH1. Treat CSCS object storage as transport, not producer. |
| 43 | KNMI HARMONIE Netherlands | P3 | Netherlands, about 2 km regular lat/lon, hourly output to f060 | API-key access, tar-packaged GRIB1. Deterministic and rolling-ensemble products are separate datasets. Needs a GRIB1 gate or explicit external normalization. |
| 44 | KNMI HARMONIE Europe | P3 | Europe 5.5 km rotated lat/lon, hourly output to f060 | GRIB1 tar packages. Rolling ensemble is 30 members delivered as six hourly batches of five; preserve each member's reference time and accumulation reset. |
| 45 | KNMI HARMONIE Caribbean | P3 | Caribbean 0.05-degree regular grid, hourly output to f060 | GRIB1 tar packages and API-key access; implement after the GRIB1 decision. |

### Shared implementation sequence

Do not create 45 unrelated downloaders. Land the reusable contracts in this
order, then add thin provider manifests and canonical maps:

1. Separate producer, licensing publisher, direct transport, and mirror in
   provenance. Make licence/attribution visible at run and API level.
2. Add a bounded remote GRIB inventory scanner that can use byte ranges when a
   provider lacks `.idx`, plus an explicit WMO bulletin-wrapper decoder. This
   unlocks CMA, Roshydromet, CWA, and later large package feeds.
3. Add one bounded CF-NetCDF/OPeNDAP acquisition contract with dimension,
   chunk, response, and decompression ceilings. Use it for GEOS-FP, Met Office,
   MET Norway, and Argentina SMN rather than provider-specific NetCDF parsers.
4. Add LAEA geometry before claiming UKV or MOGREPS-UK. Keep provider-native
   coordinates and grid mapping; any regrid is a derived product.
5. Add ensemble member plus reference-time identity before raw GEPS, ECMWF,
   MOGREPS, CFSv2, PEARP, SMHI MEPS, or ICON EPS members.
6. Add triangular topology before native global ICON, DWD ICON EPS, or
   MeteoSwiss ICON-CH. Do not silently substitute a local rectangular regrid.
7. Decide whether GRIB1 is decoded in process or normalized by a separately
   validated boundary tool before starting the three KNMI lanes.

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

### WMO WIS2 core feeds: CMA-GEPS and Roshydromet ICON

WIS2 metadata and notifications are the discovery layer. Model ownership and
licensing remain with the publishing National Meteorological and Hydrological
Service; a WIS2 Global Cache is only a replicated transport.

- WMO Unified Data Policy: <https://public.wmo.int/wmo-unified-data-policy-resolution-res1>
- WIS2 core-data guidance: <https://docs.wis2box.wis.wmo.int/en/latest/user/recommended.html>
- CMA-GEPS Global Discovery Catalogue record:
  <https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Acn-cma%3Adata.core.weather.prediction.forecast.medium-range.probabilistic.global?f=html>
- CMA source metadata:
  <https://wis2node.wis.cma.cn/oapi/collections/discovery-metadata/items/urn%3Awmo%3Amd%3Acn-cma%3Adata.core.weather.prediction.forecast.medium-range.probabilistic.global?f=json>
- CMA source-object prefix:
  `https://wis2node.wis.cma.cn/data/{YYYY-MM-DD}/wis/urn:wmo:md:cn-cma:data.core.weather.prediction.forecast.medium-range.probabilistic.global/`
- Roshydromet ICON Global Discovery Catalogue record:
  <https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Aru-roshydromet%3Awipps-dc.forecast.short-range.deterministic.limited-area.icon?f=html>
- Roshydromet source metadata:
  <http://wis2box.mecom.ru/data/metadata/urn:wmo:md:ru-roshydromet:wipps-dc.forecast.short-range.deterministic.limited-area.icon.json>
- Roshydromet source-object prefix:
  `http://wis2box.mecom.ru/data/{YYYY-MM-DD}/wis/urn:wmo:md:ru-roshydromet:wipps-dc.forecast.short-range.deterministic.limited-area.icon/`

Both records declare `wmo:dataPolicy=core`: WMO defines core data as free and
unrestricted, without charge or conditions on use. Authentication is not
required at the observed source nodes or Global Caches. CMA source metadata
declares 30-day retention; a Global Cache is only a 24-hour delivery cache.
CMA-GEPS publishes 74 lead files per run and an observed complete run was about
5.98 GB. Its files have no sidecar index, but individual messages observed at
roughly 0.2-1.2 MB make a bounded HTTP range inventory practical. The current
files use ensemble-derived, probability, and percentile product templates and
declare 31 forecasts; no raw members were observed.

Roshydromet publishes one small WMO-bulletin-wrapped GRIB2 message per object,
roughly 500 MB/day across two runs in the observed listing. The grid spans
19.5E through 193.5E and must retain its dateline-crossing longitude convention.
Prefer an HTTPS WIS2 Global Cache at runtime when the source node offers only
HTTP. Subscribe only after discovery works: the topics are
`cache/a/wis2/cn-cma/data/core/weather/prediction/forecast/medium-range/probabilistic/global`
and
`cache/a/wis2/ru-roshydromet/data/core/weather/prediction/forecast/short-range/deterministic/limited-area/icon`.

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

### Met Office

Producer and licensing publisher: **UK Met Office**. The anonymous public
transport below is the AWS Open Data Sponsorship Program in `eu-west-2`; AWS
does not become the producer. The ASDI catalogue declares British Crown
copyright under CC BY-SA. Retain that exact licence and attribution from the
catalogue record used for each object.

- Met Office external data channels:
  <https://www.metoffice.gov.uk/services/data/external-data-channels>
- Global deterministic ASDI registry:
  <https://registry.opendata.aws/met-office-global-deterministic/>
- UK deterministic/UKV ASDI registry:
  <https://registry.opendata.aws/met-office-uk-deterministic/>
- Global ensemble/MOGREPS-G ASDI registry:
  <https://registry.opendata.aws/met-office-global-ensemble/>
- UK ensemble/MOGREPS-UK ASDI registry:
  <https://registry.opendata.aws/met-office-uk-ensemble/>
- Official global NWP ASDI data sheet:
  <https://www.metoffice.gov.uk/binaries/content/assets/metofficegovuk/pdf/data/global-nwp-asdi-datasheet.pdf>
- Atmospheric data overview:
  <https://datahub.metoffice.gov.uk/docs/f/category/atmospheric/overview>

Authentication is not required for the ASDI buckets
`met-office-atmospheric-model-data`,
`met-office-global-ensemble-model-data`, and
`met-office-uk-ensemble-model-data`. Objects are CF-NetCDF/UKMO convention
files, gzip-chunked in the observed samples, and support HTTP/S3 byte ranges.
The deterministic archives retain about two years and ensemble archives about
30 days. They are free, unsupported research data and must not be treated as a
critical operational service.

Files are separated by variable and valid time, but complete cycles remain
large: an observed global 12Z cycle was about 108 GiB, UKV about 24 GiB,
MOGREPS-G about 1.97 TiB, and MOGREPS-UK about 237 GiB. Store the enumerated
object lengths in every acquisition plan. Current MOGREPS-G field files expose
18 realizations. MOGREPS-UK exposes three realizations in each hourly cycle and
forms an 18-member lagged ensemble across six reference times; member number,
reference time, and accumulation reset must all survive ingestion.

Met Office also publishes a small global deterministic core subset over WIS2,
under topic
`cache/a/wis2/uk-metoffice-wmc/data/core/weather/prediction/forecast/medium-range/deterministic/global`.
It is an alternate 24-hour transport, not a separate model lane. See the
[official WMO services page](https://www.metoffice.gov.uk/services/data/wmo-data-and-services)
and [WIS2 Global Cache data sheet](https://www.metoffice.gov.uk/binaries/content/assets/metofficegovuk/pdf/data/wis2-global-cache-on-aws-datasheet.pdf).

### NASA GMAO

Producer: **NASA Global Modeling and Assimilation Office**. Licensing
publisher: **NASA**. Direct transports are the NCCS OPeNDAP and GMAO portal
hosts; neither is a separate data owner.

- GMAO product catalogue: <https://gmao.gsfc.nasa.gov/gmao-products/>
- GEOS near-real-time products:
  <https://gmao.gsfc.nasa.gov/gmao-products/geos-near-real-time-data-products/>
- Current GEOS-FP file specification:
  <https://gmao.gsfc.nasa.gov/media/publications/zbly36ziNFDQeVqPhUoFbmYmvh/GEOS_5_FP_File_Specification_ON4v2_0_fqkxorzhgte_FgpliLt.pdf>
- OPeNDAP catalogue: <https://opendap.nccs.nasa.gov/dods/GEOS-5/fp>
- Surface latest endpoint:
  <https://opendap.nccs.nasa.gov/dods/GEOS-5/fp/0.25_deg/fcast/inst1_2d_hwl_Nx.latest>
- Pressure-level latest endpoint:
  <https://opendap.nccs.nasa.gov/dods/GEOS-5/fp/0.25_deg/fcast/inst3_3d_asm_Cp.latest>
- Dated portal root: <https://portal.nccs.nasa.gov/datashare/gmao/geos-fp/>

Authentication is not required. The current production system uses a C720
cubed-sphere solver near 12.5 km, but the published analysis/forecast files are
structured 0.3125-by-0.25-degree output. Two-dimensional collections are
hourly and three-dimensional collections are generally 3-hourly, with native
72-level and 42-pressure-level families documented. The adapter must inspect
the live DDS/DAS dimensions because `latest` aliases and older system versions
can differ from the current specification. Use OPeNDAP projections rather than
fetching global collections.

NASA's [Earth Science data-use policy](https://www.earthdata.nasa.gov/engage/open-data-services-software/data-use-policy)
says NASA-led unrestricted data are available under CC0 by default, permit
reproduction and distribution, require acknowledgement of NASA, and do not
permit implied endorsement. GMAO explicitly calls GEOS-FP experimental and
non-operational with no backup; expose that stability class in capabilities.

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

### Korea Meteorological Administration (KMA)

Producer and licensing publisher: **Korea Meteorological Administration**.
Transport is the KMA API Hub; the Korean public-data catalogue is discovery
and licensing metadata, not the model producer.

- KIM official model description:
  <https://datawiki.kma.go.kr/doku.php?id=수치모델:단중기모델:한국형수치예보모델_kim>
- Global and regional model public-data record:
  <https://www.data.go.kr/en/data/15126648/openapi.do>
- Additional KIM API record:
  <https://www.data.go.kr/data/15139467/openapi.do>
- KMA API Hub model-product catalogue:
  <https://apihub.kma.go.kr/apiList.do?seqApi=9>
- Standardized NetCDF request template:
  `https://apihub.kma.go.kr/api/typ06/cgi-bin/url/nph-kim_nc_xy_txt2_std?group={KIMG|KIMR|KIML}&nwp={NE57|R030|L010}&data=U&name={variable}&map=F&tmfc={YYYYMMDDHH}&hf={lead}&disp=A&level={level}&authKey={KEY}`
- Regional GRIB2 request template:
  `https://apihub.kma.go.kr/api/typ06/cgi-bin/url/nph-kim_grib_xy_txt1?group=KIMR&nwp=r030&data=U&varn={code}&level={level}&tmfc={YYYYMMDDHH}&hf={lead}&disp=A&authKey={KEY}`

An application/API key is required, but the public-data records declare free
access and Korea's Type 1 public-work licence: reuse is permitted without
purpose restriction with source attribution. Requests can select variable,
level, lead, grid point, or bounding box, making this much safer than bulk
cycle retrieval. The global KIMG/NE57 service currently advertises hourly
leads 1-135 and 3-hourly leads 138-288; it keeps about 180 days of global
access. KIMR/R030 and KIML/L010 are separate regional and local products.

KIM's solver is cubed-sphere, but these public endpoints expose provider-
postprocessed structured grids. Preserve that interpolation provenance. Do
not hard-code the older documented 0.125-degree, 2880x1440 global header: pin a
live NE57 geometry fixture. The retired KMA Unified Model service, discontinued
2026-03-31, is not another active lane.

### Taiwan Central Weather Administration (CWA)

Producer and licensing publisher: **Taiwan Central Weather Administration**.
Transport is CWA Open Data object storage on AWS in `ap-northeast-1`.

- Official dataset record: <https://data.gov.tw/en/datasets/58977>
- Official open-data licence: <https://data.gov.tw/license>
- CWA product documentation:
  <https://www.cwa.gov.tw/Data/data_catalog/7-2-2.pdf>
- 15 km discovery JSON:
  <https://cwaopendata.s3.ap-northeast-1.amazonaws.com/Model/M-A0061-000.json>
- 15 km GRIB2 template:
  `https://cwaopendata.s3.ap-northeast-1.amazonaws.com/Model/M-A0061-{hhh}.grb2`
- 3 km discovery JSON:
  <https://cwaopendata.s3.ap-northeast-1.amazonaws.com/Model/M-A0064-000.json>
- 3 km GRIB2 template:
  `https://cwaopendata.s3.ap-northeast-1.amazonaws.com/Model/M-A0064-{hhh}.grb2`

Authentication is not required. CWA's Open Government Data Licence 1.0 permits
reproduction, distribution, adaptation, and sublicensing for any purpose with
attribution, and declares compatibility with CC BY 4.0. Both WRF products run
every six hours with hourly leads to f084. Current discovery metadata describes
the 3 km grid as 1158x673. Files combine many messages and publish no companion
index; observed f000 sizes were about 59 MB at 15 km and 179 MB at 3 km. Before
ingest, implement a bounded remote GRIB header walk or provider-approved sparse
selection rather than downloading 85 complete lead files.

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

### MetCoOp MEPS and MET Norway AROME-Arctic

MEPS producer: **MetCoOp**, the Nordic operational collaboration. Its
licensing publisher and transport depend on the selected feed: MET Norway
THREDDS/OPeNDAP, FMI Open Data, or SMHI open data. AROME-Arctic is produced and
published by **Norwegian Meteorological Institute (MET Norway)**.

- MET Norway THREDDS documentation: <https://api.met.no/product/THREDDS>
- MEPS latest deterministic endpoint:
  `https://thredds.met.no/thredds/dodsC/meps25files/meps_det_pp_2_5km_latest.nc`
- AROME-Arctic latest endpoint:
  `https://thredds.met.no/thredds/dodsC/aromearcticlatest/arome_arctic_pp_2_5km_latest.nc`
- FMI forecast-model manual:
  <https://en.ilmatieteenlaitos.fi/open-data-manual-forecast-models>
- FMI WFS and download-service manual:
  <https://en.ilmatieteenlaitos.fi/open-data-manual-fmi-wfs-services?doAsUserLanguageId=en_US>
- FMI bounded-download template:
  `https://opendata.fmi.fi/download?producer=harmonie_scandinavia_surface&param={parameters}&format=grib2&bbox={west,south,east,north}&projection=EPSG:4326`
- SMHI CMEPS directory:
  <https://data-download.smhi.se/data/meteorology/cmeps/>
- SMHI CMEPS inventory description:
  <https://data-download.smhi.se/data/example_files/meteorology/cmeps/Filecontent_README>

Authentication is not required. For the first deterministic adapter, use
OPeNDAP projections or FMI's server-side parameter, bbox, and format filters;
cap decoded dimensions before allocation. FMI limits WFS usage to 20,000
requests/day and 600 requests per five minutes. SMHI describes a 15-member
3-hour rolling ensemble to about f066, but the live member directories can
differ from the descriptive document; discover them every cycle. Its raw
members remain gated on member identity. Record `metcoop` as producer while
retaining MET Norway, FMI, or SMHI as the licensing publisher and transport.
Do not count the same MEPS run as several model lanes merely because several
national services publish it.

### Argentina Servicio Meteorológico Nacional (SMN)

Producer and licensing publisher: **Servicio Meteorológico Nacional,
Argentina**. Transport is the AWS Open Data bucket `smn-ar-wrf` in
`us-west-2`.

- AWS Open Data registry record:
  <https://registry.opendata.aws/smn-ar-wrf-dataset/>
- Official dataset documentation:
  <https://odp-aws-smn.github.io/documentation_wrf_det/>
- General model information:
  <https://odp-aws-smn.github.io/documentation_wrf_det/Informacion_general/>
- Object structure:
  <https://odp-aws-smn.github.io/documentation_wrf_det/Estructura_de_datos/>
- NetCDF format and variables:
  <https://odp-aws-smn.github.io/documentation_wrf_det/Formato_de_datos/>
- Anonymous access:
  <https://odp-aws-smn.github.io/documentation_wrf_det/Acceso_a_los_datos/>

Authentication is not required, and the archive begins in 2022. The dataset is
CC BY 2.5 Argentina; credit SMN and retain the registry access date requested
by its citation guidance. The live archive and official documentation show
four cycles/day despite an older registry summary that says two. Public files
are CF-NetCDF on a 1249x999 Lambert grid, one hourly file through f072 plus
separate 10-minute precipitation and daily products. An observed hourly group
was about 2.58 GiB per cycle. The public file inventory is mainly surface and
derived fields; do not imply access to all 45 internal WRF model levels. Start
with one hourly surface scalar, wind, and accumulated precipitation fixture.

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
| `cma` | CMA-GEPS record declares WMO core: free and unrestricted, without charge or conditions on use | No WMO-core usage condition; retain CMA origin and WIS2 record as provenance | [CMA-GEPS discovery metadata](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Acn-cma%3Adata.core.weather.prediction.forecast.medium-range.probabilistic.global?f=html) and [WMO policy](https://public.wmo.int/wmo-unified-data-policy-resolution-res1) |
| `roshydromet` | ICON limited-area record declares WMO core: free and unrestricted, without charge or conditions on use | No WMO-core usage condition; retain Roshydromet/Hydrometcentre origin and WIS2 record as provenance | [Roshydromet discovery metadata](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Aru-roshydromet%3Awipps-dc.forecast.short-range.deterministic.limited-area.icon?f=html) and [WMO policy](https://public.wmo.int/wmo-unified-data-policy-resolution-res1) |
| `ecmwf` | Open subset may be redistributed and used commercially under CC BY 4.0 plus ECMWF terms | ECMWF attribution required by the terms; preserve licence and modification information | <https://www.ecmwf.int/en/forecasts/datasets/open-data> |
| `met-office` | ASDI atmospheric model data are British Crown copyright under CC BY-SA | UK Met Office/Crown attribution, licence link, and share-alike obligations for adapted material | [Met Office global ASDI registry](https://registry.opendata.aws/met-office-global-deterministic/) |
| `nasa-gmao` | NASA-led unrestricted Earth-science data default to CC0 and permit reproduction/distribution | Acknowledge NASA/GMAO; do not imply NASA endorsement | <https://www.earthdata.nasa.gov/engage/open-data-services-software/data-use-policy> |
| `meteo-france` | Licence Ouverte / Open Licence 2.0 permits reuse, including commercial reuse | `Météo-France`, source link, and indication of modifications | <https://www.data.gouv.fr/datasets/paquets-arpege-resolution-0-25deg> |
| `kma` | Korea Type 1 public-work licence permits unrestricted-purpose reuse with source indication | Korea Meteorological Administration, source, and any modification notice required by Type 1 | <https://www.data.go.kr/en/data/15126648/openapi.do> |
| `cwa-taiwan` | Open Government Data Licence 1.0 permits reproduction, distribution, adaptation, and sublicensing for any purpose; compatible with CC BY 4.0 | Taiwan Central Weather Administration and the supplied source/terms link | <https://data.gov.tw/license> |
| `dmi` | CC BY 4.0 permits sharing and adaptation for any purpose, including commercial | Danish Meteorological Institute/DMI, licence link, and change indication | <https://www.dmi.dk/friedata/dokumentation/terms-of-use> |
| `met-norway` | NLOD 2.0 and CC BY 4.0 are offered for MET Norway data | Credit MET Norway and link the selected licence | <https://docs.api.met.no/doc/License.html> |
| `metcoop` via `fmi` | FMI publishes its MEPS feed under CC BY 4.0 | Credit MetCoOp as producer and FMI as licensing publisher; link licence and indicate changes | <https://en.ilmatieteenlaitos.fi/open-data-licence> |
| `metcoop` via `smhi` | SMHI open data are published under CC BY 4.0 unless a dataset says otherwise | Credit MetCoOp as producer and SMHI as licensing publisher; link licence and indicate changes | <https://www.smhi.se/data/om-smhis-data/fragor-och-svar> |
| `smn-argentina` | CC BY 2.5 Argentina permits sharing and adaptation, including commercial use | Servicio Meteorológico Nacional, dataset citation, and registry access date | <https://registry.opendata.aws/smn-ar-wrf-dataset/> |
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
- CMA-GEPS: WIS2 discovery record and bounded object listing, range-scanned
  GRIB inventory, examples of probability/percentile/ensemble-derived product
  templates, forecast-count assertion, and a negative assertion that the
  fixture does not contain raw members.
- Roshydromet ICON: WIS2 metadata and notification, a complete tiny bulletin
  object, wrapper-offset assertion, regular-grid corners across 180 degrees,
  and one pressure plus one accumulated surface message.
- ECMWF: `.index` plus selected ranges for `em`, `es`, `ep`, `cf`, and one `pf`;
  assert ensemble number/control metadata rather than filename-only routing.
- Met Office: anonymous ASDI listing plus NetCDF header/chunk metadata. Prove
  global regular-grid geometry, UKV/MOGREPS-UK LAEA metadata, MOGREPS-G's 18
  realizations, and MOGREPS-UK's three realizations/reference time without
  downloading a complete field or cycle.
- NASA GMAO: OPeNDAP DDS/DAS, one tiny projected surface response, one pressure
  slice, live-vs-spec dimension assertion, and experimental-service stability
  metadata.
- Météo-France: package/API discovery JSON, one small targeted GRIB2 response,
  representative package metadata without downloading a multi-GB object, and
  a precipitation-window fixture.
- KMA: credential-stripped API help response, a bbox-selected standardized
  NetCDF field for each KIMG/KIMR/KIML grid, current cadence assertion, and
  provenance showing that the public endpoint is postprocessed from native KIM.
- Taiwan CWA: both discovery JSON records plus range-scanned inventories from
  one 15 km and one 3 km GRIB2 lead; reject any plan that falls back to all 85
  full lead objects.
- DMI: EDR collection/parameter discovery, a selected GRIB2 response, rotated
  grid coordinates, and a vector-wind rotation golden.
- MetCoOp/MET Norway: THREDDS catalogue and a tiny OPeNDAP slice, FMI filtered
  GRIB2 equivalent, and producer/licensing-publisher assertions. Before raw
  ensemble support, pin an SMHI listing proving the live member set rather than
  assuming all documented members exist.
- Argentina SMN: anonymous S3 listing, CF-NetCDF header, Lambert coordinate
  golden, one surface and one accumulation field, and an explicit inventory
  proving which public files lack model-level fields.
- CFSv2: `.idx` and selected message ranges from `flxf`, `pgbf`, and `ipvf`,
  with member and six-hour valid-time assertions.
- MeteoSwiss: STAC collection and item, parameter CSV, static geometry asset,
  and one bounded signed-asset range with the signature removed from fixtures.
- KNMI: API manifest, safe tar inventory, and the smallest legal GRIB1 sample;
  no support claim until edition-1 decode and rolling-member timing pass.

## Watchlist and access gates

These feeds are not included in the 45-lane implementation count. Recheck them
periodically, but do not build a production adapter until the named gate is
closed with an official source and a live bounded fixture.

| Feed | Evidence | Why it is not in the active queue | Promotion gate |
| --- | --- | --- | --- |
| Roshydromet SL-AV global | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Aru-roshydromet%3Awmc-moscow.forecast.medium-range.deterministic.global.sl-av?f=html) | Metadata declares WMO core and documents SLAV10 output, but no current forecast objects were present at the source node during this survey. | Observe and pin one complete live cycle before calling the feed available. |
| Cyprus Department of Meteorology WRF | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Acy-dom%3Aweather.prediction.deterministic.local?f=html) and [direct directory](https://www.dom.org.cy/wis2/data/core/weather/prediction/forecast/short-range/deterministic/limited-area/) | The five overwrite-style GRIB2 products were last modified 2026-01-06 during this 2026-08-14 survey. | Resume only after fresh, regularly advancing timestamps are observed. |
| Italy MeteoAM limited-area forecast | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Ait-meteoam%3Aforecast.short-range.deterministic.limited-area?f=html) | Metadata declares WMO core, but the advertised source endpoint repeatedly timed out and no bounded payload could be verified. | Pin a reachable official listing, retention, and GRIB fixture. |
| Australia Bureau of Meteorology ACCESS | [ACCESS NWP products](https://www.bom.gov.au/nwp/doc/access/NWPData.shtml), [copyright notice](https://www.bom.gov.au/copyright), and [data licence agreement](https://www.bom.gov.au/sites/default/files/2026-07/bureau-of-meteorology-data-licence-agreement-june-2026.pdf) | Operational model files use a Registered User/subscriber channel. Default Bureau terms do not establish unrestricted third-party or commercial redistribution. | Obtain and record a licence that covers RWS redistribution and automated access. |
| JMA GSM/MSM/LFM GPV | [official product catalogue](https://www.data.jma.go.jp/suishin/cgi-bin/catalogue/make_product_page.cgi?id=ZenModel), [JMBSC distribution](https://www.jmbsc.or.jp/en/index-e.html), and [official samples](https://www.data.jma.go.jp/developer/gpv_sample.html) | Operational GPV delivery is through the contracted/paid JMBSC service. Public sample files are suitable only as decoder fixtures and do not establish redistribution rights. | Establish an official operational access and redistribution contract; do not treat sample files as a live feed. |
| CPTEC/INPE BAM | [official anonymous directory](https://ftp.cptec.inpe.br/modelos/tempo/BAM/) and [INPE 2025-2027 open-data plan](https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/repositorio-de-arquivos/pda_inpe_25_27_v3.pdf) | The current anonymous `singleLevel` GRIB2 tree is only a limited subset; the plan schedules global raw BAM grid output for June 2027 and current redistribution terms are not explicit enough. | Confirm a complete grid inventory and official reuse/redistribution terms. |
| AEMET HARMONIE-AROME packages | [official catalogue](https://datos.gob.es/en/catalogo/e05068001-datos-del-modelo-harmonie-arome) and [AEMET legal notice](https://www.aemet.es/es/nota_legal) | Public packages are selected derived GeoTIFF/GeoJSON surface products, not a canonical full NWP state. Reuse is allowed with attribution, but counting it as full model normalization would overstate semantics. | Add only as an explicitly derived-product lane, or locate an official full-field feed. |

## Deliberately deferred adjacent feeds

The same official catalogues expose ECCC CAPS/REPS/CanSIPS/NAEFS, ECCC analysis
and air-quality systems, NASA GEOS-CF atmospheric chemistry, CMA-CW dust and
chemistry products, and multiple ocean, ice, wave, surge, and climate-analysis
products. They are valuable, but they should be researched as separate
canonical-domain lanes rather than being counted as atmospheric weather-model
support. This prevents a large catalogue number from hiding missing semantics
in the store and API.

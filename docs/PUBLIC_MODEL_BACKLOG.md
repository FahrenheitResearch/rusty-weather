# Public numerical-model implementation backlog

Research snapshot: 2026-08-14. This is a discovery and implementation-planning
document, not a claim that the feeds below are supported. Current support is
reported only by [MODEL_SUPPORT.md](MODEL_SUPPORT.md) and the running service's
`/v1/models` response.

This inventory contains 70 deduplicated atmospheric model/domain lanes that
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

### Preserve atmospheric-composition identity

Air-quality fields are not interchangeable just because their display units
can be converted. Persist chemical species or aerosol size class, concentration
basis (mass, dry-air mole fraction, mixing ratio, optical depth, or column
integral), humidity convention, vertical support, averaging interval, and raw,
bias-corrected, analysis, or forecast status. A provider-local constituent code
must resolve through a pinned official table or remain unsupported. Never map a
column integral to a surface concentration, a provider median to a deterministic
member, or a bias-corrected product over the raw field with the same canonical
identity.

## Ranked implementation queue

The order favors operational, anonymous, structured GRIB2 feeds that preserve
their native semantics with existing RWS concepts. `P1` is adapter work on an
existing contract, `P2` needs a new bounded acquisition/format adapter, and
`P3` is gated by a core storage or topology extension.

Rank numbers are stable, append-only review identifiers so links and review
notes do not churn. Rows 46-55 are the third discovery wave, rows 56-61 are the
fourth, and rows 62-70 are the fifth; use `Class` and the shared implementation
sequence below for current execution priority.

| Rank | Lane | Class | Native schedule and domain | Public data and first implementation slice |
| ---: | --- | --- | --- | --- |
| 1 | ECCC GDPS 15 km | P1 | Global 0.15-degree regular lat/lon, 00/12Z; hourly f000-f084, then 3-hourly to f240 | Anonymous per-variable/level/lead GRIB2. Start surface plus the documented pressure-level sounding set. |
| 2 | ECCC RDPS 10 km | P1 | Canada and adjacent US, rotated lat/lon, 00/06/12/18Z; hourly f000-f084 | Anonymous per-field GRIB2. Preserve the rotated grid and native interval metadata. |
| 3 | ECCC HRDPS continental 2.5 km | P1 | 2540x1290 rotated lat/lon, 00/06/12/18Z; hourly f000-f048 | Anonymous per-field GRIB2; surface plus up to 31 published pressure levels. |
| 4 | DWD ICON-EU regular-grid output | P1 | Europe, 0.0625-degree regular lat/lon; 00/06/12/18Z to f120 and 03/09/15/21Z to f30 | Anonymous per-variable/level/lead bzip2-compressed GRIB2. Use the provider's regular-grid objects. |
| 5 | DWD ICON-D2 regular-grid output | P1 | Germany and neighbors, about 2.2 km; eight cycles/day, hourly f000-f048 | The directory publishes both native icosahedral and regular-lat-lon objects. Select only filenames explicitly marked `regular-lat-lon`. |
| 6 | CMA-GEPS published ensemble products | Implemented (live verified) | Global 0.25-degree regular lat/lon, 00/12Z; 3-hourly to f078 then 6-hourly to f360 | Anonymous WIS2-core GRIB2. The implemented lane preserves 57 identified probability, percentile, and ensemble-derived fields for 31 forecasts; it does not claim raw members or unknown local parameters. Whole-object acquisition is bounded to one 38-88 MB lead file because no sidecar index is published. |
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
| 46 | CPTEC/INPE WRF South America 7 km | P1 | South America, 1019x1081 regular lat/lon at 0.07 degree, daily 00Z; hourly f000-f180 | Anonymous GRIB2 with `.inv`, `.grib2.idx`, and GrADS control sidecars. Select indexed surface and pressure messages; a whole cycle is roughly 34 GiB. |
| 47 | CPTEC/INPE BRAMS South America 8 km | P1 | South America, 978x1009 regular lat/lon, daily 00Z; hourly f000-f180 | Anonymous indexed GRIB2. Exclude the three pre-analysis files from the forecast run and preserve the grid's unequal longitude/latitude increments. |
| 48 | GeoSphere Austria C-LAEF deterministic 2.5 km | P2 | AlpeAdria, provider-regridded regular WGS84 grid; every three hours, hourly to f060 | Anonymous targeted NetCDF or GeoJSON API. Persist native-model generation and GeoSphere's 1 km-to-2.5 km interpolation provenance. |
| 49 | GeoSphere Austria C-LAEF percentiles 2.5 km | P2 | AlpeAdria, 00/12Z, hourly to f060; 17-run ensemble summarized as p10/p50/p90 | Ingest only the provider-published percentiles. Raw members are not exposed by this dataset; never reconstruct or imply member states. |
| 50 | MeteoGalicia WRF Galicia 4 km | P2 | Galicia and nearby Atlantic, 117x126 Lambert grid, 00/12Z; hourly f001-f096 at 00Z and f001-f084 at 12Z | THREDDS/OPeNDAP CF-NetCDF. Start a tiny projected surface slice and retain the two-dimensional latitude/longitude coordinates. |
| 51 | MeteoGalicia WRF Iberia 12 km | P2 | Iberia and Bay of Biscay, 171x138 Lambert grid, 00/12Z; hourly f001-f096 or f001-f084 | Same bounded THREDDS adapter as the 4 km grid, with a distinct geometry and model identity. |
| 52 | MeteoGalicia WRF Atlantic/SW Europe 36 km | P2 | Atlantic and southwest Europe, 118x104 Lambert grid, 00/12Z; hourly f001-f096 or f001-f084 | Same bounded THREDDS adapter. Public fields are a surface/derived suite plus a small fixed upper-air subset, not a complete sounding profile. |
| 53 | MeteoGalicia WRF ensemble Galicia 4 km | P3 | Galicia, daily 00Z, 21 raw members, hourly f001-f216 | OPeNDAP makes the multi-GB NetCDF files sliceable, but the dataset exposes no authoritative control/perturbed-member coordinate. Gate on an official member contract. |
| 54 | MeteoGalicia WRF ensemble Iberia 12 km | P3 | Iberia and Bay of Biscay, daily 00Z, 21 raw members, hourly f001-f216 | Same member-identity gate as the 4 km ensemble; never guess that array index zero is the control. |
| 55 | Google DeepMind WeatherNext 2 historical | P2/P3 | Global 0.25 degree, 00/06/12/18Z; 64 members and published mean, 6-hourly to 15 days | Scope the first adapter to data older than 48 hours and the published mean. Historical data are CC BY 4.0; current data have separate terms that prohibit an open RWS redistribution path. Raw members remain P3. |
| 56 | CHMI ALADIN CZ1K | P3 | Czech Republic and nearby areas, 501x290 regular lat/lon near 1 km, 00/06/12/18Z; hourly f001-f072 | Anonymous one-variable cycle aggregates compressed with bzip2. The surface/derived-only feed is GRIB1 and uses local parameter 230 for precipitation type; ship the provider tables with the fixture and never guess unknown parameters. |
| 57 | CHMI ALADIN Lambert 2.3 km | P3 | Central Europe, 1053x837 Lambert grid at 2325 m, 00/06/12/18Z; hourly f001-f072 | Anonymous per-variable GRIB1 aggregates, including a documented 17-level pressure suite. A complete current cycle is about 7.6 GiB compressed, so select field objects before transfer and enforce both bzip2 limits. |
| 58 | ARSO ALADIN Slovenia 4 km | P3 | Slovenia and surroundings, 111x71 regular lat/lon near 4 km, 00/06/12/18Z; hourly f000-f072 | Anonymous ZIP containing 73 GRIB1 leads and a bounded surface/four-pressure-level inventory. Whole-package transfer is about 30 MB; preserve the ZIP manifest and cumulative-precipitation window. |
| 59 | ARPAE-SIMC ICON-2I Emilia-Romagna crop | P2 | Emilia-Romagna, 153x81 regular lat/lon, 00/12Z; hourly f000-f072 | Anonymous single-cycle GRIB2, about 169 MB and 7,521 messages, with six pressure levels. No sidecar index is published: range-scan once, cache exact message offsets, and fail closed on provider-local unknown fields. |
| 60 | RMI ALARO Belgium 4 km | P3 | Belgium and surroundings, 177x177 regular lat/lon near 4 km, 00/06/12/18Z; hourly f000-f060 | Anonymous per-variable GRIB1, about 362 MB for a current full run. Pressure fields carry 15 levels; provider local tables are mandatory for precipitation and other local concepts. |
| 61 | UWC-West DINI-EPS Ireland | P3 | Ireland regional 2 km Lambert grids, hourly cycling; control to f060 and lagged 31-member ensemble to f054 | Met Eireann's near-real-time API delivers GRIB2 CCSDS. Five perturbed members arrive each hour and the full ensemble spans six reference times; preserve reference time, perturbation number, and accumulation resets, and never route duplicated filenames alone. |
| 62 | ECCC REPS published statistics | P1/P3 | Canada and adjacent US, 908x960 rotated lat/lon at 0.09 degree, 00/06/12/18Z; 3-hourly f000-f072 | Anonymous per-field GRIB2. Ingest provider percentiles, mean, spread, minimum, and maximum first. The control plus 20 perturbed members remain P3 until member identity is durable. |
| 63 | ECCC CAPS Arctic 3 km atmosphere | P1/P2 | Arctic basin, 2230x1830 rotated lat/lon at 0.03 degree, 00/12Z; hourly f000-f048 | Anonymous per-field GRIB2 from an experimental coupled atmosphere-ocean-ice system. Start a sparse atmospheric surface/pressure suite, preserve experimental status, and keep ocean/ice output outside this lane. |
| 64 | ECCC RAQDPS air quality | P1/P2 | North America, 729x599 rotated lat/lon at 0.09 degree, 00/12Z; hourly f000-f072 | Anonymous per-field GRIB2 for surface pollutants and separate column products. Pin ECCC constituent mappings, including wildfire-smoke PM2.5; stock ecCodes currently reports some live messages as unknown. |
| 65 | NOAA AQM/CMAQ CONUS grid 227 | P1/P2 | CONUS, 1473x1025 Lambert grid at about 5.1 km; current listing publishes 06/12Z, 72 one-hour forecast intervals | Anonymous per-variable aggregate GRIB2 without `.idx`. Start raw one-hour PM2.5 and ozone, build a bounded inventory once, and keep bias-corrected/max-window products distinct. |
| 66 | NOAA AQM/CMAQ Alaska grid 198 | P1/P2 | Alaska, 825x553 polar stereographic grid at about 6 km; current listing publishes 06/12Z, 72 one-hour intervals | Same AQM contract on a materially different provider grid. A complete current PM2.5 aggregate is about 18.5 MB and contains 72 messages. |
| 67 | NOAA AQM/CMAQ Hawaii grid 196 | P1/P2 | Hawaii, 321x225 Mercator grid at 2.5 km; current listing publishes 06/12Z, 72 one-hour intervals | Same AQM contract on the provider's Hawaii grid. A complete current PM2.5 aggregate is about 1.8 MB and is the smallest geometry/interval fixture. |
| 68 | NASA GMAO GEOS-CF v2 | P2 | Global 0.25-degree 1440x721 grid, daily production with a five-day forecast; hourly surface and pressure-level composition collections | Anonymous NCCS OPeNDAP and public S3 Zarr. Treat it as a research composition forecast, preserve half-hour-centred average times and RH35 PM identity, and use only projected slices or bounded chunks. |
| 69 | CAMS global atmospheric composition forecast | P2 | Global 0.4-degree regular grid, 00/12Z to five days; hourly single-level and provider-constrained multi-level output | Account/API-token ADS retrieval under CC BY 4.0. Use the anonymous STAC/form/cost endpoints to validate and budget a tightly targeted GRIB or zipped-NetCDF request before authenticated execution. |
| 70 | CAMS European air-quality ensemble median | P2/P3 | Europe 25W-45E, 30N-72N at 0.1 degree, daily; hourly f000-f096 on surface through 5000 m | Ingest the provider-published 11-system ensemble median first. Individual system outputs and any locally derived spread need a durable model-member identity and remain P3. |

### Shared implementation sequence

Do not create 70 unrelated downloaders. Land the reusable contracts in this
order, then add thin provider manifests and canonical maps:

1. Separate producer, licensing publisher, direct transport, and mirror in
   provenance. Make licence/attribution visible at run and API level.
2. Add a bounded remote GRIB inventory scanner that can use byte ranges when a
   provider lacks `.idx`, plus an explicit WMO bulletin-wrapper decoder. This
   unlocks CMA, Roshydromet, CWA, CPTEC/INPE, ARPAE-SIMC, NOAA AQM, and later
   large package feeds.
3. Add one bounded CF-NetCDF/OPeNDAP acquisition contract with dimension,
   chunk, response, and decompression ceilings. Use it for GEOS-FP, GEOS-CF,
   Met Office, MET Norway, Argentina SMN, GeoSphere Austria, and MeteoGalicia
   rather than provider-specific NetCDF parsers.
4. Add LAEA geometry before claiming UKV or MOGREPS-UK. Keep provider-native
   coordinates and grid mapping; any regrid is a derived product.
5. Add ensemble member plus reference-time identity before raw GEPS, ECMWF,
   REPS, MOGREPS, CFSv2, PEARP, SMHI MEPS, MeteoGalicia WRF, WeatherNext 2,
   CAMS individual systems, DINI-EPS, or ICON EPS members.
6. Add triangular topology before native global ICON, DWD ICON EPS, or
   MeteoSwiss ICON-CH. Do not silently substitute a local rectangular regrid.
7. Decide whether GRIB1 is decoded in process or normalized by a separately
   validated boundary tool before starting KNMI, CHMI, ARSO, or RMI. Add
   bounded bzip2/ZIP inventory contracts and pin every provider-local table.
8. Add a fail-closed availability/licence-window policy before WeatherNext 2:
   historical objects older than 48 hours may enter the public adapter, while
   current data remain undiscoverable and unqueryable without different rights.
9. Add the composition identity contract before RAQDPS, NOAA AQM, GEOS-CF, or
   CAMS. Require provider-table fixtures and distinct canonical roles for
   surface concentration, mixing ratio, mole fraction, optical depth, column
   integral, averaging window, humidity basis, and bias correction.

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
- REPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_reps/readme_reps-datamart_en/>
  and `https://dd.weather.gc.ca/today/ensemble/reps/10km/grib2/{HH}/{hhh}/`
- CAPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_caps/readme_caps-datamart_en/>
  and `https://dd.weather.gc.ca/today/model_caps/3km/{HH}/{hhh}/`
- RAQDPS: <https://eccc-msc.github.io/open-data/msc-data/nwp_raqdps/readme_raqdps-datamart_en/>
  and `https://dd.weather.gc.ca/today/model_raqdps/10km/grib2/{HH}/{hhh}/`

Authentication is not required. Dated DataMart roots retain roughly 30 days;
`/today` is the current-day view. No companion record indexes are published,
but files are already split by variable, level, and lead (GEPS and REPS group
members or products for one field). Whole-object GET is therefore the normal
path. Enumerate a bounded directory, HEAD selected objects, and sum their
lengths before acquisition. AMQP notification is the preferred follow
mechanism after the initial HTTP adapter.

REPS runs four times daily and publishes 3-hourly leads through f072 on a
908x960 rotated grid. Its current naming table and live directories reach f072
even though one introductory directory bullet still says f048; cadence tests
must follow observed complete roles rather than that stale bullet. The bounded
`20260814T00Z_MSC_REPS_TMP-Prob_AGL-2m_RLatLon0.09x0.09_PT024H.grib2`
fixture is 4,602,451 bytes, ETag `"463a53-658f9779ad240"`, and SHA-256
`1cb07165788e942d1c3a9b6680078f54b494f16243032157ae5f5c00b5952259`.
Direct object:
`https://dd.weather.gc.ca/today/ensemble/reps/10km/grib2/00/024/20260814T00Z_MSC_REPS_TMP-Prob_AGL-2m_RLatLon0.09x0.09_PT024H.grib2`.
Its nine messages are provider statistics: percentiles 10/25/50/75/90 in
product definition template 6, followed by spread, unweighted mean, minimum,
and maximum in template 2, each describing 20 perturbed forecasts. The paired
9,108,868-byte raw-member object has ETag `"8afd84-658f994cfa0c0"` and SHA-256
`c70292b8829c83721395fa525853f0b94f10602467601974b68e2846bc938926`
and contains the control plus perturbations 1-20. It is a member-contract
fixture, not permission to expose raw members early.
Its direct URL is
`https://dd.weather.gc.ca/today/ensemble/reps/10km/grib2/00/024/20260814T00Z_MSC_REPS_TMP_AGL-2m_RLatLon0.09x0.09_PT024H.grib2`.

CAPS runs at 00/12Z with hourly f000-f048 atmospheric objects on a 2230x1830
rotated grid. It is explicitly experimental and coupled to ocean and ice; the
first RWS lane is atmospheric GRIB2 only. The f000 2 m air-temperature object
`20260814T00Z_MSC_CAPS_AirTemp_AGL-2m_RLatLon0.03_PT000H.grib2` is
1,268,084 bytes, ETag `"135974-658fba778a080"`, and SHA-256
`8e6a8a6eaea0ec75b56595f549566c545ad8397e2edef3ac55ef8a869fad54b9`.
Direct object:
`https://dd.weather.gc.ca/today/model_caps/3km/00/000/20260814T00Z_MSC_CAPS_AirTemp_AGL-2m_RLatLon0.03_PT000H.grib2`.
Its GRIB production-status key is experimental. The geometry fixture must also
prove rotated-grid coordinates and vector-wind rotation rather than trusting
unrotated corner labels.

RAQDPS publishes deterministic hourly f000-f072 surface concentrations and
separate entire-atmosphere integrals. The f001 wildfire-smoke PM2.5 object
`20260814T00Z_MSC_RAQDPS_PM2.5-WildfireSmokePlume_Sfc_RLatLon0.09_PT001H.grib2`
is 98,069 bytes, ETag `"17f15-658f9c155f180"`, and SHA-256
`74a0629afe020a887dbd1f76ba43b44e4cc89874c0516170fc22481bdfd8d496`.
Direct object:
`https://dd.weather.gc.ca/today/model_raqdps/10km/grib2/00/001/20260814T00Z_MSC_RAQDPS_PM2.5-WildfireSmokePlume_Sfc_RLatLon0.09_PT001H.grib2`.
It is one message on a 729x599 rotated grid. Its provider-local
`constituentType=62026` is unresolved by stock ecCodes even though the filename
and ECCC inventory identify wildfire-smoke PM2.5; the adapter must pin ECCC's
official mapping and fail closed on every unmapped code. The observed message
also reports a 2 m height-above-ground fixed surface despite `Sfc` in the
filename; fixture the decoded vertical support and never route by filename
alone.

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

The first RWS lane is now implemented and live verified at one bounded lead:
57 identified statistics were written bit-exactly and passed deep-store
validation. The scheduler fetches one lead object at a time and exposes the
typed `provider_statistics_only` limitation. Message-range acquisition remains
a future bandwidth optimization, not a prerequisite for bounded operation.

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

### Copernicus Atmosphere Monitoring Service (CAMS)

The global forecast is produced by **CAMS/ECMWF**. The European forecast and
its median are produced by the **CAMS regional production consortium** and
published centrally by ECMWF. For both, the licensing publisher is the
**European Union represented by ECMWF**. Direct transport is the Atmosphere
Data Store (ADS); its backing ECMWF object-store host is only transport.

- Global dataset:
  <https://ads.atmosphere.copernicus.eu/datasets/cams-global-atmospheric-composition-forecasts>
- Global STAC collection:
  <https://ads.atmosphere.copernicus.eu/api/catalogue/v1/collections/cams-global-atmospheric-composition-forecasts>
- Global technical documentation:
  <https://confluence.ecmwf.int/pages/viewpage.action?pageId=347605172>
- European dataset:
  <https://ads.atmosphere.copernicus.eu/datasets/cams-europe-air-quality-forecasts>
- European STAC collection:
  <https://ads.atmosphere.copernicus.eu/api/catalogue/v1/collections/cams-europe-air-quality-forecasts>
- Retrieve endpoint template:
  `https://ads.atmosphere.copernicus.eu/api/retrieve/v1/processes/{dataset}`
- Cost endpoint template:
  `https://ads.atmosphere.copernicus.eu/api/retrieve/v1/processes/{dataset}/costing`

Retrieval requires a free ADS account, accepted terms, and an API token. STAC,
form, constraint, process-schema, and costing endpoints are anonymous, so an
adapter can validate and budget a credential-stripped request before execution.
Never persist the token or an expiring result URL. Both current STAC records
declare CC BY 4.0. The Copernicus licence permits worldwide reproduction,
distribution, and adaptation for any lawful purpose but requires the applicable
`Generated using` or `Contains modified Copernicus Atmosphere Monitoring
Service information [Year]` notice plus the liability disclaimer.

The global forecast is provider-interpolated to a regular 0.4-degree grid,
runs at 00/12Z, and reaches 120 hours. Surface fields are hourly; multi-level
availability is constrained by variable, pressure/model level, and lead. It
includes more than 50 chemical species, seven aerosol families, and selected
meteorological fields. Persist surface concentration, dry-air mole fraction,
mass mixing ratio, optical depth, and column products as different roles. The
ADS catalogue spans 2015 to present; the current form warns that dates older
than 30 days are slow, tape-backed retrievals.

The global form is 93,863 bytes, SHA-256
`97f7eee229c6535a5edcf71666f0596cbdbe4c04bd77e4cc6f61280683cc12e7`;
its 886,338-byte constraint graph has SHA-256
`d5823ffab0ba80a391dbf9fa1d457bce00d62538f6731f6747735976e9a196c4`.
Exact form and constraint URLs are
`https://object-store.os-api.cci2.ecmwf.int:443/cci2-prod-catalogue/resources/cams-global-atmospheric-composition-forecasts/form_97f7eee229c6535a5edcf71666f0596cbdbe4c04bd77e4cc6f61280683cc12e7.json`
and
`https://object-store.os-api.cci2.ecmwf.int:443/cci2-prod-catalogue/resources/cams-global-atmospheric-composition-forecasts/constraints_d5823ffab0ba80a391dbf9fa1d457bce00d62538f6731f6747735976e9a196c4.json`.
An anonymous cost request for `particulate_matter_2.5um`, 2026-08-13 00Z,
f000, forecast, GRIB, and area `[1,0,0,1]` returned the 40-byte response
`{"id":"size","cost":1.0,"limit":10000.0}`, SHA-256
`7ad15aee938bbf976aa2a315d50cc7022208db7a35c02f1e5b8ff14e4f9978e8`.
Fixture the exact current process-schema keys, including `data_format`, rather
than copying obsolete client examples that use `format`.

The European product is a daily, hourly, 96-hour forecast on a 0.1-degree grid
over 25W-45E and 30N-72N, with surface and 50/100/250/500/750/1000/2000/3000/
5000 m levels. Eleven regional systems contribute to the provider-published
median. Only NO, NO2, SO2, ozone, PM2.5, PM10, and dust are regularly validated;
advertise other variables as experimental. The first adapter selects
`model=ensemble` only. Individual systems are not exchangeable perturbations
and require a durable model-member identity before support. The archive rolls
over three years. The provider targets 06:45 UTC availability for f000-f048 and
08:30 UTC for f049-f096; completeness must be role-based across that split.

The European form is 11,969 bytes, SHA-256
`96247c7d47e29c46b4063e465e7a2e5ccbd28b35749df5da41b1c9b2938ef40a`;
its 75,565-byte constraint graph has SHA-256
`61b62d0a522e5bdc30f4303465e9f5f2b51a84319366f371858433a0794f0f49`.
Exact form and constraint URLs are
`https://object-store.os-api.cci2.ecmwf.int:443/cci2-prod-catalogue/resources/cams-europe-air-quality-forecasts/form_96247c7d47e29c46b4063e465e7a2e5ccbd28b35749df5da41b1c9b2938ef40a.json`
and
`https://object-store.os-api.cci2.ecmwf.int:443/cci2-prod-catalogue/resources/cams-europe-air-quality-forecasts/constraints_61b62d0a522e5bdc30f4303465e9f5f2b51a84319366f371858433a0794f0f49.json`.
An anonymous cost request for ensemble PM2.5 at level 0, 2026-08-13 00Z,
f000, forecast, GRIB, and area `[31,-24,30,-23]` returned the 39-byte response
`{"id":"size","cost":1.0,"limit":5000.0}`, SHA-256
`2fb9a946e02e4b456d9c2624ab3d37cea9f91d02c874f493fed2d09318cb0229`.
The costing result is a request-unit budget, not a byte estimate. Before
downloading, require the authenticated job's returned asset `file:size` to fit
the response/decompression budget; this survey deliberately used no account
and therefore does not claim a payload byte count.

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
hosts; the public AWS S3 bucket is a mirror. None is a separate data owner.

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
- GEOS-CF overview: <https://gmao.gsfc.nasa.gov/gmao-products/geos-cf/>
- GEOS-CF data access: <https://gmao.gsfc.nasa.gov/gmao-products/geos-cf/data-access_geos-cf/>
- GEOS-CF v2 forecast OPeNDAP root:
  <https://opendap.nccs.nasa.gov/dods/gmao/geos-cf/v2/fcst>
- GEOS-CF latest public Zarr mirror:
  `s3://smce-geos-cf-public/geos-cf-v2-fcst-latest.zarr/`

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

GEOS-CF v2 is a separate research composition forecast, not another GEOS-FP
grid alias. It publishes global 1440x721 regular 0.25-degree collections from a
daily forecast extending five days. The surface air-quality collection has 120
hourly averages; chemistry and meteorology are also published on 23 pressure
levels, and other collections have model-level or 15-minute products. GMAO says
v2 begins 2025-08-04. The public `aqc` forecast history is retained, while most
other forecast collections retain only the latest 14 days.

Use a dated dataset for numerical fixtures, never the mutable `.latest` alias.
For `aqc_tavg_1hr_glo_L1440x721_slv.20260813_09z`, the 1,572-byte DDS has
SHA-256
`740124d098c80f92a98500952d84f68a0a7ea70b01ea22ba4651c291950d9b9f`
and the 2,918-byte DAS has SHA-256
`322d6e050503928771e53d554f6d36fbf33db5fc5eb1e1d00ac9c5d9ba9c64be`.
The bounded OPeNDAP projection
`?pm25_rh35[0:1:0][0:1:0][360:1:361][720:1:721]` is 164 bytes, SHA-256
`8c38fe2e601d1c5c4ae04b374b38bc457ebc2206e6ae519fdc061aa1cb22f6b4`,
and returns four values at latitudes 0/0.25 and longitudes 0/0.25. Its first
time is 09:30Z, not the 09Z run label. The DAS defines `pm25_rh35` as PM2.5
mass including water at 35 percent relative humidity in micrograms per cubic
metre; preserve that humidity basis in the canonical identity.
The exact projection URL is
`https://opendap.nccs.nasa.gov/dods/gmao/geos-cf/v2/fcst/aqc_tavg_1hr_glo_L1440x721_slv/aqc_tavg_1hr_glo_L1440x721_slv.20260813_09z.ascii?pm25_rh35[0:1:0][0:1:0][360:1:361][720:1:721]`.

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

### Brazil CPTEC/INPE WRF and BRAMS

Producer and licensing publisher: **Instituto Nacional de Pesquisas
Espaciais (INPE), Centro de Previsão de Tempo e Estudos Climáticos (CPTEC)**.
Transport is CPTEC's `dataserver.cptec.inpe.br`; this is not a third-party
mirror.

- INPE open-data page:
  <https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/dados-abertos>
- INPE 2025-2027 open-data plan:
  <https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/repositorio-de-arquivos/pda_inpe_25_27_v3_defesoeleitoral2026.pdf>
- Brazilian Open Data Policy, Decreto 8.777/2016:
  <https://www.planalto.gov.br/ccivil_03/_ato2015-2018/2016/decreto/d8777.htm>
- WRF 7 km cycle template:
  `https://dataserver.cptec.inpe.br/dataserver_modelos/wrf/ams_07km/brutos/{YYYY}/{MM}/{DD}/00/`
- BRAMS 8 km cycle template:
  `https://dataserver.cptec.inpe.br/dataserver_modelos/brams/ams_08km/brutos/{YYYY}/{MM}/{DD}/00/`

Authentication is not required. The current INPE open-data plan identifies
the WRF South America 7 km and BRAMS South America 8 km bases as open and
daily. Under Decreto 8.777, an open federal dataset permits free use and reuse,
subject to source credit. This dataset basis is distinct from the copyright
notice on the surrounding `gov.br` page.

Both directories retain dated cycles and publish one GRIB2 file per hourly
lead with `.inv`, `.grib2.idx`, and `.ctl` sidecars. Byte ranges are supported.
One observed WRF f000 object was 156,025,368 bytes; 181 leads make an
unfiltered cycle roughly 34 GiB. Its control file declares a 1019x1081 regular
0.07-degree grid and 25 pressure levels from 1000 to 50 hPa. One observed
BRAMS f000 object was 144,055,896 bytes; its 978x1009 grid uses different
longitude and latitude increments and the same 25 pressure levels. BRAMS also
publishes f-003 through f-001; those are pre-analysis files, not forecast
leads. Build the adapter from the record sidecars and reject a plan that falls
back to whole-cycle transfer.

### GeoSphere Austria C-LAEF

Producer, licensing publisher, and direct API/bulk transport operator:
**GeoSphere Austria**.

- C-LAEF deterministic dataset:
  <https://data.hub.geosphere.at/en/dataset/nwp-v2-1h-2500m>
- Deterministic metadata/API:
  <https://dataset.api.hub.geosphere.at/v1/grid/forecast/nwp-v1-1h-2500m/metadata>
  and
  `https://dataset.api.hub.geosphere.at/v1/grid/forecast/nwp-v1-1h-2500m`
- Deterministic bulk listing:
  <https://public.hub.geosphere.at/public/datahub.html?id=nwp-v1-1h-2500m/filelisting>
- C-LAEF ensemble-statistics dataset:
  <https://data.hub.geosphere.at/en/dataset/ensemble-v2-1h-2500m>
- Ensemble metadata/API:
  <https://dataset.api.hub.geosphere.at/v1/grid/forecast/ensemble-v1-1h-2500m/metadata>
  and
  `https://dataset.api.hub.geosphere.at/v1/grid/forecast/ensemble-v1-1h-2500m`
- Ensemble bulk listing:
  <https://public.hub.geosphere.at/public/datahub.html?id=ensemble-v1-1h-2500m/filelisting>

Authentication is not required. Both datasets are CC BY 4.0. The
deterministic dataset has DOI `10.60669/jft1-g709`; the ensemble-statistics
dataset has DOI `10.60669/c1by-wh34`. The forecast API can return targeted
NetCDF or GeoJSON selected by parameter, bounding box, time, and forecast
offset. A one-parameter, one-cell, one-time NetCDF request was about 27 KB.
The API is currently marked prerelease and exposes only a small rolling set of
cycles, so discovery must not assume durable history.

These are provider-generated regular WGS84 products at approximately 2.5 km,
interpolated from native 1 km C-LAEF. Store that regridding provenance rather
than calling them native 2.5 km model output. The deterministic product runs
every three hours and is hourly through f060. The ensemble product runs at
00/12Z and exposes p10, p50, and p90 for 13 logical fields from 16 perturbed
runs plus the control; it does not expose raw members. The provider warns of a
model/API transition in early 2027, so pin collection generation, parameter
metadata, and geometry rather than relying only on the legacy API identifier.

### MeteoGalicia WRF

Producer and licensing publisher: **MeteoGalicia, Xunta de Galicia**. Direct
transport is MeteoGalicia's THREDDS service.

- Xunta open-data record and CC BY-SA 4.0 terms:
  <https://abertos.xunta.gal/catalogo/medio-abiente/-/dataset/0485/servidor-thredds-meteogalicia>
- Official THREDDS usage manual:
  <https://meteo-estaticos.xunta.gal/datosred/infoweb/numerico/thredds/Manual_uso_Thredds.pdf>
- Root catalogue:
  <https://thredds.meteogalicia.gal/thredds/catalog/catalog.xml>
- Deterministic file catalogues:
  `https://thredds.meteogalicia.gal/thredds/catalog/wrf_2d_{36km|12km|04km}/fmrc/files/catalog.xml`
- Raw-ensemble file catalogues:
  `https://thredds.meteogalicia.gal/thredds/catalog/wrf_ens_2d_{12km|04km}/fmrc/files/catalog.xml`

Authentication is not required. THREDDS exposes OPeNDAP, HTTP, WCS, NCSS,
and WMS access; use OPeNDAP projection for bounded canonical acquisition. The
three deterministic Lambert grids are 118x104 at 36 km, 171x138 at 12 km, and
117x126 at 4 km. They run at 00/12Z with hourly output from f001 through f096
at 00Z and f084 at 12Z. Current whole files are roughly 77-163 MB, while a
one-scalar OPeNDAP projection was 324 bytes. The public inventory contains
surface and derived fields plus only a small fixed upper-air subset; it is not
a full pressure-level or model-level state.

The 12 km and 4 km ensemble grids run daily at 00Z with 21 array members and
hourly output to f216. Whole files are approximately 7.4 GB and 4.5 GB, but
OPeNDAP can select a field, member, time, and cell. Dataset history refers to
members `m00` through `m20`, yet the files do not expose an authoritative
ensemble coordinate or control/perturbation mapping. Keep both raw-member
lanes gated until MeteoGalicia documents that contract; never infer that the
first array member is the control. Observed rolling catalogue depths are
fixtures, not a retention promise.

### Google DeepMind WeatherNext 2 historical data

Producer: **Google**, through the WeatherNext catalogue and an operational
Google DeepMind model. The historical licensing publisher is
**Google/WeatherNext**; its required attribution names DeepMind Technologies
Limited. For current data, the contracting licensor is Google Ireland Limited
in the EEA/Switzerland and Google LLC elsewhere. Google Cloud Storage,
BigQuery, and Google Earth Engine are transports, not independent producers.
Use stable producer identity `google-weathernext` and persist the exact
publisher, citation, third-party acknowledgements, and terms version.

- Official dataset catalogue:
  <https://developers.google.com/earth-engine/datasets/catalog/projects_gcp-public-data-weathernext_assets_weathernext_2_0_0>
- Provider-published ensemble mean:
  <https://developers.google.com/earth-engine/datasets/catalog/projects_gcp-public-data-weathernext_assets_weathernext_2_0_0_mean>
- BigQuery access guide:
  <https://developers.google.com/weathernext/guides/bigquery>
- Current-data terms:
  <https://storage.googleapis.com/weathernext-public/terms-of-use.pdf>
- Historical Zarr roots after access approval:
  `gs://weathernext/weathernext_2_0_0/zarr` and
  `gs://weathernext/weathernext_2_0_0_mean/zarr`

WeatherNext 2 is global at 0.25 degree, initializes at 00/06/12/18Z, and has
64 members at six-hour steps through 15 days. It includes surface variables
and temperature, humidity, winds, vertical velocity, and geopotential at 13
pressure levels. The publisher also provides a 64-member mean. BigQuery,
Earth Engine, and the raw historical Zarr path require a Google Cloud project
and the provider's data-request/subscription flow.

Only data older than 48 hours belong in this public backlog: Google publishes
that historical portion under CC BY 4.0. The separate current-data terms are
revocable and non-transferable and prohibit public sharing of raw/unmodified
data; subsetting or format conversion alone does not make those bytes freely
redistributable. A public RWS adapter must therefore enforce a server-side
age gate, fail closed around the boundary, and never advertise a current or
`latest` run. Start with the official mean, then add raw members only after
the ensemble-member contract and bounded Zarr/BigQuery acquisition are ready.

### NOAA/NCEP CFSv2 and Air Quality Model

Producer/licensor: **NOAA National Centers for Environmental Prediction**.
Real-time transport is NCEP NOMADS; long-term archive transport is NOAA NCEI.

- Official overview:
  <https://www.ncei.noaa.gov/products/weather-climate-models/climate-forecast-system>
- Real-time products: <https://www.nco.ncep.noaa.gov/pmb/products/cfs/>
- Real-time template:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/cfs/prod/cfs.{YYYYMMDD}/{HH}/6hrly_grib_{01|02|03|04}/`
- NCEI archive:
  <https://www.ncei.noaa.gov/data/climate-forecast-system/access/operational-9-month-forecast/>
- AQM/CMAQ product inventory:
  <https://www.nco.ncep.noaa.gov/pmb/products/aqm/>
- AQM current-cycle template:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aqm/prod/aqm.{YYYYMMDD}/{HH}/`

Authentication is not required. Each GRIB2 object has an `.idx` companion and
supports range selection. Representative current object sizes are about 4.2
MB for one `flxf` valid time, 23.9 MB for `pgbf`, and 5.2 MB for `ipvf`, before
multiplying by four members and the long 6-hourly horizon. The live NOMADS root
holds roughly a week; NCEI provides the durable archive. Ocean output should be
a later, separate canonical-domain effort.

AQM is NOAA/NWS/NCEP's operational CMAQ-based deterministic composition
forecast; NOMADS is transport, not owner. Its raw one-hour PM2.5 and ozone
files are separate 72-message GRIB2 aggregates on provider grids 227 (CONUS
Lambert), 198 (Alaska polar stereographic), and 196 (Hawaii Mercator). The
current listing publishes 06/12Z cycles even though the generic inventory page
shows four cycle placeholders; fixture observed cadence and do not advertise a
cycle merely because a URL template permits it. The live directory has no
`.idx`, but objects accept byte ranges. Inventory each selected aggregate once,
cache exact message offsets, and distinguish raw fields from bias-corrected,
8/24-hour, and maximum products. The observed NOMADS AQM root held only two
dates; treat it as volatile current transport and retain immutable fixtures.

Three 2026-08-13 12Z `ave_1hr_pm25` objects provide bounded domain fixtures:

- Grid 196 is 1,837,008 bytes with 72 messages, SHA-256
  `551e3df97df5618b6cceff4e4de0dc6c6201069f83af33ba9736db2e7b214e4f`.
  Direct object:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aqm/prod/aqm.20260813/12/aqm.t12z.ave_1hr_pm25.196.grib2`.
  It is a 321x225 Mercator grid at 2,500 m. Byte range `0-23875` is the first
  complete message, SHA-256
  `4b569cad155b402c70b5281abc72e2ab30622f10272025a8527782d4cb8510ad`.
- Grid 198 is 18,452,903 bytes with 72 messages, SHA-256
  `ce3056d4cc9af7c77835a1ca07b9599931ff0f532b823ece138feb5742e05dd1`.
  Direct object:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aqm/prod/aqm.20260813/12/aqm.t12z.ave_1hr_pm25.198.grib2`.
  It is an 825x553 polar stereographic grid at 5,953 m. Byte range `0-243774`
  is the first message, SHA-256
  `67f70cc75a72a432f44e242cfee5b01e430a891b2ebbd3a723de0fe13c897b9e`.
- Grid 227 is 62,592,411 bytes with 72 messages, SHA-256
  `149334195d267c7d0d0315de08db3699aba8a152d196f1915cbc0c6f2277f0ef`.
  Direct object:
  `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aqm/prod/aqm.20260813/12/aqm.t12z.ave_1hr_pm25.227.grib2`.
  It is a 1473x1025 Lambert grid at 5,079 m. Byte range `0-833245` is the first
  message, SHA-256
  `5de8cad0659ec29d92d39cf56b4b5d2c78c501c6abc200064f6a18be21d3233a`.

Each fixture's 72 messages describe exact one-hour average intervals from 0-1
through 71-72 hours. Preserve those bounds and the native PM mass-concentration
units rather than treating the fields as instantaneous valid-time snapshots.

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

### Czech Hydrometeorological Institute (CHMI) ALADIN

Producer and licensing publisher: **Czech Hydrometeorological Institute
(CHMI/CHMU)**. Transport is CHMI's `opendata.chmi.cz` host.

- Official model root:
  <https://opendata.chmi.cz/meteorology/weather/nwp_aladin/>
- CZ1K cycles:
  `https://opendata.chmi.cz/meteorology/weather/nwp_aladin/CZ_1km/{00|06|12|18}/`
- Lambert cycles:
  `https://opendata.chmi.cz/meteorology/weather/nwp_aladin/Lambert_2.3km/{00|06|12|18}/`
- Official content workbook:
  <https://opendata.chmi.cz/meteorology/weather/nwp_aladin/Popis_obsahu.xlsx>
- Official open-data terms:
  <https://www.chmi.cz/-/jak-mohu-pou%C5%BE%C3%ADvat-otev%C5%99en%C3%A1-data-%C4%8Dhm%C3%BA->

Authentication is not required. Both feeds are bzip2-compressed GRIB1 split by
variable and cycle, so HTTP byte ranges cannot select a GRIB message inside a
compressed object. The 2026-08-14 listing retained two or three runs per cycle,
roughly 48-72 hours. The newest CZ1K run had 31 objects totaling 230,578,224
compressed bytes; the Lambert run had 154 totaling 8,132,319,060 bytes. The
workbook defines CZ1K as a surface/derived product and Lambert as a larger
native computational-domain product with 17 pressure levels.

The bounded CZ1K precipitation-type fixture is
`ALADCZ1K4opendata_2026081400_PRECIP_TYPE.grb.bz2`: 50,465 compressed bytes,
SHA-256 `fdd24802551b200d9303a12cffafea1e664f28761963bb5d991bb102bb2a1b75`;
20,978,496 decompressed bytes, SHA-256
`1391cde07f96161c5a8abd37cb9e3b308058bc69383ffe30b7e664f01b46e625`.
It contains 72 messages on a 501x290 regular grid and uses CHMI local parameter
230. The provider's `gribtab` and `grib2table` are part of the decoding
contract. A compact Lambert geometry fixture is
`ALADLAMB4opendata_2026081400_SURFIND_TERREMER.grb.bz2`: one 1053x837 Lambert
message, 13,019 compressed bytes, SHA-256
`e86f810db630bcf9bb72107b487aa2104e46ed6e398a86bb1088d5b4bd756c7a`.

### Slovenian Environment Agency (ARSO) ALADIN

Producer and licensing publisher: **Slovenian Environment Agency (ARSO)**.
Transport is ARSO's `meteo.arso.gov.si` host.

- Official technical description:
  <https://meteo.arso.gov.si/uploads/meteo/help/sl/NumericniRezultatiGRIB.html>
- Direct model directory:
  <https://meteo.arso.gov.si/uploads/probase/www/model/data/>
- Official reuse statement:
  <https://meteo.arso.gov.si/uploads/meteo/help/en/copyright.html>

Authentication is not required. ARSO documents a 4 km Slovenia-domain ALADIN
feed, refreshed every six hours, with f000-f072 hourly and a rolling 24-hour
window. Each cycle is one ZIP with 73 GRIB1 files. The 2026-08-14 00Z package
was 29,960,273 bytes, SHA-256
`d98f7fb4d7516d29befaf5250989d0ff53b80968fda95022ee6e7529a903f7f4`;
the archive expands to 34,403,440 bytes. Its f000 file contains 28 messages on
a 111x71 regular grid, including a surface suite and 925/850/700/500 hPa
fields. ZIP byte ranges are not a safe per-entry acquisition contract, but the
whole package is small enough for an explicitly budgeted transfer. Preserve
the f072 total-precipitation interval. Public reuse requires the exact source
credit `Source: ARSO`.

### ARPAE-SIMC ICON-2I

Producer: **ARPAE-SIMC under the LAMI operational arrangement with the Italian
Air Force Meteorological Service and ARPA Piemonte**. Licensing publisher:
**ARPAE Emilia-Romagna**. Transport is `dati-simc.arpae.it`; do not derive the
producer from the GRIB centre identifier.

- Official catalogue:
  <https://dati.arpae.it/it/dataset/previsioni-meteorologiche-numeriche-emilia-romagna>
- Machine-readable catalogue record:
  <https://dati.arpae.it/api/3/action/package_show?id=previsioni-meteorologiche-numeriche-emilia-romagna>
- Direct cycle directory: <https://dati-simc.arpae.it/opendata/icon_2I/>

The catalogue declares Creative Commons Attribution (`cc-by`, version not
specified), describes ICON-2I over Italy at about 2.2 km, and publishes a
regular-grid crop over Emilia-Romagna at 00/12Z through f072. Authentication is
not required and the live directory retained about three days. The 2026-08-14
00Z object was 176,815,667 bytes, SHA-256
`38a8a877cab16ede8fdadf10722b2f833cb4ed52054ab11e8503879261e09cd4`.
It contains 7,521 GRIB2 messages for 73 leads on a 153x81 grid, including six
pressure levels. The host supports byte ranges but publishes no sidecar index;
perform one bounded inventory scan, cache exact message offsets, and keep
unresolved local fields unknown rather than mapping by resemblance.

### Royal Meteorological Institute of Belgium (RMI/KMI) ALARO

Producer and licensing publisher: **Royal Meteorological Institute of Belgium
(RMI/KMI)**. Direct transport is `opendata.meteo.be`; the metadata also names
`opendata24-me.oma.be` as an FTP transport, not a different producer.

- Official ISO metadata:
  <https://opendata.meteo.be/geonetwork/srv/eng/csw?request=GetRecordById&service=CSW&version=2.0.2&resultType=results&outputSchema=http://www.isotc211.org/2005/gmd&elementSetName=full&id=RMI_DATASET_ALARO>
- Documentation: <https://opendata.meteo.be/documentation/?dataset=alaro&lang=en>
- Direct run directory: <https://opendata.meteo.be/ftp/forecasts/alaro_40l/>
- WCS capabilities:
  <https://opendata.meteo.be/service/alaro/wcs?service=WCS&version=1.1.1&request=GetCapabilities>

The official metadata declares CC BY 4.0 and a 4 km ALARO domain. Anonymous
run directories publish 00/06/12/18Z, f000-f060 hourly, with about 24 hours of
retention. A current run contained 34 per-variable GRIB1 files totaling
379,572,710 bytes. The bounded 2 m temperature fixture was 2,871,758 bytes,
SHA-256 `37e69437fb6e9fbe34cbe3deb8159f838a1fe99e91b9912507640bcbc50a8261`,
and contains 61 messages on a 177x177 grid. The temperature file has 15
pressure levels from 1000 through 100 hPa. Local tables such as
`params227-1.tab` and `params227-228.tab` are mandatory: stock GRIB concepts can
misclassify precipitation timing. If WCS is used as a bounded bridge, prove
numerical and interval equivalence to the raw GRIB1 source.

### UWC-West DINI-EPS through Met Eireann

Producer: **UWC-West/DINI collaboration of Met Eireann, DMI, the Icelandic Met
Office, and KNMI**. Licensing publisher: **Met Eireann**. Transport is
`opendata.met.ie`; `opendata2.met.ie` is a second Met Eireann host, not a
different producer.

- Official high-value dataset record:
  <https://data.gov.ie/dataset/numerical-weather-prediction-data/resource/1fefad43-8aa9-46cb-88f6-053e712cdafa>
- Portal documentation: <https://opendata.met.ie/documentation>
- Official collaboration description:
  <https://www.met.ie/joining-forces-in-weather-forecasting-and-climate-research>
- Near-real-time listing template:
  `https://opendata.met.ie/data-portal/near-realtime/nwp?from={ISO}&to={ISO}`
- Download template:
  `https://opendata.met.ie/data-portal/near-realtime/download/nwp?files={files}`

The public near-real-time API is anonymous but restricts listing queries to a
small recent window; older archive preparation requires registration. Met
Eireann documents the current DINI-EPS as HARMONIE-AROME 43h2.2.1 on 2 km
Lambert grids with 90 vertical levels and hourly cycling. The control reaches
f060; five perturbed members are delivered each hour through f054, so the
31-member lagged ensemble spans six reference times. Products are GRIB2 with
CCSDS packing and several nested domains/field suites.

The bounded `fc2026081406+000grib2_enIoI` download was a 6,377,967-byte ZIP,
SHA-256 `71e76a8e3986391e5d701daaeea6487189b4ffcf6de88331dbfdb24e14f89b22`.
Its single 6,576,520-byte GRIB2 member has SHA-256
`32c76bc5815d31fffb1d5ea5d54b45785002993311cd3fa0f1c984257029d1c5`,
64 messages, a 229x386 Lambert grid, `perturbationNumber=1`, and
`numberOfForecastsInEnsemble=31`. The listing can repeat a filename for
different members, so validate the GRIB ensemble metadata after every download
and persist the original reference time. The public record and portal declare
CC BY 4.0 for Met Eireann's distributed product.

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
| `cams-ecmwf-global` via `copernicus-eu` | CAMS STAC declares CC BY 4.0; the Copernicus licence permits worldwide reproduction, distribution, adaptation, and commercial use | Persist CAMS/ECMWF as producer, European Union represented by ECMWF as licensing publisher, the applicable `Generated using` or `Contains modified Copernicus Atmosphere Monitoring Service information [Year]` notice, and the required EC/ECMWF liability disclaimer | [global STAC](https://ads.atmosphere.copernicus.eu/api/catalogue/v1/collections/cams-global-atmospheric-composition-forecasts) and [Copernicus licence](https://ads.atmosphere.copernicus.eu/licences/licence-to-use-copernicus-products) |
| `cams-regional-ensemble` via `copernicus-eu` | The European forecast STAC declares the same CC BY 4.0 terms | Persist the CAMS regional production consortium as producer, European Union represented by ECMWF as licensing publisher, the CAMS notice, year, modification status, and disclaimer | [European STAC](https://ads.atmosphere.copernicus.eu/api/catalogue/v1/collections/cams-europe-air-quality-forecasts) and [Copernicus licence](https://ads.atmosphere.copernicus.eu/licences/licence-to-use-copernicus-products) |
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
| `inpe-cptec` | INPE's current plan marks WRF 7 km and BRAMS 8 km open; Brazil's federal open-data definition permits free reuse subject to source credit | `INPE/CPTEC`, source URL, plan/version, and modification notice | [INPE open-data plan](https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/repositorio-de-arquivos/pda_inpe_25_27_v3_defesoeleitoral2026.pdf) and [Decreto 8.777/2016](https://www.planalto.gov.br/ccivil_03/_ato2015-2018/2016/decreto/d8777.htm) |
| `geosphere-austria` | C-LAEF deterministic and ensemble-statistics datasets are CC BY 4.0 | GeoSphere Austria, dataset DOI, licence link, provider-regridding and change indication | [deterministic dataset](https://data.hub.geosphere.at/en/dataset/nwp-v2-1h-2500m) and [ensemble dataset](https://data.hub.geosphere.at/en/dataset/ensemble-v2-1h-2500m) |
| `meteogalicia` | MeteoGalicia THREDDS model results are CC BY-SA 4.0 | MeteoGalicia/Xunta de Galicia, source and licence links, change indication, and share-alike obligations | <https://abertos.xunta.gal/catalogo/medio-abiente/-/dataset/0485/servidor-thredds-meteogalicia> |
| `google-weathernext` historical only | WeatherNext 2 data older than 48 hours are CC BY 4.0; current data are excluded from the public adapter because separate terms restrict redistribution | Persist the exact WeatherNext 2 citation naming DeepMind Technologies Limited, CC BY link, third-party acknowledgements, source, age at acquisition, and modifications | [dataset catalogue](https://developers.google.com/earth-engine/datasets/catalog/projects_gcp-public-data-weathernext_assets_weathernext_2_0_0) and [current-data terms](https://storage.googleapis.com/weathernext-public/terms-of-use.pdf) |
| `noaa-ncep` | US federal NOAA data are generally public-domain; retain dataset-specific notices | NOAA/NCEP as producer and NOMADS or NCEI only as transport | <https://www.noaa.gov/disclaimer> |
| `meteoswiss` | STAC collections declare CC BY | Federal Office of Meteorology and Climatology MeteoSwiss, licence link, and change indication | The `license` and provider fields in the [ICON-CH1 collection](https://data.geo.admin.ch/api/stac/v1/collections/ch.meteoschweiz.ogd-forecasting-icon-ch1) |
| `knmi` | HARMONIE open data is CC BY 4.0 | KNMI, licence link, and change indication | <https://english.knmidata.nl/open-data/harmonie> |
| `chmi` | CHMI open data are published under CC BY 4.0 | Czech Hydrometeorological Institute, licence link, and change indication | <https://www.chmi.cz/-/jak-mohu-pou%C5%BE%C3%ADvat-otev%C5%99en%C3%A1-data-%C4%8Dhm%C3%BA-> |
| `arso` | The official public-data statement permits reuse of the published meteorological products | Preserve the exact credit `Source: ARSO` with the source URL | <https://meteo.arso.gov.si/uploads/meteo/help/en/copyright.html> |
| `arpae-simc` | The dataset catalogue declares Creative Commons Attribution; the record does not specify a version | ARPAE Emilia-Romagna as licensing publisher, ARPAE-SIMC/LAMI production provenance, catalogue URL, and change indication | <https://dati.arpae.it/it/dataset/previsioni-meteorologiche-numeriche-emilia-romagna> |
| `rmi-belgium` | ALARO metadata declares CC BY 4.0 | Royal Meteorological Institute of Belgium, licence link, and change indication | [official ISO metadata](https://opendata.meteo.be/geonetwork/srv/eng/csw?request=GetRecordById&service=CSW&version=2.0.2&resultType=results&outputSchema=http://www.isotc211.org/2005/gmd&elementSetName=full&id=RMI_DATASET_ALARO) |
| `uwc-west-dini` via `met-eireann` | Met Eireann's NWP dataset record declares CC BY 4.0 | UWC-West/DINI collaboration as producer, Met Eireann as licensing publisher, licence link, and change indication | <https://data.gov.ie/dataset/numerical-weather-prediction-data/resource/1fefad43-8aa9-46cb-88f6-053e712cdafa> |

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
  metadata, followed later by one `allmbrs` file. For REPS, assert all nine
  published-statistic messages and use the 21-message raw counterpart only as
  a disabled member-contract fixture. CAPS must prove 2230x1830 rotated
  geometry, wind rotation, and experimental status. RAQDPS must fixture an
  official constituent-code table plus negative unknown-code and
  surface-versus-column cases.
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
- CAMS: retain anonymous STAC, form, constraint-graph, process-schema, and cost
  responses, then one credential-stripped targeted-request manifest and tiny
  authenticated GRIB or NetCDF result. Global fixtures must distinguish
  concentration, mole fraction, mixing ratio, optical depth, and column roles.
  Europe must prove `model=ensemble` is the provider median and reject all 11
  individual system values until model-member identity exists.
- Met Office: anonymous ASDI listing plus NetCDF header/chunk metadata. Prove
  global regular-grid geometry, UKV/MOGREPS-UK LAEA metadata, MOGREPS-G's 18
  realizations, and MOGREPS-UK's three realizations/reference time without
  downloading a complete field or cycle.
- NASA GMAO: for GEOS-FP, retain OPeNDAP DDS/DAS, one tiny projected surface
  response, one pressure slice, live-vs-spec dimension assertion, and
  experimental-service stability metadata. For GEOS-CF, use a dated v2 DDS/DAS
  and tiny `pm25_rh35` projection; assert 1440x721 geometry, 120 half-hour-centred
  average times, RH35/water identity, and the 14-day non-`aqc` retention class.
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
- CPTEC/INPE: dated WRF and BRAMS listings plus `.inv`, `.grib2.idx`, and
  `.ctl` sidecars. Range-select one surface, pressure, vector-wind, and
  accumulation message; assert 1019x1081 versus 978x1009 geometry, unequal
  BRAMS increments, 25 pressure levels, and rejection of BRAMS negative leads.
- GeoSphere Austria: API metadata plus one-cell/one-time targeted NetCDF for
  deterministic and percentile collections. Assert bbox, cycle/offset,
  parameter generation, p10/p50/p90 identity, DOI/licence, provider regrid,
  rolling-cycle fallback, and the announced 2027 transition boundary.
- MeteoGalicia: THREDDS catalogue, DDS/DAS, and tiny OPeNDAP slices for all
  three deterministic grids. Assert Lambert mapping, two-dimensional corner
  and interior coordinates, cycle-dependent horizon, and public field limits.
  For ensemble files, assert that member/control semantics are unavailable and
  support remains disabled rather than guessing array order.
- WeatherNext 2 historical: credential-stripped BigQuery schema and dry-run
  with `maximumBytesBilled`, or Zarr metadata plus a bounded chunk selection;
  fixture the provider mean and its 64-member statistic. Test the greater-than-
  48-hour gate on both sides of the boundary, exact citation persistence, and
  rejection of every current/`latest` discovery path before raw members.
- CFSv2: `.idx` and selected message ranges from `flxf`, `pgbf`, and `ipvf`,
  with member and six-hour valid-time assertions.
- NOAA AQM: one live directory listing and the pinned first-message ranges from
  grids 196, 198, and 227. Assert all three projections/dimensions, 72 exact
  one-hour intervals, raw-versus-bias-corrected identity, PM units, and absence
  of `.idx`; never fetch all variables to discover one field.
- MeteoSwiss: STAC collection and item, parameter CSV, static geometry asset,
  and one bounded signed-asset range with the signature removed from fixtures.
- KNMI: API manifest, safe tar inventory, and the smallest legal GRIB1 sample;
  no support claim until edition-1 decode and rolling-member timing pass.
- CHMI: dated listings for both grids, the pinned compressed and decompressed
  hashes above, provider tables, CZ1K 501x290 and Lambert 1053x837 geometry,
  bzip2 ceilings, and a negative plan proving the adapter will not fetch the
  roughly 7.6 GiB Lambert cycle when only a sparse profile was requested.
- ARSO: one four-cycle listing, ZIP central-directory inventory, compressed and
  expanded ceilings, the 28-message f000 inventory, four pressure levels, and
  an f072 accumulation golden. Reject missing, duplicate, or traversal entries
  before extraction.
- ARPAE-SIMC: catalogue JSON plus live cycle listing, bounded range inventory
  of the 7,521-message GRIB2 object, 153x81 geometry, exact offsets for a small
  surface/pressure/accumulation suite, and a negative map for every unresolved
  local parameter.
- RMI ALARO: metadata and run listing, 177x177 geometry, the 15-level pressure
  suite, and precipitation decoded with the published local tables. If WCS is
  the acquisition path, compare its values, missing cells, units, and interval
  semantics against byte-identical roles from raw GRIB1.
- DINI-EPS: a minimal near-real-time listing window and one explicitly selected
  download. Assert GRIB `perturbationNumber`, forecast-count, reference time,
  CCSDS decode, Lambert geometry, and accumulation start; include a negative
  listing with duplicate filenames proving member routing never trusts the
  filename. Do not expand the request window or infer the full lagged ensemble
  until all expected reference-time/member roles have arrived.

## Watchlist and access gates

These feeds are not included in the 70-lane implementation count. Recheck them
periodically, but do not build a production adapter until the named gate is
closed with an official source and a live bounded fixture.

| Feed | Evidence | Why it is not in the active queue | Promotion gate |
| --- | --- | --- | --- |
| Roshydromet SL-AV global | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Aru-roshydromet%3Awmc-moscow.forecast.medium-range.deterministic.global.sl-av?f=html) | Metadata declares WMO core and documents SLAV10 output, but no current forecast objects were present at the source node during this survey. | Observe and pin one complete live cycle before calling the feed available. |
| Cyprus Department of Meteorology WRF | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Acy-dom%3Aweather.prediction.deterministic.local?f=html) and [direct directory](https://www.dom.org.cy/wis2/data/core/weather/prediction/forecast/short-range/deterministic/limited-area/) | The five overwrite-style GRIB2 products were last modified 2026-01-06 during this 2026-08-14 survey. | Resume only after fresh, regularly advancing timestamps are observed. |
| Italy MeteoAM limited-area forecast | [WIS2 discovery record](https://wis2-gdc.weather.gc.ca/collections/wis2-discovery-metadata/items/urn%3Awmo%3Amd%3Ait-meteoam%3Aforecast.short-range.deterministic.limited-area?f=html) | Metadata declares WMO core, but the advertised source endpoint repeatedly timed out and no bounded payload could be verified. | Pin a reachable official listing, retention, and GRIB fixture. |
| Australia Bureau of Meteorology ACCESS | [ACCESS NWP products](https://www.bom.gov.au/nwp/doc/access/NWPData.shtml), [copyright notice](https://www.bom.gov.au/copyright), and [data licence agreement](https://www.bom.gov.au/sites/default/files/2026-07/bureau-of-meteorology-data-licence-agreement-june-2026.pdf) | Operational model files use a Registered User/subscriber channel. Default Bureau terms do not establish unrestricted third-party or commercial redistribution. | Obtain and record a licence that covers RWS redistribution and automated access. |
| JMA GSM/MSM/LFM GPV | [official product catalogue](https://www.data.jma.go.jp/suishin/cgi-bin/catalogue/make_product_page.cgi?id=ZenModel), [JMBSC distribution](https://www.jmbsc.or.jp/en/index-e.html), and [official samples](https://www.data.jma.go.jp/developer/gpv_sample.html) | Operational GPV delivery is through the contracted/paid JMBSC service. Public sample files are suitable only as decoder fixtures and do not establish redistribution rights. | Establish an official operational access and redistribution contract; do not treat sample files as a live feed. |
| CPTEC/INPE BAM | [official anonymous directory](https://ftp.cptec.inpe.br/modelos/tempo/BAM/) and [INPE 2025-2027 open-data plan](https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/repositorio-de-arquivos/pda_inpe_25_27_v3_defesoeleitoral2026.pdf) | The current anonymous `singleLevel` GRIB2 tree is only a limited subset. The plan marks the base open but schedules complete global raw BAM grid output for June 2027. | Confirm the complete raw grid, inventory, and bounded transport after the scheduled opening; the remaining gate is technical completeness, not general INPE reuse permission. |
| CPTEC/INPE ETA South America 40 km and RJ/SP 1 km | [official ETA directory](https://ftp.cptec.inpe.br/modelos/tempo/Eta/) and [INPE 2025-2027 open-data plan](https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/repositorio-de-arquivos/pda_inpe_25_27_v3_defesoeleitoral2026.pdf) | Fresh anonymous GRIB1 is live at `ams_40km/brutos/` and `rjsp_01km/brutos/`; 2026-08-14 f000 objects were 10,462,260 and 59,857,266 bytes and accepted byte ranges. The same official plan schedules ETA's formal opening for July 2027, so discoverability is not yet a sufficient redistribution grant. The advertised 8 km archive path was also broken for current 2026 data. | Wait for the scheduled opening or obtain written terms tied to these exact forecast objects, then pin provider GRIB1 tables, grids, schedules, and a bounded range inventory. |
| Singapore MSS/ASMC smoke-haze dispersion model | [official WIS2 product record](https://wis2.asmc.asean.org/smoke-haze-dispersion-model/) and [live product API](https://z9ppn1a4nj.execute-api.ap-southeast-1.amazonaws.com/v1/collections/asmc_smoke_haze_dispersion/items/ASMC_SMOKE_HAZE_DISPERSION) | The record describes 3-hourly surface PM10 estimates through 24 hours, but the API returns only a 474,117-byte `GIF89a` animation (SHA-256 `0bdf91428bd7a55c462cbcf8204d0dbd259aaae6218ded99c3589ed0db4aa3a3`), not a machine-readable concentration grid. | Locate an official structured PM10 field with explicit reuse terms, or scope the GIF separately as a derived visualization rather than canonical model data. |
| AEMET HARMONIE-AROME packages | [official catalogue](https://datos.gob.es/en/catalogo/e05068001-datos-del-modelo-harmonie-arome) and [AEMET legal notice](https://www.aemet.es/es/nota_legal) | Public packages are selected derived GeoTIFF/GeoJSON surface products, not a canonical full NWP state. Reuse is allowed with attribution, but counting it as full model normalization would overstate semantics. | Add only as an explicitly derived-product lane, or locate an official full-field feed. |
| ICPAC WRF East Africa rainfall products | [official dataset API](https://floodwatch.icpac.net/api/datasets), [WRF total-rainfall metadata](https://floodwatch.icpac.net/api/metadata/2769c1e8-97cb-4144-a460-cfca2f97ce3f), and [WRF extreme-rainfall metadata](https://floodwatch.icpac.net/api/metadata/a3a96f87-a5b5-4fb1-9bd0-603959ef6b25) | The live public products are derived daily total/extreme-rainfall COG/tile layers, not the full WRF state, and both official metadata records have a null licence field. | Obtain an ICPAC reuse/redistribution statement tied to the products and either locate the raw model grid or define an explicitly derived-rainfall canonical lane. |
| Colombia IDEAM WRF | [official model portal](https://bart.ideam.gov.co/wrfideam/) and [live NetCDF directory](https://bart.ideam.gov.co/wrfideam/new_modelo/WRF00COLOMBIA/netcdf/) | Anonymous WRF NetCDF is live, but whole aggregates are tens of GB, the transport offers no observed server-side subset/index contract, and the portal's requested citation is not an explicit redistribution licence. | Tie an official commercial/redistribution licence to this exact dataset and prove a bounded variable/time acquisition path. |
| India NCMRWF NCUM/NEPS | [official NCMRWF model guidance](https://nwp.ncmrwf.gov.in/model-guidance) and [SWFDP product portal](https://nwp.ncmrwf.gov.in/HomePage/index.php) | Official guidance documents NCUM/NEPS, but the public portal exposes charts and derived forecast products rather than a licensed, machine-readable raw model feed. The separate research portal requires registration. | Pin an official raw-object/API contract, access terms, redistribution permission, and one bounded grid fixture. |
| HungaroMet AROME and WRF | [official NWP root](https://odp.met.hu/weather/nwp/), [AROME description](https://odp.met.hu/weather/nwp/AROME/Description_shortrange_forecast-AROME-en.pdf), [WRF description](https://odp.met.hu/weather/nwp/WRF/Description_shortrange_forecast-WRF-en.pdf), and [current terms](https://odp.met.hu/ODP_General_Term_of_Use.pdf) | Anonymous single-variable/single-lead ZIP-compressed NetCDF is live for four cycles daily: AROME to f048 on a 0.025-degree grid and WRF to f036 on a 499x394 grid. The current terms allow unmodified use but require prior written consent for modifications; RWS unit normalization and store-format conversion are modifications, so open download is not sufficient. | Obtain written permission covering automated normalization, adaptation, storage, and redistribution, or wait for revised terms. Persist the required source text `Database: Meteorological Database, HungaroMet Nonprofit Zrt.` |
| WeatherNext 2 current data (48 hours or newer) | [official catalogue](https://developers.google.com/earth-engine/datasets/catalog/projects_gcp-public-data-weathernext_assets_weathernext_2_0_0) and [current-data terms](https://storage.googleapis.com/weathernext-public/terms-of-use.pdf) | Current-data rights are revocable/non-transferable and prohibit public sharing of raw/unmodified data; subsetting or format conversion alone does not create an open redistribution route. This restriction does not apply to the separately queued historical slice older than 48 hours. | Obtain a Google agreement that expressly permits the proposed RWS current-data API, or keep the server-side age gate permanently fail-closed. |

## Deliberately deferred adjacent feeds

The same official catalogues expose ECCC CanSIPS/NAEFS and analysis systems,
CMA-CW dust and chemistry products, and multiple ocean, ice, wave, surge, and
climate-analysis products. They are valuable, but they should be researched as
separate canonical-domain lanes rather than being counted as atmospheric
weather-model support. Rows 64-70 are explicitly composition-domain lanes;
their presence does not imply a complete meteorological state. This prevents a
large catalogue number from hiding missing semantics in the store and API.

Open model weights or inference code are not themselves an operational public
forecast feed. GraphCast/GenCast, FourCastNet, Pangu-Weather, Aurora, and
similar research systems therefore remain outside the active count until a
provider publishes a continuously advancing, licensed output grid or RWS owns
a separately scoped inference service with licensed initial conditions,
weights, runtime, output semantics, and reproducibility fixtures. The retired
WeatherNext Graph and Gen catalogues are not additional lanes; WeatherNext 2
is their provider-designated successor and is scoped above by legal age.

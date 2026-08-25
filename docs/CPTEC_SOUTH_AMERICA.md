# CPTEC/INPE South America model adapter contract

Rusty Weather exposes two deterministic CPTEC/INPE forecast lanes:

- `wrf-cptec-7km`: WRF South America 7 km
- `brams-cptec-8km`: BRAMS South America 8 km

Both use the official CPTEC operational forecast-data publication and CPTEC
Data Server transport. The adapter does not infer a similarly named mirror or
substitute another model.

## Native publication

| Contract | WRF 7 km | BRAMS 8 km |
| --- | --- | --- |
| Official listing | `https://ftp.cptec.inpe.br/modelos/tempo/WRF/ams_07km/` | `https://ftp.cptec.inpe.br/modelos/tempo/BRAMS/ams_08km/` |
| Object root | `https://dataserver.cptec.inpe.br/dataserver_modelos/wrf/ams_07km/brutos/` | `https://dataserver.cptec.inpe.br/dataserver_modelos/brams/ams_08km/brutos/` |
| Published cycle | daily 00 UTC | daily 00 UTC |
| Normalized leads | hourly f000-f180 | hourly f001-f180 |
| Grid | regular lat/lon, 1019x1081, 0.07 degrees | regular lat/lon, 978x1009, provider-native unequal latitude/longitude increments |
| Wind orientation | earth-relative | earth-relative |

BRAMS also publishes three pre-cycle objects through f-003. RWS forecast-hour
identity is unsigned, and pre-cycle objects are not silently relabelled as
nonnegative leads. BRAMS f000 is also excluded: its three 2 m temperature
records are inventory-labelled `anl` and all decode as PDT 0 without a
statistical process, so instantaneous/minimum/maximum identity cannot be
recovered safely. From f001 onward, instantaneous temperature is PDT 0 while
maximum/minimum are PDT 8 with explicit one-hour windows.

Each lead has a GRIB2 object and a colon-delimited text `.inv` inventory with
exact message offsets. CPTEC also publishes a binary `.grib2.idx` object. That
binary file is not a wgrib2 inventory and is never passed to the shared text
index parser. The server supports exact HTTP byte ranges, and the fetcher
validates every returned `Content-Range` and length.

The filenames bind both cycle and valid time. For example:

```text
WRF_cpt_07KM_2026081400_2026081401.grib2
BRAMS_ams_08km_2026081300_2026081301.grib2
```

Calendar rollover is computed from the requested forecast hour; a lead is not
derived by searching or guessing directory contents.

## Canonical normalization boundary

Both lanes admit the standard pressure families temperature, relative
humidity, U wind, V wind, and geopotential height. The native inventory has 25
levels: 50 hPa, 100-750 hPa every 50 hPa, then 775-1000 hPa every 25 hPa. A
50-hPa sounding profile therefore realizes 19 exact levels from 100-1000 hPa.
Native pressure RH is retained as `rh_iso`; pressure dewpoint is not invented.

The verified sounding surface contract is 2 m temperature/dewpoint, 10 m U/V,
mean-sea-level pressure, surface pressure, and surface height. The WRF indexed
allowlist additionally admits standard precipitation, column water, cloud,
visibility, gust, and composite-reflectivity records for direct surface
profiles. BRAMS local parameter 228 (`VAPMRT`) is not guessed into canonical
moisture. Its water/cloud records labelled `surface` are also excluded where
the canonical selector requires a column/layer identity.

Derived and heavy diagnostics remain disabled. Live verification proves the
bounded canonical sounding contract, not every native field or every lead.

## Reproducible evidence

Full official f001 text inventories are vendored under
`crates/rw-ingest/tests/fixtures/`; their source URLs and SHA-256 identities are
recorded in that directory's README.

Bounded live verification used `--profile sounding --level-step 50 --verify`
and then `rws validate --deep`:

| Lane | Selected bytes | RWS deep-validation result |
| --- | ---: | --- |
| WRF 7 km 2026-08-14 00z f001 | 89,714,669 bytes in 58 ranges (192,378,474-byte source) | 12 variables, 21,900 chunks, 163,380,250 payload bytes |
| BRAMS 8 km 2026-08-13 00z f001 | 81,837,751 bytes in 5 coalesced ranges (144,450,301-byte source) | 12 variables, 19,952 chunks, 182,188,508 payload bytes |

Both writer checks realized all seven requested 2-D fields bit-exactly and all
five 19-level WRF pressure volumes and five 24-level BRAMS pressure volumes
within the store quantization bound. Deep
validation decompressed and cross-checked every chunk.

BRAMS template-5.3 missing-value handling was independently refereed against
ecCodes. The f001 2 m temperature reproduced the exact 34,408-cell missing mask
and every one of its 952,394 non-missing values; its stored range is
256.702728-306.546478 K. No-missing Eta 8 km temperature and accumulated-
precipitation messages additionally matched ecCodes byte-for-byte across
814,625 values each, guarding the generic spatial-differencing path.

## Producer, transport, and use terms

The producer and publisher is CPTEC/INPE; transport is the official CPTEC Data
Server. The API emits that distinction plus a transformation notice stating
that RWS output is not an official CPTEC/INPE product.

INPE's official Open Data page describes Brazilian open-government data as
freely reusable and not subject to licence, patent, or control restrictions:

`https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/dados-abertos`

No model-directory-specific licence statement was observed. The API therefore
reports the open-data policy cautiously and links the publisher's current
page; it does not apply the website-content CC BY-ND notice to the model data.

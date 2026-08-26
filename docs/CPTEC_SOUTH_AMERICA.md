# CPTEC/INPE South America model adapter contract

Rusty Weather exposes three deterministic CPTEC/INPE forecast lanes:

- `wrf-cptec-7km`: WRF South America 7 km
- `brams-cptec-8km`: BRAMS South America 8 km
- `eta-cptec-8km`: Eta South America 8 km

All three use the official CPTEC operational forecast-data publication and CPTEC
Data Server transport. The adapter does not infer a similarly named mirror or
substitute another model.

## Native publication

| Contract | WRF 7 km | BRAMS 8 km | Eta 8 km |
| --- | --- | --- | --- |
| Official listing | `https://ftp.cptec.inpe.br/modelos/tempo/WRF/ams_07km/` | `https://ftp.cptec.inpe.br/modelos/tempo/BRAMS/ams_08km/` | official CPTEC Data Server cycle directory |
| Object root | `https://dataserver.cptec.inpe.br/dataserver_modelos/wrf/ams_07km/brutos/` | `https://dataserver.cptec.inpe.br/dataserver_modelos/brams/ams_08km/brutos/` | `https://dataserver.cptec.inpe.br/dataserver_modelos/eta/ams_08km/brutos/` |
| Published cycle | daily 00 UTC | daily 00 UTC | daily 00 UTC |
| Normalized leads | hourly f000-f180 | hourly f001-f180 | hourly f000-f264 |
| Grid | regular lat/lon, 1019x1081, 0.07 degrees | regular lat/lon, 978x1009, provider-native unequal latitude/longitude increments | regular lat/lon, 875x931, 0.08 degrees, 90W-20.08W and 55S-19.4N |
| Wind orientation | earth-relative | earth-relative | earth-relative |

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
Eta_ams_08km_2026081400_2026081401.grib2
```

Calendar rollover is computed from the requested forecast hour; a lead is not
derived by searching or guessing directory contents.

## Canonical normalization boundary

All three lanes admit the standard pressure families temperature, relative
humidity, U wind, V wind, and geopotential height. WRF and BRAMS have 25 native
levels: 50 hPa, 100-750 hPa every 50 hPa, then 775-1000 hPa every 25 hPa. Eta
has exactly 22 native levels: 50, 100, 150, 200, 250, 300, 350, 400, 450, 500,
550, 600, 650, 700, 750, 800, 850, 900, 925, 950, 1000, and 1020 hPa. A 50-hPa
sounding profile realizes 19 exact levels from 100-1000 hPa for each lane.
Native pressure RH is retained as `rh_iso`; pressure dewpoint is not invented.

The verified sounding surface contract is 2 m temperature/dewpoint, 10 m U/V,
mean-sea-level pressure, surface pressure, and surface height. The WRF indexed
allowlist additionally admits standard precipitation, column water, cloud,
visibility, gust, and composite-reflectivity records for direct surface
profiles. BRAMS local parameter 228 (`VAPMRT`) is not guessed into canonical
moisture. Its water/cloud records labelled `surface` are also excluded where
the canonical selector requires a column/layer identity.

Eta's safe surface contract is 2 m temperature/dewpoint/RH, 10 m U/V,
mean-sea-level pressure, surface pressure, and interval precipitation. The
precipitation is stored conservatively as `apcp_native_interval` rather than
being mislabeled as a cumulative run total. It does
not publish surface orography in each lead, so the sounding preset realizes
six of its seven requested surface fields and reports the absent field rather
than inventing it. Surface-labelled PWAT/cloud records, TMAX/TMIN window
records, and provider-local parameters 0/2/238, 0/2/239, and 1/0/194 remain
outside the allowlist until their exact canonical semantics are independently
validated. The legacy direct-plot registry follows the same boundary: it does
not advertise pressure dewpoint/vorticity, cumulative total QPF, reflectivity,
or cloud products for Eta merely because another GRIB model provides them.

Derived and heavy diagnostics remain disabled. Live verification proves the
bounded canonical sounding contract, not every native field or every lead.

## Reproducible evidence

Full official f001 text inventories are vendored under
`crates/rw-ingest/tests/fixtures/`; their source URLs and SHA-256 identities are
recorded in that directory's README. Eta also vendors the provider's 265-step
control contract and the exact offsets, compressed-message hashes, ecCodes
statistics, and decoded-value hashes for its f000 T2 and f006 5-6 hour APCP
template-5.3 referees.

Bounded live verification used `--profile sounding --level-step 50 --verify`
and then `rws validate --deep`:

| Lane | Selected bytes | RWS deep-validation result |
| --- | ---: | --- |
| WRF 7 km 2026-08-14 00z f001 | 89,714,669 bytes in 58 ranges (192,378,474-byte source) | 12 variables, 21,900 chunks, 163,380,250 payload bytes |
| BRAMS 8 km 2026-08-13 00z f001 | 81,837,751 bytes in 5 coalesced ranges (144,450,301-byte source) | 12 variables, 19,952 chunks, 182,188,508 payload bytes |
| Eta 8 km 2026-08-14 00z f001 | 48,843,141 bytes in 4 coalesced ranges (95,471,540-byte source), selected SHA-256 `73968DCCB713CE6212BC4B933D2D464A48DA3CF16F1163E68E593901911C082A` | 11 variables, 16,321 chunks, 144,654,249 payload bytes; 6/7 sounding surface fields plus five 19-level volumes |

The WRF and BRAMS writer checks realized all seven requested 2-D fields
bit-exactly and all five 19-level WRF pressure volumes and five 24-level BRAMS
pressure volumes within the store quantization bound. Deep validation
decompressed and cross-checked every chunk.

Eta's writer verification reopened all six realized surface fields bit-exactly
and sampled every level of all five pressure volumes within the quantization
bound. Deep validation reported no warnings. Its stored extrema were physically
bounded: 2 m temperature 254.276-304.932 K, dewpoint 250.317-298.927 K,
surface pressure 47,331-103,139 Pa, pressure temperature 191.445-318.015 K,
RH 1-100.010%, wind components -42.938-84.356 m/s, and height
-117.180-16,725.832 gpm. That row is a dated capture-time record; the Eta lane
is reported as `fixture_verified` rather than `live_verified` because the
evidence that is reproducible on demand is the pinned provider contract and
the decoder goldens, not the store round trip.

## Re-verification, 2026-08-25

The Eta acquisition contract was re-checked against the live publication:

| Check | Result |
| --- | --- |
| Cycle directory `.../eta/ams_08km/brutos/2026/08/24/00/` | HTTP 200, 265 hourly `.grib2` leads f000-f264 with matching `.inv` and `.ctl` sidecars |
| f001 inventory structure vs. the pinned 2026-08-14 fixture | identical field and level rows, including all 22 native pressure levels |
| Pinned `eta-cptec-8km.20260814.t00z.f001.inv` | reproduces byte-for-byte from the provider: 13,277 bytes, SHA-256 `A5A98FB95C833D8CB85DE328DC4EA66587B2D7AE86F519AB97306CE068D55844` |
| Pinned f000 2 m temperature decoder golden | ranged read of bytes 1616482-2099259 returns 482,778 bytes with SHA-256 `DBFC1EBDD827FEF617D06FCA0338C846791DBDBCEE923B466E52CC782AD8E6E7` |
| GRIB2 section 3 of that message | template 3.0, 814,625 points, Ni 875, Nj 931, increments 0.08 degrees, corners 90W-20.08W and 55S-19.4N, resolution/component flags `0x30` (earth-relative vectors) |
| GRIB2 section 1 of that message | originating centre 46 (INPE), reference time 2026-08-14 00 UTC |

The decoded grid matches the `GRID` row vendored in
`crates/rw-ingest/tests/fixtures/eta-cptec-8km.20260814.decoder-goldens.txt`
exactly. No drift was observed in the URL family, cadence, sidecar set,
inventory shape, or grid contract.

BRAMS template-5.3 missing-value handling was independently refereed against
ecCodes. The f001 2 m temperature reproduced the exact 34,408-cell missing mask
and every one of its 952,394 non-missing values; its stored range is
256.702728-306.546478 K. No-missing Eta 8 km temperature and accumulated-
precipitation messages additionally matched ecCodes byte-for-byte across
814,625 values each, guarding the generic spatial-differencing path. Their
compressed-message SHA-256 identities are
`DBFC1EBDD827FEF617D06FCA0338C846791DBDBCEE923B466E52CC782AD8E6E7`
and `E3BF49750AD2BBC794470D611DBE8D0E24C5323CB0AD5AE3979313A59E2C6735`;
the decoded float64-LE hashes are
`E5D6AEF32C995FBE3631822A39C63EDCA708345F71A2454D2AED904C0E856FFB`
and `ADBB6B76F9EB6BFCC2B0FBFC0F52D82CA0E5A8081187B97E10FE8A3AA73AE835`.

## Producer, transport, and use terms

The producer and publisher is CPTEC/INPE; transport is the official CPTEC Data
Server. The API emits that distinction plus a transformation notice stating
that RWS output is not an official CPTEC/INPE product.

INPE's Open Data Plan publishes Eta South America as daily open data under
Brazil's Open Data Policy (Decreto 8.777/2016):

`https://www.gov.br/inpe/pt-br/acesso-a-informacao/dados-abertos/dados-abertos`

No model-directory-specific licence statement was observed. The API therefore
reports the open-data policy cautiously and links the publisher's current
page; it does not apply the website-content CC BY-ND notice to the model data.

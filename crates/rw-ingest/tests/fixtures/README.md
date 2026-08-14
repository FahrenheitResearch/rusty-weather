# Captured provider index fixtures

These are immutable text inventories captured from official NOAA, ECMWF, and
ECCC endpoints from 2026-08-11 through 2026-08-14. They are test evidence,
not runtime data and not a promise that an upstream experimental feed will
remain available forever.

| Fixture | Official source | Capture |
| --- | --- | --- |
| `hrrr-ak.t00z.wrfprsf01.grib2.idx` | `https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260812/alaska/hrrr.t00z.wrfprsf01.ak.grib2.idx` | full, 37,547 bytes, SHA-256 `829CAA4AE7A872A8A54B81F20F1617131618E92F2D3679DF8E927753D0A4944B` |
| `hrrr-ak.t00z.wrfsfcf01.grib2.idx` | `https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260812/alaska/hrrr.t00z.wrfsfcf01.ak.grib2.idx` | full, 9,993 bytes, SHA-256 `3467B5909FE3F66712DA942493DC7DB43403A84D886E7586941E6CBB888F35F1` |
| `rap.t00z.awp130pgrbf01.grib2.idx` | `https://noaa-rap-pds.s3.amazonaws.com/rap.20260812/rap.t00z.awp130pgrbf01.grib2.idx` | full, 19,281 bytes, SHA-256 `8C2A989EB62E52603342B2C98B71D6F6DC2423C21055EFE7DD725AAC79E7697C` |
| `nam.t00z.awip3d01.tm00.grib2.idx` | `https://noaa-nam-pds.s3.amazonaws.com/nam.20260812/nam.t00z.awip3d01.tm00.grib2.idx` | full, 35,031 bytes, SHA-256 `2D7C798F7B8BE4C4553EB8B8B8B8576DEAB34330912D85A4722B8D6E4EECD4B7` |
| `gdas.t00z.pgrb2.0p25.f003.idx` | `https://noaa-gfs-bdp-pds.s3.amazonaws.com/gdas.20260812/00/atmos/gdas.t00z.pgrb2.0p25.f003.idx` | full, 40,449 bytes, SHA-256 `67897D16CC086BE9E11252A828F25147AC4F1561956439FAE2A9A3E9F40FDD66` |
| `gefs.20260812.t00z.f024.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/gens/prod/gefs.20260812/00/atmos/pgrb2ap5/gec00.t00z.pgrb2a.0p50.f024.idx` | full control-member index, 5,816 bytes, SHA-256 `8027A02475BCD54BED0B460E480E1AE8133A4DC5171C071214B763F87B148238` |
| `aigfs.20260812.t00z.f024.pres.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigfs/prod/aigfs.20260812/00/model/atmos/grib2/aigfs.t00z.pres.f024.grib2.idx` | full pressure index, 3,925 bytes, SHA-256 `5CB5E72499D98B7B2E13720C10A71EF18335D7350C090B257AF741C732532A98` |
| `aigfs.20260812.t00z.f024.sfc.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigfs/prod/aigfs.20260812/00/model/atmos/grib2/aigfs.t00z.sfc.f024.grib2.idx` | full surface index, 340 bytes, SHA-256 `9C94D5582E282A18DD812E7E8C944483C9F516D9ADB2EDE48DF5C64DF2791D86` |
| `aigefs.20260812.t00z.f024.pres.avg.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigefs/prod/aigefs.20260812/00/ensstat/products/atmos/grib2/aigefs.t00z.pres.avg.f024.grib2.idx` | full ensemble-mean pressure index, 4,546 bytes, SHA-256 `6BCAE30BDAC5BA0B55B266559B6FA82499EB0E66F5B1F95AC3172697939405AB` |
| `aigefs.20260812.t00z.f024.sfc.avg.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/aigefs/prod/aigefs.20260812/00/ensstat/products/atmos/grib2/aigefs.t00z.sfc.avg.f024.grib2.idx` | full ensemble-mean surface index, 326 bytes, SHA-256 `42D26B218B6783E4BB3F617C1D461D3E082BBDB2C12729087ED2022444F082B4` |
| `hgefs.20260812.t00z.f024.pres.avg.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/hgefs/prod/hgefs.20260812/00/ensstat/products/atmos/grib2/hgefs.t00z.pres.avg.f024.grib2.idx` | full ensemble-mean pressure index, 4,545 bytes, SHA-256 `4F08DD7A0BB06A37B8D8E25E8FF86DCD0C7CE6FEEA8058FFA6EDF66C00516FD9` |
| `hgefs.20260812.t00z.f024.sfc.avg.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/hgefs/prod/hgefs.20260812/00/ensstat/products/atmos/grib2/hgefs.t00z.sfc.avg.f024.grib2.idx` | full ensemble-mean surface index, 326 bytes, SHA-256 `C96465577343855B3DFD1E058C06793FB07815B9F5C05F93D5909BE2CEA64A44` |
| `ifs.20260812.t00z.f024.oper.index` | `https://data.ecmwf.int/forecasts/20260812/00z/ifs/0p25/oper/20260812000000-24h-oper-fc.index` | full line-delimited JSON index, 40,208 bytes, SHA-256 `8F44C5EE3BB8504DFFA8AA6927B05E2F64553BBADEFBF46725F12EA351848B59` |
| `aifs-single.20260810T0000.f024.oper.index` | `https://data.ecmwf.int/forecasts/20260810/00z/aifs-single/0p25/oper/20260810000000-24h-oper-fc.index` | exact representative rows for every ingest-selected parameter plus every published `q` pressure level, excerpted from the 29,572-byte line-delimited JSON index; full-source SHA-256 `EDFED337AA2A077E510352FA6392BFC153E67A4D6B56A2EEF859082081AADFD3` |
| `hiresw.t00z.arw_2p5km.f24.conus.grib2.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/hiresw/prod/hiresw.20260810/hiresw.t00z.arw_2p5km.f24.conus.grib2.idx` | full, 5,121 bytes, SHA-256 `66C041E89BFCF489EC5A4708370C2A3215A53FDB57EABECA22C92D5A3EA895B9` |
| `href.t00z.conus.mean.f24.grib2.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/href/prod/href.20260810/ensprod/href.t00z.conus.mean.f24.grib2.idx` | full, 4,260 bytes, SHA-256 `BA04CCFF36A03DF40FB41D9D9B914F2E1FE62E0C07EB3CF4BD62D7DD0EAD0FD5` |
| `sref.t03z.pgrb212.mean_3hrly.excerpt.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/sref/prod/sref.20260810/03/ensprod/sref.t03z.pgrb212.mean_3hrly.grib2.idx` | exact analysis/f003/f087 rows excerpted from the 155,422-byte sidecar; full-source SHA-256 `5D30AB04959A625EEA2D125DBF9BAFE9EF8DBCD019675583562448BFFB4C9F0A` |
| `refs.t00z.mean.f24.conus.grib2.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/refs.20260810/00/enspost/refs.t00z.mean.f24.conus.grib2.idx` | full, 5,230 bytes, SHA-256 `673FBFCAB62314FBA95339AA4942DB5B3A5F364FBED7AF637A0D727B325B6A3F` |
| `rrfs.t00z.prslev.3km.f024.conus.grib2.excerpt.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/rrfs.20260810/00/rrfs.t00z.prslev.3km.f024.conus.grib2.idx` | exact selector-bearing rows excerpted from the 35,741-byte sidecar; full-source SHA-256 `71E89197227C980F08D3F585D861F217C0A9E9F68E3D6DFC4397ACAB78844161` |
| `rrfs.t00z.2dfld.3km.f024.conus.grib2.excerpt.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/rrfs.20260810/00/rrfs.t00z.2dfld.3km.f024.conus.grib2.idx` | exact selector-bearing rows excerpted from the 21,615-byte sidecar; full-source SHA-256 `A0F4E5110FC37B8D7F5DBEE0347A19FE4165CE6EFF3AF4904464B83FF074C75D` |
| `rtma2p5_2dvaranl_ndfd.idx` | `https://noaa-rtma-pds.s3.amazonaws.com/rtma2p5.20260810/rtma2p5.t00z.2dvaranl_ndfd.grb2_wexp.idx` | exact ingest-selected analysis rows, 662 bytes, fixture SHA-256 `5CC6D510E52478FA5DC2D49E658AF6E9A66960B8387AB56AD55C171C2CBD7E82` |
| `urma2p5_2dvaranl_ndfd.idx` | `https://noaa-urma-pds.s3.amazonaws.com/urma2p5.20260810/urma2p5.t00z.2dvaranl_ndfd.grb2_wexp.idx` | exact ingest-selected analysis rows, 706 bytes, fixture SHA-256 `32E16EDD8D78F06586E4AB171F4A20F0F8745BEBF362D68BA683950FE677BB79` |
| `rdps.20260814.t00z.f024.inventory.txt` | `https://dd.weather.gc.ca/today/model_rdps/10km/00/024/` | exact representative filename rows and decoded grid/vector metadata from the 100,669-byte, 414-GRIB-object official listing; full-source SHA-256 `87BD53259734E95AEFEFF1DA7BBCD83332A5BD3AC6124DC46BD5AD6675952F10`; 2,675-byte fixture SHA-256 `644E6A32A0BE5DBECBCFE141523A3E25E48B828435985DEE3783228F1D355F94` |
| `hrdps.20260814.t00z.f024.inventory.txt` | `https://dd.weather.gc.ca/today/model_hrdps/continental/2.5km/00/024/` | exact representative filename rows and decoded grid/vector metadata from the 91,816-byte, 414-GRIB-object official listing; full-source SHA-256 `AF42B19E5A3D44C00AB0FDA2E1F18E052A2043319B997D0E8263D4B7B957EF8E`; 2,695-byte fixture SHA-256 `C4263EB72B5EE25468C89C1D98D2CFF32B62A25645142E8ECFEFC05B026DBBFC` |
| `geps.20260814.t00z.f024.inventory.txt` | `https://dd.weather.gc.ca/today/ensemble/geps/grib2/products/00/024/` | complete 36-object official listing plus exact identities for the bounded selected payloads; 9,714-byte source listing SHA-256 `ABC37D99EE4402ECDBDB30B5C46E8ECDC245EBE24BBCBCBFD64F7CF4C1E87E4E`; 5,839-byte fixture SHA-256 `53DB508A7970F50BF779ACE52E081DB599082F0A9B52F60550B4D4BA9E2219E4` |

The SREF product is a run-wide file containing all native forecast steps, so
the compact fixture deliberately includes its first, next, and final native
steps while preserving each selected source line byte-for-byte.

The RRFS Public excerpts preserve one exact provider row for every selector
used by each acquisition role. The surface excerpt also pins the published
23-24 hour accumulation/maxima records used for honest trailing one-hour
fields; it is not a claim that this preliminary feed is operationally stable.

The AIFS excerpt preserves provider rows byte-for-byte. It pins the exact JSON
index schema, selected parameter names, byte offset/length fields, and the
pressure levels on which specific humidity (`q`) is published. The software
converts that `q` to canonical pressure-level dewpoint during extraction; the
fixture is inventory evidence, not a substitute for a full-payload ingest
round trip.

The NOAA 2026-08-12 inventories are full provider sidecars. They pin each
adapter's operational AWS URL and product name, message/submessage structure,
surface fields, native pressure levels, and accumulation windows. RAP and NAM
also completed live f001 `--profile sounding --verify` ingests from those
exact products followed by deep run-directory validation. RAP read all
18,329,646 source bytes and produced 7 direct 2-D fields plus five 37-level
volumes. NAM read all 11,699,883 source bytes and produced 6 direct 2-D fields,
plus 37-level temperature/RH/wind/height volumes. Its six native pressure
dewpoint levels remain pinned by the inventory, but the normalized sounding
keeps the denser `rh_iso` coordinate. Missing NAM 2 m dewpoint is reported
rather than synthesized.
The GEFS fixture is explicitly the `gec00` low-resolution control member; it
does not claim perturbed-member or ensemble-statistic RWS coverage. AIGEFS and
HGEFS fixtures are explicitly post-processed mean (`avg`) products and do not
claim individual-member or spread coverage. The IFS fixture pins the complete
official JSON sidecar, including its 14 published pressure levels and exact
range offsets. All are bounded inventory/cadence evidence, not a substitute
for a full-payload ingest plus deep-store validation.

## Bounded live store verification (2026-08-14)

Each lane below fetched exactly one official 00z f024 payload (2026-08-12 for
the global wave and 2026-08-14 for the ECCC regional wave) with the sounding
profile at a 50 hPa candidate step, ran the ingest writer's `--verify` round
trip, then passed `rws validate --deep`. No cycle-wide or unbounded global
download was used.

| Model lane | Acquired bytes | Realized RWS evidence |
| --- | ---: | --- |
| GEFS `gec00` control | 12.1 MiB, six AWS index ranges | 10 variables, 5,205 chunks, 22,677,975 payload bytes; sparse native levels and honest `rh_iso` fallback |
| NOAA AI-GFS deterministic | 82.8 MiB pressure + 4.4 MiB surface from NOMADS | 9 variables, 20,772 chunks, 118,053,665 payload bytes; 11 levels in all five canonical volumes |
| NOAA AI-GEFS `avg` | 66.8 MiB pressure + 3.1 MiB surface from NOMADS | 9 variables, 20,772 chunks, 115,840,724 payload bytes; selector metadata is `ensemble_mean` |
| NOAA HGEFS `avg` | 62.0 MiB pressure + 2.9 MiB surface from NOMADS | 9 variables, 20,772 chunks, 115,321,280 payload bytes; selector metadata is `ensemble_mean` |
| ECMWF IFS Open Data `oper` | 48.5 MiB, 22 JSON-index ranges (146.6 MiB source object) | 11 variables, 20,808 chunks, 111,375,300 payload bytes; 11 levels in all five canonical volumes |
| ECCC RDPS | 51.9 MB pressure + 2.8 MB surface component bundles | 11 variables, 23,910 chunks, 219,953,866 payload bytes; five 19-level volumes plus 6/7 sounding surface fields, with absent surface orography reported rather than invented |
| ECCC HRDPS continental | 121.0 MB pressure + 16.7 MB surface component bundles | 12 variables, 64,815 chunks, 580,038,302 payload bytes; five 19-level volumes plus all seven sounding surface fields |
| ECCC GEPS published statistics | 15,462,959 bytes across 12 surface component objects | 96 typed variables, 576 chunks, 31,291,495 payload bytes; provider percentiles, mean, WMO ensemble spread, and selected probabilities only, with no raw-member or standard-deviation claim |

The NOAA AI surface products did not realize 2-m dewpoint, surface pressure,
or orography; IFS did not realize orography; and GEFS did not realize 2-m
dewpoint or orography. Capability metadata therefore disables derived/heavy
products for these lanes until a verified static/surface join exists. The
specific-humidity (`SPFH`/`q`) conversion did realize canonical pressure-level
dewpoint byte paths for all four AI/IFS lanes.

## CMA provider-statistics live verification (2026-08-14)

The official CMA GRAPES GEPS 2026-08-13 00z f024 WIS2 object was acquired as
one bounded 76,819,073-byte lead file:

`https://wis2node.wis.cma.cn/data/2026-08-13/wis/urn:wmo:md:cn-cma:data.core.weather.prediction.forecast.medium-range.probabilistic.global/Z_NAFP_C_BABJ_20260813000000_P_CMA-WIPPSGEPS-GLB-024.grib2`

Its observed SHA-256 was
`51b65f13f8d2d0cbb250c99786437d15db8cd1e775fd9c641ac75f5939a31ee1`.
The model-specific surface-statistics profile realized all 57 declared
provider products bit-exactly and `rws validate --deep` passed at 57 variables,
1,026 chunks, and 134,659,235 payload bytes. This proof covers provider-produced
means/spreads, percentiles, and probabilities only; it does not claim raw
ensemble members or undocumented local CMA parameters.

## ECCC regional verification (2026-08-14)

The ECCC regional fixtures pin the provider's per-field-object identity,
surface and pressure naming dialects, exact common five-field pressure-level
inventories, and GRIB vector-component flag. They also preserve a real
documentation/live-payload drift: the RDPS documentation still declares
1102x1076, while both captured U/V objects decode to 1140x1045. The decoder
therefore treats the normalized live GRIB grid as authoritative. Each fixture
also records SHA-256 identities for the bounded U, V, speed, and direction
objects used to independently check the grid-to-earth vector rotation; it is
not a promise that ECCC retains `today/` objects indefinitely.

The paired-vector normalization was also checked independently against the
four bounded U/V/speed/direction objects identified in each fixture. Across
every finite cell, RDPS compared 2,382,600 earth-relative components with
0.063552 m/s RMS and 0.207682 m/s maximum error; HRDPS compared 6,553,200
components with 0.006831 m/s RMS and 0.043763 m/s maximum error. Those bounds
include the provider's direction quantization (1 degree for RDPS and 0.1
degree for HRDPS), rather than comparing the decoder against itself.

The GEPS fixture pins the exact 00/12z operational inventory, invariant
3-hourly f003-f192 and 6-hourly f198-f384 scheduler horizon, selected component
objects and hashes, probability thresholds, and WMO derived-product code 4
identity as ensemble spread. It also preserves the current documentation/live
grid drift (720x361 documented, 721x360 decoded). Its cyclic grid encodes
180 degrees as both longitude endpoints, so the pinned 0.5-degree i increment
is authoritative and prevents endpoint interpolation from collapsing the
grid onto one meridian. The extended f936 Monday/Thursday products are
recorded but not scheduled, and ambiguous
`WIND-Max-*` thresholds are not normalized. This lane consumes only ECCC's
published statistics and never claims individual ensemble members.

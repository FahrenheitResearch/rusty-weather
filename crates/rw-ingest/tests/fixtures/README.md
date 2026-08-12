# Captured provider index fixtures

These are immutable text inventories captured from official NOAA and ECMWF
endpoints on 2026-08-11. They are test evidence, not runtime data and not a
promise that an upstream experimental feed will remain available forever.

| Fixture | Official source | Capture |
| --- | --- | --- |
| `aifs-single.20260810T0000.f024.oper.index` | `https://data.ecmwf.int/forecasts/20260810/00z/aifs-single/0p25/oper/20260810000000-24h-oper-fc.index` | exact representative rows for every ingest-selected parameter plus every published `q` pressure level, excerpted from the 29,572-byte line-delimited JSON index; full-source SHA-256 `EDFED337AA2A077E510352FA6392BFC153E67A4D6B56A2EEF859082081AADFD3` |
| `hiresw.t00z.arw_2p5km.f24.conus.grib2.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/hiresw/prod/hiresw.20260810/hiresw.t00z.arw_2p5km.f24.conus.grib2.idx` | full, 5,121 bytes, SHA-256 `66C041E89BFCF489EC5A4708370C2A3215A53FDB57EABECA22C92D5A3EA895B9` |
| `href.t00z.conus.mean.f24.grib2.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/href/prod/href.20260810/ensprod/href.t00z.conus.mean.f24.grib2.idx` | full, 4,260 bytes, SHA-256 `BA04CCFF36A03DF40FB41D9D9B914F2E1FE62E0C07EB3CF4BD62D7DD0EAD0FD5` |
| `sref.t03z.pgrb212.mean_3hrly.excerpt.idx` | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/sref/prod/sref.20260810/03/ensprod/sref.t03z.pgrb212.mean_3hrly.grib2.idx` | exact analysis/f003/f087 rows excerpted from the 155,422-byte sidecar; full-source SHA-256 `5D30AB04959A625EEA2D125DBF9BAFE9EF8DBCD019675583562448BFFB4C9F0A` |
| `refs.t00z.mean.f24.conus.grib2.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/refs.20260810/00/enspost/refs.t00z.mean.f24.conus.grib2.idx` | full, 5,230 bytes, SHA-256 `673FBFCAB62314FBA95339AA4942DB5B3A5F364FBED7AF637A0D727B325B6A3F` |
| `rrfs.t00z.prslev.3km.f024.conus.grib2.excerpt.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/rrfs.20260810/00/rrfs.t00z.prslev.3km.f024.conus.grib2.idx` | exact selector-bearing rows excerpted from the 35,741-byte sidecar; full-source SHA-256 `71E89197227C980F08D3F585D861F217C0A9E9F68E3D6DFC4397ACAB78844161` |
| `rrfs.t00z.2dfld.3km.f024.conus.grib2.excerpt.idx` | `https://noaa-rrfs-pds.s3.amazonaws.com/rrfs_public/rrfs.20260810/00/rrfs.t00z.2dfld.3km.f024.conus.grib2.idx` | exact selector-bearing rows excerpted from the 21,615-byte sidecar; full-source SHA-256 `A0F4E5110FC37B8D7F5DBEE0347A19FE4165CE6EFF3AF4904464B83FF074C75D` |
| `rtma2p5_2dvaranl_ndfd.idx` | `https://noaa-rtma-pds.s3.amazonaws.com/rtma2p5.20260810/rtma2p5.t00z.2dvaranl_ndfd.grb2_wexp.idx` | exact ingest-selected analysis rows, 662 bytes, fixture SHA-256 `5CC6D510E52478FA5DC2D49E658AF6E9A66960B8387AB56AD55C171C2CBD7E82` |
| `urma2p5_2dvaranl_ndfd.idx` | `https://noaa-urma-pds.s3.amazonaws.com/urma2p5.20260810/urma2p5.t00z.2dvaranl_ndfd.grb2_wexp.idx` | exact ingest-selected analysis rows, 706 bytes, fixture SHA-256 `32E16EDD8D78F06586E4AB171F4A20F0F8745BEBF362D68BA683950FE677BB79` |

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

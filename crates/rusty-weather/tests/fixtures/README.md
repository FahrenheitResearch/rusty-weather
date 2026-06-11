# Test fixtures

## `hrrr_mini.grib2` (4,857,485 bytes)

A 7-message subset of one archived HRRR CONUS run, used by
`tests/rw_store_e2e.rs` as the offline end-to-end proof of the
GRIB -> extract -> rw-store -> read-back pipeline. GRIB2 messages are
self-delimiting, so the file is a plain concatenation of the ranged
responses from the two product files below (prs messages first, in file
order, then the sfc message).

- Model run: HRRR CONUS, cycle 2026-06-08 00z, forecast hour f006
- Fetched: 2026-06-09, via `rustwx_io::fetch_bytes` with
  `source_override = SourceId::Aws` and the `variable_patterns` below
  (.idx-based byte-range subsetting), through a one-off throwaway builder
  binary (not kept; equivalent to any .idx-ranged GRIB downloader)

### Pressure file subset (3,648,506 bytes)

Source: <https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260608/conus/hrrr.t00z.wrfprsf06.grib2>
(`.idx` sidecar: same URL + `.idx`)

| idx pattern    | message bytes |
| -------------- | ------------- |
| `HGT:500 mb`   | 712,955       |
| `TMP:500 mb`   | 556,686       |
| `UGRD:500 mb`  | 614,566       |
| `VGRD:500 mb`  | 617,073       |
| `TMP:700 mb`   | 559,286       |
| `TMP:850 mb`   | 587,940       |

### Surface file subset (1,208,979 bytes)

Source: <https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260608/conus/hrrr.t00z.wrfsfcf06.grib2>
(`.idx` sidecar: same URL + `.idx`)

| idx pattern            | message bytes |
| ---------------------- | ------------- |
| `TMP:2 m above ground` | 1,208,979     |

All messages are on the full 1799 x 1059 HRRR CONUS grid
(Lambert conformal), valid 2026-06-08 06z.

## `gfs_mini.grib2` (5,364,550 bytes)

A 7-message subset of one live GFS run, used by the GFS leg of
`tests/rw_store_e2e.rs`. Same self-delimiting-concatenation technique as the
HRRR fixture; the messages were sliced by `.idx` byte offset directly from the
AWS pgrb2.0p25 file.

- Model run: GFS 0.25°, cycle 2026-06-11 00z, forecast hour f000
- Fetched: 2026-06-11, ranged GETs against the AWS file below using its `.idx`
  sidecar offsets (one-off throwaway builder; equivalent to any `.idx`-ranged
  GRIB downloader)

Source: <https://noaa-gfs-bdp-pds.s3.amazonaws.com/gfs.20260611/00/atmos/gfs.t00z.pgrb2.0p25.f000>
(`.idx` sidecar: same URL + `.idx`)

| idx pattern            | message bytes |
| ---------------------- | ------------- |
| `PRMSL:mean sea level` | 999,916       |
| `TMP:2 m above ground` | 513,069       |
| `DPT:2 m above ground` | 533,275       |
| `TMP:850 mb`           | 853,602       |
| `HGT:850 mb`           | 907,056       |
| `TMP:500 mb`           | 739,847       |
| `HGT:500 mb`           | 817,785       |

This covers MSLP + 2 m temperature/dewpoint surface fields plus a sounding-lite
2-level (850/500 hPa) temperature and height volume. The 10 m winds are omitted
to keep the fixture under 6 MB (PRMSL alone is ~1 MB on the global grid). All
messages are on the full 1440 x 721 GFS 0.25° lat/lon grid (lon-periodic),
valid 2026-06-11 00z.

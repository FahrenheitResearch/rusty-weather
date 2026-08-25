# rw-server observations

`rw-server` can expose forecast models, real satellite, SimSat, MRMS, NEXRAD-derived grids, radar mosaics and WRF/ArWen simulated radar from one authenticated service.

## Store contract

New observation writes use exact-time `rw-store.run.v2` manifests. The model namespaces are:

- `obs-satellite`
- `obs-simsat`
- `obs-mrms`
- `obs-radar`
- `obs-radar-mosaic`
- `obs-sim-radar`
- `obs-generated`

Existing `rw-sat` and SimSat v1 stores remain readable when the run has exactly one separated `YYYYMMDD` token and each HHMM manifest key points to `tHHMM.rws`.

## Map delivery

Fetch a run's registered coordinate mesh once from:

```text
GET /v1/observations/{model}/{run}/grid.bin
```

Then fetch each frame/variable plane from:

```text
GET /v1/observations/{model}/{run}/frames/{storage_slot}/{variable}.bin
```

The numeric payloads are intended for client-side GPU palette mapping and animation. Existing `/v1/geographic-window` remains available for bounded geographic subsets.

MRMS ingest carries a per-identity contract table (in
`crates/rw-observations/src/mrms.rs`) transcribed verbatim from the official
NOAA/NSSL MRMS v12.2 GRIB2 user table. For each confirmed identity it converts
that identity's exact finite `Missing` / `No Coverage` codes to `NaN` before
storage — `-99`/`-999` for the dBZ reflectivity family
(`ReflectivityAtLowestAltitude`, `MergedReflectivityQCComposite`,
`SeamlessHSR`), `-1`/`-3` for the precipitation, QPE, VIL, echo-top, MESH, and
POSH families, and the table's `0` fill for `RotationTrackML60min`. Other
values (negative dBZ, zero or trace precipitation, zero-mm hail) remain valid
data. Identities absent from the contract table keep their upstream values
untouched — sentinels and units are never inferred. The stored selector
records the source codes and normalized-cell count; the observation display
contract therefore presents the sentinel classes as transparent no-data.
Source: [NOAA/NSSL operational MRMS v12.2 GRIB2 user
table](https://raw.githubusercontent.com/NOAA-National-Severe-Storms-Laboratory/mrms-support/main/GRIB2_TABLES/UserTable_MRMS_v12.2.csv).

## Server-owned NEXRAD Level II

`[nexrad_level2_ingest]` follows only the explicit `sites` allowlist. Each site
selects a named S3-compatible archive provider and its own output spacing and
radius; there is no implicit all-sites mode, follower-side frame limit, or
fallback downsampling. Network concurrency, timeouts, compressed-object body
bounds, catch-up time, and retention are visible operator resource policies.

For every listed complete volume, the worker downloads the exact object once,
verifies its advertised byte length, records its SHA-256, rejects a decoded
station-id mismatch, grids lowest-sweep reflectivity, and writes through the
same append-only `obs-radar` path as `POST /v1/observations/nexrad/level2`.
The stored selector includes provider id, attribution, object key, object byte
length, digest, and upstream modification time. Unknown or mobile site ids are
not assigned guessed coordinates: configure latitude, longitude, and elevation
together or the cycle fails visibly.

The private status and coalesced refresh routes are:

```text
GET  /v1/observations/nexrad/level2/ingest/status
POST /v1/observations/nexrad/level2/ingest/refresh
```

Cursor state is committed atomically only after the decoded frame reopens with
its canonical snapshot id. On restart an invalid/missing stored target clears
the cursor and is fetched again; a crash between store publication and cursor
commit is harmless because the observation writer detects the exact duplicate.
Tests use fixture XML and a loopback mock HTTP server, never the live archive.

## SimSat

SimSat can either write its existing BowEcho-compatible store directly under `RW_STORE_ROOT`, or submit rendered Kelvin/RGB/derived planes to `POST /v1/observations/generated`. Use JSON `null` for transparent/off-earth/missing values.

## WRF virtual radar

The `beam_ppi` derived operation requires a `pressure3d` reflectivity variable and a `pressure3d` height variable with identical pressure levels. Each request can select a new virtual radar location, elevation, tilt, beam width, effective-Earth-radius factor and beam aggregation without rerunning the model.

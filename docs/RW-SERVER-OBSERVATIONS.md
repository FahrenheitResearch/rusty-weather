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

## SimSat

SimSat can either write its existing BowEcho-compatible store directly under `RW_STORE_ROOT`, or submit rendered Kelvin/RGB/derived planes to `POST /v1/observations/generated`. Use JSON `null` for transparent/off-earth/missing values.

## WRF virtual radar

The `beam_ppi` derived operation requires a `pressure3d` reflectivity variable and a `pressure3d` height variable with identical pressure levels. Each request can select a new virtual radar location, elevation, tilt, beam width, effective-Earth-radius factor and beam aggregation without rerunning the model.

# Observation display contract

`rw-server` observation planes contain calibrated scalar values, not pre-colored
screenshots. A client must use the variable selector returned by
`GET /v1/observations/{model}/{run}/frames` before rendering the corresponding
binary plane.

Newly written observation selectors include two additive objects:

```json
{
  "grid_display": {
    "geometry": "structured_curvilinear_lat_lon",
    "sample_location": "cell_center",
    "mask": "non_finite_values",
    "bbox_texture_safe": false
  },
  "display": {
    "semantics": "radial_velocity",
    "palette": "velocity",
    "interpolation": "velocity_fold_aware",
    "transparent_non_finite": true,
    "preferred_range": [-80.0, 80.0],
    "discontinuity_threshold": 30.0
  }
}
```

## Required client behavior

1. Decode `grid.bin` as the per-cell latitude array followed by the per-cell
   longitude array. It is a structured curvilinear mesh, not merely a bounding
   box.
2. Build map geometry from those coordinates. Do not stretch the scalar plane
   over the four extrema of the grid.
3. Treat non-finite coordinates or values as transparent and omit triangles
   touching invalid geometry. Never allow texture filtering to smear values
   from valid coverage into no-data space.
4. Select a color-table family from `display.palette`. In particular,
   `radar_velocity` uses the normal velocity table rather than a generic gray
   ramp; reflectivity uses reflectivity; ZDR, correlation coefficient, KDP,
   PHIDP, and HCA keep their own families.
5. Honor `display.interpolation`:
   - `linear`: validity-aware scalar interpolation;
   - `nearest`: no interpolation, for categories or packed color data;
   - `circular_degrees`: interpolate phase on the unit circle;
   - `velocity_fold_aware`: do not average across a signed discontinuity larger
     than `discontinuity_threshold`.

The raw observation binary formats remain version 1 and unchanged, so this is
an additive metadata contract. Existing stored runs can be read as before;
reingesting them writes the explicit display metadata. Clients should also keep
a variable-name fallback for older stores.

## Radar generation and mosaics

NEXRAD Cartesian generation interpolates in native polar coordinates before
writing the geographic plane. Range and azimuth interpolation are
validity-aware; PHIDP uses circular interpolation; HCA remains nearest-neighbor;
and velocity avoids averaging across a Nyquist fold. Gaps larger than 2.5° are
not bridged.

Radar mosaics now inherit the source variable name by default, preserving the
correct palette family. Source grids are sampled with the same semantic rules
instead of unconditional nearest-neighbor lookup. Arithmetic mosaics are
rejected for radial velocity, PHIDP, HCA, and packed color planes. A radial
velocity overlay is not an earth-relative wind field: use the `latest` method
for a presentation overlay or a proper multi-Doppler retrieval for scientific
wind analysis.

Velocity source selectors explicitly state `velocity_reference` and
`velocity_dealiased`. A Level-II grid is raw velocity radial to its source
radar unless a future producer says otherwise. A radial-velocity mosaic also
states `earth_relative_wind: false`; clients must not present it as a retrieved
U/V wind field or run a polar-sweep dealiaser on the Cartesian mosaic.

## `rw-server` response exposure

The frames catalog now includes a top-level `display` object for every variable,
including older stored runs whose selector predates this contract. Plane binary
responses repeat the critical facts in headers:

- `x-rw-observation-semantics`
- `x-rw-observation-palette`
- `x-rw-observation-interpolation`
- `x-rw-nodata`
- `x-rw-preferred-range` when defined
- `x-rw-discontinuity-threshold` when defined

The capabilities endpoint advertises `display_metadata`,
`curvilinear_grid_mesh`, and `non_finite_transparency` as supported.

Raw geostationary bands are also classified from their persisted channel
metadata: C01-C06 use the visible/reflectance family, C07 uses shortwave IR,
C08-C10 use water-vapor enhancement, and C11-C16 use the infrared family. This
prevents raw full-disk channels delivered through the generic observation API
from appearing as an arbitrary gray scalar.

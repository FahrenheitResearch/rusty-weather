# rustwx-regrid

`rustwx-regrid` converts complete horizontal fields from one grid to another.
It sits between decode/core containers and product or rendering code:

```text
rustwx-io / model readers -> rustwx-core -> rustwx-regrid -> rustwx-products -> rustwx-render
```

Rendering draws prepared fields. Regridding builds a new target-grid array from
a source-grid array. It is different from drawing a native grid on a map
projection, and it is different from point or cross-section sampling.

## Supported methods

- Nearest neighbor over grid centers, with an optional maximum distance.
- Bilinear interpolation for regular latitude/longitude source grids.
- Inverse-distance weighting over any geometry that exposes center lat/lon.
- First-order conservative remapping for regular latitude/longitude grids.

## Geometry support

| Geometry | Nearest | Bilinear | IDW | Conservative |
| --- | --- | --- | --- | --- |
| regular lat/lon | yes | yes | yes | yes |
| `rustwx_core::LatLonGrid` regular layout | yes | yes | yes | yes |
| curvilinear lat/lon centers | yes | no | yes | no |
| projected structured grid | only if it can expose lat/lon centers | no | only if it can expose lat/lon centers | no |

Projected structured grids are represented, but this crate does not invent
inverse projection math. Methods that need lat/lon centers return an explicit
unsupported-geometry error when a projection cannot provide them.

## Missing data

Sparse rows with no source contributors produce `NaN` unless a fill policy is
configured. Source `NaN` behavior is controlled by `MissingPolicy`:

- `Propagate`: any weighted `NaN` makes the target value missing.
- `RenormalizeValid`: `NaN` contributors are skipped and remaining weights are
  renormalized.
- `FillValueF32` / `FillValueF64`: missing output is replaced by that fill
  value. The fill value is cast for the other scalar precision.

## Vector fields

Earth-relative vectors can be regridded component-by-component with
`VectorRegridPolicy::ComponentsAlreadyEarthRelative`.

Grid-relative wind rotation is never assumed silently. The source or target
grid must expose `VectorOrientation::GridRelative { angle_to_east_rad }` through
`GridGeometry::vector_orientation`; otherwise vector rotation policies return
`UnsupportedVectorRotation`.

## Reusable plans

`RegridPlan::build` constructs sparse weights for a source grid, target grid,
and method. The plan can then be applied repeatedly to temperature, moisture,
height, wind components, or any other scalar field with the same source grid.

## Example

```rust
use rustwx_regrid::{
    GridShape, MissingPolicy, RegularLatLonGrid, RegridMethod, RegridOptions, RegridPlan,
};

let source = RegularLatLonGrid::new(
    GridShape::new(3, 3)?,
    35.0,
    -100.0,
    1.0,
    1.0,
    false,
)?;
let target = RegularLatLonGrid::new(
    GridShape::new(5, 5)?,
    35.0,
    -100.0,
    0.5,
    0.5,
    false,
)?;

let plan = RegridPlan::build(
    &source,
    &target,
    RegridOptions {
        method: RegridMethod::Bilinear,
        missing_policy: MissingPolicy::RenormalizeValid,
        extrapolate: false,
    },
)?;

let target_values = plan.apply_f32(&source_values)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

Run the example with:

```text
cargo run -p rustwx-regrid --example basic_regrid
```


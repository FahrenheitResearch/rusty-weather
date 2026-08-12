# wrf-formula

wrf-formula is the sandboxed, unit-aware Formula Lab engine for wrf-rust.
It compiles a deterministic expression language once and evaluates it against
a WrfFile or a custom FieldResolver.

~~~rust
let formula = wrf_formula::compile(
    r#"
    speed = sqrt(ua^2 + va^2)
    where(speed > quantity(25, "m/s"), speed, quantity(0, "m/s"))
    "#,
)?;
let output = formula.evaluate_wrf(
    &file, time_index, &Default::default(), &Default::default()
)?;
~~~

## Language

A program has zero or more newline/semicolon-delimited assignments and one
final expression. There is no implicit multiplication. Power binds tighter
than unary minus, so -2^2 means -(2^2). Chained comparisons are rejected; use
x > a and x < b.

The language includes checked arithmetic/comparisons, where/min/max/clamp,
common math and trigonometry, quantity/convert, explicit dBZ conversion,
two- and three-component vectors, WRF-aware local calculus, vertical
integration/means/interpolation, and first temporal derivatives. Scalars
broadcast to fields; two fields must have identical labeled shapes and grid
locations.

## Scientific calculus contract

- WRF ddx/ddy are mass-point differences scaled by MAPFAC_M / DX or DY. On
  3-D fields they follow terrain model levels, not constant geometric height.
- ddz differentiates along a fixed model column against nonuniform physical
  height and records the default or explicit height datum.
- 2-D divergence/curl use conformal metric forms
  m^2[Dx(u/m)+Dy(v/m)] and m^2[Dx(v/m)-Dy(u/m)].
- 2-D laplacian is the conformal scalar Laplace-Beltrami form
  m^2(Dxx+Dyy).
- These mass-grid diagnostics do not claim strict parity with native staggered
  WRF AVO/UH stencils.
- 3-D grad/div/curl/laplacian are rejected until full terrain-coordinate
  metric terms exist. Anisotropic lat/lon projection calculus is also rejected.
- dt uses a nonuniform three-time Lagrange stencil. A second-order endpoint
  needs two outputs on that side. Nested dt is rejected in schema v1.
- Vertical bounds are never silently clipped.

## Units, safety, and recipes

Data are normalized to coherent SI. Absolute temperatures and differences have
different arithmetic rules. Logarithmic dBZ requires explicit dbz_to_z/z_to_dbz
conversion. Missing and nonfinite policies are explicit.

The language has no filesystem, network, shell, imports, loops, recursion, or
arbitrary Rust/Python execution. Source, AST, memory, output, and work are
bounded. Poisson/global iterative solvers remain reviewed native kernels.

The strict portable schema is wrf-formula/v1. Recipe, plan, output, and
provenance types are Serde-enabled. Portable recipes may lower resource limits
but cannot raise immutable host ceilings. Parse untrusted JSON with
`Recipe::from_json_bytes` or `Recipe::from_json_reader`; both enforce the
1 MiB input ceiling and compile-validate the recipe before returning it.
Direct Serde callers must impose their own byte limit and call `Recipe::compile`.

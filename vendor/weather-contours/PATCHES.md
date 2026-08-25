# BowEcho vendor lineage

The source archive supplied for this integration is retained at
`upstream/autumnplot-weather-contours-sota.zip`, with the bundled MIT license
recorded under the project's canonical copyright holder.

- Archive SHA-256: `3399B35288B0A10D34AA5CD0CB92E003B3B0A194DAD12E2E1F735E928FF3D112`
- Imported native crate: `rust/weather-contours` version 0.2.0
- Imported on: 2026-08-14

BowEcho intentionally excludes the archive's TypeScript, WebGL, worker,
WebAssembly ABI, demo, and integration scripts from the compiled dependency.
The archive remains here so the vendored Rust code and every local change can
be reconstructed without relying on a transient Downloads folder.

BowEcho's native fork adds:

- deterministic degenerate-saddle topology independent of requested-level
  order and cropped-grid origin;
- exact duplicate-vertex removal and rejection of non-drawable paths;
- half-open interior isoband ownership, with the final upper bound closed;
- finite and strictly monotonic rectilinear-axis validation;
- configurable resource limits and fallible large allocations;
- panic-free validation of public packed geometry;
- additional regression and bounded randomized-invariant tests; and
- native-only `rlib` packaging with no browser or WebAssembly build surface.

The original MIT license is retained in `LICENSE` and reproduced in BowEcho's
top-level `THIRD-PARTY-NOTICES.md`.

# Large-grid WRF import notes

Rusty Weather's desktop WRF paths are intentionally conservative on large
domains. A file at or above 1 GiB, or a first-time grid at or above 10 million
3-D cells, is parked behind an explicit confirmation before either the light
store import or full diagnostics begins.

The policy comes from measurements on an 800 x 800 x 79 (~50.5 million-cell)
WRF domain:

- probing a raw wrfout through netcrust before the WRF reader spent about
  57 seconds indexing metadata, so raw WRF detection now tries `WrfFile`
  first;
- the optimized 2-D reader reduced the observed light-plane portion from
  roughly 1,241 seconds to roughly 91 seconds;
- wrf-core's per-time diagnostic cache can retain several gigabytes of 3-D
  intermediates, so it is cleared only after the final `getvar` needed by the
  isobaric-volume build; clearing earlier forces expensive recomputation and
  increased the observed peak rather than reducing it;
- wind components are split without cloning the complete UV volume, dewpoint
  is converted in place, and pressure-map display reads retain only one level
  plane rather than materializing a full pressure volume.

All desktop WRF, Formula Lab, GDEX, severe-diagnostic, and batch workers lower
their OS thread priority. User-selected or malformed data is processed off the egui
thread where practical, panicking diagnostic kernels are isolated at their
field/stage boundary, and partial diagnostic failures become named notes. A
failed hour is not silently reported as complete.

The two import profiles are different:

- **Light import** writes common surface fields and isobaric sounding volumes.
  It does not run the postprocessed approximate severe suite.
- **Full diagnostics** applies the saved core/diagnostic/raw/heavy selection.
  On a large selection the confirmation window also offers a core-only start.

Postprocessed climate archives may omit PSFC, T2, Q2, HGT, earth-rotation
metadata, and effective-layer ingredients. Any severe fields reconstructed
from lowest-model-level substitutes are stored as `approx_*`; they must not
be compared as if they were the raw-wrfout wrf-core products.

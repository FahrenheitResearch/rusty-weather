# rw-storm

`rw-storm` is Rusty Weather's UI-independent deterministic storm-cell geometry
engine. It finds connected reflectivity gates, traces the threshold boundary
with the vendored native Rust `weather-contours` engine, and emits the shared
`rw-ops-protocol` `StormCellFrame` contract used by workstations, RW Server,
and future web clients.

The first slice deliberately does **not** claim that NEXRAD Storm Tracking
Information (NST/STI) supplies polygons. NST supplies official centroids and
tracks; geometry produced here is derived from the reflectivity grid and is
identified as such in every method record.

Two entry points keep geometry honest:

- `detect_geographic` accepts rectilinear longitude/latitude axes, suitable for
  MRMS and already-georeferenced radar grids.
- `detect_level2_cartesian` accepts a rectilinear local east/north grid plus a
  radar location. Polar Level-II gate indices are not Cartesian coordinates;
  a caller must first perform radar-aware polar-to-Cartesian gridding.

There is no fixed user-facing grid-size ceiling. Connected components are
formed with a row-run union-find representation, and each qualifying
component is contoured in its own padded bounding window. COBRM/OIRT resource
budgets are derived with checked arithmetic from that window's dimensions.
The remaining limits are representational (`usize`, COBRM's packed `u32`
indices, and the versioned wire-protocol ring/cell bounds), and failures are
explicit rather than silently downsampling or truncating geometry.

Non-finite samples never become storm gates. During contour extraction they
are represented as below-threshold support so an enclosed missing region can
be emitted as an explicit hole. Frames record both that policy and the missing
sample count.

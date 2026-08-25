# Dataset adapter and publication contract

This document is the review gate for adding a measured or forecast dataset to
Rusty Weather. It applies to file importers, live followers, observation
adapters, server catalogs, derived products, and rendered tiles. The rules are
format-independent: a new GRIB, NetCDF/HDF5, radar, satellite, lightning, or
point-observation source must satisfy the same boundary.

`MUST`, `MUST NOT`, `SHOULD`, and `MAY` are normative. A source is not
production-ready merely because one sample decodes or looks plausible.

## Existing implementation anchors

New work should extend these paths instead of creating a second science path:

- `rw_observations::ObservationFrame` and `GridPlane` are the normalized
  gridded-observation boundary. `ObservationFrame::validate` rejects infinite
  plane values; missing scalar data is represented by `NaN`.
- `rw_observations::write_observation_frame` writes exact-time
  `rw-store.run.v2` data. `rw_query::ensure_variable_metadata_compatible`
  enforces one scientific variable identity within a run and causes a changed
  identity to use a new append-only run variant.
- `rw_store::grid::encode_grid_bytes` persists the full `LatLonGrid` and
  optional `GridProjection` in `grid.rwg`; the SHA-256 of those bytes is the
  grid identity referenced by each `.rws` file and `run.json`.
- `rw_store::atomic::{atomic_write_with, atomic_write_bytes}` is the required
  same-directory, fsync-and-replace publication primitive.
- `rw_sat::archive::{NativeSatelliteFrame, NativeChannelSource}` and
  `.rw-satellite-sources/<platform>/<sector>/<YYYYMMDD>/<frame>/frame.json`
  are the current native satellite source archive. `is_complete_for` defines
  the minimum required-channel check for an ABI product.
- `rw_sat::events::SatEvent::NativeFrameUpdated` is a coalescing wake-up after
  a durable native manifest update. The event epoch is not source identity;
  consumers must reconcile the manifest.
- `rw_server::satellite::SATELLITE_TILE_RECIPE_VERSION` and
  `SatelliteTileCacheKey` are the current rendered-tile recipe and cache
  anchors. An exact rendered identity must also include the source revision as
  described below.

These anchors do not waive the remaining requirements. In particular, a frame
timestamp, object key, filename, byte count, process-local wake epoch, or
`latest` alias is not by itself a source revision.

## 1. Normalize once, at the adapter boundary

An adapter MUST convert source-specific packing and missing-data conventions
before values enter shared science, storage, mosaic, interpolation, or render
code.

1. Read missing and validity metadata in the source's declared domain. For CF
   packed variables, compare `_FillValue`, every `missing_value`,
   `valid_range` (or `valid_min`/`valid_max`) against the packed value before
   applying `scale_factor` and `add_offset`.
2. Convert every declared missing/no-coverage value to `NaN`. Preserve all
   other finite values, including physically valid negative values. Do not use
   epsilon or visually inferred sentinel ranges.
3. Reject malformed bounds and non-finite scale/offset metadata. If scaling a
   valid packed value overflows, normalize the result to `NaN`; never publish
   infinity.
4. Normalize coordinate no-data to non-finite coordinates and keep the mask
   explicit. Do not invent coordinates, bridge off-earth space, or fill source
   coverage gaps in the adapter.
5. Record the source rule in scientific selector metadata: source parameter,
   packing/calibration version, exact sentinel values, and the canonical
   missing representation. Per-frame counters such as `normalized_cells` are
   diagnostics, not scientific identity.

The shared NetCDF implementation is
`rw_sat::netcdf::{read_scaled_f32, read_scaled_f32_window_strided}`; both its
classic-NetCDF and HDF5 lanes follow the packed-domain rule. The MRMS-specific
example is `rw_observations::mrms::normalize_mrms_reflectivity_sentinels`,
which converts only the documented reflectivity codes `-99` and `-999` and
retains other negative dBZ values.

## 2. Separate scientific identity from presentation

Scientific metadata is immutable within one queryable run. It includes:

- variable name, units, value semantics, kind, codec, and pressure levels;
- source collection/product/parameter, level, calibration and quality-control
  rules;
- physical time semantics and source revision;
- grid shape, coordinate arrays, projection, sample location, and mask
  semantics;
- derivation algorithm and its version when values are derived rather than
  directly measured.

Presentation metadata includes palettes, preferred display ranges, labels,
color stops, UI grouping, and renderer-only interpolation hints. Presentation
changes MUST NOT rewrite old scientific values or masquerade as a source
revision. Rendered bytes change identity through a new renderer recipe.

For observation selectors written by `rw-observations`,
`rw_query::ensure_variable_metadata_compatible` deliberately excludes the
top-level `display` and `grid_display` objects, the per-frame `valid_unix`, and
MRMS `normalized_cells`; it compares the remaining selector exactly. New
volatile or presentation fields MUST be explicitly reviewed and excluded in
that one compatibility function. Do not scatter ad-hoc ignore lists among
adapters.

If scientific metadata changes, start a new immutable run/generation. If the
same physical time arrives with different scientific values or a different
source revision, preserve the old publication and publish a new revision; do
not silently return `duplicate` and do not mutate an existing immutable cache
key.

## 3. Geometry is data

Every gridded adapter MUST retain enough geometry to locate each scalar sample
without guessing. Use a full `LatLonGrid` plus `GridProjection` in `grid.rwg`,
or a dataset-specific native-coordinate manifest that can deterministically
produce the same information. A bounding box is not a substitute for a
curvilinear mesh or fixed grid.

The adapter MUST state whether coordinates locate cell centers, corners, or
areas. Rusty Weather's observation display contract uses cell centers. XYZ
pixels are also sampled at centers; `rw-sat::tile` uses `(pixel + 0.5) /
tile_size`. Strided/overview reads must preserve the center of each aggregated
source area rather than shifting to the first sample.

Projection conformance MUST cover:

- authoritative control points, including the projection origin/nadir;
- forward/inverse round trips over the usable domain;
- x/y orientation, row order, sweep axis, longitude normalization, and the
  antimeridian when applicable;
- pixel-center and half-cell boundary behavior at full and strided resolution;
- limb/tangent behavior and rejection of off-earth or far-side points;
- adjacent tile/window seams, including a one-cell sampling halo where the
  interpolation kernel requires it.

For geostationary data, use `rw_sat::geostationary::{scan_angles_to_lat_lon,
lat_lon_to_scan_angles}` and keep the faster inverse path behaviorally
equivalent. A finite scan angle is not proof that a terrestrial point is
visible. Triangles or texture samples touching invalid geometry remain
transparent as required by `docs/OBSERVATION-DISPLAY-CONTRACT.md`.

## 4. Acquisition, source revision, and rendered identity

Keep three identities distinct:

**Acquisition identity** selects the upstream logical item: provider,
collection, platform/site/member, sector/domain, product/level/channel, and
physical time. It drives discovery and deduplication but does not prove that
the bytes are unchanged.

**Source revision** identifies the exact bytes and scientific interpretation.
Prefer a content digest. A provider ETag MAY be used as an opaque,
case-sensitive revision token only when listing and response ETags were
validated; a multipart ETag must not be relabeled as a content hash. Include
the byte count and any provider processing/calibration revision needed to
interpret the bytes. Credentials, signed URLs, private paths, and request
headers MUST NOT be persisted or exposed.

**Rendered identity** is the ordered tuple of all required source revisions,
the derived-product/renderer recipe version, output projection and extent,
XYZ coordinate or window, output dimensions, resampling method, and encoding
settings. Cache keys and immutable URLs MUST cover the complete tuple.

`rw_sat::s3::S3Object` currently carries `key`, `size_bytes`, `last_modified`,
and validated `etag`; `download_object_with_control` atomically installs the
validated body. `NativeChannelSource` currently persists the object key and
size but not a digest/ETag, and the archive's existing-file fast path compares
size only. Before native satellite derivatives become a durable prewarmed
publication, archival replacement, that manifest, and `SatelliteTileCacheKey`
must all use required-channel source revisions. `frame_id` is a time label,
not a sufficient revision.

## 5. Completeness and readiness

Each adapter MUST define its atomic source components and a deterministic
completeness predicate. Examples include all required ABI channels, every
Himawari segment in a declared segment set, one MRMS field at one valid time,
or all members/levels required by an ensemble statistic. Time proximity and a
plausible filename are not completeness checks.

Completeness and readiness are different:

- **complete** means all required source components for a named product and
  revision are durably present and validated;
- **ready** means the exact client-facing artifact set promised by the service
  has been generated and atomically published for that complete revision;
- **degraded** is allowed only when the catalog identifies the missing or
  reduced capability and the product remains scientifically honest.

`NativeSatelliteFrame::is_complete_for` and `resolve_native_frame` already
withhold ABI products missing required channels. A production prewarmer must
reconcile all atomic `frame.json` manifests at startup and after
`NativeFrameUpdated`; it must not depend on receiving every wake-up event.
Catalog `latest` pointers advance only after the promised readiness level is
published. Servers may expose a complete-but-warming state, but clients must
not infer ready from HTTP success on an unrelated product.

## 6. Atomic publication and restart behavior

Publication order is payloads first, manifest/readiness pointer last:

1. acquire and validate source bytes in bounded streaming staging;
2. decode, normalize, and validate science and geometry;
3. write every payload/derivative to a same-filesystem temporary path;
4. reopen and validate the written artifact where a reader exists;
5. atomically install immutable artifacts;
6. atomically publish the manifest, catalog generation, or `latest` pointer.

Use `rw_store::atomic` rather than bespoke `.part` handling. Cancellation,
short reads, checksum/ETag failure, decode failure, and process termination
must leave the prior publication readable and the failed revision
undiscoverable. Startup reconciliation removes or ignores stale staging,
rebuilds readiness from durable manifests, and resumes idempotently.

For `rw-store`, the normal layout remains
`<store_root>/<model>/<run>/{grid.rwg,fNNN.rws,run.json}` and writers publish
`run.json` only after the referenced grid and frame file validate. Native
satellite source objects remain outside the compact preview run under
`.rw-satellite-sources`; neither tree is a substitute for the other.

## 7. Server and client responsibilities

The server owns continuous acquisition, source validation, normalization,
scientific identity, completeness/readiness, derivation, immutable artifact
publication, retention, and bounded single-flight/cache work. It returns
exact-frame URLs, source/recipe identities, effective-resolution metadata,
attribution, and explicit availability states.

The client owns viewport selection, cancellation of obsolete requests,
resident tile reuse, parent/ancestor fallback while exact tiles arrive,
palette/application of declared presentation metadata, and honest status UI.
It MUST NOT infer capabilities from filenames, reinterpret radial velocity as
earth-relative wind, fill server no-data, change projection math, or call a
`latest` response immutable. A `latest` catalog or TileJSON response resolves
to an exact frame; immutable caching begins only at the exact revisioned URL.

## 8. Effective-resolution honesty

Catalogs MUST distinguish at least:

- native source sample spacing per input channel/field;
- effective product resolution and the input that limits it;
- output grid/tile pixel spacing at the requested location or representative
  latitude;
- resampling, aggregation, sharpening, or super-resolution method;
- spatial degradation caused by scan angle, footprint growth, or source
  coverage.

`GoesAbiProduct::descriptor` currently reports `native_resolution_km` from the
base ABI channel (`channel_resolution_km`: C02 0.5 km, C01/C03/C05/C06 1 km,
others 2 km). That is nominal nadir source spacing, not a guarantee that every
input to a composite contains independent samples at that spacing. C02 detail
transfer or variance sharpening may justify a 0.5 km output sampling grid, but
must be disclosed as sharpening; it does not turn C01/C03 into independent
0.5 km measurements. Tile zoom and PNG dimensions are delivery resolution,
not new scientific resolution.

## 9. Cursor-bounded catalogs

Every catalog whose cardinality can grow MUST use deterministic ordering,
bounded `limit`, an opaque `after` cursor, and `next_after` only when more
records exist. Cursors name the last returned immutable ordering key (or a
signed/encoded equivalent); they are not numeric offsets and must be validated
before I/O. A page retry against the same catalog generation must be stable,
with no duplicate or skipped entries.

Provider pagination remains internal. For example,
`rw_sat::s3::list_s3_objects` follows S3 continuation tokens and uses
`start-after` for incremental polling; public APIs must not leak that provider
token as Rusty Weather's durable cursor. The existing
`community_store::list_cases` `after`/`next_after` contract is the local
reference pattern. The satellite `/v1/satellite/{platform}/{sector}/{product}/frames`
endpoint is bounded today but has only `limit`; it must gain a stable cursor
before it is used as an unbounded historical catalog.

## 10. Required conformance suite

Every new dataset adapter is incomplete until automated release-mode tests
prove all applicable items below:

1. **Captured source metadata:** a small legally redistributable fixture or a
   metadata/inventory excerpt proves real variable names, dimensions, packing,
   projection, timing, and provider revision fields.
2. **Missing/packing:** scalar and vector missing values, multiple sentinels,
   exact floating sentinel comparison, packed-domain bounds, scale/offset,
   valid negative values, all-missing chunks, and no published infinities.
   Exercise every decoder backend and both full and windowed reads.
3. **Scientific identity:** unchanged input deduplicates; a same-time byte
   revision is preserved separately; units, selector, level, calibration, or
   geometry changes start a new run/generation; presentation-only and
   per-frame diagnostic changes do not poison a run.
4. **Geometry:** authoritative control points, forward/inverse tolerance,
   axis/row order, pixel centers, strided centers, antimeridian where relevant,
   limb/far-side masking, and cross-window/tile seam continuity.
5. **Completeness/readiness:** every required-component omission is withheld;
   out-of-order and revised components converge; readiness advances only after
   all promised artifacts exist; degraded states disclose exactly what is
   absent.
6. **Atomicity/restart:** cancellation and injected failure at each stage
   expose neither partial payloads nor premature catalog entries; the previous
   generation survives; restart reconciliation is idempotent.
7. **Catalogs:** multi-page ordering, invalid/stale cursor handling, retry
   stability, concurrent append behavior, and no duplicates/skips.
8. **Serving/cache:** `latest` resolves to an exact revision, source revision
   and renderer recipe separate cache entries, concurrent identical fills run
   once, ETag/304 bytes agree, and immutable headers occur only on exact URLs.
9. **Resolution/provenance:** catalog values match source metadata and product
   math; sharpening/resampling is disclosed; required source attribution and
   safe provider identifiers survive round-trip without URLs or credentials.

Run the affected crate suites with release semantics, for example:

```text
cargo test --locked --release -p rw-sat
cargo test --locked --release -p rw-observations -p rw-store -p rw-query
cargo test --locked --release -p rw-server --lib
```

A focused test is useful during development, but the adapter review records
the full affected-suite result, the fixture/source revision, and any
intentionally unsupported capability. When source metadata is absent or
ambiguous, the correct result is an explicit refusal, not a guessed value.

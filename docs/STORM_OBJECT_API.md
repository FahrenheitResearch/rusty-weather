# Private storm-object service

RW Server exposes storm-object computation only when `[operations].enabled =
true`. Every route uses the existing operations bearer scopes and returns
`Cache-Control: no-store, private`. The server never accepts a raw grid or a
model artifact from an HTTP request.

## Routes

- `GET /v1/ops/storms/status` reports the exact source connections that are
  available and the ones that remain intentionally unavailable. It also
  reports durable-cache revision, entry/byte totals, recovery events, and the
  background reconciler's exact latest source, timestamps, errors, and stale
  state.
- `GET /v1/ops/storms/methods` returns the canonical
  `rw.ops.storm-method-catalog.v1` method identities.
- `GET /v1/ops/storms/models` lists immutable installed model manifests,
  distribution policy, activation, and whether this node can execute each
  version.
- `POST /v1/ops/storms/authoritative/nexrad-level3/decode` decodes a supplied,
  base64-encoded NEXRAD Level III message 58 (NST/STI) or 62 (NSS/SS) product
  into authoritative point/track attributes. A three- or four-character
  `site_hint` may preserve an exact radar identity when transport metadata is
  incomplete. The structural input limit is 16 MiB; normal products are far
  smaller. This route does not accept or create polygon geometry.
- `POST /v1/ops/storms/cells` returns a canonical
  `rw.ops.storm-cell-frame.v1`.
- `POST /v1/ops/storms/cells?format=geojson` returns a GeoJSON FeatureCollection
  with the same source, method, warnings, cells, holes, and typed attributes.

The request schema is `rw.server.storm-cells-request.v1`. It names an existing
model/run/storage slot/field and requires both `expected_snapshot_id` and
`expected_grid_hash`. An atomic run replacement therefore produces `409` and
never silently changes the accepted scientific input.

The MRMS follower's `latest` identity supplies these fields directly:
`model`, `run`, `snapshot_id`, `grid_hash`, `storage_slot`, and the product's
`variable`. A client must not submit an automatic storm request when
`latest = null` or `fresh = false`; it should retain the last explicitly
identified frame or show that current storm objects are unavailable.

```json
{
  "schema": "rw.server.storm-cells-request.v1",
  "grid": {
    "model": "obs-mrms",
    "run": "conus-mergedreflectivityqccomposite-20260823-0123456789ab",
    "expected_snapshot_id": "<from GET run detail>",
    "expected_grid_hash": "<from GET run detail>",
    "storage_slot": 0,
    "variable": "mrms_reflectivity_lowest_altitude"
  },
  "source": {
    "kind": "mrms",
    "product": "MergedReflectivityQCComposite",
    "valid_at_unix_ms": 1787472000000,
    "grid_hash": "<same exact grid hash>"
  },
  "method": { "kind": "auto" }
}
```

`auto` uses an explicitly active, compatible, compiled Rust model when one is
registered on the node. Otherwise it uses the deterministic reflectivity
method. `deterministic` exposes threshold, valid-value bounds, minimum gate
count, minimum area, and four/eight connectivity. `machine_learning` selects
an enabled immutable model version. A native model maps manifest input names
to stored fields; a `supplied_mask` model reads its probability mask from a
stored field on the same grid/time. `tract_onnx` remains catalogued but is not
executed by this build. No Python, dynamic library, implicit download, silent
resampling, or client-uploaded multi-gigabyte JSON is involved.

Identical requests share one asynchronous single-flight fill. The key covers
the complete stored-grid reference, source identity, method controls, model
selection, executable native-backend revision where relevant, and the cache
format revision. The canonical JSON and GeoJSON bytes share the same immutable
`StormCellFrame` and are persisted as one verified directory installed by an
atomic same-parent rename below
`<server.cache_root>/.rw-storm-frame-cache/v2`. A restart verifies both payload
digests and their exact source/method identities before reuse. Incomplete
staging directories and corrupt derived entries are discarded; scientific
source data is never modified.

`[storm_prewarm]` enables request-independent deterministic analysis of
configured `ReflectivityAtLowestAltitude` frames. The worker reconciles the
newest `backfill_frames` at startup and then subscribes to committed-frame
epochs from the MRMS follower. Commit bursts coalesce and use the same
single-flight request path; there is no client/UI polling loop and the first
website or app request does not start contour work. `retention` is either
`{ mode = "bounded", frames_per_source = N }` (default 576, approximately two
days at five-minute cadence) or `{ mode = "unlimited" }`. Retention changes
only derived-cache lifetime. It does not simplify contours, reduce native
resolution, or limit how many cells/rings/points a result may contain.

## Scientific boundary

- MRMS and single-sweep Level-II reflectivity grids are supported when they
  are already stored and rectilinear in geographic coordinates.
- Level-II polar gates must first pass through the radar-aware
  `rw-observations` georeferencing path. Raw radial/gate indices are never
  treated as Cartesian coordinates.
- The v1 `StormSource::NexradLevel2` identity carries one elevation, so a
  multi-sweep composite is rejected rather than assigned a fabricated
  elevation. Extending that source contract is the precise remaining
  connection for composite Level-II fields.
- NOAA NEXRAD Level III message 58 (NST/STI) supplies authoritative centroids,
  history/forecast positions, and motion—not polygon outlines. The service's
  pure-Rust decode route exposes those supplied tracks, while the
  provenance-preserving same-site/time association remains a distinct step.
  The service never labels a derived contour as an “NCEI outline.”
- NOAA NEXRAD Level III message 62 (SS/NSS) supplies authoritative centroid
  positions and storm-structure attributes. It supplies neither tracks nor
  polygons. The decode response uses a distinct method identity and never
  promotes structure rows into tracking or contour claims.
- Curvilinear grids require an explicit projection-aware contour adapter and
  are rejected rather than approximated as rectilinear.

Installed models live below `<operations.root>/storm-models`. Manage them
offline with `rw-server --config <path> storm-models`: `install` streams a
local artifact into an immutable version after validating its manifest and
rights policy, `verify` rechecks its digest, and `enable`, `activate`,
`disable`, and `rollback` update atomic registry selections. Every mutation
reports that the running service must be restarted before it can observe the
new registry state. HTTP routes never install or mutate models.

`GET /v1/ops/storms/models` advertises the effective model limits. A JSON
`null` for installed versions, activation history, grid width/height/points,
or label work means there is no configured policy ceiling: checked `usize`
arithmetic, allocation capacity, and filesystem capacity remain the real
limits. Artifact and manifest byte limits and the 64 input-plane execution
contract remain finite and are reported explicitly. Neither case causes
downsampling, catalog truncation, or a lower-resolution result.

A trusted Rust implementation is registered with
`StormRuntime::register_native_model`; model artifacts are never loaded as
native executable code. An operator must verify the manifest's producer,
training provenance, license, artifact-distribution grant, derived-output
grant, attribution, and rights reference before enabling it.

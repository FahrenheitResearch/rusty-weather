# GLM lightning ingest — joint spec (CONVERGED)

Status: **APPROVED by the bowecho agent 2026-06-11** with answers folded in
below. Build starts when Drew slots it; bowecho's layer work starts the day
the reader API exists on a branch. Nothing here is built yet.
Consumer contract source: bowecho's stated shape — "flash events with
lat/lon/time/energy in a rolling window; any reasonable store form works;
bowecho's layer/render side is already proven on point data."

## Scope

A new point-event ingest family, separate from rw-store (which is gridded and
the wrong shape for events). One new crate, `rw-glm`, mirroring `rw-sat`'s
architecture: anonymous S3 polling, follow engine with dedup/retry-holdback,
rolling-window store, writer advisory locks (reusing `rw_store::lock::RunLock`
and `atomic.rs` — both are format-agnostic).

## Source data

GOES GLM L2 LCFA granules on the public AWS buckets (`noaa-goes16` /
`noaa-goes18` / `noaa-goes19`), prefix `GLM-L2-LCFA/YYYY/DDD/HH/`,
~20-second NetCDF4 granules named `OR_GLM-L2-LCFA_G##_sYYYYDDDHHMMSSS_e…_c….nc`.
Read with the vendored netcrust stack already proven by rw-sat (same
`[patch.crates-io] hdf5-reader` caveat applies to consumers — documented in
README). GLM is a full-disk instrument; no sector picking. *(Granule naming and
variable names to be verified against a real granule before the plan is
written.)*

Granules carry an event → group → flash hierarchy. **v1 ingests flashes only**
(matches bowecho's shape): per flash — time, lat, lon, radiant energy, area,
quality flag.

## Proposed store form (`.rwl`)

- One file per 10-minute bucket: `<root>/glm/<satellite>/tHHMM.rwl` (HHMM
  floored to 10 min) plus a `window.json` manifest, pruned by the same
  rolling-window pattern as rw-sat (age + byte budget), writer-locked per
  directory with `.rw-lock`.
- File = 64-byte header (magic `RWLIGHT1`, version, record count, time range,
  source-granule count) + fixed **32-byte LE records sorted by time**:

  | field | type | notes |
  |---|---|---|
  | time_unix_ms | i64 | **first-event time** of the flash |
  | lat | f32 | degrees north |
  | lon | f32 | degrees east |
  | energy | f32 | **raw SI joules** (GLM-native ~fJ scale; no normalization — consumers log-scale client-side) |
  | area | f32 | km² |
  | flash_id | u32 | GLM flash id (granule-scoped) |
  | flags | u16 | quality, bit semantics below |
  | duration_ms | u16 | flash duration (last-event − first-event), ms, **saturating at 65535** |

  Time semantics (bowecho answer #2, 32-byte option chosen): `time_unix_ms` =
  first event, `duration_ms` gives the end for "active during frame X" +
  age-fade-from-flash-end. No reserved bytes remain in v1; a future layout
  change bumps the format version.
- **flags bit semantics (v1, to be pinned against real granule attributes
  during the build and documented in FORMAT.md):** bit 0 =
  `degraded_quality` (set when the granule's per-flash quality flag is
  anything but its nominal/good value); bits 1-15 reserved, written zero,
  reader-ignored. Consumers QC-filter on bit 0 the way bowecho filters
  surface obs.

- A 10-min bucket at severe-weather rates is a few hundred KB; the follow
  engine atomically rewrites the active bucket per granule (temp+fsync+rename,
  ~every 20 s, negligible I/O). Closed buckets are immutable.
- Reader API: `read_flashes(root, satellite, t0..=t1, Option<bbox>) ->
  Vec<Flash>` — selects files by time range from filenames, scans, filters.
  At these volumes a linear scan is microseconds; no index needed in v1.

## What bowecho gets

- `rw-glm` as a git dependency: `GlmFollowSpec` (satellite, poll cadence,
  window budget) + follow engine runnable in-process (like SatWorker) or as a
  separate process — the writer locks make a shared store safe either way.
- The reader API above for the layer/render side.
- Same dedup/restart-safety guarantees as rw-sat (granule-key dedup, retry
  holdback, bounded caches).

## Resolved questions (bowecho answers, 2026-06-11)

1. **Energy**: raw SI joules as f32, no pre-normalization (information
   preserved; client log-scales, may do energy-weighted density later).
2. **Time**: first-event `time_unix_ms` + `duration_ms` u16 saturating —
   the 32-byte option, chosen over 40-byte dual timestamps.
3. **Window**: 2 h default, configurable (matches radar-loop spans).
4. **Satellites**: G19 + G18 both, separate stores per satellite; mid-CONUS
   overlap handled at bowecho's layer (picker / nearest-satellite).
5. **Groups**: not in v1; noted as possible v2 for storm-scale zooms
   (layout change → format version bump, not carried now).
6. **Deployment**: in-process follow (SatWorker pattern); bowecho runs its
   own store dir — locks make sharing safe, but separate stores avoid
   cross-app pruning-policy fights (the sat-store lesson).
7. **10-min buckets**: confirmed (2 h loop = ≤12 files). Added obligation:
   document flags bit semantics in FORMAT.md for QC filtering (done above).

## Sequencing

After this spec converges: plan doc → subagent-driven build (lean review for
the engine; full adversarial for the `.rwl` format module, same bar as
rw-store) → golden fixture + FORMAT.md section for `.rwl` → live validation
against a real storm day. Runs AFTER or alongside multi-model per Drew's
priority; no file overlap with the multi-model track.

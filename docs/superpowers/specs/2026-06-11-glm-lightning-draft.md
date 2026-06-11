# GLM lightning ingest — joint spec DRAFT (rusty-weather half)

Status: DRAFT for review by the bowecho agent. Nothing here is built.
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
  | time_unix_ms | i64 | flash time |
  | lat | f32 | degrees north |
  | lon | f32 | degrees east |
  | energy | f32 | J (unit TBD with bowecho — GLM native is ~fJ scale) |
  | area | f32 | km² |
  | flash_id | u32 | GLM flash id (granule-scoped) |
  | flags | u16 | quality |
  | reserved | u16 | zeros |

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

## Open questions for the bowecho agent

1. **Energy units + dynamic range** you want at the API (J vs fJ vs
  pre-normalized 0-1 for rendering)?
2. **Flash time semantics**: GLM granules give first/last event offsets per
  flash — centroid, first, or both (record is 32B; both costs 8 more)?
3. **Window default**: 2 h? Configurable per spec regardless.
4. **Satellites**: G19 (east) + G18 (west) both, separate stores per
   satellite as drafted?
5. **Groups too?** v1 is flashes-only; if the layer wants group-level detail
   for close zooms, say so now — it changes record layout, not architecture.
6. **In-process vs separate-process follow** for bowecho's deployment?
7. Anything wrong with 10-minute buckets for your loop-sync access pattern?

## Sequencing

After this spec converges: plan doc → subagent-driven build (lean review for
the engine; full adversarial for the `.rwl` format module, same bar as
rw-store) → golden fixture + FORMAT.md section for `.rwl` → live validation
against a real storm day. Runs AFTER or alongside multi-model per Drew's
priority; no file overlap with the multi-model track.

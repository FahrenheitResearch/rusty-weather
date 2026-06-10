# Streaming/spill ingest: the <1 GB design sketch

Status: design only — nothing here is built. This note records what a
sub-1-GB-peak store ingest would need, and confirms that the 2026-06
memory-diet changes (the `ingest-ram-diet` branch) do not foreclose any of
it. Today's measured envelope after that branch: ~3.7 GB peak working set
no-heavy, ~4.0 GB with the heavy stage, byte-identical hour files.

## Where today's floor comes from

After the diet, the resident floor of a full-profile HRRR hour is:

| component                                   | ~MB  | why it is resident |
|---------------------------------------------|------|--------------------|
| five f64 pressure volumes (37–40 levels)    | 2900 | kernel inputs; f32 staging would change kernel input bits |
| shared height-AGL volume (replaces gh)      |  580 | kernel input (in place of the gh volume it was derived from) |
| f64 surface planes + lat/lon                |  200 | kernel inputs |
| process base, rayon stacks, unpack scratch  |  300–450 | fixed + per-thread transients |

Everything else (extraction planes, grid clones, raw GRIB bytes, parsed
message copies, encoded chunks, the output assembly) is already freed at
its last use or spilled to disk. The f64 kernel-input volumes are the only
hard floor — they exist because the derived/heavy kernels consume the f64
values produced by the GRIB unpack, and any narrower staging would change
stored bits.

## What <1 GB needs

1. **Windowed GRIB parse.** Generalize the vendor `StreamingParser`
   (wx-core/grib-core) to selective parse from the cached file path so
   neither the raw byte buffer (~555 MB for prs+sfc) nor per-message
   `raw_data` copies are ever fully resident. The extraction entry points
   already receive `bytes_path`; the GRIB arm just does not use it yet.
   The direct-write decode added by the diet branch already consumes
   messages one at a time through a metadata-first pass, so it is shaped
   to iterate a windowed parser instead of an in-RAM `Grib2File`.

2. **f64 compute-input spill.** Write the five pressure volumes (and the
   height-AGL volume) to a memory-mapped scratch file as they decode, and
   stripe the kernels over column blocks that fault in only their rows.
   Per-column independence of every kernel in the store inventory is
   established (no cross-column reductions; `ecape_failure_count` is an
   order-independent sum), so striping cannot reorder any float op. The
   work is the wrapper plumbing: an mmap-backed provider must replace the
   `&[f64]` slices in `EcapeVolumeInputs`/`WindGridInputs`, or the
   wrappers must gather per-stripe copies (bounded by stripe size).

3. **The spill writer as the universal sink.** `HourWriter`'s spill mode
   (built by the diet branch) already streams every encoded chunk to disk
   as it is staged and assembles the final file by streaming. With (1) and
   (2), every plane goes extraction -> encode -> disk with no accumulation,
   and the peak becomes: one stripe of f64 inputs + per-thread scratch +
   the fixed base — comfortably under 1 GB.

## What the diet branch deliberately did NOT do

* No API takes whole-volume `Vec<f64>` ownership in a way that assumes
  RAM residency: the store compute lanes consume `StoreComputeInputs`
  whose volumes are private and accessed by slice, so an mmap-backed
  provider can replace the backing storage behind the same seam.
* The values-only extraction yields per-plane consumption (each plane is
  moved into the writer and freed); a windowed parser can feed the same
  loop plane by plane.
* The direct-write pressure decode separates the metadata pass (level
  records, branch selection, common levels) from value unpacking, which is
  exactly the split a windowed parser needs.
* The deferred-id writer means encode order is independent of variable
  numbering, so a streaming pipeline may encode in whatever order data
  arrives without changing file bytes.

## Hazards to avoid in the meantime

* Do not add APIs that require an in-RAM `Grib2File` to outlive decode.
* Do not let kernels grow neighborhood/reduction semantics without
  updating the striping note above — per-column independence is the
  load-bearing assumption for both striping and mmap windowing.
* Keep the writer's sorted-emission invariant (`sort_key` order) — the
  streaming sink relies on order-independent staging.

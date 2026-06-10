# Store-ingest determinism: gate semantics and known flake

## The identity gate

The memory-diet work (branch `ingest-ram-diet`) carries a hard requirement:
hour files written by the branch must be byte-identical to the ones main
writes for the same GRIB inputs, excluding writer provenance
(`writer.build` in the meta JSON and the index offset shift its length
induces). `rw_store_diff` is the comparator; `scripts/determinism_check.ps1`
is the procedure.

**Current gate semantics: the branch output must match MAIN'S MAJORITY
OUTCOME over N runs — not "any single main run".** This qualification
exists because main itself is not run-to-run deterministic (below). The
branch must additionally be self-consistent across all N of its own runs;
a branch flake fails the gate outright, it is never excused by the
main-side flake.

## Known main-side nondeterminism (open)

Observed 2026-06-09 while validating `ingest-ram-diet` (evidence retained
under `out/_review_truemain_store*`): three full-profile ingests of
hrrr 20260608 00z f006 with a clean main (`290cf4b2fce8`) binary produced
two distinct outputs. Runs 2 and 3 were byte-identical to each other and
to every branch run; run 1 differed at exactly one grid cell
(x=284, y=556) in exactly three variables:

| variable   | majority  | flake     |
|------------|-----------|-----------|
| srh_0_1km  | 3.170938  | 2.945073  |
| srh_0_3km  | 24.478735 | 24.671097 |
| mlecape    | 31.509857 | 11.610478 |

That cluster shares one upstream quantity — the per-column wind profile
feeding Bunkers storm motion (SRH integration and the ECAPE V_SR /
storm-relative inflow term). The stored f32 inputs (`u_iso`, `v_iso`,
`height_iso`, all surface planes) were bit-identical between the
differing runs, so the divergence is confined to the f64 compute inputs
or the kernels' view of them.

### Audit results (what has been ruled out)

A source-level audit of the full compute path found no shared mutable
state and no order-dependent floating-point reductions:

* `wx_math::composite::compute_srh{,_hemispheric}` and
  `metrust::calc::wind::bunkers_storm_motion`: pure per-column functions,
  `into_par_iter().map().collect()` over the cell index — rayon's indexed
  collect is placement-stable, so scheduling cannot reorder any float op;
* `metrust::calc::severe::grid::compute_ecape_triplet*` (and the
  `ecape-rs`/`sharprs` parcel kernels): per-column, no statics, no
  `unsafe`, no scratch reuse;
* products decode lane (`collect_levels` / flatten, both main's
  collect-then-flatten and the branch's direct-write): parallel unpack per
  message into disjoint or indexed destinations, sequential flatten;
* height-AGL assembly (`prepare_store_compute_inputs`,
  `compute_height_agl_3d_generic`): parallel over disjoint chunks,
  per-element math only;
* no `par_iter().sum()`/`reduce()`/hash-map iteration anywhere in the
  derived/heavy lanes.

Remaining suspects, in rough likelihood order: the C FFI unpack codecs
(OpenJPEG / libaec in `grib-core::unpack`, exercised from rayon worker
threads — only relevant for messages using those packings), allocator
poisoning, or a transient hardware fault.

### Reproduction attempts

2026-06-10: four additional full-profile heavy ingests with the same
clean-main (`290cf4b2fce8`) binary (`out/det_check/main_h1..4`) all
matched the majority outcome bit for bit. Tally across every known main
run of this hour: 6 of 7 identical, 1 flake (the original run 1), flake
never reproduced. Every branch run to date has been self-consistent and
equal to main's majority. Root-causing the single flake is tracked as
separate work; this note must be updated when it lands.

## The verify machinery

* `rw_ingest --verify` — after each write, re-opens the hour and checks a
  2D round-trip plus one profile per 3D variable (self-read check, not an
  identity check).
* `rw_store_diff a.rws b.rws` — structural equivalence of two hour files
  (payload + index + meta, `writer.build` excluded).
* `rw_store_diff a.rws b.rws c.rws ...` — N-run self-consistency: groups
  the files into equivalence classes and reports the majority/minority
  split. Use ≥3 runs per side whenever validating a determinism-sensitive
  change; a single A/B pair cannot distinguish "branch broke determinism"
  from "main flaked".
* `rw_store_diff assert-build <sha> <run.json|hour.rws>...` — asserts the
  writer build stamp inside an artifact matches the sha the producing
  binary claims to be built from. A `-dirty` stamp never matches a plain
  sha.

### Baseline binaries must be stamped and asserted

Incident (2026-06-09): a recorded "main vs branch" identity pass was
actually branch-vs-branch — the baseline binary at
`out/identity_check/bin_main/rw_ingest.exe` stamped writer build
`a7bf0c7171ee` (the branch HEAD), not main's `290cf4b2fce8`. The pass
proved nothing; an independent rebuild of true main was required.

Rules, enforced by `scripts/determinism_check.ps1`:

1. capture a baseline binary only from a CLEAN checkout of the baseline
   ref, and write `BUILD_SHA.txt` (output of
   `git rev-parse --short=12 HEAD` at capture time) next to the exe;
2. before trusting any run, `rw_store_diff assert-build <stamped sha>`
   against the `run.json` and hour file the binary just produced;
3. never compare artifacts whose build stamps you have not asserted.

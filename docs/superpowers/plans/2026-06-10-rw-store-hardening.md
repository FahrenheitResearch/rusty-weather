# rw-store Hardening (Wave 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn rw-store from spec-by-implementation into a documented, validated, exportable, concurrency-safe format that external consumers (bowecho first) can depend on.

**Architecture:** Five additive capabilities layered onto the existing `rw-store` crate: a validation library, committed golden fixtures that pin v1 bytes forever, a written FORMAT.md spec, a dependency-free NetCDF3/CDF-2 exporter (kills the interop objection), writer-side advisory locks (fixes the real bowecho collision), and an `rws` CLI that fronts all of it. No existing public API signature changes — additive only. The two diffs to existing behavior: `HourIngestWriter::begin` acquires a run-dir lock, and `RwStoreError` gains variants (breaking only for exhaustive matchers — flag to bowecho).

**Tech Stack:** Rust workspace at `C:\Users\drew\rusty-weather`. New dep: `fs4` (advisory file locks, Windows+Unix). NetCDF3 writer is from scratch (pure byte emission, zero deps) — netcrust is read-only and reads NetCDF4/HDF5, not classic. CLI uses workspace `clap` 4.5 derive like `rw_ingest`.

**Context for implementers (verified against source 2026-06-10):**

- `.rws` header (64 B, little-endian, `header.rs`): magic `b"RWSTORE1"` [0..8], version u32 [8..12] (=1), meta_len u32 [12..16], index_count u64 [16..24], index_offset u64 [24..32] (== 64 + meta_len), payload_offset u64 [32..40] (== index_offset + index_count*64), [40..64] reserved zeros.
- Index record (64 B, LE, `index.rs`): var_id u16 [0..2], kind u8 [2] (0=TILE2D, 1=COLUMN3D), flags u8 [3] (1=EMPTY, 2=CONSTANT, 4=HAS_MISSING), tile_y u32 [4..8], tile_x u32 [8..12], offset u64 [12..20] (**absolute file offset**), len u32 [20..24] (compressed), raw_len u32 [24..28] (uncompressed), center f32 [28..32], scale f32 [32..36], min f32 [36..40], max f32 [40..44], valid_count u32 [44..48], [48..64] reserved zeros. Index sorted strictly ascending by (var_id, kind, tile_y, tile_x).
- 2D tiles: 256×256 (`TILE_Y`/`TILE_X`), edge tiles clipped; payload = row-major f32 LE within the tile window, zstd-1. Codec name `zstd1_f32`.
- 3D column chunks: 16×16 columns (`COL_Y`/`COL_X`), edge-clipped; payload order `for gy { for gx { for level in levels_hpa order } }` (writer.rs:396-403); whole chunk affine-i16 quantized (one center/scale), `value = center + q * scale`, `q == i16::MIN` (`MISSING_Q`) means NaN; then zstd-1. Codec name `zstd1_affine_i16`. CONSTANT without HAS_MISSING ⇒ empty payload; CONSTANT|HAS_MISSING ⇒ payload of 0/MISSING_Q i16s; EMPTY ⇒ no payload, valid_count 0, min/max NaN.
- Meta JSON (`format.rs::RwsHourMeta`): schema `"rw-store.hour.v1"`, model, run, forecast_hour, nx, ny, grid_hash, variables[] (id, name, units, kind `"surface2d"|"pressure3d"`, codec, levels_hpa, selector), chunking {tile_y,tile_x,col_y,col_x}, writer {name,version,build}.
- `.rwg` grid file (`grid.rs`): magic `b"RWSGRID1"`, version u32, meta_len u32, lat_comp_len u64 [16..24], lon_comp_len u64 [24..32], [32..64] reserved; then meta JSON (schema `"rw-store.grid.v1"`, nx, ny, lat_raw_len, lon_raw_len, projection), then zstd-1 of lat f32 LE bytes, then zstd-1 of lon f32 LE bytes. grid_hash = sha256 hex of the full file bytes.
- `run.json` (`run.rs`): schema `"rw-store.run.v1"`, model, run, grid_hash, nx, ny, hours (BTreeMap<u16, {file, written_unix, encode_ms, variables}>), writer. All writes via `atomic.rs::atomic_write_bytes/atomic_write_with` (temp `.{name}.tmp-{pid}-{seq}` + flush + fsync + rename).
- `HourReader` (reader.rs): `open(&Path)`, `meta()`, `variable(name)`, `read_full_2d(name)`, `read_window_2d(name,x0,y0,x1,y1)`, `read_column_3d(name,ix,iy)`, `read_profile_3d(name,fx,fy)`. mmap-backed; zstd decompress via `zstd::bulk::decompress` capped at raw_len.
- Write paths needing locks: `HourIngestWriter::begin..finish` (ingest.rs:264/425 — writes grid.rwg, f*.rws, run.json) and rw-sat `enforce_window` (crates/rw-sat/src/window.rs:51-170 — deletes hour files, rewrites/deletes run.json, deletes grid.rwg + run dir). rw-sat frame writes go through the same ingest writer path — implementer of Task 7 must verify and cover whichever writer rw-sat actually uses.
- `rw_store_diff` bin (crates/rusty-weather/src/bin/rw_store_diff.rs, 395 lines, manual arg parsing): `compare()` (header/meta-sans-writer.build/index-with-offset-normalized/payload comparison), `meta_without_build()`, `record_at()`, `read_writer_build()`, `build_matches()`. `scripts/determinism_check.ps1` depends on its CLI surface — do not change its args or output format.
- Existing rw-store tests: in-file unit tests + `crates/rw-store/tests/ingest_roundtrip.rs` + `tests/pressure3d.rs`. No fixture files committed anywhere. Test dirs use `std::env::temp_dir()` + pid.
- Bin arg-parse pattern: clap derive, all `#[arg(long)]`, see `rw_ingest.rs`.
- **Reviewer hygiene (standing policy):** discard any workspace-wide rustfmt churn (`git checkout -- .` on untouched files) and `.verify*/` scratch dirs before commit/merge.

**Working directory for ALL commands: `C:\Users\drew\rusty-weather` (NOT rustwx). Branch: `rw-store-hardening` off `main`.**

---

### Task 1: Validation library (`rw_store::validate`)

**Files:**
- Create: `crates/rw-store/src/validate.rs`
- Modify: `crates/rw-store/src/lib.rs` (add `pub mod validate;` + re-export `ValidationReport`, `validate_hour_file`, `validate_run_dir`, `ValidateDepth`)
- Test: in-file `#[cfg(test)]` in `validate.rs`

**API contract:**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateDepth {
    /// Header, meta, index geometry, payload bounds — no decompression.
    Structural,
    /// Structural + decompress every chunk, verify raw_len, stats, flags.
    Deep,
}

#[derive(Debug, Default)]
pub struct ValidationReport {
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
    /// (variables, chunks, payload_bytes) observed.
    pub stats: ValidationStats,
}

#[derive(Debug, Default)]
pub struct ValidationStats {
    pub variables: usize,
    pub chunks: u64,
    pub payload_bytes: u64,
}

impl ValidationReport {
    pub fn is_ok(&self) -> bool { self.errors.is_empty() }
}

pub fn validate_hour_file(path: &Path, depth: ValidateDepth) -> RwResult<ValidationReport>;
pub fn validate_run_dir(run_dir: &Path, depth: ValidateDepth) -> RwResult<ValidationReport>;
```

`RwResult::Err` only for I/O failure opening the file; **format problems land in `report.errors`** so the CLI can print all of them, not bail at the first.

**Checks — Structural (hour file):**
1. Header parses (delegate to `RwsHeader::parse` — magic, version, offset consistency, overflow guards).
2. Meta region `[64, 64+meta_len)` in bounds, valid UTF-8, parses as `RwsHourMeta`, `schema == SCHEMA_HOUR`, nx>0, ny>0.
3. Variable ids unique, names unique, kind ∈ {"surface2d","pressure3d"}, codec matches kind (`zstd1_f32`/`zstd1_affine_i16`), pressure3d has non-empty `levels_hpa`, surface2d has empty `levels_hpa`.
4. Index region in bounds; every record parses; `var_id` exists in meta; `kind` matches the variable's kind (0↔surface2d, 1↔pressure3d); flags ⊆ (EMPTY|CONSTANT|HAS_MISSING); reserved bytes zero (warning if not).
5. Index strictly ascending by `sort_key()` — duplicates are errors.
6. Tile coordinates in range: for TILE2D, `tile_y < ceil(ny/256)`, `tile_x < ceil(nx/256)`; for COLUMN3D, `tile_y < ceil(ny/16)`, `tile_x < ceil(nx/16)`.
7. Payload spans: each `[offset, offset+len)` within `[payload_offset, file_len]`; records sorted by offset must not overlap; `len == 0` iff flags has EMPTY or (CONSTANT without HAS_MISSING).
8. Expected raw_len per record: TILE2D ⇒ `4 * th * tw`; COLUMN3D ⇒ `2 * ch * cw * levels` (th/tw/ch/cw edge-clipped from nx/ny and chunking in meta). EMPTY/CONSTANT-no-missing ⇒ raw_len 0.
9. File length: `file_len == max over records of (offset+len)` (== payload_offset when no payloads). Trailing bytes ⇒ error.
10. Per-variable chunk-set completeness: every (tile_y, tile_x) in the tiling grid present exactly once per variable (missing chunk ⇒ error).

**Checks — Deep (adds):**
11. Every non-empty payload zstd-decompresses (use `zstd::bulk::decompress` with capacity raw_len, same cap discipline as reader.rs) to exactly raw_len bytes.
12. TILE2D: decoded f32 count == th*tw; finite-count == valid_count; finite min/max == record min/max bit-tolerant (compare as f32 equality; NaN min/max only when valid_count==0); HAS_MISSING ⇔ any non-finite.
13. COLUMN3D: i16 count == ch*cw*levels; count of non-MISSING_Q == valid_count; HAS_MISSING ⇔ any MISSING_Q; CONSTANT ⇒ scale==0.0 and all non-missing q==0.
14. EMPTY records: valid_count==0, min/max NaN, payload absent.

**Checks — `validate_run_dir`:**
1. `run.json` parses, schema `"rw-store.run.v1"`.
2. `grid.rwg` exists, parses via `GridFile::open`, file sha256 == manifest `grid_hash`, grid nx/ny == manifest nx/ny.
3. Every `hours` entry's `file` exists in the dir; that hour file passes `validate_hour_file` at the requested depth; its meta model/run/grid_hash/nx/ny match the manifest; its variable names ⊇ the manifest entry's `variables` list (warning if manifest list is stale subset, error if hour file is missing one).
4. Stray `f*.rws` files in the dir not referenced by the manifest ⇒ warning.
5. `.rw-lock` / `.*.tmp-*` files ⇒ ignored (note in FORMAT.md).

- [ ] **Step 1: Write failing tests** in `validate.rs` `#[cfg(test)]`. Build a small valid hour via `HourWriter` (e.g. 40×30 grid, one 2D var with a NaN hole, one 3D var with 3 levels, writer_build "validate-test"), `finish()` to a temp dir, assert `validate_hour_file(&path, Deep)` is_ok with 2 variables. Then corruption cases, each starting from a fresh copy of the valid bytes:
  - truncate last 10 bytes ⇒ error mentioning length/bounds
  - swap two index records (break sort) ⇒ error mentioning sort/order
  - overwrite 4 bytes mid-payload of a compressed chunk with 0xFF ⇒ Deep error mentioning decompress (Structural stays ok)
  - set one record's raw_len += 2 ⇒ error
  - append 4 junk bytes ⇒ error mentioning trailing
  - set version field to 9 ⇒ error (unsupported version) without panic
  Each mutation: read file to Vec, mutate at computed offsets (parse header first to find regions), write to a new temp file.
- [ ] **Step 2: Run** `cargo test -p rw-store validate` — expect compile failure (module missing), then after stubbing, failures.
- [ ] **Step 3: Implement** `validate.rs` per the contract. Reuse `RwsHeader::parse`, `ChunkRecord::unpack`, `format.rs` constants. No panics on hostile input — every slice access bounds-checked, every alloc capped by file_len.
- [ ] **Step 4: Run** `cargo test -p rw-store` — all pass, including pre-existing tests.
- [ ] **Step 5: Commit** `feat(rw-store): validation library (structural + deep) for hour files and run dirs`

---

### Task 2: Golden fixtures pinning format v1

**Files:**
- Create: `crates/rw-store/tests/golden.rs`
- Create (generated, committed): `crates/rw-store/tests/golden/v1/f000.rws`, `grid.rwg`, `run.json`, `expected.json`
- Create: `.gitattributes` entry (repo root): `crates/rw-store/tests/golden/** -text`
- Test: `tests/golden.rs`

**Fixture definition (all literal, no clock, no RNG):** model `"golden"`, run `"20260101_00z"`, forecast_hour 0, writer_build `"golden-v1"`, written_unix `1_770_000_000`, encode_ms `0`. Grid ny=300, nx=20 (two 2D tile rows — pins tile ordering; 19×2 column chunks — pins edge clipping). Coordinates: `lat[gy*nx+gx] = 30.0 + 0.01*gy as f32`, `lon = -100.0 + 0.05*gx as f32`, projection None.

Variables:
- `t2m` (surface2d, units "K", selector `{"var":"TMP","level":"2 m above ground"}`): `v = 280.0 + (0.1*gx as f32).sin() * 5.0 + 0.02*gy as f32`
- `mask_demo` (surface2d, units "1", selector `{"var":"MASK"}`): same formula but `NaN` for all `gy >= 256` (⇒ second tile row entirely EMPTY) and `NaN` where `gx == 3 && gy < 10` (⇒ HAS_MISSING in tile 0)
- `const_demo` (surface2d, units "Pa", selector `{"var":"CONST"}`): all values exactly `101325.0` (⇒ CONSTANT tiles)
- `temp_iso` (pressure3d, units "K", levels_hpa `[850, 700, 500]`, selector `{"var":"TMP","level":"{level} mb"}`): `v = 270.0 - (level_idx as f32)*10.0 + (0.05*gx as f32).cos() + 0.01*gy as f32`, with the full column at `gx==5, gy==5` NaN across all levels (⇒ HAS_MISSING chunk)

Write via `HourIngestWriter::begin(store_root, ...)` + `add_field_2d`×3 + `add_volume` + `finish(1_770_000_000)` so grid.rwg and run.json are produced too. (`begin` will acquire the Task 7 lock once that lands — harmless in tests.)

`expected.json` (committed, generated by regen): grid_hash hex, per-variable spot checks: `t2m` full_2d values at flat indices 0, 4105 (gy=205,gx=5), 5999; `temp_iso` profile at (fx=5.5, fy=10.5) all 3 levels; `mask_demo` NaN positions confirmed at index 3 and 256*20; window read of `t2m` (x0=2,y0=250,x1=10,y1=270) checksum = sum of finite values as f64 printed with 6 decimals.

- [ ] **Step 1: Write `tests/golden.rs`** with two tests + one ignored regen:
  - `golden_v1_bytes_are_stable`: rebuild the hour in a temp dir from the literal definition above, byte-compare `f000.rws` and `grid.rwg` against the committed fixtures (`include_bytes!` won't work for runtime paths — read with `fs::read` relative to `env!("CARGO_MANIFEST_DIR")`). run.json compared as parsed JSON equality (pretty-print formatting is serde-stable, but compare structurally to be safe).
  - `golden_v1_reader_values_match_expected`: open committed fixtures with `HourReader`/`GridFile`, assert every spot value in `expected.json` (exact bit equality for 2D f32 reads — the 2D codec is lossless; 3D profile values within `1e-3` relative — quantized), assert `validate_hour_file(Deep)` and `validate_run_dir(Deep)` pass, assert NaN positions.
  - `#[ignore] regen_golden_v1`: writes the fixture files + expected.json into the source tree (`CARGO_MANIFEST_DIR/tests/golden/v1/`), printing a loud warning that committing this constitutes a format change. Also asserts reader-vs-formula agreement (2D exact, 3D within quantization tolerance) so a regen can't silently pin garbage.
- [ ] **Step 2: Run** `cargo test -p rw-store --test golden` — fails (no fixtures yet).
- [ ] **Step 3: Generate fixtures**: `cargo test -p rw-store --test golden -- --ignored regen_golden_v1`. Add `.gitattributes` line. `git add` the four fixture files + .gitattributes. Verify fixture total size is sane (< ~200 KB; the smooth formulas zstd well).
- [ ] **Step 4: Run** `cargo test -p rw-store --test golden` — both non-ignored tests pass. Sanity: flip one byte in a copy, confirm `golden_v1_bytes_are_stable` would catch it (do this as a scratch check, not a committed test).
- [ ] **Step 5: Commit** `test(rw-store): golden v1 fixtures pin on-disk bytes and reader behavior`

---

### Task 3: `docs/FORMAT.md` — the written spec

**Files:**
- Create: `docs/FORMAT.md`
- Modify: `README.md` (rw-store section: add `Format spec: docs/FORMAT.md` line)

Sections (full byte tables from the **Context** block above — transcribe and double-check each against `header.rs`/`index.rs`/`grid.rs`/`format.rs`/`codec.rs`, citing file paths):

1. **Overview & guarantees** — one self-contained file per forecast hour; LE byte order everywhere; mmap-friendly; atomic visibility.
2. **`.rws` layout** — header table, meta JSON schema (field-by-field incl. selector semantics and the `{"derived": slug}` marker), index record table, payload region. Chunk geometry: 256×256 tiles row-major-within-tile; 16×16 column chunks `[y][x][levels_hpa-order]`, edge clipping rule. Index sort order and the binary-search contract.
3. **Codecs** — `zstd1_f32`: raw f32 LE, zstd level 1. `zstd1_affine_i16`: `value = center + q*scale`, `q=i16::MIN` ⇒ NaN, q range [MIN+1, MAX], center/scale derivation in f64, CONSTANT/EMPTY/HAS_MISSING flag semantics incl. payload-present rules. **Explicit quantization disclosure**: 3D values are lossy (~range/65534 absolute step per chunk); 2D is lossless.
4. **`.rwg` layout** + grid_hash definition (sha256 of file bytes) and its role as run identity.
5. **`run.json` schema.**
6. **Versioning policy** — v1 is frozen by the golden fixtures in `crates/rw-store/tests/golden/v1/`; any byte-layout/codec/flag/kind/geometry change bumps `VERSION` and extends `SUPPORTED_VERSIONS`; reserved bytes write-zero/read-ignore; unknown meta JSON keys are reader-ignored (additive metadata allowed without bump); schema strings (`rw-store.hour.v1` etc.) version independently; golden fixtures for every released version are kept forever and gate CI; writers always emit the newest version.
7. **Concurrency contract** — single writer per run directory, enforced by an advisory lock on `<run-dir>/.rw-lock` (fs4; auto-released on process death; the file itself is never deleted). Readers are lock-free: every mutation is temp+fsync+rename so a reader never observes a partial file. Known Windows caveat: a file deletion (sat rolling-window prune) can fail while a reader has it mmapped; pruners must tolerate the failure and retry next cycle. Files matching `.*.tmp-*` and `.rw-lock` must be ignored by readers/tools.
8. **Interop** — NetCDF3 export (`rws export`, Task 6 mapping summary), and the dims/attrs scheme.
9. **Conformance** — `rws validate --deep`, golden fixture tests, `rw_store_diff`/`rws diff` equivalence semantics (writer.build masked).

- [ ] **Step 1: Write `docs/FORMAT.md`** per the outline. Every byte offset and constant cross-checked against source; cite `crates/rw-store/src/<file>.rs` for each section.
- [ ] **Step 2: Verify** every numeric claim by re-reading the cited source lines (this is a documentation conformance pass — list each claim → source line in the task report).
- [ ] **Step 3: README link.** Run `cargo test -p rw-store` (unchanged, but confirms tree still green).
- [ ] **Step 4: Commit** `docs: FORMAT.md — rw-store v1 byte-level spec, versioning policy, concurrency contract`

---

### Task 4: `HourReader::read_full_3d`

**Files:**
- Modify: `crates/rw-store/src/reader.rs`
- Test: in-file tests + extend `crates/rw-store/tests/pressure3d.rs`

**API:** `pub fn read_full_3d(&self, name: &str) -> RwResult<Vec<f32>>` — returns **level-major** `[level][y][x]` (len = levels*ny*nx), NaN for missing. Implementation: iterate the variable's COLUMN3D records (contiguous index range via the existing binary-search helpers), decode each chunk **once**, scatter `[y][x][z]` chunk order into the level-major output. Error `UnknownVariable` / wrong-kind `Format` error consistent with `read_column_3d`'s behavior.

- [ ] **Step 1: Failing test** in `tests/pressure3d.rs`: write a 3-level volume (use the existing test's synthetic data builder), then assert for every (ix, iy) sampled on a coarse lattice (every 7th x, every 5th y, plus all 4 corners): `read_full_3d` slice `[lvl*ny*nx + iy*nx + ix]` bit-equals `read_column_3d(name, ix, iy)[lvl]`. Include a NaN column and a constant chunk in the input. Also: unknown var name errors; calling it on a 2D var errors.
- [ ] **Step 2: Run** `cargo test -p rw-store --test pressure3d` — compile failure.
- [ ] **Step 3: Implement** (decode-once scatter; reuse the chunk-range lookup `read_column_3d` uses).
- [ ] **Step 4: Run** `cargo test -p rw-store` — green.
- [ ] **Step 5: Commit** `feat(rw-store): HourReader::read_full_3d level-major full-volume read`

---

### Task 5: NetCDF3 (CDF-2) writer module

**Files:**
- Create: `crates/rw-store/src/netcdf3.rs`
- Modify: `crates/rw-store/src/lib.rs` (`pub mod netcdf3;`)
- Test: in-file tests including a ~120-line **independent** mini-parser (parses our own output from the spec, not via the writer's code paths)

Self-contained NetCDF "classic 64-bit offset" (CDF-2) writer. **Everything big-endian.** No new deps.

**Layout to emit** (NetCDF classic format spec):
```
magic            'C' 'D' 'F' 0x02
numrecs          u32 BE = 0                  (no record/unlimited dims)
dim_list         tag 0x0000_000A, nelems u32, then per dim: name, len u32
gatt_list        tag 0x0000_000C, nelems u32, then per attr (or 0x0,0x0 if none)
var_list         tag 0x0000_000B, nelems u32, then per var:
                   name, ndims u32, dimid u32 × ndims,
                   vatt_list (as gatt_list), nc_type u32, vsize u32, begin u64 (CDF-2)
data             per var at begin: row-major BE values, contiguous, 4-byte aligned
```
- `name` encoding: u32 BE length, bytes, zero-pad to 4-byte multiple.
- Attr value encoding: NC_CHAR(=2): u32 nelems = byte count, bytes, pad to 4. NC_FLOAT(=5): u32 nelems = count, f32 BE values. NC_SHORT(=3)/NC_INT(=4) analogous (only CHAR and FLOAT needed here).
- `nc_type` for all data vars: NC_FLOAT = 5.
- `vsize` = product(dim lens) × 4, rounded up to multiple of 4 (f32 already aligned); if it would exceed `u32::MAX - 3`, clamp to `u32::MAX` per spec (readers use dims).
- `begin` offsets: header serialized twice — first pass with begin=0 to measure exact header length, then begins assigned sequentially (header_len, +vsize, …), second pass emits real values. Begins must be ≥ header length and ascending in var_list order.
- Empty dim/gatt/vatt lists emit `0x0000_0000 0x0000_0000` (absent-tag + zero nelems).

**API:**
```rust
pub const NC_CHAR: u32 = 2;
pub const NC_FLOAT: u32 = 5;

pub enum Nc3AttrValue { Text(String), Floats(Vec<f32>) }
pub struct Nc3Attr { pub name: String, pub value: Nc3AttrValue }
pub struct Nc3Dim { pub name: String, pub len: usize }            // index = dimid
pub struct Nc3VarDef {
    pub name: String,
    pub dimids: Vec<usize>,
    pub attrs: Vec<Nc3Attr>,
}

pub struct Nc3Writer { /* dims, gattrs, vars, out: BufWriter<File>, state */ }

impl Nc3Writer {
    /// Validates defs (dim ids in range, names non-empty and NC-safe
    /// ([A-Za-z0-9_+-.@] start alnum/underscore), no duplicate names),
    /// writes the full header, returns writer positioned for var 0's data.
    pub fn create(path: &Path, dims: Vec<Nc3Dim>, gattrs: Vec<Nc3Attr>,
                  vars: Vec<Nc3VarDef>) -> RwResult<Self>;
    /// Must be called once per var, in definition order; `values.len()`
    /// must equal the product of the var's dim lens. Writes BE f32.
    pub fn write_var(&mut self, values: &[f32]) -> RwResult<()>;
    /// Errors unless every var was written. Flushes + syncs.
    pub fn finish(self) -> RwResult<()>;
}
```
(All-fixed-size vars ⇒ no record writing needed. Misuse — wrong order/length/missed var — is an `RwStoreError::Format`.)

- [ ] **Step 1: Failing tests** with the independent parser. Tests:
  - `cdf2_minimal_file_parses`: dims y=3,x=2; one global attr `Conventions="CF-1.6"`; vars `lat(y,x)` `lon(y,x)` `t2m(y,x)` each with units + `_FillValue` NaN attr; write known values; parser asserts magic `CDF\x02`, numrecs 0, dim names/lens, attr round-trip, var dims/type, begin ≥ header end, ascending begins, vsize, and exact BE value round-trip including a NaN.
  - `cdf2_name_padding_exact`: var named length-5 (e.g. "lat_q") ⇒ padded to 8; assert pad bytes are zero at computed offsets.
  - `cdf2_misuse_errors`: wrong value count, write_var called too many times, finish before all vars ⇒ Format errors.
  - `cdf2_3d_var_layout`: dims (level=2, y=3, x=2), var `temp(level,y,x)`; assert flat index `lvl*6 + y*2 + x` lands at begin + idx*4.
- [ ] **Step 2: Run** `cargo test -p rw-store netcdf3` — compile failure.
- [ ] **Step 3: Implement** (two-pass header serialization as above).
- [ ] **Step 4: Run** `cargo test -p rw-store` — green.
- [ ] **Step 5: Commit** `feat(rw-store): dependency-free NetCDF3 (CDF-2) writer`

---

### Task 6: Hour → NetCDF3 export glue

**Files:**
- Create: `crates/rw-store/src/export.rs`
- Modify: `crates/rw-store/src/lib.rs` (`pub mod export;` + re-export `export_hour_to_netcdf3`)
- Test: in-file tests using the Task 5 mini-parser (move the parser to `crates/rw-store/src/netcdf3.rs` `#[cfg(test)] pub(crate)` or a `tests/` common module so both can use it)

**API:**
```rust
pub fn export_hour_to_netcdf3(
    hour: &HourReader,
    grid: &GridFile,
    vars: Option<&[String]>,   // None = all variables in the hour
    out: &Path,
) -> RwResult<ExportSummary>;

pub struct ExportSummary { pub variables: usize, pub bytes_written: u64 }
```

**Mapping rules:**
- Dims: `y` (ny), `x` (nx). For each **distinct** `levels_hpa` vector among exported 3D vars (first-seen order): dim `level` / `level2` / `level3`… with a same-named f32 coordinate var holding the hPa values, attrs `units="hPa"`, `long_name="pressure level"`, `positive="down"`.
- Coordinate vars `lat(y,x)`, `lon(y,x)` from `GridFile` (attrs `units="degrees_north"`/`"degrees_east"`, `long_name`).
- Every data var: NC_FLOAT, attrs `units` (from meta), `long_name` = variable name, `coordinates = "lat lon"`, `_FillValue` = f32 NaN. 2D vars on (y,x) via `read_full_2d`; 3D vars on (levelN,y,x) via `read_full_3d` (level-major — matches directly).
- 3D vars additionally get `rw_quantization = "affine-i16 per 16x16-column chunk at ingest; ~ (chunk max-min)/65534 absolute step"` — the scientific-honesty disclosure.
- Global attrs: `Conventions="CF-1.6"`, `title="rusty-weather rw-store export"`, `model`, `run`, `forecast_hour` (as Floats with one value), `grid_hash`, `source = "rw-store {schema} via rws export, writer {name} {version} {build}"`, `comment` pointing at docs/FORMAT.md URL.
- Var name sanitation: rw-store names are already `[a-z0-9_]`; assert NC-safe, error otherwise (no silent renames).
- `vars` filter: unknown name ⇒ `UnknownVariable` error listing available names. Memory: process one variable at a time (read → write_var → drop) — peak is one decoded 3D volume.

- [ ] **Step 1: Failing test**: build a synthetic hour (reuse the golden-fixture builder shape: 2D with NaN, constant 2D, one 3-level 3D, 300×20 grid) + grid in a temp store via `HourIngestWriter`; export all vars; parse with the mini-parser; assert: dim sizes, lat/lon values match formulas, 2D values bit-equal `read_full_2d`, 3D values bit-equal `read_full_3d`, NaN preserved at known holes, `units`/`coordinates`/`rw_quantization`/global attrs present and exact. Second test: `vars=["t2m"]` exports only t2m+lat/lon (+ no level dims); third: unknown var errors.
- [ ] **Step 2: Run** — compile failure.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p rw-store` — green.
- [ ] **Step 5: Commit** `feat(rw-store): NetCDF3 export of hour files (CF-ish attrs, quantization disclosure)`

---

### Task 7: Writer advisory locks (`rw_store::lock`) + integration

**Files:**
- Create: `crates/rw-store/src/lock.rs`
- Modify: `crates/rw-store/Cargo.toml` (add `fs4 = { version = "0.13", default-features = false, features = ["sync"] }` — check docs.rs for the exact current feature name for the std sync API; adjust if the API surface differs, the intent is `try_lock_exclusive` on `std::fs::File`)
- Modify: `crates/rw-store/src/error.rs` (add variant `#[error("run directory locked: {0}")] Locked(String)`)
- Modify: `crates/rw-store/src/ingest.rs` (`HourIngestWriter::begin` acquires, struct holds the guard, releases on drop after `finish`)
- Modify: `crates/rw-sat/src/window.rs` (`enforce_window`: per run dir, `try_acquire`; if locked, skip that dir this pass with a log line, retry next cycle)
- Modify: `crates/rw-sat/src/follow.rs` IF its frame-write path does not go through `HourIngestWriter` — wrap whatever writes/renames frames + manifest in the same `RunLock`. **First action of this task: read follow.rs/ingest path and report which case holds.**
- Test: in-file tests in `lock.rs` + one integration test in `crates/rw-store/tests/ingest_roundtrip.rs`

**API:**
```rust
pub struct RunLock { /* file: std::fs::File, path: PathBuf */ }

pub const LOCK_FILE_NAME: &str = ".rw-lock";

impl RunLock {
    /// Open-or-create `<run_dir>/.rw-lock` and try to take the exclusive
    /// advisory lock without blocking. Ok(None) when another writer holds it.
    pub fn try_acquire(run_dir: &Path) -> RwResult<Option<RunLock>>;

    /// Poll try_acquire every 100 ms up to `timeout`; then Err(Locked(..))
    /// naming the path and wait duration.
    pub fn acquire(run_dir: &Path, timeout: Duration) -> RwResult<RunLock>;
}

impl Drop for RunLock { /* unlock + close; NEVER delete the lock file */ }
```
Semantics to document in the module header (and already promised in FORMAT.md §7): advisory only; auto-released on process death (OS lock, not pidfile); lock file persists empty; readers never lock.

`HourIngestWriter::begin` uses `RunLock::acquire(run_dir, Duration::from_secs(60))` after creating the run dir. 60 s default because a competing hour encode finishing is the normal contention case; the error message tells the user who/what to check.

- [ ] **Step 1: Failing tests** in `lock.rs`:
  - `second_acquire_fails_while_held`: acquire in temp dir; `try_acquire` again ⇒ `Ok(None)`; drop first; `try_acquire` ⇒ `Ok(Some(_))`. (fs4 file locks are per-handle on both Windows (LockFileEx) and Unix (flock on a fresh fd), so two handles in one process behave like two processes — note this in the test comment.)
  - `acquire_times_out_with_locked_error`: hold lock, `acquire(dir, 250ms)` ⇒ `Err(RwStoreError::Locked(_))` and elapsed ≥ 250 ms.
  - `lock_file_persists_after_release`: drop guard ⇒ `.rw-lock` still exists, zero-length.
  - Integration (ingest_roundtrip.rs): begin an `HourIngestWriter`; from the test thread `RunLock::try_acquire(run_dir)` ⇒ `Ok(None)`; after `finish`, ⇒ `Ok(Some(_))`.
- [ ] **Step 2: Run** `cargo test -p rw-store lock` — compile failure.
- [ ] **Step 3: Implement** lock.rs + error variant + ingest integration. Then read the rw-sat write path and integrate (`enforce_window` skip-if-locked + frame path if needed). `cargo build -p rw-sat`.
- [ ] **Step 4: Run** `cargo test -p rw-store -p rw-sat -p rw-ingest` — green. Also `cargo test --workspace` once here (lock touches the hot ingest path used by many crates' tests).
- [ ] **Step 5: Commit** `feat(rw-store): writer advisory locks per run dir; sat window prune skips locked dirs`

---

### Task 8: `rws` CLI + diff logic lifted into the library

**Files:**
- Create: `crates/rw-store/src/diff.rs` (move `compare`, `Difference`, `meta_without_build`, `record_at`, `read_writer_build`, `build_matches` from the bin **verbatim** — only visibility/`use` changes; keep its unit tests if any, else add a round-trip test: two byte-identical files compare equal; flip a payload byte ⇒ payload difference; rewrite writer.build inside meta JSON (same length or re-pack via HourWriter with different build) ⇒ still equal)
- Modify: `crates/rw-store/src/lib.rs` (`pub mod diff;`)
- Modify: `crates/rusty-weather/src/bin/rw_store_diff.rs` → thin wrapper: identical CLI args/output/exit codes, body delegates to `rw_store::diff`. **`scripts/determinism_check.ps1` must run unmodified afterwards.**
- Create: `crates/rusty-weather/src/bin/rws.rs`
- Test: lib tests in `diff.rs`; CLI smoke is Step 4 (manual, the logic under every subcommand is library-tested in Tasks 1–6)

**CLI spec (clap derive, workspace clap 4.5):**
```
rws ls <path> [--json]            # store root, model dir, or run dir: walks
                                  # run.json manifests; prints model/run/hours/
                                  # vars/sizes (file sizes from disk)
rws dump <hour.rws> [--var NAME] [--json]
                                  # header + meta summary; with --var: that
                                  # variable's index records (tile coords,
                                  # flags, len/raw_len, min/max, valid_count)
rws validate <path> [--deep] [--json]
                                  # file ⇒ validate_hour_file; dir ⇒
                                  # validate_run_dir. Prints errors+warnings;
                                  # exit 0 iff is_ok()
rws diff <a.rws> <b.rws>          # rw_store::diff::compare; exit 0 same/1 diff
rws export <hour.rws> --grid <grid.rwg> -o <out.nc> [--vars t2m,temp_iso]
                                  # export_hour_to_netcdf3; if --grid omitted,
                                  # look for grid.rwg next to the hour file
```
`--json` modes print one serde_json object (machine-readable for bowecho tooling). Human mode: aligned columns like rw_bench output. Subcommand enum + one struct per subcommand; `fn main() -> std::process::ExitCode`.

- [ ] **Step 1: Move diff into the library** (failing build → fix imports), add the diff lib tests. `cargo test -p rw-store diff`.
- [ ] **Step 2: Rewrite `rw_store_diff.rs` as a wrapper.** `cargo build -p rusty-weather --bin rw_store_diff`, then run it against two fixture copies to confirm identical output ("identical"/exit 0) and against a mutated copy (exit 1).
- [ ] **Step 3: Implement `rws.rs`** per spec.
- [ ] **Step 4: Smoke each subcommand against the golden fixtures** (`cargo run --release -p rusty-weather --bin rws -- validate crates/rw-store/tests/golden/v1 --deep`, `ls`, `dump --var temp_iso`, `diff` fixture vs itself, `export` to a temp .nc) — paste outputs into the task report.
- [ ] **Step 5: Run** `cargo test --workspace` — green; `powershell -File scripts/determinism_check.ps1` if it runs standalone without a store (if it needs live store data, instead diff two copies of the golden fixture with the wrapped bin and note that).
- [ ] **Step 6: Commit** `feat(rusty-weather): rws CLI (ls/dump/validate/diff/export); diff logic lifted to rw-store`

---

### Task 9: README, live verification, bowecho handoff notes

**Files:**
- Modify: `README.md` (new "Inspecting and exporting stores (`rws`)" subsection under rw-store: the five subcommands with one-line examples; the xarray snippet below; FORMAT.md link if Task 3's went elsewhere)
- Create: nothing else

xarray snippet for README:
```python
import xarray as xr
ds = xr.open_dataset("f006.nc")    # scipy backend reads NetCDF3 natively
print(ds["t2m"].sel(y=500, x=900).values)
```

- [ ] **Step 1: Real-store validation**: `cargo run --release -p rusty-weather --bin rws -- validate store/hrrr/20260608_00z --deep` (the real HRRR run on disk). Expect 0 errors; warnings allowed but each must be explainable. If errors appear: STOP, investigate — either a validator bug or a real latent store bug; do not paper over.
- [ ] **Step 2: Real export**: `cargo run --release -p rusty-weather --bin rws -- export store/hrrr/20260608_00z/f006.rws -o out/f006_subset.nc --vars temperature_2m,dewpoint_2m,temperature_iso` (use real variable names from `rws ls`; include at least one 3D var). Note wall time + output size.
- [ ] **Step 3: xarray verification**: try `python -c "import xarray, sys; ds = xarray.open_dataset(sys.argv[1]); print(ds)" out/f006_subset.nc` locally; if no local python/xarray, scp the file to node3 (drew's nodes memory: SSH from PowerShell) and run there, or fall back to `python -c "from scipy.io import netcdf_file; f = netcdf_file(...)"`. Required assertions: opens without error; dims/vars listed; one spot value equals `rws dump`/reader value; lat/lon ranges sane (CONUS). Paste output.
- [ ] **Step 4: Full gate**: `cargo test --workspace` (expect ≥ 983 + new tests, all green), `cargo build --release -p rusty-weather -p rusty-weather-ui`. `git status` — tree clean except intended changes (discard rustfmt churn per policy).
- [ ] **Step 5: Update README** + commit `docs: rws CLI usage, xarray interop example`
- [ ] **Step 6: Write the bowecho handoff note** (goes in the final report to drew, not a file): additive API summary — new modules `validate`/`netcdf3`/`export`/`diff`/`lock`, new `HourReader::read_full_3d`, new error variant `RwStoreError::Locked` (**breaking for exhaustive matches on RwStoreError**), `HourIngestWriter::begin` now blocks up to 60 s on a contended run dir and can return `Locked`, `.rw-lock` files now appear in run dirs (ignore them), sat window pruning skips locked dirs for a cycle. No existing signatures changed.

---

## Self-review notes

- Spec coverage: interop → Tasks 5/6/9; spec-by-implementation → Tasks 2/3; concurrency → Task 7; migration policy → Task 3 §6 + Task 2 fixtures; tooling → Tasks 1/8. Range reads (Wave 2) intentionally absent — deferred pending a hosting decision.
- Type consistency: `ValidationReport/ValidateDepth` (T1) consumed by T2 tests and T8 CLI; `read_full_3d` (T4) consumed by T6; `Nc3Writer` (T5) consumed by T6; `RunLock`/`Locked` (T7) referenced by T2's fixture builder note and FORMAT.md §7 (written in T3 as a forward promise — T7 fulfills it; if T7's implementation diverges (e.g. skip-if-locked semantics change), T7's implementer must update FORMAT.md §7 to match).
- Order matters: T1 before T2 (fixtures assert validate passes); T4 before T6; T5 before T6; T3 promises T7 — acceptable forward reference, see above. T8 last-but-one because it fronts everything.

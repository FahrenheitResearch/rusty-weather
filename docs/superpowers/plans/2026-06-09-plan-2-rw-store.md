# rusty-weather Plan 2: rw-store — the unified storage format

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Build `rw-store` — the single format replacing both old wxstores — with real windowed 2D reads, column-chunked 3D for sub-100ms soundings, and an `rw_ingest` binary that takes a live HRRR hour from GRIB to `.rws` on disk, benchmarked.

**Architecture:** One self-contained hour file (`f###.rws`: header → JSON meta → binary chunk index → zstd payload) per (model, run, forecast hour), plus per-run `grid.rwg` (lat/lon arrays + projection, stored once) and `run.json` manifest. 2D fields = 256×256 f32 tiles, zstd-1, decoded per-window. 3D pressure fields = 16×16-column chunks × all levels, affine-i16 quantized (ported verbatim from the old volume store) then zstd-1, laid out column-contiguous so a sounding decodes 1–4 small chunks via mmap. Writes are rayon-parallel across chunks and atomic (temp+fsync+rename). **Speed is the prime directive** — every task with a perf surface states a target and measures it.

**Tech Stack:** Rust; new deps for rw-store only: `zstd = "0.13"`, `memmap2 = "0.9"` (same versions the old repo used). Port sources live in the READ-ONLY worktree `C:\Users\drew\rustwx-fastplots-wt` (cited as `WT:` below).

**Spec:** `docs/superpowers/specs/2026-06-09-rusty-weather-design.md` (section "Store v2"). Branch: `plan-2-rw-store`.

---

## Ground rules

- Same R1/no-stub/commit discipline as Plan 1 (see `2026-06-09-rusty-weather-extraction.md` ground rules; they apply verbatim).
- Ported code (codec, atomic write) is adapted minimally: rename types into rw-store's namespace, no logic changes; cite the WT: source in a comment at the top of each ported module (`// Ported from rustwx volume_store/codec.rs (review/grib-wxa-fast-plots-20260605)`).
- Every perf number measured goes into the task's commit message. Perf assertions live in `#[ignore]`d tests (run explicitly), never in default CI runs (machine-dependent).
- TDD for all new logic: failing test → implement → pass → commit.

## Format reference (normative for all tasks)

**Store layout on disk:**

```
<store_root>/
  hrrr/
    20260608_00z/
      run.json        # RwsRunManifest
      grid.rwg        # grid file: header + meta JSON + zstd lat/lon arrays
      f000.rws
      f006.rws
```

**`.rws` hour file:**

```
bytes 0-7    magic  b"RWSTORE1"
bytes 8-11   version u32 LE = 1
bytes 12-15  meta_len u32 LE
bytes 16-23  index_count u64 LE
bytes 24-31  index_offset u64 LE
bytes 32-39  payload_offset u64 LE
bytes 40-63  reserved (zero)
[meta JSON, meta_len bytes]            # RwsHourMeta
[index records, index_count * 64 bytes]
[payload]
```

**Index record (64 bytes, LE):**

```
0-1    var_id u16
2      kind u8        (0 = tile2d, 1 = column3d)
3      flags u8       (EMPTY=1, CONSTANT=2, HAS_MISSING=4)
4-7    tile_y u32     (tile/chunk row index)
8-11   tile_x u32     (tile/chunk col index)
12-19  offset u64     (absolute file offset of compressed payload)
20-23  len u32        (compressed payload length; 0 for EMPTY/CONSTANT)
24-27  raw_len u32    (uncompressed payload length)
28-31  center f32     (quantization center for 3D; the constant value for CONSTANT)
32-35  scale f32      (quantization scale for 3D; 0 for 2D)
36-39  min f32        (valid min, NaN if none)
40-43  max f32
44-47  valid_count u32
48-63  reserved (zero)
```

Records sorted by `(var_id, kind, tile_y, tile_x)` — binary-searchable.

**`RwsHourMeta` (JSON):** `schema: "rw-store.hour.v1"`, `model`, `run` (e.g. `"20260608_00z"`), `forecast_hour: u16`, `nx`, `ny`, `grid_hash` (hex sha256 of grid.rwg content), `variables: Vec<RwsVariableMeta>`, `chunking: { tile_y: 256, tile_x: 256, col_y: 16, col_x: 16 }`, `writer: { name: "rw-store", version: <crate version>, build: <build stamp or "dev"> }`.

**`RwsVariableMeta`:** `id: u16` (the var_id in index records), `name: String` (stable slug, e.g. `"temperature_2m"`, `"temperature_iso"`), `units: String`, `kind: "surface2d" | "pressure3d"`, `codec: String` (`"zstd1_f32"` for 2D, `"zstd1_affine_i16"` for 3D — per-VARIABLE so a future codec lands without a format break; readers must error on unknown codec strings), `levels_hpa: Vec<u16>` (empty for 2D; **descending** order, e.g. [1000, 975, ...] — port the old convention), `selector: serde_json::Value` (the serialized `FieldSelector`, for round-tripping back to `SelectedField2D`).

**Payload encoding:**
- 2D tile (kind=0): the tile's `y_count*x_count` f32 LE values (row-major within tile), zstd level 1. EMPTY (all NaN) and CONSTANT tiles store no payload (flags + center carry the information).
- 3D column chunk (kind=1): footprint up to 16×16 columns × `levels` values, laid out **`[y][x][z]`** (each column's levels contiguous), affine-i16 quantized per chunk (ported codec semantics: `q = round((v-center)/scale)` clamped to `[i16::MIN+1, i16::MAX]`, `i16::MIN` = missing sentinel, `v = center + scale*q`), then zstd level 1. EMPTY/CONSTANT same as 2D.
- Edge tiles/chunks are smaller (`y_count = min(256, ny - tile_y*256)` etc.) — `raw_len` is authoritative.

**Version policy:** reader accepts version 1 only (this is v1); the version-check error message must state both found and supported versions. The N/N−1 policy activates when version 2 ever exists — encode the check as `const SUPPORTED_VERSIONS: &[u32] = &[1];` so v2 adds `2` and keeps `1`.

**Performance targets (measured on the dev machine, warm OS cache unless stated):**

| Operation | Target |
|---|---|
| Encode one full HRRR hour (3D: 5 vars × ~30-40 levels @ 1799×1059; 2D: ~6-8 fields), after extraction | ≤ 5 s |
| Open hour file + read one full 2D field | ≤ 150 ms |
| Windowed 2D read | ∝ window area (a ¼-domain window ≤ ~¼ the full-read decode time + open overhead) |
| Sounding (open + bilinear profile, all 3D vars, all levels) | ≤ 100 ms hard, ≤ 25 ms expected |
| Whole-hour .rws size | ≤ ~250 MB (informational, record actual) |

---

### Task 1: Crate scaffold + error type + format constants

**Files:**
- Create: `crates/rw-store/Cargo.toml`, `crates/rw-store/src/lib.rs`, `crates/rw-store/src/error.rs`, `crates/rw-store/src/format.rs`
- Modify: root `Cargo.toml` (members — `rw-store` sorts before `rustwx-*` and `rusty-weather`)

- [x] **Step 1:** `crates/rw-store/Cargo.toml`:

```toml
[package]
name = "rw-store"
version = "0.1.0"
edition.workspace = true
license.workspace = true
publish.workspace = true
rust-version.workspace = true

[dependencies]
memmap2 = "0.9"
rayon.workspace = true
serde.workspace = true
serde_json.workspace = true
sha2 = "0.10"
thiserror.workspace = true
zstd = "0.13"
rustwx-core = { path = "../rustwx-core" }
```

- [x] **Step 2:** `src/error.rs` — `RwStoreError` via thiserror with variants: `Io(#[from] std::io::Error)`, `Format(String)` (bad magic/header/index), `UnsupportedVersion { found: u32, supported: &'static [u32] }`, `Meta(String)` (JSON/schema problems), `UnknownVariable(String)`, `Chunk(String)` (codec/decode), `Grid(String)`. `pub type RwResult<T> = Result<T, RwStoreError>;`

- [x] **Step 3:** `src/format.rs` — the normative constants: magic bytes, `VERSION: u32 = 1`, `SUPPORTED_VERSIONS: &[u32] = &[1]`, `HEADER_LEN: usize = 64`, `INDEX_RECORD_LEN: usize = 64`, `TILE_Y/TILE_X: usize = 256`, `COL_Y/COL_X: usize = 16`, flag bits (`FLAG_EMPTY: u8 = 1`, `FLAG_CONSTANT: u8 = 2`, `FLAG_HAS_MISSING: u8 = 4`), kind values (`KIND_TILE2D: u8 = 0`, `KIND_COLUMN3D: u8 = 1`), `SCHEMA_HOUR: &str = "rw-store.hour.v1"`. Plus `RwsHourMeta`/`RwsVariableMeta`/`RwsChunking`/`RwsWriterInfo` serde structs exactly as the format reference defines.

- [x] **Step 4:** `lib.rs` declares `pub mod error; pub mod format;` with a crate doc-comment summarizing the format (condense the format reference). `cargo check -p rw-store` green; commit `feat: scaffold rw-store crate with format constants and error type`.

---

### Task 2: Port the affine-i16 codec (TDD: port its tests first)

**Files:**
- Create: `crates/rw-store/src/codec.rs`
- Source: `WT:crates/rustwx-products/src/volume_store/codec.rs` (lines 1–235)

- [x] **Step 1:** Write the test module FIRST in `codec.rs` by porting the old round-trip tests (WT codec.rs:192–235): `affine_i16_round_trip_keeps_error_bounded` (random-ish synthetic values; assert `|decoded - original| <= scale` per finite value, NaN positions preserved), `constant_chunk_uses_no_payload`, `all_missing_chunk_uses_empty_flag`. Adapt to the new types: `pub struct EncodedChunk { pub flags: u8, pub center: f32, pub scale: f32, pub min: f32, pub max: f32, pub valid_count: u32, pub payload: Vec<u8> }` and `pub fn encode_affine_i16(values: &[f32]) -> EncodedChunk`, `pub fn decode_affine_i16(flags: u8, center: f32, scale: f32, payload: &[u8], value_count: usize) -> RwResult<Vec<f32>>`. Run `cargo test -p rw-store codec` → compile failure (functions absent).

- [x] **Step 2:** Implement by porting WT codec.rs:60–189 with these adaptations: flags are the new u8 bits (EMPTY/CONSTANT/HAS_MISSING; the old `FLAG_DENSE_I16` becomes implicit — payload non-empty ⇔ dense, EXCEPT constant-with-missing which keeps a sentinel payload exactly like the old code); sentinel `i16::MIN`, clamp to `[i16::MIN+1, i16::MAX]`, `center = 0.5*(min+max)`, `scale = (max-min)/(2*i16::MAX as f32)`. Cite the port source in the module header comment.

- [x] **Step 3:** Also add `pub fn encode_f32_tile(values: &[f32]) -> EncodedChunk` / `pub fn decode_f32_tile(flags, center, payload, value_count) -> RwResult<Vec<f32>>` — the 2D path: same EMPTY/CONSTANT detection (port the min/max/valid scan), dense case = raw f32 LE bytes (no quantization). Tests: exact round-trip including NaN, constant, empty.

- [x] **Step 4:** All codec tests green. Note: NO zstd in this module — compression is applied by the writer above the codec (keeps codec pure and testable). Commit `feat: rw-store codec — affine-i16 (ported) and f32 tile encode/decode`.

---

### Task 3: Index records + header pack/parse

**Files:**
- Create: `crates/rw-store/src/index.rs`, `crates/rw-store/src/header.rs`

- [x] **Step 1 (TDD):** tests first: `index_record_round_trips_through_64_bytes` (pack → unpack → equality, including edge values), `header_round_trips`, `header_rejects_bad_magic`, `header_rejects_unsupported_version` (assert the error carries found+supported), `records_sort_key_orders_by_var_kind_tile`.

- [x] **Step 2:** `index.rs`: `pub struct ChunkRecord { pub var_id: u16, pub kind: u8, pub flags: u8, pub tile_y: u32, pub tile_x: u32, pub offset: u64, pub len: u32, pub raw_len: u32, pub center: f32, pub scale: f32, pub min: f32, pub max: f32, pub valid_count: u32 }` with `pack_into(&self, out: &mut Vec<u8>)` / `unpack(bytes: &[u8; 64]) -> RwResult<Self>` per the byte layout in the format reference, plus `pub fn sort_key(&self) -> (u16, u8, u32, u32)`. `header.rs`: `pub struct RwsHeader { pub version: u32, pub meta_len: u32, pub index_count: u64, pub index_offset: u64, pub payload_offset: u64 }` with `pack` (writes exactly 64 bytes) / `parse(&[u8]) -> RwResult<RwsHeader>` (validates magic, version ∈ SUPPORTED_VERSIONS, offsets sane: `index_offset == HEADER_LEN + meta_len`, `payload_offset == index_offset + index_count*64`).

- [x] **Step 3:** green; commit `feat: rw-store binary header and chunk index records`.

---

### Task 4: Hour-file writer (2D tiles)

**Files:**
- Create: `crates/rw-store/src/writer.rs`, `crates/rw-store/src/atomic.rs`

- [x] **Step 1:** `atomic.rs` — port `atomic_write_bytes` from `WT:crates/rustwx-products/src/publication.rs:497-523` (temp file `create_new` → `write_all` → `sync_all` → remove-existing → rename, cleanup on error). Unit test: write, overwrite, content correct, no `.tmp` left behind.

- [x] **Step 2 (TDD):** writer test: build two synthetic 2D fields (e.g. 600×500 — forces 3×2 tiling with edge tiles; one field containing a NaN region and a constant region), write via the new API, then assert with raw byte inspection: magic correct, index_count == expected tile count, records sorted, EMPTY/CONSTANT tiles have len==0.

- [x] **Step 3:** implement `writer.rs`:

```rust
pub struct HourWriter { /* model, run, forecast_hour, nx, ny, grid_hash, vars + staged chunks */ }
impl HourWriter {
    pub fn new(model: &str, run: &str, forecast_hour: u16, nx: usize, ny: usize, grid_hash: &str) -> Self;
    pub fn add_surface2d(&mut self, name: &str, units: &str, selector: serde_json::Value, values: &[f32]) -> RwResult<u16>; // returns var_id
    pub fn finish(self, path: &Path) -> RwResult<RwsHourMeta>; // assembles + atomic write
}
```

`add_surface2d` splits into 256×256 tiles (port the chunking loop shape from `WT:wxstore_wxa.rs:464-686` — y0/x0/y_count/x_count math), runs `encode_f32_tile` per tile, zstd-1 compresses dense payloads (`zstd::stream::encode_all(&payload[..], 1)`), records stats. **Rayon:** parallelize per-tile encode+compress with `par_iter` over tile coordinates, collect `(coord, EncodedChunk, compressed)` then assemble serially (deterministic offsets — assembly assigns offsets in sorted order). `finish` builds meta JSON, packs sorted index, concatenates payload, atomic-writes.

- [x] **Step 4:** tests green; commit `feat: rw-store hour writer with rayon-parallel 2D tile encode`.

---

### Task 5: Hour-file reader (open + full + windowed 2D)

**Files:**
- Create: `crates/rw-store/src/reader.rs`

- [x] **Step 1 (TDD):** tests (using Task 4's writer to produce input): `read_full_round_trips_exactly` (f32 2D is lossless — bit-exact incl. NaN), `windowed_read_equals_full_read_crop` (several windows incl. tile-straddling, edge-clamped, and 1×1), `open_rejects_truncated_file`, `open_rejects_corrupt_index` (flip a byte in the index region → decode of affected chunk errors cleanly, not panic), `unknown_variable_errors`.

- [x] **Step 2:** implement:

```rust
pub struct HourReader { mmap: memmap2::Mmap, pub meta: RwsHourMeta, index: Vec<ChunkRecord> }
impl HourReader {
    pub fn open(path: &Path) -> RwResult<Self>;            // mmap (fall back to read-to-RAM on mmap failure, port strategy from WT:volume_store/chunk_payload.rs:28-85)
    pub fn variable(&self, name: &str) -> Option<&RwsVariableMeta>;
    pub fn read_full_2d(&self, name: &str) -> RwResult<Vec<f32>>;
    pub fn read_window_2d(&self, name: &str, x0: usize, y0: usize, x1: usize, y1: usize) -> RwResult<Window2D>; // exclusive upper bounds, clamped to grid
}
pub struct Window2D { pub x0: usize, pub y0: usize, pub nx: usize, pub ny: usize, pub values: Vec<f32> }
```

`read_window_2d` computes the intersecting tile range (`tile_y0 = y0/256 ..= (y1-1)/256` etc.), binary-searches the index per tile, decodes ONLY those tiles (zstd `decode_all` then `decode_f32_tile`), and copies the intersection rows into the output. **This is the windowed-read promise the old wxa never kept — the test in Step 1 is the contract.** `read_full_2d` = window over the whole grid; rayon-parallel tile decode when more than ~8 tiles.

- [x] **Step 3:** green; commit `feat: rw-store reader with true windowed 2D reads`.

---

### Task 6: 3D volumes — writer + column/profile reader

**Files:**
- Modify: `crates/rw-store/src/writer.rs`, `crates/rw-store/src/reader.rs`

- [x] **Step 1 (TDD):** tests: synthetic volume from an analytic function (e.g. `v(x,y,z) = 0.1*x + 0.2*y - 1.5*z`) on a 100×80 grid × 10 levels; `volume_round_trips_within_quantization_bound` (per chunk: `|decoded-orig| <= chunk scale`), `column_read_matches_full_decode` (read_column(ix,iy) == the column extracted from full decode), `profile_bilinear_matches_analytic` (bilinear of analytic field at fractional point, tolerance = quantization bound + epsilon), NaN-column and constant-level cases.

- [x] **Step 2:** writer side: `pub fn add_pressure3d(&mut self, name, units, selector, levels_hpa: &[u16], level_planes: &[&[f32]]) -> RwResult<u16>` — validates levels descending + plane count/len, re-lays planes into 16×16-column chunks with `[y][x][z]` order (each column's levels contiguous), `encode_affine_i16` per chunk, zstd-1, rayon-parallel across chunks. `tile_y/tile_x` in records = column-chunk coordinates (grid divided by 16).

- [x] **Step 3:** reader side: `pub fn read_column_3d(&self, name: &str, ix: usize, iy: usize) -> RwResult<Vec<f32>>` (locate chunk `(iy/16, ix/16)`, decode, slice the column at `((iy%16)*x_count + (ix%16)) * levels .. + levels`) and `pub fn read_profile_3d(&self, name: &str, fx: f64, fy: f64) -> RwResult<Vec<f32>>` (bilinear across the 4 surrounding columns — up to 4 chunk decodes worst case, 1 typical; NaN-aware: a level with any NaN corner falls back to nearest finite corner weighting, port nothing — keep it simple: if any of the 4 is NaN at a level, use weighted mean of finite corners, all-NaN → NaN).

- [x] **Step 4:** green; commit `feat: rw-store 3D column-chunked volumes with profile reads`.

---

### Task 7: Run-level files — grid.rwg + run.json + lat/lon→grid lookup

**Files:**
- Create: `crates/rw-store/src/grid.rs`, `crates/rw-store/src/run.rs`

- [x] **Step 1 (TDD):** tests: grid file round-trip (write LatLonGrid+GridProjection → read → arrays bit-exact, hash stable); `locate_finds_nearest_grid_point` on a synthetic regular grid AND a synthetic curvilinear-ish grid (rotated/sheared coordinates) — assert the returned (ix, iy, fx, fy) brackets the query point; run.json round-trip + hour registration.

- [x] **Step 2:** `grid.rs`: `.rwg` file = same 64-byte header pattern (magic `b"RWSGRID1"`, version, meta_len, then meta JSON `{ schema: "rw-store.grid.v1", nx, ny, projection: GridProjection (serde), lat_offset/lat_len/lon_offset/lon_len }`, then zstd-1 of lat f32 LE array, then zstd-1 of lon array). API: `write_grid(path, &LatLonGrid, Option<&GridProjection>) -> RwResult<String /* sha256 hex of file bytes */>`, `pub struct GridFile { pub nx, ny, pub lat: Vec<f32>, pub lon: Vec<f32>, pub projection: Option<GridProjection>, pub hash: String }` with `GridFile::open(path)`. (`LatLonGrid`/`GridProjection` come from rustwx-core and are serde-ready — verified.)

- [x] **Step 3:** the locator: `pub struct GridLocator { /* coarse index */ }` built lazily from a GridFile — downsample every 8th point into a coarse lat/lon mesh; `locate(lat, lon) -> Option<(f64, f64)>` (fractional grid coords) = nearest coarse cell scan, then local exhaustive refine over the surrounding 17×17 fine points, then bilinear inversion within the winning cell (solve fractional position from the 4 corners — straight 2D inverse bilinear with 2 Newton iterations, NaN-safe). Build time target: < 50 ms for 1799×1059 (measure in an `#[ignore]` test); locate: < 50 µs warm.

- [x] **Step 4:** `run.rs`: `RwsRunManifest { schema: "rw-store.run.v1", model, run, grid_hash, nx, ny, hours: BTreeMap<u16, RwsHourEntry { file, written_unix, encode_ms, variables: Vec<String> }>, writer: RwsWriterInfo }`, `load_or_new`, `register_hour`, atomic save. Commit `feat: rw-store run manifest, grid file, and generic grid locator`.

---

### Task 8: Public ingest API — SelectedField2D in, .rws out

**Files:**
- Create: `crates/rw-store/src/ingest.rs`
- Modify: `crates/rw-store/src/lib.rs` (re-exports: `HourWriter`, `HourReader`, `Window2D`, `GridFile`, `GridLocator`, `RwsRunManifest`, `ingest::*`)

- [x] **Step 1 (TDD):** test with hand-built `SelectedField2D`s (small grid): `write_hour_from_fields` writes grid.rwg (once — second hour reuses, asserts same hash, no rewrite), run.json registers hours, hour file contains all 2D vars + 3D var with the levels assembled in descending order from per-level `SelectedField2D`s, and a read-back through `HourReader` reconstructs a `SelectedField2D` equal to the input (selector, units, grid arrays via GridFile, values bit-exact for 2D).

- [x] **Step 2:** implement:

```rust
pub struct PressureVolumeInput<'a> {
    pub name: &'a str, pub units: &'a str, pub selector_template: serde_json::Value,
    pub levels: Vec<(u16, &'a [f32])>,  // (level_hpa, plane) — any order in, sorted descending internally
}
pub fn write_hour_from_fields(
    store_root: &Path, model: &str, run: &str, forecast_hour: u16,
    fields_2d: &[(&str /* name */, &SelectedField2D)],
    volumes: &[PressureVolumeInput<'_>],
) -> RwResult<WrittenHour> // { path, encode_ms, bytes, vars }
```

Grid consistency: all inputs must share (nx, ny) — error otherwise; grid.rwg written from the first field's `LatLonGrid` + `projection` if absent, else hash-checked. Reconstruction helper: `pub fn read_field_2d(reader: &HourReader, grid: &GridFile, name: &str) -> RwResult<SelectedField2D>`.

- [x] **Step 3:** green; `cargo test --workspace` green; commit `feat: rw-store ingest API — SelectedField2D round-trip`.

---

### Task 9: `rw_ingest` binary — live GRIB → .rws

**Files:**
- Create: `crates/rusty-weather/src/bin/rw_ingest.rs`
- Modify: `crates/rusty-weather/Cargo.toml` (add `rw-store = { path = "../rw-store" }`, re-add `rustwx-io` — it was trimmed as unused in Plan 1)

- [x] **Step 1:** CLI (clap, mirror smoke_direct's arg style): `--model` (default hrrr), `--date`, `--cycle`, `--hours` (e.g. `0,6` or `0-6`), `--store-root` (default `store`), `--cache-dir` (reuse the fetch cache pattern from smoke_direct).

Also add a `build.rs` to `crates/rusty-weather` that captures the git SHA at compile time (`git rev-parse --short=12 HEAD` + `-dirty` suffix when the tree is dirty; fall back to `"unknown"` if git fails) into `RW_BUILD_SHA` via `cargo:rustc-env=`, with `cargo:rerun-if-changed=../../.git/HEAD`. rw_ingest passes `env!("RW_BUILD_SHA")` into `RwsWriterInfo.build` so every store written records exactly which build produced it — this is the spec's day-one deploy-lottery fix and is not optional.

- [x] **Step 2:** Field plan. 2D set: build `FieldSelector`s for: 2m temperature, 2m dewpoint, 10m U, 10m V, MSLP, composite reflectivity, surface CAPE — **verify each exists** as a CanonicalField/selector the HRRR catalog supports by checking `crates/rustwx-core` (CanonicalField variants) and how `rustwx-products` direct recipes build the same selectors (grep for the recipe definitions); use the exact same constructors. 3D set: for each var in [Temperature, Dewpoint (fall back to RelativeHumidity if HRRR prs lacks DPT), U, V, GeopotentialHeight] × candidate levels `(100..=1000).step_by(25)` build `FieldSelector::isobaric(field, hpa)`. Extraction TOLERATES absent selectors (`PartialExtraction.missing`) — request the superset, store what comes back, log the realized level list.

- [x] **Step 3:** Per hour: fetch the HRRR `prs` file (3D + MSLP/CAPE live there) and `sfc` file (2m/10m/reflectivity) via the same `FetchRequest`+cache machinery smoke_direct uses (mirror its fetch code; one download per product file); `extract_fields_partial_from_model_bytes_at_forecast_hour` once per file with ALL its selectors (single decode pass); group isobaric results into `PressureVolumeInput`s; call `write_hour_from_fields`. Print per-stage timings: fetch / extract / encode / total, plus file size and realized variable+level counts. Loop hours.

- [x] **Step 4:** Build + run live: `cargo run --release -p rusty-weather --bin rw_ingest -- --model hrrr --date 20260608 --cycle 0 --hours 6` (the GRIB may already be in the smoke cache — note cache state with the timing). **Gate: encode stage ≤ 5 s.** If encode exceeds the gate, profile the obvious knobs first (rayon chunk parallelism actually engaged? zstd level 1 confirmed? avoidable copies in the chunk re-layout?) and fix before proceeding. Record everything in the commit message. Commit `feat: rw_ingest — live GRIB to .rws for HRRR`.

---

### Task 10: Read-path live validation + rw_bench

**Files:**
- Create: `crates/rusty-weather/src/bin/rw_bench.rs`

- [x] **Step 1:** `rw_bench` CLI: `--store-root`, `--model`, `--run`, `--hour`. Measures and prints (median of 5 runs each, after 1 warmup):
  - open hour file (header+meta+index parse)
  - read_full_2d of each 2D var
  - read_window_2d of a ¼-domain window and a 64×64 window
  - GridLocator build (cold) and locate (warm)
  - full sounding: locate + read_profile_3d for ALL 3D vars (the user-facing "click → sounding data" cost)
  - whole-hour file size + per-var compressed sizes

- [x] **Step 2:** Run against the Task 9 output. **Gates: sounding ≤ 100 ms (expect ≤ 25 ms); full 2D read ≤ 150 ms; window read scales with area.** Investigate and fix if missed (likely suspects: index binary-search not engaged, zstd decode of more chunks than the window needs, locator rebuilt per call). Record all numbers in the commit message AND append a "Measured (Plan 2)" row-set to the README's baseline section. Commit `feat: rw_bench + measured rw-store baselines`.

---

### Task 11: Committed GRIB fixture + offline e2e test

**Files:**
- Create: `crates/rusty-weather/tests/fixtures/` (fixture GRIB), `crates/rusty-weather/tests/rw_store_e2e.rs`

- [x] **Step 1:** Build the fixture using idx byte-range subsetting (AWS supports it — `FetchRequest.variable_patterns`): fetch ONLY `TMP:500 mb`, `TMP:700 mb`, `TMP:850 mb`, `UGRD:500 mb`, `VGRD:500 mb`, `HGT:500 mb`, `TMP:2 m above ground` from one archived HRRR cycle (write a tiny throwaway script or temporarily extend rw_ingest with a `--fixture-out` flag — your choice; if a flag, keep it, it's useful). Target ≤ 8 MB. Commit the file with a README note naming its exact source URL + byte ranges date.

- [x] **Step 2:** `rw_store_e2e.rs` (offline, runs in default `cargo test`): bytes → `extract_fields_partial_from_model_bytes_at_forecast_hour` → `write_hour_from_fields` (3D = TMP at [850,700,500] + the 500mb wind/height as a second/third volume or 2D fields as extracted — match what the fixture provides) → `HourReader` reads back → assert: 2D bit-exact, 3D within quantization bounds, window==crop on real data, profile at a known lat/lon returns plausible values (e.g. 850→500 temperature decreases). This is the CI-smoke the spec promised.

- [x] **Step 3:** `cargo test --workspace` green; commit `test: committed HRRR fixture + offline rw-store e2e`.

---

### Task 12: Docs + finish

- [x] **Step 1:** README: add an `rw-store` section (format one-paragraph summary, the rw_ingest/rw_bench commands, measured numbers). Update the spec's open-questions section: chunk shapes are now decided (256×256 / 16×16×L) — note "settled in Plan 2 with benchmarks".
- [x] **Step 2:** Check all boxes in this plan; `cargo test --workspace` final green; commit; merge `plan-2-rw-store` → `main` (no-ff) after final review; tag `rw-store-v1`.

---

## Explicitly NOT in this plan

Rendering from the store (Plan 3 wires `HourReader`/`read_field_2d` into the render path — the old wxa render glue at `WT:wxstore_wxa.rs:809-1082` is the reference for that work). The daemon/scheduler. Multi-model ingest validation beyond HRRR (the API is model-generic via selectors; GFS/RRFS-A/REFS/NBM/RAP validation lands with the pipeline plan). Derived-product precomputation at ingest.

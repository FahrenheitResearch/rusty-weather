# rw-store on-disk format (v1)

This is the byte-level specification of the rw-store format. It is sufficient
to write a conforming reader or writer in any language without reading the Rust
source. Every offset, constant, and rule below was transcribed from and
re-verified against the crate source; each section cites its source of truth.

The format has three file kinds, all under one run directory:

```
<store_root>/<model>/<run>/
    f000.rws    forecast-hour file (one per hour)
    f001.rws
    ...
    grid.rwg    per-run lat/lon coordinate file (written once)
    run.json    run manifest
```

A reader that wants raw bytes only needs the `.rws` and `.rwg` formats; the
`run.json` manifest is a convenience index over the hour files.

---

## 1. Overview and guarantees

Source of truth: `crates/rw-store/src/header.rs`, `format.rs`, `writer.rs`,
`atomic.rs`, `ingest.rs`.

- **One self-contained file per forecast hour.** Each `.rws` file holds every
  variable for a single model run hour. There are no cross-file references
  inside a `.rws` file except `grid_hash` (which names the run's `.rwg`).
- **Little-endian throughout the binary files.** Every integer and IEEE-754
  float in `.rws` and `.rwg` headers, index records, and payloads is
  little-endian. (The NetCDF3 export in §8 is big-endian — that is a foreign
  format, not rw-store.)
- **mmap-friendly fixed layout.** Each `.rws` file is laid out as
  `fixed 64-byte header → meta JSON → sorted fixed-size index → payload`, so a
  reader memory-maps the file, parses the header, reads the index, binary
  -searches it, and decompresses only the chunks it needs.
- **Atomic visibility.** Every file (`.rws`, `.rwg`, `run.json`) is written to a
  hidden temp file in the same directory, flushed, `fsync`'d, then renamed into
  place (`atomic.rs::atomic_write_with`). A concurrent reader therefore never
  observes a partially written file: it sees either the previous content or the
  complete new file. The temp name is `.{file_name}.tmp-{pid}-{seq}` and is
  removed on any failure (`atomic.rs::temp_path_for`).

### Hour file naming

The hour stem is chosen by the caller of `HourWriter::finish(path)`; rw-store
itself does not impose a name. The two shipped writers use:

- **Model runs** (`ingest.rs::HourIngestWriter::finish`, line 426):
  `f{forecast_hour:03}.rws` — e.g. `f000.rws`, `f006.rws`.
- **Satellite frames** (`crates/rw-sat/src/store.rs::frame_file_name`, line 81):
  `t{HHMM:04}.rws`, where the `forecast_hour` u16 slot carries `HHMM` of the
  scan start in UTC — e.g. `t1851.rws` for 18:51Z.

Both stems are written by the identical `HourWriter` and produce identical
internal structure; only the filename differs. The `run.json` manifest records
the actual filename per hour (§5), so a reader does not need to know the stem
convention.

---

## 2. The `.rws` hour file

Source of truth: `crates/rw-store/src/header.rs`, `format.rs`, `index.rs`,
`writer.rs`.

File structure:

```
offset            size                contents
0                 64                  header table (§2.1)
64                meta_len            meta JSON (UTF-8) (§2.2)
64+meta_len       index_count * 64    index records, sorted (§2.3)
payload_offset    ...                 chunk payloads (§2.4)
```

where `payload_offset == 64 + meta_len + index_count*64`. The file ends exactly
at the end of the last payload chunk; there are no trailing bytes.

### 2.1 Header table (64 bytes)

Source of truth: `header.rs` (`RwsHeader::pack`/`parse`, lines 44–124).

| bytes   | field            | type | value / rule                                   |
|---------|------------------|------|------------------------------------------------|
| 0..8    | `magic`          | u8×8 | `b"RWSTORE1"` (0x52 0x57 0x53 0x54 0x4F 0x52 0x45 0x31) |
| 8..12   | `version`        | u32  | `1` (current); must be in `SUPPORTED_VERSIONS`  |
| 12..16  | `meta_len`       | u32  | byte length of the meta JSON                    |
| 16..24  | `index_count`    | u64  | number of 64-byte index records                 |
| 24..32  | `index_offset`   | u64  | **must equal** `64 + meta_len`                  |
| 32..40  | `payload_offset` | u64  | **must equal** `index_offset + index_count*64`  |
| 40..64  | `reserved`       | u8×24| zeros; writers write zero, readers ignore       |

A conforming reader MUST reject a file when:

- the buffer is shorter than 64 bytes;
- `magic` is not `b"RWSTORE1"`;
- `version` is not a supported version;
- `index_offset != 64 + meta_len`;
- `payload_offset != index_offset + index_count*64` — computed with checked
  arithmetic so a hostile `index_count` cannot wrap the multiply.

### 2.2 Meta JSON

Source of truth: `format.rs` (`RwsHourMeta` et al., lines 44–87); written at
`writer.rs::finish` (lines 491–516).

The meta region is `meta_len` bytes of UTF-8 JSON immediately after the header.
It deserializes to:

```jsonc
{
  "schema": "rw-store.hour.v1",   // exact string; readers reject mismatch
  "model": "hrrr",
  "run": "20260101_00z",          // opaque run identifier string
  "forecast_hour": 0,             // u16
  "nx": 20,                       // grid width  (columns), must be > 0
  "ny": 300,                      // grid height (rows),    must be > 0
  "grid_hash": "f3c7…760e",       // sha256 hex of the run's grid.rwg (§4)
  "variables": [ /* RwsVariableMeta, see below */ ],
  "chunking": { "tile_y": 256, "tile_x": 256, "col_y": 16, "col_x": 16 },
  "writer": { "name": "rw-store", "version": "0.1.0", "build": "<git sha>" }
}
```

Each entry of `variables` is an `RwsVariableMeta`:

```jsonc
{
  "id": 0,                        // u16; index of this variable; unique
  "name": "t2m",                  // unique within the file
  "units": "K",
  "kind": "surface2d",            // "surface2d" | "pressure3d"
  "codec": "zstd1_f32",           // "zstd1_f32" (2D) | "zstd1_affine_i16" (3D)
  "levels_hpa": [],               // empty for surface2d; descending for pressure3d
  "selector": { … }               // opaque, see below
}
```

Field rules (also enforced by `validate.rs`):

- `kind == "surface2d"` requires `codec == "zstd1_f32"` and `levels_hpa == []`.
- `kind == "pressure3d"` requires `codec == "zstd1_affine_i16"` and a
  non-empty `levels_hpa`. **`levels_hpa` is the authoritative stored level
  order**: strictly descending hPa (e.g. `[850, 700, 500]`), 1000-hPa-first.
  The writer sorts levels descending before storing (`ingest.rs::add_volume`
  sorts `b.0.cmp(&a.0)`, lines 387–388; `writer.rs::validate_pressure3d`
  enforces strict descent, lines 337–342). Within a 3D chunk the per-column
  values are stored in exactly this `levels_hpa` order (§2.4).
- `chunking` records the geometry the file was written with (always
  `256/256/16/16` for v1 writers). Readers should honor the meta values for
  geometry math; a v1 reader may warn if they differ from the format constants.

**Selector semantics.** `selector` is opaque JSON describing where the field
came from. For extracted GRIB fields it is the source GRIB selection object
(e.g. `{"var":"TMP","level":"2 m above ground"}`). The special form
`{"derived": "<slug>"}` marks a field computed at ingest time rather than
extracted from GRIB (`ingest.rs::derived_selector`, lines 50–60); readers that
need the GRIB selector must treat this marker specially. Readers MUST NOT fail
on selector contents they don't recognize.

**Unknown keys.** Readers MUST ignore JSON keys they do not recognize, at every
level. New keys may be added to meta in a future v1-compatible writer without a
version bump (§6).

### 2.3 Index record table (64 bytes each)

Source of truth: `index.rs` (`ChunkRecord::pack_into`/`unpack`, lines 44–98).

There are `index_count` records, each 64 bytes, starting at `index_offset`.

| bytes   | field         | type | meaning                                            |
|---------|---------------|------|----------------------------------------------------|
| 0..2    | `var_id`      | u16  | variable id (matches a `variables[].id`)           |
| 2       | `kind`        | u8   | `0` = TILE2D, `1` = COLUMN3D                        |
| 3       | `flags`       | u8   | bitfield: `1` EMPTY, `2` CONSTANT, `4` HAS_MISSING  |
| 4..8    | `tile_y`      | u32  | chunk row index (in chunk units, not grid points)  |
| 8..12   | `tile_x`      | u32  | chunk column index                                 |
| 12..20  | `offset`      | u64  | **absolute** file offset of this chunk's payload   |
| 20..24  | `len`         | u32  | compressed payload length in bytes (0 if absent)   |
| 24..28  | `raw_len`     | u32  | uncompressed payload length in bytes               |
| 28..32  | `center`      | f32  | codec center (see §3)                              |
| 32..36  | `scale`       | f32  | codec scale (see §3)                               |
| 36..40  | `min`         | f32  | min finite value; NaN when `valid_count == 0`      |
| 40..44  | `max`         | f32  | max finite value; NaN when `valid_count == 0`      |
| 44..48  | `valid_count` | u32  | count of finite (non-NaN) values in the chunk      |
| 48..64  | `reserved`    | u8×16| zeros; writers write zero, readers ignore          |

**Sort order and binary-search contract.** Records are sorted **strictly
ascending** by the tuple `(var_id, kind, tile_y, tile_x)`
(`index.rs::sort_key`, lines 100–103; emitted sorted at `writer.rs::finish`
line 489). A reader MUST verify strict ascent on open and reject a file that
violates it (`reader.rs::open`, lines 127–136). Because the order is total and
strict, each variable's records form one contiguous run, and a chunk is located
by binary search on the tuple key.

`flags` MUST be a subset of the three known bits; bits outside
`EMPTY|CONSTANT|HAS_MISSING` (`0x07`) are invalid.

### 2.4 Payload region and chunk geometry

Source of truth: `writer.rs` (`add_surface2d` lines 179–258,
`encode_pressure3d_chunks` lines 369–434, offset assignment lines 519–527),
`format.rs` (geometry constants lines 16–23).

Chunk payloads are concatenated in index (sorted) order starting at
`payload_offset`. The writer assigns each non-empty chunk the next free absolute
offset (cursor starting at `payload_offset`); chunks with `len == 0` carry no
payload bytes (their `offset` is set but unused). The file ends exactly at the
last chunk's end.

**2D surface fields (`kind = TILE2D`).** The `ny × nx` grid is tiled into
`TILE_Y × TILE_X = 256 × 256` tiles. `tiles_y = ceil(ny/256)`,
`tiles_x = ceil(nx/256)`. Edge tiles are clipped to the grid bounds, so a tile
at `(tile_y, tile_x)` covers grid rows `[tile_y*256, min((tile_y+1)*256, ny))`
and columns `[tile_x*256, min((tile_x+1)*256, nx))`. The decoded tile payload is
the tile window's f32 values in **row-major order within the tile** (row 0 of
the window first, left to right), then run through the §3 codec. For a tile at
`(tile_y, tile_x)` the clipped window dimensions are `th = min((tile_y+1)*256,
ny) - tile_y*256` rows and `tw = min((tile_x+1)*256, nx) - tile_x*256` columns;
the row stride inside the decoded f32 buffer is `tw` — not 256 and not `nx`.
The decoded value count for a dense 2D tile is `th * tw` (= `raw_len / 4`).

**3D pressure fields (`kind = COLUMN3D`).** The `ny × nx` footprint is chunked
into `COL_Y × COL_X = 16 × 16` column blocks. `chunks_y = ceil(ny/16)`,
`chunks_x = ceil(nx/16)`; edge chunks are clipped exactly as for tiles. The
decoded chunk payload is laid out `[y][x][level]`: iterating footprint rows
`gy` then footprint columns `gx`, the full pressure column (one value per level,
in `levels_hpa` order) is contiguous (`writer.rs` lines 396–403). So for a chunk
covering `rows × cols` grid points and `L` levels, the decoded value count is
`rows * cols * L` and the column for cell `(gy, gx)` within the chunk starts at
offset `((gy_local)*cols + gx_local) * L`. This layout makes a single-point
sounding decode exactly one chunk and slice one contiguous run of `L` values.

`raw_len` is the uncompressed byte count of the decoded payload:
`4 * rows * cols` for a TILE2D dense tile (f32), `2 * rows * cols * L` for a
COLUMN3D dense chunk (i16). EMPTY and CONSTANT-without-missing chunks have
`raw_len == 0` and no payload.

---

## 3. Codecs

Source of truth: `crates/rw-store/src/codec.rs`.

Two codecs, named in the variable meta. Both produce an uncompressed payload
that the writer then compresses with **zstd level 1**
(`writer.rs::ZSTD_LEVEL = 1`, line 42); `len` is the zstd-compressed size,
`raw_len` is the uncompressed size. Decompression caps output at `raw_len`
(`reader.rs` uses `zstd::bulk::decompress(.., raw_len)`).

### 3.1 `zstd1_f32` — 2D tiles (lossless)

Source: `codec.rs::encode_f32_tile`/`decode_f32_tile` (lines 192–258).

The uncompressed payload is the tile's values as **raw little-endian f32 bytes**
in row-major-within-tile order, then zstd-1. This lane is **exactly lossless**:
values round-trip bit-for-bit (including the exact bit pattern of NaNs). For
dense tiles, the index `center`/`scale` are not used to reconstruct values —
decoding reads the f32 bytes directly.

Index fields for a 2D tile:

- `min`/`max` = min/max finite value (NaN when none finite).
- `center` = `0.5 * (min + max)` for a dense tile; `= min` for a CONSTANT tile.
- `scale` = `0.0` (unused).
- `valid_count` = count of finite values.

### 3.2 `zstd1_affine_i16` — 3D column chunks (lossy)

Source: `codec.rs::encode_affine_i16`/`decode_affine_i16` (lines 67–180).

The whole chunk is quantized to i16 with a single `(center, scale)` pair, then
zstd-1. Quantization:

```
q = round((v - center) / scale)  clamped to [-32767, 32767]
```

where the finite-value range is computed in f64 to avoid f32 overflow:

```
range  = (max as f64) - (min as f64)
center = (min + 0.5 * range) as f32
scale  = (range / (2 * 32767)) as f32         // Q_MAX = i16::MAX = 32767
```

(`codec.rs` lines 107–115.) Encoding errors if `scale` is non-finite or `<= 0`.

Decoding:

```
v = center + (q as f32) * scale
```

The sentinel **`q == i16::MIN` (-32768) = MISSING** decodes to `NaN`
(`MISSING_Q`, line 7). Finite quants are restricted to `[-32767, 32767]`
(`Q_MIN = i16::MIN+1`, `Q_MAX = i16::MAX`, lines 9–11), leaving `-32768`
exclusively for the missing sentinel.

> **Scientific-honesty box.** 3D pressure values are **quantized at ingest** and
> are therefore lossy: the worst-case absolute error in one chunk is one
> quantization step, `≈ (chunk_max - chunk_min) / 65534`. The step is per-chunk,
> so a chunk spanning a small range is quantized finely and a chunk spanning a
> wide range coarsely. **2D surface values are exact** (the `zstd1_f32` lane is
> lossless). If you need bit-exact pressure-level data, do not use this store's
> 3D lane.

### 3.3 Flag semantics and payload presence

Source: `codec.rs` (encode functions) and `validate.rs` Check 7 (lines 633–683)
/ Check 14 (lines 985–1006).

A chunk's `flags` determine whether a payload is present and how it decodes.
**The flag rules differ between the two lanes** for the constant-with-missing
case, because only the 3D lane has a no-payload CONSTANT form that can still
carry NaNs:

| condition                                 | flags (2D `zstd1_f32`)       | flags (3D `zstd1_affine_i16`) | payload |
|-------------------------------------------|------------------------------|-------------------------------|---------|
| no finite values (all NaN)                | `EMPTY`                      | `EMPTY`                       | absent  |
| all finite values equal, no missing       | `CONSTANT`                   | `CONSTANT`                    | absent  |
| all finite values equal, some NaN         | `HAS_MISSING` (dense f32)    | `CONSTANT \| HAS_MISSING`     | present |
| values vary, no missing                   | `0`                          | `0`                           | present |
| values vary, some NaN                     | `HAS_MISSING`                | `HAS_MISSING`                 | present |

Details:

- **EMPTY** (`flags & 1`): the chunk has no finite values. `len == 0`,
  `raw_len == 0`, `valid_count == 0`, `min`/`max` are NaN, no payload bytes
  exist. A reader fills the chunk's footprint with NaN.
- **CONSTANT-without-missing** (`flags == CONSTANT`, no HAS_MISSING): every
  value is the single constant `center == min == max`. `len == 0`,
  `raw_len == 0`, no payload. A reader fills with `center`.
- **3D CONSTANT with missing** (`flags == CONSTANT | HAS_MISSING`, 3D only): the
  payload IS present and is i16 quants of `0` (finite cells) or `MISSING`
  (NaN cells); finite cells decode to `center`, missing to NaN
  (`codec.rs` lines 79–93). `scale == 0`.
- **2D constant with missing**: the 2D lane does **not** produce a
  `CONSTANT | HAS_MISSING` record. A constant 2D tile that contains any NaN
  falls through to the dense path and is flagged **`HAS_MISSING` only**, with a
  present raw-f32 payload (NaNs inline), `center = 0.5*(min+max)`
  (`codec.rs` lines 198–228). This is what the lossless lane requires.
- **HAS_MISSING** (`flags & 4`): the chunk contains at least one NaN cell; the
  payload is present (dense).

**`len == 0` iff the chunk is EMPTY or CONSTANT-without-missing.** This is the
exact structural invariant a validator checks (`validate.rs` lines 634–651);
all other records carry a payload (`len > 0`).

---

## 4. The `.rwg` grid file

Source of truth: `crates/rw-store/src/grid.rs` (lines 31–151, 195–293).

One `grid.rwg` per run directory holds the per-grid-point lat/lon coordinate
arrays (and an optional projection). It is written once and shared by every hour
of the run.

### 4.1 Header table (64 bytes)

| bytes  | field          | type | value / rule                                |
|--------|----------------|------|---------------------------------------------|
| 0..8   | `magic`        | u8×8 | `b"RWSGRID1"`                               |
| 8..12  | `version`      | u32  | `1`                                         |
| 12..16 | `meta_len`     | u32  | byte length of the meta JSON                |
| 16..24 | `lat_comp_len` | u64  | byte length of the zstd-1 lat block         |
| 24..32 | `lon_comp_len` | u64  | byte length of the zstd-1 lon block         |
| 32..64 | `reserved`     | u8×32| zeros; writers write zero, readers ignore   |

Body, immediately after the header:

```
64                       meta_len        meta JSON (UTF-8)
64+meta_len              lat_comp_len    zstd-1( lat as f32 LE bytes )
64+meta_len+lat_comp_len lon_comp_len    zstd-1( lon as f32 LE bytes )
```

The file length MUST equal `64 + meta_len + lat_comp_len + lon_comp_len`
exactly (`grid.rs` lines 220–237).

### 4.2 Meta JSON

```jsonc
{
  "schema": "rw-store.grid.v1",   // exact string
  "nx": 20,
  "ny": 300,
  "lat_raw_len": 24000,           // = nx*ny*4 (uncompressed lat byte count)
  "lon_raw_len": 24000,           // = nx*ny*4
  "projection": null              // optional GridProjection, or null/absent
}
```

Both coordinate arrays are row-major (`ny` rows of `nx`), decompressed from
their zstd blocks and read as little-endian f32. `lat_raw_len` and
`lon_raw_len` MUST both equal `nx*ny*4`.

### 4.3 grid_hash — the run identity

The **grid hash is the lowercase SHA-256 hex digest of the complete `.rwg` file
bytes** (`grid.rs::sha256_hex` over the full image, lines 149/283). This hash is
the run's grid identity: every hour file's meta `grid_hash` and the `run.json`
`grid_hash` reference it. A reader pairing a hour file with a grid file MUST
confirm the hour's `grid_hash` equals the grid file's hash (the ingest read
helpers enforce this, `ingest.rs::read_grid_2d` lines 507–513). Two
byte-identical grids hash identically, so a stable grid produces one shared
`.rwg` across the run.

---

## 5. `run.json` manifest

Source of truth: `crates/rw-store/src/run.rs` (lines 14–100).

A pretty-printed JSON object (trailing newline) listing the hours written so
far for the run:

```jsonc
{
  "schema": "rw-store.run.v1",     // exact string
  "model": "hrrr",
  "run": "20260101_00z",
  "grid_hash": "f3c7…760e",        // must match grid.rwg's hash and every hour
  "nx": 20,
  "ny": 300,
  "hours": {
    "0": {                         // key = forecast_hour as a string
      "file": "f000.rws",          // plain filename, no path components
      "written_unix": 1770000000,  // caller-supplied epoch seconds
      "encode_ms": 850,            // informational wall-clock; not reproducible
      "variables": ["t2m", "mask_demo", "const_demo", "temp_iso"]
    }
  },
  "writer": { "name": "rw-store", "version": "0.1.0", "build": "<git sha>" }
}
```

Rules:

- `hours` is keyed by the forecast hour (u16) rendered as a string; entries are
  emitted in ascending key order (the writer uses a `BTreeMap`). Re-registering
  an hour overwrites in place.
- `file` MUST be a **plain filename** — no `..`, no absolute path, no drive or
  root component. The validator rejects anything else as a path-traversal risk
  (`validate.rs::is_plain_filename`, lines 94–102, 165–171).
- `written_unix` is supplied by the caller; the library never reads the wall
  clock, so replays stay deterministic.
- `encode_ms` is informational encode wall-clock and is **not** reproducible
  run-to-run; conformance tooling excludes it from byte/value comparison
  (the golden test strips it, `golden.rs::strip_encode_ms`).
- An existing manifest must agree with the run's `model`, `run`, and
  `grid_hash`; a mismatch means the directory holds a different run's data
  (`run.rs::load_or_new`, lines 67–73).

---

## 6. Versioning policy

Source of truth: `format.rs` (`VERSION`, `SUPPORTED_VERSIONS`, lines 8–10),
`grid.rs` (`GRID_VERSION`, `GRID_SUPPORTED_VERSIONS`, lines 34–36),
`crates/rw-store/tests/golden/`.

- **v1 is frozen by golden fixtures.** The byte-exact fixtures in
  `crates/rw-store/tests/golden/v1/` (`f000.rws`, `grid.rwg`, `run.json`,
  `expected.json`) are the ground truth for v1. The golden tests fail on any
  byte change to the writer output; regenerating them is, by definition, a
  format change and requires a version bump discussion.
- **What forces a version bump.** Any change to the header layout, index record
  layout, payload layout, codec behavior, flag semantics, chunk kinds, or chunk
  geometry MUST increment `VERSION` by 1 and add the old version to
  `SUPPORTED_VERSIONS`. Readers keep reading every prior version
  (`SUPPORTED_VERSIONS` is the read whitelist; `header.rs::parse` rejects
  versions outside it). Writers always emit the newest version.
- **Reserved bytes**: writers write zeros; readers ignore them. Using reserved
  bytes for new fields is a format change (version bump).
- **Unknown meta JSON keys**: readers MUST ignore them. Additive metadata (new
  keys) does NOT require a version bump — it is forward-compatible by the
  ignore rule.
- **Schema strings version independently.** `rw-store.hour.v1`,
  `rw-store.grid.v1`, and `rw-store.run.v1` carry their own `.vN` suffix and
  evolve on their own schedule from the binary `VERSION`.
- **Fixtures are kept forever.** Golden fixtures for every released version stay
  in the tree and gate CI, so old-version reads never silently regress.

The `.rwg` grid file versions separately (`GRID_VERSION`,
`GRID_SUPPORTED_VERSIONS`), currently also `1`.

---

## 7. Concurrency contract

Source of truth: this section is the normative contract. The lock implementation
lands in Task 7 of `docs/superpowers/plans/2026-06-10-rw-store-hardening.md`;
the writer-side advisory lock described here is the spec that implementation
must conform to.

- **Single writer per run directory.** At most one process may write a given
  `<store_root>/<model>/<run>/` directory at a time. A writer MUST hold an
  exclusive **advisory** OS file lock on `<run-dir>/.rw-lock` for the duration
  of its write (acquire before touching `grid.rwg`/`f*.rws`/`run.json`, release
  after the manifest is committed).
- **The lock is an OS advisory lock**, not a pidfile: it is automatically
  released when the holding process exits (including a crash). The `.rw-lock`
  file is zero-length, created on demand, and **never deleted** — its presence
  is normal and is not itself the lock.
- **Readers are lock-free.** Because every file mutation is atomic
  temp+fsync+rename (§1), a reader never observes a partial file and so needs no
  lock. Readers MUST NOT create or take the lock.
- **Windows pruning caveat.** On Windows, deleting a file that a reader currently
  has memory-mapped fails. A pruning process (e.g. the satellite rolling-window
  trimmer) MUST tolerate a failed delete and retry on a later cycle rather than
  erroring out.
- **Tools must ignore lock and temp files.** `.rw-lock` and any file matching
  `.*.tmp-*` are bookkeeping, not store content; readers, validators, and
  listing tools MUST skip them (`validate.rs` already skips them, lines
  260–263).

---

## 8. Interop — NetCDF3 export

Source of truth: this is the export mapping contract. The exporter
(`rws export` → `export_hour_to_netcdf3`) and the NetCDF3 writer land in Tasks
5/6 of the hardening plan; this section is the spec they must conform to. Treat
it as a forward reference until those land.

`rws export` writes a self-contained **NetCDF classic 64-bit-offset (CDF-2)**
file — readable by xarray (scipy backend), MetPy, and Panoply with no extra
software. The mapping from one hour file + its grid:

- **Dimensions**: `y` (= `ny`), `x` (= `nx`), plus one `level`/`level2`/…
  dimension per distinct `levels_hpa` vector among exported 3D variables, each
  with a same-named f32 coordinate variable holding the hPa values
  (`units="hPa"`, `positive="down"`).
- **Coordinate variables**: 2-D `lat(y,x)` and `lon(y,x)` from the grid file
  (`units="degrees_north"`/`"degrees_east"`).
- **Data variables**: every exported variable as `NC_FLOAT`. 2-D vars on
  `(y,x)`, 3-D vars on `(levelN,y,x)` (level-major, matching the store's level
  order). Each carries `units` (from meta), `long_name`, `coordinates="lat lon"`,
  and `_FillValue` = f32 NaN.
- **Quantization disclosure**: every 3-D variable additionally carries an
  `rw_quantization` attribute documenting the per-chunk affine-i16 quantization
  (§3.2) — the scientific-honesty marker so a downstream user knows the 3-D
  data is lossy.
- **Global attributes**: `Conventions="CF-1.6"`, plus `model`, `run`,
  `forecast_hour`, `grid_hash`, and a `source` string identifying the rw-store
  schema and writer.

NetCDF3 is big-endian; that is a property of the foreign format, not of
rw-store (which is little-endian, §1).

---

## 9. Conformance

Source of truth: `crates/rw-store/src/validate.rs`,
`crates/rw-store/tests/golden.rs`,
`crates/rusty-weather/src/bin/rw_store_diff.rs`.

Three independent conformance checks back this spec:

- **Validation library** (`validate.rs`, surfaced as `rws validate [--deep]`).
  Two depths:
  - **Structural** — header parse, meta region bounds + schema, variable
    metadata consistency, index region bounds + per-record parse, strict sort
    order, tile coordinates in range, payload spans within
    `[payload_offset, file_len]` with no overlaps, the `len == 0` invariant
    (§3.3), expected `raw_len` per record, exact end-of-file, and per-variable
    chunk-set completeness. **No decompression.**
  - **Deep** — everything structural, plus decompress every non-empty chunk,
    confirm it inflates to exactly `raw_len` bytes, and cross-check the decoded
    content against the index statistics (finite count vs `valid_count`,
    min/max, `HAS_MISSING` ⇔ any NaN, CONSTANT 3-D ⇒ `scale == 0` and all
    non-missing quants `0`). Hostile inputs are bounded (a per-chunk
    decompression cap) so validation never panics or over-allocates.
  - `validate_run_dir` additionally checks `run.json` schema, that `grid.rwg`
    hashes to the manifest `grid_hash` with matching `nx`/`ny`, that each
    referenced hour file exists and validates and agrees with the manifest on
    model/run/grid_hash/nx/ny and variable set, and warns on stray `.rws` files.
- **Golden fixtures** (`tests/golden.rs`). `golden_v1_bytes_are_stable`
  rebuilds the fixture and byte-compares `f000.rws` and `grid.rwg`
  (`run.json` compared as parsed JSON minus `encode_ms`).
  `golden_v1_reader_values_match_expected` reads the committed fixtures and
  checks decoded values against `expected.json` (2-D bit-exact, 3-D within
  quantization tolerance) and that deep validation passes.
- **Equivalence diff** (`rw_store_diff`, to be surfaced as `rws diff`).
  Two hour files compare **equivalent** when their header `version`/`index_count`,
  meta JSON (with `writer.build` masked out), index records, and payload bytes
  match — with index `offset` compared **relative to each file's
  `payload_offset`** (payload-relative), so a different-length `writer.build`
  string (which shifts every absolute offset) does not register as a difference.
  This is the determinism gate: two independent builds ingesting identical
  inputs must produce equivalent hour files.

---

### Worked example: the golden v1 header

The committed fixture `crates/rw-store/tests/golden/v1/f000.rws` is a real v1
hour file (model `golden`, run `20260101_00z`, hour 0, grid `nx=20 ny=300`, four
variables: `t2m`, `mask_demo`, `const_demo` surface2d + `temp_iso` pressure3d on
levels `[850, 700, 500]`). Its first 64 bytes decode as:

```
raw hex (bytes 0..64):
52 57 53 54 4f 52 45 31  01 00 00 00  64 03 00 00
2c 00 00 00 00 00 00 00  a4 03 00 00 00 00 00 00
a4 0e 00 00 00 00 00 00  00 00 00 00 00 00 00 00
00 00 00 00 00 00 00 00  00 00 00 00 00 00 00 00

field            bytes    value
magic            0..8     "RWSTORE1"
version          8..12    1
meta_len         12..16   868
index_count      16..24   44
index_offset     24..32   932          (= 64 + 868)        ✓
payload_offset   32..40   3748         (= 932 + 44*64)     ✓
reserved         40..64   all zero
```

The file is 68052 bytes total, so the payload region is
`68052 - 3748 = 64304` bytes. The 44 index records are the chunk count: `t2m`,
`mask_demo`, `const_demo` each tile a `20×300` grid into `ceil(300/256)=2` ×
`ceil(20/256)=1 = 2` tiles (6 tiles), and `temp_iso` chunks the same footprint
into `ceil(300/16)=19` × `ceil(20/16)=2 = 38` column chunks — `6 + 38 = 44`.

The matching `grid.rwg` first decodes as `magic="RWSGRID1"`, `version=1`,
`meta_len=104`, `lat_comp_len=662`, `lon_comp_len=78`, total length
`64 + 104 + 662 + 78 = 908` bytes, with meta
`{"schema":"rw-store.grid.v1","nx":20,"ny":300,"lat_raw_len":24000,"lon_raw_len":24000,"projection":null}`
(`24000 = 20*300*4`). Its SHA-256 is
`f3c7edfa3b093ea606ac51f29afb67ebf8eb787c2ebc6fa283cde95001b8760e`, the
`grid_hash` carried by `f000.rws` and `run.json`.

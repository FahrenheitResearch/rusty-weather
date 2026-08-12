//! Hour-file reader: mmap-backed (RAM fallback) access to one rw-store hour
//! file with true windowed 2D reads — only the tiles intersecting a requested
//! window are ever decompressed. Repeated point/window reads share a bounded
//! decoded-tile cache; `read_full_2d` deliberately bypasses it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex};

use memmap2::Mmap;
use rayon::prelude::*;
use rustwx_core::{MAX_GRID_CELLS, MAX_VOLUME_ELEMENTS};
use same_file::Handle;

#[cfg(target_endian = "big")]
use crate::codec::decode_f32_tile;
use crate::codec::{MISSING_Q, decode_affine_i16};
use crate::error::{RwResult, RwStoreError};
use crate::format::{
    CODEC_2D, CODEC_3D, COL_X, COL_Y, FLAG_CONSTANT, FLAG_EMPTY, FLAG_HAS_MISSING, HEADER_LEN,
    INDEX_RECORD_LEN, KIND_COLUMN3D, KIND_TILE2D, RwsHourMeta, RwsVariableMeta, TILE_X, TILE_Y,
};
use crate::header::RwsHeader;
use crate::index::ChunkRecord;

/// Above this many tiles, `read_full_2d` decodes them in parallel.
const PARALLEL_TILE_THRESHOLD: usize = 8;
/// Default upper bound for dense 2D tiles retained by one open hour.
pub const DEFAULT_TILE_CACHE_BYTES: usize = 8 * 1024 * 1024;
/// Hour metadata is normally kilobytes. This matches the run-manifest ceiling
/// and prevents serde from scanning a hostile multi-gigabyte JSON section.
const MAX_HOUR_META_LEN: u64 = 16 * 1024 * 1024;
/// Large WRF stores can legitimately be multi-gigabyte. mmap keeps those
/// viable, while this generous absolute ceiling rejects absurd sparse files.
const MAX_HOUR_FILE_LEN: u64 = 64 * 1024 * 1024 * 1024;
/// If mapping is unavailable, never try to materialize an arbitrarily large
/// store in RAM. Allocation is still fallible below this limit.
const MAX_RAM_FALLBACK_FILE_LEN: u64 = 1024 * 1024 * 1024;
/// 512 MiB of packed index records. This covers over eight million chunks,
/// enough for dozens of pressure volumes on the largest supported grids.
const MAX_INDEX_RECORDS: u64 = (512 * 1024 * 1024) / INDEX_RECORD_LEN as u64;
/// Existing structural validation uses the same ceiling. Real WRF pressure
/// level products are typically O(10-100) levels.
const MAX_PRESSURE_LEVELS: usize = 4_096;
/// Incompressible zstd chunks add far less than this over their raw payload.
const MAX_CHUNK_COMP_OVERHEAD: u64 = 1024 * 1024;
/// A v1 chunk's raw body is at most two MiB with the level ceiling above.
/// Eight MiB leaves codec headroom while preventing a hostile frame header
/// from requesting an enormous zstd history window.
const MAX_CHUNK_WINDOW_LOG: u32 = 23;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TileCacheKey {
    kind: u8,
    var_id: u16,
    tile_y: u32,
    tile_x: u32,
}

#[derive(Debug)]
struct CachedTile {
    values: Arc<Vec<f32>>,
    bytes: usize,
    last_used: u64,
}

/// Counters and occupancy for one [`HourReader`]'s decoded 2D tile cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TileCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub insertions: u64,
    pub evictions: u64,
    pub entries: usize,
    pub bytes: usize,
    pub capacity_bytes: usize,
}

#[derive(Debug)]
struct DecodedTileCache {
    capacity_bytes: usize,
    bytes: usize,
    clock: u64,
    hits: u64,
    misses: u64,
    insertions: u64,
    evictions: u64,
    entries: HashMap<TileCacheKey, CachedTile>,
}

impl DecodedTileCache {
    fn new(capacity_bytes: usize) -> Self {
        Self {
            capacity_bytes,
            bytes: 0,
            clock: 0,
            hits: 0,
            misses: 0,
            insertions: 0,
            evictions: 0,
            entries: HashMap::new(),
        }
    }

    fn next_stamp(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn get(&mut self, key: TileCacheKey) -> Option<Arc<Vec<f32>>> {
        let stamp = self.next_stamp();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.last_used = stamp;
            self.hits = self.hits.saturating_add(1);
            return Some(Arc::clone(&entry.values));
        }
        self.misses = self.misses.saturating_add(1);
        None
    }

    fn insert(&mut self, key: TileCacheKey, values: Arc<Vec<f32>>) -> Arc<Vec<f32>> {
        let last_used = self.next_stamp();
        if let Some(existing) = self.entries.get_mut(&key) {
            existing.last_used = last_used;
            return Arc::clone(&existing.values);
        }

        let bytes = values.len().saturating_mul(size_of::<f32>());
        if self.capacity_bytes == 0 || bytes > self.capacity_bytes {
            return values;
        }
        // Caching is optional. If its bookkeeping allocation cannot be
        // reserved, return the decoded tile without turning a successful
        // data read into an allocation error.
        if self.entries.try_reserve(1).is_err() {
            return values;
        }
        while self.bytes.saturating_add(bytes) > self.capacity_bytes {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.bytes = self.bytes.saturating_sub(removed.bytes);
                self.evictions = self.evictions.saturating_add(1);
            }
        }

        self.bytes = self.bytes.saturating_add(bytes);
        self.insertions = self.insertions.saturating_add(1);
        self.entries.insert(
            key,
            CachedTile {
                values: Arc::clone(&values),
                bytes,
                last_used,
            },
        );
        values
    }

    fn clear(&mut self) {
        *self = Self::new(self.capacity_bytes);
    }

    fn stats(&self) -> TileCacheStats {
        TileCacheStats {
            hits: self.hits,
            misses: self.misses,
            insertions: self.insertions,
            evictions: self.evictions,
            entries: self.entries.len(),
            bytes: self.bytes,
            capacity_bytes: self.capacity_bytes,
        }
    }
}

fn try_zeroed_bytes(len: usize, what: &str) -> RwResult<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|err| {
        RwStoreError::Format(format!("cannot allocate {len} bytes for {what}: {err}"))
    })?;
    bytes.resize(len, 0);
    Ok(bytes)
}

fn try_filled_f32(len: usize, value: f32, what: &str) -> RwResult<Vec<f32>> {
    let mut values = Vec::new();
    values.try_reserve_exact(len).map_err(|err| {
        RwStoreError::Format(format!(
            "cannot allocate {len} f32 values for {what}: {err}"
        ))
    })?;
    values.resize(len, value);
    Ok(values)
}

fn decompress_chunk_into(comp: &[u8], raw: &mut [u8], what: &str) -> RwResult<()> {
    let raw_len = raw.len();
    let mut decoder = zstd::stream::read::Decoder::with_buffer(comp)
        .map_err(|err| RwStoreError::Chunk(format!("{what}: zstd decode failed: {err}")))?;
    decoder
        .window_log_max(MAX_CHUNK_WINDOW_LOG)
        .map_err(|err| RwStoreError::Chunk(format!("{what}: zstd window limit: {err}")))?;
    decoder
        .read_exact(raw)
        .map_err(|err| RwStoreError::Chunk(format!("{what}: zstd decode failed: {err}")))?;
    let mut extra = [0u8; 1];
    if decoder
        .read(&mut extra)
        .map_err(|err| RwStoreError::Chunk(format!("{what}: zstd decode failed: {err}")))?
        != 0
    {
        return Err(RwStoreError::Chunk(format!(
            "{what}: decompressed beyond the expected {raw_len} bytes"
        )));
    }
    Ok(())
}

fn decompress_chunk(comp: &[u8], raw_len: usize, what: &str) -> RwResult<Vec<u8>> {
    let mut raw = try_zeroed_bytes(raw_len, what)?;
    decompress_chunk_into(comp, &mut raw, what)?;
    Ok(raw)
}

/// File bytes, mmap-first with a read-to-RAM fallback (same strategy as the
/// rustwx volume_store payload reader: if the OS refuses the map, fall back
/// to loading the file instead of failing the open).
enum FileBytes {
    Mmap(Mmap),
    Ram(Vec<u8>),
}

impl FileBytes {
    fn as_slice(&self) -> &[u8] {
        match self {
            FileBytes::Mmap(mmap) => &mmap[..],
            FileBytes::Ram(bytes) => bytes.as_slice(),
        }
    }
}

fn validate_header_bounds(header: &RwsHeader, file_len: u64) -> RwResult<()> {
    if file_len < HEADER_LEN as u64 {
        return Err(RwStoreError::Format(format!(
            "header requires {HEADER_LEN} bytes, got {file_len}"
        )));
    }
    if file_len > MAX_HOUR_FILE_LEN {
        return Err(RwStoreError::Format(format!(
            "hour file is {file_len} bytes; limit is {MAX_HOUR_FILE_LEN} bytes"
        )));
    }
    if u64::from(header.meta_len) > MAX_HOUR_META_LEN {
        return Err(RwStoreError::Format(format!(
            "hour meta JSON is {} bytes; limit is {MAX_HOUR_META_LEN} bytes",
            header.meta_len
        )));
    }
    if header.index_count > MAX_INDEX_RECORDS {
        return Err(RwStoreError::Format(format!(
            "hour chunk index has {} records; limit is {MAX_INDEX_RECORDS}",
            header.index_count
        )));
    }
    if header.payload_offset > file_len {
        return Err(RwStoreError::Format(format!(
            "file truncated inside chunk index: need {} bytes, have {file_len}",
            header.payload_offset
        )));
    }
    usize::try_from(file_len).map_err(|_| {
        RwStoreError::Format(format!("hour file length {file_len} does not fit usize"))
    })?;
    usize::try_from(header.index_offset).map_err(|_| {
        RwStoreError::Format(format!(
            "hour index offset {} does not fit usize",
            header.index_offset
        ))
    })?;
    usize::try_from(header.payload_offset).map_err(|_| {
        RwStoreError::Format(format!(
            "hour payload offset {} does not fit usize",
            header.payload_offset
        ))
    })?;
    Ok(())
}

fn parse_hour_meta(bytes: &[u8]) -> RwResult<RwsHourMeta> {
    serde_json::from_slice(bytes)
        .map_err(|err| RwStoreError::Meta(format!("hour meta JSON: {err}")))
}

/// Enforce every shape-derived ceiling before index or field allocations.
/// The expected index cardinality is exact for format v1: one record for each
/// variable/chunk coordinate, including EMPTY and CONSTANT chunks.
fn validate_hour_meta(meta: &RwsHourMeta, header: &RwsHeader) -> RwResult<()> {
    meta.validate_time_schema().map_err(RwStoreError::Meta)?;
    if meta.nx == 0 || meta.ny == 0 {
        return Err(RwStoreError::Meta(format!(
            "degenerate grid {}x{} (nx and ny must be nonzero)",
            meta.nx, meta.ny
        )));
    }
    let cells = meta
        .nx
        .checked_mul(meta.ny)
        .filter(|&cells| cells <= MAX_GRID_CELLS)
        .ok_or_else(|| {
            RwStoreError::Meta(format!(
                "grid {}x{} is invalid or exceeds the supported ceiling of {MAX_GRID_CELLS} cells",
                meta.nx, meta.ny
            ))
        })?;
    if meta.chunking.tile_y != TILE_Y
        || meta.chunking.tile_x != TILE_X
        || meta.chunking.col_y != COL_Y
        || meta.chunking.col_x != COL_X
    {
        return Err(RwStoreError::Meta(format!(
            "unsupported chunk geometry tile={}x{}, column={}x{}; format v1 requires tile={TILE_Y}x{TILE_X}, column={COL_Y}x{COL_X}",
            meta.chunking.tile_y, meta.chunking.tile_x, meta.chunking.col_y, meta.chunking.col_x
        )));
    }
    if meta.variables.len() > usize::from(u16::MAX) + 1 {
        return Err(RwStoreError::Meta(format!(
            "{} variables exceed the u16 id space",
            meta.variables.len()
        )));
    }

    let surface_chunks = meta
        .ny
        .div_ceil(TILE_Y)
        .checked_mul(meta.nx.div_ceil(TILE_X))
        .ok_or_else(|| RwStoreError::Meta("surface chunk count overflows usize".to_string()))?;
    let column_chunks = meta
        .ny
        .div_ceil(COL_Y)
        .checked_mul(meta.nx.div_ceil(COL_X))
        .ok_or_else(|| RwStoreError::Meta("column chunk count overflows usize".to_string()))?;
    let mut expected_records = 0u64;
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    for var in &meta.variables {
        if !ids.insert(var.id) {
            return Err(RwStoreError::Meta(format!(
                "duplicate variable id {}",
                var.id
            )));
        }
        if !names.insert(var.name.as_str()) {
            return Err(RwStoreError::Meta(format!(
                "duplicate variable name '{}'",
                var.name
            )));
        }
        let records = match var.kind.as_str() {
            "surface2d" => {
                if var.codec != CODEC_2D || !var.levels_hpa.is_empty() {
                    return Err(RwStoreError::Meta(format!(
                        "surface variable '{}' must use codec '{CODEC_2D}' and have no pressure levels",
                        var.name
                    )));
                }
                surface_chunks
            }
            "pressure3d" => {
                let levels = var.levels_hpa.len();
                if var.codec != CODEC_3D || levels == 0 || levels > MAX_PRESSURE_LEVELS {
                    return Err(RwStoreError::Meta(format!(
                        "pressure variable '{}' must use codec '{CODEC_3D}' and contain 1..={MAX_PRESSURE_LEVELS} levels",
                        var.name
                    )));
                }
                if let Some(pair) = var.levels_hpa.windows(2).find(|pair| pair[0] <= pair[1]) {
                    return Err(RwStoreError::Meta(format!(
                        "pressure variable '{}' levels must be strictly descending, found {} then {}",
                        var.name, pair[0], pair[1]
                    )));
                }
                let elements = levels.checked_mul(cells).ok_or_else(|| {
                    RwStoreError::Meta(format!(
                        "pressure variable '{}' element count overflows usize",
                        var.name
                    ))
                })?;
                if elements > MAX_VOLUME_ELEMENTS {
                    return Err(RwStoreError::Meta(format!(
                        "pressure variable '{}' has {elements} values; limit is {MAX_VOLUME_ELEMENTS}",
                        var.name
                    )));
                }
                column_chunks
            }
            other => {
                return Err(RwStoreError::Meta(format!(
                    "variable '{}' has unknown kind '{other}'",
                    var.name
                )));
            }
        };
        expected_records = expected_records
            .checked_add(records as u64)
            .ok_or_else(|| RwStoreError::Meta("expected index count overflows u64".to_string()))?;
    }
    if expected_records > MAX_INDEX_RECORDS {
        return Err(RwStoreError::Meta(format!(
            "metadata requires {expected_records} chunk records; limit is {MAX_INDEX_RECORDS}"
        )));
    }
    if header.index_count != expected_records {
        return Err(RwStoreError::Format(format!(
            "chunk index count {} does not match metadata geometry ({expected_records})",
            header.index_count
        )));
    }
    Ok(())
}

fn parse_and_validate_hour(data: &[u8]) -> RwResult<(RwsHeader, RwsHourMeta)> {
    let file_len = data.len() as u64;
    let header = RwsHeader::parse(data)?;
    validate_header_bounds(&header, file_len)?;
    let meta_end = usize::try_from(header.index_offset).map_err(|_| {
        RwStoreError::Format(format!(
            "hour meta end {} does not fit usize",
            header.index_offset
        ))
    })?;
    let meta = parse_hour_meta(&data[HEADER_LEN..meta_end])?;
    validate_hour_meta(&meta, &header)?;
    Ok((header, meta))
}

fn validate_chunk_record(
    index: usize,
    record: &ChunkRecord,
    meta: &RwsHourMeta,
    header: &RwsHeader,
    file_len: u64,
    variables: &BTreeMap<u16, &RwsVariableMeta>,
) -> RwResult<()> {
    let var = variables.get(&record.var_id).ok_or_else(|| {
        RwStoreError::Format(format!(
            "index record {index}: variable id {} is absent from metadata",
            record.var_id
        ))
    })?;
    let (kind, chunk_y, chunk_x, levels, bytes_per_value) = match var.kind.as_str() {
        "surface2d" => (KIND_TILE2D, TILE_Y, TILE_X, 1usize, size_of::<f32>()),
        "pressure3d" => (KIND_COLUMN3D, COL_Y, COL_X, var.levels_hpa.len(), 2usize),
        other => {
            return Err(RwStoreError::Format(format!(
                "index record {index} refers to variable '{}' with unsupported kind '{other}'",
                var.name
            )));
        }
    };
    if record.kind != kind {
        return Err(RwStoreError::Format(format!(
            "index record {index} for '{}': kind {} does not match expected {kind}",
            var.name, record.kind
        )));
    }
    let valid_flags = FLAG_EMPTY | FLAG_CONSTANT | FLAG_HAS_MISSING;
    if record.flags & !valid_flags != 0 {
        return Err(RwStoreError::Format(format!(
            "index record {index} for '{}': unknown flags 0x{:02x}",
            var.name, record.flags
        )));
    }

    let tile_y = record.tile_y as usize;
    let tile_x = record.tile_x as usize;
    let max_y = meta.ny.div_ceil(chunk_y);
    let max_x = meta.nx.div_ceil(chunk_x);
    if tile_y >= max_y || tile_x >= max_x {
        return Err(RwStoreError::Format(format!(
            "index record {index} for '{}': chunk ({tile_y},{tile_x}) is outside {max_y}x{max_x}",
            var.name
        )));
    }
    let y0 = tile_y.checked_mul(chunk_y).ok_or_else(|| {
        RwStoreError::Format(format!("index record {index}: chunk y offset overflows"))
    })?;
    let x0 = tile_x.checked_mul(chunk_x).ok_or_else(|| {
        RwStoreError::Format(format!("index record {index}: chunk x offset overflows"))
    })?;
    let rows = (meta.ny - y0).min(chunk_y);
    let cols = (meta.nx - x0).min(chunk_x);
    let values = rows
        .checked_mul(cols)
        .and_then(|count| count.checked_mul(levels))
        .ok_or_else(|| {
            RwStoreError::Format(format!("index record {index}: value count overflows usize"))
        })?;
    let expected_raw_len = values.checked_mul(bytes_per_value).ok_or_else(|| {
        RwStoreError::Format(format!(
            "index record {index}: raw byte count overflows usize"
        ))
    })?;

    let is_empty = record.flags & FLAG_EMPTY != 0;
    let is_constant_no_missing =
        record.flags & FLAG_CONSTANT != 0 && record.flags & FLAG_HAS_MISSING == 0;
    let payload_free = is_empty || is_constant_no_missing;
    if payload_free {
        if record.len != 0 || record.raw_len != 0 {
            return Err(RwStoreError::Format(format!(
                "index record {index} for '{}': empty/constant chunk must have len=raw_len=0",
                var.name
            )));
        }
    } else {
        if record.len == 0 || record.raw_len as usize != expected_raw_len {
            return Err(RwStoreError::Format(format!(
                "index record {index} for '{}': len={} raw_len={} (expected nonzero len and raw_len {expected_raw_len})",
                var.name, record.len, record.raw_len
            )));
        }
        let max_comp_len = (expected_raw_len as u64)
            .checked_add(MAX_CHUNK_COMP_OVERHEAD)
            .unwrap_or(u64::MAX);
        if u64::from(record.len) > max_comp_len {
            return Err(RwStoreError::Format(format!(
                "index record {index} for '{}': compressed length {} exceeds limit {max_comp_len}",
                var.name, record.len
            )));
        }
        if record.offset < header.payload_offset {
            return Err(RwStoreError::Format(format!(
                "index record {index} for '{}': payload offset {} precedes payload section {}",
                var.name, record.offset, header.payload_offset
            )));
        }
        let end = record
            .offset
            .checked_add(u64::from(record.len))
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "index record {index} for '{}': payload range overflows u64",
                    var.name
                ))
            })?;
        if end > file_len {
            return Err(RwStoreError::Format(format!(
                "index record {index} for '{}': payload ends at {end}, beyond file length {file_len}",
                var.name
            )));
        }
    }
    if record.valid_count as usize > values {
        return Err(RwStoreError::Format(format!(
            "index record {index} for '{}': valid_count {} exceeds chunk value count {values}",
            var.name, record.valid_count
        )));
    }
    Ok(())
}

/// A rectangular sub-region of a 2D field returned by
/// [`HourReader::read_window_2d`]. `values` is row-major, `ny` rows of `nx`.
#[derive(Debug, Clone, PartialEq)]
pub struct Window2D {
    pub x0: usize,
    pub y0: usize,
    pub nx: usize,
    pub ny: usize,
    pub values: Vec<f32>,
}

/// Grid placement and shape of one 2D surface tile.
///
/// Tile indices are zero-based. `x0`/`y0` are the tile's origin in the full
/// grid, while `nx`/`ny` are clipped at the right and bottom grid edges.
/// Values inside a tile are row-major, `ny` rows of `nx` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TileGeometry2D {
    tile_y: usize,
    tile_x: usize,
    x0: usize,
    y0: usize,
    nx: usize,
    ny: usize,
}

impl TileGeometry2D {
    pub const fn tile_y(self) -> usize {
        self.tile_y
    }

    pub const fn tile_x(self) -> usize {
        self.tile_x
    }

    pub const fn x0(self) -> usize {
        self.x0
    }

    pub const fn y0(self) -> usize {
        self.y0
    }

    pub const fn nx(self) -> usize {
        self.nx
    }

    pub const fn ny(self) -> usize {
        self.ny
    }

    /// Number of row-major values represented by this tile.
    pub const fn cell_count(self) -> usize {
        // Instances are only constructed after HourReader's checked grid
        // validation, where the complete grid is bounded by MAX_GRID_CELLS.
        self.nx * self.ny
    }
}

/// Storage-aware values for one 2D surface tile.
///
/// Keeping payload-free tiles typed avoids allocating 256x256 temporary
/// planes for all-missing or constant regions. `Dense` values are bit-exact
/// decoded f32s shared with the reader's bounded tile cache.
#[derive(Debug, Clone)]
pub enum TileData2D {
    /// Every cell is represented by `f32::NAN`.
    Empty,
    /// Every cell has the same finite value.
    Constant(f32),
    /// Row-major decoded values, including their original f32/NaN bits.
    Dense(Arc<Vec<f32>>),
}

/// One decoded or storage-specialized 2D surface tile.
#[derive(Debug, Clone)]
pub struct Tile2D {
    geometry: TileGeometry2D,
    data: TileData2D,
}

impl Tile2D {
    pub const fn geometry(&self) -> TileGeometry2D {
        self.geometry
    }

    pub const fn data(&self) -> &TileData2D {
        &self.data
    }

    pub const fn cell_count(&self) -> usize {
        self.geometry.cell_count()
    }

    /// Read one tile-local `(row, column)` value without materializing sparse
    /// tile encodings. Returns `None` when either coordinate is out of bounds.
    pub fn get(&self, row: usize, column: usize) -> Option<f32> {
        if row >= self.geometry.ny || column >= self.geometry.nx {
            return None;
        }
        let index = row * self.geometry.nx + column;
        match &self.data {
            TileData2D::Empty => Some(f32::NAN),
            TileData2D::Constant(value) => Some(*value),
            TileData2D::Dense(values) => values.get(index).copied(),
        }
    }
}

/// Allocation-free, row-major enumeration of a surface variable's tiles.
#[derive(Debug, Clone)]
pub struct Tiles2D {
    grid_nx: usize,
    grid_ny: usize,
    tiles_x: usize,
    tiles_y: usize,
    next: usize,
    tile_count: usize,
}

impl Tiles2D {
    pub const fn tiles_x(&self) -> usize {
        self.tiles_x
    }

    pub const fn tiles_y(&self) -> usize {
        self.tiles_y
    }
}

impl Iterator for Tiles2D {
    type Item = TileGeometry2D;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.tile_count {
            return None;
        }
        let ordinal = self.next;
        self.next += 1;
        let tile_y = ordinal / self.tiles_x;
        let tile_x = ordinal % self.tiles_x;
        let y0 = tile_y * TILE_Y;
        let x0 = tile_x * TILE_X;
        Some(TileGeometry2D {
            tile_y,
            tile_x,
            x0,
            y0,
            nx: (self.grid_nx - x0).min(TILE_X),
            ny: (self.grid_ny - y0).min(TILE_Y),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.tile_count - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for Tiles2D {}
impl std::iter::FusedIterator for Tiles2D {}

/// Grid placement and shape of one pressure-level column chunk.
///
/// Chunk indices are zero-based. `x0`/`y0` are the chunk's origin in the
/// full grid, while `width`/`height` are clipped at the right and bottom
/// grid edges. Values inside a chunk plane are row-major.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PressureLevelChunkGeometry3D {
    chunk_y: usize,
    chunk_x: usize,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
    level_hpa: u16,
}

impl PressureLevelChunkGeometry3D {
    pub const fn chunk_y(self) -> usize {
        self.chunk_y
    }

    pub const fn chunk_x(self) -> usize {
        self.chunk_x
    }

    pub const fn x0(self) -> usize {
        self.x0
    }

    pub const fn y0(self) -> usize {
        self.y0
    }

    pub const fn width(self) -> usize {
        self.width
    }

    pub const fn height(self) -> usize {
        self.height
    }

    pub const fn level_hpa(self) -> u16 {
        self.level_hpa
    }

    /// Number of row-major values represented by this chunk plane.
    pub const fn cell_count(self) -> usize {
        // Instances are only constructed after HourReader's checked grid
        // validation, where the complete grid is bounded by MAX_GRID_CELLS.
        self.width * self.height
    }
}

/// Storage-aware values for one pressure-level column-chunk plane.
///
/// EMPTY and payload-free CONSTANT chunks remain allocation-free. Dense
/// planes contain at most `COL_Y * COL_X` row-major values and retain the
/// exact decoded f32/NaN bits produced by the pressure-volume codec.
#[derive(Debug, Clone)]
pub enum PressureLevelChunkData3D {
    /// Every cell is represented by `f32::NAN`.
    Empty,
    /// Every cell has the same finite value.
    Constant(f32),
    /// Row-major decoded values for only the requested pressure level.
    Dense(Arc<Vec<f32>>),
}

/// One decoded or storage-specialized pressure-level column-chunk plane.
#[derive(Debug, Clone)]
pub struct PressureLevelChunk3D {
    geometry: PressureLevelChunkGeometry3D,
    data: PressureLevelChunkData3D,
}

impl PressureLevelChunk3D {
    pub const fn geometry(&self) -> PressureLevelChunkGeometry3D {
        self.geometry
    }

    pub const fn data(&self) -> &PressureLevelChunkData3D {
        &self.data
    }

    pub const fn cell_count(&self) -> usize {
        self.geometry.cell_count()
    }

    /// Read one chunk-local `(row, column)` value without materializing
    /// sparse chunk encodings. Returns `None` when either coordinate is out
    /// of bounds.
    pub fn get(&self, row: usize, column: usize) -> Option<f32> {
        if row >= self.geometry.height || column >= self.geometry.width {
            return None;
        }
        let index = row * self.geometry.width + column;
        match &self.data {
            PressureLevelChunkData3D::Empty => Some(f32::NAN),
            PressureLevelChunkData3D::Constant(value) => Some(*value),
            PressureLevelChunkData3D::Dense(values) => values.get(index).copied(),
        }
    }
}

/// Grid placement and shape of one column chunk for an explicit pressure
/// level selection.
///
/// Unlike [`PressureLevelChunkGeometry3D`], this geometry is independent of
/// any one level. The selected levels and their caller-defined order live on
/// [`SelectedPressureLevelChunk3D`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SelectedPressureLevelChunkGeometry3D {
    chunk_y: usize,
    chunk_x: usize,
    x0: usize,
    y0: usize,
    width: usize,
    height: usize,
}

impl SelectedPressureLevelChunkGeometry3D {
    pub const fn chunk_y(self) -> usize {
        self.chunk_y
    }

    pub const fn chunk_x(self) -> usize {
        self.chunk_x
    }

    pub const fn x0(self) -> usize {
        self.x0
    }

    pub const fn y0(self) -> usize {
        self.y0
    }

    pub const fn width(self) -> usize {
        self.width
    }

    pub const fn height(self) -> usize {
        self.height
    }

    /// Number of cells in each selected pressure-level plane.
    pub const fn cell_count(self) -> usize {
        // Instances are only constructed after HourReader's checked grid
        // validation, where the complete grid is bounded by MAX_GRID_CELLS.
        self.width * self.height
    }
}

/// Storage-aware values for all explicitly selected levels in one column
/// chunk.
///
/// Dense values are laid out `[selected_level][row][column]`; the outer level
/// order is exactly the caller's requested order. A dense allocation is
/// bounded by `selected_level_count * COL_Y * COL_X`. Payload-free EMPTY and
/// CONSTANT source chunks retain their allocation-free representation.
#[derive(Debug, Clone)]
pub enum SelectedPressureLevelChunkData3D {
    /// Every cell at every selected level is represented by `f32::NAN`.
    Empty,
    /// Every cell at every selected level has the same finite value.
    Constant(f32),
    /// Selected planes only, in `[selected_level][row][column]` order.
    Dense(Arc<Vec<f32>>),
}

/// Borrowed view of one selected pressure-level plane.
#[derive(Debug, Clone, Copy)]
pub enum SelectedPressureLevelPlane3D<'a> {
    Empty,
    Constant(f32),
    Dense(&'a [f32]),
}

/// One decoded or storage-specialized column chunk for an explicit, ordered
/// pressure-level selection.
#[derive(Debug, Clone)]
pub struct SelectedPressureLevelChunk3D {
    geometry: SelectedPressureLevelChunkGeometry3D,
    levels_hpa: Arc<[u16]>,
    data: SelectedPressureLevelChunkData3D,
}

impl SelectedPressureLevelChunk3D {
    pub const fn geometry(&self) -> SelectedPressureLevelChunkGeometry3D {
        self.geometry
    }

    /// Explicit pressure levels in the exact order requested by the caller.
    pub fn levels_hpa(&self) -> &[u16] {
        &self.levels_hpa
    }

    pub const fn data(&self) -> &SelectedPressureLevelChunkData3D {
        &self.data
    }

    pub const fn cell_count(&self) -> usize {
        self.geometry.cell_count()
    }

    pub fn value_count(&self) -> usize {
        // Construction checks this multiplication before allocating dense
        // values; metadata validation also caps the level count.
        self.cell_count() * self.levels_hpa.len()
    }

    /// Borrow one selected plane by its index in [`Self::levels_hpa`].
    pub fn plane(&self, selected_level_index: usize) -> Option<SelectedPressureLevelPlane3D<'_>> {
        if selected_level_index >= self.levels_hpa.len() {
            return None;
        }
        Some(match &self.data {
            SelectedPressureLevelChunkData3D::Empty => SelectedPressureLevelPlane3D::Empty,
            SelectedPressureLevelChunkData3D::Constant(value) => {
                SelectedPressureLevelPlane3D::Constant(*value)
            }
            SelectedPressureLevelChunkData3D::Dense(values) => {
                let start = selected_level_index * self.cell_count();
                SelectedPressureLevelPlane3D::Dense(&values[start..start + self.cell_count()])
            }
        })
    }

    /// Read one selected-level, chunk-local `(row, column)` value without
    /// materializing sparse chunk encodings. Returns `None` when the level or
    /// either coordinate is out of bounds.
    pub fn get(&self, selected_level_index: usize, row: usize, column: usize) -> Option<f32> {
        if selected_level_index >= self.levels_hpa.len()
            || row >= self.geometry.height
            || column >= self.geometry.width
        {
            return None;
        }
        let plane_index = row * self.geometry.width + column;
        match &self.data {
            SelectedPressureLevelChunkData3D::Empty => Some(f32::NAN),
            SelectedPressureLevelChunkData3D::Constant(value) => Some(*value),
            SelectedPressureLevelChunkData3D::Dense(values) => values
                .get(selected_level_index * self.cell_count() + plane_index)
                .copied(),
        }
    }
}

/// Allocation-bounded, row-major enumeration of a pressure variable's
/// column chunks for an explicit ordered level selection.
#[derive(Debug, Clone)]
pub struct SelectedPressureLevelChunks3D {
    grid_nx: usize,
    grid_ny: usize,
    chunks_x: usize,
    chunks_y: usize,
    levels_hpa: Arc<[u16]>,
    next: usize,
    chunk_count: usize,
}

impl SelectedPressureLevelChunks3D {
    pub const fn chunks_x(&self) -> usize {
        self.chunks_x
    }

    pub const fn chunks_y(&self) -> usize {
        self.chunks_y
    }

    /// Explicit pressure levels in the exact order requested by the caller.
    pub fn levels_hpa(&self) -> &[u16] {
        &self.levels_hpa
    }
}

impl Iterator for SelectedPressureLevelChunks3D {
    type Item = SelectedPressureLevelChunkGeometry3D;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.chunk_count {
            return None;
        }
        let ordinal = self.next;
        self.next += 1;
        let chunk_y = ordinal / self.chunks_x;
        let chunk_x = ordinal % self.chunks_x;
        let y0 = chunk_y * COL_Y;
        let x0 = chunk_x * COL_X;
        Some(SelectedPressureLevelChunkGeometry3D {
            chunk_y,
            chunk_x,
            x0,
            y0,
            width: (self.grid_nx - x0).min(COL_X),
            height: (self.grid_ny - y0).min(COL_Y),
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.chunk_count - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for SelectedPressureLevelChunks3D {}
impl std::iter::FusedIterator for SelectedPressureLevelChunks3D {}

/// Allocation-free, row-major enumeration of a pressure variable's column
/// chunks for one exact pressure level.
#[derive(Debug, Clone)]
pub struct PressureLevelChunks3D {
    grid_nx: usize,
    grid_ny: usize,
    chunks_x: usize,
    chunks_y: usize,
    level_hpa: u16,
    next: usize,
    chunk_count: usize,
}

impl PressureLevelChunks3D {
    pub const fn chunks_x(&self) -> usize {
        self.chunks_x
    }

    pub const fn chunks_y(&self) -> usize {
        self.chunks_y
    }

    pub const fn level_hpa(&self) -> u16 {
        self.level_hpa
    }
}

impl Iterator for PressureLevelChunks3D {
    type Item = PressureLevelChunkGeometry3D;

    fn next(&mut self) -> Option<Self::Item> {
        if self.next == self.chunk_count {
            return None;
        }
        let ordinal = self.next;
        self.next += 1;
        let chunk_y = ordinal / self.chunks_x;
        let chunk_x = ordinal % self.chunks_x;
        let y0 = chunk_y * COL_Y;
        let x0 = chunk_x * COL_X;
        Some(PressureLevelChunkGeometry3D {
            chunk_y,
            chunk_x,
            x0,
            y0,
            width: (self.grid_nx - x0).min(COL_X),
            height: (self.grid_ny - y0).min(COL_Y),
            level_hpa: self.level_hpa,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.chunk_count - self.next;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for PressureLevelChunks3D {}
impl std::iter::FusedIterator for PressureLevelChunks3D {}

/// Finite-value summary for one complete 2D field.
///
/// `finite_min` and `finite_max` are `None` when the field has no finite
/// values. `missing_count` counts every non-finite source value represented
/// by the store (normally NaN).
///
/// The summary is aggregated from the per-tile statistics in the validated
/// chunk index, so obtaining it does not decompress field payloads. These are
/// writer-recorded statistics, not a deep revalidation of the payload bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FieldStats2D {
    pub finite_min: Option<f32>,
    pub finite_max: Option<f32>,
    pub finite_count: u64,
    pub missing_count: u64,
}

/// Read-only handle to one rw-store hour file.
///
/// Debug is implemented manually: deriving it would dump the entire mapped
/// file contents into panic messages.
pub struct HourReader {
    bytes: FileBytes,
    /// Stable identity of the exact file handle backing `bytes`, retained so
    /// a same-path atomic replacement cannot be confused with this reader.
    source: Handle,
    meta: RwsHourMeta,
    records: Vec<ChunkRecord>,
    /// Per-variable contiguous slice of `records`, built once at open so
    /// per-tile lookups binary-search only that variable's records.
    var_ranges: BTreeMap<u16, Range<usize>>,
    tile_cache: Mutex<DecodedTileCache>,
}

impl std::fmt::Debug for HourReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HourReader")
            .field(
                "backing",
                &match self.bytes {
                    FileBytes::Mmap(_) => "mmap",
                    FileBytes::Ram(_) => "ram",
                },
            )
            .field("file_len", &self.bytes.as_slice().len())
            .field("meta", &self.meta)
            .field("records", &self.records.len())
            .finish()
    }
}

impl HourReader {
    /// Open and validate an hour file: header, meta JSON, full chunk index
    /// (including sort order — trust nothing on disk).
    pub fn open(path: &Path) -> RwResult<Self> {
        Self::open_with_tile_cache_bytes(path, DEFAULT_TILE_CACHE_BYTES)
    }

    /// Open with a caller-selected decoded dense-2D cache bound.
    /// A zero-byte bound disables caching; allocation remains lazy.
    pub fn open_with_tile_cache_bytes(path: &Path, tile_cache_bytes: usize) -> RwResult<Self> {
        let (bytes, source) = Self::open_bytes(path)?;
        let data = bytes.as_slice();

        let (header, meta) = parse_and_validate_hour(data)?;
        let index_count = usize::try_from(header.index_count).map_err(|_| {
            RwStoreError::Format(format!(
                "chunk index count {} does not fit usize",
                header.index_count
            ))
        })?;
        let variables: BTreeMap<u16, &RwsVariableMeta> =
            meta.variables.iter().map(|var| (var.id, var)).collect();
        let mut records = Vec::new();
        records.try_reserve_exact(index_count).map_err(|err| {
            RwStoreError::Format(format!(
                "cannot allocate {index_count} parsed chunk records: {err}"
            ))
        })?;
        let index_offset = usize::try_from(header.index_offset).map_err(|_| {
            RwStoreError::Format(format!(
                "chunk index offset {} does not fit usize",
                header.index_offset
            ))
        })?;
        for i in 0..index_count {
            let start = i
                .checked_mul(INDEX_RECORD_LEN)
                .and_then(|offset| index_offset.checked_add(offset))
                .ok_or_else(|| {
                    RwStoreError::Format(format!("chunk index record {i} offset overflows usize"))
                })?;
            let end = start.checked_add(INDEX_RECORD_LEN).ok_or_else(|| {
                RwStoreError::Format(format!("chunk index record {i} end overflows usize"))
            })?;
            let record = ChunkRecord::unpack(&data[start..end])?;
            validate_chunk_record(i, &record, &meta, &header, data.len() as u64, &variables)?;
            records.push(record);
        }
        for (i, pair) in records.windows(2).enumerate() {
            if pair[0].sort_key() >= pair[1].sort_key() {
                return Err(RwStoreError::Format(format!(
                    "chunk index sort order violated at records {i}..{}: {:?} !< {:?}",
                    i + 1,
                    pair[0].sort_key(),
                    pair[1].sort_key()
                )));
            }
        }

        // Records are sorted by (var_id, kind, tile_y, tile_x), so each
        // variable's records form one contiguous run.
        let mut var_ranges = BTreeMap::new();
        if !records.is_empty() {
            let mut run_start = 0usize;
            for end in 1..=records.len() {
                if end == records.len() || records[end].var_id != records[run_start].var_id {
                    var_ranges.insert(records[run_start].var_id, run_start..end);
                    run_start = end;
                }
            }
        }

        Ok(Self {
            bytes,
            source,
            meta,
            records,
            var_ranges,
            tile_cache: Mutex::new(DecodedTileCache::new(tile_cache_bytes)),
        })
    }

    fn open_bytes(path: &Path) -> RwResult<(FileBytes, Handle)> {
        let mut file = File::open(path)?;
        let source = Handle::from_file(file.try_clone()?)?;
        let file_len = file.metadata()?.len();
        if file_len < HEADER_LEN as u64 {
            return Err(RwStoreError::Format(format!(
                "header requires {HEADER_LEN} bytes, got {file_len}"
            )));
        }
        if file_len > MAX_HOUR_FILE_LEN {
            return Err(RwStoreError::Format(format!(
                "hour file is {file_len} bytes; limit is {MAX_HOUR_FILE_LEN} bytes"
            )));
        }

        // Preflight the bounded header and metadata before attempting either
        // a map or a whole-file RAM fallback.
        let mut header_bytes = [0u8; HEADER_LEN];
        file.read_exact(&mut header_bytes)?;
        let header = RwsHeader::parse(&header_bytes)?;
        validate_header_bounds(&header, file_len)?;
        let meta_len = header.meta_len as usize;
        let mut meta_bytes = try_zeroed_bytes(meta_len, "hour metadata")?;
        file.read_exact(&mut meta_bytes)?;
        let meta = parse_hour_meta(&meta_bytes)?;
        validate_hour_meta(&meta, &header)?;

        // SAFETY: the map is read-only over a read-only handle. Header/index
        // spans were preflighted, and all subsequent ranges are checked again
        // against the actual mapped length to handle concurrent file changes.
        if let Ok(mmap) = unsafe { Mmap::map(&file) } {
            return Ok((FileBytes::Mmap(mmap), source));
        }
        let bytes = Self::read_file_to_ram(&mut file, file_len)?;
        Ok((bytes, source))
    }

    fn read_file_to_ram(file: &mut File, file_len: u64) -> RwResult<FileBytes> {
        if file_len > MAX_RAM_FALLBACK_FILE_LEN {
            return Err(RwStoreError::Format(format!(
                "memory mapping failed and the {file_len}-byte hour file exceeds the {MAX_RAM_FALLBACK_FILE_LEN}-byte RAM fallback limit"
            )));
        }
        let len = usize::try_from(file_len).map_err(|_| {
            RwStoreError::Format(format!("hour file length {file_len} does not fit usize"))
        })?;
        let mut bytes = try_zeroed_bytes(len, "hour-file RAM fallback")?;
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut bytes)?;
        let mut trailing = [0u8; 1];
        if file.read(&mut trailing)? != 0 {
            return Err(RwStoreError::Format(
                "hour file grew while it was being read into RAM".to_string(),
            ));
        }
        Ok(FileBytes::Ram(bytes))
    }

    /// Hour-level metadata parsed from the file.
    pub fn meta(&self) -> &RwsHourMeta {
        &self.meta
    }

    /// Metadata for the variable named `name`, if present.
    pub fn variable(&self, name: &str) -> Option<&RwsVariableMeta> {
        self.meta.variables.iter().find(|var| var.name == name)
    }

    /// Whether `path` currently resolves to the exact file object opened by
    /// this reader, rather than merely a path with matching metadata.
    ///
    /// This remains meaningful after an atomic same-name replacement because
    /// the reader retains a cloned handle to its mmap/RAM source.
    pub fn source_matches_path(&self, path: &Path) -> RwResult<bool> {
        let current = Handle::from_path(path)?;
        Ok(self.source == current)
    }

    /// Current decoded dense-2D cache counters and occupancy.
    pub fn tile_cache_stats(&self) -> TileCacheStats {
        self.tile_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stats()
    }

    /// Drop every cached dense-2D tile and reset its counters.
    pub fn clear_tile_cache(&self) {
        self.tile_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    /// Return finite-value statistics for the complete 2D field `name`
    /// without decompressing any tile payloads.
    ///
    /// The per-tile min/max/count records are checked for the invariants that
    /// can be established from the index alone before they are aggregated.
    /// Use deep store validation when payload-to-index verification is
    /// required.
    ///
    /// There is intentionally no index-only `stats_window_2d`: index
    /// statistics describe complete tiles, so an arbitrary window's boundary
    /// tiles must be decoded to obtain exact results. Use [`Self::tiles_2d`]
    /// with [`Self::read_tile_2d`] or [`Self::read_window_2d`] for that case.
    pub fn stats_2d(&self, name: &str) -> RwResult<FieldStats2D> {
        let var = self.lookup(name)?;
        if var.kind != "surface2d" {
            return Err(RwStoreError::Format(format!(
                "variable '{name}' has kind '{}', expected 'surface2d'",
                var.kind
            )));
        }

        let tiles_y = self.meta.ny.div_ceil(TILE_Y);
        let tiles_x = self.meta.nx.div_ceil(TILE_X);
        let expected_records = tiles_y.checked_mul(tiles_x).ok_or_else(|| {
            RwStoreError::Format(format!("variable '{name}': tile count overflows usize"))
        })?;
        let range = self.var_ranges.get(&var.id).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}' (id {}) has no chunk index entries",
                var.name, var.id
            ))
        })?;
        if range.len() != expected_records {
            return Err(RwStoreError::Format(format!(
                "variable '{name}' has {} chunk index records, expected {expected_records}",
                range.len()
            )));
        }

        let mut finite_min: Option<f32> = None;
        let mut finite_max: Option<f32> = None;
        let mut finite_count = 0u64;

        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                let record = self.chunk_record(var, KIND_TILE2D, ty, tx)?;
                let (rows, cols) = self.tile_dims(ty, tx);
                let tile_count = rows.checked_mul(cols).ok_or_else(|| {
                    RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) cell count overflows usize"
                    ))
                })?;
                let valid_count = record.valid_count as usize;
                if valid_count > tile_count {
                    return Err(RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) valid_count {valid_count} exceeds tile cell count {tile_count}"
                    )));
                }

                let is_empty = record.flags & FLAG_EMPTY != 0;
                let has_missing = record.flags & FLAG_HAS_MISSING != 0;
                let is_constant = record.flags & FLAG_CONSTANT != 0;
                if is_empty {
                    if valid_count != 0 || !record.min.is_nan() || !record.max.is_nan() {
                        return Err(RwStoreError::Format(format!(
                            "variable '{name}' tile ({ty},{tx}) has inconsistent EMPTY statistics"
                        )));
                    }
                    continue;
                }

                if valid_count == 0 {
                    return Err(RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) has zero valid values without EMPTY"
                    )));
                }
                if !record.min.is_finite() || !record.max.is_finite() || record.min > record.max {
                    return Err(RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) has invalid finite range {} .. {}",
                        record.min, record.max
                    )));
                }
                if has_missing != (valid_count < tile_count) {
                    return Err(RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) HAS_MISSING flag is inconsistent with valid_count {valid_count} of {tile_count}"
                    )));
                }
                if is_constant
                    && (!record.center.is_finite()
                        || record.min != record.center
                        || record.max != record.center)
                {
                    return Err(RwStoreError::Format(format!(
                        "variable '{name}' tile ({ty},{tx}) has inconsistent CONSTANT statistics"
                    )));
                }

                finite_min = Some(finite_min.map_or(record.min, |value| value.min(record.min)));
                finite_max = Some(finite_max.map_or(record.max, |value| value.max(record.max)));
                finite_count = finite_count
                    .checked_add(u64::from(record.valid_count))
                    .ok_or_else(|| {
                        RwStoreError::Format(format!(
                            "variable '{name}' finite value count overflows u64"
                        ))
                    })?;
            }
        }

        let total_count =
            u64::try_from(self.meta.nx.checked_mul(self.meta.ny).ok_or_else(|| {
                RwStoreError::Format(format!("variable '{name}' grid cell count overflows usize"))
            })?)
            .map_err(|_| {
                RwStoreError::Format(format!(
                    "variable '{name}' grid cell count does not fit u64"
                ))
            })?;
        let missing_count = total_count.checked_sub(finite_count).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{name}' finite value count {finite_count} exceeds grid cell count {total_count}"
            ))
        })?;

        Ok(FieldStats2D {
            finite_min,
            finite_max,
            finite_count,
            missing_count,
        })
    }

    /// Read the full `ny * nx` row-major field for `name`; positions inside
    /// EMPTY tiles come back as NaN. Tiles decode in parallel when the
    /// variable has more than [`PARALLEL_TILE_THRESHOLD`] of them; results
    /// are placed serially so output is deterministic either way.
    pub fn read_full_2d(&self, name: &str) -> RwResult<Vec<f32>> {
        let var = self.lookup_2d(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        let tiles_y = ny.div_ceil(TILE_Y);
        let tiles_x = nx.div_ceil(TILE_X);

        // Resolve all records up front so index problems surface before any
        // decompression work starts.
        let job_count = tiles_y.checked_mul(tiles_x).ok_or_else(|| {
            RwStoreError::Format(format!("variable '{name}': tile count overflows usize"))
        })?;
        let mut jobs = Vec::new();
        jobs.try_reserve_exact(job_count).map_err(|err| {
            RwStoreError::Format(format!(
                "cannot allocate {job_count} tile jobs for variable '{name}': {err}"
            ))
        })?;
        for ty in 0..tiles_y {
            for tx in 0..tiles_x {
                jobs.push((ty, tx, self.chunk_record(var, KIND_TILE2D, ty, tx)?));
            }
        }

        let decode = |&(ty, tx, record): &(usize, usize, &ChunkRecord)| {
            let (rows, cols) = self.tile_dims(ty, tx);
            Ok((ty, tx, self.decode_tile(&var.name, record, rows * cols)?))
        };
        let decoded: Vec<(usize, usize, Vec<f32>)> = if jobs.len() > PARALLEL_TILE_THRESHOLD {
            jobs.par_iter().map(decode).collect::<RwResult<_>>()?
        } else {
            jobs.iter().map(decode).collect::<RwResult<_>>()?
        };

        let cells = ny.checked_mul(nx).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{name}': grid cell count overflows usize"
            ))
        })?;
        let mut values = try_filled_f32(cells, f32::NAN, &format!("variable '{name}'"))?;
        for (ty, tx, tile) in decoded {
            let (rows, cols) = self.tile_dims(ty, tx);
            let (y0, x0) = (ty * TILE_Y, tx * TILE_X);
            for row in 0..rows {
                let out = (y0 + row) * nx + x0;
                values[out..out + cols].copy_from_slice(&tile[row * cols..(row + 1) * cols]);
            }
        }
        Ok(values)
    }

    /// Enumerate the complete tile layout of 2D surface variable `name` in
    /// row-major tile order without allocating a geometry list or decoding
    /// payloads.
    pub fn tiles_2d(&self, name: &str) -> RwResult<Tiles2D> {
        let var = self.lookup_2d(name)?;
        self.tiles_2d_for_var(var)
    }

    /// Read one tile of 2D surface variable `name`.
    ///
    /// The tile indices come from [`Self::tiles_2d`]. Out-of-range indices are
    /// rejected before offset arithmetic. EMPTY and payload-free CONSTANT
    /// tiles remain allocation-free; dense tiles use the same validated
    /// decoder and bounded cache as point/window reads.
    pub fn read_tile_2d(&self, name: &str, tile_y: usize, tile_x: usize) -> RwResult<Tile2D> {
        let var = self.lookup_2d(name)?;
        let geometry = self.tile_geometry_2d(&var.name, tile_y, tile_x)?;
        self.read_tile_2d_for_var(var, geometry)
    }

    /// Visit a 2D surface variable one tile at a time in row-major tile order.
    ///
    /// The visitor receives a borrowed tile which is dropped before the next
    /// tile is read. Apart from data a visitor deliberately clones, memory is
    /// bounded by one current tile plus this reader's configured tile cache;
    /// no full-plane allocation is made. The first read or visitor error stops
    /// iteration and is returned unchanged.
    pub fn visit_tiles_2d<F>(&self, name: &str, mut visitor: F) -> RwResult<()>
    where
        F: FnMut(&Tile2D) -> RwResult<()>,
    {
        let var = self.lookup_2d(name)?;
        let tiles = self.tiles_2d_for_var(var)?;
        for geometry in tiles {
            let tile = self.read_tile_2d_for_var(var, geometry)?;
            visitor(&tile)?;
        }
        Ok(())
    }

    /// Read one bit-exact 2D value without allocating a one-cell window.
    ///
    /// Dense tiles use the same bounded cache as window reads; EMPTY and
    /// payload-free CONSTANT tiles return directly from the index.
    pub fn read_point_2d(&self, name: &str, ix: usize, iy: usize) -> RwResult<f32> {
        let var = self.lookup_2d(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        if ix >= nx || iy >= ny {
            return Err(RwStoreError::Format(format!(
                "point ({ix},{iy}) out of bounds for grid {nx} x {ny}"
            )));
        }

        let (ty, tx) = (iy / TILE_Y, ix / TILE_X);
        let record = self.chunk_record(var, KIND_TILE2D, ty, tx)?;
        if record.flags & FLAG_EMPTY != 0 {
            return Ok(f32::NAN);
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return Ok(record.center);
        }

        let (rows, cols) = self.tile_dims(ty, tx);
        let tile = self.decode_tile_cached(var, record, rows * cols)?;
        let row = iy - ty * TILE_Y;
        let col = ix - tx * TILE_X;
        let offset = row
            .checked_mul(cols)
            .and_then(|start| start.checked_add(col))
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "point ({ix},{iy}) offset overflows for variable '{name}'"
                ))
            })?;
        tile.get(offset).copied().ok_or_else(|| {
            RwStoreError::Format(format!(
                "point ({ix},{iy}) offset {offset} exceeds decoded tile for variable '{name}'"
            ))
        })
    }

    /// Read the half-open window `[x0,x1) x [y0,y1)` of `name`, clamped to
    /// the grid. Only tiles intersecting the window are touched: EMPTY tiles
    /// fill NaN and CONSTANT tiles fill their center without reading any
    /// payload bytes; dense tiles are decompressed individually (in parallel
    /// when the window spans more than [`PARALLEL_TILE_THRESHOLD`] tiles,
    /// same as [`Self::read_full_2d`]) and only the intersecting rows/cols
    /// are copied out.
    pub fn read_window_2d(
        &self,
        name: &str,
        x0: usize,
        y0: usize,
        x1: usize,
        y1: usize,
    ) -> RwResult<Window2D> {
        let var = self.lookup_2d(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        let x1 = x1.min(nx);
        let y1 = y1.min(ny);
        if x0 >= x1 || y0 >= y1 {
            return Err(RwStoreError::Format(format!(
                "window [{x0},{x1}) x [{y0},{y1}) is empty after clamping to grid {nx} x {ny}"
            )));
        }
        let wnx = x1 - x0;
        let wny = y1 - y0;
        if wnx == 1 && wny == 1 {
            let value = self.read_point_2d(name, x0, y0)?;
            return Ok(Window2D {
                x0,
                y0,
                nx: 1,
                ny: 1,
                values: try_filled_f32(1, value, &format!("variable '{name}' one-cell window"))?,
            });
        }
        let window_cells = wny.checked_mul(wnx).ok_or_else(|| {
            RwStoreError::Format(format!("variable '{name}': window size overflows usize"))
        })?;
        let mut values =
            try_filled_f32(window_cells, f32::NAN, &format!("variable '{name}' window"))?;

        // Resolve every intersecting tile's record up front so index
        // problems surface before any decompression work starts.
        let (first_ty, last_ty) = (y0 / TILE_Y, (y1 - 1) / TILE_Y);
        let (first_tx, last_tx) = (x0 / TILE_X, (x1 - 1) / TILE_X);
        let job_count = (last_ty - first_ty + 1)
            .checked_mul(last_tx - first_tx + 1)
            .ok_or_else(|| {
                RwStoreError::Format(format!("variable '{name}': window tile count overflows"))
            })?;
        let mut jobs = Vec::new();
        jobs.try_reserve_exact(job_count).map_err(|err| {
            RwStoreError::Format(format!(
                "cannot allocate {job_count} window tile jobs for variable '{name}': {err}"
            ))
        })?;
        for ty in first_ty..=last_ty {
            for tx in first_tx..=last_tx {
                jobs.push((ty, tx, self.chunk_record(var, KIND_TILE2D, ty, tx)?));
            }
        }

        // Dense tiles decompress (in parallel above the threshold); EMPTY and
        // CONSTANT tiles are handled from their records alone at placement.
        let decode = |&(ty, tx, record): &(usize, usize, &ChunkRecord)| {
            if record.flags & FLAG_EMPTY != 0
                || (record.flags & FLAG_CONSTANT != 0 && record.len == 0)
            {
                return Ok(None);
            }
            let (rows, cols) = self.tile_dims(ty, tx);
            Ok(Some(self.decode_tile_cached(var, record, rows * cols)?))
        };
        let decoded: Vec<Option<Arc<Vec<f32>>>> = if jobs.len() > PARALLEL_TILE_THRESHOLD {
            jobs.par_iter().map(decode).collect::<RwResult<_>>()?
        } else {
            jobs.iter().map(decode).collect::<RwResult<_>>()?
        };

        for (&(ty, tx, record), tile) in jobs.iter().zip(decoded) {
            let (rows, cols) = self.tile_dims(ty, tx);
            let (ty0, tx0) = (ty * TILE_Y, tx * TILE_X);
            // Window/tile intersection in grid coordinates.
            let gy0 = ty0.max(y0);
            let gy1 = (ty0 + rows).min(y1);
            let gx0 = tx0.max(x0);
            let gx1 = (tx0 + cols).min(x1);

            match tile {
                None if record.flags & FLAG_EMPTY != 0 => {
                    // Output is pre-filled with NaN.
                }
                None => {
                    for gy in gy0..gy1 {
                        let out = (gy - y0) * wnx;
                        values[out + (gx0 - x0)..out + (gx1 - x0)].fill(record.center);
                    }
                }
                Some(tile) => {
                    for gy in gy0..gy1 {
                        let src = (gy - ty0) * cols;
                        let out = (gy - y0) * wnx;
                        values[out + (gx0 - x0)..out + (gx1 - x0)]
                            .copy_from_slice(&tile[src + (gx0 - tx0)..src + (gx1 - tx0)]);
                    }
                }
            }
        }

        Ok(Window2D {
            x0,
            y0,
            nx: wnx,
            ny: wny,
            values,
        })
    }

    /// Read the full pressure column of 3D variable `name` at grid point
    /// (`ix`, `iy`): one chunk decode, one contiguous slice. The result has
    /// one value per entry of the variable's `levels_hpa` (descending
    /// pressure: index 0 is the lowest level, e.g. 1000 hPa).
    pub fn read_column_3d(&self, name: &str, ix: usize, iy: usize) -> RwResult<Vec<f32>> {
        let var = self.lookup_3d(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        if ix >= nx || iy >= ny {
            return Err(RwStoreError::Format(format!(
                "column ({ix},{iy}) out of bounds for grid {nx} x {ny}"
            )));
        }
        let levels = var.levels_hpa.len();
        let (cy, cx) = (iy / COL_Y, ix / COL_X);
        let record = self.chunk_record(var, KIND_COLUMN3D, cy, cx)?;
        let (rows, cols) = self.col_chunk_dims(cy, cx);
        let chunk = self.decode_column_chunk(&var.name, record, rows * cols * levels)?;
        // [y][x][z] layout: the column's L values are contiguous.
        let start = ((iy % COL_Y) * cols + (ix % COL_X)) * levels;
        let mut column = try_filled_f32(levels, f32::NAN, &format!("variable '{name}' column"))?;
        column.copy_from_slice(&chunk[start..start + levels]);
        Ok(column)
    }

    /// Read the full pressure volume for 3D variable `name` as a
    /// **level-major** flat array in `[level][y][x]` order (length =
    /// `levels * ny * nx`; flat index = `lvl * ny * nx + iy * nx + ix`).
    /// NaN is returned for missing/EMPTY cells.
    ///
    /// Each column chunk is decoded exactly once; the `[y][x][level]`
    /// chunk payload is scattered into the level-major output without any
    /// per-column re-decode.
    ///
    /// Errors:
    /// - [`RwStoreError::UnknownVariable`] if `name` is not in the file.
    /// - [`RwStoreError::Format`] if `name` is not a `pressure3d` variable
    ///   (consistent with [`Self::read_column_3d`]).
    pub fn read_full_3d(&self, name: &str) -> RwResult<Vec<f32>> {
        let var = self.lookup_3d(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        let levels = var.levels_hpa.len();
        // Checked arithmetic: these are all small counts for any realistic grid.
        let total = levels
            .checked_mul(ny)
            .and_then(|n| n.checked_mul(nx))
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "variable '{name}': volume size levels={levels} * ny={ny} * nx={nx} \
                     overflows usize"
                ))
            })?;

        let chunks_y = ny.div_ceil(COL_Y);
        let chunks_x = nx.div_ceil(COL_X);

        let mut output = try_filled_f32(
            total,
            f32::NAN,
            &format!("variable '{name}' pressure volume"),
        )?;

        for cy in 0..chunks_y {
            for cx in 0..chunks_x {
                let record = self.chunk_record(var, KIND_COLUMN3D, cy, cx)?;
                let (rows, cols) = self.col_chunk_dims(cy, cx);
                let chunk = self.decode_column_chunk(&var.name, record, rows * cols * levels)?;
                // chunk layout: [y_local][x_local][level] (contiguous per column).
                // Scatter into output: [level][y_global][x_global].
                let y0 = cy * COL_Y;
                let x0 = cx * COL_X;
                for gy_local in 0..rows {
                    let gy = y0 + gy_local;
                    for gx_local in 0..cols {
                        let gx = x0 + gx_local;
                        let chunk_base = (gy_local * cols + gx_local) * levels;
                        for lvl in 0..levels {
                            output[lvl * ny * nx + gy * nx + gx] = chunk[chunk_base + lvl];
                        }
                    }
                }
            }
        }

        Ok(output)
    }

    /// Read one pressure level from a 3-D variable as a row-major `[y][x]`
    /// plane. Every column chunk is decoded once, but only the requested
    /// value per column is retained, avoiding a `levels * ny * nx` allocation
    /// when a map viewer needs one isobaric surface.
    pub fn read_level_3d(&self, name: &str, level_hpa: u16) -> RwResult<Vec<f32>> {
        let var = self.lookup_3d(name)?;
        let level_index = Self::pressure_level_index(var, level_hpa)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        let cells = nx.checked_mul(ny).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{name}': plane size ny={ny} * nx={nx} overflows usize"
            ))
        })?;
        let levels = var.levels_hpa.len();
        let chunks_y = ny.div_ceil(COL_Y);
        let chunks_x = nx.div_ceil(COL_X);
        let mut output = try_filled_f32(
            cells,
            f32::NAN,
            &format!("variable '{name}' pressure level"),
        )?;

        for cy in 0..chunks_y {
            for cx in 0..chunks_x {
                let record = self.chunk_record(var, KIND_COLUMN3D, cy, cx)?;
                let (rows, cols) = self.col_chunk_dims(cy, cx);
                let chunk = self.decode_column_chunk(&var.name, record, rows * cols * levels)?;
                let y0 = cy * COL_Y;
                let x0 = cx * COL_X;
                for gy_local in 0..rows {
                    let gy = y0 + gy_local;
                    for gx_local in 0..cols {
                        let gx = x0 + gx_local;
                        let column = (gy_local * cols + gx_local) * levels;
                        output[gy * nx + gx] = chunk[column + level_index];
                    }
                }
            }
        }

        Ok(output)
    }

    /// Enumerate a pressure variable's 16x16 column chunks for one exact
    /// pressure level, in row-major chunk order. This validates the variable
    /// kind, requested level, and exact chunk-index cardinality without
    /// decoding payloads or allocating a geometry list.
    pub fn pressure_level_chunks_3d(
        &self,
        name: &str,
        level_hpa: u16,
    ) -> RwResult<PressureLevelChunks3D> {
        let var = self.lookup_3d(name)?;
        Self::pressure_level_index(var, level_hpa)?;
        self.pressure_level_chunks_3d_for_var(var, level_hpa)
    }

    /// Read one 16x16 (edge-clipped) column-chunk plane for an exact pressure
    /// level. The underlying `[y][x][level]` column chunk is decoded once and
    /// only the requested level is retained; no full level plane or pressure
    /// volume is allocated. EMPTY and payload-free CONSTANT chunks remain
    /// allocation-free in the returned representation.
    pub fn read_pressure_level_chunk_3d(
        &self,
        name: &str,
        level_hpa: u16,
        chunk_y: usize,
        chunk_x: usize,
    ) -> RwResult<PressureLevelChunk3D> {
        let var = self.lookup_3d(name)?;
        let level_index = Self::pressure_level_index(var, level_hpa)?;
        let geometry =
            self.pressure_level_chunk_geometry_3d(&var.name, level_hpa, chunk_y, chunk_x)?;
        self.read_pressure_level_chunk_3d_for_var(var, level_index, geometry)
    }

    /// Visit one exact pressure level a column chunk at a time in row-major
    /// chunk order. The callback returns before the next chunk is decoded, so
    /// callers can implement cancellation or early termination between
    /// chunks by returning an error. No full plane or volume is allocated.
    pub fn visit_pressure_level_chunks_3d<F>(
        &self,
        name: &str,
        level_hpa: u16,
        mut visitor: F,
    ) -> RwResult<()>
    where
        F: FnMut(&PressureLevelChunk3D) -> RwResult<()>,
    {
        let var = self.lookup_3d(name)?;
        let level_index = Self::pressure_level_index(var, level_hpa)?;
        let chunks = self.pressure_level_chunks_3d_for_var(var, level_hpa)?;
        for geometry in chunks {
            let chunk = self.read_pressure_level_chunk_3d_for_var(var, level_index, geometry)?;
            visitor(&chunk)?;
        }
        Ok(())
    }

    /// Enumerate a pressure variable's 16x16 column chunks for a nonempty,
    /// unique list of exact pressure levels, in row-major chunk order.
    ///
    /// The requested level order is preserved. Validation rejects duplicate
    /// or absent levels before any payload is decoded. Geometry enumeration
    /// allocates only the bounded level selection, not a chunk list, plane,
    /// or pressure volume.
    pub fn selected_pressure_level_chunks_3d(
        &self,
        name: &str,
        levels_hpa: &[u16],
    ) -> RwResult<SelectedPressureLevelChunks3D> {
        let var = self.lookup_3d(name)?;
        Self::selected_pressure_level_indices(var, levels_hpa)?;
        self.selected_pressure_level_chunks_3d_for_var(var, Arc::from(levels_hpa))
    }

    /// Read one 16x16 (edge-clipped) column chunk for a nonempty, unique,
    /// explicitly ordered pressure-level selection.
    ///
    /// The underlying `[y][x][all_levels]` chunk is decompressed once, and
    /// only the selected planes are decoded into the result. Dense output is
    /// bounded by `selected_levels * 16 * 16`; no full grid plane or volume
    /// is allocated. Payload-free EMPTY and CONSTANT chunks remain
    /// allocation-free in the returned data representation.
    pub fn read_selected_pressure_level_chunk_3d(
        &self,
        name: &str,
        levels_hpa: &[u16],
        chunk_y: usize,
        chunk_x: usize,
    ) -> RwResult<SelectedPressureLevelChunk3D> {
        let var = self.lookup_3d(name)?;
        let level_indices = Self::selected_pressure_level_indices(var, levels_hpa)?;
        let geometry =
            self.selected_pressure_level_chunk_geometry_3d(&var.name, chunk_y, chunk_x)?;
        let data =
            self.read_selected_pressure_level_chunk_data_3d_for_var(var, &level_indices, geometry)?;
        Ok(SelectedPressureLevelChunk3D {
            geometry,
            levels_hpa: Arc::from(levels_hpa),
            data,
        })
    }

    /// Visit an explicit ordered pressure-level selection one column chunk
    /// at a time in row-major chunk order.
    ///
    /// Every source chunk is decompressed at most once. The callback returns
    /// before the next chunk is decoded, so returning an error provides an
    /// immediate cancellation/early-termination checkpoint. No full grid
    /// plane or pressure volume is allocated.
    pub fn visit_selected_pressure_level_chunks_3d<F>(
        &self,
        name: &str,
        levels_hpa: &[u16],
        mut visitor: F,
    ) -> RwResult<()>
    where
        F: FnMut(&SelectedPressureLevelChunk3D) -> RwResult<()>,
    {
        let var = self.lookup_3d(name)?;
        let level_indices = Self::selected_pressure_level_indices(var, levels_hpa)?;
        let levels_hpa: Arc<[u16]> = Arc::from(levels_hpa);
        let chunks =
            self.selected_pressure_level_chunks_3d_for_var(var, Arc::clone(&levels_hpa))?;
        for geometry in chunks {
            let data = self.read_selected_pressure_level_chunk_data_3d_for_var(
                var,
                &level_indices,
                geometry,
            )?;
            visitor(&SelectedPressureLevelChunk3D {
                geometry,
                levels_hpa: Arc::clone(&levels_hpa),
                data,
            })?;
        }
        Ok(())
    }

    /// Read a bilinearly interpolated pressure profile of 3D variable
    /// `name` at fractional grid coordinates (`fx`, `fy`), clamped to the
    /// grid. Per level the value is the weighted mean over the FINITE corner
    /// columns only (weights renormalized); a level where all corners are
    /// NaN yields NaN. Each underlying chunk is decoded at most once.
    pub fn read_profile_3d(&self, name: &str, fx: f64, fy: f64) -> RwResult<Vec<f32>> {
        let var = self.lookup_3d(name)?;
        if !fx.is_finite() || !fy.is_finite() {
            return Err(RwStoreError::Format(format!(
                "profile coordinates must be finite, got ({fx}, {fy})"
            )));
        }
        let levels = var.levels_hpa.len();
        let fx = fx.clamp(0.0, (self.meta.nx - 1) as f64);
        let fy = fy.clamp(0.0, (self.meta.ny - 1) as f64);
        let (x0, x1) = (fx.floor() as usize, fx.ceil() as usize);
        let (y0, y1) = (fy.floor() as usize, fy.ceil() as usize);
        let wx = (fx - x0 as f64) as f32;
        let wy = (fy - y0 as f64) as f32;
        // Degenerate axes (exact integer / edge) produce duplicate corners;
        // their weights still sum to 1, so no special-casing is needed.
        let corners = [
            (x0, y0, (1.0 - wx) * (1.0 - wy)),
            (x1, y0, wx * (1.0 - wy)),
            (x0, y1, (1.0 - wx) * wy),
            (x1, y1, wx * wy),
        ];

        // Decode every chunk the corners touch exactly once (up to 4
        // corners may share chunks); tiny linear map, max 4 entries.
        let mut chunks: Vec<((usize, usize), Vec<f32>)> = Vec::with_capacity(4);
        for &(ix, iy, _) in &corners {
            let key = (iy / COL_Y, ix / COL_X);
            if chunks.iter().any(|(have, _)| *have == key) {
                continue;
            }
            let record = self.chunk_record(var, KIND_COLUMN3D, key.0, key.1)?;
            let (rows, cols) = self.col_chunk_dims(key.0, key.1);
            let decoded = self.decode_column_chunk(&var.name, record, rows * cols * levels)?;
            chunks.push((key, decoded));
        }
        let corner_columns: Vec<(&[f32], f32)> = corners
            .iter()
            .map(|&(ix, iy, weight)| {
                let key = (iy / COL_Y, ix / COL_X);
                let (_, cols) = self.col_chunk_dims(key.0, key.1);
                let chunk = &chunks.iter().find(|(have, _)| *have == key).unwrap().1;
                let start = ((iy % COL_Y) * cols + (ix % COL_X)) * levels;
                (&chunk[start..start + levels], weight)
            })
            .collect();

        let mut profile = Vec::with_capacity(levels);
        for k in 0..levels {
            let mut weight_sum = 0.0f32;
            let mut value_sum = 0.0f32;
            for (column, weight) in &corner_columns {
                let value = column[k];
                if value.is_finite() {
                    weight_sum += weight;
                    value_sum += weight * value;
                }
            }
            profile.push(if weight_sum > 0.0 {
                value_sum / weight_sum
            } else {
                f32::NAN
            });
        }
        Ok(profile)
    }

    fn lookup(&self, name: &str) -> RwResult<&RwsVariableMeta> {
        self.variable(name)
            .ok_or_else(|| RwStoreError::UnknownVariable(name.to_string()))
    }

    /// Like [`Self::lookup`], but additionally require a 2D surface variable.
    fn lookup_2d(&self, name: &str) -> RwResult<&RwsVariableMeta> {
        let var = self.lookup(name)?;
        if var.kind != "surface2d" {
            return Err(RwStoreError::Format(format!(
                "variable '{name}' has kind '{}', expected 'surface2d'",
                var.kind
            )));
        }
        Ok(var)
    }

    /// Like [`Self::lookup`], but additionally require a 3D pressure-level
    /// variable.
    fn lookup_3d(&self, name: &str) -> RwResult<&RwsVariableMeta> {
        let var = self.lookup(name)?;
        if var.kind != "pressure3d" {
            return Err(RwStoreError::Format(format!(
                "variable '{name}' has kind '{}', expected 'pressure3d'",
                var.kind
            )));
        }
        Ok(var)
    }

    fn pressure_level_index(var: &RwsVariableMeta, level_hpa: u16) -> RwResult<usize> {
        var.levels_hpa
            .iter()
            .position(|level| *level == level_hpa)
            .ok_or_else(|| {
                RwStoreError::Meta(format!(
                    "variable '{}' has no {level_hpa} hPa level",
                    var.name
                ))
            })
    }

    fn selected_pressure_level_indices(
        var: &RwsVariableMeta,
        levels_hpa: &[u16],
    ) -> RwResult<Vec<usize>> {
        if levels_hpa.is_empty() {
            return Err(RwStoreError::Format(format!(
                "variable '{}': selected pressure levels must not be empty",
                var.name
            )));
        }
        if levels_hpa.len() > MAX_PRESSURE_LEVELS {
            return Err(RwStoreError::Format(format!(
                "variable '{}': selected pressure level count {} exceeds limit {MAX_PRESSURE_LEVELS}",
                var.name,
                levels_hpa.len()
            )));
        }

        let mut seen = BTreeSet::new();
        for &level_hpa in levels_hpa {
            if !seen.insert(level_hpa) {
                return Err(RwStoreError::Format(format!(
                    "variable '{}': selected pressure level {level_hpa} hPa is duplicated",
                    var.name
                )));
            }
        }

        let mut indices = Vec::new();
        indices.try_reserve_exact(levels_hpa.len()).map_err(|err| {
            RwStoreError::Format(format!(
                "variable '{}': cannot allocate {} selected pressure-level indices: {err}",
                var.name,
                levels_hpa.len()
            ))
        })?;
        for &level_hpa in levels_hpa {
            indices.push(Self::pressure_level_index(var, level_hpa)?);
        }
        Ok(indices)
    }

    fn pressure_level_chunks_3d_for_var(
        &self,
        var: &RwsVariableMeta,
        level_hpa: u16,
    ) -> RwResult<PressureLevelChunks3D> {
        let chunks_y = self.meta.ny.div_ceil(COL_Y);
        let chunks_x = self.meta.nx.div_ceil(COL_X);
        let chunk_count = chunks_y.checked_mul(chunks_x).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}': column chunk count overflows usize",
                var.name
            ))
        })?;
        let range = self.var_ranges.get(&var.id).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}' (id {}) has no chunk index entries",
                var.name, var.id
            ))
        })?;
        if range.len() != chunk_count {
            return Err(RwStoreError::Format(format!(
                "variable '{}' has {} chunk index records, expected {chunk_count}",
                var.name,
                range.len()
            )));
        }
        Ok(PressureLevelChunks3D {
            grid_nx: self.meta.nx,
            grid_ny: self.meta.ny,
            chunks_x,
            chunks_y,
            level_hpa,
            next: 0,
            chunk_count,
        })
    }

    fn selected_pressure_level_chunks_3d_for_var(
        &self,
        var: &RwsVariableMeta,
        levels_hpa: Arc<[u16]>,
    ) -> RwResult<SelectedPressureLevelChunks3D> {
        // The public callers validate a nonempty selection before reaching
        // this helper. Reuse the single-level cardinality/index validation so
        // both seams reject malformed stores identically.
        let chunks = self.pressure_level_chunks_3d_for_var(var, levels_hpa[0])?;
        Ok(SelectedPressureLevelChunks3D {
            grid_nx: chunks.grid_nx,
            grid_ny: chunks.grid_ny,
            chunks_x: chunks.chunks_x,
            chunks_y: chunks.chunks_y,
            levels_hpa,
            next: 0,
            chunk_count: chunks.chunk_count,
        })
    }

    fn pressure_level_chunk_geometry_3d(
        &self,
        var_name: &str,
        level_hpa: u16,
        chunk_y: usize,
        chunk_x: usize,
    ) -> RwResult<PressureLevelChunkGeometry3D> {
        let chunks_y = self.meta.ny.div_ceil(COL_Y);
        let chunks_x = self.meta.nx.div_ceil(COL_X);
        if chunk_y >= chunks_y || chunk_x >= chunks_x {
            return Err(RwStoreError::Format(format!(
                "column chunk ({chunk_y},{chunk_x}) for variable '{var_name}' at {level_hpa} hPa is outside chunk grid {chunks_y}x{chunks_x}"
            )));
        }
        let y0 = chunk_y.checked_mul(COL_Y).ok_or_else(|| {
            RwStoreError::Format(format!(
                "column chunk y offset overflows for variable '{var_name}'"
            ))
        })?;
        let x0 = chunk_x.checked_mul(COL_X).ok_or_else(|| {
            RwStoreError::Format(format!(
                "column chunk x offset overflows for variable '{var_name}'"
            ))
        })?;
        Ok(PressureLevelChunkGeometry3D {
            chunk_y,
            chunk_x,
            x0,
            y0,
            width: (self.meta.nx - x0).min(COL_X),
            height: (self.meta.ny - y0).min(COL_Y),
            level_hpa,
        })
    }

    fn selected_pressure_level_chunk_geometry_3d(
        &self,
        var_name: &str,
        chunk_y: usize,
        chunk_x: usize,
    ) -> RwResult<SelectedPressureLevelChunkGeometry3D> {
        let chunks_y = self.meta.ny.div_ceil(COL_Y);
        let chunks_x = self.meta.nx.div_ceil(COL_X);
        if chunk_y >= chunks_y || chunk_x >= chunks_x {
            return Err(RwStoreError::Format(format!(
                "column chunk ({chunk_y},{chunk_x}) for variable '{var_name}' is outside chunk grid {chunks_y}x{chunks_x}"
            )));
        }
        let y0 = chunk_y.checked_mul(COL_Y).ok_or_else(|| {
            RwStoreError::Format(format!(
                "column chunk y offset overflows for variable '{var_name}'"
            ))
        })?;
        let x0 = chunk_x.checked_mul(COL_X).ok_or_else(|| {
            RwStoreError::Format(format!(
                "column chunk x offset overflows for variable '{var_name}'"
            ))
        })?;
        Ok(SelectedPressureLevelChunkGeometry3D {
            chunk_y,
            chunk_x,
            x0,
            y0,
            width: (self.meta.nx - x0).min(COL_X),
            height: (self.meta.ny - y0).min(COL_Y),
        })
    }

    fn read_pressure_level_chunk_3d_for_var(
        &self,
        var: &RwsVariableMeta,
        level_index: usize,
        geometry: PressureLevelChunkGeometry3D,
    ) -> RwResult<PressureLevelChunk3D> {
        let selected_geometry = SelectedPressureLevelChunkGeometry3D {
            chunk_y: geometry.chunk_y,
            chunk_x: geometry.chunk_x,
            x0: geometry.x0,
            y0: geometry.y0,
            width: geometry.width,
            height: geometry.height,
        };
        let selected_data = self.read_selected_pressure_level_chunk_data_3d_for_var(
            var,
            &[level_index],
            selected_geometry,
        )?;
        let data = match selected_data {
            SelectedPressureLevelChunkData3D::Empty => PressureLevelChunkData3D::Empty,
            SelectedPressureLevelChunkData3D::Constant(value) => {
                PressureLevelChunkData3D::Constant(value)
            }
            SelectedPressureLevelChunkData3D::Dense(values) => {
                PressureLevelChunkData3D::Dense(values)
            }
        };
        Ok(PressureLevelChunk3D { geometry, data })
    }

    fn read_selected_pressure_level_chunk_data_3d_for_var(
        &self,
        var: &RwsVariableMeta,
        selected_level_indices: &[usize],
        geometry: SelectedPressureLevelChunkGeometry3D,
    ) -> RwResult<SelectedPressureLevelChunkData3D> {
        debug_assert!(!selected_level_indices.is_empty());
        let record = self.chunk_record(var, KIND_COLUMN3D, geometry.chunk_y, geometry.chunk_x)?;
        if record.flags & FLAG_EMPTY != 0 {
            return Ok(SelectedPressureLevelChunkData3D::Empty);
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return Ok(SelectedPressureLevelChunkData3D::Constant(record.center));
        }

        let all_level_count = var.levels_hpa.len();
        let all_value_count = geometry
            .cell_count()
            .checked_mul(all_level_count)
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "variable '{}': column chunk value count overflows usize",
                    var.name
                ))
            })?;
        let expected_raw_len = all_value_count
            .checked_mul(size_of::<i16>())
            .ok_or_else(|| {
                RwStoreError::Chunk(format!(
                    "variable '{}': column chunk raw byte count overflows usize",
                    var.name
                ))
            })?;
        let raw_len = usize::try_from(record.raw_len).map_err(|_| {
            RwStoreError::Chunk(format!(
                "variable '{}' column chunk ({},{}): raw_len {} does not fit usize",
                var.name, record.tile_y, record.tile_x, record.raw_len
            ))
        })?;
        if raw_len != expected_raw_len {
            return Err(RwStoreError::Chunk(format!(
                "variable '{}' column chunk ({},{}): raw_len {} does not match {all_value_count} i16 values ({expected_raw_len} bytes)",
                var.name, record.tile_y, record.tile_x, record.raw_len
            )));
        }

        let compressed = self.payload_slice(&var.name, record)?;
        let context = format!(
            "variable '{}' column chunk ({},{})",
            var.name, record.tile_y, record.tile_x
        );
        let raw = decompress_chunk(compressed, raw_len, &context)?;
        let selected_value_count = geometry
            .cell_count()
            .checked_mul(selected_level_indices.len())
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "variable '{}': selected column chunk value count overflows usize",
                    var.name
                ))
            })?;
        let mut selected = try_filled_f32(
            selected_value_count,
            f32::NAN,
            &format!("variable '{}' selected pressure-level chunk", var.name),
        )?;

        // Decode directly from the one decompressed [cell][all_level] i16
        // chunk into a bounded [selected_level][cell] result. This avoids the
        // transient all-level f32 chunk allocation used by whole-volume APIs.
        for (selected_index, &source_level_index) in selected_level_indices.iter().enumerate() {
            debug_assert!(source_level_index < all_level_count);
            let output_plane = &mut selected[selected_index * geometry.cell_count()
                ..(selected_index + 1) * geometry.cell_count()];
            for (cell, value) in output_plane.iter_mut().enumerate() {
                let raw_value_index = cell * all_level_count + source_level_index;
                let raw_byte_index = raw_value_index * size_of::<i16>();
                let q = i16::from_le_bytes([raw[raw_byte_index], raw[raw_byte_index + 1]]);
                *value = if q == MISSING_Q {
                    f32::NAN
                } else if record.flags & FLAG_CONSTANT != 0 {
                    record.center
                } else {
                    record.center + record.scale * f32::from(q)
                };
            }
        }

        Ok(SelectedPressureLevelChunkData3D::Dense(Arc::new(selected)))
    }

    fn tiles_2d_for_var(&self, var: &RwsVariableMeta) -> RwResult<Tiles2D> {
        let tiles_y = self.meta.ny.div_ceil(TILE_Y);
        let tiles_x = self.meta.nx.div_ceil(TILE_X);
        let tile_count = tiles_y.checked_mul(tiles_x).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}': tile count overflows usize",
                var.name
            ))
        })?;
        let range = self.var_ranges.get(&var.id).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}' (id {}) has no chunk index entries",
                var.name, var.id
            ))
        })?;
        if range.len() != tile_count {
            return Err(RwStoreError::Format(format!(
                "variable '{}' has {} chunk index records, expected {tile_count}",
                var.name,
                range.len()
            )));
        }
        Ok(Tiles2D {
            grid_nx: self.meta.nx,
            grid_ny: self.meta.ny,
            tiles_x,
            tiles_y,
            next: 0,
            tile_count,
        })
    }

    fn tile_geometry_2d(
        &self,
        var_name: &str,
        tile_y: usize,
        tile_x: usize,
    ) -> RwResult<TileGeometry2D> {
        let tiles_y = self.meta.ny.div_ceil(TILE_Y);
        let tiles_x = self.meta.nx.div_ceil(TILE_X);
        if tile_y >= tiles_y || tile_x >= tiles_x {
            return Err(RwStoreError::Format(format!(
                "tile ({tile_y},{tile_x}) for variable '{var_name}' is outside tile grid {tiles_y}x{tiles_x}"
            )));
        }
        let y0 = tile_y.checked_mul(TILE_Y).ok_or_else(|| {
            RwStoreError::Format(format!("tile y offset overflows for variable '{var_name}'"))
        })?;
        let x0 = tile_x.checked_mul(TILE_X).ok_or_else(|| {
            RwStoreError::Format(format!("tile x offset overflows for variable '{var_name}'"))
        })?;
        Ok(TileGeometry2D {
            tile_y,
            tile_x,
            x0,
            y0,
            nx: (self.meta.nx - x0).min(TILE_X),
            ny: (self.meta.ny - y0).min(TILE_Y),
        })
    }

    fn read_tile_2d_for_var(
        &self,
        var: &RwsVariableMeta,
        geometry: TileGeometry2D,
    ) -> RwResult<Tile2D> {
        let record = self.chunk_record(var, KIND_TILE2D, geometry.tile_y, geometry.tile_x)?;
        let data = if record.flags & FLAG_EMPTY != 0 {
            TileData2D::Empty
        } else if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            TileData2D::Constant(record.center)
        } else {
            TileData2D::Dense(self.decode_tile_cached(var, record, geometry.cell_count())?)
        };
        Ok(Tile2D { geometry, data })
    }

    /// Find the index record for `var`'s chunk (`ty`, `tx`) of `kind`:
    /// binary search over the variable's pre-computed contiguous record
    /// range, keyed by the same (var_id, kind, tile_y, tile_x) order the
    /// index is sorted in.
    fn chunk_record(
        &self,
        var: &RwsVariableMeta,
        kind: u8,
        ty: usize,
        tx: usize,
    ) -> RwResult<&ChunkRecord> {
        let range = self.var_ranges.get(&var.id).ok_or_else(|| {
            RwStoreError::Format(format!(
                "variable '{}' (id {}) has no chunk index entries",
                var.name, var.id
            ))
        })?;
        let slice = &self.records[range.clone()];
        let key = (var.id, kind, ty as u32, tx as u32);
        let position = slice
            .binary_search_by_key(&key, ChunkRecord::sort_key)
            .map_err(|_| {
                RwStoreError::Format(format!(
                    "missing kind-{kind} chunk record for variable '{}' chunk ({ty},{tx})",
                    var.name
                ))
            })?;
        Ok(&slice[position])
    }

    /// Height/width of tile (`ty`, `tx`) after clipping to the grid edge.
    fn tile_dims(&self, ty: usize, tx: usize) -> (usize, usize) {
        let rows = (self.meta.ny - ty * TILE_Y).min(TILE_Y);
        let cols = (self.meta.nx - tx * TILE_X).min(TILE_X);
        (rows, cols)
    }

    /// Footprint height/width of 3D column chunk (`cy`, `cx`) after clipping
    /// to the grid edge.
    fn col_chunk_dims(&self, cy: usize, cx: usize) -> (usize, usize) {
        let rows = (self.meta.ny - cy * COL_Y).min(COL_Y);
        let cols = (self.meta.nx - cx * COL_X).min(COL_X);
        (rows, cols)
    }

    /// Decode one 3D column chunk to `value_count` f32s in `[y][x][z]`
    /// order. EMPTY and CONSTANT chunks are produced from flags alone — no
    /// payload bytes are read.
    fn decode_column_chunk(
        &self,
        var_name: &str,
        record: &ChunkRecord,
        value_count: usize,
    ) -> RwResult<Vec<f32>> {
        if record.flags & FLAG_EMPTY != 0 {
            return try_filled_f32(
                value_count,
                f32::NAN,
                &format!("variable '{var_name}' empty column chunk"),
            );
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return try_filled_f32(
                value_count,
                record.center,
                &format!("variable '{var_name}' constant column chunk"),
            );
        }
        let compressed = self.payload_slice(var_name, record)?;
        // Stream into a fallibly allocated exact-size buffer and cap the
        // history window requested by a hostile frame header.
        let context = format!(
            "variable '{var_name}' column chunk ({},{})",
            record.tile_y, record.tile_x
        );
        let raw = decompress_chunk(compressed, record.raw_len as usize, &context)?;
        decode_affine_i16(record.flags, record.center, record.scale, &raw, value_count)
    }

    /// Decode one tile to `value_count` f32s. EMPTY and CONSTANT chunks are
    /// produced from flags alone — no payload bytes are read.
    fn decode_tile(
        &self,
        var_name: &str,
        record: &ChunkRecord,
        value_count: usize,
    ) -> RwResult<Vec<f32>> {
        if record.flags & FLAG_EMPTY != 0 {
            return try_filled_f32(
                value_count,
                f32::NAN,
                &format!("variable '{var_name}' empty tile"),
            );
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return try_filled_f32(
                value_count,
                record.center,
                &format!("variable '{var_name}' constant tile"),
            );
        }
        let compressed = self.payload_slice(var_name, record)?;
        let expected_len = value_count.checked_mul(size_of::<f32>()).ok_or_else(|| {
            RwStoreError::Chunk(format!(
                "variable '{var_name}' tile ({},{}): value count {value_count} overflows",
                record.tile_y, record.tile_x
            ))
        })?;
        let raw_len = usize::try_from(record.raw_len).map_err(|_| {
            RwStoreError::Chunk(format!(
                "variable '{var_name}' tile ({},{}): raw_len {} does not fit usize",
                record.tile_y, record.tile_x, record.raw_len
            ))
        })?;
        if raw_len != expected_len {
            return Err(RwStoreError::Chunk(format!(
                "variable '{var_name}' tile ({},{}): raw_len {} does not match {value_count} f32 values ({expected_len} bytes)",
                record.tile_y, record.tile_x, record.raw_len
            )));
        }
        let context = format!(
            "variable '{var_name}' tile ({},{})",
            record.tile_y, record.tile_x
        );

        #[cfg(target_endian = "little")]
        {
            let mut values =
                try_filled_f32(value_count, 0.0, &format!("{context} decoded values"))?;
            let destination = bytemuck::cast_slice_mut(&mut values);
            // Decode directly into the final f32 allocation while retaining
            // the bounded zstd window and exact-output checks used elsewhere.
            decompress_chunk_into(compressed, destination, &context)?;
            Ok(values)
        }

        #[cfg(target_endian = "big")]
        {
            let raw = decompress_chunk(compressed, raw_len, &context)?;
            decode_f32_tile(record.flags, record.center, &raw, value_count)
        }
    }

    fn decode_tile_cached(
        &self,
        var: &RwsVariableMeta,
        record: &ChunkRecord,
        value_count: usize,
    ) -> RwResult<Arc<Vec<f32>>> {
        let key = TileCacheKey {
            kind: record.kind,
            var_id: var.id,
            tile_y: record.tile_y,
            tile_x: record.tile_x,
        };
        if let Some(tile) = self
            .tile_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(key)
        {
            return Ok(tile);
        }

        // Never hold the mutex while decompressing. Concurrent misses may
        // decode the same tile, but insert() converges on one cached Arc.
        let decoded = Arc::new(self.decode_tile(&var.name, record, value_count)?);
        Ok(self
            .tile_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(key, decoded))
    }

    /// Bounds-checked payload slice for `record` — validated against the
    /// file length before any indexing, so corrupt offsets error instead of
    /// panicking.
    fn payload_slice(&self, var_name: &str, record: &ChunkRecord) -> RwResult<&[u8]> {
        let data = self.bytes.as_slice();
        let end = record
            .offset
            .checked_add(u64::from(record.len))
            .ok_or_else(|| {
                RwStoreError::Format(format!(
                    "variable '{var_name}' tile ({},{}): payload range offset {} len {} overflows",
                    record.tile_y, record.tile_x, record.offset, record.len
                ))
            })?;
        if end > data.len() as u64 {
            return Err(RwStoreError::Format(format!(
                "variable '{var_name}' tile ({},{}): payload range {}..{end} exceeds file length {}",
                record.tile_y,
                record.tile_x,
                record.offset,
                data.len()
            )));
        }
        Ok(&data[record.offset as usize..end as usize])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RwStoreError;
    use crate::format::{FLAG_CONSTANT, FLAG_EMPTY, INDEX_RECORD_LEN, TILE_X, TILE_Y};
    use crate::header::RwsHeader;
    use crate::index::ChunkRecord;
    use crate::writer::HourWriter;
    use std::fs;
    use std::path::{Path, PathBuf};

    const NX: usize = 600; // columns -> x tiles of 256, 256, 88
    const NY: usize = 500; // rows    -> y tiles of 256, 244

    #[test]
    fn open_rejects_degenerate_grid() {
        // Regression: nx == 0 reached read_profile_3d's `nx - 1` and panicked.
        // A degenerate grid must be rejected at open().
        let meta = serde_json::json!({
            "schema": crate::format::SCHEMA_HOUR,
            "model": "test", "run": "20260608_00z", "forecast_hour": 0,
            "nx": 0, "ny": 5, "grid_hash": "none", "variables": [],
            "chunking": {"tile_y": 256, "tile_x": 256, "col_y": 16, "col_x": 16},
            "writer": {"name": "test", "version": "0", "build": "dev"}
        });
        let meta_bytes = serde_json::to_vec(&meta).unwrap();
        let header = crate::header::RwsHeader::for_layout(meta_bytes.len() as u32, 0);
        let mut bytes = header.pack().to_vec();
        bytes.extend_from_slice(&meta_bytes);

        let dir = test_dir("degenerate-grid");
        let path = dir.join("f000.rws");
        fs::write(&path, &bytes).unwrap();
        let err = HourReader::open(&path).unwrap_err();
        match err {
            RwStoreError::Meta(msg) => {
                assert!(msg.contains("degenerate"), "unexpected message: {msg}")
            }
            other => panic!("expected Meta error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hour_file_length_cap_rejects_before_mapping() {
        let header = RwsHeader::for_layout(0, 0);
        let err = validate_header_bounds(&header, MAX_HOUR_FILE_LEN + 1).unwrap_err();
        match err {
            RwStoreError::Format(message) => {
                assert!(message.contains("limit"), "unexpected message: {message}")
            }
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    #[test]
    fn index_count_cap_rejects_before_record_allocation() {
        let header = RwsHeader::for_layout(0, MAX_INDEX_RECORDS + 1);
        let err = validate_header_bounds(&header, header.payload_offset).unwrap_err();
        match err {
            RwStoreError::Format(message) => assert!(
                message.contains("index") && message.contains("limit"),
                "unexpected message: {message}"
            ),
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    #[test]
    fn ram_fallback_cap_rejects_before_allocation() {
        let dir = test_dir("oversized-ram-fallback");
        let path = dir.join("oversized-fallback.rws");
        let mut file = fs::File::create(&path).unwrap();
        let file_len = MAX_RAM_FALLBACK_FILE_LEN + 1;

        match HourReader::read_file_to_ram(&mut file, file_len) {
            Err(RwStoreError::Format(message)) => assert!(
                message.contains("RAM fallback limit"),
                "unexpected message: {message}"
            ),
            Err(other) => panic!("expected Format error, got {other:?}"),
            Ok(_) => panic!("oversized RAM fallback unexpectedly succeeded"),
        }
        drop(file);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_buffer_allocation_failure_is_reported() {
        let err = try_zeroed_bytes(usize::MAX, "test buffer").unwrap_err();
        match err {
            RwStoreError::Format(message) => assert!(
                message.contains("cannot allocate"),
                "unexpected message: {message}"
            ),
            other => panic!("expected Format error, got {other:?}"),
        }
    }

    fn test_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rw-store-reader-{}-{}", std::process::id(), name));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Var A "temp_2m": smooth field with tile (0,0) all-NaN (EMPTY),
    /// tile (0,1) all 42.0 (CONSTANT) — both full 256x256 aligned tiles —
    /// plus scattered NaN inside the dense tile (1,0).
    fn grid_a() -> Vec<f32> {
        let mut values: Vec<f32> = (0..NY)
            .flat_map(|y| (0..NX).map(move |x| 0.01 * x as f32 + 0.02 * y as f32))
            .collect();
        for y in 0..TILE_Y {
            for x in 0..TILE_X {
                values[y * NX + x] = f32::NAN;
            }
            for x in TILE_X..2 * TILE_X {
                values[y * NX + x] = 42.0;
            }
        }
        // Scattered NaN inside dense tile (1,0): rows 256.., cols 0..256.
        for k in 0..40usize {
            let y = 258 + k * 6; // stays < 500
            let x = (k * 37) % TILE_X;
            values[y * NX + x] = f32::NAN;
        }
        values
    }

    /// Var B "dewpoint_2m": varying everywhere; every tile encodes dense.
    fn grid_b() -> Vec<f32> {
        (0..NY)
            .flat_map(|y| (0..NX).map(move |x| 100.0 + 0.5 * x as f32 - 0.25 * y as f32))
            .collect()
    }

    fn write_sample(path: &Path) {
        let mut writer = HourWriter::new(
            "hrrr",
            "20260609_12z",
            6,
            NX,
            NY,
            "gridhash-test",
            "test-build",
        );
        writer
            .add_surface2d(
                "temp_2m",
                "K",
                serde_json::json!({"grib_short_name": "TMP"}),
                &grid_a(),
            )
            .unwrap();
        writer
            .add_surface2d(
                "dewpoint_2m",
                "K",
                serde_json::json!({"grib_short_name": "DPT"}),
                &grid_b(),
            )
            .unwrap();
        writer.finish(path).unwrap();
    }

    fn crop(full: &[f32], nx: usize, x0: usize, y0: usize, x1: usize, y1: usize) -> Vec<f32> {
        let mut out = Vec::with_capacity((y1 - y0) * (x1 - x0));
        for y in y0..y1 {
            out.extend_from_slice(&full[y * nx + x0..y * nx + x1]);
        }
        out
    }

    /// NaN-safe bit-exact slice comparison.
    fn assert_bits_eq(actual: &[f32], expected: &[f32], context: &str) {
        assert_eq!(actual.len(), expected.len(), "{context}: length mismatch");
        for (i, (a, e)) in actual.iter().zip(expected.iter()).enumerate() {
            assert_eq!(
                a.to_bits(),
                e.to_bits(),
                "{context}: value mismatch at index {i} (actual {a}, expected {e})"
            );
        }
    }

    /// Parse the on-disk chunk index of `bytes` into records.
    fn parse_records(bytes: &[u8]) -> (RwsHeader, Vec<ChunkRecord>) {
        let header = RwsHeader::parse(bytes).unwrap();
        let records = (0..header.index_count as usize)
            .map(|i| {
                let start = header.index_offset as usize + i * INDEX_RECORD_LEN;
                ChunkRecord::unpack(&bytes[start..start + INDEX_RECORD_LEN]).unwrap()
            })
            .collect();
        (header, records)
    }

    /// Flip the first four payload bytes (the zstd frame magic) of `record`
    /// so any attempt to decompress that chunk must fail.
    fn corrupt_payload(bytes: &mut [u8], record: &ChunkRecord) {
        assert!(record.len >= 4, "need a dense payload to corrupt");
        let off = record.offset as usize;
        for byte in &mut bytes[off..off + 4] {
            *byte ^= 0xFF;
        }
    }

    #[test]
    fn read_full_round_trips_exactly() {
        let dir = test_dir("full-round-trip");
        let path = dir.join("hour.rws");
        write_sample(&path);

        let reader = HourReader::open(&path).unwrap();
        assert_eq!(reader.meta().nx, NX);
        assert_eq!(reader.meta().ny, NY);
        assert_eq!(reader.variable("temp_2m").unwrap().id, 0);
        assert_eq!(reader.variable("dewpoint_2m").unwrap().id, 1);

        let full_a = reader.read_full_2d("temp_2m").unwrap();
        assert_bits_eq(&full_a, &grid_a(), "temp_2m full read");
        let full_b = reader.read_full_2d("dewpoint_2m").unwrap();
        assert_bits_eq(&full_b, &grid_b(), "dewpoint_2m full read");

        // A larger grid with 12 tiles (> 8) exercises the rayon-parallel
        // decode path; result must still be bit-exact and deterministic.
        let (big_nx, big_ny) = (1024usize, 600usize); // 4 x-tiles * 3 y-tiles
        let big: Vec<f32> = (0..big_ny)
            .flat_map(|y| {
                (0..big_nx).map(move |x| {
                    if (x + y) % 991 == 0 {
                        f32::NAN
                    } else {
                        0.125 * x as f32 - 0.375 * y as f32
                    }
                })
            })
            .collect();
        let big_path = dir.join("big.rws");
        let mut writer = HourWriter::new("hrrr", "run", 0, big_nx, big_ny, "hash", "build");
        writer
            .add_surface2d("gust_10m", "m s-1", serde_json::Value::Null, &big)
            .unwrap();
        writer.finish(&big_path).unwrap();
        let big_reader = HourReader::open(&big_path).unwrap();
        let big_full = big_reader.read_full_2d("gust_10m").unwrap();
        assert_bits_eq(&big_full, &big, "gust_10m parallel full read");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_api_enumerates_edge_geometry_and_preserves_full_read_bits() {
        let dir = test_dir("tile-api-layout");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open(&path).unwrap();

        let tiles = reader.tiles_2d("temp_2m").unwrap();
        assert_eq!((tiles.tiles_y(), tiles.tiles_x(), tiles.len()), (2, 3, 6));
        let geometries: Vec<_> = tiles.collect();
        assert_eq!(
            geometries
                .iter()
                .map(|geometry| (
                    geometry.tile_y(),
                    geometry.tile_x(),
                    geometry.y0(),
                    geometry.x0(),
                    geometry.ny(),
                    geometry.nx(),
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, 0, 0, 0, 256, 256),
                (0, 1, 0, 256, 256, 256),
                (0, 2, 0, 512, 256, 88),
                (1, 0, 256, 0, 244, 256),
                (1, 1, 256, 256, 244, 256),
                (1, 2, 256, 512, 244, 88),
            ]
        );

        let empty = reader.read_tile_2d("temp_2m", 0, 0).unwrap();
        assert!(matches!(empty.data(), TileData2D::Empty));
        assert!(empty.get(0, 0).unwrap().is_nan());
        assert!(empty.get(TILE_Y, 0).is_none());

        let constant = reader.read_tile_2d("temp_2m", 0, 1).unwrap();
        match constant.data() {
            TileData2D::Constant(value) => assert_eq!(value.to_bits(), 42.0f32.to_bits()),
            other => panic!("expected constant tile, got {other:?}"),
        }

        let edge = reader.read_tile_2d("temp_2m", 1, 2).unwrap();
        assert_eq!(
            (
                edge.geometry().x0(),
                edge.geometry().y0(),
                edge.geometry().nx(),
                edge.geometry().ny(),
            ),
            (512, 256, 88, 244)
        );
        match edge.data() {
            TileData2D::Dense(values) => assert_eq!(values.len(), edge.cell_count()),
            other => panic!("expected dense edge tile, got {other:?}"),
        }

        let full = reader.read_full_2d("temp_2m").unwrap();
        let mut assembled = vec![0.0; NX * NY];
        reader
            .visit_tiles_2d("temp_2m", |tile| {
                let geometry = tile.geometry();
                for row in 0..geometry.ny() {
                    for column in 0..geometry.nx() {
                        assembled[(geometry.y0() + row) * NX + geometry.x0() + column] =
                            tile.get(row, column).unwrap();
                    }
                }
                Ok(())
            })
            .unwrap();
        assert_bits_eq(&assembled, &full, "tile visitor versus full read");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_visitor_streams_within_the_configured_cache_bound() {
        let dir = test_dir("tile-api-streaming");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let tile_bytes = TILE_X * TILE_Y * size_of::<f32>();
        let reader = HourReader::open_with_tile_cache_bytes(&path, tile_bytes).unwrap();

        let mut visited_tiles = 0usize;
        let mut visited_cells = 0usize;
        let mut max_current_cells = 0usize;
        let mut checksum = 0u64;
        reader
            .visit_tiles_2d("dewpoint_2m", |tile| {
                visited_tiles += 1;
                visited_cells += tile.cell_count();
                max_current_cells = max_current_cells.max(tile.cell_count());
                match tile.data() {
                    TileData2D::Dense(values) => {
                        assert_eq!(values.len(), tile.cell_count());
                        for value in values.iter() {
                            checksum = checksum.wrapping_add(u64::from(value.to_bits()));
                        }
                    }
                    other => panic!("expected dense tile, got {other:?}"),
                }
                let cache = reader.tile_cache_stats();
                assert!(cache.bytes <= cache.capacity_bytes, "cache: {cache:?}");
                Ok(())
            })
            .unwrap();

        assert_eq!(visited_tiles, 6);
        assert_eq!(visited_cells, NX * NY);
        assert_eq!(max_current_cells, TILE_X * TILE_Y);
        assert_ne!(checksum, 0);
        let cache = reader.tile_cache_stats();
        assert_eq!((cache.misses, cache.insertions), (6, 6));
        assert!(cache.bytes <= tile_bytes, "cache: {cache:?}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_api_rejects_bad_indices_and_kinds_and_stops_on_visitor_error() {
        let dir = test_dir("tile-api-errors");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open(&path).unwrap();

        for (tile_y, tile_x) in [(2, 0), (0, 3), (usize::MAX, usize::MAX)] {
            let err = reader.read_tile_2d("temp_2m", tile_y, tile_x).unwrap_err();
            match err {
                RwStoreError::Format(message) => assert!(
                    message.contains("outside tile grid"),
                    "unexpected message: {message}"
                ),
                other => panic!("expected Format error, got {other:?}"),
            }
        }

        let unknown = reader.tiles_2d("not_present").unwrap_err();
        assert!(
            matches!(&unknown, RwStoreError::UnknownVariable(name) if name == "not_present"),
            "expected UnknownVariable, got {unknown:?}"
        );

        let mut visits = 0usize;
        let stopped = reader
            .visit_tiles_2d("temp_2m", |_| {
                visits += 1;
                Err(RwStoreError::Chunk("visitor stop".to_string()))
            })
            .unwrap_err();
        assert_eq!(visits, 1);
        assert!(
            matches!(&stopped, RwStoreError::Chunk(message) if message == "visitor stop"),
            "unexpected visitor error: {stopped:?}"
        );

        let pressure_path = dir.join("pressure.rws");
        let pressure_plane = vec![280.0; 4];
        let pressure_planes: [&[f32]; 1] = [&pressure_plane];
        let mut writer = HourWriter::new("hrrr", "run", 0, 2, 2, "grid", "build");
        writer
            .add_pressure3d(
                "temperature",
                "K",
                serde_json::Value::Null,
                &[1000],
                &pressure_planes,
            )
            .unwrap();
        writer.finish(&pressure_path).unwrap();
        let pressure_reader = HourReader::open(&pressure_path).unwrap();

        let enumerate_error = pressure_reader.tiles_2d("temperature").unwrap_err();
        let read_error = pressure_reader
            .read_tile_2d("temperature", 0, 0)
            .unwrap_err();
        let visit_error = pressure_reader
            .visit_tiles_2d("temperature", |_| Ok(()))
            .unwrap_err();
        for err in [enumerate_error, read_error, visit_error] {
            match err {
                RwStoreError::Format(message) => assert!(
                    message.contains("pressure3d") && message.contains("surface2d"),
                    "unexpected message: {message}"
                ),
                other => panic!("expected Format error, got {other:?}"),
            }
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn windowed_read_equals_full_read_crop() {
        let dir = test_dir("window-crop");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open(&path).unwrap();

        // (request, expected clamped (x0, y0, nx, ny))
        type Rect = (usize, usize, usize, usize);
        let cases: &[(Rect, Rect)] = &[
            ((10, 10, 50, 50), (10, 10, 40, 40)),           // tile-interior
            ((200, 200, 400, 460), (200, 200, 200, 260)),   // straddles 4 tiles
            ((500, 400, 9999, 9999), (500, 400, 100, 100)), // edge-clamped
            ((599, 499, 600, 500), (599, 499, 1, 1)),       // single cell
            ((0, 0, 600, 500), (0, 0, 600, 500)),           // full grid
        ];

        for name in ["temp_2m", "dewpoint_2m"] {
            let full = reader.read_full_2d(name).unwrap();
            for &((x0, y0, x1, y1), (ex0, ey0, enx, eny)) in cases {
                let window = reader.read_window_2d(name, x0, y0, x1, y1).unwrap();
                let context = format!("{name} window ({x0},{y0},{x1},{y1})");
                assert_eq!(
                    (window.x0, window.y0, window.nx, window.ny),
                    (ex0, ey0, enx, eny),
                    "{context}: clamped dims"
                );
                let expected = crop(&full, NX, ex0, ey0, ex0 + enx, ey0 + eny);
                assert_bits_eq(&window.values, &expected, &context);
            }
        }

        // Empty after clamping -> Format error, not a panic.
        for &(x0, y0, x1, y1) in &[
            (50usize, 50usize, 50usize, 90usize),
            (700, 0, 9999, 10),
            (30, 20, 10, 40),
        ] {
            let err = reader
                .read_window_2d("temp_2m", x0, y0, x1, y1)
                .unwrap_err();
            assert!(
                matches!(err, RwStoreError::Format(_)),
                "window ({x0},{y0},{x1},{y1}): expected Format error, got {err:?}"
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_decodes_only_intersecting_tiles() {
        let dir = test_dir("lazy-tiles");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let pristine = HourReader::open(&path).unwrap();
        let full_b = pristine.read_full_2d("dewpoint_2m").unwrap();

        // Corrupt the payload of var B's LAST dense tile — tile (1,2), far
        // from the (10,10,50,50) window which lives entirely in tile (0,0).
        let mut bytes = fs::read(&path).unwrap();
        let (_, records) = parse_records(&bytes);
        let target = records
            .iter()
            .rev()
            .find(|r| r.var_id == 1 && r.flags & (FLAG_EMPTY | FLAG_CONSTANT) == 0)
            .expect("var B must have dense tiles");
        assert_eq!(
            (target.tile_y, target.tile_x),
            (1, 2),
            "last dense tile of var B"
        );
        corrupt_payload(&mut bytes, target);
        let corrupted_path = dir.join("corrupted.rws");
        fs::write(&corrupted_path, &bytes).unwrap();

        let reader = HourReader::open(&corrupted_path).unwrap();
        // The window read never touches tile (1,2), so it must still succeed
        // and match the pristine data...
        let window = reader
            .read_window_2d("dewpoint_2m", 10, 10, 50, 50)
            .unwrap();
        assert_bits_eq(
            &window.values,
            &crop(&full_b, NX, 10, 10, 50, 50),
            "window on corrupted file",
        );
        // ...while a full read of the same variable must hit the corrupt
        // tile and fail. Together this proves untouched tiles are never
        // decompressed by the windowed read.
        let err = reader.read_full_2d("dewpoint_2m").unwrap_err();
        assert!(
            matches!(err, RwStoreError::Chunk(_)),
            "expected Chunk error from corrupt tile, got {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_and_constant_tiles_read_without_payload() {
        let dir = test_dir("empty-constant");
        let path = dir.join("hour.rws");
        write_sample(&path);

        // Writer stores EMPTY/CONSTANT chunks with len == 0 — no payload
        // bytes exist for them on disk at all.
        let bytes = fs::read(&path).unwrap();
        let (_, records) = parse_records(&bytes);
        for record in &records {
            if record.flags & (FLAG_EMPTY | FLAG_CONSTANT) != 0 {
                assert_eq!(record.len, 0, "EMPTY/CONSTANT chunks carry no payload");
            }
        }

        let reader = HourReader::open(&path).unwrap();

        // Window exactly covering the EMPTY tile (0,0) -> all NaN.
        let empty = reader.read_window_2d("temp_2m", 0, 0, 256, 256).unwrap();
        assert_eq!((empty.nx, empty.ny), (256, 256));
        assert!(
            empty.values.iter().all(|v| v.is_nan()),
            "EMPTY tile window must be all NaN"
        );

        // Window exactly covering the CONSTANT tile (0,1) -> all 42.0.
        let constant = reader.read_window_2d("temp_2m", 256, 0, 512, 256).unwrap();
        assert_eq!((constant.nx, constant.ny), (256, 256));
        assert!(
            constant
                .values
                .iter()
                .all(|v| v.to_bits() == 42.0f32.to_bits()),
            "CONSTANT tile window must be all center (42.0)"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn source_identity_detects_same_path_atomic_replacement() {
        let dir = test_dir("source-identity-replacement");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let bytes = fs::read(&path).unwrap();
        let reader = HourReader::open(&path).unwrap();

        assert!(reader.source_matches_path(&path).unwrap());
        crate::atomic::atomic_write_bytes(&path, &bytes).unwrap();
        assert!(
            !reader.source_matches_path(&path).unwrap(),
            "same pathname must not hide a replacement file object"
        );

        let replacement = HourReader::open(&path).unwrap();
        assert!(replacement.source_matches_path(&path).unwrap());
        assert_eq!(
            replacement
                .read_point_2d("dewpoint_2m", 300, 300)
                .unwrap()
                .to_bits(),
            grid_b()[300 * NX + 300].to_bits()
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rejects_truncated_file() {
        let dir = test_dir("truncated");
        let path = dir.join("hour.rws");
        write_sample(&path);

        let bytes = fs::read(&path).unwrap();
        let header = RwsHeader::parse(&bytes).unwrap();
        // Cut mid-index: keep the header, meta JSON, and half a record.
        let cut = header.index_offset as usize + INDEX_RECORD_LEN / 2;
        let truncated_path = dir.join("truncated.rws");
        fs::write(&truncated_path, &bytes[..cut]).unwrap();

        let err = HourReader::open(&truncated_path).unwrap_err();
        assert!(
            matches!(err, RwStoreError::Format(_) | RwStoreError::Io(_)),
            "expected Format/Io error for truncated file, got {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rejects_corrupt_index_order() {
        let dir = test_dir("index-order");
        let path = dir.join("hour.rws");
        write_sample(&path);

        // Swap the first two 64-byte index records on disk.
        let mut bytes = fs::read(&path).unwrap();
        let header = RwsHeader::parse(&bytes).unwrap();
        let start = header.index_offset as usize;
        let (first, second) = (
            bytes[start..start + INDEX_RECORD_LEN].to_vec(),
            bytes[start + INDEX_RECORD_LEN..start + 2 * INDEX_RECORD_LEN].to_vec(),
        );
        bytes[start..start + INDEX_RECORD_LEN].copy_from_slice(&second);
        bytes[start + INDEX_RECORD_LEN..start + 2 * INDEX_RECORD_LEN].copy_from_slice(&first);
        let swapped_path = dir.join("swapped.rws");
        fs::write(&swapped_path, &bytes).unwrap();

        let err = HourReader::open(&swapped_path).unwrap_err();
        match err {
            RwStoreError::Format(msg) => assert!(
                msg.contains("sort"),
                "Format error should mention sort order, got: {msg}"
            ),
            other => panic!("expected Format error, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_rejects_hostile_chunk_raw_len_before_decode() {
        let dir = test_dir("hostile-raw-len");
        let path = dir.join("hour.rws");
        write_sample(&path);

        let mut bytes = fs::read(&path).unwrap();
        let (header, records) = parse_records(&bytes);
        let record_index = records
            .iter()
            .position(|record| record.flags & (FLAG_EMPTY | FLAG_CONSTANT) == 0)
            .expect("sample contains a dense chunk");
        let raw_len_offset = header.index_offset as usize + record_index * INDEX_RECORD_LEN + 24;
        bytes[raw_len_offset..raw_len_offset + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        let hostile_path = dir.join("hostile.rws");
        fs::write(&hostile_path, &bytes).unwrap();

        let err = HourReader::open(&hostile_path).unwrap_err();
        match err {
            RwStoreError::Format(message) => {
                assert!(message.contains("raw_len"), "unexpected message: {message}")
            }
            other => panic!("expected Format error, got {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_payload_errors_cleanly() {
        let dir = test_dir("corrupt-payload");
        let path = dir.join("hour.rws");
        write_sample(&path);

        // Corrupt var B tile (0,0), then read a window inside that tile.
        let mut bytes = fs::read(&path).unwrap();
        let (_, records) = parse_records(&bytes);
        let target = records
            .iter()
            .find(|r| r.var_id == 1 && r.tile_y == 0 && r.tile_x == 0)
            .expect("var B tile (0,0)");
        corrupt_payload(&mut bytes, target);
        let corrupted_path = dir.join("corrupted.rws");
        fs::write(&corrupted_path, &bytes).unwrap();

        let reader = HourReader::open(&corrupted_path).unwrap();
        let err = reader
            .read_window_2d("dewpoint_2m", 0, 0, 50, 50)
            .unwrap_err();
        assert!(
            matches!(err, RwStoreError::Chunk(_)),
            "expected Chunk error for corrupt payload, got {err:?}"
        );
        // The other variable is untouched and must still read fine.
        let ok = reader.read_window_2d("temp_2m", 0, 0, 50, 50).unwrap();
        assert!(
            ok.values.iter().all(|v| v.is_nan()),
            "temp_2m tile (0,0) is EMPTY"
        );
        // A failed decode is never inserted and cannot poison later reads.
        let recovered = reader.read_point_2d("dewpoint_2m", 300, 300).unwrap();
        assert_eq!(recovered.to_bits(), grid_b()[300 * NX + 300].to_bits());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn point_reads_are_bit_exact_and_share_the_window_cache() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<HourReader>();

        let dir = test_dir("point-cache");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open_with_tile_cache_bytes(&path, 1024 * 1024).unwrap();
        let expected = grid_b();
        let (x, y) = (300usize, 300usize);

        let point = reader.read_point_2d("dewpoint_2m", x, y).unwrap();
        assert_eq!(point.to_bits(), expected[y * NX + x].to_bits());
        let after_miss = reader.tile_cache_stats();
        assert_eq!((after_miss.hits, after_miss.misses), (0, 1));
        assert_eq!(after_miss.entries, 1);

        let repeated = reader.read_point_2d("dewpoint_2m", x, y).unwrap();
        assert_eq!(repeated.to_bits(), point.to_bits());
        let one_cell = reader
            .read_window_2d("dewpoint_2m", x, y, x + 1, y + 1)
            .unwrap();
        assert_eq!(one_cell.values[0].to_bits(), point.to_bits());
        let hot = reader.tile_cache_stats();
        assert_eq!((hot.hits, hot.misses), (2, 1));

        assert!(reader.read_point_2d("temp_2m", 10, 10).unwrap().is_nan());
        assert_eq!(
            reader.read_point_2d("temp_2m", 300, 10).unwrap().to_bits(),
            42.0f32.to_bits()
        );
        let edge = reader.read_point_2d("dewpoint_2m", NX - 1, NY - 1).unwrap();
        assert_eq!(edge.to_bits(), expected[(NY - 1) * NX + NX - 1].to_bits());

        assert!(matches!(
            reader.read_point_2d("dewpoint_2m", NX, 0),
            Err(RwStoreError::Format(_))
        ));
        assert!(matches!(
            reader.read_point_2d("no_such_var", 0, 0),
            Err(RwStoreError::UnknownVariable(_))
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dense_special_f32_values_remain_bit_exact() {
        let dir = test_dir("special-f32-bits");
        let path = dir.join("hour.rws");
        let expected = vec![-0.0, f32::from_bits(1), f32::from_bits(0x7fc0_1234), 7.25];
        let mut writer = HourWriter::new(
            "hrrr",
            "2026-06-09T12:00:00Z",
            0,
            2,
            2,
            "gridhash-special-bits",
            "test-build",
        );
        writer
            .add_surface2d("special", "1", serde_json::Value::Null, &expected)
            .unwrap();
        writer.finish(&path).unwrap();

        let reader = HourReader::open(&path).unwrap();
        let full = reader.read_full_2d("special").unwrap();
        assert_bits_eq(&full, &expected, "special f32 full read");
        for (index, expected_value) in expected.iter().enumerate() {
            let actual = reader
                .read_point_2d("special", index % 2, index / 2)
                .unwrap();
            assert_eq!(actual.to_bits(), expected_value.to_bits());
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tile_cache_key_includes_variable_and_lru_stays_bounded() {
        let dir = test_dir("cache-key-lru");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let tile_bytes = TILE_X * TILE_Y * size_of::<f32>();
        let reader = HourReader::open_with_tile_cache_bytes(&path, tile_bytes).unwrap();

        let a = reader.read_point_2d("temp_2m", 10, 300).unwrap();
        let b = reader.read_point_2d("dewpoint_2m", 10, 300).unwrap();
        assert_eq!(a.to_bits(), grid_a()[300 * NX + 10].to_bits());
        assert_eq!(b.to_bits(), grid_b()[300 * NX + 10].to_bits());

        let _ = reader.read_point_2d("dewpoint_2m", 10, 10).unwrap();
        let _ = reader.read_point_2d("dewpoint_2m", 300, 10).unwrap();
        let _ = reader.read_point_2d("dewpoint_2m", 10, 10).unwrap();
        let stats = reader.tile_cache_stats();
        assert!(stats.evictions >= 3, "stats: {stats:?}");
        assert_eq!(stats.entries, 1);
        assert!(stats.bytes <= stats.capacity_bytes);

        reader.clear_tile_cache();
        assert_eq!(
            reader.tile_cache_stats(),
            TileCacheStats {
                capacity_bytes: tile_bytes,
                ..TileCacheStats::default()
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn disabled_cache_redecodes_and_shared_reader_is_thread_safe() {
        let dir = test_dir("cache-concurrency");
        let path = dir.join("hour.rws");
        write_sample(&path);

        let uncached = HourReader::open_with_tile_cache_bytes(&path, 0).unwrap();
        let _ = uncached.read_point_2d("dewpoint_2m", 300, 300).unwrap();
        let _ = uncached.read_point_2d("dewpoint_2m", 300, 300).unwrap();
        let stats = uncached.tile_cache_stats();
        assert_eq!((stats.hits, stats.misses, stats.entries), (0, 2, 0));

        let reader = Arc::new(HourReader::open_with_tile_cache_bytes(&path, 1024 * 1024).unwrap());
        let expected = grid_b()[300 * NX + 300].to_bits();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let reader = Arc::clone(&reader);
                scope.spawn(move || {
                    for _ in 0..500 {
                        assert_eq!(
                            reader
                                .read_point_2d("dewpoint_2m", 300, 300)
                                .unwrap()
                                .to_bits(),
                            expected
                        );
                    }
                });
            }
        });
        let stats = reader.tile_cache_stats();
        assert_eq!(stats.hits + stats.misses, 8 * 500);
        assert_eq!(stats.entries, 1);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn full_reads_bypass_window_cache() {
        let dir = test_dir("full-read-cache-bypass");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open_with_tile_cache_bytes(&path, 1024 * 1024).unwrap();

        let full = reader.read_full_2d("dewpoint_2m").unwrap();
        assert_bits_eq(&full, &grid_b(), "uncached full read");
        assert_eq!(
            reader.tile_cache_stats(),
            TileCacheStats {
                capacity_bytes: 1024 * 1024,
                ..TileCacheStats::default()
            }
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_variable_errors() {
        let dir = test_dir("unknown-var");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open(&path).unwrap();

        assert!(reader.variable("no_such_var").is_none());
        let err = reader.read_full_2d("no_such_var").unwrap_err();
        assert!(
            matches!(&err, RwStoreError::UnknownVariable(name) if name == "no_such_var"),
            "expected UnknownVariable, got {err:?}"
        );
        let err = reader
            .read_window_2d("no_such_var", 0, 0, 10, 10)
            .unwrap_err();
        assert!(
            matches!(&err, RwStoreError::UnknownVariable(name) if name == "no_such_var"),
            "expected UnknownVariable, got {err:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_gate_works() {
        let dir = test_dir("version-gate");
        let path = dir.join("hour.rws");
        write_sample(&path);

        let mut bytes = fs::read(&path).unwrap();
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        let v2_path = dir.join("v2.rws");
        fs::write(&v2_path, &bytes).unwrap();

        let err = HourReader::open(&v2_path).unwrap_err();
        match err {
            RwStoreError::UnsupportedVersion { found, supported } => {
                assert_eq!(found, 2);
                assert_eq!(supported, &[1]);
            }
            other => panic!("expected UnsupportedVersion, got {other:?}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }
}

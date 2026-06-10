//! Hour-file reader: mmap-backed (RAM fallback) access to one rw-store hour
//! file with true windowed 2D reads — only the tiles intersecting a requested
//! window are ever decompressed.

use std::collections::BTreeMap;
use std::fs::File;
use std::ops::Range;
use std::path::Path;

use memmap2::Mmap;
use rayon::prelude::*;

use crate::codec::{decode_affine_i16, decode_f32_tile};
use crate::error::{RwResult, RwStoreError};
use crate::format::{
    COL_X, COL_Y, FLAG_CONSTANT, FLAG_EMPTY, HEADER_LEN, INDEX_RECORD_LEN, KIND_COLUMN3D,
    KIND_TILE2D, RwsHourMeta, RwsVariableMeta, SCHEMA_HOUR, TILE_X, TILE_Y,
};
use crate::header::RwsHeader;
use crate::index::ChunkRecord;

/// Above this many tiles, `read_full_2d` decodes them in parallel.
const PARALLEL_TILE_THRESHOLD: usize = 8;

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

/// Read-only handle to one rw-store hour file.
///
/// Debug is implemented manually: deriving it would dump the entire mapped
/// file contents into panic messages.
pub struct HourReader {
    bytes: FileBytes,
    meta: RwsHourMeta,
    records: Vec<ChunkRecord>,
    /// Per-variable contiguous slice of `records`, built once at open so
    /// per-tile lookups binary-search only that variable's records.
    var_ranges: BTreeMap<u16, Range<usize>>,
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
        let bytes = Self::open_bytes(path)?;
        let data = bytes.as_slice();

        let header = RwsHeader::parse(data)?;

        let meta_end = HEADER_LEN + header.meta_len as usize;
        if data.len() < meta_end {
            return Err(RwStoreError::Format(format!(
                "file truncated inside meta JSON: need {meta_end} bytes, have {}",
                data.len()
            )));
        }
        let meta: RwsHourMeta = serde_json::from_slice(&data[HEADER_LEN..meta_end])
            .map_err(|err| RwStoreError::Meta(format!("hour meta JSON: {err}")))?;
        if meta.schema != SCHEMA_HOUR {
            return Err(RwStoreError::Meta(format!(
                "unexpected schema '{}' (expected '{SCHEMA_HOUR}')",
                meta.schema
            )));
        }
        if meta.nx == 0 || meta.ny == 0 {
            return Err(RwStoreError::Meta(format!(
                "degenerate grid {}x{} (nx and ny must be nonzero)",
                meta.nx, meta.ny
            )));
        }

        if (data.len() as u64) < header.payload_offset {
            return Err(RwStoreError::Format(format!(
                "file truncated inside chunk index: need {} bytes, have {}",
                header.payload_offset,
                data.len()
            )));
        }
        let index_count = header.index_count as usize;
        let mut records = Vec::with_capacity(index_count);
        for i in 0..index_count {
            let start = header.index_offset as usize + i * INDEX_RECORD_LEN;
            records.push(ChunkRecord::unpack(&data[start..start + INDEX_RECORD_LEN])?);
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
        let mut run_start = 0usize;
        for end in 1..=records.len() {
            if end == records.len() || records[end].var_id != records[run_start].var_id {
                var_ranges.insert(records[run_start].var_id, run_start..end);
                run_start = end;
            }
        }

        Ok(Self {
            bytes,
            meta,
            records,
            var_ranges,
        })
    }

    fn open_bytes(path: &Path) -> RwResult<FileBytes> {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len > 0 && usize::try_from(len).is_ok() {
            // SAFETY: the map is read-only over a read-only handle and every
            // consumer accesses it through bounds-checked ranges.
            if let Ok(mmap) = unsafe { Mmap::map(&file) } {
                return Ok(FileBytes::Mmap(mmap));
            }
        }
        drop(file);
        Ok(FileBytes::Ram(std::fs::read(path)?))
    }

    /// Hour-level metadata parsed from the file.
    pub fn meta(&self) -> &RwsHourMeta {
        &self.meta
    }

    /// Metadata for the variable named `name`, if present.
    pub fn variable(&self, name: &str) -> Option<&RwsVariableMeta> {
        self.meta.variables.iter().find(|var| var.name == name)
    }

    /// Read the full `ny * nx` row-major field for `name`; positions inside
    /// EMPTY tiles come back as NaN. Tiles decode in parallel when the
    /// variable has more than [`PARALLEL_TILE_THRESHOLD`] of them; results
    /// are placed serially so output is deterministic either way.
    pub fn read_full_2d(&self, name: &str) -> RwResult<Vec<f32>> {
        let var = self.lookup(name)?;
        let (nx, ny) = (self.meta.nx, self.meta.ny);
        let tiles_y = ny.div_ceil(TILE_Y);
        let tiles_x = nx.div_ceil(TILE_X);

        // Resolve all records up front so index problems surface before any
        // decompression work starts.
        let jobs = (0..tiles_y)
            .flat_map(|ty| (0..tiles_x).map(move |tx| (ty, tx)))
            .map(|(ty, tx)| Ok((ty, tx, self.chunk_record(var, KIND_TILE2D, ty, tx)?)))
            .collect::<RwResult<Vec<(usize, usize, &ChunkRecord)>>>()?;

        let decode = |&(ty, tx, record): &(usize, usize, &ChunkRecord)| {
            let (rows, cols) = self.tile_dims(ty, tx);
            Ok((ty, tx, self.decode_tile(&var.name, record, rows * cols)?))
        };
        let decoded: Vec<(usize, usize, Vec<f32>)> = if jobs.len() > PARALLEL_TILE_THRESHOLD {
            jobs.par_iter().map(decode).collect::<RwResult<_>>()?
        } else {
            jobs.iter().map(decode).collect::<RwResult<_>>()?
        };

        let mut values = vec![f32::NAN; ny * nx];
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
        let var = self.lookup(name)?;
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
        let mut values = vec![f32::NAN; wny * wnx];

        // Resolve every intersecting tile's record up front so index
        // problems surface before any decompression work starts.
        let jobs = (y0 / TILE_Y..=(y1 - 1) / TILE_Y)
            .flat_map(|ty| (x0 / TILE_X..=(x1 - 1) / TILE_X).map(move |tx| (ty, tx)))
            .map(|(ty, tx)| Ok((ty, tx, self.chunk_record(var, KIND_TILE2D, ty, tx)?)))
            .collect::<RwResult<Vec<(usize, usize, &ChunkRecord)>>>()?;

        // Dense tiles decompress (in parallel above the threshold); EMPTY and
        // CONSTANT tiles are handled from their records alone at placement.
        let decode = |&(ty, tx, record): &(usize, usize, &ChunkRecord)| {
            if record.flags & FLAG_EMPTY != 0
                || (record.flags & FLAG_CONSTANT != 0 && record.len == 0)
            {
                return Ok(None);
            }
            let (rows, cols) = self.tile_dims(ty, tx);
            Ok(Some(self.decode_tile(&var.name, record, rows * cols)?))
        };
        let decoded: Vec<Option<Vec<f32>>> = if jobs.len() > PARALLEL_TILE_THRESHOLD {
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
        Ok(chunk[start..start + levels].to_vec())
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
            return Ok(vec![f32::NAN; value_count]);
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return Ok(vec![record.center; value_count]);
        }
        let compressed = self.payload_slice(var_name, record)?;
        // bulk::decompress caps the output at raw_len — a crafted chunk cannot
        // balloon past the size the index promised.
        let raw =
            zstd::bulk::decompress(compressed, record.raw_len as usize).map_err(|err| {
                RwStoreError::Chunk(format!(
                    "zstd decode failed for variable '{var_name}' column chunk ({},{}): {err}",
                    record.tile_y, record.tile_x
                ))
            })?;
        if raw.len() != record.raw_len as usize {
            return Err(RwStoreError::Chunk(format!(
                "variable '{var_name}' column chunk ({},{}): decompressed {} bytes, \
                 index says raw_len {}",
                record.tile_y,
                record.tile_x,
                raw.len(),
                record.raw_len
            )));
        }
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
            return Ok(vec![f32::NAN; value_count]);
        }
        if record.flags & FLAG_CONSTANT != 0 && record.len == 0 {
            return Ok(vec![record.center; value_count]);
        }
        let compressed = self.payload_slice(var_name, record)?;
        // bulk::decompress caps the output at raw_len — a crafted chunk cannot
        // balloon past the size the index promised.
        let raw =
            zstd::bulk::decompress(compressed, record.raw_len as usize).map_err(|err| {
                RwStoreError::Chunk(format!(
                    "zstd decode failed for variable '{var_name}' tile ({},{}): {err}",
                    record.tile_y, record.tile_x
                ))
            })?;
        if raw.len() != record.raw_len as usize {
            return Err(RwStoreError::Chunk(format!(
                "variable '{var_name}' tile ({},{}): decompressed {} bytes, index says raw_len {}",
                record.tile_y,
                record.tile_x,
                raw.len(),
                record.raw_len
            )));
        }
        decode_f32_tile(record.flags, record.center, &raw, value_count)
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

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rw-store-reader-{}-{}",
            std::process::id(),
            name
        ));
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
            "2026-06-09T12:00:00Z",
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
        let mut writer =
            HourWriter::new("hrrr", "run", 0, big_nx, big_ny, "hash", "build");
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
    fn windowed_read_equals_full_read_crop() {
        let dir = test_dir("window-crop");
        let path = dir.join("hour.rws");
        write_sample(&path);
        let reader = HourReader::open(&path).unwrap();

        // (request, expected clamped (x0, y0, nx, ny))
        type Rect = (usize, usize, usize, usize);
        let cases: &[(Rect, Rect)] = &[
            ((10, 10, 50, 50), (10, 10, 40, 40)),        // tile-interior
            ((200, 200, 400, 460), (200, 200, 200, 260)), // straddles 4 tiles
            ((500, 400, 9999, 9999), (500, 400, 100, 100)), // edge-clamped
            ((599, 499, 600, 500), (599, 499, 1, 1)),    // single cell
            ((0, 0, 600, 500), (0, 0, 600, 500)),        // full grid
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
        for &(x0, y0, x1, y1) in
            &[(50usize, 50usize, 50usize, 90usize), (700, 0, 9999, 10), (30, 20, 10, 40)]
        {
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
        assert_eq!((target.tile_y, target.tile_x), (1, 2), "last dense tile of var B");
        corrupt_payload(&mut bytes, target);
        let corrupted_path = dir.join("corrupted.rws");
        fs::write(&corrupted_path, &bytes).unwrap();

        let reader = HourReader::open(&corrupted_path).unwrap();
        // The window read never touches tile (1,2), so it must still succeed
        // and match the pristine data...
        let window = reader.read_window_2d("dewpoint_2m", 10, 10, 50, 50).unwrap();
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
            constant.values.iter().all(|v| v.to_bits() == 42.0f32.to_bits()),
            "CONSTANT tile window must be all center (42.0)"
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
        assert!(ok.values.iter().all(|v| v.is_nan()), "temp_2m tile (0,0) is EMPTY");

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
        let err = reader.read_window_2d("no_such_var", 0, 0, 10, 10).unwrap_err();
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

//! Hour-file writer: assembles 2D surface variables into one rw-store file.
//!
//! File layout: `[64B header][meta JSON][index records, 64B each, sorted by
//! sort_key()][payload]`. Record offsets are absolute file offsets. Tiles
//! are encoded in parallel with rayon but staged and sorted deterministically,
//! so identical inputs always produce byte-identical files.

use std::path::Path;

use rayon::prelude::*;

use crate::atomic::atomic_write_bytes;
use crate::codec::encode_f32_tile;
use crate::error::{RwResult, RwStoreError};
use crate::format::{
    CODEC_2D, COL_X, COL_Y, KIND_TILE2D, RwsChunking, RwsHourMeta, RwsVariableMeta,
    RwsWriterInfo, SCHEMA_HOUR, TILE_X, TILE_Y,
};
use crate::header::RwsHeader;
use crate::index::ChunkRecord;

/// zstd compression level for dense tile payloads (matches CODEC_2D name).
const ZSTD_LEVEL: i32 = 1;

/// One encoded chunk staged for assembly; `record.offset` is assigned in
/// [`HourWriter::finish`] once the global chunk order is known.
struct StagedChunk {
    record: ChunkRecord,
    compressed: Vec<u8>,
}

/// Incremental builder for a single per-hour store file.
pub struct HourWriter {
    model: String,
    run: String,
    forecast_hour: u16,
    nx: usize,
    ny: usize,
    grid_hash: String,
    writer_build: String,
    variables: Vec<RwsVariableMeta>,
    chunks: Vec<StagedChunk>,
}

impl HourWriter {
    pub fn new(
        model: &str,
        run: &str,
        forecast_hour: u16,
        nx: usize,
        ny: usize,
        grid_hash: &str,
        writer_build: &str,
    ) -> Self {
        Self {
            model: model.to_string(),
            run: run.to_string(),
            forecast_hour,
            nx,
            ny,
            grid_hash: grid_hash.to_string(),
            writer_build: writer_build.to_string(),
            variables: Vec::new(),
            chunks: Vec::new(),
        }
    }

    /// Add a 2D surface field (row-major, `ny * nx` values). Tiles are
    /// encoded in parallel; returns the assigned variable id.
    pub fn add_surface2d(
        &mut self,
        name: &str,
        units: &str,
        selector: serde_json::Value,
        values: &[f32],
    ) -> RwResult<u16> {
        let expected = self.nx * self.ny;
        if values.len() != expected {
            return Err(RwStoreError::Format(format!(
                "variable '{name}': expected {expected} values ({} x {}), got {}",
                self.ny,
                self.nx,
                values.len()
            )));
        }
        if self.variables.iter().any(|var| var.name == name) {
            return Err(RwStoreError::Format(format!(
                "duplicate variable name '{name}'"
            )));
        }
        let var_id = u16::try_from(self.variables.len()).map_err(|_| {
            RwStoreError::Format(format!(
                "too many variables: var id for '{name}' exceeds u16"
            ))
        })?;

        let tiles_y = self.ny.div_ceil(TILE_Y);
        let tiles_x = self.nx.div_ceil(TILE_X);
        let tile_coords: Vec<(usize, usize)> = (0..tiles_y)
            .flat_map(|ty| (0..tiles_x).map(move |tx| (ty, tx)))
            .collect();

        let nx = self.nx;
        let ny = self.ny;
        // Parallel encode; collect preserves tile_coords order so staging
        // (and therefore the final file) is independent of rayon scheduling.
        let encoded: Vec<StagedChunk> = tile_coords
            .par_iter()
            .map(|&(ty, tx)| -> RwResult<StagedChunk> {
                let y0 = ty * TILE_Y;
                let x0 = tx * TILE_X;
                let y1 = (y0 + TILE_Y).min(ny);
                let x1 = (x0 + TILE_X).min(nx);
                let mut tile_values = Vec::with_capacity((y1 - y0) * (x1 - x0));
                for y in y0..y1 {
                    tile_values.extend_from_slice(&values[y * nx + x0..y * nx + x1]);
                }
                let chunk = encode_f32_tile(&tile_values);
                let compressed = if chunk.payload.is_empty() {
                    Vec::new()
                } else {
                    zstd::stream::encode_all(&chunk.payload[..], ZSTD_LEVEL)?
                };
                Ok(StagedChunk {
                    record: ChunkRecord {
                        var_id,
                        kind: KIND_TILE2D,
                        flags: chunk.flags,
                        tile_y: ty as u32,
                        tile_x: tx as u32,
                        offset: 0, // assigned in finish()
                        len: compressed.len() as u32,
                        raw_len: chunk.payload.len() as u32,
                        center: chunk.center,
                        scale: chunk.scale,
                        min: chunk.min,
                        max: chunk.max,
                        valid_count: chunk.valid_count,
                    },
                    compressed,
                })
            })
            .collect::<RwResult<Vec<StagedChunk>>>()?;

        self.chunks.extend(encoded);
        self.variables.push(RwsVariableMeta {
            id: var_id,
            name: name.to_string(),
            units: units.to_string(),
            kind: "surface2d".to_string(),
            codec: CODEC_2D.to_string(),
            levels_hpa: Vec::new(),
            selector,
        });
        Ok(var_id)
    }

    /// Assemble and atomically write the hour file, returning its metadata.
    pub fn finish(mut self, path: &Path) -> RwResult<RwsHourMeta> {
        self.chunks.sort_by_key(|chunk| chunk.record.sort_key());

        let meta = RwsHourMeta {
            schema: SCHEMA_HOUR.to_string(),
            model: self.model,
            run: self.run,
            forecast_hour: self.forecast_hour,
            nx: self.nx,
            ny: self.ny,
            grid_hash: self.grid_hash,
            variables: self.variables,
            chunking: RwsChunking {
                tile_y: TILE_Y,
                tile_x: TILE_X,
                col_y: COL_Y,
                col_x: COL_X,
            },
            writer: RwsWriterInfo {
                name: "rw-store".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                build: self.writer_build,
            },
        };
        let meta_bytes =
            serde_json::to_vec(&meta).map_err(|err| RwStoreError::Meta(err.to_string()))?;
        let meta_len = u32::try_from(meta_bytes.len()).map_err(|_| {
            RwStoreError::Format(format!("meta JSON too large: {} bytes", meta_bytes.len()))
        })?;
        let header = RwsHeader::for_layout(meta_len, self.chunks.len() as u64);

        // Assign absolute payload offsets cursor-style in sorted order.
        // EMPTY/CONSTANT chunks have len 0; their offset is wherever the
        // cursor currently sits (value unused by readers).
        let mut cursor = header.payload_offset;
        for chunk in &mut self.chunks {
            chunk.record.offset = cursor;
            cursor += chunk.compressed.len() as u64;
        }

        let total_len = cursor as usize;
        let mut bytes = Vec::with_capacity(total_len);
        bytes.extend_from_slice(&header.pack());
        bytes.extend_from_slice(&meta_bytes);
        for chunk in &self.chunks {
            chunk.record.pack_into(&mut bytes);
        }
        debug_assert_eq!(bytes.len() as u64, header.payload_offset);
        for chunk in &self.chunks {
            bytes.extend_from_slice(&chunk.compressed);
        }
        debug_assert_eq!(bytes.len(), total_len);

        atomic_write_bytes(path, &bytes)?;
        Ok(meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RwStoreError;
    use crate::format::{
        CODEC_2D, FLAG_CONSTANT, FLAG_EMPTY, KIND_TILE2D, RwsHourMeta, SCHEMA_HOUR, TILE_X,
        TILE_Y,
    };
    use crate::header::RwsHeader;
    use crate::index::ChunkRecord;
    use std::fs;
    use std::path::{Path, PathBuf};

    const NX: usize = 600; // columns -> x tiles of 256, 256, 88
    const NY: usize = 500; // rows    -> y tiles of 256, 244
    const TILES_PER_VAR: usize = 6; // 3 x-tiles * 2 y-tiles

    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rw-store-writer-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Var A: smooth field, with tile (0,0) all-NaN (EMPTY) and tile (0,1)
    /// all 42.0 (CONSTANT). Both regions are full 256x256 aligned tiles.
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
        values
    }

    /// Var B: varying everywhere; every tile must encode dense.
    fn grid_b() -> Vec<f32> {
        (0..NY)
            .flat_map(|y| (0..NX).map(move |x| 100.0 + 0.5 * x as f32 - 0.25 * y as f32))
            .collect()
    }

    fn tile_dims(tile_y: u32, tile_x: u32) -> (usize, usize) {
        let y0 = tile_y as usize * TILE_Y;
        let x0 = tile_x as usize * TILE_X;
        ((NY - y0).min(TILE_Y), (NX - x0).min(TILE_X))
    }

    fn write_sample(path: &Path) -> RwsHourMeta {
        let mut writer = HourWriter::new(
            "hrrr",
            "2026-06-09T12:00:00Z",
            6,
            NX,
            NY,
            "gridhash-test",
            "test-build",
        );
        let id_a = writer
            .add_surface2d(
                "temp_2m",
                "K",
                serde_json::json!({"grib_short_name": "TMP", "level": "2 m above ground"}),
                &grid_a(),
            )
            .unwrap();
        let id_b = writer
            .add_surface2d(
                "dewpoint_2m",
                "K",
                serde_json::json!({"grib_short_name": "DPT", "level": "2 m above ground"}),
                &grid_b(),
            )
            .unwrap();
        assert_eq!((id_a, id_b), (0, 1), "var ids assigned sequentially");
        writer.finish(path).unwrap()
    }

    #[test]
    fn writes_two_var_hour_file_with_correct_raw_layout() {
        let dir = test_dir("layout");
        let path = dir.join("hour.rws");
        let returned_meta = write_sample(&path);

        let bytes = fs::read(&path).unwrap();
        let header = RwsHeader::parse(&bytes).unwrap();

        // Meta JSON.
        let meta_end = 64 + header.meta_len as usize;
        let meta: RwsHourMeta = serde_json::from_slice(&bytes[64..meta_end]).unwrap();
        assert_eq!(meta, returned_meta, "finish() must return the written meta");
        assert_eq!(meta.schema, SCHEMA_HOUR);
        assert_eq!(meta.model, "hrrr");
        assert_eq!(meta.run, "2026-06-09T12:00:00Z");
        assert_eq!(meta.forecast_hour, 6);
        assert_eq!(meta.nx, NX);
        assert_eq!(meta.ny, NY);
        assert_eq!(meta.grid_hash, "gridhash-test");
        assert_eq!(meta.chunking.tile_y, TILE_Y);
        assert_eq!(meta.chunking.tile_x, TILE_X);
        assert_eq!(meta.writer.name, "rw-store");
        assert_eq!(meta.writer.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(meta.writer.build, "test-build");

        assert_eq!(meta.variables.len(), 2);
        assert_ne!(meta.variables[0].id, meta.variables[1].id);
        for var in &meta.variables {
            assert_eq!(var.kind, "surface2d");
            assert_eq!(var.codec, CODEC_2D);
            assert!(var.levels_hpa.is_empty());
        }
        assert_eq!(meta.variables[0].name, "temp_2m");
        assert_eq!(meta.variables[1].name, "dewpoint_2m");

        // Index records.
        assert_eq!(header.index_count as usize, 2 * TILES_PER_VAR, "12 chunks");
        assert_eq!(
            header.index_offset as usize, meta_end,
            "index follows meta JSON"
        );
        let records: Vec<ChunkRecord> = (0..header.index_count as usize)
            .map(|i| {
                let start = header.index_offset as usize + i * 64;
                ChunkRecord::unpack(&bytes[start..start + 64]).unwrap()
            })
            .collect();
        for pair in records.windows(2) {
            assert!(
                pair[0].sort_key() < pair[1].sort_key(),
                "records must be strictly sorted by sort_key"
            );
        }
        assert!(records.iter().all(|r| r.kind == KIND_TILE2D));

        // payload_offset matches the fixed layout.
        assert_eq!(
            header.payload_offset,
            header.index_offset + header.index_count * 64
        );

        // NaN tile (var 0, tile 0,0) -> EMPTY.
        let empty = records
            .iter()
            .find(|r| r.var_id == 0 && r.tile_y == 0 && r.tile_x == 0)
            .expect("record for var 0 tile (0,0)");
        assert_ne!(empty.flags & FLAG_EMPTY, 0, "NaN tile must be FLAG_EMPTY");
        assert_eq!(empty.len, 0);
        assert_eq!(empty.raw_len, 0);

        // Constant tile (var 0, tile 0,1) -> CONSTANT with center 42.0.
        let constant = records
            .iter()
            .find(|r| r.var_id == 0 && r.tile_y == 0 && r.tile_x == 1)
            .expect("record for var 0 tile (0,1)");
        assert_ne!(constant.flags & FLAG_CONSTANT, 0, "42.0 tile must be CONSTANT");
        assert_eq!(constant.len, 0);
        assert_eq!(constant.center, 42.0);

        // Dense records: 10 of 12; compressed payloads in bounds with exact raw sizes.
        let dense: Vec<&ChunkRecord> = records
            .iter()
            .filter(|r| r.flags & (FLAG_EMPTY | FLAG_CONSTANT) == 0)
            .collect();
        assert_eq!(dense.len(), 10, "4 dense tiles for var A + 6 for var B");
        for record in &dense {
            let (rows, cols) = tile_dims(record.tile_y, record.tile_x);
            assert!(record.len > 0, "dense chunk must have compressed payload");
            assert_eq!(
                record.raw_len as usize,
                rows * cols * 4,
                "raw_len must equal tile f32 byte count for tile ({},{})",
                record.tile_y,
                record.tile_x
            );
            assert!(record.offset >= header.payload_offset);
            assert!(record.offset + record.len as u64 <= bytes.len() as u64);
        }

        // Spot-check one dense payload: var B edge tile (1,2) = 244x88,
        // decompresses to the expected raw f32 bytes for that window.
        let spot = records
            .iter()
            .find(|r| r.var_id == 1 && r.tile_y == 1 && r.tile_x == 2)
            .expect("record for var 1 tile (1,2)");
        let compressed =
            &bytes[spot.offset as usize..spot.offset as usize + spot.len as usize];
        let raw = zstd::stream::decode_all(compressed).unwrap();
        assert_eq!(raw.len(), spot.raw_len as usize);
        let grid = grid_b();
        let mut expected = Vec::with_capacity(244 * 88 * 4);
        for y in 256..NY {
            for x in 512..NX {
                expected.extend_from_slice(&grid[y * NX + x].to_le_bytes());
            }
        }
        assert_eq!(raw, expected, "tile payload must be row-major within tile");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn same_input_produces_byte_identical_files() {
        let dir = test_dir("determinism");
        let path_one = dir.join("one.rws");
        let path_two = dir.join("two.rws");
        write_sample(&path_one);
        write_sample(&path_two);
        let bytes_one = fs::read(&path_one).unwrap();
        let bytes_two = fs::read(&path_two).unwrap();
        assert_eq!(
            bytes_one, bytes_two,
            "same inputs must produce byte-identical files"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_wrong_value_count() {
        let mut writer = HourWriter::new("hrrr", "run", 0, NX, NY, "hash", "build");
        let err = writer
            .add_surface2d("temp_2m", "K", serde_json::Value::Null, &[0.0; 17])
            .unwrap_err();
        assert!(
            matches!(err, RwStoreError::Format(_)),
            "expected Format error, got {err:?}"
        );
    }

    #[test]
    fn rejects_duplicate_variable_name() {
        let mut writer = HourWriter::new("hrrr", "run", 0, NX, NY, "hash", "build");
        writer
            .add_surface2d("temp_2m", "K", serde_json::Value::Null, &grid_b())
            .unwrap();
        let err = writer
            .add_surface2d("temp_2m", "K", serde_json::Value::Null, &grid_b())
            .unwrap_err();
        assert!(
            matches!(err, RwStoreError::Format(_)),
            "expected Format error, got {err:?}"
        );
    }
}

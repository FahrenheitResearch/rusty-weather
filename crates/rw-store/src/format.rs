//! On-disk format constants and metadata structures for rw-store files.

use serde::{Deserialize, Serialize};

/// Magic bytes at the start of every rw-store file.
pub const MAGIC: &[u8; 8] = b"RWSTORE1";
/// Current on-disk format version.
pub const VERSION: u32 = 1;
/// Format versions this build can read.
pub const SUPPORTED_VERSIONS: &[u32] = &[1];
/// Fixed-size file header length in bytes.
pub const HEADER_LEN: usize = 64;
/// Fixed-size chunk index record length in bytes.
pub const INDEX_RECORD_LEN: usize = 64;

/// 2D tile height in grid points.
pub const TILE_Y: usize = 256;
/// 2D tile width in grid points.
pub const TILE_X: usize = 256;
/// 3D column-chunk height in grid points.
pub const COL_Y: usize = 16;
/// 3D column-chunk width in grid points.
pub const COL_X: usize = 16;

/// Chunk contains no finite values.
pub const FLAG_EMPTY: u8 = 1;
/// All finite values in the chunk are equal.
pub const FLAG_CONSTANT: u8 = 2;
/// Chunk contains at least one missing (NaN) value.
pub const FLAG_HAS_MISSING: u8 = 4;

/// Chunk kind: 2D surface tile.
pub const KIND_TILE2D: u8 = 0;
/// Chunk kind: 3D pressure-level column chunk.
pub const KIND_COLUMN3D: u8 = 1;

/// Schema identifier embedded in per-hour metadata.
pub const SCHEMA_HOUR: &str = "rw-store.hour.v1";
/// Codec name for 2D tiles: zstd-compressed raw f32.
pub const CODEC_2D: &str = "zstd1_f32";
/// Codec name for 3D column chunks: zstd-compressed affine-quantized i16.
pub const CODEC_3D: &str = "zstd1_affine_i16";

/// Per-hour store file metadata.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsHourMeta {
    pub schema: String,
    pub model: String,
    pub run: String,
    pub forecast_hour: u16,
    pub nx: usize,
    pub ny: usize,
    pub grid_hash: String,
    pub variables: Vec<RwsVariableMeta>,
    pub chunking: RwsChunking,
    pub writer: RwsWriterInfo,
}

/// Metadata for a single variable stored in an hour file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsVariableMeta {
    pub id: u16,
    pub name: String,
    pub units: String,
    /// "surface2d" | "pressure3d"
    pub kind: String,
    pub codec: String,
    pub levels_hpa: Vec<u16>,
    pub selector: serde_json::Value,
}

/// Chunk geometry used by the writer.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsChunking {
    pub tile_y: usize,
    pub tile_x: usize,
    pub col_y: usize,
    pub col_x: usize,
}

/// Provenance of the writer that produced a store file.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RwsWriterInfo {
    pub name: String,
    pub version: String,
    pub build: String,
}

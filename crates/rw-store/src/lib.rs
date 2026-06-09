//! Per-hour, self-contained weather field store. Each file holds one model
//! run hour: 2D surface fields as zstd-compressed f32 tiles supporting
//! windowed (regional) reads, and 3D pressure-level fields as quantized
//! affine-i16 column chunks laid out for fast single-point sounding pulls.

pub mod codec;
pub mod error;
pub mod format;
pub mod header;
pub mod index;

pub use error::{RwResult, RwStoreError};

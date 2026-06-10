//! Per-hour, self-contained weather field store. Each file holds one model
//! run hour: 2D surface fields as zstd-compressed f32 tiles supporting
//! windowed (regional) reads, and 3D pressure-level fields as quantized
//! affine-i16 column chunks laid out for fast single-point sounding pulls.

pub mod atomic;
pub mod codec;
pub mod error;
pub mod format;
pub mod grid;
pub mod header;
pub mod index;
pub mod ingest;
pub mod reader;
pub mod run;
pub mod writer;

pub use error::{RwResult, RwStoreError};
pub use ingest::{
    DerivedFieldInput, HourIngestWriter, PressureVolumeInput, StoredField2D, WrittenHour,
    derived_selector, derived_selector_slug, read_field_2d, read_grid_2d, write_hour_from_fields,
    write_hour_from_fields_with_derived,
};

//! `rw-glm` — the `.rwl` flash-event store for GOES GLM lightning.
//!
//! A point-event sibling to `rw-store` (which is gridded). Flashes are written
//! into fixed 32-byte records, partitioned into 10-minute `tHHMM.rwl` buckets
//! under a per-day directory, and read back lock-free by time range and
//! bounding box.
//!
//! Layout:
//! ```text
//! <root>/glm/<satellite>/window.json
//! <root>/glm/<satellite>/<YYYYMMDD>/tHHMM.rwl
//! ```
//!
//! The on-disk format is specified byte-for-byte in `docs/FORMAT.md §10` and
//! frozen by the golden fixtures in `tests/golden/v1/`.
//!
//! This crate (Task 1) provides the format, the [`store::BucketWriter`], the
//! [`reader::read_flashes`] API, and the [`validate`] module. Granule decode
//! and the S3 follow engine arrive in later tasks; they are *not* dependencies
//! here (no s3/netcdf in Task 1).

pub mod error;
pub mod format;
pub mod reader;
pub mod store;
pub mod validate;

pub use error::{RwlError, RwlResult};
pub use format::{
    FLAG_DEGRADED_QUALITY, FlashRecord, KNOWN_FLAGS, RECORD_LEN, RwlHeader, VERSION, bucket_name,
    date_dir, saturate_duration_ms,
};
pub use reader::{BBox, Flash, read_flashes};
pub use store::{BucketWriter, WindowManifest, pack_bucket};
pub use validate::{ValidateDepth, ValidationReport, ValidationStats, validate_bucket_file};

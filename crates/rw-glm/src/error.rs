//! Error type for the rw-glm crate.
//!
//! `RwlError` mirrors the shape of `rw_store::RwStoreError` (Io / Format /
//! UnsupportedVersion / Locked) but is owned by this crate so the `.rwl`
//! format does not leak rw-store's gridded-store error vocabulary to its
//! consumers.
//!
//! **Boundary with rw-store.** rw-glm reuses `rw_store::atomic` (atomic
//! temp+fsync+rename) and `rw_store::lock::RunLock` (the per-directory writer
//! advisory lock) verbatim — both are format-agnostic. Those functions return
//! `rw_store::RwStoreError`. Rather than re-export that type or pass it
//! through, rw-glm maps it into `RwlError` at the call site via the
//! `#[from] RwStoreError` conversion below: an rw-store `Io` becomes an rw-glm
//! `Io`, a `Locked` becomes a `Locked`, and anything else is folded into
//! `Format` with its display text preserved. Consumers therefore only ever see
//! `RwlError`.

use rw_store::RwStoreError;

/// Result alias used throughout rw-glm.
pub type RwlResult<T> = Result<T, RwlError>;

/// Errors produced while reading or writing `.rwl` flash files.
#[derive(Debug, thiserror::Error)]
pub enum RwlError {
    /// An underlying I/O failure (open/read/write/rename). The reader and
    /// validator reserve this variant for real I/O — every *format* problem is
    /// surfaced through [`crate::ValidationReport::errors`] (validator) or a
    /// [`RwlError::Format`] (reader open path), never an `Io`.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A malformed `.rwl` file: bad magic, truncation, size/count mismatch, or
    /// any other layout violation surfaced by an eager reader/open path.
    #[error("malformed flash file: {0}")]
    Format(String),

    /// The file declares a format version this build does not support.
    #[error("unsupported flash-file version {found} (supported: {supported:?})")]
    UnsupportedVersion {
        found: u32,
        supported: &'static [u32],
    },

    /// The satellite store directory is locked by another writer (propagated
    /// from `rw_store::RunLock`).
    #[error("flash store locked: {0}")]
    Locked(String),
}

impl From<RwStoreError> for RwlError {
    /// Map an rw-store error (only ever produced by the reused `atomic`/`lock`
    /// helpers) into the matching `RwlError` variant. `Io` and `Locked` map
    /// one-to-one; every other rw-store variant is folded into `Format` with
    /// its display text preserved so no information is lost.
    fn from(err: RwStoreError) -> Self {
        match err {
            RwStoreError::Io(io) => RwlError::Io(io),
            RwStoreError::Locked(msg) => RwlError::Locked(msg),
            other => RwlError::Format(other.to_string()),
        }
    }
}

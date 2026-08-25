use thiserror::Error;

#[derive(Debug, Error)]
pub enum DecodeError {
    #[error("input is empty")]
    Empty,
    #[error("input length {actual} exceeds configured limit {limit}")]
    InputLimit { actual: usize, limit: usize },
    #[error("no supported Level III message (58 or 62) found in the first {searched} bytes")]
    MessageNotFound { searched: usize },
    #[error("truncated {context} at byte {offset}: need {needed} bytes, have {available}")]
    Truncated {
        context: &'static str,
        offset: usize,
        needed: usize,
        available: usize,
    },
    #[error("invalid {context} at byte {offset}: {detail}")]
    Invalid {
        context: &'static str,
        offset: usize,
        detail: String,
    },
    #[error("unsupported Level III product code {0}; expected 58 or 62")]
    UnsupportedProduct(i16),
    #[error("unsupported Level III compression method {method} at byte {offset}")]
    UnsupportedCompression { method: u16, offset: usize },
    #[error("bzip2 decompression failed: {0}")]
    Decompression(String),
    #[error("decompressed body size {actual} does not equal PDB size {expected}")]
    DecompressedSize { expected: usize, actual: usize },
    #[error("decoded collection '{collection}' exceeds configured limit {limit}")]
    Limit {
        collection: &'static str,
        limit: usize,
    },
    #[error("invalid ASCII in {context} at byte {offset}")]
    NonAscii {
        context: &'static str,
        offset: usize,
    },
    #[error("storm table and symbology disagree: {0}")]
    CrossCheck(String),
}

#[derive(Debug, Error, Clone, PartialEq)]
pub enum PairingError {
    #[error("maximum time delta must be non-negative")]
    NegativeTimeWindow,
    #[error("maximum centroid distance must be finite and non-negative")]
    InvalidDistance,
    #[error("geometry '{geometry_id}' has a non-finite centroid")]
    NonFiniteGeometry { geometry_id: String },
}

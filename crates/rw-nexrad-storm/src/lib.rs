//! Strict decoder for WSR-88D Level III storm tracking and structure products.
//!
//! The authoritative binary layout is WSR-88D ROC document 2620001AD,
//! Build 24.0 (2025-08-19), section 3.3.1 and Figures 3-6, 3-8b, 3-14,
//! and 3-16. Product semantics come from ROC document 2620003AE,
//! Build 24.0 (2025-08-19), section 18 and Appendix C Formats I and V.
//!
//! Level III products 58 and 62 supply centroids, tracks, and point
//! attributes. They do **not** supply storm-cell polygons. See [`pair_geometry`]
//! for the deliberately provenance-preserving bridge to separately derived
//! Level II geometry.

mod error;
mod pair;
mod parse;
mod types;

pub use error::{DecodeError, PairingError};
pub use pair::pair_geometry;
pub use parse::{decode, decode_with_options};
pub use types::*;

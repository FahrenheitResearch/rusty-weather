//! Grid-to-grid regridding for rustwx fields.
//!
//! This crate owns reusable sparse regridding plans. Rendering crates should
//! consume already-prepared fields rather than remapping while drawing.

pub mod bilinear;
pub mod conservative;
pub mod error;
pub mod grid;
pub mod idw;
pub mod method;
pub mod nearest;
pub mod plan;
pub mod vector;
pub mod weights;

pub use error::RegridError;
pub use grid::{
    CurvilinearLatLonGrid, GridFingerprint, GridGeometry, GridProjection, LatLon, OrientedGrid,
    ProjectedStructuredGrid, RegularLatLonGrid, RegularLatLonSpec, SweepAxis, VectorOrientation,
    core_projection_to_regrid,
};
pub use method::{ConservativeNormalization, MissingPolicy, RegridMethod, RegridOptions};
pub use plan::{RegridPlan, regrid_selected_field_f32};
pub use rustwx_core::GridShape;
pub use vector::VectorRegridPolicy;
pub use weights::SparseWeights;

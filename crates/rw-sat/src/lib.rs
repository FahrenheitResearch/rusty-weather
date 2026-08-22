//! Continuous geostationary satellite ingest and delivery for Rusty Weather.
//!
//! Native GOES files are retained for bounded, windowed Full Disk/CONUS/meso
//! rendering while compact `.rws` frames remain available as desktop previews.
//! A shared product catalog, enhancement set, and renderer keep the desktop,
//! CLI, and rw-server visually and scientifically consistent.

pub mod abi;
pub mod archive;
pub mod composite;
pub mod enhancement;
pub mod events;
pub mod export;
pub mod fci;
pub mod follow;
pub mod geostationary;
pub mod goes;
pub mod himawari;
pub mod mtg;
pub mod netcdf;
pub mod palette;
pub mod product;
pub mod product_render;
pub mod s3;
pub mod solar;
pub mod store;
pub mod tile;
pub mod window;

pub use archive::{
    NATIVE_SOURCE_ARCHIVE_DIR, NativeArchivePruneReport, NativeSatelliteFrame, archive_goes_source,
    automatic_preview_stride, list_native_frames, native_archive_root, prune_native_archive,
    resolve_native_frame,
};
pub use enhancement::{SatelliteEnhancement, default_enhancement_for_channel};
pub use events::{NEVER_CANCEL, SatError, SatEvent, print_event};
pub use follow::{FollowConfig, FollowSummary, follow};
pub use product::{
    GoesAbiProduct, SatelliteProductCategory, SatelliteProductDescriptor,
    SatelliteSectorDescriptor, product_catalog, sector_catalog,
};
pub use store::{StoredFrame, WrittenFrame, read_frame, write_band_frame};
pub use tile::{DEFAULT_TILE_SIZE, MAXIMUM_TILE_ZOOM, NativeSatelliteTile, render_native_xyz_tile};
pub use window::{EvictionReport, WindowConfig, enforce_window};

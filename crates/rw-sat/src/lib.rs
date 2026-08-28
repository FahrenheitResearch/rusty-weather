//! Continuous geostationary satellite ingest and delivery for Rusty Weather.
//!
//! Native GOES files are retained for bounded, windowed Full Disk/CONUS/meso
//! rendering while compact `.rws` frames remain available as desktop previews.
//! A shared product catalog, enhancement set, and renderer keep the desktop,
//! CLI, and rw-server visually and scientifically consistent.

pub mod abi;
pub mod archive;
pub mod cloud;
pub mod composite;
pub mod cwp;
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
    NATIVE_SOURCE_ARCHIVE_DIR, NativeArchivePruneReport, NativeL2ProductSource,
    NativeSatelliteFrame, ResolvedNativeSatelliteFrame, archive_goes_l2_source,
    archive_goes_source, automatic_preview_stride, list_native_cloud_frames, list_native_frames,
    native_archive_root, native_frame_cloud_revision, native_frame_product_revision,
    prune_native_archive, resolve_native_cloud_frame, resolve_native_cloud_frame_with_revision,
    resolve_native_frame, resolve_native_frame_with_revision,
};
pub use cloud::{
    CLOUD_CATALOG_PREFIX, CloudProduct, CloudProductDescriptor, CloudProductField,
    CloudSourceIdentity, CloudWindow, DEFAULT_CLOUD_PREVIEW_CELLS, DqfReport, DqfRule,
    MAX_DENSE_CLOUD_PLANE_CELLS, cloud_product_catalog, read_archived_cloud_preview,
    read_archived_cloud_window, read_cloud_product_field, read_cloud_product_field_window,
};
pub use cwp::{
    CLOUD_WATER_PATH_INPUTS, CloudPhase, CwpCounts, cloud_water_path_g_m2, cloud_water_path_plane,
};
pub use enhancement::{SatelliteEnhancement, default_enhancement_for_channel};
pub use events::{NEVER_CANCEL, SatError, SatEvent, print_event};
pub use follow::{FollowConfig, FollowSummary, follow};
pub use product::{
    GoesAbiProduct, SatelliteProductCategory, SatelliteProductDescriptor,
    SatelliteSectorDescriptor, product_catalog, sector_catalog,
};
pub use store::{StoredFrame, WrittenFrame, read_frame, write_band_frame};
pub use tile::{
    DEFAULT_TILE_SIZE, MAXIMUM_TILE_ZOOM, NativeSatelliteRgbaTile, NativeSatelliteTile,
    PreparedNativeSatelliteTileRenderer, render_native_xyz_tile,
};
pub use window::{EvictionReport, WindowConfig, enforce_window};

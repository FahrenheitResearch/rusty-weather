//! Reusable egui widgets/panels for browsing rw-store weather data.
//!
//! Library-first: no eframe dependency, no `egui::Context` ownership — every
//! panel renders into a host-provided `&mut egui::Ui`, so the same panels
//! mount in any egui app (the `rusty-weather-ui` shell here, bowecho, ...).
//!
//! Building blocks:
//! - [`StoreView`]: enumerate a store root (models → runs → timesteps via
//!   `run.json`) and open timestep/grid files.
//! - [`StoreWorker`]: a background IO thread so the UI never blocks on file
//!   reads; plain-data requests/responses over channels.
//! - [`RunBrowserPanel`], [`FieldViewerPanel`], [`PlotViewerPanel`],
//!   [`SoundingPanel`], [`DownloadPanel`], [`SatellitePanel`], [`SatPlayerPanel`]: the panels
//!   themselves — pure widgets over host-pushed data (the download and
//!   satellite panels never touch the network; the host owns the workers
//!   that resolve their events).
//! - [`skewt`]: bridge from store sounding data to the production
//!   `rustwx-sounding` (sharprs) skew-T renderer.
//! - [`colormap`]: the false-color ramp used by the field viewer (a data
//!   inspection aid, not the production render palette).
//! - [`stats`]: the always-on lightweight op-timing registry + one-line
//!   strip (no profiler dependency; a few `Instant::now` calls).
//! - [`synthetic`]: dev/test helper that writes a tiny synthetic store so
//!   everything runs without ingested data.
//!
//! Deep profiling: the `profiling` feature adds puffin scopes around the
//! worker's store reads, the field-viewer texture build, and the skew-T
//! build/render. Default off — hosts that want flame-level data (the
//! rusty-weather-ui shell) turn it on; bowecho compiles it out.

pub mod colormap;
pub mod iso_levels;
mod panels;
pub mod skewt;
pub mod stats;
mod store_view;
pub mod style_overrides;
pub mod synthetic;
mod worker;

pub use panels::{
    AvailabilityView, ColorTableEditorPanel, CustomDomain, DownloadEvent, DownloadPanel,
    DownloadRunState, DownloadSpec, DownloadStage, EstimateView, FieldViewerEvent,
    FieldViewerPanel, HourDoneView, ModelOption, NativePlotMapDetail, NativePlotRenderScale,
    NativePlotSampling, NativePlotSettings, NativePlotStyle, PlotViewerPanel, RunBrowserPanel,
    SatDiskUsage, SatFollowSpec, SatFollowState, SatFrameImage, SatLayerOption, SatPlayerEvent,
    SatPlayerPanel, SatRunKey, SatRunListing, SatSatelliteOption, SatSectorOption, SatelliteEvent,
    SatellitePanel, SoundingFormulaDiagnostic, SoundingPanel, SoundingViewState, StageState,
    format_bytes, shift_date_yyyymmdd, today_yyyymmdd_utc,
};

/// Install the font families used by the native SHARPpy sounding widget.
/// Hosts should call this once while configuring their egui context.
pub fn install_sounding_fonts(ctx: &egui::Context) {
    sharppyrs::install_fonts(ctx);
}

pub use store_view::{
    DEFAULT_POOLED_READER_TILE_CACHE_BYTES, HourEntry, MAX_READER_POOL_READERS,
    MAX_READER_POOL_TILE_CACHE_BYTES, ModelEntry, ReaderPoolLimits, ReaderPoolStats, RunEntry,
    StoreTree, StoreView,
};
pub use style_overrides::{
    ProductStyleBinding, StyleOverrideSettings, UserColorTable, UserExtendMode, UserLegendMode,
    UserUnitConvert, normalize_product_key,
};
pub use worker::{
    FieldData, FieldKey, HourKey, ProfileVar, SoundingData, StoreRequest, StoreResponse,
    StoreWorker, SurfaceSample, VarInfo, VarKind, format_lead_seconds, format_valid_unix,
};

/// Crate-local profiling scope: expands to `puffin::profile_scope!` under
/// the `profiling` feature and to nothing otherwise (egui's own pattern),
/// so call sites stay clean and default builds carry zero profiler code.
#[cfg(feature = "profiling")]
macro_rules! profile_scope {
    ($($arg:tt)*) => {
        puffin::profile_scope!($($arg)*);
    };
}
#[cfg(not(feature = "profiling"))]
macro_rules! profile_scope {
    ($($arg:tt)*) => {};
}
pub(crate) use profile_scope;

// Re-export the egui this crate is built against so hosts can match
// versions, plus rw-store for direct store access.
pub use egui;
pub use rw_store;

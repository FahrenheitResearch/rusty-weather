//! Reusable egui widgets/panels for browsing rw-store weather data.
//!
//! Library-first: no eframe dependency, no `egui::Context` ownership — every
//! panel renders into a host-provided `&mut egui::Ui`, so the same panels
//! mount in any egui app (the `rusty-weather-ui` shell here, bowecho, ...).
//!
//! Building blocks:
//! - [`StoreView`]: enumerate a store root (models → runs → hours via
//!   `run.json`) and open hour/grid files.
//! - [`StoreWorker`]: a background IO thread so the UI never blocks on file
//!   reads; plain-data requests/responses over channels.
//! - [`RunBrowserPanel`], [`FieldViewerPanel`], [`SoundingPanel`],
//!   [`DownloadPanel`]: the panels themselves — pure widgets over
//!   host-pushed data (the download panel never touches the network; the
//!   host owns the worker that resolves its events).
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
mod panels;
pub mod skewt;
pub mod stats;
mod store_view;
pub mod synthetic;
mod worker;

pub use panels::{
    AvailabilityView, DownloadEvent, DownloadPanel, DownloadRunState, DownloadSpec, DownloadStage,
    EstimateView, FieldViewerEvent, FieldViewerPanel, HourDoneView, ModelOption, RunBrowserPanel,
    SoundingPanel, StageState, format_bytes, shift_date_yyyymmdd,
};
pub use store_view::{HourEntry, ModelEntry, RunEntry, StoreTree, StoreView};
pub use worker::{
    FieldData, FieldKey, HourKey, ProfileVar, SoundingData, StoreRequest, StoreResponse,
    StoreWorker, SurfaceSample, VarInfo, VarKind,
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

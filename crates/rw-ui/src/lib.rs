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
//! - [`RunBrowserPanel`], [`FieldViewerPanel`], [`SoundingPanel`]: the
//!   panels themselves — pure widgets over host-pushed data.
//! - [`skewt`]: bridge from store sounding data to the production
//!   `rustwx-sounding` (sharprs) skew-T renderer.
//! - [`colormap`]: the false-color ramp used by the field viewer (a data
//!   inspection aid, not the production render palette).
//! - [`synthetic`]: dev/test helper that writes a tiny synthetic store so
//!   everything runs without ingested data.

pub mod colormap;
mod panels;
pub mod skewt;
mod store_view;
pub mod synthetic;
mod worker;

pub use panels::{FieldViewerEvent, FieldViewerPanel, RunBrowserPanel, SoundingPanel};
pub use store_view::{HourEntry, ModelEntry, RunEntry, StoreTree, StoreView};
pub use worker::{
    FieldData, FieldKey, HourKey, ProfileVar, SoundingData, StoreRequest, StoreResponse,
    StoreWorker, SurfaceSample, VarInfo, VarKind,
};

// Re-export the egui this crate is built against so hosts can match
// versions, plus rw-store for direct store access.
pub use egui;
pub use rw_store;

//! Embeddable egui panels. Each takes `&mut egui::Ui` (no `Context`
//! ownership, no eframe), holds its own widget state, and reports user
//! intent back to the host as plain events.

mod color_table_editor;
mod download;
mod field_viewer;
mod plot_viewer;
mod run_browser;
mod sat_player;
mod satellite;
mod sounding;

pub use color_table_editor::ColorTableEditorPanel;
pub use download::{
    AvailabilityView, DownloadEvent, DownloadPanel, DownloadRunState, DownloadSpec, DownloadStage,
    EstimateView, HourDoneView, ModelOption, StageState, format_bytes, shift_date_yyyymmdd,
    today_yyyymmdd_utc,
};
pub use field_viewer::{FieldViewerEvent, FieldViewerPanel};
pub use plot_viewer::{
    CustomDomain, NativePlotMapDetail, NativePlotRenderScale, NativePlotSampling,
    NativePlotSettings, NativePlotStyle, PlotViewerPanel,
};
pub use run_browser::RunBrowserPanel;
pub use sat_player::{SatFrameImage, SatPlayerEvent, SatPlayerPanel, SatRunKey, SatRunListing};
pub use satellite::{
    SatDiskUsage, SatFollowSpec, SatFollowState, SatLayerOption, SatSatelliteOption,
    SatSectorOption, SatelliteEvent, SatellitePanel,
};
pub use sounding::{SoundingPanel, SoundingViewState};

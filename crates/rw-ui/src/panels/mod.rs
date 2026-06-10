//! Embeddable egui panels. Each takes `&mut egui::Ui` (no `Context`
//! ownership, no eframe), holds its own widget state, and reports user
//! intent back to the host as plain events.

mod download;
mod field_viewer;
mod run_browser;
mod sounding;

pub use download::{
    AvailabilityView, DownloadEvent, DownloadPanel, DownloadRunState, DownloadSpec, DownloadStage,
    EstimateView, HourDoneView, ModelOption, StageState, format_bytes, shift_date_yyyymmdd,
    today_yyyymmdd_utc,
};
pub use field_viewer::{FieldViewerEvent, FieldViewerPanel};
pub use run_browser::RunBrowserPanel;
pub use sounding::SoundingPanel;

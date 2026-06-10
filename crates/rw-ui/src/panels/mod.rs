//! Embeddable egui panels. Each takes `&mut egui::Ui` (no `Context`
//! ownership, no eframe), holds its own widget state, and reports user
//! intent back to the host as plain events.

mod field_viewer;
mod run_browser;
mod sounding;

pub use field_viewer::{FieldViewerEvent, FieldViewerPanel};
pub use run_browser::RunBrowserPanel;
pub use sounding::SoundingPanel;

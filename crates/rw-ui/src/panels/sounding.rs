//! Sounding panel backed by the native [`sharppyrs`] egui widget.
//!
//! Rusty Weather still owns store reads and column normalization. Once a
//! [`SoundingColumn`] has been assembled, sharppyrs owns the analyzed profile,
//! SPC-style layout, painting, hover behavior, and interactive panel layout.

use egui::{CollapsingHeader, RichText, Ui, Vec2};
use rustwx_sounding::SoundingColumn;
use serde::{Deserialize, Serialize};

use crate::profile_scope;
use crate::skewt::build_sounding_column;
use crate::worker::SoundingData;

const MS_TO_KT: f64 = 1.943_844_492_440_604_6;

#[derive(Default)]
enum SoundingState {
    #[default]
    Empty,
    Loading,
    Error(String),
    Ready(Box<ReadySounding>),
}

struct ReadySounding {
    data: Option<SoundingData>,
    heading: String,
    subheading: String,
    read_ms: f32,
    scene: Result<SoundingScene, String>,
}

struct SoundingScene {
    column: SoundingColumn,
    profile: sharppyrs::Profile,
    derived: sharppyrs::DerivedParams,
    build_ms: f32,
}

/// Serializable host state for the user-editable sharppyrs panel layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SoundingViewState {
    #[serde(default)]
    sharppyrs_layout: Option<String>,
}

/// Point-sounding inspector. Hosts push loading/data/error state into the
/// panel; all rendering remains a pure egui widget.
pub struct SoundingPanel {
    state: SoundingState,
    loading: bool,
    layout_tokens: Option<String>,
    apply_layout_on_next_frame: bool,
}

impl Default for SoundingPanel {
    fn default() -> Self {
        Self {
            state: SoundingState::default(),
            loading: false,
            layout_tokens: None,
            apply_layout_on_next_frame: false,
        }
    }
}

impl SoundingPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_loading(&mut self) {
        if matches!(self.state, SoundingState::Ready(_)) {
            self.loading = true;
        } else {
            self.loading = false;
            self.state = SoundingState::Loading;
        }
    }

    pub fn set_error(&mut self, message: String) {
        self.loading = false;
        self.state = SoundingState::Error(message);
    }

    pub fn set_data(&mut self, data: SoundingData) {
        profile_scope!("sharppyrs_build_scene");
        let build_started = std::time::Instant::now();
        let scene = build_sounding_column(&data)
            .and_then(|column| build_sounding_scene(column, build_started));
        self.loading = false;
        let heading = data.hour.to_string();
        let subheading = sounding_place(&data);
        let read_ms = data.read_ms;
        self.state = SoundingState::Ready(Box::new(ReadySounding {
            data: Some(data),
            heading,
            subheading,
            read_ms,
            scene,
        }));
    }

    /// Install a host-assembled vertical column, used for observed soundings
    /// whose significant levels do not form a regular model isobaric grid.
    pub fn set_native_column(&mut self, data: SoundingData, column: SoundingColumn) {
        profile_scope!("sharppyrs_build_native_column");
        let scene = build_sounding_scene(column, std::time::Instant::now());
        self.loading = false;
        let heading = data.hour.to_string();
        let subheading = sounding_place(&data);
        let read_ms = data.read_ms;
        self.state = SoundingState::Ready(Box::new(ReadySounding {
            data: Some(data),
            heading,
            subheading,
            read_ms,
            scene,
        }));
    }

    /// Install a normalized observed or file-loaded profile. Metadata on the
    /// column drives both the plot title and sharppyrs locator mini-map.
    pub fn set_external_column(
        &mut self,
        column: SoundingColumn,
        heading: impl Into<String>,
        subheading: impl Into<String>,
        read_ms: f32,
    ) {
        profile_scope!("sharppyrs_build_external_column");
        let scene = build_sounding_scene(column, std::time::Instant::now());
        self.loading = false;
        self.state = SoundingState::Ready(Box::new(ReadySounding {
            data: None,
            heading: heading.into(),
            subheading: subheading.into(),
            read_ms,
            scene,
        }));
    }

    /// `(profile read ms, sharppyrs analysis build ms)` for the visible
    /// sounding, used by the app's lightweight stats strip.
    pub fn last_timings(&self) -> Option<(f32, f32)> {
        match &self.state {
            SoundingState::Ready(ready) => Some((
                ready.read_ms,
                ready
                    .scene
                    .as_ref()
                    .map(|scene| scene.build_ms)
                    .unwrap_or(0.0),
            )),
            _ => None,
        }
    }

    pub fn clear(&mut self) {
        self.loading = false;
        self.state = SoundingState::Empty;
    }

    pub fn view_state(&self) -> SoundingViewState {
        SoundingViewState {
            sharppyrs_layout: self.layout_tokens.clone(),
        }
    }

    pub fn apply_view_state(&mut self, state: SoundingViewState) {
        self.layout_tokens = state
            .sharppyrs_layout
            .filter(|tokens| sharppyrs::SoundingLayout::from_tokens(tokens).is_some());
        self.apply_layout_on_next_frame = self.layout_tokens.is_some();
    }

    pub fn view_state_json(&self) -> serde_json::Value {
        serde_json::to_value(self.view_state()).unwrap_or(serde_json::Value::Null)
    }

    pub fn apply_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        let Ok(state) = serde_json::from_value::<SoundingViewState>(value.clone()) else {
            return false;
        };
        self.apply_view_state(state);
        true
    }

    pub fn has_content(&self) -> bool {
        self.loading || !matches!(self.state, SoundingState::Empty)
    }

    pub fn ui(&mut self, ui: &mut Ui) {
        if self.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(RichText::new("refreshing sounding…").small().weak());
            });
        }

        match &mut self.state {
            SoundingState::Empty => {
                ui.label(RichText::new("Click a point on the field to pull a sounding.").weak());
            }
            SoundingState::Loading => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("reading profiles…");
                });
            }
            SoundingState::Error(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message.as_str());
            }
            SoundingState::Ready(ready) => show_sounding(
                ui,
                ready,
                &mut self.layout_tokens,
                &mut self.apply_layout_on_next_frame,
            ),
        }
    }
}

fn build_sounding_scene(
    column: SoundingColumn,
    build_started: std::time::Instant,
) -> Result<SoundingScene, String> {
    column.validate().map_err(|err| err.to_string())?;

    let wind_speed_kt: Vec<f64> = column
        .u_ms
        .iter()
        .zip(&column.v_ms)
        .map(|(u, v)| u.hypot(*v) * MS_TO_KT)
        .collect();
    let wind_direction_deg: Vec<f64> = column
        .u_ms
        .iter()
        .zip(&column.v_ms)
        .map(|(u, v)| (-u).atan2(-v).to_degrees().rem_euclid(360.0))
        .collect();
    let profile = sharppyrs::Profile::new(sharppyrs::SoundingData {
        pres: column.pressure_hpa.clone(),
        hght: column.height_m_msl.clone(),
        tmpc: column.temperature_c.clone(),
        dwpc: column.dewpoint_c.clone(),
        wdir: wind_direction_deg,
        wspd: wind_speed_kt,
        omeg: (!column.omega_pa_s.is_empty()).then(|| column.omega_pa_s.clone()),
        latitude: column.metadata.latitude_deg,
        longitude: column.metadata.longitude_deg,
        missing: None,
    })
    .ok_or_else(|| "sharppyrs rejected the sounding profile".to_string())?;
    let derived = sharppyrs::DerivedParams::compute(&profile);

    Ok(SoundingScene {
        column,
        profile,
        derived,
        build_ms: build_started.elapsed().as_secs_f32() * 1000.0,
    })
}

fn show_sounding(
    ui: &mut Ui,
    ready: &mut ReadySounding,
    layout_tokens: &mut Option<String>,
    apply_layout_on_next_frame: &mut bool,
) {
    ui.label(RichText::new(&ready.heading).strong());
    ui.label(RichText::new(&ready.subheading).small().weak());

    // Size the plot against the actual viewport before entering the scroll
    // area.  A fixed 1100 px canvas fits tall desktops, but on shorter
    // ultrawide displays it leaves the diagnostics board below the visible
    // edge.  Preserve the board's full desktop width while allowing a modest
    // vertical compression; genuinely short windows still scroll.
    let viewport_height = ui.available_height();
    let canvas_height = (viewport_height - 24.0).clamp(900.0, 1100.0);

    egui::ScrollArea::both()
        .id_salt("rw-ui-sounding-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            match &ready.scene {
                Ok(scene) => {
                    let layout_id = ui.id().with("rw-ui-sharppyrs-layout");
                    if *apply_layout_on_next_frame {
                        if let Some(layout) = layout_tokens
                            .as_deref()
                            .and_then(sharppyrs::SoundingLayout::from_tokens)
                        {
                            sharppyrs::store_layout(ui.ctx(), layout_id, &layout);
                        }
                        *apply_layout_on_next_frame = false;
                    }

                    // Keep the complete diagnostics board in a desktop-width
                    // coordinate system. Narrow hosts scroll it instead of
                    // squeezing fixed text columns over the plot.
                    let width = ui.available_width().max(1630.0);
                    let height = canvas_height;
                    let title = format!(
                        "{}  {}",
                        scene.column.metadata.station_id, scene.column.metadata.valid_time
                    );
                    ui.add(
                        sharppyrs::SoundingView::new(&scene.profile, &scene.derived)
                            .title(title)
                            .brand("rusty-weather · sharppyrs")
                            .style(sharppyrs::SkewTStyle::space_grotesk())
                            .layout_memory_id(layout_id)
                            .size(Vec2::new(width, height)),
                    );
                    if let Some(layout) = sharppyrs::stored_layout(ui.ctx(), layout_id) {
                        *layout_tokens = Some(layout.to_tokens());
                    }
                    ui.label(
                        RichText::new(format!(
                            "profile read {:.0} ms / sharppyrs analysis {:.0} ms",
                            ready.read_ms, scene.build_ms
                        ))
                        .small()
                        .weak(),
                    );
                }
                Err(message) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("sounding unavailable: {message}"),
                    );
                    ui.label(RichText::new("Raw per-level values below.").small().weak());
                }
            }

            if let Some(data) = &ready.data {
                ui.separator();
                CollapsingHeader::new("Level table")
                    .id_salt("rw-ui-sounding-levels")
                    .default_open(ready.scene.is_err())
                    .show(ui, |ui| show_level_table(ui, data));
            }
        });
}

fn sounding_place(data: &SoundingData) -> String {
    match (data.lat, data.lon) {
        (Some(lat), Some(lon)) => format!(
            "{lat:.3}°, {lon:.3}°  (grid {:.1}, {:.1})",
            data.fx, data.fy
        ),
        _ => format!("grid ({:.1}, {:.1})", data.fx, data.fy),
    }
}

/// Numeric table: rows are the union of pressure levels and columns are the
/// raw store variables. Values intentionally remain in store-native units.
fn show_level_table(ui: &mut Ui, data: &SoundingData) {
    if data.vars.is_empty() {
        ui.label(RichText::new("This timestep has no 3D pressure-level variables.").weak());
        return;
    }
    let mut levels: Vec<u16> = data
        .vars
        .iter()
        .flat_map(|var| var.levels_hpa.iter().copied())
        .collect();
    levels.sort_unstable_by(|a, b| b.cmp(a));
    levels.dedup();

    egui::ScrollArea::horizontal()
        .id_salt("rw-ui-sounding-table-scroll")
        .show(ui, |ui| {
            egui::Grid::new("rw-ui-sounding-table")
                .striped(true)
                .min_col_width(56.0)
                .show(ui, |ui| {
                    ui.strong("hPa");
                    for var in &data.vars {
                        ui.strong(&var.name)
                            .on_hover_text(format!("units: {}", var.units));
                    }
                    ui.end_row();
                    for &level in &levels {
                        ui.label(level.to_string());
                        for var in &data.vars {
                            let value = var
                                .levels_hpa
                                .iter()
                                .position(|&have| have == level)
                                .map(|index| var.values[index]);
                            match value {
                                Some(value) if value.is_finite() => ui.label(format!("{value:.1}")),
                                Some(_) => ui.label("—"),
                                None => ui.label(""),
                            };
                        }
                        ui.end_row();
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::worker::HourKey;

    fn empty_sounding() -> SoundingData {
        SoundingData {
            hour: HourKey {
                model: "synthetic".into(),
                run: "20260609_00z".into(),
                hour: 0,
                exact_time: None,
            },
            fx: 1.0,
            fy: 2.0,
            lat: Some(31.0),
            lon: Some(-100.0),
            vars: vec![],
            surface: vec![],
            read_ms: 0.0,
        }
    }

    #[test]
    fn sounding_view_state_json_round_trips_sharppyrs_layout() {
        let mut panel = SoundingPanel::new();
        panel.layout_tokens = Some(sharppyrs::SoundingLayout::default().to_tokens());
        let value = panel.view_state_json();

        let mut restored = SoundingPanel::new();
        assert!(restored.apply_view_state_json(&value));
        assert_eq!(restored.layout_tokens, panel.layout_tokens);
        assert!(restored.apply_layout_on_next_frame);
    }

    #[test]
    fn invalid_layout_tokens_are_discarded() {
        let mut panel = SoundingPanel::new();
        panel.apply_view_state(SoundingViewState {
            sharppyrs_layout: Some("not-a-layout".into()),
        });
        assert_eq!(panel.layout_tokens, None);
        assert!(!panel.apply_layout_on_next_frame);
    }

    #[test]
    fn validated_column_builds_a_sharppyrs_scene() {
        let count = 12;
        let scene = build_sounding_scene(
            SoundingColumn {
                pressure_hpa: vec![
                    1000.0, 925.0, 850.0, 700.0, 600.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0,
                    100.0,
                ],
                height_m_msl: vec![
                    100.0, 800.0, 1500.0, 2500.0, 3500.0, 5000.0, 6500.0, 8000.0, 9500.0, 11000.0,
                    13000.0, 16000.0,
                ],
                temperature_c: vec![
                    25.0, 20.0, 15.0, 4.0, -5.0, -15.0, -28.0, -43.0, -50.0, -56.0, -62.0, -68.0,
                ],
                dewpoint_c: vec![
                    20.0, 16.0, 10.0, -2.0, -10.0, -22.0, -35.0, -50.0, -58.0, -65.0, -70.0, -75.0,
                ],
                u_ms: vec![8.0; count],
                v_ms: vec![4.0; count],
                omega_pa_s: vec![0.0; count],
                metadata: rustwx_sounding::SoundingMetadata {
                    latitude_deg: Some(35.0),
                    longitude_deg: Some(-97.0),
                    ..Default::default()
                },
            },
            std::time::Instant::now(),
        )
        .expect("valid normalized columns should feed sharppyrs");

        assert_eq!(scene.profile.inner.num_levels(), count);
        assert!(scene.build_ms.is_finite());
    }

    #[test]
    fn has_content_tracks_state() {
        let mut panel = SoundingPanel::new();
        assert!(!panel.has_content());
        panel.set_loading();
        assert!(panel.has_content());
        panel.set_data(empty_sounding());
        assert!(panel.has_content());
        panel.clear();
        assert!(!panel.has_content());
    }

    #[test]
    fn loading_next_profile_keeps_existing_ready_scene() {
        let mut panel = SoundingPanel::new();
        panel.set_data(empty_sounding());
        assert!(matches!(panel.state, SoundingState::Ready(_)));
        panel.set_loading();
        assert!(panel.loading);
        assert!(matches!(panel.state, SoundingState::Ready(_)));
    }

    #[test]
    fn missing_skewt_inputs_keep_a_table_only_error() {
        let mut panel = SoundingPanel::new();
        panel.set_data(empty_sounding());
        match &panel.state {
            SoundingState::Ready(ready) => match &ready.scene {
                Err(error) => assert!(error.contains("temperature_iso"), "got: {error}"),
                Ok(_) => panic!("no inputs must not produce a sounding"),
            },
            _ => panic!("set_data must land in Ready"),
        }
    }
}

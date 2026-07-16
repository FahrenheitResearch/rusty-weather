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

#[path = "sounding_table_config.rs"]
mod sounding_table_config;

use self::sounding_table_config::{
    SoundingTableConfig, SoundingTableEditor, config_from_view_state, write_config_to_view_state,
};

#[path = "sounding_table_builtin.rs"]
mod sounding_table_builtin;

const MS_TO_KT: f64 = 1.943_844_492_440_604_6;
const SHARPPY_CANVAS_MIN_WIDTH: f32 = 1_630.0;
const SHARPPY_CANVAS_MIN_HEIGHT: f32 = 900.0;
const SHARPPY_TEXT_SCALE_MIN: f32 = 0.5;
const SHARPPY_TEXT_SCALE_MAX: f32 = 2.0;
const SHARPPY_TEXT_SCALE_DEFAULT: f32 = 1.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SoundingFontChoice {
    #[default]
    SpaceGrotesk,
    CleanSans,
    TechnicalMono,
}

impl SoundingFontChoice {
    const ALL: [Self; 3] = [Self::SpaceGrotesk, Self::CleanSans, Self::TechnicalMono];

    fn key(self) -> &'static str {
        match self {
            Self::SpaceGrotesk => "space-grotesk",
            Self::CleanSans => "clean-sans",
            Self::TechnicalMono => "technical-mono",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "space-grotesk" | "space_grotesk" | "spacegrotesk" => Some(Self::SpaceGrotesk),
            "clean-sans" | "clean_sans" | "proportional" => Some(Self::CleanSans),
            "technical-mono" | "technical_mono" | "monospace" => Some(Self::TechnicalMono),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::SpaceGrotesk => "Space Grotesk",
            Self::CleanSans => "Clean Sans",
            Self::TechnicalMono => "Technical Mono",
        }
    }

    fn preset(self) -> sharppyrs::SoundingFontPreset {
        match self {
            Self::SpaceGrotesk => sharppyrs::SoundingFontPreset::SpaceGrotesk,
            Self::CleanSans => sharppyrs::SoundingFontPreset::CleanProportional,
            Self::TechnicalMono => sharppyrs::SoundingFontPreset::TechnicalMonospace,
        }
    }
}

/// A host-provided scalar that can be placed into any customizable sounding
/// table cell. The host owns evaluation and source matching; the sounding
/// panel only formats and renders the resolved value.
#[derive(Debug, Clone, PartialEq)]
pub struct SoundingFormulaDiagnostic {
    pub id: String,
    pub label: String,
    pub units: String,
    pub source_hour: crate::worker::HourKey,
    pub value: Option<f64>,
    pub unavailable_reason: Option<String>,
}

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
    analysis: SharppyAnalysis,
    build_ms: f32,
}

struct SharppyAnalysis {
    prof: sharppyrs::Profile,
    derived: sharppyrs::DerivedParams,
}

/// Serializable host state for the user-editable sharppyrs panel layout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SoundingViewState {
    #[serde(default)]
    sharppyrs_layout: Option<String>,
    #[serde(default)]
    stretch: Option<bool>,
    #[serde(default)]
    font_preset: Option<String>,
    #[serde(default)]
    text_scale: Option<f32>,
}

/// Point-sounding inspector. Hosts push loading/data/error state into the
/// panel; all rendering remains a pure egui widget.
pub struct SoundingPanel {
    state: SoundingState,
    loading: bool,
    /// JSON members not owned by this build. Keeping them beside the typed
    /// state makes an older rw-ui a lossless pass-through for presentation
    /// controls introduced by a newer release.
    preserved_view_members: serde_json::Map<String, serde_json::Value>,
    layout_tokens: Option<String>,
    apply_layout_on_next_frame: bool,
    stretch: bool,
    font_choice: SoundingFontChoice,
    text_scale: f32,
    table_config: SoundingTableConfig,
    table_editor: SoundingTableEditor,
    formula_diagnostic: Option<SoundingFormulaDiagnostic>,
}

impl Default for SoundingPanel {
    fn default() -> Self {
        Self {
            state: SoundingState::default(),
            loading: false,
            preserved_view_members: serde_json::Map::new(),
            layout_tokens: None,
            apply_layout_on_next_frame: false,
            stretch: true,
            font_choice: SoundingFontChoice::default(),
            text_scale: SHARPPY_TEXT_SCALE_DEFAULT,
            table_config: SoundingTableConfig::default(),
            table_editor: SoundingTableEditor::default(),
            formula_diagnostic: None,
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
        self.retain_formula_for_hour(&data.hour);
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
        self.retain_formula_for_hour(&data.hour);
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
        self.formula_diagnostic = None;
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
        self.formula_diagnostic = None;
    }

    /// Supply the latest Formula Lab scalar for this panel. Results belonging
    /// to another model/run/hour are ignored, and all external observed/file
    /// profiles reject formula rows rather than presenting stale values.
    pub fn set_formula_diagnostic(&mut self, diagnostic: Option<SoundingFormulaDiagnostic>) {
        self.formula_diagnostic = diagnostic.filter(|candidate| {
            matches!(
                &self.state,
                SoundingState::Ready(ready)
                    if ready.data.as_ref().is_some_and(|data| data.hour == candidate.source_hour)
            )
        });
    }

    /// Sample a completed Formula Lab field at the exact model-sounding
    /// location and expose it to the table editor. This keeps formula values
    /// in their scientific units and refuses stale results from another
    /// timestep.
    pub fn install_formula_field(
        &mut self,
        field: &crate::worker::FieldData,
        label: impl Into<String>,
    ) -> bool {
        self.install_formula_field_with_id(
            field,
            format!("formula_lab:legacy:{}", field.key.var),
            label,
        )
    }

    /// Install a Formula Lab field using a host-provided, content-aware
    /// identity. The identity must describe the formula science rather than
    /// the viewer's mutable display-variable name, which may be renamed to
    /// avoid a collision with a stored field.
    pub fn install_formula_field_with_id(
        &mut self,
        field: &crate::worker::FieldData,
        stable_id: impl Into<String>,
        label: impl Into<String>,
    ) -> bool {
        let Some(data) = (match &self.state {
            SoundingState::Ready(ready) => ready.data.as_ref(),
            _ => None,
        }) else {
            return false;
        };
        if data.hour != field.key.hour {
            return false;
        }
        let value = sample_field_at(field, data.fx, data.fy);
        self.formula_diagnostic = Some(SoundingFormulaDiagnostic {
            id: stable_id.into(),
            label: label.into(),
            units: field.units.clone(),
            source_hour: data.hour.clone(),
            value,
            unavailable_reason: value
                .is_none()
                .then_some("Formula Lab returned no finite value at this sounding".to_owned()),
        });
        true
    }

    fn retain_formula_for_hour(&mut self, hour: &crate::worker::HourKey) {
        if self
            .formula_diagnostic
            .as_ref()
            .is_some_and(|formula| &formula.source_hour != hour)
        {
            self.formula_diagnostic = None;
        }
    }

    pub fn view_state(&self) -> SoundingViewState {
        SoundingViewState {
            sharppyrs_layout: self.layout_tokens.clone(),
            stretch: Some(self.stretch),
            font_preset: Some(self.font_choice.key().to_owned()),
            text_scale: Some(self.text_scale),
        }
    }

    pub fn apply_view_state(&mut self, state: SoundingViewState) {
        self.layout_tokens = state
            .sharppyrs_layout
            .filter(|tokens| sharppyrs::SoundingLayout::from_tokens(tokens).is_some());
        self.apply_layout_on_next_frame = self.layout_tokens.is_some();
        if let Some(stretch) = state.stretch {
            self.stretch = stretch;
        }
        if let Some(choice) = state
            .font_preset
            .as_deref()
            .and_then(SoundingFontChoice::from_key)
        {
            self.font_choice = choice;
        }
        if let Some(scale) = state.text_scale.filter(|scale| scale.is_finite()) {
            self.text_scale = scale.clamp(SHARPPY_TEXT_SCALE_MIN, SHARPPY_TEXT_SCALE_MAX);
        }
    }

    pub fn view_state_json(&self) -> serde_json::Value {
        let mut object = self.preserved_view_members.clone();
        if let Ok(serde_json::Value::Object(owned)) = serde_json::to_value(self.view_state()) {
            object.extend(owned);
        }
        let mut value = serde_json::Value::Object(object);
        let _ = write_config_to_view_state(&mut value, &self.table_config);
        value
    }

    pub fn apply_view_state_json(&mut self, value: &serde_json::Value) -> bool {
        let Ok(state) = serde_json::from_value::<SoundingViewState>(value.clone()) else {
            return false;
        };
        let Some(object) = value.as_object() else {
            return false;
        };
        self.preserved_view_members = object.clone();
        self.table_config = config_from_view_state(value).unwrap_or_default();
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
                &mut self.stretch,
                &mut self.font_choice,
                &mut self.text_scale,
                &mut self.table_config,
                &mut self.table_editor,
                self.formula_diagnostic.as_ref(),
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
        analysis: SharppyAnalysis {
            prof: profile,
            derived,
        },
        build_ms: build_started.elapsed().as_secs_f32() * 1000.0,
    })
}

fn show_sounding(
    ui: &mut Ui,
    ready: &mut ReadySounding,
    layout_tokens: &mut Option<String>,
    apply_layout_on_next_frame: &mut bool,
    stretch: &mut bool,
    font_choice: &mut SoundingFontChoice,
    text_scale: &mut f32,
    table_config: &mut SoundingTableConfig,
    table_editor: &mut SoundingTableEditor,
    formula: Option<&SoundingFormulaDiagnostic>,
) {
    ui.label(RichText::new(&ready.heading).strong());
    ui.label(RichText::new(&ready.subheading).small().weak());

    if ready.scene.is_ok() {
        ui.horizontal_wrapped(|ui| {
            ui.menu_button("Text", |ui| {
                ui.set_min_width(230.0);
                ui.label(RichText::new("FONT").small().strong());
                for choice in SoundingFontChoice::ALL {
                    ui.selectable_value(font_choice, choice, choice.label());
                }
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Size");
                    let mut percent = *text_scale * 100.0;
                    if ui
                        .add(
                            egui::Slider::new(&mut percent, 50.0..=200.0)
                                .suffix("%")
                                .integer(),
                        )
                        .changed()
                    {
                        *text_scale =
                            (percent / 100.0).clamp(SHARPPY_TEXT_SCALE_MIN, SHARPPY_TEXT_SCALE_MAX);
                    }
                    if ui
                        .small_button("Reset")
                        .on_hover_text("Reset size to 100%")
                        .clicked()
                    {
                        *text_scale = SHARPPY_TEXT_SCALE_DEFAULT;
                    }
                });
                ui.weak("Changes sounding text only; panel geometry stays independent.");
            })
            .response
            .on_hover_text("Sounding font family and independent text size");
            ui.separator();
            table_editor.header_button(ui, table_config);
            ui.separator();
            ui.weak("Pane");
            ui.selectable_value(stretch, true, "Stretch")
                .on_hover_text("Fill the sounding workspace in both directions.");
            ui.selectable_value(stretch, false, "Fit")
                .on_hover_text("Preserve the desktop sounding-board aspect ratio.");
        });

        let catalog = sounding_table_builtin::catalog(formula);
        let defaults = sounding_table_builtin::default_config();
        table_editor.show(ui.ctx(), table_config, &defaults, &catalog);
    }

    let canvas_viewport = ui.available_size();
    egui::ScrollArea::both()
        .id_salt("rw-ui-sounding-scroll")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            match &ready.scene {
                Ok(scene) => {
                    let layout_id = egui::Id::new("rw-ui-sharppyrs-layout");
                    if *apply_layout_on_next_frame {
                        if let Some(layout) = layout_tokens
                            .as_deref()
                            .and_then(sharppyrs::SoundingLayout::from_tokens)
                        {
                            sharppyrs::store_layout(ui.ctx(), layout_id, &layout);
                        }
                        *apply_layout_on_next_frame = false;
                    }

                    let size = sounding_canvas_size(canvas_viewport, *stretch);
                    let title = format!(
                        "{}  {}",
                        scene.column.metadata.station_id, scene.column.metadata.valid_time
                    );
                    let overrides =
                        sounding_table_builtin::build_board(table_config, &scene.analysis, formula);
                    let style = sharppyrs::SkewTStyle::space_grotesk()
                        .with_font_preset(font_choice.preset())
                        .with_text_scale(*text_scale);
                    let view = || {
                        let view = sharppyrs::SoundingView::new(
                            &scene.analysis.prof,
                            &scene.analysis.derived,
                        )
                        .title(title)
                        .brand("rusty-weather / sharppyrs")
                        .style(style)
                        .layout_memory_id(layout_id)
                        .size(size);
                        let Some(overrides) = overrides.as_ref() else {
                            return view;
                        };
                        let view = if overrides.generic.panels.is_empty() {
                            view
                        } else {
                            view.diagnostic_tables(&overrides.generic)
                        };
                        if overrides.native_patches.patches.is_empty() {
                            view
                        } else {
                            view.native_diagnostic_patches(&overrides.native_patches)
                        }
                    };
                    egui::Frame::new()
                        .fill(egui::Color32::BLACK)
                        .show(ui, |ui| {
                            ui.add(view());
                        });
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

fn fit_sounding_canvas_size(viewport: Vec2) -> Vec2 {
    let width_scale = viewport.x.max(0.0) / SHARPPY_CANVAS_MIN_WIDTH;
    let height_scale = viewport.y.max(0.0) / SHARPPY_CANVAS_MIN_HEIGHT;
    let scale = width_scale.min(height_scale);
    Vec2::new(
        (SHARPPY_CANVAS_MIN_WIDTH * scale).min(viewport.x),
        (SHARPPY_CANVAS_MIN_HEIGHT * scale).min(viewport.y),
    )
}

fn sounding_canvas_size(viewport: Vec2, stretch: bool) -> Vec2 {
    let viewport = Vec2::new(viewport.x.max(0.0), viewport.y.max(0.0));
    if stretch {
        viewport
    } else {
        fit_sounding_canvas_size(viewport)
    }
}

fn sample_field_at(field: &crate::worker::FieldData, fx: f64, fy: f64) -> Option<f64> {
    if field.nx == 0
        || field.ny == 0
        || field.values.len() != field.nx.saturating_mul(field.ny)
        || !fx.is_finite()
        || !fy.is_finite()
    {
        return None;
    }
    let x = fx.clamp(0.0, field.nx.saturating_sub(1) as f64);
    let y = fy.clamp(0.0, field.ny.saturating_sub(1) as f64);
    let x0 = x.floor() as usize;
    let y0 = y.floor() as usize;
    let x1 = (x0 + 1).min(field.nx - 1);
    let y1 = (y0 + 1).min(field.ny - 1);
    let tx = x - x0 as f64;
    let ty = y - y0 as f64;
    let samples = [
        (x0, y0, (1.0 - tx) * (1.0 - ty)),
        (x1, y0, tx * (1.0 - ty)),
        (x0, y1, (1.0 - tx) * ty),
        (x1, y1, tx * ty),
    ];
    let mut weighted = 0.0;
    let mut weight_sum = 0.0;
    for (x, y, weight) in samples {
        let value = f64::from(field.values[y * field.nx + x]);
        if value.is_finite() && weight > 0.0 {
            weighted += value * weight;
            weight_sum += weight;
        }
    }
    (weight_sum > 0.0).then_some(weighted / weight_sum)
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
        panel.stretch = false;
        panel.font_choice = SoundingFontChoice::TechnicalMono;
        panel.text_scale = 1.4;
        panel.table_config = sounding_table_builtin::default_config();
        let value = panel.view_state_json();

        let mut restored = SoundingPanel::new();
        assert!(restored.apply_view_state_json(&value));
        assert_eq!(restored.layout_tokens, panel.layout_tokens);
        assert!(restored.apply_layout_on_next_frame);
        assert!(!restored.stretch);
        assert_eq!(restored.font_choice, SoundingFontChoice::TechnicalMono);
        assert!((restored.text_scale - 1.4).abs() < f32::EPSILON);
        assert_eq!(restored.table_config, panel.table_config);
    }

    #[test]
    fn sounding_view_state_preserves_unknown_future_members() {
        let future = serde_json::json!({
            "sharppyrs_layout": sharppyrs::SoundingLayout::default().to_tokens(),
            "stretch": false,
            "font_preset": "technical-mono",
            "text_scale": 1.25,
            "future_fit_mode": "adaptive",
            "future_panel_weights": [0.5, 0.2, 0.3],
            "future_nested": {"scene_zoom": 0.72, "locked": true},
        });
        let mut panel = SoundingPanel::new();
        assert!(panel.apply_view_state_json(&future));

        let encoded = panel.view_state_json();
        assert_eq!(encoded["future_fit_mode"], future["future_fit_mode"]);
        assert_eq!(
            encoded["future_panel_weights"],
            future["future_panel_weights"]
        );
        assert_eq!(encoded["future_nested"], future["future_nested"]);

        let mut restored = SoundingPanel::new();
        assert!(restored.apply_view_state_json(&encoded));
        assert_eq!(
            restored.view_state_json()["future_nested"],
            future["future_nested"]
        );
    }

    #[test]
    fn formula_field_samples_the_current_sounding_and_rejects_other_hours() {
        let mut data = empty_sounding();
        data.fx = 0.5;
        data.fy = 0.5;
        let hour = data.hour.clone();
        let mut panel = SoundingPanel::new();
        panel.set_data(data);
        let field = crate::worker::FieldData {
            key: crate::worker::FieldKey {
                hour: hour.clone(),
                var: "custom_hail".to_owned(),
            },
            units: "widgets".to_owned(),
            nx: 2,
            ny: 2,
            values: vec![0.0, 10.0, 20.0, 30.0],
            range: Some((0.0, 30.0)),
            grid: None,
            lat_descending: false,
            style: None,
        };
        assert!(panel.install_formula_field(&field, "Custom hail"));
        let formula = panel.formula_diagnostic.as_ref().expect("formula");
        assert_eq!(formula.source_hour, hour);
        assert_eq!(formula.value, Some(15.0));

        let mut stale = field;
        stale.key.hour.hour = 1;
        assert!(!panel.install_formula_field(&stale, "stale"));
        assert_eq!(
            panel
                .formula_diagnostic
                .as_ref()
                .map(|formula| formula.label.as_str()),
            Some("Custom hail")
        );
    }

    #[test]
    fn invalid_layout_tokens_are_discarded() {
        let mut panel = SoundingPanel::new();
        panel.apply_view_state(SoundingViewState {
            sharppyrs_layout: Some("not-a-layout".into()),
            ..Default::default()
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

        assert_eq!(scene.analysis.prof.inner.num_levels(), count);
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

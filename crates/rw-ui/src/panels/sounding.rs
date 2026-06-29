//! Sounding panel: the production skew-T at a clicked point.
//!
//! The primary view is a SHARPpy-style sounding scene drawn directly with
//! egui painter geometry. The raw per-level numbers stay available in a
//! collapsible table below it. When an hour lacks the sounding inputs (see
//! [`crate::skewt`]), the panel says why and the table remains.

use egui::{
    Align2, CollapsingHeader, Color32, FontId, Pos2, Rect, RichText, Sense, Slider, Stroke, Ui,
    Vec2,
};
use rustwx_sounding::{NativeSounding, SoundingColumn};
use serde::{Deserialize, Serialize};

use crate::profile_scope;
use crate::skewt::build_sounding_column;
use crate::worker::SoundingData;

const FULL_IMAGE_W: f32 = 2400.0;
const FULL_IMAGE_H: f32 = 1800.0;
const TITLE_H: f32 = 44.0;
const SKEWT_LEFT_W: f32 = 1680.0;
const SKEWT_UPPER_H: f32 = 1120.0;
const SKEWT_FRAC: f32 = 1.0;
const SKEWT_MARGIN_LEFT: f32 = 70.0;
const SKEWT_MARGIN_RIGHT: f32 = 55.0;
const SKEWT_MARGIN_TOP: f32 = 28.0;
const SKEWT_MARGIN_BOT: f32 = 38.0;
const SKEWT_P_TOP: f64 = 100.0;
const SKEWT_P_BOT: f64 = 1050.0;
const SKEWT_T_MIN: f64 = -40.0;
const SKEWT_T_MAX: f64 = 50.0;
const SKEWT_SKEW: f64 = 1.0;
const HODO_X: f32 = 1680.0;
const HODO_Y: f32 = TITLE_H;
const HODO_W: f32 = 720.0;
const HODO_H: f32 = 600.0;
const HODO_MAX_RING_KT: f64 = 120.0;
const INSET_X: f32 = HODO_X;
const INSET_Y: f32 = TITLE_H + HODO_H;
const INSET_W: f32 = HODO_W;
const INSET_H: f32 = 520.0;
const MS_TO_KT: f64 = 1.943_844;
const TEXT_SCALE: f32 = 1.22;
const LAYOUT_HANDLE: f32 = 34.0;

#[derive(Default)]
enum SoundingState {
    #[default]
    Empty,
    Loading,
    Error(String),
    Ready(Box<ReadySounding>),
}

struct ReadySounding {
    data: SoundingData,
    /// Native scene, or why it could not be built for this hour/point.
    scene: Result<SoundingScene, String>,
}

struct SoundingScene {
    column: SoundingColumn,
    native: NativeSounding,
    build_ms: f32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SoundingZooms {
    #[serde(default = "default_scene_zoom")]
    scene: f32,
    #[serde(default)]
    skewt: PanelViewport,
    #[serde(default)]
    hodograph: PanelViewport,
    #[serde(default)]
    slinky: PanelViewport,
}

impl Default for SoundingZooms {
    fn default() -> Self {
        Self {
            scene: 1.08,
            skewt: PanelViewport::default(),
            hodograph: PanelViewport::default(),
            slinky: PanelViewport::default(),
        }
    }
}

fn default_scene_zoom() -> f32 {
    SoundingZooms::default().scene
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SoundingTraceStyle {
    Solid,
    Dashed,
    Dotted,
}

impl SoundingTraceStyle {
    const ALL: [Self; 3] = [Self::Solid, Self::Dashed, Self::Dotted];

    fn label(self) -> &'static str {
        match self {
            Self::Solid => "solid",
            Self::Dashed => "dash",
            Self::Dotted => "dot",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SoundingOverlaySettings {
    #[serde(default = "default_true")]
    cape_cin_fill: bool,
    #[serde(default = "default_true")]
    height_markers: bool,
    #[serde(default = "default_true")]
    level_markers: bool,
    #[serde(default = "default_true")]
    wind_barbs: bool,
    #[serde(default = "default_true")]
    wetbulb: bool,
    #[serde(default)]
    wetbulb_style: SoundingTraceStyle,
    #[serde(default = "default_true")]
    ml_parcel: bool,
    #[serde(default = "default_dashed_trace_style")]
    ml_parcel_style: SoundingTraceStyle,
    #[serde(default = "default_true")]
    mu_parcel: bool,
    #[serde(default = "default_dashed_trace_style")]
    mu_parcel_style: SoundingTraceStyle,
    #[serde(default = "default_true")]
    dcape: bool,
    #[serde(default = "default_dashed_trace_style")]
    dcape_style: SoundingTraceStyle,
    #[serde(default = "default_text_scale")]
    text_scale: f32,
    #[serde(default = "default_table_text_scale")]
    table_text_scale: f32,
    #[serde(default = "default_hodo_dot_radius")]
    hodo_dot_radius: f32,
    #[serde(default = "default_slinky_dot_radius")]
    slinky_dot_radius: f32,
}

impl Default for SoundingOverlaySettings {
    fn default() -> Self {
        Self {
            cape_cin_fill: true,
            height_markers: true,
            level_markers: true,
            wind_barbs: true,
            wetbulb: true,
            wetbulb_style: SoundingTraceStyle::Solid,
            ml_parcel: true,
            ml_parcel_style: SoundingTraceStyle::Dashed,
            mu_parcel: true,
            mu_parcel_style: SoundingTraceStyle::Dashed,
            dcape: true,
            dcape_style: SoundingTraceStyle::Dashed,
            text_scale: default_text_scale(),
            table_text_scale: default_table_text_scale(),
            hodo_dot_radius: default_hodo_dot_radius(),
            slinky_dot_radius: default_slinky_dot_radius(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_dashed_trace_style() -> SoundingTraceStyle {
    SoundingTraceStyle::Dashed
}

fn default_text_scale() -> f32 {
    1.0
}

fn default_table_text_scale() -> f32 {
    1.18
}

fn default_hodo_dot_radius() -> f32 {
    4.0
}

fn default_slinky_dot_radius() -> f32 {
    4.0
}

impl Default for SoundingTraceStyle {
    fn default() -> Self {
        Self::Solid
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct PanelViewport {
    #[serde(default = "default_panel_zoom")]
    zoom: f32,
    #[serde(default)]
    pan_x: f32,
    #[serde(default)]
    pan_y: f32,
}

impl Default for PanelViewport {
    fn default() -> Self {
        Self {
            zoom: 1.0,
            pan_x: 0.0,
            pan_y: 0.0,
        }
    }
}

impl PanelViewport {
    fn clamp(&mut self) {
        self.zoom = self.zoom.clamp(0.5, 8.0);
        let limit = 2200.0 * self.zoom.max(1.0);
        self.pan_x = self.pan_x.clamp(-limit, limit);
        self.pan_y = self.pan_y.clamp(-limit, limit);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn transform_around(self, center_x: f32, center_y: f32) -> PanelTransform {
        PanelTransform {
            center_x,
            center_y,
            zoom: self.zoom.clamp(0.5, 8.0),
            pan_x: self.pan_x,
            pan_y: self.pan_y,
        }
    }

    fn transform_from_rect(self, x: f32, y: f32, w: f32, h: f32) -> PanelTransform {
        self.transform_around(x + w / 2.0, y + h / 2.0)
    }

    fn zoom_at(&mut self, cursor_x: f32, cursor_y: f32, factor: f32, center_x: f32, center_y: f32) {
        self.clamp();
        let old = self.transform_around(center_x, center_y);
        let (content_x, content_y) = old.inverse(cursor_x, cursor_y);
        self.zoom = (self.zoom * factor).clamp(0.5, 8.0);
        self.pan_x = cursor_x - center_x - (content_x - center_x) * self.zoom;
        self.pan_y = cursor_y - center_y - (content_y - center_y) * self.zoom;
        self.clamp();
    }

    fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan_x += dx;
        self.pan_y += dy;
        self.clamp();
    }
}

fn default_panel_zoom() -> f32 {
    1.0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct ComponentFrame {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl ComponentFrame {
    const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Self { x, y, w, h }
    }

    fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x && x <= self.x + self.w && y >= self.y && y <= self.y + self.h
    }

    fn resize_handle_contains(self, x: f32, y: f32) -> bool {
        x >= self.x + self.w - LAYOUT_HANDLE
            && x <= self.x + self.w
            && y >= self.y + self.h - LAYOUT_HANDLE
            && y <= self.y + self.h
    }

    fn move_by(&mut self, dx: f32, dy: f32, canvas_w: f32, canvas_h: f32) {
        self.x = (self.x + dx).clamp(0.0, (canvas_w - self.w).max(0.0));
        self.y = (self.y + dy).clamp(0.0, (canvas_h - self.h).max(0.0));
    }

    fn resize_by(&mut self, dw: f32, dh: f32, canvas_w: f32, canvas_h: f32) {
        let max_w = (canvas_w - self.x).max(180.0);
        let max_h = (canvas_h - self.y).max(140.0);
        self.w = (self.w + dw).clamp(180.0, max_w);
        self.h = (self.h + dh).clamp(140.0, max_h);
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct SoundingLayout {
    #[serde(default = "default_canvas_w")]
    canvas_w: f32,
    #[serde(default = "default_canvas_h")]
    canvas_h: f32,
    #[serde(default = "default_title_frame")]
    title: ComponentFrame,
    #[serde(default = "default_skewt_frame")]
    skewt: ComponentFrame,
    #[serde(default = "default_hodograph_frame")]
    hodograph: ComponentFrame,
    #[serde(default = "default_slinky_frame")]
    slinky: ComponentFrame,
    #[serde(default = "default_tables_frame")]
    tables: ComponentFrame,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SoundingViewState {
    #[serde(default)]
    zooms: SoundingZooms,
    #[serde(default)]
    layout: SoundingLayout,
    #[serde(default)]
    overlays: SoundingOverlaySettings,
}

impl Default for SoundingViewState {
    fn default() -> Self {
        Self {
            zooms: SoundingZooms::default(),
            layout: SoundingLayout::default(),
            overlays: SoundingOverlaySettings::default(),
        }
    }
}

fn default_canvas_w() -> f32 {
    FULL_IMAGE_W
}

fn default_canvas_h() -> f32 {
    FULL_IMAGE_H
}

fn default_title_frame() -> ComponentFrame {
    SoundingLayout::wide().title
}

fn default_skewt_frame() -> ComponentFrame {
    SoundingLayout::wide().skewt
}

fn default_hodograph_frame() -> ComponentFrame {
    SoundingLayout::wide().hodograph
}

fn default_slinky_frame() -> ComponentFrame {
    SoundingLayout::wide().slinky
}

fn default_tables_frame() -> ComponentFrame {
    SoundingLayout::wide().tables
}

impl Default for SoundingLayout {
    fn default() -> Self {
        Self::wide()
    }
}

impl SoundingLayout {
    fn wide() -> Self {
        Self {
            canvas_w: FULL_IMAGE_W,
            canvas_h: FULL_IMAGE_H,
            title: ComponentFrame::new(0.0, 0.0, FULL_IMAGE_W, TITLE_H),
            skewt: ComponentFrame::new(0.0, TITLE_H, SKEWT_LEFT_W, SKEWT_UPPER_H),
            hodograph: ComponentFrame::new(HODO_X, HODO_Y, HODO_W, HODO_H),
            slinky: ComponentFrame::new(INSET_X, INSET_Y, INSET_W, INSET_H),
            tables: ComponentFrame::new(
                0.0,
                TITLE_H + SKEWT_UPPER_H,
                FULL_IMAGE_W,
                FULL_IMAGE_H - TITLE_H - SKEWT_UPPER_H,
            ),
        }
    }

    fn compact() -> Self {
        Self {
            canvas_w: FULL_IMAGE_W,
            canvas_h: 1220.0,
            title: ComponentFrame::new(0.0, 0.0, FULL_IMAGE_W, TITLE_H),
            skewt: ComponentFrame::new(0.0, TITLE_H + 8.0, 1530.0, 760.0),
            hodograph: ComponentFrame::new(1548.0, TITLE_H + 8.0, 410.0, 410.0),
            slinky: ComponentFrame::new(1976.0, TITLE_H + 8.0, 410.0, 410.0),
            tables: ComponentFrame::new(0.0, 830.0, FULL_IMAGE_W, 390.0),
        }
    }

    fn frame(self, region: SoundingPanelRegion) -> ComponentFrame {
        match region {
            SoundingPanelRegion::Skewt => self.skewt,
            SoundingPanelRegion::Hodograph => self.hodograph,
            SoundingPanelRegion::Slinky => self.slinky,
        }
    }

    fn frame_mut(&mut self, region: SoundingPanelRegion) -> &mut ComponentFrame {
        match region {
            SoundingPanelRegion::Skewt => &mut self.skewt,
            SoundingPanelRegion::Hodograph => &mut self.hodograph,
            SoundingPanelRegion::Slinky => &mut self.slinky,
        }
    }

    fn region_at(self, x: f32, y: f32) -> Option<SoundingPanelRegion> {
        if self.hodograph.contains(x, y) {
            Some(SoundingPanelRegion::Hodograph)
        } else if self.slinky.contains(x, y) {
            Some(SoundingPanelRegion::Slinky)
        } else if self.skewt.contains(x, y) {
            Some(SoundingPanelRegion::Skewt)
        } else {
            None
        }
    }

    fn source_frame(region: SoundingPanelRegion) -> ComponentFrame {
        match region {
            SoundingPanelRegion::Skewt => {
                ComponentFrame::new(0.0, TITLE_H, SKEWT_LEFT_W, SKEWT_UPPER_H)
            }
            SoundingPanelRegion::Hodograph => ComponentFrame::new(HODO_X, HODO_Y, HODO_W, HODO_H),
            SoundingPanelRegion::Slinky => ComponentFrame::new(INSET_X, INSET_Y, INSET_W, INSET_H),
        }
    }

    fn layout_to_source(self, region: SoundingPanelRegion, x: f32, y: f32) -> (f32, f32) {
        let dest = self.frame(region);
        let source = Self::source_frame(region);
        (
            source.x + ((x - dest.x) / dest.w.max(1.0)) * source.w,
            source.y + ((y - dest.y) / dest.h.max(1.0)) * source.h,
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct SceneCanvas {
    screen: Rect,
    layout_w: f32,
    layout_h: f32,
    source: ComponentFrame,
    dest: ComponentFrame,
    text_scale: f32,
}

impl SceneCanvas {
    fn new(
        screen: Rect,
        layout: &SoundingLayout,
        source: ComponentFrame,
        dest: ComponentFrame,
    ) -> Self {
        Self {
            screen,
            layout_w: layout.canvas_w,
            layout_h: layout.canvas_h,
            source,
            dest,
            text_scale: 1.0,
        }
    }

    fn with_text_scale(mut self, text_scale: f32) -> Self {
        self.text_scale = text_scale.clamp(0.65, 2.2);
        self
    }

    fn map_pos(self, x: f32, y: f32) -> Pos2 {
        let local_x = self.dest.x + ((x - self.source.x) / self.source.w.max(1.0)) * self.dest.w;
        let local_y = self.dest.y + ((y - self.source.y) / self.source.h.max(1.0)) * self.dest.h;
        Pos2::new(
            self.screen.left() + local_x / self.layout_w.max(1.0) * self.screen.width(),
            self.screen.top() + local_y / self.layout_h.max(1.0) * self.screen.height(),
        )
    }

    fn map_rect(self, x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect::from_min_max(self.map_pos(x, y), self.map_pos(x + w, y + h))
    }

    fn dest_rect(self) -> Rect {
        self.map_rect(self.source.x, self.source.y, self.source.w, self.source.h)
    }

    fn scale(self) -> f32 {
        let sx =
            self.screen.width() / self.layout_w.max(1.0) * self.dest.w / self.source.w.max(1.0);
        let sy =
            self.screen.height() / self.layout_h.max(1.0) * self.dest.h / self.source.h.max(1.0);
        sx.min(sy)
    }
}

/// Point-sounding inspector. Pure widget over host-pushed data:
/// `set_loading` -> `set_data`/`set_error`, render with `ui`.
pub struct SoundingPanel {
    state: SoundingState,
    zooms: SoundingZooms,
    layout: SoundingLayout,
    overlays: SoundingOverlaySettings,
    loading: bool,
    edit_layout: bool,
}

impl Default for SoundingPanel {
    fn default() -> Self {
        Self {
            state: SoundingState::default(),
            zooms: SoundingZooms::default(),
            layout: SoundingLayout::default(),
            overlays: SoundingOverlaySettings::default(),
            loading: false,
            edit_layout: false,
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

    /// Install a loaded sounding. Builds the native sounding and renders
    /// the skew-T image here — once per click, not per frame. (The GPU
    /// upload happens on the next `ui` call, which has the `Context`.)
    pub fn set_data(&mut self, data: SoundingData) {
        profile_scope!("skewt_build_scene");
        let build_start = std::time::Instant::now();
        let scene = build_sounding_column(&data).and_then(|column| {
            let native = NativeSounding::from_column(&column).map_err(|err| err.to_string())?;
            Ok(SoundingScene {
                column,
                native,
                build_ms: build_start.elapsed().as_secs_f32() * 1000.0,
            })
        });
        self.loading = false;
        self.state = SoundingState::Ready(Box::new(ReadySounding { data, scene }));
    }

    /// Install a sounding whose exact vertical column was assembled by the
    /// host, while still carrying [`SoundingData`] for labels, hovers, and
    /// the level table. Used for observed RAOBs where significant levels are
    /// not a regular model isobaric grid.
    pub fn set_native_column(&mut self, data: SoundingData, column: SoundingColumn) {
        profile_scope!("skewt_build_scene_native_column");
        let build_start = std::time::Instant::now();
        let scene = NativeSounding::from_column(&column)
            .map(|native| SoundingScene {
                column,
                native,
                build_ms: build_start.elapsed().as_secs_f32() * 1000.0,
            })
            .map_err(|err| err.to_string());
        self.loading = false;
        self.state = SoundingState::Ready(Box::new(ReadySounding { data, scene }));
    }

    /// `(profile read ms, scene build ms)` of the last loaded sounding,
    /// when one is showing (stats strip).
    pub fn last_timings(&self) -> Option<(f32, f32)> {
        match &self.state {
            SoundingState::Ready(ready) => Some((
                ready.data.read_ms,
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
            zooms: self.zooms,
            layout: self.layout,
            overlays: self.overlays,
        }
    }

    pub fn apply_view_state(&mut self, state: SoundingViewState) {
        self.zooms = state.zooms;
        self.zooms.scene = self.zooms.scene.clamp(0.35, 3.0);
        self.zooms.skewt.clamp();
        self.zooms.hodograph.clamp();
        self.zooms.slinky.clamp();
        self.layout = state.layout;
        self.layout.canvas_w = self.layout.canvas_w.clamp(900.0, 3600.0);
        self.layout.canvas_h = self.layout.canvas_h.clamp(700.0, 2600.0);
        self.overlays = state.overlays;
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

    /// Whether the panel has anything to show (host can hide it otherwise).
    pub fn has_content(&self) -> bool {
        self.loading || !matches!(self.state, SoundingState::Empty)
    }

    pub fn ui(&mut self, ui: &mut Ui) {
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
                &mut self.zooms,
                &mut self.layout,
                &mut self.overlays,
                &mut self.edit_layout,
            ),
        }
    }
}

fn show_sounding(
    ui: &mut Ui,
    ready: &mut ReadySounding,
    zooms: &mut SoundingZooms,
    layout: &mut SoundingLayout,
    overlays: &mut SoundingOverlaySettings,
    edit_layout: &mut bool,
) {
    let data = &ready.data;
    ui.label(RichText::new(format!("{}", data.hour)).strong());
    let place = match (data.lat, data.lon) {
        (Some(lat), Some(lon)) => {
            format!(
                "{lat:.3}°, {lon:.3}°  (grid {:.1}, {:.1})",
                data.fx, data.fy
            )
        }
        _ => format!("grid ({:.1}, {:.1})", data.fx, data.fy),
    };
    ui.label(RichText::new(place).small().weak());

    egui::ScrollArea::vertical()
        .id_salt("rw-ui-sounding-scroll")
        .show(ui, |ui| {
            match &ready.scene {
                Ok(scene) => {
                    show_zoom_controls(ui, zooms, layout, edit_layout);
                    show_overlay_controls(ui, overlays);
                    show_sounding_scene(ui, scene, zooms, layout, overlays, *edit_layout, data);
                    ui.label(
                        RichText::new(format!(
                            "profile read {:.0} ms / scene build {:.0} ms",
                            data.read_ms, scene.build_ms
                        ))
                        .small()
                        .weak(),
                    );
                }
                Err(message) => {
                    ui.colored_label(
                        ui.visuals().error_fg_color,
                        format!("skew-T unavailable: {message}"),
                    );
                    ui.label(RichText::new("Raw per-level values below.").small().weak());
                }
            }

            ui.separator();
            CollapsingHeader::new("Level table")
                .id_salt("rw-ui-sounding-levels")
                .default_open(ready.scene.is_err())
                .show(ui, |ui| {
                    show_level_table(ui, data, overlays.table_text_scale)
                });
        });
}

fn show_zoom_controls(
    ui: &mut Ui,
    zooms: &mut SoundingZooms,
    layout: &mut SoundingLayout,
    edit_layout: &mut bool,
) {
    zooms.scene = zooms.scene.clamp(0.5, 3.0);
    zooms.skewt.clamp();
    zooms.hodograph.clamp();
    zooms.slinky.clamp();

    ui.horizontal_wrapped(|ui| {
        ui.checkbox(edit_layout, "Edit layout")
            .on_hover_text("drag a section to move it; drag its lower-right handle to resize it");
        zoom_slider(ui, "Canvas", &mut zooms.scene, 0.35..=3.0);
        if ui
            .button("Reset view")
            .on_hover_text("reset canvas and per-plot zoom/pan")
            .clicked()
        {
            *zooms = SoundingZooms::default();
        }
        zoom_slider(ui, "Skew-T view", &mut zooms.skewt.zoom, 0.5..=8.0);
        zoom_slider(ui, "Hodo view", &mut zooms.hodograph.zoom, 0.5..=8.0);
        zoom_slider(ui, "Slinky view", &mut zooms.slinky.zoom, 0.5..=8.0);
    });

    if *edit_layout {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Wide layout").clicked() {
                *layout = SoundingLayout::wide();
            }
            if ui.button("Bottom layout").clicked() {
                *layout = SoundingLayout::compact();
            }
            if ui.button("Reset all").clicked() {
                *layout = SoundingLayout::wide();
                *zooms = SoundingZooms::default();
            }
            ui.spacing_mut().slider_width = 88.0;
            ui.add(Slider::new(&mut layout.canvas_w, 900.0..=3600.0).text("canvas w"));
            ui.add(Slider::new(&mut layout.canvas_h, 700.0..=2600.0).text("canvas h"));
        });
        ui.horizontal_wrapped(|ui| {
            frame_layout_controls(ui, "Skew-T", &mut layout.skewt);
            frame_layout_controls(ui, "Hodo", &mut layout.hodograph);
            frame_layout_controls(ui, "Slinky", &mut layout.slinky);
            frame_layout_controls(ui, "Tables", &mut layout.tables);
        });
    }
}

fn zoom_slider(ui: &mut Ui, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>) {
    let pct = (*value * 100.0).round() as i32;
    ui.spacing_mut().slider_width = 118.0;
    ui.add(
        Slider::new(value, range)
            .show_value(false)
            .text(format!("{label} {pct}%")),
    );
}

fn frame_layout_controls(ui: &mut Ui, label: &str, frame: &mut ComponentFrame) {
    ui.label(RichText::new(label).strong());
    ui.spacing_mut().slider_width = 72.0;
    ui.add(Slider::new(&mut frame.x, 0.0..=FULL_IMAGE_W).text("x"));
    ui.add(Slider::new(&mut frame.y, 0.0..=FULL_IMAGE_H).text("y"));
    ui.add(Slider::new(&mut frame.w, 180.0..=FULL_IMAGE_W).text("w"));
    ui.add(Slider::new(&mut frame.h, 140.0..=FULL_IMAGE_H).text("h"));
}

fn show_overlay_controls(ui: &mut Ui, overlays: &mut SoundingOverlaySettings) {
    CollapsingHeader::new("Overlays")
        .id_salt("rw-ui-sounding-overlays")
        .default_open(false)
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.checkbox(&mut overlays.cape_cin_fill, "CAPE/CIN fill");
                ui.checkbox(&mut overlays.height_markers, "Height marks");
                ui.checkbox(&mut overlays.level_markers, "LCL/LFC/EL");
                ui.checkbox(&mut overlays.wind_barbs, "Wind barbs");
            });
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().slider_width = 98.0;
                ui.add(Slider::new(&mut overlays.text_scale, 0.7..=1.8).text("plot text"));
                ui.add(Slider::new(&mut overlays.table_text_scale, 0.8..=2.2).text("table text"));
                ui.add(Slider::new(&mut overlays.hodo_dot_radius, 2.0..=9.0).text("hodo dots"));
                ui.add(Slider::new(&mut overlays.slinky_dot_radius, 2.0..=9.0).text("slinky dots"));
            });
            ui.horizontal_wrapped(|ui| {
                trace_overlay_control(
                    ui,
                    "Wet-bulb",
                    &mut overlays.wetbulb,
                    &mut overlays.wetbulb_style,
                );
                trace_overlay_control(
                    ui,
                    "ML parcel",
                    &mut overlays.ml_parcel,
                    &mut overlays.ml_parcel_style,
                );
                trace_overlay_control(
                    ui,
                    "MU parcel",
                    &mut overlays.mu_parcel,
                    &mut overlays.mu_parcel_style,
                );
                trace_overlay_control(ui, "DCAPE", &mut overlays.dcape, &mut overlays.dcape_style);
            });
        });
}

fn trace_overlay_control(
    ui: &mut Ui,
    label: &str,
    enabled: &mut bool,
    style: &mut SoundingTraceStyle,
) {
    ui.checkbox(enabled, label);
    ui.add_enabled_ui(*enabled, |ui| {
        egui::ComboBox::from_id_salt(("rw-ui-sounding-trace-style", label))
            .selected_text(style.label())
            .width(56.0)
            .show_ui(ui, |ui| {
                for candidate in SoundingTraceStyle::ALL {
                    ui.selectable_value(style, candidate, candidate.label());
                }
            });
    });
}

fn show_sounding_scene(
    ui: &mut Ui,
    scene: &SoundingScene,
    zooms: &mut SoundingZooms,
    layout: &mut SoundingLayout,
    overlays: &SoundingOverlaySettings,
    edit_layout: bool,
    data: &SoundingData,
) {
    let available_w = ui.available_width().max(64.0);
    let available_h = ui.available_height().max(220.0);
    let width = available_w.max(64.0) * zooms.scene;
    let size = Vec2::new(
        width,
        width * layout.canvas_h.max(1.0) / layout.canvas_w.max(1.0),
    );
    let viewport_h = size.y.min((available_h - 34.0).max(260.0)).min(1200.0);
    egui::ScrollArea::both()
        .id_salt("rw-ui-sounding-scene-scroll")
        .max_height(viewport_h)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            let (rect, response) = ui.allocate_exact_size(size, Sense::click_and_drag());
            handle_panel_viewport_interaction(ui, &response, zooms, layout, edit_layout);
            draw_sounding_scene(ui, rect, scene, data, zooms, layout, overlays, edit_layout);
            if let Some(text) = sounding_hover_text(&response, data, zooms, layout) {
                response.on_hover_text_at_pointer(text);
            }
        });
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SoundingPanelRegion {
    Skewt,
    Hodograph,
    Slinky,
}

fn handle_panel_viewport_interaction(
    ui: &mut Ui,
    response: &egui::Response,
    zooms: &mut SoundingZooms,
    layout: &mut SoundingLayout,
    edit_layout: bool,
) {
    let Some(pos) = response.hover_pos() else {
        return;
    };
    let Some((layout_x, layout_y)) = response_pos_to_layout(response, pos, layout) else {
        return;
    };
    let Some(region) = layout.region_at(layout_x, layout_y) else {
        return;
    };

    if edit_layout {
        if response.dragged() {
            let delta = ui.input(|input| input.pointer.delta());
            if delta != Vec2::ZERO {
                let dx = delta.x / response.rect.width().max(1.0) * layout.canvas_w;
                let dy = delta.y / response.rect.height().max(1.0) * layout.canvas_h;
                let canvas_w = layout.canvas_w;
                let canvas_h = layout.canvas_h;
                let frame = layout.frame_mut(region);
                if frame.resize_handle_contains(layout_x, layout_y) {
                    frame.resize_by(dx, dy, canvas_w, canvas_h);
                } else {
                    frame.move_by(dx, dy, canvas_w, canvas_h);
                }
                ui.ctx().request_repaint();
            }
        }
        return;
    }

    let (base_x, base_y) = layout.layout_to_source(region, layout_x, layout_y);

    let scroll_delta = ui.input(|input| input.smooth_scroll_delta().y);
    let zoom_delta = ui.input(|input| input.zoom_delta());
    if scroll_delta.abs() > 0.01 || (zoom_delta - 1.0).abs() > 0.001 {
        let factor = if (zoom_delta - 1.0).abs() > 0.001 {
            zoom_delta
        } else {
            (scroll_delta * 0.004).exp()
        };
        zoom_panel_at_cursor(zooms, region, base_x, base_y, factor);
        ui.input_mut(|input| input.smooth_scroll_delta = Vec2::ZERO);
        ui.ctx().request_repaint();
    }

    if response.dragged() {
        let delta = ui.input(|input| input.pointer.delta());
        if delta != Vec2::ZERO {
            let dx = delta.x / response.rect.width().max(1.0) * FULL_IMAGE_W;
            let dy = delta.y / response.rect.height().max(1.0) * FULL_IMAGE_H;
            viewport_mut(zooms, region).pan_by(dx, dy);
            ui.ctx().request_repaint();
        }
    }

    if response.double_clicked() {
        viewport_mut(zooms, region).reset();
        ui.ctx().request_repaint();
    }
}

fn response_pos_to_layout(
    response: &egui::Response,
    pos: Pos2,
    layout: &SoundingLayout,
) -> Option<(f32, f32)> {
    let rect = response.rect;
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    Some((
        (pos.x - rect.left()) / rect.width() * layout.canvas_w,
        (pos.y - rect.top()) / rect.height() * layout.canvas_h,
    ))
}

fn viewport_mut(zooms: &mut SoundingZooms, region: SoundingPanelRegion) -> &mut PanelViewport {
    match region {
        SoundingPanelRegion::Skewt => &mut zooms.skewt,
        SoundingPanelRegion::Hodograph => &mut zooms.hodograph,
        SoundingPanelRegion::Slinky => &mut zooms.slinky,
    }
}

fn zoom_panel_at_cursor(
    zooms: &mut SoundingZooms,
    region: SoundingPanelRegion,
    base_x: f32,
    base_y: f32,
    factor: f32,
) {
    let (center_x, center_y) = match region {
        SoundingPanelRegion::Skewt => {
            let plot_w = SKEWT_LEFT_W * SKEWT_FRAC - SKEWT_MARGIN_LEFT - SKEWT_MARGIN_RIGHT;
            let plot_h = SKEWT_UPPER_H - SKEWT_MARGIN_TOP - SKEWT_MARGIN_BOT;
            (
                SKEWT_MARGIN_LEFT + plot_w / 2.0,
                TITLE_H + SKEWT_MARGIN_TOP + plot_h / 2.0,
            )
        }
        SoundingPanelRegion::Hodograph => {
            let cx = HODO_X + HODO_W / 2.0;
            let cy = HODO_Y + 30.0 + ((HODO_H - 30.0 - 8.0) / 2.0);
            (cx, cy)
        }
        SoundingPanelRegion::Slinky => {
            let sep_y = INSET_Y + 30.0;
            let plot_margin = 28.0;
            let plot_top = sep_y + 8.0;
            let plot_size = (INSET_W - 2.0 * plot_margin)
                .min(INSET_H - (plot_top - INSET_Y) - plot_margin - 8.0);
            (INSET_X + INSET_W / 2.0, plot_top + plot_size / 2.0)
        }
    };
    viewport_mut(zooms, region).zoom_at(base_x, base_y, factor, center_x, center_y);
}

fn draw_sounding_scene(
    ui: &Ui,
    screen: Rect,
    scene: &SoundingScene,
    data: &SoundingData,
    zooms: &SoundingZooms,
    layout: &SoundingLayout,
    overlays: &SoundingOverlaySettings,
    edit_layout: bool,
) {
    profile_scope!("sounding_draw_scene");
    let painter = ui.painter_at(screen);
    painter.rect_filled(screen, 0.0, Color32::BLACK);

    let title_canvas = SceneCanvas::new(
        screen,
        layout,
        ComponentFrame::new(0.0, 0.0, FULL_IMAGE_W, TITLE_H),
        layout.title,
    )
    .with_text_scale(overlays.text_scale);
    let skewt_canvas = SceneCanvas::new(
        screen,
        layout,
        SoundingLayout::source_frame(SoundingPanelRegion::Skewt),
        layout.skewt,
    )
    .with_text_scale(overlays.text_scale);
    let hodo_canvas = SceneCanvas::new(
        screen,
        layout,
        SoundingLayout::source_frame(SoundingPanelRegion::Hodograph),
        layout.hodograph,
    )
    .with_text_scale(overlays.text_scale);
    let slinky_canvas = SceneCanvas::new(
        screen,
        layout,
        SoundingLayout::source_frame(SoundingPanelRegion::Slinky),
        layout.slinky,
    )
    .with_text_scale(overlays.text_scale);
    let table_canvas = SceneCanvas::new(
        screen,
        layout,
        ComponentFrame::new(
            0.0,
            TITLE_H + SKEWT_UPPER_H,
            FULL_IMAGE_W,
            FULL_IMAGE_H - TITLE_H - SKEWT_UPPER_H,
        ),
        layout.tables,
    )
    .with_text_scale(overlays.table_text_scale);

    draw_title(&painter, title_canvas, data);
    draw_skewt_panel(&painter, skewt_canvas, scene, zooms.skewt, overlays);
    draw_hodograph_panel(&painter, hodo_canvas, data, zooms.hodograph, overlays);
    draw_slinky_panel(&painter, slinky_canvas, data, zooms.slinky, overlays);
    draw_native_tables(&painter, table_canvas, scene, data);

    if edit_layout {
        draw_layout_editor_overlay(&painter, screen, layout);
    }
}

fn draw_layout_editor_overlay(painter: &egui::Painter, screen: Rect, layout: &SoundingLayout) {
    for (label, frame) in [
        ("Skew-T", layout.skewt),
        ("Hodo", layout.hodograph),
        ("Slinky", layout.slinky),
        ("Tables", layout.tables),
    ] {
        let canvas = SceneCanvas::new(
            screen,
            layout,
            ComponentFrame::new(0.0, 0.0, frame.w, frame.h),
            frame,
        );
        let rect = canvas.dest_rect();
        painter.rect_stroke(
            rect,
            0.0,
            Stroke::new(2.0, color(80, 180, 255)),
            egui::StrokeKind::Inside,
        );
        let handle = Rect::from_min_size(
            Pos2::new(rect.right() - 14.0, rect.bottom() - 14.0),
            Vec2::splat(14.0),
        );
        painter.rect_filled(handle, 0.0, color(80, 180, 255));
        painter.text(
            rect.left_top() + Vec2::new(6.0, 5.0),
            Align2::LEFT_TOP,
            label,
            FontId::monospace(12.0),
            color(230, 245, 255),
        );
    }
}

fn draw_title(painter: &egui::Painter, rect: SceneCanvas, data: &SoundingData) {
    draw_rect_filled_base(
        painter,
        rect,
        0.0,
        0.0,
        FULL_IMAGE_W,
        TITLE_H,
        Color32::BLACK,
    );
    draw_line_base(
        painter,
        rect,
        0.0,
        TITLE_H,
        FULL_IMAGE_W,
        TITLE_H,
        color(205, 205, 205),
        1.0,
    );
    let title = match (data.lat, data.lon) {
        (Some(lat), Some(lon)) => format!("{}  {:.2}N {:.2}W", data.hour, lat.abs(), lon.abs()),
        _ => format!("{}", data.hour),
    };
    draw_text_base(
        painter,
        rect,
        FULL_IMAGE_W / 2.0,
        12.0,
        &title,
        color(230, 230, 230),
        18.0,
        Align2::CENTER_TOP,
    );
}

fn draw_skewt_panel(
    painter: &egui::Painter,
    rect: SceneCanvas,
    scene: &SoundingScene,
    view: PanelViewport,
    overlays: &SoundingOverlaySettings,
) {
    profile_scope!("sounding_draw_skewt");
    let plot_left = SKEWT_MARGIN_LEFT;
    let plot_top = TITLE_H + SKEWT_MARGIN_TOP;
    let plot_w = SKEWT_LEFT_W * SKEWT_FRAC - SKEWT_MARGIN_LEFT - SKEWT_MARGIN_RIGHT;
    let plot_h = SKEWT_UPPER_H - SKEWT_MARGIN_TOP - SKEWT_MARGIN_BOT;
    let plot_right = plot_left + plot_w;
    let plot_bot = plot_top + plot_h;

    draw_rect_outline_base(
        painter,
        rect,
        plot_left,
        plot_top,
        plot_w,
        plot_h,
        color(130, 130, 130),
        1.0,
    );

    let xform = view.transform_from_rect(plot_left, plot_top, plot_w, plot_h);
    let content_painter =
        painter.with_clip_rect(base_rect(rect, 0.0, TITLE_H, SKEWT_LEFT_W, SKEWT_UPPER_H));
    let painter = &content_painter;

    for &p in &[
        1000.0, 925.0, 850.0, 700.0, 500.0, 400.0, 300.0, 250.0, 200.0, 150.0, 100.0,
    ] {
        let (_, y) = tp_to_base(0.0, p);
        draw_line_xform(
            painter,
            rect,
            xform,
            plot_left,
            y,
            plot_right,
            y,
            color_a(120, 120, 120, 150),
            1.0,
        );
        draw_text_xform(
            painter,
            rect,
            xform,
            plot_left - 10.0,
            y - 6.0,
            &format!("{p:.0}"),
            color(210, 210, 210),
            12.0,
            Align2::RIGHT_TOP,
        );
    }

    for t in (-80..=70).step_by(10) {
        let (x0, y0) = tp_to_base(t as f64, SKEWT_P_BOT);
        let (x1, y1) = tp_to_base(t as f64, SKEWT_P_TOP);
        let col = if t == 0 {
            color(80, 130, 230)
        } else {
            color_a(80, 90, 105, 120)
        };
        draw_line_xform(
            painter,
            rect,
            xform,
            x0,
            y0,
            x1,
            y1,
            col,
            if t == 0 { 2.0 } else { 1.0 },
        );
        if (-40..=50).contains(&t) {
            draw_text_xform(
                painter,
                rect,
                xform,
                x0,
                plot_bot + 9.0,
                &format!("{t}"),
                color(220, 220, 220),
                12.0,
                Align2::CENTER_TOP,
            );
        }
    }

    for theta in (-40..=80).step_by(20) {
        let mut prev = None;
        let mut p = SKEWT_P_BOT;
        while p >= SKEWT_P_TOP {
            let t = (theta as f64 + 273.15) * (p / 1000.0).powf(0.2854) - 273.15;
            let xy = tp_to_base(t, p);
            if let Some((px, py)) = prev {
                draw_line_xform(
                    painter,
                    rect,
                    xform,
                    px,
                    py,
                    xy.0,
                    xy.1,
                    color_a(115, 85, 45, 80),
                    1.0,
                );
            }
            prev = Some(xy);
            p -= 25.0;
        }
    }

    if overlays.cape_cin_fill {
        draw_cape_cin_fills_native(painter, rect, scene, xform);
    }

    draw_trace(
        painter,
        rect,
        &scene.column.pressure_hpa,
        &scene.column.temperature_c,
        color(255, 55, 55),
        3.0,
        xform,
    );
    if overlays.wetbulb {
        draw_trace_styled_native(
            painter,
            rect,
            &scene.native.profile.pres,
            &scene.native.profile.wetbulb,
            color(0, 220, 220),
            1.4,
            overlays.wetbulb_style,
            xform,
        );
    }
    draw_trace(
        painter,
        rect,
        &scene.column.pressure_hpa,
        &scene.column.dewpoint_c,
        color(55, 235, 55),
        3.0,
        xform,
    );
    if overlays.ml_parcel {
        draw_trace_styled_native(
            painter,
            rect,
            &scene.native.params.mlpcl.ptrace,
            &scene.native.params.mlpcl.ttrace,
            color(255, 210, 40),
            2.0,
            overlays.ml_parcel_style,
            xform,
        );
    }
    if overlays.mu_parcel {
        draw_trace_styled_native(
            painter,
            rect,
            &scene.native.params.mupcl.ptrace,
            &scene.native.params.mupcl.ttrace,
            color(255, 140, 35),
            2.0,
            overlays.mu_parcel_style,
            xform,
        );
    }
    if overlays.dcape {
        draw_trace_styled_native(
            painter,
            rect,
            &scene.native.params.dcape.ptrace,
            &scene.native.params.dcape.ttrace,
            color(245, 45, 220),
            1.8,
            overlays.dcape_style,
            xform,
        );
    }

    if overlays.height_markers {
        draw_height_markers_native(painter, rect, &scene.column, xform);
    }
    draw_surface_labels_native(painter, rect, &scene.column, xform);
    if overlays.level_markers {
        draw_parcel_level_markers_native(painter, rect, &scene.native, xform);
    }
    if overlays.wind_barbs {
        draw_wind_barbs_native(painter, rect, &scene.column, xform);
    }
}

fn draw_hodograph_panel(
    painter: &egui::Painter,
    rect: SceneCanvas,
    data: &SoundingData,
    view: PanelViewport,
    overlays: &SoundingOverlaySettings,
) {
    profile_scope!("sounding_draw_hodograph");
    draw_rect_outline_base(
        painter,
        rect,
        HODO_X,
        HODO_Y,
        HODO_W,
        HODO_H,
        color(220, 220, 220),
        1.0,
    );
    draw_text_base(
        painter,
        rect,
        HODO_X + HODO_W / 2.0,
        HODO_Y + 12.0,
        "Hodograph (kts)",
        color(80, 180, 255),
        28.0,
        Align2::CENTER_TOP,
    );

    let cx = HODO_X + HODO_W / 2.0;
    let cy = HODO_Y + 30.0 + ((HODO_H - 30.0 - 8.0) / 2.0);
    let radius = ((HODO_W - 72.0).min(HODO_H - 30.0 - 40.0) / 2.0 - 8.0).max(30.0);
    let scale = f64::from(radius) / HODO_MAX_RING_KT;
    let xform = view.transform_around(cx, cy);
    let content_painter = painter.with_clip_rect(base_rect(rect, HODO_X, HODO_Y, HODO_W, HODO_H));
    let painter = &content_painter;
    let marker_radius = overlays.hodo_dot_radius.clamp(2.0, 9.0);
    for kt in [20.0, 40.0, 60.0, 80.0] {
        draw_circle_xform(
            painter,
            rect,
            xform,
            cx,
            cy,
            (kt * scale) as f32,
            color(45, 50, 58),
            1.0,
        );
        draw_text_xform(
            painter,
            rect,
            xform,
            cx + (kt * scale) as f32 + 8.0,
            cy + 4.0,
            &format!("{kt:.0}"),
            color(125, 125, 150),
            18.0,
            Align2::LEFT_TOP,
        );
    }
    draw_line_xform(
        painter,
        rect,
        xform,
        cx - radius,
        cy,
        cx + radius,
        cy,
        color(70, 70, 70),
        1.0,
    );
    draw_line_xform(
        painter,
        rect,
        xform,
        cx,
        cy - radius,
        cx,
        cy + radius,
        color(70, 70, 70),
        1.0,
    );

    let points = wind_profile_points(data);
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let (x0, y0) = (cx + (a.u_kt * scale) as f32, cy - (a.v_kt * scale) as f32);
        let (x1, y1) = (cx + (b.u_kt * scale) as f32, cy - (b.v_kt * scale) as f32);
        draw_line_xform(
            painter,
            rect,
            xform,
            x0,
            y0,
            x1,
            y1,
            hodo_color(a.height_agl_m),
            3.0,
        );
    }
    for point in points
        .iter()
        .filter(|point| matches!(point.height_agl_m.round() as i32, 0..=12_500))
    {
        if (point.height_agl_m % 1000.0).abs() > 350.0 {
            continue;
        }
        let x = cx + (point.u_kt * scale) as f32;
        let y = cy - (point.v_kt * scale) as f32;
        draw_circle_filled_xform(painter, rect, xform, x, y, marker_radius, Color32::WHITE);
        draw_circle_xform(
            painter,
            rect,
            xform,
            x,
            y,
            marker_radius,
            hodo_color(point.height_agl_m),
            2.0,
        );
    }

    if let Some((ru, rv)) = bunkers_right_motion(data) {
        let x = cx + (ru * scale) as f32;
        let y = cy - (rv * scale) as f32;
        draw_circle_filled_xform(
            painter,
            rect,
            xform,
            x,
            y,
            (marker_radius - 1.0).max(2.5),
            color(255, 45, 45),
        );
        let (dir, spd) = dir_speed_from_uv_kt(ru, rv);
        draw_text_xform(
            painter,
            rect,
            xform,
            x + 10.0,
            y,
            &format!("{dir:03.0}/{spd:.0} RM"),
            color(255, 80, 80),
            16.0,
            Align2::LEFT_CENTER,
        );
    }
}

fn draw_slinky_panel(
    painter: &egui::Painter,
    rect: SceneCanvas,
    data: &SoundingData,
    view: PanelViewport,
    overlays: &SoundingOverlaySettings,
) {
    profile_scope!("sounding_draw_slinky");
    draw_rect_outline_base(
        painter,
        rect,
        INSET_X,
        INSET_Y,
        INSET_W,
        INSET_H,
        color(220, 220, 220),
        1.0,
    );
    draw_text_base(
        painter,
        rect,
        INSET_X + INSET_W / 2.0,
        INSET_Y + 10.0,
        "Storm Slinky",
        color(0, 245, 245),
        24.0,
        Align2::CENTER_TOP,
    );

    let Some((storm_u, storm_v)) = bunkers_right_motion(data) else {
        draw_text_base(
            painter,
            rect,
            INSET_X + INSET_W / 2.0,
            INSET_Y + INSET_H / 2.0,
            "No slinky data",
            color(150, 150, 150),
            18.0,
            Align2::CENTER_CENTER,
        );
        return;
    };
    let points = wind_profile_points(data);
    if points.is_empty() {
        return;
    }
    let sep_y = INSET_Y + 30.0;
    let plot_margin = 56.0;
    let plot_top = sep_y + 8.0;
    let plot_size =
        (INSET_W - 2.0 * plot_margin).min(INSET_H - (plot_top - INSET_Y) - plot_margin - 8.0);
    let cx = INSET_X + INSET_W / 2.0;
    let cy = plot_top + plot_size / 2.0;
    let xform = view.transform_around(cx, cy);
    let content_painter =
        painter.with_clip_rect(base_rect(rect, INSET_X, INSET_Y, INSET_W, INSET_H));
    let painter = &content_painter;
    let marker_radius = overlays.slinky_dot_radius.clamp(2.0, 9.0);
    let max_disp = points
        .iter()
        .map(|point| (point.u_kt - storm_u).hypot(point.v_kt - storm_v))
        .fold(0.0_f64, f64::max)
        .max(4.0);
    let scale = (f64::from(plot_size) / 2.0 - 24.0) / max_disp;
    draw_line_xform(
        painter,
        rect,
        xform,
        cx - plot_size / 2.0,
        cy,
        cx + plot_size / 2.0,
        cy,
        color(45, 45, 65),
        1.0,
    );
    draw_line_xform(
        painter,
        rect,
        xform,
        cx,
        cy - plot_size / 2.0,
        cx,
        cy + plot_size / 2.0,
        color(45, 45, 65),
        1.0,
    );
    for frac in [0.33, 0.66] {
        draw_circle_xform(
            painter,
            rect,
            xform,
            cx,
            cy,
            (max_disp * frac * scale) as f32,
            color(45, 50, 70),
            1.0,
        );
    }

    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        let x0 = cx + ((a.u_kt - storm_u) * scale) as f32;
        let y0 = cy - ((a.v_kt - storm_v) * scale) as f32;
        let x1 = cx + ((b.u_kt - storm_u) * scale) as f32;
        let y1 = cy - ((b.v_kt - storm_v) * scale) as f32;
        draw_line_xform(
            painter,
            rect,
            xform,
            x0,
            y0,
            x1,
            y1,
            color(120, 120, 150),
            2.0,
        );
    }
    for point in points {
        let x = cx + ((point.u_kt - storm_u) * scale) as f32;
        let y = cy - ((point.v_kt - storm_v) * scale) as f32;
        draw_circle_filled_xform(
            painter,
            rect,
            xform,
            x,
            y,
            marker_radius,
            hodo_color(point.height_agl_m),
        );
        draw_circle_xform(
            painter,
            rect,
            xform,
            x,
            y,
            marker_radius,
            Color32::WHITE,
            1.0,
        );
    }
}

fn draw_native_tables(
    painter: &egui::Painter,
    rect: SceneCanvas,
    scene: &SoundingScene,
    data: &SoundingData,
) {
    profile_scope!("sounding_draw_tables");
    let top = TITLE_H + SKEWT_UPPER_H;
    let p = &scene.native.params;
    let ecape = &scene.native.verified_ecape;

    draw_rect_filled_base(
        painter,
        rect,
        0.0,
        top,
        FULL_IMAGE_W,
        FULL_IMAGE_H - top,
        color(0, 0, 0),
    );
    for x in [760.0, 1370.0, 1850.0] {
        draw_line_base(
            painter,
            rect,
            x,
            top,
            x,
            FULL_IMAGE_H,
            color(90, 90, 90),
            1.0,
        );
    }

    draw_table_header(painter, rect, 14.0, top + 12.0, "PARCELS");
    for (i, (label, x)) in [
        ("PCL", 14.0),
        ("CAPE", 105.0),
        ("CINH", 165.0),
        ("3CAPE", 240.0),
        ("6CAPE", 315.0),
        ("LCL", 390.0),
        ("LFC", 455.0),
        ("EL", 520.0),
        ("LI", 575.0),
        ("ECAPE", 660.0),
        ("NCAPE", 735.0),
    ]
    .iter()
    .enumerate()
    {
        let align = if i == 0 {
            Align2::LEFT_TOP
        } else {
            Align2::RIGHT_TOP
        };
        draw_text_base(
            painter,
            rect,
            *x,
            top + 44.0,
            label,
            color(180, 180, 180),
            14.0,
            align,
        );
    }
    let parcel_rows = [
        (
            "SFC",
            p.sfcpcl.bplus,
            p.sfcpcl.bminus,
            p.sfcpcl.b3km,
            p.sfcpcl.b6km,
            p.sfcpcl.lclhght,
            p.sfcpcl.lfchght,
            p.sfcpcl.elhght,
            p.sfcpcl.li5,
            ecape.surface_based.ecape,
            ecape.surface_based.ncape,
        ),
        (
            "ML",
            p.mlpcl.bplus,
            p.mlpcl.bminus,
            p.mlpcl.b3km,
            p.mlpcl.b6km,
            p.mlpcl.lclhght,
            p.mlpcl.lfchght,
            p.mlpcl.elhght,
            p.mlpcl.li5,
            ecape.mixed_layer.ecape,
            ecape.mixed_layer.ncape,
        ),
        (
            "MU",
            p.mupcl.bplus,
            p.mupcl.bminus,
            p.mupcl.b3km,
            p.mupcl.b6km,
            p.mupcl.lclhght,
            p.mupcl.lfchght,
            p.mupcl.elhght,
            p.mupcl.li5,
            ecape.most_unstable.ecape,
            ecape.most_unstable.ncape,
        ),
    ];
    for (row, values) in parcel_rows.iter().enumerate() {
        draw_parcel_calc_row(painter, rect, top + 70.0 + row as f32 * 28.0, *values);
    }

    draw_table_header(painter, rect, 14.0, top + 178.0, "THERMO");
    let thermo_left = [
        ("PW", fmt_opt(p.precip_water, 2, " in")),
        ("Mean W", fmt_opt(p.mean_mixr, 1, " g/kg")),
        ("Low RH", fmt_opt(p.mean_rh_low, 0, "%")),
        ("Mid RH", fmt_opt(p.mean_rh_mid, 0, "%")),
        ("K", fmt_opt(p.k_index, 0, "")),
        ("TT", fmt_opt(p.t_totals, 0, "")),
    ];
    let thermo_right = [
        ("LCL T", fmt_finite(lcl_temp_c(&scene.native), 1, " C")),
        ("FRZ", fmt_opt(p.frz_lvl, 0, " m")),
        ("WBZ", fmt_opt(p.wb_zero, 0, " m")),
        ("LR 0-3", fmt_opt(p.lr03, 1, " C/km")),
        ("LR 3-6", fmt_opt(p.lr36, 1, " C/km")),
        ("LR 7-5", fmt_opt(p.lr75, 1, " C/km")),
    ];
    for (i, (label, value)) in thermo_left.iter().enumerate() {
        draw_kv(
            painter,
            rect,
            18.0,
            top + 212.0 + i as f32 * 25.0,
            label,
            value,
        );
    }
    for (i, (label, value)) in thermo_right.iter().enumerate() {
        draw_kv(
            painter,
            rect,
            380.0,
            top + 212.0 + i as f32 * 25.0,
            label,
            value,
        );
    }

    draw_table_header(painter, rect, 790.0, top + 12.0, "SHEAR / HELICITY");
    for (i, (label, srh, shear)) in [
        (
            "SFC-1km",
            Some(p.srh01.0),
            Some(vector_mag(p.shr01.0, p.shr01.1)),
        ),
        (
            "SFC-3km",
            Some(p.srh03.0),
            Some(vector_mag(p.shr03.0, p.shr03.1)),
        ),
        ("SFC-6km", None, Some(vector_mag(p.shr06.0, p.shr06.1))),
        ("SFC-8km", None, Some(vector_mag(p.shr08.0, p.shr08.1))),
        ("Eff", p.effective_srh, p.effective_bwd),
    ]
    .iter()
    .enumerate()
    {
        let y = top + 48.0 + i as f32 * 30.0;
        draw_text_base(
            painter,
            rect,
            790.0,
            y,
            label,
            color(230, 230, 230),
            15.0,
            Align2::LEFT_TOP,
        );
        draw_text_base(
            painter,
            rect,
            955.0,
            y,
            &format!("SRH {}", fmt_optional_number(*srh, 0, "")),
            value_color((*srh).unwrap_or(f64::NAN)),
            15.0,
            Align2::LEFT_TOP,
        );
        draw_text_base(
            painter,
            rect,
            1160.0,
            y,
            &format!("Shr {}", fmt_optional_number(*shear, 0, " kt")),
            color(255, 210, 80),
            15.0,
            Align2::LEFT_TOP,
        );
    }

    draw_table_header(painter, rect, 790.0, top + 226.0, "STORM MOTION");
    for (i, (label, value)) in [
        ("Bunkers RM", fmt_uv_motion(p.rstu, p.rstv)),
        ("Bunkers LM", fmt_uv_motion(p.lstu, p.lstv)),
        ("Corfidi UP", fmt_uv_motion(p.corfidi_up_u, p.corfidi_up_v)),
        ("Corfidi DN", fmt_uv_motion(p.corfidi_dn_u, p.corfidi_dn_v)),
        ("1 km wind", fmt_dir_spd(p.wind_1km.0, p.wind_1km.1)),
        ("6 km wind", fmt_dir_spd(p.wind_6km.0, p.wind_6km.1)),
    ]
    .iter()
    .enumerate()
    {
        draw_kv(
            painter,
            rect,
            794.0,
            top + 262.0 + i as f32 * 25.0,
            label,
            value,
        );
    }

    draw_table_header(painter, rect, 1400.0, top + 12.0, "COMPOSITES");
    for (i, (label, value, col)) in [
        (
            "STP cin",
            fmt_opt(p.stp_cin, 1, ""),
            stp_like_color(p.stp_cin),
        ),
        (
            "STP fixed",
            fmt_opt(p.stp_fixed, 1, ""),
            stp_like_color(p.stp_fixed),
        ),
        ("SCP", fmt_opt(p.scp, 1, ""), stp_like_color(p.scp)),
        ("SHIP", fmt_opt(p.ship, 1, ""), ship_like_color(p.ship)),
        ("EHI 0-1", fmt_opt(p.ehi01, 1, ""), stp_like_color(p.ehi01)),
        ("EHI 0-3", fmt_opt(p.ehi03, 1, ""), stp_like_color(p.ehi03)),
        (
            "Critical Angle",
            fmt_finite(p.critical_angle, 0, " deg"),
            color(230, 230, 230),
        ),
        (
            "DCAPE",
            fmt_finite(p.dcape.dcape, 0, " J/kg"),
            color(255, 210, 80),
        ),
    ]
    .iter()
    .enumerate()
    {
        draw_kv_colored(
            painter,
            rect,
            1404.0,
            top + 48.0 + i as f32 * 27.0,
            label,
            value,
            *col,
        );
    }

    draw_table_header(painter, rect, 1400.0, top + 298.0, "WATCH TYPE");
    draw_text_base(
        painter,
        rect,
        1404.0,
        top + 338.0,
        p.watch_type.label(),
        watch_color(p.watch_type.label()),
        30.0,
        Align2::LEFT_TOP,
    );

    draw_table_header(painter, rect, 1880.0, top + 12.0, "MODEL POINT");
    if let (Some(t), Some(td)) = (
        scene.column.temperature_c.first(),
        scene.column.dewpoint_c.first(),
    ) {
        draw_kv(
            painter,
            rect,
            1884.0,
            top + 48.0,
            "Sfc T/Td",
            &format!("{:.0}/{:.0} F", c_to_f(*t), c_to_f(*td)),
        );
    }
    if let Some((ru, rv)) = bunkers_right_motion(data) {
        let (dir, spd) = dir_speed_from_uv_kt(ru, rv);
        draw_kv(
            painter,
            rect,
            1884.0,
            top + 75.0,
            "Local RM",
            &format!("{dir:03.0}/{spd:.0} kt"),
        );
    }
    draw_kv(
        painter,
        rect,
        1884.0,
        top + 102.0,
        "Levels",
        &scene.column.len().to_string(),
    );
    draw_text_base(
        painter,
        rect,
        1884.0,
        top + 152.0,
        "Native egui scene",
        color(255, 210, 80),
        16.0,
        Align2::LEFT_TOP,
    );
}

fn draw_table_header(painter: &egui::Painter, rect: SceneCanvas, x: f32, y: f32, label: &str) {
    draw_text_base(
        painter,
        rect,
        x,
        y,
        label,
        color(0, 255, 255),
        18.0,
        Align2::LEFT_TOP,
    );
}

fn draw_parcel_calc_row(
    painter: &egui::Painter,
    rect: SceneCanvas,
    y: f32,
    values: (&str, f64, f64, f64, f64, f64, f64, f64, f64, f64, f64),
) {
    let (label, cape, cinh, cape3, cape6, lcl, lfc, el, li, ecape, ncape) = values;
    draw_text_base(
        painter,
        rect,
        14.0,
        y,
        label,
        color(0, 220, 255),
        15.0,
        Align2::LEFT_TOP,
    );
    for (x, value, decimals, suffix, col) in [
        (105.0, cape, 0, "", cape_color(cape)),
        (165.0, cinh, 0, "", color(255, 120, 120)),
        (240.0, cape3, 0, "", cape_color(cape3)),
        (315.0, cape6, 0, "", cape_color(cape6)),
        (390.0, lcl, 0, "", color(230, 230, 230)),
        (455.0, lfc, 0, "", color(230, 230, 230)),
        (520.0, el, 0, "", color(230, 230, 230)),
        (575.0, li, 0, "", value_color(-li)),
        (660.0, ecape, 0, "", cape_color(ecape)),
        (735.0, ncape, 2, "", color(255, 210, 80)),
    ] {
        draw_text_base(
            painter,
            rect,
            x,
            y,
            &fmt_finite(value, decimals, suffix),
            col,
            13.0,
            Align2::RIGHT_TOP,
        );
    }
}

fn draw_kv(painter: &egui::Painter, rect: SceneCanvas, x: f32, y: f32, label: &str, value: &str) {
    draw_kv_colored(painter, rect, x, y, label, value, color(230, 230, 230));
}

fn draw_kv_colored(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    label: &str,
    value: &str,
    value_col: Color32,
) {
    draw_text_base(
        painter,
        rect,
        x,
        y,
        label,
        color(180, 180, 180),
        15.0,
        Align2::LEFT_TOP,
    );
    draw_text_base(
        painter,
        rect,
        x + 150.0,
        y,
        value,
        value_col,
        15.0,
        Align2::LEFT_TOP,
    );
}

fn lcl_temp_c(native: &NativeSounding) -> f64 {
    native.profile.interp_tmpc(native.params.sfcpcl.lclpres)
}

fn vector_mag(u: f64, v: f64) -> f64 {
    (u * u + v * v).sqrt()
}

fn fmt_uv_motion(u: f64, v: f64) -> String {
    let (dir, spd) = dir_speed_from_uv_kt(u, v);
    fmt_dir_spd(dir, spd)
}

fn fmt_dir_spd(dir: f64, spd: f64) -> String {
    if dir.is_finite() && spd.is_finite() {
        format!("{dir:03.0}/{spd:.0} kt")
    } else {
        "M".to_string()
    }
}

fn fmt_opt(value: Option<f64>, decimals: usize, suffix: &str) -> String {
    fmt_optional_number(value, decimals, suffix)
}

fn fmt_optional_number(value: Option<f64>, decimals: usize, suffix: &str) -> String {
    match value {
        Some(value) => fmt_finite(value, decimals, suffix),
        None => "M".to_string(),
    }
}

fn fmt_finite(value: f64, decimals: usize, suffix: &str) -> String {
    if value.is_finite() {
        format!("{value:.decimals$}{suffix}")
    } else {
        "M".to_string()
    }
}

fn cape_color(value: f64) -> Color32 {
    if !value.is_finite() {
        color(120, 120, 120)
    } else if value >= 3000.0 {
        color(255, 80, 80)
    } else if value >= 1500.0 {
        color(255, 170, 0)
    } else if value >= 500.0 {
        color(255, 230, 80)
    } else {
        color(230, 230, 230)
    }
}

fn value_color(value: f64) -> Color32 {
    if !value.is_finite() {
        color(120, 120, 120)
    } else if value >= 250.0 {
        color(255, 80, 80)
    } else if value >= 150.0 {
        color(255, 170, 0)
    } else if value >= 75.0 {
        color(255, 230, 80)
    } else {
        color(230, 230, 230)
    }
}

fn stp_like_color(value: Option<f64>) -> Color32 {
    let Some(value) = value else {
        return color(120, 120, 120);
    };
    if value >= 4.0 {
        color(255, 80, 80)
    } else if value >= 2.0 {
        color(255, 170, 0)
    } else if value >= 1.0 {
        color(255, 230, 80)
    } else {
        color(230, 230, 230)
    }
}

fn ship_like_color(value: Option<f64>) -> Color32 {
    let Some(value) = value else {
        return color(120, 120, 120);
    };
    if value >= 2.0 {
        color(255, 80, 80)
    } else if value >= 1.0 {
        color(255, 170, 0)
    } else if value >= 0.5 {
        color(255, 230, 80)
    } else {
        color(230, 230, 230)
    }
}

fn watch_color(label: &str) -> Color32 {
    if label.contains("TOR") {
        color(255, 70, 70)
    } else if label.contains("SVR") {
        color(255, 220, 70)
    } else if label == "NONE" {
        color(150, 150, 150)
    } else {
        color(255, 170, 0)
    }
}

fn draw_trace(
    painter: &egui::Painter,
    rect: SceneCanvas,
    pressure: &[f64],
    temp_c: &[f64],
    col: Color32,
    width: f32,
    xform: PanelTransform,
) {
    for (p_pair, t_pair) in pressure.windows(2).zip(temp_c.windows(2)) {
        let p0 = p_pair[0];
        let p1 = p_pair[1];
        let t0 = t_pair[0];
        let t1 = t_pair[1];
        if !(p0.is_finite() && p1.is_finite() && t0.is_finite() && t1.is_finite()) {
            continue;
        }
        let (x0, y0) = tp_to_base(t0, p0);
        let (x1, y1) = tp_to_base(t1, p1);
        draw_line_xform(painter, rect, xform, x0, y0, x1, y1, col, width);
    }
}

fn draw_cape_cin_fills_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    scene: &SoundingScene,
    xform: PanelTransform,
) {
    let pcl = &scene.native.params.mlpcl;
    let n = pcl.ptrace.len().min(pcl.ttrace.len());
    if n < 2 {
        return;
    }

    for i in 0..n {
        let p = pcl.ptrace[i];
        let parcel_vt_c = pcl.ttrace[i];
        if !(p.is_finite() && parcel_vt_c.is_finite() && (SKEWT_P_TOP..=SKEWT_P_BOT).contains(&p)) {
            continue;
        }
        let Some(env_vt_c) =
            interp_pressure_value(&scene.native.profile.pres, &scene.native.profile.vtmp, p)
        else {
            continue;
        };
        if !env_vt_c.is_finite() || (parcel_vt_c - env_vt_c).abs() < 0.05 {
            continue;
        }
        let (parcel_x, y) = tp_to_base(parcel_vt_c, p);
        let (env_x, _) = tp_to_base(env_vt_c, p);
        let col = if parcel_vt_c > env_vt_c {
            Color32::from_rgba_unmultiplied(255, 60, 40, 70)
        } else {
            Color32::from_rgba_unmultiplied(60, 80, 255, 60)
        };
        draw_line_xform(painter, rect, xform, env_x, y, parcel_x, y, col, 4.0);
    }
}

fn draw_trace_styled_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    pressure: &[f64],
    temp_c: &[f64],
    col: Color32,
    width: f32,
    style: SoundingTraceStyle,
    xform: PanelTransform,
) {
    match style {
        SoundingTraceStyle::Solid => draw_trace(painter, rect, pressure, temp_c, col, width, xform),
        SoundingTraceStyle::Dashed => draw_dashed_trace_native(
            painter, rect, pressure, temp_c, col, width, 12.0, 8.0, xform,
        ),
        SoundingTraceStyle::Dotted => {
            draw_dashed_trace_native(painter, rect, pressure, temp_c, col, width, 2.0, 8.0, xform)
        }
    }
}

fn draw_dashed_trace_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    pressure: &[f64],
    temp_c: &[f64],
    col: Color32,
    width: f32,
    dash: f32,
    gap: f32,
    xform: PanelTransform,
) {
    for (p_pair, t_pair) in pressure.windows(2).zip(temp_c.windows(2)) {
        let p0 = p_pair[0];
        let p1 = p_pair[1];
        let t0 = t_pair[0];
        let t1 = t_pair[1];
        if !(p0.is_finite() && p1.is_finite() && t0.is_finite() && t1.is_finite()) {
            continue;
        }
        if p0 < SKEWT_P_TOP || p1 > SKEWT_P_BOT {
            continue;
        }
        let (x0, y0) = tp_to_base(t0, p0);
        let (x1, y1) = tp_to_base(t1, p1);
        draw_dashed_line_xform(painter, rect, xform, x0, y0, x1, y1, col, width, dash, gap);
    }
}

fn draw_dashed_line_xform(
    painter: &egui::Painter,
    rect: SceneCanvas,
    xform: PanelTransform,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    col: Color32,
    width: f32,
    dash: f32,
    gap: f32,
) {
    let dx = x1 - x0;
    let dy = y1 - y0;
    let len = dx.hypot(dy);
    if len <= 0.01 {
        return;
    }
    let dash = dash.max(1.0);
    let gap = gap.max(0.0);
    let period = (dash + gap).max(1.0);
    let mut dist = 0.0;
    while dist < len {
        let end = (dist + dash).min(len);
        let a = dist / len;
        let b = end / len;
        draw_line_xform(
            painter,
            rect,
            xform,
            x0 + dx * a,
            y0 + dy * a,
            x0 + dx * b,
            y0 + dy * b,
            col,
            width,
        );
        dist += period;
    }
}

fn interp_pressure_value(pressure: &[f64], values: &[f64], target_hpa: f64) -> Option<f64> {
    if pressure.len() != values.len()
        || pressure.is_empty()
        || !target_hpa.is_finite()
        || target_hpa <= 0.0
    {
        return None;
    }
    let first_p = *pressure.first()?;
    let first_v = *values.first()?;
    if target_hpa >= first_p && first_v.is_finite() {
        return Some(first_v);
    }
    let last_p = *pressure.last()?;
    let last_v = *values.last()?;
    if target_hpa <= last_p && last_v.is_finite() {
        return Some(last_v);
    }

    for (p_pair, v_pair) in pressure.windows(2).zip(values.windows(2)) {
        let p0 = p_pair[0];
        let p1 = p_pair[1];
        let v0 = v_pair[0];
        let v1 = v_pair[1];
        if !(p0.is_finite()
            && p1.is_finite()
            && p0 > 0.0
            && p1 > 0.0
            && v0.is_finite()
            && v1.is_finite())
        {
            continue;
        }
        let between =
            (p0 >= target_hpa && target_hpa >= p1) || (p1 >= target_hpa && target_hpa >= p0);
        if !between {
            continue;
        }
        let denom = p1.ln() - p0.ln();
        if denom.abs() <= f64::EPSILON {
            return Some(v0);
        }
        let t = ((target_hpa.ln() - p0.ln()) / denom).clamp(0.0, 1.0);
        return Some(v0 + (v1 - v0) * t);
    }
    None
}

fn draw_height_markers_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    column: &SoundingColumn,
    xform: PanelTransform,
) {
    let sfc_h = column.height_m_msl.first().copied().unwrap_or(0.0);
    for km in [0.0, 1.0, 3.0, 6.0, 9.0, 12.0] {
        let h = sfc_h + km * 1000.0;
        let Some(p) = pressure_at_height(column, h) else {
            continue;
        };
        let (_, y) = tp_to_base(0.0, p);
        draw_text_xform(
            painter,
            rect,
            xform,
            SKEWT_MARGIN_LEFT - 36.0,
            y,
            &format!("{km:.0}KM"),
            color(0, 230, 230),
            16.0,
            Align2::RIGHT_CENTER,
        );
    }
}

fn draw_surface_labels_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    column: &SoundingColumn,
    xform: PanelTransform,
) {
    let Some((&p, &t)) = column
        .pressure_hpa
        .first()
        .zip(column.temperature_c.first())
    else {
        return;
    };
    let td = column.dewpoint_c.first().copied().unwrap_or(f64::NAN);
    let (tx, _) = tp_to_base(t, p);
    let (tdx, _) = tp_to_base(td, p);
    let y = TITLE_H + SKEWT_UPPER_H - 30.0;
    draw_text_xform(
        painter,
        rect,
        xform,
        tx,
        y,
        &format!("{:.0}F", c_to_f(t)),
        color(255, 65, 65),
        24.0,
        Align2::CENTER_TOP,
    );
    if td.is_finite() {
        draw_text_xform(
            painter,
            rect,
            xform,
            tdx,
            y,
            &format!("{:.0}F", c_to_f(td)),
            color(70, 255, 70),
            24.0,
            Align2::CENTER_TOP,
        );
    }
}

fn draw_parcel_level_markers_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    native: &NativeSounding,
    xform: PanelTransform,
) {
    let plot_right = SKEWT_LEFT_W * SKEWT_FRAC - SKEWT_MARGIN_RIGHT;
    let p = &native.params.sfcpcl;
    let mut markers: Vec<(String, f64, f64, Color32)> = Vec::new();
    if p.lclpres.is_finite() && p.lfcpres.is_finite() && (p.lclpres - p.lfcpres).abs() < 8.0 {
        let height_m = if p.lfchght.is_finite() {
            p.lfchght
        } else {
            p.lclhght
        };
        markers.push((
            "LCL/LFC".to_string(),
            p.lfcpres,
            height_m,
            color(255, 245, 40),
        ));
    } else {
        markers.push(("LCL".to_string(), p.lclpres, p.lclhght, color(60, 255, 60)));
        markers.push(("LFC".to_string(), p.lfcpres, p.lfchght, color(255, 245, 40)));
    }
    markers.push(("EL".to_string(), p.elpres, p.elhght, color(255, 80, 255)));
    for (label, pressure, height_m, col) in markers {
        if !(pressure.is_finite() && (SKEWT_P_TOP..=SKEWT_P_BOT).contains(&pressure)) {
            continue;
        }
        let (_, y) = tp_to_base(0.0, pressure);
        draw_line_xform(
            painter,
            rect,
            xform,
            plot_right - 145.0,
            y,
            plot_right - 8.0,
            y,
            col,
            1.4,
        );
        let text = if height_m.is_finite() {
            format!("{label} {:.1}km", height_m / 1000.0)
        } else {
            label
        };
        draw_text_xform(
            painter,
            rect,
            xform,
            plot_right - 10.0,
            y - 16.0,
            &text,
            col,
            20.0,
            Align2::RIGHT_TOP,
        );
    }
}

fn draw_wind_barbs_native(
    painter: &egui::Painter,
    rect: SceneCanvas,
    column: &SoundingColumn,
    xform: PanelTransform,
) {
    let x = SKEWT_LEFT_W - 42.0;
    for (((&p, &u_ms), &v_ms), idx) in column
        .pressure_hpa
        .iter()
        .zip(&column.u_ms)
        .zip(&column.v_ms)
        .zip(0..)
    {
        if idx % 2 != 0 || !(p >= SKEWT_P_TOP && p <= SKEWT_P_BOT) {
            continue;
        }
        let (_, y) = tp_to_base(0.0, p);
        draw_wind_barb_base(painter, rect, x, y, u_ms * MS_TO_KT, v_ms * MS_TO_KT, xform);
    }
}

fn draw_wind_barb_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    u_kt: f64,
    v_kt: f64,
    xform: PanelTransform,
) {
    let spd = u_kt.hypot(v_kt);
    if spd < 2.0 {
        draw_circle_xform(painter, rect, xform, x, y, 4.0, color(0, 220, 220), 1.5);
        return;
    }
    let len = 34.0;
    let angle = (u_kt.atan2(v_kt) + std::f64::consts::PI) as f32;
    let dx = angle.sin() * len;
    let dy = angle.cos() * len;
    let x2 = x + dx;
    let y2 = y - dy;
    draw_line_xform(painter, rect, xform, x, y, x2, y2, color(0, 220, 220), 1.5);
    let barb_count = (spd / 10.0).round().clamp(1.0, 6.0) as i32;
    for i in 0..barb_count {
        let frac = 0.25 + i as f32 * 0.12;
        let bx = x + dx * frac;
        let by = y - dy * frac;
        let side_angle = angle + 0.8;
        draw_line_xform(
            painter,
            rect,
            xform,
            bx,
            by,
            bx + side_angle.sin() * 11.0,
            by - side_angle.cos() * 11.0,
            color(0, 220, 220),
            1.5,
        );
    }
}

fn pressure_at_height(column: &SoundingColumn, height_m: f64) -> Option<f64> {
    for (h_pair, p_pair) in column
        .height_m_msl
        .windows(2)
        .zip(column.pressure_hpa.windows(2))
    {
        let h0 = h_pair[0];
        let h1 = h_pair[1];
        if height_m < h0 || height_m > h1 {
            continue;
        }
        let t = ((height_m - h0) / (h1 - h0).max(1.0)).clamp(0.0, 1.0);
        let ln_p = p_pair[0].ln() + (p_pair[1].ln() - p_pair[0].ln()) * t;
        return Some(ln_p.exp());
    }
    None
}

fn tp_to_base(t: f64, p: f64) -> (f32, f32) {
    let plot_w = SKEWT_LEFT_W * SKEWT_FRAC - SKEWT_MARGIN_LEFT - SKEWT_MARGIN_RIGHT;
    let plot_h = SKEWT_UPPER_H - SKEWT_MARGIN_TOP - SKEWT_MARGIN_BOT;
    let yn = (SKEWT_P_BOT.ln() - p.ln()) / (SKEWT_P_BOT.ln() - SKEWT_P_TOP.ln());
    let t_shifted = t + SKEWT_SKEW * (SKEWT_P_BOT.ln() - p.ln()) * 25.0;
    let xn = (t_shifted - SKEWT_T_MIN) / (SKEWT_T_MAX - SKEWT_T_MIN);
    (
        SKEWT_MARGIN_LEFT + (xn as f32) * plot_w,
        TITLE_H + SKEWT_MARGIN_TOP + (1.0 - yn as f32) * plot_h,
    )
}

#[derive(Debug, Clone, Copy)]
struct PanelTransform {
    center_x: f32,
    center_y: f32,
    zoom: f32,
    pan_x: f32,
    pan_y: f32,
}

impl PanelTransform {
    fn point(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.center_x + (x - self.center_x) * self.zoom + self.pan_x,
            self.center_y + (y - self.center_y) * self.zoom + self.pan_y,
        )
    }

    fn inverse(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.center_x + (x - self.center_x - self.pan_x) / self.zoom,
            self.center_y + (y - self.center_y - self.pan_y) / self.zoom,
        )
    }
}

fn draw_text_xform(
    painter: &egui::Painter,
    rect: SceneCanvas,
    xform: PanelTransform,
    x: f32,
    y: f32,
    text: &str,
    col: Color32,
    base_size: f32,
    align: Align2,
) {
    let (x, y) = xform.point(x, y);
    draw_text_base(
        painter,
        rect,
        x,
        y,
        text,
        col,
        base_size * xform.zoom,
        align,
    );
}

fn draw_line_xform(
    painter: &egui::Painter,
    rect: SceneCanvas,
    xform: PanelTransform,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    col: Color32,
    base_width: f32,
) {
    let (x0, y0) = xform.point(x0, y0);
    let (x1, y1) = xform.point(x1, y1);
    draw_line_base(painter, rect, x0, y0, x1, y1, col, base_width * xform.zoom);
}

fn draw_circle_xform(
    painter: &egui::Painter,
    rect: SceneCanvas,
    xform: PanelTransform,
    x: f32,
    y: f32,
    r: f32,
    col: Color32,
    base_width: f32,
) {
    let (x, y) = xform.point(x, y);
    draw_circle_base(
        painter,
        rect,
        x,
        y,
        r * xform.zoom,
        col,
        base_width * xform.zoom,
    );
}

fn draw_circle_filled_xform(
    painter: &egui::Painter,
    rect: SceneCanvas,
    xform: PanelTransform,
    x: f32,
    y: f32,
    r: f32,
    col: Color32,
) {
    let (x, y) = xform.point(x, y);
    draw_circle_filled_base(painter, rect, x, y, r * xform.zoom, col);
}

fn draw_text_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    text: &str,
    col: Color32,
    base_size: f32,
    align: Align2,
) {
    let scale = rect.scale();
    painter.text(
        base_pos(rect, x, y),
        align,
        text,
        FontId::monospace((base_size * TEXT_SCALE * rect.text_scale * scale).clamp(7.0, 44.0)),
        col,
    );
}

fn draw_line_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    col: Color32,
    base_width: f32,
) {
    let scale = rect.scale();
    painter.line_segment(
        [base_pos(rect, x0, y0), base_pos(rect, x1, y1)],
        Stroke::new((base_width * scale).max(0.75), col),
    );
}

fn draw_rect_filled_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    col: Color32,
) {
    painter.rect_filled(base_rect(rect, x, y, w, h), 0.0, col);
}

fn draw_rect_outline_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    col: Color32,
    base_width: f32,
) {
    draw_line_base(painter, rect, x, y, x + w, y, col, base_width);
    draw_line_base(painter, rect, x + w, y, x + w, y + h, col, base_width);
    draw_line_base(painter, rect, x + w, y + h, x, y + h, col, base_width);
    draw_line_base(painter, rect, x, y + h, x, y, col, base_width);
}

fn draw_circle_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    r: f32,
    col: Color32,
    base_width: f32,
) {
    let scale = rect.scale();
    painter.circle_stroke(
        base_pos(rect, x, y),
        r * scale,
        Stroke::new((base_width * scale).max(0.75), col),
    );
}

fn draw_circle_filled_base(
    painter: &egui::Painter,
    rect: SceneCanvas,
    x: f32,
    y: f32,
    r: f32,
    col: Color32,
) {
    let scale = rect.scale();
    painter.circle_filled(base_pos(rect, x, y), r * scale, col);
}

fn base_pos(rect: SceneCanvas, x: f32, y: f32) -> Pos2 {
    rect.map_pos(x, y)
}

fn base_rect(rect: SceneCanvas, x: f32, y: f32, w: f32, h: f32) -> Rect {
    rect.map_rect(x, y, w, h)
}

fn hodo_color(height_m: f64) -> Color32 {
    if height_m < 1000.0 {
        color(255, 60, 60)
    } else if height_m < 3000.0 {
        color(255, 165, 0)
    } else if height_m < 6000.0 {
        color(255, 255, 0)
    } else if height_m < 9000.0 {
        color(40, 230, 40)
    } else if height_m < 12_000.0 {
        color(60, 130, 255)
    } else {
        color(200, 90, 255)
    }
}

fn color(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

fn color_a(r: u8, g: u8, b: u8, a: u8) -> Color32 {
    Color32::from_rgba_premultiplied(r, g, b, a)
}

fn c_to_f(c: f64) -> f64 {
    c * 9.0 / 5.0 + 32.0
}

fn sounding_hover_text(
    response: &egui::Response,
    data: &SoundingData,
    zooms: &SoundingZooms,
    layout: &SoundingLayout,
) -> Option<String> {
    let pos = response.hover_pos()?;
    let (layout_x, layout_y) = response_pos_to_layout(response, pos, layout)?;
    let region = layout.region_at(layout_x, layout_y)?;
    let (base_x, base_y) = layout.layout_to_source(region, layout_x, layout_y);
    let texture_size = Vec2::new(FULL_IMAGE_W, FULL_IMAGE_H);
    match region {
        SoundingPanelRegion::Skewt => {
            skewt_hover_text(base_x, base_y, texture_size, data, zooms.skewt)
        }
        SoundingPanelRegion::Hodograph => {
            hodograph_hover_text(base_x, base_y, texture_size, data, zooms.hodograph)
        }
        SoundingPanelRegion::Slinky => {
            slinky_hover_text(base_x, base_y, texture_size, data, zooms.slinky)
        }
    }
}

fn skewt_hover_text(
    image_x: f32,
    image_y: f32,
    texture_size: Vec2,
    data: &SoundingData,
    view: PanelViewport,
) -> Option<String> {
    let sx = texture_size.x.max(1.0) / FULL_IMAGE_W;
    let sy = texture_size.y.max(1.0) / FULL_IMAGE_H;
    let base_x = image_x / sx;
    let base_y = image_y / sy;

    if !(0.0..=SKEWT_LEFT_W).contains(&base_x)
        || !(TITLE_H..=TITLE_H + SKEWT_UPPER_H).contains(&base_y)
    {
        return None;
    }

    let plot_left = SKEWT_MARGIN_LEFT;
    let plot_top = TITLE_H + SKEWT_MARGIN_TOP;
    let plot_w = SKEWT_LEFT_W * SKEWT_FRAC - SKEWT_MARGIN_LEFT - SKEWT_MARGIN_RIGHT;
    let plot_h = SKEWT_UPPER_H - SKEWT_MARGIN_TOP - SKEWT_MARGIN_BOT;
    let xform = view.transform_from_rect(plot_left, plot_top, plot_w, plot_h);
    let (base_x, base_y) = xform.inverse(base_x, base_y);

    if base_x < plot_left
        || base_x > plot_left + plot_w
        || base_y < plot_top
        || base_y > plot_top + plot_h
    {
        return None;
    }

    let xn = ((base_x - plot_left) / plot_w) as f64;
    let yn = (1.0 - ((base_y - plot_top) / plot_h)) as f64;
    let p_range = SKEWT_P_BOT.ln() - SKEWT_P_TOP.ln();
    let p_hpa = (SKEWT_P_BOT.ln() - yn * p_range).exp();
    let t_shifted = SKEWT_T_MIN + xn * (SKEWT_T_MAX - SKEWT_T_MIN);
    let cursor_t_c = t_shifted - SKEWT_SKEW * (SKEWT_P_BOT.ln() - p_hpa.ln()) * 25.0;

    let mut lines = vec![format!("Skew-T: {p_hpa:.0} hPa, {cursor_t_c:.1} C")];
    if let Some(level_hpa) = nearest_profile_level(data, p_hpa) {
        let mut parts = vec![format!("{level_hpa} hPa")];
        if let Some(t) = profile_value_c(data, "temperature_iso", level_hpa) {
            parts.push(format!("T {t:.1} C"));
        }
        if let Some(td) = profile_value_c(data, "dewpoint_iso", level_hpa) {
            parts.push(format!("Td {td:.1} C"));
        }
        if let (Some(u), Some(v)) = (
            profile_value(data, "u_iso", level_hpa),
            profile_value(data, "v_iso", level_hpa),
        ) {
            let (dir, spd) = wind_dir_speed(u, v);
            parts.push(format!("wind {dir:03.0}/{spd:.0} kt"));
        }
        lines.push(parts.join("  "));
    }
    Some(lines.join("\n"))
}

fn hodograph_hover_text(
    image_x: f32,
    image_y: f32,
    texture_size: Vec2,
    data: &SoundingData,
    view: PanelViewport,
) -> Option<String> {
    let sx = texture_size.x.max(1.0) / FULL_IMAGE_W;
    let sy = texture_size.y.max(1.0) / FULL_IMAGE_H;
    let base_x = image_x / sx;
    let base_y = image_y / sy;

    if !(HODO_X..=HODO_X + HODO_W).contains(&base_x)
        || !(HODO_Y..=HODO_Y + HODO_H).contains(&base_y)
    {
        return None;
    }

    let title_h = 30.0;
    let plot_top = title_h + 2.0;
    let plot_h = (f64::from(HODO_H) - title_h - 8.0).max(90.0);
    let plot_w = f64::from(HODO_W) - 16.0;
    let cx = f64::from(HODO_W) / 2.0;
    let cy = plot_top + plot_h / 2.0;
    let xform = view.transform_around(HODO_X + cx as f32, HODO_Y + cy as f32);
    let (base_x, base_y) = xform.inverse(base_x, base_y);
    let local_x = f64::from(base_x - HODO_X);
    let local_y = f64::from(base_y - HODO_Y);
    let max_radius = (plot_w.min(plot_h) / 2.0 - 8.0).max(30.0);
    let scale = max_radius / HODO_MAX_RING_KT;

    let u_kt = (local_x - cx) / scale;
    let v_kt = (cy - local_y) / scale;
    let (dir, spd) = dir_speed_from_uv_kt(u_kt, v_kt);
    let mut lines = vec![format!(
        "Hodograph: u {u_kt:.0} kt, v {v_kt:.0} kt ({dir:03.0}/{spd:.0})"
    )];

    if let Some(point) = nearest_wind_point(data, u_kt, v_kt) {
        let (pdir, pspd) = dir_speed_from_uv_kt(point.u_kt, point.v_kt);
        lines.push(format!(
            "nearest: {} hPa  {:.1} km AGL  {pdir:03.0}/{pspd:.0}",
            point.level_hpa,
            point.height_agl_m / 1000.0
        ));
    }
    Some(lines.join("\n"))
}

fn slinky_hover_text(
    image_x: f32,
    image_y: f32,
    texture_size: Vec2,
    data: &SoundingData,
    view: PanelViewport,
) -> Option<String> {
    let sx = texture_size.x.max(1.0) / FULL_IMAGE_W;
    let sy = texture_size.y.max(1.0) / FULL_IMAGE_H;
    let base_x = image_x / sx;
    let base_y = image_y / sy;

    if !(INSET_X..=INSET_X + INSET_W).contains(&base_x)
        || !(INSET_Y..=INSET_Y + INSET_H).contains(&base_y)
    {
        return None;
    }

    let (storm_u, storm_v) = bunkers_right_motion(data)?;
    let points = wind_profile_points(data);
    if points.is_empty() {
        return None;
    }

    let rx = f64::from(INSET_X);
    let ry = f64::from(INSET_Y);
    let rw = f64::from(INSET_W);
    let rh = f64::from(INSET_H);
    let sep_y = ry + 30.0;
    let plot_margin = 28.0;
    let plot_top = sep_y + 8.0;
    let plot_size = (rw - 2.0 * plot_margin).min(rh - (plot_top - ry) - plot_margin - 8.0);
    let cx = rx + rw / 2.0;
    let cy = plot_top + plot_size / 2.0;
    let xform = view.transform_around(cx as f32, cy as f32);
    let (base_x, base_y) = xform.inverse(base_x, base_y);
    let max_disp = points
        .iter()
        .map(|point| {
            let du = point.u_kt - storm_u;
            let dv = point.v_kt - storm_v;
            (du * du + dv * dv).sqrt()
        })
        .fold(0.0_f64, f64::max)
        .max(4.0);
    let scale = (plot_size / 2.0 - 8.0) / max_disp;

    let sr_u = (f64::from(base_x) - cx) / scale;
    let sr_v = (cy - f64::from(base_y)) / scale;
    let (dir, spd) = dir_speed_from_uv_kt(sr_u, sr_v);
    let mut lines = vec![format!(
        "Storm slinky: SR u {sr_u:.0} kt, SR v {sr_v:.0} kt ({dir:03.0}/{spd:.0})"
    )];

    if let Some(point) = nearest_sr_wind_point(&points, storm_u, storm_v, sr_u, sr_v) {
        let point_sr_u = point.u_kt - storm_u;
        let point_sr_v = point.v_kt - storm_v;
        let (pdir, pspd) = dir_speed_from_uv_kt(point_sr_u, point_sr_v);
        lines.push(format!(
            "nearest: {} hPa  {:.1} km AGL  SR {pdir:03.0}/{pspd:.0}",
            point.level_hpa,
            point.height_agl_m / 1000.0
        ));
    }
    let (rm_dir, rm_spd) = dir_speed_from_uv_kt(storm_u, storm_v);
    lines.push(format!("Bunkers RM approx: {rm_dir:03.0}/{rm_spd:.0}"));
    Some(lines.join("\n"))
}

fn nearest_profile_level(data: &SoundingData, p_hpa: f64) -> Option<u16> {
    let var = data
        .vars
        .iter()
        .find(|var| var.name == "temperature_iso")
        .or_else(|| data.vars.first())?;
    var.levels_hpa.iter().copied().min_by(|a, b| {
        let da = (f64::from(*a) - p_hpa).abs();
        let db = (f64::from(*b) - p_hpa).abs();
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn profile_value(data: &SoundingData, name: &str, level_hpa: u16) -> Option<f64> {
    let var = data.vars.iter().find(|var| var.name == name)?;
    let idx = var
        .levels_hpa
        .iter()
        .position(|&level| level == level_hpa)?;
    let value = f64::from(*var.values.get(idx)?);
    value.is_finite().then_some(value)
}

fn profile_value_c(data: &SoundingData, name: &str, level_hpa: u16) -> Option<f64> {
    if name == "dewpoint_iso" && !data.vars.iter().any(|var| var.name == "dewpoint_iso") {
        let t_c = profile_value_c(data, "temperature_iso", level_hpa)?;
        let rh = profile_value(data, "rh_iso", level_hpa)?;
        return Some(crate::skewt::dewpoint_c_from_rh(t_c, rh));
    }

    let var = data.vars.iter().find(|var| var.name == name)?;
    let idx = var
        .levels_hpa
        .iter()
        .position(|&level| level == level_hpa)?;
    let value = f64::from(*var.values.get(idx)?);
    if !value.is_finite() {
        return None;
    }
    Some(if var.units == "K" {
        value - 273.15
    } else {
        value
    })
}

fn wind_dir_speed(u_ms: f64, v_ms: f64) -> (f64, f64) {
    let spd_ms = (u_ms * u_ms + v_ms * v_ms).sqrt();
    if spd_ms < 1.0e-6 {
        return (0.0, 0.0);
    }
    let mut dir = u_ms.atan2(v_ms).to_degrees() + 180.0;
    if dir >= 360.0 {
        dir -= 360.0;
    }
    (dir, spd_ms * MS_TO_KT)
}

#[derive(Debug, Clone, Copy)]
struct WindPoint {
    level_hpa: u16,
    height_agl_m: f64,
    u_kt: f64,
    v_kt: f64,
}

fn nearest_wind_point(data: &SoundingData, u_kt: f64, v_kt: f64) -> Option<WindPoint> {
    wind_profile_points(data).into_iter().min_by(|a, b| {
        let da = (a.u_kt - u_kt).hypot(a.v_kt - v_kt);
        let db = (b.u_kt - u_kt).hypot(b.v_kt - v_kt);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn nearest_sr_wind_point(
    points: &[WindPoint],
    storm_u: f64,
    storm_v: f64,
    sr_u: f64,
    sr_v: f64,
) -> Option<WindPoint> {
    points.iter().copied().min_by(|a, b| {
        let da = (a.u_kt - storm_u - sr_u).hypot(a.v_kt - storm_v - sr_v);
        let db = (b.u_kt - storm_u - sr_u).hypot(b.v_kt - storm_v - sr_v);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    })
}

fn wind_profile_points(data: &SoundingData) -> Vec<WindPoint> {
    let mut points = Vec::new();
    let sfc_h = surface_value(data, "orography").unwrap_or(0.0);
    if let (Some(u), Some(v)) = (surface_value(data, "u_10m"), surface_value(data, "v_10m")) {
        points.push(WindPoint {
            level_hpa: surface_value(data, "surface_pressure")
                .unwrap_or(0.0)
                .round() as u16,
            height_agl_m: 10.0,
            u_kt: u * MS_TO_KT,
            v_kt: v * MS_TO_KT,
        });
    }

    let Some(height) = data.vars.iter().find(|var| var.name == "height_iso") else {
        return points;
    };
    let Some(u_wind) = data.vars.iter().find(|var| var.name == "u_iso") else {
        return points;
    };
    let Some(v_wind) = data.vars.iter().find(|var| var.name == "v_iso") else {
        return points;
    };
    for &level_hpa in &height.levels_hpa {
        let Some(h_msl) = profile_value(data, "height_iso", level_hpa) else {
            continue;
        };
        let Some(u_ms) = profile_value(data, "u_iso", level_hpa) else {
            continue;
        };
        let Some(v_ms) = profile_value(data, "v_iso", level_hpa) else {
            continue;
        };
        if !u_wind.levels_hpa.contains(&level_hpa) || !v_wind.levels_hpa.contains(&level_hpa) {
            continue;
        }
        points.push(WindPoint {
            level_hpa,
            height_agl_m: (h_msl - sfc_h).max(0.0),
            u_kt: u_ms * MS_TO_KT,
            v_kt: v_ms * MS_TO_KT,
        });
    }
    points.sort_by(|a, b| {
        a.height_agl_m
            .partial_cmp(&b.height_agl_m)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    points
}

fn bunkers_right_motion(data: &SoundingData) -> Option<(f64, f64)> {
    let points = wind_profile_points(data);
    let sfc = interp_wind_height(&points, 0.0)?;
    let six = interp_wind_height(&points, 6000.0)?;
    let mut sum_u = 0.0;
    let mut sum_v = 0.0;
    let mut count = 0.0;
    let mut h = 0.0;
    while h <= 6000.0 {
        if let Some(point) = interp_wind_height(&points, h) {
            sum_u += point.u_kt;
            sum_v += point.v_kt;
            count += 1.0;
        }
        h += 500.0;
    }
    if count == 0.0 {
        return None;
    }

    let mean_u = sum_u / count;
    let mean_v = sum_v / count;
    let shear_u = six.u_kt - sfc.u_kt;
    let shear_v = six.v_kt - sfc.v_kt;
    let shear_mag = shear_u.hypot(shear_v);
    if shear_mag < 1.0e-6 {
        return None;
    }
    let d = 7.5 * MS_TO_KT;
    let ratio = d / shear_mag;
    Some((mean_u + ratio * shear_v, mean_v - ratio * shear_u))
}

fn interp_wind_height(points: &[WindPoint], height_agl_m: f64) -> Option<WindPoint> {
    let first = *points.first()?;
    if height_agl_m <= first.height_agl_m {
        return Some(first);
    }
    for pair in points.windows(2) {
        let a = pair[0];
        let b = pair[1];
        if height_agl_m < a.height_agl_m || height_agl_m > b.height_agl_m {
            continue;
        }
        let span = (b.height_agl_m - a.height_agl_m).max(1.0);
        let t = ((height_agl_m - a.height_agl_m) / span).clamp(0.0, 1.0);
        return Some(WindPoint {
            level_hpa: a.level_hpa,
            height_agl_m,
            u_kt: a.u_kt + (b.u_kt - a.u_kt) * t,
            v_kt: a.v_kt + (b.v_kt - a.v_kt) * t,
        });
    }
    points.last().copied()
}

fn surface_value(data: &SoundingData, name: &str) -> Option<f64> {
    let sample = data.surface_value(name)?;
    let value = f64::from(sample.value);
    if !value.is_finite() {
        return None;
    }
    Some(match (name, sample.units.as_str()) {
        (_, "K") => value - 273.15,
        ("surface_pressure", "Pa") => value / 100.0,
        _ => value,
    })
}

fn dir_speed_from_uv_kt(u_kt: f64, v_kt: f64) -> (f64, f64) {
    let spd = u_kt.hypot(v_kt);
    if spd < 1.0e-6 {
        return (0.0, 0.0);
    }
    let mut dir = u_kt.atan2(v_kt).to_degrees() + 180.0;
    if dir >= 360.0 {
        dir -= 360.0;
    }
    (dir, spd)
}

/// Numeric table: rows = union of levels (descending pressure), one column
/// per 3D variable. Raw store values and units — no conversions.
fn show_level_table(ui: &mut Ui, data: &SoundingData, table_text_scale: f32) {
    if data.vars.is_empty() {
        ui.label(RichText::new("This hour has no 3D pressure-level variables.").weak());
        return;
    }
    let header_size = (13.0 * table_text_scale).clamp(10.0, 26.0);
    let row_size = (12.0 * table_text_scale).clamp(9.0, 24.0);
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
                    ui.label(RichText::new("hPa").strong().size(header_size));
                    for var in &data.vars {
                        ui.label(RichText::new(&var.name).strong().size(header_size))
                            .on_hover_text(format!("units: {}", var.units));
                    }
                    ui.end_row();
                    for &level in &levels {
                        ui.label(RichText::new(format!("{level}")).size(row_size));
                        for var in &data.vars {
                            let value = var
                                .levels_hpa
                                .iter()
                                .position(|&have| have == level)
                                .map(|i| var.values[i]);
                            match value {
                                Some(v) if v.is_finite() => {
                                    ui.label(RichText::new(format!("{v:.1}")).size(row_size))
                                }
                                Some(_) => ui.label(RichText::new("—").size(row_size)),
                                None => ui.label(RichText::new("").size(row_size)),
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

    #[test]
    fn panel_transform_round_trips_zoomed_points() {
        let view = PanelViewport {
            zoom: 2.5,
            pan_x: 40.0,
            pan_y: -20.0,
        };
        let xform = view.transform_from_rect(10.0, 20.0, 300.0, 200.0);
        let original = (42.0, 88.0);
        let zoomed = xform.point(original.0, original.1);
        let round_trip = xform.inverse(zoomed.0, zoomed.1);
        assert!((round_trip.0 - original.0).abs() < 1.0e-4);
        assert!((round_trip.1 - original.1).abs() < 1.0e-4);
    }

    #[test]
    fn panel_viewport_zoom_at_keeps_cursor_anchor() {
        let mut view = PanelViewport::default();
        let center = (400.0, 300.0);
        let cursor = (525.0, 260.0);
        let before = view
            .transform_around(center.0, center.1)
            .inverse(cursor.0, cursor.1);
        view.zoom_at(cursor.0, cursor.1, 2.0, center.0, center.1);
        let after = view
            .transform_around(center.0, center.1)
            .inverse(cursor.0, cursor.1);
        assert!((after.0 - before.0).abs() < 1.0e-4);
        assert!((after.1 - before.1).abs() < 1.0e-4);
    }

    #[test]
    fn pressure_value_interpolation_uses_log_pressure() {
        let pressure = [1000.0, 500.0];
        let values = [10.0, 30.0];
        let target = (1000.0_f64 * 500.0).sqrt();
        let value = interp_pressure_value(&pressure, &values, target).unwrap();
        assert!(
            (value - 20.0).abs() < 1.0e-6,
            "got {value}, expected midpoint in log-pressure space"
        );
    }

    #[test]
    fn sounding_view_state_json_round_trips_overlay_styles() {
        let mut panel = SoundingPanel::new();
        panel.zooms.scene = 1.42;
        panel.zooms.hodograph.zoom = 1.7;
        panel.overlays.ml_parcel_style = SoundingTraceStyle::Dotted;
        panel.overlays.cape_cin_fill = false;
        panel.layout.canvas_w = 1800.0;

        let value = panel.view_state_json();

        let mut restored = SoundingPanel::new();
        assert!(restored.apply_view_state_json(&value));
        assert!((restored.zooms.scene - 1.42).abs() < 1.0e-6);
        assert!((restored.zooms.hodograph.zoom - 1.7).abs() < 1.0e-6);
        assert_eq!(
            restored.overlays.ml_parcel_style,
            SoundingTraceStyle::Dotted
        );
        assert!(!restored.overlays.cape_cin_fill);
        assert!((restored.layout.canvas_w - 1800.0).abs() < 1.0e-6);
    }

    #[test]
    fn has_content_tracks_state() {
        let mut panel = SoundingPanel::new();
        assert!(!panel.has_content());
        panel.set_loading();
        assert!(panel.has_content());
        panel.set_data(SoundingData {
            hour: HourKey {
                model: "m".into(),
                run: "r".into(),
                hour: 0,
            },
            fx: 1.0,
            fy: 2.0,
            lat: None,
            lon: None,
            vars: vec![],
            surface: vec![],
            read_ms: 0.0,
        });
        assert!(panel.has_content());
        panel.clear();
        assert!(!panel.has_content());
    }

    #[test]
    fn loading_next_profile_keeps_existing_ready_scene() {
        let mut panel = SoundingPanel::new();
        panel.set_data(SoundingData {
            hour: HourKey {
                model: "m".into(),
                run: "r".into(),
                hour: 0,
            },
            fx: 1.0,
            fy: 2.0,
            lat: None,
            lon: None,
            vars: vec![],
            surface: vec![],
            read_ms: 0.0,
        });
        assert!(matches!(panel.state, SoundingState::Ready(_)));
        panel.set_loading();
        assert!(panel.loading);
        assert!(
            matches!(panel.state, SoundingState::Ready(_)),
            "drag sampling must keep painting the previous sounding while the next point loads"
        );
    }

    /// An hour without the skew-T inputs still becomes Ready (table-only),
    /// carrying the reason instead of a rendered image.
    #[test]
    fn set_data_without_skewt_inputs_keeps_the_error() {
        let mut panel = SoundingPanel::new();
        panel.set_data(SoundingData {
            hour: HourKey {
                model: "synthetic".into(),
                run: "20260609_00z".into(),
                hour: 0,
            },
            fx: 1.0,
            fy: 2.0,
            lat: Some(31.0),
            lon: Some(-100.0),
            vars: vec![],
            surface: vec![],
            read_ms: 0.0,
        });
        match &panel.state {
            SoundingState::Ready(ready) => match &ready.scene {
                Err(err) => assert!(err.contains("temperature_iso"), "got: {err}"),
                Ok(_) => panic!("no inputs must not produce a skew-T"),
            },
            _ => panic!("set_data must land in Ready"),
        }
    }
}

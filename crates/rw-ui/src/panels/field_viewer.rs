//! Field viewer: pick a 2D variable and inspect it as a false-color texture.
//!
//! Variables with a production plot counterpart render through the EXACT
//! production colortable (`rustwx_products::viewer` resolves the style; the
//! pixels come from `rustwx_render::LeveledColormap::map`, the same function
//! the PNG rasterizer calls), with a legend whose swatch colors, tick
//! values, and labels are the production colorbar's. Variables with no
//! counterpart keep a clearly-labeled generic min..max viridis ramp.
//! The texture is cached and re-uploaded only when the loaded field changes;
//! per-frame work is just drawing the cached textures.

use std::{
    sync::{
        Arc,
        mpsc::{Receiver, TryRecvError, channel},
    },
    time::Duration,
};

use egui::{
    Align2, Color32, ColorImage, ComboBox, FontId, Image, PointerButton, Pos2, Rect, RichText,
    Sense, Slider, Stroke, StrokeKind, TextureFilter, TextureHandle, TextureOptions, Ui, Vec2,
    pos2,
};
use rustwx_render::{
    BasemapDetail, BasemapStyle, Color, ColorScale, ColormapBuildOptions, DiscreteColorScale,
    ExtendMode, LegendControls, LegendMode, LeveledColormap, LevelDensity, LineworkRole,
    RenderDensity, Rgba, StaticPlotStyle, build_colormap, colorbar_ticks, format_tick,
    legend_color_at_rel, legend_tick_rel, load_styled_basemap_features_for_detail,
};
use rw_store::grid::{GridFile, GridLocator};

use crate::colormap::{Colormap, VIRIDIS, field_to_color_image, field_to_production_color_image};
use crate::iso_levels::parse_iso_slug;
use crate::profile_scope;
use crate::style_overrides::StyleOverrideSettings;
use crate::worker::{FieldData, FieldKey, HourKey, VarInfo, VarKind};

use super::plot_viewer::CustomDomain;

/// Horizontal room reserved for the production legend (bar + ticks + labels).
const LEGEND_WIDTH: f32 = 78.0;
/// Vertical resolution of the legend bar texture (one production colorbar
/// sample per row, matching `draw_vertical_colorbar`'s per-pixel sampling).
const LEGEND_BAR_RESOLUTION: usize = 512;
const RAW_BASEMAP_DEFAULT_OPACITY: f32 = 0.9;
const RAW_BASEMAP_DEFAULT_WIDTH_SCALE: f32 = 1.0;
const RAW_BASEMAP_GEO_PAD_DEG: f64 = 2.5;

/// What the user did this frame; the host turns these into worker requests.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldViewerEvent {
    /// A different 2D variable was picked.
    VarSelected(String),
    /// The field was clicked at fractional grid coordinates.
    PointClicked { fx: f64, fy: f64 },
    /// A custom render domain was box-selected from the displayed field.
    DomainSelected(CustomDomain),
    /// The selected custom render domain was rotated on the displayed field.
    DomainRotationChanged { rotation_deg: f64 },
}

#[derive(Debug, Default, PartialEq)]
enum LoadState {
    #[default]
    Idle,
    Loading(String),
    Error(String),
    Ready,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum FieldSamplingMode {
    PixelExact,
    #[default]
    Smooth,
}

impl FieldSamplingMode {
    fn label(self) -> &'static str {
        match self {
            Self::PixelExact => "Pixel exact",
            Self::Smooth => "Smooth",
        }
    }

    fn texture_options(self) -> TextureOptions {
        match self {
            Self::PixelExact => TextureOptions {
                magnification: TextureFilter::Nearest,
                minification: TextureFilter::Nearest,
                ..Default::default()
            },
            Self::Smooth => TextureOptions {
                magnification: TextureFilter::Linear,
                minification: TextureFilter::Linear,
                ..Default::default()
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RawBasemapMode {
    Off,
    Global,
    #[default]
    Broad,
    Regional,
    Counties,
}

impl RawBasemapMode {
    const ALL: [Self; 5] = [
        Self::Off,
        Self::Global,
        Self::Broad,
        Self::Regional,
        Self::Counties,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Global => "Global",
            Self::Broad => "Broad",
            Self::Regional => "Regional",
            Self::Counties => "Counties",
        }
    }

    fn detail(self) -> Option<BasemapDetail> {
        match self {
            Self::Off => None,
            Self::Global => Some(BasemapDetail::Global),
            Self::Broad => Some(BasemapDetail::Broad),
            Self::Regional | Self::Counties => Some(BasemapDetail::Regional),
        }
    }

    fn includes_role(self, role: LineworkRole) -> bool {
        match self {
            Self::Off => false,
            Self::Global => !matches!(role, LineworkRole::County | LineworkRole::State),
            Self::Broad | Self::Regional => !matches!(role, LineworkRole::County),
            Self::Counties => true,
        }
    }

    fn max_segment_deg(self, role: LineworkRole) -> f64 {
        match (self, role) {
            (Self::Counties, LineworkRole::County) => 0.25,
            (Self::Regional | Self::Counties, _) => 0.20,
            (Self::Broad, _) => 0.45,
            (Self::Global, _) => 0.9,
            (Self::Off, _) => 1.0,
        }
    }

    fn max_located_points(self, role: LineworkRole) -> usize {
        match (self, role) {
            (Self::Counties, LineworkRole::County) => 180_000,
            (Self::Regional | Self::Counties, _) => 120_000,
            (Self::Broad, _) => 90_000,
            (Self::Global, _) => 70_000,
            (Self::Off, _) => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RawBasemapTone {
    Subtle,
    #[default]
    Normal,
    Strong,
}

impl RawBasemapTone {
    const ALL: [Self; 3] = [Self::Subtle, Self::Normal, Self::Strong];

    fn label(self) -> &'static str {
        match self {
            Self::Subtle => "Subtle",
            Self::Normal => "Normal",
            Self::Strong => "Strong",
        }
    }

    fn alpha_multiplier(self) -> f32 {
        match self {
            Self::Subtle => 0.62,
            Self::Normal => 1.0,
            Self::Strong => 1.22,
        }
    }

    fn rgb_multiplier(self) -> f32 {
        match self {
            Self::Subtle => 1.08,
            Self::Normal => 0.86,
            Self::Strong => 0.55,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RawBasemapCacheKey {
    grid_hash: String,
    mode: RawBasemapMode,
}

struct RawBasemapBuild {
    key: RawBasemapCacheKey,
    rx: Receiver<RawBasemapCache>,
}

#[derive(Debug, Clone)]
struct RawBasemapCache {
    key: RawBasemapCacheKey,
    layers: Vec<RawBasemapLayer>,
    build_ms: f32,
    point_count: usize,
}

#[derive(Debug, Clone)]
struct RawBasemapLayer {
    lines: Vec<Vec<(f64, f64)>>,
    color: Rgba,
    width: u32,
    role: LineworkRole,
}

/// A Formula Lab result kept in its unconverted scientific units plus the
/// currently resolved display style. Every restyle starts from `raw`, so unit
/// conversion is reversible and can never compound across palette edits.
#[derive(Debug, Clone)]
struct GeneratedField {
    raw: FieldData,
    style: Option<rustwx_products::viewer::StoreVariableStyle>,
}

impl GeneratedField {
    fn from_raw(mut raw: FieldData, settings: &StyleOverrideSettings) -> Self {
        // Formula Lab hands us native output values. Recompute this metadata
        // here so the retained source cannot inherit display state.
        raw.range = crate::colormap::finite_min_max(&raw.values);
        raw.style = None;
        let style = generated_field_style(&raw, settings);
        Self { raw, style }
    }

    fn restyle(&mut self, settings: &StyleOverrideSettings) {
        self.style = generated_field_style(&self.raw, settings);
    }

    fn display_units(&self) -> &str {
        self.style
            .as_ref()
            .map(|style| style.display_units.as_str())
            .unwrap_or(&self.raw.units)
    }

    fn display_field(&self) -> FieldData {
        let mut displayed = self.raw.clone();
        if let Some(style) = &self.style {
            if !style.convert.is_none() {
                for value in &mut displayed.values {
                    *value = style.convert.apply(*value);
                }
            }
            displayed.units = style.display_units.clone();
        }
        displayed.range = crate::colormap::finite_min_max(&displayed.values);
        displayed.style = self.style.clone();
        displayed
    }
}

fn generated_field_style(
    raw: &FieldData,
    settings: &StyleOverrideSettings,
) -> Option<rustwx_products::viewer::StoreVariableStyle> {
    let resolved = settings.style_for_store_variable(
        &raw.key.var,
        &serde_json::Value::Null,
        &raw.units,
        raw.key.hour.model.parse::<rustwx_core::ModelId>().ok(),
    );
    if settings.binding_for_product(&raw.key.var).is_some() {
        return resolved;
    }
    Some(auto_generated_field_style(raw))
}

/// Neutral, non-meteorological Formula Lab fallback. The full finite range is
/// represented (including outliers); constant fields get display padding only,
/// and fields with no finite samples get an explicitly labeled placeholder
/// scale because the native renderer still requires ordered levels.
fn auto_generated_field_style(
    raw: &FieldData,
) -> rustwx_products::viewer::StoreVariableStyle {
    const COLORS: [[u8; 4]; 9] = [
        [68, 1, 84, 255],
        [72, 40, 120, 255],
        [62, 74, 137, 255],
        [49, 104, 142, 255],
        [38, 130, 142, 255],
        [31, 158, 137, 255],
        [53, 183, 121, 255],
        [109, 205, 89, 255],
        [253, 231, 37, 255],
    ];
    let (range, range_note) = match raw.range {
        Some((lo, hi)) if lo.is_finite() && hi.is_finite() && lo < hi => {
            ((f64::from(lo), f64::from(hi)), "full finite range")
        }
        Some((value, _)) if value.is_finite() => {
            let center = f64::from(value);
            let padded = if center == 0.0 {
                (-1.0, 1.0)
            } else {
                let padding = (center.abs() * 0.05).max(1.0e-6);
                (center - padding, center + padding)
            };
            (padded, "constant field")
        }
        _ => ((0.0, 1.0), "no finite values"),
    };
    let levels = (0..=COLORS.len())
        .map(|index| {
            range.0 + (range.1 - range.0) * index as f64 / COLORS.len() as f64
        })
        .collect();
    rustwx_products::viewer::StoreVariableStyle {
        title: format!("{} (Formula Lab auto, {range_note})", raw.key.var),
        display_units: raw.units.clone(),
        convert: rustwx_products::viewer::UnitConvert::None,
        scale: ColorScale::Discrete(DiscreteColorScale {
            levels,
            colors: COLORS
                .into_iter()
                .map(|[r, g, b, a]| Color::rgba(r, g, b, a))
                .collect(),
            extend: ExtendMode::Neither,
            mask_below: None,
        }),
        colormap_options: ColormapBuildOptions {
            render_density: StaticPlotStyle::from_env()
                .render_density(RenderDensity::default()),
            legend: LegendControls {
                density: LevelDensity::default(),
                mode: LegendMode::SmoothRamp,
            },
        },
        cbar_tick_step: None,
        legend_mode: LegendMode::SmoothRamp,
    }
}

/// False-color 2D field inspector. Pure widget over host-pushed data:
/// `set_hour` -> `set_loading` -> `set_field`/`set_error`, render with `ui`.
pub struct FieldViewerPanel {
    hour: Option<HourKey>,
    vars: Vec<VarInfo>,
    selected_var: Option<String>,
    var_filter: String,
    field: Option<FieldData>,
    /// One ephemeral Formula Lab field retained while this hour is selected.
    generated_field: Option<GeneratedField>,
    texture: Option<TextureHandle>,
    texture_dirty: bool,
    state: LoadState,
    /// Last clicked point in fractional grid coords (marker overlay).
    clicked: Option<(f64, f64)>,
    /// Ctrl+Alt smooth-sounding mode: the last grid cell a live sounding was
    /// fired for, so we only regenerate when the pointer enters a new cell.
    live_sounding_cell: Option<(usize, usize)>,
    /// In-progress shift + pointer-button custom-domain selection.
    domain_drag: Option<DomainDrag>,
    /// In-progress shift + pointer-button corner rotation.
    domain_rotate: Option<DomainRotate>,
    /// Last completed custom-domain selection in grid coordinates.
    domain_selection: Option<DomainSelection>,
    /// Display viewport in full-grid edge coordinates. None means full field.
    view: Option<GridViewport>,
    colormap: Colormap,
    /// The production colormap for the loaded field (None = generic ramp).
    cmap: Option<LeveledColormap>,
    legend_texture: Option<TextureHandle>,
    sampling_mode: FieldSamplingMode,
    basemap_mode: RawBasemapMode,
    basemap_tone: RawBasemapTone,
    basemap_opacity: f32,
    basemap_width_scale: f32,
    basemap_cache: Option<RawBasemapCache>,
    basemap_pending: Option<RawBasemapBuild>,
    /// Wall time of the last colormap + texture-upload pass, for the
    /// always-on stats strip.
    last_texture_ms: Option<f32>,
}

impl Default for FieldViewerPanel {
    fn default() -> Self {
        Self {
            hour: None,
            vars: Vec::new(),
            selected_var: None,
            var_filter: String::new(),
            field: None,
            generated_field: None,
            texture: None,
            texture_dirty: false,
            state: LoadState::Idle,
            clicked: None,
            live_sounding_cell: None,
            domain_drag: None,
            domain_rotate: None,
            domain_selection: None,
            view: None,
            colormap: VIRIDIS,
            cmap: None,
            legend_texture: None,
            sampling_mode: FieldSamplingMode::default(),
            basemap_mode: RawBasemapMode::default(),
            basemap_tone: RawBasemapTone::default(),
            basemap_opacity: RAW_BASEMAP_DEFAULT_OPACITY,
            basemap_width_scale: RAW_BASEMAP_DEFAULT_WIDTH_SCALE,
            basemap_cache: None,
            basemap_pending: None,
            last_texture_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DomainDrag {
    start: Pos2,
    current: Pos2,
    button: PointerButton,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DomainRotate {
    center: Pos2,
    button: PointerButton,
    start_pointer_angle_deg: f64,
    start_rotation_deg: f64,
    current_rotation_deg: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DomainSelection {
    fx0: f64,
    fy0: f64,
    fx1: f64,
    fy1: f64,
    rotation_deg: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct GridViewport {
    x0: f64,
    x1: f64,
    y0: f64,
    y1: f64,
}

impl GridViewport {
    fn full(nx: usize, ny: usize) -> Self {
        Self {
            x0: 0.0,
            x1: nx.max(1) as f64,
            y0: 0.0,
            y1: ny.max(1) as f64,
        }
    }

    fn width_cells(self) -> f64 {
        (self.x1 - self.x0).max(1.0)
    }

    fn height_cells(self) -> f64 {
        (self.y1 - self.y0).max(1.0)
    }

    fn is_full(self, nx: usize, ny: usize) -> bool {
        self.x0 <= 1.0e-6
            && self.y0 <= 1.0e-6
            && (self.x1 - nx.max(1) as f64).abs() <= 1.0e-6
            && (self.y1 - ny.max(1) as f64).abs() <= 1.0e-6
    }

    fn clamped(self, nx: usize, ny: usize) -> Self {
        let full_w = nx.max(1) as f64;
        let full_h = ny.max(1) as f64;
        let width = self.width_cells().min(full_w);
        let height = self.height_cells().min(full_h);
        let x0 = self.x0.clamp(0.0, (full_w - width).max(0.0));
        let y0 = self.y0.clamp(0.0, (full_h - height).max(0.0));
        Self {
            x0,
            x1: x0 + width,
            y0,
            y1: y0 + height,
        }
    }

    fn translated(self, dx: f64, dy: f64, nx: usize, ny: usize) -> Self {
        Self {
            x0: self.x0 + dx,
            x1: self.x1 + dx,
            y0: self.y0 + dy,
            y1: self.y1 + dy,
        }
        .clamped(nx, ny)
    }

    fn zoomed_at(self, anchor_fx: f64, anchor_fy: f64, factor: f64, nx: usize, ny: usize) -> Self {
        let full_w = nx.max(1) as f64;
        let full_h = ny.max(1) as f64;
        let min_w = full_w.min(8.0);
        let min_h = full_h.min(8.0);
        let factor = factor.clamp(0.2, 5.0);
        let old_w = self.width_cells();
        let old_h = self.height_cells();
        let new_w = (old_w / factor).clamp(min_w, full_w);
        let new_h = (old_h / factor).clamp(min_h, full_h);
        let anchor_x = (anchor_fx + 0.5).clamp(0.0, full_w);
        let anchor_y = (anchor_fy + 0.5).clamp(0.0, full_h);
        let rel_x = ((anchor_x - self.x0) / old_w).clamp(0.0, 1.0);
        let rel_y = ((anchor_y - self.y0) / old_h).clamp(0.0, 1.0);
        let x0 = anchor_x - rel_x * new_w;
        let y0 = anchor_y - rel_y * new_h;
        Self {
            x0,
            x1: x0 + new_w,
            y0,
            y1: y0 + new_h,
        }
        .clamped(nx, ny)
    }

    fn texture_uv_rect(self, nx: usize, ny: usize, flip_y: bool) -> Rect {
        let nx = nx.max(1) as f64;
        let ny = ny.max(1) as f64;
        let left = (self.x0 / nx) as f32;
        let right = (self.x1 / nx) as f32;
        let (top, bottom) = if flip_y {
            ((1.0 - self.y1 / ny) as f32, (1.0 - self.y0 / ny) as f32)
        } else {
            ((self.y0 / ny) as f32, (self.y1 / ny) as f32)
        };
        Rect::from_min_max(pos2(left, top), pos2(right, bottom))
    }

    fn image_uv_to_grid(self, u: f64, v: f64, nx: usize, ny: usize, flip_y: bool) -> (f64, f64) {
        let u = u.clamp(0.0, 1.0);
        let v = v.clamp(0.0, 1.0);
        let row_edge = if flip_y {
            self.y1 - v * self.height_cells()
        } else {
            self.y0 + v * self.height_cells()
        };
        let fx = (self.x0 + u * self.width_cells() - 0.5).clamp(0.0, (nx - 1) as f64);
        let fy = (row_edge - 0.5).clamp(0.0, (ny - 1) as f64);
        (fx, fy)
    }

    fn grid_to_image_uv(
        self,
        fx: f64,
        fy: f64,
        _nx: usize,
        _ny: usize,
        flip_y: bool,
    ) -> (f64, f64) {
        let u = ((fx + 0.5 - self.x0) / self.width_cells()).clamp(0.0, 1.0);
        let row_edge = fy + 0.5;
        let v = if flip_y {
            (self.y1 - row_edge) / self.height_cells()
        } else {
            (row_edge - self.y0) / self.height_cells()
        }
        .clamp(0.0, 1.0);
        (u, v)
    }

    fn grid_to_image_uv_unclamped(self, fx: f64, fy: f64, flip_y: bool) -> (f64, f64) {
        let u = (fx + 0.5 - self.x0) / self.width_cells();
        let row_edge = fy + 0.5;
        let v = if flip_y {
            (self.y1 - row_edge) / self.height_cells()
        } else {
            (row_edge - self.y0) / self.height_cells()
        };
        (u, v)
    }
}

impl FieldViewerPanel {
    pub fn new() -> Self {
        Self {
            colormap: VIRIDIS,
            sampling_mode: FieldSamplingMode::default(),
            basemap_mode: RawBasemapMode::default(),
            basemap_tone: RawBasemapTone::default(),
            basemap_opacity: RAW_BASEMAP_DEFAULT_OPACITY,
            basemap_width_scale: RAW_BASEMAP_DEFAULT_WIDTH_SCALE,
            ..Self::default()
        }
    }

    /// Install a new hour's variable list. Keeps the current variable
    /// selection when the new hour still has it; otherwise falls back to
    /// `temperature_2m`, then the first 2D variable. The host should then
    /// fire a load for [`FieldViewerPanel::selected_var`].
    pub fn set_hour(&mut self, hour: HourKey, mut vars: Vec<VarInfo>) {
        let generated = self
            .generated_field
            .take()
            .filter(|field| field.raw.key.hour == hour)
            .filter(|field| !vars.iter().any(|var| var.name == field.raw.key.var));
        if let Some(field) = &generated {
            vars.push(VarInfo {
                name: field.raw.key.var.clone(),
                units: field.display_units().to_string(),
                kind: VarKind::Surface2D,
                levels_hpa: Vec::new(),
            });
        }
        let keep = self.selected_var.take().filter(|name| {
            vars.iter()
                .any(|v| v.kind == VarKind::Surface2D && v.name == *name)
        });
        self.selected_var = keep
            .or_else(|| {
                vars.iter()
                    .find(|v| v.kind == VarKind::Surface2D && v.name == "temperature_2m")
                    .map(|v| v.name.clone())
            })
            .or_else(|| {
                vars.iter()
                    .find(|v| v.kind == VarKind::Surface2D)
                    .map(|v| v.name.clone())
            });
        self.hour = Some(hour);
        self.vars = vars;
        self.generated_field = generated;
        self.var_filter.clear();
        self.field = None;
        self.texture = None;
        self.texture_dirty = false;
        self.clicked = None;
        self.domain_drag = None;
        self.domain_rotate = None;
        self.domain_selection = None;
        self.view = None;
        self.cmap = None;
        self.legend_texture = None;
        self.basemap_pending = None;
        self.state = LoadState::Idle;
    }

    pub fn hour(&self) -> Option<&HourKey> {
        self.hour.as_ref()
    }

    pub fn selected_var(&self) -> Option<&str> {
        self.selected_var.as_deref()
    }

    /// Variables currently advertised for the selected hour. Hosts use this
    /// read-only view for equation editors and other field pickers.
    pub fn vars(&self) -> &[VarInfo] {
        &self.vars
    }

    /// Key of the field the panel currently wants loaded, if any.
    pub fn wanted_field(&self) -> Option<FieldKey> {
        match (&self.hour, &self.selected_var) {
            (Some(hour), Some(var)) => Some(FieldKey {
                hour: hour.clone(),
                var: var.clone(),
            }),
            _ => None,
        }
    }

    pub fn set_loading(&mut self, var: &str) {
        self.state = LoadState::Loading(var.to_string());
    }

    pub fn set_error(&mut self, message: String) {
        self.state = LoadState::Error(message);
    }

    /// Install a loaded field. Stale responses (different hour/var than the
    /// current selection) are ignored.
    pub fn set_field(&mut self, data: FieldData) {
        if Some(&data.key) != self.wanted_field().as_ref() {
            return;
        }
        let dimensions_changed = self
            .field
            .as_ref()
            .is_some_and(|field| field.nx != data.nx || field.ny != data.ny);
        if dimensions_changed {
            self.view = None;
            self.clicked = None;
            self.domain_selection = None;
            self.domain_drag = None;
            self.domain_rotate = None;
            self.basemap_pending = None;
        }
        self.field = Some(data);
        self.texture_dirty = true;
        self.state = LoadState::Ready;
    }

    /// Install an in-memory generated field (for example Formula Lab output)
    /// as the current selection without requiring it to exist in rw-store.
    /// It remains in the picker until the user selects another hour.
    pub fn install_generated_field(
        &mut self,
        mut data: FieldData,
        settings: &StyleOverrideSettings,
    ) {
        if self.hour.as_ref() != Some(&data.key.hour) {
            self.vars.clear();
        }
        if let Some(previous) = self.generated_field.take() {
            self.vars.retain(|var| var.name != previous.raw.key.var);
        }
        if self.vars.iter().any(|var| var.name == data.key.var) {
            let base = format!("formula_{}", data.key.var);
            let mut candidate = base.clone();
            let mut suffix = 2usize;
            while self.vars.iter().any(|var| var.name == candidate) {
                candidate = format!("{base}_{suffix}");
                suffix += 1;
            }
            data.key.var = candidate;
        }
        let generated = GeneratedField::from_raw(data, settings);
        let displayed = generated.display_field();
        self.hour = Some(generated.raw.key.hour.clone());
        self.selected_var = Some(generated.raw.key.var.clone());
        if !self
            .vars
            .iter()
            .any(|var| var.name == generated.raw.key.var)
        {
            self.vars.push(VarInfo {
                name: generated.raw.key.var.clone(),
                units: generated.display_units().to_string(),
                kind: VarKind::Surface2D,
                levels_hpa: Vec::new(),
            });
        }
        self.field = None;
        self.texture = None;
        self.texture_dirty = false;
        self.clicked = None;
        self.domain_drag = None;
        self.domain_rotate = None;
        self.domain_selection = None;
        self.cmap = None;
        self.legend_texture = None;
        self.basemap_pending = None;
        self.generated_field = Some(generated);
        self.set_field(displayed);
    }

    /// Re-resolve the retained Formula Lab result from its raw values and
    /// immediately refresh it when it is the current selection. Returns true
    /// only when the selected field was the generated field, allowing the host
    /// to avoid a store load for an intentionally in-memory variable.
    pub fn restyle_generated_field(&mut self, settings: &StyleOverrideSettings) -> bool {
        let wanted = self.wanted_field();
        let Some(generated) = self.generated_field.as_mut() else {
            return false;
        };
        generated.restyle(settings);
        let var = generated.raw.key.var.clone();
        let units = generated.display_units().to_string();
        let displayed = (wanted.as_ref() == Some(&generated.raw.key))
            .then(|| generated.display_field());
        if let Some(info) = self.vars.iter_mut().find(|info| info.name == var) {
            info.units = units;
        }
        let Some(displayed) = displayed else {
            return false;
        };
        self.set_field(displayed);
        true
    }

    /// Restore the retained Formula Lab field instead of asking the store
    /// worker for a variable that intentionally exists only in memory.
    pub fn restore_generated_field(&mut self, var: &str) -> bool {
        let Some(field) = self.generated_field.as_ref() else {
            return false;
        };
        if field.raw.key.var != var || Some(&field.raw.key.hour) != self.hour.as_ref() {
            return false;
        }
        let field = field.display_field();
        self.set_field(field);
        true
    }

    pub fn clear(&mut self) {
        *self = Self {
            colormap: self.colormap,
            sampling_mode: self.sampling_mode,
            basemap_mode: self.basemap_mode,
            basemap_tone: self.basemap_tone,
            basemap_opacity: self.basemap_opacity,
            basemap_width_scale: self.basemap_width_scale,
            basemap_cache: self.basemap_cache.clone(),
            basemap_pending: None,
            ..Self::default()
        };
    }

    /// Wall time of the last colormap + texture-upload pass (stats strip).
    pub fn last_texture_ms(&self) -> Option<f32> {
        self.last_texture_ms
    }

    pub fn current_field(&self) -> Option<&FieldData> {
        self.field.as_ref()
    }

    /// Render the variable picker + field image. Returns at most one event.
    pub fn ui(&mut self, ui: &mut Ui) -> Option<FieldViewerEvent> {
        let mut event = None;
        self.poll_raw_basemap_build(ui);

        ui.horizontal_wrapped(|ui| {
            let previous = self.selected_var.clone();
            let mut current = previous.clone().unwrap_or_default();
            let surface_total = self
                .vars
                .iter()
                .filter(|v| v.kind == VarKind::Surface2D)
                .count();
            ui.add(
                egui::TextEdit::singleline(&mut self.var_filter)
                    .desired_width(130.0)
                    .hint_text("filter variables"),
            );
            if !self.var_filter.is_empty() && ui.button("x").on_hover_text("clear filter").clicked()
            {
                self.var_filter.clear();
            }
            let filter = self.var_filter.trim().to_ascii_lowercase();
            let matching_vars = self
                .vars
                .iter()
                .filter(|v| v.kind == VarKind::Surface2D)
                .filter(|v| {
                    let label = display_variable_name(&v.name).to_ascii_lowercase();
                    filter.is_empty()
                        || v.name.to_ascii_lowercase().contains(&filter)
                        || label.contains(&filter)
                        || v.units.to_ascii_lowercase().contains(&filter)
                })
                .map(|v| {
                    (
                        v.name.clone(),
                        display_variable_name(&v.name),
                        v.units.clone(),
                    )
                })
                .collect::<Vec<_>>();
            let current_label = if current.is_empty() {
                "pick a variable".to_string()
            } else {
                display_variable_name(&current)
            };
            ComboBox::from_id_salt("rw-ui-field-var")
                .selected_text(current_label)
                .width(220.0)
                .show_ui(ui, |ui| {
                    if matching_vars.is_empty() {
                        ui.label(RichText::new("No variables match").small().weak());
                    }
                    for (name, label, units) in &matching_vars {
                        ui.selectable_value(
                            &mut current,
                            name.clone(),
                            format!("{label} ({units})"),
                        );
                    }
                });
            ui.label(
                RichText::new(format!("{} of {} vars", matching_vars.len(), surface_total))
                    .small()
                    .weak(),
            );
            if !current.is_empty() && Some(&current) != previous.as_ref() {
                self.selected_var = Some(current.clone());
                self.field = None;
                self.texture = None;
                self.texture_dirty = false;
                self.clicked = None;
                self.domain_drag = None;
                self.domain_rotate = None;
                self.domain_selection = None;
                self.view = None;
                self.cmap = None;
                self.legend_texture = None;
                self.basemap_pending = None;
                event = Some(FieldViewerEvent::VarSelected(current));
            }

            if let Some(field) = &self.field {
                let range = match field.range {
                    Some((lo, hi)) => format!("{lo:.2} .. {hi:.2} {}", field.units),
                    None => "all values missing".to_string(),
                };
                ui.label(RichText::new(range).small().weak());
            }
            if self.view.is_some() {
                if ui
                    .button("Reset zoom")
                    .on_hover_text("return the raw data preview to the full grid")
                    .clicked()
                {
                    self.view = None;
                }
                ui.label(RichText::new("zoomed").small().weak());
            }
        });
        match self.field.as_ref().and_then(|field| field.style.as_ref()) {
            Some(style) => {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(&style.title).small().strong());
                    ui.label(
                        RichText::new(format!("production colortable · {}", style.display_units))
                            .small()
                            .weak(),
                    );
                });
            }
            None => {
                ui.label(
                    RichText::new(
                        "DATA VIEWER — generic ramp (no plot counterpart), linear min..max",
                    )
                    .small()
                    .weak(),
                );
            }
        }
        self.draw_display_controls(ui);
        ui.separator();

        match &self.state {
            LoadState::Idle if self.hour.is_none() => {
                ui.add_space(12.0);
                ui.label(RichText::new("Pick a run hour on the left.").weak());
                return event;
            }
            LoadState::Idle => {
                ui.add_space(12.0);
                ui.label(RichText::new("Pick a 2D variable above.").weak());
                return event;
            }
            LoadState::Loading(var) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("loading {var}…"));
                });
                return event;
            }
            LoadState::Error(message) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
                return event;
            }
            LoadState::Ready => {}
        }
        let Some(field) = &self.field else {
            return event;
        };

        // Display flip is DERIVED from the grid lat axis, never assumed:
        // south-to-north storage (row 0 south) flips so the image is always
        // north-at-top; north-to-south storage renders rows as stored.
        let flip_y = !field.lat_descending;

        // (Re-)upload the textures only when the loaded field changed.
        if self.texture_dirty {
            profile_scope!("field_texture_build");
            let texture_started = std::time::Instant::now();
            let image = match &field.style {
                Some(style) => {
                    // Production path: the exact colormap the PNG rasterizer
                    // builds for this product, mapped per value. Values were
                    // already converted to display units by the worker.
                    let cmap = build_colormap(&style.scale, style.colormap_options);
                    let image = field_to_production_color_image(
                        &field.values,
                        field.nx,
                        field.ny,
                        &cmap,
                        flip_y,
                    );
                    self.legend_texture = Some(ui.ctx().load_texture(
                        "rw-ui-field-legend",
                        legend_bar_image(&cmap, style.legend_mode),
                        TextureOptions::LINEAR,
                    ));
                    self.cmap = Some(cmap);
                    image
                }
                None => {
                    self.cmap = None;
                    self.legend_texture = None;
                    let (vmin, vmax) = field.range.unwrap_or((0.0, 0.0));
                    field_to_color_image(
                        &field.values,
                        field.nx,
                        field.ny,
                        vmin,
                        vmax,
                        &self.colormap,
                        flip_y,
                    )
                }
            };
            self.texture = Some(ui.ctx().load_texture(
                "rw-ui-field",
                image,
                self.sampling_mode.texture_options(),
            ));
            self.last_texture_ms = Some(texture_started.elapsed().as_secs_f32() * 1000.0);
            self.texture_dirty = false;
        }
        let Some(texture) = &self.texture else {
            return event;
        };

        // Fit the grid into the remaining space, preserving aspect; mapped
        // fields reserve a strip on the right for the production legend.
        let view = self
            .view
            .unwrap_or_else(|| GridViewport::full(field.nx, field.ny));
        let legend_width = if self.cmap.is_some() {
            LEGEND_WIDTH
        } else {
            0.0
        };
        let mut avail = ui.available_size();
        avail.x = (avail.x - legend_width).max(1.0);
        let view_width = view.width_cells() as f32;
        let view_height = view.height_cells() as f32;
        let scale = (avail.x / view_width).min(avail.y / view_height).max(0.01);
        let size = Vec2::new(view_width * scale, view_height * scale);
        let response = ui.add(
            Image::new(texture)
                .uv(view.texture_uv_rect(field.nx, field.ny, flip_y))
                .fit_to_exact_size(size)
                .sense(Sense::click_and_drag()),
        );
        let rect = response.rect;

        let basemap_request = field.grid.as_ref().and_then(|grid| {
            raw_basemap_cache_key(grid, self.basemap_mode).map(|key| (Arc::clone(grid), key))
        });
        if let Some((grid, key)) = basemap_request.as_ref() {
            let cache_ready = self
                .basemap_cache
                .as_ref()
                .is_some_and(|cache| cache.key == *key);
            let pending_ready = self
                .basemap_pending
                .as_ref()
                .is_some_and(|pending| pending.key == *key);
            if !cache_ready && !pending_ready {
                self.basemap_pending = spawn_raw_basemap_build(Arc::clone(grid), key.clone());
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
        }
        if let (Some(cache), Some((_, key))) =
            (self.basemap_cache.as_ref(), basemap_request.as_ref())
        {
            if cache.key == *key {
                draw_raw_basemap(
                    ui,
                    rect,
                    field.nx,
                    field.ny,
                    flip_y,
                    view,
                    cache,
                    self.basemap_tone,
                    self.basemap_opacity,
                    self.basemap_width_scale,
                );
            }
        }

        // Pointer position -> fractional grid coordinates (texel centers at
        // integer coords). MUST invert the exact display transform above:
        // same `flip_y`, or clicks/hovers would sample a north/south-mirrored
        // location.
        let to_grid = |pos: egui::Pos2| -> (f64, f64) {
            let u = ((pos.x - rect.left()) / rect.width()) as f64;
            let v = ((pos.y - rect.top()) / rect.height()) as f64;
            view.image_uv_to_grid(u, v, field.nx, field.ny, flip_y)
        };

        let shift_down = ui.input(|input| input.modifiers.shift);
        let alt_down = ui.input(|input| input.modifiers.alt);
        let ctrl_down = ui.input(|input| input.modifiers.ctrl || input.modifiers.mac_cmd);

        if response.hovered() {
            let scroll_delta = ui.input(|input| input.smooth_scroll_delta().y);
            let zoom_delta = ui.input(|input| input.zoom_delta());
            if scroll_delta.abs() > 0.01 || (zoom_delta - 1.0).abs() > 0.001 {
                let factor = if (zoom_delta - 1.0).abs() > 0.001 {
                    f64::from(zoom_delta)
                } else {
                    f64::from(scroll_delta * 0.004).exp()
                };
                if let Some(pos) = response.hover_pos() {
                    let (fx, fy) = to_grid(pos);
                    self.view = viewport_option(
                        view.zoomed_at(fx, fy, factor, field.nx, field.ny),
                        field.nx,
                        field.ny,
                    );
                    ui.input_mut(|input| input.smooth_scroll_delta = Vec2::ZERO);
                    ui.ctx().request_repaint();
                }
            }
        }

        if response.dragged_by(PointerButton::Primary)
            && !shift_down
            && !alt_down
            && self.domain_drag.is_none()
            && self.domain_rotate.is_none()
        {
            let delta = ui.input(|input| input.pointer.delta());
            if delta != Vec2::ZERO {
                let dx =
                    -f64::from(delta.x) / f64::from(rect.width().max(1.0)) * view.width_cells();
                let dy = if flip_y {
                    f64::from(delta.y) / f64::from(rect.height().max(1.0)) * view.height_cells()
                } else {
                    -f64::from(delta.y) / f64::from(rect.height().max(1.0)) * view.height_cells()
                };
                self.view = viewport_option(
                    view.translated(dx, dy, field.nx, field.ny),
                    field.nx,
                    field.ny,
                );
                ui.ctx().request_repaint();
            }
        }

        if response.double_clicked_by(PointerButton::Primary) && !shift_down && !alt_down {
            self.view = None;
            ui.ctx().request_repaint();
        }

        if response.clicked_by(PointerButton::Primary) && alt_down && !ctrl_down {
            if let Some(pos) = response.interact_pointer_pos() {
                let (fx, fy) = to_grid(pos);
                self.clicked = Some((fx, fy));
                event = Some(FieldViewerEvent::PointClicked { fx, fy });
            }
        }

        // Ctrl+Alt held: smooth "scrubbing" soundings. Every time the pointer
        // moves into a new grid cell over the field, fire a fresh sounding.
        // The store read is sub-millisecond warm and the worker coalesces to
        // the latest request; the sounding panel keeps its previous scene
        // while the next one computes, so scrubbing stays fluid and flicker
        // free. Gating on the integer grid cell caps regeneration to at most
        // one sounding per column — all the data resolution supports.
        if ctrl_down && alt_down && response.hovered() {
            if let Some(pos) = response.hover_pos() {
                let (fx, fy) = to_grid(pos);
                if fx >= 0.0
                    && fy >= 0.0
                    && fx.round() < field.nx as f64
                    && fy.round() < field.ny as f64
                {
                    let cell = (fx.round() as usize, fy.round() as usize);
                    if self.live_sounding_cell != Some(cell) {
                        self.live_sounding_cell = Some(cell);
                        self.clicked = Some((fx, fy));
                        event = Some(FieldViewerEvent::PointClicked { fx, fy });
                    }
                }
            }
            // Keep frames flowing while the modifier is held so pointer motion
            // is sampled smoothly.
            ui.ctx().request_repaint();
        } else {
            self.live_sounding_cell = None;
        }

        let domain_button = if shift_down && response.drag_started_by(PointerButton::Primary) {
            Some(PointerButton::Primary)
        } else if shift_down && response.drag_started_by(PointerButton::Secondary) {
            Some(PointerButton::Secondary)
        } else {
            None
        };
        if let Some(button) = domain_button {
            if let Some(pos) = response.interact_pointer_pos() {
                let pos = clamp_pos_to_rect(pos, rect);
                let mut started_rotation = false;
                if let Some(selection) = self.domain_selection {
                    if let Some((center, corners)) =
                        domain_selection_geometry(rect, field.nx, field.ny, flip_y, view, selection)
                    {
                        if pointer_near_selection_corner(pos, corners) {
                            self.domain_rotate = Some(DomainRotate {
                                center,
                                button,
                                start_pointer_angle_deg: pointer_angle_deg(center, pos),
                                start_rotation_deg: selection.rotation_deg,
                                current_rotation_deg: selection.rotation_deg,
                            });
                            self.domain_drag = None;
                            started_rotation = true;
                        }
                    }
                }
                if !started_rotation {
                    self.domain_drag = Some(DomainDrag {
                        start: pos,
                        current: pos,
                        button,
                    });
                    self.domain_rotate = None;
                }
            }
        }
        let mut finished_rotation = None;
        if let Some(rotate) = self.domain_rotate.as_mut() {
            if response.dragged_by(rotate.button) {
                if let Some(pos) = response.interact_pointer_pos() {
                    let rotation_deg = normalize_rotation(
                        rotate.start_rotation_deg + pointer_angle_deg(rotate.center, pos)
                            - rotate.start_pointer_angle_deg,
                    );
                    rotate.current_rotation_deg = rotation_deg;
                    if let Some(selection) = self.domain_selection.as_mut() {
                        selection.rotation_deg = rotation_deg;
                    }
                }
            }
            if response.drag_stopped_by(rotate.button) {
                finished_rotation = Some(rotate.current_rotation_deg);
            }
        }
        if let Some(rotation_deg) = finished_rotation {
            self.domain_rotate = None;
            if let Some(selection) = self.domain_selection.as_mut() {
                selection.rotation_deg = rotation_deg;
            }
            event = Some(FieldViewerEvent::DomainRotationChanged { rotation_deg });
        }
        if let Some(drag) = self.domain_drag.as_mut() {
            if response.dragged_by(drag.button) {
                if let Some(pos) = response.interact_pointer_pos() {
                    drag.current = clamp_pos_to_rect(pos, rect);
                }
            }
            if response.drag_stopped_by(drag.button) {
                let start = drag.start;
                let current = drag.current;
                self.domain_drag = None;
                if screen_drag_is_large_enough(start, current) {
                    let (fx0, fy0) = to_grid(start);
                    let (fx1, fy1) = to_grid(current);
                    let selection = DomainSelection {
                        fx0,
                        fy0,
                        fx1,
                        fy1,
                        rotation_deg: 0.0,
                    };
                    if let Some(grid) = field.grid.as_ref() {
                        if let Some(bounds) = domain_bounds_from_grid_selection(
                            &grid.lat, &grid.lon, grid.nx, grid.ny, selection,
                        ) {
                            let domain = CustomDomain::generated(bounds)
                                .with_rotation(selection.rotation_deg);
                            self.domain_selection = Some(selection);
                            event = Some(FieldViewerEvent::DomainSelected(domain));
                        }
                    }
                }
            }
        }

        // Marker on the last clicked point (forward display transform).
        if let Some((fx, fy)) = self.clicked {
            let (u, v) = view.grid_to_image_uv(fx, fy, field.nx, field.ny, flip_y);
            let px = rect.left() + u as f32 * rect.width();
            let py = rect.top() + v as f32 * rect.height();
            let painter = ui.painter_at(rect);
            painter.circle_stroke(pos2(px, py), 5.0, Stroke::new(2.0, Color32::WHITE));
            painter.circle_stroke(pos2(px, py), 6.5, Stroke::new(1.0, Color32::BLACK));
        }
        if let Some(selection) = self.domain_selection {
            draw_domain_selection(ui, rect, field.nx, field.ny, flip_y, view, selection);
        }
        if let Some(drag) = self.domain_drag {
            draw_screen_domain_drag(ui, rect, drag);
        }
        ui.painter_at(rect.expand(1.0)).rect_stroke(
            Rect::from_min_max(rect.min, rect.max),
            0.0,
            Stroke::new(1.0, ui.visuals().weak_text_color()),
            StrokeKind::Outside,
        );

        // Production legend: swatch colors from the colormap's legend
        // levels, tick VALUES from the production `pick_ticks`, labels from
        // `format_tick` — the same data the PNG colorbar renders from.
        if let Some(style) = &field.style {
            self.draw_legend(ui, rect, style);
        }

        // Hover readout: grid point + lat/lon + value. Lat/lon come FROM THE
        // STORED ARRAYS at the mapped (ix, iy) — the same indexing the
        // sounding uses — so the readout verifies the click mapping in-UI.
        if let Some(pos) = response.hover_pos() {
            let (fx, fy) = to_grid(pos);
            let ix = fx.round() as usize;
            let iy = fy.round() as usize;
            let value = field.values[iy * field.nx + ix];
            let place = match &field.grid {
                Some(grid) => {
                    let idx = iy * grid.nx + ix;
                    format!("  {:.3}°, {:.3}°", grid.lat[idx], grid.lon[idx])
                }
                None => String::new(),
            };
            let mut text = if value.is_nan() {
                format!("({ix}, {iy}){place}  missing")
            } else {
                format!("({ix}, {iy}){place}  {value:.2} {}", field.units)
            };
            if ctrl_down && alt_down {
                text.push_str("   ⟳ live sounding");
            }
            response.on_hover_text_at_pointer(text);
        }

        event
    }

    fn poll_raw_basemap_build(&mut self, ui: &Ui) {
        let Some(pending) = self.basemap_pending.take() else {
            return;
        };
        match pending.rx.try_recv() {
            Ok(cache) => {
                if cache.key == pending.key {
                    self.basemap_cache = Some(cache);
                }
                ui.ctx().request_repaint();
            }
            Err(TryRecvError::Empty) => {
                self.basemap_pending = Some(pending);
                ui.ctx().request_repaint_after(Duration::from_millis(100));
            }
            Err(TryRecvError::Disconnected) => {
                ui.ctx().request_repaint();
            }
        }
    }

    fn draw_display_controls(&mut self, ui: &mut Ui) {
        let previous_sampling = self.sampling_mode;
        let previous_basemap = self.basemap_mode;

        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Display").small().strong());

            ComboBox::from_id_salt("rw-ui-field-sampling")
                .selected_text(self.sampling_mode.label())
                .width(112.0)
                .show_ui(ui, |ui| {
                    ui.selectable_value(
                        &mut self.sampling_mode,
                        FieldSamplingMode::Smooth,
                        FieldSamplingMode::Smooth.label(),
                    );
                    ui.selectable_value(
                        &mut self.sampling_mode,
                        FieldSamplingMode::PixelExact,
                        FieldSamplingMode::PixelExact.label(),
                    );
                });

            ComboBox::from_id_salt("rw-ui-field-basemap")
                .selected_text(self.basemap_mode.label())
                .width(104.0)
                .show_ui(ui, |ui| {
                    for mode in RawBasemapMode::ALL {
                        ui.selectable_value(&mut self.basemap_mode, mode, mode.label());
                    }
                });

            ComboBox::from_id_salt("rw-ui-field-basemap-tone")
                .selected_text(self.basemap_tone.label())
                .width(90.0)
                .show_ui(ui, |ui| {
                    for tone in RawBasemapTone::ALL {
                        ui.selectable_value(&mut self.basemap_tone, tone, tone.label());
                    }
                });

            ui.add(
                Slider::new(&mut self.basemap_opacity, 0.05..=1.0)
                    .text("Opacity")
                    .fixed_decimals(2),
            );
            ui.add(
                Slider::new(&mut self.basemap_width_scale, 0.5..=2.5)
                    .text("Line")
                    .fixed_decimals(2),
            );

            if let Some(pending) = &self.basemap_pending {
                ui.label(
                    RichText::new(format!("basemap {} building", pending.key.mode.label()))
                        .small()
                        .weak(),
                );
            } else if let Some(cache) = &self.basemap_cache {
                ui.label(
                    RichText::new(format!(
                        "basemap {:.0} ms · {} pts",
                        cache.build_ms, cache.point_count
                    ))
                    .small()
                    .weak(),
                );
            }
        });

        self.basemap_opacity = self.basemap_opacity.clamp(0.05, 1.0);
        self.basemap_width_scale = self.basemap_width_scale.clamp(0.5, 2.5);
        if self.sampling_mode != previous_sampling {
            self.texture = None;
            self.texture_dirty = self.field.is_some();
        }
        if self.basemap_mode != previous_basemap {
            self.basemap_cache = None;
            self.basemap_pending = None;
        }
    }

    /// Draw the vertical production colorbar to the right of `image_rect`:
    /// bar pixels sampled exactly like `draw_vertical_colorbar`, tick marks
    /// placed linear-by-value (`legend_tick_rel`), labels via `format_tick`,
    /// topped with the display units.
    fn draw_legend(
        &self,
        ui: &Ui,
        image_rect: Rect,
        style: &rustwx_products::viewer::StoreVariableStyle,
    ) {
        let (Some(cmap), Some(texture)) = (&self.cmap, &self.legend_texture) else {
            return;
        };
        let painter = ui.painter();
        let gap = 10.0;
        let bar_w = 16.0;
        let top_pad = 16.0; // room for the units label
        let bar_rect = Rect::from_min_max(
            pos2(image_rect.right() + gap, image_rect.top() + top_pad),
            pos2(image_rect.right() + gap + bar_w, image_rect.bottom()),
        );
        if bar_rect.height() < 24.0 {
            return;
        }

        painter.image(
            texture.id(),
            bar_rect,
            Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        painter.rect_stroke(
            bar_rect,
            0.0,
            Stroke::new(1.0, ui.visuals().weak_text_color()),
            StrokeKind::Outside,
        );

        let text_color = ui.visuals().text_color();
        let font = FontId::proportional(10.0);
        painter.text(
            pos2(bar_rect.left(), bar_rect.top() - 3.0),
            Align2::LEFT_BOTTOM,
            &style.display_units,
            font.clone(),
            text_color,
        );

        // Tick values/positions exactly as the production colorbar labels
        // them; skip overlapping labels like the renderer does.
        let mut last_label_top = f32::INFINITY;
        let line_h = 11.0;
        for tick in colorbar_ticks(cmap, style.cbar_tick_step) {
            let Some(rel) = legend_tick_rel(cmap, tick) else {
                continue;
            };
            let y = bar_rect.bottom() - rel as f32 * bar_rect.height();
            painter.line_segment(
                [pos2(bar_rect.right(), y), pos2(bar_rect.right() + 3.0, y)],
                Stroke::new(1.0, text_color),
            );
            if y + line_h <= last_label_top || last_label_top.is_infinite() {
                painter.text(
                    pos2(bar_rect.right() + 5.0, y),
                    Align2::LEFT_CENTER,
                    format_tick(tick),
                    font.clone(),
                    text_color,
                );
                last_label_top = y - line_h;
            }
        }
    }
}

fn display_variable_name(name: &str) -> String {
    if let Some(spec) = parse_iso_slug(name) {
        return spec.label();
    }
    if let Some(slug) = name.strip_prefix("approx_") {
        let label = match slug {
            "sbcape" => "SBCAPE",
            "sbcin" => "SBCIN",
            "mlcape" => "MLCAPE",
            "mlcin" => "MLCIN",
            "mucape" => "MUCAPE",
            "mucin" => "MUCIN",
            "lcl" => "LCL height",
            "lfc" => "LFC height",
            "el" => "EL height",
            "srh_0_1km" => "0-1 km SRH",
            "srh_0_3km" => "0-3 km SRH",
            "bulk_shear_0_1km" => "0-1 km bulk shear",
            "bulk_shear_0_6km" => "0-6 km bulk shear",
            "stp" => "STP",
            "scp" => "SCP",
            "ehi" => "EHI",
            _ => slug,
        };
        return format!("Approximate {label}");
    }
    name.to_string()
}

/// One production colorbar sample per row, top = the highest legend level —
/// the same per-pixel sampling `draw_vertical_colorbar` paints.
fn legend_bar_image(cmap: &LeveledColormap, mode: rustwx_render::LegendMode) -> ColorImage {
    let mut pixels = Vec::with_capacity(LEGEND_BAR_RESOLUTION);
    for row in 0..LEGEND_BAR_RESOLUTION {
        let rel = 1.0 - (row as f64 + 0.5) / LEGEND_BAR_RESOLUTION as f64;
        let rgba = legend_color_at_rel(cmap, mode, rel);
        pixels.push(Color32::from_rgba_unmultiplied(
            rgba.r, rgba.g, rgba.b, rgba.a,
        ));
    }
    ColorImage::new([1, LEGEND_BAR_RESOLUTION], pixels)
}

fn raw_basemap_cache_key(grid: &GridFile, mode: RawBasemapMode) -> Option<RawBasemapCacheKey> {
    if mode == RawBasemapMode::Off
        || grid.nx == 0
        || grid.ny == 0
        || grid.lat.len() != grid.nx * grid.ny
        || grid.lon.len() != grid.nx * grid.ny
    {
        return None;
    }
    Some(RawBasemapCacheKey {
        grid_hash: grid.hash.clone(),
        mode,
    })
}

fn spawn_raw_basemap_build(
    grid: Arc<GridFile>,
    key: RawBasemapCacheKey,
) -> Option<RawBasemapBuild> {
    let (tx, rx) = channel();
    let worker_key = key.clone();
    std::thread::Builder::new()
        .name("rw-ui-raw-basemap".to_string())
        .spawn(move || {
            let cache = build_raw_basemap_cache(&grid, worker_key);
            let _ = tx.send(cache);
        })
        .ok()?;
    Some(RawBasemapBuild { key, rx })
}

fn build_raw_basemap_cache(grid: &GridFile, key: RawBasemapCacheKey) -> RawBasemapCache {
    let started = std::time::Instant::now();
    let Some(detail) = key.mode.detail() else {
        return RawBasemapCache {
            key,
            layers: Vec::new(),
            build_ms: 0.0,
            point_count: 0,
        };
    };
    let Some(bounds) = GridGeoBounds::from_grid(grid) else {
        return RawBasemapCache {
            key,
            layers: Vec::new(),
            build_ms: started.elapsed().as_secs_f32() * 1000.0,
            point_count: 0,
        };
    };

    let locator = GridLocator::build(grid);
    let mut layers = Vec::new();
    let mut point_count = 0usize;
    for layer in load_styled_basemap_features_for_detail(BasemapStyle::Filled, detail) {
        if !key.mode.includes_role(layer.role) {
            continue;
        }
        let (lines, points) =
            locate_raw_basemap_lines(&layer.lines, &locator, bounds, key.mode, layer.role, grid);
        point_count += points;
        if !lines.is_empty() {
            layers.push(RawBasemapLayer {
                lines,
                color: layer.color,
                width: layer.width,
                role: layer.role,
            });
        }
    }

    RawBasemapCache {
        key,
        layers,
        build_ms: started.elapsed().as_secs_f32() * 1000.0,
        point_count,
    }
}

fn locate_raw_basemap_lines(
    lines: &[Vec<(f64, f64)>],
    locator: &GridLocator,
    bounds: GridGeoBounds,
    mode: RawBasemapMode,
    role: LineworkRole,
    grid: &GridFile,
) -> (Vec<Vec<(f64, f64)>>, usize) {
    let mut out = Vec::new();
    let mut current = Vec::new();
    let mut point_count = 0usize;
    let max_points = mode.max_located_points(role);
    let max_segment_deg = mode.max_segment_deg(role);
    let min_step2 = min_raw_basemap_step_cells2(mode, role);
    let split_distance = (grid.nx.max(grid.ny) as f64 * 0.35).max(96.0);

    'lines: for line in lines {
        finish_raw_basemap_line(&mut out, &mut current);
        if line.len() < 2 {
            continue;
        }

        for segment in line.windows(2) {
            let (lon0, lat0) = segment[0];
            let (lon1, lat1) = segment[1];
            if ![lon0, lat0, lon1, lat1]
                .iter()
                .all(|value| value.is_finite())
            {
                finish_raw_basemap_line(&mut out, &mut current);
                continue;
            }

            let dlon = shortest_lon_delta(lon0, lon1);
            let dlat = lat1 - lat0;
            let steps =
                ((dlat.abs().max(dlon.abs()) / max_segment_deg).ceil() as usize).clamp(1, 96);
            for step in 0..=steps {
                if step == 0 && !current.is_empty() {
                    continue;
                }
                let t = step as f64 / steps as f64;
                let lat = lat0 + dlat * t;
                let lon = normalize_lon(lon0 + dlon * t);
                if !bounds.contains(lat, lon, RAW_BASEMAP_GEO_PAD_DEG) {
                    finish_raw_basemap_line(&mut out, &mut current);
                    continue;
                }
                let Some((fx, fy)) = locator.locate(lat, lon) else {
                    finish_raw_basemap_line(&mut out, &mut current);
                    continue;
                };
                if current.last().is_some_and(|(last_x, last_y)| {
                    (fx - last_x).abs().max((fy - last_y).abs()) > split_distance
                }) {
                    finish_raw_basemap_line(&mut out, &mut current);
                }
                if current.last().is_none_or(|(last_x, last_y)| {
                    let dx = fx - last_x;
                    let dy = fy - last_y;
                    dx * dx + dy * dy >= min_step2
                }) {
                    current.push((fx, fy));
                    point_count += 1;
                }
                if point_count >= max_points {
                    finish_raw_basemap_line(&mut out, &mut current);
                    break 'lines;
                }
            }
        }
    }
    finish_raw_basemap_line(&mut out, &mut current);
    (out, point_count)
}

fn finish_raw_basemap_line(out: &mut Vec<Vec<(f64, f64)>>, current: &mut Vec<(f64, f64)>) {
    if current.len() >= 2 {
        out.push(std::mem::take(current));
    } else {
        current.clear();
    }
}

fn min_raw_basemap_step_cells2(mode: RawBasemapMode, role: LineworkRole) -> f64 {
    let step: f64 = match (mode, role) {
        (RawBasemapMode::Counties, LineworkRole::County) => 0.55,
        (RawBasemapMode::Regional | RawBasemapMode::Counties, _) => 0.30,
        (RawBasemapMode::Broad, _) => 0.22,
        (RawBasemapMode::Global, _) => 0.18,
        (RawBasemapMode::Off, _) => 1.0,
    };
    step * step
}

fn draw_raw_basemap(
    ui: &Ui,
    image_rect: Rect,
    _nx: usize,
    _ny: usize,
    flip_y: bool,
    view: GridViewport,
    cache: &RawBasemapCache,
    tone: RawBasemapTone,
    opacity: f32,
    width_scale: f32,
) {
    if cache.layers.is_empty() || opacity <= 0.0 || width_scale <= 0.0 {
        return;
    }
    let painter = ui.painter_at(image_rect);
    let cull_rect = image_rect.expand(24.0);
    for layer in &cache.layers {
        let color = raw_basemap_color(layer.color, layer.role, tone, opacity);
        if color.a() == 0 {
            continue;
        }
        let stroke = Stroke::new(
            raw_basemap_width(layer.width, layer.role, width_scale),
            color,
        );
        for line in &layer.lines {
            for segment in line.windows(2) {
                let p0 = raw_grid_point_to_screen(segment[0], image_rect, view, flip_y);
                let p1 = raw_grid_point_to_screen(segment[1], image_rect, view, flip_y);
                if segment_might_intersect_rect(p0, p1, cull_rect) {
                    painter.line_segment([p0, p1], stroke);
                }
            }
        }
    }
}

fn raw_grid_point_to_screen(
    point: (f64, f64),
    image_rect: Rect,
    view: GridViewport,
    flip_y: bool,
) -> Pos2 {
    let (u, v) = view.grid_to_image_uv_unclamped(point.0, point.1, flip_y);
    pos2(
        image_rect.left() + u as f32 * image_rect.width(),
        image_rect.top() + v as f32 * image_rect.height(),
    )
}

fn segment_might_intersect_rect(a: Pos2, b: Pos2, rect: Rect) -> bool {
    a.x.min(b.x) <= rect.right()
        && a.x.max(b.x) >= rect.left()
        && a.y.min(b.y) <= rect.bottom()
        && a.y.max(b.y) >= rect.top()
}

fn raw_basemap_color(
    color: Rgba,
    role: LineworkRole,
    tone: RawBasemapTone,
    opacity: f32,
) -> Color32 {
    let county_alpha = if matches!(role, LineworkRole::County) {
        0.78
    } else {
        1.0
    };
    let alpha = (color.a as f32 * opacity * tone.alpha_multiplier() * county_alpha)
        .round()
        .clamp(0.0, 255.0) as u8;
    let rgb_mul = tone.rgb_multiplier();
    Color32::from_rgba_unmultiplied(
        (color.r as f32 * rgb_mul).round().clamp(0.0, 255.0) as u8,
        (color.g as f32 * rgb_mul).round().clamp(0.0, 255.0) as u8,
        (color.b as f32 * rgb_mul).round().clamp(0.0, 255.0) as u8,
        alpha,
    )
}

fn raw_basemap_width(width: u32, role: LineworkRole, scale: f32) -> f32 {
    let role_scale = match role {
        LineworkRole::Coast | LineworkRole::State => 1.1,
        LineworkRole::International | LineworkRole::Lake => 0.95,
        LineworkRole::County => 0.7,
        LineworkRole::Generic => 1.0,
    };
    ((width.max(1) as f32) * role_scale * scale).clamp(0.35, 5.0)
}

#[derive(Debug, Clone, Copy)]
struct GridGeoBounds {
    south: f64,
    north: f64,
    west: f64,
    east: f64,
}

impl GridGeoBounds {
    fn from_grid(grid: &GridFile) -> Option<Self> {
        let mut south = f64::INFINITY;
        let mut north = f64::NEG_INFINITY;
        let mut longitudes = Vec::new();
        let stride = ((grid.nx.max(grid.ny) as f64 / 240.0).ceil() as usize).max(1);
        for y in sample_axis_indices(grid.ny, stride) {
            for x in sample_axis_indices(grid.nx, stride) {
                let idx = y * grid.nx + x;
                let lat = f64::from(grid.lat[idx]);
                let lon = normalize_lon(f64::from(grid.lon[idx]));
                if lat.is_finite() && lon.is_finite() {
                    south = south.min(lat);
                    north = north.max(lat);
                    longitudes.push(lon);
                }
            }
        }
        if !south.is_finite() || !north.is_finite() {
            return None;
        }
        let (west, east) = shortest_longitude_interval(&longitudes)?;
        Some(Self {
            south,
            north,
            west,
            east,
        })
    }

    fn contains(self, lat: f64, lon: f64, pad_deg: f64) -> bool {
        lat >= self.south - pad_deg
            && lat <= self.north + pad_deg
            && longitude_in_interval(lon, self.west, self.east, pad_deg)
    }
}

fn sample_axis_indices(n: usize, stride: usize) -> Vec<usize> {
    let mut indices: Vec<usize> = (0..n).step_by(stride.max(1)).collect();
    if n > 0 && indices.last().copied() != Some(n - 1) {
        indices.push(n - 1);
    }
    indices
}

fn longitude_in_interval(lon: f64, west: f64, east: f64, pad_deg: f64) -> bool {
    let span = longitude_span(west, east);
    if span + pad_deg * 2.0 >= 359.0 {
        return true;
    }
    let lon = normalize_lon(lon);
    let west = normalize_lon(west - pad_deg);
    let east = normalize_lon(east + pad_deg);
    if west <= east {
        lon >= west && lon <= east
    } else {
        lon >= west || lon <= east
    }
}

fn shortest_lon_delta(from_lon: f64, to_lon: f64) -> f64 {
    normalize_lon(to_lon - from_lon)
}

/// Normalized image coords (`u` rightward, `v` DOWNWARD from the image top,
/// both in `[0, 1]`) -> fractional grid coords, with texel centers at
/// integer coords. `flip_y` must be the SAME flag the texture build used —
/// this is the inverse of the display transform, so clicks and hovers sample
/// the grid point actually under the pointer.
#[cfg(test)]
fn image_uv_to_grid(u: f64, v: f64, nx: usize, ny: usize, flip_y: bool) -> (f64, f64) {
    let row = if flip_y { 1.0 - v } else { v };
    let fx = (u * nx as f64 - 0.5).clamp(0.0, (nx - 1) as f64);
    let fy = (row * ny as f64 - 0.5).clamp(0.0, (ny - 1) as f64);
    (fx, fy)
}

/// Fractional grid coords -> normalized image coords; exact inverse of
/// [`image_uv_to_grid`] away from the clamped border (marker overlay).
#[cfg(test)]
fn grid_to_image_uv(fx: f64, fy: f64, nx: usize, ny: usize, flip_y: bool) -> (f64, f64) {
    let u = (fx + 0.5) / nx as f64;
    let row = (fy + 0.5) / ny as f64;
    (u, if flip_y { 1.0 - row } else { row })
}

fn clamp_pos_to_rect(pos: Pos2, rect: Rect) -> Pos2 {
    pos2(
        pos.x.clamp(rect.left(), rect.right()),
        pos.y.clamp(rect.top(), rect.bottom()),
    )
}

fn viewport_option(view: GridViewport, nx: usize, ny: usize) -> Option<GridViewport> {
    let view = view.clamped(nx, ny);
    (!view.is_full(nx, ny)).then_some(view)
}

fn screen_drag_is_large_enough(a: Pos2, b: Pos2) -> bool {
    (a.x - b.x).abs() >= 8.0 && (a.y - b.y).abs() >= 8.0
}

fn draw_screen_domain_drag(ui: &Ui, image_rect: Rect, drag: DomainDrag) {
    let rect = Rect::from_two_pos(drag.start, drag.current).intersect(image_rect);
    draw_domain_rect(ui, rect);
}

fn draw_domain_selection(
    ui: &Ui,
    image_rect: Rect,
    nx: usize,
    ny: usize,
    flip_y: bool,
    view: GridViewport,
    selection: DomainSelection,
) {
    let Some((_center, corners)) =
        domain_selection_geometry(image_rect, nx, ny, flip_y, view, selection)
    else {
        return;
    };
    draw_domain_polygon(ui, image_rect, corners);
}

fn domain_selection_geometry(
    image_rect: Rect,
    nx: usize,
    ny: usize,
    flip_y: bool,
    view: GridViewport,
    selection: DomainSelection,
) -> Option<(Pos2, [Pos2; 4])> {
    let (u0, v0) = view.grid_to_image_uv(selection.fx0, selection.fy0, nx, ny, flip_y);
    let (u1, v1) = view.grid_to_image_uv(selection.fx1, selection.fy1, nx, ny, flip_y);
    let p0 = pos2(
        image_rect.left() + u0 as f32 * image_rect.width(),
        image_rect.top() + v0 as f32 * image_rect.height(),
    );
    let p1 = pos2(
        image_rect.left() + u1 as f32 * image_rect.width(),
        image_rect.top() + v1 as f32 * image_rect.height(),
    );
    let rect = Rect::from_two_pos(p0, p1).intersect(image_rect);
    if rect.width() < 2.0 || rect.height() < 2.0 {
        return None;
    }
    let center = rect.center();
    let corners = [
        rotate_pos(rect.left_top(), center, selection.rotation_deg),
        rotate_pos(rect.right_top(), center, selection.rotation_deg),
        rotate_pos(rect.right_bottom(), center, selection.rotation_deg),
        rotate_pos(rect.left_bottom(), center, selection.rotation_deg),
    ];
    Some((center, corners))
}

fn draw_domain_polygon(ui: &Ui, image_rect: Rect, corners: [Pos2; 4]) {
    let painter = ui.painter_at(image_rect.expand(24.0));
    painter.add(egui::Shape::convex_polygon(
        corners.to_vec(),
        Color32::from_rgba_unmultiplied(0, 160, 255, 20),
        Stroke::new(0.0, Color32::TRANSPARENT),
    ));
    let outline = Stroke::new(2.0, Color32::from_rgb(0, 185, 255));
    let shadow = Stroke::new(1.0, Color32::BLACK);
    for index in 0..4 {
        let a = corners[index];
        let b = corners[(index + 1) % 4];
        painter.line_segment([a, b], Stroke::new(outline.width + 2.0, Color32::BLACK));
        painter.line_segment([a, b], outline);
    }
    for corner in corners {
        painter.circle_filled(corner, 4.5, Color32::from_rgb(0, 185, 255));
        painter.circle_stroke(corner, 5.5, shadow);
    }
}

fn pointer_near_selection_corner(pos: Pos2, corners: [Pos2; 4]) -> bool {
    corners.iter().any(|corner| corner.distance(pos) <= 18.0)
}

fn pointer_angle_deg(center: Pos2, pos: Pos2) -> f64 {
    f64::from(pos.y - center.y)
        .atan2(f64::from(pos.x - center.x))
        .to_degrees()
}

fn rotate_pos(pos: Pos2, center: Pos2, rotation_deg: f64) -> Pos2 {
    let radians = rotation_deg.to_radians();
    let sin = radians.sin() as f32;
    let cos = radians.cos() as f32;
    let dx = pos.x - center.x;
    let dy = pos.y - center.y;
    pos2(
        center.x + dx * cos - dy * sin,
        center.y + dx * sin + dy * cos,
    )
}

fn normalize_rotation(rotation_deg: f64) -> f64 {
    if !rotation_deg.is_finite() {
        return 0.0;
    }
    let normalized = ((rotation_deg + 180.0).rem_euclid(360.0)) - 180.0;
    if (normalized + 180.0).abs() < 1.0e-9 {
        180.0
    } else {
        normalized
    }
}

fn draw_domain_rect(ui: &Ui, rect: Rect) {
    if rect.width() < 2.0 || rect.height() < 2.0 {
        return;
    }
    let painter = ui.painter_at(rect.expand(2.0));
    painter.rect_filled(rect, 0.0, Color32::from_rgba_unmultiplied(0, 160, 255, 20));
    painter.rect_stroke(
        rect,
        0.0,
        Stroke::new(2.0, Color32::from_rgb(0, 185, 255)),
        StrokeKind::Outside,
    );
    painter.rect_stroke(
        rect.expand(1.0),
        0.0,
        Stroke::new(1.0, Color32::BLACK),
        StrokeKind::Outside,
    );
}

fn domain_bounds_from_grid_selection(
    lat: &[f32],
    lon: &[f32],
    nx: usize,
    ny: usize,
    selection: DomainSelection,
) -> Option<(f64, f64, f64, f64)> {
    if lat.len() != nx * ny || lon.len() != nx * ny || nx == 0 || ny == 0 {
        return None;
    }
    let x0 = selection.fx0.min(selection.fx1).floor().max(0.0) as usize;
    let x1 = selection.fx0.max(selection.fx1).ceil().min((nx - 1) as f64) as usize;
    let y0 = selection.fy0.min(selection.fy1).floor().max(0.0) as usize;
    let y1 = selection.fy0.max(selection.fy1).ceil().min((ny - 1) as f64) as usize;
    if x0 > x1 || y0 > y1 {
        return None;
    }

    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut longitudes = Vec::new();
    for y in y0..=y1 {
        for x in x0..=x1 {
            let idx = y * nx + x;
            let lat = f64::from(lat[idx]);
            let lon = normalize_lon(f64::from(lon[idx]));
            if !lat.is_finite() || !lon.is_finite() {
                continue;
            }
            south = south.min(lat);
            north = north.max(lat);
            longitudes.push(lon);
        }
    }
    let (west, east) = shortest_longitude_interval(&longitudes)?;
    if !south.is_finite() || !north.is_finite() {
        return None;
    }
    Some(expand_tiny_bounds((west, east, south, north)))
}

fn shortest_longitude_interval(longitudes: &[f64]) -> Option<(f64, f64)> {
    let mut values: Vec<f64> = longitudes
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .map(|value| normalize_lon(value).rem_euclid(360.0))
        .collect();
    if values.is_empty() {
        return None;
    }
    values.sort_by(|a, b| a.total_cmp(b));
    values.dedup_by(|a, b| (*a - *b).abs() < 1.0e-9);
    if values.len() == 1 {
        let lon = normalize_lon(values[0]);
        return Some((lon, lon));
    }

    let mut largest_gap = f64::NEG_INFINITY;
    let mut largest_gap_index = 0usize;
    for i in 0..values.len() {
        let a = values[i];
        let b = if i + 1 == values.len() {
            values[0] + 360.0
        } else {
            values[i + 1]
        };
        let gap = b - a;
        if gap > largest_gap {
            largest_gap = gap;
            largest_gap_index = i;
        }
    }

    let west = values[(largest_gap_index + 1) % values.len()];
    let east = values[largest_gap_index];
    Some((normalize_lon(west), normalize_lon(east)))
}

fn expand_tiny_bounds(bounds: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let (mut west, mut east, mut south, mut north) = bounds;
    if longitude_span(west, east) < 0.05 {
        let center = midpoint_longitude(west, east);
        west = normalize_lon(center - 0.025);
        east = normalize_lon(center + 0.025);
    }
    if north - south < 0.05 {
        let center = (north + south) * 0.5;
        south = (center - 0.025).clamp(-89.5, 89.5);
        north = (center + 0.025).clamp(-89.5, 89.5);
    }
    (west, east, south, north)
}

fn longitude_span(west: f64, east: f64) -> f64 {
    let west = normalize_lon(west);
    let east = normalize_lon(east);
    if west <= east {
        east - west
    } else {
        east + 360.0 - west
    }
}

fn midpoint_longitude(west: f64, east: f64) -> f64 {
    let west = normalize_lon(west);
    let mut east = normalize_lon(east);
    if east < west {
        east += 360.0;
    }
    normalize_lon((west + east) * 0.5)
}

fn normalize_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::style_overrides::{UserColorTable, UserUnitConvert};

    const NX: usize = 8;
    const NY: usize = 6;

    fn test_hour(hour: u16) -> HourKey {
        HourKey {
            model: "wrf".to_string(),
            run: "test".to_string(),
            hour,
        }
    }

    #[test]
    fn generated_field_is_namespaced_retained_and_never_loaded_from_store() {
        let hour = test_hour(0);
        let real = VarInfo {
            name: "temperature_2m".to_string(),
            units: "K".to_string(),
            kind: VarKind::Surface2D,
            levels_hpa: Vec::new(),
        };
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), vec![real.clone()]);
        panel.install_generated_field(
            FieldData {
                key: FieldKey {
                    hour: hour.clone(),
                    var: "temperature_2m".to_string(),
                },
                units: "K".to_string(),
                nx: NX,
                ny: NY,
                values: vec![300.0; NX * NY],
                range: Some((300.0, 300.0)),
                grid: None,
                lat_descending: false,
                style: None,
            },
            &StyleOverrideSettings::default(),
        );
        let generated_name = panel.selected_var().unwrap().to_string();
        assert_eq!(generated_name, "formula_temperature_2m");
        assert!(panel.restore_generated_field(&generated_name));

        panel.set_hour(hour, vec![real.clone()]);
        panel.selected_var = Some(generated_name.clone());
        assert!(panel.restore_generated_field(&generated_name));
        panel.set_hour(test_hour(1), vec![real]);
        panel.selected_var = Some(generated_name.clone());
        assert!(!panel.restore_generated_field(&generated_name));
    }

    fn formula_field(hour: HourKey, var: &str, values: Vec<f32>) -> FieldData {
        FieldData {
            key: FieldKey {
                hour,
                var: var.to_string(),
            },
            units: "m/s".to_string(),
            nx: values.len(),
            ny: 1,
            range: crate::colormap::finite_min_max(&values),
            values,
            grid: None,
            lat_descending: false,
            style: None,
        }
    }

    fn formula_wind_settings(product: &str) -> StyleOverrideSettings {
        let mut settings = StyleOverrideSettings::default();
        let mut table = UserColorTable::simple("Formula wind", "Formula wind", "kt");
        table.convert = UserUnitConvert::MsToKnots;
        settings.upsert_table(table);
        settings.bind_product(product, "Formula wind");
        settings
    }

    #[test]
    fn generated_field_has_plot_ready_auto_style_without_a_saved_binding() {
        let hour = test_hour(0);
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), Vec::new());
        panel.install_generated_field(
            formula_field(hour, "custom_diagnostic", vec![-4.0, 2.0, 1000.0]),
            &StyleOverrideSettings::default(),
        );

        let displayed = panel.current_field().unwrap();
        let style = displayed.style.as_ref().unwrap();
        assert_eq!(displayed.units, "m/s");
        assert_eq!(displayed.values, vec![-4.0, 2.0, 1000.0]);
        assert_eq!(displayed.range, Some((-4.0, 1000.0)));
        assert!(style.convert.is_none());
        assert!(style.title.contains("auto, full finite range"));
        let scale = style.scale.resolved_discrete();
        assert_eq!(scale.levels.first(), Some(&-4.0));
        assert_eq!(scale.levels.last(), Some(&1000.0));
    }

    #[test]
    fn generated_field_uses_existing_exact_output_binding_on_install() {
        let hour = test_hour(0);
        let raw_values = vec![5.0, 10.0, f32::NAN];
        let settings = formula_wind_settings("wind_over_15ms");
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), Vec::new());
        panel.install_generated_field(
            formula_field(hour, "wind_over_15ms", raw_values.clone()),
            &settings,
        );

        let displayed = panel.current_field().unwrap();
        let convert = rustwx_products::viewer::UnitConvert::MsToKnots;
        assert!(displayed.style.is_some());
        assert_eq!(displayed.units, "kt");
        assert_eq!(displayed.values[0], convert.apply(raw_values[0]));
        assert_eq!(displayed.values[1], convert.apply(raw_values[1]));
        assert!(displayed.values[2].is_nan());
        assert_eq!(
            displayed.range,
            Some((convert.apply(5.0), convert.apply(10.0)))
        );
        let retained = &panel.generated_field.as_ref().unwrap().raw.values;
        assert_eq!(retained.len(), raw_values.len());
        assert_eq!(retained[..2], raw_values[..2]);
        assert!(retained[2].is_nan());
        assert_eq!(panel.generated_field.as_ref().unwrap().raw.units, "m/s");
    }

    #[test]
    fn generated_field_restyle_always_converts_from_raw_once() {
        let hour = test_hour(0);
        let raw_values = vec![10.0, 20.0];
        let settings = formula_wind_settings("formula_wind");
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), Vec::new());
        panel.install_generated_field(
            formula_field(hour, "formula_wind", raw_values.clone()),
            &settings,
        );
        let first = panel.current_field().unwrap().values.clone();

        assert!(panel.restyle_generated_field(&settings));
        assert_eq!(panel.current_field().unwrap().values, first);
        assert!(panel.restyle_generated_field(&settings));
        assert_eq!(panel.current_field().unwrap().values, first);
        assert_eq!(panel.generated_field.as_ref().unwrap().raw.values, raw_values);
    }

    #[test]
    fn removing_generated_binding_reverts_to_unconverted_auto_style() {
        let hour = test_hour(0);
        let raw_values = vec![10.0, 20.0];
        let settings = formula_wind_settings("formula_wind");
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), Vec::new());
        panel.install_generated_field(
            formula_field(hour, "formula_wind", raw_values.clone()),
            &settings,
        );

        assert!(panel.restyle_generated_field(&StyleOverrideSettings::default()));
        let displayed = panel.current_field().unwrap();
        assert!(displayed.style.as_ref().unwrap().convert.is_none());
        assert_eq!(displayed.units, "m/s");
        assert_eq!(displayed.values, raw_values);
        assert_eq!(displayed.range, Some((10.0, 20.0)));
    }

    #[test]
    fn generated_field_restore_keeps_style_and_single_conversion() {
        let hour = test_hour(0);
        let settings = formula_wind_settings("formula_wind");
        let mut panel = FieldViewerPanel::new();
        panel.set_hour(hour.clone(), Vec::new());
        panel.install_generated_field(
            formula_field(hour.clone(), "formula_wind", vec![10.0]),
            &settings,
        );
        let expected = panel.current_field().unwrap().clone();

        panel.set_hour(hour, Vec::new());
        assert_eq!(panel.selected_var(), Some("formula_wind"));
        assert!(panel.restore_generated_field("formula_wind"));
        assert_eq!(panel.current_field(), Some(&expected));
    }

    #[test]
    fn generated_auto_style_handles_constant_and_nonfinite_ranges() {
        let hour = test_hour(0);
        let constant = formula_field(hour.clone(), "constant", vec![7.0, 7.0]);
        let constant_style = auto_generated_field_style(&constant);
        let constant_scale = constant_style.scale.resolved_discrete();
        assert!(constant_scale.levels.first().unwrap() < &7.0);
        assert!(constant_scale.levels.last().unwrap() > &7.0);

        let zero = formula_field(hour.clone(), "zero", vec![0.0, 0.0]);
        let zero_style = auto_generated_field_style(&zero);
        let zero_scale = zero_style.scale.resolved_discrete();
        assert_eq!(zero_scale.levels.first(), Some(&-1.0));
        assert_eq!(zero_scale.levels.last(), Some(&1.0));

        let missing = formula_field(hour, "missing", vec![f32::NAN, f32::INFINITY]);
        let missing_style = auto_generated_field_style(&missing);
        let missing_scale = missing_style.scale.resolved_discrete();
        assert_eq!(missing_scale.levels.first(), Some(&0.0));
        assert_eq!(missing_scale.levels.last(), Some(&1.0));
        assert!(missing_style.title.contains("no finite values"));
    }

    /// Row-major lat array: ascending = row 0 south (20°N..), descending =
    /// row 0 north (50°N..).
    fn lats(descending: bool) -> Vec<f32> {
        (0..NY)
            .flat_map(|y| {
                let lat = if descending {
                    50.0 - y as f32
                } else {
                    20.0 + y as f32
                };
                std::iter::repeat_n(lat, NX)
            })
            .collect()
    }

    /// The dangerous half of the orientation bug: for BOTH storage orders, a
    /// click near the BOTTOM of the displayed (north-up) image must resolve
    /// to the SOUTHERNMOST stored latitudes, and the top to the northernmost.
    #[test]
    fn click_round_trip_matches_geography_in_both_orientations() {
        for descending in [false, true] {
            let lat = lats(descending);
            let flip_y = !descending; // what the viewer derives
            let south = if descending {
                50.0 - (NY - 1) as f32
            } else {
                20.0
            };
            let north = if descending {
                50.0
            } else {
                20.0 + (NY - 1) as f32
            };

            // v = 1 is the bottom edge of the image (screen-down).
            let (_, fy) = image_uv_to_grid(0.5, 0.999, NX, NY, flip_y);
            let iy = fy.round() as usize;
            assert_eq!(
                lat[iy * NX],
                south,
                "bottom click must sample the southernmost row (descending={descending})"
            );

            let (_, fy) = image_uv_to_grid(0.5, 0.001, NX, NY, flip_y);
            let iy = fy.round() as usize;
            assert_eq!(
                lat[iy * NX],
                north,
                "top click must sample the northernmost row (descending={descending})"
            );
        }
    }

    #[test]
    fn uv_grid_mapping_round_trips() {
        for flip_y in [false, true] {
            // Interior points (clamping not in play) must round-trip exactly.
            for &(u, v) in &[(0.25, 0.25), (0.5, 0.5), (0.8, 0.4), (0.3125, 0.75)] {
                let (fx, fy) = image_uv_to_grid(u, v, NX, NY, flip_y);
                let (u2, v2) = grid_to_image_uv(fx, fy, NX, NY, flip_y);
                assert!(
                    (u2 - u).abs() < 1e-12 && (v2 - v).abs() < 1e-12,
                    "round trip (flip_y={flip_y}): ({u}, {v}) -> ({fx}, {fy}) -> ({u2}, {v2})"
                );
            }
            // Texel centers land on integer grid coords.
            let (fx, fy) = image_uv_to_grid(0.5 / NX as f64, 0.5 / NY as f64, NX, NY, flip_y);
            assert_eq!(fx, 0.0);
            assert_eq!(fy, if flip_y { (NY - 1) as f64 } else { 0.0 });
        }
    }

    #[test]
    fn viewport_mapping_round_trips_inside_zoomed_region() {
        let view = GridViewport {
            x0: 2.0,
            x1: 6.0,
            y0: 1.0,
            y1: 5.0,
        };
        for flip_y in [false, true] {
            for &(u, v) in &[(0.25, 0.25), (0.5, 0.5), (0.8, 0.4)] {
                let (fx, fy) = view.image_uv_to_grid(u, v, NX, NY, flip_y);
                let (u2, v2) = view.grid_to_image_uv(fx, fy, NX, NY, flip_y);
                assert!(
                    (u2 - u).abs() < 1e-12 && (v2 - v).abs() < 1e-12,
                    "zoom round trip (flip_y={flip_y}): ({u}, {v}) -> ({fx}, {fy}) -> ({u2}, {v2})"
                );
            }
        }
    }

    #[test]
    fn viewport_uv_rect_uses_grid_edges() {
        let view = GridViewport {
            x0: 2.0,
            x1: 6.0,
            y0: 1.0,
            y1: 5.0,
        };
        let rect = view.texture_uv_rect(NX, NY, false);
        assert!((rect.left() - 0.25).abs() < 1.0e-6, "{rect:?}");
        assert!((rect.right() - 0.75).abs() < 1.0e-6, "{rect:?}");
        assert!((rect.top() - (1.0 / 6.0)).abs() < 1.0e-6, "{rect:?}");
        assert!((rect.bottom() - (5.0 / 6.0)).abs() < 1.0e-6, "{rect:?}");

        let flipped = view.texture_uv_rect(NX, NY, true);
        assert!((flipped.top() - (1.0 / 6.0)).abs() < 1.0e-6, "{flipped:?}");
        assert!(
            (flipped.bottom() - (5.0 / 6.0)).abs() < 1.0e-6,
            "{flipped:?}"
        );
    }

    #[test]
    fn viewport_zoom_keeps_cursor_anchor_stable() {
        let nx = 80;
        let ny = 60;
        let view = GridViewport::full(nx, ny);
        let (u, v) = view.grid_to_image_uv(30.0, 20.0, nx, ny, false);
        let zoomed = view.zoomed_at(30.0, 20.0, 2.0, nx, ny);
        let (u2, v2) = zoomed.grid_to_image_uv(30.0, 20.0, nx, ny, false);

        assert!(zoomed.width_cells() < view.width_cells(), "{zoomed:?}");
        assert!((u2 - u).abs() < 1e-12, "{u} -> {u2}");
        assert!((v2 - v).abs() < 1e-12, "{v} -> {v2}");
    }

    #[test]
    fn viewport_pan_clamps_to_grid_edges() {
        let view = GridViewport {
            x0: 2.0,
            x1: 6.0,
            y0: 1.0,
            y1: 5.0,
        };
        let panned = view.translated(100.0, -100.0, NX, NY);
        assert_eq!(panned.x1, NX as f64);
        assert_eq!(panned.y0, 0.0);
        assert_eq!(panned.width_cells(), view.width_cells());
        assert_eq!(panned.height_cells(), view.height_cells());
    }

    #[test]
    fn uv_mapping_clamps_to_the_grid() {
        for flip_y in [false, true] {
            for &(u, v) in &[(-0.2, -0.2), (1.2, 1.2), (0.0, 1.0), (1.0, 0.0)] {
                let (fx, fy) = image_uv_to_grid(u, v, NX, NY, flip_y);
                assert!((0.0..=(NX - 1) as f64).contains(&fx), "fx {fx} in range");
                assert!((0.0..=(NY - 1) as f64).contains(&fy), "fy {fy} in range");
            }
        }
    }

    #[test]
    fn domain_selection_scans_curvilinear_grid_bounds() {
        let nx = 4;
        let ny = 3;
        let lat = vec![
            30.0, 30.2, 30.4, 30.6, 31.0, 31.2, 31.4, 31.6, 32.0, 32.2, 32.4, 32.6,
        ];
        let lon = vec![
            -105.0, -104.8, -104.6, -104.4, -104.5, -104.3, -104.1, -103.9, -104.0, -103.8, -103.6,
            -103.4,
        ];
        let bounds = domain_bounds_from_grid_selection(
            &lat,
            &lon,
            nx,
            ny,
            DomainSelection {
                fx0: 1.0,
                fy0: 0.0,
                fx1: 3.0,
                fy1: 1.0,
                rotation_deg: 0.0,
            },
        )
        .expect("bounds");
        assert!((bounds.0 + 104.8).abs() < 1.0e-5, "{bounds:?}");
        assert!((bounds.1 + 103.9).abs() < 1.0e-5, "{bounds:?}");
        assert!((bounds.2 - 30.2).abs() < 1.0e-6, "{bounds:?}");
        assert!((bounds.3 - 31.6).abs() < 1.0e-6, "{bounds:?}");
    }

    #[test]
    fn domain_selection_handles_antimeridian() {
        let bounds = shortest_longitude_interval(&[178.0, 179.0, -179.5, -178.5]).unwrap();
        assert!(bounds.0 > 170.0, "{bounds:?}");
        assert!(bounds.1 < -170.0, "{bounds:?}");
        assert!(longitude_span(bounds.0, bounds.1) < 5.0, "{bounds:?}");
    }

    #[test]
    fn domain_selection_geometry_rotates_around_screen_center() {
        let image_rect = Rect::from_min_max(pos2(0.0, 0.0), pos2(100.0, 100.0));
        let (_center, corners) = domain_selection_geometry(
            image_rect,
            10,
            10,
            false,
            GridViewport::full(10, 10),
            DomainSelection {
                fx0: 1.0,
                fy0: 1.0,
                fx1: 8.0,
                fy1: 8.0,
                rotation_deg: 90.0,
            },
        )
        .expect("geometry");

        assert!((corners[0].x - 85.0).abs() < 1.0e-4, "{corners:?}");
        assert!((corners[0].y - 15.0).abs() < 1.0e-4, "{corners:?}");
        assert!((corners[1].x - 85.0).abs() < 1.0e-4, "{corners:?}");
        assert!((corners[1].y - 85.0).abs() < 1.0e-4, "{corners:?}");
    }

    #[test]
    fn rotation_normalizes_to_signed_half_turns() {
        assert!((normalize_rotation(370.0) - 10.0).abs() < 1.0e-9);
        assert!((normalize_rotation(-190.0) - 170.0).abs() < 1.0e-9);
        assert_eq!(normalize_rotation(f64::NAN), 0.0);
    }
}

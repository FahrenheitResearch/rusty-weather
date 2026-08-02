//! Native map plot viewer: render the selected store field through
//! `rustwx-render` as an RGBA image and upload it directly to egui.
//!
//! This is the interactive sibling of PNG export: the expensive PNG encode
//! path is intentionally skipped for on-screen display.

use std::time::{Duration, Instant};

use egui::{
    ColorImage, ComboBox, DragValue, Image, Label, RichText, TextEdit, TextureFilter,
    TextureHandle, TextureOptions, Ui, Vec2,
};
use rustwx_core::{Field2D, GridShape, LatLonGrid, ProductKey};
use rustwx_render::{
    BasemapDetail, DomainFrame, LineworkRole, MapRenderRequest, PolygonRole, ProductVisualMode,
    RasterSampleMode, RenderPresentation, RgbaImage, StaticPlotStyle,
};
use serde::{Deserialize, Serialize};

use crate::profile_scope;
use crate::worker::{FieldData, FieldKey, HourKey};

const RESIZE_SETTLE_TIME: Duration = Duration::from_millis(180);
const ACTIVE_RESIZE_REPAINT: Duration = Duration::from_millis(16);

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlotCacheKey {
    field: FieldKey,
    width: u32,
    height: u32,
    domain: Option<DomainCacheKey>,
    settings: NativePlotSettings,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DomainCacheKey {
    name: String,
    bounds_microdeg: [i64; 4],
    rotation_millideg: i64,
}

#[derive(Debug, Clone, Copy)]
struct PendingRenderSize {
    target: (u32, u32),
    changed_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeRenderDecision {
    Wait(Duration),
    RenderNow,
}

#[derive(Debug, Default)]
struct ResizeRenderDebounce {
    pending: Option<PendingRenderSize>,
}

impl ResizeRenderDebounce {
    fn clear(&mut self) {
        self.pending = None;
    }

    fn decide(
        &mut self,
        target: (u32, u32),
        interaction_active: bool,
        now: Instant,
    ) -> ResizeRenderDecision {
        if self.pending.is_none_or(|pending| pending.target != target) {
            self.pending = Some(PendingRenderSize {
                target,
                changed_at: now,
            });
        }

        let pending = self.pending.expect("pending render size was just set");
        let elapsed = now.saturating_duration_since(pending.changed_at);
        if !interaction_active && elapsed >= RESIZE_SETTLE_TIME {
            self.pending = None;
            return ResizeRenderDecision::RenderNow;
        }

        let wait = if interaction_active {
            ACTIVE_RESIZE_REPAINT
        } else {
            RESIZE_SETTLE_TIME
                .saturating_sub(elapsed)
                .max(Duration::from_millis(1))
        };
        ResizeRenderDecision::Wait(wait)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomDomain {
    pub name: String,
    pub bounds: (f64, f64, f64, f64),
    #[serde(default)]
    pub rotation_deg: f64,
}

impl CustomDomain {
    pub fn new(name: impl Into<String>, bounds: (f64, f64, f64, f64)) -> Self {
        Self {
            name: name.into(),
            bounds: normalize_domain_bounds(bounds),
            rotation_deg: 0.0,
        }
    }

    pub fn generated(bounds: (f64, f64, f64, f64)) -> Self {
        Self::new(default_domain_name(bounds), bounds)
    }

    pub fn bounds_label(&self) -> String {
        format_domain_label(self.bounds, self.rotation_deg)
    }

    pub fn with_rotation(mut self, rotation_deg: f64) -> Self {
        self.rotation_deg = normalize_rotation(rotation_deg);
        self
    }
}

impl From<&CustomDomain> for DomainCacheKey {
    fn from(domain: &CustomDomain) -> Self {
        let (west, east, south, north) = domain.bounds;
        Self {
            name: domain.name.clone(),
            bounds_microdeg: [
                (west * 1_000_000.0).round() as i64,
                (east * 1_000_000.0).round() as i64,
                (south * 1_000_000.0).round() as i64,
                (north * 1_000_000.0).round() as i64,
            ],
            rotation_millideg: (domain.rotation_deg * 1_000.0).round() as i64,
        }
    }
}

/// Source-detail override for the native plot basemap.
///
/// `Counties` uses the regional source and enables county linework when it is
/// selected in the UI. Keeping it distinct makes the expensive/dense choice
/// discoverable without expanding rustwx-render's three source tiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativePlotMapDetail {
    #[default]
    Auto,
    Global,
    Broad,
    Regional,
    Counties,
}

impl NativePlotMapDetail {
    const ALL: [Self; 5] = [
        Self::Auto,
        Self::Global,
        Self::Broad,
        Self::Regional,
        Self::Counties,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Auto => "Auto",
            Self::Global => "Global",
            Self::Broad => "Broad",
            Self::Regional => "Regional",
            Self::Counties => "Counties (US)",
        }
    }

    fn basemap_detail(self) -> Option<BasemapDetail> {
        match self {
            Self::Auto => None,
            Self::Global => Some(BasemapDetail::Global),
            Self::Broad => Some(BasemapDetail::Broad),
            Self::Regional | Self::Counties => Some(BasemapDetail::Regional),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativePlotStyle {
    Default,
    CleanAtlas,
    CleanAtlasFast,
    CleanAtlasQuality2x,
    CleanAtlasCombined,
    Operational,
    #[default]
    OperationalFast,
    OperationalQuality2x,
    OperationalBudget30s,
}

impl NativePlotStyle {
    const ALL: [Self; 9] = [
        Self::OperationalFast,
        Self::Operational,
        Self::OperationalQuality2x,
        Self::OperationalBudget30s,
        Self::CleanAtlasFast,
        Self::CleanAtlas,
        Self::CleanAtlasQuality2x,
        Self::CleanAtlasCombined,
        Self::Default,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::Default => "Classic",
            Self::CleanAtlas => "Atlas",
            Self::CleanAtlasFast => "Atlas fast",
            Self::CleanAtlasQuality2x => "Atlas quality 2x",
            Self::CleanAtlasCombined => "Atlas best",
            Self::Operational => "Operational",
            Self::OperationalFast => "Operational fast",
            Self::OperationalQuality2x => "Operational quality 2x",
            Self::OperationalBudget30s => "Operational best",
        }
    }

    fn render_style(self) -> StaticPlotStyle {
        match self {
            Self::Default => StaticPlotStyle::Default,
            Self::CleanAtlas => StaticPlotStyle::CleanAtlas,
            Self::CleanAtlasFast => StaticPlotStyle::CleanAtlasFast,
            Self::CleanAtlasQuality2x => StaticPlotStyle::CleanAtlasQuality2x,
            Self::CleanAtlasCombined => StaticPlotStyle::CleanAtlasCombined,
            Self::Operational => StaticPlotStyle::Operational,
            Self::OperationalFast => StaticPlotStyle::OperationalFast,
            Self::OperationalQuality2x => StaticPlotStyle::OperationalQuality2x,
            Self::OperationalBudget30s => StaticPlotStyle::OperationalBudget30s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativePlotSampling {
    #[default]
    Smooth,
    PixelExact,
}

impl NativePlotSampling {
    const ALL: [Self; 2] = [Self::Smooth, Self::PixelExact];

    fn label(self) -> &'static str {
        match self {
            Self::Smooth => "Smooth",
            Self::PixelExact => "Pixel exact",
        }
    }

    fn raster_sample_mode(self) -> RasterSampleMode {
        match self {
            Self::Smooth => RasterSampleMode::Linear,
            Self::PixelExact => RasterSampleMode::Nearest,
        }
    }

    fn texture_filter(self) -> TextureFilter {
        match self {
            Self::Smooth => TextureFilter::Linear,
            Self::PixelExact => TextureFilter::Nearest,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NativePlotRenderScale {
    One,
    #[default]
    OnePointFive,
    Two,
    Three,
}

impl NativePlotRenderScale {
    const ALL: [Self; 4] = [Self::One, Self::OnePointFive, Self::Two, Self::Three];

    fn label(self) -> &'static str {
        match self {
            Self::One => "1x",
            Self::OnePointFive => "1.5x",
            Self::Two => "2x",
            Self::Three => "3x",
        }
    }

    fn factor(self) -> f32 {
        match self {
            Self::One => 1.0,
            Self::OnePointFive => 1.5,
            Self::Two => 2.0,
            Self::Three => 3.0,
        }
    }
}

/// Serializable native-map presentation state. The panel edits a staged copy;
/// pressing `Rerender` atomically applies the whole snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NativePlotSettings {
    pub map_detail: NativePlotMapDetail,
    pub show_coastlines: bool,
    pub show_countries: bool,
    pub show_states: bool,
    pub show_counties: bool,
    pub show_lakes: bool,
    pub line_width_percent: u16,
    pub line_opacity_percent: u16,
    pub sampling: NativePlotSampling,
    pub plot_style: NativePlotStyle,
    pub render_scale: NativePlotRenderScale,
}

impl Default for NativePlotSettings {
    fn default() -> Self {
        Self {
            map_detail: NativePlotMapDetail::Auto,
            show_coastlines: true,
            show_countries: true,
            show_states: true,
            show_counties: true,
            show_lakes: true,
            line_width_percent: 100,
            line_opacity_percent: 100,
            sampling: NativePlotSampling::Smooth,
            plot_style: NativePlotStyle::OperationalFast,
            render_scale: NativePlotRenderScale::OnePointFive,
        }
    }
}

impl NativePlotSettings {
    fn normalized(mut self) -> Self {
        self.line_width_percent = self.line_width_percent.clamp(50, 300);
        self.line_opacity_percent = self.line_opacity_percent.min(200);
        self
    }

    fn linework_visible(&self, role: LineworkRole) -> bool {
        match role {
            LineworkRole::Coast => self.show_coastlines,
            LineworkRole::Lake => self.show_lakes,
            LineworkRole::International => self.show_countries,
            LineworkRole::State => self.show_states,
            LineworkRole::County => self.show_counties,
            LineworkRole::Generic => true,
        }
    }
}

#[derive(Default)]
pub struct PlotViewerPanel {
    texture: Option<TextureHandle>,
    cache_key: Option<PlotCacheKey>,
    error: Option<String>,
    last_render_ms: Option<f32>,
    last_upload_ms: Option<f32>,
    draft_settings: NativePlotSettings,
    applied_settings: NativePlotSettings,
    settings_dirty: bool,
    active_domain: Option<CustomDomain>,
    saved_domains: Vec<CustomDomain>,
    domain_name_edit: String,
    saved_domains_dirty: bool,
    resize_render_debounce: ResizeRenderDebounce,
}

impl PlotViewerPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.texture = None;
        self.cache_key = None;
        self.error = None;
        self.last_render_ms = None;
        self.last_upload_ms = None;
        self.resize_render_debounce.clear();
    }

    /// Currently applied native-map settings. UI edits remain staged until
    /// [`Self::rerender`] is called (the popout's button calls the same API).
    pub fn settings(&self) -> &NativePlotSettings {
        &self.applied_settings
    }

    pub fn has_pending_settings(&self) -> bool {
        self.draft_settings != self.applied_settings
    }

    /// Replace both staged and applied settings, as used by a host restoring a
    /// persisted session. Invalid numeric fields are clamped at this boundary.
    pub fn set_settings(&mut self, settings: NativePlotSettings) {
        let settings = settings.normalized();
        self.draft_settings = settings.clone();
        self.applied_settings = settings;
        self.settings_dirty = false;
        self.clear();
    }

    pub fn settings_json(&self) -> serde_json::Value {
        serde_json::to_value(&self.applied_settings).unwrap_or(serde_json::Value::Null)
    }

    /// Restore a serialized settings snapshot. Malformed/newer data leaves the
    /// current settings untouched and reports `false` to the host.
    pub fn apply_settings_json(&mut self, value: &serde_json::Value) -> bool {
        match serde_json::from_value::<NativePlotSettings>(value.clone()) {
            Ok(settings) => {
                self.set_settings(settings);
                true
            }
            Err(_) => false,
        }
    }

    pub fn take_settings_changed(&mut self) -> bool {
        let changed = self.settings_dirty;
        self.settings_dirty = false;
        changed
    }

    /// Atomically apply every staged control and force a fresh image even when
    /// the resulting snapshot equals the previous one.
    pub fn rerender(&mut self) {
        let settings = self.draft_settings.clone().normalized();
        self.draft_settings = settings.clone();
        self.applied_settings = settings;
        self.settings_dirty = true;
        self.clear();
    }

    pub fn set_active_domain(&mut self, domain: CustomDomain) {
        let domain = normalize_domain(domain);
        self.active_domain = Some(domain.clone());
        self.domain_name_edit = domain.name;
        self.clear();
    }

    pub fn set_active_domain_rotation(&mut self, rotation_deg: f64) {
        let Some(domain) = self.active_domain.as_mut() else {
            return;
        };
        let rotation_deg = normalize_rotation(rotation_deg);
        if (domain.rotation_deg - rotation_deg).abs() < 0.01 {
            return;
        }
        domain.rotation_deg = rotation_deg;
        self.clear();
    }

    pub fn active_domain(&self) -> Option<&CustomDomain> {
        self.active_domain.as_ref()
    }

    /// Return to the complete native grid and invalidate any domain render.
    ///
    /// This is the programmatic twin of the panel's `Full grid` button. Hosts
    /// use it when a newly selected run must not inherit a custom or
    /// auto-seeded domain from the previous run.
    pub fn show_full_grid(&mut self) {
        self.active_domain = None;
        self.domain_name_edit.clear();
        self.clear();
    }

    pub fn saved_domains(&self) -> &[CustomDomain] {
        &self.saved_domains
    }

    pub fn set_saved_domains(&mut self, mut domains: Vec<CustomDomain>) {
        normalize_saved_domains(&mut domains);
        self.saved_domains = domains;
        self.saved_domains_dirty = false;
    }

    pub fn take_saved_domains_changed(&mut self) -> bool {
        let changed = self.saved_domains_dirty;
        self.saved_domains_dirty = false;
        changed
    }

    pub fn last_timings(&self) -> Option<(f32, f32)> {
        Some((self.last_render_ms?, self.last_upload_ms.unwrap_or(0.0)))
    }

    pub fn ui(&mut self, ui: &mut Ui, field: Option<&FieldData>) {
        ui.vertical(|ui| {
            self.domain_controls(ui);
            self.settings_controls(ui);

            let Some(field) = field else {
                self.clear();
                ui.label(RichText::new("Load a field to render a native plot.").weak());
                return;
            };

            // The plot must fit *inside* the remaining window. The old code
            // made the image as tall as all remaining space and then appended
            // the timing row below it. That made egui grow the window, which
            // yielded a larger plot on the next frame and repeated until the
            // screen edge. Reserving the footer breaks that feedback loop.
            let available = ui.available_size();
            let footer_height = ui.spacing().interact_size.y + ui.spacing().item_spacing.y;
            let plot_available = Vec2::new(available.x, (available.y - footer_height).max(1.0));
            let target_aspect = self
                .active_domain
                .as_ref()
                .map(|domain| domain_plot_aspect(domain.bounds, domain.rotation_deg))
                .unwrap_or(16.0 / 9.0);
            let display_size = fitted_plot_size(plot_available, target_aspect);
            let display_width = display_size.x.floor().max(1.0) as u32;
            let display_height = display_size.y.floor().max(1.0) as u32;
            let (width, height) = render_plot_size(
                display_width,
                display_height,
                self.applied_settings.render_scale.factor(),
            );
            let key = PlotCacheKey {
                field: field.key.clone(),
                width,
                height,
                domain: self.active_domain.as_ref().map(DomainCacheKey::from),
                settings: self.applied_settings.clone(),
            };

            if self.cache_key.as_ref() != Some(&key) {
                let content_changed = self.cache_key.as_ref().is_none_or(|cached| {
                    cached.field != key.field
                        || cached.domain != key.domain
                        || cached.settings != key.settings
                });
                let interaction_active = ui.input(|input| input.pointer.any_down());
                let decision = if content_changed {
                    ResizeRenderDecision::RenderNow
                } else {
                    self.resize_render_debounce.decide(
                        (width, height),
                        interaction_active,
                        Instant::now(),
                    )
                };
                match decision {
                    ResizeRenderDecision::RenderNow => {
                        self.render(ui, field, self.active_domain.clone(), key);
                    }
                    ResizeRenderDecision::Wait(wait) => ui.ctx().request_repaint_after(wait),
                }
            } else {
                self.resize_render_debounce.clear();
            }

            if let Some(message) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, message);
                return;
            }

            let Some(texture) = &self.texture else {
                ui.spinner();
                return;
            };

            ui.add(Image::new(texture).fit_to_exact_size(display_size));
            if let Some((render_ms, upload_ms)) = self.last_timings() {
                ui.add(
                    Label::new(
                        RichText::new(format!(
                            "native plot render {:.0} ms / upload {:.0} ms / {}x{}",
                            render_ms, upload_ms, width, height
                        ))
                        .small()
                        .weak(),
                    )
                    .truncate(),
                );
            }
        });
    }

    fn domain_controls(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Domain").small().strong());
            if ui
                .selectable_label(self.active_domain.is_none(), "Full grid")
                .clicked()
            {
                self.show_full_grid();
            }

            if !self.saved_domains.is_empty() {
                let selected = self
                    .active_domain
                    .as_ref()
                    .map(|domain| domain.name.as_str())
                    .unwrap_or("saved domains");
                let mut picked: Option<CustomDomain> = None;
                ComboBox::from_id_salt("rw-ui-plot-domain-picker")
                    .selected_text(selected)
                    .width(180.0)
                    .show_ui(ui, |ui| {
                        for domain in &self.saved_domains {
                            if ui
                                .selectable_label(
                                    self.active_domain
                                        .as_ref()
                                        .is_some_and(|active| active.name == domain.name),
                                    &domain.name,
                                )
                                .clicked()
                            {
                                picked = Some(domain.clone());
                            }
                        }
                    });
                if let Some(domain) = picked {
                    self.set_active_domain(domain);
                }
            }

            let active_bounds = self.active_domain.as_ref().map(|domain| domain.bounds);
            if let Some(bounds) = active_bounds {
                if self.domain_name_edit.trim().is_empty() {
                    self.domain_name_edit = default_domain_name(bounds);
                }
                ui.add(
                    TextEdit::singleline(&mut self.domain_name_edit)
                        .desired_width(160.0)
                        .hint_text("domain name"),
                );
                if ui
                    .button("Save")
                    .on_hover_text("save the current custom domain")
                    .clicked()
                {
                    self.save_active_domain();
                }
                let active_name = self
                    .active_domain
                    .as_ref()
                    .map(|domain| domain.name.as_str())
                    .unwrap_or_default();
                let can_delete = self
                    .saved_domains
                    .iter()
                    .any(|domain| domain.name == active_name);
                if ui
                    .add_enabled(can_delete, egui::Button::new("Delete"))
                    .on_hover_text("remove this saved domain")
                    .clicked()
                {
                    self.delete_active_domain();
                }
                let mut rotation_deg = self
                    .active_domain
                    .as_ref()
                    .map(|domain| domain.rotation_deg)
                    .unwrap_or(0.0);
                if ui
                    .add(
                        DragValue::new(&mut rotation_deg)
                            .speed(0.25)
                            .range(-180.0..=180.0)
                            .suffix(" deg"),
                    )
                    .on_hover_text("rotate this custom domain")
                    .changed()
                {
                    self.set_active_domain_rotation(rotation_deg);
                }
                let rotation_deg = self
                    .active_domain
                    .as_ref()
                    .map(|domain| domain.rotation_deg)
                    .unwrap_or(0.0);
                ui.label(
                    RichText::new(format_domain_label(bounds, rotation_deg))
                        .small()
                        .weak(),
                );
            } else {
                ui.label(RichText::new("full model grid").small().weak());
            }
        });
    }

    fn settings_controls(&mut self, ui: &mut Ui) {
        egui::CollapsingHeader::new("Map & render settings")
            .default_open(true)
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Detail").small().strong());
                    let previous_detail = self.draft_settings.map_detail;
                    ComboBox::from_id_salt("rw-ui-native-plot-map-detail")
                        .selected_text(self.draft_settings.map_detail.label())
                        .width(128.0)
                        .show_ui(ui, |ui| {
                            for detail in NativePlotMapDetail::ALL {
                                ui.selectable_value(
                                    &mut self.draft_settings.map_detail,
                                    detail,
                                    detail.label(),
                                );
                            }
                        });
                    if self.draft_settings.map_detail != previous_detail {
                        match self.draft_settings.map_detail {
                            NativePlotMapDetail::Counties => {
                                self.draft_settings.show_counties = true;
                            }
                            NativePlotMapDetail::Regional => {
                                self.draft_settings.show_counties = false;
                            }
                            _ => {}
                        }
                    }

                    ui.checkbox(&mut self.draft_settings.show_coastlines, "Coasts");
                    ui.checkbox(&mut self.draft_settings.show_countries, "Countries");
                    ui.checkbox(
                        &mut self.draft_settings.show_states,
                        "States / provinces",
                    );
                    ui.checkbox(&mut self.draft_settings.show_counties, "Counties (US)")
                        .on_hover_text(
                            "County boundaries are available over the United States; use States / provinces for worldwide first-order subdivisions",
                        );
                    ui.checkbox(&mut self.draft_settings.show_lakes, "Lakes");
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Lines").small().strong());
                    ui.add(
                        egui::Slider::new(&mut self.draft_settings.line_width_percent, 50..=300)
                            .text("Width")
                            .suffix("%"),
                    );
                    ui.add(
                        egui::Slider::new(&mut self.draft_settings.line_opacity_percent, 0..=200)
                            .text("Opacity")
                            .suffix("%"),
                    );
                });

                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new("Render").small().strong());
                    ComboBox::from_id_salt("rw-ui-native-plot-style")
                        .selected_text(self.draft_settings.plot_style.label())
                        .width(164.0)
                        .show_ui(ui, |ui| {
                            for style in NativePlotStyle::ALL {
                                ui.selectable_value(
                                    &mut self.draft_settings.plot_style,
                                    style,
                                    style.label(),
                                );
                            }
                        });
                    ComboBox::from_id_salt("rw-ui-native-plot-sampling")
                        .selected_text(self.draft_settings.sampling.label())
                        .width(104.0)
                        .show_ui(ui, |ui| {
                            for sampling in NativePlotSampling::ALL {
                                ui.selectable_value(
                                    &mut self.draft_settings.sampling,
                                    sampling,
                                    sampling.label(),
                                );
                            }
                        });
                    ui.label(RichText::new("Resolution").small().strong());
                    for scale in NativePlotRenderScale::ALL {
                        ui.selectable_value(
                            &mut self.draft_settings.render_scale,
                            scale,
                            scale.label(),
                        )
                        .on_hover_text("native plot render scale");
                    }
                });

                ui.horizontal_wrapped(|ui| {
                    if ui
                        .button("Defaults")
                        .on_hover_text("stage the default native map settings")
                        .clicked()
                    {
                        self.draft_settings = NativePlotSettings::default();
                    }
                    let pending = self.has_pending_settings();
                    if ui
                        .button(if pending { "Rerender *" } else { "Rerender" })
                        .on_hover_text("apply every staged setting and rebuild the native plot")
                        .clicked()
                    {
                        self.rerender();
                    }
                    if self.has_pending_settings() {
                        ui.label(RichText::new("changes pending").small().weak());
                    }
                });
            });
    }

    fn save_active_domain(&mut self) {
        let Some(active) = self.active_domain.as_mut() else {
            return;
        };
        let name = self.domain_name_edit.trim();
        if name.is_empty() {
            return;
        }
        active.name = name.to_string();
        active.bounds = normalize_domain_bounds(active.bounds);
        active.rotation_deg = normalize_rotation(active.rotation_deg);
        if let Some(existing) = self
            .saved_domains
            .iter_mut()
            .find(|domain| domain.name.eq_ignore_ascii_case(name))
        {
            *existing = active.clone();
        } else {
            self.saved_domains.push(active.clone());
        }
        normalize_saved_domains(&mut self.saved_domains);
        self.saved_domains_dirty = true;
        self.clear();
    }

    fn delete_active_domain(&mut self) {
        let Some(active) = self.active_domain.as_ref() else {
            return;
        };
        let before = self.saved_domains.len();
        self.saved_domains
            .retain(|domain| domain.name != active.name);
        if self.saved_domains.len() != before {
            self.saved_domains_dirty = true;
        }
    }

    fn render(
        &mut self,
        ui: &Ui,
        field: &FieldData,
        domain: Option<CustomDomain>,
        key: PlotCacheKey,
    ) {
        profile_scope!("native_plot_render");
        self.resize_render_debounce.clear();
        self.texture = None;
        self.error = None;
        self.cache_key = Some(key.clone());

        let render_start = std::time::Instant::now();
        let image =
            match render_field_plot(field, key.width, key.height, domain.as_ref(), &key.settings) {
                Ok(image) => image,
                Err(err) => {
                    self.error = Some(err);
                    self.last_render_ms = None;
                    self.last_upload_ms = None;
                    return;
                }
            };
        self.last_render_ms = Some(render_start.elapsed().as_secs_f32() * 1000.0);

        let upload_start = std::time::Instant::now();
        let texture_filter = key.settings.sampling.texture_filter();
        self.texture = Some(ui.ctx().load_texture(
            "rw-ui-native-plot",
            rgba_to_color_image(&image),
            TextureOptions {
                magnification: texture_filter,
                minification: texture_filter,
                ..Default::default()
            },
        ));
        self.last_upload_ms = Some(upload_start.elapsed().as_secs_f32() * 1000.0);
    }
}

fn render_field_plot(
    field: &FieldData,
    width: u32,
    height: u32,
    domain: Option<&CustomDomain>,
    settings: &NativePlotSettings,
) -> Result<RgbaImage, String> {
    let style = field
        .style
        .as_ref()
        .ok_or_else(|| "selected field has no production plot style yet".to_string())?;
    let grid_file = field
        .grid
        .as_ref()
        .ok_or_else(|| "selected field has no readable run grid".to_string())?;
    if grid_file.nx != field.nx || grid_file.ny != field.ny {
        return Err(format!(
            "grid {}x{} does not match field {}x{}",
            grid_file.nx, grid_file.ny, field.nx, field.ny
        ));
    }

    let grid = LatLonGrid {
        shape: GridShape {
            nx: field.nx,
            ny: field.ny,
        },
        lat_deg: grid_file.lat.clone(),
        lon_deg: grid_file.lon.clone(),
    };
    let core_field = Field2D::new(
        ProductKey::named(field.key.var.clone()),
        field.units.clone(),
        grid,
        field.values.clone(),
    )
    .map_err(|err| err.to_string())?;

    let bounds = geographic_bounds(&grid_file.lat, &grid_file.lon)
        .ok_or_else(|| "grid has no finite lat/lon bounds".to_string())?;
    let render_bounds = domain.map(|domain| domain.bounds).unwrap_or(bounds);
    let target_ratio = width as f64 / height as f64;
    let basemap_detail = settings.map_detail.basemap_detail();
    let projected = if let Some(domain) = domain {
        match (
            rotated_domain_needs_basemap_padding(domain.rotation_deg),
            basemap_detail,
        ) {
            (true, Some(detail)) => rustwx_products::direct::build_natural_projected_map_with_projection_and_basemap_padding_and_detail(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                target_ratio,
                1.35,
                1.10,
                detail,
            ),
            (true, None) => rustwx_products::direct::build_natural_projected_map_with_projection_and_basemap_padding(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                target_ratio,
                1.35,
                1.10,
            ),
            (false, Some(detail)) => rustwx_products::direct::build_natural_projected_map_with_projection_and_basemap_detail(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                target_ratio,
                detail,
            ),
            (false, None) => rustwx_products::direct::build_natural_projected_map_with_projection(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                target_ratio,
            ),
        }
    } else if let Some(detail) = basemap_detail {
        rustwx_products::direct::build_projected_map_with_projection_and_basemap_detail(
            &grid_file.lat,
            &grid_file.lon,
            grid_file.projection.as_ref(),
            render_bounds,
            target_ratio,
            detail,
        )
    } else {
        rustwx_products::direct::build_projected_map_with_projection(
            &grid_file.lat,
            &grid_file.lon,
            grid_file.projection.as_ref(),
            render_bounds,
            target_ratio,
        )
    }
    .map_err(|err| err.to_string())?;
    let mut projected = match domain {
        Some(domain) => projected.rotated_degrees(domain.rotation_deg),
        None => projected,
    };
    apply_native_map_settings(&mut projected, settings);

    let mut request = MapRenderRequest::from_core_field(core_field, style.scale.clone());
    rustwx_products::plot_design::StaticPlotDesign::new(
        render_bounds,
        ProductVisualMode::FilledMeteorology,
    )
    .apply_to_request(&mut request);
    if domain.is_some() {
        request.domain_frame = Some(DomainFrame {
            inset_px: 2,
            outline_width: 2,
            ..DomainFrame::map_viewport_default()
        });
    }
    request.apply_projected_map(&projected);
    request.title = Some(match domain {
        Some(domain) => format!("{} - {}", style.title, domain.name),
        None => style.title.clone(),
    });
    request.subtitle_left = Some(plot_time_subtitle(&field.key.hour));
    request.subtitle_right = Some(field.key.hour.model.to_ascii_uppercase());
    request.width = width;
    request.height = height;
    request.render_density = style.colormap_options.render_density;
    request.legend = style.colormap_options.legend;
    request.legend.mode = style.legend_mode;
    request.cbar_tick_step = style.cbar_tick_step;
    request.supersample_factor = 1;
    request.raster_sample_mode = settings.sampling.raster_sample_mode();

    rustwx_render::render_image_with_style(&request, settings.plot_style.render_style())
        .map_err(|err| err.to_string())
}

fn apply_native_map_settings(
    projected: &mut rustwx_render::ProjectedMap,
    settings: &NativePlotSettings,
) {
    projected
        .lines
        .retain(|line| settings.linework_visible(line.role));
    if !settings.show_lakes {
        projected
            .polygons
            .retain(|polygon| polygon.role != PolygonRole::Lake);
    }

    if settings.line_opacity_percent == 0 {
        projected.lines.clear();
        return;
    }

    let width_percent = u32::from(settings.line_width_percent);
    let opacity_percent = u32::from(settings.line_opacity_percent);
    let presentation = RenderPresentation::for_mode_with_style(
        ProductVisualMode::FilledMeteorology,
        settings.plot_style.render_style(),
    );
    for line in &mut projected.lines {
        // Resolve the selected plot style first, then mark the line as generic
        // so the renderer does not restyle/clamp it a second time. This makes
        // the panel's width and opacity controls authoritative per request,
        // without mutating process-wide linework environment variables.
        let styled = presentation.linework_style(line.role, line.color.into(), line.width);
        line.color = styled.color.into();
        line.width = styled.width;
        line.role = LineworkRole::Generic;
        line.width =
            (line.width.saturating_mul(width_percent).saturating_add(50) / 100).clamp(1, 16);
        line.color.a = (u32::from(line.color.a)
            .saturating_mul(opacity_percent)
            .saturating_add(50)
            / 100)
            .min(255) as u8;
    }
}

fn plot_time_subtitle(hour: &HourKey) -> String {
    format!("{} {}", hour.run, hour.time_label())
}

fn geographic_bounds(lat: &[f32], lon: &[f32]) -> Option<(f64, f64, f64, f64)> {
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut west = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    for (&lat, &lon) in lat.iter().zip(lon) {
        let lat = f64::from(lat);
        let lon = normalize_lon(f64::from(lon));
        if !lat.is_finite() || !lon.is_finite() {
            continue;
        }
        south = south.min(lat);
        north = north.max(lat);
        west = west.min(lon);
        east = east.max(lon);
    }
    if south.is_finite() && north.is_finite() && west.is_finite() && east.is_finite() {
        Some((west, east, south, north))
    } else {
        None
    }
}

fn normalize_lon(lon: f64) -> f64 {
    ((lon + 180.0).rem_euclid(360.0)) - 180.0
}

fn quantized_dimension(value: f32, min: u32, max: u32) -> u32 {
    let rounded = ((value.max(min as f32).min(max as f32)) / 32.0).round() as u32 * 32;
    rounded.max(min).min(max)
}

fn fitted_plot_size(available: Vec2, aspect: f32) -> Vec2 {
    let aspect = aspect.clamp(0.28, 3.4);
    let max_w = if available.x.is_finite() {
        available.x.clamp(1.0, 2200.0)
    } else {
        640.0
    };
    let max_h = if available.y.is_finite() {
        available.y.clamp(1.0, 1400.0)
    } else {
        360.0
    };
    let mut width = max_w;
    let mut height = width / aspect;
    if height > max_h {
        height = max_h;
        width = height * aspect;
    }
    Vec2::new(width.floor().max(1.0), height.floor().max(1.0))
}

fn render_plot_size(display_width: u32, display_height: u32, scale: f32) -> (u32, u32) {
    (
        quantized_dimension(display_width as f32 * scale, display_width, 4800),
        quantized_dimension(display_height as f32 * scale, display_height, 3200),
    )
}

fn domain_plot_aspect(bounds: (f64, f64, f64, f64), rotation_deg: f64) -> f32 {
    let lat_span = (bounds.3 - bounds.2).abs().max(0.01);
    let center_lat = ((bounds.2 + bounds.3) * 0.5).to_radians();
    let lon_span = longitude_span(bounds.0, bounds.1).max(0.01);
    let width = lon_span * center_lat.cos().abs().max(0.15);
    let height = lat_span;
    let rotation = normalize_rotation(rotation_deg).to_radians().abs();
    let rotated_width = width * rotation.cos().abs() + height * rotation.sin().abs();
    let rotated_height = width * rotation.sin().abs() + height * rotation.cos().abs();
    (rotated_width / rotated_height.max(0.01)) as f32
}

fn rotated_domain_needs_basemap_padding(rotation_deg: f64) -> bool {
    normalize_rotation(rotation_deg).abs() >= 0.05
}

fn rgba_to_color_image(image: &RgbaImage) -> ColorImage {
    let size = [image.width() as usize, image.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, image.as_raw())
}

fn normalize_domain_bounds(bounds: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let west = normalize_lon(bounds.0);
    let east = normalize_lon(bounds.1);
    let south = bounds.2.min(bounds.3).clamp(-89.5, 89.5);
    let north = bounds.2.max(bounds.3).clamp(-89.5, 89.5);
    (west, east, south, north)
}

fn normalize_domain(mut domain: CustomDomain) -> CustomDomain {
    domain.bounds = normalize_domain_bounds(domain.bounds);
    domain.rotation_deg = normalize_rotation(domain.rotation_deg);
    domain.name = domain.name.trim().to_string();
    if domain.name.is_empty() {
        domain.name = default_domain_name(domain.bounds);
    }
    domain
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

fn normalize_saved_domains(domains: &mut Vec<CustomDomain>) {
    for domain in domains.iter_mut() {
        *domain = normalize_domain(domain.clone());
    }
    domains.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });
    domains.dedup_by(|a, b| a.name.eq_ignore_ascii_case(&b.name));
}

fn default_domain_name(bounds: (f64, f64, f64, f64)) -> String {
    let center_lon = midpoint_longitude(bounds.0, bounds.1);
    let center_lat = (bounds.2 + bounds.3) * 0.5;
    format!("domain {:.1} {:.1}", center_lat, center_lon)
}

fn midpoint_longitude(west: f64, east: f64) -> f64 {
    let west = normalize_lon(west);
    let mut east = normalize_lon(east);
    if east < west {
        east += 360.0;
    }
    normalize_lon((west + east) * 0.5)
}

fn longitude_span(west: f64, east: f64) -> f64 {
    let raw_span = (east - west).abs();
    if raw_span >= 359.0 {
        return raw_span.min(360.0);
    }
    let west = normalize_lon(west);
    let east = normalize_lon(east);
    if west <= east {
        east - west
    } else {
        east + 360.0 - west
    }
}

fn format_bounds(bounds: (f64, f64, f64, f64)) -> String {
    format!(
        "W {:.2} / E {:.2} / S {:.2} / N {:.2}",
        bounds.0, bounds.1, bounds.2, bounds.3
    )
}

fn format_domain_label(bounds: (f64, f64, f64, f64), rotation_deg: f64) -> String {
    let bounds = format_bounds(bounds);
    let rotation_deg = normalize_rotation(rotation_deg);
    if rotation_deg.abs() < 0.05 {
        bounds
    } else {
        format!("{bounds} / rot {rotation_deg:.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quantized_dimensions_are_stable_and_bounded() {
        assert_eq!(quantized_dimension(10.0, 640, 2200), 640);
        assert_eq!(quantized_dimension(657.0, 640, 2200), 672);
        assert_eq!(quantized_dimension(5000.0, 640, 2200), 2200);
    }

    #[test]
    fn render_plot_size_scales_preview_without_unbounded_textures() {
        assert_eq!(render_plot_size(960, 540, 1.0), (960, 544));
        assert_eq!(render_plot_size(960, 540, 2.0), (1920, 1088));
        assert_eq!(render_plot_size(2200, 1400, 3.0), (4800, 3200));
    }

    #[test]
    fn fitted_plot_plus_footer_never_requests_more_than_the_window_has() {
        let total = Vec2::new(520.0, 300.0);
        let footer_height = 24.0;
        let available = Vec2::new(total.x, total.y - footer_height);

        for aspect in [16.0 / 9.0, 1.0, 0.3, 3.4] {
            let fitted = fitted_plot_size(available, aspect);
            assert!(fitted.x > 0.0 && fitted.y > 0.0);
            assert!(fitted.x <= total.x);
            assert!(fitted.y + footer_height <= total.y);
        }
    }

    #[test]
    fn resize_render_waits_for_settle_and_never_renders_while_dragging() {
        let start = Instant::now();
        let mut debounce = ResizeRenderDebounce::default();

        assert_eq!(
            debounce.decide((960, 544), false, start),
            ResizeRenderDecision::Wait(RESIZE_SETTLE_TIME)
        );
        assert_eq!(
            debounce.decide((960, 544), false, start + Duration::from_millis(100)),
            ResizeRenderDecision::Wait(Duration::from_millis(80))
        );
        assert_eq!(
            debounce.decide((960, 544), false, start + RESIZE_SETTLE_TIME),
            ResizeRenderDecision::RenderNow
        );

        let drag_start = start + Duration::from_secs(1);
        assert_eq!(
            debounce.decide((1024, 576), true, drag_start),
            ResizeRenderDecision::Wait(ACTIVE_RESIZE_REPAINT)
        );
        assert_eq!(
            debounce.decide((1024, 576), true, drag_start + Duration::from_millis(500)),
            ResizeRenderDecision::Wait(ACTIVE_RESIZE_REPAINT)
        );
        assert_eq!(
            debounce.decide((1024, 576), false, drag_start + Duration::from_millis(500)),
            ResizeRenderDecision::RenderNow
        );
    }

    #[test]
    fn show_full_grid_clears_domain_name_and_render_state() {
        let mut panel = PlotViewerPanel::new();
        panel.set_active_domain(CustomDomain::new("old d03", (-100.0, -99.0, 35.0, 36.0)));
        panel.error = Some("stale domain render".to_string());
        panel.last_render_ms = Some(123.0);

        panel.show_full_grid();

        assert!(panel.active_domain().is_none());
        assert!(panel.domain_name_edit.is_empty());
        assert!(panel.error.is_none());
        assert!(panel.last_render_ms.is_none());
        assert!(panel.resize_render_debounce.pending.is_none());
    }

    #[test]
    fn custom_domain_cache_key_quantizes_float_noise() {
        let a = CustomDomain::new("x", (-100.0000001, -90.0, 30.0, 40.0));
        let b = CustomDomain::new("x", (-100.0000002, -90.0, 30.0, 40.0));
        let a_key = DomainCacheKey::from(&a);
        let b_key = DomainCacheKey::from(&b);
        assert_eq!(a_key, b_key);
    }

    #[test]
    fn custom_domain_aspect_tracks_selected_bounds() {
        let wide = domain_plot_aspect((-125.0, -100.0, 35.0, 45.0), 0.0);
        let tall = domain_plot_aspect((-125.0, -120.0, 30.0, 50.0), 0.0);
        assert!(wide > tall);
    }

    #[test]
    fn custom_domain_rotation_is_part_of_the_cache_key() {
        let a = CustomDomain::new("x", (-125.0, -120.0, 35.0, 40.0)).with_rotation(0.0);
        let b = CustomDomain::new("x", (-125.0, -120.0, 35.0, 40.0)).with_rotation(15.0);
        assert_ne!(DomainCacheKey::from(&a), DomainCacheKey::from(&b));
    }

    #[test]
    fn geographic_bounds_normalize_longitudes() {
        let bounds = geographic_bounds(&[30.0, 40.0], &[240.0, 250.0]).unwrap();
        assert_eq!(bounds, (-120.0, -110.0, 30.0, 40.0));
    }

    #[test]
    fn exact_plot_subtitle_uses_physical_time_not_storage_slot() {
        let subtitle = plot_time_subtitle(&HourKey {
            model: "wrf".to_string(),
            run: "exact-run".to_string(),
            hour: 0,
            exact_time: Some(rw_store::RwsExactTime {
                lead_seconds: 31_680,
                valid_unix: 134_243_280,
            }),
        });
        assert!(subtitle.contains("+08:48:00"));
        assert!(subtitle.contains("1974-04-03 17:48:00Z"));
        assert!(!subtitle.contains("f000"));
    }

    #[test]
    fn native_plot_settings_are_staged_until_explicit_rerender() {
        let mut panel = PlotViewerPanel::new();
        assert!(panel.settings().show_states);
        panel.draft_settings.show_states = false;
        panel.draft_settings.line_width_percent = 175;

        assert!(panel.has_pending_settings());
        assert!(panel.settings().show_states);

        panel.rerender();

        assert!(!panel.has_pending_settings());
        assert!(!panel.settings().show_states);
        assert_eq!(panel.settings().line_width_percent, 175);
        assert!(panel.take_settings_changed());
        assert!(!panel.take_settings_changed());
    }

    #[test]
    fn native_plot_settings_json_round_trip_and_clamp_numeric_fields() {
        let mut settings = NativePlotSettings {
            map_detail: NativePlotMapDetail::Broad,
            show_states: false,
            line_width_percent: 900,
            line_opacity_percent: 900,
            sampling: NativePlotSampling::PixelExact,
            ..NativePlotSettings::default()
        };
        let value = serde_json::to_value(&settings).unwrap();
        let mut panel = PlotViewerPanel::new();

        assert!(panel.apply_settings_json(&value));
        settings.line_width_percent = 300;
        settings.line_opacity_percent = 200;
        assert_eq!(panel.settings(), &settings);
        assert_eq!(
            panel.settings_json(),
            serde_json::to_value(settings).unwrap()
        );
    }

    #[test]
    fn native_map_layer_controls_filter_and_scale_projected_linework() {
        fn line(role: LineworkRole) -> rustwx_render::ProjectedLineOverlay {
            rustwx_render::ProjectedLineOverlay {
                points: vec![(0.0, 0.0), (1.0, 1.0)],
                color: rustwx_render::Color::rgba(10, 20, 30, 100),
                width: 2,
                role,
            }
        }

        let mut projected = rustwx_render::ProjectedMap {
            projected_x: vec![0.0],
            projected_y: vec![0.0],
            extent: rustwx_render::ProjectedExtent {
                x_min: 0.0,
                x_max: 1.0,
                y_min: 0.0,
                y_max: 1.0,
            },
            lines: vec![
                line(LineworkRole::Coast),
                line(LineworkRole::State),
                line(LineworkRole::County),
                line(LineworkRole::Lake),
            ],
            polygons: vec![
                rustwx_render::ProjectedPolygonFill {
                    rings: vec![],
                    color: rustwx_render::Color::BLACK,
                    role: PolygonRole::Land,
                },
                rustwx_render::ProjectedPolygonFill {
                    rings: vec![],
                    color: rustwx_render::Color::BLACK,
                    role: PolygonRole::Lake,
                },
            ],
            inverse_raster_projection: None,
        };
        let settings = NativePlotSettings {
            show_states: false,
            show_counties: false,
            show_lakes: false,
            line_width_percent: 150,
            line_opacity_percent: 50,
            ..NativePlotSettings::default()
        };

        apply_native_map_settings(&mut projected, &settings);

        assert_eq!(projected.lines.len(), 1);
        assert_eq!(projected.lines[0].role, LineworkRole::Generic);
        assert_eq!(projected.lines[0].width, 3);
        assert_eq!(projected.lines[0].color.a, 50);
        assert_eq!(projected.polygons.len(), 1);
        assert_eq!(projected.polygons[0].role, PolygonRole::Land);
    }

    #[test]
    fn complete_settings_snapshot_participates_in_plot_cache_identity() {
        let field = FieldKey {
            hour: HourKey {
                model: "gfs".to_string(),
                run: "2026080200".to_string(),
                hour: 0,
                exact_time: None,
            },
            var: "tmp2m".to_string(),
        };
        let mut changed = NativePlotSettings::default();
        changed.show_states = false;
        let base = PlotCacheKey {
            field: field.clone(),
            width: 800,
            height: 600,
            domain: None,
            settings: NativePlotSettings::default(),
        };
        let changed = PlotCacheKey {
            field,
            width: 800,
            height: 600,
            domain: None,
            settings: changed,
        };

        assert_ne!(base, changed);
    }
}

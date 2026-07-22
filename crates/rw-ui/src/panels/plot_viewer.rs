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
use rustwx_render::{DomainFrame, MapRenderRequest, ProductVisualMode, RgbaImage};
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

#[derive(Default)]
pub struct PlotViewerPanel {
    texture: Option<TextureHandle>,
    cache_key: Option<PlotCacheKey>,
    error: Option<String>,
    last_render_ms: Option<f32>,
    last_upload_ms: Option<f32>,
    quality_scale: f32,
    active_domain: Option<CustomDomain>,
    saved_domains: Vec<CustomDomain>,
    domain_name_edit: String,
    saved_domains_dirty: bool,
    resize_render_debounce: ResizeRenderDebounce,
}

impl PlotViewerPanel {
    pub fn new() -> Self {
        Self {
            quality_scale: 1.5,
            ..Self::default()
        }
    }

    pub fn clear(&mut self) {
        self.texture = None;
        self.cache_key = None;
        self.error = None;
        self.last_render_ms = None;
        self.last_upload_ms = None;
        self.resize_render_debounce.clear();
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
                self.effective_quality_scale(),
            );
            let key = PlotCacheKey {
                field: field.key.clone(),
                width,
                height,
                domain: self.active_domain.as_ref().map(DomainCacheKey::from),
            };

            if self.cache_key.as_ref() != Some(&key) {
                let content_changed = self
                    .cache_key
                    .as_ref()
                    .is_none_or(|cached| cached.field != key.field || cached.domain != key.domain);
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
            ui.separator();
            ui.label(RichText::new("Resolution").small().strong());
            for (label, scale) in [("1x", 1.0), ("1.5x", 1.5), ("2x", 2.0), ("3x", 3.0)] {
                if ui
                    .selectable_label((self.effective_quality_scale() - scale).abs() < 0.01, label)
                    .on_hover_text("native plot render scale")
                    .clicked()
                {
                    self.quality_scale = scale;
                    self.clear();
                }
            }
        });
    }

    fn effective_quality_scale(&self) -> f32 {
        if self.quality_scale.is_finite() && self.quality_scale >= 0.95 {
            self.quality_scale.clamp(1.0, 3.0)
        } else {
            1.5
        }
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
        let image = match render_field_plot(field, key.width, key.height, domain.as_ref()) {
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
        self.texture = Some(ui.ctx().load_texture(
            "rw-ui-native-plot",
            rgba_to_color_image(&image),
            TextureOptions {
                magnification: TextureFilter::Linear,
                minification: TextureFilter::Linear,
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
    let projected = if let Some(domain) = domain {
        if rotated_domain_needs_basemap_padding(domain.rotation_deg) {
            rustwx_products::direct::build_natural_projected_map_with_projection_and_basemap_padding(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                width as f64 / height as f64,
                1.35,
                1.10,
            )
        } else {
            rustwx_products::direct::build_natural_projected_map_with_projection(
                &grid_file.lat,
                &grid_file.lon,
                grid_file.projection.as_ref(),
                render_bounds,
                width as f64 / height as f64,
            )
        }
    } else {
        rustwx_products::direct::build_projected_map_with_projection(
            &grid_file.lat,
            &grid_file.lon,
            grid_file.projection.as_ref(),
            render_bounds,
            width as f64 / height as f64,
        )
    }
    .map_err(|err| err.to_string())?;
    let projected = match domain {
        Some(domain) => projected.rotated_degrees(domain.rotation_deg),
        None => projected,
    };

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

    rustwx_render::render_image(&request).map_err(|err| err.to_string())
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
}

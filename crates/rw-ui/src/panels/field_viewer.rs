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

use egui::{
    Align2, Color32, ColorImage, ComboBox, FontId, Image, PointerButton, Pos2, Rect, RichText,
    Sense, Stroke, StrokeKind, TextureFilter, TextureHandle, TextureOptions, Ui, Vec2, pos2,
};
use rustwx_render::{
    LeveledColormap, build_colormap, colorbar_ticks, format_tick, legend_color_at_rel,
    legend_tick_rel,
};

use crate::colormap::{Colormap, VIRIDIS, field_to_color_image, field_to_production_color_image};
use crate::profile_scope;
use crate::worker::{FieldData, FieldKey, HourKey, VarInfo, VarKind};

use super::plot_viewer::CustomDomain;

/// Horizontal room reserved for the production legend (bar + ticks + labels).
const LEGEND_WIDTH: f32 = 78.0;
/// Vertical resolution of the legend bar texture (one production colorbar
/// sample per row, matching `draw_vertical_colorbar`'s per-pixel sampling).
const LEGEND_BAR_RESOLUTION: usize = 512;

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

/// False-color 2D field inspector. Pure widget over host-pushed data:
/// `set_hour` -> `set_loading` -> `set_field`/`set_error`, render with `ui`.
#[derive(Default)]
pub struct FieldViewerPanel {
    hour: Option<HourKey>,
    vars: Vec<VarInfo>,
    selected_var: Option<String>,
    field: Option<FieldData>,
    texture: Option<TextureHandle>,
    texture_dirty: bool,
    state: LoadState,
    /// Last clicked point in fractional grid coords (marker overlay).
    clicked: Option<(f64, f64)>,
    /// In-progress shift + secondary-button custom-domain selection.
    domain_drag: Option<DomainDrag>,
    /// In-progress shift + secondary-button corner rotation.
    domain_rotate: Option<DomainRotate>,
    /// Last completed custom-domain selection in grid coordinates.
    domain_selection: Option<DomainSelection>,
    colormap: Colormap,
    /// The production colormap for the loaded field (None = generic ramp).
    cmap: Option<LeveledColormap>,
    legend_texture: Option<TextureHandle>,
    /// Wall time of the last colormap + texture-upload pass, for the
    /// always-on stats strip.
    last_texture_ms: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DomainDrag {
    start: Pos2,
    current: Pos2,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct DomainRotate {
    center: Pos2,
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

impl FieldViewerPanel {
    pub fn new() -> Self {
        Self {
            colormap: VIRIDIS,
            ..Self::default()
        }
    }

    /// Install a new hour's variable list. Keeps the current variable
    /// selection when the new hour still has it; otherwise falls back to
    /// `temperature_2m`, then the first 2D variable. The host should then
    /// fire a load for [`FieldViewerPanel::selected_var`].
    pub fn set_hour(&mut self, hour: HourKey, vars: Vec<VarInfo>) {
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
        self.field = None;
        self.texture = None;
        self.texture_dirty = false;
        self.clicked = None;
        self.domain_drag = None;
        self.domain_rotate = None;
        self.domain_selection = None;
        self.cmap = None;
        self.legend_texture = None;
        self.state = LoadState::Idle;
    }

    pub fn hour(&self) -> Option<&HourKey> {
        self.hour.as_ref()
    }

    pub fn selected_var(&self) -> Option<&str> {
        self.selected_var.as_deref()
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
        self.field = Some(data);
        self.texture_dirty = true;
        self.state = LoadState::Ready;
    }

    pub fn clear(&mut self) {
        *self = Self {
            colormap: self.colormap,
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

        ui.horizontal_wrapped(|ui| {
            let previous = self.selected_var.clone();
            let mut current = previous.clone().unwrap_or_default();
            ComboBox::from_id_salt("rw-ui-field-var")
                .selected_text(if current.is_empty() {
                    "pick a variable"
                } else {
                    &current
                })
                .width(220.0)
                .show_ui(ui, |ui| {
                    for var in self.vars.iter().filter(|v| v.kind == VarKind::Surface2D) {
                        ui.selectable_value(
                            &mut current,
                            var.name.clone(),
                            format!("{} ({})", var.name, var.units),
                        );
                    }
                });
            if !current.is_empty() && Some(&current) != previous.as_ref() {
                self.selected_var = Some(current.clone());
                self.field = None;
                self.texture = None;
                self.texture_dirty = false;
                self.clicked = None;
                self.domain_drag = None;
                self.domain_rotate = None;
                self.domain_selection = None;
                self.cmap = None;
                self.legend_texture = None;
                event = Some(FieldViewerEvent::VarSelected(current));
            }

            if let Some(field) = &self.field {
                let range = match field.range {
                    Some((lo, hi)) => format!("{lo:.2} .. {hi:.2} {}", field.units),
                    None => "all values missing".to_string(),
                };
                ui.label(RichText::new(range).small().weak());
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
                TextureOptions {
                    magnification: TextureFilter::Linear,
                    minification: TextureFilter::Linear,
                    ..Default::default()
                },
            ));
            self.last_texture_ms = Some(texture_started.elapsed().as_secs_f32() * 1000.0);
            self.texture_dirty = false;
        }
        let Some(texture) = &self.texture else {
            return event;
        };

        // Fit the grid into the remaining space, preserving aspect; mapped
        // fields reserve a strip on the right for the production legend.
        let legend_width = if self.cmap.is_some() {
            LEGEND_WIDTH
        } else {
            0.0
        };
        let mut avail = ui.available_size();
        avail.x = (avail.x - legend_width).max(1.0);
        let scale = (avail.x / field.nx as f32)
            .min(avail.y / field.ny as f32)
            .max(0.01);
        let size = Vec2::new(field.nx as f32 * scale, field.ny as f32 * scale);
        let response = ui.add(
            Image::new(texture)
                .fit_to_exact_size(size)
                .sense(Sense::click_and_drag()),
        );
        let rect = response.rect;

        // Pointer position -> fractional grid coordinates (texel centers at
        // integer coords). MUST invert the exact display transform above:
        // same `flip_y`, or clicks/hovers would sample a north/south-mirrored
        // location.
        let to_grid = |pos: egui::Pos2| -> (f64, f64) {
            let u = ((pos.x - rect.left()) / rect.width()) as f64;
            let v = ((pos.y - rect.top()) / rect.height()) as f64;
            image_uv_to_grid(u, v, field.nx, field.ny, flip_y)
        };

        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let (fx, fy) = to_grid(pos);
                self.clicked = Some((fx, fy));
                event = Some(FieldViewerEvent::PointClicked { fx, fy });
            }
        }

        let shift_down = ui.input(|input| input.modifiers.shift);
        if response.drag_started_by(PointerButton::Secondary) && shift_down {
            if let Some(pos) = response.interact_pointer_pos() {
                let pos = clamp_pos_to_rect(pos, rect);
                let mut started_rotation = false;
                if let Some(selection) = self.domain_selection {
                    if let Some((center, corners)) =
                        domain_selection_geometry(rect, field.nx, field.ny, flip_y, selection)
                    {
                        if pointer_near_selection_corner(pos, corners) {
                            self.domain_rotate = Some(DomainRotate {
                                center,
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
                    });
                    self.domain_rotate = None;
                }
            }
        }
        let mut finished_rotation = None;
        if let Some(rotate) = self.domain_rotate.as_mut() {
            if response.dragged_by(PointerButton::Secondary) {
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
            if response.drag_stopped_by(PointerButton::Secondary) {
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
            if response.dragged_by(PointerButton::Secondary) {
                if let Some(pos) = response.interact_pointer_pos() {
                    drag.current = clamp_pos_to_rect(pos, rect);
                }
            }
            if response.drag_stopped_by(PointerButton::Secondary) {
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
            let (u, v) = grid_to_image_uv(fx, fy, field.nx, field.ny, flip_y);
            let px = rect.left() + u as f32 * rect.width();
            let py = rect.top() + v as f32 * rect.height();
            let painter = ui.painter_at(rect);
            painter.circle_stroke(pos2(px, py), 5.0, Stroke::new(2.0, Color32::WHITE));
            painter.circle_stroke(pos2(px, py), 6.5, Stroke::new(1.0, Color32::BLACK));
        }
        if let Some(selection) = self.domain_selection {
            draw_domain_selection(ui, rect, field.nx, field.ny, flip_y, selection);
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
            let text = if value.is_nan() {
                format!("({ix}, {iy}){place}  missing")
            } else {
                format!("({ix}, {iy}){place}  {value:.2} {}", field.units)
            };
            response.on_hover_text_at_pointer(text);
        }

        event
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

/// Normalized image coords (`u` rightward, `v` DOWNWARD from the image top,
/// both in `[0, 1]`) -> fractional grid coords, with texel centers at
/// integer coords. `flip_y` must be the SAME flag the texture build used —
/// this is the inverse of the display transform, so clicks and hovers sample
/// the grid point actually under the pointer.
fn image_uv_to_grid(u: f64, v: f64, nx: usize, ny: usize, flip_y: bool) -> (f64, f64) {
    let row = if flip_y { 1.0 - v } else { v };
    let fx = (u * nx as f64 - 0.5).clamp(0.0, (nx - 1) as f64);
    let fy = (row * ny as f64 - 0.5).clamp(0.0, (ny - 1) as f64);
    (fx, fy)
}

/// Fractional grid coords -> normalized image coords; exact inverse of
/// [`image_uv_to_grid`] away from the clamped border (marker overlay).
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
    selection: DomainSelection,
) {
    let Some((_center, corners)) = domain_selection_geometry(image_rect, nx, ny, flip_y, selection)
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
    selection: DomainSelection,
) -> Option<(Pos2, [Pos2; 4])> {
    let (u0, v0) = grid_to_image_uv(selection.fx0, selection.fy0, nx, ny, flip_y);
    let (u1, v1) = grid_to_image_uv(selection.fx1, selection.fy1, nx, ny, flip_y);
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

    const NX: usize = 8;
    const NY: usize = 6;

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

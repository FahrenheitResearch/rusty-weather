//! Field viewer: pick a 2D variable and inspect it as a false-color texture.
//!
//! This is a DATA VIEWER — a linear min..max ramp for eyeballing stored
//! grids — not the production plot renderer, and it says so in its header.
//! The texture is cached and re-uploaded only when the loaded field changes;
//! per-frame work is just drawing the cached texture.

use egui::{
    Color32, ComboBox, Image, Rect, RichText, Sense, Stroke, StrokeKind, TextureFilter,
    TextureHandle, TextureOptions, Ui, Vec2, pos2,
};

use crate::colormap::{Colormap, VIRIDIS, field_to_color_image};
use crate::worker::{FieldData, FieldKey, HourKey, VarInfo, VarKind};

/// What the user did this frame; the host turns these into worker requests.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldViewerEvent {
    /// A different 2D variable was picked.
    VarSelected(String),
    /// The field was clicked at fractional grid coordinates.
    PointClicked { fx: f64, fy: f64 },
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
    colormap: Colormap,
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
        let keep = self
            .selected_var
            .take()
            .filter(|name| vars.iter().any(|v| v.kind == VarKind::Surface2D && v.name == *name));
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

    /// Render the variable picker + field image. Returns at most one event.
    pub fn ui(&mut self, ui: &mut Ui) -> Option<FieldViewerEvent> {
        let mut event = None;

        ui.horizontal_wrapped(|ui| {
            let previous = self.selected_var.clone();
            let mut current = previous.clone().unwrap_or_default();
            ComboBox::from_id_salt("rw-ui-field-var")
                .selected_text(if current.is_empty() { "pick a variable" } else { &current })
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
        ui.label(
            RichText::new("DATA VIEWER — linear min..max false color, not the production renderer")
                .small()
                .weak(),
        );
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

        // (Re-)upload the texture only when the loaded field changed.
        if self.texture_dirty {
            let (vmin, vmax) = field.range.unwrap_or((0.0, 0.0));
            let image = field_to_color_image(
                &field.values,
                field.nx,
                field.ny,
                vmin,
                vmax,
                &self.colormap,
                true, // grid row 0 (south) at the image bottom
            );
            self.texture = Some(ui.ctx().load_texture(
                "rw-ui-field",
                image,
                TextureOptions {
                    magnification: TextureFilter::Nearest,
                    minification: TextureFilter::Linear,
                    ..Default::default()
                },
            ));
            self.texture_dirty = false;
        }
        let Some(texture) = &self.texture else {
            return event;
        };

        // Fit the grid into the remaining space, preserving aspect.
        let avail = ui.available_size();
        let scale = (avail.x / field.nx as f32)
            .min(avail.y / field.ny as f32)
            .max(0.01);
        let size = Vec2::new(field.nx as f32 * scale, field.ny as f32 * scale);
        let response = ui.add(
            Image::new(texture)
                .fit_to_exact_size(size)
                .sense(Sense::click()),
        );
        let rect = response.rect;

        // Pointer position -> fractional grid coordinates (texel centers at
        // integer coords; image y is flipped relative to the grid).
        let to_grid = |pos: egui::Pos2| -> (f64, f64) {
            let u = ((pos.x - rect.left()) / rect.width()) as f64;
            let v = ((pos.y - rect.top()) / rect.height()) as f64;
            let fx = (u * field.nx as f64 - 0.5).clamp(0.0, (field.nx - 1) as f64);
            let fy = ((1.0 - v) * field.ny as f64 - 0.5).clamp(0.0, (field.ny - 1) as f64);
            (fx, fy)
        };

        if response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let (fx, fy) = to_grid(pos);
                self.clicked = Some((fx, fy));
                event = Some(FieldViewerEvent::PointClicked { fx, fy });
            }
        }

        // Marker on the last clicked point.
        if let Some((fx, fy)) = self.clicked {
            let px = rect.left() + ((fx + 0.5) / field.nx as f64) as f32 * rect.width();
            let py = rect.top() + (1.0 - (fy + 0.5) / field.ny as f64) as f32 * rect.height();
            let painter = ui.painter_at(rect);
            painter.circle_stroke(pos2(px, py), 5.0, Stroke::new(2.0, Color32::WHITE));
            painter.circle_stroke(pos2(px, py), 6.5, Stroke::new(1.0, Color32::BLACK));
        }
        ui.painter_at(rect.expand(1.0)).rect_stroke(
            Rect::from_min_max(rect.min, rect.max),
            0.0,
            Stroke::new(1.0, ui.visuals().weak_text_color()),
            StrokeKind::Outside,
        );

        // Hover readout: grid point + value.
        if let Some(pos) = response.hover_pos() {
            let (fx, fy) = to_grid(pos);
            let ix = fx.round() as usize;
            let iy = fy.round() as usize;
            let value = field.values[iy * field.nx + ix];
            let text = if value.is_nan() {
                format!("({ix}, {iy})  missing")
            } else {
                format!("({ix}, {iy})  {value:.2} {}", field.units)
            };
            response.on_hover_text_at_pointer(text);
        }

        event
    }
}

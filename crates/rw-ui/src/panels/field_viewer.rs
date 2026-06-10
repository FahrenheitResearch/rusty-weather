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

        // Display flip is DERIVED from the grid lat axis, never assumed:
        // south-to-north storage (row 0 south) flips so the image is always
        // north-at-top; north-to-south storage renders rows as stored.
        let flip_y = !field.lat_descending;

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
                flip_y,
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

        // Marker on the last clicked point (forward display transform).
        if let Some((fx, fy)) = self.clicked {
            let (u, v) = grid_to_image_uv(fx, fy, field.nx, field.ny, flip_y);
            let px = rect.left() + u as f32 * rect.width();
            let py = rect.top() + v as f32 * rect.height();
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
                let lat = if descending { 50.0 - y as f32 } else { 20.0 + y as f32 };
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
            let south = if descending { 50.0 - (NY - 1) as f32 } else { 20.0 };
            let north = if descending { 50.0 } else { 20.0 + (NY - 1) as f32 };

            // v = 1 is the bottom edge of the image (screen-down).
            let (_, fy) = image_uv_to_grid(0.5, 0.999, NX, NY, flip_y);
            let iy = fy.round() as usize;
            assert_eq!(
                lat[iy * NX], south,
                "bottom click must sample the southernmost row (descending={descending})"
            );

            let (_, fy) = image_uv_to_grid(0.5, 0.001, NX, NY, flip_y);
            let iy = fy.round() as usize;
            assert_eq!(
                lat[iy * NX], north,
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
}

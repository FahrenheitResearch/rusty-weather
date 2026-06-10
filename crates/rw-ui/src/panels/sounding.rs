//! Sounding panel: the production skew-T at a clicked point.
//!
//! The primary view is the rendered `rustwx-sounding` sounding (sharprs
//! skew-T + hodograph + native parameter table with ecape-rs-verified
//! values), built and rasterized ONCE per click in [`SoundingPanel::
//! set_data`] and drawn as a cached texture. The raw per-level numbers stay
//! available in a collapsible table below it. When an hour lacks the skew-T
//! inputs (see [`crate::skewt`]), the panel says why and the table remains.

use egui::{
    CollapsingHeader, ColorImage, Image, RichText, TextureFilter, TextureHandle, TextureOptions,
    Ui, Vec2,
};

use crate::skewt::{build_native_sounding, render_sounding_image};
use crate::worker::SoundingData;

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
    /// Rendered skew-T, or why it could not be built for this hour/point.
    skewt: Result<SkewtImage, String>,
}

struct SkewtImage {
    /// Kept until the first `ui` call uploads it.
    pending: Option<ColorImage>,
    texture: Option<TextureHandle>,
    aspect: f32,
    render_ms: f32,
    upload_ms: f32,
}

/// Point-sounding inspector. Pure widget over host-pushed data:
/// `set_loading` -> `set_data`/`set_error`, render with `ui`.
#[derive(Default)]
pub struct SoundingPanel {
    state: SoundingState,
}

impl SoundingPanel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_loading(&mut self) {
        self.state = SoundingState::Loading;
    }

    pub fn set_error(&mut self, message: String) {
        self.state = SoundingState::Error(message);
    }

    /// Install a loaded sounding. Builds the native sounding and renders
    /// the skew-T image here — once per click, not per frame. (The GPU
    /// upload happens on the next `ui` call, which has the `Context`.)
    pub fn set_data(&mut self, data: SoundingData) {
        let render_start = std::time::Instant::now();
        let skewt = build_native_sounding(&data)
            .and_then(|native| render_sounding_image(&native))
            .map(|image| SkewtImage {
                aspect: image.width() as f32 / image.height().max(1) as f32,
                pending: Some(image),
                texture: None,
                render_ms: render_start.elapsed().as_secs_f32() * 1000.0,
                upload_ms: 0.0,
            });
        self.state = SoundingState::Ready(Box::new(ReadySounding { data, skewt }));
    }

    pub fn clear(&mut self) {
        self.state = SoundingState::Empty;
    }

    /// Whether the panel has anything to show (host can hide it otherwise).
    pub fn has_content(&self) -> bool {
        !matches!(self.state, SoundingState::Empty)
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
            SoundingState::Ready(ready) => show_sounding(ui, ready),
        }
    }
}

fn show_sounding(ui: &mut Ui, ready: &mut ReadySounding) {
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
            match &mut ready.skewt {
                Ok(skewt) => {
                    show_skewt(ui, skewt);
                    ui.label(
                        RichText::new(format!(
                            "profile read {:.0} ms · render {:.0} ms · upload {:.0} ms",
                            data.read_ms, skewt.render_ms, skewt.upload_ms
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
                .default_open(ready.skewt.is_err())
                .show(ui, |ui| show_level_table(ui, data));
        });
}

/// Upload the texture on the first frame after a click, then draw it
/// aspect-correct at the available width.
fn show_skewt(ui: &mut Ui, skewt: &mut SkewtImage) {
    if let Some(image) = skewt.pending.take() {
        let upload_start = std::time::Instant::now();
        skewt.texture = Some(ui.ctx().load_texture(
            "rw-ui-sounding-skewt",
            image,
            TextureOptions {
                magnification: TextureFilter::Linear,
                minification: TextureFilter::Linear,
                ..Default::default()
            },
        ));
        skewt.upload_ms = upload_start.elapsed().as_secs_f32() * 1000.0;
    }
    let Some(texture) = &skewt.texture else {
        return;
    };
    let width = ui.available_width().max(64.0);
    let size = Vec2::new(width, width / skewt.aspect);
    ui.add(Image::new(texture).fit_to_exact_size(size));
}

/// Numeric table: rows = union of levels (descending pressure), one column
/// per 3D variable. Raw store values and units — no conversions.
fn show_level_table(ui: &mut Ui, data: &SoundingData) {
    if data.vars.is_empty() {
        ui.label(RichText::new("This hour has no 3D pressure-level variables.").weak());
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
                    ui.label(RichText::new("hPa").strong());
                    for var in &data.vars {
                        ui.label(RichText::new(&var.name).strong())
                            .on_hover_text(format!("units: {}", var.units));
                    }
                    ui.end_row();
                    for &level in &levels {
                        ui.label(format!("{level}"));
                        for var in &data.vars {
                            let value = var
                                .levels_hpa
                                .iter()
                                .position(|&have| have == level)
                                .map(|i| var.values[i]);
                            match value {
                                Some(v) if v.is_finite() => ui.label(format!("{v:.1}")),
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
            SoundingState::Ready(ready) => match &ready.skewt {
                Err(err) => assert!(err.contains("temperature_iso"), "got: {err}"),
                Ok(_) => panic!("no inputs must not produce a skew-T"),
            },
            _ => panic!("set_data must land in Ready"),
        }
    }
}

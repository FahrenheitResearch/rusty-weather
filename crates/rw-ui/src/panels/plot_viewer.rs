//! Native map plot viewer: render the selected store field through
//! `rustwx-render` as an RGBA image and upload it directly to egui.
//!
//! This is the interactive sibling of PNG export: the expensive PNG encode
//! path is intentionally skipped for on-screen display.

use egui::{ColorImage, Image, RichText, TextureFilter, TextureHandle, TextureOptions, Ui, Vec2};
use rustwx_core::{Field2D, GridShape, LatLonGrid, ProductKey};
use rustwx_render::{MapRenderRequest, ProductVisualMode, RgbaImage};

use crate::profile_scope;
use crate::worker::{FieldData, FieldKey};

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlotCacheKey {
    field: FieldKey,
    width: u32,
    height: u32,
}

#[derive(Default)]
pub struct PlotViewerPanel {
    texture: Option<TextureHandle>,
    cache_key: Option<PlotCacheKey>,
    error: Option<String>,
    last_render_ms: Option<f32>,
    last_upload_ms: Option<f32>,
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
    }

    pub fn last_timings(&self) -> Option<(f32, f32)> {
        Some((self.last_render_ms?, self.last_upload_ms.unwrap_or(0.0)))
    }

    pub fn ui(&mut self, ui: &mut Ui, field: Option<&FieldData>) {
        ui.vertical(|ui| {
            let Some(field) = field else {
                self.clear();
                ui.label(RichText::new("Load a field to render a native plot.").weak());
                return;
            };

            let available = ui.available_size();
            let width = quantized_dimension(available.x, 640, 2200);
            let height = quantized_dimension(
                (width as f32 * 0.5625).min(available.y.max(360.0)),
                360,
                1400,
            );
            let key = PlotCacheKey {
                field: field.key.clone(),
                width,
                height,
            };

            if self.cache_key.as_ref() != Some(&key) {
                self.render(ui, field, key);
            }

            if let Some(message) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, message);
                return;
            }

            let Some(texture) = &self.texture else {
                ui.spinner();
                return;
            };

            ui.add(Image::new(texture).fit_to_exact_size(Vec2::new(width as f32, height as f32)));
            if let Some((render_ms, upload_ms)) = self.last_timings() {
                ui.label(
                    RichText::new(format!(
                        "native plot render {:.0} ms / upload {:.0} ms",
                        render_ms, upload_ms
                    ))
                    .small()
                    .weak(),
                );
            }
        });
    }

    fn render(&mut self, ui: &Ui, field: &FieldData, key: PlotCacheKey) {
        profile_scope!("native_plot_render");
        self.texture = None;
        self.error = None;
        self.cache_key = Some(key.clone());

        let render_start = std::time::Instant::now();
        let image = match render_field_plot(field, key.width, key.height) {
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

fn render_field_plot(field: &FieldData, width: u32, height: u32) -> Result<RgbaImage, String> {
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
    let projected = rustwx_products::direct::build_projected_map_with_projection(
        &grid_file.lat,
        &grid_file.lon,
        grid_file.projection.as_ref(),
        bounds,
        width as f64 / height as f64,
    )
    .map_err(|err| err.to_string())?;

    let mut request = MapRenderRequest::from_core_field(core_field, style.scale.clone());
    rustwx_products::plot_design::StaticPlotDesign::new(
        bounds,
        ProductVisualMode::FilledMeteorology,
    )
    .apply_to_request(&mut request);
    request.apply_projected_map(&projected);
    request.title = Some(style.title.clone());
    request.subtitle_left = Some(format!(
        "{} f{:03}",
        field.key.hour.run, field.key.hour.hour
    ));
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

fn rgba_to_color_image(image: &RgbaImage) -> ColorImage {
    let size = [image.width() as usize, image.height() as usize];
    ColorImage::from_rgba_unmultiplied(size, image.as_raw())
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
    fn geographic_bounds_normalize_longitudes() {
        let bounds = geographic_bounds(&[30.0, 40.0], &[240.0, 250.0]).unwrap();
        assert_eq!(bounds, (-120.0, -110.0, 30.0, 40.0));
    }
}

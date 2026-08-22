//! Windowed native-source rendering for MapLibre/Mapbox XYZ tiles.
//!
//! Each requested tile is projected directly into each ABI channel's fixed
//! grid.  Only the minimal NetCDF/HDF5 row/column window intersecting that
//! tile is decoded, so Full Disk C02 never becomes a whole-plane allocation.

use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::path::Path;

use image::ImageEncoder;

use crate::abi::{GoesAbiField, GoesAbiScene, read_goes_abi_field_window, read_goes_abi_scene};
use crate::archive::resolve_native_frame;
use crate::composite::{TRANSPARENT, bilinear_f32, bracket_axis};
use crate::geostationary::lat_lon_to_scan_angles_fast;
use crate::product::GoesAbiProduct;
use crate::product_render::{missing_band_error, render_product_pixel};
use crate::solar::solar_elevation_deg;

pub const DEFAULT_TILE_SIZE: u32 = 256;
pub const MAXIMUM_TILE_ZOOM: u8 = 14;

#[derive(Debug, Clone)]
pub struct NativeSatelliteTile {
    pub png: Vec<u8>,
    pub frame_id: String,
    pub valid_unix: i64,
    pub product: String,
    pub platform: String,
    pub sector: String,
}

pub fn render_native_xyz_tile(
    store_root: &Path,
    platform: &str,
    sector: &str,
    product: GoesAbiProduct,
    frame: &str,
    zoom: u8,
    tile_x: u32,
    tile_y: u32,
    tile_size: u32,
) -> Result<NativeSatelliteTile, Box<dyn Error>> {
    validate_tile(zoom, tile_x, tile_y, tile_size)?;
    let manifest = resolve_native_frame(store_root, platform, sector, product, frame)?;
    let coordinates = tile_coordinates(zoom, tile_x, tile_y, tile_size);
    let mut sampled = BTreeMap::<u8, Vec<f32>>::new();
    for &channel in product.required_channels() {
        let path = manifest.channel_path(store_root, channel)?;
        let source = read_goes_abi_scene(&path)?;
        let window = ChannelWindow::open(&path, source, &coordinates)?;
        sampled.insert(channel, window.sample_all(&coordinates));
    }

    let mut pixels = vec![0u8; coordinates.len() * 4];
    for (index, &(latitude, longitude)) in coordinates.iter().enumerate() {
        let color = if !(latitude.is_finite() && longitude.is_finite()) {
            TRANSPARENT
        } else if product.daylight_only()
            && solar_elevation_deg(
                manifest.scan_start_unix,
                f64::from(latitude),
                f64::from(longitude),
            )
            .is_none_or(|elevation| elevation <= -3.0)
        {
            TRANSPARENT
        } else {
            render_product_pixel(
                product,
                manifest.scan_start_unix,
                f64::from(latitude),
                f64::from(longitude),
                |channel| {
                    sampled
                        .get(&channel)
                        .and_then(|values| values.get(index))
                        .copied()
                        .ok_or_else(|| missing_band_error(channel))
                },
            )
            .unwrap_or(TRANSPARENT)
        };
        pixels[index * 4..index * 4 + 4].copy_from_slice(&color);
    }

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png).write_image(
        &pixels,
        tile_size,
        tile_size,
        image::ExtendedColorType::Rgba8,
    )?;
    Ok(NativeSatelliteTile {
        png,
        frame_id: manifest.frame_id,
        valid_unix: manifest.scan_start_unix,
        product: product.slug(),
        platform: manifest.platform,
        sector: manifest.sector,
    })
}

struct ChannelWindow {
    source_scene: GoesAbiScene,
    field: Option<GoesAbiField>,
    x_start: usize,
    y_start: usize,
}

impl ChannelWindow {
    fn open(
        path: &Path,
        source_scene: GoesAbiScene,
        coordinates: &[(f32, f32)],
    ) -> Result<Self, Box<dyn Error>> {
        let mut min_x = usize::MAX;
        let mut min_y = usize::MAX;
        let mut max_x = 0usize;
        let mut max_y = 0usize;
        let mut found = false;
        for &(latitude, longitude) in coordinates {
            let Some((x_scan, y_scan)) = scan_for_location(&source_scene, latitude, longitude)
            else {
                continue;
            };
            let Some((x0, x1, _)) = bracket_axis(&source_scene.fixed_grid.x_scan_rad, x_scan)
            else {
                continue;
            };
            let Some((y0, y1, _)) = bracket_axis(&source_scene.fixed_grid.y_scan_rad, y_scan)
            else {
                continue;
            };
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x1);
            max_y = max_y.max(y1);
            found = true;
        }
        if !found {
            return Ok(Self {
                source_scene,
                field: None,
                x_start: 0,
                y_start: 0,
            });
        }
        min_x = min_x.saturating_sub(1);
        min_y = min_y.saturating_sub(1);
        max_x = (max_x + 1).min(source_scene.fixed_grid.nx - 1);
        max_y = (max_y + 1).min(source_scene.fixed_grid.ny - 1);
        let field = read_goes_abi_field_window(
            path,
            "CMI",
            min_x,
            max_x - min_x + 1,
            min_y,
            max_y - min_y + 1,
        )?;
        Ok(Self {
            source_scene,
            field: Some(field),
            x_start: min_x,
            y_start: min_y,
        })
    }

    fn sample_all(&self, coordinates: &[(f32, f32)]) -> Vec<f32> {
        coordinates
            .iter()
            .map(|&(latitude, longitude)| self.sample(latitude, longitude))
            .collect()
    }

    fn sample(&self, latitude: f32, longitude: f32) -> f32 {
        let Some(field) = self.field.as_ref() else {
            return f32::NAN;
        };
        let Some((x_scan, y_scan)) = scan_for_location(&self.source_scene, latitude, longitude)
        else {
            return f32::NAN;
        };
        let Some((x0, x1, fx)) = bracket_axis(&self.source_scene.fixed_grid.x_scan_rad, x_scan)
        else {
            return f32::NAN;
        };
        let Some((y0, y1, fy)) = bracket_axis(&self.source_scene.fixed_grid.y_scan_rad, y_scan)
        else {
            return f32::NAN;
        };
        if x0 < self.x_start || y0 < self.y_start {
            return f32::NAN;
        }
        let local_x0 = x0 - self.x_start;
        let local_x1 = x1 - self.x_start;
        let local_y0 = y0 - self.y_start;
        let local_y1 = y1 - self.y_start;
        let nx = field.scene.fixed_grid.nx;
        let ny = field.scene.fixed_grid.ny;
        if local_x1 >= nx || local_y1 >= ny {
            return f32::NAN;
        }
        let index = |y: usize, x: usize| y * nx + x;
        bilinear_f32(
            field.values[index(local_y0, local_x0)],
            field.values[index(local_y0, local_x1)],
            field.values[index(local_y1, local_x0)],
            field.values[index(local_y1, local_x1)],
            fx,
            fy,
        )
    }
}

fn scan_for_location(scene: &GoesAbiScene, latitude: f32, longitude: f32) -> Option<(f64, f64)> {
    if !(latitude.is_finite() && longitude.is_finite()) {
        return None;
    }
    lat_lon_to_scan_angles_fast(
        scene.projection.perspective_point_height_m,
        scene.projection.semi_major_axis_m,
        scene.projection.semi_minor_axis_m,
        scene.projection.longitude_of_projection_origin_deg,
        scene.projection.sweep_angle_axis,
        f64::from(latitude),
        f64::from(longitude),
    )
}

fn validate_tile(zoom: u8, tile_x: u32, tile_y: u32, tile_size: u32) -> Result<(), Box<dyn Error>> {
    if zoom > MAXIMUM_TILE_ZOOM {
        return Err(boxed_error(format!(
            "satellite tile zoom {zoom} exceeds {MAXIMUM_TILE_ZOOM}"
        )));
    }
    if tile_size == 0 || tile_size > 512 {
        return Err(boxed_error("satellite tile size must be between 1 and 512"));
    }
    let width = 1u32
        .checked_shl(u32::from(zoom))
        .ok_or_else(|| boxed_error("satellite tile zoom overflow"))?;
    if tile_x >= width || tile_y >= width {
        return Err(boxed_error(format!(
            "satellite tile {zoom}/{tile_x}/{tile_y} is outside the XYZ pyramid"
        )));
    }
    Ok(())
}

fn tile_coordinates(zoom: u8, tile_x: u32, tile_y: u32, tile_size: u32) -> Vec<(f32, f32)> {
    let scale = f64::from(1u32 << zoom);
    let tile_size_f64 = f64::from(tile_size);
    let mut coordinates = Vec::with_capacity((tile_size * tile_size) as usize);
    for pixel_y in 0..tile_size {
        let world_y = (f64::from(tile_y) + (f64::from(pixel_y) + 0.5) / tile_size_f64) / scale;
        let latitude = (std::f64::consts::PI * (1.0 - 2.0 * world_y))
            .sinh()
            .atan()
            .to_degrees()
            .clamp(-85.051_128_78, 85.051_128_78);
        for pixel_x in 0..tile_size {
            let world_x = (f64::from(tile_x) + (f64::from(pixel_x) + 0.5) / tile_size_f64) / scale;
            let longitude = world_x * 360.0 - 180.0;
            coordinates.push((latitude as f32, longitude as f32));
        }
    }
    coordinates
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidInput, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xyz_coordinate_grid_is_bounded() {
        let coordinates = tile_coordinates(0, 0, 0, 16);
        assert_eq!(coordinates.len(), 256);
        assert!(coordinates.iter().all(|(latitude, longitude)| {
            (-85.1..=85.1).contains(latitude) && (-180.0..=180.0).contains(longitude)
        }));
    }

    #[test]
    fn invalid_tile_coordinates_are_rejected() {
        assert!(validate_tile(2, 4, 0, 256).is_err());
        assert!(validate_tile(MAXIMUM_TILE_ZOOM + 1, 0, 0, 256).is_err());
    }
}

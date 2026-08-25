//! Windowed native-source rendering for MapLibre/Mapbox XYZ tiles.
//!
//! Each requested tile is projected directly into each ABI channel's fixed
//! grid. Detail zooms decode the minimal native window; overview zooms use
//! globally aligned, strided area averages in bounded blocks. Full Disk C02
//! therefore never becomes a whole-plane allocation.

use std::collections::BTreeMap;
use std::error::Error;
use std::io;
use std::path::Path;

use image::ImageEncoder;

use crate::abi::{
    AbiAreaOverviewWindow, GoesAbiField, GoesAbiScene,
    read_goes_abi_field_area_filtered_window_from_scene, read_goes_abi_field_window_from_scene,
    read_goes_abi_scene_with_identity,
};
use crate::archive::resolve_native_frame;
use crate::composite::{
    TRANSPARENT, bilinear_f32, bracket_axis, validate_variance_encoding_grids, variance_encode_2x2,
};
use crate::geostationary::lat_lon_to_scan_angles_fast;
use crate::product::GoesAbiProduct;
use crate::product_render::{missing_band_error, render_product_pixel};
use crate::solar::solar_elevation_deg;

pub const DEFAULT_TILE_SIZE: u32 = 256;
pub const MAXIMUM_TILE_ZOOM: u8 = 14;

// At overview scales, two ABI-area samples per XYZ pixel retain honest source
// detail without decoding a many-gigabyte native bounding rectangle. Four
// globally phased native samples per overview-cell axis provide anti-aliasing
// while keeping a z0 Full Disk C02 request below four million decoded values.
const FIRST_EXACT_NATIVE_ZOOM: u8 = 7;
const OVERVIEW_CELLS_PER_XYZ_PIXEL: usize = 2;
const OVERVIEW_SAMPLES_PER_CELL_AXIS: usize = 4;
const MAXIMUM_OVERVIEW_READ_CELLS: usize = 1_048_576;

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
    let mut windows = BTreeMap::<u8, ChannelWindow>::new();
    for &channel in product.required_channels() {
        let channel_source = manifest.channels.get(&channel).ok_or_else(|| {
            boxed_error(format!(
                "native frame {} has no ABI C{channel:02}",
                manifest.frame_id
            ))
        })?;
        let path = manifest.channel_path(store_root, channel)?;
        let source = read_goes_abi_scene_with_identity(&path, &channel_source.object_key)?;
        if source.channel != Some(channel) {
            return Err(boxed_error(format!(
                "native frame {} maps ABI C{channel:02} to object {}",
                manifest.frame_id, channel_source.object_key
            )));
        }
        let window = ChannelWindow::open(source, &coordinates, zoom, tile_size)?;
        windows.insert(channel, window);
    }

    // The sharpened products transfer native C02 texture into C01/C03
    // reflectance before the color recipe. `true_color` intentionally remains
    // the basic unsharpened comparison/fallback product.
    let variance_sharpen_day = matches!(
        product,
        GoesAbiProduct::GeoColor
            | GoesAbiProduct::OpenGeoColorV1
            | GoesAbiProduct::SharpenedTrueColor
    ) && windows.values().all(ChannelWindow::uses_exact_native);
    if variance_sharpen_day {
        let c02 = windows
            .get(&2)
            .ok_or_else(|| boxed_error("variance-sharpened color requires ABI C02"))?;
        for channel in [1, 3] {
            let coarse = windows.get(&channel).ok_or_else(|| {
                boxed_error(format!(
                    "variance-sharpened color requires ABI C{channel:02}"
                ))
            })?;
            validate_variance_encoding_grids(&coarse.source_scene, &c02.source_scene)?;
        }
    }

    let mut sampled = BTreeMap::<u8, Vec<f32>>::new();
    for (&channel, window) in &windows {
        let values = if variance_sharpen_day && matches!(channel, 1 | 3) {
            let c02 = windows
                .get(&2)
                .ok_or_else(|| boxed_error("variance-sharpened color requires ABI C02"))?;
            window.sample_all_variance_encoded(c02, &coordinates)
        } else {
            window.sample_all(&coordinates)
        };
        sampled.insert(channel, values);
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
    sampling: TileSampling,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TileSampling {
    ExactNative,
    AreaOverview {
        x_bin_step: usize,
        y_bin_step: usize,
        x_sample_step: usize,
        y_sample_step: usize,
    },
}

impl TileSampling {
    fn for_grid(nx: usize, ny: usize, zoom: u8, tile_size: u32) -> Self {
        if zoom >= FIRST_EXACT_NATIVE_ZOOM {
            return Self::ExactNative;
        }
        let xyz_world_pixels = (tile_size as usize)
            .saturating_mul(1usize << zoom)
            .saturating_mul(OVERVIEW_CELLS_PER_XYZ_PIXEL)
            .max(1);
        let x_bin_step = nx.div_ceil(xyz_world_pixels).max(1);
        let y_bin_step = ny.div_ceil(xyz_world_pixels).max(1);
        Self::AreaOverview {
            x_bin_step,
            y_bin_step,
            x_sample_step: x_bin_step.div_ceil(OVERVIEW_SAMPLES_PER_CELL_AXIS),
            y_sample_step: y_bin_step.div_ceil(OVERVIEW_SAMPLES_PER_CELL_AXIS),
        }
    }
}

impl ChannelWindow {
    fn open(
        source_scene: GoesAbiScene,
        coordinates: &[(f32, f32)],
        zoom: u8,
        tile_size: u32,
    ) -> Result<Self, Box<dyn Error>> {
        let sampling = TileSampling::for_grid(
            source_scene.fixed_grid.nx,
            source_scene.fixed_grid.ny,
            zoom,
            tile_size,
        );
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
                sampling,
            });
        }
        let (field, x_start, y_start) = match sampling {
            TileSampling::ExactNative => {
                min_x = min_x.saturating_sub(1);
                min_y = min_y.saturating_sub(1);
                max_x = (max_x + 1).min(source_scene.fixed_grid.nx - 1);
                max_y = (max_y + 1).min(source_scene.fixed_grid.ny - 1);
                let field = read_goes_abi_field_window_from_scene(
                    &source_scene,
                    "CMI",
                    min_x,
                    max_x - min_x + 1,
                    min_y,
                    max_y - min_y + 1,
                )?;
                (field, min_x, min_y)
            }
            TileSampling::AreaOverview {
                x_bin_step,
                y_bin_step,
                x_sample_step,
                y_sample_step,
            } => {
                let (x_start, x_count) =
                    overview_axis_window(min_x, max_x, source_scene.fixed_grid.nx, x_bin_step);
                let (y_start, y_count) =
                    overview_axis_window(min_y, max_y, source_scene.fixed_grid.ny, y_bin_step);
                let field = read_goes_abi_field_area_filtered_window_from_scene(
                    &source_scene,
                    "CMI",
                    AbiAreaOverviewWindow {
                        x_start,
                        x_count,
                        y_start,
                        y_count,
                        x_bin_step,
                        y_bin_step,
                        x_sample_step,
                        y_sample_step,
                        maximum_read_cells: MAXIMUM_OVERVIEW_READ_CELLS,
                    },
                )?;
                (field, x_start, y_start)
            }
        };
        Ok(Self {
            source_scene,
            field: Some(field),
            x_start,
            y_start,
            sampling,
        })
    }

    fn uses_exact_native(&self) -> bool {
        self.sampling == TileSampling::ExactNative
    }

    fn sample_all(&self, coordinates: &[(f32, f32)]) -> Vec<f32> {
        coordinates
            .iter()
            .map(|&(latitude, longitude)| self.sample(latitude, longitude))
            .collect()
    }

    fn sample_all_variance_encoded(&self, c02: &Self, coordinates: &[(f32, f32)]) -> Vec<f32> {
        coordinates
            .iter()
            .map(|&(latitude, longitude)| self.sample_variance_encoded(c02, latitude, longitude))
            .collect()
    }

    /// Sample a virtual 0.5 km field produced from this 1 km C01/C03 window.
    ///
    /// Each of the four C02 pixels bracketing the requested location is first
    /// variance-encoded from its complete native 2x2 block. Bilinear sampling
    /// happens only after that preprocessing, preserving both the C02 detail
    /// and smooth XYZ reprojection across tile seams. `open`'s one-cell halo
    /// includes every neighboring C02 sample needed by blocks that straddle a
    /// tile boundary.
    fn sample_variance_encoded(&self, c02: &Self, latitude: f32, longitude: f32) -> f32 {
        if self.field.is_none() || c02.field.is_none() {
            return f32::NAN;
        }
        let Some((x_scan, y_scan)) = scan_for_location(&c02.source_scene, latitude, longitude)
        else {
            return f32::NAN;
        };
        let Some((x0, x1, fx)) = bracket_axis(&c02.source_scene.fixed_grid.x_scan_rad, x_scan)
        else {
            return f32::NAN;
        };
        let Some((y0, y1, fy)) = bracket_axis(&c02.source_scene.fixed_grid.y_scan_rad, y_scan)
        else {
            return f32::NAN;
        };

        bilinear_f32(
            self.variance_encoded_at_c02_pixel(c02, x0, y0),
            self.variance_encoded_at_c02_pixel(c02, x1, y0),
            self.variance_encoded_at_c02_pixel(c02, x0, y1),
            self.variance_encoded_at_c02_pixel(c02, x1, y1),
            fx,
            fy,
        )
    }

    fn variance_encoded_at_c02_pixel(&self, c02: &Self, x: usize, y: usize) -> f32 {
        let coarse_x = x / 2;
        let coarse_y = y / 2;
        let red_x = coarse_x * 2;
        let red_y = coarse_y * 2;
        let encoded = variance_encode_2x2(
            self.value_at(coarse_x, coarse_y),
            [
                c02.value_at(red_x, red_y),
                c02.value_at(red_x + 1, red_y),
                c02.value_at(red_x, red_y + 1),
                c02.value_at(red_x + 1, red_y + 1),
            ],
        );
        encoded[(y - red_y) * 2 + (x - red_x)]
    }

    fn value_at(&self, x: usize, y: usize) -> f32 {
        let Some(field) = self.field.as_ref() else {
            return f32::NAN;
        };
        let Some(local_x) = x.checked_sub(self.x_start) else {
            return f32::NAN;
        };
        let Some(local_y) = y.checked_sub(self.y_start) else {
            return f32::NAN;
        };
        let nx = field.scene.fixed_grid.nx;
        if local_x >= nx || local_y >= field.scene.fixed_grid.ny {
            return f32::NAN;
        }
        field.values[local_y * nx + local_x]
    }

    fn sample(&self, latitude: f32, longitude: f32) -> f32 {
        let Some(field) = self.field.as_ref() else {
            return f32::NAN;
        };
        let Some((x_scan, y_scan)) = scan_for_location(&self.source_scene, latitude, longitude)
        else {
            return f32::NAN;
        };
        if bracket_axis(&self.source_scene.fixed_grid.x_scan_rad, x_scan).is_none()
            || bracket_axis(&self.source_scene.fixed_grid.y_scan_rad, y_scan).is_none()
        {
            return f32::NAN;
        }
        if self.sampling != TileSampling::ExactNative {
            return sample_overview_field(field, x_scan, y_scan);
        }
        let (x0, x1, fx) = bracket_axis(&self.source_scene.fixed_grid.x_scan_rad, x_scan)
            .expect("source x bracket was checked");
        let (y0, y1, fy) = bracket_axis(&self.source_scene.fixed_grid.y_scan_rad, y_scan)
            .expect("source y bracket was checked");
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

/// Expand a source-index intersection to complete, globally anchored overview
/// cells plus one cell of interpolation halo on either side.
fn overview_axis_window(
    minimum: usize,
    maximum: usize,
    source_len: usize,
    bin_step: usize,
) -> (usize, usize) {
    debug_assert!(minimum <= maximum);
    debug_assert!(maximum < source_len);
    debug_assert!(bin_step > 0);
    let bin_count = source_len.div_ceil(bin_step);
    let first_bin = (minimum / bin_step).saturating_sub(1);
    let last_bin = (maximum / bin_step + 1).min(bin_count - 1);
    let start = first_bin * bin_step;
    let end = ((last_bin + 1) * bin_step).min(source_len);
    (start, end - start)
}

fn sample_overview_field(field: &GoesAbiField, x_scan: f64, y_scan: f64) -> f32 {
    let grid = &field.scene.fixed_grid;
    let Some((x0, x1, fx)) = bracket_overview_axis(&grid.x_scan_rad, x_scan) else {
        return f32::NAN;
    };
    let Some((y0, y1, fy)) = bracket_overview_axis(&grid.y_scan_rad, y_scan) else {
        return f32::NAN;
    };
    let index = |y: usize, x: usize| y * grid.nx + x;
    bilinear_f32(
        field.values[index(y0, x0)],
        field.values[index(y0, x1)],
        field.values[index(y1, x0)],
        field.values[index(y1, x1)],
        fx,
        fy,
    )
}

/// Overview values represent source-cell areas rather than point samples, so
/// the half-cell region at a source boundary belongs to its nearest overview
/// cell. The caller separately verifies that `value` lies on the native grid.
fn bracket_overview_axis(axis: &[f64], value: f64) -> Option<(usize, usize, f32)> {
    if axis.is_empty() || !value.is_finite() {
        return None;
    }
    if axis.len() == 1 {
        return Some((0, 0, 0.0));
    }
    if let Some(bracket) = bracket_axis(axis, value) {
        return Some(bracket);
    }
    let ascending = axis[axis.len() - 1] >= axis[0];
    let before_first = (ascending && value < axis[0]) || (!ascending && value > axis[0]);
    if before_first {
        Some((0, 0, 0.0))
    } else {
        let last = axis.len() - 1;
        Some((last, last, 0.0))
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

    const FULL_DISK_C02_OBJECT_KEY: &str = "ABI-L2-CMIPF/2026/235/02/OR_ABI-L2-CMIPF-M6C02_G18_s20262350240211_e20262350249519_c20262350249572.nc";

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

    #[test]
    fn z0_full_disk_c02_overview_reads_are_bounded() {
        let sampling = TileSampling::for_grid(21_696, 21_696, 0, 256);
        let TileSampling::AreaOverview {
            x_bin_step,
            y_bin_step,
            x_sample_step,
            y_sample_step,
        } = sampling
        else {
            panic!("z0 Full Disk C02 must use the bounded overview path");
        };
        assert_eq!((x_bin_step, y_bin_step), (43, 43));
        assert_eq!((x_sample_step, y_sample_step), (11, 11));

        let spec = AbiAreaOverviewWindow {
            x_start: 0,
            x_count: 21_696,
            y_start: 0,
            y_count: 21_696,
            x_bin_step,
            y_bin_step,
            x_sample_step,
            y_sample_step,
            maximum_read_cells: MAXIMUM_OVERVIEW_READ_CELLS,
        };
        assert_eq!(spec.output_shape(), (505, 505));
        assert_eq!(spec.sampled_shape(), (1_973, 1_973));
        assert!(
            spec.sampled_shape().0 * spec.sampled_shape().1 < 4_000_000,
            "z0 must not request the 470.7-million-cell native plane"
        );
        let maximum_block = spec.maximum_block_shape();
        assert!(maximum_block.0 * maximum_block.1 <= MAXIMUM_OVERVIEW_READ_CELLS);
    }

    #[test]
    fn adjacent_overview_windows_share_identical_bins_and_source_samples() {
        let source_len = 21_696;
        let bin_step = 43;
        let sample_step = 11;
        // Model two neighboring projected tiles whose native intersections
        // overlap. Both windows expand to whole global overview cells.
        let (left_start, left_count) = overview_axis_window(320, 910, source_len, bin_step);
        let (right_start, right_count) = overview_axis_window(850, 1_430, source_len, bin_step);
        assert_eq!(left_start % bin_step, 0);
        assert_eq!(right_start % bin_step, 0);

        let left_first_bin = left_start / bin_step;
        let left_last_bin = (left_start + left_count).div_ceil(bin_step);
        let right_first_bin = right_start / bin_step;
        let right_last_bin = (right_start + right_count).div_ceil(bin_step);
        let overlap_start = left_first_bin.max(right_first_bin);
        let overlap_end = left_last_bin.min(right_last_bin);
        assert!(overlap_start < overlap_end);

        for global_bin in overlap_start..overlap_end {
            let samples_for = |window_start: usize, window_count: usize| {
                let window_end = window_start + window_count;
                let bin_start = global_bin * bin_step;
                let bin_end = ((global_bin + 1) * bin_step)
                    .min(source_len)
                    .min(window_end);
                let first = bin_start.div_ceil(sample_step) * sample_step;
                (first..bin_end).step_by(sample_step).collect::<Vec<_>>()
            };
            assert_eq!(
                samples_for(left_start, left_count),
                samples_for(right_start, right_count),
                "shared global bin {global_bin} changed across a tile boundary"
            );
        }
    }

    #[test]
    fn zoom_seven_and_above_keep_exact_native_windows() {
        assert_eq!(
            TileSampling::for_grid(21_696, 21_696, FIRST_EXACT_NATIVE_ZOOM, 256),
            TileSampling::ExactNative
        );
        assert_eq!(
            TileSampling::for_grid(21_696, 21_696, MAXIMUM_TILE_ZOOM, 512),
            TileSampling::ExactNative
        );
        assert!(matches!(
            TileSampling::for_grid(21_696, 21_696, FIRST_EXACT_NATIVE_ZOOM - 1, 256),
            TileSampling::AreaOverview { .. }
        ));
    }

    #[test]
    #[ignore = "requires RUSTWX_SATELLITE_STORE with retained g19 Full Disk 20260823T0000"]
    fn retained_short_named_clean_ir_source_renders_a_visible_tile() {
        let root = std::env::var_os("RUSTWX_SATELLITE_STORE")
            .map(std::path::PathBuf::from)
            .expect("set RUSTWX_SATELLITE_STORE to the rw-sat store root");
        let tile = render_native_xyz_tile(
            &root,
            "g19",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            "20260823T0000",
            2,
            1,
            2,
            256,
        )
        .unwrap();
        let rgba = image::load_from_memory_with_format(&tile.png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        let visible = rgba.pixels().filter(|pixel| pixel.0[3] > 0).count();
        assert!(visible > 0, "retained C13 tile was completely transparent");
    }

    #[test]
    #[ignore = "requires RUSTWX_GOES_ABI_C02_FIXTURE"]
    fn full_disk_c02_archives_and_renders_a_native_window() {
        let source = std::env::var_os("RUSTWX_GOES_ABI_C02_FIXTURE")
            .map(std::path::PathBuf::from)
            .expect("set RUSTWX_GOES_ABI_C02_FIXTURE to a Full Disk C02 NetCDF file");
        let scene =
            crate::abi::read_goes_abi_scene_with_identity(&source, FULL_DISK_C02_OBJECT_KEY)
                .unwrap();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "rw-sat-c02-native-render-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&root).unwrap();

        let manifest =
            crate::archive::archive_goes_source(&root, &source, &scene, FULL_DISK_C02_OBJECT_KEY)
                .unwrap();
        let archived = manifest.channel_path(&root, 2).unwrap();
        assert_eq!(
            std::fs::metadata(&archived).unwrap().len(),
            std::fs::metadata(&source).unwrap().len()
        );
        let tile = render_native_xyz_tile(
            &root,
            "g18",
            "fulldisk",
            GoesAbiProduct::RawChannel(2),
            &manifest.frame_id,
            4,
            2,
            5,
            64,
        )
        .unwrap();
        let rgba = image::load_from_memory_with_format(&tile.png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        let visible = rgba.pixels().filter(|pixel| pixel.0[3] > 0).count();
        assert!(visible > 0, "retained Full Disk C02 tile was transparent");

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[ignore = "requires RUSTWX_SATELLITE_STORE with retained g18 Full Disk 20260823T0300 C01/C02/C03"]
    fn retained_full_disk_visible_channels_render_a_variance_sharpened_tile() {
        let root = std::env::var_os("RUSTWX_SATELLITE_STORE")
            .map(std::path::PathBuf::from)
            .expect("set RUSTWX_SATELLITE_STORE to the rw-sat store root");
        // 7/14/58 is a child of 5/3/14 and spans the sunlit eastern Pacific
        // near GOES-18 nadir at this scan time, avoiding a false pass from the
        // daylight transparency gate.
        let sharpened = render_native_xyz_tile(
            &root,
            "g18",
            "fulldisk",
            GoesAbiProduct::SharpenedTrueColor,
            "20260823T0300",
            7,
            14,
            58,
            128,
        )
        .unwrap();
        let basic = render_native_xyz_tile(
            &root,
            "g18",
            "fulldisk",
            GoesAbiProduct::TrueColor,
            "20260823T0300",
            7,
            14,
            58,
            128,
        )
        .unwrap();
        let open_v1 = render_native_xyz_tile(
            &root,
            "g18",
            "fulldisk",
            GoesAbiProduct::OpenGeoColorV1,
            "20260823T0300",
            5,
            3,
            14,
            128,
        )
        .unwrap();
        let sharpened_rgba =
            image::load_from_memory_with_format(&sharpened.png, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8();
        let basic_rgba = image::load_from_memory_with_format(&basic.png, image::ImageFormat::Png)
            .unwrap()
            .to_rgba8();
        let open_v1_rgba =
            image::load_from_memory_with_format(&open_v1.png, image::ImageFormat::Png)
                .unwrap()
                .to_rgba8();
        let visible = sharpened_rgba
            .pixels()
            .filter(|pixel| pixel.0[3] > 0)
            .count();
        assert!(visible > 0, "sharpened Full Disk tile was transparent");
        assert_ne!(
            sharpened_rgba.as_raw(),
            basic_rgba.as_raw(),
            "variance encoding did not change the real C01/C02/C03 tile"
        );
        assert_eq!(open_v1.product, "open_geocolor_v1");
        assert!(
            open_v1_rgba.pixels().any(|pixel| pixel.0[3] > 0),
            "open GeoColor v1 Full Disk tile was transparent"
        );
        assert_ne!(
            open_v1_rgba.as_raw(),
            sharpened_rgba.as_raw(),
            "solar normalization and published log stretch did not change the real tile"
        );
    }
}

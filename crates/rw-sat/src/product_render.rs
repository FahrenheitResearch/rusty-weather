//! Product-level rendering, including solar-aware 24-hour GeoColor.

use std::error::Error;
use std::io;

use crate::composite::{Rgba, TRANSPARENT, compose_goes_abi_rgb_pixel};
use crate::enhancement::SatelliteEnhancement;
use crate::product::GoesAbiProduct;
use crate::solar::{daylight_weight, solar_elevation_deg};

pub fn render_product_pixel<F>(
    product: GoesAbiProduct,
    valid_unix: i64,
    latitude_deg: f64,
    longitude_deg: f64,
    mut band_value: F,
) -> Result<Rgba, Box<dyn Error>>
where
    F: FnMut(u8) -> Result<f32, Box<dyn Error>>,
{
    if product == GoesAbiProduct::GeoColor {
        return render_geocolor_pixel(valid_unix, latitude_deg, longitude_deg, &mut band_value);
    }
    if product == GoesAbiProduct::TrueColor {
        return render_true_color_pixel(&mut band_value);
    }
    if let Some(style) = product.composite_style() {
        return compose_goes_abi_rgb_pixel(style, band_value);
    }
    let channel = product.base_channel();
    let value = band_value(channel)?;
    Ok(product
        .enhancement()
        .unwrap_or(SatelliteEnhancement::InfraredGrayscale)
        .color(value))
}

fn render_geocolor_pixel<F>(
    valid_unix: i64,
    latitude_deg: f64,
    longitude_deg: f64,
    band_value: &mut F,
) -> Result<Rgba, Box<dyn Error>>
where
    F: FnMut(u8) -> Result<f32, Box<dyn Error>>,
{
    let solar_elevation =
        solar_elevation_deg(valid_unix, latitude_deg, longitude_deg).unwrap_or(-90.0);
    let day_weight = daylight_weight(solar_elevation);

    let night = geocolor_night(band_value(13)?);
    if day_weight <= 0.0 {
        return Ok(night);
    }

    let day = geocolor_day(band_value(1)?, band_value(2)?, band_value(3)?);
    if day_weight >= 1.0 {
        return Ok(day);
    }
    Ok(blend_rgba(night, day, day_weight))
}

fn render_true_color_pixel<F>(band_value: &mut F) -> Result<Rgba, Box<dyn Error>>
where
    F: FnMut(u8) -> Result<f32, Box<dyn Error>>,
{
    Ok(geocolor_day(band_value(1)?, band_value(2)?, band_value(3)?))
}

fn geocolor_day(c01: f32, c02: f32, c03: f32) -> Rgba {
    if !c01.is_finite() || !c02.is_finite() || !c03.is_finite() {
        return TRANSPARENT;
    }
    let blue = corrected_visible(c01);
    let red = corrected_visible(c02);
    // ABI has no true green channel.  This standard pseudo-green mixture is
    // intentionally described as pseudo-natural/GeoColor, never raw RGB.
    let pseudo_green = corrected_visible(0.45 * c02 + 0.10 * c03 + 0.45 * c01);
    let mut rgb = [red, pseudo_green, blue];
    // Mild saturation restores the familiar land/ocean separation without
    // clipping bright cloud tops.
    let mean = (f32::from(rgb[0]) + f32::from(rgb[1]) + f32::from(rgb[2])) / 3.0;
    for channel in &mut rgb {
        let value = mean + (f32::from(*channel) - mean) * 1.18;
        *channel = value.round().clamp(0.0, 255.0) as u8;
    }
    [rgb[0], rgb[1], rgb[2], 255]
}

fn corrected_visible(value: f32) -> u8 {
    let normalized = (value.max(0.0) / 1.08).clamp(0.0, 1.0);
    (normalized.powf(0.48) * 255.0).round() as u8
}

fn geocolor_night(c13_kelvin: f32) -> Rgba {
    let [value, _, _, alpha] = SatelliteEnhancement::InfraredGrayscale.color(c13_kelvin);
    if alpha == 0 {
        return TRANSPARENT;
    }
    // Subtle cool tint matches the visual language of common 24-hour
    // GeoColor products while remaining an honest C13-only night layer.
    [
        (f32::from(value) * 0.86).round() as u8,
        (f32::from(value) * 0.93).round() as u8,
        value,
        255,
    ]
}

fn blend_rgba(left: Rgba, right: Rgba, fraction: f32) -> Rgba {
    if left[3] == 0 {
        return right;
    }
    if right[3] == 0 {
        return left;
    }
    let fraction = fraction.clamp(0.0, 1.0);
    let blend = |a: u8, b: u8| {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * fraction)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    [
        blend(left[0], right[0]),
        blend(left[1], right[1]),
        blend(left[2], right[2]),
        255,
    ]
}

pub fn missing_band_error(channel: u8) -> Box<dyn Error> {
    Box::new(io::Error::new(
        io::ErrorKind::InvalidData,
        format!("missing ABI C{channel:02} value"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn render_at(time: &str) -> Rgba {
        let valid = chrono::DateTime::parse_from_rfc3339(time)
            .unwrap()
            .timestamp();
        let values = HashMap::from([(1, 0.35), (2, 0.45), (3, 0.28), (13, 220.0)]);
        render_product_pixel(GoesAbiProduct::GeoColor, valid, 40.0, -105.0, |channel| {
            values
                .get(&channel)
                .copied()
                .ok_or_else(|| missing_band_error(channel))
        })
        .unwrap()
    }

    #[test]
    fn geocolor_changes_between_day_and_night() {
        let day = render_at("2026-06-21T19:00:00Z");
        let night = render_at("2026-06-21T07:00:00Z");
        assert_ne!(day, night);
        assert_eq!(day[3], 255);
        assert_eq!(night[3], 255);
        assert_eq!(night[2], 226);
    }
}

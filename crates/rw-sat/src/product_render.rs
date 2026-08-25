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
    if product == GoesAbiProduct::OpenGeoColorV1 {
        return render_open_geocolor_v1_pixel(
            valid_unix,
            latitude_deg,
            longitude_deg,
            &mut band_value,
        );
    }
    if matches!(
        product,
        GoesAbiProduct::SharpenedTrueColor | GoesAbiProduct::TrueColor
    ) {
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

/// Render the open, reproducible portion of the GeoColor daytime chain.
///
/// Processing order matters: the tile renderer has already variance-encoded
/// C01/C03 on the native C02 grid; here CMI is normalized for solar zenith,
/// fractional green is synthesized, and the three components receive the
/// published GeoColor display transform.
///
/// Provenance:
/// - NOAA Enterprise CMIP ATBD v4, sec. 3.4.1.2 and the CMI `standard_name`:
///   CMI solar bands are Lambertian-equivalent reflectance multiplied by the
///   cosine of solar zenith.
///   <https://www.star.nesdis.noaa.gov/goesr/documents/ATBDs/Enterprise/ATBD_Enterprise_Cloud_and_Moisture_Imagery_Product_v4_2021-01-13.pdf>
/// - Bah et al. (2018), <https://doi.org/10.1029/2018EA000379>: the open ABI
///   first-order green estimate is 45% C02 + 10% C03 + 45% C01.
/// - Miller et al. (2020), <https://doi.org/10.1175/JTECH-D-19-0134.1>:
///   clamp reflectance to 0.025..1.20, log10, then normalize -1.6..0.176.
///
/// This deliberately does not claim full SHAC. The CIRA AHI-derived 3-D
/// synthetic-green LUT is not published with the paper, and an authoritative
/// Rayleigh correction requires versioned sensor-response and atmospheric
/// LUT data (as in Apache-licensed PySpectral's LUT-driven implementation),
/// not guessed coefficients:
/// <https://github.com/pytroll/pyspectral/blob/9a58e0b5ecb26195bd305ee937b19fd74829ffa8/pyspectral/rayleigh.py>.
fn render_open_geocolor_v1_pixel<F>(
    valid_unix: i64,
    latitude_deg: f64,
    longitude_deg: f64,
    band_value: &mut F,
) -> Result<Rgba, Box<dyn Error>>
where
    F: FnMut(u8) -> Result<f32, Box<dyn Error>>,
{
    let Some(solar_elevation) = solar_elevation_deg(valid_unix, latitude_deg, longitude_deg) else {
        return Ok(TRANSPARENT);
    };
    Ok(open_geocolor_v1_day(
        band_value(1)?,
        band_value(2)?,
        band_value(3)?,
        solar_elevation,
    ))
}

fn open_geocolor_v1_day(c01: f32, c02: f32, c03: f32, solar_elevation_deg: f32) -> Rgba {
    let Some(blue) = solar_normalized_cmi(c01, solar_elevation_deg) else {
        return TRANSPARENT;
    };
    let Some(red) = solar_normalized_cmi(c02, solar_elevation_deg) else {
        return TRANSPARENT;
    };
    let Some(near_ir) = solar_normalized_cmi(c03, solar_elevation_deg) else {
        return TRANSPARENT;
    };

    // Bah et al. first-order fractional green. This is intentionally formed
    // after per-channel geometry normalization. It is not CIRA's AHI-LUT
    // synthetic green, so applying a second "7% hybrid" term here would count
    // C03 twice and would falsely imply full SHAC fidelity.
    let green = 0.45 * red + 0.10 * near_ir + 0.45 * blue;
    [
        miller_log_component(red),
        miller_log_component(green),
        miller_log_component(blue),
        255,
    ]
}

/// Convert NOAA CMI solar-band values to solar-zenith-normalized reflectance.
///
/// The 88-degree correction limit is the open operational convention used by
/// Satpy's `sunz_corrected` modifier (implementation inspected at
/// <https://github.com/pytroll/satpy/blob/591ef083c742fddbc16fef0b576604d386257c0b/satpy/modifiers/angles.py>).
/// We independently use only the physical 1/cos(SZA) correction with that
/// finite cap and reject below-horizon pixels; no atmospheric or view-angle
/// correction is implied.
fn solar_normalized_cmi(cmi: f32, solar_elevation_deg: f32) -> Option<f32> {
    if !cmi.is_finite() || !solar_elevation_deg.is_finite() || solar_elevation_deg <= 0.0 {
        return None;
    }
    const MIN_SOLAR_ELEVATION_DEG: f32 = 2.0; // SZA = 88 degrees
    let cosine_zenith = solar_elevation_deg.to_radians().sin();
    let minimum_cosine = MIN_SOLAR_ELEVATION_DEG.to_radians().sin();
    let reflectance = cmi / cosine_zenith.max(minimum_cosine);
    reflectance.is_finite().then_some(reflectance)
}

fn miller_log_component(reflectance: f32) -> u8 {
    const MIN_REFLECTANCE: f32 = 0.025;
    const MAX_REFLECTANCE: f32 = 1.20;
    const MIN_LOG10: f32 = -1.6;
    const MAX_LOG10: f32 = 0.176;

    let log_reflectance = reflectance.clamp(MIN_REFLECTANCE, MAX_REFLECTANCE).log10();
    let normalized = ((log_reflectance - MIN_LOG10) / (MAX_LOG10 - MIN_LOG10)).clamp(0.0, 1.0);
    (normalized * 255.0).round() as u8
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

    #[test]
    fn miller_display_transform_has_published_bounds_and_golden_values() {
        assert_eq!(miller_log_component(f32::NEG_INFINITY), 0);
        assert_eq!(miller_log_component(0.0), 0);
        assert_eq!(miller_log_component(0.025), 0);
        assert_eq!(miller_log_component(0.1), 86);
        assert_eq!(miller_log_component(0.2), 129);
        assert_eq!(miller_log_component(0.4), 173);
        // Miller clamps at 1.20 but normalizes log10 over -1.6..0.176, so
        // the scientifically specified upper clamp does not map to 255.
        assert_eq!(miller_log_component(1.20), 241);
        assert_eq!(miller_log_component(f32::INFINITY), 241);
    }

    #[test]
    fn solar_normalization_obeys_geometry_and_stays_finite_at_the_terminator() {
        assert!((solar_normalized_cmi(0.4, 90.0).unwrap() - 0.4).abs() < 1.0e-6);
        assert!((solar_normalized_cmi(0.4, 30.0).unwrap() - 0.8).abs() < 1.0e-6);
        let near_horizon = solar_normalized_cmi(0.01, 0.1).unwrap();
        assert!(near_horizon.is_finite());
        assert!((near_horizon - 0.01 / 2.0_f32.to_radians().sin()).abs() < 1.0e-6);
        assert_eq!(solar_normalized_cmi(0.4, 0.0), None);
        assert_eq!(solar_normalized_cmi(f32::NAN, 45.0), None);
    }

    #[test]
    fn open_v1_golden_preserves_bah_green_order_and_has_no_extra_saturation() {
        // At overhead sun CMI equals solar-normalized reflectance. Bah green
        // is 0.45*0.4 + 0.10*0.8 + 0.45*0.2 = 0.35.
        assert_eq!(
            open_geocolor_v1_day(0.2, 0.4, 0.8, 90.0),
            [173, 164, 129, 255]
        );
        // Equal inputs must remain neutral through green synthesis and stretch.
        assert_eq!(
            open_geocolor_v1_day(0.2, 0.2, 0.2, 90.0),
            [129, 129, 129, 255]
        );
        assert_eq!(open_geocolor_v1_day(f32::NAN, 0.2, 0.2, 90.0), TRANSPARENT);
        assert_eq!(open_geocolor_v1_day(0.2, 0.2, 0.2, -1.0), TRANSPARENT);
    }
}

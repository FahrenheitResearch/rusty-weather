//! Lightweight solar geometry for day/night satellite composites.

use chrono::{Datelike, TimeZone, Timelike, Utc};

/// Approximate apparent solar elevation in degrees.
///
/// The NOAA fractional-year approximation is accurate enough for the smooth
/// day/twilight/night blend used by GeoColor; it is not an astronomical
/// ephemeris API.
pub fn solar_elevation_deg(valid_unix: i64, latitude_deg: f64, longitude_deg: f64) -> Option<f32> {
    if !latitude_deg.is_finite()
        || !longitude_deg.is_finite()
        || !(-90.0..=90.0).contains(&latitude_deg)
    {
        return None;
    }
    let time = Utc.timestamp_opt(valid_unix, 0).single()?;
    let hour = f64::from(time.hour())
        + f64::from(time.minute()) / 60.0
        + f64::from(time.second()) / 3600.0;
    let days = if time.year_ce().1 % 4 == 0 {
        366.0
    } else {
        365.0
    };
    let gamma = std::f64::consts::TAU / days * (f64::from(time.ordinal0()) + (hour - 12.0) / 24.0);

    let equation_of_time_minutes = 229.18
        * (0.000_075 + 0.001_868 * gamma.cos()
            - 0.032_077 * gamma.sin()
            - 0.014_615 * (2.0 * gamma).cos()
            - 0.040_849 * (2.0 * gamma).sin());
    let declination = 0.006_918 - 0.399_912 * gamma.cos() + 0.070_257 * gamma.sin()
        - 0.006_758 * (2.0 * gamma).cos()
        + 0.000_907 * (2.0 * gamma).sin()
        - 0.002_697 * (3.0 * gamma).cos()
        + 0.001_480 * (3.0 * gamma).sin();

    let true_solar_minutes =
        (hour * 60.0 + equation_of_time_minutes + 4.0 * longitude_deg).rem_euclid(1440.0);
    let hour_angle_deg = if true_solar_minutes / 4.0 < 0.0 {
        true_solar_minutes / 4.0 + 180.0
    } else {
        true_solar_minutes / 4.0 - 180.0
    };
    let latitude = latitude_deg.to_radians();
    let hour_angle = hour_angle_deg.to_radians();
    let cosine_zenith = (latitude.sin() * declination.sin()
        + latitude.cos() * declination.cos() * hour_angle.cos())
    .clamp(-1.0, 1.0);
    let elevation = 90.0 - cosine_zenith.acos().to_degrees();
    elevation.is_finite().then_some(elevation as f32)
}

/// Smooth daylight contribution for GeoColor.
///
/// 0 = night at/below -6° solar elevation; 1 = full daytime at/above +3°.
pub fn daylight_weight(solar_elevation_deg: f32) -> f32 {
    let normalized = ((solar_elevation_deg + 6.0) / 9.0).clamp(0.0, 1.0);
    normalized * normalized * (3.0 - 2.0 * normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noon_and_midnight_have_opposite_signs() {
        let noon = chrono::DateTime::parse_from_rfc3339("2026-06-21T19:00:00Z")
            .unwrap()
            .timestamp();
        let midnight = chrono::DateTime::parse_from_rfc3339("2026-06-21T07:00:00Z")
            .unwrap()
            .timestamp();
        assert!(solar_elevation_deg(noon, 40.0, -105.0).unwrap() > 60.0);
        assert!(solar_elevation_deg(midnight, 40.0, -105.0).unwrap() < 0.0);
    }

    #[test]
    fn twilight_blend_is_smooth_and_bounded() {
        assert_eq!(daylight_weight(-10.0), 0.0);
        assert_eq!(daylight_weight(5.0), 1.0);
        let middle = daylight_weight(-1.5);
        assert!(middle > 0.0 && middle < 1.0);
    }
}

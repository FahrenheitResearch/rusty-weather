//! Conventional satellite display enhancements shared by desktop previews,
//! PNG export, and rw-server XYZ tiles.
//!
//! Scientific values remain untouched.  This module only maps calibrated ABI
//! reflectance factors or brightness temperatures to display RGBA values.

use serde::{Deserialize, Serialize};

use crate::composite::{Rgba, TRANSPARENT};

pub type EnhancementStops = &'static [(f32, [u8; 3])];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteEnhancement {
    Visible,
    VisibleHighContrast,
    InfraredGrayscale,
    InfraredEnhanced,
    WaterVapor,
    ShortwaveInfrared,
    Ozone,
}

impl SatelliteEnhancement {
    pub const ALL: [Self; 7] = [
        Self::Visible,
        Self::VisibleHighContrast,
        Self::InfraredGrayscale,
        Self::InfraredEnhanced,
        Self::WaterVapor,
        Self::ShortwaveInfrared,
        Self::Ozone,
    ];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::Visible => "visible",
            Self::VisibleHighContrast => "visible_high_contrast",
            Self::InfraredGrayscale => "infrared_grayscale",
            Self::InfraredEnhanced => "infrared_enhanced",
            Self::WaterVapor => "water_vapor",
            Self::ShortwaveInfrared => "shortwave_infrared",
            Self::Ozone => "ozone",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        Self::ALL.into_iter().find(|style| style.slug() == value)
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::Visible => "Visible",
            Self::VisibleHighContrast => "Visible · high contrast",
            Self::InfraredGrayscale => "Infrared · grayscale",
            Self::InfraredEnhanced => "Infrared · enhanced cloud tops",
            Self::WaterVapor => "Water vapor",
            Self::ShortwaveInfrared => "Shortwave infrared",
            Self::Ozone => "Ozone",
        }
    }

    pub const fn value_units(self) -> &'static str {
        match self {
            Self::Visible | Self::VisibleHighContrast => "reflectance_factor",
            _ => "K",
        }
    }

    pub const fn stops(self) -> EnhancementStops {
        match self {
            Self::Visible => VISIBLE_STOPS,
            Self::VisibleHighContrast => VISIBLE_HIGH_CONTRAST_STOPS,
            Self::InfraredGrayscale => INFRARED_GRAYSCALE_STOPS,
            Self::InfraredEnhanced => INFRARED_ENHANCED_STOPS,
            Self::WaterVapor => WATER_VAPOR_STOPS,
            Self::ShortwaveInfrared => SHORTWAVE_INFRARED_STOPS,
            Self::Ozone => OZONE_STOPS,
        }
    }

    pub fn color(self, value: f32) -> Rgba {
        if !value.is_finite() {
            return TRANSPARENT;
        }
        match self {
            Self::Visible => visible_color(value, 0.50, 1.08),
            Self::VisibleHighContrast => visible_color(value, 0.42, 0.82),
            _ => interpolate(value, self.stops()),
        }
    }
}

/// Default enhancement for one calibrated ABI channel.
pub const fn default_enhancement_for_channel(channel: u8) -> SatelliteEnhancement {
    match channel {
        1..=6 => SatelliteEnhancement::Visible,
        7 => SatelliteEnhancement::ShortwaveInfrared,
        8..=10 => SatelliteEnhancement::WaterVapor,
        12 => SatelliteEnhancement::Ozone,
        11 | 13..=16 => SatelliteEnhancement::InfraredGrayscale,
        _ => SatelliteEnhancement::InfraredGrayscale,
    }
}

fn visible_color(value: f32, gamma: f32, white_point: f32) -> Rgba {
    // ABI CMI visible values are reflectance factors.  Keep modest
    // super-unity cloud reflectance useful while avoiding the old blue-gray
    // tint that made ordinary visible imagery look unlike operational sites.
    let normalized = (value.max(0.0) / white_point).clamp(0.0, 1.0);
    let byte = (normalized.powf(gamma) * 255.0).round() as u8;
    [byte, byte, byte, 255]
}

pub fn interpolate(value: f32, stops: EnhancementStops) -> Rgba {
    if !value.is_finite() || stops.is_empty() {
        return TRANSPARENT;
    }
    if value <= stops[0].0 {
        let [r, g, b] = stops[0].1;
        return [r, g, b, 255];
    }
    for pair in stops.windows(2) {
        let (low, low_color) = pair[0];
        let (high, high_color) = pair[1];
        if value <= high {
            let fraction = if high > low {
                ((value - low) / (high - low)).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let blend = |left: u8, right: u8| {
                (f32::from(left) + (f32::from(right) - f32::from(left)) * fraction)
                    .round()
                    .clamp(0.0, 255.0) as u8
            };
            return [
                blend(low_color[0], high_color[0]),
                blend(low_color[1], high_color[1]),
                blend(low_color[2], high_color[2]),
                255,
            ];
        }
    }
    let [r, g, b] = stops[stops.len() - 1].1;
    [r, g, b, 255]
}

const VISIBLE_STOPS: EnhancementStops = &[
    (0.00, [0, 0, 0]),
    (0.20, [110, 110, 110]),
    (0.50, [181, 181, 181]),
    (0.80, [229, 229, 229]),
    (1.08, [255, 255, 255]),
];

const VISIBLE_HIGH_CONTRAST_STOPS: EnhancementStops = &[
    (0.00, [0, 0, 0]),
    (0.08, [65, 65, 65]),
    (0.25, [145, 145, 145]),
    (0.50, [211, 211, 211]),
    (0.82, [255, 255, 255]),
];

// Cold brightness temperatures are bright, warm land/ocean is dark—the
// familiar clean-window convention used by most operational model sites.
const INFRARED_GRAYSCALE_STOPS: EnhancementStops = &[
    (180.0, [255, 255, 255]),
    (200.0, [247, 247, 247]),
    (220.0, [226, 226, 226]),
    (240.0, [190, 190, 190]),
    (260.0, [145, 145, 145]),
    (280.0, [96, 96, 96]),
    (300.0, [48, 48, 48]),
    (330.0, [0, 0, 0]),
];

// A restrained operational-style cold-cloud enhancement.  It deliberately
// avoids the previous per-channel decorative palettes and keeps warm values
// in grayscale so convective-top colors remain easy to interpret.
const INFRARED_ENHANCED_STOPS: EnhancementStops = &[
    (180.0, [255, 0, 255]),
    (190.0, [220, 0, 40]),
    (200.0, [255, 86, 0]),
    (210.0, [255, 218, 0]),
    (220.0, [79, 205, 0]),
    (230.0, [0, 211, 211]),
    (240.0, [52, 101, 255]),
    (250.0, [245, 245, 245]),
    (273.15, [155, 155, 155]),
    (300.0, [55, 55, 55]),
    (330.0, [0, 0, 0]),
];

const WATER_VAPOR_STOPS: EnhancementStops = &[
    (180.0, [255, 255, 255]),
    (195.0, [181, 238, 255]),
    (210.0, [47, 150, 255]),
    (225.0, [45, 48, 181]),
    (240.0, [116, 49, 137]),
    (252.0, [174, 89, 52]),
    (265.0, [122, 105, 62]),
    (280.0, [42, 39, 28]),
    (295.0, [0, 0, 0]),
];

const SHORTWAVE_INFRARED_STOPS: EnhancementStops = &[
    (190.0, [255, 255, 255]),
    (230.0, [197, 225, 255]),
    (270.0, [116, 148, 176]),
    (300.0, [64, 64, 64]),
    (325.0, [120, 87, 42]),
    (350.0, [229, 112, 0]),
    (375.0, [229, 24, 24]),
    (410.0, [255, 245, 180]),
    (430.0, [255, 255, 255]),
];

const OZONE_STOPS: EnhancementStops = &[
    (190.0, [255, 255, 255]),
    (210.0, [187, 226, 255]),
    (230.0, [82, 142, 215]),
    (250.0, [97, 76, 154]),
    (270.0, [153, 91, 118]),
    (290.0, [123, 111, 91]),
    (310.0, [64, 64, 64]),
    (330.0, [0, 0, 0]),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tables_are_strictly_ordered() {
        for enhancement in SatelliteEnhancement::ALL {
            for pair in enhancement.stops().windows(2) {
                assert!(pair[0].0 < pair[1].0, "{}", enhancement.slug());
            }
        }
    }

    #[test]
    fn visible_and_ir_have_expected_orientation() {
        assert!(SatelliteEnhancement::Visible.color(0.8)[0] > SatelliteEnhancement::Visible.color(0.1)[0]);
        assert!(SatelliteEnhancement::InfraredGrayscale.color(200.0)[0] > SatelliteEnhancement::InfraredGrayscale.color(310.0)[0]);
        assert_eq!(SatelliteEnhancement::Visible.color(f32::NAN), TRANSPARENT);
    }
}

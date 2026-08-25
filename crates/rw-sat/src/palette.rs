//! Backward-compatible palette helpers backed by the shared conventional
//! satellite enhancements.

use crate::composite::Rgba;
use crate::enhancement::{
    EnhancementStops, SatelliteEnhancement, default_enhancement_for_channel, interpolate,
};

pub type Anchors = EnhancementStops;

/// Conventional default stops for one ABI channel.
pub fn band_anchors(channel: u8) -> Anchors {
    default_enhancement_for_channel(channel).stops()
}

/// Compatibility interpolation helper for callers that explicitly retain an
/// anchor table.
pub fn anchor_color(value: f32, anchors: Anchors) -> Rgba {
    interpolate(value, anchors)
}

/// False-color one calibrated ABI value with the same defaults used by the
/// desktop and rw-server.
pub fn band_color(channel: u8, value: f32) -> Rgba {
    default_enhancement_for_channel(channel).color(value)
}

pub fn enhancement_color(enhancement: SatelliteEnhancement, value: f32) -> Rgba {
    enhancement.color(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::TRANSPARENT;

    #[test]
    fn default_tables_are_strictly_ascending() {
        for channel in 1..=16u8 {
            for pair in band_anchors(channel).windows(2) {
                assert!(pair[0].0 < pair[1].0, "C{channel:02}");
            }
        }
    }

    #[test]
    fn clean_ir_is_bright_cold_and_dark_warm() {
        assert!(band_color(13, 200.0)[0] > band_color(13, 320.0)[0]);
        assert_eq!(band_color(13, f32::NAN), TRANSPARENT);
    }

    #[test]
    fn visible_is_neutral_grayscale() {
        let value = band_color(2, 0.5);
        assert_eq!(value[0], value[1]);
        assert_eq!(value[1], value[2]);
        assert!(value[0] > 150);
    }
}

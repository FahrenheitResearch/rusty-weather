//! Typed GOES ABI product catalog.
//!
//! UI and HTTP clients select products such as `geocolor`, `clean_ir`, or
//! `upper_water_vapor`; required ABI channels and rendering details remain an
//! implementation concern.  Raw C01-C16 access is retained under the advanced
//! category.

use serde::{Deserialize, Serialize};

use crate::composite::GoesAbiRgbCompositeStyle;
use crate::enhancement::{SatelliteEnhancement, default_enhancement_for_channel};
use crate::s3::Sector;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteProductCategory {
    Favorites,
    Visible,
    Infrared,
    WaterVapor,
    RgbComposite,
    Fire,
    Advanced,
}

impl SatelliteProductCategory {
    pub const fn title(self) -> &'static str {
        match self {
            Self::Favorites => "Favorites",
            Self::Visible => "Visible",
            Self::Infrared => "Infrared",
            Self::WaterVapor => "Water vapor",
            Self::RgbComposite => "RGB composites",
            Self::Fire => "Fire",
            Self::Advanced => "Raw ABI channels",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoesAbiProduct {
    GeoColor,
    OpenGeoColorV1,
    SharpenedTrueColor,
    TrueColor,
    CleanInfrared,
    EnhancedInfrared,
    ShortwaveInfrared,
    UpperWaterVapor,
    MidWaterVapor,
    LowerWaterVapor,
    AirMass,
    Dust,
    FireTemperature,
    DayCloudPhase,
    DayNightCloudMicrophysics,
    Sandwich,
    CloudPhase,
    Ozone,
    LongwaveInfrared,
    DirtyInfrared,
    Co2Infrared,
    RawChannel(u8),
}

impl GoesAbiProduct {
    pub const NAMED: [Self; 21] = [
        Self::GeoColor,
        Self::OpenGeoColorV1,
        Self::SharpenedTrueColor,
        Self::TrueColor,
        Self::CleanInfrared,
        Self::EnhancedInfrared,
        Self::ShortwaveInfrared,
        Self::UpperWaterVapor,
        Self::MidWaterVapor,
        Self::LowerWaterVapor,
        Self::AirMass,
        Self::Dust,
        Self::FireTemperature,
        Self::DayCloudPhase,
        Self::DayNightCloudMicrophysics,
        Self::Sandwich,
        Self::CloudPhase,
        Self::Ozone,
        Self::LongwaveInfrared,
        Self::DirtyInfrared,
        Self::Co2Infrared,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        let normalized = value.trim().to_ascii_lowercase().replace(['-', ' '], "_");
        if let Some(channel) = normalized
            .strip_prefix('c')
            .or_else(|| normalized.strip_prefix("band_"))
            .or_else(|| normalized.strip_prefix("channel_"))
            .and_then(|raw| raw.parse::<u8>().ok())
            .filter(|channel| (1..=16).contains(channel))
        {
            return Some(Self::RawChannel(channel));
        }
        match normalized.as_str() {
            "geocolor" | "geo_color" | "visible_ir" => Some(Self::GeoColor),
            "open_geocolor_v1" | "open_day_color_v1" | "published_day_color_v1" => {
                Some(Self::OpenGeoColorV1)
            }
            "sharpened_true_color" | "variance_sharpened_true_color" => {
                Some(Self::SharpenedTrueColor)
            }
            "true_color" | "truecolour" | "true_colour" | "natural_color" => Some(Self::TrueColor),
            "clean_ir" | "ir" | "infrared" | "clean_window" | "cmi_c13" => {
                Some(Self::CleanInfrared)
            }
            "enhanced_ir" | "enhanced_infrared" | "ir_enhanced" => Some(Self::EnhancedInfrared),
            "shortwave_ir" | "shortwave_infrared" | "swir" => Some(Self::ShortwaveInfrared),
            "upper_water_vapor" | "upper_wv" | "wv_upper" => Some(Self::UpperWaterVapor),
            "mid_water_vapor" | "middle_water_vapor" | "mid_wv" | "water_vapor" | "wv" => {
                Some(Self::MidWaterVapor)
            }
            "lower_water_vapor" | "lower_wv" | "wv_lower" => Some(Self::LowerWaterVapor),
            "airmass" | "air_mass" | "airmass_rgb" => Some(Self::AirMass),
            "dust" | "dust_rgb" => Some(Self::Dust),
            "fire_temperature" | "fire_temp" | "fire_rgb" => Some(Self::FireTemperature),
            "day_cloud_phase" | "cloud_phase_rgb" => Some(Self::DayCloudPhase),
            "day_night_cloud_microphysics"
            | "day_night_cloud_micro_combo"
            | "cloud_microphysics" => Some(Self::DayNightCloudMicrophysics),
            "sandwich" | "sandwich_rgb" => Some(Self::Sandwich),
            "cloud_phase" | "cloud_top_phase" => Some(Self::CloudPhase),
            "ozone" => Some(Self::Ozone),
            "longwave_ir" | "longwave_infrared" => Some(Self::LongwaveInfrared),
            "dirty_ir" | "dirty_window" => Some(Self::DirtyInfrared),
            "co2_ir" | "co2_infrared" => Some(Self::Co2Infrared),
            _ => None,
        }
    }

    pub fn slug(self) -> String {
        match self {
            Self::GeoColor => "geocolor".into(),
            Self::OpenGeoColorV1 => "open_geocolor_v1".into(),
            Self::SharpenedTrueColor => "sharpened_true_color".into(),
            Self::TrueColor => "true_color".into(),
            Self::CleanInfrared => "clean_ir".into(),
            Self::EnhancedInfrared => "enhanced_ir".into(),
            Self::ShortwaveInfrared => "shortwave_ir".into(),
            Self::UpperWaterVapor => "upper_water_vapor".into(),
            Self::MidWaterVapor => "mid_water_vapor".into(),
            Self::LowerWaterVapor => "lower_water_vapor".into(),
            Self::AirMass => "airmass".into(),
            Self::Dust => "dust".into(),
            Self::FireTemperature => "fire_temperature".into(),
            Self::DayCloudPhase => "day_cloud_phase".into(),
            Self::DayNightCloudMicrophysics => "day_night_cloud_microphysics".into(),
            Self::Sandwich => "sandwich".into(),
            Self::CloudPhase => "cloud_phase".into(),
            Self::Ozone => "ozone".into(),
            Self::LongwaveInfrared => "longwave_ir".into(),
            Self::DirtyInfrared => "dirty_ir".into(),
            Self::Co2Infrared => "co2_ir".into(),
            Self::RawChannel(channel) => format!("c{channel:02}"),
        }
    }

    pub fn title(self) -> String {
        match self {
            Self::GeoColor => "GeoColor · variance-sharpened".into(),
            Self::OpenGeoColorV1 => "Open GeoColor Day v1 · published core".into(),
            Self::SharpenedTrueColor => "Sharpened True Color · 0.5 km".into(),
            Self::TrueColor => "True Color · basic".into(),
            Self::CleanInfrared => "Clean Infrared".into(),
            Self::EnhancedInfrared => "Enhanced Infrared".into(),
            Self::ShortwaveInfrared => "Shortwave Infrared".into(),
            Self::UpperWaterVapor => "Upper-level Water Vapor".into(),
            Self::MidWaterVapor => "Mid-level Water Vapor".into(),
            Self::LowerWaterVapor => "Lower-level Water Vapor".into(),
            Self::AirMass => "Air Mass RGB".into(),
            Self::Dust => "Dust RGB".into(),
            Self::FireTemperature => "Fire Temperature RGB".into(),
            Self::DayCloudPhase => "Day Cloud Phase RGB".into(),
            Self::DayNightCloudMicrophysics => "Day/Night Cloud Microphysics RGB".into(),
            Self::Sandwich => "Sandwich RGB".into(),
            Self::CloudPhase => "Cloud-top Phase".into(),
            Self::Ozone => "Ozone".into(),
            Self::LongwaveInfrared => "Longwave Infrared".into(),
            Self::DirtyInfrared => "Dirty-window Infrared".into(),
            Self::Co2Infrared => "CO₂ Infrared".into(),
            Self::RawChannel(channel) => {
                format!("C{channel:02} · {}", abi_channel_name(channel))
            }
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::GeoColor => {
                "Nominal 0.5 km C02 variance-sharpened pseudo-natural daytime color, blended into clean-window infrared at night; atmospheric/Rayleigh correction and city lights are not yet applied."
            }
            Self::OpenGeoColorV1 => {
                "Versioned daylight product: nominal 0.5 km C02 variance encoding, NOAA CMI solar-zenith normalization, Bah et al. fractional green, and the exact Miller et al. GeoColor log display stretch. It deliberately omits Rayleigh correction and CIRA's unpublished AHI-derived synthetic-green LUT, so it is not full SHAC."
            }
            Self::SharpenedTrueColor => {
                "Nominal 0.5 km daylight pseudo-true-color with C02 variance encoding applied to C01/C03; atmospheric/Rayleigh correction is not yet applied."
            }
            Self::TrueColor => {
                "Basic unsharpened daylight pseudo-true-color from ABI C01, C02, and C03."
            }
            Self::CleanInfrared => {
                "10.3 µm clean-window brightness temperature in familiar grayscale."
            }
            Self::EnhancedInfrared => {
                "10.3 µm clean-window infrared with cold-cloud-top enhancement."
            }
            Self::ShortwaveInfrared => {
                "3.9 µm shortwave infrared for low cloud, fog, and hot spots."
            }
            Self::UpperWaterVapor => "6.2 µm upper-tropospheric water vapor.",
            Self::MidWaterVapor => "6.9 µm mid-tropospheric water vapor.",
            Self::LowerWaterVapor => "7.3 µm lower-tropospheric water vapor.",
            Self::AirMass => {
                "Air-mass RGB emphasizing stratospheric intrusions and thermal contrasts."
            }
            Self::Dust => "Dust RGB using split-window and thermal differences.",
            Self::FireTemperature => {
                "Fire-temperature RGB for active hot spots and burn intensity."
            }
            Self::DayCloudPhase => "Daylight cloud phase and particle-size RGB.",
            Self::DayNightCloudMicrophysics => "Twenty-four-hour cloud microphysics RGB.",
            Self::Sandwich => "Visible texture combined with infrared cloud-top temperature.",
            Self::CloudPhase => "8.4 µm cloud-top phase brightness temperature.",
            Self::Ozone => "9.6 µm ozone-channel brightness temperature.",
            Self::LongwaveInfrared => "11.2 µm longwave infrared.",
            Self::DirtyInfrared => "12.3 µm dirty-window infrared.",
            Self::Co2Infrared => "13.3 µm CO₂ longwave infrared.",
            Self::RawChannel(_) => {
                "Calibrated native ABI channel with its conventional default enhancement."
            }
        }
    }

    pub const fn category(self) -> SatelliteProductCategory {
        match self {
            Self::GeoColor | Self::OpenGeoColorV1 | Self::SharpenedTrueColor => {
                SatelliteProductCategory::Favorites
            }
            Self::TrueColor => SatelliteProductCategory::Visible,
            Self::CleanInfrared
            | Self::EnhancedInfrared
            | Self::ShortwaveInfrared
            | Self::CloudPhase
            | Self::Ozone
            | Self::LongwaveInfrared
            | Self::DirtyInfrared
            | Self::Co2Infrared => SatelliteProductCategory::Infrared,
            Self::UpperWaterVapor | Self::MidWaterVapor | Self::LowerWaterVapor => {
                SatelliteProductCategory::WaterVapor
            }
            Self::AirMass
            | Self::Dust
            | Self::DayCloudPhase
            | Self::DayNightCloudMicrophysics
            | Self::Sandwich => SatelliteProductCategory::RgbComposite,
            Self::FireTemperature => SatelliteProductCategory::Fire,
            Self::RawChannel(_) => SatelliteProductCategory::Advanced,
        }
    }

    pub const fn required_channels(self) -> &'static [u8] {
        match self {
            Self::GeoColor => &[1, 2, 3, 13],
            Self::OpenGeoColorV1 | Self::SharpenedTrueColor | Self::TrueColor => &[1, 2, 3],
            Self::CleanInfrared | Self::EnhancedInfrared => &[13],
            Self::ShortwaveInfrared => &[7],
            Self::UpperWaterVapor => &[8],
            Self::MidWaterVapor => &[9],
            Self::LowerWaterVapor => &[10],
            Self::AirMass => &[8, 10, 12, 13],
            Self::Dust => &[11, 13, 14, 15],
            Self::FireTemperature => &[5, 6, 7],
            Self::DayCloudPhase => &[2, 5, 13],
            Self::DayNightCloudMicrophysics => &[2, 5, 7, 13, 15],
            Self::Sandwich => &[3, 13],
            Self::CloudPhase => &[11],
            Self::Ozone => &[12],
            Self::LongwaveInfrared => &[14],
            Self::DirtyInfrared => &[15],
            Self::Co2Infrared => &[16],
            Self::RawChannel(channel) => raw_channel_slice(channel),
        }
    }

    pub const fn base_channel(self) -> u8 {
        match self {
            Self::GeoColor | Self::OpenGeoColorV1 | Self::SharpenedTrueColor | Self::TrueColor => 2,
            Self::CleanInfrared | Self::EnhancedInfrared => 13,
            Self::ShortwaveInfrared => 7,
            Self::UpperWaterVapor => 8,
            Self::MidWaterVapor => 9,
            Self::LowerWaterVapor => 10,
            Self::AirMass => 8,
            Self::Dust => 13,
            Self::FireTemperature => 7,
            Self::DayCloudPhase => 13,
            Self::DayNightCloudMicrophysics => 13,
            Self::Sandwich => 13,
            Self::CloudPhase => 11,
            Self::Ozone => 12,
            Self::LongwaveInfrared => 14,
            Self::DirtyInfrared => 15,
            Self::Co2Infrared => 16,
            Self::RawChannel(channel) => channel,
        }
    }

    pub const fn daylight_only(self) -> bool {
        matches!(
            self,
            Self::OpenGeoColorV1 | Self::SharpenedTrueColor | Self::TrueColor | Self::DayCloudPhase
        )
    }

    pub const fn composite_style(self) -> Option<GoesAbiRgbCompositeStyle> {
        match self {
            Self::GeoColor => Some(GoesAbiRgbCompositeStyle::GeoColor),
            Self::OpenGeoColorV1 | Self::SharpenedTrueColor | Self::TrueColor => {
                Some(GoesAbiRgbCompositeStyle::NaturalColor)
            }
            Self::AirMass => Some(GoesAbiRgbCompositeStyle::AirMass),
            Self::Dust => Some(GoesAbiRgbCompositeStyle::Dust),
            Self::FireTemperature => Some(GoesAbiRgbCompositeStyle::FireTemperature),
            Self::DayCloudPhase => Some(GoesAbiRgbCompositeStyle::DayCloudPhase),
            Self::DayNightCloudMicrophysics => {
                Some(GoesAbiRgbCompositeStyle::DayNightCloudMicroCombo)
            }
            Self::Sandwich => Some(GoesAbiRgbCompositeStyle::Sandwich),
            _ => None,
        }
    }

    pub const fn enhancement(self) -> Option<SatelliteEnhancement> {
        match self {
            Self::CleanInfrared => Some(SatelliteEnhancement::InfraredGrayscale),
            Self::EnhancedInfrared => Some(SatelliteEnhancement::InfraredEnhanced),
            Self::ShortwaveInfrared => Some(SatelliteEnhancement::ShortwaveInfrared),
            Self::UpperWaterVapor | Self::MidWaterVapor | Self::LowerWaterVapor => {
                Some(SatelliteEnhancement::WaterVapor)
            }
            Self::CloudPhase | Self::LongwaveInfrared | Self::DirtyInfrared | Self::Co2Infrared => {
                Some(SatelliteEnhancement::InfraredGrayscale)
            }
            Self::Ozone => Some(SatelliteEnhancement::Ozone),
            Self::RawChannel(channel) => Some(default_enhancement_for_channel(channel)),
            _ => None,
        }
    }

    pub const fn native_resolution_km(self) -> f32 {
        channel_resolution_km(self.base_channel())
    }

    pub fn descriptor(self) -> SatelliteProductDescriptor {
        SatelliteProductDescriptor {
            id: self.slug(),
            title: self.title(),
            description: self.description().to_string(),
            category: self.category(),
            required_channels: self.required_channels().to_vec(),
            base_channel: self.base_channel(),
            native_resolution_km: self.native_resolution_km(),
            daylight_only: self.daylight_only(),
            enhancement: self.enhancement().map(|value| value.slug().to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SatelliteProductDescriptor {
    pub id: String,
    pub title: String,
    pub description: String,
    pub category: SatelliteProductCategory,
    pub required_channels: Vec<u8>,
    pub base_channel: u8,
    pub native_resolution_km: f32,
    pub daylight_only: bool,
    pub enhancement: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SatelliteSectorDescriptor {
    pub id: String,
    pub title: String,
    pub cadence_seconds: u64,
    pub default_poll_seconds: u64,
}

pub fn product_catalog(include_raw_channels: bool) -> Vec<SatelliteProductDescriptor> {
    let mut products = GoesAbiProduct::NAMED
        .into_iter()
        .map(GoesAbiProduct::descriptor)
        .collect::<Vec<_>>();
    if include_raw_channels {
        products.extend((1..=16).map(|channel| GoesAbiProduct::RawChannel(channel).descriptor()));
    }
    products
}

pub fn sector_catalog() -> Vec<SatelliteSectorDescriptor> {
    [
        Sector::FullDisk,
        Sector::Conus,
        Sector::Meso1,
        Sector::Meso2,
    ]
    .into_iter()
    .map(|sector| SatelliteSectorDescriptor {
        id: sector.slug().to_string(),
        title: match sector {
            Sector::FullDisk => "Full Disk · 10 minute",
            Sector::Conus => "CONUS · 5 minute",
            Sector::Meso1 => "Mesoscale 1 · 1 minute",
            Sector::Meso2 => "Mesoscale 2 · 1 minute",
        }
        .to_string(),
        cadence_seconds: sector.cadence_secs(),
        default_poll_seconds: sector.default_poll_secs(),
    })
    .collect()
}

pub const fn channel_resolution_km(channel: u8) -> f32 {
    match channel {
        2 => 0.5,
        1 | 3 | 5 | 6 => 1.0,
        _ => 2.0,
    }
}

pub const fn abi_channel_name(channel: u8) -> &'static str {
    match channel {
        1 => "Blue 0.47 µm",
        2 => "Red 0.64 µm",
        3 => "Veggie 0.86 µm",
        4 => "Cirrus 1.37 µm",
        5 => "Snow/Ice 1.6 µm",
        6 => "Cloud Particle Size 2.2 µm",
        7 => "Shortwave Window 3.9 µm",
        8 => "Upper-level Water Vapor 6.2 µm",
        9 => "Mid-level Water Vapor 6.9 µm",
        10 => "Lower-level Water Vapor 7.3 µm",
        11 => "Cloud-top Phase 8.4 µm",
        12 => "Ozone 9.6 µm",
        13 => "Clean Window 10.3 µm",
        14 => "Longwave Window 11.2 µm",
        15 => "Dirty Window 12.3 µm",
        16 => "CO₂ Longwave 13.3 µm",
        _ => "Unknown ABI channel",
    }
}

const RAW_CHANNELS: [[u8; 1]; 16] = [
    [1],
    [2],
    [3],
    [4],
    [5],
    [6],
    [7],
    [8],
    [9],
    [10],
    [11],
    [12],
    [13],
    [14],
    [15],
    [16],
];

const fn raw_channel_slice(channel: u8) -> &'static [u8] {
    if channel >= 1 && channel <= 16 {
        &RAW_CHANNELS[(channel - 1) as usize]
    } else {
        &[]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_resolve_to_expected_products() {
        assert_eq!(
            GoesAbiProduct::parse("visible-ir"),
            Some(GoesAbiProduct::GeoColor)
        );
        assert_eq!(
            GoesAbiProduct::parse("ir"),
            Some(GoesAbiProduct::CleanInfrared)
        );
        assert_eq!(
            GoesAbiProduct::parse("water vapor"),
            Some(GoesAbiProduct::MidWaterVapor)
        );
        assert_eq!(
            GoesAbiProduct::parse("C13"),
            Some(GoesAbiProduct::RawChannel(13))
        );
    }

    #[test]
    fn geocolor_is_a_twenty_four_hour_product() {
        assert_eq!(GoesAbiProduct::GeoColor.required_channels(), &[1, 2, 3, 13]);
        assert!(!GoesAbiProduct::GeoColor.daylight_only());
        assert_eq!(
            GoesAbiProduct::SharpenedTrueColor.required_channels(),
            &[1, 2, 3]
        );
        assert!(GoesAbiProduct::SharpenedTrueColor.daylight_only());
        assert_eq!(
            GoesAbiProduct::parse("open_geocolor_v1"),
            Some(GoesAbiProduct::OpenGeoColorV1)
        );
        assert_eq!(GoesAbiProduct::parse("shac"), None);
        assert_eq!(
            GoesAbiProduct::OpenGeoColorV1.required_channels(),
            &[1, 2, 3]
        );
        assert!(GoesAbiProduct::OpenGeoColorV1.daylight_only());
        assert!(
            GoesAbiProduct::OpenGeoColorV1
                .description()
                .contains("not full SHAC")
        );
        assert_eq!(GoesAbiProduct::TrueColor.required_channels(), &[1, 2, 3]);
        assert!(GoesAbiProduct::TrueColor.daylight_only());
    }

    #[test]
    fn catalog_includes_every_raw_channel() {
        let catalog = product_catalog(true);
        assert!(catalog.iter().any(|product| product.id == "c01"));
        assert!(catalog.iter().any(|product| product.id == "c16"));
        assert_eq!(sector_catalog()[2].cadence_seconds, 60);
    }
}

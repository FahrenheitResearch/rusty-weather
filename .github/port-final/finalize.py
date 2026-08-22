from __future__ import annotations

from pathlib import Path
import re

ROOT = Path.cwd()


def write(path: str, content: str) -> None:
    target = ROOT / path
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(content.rstrip() + "\n", encoding="utf-8")


def replace_once(path: str, old: str, new: str) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"missing anchor in {path}: {old[:120]!r}")
    target.write_text(text.replace(old, new, 1), encoding="utf-8")


def insert_module(path: str, module: str) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    line = f"pub mod {module};"
    if line in text:
        return
    matches = list(re.finditer(r"^pub mod [a-zA-Z0-9_]+;\n", text, flags=re.MULTILINE))
    if not matches:
        raise SystemExit(f"no module block in {path}")
    pos = matches[-1].end()
    target.write_text(text[:pos] + line + "\n" + text[pos:], encoding="utf-8")


def add_workspace_member(member: str) -> None:
    path = ROOT / "Cargo.toml"
    text = path.read_text(encoding="utf-8")
    quoted = f'    "{member}",\n'
    if quoted in text:
        return
    marker = '    "crates/rw-query",\n'
    if marker not in text:
        raise SystemExit("workspace member anchor missing")
    path.write_text(text.replace(marker, marker + quoted, 1), encoding="utf-8")


def add_toml_dependency(path: str, name: str, value: str, after: str) -> None:
    target = ROOT / path
    text = target.read_text(encoding="utf-8")
    line = f"{name} = {value}\n"
    if re.search(rf"^{re.escape(name)}\s*=", text, flags=re.MULTILINE):
        return
    anchor = f"{after}\n"
    if anchor not in text:
        raise SystemExit(f"dependency anchor missing in {path}: {after}")
    target.write_text(text.replace(anchor, anchor + line, 1), encoding="utf-8")


write(
    "crates/rw-sat/src/products.rs",
    r'''
use serde::{Deserialize, Serialize};

use crate::composite::GoesAbiRgbCompositeStyle;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SatelliteSector {
    FullDisk,
    Conus,
    Meso1,
    Meso2,
}

impl SatelliteSector {
    pub const ALL: [Self; 4] = [Self::FullDisk, Self::Conus, Self::Meso1, Self::Meso2];

    pub const fn slug(self) -> &'static str {
        match self {
            Self::FullDisk => "fulldisk",
            Self::Conus => "conus",
            Self::Meso1 => "meso1",
            Self::Meso2 => "meso2",
        }
    }

    pub const fn title(self) -> &'static str {
        match self {
            Self::FullDisk => "Full Disk",
            Self::Conus => "CONUS",
            Self::Meso1 => "Mesoscale 1",
            Self::Meso2 => "Mesoscale 2",
        }
    }

    pub const fn cadence_seconds(self) -> u64 {
        match self {
            Self::FullDisk => 600,
            Self::Conus => 300,
            Self::Meso1 | Self::Meso2 => 60,
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
            "full" | "full_disk" | "fulldisk" | "fd" => Some(Self::FullDisk),
            "conus" | "continental_us" | "c" => Some(Self::Conus),
            "meso1" | "mesoscale1" | "mesoscale_1" | "m1" => Some(Self::Meso1),
            "meso2" | "mesoscale2" | "mesoscale_2" | "m2" => Some(Self::Meso2),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Enhancement {
    Visible,
    InfraredGray,
    InfraredEnhanced,
    WaterVapor,
    ShortwaveInfrared,
    Ozone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductCategory {
    Everyday,
    Convection,
    Moisture,
    FireAndDust,
    AdvancedBand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProductRecipe {
    Band { channel: u8, enhancement: Enhancement },
    Composite(GoesAbiRgbCompositeStyle),
    GeoColor,
}

#[derive(Debug, Clone)]
pub struct SatelliteProduct {
    pub slug: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub category: ProductCategory,
    pub recipe: ProductRecipe,
    pub base_channel: u8,
    pub required_channels: &'static [u8],
    pub nominal_resolution_km: f32,
    pub daylight_only: bool,
}

impl SatelliteProduct {
    pub const fn band(
        slug: &'static str,
        title: &'static str,
        description: &'static str,
        category: ProductCategory,
        channel: u8,
        enhancement: Enhancement,
        nominal_resolution_km: f32,
    ) -> Self {
        Self {
            slug,
            title,
            description,
            category,
            recipe: ProductRecipe::Band { channel, enhancement },
            base_channel: channel,
            required_channels: band_channels(channel),
            nominal_resolution_km,
            daylight_only: channel <= 6,
        }
    }
}

const fn band_channels(channel: u8) -> &'static [u8] {
    match channel {
        1 => &[1], 2 => &[2], 3 => &[3], 4 => &[4], 5 => &[5], 6 => &[6],
        7 => &[7], 8 => &[8], 9 => &[9], 10 => &[10], 11 => &[11], 12 => &[12],
        13 => &[13], 14 => &[14], 15 => &[15], 16 => &[16], _ => &[],
    }
}

pub fn product_catalog() -> Vec<SatelliteProduct> {
    use Enhancement::*;
    use ProductCategory::*;
    let mut products = vec![
        SatelliteProduct {
            slug: "geocolor",
            title: "GeoColor",
            description: "Pseudo-natural daytime color with a smooth twilight transition to clean-window infrared at night.",
            category: Everyday,
            recipe: ProductRecipe::GeoColor,
            base_channel: 2,
            required_channels: &[1, 2, 3, 13],
            nominal_resolution_km: 0.5,
            daylight_only: false,
        },
        SatelliteProduct {
            slug: "natural_color",
            title: "Natural Color",
            description: "Daytime pseudo-natural visible color from ABI blue, red, and veggie channels.",
            category: Everyday,
            recipe: ProductRecipe::Composite(GoesAbiRgbCompositeStyle::NaturalColor),
            base_channel: 2,
            required_channels: &[1, 2, 3],
            nominal_resolution_km: 0.5,
            daylight_only: true,
        },
        SatelliteProduct::band("clean_ir", "Clean Infrared", "ABI C13 clean longwave window with conventional grayscale.", Everyday, 13, InfraredGray, 2.0),
        SatelliteProduct::band("enhanced_ir", "Enhanced Infrared", "Color-enhanced clean-window infrared for cloud-top temperature structure.", Convection, 13, InfraredEnhanced, 2.0),
        SatelliteProduct::band("shortwave_ir", "Shortwave Infrared", "ABI C07 shortwave window for low cloud, fog, and hot-spot discrimination.", FireAndDust, 7, ShortwaveInfrared, 2.0),
        SatelliteProduct::band("upper_water_vapor", "Upper-Level Water Vapor", "ABI C08 upper-tropospheric water vapor.", Moisture, 8, WaterVapor, 2.0),
        SatelliteProduct::band("mid_water_vapor", "Mid-Level Water Vapor", "ABI C09 mid-tropospheric water vapor.", Moisture, 9, WaterVapor, 2.0),
        SatelliteProduct::band("lower_water_vapor", "Lower-Level Water Vapor", "ABI C10 lower-tropospheric water vapor.", Moisture, 10, WaterVapor, 2.0),
        SatelliteProduct::band("ozone", "Ozone", "ABI C12 ozone-sensitive infrared channel.", Moisture, 12, Ozone, 2.0),
        composite("air_mass", "Air Mass RGB", "Air-mass classification using water-vapor and ozone-sensitive channels.", Moisture, GoesAbiRgbCompositeStyle::AirMass, 2.0, false),
        composite("dust", "Dust RGB", "Dust discrimination over land and ocean.", FireAndDust, GoesAbiRgbCompositeStyle::Dust, 2.0, false),
        composite("fire_temperature", "Fire Temperature RGB", "Shortwave/near-IR hot-spot and fire-temperature composite.", FireAndDust, GoesAbiRgbCompositeStyle::FireTemperature, 2.0, false),
        composite("day_cloud_phase", "Day Cloud Phase", "Daytime cloud phase and particle-size composite.", Convection, GoesAbiRgbCompositeStyle::DayCloudPhase, 0.5, true),
        composite("day_night_cloud_microphysics", "Day/Night Cloud Microphysics", "Twenty-four-hour cloud microphysics composite.", Convection, GoesAbiRgbCompositeStyle::DayNightCloudMicroCombo, 2.0, false),
        composite("sandwich", "Sandwich", "Visible texture blended with enhanced infrared cloud tops.", Convection, GoesAbiRgbCompositeStyle::Sandwich, 1.0, true),
    ];

    const NAMES: [&str; 16] = [
        "Blue 0.47 µm", "Red 0.64 µm", "Veggie 0.86 µm", "Cirrus 1.37 µm",
        "Snow/Ice 1.6 µm", "Cloud Particle Size 2.2 µm", "Shortwave Window 3.9 µm",
        "Upper-Level Water Vapor 6.2 µm", "Mid-Level Water Vapor 6.9 µm",
        "Lower-Level Water Vapor 7.3 µm", "Cloud-Top Phase 8.4 µm", "Ozone 9.6 µm",
        "Clean IR Window 10.3 µm", "IR Longwave 11.2 µm", "Dirty IR Window 12.3 µm",
        "CO₂ Longwave 13.3 µm",
    ];
    for channel in 1..=16u8 {
        let slug: &'static str = Box::leak(format!("c{channel:02}").into_boxed_str());
        let title: &'static str = Box::leak(format!("C{channel:02} · {}", NAMES[usize::from(channel - 1)]).into_boxed_str());
        let enhancement = match channel {
            1..=6 => Visible,
            7 => ShortwaveInfrared,
            8..=10 => WaterVapor,
            12 => Ozone,
            _ => InfraredGray,
        };
        let resolution = match channel { 2 => 0.5, 1 | 3 | 5 => 1.0, _ => 2.0 };
        products.push(SatelliteProduct::band(slug, title, "Raw ABI channel with a conventional default enhancement.", AdvancedBand, channel, enhancement, resolution));
    }
    products
}

fn composite(
    slug: &'static str,
    title: &'static str,
    description: &'static str,
    category: ProductCategory,
    style: GoesAbiRgbCompositeStyle,
    nominal_resolution_km: f32,
    daylight_only: bool,
) -> SatelliteProduct {
    SatelliteProduct {
        slug,
        title,
        description,
        category,
        recipe: ProductRecipe::Composite(style),
        base_channel: style.base_channel(),
        required_channels: style.required_channels(),
        nominal_resolution_km,
        daylight_only,
    }
}

pub fn product_by_slug(value: &str) -> Option<SatelliteProduct> {
    let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
    let alias = match normalized.as_str() {
        "visible" | "true_color" | "truecolour" => "geocolor",
        "ir" | "infrared" | "clean_window" => "clean_ir",
        "eir" | "enhanced_infrared" => "enhanced_ir",
        "water_vapor" | "watervapor" | "wv" => "mid_water_vapor",
        other => other,
    };
    product_catalog().into_iter().find(|product| product.slug == alias)
}

pub fn automatic_preview_stride(nx: usize, ny: usize) -> usize {
    const MAX_PREVIEW_CELLS: usize = 4_000_000;
    for stride in [1usize, 2, 4, 8, 16, 32] {
        if nx.div_ceil(stride).saturating_mul(ny.div_ceil(stride)) <= MAX_PREVIEW_CELLS {
            return stride;
        }
    }
    32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_sector_and_channel() {
        assert_eq!(SatelliteSector::Meso1.cadence_seconds(), 60);
        assert!(product_by_slug("geocolor").unwrap().required_channels.contains(&13));
        for channel in 1..=16 {
            assert!(product_by_slug(&format!("c{channel:02}")).is_some());
        }
    }

    #[test]
    fn preview_stride_hides_large_full_disk_cost() {
        assert_eq!(automatic_preview_stride(2500, 1500), 1);
        assert!(automatic_preview_stride(21696, 21696) >= 8);
    }
}
''',
)

write(
    "crates/rw-sat/src/enhancement.rs",
    r'''
use chrono::{DateTime, Datelike, Timelike, Utc};

use crate::composite::{GoesAbiRgbCompositeStyle, Rgba, TRANSPARENT, compose_goes_abi_rgb_pixel};
use crate::products::{Enhancement, ProductRecipe, SatelliteProduct};

pub fn render_product_pixel<F>(
    product: &SatelliteProduct,
    latitude: f64,
    longitude: f64,
    valid_time: DateTime<Utc>,
    mut value: F,
) -> Rgba
where
    F: FnMut(u8) -> Option<f32>,
{
    match product.recipe {
        ProductRecipe::Band { channel, enhancement } => {
            render_band_pixel(channel, enhancement, value(channel).unwrap_or(f32::NAN))
        }
        ProductRecipe::GeoColor => geocolor(
            value(1).unwrap_or(f32::NAN),
            value(2).unwrap_or(f32::NAN),
            value(3).unwrap_or(f32::NAN),
            value(13).unwrap_or(f32::NAN),
            solar_elevation_degrees(valid_time, latitude, longitude),
        ),
        ProductRecipe::Composite(style) => compose_goes_abi_rgb_pixel(style, |channel| {
            value(channel)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing channel").into())
        })
        .unwrap_or(TRANSPARENT),
    }
}

pub fn render_band_pixel(_channel: u8, enhancement: Enhancement, value: f32) -> Rgba {
    if !value.is_finite() {
        return TRANSPARENT;
    }
    match enhancement {
        Enhancement::Visible => gray(gamma_scale(value as f64, 0.0, 1.0, 2.2)),
        Enhancement::InfraredGray => gray(unit(value as f64, 330.0, 180.0)),
        Enhancement::InfraredEnhanced => enhanced_ir(value as f64),
        Enhancement::WaterVapor => water_vapor(value as f64),
        Enhancement::ShortwaveInfrared => shortwave(value as f64),
        Enhancement::Ozone => ozone(value as f64),
    }
}

fn geocolor(c01: f32, c02: f32, c03: f32, c13: f32, solar_elevation: f64) -> Rgba {
    let visible = if c01.is_finite() && c02.is_finite() && c03.is_finite() {
        let red = gamma_scale(c02 as f64, 0.0, 1.0, 2.2);
        let green = gamma_scale(0.45 * c02 as f64 + 0.10 * c03 as f64 + 0.45 * c01 as f64, 0.0, 1.0, 2.2);
        let blue = gamma_scale(c01 as f64, 0.0, 1.0, 2.2);
        [u8c(red), u8c(green), u8c(blue), 255]
    } else {
        TRANSPARENT
    };
    let infrared = render_band_pixel(13, Enhancement::InfraredGray, c13);
    let daylight = smoothstep(-6.0, 6.0, solar_elevation);
    if visible[3] == 0 {
        return infrared;
    }
    if infrared[3] == 0 {
        return visible;
    }
    blend(infrared, visible, daylight)
}

fn enhanced_ir(kelvin: f64) -> Rgba {
    let c = kelvin - 273.15;
    let anchors = [
        (-100.0, [255, 255, 255]), (-80.0, [210, 160, 255]), (-70.0, [85, 90, 255]),
        (-60.0, [0, 210, 255]), (-50.0, [0, 230, 120]), (-40.0, [255, 245, 0]),
        (-30.0, [255, 145, 0]), (-20.0, [230, 35, 35]), (0.0, [95, 95, 95]),
        (40.0, [10, 10, 10]),
    ];
    ramp(c, &anchors)
}

fn water_vapor(kelvin: f64) -> Rgba {
    let anchors = [
        (185.0, [255, 255, 255]), (205.0, [105, 205, 255]), (225.0, [45, 85, 200]),
        (245.0, [115, 65, 145]), (260.0, [160, 115, 70]), (275.0, [55, 45, 35]),
        (290.0, [10, 10, 10]),
    ];
    ramp(kelvin, &anchors)
}

fn shortwave(kelvin: f64) -> Rgba {
    let anchors = [
        (230.0, [0, 0, 0]), (270.0, [35, 55, 105]), (300.0, [90, 125, 115]),
        (330.0, [235, 185, 35]), (360.0, [245, 80, 20]), (400.0, [230, 20, 110]),
        (430.0, [255, 255, 255]),
    ];
    ramp(kelvin, &anchors)
}

fn ozone(kelvin: f64) -> Rgba {
    let anchors = [
        (190.0, [255, 255, 255]), (220.0, [120, 185, 235]), (245.0, [85, 90, 180]),
        (265.0, [145, 80, 145]), (285.0, [155, 120, 75]), (310.0, [35, 35, 35]),
        (330.0, [5, 5, 5]),
    ];
    ramp(kelvin, &anchors)
}

fn ramp(value: f64, anchors: &[(f64, [u8; 3])]) -> Rgba {
    if value <= anchors[0].0 {
        let [r, g, b] = anchors[0].1;
        return [r, g, b, 255];
    }
    for pair in anchors.windows(2) {
        let (a, ca) = pair[0];
        let (b, cb) = pair[1];
        if value <= b {
            let t = ((value - a) / (b - a)).clamp(0.0, 1.0);
            return [lerp(ca[0], cb[0], t), lerp(ca[1], cb[1], t), lerp(ca[2], cb[2], t), 255];
        }
    }
    let [r, g, b] = anchors[anchors.len() - 1].1;
    [r, g, b, 255]
}

fn solar_elevation_degrees(time: DateTime<Utc>, latitude: f64, longitude: f64) -> f64 {
    let day = f64::from(time.ordinal());
    let hour = f64::from(time.hour()) + f64::from(time.minute()) / 60.0 + f64::from(time.second()) / 3600.0;
    let gamma = 2.0 * std::f64::consts::PI / 365.0 * (day - 1.0 + (hour - 12.0) / 24.0);
    let decl = 0.006918 - 0.399912 * gamma.cos() + 0.070257 * gamma.sin()
        - 0.006758 * (2.0 * gamma).cos() + 0.000907 * (2.0 * gamma).sin()
        - 0.002697 * (3.0 * gamma).cos() + 0.00148 * (3.0 * gamma).sin();
    let eqtime = 229.18 * (0.000075 + 0.001868 * gamma.cos() - 0.032077 * gamma.sin()
        - 0.014615 * (2.0 * gamma).cos() - 0.040849 * (2.0 * gamma).sin());
    let true_solar_minutes = (hour * 60.0 + eqtime + 4.0 * longitude).rem_euclid(1440.0);
    let hour_angle = (true_solar_minutes / 4.0 - 180.0).to_radians();
    let lat = latitude.to_radians();
    (lat.sin() * decl.sin() + lat.cos() * decl.cos() * hour_angle.cos())
        .clamp(-1.0, 1.0)
        .asin()
        .to_degrees()
}

fn unit(value: f64, min: f64, max: f64) -> f64 {
    ((value - min) / (max - min)).clamp(0.0, 1.0)
}
fn gamma_scale(value: f64, min: f64, max: f64, gamma: f64) -> f64 {
    unit(value, min, max).powf(1.0 / gamma)
}
fn gray(value: f64) -> Rgba { let c = u8c(value); [c, c, c, 255] }
fn u8c(value: f64) -> u8 { (value.clamp(0.0, 1.0) * 255.0).round() as u8 }
fn lerp(a: u8, b: u8, t: f64) -> u8 { (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8 }
fn smoothstep(a: f64, b: f64, x: f64) -> f64 { let t = unit(x, a, b); t * t * (3.0 - 2.0 * t) }
fn blend(a: Rgba, b: Rgba, t: f64) -> Rgba { [lerp(a[0], b[0], t), lerp(a[1], b[1], t), lerp(a[2], b[2], t), 255] }

#[cfg(test)]
mod tests {
    use super::*;
    use crate::products::product_by_slug;
    use chrono::TimeZone;

    #[test]
    fn geocolor_has_day_and_night_output() {
        let product = product_by_slug("geocolor").unwrap();
        let noon = Utc.with_ymd_and_hms(2026, 6, 21, 18, 0, 0).unwrap();
        let midnight = Utc.with_ymd_and_hms(2026, 6, 21, 6, 0, 0).unwrap();
        let sample = |time| render_product_pixel(&product, 40.0, -100.0, time, |channel| match channel {
            1 => Some(0.35), 2 => Some(0.45), 3 => Some(0.25), 13 => Some(240.0), _ => None,
        });
        assert_ne!(sample(noon), sample(midnight));
        assert_eq!(sample(noon)[3], 255);
        assert_eq!(sample(midnight)[3], 255);
    }
}
''',
)

write(
    "crates/rw-sat/src/preview.rs",
    r'''
use std::error::Error;
use std::path::Path;

use crate::abi::{AbiFixedGrid, GoesAbiField, read_goes_abi_field, read_goes_abi_scene};
use crate::netcdf::{open_goes_netcdf_lossy, read_scaled_f32_window};
use crate::products::automatic_preview_stride;

pub fn read_goes_abi_preview(
    path: impl AsRef<Path>,
    variable_name: &str,
    requested_stride: usize,
) -> Result<(GoesAbiField, usize), Box<dyn Error>> {
    let path = path.as_ref();
    let mut scene = read_goes_abi_scene(path)?;
    let stride = if requested_stride == 0 {
        automatic_preview_stride(scene.fixed_grid.nx, scene.fixed_grid.ny)
    } else {
        requested_stride.max(1)
    };
    if stride == 1 {
        return Ok((read_goes_abi_field(path, variable_name)?, 1));
    }

    let source_nx = scene.fixed_grid.nx;
    let source_ny = scene.fixed_grid.ny;
    let x_indices = (0..source_nx).step_by(stride).collect::<Vec<_>>();
    let y_indices = (0..source_ny).step_by(stride).collect::<Vec<_>>();
    let file = open_goes_netcdf_lossy(path)?;
    let mut values = Vec::with_capacity(x_indices.len().saturating_mul(y_indices.len()));
    let mut units = None;
    const OUTPUT_ROWS_PER_READ: usize = 8;
    let source_rows_per_read = stride.saturating_mul(OUTPUT_ROWS_PER_READ).max(1);
    for chunk_start in (0..source_ny).step_by(source_rows_per_read) {
        let count = source_rows_per_read.min(source_ny - chunk_start);
        let chunk = read_scaled_f32_window(&file, variable_name, chunk_start, count, 0, source_nx)?;
        units = units.or(chunk.units);
        for local_y in (0..count).step_by(stride) {
            let row = &chunk.values[local_y * source_nx..(local_y + 1) * source_nx];
            values.extend(x_indices.iter().map(|&x| row[x]));
        }
    }
    let expected = x_indices.len().saturating_mul(y_indices.len());
    values.truncate(expected);
    if values.len() != expected {
        return Err(format!("preview read produced {} values, expected {expected}", values.len()).into());
    }
    scene.fixed_grid = AbiFixedGrid {
        nx: x_indices.len(),
        ny: y_indices.len(),
        x_scan_rad: x_indices.iter().map(|&x| scene.fixed_grid.x_scan_rad[x]).collect(),
        y_scan_rad: y_indices.iter().map(|&y| scene.fixed_grid.y_scan_rad[y]).collect(),
    };
    Ok((GoesAbiField {
        scene,
        variable_name: variable_name.to_string(),
        units,
        values,
    }, stride))
}

#[cfg(test)]
mod tests {
    use crate::products::automatic_preview_stride;

    #[test]
    fn automatic_stride_caps_native_full_disk_preview() {
        let stride = automatic_preview_stride(21696, 21696);
        assert!(21696usize.div_ceil(stride).pow(2) <= 4_000_000);
    }
}
''',
)

write(
    "crates/rw-sat/src/source_catalog.rs",
    r'''
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::{DateTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::abi::GoesAbiScene;
use crate::s3::S3Object;

pub const SOURCE_ROOT: &str = ".rw-satellite-sources";
const SCHEMA: &str = "rw-sat.native-source.v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSourceFrame {
    pub schema: String,
    pub platform: String,
    pub bucket: String,
    pub sector: String,
    pub channel: u8,
    pub product: String,
    pub start_unix: i64,
    pub end_unix: i64,
    pub object_key: String,
    pub relative_path: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProductFrame {
    pub frame: String,
    pub valid_unix: i64,
    pub valid_time_utc: String,
    pub channels: Vec<u8>,
}

pub fn archive_goes_source(
    store_root: &Path,
    bucket: &str,
    object: &S3Object,
    downloaded: &Path,
    scene: &GoesAbiScene,
) -> Result<NativeSourceFrame, Box<dyn Error>> {
    let channel = scene.channel.ok_or("GOES source has no ABI channel")?;
    validate_object_key(&object.key)?;
    let root = store_root.join(SOURCE_ROOT);
    let data = root.join("data").join(bucket).join(&object.key);
    if let Some(parent) = data.parent() { fs::create_dir_all(parent)?; }
    if !data.is_file() || fs::metadata(&data)?.len() != object.size_bytes {
        let tmp = data.with_extension(format!("nc.tmp-{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        if fs::hard_link(downloaded, &tmp).is_err() {
            fs::copy(downloaded, &tmp)?;
        }
        if data.exists() { fs::remove_file(&data)?; }
        fs::rename(&tmp, &data)?;
    }
    let platform = scene.satellite.as_str().to_ascii_lowercase();
    let sector = sector_slug(&scene.sector);
    let relative_path = data.strip_prefix(&root)?.to_string_lossy().replace('\\', "/");
    let frame = NativeSourceFrame {
        schema: SCHEMA.to_string(),
        platform: platform.clone(),
        bucket: bucket.to_string(),
        sector: sector.clone(),
        channel,
        product: scene.product.clone(),
        start_unix: scene.start_time_utc.timestamp(),
        end_unix: scene.end_time_utc.timestamp(),
        object_key: object.key.clone(),
        relative_path,
        size_bytes: object.size_bytes,
    };
    let index = root.join("index").join(platform).join(sector).join(format!("{}_c{channel:02}.json", frame.start_unix));
    if let Some(parent) = index.parent() { fs::create_dir_all(parent)?; }
    let bytes = serde_json::to_vec_pretty(&frame)?;
    let tmp = index.with_extension(format!("json.tmp-{}", std::process::id()));
    fs::write(&tmp, bytes)?;
    if index.exists() { fs::remove_file(&index)?; }
    fs::rename(tmp, index)?;
    Ok(frame)
}

pub fn list_product_frames(
    store_root: &Path,
    platform: &str,
    sector: &str,
    required_channels: &[u8],
    limit: usize,
) -> Result<Vec<ProductFrame>, Box<dyn Error>> {
    let entries = read_index(store_root, platform, sector)?;
    let required = required_channels.iter().copied().collect::<BTreeSet<_>>();
    let mut groups = BTreeMap::<i64, BTreeSet<u8>>::new();
    for frame in entries {
        groups.entry(frame.start_unix).or_default().insert(frame.channel);
    }
    let mut result = groups.into_iter().rev().filter_map(|(valid_unix, channels)| {
        required.is_subset(&channels).then(|| ProductFrame {
            frame: valid_unix.to_string(),
            valid_unix,
            valid_time_utc: Utc.timestamp_opt(valid_unix, 0).single().map(|time| time.to_rfc3339()).unwrap_or_default(),
            channels: channels.into_iter().collect(),
        })
    }).take(limit.clamp(1, 10_000)).collect::<Vec<_>>();
    result.sort_by_key(|frame| frame.valid_unix);
    Ok(result)
}

pub fn resolve_product_sources(
    store_root: &Path,
    platform: &str,
    sector: &str,
    required_channels: &[u8],
    frame: &str,
) -> Result<(i64, BTreeMap<u8, PathBuf>), Box<dyn Error>> {
    let entries = read_index(store_root, platform, sector)?;
    let valid_unix = if frame.eq_ignore_ascii_case("latest") {
        let required = required_channels.iter().copied().collect::<BTreeSet<_>>();
        let mut groups = BTreeMap::<i64, BTreeSet<u8>>::new();
        for item in &entries { groups.entry(item.start_unix).or_default().insert(item.channel); }
        groups.into_iter().rev().find(|(_, channels)| required.is_subset(channels)).map(|(time, _)| time).ok_or("no complete frame")?
    } else {
        frame.parse::<i64>().map_err(|_| "frame must be 'latest' or a Unix timestamp")?
    };
    let root = store_root.join(SOURCE_ROOT);
    let mut paths = BTreeMap::new();
    for channel in required_channels {
        let item = entries.iter().filter(|item| item.channel == *channel)
            .min_by_key(|item| (item.start_unix - valid_unix).unsigned_abs())
            .filter(|item| (item.start_unix - valid_unix).unsigned_abs() <= 120)
            .ok_or_else(|| format!("missing C{channel:02} for frame {valid_unix}"))?;
        let path = safe_join(&root, &item.relative_path)?;
        if !path.is_file() { return Err(format!("native source missing: {}", path.display()).into()); }
        paths.insert(*channel, path);
    }
    Ok((valid_unix, paths))
}

pub fn prune_source_archive(store_root: &Path, cutoff: DateTime<Utc>) -> Result<(usize, u64), Box<dyn Error>> {
    let root = store_root.join(SOURCE_ROOT);
    let index = root.join("index");
    if !index.is_dir() { return Ok((0, 0)); }
    let mut removed = 0;
    let mut bytes = 0;
    for path in json_files(&index)? {
        let frame: NativeSourceFrame = serde_json::from_slice(&fs::read(&path)?)?;
        if frame.start_unix >= cutoff.timestamp() { continue; }
        let data = safe_join(&root, &frame.relative_path)?;
        if let Ok(meta) = fs::metadata(&data) { bytes += meta.len(); }
        let _ = fs::remove_file(data);
        fs::remove_file(path)?;
        removed += 1;
    }
    Ok((removed, bytes))
}

fn read_index(store_root: &Path, platform: &str, sector: &str) -> Result<Vec<NativeSourceFrame>, Box<dyn Error>> {
    validate_token(platform)?;
    validate_token(sector)?;
    let dir = store_root.join(SOURCE_ROOT).join("index").join(platform).join(sector);
    if !dir.is_dir() { return Ok(Vec::new()); }
    let mut frames = Vec::new();
    for path in json_files(&dir)? {
        let frame: NativeSourceFrame = serde_json::from_slice(&fs::read(path)?)?;
        if frame.schema == SCHEMA { frames.push(frame); }
    }
    Ok(frames)
}

fn json_files(root: &Path) -> Result<Vec<PathBuf>, Box<dyn Error>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_symlink() { continue; }
            if kind.is_dir() { stack.push(entry.path()); }
            else if entry.path().extension().is_some_and(|ext| ext == "json") { files.push(entry.path()); }
        }
    }
    Ok(files)
}

fn validate_token(value: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
        return Err("unsafe satellite token".into());
    }
    Ok(())
}
fn validate_object_key(value: &str) -> Result<(), Box<dyn Error>> {
    if value.is_empty() || Path::new(value).components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err("unsafe satellite object key".into());
    }
    Ok(())
}
fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, Box<dyn Error>> {
    let path = Path::new(relative);
    if path.is_absolute() || path.components().any(|component| !matches!(component, Component::Normal(_))) {
        return Err("unsafe native source path".into());
    }
    Ok(root.join(path))
}
fn sector_slug(sector: &crate::abi::AbiSector) -> String {
    match sector {
        crate::abi::AbiSector::FullDisk => "fulldisk",
        crate::abi::AbiSector::Conus => "conus",
        crate::abi::AbiSector::Mesoscale1 => "meso1",
        crate::abi::AbiSector::Mesoscale2 => "meso2",
        crate::abi::AbiSector::Mesoscale => "meso",
        crate::abi::AbiSector::Unknown(value) => value.as_str(),
    }.to_string()
}
''',
)

write(
    "crates/rw-server/src/satellite.rs",
    r'''
use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{TimeZone, Utc};
use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
use rw_sat::abi::{GoesAbiField, GoesAbiScene, read_goes_abi_field_window, read_goes_abi_scene};
use rw_sat::composite::{bilinear_f32, bracket_axis};
use rw_sat::enhancement::render_product_pixel;
use rw_sat::geostationary::lat_lon_to_scan_angles_fast;
use rw_sat::products::{SatelliteSector, product_by_slug, product_catalog};
use rw_sat::source_catalog::{list_product_frames, resolve_product_sources};
use serde::{Deserialize, Serialize};

use crate::AppState;

const TILE_SIZE: usize = 256;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/v1/satellite/catalog", get(catalog))
        .route("/v1/satellite/{platform}/{sector}/{product}/frames", get(frames))
        .route("/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json", get(tilejson))
        .route("/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}.png", get(tile))
}

#[derive(Debug, Serialize)]
struct SectorDto { slug: &'static str, title: &'static str, cadence_seconds: u64 }
#[derive(Debug, Serialize)]
struct ProductDto {
    slug: &'static str,
    title: &'static str,
    description: &'static str,
    category: rw_sat::products::ProductCategory,
    base_channel: u8,
    required_channels: &'static [u8],
    nominal_resolution_km: f32,
    daylight_only: bool,
}
#[derive(Debug, Serialize)]
struct CatalogDto { schema: &'static str, platforms: [&'static str; 4], sectors: Vec<SectorDto>, products: Vec<ProductDto> }

async fn catalog() -> Json<CatalogDto> {
    Json(CatalogDto {
        schema: "rw-server.satellite-catalog.v2",
        platforms: ["g16", "g17", "g18", "g19"],
        sectors: SatelliteSector::ALL.into_iter().map(|sector| SectorDto {
            slug: sector.slug(), title: sector.title(), cadence_seconds: sector.cadence_seconds(),
        }).collect(),
        products: product_catalog().into_iter().map(|product| ProductDto {
            slug: product.slug, title: product.title, description: product.description,
            category: product.category, base_channel: product.base_channel,
            required_channels: product.required_channels,
            nominal_resolution_km: product.nominal_resolution_km,
            daylight_only: product.daylight_only,
        }).collect(),
    })
}

#[derive(Debug, Deserialize)]
struct ProductPath { platform: String, sector: String, product: String }
#[derive(Debug, Deserialize)]
struct FramePath { platform: String, sector: String, product: String, frame: String }
#[derive(Debug, Deserialize)]
struct TilePath { platform: String, sector: String, product: String, frame: String, z: u8, x: u32, y: u32 }
#[derive(Debug, Deserialize)]
struct FrameQuery { limit: Option<usize> }

async fn frames(State(state): State<AppState>, Path(path): Path<ProductPath>, Query(query): Query<FrameQuery>) -> Response {
    let product = match validate_product_path(&path.platform, &path.sector, &path.product) { Ok(value) => value, Err(response) => return response };
    let root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector = normalized_sector(&path.sector).unwrap();
    match state.run_light(move || list_product_frames(&root, &platform, &sector, product.required_channels, query.limit.unwrap_or(288)).map_err(|e| e.to_string())).await {
        Ok(Ok(frames)) => Json(serde_json::json!({"schema":"rw-server.satellite-frames.v2","platform":platform,"sector":sector,"product":product.slug,"frames":frames})).into_response(),
        Ok(Err(error)) => api_error(StatusCode::BAD_REQUEST, &error),
        Err(_) => api_error(StatusCode::SERVICE_UNAVAILABLE, "satellite catalog is busy"),
    }
}

async fn tilejson(State(state): State<AppState>, Path(path): Path<FramePath>) -> Response {
    let product = match validate_product_path(&path.platform, &path.sector, &path.product) { Ok(value) => value, Err(response) => return response };
    let root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector = normalized_sector(&path.sector).unwrap();
    let requested = path.frame.clone();
    let resolved = match state.run_light(move || resolve_product_sources(&root, &platform, &sector, product.required_channels, &requested).map(|(time, _)| time).map_err(|e| e.to_string())).await {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => return api_error(StatusCode::NOT_FOUND, &error),
        Err(_) => return api_error(StatusCode::SERVICE_UNAVAILABLE, "satellite catalog is busy"),
    };
    let tiles = format!("/v1/satellite/{}/{}/{}/{}/tiles/{{z}}/{{x}}/{{y}}.png", path.platform, path.sector, product.slug, resolved);
    Json(serde_json::json!({
        "tilejson":"3.0.0", "name":format!("{} {} {}", path.platform, path.sector, product.title),
        "scheme":"xyz", "tiles":[tiles], "minzoom":0, "maxzoom":9,
        "attribution":"NOAA/NESDIS GOES ABI; rendered by Rusty Weather",
        "rw_valid_unix":resolved,
    })).into_response()
}

async fn tile(State(state): State<AppState>, Path(path): Path<TilePath>) -> Response {
    if path.z > 12 || path.x >= (1u32 << path.z) || path.y >= (1u32 << path.z) {
        return api_error(StatusCode::BAD_REQUEST, "invalid XYZ tile coordinate");
    }
    let product = match validate_product_path(&path.platform, &path.sector, &path.product) { Ok(value) => value, Err(response) => return response };
    let root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector = normalized_sector(&path.sector).unwrap();
    let requested = path.frame.clone();
    let result = state.run_heavy_sync(move || {
        let (valid_unix, sources) = resolve_product_sources(&root, &platform, &sector, product.required_channels, &requested).map_err(|e| e.to_string())?;
        render_tile(&product, valid_unix, &sources, path.z, path.x, path.y).map(|bytes| (valid_unix, bytes)).map_err(|e| e.to_string())
    }).await;
    match result {
        Ok(Ok((valid_unix, bytes))) => {
            let mut response = Response::new(Body::from(bytes));
            response.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            let cache = if path.frame.eq_ignore_ascii_case("latest") { "public, max-age=20" } else { "public, max-age=31536000, immutable" };
            response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
            if let Ok(value) = HeaderValue::from_str(&valid_unix.to_string()) { response.headers_mut().insert("x-rw-valid-unix", value); }
            response
        }
        Ok(Err(error)) => api_error(StatusCode::NOT_FOUND, &error),
        Err(_) => api_error(StatusCode::SERVICE_UNAVAILABLE, "satellite tile renderer is busy"),
    }
}

fn render_tile(product: &rw_sat::products::SatelliteProduct, valid_unix: i64, sources: &BTreeMap<u8, std::path::PathBuf>, z: u8, x: u32, y: u32) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let valid_time = Utc.timestamp_opt(valid_unix, 0).single().ok_or("invalid frame time")?;
    let mut channel_values = BTreeMap::<u8, Vec<f32>>::new();
    for (&channel, path) in sources {
        let scene = read_goes_abi_scene(path)?;
        channel_values.insert(channel, sample_scene_tile(path, &scene, z, x, y)?);
    }
    let mut image = RgbaImage::new(TILE_SIZE as u32, TILE_SIZE as u32);
    for py in 0..TILE_SIZE {
        for px in 0..TILE_SIZE {
            let (lat, lon) = tile_pixel_lat_lon(z, x, y, px, py);
            let index = py * TILE_SIZE + px;
            let color = render_product_pixel(product, lat, lon, valid_time, |channel| channel_values.get(&channel).and_then(|values| values.get(index)).copied());
            image.put_pixel(px as u32, py as u32, Rgba(color));
        }
    }
    let mut cursor = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image).write_to(&mut cursor, ImageFormat::Png)?;
    Ok(cursor.into_inner())
}

fn sample_scene_tile(path: &std::path::Path, scene: &GoesAbiScene, z: u8, x: u32, y: u32) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    let mut map = Vec::with_capacity(TILE_SIZE * TILE_SIZE);
    let mut min_x = usize::MAX; let mut min_y = usize::MAX; let mut max_x = 0usize; let mut max_y = 0usize;
    for py in 0..TILE_SIZE {
        for px in 0..TILE_SIZE {
            let (lat, lon) = tile_pixel_lat_lon(z, x, y, px, py);
            let bracket = lat_lon_to_scan_angles_fast(
                scene.projection.perspective_point_height_m, scene.projection.semi_major_axis_m,
                scene.projection.semi_minor_axis_m, scene.projection.longitude_of_projection_origin_deg,
                scene.projection.sweep_angle_axis, lat, lon,
            ).and_then(|(sx, sy)| {
                let xb = bracket_axis(&scene.fixed_grid.x_scan_rad, sx)?;
                let yb = bracket_axis(&scene.fixed_grid.y_scan_rad, sy)?;
                Some((xb, yb))
            });
            if let Some(((x0, x1, _), (y0, y1, _))) = bracket {
                min_x = min_x.min(x0); max_x = max_x.max(x1); min_y = min_y.min(y0); max_y = max_y.max(y1);
            }
            map.push(bracket);
        }
    }
    if min_x == usize::MAX { return Ok(vec![f32::NAN; TILE_SIZE * TILE_SIZE]); }
    let field = read_goes_abi_field_window(path, "CMI", min_x, max_x - min_x + 1, min_y, max_y - min_y + 1)?;
    let mut out = Vec::with_capacity(map.len());
    for bracket in map {
        let Some(((x0, x1, fx), (y0, y1, fy))) = bracket else { out.push(f32::NAN); continue; };
        let index = |xx: usize, yy: usize| (yy - min_y) * field.scene.fixed_grid.nx + (xx - min_x);
        out.push(bilinear_f32(field.values[index(x0,y0)], field.values[index(x1,y0)], field.values[index(x0,y1)], field.values[index(x1,y1)], fx, fy));
    }
    Ok(out)
}

fn tile_pixel_lat_lon(z: u8, x: u32, y: u32, px: usize, py: usize) -> (f64, f64) {
    let n = 2f64.powi(i32::from(z));
    let world_x = (f64::from(x) + (px as f64 + 0.5) / TILE_SIZE as f64) / n;
    let world_y = (f64::from(y) + (py as f64 + 0.5) / TILE_SIZE as f64) / n;
    let lon = world_x * 360.0 - 180.0;
    let mercator = std::f64::consts::PI * (1.0 - 2.0 * world_y);
    let lat = mercator.sinh().atan().to_degrees();
    (lat, lon)
}

fn validate_product_path(platform: &str, sector: &str, product: &str) -> Result<rw_sat::products::SatelliteProduct, Response> {
    let platform = platform.trim().to_ascii_lowercase().replace('-', "");
    if !matches!(platform.as_str(), "g16" | "goes16" | "g17" | "goes17" | "g18" | "goes18" | "g19" | "goes19") {
        return Err(api_error(StatusCode::BAD_REQUEST, "unsupported GOES platform"));
    }
    if normalized_sector(sector).is_none() { return Err(api_error(StatusCode::BAD_REQUEST, "unsupported satellite sector")); }
    product_by_slug(product).ok_or_else(|| api_error(StatusCode::NOT_FOUND, "unknown satellite product"))
}
fn normalized_sector(value: &str) -> Option<String> { SatelliteSector::parse(value).map(|sector| sector.slug().to_string()) }
fn api_error(status: StatusCode, detail: &str) -> Response { (status, Json(serde_json::json!({"error":detail}))).into_response() }
''',
)

# Wire rw-sat modules.
for module in ["enhancement", "preview", "products", "source_catalog"]:
    insert_module("crates/rw-sat/src/lib.rs", module)

# Replace follow decoder with bounded automatic preview + source archiving.
follow = ROOT / "crates/rw-sat/src/follow.rs"
text = follow.read_text(encoding="utf-8")
text = text.replace("use crate::abi::read_goes_abi_field;", "use crate::abi::read_goes_abi_scene;\nuse crate::preview::read_goes_abi_preview;\nuse crate::source_catalog::{archive_goes_source, prune_source_archive};")
text = text.replace("use crate::store::{WrittenFrame, downsample_field, frame_time, write_band_frame};", "use crate::store::{WrittenFrame, frame_time, write_band_frame};")
old = '''    let field = read_goes_abi_field(&download.path, "CMI").map_err(to_send_sync)?;
    let field = downsample_field(field, downsample);
    let frame = write_band_frame(store_root, &field, written_unix).map_err(to_send_sync)?;'''
new = '''    let scene = read_goes_abi_scene(&download.path).map_err(to_send_sync)?;
    archive_goes_source(store_root, bucket, object, &download.path, &scene).map_err(to_send_sync)?;
    let (field, preview_stride) = read_goes_abi_preview(&download.path, "CMI", downsample).map_err(to_send_sync)?;
    if preview_stride > 1 {
        sink(SatEvent::Info { message: format!("native source retained; desktop preview uses automatic 1/{preview_stride} stride") });
    }
    let frame = write_band_frame(store_root, &field, written_unix).map_err(to_send_sync)?;'''
if old not in text:
    raise SystemExit("follow decode anchor missing")
text = text.replace(old, new, 1)
cache_anchor = '''        if pruned.removed_files > 0 {
            sink(SatEvent::Info {
                message: format!(
                    "cache pruned: {} object(s), {} bytes",
                    pruned.removed_files, pruned.removed_bytes
                ),
            });
        }
'''
source_prune = cache_anchor + '''        match prune_source_archive(&config.store_root, cache_cutoff) {
            Ok((files, bytes)) if files > 0 => sink(SatEvent::Info {
                message: format!("native satellite archive pruned: {files} object(s), {bytes} bytes"),
            }),
            Ok(_) => {}
            Err(error) => sink(SatEvent::Warning { message: format!("native satellite archive prune: {error}") }),
        }
'''
if cache_anchor not in text:
    raise SystemExit("follow cache prune anchor missing")
text = text.replace(cache_anchor, source_prune, 1)
follow.write_text(text, encoding="utf-8")

# Make UI automatic and product-oriented.
ui_path = ROOT / "crates/rw-ui/src/panels/satellite.rs"
ui = ui_path.read_text(encoding="utf-8")
ui = ui.replace("downsample: 1,", "downsample: 0,", 1)
pattern = re.compile(r'''\s*ui\.label\("Detail"\);\n\s*ComboBox::from_id_salt\("rw-ui-sat-downsample"\).*?\n\s*\);\n''', re.DOTALL)
ui, count = pattern.subn('''            ui.label(RichText::new("Native source retained · preview detail selected automatically").small().weak());
''', ui, count=1)
if count == 0:
    # Keep compatibility if this panel changed; the default still activates auto mode.
    pass
ui_path.write_text(ui, encoding="utf-8")

worker_path = ROOT / "crates/rusty-weather-ui/src/sat_worker.rs"
worker = worker_path.read_text(encoding="utf-8")
worker = worker.replace("use rw_sat::composite::GoesAbiRgbCompositeStyle;", "use rw_sat::products::product_catalog;")
start = worker.find("pub fn layer_options() -> Vec<SatLayerOption> {")
end = worker.find("\n}\n\n/// Layer slug", start)
if start == -1 or end == -1:
    raise SystemExit("sat_worker layer_options anchor missing")
replacement = '''pub fn layer_options() -> Vec<SatLayerOption> {
    product_catalog()
        .into_iter()
        .map(|product| SatLayerOption {
            slug: product.slug.to_string(),
            label: product.title.to_string(),
            note: format!("{} · {} km nominal", product.description, product.nominal_resolution_km),
        })
        .collect()
'''
worker = worker[:start] + replacement + worker[end+2:]
resolve_start = worker.find("fn resolve_layer(layer: &str) -> Result<(Vec<u8>, String), String> {")
resolve_end = worker.find("\n}\n\n/// Validated pieces", resolve_start)
if resolve_start == -1 or resolve_end == -1:
    raise SystemExit("sat_worker resolve_layer anchor missing")
resolve = '''fn resolve_layer(layer: &str) -> Result<(Vec<u8>, String), String> {
    let product = rw_sat::products::product_by_slug(layer)
        .ok_or_else(|| format!("unknown satellite product '{layer}'"))?;
    Ok((product.required_channels.to_vec(), product.title.to_string()))
'''
worker = worker[:resolve_start] + resolve + worker[resolve_end+2:]
worker = worker.replace("if ![1usize, 2, 4].contains(&spec.downsample) {", "if ![0usize, 1, 2, 4, 8, 16, 32].contains(&spec.downsample) {")
worker = worker.replace('''    let detail = match spec.downsample {
        1 => String::new(),
        step => format!(" · 1/{step} res"),
    };''', '''    let detail = if spec.downsample == 0 {
        " · native source / automatic preview".to_string()
    } else if spec.downsample == 1 {
        String::new()
    } else {
        format!(" · preview 1/{}", spec.downsample)
    };''')
worker_path.write_text(worker, encoding="utf-8")

# Wire rw-server.
add_toml_dependency("crates/rw-server/Cargo.toml", "image", '{ version = "0.25", default-features = false, features = ["png"] }', 'http = "1.5.0"')
add_toml_dependency("crates/rw-server/Cargo.toml", "rw-sat", '{ path = "../rw-sat" }', 'rw-scheduler = { path = "../rw-scheduler" }')
insert_module("crates/rw-server/src/lib.rs", "satellite")

routes_path = ROOT / "crates/rw-server/src/routes.rs"
routes = routes_path.read_text(encoding="utf-8")
operational = routes.find("let operational = Router::new()")
if operational == -1:
    raise SystemExit("rw-server operational router anchor missing")
layer = routes.find(".route_layer(middleware::from_fn_with_state(", operational)
if layer == -1:
    raise SystemExit("rw-server operational route layer anchor missing")
if ".merge(crate::satellite::router())" not in routes[operational:layer]:
    routes = routes[:layer] + ".merge(crate::satellite::router())\n        " + routes[layer:]
routes_path.write_text(routes, encoding="utf-8")

write(
    "docs/satellite-v2.md",
    r'''
# Satellite v2

Rusty Weather treats the native geostationary file as the source of truth and a desktop `.rws` plane as a bounded preview. Users no longer choose internal `1/2` or `1/4` storage fractions.

## Coverage

GOES Full Disk, CONUS, Mesoscale 1, and Mesoscale 2 are first-class sectors. Nominal cadences are 10 minutes, 5 minutes, and 1 minute for each mesoscale sector. The product catalog includes every ABI channel plus GeoColor, Natural Color, clean/enhanced infrared, water-vapor products, Air Mass, Dust, Fire Temperature, cloud-phase/microphysics, Sandwich, shortwave IR, and ozone.

GeoColor is a rendered product rather than a list of component channels. It uses pseudo-natural visible color by day, smoothly transitions through twilight, and uses clean-window C13 infrared at night. It does not claim CIRA city-light data.

## Native source archive

Live GOES downloads are retained under `<store-root>/.rw-satellite-sources`. Sidecars contain platform, sector, channel, exact scan time, source key, and a safe relative path. Rolling retention prunes native files and sidecars together. Large high-resolution Full Disk channels are read in bounded windows for web tiles and decimated in bounded chunks only for the desktop preview.

## HTTP API

- `GET /v1/satellite/catalog`
- `GET /v1/satellite/{platform}/{sector}/{product}/frames`
- `GET /v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json`
- `GET /v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}.png`

`frame` may be `latest` or an immutable Unix scan timestamp. Fixed-frame tiles are immutable-cacheable. Tile rendering maps Web Mercator pixels directly into the native geostationary fixed grid and reads only the intersecting NetCDF window for each required channel.
''',
)

write(
    ".github/workflows/final-observations-satellite-ci.yml",
    r'''
name: Final observations and satellite CI

on:
  push:
    branches: [feat/final-observations-satellite-v2-cf0ca369]
  pull_request:
    branches: [codex/windows-fetch-lock-fix-20260814]

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always

jobs:
  validate:
    runs-on: ubuntu-24.04
    timeout-minutes: 120
    steps:
      - uses: actions/checkout@11d5960a326750d5838078e36cf38b85af677262
        with:
          persist-credentials: false
      - run: |
          sudo apt-get update
          sudo apt-get install -y --no-install-recommends pkg-config libgl1-mesa-dev libwayland-dev libx11-dev libxi-dev libxkbcommon-dev
      - run: rustup toolchain install 1.92.0 --profile minimal --component rustfmt --component clippy
      - run: cargo +1.92.0 fmt --all -- --check
      - run: cargo +1.92.0 check --locked --workspace --all-targets
      - run: cargo +1.92.0 test --locked -p rw-sat -p rw-observations -p rw-query -p rw-server
      - run: cargo +1.92.0 clippy --locked -p rw-sat -p rw-observations -p rw-query -p rw-server --all-targets --no-deps -- -D warnings
''',
)

# Clean obsolete bootstrap scaffolding when the script runs over an artifact checkout.
for obsolete in [
    ".github/workflows/port-observations-onto-cf0ca369.yml",
    ".github/workflows/export-final-source.yml",
    ".github/workflows/bootstrap-unified-observations.yml",
]:
    (ROOT / obsolete).unlink(missing_ok=True)
for directory in [ROOT / ".github/bootstrap", ROOT / ".github/port-final"]:
    if directory.exists():
        import shutil
        shutil.rmtree(directory)

print("satellite-v2 source port applied")

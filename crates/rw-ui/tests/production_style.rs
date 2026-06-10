//! Color-parity proof for the field viewer's production path: the texture
//! pixels the viewer builds equal the colors the PRODUCTION colormap maps
//! for the same values — for a direct plane (temperature_2m), a radar
//! product (composite_reflectivity), and a derived product (sbcape) — and
//! the legend tick values match the catalog definitions.

use egui::Color32;
use rustwx_core::{CanonicalField, FieldSelector, ModelId};
use rustwx_products::viewer::{StoreVariableStyle, operational_style_for_store_variable};
use rustwx_render::{build_colormap, colorbar_ticks};
use rw_ui::colormap::field_to_production_color_image;

fn style_for(var_name: &str, selector: &FieldSelector, units: &str) -> StoreVariableStyle {
    operational_style_for_store_variable(
        var_name,
        &serde_json::to_value(selector).expect("selector json"),
        units,
        ModelId::Hrrr,
    )
    .unwrap_or_else(|| panic!("'{var_name}' must resolve to a production style"))
}

fn derived_style(slug: &str, units: &str) -> StoreVariableStyle {
    operational_style_for_store_variable(
        slug,
        &serde_json::json!({ "derived": slug }),
        units,
        ModelId::Hrrr,
    )
    .unwrap_or_else(|| panic!("derived '{slug}' must resolve"))
}

/// The viewer texture pixel for each value equals the production
/// `LeveledColormap::map` color for that value (the rasterizer's own
/// function), with NaN transparent.
fn assert_pixels_match_production(style: &StoreVariableStyle, display_values: &[f32]) {
    let cmap = build_colormap(&style.scale, style.colormap_options);
    let image =
        field_to_production_color_image(display_values, display_values.len(), 1, &cmap, false);
    for (value, pixel) in display_values.iter().zip(&image.pixels) {
        let rgba = cmap.map(f64::from(*value));
        let expected = Color32::from_rgba_unmultiplied(rgba.r, rgba.g, rgba.b, rgba.a);
        assert_eq!(
            *pixel, expected,
            "value {value} must color exactly as production maps it"
        );
    }
}

#[test]
fn temperature_2m_texture_matches_the_production_palette() {
    let style = style_for(
        "temperature_2m",
        &FieldSelector::height_agl(CanonicalField::Temperature, 2),
        "K",
    );
    // Worker-converted display values (degF), spanning the -60..120 scale.
    let raw_kelvin = [233.15_f32, 273.15, 288.15, 305.15, f32::NAN];
    let display: Vec<f32> = raw_kelvin.iter().map(|&k| style.convert.apply(k)).collect();
    assert_pixels_match_production(&style, &display);

    // NaN stays distinct (transparent, never clamped into the ramp).
    let cmap = build_colormap(&style.scale, style.colormap_options);
    let image = field_to_production_color_image(&display, display.len(), 1, &cmap, false);
    assert_eq!(image.pixels[4].a(), 0, "NaN must be transparent");
    assert_ne!(image.pixels[0], image.pixels[3], "cold and hot differ");
}

#[test]
fn composite_reflectivity_texture_matches_the_radar_palette() {
    let style = style_for(
        "composite_reflectivity",
        &FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        "dBZ",
    );
    assert!(style.convert.is_none(), "dBZ needs no conversion");
    let values = [5.0_f32, 12.5, 32.5, 47.5, 62.5, f32::NAN];
    assert_pixels_match_production(&style, &values);

    // Below the 10 dBZ mask is transparent — the production no-echo look.
    let cmap = build_colormap(&style.scale, style.colormap_options);
    let image = field_to_production_color_image(&values, values.len(), 1, &cmap, false);
    assert_eq!(
        image.pixels[0].a(),
        0,
        "below mask_below(10) is transparent"
    );
    assert_ne!(image.pixels[2], image.pixels[3]);
    assert_ne!(image.pixels[3], image.pixels[4]);
}

#[test]
fn sbcape_texture_matches_the_cape_palette_with_the_250_mask() {
    let style = derived_style("sbcape", "J/kg");
    let values = [0.0_f32, 100.0, 500.0, 1500.0, 3500.0, f32::NAN];
    assert_pixels_match_production(&style, &values);

    let cmap = build_colormap(&style.scale, style.colormap_options);
    let image = field_to_production_color_image(&values, values.len(), 1, &cmap, false);
    assert_eq!(image.pixels[0].a(), 0, "CAPE below 250 is masked");
    assert_eq!(image.pixels[1].a(), 0, "CAPE below 250 is masked");
    assert_ne!(image.pixels[2], image.pixels[4], "scale varies with CAPE");
}

#[test]
fn legend_tick_values_match_the_catalog_definitions() {
    // sbcape: ticks every 500 J/kg from the scale's first level.
    let sbcape = derived_style("sbcape", "J/kg");
    let cmap = build_colormap(&sbcape.scale, sbcape.colormap_options);
    let ticks = colorbar_ticks(&cmap, sbcape.cbar_tick_step);
    let levels = sbcape.scale.resolved_discrete().levels;
    assert_eq!(ticks.first().copied(), levels.first().copied());
    assert!(ticks.windows(2).all(|w| (w[1] - w[0] - 500.0).abs() < 1e-9));

    // 2m dewpoint: the production Stepped legend with 10-degree ticks.
    let dewpoint = style_for(
        "dewpoint_2m",
        &FieldSelector::height_agl(CanonicalField::Dewpoint, 2),
        "K",
    );
    assert_eq!(dewpoint.cbar_tick_step, Some(10.0));
    let cmap = build_colormap(&dewpoint.scale, dewpoint.colormap_options);
    let ticks = colorbar_ticks(&cmap, dewpoint.cbar_tick_step);
    assert_eq!(ticks.first().copied(), Some(-40.0), "dewpoint scale floor");
    assert!(ticks.windows(2).all(|w| (w[1] - w[0] - 10.0).abs() < 1e-9));
    assert_eq!(ticks.last().copied(), Some(90.0), "dewpoint scale ceiling");

    // composite reflectivity: auto ticks span the 10..70 dBZ catalog levels.
    let reflectivity = style_for(
        "composite_reflectivity",
        &FieldSelector::entire_atmosphere(CanonicalField::CompositeReflectivity),
        "dBZ",
    );
    let cmap = build_colormap(&reflectivity.scale, reflectivity.colormap_options);
    let ticks = colorbar_ticks(&cmap, reflectivity.cbar_tick_step);
    assert_eq!(ticks.first().copied(), Some(10.0));
    assert_eq!(ticks.last().copied(), Some(70.0));
}

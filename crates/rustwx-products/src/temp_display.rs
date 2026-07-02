//! Fahrenheit/Celsius display selection for the surface temperature family.
//!
//! US fire-service map products default to Fahrenheit; `RUSTWX_TEMP_UNITS=c`
//! (plumbed from the render API body's `temp_units` field, the same way
//! `title_note` rides `RUSTWX_TITLE_SUFFIX`) opts a render back into
//! Celsius. The conversion is a pure display transform applied once, where
//! each lane finalizes its `MapRenderRequest`: the SAME affine map is
//! applied to the field values AND every color-scale level edge (and the
//! mask threshold), so every pixel keeps its exact color — only the numbers
//! printed along the colorbar and the units string change. Upper-air
//! temperature products are never routed through this transform
//! (meteorological convention keeps pressure-level temperatures in °C).

use rustwx_render::{ColorScale, MapRenderRequest};

/// Requested display convention for the surface temperature family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TempUnitsMode {
    /// The US fire-service default.
    #[default]
    Fahrenheit,
    /// The `temp_units: "c"` / `RUSTWX_TEMP_UNITS=c` opt-out.
    Celsius,
}

impl TempUnitsMode {
    /// Parse a request/env value. Empty means "default" (Fahrenheit);
    /// unknown strings are `None` so callers can reject them.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "f" | "degf" | "fahrenheit" => Some(Self::Fahrenheit),
            "c" | "degc" | "celsius" => Some(Self::Celsius),
            _ => None,
        }
    }

    /// The per-render-process override every map lane reads. Unset or
    /// unrecognized values keep the Fahrenheit default.
    pub fn from_env() -> Self {
        std::env::var("RUSTWX_TEMP_UNITS")
            .ok()
            .and_then(|value| Self::parse(&value))
            .unwrap_or_default()
    }
}

/// How a finalized map request currently expresses its temperature fill.
/// This is the lane-side classification that keeps the transform away from
/// non-temperature products, temperature *tendencies* (`degC/hr`
/// advection), stability indices (lifted index stays °C by convention),
/// and upper-air maps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempDisplay {
    /// An absolute temperature currently in °C (the derived and windowed
    /// surface families): °F = °C × 9/5 + 32.
    AbsoluteCelsius,
    /// A temperature *difference* currently in C degrees (dewpoint
    /// depression, temp/dewpoint range windows): Δ°F = Δ°C × 9/5 — no
    /// offset, a 0 °C spread is a 0 °F spread.
    DeltaCelsius,
    /// An absolute temperature already in °F (the direct 2 m lane's native
    /// display); converted only for the Celsius opt-out.
    AbsoluteFahrenheit,
}

/// Convert a finalized map request to the process-wide [`TempUnitsMode`]
/// (see [`apply_temp_units_display_with_mode`]).
pub fn apply_temp_units_display(request: &mut MapRenderRequest, current: TempDisplay) {
    apply_temp_units_display_with_mode(request, current, TempUnitsMode::from_env());
}

/// The single display transform behind the surface °F default. Applies the
/// SAME affine map to the fill values, every color-scale level edge, and
/// the mask threshold, so a pixel keeps its exact color; the colorbar just
/// reads in the requested unit. Colorbar ticks are re-anchored on round
/// numbers of the target unit (a 5 °C tick step becomes 10 °F ticks, not
/// labels every 9 °F from the scale floor).
pub fn apply_temp_units_display_with_mode(
    request: &mut MapRenderRequest,
    current: TempDisplay,
    mode: TempUnitsMode,
) {
    let (transform, units): (fn(f64) -> f64, &'static str) = match (current, mode) {
        (TempDisplay::AbsoluteCelsius, TempUnitsMode::Fahrenheit) => {
            (|c| c * 9.0 / 5.0 + 32.0, "degF")
        }
        (TempDisplay::DeltaCelsius, TempUnitsMode::Fahrenheit) => (|dc| dc * 9.0 / 5.0, "degF"),
        (TempDisplay::AbsoluteFahrenheit, TempUnitsMode::Celsius) => {
            (|f| (f - 32.0) * 5.0 / 9.0, "degC")
        }
        // Already displayed in the requested convention.
        (TempDisplay::AbsoluteCelsius | TempDisplay::DeltaCelsius, TempUnitsMode::Celsius)
        | (TempDisplay::AbsoluteFahrenheit, TempUnitsMode::Fahrenheit) => return,
    };

    for value in &mut request.field.values {
        *value = transform(f64::from(*value)) as f32;
    }
    request.field.units = units.to_string();

    let mut scale = request.scale.resolved_discrete();
    for level in &mut scale.levels {
        *level = transform(*level);
    }
    if let Some(mask_below) = scale.mask_below.as_mut() {
        *mask_below = transform(*mask_below);
    }
    let level_bounds = match (scale.levels.first(), scale.levels.last()) {
        (Some(&lo), Some(&hi)) => Some((lo, hi)),
        _ => None,
    };
    request.scale = ColorScale::Discrete(scale);

    if let Some(ticks) = request.cbar_ticks.as_mut() {
        // Explicit tick values live in level space: convert like levels.
        for tick in ticks.iter_mut() {
            *tick = transform(*tick);
        }
    } else if let Some(step) = request.cbar_tick_step.take() {
        // A tick STEP is a temperature difference: scale it, snap it to a
        // round number of the target unit, and re-anchor the ticks on
        // multiples of itself across the converted scale.
        let delta_factor = transform(1.0) - transform(0.0);
        let converted_step = nice_temperature_tick_step(step * delta_factor);
        let ticks = level_bounds
            .map(|(lo, hi)| tick_step_multiples(lo, hi, converted_step))
            .unwrap_or_default();
        if ticks.is_empty() {
            request.cbar_tick_step = Some(converted_step);
        } else {
            request.cbar_ticks = Some(ticks);
        }
    }
}

/// Snap a converted tick step to the nearest "round" temperature step:
/// 1 / 1.5 / 2 / 2.5 / 5 times a power of ten (9 → 10, 14.4 → 15,
/// 5.56 → 5, 1.8 → 2).
fn nice_temperature_tick_step(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 1.0;
    }
    let magnitude = 10f64.powf(value.log10().floor());
    let mut best = magnitude;
    let mut best_ratio = f64::INFINITY;
    for multiple in [1.0, 1.5, 2.0, 2.5, 5.0, 10.0] {
        let candidate = multiple * magnitude;
        let ratio = if candidate >= value {
            candidate / value
        } else {
            value / candidate
        };
        if ratio < best_ratio {
            best_ratio = ratio;
            best = candidate;
        }
    }
    best
}

/// Multiples of `step` covering `[lo, hi]` (the automatic picker's
/// anchoring, at our chosen step).
fn tick_step_multiples(lo: f64, hi: f64, step: f64) -> Vec<f64> {
    let mut ticks = Vec::new();
    if !(step > 0.0) || !lo.is_finite() || !hi.is_finite() || hi <= lo {
        return ticks;
    }
    let mut value = (lo / step).ceil() * step;
    while value <= hi + step * 0.01 {
        ticks.push(if value == 0.0 { 0.0 } else { value });
        value += step;
    }
    ticks
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_render::{
        Color, DiscreteColorScale, ExtendMode, Field2D, GridShape, LatLonGrid, ProductKey,
    };

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1.0e-9
    }

    fn sample_request(units: &str, values: Vec<f32>, levels: Vec<f64>) -> MapRenderRequest {
        let grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![35.0, 35.0, 36.0, 36.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap();
        let field = Field2D::new(ProductKey::named("temp_display_test"), units, grid, values)
            .unwrap();
        MapRenderRequest::new(
            field,
            ColorScale::Discrete(DiscreteColorScale {
                levels,
                colors: vec![
                    Color::rgba(0, 0, 255, 255),
                    Color::rgba(255, 0, 0, 255),
                ],
                extend: ExtendMode::Both,
                mask_below: None,
            }),
        )
    }

    #[test]
    fn temp_units_mode_parses_request_values_and_defaults_to_fahrenheit() {
        assert_eq!(TempUnitsMode::parse("f"), Some(TempUnitsMode::Fahrenheit));
        assert_eq!(TempUnitsMode::parse(" F "), Some(TempUnitsMode::Fahrenheit));
        assert_eq!(
            TempUnitsMode::parse("Fahrenheit"),
            Some(TempUnitsMode::Fahrenheit)
        );
        assert_eq!(TempUnitsMode::parse(""), Some(TempUnitsMode::Fahrenheit));
        assert_eq!(TempUnitsMode::parse("c"), Some(TempUnitsMode::Celsius));
        assert_eq!(TempUnitsMode::parse("degC"), Some(TempUnitsMode::Celsius));
        assert_eq!(TempUnitsMode::parse("kelvin"), None);
        assert_eq!(TempUnitsMode::default(), TempUnitsMode::Fahrenheit);
    }

    #[test]
    fn fahrenheit_default_converts_celsius_values_and_levels_identically() {
        let mut request = sample_request(
            "degC",
            vec![0.0, 20.0, 37.0, f32::NAN],
            vec![-40.0, 0.0, 40.0],
        );
        request.cbar_tick_step = Some(5.0);
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteCelsius,
            TempUnitsMode::Fahrenheit,
        );

        assert_eq!(request.field.units, "degF");
        assert!((request.field.values[0] - 32.0).abs() < 1.0e-4);
        assert!((request.field.values[1] - 68.0).abs() < 1.0e-4);
        assert!((request.field.values[2] - 98.6).abs() < 1.0e-4);
        assert!(request.field.values[3].is_nan());

        let ColorScale::Discrete(scale) = &request.scale else {
            panic!("expected discrete scale");
        };
        assert!(approx(scale.levels[0], -40.0)); // -40 is where the scales cross
        assert!(approx(scale.levels[1], 32.0));
        assert!(approx(scale.levels[2], 104.0));

        // A 5 degC step becomes round 10 degF ticks anchored on multiples.
        assert_eq!(request.cbar_tick_step, None);
        let ticks = request.cbar_ticks.as_ref().expect("re-anchored ticks");
        assert!(approx(ticks[0], -40.0));
        assert!(ticks.iter().any(|tick| approx(*tick, 0.0)));
        assert!(approx(*ticks.last().unwrap(), 100.0));
    }

    #[test]
    fn value_and_level_conversion_preserves_bin_membership() {
        // Visual identity: a value keeps the same bin (same color) after
        // both it and the level edges are converted.
        let levels_c: Vec<f64> = (-40..=40).map(f64::from).collect();
        let values_c = [-12.3_f32, 0.4, 17.9, 31.2];
        let mut request = sample_request("degC", values_c.to_vec(), levels_c.clone());
        let bin = |value: f32, levels: &[f64]| {
            levels
                .iter()
                .take_while(|level| **level <= f64::from(value))
                .count()
        };
        let bins_before: Vec<usize> = values_c
            .iter()
            .map(|value| bin(*value, &levels_c))
            .collect();
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteCelsius,
            TempUnitsMode::Fahrenheit,
        );
        let ColorScale::Discrete(scale) = &request.scale else {
            panic!("expected discrete scale");
        };
        let bins_after: Vec<usize> = request
            .field
            .values
            .iter()
            .map(|value| bin(*value, &scale.levels))
            .collect();
        assert_eq!(bins_before, bins_after);
    }

    #[test]
    fn celsius_mode_keeps_celsius_products_untouched() {
        let mut request = sample_request("degC", vec![10.0; 4], vec![-40.0, 0.0, 40.0]);
        request.cbar_tick_step = Some(5.0);
        let scale_before = request.scale.clone();
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteCelsius,
            TempUnitsMode::Celsius,
        );
        assert_eq!(request.field.units, "degC");
        assert_eq!(request.field.values, vec![10.0; 4]);
        assert_eq!(request.scale, scale_before);
        assert_eq!(request.cbar_tick_step, Some(5.0));
        assert_eq!(request.cbar_ticks, None);
    }

    #[test]
    fn celsius_opt_out_converts_the_direct_lane_fahrenheit_display() {
        let mut request = sample_request(
            "degF",
            vec![32.0, 212.0, 68.0, 75.0],
            vec![-60.0, 32.0, 120.0],
        );
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteFahrenheit,
            TempUnitsMode::Celsius,
        );
        assert_eq!(request.field.units, "degC");
        assert!((request.field.values[0] - 0.0).abs() < 1.0e-4);
        assert!((request.field.values[1] - 100.0).abs() < 1.0e-4);
        let ColorScale::Discrete(scale) = &request.scale else {
            panic!("expected discrete scale");
        };
        assert!(approx(scale.levels[1], 0.0));
        assert!(approx(scale.levels[2], 48.888_888_888_888_886));
    }

    #[test]
    fn fahrenheit_default_keeps_the_direct_lane_fahrenheit_display_untouched() {
        let mut request = sample_request("degF", vec![75.0; 4], vec![-60.0, 120.0]);
        let scale_before = request.scale.clone();
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteFahrenheit,
            TempUnitsMode::Fahrenheit,
        );
        assert_eq!(request.field.units, "degF");
        assert_eq!(request.field.values, vec![75.0; 4]);
        assert_eq!(request.scale, scale_before);
    }

    #[test]
    fn delta_conversion_scales_without_offset_and_converts_masks() {
        let mut request = sample_request(
            "degC",
            vec![0.0, 10.0, 20.0, 30.0],
            vec![0.0, 4.0, 8.0, 40.0],
        );
        if let ColorScale::Discrete(scale) = &mut request.scale {
            scale.mask_below = Some(4.0);
        }
        request.cbar_tick_step = Some(8.0);
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::DeltaCelsius,
            TempUnitsMode::Fahrenheit,
        );
        assert_eq!(request.field.units, "degF");
        assert!((request.field.values[0] - 0.0).abs() < 1.0e-4); // no +32 offset
        assert!((request.field.values[1] - 18.0).abs() < 1.0e-4);
        assert!((request.field.values[2] - 36.0).abs() < 1.0e-4);
        let ColorScale::Discrete(scale) = &request.scale else {
            panic!("expected discrete scale");
        };
        assert!(approx(scale.levels[1], 7.2));
        assert!(approx(scale.levels[3], 72.0));
        assert!(approx(scale.mask_below.unwrap(), 7.2));
        // 8 degC step -> 14.4 -> round 15 degF ticks.
        let ticks = request.cbar_ticks.as_ref().expect("re-anchored ticks");
        assert!(approx(ticks[0], 0.0));
        assert!(approx(ticks[1], 15.0));
    }

    #[test]
    fn explicit_ticks_convert_like_levels() {
        let mut request = sample_request("degC", vec![0.0; 4], vec![-40.0, 40.0]);
        request.cbar_ticks = Some(vec![-40.0, 0.0, 40.0]);
        apply_temp_units_display_with_mode(
            &mut request,
            TempDisplay::AbsoluteCelsius,
            TempUnitsMode::Fahrenheit,
        );
        let ticks = request.cbar_ticks.as_ref().unwrap();
        assert!(approx(ticks[0], -40.0));
        assert!(approx(ticks[1], 32.0));
        assert!(approx(ticks[2], 104.0));
    }

    #[test]
    fn tick_steps_snap_to_round_target_unit_steps() {
        assert!(approx(nice_temperature_tick_step(9.0), 10.0));
        assert!(approx(nice_temperature_tick_step(14.4), 15.0));
        assert!(approx(nice_temperature_tick_step(1.8), 2.0));
        assert!(approx(nice_temperature_tick_step(0.9), 1.0));
        assert!(approx(nice_temperature_tick_step(10.0 / 1.8), 5.0));
        assert!(approx(nice_temperature_tick_step(f64::NAN), 1.0));
    }
}

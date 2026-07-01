use rustwx_core::{CanonicalField, SelectedField2D, VerticalSelector};
use rustwx_render::{
    Color, ContourLayer, ContourLinePattern, GridShape, MapRenderRequest, RgbaGridField,
};

pub fn selected_orography_field(
    extracted: &std::collections::HashMap<rustwx_core::FieldSelector, SelectedField2D>,
) -> Option<&SelectedField2D> {
    extracted.values().find(|field| {
        field.selector.field == CanonicalField::GeopotentialHeight
            && matches!(field.selector.vertical, VerticalSelector::Surface)
    })
}

pub fn apply_orography_topo_overlay(
    request: &mut MapRenderRequest,
    orography: &SelectedField2D,
) -> Result<(), Box<dyn std::error::Error>> {
    let nx = request.field.grid.shape.nx;
    let ny = request.field.grid.shape.ny;
    if orography.grid.shape.nx != nx
        || orography.grid.shape.ny != ny
        || orography.values.len() != request.field.values.len()
    {
        return Ok(());
    }

    let terrain_grid = rustwx_render::LatLonGrid::new(
        GridShape::new(nx, ny)?,
        orography.grid.lat_deg.clone(),
        orography.grid.lon_deg.clone(),
    )?;
    request.set_terrain_rgba_grid(RgbaGridField::new(
        terrain_grid,
        terrain_rgba_pixels(&orography.values, nx, ny),
    )?);

    if let Some(levels) = terrain_contour_levels(&orography.values) {
        request.contours.push(ContourLayer {
            data: orography.values.clone(),
            levels,
            color: Color::rgba(92, 82, 64, 96),
            width: 1,
            labels: false,
            show_extrema: false,
            pattern: ContourLinePattern::Solid,
            major_every: Some(4),
            major_width: Some(1),
        });
    }

    Ok(())
}

fn terrain_rgba_pixels(values: &[f32], nx: usize, ny: usize) -> Vec<Color> {
    let (min_elev, max_elev) = finite_range(values).unwrap_or((0.0, 3000.0));
    let high = max_elev.max(1800.0);
    let low = min_elev.min(0.0);
    let span = (high - low).max(1.0);

    let mut pixels = Vec::with_capacity(values.len());
    for j in 0..ny {
        for i in 0..nx {
            let idx = j * nx + i;
            let elev = values[idx];
            if !elev.is_finite() {
                pixels.push(Color::TRANSPARENT);
                continue;
            }
            let t = ((elev - low) / span).clamp(0.0, 1.0).powf(0.72);
            let (r, g, b) = hypsometric_rgb(t);
            let shade = hillshade(values, nx, ny, i, j);
            pixels.push(Color::rgba(
                shade_channel(r, shade),
                shade_channel(g, shade),
                shade_channel(b, shade),
                174,
            ));
        }
    }
    pixels
}

fn finite_range(values: &[f32]) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &value in values {
        if value.is_finite() {
            min = min.min(value);
            max = max.max(value);
        }
    }
    min.is_finite().then_some((min, max))
}

fn hypsometric_rgb(t: f32) -> (u8, u8, u8) {
    const STOPS: &[(f32, (u8, u8, u8))] = &[
        (0.00, (218, 222, 188)),
        (0.24, (194, 211, 158)),
        (0.48, (190, 177, 134)),
        (0.72, (158, 145, 127)),
        (1.00, (232, 232, 224)),
    ];
    for window in STOPS.windows(2) {
        let (left_t, left) = window[0];
        let (right_t, right) = window[1];
        if t <= right_t {
            let local = ((t - left_t) / (right_t - left_t)).clamp(0.0, 1.0);
            return (
                lerp_u8(left.0, right.0, local),
                lerp_u8(left.1, right.1, local),
                lerp_u8(left.2, right.2, local),
            );
        }
    }
    STOPS.last().map(|(_, rgb)| *rgb).unwrap_or((220, 220, 210))
}

fn hillshade(values: &[f32], nx: usize, ny: usize, i: usize, j: usize) -> f32 {
    let center = values[j * nx + i];
    if !center.is_finite() || nx < 3 || ny < 3 {
        return 1.0;
    }
    let sample = |x: usize, y: usize| {
        let value = values[y * nx + x];
        if value.is_finite() { value } else { center }
    };
    let west = sample(i.saturating_sub(1), j);
    let east = sample((i + 1).min(nx - 1), j);
    let north = sample(i, j.saturating_sub(1));
    let south = sample(i, (j + 1).min(ny - 1));
    let dzdx = (east - west) / 2.0;
    let dzdy = (south - north) / 2.0;
    let normal_x = -dzdx / 850.0;
    let normal_y = dzdy / 850.0;
    let normal_z = 1.0;
    let norm = (normal_x * normal_x + normal_y * normal_y + normal_z * normal_z).sqrt();
    let (nxv, nyv, nzv) = (normal_x / norm, normal_y / norm, normal_z / norm);
    let light = (-0.55_f32, -0.62_f32, 0.56_f32);
    let light_norm = (light.0 * light.0 + light.1 * light.1 + light.2 * light.2).sqrt();
    let dot =
        (nxv * light.0 / light_norm + nyv * light.1 / light_norm + nzv * light.2 / light_norm)
            .clamp(-0.35, 1.0);
    (0.76 + dot * 0.34).clamp(0.58, 1.24)
}

fn shade_channel(value: u8, shade: f32) -> u8 {
    ((value as f32 * shade).round()).clamp(0.0, 255.0) as u8
}

fn lerp_u8(left: u8, right: u8, t: f32) -> u8 {
    (left as f32 + (right as f32 - left as f32) * t).round() as u8
}

fn terrain_contour_levels(values: &[f32]) -> Option<Vec<f64>> {
    let (min, max) = finite_range(values)?;
    let range = max - min;
    if range < 80.0 {
        return None;
    }
    let step = if range > 2800.0 {
        500.0
    } else if range > 1200.0 {
        250.0
    } else {
        100.0
    };
    let mut current = (min / step).floor() * step;
    let end = (max / step).ceil() * step;
    let mut levels = Vec::new();
    while current <= end {
        if current >= 0.0 {
            levels.push(current as f64);
        }
        current += step;
    }
    (!levels.is_empty()).then_some(levels)
}

pub fn basemap_style_env_is_topo() -> bool {
    std::env::var("RUSTWX_BASEMAP_STYLE")
        .ok()
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "topo" | "topographic" | "terrain" | "terrain-tint" | "relief"
            )
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terrain_pixels_show_hillshade_variation() {
        let values = vec![0.0, 100.0, 200.0, 300.0, 400.0, 500.0, 600.0, 700.0, 800.0];
        let pixels = terrain_rgba_pixels(&values, 3, 3);
        assert_eq!(pixels.len(), values.len());
        assert!(pixels.iter().any(|pixel| pixel.a > 0));
        assert!(pixels.windows(2).any(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn terrain_contours_choose_reasonable_levels() {
        let levels = terrain_contour_levels(&[0.0, 500.0, 1500.0, 3000.0]).unwrap();
        assert!(levels.contains(&500.0));
        assert!(levels.contains(&3000.0));
    }
}

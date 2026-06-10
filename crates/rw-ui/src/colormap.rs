//! Minimal false-color mapping for the data viewer: a linear ramp over
//! `[vmin, vmax]` sampled from a small set of anchor colors.
//!
//! This is a data-inspection aid, NOT the production render palette — the
//! plot pipeline owns real styling. Keep it dumb and predictable.

use egui::{Color32, ColorImage};

/// Color used for NaN / missing values (dark neutral, clearly not data).
pub const MISSING_COLOR: Color32 = Color32::from_rgb(28, 28, 32);

/// A piecewise-linear colormap over evenly spaced RGB anchors. Defaults to
/// [`VIRIDIS`].
#[derive(Debug, Clone, Copy)]
pub struct Colormap {
    anchors: &'static [[u8; 3]],
}

impl Default for Colormap {
    fn default() -> Self {
        VIRIDIS
    }
}

/// Viridis-like default ramp (9 anchors, perceptually ordered dark -> bright).
pub const VIRIDIS: Colormap = Colormap {
    anchors: &[
        [68, 1, 84],
        [72, 40, 120],
        [62, 74, 137],
        [49, 104, 142],
        [38, 130, 142],
        [31, 158, 137],
        [53, 183, 121],
        [109, 205, 89],
        [253, 231, 37],
    ],
};

impl Colormap {
    /// Sample the ramp at `t` in `[0, 1]` (clamped). NaN maps to
    /// [`MISSING_COLOR`].
    pub fn sample(&self, t: f32) -> Color32 {
        if t.is_nan() {
            return MISSING_COLOR;
        }
        let t = t.clamp(0.0, 1.0);
        let last = self.anchors.len() - 1;
        let scaled = t * last as f32;
        let lo = (scaled.floor() as usize).min(last);
        let hi = (lo + 1).min(last);
        let frac = scaled - lo as f32;
        let lerp = |a: u8, b: u8| -> u8 {
            (a as f32 + (b as f32 - a as f32) * frac).round().clamp(0.0, 255.0) as u8
        };
        let a = self.anchors[lo];
        let b = self.anchors[hi];
        Color32::from_rgb(lerp(a[0], b[0]), lerp(a[1], b[1]), lerp(a[2], b[2]))
    }
}

/// `(min, max)` over the finite values of `values`, or `None` if every value
/// is NaN (or the slice is empty).
pub fn finite_min_max(values: &[f32]) -> Option<(f32, f32)> {
    let mut range: Option<(f32, f32)> = None;
    for &v in values {
        if !v.is_finite() {
            continue;
        }
        range = Some(match range {
            None => (v, v),
            Some((lo, hi)) => (lo.min(v), hi.max(v)),
        });
    }
    range
}

/// Normalized position of `value` inside `[vmin, vmax]`. NaN passes through
/// as NaN (so [`Colormap::sample`] paints it [`MISSING_COLOR`]); a degenerate
/// range (`vmin >= vmax`) maps every finite value to 0.5.
pub fn normalize(value: f32, vmin: f32, vmax: f32) -> f32 {
    if value.is_nan() {
        return f32::NAN;
    }
    if vmin.is_nan() || vmax.is_nan() || vmax <= vmin {
        return 0.5;
    }
    ((value - vmin) / (vmax - vmin)).clamp(0.0, 1.0)
}

/// Build a false-color [`ColorImage`] from a row-major `ny * nx` field.
///
/// With `flip_y` the grid's row 0 lands at the BOTTOM of the image — needed
/// to display south-to-north storage north-up. Storage order varies per
/// grid, so DERIVE the flag from the lat axis
/// (`rw_store::grid::GridFile::lat_descending`); never assume a model
/// convention.
pub fn field_to_color_image(
    values: &[f32],
    nx: usize,
    ny: usize,
    vmin: f32,
    vmax: f32,
    cmap: &Colormap,
    flip_y: bool,
) -> ColorImage {
    assert_eq!(values.len(), nx * ny, "field size must be ny * nx");
    let mut pixels = Vec::with_capacity(nx * ny);
    for image_row in 0..ny {
        let grid_row = if flip_y { ny - 1 - image_row } else { image_row };
        let row = &values[grid_row * nx..(grid_row + 1) * nx];
        for &v in row {
            pixels.push(cmap.sample(normalize(v, vmin, vmax)));
        }
    }
    ColorImage::new([nx, ny], pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_hits_endpoints_and_clamps() {
        let first = VIRIDIS.anchors[0];
        let last = *VIRIDIS.anchors.last().unwrap();
        assert_eq!(VIRIDIS.sample(0.0), Color32::from_rgb(first[0], first[1], first[2]));
        assert_eq!(VIRIDIS.sample(1.0), Color32::from_rgb(last[0], last[1], last[2]));
        // Out-of-range clamps to the endpoints; NaN is the missing color.
        assert_eq!(VIRIDIS.sample(-3.0), VIRIDIS.sample(0.0));
        assert_eq!(VIRIDIS.sample(7.0), VIRIDIS.sample(1.0));
        assert_eq!(VIRIDIS.sample(f32::NAN), MISSING_COLOR);
    }

    #[test]
    fn sample_is_monotonic_in_brightness() {
        // Viridis brightness (rough luma) increases with t; 32 steps must
        // never decrease. Guards against anchor-order/interpolation bugs.
        let luma = |c: Color32| 0.299 * c.r() as f32 + 0.587 * c.g() as f32 + 0.114 * c.b() as f32;
        let mut prev = luma(VIRIDIS.sample(0.0));
        for i in 1..=32 {
            let next = luma(VIRIDIS.sample(i as f32 / 32.0));
            assert!(
                next >= prev - 1.0, // allow rounding jitter
                "luma must not decrease: step {i} went {prev} -> {next}"
            );
            prev = next;
        }
    }

    #[test]
    fn finite_min_max_skips_nan() {
        assert_eq!(finite_min_max(&[]), None);
        assert_eq!(finite_min_max(&[f32::NAN, f32::NAN]), None);
        assert_eq!(finite_min_max(&[2.0]), Some((2.0, 2.0)));
        assert_eq!(
            finite_min_max(&[f32::NAN, 3.0, -1.5, f32::NAN, 0.0]),
            Some((-1.5, 3.0))
        );
    }

    #[test]
    fn normalize_handles_degenerate_range_and_nan() {
        assert_eq!(normalize(5.0, 0.0, 10.0), 0.5);
        assert_eq!(normalize(0.0, 0.0, 10.0), 0.0);
        assert_eq!(normalize(10.0, 0.0, 10.0), 1.0);
        assert_eq!(normalize(-99.0, 0.0, 10.0), 0.0, "clamps below");
        assert_eq!(normalize(99.0, 0.0, 10.0), 1.0, "clamps above");
        assert_eq!(normalize(7.0, 4.0, 4.0), 0.5, "degenerate range -> mid");
        assert!(normalize(f32::NAN, 0.0, 1.0).is_nan());
    }

    #[test]
    fn color_image_maps_values_and_flips() {
        // 2 x 2 field: row 0 = [min, NaN], row 1 = [max, mid].
        let values = [0.0, f32::NAN, 10.0, 5.0];
        let img = field_to_color_image(&values, 2, 2, 0.0, 10.0, &VIRIDIS, false);
        assert_eq!(img.size, [2, 2]);
        assert_eq!(img.pixels[0], VIRIDIS.sample(0.0), "min -> ramp start");
        assert_eq!(img.pixels[1], MISSING_COLOR, "NaN -> missing color");
        assert_eq!(img.pixels[2], VIRIDIS.sample(1.0), "max -> ramp end");
        assert_eq!(img.pixels[3], VIRIDIS.sample(0.5), "mid -> ramp middle");

        // flip_y swaps the rows (grid row 0 moves to the image bottom).
        let flipped = field_to_color_image(&values, 2, 2, 0.0, 10.0, &VIRIDIS, true);
        assert_eq!(flipped.pixels[0], img.pixels[2]);
        assert_eq!(flipped.pixels[1], img.pixels[3]);
        assert_eq!(flipped.pixels[2], img.pixels[0]);
        assert_eq!(flipped.pixels[3], img.pixels[1]);
    }
}

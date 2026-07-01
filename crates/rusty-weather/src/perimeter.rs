#![allow(dead_code)]

//! Perimeter -> render-domain math for fire-perimeter plots.
//!
//! Converts a lon/lat fire perimeter into draw-a-box bounds through a local
//! equirectangular projection centered on the perimeter centroid, so all
//! padding/extension arithmetic happens in kilometers (docs/
//! FABLE_FIRE_WEATHER_HANDOFF.md, "Perimeter bounds algorithm"). The API
//! lane feeds the result to the existing `--domain-bounds` render path.

/// Kilometers per degree of latitude on the mean sphere (R = 6371 km).
const KM_PER_DEG_LAT: f64 = 111.194_926_644_558_74;

/// Hard geographic sanity limits for perimeter points.
const MAX_ABS_LAT_DEG: f64 = 85.0;
const MAX_PADDING_KM: f64 = 300.0;
const MAX_EXTEND_KM: f64 = 500.0;

/// One-sided domain extension toward an expected spread/wind bearing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerimeterExtension {
    /// Compass bearing the domain grows toward, degrees clockwise from
    /// north (0 = extend north, 90 = extend east).
    pub direction_deg: f64,
    pub distance_km: f64,
}

/// Controls for [`perimeter_domain_bounds`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PerimeterDomainOptions {
    /// Padding applied to every side before extension/fit, in km.
    pub padding_km: f64,
    pub extend: Option<PerimeterExtension>,
    /// Target width/height ratio in projected km; the shorter axis grows
    /// (never shrinks) to match. `None` keeps the natural padded box.
    pub aspect: Option<f64>,
    /// Minimum final span per axis, so tiny fires still get a usable map.
    pub min_span_km: f64,
    /// Maximum final span per axis, guarding against accidental huge input.
    pub max_span_km: f64,
}

impl Default for PerimeterDomainOptions {
    fn default() -> Self {
        Self {
            padding_km: 50.0,
            extend: None,
            aspect: None,
            min_span_km: 40.0,
            max_span_km: 1200.0,
        }
    }
}

/// Compute `[west, east, south, north]` render bounds around a lon/lat
/// perimeter ring (an explicitly closed ring is accepted; the duplicate
/// closing point is ignored).
pub fn perimeter_domain_bounds(
    points: &[(f64, f64)],
    opts: &PerimeterDomainOptions,
) -> Result<[f64; 4], String> {
    validate_options(opts)?;
    let ring = open_ring(points);
    if ring.len() < 3 {
        return Err(format!(
            "perimeter needs at least 3 distinct points, got {}",
            ring.len()
        ));
    }
    for (index, (lon, lat)) in ring.iter().enumerate() {
        if !lon.is_finite() || !lat.is_finite() {
            return Err(format!("perimeter point {index} is not finite"));
        }
        if *lon < -180.0 || *lon > 180.0 || lat.abs() > MAX_ABS_LAT_DEG {
            return Err(format!(
                "perimeter point {index} ({lon}, {lat}) is outside lon [-180, 180] / lat [-{MAX_ABS_LAT_DEG}, {MAX_ABS_LAT_DEG}]"
            ));
        }
    }

    let lon0 = ring.iter().map(|(lon, _)| lon).sum::<f64>() / ring.len() as f64;
    let lat0 = ring.iter().map(|(_, lat)| lat).sum::<f64>() / ring.len() as f64;
    let cos0 = lat0.to_radians().cos();
    if cos0 <= 0.05 {
        return Err(format!(
            "perimeter centroid latitude {lat0:.2} is too close to the pole for a local projection"
        ));
    }
    let km_per_deg_lon = KM_PER_DEG_LAT * cos0;

    // Perimeter extent in local tangent km, centered on the centroid.
    let mut min_x = f64::INFINITY;
    let mut max_x = f64::NEG_INFINITY;
    let mut min_y = f64::INFINITY;
    let mut max_y = f64::NEG_INFINITY;
    for (lon, lat) in ring {
        let x = (lon - lon0) * km_per_deg_lon;
        let y = (lat - lat0) * KM_PER_DEG_LAT;
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }

    min_x -= opts.padding_km;
    max_x += opts.padding_km;
    min_y -= opts.padding_km;
    max_y += opts.padding_km;

    if let Some(extend) = opts.extend {
        let bearing = extend.direction_deg.to_radians();
        let dx = bearing.sin() * extend.distance_km;
        let dy = bearing.cos() * extend.distance_km;
        if dx >= 0.0 {
            max_x += dx;
        } else {
            min_x += dx;
        }
        if dy >= 0.0 {
            max_y += dy;
        } else {
            min_y += dy;
        }
    }

    (min_x, max_x) = grow_to_min_span(min_x, max_x, opts.min_span_km);
    (min_y, max_y) = grow_to_min_span(min_y, max_y, opts.min_span_km);

    if let Some(aspect) = opts.aspect {
        let width = max_x - min_x;
        let height = max_y - min_y;
        if width < height * aspect {
            (min_x, max_x) = grow_to_min_span(min_x, max_x, height * aspect);
        } else if height < width / aspect {
            (min_y, max_y) = grow_to_min_span(min_y, max_y, width / aspect);
        }
    }

    // The max-span clamp is the hard safety and may override the aspect fit.
    (min_x, max_x) = clamp_to_max_span(min_x, max_x, opts.max_span_km);
    (min_y, max_y) = clamp_to_max_span(min_y, max_y, opts.max_span_km);

    let west = (lon0 + min_x / km_per_deg_lon).clamp(-180.0, 180.0);
    let east = (lon0 + max_x / km_per_deg_lon).clamp(-180.0, 180.0);
    let south = (lat0 + min_y / KM_PER_DEG_LAT).clamp(-MAX_ABS_LAT_DEG, MAX_ABS_LAT_DEG);
    let north = (lat0 + max_y / KM_PER_DEG_LAT).clamp(-MAX_ABS_LAT_DEG, MAX_ABS_LAT_DEG);
    if west >= east || south >= north {
        return Err(format!(
            "perimeter domain collapsed after geographic clamping: [{west}, {east}, {south}, {north}]"
        ));
    }
    Ok([west, east, south, north])
}

/// Stable content hash of the perimeter ring for render-cache keys
/// (FNV-1a over the coordinate bit patterns; no dependency needed).
pub fn perimeter_hash(points: &[(f64, f64)]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for (lon, lat) in points {
        for bits in [lon.to_bits(), lat.to_bits()] {
            for byte in bits.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
    }
    format!("{hash:016x}")
}

fn validate_options(opts: &PerimeterDomainOptions) -> Result<(), String> {
    if !opts.padding_km.is_finite() || opts.padding_km < 0.0 || opts.padding_km > MAX_PADDING_KM {
        return Err(format!(
            "padding_km must be within 0..={MAX_PADDING_KM} km, got {}",
            opts.padding_km
        ));
    }
    if let Some(extend) = opts.extend {
        if !extend.direction_deg.is_finite() {
            return Err("extend.direction_deg must be finite".to_string());
        }
        if !extend.distance_km.is_finite()
            || extend.distance_km < 0.0
            || extend.distance_km > MAX_EXTEND_KM
        {
            return Err(format!(
                "extend.distance_km must be within 0..={MAX_EXTEND_KM} km, got {}",
                extend.distance_km
            ));
        }
    }
    if let Some(aspect) = opts.aspect {
        if !aspect.is_finite() || !(0.1..=10.0).contains(&aspect) {
            return Err(format!("aspect must be within 0.1..=10, got {aspect}"));
        }
    }
    if !opts.min_span_km.is_finite() || opts.min_span_km <= 0.0 {
        return Err(format!("min_span_km must be positive, got {}", opts.min_span_km));
    }
    if !opts.max_span_km.is_finite() || opts.max_span_km < opts.min_span_km {
        return Err(format!(
            "max_span_km must be at least min_span_km ({}), got {}",
            opts.min_span_km, opts.max_span_km
        ));
    }
    Ok(())
}

/// Drop the duplicate closing point of an explicitly closed ring.
fn open_ring(points: &[(f64, f64)]) -> &[(f64, f64)] {
    match (points.first(), points.last()) {
        (Some(first), Some(last)) if points.len() > 1 && first == last => {
            &points[..points.len() - 1]
        }
        _ => points,
    }
}

/// Grow `[lo, hi]` symmetrically about its center to at least `span`.
fn grow_to_min_span(lo: f64, hi: f64, span: f64) -> (f64, f64) {
    if hi - lo >= span {
        return (lo, hi);
    }
    let center = 0.5 * (lo + hi);
    (center - 0.5 * span, center + 0.5 * span)
}

/// Shrink `[lo, hi]` symmetrically about its center to at most `span`.
fn clamp_to_max_span(lo: f64, hi: f64, span: f64) -> (f64, f64) {
    if hi - lo <= span {
        return (lo, hi);
    }
    let center = 0.5 * (lo + hi);
    (center - 0.5 * span, center + 0.5 * span)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ~10 km x ~10 km box centered near Paradise, CA.
    fn small_square() -> Vec<(f64, f64)> {
        let (lon0, lat0) = (-121.6_f64, 39.75_f64);
        let dlat = 5.0 / KM_PER_DEG_LAT;
        let dlon = 5.0 / (KM_PER_DEG_LAT * lat0.to_radians().cos());
        vec![
            (lon0 - dlon, lat0 - dlat),
            (lon0 + dlon, lat0 - dlat),
            (lon0 + dlon, lat0 + dlat),
            (lon0 - dlon, lat0 + dlat),
        ]
    }

    fn span_km(bounds: [f64; 4]) -> (f64, f64) {
        let [west, east, south, north] = bounds;
        let mid_lat = 0.5 * (south + north);
        (
            (east - west) * KM_PER_DEG_LAT * mid_lat.to_radians().cos(),
            (north - south) * KM_PER_DEG_LAT,
        )
    }

    fn base_opts() -> PerimeterDomainOptions {
        PerimeterDomainOptions {
            padding_km: 25.0,
            extend: None,
            aspect: None,
            min_span_km: 1.0,
            max_span_km: 5000.0,
        }
    }

    #[test]
    fn padding_adds_km_to_every_side() {
        let bounds = perimeter_domain_bounds(&small_square(), &base_opts()).unwrap();
        let (width_km, height_km) = span_km(bounds);
        // 10 km perimeter + 25 km padding on each side.
        assert!((width_km - 60.0).abs() < 1.0, "width {width_km}");
        assert!((height_km - 60.0).abs() < 1.0, "height {height_km}");
    }

    #[test]
    fn bounds_contain_the_whole_perimeter() {
        let perimeter = small_square();
        let [west, east, south, north] =
            perimeter_domain_bounds(&perimeter, &base_opts()).unwrap();
        for (lon, lat) in perimeter {
            assert!(lon > west && lon < east, "{lon} outside [{west}, {east}]");
            assert!(lat > south && lat < north, "{lat} outside [{south}, {north}]");
        }
    }

    #[test]
    fn bounds_center_on_the_perimeter_centroid_without_extension() {
        let [west, east, south, north] =
            perimeter_domain_bounds(&small_square(), &base_opts()).unwrap();
        assert!((0.5 * (west + east) - -121.6).abs() < 0.01);
        assert!((0.5 * (south + north) - 39.75).abs() < 0.01);
    }

    #[test]
    fn closed_rings_match_open_rings() {
        let open = small_square();
        let mut closed = open.clone();
        closed.push(open[0]);
        assert_eq!(
            perimeter_domain_bounds(&open, &base_opts()).unwrap(),
            perimeter_domain_bounds(&closed, &base_opts()).unwrap(),
        );
    }

    #[test]
    fn eastward_extension_grows_only_the_east_side() {
        let opts = base_opts();
        let base = perimeter_domain_bounds(&small_square(), &opts).unwrap();
        let extended = perimeter_domain_bounds(
            &small_square(),
            &PerimeterDomainOptions {
                extend: Some(PerimeterExtension {
                    direction_deg: 90.0,
                    distance_km: 40.0,
                }),
                ..opts
            },
        )
        .unwrap();
        assert!((extended[0] - base[0]).abs() < 1e-9, "west moved");
        assert!((extended[2] - base[2]).abs() < 1e-9, "south moved");
        assert!((extended[3] - base[3]).abs() < 1e-9, "north moved");
        let grown_km =
            (extended[1] - base[1]) * KM_PER_DEG_LAT * (39.75f64).to_radians().cos();
        assert!((grown_km - 40.0).abs() < 1.0, "east grew {grown_km} km");
    }

    #[test]
    fn northward_extension_grows_only_the_north_side() {
        let opts = base_opts();
        let base = perimeter_domain_bounds(&small_square(), &opts).unwrap();
        let extended = perimeter_domain_bounds(
            &small_square(),
            &PerimeterDomainOptions {
                extend: Some(PerimeterExtension {
                    direction_deg: 0.0,
                    distance_km: 30.0,
                }),
                ..opts
            },
        )
        .unwrap();
        assert!((extended[0] - base[0]).abs() < 1e-9);
        assert!((extended[1] - base[1]).abs() < 1e-9);
        assert!((extended[2] - base[2]).abs() < 1e-9);
        let grown_km = (extended[3] - base[3]) * KM_PER_DEG_LAT;
        assert!((grown_km - 30.0).abs() < 1.0, "north grew {grown_km} km");
    }

    #[test]
    fn diagonal_extension_splits_between_adjacent_sides() {
        let opts = base_opts();
        let base = perimeter_domain_bounds(&small_square(), &opts).unwrap();
        let extended = perimeter_domain_bounds(
            &small_square(),
            &PerimeterDomainOptions {
                extend: Some(PerimeterExtension {
                    direction_deg: 65.0,
                    distance_km: 80.0,
                }),
                ..opts
            },
        )
        .unwrap();
        let east_grow_km =
            (extended[1] - base[1]) * KM_PER_DEG_LAT * (39.75f64).to_radians().cos();
        let north_grow_km = (extended[3] - base[3]) * KM_PER_DEG_LAT;
        assert!(
            (east_grow_km - 80.0 * (65.0f64).to_radians().sin()).abs() < 1.0,
            "east {east_grow_km}"
        );
        assert!(
            (north_grow_km - 80.0 * (65.0f64).to_radians().cos()).abs() < 1.0,
            "north {north_grow_km}"
        );
        assert!((extended[0] - base[0]).abs() < 1e-9, "west moved");
        assert!((extended[2] - base[2]).abs() < 1e-9, "south moved");
    }

    #[test]
    fn aspect_fit_grows_the_short_axis_only() {
        let opts = PerimeterDomainOptions {
            aspect: Some(1.5),
            ..base_opts()
        };
        let bounds = perimeter_domain_bounds(&small_square(), &opts).unwrap();
        let (width_km, height_km) = span_km(bounds);
        assert!(
            (width_km / height_km - 1.5).abs() < 0.02,
            "aspect {}",
            width_km / height_km
        );
        // Height keeps the natural padded span; width grew.
        assert!((height_km - 60.0).abs() < 1.0, "height {height_km}");
    }

    #[test]
    fn aspect_fit_grows_height_when_width_is_long() {
        let opts = PerimeterDomainOptions {
            aspect: Some(0.5),
            ..base_opts()
        };
        let bounds = perimeter_domain_bounds(&small_square(), &opts).unwrap();
        let (width_km, height_km) = span_km(bounds);
        assert!(
            (width_km / height_km - 0.5).abs() < 0.02,
            "aspect {}",
            width_km / height_km
        );
        assert!((width_km - 60.0).abs() < 1.0, "width {width_km}");
    }

    #[test]
    fn tiny_fires_get_the_minimum_span() {
        let tiny = vec![
            (-121.6005, 39.7495),
            (-121.5995, 39.7495),
            (-121.5995, 39.7505),
            (-121.6005, 39.7505),
        ];
        let opts = PerimeterDomainOptions {
            padding_km: 5.0,
            min_span_km: 40.0,
            ..base_opts()
        };
        let bounds = perimeter_domain_bounds(&tiny, &opts).unwrap();
        let (width_km, height_km) = span_km(bounds);
        assert!((width_km - 40.0).abs() < 1.0, "width {width_km}");
        assert!((height_km - 40.0).abs() < 1.0, "height {height_km}");
    }

    #[test]
    fn huge_accidental_input_is_clamped_to_the_maximum_span() {
        let huge = vec![
            (-140.0, 25.0),
            (-100.0, 25.0),
            (-100.0, 55.0),
            (-140.0, 55.0),
        ];
        let opts = PerimeterDomainOptions {
            max_span_km: 800.0,
            ..base_opts()
        };
        let bounds = perimeter_domain_bounds(&huge, &opts).unwrap();
        let (width_km, height_km) = span_km(bounds);
        assert!(width_km <= 810.0, "width {width_km}");
        assert!(height_km <= 810.0, "height {height_km}");
    }

    #[test]
    fn degenerate_and_hostile_input_is_rejected() {
        let opts = base_opts();
        assert!(perimeter_domain_bounds(&[], &opts).is_err());
        assert!(perimeter_domain_bounds(&[(-121.6, 39.7), (-121.5, 39.8)], &opts).is_err());
        assert!(
            perimeter_domain_bounds(&[(-121.6, f64::NAN), (-121.5, 39.8), (-121.4, 39.7)], &opts)
                .is_err()
        );
        assert!(
            perimeter_domain_bounds(&[(-500.0, 39.7), (-121.5, 39.8), (-121.4, 39.7)], &opts)
                .is_err()
        );
        assert!(
            perimeter_domain_bounds(&[(-121.6, 89.0), (-121.5, 89.1), (-121.4, 89.05)], &opts)
                .is_err(),
            "polar centroids have no usable local projection"
        );
        assert!(
            perimeter_domain_bounds(
                &small_square(),
                &PerimeterDomainOptions {
                    padding_km: 5000.0,
                    ..opts
                }
            )
            .is_err(),
            "absurd padding must be rejected"
        );
    }

    #[test]
    fn perimeter_hash_is_stable_and_content_sensitive() {
        let a = perimeter_hash(&small_square());
        let b = perimeter_hash(&small_square());
        assert_eq!(a, b);
        let mut moved = small_square();
        moved[0].0 += 0.0001;
        assert_ne!(a, perimeter_hash(&moved));
        assert!(!a.is_empty());
        assert!(a.chars().all(|ch| ch.is_ascii_hexdigit()));
    }
}

//! Geographic polyline overlays: lon/lat rings (fire perimeters, incident
//! boundaries) projected into the map's linework pass so every product lane
//! draws them above the raster and below labels.
//!
//! The spec travels as a small JSON file whose path is published through
//! [`OVERLAY_POLYLINE_FILE_ENV`], matching the `RUSTWX_*` env contract the
//! render bins already use for basemap style and county linework.

use std::path::Path;

use serde::Deserialize;

use crate::presentation::LineworkRole;
use crate::request::{Color, ProjectedLineOverlay};

/// Env var carrying the overlay spec file path for a render process.
pub const OVERLAY_POLYLINE_FILE_ENV: &str = "RUSTWX_OVERLAY_POLYLINE_FILE";

/// Maximum lon/lat step between projected overlay points; longer segments
/// are densified so the stroke follows the projection's curvature.
pub(crate) const OVERLAY_DENSIFY_STEP_DEG: f64 = 0.05;

/// High-contrast perimeter styling: dark halo under an orange stroke
/// (docs/FABLE_FIRE_WEATHER_HANDOFF.md, "Overlay implementation path").
const OVERLAY_STROKE_COLOR: Color = Color {
    r: 255,
    g: 138,
    b: 0,
    a: 255,
};
const OVERLAY_HALO_COLOR: Color = Color {
    r: 26,
    g: 18,
    b: 12,
    a: 235,
};
const OVERLAY_STROKE_WIDTH: u32 = 3;
const OVERLAY_HALO_EXTRA_WIDTH: u32 = 3;

/// One or more lon/lat rings to stroke on the map.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct GeoPolylineOverlaySpec {
    /// Rings as `[lon, lat]` pairs; open rings are closed automatically.
    pub rings: Vec<Vec<(f64, f64)>>,
}

/// Parse an overlay spec from its JSON document.
pub fn parse_overlay_polyline_spec(json: &str) -> Result<GeoPolylineOverlaySpec, String> {
    let spec: GeoPolylineOverlaySpec = serde_json::from_str(json)
        .map_err(|err| format!("overlay polyline spec is not valid JSON: {err}"))?;
    if spec.rings.is_empty() {
        return Err("overlay polyline spec has no rings".to_string());
    }
    for (ring_index, ring) in spec.rings.iter().enumerate() {
        if ring.len() < 2 {
            return Err(format!(
                "overlay ring {ring_index} needs at least 2 points, got {}",
                ring.len()
            ));
        }
        for (point_index, (lon, lat)) in ring.iter().enumerate() {
            if !lon.is_finite() || !lat.is_finite() {
                return Err(format!(
                    "overlay ring {ring_index} point {point_index} is not finite"
                ));
            }
            if *lon < -180.0 || *lon > 180.0 || *lat < -90.0 || *lat > 90.0 {
                return Err(format!(
                    "overlay ring {ring_index} point {point_index} ({lon}, {lat}) is outside \
                     lon [-180, 180] / lat [-90, 90]"
                ));
            }
        }
    }
    Ok(spec)
}

/// Load an overlay spec from a JSON file on disk.
pub fn load_overlay_polyline_spec_from_path(
    path: &Path,
) -> Result<GeoPolylineOverlaySpec, String> {
    let json = std::fs::read_to_string(path)
        .map_err(|err| format!("read overlay polyline spec {}: {err}", path.display()))?;
    parse_overlay_polyline_spec(&json)
}

/// Load the overlay spec named by [`OVERLAY_POLYLINE_FILE_ENV`], if set.
/// A set-but-unreadable spec is a hard error: a requested perimeter must
/// never silently vanish from the plot.
pub fn load_overlay_polyline_spec_from_env() -> Result<Option<GeoPolylineOverlaySpec>, String> {
    match std::env::var(OVERLAY_POLYLINE_FILE_ENV) {
        Ok(value) if !value.trim().is_empty() => {
            load_overlay_polyline_spec_from_path(Path::new(value.trim())).map(Some)
        }
        _ => Ok(None),
    }
}

/// Project the spec's rings into halo + stroke line overlays. `project`
/// maps `(lon, lat)` to projected `(x, y)` — the same projector the
/// basemap linework uses, so the overlay lands exactly on the map frame.
pub(crate) fn projected_overlay_lines(
    spec: &GeoPolylineOverlaySpec,
    project: impl Fn(f64, f64) -> (f64, f64),
    densify_step_deg: f64,
) -> Vec<ProjectedLineOverlay> {
    let step = if densify_step_deg.is_finite() && densify_step_deg > 0.0 {
        densify_step_deg
    } else {
        OVERLAY_DENSIFY_STEP_DEG
    };
    let mut lines = Vec::with_capacity(spec.rings.len() * 2);
    for ring in &spec.rings {
        let mut closed = ring.clone();
        if closed.first() != closed.last() {
            if let Some(&first) = closed.first() {
                closed.push(first);
            }
        }
        let mut points = Vec::with_capacity(closed.len());
        for pair in closed.windows(2) {
            let (lon_a, lat_a) = pair[0];
            let (lon_b, lat_b) = pair[1];
            let segments = ((lon_b - lon_a).abs().max((lat_b - lat_a).abs()) / step)
                .ceil()
                .max(1.0) as usize;
            if points.is_empty() {
                points.push(project(lon_a, lat_a));
            }
            for index in 1..=segments {
                let t = index as f64 / segments as f64;
                points.push(project(lon_a + (lon_b - lon_a) * t, lat_a + (lat_b - lat_a) * t));
            }
        }
        if points.len() < 2 {
            continue;
        }
        lines.push(ProjectedLineOverlay {
            points: points.clone(),
            color: OVERLAY_HALO_COLOR,
            width: OVERLAY_STROKE_WIDTH + OVERLAY_HALO_EXTRA_WIDTH,
            role: LineworkRole::Generic,
        });
        lines.push(ProjectedLineOverlay {
            points,
            color: OVERLAY_STROKE_COLOR,
            width: OVERLAY_STROKE_WIDTH,
            role: LineworkRole::Generic,
        });
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn triangle_json() -> &'static str {
        r#"{"rings": [[[-121.7, 39.6], [-121.4, 39.7], [-121.3, 39.4]]]}"#
    }

    #[test]
    fn parses_a_single_ring_spec() {
        let spec = parse_overlay_polyline_spec(triangle_json()).unwrap();
        assert_eq!(spec.rings.len(), 1);
        assert_eq!(spec.rings[0].len(), 3);
        assert_eq!(spec.rings[0][0], (-121.7, 39.6));
    }

    #[test]
    fn rejects_malformed_and_degenerate_specs() {
        assert!(parse_overlay_polyline_spec("not json").is_err());
        assert!(parse_overlay_polyline_spec(r#"{"rings": []}"#).is_err());
        assert!(parse_overlay_polyline_spec(r#"{"rings": [[[-121.7, 39.6]]]}"#).is_err());
        assert!(
            parse_overlay_polyline_spec(
                r#"{"rings": [[[-121.7, null], [-121.4, 39.7], [-121.3, 39.4]]]}"#
            )
            .is_err()
        );
        let out_of_range = r#"{"rings": [[[-500.0, 39.6], [-121.4, 39.7], [-121.3, 39.4]]]}"#;
        assert!(
            parse_overlay_polyline_spec(out_of_range).is_err(),
            "out-of-range longitudes must be rejected"
        );
    }

    #[test]
    fn projects_each_ring_as_halo_under_stroke() {
        let spec = parse_overlay_polyline_spec(triangle_json()).unwrap();
        let lines = projected_overlay_lines(&spec, |lon, lat| (lon, lat), 10.0);
        assert_eq!(lines.len(), 2, "one halo + one stroke");
        let (halo, stroke) = (&lines[0], &lines[1]);
        assert!(halo.width > stroke.width, "halo draws wider, first");
        assert_eq!(stroke.color.a, 255);
        assert_eq!(halo.points, stroke.points);
        assert_eq!(stroke.role, LineworkRole::Generic);
    }

    #[test]
    fn rings_are_closed_before_stroking() {
        let spec = parse_overlay_polyline_spec(triangle_json()).unwrap();
        let lines = projected_overlay_lines(&spec, |lon, lat| (lon, lat), 10.0);
        let points = &lines[1].points;
        assert_eq!(
            points.first(),
            points.last(),
            "open ring must be closed by the projector"
        );
    }

    #[test]
    fn long_segments_are_densified_to_follow_projection_curvature() {
        let spec = GeoPolylineOverlaySpec {
            rings: vec![vec![(-122.0, 39.0), (-121.0, 39.0), (-121.0, 40.0)]],
        };
        let lines = projected_overlay_lines(&spec, |lon, lat| (lon, lat), 0.25);
        // Three 1-degree sides at 0.25 degree steps -> at least 12 segments.
        assert!(
            lines[1].points.len() >= 12,
            "expected densified ring, got {} points",
            lines[1].points.len()
        );
    }

    #[test]
    fn missing_spec_file_is_a_hard_error() {
        assert!(
            load_overlay_polyline_spec_from_path(Path::new(
                "definitely/not/a/real/overlay.json"
            ))
            .is_err()
        );
    }
}

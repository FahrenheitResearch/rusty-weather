// Two binaries include this module and each uses a different half: `rw_render`
// builds the payload, `rw_fire_api` only parses it back. Without this, whichever
// half a bin does not call warns as dead code (the same reason `render_all`
// carries it).
#![allow(dead_code)]

//! The `geo` block published for one rendered image: where the map plot rect
//! landed in that file and what projected coordinates it covers, so a client can
//! turn a cursor pixel into lat/lon instead of hunting for the black axes
//! rectangle and applying an empirical pad.
//!
//! It lives in its own module because two binaries share it by `#[path]`
//! include: `rw_render` writes it into `api_manifest.json` next to the images,
//! and `rw_fire_api` reads that manifest back and republishes it per file in the
//! job response. One definition means the writer and the reader cannot drift.
//!
//! The load-bearing part is `projected_bounds` plus `projection`, NOT the lat/lon
//! corners: the pixel grid is linear in PROJECTED coordinates, so Mercator's y is
//! logarithmic in latitude and the Lambert/Albers conics are not axis-aligned in
//! lat/lon at all. `geographic_bounds` and `requested_bounds` are there so a
//! client can sanity-check its own math for free.

use rustwx_render::{PlotGeometry, ProjectedExtent, ProjectionSpec};
use serde::{Deserialize, Serialize};

/// Version of the `geo` contract. Bump it when an existing field changes
/// MEANING; purely additive fields do not need it. Published because the
/// contract shipped without one and a client cannot otherwise tell whether
/// `plot_px` is pixel centres or pixel edges.
pub const GEO_SCHEMA_VERSION: u32 = 1;

/// What `plot_px` means, stated in the payload rather than left to a comment
/// somewhere: the plot rect's outer pixels are the pixel CENTRES the extent
/// edges land on, which is why the client formula divides by `width - 1`.
///
/// A string and not an enum so an older reader parses a newer value instead of
/// failing the whole document over one word it has not heard of.
pub const PIXEL_CONVENTION_EDGE_PIXELS_ARE_CENTERS: &str = "edge_pixels_are_centers";

/// Where the map plot rect landed in one rendered image, and how to invert it.
///
/// ```text
/// rel_x = (image_x - plot_px.x) / (plot_px.width  - 1)
/// rel_y = (image_y - plot_px.y) / (plot_px.height - 1)
/// X = x_min + rel_x * (x_max - x_min)
/// Y = y_min + (1 - rel_y) * (y_max - y_min)
/// ```
///
/// then unproject `(X, Y)` with `projection` on a sphere of
/// `projection.earth_radius_m`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Geo {
    pub schema: u32,
    pub image_px: SizePx,
    pub plot_px: RectPx,
    pub projected_bounds: ProjectedExtent,
    pub projection: GeoProjection,
    /// Lat/lon box of the four plot CORNERS — a hint for fitting a client map,
    /// not the frame's true lat/lon extremes, because a conic projection's frame
    /// edges bow outward between corners. Absent when a corner does not invert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geographic_bounds: Option<LonLatBox>,
    /// The bounds the job asked for, echoed back. The rendered frame is fitted
    /// to an aspect ratio and so is usually a little larger than this.
    pub requested_bounds: LonLatBox,
    pub pixel_convention: String,
}

impl Geo {
    /// Restate a captured [`PlotGeometry`] as the published payload.
    ///
    /// `requested_bounds` is `(west, east, south, north)` — the tuple order every
    /// domain in this repo uses.
    pub fn from_plot_geometry(
        geometry: &PlotGeometry,
        requested_bounds: (f64, f64, f64, f64),
    ) -> Self {
        Self {
            schema: GEO_SCHEMA_VERSION,
            image_px: SizePx {
                width: geometry.image_w,
                height: geometry.image_h,
            },
            plot_px: RectPx {
                x: geometry.plot_x,
                y: geometry.plot_y,
                width: geometry.plot_w,
                height: geometry.plot_h,
            },
            projected_bounds: geometry.projected.clone(),
            projection: GeoProjection {
                spec: geometry.projection.clone(),
                earth_radius_m: geometry.earth_radius_m,
                reference_latitude_deg: geometry.reference_latitude_deg,
                reference_longitude_deg: geometry.reference_longitude_deg,
            },
            geographic_bounds: geometry.geographic.as_ref().map(|bounds| LonLatBox {
                west: bounds.west_deg,
                east: bounds.east_deg,
                south: bounds.south_deg,
                north: bounds.north_deg,
            }),
            requested_bounds: LonLatBox {
                west: requested_bounds.0,
                east: requested_bounds.1,
                south: requested_bounds.2,
                north: requested_bounds.3,
            },
            pixel_convention: PIXEL_CONVENTION_EDGE_PIXELS_ARE_CENTERS.to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SizePx {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RectPx {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LonLatBox {
    pub west: f64,
    pub east: f64,
    pub south: f64,
    pub north: f64,
}

/// The projection the pixel grid is linear in, plus the two things a
/// [`ProjectionSpec`] on its own cannot supply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeoProjection {
    /// Flattened, not wrapped: `ProjectionSpec` is already internally tagged
    /// (`{"kind": "mercator", ...}`), so it serializes verbatim and a new
    /// projection variant needs no change here. Hand-rolling a per-variant
    /// switch is how the two descriptions of one projection drift apart.
    #[serde(flatten)]
    pub spec: ProjectionSpec,
    /// The NCEP/WRF sphere, not WGS84. Stated explicitly because a client that
    /// reaches for EPSG:3857 is kilometres off from the first pixel.
    pub earth_radius_m: f64,
    /// The values the projector RESOLVED to, which the spec does not carry: a
    /// Lambert conformal spec omits the latitude of origin that sets its y datum,
    /// and a geographic spec carries no central meridian at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_latitude_deg: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_longitude_deg: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustwx_render::GeographicBounds;

    fn lambert_geometry() -> PlotGeometry {
        PlotGeometry {
            image_w: 1600,
            image_h: 889,
            plot_x: 16,
            plot_y: 74,
            plot_w: 1568,
            plot_h: 742,
            projected: ProjectedExtent {
                x_min: -467_281.550_516_926,
                x_max: 467_281.550_516_926,
                y_min: -546_733.614_509_383_2,
                y_max: 552_466.064_544_035_1,
            },
            projection: ProjectionSpec::LambertConformal {
                standard_parallel_1_deg: 30.0,
                standard_parallel_2_deg: 60.0,
                central_meridian_deg: -119.0,
            },
            reference_latitude_deg: Some(37.0),
            reference_longitude_deg: None,
            earth_radius_m: 6_370_000.0,
            geographic: Some(GeographicBounds::new(-124.846, -113.154, 31.868, 41.957)),
        }
    }

    /// The whole point of the payload: a client reading these numbers must be
    /// able to reproduce the renderer's own pixel<->projected mapping. Compares
    /// the published contract against `PlotGeometry`'s implementation of it.
    #[test]
    fn published_fields_reproduce_the_renderers_pixel_mapping() {
        let geometry = lambert_geometry();
        let geo = Geo::from_plot_geometry(&geometry, (-124.5, -113.5, 32.0, 42.0));
        let last_x = f64::from(geo.plot_px.width - 1);
        let last_y = f64::from(geo.plot_px.height - 1);
        let span_x = geo.projected_bounds.x_max - geo.projected_bounds.x_min;
        let span_y = geo.projected_bounds.y_max - geo.projected_bounds.y_min;
        for (rel_x, rel_y) in [(0.0, 0.0), (1.0, 1.0), (0.5, 0.25), (0.13, 0.87)] {
            let image_x = f64::from(geo.plot_px.x) + rel_x * last_x;
            let image_y = f64::from(geo.plot_px.y) + rel_y * last_y;
            let expected = geometry
                .projected_at_pixel(image_x, image_y)
                .expect("the fixture extent is non-degenerate");
            let x = geo.projected_bounds.x_min + rel_x * span_x;
            let y = geo.projected_bounds.y_min + (1.0 - rel_y) * span_y;
            assert!(
                (x - expected.0).abs() < 1.0e-6 && (y - expected.1).abs() < 1.0e-6,
                "client formula at ({rel_x}, {rel_y}) gave ({x}, {y}), renderer says {expected:?}"
            );
        }
    }

    /// `#[serde(flatten)]` over an internally tagged enum is the one clever thing
    /// in this file, and a silent failure there would publish a projection a
    /// client cannot rebuild. Pins the wire shape and the round trip.
    #[test]
    fn projection_serializes_verbatim_and_survives_a_round_trip() {
        let geo = Geo::from_plot_geometry(&lambert_geometry(), (-124.5, -113.5, 32.0, 42.0));
        let json = serde_json::to_value(&geo).expect("geo serializes");
        let projection = &json["projection"];
        assert_eq!(projection["kind"], "lambert_conformal");
        assert_eq!(projection["standard_parallel_1_deg"], 30.0);
        assert_eq!(projection["central_meridian_deg"], -119.0);
        assert_eq!(projection["earth_radius_m"], 6_370_000.0);
        assert_eq!(projection["reference_latitude_deg"], 37.0);
        assert!(
            projection.get("reference_longitude_deg").is_none(),
            "an unresolved reference longitude must be omitted, not null: {projection}"
        );
        assert_eq!(json["schema"], 1);
        assert_eq!(json["pixel_convention"], "edge_pixels_are_centers");
        assert_eq!(json["plot_px"]["x"], 16);
        assert_eq!(json["image_px"]["height"], 889);
        assert_eq!(json["requested_bounds"]["west"], -124.5);

        let parsed: Geo = serde_json::from_value(json).expect("geo round-trips");
        assert_eq!(parsed, geo);
    }

    /// A Mercator domain publishes its own two parameters under the same key, so
    /// the client needs no per-variant special case either.
    #[test]
    fn a_mercator_domain_publishes_its_own_projection_parameters() {
        let mut geometry = lambert_geometry();
        geometry.projection = ProjectionSpec::Mercator {
            latitude_of_true_scale_deg: 37.2,
            central_meridian_deg: -119.75,
        };
        let geo = Geo::from_plot_geometry(&geometry, (-124.5, -113.5, 32.0, 42.0));
        let json = serde_json::to_value(&geo).expect("geo serializes");
        assert_eq!(json["projection"]["kind"], "mercator");
        assert_eq!(json["projection"]["latitude_of_true_scale_deg"], 37.2);
        assert_eq!(json["projection"]["central_meridian_deg"], -119.75);
        let parsed: Geo = serde_json::from_value(json).expect("geo round-trips");
        assert_eq!(parsed, geo);
    }
}

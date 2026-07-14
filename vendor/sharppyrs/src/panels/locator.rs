//! Location map panel: the sounding-point locator as a full inset panel
//! (state-outline basemap + crosshair), replacing the "Psbl Haz. Type" box
//! in the default window layout. Same drawing as the hodograph's corner
//! inset, scaled up to its own cell with the inset-row title framing.

use egui::{Align2, Color32, FontId, Painter, Pos2, Rect, Shape, Stroke, StrokeKind, Vec2};

use crate::Profile;
use crate::derived::DerivedParams;
use crate::skewt::SkewTStyle;
use crate::utils::qc;

const MAP_FILL: Color32 = Color32::from_rgb(0x05, 0x09, 0x0b);
const MAP_POINT_COLOR: Color32 = Color32::from_rgb(0xFF, 0xDA, 0x00);
const PT: f64 = 4.0 / 3.0;

/// Draw the location panel into `rect`.
#[allow(unused_variables)]
pub fn draw(painter: &Painter, rect: Rect, prof: &Profile, dv: &DerivedParams, style: &SkewTStyle) {
    painter.rect_filled(rect, 0.0, style.bg_color);
    painter.rect_stroke(
        rect.shrink(0.5),
        0.0,
        Stroke::new(1.0, style.fg_color),
        StrokeKind::Inside,
    );

    // Title band like the other insets (vendored inset title style).
    let hgt = rect.height() as f64;
    let title_px = ((7.0 + hgt * 0.0045) * PT) as f32;
    let title_h = (title_px * 1.6).ceil();
    painter.text(
        egui::pos2(rect.center().x, rect.min.y + title_h * 0.5),
        Align2::CENTER_CENTER,
        "Location",
        FontId::new(title_px, style.font_bold.clone()),
        style.fg_color,
    );
    painter.line_segment(
        [
            egui::pos2(rect.min.x, rect.min.y + title_h),
            egui::pos2(rect.max.x, rect.min.y + title_h),
        ],
        Stroke::new(1.0, style.fg_color),
    );

    let lat = prof.inner.station.latitude;
    let lon = prof.inner.station.longitude;
    let body = Rect::from_min_max(egui::pos2(rect.min.x, rect.min.y + title_h), rect.max);
    if !lat.is_finite() || !lon.is_finite() || lat.abs() > 90.0 || lon.abs() > 180.0 {
        painter.text(
            body.center(),
            Align2::CENTER_CENTER,
            "no location",
            FontId::new(title_px, style.font_regular.clone()),
            Color32::from_gray(0x88),
        );
        return;
    }

    let interior = body.shrink(4.0);
    if interior.width() < 40.0 || interior.height() < 30.0 {
        return;
    }
    painter.rect_filled(interior, 0.0, MAP_FILL);
    let mp = painter.with_clip_rect(interior);

    // Aspect-correct extent centered on the sounding point.
    let half_lat = 4.0f64;
    let cos_lat = lat.to_radians().cos().max(0.35);
    let aspect = (interior.width() / interior.height()) as f64;
    let half_lon = half_lat * aspect / cos_lat;
    let (west, south, east, north) = (
        lon - half_lon,
        lat - half_lat,
        lon + half_lon,
        lat + half_lat,
    );
    let map_point = |plon: f64, plat: f64| -> Pos2 {
        Pos2::new(
            interior.min.x + ((plon - west) / (east - west) * interior.width() as f64) as f32,
            interior.min.y + ((north - plat) / (north - south) * interior.height() as f64) as f32,
        )
    };

    let outline = Stroke::new(1.0, style.fg_color);
    for seg in super::hodo_map_data::SEGMENTS {
        let visible = seg.iter().any(|(slon, slat)| {
            (*slon as f64) >= west - 6.0
                && (*slon as f64) <= east + 6.0
                && (*slat as f64) >= south - 6.0
                && (*slat as f64) <= north + 6.0
        });
        if !visible {
            continue;
        }
        let pts: Vec<Pos2> = seg
            .iter()
            .map(|(slon, slat)| map_point(*slon as f64, *slat as f64))
            .collect();
        if pts.len() >= 2 {
            mp.add(Shape::line(pts, outline));
        }
    }

    // Crosshair at the sounding point.
    let c = map_point(lon, lat);
    let marker = Stroke::new(1.4, MAP_POINT_COLOR);
    mp.circle(c, 4.0, MAP_FILL, marker);
    mp.line_segment([c - Vec2::new(7.0, 0.0), c + Vec2::new(7.0, 0.0)], marker);
    mp.line_segment([c - Vec2::new(0.0, 7.0), c + Vec2::new(0.0, 7.0)], marker);

    // Lat/lon readout, bottom-left inside the map.
    let _ = qc(0.0);
    let ns = if lat >= 0.0 { "N" } else { "S" };
    let ew = if lon >= 0.0 { "E" } else { "W" };
    mp.text(
        egui::pos2(interior.min.x + 4.0, interior.max.y - 3.0),
        Align2::LEFT_BOTTOM,
        format!("{:.2}\u{b0}{ns} {:.2}\u{b0}{ew}", lat.abs(), lon.abs()),
        FontId::new((title_px * 0.9).max(9.0), style.font_regular.clone()),
        MAP_POINT_COLOR,
    );
}

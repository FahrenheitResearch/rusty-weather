//! Product orchestration APIs.
//!
//! This crate currently keeps a broad public module surface for compatibility.
//! Before changing any module visibility, update the product-surface map in
//! the guardrail tests in `src/tests.rs` so consumers and CLI tools get
//! audited intentionally.

pub mod cache;
pub mod catalog;
pub mod derived;
pub mod direct;
pub mod ecape;
pub mod gridded;
pub mod heavy;
pub mod hrrr;
pub mod named_geometry;
pub mod non_ecape;
pub mod places;
pub mod planner;
pub mod plot_design;
pub mod point_timeseries;
pub mod publication;
pub(crate) mod qpf;
pub mod runtime;
pub mod sampling;
pub mod severe;
pub mod shared_context;
pub mod source;
pub mod spec;
pub mod temp_display;
pub mod thermo_native;
pub mod topo;
pub mod viewer;
pub mod windowed;
pub mod windowed_decoder;

pub use named_geometry::{
    NamedGeoBounds, NamedGeoPoint, NamedGeometry, NamedGeometryAsset, NamedGeometryCatalog,
    NamedGeometryKind, NamedGeometrySelector,
};
pub use shared_context::{
    DomainSpec, PreparedProjectedContext, ProjectedMap, ProjectedMapProvider, WeatherPanelField,
    WeatherPanelHeader, WeatherPanelLayout, layout_key, render_two_by_four_weather_panel,
};

pub(crate) fn apply_place_label_overlay_with_density_styling(
    render_request: &mut rustwx_render::MapRenderRequest,
    overlay: &crate::places::PlaceLabelOverlay,
    domain: &crate::shared_context::DomainSpec,
    grid_lat_deg: &[f32],
    grid_lon_deg: &[f32],
    projection: Option<&rustwx_core::GridProjection>,
) -> Result<(), Box<dyn std::error::Error>> {
    let selected = overlay.selected_places_for_domain(domain);
    let start = render_request.projected_place_labels.len();
    crate::places::apply_place_label_overlay(
        render_request,
        overlay,
        domain,
        grid_lat_deg,
        grid_lon_deg,
        projection,
    )?;
    style_added_place_labels_for_density(render_request, overlay, &selected, start);
    if value_labels_enabled() {
        apply_value_labels(render_request, &selected, grid_lat_deg, grid_lon_deg, start);
    }
    Ok(())
}

/// Pivotal-style value labels: stamp the plotted field's value at each city
/// point instead of its name. Enabled per-render by the API via
/// `RUSTWX_VALUE_LABELS` on the render child (mirrors `RUSTWX_TEMP_UNITS`).
fn value_labels_enabled() -> bool {
    std::env::var("RUSTWX_VALUE_LABELS")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Compact number for a city value label: integer for typical magnitudes
/// (temp/RH/wind), one decimal for small-range fields (e.g. VPD in hPa).
fn format_value_label(value: f64) -> String {
    if value.abs() >= 10.0 {
        format!("{}", value.round() as i64)
    } else {
        let text = format!("{value:.1}");
        if text == "-0.0" { "0".to_string() } else { text }
    }
}

/// Rewrite the just-added city labels (`start..`) from names to the sampled
/// field value at each city, centered on the point with a halo. Samples the
/// SAME values the colorbar uses (`render_request.field`), so units are
/// implied by the colorbar / min-max readout — the labels stay bare numbers.
fn apply_value_labels(
    render_request: &mut rustwx_render::MapRenderRequest,
    selected: &[crate::places::SelectedPlace],
    grid_lat_deg: &[f32],
    grid_lon_deg: &[f32],
    start: usize,
) {
    // Sample first (immutable read of the field), then mutate the labels.
    let samples: Vec<Option<f64>> = selected
        .iter()
        .map(|place| {
            crate::places::nearest_grid_index(
                grid_lat_deg,
                grid_lon_deg,
                place.center_lat,
                place.center_lon,
            )
            .and_then(|index| render_request.field.values.get(index).copied())
            .map(|value| value as f64)
            .filter(|value| value.is_finite())
        })
        .collect();
    for (sample, label) in samples
        .iter()
        .zip(render_request.projected_place_labels.iter_mut().skip(start))
    {
        match sample {
            Some(value) => {
                label.label = Some(format_value_label(*value));
                label.style.label_placement =
                    rustwx_render::ProjectedLabelPlacement::Center;
                label.style.label_offset_x_px = 0;
                label.style.label_offset_y_px = 0;
                label.style.label_bold = true;
                // No name marker dot — Pivotal-style bare numbers.
                label.style.marker_radius_px = 0;
                label.style.marker_outline_width = 0;
            }
            // City has no finite value here (off-grid / masked) — drop it.
            None => {
                label.label = None;
                label.style.marker_radius_px = 0;
                label.style.marker_outline_width = 0;
            }
        }
    }
}

fn style_added_place_labels_for_density(
    render_request: &mut rustwx_render::MapRenderRequest,
    overlay: &crate::places::PlaceLabelOverlay,
    selected: &[crate::places::SelectedPlace],
    start: usize,
) {
    let aux_total = selected
        .iter()
        .filter(|place| !is_major_catalog_place(place.slug.as_str()))
        .count();
    let auxiliary_budget = if aux_total <= 2 {
        aux_total
    } else {
        aux_total.div_ceil(2).min(4)
    };
    let mut aux_seen = 0usize;

    for (place, label) in selected
        .iter()
        .zip(render_request.projected_place_labels.iter_mut().skip(start))
    {
        let is_auxiliary_catalog_place = !is_major_catalog_place(place.slug.as_str());
        label.priority = match overlay.density {
            crate::places::PlaceLabelDensityTier::None
            | crate::places::PlaceLabelDensityTier::Major => {
                rustwx_render::ProjectedPlaceLabelPriority::Primary
            }
            crate::places::PlaceLabelDensityTier::MajorAndAux => {
                if is_auxiliary_catalog_place {
                    rustwx_render::ProjectedPlaceLabelPriority::Auxiliary
                } else {
                    rustwx_render::ProjectedPlaceLabelPriority::Primary
                }
            }
            crate::places::PlaceLabelDensityTier::Dense => {
                if !is_auxiliary_catalog_place {
                    rustwx_render::ProjectedPlaceLabelPriority::Primary
                } else if aux_seen < auxiliary_budget {
                    rustwx_render::ProjectedPlaceLabelPriority::Auxiliary
                } else {
                    rustwx_render::ProjectedPlaceLabelPriority::Micro
                }
            }
            crate::places::PlaceLabelDensityTier::MaxLocal => {
                if is_auxiliary_catalog_place {
                    rustwx_render::ProjectedPlaceLabelPriority::Auxiliary
                } else {
                    rustwx_render::ProjectedPlaceLabelPriority::Primary
                }
            }
        };

        if is_auxiliary_catalog_place {
            aux_seen = aux_seen.saturating_add(1);
        }
    }
}

fn is_major_catalog_place(slug: &str) -> bool {
    crate::places::major_us_city_places()
        .iter()
        .any(|preset| preset.slug == slug)
}

#[cfg(test)]
mod tests;

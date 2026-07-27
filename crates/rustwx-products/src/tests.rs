use super::*;
use crate::places::{PlaceLabelDensityTier, PlaceLabelOverlay};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProductModuleSurfaceKind {
    StablePublic,
    OperationalPublic,
    CompatibilityPublic,
    ProofResearchPublic,
    InternalCandidatePublic,
    LegacyPublic,
    CratePrivate,
}

use ProductModuleSurfaceKind::*;

const PRODUCT_MODULE_SURFACE: &[(&str, ProductModuleSurfaceKind)] = &[
    ("cache", CompatibilityPublic),
    ("catalog", StablePublic),
    ("derived", StablePublic),
    ("direct", StablePublic),
    ("ecape", OperationalPublic),
    ("gridded", CompatibilityPublic),
    ("heavy", OperationalPublic),
    ("hrrr", LegacyPublic),
    ("named_geometry", StablePublic),
    ("non_ecape", OperationalPublic),
    ("places", CompatibilityPublic),
    ("planner", CompatibilityPublic),
    ("plot_design", InternalCandidatePublic),
    ("point_timeseries", OperationalPublic),
    ("publication", OperationalPublic),
    ("qpf", CratePrivate),
    ("runtime", CompatibilityPublic),
    ("sampling", StablePublic),
    ("severe", OperationalPublic),
    ("shared_context", StablePublic),
    ("source", CompatibilityPublic),
    ("spec", CompatibilityPublic),
    ("temp_display", OperationalPublic),
    ("thermo_native", ProofResearchPublic),
    ("topo", OperationalPublic),
    ("viewer", StablePublic),
    ("windowed", OperationalPublic),
    ("windowed_decoder", InternalCandidatePublic),
];

const CALIFORNIA_SQUARE: (f64, f64, f64, f64) = (-124.9, -113.7, 31.8, 42.7);

fn declared_product_modules() -> Vec<(&'static str, bool)> {
    include_str!("lib.rs")
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("pub mod ") {
                Some((rest.split(';').next()?.trim(), true))
            } else if let Some(rest) = trimmed.strip_prefix("pub(crate) mod ") {
                Some((rest.split(';').next()?.trim(), false))
            } else {
                None
            }
        })
        .collect()
}

fn assert_no_duplicate_names(sorted_names: &[&str], label: &str) {
    for pair in sorted_names.windows(2) {
        assert_ne!(pair[0], pair[1], "duplicate {label}: {}", pair[0]);
    }
}

fn sample_place_label_request() -> rustwx_render::MapRenderRequest {
    let grid = rustwx_render::LatLonGrid::new(
        rustwx_render::GridShape::new(2, 2).unwrap(),
        vec![31.8, 31.8, 42.7, 42.7],
        vec![-124.9, -113.7, -124.9, -113.7],
    )
    .unwrap();
    let field = rustwx_render::Field2D::new(
        rustwx_render::ProductKey::named("place_label_density_style_test"),
        "unitless",
        grid,
        vec![0.0, 0.0, 0.0, 0.0],
    )
    .unwrap();
    let mut request = rustwx_render::MapRenderRequest::contour_only(field);
    request.projected_domain = Some(rustwx_render::ProjectedDomain {
        x: vec![0.0, 1.0, 0.0, 1.0],
        y: vec![0.0, 0.0, 1.0, 1.0],
        extent: rustwx_render::ProjectedExtent {
            x_min: 0.0,
            x_max: 1.0,
            y_min: 0.0,
            y_max: 1.0,
        },
    });
    request
}

#[test]
fn public_surface_classification_covers_declared_modules() {
    let mut declared = declared_product_modules()
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    let mut classified = PRODUCT_MODULE_SURFACE
        .iter()
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();

    declared.sort_unstable();
    classified.sort_unstable();

    assert_no_duplicate_names(&declared, "declared module");
    assert_no_duplicate_names(&classified, "classified module");
    assert_eq!(classified, declared);
}

#[test]
fn public_surface_classification_marks_crate_private_modules() {
    for (module_name, is_public) in declared_product_modules() {
        let kind = PRODUCT_MODULE_SURFACE
            .iter()
            .find(|(name, _)| *name == module_name)
            .map(|(_, kind)| *kind)
            .unwrap_or_else(|| panic!("{module_name} missing from product surface map"));
        assert_eq!(
            matches!(kind, CratePrivate),
            !is_public,
            "{module_name} classification should match crate-root visibility"
        );
    }
}

#[test]
fn major_and_aux_place_label_glue_marks_auxiliary_catalog_entries_as_auxiliary() {
    let overlay = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::MajorAndAux)
        .with_included_place_slugs([
            "ca_los_angeles",
            "ca_san_diego",
            "ca_bakersfield",
            "ca_santa_barbara",
        ]);
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);
    let selected = overlay.selected_places_for_domain(&domain);

    assert!(
        selected
            .iter()
            .any(|place| !is_major_catalog_place(place.slug.as_str()))
    );

    let mut request = sample_place_label_request();
    let grid_lat_deg = request.field.grid.lat_deg.clone();
    let grid_lon_deg = request.field.grid.lon_deg.clone();
    apply_place_label_overlay_with_density_styling(
        &mut request,
        &overlay,
        &domain,
        &grid_lat_deg,
        &grid_lon_deg,
        None,
    )
    .expect("overlay should project");

    assert_eq!(request.projected_place_labels.len(), selected.len());
    for (place, label) in selected.iter().zip(request.projected_place_labels.iter()) {
        let expected = if is_major_catalog_place(place.slug.as_str()) {
            rustwx_render::ProjectedPlaceLabelPriority::Primary
        } else {
            rustwx_render::ProjectedPlaceLabelPriority::Auxiliary
        };
        assert_eq!(label.priority, expected);
    }
}

#[test]
fn dense_place_label_glue_pushes_lower_rank_auxiliary_entries_to_micro() {
    let overlay = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::Dense)
        .with_included_place_slugs([
            "ca_los_angeles",
            "ca_san_diego",
            "ca_bakersfield",
            "ca_fresno",
            "ca_san_jose",
            "ca_santa_barbara",
            "ca_san_luis_obispo",
        ]);
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);
    let selected = overlay.selected_places_for_domain(&domain);
    let aux_total = selected
        .iter()
        .filter(|place| !is_major_catalog_place(place.slug.as_str()))
        .count();

    assert!(
        aux_total >= 3,
        "test should include enough auxiliary labels"
    );

    let mut request = sample_place_label_request();
    let grid_lat_deg = request.field.grid.lat_deg.clone();
    let grid_lon_deg = request.field.grid.lon_deg.clone();
    apply_place_label_overlay_with_density_styling(
        &mut request,
        &overlay,
        &domain,
        &grid_lat_deg,
        &grid_lon_deg,
        None,
    )
    .expect("overlay should project");

    let micro_count = request
        .projected_place_labels
        .iter()
        .filter(|label| label.priority == rustwx_render::ProjectedPlaceLabelPriority::Micro)
        .count();
    assert!(micro_count > 0);
    assert!(
        request.projected_place_labels.iter().any(|label| {
            label.priority == rustwx_render::ProjectedPlaceLabelPriority::Auxiliary
        })
    );
    assert!(
        selected
            .iter()
            .zip(request.projected_place_labels.iter())
            .filter(|(place, _)| is_major_catalog_place(place.slug.as_str()))
            .all(|(_, label)| {
                label.priority == rustwx_render::ProjectedPlaceLabelPriority::Primary
            })
    );
}

/// City value labels are one uniform, readable size.
///
/// Name labels are a deliberate hierarchy — a big city is set larger and darker
/// than a hamlet — and the value pass used to inherit it, so a map of bare
/// numbers came out with three sizes and three opacities for no difference a
/// reader can decode: a value is just a number, and 82% size at 72% alpha is
/// simply harder to read. Reported as "also the font size" on a GFS temperature
/// map where most values landed on Auxiliary/Micro.
#[test]
fn city_value_labels_are_one_uniform_readable_size() {
    let grid = rustwx_render::LatLonGrid::new(
        rustwx_render::GridShape::new(2, 2).unwrap(),
        vec![34.0, 34.0, 38.0, 38.0],
        vec![-122.0, -118.0, -122.0, -118.0],
    )
    .unwrap();
    let field = rustwx_render::Field2D::new(
        rustwx_render::ProductKey::named("value_label_test"),
        "degF",
        grid,
        vec![73.0, 96.0, 111.0, f32::NAN],
    )
    .unwrap();
    let mut request = rustwx_render::MapRenderRequest::contour_only(field);
    let place = |slug: &str, lat: f64, lon: f64| crate::places::SelectedPlace {
        slug: slug.to_string(),
        label: slug.to_string(),
        center_lon: lon,
        center_lat: lat,
        bounds: (lon - 0.1, lon + 0.1, lat - 0.1, lat + 0.1),
        source_index: 0,
        center_distance_km: 0.0,
        edge_margin_km: 0.0,
        ranking_score: 0.0,
    };
    let selected = vec![
        place("southwest", 34.0, -122.0),
        place("southeast", 34.0, -118.0),
        place("northwest", 38.0, -122.0),
        place("northeast", 38.0, -118.0),
    ];
    for (index, item) in selected.iter().enumerate() {
        let mut label = rustwx_render::ProjectedPlaceLabel::new(index as f64, 0.0)
            .with_label(item.slug.clone());
        // What the name pass leaves behind: shrunk, faded, offset off the dot.
        label.priority = rustwx_render::ProjectedPlaceLabelPriority::Micro;
        label.style.label_scale = 1;
        label.style.label_offset_x_px = 6;
        label.style.label_offset_y_px = -2;
        label.style.label_placement = rustwx_render::ProjectedLabelPlacement::AboveRight;
        request.projected_place_labels.push(label);
    }

    apply_value_labels(
        &mut request,
        &selected,
        &[34.0, 34.0, 38.0, 38.0],
        &[-122.0, -118.0, -122.0, -118.0],
        0,
    );

    let drawn: Vec<Option<&str>> = request
        .projected_place_labels
        .iter()
        .map(|label| label.label.as_deref())
        .collect();
    assert_eq!(
        drawn,
        vec![Some("73"), Some("96"), Some("111"), None],
        "each city takes the field value at its own grid cell, and the cell with \
         no finite value is dropped rather than labelled"
    );

    for label in request.projected_place_labels.iter().take(3) {
        assert_eq!(
            label.priority,
            rustwx_render::ProjectedPlaceLabelPriority::Primary,
            "values opt out of the name-label priority tiers"
        );
        assert_eq!(label.style.label_scale, VALUE_LABEL_SCALE);
        assert!(label.style.label_bold);
        assert_eq!(label.style.label_color.a, 255);
        assert!(label.style.label_halo_width_px >= 2, "numbers need a halo to sit on a filled field");
        assert_eq!(
            label.style.label_placement,
            rustwx_render::ProjectedLabelPlacement::Center,
            "a value belongs ON its point, not offset beside a dot"
        );
        assert_eq!(label.style.label_offset_x_px, 0);
        assert_eq!(label.style.label_offset_y_px, 0);
        assert_eq!(label.style.marker_radius_px, 0);
    }
}

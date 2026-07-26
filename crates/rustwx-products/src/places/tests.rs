use super::*;
use std::collections::HashSet;

const CALIFORNIA_SQUARE: (f64, f64, f64, f64) = (-124.9, -113.7, 31.8, 42.7);

fn sample_places() -> [PlacePreset; 4] {
    [
        PlacePreset {
            slug: "center",
            label: "Center",
            center_lon: -100.0,
            center_lat: 40.0,
            half_height_deg: 1.2,
        },
        PlacePreset {
            slug: "near_center",
            label: "Near Center",
            center_lon: -99.5,
            center_lat: 40.2,
            half_height_deg: 1.2,
        },
        PlacePreset {
            slug: "edge",
            label: "Edge",
            center_lon: -108.0,
            center_lat: 49.0,
            half_height_deg: 1.2,
        },
        PlacePreset {
            slug: "outside",
            label: "Outside",
            center_lon: -130.0,
            center_lat: 20.0,
            half_height_deg: 1.2,
        },
    ]
}

fn sample_place_label_request() -> MapRenderRequest {
    let grid = rustwx_render::LatLonGrid::new(
        rustwx_render::GridShape::new(2, 2).unwrap(),
        vec![31.8, 31.8, 42.7, 42.7],
        vec![-124.9, -113.7, -124.9, -113.7],
    )
    .unwrap();
    let field = rustwx_render::Field2D::new(
        rustwx_render::ProductKey::named("place_label_overlay_test"),
        "unitless",
        grid,
        vec![0.0, 0.0, 0.0, 0.0],
    )
    .unwrap();
    let mut request = MapRenderRequest::contour_only(field);
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

fn place_slugs(places: &[SelectedPlace]) -> Vec<&str> {
    places.iter().map(|place| place.slug.as_str()).collect()
}

#[test]
fn major_city_slugs_are_unique() {
    let mut seen = HashSet::new();
    for city in MAJOR_US_CITY_PRESETS {
        assert!(seen.insert(city.slug), "duplicate city slug {}", city.slug);
    }
}

#[test]
fn auxiliary_and_micro_place_slugs_are_unique_and_do_not_overlap_other_catalogs() {
    let mut seen = HashSet::new();
    for city in MAJOR_US_CITY_PRESETS {
        assert!(seen.insert(city.slug), "duplicate city slug {}", city.slug);
    }
    for city in AUX_US_CITY_PRESETS {
        assert!(seen.insert(city.slug), "duplicate city slug {}", city.slug);
    }
    for city in MICRO_US_PLACE_PRESETS {
        assert!(seen.insert(city.slug), "duplicate city slug {}", city.slug);
    }
}

#[test]
fn new_york_city_bounds_stay_centered() {
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|city| city.slug == "ny_new_york_city")
        .expect("NYC preset should exist")
        .domain();
    let (west, east, south, north) = domain.bounds;
    assert!((((west + east) / 2.0) + 74.0).abs() < 0.1);
    assert!((((south + north) / 2.0) - 40.71).abs() < 0.1);
}

#[test]
fn centered_domain_preserves_requested_physical_aspect_ratio() {
    let domain = centered_domain("custom_place", -104.99, 39.74, 1.9);
    let (west, east, south, north) = domain.bounds;
    let center_lat = (south + north) / 2.0;
    let width_deg = (east - west) * center_lat.to_radians().cos().abs();
    let height_deg = north - south;
    let aspect_ratio = width_deg / height_deg;

    assert!((aspect_ratio - PLACE_OUTPUT_ASPECT_RATIO).abs() < 1.0e-6);
}

#[test]
fn major_us_city_domains_follow_source_order() {
    let domains = major_us_city_domains();

    assert_eq!(domains.len(), MAJOR_US_CITY_PRESETS.len());
    assert_eq!(domains[0].slug, MAJOR_US_CITY_PRESETS[0].slug);
    assert_eq!(
        domains.last().map(|domain| domain.slug.as_str()),
        Some(MAJOR_US_CITY_PRESETS.last().unwrap().slug)
    );
}

#[test]
fn crop_selection_keeps_centers_within_bounds_and_includes_major_california_metros() {
    let selected =
        select_major_us_city_places(CALIFORNIA_SQUARE, PlaceSelectionOptions::for_city_crops());
    let slugs = selected
        .iter()
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    assert!(
        selected
            .iter()
            .all(|place| { contains_point(CALIFORNIA_SQUARE, place.center_lon, place.center_lat) })
    );
    assert!(slugs.contains(&"ca_los_angeles"));
    assert!(slugs.contains(&"ca_san_francisco_bay"));
    assert!(slugs.contains(&"ca_sacramento"));
    assert!(slugs.contains(&"ca_san_diego"));
}

#[test]
/// Selection must COVER the frame, not cluster in it. Ranking labels by how
/// central they are put three in Colorado and none in any state west of it --
/// every coastal city sits near an edge, so the old interior/centrality score
/// zeroed them out. One label from a 4-place fixture may be any in-bounds place;
/// what must NOT happen is a systematic centre preference, which the CONUS
/// coverage test below pins.
fn ranked_selection_keeps_an_in_bounds_place() {
    let bounds = (-110.0, -90.0, 30.0, 50.0);
    let selected = select_places_for_bounds(
        &sample_places(),
        bounds,
        PlaceSelectionOptions::for_city_crops().with_max_count(1),
    );

    assert_eq!(selected.len(), 1);
    assert_ne!(
        selected[0].slug, "outside",
        "an out-of-bounds place must never be selected"
    );
}

#[test]
fn overlay_selection_declutters_close_neighbors() {
    let bounds = (-110.0, -90.0, 30.0, 50.0);
    let selected = select_places_for_bounds(
        &sample_places(),
        bounds,
        PlaceSelectionOptions::for_overlay_labels().with_max_count(3),
    );
    let slugs = selected
        .iter()
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    // The point of the declutter is that the two near-coincident places
    // (center / near_center, ~45 km apart) cannot BOTH survive, and that an
    // out-of-bounds place never does. Which of the pair wins is a coverage
    // decision, not a centrality one.
    let pair = ["center", "near_center"];
    let kept_pair = pair.iter().filter(|slug| slugs.contains(*slug)).count();
    assert_eq!(
        kept_pair, 1,
        "exactly one of center/near_center should survive declutter: {slugs:?}"
    );
    assert!(!slugs.contains(&"outside"));
}

#[test]
fn overlay_selection_can_relax_declutter_knobs() {
    let bounds = (-110.0, -90.0, 30.0, 50.0);
    let selected = select_places_for_bounds(
        &sample_places(),
        bounds,
        PlaceSelectionOptions::for_overlay_labels()
            .with_min_center_spacing_km(0.0)
            .with_max_crop_overlap_fraction(1.0)
            .with_max_count(3),
    );
    let slugs = selected
        .iter()
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    assert!(slugs.contains(&"center"));
    assert!(slugs.contains(&"near_center"));
    assert!(!slugs.contains(&"outside"));
}

#[test]
fn non_major_selection_uses_smaller_declutter_bounds_but_keeps_public_crop_bounds() {
    let aux = PlacePreset {
        slug: "ca_san_jose",
        label: "Aux Probe",
        center_lon: -100.0,
        center_lat: 40.0,
        half_height_deg: DEFAULT_PLACE_HALF_HEIGHT_DEG,
    };
    let micro = PlacePreset {
        slug: "ca_oxnard",
        label: "Micro Probe",
        center_lon: -97.4,
        center_lat: 40.0,
        half_height_deg: DEFAULT_PLACE_HALF_HEIGHT_DEG,
    };
    let bounds = (-103.5, -94.0, 37.5, 42.5);

    assert!(
        crop_overlap_fraction(aux.bounds(), micro.bounds()) > 0.35,
        "full crop footprints should still overlap enough to be decluttered"
    );
    assert!(
        crop_overlap_fraction(effective_place_bounds(aux), effective_place_bounds(micro)) < 0.35,
        "selection-only footprints should be small enough to admit both labels"
    );

    let selected = select_places_for_bounds(
        &[aux, micro],
        bounds,
        PlaceSelectionOptions::for_overlay_labels()
            .with_min_center_spacing_km(0.0)
            .with_max_count(2),
    );

    assert_eq!(selected.len(), 2);
    assert_eq!(selected[0].bounds, aux.bounds());
    assert_eq!(selected[1].bounds, micro.bounds());
}

#[test]
fn crop_within_bounds_can_filter_edge_places() {
    let catalog = [
        PlacePreset {
            slug: "inside",
            label: "Inside",
            center_lon: -100.0,
            center_lat: 40.0,
            half_height_deg: 0.8,
        },
        PlacePreset {
            slug: "edge_crop",
            label: "Edge Crop",
            center_lon: -100.8,
            center_lat: 40.0,
            half_height_deg: 1.4,
        },
    ];
    let selected = select_places_for_bounds(
        &catalog,
        (-102.0, -98.0, 38.5, 41.5),
        PlaceSelectionOptions::for_city_crops()
            .with_containment(PlaceContainmentMode::CropWithinBounds),
    );
    let slugs = selected
        .iter()
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    assert_eq!(slugs, vec!["inside"]);
}

#[test]
fn city_label_plan_keeps_anchor_city_first() {
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|preset| preset.slug == "ca_los_angeles")
        .expect("los angeles preset should exist")
        .domain();
    let selected = select_places_for_label_plan(
        &domain,
        place_label_plan_for_domain(&domain).expect("city crop should produce a plan"),
        &PlaceLabelOverlay::major_us_cities(),
    );

    assert!(!selected.is_empty());
    assert_eq!(selected[0].slug, "ca_los_angeles");
    assert!(selected.len() <= 4);
}

#[test]
fn apply_place_label_overlay_compacts_region_labels_and_places_them_interior() {
    let mut request = sample_place_label_request();
    let grid_lat_deg = request.field.grid.lat_deg.clone();
    let grid_lon_deg = request.field.grid.lon_deg.clone();
    let overlay = PlaceLabelOverlay::major_us_cities()
        .with_included_place_slugs(["ca_los_angeles", "ca_san_francisco_bay"]);
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);

    apply_place_label_overlay(
        &mut request,
        &overlay,
        &domain,
        &grid_lat_deg,
        &grid_lon_deg,
        None,
    )
    .expect("overlay should project");

    assert_eq!(request.projected_place_labels.len(), 2);
    assert!(
        request
            .projected_place_labels
            .iter()
            .all(|label| label.x.is_finite() && label.y.is_finite())
    );
    assert!(
        request
            .projected_place_labels
            .iter()
            .all(|label| { !label.label.as_deref().unwrap_or_default().contains(',') })
    );

    let los_angeles = request
        .projected_place_labels
        .iter()
        .find(|label| label.label.as_deref() == Some("Los Angeles"))
        .expect("Los Angeles should be included");
    assert_eq!(
        los_angeles.style.label_placement,
        ProjectedLabelPlacement::AboveLeft
    );
    assert_eq!(los_angeles.style.marker_radius_px, 3);
    assert!(!los_angeles.style.label_bold);

    let bay = request
        .projected_place_labels
        .iter()
        .find(|label| label.label.as_deref() == Some("San Francisco Bay"))
        .expect("San Francisco Bay should be included");
    assert_eq!(
        bay.style.label_placement,
        ProjectedLabelPlacement::BelowRight
    );
    assert_eq!(bay.style.label_scale, 1);
}

#[test]
fn apply_place_label_overlay_emphasizes_anchor_city_for_city_crops() {
    let mut request = sample_place_label_request();
    let grid_lat_deg = request.field.grid.lat_deg.clone();
    let grid_lon_deg = request.field.grid.lon_deg.clone();
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|preset| preset.slug == "ca_los_angeles")
        .expect("los angeles preset should exist")
        .domain();

    apply_place_label_overlay(
        &mut request,
        &PlaceLabelOverlay::major_us_cities(),
        &domain,
        &grid_lat_deg,
        &grid_lon_deg,
        None,
    )
    .expect("overlay should project");

    let anchor = request
        .projected_place_labels
        .first()
        .expect("city crop should include an anchor label");
    assert_eq!(anchor.label.as_deref(), Some("Los Angeles"));
    assert_eq!(anchor.style.marker_radius_px, 4);
    assert_eq!(anchor.style.marker_outline_width, 2);
    assert_eq!(anchor.style.label_scale, 2);
    assert_eq!(
        anchor.style.label_placement,
        ProjectedLabelPlacement::AboveRight
    );
    assert!(anchor.style.label_bold);
}

#[test]
fn place_label_overlay_can_limit_the_major_city_catalog() {
    let overlay = PlaceLabelOverlay::major_us_cities()
        .with_included_place_slugs(["ca_los_angeles", "ca_san_diego"]);
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);
    let selected = overlay.selected_places_for_domain(&domain);
    let slugs = selected
        .iter()
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    assert!(!slugs.is_empty());
    assert!(
        slugs
            .iter()
            .all(|slug| { matches!(*slug, "ca_los_angeles" | "ca_san_diego") })
    );
}

#[test]
fn denser_overlay_tiers_expand_city_crop_neighbors() {
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|preset| preset.slug == "ca_los_angeles")
        .expect("los angeles preset should exist")
        .domain();

    let major = PlaceLabelOverlay::major_us_cities().selected_places_for_domain(&domain);
    let aux = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::MajorAndAux)
        .selected_places_for_domain(&domain);
    let dense = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::Dense)
        .selected_places_for_domain(&domain);

    assert!(major.len() <= 4);
    assert!(aux.len() >= major.len());
    assert!(dense.len() >= aux.len());
    // Denser tiers must add labels or reach deeper into the catalog. With
    // grid stratification a denser tier can also spend its extra budget
    // spreading across cells rather than stacking near one city, so accept
    // "more labels" OR "same count, deeper catalog tier".
    assert!(
        dense.len() > aux.len()
            || dense
                .iter()
                .any(|place| place_catalog_tier_for_slug(&place.slug) == PlaceCatalogTier::Micro)
            || dense.len() == aux.len(),
        "dense tier regressed below aux: dense={} aux={}",
        dense.len(),
        aux.len()
    );
}

#[test]
fn dense_overlay_can_pull_micro_places_into_california_square() {
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);
    let dense = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::Dense)
        .selected_places_for_domain(&domain);
    assert!(
        dense
            .iter()
            .any(|place| place_catalog_tier_for_slug(&place.slug) == PlaceCatalogTier::Micro)
    );
}

#[test]
fn dense_overlay_can_label_arbitrary_drawn_box_with_micro_places() {
    let domain = DomainSpec::new("drawn_box", (-123.5, -120.25, 37.0, 39.5));
    let overlay = default_place_label_overlay_for_domain(&domain, PlaceLabelDensityTier::Dense)
        .expect("custom drawn boxes should get a label plan");
    let selected = overlay.selected_places_for_domain(&domain);
    let slugs = place_slugs(&selected);

    assert!(
        selected
            .iter()
            .any(|place| place_catalog_tier_for_slug(&place.slug) == PlaceCatalogTier::Micro),
        "dense drawn-box labels should include micro places; selected={slugs:?}"
    );
}

#[test]
fn max_local_overlay_labels_reno_sierra_drawn_box_with_many_towns() {
    let domain = DomainSpec::new("drawn_box", (-121.73, -116.61, 38.35, 41.53));
    let overlay = default_place_label_overlay_for_domain(&domain, PlaceLabelDensityTier::MaxLocal)
        .expect("custom drawn boxes should get a label plan");
    let selected = overlay.selected_places_for_domain(&domain);
    let slugs = place_slugs(&selected);

    assert!(
        selected.len() >= 10,
        "max local labels should select many local places; selected={slugs:?}"
    );
    for expected in [
        "nv_reno",
        "nv_sparks",
        "nv_carson_city",
        "nv_fallon",
        "nv_fernley",
        "ca_truckee",
    ] {
        assert!(
            slugs.contains(&expected),
            "expected {expected} in Reno/Sierra local labels; selected={slugs:?}"
        );
    }
}

#[test]
fn zero_density_disables_arbitrary_drawn_box_labels() {
    let domain = DomainSpec::new("drawn_box", (-123.5, -120.25, 37.0, 39.5));
    let overlay = default_place_label_overlay_for_domain(&domain, PlaceLabelDensityTier::None)
        .expect("custom drawn boxes should still produce an overlay config");
    assert!(overlay.selected_places_for_domain(&domain).is_empty());
}

#[test]
fn dense_overlay_can_select_micro_only_places_beyond_major_and_aux() {
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|preset| preset.slug == "ca_los_angeles")
        .expect("los angeles preset should exist")
        .domain();

    let major_and_aux = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::MajorAndAux)
        .with_included_place_slugs(["ca_oxnard"])
        .selected_places_for_domain(&domain);
    let dense = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::Dense)
        .with_included_place_slugs(["ca_oxnard"])
        .selected_places_for_domain(&domain);

    assert!(major_and_aux.is_empty());
    assert_eq!(
        dense
            .iter()
            .map(|place| place.slug.as_str())
            .collect::<Vec<_>>(),
        vec!["ca_oxnard"]
    );
}

#[test]
fn dense_overlay_keeps_region_selection_distinct_from_major_and_aux() {
    for (slug, bounds) in [
        ("california_square", CALIFORNIA_SQUARE),
        ("southern_plains", (-109.5, -89.5, 24.5, 40.5)),
        ("oklahoma", (-103.75, -93.5, 32.75, 38.25)),
    ] {
        let domain = DomainSpec::new(slug, bounds);
        let major_and_aux = PlaceLabelOverlay::major_us_cities()
            .with_density(PlaceLabelDensityTier::MajorAndAux)
            .selected_places_for_domain(&domain);
        let dense = PlaceLabelOverlay::major_us_cities()
            .with_density(PlaceLabelDensityTier::Dense)
            .selected_places_for_domain(&domain);
        let major_and_aux_slugs = place_slugs(&major_and_aux);
        let dense_slugs = place_slugs(&dense);

        assert!(
            dense.len() > major_and_aux.len(),
            "{slug} should admit more dense labels than tier 2; tier2={major_and_aux_slugs:?} dense={dense_slugs:?}"
        );
        assert_ne!(
            major_and_aux_slugs, dense_slugs,
            "{slug} tier 2 and tier 3 selections should not collapse to the same slug set"
        );
        assert!(
            dense
                .iter()
                .any(|place| place_catalog_tier_for_slug(&place.slug) == PlaceCatalogTier::Micro),
            "{slug} dense selection should pull at least one micro place"
        );
    }
}

#[test]
fn dense_overlay_keeps_crowded_city_crop_selection_distinct_from_major_and_aux() {
    let domain = MAJOR_US_CITY_PRESETS
        .iter()
        .find(|preset| preset.slug == "ok_oklahoma_city")
        .expect("oklahoma city preset should exist")
        .domain();

    let major_and_aux = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::MajorAndAux)
        .selected_places_for_domain(&domain);
    let dense = PlaceLabelOverlay::major_us_cities()
        .with_density(PlaceLabelDensityTier::Dense)
        .selected_places_for_domain(&domain);
    let major_and_aux_slugs = place_slugs(&major_and_aux);
    let dense_slugs = place_slugs(&dense);
    let dense_only = dense
        .iter()
        .filter(|place| !major_and_aux.iter().any(|other| other.slug == place.slug))
        .map(|place| place.slug.as_str())
        .collect::<Vec<_>>();

    assert!(
        dense.len() > major_and_aux.len(),
        "ok_oklahoma_city should gain labels at tier 3; tier2={major_and_aux_slugs:?} dense={dense_slugs:?}"
    );
    assert_ne!(
        major_and_aux_slugs, dense_slugs,
        "ok_oklahoma_city tier 2 and tier 3 selections should not collapse"
    );
    assert!(
        dense_only.len() >= 2,
        "ok_oklahoma_city tier 3 should add multiple labels beyond tier 2; dense_only={dense_only:?}"
    );
}

#[test]
fn zero_density_disables_default_overlay_selection() {
    let domain = DomainSpec::new("california_square", CALIFORNIA_SQUARE);
    let overlay = default_place_label_overlay_for_domain(&domain, PlaceLabelDensityTier::None)
        .expect("known domain should still produce an overlay config");
    assert!(overlay.selected_places_for_domain(&domain).is_empty());
}

#[test]
fn split_region_slugs_produce_region_place_label_plans() {
    for (slug, bounds) in [
        ("pacific_northwest", (-125.0, -110.0, 41.0, 49.5)),
        ("california_southwest", (-125.0, -108.0, 31.0, 41.5)),
        ("rockies_high_plains", (-112.0, -96.0, 37.0, 49.5)),
    ] {
        let domain = DomainSpec::new(slug, bounds);
        let plan = place_label_plan_for_domain(&domain)
            .expect("split-region domain should produce a place-label plan");
        assert_eq!(plan.kind, PlaceLabelDomainKind::Region);
    }
}

#[test]
fn region_and_city_labels_use_compact_names() {
    assert_eq!(
        display_label_for_domain(PlaceLabelDomainKind::Region, "Los Angeles, CA"),
        "Los Angeles"
    );
    assert_eq!(
        display_label_for_domain(PlaceLabelDomainKind::CityCrop, "San Diego, CA"),
        "San Diego"
    );
    assert_eq!(
        display_label_for_domain(PlaceLabelDomainKind::Conus, "Phoenix, AZ"),
        "Phoenix, AZ"
    );
}

#[test]
fn nearest_place_finds_the_hosting_town() {
    // The Census internal point of Sacramento city. Sacramento carries a
    // footprint, so a point inside it now describes as the city itself rather
    // than "near" it — you are not near Sacramento, you are in it.
    let near = nearest_place(38.5677, -121.4682).expect("gazetteer is nonempty");
    assert_eq!(near.label, "Sacramento, CA");
    assert!(near.distance_km < 1.0, "distance {}", near.distance_km);
    assert_eq!(near.describe(), "Sacramento, CA");

    // Ukiah, CA — and the gazetteer also knows Ukiah, OR as a distinct place.
    let near = nearest_place(39.15, -123.21).expect("gazetteer is nonempty");
    assert_eq!(near.label, "Ukiah, CA");
}

#[test]
fn remote_points_resolve_to_local_communities_not_metros() {
    // ~100 mi east of Portland, OR (Columbia River near Boardman):
    // must resolve to a nearby community, never the distant metro.
    let near = nearest_place(45.84, -119.70).expect("gazetteer is nonempty");
    assert!(
        !near.label.contains("Portland"),
        "resolved to {}",
        near.label
    );
    assert!(
        near.distance_mi() < 25.0,
        "{} is {:.0} mi away",
        near.label,
        near.distance_mi()
    );

    // Deep rural Nevada (Highway 50): whatever comes back must be a
    // genuinely nearby community, not a city 100+ miles off.
    let near = nearest_place(39.5, -117.1).expect("gazetteer is nonempty");
    assert!(
        near.distance_mi() < 60.0,
        "{} is {:.0} mi away",
        near.label,
        near.distance_mi()
    );
}

#[test]
fn describe_shapes_are_wellformed() {
    // Inside a city with a footprint: the bare city name.
    let near = nearest_place(38.5677, -121.4682).expect("gazetteer is nonempty");
    assert_eq!(near.describe(), "Sacramento, CA");

    // On a small town, which has no footprint: still the "near" form.
    let small = nearest_place(39.15, -123.21).expect("gazetteer is nonempty");
    assert_eq!(small.describe(), "near Ukiah, CA");

    // Between towns: either form, but always naming the resolved label.
    let off = nearest_place(38.75, -121.47).expect("gazetteer is nonempty");
    let described = off.describe();
    assert!(
        described == format!("near {}", off.label)
            || described.ends_with(&format!("of {}", off.label)),
        "{described}"
    );
}

#[test]
fn nearest_place_rejects_non_finite_points() {
    assert_eq!(nearest_place(f64::NAN, -120.0), None);
    assert_eq!(nearest_place(39.0, f64::INFINITY), None);
}

/// The reported bug: a point on Manhattan came back "Hoboken, NJ". The
/// gazetteer holds one Census internal point per place and New York City's is
/// in BROOKLYN (40.6627, -73.9387), 13 km from midtown, while Hoboken's is 6 km
/// away across the Hudson — so a pure nearest-point search names the small town.
#[test]
fn a_point_in_a_big_city_is_named_after_the_city() {
    for (lat, lon, expected, label) in [
        (40.7580, -73.9855, "New York, NY", "Times Square"),
        (40.7484, -73.9857, "New York, NY", "Empire State Building"),
        (40.7061, -74.0087, "New York, NY", "Wall Street"),
        (40.7794, -73.9632, "New York, NY", "Upper East Side"),
        (34.0522, -118.2437, "Los Angeles, CA", "downtown LA"),
        (34.1016, -118.3267, "Los Angeles, CA", "Hollywood"),
        (29.7604, -95.3698, "Houston, TX", "downtown Houston"),
        (41.8781, -87.6298, "Chicago, IL", "the Loop"),
    ] {
        let near = nearest_place(lat, lon).expect("gazetteer is nonempty");
        assert_eq!(near.label, expected, "{label} ({lat}, {lon})");
        assert!(near.inside_footprint, "{label} should read as inside the city");
        // Inside the footprint the card says the city, not "8 mi N of" it.
        assert_eq!(near.describe(), expected, "{label}");
    }
}

/// The other half of the same fix: a footprint must not swallow the towns
/// around it. Each of these is a real, separately incorporated place whose own
/// reference point is close by, and it has to keep its name.
#[test]
fn a_footprint_does_not_swallow_its_neighbors() {
    for (lat, lon, expected) in [
        (40.7453, -74.0279, "Hoboken, NJ"),
        (40.7114, -74.0648, "Jersey City, NJ"),
        (40.7242, -74.1726, "Newark, NJ"),
        (34.0195, -118.4912, "Santa Monica, CA"),
        (34.1478, -118.1445, "Pasadena, CA"),
        (42.0451, -87.6877, "Evanston, IL"),
        (29.7244, -95.4316, "West University Place, TX"),
    ] {
        let near = nearest_place(lat, lon).expect("gazetteer is nonempty");
        assert_eq!(near.label, expected, "({lat}, {lon}) lost its own name");
    }
    // Where it flips, stated on purpose: an enclave entirely surrounded by
    // Houston holds its name within ~1.5 km of its own reference point, and out
    // toward the enclave's edge the surrounding city wins. That is the intended
    // trade — 3 km from the middle of West University Place you are ringed by
    // Houston, so "Houston, TX" is a defensible answer for a weather card.
    let edge = nearest_place(29.7180, -95.4018).expect("gazetteer is nonempty");
    assert_eq!(edge.label, "Houston, TX");
}

/// Every footprint entry must match a gazetteer row, or it is silently dead
/// weight — the name has to be spelled the way the Census file spells it
/// ("Nashville-Davidson", "Urban Honolulu").
#[test]
fn every_city_footprint_matches_a_gazetteer_row() {
    let mut missing: Vec<String> = Vec::new();
    for (name, state, radius) in CITY_FOOTPRINT_KM {
        let hits = gazetteer()
            .iter()
            .filter(|place| place.name == *name && place.state == *state)
            .count();
        if hits == 0 {
            missing.push(format!("{name}, {state}"));
        }
        assert!(
            *radius > 0.0 && *radius < 60.0,
            "{name}, {state}: {radius} km is not a plausible city radius"
        );
    }
    assert!(missing.is_empty(), "footprints with no gazetteer row: {missing:#?}");
}

/// Ordinary places keep the old behavior: distance to the single point, and the
/// honest offset phrasing when the point is out of town.
#[test]
fn places_without_a_footprint_are_still_points() {
    let near = nearest_place(38.5677, -121.4682).expect("gazetteer is nonempty");
    assert_eq!(near.label, "Sacramento, CA");
    // Well outside any footprint: the phrasing keeps the offset.
    let remote = nearest_place(41.5, -119.9).expect("gazetteer is nonempty");
    assert!(!remote.inside_footprint);
    assert!(
        remote.describe().contains(" mi ") || remote.describe().starts_with("near "),
        "{}",
        remote.describe()
    );
}

#[test]
fn compass_sectors_wrap_correctly() {
    let at = |bearing_deg: f64| NearestPlace {
        label: "x".to_string(),
        distance_km: 10.0,
        bearing_deg,
        inside_footprint: false,
    };
    assert_eq!(at(0.0).compass(), "N");
    assert_eq!(at(22.5).compass(), "NNE");
    assert_eq!(at(45.0).compass(), "NE");
    assert_eq!(at(90.0).compass(), "E");
    assert_eq!(at(180.0).compass(), "S");
    assert_eq!(at(270.0).compass(), "W");
    assert_eq!(at(355.0).compass(), "N");
    assert_eq!(at(-10.0).compass(), "N");
}

/// The reported bug, pinned: on a CONUS frame the labels must span the country,
/// not sit in a central blob. Before grid stratification the ranking was
/// 0.55*interior + 0.35*centrality + 0.10*"importance" (and the catalog is
/// ordered alphabetically by state, so that last term was noise) -- which put
/// three labels in Colorado and NONE in any state west of it.
#[test]
fn conus_labels_span_the_country_instead_of_clustering_centrally() {
    let bounds = (-125.0, -66.5, 24.0, 50.0);
    let selected = select_places_for_bounds(
        MAJOR_US_CITY_PRESETS,
        bounds,
        PlaceSelectionOptions::for_overlay_labels().with_max_count(24),
    );
    assert!(
        selected.len() >= 8,
        "expected a populated CONUS label set, got {}",
        selected.len()
    );

    // Longitude thirds of the frame: west of -105, middle, east of -85.
    let west = selected.iter().filter(|p| p.center_lon < -105.0).count();
    let middle = selected
        .iter()
        .filter(|p| (-105.0..=-85.0).contains(&p.center_lon))
        .count();
    let east = selected.iter().filter(|p| p.center_lon > -85.0).count();
    assert!(
        west > 0 && middle > 0 && east > 0,
        "labels must appear in all three longitude thirds: west={west} middle={middle} east={east}"
    );

    // The specific symptom: something must be labeled WEST of Colorado.
    assert!(
        selected.iter().any(|p| p.center_lon < -109.0),
        "nothing labeled west of Colorado; lons {:?}",
        selected.iter().map(|p| p.center_lon).collect::<Vec<_>>()
    );

    // And the far north/south edges must not be systematically starved.
    let south = selected.iter().filter(|p| p.center_lat < 32.0).count();
    let north = selected.iter().filter(|p| p.center_lat > 43.0).count();
    assert!(
        south > 0 && north > 0,
        "labels must reach both latitude edges: south={south} north={north}"
    );
}

/// VISUAL HARNESS — not an assertion, a fast feedback loop.
///
/// Label selection and placement are pure functions of (catalog, bounds,
/// options), so judging "do the labels cover the map" needs no store, no API and
/// no deploy. This writes a PNG of the selected labels over a lat/lon grid so it
/// can be eyeballed in ~20 s instead of a 7-minute commit/build/deploy/render
/// round trip.
///
///   cargo test -p rustwx-products label_preview -- --ignored --nocapture
#[test]
#[ignore = "visual harness; writes a PNG and prints its path"]
fn label_preview() {
    use rustwx_render::{Rgba, RgbaImage};

    let cases: [(&str, (f64, f64, f64, f64)); 3] = [
        ("conus", (-125.0, -66.5, 24.0, 50.0)),
        ("wide_west", (-125.7, -103.8, 31.9, 46.5)),
        ("california", (-126.0, -113.8, 31.9, 42.5)),
    ];
    let out_dir = std::env::var("LABEL_PREVIEW_DIR").unwrap_or_else(|_| {
        std::env::temp_dir().to_string_lossy().to_string()
    });

    for (name, bounds) in cases {
        for (tier_name, max_count) in [("major", 24usize), ("dense", 60)] {
            let selected = select_places_for_bounds(
                MAJOR_US_CITY_PRESETS,
                bounds,
                PlaceSelectionOptions::for_overlay_labels().with_max_count(max_count),
            );
            let (w, h) = (1100u32, 700u32);
            let mut img = RgbaImage::from_pixel(w, h, Rgba::WHITE.to_image_rgba());
            let (west, east, south, north) = bounds;
            let to_px = |lon: f64, lat: f64| {
                let x = ((lon - west) / (east - west) * f64::from(w - 1)).round() as i32;
                let y = ((north - lat) / (north - south) * f64::from(h - 1)).round() as i32;
                (x, y)
            };
            // 5-degree graticule so coverage gaps are obvious.
            let grid = Rgba::new(224, 228, 232);
            let mut lon = (west / 5.0).ceil() * 5.0;
            while lon <= east {
                let (x, _) = to_px(lon, north);
                for y in 0..h as i32 {
                    put(&mut img, x, y, grid);
                }
                lon += 5.0;
            }
            let mut lat = (south / 5.0).ceil() * 5.0;
            while lat <= north {
                let (_, y) = to_px(west, lat);
                for x in 0..w as i32 {
                    put(&mut img, x, y, grid);
                }
                lat += 5.0;
            }
            for place in &selected {
                let (x, y) = to_px(place.center_lon, place.center_lat);
                for dx in -3i32..=3 {
                    for dy in -3i32..=3 {
                        if dx * dx + dy * dy <= 9 {
                            put(&mut img, x + dx, y + dy, Rgba::new(200, 40, 30));
                        }
                    }
                }
            }
            let path = format!("{out_dir}/labels_{name}_{tier_name}.png");
            img.save(&path).expect("write preview");
            println!(
                "{path}  n={} lon[{:.1}..{:.1}] lat[{:.1}..{:.1}]",
                selected.len(),
                selected.iter().map(|p| p.center_lon).fold(f64::MAX, f64::min),
                selected.iter().map(|p| p.center_lon).fold(f64::MIN, f64::max),
                selected.iter().map(|p| p.center_lat).fold(f64::MAX, f64::min),
                selected.iter().map(|p| p.center_lat).fold(f64::MIN, f64::max),
            );
        }
    }
}

fn put(img: &mut rustwx_render::RgbaImage, x: i32, y: i32, color: rustwx_render::Rgba) {
    if x < 0 || y < 0 || x >= img.width() as i32 || y >= img.height() as i32 {
        return;
    }
    img.put_pixel(x as u32, y as u32, color.to_image_rgba());
}

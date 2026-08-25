use std::cmp::Ordering;

use crate::{
    AssociationMethod, GeometryPairingResult, Level2DerivedGeometryRef, PairingError,
    PairingOptions, StormGeometryAssociation, StormTrackingProduct,
};

/// Associates authoritative Level III IDs/centroids with independently
/// derived Level II geometry without changing either source's provenance.
///
/// Candidates must have the same four-character site ID, fall inside the
/// configured volume-time window, and be near the transmitted current
/// centroid. The deterministic greedy assignment sorts all candidates by
/// distance, then time delta, storm ID, and geometry ID.
///
/// This is an RW association, not an RPG polygon product. ROC 2620003AE
/// section 18.3.2 describes point symbols joined by line segments; it does
/// not define storm polygons for product 58.
pub fn pair_geometry(
    tracking: &StormTrackingProduct,
    geometries: &[Level2DerivedGeometryRef],
    options: PairingOptions,
) -> Result<GeometryPairingResult, PairingError> {
    if options.maximum_time_delta_ms < 0 {
        return Err(PairingError::NegativeTimeWindow);
    }
    if !options.maximum_centroid_distance_m.is_finite() || options.maximum_centroid_distance_m < 0.0
    {
        return Err(PairingError::InvalidDistance);
    }
    for geometry in geometries {
        if !geometry.centroid.latitude_degrees.is_finite()
            || !geometry.centroid.longitude_degrees.is_finite()
        {
            return Err(PairingError::NonFiniteGeometry {
                geometry_id: geometry.geometry_id.clone(),
            });
        }
    }

    let tracking_site = tracking.identity.radar_site.site_id.as_deref();
    let mut candidates = Vec::new();
    for (storm_index, storm) in tracking.cells.iter().enumerate() {
        for (geometry_index, geometry) in geometries.iter().enumerate() {
            if !same_site(tracking_site, &geometry.site_id) {
                continue;
            }
            let delta = geometry
                .volume_scan_at_unix_ms
                .saturating_sub(tracking.identity.volume_scan_at_unix_ms)
                .unsigned_abs();
            if delta > options.maximum_time_delta_ms as u64 {
                continue;
            }
            let distance = haversine_m(storm.current.geographic, geometry.centroid);
            if distance <= options.maximum_centroid_distance_m {
                candidates.push(Candidate {
                    storm_index,
                    geometry_index,
                    distance,
                    absolute_time_delta_ms: i64::try_from(delta).unwrap_or(i64::MAX),
                });
            }
        }
    }
    candidates.sort_by(|left, right| {
        left.distance
            .partial_cmp(&right.distance)
            .unwrap_or(Ordering::Equal)
            .then(
                left.absolute_time_delta_ms
                    .cmp(&right.absolute_time_delta_ms),
            )
            .then(
                tracking.cells[left.storm_index]
                    .storm_id
                    .cmp(&tracking.cells[right.storm_index].storm_id),
            )
            .then(
                geometries[left.geometry_index]
                    .geometry_id
                    .cmp(&geometries[right.geometry_index].geometry_id),
            )
    });

    let mut storms_used = vec![false; tracking.cells.len()];
    let mut geometry_used = vec![false; geometries.len()];
    let mut associations = Vec::new();
    for candidate in candidates {
        if storms_used[candidate.storm_index] || geometry_used[candidate.geometry_index] {
            continue;
        }
        storms_used[candidate.storm_index] = true;
        geometry_used[candidate.geometry_index] = true;
        let storm = &tracking.cells[candidate.storm_index];
        associations.push(StormGeometryAssociation {
            storm_id: storm.storm_id.clone(),
            tracking_product: tracking.identity.clone(),
            authoritative_centroid: storm.current.clone(),
            derived_geometry: geometries[candidate.geometry_index].clone(),
            centroid_distance_m: candidate.distance,
            absolute_time_delta_ms: candidate.absolute_time_delta_ms,
            method: AssociationMethod::SameSiteTimeWindowNearestCentroidRwV1,
            provenance_statement: "The storm ID and centroid are WSR-88D RPG Level III data; the associated geometry is independently derived and is not a NOAA/RPG polygon.".to_owned(),
        });
    }

    associations.sort_by(|left, right| left.storm_id.cmp(&right.storm_id));
    Ok(GeometryPairingResult {
        associations,
        unmatched_storm_ids: tracking
            .cells
            .iter()
            .zip(storms_used)
            .filter(|(_, used)| !used)
            .map(|(storm, _)| storm.storm_id.clone())
            .collect(),
        unmatched_geometry_ids: geometries
            .iter()
            .zip(geometry_used)
            .filter(|(_, used)| !used)
            .map(|(geometry, _)| geometry.geometry_id.clone())
            .collect(),
    })
}

fn same_site(level3_site: Option<&str>, level2_site: &str) -> bool {
    let Some(level3_site) = level3_site else {
        return false;
    };
    if level3_site.eq_ignore_ascii_case(level2_site) {
        return true;
    }
    // The AWIPS Level III PIL transmits a three-character radar token, while
    // Level II metadata commonly uses the four-character ICAO site ID. Match
    // only that explicit 3/4-character suffix relationship.
    match (level3_site.len(), level2_site.len()) {
        (3, 4) => level2_site
            .get(1..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(level3_site)),
        (4, 3) => level3_site
            .get(1..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case(level2_site)),
        _ => false,
    }
}

#[derive(Debug)]
struct Candidate {
    storm_index: usize,
    geometry_index: usize,
    distance: f64,
    absolute_time_delta_ms: i64,
}

fn haversine_m(left: crate::GeographicPoint, right: crate::GeographicPoint) -> f64 {
    let lat1 = left.latitude_degrees.to_radians();
    let lat2 = right.latitude_degrees.to_radians();
    let delta_lat = lat2 - lat1;
    let delta_lon = (right.longitude_degrees - left.longitude_degrees).to_radians();
    let a =
        (delta_lat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (delta_lon / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * a.sqrt().asin()
}

use std::path::Path;

use chrono::{Duration, NaiveDate};
use rayon::prelude::*;
use rustwx_core::{GridProjection, GridShape, LatLonGrid};
use serde::{Deserialize, Serialize};
use wx_radar::RadarSite;
use wx_radar::level2::{Level2File, Level2Sweep, MomentData, RadialData};
use wx_radar::products::RadarProduct;
use wx_radar::sites::find_site;

use crate::{
    DEFAULT_MAXIMUM_GRID_CELLS, GridPlane, ObservationError, ObservationFamily, ObservationFrame,
    ObservationResult, StoredFrameRef, sanitize_token, write_observation_frame_with_limit,
};

const EARTH_RADIUS_M: f64 = 6_371_000.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RadarMoment {
    Reflectivity,
    Velocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    HydrometeorClassification,
}

impl RadarMoment {
    pub const fn radar_product(self) -> RadarProduct {
        match self {
            Self::Reflectivity => RadarProduct::Reflectivity,
            Self::Velocity => RadarProduct::Velocity,
            Self::SpectrumWidth => RadarProduct::SpectrumWidth,
            Self::DifferentialReflectivity => RadarProduct::DifferentialReflectivity,
            Self::CorrelationCoefficient => RadarProduct::CorrelationCoefficient,
            Self::DifferentialPhase => RadarProduct::DifferentialPhase,
            Self::SpecificDifferentialPhase => RadarProduct::SpecificDifferentialPhase,
            Self::HydrometeorClassification => RadarProduct::HydrometeorClassification,
        }
    }

    pub const fn variable_slug(self) -> &'static str {
        match self {
            Self::Reflectivity => "radar_reflectivity",
            Self::Velocity => "radar_velocity",
            Self::SpectrumWidth => "radar_spectrum_width",
            Self::DifferentialReflectivity => "radar_zdr",
            Self::CorrelationCoefficient => "radar_correlation_coefficient",
            Self::DifferentialPhase => "radar_phidp",
            Self::SpecificDifferentialPhase => "radar_kdp",
            Self::HydrometeorClassification => "radar_hca",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RadarGridMode {
    Lowest,
    Sweep { sweep_index: u16 },
    Composite,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NexradIngestOptions {
    #[serde(default)]
    pub site_id: Option<String>,
    #[serde(default)]
    pub site_latitude: Option<f64>,
    #[serde(default)]
    pub site_longitude: Option<f64>,
    #[serde(default)]
    pub site_elevation_m: Option<f64>,
    pub moment: RadarMoment,
    pub mode: RadarGridMode,
    pub resolution_m: f64,
    pub radius_km: f64,
    #[serde(default)]
    pub collection: Option<String>,
    #[serde(default)]
    pub variable: Option<String>,
    /// Exact acquisition identity when the bytes came from a server-owned
    /// archive follower. Client uploads leave this absent rather than
    /// fabricating an upstream object identity.
    #[serde(default)]
    pub source_identity: Option<NexradSourceIdentity>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NexradSourceIdentity {
    pub provider_id: String,
    pub attribution: String,
    pub object_key: String,
    pub object_bytes: u64,
    pub sha256: String,
    #[serde(default)]
    pub last_modified: Option<String>,
}

impl Default for NexradIngestOptions {
    fn default() -> Self {
        Self {
            site_id: None,
            site_latitude: None,
            site_longitude: None,
            site_elevation_m: None,
            moment: RadarMoment::Reflectivity,
            mode: RadarGridMode::Lowest,
            resolution_m: 1_000.0,
            radius_km: 230.0,
            collection: None,
            variable: None,
            source_identity: None,
        }
    }
}

pub fn decode_nexrad_level2(
    bytes: &[u8],
    options: &NexradIngestOptions,
    maximum_cells: usize,
) -> ObservationResult<ObservationFrame> {
    validate_options(options)?;
    let volume = Level2File::parse(bytes).map_err(ObservationError::Nexrad)?;
    if let Some(expected) = options
        .site_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        let decoded = volume.station_id.trim();
        if !decoded.is_empty() && !decoded.eq_ignore_ascii_case(expected) {
            return Err(ObservationError::Nexrad(format!(
                "decoded station id '{decoded}' does not match requested source site '{expected}'"
            )));
        }
    }
    let site = resolve_site(&volume, options)?;
    let target_product = options.moment.radar_product();
    if matches!(options.mode, RadarGridMode::Composite)
        && target_product != RadarProduct::Reflectivity
    {
        return Err(ObservationError::Invalid(
            "composite mode is currently restricted to reflectivity".into(),
        ));
    }
    let samplers = build_samplers(&volume.sweeps, options.moment);
    if samplers.is_empty() {
        return Err(ObservationError::Nexrad(format!(
            "volume contains no {} moment",
            target_product.short_name()
        )));
    }
    let selected = select_samplers(&samplers, options.mode)?;
    let grid = radar_grid(
        &site,
        options.resolution_m,
        options.radius_km,
        maximum_cells,
    )?;
    let nx = grid.shape.nx;
    let radius_m = options.radius_km * 1_000.0;
    let values = (0..grid.shape.len())
        .into_par_iter()
        .map(|index| {
            let latitude = f64::from(grid.lat_deg[index]);
            let longitude = f64::from(grid.lon_deg[index]);
            let (distance_m, bearing_deg) =
                distance_and_bearing(site.lat, site.lon, latitude, longitude);
            if !distance_m.is_finite() || distance_m > radius_m {
                return f32::NAN;
            }
            match options.mode {
                RadarGridMode::Composite => selected
                    .iter()
                    .filter_map(|sampler| sampler.sample(bearing_deg, distance_m))
                    .filter(|value| value.is_finite())
                    .reduce(f32::max)
                    .unwrap_or(f32::NAN),
                RadarGridMode::Lowest | RadarGridMode::Sweep { .. } => selected[0]
                    .sample(bearing_deg, distance_m)
                    .unwrap_or(f32::NAN),
            }
        })
        .collect::<Vec<_>>();
    debug_assert_eq!(values.len(), nx * grid.shape.ny);

    let valid_unix = volume_valid_unix(&volume)?;
    let site_id = sanitize_token(&site.id);
    let product = match options.mode {
        RadarGridMode::Lowest => format!("{}-lowest", target_product.short_name()),
        RadarGridMode::Sweep { sweep_index } => {
            format!("{}-sweep-{sweep_index}", target_product.short_name())
        }
        RadarGridMode::Composite => "composite-reflectivity".to_string(),
    };
    let variable = options
        .variable
        .clone()
        .unwrap_or_else(|| options.moment.variable_slug().to_string());
    let selected_sweeps = selected
        .iter()
        .map(|sampler| {
            serde_json::json!({
                "sweep_index": sampler.sweep_index,
                "elevation_angle_deg": sampler.elevation_angle,
                "nyquist_velocity": sampler.nyquist_velocity,
                "radial_count": sampler.radials.len(),
            })
        })
        .collect::<Vec<_>>();
    let selector = serde_json::json!({
        "radar": {
            "provider": "nexrad-level2",
            "site_id": site.id,
            "site_name": site.name,
            "site_latitude": site.lat,
            "site_longitude": site.lon,
            "site_elevation_m": site.elevation,
            "moment": options.moment,
            "mode": options.mode,
            "resolution_m": options.resolution_m,
            "radius_km": options.radius_km,
            "volume_date": volume.volume_date,
            "volume_time_ms": volume.volume_time,
            "sweep_count": volume.sweeps.len(),
            "selected_sweeps": selected_sweeps,
            "velocity_reference": if options.moment == RadarMoment::Velocity {
                Some("radial_to_radar")
            } else {
                None
            },
            "velocity_dealiased": if options.moment == RadarMoment::Velocity {
                Some(false)
            } else {
                None
            },
            "sampling": {
                "range": "linear_validity_aware",
                "azimuth": "linear_validity_aware",
                "maximum_azimuth_gap_deg": MAX_INTERPOLATED_AZIMUTH_GAP_DEG,
                "velocity": "nyquist_fold_guarded",
                "differential_phase": "circular_degrees",
                "categorical": "nearest",
            },
            "source_identity": options.source_identity,
        }
    });
    let mut provenance_roles = vec!["radar".to_string(), "level2".to_string()];
    let mut provenance_products = vec![sanitize_token(target_product.short_name())];
    if let Some(source) = &options.source_identity {
        provenance_roles.push("archive-object".into());
        provenance_products.push(sanitize_token(&source.provider_id));
        provenance_products.push(format!("sha256-{}", source.sha256.to_ascii_lowercase()));
    }
    Ok(ObservationFrame {
        family: ObservationFamily::Radar,
        collection: options
            .collection
            .clone()
            .unwrap_or_else(|| site_id.clone()),
        product,
        valid_unix,
        grid,
        projection: Some(GridProjection::Geographic),
        planes: vec![GridPlane {
            name: variable,
            units: target_product.unit().to_string(),
            selector,
            values,
        }],
        provenance_provider: "noaa-nexrad-level2".to_string(),
        provenance_roles,
        provenance_products,
    })
}

pub fn ingest_nexrad_level2(
    store_root: &Path,
    bytes: &[u8],
    options: &NexradIngestOptions,
    maximum_cells: usize,
) -> ObservationResult<StoredFrameRef> {
    let frame = decode_nexrad_level2(bytes, options, maximum_cells)?;
    write_observation_frame_with_limit(store_root, &frame, maximum_cells)
}

pub fn ingest_nexrad_level2_default_limit(
    store_root: &Path,
    bytes: &[u8],
    options: &NexradIngestOptions,
) -> ObservationResult<StoredFrameRef> {
    ingest_nexrad_level2(store_root, bytes, options, DEFAULT_MAXIMUM_GRID_CELLS)
}

fn validate_options(options: &NexradIngestOptions) -> ObservationResult<()> {
    if !options.resolution_m.is_finite()
        || !options.radius_km.is_finite()
        || options.resolution_m <= 0.0
        || options.radius_km <= 0.0
    {
        return Err(ObservationError::Invalid(
            "NEXRAD resolution and radius must be finite and greater than zero".into(),
        ));
    }
    let explicit = (
        options.site_latitude,
        options.site_longitude,
        options.site_elevation_m,
    );
    if explicit.0.is_some() || explicit.1.is_some() || explicit.2.is_some() {
        let (Some(latitude), Some(longitude), Some(elevation)) = explicit else {
            return Err(ObservationError::Invalid(
                "explicit radar location requires latitude, longitude, and elevation".into(),
            ));
        };
        if !latitude.is_finite()
            || !longitude.is_finite()
            || !elevation.is_finite()
            || !(-90.0..=90.0).contains(&latitude)
            || !(-180.0..=180.0).contains(&longitude)
        {
            return Err(ObservationError::Invalid(
                "explicit radar location is invalid".into(),
            ));
        }
    }
    if let Some(source) = &options.source_identity {
        let bounded = [
            ("provider_id", source.provider_id.as_str(), 96usize),
            ("attribution", source.attribution.as_str(), 512usize),
            ("object_key", source.object_key.as_str(), 2_048usize),
        ];
        if bounded.iter().any(|(_, value, maximum)| {
            value.trim() != *value || value.is_empty() || value.len() > *maximum
        }) || source.object_bytes == 0
            || source.sha256.len() != 64
            || !source.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || source
                .last_modified
                .as_ref()
                .is_some_and(|value| value.trim() != value || value.is_empty() || value.len() > 128)
        {
            return Err(ObservationError::Invalid(
                "NEXRAD source identity is incomplete or malformed".into(),
            ));
        }
    }
    Ok(())
}

fn resolve_site(
    volume: &Level2File,
    options: &NexradIngestOptions,
) -> ObservationResult<RadarSite> {
    let requested_id = options
        .site_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(&volume.station_id);
    if let Some(site) = find_site(requested_id) {
        return Ok(site);
    }
    match (
        options.site_latitude,
        options.site_longitude,
        options.site_elevation_m,
    ) {
        (Some(latitude), Some(longitude), Some(elevation)) => Ok(RadarSite::new(
            requested_id,
            requested_id,
            latitude,
            longitude,
            elevation,
        )),
        _ => Err(ObservationError::Nexrad(format!(
            "radar site '{requested_id}' is not in the built-in table; supply an explicit location"
        ))),
    }
}

fn radar_grid(
    site: &RadarSite,
    resolution_m: f64,
    radius_km: f64,
    maximum_cells: usize,
) -> ObservationResult<LatLonGrid> {
    let diameter_m = radius_km * 2_000.0;
    let side_f64 = (diameter_m / resolution_m).ceil();
    if !side_f64.is_finite() || side_f64 > (usize::MAX - 1) as f64 {
        return Err(ObservationError::Invalid(
            "requested radar grid dimensions overflow this platform".into(),
        ));
    }
    let side = (side_f64 as usize).checked_add(1).ok_or_else(|| {
        ObservationError::Invalid("requested radar grid dimensions overflow this platform".into())
    })?;
    let shape = GridShape::new(side, side)?;
    let cells = shape.checked_len()?;
    if cells > maximum_cells {
        return Err(ObservationError::Invalid(format!(
            "requested radar grid has {cells} cells; maximum is {maximum_cells}"
        )));
    }
    let half = (side - 1) as f64 * 0.5;
    let mut latitudes = Vec::with_capacity(cells);
    let mut longitudes = Vec::with_capacity(cells);
    for y in 0..side {
        let north_m = (half - y as f64) * resolution_m;
        for x in 0..side {
            let east_m = (x as f64 - half) * resolution_m;
            let distance = east_m.hypot(north_m);
            let bearing = east_m.atan2(north_m).to_degrees().rem_euclid(360.0);
            let (latitude, longitude) = destination(site.lat, site.lon, bearing, distance);
            latitudes.push(latitude as f32);
            longitudes.push(longitude as f32);
        }
    }
    Ok(LatLonGrid::new(shape, latitudes, longitudes)?)
}

const MAX_INTERPOLATED_AZIMUTH_GAP_DEG: f32 = 2.5;

struct RadialMoment<'a> {
    azimuth: f32,
    moment: &'a MomentData,
}

struct SweepSampler<'a> {
    sweep_index: u16,
    elevation_angle: f32,
    nyquist_velocity: Option<f32>,
    moment: RadarMoment,
    radials: Vec<RadialMoment<'a>>,
}

impl SweepSampler<'_> {
    fn sample(&self, bearing_deg: f64, distance_m: f64) -> Option<f32> {
        if self.radials.is_empty() || !bearing_deg.is_finite() || !distance_m.is_finite() {
            return None;
        }
        let bearing = (bearing_deg as f32).rem_euclid(360.0);
        match self
            .radials
            .binary_search_by(|radial| radial.azimuth.total_cmp(&bearing))
        {
            Ok(index) => self.sample_radial(index, distance_m),
            Err(insertion) => {
                let upper_index = insertion % self.radials.len();
                let lower_index = if insertion == 0 {
                    self.radials.len() - 1
                } else {
                    insertion - 1
                };
                if lower_index == upper_index {
                    return self.sample_radial(lower_index, distance_m);
                }
                let lower_azimuth = self.radials[lower_index].azimuth;
                let upper_azimuth = self.radials[upper_index].azimuth;
                let span = clockwise_degrees(lower_azimuth, upper_azimuth);
                if span <= f32::EPSILON {
                    return self.sample_radial(lower_index, distance_m);
                }
                let offset = clockwise_degrees(lower_azimuth, bearing);
                let fraction = (offset / span).clamp(0.0, 1.0);
                let lower = self.sample_radial(lower_index, distance_m);
                let upper = self.sample_radial(upper_index, distance_m);
                if span > MAX_INTERPOLATED_AZIMUTH_GAP_DEG {
                    let lower_distance = angular_difference(lower_azimuth, bearing);
                    let upper_distance = angular_difference(upper_azimuth, bearing);
                    if lower_distance.min(upper_distance) > MAX_INTERPOLATED_AZIMUTH_GAP_DEG * 0.5 {
                        return None;
                    }
                    return if lower_distance <= upper_distance {
                        lower.or(upper)
                    } else {
                        upper.or(lower)
                    };
                }
                interpolate_radar_values(self.moment, lower, upper, fraction, self.nyquist_velocity)
            }
        }
    }

    fn sample_radial(&self, index: usize, distance_m: f64) -> Option<f32> {
        let radial = self.radials.get(index)?;
        let first = f64::from(radial.moment.first_gate_range);
        let gate_size = f64::from(radial.moment.gate_size);
        if gate_size <= 0.0 || radial.moment.data.is_empty() {
            return None;
        }
        let last_index = radial.moment.data.len() - 1;
        let position = (distance_m - first) / gate_size;
        if position < -0.5 || position > last_index as f64 + 0.5 {
            return None;
        }
        let position = position.clamp(0.0, last_index as f64);
        let lower_index = position.floor() as usize;
        let upper_index = position.ceil() as usize;
        if lower_index == upper_index {
            return finite_value(radial.moment.data[lower_index]);
        }
        interpolate_radar_values(
            self.moment,
            finite_value(radial.moment.data[lower_index]),
            finite_value(radial.moment.data[upper_index]),
            (position - lower_index as f64) as f32,
            self.nyquist_velocity,
        )
    }
}

fn build_samplers<'a>(
    sweeps: &'a [Level2Sweep],
    selected_moment: RadarMoment,
) -> Vec<SweepSampler<'a>> {
    let product = selected_moment.radar_product();
    let mut samplers = sweeps
        .iter()
        .filter_map(|sweep| {
            let mut radials = sweep
                .radials
                .iter()
                .filter_map(|radial| {
                    moment(radial, product).map(|moment| RadialMoment {
                        azimuth: radial.azimuth.rem_euclid(360.0),
                        moment,
                    })
                })
                .collect::<Vec<_>>();
            if radials.is_empty() {
                return None;
            }
            radials.sort_by(|left, right| left.azimuth.total_cmp(&right.azimuth));
            Some(SweepSampler {
                sweep_index: sweep.sweep_index,
                elevation_angle: sweep.elevation_angle,
                nyquist_velocity: sweep.nyquist_velocity,
                moment: selected_moment,
                radials,
            })
        })
        .collect::<Vec<_>>();
    samplers.sort_by(|left, right| {
        left.elevation_angle
            .total_cmp(&right.elevation_angle)
            .then(left.sweep_index.cmp(&right.sweep_index))
    });
    samplers
}

fn finite_value(value: f32) -> Option<f32> {
    value.is_finite().then_some(value)
}

fn interpolate_radar_values(
    moment: RadarMoment,
    lower: Option<f32>,
    upper: Option<f32>,
    fraction: f32,
    nyquist_velocity: Option<f32>,
) -> Option<f32> {
    let fraction = fraction.clamp(0.0, 1.0);
    let (lower, upper) = match (lower, upper) {
        (Some(lower), Some(upper)) => (lower, upper),
        (Some(value), None) | (None, Some(value)) => return Some(value),
        (None, None) => return None,
    };
    match moment {
        RadarMoment::HydrometeorClassification => Some(if fraction < 0.5 { lower } else { upper }),
        RadarMoment::DifferentialPhase => {
            let delta = (upper - lower + 540.0).rem_euclid(360.0) - 180.0;
            Some((lower + delta * fraction).rem_euclid(360.0))
        }
        RadarMoment::Velocity => {
            let threshold = nyquist_velocity
                .filter(|value| value.is_finite() && *value > 0.0)
                .map(|value| (value * 1.25).max(8.0))
                .unwrap_or(30.0);
            if (upper - lower).abs() > threshold {
                Some(if fraction < 0.5 { lower } else { upper })
            } else {
                Some(lower + (upper - lower) * fraction)
            }
        }
        _ => Some(lower + (upper - lower) * fraction),
    }
}

fn clockwise_degrees(from: f32, to: f32) -> f32 {
    (to - from).rem_euclid(360.0)
}

fn select_samplers<'s, 'd>(
    samplers: &'s [SweepSampler<'d>],
    mode: RadarGridMode,
) -> ObservationResult<Vec<&'s SweepSampler<'d>>> {
    match mode {
        RadarGridMode::Lowest => Ok(vec![&samplers[0]]),
        RadarGridMode::Composite => Ok(samplers.iter().collect()),
        RadarGridMode::Sweep { sweep_index } => samplers
            .iter()
            .find(|sampler| sampler.sweep_index == sweep_index)
            .map(|sampler| vec![sampler])
            .ok_or_else(|| {
                ObservationError::Nexrad(format!(
                    "requested sweep index {sweep_index} does not contain the selected moment"
                ))
            }),
    }
}

fn moment(radial: &RadialData, product: RadarProduct) -> Option<&MomentData> {
    radial
        .moments
        .iter()
        .find(|moment| moment.product == product)
}

fn angular_difference(left: f32, right: f32) -> f32 {
    let difference = (left - right).abs().rem_euclid(360.0);
    difference.min(360.0 - difference)
}

fn volume_valid_unix(volume: &Level2File) -> ObservationResult<i64> {
    if volume.volume_date == 0 {
        return Err(ObservationError::Nexrad("volume date is zero".to_string()));
    }
    let epoch = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch date is valid");
    let date = epoch + Duration::days(i64::from(volume.volume_date) - 1);
    let midnight = date.and_hms_opt(0, 0, 0).expect("midnight is valid");
    Ok(
        (midnight + Duration::milliseconds(i64::from(volume.volume_time)))
            .and_utc()
            .timestamp(),
    )
}

fn distance_and_bearing(
    latitude_1: f64,
    longitude_1: f64,
    latitude_2: f64,
    longitude_2: f64,
) -> (f64, f64) {
    let lat1 = latitude_1.to_radians();
    let lat2 = latitude_2.to_radians();
    let dlat = lat2 - lat1;
    let dlon = (longitude_2 - longitude_1).to_radians();
    let a = (dlat * 0.5).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon * 0.5).sin().powi(2);
    let distance = 2.0 * EARTH_RADIUS_M * a.sqrt().atan2((1.0 - a).sqrt());
    let y = dlon.sin() * lat2.cos();
    let x = lat1.cos() * lat2.sin() - lat1.sin() * lat2.cos() * dlon.cos();
    let bearing = y.atan2(x).to_degrees().rem_euclid(360.0);
    (distance, bearing)
}

fn destination(latitude: f64, longitude: f64, bearing_deg: f64, distance_m: f64) -> (f64, f64) {
    let angular = distance_m / EARTH_RADIUS_M;
    let bearing = bearing_deg.to_radians();
    let lat1 = latitude.to_radians();
    let lon1 = longitude.to_radians();
    let lat2 = (lat1.sin() * angular.cos() + lat1.cos() * angular.sin() * bearing.cos()).asin();
    let lon2 = lon1
        + (bearing.sin() * angular.sin() * lat1.cos())
            .atan2(angular.cos() - lat1.sin() * lat2.sin());
    (
        lat2.to_degrees(),
        ((lon2.to_degrees() + 540.0) % 360.0) - 180.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radar_grid_is_north_up_and_centered() {
        let site = RadarSite::new("TEST", "Test", 35.0, -97.0, 300.0);
        let grid = radar_grid(&site, 1_000.0, 2.0, 100).unwrap();
        assert_eq!(grid.shape.nx, 5);
        assert_eq!(grid.shape.ny, 5);
        let center = 2 * 5 + 2;
        assert!((grid.lat_deg[center] - 35.0).abs() < 1e-4);
        assert!((grid.lon_deg[center] + 97.0).abs() < 1e-4);
        assert!(grid.lat_deg[0] > grid.lat_deg[20]);
    }

    #[test]
    fn great_circle_round_trip_is_close() {
        let (lat, lon) = destination(35.0, -97.0, 90.0, 100_000.0);
        let (distance, bearing) = distance_and_bearing(35.0, -97.0, lat, lon);
        assert!((distance - 100_000.0).abs() < 1.0);
        assert!((bearing - 90.0).abs() < 0.01);
    }

    #[test]
    fn velocity_interpolation_does_not_average_across_a_fold() {
        let value = interpolate_radar_values(
            RadarMoment::Velocity,
            Some(-24.0),
            Some(24.0),
            0.25,
            Some(25.0),
        );
        assert_eq!(value, Some(-24.0));
        let value = interpolate_radar_values(
            RadarMoment::Velocity,
            Some(-24.0),
            Some(24.0),
            0.75,
            Some(25.0),
        );
        assert_eq!(value, Some(24.0));
    }

    #[test]
    fn differential_phase_interpolates_across_zero_on_the_short_arc() {
        let value = interpolate_radar_values(
            RadarMoment::DifferentialPhase,
            Some(350.0),
            Some(10.0),
            0.5,
            None,
        )
        .unwrap();
        assert!(value.is_finite(), "got {value}");
        assert!(!(1.0..=359.0).contains(&value), "got {value}");
    }

    #[test]
    fn validity_aware_interpolation_keeps_the_finite_neighbor() {
        assert_eq!(
            interpolate_radar_values(RadarMoment::Reflectivity, Some(42.0), None, 0.75, None,),
            Some(42.0)
        );
    }

    #[test]
    fn configured_resolution_and_radius_have_no_product_policy_ceiling() {
        let options = NexradIngestOptions {
            resolution_m: 25.0,
            radius_km: 600.0,
            ..NexradIngestOptions::default()
        };
        validate_options(&options).expect(
            "operator-selected geometry is bounded by address-space checks when allocated, not a hidden product ceiling",
        );
    }

    #[test]
    fn exact_source_identity_is_validated_before_decode() {
        let mut options = NexradIngestOptions {
            source_identity: Some(NexradSourceIdentity {
                provider_id: "fixture-provider".into(),
                attribution: "Fixture provider".into(),
                object_key: "2026/08/23/KTLX/KTLX20260823_080001_V06".into(),
                object_bytes: 12_345,
                sha256: "a".repeat(64),
                last_modified: Some("2026-08-23T08:05:00Z".into()),
            }),
            ..NexradIngestOptions::default()
        };
        validate_options(&options).unwrap();
        options.source_identity.as_mut().unwrap().sha256 = "not-a-digest".into();
        assert!(validate_options(&options).is_err());
    }
}

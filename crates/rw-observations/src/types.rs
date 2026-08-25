use rustwx_core::{GridProjection, GridShape, LatLonGrid};
use serde::{Deserialize, Serialize};

use crate::{ObservationError, ObservationResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationFamily {
    Satellite,
    Mrms,
    Radar,
    RadarMosaic,
    SimulatedRadar,
    SimulatedSatellite,
    Generated,
}

impl ObservationFamily {
    pub const fn model_slug(self) -> &'static str {
        match self {
            Self::Satellite => "obs-satellite",
            Self::Mrms => "obs-mrms",
            Self::Radar => "obs-radar",
            Self::RadarMosaic => "obs-radar-mosaic",
            Self::SimulatedRadar => "obs-sim-radar",
            Self::SimulatedSatellite => "obs-simsat",
            Self::Generated => "obs-generated",
        }
    }
}

/// Scientific meaning a client must preserve when it colors or interpolates
/// an observation plane.
///
/// The server transports calibrated scalar values.  This enum keeps a radar
/// velocity plane from silently falling through to a generic grayscale ramp,
/// and keeps categorical products from being linearly blended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationValueSemantics {
    Reflectivity,
    RadialVelocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    HydrometeorClassification,
    EchoTop,
    VerticallyIntegratedLiquid,
    BrightnessTemperature,
    Reflectance,
    Precipitation,
    Rgba,
    GenericScalar,
}

impl ObservationValueSemantics {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Reflectivity => "reflectivity",
            Self::RadialVelocity => "radial_velocity",
            Self::SpectrumWidth => "spectrum_width",
            Self::DifferentialReflectivity => "differential_reflectivity",
            Self::CorrelationCoefficient => "correlation_coefficient",
            Self::DifferentialPhase => "differential_phase",
            Self::SpecificDifferentialPhase => "specific_differential_phase",
            Self::HydrometeorClassification => "hydrometeor_classification",
            Self::EchoTop => "echo_top",
            Self::VerticallyIntegratedLiquid => "vertically_integrated_liquid",
            Self::BrightnessTemperature => "brightness_temperature",
            Self::Reflectance => "reflectance",
            Self::Precipitation => "precipitation",
            Self::Rgba => "rgba",
            Self::GenericScalar => "generic_scalar",
        }
    }

    pub const fn is_ordered_scalar(self) -> bool {
        !matches!(
            self,
            Self::HydrometeorClassification | Self::Rgba | Self::GenericScalar
        )
    }
}

/// Safe interpolation rule for the semantic value domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationInterpolation {
    Linear,
    Nearest,
    CircularDegrees,
    VelocityFoldAware,
}

impl ObservationInterpolation {
    pub const fn slug(self) -> &'static str {
        match self {
            Self::Linear => "linear",
            Self::Nearest => "nearest",
            Self::CircularDegrees => "circular_degrees",
            Self::VelocityFoldAware => "velocity_fold_aware",
        }
    }
}

/// Presentation metadata carried beside the calibrated values.
///
/// `palette` is a semantic family name, not a mandate to use one exact RGB
/// table.  A workstation may substitute its own velocity/ref/ZDR table while
/// preserving the scientific family and interpolation rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationDisplayHint {
    pub semantics: ObservationValueSemantics,
    pub palette: String,
    pub interpolation: ObservationInterpolation,
    pub transparent_non_finite: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preferred_range: Option<[f32; 2]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discontinuity_threshold: Option<f32>,
}

impl ObservationDisplayHint {
    fn new(
        semantics: ObservationValueSemantics,
        palette: &str,
        interpolation: ObservationInterpolation,
        preferred_range: Option<[f32; 2]>,
        discontinuity_threshold: Option<f32>,
    ) -> Self {
        Self {
            semantics,
            palette: palette.to_string(),
            interpolation,
            transparent_non_finite: true,
            preferred_range,
            discontinuity_threshold,
        }
    }
}

/// Infer a stable display contract from the canonical family, variable, and
/// units.  The values remain untouched; this only prevents consumers from
/// guessing that every plane is an anonymous grayscale scalar.
pub fn observation_display_hint(
    family: ObservationFamily,
    variable: &str,
    units: &str,
) -> ObservationDisplayHint {
    let name = normalized_token(variable);
    let units = units.trim().to_ascii_lowercase();
    let radar_like = matches!(
        family,
        ObservationFamily::Radar
            | ObservationFamily::RadarMosaic
            | ObservationFamily::Mrms
            | ObservationFamily::SimulatedRadar
    );

    if radar_like
        && (name.contains("correlation_coefficient")
            || name.contains("rhohv")
            || name.ends_with("_rho")
            || name.ends_with("_cc"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::CorrelationCoefficient,
            "correlation_coefficient",
            ObservationInterpolation::Linear,
            Some([0.0, 1.05]),
            None,
        );
    }
    if radar_like
        && (name.contains("differential_reflectivity")
            || name.contains("radar_zdr")
            || name.ends_with("_zdr"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::DifferentialReflectivity,
            "differential_reflectivity",
            ObservationInterpolation::Linear,
            Some([-8.0, 8.0]),
            None,
        );
    }
    if radar_like
        && (name.contains("specific_differential_phase")
            || name.contains("radar_kdp")
            || name.ends_with("_kdp"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::SpecificDifferentialPhase,
            "specific_differential_phase",
            ObservationInterpolation::Linear,
            Some([-5.0, 20.0]),
            None,
        );
    }
    if radar_like
        && (name.contains("differential_phase")
            || name.contains("radar_phidp")
            || name.ends_with("_phidp"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::DifferentialPhase,
            "differential_phase",
            ObservationInterpolation::CircularDegrees,
            Some([0.0, 360.0]),
            Some(180.0),
        );
    }
    if radar_like
        && (name.contains("hydrometeor_classification")
            || name.contains("radar_hca")
            || name.ends_with("_hca"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::HydrometeorClassification,
            "hydrometeor_classification",
            ObservationInterpolation::Nearest,
            Some([0.0, 20.0]),
            None,
        );
    }
    if radar_like && (name.contains("spectrum_width") || name.ends_with("_sw")) {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::SpectrumWidth,
            "spectrum_width",
            ObservationInterpolation::Linear,
            Some([0.0, 40.0]),
            None,
        );
    }
    if radar_like && name.contains("velocity") {
        let knots = units.contains("kt") || units.contains("knot");
        return ObservationDisplayHint::new(
            ObservationValueSemantics::RadialVelocity,
            "velocity",
            ObservationInterpolation::VelocityFoldAware,
            Some(if knots {
                [-160.0, 160.0]
            } else {
                [-80.0, 80.0]
            }),
            Some(if knots { 60.0 } else { 30.0 }),
        );
    }
    if radar_like && (name.contains("reflectivity") || units.contains("dbz")) {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::Reflectivity,
            "reflectivity",
            ObservationInterpolation::Linear,
            Some([-32.0, 95.0]),
            None,
        );
    }
    if radar_like && (name.contains("echo_top") || name.contains("echotop")) {
        let range = if units.contains("km") {
            [0.0, 20.0]
        } else {
            [0.0, 20_000.0]
        };
        return ObservationDisplayHint::new(
            ObservationValueSemantics::EchoTop,
            "echo_top",
            ObservationInterpolation::Linear,
            Some(range),
            None,
        );
    }
    if radar_like
        && (name == "vil"
            || name.ends_with("_vil")
            || name.contains("vertically_integrated_liquid"))
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::VerticallyIntegratedLiquid,
            "vil",
            ObservationInterpolation::Linear,
            Some([0.0, 80.0]),
            None,
        );
    }

    if matches!(
        family,
        ObservationFamily::Satellite | ObservationFamily::SimulatedSatellite
    ) {
        if name.contains("packed_rgba") || name.ends_with("_rgba") || units.contains("rgba") {
            return ObservationDisplayHint::new(
                ObservationValueSemantics::Rgba,
                "embedded_rgba",
                ObservationInterpolation::Nearest,
                None,
                None,
            );
        }
        if units == "k" || units.contains("kelvin") || name.contains("brightness_temperature") {
            return ObservationDisplayHint::new(
                ObservationValueSemantics::BrightnessTemperature,
                "satellite_infrared",
                ObservationInterpolation::Linear,
                Some([180.0, 330.0]),
                None,
            );
        }
        if units.contains("reflectance") || name.contains("reflectance") {
            let range = if units.contains('%') {
                [0.0, 120.0]
            } else {
                [0.0, 1.2]
            };
            return ObservationDisplayHint::new(
                ObservationValueSemantics::Reflectance,
                "satellite_visible",
                ObservationInterpolation::Linear,
                Some(range),
                None,
            );
        }
    }

    if name.contains("precip")
        || name.contains("rain_rate")
        || units.contains("mm/h")
        || units.contains("mm hr")
    {
        return ObservationDisplayHint::new(
            ObservationValueSemantics::Precipitation,
            "precipitation",
            ObservationInterpolation::Linear,
            Some(if units.contains("/h") || units.contains("hr") {
                [0.0, 100.0]
            } else {
                [0.0, 250.0]
            }),
            None,
        );
    }

    ObservationDisplayHint::new(
        ObservationValueSemantics::GenericScalar,
        "generic_scalar",
        ObservationInterpolation::Linear,
        None,
        None,
    )
}

/// Read a persisted display contract when present, or infer one for an older
/// observation selector.  This keeps mosaics and clients compatible with
/// frames written before the display metadata was introduced.
pub fn observation_display_hint_from_selector(
    variable: &str,
    units: &str,
    selector: &serde_json::Value,
) -> ObservationDisplayHint {
    if let Some(display) = selector.get("display")
        && let Ok(display) = serde_json::from_value(display.clone())
    {
        return display;
    }
    let family = selector
        .pointer("/observation/family")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .or_else(|| {
            if selector.get("radar").is_some() {
                Some(ObservationFamily::Radar)
            } else if selector.get("mrms").is_some() {
                Some(ObservationFamily::Mrms)
            } else if selector.get("satellite").is_some() {
                Some(ObservationFamily::Satellite)
            } else {
                None
            }
        })
        .unwrap_or(ObservationFamily::Generated);
    let source = selector.get("source_selector").unwrap_or(selector);
    if matches!(
        family,
        ObservationFamily::Satellite | ObservationFamily::SimulatedSatellite
    ) && let Some(band) = source
        .pointer("/satellite/band")
        .and_then(|value| value.as_u64())
        .and_then(|value| u8::try_from(value).ok())
        && let Some(display) = satellite_band_display_hint(band, units)
    {
        return display;
    }
    if let Some(semantics) = source
        .pointer("/radar_mosaic/source_semantics")
        .cloned()
        .and_then(|value| serde_json::from_value::<ObservationValueSemantics>(value).ok())
    {
        return observation_display_hint(family, semantics.slug(), units);
    }
    if let Some(source_variable) = source
        .pointer("/radar_mosaic/inputs/0/variable")
        .and_then(|value| value.as_str())
    {
        let inferred =
            observation_display_hint(ObservationFamily::RadarMosaic, source_variable, units);
        if inferred.semantics != ObservationValueSemantics::GenericScalar {
            return inferred;
        }
    }
    if let Some(moment) = source
        .pointer("/radar/moment")
        .and_then(|value| value.as_str())
    {
        let inferred = observation_display_hint(ObservationFamily::Radar, moment, units);
        if inferred.semantics != ObservationValueSemantics::GenericScalar {
            return inferred;
        }
    }
    observation_display_hint(family, variable, units)
}

fn satellite_band_display_hint(band: u8, units: &str) -> Option<ObservationDisplayHint> {
    let units = units.trim().to_ascii_lowercase();
    match band {
        1..=6 => Some(ObservationDisplayHint::new(
            ObservationValueSemantics::Reflectance,
            "satellite_visible",
            ObservationInterpolation::Linear,
            Some(if units.contains('%') {
                [0.0, 120.0]
            } else {
                [0.0, 1.2]
            }),
            None,
        )),
        7 => Some(ObservationDisplayHint::new(
            ObservationValueSemantics::BrightnessTemperature,
            "satellite_shortwave_infrared",
            ObservationInterpolation::Linear,
            Some([180.0, 340.0]),
            None,
        )),
        8..=10 => Some(ObservationDisplayHint::new(
            ObservationValueSemantics::BrightnessTemperature,
            "satellite_water_vapor",
            ObservationInterpolation::Linear,
            Some([180.0, 285.0]),
            None,
        )),
        11..=16 => Some(ObservationDisplayHint::new(
            ObservationValueSemantics::BrightnessTemperature,
            "satellite_infrared",
            ObservationInterpolation::Linear,
            Some([180.0, 330.0]),
            None,
        )),
        _ => None,
    }
}

fn normalized_token(value: &str) -> String {
    value
        .trim()
        .to_ascii_lowercase()
        .replace(['-', ' ', '.'], "_")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GridPlane {
    pub name: String,
    pub units: String,
    #[serde(default)]
    pub selector: serde_json::Value,
    pub values: Vec<f32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationFrame {
    pub family: ObservationFamily,
    pub collection: String,
    pub product: String,
    pub valid_unix: i64,
    pub grid: LatLonGrid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projection: Option<GridProjection>,
    pub planes: Vec<GridPlane>,
    #[serde(default)]
    pub provenance_provider: String,
    #[serde(default)]
    pub provenance_roles: Vec<String>,
    #[serde(default)]
    pub provenance_products: Vec<String>,
}

impl ObservationFrame {
    pub fn validate(&self, maximum_cells: usize) -> ObservationResult<()> {
        if self.collection.trim().is_empty() || self.product.trim().is_empty() {
            return Err(ObservationError::Invalid(
                "collection and product must be non-empty".into(),
            ));
        }
        let cells = self.grid.shape.checked_len()?;
        if cells > maximum_cells {
            return Err(ObservationError::Invalid(format!(
                "frame has {cells} grid cells; configured maximum is {maximum_cells}"
            )));
        }
        if self.planes.is_empty() {
            return Err(ObservationError::Invalid(
                "an observation frame requires at least one plane".into(),
            ));
        }
        let mut names = std::collections::BTreeSet::new();
        for plane in &self.planes {
            if plane.name.trim().is_empty() || plane.units.len() > 128 {
                return Err(ObservationError::Invalid(
                    "plane names must be non-empty and units must be bounded".into(),
                ));
            }
            if !names.insert(plane.name.as_str()) {
                return Err(ObservationError::Invalid(format!(
                    "duplicate plane '{}'",
                    plane.name
                )));
            }
            if plane.values.len() != cells {
                return Err(ObservationError::Invalid(format!(
                    "plane '{}' has {} values; expected {cells}",
                    plane.name,
                    plane.values.len()
                )));
            }
            if plane.values.iter().any(|value| value.is_infinite()) {
                return Err(ObservationError::Invalid(format!(
                    "plane '{}' contains an infinite value; missing data must use NaN",
                    plane.name
                )));
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_regular_grid(
        family: ObservationFamily,
        collection: impl Into<String>,
        product: impl Into<String>,
        valid_unix: i64,
        nx: usize,
        ny: usize,
        latitudes: Vec<f32>,
        longitudes: Vec<f32>,
        projection: Option<GridProjection>,
        planes: Vec<GridPlane>,
    ) -> ObservationResult<Self> {
        let shape = GridShape::new(nx, ny)?;
        let grid = LatLonGrid::new(shape, latitudes, longitudes)?;
        Ok(Self {
            family,
            collection: collection.into(),
            product: product.into(),
            valid_unix,
            grid,
            projection,
            planes,
            provenance_provider: String::new(),
            provenance_roles: Vec::new(),
            provenance_products: Vec::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredFrameRef {
    pub schema: String,
    pub model: String,
    pub run: String,
    pub storage_slot: u16,
    pub valid_unix: i64,
    pub variables: Vec<String>,
    pub grid_hash: String,
    pub frame_file: String,
    pub bytes: u64,
    pub duplicate: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredPlaneRef {
    pub model: String,
    pub run: String,
    pub storage_slot: u16,
    pub variable: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeographicGridSpec {
    pub west_longitude: f64,
    pub south_latitude: f64,
    pub east_longitude: f64,
    pub north_latitude: f64,
    pub resolution_km: f64,
}

impl GeographicGridSpec {
    pub fn build(self, maximum_cells: usize) -> ObservationResult<LatLonGrid> {
        if !self.west_longitude.is_finite()
            || !self.east_longitude.is_finite()
            || !self.south_latitude.is_finite()
            || !self.north_latitude.is_finite()
            || !self.resolution_km.is_finite()
            || self.west_longitude >= self.east_longitude
            || self.south_latitude >= self.north_latitude
            || !(-180.0..=180.0).contains(&self.west_longitude)
            || !(-180.0..=180.0).contains(&self.east_longitude)
            || !(-90.0..=90.0).contains(&self.south_latitude)
            || !(-90.0..=90.0).contains(&self.north_latitude)
            || !(0.05..=100.0).contains(&self.resolution_km)
        {
            return Err(ObservationError::Invalid(
                "invalid geographic target grid".into(),
            ));
        }
        let middle_latitude = (self.south_latitude + self.north_latitude) * 0.5;
        let dy = self.resolution_km / 111.32;
        let dx = self.resolution_km / (111.32 * middle_latitude.to_radians().cos().abs().max(0.05));
        let nx =
            (((self.east_longitude - self.west_longitude) / dx).ceil() as usize).saturating_add(1);
        let ny =
            (((self.north_latitude - self.south_latitude) / dy).ceil() as usize).saturating_add(1);
        let shape = GridShape::new(nx, ny)?;
        let cells = shape.checked_len()?;
        if cells > maximum_cells {
            return Err(ObservationError::Invalid(format!(
                "target grid has {cells} cells; configured maximum is {maximum_cells}"
            )));
        }
        let actual_dx = (self.east_longitude - self.west_longitude) / (nx - 1).max(1) as f64;
        let actual_dy = (self.north_latitude - self.south_latitude) / (ny - 1).max(1) as f64;
        let mut latitudes = Vec::with_capacity(cells);
        let mut longitudes = Vec::with_capacity(cells);
        for y in 0..ny {
            let latitude = self.north_latitude - y as f64 * actual_dy;
            for x in 0..nx {
                latitudes.push(latitude as f32);
                longitudes.push((self.west_longitude + x as f64 * actual_dx) as f32);
            }
        }
        Ok(LatLonGrid::new(shape, latitudes, longitudes)?)
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    #[test]
    fn radar_velocity_never_falls_back_to_generic_grayscale() {
        let hint = observation_display_hint(ObservationFamily::Radar, "radar_velocity", "m/s");
        assert_eq!(hint.semantics, ObservationValueSemantics::RadialVelocity);
        assert_eq!(hint.palette, "velocity");
        assert_eq!(
            hint.interpolation,
            ObservationInterpolation::VelocityFoldAware
        );
        assert_eq!(hint.discontinuity_threshold, Some(30.0));
    }

    #[test]
    fn categorical_and_satellite_planes_keep_their_semantics() {
        let hca = observation_display_hint(ObservationFamily::RadarMosaic, "radar_hca", "category");
        assert_eq!(hca.interpolation, ObservationInterpolation::Nearest);

        let ir =
            observation_display_hint(ObservationFamily::Satellite, "brightness_temperature", "K");
        assert_eq!(
            ir.semantics,
            ObservationValueSemantics::BrightnessTemperature
        );
        assert_eq!(ir.palette, "satellite_infrared");
    }

    #[test]
    fn raw_satellite_bands_get_visible_ir_and_water_vapor_families() {
        let visible = serde_json::json!({
            "observation": { "family": "satellite" },
            "source_selector": { "satellite": { "band": 2 } }
        });
        let hint = observation_display_hint_from_selector("cmi_c02", "1", &visible);
        assert_eq!(hint.semantics, ObservationValueSemantics::Reflectance);
        assert_eq!(hint.palette, "satellite_visible");

        let water_vapor = serde_json::json!({
            "observation": { "family": "satellite" },
            "source_selector": { "satellite": { "band": 9 } }
        });
        let hint = observation_display_hint_from_selector("cmi_c09", "K", &water_vapor);
        assert_eq!(
            hint.semantics,
            ObservationValueSemantics::BrightnessTemperature
        );
        assert_eq!(hint.palette, "satellite_water_vapor");
    }

    #[test]
    fn selector_fallback_reads_existing_family_metadata() {
        let selector = serde_json::json!({
            "observation": { "family": "radar" }
        });
        let hint = observation_display_hint_from_selector("radar_reflectivity", "dBZ", &selector);
        assert_eq!(hint.semantics, ObservationValueSemantics::Reflectivity);
    }

    #[test]
    fn old_generic_mosaic_names_inherit_the_source_variable_semantics() {
        let selector = serde_json::json!({
            "observation": { "family": "radar_mosaic" },
            "source_selector": {
                "radar_mosaic": {
                    "inputs": [{ "variable": "radar_velocity" }]
                }
            }
        });
        let hint = observation_display_hint_from_selector("radar_mosaic", "m/s", &selector);
        assert_eq!(hint.semantics, ObservationValueSemantics::RadialVelocity);
        assert_eq!(hint.palette, "velocity");
    }
}

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
        }
        Ok(())
    }

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

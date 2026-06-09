use std::fmt;
use std::hash::{Hash, Hasher};

use rustwx_core::GridShape;
use serde::{Deserialize, Serialize};

use crate::error::RegridError;

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
const REGULAR_TOLERANCE_DEG: f64 = 1.0e-5;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct LatLon {
    pub lat_deg: f64,
    pub lon_deg: f64,
}

impl LatLon {
    pub const fn new(lat_deg: f64, lon_deg: f64) -> Self {
        Self { lat_deg, lon_deg }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GridFingerprint(String);

impl GridFingerprint {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn from_hash(prefix: &str, hash: u64) -> Self {
        Self(format!("{prefix}:{hash:016x}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for GridFingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegularLatLonSpec {
    pub shape: GridShape,
    pub lat0_deg: f64,
    pub lon0_deg: f64,
    pub dlat_deg: f64,
    pub dlon_deg: f64,
    pub global_lon_wrap: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum GridProjection {
    LatLon,
    LambertConformal {
        standard_parallel_1_deg: f64,
        standard_parallel_2_deg: Option<f64>,
        latitude_of_origin_deg: f64,
        longitude_of_origin_deg: f64,
        earth_radius_m: Option<f64>,
    },
    PolarStereographic {
        latitude_of_projection_origin_deg: f64,
        straight_vertical_longitude_from_pole_deg: f64,
        standard_parallel_deg: Option<f64>,
        earth_radius_m: Option<f64>,
    },
    Mercator {
        longitude_of_projection_origin_deg: f64,
        standard_parallel_deg: Option<f64>,
        earth_radius_m: Option<f64>,
    },
    RotatedLatLon {
        grid_north_pole_latitude_deg: f64,
        grid_north_pole_longitude_deg: f64,
    },
    Geostationary {
        longitude_of_projection_origin_deg: f64,
        perspective_point_height_m: f64,
        sweep_angle_axis: Option<SweepAxis>,
        semi_major_axis_m: Option<f64>,
        semi_minor_axis_m: Option<f64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SweepAxis {
    X,
    Y,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum VectorOrientation {
    EarthRelative,
    GridRelative { angle_to_east_rad: Vec<f64> },
}

pub trait GridGeometry {
    fn shape(&self) -> GridShape;

    fn len(&self) -> usize {
        self.shape().len()
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn fingerprint(&self) -> GridFingerprint;

    fn center_lat_lon(&self, index: usize) -> Option<LatLon>;

    fn cell_corners_lat_lon(&self, _index: usize) -> Option<[LatLon; 4]> {
        None
    }

    fn projection(&self) -> Option<&GridProjection> {
        None
    }

    fn vector_orientation(&self) -> Option<&VectorOrientation> {
        None
    }

    fn regular_lat_lon(&self) -> Option<RegularLatLonSpec> {
        None
    }

    fn geometry_name(&self) -> &'static str {
        "grid geometry"
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegularLatLonGrid {
    pub shape: GridShape,
    pub lat0_deg: f64,
    pub lon0_deg: f64,
    pub dlat_deg: f64,
    pub dlon_deg: f64,
    pub global_lon_wrap: bool,
}

impl RegularLatLonGrid {
    pub fn new(
        shape: GridShape,
        lat0_deg: f64,
        lon0_deg: f64,
        dlat_deg: f64,
        dlon_deg: f64,
        global_lon_wrap: bool,
    ) -> Result<Self, RegridError> {
        let grid = Self {
            shape,
            lat0_deg,
            lon0_deg,
            dlat_deg,
            dlon_deg,
            global_lon_wrap,
        };
        grid.validate()?;
        Ok(grid)
    }

    pub fn validate(&self) -> Result<(), RegridError> {
        if self.shape.nx == 0 || self.shape.ny == 0 {
            return Err(RegridError::InvalidGrid(format!(
                "regular lat/lon grid has invalid shape {}x{}",
                self.shape.nx, self.shape.ny
            )));
        }
        validate_lat(self.lat0_deg)?;
        validate_finite_lon(self.lon0_deg)?;
        if !self.dlat_deg.is_finite() || self.dlat_deg == 0.0 {
            return Err(RegridError::InvalidGrid(format!(
                "regular lat/lon grid has invalid dlat={}",
                self.dlat_deg
            )));
        }
        if !self.dlon_deg.is_finite() || self.dlon_deg == 0.0 {
            return Err(RegridError::InvalidGrid(format!(
                "regular lat/lon grid has invalid dlon={}",
                self.dlon_deg
            )));
        }
        for y in 0..self.shape.ny {
            validate_lat(self.lat_at_y(y))?;
        }
        Ok(())
    }

    pub fn x_y(&self, index: usize) -> Option<(usize, usize)> {
        if index >= self.shape.len() {
            return None;
        }
        Some((index % self.shape.nx, index / self.shape.nx))
    }

    pub fn index(&self, x: usize, y: usize) -> usize {
        y * self.shape.nx + x
    }

    pub fn lat_at_y(&self, y: usize) -> f64 {
        self.lat0_deg + y as f64 * self.dlat_deg
    }

    pub fn lon_at_x(&self, x: usize) -> f64 {
        self.lon0_deg + x as f64 * self.dlon_deg
    }

    pub fn spec(&self) -> RegularLatLonSpec {
        RegularLatLonSpec {
            shape: self.shape,
            lat0_deg: self.lat0_deg,
            lon0_deg: self.lon0_deg,
            dlat_deg: self.dlat_deg,
            dlon_deg: self.dlon_deg,
            global_lon_wrap: self.global_lon_wrap,
        }
    }
}

impl GridGeometry for RegularLatLonGrid {
    fn shape(&self) -> GridShape {
        self.shape
    }

    fn fingerprint(&self) -> GridFingerprint {
        let mut hash = Fnv64::new();
        hash.write_usize(self.shape.nx);
        hash.write_usize(self.shape.ny);
        hash.write_u64(self.lat0_deg.to_bits());
        hash.write_u64(self.lon0_deg.to_bits());
        hash.write_u64(self.dlat_deg.to_bits());
        hash.write_u64(self.dlon_deg.to_bits());
        hash.write_u8(u8::from(self.global_lon_wrap));
        GridFingerprint::from_hash("regular_lat_lon", hash.finish())
    }

    fn center_lat_lon(&self, index: usize) -> Option<LatLon> {
        let (x, y) = self.x_y(index)?;
        Some(LatLon::new(self.lat_at_y(y), self.lon_at_x(x)))
    }

    fn cell_corners_lat_lon(&self, index: usize) -> Option<[LatLon; 4]> {
        let (x, y) = self.x_y(index)?;
        let (south, north) = regular_lat_bounds(self.lat_at_y(y), self.dlat_deg);
        let (west, east) = regular_lon_bounds(self.lon_at_x(x), self.dlon_deg);
        Some([
            LatLon::new(south, west),
            LatLon::new(south, east),
            LatLon::new(north, east),
            LatLon::new(north, west),
        ])
    }

    fn regular_lat_lon(&self) -> Option<RegularLatLonSpec> {
        Some(self.spec())
    }

    fn geometry_name(&self) -> &'static str {
        "regular lat/lon"
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvilinearLatLonGrid {
    pub shape: GridShape,
    pub center_lats_deg: Vec<f64>,
    pub center_lons_deg: Vec<f64>,
    pub corners: Option<Vec<[LatLon; 4]>>,
}

impl CurvilinearLatLonGrid {
    pub fn new(
        shape: GridShape,
        center_lats_deg: Vec<f64>,
        center_lons_deg: Vec<f64>,
        corners: Option<Vec<[LatLon; 4]>>,
    ) -> Result<Self, RegridError> {
        let grid = Self {
            shape,
            center_lats_deg,
            center_lons_deg,
            corners,
        };
        grid.validate()?;
        Ok(grid)
    }

    pub fn validate(&self) -> Result<(), RegridError> {
        let expected = self.shape.len();
        if self.center_lats_deg.len() != expected || self.center_lons_deg.len() != expected {
            return Err(RegridError::InvalidGrid(format!(
                "curvilinear center length mismatch: expected {expected}, got lat={} lon={}",
                self.center_lats_deg.len(),
                self.center_lons_deg.len()
            )));
        }
        if let Some(corners) = &self.corners {
            if corners.len() != expected {
                return Err(RegridError::InvalidGrid(format!(
                    "curvilinear corner length mismatch: expected {expected}, got {}",
                    corners.len()
                )));
            }
        }
        for (idx, (&lat, &lon)) in self
            .center_lats_deg
            .iter()
            .zip(&self.center_lons_deg)
            .enumerate()
        {
            validate_lat(lat).map_err(|err| {
                RegridError::InvalidGrid(format!("invalid center latitude at {idx}: {err}"))
            })?;
            validate_finite_lon(lon).map_err(|err| {
                RegridError::InvalidGrid(format!("invalid center longitude at {idx}: {err}"))
            })?;
        }
        Ok(())
    }
}

impl GridGeometry for CurvilinearLatLonGrid {
    fn shape(&self) -> GridShape {
        self.shape
    }

    fn fingerprint(&self) -> GridFingerprint {
        let mut hash = Fnv64::new();
        hash.write_usize(self.shape.nx);
        hash.write_usize(self.shape.ny);
        for value in &self.center_lats_deg {
            hash.write_u64(value.to_bits());
        }
        for value in &self.center_lons_deg {
            hash.write_u64(value.to_bits());
        }
        GridFingerprint::from_hash("curvilinear_lat_lon", hash.finish())
    }

    fn center_lat_lon(&self, index: usize) -> Option<LatLon> {
        Some(LatLon::new(
            *self.center_lats_deg.get(index)?,
            *self.center_lons_deg.get(index)?,
        ))
    }

    fn cell_corners_lat_lon(&self, index: usize) -> Option<[LatLon; 4]> {
        self.corners.as_ref()?.get(index).copied()
    }

    fn geometry_name(&self) -> &'static str {
        "curvilinear lat/lon"
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProjectedStructuredGrid {
    pub shape: GridShape,
    pub projection: GridProjection,
    pub x0: f64,
    pub y0: f64,
    pub dx: f64,
    pub dy: f64,
}

impl ProjectedStructuredGrid {
    pub fn new(
        shape: GridShape,
        projection: GridProjection,
        x0: f64,
        y0: f64,
        dx: f64,
        dy: f64,
    ) -> Result<Self, RegridError> {
        let grid = Self {
            shape,
            projection,
            x0,
            y0,
            dx,
            dy,
        };
        grid.validate()?;
        Ok(grid)
    }

    pub fn validate(&self) -> Result<(), RegridError> {
        if self.shape.nx == 0 || self.shape.ny == 0 {
            return Err(RegridError::InvalidGrid(format!(
                "projected grid has invalid shape {}x{}",
                self.shape.nx, self.shape.ny
            )));
        }
        for (name, value) in [
            ("x0", self.x0),
            ("y0", self.y0),
            ("dx", self.dx),
            ("dy", self.dy),
        ] {
            if !value.is_finite() {
                return Err(RegridError::InvalidGrid(format!(
                    "projected grid has non-finite {name}={value}"
                )));
            }
        }
        if self.dx == 0.0 || self.dy == 0.0 {
            return Err(RegridError::InvalidGrid(
                "projected grid spacing must be non-zero".to_string(),
            ));
        }
        Ok(())
    }
}

impl GridGeometry for ProjectedStructuredGrid {
    fn shape(&self) -> GridShape {
        self.shape
    }

    fn fingerprint(&self) -> GridFingerprint {
        let mut hash = Fnv64::new();
        hash.write_usize(self.shape.nx);
        hash.write_usize(self.shape.ny);
        hash.write_u64(self.x0.to_bits());
        hash.write_u64(self.y0.to_bits());
        hash.write_u64(self.dx.to_bits());
        hash.write_u64(self.dy.to_bits());
        hash.write(format!("{:?}", self.projection).as_bytes());
        GridFingerprint::from_hash("projected_structured", hash.finish())
    }

    fn center_lat_lon(&self, index: usize) -> Option<LatLon> {
        if !matches!(self.projection, GridProjection::LatLon) {
            return None;
        }
        if index >= self.shape.len() {
            return None;
        }
        let x = index % self.shape.nx;
        let y = index / self.shape.nx;
        Some(LatLon::new(
            self.y0 + y as f64 * self.dy,
            self.x0 + x as f64 * self.dx,
        ))
    }

    fn projection(&self) -> Option<&GridProjection> {
        Some(&self.projection)
    }

    fn geometry_name(&self) -> &'static str {
        "projected structured"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrientedGrid<G> {
    pub grid: G,
    pub vector_orientation: VectorOrientation,
}

impl<G> OrientedGrid<G> {
    pub fn new(grid: G, vector_orientation: VectorOrientation) -> Self {
        Self {
            grid,
            vector_orientation,
        }
    }
}

impl<G: GridGeometry> GridGeometry for OrientedGrid<G> {
    fn shape(&self) -> GridShape {
        self.grid.shape()
    }

    fn fingerprint(&self) -> GridFingerprint {
        let mut hash = Fnv64::new();
        hash.write(self.grid.fingerprint().as_str().as_bytes());
        match &self.vector_orientation {
            VectorOrientation::EarthRelative => hash.write_u8(0),
            VectorOrientation::GridRelative { angle_to_east_rad } => {
                hash.write_u8(1);
                for value in angle_to_east_rad {
                    hash.write_u64(value.to_bits());
                }
            }
        }
        GridFingerprint::from_hash("oriented", hash.finish())
    }

    fn center_lat_lon(&self, index: usize) -> Option<LatLon> {
        self.grid.center_lat_lon(index)
    }

    fn cell_corners_lat_lon(&self, index: usize) -> Option<[LatLon; 4]> {
        self.grid.cell_corners_lat_lon(index)
    }

    fn projection(&self) -> Option<&GridProjection> {
        self.grid.projection()
    }

    fn vector_orientation(&self) -> Option<&VectorOrientation> {
        Some(&self.vector_orientation)
    }

    fn regular_lat_lon(&self) -> Option<RegularLatLonSpec> {
        self.grid.regular_lat_lon()
    }

    fn geometry_name(&self) -> &'static str {
        self.grid.geometry_name()
    }
}

impl GridGeometry for rustwx_core::LatLonGrid {
    fn shape(&self) -> GridShape {
        self.shape
    }

    fn fingerprint(&self) -> GridFingerprint {
        let mut hash = Fnv64::new();
        hash.write_usize(self.shape.nx);
        hash.write_usize(self.shape.ny);
        for value in &self.lat_deg {
            hash.write_u32(value.to_bits());
        }
        for value in &self.lon_deg {
            hash.write_u32(value.to_bits());
        }
        GridFingerprint::from_hash("rustwx_core_lat_lon", hash.finish())
    }

    fn center_lat_lon(&self, index: usize) -> Option<LatLon> {
        Some(LatLon::new(
            *self.lat_deg.get(index)? as f64,
            *self.lon_deg.get(index)? as f64,
        ))
    }

    fn regular_lat_lon(&self) -> Option<RegularLatLonSpec> {
        detect_regular_core_grid(self)
    }

    fn geometry_name(&self) -> &'static str {
        "rustwx-core LatLonGrid"
    }
}

pub(crate) fn validate_lat(lat_deg: f64) -> Result<(), RegridError> {
    if !lat_deg.is_finite() || !(-90.0..=90.0).contains(&lat_deg) {
        return Err(RegridError::InvalidGrid(format!(
            "latitude must be finite and in [-90, 90], got {lat_deg}"
        )));
    }
    Ok(())
}

pub(crate) fn validate_finite_lon(lon_deg: f64) -> Result<(), RegridError> {
    if !lon_deg.is_finite() {
        return Err(RegridError::InvalidGrid(format!(
            "longitude must be finite, got {lon_deg}"
        )));
    }
    Ok(())
}

pub(crate) fn normalize_lon_delta(mut delta_deg: f64) -> f64 {
    delta_deg = (delta_deg + 180.0).rem_euclid(360.0) - 180.0;
    if delta_deg == -180.0 {
        180.0
    } else {
        delta_deg
    }
}

pub(crate) fn normalize_lon_positive(delta_deg: f64) -> f64 {
    delta_deg.rem_euclid(360.0)
}

pub(crate) fn regular_lat_bounds(center_lat_deg: f64, dlat_deg: f64) -> (f64, f64) {
    let half = dlat_deg.abs() * 0.5;
    (
        (center_lat_deg - half).clamp(-90.0, 90.0),
        (center_lat_deg + half).clamp(-90.0, 90.0),
    )
}

pub(crate) fn regular_lon_bounds(center_lon_deg: f64, dlon_deg: f64) -> (f64, f64) {
    let half = dlon_deg.abs() * 0.5;
    (center_lon_deg - half, center_lon_deg + half)
}

pub fn core_projection_to_regrid(
    projection: &rustwx_core::GridProjection,
) -> Option<GridProjection> {
    Some(match projection {
        rustwx_core::GridProjection::Geographic => GridProjection::LatLon,
        rustwx_core::GridProjection::LambertConformal {
            standard_parallel_1_deg,
            standard_parallel_2_deg,
            central_meridian_deg,
        } => GridProjection::LambertConformal {
            standard_parallel_1_deg: *standard_parallel_1_deg,
            standard_parallel_2_deg: Some(*standard_parallel_2_deg),
            latitude_of_origin_deg: *standard_parallel_1_deg,
            longitude_of_origin_deg: *central_meridian_deg,
            earth_radius_m: None,
        },
        rustwx_core::GridProjection::PolarStereographic {
            true_latitude_deg,
            central_meridian_deg,
            south_pole_on_projection_plane,
        } => GridProjection::PolarStereographic {
            latitude_of_projection_origin_deg: if *south_pole_on_projection_plane {
                -90.0
            } else {
                90.0
            },
            straight_vertical_longitude_from_pole_deg: *central_meridian_deg,
            standard_parallel_deg: Some(*true_latitude_deg),
            earth_radius_m: None,
        },
        rustwx_core::GridProjection::Mercator {
            latitude_of_true_scale_deg,
            central_meridian_deg,
        } => GridProjection::Mercator {
            longitude_of_projection_origin_deg: *central_meridian_deg,
            standard_parallel_deg: Some(*latitude_of_true_scale_deg),
            earth_radius_m: None,
        },
        rustwx_core::GridProjection::Other { template: _ } => return None,
    })
}

fn detect_regular_core_grid(grid: &rustwx_core::LatLonGrid) -> Option<RegularLatLonSpec> {
    let nx = grid.shape.nx;
    let ny = grid.shape.ny;
    if nx < 2
        || ny < 2
        || grid.lat_deg.len() != grid.shape.len()
        || grid.lon_deg.len() != grid.shape.len()
    {
        return None;
    }
    let lat0 = grid.lat_deg[0] as f64;
    let lon0 = grid.lon_deg[0] as f64;
    let dlon = grid.lon_deg[1] as f64 - lon0;
    let dlat = grid.lat_deg[nx] as f64 - lat0;
    if dlat.abs() <= REGULAR_TOLERANCE_DEG || dlon.abs() <= REGULAR_TOLERANCE_DEG {
        return None;
    }
    for y in 0..ny {
        let expected_lat = lat0 + y as f64 * dlat;
        for x in 0..nx {
            let idx = y * nx + x;
            if ((grid.lat_deg[idx] as f64) - expected_lat).abs() > REGULAR_TOLERANCE_DEG {
                return None;
            }
            let expected_lon = lon0 + x as f64 * dlon;
            if normalize_lon_delta((grid.lon_deg[idx] as f64) - expected_lon).abs()
                > REGULAR_TOLERANCE_DEG
            {
                return None;
            }
        }
    }
    let span = dlon.abs() * nx as f64;
    Some(RegularLatLonSpec {
        shape: grid.shape,
        lat0_deg: lat0,
        lon0_deg: lon0,
        dlat_deg: dlat,
        dlon_deg: dlon,
        global_lon_wrap: (span - 360.0).abs() <= dlon.abs().max(1.0) * 0.01,
    })
}

struct Fnv64(u64);

impl Fnv64 {
    fn new() -> Self {
        Self(FNV_OFFSET)
    }
}

impl Hasher for Fnv64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(FNV_PRIME);
        }
    }
}

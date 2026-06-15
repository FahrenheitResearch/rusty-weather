//! Gridded flash-density derivation over the `.rwl` point-event store.
//!
//! This keeps observed lightning density tied to real satellite flash events:
//! callers ingest GOES GLM today (and MTG LI later) into the same `.rwl`
//! buckets, then derive a regular lat/lon grid for display, export, or
//! downstream products.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{RwlError, RwlResult};
use crate::reader::{BBox, read_flashes};

/// Output units for [`FlashDensityGrid::values`].
pub const FLASH_DENSITY_UNITS: &str = "flashes per 1000 km^2 per hour";

const EARTH_RADIUS_KM: f64 = 6_371.0088;

/// Lat/lon domain for a density grid.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DensityBounds {
    pub south: f32,
    pub north: f32,
    pub west: f32,
    pub east: f32,
}

impl DensityBounds {
    pub fn new(south: f32, north: f32, west: f32, east: f32) -> Self {
        Self {
            south,
            north,
            west,
            east,
        }
    }

    fn bbox(self) -> BBox {
        BBox::new(self.south, self.north, self.west, self.east)
    }

    fn validate(self) -> RwlResult<()> {
        for (name, value) in [
            ("south", self.south),
            ("north", self.north),
            ("west", self.west),
            ("east", self.east),
        ] {
            if !value.is_finite() {
                return Err(RwlError::Format(format!(
                    "density bound `{name}` is not finite"
                )));
            }
        }
        if self.south < -90.0 || self.north > 90.0 || self.south >= self.north {
            return Err(RwlError::Format(format!(
                "density latitude bounds must satisfy -90 <= south < north <= 90, got {}..{}",
                self.south, self.north
            )));
        }
        if self.west < -180.0 || self.east > 180.0 || self.west >= self.east {
            return Err(RwlError::Format(format!(
                "density longitude bounds must satisfy -180 <= west < east <= 180, got {}..{}",
                self.west, self.east
            )));
        }
        Ok(())
    }
}

/// Request for deriving a gridded flash-density product from `.rwl` buckets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlashDensityRequest {
    /// Half-open time range start, Unix milliseconds.
    pub t0_unix_ms: i64,
    /// Half-open time range end, Unix milliseconds.
    pub t1_unix_ms: i64,
    pub bounds: DensityBounds,
    pub nx: usize,
    pub ny: usize,
    /// Include flashes marked with the degraded-quality flag.
    pub include_degraded: bool,
}

impl FlashDensityRequest {
    pub fn new(
        t0_unix_ms: i64,
        t1_unix_ms: i64,
        bounds: DensityBounds,
        nx: usize,
        ny: usize,
    ) -> Self {
        Self {
            t0_unix_ms,
            t1_unix_ms,
            bounds,
            nx,
            ny,
            include_degraded: false,
        }
    }

    fn validate(&self) -> RwlResult<()> {
        if self.t1_unix_ms <= self.t0_unix_ms {
            return Err(RwlError::Format(format!(
                "density request requires t1 > t0, got {}..{}",
                self.t0_unix_ms, self.t1_unix_ms
            )));
        }
        if self.nx == 0 || self.ny == 0 {
            return Err(RwlError::Format(format!(
                "density grid dimensions must be positive, got {}x{}",
                self.nx, self.ny
            )));
        }
        self.bounds.validate()
    }
}

/// One max-density cell, useful for summaries and smoke tests.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DensityCell {
    pub x: usize,
    pub y: usize,
    pub count: u32,
    pub value: f32,
    pub lat_center: f32,
    pub lon_center: f32,
}

/// A north-first regular lat/lon grid of flash density.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FlashDensityGrid {
    pub satellite: String,
    pub t0_unix_ms: i64,
    pub t1_unix_ms: i64,
    pub bounds: DensityBounds,
    pub nx: usize,
    pub ny: usize,
    pub units: String,
    /// Density values in row-major order, row 0 = northern edge.
    pub values: Vec<f32>,
    /// Raw flash counts per cell, same layout as `values`.
    pub counts: Vec<u32>,
    /// Number of flashes counted after bbox and quality filtering.
    pub flash_count: usize,
}

impl FlashDensityGrid {
    pub fn value(&self, x: usize, y: usize) -> Option<f32> {
        (x < self.nx && y < self.ny).then(|| self.values[y * self.nx + x])
    }

    pub fn count(&self, x: usize, y: usize) -> Option<u32> {
        (x < self.nx && y < self.ny).then(|| self.counts[y * self.nx + x])
    }

    pub fn max_cell(&self) -> Option<DensityCell> {
        self.values
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| a.total_cmp(b))
            .map(|(idx, &value)| {
                let x = idx % self.nx;
                let y = idx / self.nx;
                DensityCell {
                    x,
                    y,
                    count: self.counts[idx],
                    value,
                    lat_center: self.lat_center(y),
                    lon_center: self.lon_center(x),
                }
            })
    }

    fn lat_center(&self, y: usize) -> f32 {
        let dy = (self.bounds.north - self.bounds.south) / self.ny as f32;
        self.bounds.north - (y as f32 + 0.5) * dy
    }

    fn lon_center(&self, x: usize) -> f32 {
        let dx = (self.bounds.east - self.bounds.west) / self.nx as f32;
        self.bounds.west + (x as f32 + 0.5) * dx
    }
}

/// Derive flash density from a satellite's `.rwl` store.
///
/// The output is normalized to flashes per 1000 square kilometers per hour.
/// Raw counts are carried alongside the density values so renderers can choose
/// either a normalized or event-count view.
pub fn flash_density(
    root: &Path,
    satellite: &str,
    request: &FlashDensityRequest,
) -> RwlResult<FlashDensityGrid> {
    request.validate()?;

    let mut counts = vec![0u32; request.nx * request.ny];
    let flashes = read_flashes(
        root,
        satellite,
        request.t0_unix_ms,
        request.t1_unix_ms,
        Some(request.bounds.bbox()),
    )?;

    let lat_span = request.bounds.north - request.bounds.south;
    let lon_span = request.bounds.east - request.bounds.west;
    let mut flash_count = 0usize;
    for flash in flashes {
        if !request.include_degraded && flash.is_degraded() {
            continue;
        }
        let Some((x, y)) = cell_index(
            flash.lat,
            flash.lon,
            request.bounds,
            request.nx,
            request.ny,
            lat_span,
            lon_span,
        ) else {
            continue;
        };
        counts[y * request.nx + x] = counts[y * request.nx + x].saturating_add(1);
        flash_count += 1;
    }

    let hours = (request.t1_unix_ms - request.t0_unix_ms) as f64 / 3_600_000.0;
    let mut values = Vec::with_capacity(counts.len());
    for y in 0..request.ny {
        let lat_n = request.bounds.north as f64 - y as f64 * lat_span as f64 / request.ny as f64;
        let lat_s =
            request.bounds.north as f64 - (y + 1) as f64 * lat_span as f64 / request.ny as f64;
        let lon_w = request.bounds.west as f64;
        let lon_e = request.bounds.east as f64;
        let cell_area = lat_lon_band_area_km2(lat_s, lat_n, lon_w, lon_e) / request.nx as f64;
        for x in 0..request.nx {
            let count = counts[y * request.nx + x] as f64;
            let value = if cell_area > 0.0 && hours > 0.0 {
                count * 1000.0 / cell_area / hours
            } else {
                0.0
            };
            values.push(value as f32);
        }
    }

    Ok(FlashDensityGrid {
        satellite: satellite.to_string(),
        t0_unix_ms: request.t0_unix_ms,
        t1_unix_ms: request.t1_unix_ms,
        bounds: request.bounds,
        nx: request.nx,
        ny: request.ny,
        units: FLASH_DENSITY_UNITS.to_string(),
        values,
        counts,
        flash_count,
    })
}

fn cell_index(
    lat: f32,
    lon: f32,
    bounds: DensityBounds,
    nx: usize,
    ny: usize,
    lat_span: f32,
    lon_span: f32,
) -> Option<(usize, usize)> {
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    if lat < bounds.south || lat > bounds.north || lon < bounds.west || lon > bounds.east {
        return None;
    }
    let x = (((lon - bounds.west) / lon_span) * nx as f32).floor() as isize;
    let y = (((bounds.north - lat) / lat_span) * ny as f32).floor() as isize;
    Some((
        (x.clamp(0, nx as isize - 1)) as usize,
        (y.clamp(0, ny as isize - 1)) as usize,
    ))
}

fn lat_lon_band_area_km2(lat_s: f64, lat_n: f64, lon_w: f64, lon_e: f64) -> f64 {
    let dlon = (lon_e - lon_w).to_radians().abs();
    let south = lat_s.to_radians();
    let north = lat_n.to_radians();
    EARTH_RADIUS_KM * EARTH_RADIUS_KM * dlon * (north.sin() - south.sin()).abs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BucketWriter, FLAG_DEGRADED_QUALITY, FlashRecord};
    use std::path::PathBuf;

    const BASE: i64 = 1_767_225_600_000;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rw-glm-density-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn flash(time: i64, lat: f32, lon: f32, id: u32, flags: u16) -> FlashRecord {
        FlashRecord {
            time_unix_ms: time,
            lat,
            lon,
            energy: 1.0e-15,
            area: 25.0,
            flash_id: id,
            flags,
            duration_ms: 400,
        }
    }

    #[test]
    fn derives_counts_and_density_from_store_flashes() {
        let root = temp_root("counts");
        let mut writer = BucketWriter::open(&root, "goes19").unwrap();
        writer
            .insert_flashes(
                &[
                    flash(BASE + 1000, 1.5, 0.5, 1, 0),
                    flash(BASE + 2000, 1.5, 0.5, 2, 0),
                    flash(BASE + 3000, 0.5, 1.5, 3, 0),
                    flash(BASE + 4000, 1.5, 1.5, 4, FLAG_DEGRADED_QUALITY),
                    flash(BASE + 5000, 3.0, 0.5, 5, 0),
                ],
                1,
            )
            .unwrap();
        drop(writer);

        let request = FlashDensityRequest::new(
            BASE,
            BASE + 3_600_000,
            DensityBounds::new(0.0, 2.0, 0.0, 2.0),
            2,
            2,
        );
        let grid = flash_density(&root, "goes19", &request).unwrap();
        assert_eq!(grid.flash_count, 3);
        assert_eq!(grid.counts, vec![2, 0, 0, 1]);
        assert_eq!(grid.count(0, 0), Some(2));
        assert_eq!(grid.count(1, 1), Some(1));
        assert_eq!(grid.units, FLASH_DENSITY_UNITS);
        assert!(grid.value(0, 0).unwrap() > grid.value(1, 1).unwrap());

        let mut inclusive = request.clone();
        inclusive.include_degraded = true;
        let with_degraded = flash_density(&root, "goes19", &inclusive).unwrap();
        assert_eq!(with_degraded.flash_count, 4);
        assert_eq!(with_degraded.counts, vec![2, 1, 0, 1]);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn validates_density_requests() {
        let root = temp_root("validation");
        let bad_time =
            FlashDensityRequest::new(BASE, BASE, DensityBounds::new(0.0, 2.0, 0.0, 2.0), 2, 2);
        assert!(flash_density(&root, "goes19", &bad_time).is_err());

        let bad_shape =
            FlashDensityRequest::new(BASE, BASE + 1, DensityBounds::new(0.0, 2.0, 0.0, 2.0), 0, 2);
        assert!(flash_density(&root, "goes19", &bad_shape).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}

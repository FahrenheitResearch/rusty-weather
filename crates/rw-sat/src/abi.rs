use chrono::{DateTime, Utc};
use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};

use crate::geostationary::{SweepAngleAxis, scan_angles_to_lat_lon};
use crate::goes::{GoesSatellite, parse_goes_abi_filename};
use crate::netcdf::{
    ScaledVariable, open_goes_netcdf_lossy, read_scaled_f32, read_scaled_f32_window,
    read_scaled_f32_window_strided,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbiSector {
    FullDisk,
    Conus,
    Mesoscale1,
    Mesoscale2,
    Mesoscale,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct AbiFixedGrid {
    pub nx: usize,
    pub ny: usize,
    pub x_scan_rad: Vec<f64>,
    pub y_scan_rad: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoesImagerProjection {
    pub perspective_point_height_m: f64,
    pub semi_major_axis_m: f64,
    pub semi_minor_axis_m: f64,
    pub longitude_of_projection_origin_deg: f64,
    pub sweep_angle_axis: SweepAngleAxis,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoesAbiScene {
    pub path: PathBuf,
    pub product: String,
    pub sector: AbiSector,
    pub channel: Option<u8>,
    pub satellite: GoesSatellite,
    pub start_time_utc: DateTime<Utc>,
    pub end_time_utc: DateTime<Utc>,
    pub projection: GoesImagerProjection,
    pub fixed_grid: AbiFixedGrid,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GoesAbiField {
    pub scene: GoesAbiScene,
    pub variable_name: String,
    pub units: Option<String>,
    pub values: Vec<f32>,
}

impl GoesAbiScene {
    pub fn lat_lon_mesh(&self) -> (Vec<f32>, Vec<f32>) {
        let len = self.fixed_grid.nx.saturating_mul(self.fixed_grid.ny);
        let mut lat = Vec::with_capacity(len);
        let mut lon = Vec::with_capacity(len);
        for &y in &self.fixed_grid.y_scan_rad {
            for &x in &self.fixed_grid.x_scan_rad {
                match self.projection.scan_angles_to_lat_lon(x, y) {
                    Some((lat_value, lon_value)) => {
                        lat.push(lat_value);
                        lon.push(lon_value);
                    }
                    None => {
                        lat.push(f32::NAN);
                        lon.push(f32::NAN);
                    }
                }
            }
        }
        (lat, lon)
    }

    pub fn approximate_lat_lon_bounds(
        &self,
        max_samples_per_axis: usize,
    ) -> Option<(f64, f64, f64, f64)> {
        let max_samples = max_samples_per_axis.max(2);
        let x_step = (self.fixed_grid.nx / max_samples).max(1);
        let y_step = (self.fixed_grid.ny / max_samples).max(1);
        let mut west = f64::INFINITY;
        let mut east = f64::NEG_INFINITY;
        let mut south = f64::INFINITY;
        let mut north = f64::NEG_INFINITY;
        let mut seen = false;

        let mut rows = (0..self.fixed_grid.ny).step_by(y_step).collect::<Vec<_>>();
        if rows.last().copied() != Some(self.fixed_grid.ny.saturating_sub(1)) {
            rows.push(self.fixed_grid.ny.saturating_sub(1));
        }
        let mut cols = (0..self.fixed_grid.nx).step_by(x_step).collect::<Vec<_>>();
        if cols.last().copied() != Some(self.fixed_grid.nx.saturating_sub(1)) {
            cols.push(self.fixed_grid.nx.saturating_sub(1));
        }

        for row in rows {
            let y = self.fixed_grid.y_scan_rad[row];
            for &col in &cols {
                let x = self.fixed_grid.x_scan_rad[col];
                let Some((lat, lon)) = self.projection.scan_angles_to_lat_lon(x, y) else {
                    continue;
                };
                let lat = f64::from(lat);
                let lon = f64::from(lon);
                if !(lat.is_finite() && lon.is_finite()) {
                    continue;
                }
                west = west.min(lon);
                east = east.max(lon);
                south = south.min(lat);
                north = north.max(lat);
                seen = true;
            }
        }

        seen.then_some((west, east, south, north))
    }
}

impl GoesImagerProjection {
    pub fn scan_angles_to_lat_lon(&self, x_rad: f64, y_rad: f64) -> Option<(f32, f32)> {
        scan_angles_to_lat_lon(
            self.perspective_point_height_m,
            self.semi_major_axis_m,
            self.semi_minor_axis_m,
            self.longitude_of_projection_origin_deg,
            self.sweep_angle_axis,
            x_rad,
            y_rad,
        )
    }
}

pub fn read_goes_abi_scene(path: impl AsRef<Path>) -> Result<GoesAbiScene, Box<dyn Error>> {
    let path = path.as_ref();
    read_goes_abi_scene_with_identity(path, path)
}

/// Read ABI grid metadata from `path`, while deriving the NOAA scene identity
/// from `identity_path`.
///
/// Native archives deliberately store each channel under a stable short name
/// such as `c13.nc`. The original NOAA object key retained in the frame
/// manifest remains the authoritative product, channel, platform, and scan
/// identity; callers must not reconstruct those fields from the archive name.
pub fn read_goes_abi_scene_with_identity(
    path: impl AsRef<Path>,
    identity_path: impl AsRef<Path>,
) -> Result<GoesAbiScene, Box<dyn Error>> {
    let path = path.as_ref();
    let parsed = parse_goes_abi_filename(identity_path)?;
    let file = open_goes_netcdf_lossy(path)?;
    let x = read_scaled_f32(&file, "x")?;
    let y = read_scaled_f32(&file, "y")?;
    if x.values.is_empty() || y.values.is_empty() {
        return Err(boxed_error(format!(
            "GOES ABI file has empty fixed grid axes: {}",
            path.display()
        )));
    }

    let projection = read_goes_projection(path, &file)?;

    Ok(GoesAbiScene {
        path: path.to_path_buf(),
        product: parsed.product.clone(),
        sector: sector_from_product(&parsed.product),
        channel: parsed.channel,
        satellite: parsed.satellite,
        start_time_utc: parsed.start_time_utc,
        end_time_utc: parsed.end_time_utc,
        projection,
        fixed_grid: AbiFixedGrid {
            nx: x.values.len(),
            ny: y.values.len(),
            x_scan_rad: x.values.into_iter().map(f64::from).collect(),
            y_scan_rad: y.values.into_iter().map(f64::from).collect(),
        },
    })
}

pub fn read_goes_abi_field(
    path: impl AsRef<Path>,
    variable_name: &str,
) -> Result<GoesAbiField, Box<dyn Error>> {
    let path = path.as_ref();
    let scene = read_goes_abi_scene(path)?;
    let file = open_goes_netcdf_lossy(path)?;
    let variable = read_scaled_f32(&file, variable_name)?;
    validate_field_shape(&scene, &variable)?;
    Ok(GoesAbiField {
        scene,
        variable_name: variable_name.to_string(),
        units: variable.units,
        values: variable.values,
    })
}

pub fn read_goes_abi_field_window(
    path: impl AsRef<Path>,
    variable_name: &str,
    x_start: usize,
    x_count: usize,
    y_start: usize,
    y_count: usize,
) -> Result<GoesAbiField, Box<dyn Error>> {
    let scene = read_goes_abi_scene(path)?;
    read_goes_abi_field_window_from_scene(&scene, variable_name, x_start, x_count, y_start, y_count)
}

/// Read a window using scene metadata that has already been established.
///
/// This is the archive-safe companion to [`read_goes_abi_scene_with_identity`]:
/// it opens `source_scene.path` and never reparses that storage basename.
pub fn read_goes_abi_field_window_from_scene(
    source_scene: &GoesAbiScene,
    variable_name: &str,
    x_start: usize,
    x_count: usize,
    y_start: usize,
    y_count: usize,
) -> Result<GoesAbiField, Box<dyn Error>> {
    if x_count == 0 || y_count == 0 {
        return Err(boxed_error(format!(
            "empty GOES ABI window requested for {variable_name}: x_count={x_count} y_count={y_count}"
        )));
    }
    let mut scene = source_scene.clone();
    if x_start.saturating_add(x_count) > scene.fixed_grid.nx
        || y_start.saturating_add(y_count) > scene.fixed_grid.ny
    {
        return Err(boxed_error(format!(
            "GOES ABI window {x_start}..{} x {y_start}..{} exceeds grid {}x{}",
            x_start.saturating_add(x_count),
            y_start.saturating_add(y_count),
            scene.fixed_grid.nx,
            scene.fixed_grid.ny
        )));
    }
    let file = open_goes_netcdf_lossy(&source_scene.path)?;
    let variable =
        read_scaled_f32_window(&file, variable_name, y_start, y_count, x_start, x_count)?;
    validate_window_shape(variable_name, &variable, x_count, y_count)?;
    scene.fixed_grid = AbiFixedGrid {
        nx: x_count,
        ny: y_count,
        x_scan_rad: scene.fixed_grid.x_scan_rad[x_start..x_start + x_count].to_vec(),
        y_scan_rad: scene.fixed_grid.y_scan_rad[y_start..y_start + y_count].to_vec(),
    };
    Ok(GoesAbiField {
        scene,
        variable_name: variable_name.to_string(),
        units: variable.units,
        values: variable.values,
    })
}

/// A globally aligned, bounded-memory overview window.
///
/// `*_bin_step` defines the native ABI cells represented by one overview
/// value. `*_sample_step` may thin the source samples used in that average,
/// but its phase is anchored to native-grid index zero so independently
/// rendered XYZ tiles calculate identical values for shared overview cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AbiAreaOverviewWindow {
    pub x_start: usize,
    pub x_count: usize,
    pub y_start: usize,
    pub y_count: usize,
    pub x_bin_step: usize,
    pub y_bin_step: usize,
    pub x_sample_step: usize,
    pub y_sample_step: usize,
    pub maximum_read_cells: usize,
}

impl AbiAreaOverviewWindow {
    pub fn output_shape(self) -> (usize, usize) {
        (
            self.x_count.div_ceil(self.x_bin_step),
            self.y_count.div_ceil(self.y_bin_step),
        )
    }

    pub fn sampled_shape(self) -> (usize, usize) {
        (
            aligned_sample_count(self.x_start, self.x_count, self.x_sample_step),
            aligned_sample_count(self.y_start, self.y_count, self.y_sample_step),
        )
    }

    pub fn maximum_block_shape(self) -> (usize, usize) {
        let (sample_nx, sample_ny) = self.sampled_shape();
        if sample_nx == 0 || sample_ny == 0 || self.maximum_read_cells == 0 {
            return (0, 0);
        }
        let block_nx = sample_nx.min(self.maximum_read_cells);
        let block_ny = sample_ny.min((self.maximum_read_cells / block_nx).max(1));
        (block_nx, block_ny)
    }

    fn validate(self, source: &AbiFixedGrid) -> Result<(), Box<dyn Error>> {
        if self.x_count == 0 || self.y_count == 0 {
            return Err(boxed_error("empty GOES ABI overview window requested"));
        }
        if self.x_bin_step == 0
            || self.y_bin_step == 0
            || self.x_sample_step == 0
            || self.y_sample_step == 0
        {
            return Err(boxed_error("GOES ABI overview steps must be positive"));
        }
        if self.x_sample_step > self.x_bin_step || self.y_sample_step > self.y_bin_step {
            return Err(boxed_error(
                "GOES ABI overview sampling may not skip an entire output cell",
            ));
        }
        if self.maximum_read_cells == 0 {
            return Err(boxed_error(
                "GOES ABI overview read-cell bound must be positive",
            ));
        }
        if !self.x_start.is_multiple_of(self.x_bin_step)
            || !self.y_start.is_multiple_of(self.y_bin_step)
        {
            return Err(boxed_error(
                "GOES ABI overview windows must start on global bin boundaries",
            ));
        }
        if self.x_start.saturating_add(self.x_count) > source.nx
            || self.y_start.saturating_add(self.y_count) > source.ny
        {
            return Err(boxed_error(format!(
                "GOES ABI overview {}..{} x {}..{} exceeds grid {}x{}",
                self.x_start,
                self.x_start.saturating_add(self.x_count),
                self.y_start,
                self.y_start.saturating_add(self.y_count),
                source.nx,
                source.ny
            )));
        }
        Ok(())
    }
}

/// Read a low-resolution ABI window as globally aligned area averages.
///
/// The source plane is never materialized. Each NetCDF/HDF5 selection is
/// capped at `maximum_read_cells`, accumulated into a small overview grid,
/// and then discarded. Overview samples are always real source samples; this
/// path smooths/decimates native information and never invents texture.
pub(crate) fn read_goes_abi_field_area_filtered_window_from_scene(
    source_scene: &GoesAbiScene,
    variable_name: &str,
    spec: AbiAreaOverviewWindow,
) -> Result<GoesAbiField, Box<dyn Error>> {
    spec.validate(&source_scene.fixed_grid)?;
    let (output_nx, output_ny) = spec.output_shape();
    let output_len = output_nx
        .checked_mul(output_ny)
        .ok_or_else(|| boxed_error("GOES ABI overview output size overflow"))?;
    let mut sums = vec![0.0f64; output_len];
    let mut counts = vec![0u32; output_len];

    let source_x_end = spec.x_start + spec.x_count;
    let source_y_end = spec.y_start + spec.y_count;
    let sample_x_start = align_up(spec.x_start, spec.x_sample_step)
        .ok_or_else(|| boxed_error("GOES ABI overview x alignment overflow"))?;
    let sample_y_start = align_up(spec.y_start, spec.y_sample_step)
        .ok_or_else(|| boxed_error("GOES ABI overview y alignment overflow"))?;
    let (sample_nx, sample_ny) = spec.sampled_shape();
    if sample_nx == 0 || sample_ny == 0 {
        return Err(boxed_error(
            "GOES ABI overview has no globally aligned source samples",
        ));
    }

    let file = open_goes_netcdf_lossy(&source_scene.path)?;
    let (block_sample_nx, block_sample_ny) = spec.maximum_block_shape();
    let mut units = None::<Option<String>>;

    for sample_y_offset in (0..sample_ny).step_by(block_sample_ny) {
        let rows = (sample_ny - sample_y_offset).min(block_sample_ny);
        let read_y_start = sample_y_start + sample_y_offset * spec.y_sample_step;
        let read_y_count = (rows - 1) * spec.y_sample_step + 1;
        debug_assert!(read_y_start + read_y_count <= source_y_end);

        for sample_x_offset in (0..sample_nx).step_by(block_sample_nx) {
            let columns = (sample_nx - sample_x_offset).min(block_sample_nx);
            let read_x_start = sample_x_start + sample_x_offset * spec.x_sample_step;
            let read_x_count = (columns - 1) * spec.x_sample_step + 1;
            debug_assert!(read_x_start + read_x_count <= source_x_end);
            debug_assert!(rows.saturating_mul(columns) <= spec.maximum_read_cells);

            let variable = read_scaled_f32_window_strided(
                &file,
                variable_name,
                read_y_start,
                read_y_count,
                spec.y_sample_step,
                read_x_start,
                read_x_count,
                spec.x_sample_step,
            )?;
            validate_window_shape(variable_name, &variable, columns, rows)?;
            if let Some(expected_units) = units.as_ref() {
                if expected_units != &variable.units {
                    return Err(boxed_error(format!(
                        "GOES ABI variable {variable_name} units changed between overview blocks"
                    )));
                }
            } else {
                units = Some(variable.units.clone());
            }

            for local_y in 0..rows {
                let source_y = read_y_start + local_y * spec.y_sample_step;
                let output_y = source_y / spec.y_bin_step - spec.y_start / spec.y_bin_step;
                for local_x in 0..columns {
                    let value = variable.values[local_y * columns + local_x];
                    if !value.is_finite() {
                        continue;
                    }
                    let source_x = read_x_start + local_x * spec.x_sample_step;
                    let output_x = source_x / spec.x_bin_step - spec.x_start / spec.x_bin_step;
                    let index = output_y * output_nx + output_x;
                    sums[index] += f64::from(value);
                    counts[index] += 1;
                }
            }
        }
    }

    let values = sums
        .into_iter()
        .zip(counts)
        .map(|(sum, count)| {
            if count == 0 {
                f32::NAN
            } else {
                (sum / f64::from(count)) as f32
            }
        })
        .collect();
    let mut scene = source_scene.clone();
    scene.fixed_grid = AbiFixedGrid {
        nx: output_nx,
        ny: output_ny,
        x_scan_rad: area_bin_centers(
            &source_scene.fixed_grid.x_scan_rad,
            spec.x_start,
            spec.x_count,
            spec.x_bin_step,
        ),
        y_scan_rad: area_bin_centers(
            &source_scene.fixed_grid.y_scan_rad,
            spec.y_start,
            spec.y_count,
            spec.y_bin_step,
        ),
    };
    Ok(GoesAbiField {
        scene,
        variable_name: variable_name.to_string(),
        units: units.flatten(),
        values,
    })
}

fn area_bin_centers(axis: &[f64], start: usize, count: usize, step: usize) -> Vec<f64> {
    let end = start + count;
    (start..end)
        .step_by(step)
        .map(|bin_start| {
            let bin_end = (bin_start + step).min(end);
            let values = &axis[bin_start..bin_end];
            values.iter().sum::<f64>() / values.len() as f64
        })
        .collect()
}

fn aligned_sample_count(start: usize, count: usize, step: usize) -> usize {
    if step == 0 {
        return 0;
    }
    let Some(aligned_start) = align_up(start, step) else {
        return 0;
    };
    let end = start.saturating_add(count);
    if aligned_start >= end {
        0
    } else {
        (end - aligned_start).div_ceil(step)
    }
}

fn align_up(value: usize, step: usize) -> Option<usize> {
    if step == 0 {
        return None;
    }
    value.checked_add(step - 1).map(|sum| sum / step * step)
}

/// Read an exact stride-decimated field without materializing the native
/// plane. This is the bounded preview path for 0.5 km Full Disk C02, whose
/// 21,696² native plane intentionally exceeds the whole-array reader limit.
pub fn read_goes_abi_field_strided_from_scene(
    source_scene: &GoesAbiScene,
    variable_name: &str,
    step: usize,
) -> Result<GoesAbiField, Box<dyn Error>> {
    if step == 0 {
        return Err(boxed_error("GOES ABI field stride must be positive"));
    }
    let source = &source_scene.fixed_grid;
    let nx = source.nx.div_ceil(step);
    let ny = source.ny.div_ceil(step);
    let file = open_goes_netcdf_lossy(&source_scene.path)?;
    let variable = read_scaled_f32_window_strided(
        &file,
        variable_name,
        0,
        source.ny,
        step,
        0,
        source.nx,
        step,
    )?;
    validate_window_shape(variable_name, &variable, nx, ny)?;
    let mut scene = source_scene.clone();
    scene.fixed_grid = AbiFixedGrid {
        nx,
        ny,
        x_scan_rad: source.x_scan_rad.iter().step_by(step).copied().collect(),
        y_scan_rad: source.y_scan_rad.iter().step_by(step).copied().collect(),
    };
    Ok(GoesAbiField {
        scene,
        variable_name: variable_name.to_string(),
        units: variable.units,
        values: variable.values,
    })
}

fn validate_field_shape(
    scene: &GoesAbiScene,
    variable: &ScaledVariable,
) -> Result<(), Box<dyn Error>> {
    let expected_len = scene.fixed_grid.nx.saturating_mul(scene.fixed_grid.ny);
    if variable.values.len() != expected_len {
        return Err(boxed_error(format!(
            "GOES ABI variable {} length {} does not match grid {}x{}",
            variable.name,
            variable.values.len(),
            scene.fixed_grid.nx,
            scene.fixed_grid.ny
        )));
    }
    let shape_matches = match variable.shape.as_slice() {
        [ny, nx] => *nx == scene.fixed_grid.nx && *ny == scene.fixed_grid.ny,
        [len] => *len == expected_len,
        _ => false,
    };
    if !shape_matches {
        return Err(boxed_error(format!(
            "GOES ABI variable {} shape {:?} does not match grid {}x{}",
            variable.name, variable.shape, scene.fixed_grid.nx, scene.fixed_grid.ny
        )));
    }
    Ok(())
}

fn validate_window_shape(
    variable_name: &str,
    variable: &ScaledVariable,
    x_count: usize,
    y_count: usize,
) -> Result<(), Box<dyn Error>> {
    let expected_len = x_count.saturating_mul(y_count);
    if variable.values.len() != expected_len {
        return Err(boxed_error(format!(
            "GOES ABI variable {variable_name} window length {} does not match grid {}x{}",
            variable.values.len(),
            x_count,
            y_count
        )));
    }
    let shape_matches = match variable.shape.as_slice() {
        [ny, nx] => *nx == x_count && *ny == y_count,
        [len] => *len == expected_len,
        _ => false,
    };
    if !shape_matches {
        return Err(boxed_error(format!(
            "GOES ABI variable {variable_name} window shape {:?} does not match grid {}x{}",
            variable.shape, x_count, y_count
        )));
    }
    Ok(())
}

fn read_goes_projection(
    path: &Path,
    file: &netcrust::File,
) -> Result<GoesImagerProjection, Box<dyn Error>> {
    if let Some(projection_var) = file.variable("goes_imager_projection") {
        return Ok(GoesImagerProjection {
            perspective_point_height_m: required_attr_f64(
                &projection_var,
                "perspective_point_height",
            )?,
            semi_major_axis_m: required_attr_f64(&projection_var, "semi_major_axis")?,
            semi_minor_axis_m: required_attr_f64(&projection_var, "semi_minor_axis")?,
            longitude_of_projection_origin_deg: required_attr_f64(
                &projection_var,
                "longitude_of_projection_origin",
            )?,
            sweep_angle_axis: projection_var
                .attribute("sweep_angle_axis")
                .and_then(|attr| attr.as_string())
                .map(SweepAngleAxis::parse)
                .unwrap_or(SweepAngleAxis::X),
        });
    }

    let hdf5 = hdf5_reader::Hdf5File::open(path)?;
    let projection = hdf5.dataset("goes_imager_projection")?;
    Ok(GoesImagerProjection {
        perspective_point_height_m: required_hdf5_attr_f64(
            &projection,
            "perspective_point_height",
        )?,
        semi_major_axis_m: required_hdf5_attr_f64(&projection, "semi_major_axis")?,
        semi_minor_axis_m: required_hdf5_attr_f64(&projection, "semi_minor_axis")?,
        longitude_of_projection_origin_deg: required_hdf5_attr_f64(
            &projection,
            "longitude_of_projection_origin",
        )?,
        sweep_angle_axis: projection
            .attribute("sweep_angle_axis")
            .ok()
            .and_then(|attr| attr.read_string().ok())
            .map(|value| SweepAngleAxis::parse(&value))
            .unwrap_or(SweepAngleAxis::X),
    })
}

fn required_attr_f64(variable: &netcrust::Variable, name: &str) -> Result<f64, Box<dyn Error>> {
    variable
        .attribute(name)
        .and_then(|attr| attr.as_f64())
        .ok_or_else(|| boxed_error(format!("missing numeric projection attribute: {name}")))
}

fn required_hdf5_attr_f64(
    dataset: &hdf5_reader::Dataset,
    name: &str,
) -> Result<f64, Box<dyn Error>> {
    dataset
        .attribute(name)
        .ok()
        .and_then(|attr| attr.read_scalar::<f64>().ok())
        .ok_or_else(|| boxed_error(format!("missing numeric projection attribute: {name}")))
}

fn sector_from_product(product: &str) -> AbiSector {
    let upper = product.to_ascii_uppercase();
    if upper.ends_with("M1") {
        AbiSector::Mesoscale1
    } else if upper.ends_with("M2") {
        AbiSector::Mesoscale2
    } else if upper.ends_with('M') {
        AbiSector::Mesoscale
    } else if upper.ends_with('C') {
        AbiSector::Conus
    } else if upper.ends_with('F') {
        AbiSector::FullDisk
    } else {
        AbiSector::Unknown(product.to_string())
    }
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ARCHIVED_C13_OBJECT_KEY: &str = "ABI-L2-CMIPF/2026/235/00/OR_ABI-L2-CMIPF-M6C13_G19_s20262350000207_e20262350009527_c20262350009577.nc";
    const FULL_DISK_C02_OBJECT_KEY: &str = "ABI-L2-CMIPF/2026/235/02/OR_ABI-L2-CMIPF-M6C02_G18_s20262350240211_e20262350249519_c20262350249572.nc";

    #[test]
    fn infers_sector_from_product_name() {
        assert_eq!(sector_from_product("ABI-L2-MCMIPC"), AbiSector::Conus);
        assert_eq!(sector_from_product("ABI-L2-CMIPF"), AbiSector::FullDisk);
        assert_eq!(sector_from_product("ABI-L2-CMIPM1"), AbiSector::Mesoscale1);
        assert_eq!(sector_from_product("ABI-L2-CMIPM2"), AbiSector::Mesoscale2);
        assert_eq!(sector_from_product("ABI-L2-CMIPM"), AbiSector::Mesoscale);
    }

    #[test]
    fn archived_short_name_uses_the_retained_noaa_object_identity() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "rw-sat-archived-identity-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let archived_path = directory.join("c13.nc");
        std::fs::write(&archived_path, []).unwrap();

        let short_name_error = read_goes_abi_scene(&archived_path).unwrap_err();
        assert!(
            short_name_error
                .downcast_ref::<crate::goes::GoesParseError>()
                .is_some()
        );

        let source_error =
            read_goes_abi_scene_with_identity(&archived_path, ARCHIVED_C13_OBJECT_KEY).unwrap_err();
        assert!(
            source_error
                .downcast_ref::<crate::goes::GoesParseError>()
                .is_none(),
            "the original NOAA object key should establish scene identity before opening c13.nc: {source_error}"
        );
        std::fs::remove_file(archived_path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    #[ignore = "requires RUSTWX_GOES_ABI_ARCHIVED_FIXTURE and RUSTWX_GOES_ABI_OBJECT_KEY"]
    fn reads_an_archived_abi_window_without_reparsing_its_short_name() {
        let path = PathBuf::from(
            std::env::var_os("RUSTWX_GOES_ABI_ARCHIVED_FIXTURE")
                .expect("set RUSTWX_GOES_ABI_ARCHIVED_FIXTURE to archived cNN.nc"),
        );
        let object_key = std::env::var("RUSTWX_GOES_ABI_OBJECT_KEY")
            .expect("set RUSTWX_GOES_ABI_OBJECT_KEY to the original NOAA object key");
        let scene = read_goes_abi_scene_with_identity(&path, object_key).unwrap();
        assert_eq!(scene.path, path);
        assert_eq!(scene.channel, Some(13));
        assert_eq!(scene.satellite, GoesSatellite::G19);
        assert_eq!(scene.sector, AbiSector::FullDisk);

        let x = scene.fixed_grid.nx / 2;
        let y = scene.fixed_grid.ny / 2;
        let field = read_goes_abi_field_window_from_scene(&scene, "CMI", x, 1, y, 1).unwrap();
        assert_eq!(field.values.len(), 1);
        assert_eq!(field.scene.path, path);

        let stride = 16;
        let preview = read_goes_abi_field_strided_from_scene(&scene, "CMI", stride).unwrap();
        assert_eq!(
            preview.scene.fixed_grid.nx,
            scene.fixed_grid.nx.div_ceil(stride)
        );
        assert_eq!(
            preview.scene.fixed_grid.ny,
            scene.fixed_grid.ny.div_ceil(stride)
        );
        assert_eq!(
            preview.values.len(),
            preview.scene.fixed_grid.nx * preview.scene.fixed_grid.ny
        );
        assert_eq!(preview.scene.path, path);
    }

    #[test]
    #[ignore = "requires RUSTWX_GOES_ABI_C02_FIXTURE"]
    fn reads_full_disk_c02_as_a_bounded_strided_preview() {
        let path = PathBuf::from(
            std::env::var_os("RUSTWX_GOES_ABI_C02_FIXTURE")
                .expect("set RUSTWX_GOES_ABI_C02_FIXTURE to a Full Disk C02 NetCDF file"),
        );
        let scene = read_goes_abi_scene_with_identity(&path, FULL_DISK_C02_OBJECT_KEY).unwrap();
        assert_eq!(scene.channel, Some(2));
        assert_eq!(scene.satellite, GoesSatellite::G18);
        assert_eq!(scene.sector, AbiSector::FullDisk);
        assert_eq!(scene.fixed_grid.nx, 21_696);
        assert_eq!(scene.fixed_grid.ny, 21_696);

        let stride = 8;
        let preview = read_goes_abi_field_strided_from_scene(&scene, "CMI", stride).unwrap();
        assert_eq!(preview.scene.fixed_grid.nx, 2_712);
        assert_eq!(preview.scene.fixed_grid.ny, 2_712);
        assert_eq!(preview.values.len(), 2_712 * 2_712);
        assert!(preview.values.len() <= 8_000_000);
    }

    #[test]
    #[ignore]
    fn reads_real_goes_abi_fixture() {
        let path = std::env::var_os("RUSTWX_GOES_ABI_FIXTURE")
            .expect("set RUSTWX_GOES_ABI_FIXTURE to a GOES ABI NetCDF file");
        let scene = read_goes_abi_scene(PathBuf::from(path)).unwrap();
        assert_eq!(scene.fixed_grid.nx, 2500);
        assert_eq!(scene.fixed_grid.ny, 1500);
        let center_x = scene.fixed_grid.x_scan_rad[scene.fixed_grid.nx / 2];
        let center_y = scene.fixed_grid.y_scan_rad[scene.fixed_grid.ny / 2];
        let (lat, lon) = scene
            .projection
            .scan_angles_to_lat_lon(center_x, center_y)
            .expect("center point should intersect earth");
        assert!(lat.is_finite());
        assert!(lon.is_finite());
        let field = read_goes_abi_field(scene.path.clone(), "CMI").unwrap();
        assert_eq!(field.values.len(), 2500 * 1500);
    }
}

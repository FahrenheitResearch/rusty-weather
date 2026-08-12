use crate::error::{ErrorKind, FormulaError, FormulaResult};
use crate::model::{Axis, GridConvention, GridLocation, HeightDatum, VectorBasis};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};
use wrf_core::{ComputeOpts, WrfFile, getvar};

/// A field lookup is relative to the evaluation's base time.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FieldRequest {
    pub name: String,
    pub time_offset: isize,
}

/// Raw resolver result. The evaluator validates shape labels, converts data to
/// coherent SI, and rejects unknown units unless an explicit override exists.
#[derive(Debug, Clone)]
pub struct ResolvedField {
    pub resolved_name: String,
    pub data: Arc<[f64]>,
    pub shape: Vec<usize>,
    pub axes: Vec<Axis>,
    pub units: Option<String>,
    pub grid_location: GridLocation,
    pub vector_basis: Option<VectorBasis>,
    pub description: String,
}

/// Grid facts required by scientifically meaningful local calculus.
#[derive(Debug, Clone)]
pub struct GridMetadata {
    pub nx: usize,
    pub ny: usize,
    pub nz: Option<usize>,
    pub dx_m: f64,
    pub dy_m: f64,
    pub convention: GridConvention,
    pub horizontal_calculus_supported: bool,
    pub mass_map_factor: Option<ResolvedField>,
    pub default_vertical_coordinate: Option<String>,
    pub default_height_datum: Option<HeightDatum>,
}

/// Abstract lookup makes the engine testable with manufactured fields and
/// reusable by future multi-file/chunked data sources.
pub trait FieldResolver {
    fn resolve(&self, request: &FieldRequest) -> FormulaResult<ResolvedField>;
    fn grid_metadata(&self, time_offset: isize) -> FormulaResult<GridMetadata>;
    fn time_seconds(&self, time_offset: isize) -> FormulaResult<f64>;

    fn mass_map_factor(&self, time_offset: isize) -> FormulaResult<Option<ResolvedField>> {
        Ok(self.grid_metadata(time_offset)?.mass_map_factor)
    }

    fn base_time_index(&self) -> Option<usize> {
        None
    }

    fn valid_time(&self, _time_offset: isize) -> Option<String> {
        None
    }

    fn input_identity(&self) -> Option<String> {
        None
    }
}

/// Lazy adapter over wrf-core's `getvar` registry and raw-variable fallback.
pub(crate) struct WrfResolver<'a> {
    file: &'a WrfFile,
    base_time_index: usize,
    compute_options: ComputeOpts,
}

impl<'a> WrfResolver<'a> {
    pub(crate) fn new(file: &'a WrfFile, base_time_index: usize) -> FormulaResult<Self> {
        if base_time_index >= file.nt {
            return Err(FormulaError::new(
                ErrorKind::Time,
                format!(
                    "time index {base_time_index} is outside file with {} times",
                    file.nt
                ),
            ));
        }
        Ok(Self {
            file,
            base_time_index,
            compute_options: ComputeOpts::default(),
        })
    }

    pub(crate) fn with_compute_options(mut self, options: ComputeOpts) -> Self {
        self.compute_options = options;
        self
    }

    fn absolute_time_index(&self, offset: isize) -> FormulaResult<usize> {
        let absolute = self
            .base_time_index
            .checked_add_signed(offset)
            .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Time,
                    format!("time offset {offset} is before the first output"),
                )
            })?;
        if absolute >= self.file.nt {
            return Err(FormulaError::new(
                ErrorKind::Time,
                format!(
                    "time offset {offset} resolves to {absolute}, outside {} outputs",
                    self.file.nt
                ),
            ));
        }
        Ok(absolute)
    }

    fn axes_and_location(&self, shape: &[usize]) -> FormulaResult<(Vec<Axis>, GridLocation)> {
        let result = match shape {
            [ny, nx] if *ny == self.file.ny && *nx == self.file.nx => {
                (vec![Axis::Y, Axis::X], GridLocation::Mass)
            }
            [nz, ny, nx] if *nz == self.file.nz && *ny == self.file.ny && *nx == self.file.nx => {
                (vec![Axis::Z, Axis::Y, Axis::X], GridLocation::Mass)
            }
            [nz, ny, nx]
                if *nz == self.file.nz && *ny == self.file.ny && *nx == self.file.nx_stag =>
            {
                (vec![Axis::Z, Axis::Y, Axis::X], GridLocation::XFace)
            }
            [nz, ny, nx]
                if *nz == self.file.nz && *ny == self.file.ny_stag && *nx == self.file.nx =>
            {
                (vec![Axis::Z, Axis::Y, Axis::X], GridLocation::YFace)
            }
            [nz, ny, nx]
                if *nz == self.file.nz_stag && *ny == self.file.ny && *nx == self.file.nx =>
            {
                (vec![Axis::Z, Axis::Y, Axis::X], GridLocation::ZFace)
            }
            [components, ny, nx] if *ny == self.file.ny && *nx == self.file.nx => {
                let _ = components;
                (vec![Axis::Component, Axis::Y, Axis::X], GridLocation::Mass)
            }
            [components, nz, ny, nx]
                if *nz == self.file.nz && *ny == self.file.ny && *nx == self.file.nx =>
            {
                let _ = components;
                (
                    vec![Axis::Component, Axis::Z, Axis::Y, Axis::X],
                    GridLocation::Mass,
                )
            }
            _ => {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    format!("cannot assign WRF semantic axes to shape {shape:?}"),
                ));
            }
        };
        Ok(result)
    }

    fn map_factor(&self, time_offset: isize) -> FormulaResult<Option<ResolvedField>> {
        let time = self.absolute_time_index(time_offset)?;
        if !self.file.has_var("MAPFAC_M") {
            return Ok(None);
        }
        let data = self.file.read_var("MAPFAC_M", time).map_err(|error| {
            FormulaError::new(
                ErrorKind::Resolver,
                format!("failed reading MAPFAC_M: {error}"),
            )
        })?;
        let shape = self.file.var_shape_no_time("MAPFAC_M").map_err(|error| {
            FormulaError::new(
                ErrorKind::Resolver,
                format!("failed reading MAPFAC_M shape: {error}"),
            )
        })?;
        let (axes, grid_location) = self.axes_and_location(&shape)?;
        Ok(Some(ResolvedField {
            resolved_name: "MAPFAC_M".to_string(),
            data: data.into(),
            shape,
            axes,
            units: Some("1".to_string()),
            grid_location,
            vector_basis: None,
            description: "WRF mass-point map scale factor".to_string(),
        }))
    }
}

impl FieldResolver for WrfResolver<'_> {
    fn resolve(&self, request: &FieldRequest) -> FormulaResult<ResolvedField> {
        let time = self.absolute_time_index(request.time_offset)?;
        let output = getvar(self.file, &request.name, Some(time), &self.compute_options).map_err(
            |error| {
                FormulaError::new(
                    ErrorKind::Resolver,
                    format!("could not resolve field '{}': {error}", request.name),
                )
            },
        )?;
        let (mut axes, grid_location) = self.axes_and_location(&output.shape)?;
        if let Some(definition) = wrf_core::variables::get_var_def(&request.name) {
            match (definition.dim, output.shape.len()) {
                (wrf_core::variables::VarDim::TwoD, 3) => {
                    axes = vec![Axis::Component, Axis::Y, Axis::X];
                }
                (wrf_core::variables::VarDim::ThreeD, 4) => {
                    axes = vec![Axis::Component, Axis::Z, Axis::Y, Axis::X];
                }
                _ => {}
            }
        }
        if axes.contains(&Axis::Component) {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "field '{}' has a packed component axis; packed WRF diagnostics may have heterogeneous units and must be selected through component-specific variables",
                    request.name
                ),
            ));
        }
        let resolved_name = wrf_core::variables::get_var_def(&request.name)
            .map(|definition| definition.name.to_string())
            .unwrap_or_else(|| request.name.to_ascii_uppercase());
        Ok(ResolvedField {
            resolved_name,
            data: output.data.into(),
            shape: output.shape,
            axes,
            units: if output.units.trim().is_empty() {
                None
            } else {
                Some(output.units)
            },
            grid_location,
            vector_basis: None,
            description: output.description,
        })
    }

    fn grid_metadata(&self, time_offset: isize) -> FormulaResult<GridMetadata> {
        let _ = self.absolute_time_index(time_offset)?;
        // Vertical-only operations do not require projection metadata. Missing
        // horizontal attributes are represented as unsupported/NaN and rejected
        // only when a horizontal operator or spacing requirement is requested.
        let map_proj = self.file.global_attr_i32("MAP_PROJ").unwrap_or(-1);
        let dx_m = self.file.global_attr_f64("DX").unwrap_or(f64::NAN);
        let dy_m = self.file.global_attr_f64("DY").unwrap_or(f64::NAN);
        Ok(GridMetadata {
            nx: self.file.nx,
            ny: self.file.ny,
            nz: Some(self.file.nz),
            dx_m,
            dy_m,
            convention: if map_proj == 0 {
                GridConvention::Cartesian
            } else {
                GridConvention::WrfMassPointProjected
            },
            horizontal_calculus_supported: matches!(map_proj, 0 | 1 | 2 | 3),
            mass_map_factor: None,
            default_vertical_coordinate: Some("height".to_string()),
            default_height_datum: Some(HeightDatum::Msl),
        })
    }

    fn mass_map_factor(&self, time_offset: isize) -> FormulaResult<Option<ResolvedField>> {
        self.map_factor(time_offset)
    }

    fn time_seconds(&self, time_offset: isize) -> FormulaResult<f64> {
        let index = self.absolute_time_index(time_offset)?;
        let times = self.file.times().map_err(|error| {
            FormulaError::new(
                ErrorKind::Time,
                format!("failed reading WRF Times: {error}"),
            )
        })?;
        let text = times.get(index).ok_or_else(|| {
            FormulaError::new(ErrorKind::Time, format!("WRF Times has no entry {index}"))
        })?;
        parse_wrf_time_seconds(text)
    }

    fn base_time_index(&self) -> Option<usize> {
        Some(self.base_time_index)
    }

    fn valid_time(&self, time_offset: isize) -> Option<String> {
        let index = self.absolute_time_index(time_offset).ok()?;
        self.file.times().ok()?.get(index).cloned()
    }

    fn input_identity(&self) -> Option<String> {
        let path = self.file.path.to_string_lossy();
        let metadata = std::fs::metadata(&self.file.path).ok();
        Some(match metadata {
            Some(metadata) => format!(
                "{};bytes={};modified={:?}",
                path,
                metadata.len(),
                metadata.modified().ok()
            ),
            None => path.into_owned(),
        })
    }
}

// wrf-core currently has a single-time cache and inconsistent lock ordering in
// cache-management methods. Serialize Formula Lab WRF evaluations until that
// cache becomes independently concurrency-safe. Poisoning becomes an error.
static WRF_FORMULA_EVALUATION: Mutex<()> = Mutex::new(());

pub(crate) fn lock_wrf_evaluation() -> FormulaResult<MutexGuard<'static, ()>> {
    WRF_FORMULA_EVALUATION.lock().map_err(|_| {
        FormulaError::new(
            ErrorKind::Internal,
            "WRF formula evaluation lock was poisoned",
        )
    })
}

fn parse_wrf_time_seconds(text: &str) -> FormulaResult<f64> {
    let normalized = text.trim().replace('_', "-").replace(':', "-");
    let parts: Vec<&str> = normalized.split('-').collect();
    if parts.len() != 6 {
        return Err(FormulaError::new(
            ErrorKind::Time,
            format!("expected WRF time YYYY-MM-DD_HH:MM:SS, got '{text}'"),
        ));
    }
    let parse = |index: usize, label: &str| -> FormulaResult<i64> {
        parts[index].parse::<i64>().map_err(|_| {
            FormulaError::new(
                ErrorKind::Time,
                format!("invalid {label} in WRF time '{text}'"),
            )
        })
    };
    let year = parse(0, "year")?;
    let month = parse(1, "month")?;
    let day = parse(2, "day")?;
    let hour = parse(3, "hour")?;
    let minute = parse(4, "minute")?;
    let second = parse(5, "second")?;
    if !(1600..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return Err(FormulaError::new(
            ErrorKind::Time,
            format!("out-of-range WRF time '{text}'"),
        ));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days_in_month = match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    };
    if day < 1 || day > days_in_month {
        return Err(FormulaError::new(
            ErrorKind::Time,
            format!("invalid calendar day in WRF time '{text}'"),
        ));
    }
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days_since_epoch = era * 146_097 + day_of_era - 719_468;
    Ok((days_since_epoch * 86_400 + hour * 3600 + minute * 60 + second) as f64)
}

#[cfg(test)]
mod tests {
    use super::parse_wrf_time_seconds;

    #[test]
    fn wrf_timestamps_have_correct_spacing() {
        let first = parse_wrf_time_seconds("2011-04-27_18:00:00").unwrap();
        let next = parse_wrf_time_seconds("2011-04-27_18:05:00").unwrap();
        assert_eq!(next - first, 300.0);
    }
}

use std::error::Error;
use std::io;
use std::path::Path;

use hdf5_reader::{Datatype, SliceInfo as H5SliceInfo, SliceInfoElem as H5SliceInfoElem};

#[derive(Debug, Clone, PartialEq)]
pub struct ScaledVariable {
    pub name: String,
    pub shape: Vec<usize>,
    pub units: Option<String>,
    pub values: Vec<f32>,
}

pub fn open_goes_netcdf_lossy(path: impl AsRef<Path>) -> Result<netcrust::File, Box<dyn Error>> {
    let options = netcrust::NcOpenOptions {
        metadata_mode: netcrust::NcMetadataMode::Lossy,
        ..Default::default()
    };
    Ok(netcrust::File::open_with_options(path, options)?)
}

pub fn read_scaled_f32(
    file: &netcrust::File,
    name: &str,
) -> Result<ScaledVariable, Box<dyn Error>> {
    let Some(variable) = file.variable(name) else {
        let path = file
            .path()
            .ok_or_else(|| boxed_error(format!("variable not found: {name}")))?;
        return read_scaled_f32_hdf5(path, name);
    };
    let scale = variable
        .attribute("scale_factor")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(1.0);
    let offset = variable
        .attribute("add_offset")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(0.0);
    let fill = variable
        .attribute("_FillValue")
        .and_then(|attr| attr.as_f64());
    let missing = variable
        .attribute("missing_value")
        .map(|attr| {
            attr.value()
                .as_f64_vec()
                .or_else(|| attr.as_f64().map(|value| vec![value]))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let valid_range = variable
        .attribute("valid_range")
        .and_then(|attr| attr.value().as_f64_vec())
        .and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    let valid_min = variable
        .attribute("valid_min")
        .and_then(|attr| attr.as_f64());
    let valid_max = variable
        .attribute("valid_max")
        .and_then(|attr| attr.as_f64());
    let validity = PackedValidity::new(fill, missing, valid_range, valid_min, valid_max)?;
    let units = variable
        .attribute("units")
        .and_then(|attr| attr.as_string())
        .map(str::to_string);

    let array = variable.array_f64()?;
    let shape = array.shape().to_vec();
    let values = scale_values(array.into_values(), scale, offset, &validity)?;

    Ok(ScaledVariable {
        name: name.to_string(),
        shape,
        units,
        values,
    })
}

pub fn read_scaled_f32_window(
    file: &netcrust::File,
    name: &str,
    y_start: usize,
    y_count: usize,
    x_start: usize,
    x_count: usize,
) -> Result<ScaledVariable, Box<dyn Error>> {
    read_scaled_f32_window_strided(file, name, y_start, y_count, 1, x_start, x_count, 1)
}

#[allow(clippy::too_many_arguments)]
pub fn read_scaled_f32_window_strided(
    file: &netcrust::File,
    name: &str,
    y_start: usize,
    y_count: usize,
    y_step: usize,
    x_start: usize,
    x_count: usize,
    x_step: usize,
) -> Result<ScaledVariable, Box<dyn Error>> {
    if y_count == 0 || x_count == 0 {
        return Err(boxed_error(format!(
            "empty NetCDF window requested for {name}: y_count={y_count} x_count={x_count}"
        )));
    }
    if y_step == 0 || x_step == 0 {
        return Err(boxed_error(format!(
            "NetCDF window stride must be positive for {name}: y_step={y_step} x_step={x_step}"
        )));
    }
    let Some(variable) = file.variable(name) else {
        let path = file
            .path()
            .ok_or_else(|| boxed_error(format!("variable not found: {name}")))?;
        return read_scaled_f32_hdf5_window(
            path, name, y_start, y_count, y_step, x_start, x_count, x_step,
        );
    };
    if variable.ndim() != 2 {
        return Err(boxed_error(format!(
            "window reads require a 2D variable; {name} has shape {:?}",
            variable.shape()
        )));
    }
    let scale = variable
        .attribute("scale_factor")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(1.0);
    let offset = variable
        .attribute("add_offset")
        .and_then(|attr| attr.as_f64())
        .unwrap_or(0.0);
    let fill = variable
        .attribute("_FillValue")
        .and_then(|attr| attr.as_f64());
    let missing = variable
        .attribute("missing_value")
        .map(|attr| {
            attr.value()
                .as_f64_vec()
                .or_else(|| attr.as_f64().map(|value| vec![value]))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let valid_range = variable
        .attribute("valid_range")
        .and_then(|attr| attr.value().as_f64_vec())
        .and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    let valid_min = variable
        .attribute("valid_min")
        .and_then(|attr| attr.as_f64());
    let valid_max = variable
        .attribute("valid_max")
        .and_then(|attr| attr.as_f64());
    let validity = PackedValidity::new(fill, missing, valid_range, valid_min, valid_max)?;
    let units = variable
        .attribute("units")
        .and_then(|attr| attr.as_string())
        .map(str::to_string);

    let selection = netcrust::NcSliceInfo {
        selections: vec![
            netcrust::NcSliceInfoElem::Slice {
                start: y_start as u64,
                end: y_start.saturating_add(y_count) as u64,
                step: y_step as u64,
            },
            netcrust::NcSliceInfoElem::Slice {
                start: x_start as u64,
                end: x_start.saturating_add(x_count) as u64,
                step: x_step as u64,
            },
        ],
    };
    let array = match variable.array_f64_slice(&selection) {
        Ok(array) => array,
        Err(err) => {
            let path = file.path().ok_or_else(|| {
                boxed_error(format!(
                    "failed to read {name} slice and file path is unavailable"
                ))
            })?;
            return read_scaled_f32_hdf5_window(
                path, name, y_start, y_count, y_step, x_start, x_count, x_step,
            )
            .map_err(|fallback_err| {
                boxed_error(format!(
                    "failed to read {name} slice through netcrust ({err}); HDF5 fallback failed: {fallback_err}"
                ))
            });
        }
    };
    let shape = array.shape().to_vec();
    let values = scale_values(array.into_values(), scale, offset, &validity)?;

    Ok(ScaledVariable {
        name: name.to_string(),
        shape,
        units,
        values,
    })
}

fn read_scaled_f32_hdf5(path: &Path, name: &str) -> Result<ScaledVariable, Box<dyn Error>> {
    let file = hdf5_reader::Hdf5File::open(path)?;
    let dataset = file.dataset(name)?;
    let scale = hdf5_attr_f64(&dataset, "scale_factor").unwrap_or(1.0);
    let offset = hdf5_attr_f64(&dataset, "add_offset").unwrap_or(0.0);
    let fill = hdf5_attr_f64(&dataset, "_FillValue");
    let missing = hdf5_attr_f64_vec(&dataset, "missing_value")
        .or_else(|| hdf5_attr_f64(&dataset, "missing_value").map(|value| vec![value]))
        .unwrap_or_default();
    let valid_range =
        hdf5_attr_f64_vec(&dataset, "valid_range").and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    let valid_min = hdf5_attr_f64(&dataset, "valid_min");
    let valid_max = hdf5_attr_f64(&dataset, "valid_max");
    let validity = PackedValidity::new(fill, missing, valid_range, valid_min, valid_max)?;
    let units = dataset
        .attribute("units")
        .ok()
        .and_then(|attr| attr.read_string().ok());
    let shape = dataset
        .shape()
        .iter()
        .map(|&value| usize::try_from(value))
        .collect::<Result<Vec<_>, _>>()?;
    let values = scale_values(hdf5_dataset_values_f64(&dataset)?, scale, offset, &validity)?;

    Ok(ScaledVariable {
        name: name.to_string(),
        shape,
        units,
        values,
    })
}

fn read_scaled_f32_hdf5_window(
    path: &Path,
    name: &str,
    y_start: usize,
    y_count: usize,
    y_step: usize,
    x_start: usize,
    x_count: usize,
    x_step: usize,
) -> Result<ScaledVariable, Box<dyn Error>> {
    let file = hdf5_reader::Hdf5File::open(path)?;
    let dataset = file.dataset(name)?;
    if dataset.ndim() != 2 {
        return Err(boxed_error(format!(
            "window reads require a 2D HDF5 dataset; {name} has shape {:?}",
            dataset.shape()
        )));
    }
    let scale = hdf5_attr_f64(&dataset, "scale_factor").unwrap_or(1.0);
    let offset = hdf5_attr_f64(&dataset, "add_offset").unwrap_or(0.0);
    let fill = hdf5_attr_f64(&dataset, "_FillValue");
    let missing = hdf5_attr_f64_vec(&dataset, "missing_value")
        .or_else(|| hdf5_attr_f64(&dataset, "missing_value").map(|value| vec![value]))
        .unwrap_or_default();
    let valid_range =
        hdf5_attr_f64_vec(&dataset, "valid_range").and_then(|values| match values.as_slice() {
            [min, max, ..] => Some((*min, *max)),
            _ => None,
        });
    let valid_min = hdf5_attr_f64(&dataset, "valid_min");
    let valid_max = hdf5_attr_f64(&dataset, "valid_max");
    let validity = PackedValidity::new(fill, missing, valid_range, valid_min, valid_max)?;
    let units = dataset
        .attribute("units")
        .ok()
        .and_then(|attr| attr.read_string().ok());
    let selection = H5SliceInfo {
        selections: vec![
            H5SliceInfoElem::Slice {
                start: y_start as u64,
                end: y_start.saturating_add(y_count) as u64,
                step: y_step as u64,
            },
            H5SliceInfoElem::Slice {
                start: x_start as u64,
                end: x_start.saturating_add(x_count) as u64,
                step: x_step as u64,
            },
        ],
    };
    let array = hdf5_dataset_values_f64_slice(&dataset, &selection)?;
    let shape = vec![y_count.div_ceil(y_step), x_count.div_ceil(x_step)];
    let values = scale_values(array, scale, offset, &validity)?;

    Ok(ScaledVariable {
        name: name.to_string(),
        shape,
        units,
        values,
    })
}

#[derive(Debug, Default)]
struct PackedValidity {
    missing: Vec<f64>,
    valid_min: Option<f64>,
    valid_max: Option<f64>,
}

impl PackedValidity {
    fn new(
        fill: Option<f64>,
        mut missing: Vec<f64>,
        valid_range: Option<(f64, f64)>,
        valid_min: Option<f64>,
        valid_max: Option<f64>,
    ) -> Result<Self, Box<dyn Error>> {
        if let Some(fill) = fill {
            missing.push(fill);
        }
        let (valid_min, valid_max) = valid_range
            .map(|(minimum, maximum)| (Some(minimum), Some(maximum)))
            .unwrap_or((valid_min, valid_max));
        if valid_min.is_some_and(|value| !value.is_finite())
            || valid_max.is_some_and(|value| !value.is_finite())
            || valid_min.zip(valid_max).is_some_and(|(min, max)| min > max)
        {
            return Err(boxed_error("invalid packed NetCDF validity bounds"));
        }
        Ok(Self {
            missing,
            valid_min,
            valid_max,
        })
    }

    fn rejects(&self, value: f64) -> bool {
        !value.is_finite()
            || self
                .missing
                .iter()
                .any(|&missing| value == missing || (value.is_nan() && missing.is_nan()))
            || self.valid_min.is_some_and(|minimum| value < minimum)
            || self.valid_max.is_some_and(|maximum| value > maximum)
    }
}

fn scale_values(
    values: Vec<f64>,
    scale: f64,
    offset: f64,
    validity: &PackedValidity,
) -> Result<Vec<f32>, Box<dyn Error>> {
    if !scale.is_finite() || !offset.is_finite() {
        return Err(boxed_error("invalid NetCDF scale_factor or add_offset"));
    }
    Ok(values
        .into_iter()
        .map(|value| {
            if validity.rejects(value) {
                return f32::NAN;
            }
            let scaled = (value * scale + offset) as f32;
            if scaled.is_finite() { scaled } else { f32::NAN }
        })
        .collect())
}

fn hdf5_dataset_values_f64(dataset: &hdf5_reader::Dataset) -> Result<Vec<f64>, Box<dyn Error>> {
    match dataset.dtype() {
        Datatype::FloatingPoint { size: 4, .. } => Ok(dataset
            .read_array::<f32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FloatingPoint { size: 8, .. } => {
            Ok(dataset.read_array::<f64>()?.iter().copied().collect())
        }
        Datatype::FixedPoint {
            size: 1,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i8>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u8>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i16>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u16>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u32>()?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: true,
            ..
        } => Ok(dataset
            .read_array::<i64>()?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: false,
            ..
        } => Ok(dataset
            .read_array::<u64>()?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        dtype => Err(boxed_error(format!(
            "unsupported HDF5 numeric dataset type for {}: {dtype:?}",
            dataset.name()
        ))),
    }
}

fn hdf5_dataset_values_f64_slice(
    dataset: &hdf5_reader::Dataset,
    selection: &H5SliceInfo,
) -> Result<Vec<f64>, Box<dyn Error>> {
    match dataset.dtype() {
        Datatype::FloatingPoint { size: 4, .. } => Ok(dataset
            .read_slice::<f32>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FloatingPoint { size: 8, .. } => Ok(dataset
            .read_slice::<f64>(selection)?
            .iter()
            .copied()
            .collect()),
        Datatype::FixedPoint {
            size: 1,
            signed: true,
            ..
        } => Ok(dataset
            .read_slice::<i8>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => Ok(dataset
            .read_slice::<u8>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => Ok(dataset
            .read_slice::<i16>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => Ok(dataset
            .read_slice::<u16>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: true,
            ..
        } => Ok(dataset
            .read_slice::<i32>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 4,
            signed: false,
            ..
        } => Ok(dataset
            .read_slice::<u32>(selection)?
            .iter()
            .map(|&value| f64::from(value))
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: true,
            ..
        } => Ok(dataset
            .read_slice::<i64>(selection)?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        Datatype::FixedPoint {
            size: 8,
            signed: false,
            ..
        } => Ok(dataset
            .read_slice::<u64>(selection)?
            .iter()
            .map(|&value| value as f64)
            .collect()),
        dtype => Err(boxed_error(format!(
            "unsupported HDF5 numeric dataset type for {}: {dtype:?}",
            dataset.name()
        ))),
    }
}

fn hdf5_attr_f64(dataset: &hdf5_reader::Dataset, name: &str) -> Option<f64> {
    dataset
        .attribute(name)
        .ok()
        .and_then(|attr| attr.read_as_f64().ok())
}

fn hdf5_attr_f64_vec(dataset: &hdf5_reader::Dataset, name: &str) -> Option<Vec<f64>> {
    let attr = dataset.attribute(name).ok()?;
    match &attr.datatype {
        Datatype::FloatingPoint { size: 4, .. } => Some(
            attr.read_1d::<f32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FloatingPoint { size: 8, .. } => attr.read_1d::<f64>().ok(),
        Datatype::FixedPoint {
            size: 1,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i8>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 1,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u8>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 2,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i16>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 2,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u16>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 4,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 4,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 8,
            signed: true,
            ..
        } => Some(
            attr.read_1d::<i64>()
                .ok()?
                .into_iter()
                .map(|value| value as f64)
                .collect(),
        ),
        Datatype::FixedPoint {
            size: 8,
            signed: false,
            ..
        } => Some(
            attr.read_1d::<u64>()
                .ok()?
                .into_iter()
                .map(|value| value as f64)
                .collect(),
        ),
        _ => None,
    }
}

fn boxed_error(message: impl Into<String>) -> Box<dyn Error> {
    Box::new(io::Error::new(io::ErrorKind::InvalidData, message.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cf_missing_values_and_bounds_apply_in_the_packed_domain() {
        let validity =
            PackedValidity::new(Some(-999.0), vec![-888.0], Some((0.0, 100.0)), None, None)
                .unwrap();
        let values = scale_values(
            vec![-999.0, -888.0, -1.0, 0.0, 50.0, 100.0, 101.0],
            0.5,
            1.0,
            &validity,
        )
        .unwrap();
        assert!(values[0].is_nan());
        assert!(values[1].is_nan());
        assert!(values[2].is_nan());
        assert_eq!(values[3..6], [1.0, 26.0, 51.0]);
        assert!(values[6].is_nan());
    }

    #[test]
    fn floating_fill_comparison_is_exact_not_a_half_unit_window() {
        let validity = PackedValidity::new(Some(10.25), Vec::new(), None, None, None).unwrap();
        let values = scale_values(vec![10.25, 10.3], 1.0, 0.0, &validity).unwrap();
        assert!(values[0].is_nan());
        assert_eq!(values[1], 10.3_f32);
    }

    #[test]
    fn separate_cf_valid_min_and_max_are_supported() {
        let validity = PackedValidity::new(None, Vec::new(), None, Some(2.0), Some(4.0)).unwrap();
        let values = scale_values(vec![1.0, 2.0, 4.0, 5.0], 1.0, 0.0, &validity).unwrap();
        assert!(values[0].is_nan());
        assert_eq!(values[1..3], [2.0, 4.0]);
        assert!(values[3].is_nan());
    }

    #[test]
    fn malformed_scaling_and_bounds_are_rejected() {
        assert!(PackedValidity::new(None, Vec::new(), Some((2.0, 1.0)), None, None).is_err());
        assert!(scale_values(vec![1.0], f64::INFINITY, 0.0, &PackedValidity::default()).is_err());
    }
}

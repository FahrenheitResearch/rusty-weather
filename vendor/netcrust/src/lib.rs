//! WRF-focused, pure-Rust NetCDF reader facade.
//!
//! `netcrust` is intentionally smaller than the full C-backed `netcdf` crate
//! API. It exposes the read surface used by weather workflows here: dimensions,
//! global attributes, variable metadata, promoted `f64` reads, and the WRF
//! convention of reading the first time record for 3-D-or-higher variables.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hdf5_reader::{Hdf5File, SliceInfo as H5SliceInfo, SliceInfoElem as H5SliceInfoElem};
use ndarray::ArrayD;
pub use netcdf_reader::{NcFormat, NcMetadataMode, NcOpenOptions, NcSliceInfo, NcSliceInfoElem};

use netcdf_reader::{NcAttrValue, NcDimension, NcFile, NcType, NcVariable};

/// HDF5/NetCDF4 signature bytes.
pub const HDF5_SIGNATURE: [u8; 8] = [0x89, b'H', b'D', b'F', 0x0D, 0x0A, 0x1A, 0x0A];

/// Result type used by `netcrust`.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors produced by `netcrust`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("NetCDF read error: {0}")]
    Netcdf(#[from] netcdf_reader::Error),

    #[error("HDF5 read error: {0}")]
    Hdf5(String),

    #[error("variable not found: {0}")]
    VariableNotFound(String),

    #[error("dimension size for {name} exceeds usize: {size}")]
    DimensionTooLarge { name: String, size: u64 },
}

/// Open a NetCDF file.
pub fn open(path: impl AsRef<Path>) -> Result<File> {
    File::open(path)
}

/// Returns true when `bytes` starts with the HDF5/NetCDF4 signature.
pub fn looks_like_hdf5(bytes: &[u8]) -> bool {
    bytes.len() >= HDF5_SIGNATURE.len() && bytes[..HDF5_SIGNATURE.len()] == HDF5_SIGNATURE
}

/// Returns true when `bytes` starts with a supported NetCDF classic or HDF5 signature.
pub fn looks_like_netcdf(bytes: &[u8]) -> bool {
    looks_like_hdf5(bytes) || matches!(bytes.get(..4), Some([b'C', b'D', b'F', 1 | 2 | 5]))
}

/// Opened NetCDF file.
#[derive(Clone)]
pub struct File {
    inner: Arc<NcFile>,
    hdf5: Option<Arc<Hdf5File>>,
    path: Option<PathBuf>,
    dimension_overrides: Arc<HashMap<String, usize>>,
}

impl File {
    /// Open a NetCDF file from disk.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let inner = NcFile::open(path)?;
        let dimension_overrides = infer_dimension_overrides(&inner);
        Ok(Self {
            inner: Arc::new(inner),
            hdf5: Hdf5File::open(path).ok().map(Arc::new),
            path: Some(path.to_path_buf()),
            dimension_overrides: Arc::new(dimension_overrides),
        })
    }

    /// Open a NetCDF file from disk with custom reader options.
    pub fn open_with_options(path: impl AsRef<Path>, options: NcOpenOptions) -> Result<Self> {
        let path = path.as_ref();
        let inner = NcFile::open_with_options(path, options)?;
        let dimension_overrides = infer_dimension_overrides(&inner);
        Ok(Self {
            inner: Arc::new(inner),
            hdf5: Hdf5File::open(path).ok().map(Arc::new),
            path: Some(path.to_path_buf()),
            dimension_overrides: Arc::new(dimension_overrides),
        })
    }

    /// Open a NetCDF file from in-memory bytes.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let inner = NcFile::from_bytes(bytes)?;
        let dimension_overrides = infer_dimension_overrides(&inner);
        Ok(Self {
            inner: Arc::new(inner),
            hdf5: Hdf5File::from_bytes(bytes).ok().map(Arc::new),
            path: None,
            dimension_overrides: Arc::new(dimension_overrides),
        })
    }

    /// Open a NetCDF file from in-memory bytes with custom reader options.
    pub fn from_bytes_with_options(bytes: &[u8], options: NcOpenOptions) -> Result<Self> {
        let inner = NcFile::from_bytes_with_options(bytes, options)?;
        let dimension_overrides = infer_dimension_overrides(&inner);
        Ok(Self {
            inner: Arc::new(inner),
            hdf5: Hdf5File::from_bytes(bytes).ok().map(Arc::new),
            path: None,
            dimension_overrides: Arc::new(dimension_overrides),
        })
    }

    /// Source path when the file came from disk.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Detected NetCDF format.
    pub fn format(&self) -> NcFormat {
        self.inner.format()
    }

    /// Root-group dimensions.
    pub fn dimensions(&self) -> Result<Vec<Dimension>> {
        self.inner
            .dimensions()?
            .iter()
            .map(|dim| Dimension::try_from(dim, &self.dimension_overrides))
            .collect()
    }

    /// Find a dimension by name or root-relative path.
    pub fn dimension(&self, name: &str) -> Option<Dimension> {
        self.inner
            .dimension(name)
            .ok()
            .and_then(|dim| Dimension::try_from(dim, &self.dimension_overrides).ok())
    }

    /// Root-group variables.
    pub fn variables(&self) -> Result<Vec<Variable>> {
        self.inner
            .variables()?
            .iter()
            .map(|var| {
                Variable::try_from_reader(self.inner.clone(), self.dimension_overrides.clone(), var)
            })
            .collect()
    }

    /// Find a variable by name or root-relative path.
    pub fn variable(&self, name: &str) -> Option<Variable> {
        self.inner.variable(name).ok().and_then(|var| {
            Variable::try_from_reader(self.inner.clone(), self.dimension_overrides.clone(), var)
                .ok()
        })
    }

    /// Find a root-group/global attribute by name or root-relative path.
    pub fn attribute(&self, name: &str) -> Option<Attribute> {
        self.inner
            .global_attribute(name)
            .ok()
            .map(Attribute::from_reader)
    }

    /// Root-group/global attributes.
    pub fn attributes(&self) -> Result<Vec<Attribute>> {
        Ok(self
            .inner
            .global_attributes()?
            .iter()
            .map(Attribute::from_reader)
            .collect())
    }

    /// Read a variable as promoted `f64` values with shape metadata.
    pub fn read_array_f64(&self, name: &str) -> Result<DataArray> {
        match self.inner.read_variable_as_f64(name) {
            Ok(array) => Ok(DataArray::from_ndarray(array)),
            Err(err) => self.read_hdf5_dataset_all(name).map_err(|_| err.into()),
        }
    }

    /// Read a hyperslab selection as promoted `f64` values with shape metadata.
    pub fn read_array_f64_slice(&self, name: &str, selection: &NcSliceInfo) -> Result<DataArray> {
        let array = self.inner.read_variable_slice_as_f64(name, selection)?;
        Ok(DataArray::from_ndarray(array))
    }

    /// Read a variable as promoted flat `f64` values.
    pub fn read_f64(&self, name: &str) -> Result<Vec<f64>> {
        Ok(self.read_array_f64(name)?.into_values())
    }

    /// Read the first WRF time record for variables with rank >= 3; otherwise read all values.
    ///
    /// This mirrors the behavior used by the current `rustwx-wrf` reader for
    /// WRF variables shaped like `[Time, south_north, west_east]` or
    /// `[Time, bottom_top, south_north, west_east]`.
    pub fn read_array_f64_first_record_or_all(&self, name: &str) -> Result<DataArray> {
        let variable = match self.inner.variable(name) {
            Ok(variable) => variable,
            Err(_) => return self.read_hdf5_dataset_first_record_or_all(name),
        };

        if variable.ndim() >= 3 {
            let selection = first_record_selection(variable.ndim());
            let array = self.inner.read_variable_slice_as_f64(name, &selection)?;
            Ok(DataArray::from_ndarray(array))
        } else {
            self.read_array_f64(name)
        }
    }

    fn read_hdf5_dataset_all(&self, name: &str) -> Result<DataArray> {
        let Some(hdf5) = self.hdf5.as_ref() else {
            return Err(Error::VariableNotFound(name.to_string()));
        };
        let dataset = hdf5
            .dataset(name)
            .map_err(|_| Error::VariableNotFound(name.to_string()))?;
        read_hdf5_dataset_as_f64(&dataset, None)
    }

    fn read_hdf5_dataset_first_record_or_all(&self, name: &str) -> Result<DataArray> {
        let Some(hdf5) = self.hdf5.as_ref() else {
            return Err(Error::VariableNotFound(name.to_string()));
        };
        let dataset = hdf5
            .dataset(name)
            .map_err(|_| Error::VariableNotFound(name.to_string()))?;
        let selection = if dataset.ndim() >= 3 {
            Some(first_hdf5_record_selection(dataset.ndim()))
        } else {
            None
        };
        read_hdf5_dataset_as_f64(&dataset, selection.as_ref())
    }

    /// Read first WRF time record or all values as flat promoted `f64` values.
    pub fn read_f64_first_record_or_all(&self, name: &str) -> Result<Vec<f64>> {
        Ok(self.read_array_f64_first_record_or_all(name)?.into_values())
    }
}

/// Dimension metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dimension {
    name: String,
    len: usize,
    unlimited: bool,
}

impl Dimension {
    fn try_from(dim: &NcDimension, overrides: &HashMap<String, usize>) -> Result<Self> {
        Ok(Self {
            name: dim.name.clone(),
            len: match overrides.get(&dim.name) {
                Some(len) => *len,
                None => usize::try_from(dim.size).map_err(|_| Error::DimensionTooLarge {
                    name: dim.name.clone(),
                    size: dim.size,
                })?,
            },
            unlimited: dim.is_unlimited,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_unlimited(&self) -> bool {
        self.unlimited
    }
}

/// Variable metadata and read helpers.
#[derive(Clone)]
pub struct Variable {
    file: Arc<NcFile>,
    name: String,
    dimensions: Vec<Dimension>,
    dtype: DataType,
    attributes: Vec<Attribute>,
}

impl Variable {
    fn try_from_reader(
        file: Arc<NcFile>,
        dimension_overrides: Arc<HashMap<String, usize>>,
        var: &NcVariable,
    ) -> Result<Self> {
        Ok(Self {
            file,
            name: var.name().to_string(),
            dimensions: var
                .dimensions()
                .iter()
                .map(|dim| Dimension::try_from(dim, &dimension_overrides))
                .collect::<Result<_>>()?,
            dtype: DataType::from(var.dtype()),
            attributes: var
                .attributes()
                .iter()
                .map(Attribute::from_reader)
                .collect(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn dimensions(&self) -> &[Dimension] {
        &self.dimensions
    }

    pub fn shape(&self) -> Vec<usize> {
        self.dimensions.iter().map(Dimension::len).collect()
    }

    pub fn ndim(&self) -> usize {
        self.dimensions.len()
    }

    pub fn dtype(&self) -> &DataType {
        &self.dtype
    }

    pub fn attributes(&self) -> &[Attribute] {
        &self.attributes
    }

    pub fn attribute(&self, name: &str) -> Option<&Attribute> {
        self.attributes.iter().find(|attr| attr.name() == name)
    }

    /// Read this variable as promoted `f64` values with shape metadata.
    pub fn array_f64(&self) -> Result<DataArray> {
        let array = self.file.read_variable_as_f64(&self.name)?;
        Ok(DataArray::from_ndarray(array))
    }

    /// Read a hyperslab selection as promoted `f64` values with shape metadata.
    pub fn array_f64_slice(&self, selection: &NcSliceInfo) -> Result<DataArray> {
        let array = self
            .file
            .read_variable_slice_as_f64(&self.name, selection)?;
        Ok(DataArray::from_ndarray(array))
    }

    /// Read this variable as flat promoted `f64` values.
    pub fn values_f64(&self) -> Result<Vec<f64>> {
        Ok(self.array_f64()?.into_values())
    }

    /// Read the first WRF time record for rank >= 3; otherwise read all values.
    pub fn array_f64_first_record_or_all(&self) -> Result<DataArray> {
        if self.ndim() >= 3 {
            let selection = first_record_selection(self.ndim());
            let array = self
                .file
                .read_variable_slice_as_f64(&self.name, &selection)?;
            Ok(DataArray::from_ndarray(array))
        } else {
            self.array_f64()
        }
    }

    /// Read first WRF time record or all values as flat promoted `f64` values.
    pub fn values_f64_first_record_or_all(&self) -> Result<Vec<f64>> {
        Ok(self.array_f64_first_record_or_all()?.into_values())
    }
}

/// Supported public datatype names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataType {
    I8,
    Char,
    I16,
    I32,
    F32,
    F64,
    U8,
    U16,
    U32,
    I64,
    U64,
    String,
    Compound,
    Opaque,
    Array,
    VLen,
}

impl From<&NcType> for DataType {
    fn from(value: &NcType) -> Self {
        match value {
            NcType::Byte => Self::I8,
            NcType::Char => Self::Char,
            NcType::Short => Self::I16,
            NcType::Int => Self::I32,
            NcType::Float => Self::F32,
            NcType::Double => Self::F64,
            NcType::UByte => Self::U8,
            NcType::UShort => Self::U16,
            NcType::UInt => Self::U32,
            NcType::Int64 => Self::I64,
            NcType::UInt64 => Self::U64,
            NcType::String => Self::String,
            NcType::Compound { .. } => Self::Compound,
            NcType::Opaque { .. } => Self::Opaque,
            NcType::Array { .. } => Self::Array,
            NcType::VLen { .. } => Self::VLen,
        }
    }
}

/// Root-group/global attribute metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    name: String,
    value: AttributeValue,
}

impl Attribute {
    fn from_reader(attr: &netcdf_reader::NcAttribute) -> Self {
        Self {
            name: attr.name.clone(),
            value: AttributeValue::from(&attr.value),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &AttributeValue {
        &self.value
    }

    pub fn as_f64(&self) -> Option<f64> {
        self.value.as_f64()
    }

    pub fn as_string(&self) -> Option<&str> {
        self.value.as_string()
    }
}

/// Attribute values.
#[derive(Debug, Clone, PartialEq)]
pub enum AttributeValue {
    Bytes(Vec<i8>),
    Chars(String),
    Shorts(Vec<i16>),
    Ints(Vec<i32>),
    Floats(Vec<f32>),
    Doubles(Vec<f64>),
    UBytes(Vec<u8>),
    UShorts(Vec<u16>),
    UInts(Vec<u32>),
    Int64s(Vec<i64>),
    UInt64s(Vec<u64>),
    Strings(Vec<String>),
}

impl AttributeValue {
    pub fn as_string(&self) -> Option<&str> {
        match self {
            Self::Chars(value) => Some(value),
            Self::Strings(values) if values.len() == 1 => values.first().map(String::as_str),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Bytes(values) => values.first().map(|&value| value as f64),
            Self::Shorts(values) => values.first().map(|&value| value as f64),
            Self::Ints(values) => values.first().map(|&value| value as f64),
            Self::Floats(values) => values.first().map(|&value| value as f64),
            Self::Doubles(values) => values.first().copied(),
            Self::UBytes(values) => values.first().map(|&value| value as f64),
            Self::UShorts(values) => values.first().map(|&value| value as f64),
            Self::UInts(values) => values.first().map(|&value| value as f64),
            Self::Int64s(values) => values.first().map(|&value| value as f64),
            Self::UInt64s(values) => values.first().map(|&value| value as f64),
            Self::Chars(_) | Self::Strings(_) => None,
        }
    }

    pub fn as_f64_vec(&self) -> Option<Vec<f64>> {
        match self {
            Self::Bytes(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Shorts(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Ints(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Floats(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Doubles(values) => Some(values.clone()),
            Self::UBytes(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::UShorts(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::UInts(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Int64s(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::UInt64s(values) => Some(values.iter().map(|&value| value as f64).collect()),
            Self::Chars(_) | Self::Strings(_) => None,
        }
    }
}

impl From<&NcAttrValue> for AttributeValue {
    fn from(value: &NcAttrValue) -> Self {
        match value {
            NcAttrValue::Bytes(values) => Self::Bytes(values.clone()),
            NcAttrValue::Chars(value) => Self::Chars(value.clone()),
            NcAttrValue::Shorts(values) => Self::Shorts(values.clone()),
            NcAttrValue::Ints(values) => Self::Ints(values.clone()),
            NcAttrValue::Floats(values) => Self::Floats(values.clone()),
            NcAttrValue::Doubles(values) => Self::Doubles(values.clone()),
            NcAttrValue::UBytes(values) => Self::UBytes(values.clone()),
            NcAttrValue::UShorts(values) => Self::UShorts(values.clone()),
            NcAttrValue::UInts(values) => Self::UInts(values.clone()),
            NcAttrValue::Int64s(values) => Self::Int64s(values.clone()),
            NcAttrValue::UInt64s(values) => Self::UInt64s(values.clone()),
            NcAttrValue::Strings(values) => Self::Strings(values.clone()),
        }
    }
}

/// Dense numeric variable data.
#[derive(Debug, Clone, PartialEq)]
pub struct DataArray {
    shape: Vec<usize>,
    values: Vec<f64>,
}

impl DataArray {
    fn from_ndarray(array: ArrayD<f64>) -> Self {
        Self {
            shape: array.shape().to_vec(),
            values: array.iter().copied().collect(),
        }
    }

    fn from_shape_values(shape: Vec<usize>, values: Vec<f64>) -> Self {
        Self { shape, values }
    }

    pub fn shape(&self) -> &[usize] {
        &self.shape
    }

    pub fn ndim(&self) -> usize {
        self.shape.len()
    }

    pub fn values(&self) -> &[f64] {
        &self.values
    }

    pub fn into_values(self) -> Vec<f64> {
        self.values
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}

fn first_record_selection(ndim: usize) -> NcSliceInfo {
    let mut selections = Vec::with_capacity(ndim);
    selections.push(NcSliceInfoElem::Index(0));
    selections.extend((1..ndim).map(|_| NcSliceInfoElem::Slice {
        start: 0,
        end: u64::MAX,
        step: 1,
    }));
    NcSliceInfo { selections }
}

fn first_hdf5_record_selection(ndim: usize) -> H5SliceInfo {
    let mut selections = Vec::with_capacity(ndim);
    selections.push(H5SliceInfoElem::Index(0));
    selections.extend((1..ndim).map(|_| H5SliceInfoElem::Slice {
        start: 0,
        end: u64::MAX,
        step: 1,
    }));
    H5SliceInfo { selections }
}

fn read_hdf5_dataset_as_f64(
    dataset: &hdf5_reader::Dataset,
    selection: Option<&H5SliceInfo>,
) -> Result<DataArray> {
    read_hdf5_numeric::<f64>(dataset, selection)
        .or_else(|_| read_hdf5_numeric::<f32>(dataset, selection))
        .or_else(|_| read_hdf5_numeric::<i32>(dataset, selection))
        .or_else(|_| read_hdf5_numeric::<i16>(dataset, selection))
        .or_else(|_| read_hdf5_numeric::<u32>(dataset, selection))
        .or_else(|_| read_hdf5_numeric::<u16>(dataset, selection))
        .or_else(|_| read_hdf5_numeric::<u8>(dataset, selection))
        .or_else(|err| read_hdf5_numeric::<i8>(dataset, selection).map_err(|_| err))
        .map_err(|err| Error::Hdf5(err.to_string()))
}

fn read_hdf5_array<T: hdf5_reader::H5Type>(
    dataset: &hdf5_reader::Dataset,
    selection: Option<&H5SliceInfo>,
) -> std::result::Result<ArrayD<T>, hdf5_reader::error::Error> {
    match selection {
        Some(selection) => dataset.read_slice::<T>(selection),
        None => dataset.read_array::<T>(),
    }
}

fn read_hdf5_numeric<T>(
    dataset: &hdf5_reader::Dataset,
    selection: Option<&H5SliceInfo>,
) -> std::result::Result<DataArray, hdf5_reader::error::Error>
where
    T: hdf5_reader::H5Type + Copy + Into<f64>,
{
    let array = read_hdf5_array::<T>(dataset, selection)?;
    Ok(DataArray::from_shape_values(
        array.shape().to_vec(),
        array.iter().map(|value| (*value).into()).collect(),
    ))
}

fn infer_dimension_overrides(file: &NcFile) -> HashMap<String, usize> {
    let Ok(dimensions) = file.dimensions() else {
        return HashMap::new();
    };
    let zero_dims = dimensions
        .iter()
        .filter(|dim| dim.size == 0)
        .map(|dim| dim.name.clone())
        .collect::<Vec<_>>();
    if zero_dims.is_empty() {
        return HashMap::new();
    }

    let Ok(variables) = file.variables() else {
        return HashMap::new();
    };

    let mut overrides = HashMap::new();
    for dim_name in zero_dims {
        if let Some(coord_var) = variables.iter().find(|var| {
            var.name() == dim_name
                && !matches!(var.dtype(), NcType::Char | NcType::String)
                && var.dimensions().len() == 1
                && var.dimensions()[0].name == dim_name
        }) {
            if let Ok(array) = file.read_variable_as_f64(coord_var.name()) {
                if let Some(&len) = array.shape().first() {
                    if len > 0 {
                        overrides.insert(dim_name.clone(), len);
                        continue;
                    }
                }
            }
        }

        let mut candidates = variables
            .iter()
            .filter_map(|var| {
                let axis = var
                    .dimensions()
                    .iter()
                    .position(|dim| dim.name == dim_name)?;
                if var.name() == dim_name || matches!(var.dtype(), NcType::Char | NcType::String) {
                    return None;
                }
                let other_elements = var
                    .dimensions()
                    .iter()
                    .enumerate()
                    .filter(|(idx, _)| *idx != axis)
                    .map(|(_, dim)| dim.size.max(1))
                    .product::<u64>();
                Some((other_elements, axis, var.name().to_string()))
            })
            .collect::<Vec<_>>();

        candidates.sort_by_key(|(other_elements, _, _)| *other_elements);

        for (_, axis, variable_name) in candidates {
            let Ok(array) = file.read_variable_as_f64(&variable_name) else {
                continue;
            };
            if let Some(&len) = array.shape().get(axis) {
                if len > 0 {
                    overrides.insert(dim_name.clone(), len);
                    break;
                }
            }
        }
    }

    overrides
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_netcdf_signatures() {
        assert!(looks_like_hdf5(&HDF5_SIGNATURE));
        assert!(looks_like_netcdf(&HDF5_SIGNATURE));
        assert!(looks_like_netcdf(b"CDF\x01extra"));
        assert!(looks_like_netcdf(b"CDF\x02extra"));
        assert!(looks_like_netcdf(b"CDF\x05extra"));
        assert!(!looks_like_netcdf(b"CDF\x09extra"));
        assert!(!looks_like_netcdf(b"NOPE"));
    }

    #[test]
    fn first_record_selection_drops_leading_axis() {
        let selection = first_record_selection(4);
        assert_eq!(selection.selections.len(), 4);
        assert!(matches!(selection.selections[0], NcSliceInfoElem::Index(0)));
        for elem in &selection.selections[1..] {
            assert!(matches!(
                elem,
                NcSliceInfoElem::Slice {
                    start: 0,
                    end: u64::MAX,
                    step: 1
                }
            ));
        }
    }

    #[test]
    fn attribute_value_promotes_numeric_scalars() {
        assert_eq!(AttributeValue::Ints(vec![42]).as_f64(), Some(42.0));
        assert_eq!(AttributeValue::Floats(vec![1.5]).as_f64(), Some(1.5));
        assert_eq!(
            AttributeValue::Chars("Lambert".to_string()).as_string(),
            Some("Lambert")
        );
    }
}

use crate::error::{ErrorKind, FormulaError, FormulaResult};
use crate::units::Unit;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

/// Semantic axes prevent accidental operations on arrays that merely share a length.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Axis {
    Component,
    Time,
    Z,
    Y,
    X,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridLocation {
    Mass,
    XFace,
    YFace,
    ZFace,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VectorBasis {
    GridProjected,
    EarthRelative,
    Unknown,
}

/// This records exactly what the local calculus operators mean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridConvention {
    /// WRF mass-point physical derivative: computational differences are
    /// multiplied by MAPFAC_M / DX or MAPFAC_M / DY. This is not the native
    /// C-grid AVO/UH stencil, which uses stagger-specific map factors.
    WrfMassPointProjected,
    Cartesian,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BoundaryPolicy {
    OneSidedSecondOrder,
    Missing,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissingPolicy {
    Propagate,
    Error,
    /// Only supported by vertical reductions. An all-missing column remains missing.
    IgnoreInReductions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NonFinitePolicy {
    Propagate,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shape {
    pub dims: Vec<usize>,
    pub axes: Vec<Axis>,
}

impl Shape {
    pub fn new(dims: Vec<usize>, axes: Vec<Axis>) -> FormulaResult<Self> {
        if dims.len() != axes.len() {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "{} dimensions require {} axis labels, got {}",
                    dims.len(),
                    dims.len(),
                    axes.len()
                ),
            ));
        }
        if dims.iter().any(|size| *size == 0) {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                "zero-length axes are not supported",
            ));
        }
        let _ = checked_element_count(&dims)?;
        Ok(Self { dims, axes })
    }

    pub fn element_count(&self) -> FormulaResult<usize> {
        checked_element_count(&self.dims)
    }

    pub fn horizontal(nx: usize, ny: usize) -> FormulaResult<Self> {
        Self::new(vec![ny, nx], vec![Axis::Y, Axis::X])
    }

    pub fn volume(nx: usize, ny: usize, nz: usize) -> FormulaResult<Self> {
        Self::new(vec![nz, ny, nx], vec![Axis::Z, Axis::Y, Axis::X])
    }

    pub fn without_z(&self) -> FormulaResult<Self> {
        if self.axes.as_slice() != [Axis::Z, Axis::Y, Axis::X] {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                "vertical reduction requires axes [z, y, x]",
            ));
        }
        Self::new(vec![self.dims[1], self.dims[2]], vec![Axis::Y, Axis::X])
    }
}

pub(crate) fn checked_element_count(dims: &[usize]) -> FormulaResult<usize> {
    dims.iter().try_fold(1_usize, |count, size| {
        count.checked_mul(*size).ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "array shape element count overflow")
        })
    })
}

#[derive(Debug, Clone)]
pub(crate) struct Field {
    pub data: Arc<[f64]>,
    pub shape: Shape,
    pub unit: Unit,
    pub location: GridLocation,
    pub description: String,
}

impl Field {
    pub fn validate(&self, max_elements: usize) -> FormulaResult<()> {
        let expected = self.shape.element_count()?;
        if expected != self.data.len() {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "field shape {:?} requires {expected} elements, got {}",
                    self.shape.dims,
                    self.data.len()
                ),
            ));
        }
        if expected > max_elements {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("field has {expected} elements; limit is {max_elements}"),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Scalar {
    pub value: f64,
    pub unit: Unit,
}

#[derive(Debug, Clone)]
pub(crate) struct VectorField {
    pub components: Vec<Field>,
    pub basis: VectorBasis,
}

impl VectorField {
    pub fn validate(&self, max_elements: usize) -> FormulaResult<()> {
        let first = self.components.first().ok_or_else(|| {
            FormulaError::new(ErrorKind::Shape, "a vector requires at least one component")
        })?;
        first.validate(max_elements)?;
        if first.unit.logarithmic || first.unit.is_absolute_temperature() {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "vectors cannot contain logarithmic or affine absolute-temperature components",
            ));
        }
        for component in self.components.iter().skip(1) {
            component.validate(max_elements)?;
            if component.shape != first.shape {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    "vector components must have identical labeled shapes",
                ));
            }
            if !component.unit.compatible(&first.unit)
                || component.unit.temperature_kind != first.unit.temperature_kind
                || component.unit.logarithmic != first.unit.logarithmic
            {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    "vector components must have compatible linear units",
                ));
            }
            if component.location != first.location {
                return Err(FormulaError::new(
                    ErrorKind::Grid,
                    "vector components must share a grid location",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Value {
    Scalar(Scalar),
    Field(Field),
    Vector(VectorField),
    Text(String),
}

impl Value {
    pub fn unit(&self) -> FormulaResult<&Unit> {
        match self {
            Self::Scalar(value) => Ok(&value.unit),
            Self::Field(value) => Ok(&value.unit),
            Self::Vector(value) => value
                .components
                .first()
                .map(|field| &field.unit)
                .ok_or_else(|| FormulaError::new(ErrorKind::Internal, "vector has no components")),
            Self::Text(_) => Err(FormulaError::new(
                ErrorKind::Unit,
                "text has no physical unit",
            )),
        }
    }
}

/// Hard limits are checked before parsing and before every allocation/work loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub max_source_bytes: usize,
    pub max_tokens: usize,
    pub max_ast_nodes: usize,
    pub max_ast_depth: usize,
    pub max_identifier_bytes: usize,
    pub max_function_arity: usize,
    pub max_assignments: usize,
    pub max_dependencies: usize,
    pub max_output_elements: usize,
    /// Maximum bytes in any one contiguous numeric allocation.
    pub max_working_bytes: usize,
    pub max_total_allocated_bytes: u64,
    pub max_operations: u64,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_tokens: 16_384,
            max_ast_nodes: 16_384,
            max_ast_depth: 128,
            max_identifier_bytes: 128,
            max_function_arity: 16,
            max_assignments: 1024,
            max_dependencies: 1024,
            max_output_elements: 128 * 1024 * 1024,
            max_working_bytes: 1024 * 1024 * 1024,
            max_total_allocated_bytes: 4 * 1024 * 1024 * 1024,
            max_operations: 4_000_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParameterSpec {
    pub name: String,
    pub units: String,
    pub default: f64,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    #[serde(default)]
    pub description: String,
}

pub type ParameterValues = BTreeMap<String, f64>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompileOptions {
    #[serde(default)]
    pub parameters: Vec<ParameterSpec>,
    #[serde(default)]
    pub limits: ResourceLimits,
}

impl Default for CompileOptions {
    fn default() -> Self {
        Self {
            parameters: Vec::new(),
            limits: ResourceLimits::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EvaluationOptions {
    pub boundary_policy: BoundaryPolicy,
    pub missing_policy: MissingPolicy,
    pub non_finite_policy: NonFinitePolicy,
    /// Unit declarations for raw WRF variables whose files do not carry units.
    #[serde(default, deserialize_with = "deserialize_unit_overrides")]
    pub variable_unit_overrides: BTreeMap<String, String>,
}

fn deserialize_unit_overrides<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct UnitOverrideVisitor;

    impl<'de> serde::de::Visitor<'de> for UnitOverrideVisitor {
        type Value = BTreeMap<String, String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a map of unique field names to unit strings")
        }

        fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut output = BTreeMap::new();
            let mut canonical = BTreeSet::new();
            while let Some((name, units)) = access.next_entry::<String, String>()? {
                if !canonical.insert(name.to_ascii_lowercase()) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate ASCII-case-insensitive unit override key '{name}'"
                    )));
                }
                output.insert(name, units);
            }
            Ok(output)
        }
    }

    deserializer.deserialize_map(UnitOverrideVisitor)
}

impl Default for EvaluationOptions {
    fn default() -> Self {
        Self {
            boundary_policy: BoundaryPolicy::OneSidedSecondOrder,
            missing_policy: MissingPolicy::Propagate,
            non_finite_policy: NonFinitePolicy::Propagate,
            variable_unit_overrides: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Requirement {
    Field { name: String },
    MassMapFactor,
    PhysicalHeight { datum: HeightDatum },
    AdjacentTimes,
    GridProjectedVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeightDatum {
    Msl,
    Agl,
    ResolverDefault,
    ExplicitField,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPlan {
    pub canonical_source: String,
    pub dependencies: Vec<String>,
    pub functions: Vec<String>,
    pub assignments: Vec<String>,
    pub requirements: Vec<Requirement>,
    pub ast_nodes: usize,
    pub ast_depth: usize,
    pub recipe_requirements: Option<crate::recipe::RecipeRequirements>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedInputProvenance {
    pub requested_name: String,
    pub resolved_name: String,
    pub time_offset: isize,
    pub shape: Vec<usize>,
    pub axes: Vec<Axis>,
    pub source_units: Option<String>,
    pub effective_units: String,
    pub unit_override_used: Option<String>,
    pub grid_location: GridLocation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaProvenance {
    pub engine_version: String,
    pub canonical_source: String,
    pub source_fingerprint: String,
    pub base_time_index: Option<usize>,
    pub valid_time: Option<String>,
    pub input_identity: Option<String>,
    pub recipe_name: Option<String>,
    pub recipe_version: Option<String>,
    pub recipe_references: Vec<String>,
    pub recipe_requirements: Option<crate::recipe::RecipeRequirements>,
    pub variable_unit_overrides: BTreeMap<String, String>,
    pub inputs: Vec<ResolvedInputProvenance>,
    pub parameters: ParameterValues,
    pub boundary_policy: BoundaryPolicy,
    pub missing_policy: MissingPolicy,
    pub non_finite_policy: NonFinitePolicy,
    pub grid_convention: Option<GridConvention>,
    pub vertical_height_datums: Vec<HeightDatum>,
    pub warnings: Vec<String>,
}

/// Contiguous row-major output. Vector output prepends a component axis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaOutput {
    pub data: Vec<f64>,
    pub shape: Vec<usize>,
    pub axes: Vec<Axis>,
    pub units: String,
    pub description: String,
    pub provenance: FormulaProvenance,
}

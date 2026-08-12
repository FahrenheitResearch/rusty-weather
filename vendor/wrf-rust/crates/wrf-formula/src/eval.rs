use crate::ast::{BinaryOp, Expr, ExprKind, UnaryOp};
use crate::compile::CompiledFormula;
use crate::error::{ErrorKind, FormulaError, FormulaResult, Span};
use crate::model::{
    Axis, BoundaryPolicy, EvaluationOptions, Field, FormulaOutput, FormulaProvenance,
    GridConvention, GridLocation, HeightDatum, MissingPolicy, NonFinitePolicy, ParameterValues,
    ResolvedInputProvenance, Scalar, Shape, Value, VectorBasis, VectorField,
};
use crate::resolver::{FieldRequest, FieldResolver, GridMetadata, ResolvedField};
use crate::units::{Dimension, TemperatureKind, Unit};
use std::collections::{BTreeMap, BTreeSet, HashMap};

pub(crate) fn evaluate<R: FieldResolver>(
    formula: &CompiledFormula,
    resolver: &R,
    parameter_values: &ParameterValues,
    options: &EvaluationOptions,
) -> FormulaResult<FormulaOutput> {
    if options.variable_unit_overrides.len() > formula.options.limits.max_dependencies {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "{} unit overrides exceed limit {}",
                options.variable_unit_overrides.len(),
                formula.options.limits.max_dependencies
            ),
        ));
    }
    let mut override_names = BTreeSet::new();
    for (name, units) in &options.variable_unit_overrides {
        if name.len() > formula.options.limits.max_identifier_bytes || units.len() > 256 {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!("unit override '{name}' exceeds name/unit string limits"),
            ));
        }
        if !override_names.insert(name.to_ascii_lowercase()) {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                format!("variable unit overrides contain case-insensitive duplicate '{name}'"),
            ));
        }
    }
    let (parameters, parameter_provenance) = prepare_parameters(formula, parameter_values)?;
    let assignments = formula
        .program
        .assignments
        .iter()
        .map(|assignment| (assignment.name.to_ascii_lowercase(), &assignment.value))
        .collect();
    let mut evaluator = Evaluator {
        formula,
        resolver,
        options,
        parameters,
        parameter_provenance,
        assignments,
        value_cache: HashMap::new(),
        input_provenance: Vec::new(),
        warnings: Vec::new(),
        used_unit_overrides: BTreeSet::new(),
        used_grid_convention: None,
        used_height_datums: BTreeSet::new(),
        meter: Meter::new(formula),
        grid_cache: HashMap::new(),
        map_factor_cache: HashMap::new(),
        default_height_cache: HashMap::new(),
    };
    evaluator.preflight_recipe_requirements()?;
    let value = evaluator.eval_expr(&formula.program.output, 0)?;
    evaluator.finish(value)
}

fn binary_result_unit(op: BinaryOp, left: &Unit, right: &Unit) -> FormulaResult<Unit> {
    match op {
        BinaryOp::Add | BinaryOp::Sub => add_sub_unit(op, left, right),
        BinaryOp::Mul => left.multiply(right),
        BinaryOp::Div => left.divide(right),
        _ => Err(FormulaError::new(
            ErrorKind::Internal,
            "non-arithmetic operator requested an arithmetic unit",
        )),
    }
}

fn add_sub_unit(op: BinaryOp, left: &Unit, right: &Unit) -> FormulaResult<Unit> {
    if !left.compatible(right) || left.logarithmic || right.logarithmic {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("cannot add/subtract {} and {}", left, right),
        ));
    }
    use TemperatureKind::{Absolute, Difference, None};
    match (op, left.temperature_kind, right.temperature_kind) {
        (BinaryOp::Add, Absolute, Absolute) => Err(FormulaError::new(
            ErrorKind::Unit,
            "adding two absolute temperatures is undefined; add a temperature difference",
        )),
        (BinaryOp::Add, Absolute, Difference) => Ok(left.clone()),
        (BinaryOp::Add, Difference, Absolute) => Ok(right.clone()),
        (BinaryOp::Add, Difference, Difference) => Ok(left.clone()),
        (BinaryOp::Sub, Absolute, Absolute) => Ok(Unit::temperature_difference("delta_K", 1.0)),
        (BinaryOp::Sub, Absolute, Difference) => Ok(left.clone()),
        (BinaryOp::Sub, Difference, Absolute) => Err(FormulaError::new(
            ErrorKind::Unit,
            "subtracting an absolute temperature from a temperature difference is undefined",
        )),
        (BinaryOp::Sub, Difference, Difference) => Ok(left.clone()),
        (_, None, None) => Ok(left.clone()),
        _ => Err(FormulaError::new(
            ErrorKind::Unit,
            "temperature arithmetic requires explicit absolute/difference-compatible units",
        )),
    }
}

fn apply_arithmetic(op: BinaryOp, left: f64, right: f64) -> f64 {
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div => left / right,
        _ => f64::NAN,
    }
}

fn ensure_same_field_geometry(left: &Field, right: &Field) -> FormulaResult<()> {
    if left.shape != right.shape {
        return Err(FormulaError::new(
            ErrorKind::Shape,
            format!(
                "field shapes/axes differ: {:?}/{:?} versus {:?}/{:?}",
                left.shape.dims, left.shape.axes, right.shape.dims, right.shape.axes
            ),
        ));
    }
    if left.location != right.location {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "field grid locations differ",
        ));
    }
    Ok(())
}

fn ensure_comparable_units(left: &Unit, right: &Unit) -> FormulaResult<()> {
    if !left.compatible(right)
        || left.temperature_kind != right.temperature_kind
        || left.logarithmic != right.logarithmic
    {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("units {} and {} are not directly comparable", left, right),
        ));
    }
    Ok(())
}

fn common_selection_unit(left: &Unit, right: &Unit) -> FormulaResult<Unit> {
    ensure_comparable_units(left, right)?;
    Ok(left.clone())
}

fn ensure_conversion_compatible(source: &Unit, target: &Unit) -> FormulaResult<()> {
    if !source.compatible(target)
        || source.temperature_kind != target.temperature_kind
        || source.logarithmic != target.logarithmic
    {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("cannot convert {} to {}", source, target),
        ));
    }
    Ok(())
}

fn set_value_unit(value: &mut Value, unit: Unit) -> FormulaResult<()> {
    match value {
        Value::Scalar(value) => value.unit = unit,
        Value::Field(value) => value.unit = unit,
        Value::Vector(value) => {
            for component in &mut value.components {
                component.unit = unit.clone();
            }
        }
        Value::Text(_) => {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "text cannot carry a physical unit",
            ));
        }
    }
    Ok(())
}

fn require_boolean_unit(unit: &Unit) -> FormulaResult<()> {
    require_plain_dimensionless(unit)
}

fn require_plain_dimensionless(unit: &Unit) -> FormulaResult<()> {
    if !unit.is_dimensionless() || unit.logarithmic {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("operation requires a plain dimensionless value, got {unit}"),
        ));
    }
    Ok(())
}

fn truthy(value: f64) -> bool {
    value != 0.0
}

fn reject_vector(value: &Value, operation: &str) -> FormulaResult<()> {
    if matches!(value, Value::Vector(_)) {
        Err(FormulaError::new(
            ErrorKind::Shape,
            format!(
                "{operation} does not preserve physical vector semantics; select a component or use a vector-specific function"
            ),
        ))
    } else {
        Ok(())
    }
}

fn expect_field(value: Value, context: &str) -> FormulaResult<Field> {
    match value {
        Value::Field(field) => Ok(field),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            format!("{context} requires a scalar field"),
        )),
    }
}

fn expect_vector(value: Value, context: &str) -> FormulaResult<VectorField> {
    match value {
        Value::Vector(vector) => Ok(vector),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            format!("{context} requires a vector field"),
        )),
    }
}

fn expect_scalar(value: Value, context: &str) -> FormulaResult<Scalar> {
    match value {
        Value::Scalar(scalar) => Ok(scalar),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            format!("{context} requires a scalar"),
        )),
    }
}

fn expect_text(value: Value, context: &str) -> FormulaResult<String> {
    match value {
        Value::Text(text) => Ok(text),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            format!("{context} requires a string literal"),
        )),
    }
}

fn expect_dimensionless_integer(value: Scalar, context: &str) -> FormulaResult<usize> {
    require_plain_dimensionless(&value.unit)?;
    if !value.value.is_finite()
        || value.value < 0.0
        || value.value.fract() != 0.0
        || value.value > usize::MAX as f64
    {
        return Err(FormulaError::new(
            ErrorKind::Domain,
            format!("{context} must be a non-negative integer"),
        ));
    }
    Ok(value.value as usize)
}

fn value_field(value: &Value) -> Option<&Field> {
    match value {
        Value::Field(field) => Some(field),
        _ => None,
    }
}

fn value_at(value: &Value, index: usize) -> FormulaResult<f64> {
    match value {
        Value::Scalar(scalar) => Ok(scalar.value),
        Value::Field(field) => field.data.get(index).copied().ok_or_else(|| {
            FormulaError::new(ErrorKind::Internal, "field index outside validated shape")
        }),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            "operation requires scalars or scalar fields",
        )),
    }
}

fn require_mass_scalar_grid(field: &Field) -> FormulaResult<()> {
    if field.location != GridLocation::Mass
        || !matches!(
            field.shape.axes.as_slice(),
            [Axis::Y, Axis::X] | [Axis::Z, Axis::Y, Axis::X]
        )
    {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "local calculus requires a scalar mass-point field with [y,x] or [z,y,x] axes; staggered data must be explicitly destaggered",
        ));
    }
    Ok(())
}

fn require_mass_volume(field: &Field) -> FormulaResult<()> {
    if field.location != GridLocation::Mass
        || field.shape.axes.as_slice() != [Axis::Z, Axis::Y, Axis::X]
    {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "operation requires a mass-point [z,y,x] volume",
        ));
    }
    Ok(())
}

fn require_physical_height(field: &Field) -> FormulaResult<()> {
    require_mass_volume(field)?;
    if field.unit.dimension != Dimension::LENGTH || field.unit.logarithmic {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            "vertical coordinate must have physical length units",
        ));
    }
    Ok(())
}

fn require_length_scalar(value: &Scalar, context: &str) -> FormulaResult<()> {
    if value.unit.dimension != Dimension::LENGTH || value.unit.logarithmic {
        return Err(FormulaError::new(
            ErrorKind::Unit,
            format!("{context} requires length units"),
        ));
    }
    Ok(())
}

fn require_grid_vector(vector: &VectorField) -> FormulaResult<()> {
    vector.validate(usize::MAX)?;
    if vector.basis != VectorBasis::GridProjected {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "div/curl require grid-projected components; earth-relative winds need an explicit projection rotation first",
        ));
    }
    for component in &vector.components {
        require_mass_scalar_grid(component)?;
    }
    Ok(())
}

fn field_grid_dimensions(field: &Field) -> FormulaResult<(usize, usize, usize)> {
    match (field.shape.axes.as_slice(), field.shape.dims.as_slice()) {
        ([Axis::Y, Axis::X], [ny, nx]) => Ok((1, *ny, *nx)),
        ([Axis::Z, Axis::Y, Axis::X], [nz, ny, nx]) => Ok((*nz, *ny, *nx)),
        _ => Err(FormulaError::new(
            ErrorKind::Shape,
            "expected [y,x] or [z,y,x] field",
        )),
    }
}

fn finite_difference_uniform(
    data: &[f64],
    k: usize,
    y: usize,
    x: usize,
    _nz: usize,
    ny: usize,
    nx: usize,
    axis: Axis,
    spacing: f64,
) -> FormulaResult<f64> {
    let coordinate = if axis == Axis::X { x } else { y };
    let size = if axis == Axis::X { nx } else { ny };
    if size < 3 {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "one_sided_second_order/centered derivative requires at least three points",
        ));
    }
    let at = |position: usize| -> FormulaResult<f64> {
        let (sample_y, sample_x) = if axis == Axis::X {
            (y, position)
        } else {
            (position, x)
        };
        data.get((k * ny + sample_y) * nx + sample_x)
            .copied()
            .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Internal,
                    "finite-difference index outside validated shape",
                )
            })
    };
    if coordinate == 0 {
        Ok((-3.0 * at(0)? + 4.0 * at(1)? - at(2)?) / (2.0 * spacing))
    } else if coordinate + 1 == size {
        Ok((3.0 * at(size - 1)? - 4.0 * at(size - 2)? + at(size - 3)?) / (2.0 * spacing))
    } else {
        Ok((at(coordinate + 1)? - at(coordinate - 1)?) / (2.0 * spacing))
    }
}

fn second_difference_2d(
    data: &[f64],
    y: usize,
    x: usize,
    ny: usize,
    nx: usize,
    axis: Axis,
    spacing: f64,
) -> FormulaResult<f64> {
    let coordinate = if axis == Axis::X { x } else { y };
    let size = if axis == Axis::X { nx } else { ny };
    let at = |position: usize| -> FormulaResult<f64> {
        let (sample_y, sample_x) = if axis == Axis::X {
            (y, position)
        } else {
            (position, x)
        };
        data.get(sample_y * nx + sample_x).copied().ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Internal,
                "second-difference index outside validated shape",
            )
        })
    };
    let numerator = if coordinate == 0 {
        2.0 * at(0)? - 5.0 * at(1)? + 4.0 * at(2)? - at(3)?
    } else if coordinate + 1 == size {
        2.0 * at(size - 1)? - 5.0 * at(size - 2)? + 4.0 * at(size - 3)? - at(size - 4)?
    } else {
        at(coordinate + 1)? - 2.0 * at(coordinate)? + at(coordinate - 1)?
    };
    Ok(numerator / (spacing * spacing))
}

fn validate_height_column(
    height: &Field,
    y: usize,
    x: usize,
    nz: usize,
    ny: usize,
    nx: usize,
) -> FormulaResult<()> {
    let mut previous = None;
    for k in 0..nz {
        let value = *height.data.get((k * ny + y) * nx + x).ok_or_else(|| {
            FormulaError::new(ErrorKind::Internal, "height index outside validated shape")
        })?;
        if !value.is_finite() {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "physical height contains a non-finite value",
            ));
        }
        if previous.is_some_and(|previous| value <= previous) {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "physical height must increase strictly within every column",
            ));
        }
        previous = Some(value);
    }
    Ok(())
}

fn derivative_nonuniform_column(
    field: &Field,
    height: &Field,
    k: usize,
    y: usize,
    x: usize,
    nz: usize,
    ny: usize,
    nx: usize,
) -> FormulaResult<f64> {
    if nz < 3 {
        return Err(FormulaError::new(
            ErrorKind::Grid,
            "one_sided_second_order vertical derivative requires at least three levels",
        ));
    }
    let levels = if k == 0 {
        [0, 1, 2]
    } else if k + 1 == nz {
        [nz - 3, nz - 2, nz - 1]
    } else {
        [k - 1, k, k + 1]
    };
    let sample = |level: usize, source: &Field| -> FormulaResult<f64> {
        source
            .data
            .get((level * ny + y) * nx + x)
            .copied()
            .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Internal,
                    "vertical derivative index outside validated shape",
                )
            })
    };
    let x0 = sample(levels[0], height)?;
    let x1 = sample(levels[1], height)?;
    let x2 = sample(levels[2], height)?;
    let target = sample(k, height)?;
    let f0 = sample(levels[0], field)?;
    let f1 = sample(levels[1], field)?;
    let f2 = sample(levels[2], field)?;
    let w0 = (2.0 * target - x1 - x2) / ((x0 - x1) * (x0 - x2));
    let w1 = (2.0 * target - x0 - x2) / ((x1 - x0) * (x1 - x2));
    let w2 = (2.0 * target - x0 - x1) / ((x2 - x0) * (x2 - x1));
    Ok(w0 * f0 + w1 * f1 + w2 * f2)
}

fn linear_interpolate(x0: f64, x1: f64, y0: f64, y1: f64, target: f64) -> f64 {
    y0 + (target - x0) / (x1 - x0) * (y1 - y0)
}

fn available_time<R: FieldResolver>(resolver: &R, offset: isize) -> FormulaResult<Option<f64>> {
    match resolver.time_seconds(offset) {
        Ok(value) if value.is_finite() => Ok(Some(value)),
        Ok(_) => Err(FormulaError::new(
            ErrorKind::Time,
            "resolver returned a non-finite valid time",
        )),
        Err(error)
            if error.kind == ErrorKind::Time
                && (error.message.contains("outside")
                    || error.message.contains("before the first")) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn three_point_derivative_weights(times: [f64; 3], target: f64) -> FormulaResult<[f64; 3]> {
    if times.iter().any(|time| !time.is_finite()) || !target.is_finite() {
        return Err(FormulaError::new(
            ErrorKind::Time,
            "temporal stencil times must be finite",
        ));
    }
    if !(times[0] < times[1] && times[1] < times[2]) {
        return Err(FormulaError::new(
            ErrorKind::Time,
            format!("temporal stencil must be strictly increasing, got {times:?}"),
        ));
    }
    let x0 = times[0];
    let x1 = times[1];
    let x2 = times[2];
    let weights = [
        (2.0 * target - x1 - x2) / ((x0 - x1) * (x0 - x2)),
        (2.0 * target - x0 - x2) / ((x1 - x0) * (x1 - x2)),
        (2.0 * target - x0 - x1) / ((x2 - x0) * (x2 - x1)),
    ];
    if weights.iter().any(|weight| !weight.is_finite()) {
        return Err(FormulaError::new(
            ErrorKind::Time,
            "temporal derivative weights overflowed",
        ));
    }
    Ok(weights)
}

fn source_fingerprint(source: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in source.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn effective_parameter_values(
    formula: &CompiledFormula,
    values: &BTreeMap<String, Scalar>,
) -> ParameterValues {
    let mut output = ParameterValues::new();
    for spec in &formula.options.parameters {
        if let Some(value) = values.get(&spec.name.to_ascii_lowercase()) {
            output.insert(spec.name.clone(), value.unit.from_si(value.value));
        }
    }
    output
}

fn prepare_parameters(
    formula: &CompiledFormula,
    supplied: &ParameterValues,
) -> FormulaResult<(BTreeMap<String, Scalar>, ParameterValues)> {
    let mut supplied_names = BTreeSet::new();
    for name in supplied.keys() {
        if !supplied_names.insert(name.to_ascii_lowercase()) {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!("parameter overrides contain case-insensitive duplicate '{name}'"),
            ));
        }
        if !formula
            .options
            .parameters
            .iter()
            .any(|spec| spec.name.eq_ignore_ascii_case(name))
        {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!("unknown parameter override '{name}'"),
            ));
        }
    }
    let mut output = BTreeMap::new();
    let mut provenance = ParameterValues::new();
    for spec in &formula.options.parameters {
        let raw = supplied
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(&spec.name))
            .map(|(_, value)| *value)
            .unwrap_or(spec.default);
        if !raw.is_finite() {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!("parameter '{}' must be finite", spec.name),
            ));
        }
        if spec.minimum.is_some_and(|minimum| raw < minimum)
            || spec.maximum.is_some_and(|maximum| raw > maximum)
        {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "parameter '{}' value {raw} lies outside declared bounds",
                    spec.name
                ),
            ));
        }
        let unit = crate::parse_unit(&spec.units)?;
        let value_si = unit.to_si(raw);
        if !value_si.is_finite() {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "parameter '{}' overflows/nonfinite after conversion to SI",
                    spec.name
                ),
            ));
        }
        output.insert(
            spec.name.to_ascii_lowercase(),
            Scalar {
                value: value_si,
                unit,
            },
        );
        provenance.insert(spec.name.clone(), raw);
    }
    Ok((output, provenance))
}

struct Meter {
    operations: u64,
    allocated_bytes: u64,
    max_operations: u64,
    max_working_bytes: u64,
    max_total_allocated_bytes: u64,
    max_elements: usize,
}

impl Meter {
    fn new(formula: &CompiledFormula) -> Self {
        let limits = &formula.options.limits;
        Self {
            operations: 0,
            allocated_bytes: 0,
            max_operations: limits.max_operations,
            max_working_bytes: limits.max_working_bytes as u64,
            max_total_allocated_bytes: limits.max_total_allocated_bytes,
            max_elements: limits.max_output_elements,
        }
    }

    fn work(&mut self, count: usize, span: Option<Span>) -> FormulaResult<()> {
        let count = u64::try_from(count)
            .map_err(|_| FormulaError::new(ErrorKind::Limit, "operation count overflow"))?;
        self.operations = self
            .operations
            .checked_add(count)
            .ok_or_else(|| FormulaError::new(ErrorKind::Limit, "operation count overflow"))?;
        if self.operations > self.max_operations {
            let error = FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "evaluation requires more than {} element operations",
                    self.max_operations
                ),
            );
            return Err(if let Some(span) = span {
                error.at(span)
            } else {
                error
            });
        }
        Ok(())
    }

    fn allocate(&mut self, elements: usize, span: Option<Span>) -> FormulaResult<()> {
        if elements > self.max_elements {
            let error = FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "array has {elements} elements; limit is {}",
                    self.max_elements
                ),
            );
            return Err(if let Some(span) = span {
                error.at(span)
            } else {
                error
            });
        }
        let bytes = elements
            .checked_mul(std::mem::size_of::<f64>())
            .ok_or_else(|| FormulaError::new(ErrorKind::Limit, "allocation byte count overflow"))?;
        let bytes = u64::try_from(bytes)
            .map_err(|_| FormulaError::new(ErrorKind::Limit, "allocation byte count overflow"))?;
        self.allocated_bytes = self
            .allocated_bytes
            .checked_add(bytes)
            .ok_or_else(|| FormulaError::new(ErrorKind::Limit, "cumulative allocation overflow"))?;
        if bytes > self.max_working_bytes || self.allocated_bytes > self.max_total_allocated_bytes {
            let error = FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "evaluation allocation budget exceeded (single allocation {bytes} bytes, {} cumulative bytes; limits {}/{})",
                    self.allocated_bytes, self.max_working_bytes, self.max_total_allocated_bytes
                ),
            );
            return Err(if let Some(span) = span {
                error.at(span)
            } else {
                error
            });
        }
        Ok(())
    }
}

struct Evaluator<'a, R: FieldResolver> {
    formula: &'a CompiledFormula,
    resolver: &'a R,
    options: &'a EvaluationOptions,
    parameters: BTreeMap<String, Scalar>,
    parameter_provenance: ParameterValues,
    assignments: BTreeMap<String, &'a Expr>,
    value_cache: HashMap<(String, isize), Value>,
    input_provenance: Vec<ResolvedInputProvenance>,
    warnings: Vec<String>,
    used_unit_overrides: BTreeSet<String>,
    used_grid_convention: Option<GridConvention>,
    used_height_datums: BTreeSet<HeightDatum>,
    meter: Meter,
    grid_cache: HashMap<isize, GridMetadata>,
    map_factor_cache: HashMap<isize, Field>,
    default_height_cache: HashMap<isize, Field>,
}

impl<R: FieldResolver> Evaluator<'_, R> {
    fn preflight_recipe_requirements(&mut self) -> FormulaResult<()> {
        let Some(requirements) = self.formula.recipe_requirements.clone() else {
            return Ok(());
        };
        for name in &requirements.fields {
            let field = self
                .resolve_field(name, 0, self.formula.program.output.span)
                .map_err(|error| {
                    FormulaError::new(
                        ErrorKind::Resolver,
                        format!(
                            "recipe-required field '{name}' is unavailable or invalid: {error}"
                        ),
                    )
                })?;
            let lower = name.to_ascii_lowercase();
            if self
                .formula
                .plan()
                .dependencies
                .iter()
                .any(|dependency| dependency.eq_ignore_ascii_case(name))
                && !self.assignments.contains_key(&lower)
                && !self.parameters.contains_key(&lower)
                && !matches!(lower.as_str(), "pi" | "e" | "true" | "false")
            {
                self.value_cache.insert((lower, 0), Value::Field(field));
            }
        }
        if let Some(maximum_cadence) = requirements.maximum_cadence_seconds {
            let current = self.resolver.time_seconds(0)?;
            let mut intervals = Vec::new();
            if let Some(previous) = available_time(self.resolver, -1)? {
                if previous >= current {
                    return Err(FormulaError::new(
                        ErrorKind::Time,
                        "previous valid time must precede current time",
                    ));
                }
                intervals.push(current - previous);
            }
            if let Some(next) = available_time(self.resolver, 1)? {
                if next <= current {
                    return Err(FormulaError::new(
                        ErrorKind::Time,
                        "next valid time must follow current time",
                    ));
                }
                intervals.push(next - current);
            }
            if intervals.is_empty() {
                return Err(FormulaError::new(
                    ErrorKind::Time,
                    "recipe cadence requirement needs an adjacent output",
                ));
            }
            let cadence = intervals.into_iter().fold(0.0_f64, f64::max);
            if !cadence.is_finite() || cadence <= 0.0 || cadence > maximum_cadence {
                return Err(FormulaError::new(
                    ErrorKind::Time,
                    format!(
                        "output cadence {cadence} s exceeds recipe maximum {maximum_cadence} s"
                    ),
                ));
            }
        }
        if requirements.maximum_horizontal_spacing_m.is_some()
            || requirements.minimum_vertical_levels.is_some()
        {
            let grid = self.grid_metadata(0)?;
            if let Some(maximum_spacing) = requirements.maximum_horizontal_spacing_m {
                if !grid.dx_m.is_finite()
                    || !grid.dy_m.is_finite()
                    || grid.dx_m <= 0.0
                    || grid.dy_m <= 0.0
                {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "resolver returned non-finite or non-positive horizontal spacing",
                    ));
                }
                if !grid.horizontal_calculus_supported {
                    return Err(FormulaError::new(
                        ErrorKind::Unsupported,
                        "physical horizontal-spacing preflight is unavailable for this anisotropic projection",
                    ));
                }
                self.record_grid_convention(&grid.convention)?;
                let mut actual = grid.dx_m.max(grid.dy_m);
                if grid.convention == GridConvention::WrfMassPointProjected {
                    let map =
                        self.mass_map_factor_field(0, &grid, self.formula.program.output.span)?;
                    actual = 0.0;
                    for factor in map.data.iter() {
                        if !factor.is_finite() || *factor <= 0.0 {
                            return Err(FormulaError::new(
                                ErrorKind::Grid,
                                "MAPFAC_M contains invalid values during spacing preflight",
                            ));
                        }
                        actual = actual.max((grid.dx_m / *factor).max(grid.dy_m / *factor));
                    }
                }
                if actual > maximum_spacing {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        format!(
                            "horizontal spacing {actual} m exceeds recipe maximum {maximum_spacing} m"
                        ),
                    ));
                }
            }
            if let Some(minimum_levels) = requirements.minimum_vertical_levels {
                let actual = grid.nz.unwrap_or(0);
                if actual < minimum_levels {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        format!(
                            "grid has {actual} vertical levels; recipe requires at least {minimum_levels}"
                        ),
                    ));
                }
            }
        }
        self.warnings.extend(
            requirements
                .notes
                .iter()
                .map(|note| format!("Recipe note: {note}")),
        );
        Ok(())
    }

    fn eval_expr(&mut self, expr: &Expr, time_offset: isize) -> FormulaResult<Value> {
        let result = match &expr.kind {
            ExprKind::Number(value) => Ok(Value::Scalar(Scalar {
                value: *value,
                unit: Unit::dimensionless(),
            })),
            ExprKind::Text(value) => Ok(Value::Text(value.clone())),
            ExprKind::Identifier(name) => self.eval_identifier(name, time_offset, expr.span),
            ExprKind::Unary { op, value } => {
                let value = self.eval_expr(value, time_offset)?;
                self.eval_unary(*op, value, expr.span)
            }
            ExprKind::Binary { op, left, right } => {
                let left = self.eval_expr(left, time_offset)?;
                let right = self.eval_expr(right, time_offset)?;
                self.eval_binary(*op, left, right, expr.span)
            }
            ExprKind::Call { name, args } => self.eval_call(name, args, time_offset, expr.span),
        };
        result.map_err(|error| {
            if error.span.is_none() {
                error.at(expr.span)
            } else {
                error
            }
        })
    }

    fn eval_identifier(
        &mut self,
        name: &str,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "pi" => {
                return Ok(Value::Scalar(Scalar {
                    value: std::f64::consts::PI,
                    unit: Unit::dimensionless(),
                }));
            }
            "e" => {
                return Ok(Value::Scalar(Scalar {
                    value: std::f64::consts::E,
                    unit: Unit::dimensionless(),
                }));
            }
            "true" => {
                return Ok(Value::Scalar(Scalar {
                    value: 1.0,
                    unit: Unit::dimensionless(),
                }));
            }
            "false" => {
                return Ok(Value::Scalar(Scalar {
                    value: 0.0,
                    unit: Unit::dimensionless(),
                }));
            }
            _ => {}
        }
        if let Some(parameter) = self.parameters.get(&lower) {
            return Ok(Value::Scalar(parameter.clone()));
        }
        let key = (lower.clone(), time_offset);
        if let Some(value) = self.value_cache.get(&key) {
            return Ok(value.clone());
        }
        if let Some(expression) = self.assignments.get(&lower).copied() {
            let value = self.eval_expr(expression, time_offset)?;
            self.value_cache.insert(key, value.clone());
            return Ok(value);
        }
        let value = Value::Field(self.resolve_field(name, time_offset, span)?);
        self.value_cache.insert(key, value.clone());
        Ok(value)
    }

    fn resolve_field(
        &mut self,
        name: &str,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Field> {
        let request = FieldRequest {
            name: name.to_string(),
            time_offset,
        };
        let resolved = self.resolver.resolve(&request)?;
        self.resolved_to_field(name, time_offset, resolved, span)
    }

    fn resolved_to_field(
        &mut self,
        requested_name: &str,
        time_offset: isize,
        resolved: ResolvedField,
        span: Span,
    ) -> FormulaResult<Field> {
        if resolved.axes.contains(&Axis::Component) {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                "packed component fields are rejected because one unit/grid basis cannot safely describe heterogeneous components",
            )
            .at(span));
        }
        let shape = Shape::new(resolved.shape.clone(), resolved.axes.clone())?;
        let expected = shape.element_count()?;
        if resolved.data.len() != expected {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "resolver returned {} values for shape {:?} ({expected} required)",
                    resolved.data.len(),
                    resolved.shape
                ),
            )
            .at(span));
        }
        self.meter.allocate(expected, Some(span))?;
        self.meter.work(expected, Some(span))?;
        let matching_overrides: Vec<(&String, &String)> = self
            .options
            .variable_unit_overrides
            .iter()
            .filter(|(key, _)| {
                key.eq_ignore_ascii_case(requested_name)
                    || key.eq_ignore_ascii_case(&resolved.resolved_name)
            })
            .collect();
        if matching_overrides.len() > 1 {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                format!(
                    "multiple unit overrides match field '{requested_name}'; use one canonical key"
                ),
            )
            .at(span));
        }
        let source_declared_units = resolved.units.clone();
        let parsed_source = source_declared_units
            .as_ref()
            .map(|text| crate::parse_unit(text));
        if parsed_source.as_ref().is_some_and(Result::is_ok) && !matching_overrides.is_empty() {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                format!("field '{requested_name}' declares recognized units; overrides cannot reinterpret known metadata"),
            )
            .at(span));
        }
        let used_override = if source_declared_units.is_none()
            || parsed_source.as_ref().is_some_and(Result::is_err)
        {
            matching_overrides.first().map(|(key, _)| (*key).clone())
        } else {
            None
        };
        if let Some(key) = &used_override {
            self.used_unit_overrides.insert(key.clone());
        }
        let units_text = if used_override.is_some() {
            matching_overrides.first().map(|(_, value)| (*value).clone())
        } else {
            source_declared_units.clone()
        }
        .ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Unit,
                    format!("field '{requested_name}' has unknown units; declare EvaluationOptions.variable_unit_overrides"),
                )
                .at(span)
            })?;
        let unit = crate::parse_unit(&units_text).map_err(|error| {
            FormulaError::new(
                ErrorKind::Unit,
                format!("field '{requested_name}' units '{units_text}' are unsupported: {error}"),
            )
            .at(span)
        })?;
        if source_declared_units
            .as_ref()
            .is_some_and(|text| crate::parse_unit(text).is_err())
            && used_override.is_some()
        {
            self.warnings.push(format!(
                "Field '{requested_name}' declared unsupported units {:?}; assumed '{}' via an explicit override.",
                source_declared_units, units_text
            ));
        }
        let mut data = resolved.data;
        for value in data.iter() {
            if value.is_nan() && self.options.missing_policy == MissingPolicy::Error {
                return Err(FormulaError::new(
                    ErrorKind::MissingData,
                    format!("field '{requested_name}' contains missing values"),
                )
                .at(span));
            }
            if value.is_infinite() && self.options.non_finite_policy == NonFinitePolicy::Error {
                return Err(FormulaError::new(
                    ErrorKind::NonFinite,
                    format!("field '{requested_name}' contains an infinite value"),
                )
                .at(span));
            }
        }
        if unit.scale != 1.0 || unit.offset != 0.0 {
            self.meter.allocate(data.len(), Some(span))?;
            for value in std::sync::Arc::make_mut(&mut data) {
                *value = unit.to_si(*value);
            }
        }
        for value in data.iter() {
            if value.is_nan() && self.options.missing_policy == MissingPolicy::Error {
                return Err(FormulaError::new(
                    ErrorKind::MissingData,
                    format!("unit conversion for '{requested_name}' produced NaN"),
                )
                .at(span));
            }
            if value.is_infinite() && self.options.non_finite_policy == NonFinitePolicy::Error {
                return Err(FormulaError::new(
                    ErrorKind::NonFinite,
                    format!("unit conversion for '{requested_name}' overflowed"),
                )
                .at(span));
            }
        }
        let field = Field {
            data,
            shape,
            unit: unit.clone(),
            location: resolved.grid_location,
            description: resolved.description,
        };
        field.validate(self.meter.max_elements)?;
        self.input_provenance.push(ResolvedInputProvenance {
            requested_name: requested_name.to_string(),
            resolved_name: resolved.resolved_name,
            time_offset,
            shape: resolved.shape,
            axes: resolved.axes,
            source_units: source_declared_units,
            effective_units: units_text,
            unit_override_used: used_override,
            grid_location: resolved.grid_location,
        });
        Ok(field)
    }

    fn eval_unary(&mut self, op: UnaryOp, value: Value, span: Span) -> FormulaResult<Value> {
        match op {
            UnaryOp::Pos => Ok(value),
            UnaryOp::Neg => {
                if value.unit()?.is_absolute_temperature() {
                    return Err(FormulaError::new(
                        ErrorKind::Unit,
                        "negating an absolute temperature is representation-dependent; subtract a reference or negate a temperature difference",
                    ));
                }
                self.map_numeric(value, span, |number| -number, "negation")
            }
            UnaryOp::Not => self.map_boolean(value, span, |truth| !truth),
        }
    }

    fn eval_binary(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        match op {
            BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => {
                self.numeric_binary(op, left, right, span)
            }
            BinaryOp::Pow => self.power(left, right, span),
            BinaryOp::Eq
            | BinaryOp::NotEq
            | BinaryOp::Less
            | BinaryOp::LessEq
            | BinaryOp::Greater
            | BinaryOp::GreaterEq => self.compare(op, left, right, span),
            BinaryOp::And | BinaryOp::Or => self.logical_binary(op, left, right, span),
        }
    }

    fn map_numeric<F>(
        &mut self,
        value: Value,
        span: Span,
        operation: F,
        label: &str,
    ) -> FormulaResult<Value>
    where
        F: Fn(f64) -> f64 + Copy,
    {
        match value {
            Value::Scalar(mut scalar) => {
                if scalar.unit.logarithmic {
                    return Err(FormulaError::new(
                        ErrorKind::Unit,
                        format!("{label} is not defined for logarithmic units"),
                    ));
                }
                scalar.value = operation(scalar.value);
                self.check_number(scalar.value, span, label)?;
                Ok(Value::Scalar(scalar))
            }
            Value::Field(mut field) => {
                if field.unit.logarithmic {
                    return Err(FormulaError::new(
                        ErrorKind::Unit,
                        format!("{label} is not defined for logarithmic units"),
                    ));
                }
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = operation(*value);
                    self.check_number(*value, span, label)?;
                }
                Ok(Value::Field(field))
            }
            Value::Vector(mut vector) => {
                for field in &mut vector.components {
                    if field.unit.logarithmic {
                        return Err(FormulaError::new(
                            ErrorKind::Unit,
                            format!("{label} is not defined for logarithmic units"),
                        ));
                    }
                    self.meter.work(field.data.len(), Some(span))?;
                    self.meter.allocate(field.data.len(), Some(span))?;
                    for value in std::sync::Arc::make_mut(&mut field.data) {
                        *value = operation(*value);
                        self.check_number(*value, span, label)?;
                    }
                }
                Ok(Value::Vector(vector))
            }
            Value::Text(_) => Err(FormulaError::new(
                ErrorKind::Shape,
                format!("{label} requires numeric data"),
            )),
        }
    }

    fn check_number(&self, value: f64, span: Span, operation: &str) -> FormulaResult<()> {
        if value.is_nan() && self.options.missing_policy == MissingPolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::MissingData,
                format!("{operation} produced a missing value"),
            )
            .at(span));
        }
        if value.is_infinite() && self.options.non_finite_policy == NonFinitePolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::NonFinite,
                format!("{operation} produced infinity"),
            )
            .at(span));
        }
        Ok(())
    }

    fn numeric_binary(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        match (left, right) {
            (Value::Vector(left), Value::Vector(right)) => {
                if !matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                    return Err(FormulaError::new(
                        ErrorKind::Shape,
                        "vector-vector multiplication/division is not implicit; use dot() or component operations",
                    ));
                }
                if left.basis != right.basis || left.components.len() != right.components.len() {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "vector operands require identical component count and basis",
                    ));
                }
                let mut components = Vec::with_capacity(left.components.len());
                for (left_field, right_field) in left.components.into_iter().zip(right.components) {
                    match self.numeric_binary(
                        op,
                        Value::Field(left_field),
                        Value::Field(right_field),
                        span,
                    )? {
                        Value::Field(field) => components.push(field),
                        _ => {
                            return Err(FormulaError::new(
                                ErrorKind::Internal,
                                "vector component operation returned non-field",
                            ));
                        }
                    }
                }
                Ok(Value::Vector(VectorField {
                    components,
                    basis: left.basis,
                }))
            }
            (Value::Vector(mut vector), Value::Scalar(scalar))
                if matches!(op, BinaryOp::Mul | BinaryOp::Div) =>
            {
                for component in &mut vector.components {
                    let result = self.numeric_binary(
                        op,
                        Value::Field(component.clone()),
                        Value::Scalar(scalar.clone()),
                        span,
                    )?;
                    *component = expect_field(result, "vector scaling")?;
                }
                Ok(Value::Vector(vector))
            }
            (Value::Scalar(scalar), Value::Vector(mut vector)) if op == BinaryOp::Mul => {
                for component in &mut vector.components {
                    let result = self.numeric_binary(
                        op,
                        Value::Scalar(scalar.clone()),
                        Value::Field(component.clone()),
                        span,
                    )?;
                    *component = expect_field(result, "vector scaling")?;
                }
                Ok(Value::Vector(vector))
            }
            (Value::Vector(_), _) | (_, Value::Vector(_)) => Err(FormulaError::new(
                ErrorKind::Shape,
                "unsupported scalar/vector or field/vector arithmetic",
            )),
            (Value::Text(_), _) | (_, Value::Text(_)) => Err(FormulaError::new(
                ErrorKind::Shape,
                "arithmetic requires numeric values",
            )),
            (Value::Scalar(left), Value::Scalar(right)) => {
                let unit = binary_result_unit(op, &left.unit, &right.unit)?;
                let value = apply_arithmetic(op, left.value, right.value);
                self.check_number(value, span, "arithmetic")?;
                Ok(Value::Scalar(Scalar { value, unit }))
            }
            (Value::Field(mut left), Value::Scalar(right)) => {
                let unit = binary_result_unit(op, &left.unit, &right.unit)?;
                self.meter.work(left.data.len(), Some(span))?;
                self.meter.allocate(left.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut left.data) {
                    *value = apply_arithmetic(op, *value, right.value);
                    self.check_number(*value, span, "arithmetic")?;
                }
                left.unit = unit;
                Ok(Value::Field(left))
            }
            (Value::Scalar(left), Value::Field(mut right)) => {
                let unit = binary_result_unit(op, &left.unit, &right.unit)?;
                self.meter.work(right.data.len(), Some(span))?;
                self.meter.allocate(right.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut right.data) {
                    *value = apply_arithmetic(op, left.value, *value);
                    self.check_number(*value, span, "arithmetic")?;
                }
                right.unit = unit;
                Ok(Value::Field(right))
            }
            (Value::Field(mut left), Value::Field(right)) => {
                ensure_same_field_geometry(&left, &right)?;
                let unit = binary_result_unit(op, &left.unit, &right.unit)?;
                self.meter.work(left.data.len(), Some(span))?;
                self.meter.allocate(left.data.len(), Some(span))?;
                for (left_value, right_value) in std::sync::Arc::make_mut(&mut left.data)
                    .iter_mut()
                    .zip(right.data.iter())
                {
                    *left_value = apply_arithmetic(op, *left_value, *right_value);
                    self.check_number(*left_value, span, "arithmetic")?;
                }
                left.unit = unit;
                Ok(Value::Field(left))
            }
        }
    }

    fn power(&mut self, left: Value, right: Value, span: Span) -> FormulaResult<Value> {
        if matches!(left, Value::Vector(_)) {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                "component-wise vector powers are not physical vectors; select a component explicitly",
            ));
        }
        let exponent = match right {
            Value::Scalar(value) if value.unit.is_dimensionless() && !value.unit.logarithmic => {
                value.value
            }
            Value::Scalar(_) => {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    "power exponent must be dimensionless",
                ));
            }
            _ => {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    "power exponent must be a scalar",
                ));
            }
        };
        if !exponent.is_finite() {
            return Err(FormulaError::new(
                ErrorKind::Domain,
                "power exponent must be finite",
            ));
        }
        let source_unit = left.unit()?.clone();
        let result_unit = if source_unit.is_dimensionless() {
            Unit::dimensionless()
        } else {
            let rounded = exponent.round();
            if (exponent - rounded).abs() > 1.0e-12 || !(-12.0..=12.0).contains(&rounded) {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    "unitful powers require an integral scalar exponent in [-12, 12]",
                ));
            }
            source_unit.integer_power(rounded as i16)?
        };
        let mut result = self.map_numeric(left, span, |value| value.powf(exponent), "power")?;
        set_value_unit(&mut result, result_unit)?;
        Ok(result)
    }

    fn compare(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        let left_unit = left.unit()?.clone();
        let right_unit = right.unit()?.clone();
        ensure_comparable_units(&left_unit, &right_unit)?;
        self.elementwise_pair(left, right, span, Unit::dimensionless(), |left, right| {
            if left.is_nan() || right.is_nan() {
                return f64::NAN;
            }
            let truth = match op {
                BinaryOp::Eq => left == right,
                BinaryOp::NotEq => left != right,
                BinaryOp::Less => left < right,
                BinaryOp::LessEq => left <= right,
                BinaryOp::Greater => left > right,
                BinaryOp::GreaterEq => left >= right,
                _ => false,
            };
            if truth { 1.0 } else { 0.0 }
        })
    }

    fn logical_binary(
        &mut self,
        op: BinaryOp,
        left: Value,
        right: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        require_boolean_unit(left.unit()?)?;
        require_boolean_unit(right.unit()?)?;
        self.elementwise_pair(left, right, span, Unit::dimensionless(), |left, right| {
            if left.is_nan() || right.is_nan() {
                return f64::NAN;
            }
            let left = truthy(left);
            let right = truthy(right);
            if match op {
                BinaryOp::And => left && right,
                BinaryOp::Or => left || right,
                _ => false,
            } {
                1.0
            } else {
                0.0
            }
        })
    }

    fn map_boolean<F>(&mut self, value: Value, span: Span, operation: F) -> FormulaResult<Value>
    where
        F: Fn(bool) -> bool + Copy,
    {
        require_boolean_unit(value.unit()?)?;
        match value {
            Value::Scalar(value) => Ok(Value::Scalar(Scalar {
                value: {
                    let output = if value.value.is_nan() {
                        f64::NAN
                    } else if operation(truthy(value.value)) {
                        1.0
                    } else {
                        0.0
                    };
                    self.check_number(output, span, "logical operation")?;
                    output
                },
                unit: Unit::dimensionless(),
            })),
            Value::Field(mut field) => {
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = if value.is_nan() {
                        f64::NAN
                    } else if operation(truthy(*value)) {
                        1.0
                    } else {
                        0.0
                    };
                    self.check_number(*value, span, "logical operation")?;
                }
                field.unit = Unit::dimensionless();
                Ok(Value::Field(field))
            }
            _ => Err(FormulaError::new(
                ErrorKind::Shape,
                "logical operations support scalars and scalar fields",
            )),
        }
    }

    fn elementwise_pair<F>(
        &mut self,
        left: Value,
        right: Value,
        span: Span,
        unit: Unit,
        operation: F,
    ) -> FormulaResult<Value>
    where
        F: Fn(f64, f64) -> f64 + Copy,
    {
        match (left, right) {
            (Value::Scalar(left), Value::Scalar(right)) => {
                let value = operation(left.value, right.value);
                self.check_number(value, span, "elementwise operation")?;
                Ok(Value::Scalar(Scalar { value, unit }))
            }
            (Value::Field(mut field), Value::Scalar(scalar)) => {
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = operation(*value, scalar.value);
                    self.check_number(*value, span, "elementwise operation")?;
                }
                field.unit = unit;
                Ok(Value::Field(field))
            }
            (Value::Scalar(scalar), Value::Field(mut field)) => {
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = operation(scalar.value, *value);
                    self.check_number(*value, span, "elementwise operation")?;
                }
                field.unit = unit;
                Ok(Value::Field(field))
            }
            (Value::Field(mut left), Value::Field(right)) => {
                ensure_same_field_geometry(&left, &right)?;
                self.meter.work(left.data.len(), Some(span))?;
                self.meter.allocate(left.data.len(), Some(span))?;
                for (left_value, right_value) in std::sync::Arc::make_mut(&mut left.data)
                    .iter_mut()
                    .zip(right.data.iter())
                {
                    *left_value = operation(*left_value, *right_value);
                    self.check_number(*left_value, span, "elementwise operation")?;
                }
                left.unit = unit;
                Ok(Value::Field(left))
            }
            _ => Err(FormulaError::new(
                ErrorKind::Shape,
                "operation supports scalar broadcast and identically labeled scalar fields only",
            )),
        }
    }

    fn eval_call(
        &mut self,
        name: &str,
        args: &[Expr],
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        let name = name.to_ascii_lowercase();
        if name == "dt" {
            return self.temporal_derivative(&args[0], time_offset, span);
        }
        let mut values = Vec::with_capacity(args.len());
        for arg in args {
            values.push(self.eval_expr(arg, time_offset)?);
        }
        match name.as_str() {
            "where" => self.where_value(values.remove(0), values.remove(0), values.remove(0), span),
            "min" | "max" => {
                let left = values.remove(0);
                let right = values.remove(0);
                let unit = common_selection_unit(left.unit()?, right.unit()?)?;
                let take_max = name == "max";
                self.elementwise_pair(left, right, span, unit, move |left, right| {
                    if left.is_nan() || right.is_nan() {
                        f64::NAN
                    } else if take_max {
                        left.max(right)
                    } else {
                        left.min(right)
                    }
                })
            }
            "clamp" => self.clamp_value(values.remove(0), values.remove(0), values.remove(0), span),
            "abs" => {
                reject_vector(&values[0], "abs")?;
                if values[0].unit()?.is_absolute_temperature() {
                    return Err(FormulaError::new(
                        ErrorKind::Unit,
                        "abs() of an absolute temperature is representation-dependent; subtract a reference temperature first",
                    ));
                }
                self.map_numeric(values.remove(0), span, f64::abs, "abs")
            }
            "sqrt" => {
                reject_vector(&values[0], "sqrt")?;
                let unit = values[0].unit()?.square_root()?;
                self.unary_function(values.remove(0), span, unit, "sqrt", |value| {
                    if value < 0.0 { f64::NAN } else { value.sqrt() }
                })
            }
            "exp" | "ln" | "log" | "log10" => {
                reject_vector(&values[0], &name)?;
                require_plain_dimensionless(values[0].unit()?)?;
                let label = name.clone();
                self.unary_function(
                    values.remove(0),
                    span,
                    Unit::dimensionless(),
                    &label,
                    |value| match label.as_str() {
                        "exp" => value.exp(),
                        "log10" => {
                            if value > 0.0 {
                                value.log10()
                            } else {
                                f64::NAN
                            }
                        }
                        _ => {
                            if value > 0.0 {
                                value.ln()
                            } else {
                                f64::NAN
                            }
                        }
                    },
                )
            }
            "sin" | "cos" | "tan" => {
                reject_vector(&values[0], &name)?;
                require_plain_dimensionless(values[0].unit()?)?;
                let label = name.clone();
                self.unary_function(
                    values.remove(0),
                    span,
                    Unit::dimensionless(),
                    &label,
                    |value| match label.as_str() {
                        "sin" => value.sin(),
                        "cos" => value.cos(),
                        _ => value.tan(),
                    },
                )
            }
            "asin" | "acos" | "atan" => {
                reject_vector(&values[0], &name)?;
                require_plain_dimensionless(values[0].unit()?)?;
                let label = name.clone();
                let radians = crate::parse_unit("rad")?;
                self.unary_function(values.remove(0), span, radians, &label, |value| match label
                    .as_str()
                {
                    "asin" => {
                        if (-1.0..=1.0).contains(&value) {
                            value.asin()
                        } else {
                            f64::NAN
                        }
                    }
                    "acos" => {
                        if (-1.0..=1.0).contains(&value) {
                            value.acos()
                        } else {
                            f64::NAN
                        }
                    }
                    _ => value.atan(),
                })
            }
            "atan2" => {
                let y = values.remove(0);
                let x = values.remove(0);
                ensure_comparable_units(y.unit()?, x.unit()?)?;
                self.elementwise_pair(y, x, span, crate::parse_unit("rad")?, |y, x| {
                    if y.is_nan() || x.is_nan() {
                        f64::NAN
                    } else {
                        y.atan2(x)
                    }
                })
            }
            "floor" | "ceil" | "round" => {
                reject_vector(&values[0], &name)?;
                if values[0].unit()?.logarithmic {
                    return Err(FormulaError::new(
                        ErrorKind::Unit,
                        "rounding logarithmic quantities requires explicit conversion",
                    ));
                }
                let label = name.clone();
                self.round_in_display_units(values.remove(0), span, &label)
            }
            "is_finite" => {
                reject_vector(&values[0], "is_finite")?;
                self.unary_function(
                    values.remove(0),
                    span,
                    Unit::dimensionless(),
                    "is_finite",
                    |value| {
                        if value.is_finite() { 1.0 } else { 0.0 }
                    },
                )
            }
            "pow" => self.power(values.remove(0), values.remove(0), span),
            "quantity" => self.attach_unit(values.remove(0), values.remove(0), span),
            "convert" => self.convert_display_unit(values.remove(0), values.remove(0)),
            "grid_vector" => self.make_vector(values, VectorBasis::GridProjected),
            "earth_vector" => self.make_vector(values, VectorBasis::EarthRelative),
            "component" => self.vector_component(values.remove(0), values.remove(0)),
            "magnitude" => self.vector_magnitude(values.remove(0), span),
            "dot" => self.vector_dot(values.remove(0), values.remove(0), span),
            "dbz_to_z" => self.dbz_to_z(values.remove(0), span),
            "z_to_dbz" => self.z_to_dbz(values.remove(0), span),
            "ddx" => self.horizontal_derivative(
                expect_field(values.remove(0), "ddx")?,
                time_offset,
                Axis::X,
                span,
            ),
            "ddy" => self.horizontal_derivative(
                expect_field(values.remove(0), "ddy")?,
                time_offset,
                Axis::Y,
                span,
            ),
            "ddz" => {
                let field = expect_field(values.remove(0), "ddz")?;
                let height = if values.is_empty() {
                    self.default_height(time_offset, span)?
                } else {
                    self.used_height_datums.insert(HeightDatum::ExplicitField);
                    expect_field(values.remove(0), "ddz height")?
                };
                self.vertical_derivative(field, height, span)
                    .map(Value::Field)
            }
            "grad" => {
                let field = expect_field(values.remove(0), "grad")?;
                let height = if values.is_empty() {
                    None
                } else {
                    Some(expect_field(values.remove(0), "grad height")?)
                };
                self.gradient(field, height, time_offset, span)
            }
            "div" => self.divergence(expect_vector(values.remove(0), "div")?, time_offset, span),
            "curl" => self.curl(expect_vector(values.remove(0), "curl")?, time_offset, span),
            "laplacian" => {
                let field = expect_field(values.remove(0), "laplacian")?;
                let height = if values.is_empty() {
                    None
                } else {
                    Some(expect_field(values.remove(0), "laplacian height")?)
                };
                self.laplacian(field, height, time_offset, span)
            }
            "integrate_z" | "mean_z" => {
                self.used_height_datums.insert(HeightDatum::ExplicitField);
                let field = expect_field(values.remove(0), &name)?;
                let height = expect_field(values.remove(0), &name)?;
                let lower = expect_scalar(values.remove(0), "vertical lower bound")?;
                let upper = expect_scalar(values.remove(0), "vertical upper bound")?;
                self.vertical_reduce(field, height, lower, upper, name == "mean_z", span)
            }
            "interpolate_z" => {
                self.used_height_datums.insert(HeightDatum::ExplicitField);
                let field = expect_field(values.remove(0), "interpolate_z")?;
                let height = expect_field(values.remove(0), "interpolate_z")?;
                let target = expect_scalar(values.remove(0), "interpolation target")?;
                self.interpolate_vertical(field, height, target, span)
            }
            _ => Err(FormulaError::new(
                ErrorKind::UnknownFunction,
                format!("unknown function '{name}'"),
            )),
        }
    }

    fn unary_function<F>(
        &mut self,
        value: Value,
        span: Span,
        unit: Unit,
        label: &str,
        operation: F,
    ) -> FormulaResult<Value>
    where
        F: Fn(f64) -> f64 + Copy,
    {
        match value {
            Value::Scalar(value) => {
                let output = operation(value.value);
                self.check_number(output, span, label)?;
                Ok(Value::Scalar(Scalar {
                    value: output,
                    unit,
                }))
            }
            Value::Field(mut field) => {
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = operation(*value);
                    self.check_number(*value, span, label)?;
                }
                field.unit = unit;
                Ok(Value::Field(field))
            }
            _ => Err(FormulaError::new(
                ErrorKind::Shape,
                format!("{label} supports scalar values and scalar fields"),
            )),
        }
    }

    fn round_in_display_units(
        &mut self,
        value: Value,
        span: Span,
        operation: &str,
    ) -> FormulaResult<Value> {
        let unit = value.unit()?.clone();
        if unit.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "rounding logarithmic quantities requires explicit conversion",
            ));
        }
        self.unary_function(value, span, unit.clone(), operation, |value_si| {
            let displayed = unit.from_si(value_si);
            let rounded = match operation {
                "floor" => displayed.floor(),
                "ceil" => displayed.ceil(),
                _ => displayed.round(),
            };
            unit.to_si(rounded)
        })
    }

    fn attach_unit(
        &mut self,
        mut value: Value,
        unit_value: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        let text = expect_text(unit_value, "quantity unit")?;
        let unit = crate::parse_unit(&text)?;
        if !value.unit()?.is_dimensionless()
            || value.unit()?.logarithmic
            || value.unit()?.scale != 1.0
            || value.unit()?.offset != 0.0
            || value.unit()?.symbol != "1"
        {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "quantity() requires an untagged dimensionless numeric value",
            ));
        }
        match &mut value {
            Value::Scalar(scalar) => scalar.value = unit.to_si(scalar.value),
            Value::Field(field) => {
                self.meter.work(field.data.len(), Some(span))?;
                self.meter.allocate(field.data.len(), Some(span))?;
                for value in std::sync::Arc::make_mut(&mut field.data) {
                    *value = unit.to_si(*value);
                }
            }
            _ => {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    "quantity() does not accept vectors or text",
                ));
            }
        }
        set_value_unit(&mut value, unit)?;
        match &value {
            Value::Scalar(value) => self.check_number(value.value, span, "quantity conversion")?,
            Value::Field(field) => {
                for value in field.data.iter() {
                    self.check_number(*value, span, "quantity conversion")?;
                }
            }
            _ => {}
        }
        Ok(value)
    }

    fn convert_display_unit(
        &mut self,
        mut value: Value,
        unit_value: Value,
    ) -> FormulaResult<Value> {
        let text = expect_text(unit_value, "conversion unit")?;
        let target = crate::parse_unit(&text)?;
        let source = value.unit()?;
        ensure_conversion_compatible(source, &target)?;
        set_value_unit(&mut value, target)?;
        Ok(value)
    }

    fn make_vector(&self, values: Vec<Value>, basis: VectorBasis) -> FormulaResult<Value> {
        let mut components = Vec::with_capacity(values.len());
        for value in values {
            components.push(expect_field(value, "vector component")?);
        }
        let vector = VectorField { components, basis };
        vector.validate(self.meter.max_elements)?;
        if vector.components.len() < 2 || vector.components.len() > 3 {
            return Err(FormulaError::new(
                ErrorKind::Shape,
                "a vector requires two or three components",
            ));
        }
        Ok(Value::Vector(vector))
    }

    fn vector_component(&self, value: Value, index: Value) -> FormulaResult<Value> {
        let vector = expect_vector(value, "component")?;
        let index = expect_dimensionless_integer(
            expect_scalar(index, "component index")?,
            "component index",
        )?;
        let field = vector.components.get(index).cloned().ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Shape,
                format!(
                    "component index {index} outside vector with {} components",
                    vector.components.len()
                ),
            )
        })?;
        Ok(Value::Field(field))
    }

    fn vector_magnitude(&mut self, value: Value, span: Span) -> FormulaResult<Value> {
        let vector = expect_vector(value, "magnitude")?;
        vector.validate(self.meter.max_elements)?;
        let mut output = vector.components[0].clone();
        self.meter.allocate(output.data.len(), Some(span))?;
        self.meter.work(
            output.data.len().saturating_mul(vector.components.len()),
            Some(span),
        )?;
        let output_data = std::sync::Arc::make_mut(&mut output.data);
        for index in 0..output_data.len() {
            let mut sum = 0.0;
            for component in &vector.components {
                sum += component.data[index] * component.data[index];
            }
            output_data[index] = sum.sqrt();
            self.check_number(output_data[index], span, "vector magnitude")?;
        }
        output.description = "vector magnitude".to_string();
        Ok(Value::Field(output))
    }

    fn vector_dot(&mut self, left: Value, right: Value, span: Span) -> FormulaResult<Value> {
        let left = expect_vector(left, "dot")?;
        let right = expect_vector(right, "dot")?;
        if left.basis != right.basis || left.components.len() != right.components.len() {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "dot product vectors must have identical basis/component count",
            ));
        }
        left.validate(self.meter.max_elements)?;
        right.validate(self.meter.max_elements)?;
        ensure_same_field_geometry(&left.components[0], &right.components[0])?;
        let unit = left.components[0]
            .unit
            .multiply(&right.components[0].unit)?;
        let mut output = left.components[0].clone();
        self.meter.allocate(output.data.len(), Some(span))?;
        self.meter.work(
            output.data.len().saturating_mul(left.components.len()),
            Some(span),
        )?;
        let output_data = std::sync::Arc::make_mut(&mut output.data);
        for index in 0..output_data.len() {
            let mut sum = 0.0;
            for (left, right) in left.components.iter().zip(&right.components) {
                sum += left.data[index] * right.data[index];
            }
            output_data[index] = sum;
            self.check_number(output_data[index], span, "vector dot product")?;
        }
        output.unit = unit;
        output.description = "vector dot product".to_string();
        Ok(Value::Field(output))
    }

    fn dbz_to_z(&mut self, value: Value, span: Span) -> FormulaResult<Value> {
        if !value.unit()?.logarithmic || !value.unit()?.symbol.eq_ignore_ascii_case("dbz") {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "dbz_to_z() requires dBZ input",
            ));
        }
        let unit = Unit {
            dimension: Dimension([3, 0, 0, 0, 0, 0, 0]),
            temperature_kind: TemperatureKind::None,
            symbol: "mm^6/m^3".to_string(),
            scale: 1.0e-18,
            offset: 0.0,
            logarithmic: false,
        };
        self.unary_function(value, span, unit, "dbz_to_z", |dbz| {
            10.0_f64.powf(dbz / 10.0) * 1.0e-18
        })
    }

    fn z_to_dbz(&mut self, value: Value, span: Span) -> FormulaResult<Value> {
        if value.unit()?.dimension != Dimension([3, 0, 0, 0, 0, 0, 0]) || value.unit()?.logarithmic
        {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "z_to_dbz() requires linear radar reflectivity with length^3 dimensions",
            ));
        }
        let unit = crate::parse_unit("dBZ")?;
        self.unary_function(value, span, unit, "z_to_dbz", |z_si| {
            if z_si > 0.0 {
                10.0 * (z_si / 1.0e-18).log10()
            } else {
                f64::NAN
            }
        })
    }

    fn horizontal_derivative(
        &mut self,
        field: Field,
        time_offset: isize,
        axis: Axis,
        span: Span,
    ) -> FormulaResult<Value> {
        require_mass_scalar_grid(&field)?;
        if !matches!(axis, Axis::X | Axis::Y) {
            return Err(FormulaError::new(
                ErrorKind::Internal,
                "horizontal derivative axis must be x or y",
            ));
        }
        let (nz, ny, nx) = field_grid_dimensions(&field)?;
        let grid = self.grid_metadata(time_offset)?;
        if !grid.horizontal_calculus_supported {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "horizontal calculus is unavailable for this anisotropic/unsupported grid projection",
            ));
        }
        self.record_grid_convention(&grid.convention)?;
        if grid.nx != nx
            || grid.ny != ny
            || grid
                .nz
                .is_some_and(|value| field.shape.axes.contains(&Axis::Z) && value != nz)
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "resolver grid metadata does not match field shape",
            ));
        }
        let spacing = if axis == Axis::X {
            grid.dx_m
        } else {
            grid.dy_m
        };
        if !spacing.is_finite() || spacing <= 0.0 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                format!("grid spacing must be finite and positive, got {spacing}"),
            ));
        }
        let map_factor = match grid.convention {
            GridConvention::WrfMassPointProjected => {
                let map = self.mass_map_factor_field(time_offset, &grid, span)?;
                if map.shape.axes.as_slice() != [Axis::Y, Axis::X]
                    || map.shape.dims.as_slice() != [ny, nx]
                    || map.location != GridLocation::Mass
                    || !map.unit.is_dimensionless()
                    || map.unit.logarithmic
                {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "MAPFAC_M must be a dimensionless mass-grid [y,x] field",
                    ));
                }
                Some(map)
            }
            GridConvention::Cartesian => None,
        };
        if grid.convention == GridConvention::WrfMassPointProjected
            && !self
                .warnings
                .iter()
                .any(|warning| warning.contains("terrain-following model surfaces"))
        {
            self.warnings.push(
                "ddx/ddy are mass-point physical derivatives along terrain-following model surfaces (MAPFAC_M-scaled), not derivatives at constant geometric height and not the native staggered C-grid AVO/UH stencil."
                    .to_string(),
            );
        }
        let axis_size = if axis == Axis::X { nx } else { ny };
        if axis_size < 2 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "horizontal derivative requires at least two points on the differentiated axis",
            ));
        }
        if self.options.boundary_policy == BoundaryPolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "horizontal derivative touches domain boundaries; choose one_sided_second_order or missing boundary policy",
            ));
        }
        self.meter.allocate(field.data.len(), Some(span))?;
        self.meter
            .work(field.data.len().saturating_mul(4), Some(span))?;
        let mut output = vec![f64::NAN; field.data.len()];
        for k in 0..nz {
            for y in 0..ny {
                for x in 0..nx {
                    let coordinate = if axis == Axis::X { x } else { y };
                    if self.options.boundary_policy == BoundaryPolicy::Missing
                        && (coordinate == 0 || coordinate + 1 == axis_size)
                    {
                        continue;
                    }
                    let derivative =
                        finite_difference_uniform(&field.data, k, y, x, nz, ny, nx, axis, spacing)?;
                    let map = map_factor
                        .as_ref()
                        .map_or(1.0, |factor| factor.data[y * nx + x]);
                    if !map.is_finite() || map <= 0.0 {
                        return Err(FormulaError::new(
                            ErrorKind::Grid,
                            "MAPFAC_M contains a non-finite or non-positive value",
                        ));
                    }
                    let index = (k * ny + y) * nx + x;
                    output[index] = derivative * map;
                    self.check_number(output[index], span, "horizontal derivative")?;
                }
            }
        }
        let unit = field.unit.derivative_by(&crate::parse_unit("m")?)?;
        Ok(Value::Field(Field {
            data: output.into(),
            shape: field.shape,
            unit,
            location: GridLocation::Mass,
            description: format!(
                "{} derivative of {}",
                if axis == Axis::X { "x" } else { "y" },
                field.description
            ),
        }))
    }

    fn default_height(&mut self, time_offset: isize, span: Span) -> FormulaResult<Field> {
        if let Some(height) = self.default_height_cache.get(&time_offset) {
            return Ok(height.clone());
        }
        let grid = self.grid_metadata(time_offset)?;
        let name = grid.default_vertical_coordinate.ok_or_else(|| {
            FormulaError::new(ErrorKind::Grid, "resolver does not declare a default physical-height coordinate; pass height explicitly")
        })?;
        let datum = grid.default_height_datum.ok_or_else(|| {
            FormulaError::new(
                ErrorKind::Grid,
                "resolver default height does not declare MSL/AGL datum",
            )
        })?;
        self.used_height_datums.insert(datum);
        let height = self.resolve_field(&name, time_offset, span)?;
        self.default_height_cache
            .insert(time_offset, height.clone());
        Ok(height)
    }

    fn record_grid_convention(&mut self, convention: &GridConvention) -> FormulaResult<()> {
        if self
            .used_grid_convention
            .as_ref()
            .is_some_and(|existing| existing != convention)
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "grid convention changed during one formula evaluation",
            ));
        }
        self.used_grid_convention = Some(convention.clone());
        Ok(())
    }

    fn grid_metadata(&mut self, time_offset: isize) -> FormulaResult<GridMetadata> {
        if let Some(grid) = self.grid_cache.get(&time_offset) {
            return Ok(grid.clone());
        }
        let grid = self.resolver.grid_metadata(time_offset)?;
        self.grid_cache.insert(time_offset, grid.clone());
        Ok(grid)
    }

    fn mass_map_factor_field(
        &mut self,
        time_offset: isize,
        grid: &GridMetadata,
        span: Span,
    ) -> FormulaResult<Field> {
        if let Some(field) = self.map_factor_cache.get(&time_offset) {
            return Ok(field.clone());
        }
        let resolved = match grid.mass_map_factor.clone() {
            Some(field) => field,
            None => self.resolver.mass_map_factor(time_offset)?.ok_or_else(|| {
                FormulaError::new(ErrorKind::Grid, "WRF conformal calculus requires MAPFAC_M")
            })?,
        };
        let field = self.resolved_to_field("MAPFAC_M", time_offset, resolved, span)?;
        if field.location != GridLocation::Mass
            || field.shape.axes.as_slice() != [Axis::Y, Axis::X]
            || field.shape.dims.as_slice() != [grid.ny, grid.nx]
            || !field.unit.is_dimensionless()
            || field.unit.logarithmic
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "MAPFAC_M must be a plain dimensionless mass-point [y,x] field matching grid metadata",
            ));
        }
        self.map_factor_cache.insert(time_offset, field.clone());
        Ok(field)
    }

    fn vertical_derivative(
        &mut self,
        field: Field,
        height: Field,
        span: Span,
    ) -> FormulaResult<Field> {
        require_mass_volume(&field)?;
        require_physical_height(&height)?;
        ensure_same_field_geometry(&field, &height)?;
        let (nz, ny, nx) = field_grid_dimensions(&field)?;
        if nz < 3 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "second-order vertical derivative requires at least three levels",
            ));
        }
        if self.options.boundary_policy == BoundaryPolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "vertical derivative touches column boundaries; choose one_sided_second_order or missing boundary policy",
            ));
        }
        self.meter.allocate(field.data.len(), Some(span))?;
        self.meter
            .work(field.data.len().saturating_mul(8), Some(span))?;
        let mut output = vec![f64::NAN; field.data.len()];
        for y in 0..ny {
            for x in 0..nx {
                validate_height_column(&height, y, x, nz, ny, nx)?;
                for k in 0..nz {
                    if self.options.boundary_policy == BoundaryPolicy::Missing
                        && (k == 0 || k + 1 == nz)
                    {
                        continue;
                    }
                    let value = derivative_nonuniform_column(&field, &height, k, y, x, nz, ny, nx)?;
                    let index = (k * ny + y) * nx + x;
                    output[index] = value;
                    self.check_number(value, span, "vertical derivative")?;
                }
            }
        }
        if !self
            .warnings
            .iter()
            .any(|warning| warning.contains("fixed model column"))
        {
            self.warnings.push(
                "ddz follows physical height along each fixed model column; it is not a terrain-coordinate transformation of horizontal derivatives."
                    .to_string(),
            );
        }
        Ok(Field {
            data: output.into(),
            shape: field.shape,
            unit: field.unit.derivative_by(&crate::parse_unit("m")?)?,
            location: GridLocation::Mass,
            description: format!("vertical derivative of {}", field.description),
        })
    }

    fn gradient(
        &mut self,
        field: Field,
        height: Option<Field>,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        if field.shape.axes.as_slice() != [Axis::Y, Axis::X] || height.is_some() {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "3-D grad is intentionally rejected: terrain-following vector metric terms are not implemented",
            ));
        }
        let x = expect_field(
            self.horizontal_derivative(field.clone(), time_offset, Axis::X, span)?,
            "grad x",
        )?;
        let y = expect_field(
            self.horizontal_derivative(field.clone(), time_offset, Axis::Y, span)?,
            "grad y",
        )?;
        let components = vec![x, y];
        let vector = VectorField {
            components,
            basis: VectorBasis::GridProjected,
        };
        vector.validate(self.meter.max_elements)?;
        Ok(Value::Vector(vector))
    }

    fn divergence(
        &mut self,
        vector: VectorField,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        require_grid_vector(&vector)?;
        if vector.components.len() != 2
            || vector
                .components
                .iter()
                .any(|field| field.shape.axes.as_slice() != [Axis::Y, Axis::X])
        {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "div supports only 2-D grid-projected mass-point vectors; 3-D terrain-coordinate metric terms are not implemented",
            ));
        }
        self.metric_divergence_or_curl(
            &vector.components[0],
            &vector.components[1],
            time_offset,
            false,
            span,
        )
    }

    fn curl(
        &mut self,
        vector: VectorField,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        require_grid_vector(&vector)?;
        if vector.components.len() != 2
            || vector
                .components
                .iter()
                .any(|field| field.shape.axes.as_slice() != [Axis::Y, Axis::X])
        {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "curl supports only 2-D grid-projected mass-point vectors; 3-D terrain-coordinate metric terms are not implemented",
            ));
        }
        match vector.components.len() {
            2 => self.metric_divergence_or_curl(
                &vector.components[0],
                &vector.components[1],
                time_offset,
                true,
                span,
            ),
            _ => Err(FormulaError::new(
                ErrorKind::Shape,
                "curl requires a two- or three-component vector",
            )),
        }
    }

    fn metric_divergence_or_curl(
        &mut self,
        u: &Field,
        v: &Field,
        time_offset: isize,
        curl: bool,
        span: Span,
    ) -> FormulaResult<Value> {
        ensure_same_field_geometry(u, v)?;
        ensure_comparable_units(&u.unit, &v.unit)?;
        let (_, ny, nx) = field_grid_dimensions(u)?;
        if nx < 3 || ny < 3 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "second-order div/curl requires at least three points on each axis",
            ));
        }
        if self.options.boundary_policy == BoundaryPolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "div/curl touches domain boundaries; choose one_sided_second_order or missing",
            ));
        }
        let grid = self.grid_metadata(time_offset)?;
        if !grid.horizontal_calculus_supported {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "horizontal calculus is unavailable for this anisotropic/unsupported grid projection",
            ));
        }
        self.record_grid_convention(&grid.convention)?;
        if grid.nx != nx
            || grid.ny != ny
            || !grid.dx_m.is_finite()
            || !grid.dy_m.is_finite()
            || grid.dx_m <= 0.0
            || grid.dy_m <= 0.0
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "invalid grid metadata for div/curl",
            ));
        }
        let map = match grid.convention {
            GridConvention::WrfMassPointProjected => {
                let map = self.mass_map_factor_field(time_offset, &grid, span)?;
                if map.shape.axes.as_slice() != [Axis::Y, Axis::X]
                    || map.shape.dims.as_slice() != [ny, nx]
                    || !map.unit.is_dimensionless()
                    || map.unit.logarithmic
                    || map.location != GridLocation::Mass
                {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "invalid MAPFAC_M for div/curl",
                    ));
                }
                Some(map)
            }
            GridConvention::Cartesian => None,
        };
        self.meter.allocate(u.data.len(), Some(span))?;
        self.meter.allocate(v.data.len(), Some(span))?;
        self.meter.allocate(u.data.len(), Some(span))?;
        self.meter
            .work(u.data.len().saturating_mul(12), Some(span))?;
        let mut u_over_map = Vec::with_capacity(u.data.len());
        let mut v_over_map = Vec::with_capacity(v.data.len());
        for index in 0..u.data.len() {
            let factor = map.as_ref().map_or(1.0, |map| map.data[index]);
            if !factor.is_finite() || factor <= 0.0 {
                return Err(FormulaError::new(
                    ErrorKind::Grid,
                    "MAPFAC_M contains invalid values",
                ));
            }
            u_over_map.push(u.data[index] / factor);
            v_over_map.push(v.data[index] / factor);
        }
        let mut output = vec![f64::NAN; u.data.len()];
        for y in 0..ny {
            for x in 0..nx {
                if self.options.boundary_policy == BoundaryPolicy::Missing
                    && (x == 0 || x + 1 == nx || y == 0 || y + 1 == ny)
                {
                    continue;
                }
                let factor = map.as_ref().map_or(1.0, |map| map.data[y * nx + x]);
                let x_term = finite_difference_uniform(
                    if curl { &v_over_map } else { &u_over_map },
                    0,
                    y,
                    x,
                    1,
                    ny,
                    nx,
                    Axis::X,
                    grid.dx_m,
                )?;
                let y_term = finite_difference_uniform(
                    if curl { &u_over_map } else { &v_over_map },
                    0,
                    y,
                    x,
                    1,
                    ny,
                    nx,
                    Axis::Y,
                    grid.dy_m,
                )?;
                output[y * nx + x] = factor
                    * factor
                    * if curl {
                        x_term - y_term
                    } else {
                        x_term + y_term
                    };
                self.check_number(
                    output[y * nx + x],
                    span,
                    if curl { "curl" } else { "divergence" },
                )?;
            }
        }
        self.warnings.push(
            format!(
                "{} is the conformal mass-grid metric form m^2[D(component/m)]; it is not strict staggered WRF AVO parity.",
                if curl { "curl" } else { "div" }
            ),
        );
        Ok(Value::Field(Field {
            data: output.into(),
            shape: u.shape.clone(),
            unit: u.unit.derivative_by(&crate::parse_unit("m")?)?,
            location: GridLocation::Mass,
            description: if curl {
                "2-D conformal vertical curl"
            } else {
                "2-D conformal divergence"
            }
            .to_string(),
        }))
    }

    fn laplacian(
        &mut self,
        field: Field,
        height: Option<Field>,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        if field.shape.axes.as_slice() != [Axis::Y, Axis::X] || height.is_some() {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "laplacian supports only 2-D scalar fields; 3-D terrain-coordinate metric terms are not implemented",
            ));
        }
        self.scalar_laplace_beltrami(field, time_offset, span)
    }

    fn scalar_laplace_beltrami(
        &mut self,
        field: Field,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        require_mass_scalar_grid(&field)?;
        let (_, ny, nx) = field_grid_dimensions(&field)?;
        if nx < 3 || ny < 3 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "2-D laplacian requires at least three points on each horizontal axis",
            ));
        }
        if self.options.boundary_policy == BoundaryPolicy::OneSidedSecondOrder && (nx < 4 || ny < 4)
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "second-order one-sided laplacian boundaries require at least four points on each horizontal axis",
            ));
        }
        if self.options.boundary_policy == BoundaryPolicy::Error {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "laplacian touches domain boundaries; choose one_sided_second_order or missing",
            ));
        }
        let grid = self.grid_metadata(time_offset)?;
        if !grid.horizontal_calculus_supported {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                "horizontal calculus is unavailable for this anisotropic/unsupported grid projection",
            ));
        }
        self.record_grid_convention(&grid.convention)?;
        if grid.nx != nx
            || grid.ny != ny
            || !grid.dx_m.is_finite()
            || !grid.dy_m.is_finite()
            || grid.dx_m <= 0.0
            || grid.dy_m <= 0.0
        {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "invalid grid metadata for laplacian",
            ));
        }
        let map = match grid.convention {
            GridConvention::WrfMassPointProjected => {
                let map = self.mass_map_factor_field(time_offset, &grid, span)?;
                if map.shape.axes.as_slice() != [Axis::Y, Axis::X]
                    || map.shape.dims.as_slice() != [ny, nx]
                    || !map.unit.is_dimensionless()
                    || map.unit.logarithmic
                    || map.location != GridLocation::Mass
                {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "invalid MAPFAC_M for laplacian",
                    ));
                }
                Some(map)
            }
            GridConvention::Cartesian => None,
        };
        self.meter.allocate(field.data.len(), Some(span))?;
        self.meter
            .work(field.data.len().saturating_mul(12), Some(span))?;
        let mut output = vec![f64::NAN; field.data.len()];
        for y in 0..ny {
            for x in 0..nx {
                if self.options.boundary_policy == BoundaryPolicy::Missing
                    && (x == 0 || x + 1 == nx || y == 0 || y + 1 == ny)
                {
                    continue;
                }
                let dxx = second_difference_2d(&field.data, y, x, ny, nx, Axis::X, grid.dx_m)?;
                let dyy = second_difference_2d(&field.data, y, x, ny, nx, Axis::Y, grid.dy_m)?;
                let map_factor = map.as_ref().map_or(1.0, |field| field.data[y * nx + x]);
                if !map_factor.is_finite() || map_factor <= 0.0 {
                    return Err(FormulaError::new(
                        ErrorKind::Grid,
                        "MAPFAC_M contains invalid values",
                    ));
                }
                output[y * nx + x] = map_factor * map_factor * (dxx + dyy);
                self.check_number(output[y * nx + x], span, "laplacian")?;
            }
        }
        self.warnings.push(
            "laplacian is the 2-D conformal scalar Laplace-Beltrami operator m^2(Dxx+Dyy) on the model surface; it is not a 3-D terrain-coordinate Laplacian."
                .to_string(),
        );
        let length = crate::parse_unit("m")?;
        let unit = field.unit.derivative_by(&length)?.derivative_by(&length)?;
        Ok(Value::Field(Field {
            data: output.into(),
            shape: field.shape,
            unit,
            location: GridLocation::Mass,
            description: "2-D conformal scalar Laplace-Beltrami".to_string(),
        }))
    }

    fn vertical_reduce(
        &mut self,
        field: Field,
        height: Field,
        lower: Scalar,
        upper: Scalar,
        mean: bool,
        span: Span,
    ) -> FormulaResult<Value> {
        require_mass_volume(&field)?;
        require_physical_height(&height)?;
        ensure_same_field_geometry(&field, &height)?;
        require_length_scalar(&lower, "vertical lower bound")?;
        require_length_scalar(&upper, "vertical upper bound")?;
        if !(lower.value < upper.value) {
            return Err(FormulaError::new(
                ErrorKind::Domain,
                "vertical lower bound must be less than upper bound",
            ));
        }
        if self.options.missing_policy == MissingPolicy::IgnoreInReductions
            && !self
                .warnings
                .iter()
                .any(|warning| warning.contains("valid vertical segments"))
        {
            self.warnings.push(
                "MissingPolicy::IgnoreInReductions integrates/averages only valid vertical segments; an all-missing column remains missing."
                    .to_string(),
            );
        }
        let (nz, ny, nx) = field_grid_dimensions(&field)?;
        if nz < 2 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "vertical reduction requires at least two levels",
            ));
        }
        let output_shape = field.shape.without_z()?;
        let output_len = output_shape.element_count()?;
        self.meter.allocate(output_len, Some(span))?;
        self.meter
            .work(field.data.len().saturating_mul(10), Some(span))?;
        let mut output = vec![f64::NAN; output_len];
        for y in 0..ny {
            for x in 0..nx {
                validate_height_column(&height, y, x, nz, ny, nx)?;
                let bottom = height.data[y * nx + x];
                let top = height.data[((nz - 1) * ny + y) * nx + x];
                if lower.value < bottom || upper.value > top {
                    if self.options.boundary_policy == BoundaryPolicy::Missing {
                        continue;
                    }
                    return Err(FormulaError::new(
                        ErrorKind::Domain,
                        format!("requested vertical layer [{}, {}] m lies outside column [{bottom}, {top}] m", lower.value, upper.value),
                    )
                    .at(span));
                }
                let mut integral = 0.0;
                let mut covered = 0.0;
                let mut missing = false;
                for k in 0..nz - 1 {
                    let first = (k * ny + y) * nx + x;
                    let second = ((k + 1) * ny + y) * nx + x;
                    let z0 = height.data[first];
                    let z1 = height.data[second];
                    let start = lower.value.max(z0);
                    let end = upper.value.min(z1);
                    if !(start < end) {
                        continue;
                    }
                    let f0 = field.data[first];
                    let f1 = field.data[second];
                    if f0.is_nan() || f1.is_nan() {
                        match self.options.missing_policy {
                            MissingPolicy::Error => {
                                return Err(FormulaError::new(
                                    ErrorKind::MissingData,
                                    "vertical reduction encountered missing input",
                                )
                                .at(span));
                            }
                            MissingPolicy::Propagate => {
                                missing = true;
                                break;
                            }
                            MissingPolicy::IgnoreInReductions => continue,
                        }
                    }
                    let start_value = linear_interpolate(z0, z1, f0, f1, start);
                    let end_value = linear_interpolate(z0, z1, f0, f1, end);
                    integral += 0.5 * (start_value + end_value) * (end - start);
                    covered += end - start;
                }
                let index = y * nx + x;
                output[index] = if missing || covered == 0.0 {
                    f64::NAN
                } else if mean {
                    integral / covered
                } else {
                    integral
                };
                self.check_number(output[index], span, "vertical reduction")?;
            }
        }
        let unit = if mean {
            field.unit.clone()
        } else {
            field.unit.multiply(&crate::parse_unit("m")?)?
        };
        Ok(Value::Field(Field {
            data: output.into(),
            shape: output_shape,
            unit,
            location: GridLocation::Mass,
            description: if mean {
                "vertical layer mean"
            } else {
                "vertical layer integral"
            }
            .to_string(),
        }))
    }

    fn interpolate_vertical(
        &mut self,
        field: Field,
        height: Field,
        target: Scalar,
        span: Span,
    ) -> FormulaResult<Value> {
        require_mass_volume(&field)?;
        require_physical_height(&height)?;
        ensure_same_field_geometry(&field, &height)?;
        require_length_scalar(&target, "vertical interpolation target")?;
        let (nz, ny, nx) = field_grid_dimensions(&field)?;
        if nz < 2 {
            return Err(FormulaError::new(
                ErrorKind::Grid,
                "vertical interpolation requires at least two levels",
            ));
        }
        let output_shape = field.shape.without_z()?;
        let output_len = output_shape.element_count()?;
        self.meter.allocate(output_len, Some(span))?;
        self.meter
            .work(field.data.len().saturating_mul(4), Some(span))?;
        let mut output = vec![f64::NAN; output_len];
        for y in 0..ny {
            for x in 0..nx {
                validate_height_column(&height, y, x, nz, ny, nx)?;
                let bottom = height.data[y * nx + x];
                let top = height.data[((nz - 1) * ny + y) * nx + x];
                if target.value < bottom || target.value > top {
                    if self.options.boundary_policy == BoundaryPolicy::Missing {
                        continue;
                    }
                    return Err(FormulaError::new(
                        ErrorKind::Domain,
                        format!("vertical interpolation target {} m lies outside column [{bottom}, {top}] m", target.value),
                    )
                    .at(span));
                }
                for k in 0..nz - 1 {
                    let first = (k * ny + y) * nx + x;
                    let second = ((k + 1) * ny + y) * nx + x;
                    let z0 = height.data[first];
                    let z1 = height.data[second];
                    if target.value >= z0 && target.value <= z1 {
                        let f0 = field.data[first];
                        let f1 = field.data[second];
                        if f0.is_nan() || f1.is_nan() {
                            if self.options.missing_policy == MissingPolicy::Error {
                                return Err(FormulaError::new(
                                    ErrorKind::MissingData,
                                    "vertical interpolation encountered missing input",
                                )
                                .at(span));
                            }
                        } else {
                            output[y * nx + x] = linear_interpolate(z0, z1, f0, f1, target.value);
                        }
                        break;
                    }
                }
                self.check_number(output[y * nx + x], span, "vertical interpolation")?;
            }
        }
        Ok(Value::Field(Field {
            data: output.into(),
            shape: output_shape,
            unit: field.unit,
            location: GridLocation::Mass,
            description: "vertical interpolation".to_string(),
        }))
    }

    fn temporal_derivative(
        &mut self,
        expression: &Expr,
        time_offset: isize,
        span: Span,
    ) -> FormulaResult<Value> {
        let previous_offset = time_offset
            .checked_sub(1)
            .ok_or_else(|| FormulaError::new(ErrorKind::Time, "temporal offset underflow"))?;
        let next_offset = time_offset
            .checked_add(1)
            .ok_or_else(|| FormulaError::new(ErrorKind::Time, "temporal offset overflow"))?;
        let current_time = self.resolver.time_seconds(time_offset)?;
        let previous_time = available_time(self.resolver, previous_offset)?;
        let next_time = available_time(self.resolver, next_offset)?;
        match (previous_time, next_time) {
            (Some(previous), Some(next)) => {
                let earlier = self.eval_expr(expression, previous_offset)?;
                let current = self.eval_expr(expression, time_offset)?;
                let later = self.eval_expr(expression, next_offset)?;
                let weights =
                    three_point_derivative_weights([previous, current_time, next], current_time)?;
                self.temporal_linear_combination(vec![earlier, current, later], weights, span)
            }
            (previous, next)
                if self.options.boundary_policy == BoundaryPolicy::OneSidedSecondOrder =>
            {
                if let Some(next_time) = next {
                    let second_next_offset = time_offset.checked_add(2).ok_or_else(|| {
                        FormulaError::new(ErrorKind::Time, "temporal offset overflow")
                    })?;
                    let second_next_time = available_time(self.resolver, second_next_offset)?
                        .ok_or_else(|| {
                            FormulaError::new(
                                ErrorKind::Time,
                                "second-order forward dt requires two following output times",
                            )
                        })?;
                    let current = self.eval_expr(expression, time_offset)?;
                    let later = self.eval_expr(expression, next_offset)?;
                    let second_later = self.eval_expr(expression, second_next_offset)?;
                    let weights = three_point_derivative_weights(
                        [current_time, next_time, second_next_time],
                        current_time,
                    )?;
                    self.temporal_linear_combination(
                        vec![current, later, second_later],
                        weights,
                        span,
                    )
                } else if let Some(previous_time) = previous {
                    let second_previous_offset = time_offset.checked_sub(2).ok_or_else(|| {
                        FormulaError::new(ErrorKind::Time, "temporal offset underflow")
                    })?;
                    let second_previous_time =
                        available_time(self.resolver, second_previous_offset)?.ok_or_else(
                            || {
                                FormulaError::new(
                                    ErrorKind::Time,
                                    "second-order backward dt requires two preceding output times",
                                )
                            },
                        )?;
                    let second_earlier = self.eval_expr(expression, second_previous_offset)?;
                    let earlier = self.eval_expr(expression, previous_offset)?;
                    let current = self.eval_expr(expression, time_offset)?;
                    let weights = three_point_derivative_weights(
                        [second_previous_time, previous_time, current_time],
                        current_time,
                    )?;
                    self.temporal_linear_combination(
                        vec![second_earlier, earlier, current],
                        weights,
                        span,
                    )
                } else {
                    Err(FormulaError::new(
                        ErrorKind::Time,
                        "dt requires at least one adjacent output time",
                    ))
                }
            }
            _ if self.options.boundary_policy == BoundaryPolicy::Missing => {
                let current = self.eval_expr(expression, time_offset)?;
                self.missing_temporal_like(current, span)
            }
            _ => Err(FormulaError::new(
                ErrorKind::Time,
                "dt requires adjacent output times; select one_sided_second_order or missing boundary policy at a time boundary",
            )),
        }
    }

    fn temporal_linear_combination(
        &mut self,
        values: Vec<Value>,
        weights: [f64; 3],
        span: Span,
    ) -> FormulaResult<Value> {
        if values.len() != 3 {
            return Err(FormulaError::new(
                ErrorKind::Internal,
                "temporal stencil requires three values",
            ));
        }
        let source_unit = values[0].unit()?.clone();
        if source_unit.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "dt of a logarithmic quantity requires explicit linear conversion",
            ));
        }
        for value in values.iter().skip(1) {
            ensure_comparable_units(&source_unit, value.unit()?)?;
        }
        let result_unit = source_unit.derivative_by(&crate::parse_unit("s")?)?;
        match &values[0] {
            Value::Scalar(_) => {
                let mut result = 0.0;
                for (value, weight) in values.iter().zip(weights) {
                    result += value_at(value, 0)? * weight;
                }
                self.check_number(result, span, "temporal derivative")?;
                Ok(Value::Scalar(Scalar {
                    value: result,
                    unit: result_unit,
                }))
            }
            Value::Field(template) => {
                for value in &values {
                    ensure_same_field_geometry(
                        template,
                        value_field(value).ok_or_else(|| {
                            FormulaError::new(
                                ErrorKind::Shape,
                                "dt expression changed scalar/field kind across times",
                            )
                        })?,
                    )?;
                }
                let mut output = template.clone();
                self.meter.allocate(output.data.len(), Some(span))?;
                self.meter
                    .work(output.data.len().saturating_mul(6), Some(span))?;
                let data = std::sync::Arc::make_mut(&mut output.data);
                for index in 0..data.len() {
                    let first = values.get(0).ok_or_else(|| {
                        FormulaError::new(ErrorKind::Internal, "missing first temporal sample")
                    })?;
                    let second = values.get(1).ok_or_else(|| {
                        FormulaError::new(ErrorKind::Internal, "missing second temporal sample")
                    })?;
                    let third = values.get(2).ok_or_else(|| {
                        FormulaError::new(ErrorKind::Internal, "missing third temporal sample")
                    })?;
                    data[index] = value_at(first, index)? * weights[0]
                        + value_at(second, index)? * weights[1]
                        + value_at(third, index)? * weights[2];
                    self.check_number(data[index], span, "temporal derivative")?;
                }
                output.unit = result_unit;
                output.description = "temporal derivative at fixed model-grid index".to_string();
                self.warnings.push("dt is evaluated at a fixed model-grid index; moving nests or changing grids require explicit remapping.".to_string());
                Ok(Value::Field(output))
            }
            Value::Vector(template) => {
                for value in &values {
                    let vector = match value {
                        Value::Vector(vector)
                            if vector.basis == template.basis
                                && vector.components.len() == template.components.len() =>
                        {
                            vector
                        }
                        _ => {
                            return Err(FormulaError::new(
                                ErrorKind::Shape,
                                "dt vector basis/component count changed across times",
                            ));
                        }
                    };
                    vector.validate(self.meter.max_elements)?;
                }
                let mut components = Vec::with_capacity(template.components.len());
                for component in 0..template.components.len() {
                    let mut samples = Vec::with_capacity(values.len());
                    for value in &values {
                        match value {
                            Value::Vector(vector) => {
                                let field = vector.components.get(component).ok_or_else(|| {
                                    FormulaError::new(
                                        ErrorKind::Shape,
                                        "dt vector component disappeared across times",
                                    )
                                })?;
                                samples.push(Value::Field(field.clone()));
                            }
                            _ => {
                                return Err(FormulaError::new(
                                    ErrorKind::Shape,
                                    "dt expression changed from vector across times",
                                ));
                            }
                        }
                    }
                    components.push(expect_field(
                        self.temporal_linear_combination(samples, weights, span)?,
                        "dt vector component",
                    )?);
                }
                Ok(Value::Vector(VectorField {
                    components,
                    basis: template.basis,
                }))
            }
            Value::Text(_) => Err(FormulaError::new(
                ErrorKind::Shape,
                "dt cannot operate on text",
            )),
        }
    }

    fn missing_temporal_like(&mut self, mut value: Value, span: Span) -> FormulaResult<Value> {
        let source_unit = value.unit()?.clone();
        if source_unit.logarithmic {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                "dt of a logarithmic quantity requires explicit linear conversion",
            ));
        }
        let result_unit = source_unit.derivative_by(&crate::parse_unit("s")?)?;
        match &mut value {
            Value::Scalar(scalar) => {
                scalar.value = f64::NAN;
            }
            Value::Field(field) => {
                self.meter.allocate(field.data.len(), Some(span))?;
                std::sync::Arc::make_mut(&mut field.data).fill(f64::NAN);
            }
            Value::Vector(vector) => {
                for field in &mut vector.components {
                    self.meter.allocate(field.data.len(), Some(span))?;
                    std::sync::Arc::make_mut(&mut field.data).fill(f64::NAN);
                }
            }
            Value::Text(_) => {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    "dt cannot operate on text",
                ));
            }
        }
        set_value_unit(&mut value, result_unit)?;
        Ok(value)
    }

    fn where_value(
        &mut self,
        condition: Value,
        when_true: Value,
        when_false: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        require_boolean_unit(condition.unit()?)?;
        let unit = common_selection_unit(when_true.unit()?, when_false.unit()?)?;
        self.ternary_scalar_field(
            condition,
            when_true,
            when_false,
            unit,
            span,
            |condition, yes, no| {
                if condition.is_nan() {
                    f64::NAN
                } else if truthy(condition) {
                    yes
                } else {
                    no
                }
            },
        )
    }

    fn clamp_value(
        &mut self,
        value: Value,
        minimum: Value,
        maximum: Value,
        span: Span,
    ) -> FormulaResult<Value> {
        let unit = common_selection_unit(value.unit()?, minimum.unit()?)?;
        let _ = common_selection_unit(&unit, maximum.unit()?)?;
        let length = [
            value_field(&value),
            value_field(&minimum),
            value_field(&maximum),
        ]
        .into_iter()
        .flatten()
        .next()
        .map_or(1, |field| field.data.len());
        for index in 0..length {
            let lower = value_at(&minimum, index)?;
            let upper = value_at(&maximum, index)?;
            if !lower.is_nan() && !upper.is_nan() && lower > upper {
                return Err(FormulaError::new(
                    ErrorKind::Domain,
                    format!("clamp lower bound exceeds upper bound at flattened index {index}"),
                )
                .at(span));
            }
        }
        self.ternary_scalar_field(
            value,
            minimum,
            maximum,
            unit,
            span,
            |value, minimum, maximum| {
                if value.is_nan() || minimum.is_nan() || maximum.is_nan() {
                    f64::NAN
                } else if minimum > maximum {
                    f64::NAN
                } else {
                    value.clamp(minimum, maximum)
                }
            },
        )
    }

    fn ternary_scalar_field<F>(
        &mut self,
        first: Value,
        second: Value,
        third: Value,
        unit: Unit,
        span: Span,
        operation: F,
    ) -> FormulaResult<Value>
    where
        F: Fn(f64, f64, f64) -> f64,
    {
        reject_vector(&first, "ternary operation")?;
        reject_vector(&second, "ternary operation")?;
        reject_vector(&third, "ternary operation")?;
        let template = [
            value_field(&first),
            value_field(&second),
            value_field(&third),
        ]
        .into_iter()
        .flatten()
        .next();
        if let Some(template) = template {
            for field in [
                value_field(&first),
                value_field(&second),
                value_field(&third),
            ]
            .into_iter()
            .flatten()
            {
                ensure_same_field_geometry(template, field)?;
            }
            let mut output = template.clone();
            self.meter.allocate(output.data.len(), Some(span))?;
            self.meter
                .work(output.data.len().saturating_mul(4), Some(span))?;
            let output_data = std::sync::Arc::make_mut(&mut output.data);
            for index in 0..output_data.len() {
                let first_value = value_at(&first, index)?;
                let second_value = value_at(&second, index)?;
                let third_value = value_at(&third, index)?;
                output_data[index] = operation(first_value, second_value, third_value);
                self.check_number(output_data[index], span, "ternary operation")?;
            }
            output.unit = unit;
            Ok(Value::Field(output))
        } else {
            let output = operation(
                value_at(&first, 0)?,
                value_at(&second, 0)?,
                value_at(&third, 0)?,
            );
            self.check_number(output, span, "ternary operation")?;
            Ok(Value::Scalar(Scalar {
                value: output,
                unit,
            }))
        }
    }

    fn finish(mut self, value: Value) -> FormulaResult<FormulaOutput> {
        let unused: Vec<String> = self
            .options
            .variable_unit_overrides
            .keys()
            .filter(|key| !self.used_unit_overrides.contains(*key))
            .cloned()
            .collect();
        if !unused.is_empty() {
            return Err(FormulaError::new(
                ErrorKind::Unit,
                format!("unused variable unit override(s): {}", unused.join(", ")),
            ));
        }
        let source_unit = value.unit()?.clone();
        let output_unit = match &self.formula.expected_output_units {
            Some(text) => {
                let expected = crate::parse_unit(text)?;
                ensure_conversion_compatible(&source_unit, &expected)?;
                expected
            }
            None => source_unit,
        };
        let (data, shape, axes, description) = match value {
            Value::Scalar(value) => {
                self.meter.allocate(1, None)?;
                (
                    vec![output_unit.from_si(value.value)],
                    Vec::new(),
                    Vec::new(),
                    "custom scalar formula".to_string(),
                )
            }
            Value::Field(field) => {
                self.meter.allocate(field.data.len(), None)?;
                let data = field
                    .data
                    .iter()
                    .map(|value| output_unit.from_si(*value))
                    .collect();
                (data, field.shape.dims, field.shape.axes, field.description)
            }
            Value::Vector(vector) => {
                vector.validate(self.meter.max_elements)?;
                let first = &vector.components[0];
                let total = first
                    .data
                    .len()
                    .checked_mul(vector.components.len())
                    .ok_or_else(|| {
                        FormulaError::new(ErrorKind::Limit, "vector output size overflow")
                    })?;
                self.meter.allocate(total, None)?;
                let mut data = Vec::with_capacity(total);
                for component in &vector.components {
                    data.extend(
                        component
                            .data
                            .iter()
                            .map(|value| output_unit.from_si(*value)),
                    );
                }
                let mut shape = Vec::with_capacity(first.shape.dims.len() + 1);
                shape.push(vector.components.len());
                shape.extend(first.shape.dims.iter().copied());
                let mut axes = Vec::with_capacity(first.shape.axes.len() + 1);
                axes.push(Axis::Component);
                axes.extend(first.shape.axes.iter().copied());
                (
                    data,
                    shape,
                    axes,
                    format!("custom {:?} vector formula", vector.basis),
                )
            }
            Value::Text(_) => {
                return Err(FormulaError::new(
                    ErrorKind::Shape,
                    "final formula result cannot be text",
                ));
            }
        };
        let grid_convention = self.used_grid_convention.clone();
        self.input_provenance.sort_by(|left, right| {
            (&left.requested_name, left.time_offset, &left.resolved_name).cmp(&(
                &right.requested_name,
                right.time_offset,
                &right.resolved_name,
            ))
        });
        self.input_provenance.dedup_by(|left, right| {
            left.requested_name == right.requested_name
                && left.resolved_name == right.resolved_name
                && left.time_offset == right.time_offset
        });
        for value in &data {
            self.check_number(
                *value,
                self.formula.program.output.span,
                "output unit conversion",
            )?;
        }
        Ok(FormulaOutput {
            data,
            shape,
            axes,
            units: output_unit.symbol.clone(),
            description,
            provenance: FormulaProvenance {
                engine_version: crate::ENGINE_VERSION.to_string(),
                canonical_source: self.formula.canonical_source().to_string(),
                source_fingerprint: source_fingerprint(self.formula.source()),
                base_time_index: self.resolver.base_time_index(),
                valid_time: self.resolver.valid_time(0),
                input_identity: self.resolver.input_identity(),
                recipe_name: self.formula.recipe_name.clone(),
                recipe_version: self.formula.recipe_version.clone(),
                recipe_references: self.formula.recipe_references.clone(),
                recipe_requirements: self.formula.recipe_requirements.clone(),
                variable_unit_overrides: self.options.variable_unit_overrides.clone(),
                inputs: self.input_provenance,
                parameters: self.parameter_provenance,
                boundary_policy: self.options.boundary_policy,
                missing_policy: self.options.missing_policy,
                non_finite_policy: self.options.non_finite_policy,
                grid_convention,
                vertical_height_datums: self.used_height_datums.into_iter().collect(),
                warnings: self.warnings,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldRequest, GridMetadata, ResolvedField, compile};

    #[derive(Clone)]
    struct SyntheticResolver {
        fields: HashMap<(String, isize), ResolvedField>,
        grid: GridMetadata,
        times: HashMap<isize, f64>,
    }

    impl FieldResolver for SyntheticResolver {
        fn resolve(&self, request: &FieldRequest) -> FormulaResult<ResolvedField> {
            self.fields
                .get(&(request.name.to_ascii_lowercase(), request.time_offset))
                .cloned()
                .ok_or_else(|| {
                    FormulaError::new(
                        ErrorKind::Resolver,
                        format!(
                            "synthetic field '{}' missing at offset {}",
                            request.name, request.time_offset
                        ),
                    )
                })
        }

        fn grid_metadata(&self, _time_offset: isize) -> FormulaResult<GridMetadata> {
            Ok(self.grid.clone())
        }

        fn time_seconds(&self, time_offset: isize) -> FormulaResult<f64> {
            self.times.get(&time_offset).copied().ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::Time,
                    format!("time offset {time_offset} outside synthetic range"),
                )
            })
        }
    }

    fn resolved(
        name: &str,
        data: Vec<f64>,
        shape: Vec<usize>,
        axes: Vec<Axis>,
        units: &str,
    ) -> ResolvedField {
        ResolvedField {
            resolved_name: name.to_string(),
            data: data.into(),
            shape,
            axes,
            units: Some(units.to_string()),
            grid_location: GridLocation::Mass,
            vector_basis: None,
            description: name.to_string(),
        }
    }

    fn grid(
        nx: usize,
        ny: usize,
        nz: Option<usize>,
        convention: GridConvention,
        map: Option<Vec<f64>>,
    ) -> GridMetadata {
        GridMetadata {
            nx,
            ny,
            nz,
            dx_m: 1.0,
            dy_m: 1.0,
            convention,
            horizontal_calculus_supported: true,
            mass_map_factor: map
                .map(|data| resolved("MAPFAC_M", data, vec![ny, nx], vec![Axis::Y, Axis::X], "1")),
            default_vertical_coordinate: Some("z".to_string()),
            default_height_datum: Some(HeightDatum::Msl),
        }
    }

    fn evaluate_source(source: &str, resolver: &SyntheticResolver) -> FormulaOutput {
        compile(source)
            .unwrap()
            .evaluate(
                resolver,
                &ParameterValues::new(),
                &EvaluationOptions::default(),
            )
            .unwrap()
    }

    #[test]
    fn conformal_metric_divergence_and_curl_match_manufactured_linear_fields() {
        let (nx, ny) = (5, 5);
        let mut map = Vec::new();
        let mut divergent_u = Vec::new();
        let mut divergent_v = Vec::new();
        let mut rotating_u = Vec::new();
        let mut rotating_v = Vec::new();
        for y in 0..ny {
            for x in 0..nx {
                let m = 0.8 + 0.03 * x as f64 + 0.02 * y as f64;
                map.push(m);
                divergent_u.push(m * x as f64);
                divergent_v.push(m * y as f64);
                rotating_u.push(-m * y as f64);
                rotating_v.push(m * x as f64);
            }
        }
        let mut fields = HashMap::new();
        fields.insert(
            ("u".to_string(), 0),
            resolved(
                "u",
                divergent_u,
                vec![ny, nx],
                vec![Axis::Y, Axis::X],
                "m/s",
            ),
        );
        fields.insert(
            ("v".to_string(), 0),
            resolved(
                "v",
                divergent_v,
                vec![ny, nx],
                vec![Axis::Y, Axis::X],
                "m/s",
            ),
        );
        fields.insert(
            ("ru".to_string(), 0),
            resolved(
                "ru",
                rotating_u,
                vec![ny, nx],
                vec![Axis::Y, Axis::X],
                "m/s",
            ),
        );
        fields.insert(
            ("rv".to_string(), 0),
            resolved(
                "rv",
                rotating_v,
                vec![ny, nx],
                vec![Axis::Y, Axis::X],
                "m/s",
            ),
        );
        let resolver = SyntheticResolver {
            fields,
            grid: grid(
                nx,
                ny,
                None,
                GridConvention::WrfMassPointProjected,
                Some(map.clone()),
            ),
            times: HashMap::from([(0, 0.0)]),
        };
        let divergence = evaluate_source("div(grid_vector(u, v))", &resolver);
        let curl = evaluate_source("curl(grid_vector(ru, rv))", &resolver);
        for ((divergence, curl), m) in divergence.data.iter().zip(&curl.data).zip(map) {
            assert!((*divergence - 2.0 * m * m).abs() < 1.0e-10);
            assert!((*curl - 2.0 * m * m).abs() < 1.0e-10);
        }
    }

    #[test]
    fn cartesian_laplace_beltrami_of_quadratic_is_four() {
        let (nx, ny) = (5, 5);
        let mut values = Vec::new();
        for y in 0..ny {
            for x in 0..nx {
                values.push((x * x + y * y) as f64);
            }
        }
        let resolver = SyntheticResolver {
            fields: HashMap::from([(
                ("f".to_string(), 0),
                resolved("f", values, vec![ny, nx], vec![Axis::Y, Axis::X], "m^2"),
            )]),
            grid: grid(nx, ny, None, GridConvention::Cartesian, None),
            times: HashMap::from([(0, 0.0)]),
        };
        let output = evaluate_source("laplacian(f)", &resolver);
        assert!(
            output
                .data
                .iter()
                .all(|value| (*value - 4.0).abs() < 1.0e-10)
        );
    }

    #[test]
    fn divergence_of_gradient_equals_laplace_beltrami() {
        let (nx, ny) = (5, 5);
        let mut map = Vec::new();
        let mut values = Vec::new();
        for y in 0..ny {
            for x in 0..nx {
                map.push(0.9 + 0.02 * x as f64 + 0.01 * y as f64);
                values.push((x * x + y * y) as f64);
            }
        }
        let resolver = SyntheticResolver {
            fields: HashMap::from([(
                ("f".to_string(), 0),
                resolved("f", values, vec![ny, nx], vec![Axis::Y, Axis::X], "m^2"),
            )]),
            grid: grid(
                nx,
                ny,
                None,
                GridConvention::WrfMassPointProjected,
                Some(map),
            ),
            times: HashMap::from([(0, 0.0)]),
        };
        let divergence = evaluate_source("div(grad(f))", &resolver);
        let laplacian = evaluate_source("laplacian(f)", &resolver);
        assert!(
            divergence
                .data
                .iter()
                .zip(laplacian.data)
                .all(|(left, right)| (*left - right).abs() < 1.0e-10)
        );
    }

    #[test]
    fn nonuniform_vertical_derivative_is_exact_for_quadratic() {
        let (nx, ny, nz) = (2, 2, 4);
        let levels = [0.0, 1.0, 3.0, 6.0];
        let mut z = Vec::new();
        let mut f = Vec::new();
        for level in levels {
            for _ in 0..nx * ny {
                z.push(level);
                f.push(level * level);
            }
        }
        let resolver = SyntheticResolver {
            fields: HashMap::from([
                (
                    ("f".to_string(), 0),
                    resolved(
                        "f",
                        f,
                        vec![nz, ny, nx],
                        vec![Axis::Z, Axis::Y, Axis::X],
                        "m^2",
                    ),
                ),
                (
                    ("z".to_string(), 0),
                    resolved(
                        "z",
                        z,
                        vec![nz, ny, nx],
                        vec![Axis::Z, Axis::Y, Axis::X],
                        "m",
                    ),
                ),
            ]),
            grid: grid(nx, ny, Some(nz), GridConvention::Cartesian, None),
            times: HashMap::from([(0, 0.0)]),
        };
        let output = evaluate_source("ddz(f, z)", &resolver);
        for (level, layer) in levels.iter().zip(output.data.chunks(nx * ny)) {
            assert!(
                layer
                    .iter()
                    .all(|value| (*value - 2.0 * level).abs() < 1.0e-10)
            );
        }
    }

    #[test]
    fn vertical_integral_mean_and_range_policy_are_explicit() {
        let (nx, ny, nz) = (2, 2, 3);
        let levels = [0.0, 1.0, 3.0];
        let mut z = Vec::new();
        for level in levels {
            z.extend(std::iter::repeat(level).take(nx * ny));
        }
        let resolver = SyntheticResolver {
            fields: HashMap::from([
                (
                    ("f".to_string(), 0),
                    resolved(
                        "f",
                        z.clone(),
                        vec![nz, ny, nx],
                        vec![Axis::Z, Axis::Y, Axis::X],
                        "m",
                    ),
                ),
                (
                    ("z".to_string(), 0),
                    resolved(
                        "z",
                        z,
                        vec![nz, ny, nx],
                        vec![Axis::Z, Axis::Y, Axis::X],
                        "m",
                    ),
                ),
            ]),
            grid: grid(nx, ny, Some(nz), GridConvention::Cartesian, None),
            times: HashMap::from([(0, 0.0)]),
        };
        let integral = evaluate_source(
            r#"integrate_z(f, z, quantity(0, "m"), quantity(3, "m"))"#,
            &resolver,
        );
        let mean = evaluate_source(
            r#"mean_z(f, z, quantity(0, "m"), quantity(3, "m"))"#,
            &resolver,
        );
        assert!(
            integral
                .data
                .iter()
                .all(|value| (*value - 4.5).abs() < 1.0e-10)
        );
        assert!(mean.data.iter().all(|value| (*value - 1.5).abs() < 1.0e-10));
        let formula = compile(r#"integrate_z(f, z, quantity(-1, "m"), quantity(3, "m"))"#).unwrap();
        assert!(
            formula
                .evaluate(
                    &resolver,
                    &ParameterValues::new(),
                    &EvaluationOptions::default()
                )
                .is_err()
        );
    }

    fn temporal_resolver(times: &[(isize, f64)]) -> SyntheticResolver {
        let mut fields = HashMap::new();
        let mut time_map = HashMap::new();
        for (offset, time) in times {
            fields.insert(
                ("f".to_string(), *offset),
                resolved(
                    "f",
                    vec![*time * *time; 4],
                    vec![2, 2],
                    vec![Axis::Y, Axis::X],
                    "K",
                ),
            );
            time_map.insert(*offset, *time);
        }
        SyntheticResolver {
            fields,
            grid: grid(2, 2, None, GridConvention::Cartesian, None),
            times: time_map,
        }
    }

    #[test]
    fn irregular_three_point_time_derivative_is_exact() {
        for (resolver, expected) in [
            (temporal_resolver(&[(-1, 0.0), (0, 2.0), (1, 5.0)]), 4.0),
            (temporal_resolver(&[(0, 0.0), (1, 2.0), (2, 5.0)]), 0.0),
            (temporal_resolver(&[(-2, 0.0), (-1, 2.0), (0, 5.0)]), 10.0),
        ] {
            let output = evaluate_source("dt(f)", &resolver);
            assert!(
                output
                    .data
                    .iter()
                    .all(|value| (*value - expected).abs() < 1.0e-10)
            );
        }
    }

    #[test]
    fn missing_time_boundary_preserves_derivative_units() {
        let resolver = temporal_resolver(&[(0, 2.0)]);
        let mut options = EvaluationOptions::default();
        options.boundary_policy = BoundaryPolicy::Missing;
        let output = compile("dt(f)")
            .unwrap()
            .evaluate(&resolver, &ParameterValues::new(), &options)
            .unwrap();
        assert_eq!(output.units, "K/s");
        assert!(output.data.iter().all(|value| value.is_nan()));
    }

    #[test]
    fn rejects_case_colliding_unit_overrides() {
        let resolver = SyntheticResolver {
            fields: HashMap::new(),
            grid: grid(2, 2, None, GridConvention::Cartesian, None),
            times: HashMap::from([(0, 0.0)]),
        };
        let mut options = EvaluationOptions::default();
        options
            .variable_unit_overrides
            .insert("x".to_string(), "m".to_string());
        options
            .variable_unit_overrides
            .insert("X".to_string(), "m".to_string());
        let error = compile("1")
            .unwrap()
            .evaluate(&resolver, &ParameterValues::new(), &options)
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unit);
    }

    #[test]
    fn rejects_negating_affine_absolute_temperature() {
        let resolver = SyntheticResolver {
            fields: HashMap::new(),
            grid: grid(2, 2, None, GridConvention::Cartesian, None),
            times: HashMap::from([(0, 0.0)]),
        };
        let error = compile(r#"-quantity(20, "degC")"#)
            .unwrap()
            .evaluate(
                &resolver,
                &ParameterValues::new(),
                &EvaluationOptions::default(),
            )
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unit);
    }
}

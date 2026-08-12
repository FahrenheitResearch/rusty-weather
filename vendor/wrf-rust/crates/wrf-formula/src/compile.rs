use crate::FormulaOutput;
use crate::ast::{BinaryOp, Expr, ExprKind, Program, UnaryOp};
use crate::error::{ErrorKind, FormulaError, FormulaResult};
use crate::eval;
use crate::lexer::lex;
use crate::model::{
    CompileOptions, EvaluationOptions, ExecutionPlan, HeightDatum, ParameterSpec, ParameterValues,
    Requirement,
};
use crate::parser::{measure_program, parse};
use crate::resolver::{FieldResolver, WrfResolver};
use std::collections::{BTreeMap, BTreeSet};
use wrf_core::WrfFile;

/// Compile with conservative defaults.
pub fn compile(source: &str) -> FormulaResult<CompiledFormula> {
    compile_with_options(source, &CompileOptions::default())
}

/// Compile once and evaluate many times or against multiple resolvers.
pub fn compile_with_options(
    source: &str,
    options: &CompileOptions,
) -> FormulaResult<CompiledFormula> {
    validate_parameters(&options.parameters, &options.limits)?;
    let tokens = lex(source, &options.limits)?;
    let program = parse(tokens, &options.limits)?;
    let analysis = analyze(&program, options)?;
    Ok(CompiledFormula {
        source: source.to_string(),
        canonical_source: analysis.canonical_source.clone(),
        program,
        options: options.clone(),
        plan: analysis,
        recipe_name: None,
        recipe_version: None,
        recipe_references: Vec::new(),
        expected_output_units: None,
        recipe_evaluation_options: None,
        recipe_requirements: None,
    })
}

#[derive(Debug, Clone)]
pub struct CompiledFormula {
    source: String,
    canonical_source: String,
    pub(crate) program: Program,
    pub(crate) options: CompileOptions,
    plan: ExecutionPlan,
    pub(crate) recipe_name: Option<String>,
    pub(crate) recipe_version: Option<String>,
    pub(crate) recipe_references: Vec<String>,
    pub(crate) expected_output_units: Option<String>,
    recipe_evaluation_options: Option<EvaluationOptions>,
    pub(crate) recipe_requirements: Option<crate::recipe::RecipeRequirements>,
}

impl CompiledFormula {
    pub fn source(&self) -> &str {
        &self.source
    }

    pub fn canonical_source(&self) -> &str {
        &self.canonical_source
    }

    pub fn plan(&self) -> &ExecutionPlan {
        &self.plan
    }

    pub fn parameters(&self) -> &[ParameterSpec] {
        &self.options.parameters
    }

    pub fn recipe_evaluation_options(&self) -> Option<&EvaluationOptions> {
        self.recipe_evaluation_options.as_ref()
    }

    pub fn evaluate<R: FieldResolver>(
        &self,
        resolver: &R,
        parameters: &ParameterValues,
        options: &EvaluationOptions,
    ) -> FormulaResult<FormulaOutput> {
        eval::evaluate(self, resolver, parameters, options)
    }

    pub fn evaluate_with_recipe_defaults<R: FieldResolver>(
        &self,
        resolver: &R,
        parameters: &ParameterValues,
    ) -> FormulaResult<FormulaOutput> {
        let defaults = self
            .recipe_evaluation_options
            .as_ref()
            .cloned()
            .unwrap_or_default();
        self.evaluate(resolver, parameters, &defaults)
    }

    /// Direct convenience adapter. Formula evaluations are currently globally
    /// serialized because wrf-core's single-time cache is not concurrency-safe
    /// across different time indices; generic resolvers remain independently parallel.
    pub fn evaluate_wrf(
        &self,
        file: &WrfFile,
        time_index: usize,
        parameters: &ParameterValues,
        options: &EvaluationOptions,
    ) -> FormulaResult<FormulaOutput> {
        let _guard = crate::resolver::lock_wrf_evaluation()?;
        let nxy = file.nx.checked_mul(file.ny).ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "WRF horizontal shape overflows usize")
        })?;
        let nxyz = nxy.checked_mul(file.nz).ok_or_else(|| {
            FormulaError::new(ErrorKind::Limit, "WRF volume shape overflows usize")
        })?;
        if nxyz > self.options.limits.max_output_elements {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "WRF volume has {nxyz} elements before diagnostic allocation; formula limit is {}",
                    self.options.limits.max_output_elements
                ),
            ));
        }
        let field_bytes = nxyz
            .checked_mul(std::mem::size_of::<f64>())
            .ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "WRF field byte size overflows usize")
            })?;
        if field_bytes > self.options.limits.max_working_bytes {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "one WRF volume needs {field_bytes} bytes before getvar allocation; per-allocation limit is {}",
                    self.options.limits.max_working_bytes
                ),
            ));
        }
        let mut preflight_fields: BTreeSet<String> = self
            .plan
            .dependencies
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        if let Some(requirements) = &self.recipe_requirements {
            preflight_fields.extend(
                requirements
                    .fields
                    .iter()
                    .map(|name| name.to_ascii_lowercase()),
            );
        }
        let estimated_input_bytes = (field_bytes as u64)
            .checked_mul(preflight_fields.len().max(1) as u64)
            .ok_or_else(|| {
                FormulaError::new(ErrorKind::Limit, "WRF dependency byte estimate overflow")
            })?;
        if estimated_input_bytes > self.options.limits.max_total_allocated_bytes {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "WRF dependency preflight estimates {estimated_input_bytes} bytes; total formula allocation limit is {}",
                    self.options.limits.max_total_allocated_bytes
                ),
            ));
        }
        let resolver = WrfResolver::new(file, time_index)?;
        self.evaluate(&resolver, parameters, options)
    }

    pub fn evaluate_wrf_with_recipe_defaults(
        &self,
        file: &WrfFile,
        time_index: usize,
        parameters: &ParameterValues,
    ) -> FormulaResult<FormulaOutput> {
        let defaults = self
            .recipe_evaluation_options
            .as_ref()
            .cloned()
            .unwrap_or_default();
        self.evaluate_wrf(file, time_index, parameters, &defaults)
    }

    pub(crate) fn with_recipe_metadata(
        mut self,
        name: String,
        version: String,
        references: Vec<String>,
        expected_output_units: Option<String>,
        evaluation_options: EvaluationOptions,
        requirements: crate::recipe::RecipeRequirements,
    ) -> Self {
        self.recipe_name = Some(name);
        self.recipe_version = Some(version);
        self.recipe_references = references;
        self.expected_output_units = expected_output_units;
        self.recipe_evaluation_options = Some(evaluation_options);
        self.recipe_requirements = Some(requirements.clone());
        self.plan.recipe_requirements = Some(requirements);
        self
    }
}

fn collect_live_assignments(
    expr: &Expr,
    assignments: &BTreeMap<String, &Expr>,
    live: &mut BTreeSet<String>,
) {
    match &expr.kind {
        ExprKind::Identifier(name) => {
            let lower = name.to_ascii_lowercase();
            if let Some(value) = assignments.get(&lower) {
                if live.insert(lower) {
                    collect_live_assignments(value, assignments, live);
                }
            }
        }
        ExprKind::Unary { value, .. } => collect_live_assignments(value, assignments, live),
        ExprKind::Binary { left, right, .. } => {
            collect_live_assignments(left, assignments, live);
            collect_live_assignments(right, assignments, live);
        }
        ExprKind::Call { args, .. } => {
            for arg in args {
                collect_live_assignments(arg, assignments, live);
            }
        }
        _ => {}
    }
}

fn validate_parameters(
    parameters: &[ParameterSpec],
    limits: &crate::model::ResourceLimits,
) -> FormulaResult<()> {
    if parameters.len() > limits.max_dependencies {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "{} parameters exceed limit {}",
                parameters.len(),
                limits.max_dependencies
            ),
        ));
    }
    let mut names = BTreeSet::new();
    for parameter in parameters {
        validate_identifier(&parameter.name)
            .map_err(|error| error.note("while validating parameter declaration"))?;
        if parameter.name.len() > limits.max_identifier_bytes {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "parameter '{}' exceeds identifier byte limit {}",
                    parameter.name, limits.max_identifier_bytes
                ),
            ));
        }
        let lower = parameter.name.to_ascii_lowercase();
        if !names.insert(lower.clone()) {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "duplicate parameter '{}' (names are ASCII case-insensitive)",
                    parameter.name
                ),
            ));
        }
        if is_reserved(&lower) {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "parameter '{}' shadows a function or reserved constant",
                    parameter.name
                ),
            ));
        }
        if wrf_core::variables::get_var_def(&parameter.name).is_some() {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "parameter '{}' shadows a registered WRF diagnostic",
                    parameter.name
                ),
            ));
        }
        if !parameter.default.is_finite()
            || parameter.minimum.is_some_and(|value| !value.is_finite())
            || parameter.maximum.is_some_and(|value| !value.is_finite())
        {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!(
                    "parameter '{}' bounds/default must be finite",
                    parameter.name
                ),
            ));
        }
        if let (Some(minimum), Some(maximum)) = (parameter.minimum, parameter.maximum) {
            if minimum > maximum {
                return Err(FormulaError::new(
                    ErrorKind::Parameter,
                    format!("parameter '{}' minimum exceeds maximum", parameter.name),
                ));
            }
        }
        if parameter
            .minimum
            .is_some_and(|minimum| parameter.default < minimum)
            || parameter
                .maximum
                .is_some_and(|maximum| parameter.default > maximum)
        {
            return Err(FormulaError::new(
                ErrorKind::Parameter,
                format!("parameter '{}' default lies outside bounds", parameter.name),
            ));
        }
        crate::parse_unit(&parameter.units).map_err(|error| {
            FormulaError::new(
                ErrorKind::Parameter,
                format!("parameter '{}' has invalid units: {error}", parameter.name),
            )
        })?;
    }
    Ok(())
}

fn analyze(program: &Program, options: &CompileOptions) -> FormulaResult<ExecutionPlan> {
    let parameter_names: BTreeSet<String> = options
        .parameters
        .iter()
        .map(|value| value.name.to_ascii_lowercase())
        .collect();
    let mut assignment_positions = BTreeMap::new();
    for (index, assignment) in program.assignments.iter().enumerate() {
        validate_identifier(&assignment.name).map_err(|error| error.at(assignment.span))?;
        let lower = assignment.name.to_ascii_lowercase();
        if parameter_names.contains(&lower) || is_reserved(&lower) {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!(
                    "assignment '{}' shadows a parameter, function, or reserved constant",
                    assignment.name
                ),
            )
            .at(assignment.span));
        }
        if wrf_core::variables::get_var_def(&assignment.name).is_some() {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!(
                    "assignment '{}' shadows a registered WRF diagnostic",
                    assignment.name
                ),
            )
            .at(assignment.span));
        }
        if assignment_positions.insert(lower, index).is_some() {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!(
                    "duplicate assignment '{}' (names are ASCII case-insensitive)",
                    assignment.name
                ),
            )
            .at(assignment.span));
        }
    }

    let assignment_expressions: BTreeMap<String, &Expr> = program
        .assignments
        .iter()
        .map(|assignment| (assignment.name.to_ascii_lowercase(), &assignment.value))
        .collect();
    let mut live_assignments = BTreeSet::new();
    collect_live_assignments(
        &program.output,
        &assignment_expressions,
        &mut live_assignments,
    );
    for assignment in &program.assignments {
        if !live_assignments.contains(&assignment.name.to_ascii_lowercase()) {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                format!(
                    "assignment '{}' is never used by the final expression",
                    assignment.name
                ),
            )
            .at(assignment.span));
        }
    }

    let mut assignment_contains_dt = BTreeMap::new();
    for assignment in &program.assignments {
        validate_no_nested_dt(&assignment.value, &assignment_contains_dt)?;
        let contains = expanded_contains_dt(&assignment.value, &assignment_contains_dt);
        assignment_contains_dt.insert(assignment.name.to_ascii_lowercase(), contains);
    }
    validate_no_nested_dt(&program.output, &assignment_contains_dt)?;

    let mut state = AnalysisState::default();
    for (index, assignment) in program.assignments.iter().enumerate() {
        analyze_expr(
            &assignment.value,
            Some(index),
            &assignment_positions,
            &parameter_names,
            &mut state,
        )?;
    }
    analyze_expr(
        &program.output,
        Some(program.assignments.len()),
        &assignment_positions,
        &parameter_names,
        &mut state,
    )?;
    if state.dependencies.len() > options.limits.max_dependencies {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!(
                "formula has {} field dependencies; limit is {}",
                state.dependencies.len(),
                options.limits.max_dependencies
            ),
        ));
    }
    for dependency in &state.dependencies {
        state.requirements.insert(Requirement::Field {
            name: dependency.clone(),
        });
    }
    let (ast_nodes, ast_depth) = measure_program(program);
    Ok(ExecutionPlan {
        canonical_source: canonical_program(program),
        dependencies: state.dependencies.into_iter().collect(),
        functions: state.functions.into_iter().collect(),
        assignments: program
            .assignments
            .iter()
            .map(|assignment| assignment.name.clone())
            .collect(),
        requirements: state.requirements.into_iter().collect(),
        ast_nodes,
        ast_depth,
        recipe_requirements: None,
    })
}

fn validate_no_nested_dt(
    expr: &Expr,
    assignment_contains_dt: &BTreeMap<String, bool>,
) -> FormulaResult<()> {
    match &expr.kind {
        ExprKind::Call { name, args } => {
            if name.eq_ignore_ascii_case("dt")
                && args
                    .first()
                    .is_some_and(|argument| expanded_contains_dt(argument, assignment_contains_dt))
            {
                return Err(FormulaError::new(
                    ErrorKind::Unsupported,
                    "nested dt is intentionally unsupported in v1, including through assignment aliases",
                )
                .at(expr.span));
            }
            for arg in args {
                validate_no_nested_dt(arg, assignment_contains_dt)?;
            }
        }
        ExprKind::Unary { value, .. } => validate_no_nested_dt(value, assignment_contains_dt)?,
        ExprKind::Binary { left, right, .. } => {
            validate_no_nested_dt(left, assignment_contains_dt)?;
            validate_no_nested_dt(right, assignment_contains_dt)?;
        }
        _ => {}
    }
    Ok(())
}

fn expanded_contains_dt(expr: &Expr, assignment_contains_dt: &BTreeMap<String, bool>) -> bool {
    match &expr.kind {
        ExprKind::Identifier(name) => assignment_contains_dt
            .get(&name.to_ascii_lowercase())
            .copied()
            .unwrap_or(false),
        ExprKind::Unary { value, .. } => expanded_contains_dt(value, assignment_contains_dt),
        ExprKind::Binary { left, right, .. } => {
            expanded_contains_dt(left, assignment_contains_dt)
                || expanded_contains_dt(right, assignment_contains_dt)
        }
        ExprKind::Call { name, args } => {
            name.eq_ignore_ascii_case("dt")
                || args
                    .iter()
                    .any(|arg| expanded_contains_dt(arg, assignment_contains_dt))
        }
        _ => false,
    }
}

#[derive(Default)]
struct AnalysisState {
    dependencies: BTreeSet<String>,
    functions: BTreeSet<String>,
    requirements: BTreeSet<Requirement>,
}

fn analyze_expr(
    expr: &Expr,
    assignment_index: Option<usize>,
    assignments: &BTreeMap<String, usize>,
    parameters: &BTreeSet<String>,
    state: &mut AnalysisState,
) -> FormulaResult<()> {
    match &expr.kind {
        ExprKind::Number(_) => {}
        ExprKind::Text(_) => return Err(FormulaError::new(
            ErrorKind::Compile,
            "string literals are only permitted as the unit argument of quantity() or convert()",
        )
        .at(expr.span)),
        ExprKind::Identifier(name) => {
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(), "pi" | "e" | "true" | "false")
                || parameters.contains(&lower)
            {
                return Ok(());
            }
            if let Some(position) = assignments.get(&lower) {
                if assignment_index.is_some_and(|current| *position >= current) {
                    return Err(FormulaError::new(
                        ErrorKind::Compile,
                        format!("assignment '{name}' is referenced before it is defined"),
                    )
                    .at(expr.span));
                }
            } else {
                state.dependencies.insert(name.clone());
            }
        }
        ExprKind::Unary { value, .. } => {
            analyze_expr(value, assignment_index, assignments, parameters, state)?
        }
        ExprKind::Binary { left, right, .. } => {
            analyze_expr(left, assignment_index, assignments, parameters, state)?;
            analyze_expr(right, assignment_index, assignments, parameters, state)?;
        }
        ExprKind::Call { name, args } => {
            let lower = name.to_ascii_lowercase();
            if lower == "dt"
                && args
                    .first()
                    .is_some_and(|argument| contains_call(argument, "dt"))
            {
                return Err(FormulaError::new(
                    ErrorKind::Unsupported,
                    "nested dt is intentionally unsupported in v1; only first temporal derivatives are allowed",
                )
                .at(expr.span));
            }
            if is_unsupported_global(&lower) {
                return Err(FormulaError::new(
                    ErrorKind::Unsupported,
                    format!("'{name}' is a global/iterative operation and is intentionally unavailable in arbitrary expressions; implement it as a reviewed native kernel with explicit boundary conditions"),
                )
                .at(expr.span));
            }
            let (minimum, maximum) = function_arity(&lower).ok_or_else(|| {
                FormulaError::new(
                    ErrorKind::UnknownFunction,
                    format!("unknown function '{name}'"),
                )
                .at(expr.span)
            })?;
            if args.len() < minimum || args.len() > maximum {
                return Err(FormulaError::new(
                    ErrorKind::Arity,
                    if minimum == maximum {
                        format!(
                            "function '{name}' requires {minimum} arguments, got {}",
                            args.len()
                        )
                    } else {
                        format!(
                            "function '{name}' requires {minimum}..={maximum} arguments, got {}",
                            args.len()
                        )
                    },
                )
                .at(expr.span));
            }
            state.functions.insert(lower.clone());
            add_requirements(&lower, args.len(), &mut state.requirements);
            for (index, arg) in args.iter().enumerate() {
                let unit_string = matches!(lower.as_str(), "quantity" | "convert") && index == 1;
                if unit_string {
                    match &arg.kind {
                        ExprKind::Text(unit) => {
                            crate::parse_unit(unit).map_err(|error| {
                                FormulaError::new(
                                    ErrorKind::Unit,
                                    format!("invalid unit '{unit}': {error}"),
                                )
                                .at(arg.span)
                            })?;
                        }
                        _ => {
                            return Err(FormulaError::new(
                                ErrorKind::Compile,
                                "unit argument must be a string literal",
                            )
                            .at(arg.span));
                        }
                    }
                } else {
                    analyze_expr(arg, assignment_index, assignments, parameters, state)?;
                }
            }
        }
    }
    Ok(())
}

fn contains_call(expr: &Expr, target: &str) -> bool {
    match &expr.kind {
        ExprKind::Unary { value, .. } => contains_call(value, target),
        ExprKind::Binary { left, right, .. } => {
            contains_call(left, target) || contains_call(right, target)
        }
        ExprKind::Call { name, args } => {
            name.eq_ignore_ascii_case(target) || args.iter().any(|arg| contains_call(arg, target))
        }
        _ => false,
    }
}

fn add_requirements(name: &str, arity: usize, requirements: &mut BTreeSet<Requirement>) {
    if matches!(name, "ddx" | "ddy" | "grad" | "div" | "curl" | "laplacian") {
        requirements.insert(Requirement::MassMapFactor);
    }
    if matches!(name, "div" | "curl") {
        requirements.insert(Requirement::GridProjectedVector);
    }
    if name == "ddz" {
        requirements.insert(Requirement::PhysicalHeight {
            datum: if arity == 1 {
                HeightDatum::ResolverDefault
            } else {
                HeightDatum::ExplicitField
            },
        });
    }
    if matches!(name, "integrate_z" | "mean_z" | "interpolate_z") {
        requirements.insert(Requirement::PhysicalHeight {
            datum: HeightDatum::ExplicitField,
        });
    }
    if name == "dt" {
        requirements.insert(Requirement::AdjacentTimes);
    }
}

pub(crate) fn function_arity(name: &str) -> Option<(usize, usize)> {
    match name {
        "where" | "clamp" => Some((3, 3)),
        "min" | "max" | "atan2" | "pow" | "quantity" | "convert" | "dot" | "component" => {
            Some((2, 2))
        }
        "abs" | "sqrt" | "exp" | "ln" | "log" | "log10" | "sin" | "cos" | "tan" | "asin"
        | "acos" | "atan" | "floor" | "ceil" | "round" | "is_finite" | "magnitude" | "div"
        | "curl" | "dt" | "dbz_to_z" | "z_to_dbz" => Some((1, 1)),
        "grid_vector" | "earth_vector" => Some((2, 3)),
        "ddx" | "ddy" => Some((1, 1)),
        "ddz" => Some((1, 2)),
        "grad" | "laplacian" => Some((1, 1)),
        "integrate_z" | "mean_z" => Some((4, 4)),
        "interpolate_z" => Some((3, 3)),
        _ => None,
    }
}

fn is_unsupported_global(name: &str) -> bool {
    matches!(
        name,
        "solve"
            | "solve_poisson"
            | "poisson"
            | "fft"
            | "iterate"
            | "loop"
            | "while"
            | "eval"
            | "exec"
            | "import"
    )
}

fn is_reserved(name: &str) -> bool {
    function_arity(name).is_some()
        || is_unsupported_global(name)
        || matches!(name, "pi" | "e" | "true" | "false")
}

fn validate_identifier(name: &str) -> FormulaResult<()> {
    let mut chars = name.chars();
    let first = chars
        .next()
        .ok_or_else(|| FormulaError::new(ErrorKind::Compile, "identifier cannot be empty"))?;
    if !(first.is_ascii_alphabetic() || first == '_')
        || chars.any(|character| !(character.is_ascii_alphanumeric() || character == '_'))
    {
        return Err(FormulaError::new(
            ErrorKind::Compile,
            format!("invalid identifier '{name}'"),
        ));
    }
    Ok(())
}

fn canonical_program(program: &Program) -> String {
    let mut output = String::new();
    for assignment in &program.assignments {
        output.push_str(&assignment.name);
        output.push_str(" = ");
        canonical_expr(&assignment.value, &mut output);
        output.push_str(";\n");
    }
    canonical_expr(&program.output, &mut output);
    output
}

fn canonical_expr(expr: &Expr, output: &mut String) {
    match &expr.kind {
        ExprKind::Number(value) => output.push_str(&format!("{value:?}")),
        ExprKind::Text(value) => {
            output.push('"');
            for character in value.chars() {
                match character {
                    '\\' => output.push_str("\\\\"),
                    '"' => output.push_str("\\\""),
                    '\n' => output.push_str("\\n"),
                    '\t' => output.push_str("\\t"),
                    other => output.push(other),
                }
            }
            output.push('"');
        }
        ExprKind::Identifier(name) => output.push_str(name),
        ExprKind::Unary { op, value } => {
            output.push('(');
            output.push_str(match op {
                UnaryOp::Neg => "-",
                UnaryOp::Pos => "+",
                UnaryOp::Not => "not ",
            });
            canonical_expr(value, output);
            output.push(')');
        }
        ExprKind::Binary { op, left, right } => {
            output.push('(');
            canonical_expr(left, output);
            output.push_str(match op {
                BinaryOp::Add => " + ",
                BinaryOp::Sub => " - ",
                BinaryOp::Mul => " * ",
                BinaryOp::Div => " / ",
                BinaryOp::Pow => " ^ ",
                BinaryOp::Eq => " == ",
                BinaryOp::NotEq => " != ",
                BinaryOp::Less => " < ",
                BinaryOp::LessEq => " <= ",
                BinaryOp::Greater => " > ",
                BinaryOp::GreaterEq => " >= ",
                BinaryOp::And => " and ",
                BinaryOp::Or => " or ",
            });
            canonical_expr(right, output);
            output.push(')');
        }
        ExprKind::Call { name, args } => {
            output.push_str(&name.to_ascii_lowercase());
            output.push('(');
            for (index, arg) in args.iter().enumerate() {
                if index > 0 {
                    output.push_str(", ");
                }
                canonical_expr(arg, output);
            }
            output.push(')');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_dependencies_and_requirements() {
        let formula = compile("v = ddx(temp)\nv * quantity(1.0, \"s\")").unwrap();
        assert_eq!(formula.plan.dependencies, vec!["temp"]);
        assert!(
            formula
                .plan
                .requirements
                .contains(&Requirement::MassMapFactor)
        );
    }

    #[test]
    fn rejects_forward_assignment_reference() {
        assert!(compile("a = b + 1\nb = 2\na").is_err());
    }

    #[test]
    fn rejects_global_solver_in_expression() {
        let error = compile("solve_poisson(temp)").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn rejects_nested_temporal_derivatives() {
        let error = compile("dt(dt(temp))").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Unsupported);
    }

    #[test]
    fn rejects_nested_temporal_derivative_through_assignments() {
        assert!(compile("a = dt(temp)\ndt(a)").is_err());
        assert!(compile("a = dt(temp)\nb = a\ndt(b)").is_err());
    }

    #[test]
    fn rejects_dead_assignments_instead_of_planning_unreachable_work() {
        let error = compile("unused = dt(missing_field)\n1").unwrap_err();
        assert_eq!(error.kind, ErrorKind::Compile);
    }
}

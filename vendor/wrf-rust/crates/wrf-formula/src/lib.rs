//! A deterministic, sandboxed expression engine for custom WRF diagnostics.
//!
//! Formulae may perform unit-checked algebra and local differential operations,
//! but cannot access the filesystem, network, shell, or arbitrary native code.
//! Iterative/global solvers (for example a Poisson solve) intentionally remain
//! outside the expression language and must be implemented as reviewed kernels.

mod ast;
mod compile;
mod error;
mod eval;
mod lexer;
mod model;
mod parser;
mod recipe;
mod resolver;
mod units;

pub use compile::{CompiledFormula, compile, compile_with_options};
pub use error::{ErrorKind, FormulaError, FormulaResult, Span};
pub use model::{
    Axis, BoundaryPolicy, CompileOptions, EvaluationOptions, ExecutionPlan, FormulaOutput,
    FormulaProvenance, GridConvention, GridLocation, HeightDatum, MissingPolicy, NonFinitePolicy,
    ParameterSpec, ParameterValues, Requirement, ResourceLimits, VectorBasis,
};
pub use recipe::{MAX_RECIPE_BYTES, Recipe, RecipeReference, RecipeRequirements};
pub use resolver::{FieldRequest, FieldResolver, GridMetadata, ResolvedField};
pub use units::{Dimension, TemperatureKind, Unit, parse_unit};

/// Expression language and provenance schema version.
pub const ENGINE_VERSION: &str = env!("CARGO_PKG_VERSION");

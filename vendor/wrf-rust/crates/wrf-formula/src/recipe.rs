use crate::compile::{CompiledFormula, compile_with_options};
use crate::error::{ErrorKind, FormulaError, FormulaResult};
use crate::model::{CompileOptions, EvaluationOptions, ParameterSpec, ResourceLimits};
use serde::{Deserialize, Serialize};
use std::io::Read;

/// Maximum input accepted by the canonical bounded JSON loaders.
pub const MAX_RECIPE_BYTES: usize = 1024 * 1024;

fn schema_id() -> String {
    "wrf-formula/v1".to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeReference {
    #[serde(default)]
    pub citation: String,
    pub doi: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeRequirements {
    /// Additional fields required by the scientific method but not necessarily
    /// referenced directly by the expression (for preflight/user guidance).
    #[serde(default)]
    pub fields: Vec<String>,
    pub maximum_cadence_seconds: Option<f64>,
    pub maximum_horizontal_spacing_m: Option<f64>,
    pub minimum_vertical_levels: Option<usize>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Portable custom diagnostic definition. Direct Serde callers must impose
/// their own byte limit; untrusted JSON should use the bounded loader methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub schema: String,
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default)]
    pub references: Vec<RecipeReference>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub parameters: Vec<ParameterSpec>,
    pub expected_output_units: Option<String>,
    #[serde(default)]
    pub requirements: RecipeRequirements,
    #[serde(default)]
    pub evaluation_options: EvaluationOptions,
    /// Optional stricter per-recipe ceilings. Every value must be no greater
    /// than the engine's immutable host defaults.
    pub resource_limits: Option<ResourceLimits>,
}

impl Recipe {
    /// Parse and fully validate canonical JSON under a fixed input byte limit.
    pub fn from_json_bytes(bytes: &[u8]) -> FormulaResult<Self> {
        if bytes.len() > MAX_RECIPE_BYTES {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                format!(
                    "recipe JSON has {} bytes; limit is {MAX_RECIPE_BYTES}",
                    bytes.len()
                ),
            ));
        }
        let recipe: Self = serde_json::from_slice(bytes).map_err(|error| {
            FormulaError::new(ErrorKind::Parse, format!("invalid recipe JSON: {error}"))
        })?;
        let _ = recipe.compile()?;
        Ok(recipe)
    }

    /// Read at most MAX_RECIPE_BYTES plus one sentinel byte, then parse.
    pub fn from_json_reader<R: Read>(reader: R) -> FormulaResult<Self> {
        let limit = u64::try_from(MAX_RECIPE_BYTES).map_err(|_| {
            FormulaError::new(ErrorKind::Internal, "recipe byte limit does not fit u64")
        })?;
        let mut bounded = reader.take(limit + 1);
        let mut bytes = Vec::new();
        bounded.read_to_end(&mut bytes).map_err(|error| {
            FormulaError::new(
                ErrorKind::Parse,
                format!("failed reading recipe JSON: {error}"),
            )
        })?;
        Self::from_json_bytes(&bytes)
    }

    pub fn compile(&self) -> FormulaResult<CompiledFormula> {
        if self.schema != schema_id() {
            return Err(FormulaError::new(
                ErrorKind::Unsupported,
                format!(
                    "recipe schema '{}' is not supported; this engine supports '{}'",
                    self.schema,
                    schema_id()
                ),
            ));
        }
        if self.name.trim().is_empty() || self.version.trim().is_empty() {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                "recipe name and version cannot be empty",
            ));
        }
        for reference in &self.references {
            let empty = reference.citation.trim().is_empty()
                && reference
                    .doi
                    .as_ref()
                    .map_or(true, |value| value.trim().is_empty())
                && reference
                    .url
                    .as_ref()
                    .map_or(true, |value| value.trim().is_empty());
            if empty {
                return Err(FormulaError::new(
                    ErrorKind::Compile,
                    "each recipe reference must include a citation, DOI, or URL",
                ));
            }
        }
        let mut requirement_fields = std::collections::BTreeSet::new();
        for field in &self.requirements.fields {
            let mut chars = field.chars();
            let valid = chars
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if !valid || field.len() > 128 {
                return Err(FormulaError::new(
                    ErrorKind::Compile,
                    format!("invalid required field name '{field}'"),
                ));
            }
            if !requirement_fields.insert(field.to_ascii_lowercase()) {
                return Err(FormulaError::new(
                    ErrorKind::Compile,
                    format!("duplicate required field '{field}'"),
                ));
            }
        }
        if self.evaluation_options.variable_unit_overrides.len() > 1024 {
            return Err(FormulaError::new(
                ErrorKind::Limit,
                "recipe has more than 1024 variable unit overrides",
            ));
        }
        let mut override_names = std::collections::BTreeSet::new();
        for (name, units) in &self.evaluation_options.variable_unit_overrides {
            let mut chars = name.chars();
            let valid_name = chars
                .next()
                .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_')
                && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
            if !valid_name || name.len() > 128 {
                return Err(FormulaError::new(
                    ErrorKind::Compile,
                    format!("invalid variable unit override name '{name}'"),
                ));
            }
            if !override_names.insert(name.to_ascii_lowercase()) {
                return Err(FormulaError::new(
                    ErrorKind::Compile,
                    format!("duplicate variable unit override '{name}'"),
                ));
            }
            if units.trim().is_empty() || units.len() > 256 {
                return Err(FormulaError::new(
                    ErrorKind::Unit,
                    format!("invalid unit override text for '{name}'"),
                ));
            }
            crate::parse_unit(units).map_err(|error| {
                FormulaError::new(
                    ErrorKind::Unit,
                    format!("invalid unit override for '{name}': {error}"),
                )
            })?;
        }
        if self
            .requirements
            .maximum_cadence_seconds
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
            || self
                .requirements
                .maximum_horizontal_spacing_m
                .is_some_and(|value| !value.is_finite() || value <= 0.0)
            || self.requirements.minimum_vertical_levels == Some(0)
        {
            return Err(FormulaError::new(
                ErrorKind::Compile,
                "recipe resolution/cadence requirements must be finite and positive",
            ));
        }
        if let Some(units) = &self.expected_output_units {
            crate::parse_unit(units).map_err(|error| {
                FormulaError::new(
                    ErrorKind::Unit,
                    format!("invalid expected output units '{units}': {error}"),
                )
            })?;
        }
        validate_metadata_bounds(self)?;
        // Portable recipes never control host resource ceilings. Applications
        // may call compile_with_options separately with ceilings no weaker than
        // their own trusted policy.
        let limits = match &self.resource_limits {
            Some(requested) => validate_requested_limits(requested)?,
            None => ResourceLimits::default(),
        };
        let options = CompileOptions {
            parameters: self.parameters.clone(),
            limits,
        };
        let references = self.references.iter().map(reference_text).collect();
        Ok(
            compile_with_options(&self.source, &options)?.with_recipe_metadata(
                self.name.clone(),
                self.version.clone(),
                references,
                self.expected_output_units.clone(),
                self.evaluation_options.clone(),
                self.requirements.clone(),
            ),
        )
    }
}

fn validate_requested_limits(requested: &ResourceLimits) -> FormulaResult<ResourceLimits> {
    let host = ResourceLimits::default();
    macro_rules! no_greater {
        ($field:ident) => {
            if requested.$field == 0 {
                return Err(FormulaError::new(
                    ErrorKind::Limit,
                    format!(
                        "recipe resource_limits.{} must be positive",
                        stringify!($field)
                    ),
                ));
            }
            if requested.$field > host.$field {
                return Err(FormulaError::new(
                    ErrorKind::Limit,
                    format!(
                        "recipe resource_limits.{}={} exceeds immutable host ceiling {}",
                        stringify!($field),
                        requested.$field,
                        host.$field
                    ),
                ));
            }
        };
    }
    no_greater!(max_source_bytes);
    no_greater!(max_tokens);
    no_greater!(max_ast_nodes);
    no_greater!(max_ast_depth);
    no_greater!(max_identifier_bytes);
    no_greater!(max_function_arity);
    no_greater!(max_assignments);
    no_greater!(max_dependencies);
    no_greater!(max_output_elements);
    no_greater!(max_working_bytes);
    no_greater!(max_total_allocated_bytes);
    no_greater!(max_operations);
    Ok(requested.clone())
}

fn validate_metadata_bounds(recipe: &Recipe) -> FormulaResult<()> {
    const MAX_METADATA_ITEMS: usize = 1024;
    const MAX_METADATA_BYTES: usize = 256 * 1024;
    let item_count = recipe.authors.len()
        + recipe.references.len()
        + recipe.tags.len()
        + recipe.parameters.len()
        + recipe.requirements.fields.len()
        + recipe.requirements.notes.len()
        + recipe.evaluation_options.variable_unit_overrides.len();
    if item_count > MAX_METADATA_ITEMS {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!("recipe has {item_count} metadata items; limit is {MAX_METADATA_ITEMS}"),
        ));
    }
    let mut bytes = recipe.schema.len()
        + recipe.name.len()
        + recipe.version.len()
        + recipe.description.len()
        + recipe.source.len()
        + recipe.expected_output_units.as_ref().map_or(0, String::len)
        + match recipe.evaluation_options.boundary_policy {
            crate::BoundaryPolicy::OneSidedSecondOrder => "one_sided_second_order".len(),
            crate::BoundaryPolicy::Missing => "missing".len(),
            crate::BoundaryPolicy::Error => "error".len(),
        }
        + match recipe.evaluation_options.missing_policy {
            crate::MissingPolicy::Propagate => "propagate".len(),
            crate::MissingPolicy::Error => "error".len(),
            crate::MissingPolicy::IgnoreInReductions => "ignore_in_reductions".len(),
        }
        + match recipe.evaluation_options.non_finite_policy {
            crate::NonFinitePolicy::Propagate => "propagate".len(),
            crate::NonFinitePolicy::Error => "error".len(),
        };
    for value in recipe
        .authors
        .iter()
        .chain(recipe.tags.iter())
        .chain(recipe.requirements.fields.iter())
        .chain(recipe.requirements.notes.iter())
    {
        bytes = bytes.saturating_add(value.len());
    }
    for reference in &recipe.references {
        bytes = bytes.saturating_add(reference.citation.len());
        bytes = bytes.saturating_add(reference.doi.as_ref().map_or(0, String::len));
        bytes = bytes.saturating_add(reference.url.as_ref().map_or(0, String::len));
    }
    for parameter in &recipe.parameters {
        bytes = bytes
            .saturating_add(parameter.name.len())
            .saturating_add(parameter.units.len())
            .saturating_add(parameter.description.len());
    }
    for (name, units) in &recipe.evaluation_options.variable_unit_overrides {
        bytes = bytes.saturating_add(name.len()).saturating_add(units.len());
    }
    if bytes > MAX_METADATA_BYTES {
        return Err(FormulaError::new(
            ErrorKind::Limit,
            format!("recipe metadata/source has {bytes} bytes; limit is {MAX_METADATA_BYTES}"),
        ));
    }
    Ok(())
}

fn reference_text(reference: &RecipeReference) -> String {
    let mut parts = Vec::new();
    if !reference.citation.trim().is_empty() {
        parts.push(reference.citation.trim().to_string());
    }
    if let Some(doi) = &reference.doi {
        parts.push(format!("doi:{doi}"));
    }
    if let Some(url) = &reference.url {
        parts.push(url.clone());
    }
    parts.join("; ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_json_loader_accepts_canonical_minimal_recipe() {
        let json = br#"{
            "schema":"wrf-formula/v1",
            "name":"scalar",
            "version":"1",
            "source":"1",
            "parameters":[]
        }"#;
        let recipe = Recipe::from_json_bytes(json).unwrap();
        assert_eq!(recipe.name, "scalar");
    }

    #[test]
    fn bounded_json_loader_rejects_oversized_input_before_parsing() {
        let bytes = vec![b' '; MAX_RECIPE_BYTES + 1];
        let error = Recipe::from_json_bytes(&bytes).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Limit);
    }

    #[test]
    fn bounded_json_loader_rejects_case_colliding_override_keys() {
        let json = br#"{
            "schema":"wrf-formula/v1",
            "name":"duplicate",
            "version":"1",
            "source":"1",
            "parameters":[],
            "evaluation_options":{
                "variable_unit_overrides":{"T2":"K","t2":"K"}
            }
        }"#;
        let error = Recipe::from_json_bytes(json).unwrap_err();
        assert_eq!(error.kind, ErrorKind::Parse);
    }
}

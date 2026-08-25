//! Conservative temporal capability descriptions for stored variables.
//!
//! These descriptions are catalog hints, not an execution-time inference
//! mechanism. The temporal engine still requires callers to supply explicit
//! semantics and a compatible reducer. Unknown or ambiguous fields remain
//! available for raw point/window access but are marked manual-only here.

use std::collections::BTreeMap;

use rw_store::format::RwsVariableMeta;
use serde::{Deserialize, Serialize};

use crate::{IntervalSupport, TemporalReducer, TemporalSemantics};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalValueClass {
    InstantaneousScalar,
    IntervalAccumulation,
    CumulativeAccumulation,
    Rate,
    VectorComponent,
    CircularDirection,
    Categorical,
    /// A fixed-window maximum, such as a trailing one-hour wind maximum.
    IntervalExtremum,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalCapabilityBasis {
    CanonicalSelector,
    CanonicalSelectorAndName,
    BuiltInDerivedVariable,
    NameAndUnits,
    UnsupportedVariableKind,
    ManualRequired,
}

/// Individual operations a client may safely present for this variable.
/// Names distinguish raw scalar extrema from interval/increment extrema so a
/// UI cannot accidentally advertise `range` for an accumulation or angle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TemporalOperation {
    ScalarMinimum,
    ScalarMaximum,
    ScalarRange,
    TimeWeightedMean,
    ArgMinimumTime,
    ArgMaximumTime,
    IntervalTotal,
    MinimumIntervalAmount,
    MaximumIntervalAmount,
    RangeIntervalAmount,
    MinimumOfIntervalMaxima,
    MaximumOfIntervalMaxima,
    RangeOfIntervalMaxima,
    ArgMinimumIntervalMaximumTime,
    ArgMaximumIntervalMaximumTime,
    TotalIncrement,
    MinimumIncrement,
    MaximumIncrement,
    RangeIncrement,
    MinimumRate,
    MaximumRate,
    RangeRate,
    DurationWeightedMean,
    Integral,
    MinimumVectorSpeed,
    MaximumVectorSpeed,
    RangeVectorSpeed,
    TimeWeightedMeanSpeed,
    VectorMean,
    CircularMean,
    CategoryMode,
    CategoryDuration,
    CategoryTransitions,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VariableTemporalCapability {
    pub value_class: TemporalValueClass,
    pub basis: TemporalCapabilityBasis,
    /// A fully specified, conservative semantics value suitable for a client
    /// default. `None` means the caller must supply trusted metadata.
    pub recommended_semantics: Option<TemporalSemantics>,
    /// Whole reducer responses whose complete output is scientifically valid.
    pub supported_reducers: Vec<TemporalReducer>,
    pub operations: Vec<TemporalOperation>,
    /// Ordered variable list required by the reducer. Vector summaries always
    /// list eastward U first and northward V second.
    pub required_variables: Vec<String>,
    pub requires_manual_semantics: bool,
    pub note: String,
}

impl VariableTemporalCapability {
    pub fn supports(&self, operation: TemporalOperation) -> bool {
        self.operations.contains(&operation)
    }

    fn unknown(basis: TemporalCapabilityBasis, note: impl Into<String>) -> Self {
        Self {
            value_class: TemporalValueClass::Unknown,
            basis,
            recommended_semantics: None,
            supported_reducers: Vec::new(),
            operations: Vec::new(),
            required_variables: Vec::new(),
            requires_manual_semantics: true,
            note: note.into(),
        }
    }
}

/// Describe one variable in isolation. A U/V component remains manual-only
/// here because its companion cannot be verified. Catalog construction should
/// use [`variable_temporal_capabilities`] for an inventory-aware result.
pub fn variable_temporal_capability(meta: &RwsVariableMeta) -> VariableTemporalCapability {
    variable_temporal_capabilities(std::slice::from_ref(meta))
        .remove(&meta.name)
        .expect("one input metadata record produces one capability")
}

/// Describe a complete variable inventory and verify vector companions.
pub fn variable_temporal_capabilities(
    variables: &[RwsVariableMeta],
) -> BTreeMap<String, VariableTemporalCapability> {
    let inventory: BTreeMap<&str, &RwsVariableMeta> = variables
        .iter()
        .map(|meta| (meta.name.as_str(), meta))
        .collect();
    variables
        .iter()
        .map(|meta| {
            let mut capability = classify_base(meta);
            if capability.value_class == TemporalValueClass::VectorComponent {
                finish_vector_capability(meta, &inventory, &mut capability);
            }
            (meta.name.clone(), capability)
        })
        .collect()
}

fn classify_base(meta: &RwsVariableMeta) -> VariableTemporalCapability {
    if meta.kind != "surface2d" && meta.kind != "pressure3d" {
        return VariableTemporalCapability::unknown(
            TemporalCapabilityBasis::UnsupportedVariableKind,
            format!(
                "temporal-grid reducers do not support variable kind '{}' for '{}'",
                meta.kind, meta.name
            ),
        );
    }

    let name = normalize(&meta.name);
    let field = selector_field(&meta.selector);
    let product = selector_product(&meta.selector);

    if meta.kind == "pressure3d" {
        if matches!(field.as_deref(), Some("uwind" | "vwind")) {
            return VariableTemporalCapability {
                value_class: TemporalValueClass::VectorComponent,
                basis: TemporalCapabilityBasis::CanonicalSelector,
                recommended_semantics: None,
                supported_reducers: Vec::new(),
                operations: Vec::new(),
                required_variables: Vec::new(),
                requires_manual_semantics: true,
                note: "the pressure-level U/V companion must be present with matching units and an identical pressure axis before vector reduction is enabled".into(),
            };
        }
        if field
            .as_deref()
            .is_some_and(is_canonical_pressure_scalar_field)
        {
            return scalar_capability(
                TemporalCapabilityBasis::CanonicalSelector,
                "the canonical pressure-level selector identifies an instantaneous continuous scalar; each requested pressure level is reduced independently",
                &meta.name,
            );
        }
        return VariableTemporalCapability::unknown(
            TemporalCapabilityBasis::ManualRequired,
            "this pressure-volume selector is not one of the canonical temperature, dewpoint, relative-humidity, or geopotential-height fields; explicit trusted semantics are required",
        );
    }

    if let Some(seconds) = provider_published_statistic_window_seconds(&name) {
        return VariableTemporalCapability::unknown(
            TemporalCapabilityBasis::ManualRequired,
            format!(
                "the variable name preserves a trailing {seconds}-second provider-published ensemble-statistic window, but the canonical selector has no interval/statistical-process dimension; raw point/window query is supported while temporal reduction requires explicit trusted semantics"
            ),
        );
    }

    // A probability is a scalar probability at each valid time even when the
    // thresholded physical field is itself accumulated or categorical.
    if product.as_deref() == Some("probability") || is_probability_name(&name) {
        return scalar_capability(
            TemporalCapabilityBasis::CanonicalSelector,
            "probability values are instantaneous continuous scalars",
            &meta.name,
        );
    }

    if is_categorical(&name, field.as_deref()) {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::Categorical,
            basis: if field.is_some() {
                TemporalCapabilityBasis::CanonicalSelector
            } else {
                TemporalCapabilityBasis::NameAndUnits
            },
            recommended_semantics: Some(TemporalSemantics::Categorical),
            supported_reducers: vec![TemporalReducer::CategoricalSummary],
            operations: vec![
                TemporalOperation::CategoryMode,
                TemporalOperation::CategoryDuration,
                TemporalOperation::CategoryTransitions,
            ],
            required_variables: vec![meta.name.clone()],
            requires_manual_semantics: false,
            note: "category codes use mode, duration, and transitions; numeric extrema and range are not valid".into(),
        };
    }

    if let Some(seconds) = fixed_accumulation_window_seconds(&name)
        && (field.as_deref() == Some("totalprecipitation")
            || derived_slug(&meta.selector).is_some_and(|slug| normalize(slug) == name))
    {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::IntervalAccumulation,
            basis: TemporalCapabilityBasis::CanonicalSelectorAndName,
            recommended_semantics: Some(TemporalSemantics::IntervalAccumulation {
                support: IntervalSupport::EndsAtValidTime { seconds },
            }),
            supported_reducers: vec![TemporalReducer::IntervalSummary],
            operations: vec![
                TemporalOperation::IntervalTotal,
                TemporalOperation::MinimumIntervalAmount,
                TemporalOperation::MaximumIntervalAmount,
                TemporalOperation::RangeIntervalAmount,
                TemporalOperation::ArgMinimumTime,
                TemporalOperation::ArgMaximumTime,
            ],
            required_variables: vec![meta.name.clone()],
            requires_manual_semantics: false,
            note: format!(
                "the name and selector identify a trailing {}-second accumulation ending at valid time",
                seconds
            ),
        };
    }

    if is_cumulative_accumulation_name(&name) && field.as_deref() == Some("totalprecipitation") {
        return VariableTemporalCapability {
                value_class: TemporalValueClass::CumulativeAccumulation,
                basis: TemporalCapabilityBasis::CanonicalSelectorAndName,
                recommended_semantics: Some(TemporalSemantics::CumulativeFromOrigin {
                    include_first_value: false,
                    reset_tolerance: 0.0,
                }),
                supported_reducers: vec![TemporalReducer::CumulativeSummary],
                operations: vec![
                    TemporalOperation::TotalIncrement,
                    TemporalOperation::MinimumIncrement,
                    TemporalOperation::MaximumIncrement,
                    TemporalOperation::RangeIncrement,
                    TemporalOperation::ArgMinimumTime,
                    TemporalOperation::ArgMaximumTime,
                ],
                required_variables: vec![meta.name.clone()],
                requires_manual_semantics: false,
                note: "run/bucket totals use reset-aware increments; raw minimum, maximum, and range are not valid accumulation summaries".into(),
            };
    }

    if let Some(window_seconds) = fixed_interval_extremum_window_seconds(&name) {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::IntervalExtremum,
            basis: TemporalCapabilityBasis::CanonicalSelectorAndName,
            recommended_semantics: Some(TemporalSemantics::IntervalMaximum {
                support: IntervalSupport::EndsAtValidTime {
                    seconds: window_seconds,
                },
            }),
            supported_reducers: vec![TemporalReducer::IntervalMaximumSummary],
            operations: vec![
                TemporalOperation::MinimumOfIntervalMaxima,
                TemporalOperation::MaximumOfIntervalMaxima,
                TemporalOperation::RangeOfIntervalMaxima,
                TemporalOperation::ArgMinimumIntervalMaximumTime,
                TemporalOperation::ArgMaximumIntervalMaximumTime,
            ],
            required_variables: vec![meta.name.clone()],
            requires_manual_semantics: false,
            note: format!(
                "each sample is a trailing {}-second maximum; the reducer reports extrema and range across those interval maxima plus finite count and union-of-support coverage, never a sum or instantaneous mean",
                window_seconds
            ),
        };
    }

    if let Some((seconds_per_rate_unit, integral_units)) =
        rate_spec(&name, field.as_deref(), &meta.units)
    {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::Rate,
            basis: if field.as_deref() == Some("lightningflashdensity") {
                TemporalCapabilityBasis::CanonicalSelector
            } else {
                TemporalCapabilityBasis::NameAndUnits
            },
            recommended_semantics: Some(TemporalSemantics::IntervalRate {
                support: IntervalSupport::UntilNextExpectedTime,
                seconds_per_rate_unit,
                integral_units,
            }),
            supported_reducers: vec![TemporalReducer::RateSummary],
            operations: vec![
                TemporalOperation::MinimumRate,
                TemporalOperation::MaximumRate,
                TemporalOperation::RangeRate,
                TemporalOperation::DurationWeightedMean,
                TemporalOperation::Integral,
                TemporalOperation::ArgMinimumTime,
                TemporalOperation::ArgMaximumTime,
            ],
            required_variables: vec![meta.name.clone()],
            requires_manual_semantics: false,
            note:
                "the canonical rate identity and time-denominated units define a physical integral"
                    .into(),
        };
    }

    if is_circular_direction(&name, field.as_deref(), &meta.units) {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::CircularDirection,
            basis: TemporalCapabilityBasis::NameAndUnits,
            recommended_semantics: Some(TemporalSemantics::CircularDegrees),
            supported_reducers: vec![TemporalReducer::CircularMean],
            operations: vec![TemporalOperation::CircularMean],
            required_variables: vec![meta.name.clone()],
            requires_manual_semantics: false,
            note: "angular directions require circular statistics; scalar minimum, maximum, and range are rejected".into(),
        };
    }

    if matches!(field.as_deref(), Some("uwind" | "vwind")) {
        return VariableTemporalCapability {
            value_class: TemporalValueClass::VectorComponent,
            basis: TemporalCapabilityBasis::CanonicalSelector,
            recommended_semantics: None,
            supported_reducers: Vec::new(),
            operations: Vec::new(),
            required_variables: Vec::new(),
            requires_manual_semantics: true,
            note: "the U/V companion must be present with matching units before vector reduction is enabled".into(),
        };
    }

    if field.as_deref() == Some("totalprecipitation") {
        return VariableTemporalCapability::unknown(
            TemporalCapabilityBasis::ManualRequired,
            "total precipitation lacks an unambiguous run-total or fixed-window name; declare its accumulation support explicitly",
        );
    }

    if name == "uh_2to5km" && field.as_deref() == Some("updrafthelicity") {
        return VariableTemporalCapability::unknown(
            TemporalCapabilityBasis::ManualRequired,
            "the stored selector does not retain whether this updraft-helicity plane is instantaneous or a fixed-window maximum; use the explicit *_max_1h variable when available",
        );
    }

    if field.as_deref().is_some_and(is_canonical_scalar_field) {
        return scalar_capability(
            TemporalCapabilityBasis::CanonicalSelector,
            "the canonical selector identifies an instantaneous continuous scalar",
            &meta.name,
        );
    }

    if derived_slug(&meta.selector).is_some_and(|slug| is_known_derived_scalar(&normalize(slug))) {
        return scalar_capability(
            TemporalCapabilityBasis::BuiltInDerivedVariable,
            "the stored name is a built-in instantaneous derived scalar",
            &meta.name,
        );
    }

    VariableTemporalCapability::unknown(
        TemporalCapabilityBasis::ManualRequired,
        "the store metadata does not unambiguously define temporal semantics; raw sampling remains available",
    )
}

fn scalar_capability(
    basis: TemporalCapabilityBasis,
    note: impl Into<String>,
    variable: &str,
) -> VariableTemporalCapability {
    VariableTemporalCapability {
        value_class: TemporalValueClass::InstantaneousScalar,
        basis,
        recommended_semantics: Some(TemporalSemantics::InstantaneousScalar),
        supported_reducers: vec![TemporalReducer::ScalarSummary],
        operations: vec![
            TemporalOperation::ScalarMinimum,
            TemporalOperation::ScalarMaximum,
            TemporalOperation::ScalarRange,
            TemporalOperation::TimeWeightedMean,
            TemporalOperation::ArgMinimumTime,
            TemporalOperation::ArgMaximumTime,
        ],
        required_variables: vec![variable.to_string()],
        requires_manual_semantics: false,
        note: note.into(),
    }
}

fn finish_vector_capability(
    meta: &RwsVariableMeta,
    inventory: &BTreeMap<&str, &RwsVariableMeta>,
    capability: &mut VariableTemporalCapability,
) {
    let Some((u_name, v_name)) =
        vector_pair_names(&meta.name, selector_field(&meta.selector).as_deref())
    else {
        return;
    };
    let (Some(u), Some(v)) = (
        inventory.get(u_name.as_str()),
        inventory.get(v_name.as_str()),
    ) else {
        capability.note = format!(
            "vector reduction requires both '{}' and '{}' in the same run inventory",
            u_name, v_name
        );
        return;
    };
    if u.kind != v.kind
        || !matches!(u.kind.as_str(), "surface2d" | "pressure3d")
        || u.units != v.units
        || u.levels_hpa != v.levels_hpa
        || selector_field(&u.selector).as_deref() != Some("uwind")
        || selector_field(&v.selector).as_deref() != Some("vwind")
    {
        capability.note = format!(
            "vector companions '{}' and '{}' must be same-kind U/V fields with identical units and pressure axes",
            u_name, v_name
        );
        return;
    }
    capability.recommended_semantics = Some(TemporalSemantics::VectorComponents);
    capability.supported_reducers = vec![TemporalReducer::VectorSummary];
    capability.operations = vec![
        TemporalOperation::MinimumVectorSpeed,
        TemporalOperation::MaximumVectorSpeed,
        TemporalOperation::RangeVectorSpeed,
        TemporalOperation::TimeWeightedMeanSpeed,
        TemporalOperation::VectorMean,
        TemporalOperation::ArgMinimumTime,
        TemporalOperation::ArgMaximumTime,
    ];
    capability.required_variables = vec![u_name, v_name];
    capability.requires_manual_semantics = false;
    capability.note = "paired U/V components enable speed extrema, speed range, and vector means; component-wise direction statistics are not advertised".into();
}

fn selector_field(selector: &serde_json::Value) -> Option<String> {
    selector.get("field")?.as_str().map(normalize_compact)
}

fn selector_product(selector: &serde_json::Value) -> Option<String> {
    let product = selector.get("product")?;
    if let Some(text) = product.as_str() {
        return Some(normalize_compact(text));
    }
    product
        .as_object()?
        .keys()
        .next()
        .map(|key| normalize_compact(key))
}

fn derived_slug(selector: &serde_json::Value) -> Option<&str> {
    selector.get("derived")?.as_str()
}

fn normalize(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

fn normalize_compact(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn normalize_units(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace(' ', "")
}

fn is_probability_name(name: &str) -> bool {
    name.starts_with("probability_")
        || name.contains("_probability_")
        || name.starts_with("prob_")
        || name.contains("_prob_")
}

fn provider_published_statistic_window_seconds(name: &str) -> Option<u64> {
    fn embedded_hours(name: &str, marker: &str) -> Option<u64> {
        let (_, rest) = name.split_once(marker)?;
        let (token, statistic_suffix) = rest.split_once('_')?;
        if statistic_suffix.is_empty() {
            return None;
        }
        token
            .strip_suffix('h')?
            .parse::<u64>()
            .ok()
            .filter(|hours| *hours > 0 && *hours <= 24 * 31)
    }

    let hours = embedded_hours(name, "_max_")
        .or_else(|| embedded_hours(name, "_min_"))
        .or_else(|| {
            name.strip_prefix("total_precipitation_")
                .and_then(|rest| {
                    let (token, statistic_suffix) = rest.split_once('_')?;
                    if statistic_suffix.is_empty() {
                        return None;
                    }
                    token.strip_suffix('h')?.parse::<u64>().ok()
                })
                .filter(|hours| *hours > 0 && *hours <= 24 * 31)
        })?;
    Some(hours * 3_600)
}

fn is_categorical(name: &str, field: Option<&str>) -> bool {
    matches!(
        field,
        Some(
            "categoricalrain"
                | "categoricalfreezingrain"
                | "categoricalicepellets"
                | "categoricalsnow"
                | "landseamask"
        )
    ) || name.starts_with("categorical_")
        || name == "precipitation_type"
        || name == "land_sea_mask"
        || name.ends_with("_category")
}

fn fixed_accumulation_window_seconds(name: &str) -> Option<u64> {
    let accumulation = name == "qpf_1h"
        || name == "apcp_1h"
        || name.contains("precipitation_accumulation_")
        || name.contains("precip_accum_")
        || name.contains("qpf_")
        || name.contains("apcp_");
    if !accumulation {
        return None;
    }
    let suffix = name.rsplit('_').next()?.strip_suffix('h')?;
    let hours = suffix.parse::<u64>().ok()?;
    (hours > 0 && hours <= 24 * 31).then(|| hours * 3_600)
}

fn is_cumulative_accumulation_name(name: &str) -> bool {
    name == "apcp_run_total"
        || name == "qpf_run_total"
        || name == "total_qpf"
        || name.ends_with("_total_qpf")
        || ((name.contains("apcp") || name.contains("precip")) && name.ends_with("_run_total"))
}

fn fixed_interval_extremum_window_seconds(name: &str) -> Option<u64> {
    let (_, suffix) = name
        .rsplit_once("_max_")
        .or_else(|| name.rsplit_once("_maximum_"))?;
    let hours = suffix.strip_suffix('h')?.parse::<u64>().ok()?;
    (hours > 0 && hours <= 24 * 31).then(|| hours * 3_600)
}

fn rate_spec(name: &str, field: Option<&str>, units: &str) -> Option<(f64, String)> {
    if field == Some("lightningflashdensity") {
        return match normalize_units(units).as_str() {
            "km^-2day^-1" | "km-2day-1" => Some((86_400.0, "km^-2".into())),
            "m^-2s^-1" | "m-2s-1" => Some((1.0, "m^-2".into())),
            _ => None,
        };
    }
    if !(name.contains("precipitation_rate")
        || name.contains("precip_rate")
        || name.contains("rain_rate")
        || name.contains("snowfall_rate"))
    {
        return None;
    }
    match normalize_units(units).as_str() {
        "kg/m^2/s" | "kgm^-2s^-1" | "kgm-2s-1" => Some((1.0, "kg/m^2".into())),
        "mm/s" | "mms^-1" | "mms-1" => Some((1.0, "mm".into())),
        "mm/h" | "mm/hr" | "mmh^-1" | "mmhr^-1" | "mmh-1" => Some((3_600.0, "mm".into())),
        "m/s" | "ms^-1" | "ms-1" => Some((1.0, "m".into())),
        _ => None,
    }
}

fn is_circular_direction(name: &str, field: Option<&str>, units: &str) -> bool {
    let angular_units = matches!(
        normalize_units(units).as_str(),
        "degree" | "degrees" | "deg" | "degrees_true" | "degree_true" | "\u{00b0}"
    );
    angular_units
        && (field == Some("winddirection")
            || name == "wdir"
            || name.starts_with("wdir_")
            || name.contains("wind_direction")
            || name.ends_with("_direction"))
}

fn is_canonical_scalar_field(field: &str) -> bool {
    matches!(
        field,
        "pressure"
            | "geopotentialheight"
            | "temperature"
            | "relativehumidity"
            | "dewpoint"
            | "pressurereducedtomeansealevel"
            | "absolutevorticity"
            | "relativevorticity"
            | "windspeed"
            | "windgust"
            | "totalcloudcover"
            | "lowcloudcover"
            | "middlecloudcover"
            | "highcloudcover"
            | "precipitablewater"
            | "probabilityofprecipitation"
            | "visibility"
            | "simulatedinfraredbrightnesstemperature"
            | "radarreflectivity"
            | "compositereflectivity"
            | "updrafthelicity"
            | "smokemassdensity"
            | "columnintegratedsmoke"
    )
}

fn is_canonical_pressure_scalar_field(field: &str) -> bool {
    matches!(
        field,
        "temperature" | "dewpoint" | "relativehumidity" | "geopotentialheight"
    )
}

fn vector_pair_names(name: &str, field: Option<&str>) -> Option<(String, String)> {
    let is_u = field == Some("uwind");
    let is_v = field == Some("vwind");
    if !is_u && !is_v {
        return None;
    }
    if let Some(rest) = name.strip_prefix(if is_u { "u_" } else { "v_" }) {
        return Some((format!("u_{rest}"), format!("v_{rest}")));
    }
    let token = if is_u { "_u_" } else { "_v_" };
    if let Some(index) = name.find(token) {
        let mut u = name.to_string();
        let mut v = name.to_string();
        u.replace_range(index..index + token.len(), "_u_");
        v.replace_range(index..index + token.len(), "_v_");
        return Some((u, v));
    }
    let token = if is_u { "u_wind" } else { "v_wind" };
    if let Some(index) = name.find(token) {
        let mut u = name.to_string();
        let mut v = name.to_string();
        u.replace_range(index..index + token.len(), "u_wind");
        v.replace_range(index..index + token.len(), "v_wind");
        return Some((u, v));
    }
    None
}

fn is_known_derived_scalar(name: &str) -> bool {
    matches!(
        name,
        "sbcape"
            | "sbcin"
            | "sblcl"
            | "mlcape"
            | "mlcin"
            | "mucape"
            | "mucin"
            | "dcape"
            | "sbecape"
            | "mlecape"
            | "muecape"
            | "sb_ecape_derived_cape_ratio"
            | "ml_ecape_derived_cape_ratio"
            | "mu_ecape_derived_cape_ratio"
            | "sb_ecape_native_cape_ratio"
            | "ml_ecape_native_cape_ratio"
            | "mu_ecape_native_cape_ratio"
            | "sbncape"
            | "sbecin"
            | "mlecin"
            | "ecape_scp"
            | "ecape_ehi_0_1km"
            | "ecape_ehi_0_3km"
            | "ecape_stp"
            | "theta_e_2m_10m_winds"
            | "vpd_2m"
            | "dewpoint_depression_2m"
            | "wetbulb_2m"
            | "fire_weather_composite"
            | "apparent_temperature_2m"
            | "heat_index_2m"
            | "wind_chill_2m"
            | "lifted_index"
            | "lapse_rate_700_500"
            | "lapse_rate_0_3km"
            | "bulk_shear_0_1km"
            | "bulk_shear_0_6km"
            | "srh_0_1km"
            | "srh_0_3km"
            | "ehi_0_1km"
            | "ehi_0_3km"
            | "stp_fixed"
            | "scp_mu_0_3km_0_6km_proxy"
            | "temperature_advection_700mb"
            | "temperature_advection_850mb"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, units: &str, selector: serde_json::Value) -> RwsVariableMeta {
        RwsVariableMeta {
            id: 0,
            name: name.into(),
            units: units.into(),
            kind: "surface2d".into(),
            codec: "affine_i16_zstd_tiles_v1".into(),
            levels_hpa: Vec::new(),
            selector,
        }
    }

    #[test]
    fn representative_model_fields_get_conservative_native_capabilities() {
        let fields = vec![
            meta(
                "temperature_2m",
                "K",
                serde_json::json!({"field":"Temperature"}),
            ),
            meta(
                "apcp_run_total",
                "kg/m^2",
                serde_json::json!({"field":"TotalPrecipitation"}),
            ),
            meta(
                "apcp_1h",
                "kg/m^2",
                serde_json::json!({"field":"TotalPrecipitation"}),
            ),
            meta("u_10m", "m/s", serde_json::json!({"field":"UWind"})),
            meta("v_10m", "m/s", serde_json::json!({"field":"VWind"})),
            meta(
                "mslp",
                "Pa",
                serde_json::json!({"field":"PressureReducedToMeanSeaLevel"}),
            ),
            meta(
                "href_mean_temperature_2m",
                "K",
                serde_json::json!({"field":"Temperature", "product":"EnsembleMean"}),
            ),
            meta(
                "nbm_probability_temperature_below_freezing",
                "%",
                serde_json::json!({"field":"Temperature", "product":{"Probability":{"upper_limit_milli":273150}}}),
            ),
        ];
        let capabilities = variable_temporal_capabilities(&fields);

        // HRRR and RTMA scalar fields are ordinary instantaneous fields.
        assert_eq!(
            capabilities["temperature_2m"].value_class,
            TemporalValueClass::InstantaneousScalar
        );
        assert_eq!(
            capabilities["mslp"].value_class,
            TemporalValueClass::InstantaneousScalar
        );
        assert!(capabilities["mslp"].supports(TemporalOperation::ScalarRange));

        // GFS/HRRR total and fixed-window APCP never advertise raw scalar range.
        let total = &capabilities["apcp_run_total"];
        assert_eq!(
            total.value_class,
            TemporalValueClass::CumulativeAccumulation
        );
        assert!(total.supports(TemporalOperation::TotalIncrement));
        assert!(total.supports(TemporalOperation::RangeIncrement));
        assert!(!total.supports(TemporalOperation::ScalarMinimum));
        assert!(!total.supports(TemporalOperation::ScalarRange));
        let hourly = &capabilities["apcp_1h"];
        assert_eq!(hourly.value_class, TemporalValueClass::IntervalAccumulation);
        assert!(hourly.supports(TemporalOperation::IntervalTotal));
        assert!(hourly.supports(TemporalOperation::RangeIntervalAmount));
        assert!(!hourly.supports(TemporalOperation::ScalarRange));

        // NBM-style paired winds are enabled only as an ordered pair.
        let u = &capabilities["u_10m"];
        assert_eq!(u.required_variables, ["u_10m", "v_10m"]);
        assert_eq!(u.supported_reducers, [TemporalReducer::VectorSummary]);
        assert!(u.supports(TemporalOperation::RangeVectorSpeed));
        assert!(!u.requires_manual_semantics);

        // Ensemble means and probabilities are temporal scalars, not an
        // accumulation of the thresholded physical quantity.
        assert_eq!(
            capabilities["href_mean_temperature_2m"].value_class,
            TemporalValueClass::InstantaneousScalar
        );
        let probability = &capabilities["nbm_probability_temperature_below_freezing"];
        assert_eq!(
            probability.value_class,
            TemporalValueClass::InstantaneousScalar
        );
        assert!(probability.supports(TemporalOperation::ScalarMaximum));
    }

    #[test]
    fn directions_categories_rates_and_unknown_wrf_fields_reject_scalar_extrema() {
        let direction = meta(
            "wind_direction_10m",
            "degrees",
            serde_json::json!({"derived":"wind_direction_10m"}),
        );
        let category = meta(
            "categorical_snow",
            "0/1",
            serde_json::json!({"field":"CategoricalSnow"}),
        );
        let rate = meta(
            "lightning_flash_density",
            "km^-2 day^-1",
            serde_json::json!({"field":"LightningFlashDensity"}),
        );
        let unknown = meta(
            "wrf_custom_tornado_metric",
            "widget",
            serde_json::json!({"derived":"wrf_custom_tornado_metric"}),
        );

        let direction = variable_temporal_capability(&direction);
        assert_eq!(direction.value_class, TemporalValueClass::CircularDirection);
        assert_eq!(direction.operations, [TemporalOperation::CircularMean]);
        assert!(!direction.supports(TemporalOperation::ScalarMinimum));

        let category = variable_temporal_capability(&category);
        assert_eq!(category.value_class, TemporalValueClass::Categorical);
        assert!(category.supports(TemporalOperation::CategoryMode));
        assert!(!category.supports(TemporalOperation::ScalarMaximum));

        let rate = variable_temporal_capability(&rate);
        assert_eq!(rate.value_class, TemporalValueClass::Rate);
        assert!(rate.supports(TemporalOperation::Integral));
        assert!(rate.supports(TemporalOperation::RangeRate));
        assert!(!rate.supports(TemporalOperation::ScalarRange));

        let unknown = variable_temporal_capability(&unknown);
        assert_eq!(unknown.value_class, TemporalValueClass::Unknown);
        assert!(unknown.requires_manual_semantics);
        assert!(unknown.supported_reducers.is_empty());
        assert!(unknown.operations.is_empty());
    }

    #[test]
    fn native_accumulation_names_preserve_fixed_and_ambiguous_support() {
        let three_hour = variable_temporal_capability(&meta(
            "apcp_3h",
            "kg/m^2",
            serde_json::json!({"field":"TotalPrecipitation"}),
        ));
        assert_eq!(
            three_hour.recommended_semantics,
            Some(TemporalSemantics::IntervalAccumulation {
                support: IntervalSupport::EndsAtValidTime { seconds: 10_800 }
            })
        );
        assert!(three_hour.supports(TemporalOperation::RangeIntervalAmount));

        let variable_window = variable_temporal_capability(&meta(
            "apcp_native_interval",
            "kg/m^2",
            serde_json::json!({"field":"TotalPrecipitation"}),
        ));
        assert_eq!(variable_window.value_class, TemporalValueClass::Unknown);
        assert!(variable_window.requires_manual_semantics);
        assert!(variable_window.operations.is_empty());
    }

    #[test]
    fn unpaired_vector_is_manual_and_interval_maximum_is_precisely_reducible() {
        let u = meta("u_wind_500hpa", "m/s", serde_json::json!({"field":"UWind"}));
        let u = variable_temporal_capability(&u);
        assert_eq!(u.value_class, TemporalValueClass::VectorComponent);
        assert!(u.requires_manual_semantics);
        assert!(u.supported_reducers.is_empty());

        let maximum = meta(
            "wind_speed_10m_max_1h",
            "m/s",
            serde_json::json!({"field":"WindSpeed"}),
        );
        let maximum = variable_temporal_capability(&maximum);
        assert_eq!(maximum.value_class, TemporalValueClass::IntervalExtremum);
        assert_eq!(
            maximum.recommended_semantics,
            Some(TemporalSemantics::IntervalMaximum {
                support: IntervalSupport::EndsAtValidTime { seconds: 3_600 }
            })
        );
        assert_eq!(
            maximum.supported_reducers,
            [TemporalReducer::IntervalMaximumSummary]
        );
        assert!(maximum.supports(TemporalOperation::MinimumOfIntervalMaxima));
        assert!(maximum.supports(TemporalOperation::MaximumOfIntervalMaxima));
        assert!(maximum.supports(TemporalOperation::RangeOfIntervalMaxima));
        assert!(!maximum.supports(TemporalOperation::ScalarMinimum));
        assert!(!maximum.requires_manual_semantics);

        let ambiguous_uh = meta(
            "uh_2to5km",
            "m^2/s^2",
            serde_json::json!({"field":"UpdraftHelicity"}),
        );
        let ambiguous_uh = variable_temporal_capability(&ambiguous_uh);
        assert_eq!(ambiguous_uh.value_class, TemporalValueClass::Unknown);
        assert!(ambiguous_uh.requires_manual_semantics);
    }

    #[test]
    fn provider_published_window_statistics_remain_queryable_but_temporally_manual() {
        for (name, expected_seconds) in [
            ("wind_gust_10m_max_3h_prob_gt_15000m", 10_800),
            ("total_precipitation_6h_p50", 21_600),
            ("temperature_2m_min_24h_prob_lt_273140m", 86_400),
        ] {
            let capability = variable_temporal_capability(&meta(
                name,
                "%",
                serde_json::json!({
                    "field": "Temperature",
                    "product": {"Probability": {"lower_limit_milli": 15000}}
                }),
            ));
            assert_eq!(
                capability.value_class,
                TemporalValueClass::Unknown,
                "{name}"
            );
            assert!(capability.requires_manual_semantics, "{name}");
            assert!(capability.operations.is_empty(), "{name}");
            assert!(
                capability
                    .note
                    .contains(&format!("{expected_seconds}-second")),
                "{}",
                capability.note
            );
        }
    }

    #[test]
    fn reps_three_hour_statistics_are_queryable_but_temporally_manual() {
        for (name, units) in [
            ("total_precipitation_3h_p50", "kg/m^2"),
            ("total_precipitation_3h_ensemble_mean", "kg/m^2"),
            ("total_precipitation_3h_probability_gt_10mm", "%"),
        ] {
            let capability = variable_temporal_capability(&meta(
                name,
                units,
                serde_json::json!({
                    "field":"TotalPrecipitation",
                    "product":{"Percentile":50}
                }),
            ));
            assert_eq!(capability.value_class, TemporalValueClass::Unknown);
            assert!(capability.requires_manual_semantics);
            assert!(capability.operations.is_empty());
            assert!(capability.note.contains("10800-second"));
        }
    }
}

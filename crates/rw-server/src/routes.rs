use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use rustwx_core::{ModelId, SourceId};
use rw_ingest::{IngestCapabilityLimitation, IngestSupportStatus, model_ingest_capability};
use rw_query::{
    IndexWindow2DRequest, IntervalSupport, MissingPolicy, PointSeriesRequest, PointSeriesResult,
    ProfileRequest, QueryError, SpatialStatsSeriesRequest, TemporalCapabilityBasis,
    TemporalGridRequest, TemporalOperation, TemporalReducer, TemporalReductionLimits,
    TemporalSemantics, TemporalValueClass, TemporalVerticalSelection, TemporalWindow,
    TimeExpectation, TimeRange, VariableCapability, query_point_series, query_profile,
    query_spatial_stats_series, query_window_2d, reduce_temporal_grid_with_cancel,
    reduce_temporal_grid_with_cancel_and_limits,
};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};
use tower_http::sensitive_headers::SetSensitiveRequestHeadersLayer;
use tower_http::services::ServeFile;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::error;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use crate::config::ConfigError;
use crate::problem::ProblemDetails;
use crate::{AppState, CancellationToken, ExecutionError, JobError, JobStatus};

const REQUEST_ID_HEADER: &str = "x-request-id";
const RETIRED_MODEL_ID: ModelId = ModelId::RrfsFireWx;
const RETIRED_VARIABLE_NAME: &str = "fire_weather_composite";

#[derive(Debug, Clone, Copy)]
struct RequestId(Uuid);

fn is_retired_model_slug(model: &str) -> bool {
    model
        .trim()
        .to_ascii_lowercase()
        .parse::<ModelId>()
        .is_ok_and(|parsed| parsed == RETIRED_MODEL_ID)
}

fn is_retired_variable_name(variable: &str) -> bool {
    let variable = variable.trim();
    variable.eq_ignore_ascii_case(RETIRED_VARIABLE_NAME)
        || variable.eq_ignore_ascii_case("fire-weather-composite")
}

/// Keep retired product identities behind the public HTTP boundary while
/// preserving rw-store/rw-query compatibility for local and embedded users.
/// The response deliberately matches an unknown route and never echoes the
/// blocked model or variable name.
fn reject_retired_selection<'a>(
    state: &AppState,
    request_id: Uuid,
    model: &str,
    variables: impl IntoIterator<Item = &'a str>,
) -> Option<Response> {
    if is_retired_model_slug(model) || variables.into_iter().any(is_retired_variable_name) {
        state.metrics.reject();
        Some(ProblemDetails::not_found(request_id).into_response())
    } else {
        None
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct HealthResponse {
    status: &'static str,
    uptime_seconds: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VersionResponse {
    service: &'static str,
    version: &'static str,
    git: &'static str,
    api: &'static str,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProductCapabilityResponse {
    product: String,
    surface_source: bool,
    pressure_source: bool,
    indexed_subset: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderAttributionResponse {
    provider: String,
    copyright_statement: String,
    notice: String,
    source_url: String,
    license: String,
    license_url: String,
    terms_url: String,
    modification_notice: String,
    disclaimer: String,
}

impl From<rw_query::ProviderAttribution> for ProviderAttributionResponse {
    fn from(value: rw_query::ProviderAttribution) -> Self {
        Self {
            provider: value.provider,
            copyright_statement: value.copyright_statement,
            notice: value.notice,
            source_url: value.source_url,
            license: value.license,
            license_url: value.license_url,
            terms_url: value.terms_url,
            modification_notice: value.modification_notice,
            disclaimer: value.disclaimer,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ModelCapabilityResponse {
    id: String,
    description: String,
    cycle_hours_utc: Vec<u8>,
    max_forecast_hour: u16,
    registry_source_count: usize,
    ingest_status: &'static str,
    verification: &'static str,
    limitations: Vec<ApiIngestCapabilityLimitation>,
    products: Vec<ProductCapabilityResponse>,
    provider_attributions: Vec<ProviderAttributionResponse>,
    stored_run_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiIngestCapabilityLimitation {
    AnalysisOnly,
    SurfaceOnly,
    EnsembleMeanOnly,
    SparsePressureLevels,
    DerivedProductsDisabled,
    ConusOnly,
    PreOperationalFeed,
}

impl From<IngestCapabilityLimitation> for ApiIngestCapabilityLimitation {
    fn from(value: IngestCapabilityLimitation) -> Self {
        match value {
            IngestCapabilityLimitation::AnalysisOnly => Self::AnalysisOnly,
            IngestCapabilityLimitation::SurfaceOnly => Self::SurfaceOnly,
            IngestCapabilityLimitation::EnsembleMeanOnly => Self::EnsembleMeanOnly,
            IngestCapabilityLimitation::SparsePressureLevels => Self::SparsePressureLevels,
            IngestCapabilityLimitation::DerivedProductsDisabled => Self::DerivedProductsDisabled,
            IngestCapabilityLimitation::ConusOnly => Self::ConusOnly,
            IngestCapabilityLimitation::PreOperationalFeed => Self::PreOperationalFeed,
        }
    }
}

fn provider_attributions(
    summary: &rustwx_models::ModelSummary,
) -> Vec<ProviderAttributionResponse> {
    let mut attributions = Vec::with_capacity(2);
    if summary
        .sources
        .iter()
        .any(|source| source.id == SourceId::Ecmwf)
    {
        attributions.push(rw_query::ecmwf_provider_attribution().into());
    }
    if summary.sources.iter().any(|source| {
        matches!(
            source.id,
            SourceId::Aws | SourceId::Nomads | SourceId::Google | SourceId::Azure | SourceId::Ncei
        )
    }) {
        attributions.push(rw_query::noaa_provider_attribution().into());
    }
    attributions
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiTemporalValueClass {
    InstantaneousScalar,
    IntervalAccumulation,
    CumulativeAccumulation,
    Rate,
    VectorComponent,
    CircularDirection,
    Categorical,
    IntervalExtremum,
    Unknown,
}

impl From<TemporalValueClass> for ApiTemporalValueClass {
    fn from(value: TemporalValueClass) -> Self {
        match value {
            TemporalValueClass::InstantaneousScalar => Self::InstantaneousScalar,
            TemporalValueClass::IntervalAccumulation => Self::IntervalAccumulation,
            TemporalValueClass::CumulativeAccumulation => Self::CumulativeAccumulation,
            TemporalValueClass::Rate => Self::Rate,
            TemporalValueClass::VectorComponent => Self::VectorComponent,
            TemporalValueClass::CircularDirection => Self::CircularDirection,
            TemporalValueClass::Categorical => Self::Categorical,
            TemporalValueClass::IntervalExtremum => Self::IntervalExtremum,
            TemporalValueClass::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiTemporalCapabilityBasis {
    CanonicalSelector,
    CanonicalSelectorAndName,
    BuiltInDerivedVariable,
    NameAndUnits,
    UnsupportedVariableKind,
    ManualRequired,
}

impl From<TemporalCapabilityBasis> for ApiTemporalCapabilityBasis {
    fn from(value: TemporalCapabilityBasis) -> Self {
        match value {
            TemporalCapabilityBasis::CanonicalSelector => Self::CanonicalSelector,
            TemporalCapabilityBasis::CanonicalSelectorAndName => Self::CanonicalSelectorAndName,
            TemporalCapabilityBasis::BuiltInDerivedVariable => Self::BuiltInDerivedVariable,
            TemporalCapabilityBasis::NameAndUnits => Self::NameAndUnits,
            TemporalCapabilityBasis::UnsupportedVariableKind => Self::UnsupportedVariableKind,
            TemporalCapabilityBasis::ManualRequired => Self::ManualRequired,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiTemporalOperation {
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

impl From<TemporalOperation> for ApiTemporalOperation {
    fn from(value: TemporalOperation) -> Self {
        match value {
            TemporalOperation::ScalarMinimum => Self::ScalarMinimum,
            TemporalOperation::ScalarMaximum => Self::ScalarMaximum,
            TemporalOperation::ScalarRange => Self::ScalarRange,
            TemporalOperation::TimeWeightedMean => Self::TimeWeightedMean,
            TemporalOperation::ArgMinimumTime => Self::ArgMinimumTime,
            TemporalOperation::ArgMaximumTime => Self::ArgMaximumTime,
            TemporalOperation::IntervalTotal => Self::IntervalTotal,
            TemporalOperation::MinimumIntervalAmount => Self::MinimumIntervalAmount,
            TemporalOperation::MaximumIntervalAmount => Self::MaximumIntervalAmount,
            TemporalOperation::RangeIntervalAmount => Self::RangeIntervalAmount,
            TemporalOperation::MinimumOfIntervalMaxima => Self::MinimumOfIntervalMaxima,
            TemporalOperation::MaximumOfIntervalMaxima => Self::MaximumOfIntervalMaxima,
            TemporalOperation::RangeOfIntervalMaxima => Self::RangeOfIntervalMaxima,
            TemporalOperation::ArgMinimumIntervalMaximumTime => Self::ArgMinimumIntervalMaximumTime,
            TemporalOperation::ArgMaximumIntervalMaximumTime => Self::ArgMaximumIntervalMaximumTime,
            TemporalOperation::TotalIncrement => Self::TotalIncrement,
            TemporalOperation::MinimumIncrement => Self::MinimumIncrement,
            TemporalOperation::MaximumIncrement => Self::MaximumIncrement,
            TemporalOperation::RangeIncrement => Self::RangeIncrement,
            TemporalOperation::MinimumRate => Self::MinimumRate,
            TemporalOperation::MaximumRate => Self::MaximumRate,
            TemporalOperation::RangeRate => Self::RangeRate,
            TemporalOperation::DurationWeightedMean => Self::DurationWeightedMean,
            TemporalOperation::Integral => Self::Integral,
            TemporalOperation::MinimumVectorSpeed => Self::MinimumVectorSpeed,
            TemporalOperation::MaximumVectorSpeed => Self::MaximumVectorSpeed,
            TemporalOperation::RangeVectorSpeed => Self::RangeVectorSpeed,
            TemporalOperation::TimeWeightedMeanSpeed => Self::TimeWeightedMeanSpeed,
            TemporalOperation::VectorMean => Self::VectorMean,
            TemporalOperation::CircularMean => Self::CircularMean,
            TemporalOperation::CategoryMode => Self::CategoryMode,
            TemporalOperation::CategoryDuration => Self::CategoryDuration,
            TemporalOperation::CategoryTransitions => Self::CategoryTransitions,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiMissingPolicy {
    Strict,
    Partial,
}

impl Default for ApiMissingPolicy {
    fn default() -> Self {
        Self::Strict
    }
}

impl From<ApiMissingPolicy> for MissingPolicy {
    fn from(value: ApiMissingPolicy) -> Self {
        match value {
            ApiMissingPolicy::Strict => Self::Strict,
            ApiMissingPolicy::Partial => Self::Partial,
        }
    }
}

#[derive(Debug, Deserialize, IntoParams, ToSchema)]
#[into_params(parameter_in = Query)]
pub struct PointQueryRequest {
    model: String,
    run: String,
    latitude: f64,
    longitude: f64,
    /// Comma-separated variable names.
    variables: String,
    start_unix: Option<i64>,
    end_unix: Option<i64>,
    #[serde(default)]
    missing_policy: ApiMissingPolicy,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CoordinateRequest {
    latitude: f64,
    longitude: f64,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct PointsRequest {
    model: String,
    run: String,
    points: Vec<CoordinateRequest>,
    variables: Vec<String>,
    start_unix: Option<i64>,
    end_unix: Option<i64>,
    #[serde(default)]
    missing_policy: ApiMissingPolicy,
}

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ProfileApiRequest {
    model: String,
    run: String,
    latitude: f64,
    longitude: f64,
    storage_slot: u16,
    variables: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiTemporalWindow {
    Utc { start_unix: i64, end_unix: i64 },
    LocalDay { date: String, timezone: String },
}

impl From<&ApiTemporalWindow> for TemporalWindow {
    fn from(value: &ApiTemporalWindow) -> Self {
        match value {
            ApiTemporalWindow::Utc {
                start_unix,
                end_unix,
            } => Self::Utc {
                start_unix: *start_unix,
                end_unix: *end_unix,
            },
            ApiTemporalWindow::LocalDay { date, timezone } => Self::LocalDay {
                date: date.clone(),
                timezone: timezone.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum ApiTimeExpectation {
    ManifestAxis,
    FixedCadence {
        step_seconds: u64,
        anchor_unix: Option<i64>,
    },
}

impl Default for ApiTimeExpectation {
    fn default() -> Self {
        Self::ManifestAxis
    }
}

impl From<&ApiTimeExpectation> for TimeExpectation {
    fn from(value: &ApiTimeExpectation) -> Self {
        match value {
            ApiTimeExpectation::ManifestAxis => Self::ManifestAxis,
            ApiTimeExpectation::FixedCadence {
                step_seconds,
                anchor_unix,
            } => Self::FixedCadence {
                step_seconds: *step_seconds,
                anchor_unix: *anchor_unix,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiIntervalSupport {
    StartsAtValidTime { seconds: u64 },
    EndsAtValidTime { seconds: u64 },
    UntilNextExpectedTime,
    SincePreviousExpectedTime,
}

impl From<ApiIntervalSupport> for IntervalSupport {
    fn from(value: ApiIntervalSupport) -> Self {
        match value {
            ApiIntervalSupport::StartsAtValidTime { seconds } => {
                Self::StartsAtValidTime { seconds }
            }
            ApiIntervalSupport::EndsAtValidTime { seconds } => Self::EndsAtValidTime { seconds },
            ApiIntervalSupport::UntilNextExpectedTime => Self::UntilNextExpectedTime,
            ApiIntervalSupport::SincePreviousExpectedTime => Self::SincePreviousExpectedTime,
        }
    }
}

impl From<&IntervalSupport> for ApiIntervalSupport {
    fn from(value: &IntervalSupport) -> Self {
        match value {
            IntervalSupport::StartsAtValidTime { seconds } => {
                Self::StartsAtValidTime { seconds: *seconds }
            }
            IntervalSupport::EndsAtValidTime { seconds } => {
                Self::EndsAtValidTime { seconds: *seconds }
            }
            IntervalSupport::UntilNextExpectedTime => Self::UntilNextExpectedTime,
            IntervalSupport::SincePreviousExpectedTime => Self::SincePreviousExpectedTime,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiTemporalSemantics {
    InstantaneousScalar,
    IntervalAccumulation {
        support: ApiIntervalSupport,
    },
    IntervalMaximum {
        support: ApiIntervalSupport,
    },
    CumulativeFromOrigin {
        #[serde(default)]
        include_first_value: bool,
        #[serde(default)]
        reset_tolerance: f64,
    },
    IntervalRate {
        support: ApiIntervalSupport,
        seconds_per_rate_unit: f64,
        integral_units: String,
    },
    VectorComponents,
    CircularDegrees,
    Categorical,
    Unknown,
}

impl From<&ApiTemporalSemantics> for TemporalSemantics {
    fn from(value: &ApiTemporalSemantics) -> Self {
        match value {
            ApiTemporalSemantics::InstantaneousScalar => Self::InstantaneousScalar,
            ApiTemporalSemantics::IntervalAccumulation { support } => Self::IntervalAccumulation {
                support: (*support).into(),
            },
            ApiTemporalSemantics::IntervalMaximum { support } => Self::IntervalMaximum {
                support: (*support).into(),
            },
            ApiTemporalSemantics::CumulativeFromOrigin {
                include_first_value,
                reset_tolerance,
            } => Self::CumulativeFromOrigin {
                include_first_value: *include_first_value,
                reset_tolerance: *reset_tolerance,
            },
            ApiTemporalSemantics::IntervalRate {
                support,
                seconds_per_rate_unit,
                integral_units,
            } => Self::IntervalRate {
                support: (*support).into(),
                seconds_per_rate_unit: *seconds_per_rate_unit,
                integral_units: integral_units.clone(),
            },
            ApiTemporalSemantics::VectorComponents => Self::VectorComponents,
            ApiTemporalSemantics::CircularDegrees => Self::CircularDegrees,
            ApiTemporalSemantics::Categorical => Self::Categorical,
            ApiTemporalSemantics::Unknown => Self::Unknown,
        }
    }
}

impl From<&TemporalSemantics> for ApiTemporalSemantics {
    fn from(value: &TemporalSemantics) -> Self {
        match value {
            TemporalSemantics::InstantaneousScalar => Self::InstantaneousScalar,
            TemporalSemantics::IntervalAccumulation { support } => Self::IntervalAccumulation {
                support: support.into(),
            },
            TemporalSemantics::IntervalMaximum { support } => Self::IntervalMaximum {
                support: support.into(),
            },
            TemporalSemantics::CumulativeFromOrigin {
                include_first_value,
                reset_tolerance,
            } => Self::CumulativeFromOrigin {
                include_first_value: *include_first_value,
                reset_tolerance: *reset_tolerance,
            },
            TemporalSemantics::IntervalRate {
                support,
                seconds_per_rate_unit,
                integral_units,
            } => Self::IntervalRate {
                support: support.into(),
                seconds_per_rate_unit: *seconds_per_rate_unit,
                integral_units: integral_units.clone(),
            },
            TemporalSemantics::VectorComponents => Self::VectorComponents,
            TemporalSemantics::CircularDegrees => Self::CircularDegrees,
            TemporalSemantics::Categorical => Self::Categorical,
            TemporalSemantics::Unknown => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiTemporalReducer {
    ScalarSummary,
    IntervalSummary,
    IntervalMaximumSummary,
    CumulativeSummary,
    RateSummary,
    VectorSummary,
    CircularMean,
    CategoricalSummary,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ApiTemporalVerticalSelection {
    PressureLevels { levels_hpa: Vec<u16> },
}

impl From<&ApiTemporalVerticalSelection> for TemporalVerticalSelection {
    fn from(value: &ApiTemporalVerticalSelection) -> Self {
        match value {
            ApiTemporalVerticalSelection::PressureLevels { levels_hpa } => Self::PressureLevels {
                levels_hpa: levels_hpa.clone(),
            },
        }
    }
}

impl From<ApiTemporalReducer> for TemporalReducer {
    fn from(value: ApiTemporalReducer) -> Self {
        match value {
            ApiTemporalReducer::ScalarSummary => Self::ScalarSummary,
            ApiTemporalReducer::IntervalSummary => Self::IntervalSummary,
            ApiTemporalReducer::IntervalMaximumSummary => Self::IntervalMaximumSummary,
            ApiTemporalReducer::CumulativeSummary => Self::CumulativeSummary,
            ApiTemporalReducer::RateSummary => Self::RateSummary,
            ApiTemporalReducer::VectorSummary => Self::VectorSummary,
            ApiTemporalReducer::CircularMean => Self::CircularMean,
            ApiTemporalReducer::CategoricalSummary => Self::CategoricalSummary,
        }
    }
}

impl From<TemporalReducer> for ApiTemporalReducer {
    fn from(value: TemporalReducer) -> Self {
        match value {
            TemporalReducer::ScalarSummary => Self::ScalarSummary,
            TemporalReducer::IntervalSummary => Self::IntervalSummary,
            TemporalReducer::IntervalMaximumSummary => Self::IntervalMaximumSummary,
            TemporalReducer::CumulativeSummary => Self::CumulativeSummary,
            TemporalReducer::RateSummary => Self::RateSummary,
            TemporalReducer::VectorSummary => Self::VectorSummary,
            TemporalReducer::CircularMean => Self::CircularMean,
            TemporalReducer::CategoricalSummary => Self::CategoricalSummary,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct TemporalGridApiRequest {
    model: String,
    run: String,
    variables: Vec<String>,
    semantics: ApiTemporalSemantics,
    reducer: ApiTemporalReducer,
    window: ApiTemporalWindow,
    #[serde(default)]
    expectation: ApiTimeExpectation,
    #[serde(default)]
    missing_policy: ApiMissingPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vertical: Option<ApiTemporalVerticalSelection>,
}

impl TemporalGridApiRequest {
    fn query(&self) -> TemporalGridRequest {
        TemporalGridRequest {
            variables: self.variables.clone(),
            semantics: (&self.semantics).into(),
            reducer: self.reducer.into(),
            window: (&self.window).into(),
            expectation: (&self.expectation).into(),
            missing_policy: self.missing_policy.into(),
            vertical: self.vertical.as_ref().map(Into::into),
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VariableTemporalCapabilityResponse {
    value_class: ApiTemporalValueClass,
    basis: ApiTemporalCapabilityBasis,
    recommended_semantics: Option<ApiTemporalSemantics>,
    supported_reducers: Vec<ApiTemporalReducer>,
    operations: Vec<ApiTemporalOperation>,
    required_variables: Vec<String>,
    requires_manual_semantics: bool,
    note: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct VariableCapabilityResponse {
    name: String,
    units: String,
    kind: String,
    codec: String,
    levels_hpa: Vec<u16>,
    selector: serde_json::Value,
    available_slots: Vec<u16>,
    available_samples: usize,
    expected_samples: usize,
    coverage: f64,
    point_series: bool,
    pressure_profile: bool,
    scalar_temporal_reduction: bool,
    temporal: VariableTemporalCapabilityResponse,
}

impl From<VariableCapability> for VariableCapabilityResponse {
    fn from(value: VariableCapability) -> Self {
        let temporal = value.temporal;
        Self {
            name: value.name,
            units: value.units,
            kind: value.kind,
            codec: value.codec,
            levels_hpa: value.levels_hpa,
            selector: value.selector,
            available_slots: value.available_slots,
            available_samples: value.available_samples,
            expected_samples: value.expected_samples,
            coverage: value.coverage,
            point_series: value.point_series,
            pressure_profile: value.pressure_profile,
            scalar_temporal_reduction: value.scalar_temporal_reduction,
            temporal: VariableTemporalCapabilityResponse {
                value_class: temporal.value_class.into(),
                basis: temporal.basis.into(),
                recommended_semantics: temporal.recommended_semantics.as_ref().map(Into::into),
                supported_reducers: temporal
                    .supported_reducers
                    .into_iter()
                    .map(Into::into)
                    .collect(),
                operations: temporal.operations.into_iter().map(Into::into).collect(),
                required_variables: temporal.required_variables,
                requires_manual_semantics: temporal.requires_manual_semantics,
                note: temporal.note,
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct WindowApiRequest {
    model: String,
    run: String,
    storage_slot: u16,
    variable: String,
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct SpatialSeriesApiRequest {
    model: String,
    run: String,
    variable: String,
    start_unix: Option<i64>,
    end_unix: Option<i64>,
    #[serde(default)]
    missing_policy: ApiMissingPolicy,
}

#[derive(Debug, Deserialize)]
struct ModelPath {
    model: String,
}

#[derive(Debug, Deserialize)]
struct RunPath {
    model: String,
    run: String,
}

#[derive(Debug, Deserialize)]
struct JobPath {
    id: String,
}

#[derive(Debug, Deserialize)]
struct ArtifactPath {
    hash: String,
    file: String,
}

#[derive(Debug)]
enum JobWorkError {
    Query(QueryError),
    Json(serde_json::Error),
    Cancelled,
}

#[derive(Debug)]
enum ResponseWorkError {
    Query(QueryError),
    Json(serde_json::Error),
}

impl From<QueryError> for ResponseWorkError {
    fn from(error: QueryError) -> Self {
        Self::Query(error)
    }
}

impl From<serde_json::Error> for ResponseWorkError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub fn build_router(state: AppState) -> Result<Router, ConfigError> {
    let public = Router::new()
        .route("/v1/health/live", get(health_live))
        .route("/v1/health/ready", get(health_ready))
        .route("/v1/version", get(version))
        .route("/v1/openapi.json", get(openapi));

    let protected = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/{model}/runs", get(list_runs))
        .route("/v1/models/{model}/runs/{run}", get(run_detail))
        .route(
            "/v1/models/{model}/runs/{run}/variables",
            get(run_variables),
        )
        .route("/v1/point", get(point))
        .route("/v1/points", post(points))
        .route("/v1/profile", post(profile))
        .route("/v1/analytics/temporal-grid", post(temporal_grid))
        .route("/v1/jobs/temporal-grid", post(submit_temporal_grid_job))
        .route("/v1/window", post(window))
        .route("/v1/analytics/spatial-series", post(spatial_series))
        .route("/v1/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/v1/artifacts/{hash}/{file}", get(artifact))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authentication,
        ));

    let metrics = Router::new().route("/metrics", get(metrics));
    let metrics = if state.config.auth.protect_metrics {
        metrics.route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authentication,
        ))
    } else {
        metrics
    };

    let mut router = public
        .merge(protected)
        .merge(metrics)
        .fallback(fallback)
        .with_state(state.clone())
        .layer(DefaultBodyLimit::max(
            state.config.limits.request_body_bytes,
        ))
        .layer(SetSensitiveRequestHeadersLayer::new(std::iter::once(
            header::AUTHORIZATION,
        )))
        .layer(CatchPanicLayer::new())
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            request_context,
        ));

    if !state.config.server.cors_origins.is_empty() {
        let origins = state
            .config
            .server
            .cors_origins
            .iter()
            .map(|origin| {
                HeaderValue::from_str(origin)
                    .map_err(|_| ConfigError::Invalid(format!("invalid CORS origin '{origin}'")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        router = router.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    }
    Ok(router)
}

async fn request_context(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let request_id = RequestId(Uuid::new_v4());
    request.extensions_mut().insert(request_id);
    let started = Instant::now();
    let guard = state.metrics.begin_request();
    let response = next.run(request).await;
    let mut response = normalize_framework_error(response, request_id.0);
    guard.finish(
        started.elapsed(),
        response.status().is_client_error() || response.status().is_server_error(),
    );
    if let Ok(value) = HeaderValue::from_str(&request_id.0.to_string()) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    response
}

fn normalize_framework_error(response: Response, request_id: Uuid) -> Response {
    let status = response.status();
    let is_problem = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(crate::problem::PROBLEM_CONTENT_TYPE));
    if is_problem || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let problem = match status {
        StatusCode::PAYLOAD_TOO_LARGE => ProblemDetails::new(
            status,
            "PAYLOAD_TOO_LARGE",
            "Request body is too large",
            "Reduce the request body below the configured byte limit.",
            request_id,
        ),
        StatusCode::METHOD_NOT_ALLOWED => ProblemDetails::new(
            status,
            "METHOD_NOT_ALLOWED",
            "Method is not allowed",
            "Use one of the methods documented for this endpoint.",
            request_id,
        ),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => ProblemDetails::new(
            status,
            "UNSUPPORTED_MEDIA_TYPE",
            "Unsupported media type",
            "JSON request endpoints require application/json.",
            request_id,
        ),
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ProblemDetails::new(
            status,
            "INVALID_REQUEST",
            "Invalid request",
            "The request body or parameters could not be decoded.",
            request_id,
        ),
        _ if status.is_server_error() => ProblemDetails::internal(request_id),
        _ => ProblemDetails::new(
            status,
            "REQUEST_REJECTED",
            "Request rejected",
            "The request could not be accepted.",
            request_id,
        ),
    };
    problem.into_response()
}

async fn require_authentication(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if state.tokens.is_empty()
        || state
            .tokens
            .verify_authorization_header(request.headers().get(header::AUTHORIZATION))
    {
        return next.run(request).await;
    }
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or(RequestId(Uuid::nil()));
    state.metrics.reject();
    let mut response = ProblemDetails::unauthorized(request_id.0).into_response();
    response.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Bearer realm=\"rusty-weather\""),
    );
    response
}

async fn health_live(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "live",
        uptime_seconds: state.uptime().as_secs(),
    })
}

async fn health_ready(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let catalog = state.catalog.clone();
    match state.run_light(move || catalog.probe_readable()).await {
        Ok(Ok(())) => Json(HealthResponse {
            status: "ready",
            uptime_seconds: state.uptime().as_secs(),
        })
        .into_response(),
        Ok(Err(_)) => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "NOT_READY",
            "Service is not ready",
            "The configured store is not currently readable.",
            request_id.0,
        )
        .into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn version() -> Json<VersionResponse> {
    Json(VersionResponse {
        service: "rusty-weather",
        version: env!("CARGO_PKG_VERSION"),
        git: option_env!("RW_BUILD_SHA").unwrap_or("unknown"),
        api: "v1",
    })
}

async fn openapi(Extension(request_id): Extension<RequestId>) -> Response {
    json_with_etag(StatusCode::OK, &crate::openapi::document(), request_id.0)
}

async fn list_models(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let catalog = state.catalog.clone();
    match state.run_light(move || catalog.list_models()).await {
        Ok(Ok(stored)) => {
            let stored: BTreeMap<_, _> = stored
                .into_iter()
                .filter(|entry| !is_retired_model_slug(&entry.model))
                .map(|entry| (entry.model, entry.run_count))
                .collect();
            let mut result = Vec::new();
            for summary in rustwx_models::built_in_models()
                .iter()
                .filter(|summary| summary.id != ModelId::RrfsFireWx)
            {
                let capability = model_ingest_capability(summary.id);
                let verification = if summary.id == ModelId::WrfGdex {
                    "local_import"
                } else {
                    capability.verification.as_str()
                };
                let limitations = capability
                    .limitations
                    .iter()
                    .copied()
                    .map(ApiIngestCapabilityLimitation::from)
                    .collect();
                let products = capability
                    .products
                    .into_iter()
                    .map(|product| ProductCapabilityResponse {
                        product: product.product.to_string(),
                        surface_source: product.surface_source,
                        pressure_source: product.pressure_source,
                        indexed_subset: !product.idx_patterns.is_empty(),
                    })
                    .collect();
                result.push(ModelCapabilityResponse {
                    id: summary.id.to_string(),
                    description: summary.description.to_string(),
                    cycle_hours_utc: summary.cycle_hours_utc.to_vec(),
                    max_forecast_hour: summary.max_forecast_hour,
                    registry_source_count: summary.sources.len(),
                    ingest_status: match (summary.id, capability.status) {
                        (ModelId::WrfGdex, _) => "local_import",
                        (_, IngestSupportStatus::Ready) => "ready",
                        (_, IngestSupportStatus::Unsupported) => "catalogued",
                    },
                    verification,
                    limitations,
                    products,
                    provider_attributions: provider_attributions(summary),
                    stored_run_count: stored.get(&summary.id.to_string()).copied().unwrap_or(0),
                });
            }
            for (model, run_count) in stored {
                if result.iter().any(|entry| entry.id == model) {
                    continue;
                }
                result.push(ModelCapabilityResponse {
                    id: model,
                    description: "Compatible local rw-store model".into(),
                    cycle_hours_utc: Vec::new(),
                    max_forecast_hour: 0,
                    registry_source_count: 0,
                    ingest_status: "local_store",
                    verification: "stored",
                    limitations: Vec::new(),
                    products: Vec::new(),
                    provider_attributions: Vec::new(),
                    stored_run_count: run_count,
                });
            }
            result.sort_by(|left, right| left.id.cmp(&right.id));
            json_with_etag(StatusCode::OK, &result, request_id.0)
        }
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn list_runs(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ModelPath>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &path.model,
        std::iter::empty::<&str>(),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    match state
        .run_light(move || {
            let model = path.model;
            let mut runs = catalog.list_runs(&model)?;
            for entry in &mut runs {
                // `rw-query` intentionally retains the complete internal
                // inventory. Recompute only the public count so the hidden
                // compatibility field cannot affect catalog metadata.
                let snapshot = catalog.snapshot(&model, &entry.run.run)?;
                entry.variable_count = snapshot
                    .variable_capabilities()?
                    .into_iter()
                    .filter(|variable| !is_retired_variable_name(&variable.name))
                    .count();
            }
            Ok(runs)
        })
        .await
    {
        Ok(Ok(runs)) => json_with_etag(StatusCode::OK, &runs, request_id.0),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn run_detail(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<RunPath>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &path.model,
        std::iter::empty::<&str>(),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    match state
        .run_light(move || catalog.snapshot(&path.model, &path.run))
        .await
    {
        Ok(Ok(snapshot)) => json_with_etag(StatusCode::OK, snapshot.descriptor(), request_id.0),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn run_variables(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<RunPath>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &path.model,
        std::iter::empty::<&str>(),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&path.model, &path.run)?;
            snapshot.variable_capabilities().map(|variables| {
                variables
                    .into_iter()
                    .filter(|variable| !is_retired_variable_name(&variable.name))
                    .map(VariableCapabilityResponse::from)
                    .collect::<Vec<_>>()
            })
        })
        .await
    {
        Ok(Ok(variables)) => json_with_etag(StatusCode::OK, &variables, request_id.0),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn point(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Query(request): Query<PointQueryRequest>,
) -> Response {
    let variables = parse_csv_variables(&request.variables);
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        variables.iter().map(String::as_str),
    ) {
        return response;
    }
    let query = PointSeriesRequest {
        latitude: request.latitude,
        longitude: request.longitude,
        variables,
        time: TimeRange {
            start_unix: request.start_unix,
            end_unix: request.end_unix,
        },
        missing_policy: request.missing_policy.into(),
    };
    let catalog = state.catalog.clone();
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            cache_or_compute(
                &cache,
                &metrics,
                "point",
                &snapshot.descriptor().snapshot_id,
                &query,
                || query_point_series(&snapshot, &query),
            )
        })
        .await
    {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn points(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<PointsRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        request.variables.iter().map(String::as_str),
    ) {
        return response;
    }
    if request.points.is_empty() || request.points.len() > state.config.limits.points_per_query {
        state.metrics.reject();
        return ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "QUERY_LIMIT",
            "Point count is outside the configured limit",
            "Submit at least one point and keep the request within the advertised service limits.",
            request_id.0,
        )
        .into_response();
    }
    let catalog = state.catalog.clone();
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let selected_times = snapshot
                .select_timepoints(TimeRange {
                    start_unix: request.start_unix,
                    end_unix: request.end_unix,
                })?
                .len();
            let output_values = request
                .points
                .len()
                .checked_mul(selected_times)
                .and_then(|count| count.checked_mul(request.variables.len()))
                .ok_or(QueryError::LimitExceeded {
                    what: "multi-point values",
                    requested: usize::MAX,
                    limit: snapshot.limits().max_point_values,
                })?;
            if output_values > snapshot.limits().max_point_values {
                return Err(QueryError::LimitExceeded {
                    what: "multi-point values",
                    requested: output_values,
                    limit: snapshot.limits().max_point_values,
                }
                .into());
            }
            cache_or_compute(
                &cache,
                &metrics,
                "points",
                &snapshot.descriptor().snapshot_id,
                &request,
                || {
                    let mut results = Vec::with_capacity(request.points.len());
                    for point in &request.points {
                        results.push(query_point_series(
                            &snapshot,
                            &PointSeriesRequest {
                                latitude: point.latitude,
                                longitude: point.longitude,
                                variables: request.variables.clone(),
                                time: TimeRange {
                                    start_unix: request.start_unix,
                                    end_unix: request.end_unix,
                                },
                                missing_policy: request.missing_policy.into(),
                            },
                        )?);
                    }
                    Ok::<Vec<PointSeriesResult>, QueryError>(results)
                },
            )
        })
        .await
    {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn profile(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<ProfileApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        request.variables.iter().map(String::as_str),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            cache_or_compute(
                &cache,
                &metrics,
                "profile",
                &snapshot.descriptor().snapshot_id,
                &request,
                || {
                    query_profile(
                        &snapshot,
                        &ProfileRequest {
                            latitude: request.latitude,
                            longitude: request.longitude,
                            storage_slot: request.storage_slot,
                            variables: request.variables.clone(),
                        },
                    )
                },
            )
        })
        .await
    {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn window(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<WindowApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        std::iter::once(request.variable.as_str()),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    let json_limit = state.config.limits.json_grid_values;
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let width = request.x1.checked_sub(request.x0).ok_or_else(|| {
                QueryError::InvalidRequest("window x bounds are reversed".to_string())
            })?;
            let height = request.y1.checked_sub(request.y0).ok_or_else(|| {
                QueryError::InvalidRequest("window y bounds are reversed".to_string())
            })?;
            let cells = width.checked_mul(height).ok_or(QueryError::LimitExceeded {
                what: "JSON grid values",
                requested: usize::MAX,
                limit: json_limit,
            })?;
            if cells > json_limit {
                return Err(QueryError::LimitExceeded {
                    what: "JSON grid values",
                    requested: cells,
                    limit: json_limit,
                }
                .into());
            }
            let query = IndexWindow2DRequest {
                storage_slot: request.storage_slot,
                variable: request.variable.clone(),
                x0: request.x0,
                y0: request.y0,
                x1: request.x1,
                y1: request.y1,
            };
            cache_or_compute(
                &cache,
                &metrics,
                "window",
                &snapshot.descriptor().snapshot_id,
                &request,
                || query_window_2d(&snapshot, &query),
            )
        })
        .await
    {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn spatial_series(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<SpatialSeriesApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        std::iter::once(request.variable.as_str()),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    match state
        .run_light(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let query = SpatialStatsSeriesRequest {
                variable: request.variable.clone(),
                time: TimeRange {
                    start_unix: request.start_unix,
                    end_unix: request.end_unix,
                },
                missing_policy: request.missing_policy.into(),
            };
            cache_or_compute(
                &cache,
                &metrics,
                "spatial_series",
                &snapshot.descriptor().snapshot_id,
                &request,
                || query_spatial_stats_series(&snapshot, &query),
            )
        })
        .await
    {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn temporal_grid(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<TemporalGridApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        request.variables.iter().map(String::as_str),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    let json_limit = state.config.limits.json_grid_values;
    let sync_output_limit = state.config.limits.sync_result_values;
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancellation.clone();
    let result = state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let cells = snapshot
                .descriptor()
                .nx
                .checked_mul(snapshot.descriptor().ny)
                .ok_or(QueryError::LimitExceeded {
                    what: "JSON grid values",
                    requested: usize::MAX,
                    limit: json_limit,
                })?;
            if cells > json_limit {
                return Err(QueryError::LimitExceeded {
                    what: "JSON grid values",
                    requested: cells,
                    limit: json_limit,
                }
                .into());
            }
            let query = request.query();
            cache_or_compute(
                &cache,
                &metrics,
                "temporal_grid",
                &snapshot.descriptor().snapshot_id,
                &request,
                || {
                    reduce_temporal_grid_with_cancel_and_limits(
                        &snapshot,
                        &query,
                        TemporalReductionLimits {
                            max_reduction_cells: json_limit,
                            max_output_values: sync_output_limit,
                        },
                        || worker_cancellation.load(Ordering::Acquire),
                    )
                },
            )
        })
        .await;
    if matches!(result, Err(ExecutionError::ExecutionTimeout)) {
        // `spawn_blocking` cannot be preempted by dropping its JoinHandle.
        // Signal the tile/timestep checkpoints so timed-out work promptly
        // releases its heavy permit instead of continuing unseen.
        cancellation.store(true, Ordering::Release);
    }
    match result {
        Ok(Ok(bytes)) => json_bytes_with_etag(StatusCode::OK, bytes),
        Ok(Err(error)) => response_work_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn submit_temporal_grid_job(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<TemporalGridApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        request.variables.iter().map(String::as_str),
    ) {
        return response;
    }
    let request_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(error) => {
            error!(request_id = %request_id.0, %error, "job request serialization failed");
            return ProblemDetails::internal(request_id.0).into_response();
        }
    };
    // Resolve the immutable snapshot at admission time. A job may wait for a
    // heavy-work permit, and the named run can be atomically replaced during
    // that wait. Pinning the accepted generation prevents a 202 response from
    // silently changing meaning before execution begins.
    let catalog = state.catalog.clone();
    let model = request.model.clone();
    let run = request.run.clone();
    let accepted_snapshot_id = match state
        .run_light(move || {
            catalog
                .snapshot(&model, &run)
                .map(|snapshot| snapshot.descriptor().snapshot_id.clone())
        })
        .await
    {
        Ok(Ok(snapshot_id)) => snapshot_id,
        Ok(Err(error)) => return query_problem(error, request_id.0).into_response(),
        Err(error) => return execution_problem(error, request_id.0).into_response(),
    };
    let mut fingerprint_hasher = blake3::Hasher::new();
    fingerprint_hasher.update(b"rw-server.temporal-grid-job.v1\0");
    fingerprint_hasher.update(&(request_bytes.len() as u64).to_le_bytes());
    fingerprint_hasher.update(&request_bytes);
    fingerprint_hasher.update(accepted_snapshot_id.as_bytes());
    let fingerprint = fingerprint_hasher.finalize().to_hex().to_string();
    let (job, cancellation) = match state.jobs.create("temporal_grid", fingerprint) {
        Ok(created) => created,
        Err(error) => return job_problem(error, request_id.0).into_response(),
    };
    let job_id = job.id;
    let query = request.query();
    let job_manager = state.jobs.clone();
    let task_state = state.clone();
    tokio::spawn(async move {
        match job_manager.mark_running(job_id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                error!(%job_id, %error, "failed to start asynchronous job");
                return;
            }
        }
        let catalog = task_state.catalog.clone();
        let worker_cancellation = cancellation.clone();
        let cancellation_waiter = cancellation.clone();
        let work = task_state.run_heavy_job(move || {
            if worker_cancellation.is_cancelled() {
                return Err(JobWorkError::Cancelled);
            }
            let snapshot = catalog
                .snapshot(&request.model, &request.run)
                .map_err(JobWorkError::Query)?;
            if snapshot.descriptor().snapshot_id != accepted_snapshot_id {
                return Err(JobWorkError::Query(QueryError::ManifestInvalidated));
            }
            let result = reduce_temporal_grid_with_cancel(&snapshot, &query, || {
                worker_cancellation.is_cancelled()
            })
            .map_err(|error| match error {
                QueryError::Cancelled => JobWorkError::Cancelled,
                error => JobWorkError::Query(error),
            })?;
            if worker_cancellation.is_cancelled() {
                return Err(JobWorkError::Cancelled);
            }
            serde_json::to_vec(&result).map_err(JobWorkError::Json)
        });
        tokio::pin!(work);
        let result = tokio::select! {
            _ = wait_for_cancellation(cancellation_waiter) => return,
            result = &mut work => result,
        };
        match result {
            Ok(Ok(bytes)) => {
                if let Err(error) =
                    job_manager.succeed(job_id, "temporal-grid.json", "application/json", &bytes)
                {
                    error!(%job_id, %error, "failed to publish job artifact");
                    let code = if matches!(error, JobError::ResultTooLarge) {
                        "RESULT_TOO_LARGE"
                    } else {
                        "ARTIFACT_FAILED"
                    };
                    let _ = job_manager.fail(job_id, code);
                }
            }
            Ok(Err(JobWorkError::Cancelled)) => {
                let _ = job_manager.cancel(job_id);
            }
            Ok(Err(JobWorkError::Query(error))) => {
                error!(%job_id, %error, "asynchronous query failed");
                let _ = job_manager.fail(job_id, "QUERY_FAILED");
            }
            Ok(Err(JobWorkError::Json(error))) => {
                error!(%job_id, %error, "asynchronous result serialization failed");
                let _ = job_manager.fail(job_id, "SERIALIZATION_FAILED");
            }
            Err(error) => {
                // The blocking task retains its heavy-work permit after the
                // async deadline. Signal the reducer so it exits at its next
                // tile/timestep cancellation checkpoint.
                cancellation.cancel();
                error!(%job_id, %error, "asynchronous query execution failed");
                let code = match error {
                    ExecutionError::AdmissionTimeout => "ADMISSION_TIMEOUT",
                    ExecutionError::ExecutionTimeout => "DEADLINE_EXCEEDED",
                    ExecutionError::ShuttingDown => "SHUTTING_DOWN",
                    ExecutionError::Join(_) => "WORKER_FAILED",
                };
                let _ = job_manager.fail(job_id, code);
            }
        }
    });

    let mut response = json_with_etag(StatusCode::ACCEPTED, &job, request_id.0);
    if let Ok(value) = HeaderValue::from_str(&format!("/v1/jobs/{job_id}")) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    response
}

async fn wait_for_cancellation(cancellation: CancellationToken) {
    while !cancellation.is_cancelled() {
        // Job count is bounded, so a short cooperative poll is bounded too.
        // Dropping a still-admitting `run_heavy_job` future cancels its
        // semaphore wait; an already-running reducer sees the same token at
        // its next cancellation checkpoint.
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn get_job(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<JobPath>,
) -> Response {
    let id = match Uuid::parse_str(&path.id) {
        Ok(id) => id,
        Err(_) => {
            return ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "INVALID_JOB_ID",
                "Invalid job id",
                "Job identifiers must be canonical UUIDs.",
                request_id.0,
            )
            .into_response();
        }
    };
    match state.jobs.get(id) {
        Ok(job) => json_with_etag(StatusCode::OK, &job, request_id.0),
        Err(error) => job_problem(error, request_id.0).into_response(),
    }
}

async fn cancel_job(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<JobPath>,
) -> Response {
    let id = match Uuid::parse_str(&path.id) {
        Ok(id) => id,
        Err(_) => {
            return ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "INVALID_JOB_ID",
                "Invalid job id",
                "Job identifiers must be canonical UUIDs.",
                request_id.0,
            )
            .into_response();
        }
    };
    match state.jobs.cancel(id) {
        Ok(job) => {
            let status = if matches!(job.status, JobStatus::Cancelled) {
                StatusCode::ACCEPTED
            } else {
                StatusCode::OK
            };
            json_with_etag(status, &job, request_id.0)
        }
        Err(error) => job_problem(error, request_id.0).into_response(),
    }
}

async fn artifact(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ArtifactPath>,
    request: Request,
) -> Response {
    let artifact_path = match state.jobs.artifact_path(&path.hash, &path.file) {
        Ok(path) => path,
        Err(error) => return job_problem(error, request_id.0).into_response(),
    };
    match ServeFile::new(artifact_path).oneshot(request).await {
        Ok(mut response) => {
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("private, max-age=31536000, immutable"),
            );
            response.map(Body::new)
        }
        Err(error) => {
            error!(request_id = %request_id.0, %error, "artifact streaming failed");
            ProblemDetails::internal(request_id.0).into_response()
        }
    }
}

async fn metrics(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    match state.metrics.encode() {
        Ok(body) => (
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(_) => ProblemDetails::internal(request_id.0).into_response(),
    }
}

async fn fallback(Extension(request_id): Extension<RequestId>) -> Response {
    ProblemDetails::not_found(request_id.0).into_response()
}

fn parse_csv_variables(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect()
}

fn json_with_etag<T>(status: StatusCode, value: &T, request_id: Uuid) -> Response
where
    T: Serialize + ?Sized,
{
    let body = match serde_json::to_vec(value) {
        Ok(body) => body,
        Err(error) => {
            error!(%request_id, %error, "response serialization failed");
            return ProblemDetails::internal(request_id).into_response();
        }
    };
    let etag = format!("\"{}\"", blake3::hash(&body).to_hex());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn json_bytes_with_etag(status: StatusCode, body: Bytes) -> Response {
    let digest = blake3::hash(&body).to_hex();
    let etag = format!("{digest:?}");
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn cache_or_compute<Request, Output>(
    cache: &moka::sync::Cache<String, Bytes>,
    metrics: &crate::Metrics,
    endpoint: &str,
    snapshot_id: &str,
    request: &Request,
    compute: impl FnOnce() -> Result<Output, QueryError>,
) -> Result<Bytes, ResponseWorkError>
where
    Request: Serialize + ?Sized,
    Output: Serialize,
{
    let request_bytes = serde_json::to_vec(request)?;
    let request_hash = blake3::hash(&request_bytes).to_hex();
    let key = format!("v1:{endpoint}:{snapshot_id}:{request_hash}");
    if let Some(bytes) = cache.get(&key) {
        metrics.cache_hit();
        return Ok(bytes);
    }
    metrics.cache_miss();
    let output = compute()?;
    let bytes = Bytes::from(serde_json::to_vec(&output)?);
    cache.insert(key, bytes.clone());
    Ok(bytes)
}

fn query_problem(error: QueryError, request_id: Uuid) -> ProblemDetails {
    match error {
        QueryError::LimitExceeded { .. } => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "QUERY_LIMIT",
            "Query limit exceeded",
            "Reduce the requested variables, points, times, or grid size.",
            request_id,
        ),
        QueryError::InvalidRequest(_)
        | QueryError::InvalidLegacyRunSlug { .. }
        | QueryError::InvalidTimeRange { .. }
        | QueryError::EmptyTimeSelection
        | QueryError::PointOutsideGrid { .. } => ProblemDetails::new(
            StatusCode::BAD_REQUEST,
            "INVALID_QUERY",
            "Invalid query",
            "The request parameters are invalid for the selected data.",
            request_id,
        ),
        QueryError::UnknownModel(_)
        | QueryError::UnknownRun { .. }
        | QueryError::UnknownStorageSlot(_)
        | QueryError::UnknownVariable(_) => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "DATA_NOT_FOUND",
            "Requested data was not found",
            "The selected run does not contain the requested data.",
            request_id,
        ),
        QueryError::WrongVariableKind { .. }
        | QueryError::MissingExpectedTime { .. }
        | QueryError::MissingVariable { .. }
        | QueryError::MissingValue { .. }
        | QueryError::InvalidCategory { .. } => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "DATA_INCOMPATIBLE",
            "Stored data cannot satisfy the request",
            "Choose compatible variables or explicitly request partial missing-data handling.",
            request_id,
        ),
        QueryError::SnapshotInvalidated { .. }
        | QueryError::ManifestInvalidated
        | QueryError::InconsistentVariable { .. }
        | QueryError::VariableInventoryMismatch { .. } => ProblemDetails::new(
            StatusCode::CONFLICT,
            "SNAPSHOT_INVALIDATED",
            "Run changed during the query",
            "Retry the request against a newly resolved run snapshot.",
            request_id,
        ),
        QueryError::Store(error) => {
            error!(%request_id, %error, "store query failed");
            ProblemDetails::internal(request_id)
        }
        QueryError::Io(error) => {
            error!(%request_id, %error, "query I/O failed");
            ProblemDetails::internal(request_id)
        }
        QueryError::Json(error) => {
            error!(%request_id, %error, "query metadata decoding failed");
            ProblemDetails::internal(request_id)
        }
        QueryError::Allocation { what, detail } => {
            error!(%request_id, %what, %detail, "query allocation failed");
            ProblemDetails::internal(request_id)
        }
        QueryError::Cancelled => ProblemDetails::new(
            StatusCode::CONFLICT,
            "QUERY_CANCELLED",
            "Query cancelled",
            "The query was cancelled before completion.",
            request_id,
        ),
    }
}

fn execution_problem(error: ExecutionError, request_id: Uuid) -> ProblemDetails {
    match error {
        ExecutionError::AdmissionTimeout => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "BUSY",
            "Service is busy",
            "Retry after active work completes.",
            request_id,
        ),
        ExecutionError::ExecutionTimeout => ProblemDetails::new(
            StatusCode::GATEWAY_TIMEOUT,
            "DEADLINE_EXCEEDED",
            "Query deadline exceeded",
            "Narrow the request or submit it as an asynchronous job.",
            request_id,
        ),
        ExecutionError::ShuttingDown => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "SHUTTING_DOWN",
            "Service is shutting down",
            "Retry against a healthy instance.",
            request_id,
        ),
        ExecutionError::Join(error) => {
            error!(%request_id, %error, "blocking query worker failed");
            ProblemDetails::internal(request_id)
        }
    }
}

fn response_work_problem(error: ResponseWorkError, request_id: Uuid) -> ProblemDetails {
    match error {
        ResponseWorkError::Query(error) => query_problem(error, request_id),
        ResponseWorkError::Json(error) => {
            error!(%request_id, %error, "cached response serialization failed");
            ProblemDetails::internal(request_id)
        }
    }
}

fn job_problem(error: JobError, request_id: Uuid) -> ProblemDetails {
    match error {
        JobError::Capacity => ProblemDetails::new(
            StatusCode::TOO_MANY_REQUESTS,
            "JOB_CAPACITY",
            "Asynchronous job capacity is full",
            "Retry after active jobs finish.",
            request_id,
        ),
        JobError::NotFound(_) | JobError::ArtifactNotFound => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "JOB_NOT_FOUND",
            "Job or artifact was not found",
            "The requested asynchronous result does not exist.",
            request_id,
        ),
        JobError::InvalidTransition => ProblemDetails::new(
            StatusCode::CONFLICT,
            "JOB_STATE_CONFLICT",
            "Job state conflict",
            "The requested transition is not valid for the current job state.",
            request_id,
        ),
        JobError::ResultTooLarge => ProblemDetails::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "RESULT_TOO_LARGE",
            "Job result is too large",
            "Narrow the requested domain, times, variables, or operations.",
            request_id,
        ),
        JobError::Invalid(detail) => {
            error!(%request_id, %detail, "invalid job metadata or path");
            ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "INVALID_JOB_REQUEST",
                "Invalid job request",
                "The job or artifact identifier is invalid.",
                request_id,
            )
        }
        JobError::Io(error) => {
            error!(%request_id, %error, "job I/O failed");
            ProblemDetails::internal(request_id)
        }
        JobError::Json(error) => {
            error!(%request_id, %error, "job metadata decoding failed");
            ProblemDetails::internal(request_id)
        }
        JobError::Store(error) => {
            error!(%request_id, %error, "job durable publication failed");
            ProblemDetails::internal(request_id)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use rustwx_core::{GridShape, LatLonGrid};
    use rw_store::ingest::{DerivedFieldInput, write_hour_from_grid_with_derived_exact};
    use rw_store::run::RwsRunManifest;
    use rw_store::{PressureVolumeInput, RwsExactTime, RwsSourceProvenance};
    use std::fs;
    use tower::ServiceExt;

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const FIXTURE_MODEL: &str = "fixture-model";
    const LEGACY_RETIRED_MODEL: &str = "rrfs-firewx";
    const FIXTURE_RUN: &str = "fixture-run";
    const FIXTURE_ORIGIN: i64 = 1_700_000_000;

    fn test_app() -> (tempfile::TempDir, Router) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let router = build_router(AppState::new(config, tokens).unwrap()).unwrap();
        (directory, router)
    }

    fn test_app_with_store() -> (tempfile::TempDir, Router) {
        test_app_with_store_limit(crate::AppConfig::default().limits.sync_result_values)
    }

    fn test_app_with_store_limit(sync_result_values: usize) -> (tempfile::TempDir, Router) {
        let (directory, state) = test_state_with_store_limit(sync_result_values);
        let router = build_router(state).unwrap();
        (directory, router)
    }

    fn test_state_with_store_limit(sync_result_values: usize) -> (tempfile::TempDir, AppState) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.limits.sync_result_values = sync_result_values;
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![40.0, 40.0, 41.0, 41.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap();
        for (slot, lead, values) in [
            (0u16, 0u64, [1.0, 2.0, 3.0, 4.0]),
            (1u16, 900u64, [2.0, 3.0, 4.0, 5.0]),
        ] {
            let fields = [
                DerivedFieldInput {
                    name: "scalar",
                    units: "K",
                    values: &values,
                },
                DerivedFieldInput {
                    name: RETIRED_VARIABLE_NAME,
                    units: "1",
                    values: &values,
                },
            ];
            let pressure_850 = [280.0 + slot as f32; 4];
            let pressure_500 = [250.0 + slot as f32; 4];
            let volumes = [PressureVolumeInput {
                name: "temperature_iso",
                units: "K",
                selector_template: serde_json::json!({"fixture": "temperature_iso"}),
                levels: vec![(850, &pressure_850), (500, &pressure_500)],
            }];
            write_hour_from_grid_with_derived_exact(
                &config.server.store_root,
                FIXTURE_MODEL,
                FIXTURE_RUN,
                slot,
                RwsExactTime::new(lead, FIXTURE_ORIGIN + lead as i64),
                &grid,
                None,
                &[],
                &fields,
                &volumes,
                "rw-server-test",
                1_800_000_000 + u64::from(slot),
            )
            .unwrap();
        }
        // Keep one valid legacy run on disk. The storage/query libraries must
        // remain backwards-compatible with it while every public HTTP entry
        // point treats the retired namespace as nonexistent.
        let legacy_values = [10.0, 20.0, 30.0, 40.0];
        let legacy_fields = [DerivedFieldInput {
            name: "scalar",
            units: "K",
            values: &legacy_values,
        }];
        write_hour_from_grid_with_derived_exact(
            &config.server.store_root,
            LEGACY_RETIRED_MODEL,
            FIXTURE_RUN,
            0,
            RwsExactTime::new(0, FIXTURE_ORIGIN),
            &grid,
            None,
            &[],
            &legacy_fields,
            &[],
            "rw-server-test",
            1_800_000_100,
        )
        .unwrap();
        let manifest_path = config
            .server
            .store_root
            .join(FIXTURE_MODEL)
            .join(FIXTURE_RUN)
            .join("run.json");
        let mut manifest = RwsRunManifest::load(&manifest_path).unwrap();
        manifest.hours.get_mut(&0).unwrap().source_provenance = vec![
            RwsSourceProvenance::new(
                "ECMWF-OPEN-DATA",
                vec!["pressure".into()],
                vec!["oper".into()],
            )
            .unwrap(),
        ];
        manifest.hours.get_mut(&1).unwrap().source_provenance = vec![
            RwsSourceProvenance::new(
                "ecmwf-open-data",
                vec!["surface".into()],
                vec!["oper".into()],
            )
            .unwrap(),
        ];
        manifest.save(&manifest_path).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let state = AppState::new(config, tokens).unwrap();
        (directory, state)
    }

    async fn post_json(app: Router, path: &str, value: serde_json::Value) -> Response {
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(serde_json::to_vec(&value).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
    }

    async fn get_with_token(app: Router, path: &str) -> Response {
        app.oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
    }

    fn temporal_fixture_request() -> serde_json::Value {
        serde_json::json!({
            "model": FIXTURE_MODEL,
            "run": FIXTURE_RUN,
            "variables": ["scalar"],
            "semantics": {"kind": "instantaneous_scalar"},
            "reducer": "scalar_summary",
            "window": {
                "kind": "utc",
                "start_unix": FIXTURE_ORIGIN,
                "end_unix": FIXTURE_ORIGIN + 1800
            },
            "expectation": {"basis": "manifest_axis"},
            "missing_policy": "partial"
        })
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn assert_private_resource_is_not_found(response: Response) {
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            crate::problem::PROBLEM_CONTENT_TYPE
        );
        let problem = response_json(response).await;
        assert_eq!(problem["code"], "NOT_FOUND");
        let serialized = serde_json::to_string(&problem)
            .unwrap()
            .to_ascii_lowercase();
        assert!(!serialized.contains("fire"));
        assert!(!serialized.contains(RETIRED_VARIABLE_NAME));
    }

    async fn wait_for_terminal_job(app: &Router, id: &str) -> serde_json::Value {
        for _ in 0..200 {
            let response = get_with_token(app.clone(), &format!("/v1/jobs/{id}")).await;
            assert_eq!(response.status(), StatusCode::OK);
            let job = response_json(response).await;
            if matches!(
                job["status"].as_str(),
                Some("succeeded" | "failed" | "cancelled")
            ) {
                return job;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("job {id} did not reach a terminal state");
    }

    #[tokio::test]
    async fn health_is_public_but_data_routes_require_a_token() {
        let (directory, app) = test_app();
        // Legacy private namespace presence must not reintroduce the retired
        // identifier through dynamic local-store discovery.
        fs::create_dir_all(directory.path().join("store").join("rrfs-firewx")).unwrap();
        let health = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/health/live")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert!(health.headers().contains_key(REQUEST_ID_HEADER));

        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            denied.headers().get(header::CONTENT_TYPE).unwrap(),
            crate::problem::PROBLEM_CONTENT_TYPE
        );

        let allowed = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert!(allowed.headers().contains_key(header::ETAG));
        let body = to_bytes(allowed.into_body(), 1024 * 1024).await.unwrap();
        let models: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let models = models.as_array().expect("models response must be an array");
        assert!(!models.is_empty());
        assert!(models.iter().all(|model| model["id"] != "rrfs-firewx"));
        let wrf = models
            .iter()
            .find(|model| model["id"] == "wrf")
            .expect("local WRF import capability must be public");
        assert_eq!(wrf["ingest_status"], "local_import");
        assert_eq!(wrf["verification"], "local_import");

        let model = |id: &str| {
            models
                .iter()
                .find(|model| model["id"] == id)
                .unwrap_or_else(|| panic!("missing model capability {id}"))
        };
        let nbm = model("nbm");
        assert_eq!(
            nbm["limitations"],
            serde_json::json!(["surface_only", "conus_only"])
        );
        assert_eq!(nbm["products"][0]["product"], "core/co");
        assert_eq!(nbm["products"][0]["surface_source"], true);
        assert_eq!(nbm["products"][0]["pressure_source"], false);

        for id in ["aigefs", "hgefs"] {
            assert_eq!(
                model(id)["limitations"],
                serde_json::json!(["ensemble_mean_only", "derived_products_disabled"])
            );
        }
        assert_eq!(
            model("hiresw")["limitations"],
            serde_json::json!(["surface_only", "conus_only"])
        );
        for id in ["href", "sref"] {
            assert_eq!(
                model(id)["limitations"],
                serde_json::json!([
                    "ensemble_mean_only",
                    "sparse_pressure_levels",
                    "derived_products_disabled",
                    "conus_only"
                ])
            );
        }
        assert_eq!(
            model("refs")["limitations"],
            serde_json::json!([
                "ensemble_mean_only",
                "sparse_pressure_levels",
                "derived_products_disabled",
                "conus_only",
                "pre_operational_feed"
            ])
        );
    }

    #[tokio::test]
    async fn valid_legacy_retired_model_is_unaddressable_on_every_data_route() {
        let (directory, app) = test_app_with_store();
        assert!(
            directory
                .path()
                .join("store")
                .join(LEGACY_RETIRED_MODEL)
                .join(FIXTURE_RUN)
                .join("run.json")
                .is_file(),
            "the regression fixture must be a real readable legacy store"
        );

        for path in [
            format!("/v1/models/{LEGACY_RETIRED_MODEL}/runs"),
            format!("/v1/models/{LEGACY_RETIRED_MODEL}/runs/{FIXTURE_RUN}"),
            format!("/v1/models/{LEGACY_RETIRED_MODEL}/runs/{FIXTURE_RUN}/variables"),
            format!(
                "/v1/point?model={LEGACY_RETIRED_MODEL}&run={FIXTURE_RUN}&latitude=40&longitude=-100&variables=scalar"
            ),
        ] {
            assert_private_resource_is_not_found(get_with_token(app.clone(), &path).await).await;
        }

        let requests = [
            (
                "/v1/points",
                serde_json::json!({
                    "model": LEGACY_RETIRED_MODEL,
                    "run": FIXTURE_RUN,
                    "points": [{"latitude": 40.0, "longitude": -100.0}],
                    "variables": ["scalar"]
                }),
            ),
            (
                "/v1/profile",
                serde_json::json!({
                    "model": LEGACY_RETIRED_MODEL,
                    "run": FIXTURE_RUN,
                    "latitude": 40.0,
                    "longitude": -100.0,
                    "storage_slot": 0,
                    "variables": ["scalar"]
                }),
            ),
            (
                "/v1/window",
                serde_json::json!({
                    "model": LEGACY_RETIRED_MODEL,
                    "run": FIXTURE_RUN,
                    "storage_slot": 0,
                    "variable": "scalar",
                    "x0": 0,
                    "y0": 0,
                    "x1": 1,
                    "y1": 1
                }),
            ),
            (
                "/v1/analytics/spatial-series",
                serde_json::json!({
                    "model": LEGACY_RETIRED_MODEL,
                    "run": FIXTURE_RUN,
                    "variable": "scalar"
                }),
            ),
        ];
        for (path, request) in requests {
            assert_private_resource_is_not_found(post_json(app.clone(), path, request).await).await;
        }

        let mut temporal = temporal_fixture_request();
        temporal["model"] = serde_json::json!(LEGACY_RETIRED_MODEL);
        assert_private_resource_is_not_found(
            post_json(app.clone(), "/v1/analytics/temporal-grid", temporal.clone()).await,
        )
        .await;
        assert_private_resource_is_not_found(
            post_json(app, "/v1/jobs/temporal-grid", temporal).await,
        )
        .await;
    }

    #[tokio::test]
    async fn retired_variable_is_neither_catalogued_nor_queryable() {
        let (_directory, app) = test_app_with_store();
        let runs = get_with_token(app.clone(), &format!("/v1/models/{FIXTURE_MODEL}/runs")).await;
        assert_eq!(runs.status(), StatusCode::OK);
        let runs = response_json(runs).await;
        assert_eq!(
            runs[0]["variable_count"], 2,
            "the public count excludes the retired compatibility variable"
        );
        let variables = get_with_token(
            app.clone(),
            &format!("/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/variables"),
        )
        .await;
        assert_eq!(variables.status(), StatusCode::OK);
        let variables = response_json(variables).await;
        let serialized = serde_json::to_string(&variables)
            .unwrap()
            .to_ascii_lowercase();
        assert!(!serialized.contains("fire"));
        assert!(!serialized.contains(RETIRED_VARIABLE_NAME));

        let point = format!(
            "/v1/point?model={FIXTURE_MODEL}&run={FIXTURE_RUN}&latitude=40&longitude=-100&variables={RETIRED_VARIABLE_NAME}"
        );
        assert_private_resource_is_not_found(get_with_token(app.clone(), &point).await).await;

        let requests = [
            (
                "/v1/points",
                serde_json::json!({
                    "model": FIXTURE_MODEL,
                    "run": FIXTURE_RUN,
                    "points": [{"latitude": 40.0, "longitude": -100.0}],
                    "variables": [RETIRED_VARIABLE_NAME]
                }),
            ),
            (
                "/v1/profile",
                serde_json::json!({
                    "model": FIXTURE_MODEL,
                    "run": FIXTURE_RUN,
                    "latitude": 40.0,
                    "longitude": -100.0,
                    "storage_slot": 0,
                    "variables": [RETIRED_VARIABLE_NAME]
                }),
            ),
            (
                "/v1/window",
                serde_json::json!({
                    "model": FIXTURE_MODEL,
                    "run": FIXTURE_RUN,
                    "storage_slot": 0,
                    "variable": RETIRED_VARIABLE_NAME,
                    "x0": 0,
                    "y0": 0,
                    "x1": 1,
                    "y1": 1
                }),
            ),
            (
                "/v1/analytics/spatial-series",
                serde_json::json!({
                    "model": FIXTURE_MODEL,
                    "run": FIXTURE_RUN,
                    "variable": RETIRED_VARIABLE_NAME
                }),
            ),
        ];
        for (path, request) in requests {
            assert_private_resource_is_not_found(post_json(app.clone(), path, request).await).await;
        }

        let mut temporal = temporal_fixture_request();
        temporal["variables"] = serde_json::json!([RETIRED_VARIABLE_NAME]);
        assert_private_resource_is_not_found(
            post_json(app.clone(), "/v1/analytics/temporal-grid", temporal.clone()).await,
        )
        .await;
        assert_private_resource_is_not_found(
            post_json(app, "/v1/jobs/temporal-grid", temporal).await,
        )
        .await;
    }

    #[tokio::test]
    async fn readiness_rejects_a_non_directory_store_root() {
        let (directory, app) = test_app();
        let store = directory.path().join("store");
        fs::remove_dir(&store).unwrap();
        fs::write(&store, b"not a store directory").unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "NOT_READY");
    }

    #[tokio::test]
    async fn unknown_routes_return_problem_json_with_a_request_id() {
        let (_directory, app) = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().contains_key(REQUEST_ID_HEADER));
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "NOT_FOUND");
    }

    #[tokio::test]
    async fn missing_models_and_runs_return_not_found_at_catalog_and_job_admission() {
        let (_directory, app) = test_app_with_store();
        let missing_model = get_with_token(app.clone(), "/v1/models/not-present/runs").await;
        assert_eq!(missing_model.status(), StatusCode::NOT_FOUND);
        assert_eq!(response_json(missing_model).await["code"], "DATA_NOT_FOUND");

        let mut request = temporal_fixture_request();
        request["run"] = serde_json::json!("not-present");
        let missing_run = post_json(app, "/v1/jobs/temporal-grid", request).await;
        assert_eq!(missing_run.status(), StatusCode::NOT_FOUND);
        assert_eq!(response_json(missing_run).await["code"], "DATA_NOT_FOUND");
    }

    #[tokio::test]
    async fn framework_body_rejections_use_problem_json() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.limits.request_body_bytes = 64;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/points")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; 128]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE).unwrap(),
            crate::problem::PROBLEM_CONTENT_TYPE
        );
        assert!(response.headers().contains_key(REQUEST_ID_HEADER));
    }

    #[tokio::test]
    async fn variable_catalog_exposes_manual_only_temporal_capabilities() {
        let (_directory, app) = test_app_with_store();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/variables"
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let variables: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let scalar = variables
            .as_array()
            .unwrap()
            .iter()
            .find(|variable| variable["name"] == "scalar")
            .expect("scalar capability");
        assert_eq!(scalar["name"], "scalar");
        assert_eq!(scalar["temporal"]["value_class"], "unknown");
        assert_eq!(scalar["temporal"]["requires_manual_semantics"], true);
        assert_eq!(scalar["temporal"]["operations"], serde_json::json!([]));
        assert_eq!(scalar["scalar_temporal_reduction"], false);
    }

    #[tokio::test]
    async fn model_catalog_exposes_ecmwf_provider_terms() {
        let (_directory, app) = test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let models: serde_json::Value = serde_json::from_slice(&body).unwrap();
        for id in ["ecmwf-open-data", "aifs"] {
            let model = models
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["id"] == id)
                .unwrap_or_else(|| panic!("missing built-in {id} capability"));
            let attribution = &model["provider_attributions"][0];
            assert_eq!(
                attribution["copyright_statement"],
                "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF)."
            );
            assert_eq!(
                attribution["license"],
                "This ECMWF data is published under a Creative Commons Attribution 4.0 International (CC BY 4.0)."
            );
            assert_eq!(
                attribution["license_url"],
                "https://creativecommons.org/licenses/by/4.0/"
            );
            assert_eq!(
                attribution["terms_url"],
                "https://apps.ecmwf.int/datasets/licences/general/"
            );
            assert!(
                attribution["modification_notice"]
                    .as_str()
                    .unwrap()
                    .contains("has been subset")
            );
        }

        let hrrr = models
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "hrrr")
            .expect("missing built-in HRRR capability");
        let noaa = hrrr["provider_attributions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|attribution| {
                attribution["provider"]
                    .as_str()
                    .is_some_and(|provider| provider.contains("NOAA"))
            })
            .expect("NOAA attribution");
        assert!(
            noaa["modification_notice"]
                .as_str()
                .unwrap()
                .contains("not an official NOAA/NWS product")
        );
    }

    #[tokio::test]
    async fn run_detail_exposes_safe_persisted_provenance_union() {
        let (_directory, app) = test_app_with_store();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}"))
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let run: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            run["source_provenance"],
            serde_json::json!([{
                "provider": "ecmwf-open-data",
                "roles": ["pressure", "surface"],
                "products": ["oper"]
            }])
        );
        assert_eq!(
            run["provider_attributions"][0]["copyright_statement"],
            "This service is based on data and products of the European Centre for Medium-Range Weather Forecasts (ECMWF)."
        );
        assert!(
            run["provider_attributions"][0]["modification_notice"]
                .as_str()
                .unwrap()
                .contains("has been subset")
        );
        let serialized = serde_json::to_string(&run["source_provenance"]).unwrap();
        assert!(!serialized.contains("https://"));
        assert!(!serialized.contains("authorization"));
    }

    #[tokio::test]
    async fn multi_point_requests_share_one_output_value_budget() {
        let (_directory, app) = test_app_with_store_limit(3);
        let response = post_json(
            app,
            "/v1/points",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "points": [
                    {"latitude": 40.0, "longitude": -100.0},
                    {"latitude": 41.0, "longitude": -99.0}
                ],
                "variables": ["scalar"],
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let problem: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(problem["code"], "QUERY_LIMIT");
    }

    #[tokio::test]
    async fn authenticated_artifacts_are_private_cache_entries() {
        let (directory, app) = test_app();
        let hash = "a".repeat(64);
        let object = directory
            .path()
            .join("artifacts")
            .join("objects")
            .join(&hash);
        fs::create_dir_all(&object).unwrap();
        fs::write(object.join("result.json"), b"{}").unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/v1/artifacts/{hash}/result.json"))
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "private, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn phase_two_temporal_window_and_spatial_routes_return_real_data() {
        let (_directory, app) = test_app_with_store();

        let window = post_json(
            app.clone(),
            "/v1/window",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "storage_slot": 1,
                "variable": "scalar",
                "x0": 1,
                "y0": 0,
                "x1": 2,
                "y1": 2
            }),
        )
        .await;
        assert_eq!(window.status(), StatusCode::OK);
        let body = to_bytes(window.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["values"], serde_json::json!([3.0, 5.0]));

        let spatial = post_json(
            app.clone(),
            "/v1/analytics/spatial-series",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "variable": "scalar",
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(spatial.status(), StatusCode::OK);
        let body = to_bytes(spatial.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["samples"].as_array().unwrap().len(), 2);
        assert_eq!(value["samples"][0]["minimum"], 1.0);
        assert_eq!(value["samples"][1]["maximum"], 5.0);

        let temporal = post_json(
            app,
            "/v1/analytics/temporal-grid",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "variables": ["scalar"],
                "semantics": {"kind": "instantaneous_scalar"},
                "reducer": "scalar_summary",
                "window": {
                    "kind": "utc",
                    "start_unix": FIXTURE_ORIGIN,
                    "end_unix": FIXTURE_ORIGIN + 1800
                },
                "expectation": {"basis": "manifest_axis"},
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(temporal.status(), StatusCode::OK);
        let body = to_bytes(temporal.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["result"], "scalar");
        assert_eq!(value["data"]["minimum"][0], 1.0);
        assert_eq!(value["data"]["maximum"][0], 2.0);
        assert_eq!(
            value["data"]["metadata"]["semantics"]["kind"],
            "instantaneous_scalar"
        );
    }

    #[tokio::test]
    async fn production_api_surface_round_trips_catalog_queries_cache_jobs_and_artifacts() {
        let (directory, app) = test_app_with_store();

        let ready = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/health/ready")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
        assert_eq!(response_json(ready).await["status"], "ready");

        let models = response_json(get_with_token(app.clone(), "/v1/models").await).await;
        let fixture_model = models
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == FIXTURE_MODEL)
            .expect("fixture model in merged catalog");
        assert_eq!(fixture_model["stored_run_count"], 1);

        let runs = response_json(
            get_with_token(app.clone(), &format!("/v1/models/{FIXTURE_MODEL}/runs")).await,
        )
        .await;
        assert_eq!(runs.as_array().unwrap().len(), 1);
        assert_eq!(runs[0]["run"]["run"], FIXTURE_RUN);
        assert_eq!(runs[0]["run"]["sample_count"], 2);

        let variables = response_json(
            get_with_token(
                app.clone(),
                &format!("/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/variables"),
            )
            .await,
        )
        .await;
        assert!(
            variables
                .as_array()
                .unwrap()
                .iter()
                .any(|variable| variable["name"] == "scalar" && variable["point_series"] == true)
        );
        assert!(variables.as_array().unwrap().iter().any(|variable| {
            variable["name"] == "temperature_iso" && variable["pressure_profile"] == true
        }));

        let point_path = format!(
            "/v1/point?model={FIXTURE_MODEL}&run={FIXTURE_RUN}&latitude=40&longitude=-100&variables=scalar&missing_policy=partial"
        );
        let first_point = get_with_token(app.clone(), &point_path).await;
        assert_eq!(first_point.status(), StatusCode::OK);
        let first_etag = first_point.headers()[header::ETAG].clone();
        let first_point = response_json(first_point).await;
        assert_eq!(
            first_point["variables"][0]["values"],
            serde_json::json!([1.0, 2.0])
        );

        let second_point = get_with_token(app.clone(), &point_path).await;
        assert_eq!(second_point.status(), StatusCode::OK);
        assert_eq!(second_point.headers()[header::ETAG], first_etag);
        let second_point = response_json(second_point).await;
        assert_eq!(second_point, first_point);

        let points = post_json(
            app.clone(),
            "/v1/points",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "points": [
                    {"latitude": 40.0, "longitude": -100.0},
                    {"latitude": 41.0, "longitude": -99.0}
                ],
                "variables": ["scalar"],
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(points.status(), StatusCode::OK);
        let points = response_json(points).await;
        assert_eq!(points.as_array().unwrap().len(), 2);
        assert_eq!(
            points[1]["variables"][0]["values"],
            serde_json::json!([4.0, 5.0])
        );

        let profile = post_json(
            app.clone(),
            "/v1/profile",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "latitude": 40.5,
                "longitude": -99.5,
                "storage_slot": 0,
                "variables": ["temperature_iso"]
            }),
        )
        .await;
        assert_eq!(profile.status(), StatusCode::OK);
        let profile = response_json(profile).await;
        assert_eq!(
            profile["variables"][0]["levels_hpa"],
            serde_json::json!([850, 500])
        );
        assert_eq!(
            profile["variables"][0]["values"],
            serde_json::json!([280.0, 250.0])
        );

        let job_response = post_json(
            app.clone(),
            "/v1/jobs/temporal-grid",
            temporal_fixture_request(),
        )
        .await;
        assert_eq!(job_response.status(), StatusCode::ACCEPTED);
        let location = job_response.headers()[header::LOCATION]
            .to_str()
            .unwrap()
            .to_string();
        let submitted = response_json(job_response).await;
        let job_id = submitted["id"].as_str().unwrap();
        assert_eq!(location, format!("/v1/jobs/{job_id}"));
        let finished = wait_for_terminal_job(&app, job_id).await;
        assert_eq!(finished["status"], "succeeded");
        let artifact_path = finished["artifact"]["download_path"].as_str().unwrap();

        let denied_artifact = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(artifact_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_artifact.status(), StatusCode::UNAUTHORIZED);
        let artifact = get_with_token(app.clone(), artifact_path).await;
        assert_eq!(artifact.status(), StatusCode::OK);
        assert_eq!(
            artifact.headers()[header::CACHE_CONTROL],
            "private, max-age=31536000, immutable"
        );
        let artifact_json = response_json(artifact).await;
        assert_eq!(artifact_json["result"], "scalar");
        assert_eq!(artifact_json["data"]["minimum"][0], 1.0);

        // Replacing even an otherwise compatible run manifest produces a new
        // snapshot key, so cached data from the accepted generation is never
        // served as the replacement generation.
        let manifest_path = directory
            .path()
            .join("store")
            .join(FIXTURE_MODEL)
            .join(FIXTURE_RUN)
            .join("run.json");
        let mut manifest = RwsRunManifest::load(&manifest_path).unwrap();
        manifest.writer.build = "rw-server-test-replacement".to_string();
        manifest.save(&manifest_path).unwrap();
        let replacement_point = get_with_token(app.clone(), &point_path).await;
        assert_eq!(replacement_point.status(), StatusCode::OK);
        assert_ne!(replacement_point.headers()[header::ETAG], first_etag);
        let replacement_point = response_json(replacement_point).await;
        assert_ne!(
            replacement_point["run"]["snapshot_id"],
            first_point["run"]["snapshot_id"]
        );
        assert_eq!(
            replacement_point["variables"][0]["values"],
            first_point["variables"][0]["values"]
        );

        let metrics = get_with_token(app, "/metrics").await;
        assert_eq!(metrics.status(), StatusCode::OK);
        let metrics = to_bytes(metrics.into_body(), 256 * 1024).await.unwrap();
        let metrics = String::from_utf8(metrics.to_vec()).unwrap();
        assert!(metrics.contains("rw_response_cache_hits_total 1"));
        assert!(metrics.contains("rw_response_cache_misses_total 4"));
    }

    #[tokio::test]
    async fn asynchronous_jobs_reject_snapshot_substitution_after_admission() {
        let (directory, state) =
            test_state_with_store_limit(crate::AppConfig::default().limits.sync_result_values);
        let permits = state.config.limits.heavy_concurrency;
        let release = std::sync::Arc::new(std::sync::Barrier::new(permits + 1));
        let mut blockers = Vec::new();
        for _ in 0..permits {
            let blocker_state = state.clone();
            let blocker_release = release.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            blockers.push(tokio::spawn(async move {
                blocker_state
                    .run_heavy_job(move || {
                        let _ = started_tx.send(());
                        blocker_release.wait();
                    })
                    .await
                    .unwrap();
            }));
            started_rx.await.unwrap();
        }
        let app = build_router(state).unwrap();

        let submitted = post_json(
            app.clone(),
            "/v1/jobs/temporal-grid",
            temporal_fixture_request(),
        )
        .await;
        assert_eq!(submitted.status(), StatusCode::ACCEPTED);
        let submitted = response_json(submitted).await;
        let job_id = submitted["id"].as_str().unwrap().to_string();

        let manifest_path = directory
            .path()
            .join("store")
            .join(FIXTURE_MODEL)
            .join(FIXTURE_RUN)
            .join("run.json");
        let mut manifest = RwsRunManifest::load(&manifest_path).unwrap();
        manifest.writer.build = "replacement-before-job-execution".to_string();
        manifest.save(&manifest_path).unwrap();

        release.wait();
        for blocker in blockers {
            blocker.await.unwrap();
        }
        let finished = wait_for_terminal_job(&app, &job_id).await;
        assert_eq!(finished["status"], "failed");
        assert_eq!(finished["error_code"], "QUERY_FAILED");
        assert!(finished["artifact"].is_null());
    }

    #[tokio::test]
    async fn queued_asynchronous_jobs_are_cancellable_through_the_http_api() {
        let (_directory, state) =
            test_state_with_store_limit(crate::AppConfig::default().limits.sync_result_values);
        let permits = state.config.limits.heavy_concurrency;
        let release = std::sync::Arc::new(std::sync::Barrier::new(permits + 1));
        let mut blockers = Vec::new();
        for _ in 0..permits {
            let blocker_state = state.clone();
            let blocker_release = release.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            blockers.push(tokio::spawn(async move {
                blocker_state
                    .run_heavy_job(move || {
                        let _ = started_tx.send(());
                        blocker_release.wait();
                    })
                    .await
                    .unwrap();
            }));
            started_rx.await.unwrap();
        }
        let app = build_router(state).unwrap();
        let submitted = post_json(
            app.clone(),
            "/v1/jobs/temporal-grid",
            temporal_fixture_request(),
        )
        .await;
        assert_eq!(submitted.status(), StatusCode::ACCEPTED);
        let submitted = response_json(submitted).await;
        let job_id = submitted["id"].as_str().unwrap().to_string();

        let cancelled = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri(format!("/v1/jobs/{job_id}"))
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancelled.status(), StatusCode::ACCEPTED);
        let cancelled = response_json(cancelled).await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["error_code"], "CANCELLED");

        release.wait();
        for blocker in blockers {
            blocker.await.unwrap();
        }
        let finished = wait_for_terminal_job(&app, &job_id).await;
        assert_eq!(finished["status"], "cancelled");
        assert!(finished["artifact"].is_null());
    }
}

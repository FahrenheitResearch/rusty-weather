use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use rustwx_core::{ModelId, SourceId};
use rw_community_protocol::{
    BeginRunGenerationRequest, CaseRoomManifest, FinalizeRunGenerationRequest,
    PublishCaseArtifactRequest, ResolveObjectRequest, RevokePublicationRequest,
    RevokeRunGenerationRequest,
};
use rw_federation_proxy::{
    FEDERATION_HOP_HEADER, FEDERATION_LOCAL_OBJECT_PATH_PREFIX, FEDERATION_LOCAL_RESOLVE_PATH,
    FEDERATION_PROXY_PATH, FederationProxyError, FederationProxyRequest,
};
use rw_ingest::{
    IngestCapabilityLimitation, IngestSupportStatus, indexed_subset_available,
    model_ingest_capability,
};
use rw_observations::encode_plane_blob;
use rw_query::{
    GeographicBoundingBox, GeographicVerticalSelection, GeographicWindowLimits,
    GeographicWindowRequest, IndexWindow2DRequest, IntervalSupport, MissingPolicy,
    PointSeriesRequest, PointSeriesResult, ProfileCycleRequest, ProfileRequest, QueryError,
    SpatialStatsSeriesRequest, TemporalCapabilityBasis, TemporalGridRequest, TemporalOperation,
    TemporalReducer, TemporalReductionLimits, TemporalSemantics, TemporalValueClass,
    TemporalVerticalSelection, TemporalWindow, TimeExpectation, TimeRange, VariableCapability,
    query_geographic_window_with_cancel, query_point_series, query_profile,
    query_profile_cycle_with_cancel, query_spatial_stats_series, query_window_2d,
    reduce_temporal_grid_with_cancel, reduce_temporal_grid_with_cancel_and_limits,
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

use crate::community::CommunityError;
use crate::community_relay::{
    CommunityRelayError, HistoricalRelayLookupRequest, RelayAdvertiseRequest,
    RelayGrantPollRequest, RelayKillSwitchRequest, RelayRouteRegistrationRequest,
    RelaySessionCompletionRequest, RelaySessionFailureRequest, RelayTransportGrantRequest,
};
use crate::config::ConfigError;
use crate::federation::FederationError;
use crate::federation_proxy::{FederationProxyControlError, FederationProxyKillSwitchRequest};
use crate::generation_replication::{GenerationReplicationError, ReplicationKillSwitchRequest};
use crate::problem::ProblemDetails;
use crate::{AppState, CancellationToken, ExecutionError, JobError, JobStatus};

const REQUEST_ID_HEADER: &str = "x-request-id";
const RETIRED_MODEL_ID: ModelId = ModelId::RrfsFireWx;
const RETIRED_VARIABLE_NAME: &str = "fire_weather_composite";
/// Model forecast planes ship in the same versioned `RWOBF32` f32 container as
/// observation planes, under their own media type so the payload's domain is
/// never inferred from the container alone.
const MODEL_PLANE_CONTENT_TYPE: &str = "application/vnd.rusty-weather.model-plane+f32";

/// Every explicitly registered production HTTP operation in [`build_router`]
/// and its merged route modules. Axum's automatic `HEAD` companions for `GET`,
/// the configured CORS layer's `OPTIONS` handling, and the catch-all fallback
/// are protocol mechanics rather than separately documented operations.
///
/// Keep this independent inventory byte-for-byte aligned with the router. The
/// generated-contract tests compare it to OpenAPI so a documented operation
/// cannot be left as an acceptance follow-up.
pub const PRODUCTION_ROUTE_MANIFEST: &[(&str, &str)] = &[
    ("GET", "/metrics"),
    ("POST", "/v1/analytics/spatial-series"),
    ("POST", "/v1/analytics/temporal-grid"),
    ("GET", "/v1/artifacts/{hash}/{file}"),
    ("POST", "/v1/community/artifacts"),
    ("POST", "/v1/community/artifacts/{sha256}/revoke"),
    ("GET", "/v1/community/cases"),
    ("POST", "/v1/community/cases"),
    ("GET", "/v1/community/cases/{case_id}"),
    ("POST", "/v1/community/cases/{case_id}/revoke"),
    ("GET", "/v1/community/generation-replication/capabilities"),
    ("POST", "/v1/community/generation-replication/operator/gc"),
    (
        "POST",
        "/v1/community/generation-replication/operator/kill-switch",
    ),
    (
        "GET",
        "/v1/community/generation-replication/operator/status",
    ),
    ("GET", "/v1/community/generation-replication/owner"),
    ("GET", "/v1/community/generations"),
    ("POST", "/v1/community/generations"),
    ("DELETE", "/v1/community/generations/{generation_id}"),
    ("GET", "/v1/community/generations/{generation_id}"),
    (
        "POST",
        "/v1/community/generations/{generation_id}/chunks/{sha256}",
    ),
    ("POST", "/v1/community/generations/{generation_id}/finalize"),
    ("GET", "/v1/community/generations/{generation_id}/missing"),
    (
        "GET",
        "/v1/community/generations/{generation_id}/publication",
    ),
    ("POST", "/v1/community/generations/{generation_id}/revoke"),
    ("GET", "/v1/community/objects/{sha256}"),
    ("POST", "/v1/community/objects/resolve"),
    ("POST", "/v1/community/relay/advertisements"),
    ("POST", "/v1/community/relay/grants/next"),
    ("POST", "/v1/community/relay/historical/lookups"),
    ("POST", "/v1/community/relay/operator/kill-switch"),
    ("GET", "/v1/community/relay/operator/status"),
    ("POST", "/v1/community/relay/routes"),
    (
        "POST",
        "/v1/community/relay/sessions/{session_id}/grants/{role}",
    ),
    ("POST", "/v1/community/relay/sessions/complete"),
    ("POST", "/v1/community/relay/sessions/fail"),
    ("POST", "/v1/community/relay/sessions/revoke"),
    ("POST", "/v1/community/relay/transport"),
    ("GET", "/v1/federation/health"),
    ("GET", "/v1/federation/objects/{sha256}"),
    ("POST", "/v1/federation/objects/resolve"),
    ("POST", "/v1/federation/objects/resolve-local"),
    ("GET", "/v1/federation/origins"),
    ("GET", "/v1/federation/origins/{origin_id}"),
    ("POST", "/v1/federation/proxy/operator/kill-switch"),
    ("GET", "/v1/federation/proxy/operator/status"),
    ("POST", "/v1/geographic-window"),
    ("GET", "/v1/health/live"),
    ("GET", "/v1/health/ready"),
    ("DELETE", "/v1/jobs/{id}"),
    ("GET", "/v1/jobs/{id}"),
    ("POST", "/v1/jobs/temporal-grid"),
    ("GET", "/v1/models"),
    ("GET", "/v1/models/{model}/latest-run"),
    ("GET", "/v1/models/{model}/runs"),
    ("GET", "/v1/models/{model}/runs/{run}"),
    (
        "GET",
        "/v1/models/{model}/runs/{run}/planes/{storage_slot}/{variable}",
    ),
    ("GET", "/v1/models/{model}/runs/{run}/variables"),
    ("GET", "/v1/observations"),
    ("GET", "/v1/observations/{model}/{run}/frames"),
    (
        "GET",
        "/v1/observations/{model}/{run}/frames/{storage_slot}/{variable}",
    ),
    ("GET", "/v1/observations/{model}/{run}/grid.bin"),
    ("GET", "/v1/observations/capabilities"),
    ("POST", "/v1/observations/generated"),
    ("POST", "/v1/observations/mrms/ingest/refresh"),
    ("GET", "/v1/observations/mrms/ingest/status"),
    ("POST", "/v1/observations/mrms/latest"),
    ("POST", "/v1/observations/nexrad/level2"),
    ("POST", "/v1/observations/nexrad/level2/ingest/refresh"),
    ("GET", "/v1/observations/nexrad/level2/ingest/status"),
    ("POST", "/v1/observations/radar/mosaic"),
    ("POST", "/v1/observations/wrf-radar/derive"),
    ("GET", "/v1/openapi.json"),
    ("POST", "/v1/ops/storms/authoritative/nexrad-level3/decode"),
    ("POST", "/v1/ops/storms/cells"),
    ("GET", "/v1/ops/storms/methods"),
    ("GET", "/v1/ops/storms/models"),
    ("GET", "/v1/ops/storms/status"),
    ("GET", "/v1/origin-catalog/status"),
    ("GET", "/v1/point"),
    ("POST", "/v1/points"),
    ("POST", "/v1/profile"),
    ("POST", "/v1/profile-cycle"),
    (
        "GET",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json",
    ),
    (
        "GET",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{source_revision}/{z}/{x}/{y}",
    ),
    (
        "GET",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{z}/{x}/{y}",
    ),
    (
        "GET",
        "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}",
    ),
    ("GET", "/v1/satellite/{platform}/{sector}/{product}/frames"),
    ("GET", "/v1/satellite/catalog"),
    ("GET", "/v1/satellite/prewarm/status"),
    ("GET", "/v1/version"),
    ("POST", "/v1/window"),
];

#[derive(Debug, Clone, Copy)]
pub(crate) struct RequestId(pub(crate) Uuid);

#[derive(Debug, Clone)]
struct AuthPrincipal(String);

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
    /// `ready` and `degraded` both mean the core server can accept traffic.
    /// Degraded identifies optional data followers only; HTTP 503 is reserved
    /// for a failed core probe or an explicit operator readiness gate.
    status: &'static str,
    uptime_seconds: u64,
    degraded_subsystems: Vec<&'static str>,
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
    ProviderStatisticsOnly,
    EnsembleControlMemberOnly,
    SparsePressureLevels,
    TwoDimensionalStatisticsOnly,
    DerivedProductsDisabled,
    ConusOnly,
    PreOperationalFeed,
    ExtendedRangeNotScheduled,
}

impl From<IngestCapabilityLimitation> for ApiIngestCapabilityLimitation {
    fn from(value: IngestCapabilityLimitation) -> Self {
        match value {
            IngestCapabilityLimitation::AnalysisOnly => Self::AnalysisOnly,
            IngestCapabilityLimitation::SurfaceOnly => Self::SurfaceOnly,
            IngestCapabilityLimitation::EnsembleMeanOnly => Self::EnsembleMeanOnly,
            IngestCapabilityLimitation::ProviderStatisticsOnly => Self::ProviderStatisticsOnly,
            IngestCapabilityLimitation::EnsembleControlMemberOnly => {
                Self::EnsembleControlMemberOnly
            }
            IngestCapabilityLimitation::SparsePressureLevels => Self::SparsePressureLevels,
            IngestCapabilityLimitation::TwoDimensionalStatisticsOnly => {
                Self::TwoDimensionalStatisticsOnly
            }
            IngestCapabilityLimitation::DerivedProductsDisabled => Self::DerivedProductsDisabled,
            IngestCapabilityLimitation::ConusOnly => Self::ConusOnly,
            IngestCapabilityLimitation::PreOperationalFeed => Self::PreOperationalFeed,
            IngestCapabilityLimitation::ExtendedRangeNotScheduled => {
                Self::ExtendedRangeNotScheduled
            }
        }
    }
}

fn provider_attributions(
    summary: &rustwx_models::ModelSummary,
) -> Vec<ProviderAttributionResponse> {
    let mut attributions = Vec::with_capacity(4);
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
    if summary
        .sources
        .iter()
        .any(|source| source.id == SourceId::Eccc)
    {
        let attribution = match summary.id {
            ModelId::Geps => rw_query::geps_provider_attribution(),
            ModelId::Reps => rw_query::reps_provider_attribution(),
            ModelId::GdpsGeml => rw_query::gdps_geml_provider_attribution(),
            _ => rw_query::eccc_provider_attribution(),
        };
        attributions.push(attribution.into());
    }
    if summary
        .sources
        .iter()
        .any(|source| source.id == SourceId::Cma)
    {
        attributions.push(rw_query::cma_provider_attribution().into());
    }
    if summary
        .sources
        .iter()
        .any(|source| source.id == SourceId::Dwd)
    {
        attributions.push(rw_query::dwd_provider_attribution().into());
    }
    if summary.sources.iter().any(|source| {
        matches!(
            source.id,
            SourceId::RoshydrometWis2Cache | SourceId::RoshydrometWis2Origin
        )
    }) {
        attributions.push(rw_query::roshydromet_provider_attribution().into());
    }
    if summary
        .sources
        .iter()
        .any(|source| source.id == SourceId::Cptec)
    {
        attributions.push(rw_query::cptec_provider_attribution().into());
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

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApiMissingPolicy {
    #[default]
    Strict,
    Partial,
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

#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct ProfileCycleApiRequest {
    model: String,
    run: String,
    latitude: f64,
    longitude: f64,
    variables: Vec<String>,
    #[serde(default)]
    surface_variables: Vec<String>,
    start_unix: Option<i64>,
    end_unix: Option<i64>,
    #[serde(default)]
    missing_policy: ApiMissingPolicy,
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

#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
#[serde(tag = "basis", rename_all = "snake_case")]
pub enum ApiTimeExpectation {
    #[default]
    ManifestAxis,
    FixedCadence {
        step_seconds: u64,
        anchor_unix: Option<i64>,
    },
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
    profile_cycle: bool,
    geographic_window: bool,
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
            profile_cycle: value.profile_cycle,
            geographic_window: value.geographic_window,
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
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GeographicVerticalApiSelection {
    Surface2d,
    PressureLevels { levels_hpa: Vec<u16> },
}

impl From<&GeographicVerticalApiSelection> for GeographicVerticalSelection {
    fn from(value: &GeographicVerticalApiSelection) -> Self {
        match value {
            GeographicVerticalApiSelection::Surface2d => Self::Surface2d,
            GeographicVerticalApiSelection::PressureLevels { levels_hpa } => Self::PressureLevels {
                levels_hpa: levels_hpa.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GeographicWindowApiRequest {
    model: String,
    run: String,
    expected_snapshot_id: String,
    expected_grid_hash: String,
    storage_slot: u16,
    variables: Vec<String>,
    west_longitude: f64,
    south_latitude: f64,
    east_longitude: f64,
    north_latitude: f64,
    vertical: GeographicVerticalApiSelection,
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

/// `{variable}` captures the whole filename, including its required `.bin`
/// suffix, because matchit 0.8 (axum 0.8) does not support dynamic suffixes:
/// a `{variable}.bin` template would panic at router construction. The
/// observation plane route uses the same shape for the same reason.
#[derive(Debug, Deserialize)]
struct ModelPlanePath {
    model: String,
    run: String,
    storage_slot: u16,
    variable: String,
}

/// The identity guards are modelled as optional here only so a missing one
/// answers with `application/problem+json` like every other invalid request,
/// instead of axum's plain-text query rejection. The handler requires both.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelPlaneQuery {
    expected_snapshot_id: Option<String>,
    expected_grid_hash: Option<String>,
    level_hpa: Option<u16>,
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

#[derive(Debug, Deserialize)]
struct CommunityObjectPath {
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct CommunityCasePath {
    case_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommunityCaseDirectoryQuery {
    after: Option<String>,
    #[serde(default = "default_case_directory_limit")]
    limit: usize,
}

const fn default_case_directory_limit() -> usize {
    50
}

impl CommunityCaseDirectoryQuery {
    fn validate(&self) -> bool {
        (1..=rw_community_protocol::MAX_CASE_DIRECTORY_PAGE).contains(&self.limit)
            && self.after.as_ref().is_none_or(|cursor| {
                !cursor.is_empty()
                    && cursor.len() <= 128
                    && cursor
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            })
    }
}

#[derive(Debug, Deserialize)]
struct RelaySessionGrantPath {
    session_id: String,
    role: String,
}

#[derive(Debug, Deserialize)]
struct FederationOriginPath {
    origin_id: String,
}

#[derive(Debug, Deserialize)]
struct ReplicationGenerationPath {
    generation_id: String,
}

#[derive(Debug, Deserialize)]
struct ReplicationChunkPath {
    generation_id: String,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicationMissingQuery {
    after: Option<String>,
    #[serde(default = "default_replication_missing_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicationOwnerListQuery {
    after: Option<String>,
    #[serde(default = "default_replication_owner_list_limit")]
    limit: usize,
}

const fn default_replication_owner_list_limit() -> usize {
    50
}

const fn default_replication_missing_limit() -> usize {
    256
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

    // Only conventional operational reads are publication-gated. Job result,
    // case-room, federation, and relay control routes remain independently
    // available and never gain a StoreCatalog bypass through this router.
    let operational = Router::new()
        .route("/v1/models", get(list_models))
        .route("/v1/models/{model}/runs", get(list_runs))
        .route("/v1/models/{model}/latest-run", get(latest_run))
        .route("/v1/models/{model}/runs/{run}", get(run_detail))
        .route(
            "/v1/models/{model}/runs/{run}/planes/{storage_slot}/{variable}",
            get(model_plane_binary),
        )
        .route(
            "/v1/models/{model}/runs/{run}/variables",
            get(run_variables),
        )
        .route("/v1/point", get(point))
        .route("/v1/points", post(points))
        .route("/v1/profile", post(profile))
        .route("/v1/profile-cycle", post(profile_cycle))
        .route("/v1/analytics/temporal-grid", post(temporal_grid))
        .route("/v1/jobs/temporal-grid", post(submit_temporal_grid_job))
        .route("/v1/window", post(window))
        .route("/v1/geographic-window", post(geographic_window))
        .route("/v1/analytics/spatial-series", post(spatial_series))
        .merge(crate::observations::read_router())
        .merge(crate::satellite::read_router())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_origin_catalog_ready,
        ));

    let protected = Router::new()
        .merge(operational)
        .merge(crate::observations::write_router(&state.config.limits))
        .route("/v1/jobs/{id}", get(get_job).delete(cancel_job))
        .route("/v1/artifacts/{hash}/{file}", get(artifact))
        .route(
            rw_community_protocol::RESOLVE_OBJECT_PATH,
            post(resolve_community_object),
        )
        .route("/v1/community/objects/{sha256}", get(community_object))
        .route(
            rw_community_protocol::PUBLISH_CASE_ARTIFACT_PATH,
            post(publish_community_case_artifact),
        )
        .route(
            rw_community_protocol::REVOKE_CASE_ARTIFACT_PATH_TEMPLATE,
            post(revoke_community_case_artifact),
        )
        .route(
            rw_community_protocol::CREATE_CASE_PATH,
            get(list_community_cases).post(publish_community_case),
        )
        .route("/v1/community/cases/{case_id}", get(community_case))
        .route(
            rw_community_protocol::REVOKE_CASE_PATH_TEMPLATE,
            post(revoke_community_case),
        )
        .route(
            "/v1/community/relay/advertisements",
            post(advertise_community_relay_object),
        )
        .route(
            "/v1/community/relay/historical/lookups",
            post(lookup_community_relay_historical),
        )
        .route(
            "/v1/community/relay/grants/next",
            post(next_community_relay_grant),
        )
        .route(
            "/v1/community/relay/sessions/{session_id}/grants/{role}",
            post(community_relay_session_grant),
        )
        .route(
            "/v1/community/relay/routes",
            post(register_community_relay_route),
        )
        .route(
            "/v1/community/relay/transport",
            post(community_relay_transport_grant),
        )
        .route(
            "/v1/community/relay/sessions/complete",
            post(complete_community_relay_session),
        )
        .route(
            "/v1/community/relay/sessions/fail",
            post(fail_community_relay_session),
        )
        .route(
            "/v1/community/relay/sessions/revoke",
            post(revoke_community_relay_session),
        )
        .route(
            "/v1/community/relay/operator/kill-switch",
            post(set_community_relay_kill_switch),
        )
        .route(
            "/v1/community/relay/operator/status",
            get(community_relay_status),
        )
        .route(
            rw_community_protocol::FEDERATION_CATALOG_PATH,
            get(federation_catalog),
        )
        .route(
            rw_community_protocol::FEDERATION_ORIGIN_PATH_TEMPLATE,
            get(federation_origin),
        )
        .route("/v1/federation/health", get(federation_health))
        .route(FEDERATION_PROXY_PATH, post(resolve_federation_proxy))
        .route(
            "/v1/federation/proxy/operator/status",
            get(federation_proxy_operator_status),
        )
        .route(
            "/v1/federation/proxy/operator/kill-switch",
            post(set_federation_proxy_kill_switch),
        )
        .route("/v1/origin-catalog/status", get(origin_catalog_status))
        .route(
            "/v1/community/generation-replication/owner",
            get(generation_replication_owner),
        )
        .route(
            rw_community_protocol::RUN_GENERATION_CAPABILITIES_PATH,
            get(generation_replication_capabilities),
        )
        .route(
            rw_community_protocol::BEGIN_RUN_GENERATION_PATH,
            get(list_generation_replication_records).post(begin_generation_replication),
        )
        .route(
            "/v1/community/generations/{generation_id}",
            get(generation_replication_status).delete(cancel_generation_replication),
        )
        .route(
            rw_community_protocol::RUN_GENERATION_PUBLICATION_PATH_TEMPLATE,
            get(generation_replication_publication),
        )
        .route(
            "/v1/community/generations/{generation_id}/missing",
            get(generation_replication_missing),
        )
        .route(
            "/v1/community/generations/{generation_id}/chunks/{sha256}",
            post(upload_generation_replication_chunk).layer(DefaultBodyLimit::max(
                usize::try_from(
                    state
                        .config
                        .generation_replication
                        .limits
                        .maximum_chunk_bytes,
                )
                .unwrap_or(usize::MAX),
            )),
        )
        .route(
            "/v1/community/generations/{generation_id}/finalize",
            post(finalize_generation_replication),
        )
        .route(
            "/v1/community/generations/{generation_id}/revoke",
            post(revoke_generation_replication),
        )
        .route(
            "/v1/community/generation-replication/operator/status",
            get(generation_replication_operator_status),
        )
        .route(
            "/v1/community/generation-replication/operator/kill-switch",
            post(set_generation_replication_kill_switch),
        )
        .route(
            "/v1/community/generation-replication/operator/gc",
            post(run_generation_replication_gc),
        )
        .merge(crate::mrms_ingest::router(state.clone()))
        .merge(crate::nexrad_level2_ingest::router(state.clone()))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authentication,
        ));

    // Dedicated public-origin service-to-service routes. Ordinary BowEcho API
    // tokens cannot authenticate here, and the one-hop header is consumed by
    // the handler rather than forwarded to another origin.
    let federation_origin = Router::new()
        .route(
            FEDERATION_LOCAL_RESOLVE_PATH,
            post(resolve_federation_local_only),
        )
        .route(
            &format!("{FEDERATION_LOCAL_OBJECT_PATH_PREFIX}/{{sha256}}"),
            get(federation_local_object),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_federation_origin_authentication,
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
        .merge(crate::operations::router(state.clone()))
        .merge(federation_origin)
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
                let uri = origin
                    .parse::<http::Uri>()
                    .map_err(|_| ConfigError::Invalid(format!("invalid CORS origin '{origin}'")))?;
                if origin == "*"
                    || !matches!(uri.scheme_str(), Some("http" | "https"))
                    || uri.authority().is_none()
                    || uri.path() != "/"
                    || origin.ends_with('/')
                    || uri.query().is_some()
                    || uri
                        .authority()
                        .is_some_and(|authority| authority.as_str().contains('@'))
                {
                    return Err(ConfigError::Invalid(format!(
                        "CORS origin must be an exact HTTP(S) origin: '{origin}'"
                    )));
                }
                HeaderValue::from_str(origin)
                    .map_err(|_| ConfigError::Invalid(format!("invalid CORS origin '{origin}'")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        router = router.layer(browser_cors_layer(origins));
    }
    Ok(router)
}

fn browser_cors_layer(origins: Vec<HeaderValue>) -> CorsLayer {
    // Bearer authorization is explicitly allowed, but cookie credentials stay
    // disabled. In particular, this must never become wildcard + credentials.
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::DELETE,
        ])
        .allow_headers([
            header::ACCEPT,
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_NONE_MATCH,
        ])
        .expose_headers([
            header::CACHE_CONTROL,
            header::ETAG,
            header::HeaderName::from_static("x-request-id"),
            header::HeaderName::from_static("x-rw-satellite-frame"),
            header::HeaderName::from_static("x-rw-satellite-recipe"),
            header::HeaderName::from_static("x-rw-satellite-source-revision"),
            header::HeaderName::from_static("x-rw-valid-unix"),
        ])
        .max_age(Duration::from_secs(10 * 60))
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
    // Route-local privacy middleware runs before this final framework-error
    // normalization. Preserve its cache policy when replacing an extractor or
    // body-limit response with RFC 9457 JSON; otherwise a private route's 4xx
    // response can accidentally lose `no-store` at the outermost boundary.
    let cache_control = response.headers().get(header::CACHE_CONTROL).cloned();
    let pragma = response.headers().get(header::PRAGMA).cloned();
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
    let mut normalized = problem.into_response();
    if let Some(value) = cache_control {
        normalized
            .headers_mut()
            .insert(header::CACHE_CONTROL, value);
    }
    if let Some(value) = pragma {
        normalized.headers_mut().insert(header::PRAGMA, value);
    }
    normalized
}

async fn require_authentication(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    if state.tokens.is_empty() {
        request
            .extensions_mut()
            .insert(AuthPrincipal("local-unauthenticated".into()));
        return next.run(request).await;
    }
    if let Some(principal) = state
        .tokens
        .authorization_principal(request.headers().get(header::AUTHORIZATION))
    {
        request.extensions_mut().insert(AuthPrincipal(principal));
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

async fn require_federation_origin_authentication(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let enabled = state.config.federation.proxy.accept_local_resolve;
    let principal = enabled.then(|| {
        state
            .federation_origin_tokens
            .authorization_principal(request.headers().get(header::AUTHORIZATION))
    });
    if let Some(Some(principal)) = principal {
        request.extensions_mut().insert(AuthPrincipal(principal));
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
        HeaderValue::from_static("Bearer realm=\"rusty-weather-federation-origin\""),
    );
    response
}

async fn require_origin_catalog_ready(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    if !state.catalog.publication_gate_enabled() {
        return next.run(request).await;
    }
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or(RequestId(Uuid::nil()));
    let catalog = state.catalog.clone();
    match state.run_light(move || catalog.publication_ready()).await {
        Ok(true) => next.run(request).await,
        Ok(false) => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ORIGIN_CATALOG_UNAVAILABLE",
            "Origin publication catalog is unavailable",
            "Retry against a healthy authoritative origin.",
            request_id.0,
        )
        .into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn health_live(State(state): State<AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "live",
        uptime_seconds: state.uptime().as_secs(),
        degraded_subsystems: Vec::new(),
    })
}

async fn health_ready(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    // Prove the real query/catalog path first. Optional upstream state must not
    // hide a failed durable store, an unavailable publication catalog, or a
    // saturated/shutting-down query executor.
    let catalog = state.catalog.clone();
    match state.run_light(move || catalog.probe_readable()).await {
        Ok(Ok(())) => {}
        Ok(Err(_)) => {
            return ProblemDetails::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "NOT_READY",
                "Service is not ready",
                "The configured store or origin publication catalog is not currently ready.",
                request_id.0,
            )
            .into_response();
        }
        Err(error) => return execution_problem(error, request_id.0).into_response(),
    }

    if !state.mrms_ingest.server_readiness_ok() {
        return ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "MRMS_INGEST_NOT_READY",
            "Service is not ready",
            "The enabled MRMS follower has not produced a fresh frame for every configured product.",
            request_id.0,
        )
        .into_response();
    }
    if !state.nexrad_level2_ingest.server_readiness_ok() {
        return ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "NEXRAD_LEVEL2_INGEST_NOT_READY",
            "Service is not ready",
            "The enabled NEXRAD Level II follower has not stored a fresh exact volume for every configured site.",
            request_id.0,
        )
        .into_response();
    }
    let mut degraded_subsystems = Vec::with_capacity(2);
    if state.mrms_ingest.is_degraded() {
        degraded_subsystems.push("mrms_ingest");
    }
    if state.nexrad_level2_ingest.is_degraded() {
        degraded_subsystems.push("nexrad_level2_ingest");
    }
    let status = if degraded_subsystems.is_empty() {
        "ready"
    } else {
        "degraded"
    };
    Json(HealthResponse {
        status,
        uptime_seconds: state.uptime().as_secs(),
        degraded_subsystems,
    })
    .into_response()
}

async fn origin_catalog_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let catalog = state.catalog.clone();
    match state.run_light(move || catalog.health_status()).await {
        Ok(status) => json_no_store(StatusCode::OK, &status, request_id.0),
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
    let restrict_to_published = catalog.publication_gate_enabled();
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
                .filter(|summary| {
                    !restrict_to_published || stored.contains_key(&summary.id.to_string())
                })
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
                        indexed_subset: indexed_subset_available(summary.id, &product),
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

async fn latest_run(
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
        return private_no_store(response);
    }
    let catalog = state.catalog.clone();
    let response = match state
        .run_light(move || catalog.latest_run(&path.model))
        .await
    {
        Ok(Ok(snapshot)) => json_no_store(StatusCode::OK, snapshot.descriptor(), request_id.0),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    };
    private_no_store(response)
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

/// One decoded model plane plus the stored facts the response advertises.
struct ModelPlane {
    bytes: Vec<u8>,
    etag: String,
    variable: String,
    units: String,
    codec: String,
    valid_unix: i64,
    level_hpa: Option<u16>,
}

/// Serve one complete forecast plane as binary f32.
///
/// This is the model sibling of the observation plane route: same `RWOBF32`
/// container, same immutable caching contract, same authentication scope. It
/// differs in two deliberate ways.
///
/// The URL carries `expected_snapshot_id`/`expected_grid_hash` the way
/// `/v1/geographic-window` does. Run names are reused across atomic
/// republication, so without the guard the URL would not identify one
/// immutable body and `Cache-Control: immutable` would be false.
///
/// It publishes no palette or interpolation hints. Model variables carry no
/// stored display metadata, and inventing observation-shaped semantics for
/// them would be a fabricated claim. Styling inputs — selector, kind,
/// `levels_hpa`, `available_slots` — stay on
/// `/v1/models/{model}/runs/{run}/variables`, which is authoritative.
async fn model_plane_binary(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ModelPlanePath>,
    Query(query): Query<ModelPlaneQuery>,
) -> Response {
    let Some(variable_name) = path
        .variable
        .strip_suffix(".bin")
        .filter(|variable| !variable.is_empty())
        .map(str::to_owned)
    else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &path.model,
        std::iter::once(variable_name.as_str()),
    ) {
        return response;
    }
    let (Some(expected_snapshot_id), Some(expected_grid_hash)) =
        (query.expected_snapshot_id, query.expected_grid_hash)
    else {
        return ProblemDetails::new(
            StatusCode::BAD_REQUEST,
            "INVALID_QUERY",
            "Invalid query",
            "expected_snapshot_id and expected_grid_hash are required so the plane URL names one immutable run generation.",
            request_id.0,
        )
        .into_response();
    };
    let level_hpa = query.level_hpa;
    let catalog = state.catalog.clone();
    match state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&path.model, &path.run)?;
            let descriptor = snapshot.descriptor();
            if expected_snapshot_id != descriptor.snapshot_id
                || expected_grid_hash != descriptor.grid_hash
            {
                return Err(QueryError::InvalidRequest(
                    "model plane snapshot_id/grid_hash does not match the resolved immutable run"
                        .into(),
                ));
            }
            let (time, metadata, values) = match level_hpa {
                None => {
                    let field = snapshot.read_surface_2d(path.storage_slot, &variable_name)?;
                    (field.time, field.metadata, field.values)
                }
                Some(level_hpa) => {
                    let field = snapshot.read_pressure_level_2d(
                        path.storage_slot,
                        &variable_name,
                        level_hpa,
                    )?;
                    (field.time, field.metadata, field.values)
                }
            };
            let grid = snapshot.grid();
            let bytes = encode_plane_blob(
                &metadata.name,
                &metadata.units,
                time.valid_unix,
                grid.nx,
                grid.ny,
                &values,
            )
            .map_err(|error| QueryError::InvalidRequest(error.to_string()))?;
            let etag = format!(
                "\"{}-{}-{}-{}\"",
                descriptor.snapshot_id,
                path.storage_slot,
                metadata.id,
                level_hpa.map_or_else(|| "surface".to_owned(), |level| format!("{level}hpa"))
            );
            Ok::<_, QueryError>(ModelPlane {
                bytes,
                etag,
                variable: metadata.name,
                units: metadata.units,
                codec: metadata.codec,
                valid_unix: time.valid_unix,
                level_hpa,
            })
        })
        .await
    {
        Ok(Ok(plane)) => model_plane_response(plane),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

fn model_plane_response(plane: ModelPlane) -> Response {
    let mut response = Response::new(Body::from(plane.bytes));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(MODEL_PLANE_CONTENT_TYPE),
    );
    // The identity guards in the URL pin one immutable run generation, so the
    // body at this URL can never change.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    // Non-finite cells are absent values, not zero. This is a value-domain
    // contract the stored f32 payload genuinely carries, unlike palette or
    // interpolation hints, which models do not store.
    headers.insert(
        "x-rw-nodata",
        HeaderValue::from_static("non-finite-transparent"),
    );
    if let Ok(value) = HeaderValue::from_str(&plane.etag) {
        headers.insert(header::ETAG, value);
    }
    if let Ok(value) = HeaderValue::from_str(&plane.variable) {
        headers.insert("x-rw-model-variable", value);
    }
    if let Ok(value) = HeaderValue::from_str(&plane.units) {
        headers.insert("x-rw-model-units", value);
    }
    // `zstd1_affine_i16` pressure planes are dequantized approximations of the
    // ingested field; `zstd1_f32` surface planes are exact. Publishing the
    // stored codec keeps that difference visible instead of implying that
    // every f32 payload is lossless.
    if let Ok(value) = HeaderValue::from_str(&plane.codec) {
        headers.insert("x-rw-model-codec", value);
    }
    if let Ok(value) = HeaderValue::from_str(&plane.valid_unix.to_string()) {
        headers.insert("x-rw-valid-unix", value);
    }
    if let Some(level_hpa) = plane.level_hpa
        && let Ok(value) = HeaderValue::from_str(&level_hpa.to_string())
    {
        headers.insert("x-rw-model-level-hpa", value);
    }
    response
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

async fn profile_cycle(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<ProfileCycleApiRequest>,
) -> Response {
    if let Some(response) = reject_retired_selection(
        &state,
        request_id.0,
        &request.model,
        request
            .variables
            .iter()
            .chain(request.surface_variables.iter())
            .map(String::as_str),
    ) {
        return response;
    }
    let catalog = state.catalog.clone();
    let cache = state.response_cache.clone();
    let metrics = state.metrics.clone();
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancellation.clone();
    let result = state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let query = ProfileCycleRequest {
                latitude: request.latitude,
                longitude: request.longitude,
                variables: request.variables.clone(),
                surface_variables: request.surface_variables.clone(),
                time: TimeRange {
                    start_unix: request.start_unix,
                    end_unix: request.end_unix,
                },
                missing_policy: request.missing_policy.into(),
            };
            cache_or_compute(
                &cache,
                &metrics,
                "profile_cycle_v1",
                &snapshot.descriptor().snapshot_id,
                &request,
                || {
                    query_profile_cycle_with_cancel(&snapshot, &query, || {
                        worker_cancellation.load(Ordering::Acquire)
                    })
                },
            )
        })
        .await;
    if matches!(result, Err(ExecutionError::ExecutionTimeout)) {
        // Dropping a spawn_blocking handle does not preempt it. Profile-cycle
        // queries check this flag at every time/variable/decode boundary.
        cancellation.store(true, Ordering::Release);
    }
    match result {
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

async fn geographic_window(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<GeographicWindowApiRequest>,
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
    let limits = GeographicWindowLimits {
        max_native_cells: state.config.limits.geographic_window_cells,
        max_output_values: state.config.limits.geographic_window_output_values,
    };
    let cancellation = Arc::new(AtomicBool::new(false));
    let worker_cancellation = cancellation.clone();
    let result = state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&request.model, &request.run)?;
            let query = GeographicWindowRequest {
                expected_snapshot_id: request.expected_snapshot_id.clone(),
                expected_grid_hash: request.expected_grid_hash.clone(),
                storage_slot: request.storage_slot,
                variables: request.variables.clone(),
                bbox: GeographicBoundingBox {
                    west_longitude: request.west_longitude,
                    south_latitude: request.south_latitude,
                    east_longitude: request.east_longitude,
                    north_latitude: request.north_latitude,
                },
                vertical: (&request.vertical).into(),
            };
            cache_or_compute(
                &cache,
                &metrics,
                "geographic_window_v1",
                &snapshot.descriptor().snapshot_id,
                &request,
                || {
                    query_geographic_window_with_cancel(&snapshot, &query, limits, || {
                        worker_cancellation.load(Ordering::Acquire)
                    })
                },
            )
        })
        .await;
    if matches!(result, Err(ExecutionError::ExecutionTimeout)) {
        cancellation.store(true, Ordering::Release);
    }
    match result {
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

async fn resolve_community_object(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<ResolveObjectRequest>,
) -> Response {
    let is_heavy = matches!(
        &request.request.query,
        rw_community_protocol::ShareQuery::TemporalGrid { .. }
    );
    let community = state.community.clone();
    let catalog = state.catalog.clone();
    let result = if is_heavy {
        state
            .run_heavy_sync(move || community.resolve(&principal.0, &request, &catalog))
            .await
    } else {
        state
            .run_light(move || community.resolve(&principal.0, &request, &catalog))
            .await
    };
    match result {
        Ok(Ok((resolved, _object))) => json_with_etag(StatusCode::OK, &resolved, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn community_object(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<CommunityObjectPath>,
) -> Response {
    let community = state.community.clone();
    let hash = path.sha256.clone();
    match state
        .run_light(move || community.object(&principal.0, &hash))
        .await
    {
        Ok(Ok(bytes)) => immutable_object_response(bytes, &path.sha256),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn publish_community_case(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(manifest): Json<CaseRoomManifest>,
) -> Response {
    let community = state.community.clone();
    match state
        .run_light(move || community.publish_case(&principal.0, manifest))
        .await
    {
        Ok(Ok(signed)) => json_with_etag(StatusCode::CREATED, &signed, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn publish_community_case_artifact(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(publication): Json<PublishCaseArtifactRequest>,
) -> Response {
    let community = state.community.clone();
    match state
        .run_light(move || community.publish_case_artifact(&principal.0, publication))
        .await
    {
        Ok(Ok(signed)) => json_with_etag(StatusCode::CREATED, &signed, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn revoke_community_case_artifact(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<CommunityObjectPath>,
    Json(request): Json<RevokePublicationRequest>,
) -> Response {
    let community = state.community.clone();
    match state
        .run_light(move || community.revoke_case_artifact(&principal.0, &path.sha256, request))
        .await
    {
        Ok(Ok(tombstone)) => json_with_etag(StatusCode::OK, &tombstone, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn revoke_community_case(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<CommunityCasePath>,
    Json(request): Json<RevokePublicationRequest>,
) -> Response {
    let community = state.community.clone();
    match state
        .run_light(move || community.revoke_case(&principal.0, &path.case_id, request))
        .await
    {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn community_case(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<CommunityCasePath>,
) -> Response {
    let community = state.community.clone();
    match state
        .run_light(move || community.case(&principal.0, &path.case_id))
        .await
    {
        Ok(Ok(signed)) => json_with_etag(StatusCode::OK, &signed, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn list_community_cases(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Query(query): Query<CommunityCaseDirectoryQuery>,
) -> Response {
    if !query.validate() {
        return ProblemDetails::new(
            StatusCode::BAD_REQUEST,
            "INVALID_CASE_CURSOR",
            "Case directory cursor or limit is invalid",
            "Use an opaque case id cursor and a limit from 1 through 100.",
            request_id.0,
        )
        .into_response();
    }
    let community = state.community.clone();
    match state
        .run_light(move || community.list_cases(&principal.0, query.after.as_deref(), query.limit))
        .await
    {
        Ok(Ok(page)) => json_no_store(StatusCode::OK, &page, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn advertise_community_relay_object(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelayAdvertiseRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.advertise(&principal.0, request))
        .await
    {
        Ok(Ok(receipt)) => json_no_store(StatusCode::CREATED, &receipt, request_id.0),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn lookup_community_relay_historical(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<HistoricalRelayLookupRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.historical_lookup_json(&principal.0, request))
        .await
    {
        Ok(Ok((bytes, issued))) => {
            state.metrics.record_relay_lookup(issued);
            secret_json_response(StatusCode::OK, bytes)
        }
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn next_community_relay_grant(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelayGrantPollRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.next_grant_json(&principal.0, request))
        .await
    {
        Ok(Ok(bytes)) => secret_json_response(StatusCode::OK, bytes),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn community_relay_session_grant(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<RelaySessionGrantPath>,
) -> Response {
    let role = match path.role.as_str() {
        "uploader" => rw_community_relay::RelayRole::Uploader,
        "downloader" => rw_community_relay::RelayRole::Downloader,
        _ => return ProblemDetails::not_found(request_id.0).into_response(),
    };
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.grant_for_session_json(&principal.0, &path.session_id, role))
        .await
    {
        Ok(Ok(bytes)) => secret_json_response(StatusCode::OK, bytes),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn register_community_relay_route(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelayRouteRegistrationRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.register_route(&principal.0, request))
        .await
    {
        Ok(Ok(receipt)) => json_no_store(StatusCode::OK, &receipt, request_id.0),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn community_relay_transport_grant(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelayTransportGrantRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.transport_grant_json(&principal.0, request))
        .await
    {
        Ok(Ok(bytes)) => secret_json_response(StatusCode::OK, bytes),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn complete_community_relay_session(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelaySessionCompletionRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.complete(&principal.0, request))
        .await
    {
        Ok(Ok((terminal, promotion))) => {
            state.metrics.record_relay_completion(promotion.is_some());
            json_no_store(StatusCode::OK, &terminal, request_id.0)
        }
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn fail_community_relay_session(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelaySessionFailureRequest>,
) -> Response {
    terminal_community_relay_session(state, request_id, principal, request, false).await
}

async fn revoke_community_relay_session(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelaySessionFailureRequest>,
) -> Response {
    terminal_community_relay_session(state, request_id, principal, request, true).await
}

async fn terminal_community_relay_session(
    state: AppState,
    request_id: RequestId,
    principal: AuthPrincipal,
    request: RelaySessionFailureRequest,
    revoke: bool,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.fail_or_revoke(&principal.0, request, revoke))
        .await
    {
        Ok(Ok(terminal)) => {
            state.metrics.record_relay_failure();
            json_no_store(StatusCode::OK, &terminal, request_id.0)
        }
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn set_community_relay_kill_switch(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<RelayKillSwitchRequest>,
) -> Response {
    let relay = state.community_relay.clone();
    match state
        .run_light(move || relay.set_kill_switch(&principal.0, request))
        .await
    {
        Ok(Ok(status)) => {
            state.metrics.set_relay_kill_switch(status.kill_switch);
            json_no_store(StatusCode::OK, &status, request_id.0)
        }
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn community_relay_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let relay = state.community_relay.clone();
    match state.run_light(move || relay.status(&principal.0)).await {
        Ok(Ok(status)) => json_no_store(StatusCode::OK, &status, request_id.0),
        Ok(Err(error)) => community_relay_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn begin_generation_replication(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<BeginRunGenerationRequest>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.begin(&principal.0, request))
        .await
    {
        Ok(Ok(status)) => {
            state.metrics.record_replication_begin();
            json_no_store(StatusCode::CREATED, &status, request_id.0)
        }
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_owner(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.owner_identity(&principal.0))
        .await
    {
        Ok(Ok(owner)) => json_no_store(StatusCode::OK, &owner, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_capabilities(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.owner_capabilities(&principal.0))
        .await
    {
        Ok(Ok(capabilities)) => json_no_store(StatusCode::OK, &capabilities, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn list_generation_replication_records(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Query(query): Query<ReplicationOwnerListQuery>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || {
            replication.owner_records(&principal.0, query.after.as_deref(), query.limit)
        })
        .await
    {
        Ok(Ok(page)) => json_no_store(StatusCode::OK, &page, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.upload_status(&principal.0, &path.generation_id))
        .await
    {
        Ok(Ok(status)) => json_no_store(StatusCode::OK, &status, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn cancel_generation_replication(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.cancel_upload(&principal.0, &path.generation_id))
        .await
    {
        Ok(Ok(cancelled)) => json_no_store(StatusCode::OK, &cancelled, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_publication(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.owner_record(&principal.0, &path.generation_id))
        .await
    {
        Ok(Ok(record)) => json_no_store(StatusCode::OK, &record, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_missing(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
    Query(query): Query<ReplicationMissingQuery>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || {
            replication.missing_chunks(
                &principal.0,
                &path.generation_id,
                query.after.as_deref(),
                query.limit,
            )
        })
        .await
    {
        Ok(Ok(page)) => json_no_store(StatusCode::OK, &page, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn upload_generation_replication_chunk(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationChunkPath>,
    request: Request,
) -> Response {
    let content_type_ok = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("application/octet-stream"));
    if !content_type_ok {
        return ProblemDetails::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "GENERATION_CHUNK_MEDIA_TYPE",
            "Generation chunk media type is unsupported",
            "Send the exact declared chunk as application/octet-stream.",
            request_id.0,
        )
        .into_response();
    }
    let body = match axum::body::to_bytes(
        request.into_body(),
        usize::try_from(
            state
                .config
                .generation_replication
                .limits
                .maximum_chunk_bytes,
        )
        .unwrap_or(usize::MAX),
    )
    .await
    {
        Ok(body) => body,
        Err(_) => {
            return ProblemDetails::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "REPLICATION_CHUNK_TOO_LARGE",
                "Generation chunk is too large",
                "Upload exactly the bounded content-addressed chunk declared by the generation manifest.",
                request_id.0,
            )
            .into_response();
        }
    };
    if body.is_empty() {
        return ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "GENERATION_CHUNK_EMPTY",
            "Generation chunk is empty",
            "Upload the non-empty content-addressed chunk declared by the generation manifest.",
            request_id.0,
        )
        .into_response();
    }
    if body.len() as u64
        > state
            .config
            .generation_replication
            .limits
            .maximum_chunk_bytes
    {
        return ProblemDetails::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "REPLICATION_CHUNK_TOO_LARGE",
            "Generation chunk is too large",
            "Upload exactly the bounded content-addressed chunk declared by the generation manifest.",
            request_id.0,
        )
        .into_response();
    }
    let byte_count = body.len() as u64;
    let replication = state.generation_replication.clone();
    match state
        .run_heavy_sync(move || {
            replication.upload_chunk(&principal.0, &path.generation_id, &path.sha256, &body)
        })
        .await
    {
        Ok(Ok(())) => {
            state.metrics.record_replication_upload(byte_count);
            let mut response = StatusCode::NO_CONTENT.into_response();
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static("no-store, private"),
            );
            response
                .headers_mut()
                .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
            response
        }
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn finalize_generation_replication(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
    Json(request): Json<FinalizeRunGenerationRequest>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_heavy_job(move || replication.finalize(&principal.0, &path.generation_id, request))
        .await
    {
        Ok(Ok(outcome)) => {
            state.metrics.record_replication_finalize();
            let status = if outcome.was_already_published {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            json_no_store(status, &outcome.published, request_id.0)
        }
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn revoke_generation_replication(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<ReplicationGenerationPath>,
    Json(request): Json<RevokeRunGenerationRequest>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.revoke(&principal.0, &path.generation_id, request))
        .await
    {
        Ok(Ok(tombstone)) => {
            state.metrics.record_replication_revoke();
            json_no_store(StatusCode::OK, &tombstone, request_id.0)
        }
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn generation_replication_operator_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.operator_status(&principal.0))
        .await
    {
        Ok(Ok(status)) => json_no_store(StatusCode::OK, &status, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn set_generation_replication_kill_switch(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<ReplicationKillSwitchRequest>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_light(move || replication.set_kill_switch(&principal.0, request))
        .await
    {
        Ok(Ok(status)) => {
            state
                .metrics
                .set_replication_kill_switch(status.kill_switch);
            json_no_store(StatusCode::OK, &status, request_id.0)
        }
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn run_generation_replication_gc(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let replication = state.generation_replication.clone();
    match state
        .run_heavy_sync(move || replication.garbage_collect(&principal.0))
        .await
    {
        Ok(Ok(report)) => json_no_store(StatusCode::OK, &report, request_id.0),
        Ok(Err(error)) => generation_replication_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn federation_catalog(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(_principal): Extension<AuthPrincipal>,
) -> Response {
    let federation = state.federation.clone();
    match state.run_light(move || federation.catalog()).await {
        Ok(Ok(catalog)) => json_with_etag(StatusCode::OK, &catalog, request_id.0),
        Ok(Err(error)) => federation_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn federation_origin(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(_principal): Extension<AuthPrincipal>,
    Path(path): Path<FederationOriginPath>,
) -> Response {
    let federation = state.federation.clone();
    match state
        .run_light(move || federation.descriptor(&path.origin_id))
        .await
    {
        Ok(Ok(descriptor)) => json_with_etag(StatusCode::OK, &descriptor, request_id.0),
        Ok(Err(error)) => federation_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn federation_health(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(_principal): Extension<AuthPrincipal>,
) -> Response {
    let federation = state.federation.clone();
    match state.run_light(move || federation.health_status()).await {
        Ok(Ok(status)) => json_with_etag(StatusCode::OK, &status, request_id.0),
        Ok(Err(error)) => federation_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn resolve_federation_proxy(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
    Json(request): Json<FederationProxyRequest>,
) -> Response {
    // A one-hop request may never re-enter the authority proxy.
    if headers.contains_key(FEDERATION_HOP_HEADER) {
        return federation_proxy_problem(FederationProxyError::InvalidRequest, request_id.0)
            .into_response();
    }
    let Some(proxy) = state.federation_proxy.clone() else {
        return federation_proxy_problem(FederationProxyError::Disabled, request_id.0)
            .into_response();
    };
    match state
        .run_light(move || proxy.resolve(&principal.0, &request))
        .await
    {
        Ok(Ok(result)) => json_no_store(StatusCode::OK, &result.response, request_id.0),
        Ok(Err(error)) => federation_proxy_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn federation_proxy_operator_status(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
) -> Response {
    let Some(proxy) = state.federation_proxy.clone() else {
        return federation_proxy_control_problem(
            FederationProxyControlError::Disabled,
            request_id.0,
        )
        .into_response();
    };
    match state
        .run_light(move || proxy.operator_status(&principal.0))
        .await
    {
        Ok(Ok(status)) => json_no_store(StatusCode::OK, &status, request_id.0),
        Ok(Err(error)) => federation_proxy_control_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn set_federation_proxy_kill_switch(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Json(request): Json<FederationProxyKillSwitchRequest>,
) -> Response {
    let Some(proxy) = state.federation_proxy.clone() else {
        return federation_proxy_control_problem(
            FederationProxyControlError::Disabled,
            request_id.0,
        )
        .into_response();
    };
    match state
        .run_light(move || proxy.set_kill_switch(&principal.0, request))
        .await
    {
        Ok(Ok(status)) => {
            state
                .metrics
                .set_federation_proxy_kill_switch(status.kill_switch);
            json_no_store(StatusCode::OK, &status, request_id.0)
        }
        Ok(Err(error)) => {
            if matches!(error, FederationProxyControlError::Persistence) {
                state.metrics.set_federation_proxy_kill_switch(true);
            }
            federation_proxy_control_problem(error, request_id.0).into_response()
        }
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn resolve_federation_local_only(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    headers: HeaderMap,
    Json(request): Json<ResolveObjectRequest>,
) -> Response {
    if !has_exact_federation_hop(&headers) {
        return federation_proxy_problem(FederationProxyError::InvalidRequest, request_id.0)
            .into_response();
    }
    let community = state.community.clone();
    let catalog = state.catalog.clone();
    match state
        .run_light(move || community.resolve_local_only(&principal.0, &request, &catalog))
        .await
    {
        Ok(Ok((resolved, _object))) => json_no_store(StatusCode::OK, &resolved, request_id.0),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn federation_local_object(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Extension(principal): Extension<AuthPrincipal>,
    Path(path): Path<CommunityObjectPath>,
) -> Response {
    let community = state.community.clone();
    let sha256 = path.sha256.clone();
    match state
        .run_light(move || community.federation_object_local_only(&principal.0, &sha256))
        .await
    {
        Ok(Ok(bytes)) => immutable_object_response(bytes, &path.sha256),
        Ok(Err(error)) => community_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

fn has_exact_federation_hop(headers: &HeaderMap) -> bool {
    let mut values = headers.get_all(FEDERATION_HOP_HEADER).iter();
    values.next().is_some_and(|value| value.as_bytes() == b"1") && values.next().is_none()
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

fn immutable_object_response(bytes: Bytes, sha256: &str) -> Response {
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    if let Ok(value) = HeaderValue::from_str(&format!("\"{sha256}\"")) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response
}

fn json_no_store<T>(status: StatusCode, value: &T, request_id: Uuid) -> Response
where
    T: Serialize + ?Sized,
{
    match serde_json::to_vec(value) {
        Ok(body) => secret_json_response(status, body),
        Err(error) => {
            error!(%request_id, %error, "private response serialization failed");
            ProblemDetails::internal(request_id).into_response()
        }
    }
}

fn secret_json_response(status: StatusCode, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

fn private_no_store(mut response: Response) -> Response {
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response.headers_mut().remove(header::ETAG);
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
        | QueryError::UnknownVariable(_)
        | QueryError::UnknownPressureLevel { .. } => ProblemDetails::new(
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

fn community_problem(error: CommunityError, request_id: Uuid) -> ProblemDetails {
    match error {
        CommunityError::Disabled | CommunityError::Killed => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "COMMUNITY_CACHE_DISABLED",
            "Community Cache is unavailable",
            "Use the normal Rusty Weather origin API or retry after the feature is enabled.",
            request_id,
        ),
        CommunityError::NotFound => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "COMMUNITY_OBJECT_NOT_FOUND",
            "Community object was not found",
            "Resolve the immutable request again; the origin remains the fallback.",
            request_id,
        ),
        CommunityError::Unsupported => ProblemDetails::new(
            StatusCode::NOT_IMPLEMENTED,
            "COMMUNITY_QUERY_UNSUPPORTED",
            "Community object type is not available",
            "Use the normal Rusty Weather query endpoint for this object type.",
            request_id,
        ),
        CommunityError::OriginCatalogUnavailable => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "ORIGIN_CATALOG_UNAVAILABLE",
            "Origin publication catalog is unavailable",
            "Retry against a healthy authoritative origin.",
            request_id,
        ),
        CommunityError::Cas(crate::community_store::CommunityStoreError::Quota) => {
            ProblemDetails::new(
                StatusCode::TOO_MANY_REQUESTS,
                "COMMUNITY_QUOTA",
                "Community Cache quota reached",
                "Retry after the quota window resets or use the normal origin API.",
                request_id,
            )
        }
        CommunityError::Cas(crate::community_store::CommunityStoreError::TooLarge)
        | CommunityError::Cas(crate::community_store::CommunityStoreError::HashMismatch)
        | CommunityError::Cas(crate::community_store::CommunityStoreError::Invalid(_))
        | CommunityError::Protocol(_) => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "COMMUNITY_OBJECT_REJECTED",
            "Community object failed validation",
            "The request, signature, hash, schema, attribution, or size contract is invalid.",
            request_id,
        ),
        CommunityError::Query(error) => query_problem(error, request_id),
        CommunityError::Upstream(detail) => {
            error!(%request_id, %detail, "Community Cache upstream failed");
            ProblemDetails::new(
                StatusCode::BAD_GATEWAY,
                "COMMUNITY_UPSTREAM_FAILED",
                "Community origin is unavailable",
                "Retry the normal Rusty Weather origin request.",
                request_id,
            )
        }
        CommunityError::Invalid(detail) => {
            error!(%request_id, %detail, "Community Cache state is invalid");
            ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "COMMUNITY_REQUEST_INVALID",
                "Community request is invalid",
                "Correct the request identity and try again.",
                request_id,
            )
        }
        CommunityError::Io(error) => {
            error!(%request_id, %error, "Community Cache I/O failed");
            ProblemDetails::internal(request_id)
        }
        CommunityError::Json(error) => {
            error!(%request_id, %error, "Community Cache JSON failed");
            ProblemDetails::internal(request_id)
        }
        CommunityError::Store(error)
        | CommunityError::Cas(crate::community_store::CommunityStoreError::Store(error)) => {
            error!(%request_id, %error, "Community Cache publication failed");
            ProblemDetails::internal(request_id)
        }
        CommunityError::Cas(crate::community_store::CommunityStoreError::Killed) => {
            ProblemDetails::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "COMMUNITY_CACHE_DISABLED",
                "Community Cache is unavailable",
                "Use the normal Rusty Weather origin API.",
                request_id,
            )
        }
        CommunityError::Cas(crate::community_store::CommunityStoreError::Io(error)) => {
            error!(%request_id, %error, "Community CAS I/O failed");
            ProblemDetails::internal(request_id)
        }
        CommunityError::Cas(crate::community_store::CommunityStoreError::Json(error)) => {
            error!(%request_id, %error, "Community CAS metadata failed");
            ProblemDetails::internal(request_id)
        }
    }
}

fn community_relay_problem(error: CommunityRelayError, request_id: Uuid) -> ProblemDetails {
    match error {
        CommunityRelayError::Disabled
        | CommunityRelayError::Relay(
            rw_community_relay::RelayError::Disabled | rw_community_relay::RelayError::SecurityGate,
        ) => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "COMMUNITY_RELAY_DISABLED",
            "Private Community Sharing is unavailable",
            "Continue with the archival HTTPS origin or treat the historical object as unavailable.",
            request_id,
        ),
        CommunityRelayError::NotFound
        | CommunityRelayError::Relay(rw_community_relay::RelayError::NotAvailable) => {
            ProblemDetails::new(
                StatusCode::NOT_FOUND,
                "COMMUNITY_RELAY_NOT_FOUND",
                "No private community copy is available",
                "Continue immediately to the archival HTTPS origin or report the object unavailable.",
                request_id,
            )
        }
        CommunityRelayError::Forbidden
        | CommunityRelayError::Relay(
            rw_community_relay::RelayError::CredentialInvalid
            | rw_community_relay::RelayError::CredentialExpired
            | rw_community_relay::RelayError::CredentialRevoked,
        ) => ProblemDetails::new(
            StatusCode::FORBIDDEN,
            "COMMUNITY_RELAY_FORBIDDEN",
            "Community relay authorization was rejected",
            "Use only the participant grant issued to this authenticated account.",
            request_id,
        ),
        CommunityRelayError::Relay(
            rw_community_relay::RelayError::QuotaReached
            | rw_community_relay::RelayError::CostThresholdReached
            | rw_community_relay::RelayError::MeteredNetworkPaused
            | rw_community_relay::RelayError::PolicyDenied,
        ) => ProblemDetails::new(
            StatusCode::TOO_MANY_REQUESTS,
            "COMMUNITY_RELAY_POLICY",
            "Community relay policy denied this transfer",
            "Continue immediately to the archival HTTPS origin or unavailable result.",
            request_id,
        ),
        CommunityRelayError::Invalid
        | CommunityRelayError::Relay(
            rw_community_relay::RelayError::UntrustedObject
            | rw_community_relay::RelayError::UnsafeIdentifier
            | rw_community_relay::RelayError::ProviderRejected
            | rw_community_relay::RelayError::EnvelopeRejected
            | rw_community_relay::RelayError::AuthenticationFailed
            | rw_community_relay::RelayError::Replay
            | rw_community_relay::RelayError::OutOfOrder
            | rw_community_relay::RelayError::ObjectMismatch
            | rw_community_relay::RelayError::KeyAgreementRejected
            | rw_community_relay::RelayError::DnsRejected,
        ) => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "COMMUNITY_RELAY_REJECTED",
            "Community relay request failed validation",
            "Discard this relay attempt and continue to the archival HTTPS origin.",
            request_id,
        ),
        CommunityRelayError::Persistence
        | CommunityRelayError::Io(_)
        | CommunityRelayError::Relay(
            rw_community_relay::RelayError::PersistenceRejected
            | rw_community_relay::RelayError::ProviderUnavailable
            | rw_community_relay::RelayError::TransportUnavailable,
        ) => {
            error!(%request_id, "Community relay control plane unavailable");
            ProblemDetails::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "COMMUNITY_RELAY_UNAVAILABLE",
                "Private Community Sharing is unavailable",
                "Continue immediately to the archival HTTPS origin or unavailable result.",
                request_id,
            )
        }
    }
}

fn federation_problem(error: FederationError, request_id: Uuid) -> ProblemDetails {
    match error {
        FederationError::Disabled => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FEDERATION_DISABLED",
            "Public-origin federation is unavailable",
            "Use the authoritative Rusty Weather origin or retry after federation is enabled.",
            request_id,
        ),
        FederationError::NotFound => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "FEDERATED_ORIGIN_NOT_FOUND",
            "Federated origin was not found",
            "Refresh the signed federation catalog before selecting an archival origin.",
            request_id,
        ),
        FederationError::Persistence => {
            error!(%request_id, "federation health persistence failed");
            ProblemDetails::internal(request_id)
        }
        FederationError::Invalid(detail) => {
            error!(%request_id, %detail, "federation request is invalid");
            ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "FEDERATION_REQUEST_INVALID",
                "Federation request is invalid",
                "Use a canonical origin identifier from the signed catalog.",
                request_id,
            )
        }
        FederationError::Protocol(error) => {
            error!(%request_id, %error, "federation signature or policy rejected");
            ProblemDetails::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "FEDERATION_DESCRIPTOR_REJECTED",
                "Federation descriptor failed validation",
                "Refresh the catalog; untrusted, revoked, expired, malformed, or unsafe origins fail closed.",
                request_id,
            )
        }
        FederationError::Io(error) => {
            error!(%request_id, %error, "federation descriptor I/O failed");
            ProblemDetails::internal(request_id)
        }
        FederationError::Json(error) => {
            error!(%request_id, %error, "federation descriptor JSON failed");
            ProblemDetails::internal(request_id)
        }
    }
}

fn federation_proxy_problem(error: FederationProxyError, request_id: Uuid) -> ProblemDetails {
    match error {
        FederationProxyError::Disabled => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FEDERATION_PROXY_DISABLED",
            "Federated data failover is unavailable",
            "Continue with the authoritative local/R2/origin request path.",
            request_id,
        ),
        FederationProxyError::InvalidRequest
        | FederationProxyError::UnapprovedOriginHint
        | FederationProxyError::Protocol(_) => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FEDERATION_PROXY_REQUEST_REJECTED",
            "Federated data request failed validation",
            "Use an exact canonical request and an origin from the current signed catalog.",
            request_id,
        ),
        FederationProxyError::NoCandidate => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "FEDERATION_PROXY_OBJECT_NOT_FOUND",
            "No approved origin has the exact object",
            "Continue with the authoritative local compute path or report the object unavailable.",
            request_id,
        ),
        FederationProxyError::Quota => ProblemDetails::new(
            StatusCode::TOO_MANY_REQUESTS,
            "FEDERATION_PROXY_QUOTA_EXHAUSTED",
            "Federated data quota is exhausted",
            "Continue with the authoritative local/R2/origin request path.",
            request_id,
        ),
        FederationProxyError::Unavailable { .. } | FederationProxyError::Stage => {
            ProblemDetails::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "FEDERATION_PROXY_UNAVAILABLE",
                "Federated data failover is unavailable",
                "Continue immediately with the authoritative local/R2/origin request path.",
                request_id,
            )
        }
        FederationProxyError::InvalidConfiguration => {
            error!(%request_id, "federation proxy configuration invariant failed");
            ProblemDetails::internal(request_id)
        }
    }
}

fn federation_proxy_control_problem(
    error: FederationProxyControlError,
    request_id: Uuid,
) -> ProblemDetails {
    match error {
        FederationProxyControlError::Disabled => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "FEDERATION_PROXY_CONTROL_DISABLED",
            "Federation proxy control is unavailable",
            "Enable the federation proxy before using its runtime operator controls.",
            request_id,
        ),
        FederationProxyControlError::Forbidden => ProblemDetails::new(
            StatusCode::FORBIDDEN,
            "FEDERATION_PROXY_CONTROL_FORBIDDEN",
            "Federation proxy operator authorization was rejected",
            "Use a configured federation proxy operator account.",
            request_id,
        ),
        FederationProxyControlError::Invalid => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "FEDERATION_PROXY_CONTROL_REJECTED",
            "Federation proxy control request failed validation",
            "Use the current closed kill-switch schema.",
            request_id,
        ),
        FederationProxyControlError::Persistence => {
            error!(%request_id, "federation proxy control persistence failed closed");
            ProblemDetails::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "FEDERATION_PROXY_CONTROL_UNAVAILABLE",
                "Federation proxy control state is unavailable",
                "The proxy remains stopped; repair durable control storage before retrying.",
                request_id,
            )
        }
    }
}

fn generation_replication_problem(
    error: GenerationReplicationError,
    request_id: Uuid,
) -> ProblemDetails {
    use rw_generation_replication::ReplicationError;

    match error {
        GenerationReplicationError::Disabled
        | GenerationReplicationError::Engine(
            ReplicationError::Disabled | ReplicationError::KillSwitch,
        ) => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "GENERATION_REPLICATION_DISABLED",
            "Generation replication is unavailable",
            "Use the normal HTTPS query API or retry after the advanced feature is enabled.",
            request_id,
        ),
        GenerationReplicationError::Forbidden
        | GenerationReplicationError::Engine(ReplicationError::WrongOwner) => ProblemDetails::new(
            StatusCode::FORBIDDEN,
            "GENERATION_REPLICATION_FORBIDDEN",
            "Generation replication authorization was rejected",
            "Use only a generation owned by this authenticated account.",
            request_id,
        ),
        GenerationReplicationError::Engine(ReplicationError::NotFound) => ProblemDetails::new(
            StatusCode::NOT_FOUND,
            "GENERATION_REPLICATION_NOT_FOUND",
            "Generation upload was not found",
            "Begin the exact generation upload or refresh its status.",
            request_id,
        ),
        GenerationReplicationError::Engine(ReplicationError::Conflict) => ProblemDetails::new(
            StatusCode::CONFLICT,
            "GENERATION_REPLICATION_CONFLICT",
            "Generation publication conflicts with existing state",
            "Do not replace an existing different generation; use the exact signed identity.",
            request_id,
        ),
        GenerationReplicationError::Engine(ReplicationError::Expired) => ProblemDetails::new(
            StatusCode::GONE,
            "GENERATION_REPLICATION_EXPIRED",
            "Generation upload expired",
            "Create a fresh signed publication manifest within the configured retention window.",
            request_id,
        ),
        GenerationReplicationError::Engine(ReplicationError::Quota(_)) => ProblemDetails::new(
            StatusCode::TOO_MANY_REQUESTS,
            "GENERATION_REPLICATION_QUOTA",
            "Generation replication quota reached",
            "Wait for upload expiry or retention cleanup, or ask the operator to review audited capacity.",
            request_id,
        ),
        GenerationReplicationError::Engine(ReplicationError::Busy) => ProblemDetails::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "GENERATION_REPLICATION_BUSY",
            "Generation replication is busy",
            "Retry after the active generation operation completes.",
            request_id,
        ),
        GenerationReplicationError::Invalid
        | GenerationReplicationError::UnsafeSecret
        | GenerationReplicationError::Engine(
            ReplicationError::InvalidOwner
            | ReplicationError::MissingChunk
            | ReplicationError::UnknownChunk
            | ReplicationError::InvalidGeneration(_)
            | ReplicationError::Protocol(_),
        ) => ProblemDetails::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "GENERATION_REPLICATION_REJECTED",
            "Generation replication request failed validation",
            "Correct the closed manifest, rights, provenance, attribution, identity, hash, or chunk contract.",
            request_id,
        ),
        GenerationReplicationError::Io(error)
        | GenerationReplicationError::Engine(ReplicationError::Io(error)) => {
            error!(%request_id, %error, "generation replication I/O failed");
            ProblemDetails::internal(request_id)
        }
        GenerationReplicationError::Engine(error) => {
            error!(%request_id, %error, "generation replication state failed closed");
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
    use base64::Engine as _;
    use rustwx_core::{CycleSpec, GridShape, LatLonGrid};
    use rw_community_protocol::{
        AttributionNotice, BEGIN_RUN_GENERATION_SCHEMA, Compression, DataOrigin,
        FINALIZE_RUN_GENERATION_SCHEMA, MissingPolicy, OBJECT_SCHEMA, ObjectManifest,
        PROFILE_PAYLOAD_SCHEMA, ProfileObjectPayload, PublicationGrant, REQUEST_SCHEMA,
        REVOKE_RUN_GENERATION_SCHEMA, RUN_GENERATION_CHUNK_SCHEMA_V1, RUN_GENERATION_FILE_SCHEMA,
        RUN_GENERATION_REPLICATION_SCHEMA, RecipeIdentity, RunGenerationFile,
        RunGenerationFileChunk, RunGenerationFileKind, RunGenerationReplicationManifest,
        ShareQuery, ShareRequest, SourceProvenance, SurfaceSample, TimeWindow,
        generation_content_sha256, object_sha256, request_sha256, sign_object_manifest,
    };
    use rw_community_relay::{
        CloudflareTurnAdapter, ProviderCredentialLease, ProviderCredentialRequest, RelayError,
        RelayProvider, RelayRole, SecretText, parse_historical_lookup_response_bounded,
        parse_participant_grant_bounded,
    };
    use rw_query::RunSnapshot;
    use rw_scheduler::{
        OriginCatalogPlanConfig, OriginCatalogState, OriginCatalogStateStore,
        OriginPublishedGeneration, OriginPublishedLane, cycle_origin_unix,
    };
    use rw_store::ingest::{
        DerivedFieldInput, HourIngestWriter, write_hour_from_grid_with_derived_exact,
    };
    use rw_store::run::RwsRunManifest;
    use rw_store::{PressureVolumeInput, RwsExactTime, RwsSourceProvenance};
    use sha2::{Digest as _, Sha256};
    use std::collections::BTreeMap;
    use std::fs;
    use tower::ServiceExt;

    const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const RELAY_REQUESTER_TOKEN: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const RELAY_OUTSIDER_TOKEN: &str = "cccccccccccccccccccccccccccccccc";
    const FEDERATION_ORIGIN_TOKEN: &str = "dddddddddddddddddddddddddddddddd";
    const TEST_RELAY_ORIGIN_KEY_ID: &str = "test-origin";
    const FIXTURE_MODEL: &str = "fixture-model";
    const LEGACY_RETIRED_MODEL: &str = "rrfs-firewx";
    const FIXTURE_RUN: &str = "fixture-run";
    const FIXTURE_ORIGIN: i64 = 1_700_000_000;

    fn test_app() -> (tempfile::TempDir, Router) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let router = build_router(AppState::new(config, tokens).unwrap()).unwrap();
        (directory, router)
    }

    fn federation_local_http_test_app() -> (
        tempfile::TempDir,
        Router,
        ResolveObjectRequest,
        Vec<u8>,
        String,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let key_path = directory.path().join("community-signing.key");
        let origin_token_path = directory.path().join("federation-origin.token");
        crate::test_support::write_private_file(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode([31_u8; 32]),
        );
        crate::test_support::write_private_file(&origin_token_path, FEDERATION_ORIGIN_TOKEN);

        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        config.community.enabled = true;
        config.community.capacity_audit_completed = true;
        config.community.root = directory.path().join("community");
        config.community.signing_key_file = Some(key_path);
        config.federation.proxy.security_tests_passed = true;
        config.federation.proxy.accept_local_resolve = true;
        config.federation.proxy.local_resolve_token_file = Some(origin_token_path);
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let state = AppState::new(config, tokens).unwrap();

        let share = ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812T00Z".into(),
            snapshot_id: "a".repeat(64),
            grid_hash: "b".repeat(64),
            variables: vec!["temperature".into(), "temperature_2m".into()],
            query: ShareQuery::Profile {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                storage_slot: 1,
                valid_unix: 1_786_512_000,
                pressure_variables: vec!["temperature".into()],
                surface_variables: vec!["temperature_2m".into()],
                pressure_levels_hpa: vec![],
            },
            recipe: RecipeIdentity {
                recipe_id: "native-profile".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                forecast_producer: None,
                licensing_publisher: None,
                transport_provider: None,
                transport_is_mirror: false,
                roles: vec!["pressure".into(), "surface".into()],
                products: vec!["wrfprs".into(), "wrfsfc".into()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        };
        let identity = request_sha256(&share).unwrap();
        let object = serde_json::to_vec(&ProfileObjectPayload {
            schema: PROFILE_PAYLOAD_SCHEMA.into(),
            request_sha256: identity.clone(),
            profile: serde_json::json!({"schema": "rw.profile.test.v1", "levels": []}),
            surface_samples: vec![SurfaceSample {
                variable: "temperature_2m".into(),
                units: "K".into(),
                value: Some(299.0),
            }],
        })
        .unwrap();
        let authority = state
            .community
            .federation_authority_signing_material()
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        let signed = sign_object_manifest(
            ObjectManifest {
                schema: OBJECT_SCHEMA.into(),
                request: share.clone(),
                request_sha256: identity,
                object_sha256: object_sha256(&object),
                content_type: "application/json".into(),
                compression: Compression::None,
                encoded_size: object.len() as u64,
                decoded_size: object.len() as u64,
                attributions: vec![],
                modification_notices: vec!["Verified public-origin object.".into()],
                created_unix: now,
                expires_unix: now + 600,
            },
            authority.signing_key_id,
            &authority.signing_key,
        )
        .unwrap();
        state
            .community
            .stage_verified_federated_object(&signed.manifest.request_sha256, &signed, &object)
            .unwrap();
        let sha256 = signed.manifest.object_sha256;
        let router = build_router(state).unwrap();
        (
            directory,
            router,
            ResolveObjectRequest {
                schema: rw_community_protocol::RESOLVE_SCHEMA.into(),
                request: share,
            },
            object,
            sha256,
        )
    }

    fn federation_proxy_control_http_test_app() -> (tempfile::TempDir, Router) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN, RELAY_REQUESTER_TOKEN]).unwrap();
        let operator_header = HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap();
        let operator_principal = tokens
            .authorization_principal(Some(&operator_header))
            .unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let mut state = AppState::new(config, tokens).unwrap();
        state.federation_proxy = Some(Arc::new(
            crate::federation_proxy::test_server_federation_proxy(
                &directory.path().join("federation-control.json"),
                true,
                operator_principal,
            ),
        ));
        state.metrics.set_federation_proxy_kill_switch(true);
        (directory, build_router(state).unwrap())
    }

    #[derive(Debug, Default)]
    struct TestRelayProvider {
        issued: u64,
    }

    impl RelayProvider for TestRelayProvider {
        fn issue(
            &mut self,
            request: &ProviderCredentialRequest,
            now_unix: i64,
        ) -> Result<ProviderCredentialLease, RelayError> {
            self.issued = self.issued.saturating_add(1);
            let response = serde_json::json!({
                "iceServers": [{
                    "urls": ["turn:turn.cloudflare.com:3478?transport=udp"],
                    "username": format!("test-turn-user-{}", self.issued),
                    "credential": format!("test-turn-secret-{}", self.issued),
                }]
            });
            CloudflareTurnAdapter::default().parse_and_sanitize(
                &serde_json::to_vec(&response).unwrap(),
                now_unix,
                request.expires_unix,
            )
        }

        fn revoke(&mut self, _revocation_id: &SecretText) -> Result<(), RelayError> {
            Ok(())
        }
    }

    fn relay_http_test_app() -> (
        tempfile::TempDir,
        Router,
        ed25519_dalek::SigningKey,
        ed25519_dalek::VerifyingKey,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens =
            crate::TokenSet::from_tokens([TOKEN, RELAY_REQUESTER_TOKEN, RELAY_OUTSIDER_TOKEN])
                .unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let mut state = AppState::new(config, tokens).unwrap();
        let origin_key = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let relay_key = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let relay_verifying_key = relay_key.verifying_key();
        let relay_config = crate::config::CommunityRelayConfig {
            enabled: true,
            security_tests_passed: true,
            capacity_audit_completed: true,
            provider_pricing_verified: true,
            kill_switch: false,
            state_file: directory.path().join("relay-state.json"),
            cloudflare: crate::config::CloudflareRelayConfig {
                audited_relay_cidrs: vec!["104.16.0.0/24".into()],
                ..crate::config::CloudflareRelayConfig::default()
            },
            ..crate::config::CommunityRelayConfig::default()
        };
        state.community_relay = crate::community_relay::CommunityRelayService::with_test_provider(
            &relay_config,
            rw_community_protocol::ProtocolLimits::default(),
            TEST_RELAY_ORIGIN_KEY_ID,
            origin_key.verifying_key(),
            relay_key,
            Box::new(TestRelayProvider::default()),
        )
        .unwrap();
        let router = build_router(state).unwrap();
        (directory, router, origin_key, relay_verifying_key)
    }

    fn signed_relay_test_manifest(
        origin_key: &ed25519_dalek::SigningKey,
    ) -> rw_community_protocol::SignedObjectManifest {
        let now = chrono::Utc::now().timestamp();
        let request = ShareRequest {
            schema: REQUEST_SCHEMA.into(),
            model: "hrrr".into(),
            run: "20260812T00Z".into(),
            snapshot_id: "a".repeat(64),
            grid_hash: "b".repeat(64),
            variables: vec!["temperature_2m".into()],
            query: ShareQuery::PointSeries {
                latitude_e7: 350_000_000,
                longitude_e7: -970_000_000,
                window: TimeWindow::Utc {
                    start_unix: now.saturating_sub(3_600),
                    end_unix: now,
                },
                missing_policy: MissingPolicy::Strict,
            },
            recipe: RecipeIdentity {
                recipe_id: "native-window".into(),
                recipe_version: "1".into(),
                parameters: BTreeMap::new(),
            },
            source_provenance: vec![SourceProvenance {
                provider: "noaa-aws-public-data".into(),
                forecast_producer: None,
                licensing_publisher: None,
                transport_provider: None,
                transport_is_mirror: false,
                roles: vec!["surface".into()],
                products: vec!["wrfsfc".into()],
            }],
            publication: PublicationGrant {
                data_origin: DataOrigin::PublicProvider,
                explicit_owner_publication: false,
                redistribution_rights_confirmed: true,
            },
        };
        let body = vec![7_u8; 4096];
        let manifest = ObjectManifest {
            schema: OBJECT_SCHEMA.into(),
            request_sha256: request_sha256(&request).unwrap(),
            object_sha256: object_sha256(&body),
            request,
            content_type: "application/vnd.rusty-weather.window+zstd".into(),
            compression: Compression::None,
            encoded_size: body.len() as u64,
            decoded_size: body.len() as u64,
            attributions: Vec::new(),
            modification_notices: vec!["Derived by Rusty Weather.".into()],
            created_unix: now.saturating_sub(1),
            expires_unix: now.saturating_add(3600),
        };
        sign_object_manifest(manifest, TEST_RELAY_ORIGIN_KEY_ID, origin_key).unwrap()
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
        config.server.cache_root = directory.path().join("cache");
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
            let optional_850 = [270.0 + slot as f32; 4];
            let optional_500 = [240.0 + slot as f32; 4];
            let mut volumes = vec![PressureVolumeInput {
                name: "temperature_iso",
                units: "K",
                selector_template: serde_json::json!({"fixture": "temperature_iso"}),
                levels: vec![(850, &pressure_850), (500, &pressure_500)],
            }];
            if slot == 1 {
                volumes.push(PressureVolumeInput {
                    name: "optional_pressure_iso",
                    units: "K",
                    selector_template: serde_json::json!({"fixture": "optional_pressure_iso"}),
                    levels: vec![(850, &optional_850), (500, &optional_500)],
                });
            }
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
            RwsSourceProvenance::new_structured(
                "ECMWF-OPEN-DATA",
                "ECMWF",
                "ECMWF",
                "ECMWF-OPEN-DATA",
                false,
                vec!["pressure".into()],
                vec!["oper".into()],
            )
            .unwrap(),
        ];
        manifest.hours.get_mut(&1).unwrap().source_provenance = vec![
            RwsSourceProvenance::new_structured(
                "ecmwf-open-data",
                "ecmwf",
                "ecmwf",
                "ecmwf-open-data",
                false,
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
        post_json_with_token(app, path, value, TOKEN).await
    }

    async fn post_json_with_token(
        app: Router,
        path: &str,
        value: serde_json::Value,
        token: &str,
    ) -> Response {
        app.oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
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

    fn replication_file(
        kind: RunGenerationFileKind,
        file_name: &str,
        bytes: &[u8],
    ) -> RunGenerationFile {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        RunGenerationFile {
            schema: RUN_GENERATION_FILE_SCHEMA.into(),
            kind,
            file_name: file_name.into(),
            byte_size: bytes.len() as u64,
            file_sha256: sha256.clone(),
            chunks: vec![RunGenerationFileChunk {
                schema: RUN_GENERATION_CHUNK_SCHEMA_V1.into(),
                ordinal: 0,
                file_offset: 0,
                object_sha256: sha256,
                byte_size: bytes.len() as u64,
            }],
        }
    }

    fn replication_http_fixture(
        source_store: &std::path::Path,
        owner_principal_sha256: String,
        now_unix: i64,
    ) -> (RunGenerationReplicationManifest, BTreeMap<String, Vec<u8>>) {
        const MODEL: &str = "wrf";
        const RUN: &str = "20260812_00z";
        const VALID: i64 = 1_786_512_000;
        let grid = LatLonGrid::new(
            GridShape::new(2, 2).unwrap(),
            vec![40.0, 40.0, 41.0, 41.0],
            vec![-100.0, -99.0, -100.0, -99.0],
        )
        .unwrap();
        let mut writer = HourIngestWriter::begin_exact(
            source_store,
            MODEL,
            RUN,
            0,
            RwsExactTime::new(0, VALID),
            &grid,
            None,
            "replication-http-test",
        )
        .unwrap();
        writer
            .set_source_provenance(vec![
                RwsSourceProvenance::new(
                    "simulation-owner",
                    vec!["generation".into()],
                    vec!["rws".into()],
                )
                .unwrap(),
            ])
            .unwrap();
        writer
            .add_derived_2d("temperature", "K", &[280.0, 281.0, 282.0, 283.0])
            .unwrap();
        writer.finish(now_unix as u64).unwrap();

        let run_dir = source_store.join(MODEL).join(RUN);
        let snapshot = RunSnapshot::open(source_store, MODEL, RUN).unwrap();
        let mut objects = BTreeMap::new();
        let mut file = |kind, name: &str| {
            let bytes = fs::read(run_dir.join(name)).unwrap();
            let descriptor = replication_file(kind, name, &bytes);
            objects.insert(descriptor.chunks[0].object_sha256.clone(), bytes);
            descriptor
        };
        let files = vec![
            file(RunGenerationFileKind::RunManifest, "run.json"),
            file(RunGenerationFileKind::Grid, "grid.rwg"),
            file(
                RunGenerationFileKind::Hour {
                    storage_slot: 0,
                    valid_unix: VALID,
                },
                "f000.rws",
            ),
        ];
        let mut manifest = RunGenerationReplicationManifest {
            schema: RUN_GENERATION_REPLICATION_SCHEMA.into(),
            generation_id: "wrf-http-lifecycle".into(),
            model: MODEL.into(),
            run: RUN.into(),
            source_snapshot_id: snapshot.descriptor().snapshot_id.clone(),
            grid_hash: snapshot.descriptor().grid_hash.clone(),
            owner_principal_sha256,
            publication: PublicationGrant {
                data_origin: DataOrigin::PrivateWrf,
                explicit_owner_publication: true,
                redistribution_rights_confirmed: true,
            },
            source_provenance: vec![SourceProvenance {
                provider: "simulation-owner".into(),
                forecast_producer: None,
                licensing_publisher: None,
                transport_provider: None,
                transport_is_mirror: false,
                roles: vec!["generation".into()],
                products: vec!["rws".into()],
            }],
            total_bytes: files.iter().map(|file| file.byte_size).sum(),
            files,
            generation_sha256: "00".repeat(32),
            published_unix: now_unix,
            retain_until_unix: now_unix + 3_600,
            attributions: vec![AttributionNotice {
                provider: "simulation-owner".into(),
                notice: "Published by the simulation owner.".into(),
                source_url: "https://example.invalid/source".into(),
                license: "Owner-authorized redistribution".into(),
                license_url: "https://example.invalid/license".into(),
                terms_url: "https://example.invalid/terms".into(),
                disclaimer: "Experimental simulation.".into(),
            }],
            modification_notices: vec!["Inventoried as immutable rws chunks.".into()],
        };
        manifest.generation_sha256 = generation_content_sha256(&manifest).unwrap();
        (manifest, objects)
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
    async fn browser_cors_supports_map_tiles_and_conditional_get_without_credentials() {
        let origin = HeaderValue::from_static("https://radar.example.edu");
        let app = Router::new()
            .route(
                "/tile",
                get(|| async {
                    let mut response = Response::new(Body::from("tile"));
                    response
                        .headers_mut()
                        .insert(header::ETAG, HeaderValue::from_static("\"fixture\""));
                    response
                }),
            )
            .layer(browser_cors_layer(vec![origin.clone()]));

        let preflight = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::OPTIONS)
                    .uri("/tile")
                    .header(header::ORIGIN, origin.clone())
                    .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                    .header(
                        header::ACCESS_CONTROL_REQUEST_HEADERS,
                        "authorization,if-none-match",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(preflight.status(), StatusCode::OK);
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            origin
        );
        let allowed_headers = preflight.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed_headers.contains("authorization"));
        assert!(allowed_headers.contains("if-none-match"));
        assert!(
            !preflight
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );

        let get_response = app
            .oneshot(
                Request::builder()
                    .uri("/tile")
                    .header(header::ORIGIN, "https://radar.example.edu")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let exposed = get_response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(exposed.contains("etag"));
        assert!(exposed.contains("x-rw-satellite-frame"));
        assert!(exposed.contains("x-rw-satellite-source-revision"));
        assert!(
            !get_response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );
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

        let denied_profile_cycle = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/profile-cycle")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied_profile_cycle.status(), StatusCode::UNAUTHORIZED);

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
        assert_eq!(models.len(), 34);
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

        let gdps = model("gdps");
        assert_eq!(gdps["ingest_status"], "ready");
        assert_eq!(gdps["verification"], "live_verified");
        assert_eq!(
            gdps["limitations"],
            serde_json::json!(["sparse_pressure_levels"])
        );
        assert_eq!(
            gdps["provider_attributions"][0]["notice"],
            "Data Source: Environment and Climate Change Canada"
        );
        let geml = model("gdps-geml");
        assert_eq!(geml["ingest_status"], "ready");
        assert_eq!(geml["verification"], "live_verified");
        assert_eq!(
            geml["limitations"],
            serde_json::json!([
                "sparse_pressure_levels",
                "derived_products_disabled",
                "pre_operational_feed"
            ])
        );
        assert_eq!(geml["products"][0]["product"], "rws-pressure");
        assert_eq!(geml["products"][1]["product"], "rws-surface");
        assert_eq!(
            geml["provider_attributions"][0]["source_url"],
            "https://eccc-msc.github.io/open-data/msc-data/nwp_gdps/readme_gdps-geml-datamart_en/"
        );
        for id in ["rdps", "hrdps"] {
            let regional = model(id);
            assert_eq!(regional["ingest_status"], "ready");
            assert_eq!(regional["verification"], "live_verified");
            assert_eq!(
                regional["limitations"],
                serde_json::json!(["sparse_pressure_levels", "derived_products_disabled"])
            );
            assert_eq!(regional["products"][0]["product"], "rws-pressure");
            assert_eq!(regional["products"][1]["product"], "rws-surface");
            assert_eq!(
                regional["provider_attributions"][0]["notice"],
                "Data Source: Environment and Climate Change Canada"
            );
        }
        let geps = model("geps");
        assert_eq!(geps["ingest_status"], "ready");
        assert_eq!(geps["verification"], "live_verified");
        assert_eq!(
            geps["limitations"],
            serde_json::json!([
                "provider_statistics_only",
                "sparse_pressure_levels",
                "two_dimensional_statistics_only",
                "derived_products_disabled",
                "extended_range_not_scheduled"
            ])
        );
        assert_eq!(geps["products"][0]["product"], "rws-published-statistics");
        assert_eq!(geps["products"][0]["surface_source"], true);
        assert_eq!(geps["products"][0]["pressure_source"], false);
        assert_eq!(
            geps["provider_attributions"][0]["notice"],
            "Data Source: Environment and Climate Change Canada"
        );
        assert_eq!(
            geps["provider_attributions"][0]["source_url"],
            "https://eccc-msc.github.io/open-data/msc-data/nwp_geps/readme_geps-datamart_en/"
        );

        let reps = model("reps");
        assert_eq!(reps["ingest_status"], "ready");
        assert_eq!(reps["verification"], "live_verified");
        assert_eq!(
            reps["limitations"],
            serde_json::json!([
                "provider_statistics_only",
                "surface_only",
                "derived_products_disabled"
            ])
        );
        assert_eq!(
            reps["products"][0]["product"],
            "rws-reps-provider-statistics"
        );
        assert_eq!(reps["products"][0]["surface_source"], true);
        assert_eq!(reps["products"][0]["pressure_source"], false);
        assert_eq!(
            reps["provider_attributions"][0]["source_url"],
            "https://eccc-msc.github.io/open-data/msc-data/nwp_reps/readme_reps-datamart_en/"
        );

        for id in ["icon-eu", "icon-d2"] {
            let icon = model(id);
            assert_eq!(icon["ingest_status"], "ready");
            assert_eq!(icon["verification"], "live_verified");
            assert_eq!(
                icon["limitations"],
                serde_json::json!(["sparse_pressure_levels", "derived_products_disabled"])
            );
            assert_eq!(icon["products"][0]["product"], "rws-pressure");
            assert_eq!(icon["products"][1]["product"], "rws-surface");
            assert_eq!(
                icon["provider_attributions"][0]["notice"],
                "Source: Deutscher Wetterdienst"
            );
            assert_eq!(
                icon["provider_attributions"][0]["license"],
                "Creative Commons Attribution 4.0 International (CC BY 4.0)."
            );
        }

        let icon_ru = model("icon-ru");
        assert_eq!(icon_ru["ingest_status"], "ready");
        assert_eq!(icon_ru["verification"], "live_verified");
        assert_eq!(icon_ru["registry_source_count"], 2);
        assert_eq!(
            icon_ru["limitations"],
            serde_json::json!(["sparse_pressure_levels", "derived_products_disabled"])
        );
        assert_eq!(icon_ru["products"][0]["product"], "rws-pressure");
        assert_eq!(icon_ru["products"][1]["product"], "rws-surface");
        assert_eq!(
            icon_ru["provider_attributions"][0]["notice"],
            "Data source: Roshydromet WIPPS Designated Centre Moscow, distributed through WIS2."
        );

        for id in ["wrf-cptec-7km", "brams-cptec-8km"] {
            let cptec = model(id);
            assert_eq!(cptec["ingest_status"], "ready");
            assert_eq!(cptec["verification"], "live_verified");
            assert_eq!(cptec["cycle_hours_utc"], serde_json::json!([0]));
            assert_eq!(cptec["max_forecast_hour"], 180);
            assert_eq!(
                cptec["limitations"],
                serde_json::json!(["sparse_pressure_levels", "derived_products_disabled"])
            );
            assert_eq!(cptec["products"][0]["product"], "raw");
            assert_eq!(cptec["products"][0]["surface_source"], true);
            assert_eq!(cptec["products"][0]["pressure_source"], true);
            assert_eq!(cptec["products"][0]["indexed_subset"], true);
            assert!(
                cptec["provider_attributions"][0]["provider"]
                    .as_str()
                    .unwrap()
                    .contains("CPTEC")
            );
            assert!(
                cptec["provider_attributions"][0]["notice"]
                    .as_str()
                    .unwrap()
                    .contains("CPTEC Data Server")
            );
            assert!(
                cptec["provider_attributions"][0]["license"]
                    .as_str()
                    .unwrap()
                    .contains("no model-directory-specific")
            );
        }

        for id in ["rap", "nam"] {
            assert_eq!(model(id)["verification"], "live_verified");
        }
        for id in ["hrrr-ak", "gdas"] {
            assert_eq!(model(id)["verification"], "fixture_verified");
        }
        assert_eq!(model("hrrr-ak")["products"][0]["product"], "prs");
        assert_eq!(model("hrrr-ak")["products"][1]["product"], "sfc");
        assert_eq!(model("rap")["products"][0]["product"], "awp130pgrb");
        assert_eq!(model("nam")["products"][0]["product"], "awip3d");
        assert_eq!(model("gdas")["products"][0]["product"], "pgrb2.0p25");

        for id in ["aigefs", "hgefs"] {
            assert_eq!(
                model(id)["limitations"],
                serde_json::json!([
                    "ensemble_mean_only",
                    "sparse_pressure_levels",
                    "derived_products_disabled"
                ])
            );
            assert_eq!(model(id)["verification"], "live_verified");
            assert!(
                model(id)["products"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|product| product["indexed_subset"] == false)
            );
        }
        assert_eq!(
            model("gefs")["limitations"],
            serde_json::json!([
                "ensemble_control_member_only",
                "sparse_pressure_levels",
                "derived_products_disabled"
            ])
        );
        assert_eq!(model("gefs")["verification"], "live_verified");
        assert_eq!(model("gefs")["max_forecast_hour"], 840);
        assert_eq!(model("gefs")["products"][0]["indexed_subset"], true);
        for id in ["aigfs", "ecmwf-open-data"] {
            assert_eq!(
                model(id)["limitations"],
                serde_json::json!(["sparse_pressure_levels", "derived_products_disabled"])
            );
            assert_eq!(model(id)["verification"], "live_verified");
        }
        assert!(
            model("aigfs")["products"]
                .as_array()
                .unwrap()
                .iter()
                .all(|product| product["indexed_subset"] == false)
        );
        assert_eq!(
            model("ecmwf-open-data")["products"][0]["indexed_subset"],
            true
        );
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
    async fn latest_run_pointer_is_authenticated_and_never_cached() {
        let (_directory, app) = test_app_with_store();
        let path = format!("/v1/models/{FIXTURE_MODEL}/latest-run");

        let denied = app
            .clone()
            .oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let latest = get_with_token(app.clone(), &path).await;
        assert_eq!(latest.status(), StatusCode::OK);
        assert_eq!(latest.headers()[header::CACHE_CONTROL], "no-store, private");
        assert_eq!(latest.headers()[header::PRAGMA], "no-cache");
        assert!(!latest.headers().contains_key(header::ETAG));
        let latest = response_json(latest).await;
        assert_eq!(latest["model"], FIXTURE_MODEL);
        assert_eq!(latest["run"], FIXTURE_RUN);
        assert_eq!(latest["origin_unix"], FIXTURE_ORIGIN);

        // `latest` remains a legal run ID. The old colliding spelling now
        // dispatches through the ordinary run-detail route, not the pointer.
        let literal_run = get_with_token(
            app.clone(),
            &format!("/v1/models/{FIXTURE_MODEL}/runs/latest"),
        )
        .await;
        assert_eq!(literal_run.status(), StatusCode::NOT_FOUND);

        let missing = get_with_token(app, "/v1/models/not-present/latest-run").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            missing.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(missing.headers()[header::PRAGMA], "no-cache");
        assert_eq!(response_json(missing).await["code"], "DATA_NOT_FOUND");
    }

    #[tokio::test]
    async fn federation_catalog_is_authenticated_and_feature_gated() {
        let (_directory, app) = test_app();
        for path in [
            rw_community_protocol::FEDERATION_CATALOG_PATH,
            "/v1/federation/health",
        ] {
            let denied = app
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

            let disabled = get_with_token(app.clone(), path).await;
            assert_eq!(disabled.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = response_json(disabled).await;
            assert_eq!(body["code"], "FEDERATION_DISABLED");
        }
    }

    #[tokio::test]
    async fn federation_one_hop_routes_use_a_separate_token_and_serve_verified_bytes() {
        let (_directory, app, resolve, expected_object, object_sha256) =
            federation_local_http_test_app();
        let resolve_body = serde_json::to_vec(&resolve).unwrap();
        let request = |token: &'static str, hop: Option<&'static str>| {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri(FEDERATION_LOCAL_RESOLVE_PATH)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(hop) = hop {
                builder = builder.header(FEDERATION_HOP_HEADER, hop);
            }
            builder.body(Body::from(resolve_body.clone())).unwrap()
        };

        let ordinary_token_denied = app
            .clone()
            .oneshot(request(TOKEN, Some("1")))
            .await
            .unwrap();
        assert_eq!(ordinary_token_denied.status(), StatusCode::UNAUTHORIZED);

        let missing_hop = app
            .clone()
            .oneshot(request(FEDERATION_ORIGIN_TOKEN, None))
            .await
            .unwrap();
        assert_eq!(missing_hop.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let resolved = app
            .clone()
            .oneshot(request(FEDERATION_ORIGIN_TOKEN, Some("1")))
            .await
            .unwrap();
        assert_eq!(resolved.status(), StatusCode::OK);
        let resolved_json = response_json(resolved).await;
        assert_eq!(
            resolved_json["signed_manifest"]["manifest"]["object_sha256"],
            object_sha256
        );
        let serialized = serde_json::to_string(&resolved_json).unwrap();
        assert!(!serialized.contains(FEDERATION_ORIGIN_TOKEN));
        assert!(!serialized.contains("https://"));

        let object = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "{FEDERATION_LOCAL_OBJECT_PATH_PREFIX}/{object_sha256}"
                    ))
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {FEDERATION_ORIGIN_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(object.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(object.into_body(), 1024 * 1024).await.unwrap(),
            expected_object
        );

        let recursive = FederationProxyRequest {
            schema: rw_federation_proxy::FEDERATION_PROXY_SCHEMA.into(),
            request: resolve.request,
            preferred_origin_id: None,
        };
        let rejected_reentry = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(FEDERATION_PROXY_PATH)
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(FEDERATION_HOP_HEADER, "1")
                    .body(Body::from(serde_json::to_vec(&recursive).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected_reentry.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let problem = response_json(rejected_reentry).await;
        let text = serde_json::to_string(&problem).unwrap();
        assert!(!text.contains(FEDERATION_ORIGIN_TOKEN));
        assert!(!text.contains("127.0.0.1"));
    }

    #[tokio::test]
    async fn federation_proxy_operator_control_is_authenticated_coarse_durable_and_no_store() {
        let (_directory, app) = federation_proxy_control_http_test_app();
        let status_path = "/v1/federation/proxy/operator/status";
        let kill_path = "/v1/federation/proxy/operator/kill-switch";

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(status_path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let forbidden = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(status_path)
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {RELAY_REQUESTER_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let status = get_with_token(app.clone(), status_path).await;
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(status.headers()[header::CACHE_CONTROL], "no-store, private");
        let status = response_json(status).await;
        assert_eq!(status["kill_switch"], true);
        assert_eq!(status["persistence_healthy"], true);
        let serialized = serde_json::to_string(&status).unwrap();
        for forbidden in ["principal", "origin", "url", "address", "credential"] {
            assert!(!serialized.contains(forbidden));
        }

        let set = |schema: &'static str, engaged: bool| {
            Request::builder()
                .method(Method::POST)
                .uri(kill_path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(
                        &crate::federation_proxy::FederationProxyKillSwitchRequest {
                            schema: schema.into(),
                            engaged,
                        },
                    )
                    .unwrap(),
                ))
                .unwrap()
        };
        let invalid = app.clone().oneshot(set("wrong", false)).await.unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let disengaged = app
            .clone()
            .oneshot(set(
                crate::federation_proxy::FEDERATION_PROXY_KILL_SWITCH_SCHEMA,
                false,
            ))
            .await
            .unwrap();
        assert_eq!(disengaged.status(), StatusCode::OK);
        assert_eq!(
            disengaged.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert_eq!(response_json(disengaged).await["kill_switch"], false);

        let metrics = get_with_token(app.clone(), "/metrics").await;
        let metrics = String::from_utf8(
            to_bytes(metrics.into_body(), 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(metrics.contains("rw_federation_proxy_kill_switch 0"));

        let engaged = app
            .oneshot(set(
                crate::federation_proxy::FEDERATION_PROXY_KILL_SWITCH_SCHEMA,
                true,
            ))
            .await
            .unwrap();
        assert_eq!(engaged.status(), StatusCode::OK);
        assert_eq!(response_json(engaged).await["kill_switch"], true);
    }

    #[tokio::test]
    async fn generation_chunk_route_enforces_media_type_empty_size_and_hash_gates() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.generation_replication.limits.maximum_chunk_bytes = 4;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();
        let path = format!(
            "/v1/community/generations/generation-a/chunks/{}",
            "a".repeat(64)
        );
        let request = |content_type: &'static str, body: Vec<u8>, path: &str| {
            Request::builder()
                .method(Method::POST)
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header(header::CONTENT_TYPE, content_type)
                .body(Body::from(body))
                .unwrap()
        };

        let parameterized = app
            .clone()
            .oneshot(request(
                "application/octet-stream; charset=binary",
                vec![1],
                &path,
            ))
            .await
            .unwrap();
        assert_eq!(parameterized.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

        let empty = app
            .clone()
            .oneshot(request("application/octet-stream", Vec::new(), &path))
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let oversized = app
            .clone()
            .oneshot(request("application/octet-stream", vec![1; 5], &path))
            .await
            .unwrap();
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);

        let malformed_hash_path = "/v1/community/generations/generation-a/chunks/not-a-sha256";
        let malformed_hash = app
            .clone()
            .oneshot(request(
                "application/octet-stream",
                vec![1],
                malformed_hash_path,
            ))
            .await
            .unwrap();
        assert_eq!(malformed_hash.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let disabled = app
            .oneshot(request("application/octet-stream", vec![1], &path))
            .await
            .unwrap();
        assert_eq!(disabled.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn replication_owner_identity_is_caller_isolated_and_operator_status_is_coarse() {
        use base64::Engine as _;

        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.origin_catalog.enabled = true;
        config.origin_catalog.publication_sources =
            crate::origin_catalog::PublicationSourceMode::Replication;
        config.generation_replication.enabled = true;
        config.generation_replication.security_tests_passed = true;
        config.generation_replication.capacity_audit_completed = true;
        config.generation_replication.control_root = directory.path().join("replication");
        let key_path = directory.path().join("replication.key");
        crate::test_support::write_private_file(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode([91; 32]),
        );
        config.generation_replication.signing_key_file = Some(key_path);
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN, RELAY_REQUESTER_TOKEN]).unwrap();
        let operator_header = HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap();
        let operator_principal = tokens
            .authorization_principal(Some(&operator_header))
            .unwrap();
        config.generation_replication.operator_principals = vec![operator_principal];
        config.validate(!tokens.is_empty()).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

        let owner = |token: &'static str| {
            Request::builder()
                .uri("/v1/community/generation-replication/owner")
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap()
        };
        let first = app.clone().oneshot(owner(TOKEN)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(first.headers()[header::CACHE_CONTROL], "no-store, private");
        let first = response_json(first).await;
        let second = app
            .clone()
            .oneshot(owner(RELAY_REQUESTER_TOKEN))
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::OK);
        let second = response_json(second).await;
        assert_ne!(
            first["owner_principal_sha256"],
            second["owner_principal_sha256"]
        );
        assert_eq!(first["owner_principal_sha256"].as_str().unwrap().len(), 64);

        let operator = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/community/generation-replication/operator/status")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(operator.status(), StatusCode::OK);
        let serialized = serde_json::to_string(&response_json(operator).await).unwrap();
        for forbidden in [
            "owner_principal",
            "generation_id",
            "store_root",
            "source_url",
        ] {
            assert!(!serialized.contains(forbidden));
        }

        let outsider = app
            .oneshot(
                Request::builder()
                    .uri("/v1/community/generation-replication/operator/status")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {RELAY_REQUESTER_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outsider.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn replication_http_lifecycle_publishes_queries_and_revokes_exact_generation() {
        let directory = tempfile::tempdir().unwrap();
        let source_store = directory.path().join("source");
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.origin_catalog.enabled = true;
        config.origin_catalog.publication_sources =
            crate::origin_catalog::PublicationSourceMode::Replication;
        config.generation_replication.enabled = true;
        config.generation_replication.security_tests_passed = true;
        config.generation_replication.capacity_audit_completed = true;
        config.generation_replication.kill_switch = false;
        config.generation_replication.control_root = directory.path().join("replication");
        let key_path = directory.path().join("replication.key");
        crate::test_support::write_private_file(
            &key_path,
            base64::engine::general_purpose::STANDARD.encode([92; 32]),
        );
        config.generation_replication.signing_key_file = Some(key_path);
        fs::create_dir_all(&source_store).unwrap();
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN, RELAY_REQUESTER_TOKEN]).unwrap();
        let operator_header = HeaderValue::from_str(&format!("Bearer {TOKEN}")).unwrap();
        let operator_principal = tokens
            .authorization_principal(Some(&operator_header))
            .unwrap();
        config.generation_replication.operator_principals = vec![operator_principal];
        config.validate(!tokens.is_empty()).unwrap();
        let expected_replication = config.generation_replication.clone();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

        let owner_response =
            get_with_token(app.clone(), "/v1/community/generation-replication/owner").await;
        assert_eq!(owner_response.status(), StatusCode::OK);
        let owner = response_json(owner_response).await["owner_principal_sha256"]
            .as_str()
            .unwrap()
            .to_string();
        let capabilities = get_with_token(
            app.clone(),
            rw_community_protocol::RUN_GENERATION_CAPABILITIES_PATH,
        )
        .await;
        assert_eq!(capabilities.status(), StatusCode::OK);
        assert_eq!(
            capabilities.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        let capabilities = response_json(capabilities).await;
        assert_eq!(capabilities["owner_principal_sha256"], owner);
        assert_eq!(capabilities["accepting_uploads"], true);
        assert_eq!(
            capabilities["limits"]["maximum_chunk_bytes"],
            expected_replication.limits.maximum_chunk_bytes
        );
        assert_eq!(
            capabilities["limits"]["maximum_retention_seconds"],
            expected_replication.limits.maximum_retention_seconds
        );
        assert_eq!(
            capabilities["limits"]["upload_ttl_seconds"],
            expected_replication.quotas.upload_ttl_seconds
        );
        assert_eq!(
            capabilities["quota"]["maximum_storage_bytes"],
            expected_replication.quotas.per_owner_storage_bytes
        );
        assert_eq!(capabilities["usage"]["active_uploads"], 0);
        let outsider_list = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/community/generations?limit=10")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {RELAY_REQUESTER_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outsider_list.status(), StatusCode::OK);
        assert!(
            response_json(outsider_list).await["records"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let now = chrono::Utc::now().timestamp();
        let (manifest, objects) = replication_http_fixture(&source_store, owner, now);
        let generation_id = manifest.generation_id.clone();
        let generation_sha256 = manifest.generation_sha256.clone();

        let begin = post_json(
            app.clone(),
            "/v1/community/generations",
            serde_json::json!({
                "schema": BEGIN_RUN_GENERATION_SCHEMA,
                "manifest": manifest,
            }),
        )
        .await;
        assert_eq!(begin.status(), StatusCode::CREATED);
        assert_eq!(begin.headers()[header::CACHE_CONTROL], "no-store, private");
        let begin = response_json(begin).await;
        assert_eq!(begin["missing_chunks"], objects.len());

        let missing_path = format!("/v1/community/generations/{generation_id}/missing?limit=32");
        let missing = get_with_token(app.clone(), &missing_path).await;
        assert_eq!(missing.status(), StatusCode::OK);
        let missing = response_json(missing).await;
        assert_eq!(missing["chunks"].as_array().unwrap().len(), objects.len());

        for (sha256, bytes) in &objects {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(format!(
                            "/v1/community/generations/{generation_id}/chunks/{sha256}"
                        ))
                        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                        .header(header::CONTENT_TYPE, "application/octet-stream")
                        .body(Body::from(bytes.clone()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-store, private"
            );
        }
        let missing = get_with_token(app.clone(), &missing_path).await;
        assert_eq!(missing.status(), StatusCode::OK);
        assert!(
            response_json(missing).await["chunks"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        let finalize = post_json(
            app.clone(),
            &format!("/v1/community/generations/{generation_id}/finalize"),
            serde_json::json!({
                "schema": FINALIZE_RUN_GENERATION_SCHEMA,
                "generation_sha256": generation_sha256,
            }),
        )
        .await;
        assert_eq!(finalize.status(), StatusCode::CREATED);
        let finalized = response_json(finalize).await;
        assert_eq!(finalized["generation_id"], generation_id);
        assert_eq!(finalized["model"], "wrf");
        assert_eq!(finalized["run"], "20260812_00z");

        let replay_finalize = post_json(
            app.clone(),
            &format!("/v1/community/generations/{generation_id}/finalize"),
            serde_json::json!({
                "schema": FINALIZE_RUN_GENERATION_SCHEMA,
                "generation_sha256": generation_sha256,
            }),
        )
        .await;
        assert_eq!(replay_finalize.status(), StatusCode::OK);
        assert_eq!(response_json(replay_finalize).await, finalized);

        let exact_path = format!("/v1/community/generations/{generation_id}/publication");
        let exact = get_with_token(app.clone(), &exact_path).await;
        assert_eq!(exact.status(), StatusCode::OK);
        assert_eq!(exact.headers()[header::CACHE_CONTROL], "no-store, private");
        assert_eq!(response_json(exact).await["state"], "published");
        let outsider_exact = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&exact_path)
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {RELAY_REQUESTER_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outsider_exact.status(), StatusCode::NOT_FOUND);
        let list = get_with_token(app.clone(), "/v1/community/generations?limit=1").await;
        assert_eq!(list.status(), StatusCode::OK);
        let list = response_json(list).await;
        assert_eq!(list["records"][0]["state"], "published");
        assert_eq!(list["records"][0]["generation_id"], generation_id);
        assert!(list.get("next_after").is_none());
        assert_eq!(
            get_with_token(app.clone(), "/v1/community/generations?limit=101")
                .await
                .status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );

        let query_path = "/v1/models/wrf/runs/20260812_00z";
        let query = get_with_token(app.clone(), query_path).await;
        assert_eq!(query.status(), StatusCode::OK);
        let query = response_json(query).await;
        assert_eq!(query["model"], "wrf");
        assert_eq!(query["run"], "20260812_00z");

        let revoke = post_json(
            app.clone(),
            &format!("/v1/community/generations/{generation_id}/revoke"),
            serde_json::json!({
                "schema": REVOKE_RUN_GENERATION_SCHEMA,
                "generation_sha256": generation_sha256,
                "rights_withdrawn": true,
                "reason": "Owner withdrew this HTTP test publication.",
            }),
        )
        .await;
        assert_eq!(revoke.status(), StatusCode::OK);
        let revoke = response_json(revoke).await;
        assert_eq!(revoke["generation_id"], generation_id);
        assert_eq!(revoke["rights_withdrawn"], true);
        let tombstone = get_with_token(app.clone(), &exact_path).await;
        assert_eq!(tombstone.status(), StatusCode::OK);
        assert_eq!(response_json(tombstone).await["state"], "tombstone");

        assert_eq!(
            get_with_token(app.clone(), query_path).await.status(),
            StatusCode::NOT_FOUND
        );
        let status = get_with_token(
            app.clone(),
            "/v1/community/generation-replication/operator/status",
        )
        .await;
        assert_eq!(status.status(), StatusCode::OK);
        let status = response_json(status).await;
        assert_eq!(status["published_generations"], 0);
        assert_eq!(status["tombstones"], 1);
        assert_eq!(status["pending_retirements"], 0);
        assert_eq!(status["pending_retirement_bytes"], 0);

        let replay = post_json(
            app.clone(),
            "/v1/community/generations",
            serde_json::json!({
                "schema": BEGIN_RUN_GENERATION_SCHEMA,
                "manifest": manifest,
            }),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::CONFLICT);

        let mut cancellable_manifest = manifest.clone();
        cancellable_manifest.generation_id = "wrf-http-cancellable".into();
        let cancellable = post_json(
            app.clone(),
            "/v1/community/generations",
            serde_json::json!({
                "schema": BEGIN_RUN_GENERATION_SCHEMA,
                "manifest": cancellable_manifest,
            }),
        )
        .await;
        assert_eq!(cancellable.status(), StatusCode::CREATED);
        let outsider_cancel = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/v1/community/generations/wrf-http-cancellable")
                    .header(
                        header::AUTHORIZATION,
                        format!("Bearer {RELAY_REQUESTER_TOKEN}"),
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(outsider_cancel.status(), StatusCode::FORBIDDEN);
        let killed = post_json(
            app.clone(),
            "/v1/community/generation-replication/operator/kill-switch",
            serde_json::json!({
                "schema": crate::generation_replication::REPLICATION_KILL_SWITCH_SCHEMA,
                "engaged": true,
            }),
        )
        .await;
        assert_eq!(killed.status(), StatusCode::OK);
        assert_eq!(response_json(killed).await["kill_switch"], true);
        let cancel = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::DELETE)
                    .uri("/v1/community/generations/wrf-http-cancellable")
                    .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cancel.status(), StatusCode::OK);
        assert_eq!(cancel.headers()[header::CACHE_CONTROL], "no-store, private");
        let cancel = response_json(cancel).await;
        assert_eq!(cancel["generation_id"], "wrf-http-cancellable");
        assert_eq!(cancel["released_reserved_bytes"], manifest.total_bytes);
        let killed_capabilities = get_with_token(
            app.clone(),
            rw_community_protocol::RUN_GENERATION_CAPABILITIES_PATH,
        )
        .await;
        assert_eq!(killed_capabilities.status(), StatusCode::OK);
        let killed_capabilities = response_json(killed_capabilities).await;
        assert_eq!(killed_capabilities["accepting_uploads"], false);
        assert_eq!(killed_capabilities["usage"]["active_uploads"], 0);
        assert_eq!(killed_capabilities["usage"]["reserved_bytes"], 0);

        let gc = post_json(
            app,
            "/v1/community/generation-replication/operator/gc",
            serde_json::json!({}),
        )
        .await;
        assert_eq!(gc.status(), StatusCode::OK);
        let gc = response_json(gc).await;
        for field in [
            "expired_uploads",
            "expired_publications",
            "retired_generations",
            "pending_retirements",
            "orphan_chunks",
            "orphan_manifests",
            "stale_candidates",
        ] {
            assert!(gc[field].is_number(), "missing coarse GC field {field}");
        }
    }

    #[tokio::test]
    async fn community_relay_http_flow_is_cold_only_no_store_and_principal_isolated() {
        let (_directory, app, origin_key, relay_verifying_key) = relay_http_test_app();
        let signed_manifest = signed_relay_test_manifest(&origin_key);
        let object_sha256 = signed_manifest.manifest.object_sha256.clone();

        // A current/operational request cannot accidentally enter the cold
        // relay path, even when an exact hash is supplied.
        let operational = post_json_with_token(
            app.clone(),
            "/v1/community/relay/historical/lookups",
            serde_json::json!({
                "schema": "rw.community.relay-historical-lookup.v1",
                "historical": false,
                "object_sha256": object_sha256,
                "opted_in": true,
                "download_allowance_bytes": 1_000_000,
            }),
            RELAY_REQUESTER_TOKEN,
        )
        .await;
        assert_eq!(operational.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let advertisement = post_json_with_token(
            app.clone(),
            "/v1/community/relay/advertisements",
            serde_json::json!({
                "schema": "rw.community.relay-advertise-request.v1",
                "signed_manifest": signed_manifest,
                "opted_in": true,
                "categories": ["point_series"],
                "disk_allowance_bytes": 1_000_000,
                "upload_allowance_bytes": 1_000_000,
                "metered_network": false,
                "allow_metered_seeding": false,
            }),
            TOKEN,
        )
        .await;
        assert_eq!(advertisement.status(), StatusCode::CREATED);
        assert!(
            advertisement.headers()[header::CACHE_CONTROL]
                .to_str()
                .unwrap()
                .contains("no-store")
        );

        let lookup = post_json_with_token(
            app.clone(),
            "/v1/community/relay/historical/lookups",
            serde_json::json!({
                "schema": "rw.community.relay-historical-lookup.v1",
                "historical": true,
                "object_sha256": object_sha256,
                "opted_in": true,
                "download_allowance_bytes": 1_000_000,
            }),
            RELAY_REQUESTER_TOKEN,
        )
        .await;
        assert_eq!(lookup.status(), StatusCode::OK);
        assert_eq!(lookup.headers()[header::CACHE_CONTROL], "no-store, private");
        assert!(lookup.headers().get(header::ETAG).is_none());
        let lookup_body = to_bytes(lookup.into_body(), 256 * 1024).await.unwrap();
        let lookup_wire = parse_historical_lookup_response_bounded(&lookup_body).unwrap();
        let downloader = lookup_wire.participant_grant.unwrap();
        let relay_keys = rw_community_protocol::TrustedSigningKeys::from([(
            "rw-relay-v1".to_string(),
            relay_verifying_key,
        )]);
        downloader
            .validate(
                &object_sha256,
                RelayRole::Downloader,
                chrono::Utc::now().timestamp(),
                &relay_keys,
                &rw_community_protocol::ProtocolLimits::default(),
            )
            .unwrap();
        assert_eq!(downloader.role, RelayRole::Downloader);
        assert_eq!(downloader.object_sha256, object_sha256);
        let session_id = downloader.session_id.clone();
        let downloader_serialized = serde_json::to_string(&downloader).unwrap();
        for forbidden in [
            TOKEN,
            RELAY_REQUESTER_TOKEN,
            RELAY_OUTSIDER_TOKEN,
            "peer_ip",
            "host_candidate",
            "server_reflexive",
        ] {
            assert!(
                !downloader_serialized.contains(forbidden),
                "participant response leaked forbidden marker {forbidden}"
            );
        }

        let uploader = post_json_with_token(
            app.clone(),
            "/v1/community/relay/grants/next",
            serde_json::json!({"schema": "rw.community.relay-grant-poll.v1"}),
            TOKEN,
        )
        .await;
        assert_eq!(uploader.status(), StatusCode::OK);
        assert_eq!(
            uploader.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        let uploader_body = to_bytes(uploader.into_body(), 256 * 1024).await.unwrap();
        let uploader = parse_participant_grant_bounded(
            &uploader_body,
            &object_sha256,
            RelayRole::Uploader,
            chrono::Utc::now().timestamp(),
            &relay_keys,
            &rw_community_protocol::ProtocolLimits::default(),
        )
        .unwrap();
        assert_eq!(uploader.role, RelayRole::Uploader);
        assert_eq!(uploader.session_id, session_id);

        // Neither an unrelated authenticated account nor the counterpart can
        // enumerate or retrieve another participant's grant.
        let outsider_poll = post_json_with_token(
            app.clone(),
            "/v1/community/relay/grants/next",
            serde_json::json!({"schema": "rw.community.relay-grant-poll.v1"}),
            RELAY_OUTSIDER_TOKEN,
        )
        .await;
        assert_eq!(outsider_poll.status(), StatusCode::NOT_FOUND);

        for (token, role) in [
            (TOKEN, "downloader"),
            (RELAY_REQUESTER_TOKEN, "uploader"),
            (RELAY_OUTSIDER_TOKEN, "uploader"),
        ] {
            let response = post_json_with_token(
                app.clone(),
                &format!("/v1/community/relay/sessions/{session_id}/grants/{role}"),
                serde_json::json!({}),
                token,
            )
            .await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }

        // Shared client/server wire types round-trip through the authenticated
        // route endpoint, including the opposite signed role credential. No
        // host/srflx/direct candidate is representable in this exchange.
        let now = chrono::Utc::now().timestamp();
        let uploader_keys = rw_community_relay::EphemeralKeyPair::generate();
        let downloader_keys = rw_community_relay::EphemeralKeyPair::generate();
        let uploader_offer = uploader_keys
            .offer(
                &uploader.credential,
                RelayRole::Uploader,
                now,
                &rw_community_protocol::ProtocolLimits::default(),
            )
            .unwrap();
        let downloader_offer = downloader_keys
            .offer(
                &downloader.credential,
                RelayRole::Downloader,
                now,
                &rw_community_protocol::ProtocolLimits::default(),
            )
            .unwrap();
        for (token, credential, offer, allocation) in [
            (
                TOKEN,
                uploader.credential.clone(),
                uploader_offer,
                "104.16.0.7:49152",
            ),
            (
                RELAY_REQUESTER_TOKEN,
                downloader.credential.clone(),
                downloader_offer,
                "104.16.0.8:49153",
            ),
        ] {
            let registration = rw_community_relay::RelayRouteRegistrationRequest {
                schema: rw_community_relay::RELAY_ROUTE_REGISTRATION_SCHEMA.into(),
                credential,
                offer,
                turn_local_addr: allocation.into(),
            };
            let response = post_json_with_token(
                app.clone(),
                rw_community_relay::RELAY_ROUTE_REGISTRATION_PATH,
                serde_json::to_value(registration).unwrap(),
                token,
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        let transport = post_json_with_token(
            app.clone(),
            rw_community_relay::RELAY_TRANSPORT_GRANT_PATH,
            serde_json::to_value(rw_community_relay::RelayTransportGrantRequest {
                schema: rw_community_relay::RELAY_TRANSPORT_GRANT_REQUEST_SCHEMA.into(),
                role: RelayRole::Downloader,
                credential: downloader.credential.clone(),
            })
            .unwrap(),
            RELAY_REQUESTER_TOKEN,
        )
        .await;
        assert_eq!(transport.status(), StatusCode::OK);
        assert_eq!(
            transport.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        let transport_body = to_bytes(transport.into_body(), 256 * 1024).await.unwrap();
        let route_policy =
            rw_community_relay::RelayRoutePolicy::from_audited_cidrs(["104.16.0.0/24"]).unwrap();
        let (transport_wire, _route, _binding) = rw_community_relay::parse_transport_route_bounded(
            &transport_body,
            rw_community_relay::TransportRouteExpectation {
                session_id: &session_id,
                role: RelayRole::Downloader,
                own_credential: &downloader.credential,
                object_sha256: &object_sha256,
                encoded_size: downloader.encoded_size,
                now_unix: now,
                trusted_relay_keys: &relay_keys,
                limits: &rw_community_protocol::ProtocolLimits::default(),
                policy: &route_policy,
            },
        )
        .unwrap();
        assert_eq!(transport_wire.peer_credential, uploader.credential);
        let transport_text = String::from_utf8(transport_body.to_vec()).unwrap();
        for forbidden in ["peer_ip", "host_candidate", "server_reflexive", TOKEN] {
            assert!(!transport_text.contains(forbidden));
        }

        // One role can never claim success. Only matching exact-byte reports
        // from both authenticated signed roles complete the broker session.
        let downloader_complete = post_json_with_token(
            app.clone(),
            rw_community_relay::RELAY_SESSION_COMPLETE_PATH,
            serde_json::to_value(rw_community_relay::RelaySessionCompletionRequest {
                schema: rw_community_relay::RELAY_SESSION_COMPLETION_SCHEMA.into(),
                role: RelayRole::Downloader,
                credential: downloader.credential,
                transferred_bytes: downloader.encoded_size,
            })
            .unwrap(),
            RELAY_REQUESTER_TOKEN,
        )
        .await;
        assert_eq!(downloader_complete.status(), StatusCode::OK);
        let downloader_terminal: rw_community_relay::RelayTerminalResponse =
            serde_json::from_value(response_json(downloader_complete).await).unwrap();
        assert!(!downloader_terminal.session_complete);
        let uploader_complete = post_json_with_token(
            app,
            rw_community_relay::RELAY_SESSION_COMPLETE_PATH,
            serde_json::to_value(rw_community_relay::RelaySessionCompletionRequest {
                schema: rw_community_relay::RELAY_SESSION_COMPLETION_SCHEMA.into(),
                role: RelayRole::Uploader,
                credential: uploader.credential,
                transferred_bytes: uploader.encoded_size,
            })
            .unwrap(),
            TOKEN,
        )
        .await;
        assert_eq!(uploader_complete.status(), StatusCode::OK);
        let uploader_terminal: rw_community_relay::RelayTerminalResponse =
            serde_json::from_value(response_json(uploader_complete).await).unwrap();
        assert!(uploader_terminal.session_complete);
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
                "/v1/profile-cycle",
                serde_json::json!({
                    "model": LEGACY_RETIRED_MODEL,
                    "run": FIXTURE_RUN,
                    "latitude": 40.0,
                    "longitude": -100.0,
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
            runs[0]["variable_count"], 3,
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
                "/v1/profile-cycle",
                serde_json::json!({
                    "model": FIXTURE_MODEL,
                    "run": FIXTURE_RUN,
                    "latitude": 40.0,
                    "longitude": -100.0,
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
    async fn enabled_missing_origin_catalog_fails_operational_routes_closed() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.origin_catalog.enabled = true;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        // No .rw-origin-catalog.json exists: startup is intentionally pending
        // and must never fall back to a broad store scan.
        fs::create_dir_all(
            config
                .server
                .store_root
                .join("hidden-model")
                .join("hidden-run"),
        )
        .unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

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
        assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response_json(ready).await["code"], "NOT_READY");

        let unauthenticated = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

        let models = get_with_token(app.clone(), "/v1/models").await;
        assert_eq!(models.status(), StatusCode::SERVICE_UNAVAILABLE);
        let problem = response_json(models).await;
        assert_eq!(problem["code"], "ORIGIN_CATALOG_UNAVAILABLE");
        let serialized = serde_json::to_string(&problem).unwrap();
        assert!(!serialized.contains("hidden-model"));
        assert!(!serialized.contains("hidden-run"));

        let profile_cycle = post_json(
            app.clone(),
            "/v1/profile-cycle",
            serde_json::json!({
                "model": "hidden-model",
                "run": "hidden-run",
                "latitude": 40.0,
                "longitude": -100.0,
                "variables": ["temperature"]
            }),
        )
        .await;
        assert_eq!(profile_cycle.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response_json(profile_cycle).await["code"],
            "ORIGIN_CATALOG_UNAVAILABLE"
        );

        let status = get_with_token(app, "/v1/origin-catalog/status").await;
        assert_eq!(status.status(), StatusCode::OK);
        assert_eq!(status.headers()[header::CACHE_CONTROL], "no-store, private");
        let status = response_json(status).await;
        assert_eq!(status["state"], "pending");
        assert_eq!(status["ready"], false);
        let serialized = serde_json::to_string(&status).unwrap();
        assert!(!serialized.contains("hidden-model"));
        assert!(!serialized.contains("hidden-run"));
    }

    #[tokio::test]
    async fn stale_origin_catalog_returns_503_instead_of_an_empty_model_list() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.origin_catalog.enabled = true;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();

        let cycle = CycleSpec::new("20260812", 0).unwrap();
        let mut catalog = OriginCatalogState::empty(&OriginCatalogPlanConfig::default());
        catalog.updated_unix = chrono::Utc::now().timestamp().saturating_sub(7_201);
        catalog.lanes[0] = OriginPublishedLane {
            id: "hrrr-hourly".into(),
            active: Some(OriginPublishedGeneration {
                model: ModelId::Hrrr,
                cycle: cycle.clone(),
                run_id: "20260812_00z".into(),
                coverage_complete: false,
                available_valid_unix: [cycle_origin_unix(&cycle).unwrap()].into(),
            }),
            previous: None,
        };
        OriginCatalogStateStore::new(&config.server.store_root)
            .save(&OriginCatalogPlanConfig::default(), &catalog)
            .unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        config.validate(!tokens.is_empty()).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

        let response = get_with_token(app.clone(), "/v1/models").await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let problem = response_json(response).await;
        assert_eq!(problem["code"], "ORIGIN_CATALOG_UNAVAILABLE");
        assert!(
            !serde_json::to_string(&problem)
                .unwrap()
                .contains("20260812")
        );

        let status = response_json(get_with_token(app, "/v1/origin-catalog/status").await).await;
        assert_eq!(status["state"], "unavailable");
        assert_eq!(status["published_runs"], 0);
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
        config.server.cache_root = directory.path().join("cache");
        config.limits.request_body_bytes = 64;
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let tokens = crate::TokenSet::from_tokens([TOKEN]).unwrap();
        let app = build_router(AppState::new(config, tokens).unwrap()).unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/profile-cycle")
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

        for id in ["hrrr", "gefs", "aigfs", "aigefs", "hgefs"] {
            let model = models
                .as_array()
                .unwrap()
                .iter()
                .find(|model| model["id"] == id)
                .unwrap_or_else(|| panic!("missing built-in {id} capability"));
            let noaa = model["provider_attributions"]
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
    }

    #[tokio::test]
    async fn model_catalog_exposes_cma_statistics_scope_and_wmo_policy() {
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
        let model = models
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "cma-geps")
            .expect("CMA GEPS capability");
        assert_eq!(model["verification"], "live_verified");
        assert_eq!(
            model["limitations"],
            serde_json::json!([
                "provider_statistics_only",
                "sparse_pressure_levels",
                "derived_products_disabled"
            ])
        );
        let attribution = &model["provider_attributions"][0];
        assert_eq!(
            attribution["provider"],
            "China Meteorological Administration (CMA)"
        );
        assert!(
            attribution["license"]
                .as_str()
                .unwrap()
                .contains("WMO Unified Data Policy")
        );
        assert!(
            attribution["modification_notice"]
                .as_str()
                .unwrap()
                .contains("not an official CMA product")
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
                "forecast_producer": "ecmwf",
                "licensing_publisher": "ecmwf",
                "transport_provider": "ecmwf-open-data",
                "transport_is_mirror": false,
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
    async fn profile_cycle_shares_one_decoded_level_value_budget_across_times() {
        let (_directory, app) = test_app_with_store_limit(3);
        let response = post_json(
            app,
            "/v1/profile-cycle",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "latitude": 40.0,
                "longitude": -100.0,
                "variables": ["temperature_iso"],
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let problem = response_json(response).await;
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
    async fn geographic_window_is_authenticated_snapshot_bound_and_self_describing() {
        let (_directory, state) = test_state_with_store_limit(2_000_000);
        let descriptor = state
            .catalog
            .snapshot(FIXTURE_MODEL, FIXTURE_RUN)
            .unwrap()
            .descriptor()
            .clone();
        let app = build_router(state).unwrap();
        let request = serde_json::json!({
            "model": FIXTURE_MODEL,
            "run": FIXTURE_RUN,
            "expected_snapshot_id": descriptor.snapshot_id,
            "expected_grid_hash": descriptor.grid_hash,
            "storage_slot": 0,
            "variables": ["temperature_iso"],
            "west_longitude": -100.1,
            "south_latitude": 39.9,
            "east_longitude": -98.9,
            "north_latitude": 41.1,
            "vertical": {"kind": "pressure_levels", "levels_hpa": [500, 850]}
        });
        let denied = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/v1/geographic-window")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(serde_json::to_vec(&request).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let response = post_json(app.clone(), "/v1/geographic-window", request.clone()).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(header::ETAG));
        let body = response_json(response).await;
        assert_eq!(body["schema"], "rw.query.geographic-window.v1");
        assert_eq!(body["run"]["snapshot_id"], descriptor.snapshot_id);
        assert_eq!(body["run"]["grid_hash"], descriptor.grid_hash);
        assert_eq!(
            body["envelope"],
            serde_json::json!({"x0": 0, "y0": 0, "nx": 2, "ny": 2})
        );
        assert_eq!(body["latitudes"].as_array().unwrap().len(), 4);
        assert_eq!(body["longitudes"].as_array().unwrap().len(), 4);
        assert_eq!(
            body["cell_mask"],
            serde_json::json!([true, true, true, true])
        );
        assert_eq!(body["fields"][0]["data"]["kind"], "pressure_levels");
        assert_eq!(
            body["fields"][0]["data"]["levels_hpa"],
            serde_json::json!([500, 850])
        );
        assert_eq!(
            body["fields"][0]["data"]["values"]
                .as_array()
                .unwrap()
                .len(),
            8
        );

        let mut stale = request;
        stale["expected_snapshot_id"] = serde_json::Value::String("0".repeat(64));
        let rejected = post_json(app, "/v1/geographic-window", stale).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
    }

    /// Decode the `RWOBF32` plane container the model plane route shares with
    /// the observation plane route.
    fn decode_plane_blob(bytes: &[u8]) -> (usize, usize, i64, String, String, Vec<f32>) {
        assert_eq!(&bytes[0..8], b"RWOBF32\0", "plane magic");
        assert_eq!(u32::from_le_bytes(bytes[8..12].try_into().unwrap()), 1);
        let nx = u32::from_le_bytes(bytes[12..16].try_into().unwrap()) as usize;
        let ny = u32::from_le_bytes(bytes[16..20].try_into().unwrap()) as usize;
        let valid_unix = i64::from_le_bytes(bytes[20..28].try_into().unwrap());
        let variable_len = u16::from_le_bytes(bytes[28..30].try_into().unwrap()) as usize;
        let unit_len = u16::from_le_bytes(bytes[30..32].try_into().unwrap()) as usize;
        assert_eq!(u32::from_le_bytes(bytes[32..36].try_into().unwrap()), 0);
        let variable = String::from_utf8(bytes[36..36 + variable_len].to_vec()).unwrap();
        let units =
            String::from_utf8(bytes[36 + variable_len..36 + variable_len + unit_len].to_vec())
                .unwrap();
        let values_offset = 36 + variable_len + unit_len;
        let values = bytes[values_offset..]
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<_>>();
        assert_eq!(values.len(), nx * ny, "plane carries exactly nx*ny values");
        (nx, ny, valid_unix, variable, units, values)
    }

    fn model_plane_uri(
        descriptor: &rw_query::RunDescriptor,
        storage_slot: u16,
        variable: &str,
        level_hpa: Option<u16>,
    ) -> String {
        let mut uri = format!(
            "/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/planes/{storage_slot}/{variable}\
             ?expected_snapshot_id={}&expected_grid_hash={}",
            descriptor.snapshot_id, descriptor.grid_hash
        );
        if let Some(level_hpa) = level_hpa {
            uri.push_str(&format!("&level_hpa={level_hpa}"));
        }
        uri
    }

    fn fixture_descriptor(state: &AppState) -> rw_query::RunDescriptor {
        state
            .catalog
            .snapshot(FIXTURE_MODEL, FIXTURE_RUN)
            .unwrap()
            .descriptor()
            .clone()
    }

    #[tokio::test]
    async fn model_surface_plane_is_authenticated_snapshot_bound_and_immutable() {
        let (_directory, state) = test_state_with_store_limit(2_000_000);
        let descriptor = fixture_descriptor(&state);
        let app = build_router(state).unwrap();
        let uri = model_plane_uri(&descriptor, 0, "scalar.bin", None);

        let denied = app
            .clone()
            .oneshot(Request::builder().uri(&uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let response = get_with_token(app.clone(), &uri).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "application/vnd.rusty-weather.model-plane+f32"
        );
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(
            etag.starts_with(&format!("\"{}-0-", descriptor.snapshot_id)),
            "ETag must bind the immutable snapshot and slot: {etag}"
        );
        assert!(etag.ends_with("-surface\""), "ETag names the level: {etag}");
        assert_eq!(response.headers()["x-rw-model-variable"], "scalar");
        assert_eq!(response.headers()["x-rw-model-units"], "K");
        assert_eq!(response.headers()["x-rw-model-codec"], "zstd1_f32");
        assert_eq!(
            response.headers()["x-rw-valid-unix"],
            FIXTURE_ORIGIN.to_string()
        );
        assert_eq!(response.headers()["x-rw-nodata"], "non-finite-transparent");
        assert!(
            !response.headers().contains_key("x-rw-model-level-hpa"),
            "a surface plane must not claim a pressure level"
        );

        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let (nx, ny, valid_unix, variable, units, values) = decode_plane_blob(&body);
        assert_eq!((nx, ny), (descriptor.nx, descriptor.ny));
        assert_eq!(valid_unix, FIXTURE_ORIGIN);
        assert_eq!(variable, "scalar");
        assert_eq!(units, "K");
        assert_eq!(values, vec![1.0, 2.0, 3.0, 4.0]);

        // The second slot is a distinct immutable resource with its own ETag.
        let later = get_with_token(app, &model_plane_uri(&descriptor, 1, "scalar.bin", None)).await;
        assert_eq!(later.status(), StatusCode::OK);
        assert_ne!(later.headers()[header::ETAG], etag.as_str());
        assert_eq!(
            later.headers()["x-rw-valid-unix"],
            (FIXTURE_ORIGIN + 900).to_string()
        );
        let body = to_bytes(later.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(decode_plane_blob(&body).5, vec![2.0, 3.0, 4.0, 5.0]);
    }

    /// A run name is not an immutable identity: publishers replace runs
    /// atomically under the same name. The plane URL therefore has to name the
    /// snapshot it means, exactly like `/v1/geographic-window`, or the
    /// `immutable` cache directive above would be a lie.
    #[tokio::test]
    async fn model_plane_requires_and_enforces_the_snapshot_identity_guard() {
        let (_directory, state) = test_state_with_store_limit(2_000_000);
        let descriptor = fixture_descriptor(&state);
        let app = build_router(state).unwrap();

        for uri in [
            format!("/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/planes/0/scalar.bin"),
            format!(
                "/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/planes/0/scalar.bin?expected_snapshot_id={}",
                descriptor.snapshot_id
            ),
            format!(
                "/v1/models/{FIXTURE_MODEL}/runs/{FIXTURE_RUN}/planes/0/scalar.bin?expected_grid_hash={}",
                descriptor.grid_hash
            ),
        ] {
            let response = get_with_token(app.clone(), &uri).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "missing identity guard must fail explicitly: {uri}"
            );
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "application/problem+json"
            );
        }

        let mut stale = descriptor.clone();
        stale.snapshot_id = "0".repeat(64);
        let rejected =
            get_with_token(app.clone(), &model_plane_uri(&stale, 0, "scalar.bin", None)).await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        let mut wrong_grid = descriptor.clone();
        wrong_grid.grid_hash = "1".repeat(64);
        let rejected = get_with_token(
            app.clone(),
            &model_plane_uri(&wrong_grid, 0, "scalar.bin", None),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);

        // Unknown query parameters are refused rather than silently ignored.
        let unknown = get_with_token(
            app,
            &format!(
                "{}&unsupported=1",
                model_plane_uri(&descriptor, 0, "scalar.bin", None)
            ),
        )
        .await;
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn model_pressure_plane_serves_one_exact_level_and_says_it_is_quantized() {
        let (_directory, state) = test_state_with_store_limit(2_000_000);
        let descriptor = fixture_descriptor(&state);
        let app = build_router(state).unwrap();

        let response = get_with_token(
            app.clone(),
            &model_plane_uri(&descriptor, 1, "temperature_iso.bin", Some(850)),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-rw-model-level-hpa"], "850");
        assert_eq!(
            response.headers()["x-rw-model-codec"],
            "zstd1_affine_i16",
            "pressure planes are dequantized, and the response must say so"
        );
        let etag = response.headers()[header::ETAG]
            .to_str()
            .unwrap()
            .to_owned();
        assert!(etag.ends_with("-850hpa\""), "ETag names the level: {etag}");
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let (_, _, valid_unix, variable, units, values) = decode_plane_blob(&body);
        assert_eq!(valid_unix, FIXTURE_ORIGIN + 900);
        assert_eq!(variable, "temperature_iso");
        assert_eq!(units, "K");
        for value in &values {
            assert!(
                (value - 281.0).abs() < 0.01,
                "{value} is not the stored 850 hPa level"
            );
        }

        let upper = get_with_token(
            app.clone(),
            &model_plane_uri(&descriptor, 1, "temperature_iso.bin", Some(500)),
        )
        .await;
        assert_eq!(upper.status(), StatusCode::OK);
        assert_ne!(upper.headers()[header::ETAG], etag.as_str());
        let body = to_bytes(upper.into_body(), 1024 * 1024).await.unwrap();
        for value in &decode_plane_blob(&body).5 {
            assert!(
                (value - 251.0).abs() < 0.01,
                "{value} is not the 500 hPa level"
            );
        }

        // A level this run does not store is missing data, not a server fault.
        let absent = get_with_token(
            app.clone(),
            &model_plane_uri(&descriptor, 1, "temperature_iso.bin", Some(700)),
        )
        .await;
        assert_eq!(absent.status(), StatusCode::NOT_FOUND);

        // Kind mismatches stay explicit in both directions.
        let surface_with_level = get_with_token(
            app.clone(),
            &model_plane_uri(&descriptor, 0, "scalar.bin", Some(850)),
        )
        .await;
        assert_eq!(
            surface_with_level.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let pressure_without_level = get_with_token(
            app,
            &model_plane_uri(&descriptor, 1, "temperature_iso.bin", None),
        )
        .await;
        assert_eq!(
            pressure_without_level.status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    #[tokio::test]
    async fn model_plane_failures_are_explicit_and_never_leak_retired_identities() {
        let (_directory, state) = test_state_with_store_limit(2_000_000);
        let descriptor = fixture_descriptor(&state);
        let app = build_router(state).unwrap();

        for (label, uri) in [
            (
                "unknown slot",
                model_plane_uri(&descriptor, 9, "scalar.bin", None),
            ),
            (
                "unknown variable",
                model_plane_uri(&descriptor, 0, "absent.bin", None),
            ),
            (
                "missing .bin suffix",
                model_plane_uri(&descriptor, 0, "scalar", None),
            ),
            (
                "bare .bin filename",
                model_plane_uri(&descriptor, 0, ".bin", None),
            ),
            (
                "retired variable",
                model_plane_uri(&descriptor, 0, "fire_weather_composite.bin", None),
            ),
        ] {
            let response = get_with_token(app.clone(), &uri).await;
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{label} must be an explicit 404"
            );
        }

        let retired_model = get_with_token(
            app,
            &format!(
                "/v1/models/{LEGACY_RETIRED_MODEL}/runs/{FIXTURE_RUN}/planes/0/scalar.bin\
                 ?expected_snapshot_id={}&expected_grid_hash={}",
                descriptor.snapshot_id, descriptor.grid_hash
            ),
        )
        .await;
        assert_eq!(retired_model.status(), StatusCode::NOT_FOUND);
        let body = to_bytes(retired_model.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(
            !text.contains(LEGACY_RETIRED_MODEL),
            "the retired model name must never be echoed: {text}"
        );
    }

    /// The plane route exists so a whole forecast field can be scrubbed. It
    /// must therefore return the complete native grid, and no synchronous
    /// query budget may quietly truncate it the way the JSON window path caps
    /// cells.
    #[tokio::test]
    async fn model_plane_returns_the_full_native_grid_under_the_tightest_query_budget() {
        let (_directory, state) = test_state_with_store_limit(1);
        let descriptor = fixture_descriptor(&state);
        let cells = descriptor.nx * descriptor.ny;
        let app = build_router(state).unwrap();

        let response =
            get_with_token(app, &model_plane_uri(&descriptor, 0, "scalar.bin", None)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let (nx, ny, _, _, _, values) = decode_plane_blob(&body);
        assert_eq!((nx, ny), (descriptor.nx, descriptor.ny));
        assert_eq!(
            values.len(),
            cells,
            "the plane route must not inherit sync_result_values as a hidden cell ceiling"
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
            variable["name"] == "temperature_iso"
                && variable["pressure_profile"] == true
                && variable["profile_cycle"] == true
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

        let profile_cycle = post_json(
            app.clone(),
            "/v1/profile-cycle",
            serde_json::json!({
                "model": FIXTURE_MODEL,
                "run": FIXTURE_RUN,
                "latitude": 40.5,
                "longitude": -99.5,
                "variables": ["optional_pressure_iso"],
                "surface_variables": ["scalar"],
                "missing_policy": "partial"
            }),
        )
        .await;
        assert_eq!(profile_cycle.status(), StatusCode::OK);
        assert!(profile_cycle.headers().contains_key(header::ETAG));
        let profile_cycle = response_json(profile_cycle).await;
        assert_eq!(profile_cycle["run"]["run"], FIXTURE_RUN);
        assert_eq!(profile_cycle["run"]["sample_count"], 2);
        assert_eq!(
            profile_cycle["point"]["requested_latitude"],
            serde_json::json!(40.5)
        );
        assert_eq!(
            profile_cycle["requested_variables"],
            serde_json::json!(["optional_pressure_iso"])
        );
        assert_eq!(
            profile_cycle["requested_surface_variables"],
            serde_json::json!(["scalar"])
        );
        assert_eq!(profile_cycle["missing_policy"], "partial");
        assert_eq!(profile_cycle["samples"].as_array().unwrap().len(), 2);
        assert_eq!(profile_cycle["samples"][0]["time"]["storage_slot"], 0);
        assert_eq!(profile_cycle["samples"][0]["status"], "partial");
        assert_eq!(
            profile_cycle["samples"][0]["missing_variables"],
            serde_json::json!(["optional_pressure_iso"])
        );
        assert_eq!(
            profile_cycle["samples"][0]["source_provenance"][0]["provider"],
            "ecmwf-open-data"
        );
        assert_eq!(
            profile_cycle["samples"][0]["surface_samples"][0],
            serde_json::json!({"variable": "scalar", "units": "K", "value": 4.0})
        );
        assert_eq!(
            profile_cycle["samples"][0]["missing_surface_variables"],
            serde_json::json!([])
        );
        assert_eq!(profile_cycle["samples"][1]["time"]["storage_slot"], 1);
        assert_eq!(profile_cycle["samples"][1]["status"], "complete");
        assert_eq!(
            profile_cycle["samples"][1]["variables"][0]["levels_hpa"],
            serde_json::json!([850, 500])
        );
        assert_eq!(
            profile_cycle["samples"][1]["variables"][0]["values"],
            serde_json::json!([271.0, 241.0])
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
        assert!(metrics.contains("rw_response_cache_misses_total 5"));
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

#![allow(dead_code)] // Utoipa consumes the document-only handler stubs via its derive macro.

use utoipa::openapi::OpenApi as OpenApiDocument;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::problem::ProblemDetails;
use crate::routes::{
    ApiIngestCapabilityLimitation, ApiIntervalSupport, ApiMissingPolicy,
    ApiTemporalCapabilityBasis, ApiTemporalOperation, ApiTemporalReducer, ApiTemporalSemantics,
    ApiTemporalValueClass, ApiTemporalVerticalSelection, ApiTemporalWindow, ApiTimeExpectation,
    CoordinateRequest, HealthResponse, ModelCapabilityResponse, PointQueryRequest, PointsRequest,
    ProductCapabilityResponse, ProfileApiRequest, ProviderAttributionResponse,
    SpatialSeriesApiRequest, TemporalGridApiRequest, VariableCapabilityResponse,
    VariableTemporalCapabilityResponse, VersionResponse, WindowApiRequest,
};
use crate::{ArtifactRef, JobStatus, JobView};

/// Sanitized union of resolved providers represented in a run.
#[derive(utoipa::ToSchema)]
struct SourceProvenanceResponse {
    provider: String,
    roles: Vec<String>,
    products: Vec<String>,
}

/// Public run identity and exact stored time-axis summary.
#[derive(utoipa::ToSchema)]
struct RunDescriptorResponse {
    model: String,
    run: String,
    schema: String,
    snapshot_id: String,
    grid_hash: String,
    nx: usize,
    ny: usize,
    exact_time_axis: bool,
    origin_unix: Option<i64>,
    sample_count: usize,
    first_valid_unix: Option<i64>,
    last_valid_unix: Option<i64>,
    source_provenance: Vec<SourceProvenanceResponse>,
    provider_attributions: Vec<ProviderAttributionResponse>,
}

#[derive(utoipa::ToSchema)]
struct RunCatalogEntryResponse {
    run: RunDescriptorResponse,
    variable_count: usize,
}

/// One exact stored valid time. Internal store filenames are intentionally not exposed.
#[derive(utoipa::ToSchema)]
struct TimePointResponse {
    storage_slot: u16,
    lead_seconds: u64,
    valid_unix: i64,
}

#[derive(utoipa::ToSchema)]
struct GridPointResponse {
    requested_latitude: f64,
    requested_longitude: f64,
    x: usize,
    y: usize,
    grid_latitude: f32,
    grid_longitude: f32,
}

#[derive(utoipa::ToSchema)]
struct PointVariableSeriesResponse {
    name: String,
    units: String,
    values: Vec<Option<f32>>,
    available_samples: usize,
    expected_samples: usize,
    coverage: f64,
}

#[derive(utoipa::ToSchema)]
struct PointSeriesResponse {
    run: RunDescriptorResponse,
    point: GridPointResponse,
    axis: Vec<TimePointResponse>,
    variables: Vec<PointVariableSeriesResponse>,
}

#[derive(utoipa::ToSchema)]
struct PressureProfileResponse {
    name: String,
    units: String,
    levels_hpa: Vec<u16>,
    values: Vec<Option<f32>>,
    available_levels: usize,
    expected_levels: usize,
    coverage: f64,
}

#[derive(utoipa::ToSchema)]
struct ProfileResponse {
    run: RunDescriptorResponse,
    point: GridPointResponse,
    time: TimePointResponse,
    variables: Vec<PressureProfileResponse>,
}

#[derive(utoipa::ToSchema)]
struct IndexWindowResponse {
    run: RunDescriptorResponse,
    time: TimePointResponse,
    variable: String,
    units: String,
    x0: usize,
    y0: usize,
    nx: usize,
    ny: usize,
    values: Vec<Option<f32>>,
}

#[derive(utoipa::ToSchema)]
struct SpatialStatsSampleResponse {
    time: TimePointResponse,
    variable_available: bool,
    minimum: Option<f32>,
    maximum: Option<f32>,
    finite_count: u64,
    missing_count: u64,
}

#[derive(utoipa::ToSchema)]
struct SpatialSeriesResponse {
    run: RunDescriptorResponse,
    variable: String,
    units: String,
    samples: Vec<SpatialStatsSampleResponse>,
    expected_samples: usize,
    available_samples: usize,
    coverage: f64,
}

#[derive(utoipa::ToSchema)]
struct ResolvedTemporalWindowResponse {
    start_unix: i64,
    end_unix: i64,
    duration_seconds: u64,
    requested_local_date: Option<String>,
    timezone: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct TemporalCompletenessResponse {
    expectation: ApiTimeExpectation,
    expected_samples: usize,
    available_samples: usize,
    missing_samples: usize,
    missing_valid_unix: Vec<i64>,
    expected_duration_seconds: u64,
    covered_duration_seconds: u64,
    duration_coverage: f64,
    largest_gap_seconds: u64,
}

#[derive(utoipa::ToSchema)]
struct TemporalGridMetadataResponse {
    run: RunDescriptorResponse,
    variables: Vec<String>,
    units: Vec<String>,
    semantics: ApiTemporalSemantics,
    reducer: ApiTemporalReducer,
    nx: usize,
    ny: usize,
    levels_hpa: Option<Vec<u16>>,
    layout: Option<TemporalGridLayoutResponse>,
    shape: Option<[usize; 3]>,
    axis: Vec<TimePointResponse>,
    window: ResolvedTemporalWindowResponse,
    completeness: TemporalCompletenessResponse,
}

#[derive(utoipa::ToSchema)]
enum TemporalGridLayoutResponse {
    #[serde(rename = "level_y_x")]
    LevelYX,
}

#[derive(utoipa::ToSchema)]
struct ScalarSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    minimum: Vec<Option<f64>>,
    maximum: Vec<Option<f64>>,
    range: Vec<Option<f64>>,
    time_weighted_mean: Vec<Option<f64>>,
    argmin_time_index: Vec<Option<u32>>,
    argmax_time_index: Vec<Option<u32>>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct IntervalSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    total: Vec<Option<f64>>,
    minimum_interval: Vec<Option<f64>>,
    maximum_interval: Vec<Option<f64>>,
    range_interval: Vec<Option<f64>>,
    argmin_time_index: Vec<Option<u32>>,
    argmax_time_index: Vec<Option<u32>>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct IntervalMaximumSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    minimum_of_interval_maxima: Vec<Option<f64>>,
    maximum_of_interval_maxima: Vec<Option<f64>>,
    range_of_interval_maxima: Vec<Option<f64>>,
    argmin_interval_maximum_time_index: Vec<Option<u32>>,
    argmax_interval_maximum_time_index: Vec<Option<u32>>,
    finite_interval_maximum_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct CumulativeSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    total_increment: Vec<Option<f64>>,
    minimum_increment: Vec<Option<f64>>,
    maximum_increment: Vec<Option<f64>>,
    range_increment: Vec<Option<f64>>,
    argmin_time_index: Vec<Option<u32>>,
    argmax_time_index: Vec<Option<u32>>,
    finite_increment_count: Vec<u32>,
    reset_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct RateSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    integral_units: String,
    minimum_rate: Vec<Option<f64>>,
    maximum_rate: Vec<Option<f64>>,
    range_rate: Vec<Option<f64>>,
    duration_weighted_mean: Vec<Option<f64>>,
    integral: Vec<Option<f64>>,
    argmin_time_index: Vec<Option<u32>>,
    argmax_time_index: Vec<Option<u32>>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct VectorSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    minimum_speed: Vec<Option<f64>>,
    maximum_speed: Vec<Option<f64>>,
    range_speed: Vec<Option<f64>>,
    time_weighted_mean_speed: Vec<Option<f64>>,
    vector_mean_u: Vec<Option<f64>>,
    vector_mean_v: Vec<Option<f64>>,
    vector_mean_speed: Vec<Option<f64>>,
    vector_mean_direction_toward_degrees: Vec<Option<f64>>,
    argmin_time_index: Vec<Option<u32>>,
    argmax_time_index: Vec<Option<u32>>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct CircularMeanGridResponse {
    metadata: TemporalGridMetadataResponse,
    mean_degrees: Vec<Option<f64>>,
    resultant_length: Vec<Option<f64>>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

#[derive(utoipa::ToSchema)]
struct CategoryDurationResponse {
    category: i32,
    duration_seconds: u64,
}

#[derive(utoipa::ToSchema)]
struct CategoricalSummaryGridResponse {
    metadata: TemporalGridMetadataResponse,
    mode: Vec<Option<i32>>,
    mode_duration_seconds: Vec<u64>,
    category_durations: Vec<Vec<CategoryDurationResponse>>,
    transitions: Vec<u32>,
    finite_count: Vec<u32>,
    covered_duration_seconds: Vec<u64>,
    duration_coverage: Vec<f64>,
}

/// Exact tagged union emitted by the temporal-grid reducer and job artifacts.
#[derive(utoipa::ToSchema)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
enum TemporalGridResponse {
    Scalar(ScalarSummaryGridResponse),
    Interval(IntervalSummaryGridResponse),
    IntervalMaximum(IntervalMaximumSummaryGridResponse),
    Cumulative(CumulativeSummaryGridResponse),
    Rate(RateSummaryGridResponse),
    Vector(VectorSummaryGridResponse),
    Circular(CircularMeanGridResponse),
    Categorical(CategoricalSummaryGridResponse),
}

// Community Cache uses a separately versioned canonical protocol crate. Keep
// the OpenAPI boundary self-describing without duplicating that crate's many
// closed tagged unions here; the runtime still deserializes the exact DTOs.
#[derive(utoipa::ToSchema)]
struct CommunityResolveRequestDoc {
    schema: String,
    request: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct CommunityResolveResponseDoc {
    schema: String,
    request_sha256: String,
    signed_manifest: Option<serde_json::Value>,
    delivery_order: Vec<String>,
}

#[derive(utoipa::ToSchema)]
struct SignedCommunityObjectManifestDoc {
    manifest: serde_json::Value,
    signature: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct CommunityCaseManifestDoc {
    schema: String,
    case_id: String,
    title: String,
    event_start_unix: i64,
    event_end_unix: i64,
    retain_until_unix: i64,
    artifacts: Vec<serde_json::Value>,
}

#[derive(utoipa::ToSchema)]
struct SignedCommunityCaseManifestDoc {
    manifest: CommunityCaseManifestDoc,
    signature: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct CommunityCaseArtifactPublicationDoc {
    schema: String,
    owner_principal_sha256: String,
    request: serde_json::Value,
    payload: serde_json::Value,
    published_unix: i64,
    retain_until_unix: i64,
    attributions: Vec<serde_json::Value>,
    modification_notices: Vec<String>,
}

#[derive(utoipa::ToSchema)]
struct CommunityRevocationDoc {
    schema: String,
    rights_withdrawn: bool,
    reason: String,
}

#[derive(utoipa::ToSchema)]
struct CommunityPublicationTombstoneDoc {
    schema: String,
    owner_principal_sha256: String,
    request_sha256: String,
    object_sha256: String,
    revoked_unix: i64,
    rights_withdrawn: bool,
    reason: String,
}

/// Runtime parsing uses the exact closed DTOs in `rw-community-protocol`.
/// These document wrappers keep the HTTP surface legible without maintaining
/// a second canonical-signing implementation in the server crate.
#[derive(utoipa::ToSchema)]
struct FederationOriginDescriptorDoc {
    schema: String,
    origin_id: String,
    display_name: String,
    https_base_url: String,
    health_path: String,
    descriptor_signing_keys: Vec<serde_json::Value>,
    object_signing_keys: Vec<serde_json::Value>,
    models: Vec<serde_json::Value>,
    geographic_coverage: Vec<serde_json::Value>,
    retention: serde_json::Value,
    api_schema_version: String,
    build_version: String,
    issued_unix: i64,
    expires_unix: i64,
    policy_links: serde_json::Value,
    replication: serde_json::Value,
    quotas: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct SignedFederationOriginDescriptorDoc {
    descriptor: FederationOriginDescriptorDoc,
    signature: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct FederationCatalogDoc {
    schema: String,
    catalog_id: String,
    generated_unix: i64,
    expires_unix: i64,
    origins: Vec<SignedFederationOriginDescriptorDoc>,
}

#[derive(utoipa::ToSchema)]
struct SignedFederationCatalogDoc {
    catalog: FederationCatalogDoc,
    signature: serde_json::Value,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Rusty Weather API",
        version = "1.0.0",
        description = "Self-hosted model catalog, point/profile queries, and exact-time temporal analytics."
    ),
    paths(
        live_doc,
        ready_doc,
        version_doc,
        openapi_doc,
        models_doc,
        runs_doc,
        run_doc,
        variables_doc,
        point_doc,
        points_doc,
        profile_doc,
        window_doc,
        spatial_series_doc,
        temporal_grid_doc,
        submit_temporal_grid_job_doc,
        job_doc,
        cancel_job_doc,
        artifact_doc,
        community_resolve_doc,
        community_object_doc,
        community_publish_artifact_doc,
        community_revoke_artifact_doc,
        community_create_case_doc,
        community_case_doc,
        community_revoke_case_doc,
        federation_catalog_doc,
        federation_origin_doc,
        metrics_doc,
    ),
    components(schemas(
        HealthResponse,
        VersionResponse,
        ProductCapabilityResponse,
        ProviderAttributionResponse,
        ModelCapabilityResponse,
        ApiIngestCapabilityLimitation,
        ApiMissingPolicy,
        CoordinateRequest,
        PointQueryRequest,
        PointsRequest,
        ProfileApiRequest,
        WindowApiRequest,
        SpatialSeriesApiRequest,
        TemporalGridApiRequest,
        ApiTemporalWindow,
        ApiTimeExpectation,
        ApiIntervalSupport,
        ApiTemporalSemantics,
        ApiTemporalReducer,
        ApiTemporalVerticalSelection,
        ApiTemporalValueClass,
        ApiTemporalCapabilityBasis,
        ApiTemporalOperation,
        VariableTemporalCapabilityResponse,
        VariableCapabilityResponse,
        ProblemDetails,
        ArtifactRef,
        JobStatus,
        JobView,
        SourceProvenanceResponse,
        RunDescriptorResponse,
        RunCatalogEntryResponse,
        TimePointResponse,
        GridPointResponse,
        PointVariableSeriesResponse,
        PointSeriesResponse,
        PressureProfileResponse,
        ProfileResponse,
        IndexWindowResponse,
        SpatialStatsSampleResponse,
        SpatialSeriesResponse,
        ResolvedTemporalWindowResponse,
        TemporalCompletenessResponse,
        TemporalGridMetadataResponse,
        TemporalGridLayoutResponse,
        ScalarSummaryGridResponse,
        IntervalSummaryGridResponse,
        IntervalMaximumSummaryGridResponse,
        CumulativeSummaryGridResponse,
        RateSummaryGridResponse,
        VectorSummaryGridResponse,
        CircularMeanGridResponse,
        CategoryDurationResponse,
        CategoricalSummaryGridResponse,
        TemporalGridResponse,
        CommunityResolveRequestDoc,
        CommunityResolveResponseDoc,
        SignedCommunityObjectManifestDoc,
        CommunityCaseManifestDoc,
        SignedCommunityCaseManifestDoc,
        CommunityCaseArtifactPublicationDoc,
        CommunityRevocationDoc,
        CommunityPublicationTombstoneDoc,
        FederationOriginDescriptorDoc,
        SignedFederationOriginDescriptorDoc,
        FederationCatalogDoc,
        SignedFederationCatalogDoc,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "health", description = "Process and store readiness"),
        (name = "catalog", description = "Models, runs, and variables"),
        (name = "query", description = "Bounded synchronous queries"),
        (name = "analytics", description = "Exact-time and diurnal analytics"),
        (name = "jobs", description = "Bounded asynchronous work and immutable artifacts"),
        (name = "community", description = "Opt-in signed Community Cache objects and deliberate case publication"),
        (name = "federation", description = "Operator-approved signed discovery for deliberately public institutional HTTPS origins"),
        (name = "operations", description = "Operator metrics")
    )
)]
pub struct ApiDoc;

pub fn document() -> OpenApiDocument {
    ApiDoc::openapi()
}

struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut OpenApiDocument) {
        if let Some(components) = openapi.components.as_mut() {
            components.add_security_scheme(
                "bearer_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("opaque")
                        .description(Some("Token supplied by RW_API_TOKENS or RW_API_TOKEN_FILE"))
                        .build(),
                ),
            );
        }
    }
}

#[utoipa::path(
    get,
    path = "/v1/health/live",
    tag = "health",
    responses(
        (status = 200, description = "Process is live", body = HealthResponse),
        (status = 500, description = "Unexpected server failure", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn live_doc() {}

#[utoipa::path(
    get,
    path = "/v1/health/ready",
    tag = "health",
    responses(
        (status = 200, description = "Store and query executor are ready", body = HealthResponse),
        (status = 500, description = "Unexpected readiness failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Store is unreadable, service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Readiness check exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn ready_doc() {}

#[utoipa::path(
    get,
    path = "/v1/version",
    tag = "health",
    responses(
        (status = 200, description = "Service build identity", body = VersionResponse),
        (status = 500, description = "Unexpected server failure", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn version_doc() {}

#[utoipa::path(
    get,
    path = "/v1/openapi.json",
    tag = "health",
    responses(
        (status = 200, description = "This OpenAPI 3 document", body = serde_json::Value),
        (status = 500, description = "Document serialization failed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn openapi_doc() {}

#[utoipa::path(
    get,
    path = "/v1/models",
    tag = "catalog",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Known ingest capabilities merged with stored run counts", body = [ModelCapabilityResponse]),
        (status = 400, description = "Stored catalog contains an invalid entry", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Configured catalog limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Catalog I/O or metadata failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Catalog work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn models_doc() {}

#[utoipa::path(
    get,
    path = "/v1/models/{model}/runs",
    tag = "catalog",
    security(("bearer_auth" = [])),
    params(("model" = String, Path, description = "Canonical stored model id")),
    responses(
        (status = 200, description = "Immutable run snapshots available for the model", body = [RunCatalogEntryResponse]),
        (status = 400, description = "Model id or stored catalog entry is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "A run changed while its snapshot was resolved", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Configured catalog limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Model directory, run metadata, or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Catalog work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn runs_doc() {}

#[utoipa::path(
    get,
    path = "/v1/models/{model}/runs/{run}",
    tag = "catalog",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Canonical stored model id"),
        ("run" = String, Path, description = "Explicit immutable run id")
    ),
    responses(
        (status = 200, description = "Resolved immutable run descriptor", body = RunDescriptorResponse),
        (status = 400, description = "Model id, run id, or run metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while its snapshot was resolved", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Configured snapshot limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Snapshot resolution exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn run_doc() {}

#[utoipa::path(
    get,
    path = "/v1/models/{model}/runs/{run}/variables",
    tag = "catalog",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Canonical stored model id"),
        ("run" = String, Path, description = "Explicit immutable run id")
    ),
    responses(
        (status = 200, description = "Stored variable coverage and temporal capabilities", body = [VariableCapabilityResponse]),
        (status = 400, description = "Model id, run id, or variable metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while variable capabilities were inventoried", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Configured snapshot or catalog limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Capability inventory exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn variables_doc() {}

#[utoipa::path(
    get,
    path = "/v1/point",
    tag = "query",
    security(("bearer_auth" = [])),
    params(PointQueryRequest),
    responses(
        (status = 200, description = "Exact-time series at the nearest stored grid point", body = PointSeriesResponse),
        (status = 400, description = "Coordinates, variables, or time range are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "A requested variable or storage slot is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Query limit or strict missing-data requirement was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn point_doc() {}

#[utoipa::path(
    post,
    path = "/v1/points",
    tag = "query",
    security(("bearer_auth" = [])),
    request_body = PointsRequest,
    responses(
        (status = 200, description = "One exact-time series result per requested point", body = [PointSeriesResponse]),
        (status = 400, description = "Request body, coordinates, variables, or time range are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "A requested variable or storage slot is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Point/value limit or strict missing-data requirement was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn points_doc() {}

#[utoipa::path(
    post,
    path = "/v1/profile",
    tag = "query",
    security(("bearer_auth" = [])),
    request_body = ProfileApiRequest,
    responses(
        (status = 200, description = "Pressure-level profiles at one stored time and nearest grid point", body = ProfileResponse),
        (status = 400, description = "Request body, coordinates, or variables are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "A requested variable or storage slot is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Query limit, variable kind, or strict missing-data requirement was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn profile_doc() {}

#[utoipa::path(
    post,
    path = "/v1/window",
    tag = "query",
    security(("bearer_auth" = [])),
    request_body = WindowApiRequest,
    responses(
        (status = 200, description = "Stored two-dimensional index window", body = IndexWindowResponse),
        (status = 400, description = "Request body or window bounds are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Requested variable or storage slot is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "JSON grid-value or query limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn window_doc() {}

#[utoipa::path(
    post,
    path = "/v1/analytics/spatial-series",
    tag = "analytics",
    security(("bearer_auth" = [])),
    request_body = SpatialSeriesApiRequest,
    responses(
        (status = 200, description = "Per-valid-time full-grid minimum, maximum, and coverage", body = SpatialSeriesResponse),
        (status = 400, description = "Request body, variable, or time range is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Requested variable is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Query limit or strict missing-data requirement was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn spatial_series_doc() {}

#[utoipa::path(
    post,
    path = "/v1/analytics/temporal-grid",
    tag = "analytics",
    security(("bearer_auth" = [])),
    request_body = TemporalGridApiRequest,
    responses(
        (status = 200, description = "Semantics-aware full-grid temporal reduction", body = TemporalGridResponse),
        (status = 400, description = "Request body, temporal window, semantics, or variables are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "A requested variable or storage slot is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Reducer, query limit, or strict missing-data requirement was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn temporal_grid_doc() {}

#[utoipa::path(
    post,
    path = "/v1/jobs/temporal-grid",
    tag = "jobs",
    security(("bearer_auth" = [])),
    request_body = TemporalGridApiRequest,
    responses(
        (status = 202, description = "Durable asynchronous job accepted", body = JobView),
        (status = 400, description = "Request body or job request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The requested model or run does not exist", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the job was being admitted", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Request body could not be decoded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Asynchronous job capacity is full", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Job metadata or durable publication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Snapshot admission exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_temporal_grid_job_doc() {}

#[utoipa::path(
    get,
    path = "/v1/jobs/{id}",
    tag = "jobs",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "Canonical job UUID")),
    responses(
        (status = 200, description = "Current durable job state", body = JobView),
        (status = 400, description = "Job id is not a canonical UUID", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Job was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Job metadata could not be read", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn job_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/jobs/{id}",
    tag = "jobs",
    security(("bearer_auth" = [])),
    params(("id" = String, Path, description = "Canonical job UUID")),
    responses(
        (status = 200, description = "Terminal job returned without a new cancellation transition", body = JobView),
        (status = 202, description = "Cancellation accepted", body = JobView),
        (status = 400, description = "Job id is not a canonical UUID", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Job was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Job cannot make the requested state transition", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Job metadata could not be updated", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn cancel_job_doc() {}

#[utoipa::path(
    get,
    path = "/v1/artifacts/{hash}/{file}",
    tag = "jobs",
    security(("bearer_auth" = [])),
    params(
        ("hash" = String, Path, description = "Artifact SHA-256"),
        ("file" = String, Path, description = "Artifact filename from JobView")
    ),
    responses(
        (status = 200, description = "Immutable temporal-grid JSON artifact", content_type = "application/json", body = TemporalGridResponse),
        (status = 400, description = "Artifact hash or filename is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Artifact was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Artifact could not be streamed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn artifact_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/objects/resolve",
    tag = "community",
    description = "Resolve one canonical allowed query artifact in strict Phase 1 order: local CAS, R2-compatible hot storage, then the authoritative Hetzner dynamic HTTPS origin, with optional R2 promotion after origin success. Hetzner signs manifests and is never reached through TURN. Community Cache is disabled by default; there is no direct peer-connectivity code in Phase 1.",
    security(("bearer_auth" = [])),
    request_body = CommunityResolveRequestDoc,
    responses(
        (status = 200, description = "Origin-signed immutable object manifest", body = CommunityResolveResponseDoc),
        (status = 400, description = "Canonical identity or origin snapshot is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Requested immutable source data was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Signature, hash, schema, size, attribution, or private-publication policy failed closed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Community transfer quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Origin query or durable publication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, service busy, or shutdown in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Origin computation exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_resolve_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/objects/{sha256}",
    tag = "community",
    security(("bearer_auth" = [])),
    params(("sha256" = String, Path, description = "Exact object SHA-256 from its signed manifest")),
    responses(
        (status = 200, description = "Immutable encoded object bytes", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 400, description = "Object identity is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Object was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Object failed content-address verification", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Community download quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Object could not be read", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled or service busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Object read exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_object_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/artifacts",
    tag = "community",
    description = "Explicitly publish one closed, typed case artifact (annotation, derived table, overlay, or PNG/WebP rendered image). The canonical request is bound to the authenticated owner principal and exact source snapshot/grid. Private WRF, ArWen, and user-provided artifacts require confirmed redistribution rights, attribution/license fields, and a separate server gate. Paths, URLs, HTML/script, arbitrary files, raw wrfout, and complete runs are not accepted.",
    security(("bearer_auth" = [])),
    request_body = CommunityCaseArtifactPublicationDoc,
    responses(
        (status = 201, description = "Origin-signed content-addressed artifact manifest", body = SignedCommunityObjectManifestDoc),
        (status = 400, description = "Owner binding, identity, payload, or retention is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Typed artifact exceeds configured bounds", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Rights, attribution, schema, hash, signature, or typed payload validation failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Publication quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Artifact publication disabled, killed, or service busy", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_publish_artifact_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/artifacts/{sha256}/revoke",
    tag = "community",
    description = "Create a durable rights-withdrawal tombstone and stop serving an owner-published artifact. Only the authenticated publication owner may revoke it.",
    security(("bearer_auth" = [])),
    params(("sha256" = String, Path, description = "Exact published object SHA-256")),
    request_body = CommunityRevocationDoc,
    responses(
        (status = 200, description = "Durable publication tombstone", body = CommunityPublicationTombstoneDoc),
        (status = 400, description = "Revocation or owner binding is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Owned publication was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Revocation confirmation or tombstone schema failed validation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Publication quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Publication disabled, killed, or service busy", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_revoke_artifact_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/cases",
    tag = "community",
    description = "Deliberately publish one case room. Passive searches are never published. Private WRF/ArWen requires explicit owner publication and confirmed redistribution rights.",
    security(("bearer_auth" = [])),
    request_body = CommunityCaseManifestDoc,
    responses(
        (status = 201, description = "Signed case-room manifest", body = SignedCommunityCaseManifestDoc),
        (status = 400, description = "Case manifest is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "A referenced immutable artifact is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Signature, rights, attribution, schema, or size policy failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Community upload quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Case signing or durable publication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Case publication disabled, killed, or service busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Case publication exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_create_case_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/cases/{case_id}",
    tag = "community",
    security(("bearer_auth" = [])),
    params(("case_id" = String, Path, description = "Opaque case-room id")),
    responses(
        (status = 200, description = "Signed case-room manifest", body = SignedCommunityCaseManifestDoc),
        (status = 400, description = "Case id is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Case room was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Stored case signature or schema failed verification", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Community download quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Case metadata could not be read", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled or service busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Case read exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_case_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/cases/{case_id}/revoke",
    tag = "community",
    description = "Withdraw a deliberately published case room and persist a tombstone. The authenticated principal must own every referenced typed artifact.",
    security(("bearer_auth" = [])),
    params(("case_id" = String, Path, description = "Opaque case-room id")),
    request_body = CommunityRevocationDoc,
    responses(
        (status = 204, description = "Case publication revoked"),
        (status = 400, description = "Revocation or owner binding is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Owned case publication was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Revocation confirmation or tombstone schema failed validation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Publication quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Case publication disabled, killed, or service busy", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_revoke_case_doc() {}

#[utoipa::path(
    get,
    path = "/v1/federation/origins",
    tag = "federation",
    description = "Return an authority-signed catalog containing origin-signed descriptors for operator-approved university, lab, and public Rusty Weather HTTPS origins. Ordinary Community Cache clients never appear in this catalog and there is no self-registration endpoint.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Signed bounded public-origin catalog", body = SignedFederationCatalogDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A descriptor is expired, revoked, untrusted, malformed, or has an unsafe URL", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Public-origin federation is disabled or the service is busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Catalog signing exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_catalog_doc() {}

#[utoipa::path(
    get,
    path = "/v1/federation/origins/{origin_id}",
    tag = "federation",
    description = "Return one exact origin-signed descriptor already admitted by the operator allowlist and revocation policy.",
    security(("bearer_auth" = [])),
    params(("origin_id" = String, Path, description = "Canonical public origin id from the signed catalog")),
    responses(
        (status = 200, description = "Signed public-origin descriptor", body = SignedFederationOriginDescriptorDoc),
        (status = 400, description = "Origin id is not canonical", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Origin is absent from the approved catalog", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Descriptor is expired, revoked, untrusted, malformed, or has an unsafe URL", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Public-origin federation is disabled or the service is busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Descriptor verification exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_origin_doc() {}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "operations",
    description = "OpenMetrics endpoint. It is protected by bearer authentication by default (auth.protect_metrics = true); operators may explicitly opt out with auth.protect_metrics = false. When no tokens are configured, authentication middleware permits local requests.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "OpenMetrics text exposition", content_type = "application/openmetrics-text", body = String),
        (status = 401, description = "Bearer authentication failed when metrics protection and tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Metrics encoding failed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn metrics_doc() {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn operation<'a>(
        document: &'a Value,
        path: &str,
        method: &str,
    ) -> &'a serde_json::Map<String, Value> {
        document["paths"][path][method]
            .as_object()
            .unwrap_or_else(|| panic!("missing OpenAPI operation {method} {path}"))
    }

    fn response_schema<'a>(
        document: &'a Value,
        path: &str,
        method: &str,
        status: &str,
        content_type: &str,
    ) -> &'a Value {
        &operation(document, path, method)["responses"][status]["content"][content_type]["schema"]
    }

    fn assert_response_ref(
        document: &Value,
        path: &str,
        method: &str,
        expected: &str,
        array: bool,
    ) {
        let schema = response_schema(document, path, method, "200", "application/json");
        let reference = if array {
            assert_eq!(
                schema["type"], "array",
                "{method} {path} must return an array"
            );
            schema["items"]["$ref"].as_str()
        } else {
            schema["$ref"].as_str()
        };
        assert_eq!(
            reference,
            Some(expected),
            "{method} {path} must publish its concrete response schema"
        );
    }

    #[test]
    fn stable_v1_paths_and_security_scheme_are_present() {
        let value = serde_json::to_value(document()).unwrap();
        let paths = value["paths"].as_object().unwrap();
        for path in [
            "/v1/health/live",
            "/v1/openapi.json",
            "/v1/models",
            "/v1/point",
            "/v1/window",
            "/v1/analytics/spatial-series",
            "/v1/analytics/temporal-grid",
            "/v1/jobs/temporal-grid",
            "/v1/jobs/{id}",
            "/v1/artifacts/{hash}/{file}",
            "/metrics",
        ] {
            assert!(paths.contains_key(path), "missing OpenAPI path {path}");
        }
        assert!(
            paths["/v1/window"]["post"]["responses"]
                .get("501")
                .is_none()
        );
        assert!(
            paths["/v1/analytics/spatial-series"]["post"]["responses"]
                .get("501")
                .is_none()
        );
        assert!(
            value["components"]["schemas"]["VariableCapabilityResponse"].is_object(),
            "variable capability schema must be published"
        );
        assert!(
            value["components"]["schemas"]["VariableTemporalCapabilityResponse"].is_object(),
            "temporal capability schema must be published"
        );
        assert!(value["components"]["securitySchemes"]["bearer_auth"].is_object());
        let submit_responses = &paths["/v1/jobs/temporal-grid"]["post"]["responses"];
        for status in ["404", "409", "503", "504"] {
            assert!(
                submit_responses[status].is_object(),
                "job submission must document runtime status {status}"
            );
        }
    }

    #[test]
    fn community_cache_paths_are_explicitly_documented_and_protected() {
        let value = serde_json::to_value(document()).unwrap();
        for (path, method) in [
            ("/v1/community/objects/resolve", "post"),
            ("/v1/community/objects/{sha256}", "get"),
            ("/v1/community/artifacts", "post"),
            ("/v1/community/artifacts/{sha256}/revoke", "post"),
            ("/v1/community/cases", "post"),
            ("/v1/community/cases/{case_id}", "get"),
            ("/v1/community/cases/{case_id}/revoke", "post"),
        ] {
            let operation = operation(&value, path, method);
            assert_eq!(
                operation["security"][0]["bearer_auth"],
                serde_json::json!([])
            );
            assert!(operation["responses"]["401"].is_object());
            assert!(operation["responses"]["422"].is_object());
            assert!(operation["responses"]["429"].is_object());
        }
        let description = operation(&value, "/v1/community/objects/resolve", "post")["description"]
            .as_str()
            .unwrap();
        assert!(description.contains("no direct peer-connectivity"));
    }

    #[test]
    fn federation_is_get_only_authenticated_and_concretely_documented() {
        let value = serde_json::to_value(document()).unwrap();
        for path in [
            "/v1/federation/origins",
            "/v1/federation/origins/{origin_id}",
        ] {
            let operation = operation(&value, path, "get");
            assert_eq!(
                operation["security"][0]["bearer_auth"],
                serde_json::json!([])
            );
            assert!(operation["responses"]["401"].is_object());
            assert!(operation["responses"]["422"].is_object());
            assert!(value["paths"][path]["post"].is_null());
        }
        assert_response_ref(
            &value,
            "/v1/federation/origins",
            "get",
            "#/components/schemas/SignedFederationCatalogDoc",
            false,
        );
        assert_response_ref(
            &value,
            "/v1/federation/origins/{origin_id}",
            "get",
            "#/components/schemas/SignedFederationOriginDescriptorDoc",
            false,
        );
    }

    #[test]
    fn catalog_and_query_successes_publish_concrete_wire_schemas() {
        let value = serde_json::to_value(document()).unwrap();
        for (path, method, expected, array) in [
            (
                "/v1/models",
                "get",
                "#/components/schemas/ModelCapabilityResponse",
                true,
            ),
            (
                "/v1/models/{model}/runs",
                "get",
                "#/components/schemas/RunCatalogEntryResponse",
                true,
            ),
            (
                "/v1/models/{model}/runs/{run}",
                "get",
                "#/components/schemas/RunDescriptorResponse",
                false,
            ),
            (
                "/v1/models/{model}/runs/{run}/variables",
                "get",
                "#/components/schemas/VariableCapabilityResponse",
                true,
            ),
            (
                "/v1/point",
                "get",
                "#/components/schemas/PointSeriesResponse",
                false,
            ),
            (
                "/v1/points",
                "post",
                "#/components/schemas/PointSeriesResponse",
                true,
            ),
            (
                "/v1/profile",
                "post",
                "#/components/schemas/ProfileResponse",
                false,
            ),
            (
                "/v1/window",
                "post",
                "#/components/schemas/IndexWindowResponse",
                false,
            ),
            (
                "/v1/analytics/spatial-series",
                "post",
                "#/components/schemas/SpatialSeriesResponse",
                false,
            ),
            (
                "/v1/analytics/temporal-grid",
                "post",
                "#/components/schemas/TemporalGridResponse",
                false,
            ),
        ] {
            assert_response_ref(&value, path, method, expected, array);
        }
    }

    #[test]
    fn provenance_and_attribution_have_concrete_component_schemas() {
        let value = serde_json::to_value(document()).unwrap();
        let schemas = &value["components"]["schemas"];
        assert!(schemas["SourceProvenanceResponse"].is_object());
        assert!(schemas["ProviderAttributionResponse"].is_object());
        assert_eq!(
            schemas["RunDescriptorResponse"]["properties"]["source_provenance"]["items"]["$ref"],
            "#/components/schemas/SourceProvenanceResponse"
        );
        assert_eq!(
            schemas["RunDescriptorResponse"]["properties"]["provider_attributions"]["items"]["$ref"],
            "#/components/schemas/ProviderAttributionResponse"
        );
        assert_eq!(
            schemas["ModelCapabilityResponse"]["properties"]["provider_attributions"]["items"]["$ref"],
            "#/components/schemas/ProviderAttributionResponse"
        );
    }

    #[test]
    fn model_limitations_publish_stable_snake_case_enum_values() {
        let value = serde_json::to_value(document()).unwrap();
        let schemas = &value["components"]["schemas"];
        assert_eq!(
            schemas["ModelCapabilityResponse"]["properties"]["limitations"]["items"]["$ref"],
            "#/components/schemas/ApiIngestCapabilityLimitation"
        );
        assert_eq!(
            schemas["ApiIngestCapabilityLimitation"]["enum"],
            serde_json::json!([
                "analysis_only",
                "surface_only",
                "ensemble_mean_only",
                "sparse_pressure_levels",
                "derived_products_disabled",
                "conus_only",
                "pre_operational_feed"
            ])
        );
    }

    #[test]
    fn native_window_schema_represents_missing_cells_as_null() {
        let value = serde_json::to_value(document()).unwrap();
        assert_eq!(
            value["components"]["schemas"]["IndexWindowResponse"]["properties"]["values"]["items"]
                ["type"],
            serde_json::json!(["number", "null"])
        );
    }

    #[test]
    fn temporal_grid_schema_matches_the_runtime_tagged_union() {
        let value = serde_json::to_value(document()).unwrap();
        let variants = value["components"]["schemas"]["TemporalGridResponse"]["oneOf"]
            .as_array()
            .expect("temporal grid response must be a oneOf union");
        assert_eq!(variants.len(), 8);
        let mut names = variants
            .iter()
            .map(|variant| {
                let required = variant["required"].as_array().unwrap();
                assert!(required.contains(&Value::String("result".into())));
                assert!(required.contains(&Value::String("data".into())));
                assert!(variant["properties"]["data"]["$ref"].is_string());
                variant["properties"]["result"]["enum"][0]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            [
                "categorical",
                "circular",
                "cumulative",
                "interval",
                "interval_maximum",
                "rate",
                "scalar",
                "vector",
            ]
        );
    }

    #[test]
    fn documented_errors_are_problem_details_with_the_runtime_media_type() {
        let value = serde_json::to_value(document()).unwrap();
        let paths = value["paths"].as_object().unwrap();
        for (path, path_item) in paths {
            for method in ["get", "post", "delete"] {
                let Some(operation) = path_item.get(method) else {
                    continue;
                };
                let responses = operation["responses"].as_object().unwrap();
                for (status, response) in responses {
                    let Ok(status) = status.parse::<u16>() else {
                        continue;
                    };
                    if status < 400 {
                        continue;
                    }
                    assert_eq!(
                        response["content"]["application/problem+json"]["schema"]["$ref"],
                        "#/components/schemas/ProblemDetails",
                        "{method} {path} status {status} must use ProblemDetails"
                    );
                }
            }
        }

        let point = &operation(&value, "/v1/point", "get")["responses"];
        for status in ["400", "401", "404", "409", "422", "500", "503", "504"] {
            assert!(point.get(status).is_some(), "point response omits {status}");
        }
        let points = &operation(&value, "/v1/points", "post")["responses"];
        for status in ["413", "415"] {
            assert!(
                points.get(status).is_some(),
                "points response omits {status}"
            );
        }
    }

    #[test]
    fn metrics_declares_the_protected_default_and_explicit_opt_out() {
        let value = serde_json::to_value(document()).unwrap();
        let metrics = operation(&value, "/metrics", "get");
        assert_eq!(metrics["security"][0]["bearer_auth"], serde_json::json!([]));
        let description = metrics["description"].as_str().unwrap();
        assert!(description.contains("protect_metrics = true"));
        assert!(description.contains("protect_metrics = false"));
        assert!(metrics["responses"].get("401").is_some());
    }
}

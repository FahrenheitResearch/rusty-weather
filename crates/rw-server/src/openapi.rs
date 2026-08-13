#![allow(dead_code)] // Utoipa consumes the document-only handler stubs via its derive macro.

use utoipa::openapi::OpenApi as OpenApiDocument;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use crate::federation_proxy::{FederationProxyKillSwitchRequest, FederationProxyStatusResponse};
use crate::generation_replication::{
    ReplicationGarbageCollectionResponse, ReplicationKillSwitchRequest, ReplicationOwnerResponse,
    ReplicationStatusResponse,
};
use crate::origin_catalog::OriginCatalogHealthStatus;
use crate::problem::ProblemDetails;
use crate::routes::{
    ApiIngestCapabilityLimitation, ApiIntervalSupport, ApiMissingPolicy,
    ApiTemporalCapabilityBasis, ApiTemporalOperation, ApiTemporalReducer, ApiTemporalSemantics,
    ApiTemporalValueClass, ApiTemporalVerticalSelection, ApiTemporalWindow, ApiTimeExpectation,
    CoordinateRequest, GeographicVerticalApiSelection, GeographicWindowApiRequest, HealthResponse,
    ModelCapabilityResponse, PointQueryRequest, PointsRequest, ProductCapabilityResponse,
    ProfileApiRequest, ProviderAttributionResponse, SpatialSeriesApiRequest,
    TemporalGridApiRequest, VariableCapabilityResponse, VariableTemporalCapabilityResponse,
    VersionResponse, WindowApiRequest,
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
struct NativeGridEnvelopeResponse {
    x0: usize,
    y0: usize,
    nx: usize,
    ny: usize,
}

#[derive(utoipa::ToSchema)]
struct GeographicBoundingBoxResponse {
    west_longitude: f64,
    south_latitude: f64,
    east_longitude: f64,
    north_latitude: f64,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum LongitudeArcResponse {
    Ordinary,
    CrossesAntimeridian,
    FullGlobe,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum GridProjectionResponse {
    Geographic,
    LambertConformal {
        standard_parallel_1_deg: f64,
        standard_parallel_2_deg: f64,
        central_meridian_deg: f64,
    },
    PolarStereographic {
        true_latitude_deg: f64,
        central_meridian_deg: f64,
        south_pole_on_projection_plane: bool,
    },
    Mercator {
        latitude_of_true_scale_deg: f64,
        central_meridian_deg: f64,
    },
    Other {
        template: u16,
    },
}

#[derive(utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum GeographicFieldValuesResponse {
    Surface2d {
        values: Vec<Option<f32>>,
    },
    PressureLevels {
        levels_hpa: Vec<u16>,
        values: Vec<Option<f32>>,
    },
}

#[derive(utoipa::ToSchema)]
struct GeographicFieldResponse {
    variable: String,
    units: String,
    selector: serde_json::Value,
    data: GeographicFieldValuesResponse,
}

/// Schema `rw.query.geographic-window.v1`. Projection carries the exact
/// serialized rustwx-core GridProjection tagged union or null.
#[derive(utoipa::ToSchema)]
struct GeographicWindowResponse {
    schema: String,
    run: RunDescriptorResponse,
    time: TimePointResponse,
    requested_bbox: GeographicBoundingBoxResponse,
    longitude_arc: LongitudeArcResponse,
    envelope_semantics: String,
    cell_inclusion_semantics: String,
    envelope: NativeGridEnvelopeResponse,
    latitudes: Vec<Option<f32>>,
    longitudes: Vec<Option<f32>>,
    cell_mask: Vec<bool>,
    mask_required: bool,
    projection: Option<GridProjectionResponse>,
    fields: Vec<GeographicFieldResponse>,
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
struct CommunityCaseDirectoryPageDoc {
    schema: String,
    cases: Vec<SignedCommunityCaseManifestDoc>,
    next_after: Option<String>,
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

#[derive(utoipa::ToSchema)]
struct RelayAdvertisementRequestDoc {
    schema: String,
    signed_manifest: SignedCommunityObjectManifestDoc,
    opted_in: bool,
    categories: Vec<String>,
    disk_allowance_bytes: u64,
    upload_allowance_bytes: u64,
    metered_network: bool,
    allow_metered_seeding: bool,
}

#[derive(utoipa::ToSchema)]
struct RelayAdvertisementReceiptDoc {
    schema: String,
    advertisement_id: String,
    object_sha256: String,
    expires_unix: i64,
}

#[derive(utoipa::ToSchema)]
struct HistoricalRelayLookupRequestDoc {
    schema: String,
    historical: bool,
    object_sha256: String,
    opted_in: bool,
    download_allowance_bytes: u64,
}

#[derive(utoipa::ToSchema)]
struct RelayTurnAccessDoc {
    urls: Vec<String>,
    username: String,
    credential: String,
    expires_unix: i64,
}

#[derive(utoipa::ToSchema)]
struct ParticipantRelayGrantDoc {
    schema: String,
    session_id: String,
    object_sha256: String,
    encoded_size: u64,
    role: String,
    candidate: serde_json::Value,
    credential: serde_json::Value,
    turn: RelayTurnAccessDoc,
}

#[derive(utoipa::ToSchema)]
struct HistoricalRelayLookupResponseDoc {
    schema: String,
    participant_grant: Option<ParticipantRelayGrantDoc>,
    fallback: Option<serde_json::Value>,
}

#[derive(utoipa::ToSchema)]
struct RelayGrantPollRequestDoc {
    schema: String,
}

#[derive(utoipa::ToSchema)]
struct RelayRouteRegistrationRequestDoc {
    schema: String,
    credential: serde_json::Value,
    offer: serde_json::Value,
    /// This participant's own provider allocation returned by TURN. It is
    /// transport-private, never a host/server-reflexive/direct candidate.
    turn_local_addr: String,
}

#[derive(utoipa::ToSchema)]
struct RelayRouteRegistrationReceiptDoc {
    schema: String,
    session_id: String,
    role: String,
    binding_ready: bool,
}

#[derive(utoipa::ToSchema)]
struct RelayTransportGrantRequestDoc {
    schema: String,
    role: String,
    credential: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct RelayTransportGrantDoc {
    schema: String,
    session_id: String,
    role: String,
    peer_relay_allocation: String,
    signed_binding: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct RelaySessionCompletionRequestDoc {
    schema: String,
    credential: serde_json::Value,
    transferred_bytes: u64,
}

#[derive(utoipa::ToSchema)]
struct RelaySessionFailureRequestDoc {
    schema: String,
    role: String,
    credential: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct RelayTerminalResponseDoc {
    fallback: Option<serde_json::Value>,
    promotion_requested: bool,
}

#[derive(utoipa::ToSchema)]
struct RelayKillSwitchRequestDoc {
    schema: String,
    enabled: bool,
}

#[derive(utoipa::ToSchema)]
struct RelayStatusResponseDoc {
    schema: String,
    enabled: bool,
    kill_switch: bool,
    persistence_healthy: bool,
    transport_route_gate_configured: bool,
    sessions_issued: u64,
    sessions_completed: u64,
    sessions_failed: u64,
    promotion_signals: u64,
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

#[derive(utoipa::ToSchema)]
struct FederationOriginHealthStatusDoc {
    origin_id: String,
    state: String,
    consecutive_failures: u32,
    quarantine_until_unix: Option<i64>,
    last_probe_unix: Option<i64>,
    last_success_unix: Option<i64>,
}

#[derive(utoipa::ToSchema)]
struct FederationHealthStatusDoc {
    schema: String,
    monitor_enabled: bool,
    total_origins: usize,
    healthy_origins: usize,
    degraded_origins: usize,
    quarantined_origins: usize,
    unknown_origins: usize,
    last_round_unix: Option<i64>,
    origins: Vec<FederationOriginHealthStatusDoc>,
}

#[derive(utoipa::ToSchema)]
struct FederationProxyRequestDoc {
    schema: String,
    /// Exact canonical ShareRequest; mutable aliases are forbidden.
    request: serde_json::Value,
    /// Optional preference among origins already admitted by signed policy.
    preferred_origin_id: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationManifestDoc {
    schema: String,
    generation_id: String,
    model: String,
    run: String,
    source_snapshot_id: String,
    grid_hash: String,
    owner_principal_sha256: String,
    publication: serde_json::Value,
    source_provenance: Vec<serde_json::Value>,
    files: Vec<serde_json::Value>,
    total_bytes: u64,
    generation_sha256: String,
    published_unix: i64,
    retain_until_unix: i64,
    attributions: Vec<serde_json::Value>,
    modification_notices: Vec<String>,
}

#[derive(utoipa::ToSchema)]
struct BeginRunGenerationRequestDoc {
    schema: String,
    manifest: RunGenerationManifestDoc,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationUploadStatusDoc {
    schema: String,
    generation_id: String,
    generation_sha256: String,
    total_chunks: u32,
    missing_chunks: u32,
    upload_expires_unix: i64,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationMissingChunkDoc {
    object_sha256: String,
    byte_size: u64,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationMissingPageDoc {
    schema: String,
    generation_id: String,
    chunks: Vec<RunGenerationMissingChunkDoc>,
    next_after: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct FinalizeRunGenerationRequestDoc {
    schema: String,
    generation_sha256: String,
}

#[derive(utoipa::ToSchema)]
struct PublishedRunGenerationDoc {
    schema: String,
    generation_id: String,
    generation_sha256: String,
    source_snapshot_id: String,
    local_snapshot_id: String,
    grid_hash: String,
    model: String,
    run: String,
    published_unix: i64,
}

#[derive(utoipa::ToSchema)]
struct RevokeRunGenerationRequestDoc {
    schema: String,
    generation_sha256: String,
    rights_withdrawn: bool,
    reason: String,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationTombstoneDoc {
    schema: String,
    generation_id: String,
    generation_sha256: String,
    owner_principal_sha256: String,
    revoked_unix: i64,
    rights_withdrawn: bool,
    reason: String,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationAdvertisedLimitsDoc {
    maximum_generation_bytes: u64,
    maximum_files: u64,
    maximum_chunks: u64,
    maximum_chunk_bytes: u64,
    maximum_manifest_bytes: u64,
    minimum_retention_seconds: i64,
    maximum_retention_seconds: i64,
    maximum_provenance_entries: u64,
    maximum_attributions: u64,
    upload_ttl_seconds: i64,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationOwnerQuotaDoc {
    maximum_storage_bytes: u64,
    maximum_generations: u64,
    maximum_concurrent_uploads: u64,
    maximum_monthly_upload_bytes: u64,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationOwnerUsageDoc {
    active_uploads: u64,
    live_publications: u64,
    pending_retirements: u64,
    tombstones: u64,
    reserved_bytes: u64,
    published_bytes: u64,
    pending_retirement_bytes: u64,
    billing_utc_month: String,
    monthly_accepted_upload_bytes: u64,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationOwnerCapabilitiesDoc {
    schema: String,
    owner_principal_sha256: String,
    accepting_uploads: bool,
    limits: RunGenerationAdvertisedLimitsDoc,
    quota: RunGenerationOwnerQuotaDoc,
    usage: RunGenerationOwnerUsageDoc,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum RunGenerationOwnerRecordStateDoc {
    Published,
    Tombstone,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationOwnerRecordDoc {
    schema: String,
    state: RunGenerationOwnerRecordStateDoc,
    generation_id: String,
    generation_sha256: String,
    publication: Option<PublishedRunGenerationDoc>,
    tombstone: Option<RunGenerationTombstoneDoc>,
}

#[derive(utoipa::ToSchema)]
struct RunGenerationOwnerListPageDoc {
    schema: String,
    records: Vec<RunGenerationOwnerRecordDoc>,
    next_after: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct CancelledRunGenerationDoc {
    schema: String,
    generation_id: String,
    generation_sha256: String,
    cancelled_unix: i64,
    released_reserved_bytes: u64,
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
        latest_run_doc,
        run_doc,
        variables_doc,
        point_doc,
        points_doc,
        profile_doc,
        window_doc,
        geographic_window_doc,
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
        community_list_cases_doc,
        community_case_doc,
        community_revoke_case_doc,
        relay_advertise_doc,
        relay_historical_lookup_doc,
        relay_next_grant_doc,
        relay_session_grant_doc,
        relay_register_route_doc,
        relay_transport_doc,
        relay_complete_doc,
        relay_fail_doc,
        relay_revoke_doc,
        relay_kill_switch_doc,
        relay_status_doc,
        federation_catalog_doc,
        federation_origin_doc,
        federation_health_doc,
        federation_proxy_resolve_doc,
        federation_proxy_operator_status_doc,
        federation_proxy_kill_switch_doc,
        federation_local_resolve_doc,
        federation_local_object_doc,
        origin_catalog_status_doc,
        generation_replication_owner_doc,
        generation_replication_capabilities_doc,
        generation_replication_list_doc,
        generation_replication_begin_doc,
        generation_replication_status_doc,
        generation_replication_cancel_doc,
        generation_replication_publication_doc,
        generation_replication_missing_doc,
        generation_replication_upload_doc,
        generation_replication_finalize_doc,
        generation_replication_revoke_doc,
        generation_replication_operator_status_doc,
        generation_replication_kill_switch_doc,
        generation_replication_gc_doc,
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
        GeographicVerticalApiSelection,
        GeographicWindowApiRequest,
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
        NativeGridEnvelopeResponse,
        GeographicBoundingBoxResponse,
        LongitudeArcResponse,
        GridProjectionResponse,
        GeographicFieldValuesResponse,
        GeographicFieldResponse,
        GeographicWindowResponse,
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
        CommunityCaseDirectoryPageDoc,
        CommunityCaseArtifactPublicationDoc,
        CommunityRevocationDoc,
        CommunityPublicationTombstoneDoc,
        RelayAdvertisementRequestDoc,
        RelayAdvertisementReceiptDoc,
        HistoricalRelayLookupRequestDoc,
        RelayTurnAccessDoc,
        ParticipantRelayGrantDoc,
        HistoricalRelayLookupResponseDoc,
        RelayGrantPollRequestDoc,
        RelayRouteRegistrationRequestDoc,
        RelayRouteRegistrationReceiptDoc,
        RelayTransportGrantRequestDoc,
        RelayTransportGrantDoc,
        RelaySessionCompletionRequestDoc,
        RelaySessionFailureRequestDoc,
        RelayTerminalResponseDoc,
        RelayKillSwitchRequestDoc,
        RelayStatusResponseDoc,
        FederationOriginDescriptorDoc,
        SignedFederationOriginDescriptorDoc,
        FederationCatalogDoc,
        SignedFederationCatalogDoc,
        FederationOriginHealthStatusDoc,
        FederationHealthStatusDoc,
        FederationProxyRequestDoc,
        FederationProxyStatusResponse,
        FederationProxyKillSwitchRequest,
        OriginCatalogHealthStatus,
        ReplicationOwnerResponse,
        ReplicationStatusResponse,
        ReplicationKillSwitchRequest,
        ReplicationGarbageCollectionResponse,
        RunGenerationManifestDoc,
        BeginRunGenerationRequestDoc,
        RunGenerationUploadStatusDoc,
        RunGenerationMissingChunkDoc,
        RunGenerationMissingPageDoc,
        FinalizeRunGenerationRequestDoc,
        PublishedRunGenerationDoc,
        RevokeRunGenerationRequestDoc,
        RunGenerationTombstoneDoc,
        RunGenerationAdvertisedLimitsDoc,
        RunGenerationOwnerQuotaDoc,
        RunGenerationOwnerUsageDoc,
        RunGenerationOwnerCapabilitiesDoc,
        RunGenerationOwnerRecordStateDoc,
        RunGenerationOwnerRecordDoc,
        RunGenerationOwnerListPageDoc,
        CancelledRunGenerationDoc,
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
        (name = "generation-replication", description = "Advanced default-off owner publication of complete immutable rw-store generations"),
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
            components.add_security_scheme(
                "federation_origin_auth",
                SecurityScheme::Http(
                    HttpBuilder::new()
                        .scheme(HttpAuthScheme::Bearer)
                        .bearer_format("opaque")
                        .description(Some(
                            "Dedicated per-origin token; ordinary BowEcho API tokens are rejected",
                        ))
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
    path = "/v1/models/{model}/runs/latest",
    tag = "catalog",
    security(("bearer_auth" = [])),
    params(("model" = String, Path, description = "Canonical stored model id")),
    description = "Resolve the newest visible, authorized run by physical cycle origin. The mutable pointer is deterministic, private, and no-store; clients should bind subsequent queries to the returned immutable run and snapshot identities.",
    responses(
        (status = 200, description = "Newest visible immutable run descriptor; private no-store", body = RunDescriptorResponse),
        (status = 400, description = "Model id or visible run ordering metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "No visible run is available for the model", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "The selected run changed while its snapshot was resolved", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Configured catalog or snapshot limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Catalog, run metadata, or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Publication catalog is unavailable, service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Latest-run resolution exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn latest_run_doc() {}

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
    path = "/v1/geographic-window",
    tag = "query",
    description = "Resolve a finite geographic bbox against one exact snapshot/grid, then return its minimal native rectangular envelope with cropped lat/lon arrays, exact projection metadata, and a cell mask. Longitude is an eastward arc; west > east crosses the antimeridian and -180..180 selects the globe. Surface fields and explicit pressure levels are read from intersecting native chunks only; no full-grid coordinate payload or vertical reduction is emitted.",
    security(("bearer_auth" = [])),
    request_body = GeographicWindowApiRequest,
    responses(
        (status = 200, description = "Versioned self-describing geographic-domain field window", body = GeographicWindowResponse),
        (status = 400, description = "Bounds, identity, variables, levels, or request shape are invalid or have no overlap", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Requested variable, storage slot, model, or run is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the geographic window was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Geographic cell/output cap or variable-kind constraint was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy or shutting down", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Geographic extraction exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn geographic_window_doc() {}

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
    path = "/v1/community/cases",
    tag = "community",
    description = "Return a cursor-bounded directory containing only deliberate, complete origin-signed case-room publications. Passive searches never appear. Responses are private and no-store.",
    security(("bearer_auth" = [])),
    params(
        ("after" = Option<String>, Query, description = "Opaque last case id from the previous page"),
        ("limit" = Option<usize>, Query, description = "Page size from 1 through 100; default 50")
    ),
    responses(
        (status = 200, description = "Strictly ordered signed case-room page", body = CommunityCaseDirectoryPageDoc),
        (status = 400, description = "Cursor or limit is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A stored case or referenced artifact failed verification", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Community download quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Case rooms are disabled, killed, or service is busy", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn community_list_cases_doc() {}

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
    post,
    path = "/v1/community/relay/advertisements",
    tag = "community",
    description = "Advertise one exact cold immutable object from an origin-signed manifest. This opt-in endpoint accepts no arbitrary file, raw directory, private unpublished run, seed listing, address, or direct candidate.",
    security(("bearer_auth" = [])),
    request_body = RelayAdvertisementRequestDoc,
    responses(
        (status = 201, description = "Durably accepted advertisement", body = RelayAdvertisementReceiptDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Manifest, rights, category, signature, or schema rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Storage, upload, metered-network, concurrency, or cost policy denied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay disabled, killed, or durable state unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_advertise_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/historical/lookups",
    tag = "community",
    description = "Resolve only an exact cold historical origin-signed hash. `historical` must be true; current operational local → R2 → Hetzner traffic never invokes this endpoint. A miss immediately selects archival HTTPS or honest unavailable fallback.",
    security(("bearer_auth" = [])),
    request_body = HistoricalRelayLookupRequestDoc,
    responses(
        (status = 200, description = "Caller-specific downloader grant or immediate fallback; private no-store", body = HistoricalRelayLookupResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Not an exact cold historical request", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Download, concurrency, metered-network, or cost policy denied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay/provider unavailable; continue immediately to archival HTTPS/unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_historical_lookup_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/grants/next",
    tag = "community",
    description = "Poll only the authenticated caller's oldest uploader grant. There is no session, seed, peer, account, or address directory.",
    security(("bearer_auth" = [])),
    request_body = RelayGrantPollRequestDoc,
    responses(
        (status = 200, description = "Caller-specific participant grant; private no-store", body = ParticipantRelayGrantDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "No grant exists for this authenticated principal", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_next_grant_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/sessions/{session_id}/grants/{role}",
    tag = "community",
    description = "Retrieve one exact participant grant only when the authenticated caller owns that session role.",
    security(("bearer_auth" = [])),
    params(
        ("session_id" = String, Path, description = "Opaque relay session id"),
        ("role" = String, Path, description = "uploader or downloader")
    ),
    responses(
        (status = 200, description = "Caller-specific participant grant; private no-store", body = ParticipantRelayGrantDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "No matching caller-owned grant", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_session_grant_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/routes",
    tag = "community",
    description = "Register only this participant's own TURN `local_addr()` after subject/role/credential/key binding and an operator-audited provider-allocation CIDR check. Host, server-reflexive, private, STUN, ICE, and direct candidates are forbidden. The address is transport-private and the response is no-store.",
    security(("bearer_auth" = [])),
    request_body = RelayRouteRegistrationRequestDoc,
    responses(
        (status = 200, description = "Route registration receipt without any address", body = RelayRouteRegistrationReceiptDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Participant credential does not belong to caller", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Route, role, offer, binding, or replay rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay or audited route gate unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_register_route_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/transport",
    tag = "community",
    description = "Return only the authenticated participant's counterpart TURN-provider allocation and signed end-to-end key transcript. It never returns a peer host/server-reflexive/direct address, combined grants, or account identity; response bodies are private no-store and excluded from logs.",
    security(("bearer_auth" = [])),
    request_body = RelayTransportGrantRequestDoc,
    responses(
        (status = 200, description = "Participant-specific transport-private relay route", body = RelayTransportGrantDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Participant credential does not belong to caller", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Counterpart TURN allocation is not ready", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_transport_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/sessions/complete",
    tag = "community",
    description = "Durably record successful receipt. Only the authenticated downloader credential may complete a session; full conservative reservation accounting is retained.",
    security(("bearer_auth" = [])),
    request_body = RelaySessionCompletionRequestDoc,
    responses(
        (status = 200, description = "Durable terminal result and optional R2 promotion signal", body = RelayTerminalResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Credential does not belong to caller/downloader", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Completion size, state, or credential rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Terminal state could not be durably committed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_complete_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/sessions/fail",
    tag = "community",
    description = "Durably fail and revoke a caller-owned participant session, then return immediate archival HTTPS/unavailable fallback.",
    security(("bearer_auth" = [])),
    request_body = RelaySessionFailureRequestDoc,
    responses(
        (status = 200, description = "Durable terminal fallback", body = RelayTerminalResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Credential does not belong to caller", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Terminal state could not be durably committed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_fail_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/sessions/revoke",
    tag = "community",
    description = "Explicitly revoke a caller-owned participant session and return immediate archival HTTPS/unavailable fallback.",
    security(("bearer_auth" = [])),
    request_body = RelaySessionFailureRequestDoc,
    responses(
        (status = 200, description = "Durable terminal fallback", body = RelayTerminalResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Credential does not belong to caller", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Terminal state could not be durably committed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_revoke_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/relay/operator/kill-switch",
    tag = "operations",
    description = "Immediately stop admissions, clear ephemeral dispatch/routes, revoke active sessions, and durably record the relay kill switch. Access is restricted to configured authenticated-principal digests.",
    security(("bearer_auth" = [])),
    request_body = RelayKillSwitchRequestDoc,
    responses(
        (status = 200, description = "Redacted relay status; private no-store", body = RelayStatusResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured relay operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Kill state could not be durably committed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_kill_switch_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/relay/operator/status",
    tag = "operations",
    description = "Return coarse counters and gates only. No session, seed, participant, credential, route, address, or user identity is exposed.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Redacted relay status; private no-store", body = RelayStatusResponseDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured relay operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Relay unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn relay_status_doc() {}

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
    path = "/v1/federation/health",
    tag = "federation",
    description = "Return coarse operator health and quarantine state for deliberately public origins. The response never contains resolved addresses, endpoint URLs, bearer credentials, or transport errors.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Address-free public-origin health summary", body = FederationHealthStatusDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Public-origin federation is disabled or the service is busy", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Health status retrieval exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_health_doc() {}

#[utoipa::path(
    post,
    path = "/v1/federation/objects/resolve",
    tag = "federation",
    description = "Ask the authoritative Rusty Weather server to try bounded deterministic failover across operator-approved public origins. The authority alone holds origin-scoped credentials, verifies exact request identity, descriptor object keys, expiry, hash, size, schema, provenance, and attribution, then re-signs and stages the object under its normal immutable contract. A one-hop header is rejected here to prevent recursion.",
    security(("bearer_auth" = [])),
    request_body = FederationProxyRequestDoc,
    responses(
        (status = 200, description = "Authority-signed immutable object manifest; private no-store", body = CommunityResolveResponseDoc),
        (status = 401, description = "BowEcho bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "No approved healthy origin has the exact object", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Request, hint, signature, object identity, provenance, schema, or one-hop boundary rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Per-principal federation quota exhausted", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Proxy disabled, killed, unavailable, or staging failed; caller must continue normal authority fallback", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Bounded authority work exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_proxy_resolve_doc() {}

#[utoipa::path(
    get,
    path = "/v1/federation/proxy/operator/status",
    tag = "operations",
    description = "Return only enabled, durable-persistence health, and runtime kill state. No principal, origin, URL, address, credential, request, or quota identity is exposed.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Coarse federation proxy status; private no-store", body = FederationProxyStatusResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured federation proxy operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Federation proxy control is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Status retrieval exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_proxy_operator_status_doc() {}

#[utoipa::path(
    post,
    path = "/v1/federation/proxy/operator/kill-switch",
    tag = "operations",
    description = "Durably engage or disengage public-origin proxy transfers. Engaging stops transport before persistence; disengaging never reopens transport until the atomic control state is durable.",
    security(("bearer_auth" = [])),
    request_body = FederationProxyKillSwitchRequest,
    responses(
        (status = 200, description = "Durably updated coarse status; private no-store", body = FederationProxyStatusResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured federation proxy operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Kill-switch schema rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Kill state could not be durably committed and proxy remains stopped", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Control update exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_proxy_kill_switch_doc() {}

#[utoipa::path(
    post,
    path = "/v1/federation/objects/resolve-local",
    tag = "federation",
    description = "Origin-to-authority one-hop resolver. Requires the dedicated origin token and exactly `X-Rusty-Federation-Hop: 1`; consults only this node's CAS, R2, and local published store and never invokes federation or Community relay.",
    security(("federation_origin_auth" = [])),
    request_body = CommunityResolveRequestDoc,
    responses(
        (status = 200, description = "Origin-signed exact immutable object manifest", body = CommunityResolveResponseDoc),
        (status = 401, description = "Dedicated federation-origin bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Exact object is not retained or locally computable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Canonical request or one-hop boundary rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Local-only resolver unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_local_resolve_doc() {}

#[utoipa::path(
    get,
    path = "/v1/federation/objects/{sha256}",
    tag = "federation",
    description = "Fetch bytes referenced by the immediately preceding one-hop signed manifest. Requires the dedicated origin token; ordinary BowEcho tokens are rejected.",
    security(("federation_origin_auth" = [])),
    params(("sha256" = String, Path, description = "Lowercase SHA-256 from the signed object manifest")),
    responses(
        (status = 200, description = "Verified immutable encoded object", content_type = "application/octet-stream", body = Vec<u8>),
        (status = 401, description = "Dedicated federation-origin bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Object is absent, expired, revoked, or invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Origin data quota exhausted", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Local immutable store unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn federation_local_object_doc() {}

#[utoipa::path(
    get,
    path = "/v1/origin-catalog/status",
    tag = "operations",
    description = "Return a coarse publication-gate state without paths, model names, run IDs, aliases, or validation errors.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Address- and identity-free origin publication status; private no-store", body = OriginCatalogHealthStatus),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Status worker is unavailable or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Status retrieval exceeded its deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn origin_catalog_status_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generation-replication/owner",
    tag = "generation-replication",
    description = "Return only this authenticated caller's replication-domain owner hash for constructing an owner-bound manifest. No bearer token or other owner is exposed.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Caller-specific owner identity; private no-store", body = ReplicationOwnerResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Advanced replication is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_owner_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generation-replication/capabilities",
    tag = "generation-replication",
    description = "Return actual runtime protocol ceilings, retention/upload lifetime, per-owner quota ceilings, and only this caller's usage. Global and other-owner utilization is omitted. Private no-store and available while the kill switch is engaged for safe planning/reconciliation.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Caller-specific replication planning contract; private no-store", body = RunGenerationOwnerCapabilitiesDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Advanced replication is disabled or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_capabilities_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generations",
    tag = "generation-replication",
    description = "List a deterministic bounded generation-id ordered page containing only this caller's live publications and tombstones. Active uploads use the exact upload-status route. Private no-store; another owner's identities never appear.",
    security(("bearer_auth" = [])),
    params(
        ("after" = Option<String>, Query, description = "Exact previous generation-id cursor"),
        ("limit" = Option<usize>, Query, description = "Bounded page size, maximum 100")
    ),
    responses(
        (status = 200, description = "Owner-isolated publication/tombstone page; private no-store", body = RunGenerationOwnerListPageDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Cursor or page limit rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Advanced replication is disabled or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_list_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generations",
    tag = "generation-replication",
    description = "Begin or idempotently resume one exact closed run-generation upload. Private WRF/ArWen requires explicit owner publication, confirmed redistribution rights, attribution, and modification notices.",
    security(("bearer_auth" = [])),
    request_body = BeginRunGenerationRequestDoc,
    responses(
        (status = 201, description = "Bounded resumable upload admitted", body = RunGenerationUploadStatusDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Generation identity conflicts with durable state", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Manifest body exceeds the configured JSON limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Manifest, owner, rights, provenance, attribution, schema, or limits failed closed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Owner or global quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_begin_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generations/{generation_id}",
    tag = "generation-replication",
    security(("bearer_auth" = [])),
    params(("generation_id" = String, Path, description = "Owner-bound canonical generation id")),
    responses(
        (status = 200, description = "Owned upload status; private no-store", body = RunGenerationUploadStatusDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this generation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Upload not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_status_doc() {}

#[utoipa::path(
    delete,
    path = "/v1/community/generations/{generation_id}",
    tag = "generation-replication",
    description = "Cancel one active owned upload and durably release its storage/count/concurrency reservation. Cancellation remains available while the kill switch is engaged and never publishes or authorizes data.",
    security(("bearer_auth" = [])),
    params(("generation_id" = String, Path, description = "Owner-bound canonical generation id")),
    responses(
        (status = 200, description = "Upload cancelled and reservation released; private no-store", body = CancelledRunGenerationDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this active upload", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Active upload not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Generation id rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled or durable cancellation unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_cancel_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generations/{generation_id}/publication",
    tag = "generation-replication",
    description = "Reconcile one exact caller-owned live publication or tombstone. Absent and other-owner identities are deliberately indistinguishable. Private no-store and available while killed.",
    security(("bearer_auth" = [])),
    params(("generation_id" = String, Path, description = "Owner-bound canonical generation id")),
    responses(
        (status = 200, description = "Exact owned publication or tombstone; private no-store", body = RunGenerationOwnerRecordDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "No caller-owned publication or tombstone exists", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Generation id rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled or durable state unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_publication_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generations/{generation_id}/missing",
    tag = "generation-replication",
    security(("bearer_auth" = [])),
    params(
        ("generation_id" = String, Path, description = "Owner-bound canonical generation id"),
        ("after" = Option<String>, Query, description = "Exact previous chunk SHA-256 cursor"),
        ("limit" = Option<usize>, Query, description = "Bounded page size, maximum 1024")
    ),
    responses(
        (status = 200, description = "Exact missing content-addressed chunks; private no-store", body = RunGenerationMissingPageDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this generation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Upload not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Cursor or page limit rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_missing_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generations/{generation_id}/chunks/{sha256}",
    tag = "generation-replication",
    description = "Upload one exact non-empty manifest-declared SHA-256 chunk. Content-Type must be exactly application/octet-stream.",
    security(("bearer_auth" = [])),
    params(
        ("generation_id" = String, Path, description = "Owner-bound canonical generation id"),
        ("sha256" = String, Path, description = "Lowercase exact chunk SHA-256")
    ),
    request_body(content = Vec<u8>, content_type = "application/octet-stream"),
    responses(
        (status = 204, description = "Exact chunk admitted; private no-store"),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this generation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Chunk exceeds its route-specific configured maximum", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Content-Type is absent or not exactly application/octet-stream", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Empty, malformed, undeclared, wrong-size, or wrong-hash chunk rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 429, description = "Monthly upload quota reached", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_upload_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generations/{generation_id}/finalize",
    tag = "generation-replication",
    security(("bearer_auth" = [])),
    params(("generation_id" = String, Path, description = "Owner-bound canonical generation id")),
    request_body = FinalizeRunGenerationRequestDoc,
    responses(
        (status = 200, description = "Exact durable publication reconciled after a prior successful finalize or lost response", body = PublishedRunGenerationDoc),
        (status = 201, description = "Deep-validated generation atomically published", body = PublishedRunGenerationDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this generation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "A scheduler-owned or different generation occupies the namespace", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Missing/tampered chunks or deep rw-store validation failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_finalize_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generations/{generation_id}/revoke",
    tag = "generation-replication",
    security(("bearer_auth" = [])),
    params(("generation_id" = String, Path, description = "Owner-bound canonical generation id")),
    request_body = RevokeRunGenerationRequestDoc,
    responses(
        (status = 200, description = "Durable owner-bound rights-withdrawal tombstone", body = RunGenerationTombstoneDoc),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller does not own this generation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Owned publication not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Revocation identity or confirmation rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Feature disabled, killed, busy, or unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_revoke_doc() {}

#[utoipa::path(
    get,
    path = "/v1/community/generation-replication/operator/status",
    tag = "operations",
    description = "Return authorized and pending-retirement counts, byte totals, health, and kill state only; never owner IDs, generations, paths, URLs, models, or runs.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Coarse replication status; private no-store", body = ReplicationStatusResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured replication operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Replication is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_operator_status_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generation-replication/operator/kill-switch",
    tag = "operations",
    security(("bearer_auth" = [])),
    request_body = ReplicationKillSwitchRequest,
    responses(
        (status = 200, description = "Durably updated coarse status; private no-store", body = ReplicationStatusResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured replication operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Kill-switch schema rejected", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Kill state could not be committed", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_kill_switch_doc() {}

#[utoipa::path(
    post,
    path = "/v1/community/generation-replication/operator/gc",
    tag = "operations",
    description = "Run one bounded collection pass that terminally expires due publications before retiring their local generations, then removes expired uploads, unreferenced chunks, stale candidates, and orphan signed manifests.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "Coarse bounded collection counts; private no-store", body = ReplicationGarbageCollectionResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Caller is not a configured replication operator", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Collection is unavailable or busy", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn generation_replication_gc_doc() {}

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
            "/v1/models/{model}/runs/latest",
            "/v1/point",
            "/v1/window",
            "/v1/analytics/spatial-series",
            "/v1/analytics/temporal-grid",
            "/v1/jobs/temporal-grid",
            "/v1/jobs/{id}",
            "/v1/artifacts/{hash}/{file}",
            "/v1/origin-catalog/status",
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
        let origin_status = operation(&value, "/v1/origin-catalog/status", "get");
        assert_eq!(
            origin_status["security"][0]["bearer_auth"],
            serde_json::json!([])
        );
        assert_response_ref(
            &value,
            "/v1/origin-catalog/status",
            "get",
            "#/components/schemas/OriginCatalogHealthStatus",
            false,
        );
        let latest = operation(&value, "/v1/models/{model}/runs/latest", "get");
        assert_eq!(latest["security"][0]["bearer_auth"], serde_json::json!([]));
        assert!(latest["responses"]["401"].is_object());
        assert!(
            latest["description"]
                .as_str()
                .is_some_and(|description| description.contains("no-store"))
        );
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
    fn generation_replication_contract_is_complete_authenticated_and_default_safe() {
        let value = serde_json::to_value(document()).unwrap();
        for (path, method) in [
            ("/v1/community/generation-replication/owner", "get"),
            ("/v1/community/generation-replication/capabilities", "get"),
            ("/v1/community/generations", "get"),
            ("/v1/community/generations", "post"),
            ("/v1/community/generations/{generation_id}", "get"),
            ("/v1/community/generations/{generation_id}", "delete"),
            (
                "/v1/community/generations/{generation_id}/publication",
                "get",
            ),
            ("/v1/community/generations/{generation_id}/missing", "get"),
            (
                "/v1/community/generations/{generation_id}/chunks/{sha256}",
                "post",
            ),
            ("/v1/community/generations/{generation_id}/finalize", "post"),
            ("/v1/community/generations/{generation_id}/revoke", "post"),
            (
                "/v1/community/generation-replication/operator/status",
                "get",
            ),
            (
                "/v1/community/generation-replication/operator/kill-switch",
                "post",
            ),
            ("/v1/community/generation-replication/operator/gc", "post"),
        ] {
            let operation = operation(&value, path, method);
            assert_eq!(
                operation["security"][0]["bearer_auth"],
                serde_json::json!([])
            );
            assert!(operation["responses"]["401"].is_object());
            assert!(operation["responses"]["503"].is_object());
        }
        let upload = operation(
            &value,
            "/v1/community/generations/{generation_id}/chunks/{sha256}",
            "post",
        );
        for status in ["413", "415", "422"] {
            assert!(upload["responses"][status].is_object());
        }
        assert_eq!(
            upload["requestBody"]["content"]["application/octet-stream"]["schema"]["type"],
            "array"
        );
        let status_schema = &value["components"]["schemas"]["ReplicationStatusResponse"];
        let serialized = serde_json::to_string(status_schema).unwrap();
        for forbidden in ["owner_principal", "generation_id", "path", "source_url"] {
            assert!(!serialized.contains(forbidden));
        }
        let finalize = operation(
            &value,
            "/v1/community/generations/{generation_id}/finalize",
            "post",
        );
        assert!(finalize["responses"]["200"].is_object());
        assert!(finalize["responses"]["201"].is_object());
        let exact = operation(
            &value,
            "/v1/community/generations/{generation_id}/publication",
            "get",
        );
        assert!(exact["responses"]["404"].is_object());
        assert!(exact["responses"].get("403").is_none());
        let capabilities = operation(
            &value,
            "/v1/community/generation-replication/capabilities",
            "get",
        );
        assert!(capabilities["responses"]["200"].is_object());
        let advertised_limits =
            &value["components"]["schemas"]["RunGenerationAdvertisedLimitsDoc"]["properties"];
        for required in [
            "maximum_chunk_bytes",
            "maximum_retention_seconds",
            "upload_ttl_seconds",
        ] {
            assert!(advertised_limits[required].is_object());
        }
        assert!(value["components"]["schemas"]["RunGenerationOwnerQuotaDoc"]["properties"]
            ["maximum_storage_bytes"]
            .is_object());
        assert!(value["components"]["schemas"]["RunGenerationOwnerUsageDoc"]["properties"]
            ["monthly_accepted_upload_bytes"]
            .is_object());
    }

    #[test]
    fn federation_discovery_and_proxy_are_authenticated_and_concretely_documented() {
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
        let health = operation(&value, "/v1/federation/health", "get");
        assert_eq!(health["security"][0]["bearer_auth"], serde_json::json!([]));
        assert!(health["responses"]["401"].is_object());
        assert!(value["paths"]["/v1/federation/health"]["post"].is_null());
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
        assert_response_ref(
            &value,
            "/v1/federation/health",
            "get",
            "#/components/schemas/FederationHealthStatusDoc",
            false,
        );
        let proxy = operation(&value, "/v1/federation/objects/resolve", "post");
        assert_eq!(proxy["security"][0]["bearer_auth"], serde_json::json!([]));
        for status in ["401", "404", "422", "429", "503", "504"] {
            assert!(proxy["responses"][status].is_object());
        }
        assert_eq!(
            proxy["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/FederationProxyRequestDoc"
        );
        for (path, method) in [
            ("/v1/federation/proxy/operator/status", "get"),
            ("/v1/federation/proxy/operator/kill-switch", "post"),
        ] {
            let control = operation(&value, path, method);
            assert_eq!(control["security"][0]["bearer_auth"], serde_json::json!([]));
            for status in ["401", "403", "503", "504"] {
                assert!(control["responses"][status].is_object());
            }
        }
        assert!(
            operation(
                &value,
                "/v1/federation/proxy/operator/kill-switch",
                "post"
            )["responses"]["422"]
                .is_object()
        );
        let status_schema = &value["components"]["schemas"]["FederationProxyStatusResponse"];
        let serialized = serde_json::to_string(status_schema).unwrap();
        for forbidden in [
            "principal",
            "origin_id",
            "url",
            "address",
            "credential",
            "quota",
        ] {
            assert!(!serialized.contains(forbidden));
        }
        for (path, method) in [
            ("/v1/federation/objects/resolve-local", "post"),
            ("/v1/federation/objects/{sha256}", "get"),
        ] {
            let local = operation(&value, path, method);
            assert_eq!(
                local["security"][0]["federation_origin_auth"],
                serde_json::json!([])
            );
            assert!(local["responses"]["401"].is_object());
            let serialized = serde_json::to_string(local).unwrap();
            assert!(!serialized.contains("bearer_token_file"));
            assert!(!serialized.contains("https_base_url"));
        }
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
                "/v1/models/{model}/runs/latest",
                "get",
                "#/components/schemas/RunDescriptorResponse",
                false,
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

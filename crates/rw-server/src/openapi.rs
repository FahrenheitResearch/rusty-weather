#![allow(dead_code)] // Utoipa consumes the document-only handler stubs via its derive macro.

use utoipa::openapi::OpenApi as OpenApiDocument;
use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use rw_nexrad_storm::{
    AzimuthRange, Compression, CoordinateProvenance, ForecastPoint, GeographicPoint,
    HeightQualifier, NexradStormProduct, ProductIdentity, ProductProvenance, QualifiedHeight,
    RadarRelativePosition, SiteIdentity, SiteIdentitySource, SpecificationReferenceOwned,
    StormMotion, StormStructureCell, StormStructureProduct, StormTrackingProduct, SuppliedGeometry,
    TrackPoint, TrackedStormCell, TransportIdentity, ValidationNotice,
};
use rw_ops_protocol::{
    ContourRing, GeoPoint, ModelInputSource, StormCell, StormCellFrame, StormMethodCatalog,
    StormMethodIdentity, StormMethodKind, StormModelBackend, StormModelInput, StormModelManifest,
    StormSource,
};

use crate::federation_proxy::{FederationProxyKillSwitchRequest, FederationProxyStatusResponse};
use crate::generation_replication::{
    ReplicationGarbageCollectionResponse, ReplicationKillSwitchRequest, ReplicationOwnerResponse,
    ReplicationStatusResponse,
};
use crate::mrms_ingest::{
    MrmsIngestStatus, MrmsProductPhase, MrmsProductStatus, MrmsRefreshResponse,
    MrmsStoredFrameStatus,
};
use crate::nexrad_level2_ingest::{
    NexradLevel2IngestStatus, NexradLevel2RefreshResponse, NexradLevel2SitePhase,
    NexradLevel2SiteStatus, NexradLevel2SourceObjectStatus, NexradLevel2StoredFrameStatus,
};
use crate::origin_catalog::OriginCatalogHealthStatus;
use crate::problem::ProblemDetails;
use crate::routes::{
    ApiIngestCapabilityLimitation, ApiIntervalSupport, ApiMissingPolicy,
    ApiTemporalCapabilityBasis, ApiTemporalOperation, ApiTemporalReducer, ApiTemporalSemantics,
    ApiTemporalValueClass, ApiTemporalVerticalSelection, ApiTemporalWindow, ApiTimeExpectation,
    CoordinateRequest, GeographicVerticalApiSelection, GeographicWindowApiRequest, HealthResponse,
    ModelCapabilityResponse, PointQueryRequest, PointsRequest, ProductCapabilityResponse,
    ProfileApiRequest, ProfileCycleApiRequest, ProviderAttributionResponse,
    SpatialSeriesApiRequest, TemporalGridApiRequest, VariableCapabilityResponse,
    VariableTemporalCapabilityResponse, VersionResponse, WindowApiRequest,
};
use crate::{ArtifactRef, JobStatus, JobView};

/// Sanitized union of resolved providers represented in a run.
#[derive(utoipa::ToSchema)]
struct SourceProvenanceResponse {
    provider: String,
    forecast_producer: Option<String>,
    licensing_publisher: Option<String>,
    transport_provider: Option<String>,
    transport_is_mirror: bool,
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
#[serde(rename_all = "snake_case")]
enum ObservationKindDoc {
    Satellite,
    SimulatedSatellite,
    Mrms,
    Radar,
    RadarMosaic,
    SimulatedRadar,
    Generated,
}

#[derive(utoipa::ToSchema)]
struct ObservationCapabilitiesResponseDoc {
    /// `rw-server.observation-capabilities.v1`.
    schema: String,
    satellite_store_delivery: bool,
    simsat_store_delivery: bool,
    arbitrary_generated_grid_import: bool,
    mrms_latest_ingest: bool,
    nexrad_level2_ingest: bool,
    single_site_composite: bool,
    multi_source_mosaic: bool,
    wrf_composite_reflectivity: bool,
    wrf_pressure_level_reflectivity: bool,
    wrf_echo_top: bool,
    wrf_vil: bool,
    wrf_virtual_radar_ppi: bool,
    maximum_grid_cells: usize,
    maximum_mosaic_inputs: usize,
    maximum_level2_upload_bytes: usize,
    maximum_generated_upload_bytes: usize,
    binary_grid_content_type: String,
    binary_plane_content_type: String,
    display_metadata: bool,
    curvilinear_grid_mesh: bool,
    non_finite_transparency: bool,
}

#[derive(utoipa::ToSchema)]
struct ObservationRunSummaryDoc {
    kind: ObservationKindDoc,
    run: RunDescriptorResponse,
    variable_count: usize,
}

#[derive(utoipa::ToSchema)]
struct ObservationCatalogResponseDoc {
    /// `rw-server.observation-catalog.v1`.
    schema: String,
    runs: Vec<ObservationRunSummaryDoc>,
    /// True only when the caller supplied `limit` and more runs existed.
    truncated: bool,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObservationValueSemanticsDoc {
    Reflectivity,
    RadialVelocity,
    SpectrumWidth,
    DifferentialReflectivity,
    CorrelationCoefficient,
    DifferentialPhase,
    SpecificDifferentialPhase,
    HydrometeorClassification,
    EchoTop,
    VerticallyIntegratedLiquid,
    BrightnessTemperature,
    Reflectance,
    Precipitation,
    Rgba,
    GenericScalar,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObservationInterpolationDoc {
    Linear,
    Nearest,
    CircularDegrees,
    VelocityFoldAware,
}

#[derive(utoipa::ToSchema)]
struct ObservationDisplayHintDoc {
    semantics: ObservationValueSemanticsDoc,
    palette: String,
    interpolation: ObservationInterpolationDoc,
    transparent_non_finite: bool,
    preferred_range: Option<[f32; 2]>,
    discontinuity_threshold: Option<f32>,
}

#[derive(utoipa::ToSchema)]
struct ObservationVariableSummaryDoc {
    name: String,
    units: String,
    kind: String,
    selector: serde_json::Value,
    display: ObservationDisplayHintDoc,
    available_slots: Vec<u16>,
}

#[derive(utoipa::ToSchema)]
struct ObservationFramesResponseDoc {
    /// `rw-server.observation-frames.v1`.
    schema: String,
    kind: ObservationKindDoc,
    run: RunDescriptorResponse,
    frames: Vec<TimePointResponse>,
    variables: Vec<ObservationVariableSummaryDoc>,
}

/// `RWOBGRID` binary grid geometry. See the observation capabilities response
/// for the versioned media type.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
struct ObservationGridBinaryDoc(Vec<u8>);

/// `RWOBF32` calibrated observation plane. Missing/non-coverage cells are
/// encoded as non-finite values and must remain transparent.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
struct ObservationPlaneBinaryDoc(Vec<u8>);

/// One complete model forecast plane in the same versioned `RWOBF32` f32
/// container as observation planes: little-endian header (`RWOBF32\0`, version,
/// `nx`, `ny`, `valid_unix`, variable/unit lengths, reserved word), the
/// variable and unit strings, then exactly `nx * ny` row-major `f32` cells.
/// Non-finite cells are absent values and must remain transparent.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
struct ModelPlaneBinaryDoc(Vec<u8>);

/// Caller-supplied NEXRAD Archive-II volume.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
struct NexradLevel2ArchiveDoc(Vec<u8>);

#[derive(utoipa::ToSchema)]
struct MrmsMessageSelectorDoc {
    discipline: Option<u8>,
    parameter_category: Option<u8>,
    parameter_number: Option<u8>,
    level_type: Option<u8>,
    message_index: Option<usize>,
}

#[derive(utoipa::ToSchema)]
struct MrmsIngestRequestDoc {
    product: String,
    collection: Option<String>,
    variable: Option<String>,
    units: Option<String>,
    #[serde(default)]
    selector: MrmsMessageSelectorDoc,
}

#[derive(utoipa::ToSchema)]
struct StoredObservationPlaneRefDoc {
    model: String,
    run: String,
    storage_slot: u16,
    variable: String,
}

#[derive(utoipa::ToSchema)]
struct GeographicObservationGridSpecDoc {
    west_longitude: f64,
    south_latitude: f64,
    east_longitude: f64,
    north_latitude: f64,
    resolution_km: f64,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObservationMosaicMethodDoc {
    Maximum,
    Mean,
    Latest,
}

#[derive(utoipa::ToSchema)]
struct RadarMosaicRequestDoc {
    inputs: Vec<StoredObservationPlaneRefDoc>,
    target: GeographicObservationGridSpecDoc,
    method: ObservationMosaicMethodDoc,
    collection: Option<String>,
    product: Option<String>,
    variable: Option<String>,
    units: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct StoredObservationVariableRefDoc {
    model: String,
    run: String,
    storage_slot: u16,
    variable: String,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObservationBeamAggregationDoc {
    Center,
    Maximum,
    Mean,
}

#[derive(utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SimulatedRadarOperationDoc {
    PassThrough,
    CompositeMax,
    PressureLevel {
        level_hpa: u16,
    },
    EchoTop {
        threshold_dbz: f32,
        height_variable: String,
    },
    Vil {
        height_variable: String,
    },
    BeamPpi {
        height_variable: String,
        radar_latitude: f64,
        radar_longitude: f64,
        radar_elevation_m: f64,
        tilt_deg: f64,
        #[serde(default)]
        beam_width_deg: f64,
        #[serde(default)]
        earth_radius_factor: f64,
        #[serde(default)]
        max_range_km: f64,
        #[serde(default)]
        aggregation: ObservationBeamAggregationDoc,
        minimum_dbz: Option<f32>,
    },
}

#[derive(utoipa::ToSchema)]
struct SimulatedRadarRequestDoc {
    source: StoredObservationVariableRefDoc,
    operation: SimulatedRadarOperationDoc,
    collection: Option<String>,
    product: Option<String>,
    variable: Option<String>,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ObservationFamilyDoc {
    Satellite,
    Mrms,
    Radar,
    RadarMosaic,
    SimulatedRadar,
    SimulatedSatellite,
    Generated,
}

#[derive(utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct GeneratedObservationPlaneRequestDoc {
    name: String,
    units: String,
    #[serde(default)]
    selector: serde_json::Value,
    /// One value per grid cell. Null means missing/no coverage and is stored as NaN.
    values: Vec<Option<f32>>,
}

#[derive(utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
struct GeneratedObservationFrameRequestDoc {
    family: ObservationFamilyDoc,
    collection: String,
    product: String,
    valid_unix: i64,
    nx: usize,
    ny: usize,
    /// Row-major cell-center latitudes. Null means the coordinate is unavailable.
    latitudes: Vec<Option<f32>>,
    /// Row-major cell-center longitudes. Null means the coordinate is unavailable.
    longitudes: Vec<Option<f32>>,
    projection: Option<GridProjectionResponse>,
    planes: Vec<GeneratedObservationPlaneRequestDoc>,
    /// Exact upstream/provider identity; empty means the caller supplied none.
    #[serde(default)]
    provenance_provider: String,
    #[serde(default)]
    provenance_roles: Vec<String>,
    #[serde(default)]
    provenance_products: Vec<String>,
}

/// Artifact schema written when an accepted observation job succeeds.
#[derive(utoipa::ToSchema)]
struct StoredObservationFrameRefDoc {
    schema: String,
    model: String,
    run: String,
    storage_slot: u16,
    valid_unix: i64,
    variables: Vec<String>,
    grid_hash: String,
    frame_file: String,
    bytes: u64,
    duplicate: bool,
}

/// Immutable JSON artifact emitted by one of the currently registered job
/// submission families.
#[derive(utoipa::ToSchema)]
#[serde(untagged)]
enum JobArtifactDoc {
    TemporalGrid(Box<TemporalGridResponse>),
    Observation(StoredObservationFrameRefDoc),
}

#[derive(utoipa::ToSchema)]
struct SatellitePlatformDescriptorDoc {
    id: String,
    title: String,
    role: String,
}

#[derive(utoipa::ToSchema)]
struct SatelliteSectorDescriptorDoc {
    id: String,
    title: String,
    cadence_seconds: u64,
    default_poll_seconds: u64,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum SatelliteProductCategoryDoc {
    Favorites,
    Visible,
    Infrared,
    WaterVapor,
    RgbComposite,
    Fire,
    Advanced,
}

#[derive(utoipa::ToSchema)]
struct SatelliteProductDescriptorDoc {
    id: String,
    title: String,
    description: String,
    category: SatelliteProductCategoryDoc,
    required_channels: Vec<u8>,
    base_channel: u8,
    native_resolution_km: f32,
    daylight_only: bool,
    enhancement: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct SatelliteEnhancementStopDoc {
    value: f32,
    rgb: [u8; 3],
}

#[derive(utoipa::ToSchema)]
struct SatelliteEnhancementDescriptorDoc {
    id: String,
    title: String,
    value_units: String,
    stops: Vec<SatelliteEnhancementStopDoc>,
}

#[derive(utoipa::ToSchema)]
struct SatelliteCatalogResponseDoc {
    /// `rw-server.satellite-catalog.v3`.
    schema: String,
    platforms: Vec<SatellitePlatformDescriptorDoc>,
    sectors: Vec<SatelliteSectorDescriptorDoc>,
    products: Vec<SatelliteProductDescriptorDoc>,
    enhancements: Vec<SatelliteEnhancementDescriptorDoc>,
    native_source_archive: bool,
    full_disk_native_window_reads: bool,
    latest_frame_alias: String,
    maximum_tile_zoom: u8,
    tile_size: u32,
    renderer_recipe: String,
    geocolor_note: String,
}

#[derive(utoipa::ToSchema)]
struct SatelliteFrameDescriptorDoc {
    id: String,
    /// BLAKE3 content identity of the complete required-channel native frame.
    source_revision: String,
    scan_start_unix: i64,
    scan_end_unix: i64,
    channels: Vec<u8>,
}

#[derive(utoipa::ToSchema)]
struct SatelliteFramesResponseDoc {
    /// `rw-server.satellite-frames.v3`.
    schema: String,
    platform: String,
    sector: String,
    product: SatelliteProductDescriptorDoc,
    cadence_seconds: u64,
    frames: Vec<SatelliteFrameDescriptorDoc>,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct SatelliteTileJsonResponseDoc {
    tilejson: String,
    name: String,
    description: String,
    scheme: String,
    tiles: Vec<String>,
    minzoom: u8,
    maxzoom: u8,
    bounds: [f64; 4],
    attribution: String,
    tile_size: u32,
    renderer_recipe: String,
    frame: String,
    source_revision: String,
}

#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
struct SatellitePngTileDoc(Vec<u8>);

#[derive(utoipa::ToSchema)]
struct SatellitePrewarmWorkKeyDoc {
    renderer_recipe: String,
    platform: String,
    sector: String,
    product: String,
    minute_frame_id: String,
    source_revision: String,
    plan_digest: String,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum SatellitePrewarmPhaseDoc {
    Disabled,
    WaitingForSource,
    Reconciling,
    Rendering,
    Ready,
    Degraded,
    Stopped,
}

#[derive(utoipa::ToSchema)]
struct SatellitePrewarmStatusDoc {
    /// `rw-server.satellite-prewarm-status.v1`.
    schema: String,
    enabled: bool,
    ready: bool,
    phase: SatellitePrewarmPhaseDoc,
    active_work: Option<SatellitePrewarmWorkKeyDoc>,
    configured_sources: usize,
    waiting_sources: usize,
    reconcile_count: u64,
    planned_tiles: u64,
    completed_tiles: u64,
    failed_tiles: u64,
    completed_product_frames: u64,
    last_reconcile_unix_ms: Option<i64>,
    last_success_unix_ms: Option<i64>,
    last_error: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct StoredStormGridRefDoc {
    model: String,
    run: String,
    /// Exact immutable generation required by this request.
    expected_snapshot_id: String,
    /// Exact native-grid identity required by this request.
    expected_grid_hash: String,
    storage_slot: u16,
    variable: String,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ConnectivityRequestDoc {
    Four,
    Eight,
}

#[derive(utoipa::ToSchema)]
struct DetectionRequestDoc {
    #[serde(default)]
    threshold_dbz: f32,
    #[serde(default)]
    minimum_valid_dbz: f32,
    #[serde(default)]
    maximum_valid_dbz: f32,
    #[serde(default)]
    minimum_gate_count: usize,
    #[serde(default)]
    minimum_area_km2: f64,
    #[serde(default)]
    connectivity: ConnectivityRequestDoc,
}

#[derive(utoipa::ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StormMethodRequestDoc {
    /// Prefer a compatible active compiled-Rust model, then honestly fall
    /// back to deterministic contours when no such model is executable.
    Auto {
        #[serde(default)]
        deterministic: DetectionRequestDoc,
    },
    Deterministic {
        #[serde(default)]
        config: DetectionRequestDoc,
    },
    MachineLearning {
        model_id: String,
        model_version: Option<String>,
        #[serde(default)]
        input_variables: std::collections::BTreeMap<String, String>,
        supplied_mask_variable: Option<String>,
    },
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum StormResponseFormatDoc {
    Canonical,
    Geojson,
}

#[derive(utoipa::ToSchema)]
struct StormCellsRequestDoc {
    /// Must be `rw.server.storm-cells-request.v1`.
    schema: String,
    grid: StoredStormGridRefDoc,
    /// Scientific source identity that must agree with the stored grid.
    source: StormSource,
    method: StormMethodRequestDoc,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum StormDistributionGrantDoc {
    NodeOnly,
    CompanyInternal,
    Public,
}

#[derive(utoipa::ToSchema)]
struct StormModelUsePolicyDoc {
    artifact_distribution: StormDistributionGrantDoc,
    derived_output_distribution: StormDistributionGrantDoc,
    /// Attribution downstream clients must preserve.
    required_attribution: String,
    /// License, contract, or internal approval used for auditing.
    rights_reference: String,
}

#[derive(utoipa::ToSchema)]
struct StormModelStatusDoc {
    manifest: StormModelManifest,
    policy: StormModelUsePolicyDoc,
    enabled: bool,
    active: bool,
    executable_on_this_node: bool,
    /// `compiled_rust_backend`, `stored_probability_mask`, or
    /// `executor_not_compiled`.
    execution_mode: String,
}

#[derive(utoipa::ToSchema)]
struct StormModelCatalogDoc {
    /// `rw.server.storm-model-catalog.v1`.
    schema: String,
    generated_at_unix_ms: i64,
    models: Vec<StormModelStatusDoc>,
}

#[derive(utoipa::ToSchema)]
struct StormDiskCacheHealthDoc {
    ready: bool,
    cache_revision: String,
    entries: u64,
    bytes: u64,
    recovered_staging_entries: u64,
    recovered_invalid_entries: u64,
    disk_hits: u64,
    atomic_store_writes: u64,
    last_hit_unix_ms: Option<i64>,
    last_store_unix_ms: Option<i64>,
    last_error_unix_ms: Option<i64>,
    last_error: Option<String>,
}

#[derive(utoipa::ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum StormCacheRetentionDoc {
    Bounded { frames_per_source: usize },
    Unlimited,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum StormPrewarmPhaseDoc {
    Disabled,
    Starting,
    WaitingForSource,
    Reconciling,
    Ready,
    Degraded,
    Stopped,
}

#[derive(utoipa::ToSchema)]
struct StormPrewarmSourceStatusDoc {
    product: String,
    variable: String,
    model: String,
    run: String,
    snapshot_id: String,
    grid_hash: String,
    storage_slot: u16,
    valid_at_unix_ms: i64,
    cache_key: String,
    method: String,
    cache_revision: String,
}

#[derive(utoipa::ToSchema)]
struct StormPrewarmStatusDoc {
    schema: String,
    enabled: bool,
    ready: bool,
    phase: StormPrewarmPhaseDoc,
    checked_at_unix_ms: i64,
    cache_revision: String,
    backfill_frames: usize,
    retention: StormCacheRetentionDoc,
    trigger_epoch: u64,
    coalesced_triggers: u64,
    in_flight: bool,
    restart_reconciled: bool,
    last_attempt_unix_ms: Option<i64>,
    last_success_unix_ms: Option<i64>,
    last_source_valid_unix_ms: Option<i64>,
    stale: bool,
    reconciled_frames: u64,
    latest_source: Option<StormPrewarmSourceStatusDoc>,
    last_error_unix_ms: Option<i64>,
    last_error: Option<String>,
}

#[derive(utoipa::ToSchema)]
struct StormSourceLinkageStatusDoc {
    source: String,
    available: bool,
    /// Whether geometry is authoritative, derived, and/or absent upstream.
    geometry: String,
    /// Human-readable provenance and capability limitation.
    detail: String,
}

#[derive(utoipa::ToSchema)]
struct StormServiceStatusDoc {
    /// `rw.server.storm-service-status.v1`.
    schema: String,
    generated_at_unix_ms: i64,
    ready: bool,
    stored_source_execution: bool,
    direct_client_grid_uploads: bool,
    exact_frame_single_flight: bool,
    frame_cache_scope: String,
    frame_cache_revision: String,
    frame_cache_max_bytes: u64,
    durable_cache: Option<StormDiskCacheHealthDoc>,
    prewarm: StormPrewarmStatusDoc,
    source_linkage: Vec<StormSourceLinkageStatusDoc>,
}

#[derive(utoipa::ToSchema)]
struct StormGeoJsonGeometryDoc {
    /// `Polygon` or `MultiPolygon`.
    r#type: String,
    /// RFC 7946 longitude/latitude coordinate arrays, including contour holes.
    coordinates: serde_json::Value,
}

#[derive(utoipa::ToSchema)]
struct StormGeoJsonPropertiesDoc {
    cell_id: String,
    track_id: Option<String>,
    centroid: GeoPoint,
    area_km2: f64,
    maximum_reflectivity_dbz: Option<f64>,
    echo_top_m: Option<f64>,
    confidence: Option<f64>,
    attributes: std::collections::BTreeMap<String, String>,
}

#[derive(utoipa::ToSchema)]
struct StormGeoJsonFeatureDoc {
    r#type: String,
    id: String,
    geometry: StormGeoJsonGeometryDoc,
    properties: StormGeoJsonPropertiesDoc,
}

#[derive(utoipa::ToSchema)]
struct StormGeoJsonFeatureCollectionDoc {
    r#type: String,
    /// `rw.ops.storm-cell-geojson.v1`.
    schema: String,
    generated_at_unix_ms: i64,
    source: StormSource,
    method: StormMethodIdentity,
    partial: bool,
    warnings: Vec<String>,
    features: Vec<StormGeoJsonFeatureDoc>,
}

#[derive(utoipa::ToSchema)]
struct NexradLevel3StormDecodeRequestDoc {
    /// Must be `rw.server.nexrad-level3-storm-decode-request.v1`.
    schema: String,
    /// Optional four-character radar identifier used only when transport
    /// metadata does not identify the site.
    site_hint: Option<String>,
    /// Canonical base64 for one supplied Level III message 58 or 62 product.
    product_base64: String,
}

#[derive(utoipa::ToSchema)]
struct NexradLevel3StormDecodeResponseDoc {
    /// `rw.ops.nexrad-level3-storm-product.v1`.
    schema: String,
    generated_at_unix_ms: i64,
    method: StormMethodIdentity,
    product: NexradStormProduct,
    /// Explicit statement of geometry supplied by NOAA and geometry absent
    /// from the product; downstream displays must retain it.
    geometry_statement: String,
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
struct TimeRangeResponse {
    start_unix: Option<i64>,
    end_unix: Option<i64>,
}

#[derive(utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
enum ProfileCycleSampleStatusResponse {
    Complete,
    Partial,
    Gap,
}

#[derive(utoipa::ToSchema)]
struct ProfileCycleSampleResponse {
    time: TimePointResponse,
    source_provenance: Vec<SourceProvenanceResponse>,
    status: ProfileCycleSampleStatusResponse,
    variables: Vec<PressureProfileResponse>,
    missing_variables: Vec<String>,
    surface_samples: Vec<ProfileSurfaceSampleResponse>,
    missing_surface_variables: Vec<String>,
}

#[derive(utoipa::ToSchema)]
struct ProfileSurfaceSampleResponse {
    variable: String,
    units: String,
    value: Option<f32>,
}

#[derive(utoipa::ToSchema)]
struct ProfileCycleResponse {
    run: RunDescriptorResponse,
    point: GridPointResponse,
    requested_variables: Vec<String>,
    requested_surface_variables: Vec<String>,
    requested_time: TimeRangeResponse,
    missing_policy: ApiMissingPolicy,
    samples: Vec<ProfileCycleSampleResponse>,
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
        description = "Self-hosted model and observation catalogs, exact-time queries and analytics, native satellite delivery, bounded jobs, controlled sharing, and authenticated storm-analysis operations."
    ),
    paths(
        live_doc,
        ready_doc,
        version_doc,
        openapi_doc,
        observation_capabilities_doc,
        observation_catalog_doc,
        observation_frames_doc,
        observation_grid_doc,
        observation_plane_doc,
        submit_mrms_latest_doc,
        submit_nexrad_level2_doc,
        submit_radar_mosaic_doc,
        submit_simulated_radar_doc,
        submit_generated_observation_doc,
        mrms_ingest_status_doc,
        mrms_ingest_refresh_doc,
        nexrad_level2_ingest_status_doc,
        nexrad_level2_ingest_refresh_doc,
        satellite_catalog_doc,
        satellite_prewarm_status_doc,
        satellite_frames_doc,
        satellite_tilejson_doc,
        satellite_legacy_tile_doc,
        satellite_versioned_tile_doc,
        satellite_revisioned_tile_doc,
        models_doc,
        runs_doc,
        latest_run_doc,
        run_doc,
        variables_doc,
        model_plane_doc,
        point_doc,
        points_doc,
        profile_doc,
        profile_cycle_doc,
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
        storm_status_doc,
        storm_methods_doc,
        storm_models_doc,
        storm_cells_doc,
        nexrad_level3_storm_decode_doc,
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
        ProfileCycleApiRequest,
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
        ObservationKindDoc,
        ObservationCapabilitiesResponseDoc,
        ObservationRunSummaryDoc,
        ObservationCatalogResponseDoc,
        ObservationValueSemanticsDoc,
        ObservationInterpolationDoc,
        ObservationDisplayHintDoc,
        ObservationVariableSummaryDoc,
        ObservationFramesResponseDoc,
        ObservationGridBinaryDoc,
        ObservationPlaneBinaryDoc,
        ModelPlaneBinaryDoc,
        NexradLevel2ArchiveDoc,
        MrmsMessageSelectorDoc,
        MrmsIngestRequestDoc,
        StoredObservationPlaneRefDoc,
        GeographicObservationGridSpecDoc,
        ObservationMosaicMethodDoc,
        RadarMosaicRequestDoc,
        StoredObservationVariableRefDoc,
        ObservationBeamAggregationDoc,
        SimulatedRadarOperationDoc,
        SimulatedRadarRequestDoc,
        ObservationFamilyDoc,
        GeneratedObservationPlaneRequestDoc,
        GeneratedObservationFrameRequestDoc,
        StoredObservationFrameRefDoc,
        JobArtifactDoc,
        SatellitePlatformDescriptorDoc,
        SatelliteSectorDescriptorDoc,
        SatelliteProductCategoryDoc,
        SatelliteProductDescriptorDoc,
        SatelliteEnhancementStopDoc,
        SatelliteEnhancementDescriptorDoc,
        SatelliteCatalogResponseDoc,
        SatelliteFrameDescriptorDoc,
        SatelliteFramesResponseDoc,
        SatelliteTileJsonResponseDoc,
        SatellitePngTileDoc,
        SatellitePrewarmWorkKeyDoc,
        SatellitePrewarmPhaseDoc,
        SatellitePrewarmStatusDoc,
        GridPointResponse,
        PointVariableSeriesResponse,
        PointSeriesResponse,
        PressureProfileResponse,
        ProfileResponse,
        TimeRangeResponse,
        ProfileCycleSampleStatusResponse,
        ProfileSurfaceSampleResponse,
        ProfileCycleSampleResponse,
        ProfileCycleResponse,
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
        MrmsProductPhase,
        MrmsStoredFrameStatus,
        MrmsProductStatus,
        MrmsIngestStatus,
        MrmsRefreshResponse,
        NexradLevel2SitePhase,
        NexradLevel2SourceObjectStatus,
        NexradLevel2StoredFrameStatus,
        NexradLevel2SiteStatus,
        NexradLevel2IngestStatus,
        NexradLevel2RefreshResponse,
        StoredStormGridRefDoc,
        ConnectivityRequestDoc,
        DetectionRequestDoc,
        StormMethodRequestDoc,
        StormResponseFormatDoc,
        StormCellsRequestDoc,
        StormDistributionGrantDoc,
        StormModelUsePolicyDoc,
        StormModelStatusDoc,
        StormModelCatalogDoc,
        StormDiskCacheHealthDoc,
        StormCacheRetentionDoc,
        StormPrewarmPhaseDoc,
        StormPrewarmSourceStatusDoc,
        StormPrewarmStatusDoc,
        StormSourceLinkageStatusDoc,
        StormServiceStatusDoc,
        StormGeoJsonGeometryDoc,
        StormGeoJsonPropertiesDoc,
        StormGeoJsonFeatureDoc,
        StormGeoJsonFeatureCollectionDoc,
        NexradLevel3StormDecodeRequestDoc,
        NexradLevel3StormDecodeResponseDoc,
        GeoPoint,
        StormSource,
        StormMethodKind,
        StormMethodIdentity,
        ContourRing,
        StormCell,
        StormCellFrame,
        StormMethodCatalog,
        StormModelBackend,
        ModelInputSource,
        StormModelInput,
        StormModelManifest,
        NexradStormProduct,
        ProductIdentity,
        ValidationNotice,
        SiteIdentity,
        SiteIdentitySource,
        TransportIdentity,
        Compression,
        ProductProvenance,
        SpecificationReferenceOwned,
        SuppliedGeometry,
        GeographicPoint,
        RadarRelativePosition,
        StormTrackingProduct,
        TrackedStormCell,
        TrackPoint,
        ForecastPoint,
        CoordinateProvenance,
        AzimuthRange,
        StormMotion,
        StormStructureProduct,
        StormStructureCell,
        QualifiedHeight,
        HeightQualifier,
    )),
    modifiers(&SecurityAddon),
    tags(
        (name = "health", description = "Process and store readiness"),
        (name = "catalog", description = "Models, runs, and variables"),
        (name = "observations", description = "Exact stored observation catalogs, calibrated native grids, and bounded ingestion jobs"),
        (name = "satellite", description = "Native-source satellite frame discovery, provenance-bound TileJSON, immutable revisioned PNG tiles, and prewarm status"),
        (name = "query", description = "Bounded synchronous queries"),
        (name = "analytics", description = "Exact-time and diurnal analytics"),
        (name = "jobs", description = "Bounded asynchronous work and immutable artifacts"),
        (name = "community", description = "Opt-in signed Community Cache objects and deliberate case publication"),
        (name = "federation", description = "Operator-approved signed discovery for deliberately public institutional HTTPS origins"),
        (name = "generation-replication", description = "Advanced default-off owner publication of complete immutable rw-store generations"),
        (name = "operations", description = "Authenticated private operations: storm analysis and operator metrics"),
        (name = "mrms-ingest", description = "Private status and coalesced control for default-off server-owned NOAA MRMS ingest"),
        (name = "nexrad-level2-ingest", description = "Private status and coalesced control for explicit-site, provider-configurable NEXRAD Level II acquisition")
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
            for (name, description) in [
                (
                    "operations_read_auth",
                    "Operations read, write, ingest, or admin token; response is private and no-store",
                ),
                (
                    "operations_admin_auth",
                    "Dedicated operations admin token, or a general API token only when the default-off legacy elevation gate is explicit",
                ),
            ] {
                components.add_security_scheme(
                    name,
                    SecurityScheme::Http(
                        HttpBuilder::new()
                            .scheme(HttpAuthScheme::Bearer)
                            .bearer_format("opaque")
                            .description(Some(description))
                            .build(),
                    ),
                );
            }
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
    description = "Probe the core query executor, configured scientific store, and optional publication catalog. HTTP 200 means core traffic is usable; status `degraded` names optional MRMS or NEXRAD Level II followers that are warming, stale, or in backoff. Optional-source degradation returns 503 only when the operator explicitly enables that subsystem's deployment-wide readiness gate.",
    responses(
        (status = 200, description = "Core store, catalog, and query executor are usable; body status is ready or degraded", body = HealthResponse),
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
    path = "/v1/observations/capabilities",
    tag = "observations",
    security(("bearer_auth" = [])),
    description = "Describe the live observation boundary, native-grid limits, accepted derivations, display metadata, and exact binary media types. The route is publication-gated with the other data reads and carries no server-assigned cache lifetime.",
    responses(
        (status = 200, description = "Observation delivery and ingestion capabilities", body = ObservationCapabilitiesResponseDoc),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn observation_capabilities_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations",
    tag = "observations",
    security(("bearer_auth" = [])),
    params(
        ("model" = Option<String>, Query, description = "Optional exact stored observation model id"),
        ("limit" = Option<usize>, Query, minimum = 1, description = "Optional positive caller-selected run limit; omission returns every enumerable observation run")
    ),
    description = "Enumerate the complete stored observation-run catalog, optionally filtered by model. Every run descriptor includes its exact snapshot/grid identity, time extent, source provenance, and provider attribution. The response carries no server-assigned cache lifetime.",
    responses(
        (status = 200, description = "Complete or explicitly caller-limited observation catalog", body = ObservationCatalogResponseDoc),
        (status = 400, description = "The requested limit or stored catalog entry is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A configured catalog/allocation limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Catalog I/O, metadata, allocation, or serialization failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable, the service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Catalog work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn observation_catalog_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations/{model}/{run}/frames",
    tag = "observations",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Exact stored observation model id"),
        ("run" = String, Path, description = "Exact immutable observation run id")
    ),
    description = "Resolve one immutable observation snapshot into its exact time axis and variable inventory. Variable selectors and display metadata state calibrated-value semantics, safe interpolation, palette family, transparency, preferred range, and discontinuity behavior. The embedded run descriptor carries source provenance and provider attribution.",
    responses(
        (status = 200, description = "Snapshot-bound observation frames and variables", body = ObservationFramesResponseDoc),
        (status = 400, description = "The stored run or variable metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The observation family, model, or run is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A configured snapshot/allocation limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable, the service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Snapshot work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn observation_frames_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations/{model}/{run}/grid.bin",
    tag = "observations",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Exact stored observation model id"),
        ("run" = String, Path, description = "Exact immutable observation run id")
    ),
    description = "Return the snapshot's native cell-center grid as the versioned `RWOBGRID` binary format. The ETag is derived from the immutable snapshot id and the response is safe for long-lived shared caching.",
    responses(
        (status = 200, description = "Immutable native observation grid", body = ObservationGridBinaryDoc, content_type = "application/vnd.rusty-weather.observation-grid+f32",
            headers(
                ("Cache-Control" = String, description = "Always public, max-age=31536000, immutable"),
                ("ETag" = String, description = "Strong snapshot-bound grid validator")
            )
        ),
        (status = 400, description = "The stored grid metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The observation family, model, or run is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A configured snapshot/allocation limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable, the service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Grid work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn observation_grid_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations/{model}/{run}/frames/{storage_slot}/{variable}",
    tag = "observations",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Exact stored observation model id"),
        ("run" = String, Path, description = "Exact immutable observation run id"),
        ("storage_slot" = u16, Path, description = "Exact slot from the frame catalog"),
        ("variable" = String, Path, description = "Exact surface-plane variable filename, including the required .bin suffix")
    ),
    description = "Return one calibrated row-major plane as the versioned `RWOBF32` binary format. The strong ETag binds snapshot, slot, and stored variable identity. Scientific semantics are repeated in response headers; authoritative source provenance remains in the frames/run descriptor rather than being fabricated in the binary payload.",
    responses(
        (status = 200, description = "Immutable calibrated observation plane", body = ObservationPlaneBinaryDoc, content_type = "application/vnd.rusty-weather.observation-plane+f32",
            headers(
                ("Cache-Control" = String, description = "Always public, max-age=31536000, immutable"),
                ("ETag" = String, description = "Strong snapshot, slot, and variable validator"),
                ("x-rw-observation-semantics" = String, description = "Calibrated value semantics"),
                ("x-rw-observation-interpolation" = String, description = "Safe interpolation rule"),
                ("x-rw-observation-palette" = String, description = "Semantic palette family"),
                ("x-rw-nodata" = String, description = "Non-finite transparency contract"),
                ("x-rw-preferred-range" = String, description = "Optional comma-separated calibrated display range"),
                ("x-rw-discontinuity-threshold" = String, description = "Optional discontinuity threshold")
            )
        ),
        (status = 400, description = "The stored plane metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The observation family, model, run, slot, or variable is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "A configured snapshot or allocation limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata or store I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable, the service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Plane work exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn observation_plane_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/mrms/latest",
    tag = "observations",
    security(("bearer_auth" = [])),
    description = "Submit a bounded asynchronous fetch/decode/store job for the caller-selected current NOAA MRMS product. No example URL, credential, or source identity is invented. A successful job publishes a `StoredObservationFrameRefDoc` JSON artifact through the normal job/artifact APIs.",
    request_body(content = MrmsIngestRequestDoc, content_type = "application/json"),
    responses(
        (status = 202, description = "Observation job accepted", body = JobView),
        (status = 400, description = "The JSON syntax, selector, or job request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The request exceeds the configured body limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The JSON shape could not be deserialized", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The job record or request fingerprint could not be created", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or job capacity is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_mrms_latest_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/nexrad/level2",
    tag = "observations",
    security(("bearer_auth" = [])),
    params(
        ("site" = Option<String>, Query, description = "Optional radar site id; otherwise decoded or explicit coordinates identify the site"),
        ("site_latitude" = Option<f64>, Query, description = "Optional explicit radar latitude"),
        ("site_longitude" = Option<f64>, Query, description = "Optional explicit radar longitude"),
        ("site_elevation_m" = Option<f64>, Query, description = "Optional explicit radar elevation in metres"),
        ("moment" = Option<String>, Query, description = "Radar moment: reflectivity/ref, velocity/vel, spectrum_width/sw, zdr, correlation_coefficient/rho/cc, differential_phase/phi/phidp, kdp, or hca"),
        ("mode" = Option<String>, Query, description = "Grid mode: lowest (default), composite, or sweep"),
        ("sweep_index" = Option<u16>, Query, description = "Required only for mode=sweep"),
        ("resolution_m" = Option<f64>, Query, description = "Output Cartesian cell resolution in metres; default 1000"),
        ("radius_km" = Option<f64>, Query, description = "Output radius in kilometres; default 230"),
        ("collection" = Option<String>, Query, description = "Optional stored collection override"),
        ("variable" = Option<String>, Query, description = "Optional stored variable override")
    ),
    description = "Submit one caller-supplied NEXRAD Archive-II volume for asynchronous decode and storage. The upload route deliberately assigns no upstream archive provenance; server-owned follower provenance is exposed by the separate ingest status route. A successful job publishes a `StoredObservationFrameRefDoc` JSON artifact.",
    request_body(content = NexradLevel2ArchiveDoc, content_type = "application/octet-stream"),
    responses(
        (status = 202, description = "Observation job accepted", body = JobView),
        (status = 400, description = "Query options are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The Archive-II body is empty or exceeds the dedicated Level-II upload limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The job record or request fingerprint could not be created", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or job capacity is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_nexrad_level2_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/radar/mosaic",
    tag = "observations",
    security(("bearer_auth" = [])),
    description = "Submit an asynchronous mosaic over exact stored source planes. The resulting frame records every source plane identity in its selector/provenance metadata and never claims an authoritative upstream polygon product. A successful job publishes a `StoredObservationFrameRefDoc` JSON artifact.",
    request_body(content = RadarMosaicRequestDoc, content_type = "application/json"),
    responses(
        (status = 202, description = "Observation job accepted", body = JobView),
        (status = 400, description = "The JSON syntax or job request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The request exceeds the configured body limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The JSON shape could not be deserialized", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The job record or request fingerprint could not be created", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or job capacity is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_radar_mosaic_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/wrf-radar/derive",
    tag = "observations",
    security(("bearer_auth" = [])),
    description = "Submit an asynchronous deterministic radar derivation from one exact stored model variable. The source model/run/slot/variable identity is preserved in the output selector and provenance metadata. A successful job publishes a `StoredObservationFrameRefDoc` JSON artifact.",
    request_body(content = SimulatedRadarRequestDoc, content_type = "application/json"),
    responses(
        (status = 202, description = "Observation job accepted", body = JobView),
        (status = 400, description = "The JSON syntax or job request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The request exceeds the configured body limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The JSON shape could not be deserialized", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The job record or request fingerprint could not be created", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or job capacity is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_simulated_radar_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/generated",
    tag = "observations",
    security(("bearer_auth" = [])),
    description = "Submit one arbitrary calibrated native grid for asynchronous validation and storage. Null coordinates/values become non-finite missing cells; callers supply honest provider, role, and product provenance or leave those fields empty. A successful job publishes a `StoredObservationFrameRefDoc` JSON artifact.",
    request_body(content = GeneratedObservationFrameRequestDoc, content_type = "application/json"),
    responses(
        (status = 202, description = "Observation job accepted", body = JobView),
        (status = 400, description = "The JSON syntax, grid, values, provenance, or job request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The request exceeds the dedicated generated-observation upload limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The JSON shape could not be deserialized", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The job record or request fingerprint could not be created", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or job capacity is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn submit_generated_observation_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations/mrms/ingest/status",
    tag = "mrms-ingest",
    security(("bearer_auth" = [])),
    description = "Private, no-store snapshot of configured MRMS products, bounded worker state, latest exact stored identities, and source-valid-time freshness. The route exists only when background MRMS ingest is enabled.",
    responses(
        (status = 200, description = "Current private MRMS follower status", body = MrmsIngestStatus),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Background MRMS ingest is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn mrms_ingest_status_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/mrms/ingest/refresh",
    tag = "mrms-ingest",
    security(("bearer_auth" = [])),
    description = "Advance the follower's coalescing wake epoch. Any number of client requests received before the workers observe the epoch cause one immediate cycle per configured product, not one upstream job per request.",
    responses(
        (status = 202, description = "Refresh wake accepted and coalesced", body = MrmsRefreshResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Background MRMS ingest is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn mrms_ingest_refresh_doc() {}

#[utoipa::path(
    get,
    path = "/v1/observations/nexrad/level2/ingest/status",
    tag = "nexrad-level2-ingest",
    security(("bearer_auth" = [])),
    description = "Private, no-store status for every explicitly allowed radar site. Exact provider object identity, SHA-256, decoded valid time, canonical stored snapshot, freshness, retry state, and operator resource controls are reported. The route exists only when background Level II ingest is enabled.",
    responses(
        (status = 200, description = "Current private NEXRAD Level II follower status", body = NexradLevel2IngestStatus),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Background NEXRAD Level II ingest is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn nexrad_level2_ingest_status_doc() {}

#[utoipa::path(
    post,
    path = "/v1/observations/nexrad/level2/ingest/refresh",
    tag = "nexrad-level2-ingest",
    security(("bearer_auth" = [])),
    description = "Advance one coalescing wake epoch. Concurrent client or operator requests produce one immediate cycle per configured site rather than one archive fetch per request.",
    responses(
        (status = 202, description = "Refresh wake accepted and coalesced", body = NexradLevel2RefreshResponse),
        (status = 401, description = "Bearer authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Background NEXRAD Level II ingest is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn nexrad_level2_ingest_refresh_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/catalog",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("include_raw_channels" = Option<bool>, Query, description = "Include all raw ABI C01-C16 products in addition to named recipes; default false")
    ),
    description = "Return the native GOES platform, sector, product, enhancement, renderer-recipe, and attribution contract. Required channels and daylight-only flags are explicit. The response carries no server-assigned cache lifetime.",
    responses(
        (status = 200, description = "Native satellite product catalog", body = SatelliteCatalogResponseDoc),
        (status = 400, description = "The query parameters could not be decoded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Catalog serialization failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_catalog_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/prewarm/status",
    tag = "satellite",
    security(("bearer_auth" = [])),
    description = "Return current request-independent satellite tile prewarm state, exact active work identity, bounded counters, and the latest sanitized failure. This live status response carries no validator or server-assigned cache lifetime.",
    responses(
        (status = 200, description = "Current satellite prewarm status", body = SatellitePrewarmStatusDoc),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Status serialization failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_prewarm_status_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/{platform}/{sector}/{product}/frames",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("platform" = String, Path, description = "Native satellite platform id from the catalog"),
        ("sector" = String, Path, description = "Native ABI sector id from the catalog"),
        ("product" = String, Path, description = "Named or raw-channel product id from the catalog"),
        ("limit" = Option<usize>, Query, minimum = 1, description = "Optional positive caller-selected page size; omission returns every retained complete frame")
    ),
    description = "List complete retained native-source frames. Each frame carries exact scan bounds, present channels, and a BLAKE3 source_revision computed from the committed required-channel content. Incomplete native frames are excluded. The response carries no server-assigned cache lifetime.",
    responses(
        (status = 200, description = "Complete native frames in newest-first archive order", body = SatelliteFramesResponseDoc),
        (status = 400, description = "The limit, platform, sector, product, or archive identity is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The product, sector, or native frame archive is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Native archive I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or satellite worker is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_frames_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("platform" = String, Path, description = "Native satellite platform id"),
        ("sector" = String, Path, description = "Native ABI sector id"),
        ("product" = String, Path, description = "Named or raw-channel product id"),
        ("frame" = String, Path, description = "Exact YYYYMMDDTHHMM native frame id or the mutable latest alias"),
        ("If-None-Match" = Option<String>, Header, description = "Conditional validator from an earlier response")
    ),
    description = "Resolve one frame (or `latest`) and return TileJSON 3.0 pointing only at the renderer-recipe plus exact source-revision tile route. Exact source provenance is repeated in the body and `x-rw-satellite-source-revision`. Exact-frame TileJSON is revalidated after five minutes; `latest` is always revalidated and never treated as immutable.",
    responses(
        (status = 200, description = "TileJSON bound to an exact resolved native source revision", body = SatelliteTileJsonResponseDoc, content_type = "application/json",
            headers(
                ("Cache-Control" = String, description = "latest: no-cache, max-age=0, must-revalidate; exact frame: public, max-age=300, must-revalidate"),
                ("ETag" = String, description = "Strong response-body validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 304, description = "If-None-Match matched; body omitted",
            headers(
                ("Cache-Control" = String, description = "Same policy as the selected frame"),
                ("ETag" = String, description = "Matching strong validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 400, description = "The path, public request origin, or native archive identity is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The product, sector, frame, or complete native source is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Native archive I/O or serialization failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or satellite worker is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_tilejson_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("platform" = String, Path, description = "Native satellite platform id"),
        ("sector" = String, Path, description = "Native ABI sector id"),
        ("product" = String, Path, description = "Named or raw-channel product id"),
        ("frame" = String, Path, description = "Exact YYYYMMDDTHHMM native frame id or the mutable latest alias"),
        ("z" = u8, Path, description = "XYZ zoom, bounded by the satellite catalog maximum"),
        ("x" = u32, Path, description = "XYZ tile column"),
        ("y" = String, Path, description = "XYZ tile row including the required .png suffix"),
        ("If-None-Match" = Option<String>, Header, description = "Conditional validator from an earlier response")
    ),
    description = "Compatibility tile route without renderer/source revisions in the URL. The body and provenance headers still bind the resolved frame, current renderer recipe, exact valid time, and exact native source revision. Exact frames must revalidate; `latest` is no-store. New consumers should follow TileJSON to the fully revisioned route.",
    responses(
        (status = 200, description = "Rendered satellite PNG", body = SatellitePngTileDoc, content_type = "image/png",
            headers(
                ("Cache-Control" = String, description = "latest: no-store; exact frame: public, max-age=0, must-revalidate"),
                ("ETag" = String, description = "Strong rendered-byte validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 304, description = "If-None-Match matched; body omitted",
            headers(
                ("Cache-Control" = String, description = "latest: no-store; exact frame: public, max-age=0, must-revalidate"),
                ("ETag" = String, description = "Matching strong validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 400, description = "The frame or XYZ request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The product, sector, frame, native source, or tile is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Native archive or tile-cache I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or satellite worker is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_legacy_tile_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{z}/{x}/{y}",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("platform" = String, Path, description = "Native satellite platform id"),
        ("sector" = String, Path, description = "Native ABI sector id"),
        ("product" = String, Path, description = "Named or raw-channel product id"),
        ("frame" = String, Path, description = "Exact YYYYMMDDTHHMM native frame id or the mutable latest alias"),
        ("recipe" = String, Path, description = "Exact renderer recipe advertised by the satellite catalog"),
        ("z" = u8, Path, description = "XYZ zoom, bounded by the satellite catalog maximum"),
        ("x" = u32, Path, description = "XYZ tile column"),
        ("y" = String, Path, description = "XYZ tile row including the required .png suffix"),
        ("If-None-Match" = Option<String>, Header, description = "Conditional validator from an earlier response")
    ),
    description = "Renderer-versioned tile route without a native source revision in the URL. Exact frames still require revalidation because required native channels can be replaced under the same minute id; `latest` is no-store. The resolved source revision is always returned in provenance headers.",
    responses(
        (status = 200, description = "Rendered satellite PNG", body = SatellitePngTileDoc, content_type = "image/png",
            headers(
                ("Cache-Control" = String, description = "latest: no-store; exact frame: public, max-age=0, must-revalidate"),
                ("ETag" = String, description = "Strong rendered-byte validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 304, description = "If-None-Match matched; body omitted",
            headers(
                ("Cache-Control" = String, description = "latest: no-store; exact frame: public, max-age=0, must-revalidate"),
                ("ETag" = String, description = "Matching strong validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 400, description = "The frame or XYZ request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The recipe, product, sector, frame, native source, or tile is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Native archive or tile-cache I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or satellite worker is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_versioned_tile_doc() {}

#[utoipa::path(
    get,
    path = "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{source_revision}/{z}/{x}/{y}",
    tag = "satellite",
    security(("bearer_auth" = [])),
    params(
        ("platform" = String, Path, description = "Native satellite platform id"),
        ("sector" = String, Path, description = "Native ABI sector id"),
        ("product" = String, Path, description = "Named or raw-channel product id"),
        ("frame" = String, Path, description = "Exact YYYYMMDDTHHMM native frame id; latest remains accepted but no-store"),
        ("recipe" = String, Path, description = "Exact renderer recipe advertised by the satellite catalog"),
        ("source_revision" = String, Path, min_length = 64, max_length = 64, pattern = "^[0-9a-f]{64}$", description = "Exact 64-character lowercase BLAKE3 required-channel content revision"),
        ("z" = u8, Path, description = "XYZ zoom, bounded by the satellite catalog maximum"),
        ("x" = u32, Path, description = "XYZ tile column"),
        ("y" = String, Path, description = "XYZ tile row including the required .png suffix"),
        ("If-None-Match" = Option<String>, Header, description = "Conditional validator from an earlier response")
    ),
    description = "Fully provenance-bound tile route used by generated TileJSON. For an exact frame, recipe plus source_revision is the complete immutable render identity and may be cached for one year even after raw-source retention expires when the durable tile cache still holds the bytes. The mutable `latest` alias remains no-store.",
    responses(
        (status = 200, description = "Rendered provenance-bound satellite PNG", body = SatellitePngTileDoc, content_type = "image/png",
            headers(
                ("Cache-Control" = String, description = "exact frame: public, max-age=31536000, immutable; latest: no-store"),
                ("ETag" = String, description = "Strong rendered-byte validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 304, description = "If-None-Match matched; body omitted",
            headers(
                ("Cache-Control" = String, description = "exact frame: public, max-age=31536000, immutable; latest: no-store"),
                ("ETag" = String, description = "Matching strong validator"),
                ("Vary" = String, description = "Always host"),
                ("x-rw-satellite-frame" = String, description = "Exact resolved native frame id"),
                ("x-rw-valid-unix" = String, description = "Exact frame valid time"),
                ("x-rw-satellite-recipe" = String, description = "Exact renderer recipe"),
                ("x-rw-satellite-source-revision" = String, description = "Exact native required-channel content revision")
            )
        ),
        (status = 400, description = "The frame or XYZ request is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The recipe, source revision, product, sector, frame, or tile is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Native archive or tile-cache I/O failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog or satellite worker is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn satellite_revisioned_tile_doc() {}

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
    path = "/v1/models/{model}/latest-run",
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
    path = "/v1/models/{model}/runs/{run}/planes/{storage_slot}/{variable}",
    tag = "catalog",
    security(("bearer_auth" = [])),
    params(
        ("model" = String, Path, description = "Canonical stored model id"),
        ("run" = String, Path, description = "Explicit immutable run id"),
        ("storage_slot" = u16, Path, description = "Exact storage slot from the run's time axis"),
        ("variable" = String, Path, description = "Exact stored variable filename, including the required .bin suffix"),
        ("expected_snapshot_id" = String, Query, description = "Required. The snapshot_id this plane must belong to; a mismatch fails instead of silently following an atomic republication of the same run name"),
        ("expected_grid_hash" = String, Query, description = "Required. The grid_hash this plane must belong to"),
        ("level_hpa" = Option<u16>, Query, description = "Omit for a surface2d variable; supply one exact stored pressure level for a pressure3d variable. There is no default level and no implicit vertical reduction")
    ),
    description = "Return one complete forecast plane as binary f32 for the named run generation. The whole native grid is returned: no cell ceiling, no downsampling, and no truncation by the synchronous query budgets that bound the JSON window routes. \
Because the URL pins snapshot_id and grid_hash it names one immutable body, which is what makes the long-lived immutable cache directive true. \
The payload uses the same versioned RWOBF32 container as observation planes under its own media type. Surface variables are stored as lossless zstd1_f32; pressure levels are stored as zstd1_affine_i16 and are therefore dequantized approximations, so the exact stored codec ships in x-rw-model-codec rather than letting an f32 payload imply losslessness. \
No palette, semantics, or interpolation hints are emitted: model variables carry no stored display metadata, and the authoritative styling inputs (selector, kind, levels_hpa, available_slots) stay on /v1/models/{model}/runs/{run}/variables.",
    responses(
        (status = 200, description = "Immutable full-grid model forecast plane", body = ModelPlaneBinaryDoc, content_type = "application/vnd.rusty-weather.model-plane+f32",
            headers(
                ("Cache-Control" = String, description = "Always public, max-age=31536000, immutable; the URL's snapshot/grid guards make the body immutable"),
                ("ETag" = String, description = "Strong snapshot, slot, variable, and level validator"),
                ("x-rw-model-variable" = String, description = "Exact stored variable name"),
                ("x-rw-model-units" = String, description = "Exact stored units"),
                ("x-rw-model-codec" = String, description = "Exact stored codec: zstd1_f32 (lossless surface) or zstd1_affine_i16 (dequantized pressure level)"),
                ("x-rw-valid-unix" = i64, description = "Valid time of the plane in Unix seconds"),
                ("x-rw-model-level-hpa" = u16, description = "Present only for a pressure level; absent on surface planes"),
                ("x-rw-nodata" = String, description = "Non-finite transparency contract for absent cells")
            )
        ),
        (status = 400, description = "The identity guards are missing or do not match the resolved run, a query parameter is unsupported, or the stored plane metadata is invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The model, run, storage slot, variable, pressure level, or .bin filename is absent", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the plane was decoded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The variable kind does not match the requested vertical selection, or a configured snapshot limit was exceeded", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Run metadata, store I/O, or allocation failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "The publication catalog is unavailable, the service is busy, or shutdown is in progress", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Plane decoding exceeded its execution deadline", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn model_plane_doc() {}

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
    path = "/v1/profile-cycle",
    tag = "query",
    security(("bearer_auth" = [])),
    request_body = ProfileCycleApiRequest,
    responses(
        (status = 200, description = "Deterministically ordered pressure profiles, colocated surface samples, and explicit gaps for every selected stored time in one immutable run", body = ProfileCycleResponse),
        (status = 400, description = "Request body, coordinates, variables, or half-open time range are invalid", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Bearer authentication failed when tokens are configured", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The run or a variable absent from the complete selection was not found", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "Run changed while the query was executing", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "Request body exceeds the configured byte limit", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "Request is not application/json", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "Variable, selected-time, decoded-value, or strict missing-data limit was not satisfied", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Store, metadata, allocation, or serialization failure", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Service is busy, shutting down, or the authoritative publication catalog is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "Query exceeded its execution deadline and cooperative cancellation was requested", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn profile_cycle_doc() {}

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
        (status = 200, description = "Immutable temporal-grid or stored-observation JSON artifact", content_type = "application/json", body = JobArtifactDoc,
            headers(
                ("Cache-Control" = String, description = "Always private, max-age=31536000, immutable")
            )
        ),
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
    path = "/v1/ops/storms/status",
    tag = "operations",
    description = "Return the private storm runtime, exact-frame memory/disk cache, request-independent MRMS prewarm, and source-linkage status. The route exists only when private operations state is enabled. Every success and failure response is authenticated, `Cache-Control: no-store, private`, has `Pragma: no-cache`, and carries no ETag.",
    security(("operations_read_auth" = [])),
    responses(
        (status = 200, description = "Current private storm service status and honest upstream/derived geometry linkage", body = StormServiceStatusDoc),
        (status = 401, description = "Operations authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Operations read scope is insufficient", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Private operations/storm state is disabled", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn storm_status_doc() {}

#[utoipa::path(
    get,
    path = "/v1/ops/storms/methods",
    tag = "operations",
    description = "Discover every storm method the node can identify: authoritative NOAA Level III products, deterministic Rust contours, and installed machine-learning manifests. Catalog entries expose method, upstream product, model, version, parameters, and geometry provenance. Responses are authenticated, private, and no-store.",
    security(("operations_read_auth" = [])),
    responses(
        (status = 200, description = "Versioned storm method catalog", body = StormMethodCatalog),
        (status = 401, description = "Operations authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Operations read scope is insufficient", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Private operations/storm state is disabled", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "The catalog failed its own wire-contract validation", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Storm runtime is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn storm_methods_doc() {}

#[utoipa::path(
    get,
    path = "/v1/ops/storms/models",
    tag = "operations",
    description = "List immutable storm-model manifests, artifact and derived-output distribution policy, activation state, and whether a trusted Rust execution path exists on this node. HTTP clients cannot install or upload model artifacts. Responses are authenticated, private, and no-store.",
    security(("operations_read_auth" = [])),
    responses(
        (status = 200, description = "Installed private storm-model catalog and execution readiness", body = StormModelCatalogDoc),
        (status = 401, description = "Operations authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Operations read scope is insufficient", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Private operations/storm state is disabled", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Model registry state could not be read", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Storm runtime is unavailable", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn storm_models_doc() {}

#[utoipa::path(
    post,
    path = "/v1/ops/storms/cells",
    tag = "operations",
    description = "Derive or retrieve one exact storm-cell frame from a snapshot-bound field already present in the validated RW store. Clients provide source identity and a deterministic, automatic, or installed-model method; raw grids and executable model artifacts are never uploaded. `format=geojson` returns RFC 7946 contour geometry while preserving the same source, method, partial-result, warning, and provenance fields as canonical JSON. Every response is authenticated, private, and no-store.",
    security(("operations_read_auth" = [])),
    params(
        ("format" = Option<StormResponseFormatDoc>, Query, description = "canonical (default application/json) or geojson (application/geo+json)")
    ),
    request_body(content = StormCellsRequestDoc, content_type = "application/json"),
    responses(
        (status = 200, description = "Exact cached or newly computed storm-cell frame in the requested representation", content(
            (StormCellFrame = "application/json"),
            (StormGeoJsonFeatureCollectionDoc = "application/geo+json")
        )),
        (status = 400, description = "The query or JSON syntax is malformed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Operations authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Operations read scope is insufficient", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "The exact stored generation, field, time, or requested model is unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 409, description = "The stored generation differs from expected_snapshot_id; refresh its descriptor before retrying", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The JSON request exceeds the endpoint body boundary", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request content type is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The schema, source identity, grid, method, model policy, or scientific values are not executable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Storm processing failed without exposing private internals", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Storm workers are busy, shutting down, or unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "The configured heavy-job deadline expired", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn storm_cells_doc() {}

#[utoipa::path(
    post,
    path = "/v1/ops/storms/authoritative/nexrad-level3/decode",
    tag = "operations",
    description = "Decode one caller-supplied authoritative NOAA/RPG NEXRAD Level III message 58 (NST/STI tracking) or message 62 (SS/NSS structure) product with the pure-Rust decoder. The response preserves product identity, ROC specification references, transport identity, validation notices, and an explicit geometry statement: message 58 supplies centroids/tracks but no polygons; message 62 supplies centroids/structure attributes but neither polygons nor tracks. Every response is authenticated, private, and no-store.",
    security(("operations_read_auth" = [])),
    request_body(content = NexradLevel3StormDecodeRequestDoc, content_type = "application/json"),
    responses(
        (status = 200, description = "Authoritative decoded Level III storm product with complete identity and geometry provenance", body = NexradLevel3StormDecodeResponseDoc),
        (status = 400, description = "The JSON syntax is malformed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 401, description = "Operations authentication failed", content_type = "application/problem+json", body = ProblemDetails),
        (status = 403, description = "Operations read scope is insufficient", content_type = "application/problem+json", body = ProblemDetails),
        (status = 404, description = "Private operations/storm state is disabled", content_type = "application/problem+json", body = ProblemDetails),
        (status = 413, description = "The encoded request exceeds the endpoint body boundary", content_type = "application/problem+json", body = ProblemDetails),
        (status = 415, description = "The request content type is not JSON", content_type = "application/problem+json", body = ProblemDetails),
        (status = 422, description = "The schema, base64, structural bounds, or Level III product are invalid or unsupported", content_type = "application/problem+json", body = ProblemDetails),
        (status = 500, description = "Decoding failed without exposing private internals", content_type = "application/problem+json", body = ProblemDetails),
        (status = 503, description = "Lightweight workers are busy, shutting down, or unavailable", content_type = "application/problem+json", body = ProblemDetails),
        (status = 504, description = "The configured lightweight-job deadline expired", content_type = "application/problem+json", body = ProblemDetails)
    )
)]
fn nexrad_level3_storm_decode_doc() {}

#[utoipa::path(
    get,
    path = "/metrics",
    tag = "operations",
    description = "OpenMetrics endpoint. It is protected by bearer authentication by default (auth.protect_metrics = true); operators may explicitly opt out with auth.protect_metrics = false. When no tokens are configured, authentication middleware permits local requests.",
    security(("bearer_auth" = [])),
    responses(
        (status = 200, description = "OpenMetrics text exposition", content_type = "application/openmetrics-text; version=1.0.0; charset=utf-8", body = String),
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
            "/v1/models/{model}/latest-run",
            "/v1/point",
            "/v1/profile-cycle",
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
            !paths.contains_key("/v1/models/{model}/runs/latest"),
            "latest-run pointer must not reserve the legal run ID 'latest'"
        );
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
        let latest = operation(&value, "/v1/models/{model}/latest-run", "get");
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
    fn storm_contracts_are_complete_authenticated_and_browser_discoverable() {
        let value = serde_json::to_value(document()).unwrap();
        let routes = [
            ("/v1/ops/storms/status", "get"),
            ("/v1/ops/storms/methods", "get"),
            ("/v1/ops/storms/models", "get"),
            ("/v1/ops/storms/cells", "post"),
            ("/v1/ops/storms/authoritative/nexrad-level3/decode", "post"),
        ];
        for (path, method) in routes {
            let operation = operation(&value, path, method);
            assert_eq!(
                operation["security"][0]["operations_read_auth"],
                serde_json::json!([]),
                "{method} {path} must require a private operations bearer credential"
            );
            assert!(
                operation["responses"]["401"].is_object(),
                "{method} {path} must document authentication failure"
            );
            let description = operation["description"].as_str().unwrap();
            assert!(description.contains("private"), "{method} {path}");
            assert!(description.contains("no-store"), "{method} {path}");
        }

        let auth = &value["components"]["securitySchemes"]["operations_read_auth"];
        assert_eq!(auth["type"], "http");
        assert_eq!(auth["scheme"], "bearer");

        assert_response_ref(
            &value,
            "/v1/ops/storms/status",
            "get",
            "#/components/schemas/StormServiceStatusDoc",
            false,
        );
        assert_response_ref(
            &value,
            "/v1/ops/storms/methods",
            "get",
            "#/components/schemas/StormMethodCatalog",
            false,
        );
        assert_response_ref(
            &value,
            "/v1/ops/storms/models",
            "get",
            "#/components/schemas/StormModelCatalogDoc",
            false,
        );

        let cells = operation(&value, "/v1/ops/storms/cells", "post");
        assert_eq!(
            cells["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/StormCellsRequestDoc"
        );
        assert_eq!(
            cells["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/StormCellFrame"
        );
        assert_eq!(
            cells["responses"]["200"]["content"]["application/geo+json"]["schema"]["$ref"],
            "#/components/schemas/StormGeoJsonFeatureCollectionDoc"
        );
        assert!(
            cells["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|parameter| parameter["name"] == "format")
        );
        for status in ["409", "422", "503"] {
            assert!(
                cells["responses"][status].is_object(),
                "storm cells must document {status}"
            );
        }

        let level3 = operation(
            &value,
            "/v1/ops/storms/authoritative/nexrad-level3/decode",
            "post",
        );
        assert_eq!(
            level3["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/NexradLevel3StormDecodeRequestDoc"
        );
        assert_eq!(
            level3["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/NexradLevel3StormDecodeResponseDoc"
        );
        for status in ["422", "503"] {
            assert!(
                level3["responses"][status].is_object(),
                "Level III decode must document {status}"
            );
        }

        let schemas = &value["components"]["schemas"];
        for field in ["expected_snapshot_id", "expected_grid_hash", "storage_slot"] {
            assert!(
                schemas["StoredStormGridRefDoc"]["properties"][field].is_object(),
                "stored storm request must expose {field}"
            );
        }
        for field in [
            "upstream_product",
            "model_id",
            "model_version",
            "parameters",
        ] {
            assert!(
                schemas["StormMethodIdentity"]["properties"][field].is_object(),
                "storm method provenance must expose {field}"
            );
        }
        for field in ["source", "method", "partial", "warnings", "cells"] {
            assert!(
                schemas["StormCellFrame"]["properties"][field].is_object(),
                "canonical storm frame must expose {field}"
            );
        }
        for field in ["source", "method", "partial", "warnings", "features"] {
            assert!(
                schemas["StormGeoJsonFeatureCollectionDoc"]["properties"][field].is_object(),
                "GeoJSON storm frame must preserve {field}"
            );
        }
        for field in [
            "artifact_sha256",
            "producer",
            "license",
            "training_provenance",
        ] {
            assert!(
                schemas["StormModelManifest"]["properties"][field].is_object(),
                "model manifest provenance must expose {field}"
            );
        }
        for field in [
            "format_specification",
            "product_specification",
            "supplied_geometry",
            "geometry_statement",
        ] {
            assert!(
                schemas["ProductProvenance"]["properties"][field].is_object(),
                "authoritative product provenance must expose {field}"
            );
        }
        for field in ["method", "product", "geometry_statement"] {
            assert!(
                schemas["NexradLevel3StormDecodeResponseDoc"]["properties"][field].is_object(),
                "Level III response must expose {field}"
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
                "/v1/models/{model}/latest-run",
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
                "/v1/profile-cycle",
                "post",
                "#/components/schemas/ProfileCycleResponse",
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
        for field in [
            "provider",
            "forecast_producer",
            "licensing_publisher",
            "transport_provider",
            "transport_is_mirror",
            "roles",
            "products",
        ] {
            assert!(
                schemas["SourceProvenanceResponse"]["properties"][field].is_object(),
                "missing structured provenance schema field {field}"
            );
        }
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
        assert_eq!(
            schemas["ProfileCycleSampleResponse"]["properties"]["source_provenance"]["items"]["$ref"],
            "#/components/schemas/SourceProvenanceResponse"
        );
        assert_eq!(
            schemas["ProfileCycleSampleResponse"]["properties"]["surface_samples"]["items"]["$ref"],
            "#/components/schemas/ProfileSurfaceSampleResponse"
        );
        assert!(
            schemas["ProfileCycleResponse"]["properties"]["requested_surface_variables"]
                .is_object()
        );
        assert_eq!(
            schemas["ProfileCycleSampleStatusResponse"]["enum"],
            serde_json::json!(["complete", "partial", "gap"])
        );
        assert!(schemas["VariableCapabilityResponse"]["properties"]["profile_cycle"].is_object());
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
                "provider_statistics_only",
                "ensemble_control_member_only",
                "sparse_pressure_levels",
                "two_dimensional_statistics_only",
                "derived_products_disabled",
                "conus_only",
                "pre_operational_feed",
                "extended_range_not_scheduled"
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
            for method in ["get", "post", "put", "delete"] {
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

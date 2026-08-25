//! Private, snapshot-bound storm-object HTTP service.
//!
//! The service accepts only references to fields already present in the
//! validated RW store. Raw grids and model artifacts are never accepted over
//! these routes. That keeps request bodies small, preserves source identity,
//! and lets the server's heavy-work semaphore bound concurrent contour work.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{DefaultBodyLimit, Extension, Query, State};
use axum::http::{StatusCode, header};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use rw_nexrad_storm::{DecodeOptions, NexradStormProduct, decode_with_options};
use rw_ops_protocol::{
    GeoPoint, NEXRAD_LEVEL3_STORM_DECODE_PATH, STORM_CELL_FRAME_SCHEMA, STORM_CELLS_PATH,
    STORM_METHOD_CATALOG_SCHEMA, STORM_METHODS_PATH, STORM_MODELS_PATH, StormCell, StormCellFrame,
    StormMethodCatalog, StormMethodIdentity, StormMethodKind, StormModelBackend, StormSource,
};
use rw_query::{QueryError, RunSnapshot, SurfaceField2D};
use rw_storm::{Connectivity, DetectionConfig, GeographicGrid};
use rw_storm_ml::{
    DistributionAudience, GridGeometry, MaskOutput, ModelInputBatch, ModelInputPlane, ModelKey,
    ModelLimits, ModelRegistry, NativeBackendRegistry, NativeStormModel, RegistryError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use tracing::error;

use crate::problem::ProblemDetails;
use crate::routes::RequestId;
use crate::storm_cache::{
    CachedStormFrame, STORM_CACHE_REVISION, StormCacheIdentity, StormDiskCacheHealth,
};
use crate::storm_prewarm::StormPrewarmStatus;
use crate::{AppState, ExecutionError};

const STORM_REQUEST_SCHEMA: &str = "rw.server.storm-cells-request.v1";
const STORM_STATUS_SCHEMA: &str = "rw.server.storm-service-status.v1";
const STORM_MODEL_CATALOG_SCHEMA: &str = "rw.server.storm-model-catalog.v1";
const STORM_GEOJSON_SCHEMA: &str = "rw.ops.storm-cell-geojson.v1";
const NEXRAD_LEVEL3_DECODE_REQUEST_SCHEMA: &str = "rw.server.nexrad-level3-storm-decode-request.v1";
const NEXRAD_LEVEL3_DECODE_RESPONSE_SCHEMA: &str = "rw.ops.nexrad-level3-storm-product.v1";
const STORM_STATUS_PATH: &str = "/v1/ops/storms/status";
const MAX_STORM_REQUEST_BYTES: usize = 64 * 1024;
const MAX_LEVEL3_JSON_REQUEST_BYTES: usize = 24 * 1024 * 1024;
const MAX_LEVEL3_PRODUCT_BYTES: usize = 16 * 1024 * 1024;
const RECTILINEAR_TOLERANCE_DEGREES: f64 = 1.0e-5;

/// Immutable model registry plus trusted Rust backends compiled into this
/// process. Model installation/activation is deliberately an offline,
/// operator-controlled action; HTTP clients cannot upload executable state.
#[derive(Clone)]
pub struct StormRuntime {
    models: Arc<ModelRegistry>,
    native: Arc<RwLock<NativeBackendRegistry>>,
    /// Changes whenever the executable backend set changes. Model registry
    /// state is immutable after `open`; this revision closes the one remaining
    /// cache-identity gap for Auto/native execution.
    cache_revision: Arc<AtomicU64>,
}

impl std::fmt::Debug for StormRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StormRuntime")
            .field("installed_models", &self.models.installed().len())
            .finish_non_exhaustive()
    }
}

impl StormRuntime {
    pub fn open(operations_root: &Path) -> Result<Self, StormServiceError> {
        // `fs::canonicalize` produces a verbatim `\\?\` path on Windows.
        // Walking that prefix one component at a time for the registry's
        // reparse-point audit fails with ERROR_INVALID_FUNCTION. `absolute`
        // provides the required normalized absolute identity without changing
        // the path namespace or following links; the registry performs its own
        // link/reparse audit immediately below.
        let absolute_root = std::path::absolute(operations_root)?;
        let models =
            ModelRegistry::open(absolute_root.join("storm-models"), ModelLimits::default())?;
        Ok(Self {
            models: Arc::new(models),
            native: Arc::new(RwLock::new(NativeBackendRegistry::new())),
            cache_revision: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Register a trusted model implementation compiled into the Rust server.
    /// This is a startup hook, not a dynamic-library or network loader.
    pub fn register_native_model(
        &self,
        key: ModelKey,
        backend: Arc<dyn NativeStormModel>,
    ) -> Result<(), StormServiceError> {
        self.native
            .write()
            .map_err(|_| StormServiceError::RuntimePoisoned)?
            .register(&self.models, key, backend)?;
        self.cache_revision.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    pub(crate) fn cache_revision(&self) -> u64 {
        self.cache_revision.load(Ordering::Acquire)
    }
}

#[derive(Debug, Error)]
pub enum StormServiceError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Query(#[from] QueryError),
    #[error(transparent)]
    Detection(#[from] rw_storm::StormError),
    #[error(transparent)]
    Model(#[from] RegistryError),
    #[error("storm runtime lock was poisoned")]
    RuntimePoisoned,
    #[error("request schema is unsupported")]
    UnsupportedSchema,
    #[error("stored run generation differs from the required snapshot identity")]
    SnapshotMismatch,
    #[error("stored source identity is incompatible: {0}")]
    SourceMismatch(String),
    #[error("stored grid is not rectilinear geographic data: {0}")]
    UnsupportedGrid(String),
    #[error("storm method request is invalid: {0}")]
    InvalidMethod(String),
    #[error("NEXRAD Level III request is invalid: {0}")]
    InvalidLevel3Request(String),
    #[error("NEXRAD Level III product could not be decoded: {0}")]
    Level3Decode(String),
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredStormGridRef {
    pub(crate) model: String,
    pub(crate) run: String,
    pub(crate) expected_snapshot_id: String,
    pub(crate) expected_grid_hash: String,
    pub(crate) storage_slot: u16,
    pub(crate) variable: String,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum ConnectivityRequest {
    #[default]
    Four,
    Eight,
}

impl From<ConnectivityRequest> for Connectivity {
    fn from(value: ConnectivityRequest) -> Self {
        match value {
            ConnectivityRequest::Four => Self::Four,
            ConnectivityRequest::Eight => Self::Eight,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct DetectionRequest {
    threshold_dbz: f32,
    minimum_valid_dbz: f32,
    maximum_valid_dbz: f32,
    minimum_gate_count: usize,
    minimum_area_km2: f64,
    connectivity: ConnectivityRequest,
}

impl Default for DetectionRequest {
    fn default() -> Self {
        let defaults = DetectionConfig::default();
        Self {
            threshold_dbz: defaults.threshold_dbz,
            minimum_valid_dbz: defaults.minimum_valid_dbz,
            maximum_valid_dbz: defaults.maximum_valid_dbz,
            minimum_gate_count: defaults.minimum_gate_count,
            minimum_area_km2: defaults.minimum_area_km2,
            connectivity: ConnectivityRequest::Four,
        }
    }
}

impl From<DetectionRequest> for DetectionConfig {
    fn from(value: DetectionRequest) -> Self {
        Self {
            threshold_dbz: value.threshold_dbz,
            minimum_valid_dbz: value.minimum_valid_dbz,
            maximum_valid_dbz: value.maximum_valid_dbz,
            minimum_gate_count: value.minimum_gate_count,
            minimum_area_km2: value.minimum_area_km2,
            connectivity: value.connectivity.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum StormMethodRequest {
    /// Prefer an explicitly activated compatible compiled Rust model. Fall
    /// back to the deterministic method when no such model is ready.
    Auto {
        #[serde(default)]
        deterministic: DetectionRequest,
    },
    Deterministic {
        #[serde(default)]
        config: DetectionRequest,
    },
    MachineLearning {
        model_id: String,
        #[serde(default)]
        model_version: Option<String>,
        /// Model input-name to stored variable-name mapping. A one-input model
        /// may omit this map and use `grid.variable`.
        #[serde(default)]
        input_variables: BTreeMap<String, String>,
        /// Required only for a `supplied_mask` model. This field is read from
        /// the same immutable grid/time, never uploaded in the request.
        #[serde(default)]
        supplied_mask_variable: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StormCellsRequest {
    pub(crate) schema: String,
    pub(crate) grid: StoredStormGridRef,
    pub(crate) source: StormSource,
    pub(crate) method: StormMethodRequest,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct NexradLevel3DecodeRequest {
    schema: String,
    #[serde(default)]
    site_hint: Option<String>,
    product_base64: String,
}

#[derive(Debug, Serialize)]
struct NexradLevel3DecodeResponse {
    schema: &'static str,
    generated_at_unix_ms: i64,
    method: StormMethodIdentity,
    product: NexradStormProduct,
    geometry_statement: &'static str,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ResponseFormat {
    #[default]
    Canonical,
    Geojson,
}

#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CellsQuery {
    format: ResponseFormat,
}

#[derive(Debug)]
pub(crate) enum StormFrameFillError {
    Service(StormServiceError),
    Execution(ExecutionError),
}

#[derive(Debug, Serialize)]
struct StormServiceStatus {
    schema: &'static str,
    generated_at_unix_ms: i64,
    ready: bool,
    stored_source_execution: bool,
    direct_client_grid_uploads: bool,
    exact_frame_single_flight: bool,
    frame_cache_scope: &'static str,
    frame_cache_revision: &'static str,
    frame_cache_max_bytes: u64,
    durable_cache: Option<StormDiskCacheHealth>,
    prewarm: StormPrewarmStatus,
    source_linkage: Vec<SourceLinkageStatus>,
}

#[derive(Debug, Serialize)]
struct SourceLinkageStatus {
    source: &'static str,
    available: bool,
    geometry: &'static str,
    detail: &'static str,
}

#[derive(Debug, Serialize)]
struct StormModelCatalog {
    schema: &'static str,
    generated_at_unix_ms: i64,
    limits: StormModelLimitsStatus,
    models: Vec<StormModelStatus>,
}

#[derive(Debug, Serialize)]
struct StormModelLimitsStatus {
    /// `None` means no configured policy ceiling; checked `usize` arithmetic,
    /// allocation, and filesystem capacity remain authoritative.
    maximum_installed_versions: Option<u64>,
    maximum_activation_history_entries_per_model: Option<u64>,
    maximum_artifact_bytes: u64,
    maximum_manifest_bytes: u64,
    maximum_grid_width: Option<u64>,
    maximum_grid_height: Option<u64>,
    maximum_grid_points: Option<u64>,
    maximum_input_planes: u64,
    maximum_label_work_points: Option<u64>,
    null_policy: &'static str,
}

impl From<ModelLimits> for StormModelLimitsStatus {
    fn from(limits: ModelLimits) -> Self {
        Self {
            maximum_installed_versions: configured_usize_limit(limits.max_installed_versions),
            maximum_activation_history_entries_per_model: configured_usize_limit(
                limits.max_activation_history,
            ),
            maximum_artifact_bytes: limits.max_artifact_bytes,
            maximum_manifest_bytes: limits.max_manifest_bytes,
            maximum_grid_width: configured_usize_limit(limits.max_grid_width),
            maximum_grid_height: configured_usize_limit(limits.max_grid_height),
            maximum_grid_points: configured_usize_limit(limits.max_grid_points),
            maximum_input_planes: limits.max_input_planes as u64,
            maximum_label_work_points: configured_usize_limit(limits.max_label_work_points),
            null_policy: "no configured ceiling; checked address-space, allocation, and filesystem capacity apply",
        }
    }
}

fn configured_usize_limit(limit: usize) -> Option<u64> {
    (limit != usize::MAX).then_some(limit as u64)
}

#[derive(Debug, Serialize)]
struct StormModelStatus {
    manifest: rw_ops_protocol::StormModelManifest,
    policy: rw_storm_ml::ModelUsePolicy,
    enabled: bool,
    active: bool,
    executable_on_this_node: bool,
    execution_mode: &'static str,
}

#[derive(Debug, Serialize)]
struct GeoJsonFeatureCollection {
    r#type: &'static str,
    schema: &'static str,
    generated_at_unix_ms: i64,
    source: StormSource,
    method: StormMethodIdentity,
    partial: bool,
    warnings: Vec<String>,
    features: Vec<GeoJsonFeature>,
}

#[derive(Debug, Serialize)]
struct GeoJsonFeature {
    r#type: &'static str,
    id: String,
    geometry: Value,
    properties: Value,
}

pub(crate) fn router(state: AppState) -> Router<AppState> {
    if state.storms.is_none() {
        return Router::new();
    }
    let normal = Router::new()
        .route(STORM_STATUS_PATH, get(status))
        .route(STORM_METHODS_PATH, get(methods))
        .route(STORM_MODELS_PATH, get(models))
        .route(STORM_CELLS_PATH, post(cells))
        .layer(DefaultBodyLimit::max(MAX_STORM_REQUEST_BYTES));
    let level3 = Router::new()
        .route(NEXRAD_LEVEL3_STORM_DECODE_PATH, post(decode_level3))
        .layer(DefaultBodyLimit::max(MAX_LEVEL3_JSON_REQUEST_BYTES));
    normal
        .merge(level3)
        // This local outer layer also covers extractor/body-limit rejections,
        // which occur before the operations handler middleware can decorate a
        // response. Private derived data and even its error shape must never
        // become a shared intermediary cache entry.
        .layer(middleware::map_response(private_storm_response))
}

async fn private_storm_response(mut response: Response) -> Response {
    use axum::http::{HeaderValue, header};
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

async fn status(State(state): State<AppState>) -> Response {
    let ready = state.storms.is_some();
    Json(StormServiceStatus {
        schema: STORM_STATUS_SCHEMA,
        generated_at_unix_ms: now_unix_ms(),
        ready,
        stored_source_execution: ready,
        direct_client_grid_uploads: false,
        exact_frame_single_flight: true,
        frame_cache_scope: "process_memory_plus_verified_atomic_disk",
        frame_cache_revision: STORM_CACHE_REVISION,
        frame_cache_max_bytes: state.config.limits.response_cache_bytes,
        durable_cache: state.storm_disk_cache.as_ref().map(|cache| cache.health()),
        prewarm: state.storm_prewarm_status.status(),
        source_linkage: vec![
            SourceLinkageStatus {
                source: "mrms_reflectivity",
                available: true,
                geometry: "derived_reflectivity_threshold_contour",
                detail: "Runs against an exact stored MRMS grid generation; this is not an NCEI polygon product.",
            },
            SourceLinkageStatus {
                source: "nexrad_level2_single_sweep_reflectivity",
                available: true,
                geometry: "derived_reflectivity_threshold_contour",
                detail: "Runs against the already georeferenced Level-II sweep stored by rw-observations; raw polar gates are not treated as Cartesian coordinates.",
            },
            SourceLinkageStatus {
                source: "nexrad_level3_nst_sti_message_58",
                available: true,
                geometry: "authoritative_centroids_tracks_only",
                detail: "The pure-Rust decoder accepts supplied NOAA message 58 products and returns authoritative IDs, positions, and tracks. It never represents separately derived contour geometry as an NOAA/RPG polygon.",
            },
            SourceLinkageStatus {
                source: "nexrad_level3_ss_nss_message_62",
                available: true,
                geometry: "authoritative_centroids_and_structure_attributes_only",
                detail: "The pure-Rust decoder accepts supplied NOAA message 62 products and returns authoritative centroid positions and storm-structure attributes. Message 62 supplies neither polygons nor tracks.",
            },
        ],
    })
    .into_response()
}

async fn methods(State(state): State<AppState>) -> Response {
    let Some(runtime) = state.storms.as_ref() else {
        return unavailable(uuid::Uuid::nil());
    };
    let mut methods = vec![
        authoritative_tracking_method_identity(),
        authoritative_structure_method_identity(),
        deterministic_method_identity(DetectionRequest::default()),
    ];
    methods.extend(
        runtime
            .models
            .installed()
            .map(|model| model_method_identity(&model.manifest)),
    );
    let catalog = StormMethodCatalog {
        schema: STORM_METHOD_CATALOG_SCHEMA.into(),
        generated_at_unix_ms: now_unix_ms(),
        methods,
    };
    match catalog.validate() {
        Ok(()) => Json(catalog).into_response(),
        Err(error) => {
            error!(%error, "storm method catalog failed its wire contract");
            ProblemDetails::internal(uuid::Uuid::nil()).into_response()
        }
    }
}

async fn decode_level3(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<NexradLevel3DecodeRequest>,
) -> Response {
    if request.schema != NEXRAD_LEVEL3_DECODE_REQUEST_SCHEMA {
        return storm_problem(&StormServiceError::UnsupportedSchema, request_id.0);
    }
    // Reject an obviously oversized encoded body before allocating decoded
    // storage. The decoder independently enforces the exact 16 MiB binary
    // structural bound.
    let maximum_encoded = MAX_LEVEL3_PRODUCT_BYTES.div_ceil(3).saturating_mul(4);
    if request.product_base64.len() > maximum_encoded {
        return storm_problem(
            &StormServiceError::InvalidLevel3Request(format!(
                "encoded product exceeds the {MAX_LEVEL3_PRODUCT_BYTES}-byte decoder boundary"
            )),
            request_id.0,
        );
    }
    let bytes = match BASE64_STANDARD.decode(request.product_base64.as_bytes()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return storm_problem(
                &StormServiceError::InvalidLevel3Request(format!(
                    "product_base64 is not canonical base64: {error}"
                )),
                request_id.0,
            );
        }
    };
    if bytes.len() > MAX_LEVEL3_PRODUCT_BYTES {
        return storm_problem(
            &StormServiceError::InvalidLevel3Request(format!(
                "decoded product exceeds {MAX_LEVEL3_PRODUCT_BYTES} bytes"
            )),
            request_id.0,
        );
    }
    let options = DecodeOptions {
        site_hint: request.site_hint,
        ..DecodeOptions::default()
    };
    let result = state
        .run_light(move || {
            decode_with_options(&bytes, &options)
                .map_err(|error| StormServiceError::Level3Decode(error.to_string()))
        })
        .await;
    match result {
        Ok(Ok(product)) => {
            let kind = Level3StormProductKind::from_product(&product);
            Json(NexradLevel3DecodeResponse {
                schema: NEXRAD_LEVEL3_DECODE_RESPONSE_SCHEMA,
                generated_at_unix_ms: now_unix_ms(),
                method: kind.method_identity(),
                product,
                geometry_statement: kind.geometry_statement(),
            })
            .into_response()
        }
        Ok(Err(error)) => storm_problem(&error, request_id.0),
        Err(error) => execution_problem(&error, request_id.0),
    }
}

async fn models(State(state): State<AppState>) -> Response {
    let Some(runtime) = state.storms.as_ref() else {
        return unavailable(uuid::Uuid::nil());
    };
    let native = match runtime.native.read() {
        Ok(native) => native,
        Err(_) => return ProblemDetails::internal(uuid::Uuid::nil()).into_response(),
    };
    let models = runtime
        .models
        .installed()
        .map(|model| {
            let active = runtime
                .models
                .active(&model.key.model_id)
                .is_ok_and(|active| active.key == model.key);
            let (executable_on_this_node, execution_mode) = match model.manifest.backend {
                StormModelBackend::NativeRust => {
                    (native.contains(&model.key), "compiled_rust_backend")
                }
                StormModelBackend::SuppliedMask => (true, "stored_probability_mask"),
                StormModelBackend::TractOnnx => (false, "executor_not_compiled"),
            };
            StormModelStatus {
                manifest: model.manifest.clone(),
                policy: model.policy.clone(),
                enabled: runtime.models.is_enabled(&model.key),
                active,
                executable_on_this_node,
                execution_mode,
            }
        })
        .collect();
    Json(StormModelCatalog {
        schema: STORM_MODEL_CATALOG_SCHEMA,
        generated_at_unix_ms: now_unix_ms(),
        limits: runtime.models.limits().into(),
        models,
    })
    .into_response()
}

async fn cells(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Query(query): Query<CellsQuery>,
    Json(request): Json<StormCellsRequest>,
) -> Response {
    if request.schema != STORM_REQUEST_SCHEMA {
        return storm_problem(&StormServiceError::UnsupportedSchema, request_id.0);
    }
    if state.storms.is_none() {
        return unavailable(request_id.0);
    }
    let cached = match obtain_cached_frame(&state, request).await {
        Ok(frame) => frame,
        Err(error) => match error.as_ref() {
            StormFrameFillError::Service(error) => return storm_problem(error, request_id.0),
            StormFrameFillError::Execution(error) => {
                return execution_problem(error, request_id.0);
            }
        },
    };
    match query.format {
        ResponseFormat::Canonical => json_bytes(cached.canonical.clone(), "application/json"),
        ResponseFormat::Geojson => json_bytes(cached.geojson.clone(), "application/geo+json"),
    }
}

pub(crate) async fn obtain_cached_frame(
    state: &AppState,
    request: StormCellsRequest,
) -> Result<Arc<CachedStormFrame>, Arc<StormFrameFillError>> {
    let Some(runtime) = state.storms.clone() else {
        return Err(Arc::new(StormFrameFillError::Service(
            StormServiceError::InvalidMethod("private storm runtime is disabled".into()),
        )));
    };
    let cache_key = storm_frame_cache_key(&request, runtime.cache_revision())
        .map_err(|error| Arc::new(StormFrameFillError::Service(error)))?;
    let identity = storm_cache_identity(&request, cache_key.clone());
    let current_grid = request.grid.clone();
    let cache = state.storm_frame_cache.clone();
    let fill_state = state.clone();
    let cached = cache
        .try_get_with(cache_key, async move {
            let catalog = fill_state.catalog.clone();
            let fill_runtime = runtime.clone();
            let disk_cache = fill_state.storm_disk_cache.clone();
            match fill_state
                .run_heavy_job(move || {
                    if let Some(disk) = &disk_cache {
                        match disk.load(&identity) {
                            Ok(Some(frame)) => return Ok(frame),
                            Ok(None) => {}
                            Err(error) => {
                                tracing::warn!(%error, "durable storm cache read failed; recomputing exact frame");
                            }
                        }
                    }
                    let frame = execute_request(&catalog, &fill_runtime, request)?;
                    let cached = Arc::new(cache_frame(frame)?);
                    if let Some(disk) = &disk_cache
                        && let Err(error) = disk.store(&identity, &cached)
                    {
                        tracing::warn!(%error, "durable storm cache write failed; serving computed frame from memory");
                    }
                    Ok(cached)
                })
                .await
            {
                Ok(Ok(frame)) => Ok(frame),
                Ok(Err(error)) => Err(StormFrameFillError::Service(error)),
                Err(error) => Err(StormFrameFillError::Execution(error)),
            }
        })
        .await?;
    // Cache hits remain snapshot-bound. Observation runs append by atomically
    // replacing their manifest, so an older exact request must become a 409
    // even if its derived bytes still exist in memory or on disk. Validate
    // after the fill/hit to establish the response's source-generation
    // linearization point and close the replacement race around cache I/O.
    let validation_catalog = state.catalog.clone();
    match state
        .run_light(move || {
            let snapshot = validation_catalog.snapshot(&current_grid.model, &current_grid.run)?;
            validate_snapshot(&snapshot, &current_grid)
        })
        .await
    {
        Ok(Ok(())) => Ok(cached),
        Ok(Err(error)) => Err(Arc::new(StormFrameFillError::Service(error))),
        Err(error) => Err(Arc::new(StormFrameFillError::Execution(error))),
    }
}

fn cache_frame(frame: StormCellFrame) -> Result<CachedStormFrame, StormServiceError> {
    let geojson = frame_to_geojson(&frame)?;
    let canonical = serde_json::to_vec(&frame).map_err(|error| {
        StormServiceError::InvalidMethod(format!("canonical storm JSON failed: {error}"))
    })?;
    let geojson = serde_json::to_vec(&geojson).map_err(|error| {
        StormServiceError::InvalidMethod(format!("storm GeoJSON failed: {error}"))
    })?;
    Ok(CachedStormFrame {
        frame: Arc::new(frame),
        canonical: canonical.into(),
        geojson: geojson.into(),
    })
}

fn json_bytes(bytes: bytes::Bytes, content_type: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], bytes).into_response()
}

pub(crate) fn storm_frame_cache_key(
    request: &StormCellsRequest,
    runtime_revision: u64,
) -> Result<String, StormServiceError> {
    let request_bytes = serde_json::to_vec(request).map_err(|error| {
        StormServiceError::InvalidMethod(format!(
            "request could not be canonicalized for exact-frame execution: {error}"
        ))
    })?;
    let mut hash = blake3::Hasher::new();
    hash.update(STORM_CACHE_REVISION.as_bytes());
    hash.update(b"\0");
    let executable_revision = match request.method {
        // Explicit deterministic execution is fully identified by its method
        // version and parameters; native model registration cannot change it.
        StormMethodRequest::Deterministic { .. } => 0,
        StormMethodRequest::Auto { .. } | StormMethodRequest::MachineLearning { .. } => {
            runtime_revision
        }
    };
    hash.update(&executable_revision.to_le_bytes());
    hash.update(&(request_bytes.len() as u64).to_le_bytes());
    hash.update(&request_bytes);
    Ok(hash.finalize().to_hex().to_string())
}

fn storm_cache_identity(request: &StormCellsRequest, key: String) -> StormCacheIdentity {
    StormCacheIdentity {
        key,
        model: request.grid.model.clone(),
        run: request.grid.run.clone(),
        snapshot_id: request.grid.expected_snapshot_id.clone(),
        grid_hash: request.grid.expected_grid_hash.clone(),
        storage_slot: request.grid.storage_slot,
        variable: request.grid.variable.clone(),
        source: request.source.clone(),
    }
}

fn execute_request(
    catalog: &crate::origin_catalog::PublishedStoreCatalog,
    runtime: &StormRuntime,
    request: StormCellsRequest,
) -> Result<StormCellFrame, StormServiceError> {
    request.source.validate().map_err(RegistryError::from)?;
    let snapshot = catalog.snapshot(&request.grid.model, &request.grid.run)?;
    validate_snapshot(&snapshot, &request.grid)?;
    let primary = snapshot.read_surface_2d(request.grid.storage_slot, &request.grid.variable)?;
    validate_source(&snapshot, &primary, &request.source)?;
    let (longitudes, latitudes) = rectilinear_axes(&snapshot)?;

    match request.method {
        StormMethodRequest::Deterministic { config } => {
            validate_reflectivity_field(&primary)?;
            detect_deterministic(request.source, &primary, &longitudes, &latitudes, config)
        }
        StormMethodRequest::Auto { deterministic } => {
            validate_reflectivity_field(&primary)?;
            if let Some(frame) = try_auto_native(
                runtime,
                &snapshot,
                &request.grid,
                &request.source,
                &primary,
                &longitudes,
                &latitudes,
            )? {
                Ok(frame)
            } else {
                detect_deterministic(
                    request.source,
                    &primary,
                    &longitudes,
                    &latitudes,
                    deterministic,
                )
            }
        }
        StormMethodRequest::MachineLearning {
            model_id,
            model_version,
            input_variables,
            supplied_mask_variable,
        } => execute_ml(
            runtime,
            &snapshot,
            &request.grid,
            request.source,
            &longitudes,
            &latitudes,
            &model_id,
            model_version.as_deref(),
            &input_variables,
            supplied_mask_variable.as_deref(),
        ),
    }
}

fn validate_snapshot(
    snapshot: &RunSnapshot,
    reference: &StoredStormGridRef,
) -> Result<(), StormServiceError> {
    let descriptor = snapshot.descriptor();
    if descriptor.snapshot_id != reference.expected_snapshot_id
        || descriptor.grid_hash != reference.expected_grid_hash
    {
        return Err(StormServiceError::SnapshotMismatch);
    }
    Ok(())
}

fn validate_source(
    snapshot: &RunSnapshot,
    field: &SurfaceField2D,
    source: &StormSource,
) -> Result<(), StormServiceError> {
    let valid_at_unix_ms = field.time.valid_unix.checked_mul(1_000).ok_or_else(|| {
        StormServiceError::SourceMismatch("valid time overflows milliseconds".into())
    })?;
    match source {
        StormSource::Mrms {
            product,
            valid_at_unix_ms: claimed_time,
            grid_hash,
        } => {
            if !snapshot
                .descriptor()
                .source_provenance
                .iter()
                .any(|provenance| provenance.provider == "noaa-mrms")
            {
                return Err(StormServiceError::SourceMismatch(
                    "stored run provenance does not identify NOAA MRMS".into(),
                ));
            }
            if *claimed_time != valid_at_unix_ms || grid_hash != &snapshot.descriptor().grid_hash {
                return Err(StormServiceError::SourceMismatch(
                    "MRMS valid time or grid hash differs from the stored snapshot".into(),
                ));
            }
            let selector = source_selector(&field.metadata.selector);
            let actual_product = selector
                .pointer("/mrms/product")
                .or_else(|| field.metadata.selector.pointer("/observation/product"))
                .and_then(Value::as_str);
            if actual_product != Some(product.as_str()) {
                return Err(StormServiceError::SourceMismatch(
                    "MRMS product differs from stored selector metadata".into(),
                ));
            }
        }
        StormSource::NexradLevel2 {
            site,
            volume_at_unix_ms,
            elevation_degrees_milli,
            moment,
        } => {
            if !snapshot
                .descriptor()
                .source_provenance
                .iter()
                .any(|provenance| provenance.provider == "noaa-nexrad-level2")
            {
                return Err(StormServiceError::SourceMismatch(
                    "stored run provenance does not identify NOAA NEXRAD Level II".into(),
                ));
            }
            if *volume_at_unix_ms != valid_at_unix_ms {
                return Err(StormServiceError::SourceMismatch(
                    "Level-II volume time differs from the stored snapshot".into(),
                ));
            }
            let radar = source_selector(&field.metadata.selector)
                .get("radar")
                .ok_or_else(|| {
                    StormServiceError::SourceMismatch(
                        "stored selector is not Level-II radar data".into(),
                    )
                })?;
            let provider = radar.get("provider").and_then(Value::as_str);
            let actual_site = radar.get("site_id").and_then(Value::as_str);
            let actual_moment = radar.get("moment").and_then(Value::as_str);
            if provider != Some("nexrad-level2")
                || !actual_site.is_some_and(|value| value.eq_ignore_ascii_case(site))
                || !actual_moment.is_some_and(|value| value.eq_ignore_ascii_case(moment))
            {
                return Err(StormServiceError::SourceMismatch(
                    "Level-II provider, site, or moment differs from stored selector metadata"
                        .into(),
                ));
            }
            let sweeps = radar
                .get("selected_sweeps")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    StormServiceError::SourceMismatch(
                        "stored Level-II selector omits selected sweeps".into(),
                    )
                })?;
            if sweeps.len() != 1 {
                return Err(StormServiceError::SourceMismatch(
                    "the v1 source contract can identify one Level-II sweep; a multi-sweep composite cannot be assigned a fabricated elevation".into(),
                ));
            }
            let actual_elevation = sweeps[0]
                .get("elevation_angle_deg")
                .and_then(Value::as_f64)
                .map(|value| (value * 1_000.0).round() as i32);
            if actual_elevation != Some(*elevation_degrees_milli) {
                return Err(StormServiceError::SourceMismatch(
                    "Level-II elevation differs from the stored selected sweep".into(),
                ));
            }
        }
    }
    Ok(())
}

fn source_selector(selector: &Value) -> &Value {
    selector.get("source_selector").unwrap_or(selector)
}

fn validate_reflectivity_field(field: &SurfaceField2D) -> Result<(), StormServiceError> {
    if !field.metadata.units.eq_ignore_ascii_case("dbz") {
        return Err(StormServiceError::SourceMismatch(
            "deterministic storm cells require a stored reflectivity field in dBZ".into(),
        ));
    }
    let selector = &field.metadata.selector;
    let semantics = selector
        .pointer("/display/semantics")
        .and_then(Value::as_str);
    let source = source_selector(selector);
    let source_reflectivity = source.get("mrms").is_some()
        || source
            .pointer("/radar/moment")
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case("reflectivity"));
    if semantics != Some("reflectivity") && !source_reflectivity {
        return Err(StormServiceError::SourceMismatch(
            "stored field metadata does not identify reflectivity".into(),
        ));
    }
    Ok(())
}

fn rectilinear_axes(snapshot: &RunSnapshot) -> Result<(Vec<f64>, Vec<f64>), StormServiceError> {
    let grid = snapshot.grid();
    let expected = grid
        .nx
        .checked_mul(grid.ny)
        .ok_or_else(|| StormServiceError::UnsupportedGrid("grid dimensions overflow".into()))?;
    if grid.lat.len() != expected || grid.lon.len() != expected || grid.nx < 2 || grid.ny < 2 {
        return Err(StormServiceError::UnsupportedGrid(
            "coordinate arrays do not match a two-dimensional grid".into(),
        ));
    }
    let longitudes = grid.lon[..grid.nx]
        .iter()
        .copied()
        .map(f64::from)
        .collect::<Vec<_>>();
    let latitudes = (0..grid.ny)
        .map(|y| f64::from(grid.lat[y * grid.nx]))
        .collect::<Vec<_>>();
    for (y, expected_latitude) in latitudes.iter().copied().enumerate() {
        for (x, expected_longitude) in longitudes.iter().copied().enumerate() {
            let index = y * grid.nx + x;
            let latitude = f64::from(grid.lat[index]);
            let longitude = f64::from(grid.lon[index]);
            if !latitude.is_finite()
                || !longitude.is_finite()
                || (latitude - expected_latitude).abs() > RECTILINEAR_TOLERANCE_DEGREES
                || (longitude - expected_longitude).abs() > RECTILINEAR_TOLERANCE_DEGREES
            {
                return Err(StormServiceError::UnsupportedGrid(
                    "curvilinear coordinates require a future contour projection adapter; they are not approximated as rectilinear".into(),
                ));
            }
        }
    }
    Ok((longitudes, latitudes))
}

fn detect_deterministic(
    source: StormSource,
    field: &SurfaceField2D,
    longitudes: &[f64],
    latitudes: &[f64],
    config: DetectionRequest,
) -> Result<StormCellFrame, StormServiceError> {
    Ok(rw_storm::detect_geographic(
        source,
        now_unix_ms(),
        GeographicGrid {
            values_dbz: &field.values,
            longitudes,
            latitudes,
        },
        config.into(),
    )?)
}

#[allow(clippy::too_many_arguments)]
fn try_auto_native(
    runtime: &StormRuntime,
    snapshot: &RunSnapshot,
    grid: &StoredStormGridRef,
    source: &StormSource,
    primary: &SurfaceField2D,
    longitudes: &[f64],
    latitudes: &[f64],
) -> Result<Option<StormCellFrame>, StormServiceError> {
    let native = runtime
        .native
        .read()
        .map_err(|_| StormServiceError::RuntimePoisoned)?;
    let model_ids = runtime
        .models
        .installed()
        .map(|model| model.key.model_id.clone())
        .collect::<BTreeSet<_>>();
    for model_id in model_ids {
        let Ok(model) = runtime.models.active_for_execution(&model_id) else {
            continue;
        };
        if model.manifest.backend != StormModelBackend::NativeRust
            || !native.contains(&model.key)
            || model.manifest.inputs.len() != 1
            || !stored_input_matches(&model.manifest.inputs[0], primary)
        {
            continue;
        }
        let expected = &model.manifest.inputs[0];
        let plane = ModelInputPlane {
            name: &expected.name,
            source: expected.source.clone(),
            field: &expected.field,
            units: &expected.units,
            values: &primary.values,
        };
        let batch = ModelInputBatch {
            source,
            geometry: GridGeometry::Geographic {
                longitudes,
                latitudes,
            },
            planes: std::slice::from_ref(&plane),
        };
        let frame = native.infer_canonical(
            &runtime.models,
            &model.key,
            now_unix_ms(),
            batch,
            DistributionAudience::CompanyCoworker,
        )?;
        validate_snapshot(snapshot, grid)?;
        return Ok(Some(frame));
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn execute_ml(
    runtime: &StormRuntime,
    snapshot: &RunSnapshot,
    grid: &StoredStormGridRef,
    source: StormSource,
    longitudes: &[f64],
    latitudes: &[f64],
    model_id: &str,
    model_version: Option<&str>,
    input_variables: &BTreeMap<String, String>,
    supplied_mask_variable: Option<&str>,
) -> Result<StormCellFrame, StormServiceError> {
    let key = match model_version {
        Some(version) => ModelKey::new(model_id, version)?,
        None => runtime.models.active_for_execution(model_id)?.key.clone(),
    };
    let model = runtime.models.enabled_for_execution(&key)?;
    match model.manifest.backend {
        StormModelBackend::SuppliedMask => {
            if !input_variables.is_empty() {
                return Err(StormServiceError::InvalidMethod(
                    "supplied-mask execution does not accept model input mappings".into(),
                ));
            }
            let variable = supplied_mask_variable.unwrap_or(&grid.variable);
            let mask = snapshot.read_surface_2d(grid.storage_slot, variable)?;
            if !stored_field_identities(&mask.metadata.selector, &mask.metadata.name)
                .into_iter()
                .any(|identity| {
                    normalize_identity(identity) == normalize_identity(&model.manifest.output_name)
                })
            {
                return Err(StormServiceError::InvalidMethod(format!(
                    "stored supplied-mask variable '{variable}' does not match model output '{}'",
                    model.manifest.output_name
                )));
            }
            validate_snapshot(snapshot, grid)?;
            rw_storm_ml::canonicalize_supplied_mask(
                &runtime.models,
                &key,
                source,
                now_unix_ms(),
                GridGeometry::Geographic {
                    longitudes,
                    latitudes,
                },
                MaskOutput::Probabilities {
                    width: snapshot.descriptor().nx,
                    height: snapshot.descriptor().ny,
                    values: &mask.values,
                },
                DistributionAudience::CompanyCoworker,
            )
            .map_err(Into::into)
        }
        StormModelBackend::NativeRust => {
            if supplied_mask_variable.is_some() {
                return Err(StormServiceError::InvalidMethod(
                    "native Rust execution does not accept a supplied mask".into(),
                ));
            }
            let mut fields = Vec::with_capacity(model.manifest.inputs.len());
            for expected in &model.manifest.inputs {
                let variable = input_variables
                    .get(&expected.name)
                    .map(String::as_str)
                    .or_else(|| (model.manifest.inputs.len() == 1).then_some(grid.variable.as_str()))
                    .ok_or_else(|| {
                        StormServiceError::InvalidMethod(format!(
                            "model input '{}' has no stored variable mapping",
                            expected.name
                        ))
                    })?;
                let field = snapshot.read_surface_2d(grid.storage_slot, variable)?;
                if !stored_input_matches(expected, &field) {
                    return Err(StormServiceError::InvalidMethod(format!(
                        "stored variable '{variable}' does not match model input '{}' field/units/source contract",
                        expected.name
                    )));
                }
                fields.push(field);
            }
            let planes = model
                .manifest
                .inputs
                .iter()
                .zip(&fields)
                .map(|(expected, field)| ModelInputPlane {
                    name: &expected.name,
                    source: expected.source.clone(),
                    field: &expected.field,
                    units: &expected.units,
                    values: &field.values,
                })
                .collect::<Vec<_>>();
            let batch = ModelInputBatch {
                source: &source,
                geometry: GridGeometry::Geographic {
                    longitudes,
                    latitudes,
                },
                planes: &planes,
            };
            let native = runtime
                .native
                .read()
                .map_err(|_| StormServiceError::RuntimePoisoned)?;
            let frame = native.infer_canonical(
                &runtime.models,
                &key,
                now_unix_ms(),
                batch,
                DistributionAudience::CompanyCoworker,
            )?;
            validate_snapshot(snapshot, grid)?;
            Ok(frame)
        }
        StormModelBackend::TractOnnx => Err(StormServiceError::InvalidMethod(
            "this build does not execute tract/ONNX artifacts; install a supplied-mask model or compile and register a native Rust backend".into(),
        )),
    }
}

fn stored_input_matches(
    expected: &rw_ops_protocol::StormModelInput,
    field: &SurfaceField2D,
) -> bool {
    if field.metadata.units != expected.units {
        return false;
    }
    let expected_field = normalize_identity(&expected.field);
    stored_field_identities(&field.metadata.selector, &field.metadata.name)
        .into_iter()
        .any(|actual| normalize_identity(actual) == expected_field)
}

fn stored_field_identities<'a>(selector: &'a Value, variable: &'a str) -> Vec<&'a str> {
    let source = source_selector(selector);
    [
        Some(variable),
        source.pointer("/mrms/product").and_then(Value::as_str),
        source
            .pointer("/mrms/parameter_name")
            .and_then(Value::as_str),
        source.pointer("/radar/moment").and_then(Value::as_str),
        selector
            .pointer("/observation/product")
            .and_then(Value::as_str),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn normalize_identity(value: &str) -> String {
    value
        .bytes()
        .filter(|byte| byte.is_ascii_alphanumeric())
        .map(|byte| byte.to_ascii_lowercase() as char)
        .collect()
}

fn authoritative_tracking_method_identity() -> StormMethodIdentity {
    let mut parameters = BTreeMap::new();
    parameters.insert(
        "supplied_geometry".into(),
        "centroid_points_and_tracks_only".into(),
    );
    parameters.insert("decoder_runtime".into(), "pure_rust".into());
    parameters.insert(
        "polygon_geometry".into(),
        "not_supplied_by_level3_product".into(),
    );
    StormMethodIdentity {
        method_id: "noaa-nexrad-level3-nst-sti".into(),
        method_version: "roc-2620003ae-build-24.0".into(),
        kind: StormMethodKind::Authoritative,
        display_name: "NOAA NST/STI tracks".into(),
        description: "Authoritative WSR-88D RPG storm IDs, centroids, history, forecasts, and motion from Level III message 58. The product does not supply polygon outlines; any paired contour remains separately derived.".into(),
        upstream_product: Some("NEXRAD Level III message 58 (NST/STI)".into()),
        model_id: None,
        model_version: None,
        parameters,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level3StormProductKind {
    Tracking,
    Structure,
}

impl Level3StormProductKind {
    fn from_product(product: &NexradStormProduct) -> Self {
        match product {
            NexradStormProduct::StormTracking(_) => Self::Tracking,
            NexradStormProduct::StormStructure(_) => Self::Structure,
        }
    }

    fn method_identity(self) -> StormMethodIdentity {
        match self {
            Self::Tracking => authoritative_tracking_method_identity(),
            Self::Structure => authoritative_structure_method_identity(),
        }
    }

    fn geometry_statement(self) -> &'static str {
        match self {
            Self::Tracking => {
                "NOAA/RPG message 58 supplies storm IDs, centroid positions, history, forecasts, and motion. It does not supply storm polygons; any displayed outline must retain separate deterministic or machine-learning geometry provenance."
            }
            Self::Structure => {
                "NOAA/RPG message 62 supplies storm IDs, centroid positions, and structure attributes. It supplies neither storm polygons nor tracks; any displayed outline or track must retain separate provenance."
            }
        }
    }
}

fn authoritative_structure_method_identity() -> StormMethodIdentity {
    let mut parameters = BTreeMap::new();
    parameters.insert("supplied_geometry".into(), "centroid_points_only".into());
    parameters.insert("decoder_runtime".into(), "pure_rust".into());
    parameters.insert(
        "polygon_geometry".into(),
        "not_supplied_by_level3_product".into(),
    );
    parameters.insert("track_geometry".into(), "not_supplied".into());
    StormMethodIdentity {
        method_id: "noaa-nexrad-level3-ss-nss".into(),
        method_version: "roc-2620003ae-build-24.0".into(),
        kind: StormMethodKind::Authoritative,
        display_name: "NOAA SS/NSS structure".into(),
        description: "Authoritative WSR-88D RPG storm IDs, centroid positions, and storm-structure attributes from Level III message 62. The product supplies neither polygon outlines nor tracks; any paired geometry remains separately derived.".into(),
        upstream_product: Some("NEXRAD Level III message 62 (SS/NSS)".into()),
        model_id: None,
        model_version: None,
        parameters,
    }
}

fn deterministic_method_identity(config: DetectionRequest) -> StormMethodIdentity {
    let mut parameters = BTreeMap::new();
    parameters.insert("threshold_dbz".into(), config.threshold_dbz.to_string());
    parameters.insert(
        "minimum_gate_count".into(),
        config.minimum_gate_count.to_string(),
    );
    parameters.insert(
        "minimum_area_km2".into(),
        config.minimum_area_km2.to_string(),
    );
    parameters.insert(
        "source_input".into(),
        "snapshot_bound_stored_reflectivity".into(),
    );
    parameters.insert("contour_engine".into(), "weather_contours_oirt".into());
    StormMethodIdentity {
        method_id: rw_storm::DETERMINISTIC_METHOD_ID.into(),
        method_version: rw_storm::DETERMINISTIC_METHOD_VERSION.into(),
        kind: StormMethodKind::Deterministic,
        display_name: "Deterministic reflectivity cells".into(),
        description: "Connected stored reflectivity samples with derived threshold-contour geometry. This is not an authoritative NOAA/NCEI polygon product.".into(),
        upstream_product: None,
        model_id: None,
        model_version: None,
        parameters,
    }
}

fn model_method_identity(manifest: &rw_ops_protocol::StormModelManifest) -> StormMethodIdentity {
    let mut parameters = BTreeMap::new();
    parameters.insert("artifact_sha256".into(), manifest.artifact_sha256.clone());
    parameters.insert(
        "backend".into(),
        match manifest.backend {
            StormModelBackend::NativeRust => "native_rust",
            StormModelBackend::TractOnnx => "tract_onnx",
            StormModelBackend::SuppliedMask => "supplied_mask",
        }
        .into(),
    );
    parameters.insert(
        "probability_threshold".into(),
        manifest.probability_threshold.to_string(),
    );
    StormMethodIdentity {
        method_id: "rw-storm-ml".into(),
        method_version: manifest.model_version.clone(),
        kind: StormMethodKind::MachineLearning,
        display_name: manifest.display_name.clone(),
        description: manifest.description.clone(),
        upstream_product: None,
        model_id: Some(manifest.model_id.clone()),
        model_version: Some(manifest.model_version.clone()),
        parameters,
    }
}

/// Approximate live heap footprint for Moka's byte-capacity eviction. This is
/// intentionally a memory accounting estimate, not a scientific/output-size
/// limit; an oversized exact frame is still computed and served even when it
/// cannot remain resident after the single-flight fill.
pub(crate) fn estimated_frame_bytes(frame: &StormCellFrame) -> usize {
    let mut bytes = std::mem::size_of::<StormCellFrame>();
    bytes = bytes.saturating_add(frame.schema.capacity());
    match &frame.source {
        StormSource::Mrms {
            product, grid_hash, ..
        } => {
            bytes = bytes.saturating_add(product.capacity());
            bytes = bytes.saturating_add(grid_hash.capacity());
        }
        StormSource::NexradLevel2 { site, moment, .. } => {
            bytes = bytes.saturating_add(site.capacity());
            bytes = bytes.saturating_add(moment.capacity());
        }
    }
    bytes = bytes
        .saturating_add(frame.method.method_id.capacity())
        .saturating_add(frame.method.method_version.capacity())
        .saturating_add(frame.method.display_name.capacity())
        .saturating_add(frame.method.description.capacity());
    for value in [
        &frame.method.upstream_product,
        &frame.method.model_id,
        &frame.method.model_version,
    ]
    .into_iter()
    .flatten()
    {
        bytes = bytes.saturating_add(value.capacity());
    }
    for (key, value) in &frame.method.parameters {
        bytes = bytes
            .saturating_add(std::mem::size_of::<(String, String)>())
            .saturating_add(key.capacity())
            .saturating_add(value.capacity());
    }
    bytes = bytes.saturating_add(
        frame
            .cells
            .capacity()
            .saturating_mul(std::mem::size_of::<StormCell>()),
    );
    for cell in &frame.cells {
        bytes = bytes.saturating_add(cell.cell_id.capacity());
        if let Some(track_id) = &cell.track_id {
            bytes = bytes.saturating_add(track_id.capacity());
        }
        bytes = bytes.saturating_add(
            cell.rings
                .capacity()
                .saturating_mul(std::mem::size_of::<rw_ops_protocol::ContourRing>()),
        );
        for ring in &cell.rings {
            bytes = bytes.saturating_add(
                ring.points
                    .capacity()
                    .saturating_mul(std::mem::size_of::<GeoPoint>()),
            );
        }
        for (key, value) in &cell.attributes {
            bytes = bytes
                .saturating_add(std::mem::size_of::<(String, String)>())
                .saturating_add(key.capacity())
                .saturating_add(value.capacity());
        }
    }
    bytes = bytes.saturating_add(
        frame
            .warnings
            .capacity()
            .saturating_mul(std::mem::size_of::<String>()),
    );
    for warning in &frame.warnings {
        bytes = bytes.saturating_add(warning.capacity());
    }
    bytes
}

fn frame_to_geojson(frame: &StormCellFrame) -> Result<GeoJsonFeatureCollection, StormServiceError> {
    if frame.schema != STORM_CELL_FRAME_SCHEMA {
        return Err(StormServiceError::InvalidMethod(
            "canonical frame schema changed before GeoJSON conversion".into(),
        ));
    }
    let features = frame
        .cells
        .iter()
        .map(cell_to_geojson)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GeoJsonFeatureCollection {
        r#type: "FeatureCollection",
        schema: STORM_GEOJSON_SCHEMA,
        generated_at_unix_ms: frame.generated_at_unix_ms,
        source: frame.source.clone(),
        method: frame.method.clone(),
        partial: frame.partial,
        warnings: frame.warnings.clone(),
        features,
    })
}

fn cell_to_geojson(cell: &StormCell) -> Result<GeoJsonFeature, StormServiceError> {
    let mut polygons: Vec<Vec<Vec<[f64; 2]>>> = cell
        .rings
        .iter()
        .filter(|ring| !ring.hole)
        .map(|ring| vec![ring_coordinates(&ring.points)])
        .collect();
    if polygons.is_empty() {
        return Err(StormServiceError::InvalidMethod(
            "storm cell has no exterior ring".into(),
        ));
    }
    for hole in cell.rings.iter().filter(|ring| ring.hole) {
        let Some(point) = hole.points.first() else {
            continue;
        };
        let owner = polygons
            .iter()
            .enumerate()
            .filter(|(_, polygon)| point_in_coordinates(point, &polygon[0]))
            .min_by(|(_, left), (_, right)| {
                coordinate_area(&left[0])
                    .partial_cmp(&coordinate_area(&right[0]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(index, _)| index)
            .ok_or_else(|| {
                StormServiceError::InvalidMethod(
                    "contour hole is not contained by an exterior ring".into(),
                )
            })?;
        polygons[owner].push(ring_coordinates(&hole.points));
    }
    let geometry = if polygons.len() == 1 {
        json!({"type": "Polygon", "coordinates": polygons.pop().unwrap()})
    } else {
        json!({"type": "MultiPolygon", "coordinates": polygons})
    };
    Ok(GeoJsonFeature {
        r#type: "Feature",
        id: cell.cell_id.clone(),
        geometry,
        properties: json!({
            "cell_id": cell.cell_id,
            "track_id": cell.track_id,
            "centroid": {
                "latitude": cell.centroid.latitude,
                "longitude": cell.centroid.longitude,
            },
            "area_km2": cell.area_km2,
            "maximum_reflectivity_dbz": cell.maximum_reflectivity_dbz,
            "echo_top_m": cell.echo_top_m,
            "confidence": cell.confidence,
            "attributes": cell.attributes,
        }),
    })
}

fn ring_coordinates(points: &[GeoPoint]) -> Vec<[f64; 2]> {
    points
        .iter()
        .map(|point| [point.longitude, point.latitude])
        .collect()
}

fn point_in_coordinates(point: &GeoPoint, ring: &[[f64; 2]]) -> bool {
    let mut inside = false;
    for pair in ring.windows(2) {
        let ([x1, y1], [x2, y2]) = (pair[0], pair[1]);
        if ((y1 > point.latitude) != (y2 > point.latitude))
            && point.longitude < (x2 - x1) * (point.latitude - y1) / (y2 - y1) + x1
        {
            inside = !inside;
        }
    }
    inside
}

fn coordinate_area(ring: &[[f64; 2]]) -> f64 {
    ring.windows(2)
        .map(|pair| pair[0][0] * pair[1][1] - pair[1][0] * pair[0][1])
        .sum::<f64>()
        .abs()
        * 0.5
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or(1)
}

fn unavailable(request_id: uuid::Uuid) -> Response {
    ProblemDetails::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "STORM_SERVICE_UNAVAILABLE",
        "Storm service is unavailable",
        "Enable private operations state and configure authenticated operations credentials.",
        request_id,
    )
    .into_response()
}

fn storm_problem(error_value: &StormServiceError, request_id: uuid::Uuid) -> Response {
    let (status, code, title, detail) = match error_value {
        StormServiceError::Query(QueryError::UnknownModel(_))
        | StormServiceError::Query(QueryError::UnknownRun { .. })
        | StormServiceError::Query(QueryError::UnknownVariable(_))
        | StormServiceError::Query(QueryError::UnknownStorageSlot(_))
        | StormServiceError::Model(RegistryError::NotInstalled(_))
        | StormServiceError::Model(RegistryError::NoActiveVersion(_)) => (
            StatusCode::NOT_FOUND,
            "STORM_SOURCE_NOT_FOUND",
            "Storm source was not found",
            "The requested stored generation, field, time, or model is unavailable.".to_string(),
        ),
        StormServiceError::SnapshotMismatch => (
            StatusCode::CONFLICT,
            "STORM_SNAPSHOT_CHANGED",
            "Stored generation changed",
            "Refresh the run descriptor and explicitly retry against its new snapshot identity."
                .to_string(),
        ),
        StormServiceError::UnsupportedSchema
        | StormServiceError::SourceMismatch(_)
        | StormServiceError::UnsupportedGrid(_)
        | StormServiceError::InvalidMethod(_)
        | StormServiceError::InvalidLevel3Request(_)
        | StormServiceError::Level3Decode(_)
        | StormServiceError::Detection(_)
        | StormServiceError::Model(
            RegistryError::Disabled(_)
            | RegistryError::IncompatibleInput(_)
            | RegistryError::InvalidOutput(_)
            | RegistryError::BackendUnavailable(_)
            | RegistryError::NativeBackendMissing(_)
            | RegistryError::DistributionDenied { .. },
        )
        | StormServiceError::Query(
            QueryError::InvalidRequest(_)
            | QueryError::WrongVariableKind { .. }
            | QueryError::LimitExceeded { .. },
        ) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "STORM_REQUEST_INVALID",
            "Storm request is not executable",
            error_value.to_string(),
        ),
        _ => {
            error!(request_id = %request_id, error = %error_value, "private storm request failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "STORM_PROCESSING_FAILED",
                "Storm processing failed",
                "The server could not safely complete the requested storm method.".to_string(),
            )
        }
    };
    ProblemDetails::new(status, code, title, detail, request_id).into_response()
}

fn execution_problem(error_value: &ExecutionError, request_id: uuid::Uuid) -> Response {
    let (status, code, title, detail) = match error_value {
        ExecutionError::AdmissionTimeout => (
            StatusCode::SERVICE_UNAVAILABLE,
            "STORM_BUSY",
            "Storm workers are busy",
            "Retry after active contour work completes.",
        ),
        ExecutionError::ExecutionTimeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "STORM_DEADLINE_EXCEEDED",
            "Storm processing timed out",
            "The node's configured heavy-job deadline expired.",
        ),
        ExecutionError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "SHUTTING_DOWN",
            "Service is shutting down",
            "Retry against a healthy node.",
        ),
        ExecutionError::Join(error) => {
            error!(request_id = %request_id, %error, "storm worker failed");
            return ProblemDetails::internal(request_id).into_response();
        }
    };
    ProblemDetails::new(status, code, title, detail, request_id).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, header};
    use rustwx_core::{GridShape, LatLonGrid};
    use rw_observations::{
        GridPlane, ObservationFamily, ObservationFrame, write_observation_frame_with_limit,
    };
    use rw_ops_protocol::{
        ModelInputSource, STORM_MODEL_MANIFEST_SCHEMA, StormModelInput, StormModelManifest,
    };
    use rw_storm_ml::{ModelRegistry, ModelUsePolicy};
    use sha2::{Digest, Sha256};
    use std::fs;
    use tower::ServiceExt as _;

    const READ_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";
    const VALID_UNIX: i64 = 1_700_000_000;
    const PRODUCT: &str = "MergedReflectivityQCComposite";

    struct Fixture {
        _directory: tempfile::TempDir,
        app: Router,
        config: crate::AppConfig,
        request: Value,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let artifact_root = directory.path().join("artifacts");
        let operations_root = directory.path().join("operations");
        fs::create_dir_all(&store_root).unwrap();
        fs::create_dir_all(&artifact_root).unwrap();

        let nx = 5;
        let ny = 5;
        let mut latitudes = Vec::new();
        let mut longitudes = Vec::new();
        for y in 0..ny {
            for x in 0..nx {
                latitudes.push(35.0 + y as f32 * 0.1);
                longitudes.push(-98.0 + x as f32 * 0.1);
            }
        }
        let grid = LatLonGrid::new(GridShape::new(nx, ny).unwrap(), latitudes, longitudes).unwrap();
        let mut reflectivity = vec![10.0_f32; nx * ny];
        let mut probability = vec![0.0_f32; nx * ny];
        for y in 1..4 {
            for x in 1..4 {
                reflectivity[y * nx + x] = 50.0;
                probability[y * nx + x] = 0.9;
            }
        }
        let frame = ObservationFrame {
            family: ObservationFamily::Mrms,
            collection: "conus".into(),
            product: PRODUCT.into(),
            valid_unix: VALID_UNIX,
            grid,
            projection: None,
            planes: vec![
                GridPlane {
                    name: "mrms_reflectivity".into(),
                    units: "dBZ".into(),
                    selector: json!({
                        "mrms": {
                            "product": PRODUCT,
                            "parameter_name": "ReflectivityAtLowestAltitude"
                        }
                    }),
                    values: reflectivity,
                },
                GridPlane {
                    name: "storm_probability".into(),
                    units: "1".into(),
                    selector: json!({"derived": {"field": "storm_probability"}}),
                    values: probability,
                },
            ],
            provenance_provider: "noaa-mrms".into(),
            provenance_roles: vec!["radar".into(), "mosaic".into()],
            provenance_products: vec!["merged-reflectivity-qc-composite".into()],
        };
        let stored = write_observation_frame_with_limit(&store_root, &frame, nx * ny).unwrap();

        let read_tokens = directory.path().join("storm-read.tokens");
        crate::test_support::write_private_file(&read_tokens, READ_TOKEN);
        fs::create_dir_all(&operations_root).unwrap();
        let artifact = b"rw-server supplied-mask test fixture";
        let manifest = StormModelManifest {
            schema: STORM_MODEL_MANIFEST_SCHEMA.into(),
            model_id: "fixture-mask".into(),
            model_version: "1".into(),
            backend: StormModelBackend::SuppliedMask,
            artifact_sha256: format!("{:x}", Sha256::digest(artifact)),
            display_name: "Fixture supplied mask".into(),
            description:
                "Stored probability mask used to prove the private RW Server model boundary.".into(),
            inputs: vec![StormModelInput {
                name: "reflectivity".into(),
                source: ModelInputSource::MrmsProduct,
                field: "mrms_reflectivity".into(),
                units: "dBZ".into(),
                minimum: Some(-20.0),
                maximum: Some(90.0),
                missing_value: None,
            }],
            output_name: "storm_probability".into(),
            probability_threshold: 0.5,
            minimum_area_km2: Some(0.0),
            producer: "RW Server test".into(),
            license: Some("private test fixture".into()),
            training_provenance: Some("synthetic square fixture; no observational data".into()),
        };
        let key = ModelKey::new("fixture-mask", "1").unwrap();
        let mut registry = ModelRegistry::open(
            std::path::absolute(operations_root.join("storm-models")).unwrap(),
            ModelLimits::default(),
        )
        .unwrap();
        registry
            .install(
                manifest,
                ModelUsePolicy::private_company("Test attribution", "test rights"),
                artifact.as_slice(),
            )
            .unwrap();
        registry.enable(&key).unwrap();
        registry.activate(&key).unwrap();
        drop(registry);
        let mut config = crate::AppConfig::default();
        config.server.store_root = store_root;
        config.server.artifact_root = artifact_root;
        config.server.cache_root = directory.path().join("cache");
        config.operations.enabled = true;
        config.operations.root = operations_root;
        config.auth.ops_read_token_file = Some(read_tokens);
        config.validate(false).unwrap();
        let state = AppState::new(config.clone(), crate::TokenSet::default()).unwrap();
        let descriptor = state
            .catalog
            .snapshot(&stored.model, &stored.run)
            .unwrap()
            .descriptor()
            .clone();
        let request = json!({
            "schema": STORM_REQUEST_SCHEMA,
            "grid": {
                "model": stored.model,
                "run": stored.run,
                "expected_snapshot_id": descriptor.snapshot_id,
                "expected_grid_hash": descriptor.grid_hash,
                "storage_slot": stored.storage_slot,
                "variable": "mrms_reflectivity"
            },
            "source": {
                "kind": "mrms",
                "product": PRODUCT,
                "valid_at_unix_ms": VALID_UNIX * 1000,
                "grid_hash": stored.grid_hash
            },
            "method": {"kind": "auto"}
        });
        Fixture {
            _directory: directory,
            app: crate::build_router(state).unwrap(),
            config,
            request,
        }
    }

    async fn request(
        app: &Router,
        method: Method,
        uri: &str,
        token: Option<&str>,
        value: Option<&Value>,
    ) -> Response {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let body = match value {
            Some(value) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(serde_json::to_vec(value).unwrap())
            }
            None => Body::empty(),
        };
        app.clone()
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn json_body(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn assert_private(response: &Response) {
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store, private"
        );
        assert_eq!(response.headers().get(header::PRAGMA).unwrap(), "no-cache");
    }

    #[tokio::test]
    async fn stored_mrms_cells_are_authenticated_snapshot_bound_and_geojson_ready() {
        let fixture = fixture();
        let denied = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            None,
            Some(&fixture.request),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_private(&denied);

        let response = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&fixture.request),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private(&response);
        let frame: StormCellFrame = serde_json::from_value(json_body(response).await).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.method.kind, StormMethodKind::Deterministic);
        assert_eq!(frame.cells.len(), 1);
        assert_eq!(
            frame.cells[0].attributes["geometry_provenance"],
            "derived_reflectivity_threshold_contour"
        );

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let geojson = request(
            &fixture.app,
            Method::POST,
            &format!("{STORM_CELLS_PATH}?format=geojson"),
            Some(READ_TOKEN),
            Some(&fixture.request),
        )
        .await;
        assert_eq!(geojson.status(), StatusCode::OK);
        assert_private(&geojson);
        let geojson = json_body(geojson).await;
        assert_eq!(geojson["type"], "FeatureCollection");
        assert_eq!(geojson["schema"], STORM_GEOJSON_SCHEMA);
        assert_eq!(
            geojson["generated_at_unix_ms"], frame.generated_at_unix_ms,
            "canonical and GeoJSON responses must project the same cached frame"
        );
        assert_eq!(geojson["features"][0]["geometry"]["type"], "Polygon");
        let first = &geojson["features"][0]["geometry"]["coordinates"][0][0];
        assert!(
            first[0].as_f64().unwrap() < 0.0,
            "GeoJSON longitude comes first"
        );
        assert!(
            first[1].as_f64().unwrap() > 0.0,
            "GeoJSON latitude comes second"
        );
    }

    #[tokio::test]
    async fn catalog_is_private_and_never_calls_nst_an_outline() {
        let fixture = fixture();
        let status = request(
            &fixture.app,
            Method::GET,
            STORM_STATUS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(status.status(), StatusCode::OK);
        assert_private(&status);
        let status = json_body(status).await;
        assert_eq!(status["direct_client_grid_uploads"], false);
        assert_eq!(status["exact_frame_single_flight"], true);
        assert_eq!(
            status["frame_cache_scope"],
            "process_memory_plus_verified_atomic_disk"
        );
        assert_eq!(status["frame_cache_revision"], STORM_CACHE_REVISION);
        assert_eq!(status["durable_cache"]["ready"], true);
        assert!(status["frame_cache_max_bytes"].as_u64().unwrap() > 0);
        let nst = status["source_linkage"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["source"] == "nexrad_level3_nst_sti_message_58")
            .unwrap();
        assert_eq!(nst["available"], true);
        assert_eq!(nst["geometry"], "authoritative_centroids_tracks_only");

        let methods = request(
            &fixture.app,
            Method::GET,
            STORM_METHODS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(methods.status(), StatusCode::OK);
        assert_private(&methods);
        let methods = json_body(methods).await;
        let methods = methods["methods"].as_array().unwrap();
        let authoritative = methods
            .iter()
            .find(|method| method["kind"] == "authoritative")
            .unwrap();
        assert!(
            authoritative["description"]
                .as_str()
                .unwrap()
                .contains("does not supply polygon outlines")
        );
        let deterministic = methods
            .iter()
            .find(|method| method["kind"] == "deterministic")
            .unwrap();
        assert!(
            deterministic["description"]
                .as_str()
                .unwrap()
                .contains("not an authoritative NOAA/NCEI polygon")
        );

        let models = request(
            &fixture.app,
            Method::GET,
            STORM_MODELS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        assert_eq!(models.status(), StatusCode::OK);
        assert_private(&models);
        let models = json_body(models).await;
        assert!(models["limits"]["maximum_installed_versions"].is_null());
        assert!(models["limits"]["maximum_activation_history_entries_per_model"].is_null());
        assert!(models["limits"]["maximum_grid_width"].is_null());
        assert!(models["limits"]["maximum_grid_height"].is_null());
        assert!(models["limits"]["maximum_grid_points"].is_null());
        assert!(models["limits"]["maximum_label_work_points"].is_null());
        assert_eq!(models["limits"]["maximum_input_planes"], 64);
        assert_eq!(
            models["limits"]["maximum_artifact_bytes"],
            4_u64 * 1024 * 1024 * 1024
        );
        assert!(
            models["limits"]["null_policy"]
                .as_str()
                .unwrap()
                .contains("no configured ceiling")
        );
        assert_eq!(models["models"][0]["manifest"]["model_id"], "fixture-mask");
        assert_eq!(models["models"][0]["enabled"], true);
        assert_eq!(models["models"][0]["active"], true);
        assert_eq!(models["models"][0]["executable_on_this_node"], true);
        assert_eq!(
            models["models"][0]["execution_mode"],
            "stored_probability_mask"
        );
    }

    #[tokio::test]
    async fn level3_decode_is_private_bounded_and_rejects_non_products_honestly() {
        let fixture = fixture();
        let body = json!({
            "schema": NEXRAD_LEVEL3_DECODE_REQUEST_SCHEMA,
            "site_hint": "KTLX",
            "product_base64": BASE64_STANDARD.encode(b"not a Level III product")
        });
        let denied = request(
            &fixture.app,
            Method::POST,
            NEXRAD_LEVEL3_STORM_DECODE_PATH,
            None,
            Some(&body),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        assert_private(&denied);

        let rejected = request(
            &fixture.app,
            Method::POST,
            NEXRAD_LEVEL3_STORM_DECODE_PATH,
            Some(READ_TOKEN),
            Some(&body),
        )
        .await;
        assert_eq!(rejected.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_private(&rejected);
        let problem = json_body(rejected).await;
        assert_eq!(problem["code"], "STORM_REQUEST_INVALID");
        assert!(
            problem["detail"]
                .as_str()
                .unwrap()
                .contains("could not be decoded")
        );
    }

    #[test]
    fn level3_method_identity_never_assigns_tracks_or_polygons_to_structure() {
        fn identity(message_code: i16, supplied_geometry: &str) -> Value {
            json!({
                "message_code": message_code,
                "mnemonic": if message_code == 58 { "NST/STI" } else { "NSS/SS" },
                "product_version": 1,
                "radar_site": {"site_id": "KTLX", "source": "caller_hint"},
                "radar_location": {"latitude_degrees": 35.333, "longitude_degrees": -97.278},
                "radar_height_feet": 1277,
                "message_at_unix_ms": 1_700_000_000_000_i64,
                "volume_scan_at_unix_ms": 1_700_000_000_000_i64,
                "generated_at_unix_ms": 1_700_000_001_000_i64,
                "message_sequence": 1,
                "volume_scan_number": 2,
                "source_id": 3,
                "destination_id": 4,
                "operational_mode": 2,
                "volume_coverage_pattern": 212,
                "compression": "none",
                "transport": {"wmo_heading": null, "wmo_origin": null, "product_identifier": null},
                "provenance": {
                    "producer": "WSR-88D Radar Operations Center",
                    "format_specification": {"authority": "ROC", "document": "2620001AD", "build": "24.0", "issued": "2025-08-19", "references": "test"},
                    "product_specification": {"authority": "ROC", "document": "2620003AE", "build": "24.0", "issued": "2025-08-19", "references": "test"},
                    "supplied_geometry": supplied_geometry,
                    "geometry_statement": "fixture"
                },
                "validation_notices": []
            })
        }
        let tracking_product: NexradStormProduct = serde_json::from_value(json!({
            "product": "storm_tracking",
            "identity": identity(58, "centroid_points_and_tracks"),
            "cells": [],
            "forecast_interval_minutes": null,
            "number_of_past_volumes": null
        }))
        .unwrap();
        let structure_product: NexradStormProduct = serde_json::from_value(json!({
            "product": "storm_structure",
            "identity": identity(62, "centroid_points_only"),
            "cells": [],
            "reported_cell_count": 0
        }))
        .unwrap();
        assert_eq!(
            Level3StormProductKind::from_product(&tracking_product),
            Level3StormProductKind::Tracking
        );
        assert_eq!(
            Level3StormProductKind::from_product(&structure_product),
            Level3StormProductKind::Structure
        );

        let tracking = Level3StormProductKind::Tracking.method_identity();
        assert_eq!(tracking.method_id, "noaa-nexrad-level3-nst-sti");
        assert_eq!(
            tracking.parameters["supplied_geometry"],
            "centroid_points_and_tracks_only"
        );
        assert!(
            Level3StormProductKind::Tracking
                .geometry_statement()
                .contains("history, forecasts, and motion")
        );
        assert!(
            Level3StormProductKind::Tracking
                .geometry_statement()
                .contains("does not supply storm polygons")
        );

        let structure = Level3StormProductKind::Structure.method_identity();
        assert_eq!(structure.method_id, "noaa-nexrad-level3-ss-nss");
        assert_eq!(
            structure.parameters["supplied_geometry"],
            "centroid_points_only"
        );
        assert_eq!(structure.parameters["track_geometry"], "not_supplied");
        assert!(
            Level3StormProductKind::Structure
                .geometry_statement()
                .contains("neither storm polygons nor tracks")
        );
        assert!(!structure.description.contains("history"));
        assert!(!structure.description.contains("forecast"));
        assert!(!structure.description.contains("motion"));
    }

    #[tokio::test]
    async fn concurrent_identical_requests_share_one_compute_and_atomic_store() {
        let fixture = fixture();
        let mut request_value = fixture.request.clone();
        request_value["method"] = json!({"kind": "deterministic"});
        let mut tasks = tokio::task::JoinSet::new();
        for _ in 0..24 {
            let app = fixture.app.clone();
            let request_value = request_value.clone();
            tasks.spawn(async move {
                let response = request(
                    &app,
                    Method::POST,
                    STORM_CELLS_PATH,
                    Some(READ_TOKEN),
                    Some(&request_value),
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                to_bytes(response.into_body(), 4 * 1024 * 1024)
                    .await
                    .unwrap()
            });
        }
        let mut bodies = Vec::new();
        while let Some(result) = tasks.join_next().await {
            bodies.push(result.unwrap());
        }
        assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));

        let status = request(
            &fixture.app,
            Method::GET,
            STORM_STATUS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        let status = json_body(status).await;
        assert_eq!(status["durable_cache"]["entries"], 1);
        assert_eq!(status["durable_cache"]["atomic_store_writes"], 1);
    }

    #[tokio::test]
    async fn request_after_restart_is_a_durable_byte_exact_cache_hit() {
        let fixture = fixture();
        let mut request_value = fixture.request.clone();
        request_value["method"] = json!({"kind": "deterministic"});
        let first = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&request_value),
        )
        .await;
        let first = to_bytes(first.into_body(), 4 * 1024 * 1024).await.unwrap();

        let restarted_state =
            AppState::new(fixture.config.clone(), crate::TokenSet::default()).unwrap();
        let restarted = crate::build_router(restarted_state).unwrap();
        let second = request(
            &restarted,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&request_value),
        )
        .await;
        let second = to_bytes(second.into_body(), 4 * 1024 * 1024).await.unwrap();
        assert_eq!(first, second);

        let status = request(
            &restarted,
            Method::GET,
            STORM_STATUS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        let status = json_body(status).await;
        assert_eq!(status["durable_cache"]["disk_hits"], 1);
        assert_eq!(status["durable_cache"]["atomic_store_writes"], 0);
    }

    #[tokio::test]
    async fn enabled_supplied_mask_is_canonicalized_from_the_same_stored_grid() {
        let fixture = fixture();
        let mut request_value = fixture.request.clone();
        request_value["method"] = json!({
            "kind": "machine_learning",
            "model_id": "fixture-mask",
            "model_version": "1",
            "supplied_mask_variable": "storm_probability"
        });
        let response = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&request_value),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private(&response);
        let frame: StormCellFrame = serde_json::from_value(json_body(response).await).unwrap();
        frame.validate().unwrap();
        assert_eq!(frame.method.kind, StormMethodKind::MachineLearning);
        assert_eq!(frame.method.model_id.as_deref(), Some("fixture-mask"));
        assert_eq!(frame.method.model_version.as_deref(), Some("1"));
        assert_eq!(frame.cells.len(), 1);
        assert!((frame.cells[0].confidence.unwrap() - 0.9).abs() < 1.0e-5);
        assert_eq!(frame.cells[0].maximum_reflectivity_dbz, None);
        assert_eq!(
            frame.cells[0].attributes["geometry_provenance"],
            "model_probability_threshold_contour"
        );
    }

    #[tokio::test]
    async fn changed_snapshot_invalidates_durable_source_and_oversized_body_fails_closed() {
        let fixture = fixture();
        let mut original = fixture.request.clone();
        original["method"] = json!({"kind": "deterministic"});
        let populated = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&original),
        )
        .await;
        assert_eq!(populated.status(), StatusCode::OK);

        let mut changed = original;
        changed["grid"]["expected_snapshot_id"] = json!("a".repeat(64));
        let response = request(
            &fixture.app,
            Method::POST,
            STORM_CELLS_PATH,
            Some(READ_TOKEN),
            Some(&changed),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_private(&response);
        assert_eq!(json_body(response).await["code"], "STORM_SNAPSHOT_CHANGED");
        let status = request(
            &fixture.app,
            Method::GET,
            STORM_STATUS_PATH,
            Some(READ_TOKEN),
            None,
        )
        .await;
        let status = json_body(status).await;
        assert_eq!(status["durable_cache"]["entries"], 1);
        assert_eq!(status["durable_cache"]["disk_hits"], 0);

        let response = fixture
            .app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(STORM_CELLS_PATH)
                    .header(header::AUTHORIZATION, format!("Bearer {READ_TOKEN}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; MAX_STORM_REQUEST_BYTES + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_private(&response);
    }
}

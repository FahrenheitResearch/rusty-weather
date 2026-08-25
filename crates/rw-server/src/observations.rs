use std::io::Write;

use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use rustwx_core::{GridProjection, GridShape, LatLonGrid, MAX_GRID_CELLS};
use rw_observations::{
    GridPlane, MrmsIngestRequest, NexradIngestOptions, ObservationDisplayHint, ObservationError,
    ObservationFamily, ObservationFrame, RadarGridMode, RadarMoment, RadarMosaicRequest,
    SimulatedRadarRequest, StoredFrameRef, build_and_store_radar_mosaic,
    derive_and_store_simulated_radar, encode_grid_blob, encode_plane_blob, ingest_mrms_latest,
    ingest_nexrad_level2, observation_display_hint_from_selector,
    write_observation_frame_with_limit,
};
use rw_query::{QueryError, RunDescriptor, TimePoint};
use rw_store::reader::HourReader;
use serde::{Deserialize, Serialize};
use tracing::error;
use uuid::Uuid;

use crate::config::LimitsConfig;
use crate::problem::ProblemDetails;
use crate::routes::RequestId;
use crate::{AppState, CancellationToken, ExecutionError, JobError};

// This is an address-space-derived format boundary, not an operator safety or
// product-resolution ceiling. Observation ingest otherwise preserves native
// grids and lets allocation/OS capacity determine the practical limit.
const MAXIMUM_OBSERVATION_GRID_CELLS: usize = MAX_GRID_CELLS;
// Input-count is not a scientific or allocation budget. The authenticated
// request-body guard, opened-source allocations, target-grid address-space
// validation, and heavy-job semaphore provide the real resource boundaries.
const MAXIMUM_MOSAIC_INPUTS: usize = usize::MAX;
const OBSERVATION_GRID_CONTENT_TYPE: &str = "application/vnd.rusty-weather.observation-grid+f32";
const OBSERVATION_PLANE_CONTENT_TYPE: &str = "application/vnd.rusty-weather.observation-plane+f32";

pub(crate) fn read_router() -> Router<AppState> {
    Router::new()
        .route("/v1/observations/capabilities", get(capabilities))
        .route("/v1/observations", get(observation_catalog))
        .route(
            "/v1/observations/{model}/{run}/frames",
            get(observation_frames),
        )
        .route(
            "/v1/observations/{model}/{run}/grid.bin",
            get(observation_grid_binary),
        )
        .route(
            "/v1/observations/{model}/{run}/frames/{storage_slot}/{variable}",
            get(observation_plane_binary),
        )
}

pub(crate) fn write_router(limits: &LimitsConfig) -> Router<AppState> {
    let normal = Router::new()
        .route("/v1/observations/mrms/latest", post(submit_mrms_latest))
        .route("/v1/observations/radar/mosaic", post(submit_radar_mosaic))
        .route(
            "/v1/observations/wrf-radar/derive",
            post(submit_simulated_radar),
        );
    let large = Router::new()
        .route(
            "/v1/observations/nexrad/level2",
            post(submit_nexrad_level2).layer(DefaultBodyLimit::max(
                limits.observation_level2_upload_bytes,
            )),
        )
        .route(
            "/v1/observations/generated",
            post(submit_generated_frame).layer(DefaultBodyLimit::max(
                limits.observation_generated_upload_bytes,
            )),
        );
    normal.merge(large)
}

#[derive(Debug, Serialize)]
struct ObservationCapabilitiesResponse {
    schema: &'static str,
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
    binary_grid_content_type: &'static str,
    binary_plane_content_type: &'static str,
    display_metadata: bool,
    curvilinear_grid_mesh: bool,
    non_finite_transparency: bool,
}

async fn capabilities(State(state): State<AppState>) -> Json<ObservationCapabilitiesResponse> {
    Json(capabilities_response(&state.config.limits))
}

fn capabilities_response(limits: &LimitsConfig) -> ObservationCapabilitiesResponse {
    ObservationCapabilitiesResponse {
        schema: "rw-server.observation-capabilities.v1",
        satellite_store_delivery: true,
        simsat_store_delivery: true,
        arbitrary_generated_grid_import: true,
        mrms_latest_ingest: true,
        nexrad_level2_ingest: true,
        single_site_composite: true,
        multi_source_mosaic: true,
        wrf_composite_reflectivity: true,
        wrf_pressure_level_reflectivity: true,
        wrf_echo_top: true,
        wrf_vil: true,
        wrf_virtual_radar_ppi: true,
        maximum_grid_cells: MAXIMUM_OBSERVATION_GRID_CELLS,
        maximum_mosaic_inputs: MAXIMUM_MOSAIC_INPUTS,
        maximum_level2_upload_bytes: limits.observation_level2_upload_bytes,
        maximum_generated_upload_bytes: limits.observation_generated_upload_bytes,
        binary_grid_content_type: OBSERVATION_GRID_CONTENT_TYPE,
        binary_plane_content_type: OBSERVATION_PLANE_CONTENT_TYPE,
        display_metadata: true,
        curvilinear_grid_mesh: true,
        non_finite_transparency: true,
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObservationCatalogQuery {
    model: Option<String>,
    /// Optional caller-selected response limit. Omitting it returns every
    /// observation run the store can enumerate; zero is never meaningful.
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ObservationKind {
    Satellite,
    SimulatedSatellite,
    Mrms,
    Radar,
    RadarMosaic,
    SimulatedRadar,
    Generated,
}

#[derive(Debug, Serialize)]
struct ObservationRunSummary {
    kind: ObservationKind,
    run: RunDescriptor,
    variable_count: usize,
}

#[derive(Debug, Serialize)]
struct ObservationCatalogResponse {
    schema: &'static str,
    runs: Vec<ObservationRunSummary>,
    truncated: bool,
}

async fn observation_catalog(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Query(query): Query<ObservationCatalogQuery>,
) -> Response {
    if let Err(detail) = validate_observation_catalog_limit(query.limit) {
        return ProblemDetails::new(
            StatusCode::BAD_REQUEST,
            "INVALID_OBSERVATION_CATALOG_REQUEST",
            "Invalid observation catalog request",
            detail,
            request_id.0,
        )
        .into_response();
    }
    let catalog = state.catalog.clone();
    match state
        .run_heavy_sync(move || {
            let mut output = Vec::new();
            for model_entry in catalog.list_models()? {
                if query
                    .model
                    .as_ref()
                    .is_some_and(|model| model != &model_entry.model)
                {
                    continue;
                }
                let Some(kind) = observation_kind_for_model(&model_entry.model) else {
                    continue;
                };
                for run in catalog.list_runs(&model_entry.model)? {
                    output
                        .try_reserve(1)
                        .map_err(|error| QueryError::Allocation {
                            what: "observation catalog response",
                            detail: error.to_string(),
                        })?;
                    output.push(ObservationRunSummary {
                        kind,
                        run: run.run,
                        variable_count: run.variable_count,
                    });
                }
            }
            let response = finalize_observation_catalog(output, query.limit);
            encode_observation_catalog(&response)
        })
        .await
    {
        Ok(Ok(body)) => (
            [(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            )],
            body,
        )
            .into_response(),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

fn validate_observation_catalog_limit(limit: Option<usize>) -> Result<(), &'static str> {
    if limit == Some(0) {
        Err("limit must be greater than zero when supplied")
    } else {
        Ok(())
    }
}

struct CheckedJsonBuffer(Vec<u8>);

impl Write for CheckedJsonBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.try_reserve(bytes.len()).map_err(|error| {
            std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!("observation catalog JSON allocation failed: {error}"),
            )
        })?;
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encode_observation_catalog(
    response: &ObservationCatalogResponse,
) -> Result<Vec<u8>, QueryError> {
    let mut body = CheckedJsonBuffer(Vec::new());
    serde_json::to_writer(&mut body, response)?;
    Ok(body.0)
}

fn finalize_observation_catalog(
    mut runs: Vec<ObservationRunSummary>,
    limit: Option<usize>,
) -> ObservationCatalogResponse {
    let truncated = limit.is_some_and(|limit| runs.len() > limit);
    if let Some(limit) = limit {
        runs.truncate(limit);
    }
    ObservationCatalogResponse {
        schema: "rw-server.observation-catalog.v1",
        runs,
        truncated,
    }
}

#[derive(Debug, Deserialize)]
struct ObservationRunPath {
    model: String,
    run: String,
}

#[derive(Debug, Serialize)]
struct ObservationVariableSummary {
    name: String,
    units: String,
    kind: String,
    selector: serde_json::Value,
    display: ObservationDisplayHint,
    available_slots: Vec<u16>,
}

#[derive(Debug, Serialize)]
struct ObservationFramesResponse {
    schema: &'static str,
    kind: ObservationKind,
    run: RunDescriptor,
    frames: Vec<TimePoint>,
    variables: Vec<ObservationVariableSummary>,
}

async fn observation_frames(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ObservationRunPath>,
) -> Response {
    let Some(kind) = observation_kind_for_model(&path.model) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let catalog = state.catalog.clone();
    match state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&path.model, &path.run)?;
            let variables = snapshot
                .variable_capabilities()?
                .into_iter()
                .map(|variable| {
                    let display = observation_display_hint_from_selector(
                        &variable.name,
                        &variable.units,
                        &variable.selector,
                    );
                    ObservationVariableSummary {
                        name: variable.name,
                        units: variable.units,
                        kind: variable.kind,
                        selector: variable.selector,
                        display,
                        available_slots: variable.available_slots,
                    }
                })
                .collect();
            Ok::<_, QueryError>(ObservationFramesResponse {
                schema: "rw-server.observation-frames.v1",
                kind,
                run: snapshot.descriptor().clone(),
                frames: snapshot.time_axis().to_vec(),
                variables,
            })
        })
        .await
    {
        Ok(Ok(response)) => Json(response).into_response(),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn observation_grid_binary(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ObservationRunPath>,
) -> Response {
    if observation_kind_for_model(&path.model).is_none() {
        return ProblemDetails::not_found(request_id.0).into_response();
    }
    let catalog = state.catalog.clone();
    match state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&path.model, &path.run)?;
            let etag = format!("\"{}-grid\"", snapshot.descriptor().snapshot_id);
            Ok::<_, QueryError>((encode_grid_blob(snapshot.grid()), etag))
        })
        .await
    {
        Ok(Ok((bytes, etag))) => binary_response(bytes, OBSERVATION_GRID_CONTENT_TYPE, &etag, None),
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ObservationPlanePath {
    model: String,
    run: String,
    storage_slot: u16,
    variable: String,
}

async fn observation_plane_binary(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ObservationPlanePath>,
) -> Response {
    if observation_kind_for_model(&path.model).is_none() {
        return ProblemDetails::not_found(request_id.0).into_response();
    }
    let Some(variable_name) = path
        .variable
        .strip_suffix(".bin")
        .filter(|variable| !variable.is_empty())
        .map(str::to_owned)
    else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let catalog = state.catalog.clone();
    match state
        .run_heavy_sync(move || {
            let snapshot = catalog.snapshot(&path.model, &path.run)?;
            let time = snapshot.timepoint(path.storage_slot)?;
            let entry = snapshot
                .manifest()
                .hours
                .get(&path.storage_slot)
                .ok_or(QueryError::UnknownStorageSlot(path.storage_slot))?;
            let file = snapshot
                .store_root()
                .join(&path.model)
                .join(&path.run)
                .join(&entry.file);
            let reader = HourReader::open(&file)?;
            let variable = reader
                .variable(&variable_name)
                .ok_or_else(|| QueryError::UnknownVariable(variable_name.clone()))?;
            if variable.kind != "surface2d" {
                return Err(QueryError::WrongVariableKind {
                    variable: variable_name,
                    expected: "surface2d",
                    actual: variable.kind.clone(),
                });
            }
            let display = observation_display_hint_from_selector(
                &variable.name,
                &variable.units,
                &variable.selector,
            );
            let values = reader.read_full_2d(&variable.name)?;
            let bytes = encode_plane_blob(
                &variable.name,
                &variable.units,
                time.valid_unix,
                snapshot.grid().nx,
                snapshot.grid().ny,
                &values,
            )
            .map_err(|error| QueryError::InvalidRequest(error.to_string()))?;
            let etag = format!(
                "\"{}-{}-{}\"",
                snapshot.descriptor().snapshot_id,
                path.storage_slot,
                variable.id
            );
            Ok::<_, QueryError>((bytes, etag, display))
        })
        .await
    {
        Ok(Ok((bytes, etag, display))) => {
            binary_response(bytes, OBSERVATION_PLANE_CONTENT_TYPE, &etag, Some(&display))
        }
        Ok(Err(error)) => query_problem(error, request_id.0).into_response(),
        Err(error) => execution_problem(error, request_id.0).into_response(),
    }
}

async fn submit_mrms_latest(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<MrmsIngestRequest>,
) -> Response {
    let request_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(_) => return ProblemDetails::internal(request_id.0).into_response(),
    };
    let store_root = state.config.server.store_root.clone();
    submit_observation_job(
        state,
        request_id.0,
        "mrms_latest_ingest",
        &request_bytes,
        move |_| ingest_mrms_latest(&store_root, &request, MAXIMUM_OBSERVATION_GRID_CELLS),
    )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NexradUploadQuery {
    site: Option<String>,
    site_latitude: Option<f64>,
    site_longitude: Option<f64>,
    site_elevation_m: Option<f64>,
    moment: Option<String>,
    mode: Option<String>,
    sweep_index: Option<u16>,
    resolution_m: Option<f64>,
    radius_km: Option<f64>,
    collection: Option<String>,
    variable: Option<String>,
}

async fn submit_nexrad_level2(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Query(query): Query<NexradUploadQuery>,
    body: Bytes,
) -> Response {
    if body.is_empty() || body.len() > state.config.limits.observation_level2_upload_bytes {
        return ProblemDetails::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "INVALID_LEVEL2_UPLOAD",
            "Invalid NEXRAD Level-II upload",
            "Upload a non-empty Archive-II volume within the configured limit.",
            request_id.0,
        )
        .into_response();
    }
    let options = match nexrad_options(query) {
        Ok(options) => options,
        Err(detail) => {
            return ProblemDetails::new(
                StatusCode::BAD_REQUEST,
                "INVALID_LEVEL2_OPTIONS",
                "Invalid NEXRAD Level-II options",
                detail,
                request_id.0,
            )
            .into_response();
        }
    };
    let mut fingerprint = serde_json::to_vec(&options).unwrap_or_default();
    fingerprint.extend_from_slice(blake3::hash(&body).as_bytes());
    let store_root = state.config.server.store_root.clone();
    submit_observation_job(
        state,
        request_id.0,
        "nexrad_level2_ingest",
        &fingerprint,
        move |_| ingest_nexrad_level2(&store_root, &body, &options, MAXIMUM_OBSERVATION_GRID_CELLS),
    )
}

async fn submit_radar_mosaic(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<RadarMosaicRequest>,
) -> Response {
    let request_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(_) => return ProblemDetails::internal(request_id.0).into_response(),
    };
    let store_root = state.config.server.store_root.clone();
    submit_observation_job(
        state,
        request_id.0,
        "radar_mosaic",
        &request_bytes,
        move |_| {
            build_and_store_radar_mosaic(
                &store_root,
                &request,
                MAXIMUM_OBSERVATION_GRID_CELLS,
                MAXIMUM_MOSAIC_INPUTS,
            )
        },
    )
}

async fn submit_simulated_radar(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<SimulatedRadarRequest>,
) -> Response {
    let request_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(_) => return ProblemDetails::internal(request_id.0).into_response(),
    };
    let store_root = state.config.server.store_root.clone();
    submit_observation_job(
        state,
        request_id.0,
        "simulated_radar",
        &request_bytes,
        move |_| {
            derive_and_store_simulated_radar(&store_root, &request, MAXIMUM_OBSERVATION_GRID_CELLS)
        },
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedFrameRequest {
    family: ObservationFamily,
    collection: String,
    product: String,
    valid_unix: i64,
    nx: usize,
    ny: usize,
    latitudes: Vec<Option<f32>>,
    longitudes: Vec<Option<f32>>,
    #[serde(default)]
    projection: Option<GridProjection>,
    planes: Vec<GeneratedPlaneRequest>,
    #[serde(default)]
    provenance_provider: String,
    #[serde(default)]
    provenance_roles: Vec<String>,
    #[serde(default)]
    provenance_products: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedPlaneRequest {
    name: String,
    units: String,
    #[serde(default)]
    selector: serde_json::Value,
    values: Vec<Option<f32>>,
}

async fn submit_generated_frame(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Json(request): Json<GeneratedFrameRequest>,
) -> Response {
    let request_bytes = match serde_json::to_vec(&request) {
        Ok(bytes) => bytes,
        Err(_) => return ProblemDetails::internal(request_id.0).into_response(),
    };
    let GeneratedFrameRequest {
        family,
        collection,
        product,
        valid_unix,
        nx,
        ny,
        latitudes,
        longitudes,
        projection,
        planes,
        provenance_provider,
        provenance_roles,
        provenance_products,
    } = request;
    let shape = match GridShape::new(nx, ny) {
        Ok(shape) => shape,
        Err(error) => {
            return observation_problem(error.into(), request_id.0).into_response();
        }
    };
    let latitudes = latitudes
        .into_iter()
        .map(|value| value.unwrap_or(f32::NAN))
        .collect();
    let longitudes = longitudes
        .into_iter()
        .map(|value| value.unwrap_or(f32::NAN))
        .collect();
    let grid = match LatLonGrid::new(shape, latitudes, longitudes) {
        Ok(grid) => grid,
        Err(error) => {
            return observation_problem(error.into(), request_id.0).into_response();
        }
    };
    let planes = planes
        .into_iter()
        .map(|plane| GridPlane {
            name: plane.name,
            units: plane.units,
            selector: plane.selector,
            values: plane
                .values
                .into_iter()
                .map(|value| value.unwrap_or(f32::NAN))
                .collect(),
        })
        .collect();
    let frame = ObservationFrame {
        family,
        collection,
        product,
        valid_unix,
        grid,
        projection,
        planes,
        provenance_provider,
        provenance_roles,
        provenance_products,
    };
    if let Err(error) = frame.validate(MAXIMUM_OBSERVATION_GRID_CELLS) {
        return observation_problem(error, request_id.0).into_response();
    }
    let store_root = state.config.server.store_root.clone();
    submit_observation_job(
        state,
        request_id.0,
        "generated_observation",
        &request_bytes,
        move |_| {
            write_observation_frame_with_limit(&store_root, &frame, MAXIMUM_OBSERVATION_GRID_CELLS)
        },
    )
}

fn submit_observation_job<F>(
    state: AppState,
    request_id: Uuid,
    kind: &'static str,
    request_bytes: &[u8],
    work: F,
) -> Response
where
    F: FnOnce(CancellationToken) -> Result<StoredFrameRef, ObservationError> + Send + 'static,
{
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"rw-server.observation-job.v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(&(request_bytes.len() as u64).to_le_bytes());
    hasher.update(request_bytes);
    let fingerprint = hasher.finalize().to_hex().to_string();
    let (job, cancellation) = match state.jobs.create(kind, fingerprint) {
        Ok(job) => job,
        Err(error) => return job_problem(error, request_id).into_response(),
    };
    let job_id = job.id;
    let task_state = state.clone();
    let job_manager = state.jobs.clone();
    tokio::spawn(async move {
        match job_manager.mark_running(job_id) {
            Ok(true) => {}
            Ok(false) => return,
            Err(error) => {
                error!(%job_id, %error, "failed to start observation job");
                return;
            }
        }
        let worker_cancellation = cancellation.clone();
        let result = task_state
            .run_heavy_job(move || {
                if worker_cancellation.is_cancelled() {
                    return Err(ObservationError::Invalid("job cancelled".into()));
                }
                work(worker_cancellation)
            })
            .await;
        match result {
            Ok(Ok(stored)) => match serde_json::to_vec_pretty(&stored) {
                Ok(bytes) => {
                    if let Err(error) = job_manager.succeed(
                        job_id,
                        "observation-result.json",
                        "application/json",
                        &bytes,
                    ) {
                        error!(%job_id, %error, "failed to publish observation job artifact");
                        let _ = job_manager.fail(job_id, "ARTIFACT_WRITE_FAILED");
                    }
                }
                Err(error) => {
                    error!(%job_id, %error, "failed to encode observation result");
                    let _ = job_manager.fail(job_id, "RESULT_ENCODING_FAILED");
                }
            },
            Ok(Err(error)) => {
                error!(%job_id, %error, "observation job failed");
                let _ = job_manager.fail(job_id, "OBSERVATION_FAILED");
            }
            Err(error) => {
                cancellation.cancel();
                error!(%job_id, %error, "observation job execution failed");
                let _ = job_manager.fail(job_id, "EXECUTION_FAILED");
            }
        }
    });
    (StatusCode::ACCEPTED, Json(job)).into_response()
}

fn nexrad_options(query: NexradUploadQuery) -> Result<NexradIngestOptions, String> {
    let moment = match query
        .moment
        .as_deref()
        .unwrap_or("reflectivity")
        .to_ascii_lowercase()
        .as_str()
    {
        "ref" | "reflectivity" => RadarMoment::Reflectivity,
        "vel" | "velocity" => RadarMoment::Velocity,
        "sw" | "spectrum_width" | "spectrum-width" => RadarMoment::SpectrumWidth,
        "zdr" => RadarMoment::DifferentialReflectivity,
        "rho" | "cc" | "correlation_coefficient" => RadarMoment::CorrelationCoefficient,
        "phi" | "phidp" => RadarMoment::DifferentialPhase,
        "kdp" => RadarMoment::SpecificDifferentialPhase,
        "hca" => RadarMoment::HydrometeorClassification,
        other => return Err(format!("unknown radar moment '{other}'")),
    };
    let mode = match query
        .mode
        .as_deref()
        .unwrap_or("lowest")
        .to_ascii_lowercase()
        .as_str()
    {
        "lowest" => RadarGridMode::Lowest,
        "composite" => RadarGridMode::Composite,
        "sweep" => RadarGridMode::Sweep {
            sweep_index: query
                .sweep_index
                .ok_or_else(|| "mode=sweep requires sweep_index".to_string())?,
        },
        other => return Err(format!("unknown radar grid mode '{other}'")),
    };
    Ok(NexradIngestOptions {
        site_id: query.site,
        site_latitude: query.site_latitude,
        site_longitude: query.site_longitude,
        site_elevation_m: query.site_elevation_m,
        moment,
        mode,
        resolution_m: query.resolution_m.unwrap_or(1_000.0),
        radius_km: query.radius_km.unwrap_or(230.0),
        collection: query.collection,
        variable: query.variable,
        source_identity: None,
    })
}

fn observation_kind_for_model(model: &str) -> Option<ObservationKind> {
    let model = model.to_ascii_lowercase();
    match model.as_str() {
        "g16" | "g17" | "g18" | "g19" | "goes16" | "goes17" | "goes18" | "goes19" | "himawari"
        | "himawari9" | "mtg" | "mtg-fci" | "obs-satellite" => Some(ObservationKind::Satellite),
        "simsat" | "obs-simsat" => Some(ObservationKind::SimulatedSatellite),
        "mrms" | "obs-mrms" => Some(ObservationKind::Mrms),
        "radar" | "nexrad" | "obs-radar" => Some(ObservationKind::Radar),
        "eumetnet-opera" | "imgw-polrad" | "obs-radar-mosaic" => Some(ObservationKind::RadarMosaic),
        "obs-sim-radar" => Some(ObservationKind::SimulatedRadar),
        "obs-generated" => Some(ObservationKind::Generated),
        _ => None,
    }
}

fn binary_response(
    bytes: Vec<u8>,
    content_type: &'static str,
    etag: &str,
    display: Option<&ObservationDisplayHint>,
) -> Response {
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    if let Some(display) = display {
        insert_display_headers(&mut response, display);
    }
    response
}

fn insert_display_headers(response: &mut Response, display: &ObservationDisplayHint) {
    response.headers_mut().insert(
        "x-rw-observation-semantics",
        HeaderValue::from_static(display.semantics.slug()),
    );
    response.headers_mut().insert(
        "x-rw-observation-interpolation",
        HeaderValue::from_static(display.interpolation.slug()),
    );
    response.headers_mut().insert(
        "x-rw-nodata",
        HeaderValue::from_static(if display.transparent_non_finite {
            "non-finite-transparent"
        } else {
            "unspecified"
        }),
    );
    if let Ok(value) = HeaderValue::from_str(&display.palette) {
        response
            .headers_mut()
            .insert("x-rw-observation-palette", value);
    }
    if let Some([minimum, maximum]) = display.preferred_range
        && let Ok(value) = HeaderValue::from_str(&format!("{minimum},{maximum}"))
    {
        response.headers_mut().insert("x-rw-preferred-range", value);
    }
    if let Some(threshold) = display.discontinuity_threshold
        && let Ok(value) = HeaderValue::from_str(&threshold.to_string())
    {
        response
            .headers_mut()
            .insert("x-rw-discontinuity-threshold", value);
    }
}

fn observation_problem(error: ObservationError, request_id: Uuid) -> ProblemDetails {
    let (status, code, title) = match &error {
        ObservationError::Invalid(_) | ObservationError::Core(_) => (
            StatusCode::BAD_REQUEST,
            "INVALID_OBSERVATION_REQUEST",
            "Invalid observation request",
        ),
        ObservationError::Query(QueryError::UnknownModel(_))
        | ObservationError::Query(QueryError::UnknownRun { .. })
        | ObservationError::Query(QueryError::UnknownVariable(_))
        | ObservationError::Query(QueryError::UnknownStorageSlot(_))
        | ObservationError::Query(QueryError::UnknownPressureLevel { .. }) => (
            StatusCode::NOT_FOUND,
            "OBSERVATION_NOT_FOUND",
            "Observation source not found",
        ),
        ObservationError::Query(QueryError::LimitExceeded { .. }) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "OBSERVATION_LIMIT_EXCEEDED",
            "Observation limit exceeded",
        ),
        ObservationError::Mrms(_) | ObservationError::Nexrad(_) => (
            StatusCode::BAD_GATEWAY,
            "OBSERVATION_UPSTREAM_FAILED",
            "Observation upstream failed",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OBSERVATION_FAILED",
            "Observation processing failed",
        ),
    };
    ProblemDetails::new(status, code, title, error.to_string(), request_id)
}

fn query_problem(error: QueryError, request_id: Uuid) -> ProblemDetails {
    observation_problem(ObservationError::Query(error), request_id)
}

fn execution_problem(error: ExecutionError, request_id: Uuid) -> ProblemDetails {
    let (status, code, title) = match &error {
        ExecutionError::AdmissionTimeout | ExecutionError::ShuttingDown => (
            StatusCode::SERVICE_UNAVAILABLE,
            "OBSERVATION_SERVICE_BUSY",
            "Observation service is busy",
        ),
        ExecutionError::ExecutionTimeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "OBSERVATION_TIMEOUT",
            "Observation processing timed out",
        ),
        ExecutionError::Join(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "OBSERVATION_WORKER_FAILED",
            "Observation worker failed",
        ),
    };
    ProblemDetails::new(status, code, title, error.to_string(), request_id)
}

fn job_problem(error: JobError, request_id: Uuid) -> ProblemDetails {
    let (status, code, title) = match &error {
        JobError::Capacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            "JOB_CAPACITY",
            "Observation job capacity is full",
        ),
        JobError::Invalid(_) => (
            StatusCode::BAD_REQUEST,
            "INVALID_JOB",
            "Observation job is invalid",
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "JOB_FAILED",
            "Observation job could not be created",
        ),
    };
    ProblemDetails::new(status, code, title, error.to_string(), request_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observation_routes_accept_filename_suffixes_under_axum_08() {
        let _ = read_router();
    }

    #[test]
    fn binary_planes_expose_the_same_semantics_as_the_frames_catalog() {
        let display = rw_observations::observation_display_hint(
            ObservationFamily::Radar,
            "radar_velocity",
            "m/s",
        );
        let response = binary_response(
            vec![0, 1, 2, 3],
            OBSERVATION_PLANE_CONTENT_TYPE,
            "\"test\"",
            Some(&display),
        );
        assert_eq!(
            response.headers()["x-rw-observation-semantics"],
            "radial_velocity"
        );
        assert_eq!(response.headers()["x-rw-observation-palette"], "velocity");
        assert_eq!(
            response.headers()["x-rw-observation-interpolation"],
            "velocity_fold_aware"
        );
        assert_eq!(response.headers()["x-rw-nodata"], "non-finite-transparent");
    }

    #[test]
    fn capabilities_publish_effective_upload_limits_and_no_mosaic_product_cap() {
        let limits = LimitsConfig {
            observation_level2_upload_bytes: 511 * 1024 * 1024,
            observation_generated_upload_bytes: 383 * 1024 * 1024,
            ..LimitsConfig::default()
        };
        let response = capabilities_response(&limits);
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(
            value["maximum_level2_upload_bytes"],
            limits.observation_level2_upload_bytes
        );
        assert_eq!(
            value["maximum_generated_upload_bytes"],
            limits.observation_generated_upload_bytes
        );
        assert_eq!(value["maximum_mosaic_inputs"], usize::MAX);
    }

    fn observation_run(index: usize) -> ObservationRunSummary {
        ObservationRunSummary {
            kind: ObservationKind::Mrms,
            run: RunDescriptor {
                model: "obs-mrms".to_owned(),
                run: format!("run-{index:04}"),
                schema: "rw-store.run.v2".to_owned(),
                snapshot_id: format!("snapshot-{index:04}"),
                grid_hash: "grid".to_owned(),
                nx: 1,
                ny: 1,
                exact_time_axis: true,
                origin_unix: Some(index as i64),
                sample_count: 1,
                first_valid_unix: Some(index as i64),
                last_valid_unix: Some(index as i64),
                source_provenance: Vec::new(),
                provider_attributions: Vec::new(),
            },
            variable_count: 1,
        }
    }

    #[test]
    fn observation_catalog_has_no_default_run_ceiling_and_explicit_limit_is_honest() {
        assert!(validate_observation_catalog_limit(None).is_ok());
        assert!(validate_observation_catalog_limit(Some(1_075)).is_ok());
        assert!(validate_observation_catalog_limit(Some(0)).is_err());

        let all = finalize_observation_catalog((0..1_075).map(observation_run).collect(), None);
        assert_eq!(all.runs.len(), 1_075);
        assert!(!all.truncated);
        let wire = encode_observation_catalog(&all).unwrap();
        let decoded: serde_json::Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(decoded["runs"].as_array().unwrap().len(), 1_075);
        assert_eq!(decoded["truncated"], false);

        let limited =
            finalize_observation_catalog((0..1_075).map(observation_run).collect(), Some(73));
        assert_eq!(limited.runs.len(), 73);
        assert!(limited.truncated);

        let exact = finalize_observation_catalog((0..73).map(observation_run).collect(), Some(73));
        assert_eq!(exact.runs.len(), 73);
        assert!(!exact.truncated);
    }
}

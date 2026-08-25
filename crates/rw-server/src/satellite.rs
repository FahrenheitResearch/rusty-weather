//! Native GOES satellite catalog and MapLibre/Mapbox tile delivery.

use std::future::Future;
use std::io;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Extension, OriginalUri, Path, Query, State};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use bytes::Bytes;
use rw_sat::{
    DEFAULT_TILE_SIZE, GoesAbiProduct, MAXIMUM_TILE_ZOOM, SatelliteEnhancement,
    SatelliteProductDescriptor, SatelliteSectorDescriptor, list_native_frames, product_catalog,
    render_native_xyz_tile, resolve_native_frame_with_revision, sector_catalog,
};
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::problem::ProblemDetails;
use crate::routes::RequestId;
use crate::satellite_tile_cache::SatelliteTileDiskCache;
use crate::state::{CachedSatelliteTile, SatelliteTileCacheKey};
use crate::{AppState, ExecutionError};

/// Changes whenever native ABI pixels, enhancement recipes, or PNG encoding
/// can change. It is part of the tile URL, so an old immutable CDN object can
/// never masquerade as output from a new renderer.
pub const SATELLITE_TILE_RECIPE_VERSION: &str = "rw-sat-native-v2";

const LATEST_CACHE_CONTROL: &str = "no-store";
const LEGACY_EXACT_CACHE_CONTROL: &str = "public, max-age=0, must-revalidate";
const VERSIONED_EXACT_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";
const LATEST_TILEJSON_CACHE_CONTROL: &str = "no-cache, max-age=0, must-revalidate";
const EXACT_TILEJSON_CACHE_CONTROL: &str = "public, max-age=300, must-revalidate";

#[derive(Debug)]
enum SatelliteTileFillError {
    Render(String),
    Execution(String),
    SourceRevisionUnavailable,
}

#[derive(Debug)]
enum SatelliteRenderAttemptError {
    Render(String),
    SourceRevisionChanged,
}

#[derive(Clone, Copy, Debug)]
struct SatelliteXyz {
    zoom: u8,
    x: u32,
    y: u32,
}

pub(crate) fn read_router() -> Router<AppState> {
    Router::new()
        .route("/v1/satellite/catalog", get(catalog))
        .route("/v1/satellite/prewarm/status", get(prewarm_status))
        .route(
            "/v1/satellite/{platform}/{sector}/{product}/frames",
            get(frames),
        )
        .route(
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tilejson.json",
            get(tilejson),
        )
        .route(
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{z}/{x}/{y}",
            get(legacy_tile),
        )
        .route(
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{z}/{x}/{y}",
            get(versioned_tile),
        )
        .route(
            "/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{recipe}/{source_revision}/{z}/{x}/{y}",
            get(revisioned_tile),
        )
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogQuery {
    #[serde(default)]
    include_raw_channels: bool,
}

#[derive(Debug, Serialize)]
struct PlatformDescriptor {
    id: &'static str,
    title: &'static str,
    role: &'static str,
}

#[derive(Debug, Serialize)]
struct EnhancementStop {
    value: f32,
    rgb: [u8; 3],
}

#[derive(Debug, Serialize)]
struct EnhancementDescriptor {
    id: &'static str,
    title: &'static str,
    value_units: &'static str,
    stops: Vec<EnhancementStop>,
}

#[derive(Debug, Serialize)]
struct SatelliteCatalogResponse {
    schema: &'static str,
    platforms: Vec<PlatformDescriptor>,
    sectors: Vec<SatelliteSectorDescriptor>,
    products: Vec<SatelliteProductDescriptor>,
    enhancements: Vec<EnhancementDescriptor>,
    native_source_archive: bool,
    full_disk_native_window_reads: bool,
    latest_frame_alias: &'static str,
    maximum_tile_zoom: u8,
    tile_size: u32,
    renderer_recipe: &'static str,
    geocolor_note: &'static str,
}

async fn catalog(Query(query): Query<CatalogQuery>) -> Json<SatelliteCatalogResponse> {
    Json(SatelliteCatalogResponse {
        schema: "rw-server.satellite-catalog.v3",
        platforms: vec![
            PlatformDescriptor {
                id: "g19",
                title: "GOES-19 East",
                role: "operational_east",
            },
            PlatformDescriptor {
                id: "g18",
                title: "GOES-18 West",
                role: "operational_west",
            },
            PlatformDescriptor {
                id: "g16",
                title: "GOES-16 archive",
                role: "archive",
            },
        ],
        sectors: sector_catalog(),
        products: product_catalog(query.include_raw_channels),
        enhancements: SatelliteEnhancement::ALL
            .into_iter()
            .map(|enhancement| EnhancementDescriptor {
                id: enhancement.slug(),
                title: enhancement.title(),
                value_units: enhancement.value_units(),
                stops: enhancement
                    .stops()
                    .iter()
                    .map(|(value, rgb)| EnhancementStop {
                        value: *value,
                        rgb: *rgb,
                    })
                    .collect(),
            })
            .collect(),
        native_source_archive: true,
        full_disk_native_window_reads: true,
        latest_frame_alias: "latest",
        maximum_tile_zoom: MAXIMUM_TILE_ZOOM,
        tile_size: DEFAULT_TILE_SIZE,
        renderer_recipe: SATELLITE_TILE_RECIPE_VERSION,
        geocolor_note: "Daytime C01/C03 are variance-sharpened with native C02 detail before pseudo-green construction and blended with ABI C13 infrared at night; atmospheric/Rayleigh correction and city lights are not yet applied.",
    })
}

#[derive(Debug, Deserialize)]
struct ProductPath {
    platform: String,
    sector: String,
    product: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameQuery {
    /// Optional explicit page size. Omission means every retained complete
    /// frame; callers that need pagination choose the page size themselves.
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize)]
struct FrameDescriptor {
    id: String,
    source_revision: String,
    scan_start_unix: i64,
    scan_end_unix: i64,
    channels: Vec<u8>,
}

#[derive(Debug, Serialize)]
struct FramesResponse {
    schema: &'static str,
    platform: String,
    sector: String,
    product: SatelliteProductDescriptor,
    cadence_seconds: u64,
    frames: Vec<FrameDescriptor>,
}

async fn frames(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<ProductPath>,
    Query(query): Query<FrameQuery>,
) -> Response {
    if query.limit == Some(0) {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_FRAME_LIMIT",
            "Satellite frame limit must be greater than zero.",
            request_id.0,
        );
    }
    let Some(product) = GoesAbiProduct::parse(&path.product) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let Some(sector) = rw_sat::s3::Sector::parse(&path.sector) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let store_root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector_slug = sector.slug().to_string();
    let lookup_platform = platform.clone();
    let lookup_sector = sector_slug.clone();
    let requested = query.limit.unwrap_or(usize::MAX);
    match state
        .run_heavy_sync(move || {
            list_native_frames(
                &store_root,
                &lookup_platform,
                &lookup_sector,
                product,
                requested,
            )?
            .into_iter()
            .map(|listed| {
                resolve_native_frame_with_revision(
                    &store_root,
                    &lookup_platform,
                    &lookup_sector,
                    product,
                    &listed.frame_id,
                )
            })
            .collect::<io::Result<Vec<_>>>()
        })
        .await
    {
        Ok(Ok(found)) => Json(FramesResponse {
            schema: "rw-server.satellite-frames.v3",
            platform: path.platform.to_ascii_lowercase(),
            sector: sector.slug().to_string(),
            product: product.descriptor(),
            cadence_seconds: sector.cadence_secs(),
            frames: found
                .into_iter()
                .map(|resolved| FrameDescriptor {
                    id: resolved.frame.frame_id,
                    source_revision: resolved.source_revision,
                    scan_start_unix: resolved.frame.scan_start_unix,
                    scan_end_unix: resolved.frame.scan_end_unix,
                    channels: resolved.frame.channels.into_keys().collect(),
                })
                .collect(),
        })
        .into_response(),
        Ok(Err(error)) => satellite_io_problem(error, request_id.0),
        Err(error) => execution_problem(error, request_id.0),
    }
}

#[derive(Debug, Deserialize)]
struct FramePath {
    platform: String,
    sector: String,
    product: String,
    frame: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TileJsonResponse {
    tilejson: &'static str,
    name: String,
    description: String,
    scheme: &'static str,
    tiles: Vec<String>,
    minzoom: u8,
    maxzoom: u8,
    bounds: [f64; 4],
    attribution: &'static str,
    tile_size: u32,
    renderer_recipe: &'static str,
    frame: String,
    source_revision: String,
}

async fn tilejson(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
    Path(path): Path<FramePath>,
) -> Response {
    let Some(product) = GoesAbiProduct::parse(&path.product) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let Some(sector) = rw_sat::s3::Sector::parse(&path.sector) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let platform = path.platform.to_ascii_lowercase();
    if !valid_url_component(&platform) || !valid_frame_or_latest(&path.frame) {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_TILEJSON",
            "The satellite TileJSON request is invalid.",
            request_id.0,
        );
    }
    let requested_latest = path.frame.eq_ignore_ascii_case("latest");
    let store_root = state.config.server.store_root.clone();
    let lookup_platform = platform.clone();
    let sector_slug = sector.slug().to_string();
    let requested_frame = path.frame.clone();
    let resolved = match state
        .run_heavy_sync(move || {
            resolve_native_frame_with_revision(
                &store_root,
                &lookup_platform,
                &sector_slug,
                product,
                &requested_frame,
            )
        })
        .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(error)) => return satellite_io_problem(error, request_id.0),
        Err(error) => return execution_problem(error, request_id.0),
    };
    let frame = resolved.frame.frame_id;
    let source_revision = resolved.source_revision;
    let base_url = match public_base_url(
        state.config.server.public_base_url.as_deref(),
        &uri,
        &headers,
    ) {
        Ok(base_url) => base_url,
        Err(()) => {
            return problem(
                StatusCode::BAD_REQUEST,
                "INVALID_PUBLIC_REQUEST_ORIGIN",
                "A canonical request Host or configured public base URL is required.",
                request_id.0,
            );
        }
    };
    let product_slug = product.slug();
    let response = TileJsonResponse {
        tilejson: "3.0.0",
        name: format!("{} · {} · {}", platform, sector.slug(), product.title()),
        description: product.description().to_string(),
        scheme: "xyz",
        tiles: vec![versioned_tile_url(
            &base_url,
            &platform,
            sector.slug(),
            &product_slug,
            &frame,
            &source_revision,
        )],
        minzoom: 0,
        maxzoom: MAXIMUM_TILE_ZOOM,
        bounds: [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
        attribution: "NOAA/NESDIS; rendered by Rusty Weather",
        tile_size: DEFAULT_TILE_SIZE,
        renderer_recipe: SATELLITE_TILE_RECIPE_VERSION,
        frame: frame.clone(),
        source_revision: source_revision.clone(),
    };
    with_source_revision(
        json_cache_response(
            &response,
            if requested_latest {
                LATEST_TILEJSON_CACHE_CONTROL
            } else {
                EXACT_TILEJSON_CACHE_CONTROL
            },
            &headers,
            Some((&frame, None)),
            request_id.0,
        ),
        &source_revision,
    )
}

#[derive(Debug, Deserialize)]
struct TilePath {
    platform: String,
    sector: String,
    product: String,
    frame: String,
    z: u8,
    x: u32,
    y: String,
}

#[derive(Debug, Deserialize)]
struct VersionedTilePath {
    platform: String,
    sector: String,
    product: String,
    frame: String,
    recipe: String,
    z: u8,
    x: u32,
    y: String,
}

#[derive(Debug, Deserialize)]
struct RevisionedTilePath {
    platform: String,
    sector: String,
    product: String,
    frame: String,
    recipe: String,
    source_revision: String,
    z: u8,
    x: u32,
    y: String,
}

async fn legacy_tile(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    Path(path): Path<TilePath>,
) -> Response {
    render_tile(state, request_id, headers, path, None).await
}

async fn versioned_tile(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    Path(path): Path<VersionedTilePath>,
) -> Response {
    if path.recipe != SATELLITE_TILE_RECIPE_VERSION {
        return problem(
            StatusCode::NOT_FOUND,
            "UNKNOWN_SATELLITE_RENDERER_RECIPE",
            "The requested satellite renderer recipe is not available.",
            request_id.0,
        );
    }
    render_tile(
        state,
        request_id,
        headers,
        TilePath {
            platform: path.platform,
            sector: path.sector,
            product: path.product,
            frame: path.frame,
            z: path.z,
            x: path.x,
            y: path.y,
        },
        None,
    )
    .await
}

async fn revisioned_tile(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    Path(path): Path<RevisionedTilePath>,
) -> Response {
    if path.recipe != SATELLITE_TILE_RECIPE_VERSION || !valid_source_revision(&path.source_revision)
    {
        return problem(
            StatusCode::NOT_FOUND,
            "UNKNOWN_SATELLITE_RENDERER_REVISION",
            "The requested satellite renderer or source revision is not available.",
            request_id.0,
        );
    }
    let source_revision = path.source_revision;
    render_tile(
        state,
        request_id,
        headers,
        TilePath {
            platform: path.platform,
            sector: path.sector,
            product: path.product,
            frame: path.frame,
            z: path.z,
            x: path.x,
            y: path.y,
        },
        Some(source_revision),
    )
    .await
}

async fn render_tile(
    state: AppState,
    request_id: RequestId,
    headers: HeaderMap,
    path: TilePath,
    requested_source_revision: Option<String>,
) -> Response {
    let Some(product) = GoesAbiProduct::parse(&path.product) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let Some(sector) = rw_sat::s3::Sector::parse(&path.sector) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let Some(tile_y) = path
        .y
        .strip_suffix(".png")
        .and_then(|value| value.parse::<u32>().ok())
    else {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_TILE",
            "The satellite tile request is invalid.",
            request_id.0,
        );
    };
    let requested_latest = path.frame.eq_ignore_ascii_case("latest");
    if !valid_url_component(&path.platform) || !valid_frame_or_latest(&path.frame) {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_TILE",
            "The satellite tile request is invalid.",
            request_id.0,
        );
    }
    let store_root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector_slug = sector.slug().to_string();
    let xyz = SatelliteXyz {
        zoom: path.z,
        x: path.x,
        y: tile_y,
    };
    // A revisioned exact URL contains the complete immutable cache identity.
    // Consult memory and durable storage before touching the scientific
    // archive or a heavy-worker permit. This keeps already-rendered website
    // tiles available after raw-source retention has legitimately expired.
    if let Some(source_revision) = requested_source_revision.as_deref()
        && !requested_latest
    {
        let result = load_or_render_exact_tile(
            state.clone(),
            platform.clone(),
            sector_slug.clone(),
            product,
            path.frame.clone(),
            source_revision.to_owned(),
            xyz,
        )
        .await;
        return tile_result_response(
            result,
            VERSIONED_EXACT_CACHE_CONTROL,
            &headers,
            request_id.0,
        );
    }
    let lookup_root = store_root.clone();
    let lookup_platform = platform.clone();
    let lookup_sector = sector_slug.clone();
    let requested_frame = path.frame.clone();
    let resolved = match state
        .run_heavy_sync(move || {
            resolve_native_frame_with_revision(
                &lookup_root,
                &lookup_platform,
                &lookup_sector,
                product,
                &requested_frame,
            )
        })
        .await
    {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(error)) => return satellite_io_problem(error, request_id.0),
        Err(error) => return execution_problem(error, request_id.0),
    };
    let frame = resolved.frame.frame_id;
    let current_source_revision = resolved.source_revision;
    let source_revision = requested_source_revision
        .clone()
        .unwrap_or_else(|| current_source_revision.clone());
    if source_revision != current_source_revision {
        return source_revision_unavailable_problem(request_id.0);
    }
    let result = load_or_render_exact_tile(
        state,
        platform,
        sector_slug,
        product,
        frame,
        source_revision,
        xyz,
    )
    .await;
    tile_result_response(
        result,
        tile_cache_control(requested_latest, requested_source_revision.is_some()),
        &headers,
        request_id.0,
    )
}

async fn prewarm_status(
    State(state): State<AppState>,
) -> Json<crate::satellite_prewarm::SatellitePrewarmStatus> {
    Json(state.satellite_prewarm_status.snapshot())
}

async fn load_or_render_exact_tile(
    state: AppState,
    platform: String,
    sector: String,
    product: GoesAbiProduct,
    frame: String,
    source_revision: String,
    xyz: SatelliteXyz,
) -> Result<Arc<CachedSatelliteTile>, Arc<SatelliteTileFillError>> {
    let cache_key = satellite_tile_cache_key(
        SATELLITE_TILE_RECIPE_VERSION,
        &source_revision,
        &platform,
        &sector,
        &product.slug(),
        &frame,
        xyz,
    );
    let tile_cache = state.satellite_tile_cache.clone();
    let tile_disk_cache = state.satellite_tile_disk_cache.clone();
    let render_state = state.clone();
    let store_root = state.config.server.store_root.clone();
    let expected_frame = frame.clone();
    let returned_frame = frame.clone();
    let expected_source_revision = source_revision.clone();
    let render_source_revision = source_revision;
    cached_satellite_tile(&tile_cache, &tile_disk_cache, cache_key, async move {
        match render_state
            .run_heavy_sync(move || {
                let resolved = resolve_native_frame_with_revision(
                    &store_root,
                    &platform,
                    &sector,
                    product,
                    &expected_frame,
                )
                .map_err(|error| SatelliteRenderAttemptError::Render(error.to_string()))?;
                if resolved.source_revision != expected_source_revision {
                    return Err(SatelliteRenderAttemptError::SourceRevisionChanged);
                }
                let tile = render_native_xyz_tile(
                    &store_root,
                    &platform,
                    &sector,
                    product,
                    &expected_frame,
                    xyz.zoom,
                    xyz.x,
                    xyz.y,
                    DEFAULT_TILE_SIZE,
                )
                .map_err(|error| SatelliteRenderAttemptError::Render(error.to_string()))?;
                let confirmed = resolve_native_frame_with_revision(
                    &store_root,
                    &platform,
                    &sector,
                    product,
                    &expected_frame,
                )
                .map_err(|error| SatelliteRenderAttemptError::Render(error.to_string()))?;
                if confirmed.source_revision != expected_source_revision {
                    return Err(SatelliteRenderAttemptError::SourceRevisionChanged);
                }
                Ok(tile)
            })
            .await
        {
            Ok(Ok(tile)) => {
                if tile.frame_id != returned_frame {
                    return Err(SatelliteTileFillError::Render(format!(
                        "satellite renderer returned frame {} for exact frame {returned_frame}",
                        tile.frame_id
                    )));
                }
                let png = Bytes::from(tile.png);
                let etag = format!("\"{}\"", blake3::hash(&png).to_hex());
                Ok(Arc::new(CachedSatelliteTile {
                    png,
                    etag,
                    frame_id: tile.frame_id,
                    source_revision: render_source_revision,
                    valid_unix: tile.valid_unix,
                }))
            }
            Ok(Err(SatelliteRenderAttemptError::Render(error))) => {
                Err(SatelliteTileFillError::Render(error))
            }
            Ok(Err(SatelliteRenderAttemptError::SourceRevisionChanged)) => {
                Err(SatelliteTileFillError::SourceRevisionUnavailable)
            }
            Err(error) => Err(SatelliteTileFillError::Execution(error.to_string())),
        }
    })
    .await
}

/// Populate or validate one immutable tile through the same exact cache and
/// renderer path used by HTTP. Durable cache hits do not consume a heavy
/// worker or require the retained native source to still exist.
pub(crate) struct SatellitePrewarmTile {
    pub platform: String,
    pub sector: String,
    pub product: GoesAbiProduct,
    pub frame: String,
    pub source_revision: String,
    pub z: u8,
    pub x: u32,
    pub y: u32,
}

pub(crate) async fn prewarm_revisioned_tile(
    state: AppState,
    request: SatellitePrewarmTile,
) -> Result<(), String> {
    load_or_render_exact_tile(
        state,
        request.platform,
        request.sector,
        request.product,
        request.frame,
        request.source_revision,
        SatelliteXyz {
            zoom: request.z,
            x: request.x,
            y: request.y,
        },
    )
    .await
    .map(|_| ())
    .map_err(|error| match error.as_ref() {
        SatelliteTileFillError::Render(detail) => detail.clone(),
        SatelliteTileFillError::Execution(detail) => detail.clone(),
        SatelliteTileFillError::SourceRevisionUnavailable => {
            "satellite source revision changed or is unavailable".to_owned()
        }
    })
}

fn tile_result_response(
    result: Result<Arc<CachedSatelliteTile>, Arc<SatelliteTileFillError>>,
    cache_control: &'static str,
    headers: &HeaderMap,
    request_id: uuid::Uuid,
) -> Response {
    match result {
        Ok(tile) => tile_response(tile, cache_control, headers),
        Err(error) => match error.as_ref() {
            SatelliteTileFillError::Render(error) => {
                satellite_render_problem(error.clone(), request_id)
            }
            SatelliteTileFillError::Execution(error) => cached_execution_problem(error, request_id),
            SatelliteTileFillError::SourceRevisionUnavailable => {
                source_revision_unavailable_problem(request_id)
            }
        },
    }
}

fn tile_cache_control(requested_latest: bool, has_source_revision: bool) -> &'static str {
    if requested_latest {
        LATEST_CACHE_CONTROL
    } else if has_source_revision {
        VERSIONED_EXACT_CACHE_CONTROL
    } else {
        LEGACY_EXACT_CACHE_CONTROL
    }
}

fn satellite_tile_cache_key(
    recipe: &str,
    source_revision: &str,
    platform: &str,
    sector: &str,
    product: &str,
    frame: &str,
    tile: SatelliteXyz,
) -> SatelliteTileCacheKey {
    SatelliteTileCacheKey {
        recipe: recipe.to_string(),
        source_revision: source_revision.to_string(),
        platform: platform.to_string(),
        sector: sector.to_string(),
        product: product.to_string(),
        frame: frame.to_string(),
        zoom: tile.zoom,
        x: tile.x,
        y: tile.y,
        tile_size: DEFAULT_TILE_SIZE,
    }
}

async fn cached_satellite_tile<F>(
    cache: &moka::future::Cache<SatelliteTileCacheKey, Arc<CachedSatelliteTile>>,
    disk_cache: &SatelliteTileDiskCache,
    key: SatelliteTileCacheKey,
    render: F,
) -> Result<Arc<CachedSatelliteTile>, Arc<SatelliteTileFillError>>
where
    F: Future<Output = Result<Arc<CachedSatelliteTile>, SatelliteTileFillError>> + Send + 'static,
{
    let load_cache = disk_cache.clone();
    let load_key = key.clone();
    let store_cache = disk_cache.clone();
    let store_key = key.clone();
    cache
        .try_get_with(key, async move {
            match tokio::task::spawn_blocking(move || load_cache.load(&load_key)).await {
                Ok(Ok(Some(tile))) => return Ok(tile),
                Ok(Ok(None)) => {}
                Ok(Err(error)) => {
                    warn!(%error, "durable satellite tile cache read failed; rendering exact tile");
                }
                Err(error) => {
                    warn!(%error, "durable satellite tile cache worker failed; rendering exact tile");
                }
            }

            let tile = render.await?;
            let stored_tile = tile.clone();
            match tokio::task::spawn_blocking(move || {
                store_cache.store(&store_key, stored_tile.as_ref())
            })
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    warn!(%error, "durable satellite tile cache write failed; serving rendered tile");
                }
                Err(error) => {
                    warn!(%error, "durable satellite tile cache worker failed; serving rendered tile");
                }
            }
            Ok(tile)
        })
        .await
}

fn valid_url_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_source_revision(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn versioned_tile_url(
    base_url: &str,
    platform: &str,
    sector: &str,
    product: &str,
    frame: &str,
    source_revision: &str,
) -> String {
    format!(
        "{base_url}/v1/satellite/{platform}/{sector}/{product}/{frame}/tiles/{SATELLITE_TILE_RECIPE_VERSION}/{source_revision}/{{z}}/{{x}}/{{y}}.png"
    )
}

fn valid_frame_or_latest(value: &str) -> bool {
    if value.eq_ignore_ascii_case("latest") {
        return true;
    }
    let bytes = value.as_bytes();
    bytes.len() == 13
        && bytes[8] == b'T'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..].iter().all(u8::is_ascii_digit)
}

fn public_base_url(configured: Option<&str>, uri: &Uri, headers: &HeaderMap) -> Result<String, ()> {
    if let Some(configured) = configured {
        let parsed = configured.parse::<Uri>().map_err(|_| ())?;
        let authority = parsed.authority().ok_or(())?;
        if parsed.scheme_str() != Some("https")
            || !safe_authority(authority)
            || parsed.query().is_some()
            || configured.ends_with('/')
            || configured.contains(['\r', '\n', '\\', '#'])
            || parsed
                .path()
                .split('/')
                .any(|part| matches!(part, "." | ".."))
        {
            return Err(());
        }
        return Ok(configured.to_string());
    }

    if let (Some(scheme), Some(authority)) = (uri.scheme_str(), uri.authority()) {
        if !matches!(scheme, "http" | "https") || !safe_authority(authority) {
            return Err(());
        }
        if let Some(host) = headers.get(header::HOST) {
            let host = host
                .to_str()
                .map_err(|_| ())?
                .parse::<Authority>()
                .map_err(|_| ())?;
            if host != *authority {
                return Err(());
            }
        }
        return Ok(format!("{scheme}://{authority}"));
    }

    let authority = headers
        .get(header::HOST)
        .ok_or(())?
        .to_str()
        .map_err(|_| ())?
        .parse::<Authority>()
        .map_err(|_| ())?;
    if !safe_authority(&authority) {
        return Err(());
    }
    // rw-server itself is plain HTTP. TLS-terminating proxies must configure
    // `server.public_base_url`; forwarded headers are intentionally untrusted.
    Ok(format!("http://{authority}"))
}

fn safe_authority(authority: &Authority) -> bool {
    let host = authority.host();
    !authority.as_str().contains('@')
        && !host.is_empty()
        && host.len() <= 253
        && host.is_ascii()
        && !host.ends_with('.')
        && !authority
            .as_str()
            .bytes()
            .any(|byte| byte.is_ascii_whitespace())
}

fn json_cache_response<T: Serialize + ?Sized>(
    value: &T,
    cache_control: &'static str,
    request_headers: &HeaderMap,
    frame: Option<(&str, Option<i64>)>,
    request_id: uuid::Uuid,
) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => cacheable_response(
            body,
            "application/json",
            cache_control,
            request_headers,
            frame,
        ),
        Err(error) => {
            error!(%request_id, %error, "satellite response serialization failed");
            ProblemDetails::internal(request_id).into_response()
        }
    }
}

fn tile_response(
    tile: Arc<CachedSatelliteTile>,
    cache_control: &'static str,
    request_headers: &HeaderMap,
) -> Response {
    let mut response = cacheable_bytes_response(
        tile.png.clone(),
        "image/png",
        cache_control,
        request_headers,
        Some((&tile.frame_id, Some(tile.valid_unix))),
        &tile.etag,
    );
    insert_source_revision(&mut response, &tile.source_revision);
    response
}

fn with_source_revision(mut response: Response, source_revision: &str) -> Response {
    insert_source_revision(&mut response, source_revision);
    response
}

fn insert_source_revision(response: &mut Response, source_revision: &str) {
    if let Ok(value) = HeaderValue::from_str(source_revision) {
        response
            .headers_mut()
            .insert("x-rw-satellite-source-revision", value);
    }
}

fn cacheable_response(
    body: Vec<u8>,
    content_type: &'static str,
    cache_control: &'static str,
    request_headers: &HeaderMap,
    frame: Option<(&str, Option<i64>)>,
) -> Response {
    let etag = format!("\"{}\"", blake3::hash(&body).to_hex());
    cacheable_bytes_response(
        Bytes::from(body),
        content_type,
        cache_control,
        request_headers,
        frame,
        &etag,
    )
}

fn cacheable_bytes_response(
    body: Bytes,
    content_type: &'static str,
    cache_control: &'static str,
    request_headers: &HeaderMap,
    frame: Option<(&str, Option<i64>)>,
    etag: &str,
) -> Response {
    let not_modified = if_none_match_matches(request_headers, etag);
    let mut response = Response::new(if not_modified {
        Body::empty()
    } else {
        Body::from(body)
    });
    if not_modified {
        *response.status_mut() = StatusCode::NOT_MODIFIED;
    } else {
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
        .headers_mut()
        .insert(header::VARY, HeaderValue::from_static("host"));
    if let Ok(value) = HeaderValue::from_str(etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    response.headers_mut().insert(
        "x-rw-satellite-recipe",
        HeaderValue::from_static(SATELLITE_TILE_RECIPE_VERSION),
    );
    if let Some((frame_id, valid_unix)) = frame {
        if let Ok(value) = HeaderValue::from_str(frame_id) {
            response.headers_mut().insert("x-rw-satellite-frame", value);
        }
        if let Some(valid_unix) = valid_unix
            && let Ok(value) = HeaderValue::from_str(&valid_unix.to_string())
        {
            response.headers_mut().insert("x-rw-valid-unix", value);
        }
    }
    response
}

fn if_none_match_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

fn satellite_io_problem(error: io::Error, request_id: uuid::Uuid) -> Response {
    match error.kind() {
        io::ErrorKind::NotFound => ProblemDetails::not_found(request_id).into_response(),
        io::ErrorKind::InvalidInput => problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_REQUEST",
            "The satellite request is invalid.",
            request_id,
        ),
        _ => {
            error!(%request_id, %error, "satellite archive read failed");
            ProblemDetails::internal(request_id).into_response()
        }
    }
}

fn satellite_render_problem(error: String, request_id: uuid::Uuid) -> Response {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("not found")
        || normalized.contains("no complete satellite frame")
        || normalized.contains("is incomplete")
        || normalized.contains("has no abi")
    {
        return ProblemDetails::not_found(request_id).into_response();
    }
    if normalized.contains("invalid")
        || normalized.contains("outside")
        || normalized.contains("exceeds")
        || normalized.contains("must be")
    {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_TILE",
            "The satellite tile request is invalid.",
            request_id,
        );
    }
    error!(%request_id, %error, "satellite tile render failed");
    ProblemDetails::internal(request_id).into_response()
}

fn execution_problem(error: ExecutionError, request_id: uuid::Uuid) -> Response {
    error!(%request_id, %error, "satellite worker execution failed");
    ProblemDetails::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "SATELLITE_WORKER_UNAVAILABLE",
        "Satellite rendering is temporarily unavailable",
        "Retry the request or use a narrower/fixed frame request.",
        request_id,
    )
    .into_response()
}

fn cached_execution_problem(error: &str, request_id: uuid::Uuid) -> Response {
    error!(%request_id, %error, "satellite worker execution failed");
    ProblemDetails::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "SATELLITE_WORKER_UNAVAILABLE",
        "Satellite rendering is temporarily unavailable",
        "Retry the request or use a narrower/fixed frame request.",
        request_id,
    )
    .into_response()
}

fn source_revision_unavailable_problem(request_id: uuid::Uuid) -> Response {
    ProblemDetails::new(
        StatusCode::NOT_FOUND,
        "SATELLITE_SOURCE_REVISION_UNAVAILABLE",
        "Satellite source revision is unavailable",
        "The exact source revision is no longer renderable and was not found in the durable tile cache.",
        request_id,
    )
    .into_response()
}

fn problem(
    status: StatusCode,
    code: &'static str,
    detail: &'static str,
    request_id: uuid::Uuid,
) -> Response {
    ProblemDetails::new(
        status,
        code,
        "Satellite request rejected",
        detail,
        request_id,
    )
    .into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use axum::body::to_bytes;
    use axum::http::Request;
    use rw_sat::archive::{NATIVE_FRAME_SCHEMA, NativeChannelSource};
    use tower::ServiceExt as _;

    use super::*;
    use crate::{AppConfig, TokenSet};

    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x60,
        0x60, 0x60, 0x60, 0x00, 0x00, 0x00, 0x05, 0x00, 0x01, 0xa5, 0xf6, 0x45, 0x40, 0x00, 0x00,
        0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    const TEST_SOURCE_REVISION: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    const OTHER_SOURCE_REVISION: &str =
        "2222222222222222222222222222222222222222222222222222222222222222";

    #[test]
    fn satellite_routes_accept_png_filename_suffixes_under_axum_08() {
        let _ = read_router();
    }

    #[test]
    fn tile_urls_are_absolute_frame_pinned_and_recipe_versioned() {
        assert_eq!(
            versioned_tile_url(
                "https://weather.example.edu/api",
                "g18",
                "fulldisk",
                "c13",
                "20260822T1941",
                TEST_SOURCE_REVISION,
            ),
            format!(
                "https://weather.example.edu/api/v1/satellite/g18/fulldisk/c13/20260822T1941/tiles/{SATELLITE_TILE_RECIPE_VERSION}/{TEST_SOURCE_REVISION}/{{z}}/{{x}}/{{y}}.png"
            )
        );
        assert_eq!(
            tile_cache_control(false, true),
            VERSIONED_EXACT_CACHE_CONTROL
        );
        assert_eq!(tile_cache_control(false, false), LEGACY_EXACT_CACHE_CONTROL);
        assert_eq!(tile_cache_control(true, true), LATEST_CACHE_CONTROL);
    }

    #[test]
    fn request_origin_fallback_never_trusts_forwarded_headers() {
        let uri = "/v1/satellite/g18/full_disk/c13/latest/tilejson.json"
            .parse::<Uri>()
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:8788"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("attacker.example"),
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(
            public_base_url(None, &uri, &headers).unwrap(),
            "http://127.0.0.1:8788"
        );
        assert_eq!(
            public_base_url(Some("https://weather.example.edu/api"), &uri, &headers,).unwrap(),
            "https://weather.example.edu/api"
        );

        headers.insert(header::HOST, HeaderValue::from_static("user@example.edu"));
        assert!(public_base_url(None, &uri, &headers).is_err());
    }

    #[test]
    fn conditional_get_accepts_lists_and_weak_request_validators() {
        let body = b"stable satellite bytes".to_vec();
        let etag = format!("\"{}\"", blake3::hash(&body).to_hex());
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"different\", W/{etag}")).unwrap(),
        );
        let response = cacheable_response(
            body,
            "image/png",
            VERSIONED_EXACT_CACHE_CONTROL,
            &headers,
            Some(("20260822T1941", Some(1_777_000_000))),
        );
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()[header::ETAG], etag);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            VERSIONED_EXACT_CACHE_CONTROL
        );
        assert_eq!(
            response.headers()["x-rw-satellite-recipe"],
            SATELLITE_TILE_RECIPE_VERSION
        );
    }

    #[tokio::test]
    async fn concurrent_same_key_requests_render_once_and_reuse_cached_etag() {
        let directory = tempfile::tempdir().unwrap();
        let disk_cache = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        let cache = moka::future::Cache::builder().max_capacity(32).build();
        let key = satellite_tile_cache_key(
            SATELLITE_TILE_RECIPE_VERSION,
            TEST_SOURCE_REVISION,
            "g18",
            "fulldisk",
            "c13",
            "20260822T1951",
            SatelliteXyz {
                zoom: 7,
                x: 19,
                y: 41,
            },
        );
        let renders = Arc::new(AtomicUsize::new(0));
        let mut requests = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let disk_cache = disk_cache.clone();
            let key = key.clone();
            let renders = renders.clone();
            requests.push(tokio::spawn(async move {
                cached_satellite_tile(&cache, &disk_cache, key, async move {
                    renders.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    Ok(fixture_cached_tile(
                        "20260822T1951",
                        TEST_SOURCE_REVISION,
                        b"one native png",
                    ))
                })
                .await
                .unwrap()
            }));
        }

        let mut shared = None;
        for request in requests {
            let tile = request.await.unwrap();
            if let Some(first) = &shared {
                assert!(Arc::ptr_eq(first, &tile));
            } else {
                shared = Some(tile);
            }
        }
        assert_eq!(renders.load(Ordering::SeqCst), 1);

        let tile = shared.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&tile.etag).unwrap(),
        );
        let response = tile_response(tile, VERSIONED_EXACT_CACHE_CONTROL, &headers);
        assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(response.headers()["x-rw-satellite-frame"], "20260822T1951");
        assert_eq!(renders.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_separates_renderer_recipe_and_exact_frame() {
        let directory = tempfile::tempdir().unwrap();
        let disk_cache = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        let cache = moka::future::Cache::builder().max_capacity(32).build();
        let renders = Arc::new(AtomicUsize::new(0));
        let identities = [
            (
                SATELLITE_TILE_RECIPE_VERSION,
                "20260822T1941",
                TEST_SOURCE_REVISION,
            ),
            (
                "rw-sat-native-v3-test",
                "20260822T1941",
                TEST_SOURCE_REVISION,
            ),
            (
                SATELLITE_TILE_RECIPE_VERSION,
                "20260822T1951",
                TEST_SOURCE_REVISION,
            ),
            (
                SATELLITE_TILE_RECIPE_VERSION,
                "20260822T1941",
                OTHER_SOURCE_REVISION,
            ),
        ];

        for (recipe, frame, source_revision) in identities {
            let key = satellite_tile_cache_key(
                recipe,
                source_revision,
                "g18",
                "fulldisk",
                "c13",
                frame,
                SatelliteXyz {
                    zoom: 7,
                    x: 19,
                    y: 41,
                },
            );
            let renders = renders.clone();
            cached_satellite_tile(&cache, &disk_cache, key, async move {
                renders.fetch_add(1, Ordering::SeqCst);
                Ok(fixture_cached_tile(
                    frame,
                    source_revision,
                    frame.as_bytes(),
                ))
            })
            .await
            .unwrap();
        }

        let first_key = satellite_tile_cache_key(
            SATELLITE_TILE_RECIPE_VERSION,
            TEST_SOURCE_REVISION,
            "g18",
            "fulldisk",
            "c13",
            "20260822T1941",
            SatelliteXyz {
                zoom: 7,
                x: 19,
                y: 41,
            },
        );
        let renders_again = renders.clone();
        cached_satellite_tile(&cache, &disk_cache, first_key, async move {
            renders_again.fetch_add(1, Ordering::SeqCst);
            Ok(fixture_cached_tile(
                "20260822T1941",
                TEST_SOURCE_REVISION,
                b"must not render",
            ))
        })
        .await
        .unwrap();

        assert_eq!(renders.load(Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn fresh_process_cache_reuses_durable_exact_tile_without_rendering() {
        let directory = tempfile::tempdir().unwrap();
        let key = satellite_tile_cache_key(
            SATELLITE_TILE_RECIPE_VERSION,
            TEST_SOURCE_REVISION,
            "g18",
            "fulldisk",
            "open_geocolor_v1",
            "20260822T1951",
            SatelliteXyz {
                zoom: 7,
                x: 19,
                y: 41,
            },
        );
        let renders = Arc::new(AtomicUsize::new(0));

        let first_memory = moka::future::Cache::builder().max_capacity(32).build();
        let first_disk = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        let first_renders = renders.clone();
        let first = cached_satellite_tile(&first_memory, &first_disk, key.clone(), async move {
            first_renders.fetch_add(1, Ordering::SeqCst);
            Ok(fixture_cached_tile(
                "20260822T1951",
                TEST_SOURCE_REVISION,
                TINY_PNG,
            ))
        })
        .await
        .unwrap();
        assert_eq!(renders.load(Ordering::SeqCst), 1);
        drop(first_memory);
        drop(first_disk);

        let restarted_memory = moka::future::Cache::builder().max_capacity(32).build();
        let reopened_disk = SatelliteTileDiskCache::open(directory.path(), 1024 * 1024).unwrap();
        let restarted_renders = renders.clone();
        let reopened = cached_satellite_tile(&restarted_memory, &reopened_disk, key, async move {
            restarted_renders.fetch_add(1, Ordering::SeqCst);
            Ok(fixture_cached_tile(
                "20260822T1951",
                TEST_SOURCE_REVISION,
                b"must not render",
            ))
        })
        .await
        .unwrap();

        assert_eq!(renders.load(Ordering::SeqCst), 1);
        assert_eq!(reopened.etag, first.etag);
        assert_eq!(reopened.png, first.png);
    }

    #[tokio::test]
    async fn revisioned_http_tile_survives_native_source_retention() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.server.store_root = directory.path().join("read-only-store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let state = AppState::new(config, TokenSet::default()).unwrap();
        let frame = "20260822T1951";
        let key = satellite_tile_cache_key(
            SATELLITE_TILE_RECIPE_VERSION,
            TEST_SOURCE_REVISION,
            "g18",
            "fulldisk",
            "c13",
            frame,
            SatelliteXyz {
                zoom: 0,
                x: 0,
                y: 0,
            },
        );
        state
            .satellite_tile_disk_cache
            .store(
                &key,
                fixture_cached_tile(frame, TEST_SOURCE_REVISION, TINY_PNG).as_ref(),
            )
            .unwrap();
        // There is deliberately no native manifest or NetCDF source. The
        // exact immutable URL must remain serviceable from the derived cache.
        let app = read_router()
            .with_state(state)
            .layer(Extension(RequestId(uuid::Uuid::nil())));
        let path = format!(
            "/v1/satellite/g18/fulldisk/c13/{frame}/tiles/{SATELLITE_TILE_RECIPE_VERSION}/{TEST_SOURCE_REVISION}/0/0/0.png"
        );
        let response = app
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            VERSIONED_EXACT_CACHE_CONTROL
        );
        assert_eq!(response.headers()["x-rw-satellite-frame"], frame);
        assert_eq!(
            to_bytes(response.into_body(), 1024 * 1024).await.unwrap(),
            Bytes::from_static(TINY_PNG)
        );
    }

    #[test]
    fn latest_tile_cache_identity_is_the_resolved_complete_frame() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        fs::create_dir_all(&store_root).unwrap();
        write_frame_manifest(&store_root, "20260822T1941");
        write_frame_manifest(&store_root, "20260822T1951");

        let resolved = resolve_native_frame_with_revision(
            &store_root,
            "g18",
            "fulldisk",
            GoesAbiProduct::CleanInfrared,
            "latest",
        )
        .unwrap();
        let key = satellite_tile_cache_key(
            SATELLITE_TILE_RECIPE_VERSION,
            &resolved.source_revision,
            "g18",
            "fulldisk",
            "clean_ir",
            &resolved.frame.frame_id,
            SatelliteXyz {
                zoom: 7,
                x: 19,
                y: 41,
            },
        );

        assert_eq!(resolved.frame.frame_id, "20260822T1951");
        assert_eq!(key.frame, "20260822T1951");
        assert_eq!(key.source_revision, resolved.source_revision);
        assert_ne!(key.frame, "latest");
    }

    #[tokio::test]
    async fn latest_tilejson_pins_one_exact_frame_and_revalidates() {
        let directory = tempfile::tempdir().unwrap();
        let store_root = directory.path().join("store");
        let artifact_root = directory.path().join("artifacts");
        let cache_root = directory.path().join("cache");
        fs::create_dir_all(&store_root).unwrap();
        fs::create_dir_all(&artifact_root).unwrap();
        write_frame_manifest(&store_root, "20260822T1941");
        write_frame_manifest(&store_root, "20260822T1951");

        let mut config = AppConfig::default();
        config.server.store_root = store_root;
        config.server.artifact_root = artifact_root;
        config.server.cache_root = cache_root;
        config.server.public_base_url = Some("https://weather.example.edu/api".into());
        let state = AppState::new(config, TokenSet::default()).unwrap();
        let app = read_router()
            .with_state(state)
            .layer(Extension(RequestId(uuid::Uuid::nil())));
        let path = "/v1/satellite/g18/full_disk/c13/latest/tilejson.json";

        let first = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first.headers()[header::CACHE_CONTROL],
            LATEST_TILEJSON_CACHE_CONTROL
        );
        assert_eq!(first.headers()["x-rw-satellite-frame"], "20260822T1951");
        let etag = first.headers()[header::ETAG].clone();
        let body = to_bytes(first.into_body(), 1024 * 1024).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["frame"], "20260822T1951");
        assert_eq!(value["rendererRecipe"], SATELLITE_TILE_RECIPE_VERSION);
        let source_revision = value["sourceRevision"].as_str().unwrap();
        assert!(valid_source_revision(source_revision));
        assert_eq!(
            value["tiles"][0],
            format!(
                "https://weather.example.edu/api/v1/satellite/g18/fulldisk/c13/20260822T1951/tiles/{SATELLITE_TILE_RECIPE_VERSION}/{source_revision}/{{z}}/{{x}}/{{y}}.png"
            )
        );

        let second = app
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::IF_NONE_MATCH, etag)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.status(), StatusCode::NOT_MODIFIED);
    }

    fn write_frame_manifest(store_root: &std::path::Path, frame_id: &str) {
        let source_bytes = b"fixture native source";
        let content_blake3 = blake3::hash(source_bytes).to_hex().to_string();
        let relative_path = format!(
            ".rw-satellite-sources/g18/fulldisk/{}/{frame_id}/c13-{content_blake3}.nc",
            &frame_id[..8],
        );
        let channel = NativeChannelSource {
            channel: 13,
            object_key: format!("fixture/{frame_id}/c13.nc"),
            relative_path,
            byte_size: u64::try_from(source_bytes.len()).unwrap(),
            content_blake3: Some(content_blake3),
            scan_start_unix: 1_777_000_000,
            scan_end_unix: 1_777_000_600,
        };
        let manifest = rw_sat::NativeSatelliteFrame {
            schema: NATIVE_FRAME_SCHEMA.into(),
            platform: "g18".into(),
            sector: "fulldisk".into(),
            frame_id: frame_id.into(),
            scan_start_unix: channel.scan_start_unix,
            scan_end_unix: channel.scan_end_unix,
            channels: BTreeMap::from([(13, channel.clone())]),
        };
        let directory = rw_sat::native_archive_root(store_root)
            .join("g18")
            .join("fulldisk")
            .join(&frame_id[..8])
            .join(frame_id);
        fs::create_dir_all(&directory).unwrap();
        fs::write(store_root.join(&channel.relative_path), source_bytes).unwrap();
        fs::write(
            directory.join("frame.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn fixture_cached_tile(
        frame_id: &str,
        source_revision: &str,
        _png: &[u8],
    ) -> Arc<CachedSatelliteTile> {
        let png = Bytes::from_static(TINY_PNG);
        Arc::new(CachedSatelliteTile {
            etag: format!("\"{}\"", blake3::hash(&png).to_hex()),
            png,
            frame_id: frame_id.to_string(),
            source_revision: source_revision.to_string(),
            valid_unix: 1_777_000_000,
        })
    }
}

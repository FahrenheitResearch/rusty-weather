//! Native GOES satellite catalog and MapLibre/Mapbox tile delivery.

use std::io;

use axum::body::Body;
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use rw_sat::{
    DEFAULT_TILE_SIZE, GoesAbiProduct, MAXIMUM_TILE_ZOOM, SatelliteEnhancement,
    SatelliteProductDescriptor, SatelliteSectorDescriptor, list_native_frames, product_catalog,
    render_native_xyz_tile, sector_catalog,
};
use serde::{Deserialize, Serialize};
use tracing::error;

use crate::problem::ProblemDetails;
use crate::routes::RequestId;
use crate::{AppState, ExecutionError};

pub(crate) fn read_router() -> Router<AppState> {
    Router::new()
        .route("/v1/satellite/catalog", get(catalog))
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
            get(tile),
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
    geocolor_note: &'static str,
}

async fn catalog(Query(query): Query<CatalogQuery>) -> Json<SatelliteCatalogResponse> {
    Json(SatelliteCatalogResponse {
        schema: "rw-server.satellite-catalog.v2",
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
        geocolor_note: "Daytime pseudo-natural color blended with ABI C13 infrared at night; no city-light dataset is implied.",
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
    #[serde(default = "default_frame_limit")]
    limit: usize,
}

const fn default_frame_limit() -> usize {
    120
}

#[derive(Debug, Serialize)]
struct FrameDescriptor {
    id: String,
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
    if query.limit == 0 || query.limit > 2_000 {
        return problem(
            StatusCode::BAD_REQUEST,
            "INVALID_SATELLITE_FRAME_LIMIT",
            "Satellite frame limit must be between 1 and 2000.",
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
    match state
        .run_heavy_sync(move || {
            list_native_frames(&store_root, &platform, &sector_slug, product, query.limit)
        })
        .await
    {
        Ok(Ok(found)) => Json(FramesResponse {
            schema: "rw-server.satellite-frames.v2",
            platform: path.platform.to_ascii_lowercase(),
            sector: sector.slug().to_string(),
            product: product.descriptor(),
            cadence_seconds: sector.cadence_secs(),
            frames: found
                .into_iter()
                .map(|frame| FrameDescriptor {
                    id: frame.frame_id,
                    scan_start_unix: frame.scan_start_unix,
                    scan_end_unix: frame.scan_end_unix,
                    channels: frame.channels.into_keys().collect(),
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
}

async fn tilejson(
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<FramePath>,
) -> Response {
    let Some(product) = GoesAbiProduct::parse(&path.product) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let Some(sector) = rw_sat::s3::Sector::parse(&path.sector) else {
        return ProblemDetails::not_found(request_id.0).into_response();
    };
    let frame = path.frame.to_ascii_lowercase();
    let platform = path.platform.to_ascii_lowercase();
    let product_slug = product.slug();
    Json(TileJsonResponse {
        tilejson: "3.0.0",
        name: format!("{} · {} · {}", platform, sector.slug(), product.title()),
        description: product.description().to_string(),
        scheme: "xyz",
        tiles: vec![format!(
            "/v1/satellite/{platform}/{}/{product_slug}/{frame}/tiles/{{z}}/{{x}}/{{y}}.png",
            sector.slug()
        )],
        minzoom: 0,
        maxzoom: MAXIMUM_TILE_ZOOM,
        bounds: [-180.0, -85.051_128_78, 180.0, 85.051_128_78],
        attribution: "NOAA/NESDIS; rendered by Rusty Weather",
        tile_size: DEFAULT_TILE_SIZE,
    })
    .into_response()
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

async fn tile(
    State(state): State<AppState>,
    Extension(request_id): Extension<RequestId>,
    Path(path): Path<TilePath>,
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
    let store_root = state.config.server.store_root.clone();
    let platform = path.platform.to_ascii_lowercase();
    let sector_slug = sector.slug().to_string();
    let frame = path.frame.clone();
    let result = state
        .run_heavy_sync(move || {
            render_native_xyz_tile(
                &store_root,
                &platform,
                &sector_slug,
                product,
                &frame,
                path.z,
                path.x,
                tile_y,
                DEFAULT_TILE_SIZE,
            )
            .map_err(|error| error.to_string())
        })
        .await;
    match result {
        Ok(Ok(tile)) => {
            let mut response = Response::new(Body::from(tile.png));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                HeaderValue::from_static(if requested_latest {
                    "no-cache, max-age=0"
                } else {
                    "public, max-age=31536000, immutable"
                }),
            );
            if let Ok(value) = HeaderValue::from_str(&tile.frame_id) {
                response.headers_mut().insert("x-rw-satellite-frame", value);
            }
            if let Ok(value) = HeaderValue::from_str(&tile.valid_unix.to_string()) {
                response.headers_mut().insert("x-rw-valid-unix", value);
            }
            response
        }
        Ok(Err(error)) => satellite_render_problem(error, request_id.0),
        Err(error) => execution_problem(error, request_id.0),
    }
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
    use super::*;

    #[test]
    fn satellite_routes_accept_png_filename_suffixes_under_axum_08() {
        let _ = read_router();
    }
}

//! Authenticated operations HTTP boundary.
//!
//! These routes are absent unless `[operations].enabled = true`. They use
//! operations-specific least-privilege credentials even when the ordinary
//! server is intentionally unauthenticated on loopback.

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::auth::OperationsScope;
use crate::problem::ProblemDetails;
use crate::routes::RequestId;

pub(crate) fn router(state: AppState) -> Router<AppState> {
    if !state.config.operations.enabled {
        return Router::new();
    }

    crate::storms::router(state.clone()).route_layer(middleware::from_fn_with_state(
        state,
        require_operations_read,
    ))
}

async fn require_operations_read(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    require_operations_scope(state, request, next, OperationsScope::Read).await
}

async fn require_operations_scope(
    state: AppState,
    mut request: Request,
    next: Next,
    required: OperationsScope,
) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .copied()
        .unwrap_or(RequestId(uuid::Uuid::nil()));
    let Some(principal) = state
        .operations_tokens
        .authorize(request.headers().get(header::AUTHORIZATION))
    else {
        state.metrics.reject();
        let mut response = private_response(ProblemDetails::unauthorized(request_id.0));
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"rusty-weather-operations\""),
        );
        return response;
    };
    if !principal.scope.permits(required) {
        state.metrics.reject();
        return private_response(ProblemDetails::new(
            StatusCode::FORBIDDEN,
            "OPS_SCOPE_FORBIDDEN",
            "Operations scope is insufficient",
            "Use a bearer token granted the required operations scope.",
            request_id.0,
        ));
    }
    request.extensions_mut().insert(principal);
    private_headers(next.run(request).await)
}

fn private_response(problem: ProblemDetails) -> Response {
    private_headers(problem.into_response())
}

fn private_headers(mut response: Response) -> Response {
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request};
    use rw_ops_protocol::STORM_METHODS_PATH;
    use std::fs;
    use tower::ServiceExt as _;

    const READ_TOKEN: &str = "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr";
    const ADMIN_A_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const LEGACY_DATA_TOKEN: &str = "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz";

    fn test_config(directory: &tempfile::TempDir) -> crate::AppConfig {
        let read_path = directory.path().join("ops-read.tokens");
        let admin_path = directory.path().join("ops-admin.tokens");
        crate::test_support::write_private_file(&read_path, READ_TOKEN);
        crate::test_support::write_private_file(&admin_path, ADMIN_A_TOKEN);

        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        config.server.cache_root = directory.path().join("cache");
        config.operations.enabled = true;
        config.operations.root = directory.path().join("operations");
        config.auth.ops_read_token_file = Some(read_path);
        config.auth.ops_admin_token_file = Some(admin_path);
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        config.validate(false).unwrap();
        config
    }

    fn app(config: crate::AppConfig) -> Router {
        app_with_tokens(config, crate::TokenSet::default())
    }

    fn app_with_tokens(config: crate::AppConfig, tokens: crate::TokenSet) -> Router {
        crate::build_router(crate::AppState::new(config, tokens).expect("open test state"))
            .expect("build test router")
    }

    async fn get(app: &Router, path: &str, token: Option<&str>) -> Response {
        let mut builder = Request::builder().method(Method::GET).uri(path);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        app.clone()
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    fn assert_private(response: &Response) {
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-store, private"
        );
        assert!(response.headers().get(header::ETAG).is_none());
    }

    #[tokio::test]
    async fn operations_routes_require_an_operations_credential() {
        let directory = tempfile::tempdir().unwrap();
        let secure = app(test_config(&directory));

        let response = get(&secure, STORM_METHODS_PATH, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        assert_private(&response);

        let response = get(&secure, STORM_METHODS_PATH, Some(READ_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private(&response);
    }

    #[tokio::test]
    async fn operations_are_absent_when_default_disabled() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = crate::AppConfig::default();
        config.server.store_root = directory.path().join("store");
        config.server.artifact_root = directory.path().join("artifacts");
        fs::create_dir_all(&config.server.store_root).unwrap();
        fs::create_dir_all(&config.server.artifact_root).unwrap();
        let disabled = app(config);

        let response = get(&disabled, STORM_METHODS_PATH, None).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn general_api_tokens_require_explicit_legacy_operations_admin_gate() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = test_config(&directory);
        let legacy = crate::TokenSet::from_tokens([LEGACY_DATA_TOKEN]).unwrap();
        config.validate(true).unwrap();
        let secure = app_with_tokens(config.clone(), legacy.clone());

        let response = get(&secure, STORM_METHODS_PATH, Some(ADMIN_A_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private(&response);

        let response = get(&secure, STORM_METHODS_PATH, Some(LEGACY_DATA_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_private(&response);

        config.auth.legacy_api_tokens_are_operations_admins = true;
        config.validate(true).unwrap();
        let compatibility = app_with_tokens(config, legacy);

        let response = get(&compatibility, STORM_METHODS_PATH, Some(LEGACY_DATA_TOKEN)).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_private(&response);
    }
}

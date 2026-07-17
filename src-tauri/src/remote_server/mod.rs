mod auth;
mod config;
mod error;

use std::sync::Arc;

use auth::TokenAuthenticator;
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use error::{is_error_envelope, request_id, success, ApiError, RequestIdGenerator};
use serde_json::json;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
    request_id::{PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

pub use auth::Role;
pub use config::{ConfigError, ServerConfig};

pub fn build_router(config: ServerConfig) -> Result<Router, ConfigError> {
    build_router_with_private_routes(config, Router::new())
}

pub fn build_router_with_private_routes(
    config: ServerConfig,
    additional_private_routes: Router,
) -> Result<Router, ConfigError> {
    config.validate()?;

    let authenticator = Arc::new(TokenAuthenticator::from_config(&config));
    let admin_routes = Router::new()
        .route("/api/v1/projects", post(create_project))
        .route_layer(middleware::from_fn(require_admin));
    let routes = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/projects", get(list_projects))
        .merge(admin_routes)
        .nest("/api/v1", additional_private_routes)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed);
    let routes = with_body_limit(routes, config.max_upload_bytes).layer(
        middleware::from_fn_with_state(authenticator, authenticate_private_api),
    );

    Ok(routes
        .layer(cors_layer(&config.allowed_origins))
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(RequestIdGenerator))
        .layer(middleware::from_fn(remove_empty_request_id)))
}

fn with_body_limit(router: Router, max_upload_bytes: usize) -> Router {
    router
        .layer(DefaultBodyLimit::max(max_upload_bytes))
        .layer(RequestBodyLimitLayer::new(max_upload_bytes))
        .layer(middleware::from_fn(envelope_body_limit_rejections))
        .layer(middleware::from_fn_with_state(
            max_upload_bytes,
            enforce_declared_body_limit,
        ))
}

async fn health(request: Request) -> Response {
    let request_id = request_id(request.extensions());
    success(
        StatusCode::OK,
        json!({
            "service": "image-annotation-server",
            "version": env!("CARGO_PKG_VERSION"),
            "runtime": "standalone",
            "capabilities": [
                "authenticated-project-listing",
                "role-based-authorization",
                "project-creation-placeholder"
            ]
        }),
        request_id,
    )
}

async fn list_projects(request: Request) -> Response {
    success(
        StatusCode::OK,
        Vec::<serde_json::Value>::new(),
        request_id(request.extensions()),
    )
}

async fn create_project(request: Request) -> Response {
    ApiError::not_implemented().into_response(request_id(request.extensions()))
}

async fn authenticate_private_api(
    State(authenticator): State<Arc<TokenAuthenticator>>,
    mut request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let is_api_v1 = path == "/api/v1" || path.starts_with("/api/v1/");
    let is_public_health = request.method() == Method::GET && path == "/api/v1/health";
    if !is_api_v1 || is_public_health {
        return next.run(request).await;
    }

    match authenticator.authenticate(request.headers()) {
        Some(role) => {
            request.extensions_mut().insert(role);
            next.run(request).await
        }
        None => ApiError::unauthorized().into_response(request_id(request.extensions())),
    }
}

async fn require_admin(request: Request, next: Next) -> Response {
    match request.extensions().get::<Role>().copied() {
        Some(role) if role.allows(Role::Admin) => next.run(request).await,
        Some(_) => ApiError::forbidden().into_response(request_id(request.extensions())),
        None => ApiError::unauthorized().into_response(request_id(request.extensions())),
    }
}

async fn not_found(request: Request) -> Response {
    ApiError::not_found().into_response(request_id(request.extensions()))
}

async fn method_not_allowed(request: Request) -> Response {
    ApiError::method_not_allowed().into_response(request_id(request.extensions()))
}

async fn remove_empty_request_id(mut request: Request, next: Next) -> Response {
    let has_empty_request_id = request
        .headers()
        .get("x-request-id")
        .is_some_and(|value| value.as_bytes().is_empty());
    if has_empty_request_id {
        request.headers_mut().remove("x-request-id");
    }

    next.run(request).await
}

async fn enforce_declared_body_limit(
    State(max_upload_bytes): State<usize>,
    request: Request,
    next: Next,
) -> Response {
    let content_length = request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());

    if content_length.is_some_and(|length| length > max_upload_bytes as u64) {
        return ApiError::payload_too_large().into_response(request_id(request.extensions()));
    }

    next.run(request).await
}

async fn envelope_body_limit_rejections(request: Request, next: Next) -> Response {
    let response_request_id = request_id(request.extensions());
    let response = next.run(request).await;

    if response.status() == StatusCode::PAYLOAD_TOO_LARGE && !is_error_envelope(&response) {
        ApiError::payload_too_large().into_response(response_request_id)
    } else {
        response
    }
}

fn cors_layer(origins: &[String]) -> CorsLayer {
    let layer = CorsLayer::new()
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PATCH,
            Method::PUT,
            Method::DELETE,
        ])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]);

    if origins.is_empty() {
        layer
    } else {
        let origins = origins
            .iter()
            .map(|origin| {
                origin
                    .parse::<HeaderValue>()
                    .expect("origins are validated")
            })
            .collect::<Vec<_>>();
        layer.allow_origin(AllowOrigin::list(origins))
    }
}

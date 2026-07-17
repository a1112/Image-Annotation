mod auth;
mod config;
mod error;

use std::sync::Arc;

use auth::{Authorization, TokenAuthenticator};
use axum::{
    extract::{Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Router,
};
use error::{request_id, success, ApiError, RequestIdGenerator};
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
    config.validate()?;

    let authenticator = Arc::new(TokenAuthenticator::from_config(&config));
    let reader_routes = with_body_limit(
        Router::new().route("/api/v1/projects", get(list_projects)),
        config.max_upload_bytes,
    )
    .route_layer(middleware::from_fn_with_state(
        authenticator.clone(),
        require_reader,
    ));
    let admin_routes = with_body_limit(
        Router::new().route("/api/v1/projects", post(create_project)),
        config.max_upload_bytes,
    )
    .route_layer(middleware::from_fn_with_state(authenticator, require_admin));

    Ok(Router::new()
        .route("/api/v1/health", get(health))
        .merge(reader_routes)
        .merge(admin_routes)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(cors_layer(&config.allowed_origins))
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(RequestIdGenerator))
        .layer(middleware::from_fn(remove_empty_request_id)))
}

fn with_body_limit(router: Router, max_upload_bytes: usize) -> Router {
    router
        .layer(RequestBodyLimitLayer::new(max_upload_bytes))
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

async fn require_reader(
    State(authenticator): State<Arc<TokenAuthenticator>>,
    request: Request,
    next: Next,
) -> Response {
    authorize(authenticator, Role::Reader, request, next).await
}

async fn require_admin(
    State(authenticator): State<Arc<TokenAuthenticator>>,
    request: Request,
    next: Next,
) -> Response {
    authorize(authenticator, Role::Admin, request, next).await
}

async fn authorize(
    authenticator: Arc<TokenAuthenticator>,
    required: Role,
    request: Request,
    next: Next,
) -> Response {
    match authenticator.authorize(request.headers(), required) {
        Authorization::Authorized => next.run(request).await,
        Authorization::Unauthorized => {
            ApiError::unauthorized().into_response(request_id(request.extensions()))
        }
        Authorization::Forbidden => {
            ApiError::forbidden().into_response(request_id(request.extensions()))
        }
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

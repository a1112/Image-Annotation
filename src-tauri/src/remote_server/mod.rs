mod annotations;
mod auth;
mod config;
mod error;
mod imports;
mod projects;
mod samples;
mod service;
mod storage;

use std::sync::Arc;

use auth::TokenAuthenticator;
use axum::{
    extract::{DefaultBodyLimit, Request, State},
    http::{header, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use error::{is_error_envelope, request_id, success, ApiError, RequestIdGenerator};
use serde_json::json;
use service::RemoteSampleService;
use tower_http::{
    cors::{AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
    request_id::{PropagateRequestIdLayer, SetRequestIdLayer},
    trace::TraceLayer,
};

const DEFAULT_API_BODY_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const MULTIPART_FRAMING_ALLOWANCE_BYTES: usize = 1024 * 1024;

pub use auth::Role;
pub use config::{ConfigError, ServerConfig};
pub use error::ServerBuildError;

#[derive(Default)]
pub struct PrivateRouteGroups {
    reader: Option<Router>,
    editor: Option<Router>,
    admin: Option<Router>,
}

impl PrivateRouteGroups {
    pub fn with_reader(mut self, routes: Router) -> Self {
        self.reader = Some(merge_optional_router(self.reader, routes));
        self
    }

    pub fn with_editor(mut self, routes: Router) -> Self {
        self.editor = Some(merge_optional_router(self.editor, routes));
        self
    }

    pub fn with_admin(mut self, routes: Router) -> Self {
        self.admin = Some(merge_optional_router(self.admin, routes));
        self
    }
}

pub fn build_router(config: ServerConfig) -> Result<Router, ServerBuildError> {
    build_router_with_private_routes(config, PrivateRouteGroups::default())
}

pub fn build_router_with_private_routes(
    config: ServerConfig,
    private_route_groups: PrivateRouteGroups,
) -> Result<Router, ServerBuildError> {
    config.validate().map_err(ServerBuildError::from)?;

    let authenticator = Arc::new(TokenAuthenticator::from_config(&config));
    let service = RemoteSampleService::initialize(&config)?;
    let max_upload_bytes = config.max_upload_bytes;
    let private_route_groups = private_route_groups
        .with_reader(with_api_body_limit(projects::reader_routes(
            service.clone(),
        )))
        .with_reader(with_api_body_limit(samples::reader_routes(service.clone())))
        .with_reader(with_api_body_limit(annotations::reader_routes(
            service.clone(),
        )))
        .with_reader(with_api_body_limit(imports::reader_routes(service.clone())))
        .with_editor(with_api_body_limit(samples::editor_routes(service.clone())))
        .with_editor(with_api_body_limit(annotations::editor_routes(
            service.clone(),
        )))
        .with_editor(imports::editor_routes(service.clone(), max_upload_bytes))
        .with_admin(with_api_body_limit(projects::admin_routes(service)));
    let private_routes = protect_private_route_groups(private_route_groups);
    let routes = Router::new()
        .route("/api/v1/health", get(health))
        .nest("/api/v1", private_routes)
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(DEFAULT_API_BODY_LIMIT_BYTES))
        .layer(middleware::from_fn(envelope_body_limit_rejections))
        .layer(middleware::from_fn_with_state(
            authenticator,
            authenticate_private_api,
        ));

    Ok(routes
        .layer(cors_layer(&config.allowed_origins))
        .layer(TraceLayer::new_for_http())
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(RequestIdGenerator))
        .layer(middleware::from_fn(remove_incoming_request_id)))
}

fn merge_optional_router(existing: Option<Router>, routes: Router) -> Router {
    match existing {
        Some(existing) => existing.merge(routes),
        None => routes,
    }
}

fn protect_private_route_groups(groups: PrivateRouteGroups) -> Router {
    [
        (groups.reader, Role::Reader),
        (groups.editor, Role::Editor),
        (groups.admin, Role::Admin),
    ]
    .into_iter()
    .fold(Router::new(), |router, (routes, role)| {
        let Some(routes) = routes else {
            return router;
        };
        router.merge(routes.route_layer(middleware::from_fn_with_state(role, require_role)))
    })
}

fn with_api_body_limit(router: Router) -> Router {
    router
        .layer(DefaultBodyLimit::max(DEFAULT_API_BODY_LIMIT_BYTES))
        .layer(RequestBodyLimitLayer::new(DEFAULT_API_BODY_LIMIT_BYTES))
        .layer(middleware::from_fn_with_state(
            DEFAULT_API_BODY_LIMIT_BYTES,
            enforce_declared_body_limit,
        ))
}

pub fn with_upload_body_limit<S>(router: Router<S>, max_upload_bytes: usize) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    let extractor_limit = max_upload_bytes.saturating_add(MULTIPART_FRAMING_ALLOWANCE_BYTES);
    router.layer(DefaultBodyLimit::max(extractor_limit))
}

pub async fn shutdown_signal() {
    #[cfg(unix)]
    {
        shutdown_signal_unix().await;
    }

    #[cfg(not(unix))]
    {
        shutdown_signal_ctrl_c().await;
    }
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
                "remote-project-lifecycle",
                "recoverable-project-trash"
            ]
        }),
        request_id,
    )
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

async fn require_role(State(required_role): State<Role>, request: Request, next: Next) -> Response {
    match request.extensions().get::<Role>().copied() {
        Some(role) if role.allows(required_role) => next.run(request).await,
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

async fn remove_incoming_request_id(mut request: Request, next: Next) -> Response {
    request.headers_mut().remove("x-request-id");
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

#[cfg(unix)]
async fn shutdown_signal_unix() {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = match signal(SignalKind::terminate()) {
        Ok(signal) => signal,
        Err(error) => {
            tracing::error!(%error, "failed to listen for SIGTERM");
            shutdown_signal_ctrl_c().await;
            return;
        }
    };

    tokio::select! {
        result = tokio::signal::ctrl_c() => log_ctrl_c_result(result),
        signal = terminate.recv() => {
            if signal.is_some() {
                tracing::info!("SIGTERM received");
            } else {
                tracing::error!("SIGTERM signal stream closed");
            }
        }
    }
}

async fn shutdown_signal_ctrl_c() {
    log_ctrl_c_result(tokio::signal::ctrl_c().await);
}

fn log_ctrl_c_result(result: std::io::Result<()>) {
    match result {
        Ok(()) => tracing::info!("Ctrl+C received"),
        Err(error) => tracing::error!(%error, "failed to listen for Ctrl+C"),
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
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::IF_MATCH,
            header::IF_NONE_MATCH,
            header::RANGE,
        ])
        .expose_headers([
            header::ETAG,
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CONTENT_DISPOSITION,
            header::HeaderName::from_static("x-request-id"),
        ]);

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

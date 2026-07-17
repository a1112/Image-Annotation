use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use axum::{
    body::Body,
    extract::Multipart,
    http::{header, Method, Request, StatusCode},
    routing::post,
    Router,
};
use clap::CommandFactory;
use http_body_util::BodyExt;
use image_annotation_lib::remote_server::{
    build_router, build_router_with_private_routes, shutdown_signal, with_upload_body_limit,
    PrivateRouteGroups, Role, ServerConfig,
};
use serde_json::Value;
use tower::ServiceExt;

const READER_TOKEN: &str = "reader-token-0123456789abcdef0123456789abcdef";
const EDITOR_TOKEN: &str = "editor-token-0123456789abcdef0123456789abcdef";
const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const SHARED_TOKEN: &str = "shared-token-0123456789abcdef0123456789abcdef";
const HELP_READER_SECRET: &str = "help-reader-0123456789abcdef0123456789abcdef";
const HELP_EDITOR_SECRET: &str = "help-editor-0123456789abcdef0123456789abcdef";
const HELP_ADMIN_SECRET: &str = "help-admin-0123456789abcdef0123456789abcdef";

fn test_config(bind_ip: Ipv4Addr) -> ServerConfig {
    ServerConfig {
        bind: SocketAddr::new(IpAddr::V4(bind_ip), 17311),
        data_dir: PathBuf::from("test-data"),
        reader_token: None,
        editor_token: None,
        admin_token: None,
        allowed_origins: Vec::new(),
        max_upload_bytes: 2 * 1024 * 1024 * 1024,
    }
}

async fn request(
    config: ServerConfig,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let app = build_router(config).expect("test server config should be valid");
    let mut request = Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }

    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).expect("response should be valid JSON");

    (status, headers, json)
}

fn assert_request_id(headers: &axum::http::HeaderMap, body: &Value) {
    let header_request_id = headers
        .get("x-request-id")
        .expect("response should include X-Request-Id")
        .to_str()
        .unwrap();
    let body_request_id = body["requestId"]
        .as_str()
        .expect("response envelope should include requestId");

    assert!(!body_request_id.is_empty());
    assert_eq!(header_request_id, body_request_id);
}

async fn health_with_client_request_id(
    app: Router,
    client_request_id: &str,
) -> (axum::http::HeaderMap, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("x-request-id", client_request_id)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).unwrap();
    (headers, json)
}

async fn accept_multipart(mut multipart: Multipart) -> StatusCode {
    let field = multipart
        .next_field()
        .await
        .expect("multipart should parse")
        .expect("payload field should exist");
    let bytes = field.bytes().await.expect("payload should be readable");
    assert!(!bytes.is_empty());
    StatusCode::NO_CONTENT
}

fn multipart_body(boundary: &str, payload_bytes: usize) -> Vec<u8> {
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"payload\"; filename=\"payload.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend(std::iter::repeat_n(b'x', payload_bytes));
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[test]
fn non_loopback_bind_requires_token() {
    let mut config = test_config(Ipv4Addr::UNSPECIFIED);
    config.reader_token = Some(String::new());

    let error = config.validate().unwrap_err();

    assert_eq!(error.code(), "token_required_for_non_loopback_bind");
}

#[test]
fn loopback_without_token_allowed() {
    let config = test_config(Ipv4Addr::LOCALHOST);

    assert!(config.validate().is_ok());
}

#[test]
fn short_token_config_rejected() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some("too-short".to_string());

    let error = config.validate().unwrap_err();

    assert_eq!(error.code(), "token_too_short");
}

#[test]
fn role_order() {
    assert!(Role::Reader < Role::Editor);
    assert!(Role::Editor < Role::Admin);
    assert!(Role::Admin.allows(Role::Reader));
    assert!(!Role::Reader.allows(Role::Editor));
}

#[test]
fn shutdown_signal_is_a_sendable_future() {
    fn assert_shutdown_future<F>(_: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
    }

    assert_shutdown_future(shutdown_signal());
}

#[test]
fn cli_defaults_are_stable() {
    let config = ServerConfig::try_parse_from(["image-annotation-server"]).unwrap();

    assert_eq!(config.bind, "127.0.0.1:17311".parse().unwrap());
    assert_eq!(config.max_upload_bytes, 2048 * 1024 * 1024);
}

#[test]
fn cli_fields_parse_into_server_config() {
    let config = ServerConfig::try_parse_from([
        "image-annotation-server",
        "--bind",
        "127.0.0.1:18080",
        "--data-dir",
        "cli-data",
        "--reader-token",
        READER_TOKEN,
        "--editor-token",
        EDITOR_TOKEN,
        "--admin-token",
        ADMIN_TOKEN,
        "--allowed-origin",
        "https://one.example,https://two.example",
        "--max-upload-mib",
        "16",
    ])
    .unwrap();

    assert_eq!(config.bind, "127.0.0.1:18080".parse().unwrap());
    assert_eq!(config.data_dir, PathBuf::from("cli-data"));
    assert_eq!(config.reader_token.as_deref(), Some(READER_TOKEN));
    assert_eq!(config.editor_token.as_deref(), Some(EDITOR_TOKEN));
    assert_eq!(config.admin_token.as_deref(), Some(ADMIN_TOKEN));
    assert_eq!(
        config.allowed_origins,
        ["https://one.example", "https://two.example"]
    );
    assert_eq!(config.max_upload_bytes, 16 * 1024 * 1024);
}

#[test]
fn token_env_values_are_hidden_from_long_help() {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "render_server_help_with_token_env",
            "--ignored",
            "--nocapture",
        ])
        .env("IMAGE_ANNOTATION_READER_TOKEN", HELP_READER_SECRET)
        .env("IMAGE_ANNOTATION_EDITOR_TOKEN", HELP_EDITOR_SECRET)
        .env("IMAGE_ANNOTATION_ADMIN_TOKEN", HELP_ADMIN_SECRET)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{rendered}");
    assert!(!rendered.contains(HELP_READER_SECRET), "{rendered}");
    assert!(!rendered.contains(HELP_EDITOR_SECRET), "{rendered}");
    assert!(!rendered.contains(HELP_ADMIN_SECRET), "{rendered}");
}

#[test]
#[ignore]
fn render_server_help_with_token_env() {
    let mut command = ServerConfig::command();
    print!("{}", command.render_long_help());
}

#[test]
fn server_config_debug_redacts_tokens() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());

    let debug = format!("{config:?}");

    assert!(!debug.contains(READER_TOKEN));
    assert!(!debug.contains(EDITOR_TOKEN));
    assert!(!debug.contains(ADMIN_TOKEN));
    assert!(debug.contains("reader_token_configured: true"));
    assert!(debug.contains("editor_token_configured: true"));
    assert!(debug.contains("admin_token_configured: true"));
}

#[tokio::test]
async fn health_public() {
    let (status, headers, body) = request(
        test_config(Ipv4Addr::LOCALHOST),
        Method::GET,
        "/api/v1/health",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["service"], "image-annotation-server");
    assert!(body["data"]["version"].is_string());
    assert!(body["data"]["runtime"].is_string());
    assert!(body["data"]["capabilities"].is_array());
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn projects_requires_bearer() {
    let (status, headers, body) = request(
        test_config(Ipv4Addr::LOCALHOST),
        Method::GET,
        "/api/v1/projects",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert!(body["error"]["message"].is_string());
    assert!(body["error"]["details"].is_object());
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn unknown_private_route_requires_bearer() {
    let (status, headers, body) = request(
        test_config(Ipv4Addr::LOCALHOST),
        Method::GET,
        "/api/v1/unknown-private-route",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn private_route_wrong_method_requires_bearer() {
    let (status, headers, body) = request(
        test_config(Ipv4Addr::LOCALHOST),
        Method::PATCH,
        "/api/v1/projects",
        None,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn authenticated_unknown_private_route_is_not_found() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());

    let (status, headers, body) = request(
        config,
        Method::GET,
        "/api/v1/unknown-private-route",
        Some(READER_TOKEN),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "not_found");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn authenticated_private_route_wrong_method_is_not_allowed() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());

    let (status, headers, body) = request(
        config,
        Method::PATCH,
        "/api/v1/projects",
        Some(READER_TOKEN),
    )
    .await;

    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], "method_not_allowed");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn reader_can_list() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());

    let (status, headers, body) =
        request(config, Method::GET, "/api/v1/projects", Some(READER_TOKEN)).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], serde_json::json!([]));
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn reader_cannot_admin() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());

    let (status, headers, body) =
        request(config, Method::POST, "/api/v1/projects", Some(READER_TOKEN)).await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn role_classified_routes_enforce_editor_and_admin_before_handlers() {
    let editor_called = Arc::new(AtomicBool::new(false));
    let editor_state = editor_called.clone();
    let editor_routes = Router::new().route(
        "/editor-probe",
        post(move || {
            let editor_state = editor_state.clone();
            async move {
                editor_state.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let admin_called = Arc::new(AtomicBool::new(false));
    let admin_state = admin_called.clone();
    let admin_routes = Router::new().route(
        "/admin-probe",
        post(move || {
            let admin_state = admin_state.clone();
            async move {
                admin_state.store(true, Ordering::SeqCst);
                StatusCode::NO_CONTENT
            }
        }),
    );
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let groups = PrivateRouteGroups::default()
        .with_editor(editor_routes)
        .with_admin(admin_routes);
    let app = build_router_with_private_routes(config, groups).unwrap();

    let reader_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/editor-probe")
                .header(header::AUTHORIZATION, format!("Bearer {READER_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let reader_status = reader_response.status();
    let reader_headers = reader_response.headers().clone();
    let reader_body = reader_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let reader_json: Value = serde_json::from_slice(&reader_body).unwrap();

    assert_eq!(reader_status, StatusCode::FORBIDDEN);
    assert_eq!(reader_json["error"]["code"], "forbidden");
    assert!(!editor_called.load(Ordering::SeqCst));
    assert_request_id(&reader_headers, &reader_json);

    let editor_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/editor-probe")
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(editor_response.status(), StatusCode::NO_CONTENT);
    assert!(editor_called.load(Ordering::SeqCst));

    let editor_admin_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/admin-probe")
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let editor_admin_status = editor_admin_response.status();
    let editor_admin_headers = editor_admin_response.headers().clone();
    let editor_admin_body = editor_admin_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let editor_admin_json: Value = serde_json::from_slice(&editor_admin_body).unwrap();

    assert_eq!(editor_admin_status, StatusCode::FORBIDDEN);
    assert_eq!(editor_admin_json["error"]["code"], "forbidden");
    assert!(!admin_called.load(Ordering::SeqCst));
    assert_request_id(&editor_admin_headers, &editor_admin_json);

    let admin_response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/admin-probe")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(admin_response.status(), StatusCode::NO_CONTENT);
    assert!(admin_called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn admin_route_is_an_explicit_placeholder() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());

    let (status, headers, body) =
        request(config, Method::POST, "/api/v1/projects", Some(ADMIN_TOKEN)).await;

    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["error"]["code"], "not_implemented");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn invalid_token() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());

    let (status, headers, body) = request(
        config,
        Method::GET,
        "/api/v1/projects",
        Some("wrong-secret"),
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "unauthorized");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn malformed_authorization_uses_error_envelope() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    let app = build_router(config).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/projects")
                .header(header::AUTHORIZATION, "Bearer malformed token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "unauthorized");
    assert_request_id(&headers, &json);
}

#[tokio::test]
async fn empty_client_request_id_is_replaced() {
    let app = build_router(test_config(Ipv4Addr::LOCALHOST)).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header("x-request-id", "")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert_request_id(&headers, &json);
}

#[tokio::test]
async fn client_request_ids_are_never_echoed_and_server_ids_are_unique() {
    let app = build_router(test_config(Ipv4Addr::LOCALHOST)).unwrap();
    let forged = "client-forged-request-id";

    let (first_headers, first_body) = health_with_client_request_id(app.clone(), forged).await;
    let (second_headers, second_body) = health_with_client_request_id(app, forged).await;

    assert_request_id(&first_headers, &first_body);
    assert_request_id(&second_headers, &second_body);
    assert_ne!(first_body["requestId"], forged);
    assert_ne!(second_body["requestId"], forged);
    assert_ne!(first_body["requestId"], second_body["requestId"]);
}

#[tokio::test]
async fn configured_origin_allows_future_mutation_preflight() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.allowed_origins = vec!["https://annotation.example".to_string()];
    let app = build_router(config).unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/v1/projects")
                .header(header::ORIGIN, "https://annotation.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "PATCH")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&header::HeaderValue::from_static(
            "https://annotation.example"
        ))
    );
    let allowed_methods = response
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_METHODS)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        allowed_methods
            .split(',')
            .any(|method| method.trim() == "PATCH"),
        "allowed methods were {allowed_methods}"
    );
    assert!(response.headers().contains_key("x-request-id"));
}

#[tokio::test]
async fn configured_cors_allows_and_exposes_api_headers() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.allowed_origins = vec!["https://annotation.example".to_string()];
    let app = build_router(config).unwrap();
    let preflight = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/v1/projects")
                .header(header::ORIGIN, "https://annotation.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type,if-match,if-none-match,range",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let allowed_headers = preflight
        .headers()
        .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
        .expect("preflight should include allowed headers")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();

    for expected in [
        "authorization",
        "content-type",
        "if-match",
        "if-none-match",
        "range",
    ] {
        assert!(
            allowed_headers
                .split(',')
                .any(|value| value.trim() == expected),
            "allowed headers were {allowed_headers}"
        );
    }

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header(header::ORIGIN, "https://annotation.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let exposed_headers = response
        .headers()
        .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        .expect("CORS response should expose API headers")
        .to_str()
        .unwrap()
        .to_ascii_lowercase();

    for expected in [
        "etag",
        "content-range",
        "accept-ranges",
        "content-disposition",
        "x-request-id",
    ] {
        assert!(
            exposed_headers
                .split(',')
                .any(|value| value.trim() == expected),
            "exposed headers were {exposed_headers}"
        );
    }
}

#[tokio::test]
async fn unconfigured_origin_is_not_allowed() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.allowed_origins = vec!["https://annotation.example".to_string()];
    let app = build_router(config).unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/v1/projects")
                .header(header::ORIGIN, "https://untrusted.example")
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
}

#[tokio::test]
async fn cors_default_does_not_emit_wildcard() {
    let app = build_router(test_config(Ipv4Addr::LOCALHOST)).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/health")
                .header(header::ORIGIN, "https://annotation.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert!(!response
        .headers()
        .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN));
    assert_ne!(
        response.headers().get(header::ACCESS_CONTROL_ALLOW_ORIGIN),
        Some(&header::HeaderValue::from_static("*"))
    );
}

#[tokio::test]
async fn configured_limit_overrides_multipart_default() {
    const BOUNDARY: &str = "image-annotation-test-boundary";
    let body = multipart_body(BOUNDARY, 2 * 1024 * 1024 + 64 * 1024);

    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.max_upload_bytes = 4 * 1024 * 1024;
    let editor_routes = with_upload_body_limit(
        Router::new().route("/multipart-probe", post(accept_multipart)),
        config.max_upload_bytes,
    );
    let groups = PrivateRouteGroups::default().with_editor(editor_routes);
    let app = build_router_with_private_routes(config, groups).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/multipart-probe")
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(header::CONTENT_LENGTH, body.len().to_string())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn upload_limit_leaves_room_for_multipart_framing() {
    const BOUNDARY: &str = "image-annotation-framing-boundary";
    const FILE_BYTES: usize = 3 * 1024 * 1024;
    let body = multipart_body(BOUNDARY, FILE_BYTES);
    assert!(body.len() > FILE_BYTES);

    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.max_upload_bytes = FILE_BYTES;
    let editor_routes = with_upload_body_limit(
        Router::new().route("/multipart-framing-probe", post(accept_multipart)),
        config.max_upload_bytes,
    );
    let groups = PrivateRouteGroups::default().with_editor(editor_routes);
    let app = build_router_with_private_routes(config, groups).unwrap();
    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/multipart-framing-probe")
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .header(header::CONTENT_LENGTH, body.len().to_string())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn oversized_body_uses_error_envelope() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    config.max_upload_bytes = 4 * 1024 * 1024;
    let app = build_router(config).unwrap();
    let body = vec![b'x'; 2 * 1024 * 1024 + 1];

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/projects")
                .header(header::AUTHORIZATION, format!("Bearer {ADMIN_TOKEN}"))
                .header(header::CONTENT_LENGTH, body.len().to_string())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).expect("413 should use the error envelope");

    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(json["error"]["code"], "payload_too_large");
    assert_request_id(&headers, &json);
}

#[tokio::test]
async fn missing_credentials_precedes_body_limit() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.max_upload_bytes = 4 * 1024 * 1024;
    let app = build_router(config).unwrap();
    let body = vec![b'x'; 2 * 1024 * 1024 + 1];

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/projects")
                .header(header::CONTENT_LENGTH, body.len().to_string())
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json["error"]["code"], "unauthorized");
    assert_request_id(&headers, &json);
}

#[test]
fn duplicate_token_config() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(SHARED_TOKEN.to_string());
    config.admin_token = Some(SHARED_TOKEN.to_string());

    let error = config.validate().unwrap_err();

    assert_eq!(error.code(), "duplicate_role_token");
}

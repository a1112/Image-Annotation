use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
};

use axum::{
    body::Body,
    http::{header, Method, Request, StatusCode},
};
use http_body_util::BodyExt;
use image_annotation_lib::remote_server::{build_router, Role, ServerConfig};
use serde_json::Value;
use tower::ServiceExt;

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
fn role_order() {
    assert!(Role::Reader < Role::Editor);
    assert!(Role::Editor < Role::Admin);
    assert!(Role::Admin.allows(Role::Reader));
    assert!(!Role::Reader.allows(Role::Editor));
}

#[test]
fn cli_defaults_are_stable() {
    let config = ServerConfig::try_parse_from(["image-annotation-server"]).unwrap();

    assert_eq!(config.bind, "127.0.0.1:17311".parse().unwrap());
    assert_eq!(config.max_upload_bytes, 2048 * 1024 * 1024);
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
async fn reader_can_list() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some("reader-secret".to_string());

    let (status, headers, body) = request(
        config,
        Method::GET,
        "/api/v1/projects",
        Some("reader-secret"),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"], serde_json::json!([]));
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn reader_cannot_admin() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some("reader-secret".to_string());

    let (status, headers, body) = request(
        config,
        Method::POST,
        "/api/v1/projects",
        Some("reader-secret"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn admin_route_is_an_explicit_placeholder() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some("admin-secret".to_string());

    let (status, headers, body) = request(
        config,
        Method::POST,
        "/api/v1/projects",
        Some("admin-secret"),
    )
    .await;

    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
    assert_eq!(body["error"]["code"], "not_implemented");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn invalid_token() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some("reader-secret".to_string());

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
async fn oversized_body_uses_error_envelope() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some("admin-secret".to_string());
    config.max_upload_bytes = 1;
    let app = build_router(config).unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/projects")
                .header(header::AUTHORIZATION, "Bearer admin-secret")
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::from("{}"))
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
    config.max_upload_bytes = 1;
    let app = build_router(config).unwrap();

    let response = app
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/projects")
                .header(header::CONTENT_LENGTH, "2")
                .body(Body::from("{}"))
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
    config.reader_token = Some("shared-secret".to_string());
    config.admin_token = Some("shared-secret".to_string());

    let error = config.validate().unwrap_err();

    assert_eq!(error.code(), "duplicate_role_token");
}

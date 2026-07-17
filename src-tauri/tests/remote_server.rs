use std::{
    fs,
    future::Future,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier, Mutex, OnceLock,
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
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
use image_annotation_lib::{
    project_fs::ProjectManifest,
    remote_server::{
        build_router, build_router_with_private_routes, shutdown_signal, with_upload_body_limit,
        PrivateRouteGroups, Role, ServerConfig,
    },
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use tower::ServiceExt;

const READER_TOKEN: &str = "reader-token-0123456789abcdef0123456789abcdef";
const EDITOR_TOKEN: &str = "editor-token-0123456789abcdef0123456789abcdef";
const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const SHARED_TOKEN: &str = "shared-token-0123456789abcdef0123456789abcdef";
const HELP_READER_SECRET: &str = "help-reader-0123456789abcdef0123456789abcdef";
const HELP_EDITOR_SECRET: &str = "help-editor-0123456789abcdef0123456789abcdef";
const HELP_ADMIN_SECRET: &str = "help-admin-0123456789abcdef0123456789abcdef";
const CONCURRENT_DATA_DIR_ENV: &str = "IMAGE_ANNOTATION_CONCURRENT_TEST_DATA_DIR";
static PROJECT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_DATA_ROOT: OnceLock<PathBuf> = OnceLock::new();
static TEST_SCHEMA_MIGRATION: Mutex<()> = Mutex::new(());

struct RemoveDirectoryOnDrop(PathBuf);

impl Drop for RemoveDirectoryOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct RemoteProjectCleanup {
    data_dir: PathBuf,
    project_id: String,
}

impl RemoteProjectCleanup {
    fn new(data_dir: &Path, project_id: &str) -> Self {
        Self {
            data_dir: data_dir.to_path_buf(),
            project_id: project_id.to_string(),
        }
    }
}

impl Drop for RemoteProjectCleanup {
    fn drop(&mut self) {
        remove_directory_entry(&self.data_dir.join("projects").join(&self.project_id));
        remove_directory_entry(
            &self
                .data_dir
                .join("trash")
                .join("projects")
                .join(&self.project_id),
        );
        if let Ok(connection) = rusqlite::Connection::open(self.data_dir.join("server.sqlite")) {
            let _ = connection.execute(
                "DELETE FROM trashed_projects WHERE project_id = ?1",
                [&self.project_id],
            );
            let _ = connection.execute(
                "DELETE FROM project_metadata WHERE project_id = ?1",
                [&self.project_id],
            );
        }
    }
}

struct RemoveLinksOnDrop(Vec<PathBuf>);

impl Drop for RemoveLinksOnDrop {
    fn drop(&mut self) {
        for path in &self.0 {
            remove_directory_entry(path);
        }
    }
}

fn remove_directory_entry(path: &Path) {
    if !path.exists() && fs::symlink_metadata(path).is_err() {
        return;
    }
    if fs::remove_dir(path).is_ok() {
        return;
    }
    if fs::remove_file(path).is_ok() {
        return;
    }
    let _ = fs::remove_dir_all(path);
}

fn process_data_root() -> PathBuf {
    PROCESS_DATA_ROOT
        .get_or_init(|| {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "image-annotation-remote-server-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&root).unwrap();
            root
        })
        .clone()
}

fn run_ignored_test_in_subprocess(test_name: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{rendered}");
}

fn unique_project(prefix: &str) -> (String, String) {
    let sequence = PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!("{prefix} {} {sequence}", std::process::id());
    let id = name.to_ascii_lowercase().replace(' ', "-");
    (name, id)
}

fn fixture_manifest(id: &str, name: &str, root: &std::path::Path) -> ProjectManifest {
    ProjectManifest {
        id: id.to_string(),
        name: name.to_string(),
        source_dataset_key: "remote-workspace-test".to_string(),
        format: "yolo-detect".to_string(),
        root_path: root.to_string_lossy().to_string(),
        created_at: "fixture-created-at".to_string(),
        class_count: 0,
        image_count: 0,
    }
}

fn ensure_lifecycle_test_columns(connection: &rusqlite::Connection) {
    let _guard = TEST_SCHEMA_MIGRATION
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if !sqlite_column_exists(connection, "trashed_projects", "state") {
        connection
            .execute(
                "ALTER TABLE trashed_projects
                 ADD COLUMN state TEXT NOT NULL DEFAULT 'trashed'",
                [],
            )
            .unwrap();
    }
    if !sqlite_column_exists(connection, "service_audit", "status") {
        connection
            .execute(
                "ALTER TABLE service_audit
                 ADD COLUMN status TEXT NOT NULL DEFAULT 'completed'",
                [],
            )
            .unwrap();
    }
}

fn sqlite_column_exists(connection: &rusqlite::Connection, table: &str, column: &str) -> bool {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .unwrap()
        .filter_map(Result::ok)
        .any(|name| name == column);
    exists
}

fn set_trash_state(connection: &rusqlite::Connection, project_id: &str, state: &str) {
    connection
        .execute(
            r#"
            INSERT INTO trashed_projects (project_id, trashed_at, state)
            VALUES (?1, 'interrupted-at', ?2)
            ON CONFLICT(project_id) DO UPDATE SET state = excluded.state
            "#,
            rusqlite::params![project_id, state],
        )
        .unwrap();
}

fn trash_state(data_dir: &Path, project_id: &str) -> Option<String> {
    rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state FROM trashed_projects WHERE project_id = ?1",
            [project_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

#[cfg(unix)]
fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_directory_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, link)
}

fn test_config(bind_ip: Ipv4Addr) -> ServerConfig {
    ServerConfig {
        bind: SocketAddr::new(IpAddr::V4(bind_ip), 17311),
        data_dir: process_data_root(),
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

async fn router_request(
    app: &Router,
    method: Method,
    uri: &str,
    bearer: &str,
    json_body: Option<Value>,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    let body = match json_body {
        Some(value) => {
            request = request.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&value).unwrap())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).expect("response should be valid JSON");

    (status, headers, json)
}

async fn create_empty_project(app: &Router, name: &str, project_id: &str) -> Value {
    let (status, _, project) = router_request(
        app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    assert_eq!(project["data"]["id"], project_id);
    project
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
fn concurrent_router_initialization_for_same_data_dir_succeeds() {
    let data_dir = std::env::temp_dir().join(format!(
        "image-annotation-concurrent-router-{}-{}",
        std::process::id(),
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "concurrent_router_initialization_child",
            "--ignored",
            "--nocapture",
        ])
        .env(CONCURRENT_DATA_DIR_ENV, &data_dir)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let _ = fs::remove_dir_all(data_dir);
    assert!(output.status.success(), "{rendered}");
}

#[test]
#[ignore]
fn concurrent_router_initialization_child() {
    const ROUNDS: usize = 12;
    const WORKERS: usize = 64;
    let data_dir = PathBuf::from(std::env::var_os(CONCURRENT_DATA_DIR_ENV).unwrap());
    fs::create_dir_all(&data_dir).unwrap();
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_dir.clone();

    for round in 0..ROUNDS {
        for suffix in ["", "-wal", "-shm"] {
            let _ = fs::remove_file(data_dir.join(format!("server.sqlite{suffix}")));
        }
        let barrier = Arc::new(Barrier::new(WORKERS));
        let handles = (0..WORKERS)
            .map(|_| {
                let barrier = barrier.clone();
                let config = config.clone();
                thread::spawn(move || {
                    barrier.wait();
                    build_router(config)
                        .map(|_| ())
                        .map_err(|error| error.code())
                })
            })
            .collect::<Vec<_>>();
        let results = handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .expect("router initialization thread panicked")
            })
            .collect::<Vec<_>>();

        assert!(
            results.iter().all(Result::is_ok),
            "round {round} concurrent router initialization failures: {results:?}"
        );
    }
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
    assert!(body["data"].is_array());
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
async fn invalid_project_create_uses_validation_envelope() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());

    let (status, headers, body) =
        request(config, Method::POST, "/api/v1/projects", Some(ADMIN_TOKEN)).await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "validation");
    assert_request_id(&headers, &body);
}

#[tokio::test]
async fn admin_project_lifecycle_moves_directories_and_records_audit() {
    let (name, project_id) = unique_project("Task3 lifecycle");
    let renamed = format!("{name} renamed");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();

    let (create_status, create_headers, created) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect"
        })),
    )
    .await;
    assert_eq!(create_status, StatusCode::CREATED, "{created}");
    assert_eq!(created["data"]["id"], project_id);
    assert_eq!(created["data"]["name"], name);
    assert_request_id(&create_headers, &created);
    let create_request_id = created["requestId"].as_str().unwrap().to_string();

    let (active_duplicate_status, _, active_duplicate) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect"
        })),
    )
    .await;
    assert_eq!(
        active_duplicate_status,
        StatusCode::CONFLICT,
        "{active_duplicate}"
    );
    assert_eq!(active_duplicate["error"]["code"], "conflict");

    let active_dir = data_dir.join("projects").join(&project_id);
    let trash_dir = data_dir.join("trash").join("projects").join(&project_id);
    assert!(active_dir.join("project.json").is_file());
    assert!(active_dir.join("project.sqlite").is_file());

    let (list_status, _, listed) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(list_status, StatusCode::OK, "{listed}");
    assert!(listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == project_id));

    let project_uri = format!("/api/v1/projects/{project_id}");
    let (get_status, _, detail) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(get_status, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["id"], project_id);

    let (editor_get_status, _, editor_detail) =
        router_request(&app, Method::GET, &project_uri, EDITOR_TOKEN, None).await;
    assert_eq!(editor_get_status, StatusCode::OK, "{editor_detail}");

    let (editor_patch_status, _, editor_patch) = router_request(
        &app,
        Method::PATCH,
        &project_uri,
        EDITOR_TOKEN,
        Some(serde_json::json!({"name": renamed})),
    )
    .await;
    assert_eq!(editor_patch_status, StatusCode::FORBIDDEN, "{editor_patch}");
    assert_eq!(editor_patch["error"]["code"], "forbidden");

    let (rename_status, _, renamed_project) = router_request(
        &app,
        Method::PATCH,
        &project_uri,
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": renamed,
            "description": "remote lifecycle project"
        })),
    )
    .await;
    assert_eq!(rename_status, StatusCode::OK, "{renamed_project}");
    assert_eq!(renamed_project["data"]["name"], renamed);
    assert_eq!(
        renamed_project["data"]["description"],
        "remote lifecycle project"
    );
    let manifest: Value =
        serde_json::from_slice(&fs::read(active_dir.join("project.json")).unwrap()).unwrap();
    assert_eq!(manifest["name"], renamed);
    let project_db = rusqlite::Connection::open(active_dir.join("project.sqlite")).unwrap();
    let indexed_name: String = project_db
        .query_row("SELECT name FROM projects LIMIT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(indexed_name, renamed);
    drop(project_db);

    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &project_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["data"]["projectId"], project_id);
    assert_eq!(deleted["data"]["status"], "trashed");
    assert!(!active_dir.exists());
    assert!(trash_dir.join("project.json").is_file());

    let (missing_status, _, missing) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["error"]["code"], "not_found");
    let (trashed_list_status, _, trashed_list) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(trashed_list_status, StatusCode::OK, "{trashed_list}");
    assert!(!trashed_list["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == project_id));

    let (second_delete_status, _, second_deleted) =
        router_request(&app, Method::DELETE, &project_uri, ADMIN_TOKEN, None).await;
    assert_eq!(second_delete_status, StatusCode::OK, "{second_deleted}");
    assert_eq!(second_deleted["data"], deleted["data"]);

    let (duplicate_status, _, duplicate) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect",
            "demoTemplate": "empty"
        })),
    )
    .await;
    assert_eq!(duplicate_status, StatusCode::CONFLICT, "{duplicate}");
    assert_eq!(duplicate["error"]["code"], "conflict");

    let restore_uri = format!("{project_uri}/restore");
    let (restore_status, _, restored) =
        router_request(&app, Method::POST, &restore_uri, ADMIN_TOKEN, None).await;
    assert_eq!(restore_status, StatusCode::OK, "{restored}");
    assert_eq!(restored["data"]["id"], project_id);
    assert_eq!(restored["data"]["name"], renamed);
    assert!(active_dir.join("project.json").is_file());
    assert!(!trash_dir.exists());

    let (second_restore_status, _, second_restored) =
        router_request(&app, Method::POST, &restore_uri, ADMIN_TOKEN, None).await;
    assert_eq!(second_restore_status, StatusCode::OK, "{second_restored}");
    assert_eq!(second_restored["data"]["id"], project_id);
    let (restored_list_status, _, restored_list) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(restored_list_status, StatusCode::OK, "{restored_list}");
    assert!(restored_list["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == project_id));

    let server_db = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let create_audit_count: i64 = server_db
        .query_row(
            "SELECT COUNT(*) FROM service_audit
             WHERE request_id = ?1 AND role = 'admin'
               AND action = 'create_project' AND project_id = ?2",
            rusqlite::params![create_request_id, project_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(create_audit_count, 1);
    let audit_text: String = server_db
        .query_row(
            "SELECT group_concat(request_id || role || action || ifnull(project_id, '') ||
                    ifnull(image_id, '') || message, '')
             FROM service_audit",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!audit_text.contains(ADMIN_TOKEN));
    for action in [
        "create_project",
        "update_project",
        "delete_project",
        "restore_project",
    ] {
        assert!(audit_text.contains(action), "missing audit action {action}");
    }

    for table in ["service_audit", "trashed_projects", "import_sessions"] {
        let exists: i64 = server_db
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(exists, 1, "missing server table {table}");
    }
}

#[test]
fn startup_reconciles_interrupted_project_lifecycle_states() {
    run_ignored_test_in_subprocess("startup_reconciles_interrupted_project_lifecycle_states_child");
}

#[tokio::test]
#[ignore]
async fn startup_reconciles_interrupted_project_lifecycle_states_child() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let cases = [
        unique_project("Task3 reconcile trashing active"),
        unique_project("Task3 reconcile trashing trash"),
        unique_project("Task3 reconcile restoring trash"),
        unique_project("Task3 reconcile restoring active"),
    ];
    let _cleanup: Vec<_> = cases
        .iter()
        .map(|(_, project_id)| RemoteProjectCleanup::new(&data_dir, project_id))
        .collect();

    for (name, project_id) in &cases {
        create_empty_project(&app, name, project_id).await;
    }

    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&connection);
    set_trash_state(&connection, &cases[0].1, "trashing");
    set_trash_state(&connection, &cases[1].1, "trashing");
    set_trash_state(&connection, &cases[2].1, "restoring");
    set_trash_state(&connection, &cases[3].1, "restoring");
    drop(connection);

    for case_index in [1_usize, 2] {
        fs::rename(
            data_dir.join("projects").join(&cases[case_index].1),
            data_dir
                .join("trash")
                .join("projects")
                .join(&cases[case_index].1),
        )
        .unwrap();
    }

    let _reconciled = build_router(config).unwrap();

    for case_index in [0_usize, 1] {
        let project_id = &cases[case_index].1;
        assert!(!data_dir.join("projects").join(project_id).exists());
        assert!(data_dir
            .join("trash")
            .join("projects")
            .join(project_id)
            .is_dir());
        assert_eq!(
            trash_state(&data_dir, project_id).as_deref(),
            Some("trashed")
        );
    }
    for case_index in [2_usize, 3] {
        let project_id = &cases[case_index].1;
        assert!(data_dir.join("projects").join(project_id).is_dir());
        assert!(!data_dir
            .join("trash")
            .join("projects")
            .join(project_id)
            .exists());
        assert_eq!(trash_state(&data_dir, project_id), None);
    }
}

#[test]
fn startup_rejects_active_and_trash_collision_without_changing_either() {
    run_ignored_test_in_subprocess(
        "startup_rejects_active_and_trash_collision_without_changing_either_child",
    );
}

#[tokio::test]
#[ignore]
async fn startup_rejects_active_and_trash_collision_without_changing_either_child() {
    let (name, project_id) = unique_project("Task3 reconcile collision");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;

    let active_dir = data_dir.join("projects").join(&project_id);
    let trash_dir = data_dir.join("trash").join("projects").join(&project_id);
    fs::create_dir(&trash_dir).unwrap();
    fs::write(trash_dir.join("collision-marker"), b"do not remove").unwrap();
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&connection);
    set_trash_state(&connection, &project_id, "trashing");
    drop(connection);

    let error = match build_router(config) {
        Ok(_) => panic!("startup should reject simultaneous active and trash directories"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "project_state_conflict");
    assert!(active_dir.join("project.json").is_file());
    assert_eq!(
        fs::read(trash_dir.join("collision-marker")).unwrap(),
        b"do not remove"
    );
    assert_eq!(
        trash_state(&data_dir, &project_id).as_deref(),
        Some("trashing")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_create_for_same_project_id_has_one_winner() {
    let (name, project_id) = unique_project("Task3 concurrent create");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let first_app = build_router(config.clone()).unwrap();
    let second_app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    let mut requests = Vec::new();

    for index in 0..12 {
        let app = if index % 2 == 0 {
            first_app.clone()
        } else {
            second_app.clone()
        };
        let name = name.clone();
        requests.push(tokio::spawn(async move {
            router_request(
                &app,
                Method::POST,
                "/api/v1/projects",
                ADMIN_TOKEN,
                Some(serde_json::json!({
                    "name": name,
                    "datasetType": "yolo-detect"
                })),
            )
            .await
            .0
        }));
    }

    let mut statuses = Vec::new();
    for request in requests {
        statuses.push(request.await.unwrap());
    }
    assert_eq!(
        statuses
            .iter()
            .filter(|status| **status == StatusCode::CREATED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert!(
        statuses
            .iter()
            .filter(|status| **status != StatusCode::CREATED)
            .all(|status| *status == StatusCode::CONFLICT),
        "{statuses:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rename_and_delete_leave_one_consistent_trashed_project() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let rename_app = build_router(config.clone()).unwrap();
    let delete_app = build_router(config).unwrap();

    for _ in 0..10 {
        let (name, project_id) = unique_project("Task3 concurrent mutation");
        let renamed = format!("{name} renamed");
        let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
        create_empty_project(&rename_app, &name, &project_id).await;
        let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
        ensure_lifecycle_test_columns(&connection);
        drop(connection);
        let project_uri = format!("/api/v1/projects/{project_id}");

        let (renamed_response, deleted_response) = tokio::join!(
            router_request(
                &rename_app,
                Method::PATCH,
                &project_uri,
                ADMIN_TOKEN,
                Some(serde_json::json!({"name": renamed}))
            ),
            router_request(&delete_app, Method::DELETE, &project_uri, ADMIN_TOKEN, None)
        );

        assert!(
            matches!(renamed_response.0, StatusCode::OK | StatusCode::NOT_FOUND),
            "{}",
            renamed_response.2
        );
        assert_eq!(deleted_response.0, StatusCode::OK, "{}", deleted_response.2);
        let active_dir = data_dir.join("projects").join(&project_id);
        let trash_dir = data_dir.join("trash").join("projects").join(&project_id);
        assert!(!active_dir.exists());
        assert!(trash_dir.is_dir());
        assert_eq!(
            trash_state(&data_dir, &project_id).as_deref(),
            Some("trashed")
        );

        let manifest: ProjectManifest =
            serde_json::from_slice(&fs::read(trash_dir.join("project.json")).unwrap()).unwrap();
        let indexed_name: String = rusqlite::Connection::open(trash_dir.join("project.sqlite"))
            .unwrap()
            .query_row("SELECT name FROM projects LIMIT 1", [], |row| row.get(0))
            .unwrap();
        assert_eq!(manifest.name, indexed_name);
    }
}

#[tokio::test]
async fn mutations_record_intent_completion_failure_and_update_action() {
    let (name, project_id) = unique_project("Task3 audit intent");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&connection);
    drop(connection);

    let created = create_empty_project(&app, &name, &project_id).await;
    let create_request_id = created["requestId"].as_str().unwrap();
    let (duplicate_status, _, duplicate) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect"
        })),
    )
    .await;
    assert_eq!(duplicate_status, StatusCode::CONFLICT, "{duplicate}");
    let duplicate_request_id = duplicate["requestId"].as_str().unwrap();
    let (update_status, _, updated) = router_request(
        &app,
        Method::PATCH,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        Some(serde_json::json!({"description": "description only"})),
    )
    .await;
    assert_eq!(update_status, StatusCode::OK, "{updated}");
    let update_request_id = updated["requestId"].as_str().unwrap();

    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let create_status: String = connection
        .query_row(
            "SELECT status FROM service_audit WHERE request_id = ?1",
            [create_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let duplicate_status: String = connection
        .query_row(
            "SELECT status FROM service_audit WHERE request_id = ?1",
            [duplicate_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let (update_action, update_status): (String, String) = connection
        .query_row(
            "SELECT action, status FROM service_audit WHERE request_id = ?1",
            [update_request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(create_status, "completed");
    assert_eq!(duplicate_status, "failed");
    assert_eq!(update_action, "update_project");
    assert_eq!(update_status, "completed");
}

#[test]
fn startup_repairs_corrupt_manifest_from_sqlite_and_prefers_valid_backup() {
    run_ignored_test_in_subprocess(
        "startup_repairs_corrupt_manifest_from_sqlite_and_prefers_valid_backup_child",
    );
}

#[tokio::test]
#[ignore]
async fn startup_repairs_corrupt_manifest_from_sqlite_and_prefers_valid_backup_child() {
    let (sqlite_name, sqlite_project_id) = unique_project("Task3 sqlite manifest recovery");
    let (backup_name, backup_project_id) = unique_project("Task3 backup manifest recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _sqlite_cleanup = RemoteProjectCleanup::new(&data_dir, &sqlite_project_id);
    let _backup_cleanup = RemoteProjectCleanup::new(&data_dir, &backup_project_id);
    create_empty_project(&app, &sqlite_name, &sqlite_project_id).await;
    create_empty_project(&app, &backup_name, &backup_project_id).await;

    let sqlite_project_dir = data_dir.join("projects").join(&sqlite_project_id);
    let sqlite_manifest_path = sqlite_project_dir.join("project.json");
    fs::write(&sqlite_manifest_path, b"{ interrupted manifest").unwrap();

    let backup_project_dir = data_dir.join("projects").join(&backup_project_id);
    let backup_manifest_path = backup_project_dir.join("project.json");
    let backup_path = backup_manifest_path.with_extension("json.bak");
    fs::rename(&backup_manifest_path, &backup_path).unwrap();
    fs::write(&backup_manifest_path, b"{ interrupted replacement").unwrap();
    rusqlite::Connection::open(backup_project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE projects SET name = 'sqlite fallback should not win' WHERE id = ?1",
            [&backup_project_id],
        )
        .unwrap();

    let repaired_app = build_router(config).unwrap();
    for (project_id, expected_name) in [
        (&sqlite_project_id, &sqlite_name),
        (&backup_project_id, &backup_name),
    ] {
        let (status, _, project) = router_request(
            &repaired_app,
            Method::GET,
            &format!("/api/v1/projects/{project_id}"),
            READER_TOKEN,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{project}");
        assert_eq!(project["data"]["name"], expected_name.as_str());
        let manifest: ProjectManifest = serde_json::from_slice(
            &fs::read(
                data_dir
                    .join("projects")
                    .join(project_id)
                    .join("project.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.name, expected_name.as_str());
    }
    assert!(!backup_path.exists());
}

#[tokio::test]
async fn project_mutations_reject_existing_directory_links_outside_configured_roots() {
    let (_, project_id) = unique_project("Task3 escaped path");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let active_dir = data_dir.join("projects").join(&project_id);
    let trash_dir = data_dir.join("trash").join("projects").join(&project_id);
    let external_dir = std::env::temp_dir().join(format!(
        "image-annotation-external-project-{}-{}",
        std::process::id(),
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("sentinel"), b"external data").unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let _link_cleanup = RemoveLinksOnDrop(vec![active_dir.clone(), trash_dir.clone()]);
    match create_directory_link(&external_dir, &active_dir) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create test directory link: {error}"),
    }

    let (status, _, response) = router_request(
        &app,
        Method::DELETE,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        None,
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{response}");
    assert_eq!(response["error"]["code"], "storage");
    assert!(fs::symlink_metadata(&active_dir).is_ok());
    assert!(fs::symlink_metadata(&trash_dir).is_err());
    assert_eq!(
        fs::read(external_dir.join("sentinel")).unwrap(),
        b"external data"
    );
}

#[test]
fn server_database_uses_wal_after_initialization() {
    let config = test_config(Ipv4Addr::LOCALHOST);
    let data_dir = config.data_dir.clone();
    let _app = build_router(config).unwrap();

    let mode: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode.to_ascii_lowercase(), "wal");
}

#[tokio::test]
async fn project_routes_reject_invalid_input_and_enforce_admin_mutations() {
    let (name, _) = unique_project("Task3 boundary");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let app = build_router(config).unwrap();

    let (reader_create_status, _, reader_create) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        READER_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "yolo-detect"
        })),
    )
    .await;
    assert_eq!(
        reader_create_status,
        StatusCode::FORBIDDEN,
        "{reader_create}"
    );

    for (method, uri, body) in [
        (
            Method::PATCH,
            "/api/v1/projects/missing-project",
            Some(serde_json::json!({"name": "renamed"})),
        ),
        (Method::DELETE, "/api/v1/projects/missing-project", None),
        (
            Method::POST,
            "/api/v1/projects/missing-project/restore",
            None,
        ),
    ] {
        let (status, _, response) = router_request(&app, method, uri, EDITOR_TOKEN, body).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{response}");
        assert_eq!(response["error"]["code"], "forbidden");
    }

    let (invalid_type_status, _, invalid_type) = router_request(
        &app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": "unsupported"
        })),
    )
    .await;
    assert_eq!(
        invalid_type_status,
        StatusCode::BAD_REQUEST,
        "{invalid_type}"
    );
    assert_eq!(invalid_type["error"]["code"], "validation");

    let (traversal_status, _, traversal) = router_request(
        &app,
        Method::DELETE,
        "/api/v1/projects/%2E%2E%2Foutside",
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(traversal_status, StatusCode::BAD_REQUEST, "{traversal}");
    assert_eq!(traversal["error"]["code"], "validation");
}

#[tokio::test]
async fn corrupt_workspace_manifest_never_falls_back_outside_the_configured_root() {
    let (workspace_name, project_id) = unique_project("Task3 workspace only");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let workspace_dir = data_dir.join("projects").join(&project_id);
    let workspace_manifest_path = workspace_dir.join("project.json");
    let _workspace_guard = RemoveDirectoryOnDrop(workspace_dir.clone());
    fs::create_dir_all(&workspace_dir).unwrap();
    fs::write(
        &workspace_manifest_path,
        serde_json::to_vec_pretty(&fixture_manifest(
            &project_id,
            &workspace_name,
            &workspace_dir,
        ))
        .unwrap(),
    )
    .unwrap();

    let project_uri = format!("/api/v1/projects/{project_id}");
    let (workspace_status, _, workspace_project) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(workspace_status, StatusCode::OK, "{workspace_project}");
    assert_eq!(workspace_project["data"]["name"], workspace_name);

    fs::write(&workspace_manifest_path, b"{ invalid workspace manifest").unwrap();

    let (corrupt_status, _, corrupt_project) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(
        corrupt_status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{corrupt_project}"
    );
    assert_eq!(corrupt_project["error"]["code"], "storage");
    let corrupt_response = corrupt_project.to_string();
    assert!(!corrupt_response.contains(&workspace_dir.to_string_lossy().to_string()));

    let (list_status, _, listed) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(list_status, StatusCode::OK, "{listed}");
    assert!(!listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == project_id));
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

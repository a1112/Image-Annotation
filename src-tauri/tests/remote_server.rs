use std::{
    collections::HashMap,
    fs,
    future::Future,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
    pin::Pin,
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Barrier, Mutex, OnceLock, Weak,
    },
    task::{Context, Poll},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
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
        build_router as build_remote_router,
        build_router_with_private_routes as build_remote_router_with_private_routes,
        shutdown_signal, with_upload_body_limit, PrivateRouteGroups, Role, ServerBuildError,
        ServerConfig,
    },
    storage::{self, StoredImage},
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tower::{Service, ServiceExt};

const READER_TOKEN: &str = "reader-token-0123456789abcdef0123456789abcdef";
const EDITOR_TOKEN: &str = "editor-token-0123456789abcdef0123456789abcdef";
const ADMIN_TOKEN: &str = "admin-token-0123456789abcdef0123456789abcdef";
const SHARED_TOKEN: &str = "shared-token-0123456789abcdef0123456789abcdef";
const HELP_READER_SECRET: &str = "help-reader-0123456789abcdef0123456789abcdef";
const HELP_EDITOR_SECRET: &str = "help-editor-0123456789abcdef0123456789abcdef";
const HELP_ADMIN_SECRET: &str = "help-admin-0123456789abcdef0123456789abcdef";
const CONCURRENT_DATA_DIR_ENV: &str = "IMAGE_ANNOTATION_CONCURRENT_TEST_DATA_DIR";
const ISOLATED_DATA_DIR_ENV: &str = "IMAGE_ANNOTATION_ISOLATED_TEST_DATA_DIR";
const LEASE_DATA_DIR_ENV: &str = "IMAGE_ANNOTATION_LEASE_TEST_DATA_DIR";
static PROJECT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static TEST_DATA_ROOT_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static TEST_SCHEMA_MIGRATION: Mutex<()> = Mutex::new(());
static TEST_DATA_ROOT_GUARDS: OnceLock<Mutex<HashMap<PathBuf, Weak<TestDataRoot>>>> =
    OnceLock::new();

#[derive(Debug, Clone)]
struct TestDataRootGuard(Arc<TestDataRoot>);

#[derive(Debug)]
struct TestDataRoot {
    path: PathBuf,
}

impl Drop for TestDataRoot {
    fn drop(&mut self) {
        if let Err(error) = remove_test_data_root(&self.path) {
            if thread::panicking() {
                eprintln!("failed to remove test data root {:?}: {error}", self.path);
            } else {
                panic!("failed to remove test data root {:?}: {error}", self.path);
            }
        }
    }
}

impl TestDataRootGuard {
    fn new(path: PathBuf) -> Self {
        assert!(path.is_absolute(), "test data root must be absolute");
        fs::create_dir_all(&path).unwrap();
        let registry = TEST_DATA_ROOT_GUARDS.get_or_init(|| Mutex::new(HashMap::new()));
        let mut roots = registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(root) = roots.get(&path).and_then(Weak::upgrade) {
            return Self(root);
        }
        let root = Arc::new(TestDataRoot { path: path.clone() });
        roots.insert(path, Arc::downgrade(&root));
        Self(root)
    }

    fn path(&self) -> &Path {
        &self.0.path
    }
}

#[derive(Debug, Clone)]
struct TestServerConfig {
    config: ServerConfig,
    data_root: TestDataRootGuard,
}

impl Deref for TestServerConfig {
    type Target = ServerConfig;

    fn deref(&self) -> &Self::Target {
        &self.config
    }
}

impl DerefMut for TestServerConfig {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.config
    }
}

struct TestRouter {
    router: Option<Router>,
    data_root: Option<TestDataRootGuard>,
}

impl Clone for TestRouter {
    fn clone(&self) -> Self {
        Self {
            router: self.router.clone(),
            data_root: self.data_root.clone(),
        }
    }
}

impl Deref for TestRouter {
    type Target = Router;

    fn deref(&self) -> &Self::Target {
        self.router
            .as_ref()
            .expect("test router was already dropped")
    }
}

impl Service<Request<Body>> for TestRouter {
    type Response = <Router as Service<Request<Body>>>::Response;
    type Error = <Router as Service<Request<Body>>>::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        <Router as Service<Request<Body>>>::poll_ready(
            self.router
                .as_mut()
                .expect("test router was already dropped"),
            context,
        )
    }

    fn call(&mut self, request: Request<Body>) -> Self::Future {
        let future = <Router as Service<Request<Body>>>::call(
            self.router
                .as_mut()
                .expect("test router was already dropped"),
            request,
        );
        let data_root = self
            .data_root
            .as_ref()
            .expect("test data root was already dropped")
            .clone();
        Box::pin(async move {
            let response = future.await;
            drop(data_root);
            response
        })
    }
}

impl Drop for TestRouter {
    fn drop(&mut self) {
        drop(self.router.take());
        drop(self.data_root.take());
    }
}

#[derive(Debug)]
struct TestServerBuildError {
    error: ServerBuildError,
    _data_root: TestDataRootGuard,
}

impl Deref for TestServerBuildError {
    type Target = ServerBuildError;

    fn deref(&self) -> &Self::Target {
        &self.error
    }
}

fn build_router(config: TestServerConfig) -> Result<TestRouter, TestServerBuildError> {
    let TestServerConfig { config, data_root } = config;
    match build_remote_router(config) {
        Ok(router) => Ok(TestRouter {
            router: Some(router),
            data_root: Some(data_root),
        }),
        Err(error) => Err(TestServerBuildError {
            error,
            _data_root: data_root,
        }),
    }
}

fn build_router_with_private_routes(
    config: TestServerConfig,
    groups: PrivateRouteGroups,
) -> Result<TestRouter, TestServerBuildError> {
    let TestServerConfig { config, data_root } = config;
    match build_remote_router_with_private_routes(config, groups) {
        Ok(router) => Ok(TestRouter {
            router: Some(router),
            data_root: Some(data_root),
        }),
        Err(error) => Err(TestServerBuildError {
            error,
            _data_root: data_root,
        }),
    }
}

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

fn remove_test_data_root(data_dir: &Path) -> std::io::Result<()> {
    let started = Instant::now();
    loop {
        match fs::remove_dir_all(data_dir) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(_) if started.elapsed() < Duration::from_secs(3) => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn run_ignored_test_in_subprocess(test_name: &str) {
    let data_dir = unique_temp_root("image-annotation-isolated-child");
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test_name, "--ignored", "--nocapture"])
        .env(ISOLATED_DATA_DIR_ENV, &data_dir)
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
fn representative_router_root_is_removed_before_child_returns() {
    let data_dir = unique_temp_root("image-annotation-explicit-root-cleanup");
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "representative_router_root_cleanup_child",
            "--ignored",
            "--nocapture",
        ])
        .env(ISOLATED_DATA_DIR_ENV, &data_dir)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "{rendered}");
    assert!(
        !data_dir.exists(),
        "child must remove its complete test root before returning: {data_dir:?}"
    );
}

#[tokio::test]
#[ignore]
async fn representative_router_root_cleanup_child() {
    let (name, project_id) = unique_project("Task4 explicit root cleanup");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let content_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/content");
    let thumbnail_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");
    let (content_status, _, content) =
        router_raw_request(&app, Method::GET, &content_uri, READER_TOKEN, &[]).await;
    assert_eq!(content_status, StatusCode::OK);
    assert!(!content.is_empty());
    let (thumbnail_status, _, thumbnail) =
        router_raw_request(&app, Method::GET, &thumbnail_uri, READER_TOKEN, &[]).await;
    assert_eq!(thumbnail_status, StatusCode::OK);
    assert!(!thumbnail.is_empty());
    let sqlite = rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(&project_id)
            .join("project.sqlite"),
    )
    .unwrap();
    let image_count: u64 = sqlite
        .query_row("SELECT COUNT(*) FROM images", [], |row| row.get(0))
        .unwrap();
    assert!(image_count > 0);

    drop(sqlite);
    drop(app);
    drop(config);
    assert!(
        !data_dir.exists(),
        "RAII guard must remove the root after Router and SQLite handles close"
    );
}

fn unique_project(prefix: &str) -> (String, String) {
    let sequence = PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!("{prefix} {} {sequence}", std::process::id());
    let id = name.to_ascii_lowercase().replace(' ', "-");
    (name, id)
}

fn unique_temp_root(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}-{}-{}",
        std::process::id(),
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
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
    for (table, column, definition) in [
        ("service_audit", "operation_id", "TEXT"),
        (
            "service_audit",
            "state",
            "TEXT NOT NULL DEFAULT 'completed'",
        ),
        ("service_audit", "payload", "TEXT NOT NULL DEFAULT '{}'"),
        ("service_audit", "updated_at", "TEXT NOT NULL DEFAULT ''"),
        ("trashed_projects", "operation_id", "TEXT"),
    ] {
        if !sqlite_column_exists(connection, table, column) {
            connection
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )
                .unwrap();
        }
    }
    connection
        .execute(
            "UPDATE service_audit
             SET operation_id = 'legacy-' || id
             WHERE operation_id IS NULL",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE service_audit SET state = status
             WHERE state = 'completed' AND status <> 'completed'",
            [],
        )
        .unwrap();
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

fn insert_pending_operation(
    connection: &rusqlite::Connection,
    project_id: &str,
    action: &str,
) -> String {
    ensure_lifecycle_test_columns(connection);
    let operation_id = format!(
        "fixture-operation-{}-{}",
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        project_id
    );
    let payload = serde_json::json!({"projectId": project_id}).to_string();
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                operation_id, request_id, role, action, project_id, image_id,
                message, status, state, payload, created_at, updated_at
            )
            VALUES (
                ?1, ?2, 'admin', ?3, ?4, NULL,
                'fixture pending operation', 'pending', 'pending', ?5,
                'fixture-created-at', 'fixture-created-at'
            )
            "#,
            rusqlite::params![
                operation_id,
                format!("fixture-request-{project_id}"),
                action,
                project_id,
                payload
            ],
        )
        .unwrap();
    operation_id
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SampleMetadataFixture {
    split: String,
    status: String,
    qa_status: String,
    review_note: Option<String>,
}

fn read_sample_metadata(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
) -> SampleMetadataFixture {
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .query_row(
        "SELECT split, status, qa_status, review_note FROM images WHERE id = ?1",
        [image_id],
        |row| {
            Ok(SampleMetadataFixture {
                split: row.get(0)?,
                status: row.get(1)?,
                qa_status: row.get(2)?,
                review_note: row.get(3)?,
            })
        },
    )
    .unwrap()
}

fn write_sample_metadata(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
    metadata: &SampleMetadataFixture,
) {
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .execute(
        "UPDATE images
         SET split = ?2, status = ?3, qa_status = ?4, review_note = ?5
         WHERE id = ?1",
        rusqlite::params![
            image_id,
            metadata.split,
            metadata.status,
            metadata.qa_status,
            metadata.review_note,
        ],
    )
    .unwrap();
}

fn sample_mutation_payload(
    project_id: &str,
    image_id: &str,
    before: &SampleMetadataFixture,
    after: &SampleMetadataFixture,
) -> Value {
    serde_json::json!({
        "projectId": project_id,
        "imageId": image_id,
        "before": before,
        "patch": {
            "split": after.split,
            "status": after.status,
            "qaStatus": after.qa_status,
            "reviewNote": after.review_note,
        },
        "after": after,
    })
}

fn insert_pending_sample_operation(
    connection: &rusqlite::Connection,
    record_project_id: &str,
    record_image_id: &str,
    payload: &Value,
) -> String {
    ensure_lifecycle_test_columns(connection);
    let sequence = PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let operation_id = format!("fixture-sample-operation-{sequence}");
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                operation_id, request_id, role, action, project_id, image_id,
                message, status, state, payload, created_at, updated_at
            )
            VALUES (
                ?1, ?2, 'editor', 'update_sample_metadata', ?3, ?4,
                'fixture pending sample mutation', 'pending', 'pending', ?5,
                'fixture-created-at', 'fixture-created-at'
            )
            "#,
            rusqlite::params![
                operation_id,
                format!("fixture-sample-request-{sequence}"),
                record_project_id,
                record_image_id,
                payload.to_string(),
            ],
        )
        .unwrap();
    operation_id
}

fn insert_project_sample_update_event(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
    operation_id: &str,
) {
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .execute(
        "INSERT INTO audit_events (id, action, image_id, message, created_at)
         VALUES (?1, 'sample.update', ?2, 'fixture committed sample update', 'fixture-created-at')",
        rusqlite::params![operation_id, image_id],
    )
    .unwrap();
}

fn insert_project_sample_rollback_event(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
    operation_id: &str,
) {
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .execute(
        "INSERT INTO audit_events (id, action, image_id, message, created_at)
         VALUES (?1, 'sample.update.rollback', ?2, 'fixture compensated sample update', 'fixture-created-at')",
        rusqlite::params![format!("{operation_id}:rollback"), image_id],
    )
    .unwrap();
}

fn project_sample_operation_event_count(
    data_dir: &Path,
    project_id: &str,
    operation_id: &str,
) -> u64 {
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .query_row(
        "SELECT COUNT(*) FROM audit_events WHERE id IN (?1, ?2)",
        rusqlite::params![operation_id, format!("{operation_id}:rollback")],
        |row| row.get(0),
    )
    .unwrap()
}

fn sample_operation_state(data_dir: &Path, operation_id: &str) -> (String, String) {
    rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state, message FROM service_audit WHERE operation_id = ?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn insert_legacy_pending_create_operation(
    connection: &rusqlite::Connection,
    project_id: &str,
    name: &str,
) -> String {
    ensure_lifecycle_test_columns(connection);
    let operation_id = format!(
        "legacy-create-operation-{}-{project_id}",
        PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let payload = serde_json::json!({
        "projectId": project_id,
        "name": name,
        "datasetType": "yolo-detect",
        "demoTemplate": "empty"
    })
    .to_string();
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                operation_id, request_id, role, action, project_id, image_id,
                message, status, state, payload, created_at, updated_at
            )
            VALUES (
                ?1, ?2, 'admin', 'create_project', ?3, NULL,
                'legacy pending create operation', 'pending', 'pending', ?4,
                'legacy-created-at', 'legacy-created-at'
            )
            "#,
            rusqlite::params![
                operation_id,
                format!("legacy-create-request-{project_id}"),
                project_id,
                payload
            ],
        )
        .unwrap();
    operation_id
}

fn set_trash_state(
    connection: &rusqlite::Connection,
    project_id: &str,
    state: &str,
    operation_id: Option<&str>,
) {
    ensure_lifecycle_test_columns(connection);
    connection
        .execute(
            r#"
            INSERT INTO trashed_projects (project_id, trashed_at, state, operation_id)
            VALUES (?1, 'interrupted-at', ?2, ?3)
            ON CONFLICT(project_id) DO UPDATE SET
                state = excluded.state,
                operation_id = excluded.operation_id
            "#,
            rusqlite::params![project_id, state, operation_id],
        )
        .unwrap();
}

fn operation_state(data_dir: &Path, operation_id: &str) -> Option<String> {
    rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state FROM service_audit WHERE operation_id = ?1",
            [operation_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap()
}

fn install_completion_failure_trigger(data_dir: &Path, trigger_name: &str, action: &str) {
    install_audit_state_failure_trigger(data_dir, trigger_name, action, "completed");
}

fn install_audit_state_failure_trigger(
    data_dir: &Path,
    trigger_name: &str,
    action: &str,
    state: &str,
) {
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&connection);
    connection
        .execute_batch(&format!(
            r#"
            CREATE TRIGGER {trigger_name}
            BEFORE UPDATE ON service_audit
            WHEN OLD.action = '{action}' AND NEW.status = '{state}'
            BEGIN
                SELECT RAISE(ABORT, 'injected audit state failure');
            END;
            "#
        ))
        .unwrap();
}

fn drop_test_trigger(data_dir: &Path, trigger_name: &str) {
    rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .execute_batch(&format!("DROP TRIGGER IF EXISTS {trigger_name};"))
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

#[cfg(unix)]
fn create_file_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(windows)]
fn create_file_link(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, link)
}

fn test_config(bind_ip: Ipv4Addr) -> TestServerConfig {
    let data_dir = if let Some(data_dir) = std::env::var_os(ISOLATED_DATA_DIR_ENV) {
        PathBuf::from(data_dir)
    } else {
        let sequence = TEST_DATA_ROOT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "image-annotation-remote-server-{}-{sequence}",
            std::process::id()
        ))
    };
    let data_root = TestDataRootGuard::new(data_dir.clone());
    TestServerConfig {
        config: ServerConfig {
            bind: SocketAddr::new(IpAddr::V4(bind_ip), 17311),
            data_dir,
            reader_token: None,
            editor_token: None,
            admin_token: None,
            allowed_origins: Vec::new(),
            max_upload_bytes: 2 * 1024 * 1024 * 1024,
        },
        data_root,
    }
}

#[test]
fn ordinary_test_configs_use_distinct_data_roots_and_clones_share_one() {
    let first = test_config(Ipv4Addr::LOCALHOST);
    let first_clone = first.clone();
    let second = test_config(Ipv4Addr::LOCALHOST);
    let first_root = first.data_dir.clone();
    let second_root = second.data_dir.clone();

    assert_eq!(first.data_dir, first_clone.data_dir);
    assert_ne!(first.data_dir, second.data_dir);
    assert_eq!(first.data_root.path(), first.data_dir);
    assert_eq!(second.data_root.path(), second.data_dir);
    drop(first);
    assert!(first_root.exists(), "clone must retain the shared root");
    drop(first_clone);
    assert!(!first_root.exists(), "last clone must remove the root");
    drop(second);
    assert!(!second_root.exists(), "independent root must be removed");
}

async fn request(
    config: TestServerConfig,
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
    app: &TestRouter,
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

async fn router_json_request_with_headers(
    app: &TestRouter,
    method: Method,
    uri: &str,
    bearer: &str,
    request_headers: &[(&str, &str)],
    json_body: Value,
) -> (StatusCode, axum::http::HeaderMap, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
        .header(header::CONTENT_TYPE, "application/json");
    for (name, value) in request_headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&body).expect("response should be valid JSON");
    (status, headers, json)
}

async fn router_raw_request(
    app: &TestRouter,
    method: Method,
    uri: &str,
    bearer: &str,
    request_headers: &[(&str, &str)],
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::AUTHORIZATION, format!("Bearer {bearer}"));
    for (name, value) in request_headers {
        request = request.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
}

async fn create_empty_project(app: &TestRouter, name: &str, project_id: &str) -> Value {
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

async fn create_demo_project(
    app: &TestRouter,
    name: &str,
    project_id: &str,
    dataset_type: &str,
    demo_template: &str,
) -> Value {
    let (status, _, project) = router_request(
        app,
        Method::POST,
        "/api/v1/projects",
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": name,
            "datasetType": dataset_type,
            "demoTemplate": demo_template
        })),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    assert_eq!(project["data"]["id"], project_id);
    project
}

fn seed_remote_sample_fixture(data_dir: &Path, project_id: &str) {
    let connection = rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(project_id)
            .join("project.sqlite"),
    )
    .unwrap();
    connection
        .execute(
            "UPDATE images SET split = 'train', status = '已标注', qa_status = '待质检' WHERE id = 'demo_001'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE images SET split = 'val', status = '草稿', qa_status = '' WHERE id = 'demo_002'",
            [],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE images SET split = 'train', status = '已标注', qa_status = '待质检' WHERE id = 'demo_003'",
            [],
        )
        .unwrap();
    for (image_id, objects) in [
        (
            "demo_001",
            serde_json::json!([
                {
                    "id": "bbox-1",
                    "classId": 0,
                    "label": "object",
                    "type": "bbox",
                    "bbox": {"x": 10.0, "y": 12.0, "width": 30.0, "height": 24.0},
                    "attributes": {}
                },
                {
                    "id": "bbox-2",
                    "classId": 1,
                    "label": "region",
                    "type": "bbox",
                    "bbox": {"x": 20.0, "y": 22.0, "width": 40.0, "height": 34.0},
                    "attributes": {}
                }
            ]),
        ),
        (
            "demo_002",
            serde_json::json!([
                {
                    "id": "bbox-3",
                    "classId": 0,
                    "label": "object",
                    "type": "bbox",
                    "bbox": {"x": 8.0, "y": 9.0, "width": 18.0, "height": 19.0},
                    "attributes": {}
                }
            ]),
        ),
        (
            "demo_003",
            serde_json::json!([
                {
                    "id": "polygon-1",
                    "classId": 1,
                    "label": "region",
                    "type": "polygon",
                    "polygon": [
                        {"x": 1.0, "y": 1.0},
                        {"x": 11.0, "y": 1.0},
                        {"x": 11.0, "y": 11.0}
                    ],
                    "attributes": {}
                }
            ]),
        ),
    ] {
        connection
            .execute(
                "INSERT INTO annotations (id, image_id, revision, object_json, updated_at)
                 VALUES (?1, ?1, 'fixture-revision', ?2, 'fixture-updated-at')",
                rusqlite::params![image_id, objects.to_string()],
            )
            .unwrap();
    }
}

fn clear_annotation_fixture(data_dir: &Path, project_id: &str, image_id: &str) {
    let project_dir = data_dir.join("projects").join(project_id);
    let connection = rusqlite::Connection::open(project_dir.join("project.sqlite")).unwrap();
    connection
        .execute("DELETE FROM annotations WHERE image_id = ?1", [image_id])
        .unwrap();
    connection
        .execute(
            "DELETE FROM annotation_versions WHERE image_id = ?1",
            [image_id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE images SET status = '未标注', qa_status = '', review_note = NULL WHERE id = ?1",
            [image_id],
        )
        .unwrap();
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join(format!("{image_id}.json"));
    let _ = fs::remove_file(managed_path);
}

fn rewrite_project_format(data_dir: &Path, project_id: &str, format: &str) {
    let project_dir = data_dir.join("projects").join(project_id);
    let manifest_path = project_dir.join("project.json");
    let mut manifest: Value = serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest["format"] = Value::String(format.to_string());
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE projects SET format = ?1 WHERE id = ?2",
            rusqlite::params![format, project_id],
        )
        .unwrap();
}

fn bbox_annotation_body(id: &str, class_id: u32, label: &str) -> Value {
    serde_json::json!({
        "objects": [{
            "id": id,
            "classId": class_id,
            "label": label,
            "type": "bbox",
            "bbox": {
                "x": 2.0,
                "y": 3.0,
                "width": 12.0,
                "height": 10.0
            },
            "attributes": {}
        }]
    })
}

fn polygon_annotation_body(id: &str, class_id: u32, label: &str) -> Value {
    serde_json::json!({
        "objects": [{
            "id": id,
            "classId": class_id,
            "label": label,
            "type": "polygon",
            "polygon": [
                {"x": 2.0, "y": 2.0},
                {"x": 18.0, "y": 2.0},
                {"x": 18.0, "y": 16.0},
                {"x": 2.0, "y": 16.0}
            ],
            "attributes": {}
        }]
    })
}

fn annotation_project_sqlite(data_dir: &Path, project_id: &str) -> PathBuf {
    data_dir
        .join("projects")
        .join(project_id)
        .join("project.sqlite")
}

fn insert_pending_annotation_operation(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
    action: &str,
) -> String {
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&connection);
    let sequence = PROJECT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let operation_id = format!("fixture-annotation-operation-{sequence}");
    let payload = serde_json::json!({
        "projectId": project_id,
        "imageId": image_id
    });
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                operation_id, request_id, role, action, project_id, image_id,
                message, status, state, payload, created_at, updated_at
            )
            VALUES (
                ?1, ?2, 'editor', ?3, ?4, ?5,
                'fixture pending annotation mutation', 'pending', 'pending', ?6,
                'fixture-created-at', 'fixture-created-at'
            )
            "#,
            rusqlite::params![
                operation_id,
                format!("fixture-annotation-request-{sequence}"),
                action,
                project_id,
                image_id,
                payload.to_string(),
            ],
        )
        .unwrap();
    operation_id
}

fn insert_project_annotation_event(
    data_dir: &Path,
    project_id: &str,
    image_id: &str,
    operation_id: &str,
    action: &str,
) {
    rusqlite::Connection::open(annotation_project_sqlite(data_dir, project_id))
        .unwrap()
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at)
             VALUES (?1, ?2, ?3, 'fixture annotation evidence', 'fixture-created-at')",
            rusqlite::params![operation_id, action, image_id],
        )
        .unwrap();
}

fn sha256_etag(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let encoded = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("\"sha256-{encoded}\"")
}

fn sha256_hex_fixture(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn legacy_source_version_fixture(path: &Path) -> String {
    let metadata = fs::metadata(path).unwrap();
    let modified = metadata
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{}:{modified}", metadata.len())
}

fn bmp_fixture_bytes() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(70);
    bytes.extend_from_slice(b"BM");
    bytes.extend_from_slice(&70_u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 4]);
    bytes.extend_from_slice(&54_u32.to_le_bytes());
    bytes.extend_from_slice(&40_u32.to_le_bytes());
    bytes.extend_from_slice(&2_i32.to_le_bytes());
    bytes.extend_from_slice(&2_i32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&24_u16.to_le_bytes());
    bytes.extend_from_slice(&0_u32.to_le_bytes());
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&[0; 16]);
    bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0, 0, 0]);
    bytes.extend_from_slice(&[255, 0, 0, 255, 255, 255, 0, 0]);
    bytes
}

fn webp_fixture_bytes() -> Vec<u8> {
    let pixels =
        image::RgbImage::from_raw(2, 2, vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255])
            .unwrap();
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut output, image::ImageFormat::WebP)
        .unwrap();
    output.into_inner()
}

fn png_fixture_bytes(color: [u8; 3]) -> Vec<u8> {
    let pixels = image::RgbImage::from_pixel(8, 8, image::Rgb(color));
    let mut output = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(pixels)
        .write_to(&mut output, image::ImageFormat::Png)
        .unwrap();
    output.into_inner()
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
    app: TestRouter,
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

fn dataset_multipart_body(boundary: &str, files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, bytes) in files {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"files\"; filename=\"{name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

fn zip_fixture(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let output = std::io::Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(output);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        archive.start_file(*name, options).unwrap();
        archive.write_all(bytes).unwrap();
    }
    archive.finish().unwrap().into_inner()
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
        .env(ISOLATED_DATA_DIR_ENV, &data_dir)
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
fn data_root_lease_rejects_a_second_server_process() {
    run_ignored_test_in_subprocess("data_root_lease_coordinator_child");
}

#[test]
#[ignore]
fn data_root_lease_coordinator_child() {
    let data_dir = unique_temp_root("image-annotation-data-lease");
    let _cleanup = RemoveDirectoryOnDrop(data_dir.clone());
    fs::create_dir_all(&data_dir).unwrap();
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_dir.clone();
    let app = build_router(config).unwrap();

    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "data_root_lease_contender_child",
            "--ignored",
            "--nocapture",
        ])
        .env(LEASE_DATA_DIR_ENV, &data_dir)
        .output()
        .unwrap();
    let rendered = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    drop(app);
    assert!(output.status.success(), "{rendered}");
}

#[test]
#[ignore]
fn data_root_lease_contender_child() {
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = PathBuf::from(std::env::var_os(LEASE_DATA_DIR_ENV).unwrap());

    let error = match build_router(config) {
        Ok(_) => panic!("a second process must not acquire the same data root"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "data_root_in_use");
}

#[test]
fn project_root_link_outside_data_root_is_rejected() {
    run_ignored_test_in_subprocess("project_root_link_outside_data_root_is_rejected_child");
}

#[test]
fn trash_parent_link_is_rejected_without_external_writes() {
    run_ignored_test_in_subprocess("trash_parent_link_is_rejected_without_external_writes_child");
}

#[test]
#[ignore]
fn trash_parent_link_is_rejected_without_external_writes_child() {
    let data_dir = unique_temp_root("image-annotation-linked-trash");
    let external_dir = unique_temp_root("image-annotation-linked-trash-external");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("sentinel"), b"outside").unwrap();
    let _data_cleanup = RemoveDirectoryOnDrop(data_dir.clone());
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let trash_link = data_dir.join("trash");
    let _link_cleanup = RemoveLinksOnDrop(vec![trash_link.clone()]);
    match create_directory_link(&external_dir, &trash_link) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create trash parent link: {error}"),
    }
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_dir;

    let error = match build_router(config) {
        Ok(_) => panic!("linked trash parent outside data root must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "server_initialization_failed");
    assert_eq!(fs::read(external_dir.join("sentinel")).unwrap(), b"outside");
    assert!(!external_dir.join("projects").exists());
}

#[test]
fn linked_data_root_is_rejected() {
    run_ignored_test_in_subprocess("linked_data_root_is_rejected_child");
}

#[test]
#[ignore]
fn linked_data_root_is_rejected_child() {
    let data_root_link = unique_temp_root("image-annotation-linked-data-root");
    let external_dir = unique_temp_root("image-annotation-linked-data-root-external");
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("sentinel"), b"outside").unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let _link_cleanup = RemoveLinksOnDrop(vec![data_root_link.clone()]);
    match create_directory_link(&external_dir, &data_root_link) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create data root link: {error}"),
    }
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_root_link;

    let error = match build_router(config) {
        Ok(_) => panic!("linked data root must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "server_initialization_failed");
    assert_eq!(fs::read(external_dir.join("sentinel")).unwrap(), b"outside");
    assert!(!external_dir.join("projects").exists());
    assert!(!external_dir.join("server.sqlite").exists());
}

#[test]
#[ignore]
fn project_root_link_outside_data_root_is_rejected_child() {
    let data_dir = unique_temp_root("image-annotation-linked-root");
    let external_dir = unique_temp_root("image-annotation-linked-root-external");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&external_dir).unwrap();
    fs::write(external_dir.join("sentinel"), b"outside").unwrap();
    let _data_cleanup = RemoveDirectoryOnDrop(data_dir.clone());
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let projects_link = data_dir.join("projects");
    let _link_cleanup = RemoveLinksOnDrop(vec![projects_link.clone()]);
    match create_directory_link(&external_dir, &projects_link) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create project root link: {error}"),
    }
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_dir;

    let error = match build_router(config) {
        Ok(_) => panic!("linked projects root outside data root must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "server_initialization_failed");
    assert_eq!(fs::read(external_dir.join("sentinel")).unwrap(), b"outside");
}

#[test]
fn server_database_link_outside_data_root_is_rejected() {
    run_ignored_test_in_subprocess("server_database_link_outside_data_root_is_rejected_child");
}

#[test]
#[ignore]
fn server_database_link_outside_data_root_is_rejected_child() {
    let data_dir = unique_temp_root("image-annotation-linked-server-db");
    let external_dir = unique_temp_root("image-annotation-linked-server-db-external");
    fs::create_dir_all(&data_dir).unwrap();
    fs::create_dir_all(&external_dir).unwrap();
    let external_db = external_dir.join("external.sqlite");
    rusqlite::Connection::open(&external_db)
        .unwrap()
        .execute("CREATE TABLE sentinel (value TEXT NOT NULL)", [])
        .unwrap();
    let server_db_link = data_dir.join("server.sqlite");
    let _data_cleanup = RemoveDirectoryOnDrop(data_dir.clone());
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let _link_cleanup = RemoveLinksOnDrop(vec![server_db_link.clone()]);
    match create_file_link(&external_db, &server_db_link) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create server database link: {error}"),
    }
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.data_dir = data_dir;

    let error = match build_router(config) {
        Ok(_) => panic!("linked server database outside data root must be rejected"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "server_initialization_failed");
    let external = rusqlite::Connection::open(&external_db).unwrap();
    let service_tables: i64 = external
        .query_row(
            "SELECT COUNT(*) FROM sqlite_schema
             WHERE type = 'table' AND name = 'service_audit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(service_tables, 0);
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

    assert_eq!(status, StatusCode::OK, "{body}");
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
    let lifecycle_db = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    ensure_lifecycle_test_columns(&lifecycle_db);
    let (delete_operation_id, delete_action, delete_state, delete_payload): (
        String,
        String,
        String,
        String,
    ) = lifecycle_db
        .query_row(
            r#"
            SELECT t.operation_id, a.action, a.state, a.payload
            FROM trashed_projects t
            JOIN service_audit a ON a.operation_id = t.operation_id
            WHERE t.project_id = ?1
            "#,
            [&project_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert!(!delete_operation_id.is_empty());
    assert_eq!(delete_action, "delete_project");
    assert_eq!(delete_state, "completed");
    assert_eq!(
        serde_json::from_str::<Value>(&delete_payload).unwrap()["projectId"],
        project_id
    );
    drop(lifecycle_db);

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

#[test]
fn legacy_tombstone_schema_migrates_to_a_bound_delete_operation() {
    run_ignored_test_in_subprocess(
        "legacy_tombstone_schema_migrates_to_a_bound_delete_operation_child",
    );
}

#[tokio::test]
#[ignore]
async fn legacy_tombstone_schema_migrates_to_a_bound_delete_operation_child() {
    let (name, project_id) = unique_project("Task3 legacy tombstone migration");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    let (delete_status, _, deleted) = router_request(
        &app,
        Method::DELETE,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    drop(app);

    let mut connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute_batch(
            r#"
            PRAGMA foreign_keys = OFF;
            DROP TABLE trashed_projects;
            DROP TABLE service_audit;

            CREATE TABLE service_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT NOT NULL,
                role TEXT NOT NULL,
                action TEXT NOT NULL,
                project_id TEXT,
                image_id TEXT,
                message TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'completed',
                created_at TEXT NOT NULL
            );
            CREATE INDEX idx_service_audit_request_id
                ON service_audit(request_id);
            CREATE INDEX idx_service_audit_project_id
                ON service_audit(project_id);

            CREATE TABLE trashed_projects (
                project_id TEXT PRIMARY KEY,
                trashed_at TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'trashed'
            );
            "#,
        )
        .unwrap();
    transaction
        .execute(
            "INSERT INTO trashed_projects (project_id, trashed_at, state)
             VALUES (?1, 'legacy-trashed-at', 'trashed')",
            [&project_id],
        )
        .unwrap();
    transaction.commit().unwrap();
    drop(connection);

    let migrated = build_router(config.clone()).unwrap();
    let binding: (String, String, String, String, String, String, String) =
        rusqlite::Connection::open(data_dir.join("server.sqlite"))
            .unwrap()
            .query_row(
                "SELECT tp.operation_id, audit.action, audit.project_id,
                        audit.state, audit.status, audit.payload, audit.role
                 FROM trashed_projects AS tp
                 JOIN service_audit AS audit
                   ON audit.operation_id = tp.operation_id
                 WHERE tp.project_id = ?1",
                [&project_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .unwrap();
    assert!(!binding.0.is_empty());
    assert_eq!(binding.1, "delete_project");
    assert_eq!(binding.2, project_id);
    assert_eq!(binding.3, "completed");
    assert_eq!(binding.4, "completed");
    assert_eq!(
        serde_json::from_str::<Value>(&binding.5).unwrap()["projectId"],
        project_id
    );
    assert_eq!(binding.6, "system");
    let _idempotent_restart = build_router(config).unwrap();
    drop(migrated);
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
    let operation_ids = [
        insert_pending_operation(&connection, &cases[0].1, "delete_project"),
        insert_pending_operation(&connection, &cases[1].1, "delete_project"),
        insert_pending_operation(&connection, &cases[2].1, "restore_project"),
        insert_pending_operation(&connection, &cases[3].1, "restore_project"),
    ];
    set_trash_state(
        &connection,
        &cases[0].1,
        "trashing",
        Some(&operation_ids[0]),
    );
    set_trash_state(
        &connection,
        &cases[1].1,
        "trashing",
        Some(&operation_ids[1]),
    );
    set_trash_state(
        &connection,
        &cases[2].1,
        "restoring",
        Some(&operation_ids[2]),
    );
    set_trash_state(
        &connection,
        &cases[3].1,
        "restoring",
        Some(&operation_ids[3]),
    );
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
        assert_eq!(
            operation_state(&data_dir, &operation_ids[case_index]).as_deref(),
            Some("completed")
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
        assert_eq!(
            operation_state(&data_dir, &operation_ids[case_index]).as_deref(),
            Some("completed")
        );
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
    let operation_id = insert_pending_operation(&connection, &project_id, "delete_project");
    set_trash_state(&connection, &project_id, "trashing", Some(&operation_id));
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

#[test]
fn startup_rejects_missing_or_mismatched_lifecycle_operation() {
    run_ignored_test_in_subprocess(
        "startup_rejects_missing_or_mismatched_lifecycle_operation_child",
    );
}

#[tokio::test]
#[ignore]
async fn startup_rejects_missing_or_mismatched_lifecycle_operation_child() {
    let (name, project_id) = unique_project("Task3 operation mismatch");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    let active_dir = data_dir.join("projects").join(&project_id);
    let trash_dir = data_dir.join("trash").join("projects").join(&project_id);
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let wrong_operation = insert_pending_operation(&connection, &project_id, "restore_project");
    set_trash_state(&connection, &project_id, "trashing", Some(&wrong_operation));
    drop(connection);

    let wrong_action_error = match build_router(config.clone()) {
        Ok(_) => panic!("mismatched lifecycle action must reject startup"),
        Err(error) => error,
    };
    assert_eq!(wrong_action_error.code(), "server_initialization_failed");
    assert!(active_dir.is_dir());
    assert!(!trash_dir.exists());

    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    connection
        .execute(
            "DELETE FROM trashed_projects WHERE project_id = ?1",
            [&project_id],
        )
        .unwrap();
    connection
        .execute(
            "DELETE FROM service_audit WHERE operation_id = ?1",
            [&wrong_operation],
        )
        .unwrap();
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    set_trash_state(
        &connection,
        &project_id,
        "trashing",
        Some("missing-operation"),
    );
    drop(connection);

    let missing_operation_error = match build_router(config) {
        Ok(_) => panic!("missing lifecycle operation must reject startup"),
        Err(error) => error,
    };
    assert_eq!(
        missing_operation_error.code(),
        "server_initialization_failed"
    );
    assert!(active_dir.is_dir());
    assert!(!trash_dir.exists());
}

#[test]
fn restore_rejects_tombstone_with_mismatched_operation() {
    run_ignored_test_in_subprocess("restore_rejects_tombstone_with_mismatched_operation_child");
}

#[tokio::test]
#[ignore]
async fn restore_rejects_tombstone_with_mismatched_operation_child() {
    let (name, project_id) = unique_project("Task3 runtime operation mismatch");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    let project_uri = format!("/api/v1/projects/{project_id}");
    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &project_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let wrong_operation = insert_pending_operation(&connection, &project_id, "update_project");
    connection
        .execute(
            "UPDATE trashed_projects SET operation_id = ?1 WHERE project_id = ?2",
            rusqlite::params![wrong_operation, project_id],
        )
        .unwrap();
    drop(connection);

    let (restore_status, _, restore) = router_request(
        &app,
        Method::POST,
        &format!("{project_uri}/restore"),
        ADMIN_TOKEN,
        None,
    )
    .await;

    assert_eq!(
        restore_status,
        StatusCode::INTERNAL_SERVER_ERROR,
        "{restore}"
    );
    assert_eq!(restore["error"]["code"], "storage");
    assert!(!data_dir.join("projects").join(&project_id).exists());
    assert!(data_dir
        .join("trash")
        .join("projects")
        .join(&project_id)
        .is_dir());
}

#[test]
fn pending_create_completion_is_reconciled_on_startup() {
    run_ignored_test_in_subprocess("pending_create_completion_is_reconciled_on_startup_child");
}

#[test]
fn pending_create_rejects_a_different_valid_project() {
    run_ignored_test_in_subprocess("pending_create_rejects_a_different_valid_project_child");
}

#[test]
fn markerless_pending_create_operations_fail_without_claiming_projects() {
    run_ignored_test_in_subprocess(
        "markerless_pending_create_operations_fail_without_claiming_projects_child",
    );
}

#[tokio::test]
#[ignore]
async fn markerless_pending_create_operations_fail_without_claiming_projects_child() {
    let (existing_name, existing_id) = unique_project("Task3 legacy existing create");
    let (absent_name, absent_id) = unique_project("Task3 legacy absent create");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _existing_cleanup = RemoteProjectCleanup::new(&data_dir, &existing_id);
    let _absent_cleanup = RemoteProjectCleanup::new(&data_dir, &absent_id);
    create_empty_project(&app, &existing_name, &existing_id).await;
    let existing_project_dir = data_dir.join("projects").join(&existing_id);
    let original_manifest = fs::read(existing_project_dir.join("project.json")).unwrap();

    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let existing_operation =
        insert_legacy_pending_create_operation(&connection, &existing_id, &existing_name);
    let absent_operation =
        insert_legacy_pending_create_operation(&connection, &absent_id, &absent_name);
    drop(connection);
    drop(app);

    let _restarted = build_router(config).unwrap_or_else(|error| {
        let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
        let states = [&existing_operation, &absent_operation].map(|operation_id| {
            connection
                .query_row(
                    "SELECT state, message FROM service_audit WHERE operation_id = ?1",
                    [operation_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .unwrap()
        });
        panic!("restart failed: {error:?}; operation states: {states:?}");
    });
    assert_eq!(
        operation_state(&data_dir, &existing_operation).as_deref(),
        Some("failed")
    );
    assert_eq!(
        operation_state(&data_dir, &absent_operation).as_deref(),
        Some("failed")
    );
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    for operation_id in [&existing_operation, &absent_operation] {
        let message: String = connection
            .query_row(
                "SELECT message FROM service_audit WHERE operation_id = ?1",
                [operation_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(message.contains("legacy create ownership is unknown"));
    }
    drop(connection);
    assert_eq!(
        fs::read(existing_project_dir.join("project.json")).unwrap(),
        original_manifest
    );
    let stored_name: String = rusqlite::Connection::open_with_flags(
        existing_project_dir.join("project.sqlite"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
    .query_row(
        "SELECT name FROM projects WHERE id = ?1",
        [&existing_id],
        |row| row.get(0),
    )
    .unwrap();
    assert_eq!(stored_name, existing_name);
    assert!(!data_dir.join("projects").join(absent_id).exists());
}

#[test]
fn duplicate_create_pending_operation_never_owns_the_existing_project() {
    run_ignored_test_in_subprocess(
        "duplicate_create_pending_operation_never_owns_the_existing_project_child",
    );
}

#[tokio::test]
#[ignore]
async fn duplicate_create_pending_operation_never_owns_the_existing_project_child() {
    let (name, project_id) = unique_project("Task3 duplicate create ownership");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    let created = create_empty_project(&app, &name, &project_id).await;
    let original_request_id = created["requestId"].as_str().unwrap();
    install_audit_state_failure_trigger(
        &data_dir,
        "fail_duplicate_create_finalization",
        "create_project",
        "failed",
    );

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
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let pending_state: String = connection
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [duplicate_request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(pending_state, "pending");
    drop(connection);
    drop_test_trigger(&data_dir, "fail_duplicate_create_finalization");

    let _restarted = build_router(config).unwrap();
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let original_state: String = connection
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [original_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let duplicate_state: String = connection
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [duplicate_request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(original_state, "completed");
    assert_eq!(duplicate_state, "failed");
    let manifest: ProjectManifest = serde_json::from_slice(
        &fs::read(
            data_dir
                .join("projects")
                .join(&project_id)
                .join("project.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(manifest.name, name);
}

#[tokio::test]
#[ignore]
async fn pending_create_rejects_a_different_valid_project_child() {
    let (name, project_id) = unique_project("Task3 pending create mismatch");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    install_completion_failure_trigger(
        &data_dir,
        "fail_mismatched_create_completion",
        "create_project",
    );

    let (status, _, created) = router_request(
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
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let request_id = created["requestId"].as_str().unwrap();
    let project_dir = data_dir.join("projects").join(&project_id);
    let manifest_path = project_dir.join("project.json");
    let mut manifest: ProjectManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.name = "Different valid project".to_string();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE projects SET name = ?1 WHERE id = ?2",
            rusqlite::params![manifest.name, project_id],
        )
        .unwrap();
    drop_test_trigger(&data_dir, "fail_mismatched_create_completion");

    let error = match build_router(config) {
        Ok(_) => panic!("a pending create must not complete a different valid project"),
        Err(error) => error,
    };

    assert_eq!(error.code(), "server_initialization_failed");
    assert!(project_dir.is_dir());
    let state: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "pending");
}

#[tokio::test]
#[ignore]
async fn pending_create_completion_is_reconciled_on_startup_child() {
    let (name, project_id) = unique_project("Task3 pending create");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    install_completion_failure_trigger(&data_dir, "fail_create_completion", "create_project");

    let (status, _, created) = router_request(
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
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let request_id = created["requestId"].as_str().unwrap();
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let (operation_id, pending_state, payload): (String, String, String) = connection
        .query_row(
            "SELECT operation_id, state, payload FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(pending_state, "pending");
    assert_eq!(
        serde_json::from_str::<Value>(&payload).unwrap()["projectId"],
        project_id
    );
    drop(connection);
    drop_test_trigger(&data_dir, "fail_create_completion");

    let _reconciled = build_router(config).unwrap();
    assert_eq!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("completed")
    );
}

#[test]
fn failed_create_removes_its_partial_project_before_marking_failed() {
    run_ignored_test_in_subprocess(
        "failed_create_removes_its_partial_project_before_marking_failed_child",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn failed_create_removes_its_partial_project_before_marking_failed_child() {
    let (name, project_id) = unique_project("Task3 partial create");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    let request_app = app.clone();
    let request_name = name.clone();
    let request = tokio::spawn(async move {
        router_request(
            &request_app,
            Method::POST,
            "/api/v1/projects",
            ADMIN_TOKEN,
            Some(serde_json::json!({
                "name": request_name,
                "datasetType": "yolo-detect",
                "demoTemplate": "demo-bbox"
            })),
        )
        .await
    });
    let active_dir = data_dir.join("projects").join(&project_id);
    let started = Instant::now();
    while !active_dir.is_dir() {
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "project creation never created its active directory"
        );
        thread::sleep(Duration::from_millis(1));
    }
    fs::create_dir(active_dir.join("project.json.tmp"))
        .expect("test must inject a manifest replacement failure");

    let (status, _, response) = request.await.unwrap();
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{response}");
    assert!(
        !active_dir.exists(),
        "partial active project was not cleaned"
    );
    let request_id = response["requestId"].as_str().unwrap();
    let state: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "failed");
}

#[test]
fn failed_update_rolls_back_before_marking_operation_failed() {
    run_ignored_test_in_subprocess(
        "failed_update_rolls_back_before_marking_operation_failed_child",
    );
}

#[tokio::test]
#[ignore]
async fn failed_update_rolls_back_before_marking_operation_failed_child() {
    let (name, project_id) = unique_project("Task3 compensated update");
    let renamed = format!("{name} renamed");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    install_completion_failure_trigger(&data_dir, "fail_update_completion", "update_project");

    let (status, _, response) = router_request(
        &app,
        Method::PATCH,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        Some(serde_json::json!({
            "name": renamed,
            "description": "must roll back"
        })),
    )
    .await;
    drop_test_trigger(&data_dir, "fail_update_completion");
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{response}");
    let request_id = response["requestId"].as_str().unwrap();
    let project_dir = data_dir.join("projects").join(&project_id);
    let manifest: ProjectManifest =
        serde_json::from_slice(&fs::read(project_dir.join("project.json")).unwrap()).unwrap();
    let indexed_name: String = rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .query_row(
            "SELECT name FROM projects WHERE id = ?1",
            [&project_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(manifest.name, name);
    assert_eq!(indexed_name, name);

    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let description: Option<String> = connection
        .query_row(
            "SELECT description FROM project_metadata WHERE project_id = ?1",
            [&project_id],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    let (state, payload): (String, String) = connection
        .query_row(
            "SELECT state, payload FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(description, None);
    assert_eq!(state, "failed");
    let payload: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(payload["old"]["name"], name);
    assert_eq!(payload["new"]["name"], renamed);
}

#[test]
fn name_update_with_only_audit_completion_failure_returns_success() {
    run_ignored_test_in_subprocess(
        "name_update_with_only_audit_completion_failure_returns_success_child",
    );
}

#[test]
fn lifecycle_completion_failure_returns_success_and_reconciles() {
    run_ignored_test_in_subprocess(
        "lifecycle_completion_failure_returns_success_and_reconciles_child",
    );
}

#[tokio::test]
#[ignore]
async fn lifecycle_completion_failure_returns_success_and_reconciles_child() {
    let (name, project_id) = unique_project("Task3 lifecycle completion");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    let project_uri = format!("/api/v1/projects/{project_id}");

    install_completion_failure_trigger(&data_dir, "fail_delete_completion", "delete_project");
    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &project_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    assert!(!data_dir.join("projects").join(&project_id).exists());
    assert!(data_dir
        .join("trash")
        .join("projects")
        .join(&project_id)
        .is_dir());
    assert_eq!(
        trash_state(&data_dir, &project_id).as_deref(),
        Some("trashing")
    );
    drop_test_trigger(&data_dir, "fail_delete_completion");

    let reconciled_delete = build_router(config.clone()).unwrap();
    assert_eq!(
        trash_state(&data_dir, &project_id).as_deref(),
        Some("trashed")
    );
    install_completion_failure_trigger(&data_dir, "fail_restore_completion", "restore_project");
    let (restore_status, _, restored) = router_request(
        &reconciled_delete,
        Method::POST,
        &format!("{project_uri}/restore"),
        ADMIN_TOKEN,
        None,
    )
    .await;
    assert_eq!(restore_status, StatusCode::OK, "{restored}");
    assert!(data_dir.join("projects").join(&project_id).is_dir());
    assert!(!data_dir
        .join("trash")
        .join("projects")
        .join(&project_id)
        .exists());
    assert_eq!(
        trash_state(&data_dir, &project_id).as_deref(),
        Some("restoring")
    );
    drop_test_trigger(&data_dir, "fail_restore_completion");

    let _reconciled_restore = build_router(config).unwrap();
    assert_eq!(trash_state(&data_dir, &project_id), None);
    let pending_lifecycle_operations: i64 =
        rusqlite::Connection::open(data_dir.join("server.sqlite"))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM service_audit
                 WHERE project_id = ?1
                   AND action IN ('delete_project', 'restore_project')
                   AND state = 'pending'",
                [&project_id],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(pending_lifecycle_operations, 0);
}

#[tokio::test]
#[ignore]
async fn name_update_with_only_audit_completion_failure_returns_success_child() {
    let (name, project_id) = unique_project("Task3 pending update");
    let renamed = format!("{name} renamed");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    install_completion_failure_trigger(&data_dir, "fail_name_update_completion", "update_project");

    let (status, _, updated) = router_request(
        &app,
        Method::PATCH,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        Some(serde_json::json!({"name": renamed})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["name"], renamed);
    let request_id = updated["requestId"].as_str().unwrap();
    let connection = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let operation_id: String = connection
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1 AND state = 'pending'",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    drop(connection);
    drop_test_trigger(&data_dir, "fail_name_update_completion");

    let _reconciled = build_router(config).unwrap();
    assert_eq!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("completed")
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
    let (create_state, create_operation_id, create_payload): (String, String, String) = connection
        .query_row(
            "SELECT state, operation_id, payload
             FROM service_audit WHERE request_id = ?1",
            [create_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let duplicate_state: String = connection
        .query_row(
            "SELECT state FROM service_audit WHERE request_id = ?1",
            [duplicate_request_id],
            |row| row.get(0),
        )
        .unwrap();
    let (update_action, update_state, update_payload): (String, String, String) = connection
        .query_row(
            "SELECT action, state, payload
             FROM service_audit WHERE request_id = ?1",
            [update_request_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(create_state, "completed");
    assert!(!create_operation_id.is_empty());
    assert_eq!(
        serde_json::from_str::<Value>(&create_payload).unwrap()["projectId"],
        project_id
    );
    assert_eq!(duplicate_state, "failed");
    assert_eq!(update_action, "update_project");
    assert_eq!(update_state, "completed");
    assert_eq!(
        serde_json::from_str::<Value>(&update_payload).unwrap()["new"]["description"],
        "description only"
    );
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
    let (stale_name, stale_project_id) = unique_project("Task3 stale manifest artifacts");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    let _sqlite_cleanup = RemoteProjectCleanup::new(&data_dir, &sqlite_project_id);
    let _backup_cleanup = RemoteProjectCleanup::new(&data_dir, &backup_project_id);
    let _stale_cleanup = RemoteProjectCleanup::new(&data_dir, &stale_project_id);
    create_empty_project(&app, &sqlite_name, &sqlite_project_id).await;
    create_empty_project(&app, &backup_name, &backup_project_id).await;
    create_empty_project(&app, &stale_name, &stale_project_id).await;

    let sqlite_project_dir = data_dir.join("projects").join(&sqlite_project_id);
    let sqlite_manifest_path = sqlite_project_dir.join("project.json");
    fs::write(&sqlite_manifest_path, b"{ interrupted manifest").unwrap();

    let backup_project_dir = data_dir.join("projects").join(&backup_project_id);
    let backup_manifest_path = backup_project_dir.join("project.json");
    let backup_path = backup_manifest_path.with_extension("json.bak");
    let backup_temp_path = backup_manifest_path.with_extension("json.tmp");
    fs::rename(&backup_manifest_path, &backup_path).unwrap();
    fs::write(&backup_manifest_path, b"{ interrupted replacement").unwrap();
    let mut interrupted_temp: ProjectManifest =
        serde_json::from_slice(&fs::read(&backup_path).unwrap()).unwrap();
    interrupted_temp.name = "uncommitted temporary name".to_string();
    fs::write(
        &backup_temp_path,
        serde_json::to_vec_pretty(&interrupted_temp).unwrap(),
    )
    .unwrap();
    rusqlite::Connection::open(backup_project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE projects SET name = 'sqlite fallback should not win' WHERE id = ?1",
            [&backup_project_id],
        )
        .unwrap();

    let stale_project_dir = data_dir.join("projects").join(&stale_project_id);
    let stale_manifest_path = stale_project_dir.join("project.json");
    let stale_backup_path = stale_manifest_path.with_extension("json.bak");
    let stale_temp_path = stale_manifest_path.with_extension("json.tmp");
    fs::copy(&stale_manifest_path, &stale_backup_path).unwrap();
    fs::copy(&stale_manifest_path, &stale_temp_path).unwrap();

    let repaired_app = build_router(config).unwrap();
    for (project_id, expected_name) in [
        (&sqlite_project_id, &sqlite_name),
        (&backup_project_id, &backup_name),
        (&stale_project_id, &stale_name),
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
    assert!(!backup_temp_path.exists());
    assert!(!stale_backup_path.exists());
    assert!(!stale_temp_path.exists());
}

#[test]
fn project_file_links_outside_project_directory_are_rejected() {
    run_ignored_test_in_subprocess(
        "project_file_links_outside_project_directory_are_rejected_child",
    );
}

#[test]
fn project_sqlite_sidecar_links_are_rejected_before_patch() {
    run_ignored_test_in_subprocess("project_sqlite_sidecar_links_are_rejected_before_patch_child");
}

#[tokio::test]
#[ignore]
async fn project_sqlite_sidecar_links_are_rejected_before_patch_child() {
    let (name, project_id) = unique_project("Task3 linked sqlite sidecar");
    let renamed = format!("{name} renamed");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;

    let external_dir = unique_temp_root("image-annotation-linked-sqlite-sidecar");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let sentinel = external_dir.join("sentinel");
    let sentinel_bytes = b"external sqlite sidecar sentinel".to_vec();
    fs::write(&sentinel, &sentinel_bytes).unwrap();
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar = project_dir.join("project.sqlite-journal");
    let _link_cleanup = RemoveLinksOnDrop(vec![sidecar.clone()]);
    match create_file_link(&sentinel, &sidecar) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("failed to create project SQLite sidecar link: {error}"),
    }

    let (status, _, response) = router_request(
        &app,
        Method::PATCH,
        &format!("/api/v1/projects/{project_id}"),
        ADMIN_TOKEN,
        Some(serde_json::json!({"name": renamed})),
    )
    .await;

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{response}");
    assert_eq!(response["error"]["code"], "storage");
    assert_eq!(fs::read(&sentinel).unwrap(), sentinel_bytes);
    let manifest: ProjectManifest =
        serde_json::from_slice(&fs::read(project_dir.join("project.json")).unwrap()).unwrap();
    assert_eq!(manifest.name, name);
}

#[tokio::test]
#[ignore]
async fn project_file_links_outside_project_directory_are_rejected_child() {
    let (manifest_name, manifest_project_id) = unique_project("Task3 linked manifest");
    let (sqlite_name, sqlite_project_id) = unique_project("Task3 linked sqlite");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _manifest_cleanup = RemoteProjectCleanup::new(&data_dir, &manifest_project_id);
    let _sqlite_cleanup = RemoteProjectCleanup::new(&data_dir, &sqlite_project_id);
    create_empty_project(&app, &manifest_name, &manifest_project_id).await;
    create_empty_project(&app, &sqlite_name, &sqlite_project_id).await;

    let external_dir = unique_temp_root("image-annotation-linked-project-files");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let manifest_path = data_dir
        .join("projects")
        .join(&manifest_project_id)
        .join("project.json");
    let sqlite_path = data_dir
        .join("projects")
        .join(&sqlite_project_id)
        .join("project.sqlite");
    let external_manifest = external_dir.join("project.json");
    let external_sqlite = external_dir.join("project.sqlite");
    fs::copy(&manifest_path, &external_manifest).unwrap();
    fs::copy(&sqlite_path, &external_sqlite).unwrap();
    let external_manifest_bytes = fs::read(&external_manifest).unwrap();
    let external_sqlite_bytes = fs::read(&external_sqlite).unwrap();
    fs::remove_file(&manifest_path).unwrap();
    fs::remove_file(&sqlite_path).unwrap();
    let _link_cleanup = RemoveLinksOnDrop(vec![manifest_path.clone(), sqlite_path.clone()]);

    for (target, link) in [
        (&external_manifest, &manifest_path),
        (&external_sqlite, &sqlite_path),
    ] {
        match create_file_link(target, link) {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::Unsupported
                ) =>
            {
                return;
            }
            Err(error) => panic!("failed to create project file link: {error}"),
        }
    }

    for project_id in [&manifest_project_id, &sqlite_project_id] {
        let (status, _, response) = router_request(
            &app,
            Method::GET,
            &format!("/api/v1/projects/{project_id}"),
            READER_TOKEN,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{response}");
        assert_eq!(response["error"]["code"], "storage");
        assert!(!response
            .to_string()
            .contains(&external_dir.to_string_lossy().to_string()));
    }
    let (list_status, _, listed) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(list_status, StatusCode::OK, "{listed}");
    for project_id in [&manifest_project_id, &sqlite_project_id] {
        assert!(!listed["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|project| project["id"] == project_id.as_str()));
    }
    assert_eq!(
        fs::read(&external_manifest).unwrap(),
        external_manifest_bytes
    );
    assert_eq!(fs::read(&external_sqlite).unwrap(), external_sqlite_bytes);
    assert!(!external_dir.join("project.sqlite-wal").exists());
    assert!(!external_dir.join("project.sqlite-shm").exists());
}

#[test]
fn remote_list_excludes_projects_linked_to_external_roots() {
    run_ignored_test_in_subprocess("remote_list_excludes_projects_linked_to_external_roots_child");
}

#[tokio::test]
#[ignore]
async fn remote_list_excludes_projects_linked_to_external_roots_child() {
    let (name, project_id) = unique_project("Task3 external root");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let _cleanup = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &name, &project_id).await;
    let external_dir = unique_temp_root("image-annotation-external-manifest-root");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let project_dir = data_dir.join("projects").join(&project_id);
    let manifest_path = project_dir.join("project.json");
    let mut manifest: ProjectManifest =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    manifest.source_dataset_key = "local-linked".to_string();
    manifest.root_path = external_dir.to_string_lossy().to_string();
    fs::write(
        &manifest_path,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE projects SET source_dataset_key = 'local-linked', root_path = ?1
             WHERE id = ?2",
            rusqlite::params![external_dir.to_string_lossy(), project_id],
        )
        .unwrap();

    let project_uri = format!("/api/v1/projects/{project_id}");
    let (get_status, _, detail) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(get_status, StatusCode::INTERNAL_SERVER_ERROR, "{detail}");
    assert_eq!(detail["error"]["code"], "storage");
    assert!(!detail
        .to_string()
        .contains(&external_dir.to_string_lossy().to_string()));

    let (list_status, _, listed) =
        router_request(&app, Method::GET, "/api/v1/projects", READER_TOKEN, None).await;
    assert_eq!(list_status, StatusCode::OK, "{listed}");
    assert!(!listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|project| project["id"] == project_id));
    assert!(!listed
        .to_string()
        .contains(&external_dir.to_string_lossy().to_string()));
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
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    let workspace_dir = data_dir.join("projects").join(&project_id);
    let workspace_manifest_path = workspace_dir.join("project.json");
    let _workspace_guard = RemoteProjectCleanup::new(&data_dir, &project_id);
    create_empty_project(&app, &workspace_name, &project_id).await;

    let project_uri = format!("/api/v1/projects/{project_id}");
    let (workspace_status, _, workspace_project) =
        router_request(&app, Method::GET, &project_uri, READER_TOKEN, None).await;
    assert_eq!(workspace_status, StatusCode::OK, "{workspace_project}");
    assert_eq!(workspace_project["data"]["name"], workspace_name);

    fs::write(&workspace_manifest_path, b"{ invalid workspace manifest").unwrap();
    fs::write(
        workspace_dir.join("project.sqlite"),
        b"invalid project sqlite",
    )
    .unwrap();

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

#[test]
fn sample_list_combines_filters_with_accurate_total_and_pagination() {
    run_ignored_test_in_subprocess(
        "sample_list_combines_filters_with_accurate_total_and_pagination_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_list_combines_filters_with_accurate_total_and_pagination_child() {
    let (name, project_id) = unique_project("Task4 sample list");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    seed_remote_sample_fixture(&data_dir, &project_id);
    rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(&project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .execute("DROP TABLE sample_class_links", [])
    .unwrap();

    let uri = format!(
        "/api/v1/projects/{project_id}/samples?offset=0&limit=1&split=train&status=%E5%B7%B2%E6%A0%87%E6%B3%A8&qaStatus=%E5%BE%85%E8%B4%A8%E6%A3%80&classId=1&label=region&q=demo_00"
    );
    let (status, _, response) = router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert_eq!(response["data"]["offset"], 0);
    assert_eq!(response["data"]["limit"], 1);
    assert_eq!(response["data"]["total"], 2);
    assert_eq!(response["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(response["data"]["items"][0]["id"], "demo_001");
    assert_eq!(response["data"]["items"][0]["annotationCount"], 2);
    assert_eq!(response["data"]["items"][0]["classes"][1]["id"], 1);

    let page_uri = format!("/api/v1/projects/{project_id}/samples?offset=1&limit=1");
    let (page_status, _, page) =
        router_request(&app, Method::GET, &page_uri, READER_TOKEN, None).await;
    assert_eq!(page_status, StatusCode::OK, "{page}");
    assert_eq!(page["data"]["total"], 3);
    assert_eq!(page["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["data"]["items"][0]["id"], "demo_002");
}

#[test]
fn sample_list_filters_classification_samples_by_actual_class() {
    run_ignored_test_in_subprocess(
        "sample_list_filters_classification_samples_by_actual_class_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_list_filters_classification_samples_by_actual_class_child() {
    let (name, project_id) = unique_project("Task4 classification");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let app = build_router(config).unwrap();
    create_demo_project(
        &app,
        &name,
        &project_id,
        "image-classification",
        "demo-classification",
    )
    .await;

    for query in ["classId=1", "label=region"] {
        let uri = format!("/api/v1/projects/{project_id}/samples?{query}");
        let (status, _, response) =
            router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
        assert_eq!(status, StatusCode::OK, "{response}");
        assert_eq!(response["data"]["total"], 1);
        assert_eq!(response["data"]["items"][0]["id"], "demo_002");
        assert_eq!(response["data"]["items"][0]["classes"][0]["id"], 1);
        assert_eq!(
            response["data"]["items"][0]["classes"][0]["label"],
            "region"
        );
    }
}

#[test]
fn classification_links_rebuild_after_project_index_upsert() {
    run_ignored_test_in_subprocess("classification_links_rebuild_after_project_index_upsert_child");
}

#[tokio::test]
#[ignore]
async fn classification_links_rebuild_after_project_index_upsert_child() {
    let (name, project_id) = unique_project("Task4 classification reindex");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(
        &app,
        &name,
        &project_id,
        "image-classification",
        "demo-classification",
    )
    .await;
    let initial_uri = format!("/api/v1/projects/{project_id}/samples?classId=0");
    let (initial_status, _, initial) =
        router_request(&app, Method::GET, &initial_uri, READER_TOKEN, None).await;
    assert_eq!(initial_status, StatusCode::OK, "{initial}");
    assert!(initial["data"]["total"].as_u64().unwrap() > 0);

    let project_dir = data_dir.join("projects").join(&project_id);
    let original_dir = project_dir.join("assets").join("original");
    let region_dir = original_dir.join("images").join("train").join("region");
    fs::copy(
        original_dir
            .join("images")
            .join("train")
            .join("object")
            .join("demo_001.png"),
        region_dir.join("moved.png"),
    )
    .unwrap();
    fs::copy(
        region_dir.join("demo_002.png"),
        region_dir.join("fresh.png"),
    )
    .unwrap();
    let mut manifest: ProjectManifest =
        serde_json::from_slice(&fs::read(project_dir.join("project.json")).unwrap()).unwrap();
    manifest.image_count = 2;
    let sqlite = project_dir.join("project.sqlite");
    let classes = storage::read_classes(&sqlite).unwrap();
    storage::upsert_project_index(
        &sqlite,
        &manifest,
        &[
            StoredImage {
                id: "demo_001".to_string(),
                file_name: "images/train/region/moved.png".to_string(),
                width: 640,
                height: 480,
                split: "train".to_string(),
                status: "未标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
            StoredImage {
                id: "fresh-region".to_string(),
                file_name: "images/train/region/fresh.png".to_string(),
                width: 640,
                height: 480,
                split: "train".to_string(),
                status: "未标注".to_string(),
                qa_status: String::new(),
                review_note: None,
            },
        ],
        &classes,
    )
    .unwrap();

    let region_uri = format!("/api/v1/projects/{project_id}/samples?classId=1");
    let (region_status, _, region) =
        router_request(&app, Method::GET, &region_uri, READER_TOKEN, None).await;
    assert_eq!(region_status, StatusCode::OK, "{region}");
    assert_eq!(region["data"]["total"], 2);
    let ids = region["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(ids.contains(&"demo_001"));
    assert!(ids.contains(&"fresh-region"));

    let object_uri = format!("/api/v1/projects/{project_id}/samples?classId=0");
    let (object_status, _, object) =
        router_request(&app, Method::GET, &object_uri, READER_TOKEN, None).await;
    assert_eq!(object_status, StatusCode::OK, "{object}");
    assert_eq!(object["data"]["total"], 0);
}

#[test]
fn sample_detail_and_patch_validate_fields_and_roles() {
    run_ignored_test_in_subprocess("sample_detail_and_patch_validate_fields_and_roles_child");
}

#[tokio::test]
#[ignore]
async fn sample_detail_and_patch_validate_fields_and_roles_child() {
    let (name, project_id) = unique_project("Task4 sample patch");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    seed_remote_sample_fixture(&data_dir, &project_id);
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001");

    let (detail_status, _, detail) =
        router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
    assert_eq!(detail_status, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["id"], "demo_001");
    assert_eq!(detail["data"]["fileName"], "demo_001.png");
    assert_eq!(detail["data"]["annotationRevision"], "fixture-revision");
    assert_eq!(detail["data"]["classes"][0]["label"], "object");
    assert!(!detail
        .to_string()
        .contains(&data_dir.to_string_lossy().to_string()));

    let patch = serde_json::json!({
        "split": "val",
        "status": "草稿",
        "qaStatus": "驳回",
        "reviewNote": "needs another pass"
    });
    let (reader_status, _, reader_response) =
        router_request(&app, Method::PATCH, &uri, READER_TOKEN, Some(patch.clone())).await;
    assert_eq!(reader_status, StatusCode::FORBIDDEN, "{reader_response}");

    let (editor_status, _, updated) =
        router_request(&app, Method::PATCH, &uri, EDITOR_TOKEN, Some(patch)).await;
    assert_eq!(editor_status, StatusCode::OK, "{updated}");
    assert_eq!(updated["data"]["split"], "val");
    assert_eq!(updated["data"]["status"], "草稿");
    assert_eq!(updated["data"]["qaStatus"], "驳回");
    assert_eq!(updated["data"]["reviewNote"], "needs another pass");
    assert_eq!(updated["data"]["annotationRevision"], "fixture-revision");

    let patch_request_id = updated["requestId"].as_str().unwrap();
    let server = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let (
        audit_role,
        audit_action,
        audit_project_id,
        audit_image_id,
        audit_state,
        audit_message,
        audit_payload,
    ): (String, String, String, String, String, String, String) = server
        .query_row(
            "SELECT role, action, project_id, image_id, state, message, payload
             FROM service_audit WHERE request_id = ?1",
            [patch_request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(audit_role, "editor");
    assert_eq!(audit_action, "update_sample_metadata");
    assert_eq!(audit_project_id, project_id);
    assert_eq!(audit_image_id, "demo_001");
    assert_eq!(audit_state, "completed");
    assert_eq!(audit_message, "sample metadata updated");
    let audit_payload: Value = serde_json::from_str(&audit_payload).unwrap();
    assert_eq!(audit_payload["projectId"], project_id);
    assert_eq!(audit_payload["imageId"], "demo_001");
    assert_eq!(audit_payload["patch"]["split"], "val");
    assert_eq!(audit_payload["patch"]["status"], "草稿");

    let missing_uri = format!("/api/v1/projects/{project_id}/samples/missing-sample");
    let (missing_status, _, missing) = router_request(
        &app,
        Method::PATCH,
        &missing_uri,
        EDITOR_TOKEN,
        Some(serde_json::json!({"split": "test"})),
    )
    .await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing}");
    let completed_missing: u64 = server
        .query_row(
            "SELECT COUNT(*) FROM service_audit
             WHERE request_id = ?1 AND action = 'update_sample_metadata' AND state = 'completed'",
            [missing["requestId"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(completed_missing, 0);

    for invalid in [
        serde_json::json!({"split": "production"}),
        serde_json::json!({"status": "deleted"}),
        serde_json::json!({"annotationRevision": "forbidden"}),
        serde_json::json!({}),
    ] {
        let (status, _, response) =
            router_request(&app, Method::PATCH, &uri, EDITOR_TOKEN, Some(invalid)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_eq!(response["error"]["code"], "validation");
    }
}

#[test]
fn pending_sample_mutation_before_project_update_fails_on_restart() {
    run_ignored_test_in_subprocess(
        "pending_sample_mutation_before_project_update_fails_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn pending_sample_mutation_before_project_update_fails_on_restart_child() {
    let (name, project_id) = unique_project("Task4 pending sample before");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before = read_sample_metadata(&data_dir, &project_id, "demo_001");
    let after = SampleMetadataFixture {
        split: "val".to_string(),
        status: "草稿".to_string(),
        qa_status: "驳回".to_string(),
        review_note: Some("pending target".to_string()),
    };
    let payload = sample_mutation_payload(&project_id, "demo_001", &before, &after);
    let operation_id = insert_pending_sample_operation(
        &rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap(),
        &project_id,
        "demo_001",
        &payload,
    );
    drop(app);

    let _restarted = build_router(config).expect("before-update pending must not block startup");

    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        before
    );
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "failed");
    assert!(message.contains("not committed"), "{message}");
}

#[test]
fn pending_samples_without_project_evidence_preserve_unrelated_current_metadata() {
    run_ignored_test_in_subprocess(
        "pending_samples_without_project_evidence_preserve_unrelated_current_metadata_child",
    );
}

#[tokio::test]
#[ignore]
async fn pending_samples_without_project_evidence_preserve_unrelated_current_metadata_child() {
    let (name, project_id) = unique_project("Task4 sample no project evidence");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before_one = read_sample_metadata(&data_dir, &project_id, "demo_001");
    let after_one = SampleMetadataFixture {
        split: "val".to_string(),
        status: "已标注".to_string(),
        qa_status: "待质检".to_string(),
        review_note: Some("coincidental target".to_string()),
    };
    let before_two = read_sample_metadata(&data_dir, &project_id, "demo_002");
    let after_two = SampleMetadataFixture {
        split: "test".to_string(),
        status: "已标注".to_string(),
        qa_status: "通过".to_string(),
        review_note: Some("unused target".to_string()),
    };
    let unrelated_two = SampleMetadataFixture {
        split: "val".to_string(),
        status: before_two.status.clone(),
        qa_status: "驳回".to_string(),
        review_note: Some("later local edit".to_string()),
    };
    let server = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let operation_one = insert_pending_sample_operation(
        &server,
        &project_id,
        "demo_001",
        &sample_mutation_payload(&project_id, "demo_001", &before_one, &after_one),
    );
    let operation_two = insert_pending_sample_operation(
        &server,
        &project_id,
        "demo_002",
        &sample_mutation_payload(&project_id, "demo_002", &before_two, &after_two),
    );
    drop(server);
    write_sample_metadata(&data_dir, &project_id, "demo_001", &after_one);
    write_sample_metadata(&data_dir, &project_id, "demo_002", &unrelated_two);
    drop(app);

    let _restarted = build_router(config).expect("missing project evidence must not block startup");

    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        after_one
    );
    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_002"),
        unrelated_two
    );
    for operation_id in [&operation_one, &operation_two] {
        let (state, message) = sample_operation_state(&data_dir, operation_id);
        assert_eq!(state, "failed", "{operation_id}: {message}");
        assert!(message.contains("not committed"), "{message}");
        assert_eq!(
            project_sample_operation_event_count(&data_dir, &project_id, operation_id),
            0
        );
    }
}

#[test]
fn pending_sample_mutation_after_project_commit_completes_on_restart() {
    run_ignored_test_in_subprocess(
        "pending_sample_mutation_after_project_commit_completes_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn pending_sample_mutation_after_project_commit_completes_on_restart_child() {
    let (name, project_id) = unique_project("Task4 pending sample applied");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before = read_sample_metadata(&data_dir, &project_id, "demo_001");
    let after = SampleMetadataFixture {
        split: "test".to_string(),
        status: "已标注".to_string(),
        qa_status: "待质检".to_string(),
        review_note: Some("committed target".to_string()),
    };
    let payload = sample_mutation_payload(&project_id, "demo_001", &before, &after);
    let operation_id = insert_pending_sample_operation(
        &rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap(),
        &project_id,
        "demo_001",
        &payload,
    );
    write_sample_metadata(&data_dir, &project_id, "demo_001", &after);
    insert_project_sample_update_event(&data_dir, &project_id, "demo_001", &operation_id);
    drop(app);

    let _restarted = build_router(config).expect("applied pending must not block startup");

    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        after
    );
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "completed");
    assert!(message.contains("reconciled"), "{message}");
}

#[test]
fn committed_sample_mutation_preserves_later_local_edit_on_restart() {
    run_ignored_test_in_subprocess(
        "committed_sample_mutation_preserves_later_local_edit_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn committed_sample_mutation_preserves_later_local_edit_on_restart_child() {
    let (name, project_id) = unique_project("Task4 committed sample later edit");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before = read_sample_metadata(&data_dir, &project_id, "demo_001");
    let after = SampleMetadataFixture {
        split: "val".to_string(),
        status: "已标注".to_string(),
        qa_status: "待质检".to_string(),
        review_note: Some("full target".to_string()),
    };
    let later_edit = SampleMetadataFixture {
        split: "test".to_string(),
        status: "草稿".to_string(),
        qa_status: "驳回".to_string(),
        review_note: Some("local edit after remote commit".to_string()),
    };
    let payload = sample_mutation_payload(&project_id, "demo_001", &before, &after);
    let operation_id = insert_pending_sample_operation(
        &rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap(),
        &project_id,
        "demo_001",
        &payload,
    );
    insert_project_sample_update_event(&data_dir, &project_id, "demo_001", &operation_id);
    write_sample_metadata(&data_dir, &project_id, "demo_001", &later_edit);
    drop(app);

    let _restarted = build_router(config).expect("committed pending must not block startup");

    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        later_edit
    );
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "completed");
    assert!(message.contains("reconciled"), "{message}");
    assert_eq!(
        project_sample_operation_event_count(&data_dir, &project_id, &operation_id),
        1
    );
}

#[test]
fn compensated_sample_mutation_preserves_current_metadata_on_restart() {
    run_ignored_test_in_subprocess(
        "compensated_sample_mutation_preserves_current_metadata_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn compensated_sample_mutation_preserves_current_metadata_on_restart_child() {
    let (name, project_id) = unique_project("Task4 compensated sample");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before = read_sample_metadata(&data_dir, &project_id, "demo_001");
    let target = SampleMetadataFixture {
        split: "val".to_string(),
        status: "已标注".to_string(),
        qa_status: "待质检".to_string(),
        review_note: Some("compensated target".to_string()),
    };
    let current = SampleMetadataFixture {
        split: "test".to_string(),
        status: "草稿".to_string(),
        qa_status: "驳回".to_string(),
        review_note: Some("local state after compensation".to_string()),
    };
    let operation_id = insert_pending_sample_operation(
        &rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap(),
        &project_id,
        "demo_001",
        &sample_mutation_payload(&project_id, "demo_001", &before, &target),
    );
    insert_project_sample_update_event(&data_dir, &project_id, "demo_001", &operation_id);
    insert_project_sample_rollback_event(&data_dir, &project_id, "demo_001", &operation_id);
    write_sample_metadata(&data_dir, &project_id, "demo_001", &current);
    drop(app);

    let _restarted = build_router(config).expect("compensated pending must not block startup");

    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        current
    );
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "failed");
    assert!(message.contains("compensated"), "{message}");
    assert_eq!(
        project_sample_operation_event_count(&data_dir, &project_id, &operation_id),
        2
    );
}

#[test]
fn legacy_applied_sample_without_bound_evidence_becomes_indeterminate() {
    run_ignored_test_in_subprocess(
        "legacy_applied_sample_without_bound_evidence_becomes_indeterminate_child",
    );
}

#[tokio::test]
#[ignore]
async fn legacy_applied_sample_without_bound_evidence_becomes_indeterminate_child() {
    let (name, project_id) = unique_project("Task4 legacy sample evidence");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let applied = SampleMetadataFixture {
        split: "val".to_string(),
        status: "草稿".to_string(),
        qa_status: String::new(),
        review_note: Some("legacy applied metadata".to_string()),
    };
    let operation_id = insert_pending_sample_operation(
        &rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap(),
        &project_id,
        "demo_001",
        &serde_json::json!({
            "projectId": project_id,
            "imageId": "demo_001",
            "patch": {
                "split": applied.split,
                "status": applied.status,
                "reviewNote": applied.review_note
            }
        }),
    );
    write_sample_metadata(&data_dir, &project_id, "demo_001", &applied);
    drop(app);

    let restarted = build_router(config.clone()).expect("legacy pending must not block startup");
    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        applied
    );
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "indeterminate");
    assert!(message.contains("indeterminate"), "{message}");
    assert!(!message.contains("not applied"), "{message}");
    assert_eq!(
        project_sample_operation_event_count(&data_dir, &project_id, &operation_id),
        0
    );
    drop(restarted);

    let _second_restart = build_router(config).expect("indeterminate must not be reprocessed");
    let (second_state, second_message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(second_state, "indeterminate");
    assert_eq!(second_message, message);
}

#[test]
fn sample_patch_stays_applied_when_global_completion_temporarily_fails() {
    run_ignored_test_in_subprocess(
        "sample_patch_stays_applied_when_global_completion_temporarily_fails_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_patch_stays_applied_when_global_completion_temporarily_fails_child() {
    let (name, project_id) = unique_project("Task4 sample audit completion");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let server = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    server
        .execute_batch(
            r#"
            CREATE TRIGGER fail_sample_audit_completion
            BEFORE UPDATE OF state ON service_audit
            WHEN OLD.action = 'update_sample_metadata'
             AND OLD.state = 'pending'
             AND NEW.state = 'completed'
            BEGIN
                SELECT RAISE(FAIL, 'forced sample completion failure');
            END;
            "#,
        )
        .unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001");
    let (status, _, response) = router_request(
        &app,
        Method::PATCH,
        &uri,
        EDITOR_TOKEN,
        Some(serde_json::json!({
            "split": "test",
            "status": "已标注",
            "qaStatus": "待质检",
            "reviewNote": "completion retry"
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let request_id = response["requestId"].as_str().unwrap();
    let (operation_id, state): (String, String) = server
        .query_row(
            "SELECT operation_id, state FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "pending");
    let project_event: (String, String) = rusqlite::Connection::open(
        data_dir
            .join("projects")
            .join(&project_id)
            .join("project.sqlite"),
    )
    .unwrap()
    .query_row(
        "SELECT action, image_id FROM audit_events WHERE id = ?1",
        [&operation_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .unwrap();
    assert_eq!(project_event.0, "sample.update");
    assert_eq!(project_event.1, "demo_001");
    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        SampleMetadataFixture {
            split: "test".to_string(),
            status: "已标注".to_string(),
            qa_status: "待质检".to_string(),
            review_note: Some("completion retry".to_string()),
        }
    );

    server
        .execute("DROP TRIGGER fail_sample_audit_completion", [])
        .unwrap();
    drop(server);
    drop(app);
    let _restarted = build_router(config).expect("pending applied sample must reconcile");
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "completed");
    assert!(message.contains("reconciled"), "{message}");
}

#[test]
fn sample_routes_report_unknown_resources_and_invalid_filters() {
    run_ignored_test_in_subprocess(
        "sample_routes_report_unknown_resources_and_invalid_filters_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_routes_report_unknown_resources_and_invalid_filters_child() {
    let (name, project_id) = unique_project("Task4 unknown samples");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;

    for uri in [
        "/api/v1/projects/missing-project/samples".to_string(),
        format!("/api/v1/projects/{project_id}/samples/missing-sample"),
        format!("/api/v1/projects/{project_id}/samples/missing-sample/content"),
        format!("/api/v1/projects/{project_id}/samples/missing-sample/thumbnail"),
    ] {
        let (status, _, response) =
            router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{response}");
        assert_eq!(response["error"]["code"], "not_found");
    }

    for query in [
        "split=production",
        "status=deleted",
        "qaStatus=unknown",
        "classId=not-a-number",
        "limit=0",
        "limit=501",
    ] {
        let uri = format!("/api/v1/projects/{project_id}/samples?{query}");
        let (status, _, response) =
            router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{response}");
        assert_eq!(response["error"]["code"], "validation");
    }
}

#[test]
fn sample_content_supports_etag_head_and_single_byte_ranges() {
    run_ignored_test_in_subprocess(
        "sample_content_supports_etag_head_and_single_byte_ranges_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_content_supports_etag_head_and_single_byte_ranges_child() {
    let (name, project_id) = unique_project("Task4 sample content");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let project_dir = data_dir.join("projects").join(&project_id);
    let original_dir = project_dir.join("assets").join("original");
    let nested_dir = original_dir.join("nested");
    fs::create_dir(&nested_dir).unwrap();
    let indexed_name = "nested/报告 sample; 100%.png";
    fs::copy(
        original_dir
            .join("images")
            .join("train")
            .join("demo_001.png"),
        nested_dir.join("报告 sample; 100%.png"),
    )
    .unwrap();
    rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE images SET file_name = ?1 WHERE id = 'demo_001'",
            [indexed_name],
        )
        .unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/content");

    let (status, headers, bytes) =
        router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::ACCEPT_RANGES], "bytes");
    assert!(!bytes.is_empty());
    let etag = headers[header::ETAG].to_str().unwrap().to_string();
    let disposition = headers[header::CONTENT_DISPOSITION].to_str().unwrap();
    assert!(disposition.starts_with("inline; filename=\""));
    assert!(disposition.contains("filename*=UTF-8''"));
    assert!(disposition.contains("%E6%8A%A5%E5%91%8A"));
    assert!(disposition.contains("%3B"));
    assert!(disposition.contains("%25"));
    assert!(!disposition.contains("sample; 100%"));
    assert!(!disposition.contains("nested"));
    assert!(!disposition.contains('\\'));
    assert!(!disposition.contains('\r'));
    assert!(!disposition.contains('\n'));
    assert!(!disposition.contains(&data_dir.to_string_lossy().to_string()));

    let (cached_status, cached_headers, cached_body) = router_raw_request(
        &app,
        Method::GET,
        &uri,
        READER_TOKEN,
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(cached_status, StatusCode::NOT_MODIFIED);
    assert_eq!(cached_headers[header::ETAG], etag);
    assert_eq!(
        cached_headers[header::CONTENT_DISPOSITION],
        headers[header::CONTENT_DISPOSITION]
    );
    assert!(cached_body.is_empty());

    let (head_status, head_headers, head_body) =
        router_raw_request(&app, Method::HEAD, &uri, READER_TOKEN, &[]).await;
    assert_eq!(head_status, StatusCode::OK);
    assert_eq!(head_headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(head_headers[header::ETAG], etag);
    assert_eq!(
        head_headers[header::CONTENT_DISPOSITION],
        headers[header::CONTENT_DISPOSITION]
    );
    assert!(head_body.is_empty());

    let (range_status, range_headers, range_body) = router_raw_request(
        &app,
        Method::GET,
        &uri,
        READER_TOKEN,
        &[("range", "bytes=0-9")],
    )
    .await;
    assert_eq!(range_status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(range_body, bytes[..10]);
    assert_eq!(
        range_headers[header::CONTENT_RANGE],
        format!("bytes 0-9/{}", bytes.len())
    );
    assert_eq!(range_headers[header::ACCEPT_RANGES], "bytes");
    assert_eq!(
        range_headers[header::CONTENT_DISPOSITION],
        headers[header::CONTENT_DISPOSITION]
    );

    for range in ["bytes=999999-", "bytes=0-1,4-5", "items=0-1"] {
        let (range_status, range_headers, range_body) =
            router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[("range", range)]).await;
        assert_eq!(range_status, StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(
            range_headers[header::CONTENT_RANGE],
            format!("bytes */{}", bytes.len())
        );
        let error: Value = serde_json::from_slice(&range_body).unwrap();
        assert_eq!(error["error"]["code"], "range_not_satisfiable");
    }
}

#[test]
fn thumbnail_is_generated_in_project_storage_with_sha256_etag() {
    run_ignored_test_in_subprocess(
        "thumbnail_is_generated_in_project_storage_with_sha256_etag_child",
    );
}

#[tokio::test]
#[ignore]
async fn thumbnail_is_generated_in_project_storage_with_sha256_etag_child() {
    let (name, project_id) = unique_project("Task4 thumbnail");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let thumbnail_dir = data_dir
        .join("projects")
        .join(&project_id)
        .join("assets")
        .join("thumbnails");
    fs::remove_dir_all(&thumbnail_dir).unwrap();
    fs::create_dir(&thumbnail_dir).unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");

    let (status, headers, bytes) =
        router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CONTENT_TYPE], "image/jpeg");
    assert_eq!(headers[header::ETAG], sha256_etag(&bytes));
    let disposition = headers[header::CONTENT_DISPOSITION].to_str().unwrap();
    assert!(disposition.starts_with("inline; filename=\""));
    assert!(disposition.contains("demo_001-thumbnail.jpg"));
    let files = fs::read_dir(&thumbnail_dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(files.len(), 1);
    let thumbnail_path = files[0].path();
    assert_eq!(fs::read(&thumbnail_path).unwrap(), bytes);
    let (width, height) = image::image_dimensions(&thumbnail_path).unwrap();
    assert!(width <= 320 && height <= 320);

    let etag = headers[header::ETAG].to_str().unwrap().to_string();
    let (cached_status, cached_headers, cached_body) = router_raw_request(
        &app,
        Method::GET,
        &uri,
        READER_TOKEN,
        &[("if-none-match", &etag)],
    )
    .await;
    assert_eq!(cached_status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        cached_headers[header::CONTENT_DISPOSITION],
        headers[header::CONTENT_DISPOSITION]
    );
    assert!(cached_body.is_empty());
}

#[test]
fn thumbnail_cache_changes_when_source_bytes_change() {
    run_ignored_test_in_subprocess("thumbnail_cache_changes_when_source_bytes_change_child");
}

#[tokio::test]
#[ignore]
async fn thumbnail_cache_changes_when_source_bytes_change_child() {
    let (name, project_id) = unique_project("Task4 thumbnail source fingerprint");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let project_dir = data_dir.join("projects").join(&project_id);
    let thumbnail_dir = project_dir.join("assets").join("thumbnails");
    let source_path = project_dir
        .join("assets")
        .join("original")
        .join("images")
        .join("train")
        .join("demo_001.png");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");

    let (first_status, first_headers, first_bytes) =
        router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(first_status, StatusCode::OK);
    let first_etag = first_headers[header::ETAG].to_str().unwrap().to_string();

    fs::write(&source_path, png_fixture_bytes([0, 220, 30])).unwrap();
    let (second_status, second_headers, second_bytes) =
        router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(second_status, StatusCode::OK);
    let second_etag = second_headers[header::ETAG].to_str().unwrap();
    assert_ne!(second_bytes, first_bytes);
    assert_ne!(second_etag, first_etag);
    let cached_jpegs = fs::read_dir(thumbnail_dir)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .into_iter()
        .filter(|entry| entry.path().extension().is_some_and(|value| value == "jpg"))
        .count();
    assert_eq!(cached_jpegs, 1);
}

#[test]
fn thumbnail_context_cleans_only_regular_controlled_temp_files() {
    run_ignored_test_in_subprocess(
        "thumbnail_context_cleans_only_regular_controlled_temp_files_child",
    );
}

#[tokio::test]
#[ignore]
async fn thumbnail_context_cleans_only_regular_controlled_temp_files_child() {
    let (name, project_id) = unique_project("Task4 thumbnail temp cleanup");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let thumbnail_dir = data_dir
        .join("projects")
        .join(&project_id)
        .join("assets")
        .join("thumbnails");
    let stale_temp = thumbnail_dir.join(".thumbnail-stale.tmp");
    let neighboring_file = thumbnail_dir.join("neighbor.tmp");
    fs::write(&stale_temp, b"stale thumbnail temp").unwrap();
    fs::write(&neighboring_file, b"keep neighboring file").unwrap();
    let external_dir = unique_temp_root("task4-thumbnail-temp-external");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let external_target = external_dir.join("sentinel");
    fs::write(&external_target, b"external target").unwrap();
    let linked_temp = thumbnail_dir.join(".thumbnail-linked.tmp");
    let linked = create_file_link(&external_target, &linked_temp).is_ok();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");

    let (status, _, _) = router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(status, StatusCode::OK);
    assert!(!stale_temp.exists());
    assert_eq!(
        fs::read(&neighboring_file).unwrap(),
        b"keep neighboring file"
    );
    assert_eq!(fs::read(&external_target).unwrap(), b"external target");
    if linked {
        assert!(fs::symlink_metadata(&linked_temp).is_ok());
        assert_eq!(fs::read(&linked_temp).unwrap(), b"external target");
    }
}

async fn assert_thumbnail_fixture_supported(
    name_prefix: &str,
    extension: &str,
    content_type: &str,
    source: Vec<u8>,
) {
    let decoded_source = image::load_from_memory(&source).unwrap();
    assert!(decoded_source.width() > 0);
    assert!(decoded_source.height() > 0);
    let (name, project_id) = unique_project(name_prefix);
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let project_dir = data_dir.join("projects").join(&project_id);
    let file_name = format!("fixture.{extension}");
    fs::write(
        project_dir.join("assets").join("original").join(&file_name),
        &source,
    )
    .unwrap();
    rusqlite::Connection::open(project_dir.join("project.sqlite"))
        .unwrap()
        .execute(
            "UPDATE images SET file_name = ?1, width = 2, height = 2 WHERE id = 'demo_001'",
            [&file_name],
        )
        .unwrap();

    let content_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/content");
    let (content_status, content_headers, content) =
        router_raw_request(&app, Method::GET, &content_uri, READER_TOKEN, &[]).await;
    assert_eq!(content_status, StatusCode::OK);
    assert_eq!(content_headers[header::CONTENT_TYPE], content_type);
    assert_eq!(content, source);

    let thumbnail_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");
    let (thumbnail_status, thumbnail_headers, thumbnail) =
        router_raw_request(&app, Method::GET, &thumbnail_uri, READER_TOKEN, &[]).await;
    assert_eq!(thumbnail_status, StatusCode::OK);
    assert_eq!(thumbnail_headers[header::CONTENT_TYPE], "image/jpeg");
    let decoded_thumbnail = image::load_from_memory(&thumbnail).unwrap();
    assert!(decoded_thumbnail.width() > 0 && decoded_thumbnail.width() <= 320);
    assert!(decoded_thumbnail.height() > 0 && decoded_thumbnail.height() <= 320);
    assert_eq!(thumbnail_headers[header::ETAG], sha256_etag(&thumbnail));
}

#[test]
fn bmp_sample_generates_a_jpeg_thumbnail() {
    run_ignored_test_in_subprocess("bmp_sample_generates_a_jpeg_thumbnail_child");
}

#[tokio::test]
#[ignore]
async fn bmp_sample_generates_a_jpeg_thumbnail_child() {
    assert_thumbnail_fixture_supported(
        "Task4 BMP thumbnail",
        "bmp",
        "image/bmp",
        bmp_fixture_bytes(),
    )
    .await;
}

#[test]
fn webp_sample_generates_a_jpeg_thumbnail() {
    run_ignored_test_in_subprocess("webp_sample_generates_a_jpeg_thumbnail_child");
}

#[tokio::test]
#[ignore]
async fn webp_sample_generates_a_jpeg_thumbnail_child() {
    assert_thumbnail_fixture_supported(
        "Task4 WebP thumbnail",
        "webp",
        "image/webp",
        webp_fixture_bytes(),
    )
    .await;
}

#[test]
fn sample_assets_reject_database_path_escape_and_file_links() {
    run_ignored_test_in_subprocess(
        "sample_assets_reject_database_path_escape_and_file_links_child",
    );
}

#[tokio::test]
#[ignore]
async fn sample_assets_reject_database_path_escape_and_file_links_child() {
    let (name, project_id) = unique_project("Task4 asset escape");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let project_dir = data_dir.join("projects").join(&project_id);
    let sqlite = project_dir.join("project.sqlite");
    let external_dir = unique_temp_root("task4-external-assets");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let sentinel = external_dir.join("sentinel.png");
    fs::write(&sentinel, b"external sentinel").unwrap();

    rusqlite::Connection::open(&sqlite)
        .unwrap()
        .execute(
            "UPDATE images SET file_name = ?1 WHERE id = 'demo_001'",
            [sentinel.to_string_lossy().to_string()],
        )
        .unwrap();
    let detail_uri = format!("/api/v1/projects/{project_id}/samples/demo_001");
    let (detail_status, _, detail) =
        router_request(&app, Method::GET, &detail_uri, READER_TOKEN, None).await;
    assert_eq!(detail_status, StatusCode::INTERNAL_SERVER_ERROR, "{detail}");
    assert_eq!(detail["error"]["code"], "storage");
    assert!(!detail
        .to_string()
        .contains(&external_dir.to_string_lossy().to_string()));

    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/content");
    let (escape_status, _, escape_body) =
        router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
    assert_eq!(escape_status, StatusCode::INTERNAL_SERVER_ERROR);
    let escape_error: Value = serde_json::from_slice(&escape_body).unwrap();
    assert_eq!(escape_error["error"]["code"], "storage");
    assert_eq!(fs::read(&sentinel).unwrap(), b"external sentinel");

    let link_path = project_dir
        .join("assets")
        .join("original")
        .join("linked.png");
    if create_file_link(&sentinel, &link_path).is_ok() {
        rusqlite::Connection::open(&sqlite)
            .unwrap()
            .execute(
                "UPDATE images SET file_name = 'linked.png' WHERE id = 'demo_001'",
                [],
            )
            .unwrap();
        let (link_status, _, link_body) =
            router_raw_request(&app, Method::GET, &uri, READER_TOKEN, &[]).await;
        assert_eq!(link_status, StatusCode::INTERNAL_SERVER_ERROR);
        let link_error: Value = serde_json::from_slice(&link_body).unwrap();
        assert_eq!(link_error["error"]["code"], "storage");
        assert_eq!(fs::read(&sentinel).unwrap(), b"external sentinel");
        remove_directory_entry(&link_path);
    }
    rusqlite::Connection::open(&sqlite)
        .unwrap()
        .execute(
            "UPDATE images SET file_name = 'demo_001.png' WHERE id = 'demo_001'",
            [],
        )
        .unwrap();
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
async fn dataset_import_is_analyzed_before_explicit_commit_and_can_be_cancelled() {
    const BOUNDARY: &str = "remote-dataset-import-boundary";
    let (name, project_id) = unique_project("Remote dataset import");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_empty_project(&app, &name, &project_id).await;

    let png = png_fixture_bytes([31, 127, 223]);
    let yaml = b"path: .\ntrain: images/train\nnames:\n  0: object\n";
    let label = b"0 0.5 0.5 0.4 0.4\n";
    let upload_body = dataset_multipart_body(
        BOUNDARY,
        &[
            ("images/train/sample.png", png.as_slice()),
            ("labels/train/sample.txt", label),
            ("data.yaml", yaml),
        ],
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(upload_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    let upload_status = response.status();
    let upload_headers = response.headers().clone();
    let upload: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(upload_status, StatusCode::CREATED, "{upload}");
    assert_request_id(&upload_headers, &upload);
    assert_eq!(upload["data"]["state"], "analyzed");
    assert_eq!(upload["data"]["detectedFormat"], "yolo-detect");
    assert_eq!(upload["data"]["imageCount"], 1);
    assert_eq!(upload["data"]["annotationCount"], 1);
    assert!(!upload["data"]["tree"].as_array().unwrap().is_empty());
    assert!(!upload["data"]["tree"][0]["children"]
        .as_array()
        .unwrap()
        .is_empty());
    let import_id = upload["data"]["id"].as_str().unwrap();
    assert!(data_dir
        .join("staging")
        .join("imports")
        .join(import_id)
        .join("payload")
        .is_dir());

    let (before_status, _, before) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/projects/{project_id}/samples"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(before_status, StatusCode::OK, "{before}");
    assert_eq!(before["data"]["total"], 0);

    let (get_status, _, fetched) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/imports/{import_id}"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(get_status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["data"]["state"], "analyzed");

    let (commit_status, _, committed) = router_request(
        &app,
        Method::POST,
        &format!("/api/v1/imports/{import_id}/commit"),
        EDITOR_TOKEN,
        Some(serde_json::json!({ "format": "yolo-detect" })),
    )
    .await;
    assert_eq!(commit_status, StatusCode::OK, "{committed}");
    assert_eq!(committed["data"]["state"], "completed");
    assert_eq!(committed["data"]["imageCount"], 1);

    let (after_status, _, after) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/projects/{project_id}/samples"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(after_status, StatusCode::OK, "{after}");
    assert_eq!(after["data"]["total"], 1);

    let cancel_response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(upload_body))
                .unwrap(),
        )
        .await
        .unwrap();
    let pending: Value = serde_json::from_slice(
        &cancel_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let cancelled_id = pending["data"]["id"].as_str().unwrap();
    let (cancel_status, _, cancelled) = router_request(
        &app,
        Method::DELETE,
        &format!("/api/v1/imports/{cancelled_id}"),
        EDITOR_TOKEN,
        None,
    )
    .await;
    assert_eq!(cancel_status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["data"]["state"], "cancelled");
    assert!(!data_dir
        .join("staging")
        .join("imports")
        .join(cancelled_id)
        .exists());
}

#[tokio::test]
async fn dataset_import_rejects_parent_traversal_without_writing_outside_payload() {
    const BOUNDARY: &str = "remote-import-traversal-boundary";
    let (name, project_id) = unique_project("Remote import traversal");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_empty_project(&app, &name, &project_id).await;
    let png = png_fixture_bytes([220, 32, 64]);
    let body = dataset_multipart_body(BOUNDARY, &[("../escape.png", png.as_slice())]);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response_body}");
    assert_eq!(response_body["error"]["code"], "validation");
    assert!(!data_dir
        .join("staging")
        .join("imports")
        .join("escape.png")
        .exists());
    assert!(!data_dir.join("staging").join("escape.png").exists());
}

#[tokio::test]
async fn dataset_import_never_accepts_server_paths_or_disallowed_extensions() {
    const BOUNDARY: &str = "remote-import-invalid-source-boundary";
    let (name, project_id) = unique_project("Remote import invalid source");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_empty_project(&app, &name, &project_id).await;

    let (path_status, _, path_response) = router_request(
        &app,
        Method::POST,
        &format!("/api/v1/projects/{project_id}/imports"),
        EDITOR_TOKEN,
        Some(serde_json::json!({
            "path": "L:\\data_tool\\datas\\lg\\1580_2d\\train"
        })),
    )
    .await;
    assert_eq!(path_status, StatusCode::BAD_REQUEST, "{path_response}");
    assert_eq!(path_response["error"]["code"], "validation");

    let body = dataset_multipart_body(BOUNDARY, &[("payload.exe", b"not allowed")]);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response_body}");
    assert_eq!(response_body["error"]["code"], "validation");
    assert!(!data_dir.join("payload.exe").exists());
}

#[tokio::test]
async fn dataset_import_enforces_limit_against_decompressed_zip_bytes() {
    const BOUNDARY: &str = "remote-import-zip-limit-boundary";
    let (name, project_id) = unique_project("Remote import zip limit");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    config.max_upload_bytes = 1_024;
    let app = build_router(config).unwrap();
    create_empty_project(&app, &name, &project_id).await;
    let expanded = vec![b'x'; 4_096];
    let archive = zip_fixture(&[("labels/train/large.txt", expanded.as_slice())]);
    assert!(
        archive.len() < 1_024,
        "fixture must exercise decompression limit"
    );
    let body = dataset_multipart_body(BOUNDARY, &[("dataset.zip", archive.as_slice())]);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{response_body}");
    assert_eq!(response_body["error"]["code"], "payload_too_large");
}

#[tokio::test]
async fn dataset_import_rejects_parent_traversal_inside_zip() {
    const BOUNDARY: &str = "remote-import-zip-traversal-boundary";
    let (name, project_id) = unique_project("Remote import zip traversal");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_empty_project(&app, &name, &project_id).await;
    let archive = zip_fixture(&[("../escape.txt", b"outside")]);
    let body = dataset_multipart_body(BOUNDARY, &[("dataset.zip", archive.as_slice())]);

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response_body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(status, StatusCode::BAD_REQUEST, "{response_body}");
    assert_eq!(response_body["error"]["code"], "validation");
    assert!(!data_dir
        .join("staging")
        .join("imports")
        .join("escape.txt")
        .exists());
}

#[tokio::test]
async fn remote_import_flow_indexes_voc_labelme_and_coco_datasets() {
    let png = png_fixture_bytes([72, 104, 184]);
    let voc = br#"<annotation><filename>sample.png</filename><size><width>8</width><height>8</height><depth>3</depth></size><object><name>defect</name><bndbox><xmin>1</xmin><ymin>1</ymin><xmax>6</xmax><ymax>6</ymax></bndbox></object></annotation>"#.to_vec();
    let labelme = serde_json::to_vec(&serde_json::json!({
        "version": "5.4.1",
        "flags": {},
        "shapes": [{
            "label": "defect",
            "points": [[1.0, 1.0], [6.0, 6.0]],
            "group_id": null,
            "shape_type": "rectangle",
            "flags": {}
        }],
        "imagePath": "sample.png",
        "imageData": null,
        "imageHeight": 8,
        "imageWidth": 8
    }))
    .unwrap();
    let coco = serde_json::to_vec(&serde_json::json!({
        "images": [{"id": 1, "file_name": "images/sample.png", "width": 8, "height": 8}],
        "categories": [{"id": 1, "name": "defect"}],
        "annotations": [{
            "id": 1,
            "image_id": 1,
            "category_id": 1,
            "bbox": [1.0, 1.0, 5.0, 5.0],
            "area": 25.0,
            "iscrowd": 0
        }]
    }))
    .unwrap();
    let cases = [
        (
            "voc-detect",
            vec![("sample.png", png.clone()), ("sample.xml", voc)],
        ),
        (
            "labelme",
            vec![("sample.png", png.clone()), ("sample.json", labelme)],
        ),
        (
            "coco",
            vec![
                ("images/sample.png", png),
                ("annotations/instances.json", coco),
            ],
        ),
    ];

    for (index, (format, files)) in cases.into_iter().enumerate() {
        let (name, project_id) = unique_project(&format!("Remote {format} import"));
        let mut config = test_config(Ipv4Addr::LOCALHOST);
        config.reader_token = Some(READER_TOKEN.to_string());
        config.editor_token = Some(EDITOR_TOKEN.to_string());
        config.admin_token = Some(ADMIN_TOKEN.to_string());
        let app = build_router(config).unwrap();
        create_empty_project(&app, &name, &project_id).await;
        let boundary = format!("remote-format-import-boundary-{index}");
        let file_refs = files
            .iter()
            .map(|(path, bytes)| (*path, bytes.as_slice()))
            .collect::<Vec<_>>();
        let body = dataset_multipart_body(&boundary, &file_refs);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/v1/projects/{project_id}/imports"))
                    .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                    .header(
                        header::CONTENT_TYPE,
                        format!("multipart/form-data; boundary={boundary}"),
                    )
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let upload_status = response.status();
        let uploaded: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(upload_status, StatusCode::CREATED, "{format}: {uploaded}");
        assert_eq!(uploaded["data"]["detectedFormat"], format);
        assert_eq!(uploaded["data"]["imageCount"], 1);
        assert_eq!(uploaded["data"]["annotationCount"], 1);
        let import_id = uploaded["data"]["id"].as_str().unwrap();

        let (commit_status, _, committed) = router_request(
            &app,
            Method::POST,
            &format!("/api/v1/imports/{import_id}/commit"),
            EDITOR_TOKEN,
            Some(serde_json::json!({ "format": format })),
        )
        .await;
        assert_eq!(commit_status, StatusCode::OK, "{format}: {committed}");
        let (sample_status, _, samples) = router_request(
            &app,
            Method::GET,
            &format!("/api/v1/projects/{project_id}/samples"),
            READER_TOKEN,
            None,
        )
        .await;
        assert_eq!(sample_status, StatusCode::OK, "{format}: {samples}");
        assert_eq!(samples["data"]["total"], 1, "{format}: {samples}");
    }
}

#[tokio::test]
async fn admin_can_trash_and_restore_a_sample_with_all_managed_files() {
    let (name, project_id) = unique_project("Remote sample trash");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    seed_remote_sample_fixture(&data_dir, &project_id);
    let project_dir = data_dir.join("projects").join(&project_id);
    let managed_annotation = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    fs::write(&managed_annotation, br#"{"imageId":"demo_001"}"#).unwrap();
    let content_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/content");
    let thumbnail_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/thumbnail");
    let annotation_uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let sample_uri = format!("/api/v1/projects/{project_id}/samples/demo_001");
    let restore_uri = format!("{sample_uri}/restore");
    let (thumbnail_status, _, _) =
        router_raw_request(&app, Method::GET, &thumbnail_uri, READER_TOKEN, &[]).await;
    assert_eq!(thumbnail_status, StatusCode::OK);
    let original_image = project_dir
        .join("assets")
        .join("original")
        .join("images")
        .join("train")
        .join("demo_001.png");
    let sidecar = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    assert!(original_image.is_file());
    assert!(sidecar.is_file());
    assert!(managed_annotation.is_file());

    let (editor_status, _, editor_response) =
        router_request(&app, Method::DELETE, &sample_uri, EDITOR_TOKEN, None).await;
    assert_eq!(editor_status, StatusCode::FORBIDDEN, "{editor_response}");

    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &sample_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    assert_eq!(deleted["data"]["status"], "trashed");
    let delete_request_id = deleted["requestId"].as_str().unwrap();
    assert!(!original_image.exists());
    assert!(!sidecar.exists());
    assert!(!managed_annotation.exists());
    let connection = rusqlite::Connection::open(project_dir.join("project.sqlite")).unwrap();
    let (state, move_plan): (String, String) = connection
        .query_row(
            "SELECT state, move_plan_json FROM sample_trash WHERE image_id = 'demo_001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, "trashed");
    let move_plan: Value = serde_json::from_str(&move_plan).unwrap();
    assert!(move_plan.as_array().unwrap().len() >= 4);
    for item in move_plan.as_array().unwrap() {
        assert!(project_dir
            .join(item["trashPath"].as_str().unwrap())
            .is_file());
    }

    let (list_status, _, list) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/projects/{project_id}/samples"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(list_status, StatusCode::OK, "{list}");
    assert_eq!(list["data"]["total"], 2);
    let (project_status, _, project) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/projects/{project_id}"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(project_status, StatusCode::OK, "{project}");
    assert_eq!(project["data"]["imageCount"], 2);
    for uri in [&content_uri, &thumbnail_uri, &annotation_uri] {
        let (status, _, body) = router_raw_request(&app, Method::GET, uri, READER_TOKEN, &[]).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{}",
            String::from_utf8_lossy(&body)
        );
    }
    let (repeat_status, _, repeated) =
        router_request(&app, Method::DELETE, &sample_uri, ADMIN_TOKEN, None).await;
    assert_eq!(repeat_status, StatusCode::OK, "{repeated}");
    assert_eq!(repeated["data"]["status"], "trashed");
    let (editor_restore_status, _, editor_restore) =
        router_request(&app, Method::POST, &restore_uri, EDITOR_TOKEN, None).await;
    assert_eq!(
        editor_restore_status,
        StatusCode::FORBIDDEN,
        "{editor_restore}"
    );

    fs::create_dir_all(original_image.parent().unwrap()).unwrap();
    fs::write(&original_image, b"occupied").unwrap();
    let (conflict_status, _, conflict) =
        router_request(&app, Method::POST, &restore_uri, ADMIN_TOKEN, None).await;
    assert_eq!(conflict_status, StatusCode::CONFLICT, "{conflict}");
    fs::remove_file(&original_image).unwrap();

    let (restore_status, _, restored) =
        router_request(&app, Method::POST, &restore_uri, ADMIN_TOKEN, None).await;
    assert_eq!(restore_status, StatusCode::OK, "{restored}");
    assert_eq!(restored["data"]["status"], "restored");
    let restore_request_id = restored["requestId"].as_str().unwrap();
    assert!(original_image.is_file());
    assert!(sidecar.is_file());
    assert!(managed_annotation.is_file());
    let trash_count: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM sample_trash WHERE image_id = 'demo_001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trash_count, 0);
    let (restored_status, _, restored_sample) =
        router_request(&app, Method::GET, &sample_uri, READER_TOKEN, None).await;
    assert_eq!(restored_status, StatusCode::OK, "{restored_sample}");
    assert_eq!(restored_sample["data"]["id"], "demo_001");
    let (project_status, _, project) = router_request(
        &app,
        Method::GET,
        &format!("/api/v1/projects/{project_id}"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(project_status, StatusCode::OK, "{project}");
    assert_eq!(project["data"]["imageCount"], 3);

    let audit = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    for (request_id, expected_action) in [
        (delete_request_id, "delete_sample"),
        (restore_request_id, "restore_sample"),
    ] {
        let (action, state, image_id): (String, String, Option<String>) = audit
            .query_row(
                "SELECT action, state, image_id FROM service_audit WHERE request_id = ?1",
                [request_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(action, expected_action);
        assert_eq!(state, "completed");
        assert_eq!(image_id.as_deref(), Some("demo_001"));
    }
}

#[tokio::test]
async fn startup_completes_an_interrupted_sample_restore() {
    let (name, project_id) = unique_project("Interrupted sample restore");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let sample_uri = format!("/api/v1/projects/{project_id}/samples/demo_001");
    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &sample_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");
    let project_dir = data_dir.join("projects").join(&project_id);
    let connection = rusqlite::Connection::open(project_dir.join("project.sqlite")).unwrap();
    let move_plan: String = connection
        .query_row(
            "SELECT move_plan_json FROM sample_trash WHERE image_id = 'demo_001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let move_plan: Value = serde_json::from_str(&move_plan).unwrap();
    connection
        .execute(
            "UPDATE sample_trash SET state = 'restoring' WHERE image_id = 'demo_001'",
            [],
        )
        .unwrap();
    let first = &move_plan.as_array().unwrap()[0];
    let source = project_dir.join(first["sourcePath"].as_str().unwrap());
    let trash = project_dir.join(first["trashPath"].as_str().unwrap());
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::rename(&trash, &source).unwrap();
    drop(connection);
    drop(app);

    let restarted = build_router(config).expect("interrupted sample restore must reconcile");
    let connection = rusqlite::Connection::open(project_dir.join("project.sqlite")).unwrap();
    let trash_count: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM sample_trash WHERE image_id = 'demo_001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trash_count, 0);
    for item in move_plan.as_array().unwrap() {
        assert!(project_dir
            .join(item["sourcePath"].as_str().unwrap())
            .is_file());
        assert!(!project_dir
            .join(item["trashPath"].as_str().unwrap())
            .exists());
    }
    let (status, _, sample) =
        router_request(&restarted, Method::GET, &sample_uri, READER_TOKEN, None).await;
    assert_eq!(status, StatusCode::OK, "{sample}");
}

#[tokio::test]
async fn startup_rejects_a_tampered_sample_trash_move_plan() {
    let (name, project_id) = unique_project("Tampered sample trash plan");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let sample_uri = format!("/api/v1/projects/{project_id}/samples/demo_001");
    let (delete_status, _, deleted) =
        router_request(&app, Method::DELETE, &sample_uri, ADMIN_TOKEN, None).await;
    assert_eq!(delete_status, StatusCode::OK, "{deleted}");

    let project_dir = data_dir.join("projects").join(&project_id);
    let sqlite = project_dir.join("project.sqlite");
    let connection = rusqlite::Connection::open(&sqlite).unwrap();
    connection
        .execute(
            "UPDATE sample_trash
             SET state = 'trashing', move_plan_json = ?1
             WHERE image_id = 'demo_001'",
            [serde_json::json!([{
                "sourcePath": "project.sqlite",
                "trashPath": "trash/samples/demo_001/project.sqlite"
            }])
            .to_string()],
        )
        .unwrap();
    drop(connection);
    drop(app);

    let failed_start = build_router(config);
    assert!(failed_start.is_err());
    assert!(sqlite.is_file());
    assert!(!project_dir
        .join("trash")
        .join("samples")
        .join("demo_001")
        .join("project.sqlite")
        .exists());
    drop(failed_start);
}

#[tokio::test]
async fn analyzed_import_becomes_inspectable_failed_session_after_restart() {
    const BOUNDARY: &str = "remote-import-restart-boundary";
    let (name, project_id) = unique_project("Remote import restart");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_empty_project(&app, &name, &project_id).await;
    let png = png_fixture_bytes([30, 180, 90]);
    let body = dataset_multipart_body(
        BOUNDARY,
        &[
            ("images/train/restart.png", png.as_slice()),
            ("labels/train/restart.txt", b"0 0.5 0.5 0.2 0.2\n"),
            ("data.yaml", b"names:\n  0: object\n"),
        ],
    );
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/v1/projects/{project_id}/imports"))
                .header(header::AUTHORIZATION, format!("Bearer {EDITOR_TOKEN}"))
                .header(
                    header::CONTENT_TYPE,
                    format!("multipart/form-data; boundary={BOUNDARY}"),
                )
                .body(Body::from(body))
                .unwrap(),
        )
        .await
        .unwrap();
    let uploaded: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    let import_id = uploaded["data"]["id"].as_str().unwrap().to_string();
    let staging_dir = data_dir.join("staging").join("imports").join(&import_id);
    assert!(staging_dir.is_dir());
    drop(app);

    let restarted = build_router(config).expect("stale import must not block restart");
    let (get_status, _, fetched) = router_request(
        &restarted,
        Method::GET,
        &format!("/api/v1/imports/{import_id}"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(get_status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["data"]["state"], "failed");
    assert!(fetched["data"]["errorMessage"]
        .as_str()
        .unwrap()
        .contains("restarted"));
    assert!(staging_dir.is_dir(), "failed staging remains inspectable");

    let (cancel_status, _, cancelled) = router_request(
        &restarted,
        Method::DELETE,
        &format!("/api/v1/imports/{import_id}"),
        EDITOR_TOKEN,
        None,
    )
    .await;
    assert_eq!(cancel_status, StatusCode::OK, "{cancelled}");
    assert_eq!(cancelled["data"]["state"], "cancelled");
    assert!(!staging_dir.exists());
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

#[tokio::test]
async fn remote_annotations_support_revision_etags_conflicts_and_history() {
    let (name, project_id) = unique_project("Task5 annotation revisions");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (empty_status, empty_headers, empty) =
        router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
    assert_eq!(empty_status, StatusCode::OK, "{empty}");
    assert_eq!(empty["data"]["imageId"], "demo_001");
    assert_eq!(empty["data"]["revision"], Value::Null);
    let native_objects = empty["data"]["objects"].as_array().unwrap();
    assert_eq!(native_objects.len(), 2);
    assert_eq!(native_objects[0]["id"], "ann-0");
    assert_eq!(native_objects[1]["id"], "ann-1");
    assert!(!empty_headers.contains_key(header::ETAG));
    assert!(!empty
        .to_string()
        .contains(&data_dir.to_string_lossy().to_string()));

    let (first_status, first_headers, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("bbox-first", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let first_revision = first["data"]["revision"].as_str().unwrap().to_string();
    let first_etag = first_headers[header::ETAG].to_str().unwrap().to_string();
    assert_eq!(first_etag, format!("\"{first_revision}\""));

    let (second_status, second_headers, second) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", &first_etag)],
        bbox_annotation_body("bbox-second", 1, "region"),
    )
    .await;
    assert_eq!(second_status, StatusCode::OK, "{second}");
    let second_revision = second["data"]["revision"].as_str().unwrap().to_string();
    let second_etag = second_headers[header::ETAG].to_str().unwrap().to_string();
    assert_ne!(second_revision, first_revision);
    assert_eq!(second_etag, format!("\"{second_revision}\""));

    let (conflict_status, _, conflict) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", &first_etag)],
        bbox_annotation_body("stale", 0, "object"),
    )
    .await;
    assert_eq!(conflict_status, StatusCode::CONFLICT, "{conflict}");
    assert_eq!(conflict["error"]["code"], "revision_conflict");

    let history_uri = format!("{uri}/history");
    let (history_status, _, history) =
        router_request(&app, Method::GET, &history_uri, READER_TOKEN, None).await;
    assert_eq!(history_status, StatusCode::OK, "{history}");
    let revisions = history["data"]["items"].as_array().unwrap();
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0]["revision"], first_revision);
    assert_eq!(revisions[1]["revision"], second_revision);
    assert_eq!(revisions[0]["objects"][0]["id"], "bbox-first");
    assert_eq!(revisions[1]["objects"][0]["id"], "bbox-second");
    assert!(!history
        .to_string()
        .contains(&data_dir.to_string_lossy().to_string()));

    let native_path = data_dir
        .join("projects")
        .join(&project_id)
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let native: Value = serde_json::from_slice(&fs::read(native_path).unwrap()).unwrap();
    assert_eq!(native["revision"], second_revision);
    assert_eq!(native["objects"][0]["id"], "bbox-second");
}

#[tokio::test]
async fn annotation_if_match_boundaries_and_editor_authorization_are_explicit() {
    let (name, project_id) = unique_project("Task5 if match");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    clear_annotation_fixture(&data_dir, &project_id, "demo_002");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let second_uri = format!("/api/v1/projects/{project_id}/samples/demo_002/annotations");

    let (reader_status, _, reader_error) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        READER_TOKEN,
        &[],
        bbox_annotation_body("reader-write", 0, "object"),
    )
    .await;
    assert_eq!(reader_status, StatusCode::FORBIDDEN, "{reader_error}");

    let (first_status, first_headers, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("first", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let etag = first_headers[header::ETAG].to_str().unwrap().to_string();

    for (headers, expected_status, expected_code) in [
        (
            Vec::<(&str, &str)>::new(),
            StatusCode::CONFLICT,
            "revision_conflict",
        ),
        (
            vec![("if-match", "W/\"not-strong\"")],
            StatusCode::CONFLICT,
            "revision_conflict",
        ),
        (
            vec![("if-match", "not-quoted")],
            StatusCode::BAD_REQUEST,
            "validation",
        ),
    ] {
        let (status, _, body) = router_json_request_with_headers(
            &app,
            Method::PUT,
            &uri,
            EDITOR_TOKEN,
            &headers,
            bbox_annotation_body("boundary", 0, "object"),
        )
        .await;
        assert_eq!(status, expected_status, "{body}");
        assert_eq!(body["error"]["code"], expected_code);
    }

    let weak_exact = format!("W/{etag}");
    let (weak_status, _, weak) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", &weak_exact)],
        bbox_annotation_body("weak", 0, "object"),
    )
    .await;
    assert_eq!(weak_status, StatusCode::CONFLICT, "{weak}");
    assert_eq!(weak["error"]["code"], "revision_conflict");

    let (wildcard_status, _, wildcard) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", "*")],
        bbox_annotation_body("wildcard", 0, "object"),
    )
    .await;
    assert_eq!(wildcard_status, StatusCode::OK, "{wildcard}");

    let (empty_wildcard_status, _, empty_wildcard) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &second_uri,
        EDITOR_TOKEN,
        &[("if-match", "*")],
        bbox_annotation_body("empty-wildcard", 0, "object"),
    )
    .await;
    assert_eq!(
        empty_wildcard_status,
        StatusCode::CONFLICT,
        "{empty_wildcard}"
    );
    assert_eq!(empty_wildcard["error"]["code"], "revision_conflict");

    let (extra_status, _, extra) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", "*")],
        serde_json::json!({"objects": [], "revision": "client-controlled"}),
    )
    .await;
    assert_eq!(extra_status, StatusCode::BAD_REQUEST, "{extra}");
}

#[tokio::test]
async fn annotation_body_revision_and_if_match_must_describe_the_same_version() {
    let (name, project_id) = unique_project("Task5 body revision");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (first_status, _, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("body-revision-first", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let first_revision = first["data"]["revision"].as_str().unwrap();

    let mut body_only_payload = bbox_annotation_body("body-revision-second", 0, "object");
    body_only_payload["revision"] = Value::String(first_revision.to_string());
    let (body_only_status, _, body_only) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        body_only_payload,
    )
    .await;
    assert_eq!(body_only_status, StatusCode::OK, "{body_only}");
    let second_revision = body_only["data"]["revision"].as_str().unwrap();

    let mut stale_payload = bbox_annotation_body("body-revision-stale", 0, "object");
    stale_payload["revision"] = Value::String(first_revision.to_string());
    let (stale_status, _, stale) =
        router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], stale_payload)
            .await;
    assert_eq!(stale_status, StatusCode::CONFLICT, "{stale}");
    assert_eq!(stale["error"]["code"], "revision_conflict");

    let mut matching_payload = bbox_annotation_body("body-revision-third", 0, "object");
    matching_payload["revision"] = Value::String(second_revision.to_string());
    let matching_header = format!("\"{second_revision}\"");
    let (matching_status, _, matching) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", &matching_header)],
        matching_payload,
    )
    .await;
    assert_eq!(matching_status, StatusCode::OK, "{matching}");
    let third_revision = matching["data"]["revision"].as_str().unwrap();

    let mut mismatched_payload = bbox_annotation_body("body-revision-mismatch", 0, "object");
    mismatched_payload["revision"] = Value::String(second_revision.to_string());
    let mismatched_header = format!("\"{third_revision}\"");
    let (mismatched_status, _, mismatched) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[("if-match", &mismatched_header)],
        mismatched_payload,
    )
    .await;
    assert_eq!(mismatched_status, StatusCode::BAD_REQUEST, "{mismatched}");
    assert_eq!(mismatched["error"]["code"], "validation");

    for invalid_header in [
        format!("\"{third_revision}\", \"different\""),
        format!("W/\"{third_revision}\""),
        format!("W/\"other\", \"{third_revision}\""),
        "*".to_string(),
    ] {
        let mut payload = bbox_annotation_body("body-revision-invalid-header", 0, "object");
        payload["revision"] = Value::String(third_revision.to_string());
        let (status, _, body) = router_json_request_with_headers(
            &app,
            Method::PUT,
            &uri,
            EDITOR_TOKEN,
            &[("if-match", &invalid_header)],
            payload,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{invalid_header}: {body}");
        assert_eq!(body["error"]["code"], "validation");
    }
}

#[tokio::test]
async fn annotation_save_updates_managed_json_yolo_sidecar_and_source_mapping() {
    let (name, project_id) = unique_project("Task5 native YOLO persistence");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("native-yolo", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");

    let project_dir = data_dir.join("projects").join(&project_id);
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let managed: Value =
        serde_json::from_slice(&fs::read(&managed_path).expect("managed JSON must be written"))
            .unwrap();
    assert_eq!(managed["revision"], saved["data"]["revision"]);
    assert_eq!(managed["objects"][0]["id"], "native-yolo");
    assert!(
        !project_dir
            .join("annotations")
            .join("demo_001.json")
            .exists(),
        "managed annotations must not use the legacy parent directory"
    );

    let yolo_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    assert_eq!(
        fs::read_to_string(&yolo_path).unwrap(),
        "0 0.012500 0.019048 0.018750 0.023810\n"
    );

    let connection =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id)).unwrap();
    let (relative_path, annotation_path, source_version): (String, String, String) = connection
        .query_row(
            "SELECT relative_path, annotation_path, source_version
             FROM image_sources WHERE image_id = 'demo_001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(relative_path, "images/train/demo_001.png");
    assert_eq!(annotation_path, "labels/train/demo_001.txt");
    assert!(!source_version.is_empty());
}

#[tokio::test]
async fn annotation_get_loads_native_sidecars_before_the_first_sqlite_save() {
    for (format, demo_template, expected_type) in [
        ("yolo-detect", "demo-bbox", "bbox"),
        ("yolo-seg", "demo-polygon", "polygon"),
        ("voc-detect", "demo-bbox", "bbox"),
        ("labelme", "demo-bbox", "bbox"),
    ] {
        let (name, project_id) = unique_project(&format!("Task5 {format} native read"));
        let mut config = test_config(Ipv4Addr::LOCALHOST);
        config.reader_token = Some(READER_TOKEN.to_string());
        config.editor_token = Some(EDITOR_TOKEN.to_string());
        config.admin_token = Some(ADMIN_TOKEN.to_string());
        let data_dir = config.data_dir.clone();
        let app = build_router(config).unwrap();
        let create_format = if format == "yolo-seg" {
            "yolo-seg"
        } else {
            "yolo-detect"
        };
        create_demo_project(&app, &name, &project_id, create_format, demo_template).await;
        clear_annotation_fixture(&data_dir, &project_id, "demo_001");
        if format != create_format {
            rewrite_project_format(&data_dir, &project_id, format);
        }
        let original = data_dir
            .join("projects")
            .join(&project_id)
            .join("assets")
            .join("original");
        match format {
            "yolo-detect" => {
                fs::write(
                    original.join("labels").join("train").join("demo_001.txt"),
                    b"0 0.500000 0.500000 0.250000 0.250000\n",
                )
                .unwrap();
            }
            "yolo-seg" => {
                fs::write(
                    original.join("labels").join("train").join("demo_001.txt"),
                    b"0 0.100000 0.100000 0.800000 0.100000 0.800000 0.800000\n",
                )
                .unwrap();
            }
            "voc-detect" => {
                let path = original
                    .join("Annotations")
                    .join("train")
                    .join("demo_001.xml");
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(
                    path,
                    br#"<annotation><filename>demo_001.png</filename><size><width>640</width><height>420</height><depth>4</depth></size><object><name>object</name><bndbox><xmin>10</xmin><ymin>12</ymin><xmax>40</xmax><ymax>36</ymax></bndbox></object></annotation>"#,
                )
                .unwrap();
            }
            "labelme" => {
                fs::write(
                    original.join("images").join("train").join("demo_001.json"),
                    serde_json::to_vec(&serde_json::json!({
                        "version": "5.0.1",
                        "flags": {},
                        "shapes": [{
                            "label": "object",
                            "points": [[10.0, 12.0], [40.0, 36.0]],
                            "group_id": null,
                            "shape_type": "rectangle",
                            "flags": {}
                        }],
                        "imagePath": "demo_001.png",
                        "imageData": null,
                        "imageHeight": 420,
                        "imageWidth": 640
                    }))
                    .unwrap(),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }

        let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
        let (status, _, body) = router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
        assert_eq!(status, StatusCode::OK, "{format}: {body}");
        assert_eq!(body["data"]["revision"], Value::Null, "{format}: {body}");
        assert_eq!(
            body["data"]["objects"][0]["type"], expected_type,
            "{format}: {body}"
        );
        assert_eq!(
            body["data"]["objects"][0]["label"], "object",
            "{format}: {body}"
        );

        if format == "yolo-detect" {
            let (first_status, _, first) = router_json_request_with_headers(
                &app,
                Method::PUT,
                &uri,
                EDITOR_TOKEN,
                &[],
                bbox_annotation_body("first-sqlite-save", 0, "object"),
            )
            .await;
            assert_eq!(first_status, StatusCode::OK, "{first}");
            let (second_status, _, second) = router_json_request_with_headers(
                &app,
                Method::PUT,
                &uri,
                EDITOR_TOKEN,
                &[],
                bbox_annotation_body("missing-first-revision", 0, "object"),
            )
            .await;
            assert_eq!(second_status, StatusCode::CONFLICT, "{second}");
            assert_eq!(second["error"]["code"], "revision_conflict");
        }
    }
}

#[tokio::test]
async fn annotation_save_writes_yolo_seg_voc_and_labelme_sidecars() {
    for (format, demo_template) in [
        ("yolo-seg", "demo-polygon"),
        ("voc-detect", "demo-bbox"),
        ("labelme", "demo-bbox"),
    ] {
        let (name, project_id) = unique_project(&format!("Task5 {format} persistence"));
        let mut config = test_config(Ipv4Addr::LOCALHOST);
        config.editor_token = Some(EDITOR_TOKEN.to_string());
        config.admin_token = Some(ADMIN_TOKEN.to_string());
        let data_dir = config.data_dir.clone();
        let app = build_router(config).unwrap();
        let create_format = if format == "yolo-seg" {
            "yolo-seg"
        } else {
            "yolo-detect"
        };
        create_demo_project(&app, &name, &project_id, create_format, demo_template).await;
        clear_annotation_fixture(&data_dir, &project_id, "demo_001");
        if format != create_format {
            rewrite_project_format(&data_dir, &project_id, format);
        }
        let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
        let payload = if format == "yolo-seg" {
            polygon_annotation_body("native-polygon", 0, "object")
        } else {
            bbox_annotation_body("native-bbox", 0, "object")
        };

        let (status, _, saved) =
            router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], payload)
                .await;
        assert_eq!(status, StatusCode::OK, "{format}: {saved}");

        let original = data_dir
            .join("projects")
            .join(&project_id)
            .join("assets")
            .join("original");
        match format {
            "yolo-seg" => {
                let sidecar =
                    fs::read_to_string(original.join("labels").join("train").join("demo_001.txt"))
                        .unwrap();
                assert_eq!(
                    sidecar,
                    "0 0.003125 0.004762 0.028125 0.004762 0.028125 0.038095 0.003125 0.038095\n"
                );
            }
            "voc-detect" => {
                let sidecar = fs::read_to_string(
                    original
                        .join("Annotations")
                        .join("train")
                        .join("demo_001.xml"),
                )
                .unwrap();
                assert!(sidecar.contains("<name>object</name>"), "{sidecar}");
                assert!(sidecar.contains("<xmin>2</xmin>"), "{sidecar}");
            }
            "labelme" => {
                let sidecar: Value = serde_json::from_slice(
                    &fs::read(original.join("images").join("train").join("demo_001.json")).unwrap(),
                )
                .unwrap();
                assert_eq!(sidecar["shapes"][0]["label"], "object");
                assert_eq!(sidecar["shapes"][0]["shape_type"], "rectangle");
            }
            _ => unreachable!(),
        }
    }
}

#[tokio::test]
async fn annotation_save_rejects_external_sidecar_edits_without_overwriting_them() {
    let (name, project_id) = unique_project("Task5 external sidecar conflict");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (first_status, _, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("before-external-edit", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let first_revision = first["data"]["revision"].as_str().unwrap();
    let sidecar_path = data_dir
        .join("projects")
        .join(&project_id)
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    fs::write(&sidecar_path, b"external edit\n").unwrap();
    let mut payload = bbox_annotation_body("must-not-win", 0, "object");
    payload["revision"] = Value::String(first_revision.to_string());

    let (status, _, body) =
        router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], payload).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "conflict");
    assert_eq!(fs::read(&sidecar_path).unwrap(), b"external edit\n");
    let revision: String =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT revision FROM annotations WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(revision, first_revision);
}

#[tokio::test]
async fn annotation_save_accepts_legacy_source_versions_and_migrates_them_safely() {
    let (name, project_id) = unique_project("Task5 legacy source version");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (first_status, _, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("legacy-first", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let first_revision = first["data"]["revision"].as_str().unwrap().to_string();
    let project_dir = data_dir.join("projects").join(&project_id);
    let project_db = annotation_project_sqlite(&data_dir, &project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let legacy_version = legacy_source_version_fixture(&sidecar_path);
    rusqlite::Connection::open(&project_db)
        .unwrap()
        .execute(
            "UPDATE image_sources SET source_version = ?1 WHERE image_id = 'demo_001'",
            [&legacy_version],
        )
        .unwrap();
    let mut second_payload = bbox_annotation_body("legacy-second", 0, "object");
    second_payload["revision"] = Value::String(first_revision);

    let (second_status, _, second) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        second_payload,
    )
    .await;
    assert_eq!(second_status, StatusCode::OK, "{second}");
    let second_revision = second["data"]["revision"].as_str().unwrap().to_string();
    let migrated_version: String = rusqlite::Connection::open(&project_db)
        .unwrap()
        .query_row(
            "SELECT source_version FROM image_sources WHERE image_id = 'demo_001'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(
        migrated_version.starts_with("sha256:"),
        "{migrated_version}"
    );

    let external_bytes = b"external edit after legacy migration\n";
    fs::write(&sidecar_path, external_bytes).unwrap();
    let mut third_payload = bbox_annotation_body("legacy-third", 0, "object");
    third_payload["revision"] = Value::String(second_revision);
    let (third_status, _, third) =
        router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], third_payload)
            .await;
    assert_eq!(third_status, StatusCode::CONFLICT, "{third}");
    assert_eq!(third["error"]["code"], "conflict");
    assert_eq!(fs::read(&sidecar_path).unwrap(), external_bytes);
}

#[tokio::test]
async fn annotation_save_rolls_back_files_and_sqlite_when_source_mapping_commit_fails() {
    let (name, project_id) = unique_project("Task5 annotation compensation");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (first_status, _, first) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("compensation-before", 0, "object"),
    )
    .await;
    assert_eq!(first_status, StatusCode::OK, "{first}");
    let first_revision = first["data"]["revision"].as_str().unwrap().to_string();
    let project_dir = data_dir.join("projects").join(&project_id);
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let managed_before = fs::read(&managed_path).unwrap();
    let sidecar_before = fs::read(&sidecar_path).unwrap();
    let project_db = annotation_project_sqlite(&data_dir, &project_id);
    let source_before: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(&project_db)
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    rusqlite::Connection::open(&project_db)
        .unwrap()
        .execute_batch(
            r#"
            CREATE TRIGGER fail_task5_source_mapping
            BEFORE UPDATE OF source_version ON image_sources
            WHEN OLD.image_id = 'demo_001'
            BEGIN
                SELECT RAISE(ABORT, 'injected source mapping failure');
            END;
            "#,
        )
        .unwrap();
    let mut payload = bbox_annotation_body("compensation-after", 0, "object");
    payload["revision"] = Value::String(first_revision.clone());
    payload["objects"][0]["bbox"]["width"] = serde_json::json!(24.0);
    payload["objects"][0]["bbox"]["height"] = serde_json::json!(18.0);

    let (status, _, body) =
        router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], payload).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "storage");
    assert_eq!(fs::read(&managed_path).unwrap(), managed_before);
    assert_eq!(fs::read(&sidecar_path).unwrap(), sidecar_before);
    let (revision, object_json): (String, String) = rusqlite::Connection::open(&project_db)
        .unwrap()
        .query_row(
            "SELECT revision, object_json FROM annotations WHERE image_id = 'demo_001'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(revision, first_revision);
    assert_eq!(
        serde_json::from_str::<Value>(&object_json).unwrap()[0]["id"],
        "compensation-before"
    );
    let source_after: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(project_db)
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    assert_eq!(source_after, source_before);
}

#[tokio::test]
async fn annotation_save_rejects_formats_without_safe_per_image_writeback() {
    let (name, project_id) = unique_project("Task5 unsupported native writeback");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (status, _, body) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        polygon_annotation_body("unsupported", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "annotation_validation");
    let annotation_count: u64 =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM annotations WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(annotation_count, 0);
}

#[tokio::test]
async fn annotation_validation_rejects_invalid_classes_geometry_and_size() {
    let (name, project_id) = unique_project("Task5 annotation validation");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let invalid_payloads = [
        serde_json::json!({"objects": [{
            "id": "unknown-class", "classId": 999, "label": "unknown", "type": "bbox",
            "bbox": {"x": 1.0, "y": 1.0, "width": 2.0, "height": 2.0}, "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "zero-width", "classId": 0, "label": "object", "type": "bbox",
            "bbox": {"x": 1.0, "y": 1.0, "width": 0.0, "height": 2.0}, "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "out-of-bounds", "classId": 0, "label": "object", "type": "bbox",
            "bbox": {"x": 639.0, "y": 419.0, "width": 10.0, "height": 10.0}, "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "short-polygon", "classId": 1, "label": "region", "type": "polygon",
            "polygon": [{"x": 1.0, "y": 1.0}, {"x": 2.0, "y": 2.0}], "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "duplicate-polygon", "classId": 1, "label": "region", "type": "polygon",
            "polygon": [
                {"x": 1.0, "y": 1.0}, {"x": 1.0, "y": 1.0}, {"x": 2.0, "y": 2.0}
            ], "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "non-finite", "classId": 0, "label": "object", "type": "bbox",
            "bbox": {"x": "NaN", "y": 1.0, "width": 2.0, "height": 2.0}, "attributes": {}
        }]}),
        serde_json::json!({"objects": [{
            "id": "unknown-shape", "classId": 0, "label": "object", "type": "ellipse",
            "attributes": {}
        }]}),
    ];
    for payload in invalid_payloads {
        let (status, _, body) =
            router_json_request_with_headers(&app, Method::PUT, &uri, EDITOR_TOKEN, &[], payload)
                .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "annotation_validation");
    }

    let object = bbox_annotation_body("many", 0, "object")["objects"][0].clone();
    let too_many = Value::Array((0..1001).map(|_| object.clone()).collect());
    let (large_status, _, large) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        serde_json::json!({"objects": too_many}),
    )
    .await;
    assert_eq!(large_status, StatusCode::UNPROCESSABLE_ENTITY, "{large}");
    assert_eq!(large["error"]["code"], "annotation_validation");
}

#[tokio::test]
async fn classification_projects_reject_geometric_annotation_writes() {
    let (name, project_id) = unique_project("Task5 classification annotations");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let app = build_router(config).unwrap();
    create_demo_project(
        &app,
        &name,
        &project_id,
        "image-classification",
        "demo-classification",
    )
    .await;
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (status, _, body) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("unsupported", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "annotation_validation");
}

#[tokio::test]
async fn annotation_submit_and_review_workflow_records_both_decisions() {
    let (name, project_id) = unique_project("Task5 review workflow");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let base = format!("/api/v1/projects/{project_id}/samples/demo_001");

    let (reader_submit_status, _, reader_submit) = router_request(
        &app,
        Method::POST,
        &format!("{base}/submit"),
        READER_TOKEN,
        None,
    )
    .await;
    assert_eq!(
        reader_submit_status,
        StatusCode::FORBIDDEN,
        "{reader_submit}"
    );
    let (reader_review_status, _, reader_review) = router_json_request_with_headers(
        &app,
        Method::POST,
        &format!("{base}/review"),
        READER_TOKEN,
        &[],
        serde_json::json!({"decision": "approved", "note": ""}),
    )
    .await;
    assert_eq!(
        reader_review_status,
        StatusCode::FORBIDDEN,
        "{reader_review}"
    );

    let (submit_status, _, submit) = router_request(
        &app,
        Method::POST,
        &format!("{base}/submit"),
        EDITOR_TOKEN,
        None,
    )
    .await;
    assert_eq!(submit_status, StatusCode::OK, "{submit}");
    assert_eq!(submit["data"]["status"], "待质检");
    assert_eq!(submit["data"]["qaStatus"], "待质检");

    let (approved_status, _, approved) = router_json_request_with_headers(
        &app,
        Method::POST,
        &format!("{base}/review"),
        EDITOR_TOKEN,
        &[],
        serde_json::json!({"decision": "approved", "note": "looks good"}),
    )
    .await;
    assert_eq!(approved_status, StatusCode::OK, "{approved}");
    assert_eq!(approved["data"]["status"], "通过");
    assert_eq!(approved["data"]["qaStatus"], "通过");
    assert_eq!(approved["data"]["reviewNote"], "looks good");

    let (resubmit_status, _, resubmit) = router_request(
        &app,
        Method::POST,
        &format!("{base}/submit"),
        EDITOR_TOKEN,
        None,
    )
    .await;
    assert_eq!(resubmit_status, StatusCode::OK, "{resubmit}");
    let (rejected_status, _, rejected) = router_json_request_with_headers(
        &app,
        Method::POST,
        &format!("{base}/review"),
        EDITOR_TOKEN,
        &[],
        serde_json::json!({"decision": "rejected", "note": "fix the boundary"}),
    )
    .await;
    assert_eq!(rejected_status, StatusCode::OK, "{rejected}");
    assert_eq!(rejected["data"]["status"], "草稿");
    assert_eq!(rejected["data"]["qaStatus"], "驳回");
    assert_eq!(rejected["data"]["reviewNote"], "fix the boundary");

    let sqlite =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id)).unwrap();
    let reviews = sqlite
        .prepare(
            "SELECT id, decision FROM qa_reviews WHERE image_id = ?1 ORDER BY created_at, rowid",
        )
        .unwrap()
        .query_map(["demo_001"], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        reviews
            .iter()
            .map(|(_, decision)| decision.as_str())
            .collect::<Vec<_>>(),
        vec!["通过", "驳回"]
    );
    let server = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    for ((operation_id, _), expected_decision) in reviews.iter().zip(["approved", "rejected"]) {
        let (action, state, payload): (String, String, String) = server
            .query_row(
                "SELECT action, state, payload FROM service_audit WHERE operation_id = ?1",
                [operation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(action, "review_annotations");
        assert_eq!(state, "completed");
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap()["decision"],
            expected_decision
        );
    }

    for payload in [
        serde_json::json!({"decision": "unknown", "note": ""}),
        serde_json::json!({"decision": "approved", "note": "x".repeat(2001)}),
    ] {
        let (status, _, body) = router_json_request_with_headers(
            &app,
            Method::POST,
            &format!("{base}/review"),
            EDITOR_TOKEN,
            &[],
            payload,
        )
        .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(body["error"]["code"], "annotation_validation");
    }

    let (missing_status, _, missing) = router_request(
        &app,
        Method::POST,
        &format!("/api/v1/projects/{project_id}/samples/missing/submit"),
        EDITOR_TOKEN,
        None,
    )
    .await;
    assert_eq!(missing_status, StatusCode::NOT_FOUND, "{missing}");
}

#[tokio::test]
async fn annotation_mutations_have_correlated_global_and_project_audit_evidence() {
    let (name, project_id) = unique_project("Task5 audit evidence");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("audit", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let server = rusqlite::Connection::open(data_dir.join("server.sqlite")).unwrap();
    let (operation_id, role, action, record_project, record_image, state, payload): (
        String,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = server
        .query_row(
            "SELECT operation_id, role, action, project_id, image_id, state, payload
             FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(role, "editor");
    assert_eq!(action, "save_annotations");
    assert_eq!(record_project, project_id);
    assert_eq!(record_image, "demo_001");
    assert_eq!(state, "completed");
    let payload_json = serde_json::from_str::<Value>(&payload).unwrap();
    assert_eq!(payload_json["projectId"], project_id);
    assert_eq!(payload_json["imageId"], "demo_001");
    assert_eq!(payload_json["objectCount"], 1);
    assert!(payload_json["contentSha256"].as_str().is_some());
    assert!(!payload.contains(&data_dir.to_string_lossy().to_string()));

    let project =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id)).unwrap();
    let project_action: String = project
        .query_row(
            "SELECT action FROM audit_events WHERE id = ?1 AND image_id = 'demo_001'",
            [&operation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(project_action, "annotation.save");
}

#[test]
fn annotation_completion_failure_recovers_from_project_evidence() {
    run_ignored_test_in_subprocess(
        "annotation_completion_failure_recovers_from_project_evidence_child",
    );
}

#[test]
fn completed_annotation_file_transaction_recovers_as_an_orphan_on_restart() {
    run_ignored_test_in_subprocess(
        "completed_annotation_file_transaction_recovers_as_an_orphan_on_restart_child",
    );
}

#[test]
fn completed_annotation_orphan_preserves_a_divergent_external_sidecar_on_restart() {
    run_ignored_test_in_subprocess(
        "completed_annotation_orphan_preserves_a_divergent_external_sidecar_on_restart_child",
    );
}

#[test]
fn completed_annotation_orphan_preserves_divergent_managed_json_on_restart() {
    run_ignored_test_in_subprocess(
        "completed_annotation_orphan_preserves_divergent_managed_json_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn completed_annotation_file_transaction_recovers_as_an_orphan_on_restart_child() {
    let (name, project_id) = unique_project("Task5 orphan committed recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let old_sidecar = fs::read(&sidecar_path).unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("orphan-committed", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let operation_id: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let new_sidecar = fs::read(&sidecar_path).unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.old"), &old_sidecar).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), &new_sidecar).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": "images/train/demo_001.png",
            "sidecarRelativePath": "labels/train/demo_001.txt",
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": true,
            "managedOldSha256": null,
            "managedNewSha256": null,
            "sidecarOldSha256": sha256_hex_fixture(&old_sidecar),
            "sidecarNewSha256": sha256_hex_fixture(&new_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();
    fs::remove_file(&managed_path).unwrap();
    fs::remove_file(&sidecar_path).unwrap();
    rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
        .unwrap()
        .execute(
            "UPDATE image_sources SET source_version = 'stale' WHERE image_id = 'demo_001'",
            [],
        )
        .unwrap();
    drop(app);

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), new_sidecar);
    let managed: Value = serde_json::from_slice(&fs::read(&managed_path).unwrap()).unwrap();
    assert_eq!(managed["objects"][0]["id"], "orphan-committed");
    let source_version: String =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT source_version FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(
        source_version,
        format!("sha256:{}", sha256_hex_fixture(&new_sidecar))
    );
    assert!(!transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn completed_annotation_orphan_preserves_a_divergent_external_sidecar_on_restart_child() {
    let (name, project_id) = unique_project("Task5 divergent orphan recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let old_sidecar = fs::read(&sidecar_path).unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("divergent-orphan", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let operation_id: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    let new_sidecar = fs::read(&sidecar_path).unwrap();
    let source_before: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.old"), &old_sidecar).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), &new_sidecar).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": source_before.1.clone(),
            "sidecarRelativePath": source_before.3.clone(),
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": true,
            "managedOldSha256": null,
            "managedNewSha256": null,
            "sidecarOldSha256": sha256_hex_fixture(&old_sidecar),
            "sidecarNewSha256": sha256_hex_fixture(&new_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();
    let external_sidecar = b"externally edited after completed save\n";
    fs::write(&sidecar_path, external_sidecar).unwrap();
    drop(app);

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), external_sidecar);
    let source_after: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    assert_eq!(source_after, source_before);
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "indeterminate");
    assert_eq!(
        message,
        "annotation file recovery conflicted with external content"
    );
    assert!(transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn completed_annotation_orphan_preserves_divergent_managed_json_on_restart_child() {
    let (name, project_id) = unique_project("Task5 divergent managed recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let old_sidecar = fs::read(&sidecar_path).unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("divergent-managed", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let operation_id: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    let new_sidecar = fs::read(&sidecar_path).unwrap();
    let new_managed = fs::read(&managed_path).unwrap();
    let source: (String, Option<String>) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT relative_path, annotation_path
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.old"), &old_sidecar).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), &new_sidecar).unwrap();
    fs::write(transaction_dir.join("managed.new"), &new_managed).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": source.0,
            "sidecarRelativePath": source.1,
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": true,
            "managedOldSha256": null,
            "managedNewSha256": sha256_hex_fixture(&new_managed),
            "sidecarOldSha256": sha256_hex_fixture(&old_sidecar),
            "sidecarNewSha256": sha256_hex_fixture(&new_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();
    let external_managed = br#"{"external":"managed edit"}"#;
    fs::write(&managed_path, external_managed).unwrap();
    drop(app);

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&managed_path).unwrap(), external_managed);
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "indeterminate");
    assert_eq!(
        message,
        "annotation file recovery conflicted with external content"
    );
    assert!(transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn annotation_completion_failure_recovers_from_project_evidence_child() {
    let (name, project_id) = unique_project("Task5 completion recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    install_completion_failure_trigger(&data_dir, "fail_task5_save_completion", "save_annotations");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("recoverable", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let operation_id: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("pending")
    );
    let native_path = data_dir
        .join("projects")
        .join(&project_id)
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let sidecar_path = data_dir
        .join("projects")
        .join(&project_id)
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    fs::remove_file(&native_path).unwrap();
    fs::remove_file(&sidecar_path).unwrap();
    let later_metadata = SampleMetadataFixture {
        split: "test".to_string(),
        status: "通过".to_string(),
        qa_status: "通过".to_string(),
        review_note: Some("local edit after commit".to_string()),
    };
    write_sample_metadata(&data_dir, &project_id, "demo_001", &later_metadata);
    drop(app);
    drop_test_trigger(&data_dir, "fail_task5_save_completion");

    let restarted = build_router(config).unwrap();
    assert_eq!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("completed")
    );
    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        later_metadata,
        "recovery must not overwrite a later local metadata edit"
    );
    let native: Value = serde_json::from_slice(&fs::read(native_path).unwrap()).unwrap();
    assert_eq!(native["objects"][0]["id"], "recoverable");
    assert_eq!(native["status"], "通过");
    assert_eq!(
        fs::read_to_string(sidecar_path).unwrap(),
        "0 0.012500 0.019048 0.018750 0.023810\n"
    );
    drop(restarted);
}

#[test]
fn annotation_routes_reject_project_sqlite_sidecar_links() {
    run_ignored_test_in_subprocess("annotation_routes_reject_project_sqlite_sidecar_links_child");
}

#[tokio::test]
#[ignore]
async fn annotation_routes_reject_project_sqlite_sidecar_links_child() {
    let (name, project_id) = unique_project("Task5 sqlite sidecar");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let external_dir = unique_temp_root("task5-sidecar-external");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let sentinel = external_dir.join("sentinel");
    fs::write(&sentinel, b"external sqlite sidecar sentinel").unwrap();
    let sidecar = data_dir
        .join("projects")
        .join(&project_id)
        .join("project.sqlite-wal");
    let linked = create_file_link(&sentinel, &sidecar).is_ok();
    if !linked {
        return;
    }
    let _link_cleanup = RemoveLinksOnDrop(vec![sidecar]);
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");

    let (status, _, body) = router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    assert_eq!(body["error"]["code"], "storage");
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        b"external sqlite sidecar sentinel"
    );
}

#[test]
fn pending_annotation_mutations_reconcile_only_from_bound_project_evidence() {
    run_ignored_test_in_subprocess(
        "pending_annotation_mutations_reconcile_only_from_bound_project_evidence_child",
    );
}

#[test]
fn pending_annotation_recovery_rejects_a_journal_bound_to_another_sample() {
    run_ignored_test_in_subprocess(
        "pending_annotation_recovery_rejects_a_journal_bound_to_another_sample_child",
    );
}

#[test]
fn corrupt_annotation_journal_is_quarantined_without_blocking_restarts() {
    run_ignored_test_in_subprocess(
        "corrupt_annotation_journal_is_quarantined_without_blocking_restarts_child",
    );
}

#[test]
fn annotation_journal_cannot_delete_another_samples_sidecar() {
    run_ignored_test_in_subprocess(
        "annotation_journal_cannot_delete_another_samples_sidecar_child",
    );
}

#[test]
fn committed_annotation_recovery_rejects_corrupted_staged_content() {
    run_ignored_test_in_subprocess(
        "committed_annotation_recovery_rejects_corrupted_staged_content_child",
    );
}

#[test]
fn uncommitted_annotation_file_transaction_rolls_back_on_restart() {
    run_ignored_test_in_subprocess(
        "uncommitted_annotation_file_transaction_rolls_back_on_restart_child",
    );
}

#[tokio::test]
#[ignore]
async fn corrupt_annotation_journal_is_quarantined_without_blocking_restarts_child() {
    let (name, project_id) = unique_project("Task5 corrupt journal quarantine");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("journal-baseline", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let sidecar_before = fs::read(&sidecar_path).unwrap();
    let managed_before = fs::read(&managed_path).unwrap();
    let source_before: (String, String, Option<String>) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT relative_path, source_version, annotation_path
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    let revision_before: String =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT revision FROM annotations WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(b"truncated-journal"));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("journal.json"), b"{\"version\":").unwrap();
    drop(app);

    let restarted_once = build_router(config.clone()).unwrap();
    drop(restarted_once);
    let restarted_twice = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), sidecar_before);
    assert_eq!(fs::read(&managed_path).unwrap(), managed_before);
    let source_after: (String, String, Option<String>) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT relative_path, source_version, annotation_path
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
    assert_eq!(source_after, source_before);
    let revision_after: String =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT revision FROM annotations WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(revision_after, revision_before);
    assert!(!transaction_dir.exists());
    let quarantine = project_dir
        .join("annotations")
        .join("transaction-quarantine");
    assert_eq!(fs::read_dir(quarantine).unwrap().count(), 1);
    drop(restarted_twice);
}

#[tokio::test]
#[ignore]
async fn annotation_journal_cannot_delete_another_samples_sidecar_child() {
    let (name, project_id) = unique_project("Task5 journal path deletion attack");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_002");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_002/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("sample-b-path", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    drop(app);

    let operation_id =
        insert_pending_annotation_operation(&data_dir, &project_id, "demo_001", "save_annotations");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_002.txt");
    let sidecar_before = fs::read(&sidecar_path).unwrap();
    let source: (String, Option<String>) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT relative_path, annotation_path
                 FROM image_sources WHERE image_id = 'demo_002'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), &sidecar_before).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": source.0,
            "sidecarRelativePath": source.1,
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": false,
            "managedOldSha256": null,
            "managedNewSha256": null,
            "sidecarOldSha256": null,
            "sidecarNewSha256": sha256_hex_fixture(&sidecar_before)
        }))
        .unwrap(),
    )
    .unwrap();

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), sidecar_before);
    assert_ne!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("completed")
    );
    assert!(transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn committed_annotation_recovery_rejects_corrupted_staged_content_child() {
    let (name, project_id) = unique_project("Task5 staged corruption");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let old_sidecar = fs::read(&sidecar_path).unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (status, _, saved) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("staged-corruption", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    let request_id = saved["requestId"].as_str().unwrap();
    let operation_id: String = rusqlite::Connection::open(data_dir.join("server.sqlite"))
        .unwrap()
        .query_row(
            "SELECT operation_id FROM service_audit WHERE request_id = ?1",
            [request_id],
            |row| row.get(0),
        )
        .unwrap();
    let intended_sidecar = fs::read(&sidecar_path).unwrap();
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let managed_new = fs::read(&managed_path).unwrap();
    let source: (String, Option<String>) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT relative_path, annotation_path
                 FROM image_sources WHERE image_id = 'demo_001'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.old"), &old_sidecar).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), b"truncated").unwrap();
    fs::write(transaction_dir.join("managed.new"), &managed_new).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": source.0,
            "sidecarRelativePath": source.1,
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": true,
            "managedOldSha256": null,
            "managedNewSha256": sha256_hex_fixture(&managed_new),
            "sidecarOldSha256": sha256_hex_fixture(&old_sidecar),
            "sidecarNewSha256": sha256_hex_fixture(&intended_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&sidecar_path, &old_sidecar).unwrap();
    drop(app);

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), old_sidecar);
    let (state, message) = sample_operation_state(&data_dir, &operation_id);
    assert_eq!(state, "indeterminate");
    assert_eq!(
        message,
        "annotation transaction identity or content is inconsistent"
    );
    assert!(transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn uncommitted_annotation_file_transaction_rolls_back_on_restart_child() {
    let (name, project_id) = unique_project("Task5 uncommitted file recovery");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    drop(app);

    let operation_id =
        insert_pending_annotation_operation(&data_dir, &project_id, "demo_001", "save_annotations");
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_001.txt");
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_001.json");
    let old_sidecar = fs::read(&sidecar_path).unwrap();
    let new_sidecar = b"0 0.100000 0.100000 0.200000 0.200000\n";
    let new_managed = br#"{"imageId":"demo_001","revision":"uncommitted"}"#;
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir_all(&transaction_dir).unwrap();
    fs::write(transaction_dir.join("sidecar.old"), &old_sidecar).unwrap();
    fs::write(transaction_dir.join("sidecar.new"), new_sidecar).unwrap();
    fs::write(transaction_dir.join("managed.new"), new_managed).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_001",
            "imageRelativePath": "images/train/demo_001.png",
            "sidecarRelativePath": "labels/train/demo_001.txt",
            "expectedSourceVersion": "",
            "managedHadOriginal": false,
            "sidecarHadOriginal": true,
            "managedOldSha256": null,
            "managedNewSha256": sha256_hex_fixture(new_managed),
            "sidecarOldSha256": sha256_hex_fixture(&old_sidecar),
            "sidecarNewSha256": sha256_hex_fixture(new_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&sidecar_path, new_sidecar).unwrap();
    fs::write(&managed_path, new_managed).unwrap();

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), old_sidecar);
    assert!(!managed_path.exists());
    assert!(!transaction_dir.exists());
    let annotation_count: u64 =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM annotations WHERE image_id = 'demo_001'",
                [],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(annotation_count, 0);
    assert_eq!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("indeterminate")
    );
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn pending_annotation_recovery_rejects_a_journal_bound_to_another_sample_child() {
    let (name, project_id) = unique_project("Task5 journal sample binding");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_002");
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_002/annotations");
    let (status, _, body) = router_json_request_with_headers(
        &app,
        Method::PUT,
        &uri,
        EDITOR_TOKEN,
        &[],
        bbox_annotation_body("sample-b-before", 0, "object"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    drop(app);

    let operation_id =
        insert_pending_annotation_operation(&data_dir, &project_id, "demo_001", "save_annotations");
    insert_project_annotation_event(
        &data_dir,
        &project_id,
        "demo_001",
        &operation_id,
        "annotation.save",
    );
    let project_dir = data_dir.join("projects").join(&project_id);
    let sidecar_path = project_dir
        .join("assets")
        .join("original")
        .join("labels")
        .join("train")
        .join("demo_002.txt");
    let managed_path = project_dir
        .join("annotations")
        .join("native")
        .join("demo_002.json");
    let sidecar_before = fs::read(&sidecar_path).unwrap();
    let managed_before = fs::read(&managed_path).ok();
    let source_before: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_002'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    let transaction_dir = project_dir
        .join("annotations")
        .join("transactions")
        .join(sha256_hex_fixture(operation_id.as_bytes()));
    fs::create_dir(&transaction_dir).unwrap();
    if let Some(bytes) = managed_before.as_deref() {
        fs::write(transaction_dir.join("managed.old"), bytes).unwrap();
    }
    fs::write(transaction_dir.join("sidecar.old"), &sidecar_before).unwrap();
    let malicious_sidecar = b"0 0.900000 0.900000 0.100000 0.100000\n";
    fs::write(transaction_dir.join("sidecar.new"), malicious_sidecar).unwrap();
    fs::write(
        transaction_dir.join("journal.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "version": 2,
            "operationId": operation_id,
            "projectId": project_id,
            "imageId": "demo_002",
            "imageRelativePath": source_before.1.clone(),
            "sidecarRelativePath": source_before.3.clone(),
            "expectedSourceVersion": "",
            "managedHadOriginal": managed_before.is_some(),
            "sidecarHadOriginal": true,
            "managedOldSha256": managed_before.as_deref().map(sha256_hex_fixture),
            "managedNewSha256": null,
            "sidecarOldSha256": sha256_hex_fixture(&sidecar_before),
            "sidecarNewSha256": sha256_hex_fixture(malicious_sidecar)
        }))
        .unwrap(),
    )
    .unwrap();

    let restarted = build_router(config).unwrap();
    assert_eq!(fs::read(&sidecar_path).unwrap(), sidecar_before);
    assert_eq!(fs::read(&managed_path).ok(), managed_before);
    let source_after: (String, String, Option<String>, Option<String>, String) =
        rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
            .unwrap()
            .query_row(
                "SELECT image_id, relative_path, external_id, annotation_path, source_version
                 FROM image_sources WHERE image_id = 'demo_002'",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .unwrap();
    assert_eq!(source_after, source_before);
    assert_ne!(
        operation_state(&data_dir, &operation_id).as_deref(),
        Some("completed")
    );
    assert!(transaction_dir.exists());
    drop(restarted);
}

#[tokio::test]
#[ignore]
async fn pending_annotation_mutations_reconcile_only_from_bound_project_evidence_child() {
    let (name, project_id) = unique_project("Task5 pending evidence");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config.clone()).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    let before = read_sample_metadata(&data_dir, &project_id, "demo_001");
    drop(app);

    let committed = insert_pending_annotation_operation(
        &data_dir,
        &project_id,
        "demo_001",
        "submit_annotations",
    );
    insert_project_annotation_event(
        &data_dir,
        &project_id,
        "demo_001",
        &committed,
        "annotation.submit",
    );
    let missing = insert_pending_annotation_operation(
        &data_dir,
        &project_id,
        "demo_002",
        "review_annotations",
    );
    let conflicting =
        insert_pending_annotation_operation(&data_dir, &project_id, "demo_003", "save_annotations");
    insert_project_annotation_event(
        &data_dir,
        &project_id,
        "demo_003",
        &conflicting,
        "qa.review",
    );
    let orphaned_rollback =
        insert_pending_annotation_operation(&data_dir, &project_id, "demo_001", "save_annotations");
    insert_project_annotation_event(
        &data_dir,
        &project_id,
        "demo_001",
        &format!("{orphaned_rollback}:rollback"),
        "annotation.save.rollback",
    );

    let restarted = build_router(config).unwrap();
    assert_eq!(
        operation_state(&data_dir, &committed).as_deref(),
        Some("completed")
    );
    assert_eq!(
        operation_state(&data_dir, &missing).as_deref(),
        Some("indeterminate")
    );
    assert_eq!(
        operation_state(&data_dir, &conflicting).as_deref(),
        Some("indeterminate")
    );
    assert_eq!(
        operation_state(&data_dir, &orphaned_rollback).as_deref(),
        Some("indeterminate")
    );
    assert_eq!(
        read_sample_metadata(&data_dir, &project_id, "demo_001"),
        before,
        "reconciliation must not infer or overwrite sample state"
    );
    drop(restarted);
}

#[test]
fn annotation_native_storage_rejects_links_and_cleans_controlled_temps() {
    run_ignored_test_in_subprocess(
        "annotation_native_storage_rejects_links_and_cleans_controlled_temps_child",
    );
}

#[tokio::test]
#[ignore]
async fn annotation_native_storage_rejects_links_and_cleans_controlled_temps_child() {
    let (name, project_id) = unique_project("Task5 native safety");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;
    clear_annotation_fixture(&data_dir, &project_id, "demo_001");
    let annotations_dir = data_dir
        .join("projects")
        .join(&project_id)
        .join("annotations")
        .join("native");
    let stale_temp = annotations_dir.join(".annotation-stale.tmp");
    let neighbor = annotations_dir.join("neighbor.tmp");
    fs::write(&stale_temp, b"stale").unwrap();
    fs::write(&neighbor, b"keep").unwrap();
    let uri = format!("/api/v1/projects/{project_id}/samples/demo_001/annotations");
    let (read_status, _, read) = router_request(&app, Method::GET, &uri, READER_TOKEN, None).await;
    assert_eq!(read_status, StatusCode::OK, "{read}");
    assert!(!stale_temp.exists());
    assert_eq!(fs::read(&neighbor).unwrap(), b"keep");

    let external_dir = unique_temp_root("task5-native-link-external");
    fs::create_dir_all(&external_dir).unwrap();
    let _external_cleanup = RemoveDirectoryOnDrop(external_dir.clone());
    let sentinel = external_dir.join("sentinel.json");
    fs::write(&sentinel, b"external sentinel").unwrap();
    let native_path = annotations_dir.join("demo_001.json");
    let linked = create_file_link(&sentinel, &native_path).is_ok();
    if linked {
        let (write_status, _, write) = router_json_request_with_headers(
            &app,
            Method::PUT,
            &uri,
            EDITOR_TOKEN,
            &[],
            bbox_annotation_body("linked", 0, "object"),
        )
        .await;
        assert_eq!(write_status, StatusCode::INTERNAL_SERVER_ERROR, "{write}");
        assert_eq!(write["error"]["code"], "storage");
        assert_eq!(fs::read(&sentinel).unwrap(), b"external sentinel");
        let annotation_count: u64 =
            rusqlite::Connection::open(annotation_project_sqlite(&data_dir, &project_id))
                .unwrap()
                .query_row(
                    "SELECT COUNT(*) FROM annotations WHERE image_id = 'demo_001'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(annotation_count, 0);
    }
}

#[tokio::test]
async fn annotation_routes_return_not_found_without_leaking_internal_paths() {
    let (name, project_id) = unique_project("Task5 missing resources");
    let mut config = test_config(Ipv4Addr::LOCALHOST);
    config.reader_token = Some(READER_TOKEN.to_string());
    config.editor_token = Some(EDITOR_TOKEN.to_string());
    config.admin_token = Some(ADMIN_TOKEN.to_string());
    let data_dir = config.data_dir.clone();
    let app = build_router(config).unwrap();
    create_demo_project(&app, &name, &project_id, "yolo-detect", "demo-bbox").await;

    for (method, uri, token, payload) in [
        (
            Method::GET,
            "/api/v1/projects/missing/samples/demo_001/annotations".to_string(),
            READER_TOKEN,
            None,
        ),
        (
            Method::GET,
            format!("/api/v1/projects/{project_id}/samples/missing/annotations"),
            READER_TOKEN,
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/projects/{project_id}/samples/missing/review"),
            EDITOR_TOKEN,
            Some(serde_json::json!({"decision": "approved", "note": ""})),
        ),
    ] {
        let (status, _, body) = router_request(&app, method, &uri, token, payload).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert!(!body
            .to_string()
            .contains(&data_dir.to_string_lossy().to_string()));
        assert!(!body.to_string().contains("sqlite"));
    }
}

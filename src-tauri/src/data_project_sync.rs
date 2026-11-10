//! Cloud synchronization for the data-project repository.
//!
//! The sync boundary deliberately uses a GraphBundle rather than uploading a
//! JSON description of a project.  Commits, refs, dataset membership and
//! metadata are synchronized together, while every image/annotation/metadata
//! object is uploaded and verified by its project-scoped SHA-256 blob.
//!
//! `lib.rs` owns Tauri command registration.  This module exposes plain Rust
//! functions and types so desktop/mobile command wrappers can be added without
//! coupling the HTTP transport to the UI.

use crate::{
    credentials::{self, SessionCredentials},
    data_project_versioning::{
        self, DataProjectBranch, DataProjectGraph, DataProjectMetadata, VersionCommit,
        VersionCommitEntry,
    },
    data_projects,
    domain::AnnotationObject,
    project_fs::{self, ProjectManifest},
    storage::{self, StoredClass, StoredImage},
};
use chrono::{DateTime, SecondsFormat, Utc};
use reqwest::blocking::{Body, Client, RequestBuilder, Response};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDataProject {
    pub id: String,
    pub organization_id: Option<String>,
    pub owner_id: String,
    pub name: String,
    pub description: String,
    pub status: String,
    pub revision: i64,
    pub role: String,
    pub dataset_count: i64,
    pub ref_count: i64,
    pub commit_count: i64,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDatasetMembership {
    pub dataset_key: String,
    pub dataset_id: Option<String>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub position: i32,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteObjectDescriptor {
    pub hash: String,
    pub byte_size: i64,
    pub kind: String,
    pub mime_type: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteCommit {
    pub commit_id: String,
    #[serde(default)]
    pub parent_ids: Vec<String>,
    pub message: String,
    pub tree: Value,
    #[serde(default)]
    pub metadata: Value,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteRef {
    pub name: String,
    pub commit_id: Option<String>,
    pub revision: i64,
    pub updated_by: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct GraphBundle {
    pub project: RemoteDataProject,
    #[serde(default)]
    pub members: Vec<Value>,
    #[serde(default)]
    pub datasets: Vec<RemoteDatasetMembership>,
    #[serde(default)]
    pub commits: Vec<RemoteCommit>,
    #[serde(default)]
    pub refs: Vec<RemoteRef>,
    #[serde(default)]
    pub objects: Vec<RemoteObjectDescriptor>,
    pub remote_revision: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SyncReport {
    pub local_project_id: String,
    pub remote_project_id: String,
    pub remote_revision: i64,
    pub local_head: Option<String>,
    pub remote_head: Option<String>,
    pub outcome: String,
    pub tracking_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub project: RemoteDataProject,
    pub remote_revision: i64,
    pub refs: Vec<RemoteRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDataProjectMember {
    pub user_id: String,
    pub role: String,
    #[serde(default)]
    pub joined_at: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDataProjectMemberRole {
    pub user_id: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDataProjectMemberRemoval {
    pub user_id: String,
    pub deleted: bool,
}

/// UI-safe status for a locally connected project.  It intentionally does
/// not carry the server URL, account identifier, access token, or refresh
/// token; those remain inside the credential-backed sync boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectSyncStatus {
    pub local_project_id: String,
    pub remote_project_id: Option<String>,
    pub remote_revision: Option<i64>,
    pub local_head: Option<String>,
    pub remote_head: Option<String>,
    pub dirty: bool,
    pub state: String,
    pub tracking_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct AuthenticatedClient {
    http: Client,
    session: SessionCredentials,
}

#[derive(Debug, Clone)]
struct LocalDataset {
    id: String,
    metadata_hash: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalCommit {
    id: String,
    parent_id: Option<String>,
    second_parent_id: Option<String>,
    message: String,
    created_at: String,
    dataset_ids: Vec<String>,
    dataset_metadata: BTreeMap<String, String>,
    project_metadata: DataProjectMetadata,
    entries: Vec<data_projects::DataProjectCommitEntry>,
}

#[derive(Debug, Clone)]
struct LocalBranch {
    name: String,
    head_commit_id: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalObject {
    hash: String,
    kind: String,
    byte_size: i64,
}

#[derive(Debug, Clone)]
struct LocalGraph {
    id: String,
    name: String,
    description: String,
    account_scope: Option<String>,
    datasets: Vec<LocalDataset>,
    commits: Vec<LocalCommit>,
    branches: Vec<LocalBranch>,
    head: Option<String>,
    objects: Vec<LocalObject>,
}

#[derive(Debug, Clone)]
struct SyncState {
    remote_project_id: String,
    server_url: String,
    account_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SyncRefBinding {
    remote_name: String,
    local_name: String,
    tracking: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadRelation {
    Equal,
    LocalAhead,
    RemoteAhead,
    Diverged,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct NonMainRefPlan {
    tracking_needed: bool,
    local_changes: bool,
    conflict_ref: Option<String>,
    tracking_ref: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteProjectResponse {
    id: String,
    #[serde(default)]
    organization_id: Option<String>,
    owner_id: String,
    name: String,
    description: String,
    status: String,
    revision: i64,
    role: String,
    dataset_count: i64,
    ref_count: i64,
    commit_count: i64,
    updated_at: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PushRequest {
    expected_remote_revision: i64,
    datasets: Vec<Value>,
    commits: Vec<Value>,
    refs: Vec<Value>,
}

fn now_string() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn account_scope(session: &SessionCredentials) -> String {
    format!(
        "{}\n{}",
        session.server_url.trim_end_matches('/'),
        session.account_id
    )
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
}

fn authenticated_client() -> Result<AuthenticatedClient, String> {
    let session = credentials::refreshed_session()?
        .ok_or_else(|| "登录会话不存在，请先连接云端账户".to_string())?;
    if session.access_token.trim().is_empty() {
        return Err("登录会话没有可用访问令牌".to_string());
    }
    let server = session.server_url.trim_end_matches('/');
    if !(server.starts_with("http://") || server.starts_with("https://")) {
        return Err("云端地址必须使用 http 或 https".to_string());
    }
    let http = Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(120))
        .build()
        .map_err(|error| format!("创建云端同步客户端失败: {error}"))?;
    Ok(AuthenticatedClient { http, session })
}

fn url(client: &AuthenticatedClient, path: &str) -> String {
    format!(
        "{}/api/v1{}",
        client.session.server_url.trim_end_matches('/'),
        path
    )
}

fn authenticated_request(client: &AuthenticatedClient, request: RequestBuilder) -> RequestBuilder {
    request.bearer_auth(&client.session.access_token)
}

fn response_error(mut response: Response, action: &str) -> String {
    let status = response.status();
    let parsed = response.json::<ErrorEnvelope>().ok();
    let message = parsed
        .as_ref()
        .and_then(|error| error.message.as_deref())
        .filter(|message| !message.trim().is_empty())
        .unwrap_or("云端请求失败");
    let code = parsed
        .as_ref()
        .and_then(|error| error.code.as_deref())
        .unwrap_or("REMOTE_ERROR");
    format!("{action}失败 ({} {code}): {message}", status.as_u16())
}

fn decode_json<T: DeserializeOwned>(response: Response, action: &str) -> Result<T, String> {
    if !response.status().is_success() {
        return Err(response_error(response, action));
    }
    response
        .json::<T>()
        .map_err(|error| format!("{action}响应格式无效: {error}"))
}

fn get_json<T: DeserializeOwned>(
    client: &AuthenticatedClient,
    path: &str,
    action: &str,
) -> Result<T, String> {
    let response = authenticated_request(client, client.http.get(url(client, path)))
        .send()
        .map_err(|error| format!("{action}网络请求失败: {error}"))?;
    decode_json(response, action)
}

fn post_json<T: DeserializeOwned>(
    client: &AuthenticatedClient,
    path: &str,
    body: &Value,
    action: &str,
) -> Result<T, String> {
    let response = authenticated_request(client, client.http.post(url(client, path)))
        .json(body)
        .send()
        .map_err(|error| format!("{action}网络请求失败: {error}"))?;
    decode_json(response, action)
}

fn delete_json<T: DeserializeOwned>(
    client: &AuthenticatedClient,
    path: &str,
    action: &str,
) -> Result<T, String> {
    let response = authenticated_request(client, client.http.delete(url(client, path)))
        .send()
        .map_err(|error| format!("{action}网络请求失败: {error}"))?;
    decode_json(response, action)
}

fn graph(client: &AuthenticatedClient, remote_project_id: &str) -> Result<GraphBundle, String> {
    validate_remote_project_id(remote_project_id)?;
    get_json(
        client,
        &format!("/data-projects/{remote_project_id}/graph"),
        "读取云端数据项目",
    )
}

fn validate_remote_project_id(remote_project_id: &str) -> Result<(), String> {
    Uuid::parse_str(remote_project_id)
        .map(|_| ())
        .map_err(|_| "云端数据项目 ID 无效".to_string())
}

fn validate_local_project_id(local_project_id: &str) -> Result<(), String> {
    if local_project_id.trim().is_empty()
        || local_project_id.len() > 256
        || local_project_id == "."
        || local_project_id == ".."
        || local_project_id.contains('/')
        || local_project_id.contains('\\')
        || local_project_id.chars().any(|value| value.is_control())
    {
        return Err("本地数据项目 ID 无效".to_string());
    }
    Ok(())
}

pub fn list_remote_data_projects() -> Result<Vec<RemoteDataProject>, String> {
    let client = authenticated_client()?;
    get_json(&client, "/data-projects", "读取云端数据项目列表")
}

pub fn create_remote_data_project(
    name: &str,
    description: &str,
    organization_id: Option<&str>,
) -> Result<RemoteDataProject, String> {
    let client = authenticated_client()?;
    let organization_id = organization_id
        .map(|value| {
            value
                .parse::<Uuid>()
                .map_err(|_| "organizationId 无效".to_string())
        })
        .transpose()?;
    let body = json!({
        "name": name,
        "description": description,
        "organizationId": organization_id
    });
    post_json(&client, "/data-projects", &body, "创建云端数据项目")
}

pub fn status_remote_data_project(remote_project_id: &str) -> Result<RemoteStatus, String> {
    validate_remote_project_id(remote_project_id)?;
    let client = authenticated_client()?;
    get_json(
        &client,
        &format!("/data-projects/{remote_project_id}/status"),
        "读取云端数据项目状态",
    )
}

pub fn connect_remote_data_project(
    local_project_id: &str,
    remote_project_id: &str,
) -> Result<GraphBundle, String> {
    validate_local_project_id(local_project_id)?;
    validate_remote_project_id(remote_project_id)?;
    let client = authenticated_client()?;
    let bundle = graph(&client, remote_project_id)?;
    ensure_local_scope(local_project_id, &client.session)?;
    if !local_project_exists(local_project_id)? {
        return Err("本地数据项目不存在，无法连接云端项目".to_string());
    }
    bind_local_project_scope(local_project_id, &client.session)?;
    save_sync_state(
        local_project_id,
        &SyncState {
            remote_project_id: remote_project_id.to_string(),
            server_url: client.session.server_url.trim_end_matches('/').to_string(),
            account_id: client.session.account_id.clone(),
        },
    )?;
    Ok(bundle)
}

fn connected_remote_project(
    local_project_id: &str,
    client: &AuthenticatedClient,
) -> Result<String, String> {
    validate_local_project_id(local_project_id)?;
    load_sync_state(local_project_id, &client.session)?
        .map(|state| state.remote_project_id)
        .ok_or_else(|| "本地数据项目尚未连接云端项目".to_string())
}

pub fn list_data_project_cloud_members(
    local_project_id: &str,
) -> Result<Vec<RemoteDataProjectMember>, String> {
    let client = authenticated_client()?;
    let remote_project_id = connected_remote_project(local_project_id, &client)?;
    get_json(
        &client,
        &format!("/data-projects/{remote_project_id}/members"),
        "读取云端数据项目成员",
    )
}

pub fn upsert_data_project_cloud_member(
    local_project_id: &str,
    user_id: &str,
    role: &str,
) -> Result<RemoteDataProjectMemberRole, String> {
    let client = authenticated_client()?;
    let remote_project_id = connected_remote_project(local_project_id, &client)?;
    validate_remote_project_id(&remote_project_id)?;
    Uuid::parse_str(user_id).map_err(|_| "云端成员用户 ID 无效".to_string())?;
    if !matches!(role, "owner" | "admin" | "editor" | "viewer") {
        return Err("云端成员角色无效".to_string());
    }
    post_json(
        &client,
        &format!("/data-projects/{remote_project_id}/members"),
        &json!({ "userId": user_id, "role": role }),
        "更新云端数据项目成员",
    )
}

pub fn remove_data_project_cloud_member(
    local_project_id: &str,
    user_id: &str,
) -> Result<RemoteDataProjectMemberRemoval, String> {
    let client = authenticated_client()?;
    let remote_project_id = connected_remote_project(local_project_id, &client)?;
    validate_remote_project_id(&remote_project_id)?;
    Uuid::parse_str(user_id).map_err(|_| "云端成员用户 ID 无效".to_string())?;
    delete_json(
        &client,
        &format!("/data-projects/{remote_project_id}/members/{user_id}"),
        "移除云端数据项目成员",
    )
}

fn local_data_project_database() -> PathBuf {
    project_fs::workspace_data_root().join("data-projects.sqlite")
}

fn open_local_data_project_database() -> Result<Connection, String> {
    let path = local_data_project_database();
    // The local backend owns first-run migrations.  Do not call its account-
    // filtered listing on every sync read: after a cross-account clone the
    // database intentionally contains trusted remote metadata that the
    // legacy listing path cannot yet interpret.  Existing files are already
    // migrated by the backend/versioning layer before this module touches the
    // graph.
    if !path.exists() {
        let _ = data_projects::list_data_projects()?;
    }
    Connection::open(path).map_err(|error| error.to_string())
}

fn ensure_local_scope(local_project_id: &str, session: &SessionCredentials) -> Result<(), String> {
    let connection = open_local_data_project_database()?;
    // `account_scope` is nullable for legacy/local-only projects.  Decode
    // the row as Option<Option<String>> so an existing NULL is not confused
    // with a missing project (the caller performs that existence check when
    // a connection is requested).
    let scope: Option<Option<String>> = connection
        .query_row(
            "SELECT account_scope FROM data_projects WHERE id = ?1",
            params![local_project_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if scope.flatten()
        .as_deref()
        .is_some_and(|value| value != account_scope(session))
    {
        return Err("本地数据项目属于其他云端账户作用域".to_string());
    }
    Ok(())
}

fn bind_local_project_scope(
    local_project_id: &str,
    session: &SessionCredentials,
) -> Result<(), String> {
    let connection = open_local_data_project_database()?;
    let scope = account_scope(session);
    let updated = connection
        .execute(
            "UPDATE data_projects SET account_scope = ?2, updated_at = ?3 WHERE id = ?1 AND account_scope IS NULL",
            params![local_project_id, scope, now_string()],
        )
        .map_err(|error| format!("绑定本地数据项目账户作用域失败: {error}"))?;
    if updated == 0 {
        let existing: Option<String> = connection
            .query_row(
                "SELECT account_scope FROM data_projects WHERE id = ?1",
                params![local_project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| format!("读取本地数据项目账户作用域失败: {error}"))?;
        if existing.as_deref() != Some(scope.as_str()) {
            return Err("本地数据项目已绑定其他云端账户作用域".to_string());
        }
    }
    Ok(())
}

fn local_graph(local_project_id: &str) -> Result<LocalGraph, String> {
    // The versioning layer owns the authoritative branch/DAG view. Calling
    // export first also upgrades legacy databases with parent2/branch tables;
    // authorization or corruption failures are intentionally propagated so a
    // stale history cannot be exposed through the legacy SQL reader.
    data_project_versioning::export_data_project_graph(local_project_id)
        .map_err(|error| format!("读取本地数据项目版本图失败: {error}"))?;
    let connection = open_local_data_project_database()?;
    let project = connection
        .query_row(
            "SELECT name, description, account_scope, head_commit_id FROM data_projects WHERE id = ?1",
            params![local_project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .map_err(|error| format!("读取本地数据项目失败: {error}"))?;
    let catalog_dataset_ids = connection
        .prepare("SELECT dataset_id FROM data_project_datasets WHERE data_project_id = ?1 ORDER BY dataset_id")
        .map_err(|error| error.to_string())?
        .query_map(params![local_project_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let commit_rows = connection
        .prepare("SELECT id, parent_id, parent2_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json FROM data_project_commits WHERE data_project_id = ?1 ORDER BY created_at, id")
        .map_err(|error| error.to_string())?
        .query_map(params![local_project_id], |row| {
            let dataset_ids: Vec<String> = serde_json::from_str(&row.get::<_, String>(5)?)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            let dataset_metadata: BTreeMap<String, String> = serde_json::from_str(&row.get::<_, String>(6)?)
                .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
            let project_metadata = data_project_versioning::parse_project_metadata_json(
                &row.get::<_, String>(7)?,
            )
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                error,
            ))))?;
            Ok(LocalCommit {
                id: row.get(0)?,
                parent_id: row.get(1)?,
                second_parent_id: row.get(2)?,
                message: row.get(3)?,
                created_at: row.get(4)?,
                dataset_ids,
                dataset_metadata,
                project_metadata,
                entries: Vec::new(),
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    let mut commits = commit_rows;
    for commit in &mut commits {
        commit.entries = connection
            .prepare("SELECT dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height FROM data_project_commit_entries WHERE commit_id = ?1 ORDER BY dataset_id, image_id")
            .map_err(|error| error.to_string())?
            .query_map(params![commit.id], |row| {
                Ok(data_projects::DataProjectCommitEntry {
                    dataset_id: row.get(0)?,
                    image_id: row.get(1)?,
                    file_name: row.get(2)?,
                    image_hash: row.get(3)?,
                    annotation_hash: row.get(4)?,
                    status: row.get(5)?,
                    qa_status: row.get(6)?,
                    split: row.get(7)?,
                    width: row.get(8)?,
                    height: row.get(9)?,
                })
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
    }
    let mut branches = connection
        .prepare(
            "SELECT name, head_commit_id FROM data_project_branches WHERE data_project_id = ?1 ORDER BY name",
        )
        .map_err(|error| error.to_string())?
        .query_map(params![local_project_id], |row| {
            Ok(LocalBranch {
                name: row.get(0)?,
                head_commit_id: row.get(1)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if branches.is_empty() {
        branches.push(LocalBranch {
            name: "main".to_string(),
            head_commit_id: project.3.clone(),
        });
    }
    let head = branches
        .iter()
        .find(|branch| branch.name == "main")
        .map(|branch| branch.head_commit_id.clone())
        .unwrap_or(project.3);
    // Cloud membership is the snapshot of the main ref, not whichever
    // branch happens to be checked out in the local catalog.  A feature-only
    // dataset therefore remains in its feature commit/history while a push
    // from that checkout cannot silently rewrite the cloud main membership.
    let main_snapshot = branches
        .iter()
        .find(|branch| branch.name == "main")
        .and_then(|branch| branch.head_commit_id.as_deref())
        .map(|commit_id| {
            commits
                .iter()
                .find(|commit| commit.id == commit_id)
                .ok_or_else(|| format!("主分支指向不存在的本地提交: {commit_id}"))
        })
        .transpose()?;
    let (main_dataset_ids, main_dataset_metadata) = if let Some(commit) = main_snapshot {
        (commit.dataset_ids.clone(), commit.dataset_metadata.clone())
    } else {
        let active_branch: Option<String> = connection
            .query_row(
                "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
                params![local_project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if active_branch.as_deref().is_some_and(|branch| branch != "main") {
            (Vec::new(), BTreeMap::new())
        } else {
            (catalog_dataset_ids.clone(), BTreeMap::new())
        }
    };
    // The project catalog reflects the active/main checkout, but cloud
    // membership must also advertise datasets that exist only on a feature
    // branch. Otherwise a branch-only group/commit cannot round-trip through
    // clone and the server may reject its tree key as unknown.
    let mut all_dataset_ids = local_graph_dataset_keys_from_parts(
        &catalog_dataset_ids,
        &commits,
        &main_dataset_ids,
    );
    let mut metadata_by_dataset = main_dataset_metadata;
    for commit in &commits {
        for (dataset_id, hash) in &commit.dataset_metadata {
            metadata_by_dataset
                .entry(dataset_id.clone())
                .or_insert_with(|| hash.clone());
        }
    }
    all_dataset_ids.sort();
    let datasets = all_dataset_ids
        .into_iter()
        .map(|id| LocalDataset {
            metadata_hash: metadata_by_dataset.get(&id).cloned(),
            id,
        })
        .collect::<Vec<_>>();
    let referenced_hashes = commits.iter().flat_map(|commit| {
        commit.dataset_metadata.values().cloned().chain(
            commit.entries.iter().flat_map(|entry| [entry.image_hash.clone(), entry.annotation_hash.clone()])
        )
    }).collect::<BTreeSet<_>>();
    let objects = connection
        .prepare("SELECT hash, kind, byte_length FROM data_project_objects ORDER BY hash")
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok(LocalObject {
                hash: row.get(0)?,
                kind: row.get(1)?,
                byte_size: row.get(2)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    // The object directory is workspace-wide. Publishing one project must
    // never grant its cloud members access to another project's objects.
    let objects = project_objects_only(objects, &referenced_hashes)?;
    Ok(LocalGraph {
        id: local_project_id.to_string(),
        name: project.0,
        description: project.1,
        account_scope: project.2,
        datasets,
        commits,
        branches,
        head,
        objects,
    })
}

fn project_objects_only(
    mut objects: Vec<LocalObject>,
    referenced_hashes: &BTreeSet<String>,
) -> Result<Vec<LocalObject>, String> {
    objects.retain(|object| referenced_hashes.contains(&object.hash));
    if objects.len() != referenced_hashes.len() {
        return Err("本项目提交引用的对象缺少完整本地索引，请先修复后再同步".to_string());
    }
    Ok(objects)
}

fn local_object_path(hash: &str) -> PathBuf {
    project_fs::workspace_data_root()
        .join("data-project-objects")
        .join(hash.to_ascii_lowercase())
}

fn canonical_dataset_key(dataset_id: &str, local_to_remote: &BTreeMap<String, String>) -> String {
    local_to_remote
        .get(dataset_id)
        .cloned()
        .unwrap_or_else(|| dataset_id.to_string())
}

fn local_project_metadata_tree(
    metadata: &DataProjectMetadata,
    local_to_remote: &BTreeMap<String, String>,
) -> Value {
    let mut value = serde_json::to_value(metadata).unwrap_or_else(|_| json!({
        "schemaVersion": 1,
        "datasetGroups": []
    }));
    if let Some(groups) = value.get_mut("datasetGroups").and_then(Value::as_array_mut) {
        for group in groups {
            if let Some(keys) = group.get_mut("datasetKeys").and_then(Value::as_array_mut) {
                for key in keys {
                    if let Some(remote) = key
                        .as_str()
                        .map(|key| canonical_dataset_key(key, local_to_remote))
                    {
                        *key = Value::String(remote);
                    }
                }
            }
        }
    }
    value
}

fn local_tree(
    commit: &LocalCommit,
    local_to_remote: &BTreeMap<String, String>,
) -> Value {
    let dataset_ids = commit
        .dataset_ids
        .iter()
        .map(|dataset_id| canonical_dataset_key(dataset_id, local_to_remote))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let dataset_metadata = commit
        .dataset_metadata
        .iter()
        .map(|(dataset_id, hash)| {
            (
                canonical_dataset_key(dataset_id, local_to_remote),
                hash.clone(),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let entries = commit
        .entries
        .iter()
        .cloned()
        .map(|mut entry| {
            entry.dataset_id = canonical_dataset_key(&entry.dataset_id, local_to_remote);
            entry
        })
        .collect::<Vec<_>>();
    json!({
        "datasetIds": dataset_ids,
        "datasetMetadata": dataset_metadata,
        "datasetMetadataHashes": dataset_metadata.clone(),
        "projectMetadata": local_project_metadata_tree(&commit.project_metadata, local_to_remote),
        "entries": entries
    })
}

fn local_push_payload(
    local: &LocalGraph,
    remote: &GraphBundle,
    bindings: &BTreeMap<String, SyncRefBinding>,
    local_to_remote: &BTreeMap<String, String>,
) -> Result<PushRequest, String> {
    let remote_commits = remote
        .commits
        .iter()
        .map(|commit| commit.commit_id.as_str())
        .collect::<BTreeSet<_>>();
    let commits = local
        .commits
        .iter()
        .filter(|commit| !remote_commits.contains(commit.id.as_str()))
        .map(|commit| {
            let mut parent_ids = commit
                .parent_id
                .iter()
                .chain(commit.second_parent_id.iter())
                .cloned()
                .collect::<Vec<_>>();
            parent_ids.dedup();
            json!({
                "commitId": commit.id,
                "parentIds": parent_ids,
                "message": commit.message,
                "tree": local_tree(commit, local_to_remote),
                "metadata": {},
                "createdAt": commit.created_at
            })
        })
        .collect::<Vec<_>>();
    let datasets = local
        .datasets
        .iter()
        .map(|dataset| {
            json!({
                "datasetKey": canonical_dataset_key(&dataset.id, local_to_remote),
                "datasetId": Value::Null,
                "metadata": dataset.metadata_hash.as_ref().map(|hash| json!({
                    "metadataHash": hash
                })).unwrap_or_else(|| json!({})),
                "position": 0
            })
        })
        .collect::<Vec<_>>();
    let mut refs = local
        .branches
        .iter()
        // origin:: refs are local tracking refs for a remote fork and must
        // never be published back as if they were user-owned branches.
        .filter(|branch| !branch.name.starts_with("origin::"))
        .map(|branch| {
            let remote_ref = remote_ref_for_local_branch(remote, &branch.name);
            let expected_commit_id = remote_ref.and_then(|reference| reference.commit_id.clone());
            let name = remote_ref
                .map(|reference| reference.name.clone())
                .unwrap_or_else(|| branch.name.clone());
            let commit_id = branch.head_commit_id.clone();
            if remote_ref
                .and_then(|reference| reference.commit_id.as_ref())
                == commit_id.as_ref()
            {
                return None;
            }
            // An empty local branch has no commit to publish.  Do not create
            // a remote null ref just because the local branch exists; null
            // refs are reserved for an explicit deletion CAS.
            if remote_ref.is_none() && commit_id.is_none() {
                return None;
            }
            Some(json!({
                "name": name,
                "expectedCommitId": expected_commit_id,
                "commitId": commit_id
            }))
        })
        .flatten()
        .collect::<Vec<_>>();

    // A direct local branch that was previously bound to a remote ref is an
    // explicit local deletion when the branch record is gone.  Keep a
    // tracking-only origin:: branch from deleting its remote counterpart.
    for remote_ref in &remote.refs {
        if remote_ref.name == "main"
            || remote_ref_for_local_branch(remote, &remote_ref.name)
                .and_then(|reference| {
                    local.branches.iter().find(|branch| {
                        !branch.name.starts_with("origin::")
                            && remote_ref_for_local_branch(remote, &branch.name)
                                .map(|candidate| candidate.name == reference.name)
                                .unwrap_or(false)
                    })
                })
                .is_some()
        {
            continue;
        }
        if local_tracking_branch(local, &remote_ref.name)?.is_some() {
            continue;
        }
        if let Some(binding) = bindings.get(&remote_ref.name) {
            if !binding.tracking {
                refs.push(json!({
                    "name": remote_ref.name,
                    "expectedCommitId": remote_ref.commit_id,
                    "commitId": Value::Null
                }));
            }
        }
    }

    Ok(PushRequest {
        expected_remote_revision: remote.remote_revision,
        datasets,
        commits,
        refs,
    })
}

fn datasets_match_remote(local: &LocalGraph, remote: &GraphBundle) -> Result<bool, String> {
    let remote_map = remote_dataset_map(&local.id, remote)?;
    let local_ids = local
        .datasets
        .iter()
        .map(|dataset| dataset.id.as_str())
        .collect::<BTreeSet<_>>();
    let remote_ids = remote_map
        .values()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    Ok(local_ids == remote_ids)
}

fn local_ancestor(local: &LocalGraph, descendant: Option<&str>, ancestor: Option<&str>) -> bool {
    let (Some(current), Some(ancestor)) = (descendant, ancestor) else {
        return ancestor.is_none();
    };
    let parents = local
        .commits
        .iter()
        .map(|commit| {
            (
                commit.id.as_str(),
                commit
                    .parent_id
                    .iter()
                    .chain(commit.second_parent_id.iter())
                    .map(String::as_str)
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<HashMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut pending = vec![current];
    while let Some(current) = pending.pop() {
        if current == ancestor {
            return true;
        }
        if !seen.insert(current) {
            continue;
        }
        if let Some(parent_ids) = parents.get(current) {
            pending.extend(parent_ids.iter().copied());
        }
    }
    false
}

fn remote_ancestor(remote: &GraphBundle, descendant: Option<&str>, ancestor: Option<&str>) -> bool {
    let (Some(descendant), Some(ancestor)) = (descendant, ancestor) else {
        return ancestor.is_none();
    };
    let parents = remote
        .commits
        .iter()
        .map(|commit| (commit.commit_id.as_str(), commit.parent_ids.as_slice()))
        .collect::<HashMap<_, _>>();
    let mut seen = BTreeSet::new();
    let mut pending = vec![descendant];
    while let Some(current) = pending.pop() {
        if current == ancestor {
            return true;
        }
        if !seen.insert(current) {
            continue;
        }
        if let Some(parent_ids) = parents.get(current) {
            pending.extend(parent_ids.iter().map(String::as_str));
        }
    }
    false
}

fn head_relation(
    local: &LocalGraph,
    remote: &GraphBundle,
    local_head: Option<&str>,
    remote_head: Option<&str>,
) -> HeadRelation {
    if local_head == remote_head {
        return HeadRelation::Equal;
    }
    // `local_ancestor(local, local_head, remote_head)` means the remote head
    // is reachable from the local head, so the local side is ahead.  The
    // inverse check uses the complete remote graph for a remote-only commit.
    let local_ahead = local_ancestor(local, local_head, remote_head);
    let remote_ahead = remote_ancestor(remote, remote_head, local_head);
    match (local_ahead, remote_ahead) {
        (true, false) => HeadRelation::LocalAhead,
        (false, true) => HeadRelation::RemoteAhead,
        _ => HeadRelation::Diverged,
    }
}

fn remote_ref_for_local_branch<'a>(
    remote: &'a GraphBundle,
    local_name: &str,
) -> Option<&'a RemoteRef> {
    remote.refs.iter().find(|reference| {
        reference.name == local_name
            || safe_local_branch_name(&reference.name)
                .map(|name| name == local_name)
                .unwrap_or(false)
    })
}

fn tracking_branch_name(remote_name: &str) -> Result<String, String> {
    Ok(format!(
        "origin::{}",
        safe_local_branch_name(remote_name)?
    ))
}

fn local_tracking_branch<'a>(local: &'a LocalGraph, remote_name: &str) -> Result<Option<&'a LocalBranch>, String> {
    let tracking_name = tracking_branch_name(remote_name)?;
    Ok(local.branches.iter().find(|branch| branch.name == tracking_name))
}

fn inspect_non_main_refs(
    local: &LocalGraph,
    remote: &GraphBundle,
    bindings: &BTreeMap<String, SyncRefBinding>,
) -> Result<NonMainRefPlan, String> {
    let mut plan = NonMainRefPlan::default();

    for remote_ref in remote.refs.iter().filter(|reference| reference.name != "main") {
        if let Some(local_branch) = remote_ref_for_local_branch(remote, &remote_ref.name)
            .and_then(|reference| {
                local.branches.iter().find(|branch| {
                    !branch.name.starts_with("origin::")
                        && remote_ref_for_local_branch(remote, &branch.name)
                            .map(|candidate| candidate.name == reference.name)
                            .unwrap_or(false)
                })
            })
        {
            let relation = head_relation(
                local,
                remote,
                local_branch.head_commit_id.as_deref(),
                remote_ref.commit_id.as_deref(),
            );
            if remote_ref.commit_id.is_none()
                && bindings
                    .get(&remote_ref.name)
                    .is_some_and(|binding| !binding.tracking)
            {
                plan.conflict_ref = Some(tracking_branch_name(&remote_ref.name)?);
                continue;
            }
            match relation {
                HeadRelation::Equal => {}
                HeadRelation::LocalAhead => plan.local_changes = true,
                HeadRelation::RemoteAhead | HeadRelation::Diverged => {
                    plan.tracking_needed = true;
                    if plan.tracking_ref.is_none() {
                        plan.tracking_ref = Some(tracking_branch_name(&remote_ref.name)?);
                    }
                    if plan.conflict_ref.is_none() {
                        plan.conflict_ref = Some(tracking_branch_name(&remote_ref.name)?);
                    }
                }
            }
            continue;
        }

        let tracking = local_tracking_branch(local, &remote_ref.name)?;
        if tracking.is_some() {
            if tracking
                .map(|branch| branch.head_commit_id != remote_ref.commit_id)
                .unwrap_or(false)
            {
                plan.tracking_needed = true;
                if plan.tracking_ref.is_none() {
                    plan.tracking_ref = Some(tracking_branch_name(&remote_ref.name)?);
                }
            }
        } else if let Some(binding) = bindings.get(&remote_ref.name) {
            if !binding.tracking {
                // The direct branch disappeared locally while the remote ref
                // still exists: this is a pending remote deletion, not a
                // reason to materialize the remote branch over the checkout.
                plan.local_changes = true;
            } else {
                plan.tracking_needed = true;
                if plan.tracking_ref.is_none() {
                    plan.tracking_ref = Some(tracking_branch_name(&remote_ref.name)?);
                }
            }
        } else {
            plan.tracking_needed = true;
            if plan.tracking_ref.is_none() {
                plan.tracking_ref = Some(tracking_branch_name(&remote_ref.name)?);
            }
        }
    }

    for local_branch in local
        .branches
        .iter()
        .filter(|branch| !branch.name.starts_with("origin::") && branch.name != "main")
    {
        if remote_ref_for_local_branch(remote, &local_branch.name).is_some() {
            continue;
        }
        // A direct branch which was previously bound to a remote ref must not
        // silently recreate a branch that another client deleted.  A fresh
        // local branch has no binding and is safe to publish.
        let previous_direct = bindings.values().find(|binding| {
            !binding.tracking && binding.local_name == local_branch.name
        });
        if let Some(binding) = previous_direct {
            if plan.conflict_ref.is_none() {
                plan.conflict_ref = Some(binding.local_name.clone());
            }
        } else {
            plan.local_changes = true;
        }
    }

    Ok(plan)
}

fn sync_database_path() -> PathBuf {
    project_fs::workspace_data_root().join("data-project-sync.sqlite")
}

fn open_sync_database() -> Result<Connection, String> {
    let path = sync_database_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS data_project_sync_state (
                local_project_id TEXT PRIMARY KEY,
                remote_project_id TEXT NOT NULL,
                server_url TEXT NOT NULL,
                account_id TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS data_project_sync_refs (
                local_project_id TEXT NOT NULL,
                ref_name TEXT NOT NULL,
                commit_id TEXT,
                updated_at TEXT NOT NULL,
                local_ref_name TEXT NOT NULL DEFAULT '',
                tracking INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY(local_project_id, ref_name)
            );
            CREATE TABLE IF NOT EXISTS data_project_sync_commits (
                local_project_id TEXT NOT NULL,
                commit_id TEXT NOT NULL,
                parent_ids_json TEXT NOT NULL,
                tree_json TEXT NOT NULL,
                metadata_json TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY(local_project_id, commit_id)
            );
            CREATE TABLE IF NOT EXISTS data_project_sync_datasets (
                local_project_id TEXT NOT NULL,
                remote_dataset_key TEXT NOT NULL,
                local_dataset_id TEXT NOT NULL,
                PRIMARY KEY(local_project_id, remote_dataset_key)
            );
            "#,
        )
        .map_err(|error| error.to_string())?;
    let columns = connection
        .prepare("PRAGMA table_info(data_project_sync_refs)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !columns.iter().any(|column| column == "local_ref_name") {
        connection
            .execute(
                "ALTER TABLE data_project_sync_refs ADD COLUMN local_ref_name TEXT NOT NULL DEFAULT ''",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if !columns.iter().any(|column| column == "tracking") {
        connection
            .execute(
                "ALTER TABLE data_project_sync_refs ADD COLUMN tracking INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(connection)
}

fn save_sync_state(local_project_id: &str, state: &SyncState) -> Result<(), String> {
    let connection = open_sync_database()?;
    connection
        .execute(
            "INSERT INTO data_project_sync_state (local_project_id, remote_project_id, server_url, account_id, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(local_project_id) DO UPDATE SET remote_project_id = excluded.remote_project_id, server_url = excluded.server_url, account_id = excluded.account_id, updated_at = excluded.updated_at",
            params![local_project_id, state.remote_project_id, state.server_url, state.account_id, now_string()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn load_sync_state(
    local_project_id: &str,
    session: &SessionCredentials,
) -> Result<Option<SyncState>, String> {
    let connection = open_sync_database()?;
    let state = connection
        .query_row(
            "SELECT remote_project_id, server_url, account_id FROM data_project_sync_state WHERE local_project_id = ?1",
            params![local_project_id],
            |row| {
                Ok(SyncState {
                    remote_project_id: row.get(0)?,
                    server_url: row.get(1)?,
                    account_id: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(state) = state else {
        return Ok(None);
    };
    if state.server_url.trim_end_matches('/') != session.server_url.trim_end_matches('/')
        || state.account_id != session.account_id
    {
        return Err("本地数据项目绑定了其他云端账户作用域".to_string());
    }
    validate_remote_project_id(&state.remote_project_id)?;
    Ok(Some(state))
}

fn load_ref_bindings(
    local_project_id: &str,
) -> Result<BTreeMap<String, SyncRefBinding>, String> {
    let connection = open_sync_database()?;
    let mut statement = connection
        .prepare(
            "SELECT ref_name, local_ref_name, tracking FROM data_project_sync_refs WHERE local_project_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![local_project_id], |row| {
            let remote_name: String = row.get(0)?;
            let local_name: String = row.get(1)?;
            let tracking: i64 = row.get(2)?;
            Ok(SyncRefBinding {
                remote_name,
                local_name,
                tracking: tracking != 0,
            })
        })
        .map_err(|error| error.to_string())?;
    let mut bindings = BTreeMap::new();
    for row in rows {
        let binding = row.map_err(|error| error.to_string())?;
        if !binding.remote_name.trim().is_empty() {
            bindings.insert(binding.remote_name.clone(), binding);
        }
    }
    Ok(bindings)
}

fn save_full_graph(
    local_project_id: &str,
    bundle: &GraphBundle,
    local: Option<&LocalGraph>,
    track_main: bool,
    track_non_main: bool,
) -> Result<(), String> {
    let mut connection = open_sync_database()?;
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM data_project_sync_commits WHERE local_project_id = ?1",
        params![local_project_id],
    )
    .map_err(|error| error.to_string())?;
    tx.execute(
        "DELETE FROM data_project_sync_refs WHERE local_project_id = ?1",
        params![local_project_id],
    )
    .map_err(|error| error.to_string())?;
    for commit in &bundle.commits {
        tx.execute(
            "INSERT INTO data_project_sync_commits (local_project_id, commit_id, parent_ids_json, tree_json, metadata_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                local_project_id,
                commit.commit_id,
                serde_json::to_string(&commit.parent_ids).map_err(|error| error.to_string())?,
                serde_json::to_string(&commit.tree).map_err(|error| error.to_string())?,
                serde_json::to_string(&commit.metadata).map_err(|error| error.to_string())?,
                commit.created_at
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    for reference in &bundle.refs {
        let prefer_tracking = if reference.name == "main" {
            track_main
        } else {
            track_non_main
        };
        let direct = local.and_then(|graph| {
            graph.branches.iter().find(|branch| {
                !branch.name.starts_with("origin::")
                    && remote_ref_for_local_branch(bundle, &branch.name)
                        .map(|remote| remote.name == reference.name)
                        .unwrap_or(false)
            })
        });
        let tracking = tracking_branch_name(&reference.name).ok().and_then(|name| {
            local.and_then(|graph| {
                graph
                    .branches
                    .iter()
                    .find(|branch| branch.name == name)
            })
        });
        let (local_ref_name, tracking) = if prefer_tracking {
            tracking
                .map(|branch| (branch.name.clone(), true))
                .or_else(|| direct.map(|branch| (branch.name.clone(), false)))
        } else {
            direct
                .map(|branch| (branch.name.clone(), false))
                .or_else(|| tracking.map(|branch| (branch.name.clone(), true)))
        }
        .unwrap_or_else(|| {
            (
                tracking_branch_name(&reference.name)
                    .unwrap_or_else(|_| format!("origin::{}", reference.name)),
                true,
            )
        });
        tx.execute(
            "INSERT INTO data_project_sync_refs (local_project_id, ref_name, commit_id, updated_at, local_ref_name, tracking) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                local_project_id,
                reference.name,
                reference.commit_id,
                now_string(),
                local_ref_name,
                if tracking { 1_i64 } else { 0_i64 }
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())?;
    Ok(())
}

fn ensure_local_clean(local_project_id: &str) -> Result<(), String> {
    let tree = data_projects::get_data_project_working_tree(local_project_id)?;
    if !tree.changes.is_empty() {
        return Err("本地数据项目工作区有未提交变化，拒绝云端覆盖；请先提交或另存分支".to_string());
    }
    Ok(())
}

fn object_descriptor_map(bundle: &GraphBundle) -> HashMap<String, RemoteObjectDescriptor> {
    bundle
        .objects
        .iter()
        .cloned()
        .map(|object| (object.hash.to_ascii_lowercase(), object))
        .collect()
}

fn hash_file(path: &Path) -> Result<(String, i64), String> {
    let mut input = File::open(path).map_err(|error| format!("打开对象失败: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    let mut size = 0_i64;
    loop {
        let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size = size.saturating_add(read as i64);
    }
    Ok((format!("{:x}", hasher.finalize()), size))
}

fn upload_object(
    client: &AuthenticatedClient,
    remote_project_id: &str,
    object: &LocalObject,
) -> Result<(), String> {
    if !valid_hash(&object.hash) {
        return Err(format!("本地对象 hash 无效: {}", object.hash));
    }
    let path = local_object_path(&object.hash);
    let (hash, size) = hash_file(&path)?;
    if hash != object.hash.to_ascii_lowercase() || size != object.byte_size {
        return Err(format!("本地对象校验失败: {}", object.hash));
    }
    let file = File::open(&path).map_err(|error| error.to_string())?;
    let response = authenticated_request(
        client,
        client.http.put(url(
            client,
            &format!("/data-projects/{remote_project_id}/objects/{hash}"),
        )),
    )
    .header("X-Object-Size", size.to_string())
    .header("X-Object-Kind", match object.kind.as_str() {
        "image" | "annotation" | "dataset-metadata" | "project-metadata" => object.kind.as_str(),
        _ => "other",
    })
    .header("Content-Type", "application/octet-stream")
    .body(Body::sized(file, u64::try_from(size).map_err(|_| "本地对象大小无效".to_string())?))
    .send()
    .map_err(|error| format!("上传项目对象失败: {error}"))?;
    if !response.status().is_success() {
        return Err(response_error(response, "上传项目对象"));
    }
    Ok(())
}

fn download_object(
    client: &AuthenticatedClient,
    remote_project_id: &str,
    object: &RemoteObjectDescriptor,
    destination: &Path,
) -> Result<(), String> {
    if !valid_hash(&object.hash) || object.byte_size < 0 {
        return Err("云端对象描述无效".to_string());
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temp = destination.with_extension(format!("part-{}", Uuid::new_v4()));
    let mut response = authenticated_request(
        client,
        client.http.get(url(
            client,
            &format!("/data-projects/{remote_project_id}/objects/{}", object.hash),
        )),
    )
    .send()
    .map_err(|error| format!("下载项目对象失败: {error}"))?;
    if !response.status().is_success() {
        return Err(response_error(response, "下载项目对象"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|error| format!("创建对象临时文件失败: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    let mut size = 0_i64;
    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("读取项目对象失败: {error}"))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as i64);
        if size > object.byte_size {
            let _ = fs::remove_file(&temp);
            return Err("云端对象超过声明大小".to_string());
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| error.to_string())?;
    }
    file.flush().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    let hash = format!("{:x}", hasher.finalize());
    if hash != object.hash.to_ascii_lowercase() || size != object.byte_size {
        let _ = fs::remove_file(&temp);
        return Err("云端对象 SHA-256 或大小校验失败".to_string());
    }
    fs::rename(&temp, destination).map_err(|error| error.to_string())?;
    Ok(())
}

fn safe_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if path.is_absolute() {
        return Err("远端文件名不能是绝对路径".to_string());
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => safe.push(value),
            _ => return Err("远端文件名包含非法路径片段".to_string()),
        }
    }
    if safe.as_os_str().is_empty() {
        return Err("远端文件名为空".to_string());
    }
    Ok(safe)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MetadataPayload {
    manifest: ProjectManifest,
    #[serde(default)]
    classes: Vec<StoredClassPayload>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredClassPayload {
    id: u32,
    label: String,
    #[serde(default)]
    color: String,
}

fn read_metadata(hash: &str) -> Result<MetadataPayload, String> {
    let path = local_object_path(hash);
    let data = fs::read_to_string(path).map_err(|error| error.to_string())?;
    serde_json::from_str(&data).map_err(|error| format!("数据集元数据无效: {error}"))
}

fn annotation_objects(hash: &str) -> Result<(Vec<AnnotationObject>, bool, String), String> {
    let data = fs::read_to_string(local_object_path(hash)).map_err(|error| error.to_string())?;
    let value: Value = serde_json::from_str(&data).map_err(|error| error.to_string())?;
    let objects =
        serde_json::from_value(value.get("objects").cloned().unwrap_or_else(|| json!([])))
            .map_err(|error| format!("标签对象无效: {error}"))?;
    let verified = value
        .get("verified")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let revision = value
        .get("revision")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((objects, verified, revision))
}

fn bind_cloned_dataset_remote_config(
    sqlite_path: &Path,
    session: &SessionCredentials,
    remote_project_id: &str,
    local_dataset_id: &str,
) -> Result<(), String> {
    let connection = Connection::open(sqlite_path).map_err(|error| error.to_string())?;
    let now = now_string();
    connection
        .execute(
            "INSERT INTO remote_project_configs (project_id, server_url, remote_project_id, device_id, cache_policy, auto_sync, created_at, updated_at, account_id) VALUES (?1, ?2, ?3, ?4, 'on_demand', 0, ?5, ?5, ?6) ON CONFLICT(project_id) DO UPDATE SET server_url = excluded.server_url, remote_project_id = excluded.remote_project_id, updated_at = excluded.updated_at, account_id = excluded.account_id",
            params![
                local_dataset_id,
                session.server_url.trim_end_matches('/'),
                remote_project_id,
                format!("data-project-{remote_project_id}"),
                now,
                session.account_id
            ],
        )
        .map_err(|error| format!("绑定克隆数据集云端作用域失败: {error}"))?;
    Ok(())
}

fn tree_entries(tree: &Value) -> Result<Vec<data_projects::DataProjectCommitEntry>, String> {
    serde_json::from_value(tree.get("entries").cloned().unwrap_or_else(|| json!([])))
        .map_err(|error| format!("提交树条目无效: {error}"))
}

fn tree_dataset_metadata(tree: &Value) -> BTreeMap<String, String> {
    for field in ["datasetMetadata", "datasetMetadataHashes"] {
        if let Some(metadata) = tree
            .get(field)
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        {
            return metadata;
        }
    }
    BTreeMap::new()
}

fn tree_dataset_keys(tree: &Value) -> Result<BTreeSet<String>, String> {
    let mut keys = tree_dataset_metadata(tree)
        .into_keys()
        .collect::<BTreeSet<_>>();
    if let Some(dataset_ids) = tree.get("datasetIds") {
        let dataset_ids = dataset_ids
            .as_array()
            .ok_or_else(|| "提交树 datasetIds 格式无效".to_string())?;
        for dataset_id in dataset_ids {
            let dataset_id = dataset_id
                .as_str()
                .ok_or_else(|| "提交树 datasetIds 必须是字符串".to_string())?;
            if dataset_id.trim().is_empty() {
                return Err("提交树包含空数据集 ID".to_string());
            }
            keys.insert(dataset_id.to_string());
        }
    }
    for entry in tree_entries(tree)? {
        if entry.dataset_id.trim().is_empty() {
            return Err("提交树包含空数据集条目".to_string());
        }
        keys.insert(entry.dataset_id);
    }
    if let Some(groups) = tree
        .get("projectMetadata")
        .and_then(|value| value.get("datasetGroups"))
        .and_then(Value::as_array)
    {
        for group in groups {
            if let Some(dataset_keys) = group.get("datasetKeys").and_then(Value::as_array) {
                for dataset_key in dataset_keys {
                    let dataset_key = dataset_key
                        .as_str()
                        .ok_or_else(|| "提交树 dataset group key 必须是字符串".to_string())?;
                    if dataset_key.trim().is_empty() {
                        return Err("提交树 dataset group 包含空数据集 ID".to_string());
                    }
                    keys.insert(dataset_key.to_string());
                }
            }
        }
    }
    Ok(keys)
}

fn dataset_snapshot_trees(bundle: &GraphBundle) -> Result<BTreeMap<String, Value>, String> {
    let mut snapshots = BTreeMap::new();
    for commit in &bundle.commits {
        let keys = tree_dataset_keys(&commit.tree)?;
        let metadata = tree_dataset_metadata(&commit.tree);
        for key in keys {
            // Prefer a tree that carries the metadata hash. A later commit
            // may mention a dataset in datasetIds while relying on the
            // metadata captured by an earlier snapshot.
            if metadata.contains_key(&key) || !snapshots.contains_key(&key) {
                snapshots.insert(key, commit.tree.clone());
            }
        }
    }
    Ok(snapshots)
}

fn materialize_dataset(
    local_dataset_id: &str,
    remote_dataset_key: &str,
    tree: &Value,
    client: &AuthenticatedClient,
    remote_project_id: &str,
    object_map: &HashMap<String, RemoteObjectDescriptor>,
) -> Result<(), String> {
    let metadata_hash = tree_dataset_metadata(tree)
        .get(remote_dataset_key)
        .cloned()
        .ok_or_else(|| format!("提交树缺少数据集元数据: {remote_dataset_key}"))?;
    let metadata_object = object_map
        .get(&metadata_hash.to_ascii_lowercase())
        .ok_or_else(|| format!("缺少数据集元数据对象: {metadata_hash}"))?;
    let metadata_path = local_object_path(&metadata_hash);
    if !metadata_path.exists() {
        download_object(client, remote_project_id, metadata_object, &metadata_path)?;
    }
    let metadata = read_metadata(&metadata_hash)?;
    let paths = project_fs::ensure_workspace_project_dirs(local_dataset_id)?;
    let mut manifest = metadata.manifest;
    manifest.id = local_dataset_id.to_string();
    manifest.source_dataset_key = format!("remote-clone:{remote_project_id}:{remote_dataset_key}");
    manifest.root_path = paths.root.to_string_lossy().to_string();
    let classes = metadata
        .classes
        .into_iter()
        .map(|class| StoredClass {
            id: class.id,
            label: class.label,
            color: class.color,
        })
        .collect::<Vec<_>>();
    let entries = tree_entries(tree)?
        .into_iter()
        .filter(|entry| entry.dataset_id == remote_dataset_key)
        .collect::<Vec<_>>();
    let mut images = Vec::with_capacity(entries.len());
    for entry in &entries {
        let relative = safe_relative_path(&entry.file_name)?;
        let image_path = paths.raw.join(relative);
        let object = object_map
            .get(&entry.image_hash.to_ascii_lowercase())
            .ok_or_else(|| format!("缺少图像对象: {}", entry.image_hash))?;
        if !image_path.exists() {
            download_object(client, remote_project_id, object, &image_path)?;
        }
        images.push(StoredImage {
            id: entry.image_id.clone(),
            file_name: entry.file_name.clone(),
            width: entry.width,
            height: entry.height,
            split: entry.split.clone(),
            status: entry.status.clone(),
            qa_status: entry.qa_status.clone(),
            review_note: None,
        });
    }
    manifest.image_count = images.len() as u32;
    manifest.class_count = classes.len() as u32;
    project_fs::write_manifest(&manifest)?;
    storage::initialize_project_database(&paths.sqlite)?;
    // Keep a materialized clone protected by the existing dataset account
    // guards.  The metadata bytes and object hashes remain untouched.
    bind_cloned_dataset_remote_config(
        &paths.sqlite,
        &client.session,
        remote_project_id,
        local_dataset_id,
    )?;
    storage::upsert_project_index(&paths.sqlite, &manifest, &images, &classes)?;
    let mut connection = Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    for entry in entries {
        let object = object_map
            .get(&entry.annotation_hash.to_ascii_lowercase())
            .ok_or_else(|| format!("缺少标签对象: {}", entry.annotation_hash))?;
        let annotation_path = local_object_path(&entry.annotation_hash);
        if !annotation_path.exists() {
            download_object(client, remote_project_id, object, &annotation_path)?;
        }
        let (objects, verified, revision) = annotation_objects(&entry.annotation_hash)?;
        let object_json = serde_json::to_string(&objects).map_err(|error| error.to_string())?;
        let now = now_string();
        connection
            .execute(
                "INSERT INTO annotations (id, image_id, revision, object_json, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(id) DO UPDATE SET image_id = excluded.image_id, revision = excluded.revision, object_json = excluded.object_json, updated_at = excluded.updated_at",
                params![entry.image_id, entry.image_id, revision, object_json, now],
            )
            .map_err(|error| error.to_string())?;
        if verified {
            connection
                .execute(
                    "INSERT INTO image_verifications (image_id, verified_at) VALUES (?1, ?2) ON CONFLICT(image_id) DO UPDATE SET verified_at = excluded.verified_at",
                    params![entry.image_id, now],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn materialize_remote_dataset_snapshots(
    local: Option<&LocalGraph>,
    bundle: &GraphBundle,
    current_map: &BTreeMap<String, String>,
    graph_map: &BTreeMap<String, String>,
    client: &AuthenticatedClient,
    remote_project_id: &str,
    track_main: bool,
) -> Result<(), String> {
    let snapshots = dataset_snapshot_trees(bundle)?;
    let main_commit = remote_main_head(bundle)
        .and_then(|head| {
            bundle
                .commits
                .iter()
                .find(|commit| commit.commit_id == head.as_str())
        });
    let main_keys = main_commit
        .map(|commit| tree_dataset_keys(&commit.tree))
        .transpose()?;
    let object_map = object_descriptor_map(bundle);
    for (remote_dataset_key, local_dataset_id) in graph_map {
        let current_membership = current_map.contains_key(remote_dataset_key);
        let tree = if current_membership && !track_main {
            main_commit
                .zip(main_keys.as_ref())
                .filter(|(_, keys)| keys.contains(remote_dataset_key))
                .map(|(commit, _)| &commit.tree)
                .or_else(|| snapshots.get(remote_dataset_key))
        } else {
            snapshots.get(remote_dataset_key)
        };
        let Some(tree) = tree else {
            // A current membership without a commit tree has no image/label
            // snapshot to materialize yet; keep the membership for a later
            // commit instead of inventing an empty dataset.
            continue;
        };
        let existing_local = local
            .map(|graph| graph.datasets.iter().any(|dataset| dataset.id == *local_dataset_id))
            .unwrap_or(false)
            || project_fs::project_paths(local_dataset_id).root.exists();
        if track_main && current_membership && existing_local {
            // A divergent pull must preserve the user's active dataset tree;
            // the imported commit graph and object store still retain the
            // remote snapshot for an explicit tracking-branch checkout.
            continue;
        }
        if !current_membership && existing_local {
            // Historical/feature-only datasets may already have a local
            // materialization. Never overwrite it from a remote branch pull.
            continue;
        }
        materialize_dataset(
            local_dataset_id,
            remote_dataset_key,
            tree,
            client,
            remote_project_id,
            &object_map,
        )?;
    }
    Ok(())
}

fn remote_dataset_map(
    local_project_id: &str,
    bundle: &GraphBundle,
) -> Result<BTreeMap<String, String>, String> {
    let stored = load_dataset_key_bindings(local_project_id)?;
    let mut map = BTreeMap::new();
    for dataset in &bundle.datasets {
        let local = stored
            .get(&dataset.dataset_key)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "remote-{}",
                    format!(
                        "{:x}",
                        Sha256::digest(
                            format!("{}\n{}", local_project_id, dataset.dataset_key).as_bytes()
                        )
                    )
                )
            });
        map.insert(dataset.dataset_key.clone(), local);
    }
    Ok(map)
}

fn load_dataset_key_bindings(
    local_project_id: &str,
) -> Result<BTreeMap<String, String>, String> {
    let connection = open_sync_database()?;
    let mut statement = connection
        .prepare(
            "SELECT remote_dataset_key, local_dataset_id FROM data_project_sync_datasets WHERE local_project_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![local_project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(|error| error.to_string())
}

fn remote_graph_dataset_map(
    local_project_id: &str,
    bundle: &GraphBundle,
    current: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, String> {
    let stored = load_dataset_key_bindings(local_project_id)?;
    let mut map = current.clone();
    let keys = bundle
        .commits
        .iter()
        .map(|commit| tree_dataset_keys(&commit.tree))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    for remote_dataset_key in keys {
        if map.contains_key(&remote_dataset_key) {
            continue;
        }
        let local_dataset_id = stored
            .get(&remote_dataset_key)
            .cloned()
            .unwrap_or_else(|| {
                format!(
                    "remote-{}",
                    format!(
                        "{:x}",
                        Sha256::digest(
                            format!("{}\n{}", local_project_id, remote_dataset_key).as_bytes()
                        )
                    )
                )
            });
        map.insert(remote_dataset_key, local_dataset_id);
    }
    Ok(map)
}

fn save_remote_dataset_map(
    local_project_id: &str,
    map: &BTreeMap<String, String>,
) -> Result<(), String> {
    let connection = open_sync_database()?;
    for (remote, local) in map {
        connection
            .execute(
                "INSERT INTO data_project_sync_datasets (local_project_id, remote_dataset_key, local_dataset_id) VALUES (?1, ?2, ?3) ON CONFLICT(local_project_id, remote_dataset_key) DO UPDATE SET local_dataset_id = excluded.local_dataset_id",
                params![local_project_id, remote, local],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn local_graph_dataset_keys(local: &LocalGraph) -> BTreeSet<String> {
    let mut keys = local
        .datasets
        .iter()
        .map(|dataset| dataset.id.clone())
        .collect::<BTreeSet<_>>();
    for commit in &local.commits {
        keys.extend(commit.dataset_ids.iter().cloned());
        keys.extend(commit.dataset_metadata.keys().cloned());
        keys.extend(commit.project_metadata.dataset_groups.iter().flat_map(|group| {
            group.dataset_keys.iter().cloned()
        }));
        keys.extend(commit.entries.iter().map(|entry| entry.dataset_id.clone()));
    }
    keys
}

fn local_graph_dataset_keys_from_parts(
    catalog_dataset_ids: &[String],
    commits: &[LocalCommit],
    main_dataset_ids: &[String],
) -> Vec<String> {
    let mut keys = catalog_dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    keys.extend(main_dataset_ids.iter().cloned());
    for commit in commits {
        keys.extend(commit.dataset_ids.iter().cloned());
        keys.extend(commit.dataset_metadata.keys().cloned());
        keys.extend(commit.project_metadata.dataset_groups.iter().flat_map(|group| {
            group.dataset_keys.iter().cloned()
        }));
        keys.extend(commit.entries.iter().map(|entry| entry.dataset_id.clone()));
    }
    keys.into_iter().collect()
}

fn canonical_dataset_map(
    local_project_id: &str,
    local: &LocalGraph,
    remote: &GraphBundle,
) -> Result<BTreeMap<String, String>, String> {
    let current = remote_dataset_map(local_project_id, remote)?;
    let mut remote_to_local = remote_graph_dataset_map(local_project_id, remote, &current)?;
    let persisted = load_dataset_key_bindings(local_project_id)?;
    let local_keys = local_graph_dataset_keys(local);

    // A locally-originated project historically had no sync mapping row. If
    // its local dataset ID is also present as a remote key, retain that
    // identity instead of assigning the clone-style deterministic ID.
    for local_key in &local_keys {
        if remote_to_local.contains_key(local_key) && !persisted.contains_key(local_key) {
            remote_to_local.insert(local_key.clone(), local_key.clone());
        }
    }

    let mut local_to_remote = BTreeMap::new();
    let mut remote_owner = BTreeMap::new();
    for (remote_key, local_id) in remote_to_local {
        if let Some(previous) = local_to_remote.insert(local_id.clone(), remote_key.clone()) {
            return Err(format!(
                "本地数据集映射冲突: {local_id} 同时对应 {previous} 和 {remote_key}"
            ));
        }
        remote_owner.insert(remote_key, local_id);
    }
    for local_key in local_keys {
        if local_to_remote.contains_key(&local_key) {
            continue;
        }
        if let Some(owner) = remote_owner.get(&local_key) {
            return Err(format!(
                "本地数据集 ID 与云端 canonical key 冲突: {local_key} -> {owner}"
            ));
        }
        local_to_remote.insert(local_key.clone(), local_key);
    }
    Ok(local_to_remote)
}

fn save_local_dataset_bindings(
    local_project_id: &str,
    local: &LocalGraph,
    remote: &GraphBundle,
) -> Result<(), String> {
    let current = remote_dataset_map(local_project_id, remote)?;
    let mut graph_map = remote_graph_dataset_map(local_project_id, remote, &current)?;
    let canonical = canonical_dataset_map(local_project_id, local, remote)?;
    for (local_key, remote_key) in canonical {
        if graph_map.contains_key(&remote_key) {
            // Persist the reverse half of the canonical mapping after a
            // successful push. Future payloads can then translate local IDs
            // back to the exact server keys without changing blob hashes.
            graph_map.insert(remote_key, local_key);
        }
    }
    save_remote_dataset_map(local_project_id, &graph_map)
}

fn rewrite_tree(tree: &Value, dataset_map: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut tree = tree.clone();
    if let Some(dataset_ids) = tree.get_mut("datasetIds").and_then(Value::as_array_mut) {
        for id in dataset_ids {
            if let Some(value) = id.as_str().and_then(|key| dataset_map.get(key)) {
                *id = Value::String(value.clone());
            }
        }
    }
    for field in ["datasetMetadata", "datasetMetadataHashes"] {
        if let Some(metadata) = tree.get_mut(field).and_then(Value::as_object_mut) {
            let original = std::mem::take(metadata);
            for (key, value) in original {
                metadata.insert(dataset_map.get(&key).cloned().unwrap_or(key), value);
            }
        }
    }
    if let Some(entries) = tree.get_mut("entries").and_then(Value::as_array_mut) {
        for entry in entries {
            if let Some(dataset_id) = entry.get_mut("datasetId") {
                if let Some(local) = dataset_id.as_str().and_then(|key| dataset_map.get(key)) {
                    *dataset_id = Value::String(local.clone());
                }
            }
        }
    }
    if let Some(groups) = tree
        .get_mut("projectMetadata")
        .and_then(|value| value.get_mut("datasetGroups"))
        .and_then(Value::as_array_mut)
    {
        for group in groups {
            if let Some(keys) = group.get_mut("datasetKeys").and_then(Value::as_array_mut) {
                for key in keys {
                    if let Some(local) = key.as_str().and_then(|key| dataset_map.get(key)) {
                        *key = Value::String(local.clone());
                    }
                }
            }
        }
    }
    Ok(tree)
}

fn normalize_rewritten_tree(tree: &mut Value) {
    if let Some(dataset_ids) = tree.get_mut("datasetIds").and_then(Value::as_array_mut) {
        dataset_ids.sort_by(|left, right| {
            left.as_str()
                .unwrap_or_default()
                .cmp(right.as_str().unwrap_or_default())
        });
    }
    if let Some(entries) = tree.get_mut("entries").and_then(Value::as_array_mut) {
        entries.sort_by(|left, right| {
            let left_dataset = left
                .get("datasetId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let right_dataset = right
                .get("datasetId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            left_dataset.cmp(right_dataset).then_with(|| {
                left.get("imageId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .cmp(
                        right
                            .get("imageId")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    )
            })
        });
    }
    if let Some(groups) = tree
        .get_mut("projectMetadata")
        .and_then(|value| value.get_mut("datasetGroups"))
        .and_then(Value::as_array_mut)
    {
        groups.sort_by(|left, right| {
            left.get("position")
                .and_then(Value::as_i64)
                .unwrap_or_default()
                .cmp(&right.get("position").and_then(Value::as_i64).unwrap_or_default())
                .then(
                    left.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .cmp(right.get("id").and_then(Value::as_str).unwrap_or_default()),
                )
        });
    }
}

fn safe_local_branch_name(remote_name: &str) -> Result<String, String> {
    let name = remote_name.replace(['/', '\\'], "::");
    if name.trim().is_empty()
        || name.len() > 128
        || name == "."
        || name == ".."
        || name
            .chars()
            .any(|value| value.is_control() || value.is_whitespace())
    {
        return Err(format!("云端分支名无法映射到本地分支: {remote_name}"));
    }
    Ok(name)
}

fn remote_main_head(bundle: &GraphBundle) -> Option<String> {
    bundle
        .refs
        .iter()
        .find(|reference| reference.name == "main")
        .and_then(|reference| reference.commit_id.clone())
}

fn remote_version_graph(
    local_project_id: &str,
    bundle: &GraphBundle,
    dataset_map: &BTreeMap<String, String>,
    track_main: bool,
    track_non_main: bool,
) -> Result<DataProjectGraph, String> {
    let mut commits = Vec::with_capacity(bundle.commits.len());
    for commit in &bundle.commits {
        if commit.parent_ids.len() > 2 {
            return Err(format!(
                "云端提交包含超过两个父提交，无法导入: {}",
                commit.commit_id
            ));
        }
        let mut tree = rewrite_tree(&commit.tree, dataset_map)?;
        normalize_rewritten_tree(&mut tree);
        let entries = tree_entries(&tree)?
            .into_iter()
            .map(|entry| VersionCommitEntry {
                dataset_id: entry.dataset_id,
                image_id: entry.image_id,
                file_name: entry.file_name,
                image_hash: entry.image_hash,
                annotation_hash: entry.annotation_hash,
                status: entry.status,
                qa_status: entry.qa_status,
                split: entry.split,
                width: entry.width,
                height: entry.height,
            })
            .collect();
        let project_metadata = tree
            .get("projectMetadata")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let mut project_metadata: DataProjectMetadata = serde_json::from_value(project_metadata)
            .map_err(|error| format!("提交项目元数据无效: {error}"))?;
        let dataset_ids = tree
            .get("datasetIds")
            .cloned()
            .map(|value| {
                serde_json::from_value::<Vec<String>>(value)
                    .map_err(|error| format!("提交数据集列表无效: {error}"))
            })
            .transpose()?
            .unwrap_or_default();
        data_project_versioning::validate_project_metadata(&mut project_metadata, &dataset_ids)?;
        commits.push(VersionCommit {
            id: commit.commit_id.clone(),
            data_project_id: local_project_id.to_string(),
            branch: None,
            parent_id: commit.parent_ids.first().cloned(),
            second_parent_id: commit.parent_ids.get(1).cloned(),
            message: commit.message.clone(),
            created_at: commit.created_at.clone(),
            dataset_ids,
            dataset_metadata: tree_dataset_metadata(&tree),
            project_metadata,
            entries,
        });
    }

    let mut branches = Vec::with_capacity(bundle.refs.len());
    let mut branch_names = BTreeSet::new();
    for reference in &bundle.refs {
        let name = if reference.name == "main" {
            if track_main {
                "origin::main".to_string()
            } else {
                "main".to_string()
            }
        } else if track_non_main {
            tracking_branch_name(&reference.name)?
        } else {
            safe_local_branch_name(&reference.name)?
        };
        if !branch_names.insert(name.clone()) {
            return Err(format!("云端分支映射冲突: {name}"));
        }
        // The active local main ref is advanced separately with an explicit
        // CAS after graph import. Keeping this incoming head empty prevents a
        // graph import from silently moving a user's active branch.
        let head_commit_id = if reference.name == "main" && !track_main {
            None
        } else {
            reference.commit_id.clone()
        };
        branches.push(DataProjectBranch {
            data_project_id: local_project_id.to_string(),
            name,
            head_commit_id,
            created_at: reference.updated_at.clone(),
            updated_at: reference.updated_at.clone(),
            current: reference.name == "main" && !track_main,
        });
    }
    if !branch_names.contains("main") {
        branches.push(DataProjectBranch {
            data_project_id: local_project_id.to_string(),
            name: "main".to_string(),
            head_commit_id: None,
            created_at: now_string(),
            updated_at: now_string(),
            current: true,
        });
    }
    let object_hashes = bundle
        .objects
        .iter()
        .map(|object| object.hash.to_ascii_lowercase())
        .collect::<Vec<_>>();
    Ok(DataProjectGraph {
        data_project_id: local_project_id.to_string(),
        branches,
        commits,
        object_hashes,
    })
}

fn remote_trusted_source_scope(bundle: &GraphBundle) -> Result<Option<String>, String> {
    let objects = object_descriptor_map(bundle);
    let mut scopes = BTreeSet::new();
    for commit in &bundle.commits {
        for hash in tree_dataset_metadata(&commit.tree).values() {
            let object = objects
                .get(&hash.to_ascii_lowercase())
                .ok_or_else(|| format!("云端提交缺少数据集元数据对象: {hash}"))?;
            let data = fs::read(local_object_path(&object.hash))
                .map_err(|error| format!("读取云端数据集元数据失败: {error}"))?;
            let value: Value = serde_json::from_slice(&data)
                .map_err(|error| format!("云端数据集元数据无效: {error}"))?;
            if let Some(scope) = value.get("accountScope") {
                if let Some(scope) = scope.as_str() {
                    if !scope.trim().is_empty() {
                        scopes.insert(scope.to_string());
                    }
                } else if !scope.is_null() {
                    return Err("云端数据集元数据 accountScope 无效".to_string());
                }
            }
        }
    }
    if scopes.len() > 1 {
        return Err("云端项目包含多个来源账户作用域，拒绝导入".to_string());
    }
    Ok(scopes.into_iter().next())
}

fn ensure_local_project_row(
    local_project_id: &str,
    bundle: &GraphBundle,
    session: &SessionCredentials,
) -> Result<(), String> {
    let mut connection = open_local_data_project_database()?;
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM data_projects WHERE id = ?1)",
            params![local_project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !exists {
        connection
            .execute(
                "INSERT INTO data_projects (id, name, description, created_at, updated_at, account_scope) VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                params![
                    local_project_id,
                    bundle.project.name,
                    bundle.project.description,
                    now_string(),
                    account_scope(session)
                ],
            )
            .map_err(|error| format!("创建本地云端项目镜像失败: {error}"))?;
    }
    Ok(())
}

fn apply_remote_membership(
    local_project_id: &str,
    bundle: &GraphBundle,
    dataset_map: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut connection = open_local_data_project_database()?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
            params![local_project_id],
        )
        .map_err(|error| error.to_string())?;
    for local_dataset_id in dataset_map.values() {
        let owner: Option<String> = transaction
            .query_row(
                "SELECT data_project_id FROM data_project_datasets WHERE dataset_id = ?1",
                params![local_dataset_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if owner
            .as_deref()
            .is_some_and(|owner| owner != local_project_id)
        {
            return Err(format!(
                "本地数据集已属于其他数据项目，拒绝覆盖: {local_dataset_id}"
            ));
        }
        transaction
            .execute(
                "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3) ON CONFLICT(dataset_id) DO UPDATE SET data_project_id = excluded.data_project_id, assigned_at = excluded.assigned_at",
                params![local_dataset_id, local_project_id, now_string()],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "UPDATE data_projects SET name = ?2, description = ?3, updated_at = ?4 WHERE id = ?1",
            params![
                local_project_id,
                bundle.project.name,
                bundle.project.description,
                now_string()
            ],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn import_remote_version_graph(
    local_project_id: &str,
    bundle: &GraphBundle,
    graph_dataset_map: &BTreeMap<String, String>,
    current_dataset_map: &BTreeMap<String, String>,
    track_main: bool,
    track_non_main: bool,
    expected_local_head: Option<&str>,
) -> Result<(), String> {
    let graph = remote_version_graph(
        local_project_id,
        bundle,
        graph_dataset_map,
        track_main,
        track_non_main,
    )?;
    let trusted_scope = remote_trusted_source_scope(bundle)?;
    data_project_versioning::import_data_project_graph_authorized(
        &graph,
        trusted_scope.as_deref(),
    )?;
    if !track_main {
        data_project_versioning::compare_and_swap_data_project_branch(
            local_project_id,
            "main",
            expected_local_head,
            remote_main_head(bundle).as_deref(),
        )?;
    }
    Ok(())
}

fn persist_local_graph(
    local_project_id: &str,
    bundle: &GraphBundle,
    dataset_map: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut connection = open_local_data_project_database()?;
    let exists: bool = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM data_projects WHERE id = ?1)",
            params![local_project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let project = &bundle.project;
    let scope = account_scope(&authenticated_client()?.session);
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    if !exists {
        tx.execute(
            "INSERT INTO data_projects (id, name, description, created_at, updated_at, account_scope) VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![local_project_id, project.name, project.description, now_string(), scope],
        )
        .map_err(|error| error.to_string())?;
    } else {
        tx.execute(
            "UPDATE data_projects SET name = ?2, description = ?3, account_scope = COALESCE(account_scope, ?4), updated_at = ?5 WHERE id = ?1",
            params![local_project_id, project.name, project.description, scope, now_string()],
        )
        .map_err(|error| error.to_string())?;
    }
    // A clean pull/clone makes the remote membership authoritative. Remove
    // stale local assignments first so a later publish cannot resurrect a
    // dataset that a remote member removed.
    tx.execute(
        "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
        params![local_project_id],
    )
    .map_err(|error| error.to_string())?;
    for (remote, local) in dataset_map {
        let owner: Option<String> = tx
            .query_row(
                "SELECT data_project_id FROM data_project_datasets WHERE dataset_id = ?1",
                params![local],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if owner
            .as_deref()
            .is_some_and(|owner| owner != local_project_id)
        {
            return Err(format!("本地数据集已属于其他数据项目，拒绝覆盖: {local}"));
        }
        tx.execute(
            "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3) ON CONFLICT(dataset_id) DO UPDATE SET data_project_id = excluded.data_project_id, assigned_at = excluded.assigned_at",
            params![local, local_project_id, now_string()],
        )
        .map_err(|error| error.to_string())?;
        let _ = remote;
    }
    let remote_commits = bundle
        .commits
        .iter()
        .map(|commit| {
            let mut tree = rewrite_tree(&commit.tree, dataset_map)?;
            normalize_rewritten_tree(&mut tree);
            let entries = tree_entries(&tree)?;
            Ok((commit, tree, entries))
        })
        .collect::<Result<Vec<_>, String>>()?;
    for (commit, tree, entries) in remote_commits {
        let dataset_ids = tree.get("datasetIds").cloned().unwrap_or_else(|| json!([]));
        let dataset_metadata = tree
            .get("datasetMetadata")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let project_metadata = tree
            .get("projectMetadata")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let parent_id = commit.parent_ids.first().cloned();
        let second_parent_id = commit.parent_ids.get(1).cloned();
        let dataset_ids_json = dataset_ids.to_string();
        let dataset_metadata_json = dataset_metadata.to_string();
        let project_metadata_json = project_metadata.to_string();
        let existing = tx
            .query_row(
                "SELECT data_project_id, parent_id, parent2_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json FROM data_project_commits WHERE id = ?1",
                params![commit.commit_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, String>(7)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some((
            owner,
            existing_parent,
            existing_second_parent,
            existing_message,
            existing_created_at,
            existing_ids,
            existing_metadata,
            existing_project_metadata,
        )) = existing
        {
            if owner != local_project_id
                || existing_parent != parent_id
                || existing_second_parent != second_parent_id
                || existing_message != commit.message
                || !data_project_versioning::commit_timestamps_equal(
                    &existing_created_at,
                    &commit.created_at,
                )
                || existing_ids != dataset_ids_json
                || existing_metadata != dataset_metadata_json
                || existing_project_metadata != project_metadata_json
            {
                return Err(format!(
                    "本地提交与云端 immutable commit 不一致，拒绝覆盖: {}",
                    commit.commit_id
                ));
            }
        } else {
            tx.execute(
                "INSERT INTO data_project_commits (id, data_project_id, parent_id, parent2_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![commit.commit_id, local_project_id, parent_id, second_parent_id, commit.message, commit.created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json],
            )
            .map_err(|error| error.to_string())?;
        }
        tx.execute(
            "DELETE FROM data_project_commit_entries WHERE commit_id = ?1",
            params![commit.commit_id],
        )
        .map_err(|error| error.to_string())?;
        for entry in entries {
            tx.execute(
                "INSERT INTO data_project_commit_entries (commit_id, dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![commit.commit_id, entry.dataset_id, entry.image_id, entry.file_name, entry.image_hash, entry.annotation_hash, entry.status, entry.qa_status, entry.split, entry.width, entry.height],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    let head = bundle
        .refs
        .iter()
        .find(|reference| reference.name == "main")
        .and_then(|reference| reference.commit_id.clone());
    tx.execute(
        "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1",
        params![local_project_id, head, now_string()],
    )
    .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(())
}

fn download_graph_objects(
    client: &AuthenticatedClient,
    remote_project_id: &str,
    bundle: &GraphBundle,
) -> Result<(), String> {
    if bundle.objects.len() > 1_000_000 {
        return Err("云端对象数量过大".to_string());
    }
    let map = object_descriptor_map(bundle);
    let root = project_fs::workspace_data_root().join("data-project-objects");
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    for object in map.values() {
        if !valid_hash(&object.hash) {
            return Err("云端对象 hash 无效".to_string());
        }
        let destination = root.join(object.hash.to_ascii_lowercase());
        if destination.exists() {
            let (hash, size) = hash_file(&destination)?;
            if hash != object.hash.to_ascii_lowercase() || size != object.byte_size {
                return Err(format!("本地对象与云端不一致: {}", object.hash));
            }
            continue;
        }
        download_object(client, remote_project_id, object, &destination)?;
    }
    Ok(())
}

fn pull_remote_graph(
    client: &AuthenticatedClient,
    local_project_id: &str,
    remote_project_id: &str,
    local: &LocalGraph,
    remote: &GraphBundle,
    track_main: bool,
    track_non_main: bool,
) -> Result<(), String> {
    let current_map = remote_dataset_map(local_project_id, remote)?;
    let graph_map = remote_graph_dataset_map(local_project_id, remote, &current_map)?;
    save_remote_dataset_map(local_project_id, &graph_map)?;
    download_graph_objects(client, remote_project_id, remote)?;
    materialize_remote_dataset_snapshots(
        Some(local),
        remote,
        &current_map,
        &graph_map,
        client,
        remote_project_id,
        track_main,
    )?;
    import_remote_version_graph(
        local_project_id,
        remote,
        &graph_map,
        &current_map,
        track_main,
        track_non_main,
        local.head.as_deref(),
    )?;
    // The imported graph may have created origin:: tracking branches.  Read
    // it back before recording bindings so a remote fork is never persisted
    // as the user's direct branch merely because that branch existed before
    // the pull.
    let imported_local = local_graph(local_project_id)?;
    save_full_graph(
        local_project_id,
        remote,
        Some(&imported_local),
        track_main,
        track_non_main,
    )?;
    Ok(())
}

pub fn clone_remote_data_project_into(
    remote_project_id: &str,
    local_project_id: Option<&str>,
) -> Result<SyncReport, String> {
    let client = authenticated_client()?;
    let bundle = graph(&client, remote_project_id)?;
    let local_project_id = local_project_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("data-project-{}", Uuid::new_v4()));
    validate_local_project_id(&local_project_id)?;
    // A caller may supply an explicit local ID, but clone never overwrites an
    // existing project (regardless of its dirty state or account scope).
    ensure_local_scope(&local_project_id, &client.session)?;
    if local_project_exists(&local_project_id)? {
        return Err("目标本地数据项目已存在，请使用新的本地 ID；不会覆盖已有项目".to_string());
    }
    let result = (|| {
        let current_map = remote_dataset_map(&local_project_id, &bundle)?;
        let graph_map = remote_graph_dataset_map(&local_project_id, &bundle, &current_map)?;
        save_remote_dataset_map(&local_project_id, &graph_map)?;
        download_graph_objects(&client, remote_project_id, &bundle)?;
        ensure_local_project_row(&local_project_id, &bundle, &client.session)?;
        materialize_remote_dataset_snapshots(
            None,
            &bundle,
            &current_map,
            &graph_map,
            &client,
            remote_project_id,
            false,
        )?;
        import_remote_version_graph(
            &local_project_id,
            &bundle,
            &graph_map,
            &current_map,
            false,
            false,
            None,
        )?;
        let head = remote_main_head(&bundle);
        let cloned_local = local_graph(&local_project_id)?;
        save_full_graph(
            &local_project_id,
            &bundle,
            Some(&cloned_local),
            false,
            false,
        )?;
        save_sync_state(
            &local_project_id,
            &SyncState {
                remote_project_id: remote_project_id.to_string(),
                server_url: client.session.server_url.trim_end_matches('/').to_string(),
                account_id: client.session.account_id.clone(),
            },
        )?;
        Ok(SyncReport {
            local_project_id: local_project_id.clone(),
            remote_project_id: remote_project_id.to_string(),
            remote_revision: bundle.remote_revision,
            local_head: head.clone(),
            remote_head: head,
            outcome: "cloned".to_string(),
            tracking_ref: None,
        })
    })();
    if result.is_err() {
        let _ = cleanup_partial_clone(&local_project_id, remote_project_id);
    }
    result
}

fn sqlite_table_exists(connection: &Connection, table: &str) -> Result<bool, String> {
    connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![table],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

/// Remove every local side effect made by a failed clone.  The graph/object
/// import intentionally happens in several stores (sync DB, versioning DB,
/// and materialized dataset projects), so a database transaction alone cannot
/// make clone atomic.  Content-addressed object files/index rows are shared by
/// projects and are deliberately retained; the next attempt can verify and
/// reuse them safely.
fn cleanup_partial_clone(local_project_id: &str, remote_project_id: &str) -> Result<(), String> {
    let mut sync_connection = open_sync_database()?;
    let dataset_bindings = {
        let mut statement = sync_connection
            .prepare(
                "SELECT remote_dataset_key, local_dataset_id FROM data_project_sync_datasets WHERE local_project_id = ?1",
            )
            .map_err(|error| error.to_string())?;
        let bindings = statement
            .query_map(params![local_project_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?;
        bindings
    };
    {
        let transaction = sync_connection
            .transaction()
            .map_err(|error| error.to_string())?;
        for table in [
            "data_project_sync_state",
            "data_project_sync_refs",
            "data_project_sync_commits",
            "data_project_sync_datasets",
        ] {
            transaction
                .execute(
                    &format!("DELETE FROM {table} WHERE local_project_id = ?1"),
                    params![local_project_id],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())?;
    }

    let mut local_connection = open_local_data_project_database()?;
    local_connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|error| error.to_string())?;
    let dataset_owners = if sqlite_table_exists(&local_connection, "data_project_datasets")? {
        let mut statement = local_connection
            .prepare("SELECT dataset_id FROM data_project_datasets WHERE data_project_id = ?1")
            .map_err(|error| error.to_string())?;
        let owned = statement
            .query_map(params![local_project_id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<BTreeSet<_>, _>>()
            .map_err(|error| error.to_string())?;
        let mut owners = BTreeMap::new();
        for dataset_id in owned {
            owners.insert(dataset_id, local_project_id.to_string());
        }
        let mut statement = local_connection
            .prepare("SELECT dataset_id, data_project_id FROM data_project_datasets")
            .map_err(|error| error.to_string())?;
        for row in statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|error| error.to_string())?
        {
            let (dataset_id, owner) = row.map_err(|error| error.to_string())?;
            owners.insert(dataset_id, owner);
        }
        owners
    } else {
        BTreeMap::new()
    };

    // Some clone failures occur before the versioning schema has been opened.
    // Delete only tables which are present so cleanup never hides the original
    // network/validation error with a migration-order error.
    for table in [
        "data_project_merge_conflicts",
        "data_project_merge_sessions",
        "data_project_active_branches",
        "data_project_branches",
        "data_project_trusted_scopes",
        "data_project_commit_parents",
        "data_project_commit_entries",
    ] {
        if !sqlite_table_exists(&local_connection, table)? {
            continue;
        }
        let sql = match table {
            "data_project_merge_conflicts" => {
                "DELETE FROM data_project_merge_conflicts WHERE session_id IN (SELECT id FROM data_project_merge_sessions WHERE data_project_id = ?1)"
            }
            "data_project_merge_sessions" => {
                "DELETE FROM data_project_merge_sessions WHERE data_project_id = ?1"
            }
            "data_project_active_branches" => {
                "DELETE FROM data_project_active_branches WHERE data_project_id = ?1"
            }
            "data_project_branches" => {
                "DELETE FROM data_project_branches WHERE data_project_id = ?1"
            }
            "data_project_trusted_scopes" => {
                "DELETE FROM data_project_trusted_scopes WHERE data_project_id = ?1"
            }
            "data_project_commit_parents" => {
                "DELETE FROM data_project_commit_parents WHERE commit_id IN (SELECT id FROM data_project_commits WHERE data_project_id = ?1)"
            }
            "data_project_commit_entries" => {
                "DELETE FROM data_project_commit_entries WHERE commit_id IN (SELECT id FROM data_project_commits WHERE data_project_id = ?1)"
            }
            _ => continue,
        };
        local_connection
            .execute(sql, params![local_project_id])
            .map_err(|error| error.to_string())?;
    }
    if sqlite_table_exists(&local_connection, "data_project_commits")? {
        // Break the self-reference first; otherwise an immediate SQLite FK can
        // reject deletion when a parent row is visited before its child.
        local_connection
            .execute(
                "UPDATE data_project_commits SET parent_id = NULL, parent2_id = NULL WHERE data_project_id = ?1",
                params![local_project_id],
            )
            .map_err(|error| error.to_string())?;
        local_connection
            .execute(
                "DELETE FROM data_project_commits WHERE data_project_id = ?1",
                params![local_project_id],
            )
            .map_err(|error| error.to_string())?;
    }
    if sqlite_table_exists(&local_connection, "data_project_datasets")? {
        local_connection
            .execute(
                "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
                params![local_project_id],
            )
            .map_err(|error| error.to_string())?;
    }
    if sqlite_table_exists(&local_connection, "data_projects")? {
        local_connection
            .execute(
                "DELETE FROM data_projects WHERE id = ?1",
                params![local_project_id],
            )
            .map_err(|error| error.to_string())?;
    }

    // Materialized dataset roots are removed only when they were owned by the
    // failed project (or are an otherwise-unassigned deterministic clone ID)
    // and their remote binding is exactly this cloud project.  Never remove a
    // user dataset merely because it shares a content hash or remote ID.
    for (remote_key, local_dataset_id) in dataset_bindings {
        let deterministic_id = format!(
            "remote-{:x}",
            Sha256::digest(format!("{}\n{}", local_project_id, remote_key).as_bytes())
        );
        if dataset_owners
            .get(&local_dataset_id)
            .is_some_and(|owner| owner != local_project_id)
        {
            continue;
        }
        if !dataset_owners.contains_key(&local_dataset_id) && local_dataset_id != deterministic_id {
            continue;
        }
        let paths = project_fs::project_paths(&local_dataset_id);
        if !paths.root.exists() || !paths.sqlite.exists() {
            continue;
        }
        let Ok(dataset_connection) = Connection::open(&paths.sqlite) else {
            continue;
        };
        if !sqlite_table_exists(&dataset_connection, "remote_project_configs")? {
            continue;
        }
        let remote_matches: bool = dataset_connection
            .query_row(
                "SELECT EXISTS (SELECT 1 FROM remote_project_configs WHERE project_id = ?1 AND remote_project_id = ?2)",
                params![local_dataset_id, remote_project_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if remote_matches {
            fs::remove_dir_all(&paths.root).map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}

fn local_project_exists(local_project_id: &str) -> Result<bool, String> {
    let connection = match Connection::open(local_data_project_database()) {
        Ok(connection) => connection,
        Err(_) => return Ok(false),
    };
    connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM data_projects WHERE id = ?1)",
            params![local_project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
}

fn sync_data_project_with_remote(
    local_project_id: &str,
    remote_project_id: &str,
) -> Result<SyncReport, String> {
    validate_local_project_id(local_project_id)?;
    validate_remote_project_id(remote_project_id)?;
    let client = authenticated_client()?;
    ensure_local_scope(local_project_id, &client.session)?;
    ensure_local_clean(local_project_id)?;
    let local = local_graph(local_project_id)?;
    let remote = graph(&client, remote_project_id)?;
    let bindings = load_ref_bindings(local_project_id)?;
    let local_head = local.head.clone();
    let remote_head = remote_main_head(&remote);
    let relation = head_relation(&local, &remote, local_head.as_deref(), remote_head.as_deref());
    let non_main = inspect_non_main_refs(&local, &remote, &bindings)?;
    let datasets_match = datasets_match_remote(&local, &remote)?;
    let writable = matches!(remote.project.role.as_str(), "owner" | "admin" | "editor");

    if !writable
        && (relation == HeadRelation::LocalAhead || non_main.local_changes || !datasets_match)
    {
        return Err(
            "云端数据项目当前角色为只读，本地存在领先提交、分支或数据集变化，不能推送；请在可写成员端合并后再同步"
                .to_string(),
        );
    }

    if relation == HeadRelation::Diverged || non_main.conflict_ref.is_some() {
        pull_remote_graph(
            &client,
            local_project_id,
            remote_project_id,
            &local,
            &remote,
            true,
            true,
        )?;
        return Ok(SyncReport {
            local_project_id: local_project_id.to_string(),
            remote_project_id: remote_project_id.to_string(),
            remote_revision: remote.remote_revision,
            local_head,
            remote_head,
            outcome: "diverged".to_string(),
            tracking_ref: non_main
                .conflict_ref
                .or_else(|| Some("origin::main".to_string())),
        });
    }

    if relation == HeadRelation::RemoteAhead {
        pull_remote_graph(
            &client,
            local_project_id,
            remote_project_id,
            &local,
            &remote,
            false,
            true,
        )?;
        return Ok(SyncReport {
            local_project_id: local_project_id.to_string(),
            remote_project_id: remote_project_id.to_string(),
            remote_revision: remote.remote_revision,
            local_head,
            remote_head,
            outcome: "pulled".to_string(),
            tracking_ref: non_main.tracking_ref,
        });
    }

    // Import remote non-main refs as origin:: tracking branches before any
    // local push. This preserves a remote-only branch and never overwrites a
    // user-owned branch with a remote head.
    if non_main.tracking_needed {
        pull_remote_graph(
            &client,
            local_project_id,
            remote_project_id,
            &local,
            &remote,
            false,
            true,
        )?;
    }

    if !writable {
        return Ok(SyncReport {
            local_project_id: local_project_id.to_string(),
            remote_project_id: remote_project_id.to_string(),
            remote_revision: remote.remote_revision,
            local_head,
            remote_head,
            outcome: if non_main.tracking_needed {
                "pulled".to_string()
            } else {
                "upToDate".to_string()
            },
            tracking_ref: non_main.tracking_ref,
        });
    }

    let local_to_remote = canonical_dataset_map(local_project_id, &local, &remote)?;
    let payload = local_push_payload(&local, &remote, &bindings, &local_to_remote)?;
    if payload.commits.is_empty() && payload.refs.is_empty() && datasets_match {
        return Ok(SyncReport {
            local_project_id: local_project_id.to_string(),
            remote_project_id: remote_project_id.to_string(),
            remote_revision: remote.remote_revision,
            local_head,
            remote_head,
            outcome: if non_main.tracking_needed {
                "pulled".to_string()
            } else {
                "upToDate".to_string()
            },
            tracking_ref: non_main.tracking_ref,
        });
    }

    let remote_objects = object_descriptor_map(&remote);
    for object in &local.objects {
        if !remote_objects.contains_key(&object.hash.to_ascii_lowercase()) {
            upload_object(&client, remote_project_id, object)?;
        }
    }
    let body = serde_json::to_value(&payload).map_err(|error| error.to_string())?;
    let pushed: Value = post_json(
        &client,
        &format!("/data-projects/{remote_project_id}/push"),
        &body,
        "推送数据项目",
    )?;
    let new_revision = pushed
        .get("remoteRevision")
        .and_then(Value::as_i64)
        .unwrap_or(remote.remote_revision);
    let refreshed = graph(&client, remote_project_id)?;
    save_local_dataset_bindings(local_project_id, &local, &refreshed)?;
    save_full_graph(local_project_id, &refreshed, Some(&local), false, false)?;
    save_sync_state(
        local_project_id,
        &SyncState {
            remote_project_id: remote_project_id.to_string(),
            server_url: client.session.server_url.trim_end_matches('/').to_string(),
            account_id: client.session.account_id.clone(),
        },
    )?;
    Ok(SyncReport {
        local_project_id: local_project_id.to_string(),
        remote_project_id: remote_project_id.to_string(),
        remote_revision: new_revision,
        local_head,
        remote_head: local.head,
        outcome: "pushed".to_string(),
        tracking_ref: non_main.tracking_ref,
    })
}

/// Publish the clean local project to its already connected cloud project.
/// Connection is explicit so a local project can never accidentally publish
/// into a project belonging to another account or server scope.
pub fn publish_data_project(local_project_id: &str) -> Result<SyncReport, String> {
    let client = authenticated_client()?;
    let state = load_sync_state(local_project_id, &client.session)?
        .ok_or_else(|| "本地数据项目尚未连接云端项目".to_string())?;
    sync_data_project_with_remote(local_project_id, &state.remote_project_id)
}

/// Synchronize a connected local project.  The remote ID is read from the
/// account-scoped sync state, keeping transport details out of frontend code.
pub fn sync_data_project(local_project_id: &str) -> Result<SyncReport, String> {
    publish_data_project(local_project_id)
}

/// Clone a complete cloud project into a fresh isolated local project.  The
/// optional local-ID variant remains available as `clone_remote_data_project_into`.
pub fn clone_remote_data_project(remote_project_id: &str) -> Result<SyncReport, String> {
    clone_remote_data_project_into(remote_project_id, None)
}

pub fn get_data_project_sync_status(
    local_project_id: &str,
) -> Result<DataProjectSyncStatus, String> {
    validate_local_project_id(local_project_id)?;
    let client = authenticated_client()?;
    ensure_local_scope(local_project_id, &client.session)?;
    let local = local_graph(local_project_id)?;
    let dirty = data_projects::get_data_project_working_tree(local_project_id)
        .map(|tree| !tree.changes.is_empty())?;
    let Some(state) = load_sync_state(local_project_id, &client.session)? else {
        return Ok(DataProjectSyncStatus {
            local_project_id: local_project_id.to_string(),
            remote_project_id: None,
            remote_revision: None,
            local_head: local.head,
            remote_head: None,
            dirty,
            state: if dirty { "dirty" } else { "unbound" }.to_string(),
            tracking_ref: None,
        });
    };
    let remote = graph(&client, &state.remote_project_id)?;
    let bindings = load_ref_bindings(local_project_id)?;
    let remote_head = remote_main_head(&remote);
    let relation = head_relation(&local, &remote, local.head.as_deref(), remote_head.as_deref());
    let non_main = inspect_non_main_refs(&local, &remote, &bindings)?;
    let datasets_match = datasets_match_remote(&local, &remote)?;
    let writable = matches!(remote.project.role.as_str(), "owner" | "admin" | "editor");
    let tracking_ref = non_main
        .conflict_ref
        .clone()
        .or_else(|| (relation == HeadRelation::Diverged).then(|| "origin::main".to_string()))
        .or(non_main.tracking_ref.clone());
    let local_ahead = relation == HeadRelation::LocalAhead
        || non_main.local_changes
        || !datasets_match;
    let state_name = if dirty {
        "dirty"
    } else if relation == HeadRelation::Diverged || non_main.conflict_ref.is_some() {
        "diverged"
    } else if local_ahead && !writable {
        "readOnlyAhead"
    } else if local_ahead
        || relation == HeadRelation::RemoteAhead
        || non_main.tracking_needed
    {
        "needsSync"
    } else {
        "upToDate"
    };
    Ok(DataProjectSyncStatus {
        local_project_id: local_project_id.to_string(),
        remote_project_id: Some(state.remote_project_id),
        remote_revision: Some(remote.remote_revision),
        local_head: local.head,
        remote_head,
        dirty,
        state: state_name.to_string(),
        tracking_ref,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_graph(branches: Vec<LocalBranch>, commits: Vec<LocalCommit>) -> LocalGraph {
        LocalGraph {
            id: "local-project".to_string(),
            name: "local".to_string(),
            description: String::new(),
            account_scope: None,
            datasets: Vec::new(),
            commits,
            head: branches
                .iter()
                .find(|branch| branch.name == "main")
                .and_then(|branch| branch.head_commit_id.clone()),
            branches,
            objects: Vec::new(),
        }
    }

    fn remote_graph(refs: Vec<RemoteRef>, commits: Vec<RemoteCommit>) -> GraphBundle {
        GraphBundle {
            project: RemoteDataProject {
                id: "00000000-0000-0000-0000-000000000001".to_string(),
                organization_id: None,
                owner_id: "00000000-0000-0000-0000-000000000002".to_string(),
                name: "remote".to_string(),
                description: String::new(),
                status: "active".to_string(),
                revision: 1,
                role: "owner".to_string(),
                dataset_count: 0,
                ref_count: refs.len() as i64,
                commit_count: commits.len() as i64,
                updated_at: String::new(),
            },
            members: Vec::new(),
            datasets: Vec::new(),
            commits,
            refs,
            objects: Vec::new(),
            remote_revision: 1,
        }
    }

    fn local_commit(id: &str, parent_id: Option<&str>) -> LocalCommit {
        LocalCommit {
            id: id.to_string(),
            parent_id: parent_id.map(str::to_string),
            second_parent_id: None,
            message: id.to_string(),
            created_at: String::new(),
            dataset_ids: Vec::new(),
            dataset_metadata: BTreeMap::new(),
            project_metadata: DataProjectMetadata::default(),
            entries: Vec::new(),
        }
    }

    fn remote_commit(id: &str, parent_ids: &[&str]) -> RemoteCommit {
        RemoteCommit {
            commit_id: id.to_string(),
            parent_ids: parent_ids.iter().map(|parent| (*parent).to_string()).collect(),
            message: id.to_string(),
            tree: json!({"entries": []}),
            metadata: json!({}),
            created_by: "00000000-0000-0000-0000-000000000002".to_string(),
            created_at: String::new(),
        }
    }

    fn remote_ref(name: &str, commit_id: Option<&str>) -> RemoteRef {
        RemoteRef {
            name: name.to_string(),
            commit_id: commit_id.map(str::to_string),
            revision: 1,
            updated_by: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn project_object_filter_excludes_unreferenced_workspace_blob() {
        let objects = vec![
            LocalObject {
                hash: "a".repeat(64),
                kind: "image".to_string(),
                byte_size: 1,
            },
            LocalObject {
                hash: "b".repeat(64),
                kind: "image".to_string(),
                byte_size: 1,
            },
        ];
        let referenced = ["a".repeat(64)].into_iter().collect::<BTreeSet<_>>();
        let filtered = project_objects_only(objects, &referenced).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].hash, "a".repeat(64));
    }

    #[test]
    fn project_object_filter_rejects_missing_referenced_blob() {
        let referenced = ["a".repeat(64)].into_iter().collect::<BTreeSet<_>>();
        let error = project_objects_only(Vec::new(), &referenced).unwrap_err();
        assert!(error.contains("缺少完整本地索引"));
    }

    #[test]
    fn dataset_snapshot_keys_include_feature_only_dataset_history() {
        let feature_tree = json!({
            "datasetIds": ["feature-dataset"],
            "datasetMetadata": {"feature-dataset": "a".repeat(64)},
            "entries": [{
                "datasetId": "feature-dataset",
                "imageId": "image-1",
                "fileName": "image-1.jpg",
                "imageHash": "b".repeat(64),
                "annotationHash": "c".repeat(64),
                "status": "raw",
                "qaStatus": "",
                "split": "train",
                "width": 1,
                "height": 1
            }]
        });
        let keys = tree_dataset_keys(&feature_tree).unwrap();
        assert!(keys.contains("feature-dataset"));
        let bundle = remote_graph(
            vec![remote_ref("main", None), remote_ref("feature", Some("feature"))],
            vec![remote_commit("feature", &[])],
        );
        let mut bundle = bundle;
        bundle.commits[0].tree = feature_tree;
        let snapshots = dataset_snapshot_trees(&bundle).unwrap();
        assert!(snapshots.contains_key("feature-dataset"));
    }

    #[test]
    fn local_graph_dataset_keys_include_non_main_commit_membership() {
        let mut feature = local_commit("feature", Some("base"));
        feature.dataset_ids = vec!["feature-dataset".to_string()];
        feature
            .dataset_metadata
            .insert("feature-dataset".to_string(), "a".repeat(64));
        feature.entries.push(data_projects::DataProjectCommitEntry {
            dataset_id: "feature-dataset".to_string(),
            image_id: "image-1".to_string(),
            file_name: "image-1.jpg".to_string(),
            image_hash: "b".repeat(64),
            annotation_hash: "c".repeat(64),
            status: "raw".to_string(),
            qa_status: String::new(),
            split: "train".to_string(),
            width: 1,
            height: 1,
        });
        let local = local_graph(
            vec![LocalBranch {
                name: "main".to_string(),
                head_commit_id: Some("base".to_string()),
            }],
            vec![local_commit("base", None), feature],
        );
        assert!(local_graph_dataset_keys(&local).contains("feature-dataset"));
    }

    #[test]
    fn canonical_payload_rewrites_clone_dataset_ids_but_not_object_hashes() {
        let mut commit = local_commit("child", Some("base"));
        commit.dataset_ids = vec!["local-b".to_string()];
        commit
            .dataset_metadata
            .insert("local-b".to_string(), "a".repeat(64));
        commit.entries.push(data_projects::DataProjectCommitEntry {
            dataset_id: "local-b".to_string(),
            image_id: "image-1".to_string(),
            file_name: "image-1.jpg".to_string(),
            image_hash: "b".repeat(64),
            annotation_hash: "c".repeat(64),
            status: "raw".to_string(),
            qa_status: String::new(),
            split: "train".to_string(),
            width: 1,
            height: 1,
        });
        let mut local = local_graph(
            vec![LocalBranch {
                name: "main".to_string(),
                head_commit_id: Some("child".to_string()),
            }],
            vec![commit.clone()],
        );
        local.datasets.push(LocalDataset {
            id: "local-b".to_string(),
            metadata_hash: Some("a".repeat(64)),
        });
        let tree = local_tree(
            &commit,
            &[("local-b".to_string(), "canonical-a".to_string())]
                .into_iter()
                .collect(),
        );
        assert_eq!(tree["datasetIds"][0], "canonical-a");
        assert_eq!(tree["datasetMetadata"]["canonical-a"], "a".repeat(64));
        assert_eq!(tree["datasetMetadataHashes"]["canonical-a"], "a".repeat(64));
        assert_eq!(tree["entries"][0]["datasetId"], "canonical-a");
        assert_eq!(tree["entries"][0]["imageHash"], "b".repeat(64));
        assert_eq!(tree["entries"][0]["annotationHash"], "c".repeat(64));
        let payload = local_push_payload(
            &local,
            &remote_graph(vec![remote_ref("main", Some("base"))], vec![]),
            &BTreeMap::new(),
            &[("local-b".to_string(), "canonical-a".to_string())]
                .into_iter()
                .collect(),
        )
        .unwrap();
        assert_eq!(payload.datasets[0]["datasetKey"], "canonical-a");
    }

    #[test]
    fn canonical_payload_rewrites_group_keys_and_preserves_metadata_extensions() {
        let mut commit = local_commit("groups", None);
        commit.dataset_ids = vec!["local-a".to_string(), "local-b".to_string()];
        commit.project_metadata = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![data_project_versioning::DataProjectDatasetGroup {
                id: "group-1".to_string(),
                name: "Target order".to_string(),
                dataset_keys: vec!["local-b".to_string(), "local-a".to_string()],
                position: 0,
            }],
            extra: BTreeMap::from([("futureField".to_string(), json!({"keep": true}))]),
        };
        let tree = local_tree(
            &commit,
            &[
                ("local-a".to_string(), "canonical-a".to_string()),
                ("local-b".to_string(), "canonical-b".to_string()),
            ]
            .into_iter()
            .collect(),
        );
        assert_eq!(
            tree["projectMetadata"]["datasetGroups"][0]["datasetKeys"],
            json!(["canonical-b", "canonical-a"])
        );
        assert_eq!(
            tree["projectMetadata"]["futureField"]["keep"],
            Value::Bool(true)
        );
    }

    #[test]
    fn remote_graph_sort_is_stable_after_dataset_id_rewrite() {
        let entry = |dataset_id: &str, image_id: &str| {
            json!({
                "datasetId": dataset_id,
                "imageId": image_id,
                "fileName": format!("{image_id}.jpg"),
                "imageHash": "b".repeat(64),
                "annotationHash": "c".repeat(64),
                "status": "raw",
                "qaStatus": "",
                "split": "train",
                "width": 1,
                "height": 1
            })
        };
        let mut commit = remote_commit("commit", &[]);
        commit.tree = json!({
            "datasetIds": ["remote-z", "remote-a"],
            "datasetMetadata": {
                "remote-z": "d".repeat(64),
                "remote-a": "e".repeat(64)
            },
            "entries": [
                entry("remote-z", "image-2"),
                entry("remote-a", "image-9"),
                entry("remote-a", "image-1")
            ]
        });
        let bundle = remote_graph(
            vec![remote_ref("main", Some("commit"))],
            vec![commit],
        );
        let dataset_map = [
            ("remote-z".to_string(), "local-z".to_string()),
            ("remote-a".to_string(), "local-a".to_string()),
        ]
        .into_iter()
        .collect();
        let graph = remote_version_graph("local-project", &bundle, &dataset_map, false, false)
            .expect("rewrite remote graph");
        assert_eq!(
            graph.commits[0].dataset_ids,
            vec!["local-a".to_string(), "local-z".to_string()]
        );
        let entries = &graph.commits[0].entries;
        assert_eq!(entries[0].dataset_id, "local-a");
        assert_eq!(entries[0].image_id, "image-1");
        assert_eq!(entries[1].dataset_id, "local-a");
        assert_eq!(entries[1].image_id, "image-9");
        assert_eq!(entries[2].dataset_id, "local-z");
        assert_eq!(entries[2].image_id, "image-2");
    }

    #[test]
    fn feature_only_local_history_is_a_pushable_ref_change() {
        let local = local_graph(
            vec![
                LocalBranch {
                    name: "main".to_string(),
                    head_commit_id: Some("base".to_string()),
                },
                LocalBranch {
                    name: "feature".to_string(),
                    head_commit_id: Some("feature".to_string()),
                },
            ],
            vec![local_commit("feature", Some("base"))],
        );
        let remote = remote_graph(
            vec![remote_ref("main", Some("base"))],
            vec![remote_commit("base", &[])],
        );
        let payload = local_push_payload(
            &local,
            &remote,
            &BTreeMap::new(),
            &BTreeMap::new(),
        )
        .unwrap();
        assert!(payload.commits.iter().any(|commit| {
            commit.get("commitId").and_then(Value::as_str) == Some("feature")
        }));
        assert!(payload.refs.iter().any(|reference| {
            reference.get("name").and_then(Value::as_str) == Some("feature")
                && reference.get("commitId").and_then(Value::as_str) == Some("feature")
        }));
        assert!(!payload.refs.iter().any(|reference| {
            reference.get("name").and_then(Value::as_str) == Some("main")
        }));
    }

    #[test]
    fn remote_only_feature_is_imported_as_tracking_ref() {
        let local = local_graph(
            vec![LocalBranch {
                name: "main".to_string(),
                head_commit_id: Some("base".to_string()),
            }],
            vec![local_commit("base", None)],
        );
        let remote = remote_graph(
            vec![
                remote_ref("main", Some("base")),
                remote_ref("feature", Some("remote-feature")),
            ],
            vec![
                remote_commit("base", &[]),
                remote_commit("remote-feature", &["base"]),
            ],
        );
        let plan = inspect_non_main_refs(&local, &remote, &BTreeMap::new()).unwrap();
        assert!(plan.tracking_needed);
        assert_eq!(plan.tracking_ref.as_deref(), Some("origin::feature"));
        assert!(!plan.local_changes);
        assert!(plan.conflict_ref.is_none());
    }

    #[test]
    fn deleting_a_bound_local_ref_emits_a_cas_delete() {
        let local = local_graph(
            vec![LocalBranch {
                name: "main".to_string(),
                head_commit_id: Some("base".to_string()),
            }],
            vec![local_commit("base", None)],
        );
        let remote = remote_graph(
            vec![
                remote_ref("main", Some("base")),
                remote_ref("feature", Some("remote-feature")),
            ],
            vec![
                remote_commit("base", &[]),
                remote_commit("remote-feature", &["base"]),
            ],
        );
        let mut bindings = BTreeMap::new();
        bindings.insert(
            "feature".to_string(),
            SyncRefBinding {
                remote_name: "feature".to_string(),
                local_name: "feature".to_string(),
                tracking: false,
            },
        );
        let plan = inspect_non_main_refs(&local, &remote, &bindings).unwrap();
        assert!(plan.local_changes);
        let payload = local_push_payload(&local, &remote, &bindings, &BTreeMap::new()).unwrap();
        assert!(payload.refs.iter().any(|reference| {
            reference.get("name").and_then(Value::as_str) == Some("feature")
                && reference.get("expectedCommitId").and_then(Value::as_str)
                    == Some("remote-feature")
                && reference.get("commitId").is_some_and(Value::is_null)
        }));
    }
}

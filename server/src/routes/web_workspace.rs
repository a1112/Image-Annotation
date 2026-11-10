//! PostgreSQL/S3 backed browser workspace for data projects.
//!
//! The native application has a local checkout and a local SQLite repository.
//! The browser cannot use either of those boundaries, so this module keeps a
//! branch-local working tree in PostgreSQL and stores image/annotation bytes in
//! the existing project-scoped S3 object store.  The 0005 commit graph remains
//! the immutable graph shared with desktop sync; this module only adds the
//! durable web draft and command facade around it.

use super::ApiError;
use crate::{
    auth::AuthUser,
    routes::data_project_metadata::{self, DatasetGroup, ProjectMetadata},
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use futures_util::StreamExt;
use image::ImageReader;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, Postgres, Row, Transaction};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path as FsPath, PathBuf},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const MAX_WEB_IMAGE_BYTES: i64 = 256 * 1024 * 1024;
const MAX_WEB_IMAGE_PIXELS: u64 = 100_000_000;
const MAX_COMMAND_JSON_BYTES: usize = 32 * 1024 * 1024;
const MAX_PAGE_SIZE: i64 = 500;
const MAX_CHANGE_COUNT: usize = 20_000;
const MERGE_RESERVATION_TTL_SECONDS: i64 = 5 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebPermission {
    View,
    Edit,
    Manage,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebDataProject {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub dataset_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub account_scope: Option<String>,
    pub revision: i64,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebCounts {
    pub raw: i64,
    pub in_progress: i64,
    pub pending_review: i64,
    pub approved: i64,
    pub rejected: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebChange {
    pub kind: String,
    pub dataset_id: String,
    pub image_id: Option<String>,
    pub file_name: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebWorkingTree {
    pub data_project_id: Uuid,
    pub branch: String,
    pub revision: i64,
    pub counts: WebCounts,
    pub changes: Vec<WebChange>,
    pub head_commit_id: Option<String>,
    pub dirty: bool,
    pub project_metadata: ProjectMetadata,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebCommitEntry {
    pub dataset_id: String,
    pub image_id: String,
    pub file_name: String,
    pub image_hash: String,
    pub annotation_hash: String,
    pub status: String,
    pub qa_status: String,
    pub split: String,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebCommit {
    pub id: String,
    pub data_project_id: Uuid,
    pub parent_id: Option<String>,
    pub second_parent_id: Option<String>,
    pub branch: Option<String>,
    pub message: String,
    pub created_at: DateTime<Utc>,
    pub dataset_ids: Vec<String>,
    pub dataset_metadata: BTreeMap<String, String>,
    pub entries: Vec<WebCommitEntry>,
    pub project_metadata: ProjectMetadata,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebBranch {
    pub data_project_id: Uuid,
    pub name: String,
    pub head_commit_id: Option<String>,
    pub current: bool,
    pub protected: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebConflict {
    pub key: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ours: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theirs: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebMergeResult {
    pub session_id: Option<Uuid>,
    pub commit: Option<WebCommit>,
    pub conflicts: Vec<WebConflict>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct WorkEntry {
    dataset_key: String,
    dataset_id: Option<Uuid>,
    image_id: String,
    file_name: String,
    image_hash: String,
    image_byte_size: Option<i64>,
    image_mime_type: Option<String>,
    annotation_hash: String,
    annotation: Value,
    status: String,
    qa_status: String,
    split: String,
    width: i32,
    height: i32,
    review_note: Option<String>,
    tags: Value,
    source_item_id: Option<Uuid>,
    revision: i64,
}

#[derive(Debug, Clone)]
struct MetadataRow {
    dataset_key: String,
    dataset_id: Option<Uuid>,
    metadata_hash: Option<String>,
    metadata: Value,
}

#[derive(Debug, Clone)]
struct TreeDocument {
    project_metadata: ProjectMetadata,
    metadata: Vec<MetadataRow>,
    entries: Vec<WorkEntry>,
}

#[derive(Debug, Clone)]
enum WebDatasetTarget {
    Project {
        project_id: Uuid,
        dataset_key: String,
        dataset_id: Option<Uuid>,
    },
    Standalone {
        dataset_id: Uuid,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadImageQuery {
    file_name: String,
    #[serde(default)]
    branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContentQuery {
    #[serde(default)]
    pub branch: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImagePageQuery {
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
}

fn command_name(name: &str) -> String {
    name.chars()
        .filter(|ch| *ch != '_' && *ch != '-')
        .flat_map(char::to_lowercase)
        .collect()
}

fn args_object(value: Value) -> Result<Value, ApiError> {
    if serde_json::to_vec(&value)
        .map(|bytes| bytes.len())
        .unwrap_or(MAX_COMMAND_JSON_BYTES + 1)
        > MAX_COMMAND_JSON_BYTES
    {
        return Err(ApiError::bad_request("web command payload is too large"));
    }
    if let Some(args) = value.get("args") {
        if args.is_object() {
            return Ok(args.clone());
        }
    }
    if value.is_object() {
        Ok(value)
    } else {
        Err(ApiError::bad_request("web command input must be an object"))
    }
}

fn arg<'a>(args: &'a Value, name: &str) -> Option<&'a Value> {
    args.get(name).or_else(|| {
        let wanted = command_name(name);
        args.as_object()?
            .iter()
            .find_map(|(key, value)| (command_name(key) == wanted).then_some(value))
    })
}

fn required_string(args: &Value, name: &str) -> Result<String, ApiError> {
    arg(args, name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| ApiError::bad_request(format!("{name} is required")))
}

fn optional_string(args: &Value, name: &str) -> Result<Option<String>, ApiError> {
    match arg(args, name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .map(Some)
            .ok_or_else(|| ApiError::bad_request(format!("{name} must be a string or null"))),
    }
}

/// Merge callers historically used both the native-compatible
/// `sourceBranch`/`targetBranch` pair and the three-way terminology
/// `theirsBranch`/`oursBranch`.  Accept either spelling, but never silently
/// choose between conflicting values when a client sends both.
fn merge_branch_value(
    args: &Value,
    canonical: &str,
    alias: &str,
) -> Result<Option<String>, ApiError> {
    let canonical_value = optional_string(args, canonical)?;
    let alias_value = optional_string(args, alias)?;
    if let (Some(canonical_value), Some(alias_value)) = (&canonical_value, &alias_value) {
        if canonical_value != alias_value {
            return Err(ApiError::bad_request(format!(
                "{canonical} and {alias} must identify the same branch"
            )));
        }
    }
    Ok(canonical_value.or(alias_value))
}

fn required_string_alias(args: &Value, canonical: &str, alias: &str) -> Result<String, ApiError> {
    merge_branch_value(args, canonical, alias)?
        .ok_or_else(|| ApiError::bad_request(format!("{canonical} is required")))
}

fn optional_i64(args: &Value, name: &str) -> Result<Option<i64>, ApiError> {
    match arg(args, name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .map(Some)
            .or_else(|| {
                value
                    .as_str()
                    .and_then(|value| value.trim().parse::<i64>().ok())
                    .map(Some)
            })
            .ok_or_else(|| ApiError::bad_request(format!("{name} must be an integer or null"))),
    }
}

/// Browser callers use `expectedRevision`, while the native-compatible
/// command payloads historically used `baseRevision` or `revision`.  Treat
/// all three as the same optimistic-concurrency token and reject malformed
/// values instead of silently falling back to an unconditional write.
fn expected_revision(args: &Value) -> Result<Option<i64>, ApiError> {
    if let Some(revision) = optional_i64(args, "expectedRevision")? {
        return Ok(Some(revision));
    }
    if let Some(revision) = optional_i64(args, "baseRevision")? {
        return Ok(Some(revision));
    }
    optional_i64(args, "revision")
}

fn optional_value(args: &Value, name: &str) -> Option<Value> {
    arg(args, name).filter(|value| !value.is_null()).cloned()
}

fn required_uuid(args: &Value, name: &str) -> Result<Uuid, ApiError> {
    let value = required_string(args, name)?;
    Uuid::parse_str(&value).map_err(|_| ApiError::bad_request(format!("{name} is not a UUID")))
}

fn optional_uuid(args: &Value, name: &str) -> Result<Option<Uuid>, ApiError> {
    optional_string(args, name)?.map_or(Ok(None), |value| {
        Uuid::parse_str(&value)
            .map(Some)
            .map_err(|_| ApiError::bad_request(format!("{name} is not a UUID")))
    })
}

fn parse_uuid_string(value: &str, field: &str) -> Result<Uuid, ApiError> {
    Uuid::parse_str(value).map_err(|_| ApiError::bad_request(format!("{field} is not a UUID")))
}

fn validate_branch(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(ApiError::bad_request("invalid branch name"));
    }
    Ok(value.to_string())
}

fn validate_dataset_key(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.contains("::") || value.contains('\0') {
        return Err(ApiError::bad_request("invalid canonical dataset key"));
    }
    Ok(value.to_string())
}

fn browser_dataset_id(project_id: Uuid, dataset_key: &str) -> String {
    format!("{project_id}::{dataset_key}")
}

fn parse_browser_dataset_id(value: &str) -> Result<(Uuid, String), ApiError> {
    let (project, key) = value.split_once("::").ok_or_else(|| {
        ApiError::bad_request("datasetId must be <projectUUID>::<canonicalDatasetKey>")
    })?;
    let project_id = parse_uuid_string(project, "datasetId project")?;
    Ok((project_id, validate_dataset_key(key)?))
}

fn parse_project_metadata(value: Option<&Value>) -> Result<ProjectMetadata, ApiError> {
    data_project_metadata::parse(value)
        .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))
}

fn validate_project_metadata_for_tree(
    metadata: &ProjectMetadata,
    dataset_keys: impl Iterator<Item = String>,
) -> Result<(), ApiError> {
    let dataset_keys = dataset_keys.collect::<BTreeSet<_>>();
    data_project_metadata::validate(metadata, Some(&dataset_keys))
        .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(u8::is_ascii_hexdigit)
}

fn json_hash(value: &Value) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ApiError::internal_message(format!("encode JSON object: {error}")))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn annotation_payload(annotation: &Value, status: &str, qa_status: &str) -> Value {
    json!({
        "objects": annotation,
        "status": status,
        "qaStatus": qa_status
    })
}

fn annotation_hash(annotation: &Value, status: &str, qa_status: &str) -> Result<String, ApiError> {
    json_hash(&annotation_payload(annotation, status, qa_status))
}

/// Build the metadata object consumed by the native graph importer.
///
/// The web dataset API historically stores a schema (`{"classes": [...]}`)
/// while the native project graph deliberately stores a complete manifest and
/// an account-scope claim.  Keep the original schema fields, but add the
/// stable fields required by the graph protocol before hashing the object.
/// Existing explicit account-scope claims are retained: this is important for
/// a viewer cloning a project owned by another account, where replacing the
/// claim with the viewer would make the imported history appear local.
fn canonical_metadata_value(
    raw: &Value,
    dataset_id: Option<Uuid>,
    dataset_key: &str,
    dataset_name: &str,
    image_count: usize,
    owner_scope: &str,
) -> Value {
    let classes = schema_classes(raw);
    let classes = Value::Array(
        classes
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
            .filter_map(|(index, class)| {
                let (id, label, color) = if let Some(object) = class.as_object() {
                    let id = object
                        .get("id")
                        .and_then(|value| {
                            value
                                .as_u64()
                                .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                        })
                        .unwrap_or(index as u64) as u32;
                    let label = object
                        .get("label")
                        .or_else(|| object.get("name"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .trim()
                        .to_string();
                    let color = object
                        .get("color")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    (id, label, color)
                } else if let Some(label) = class.as_str() {
                    (index as u32, label.trim().to_string(), String::new())
                } else {
                    return None;
                };
                Some(json!({ "id": id, "label": label, "color": color }))
            })
            .collect(),
    );
    let class_count = classes.as_array().map_or(0, Vec::len);
    let mut result = raw.as_object().cloned().unwrap_or_default();
    let mut manifest = raw
        .get("manifest")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    manifest.insert(
        "id".to_string(),
        Value::String(
            dataset_id
                .map(|value| value.to_string())
                .unwrap_or_else(|| dataset_key.to_string()),
        ),
    );
    manifest.insert("name".to_string(), Value::String(dataset_name.to_string()));
    manifest.insert(
        "sourceDatasetKey".to_string(),
        Value::String(dataset_key.to_string()),
    );
    manifest
        .entry("format".to_string())
        .or_insert_with(|| Value::String("web".to_string()));
    manifest
        .entry("rootPath".to_string())
        .or_insert_with(|| Value::String(String::new()));
    manifest
        .entry("createdAt".to_string())
        .or_insert_with(|| Value::String(Utc::now().to_rfc3339()));
    manifest.insert("classCount".to_string(), json!(class_count));
    manifest.insert("imageCount".to_string(), json!(image_count));
    result.insert("manifest".to_string(), Value::Object(manifest));
    if !result.contains_key("accountScope") {
        result.insert(
            "accountScope".to_string(),
            Value::String(owner_scope.to_string()),
        );
    }
    result.insert("classes".to_string(), classes);
    Value::Object(result)
}

fn annotation_is_empty(annotation: &Value) -> bool {
    annotation.as_array().is_some_and(|items| items.is_empty())
}

fn status_bucket(status: &str, qa_status: &str, annotation: &Value) -> &'static str {
    let status = status.to_ascii_lowercase();
    let qa = qa_status.to_ascii_lowercase();
    if matches!(qa.as_str(), "approved" | "通过" | "pass" | "passed") {
        return "approved";
    }
    if matches!(qa.as_str(), "rejected" | "驳回" | "reject") {
        return "rejected";
    }
    if matches!(
        status.as_str(),
        "pendingreview" | "pending_review" | "submitted" | "待审核"
    ) || matches!(qa.as_str(), "pendingreview" | "pending_review" | "待审核")
    {
        return "pendingReview";
    }
    if annotation_is_empty(annotation)
        || matches!(status.as_str(), "raw" | "unannotated" | "未处理")
    {
        return "raw";
    }
    "inProgress"
}

fn status_for_annotation(annotation: &Value) -> String {
    if annotation_is_empty(annotation) {
        "raw".to_string()
    } else {
        "inProgress".to_string()
    }
}

fn default_qa_status() -> String {
    "pending".to_string()
}

fn sanitize_file_name(value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    let name = FsPath::new(value)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(value)
        .trim();
    if name.is_empty() || name == "." || name == ".." || name.len() > 512 {
        return Err(ApiError::bad_request("fileName is invalid"));
    }
    Ok(name.to_string())
}

fn object_key(project_id: Uuid, hash: &str) -> String {
    format!("data-projects/{project_id}/objects/{hash}")
}

fn temp_object_path(project_id: Uuid) -> PathBuf {
    std::env::temp_dir().join(format!(
        "image-annotation-web-{project_id}-{}",
        Uuid::new_v4()
    ))
}

fn role_allows(role: &str, permission: WebPermission) -> bool {
    match permission {
        WebPermission::View => matches!(role, "owner" | "admin" | "editor" | "viewer"),
        WebPermission::Edit => matches!(role, "owner" | "admin" | "editor"),
        WebPermission::Manage => matches!(role, "owner" | "admin"),
    }
}

async fn require_project(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    permission: WebPermission,
) -> Result<String, ApiError> {
    let row = sqlx::query(
        "SELECT dm.role FROM data_project_members dm JOIN data_projects p ON p.id = dm.project_id JOIN users owner ON owner.id = p.owner_id JOIN users u ON u.id = dm.user_id WHERE dm.project_id = $1 AND dm.user_id = $2 AND dm.deleted_at IS NULL AND p.deleted_at IS NULL AND p.status <> 'deleted' AND u.deleted_at IS NULL AND COALESCE(p.organization_id, owner.organization_id) IS NOT DISTINCT FROM u.organization_id",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::forbidden("data-project membership is required"))?;
    let role: String = row.get("role");
    if role_allows(&role, permission) {
        Ok(role)
    } else {
        Err(ApiError::forbidden(format!(
            "role {role} lacks web data-project permission"
        )))
    }
}

async fn ensure_project_dataset(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    dataset_key: &str,
    permission: WebPermission,
) -> Result<Option<Uuid>, ApiError> {
    require_project(state, user_id, project_id, permission).await?;
    let row = sqlx::query(
        "SELECT pd.dataset_id, d.organization_id, d.deleted_at, d.status, p.organization_id AS project_organization_id, owner.organization_id AS owner_organization_id FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id JOIN users owner ON owner.id = p.owner_id LEFT JOIN datasets d ON d.id = pd.dataset_id WHERE pd.project_id = $1 AND pd.dataset_key = $2",
    )
    .bind(project_id)
    .bind(dataset_key)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project dataset not found"))?;
    let dataset_id = row.get::<Option<Uuid>, _>("dataset_id");
    if let Some(_dataset_id) = dataset_id {
        let deleted_at = row.get::<Option<DateTime<Utc>>, _>("deleted_at");
        let status = row.get::<Option<String>, _>("status");
        let dataset_organization: Option<Uuid> = row.get("organization_id");
        if dataset_organization.is_none() && deleted_at.is_none() && status.is_none() {
            return Err(ApiError::not_found("source dataset not found"));
        }
        if deleted_at.is_some() || status.as_deref().is_some_and(|status| status == "deleted") {
            return Err(ApiError::not_found("source dataset is deleted"));
        }
        let project_organization: Option<Uuid> = row.get("project_organization_id");
        let owner_organization: Option<Uuid> = row.get("owner_organization_id");
        if project_organization.or(owner_organization) != dataset_organization {
            return Err(ApiError::forbidden(
                "data-project dataset organization differs from the project scope",
            ));
        }
    }
    Ok(dataset_id)
}

async fn resolve_dataset_identity(
    state: &AppState,
    user_id: Uuid,
    value: &str,
    permission: WebPermission,
) -> Result<(Uuid, String, Option<Uuid>), ApiError> {
    if let Ok((project_id, key)) = parse_browser_dataset_id(value) {
        let dataset_id =
            ensure_project_dataset(state, user_id, project_id, &key, permission).await?;
        return Ok((project_id, key, dataset_id));
    }
    let dataset_id = parse_uuid_string(value, "datasetId")?;
    let row = sqlx::query(
        "SELECT pd.project_id, pd.dataset_key FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id WHERE pd.dataset_id = $1 AND p.deleted_at IS NULL ORDER BY pd.project_id LIMIT 1",
    )
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset is not assigned to a visible data project"))?;
    let project_id: Uuid = row.get("project_id");
    let key: String = row.get("dataset_key");
    require_project(state, user_id, project_id, permission).await?;
    Ok((project_id, key, Some(dataset_id)))
}

async fn require_standalone_dataset(
    state: &AppState,
    user_id: Uuid,
    dataset_id: Uuid,
) -> Result<(), ApiError> {
    let row = sqlx::query(
        "SELECT d.organization_id FROM datasets d JOIN dataset_members dm ON dm.dataset_id = d.id AND dm.user_id = $2 AND dm.deleted_at IS NULL WHERE d.id = $1 AND d.deleted_at IS NULL AND d.status <> 'deleted' AND NOT EXISTS (SELECT 1 FROM data_project_datasets pd WHERE pd.dataset_id = d.id)",
    )
    .bind(dataset_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset is not available to this account"))?;
    let organization_id: Option<Uuid> = row.get("organization_id");
    let user_organization: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    if organization_id.is_some() && organization_id != user_organization {
        return Err(ApiError::forbidden(
            "dataset organization differs from signed-in user",
        ));
    }
    Ok(())
}

async fn resolve_web_dataset_target(
    state: &AppState,
    user_id: Uuid,
    value: &str,
    permission: WebPermission,
) -> Result<WebDatasetTarget, ApiError> {
    if let Ok((project_id, key)) = parse_browser_dataset_id(value) {
        let dataset_id =
            ensure_project_dataset(state, user_id, project_id, &key, permission).await?;
        return Ok(WebDatasetTarget::Project {
            project_id,
            dataset_key: key,
            dataset_id,
        });
    }
    let dataset_id = parse_uuid_string(value, "datasetId")?;
    let mapping = sqlx::query(
        "SELECT project_id, dataset_key, dataset_id FROM data_project_datasets WHERE dataset_id = $1 ORDER BY project_id LIMIT 1",
    )
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(row) = mapping {
        let project_id: Uuid = row.get("project_id");
        require_project(state, user_id, project_id, permission).await?;
        return Ok(WebDatasetTarget::Project {
            project_id,
            dataset_key: row.get("dataset_key"),
            dataset_id: row.get("dataset_id"),
        });
    }
    require_standalone_dataset(state, user_id, dataset_id).await?;
    Ok(WebDatasetTarget::Standalone { dataset_id })
}

async fn ensure_branch_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    user_id: Uuid,
    branch: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_refs (project_id, ref_name, commit_id, revision, updated_by) VALUES ($1, $2, NULL, 1, $3) ON CONFLICT (project_id, ref_name) DO NOTHING",
    )
    .bind(project_id)
    .bind(branch)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_branches (project_id, branch_name, protected, created_by) VALUES ($1, $2, $3, $4) ON CONFLICT (project_id, branch_name) DO UPDATE SET deleted_at = NULL, updated_at = now()",
    )
    .bind(project_id)
    .bind(branch)
    .bind(branch == "main")
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    let base_commit: Option<String> = sqlx::query_scalar(
        "SELECT commit_id FROM data_project_refs WHERE project_id = $1 AND ref_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_one(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_worktrees (project_id, branch_name, base_commit_id, revision, updated_by) VALUES ($1, $2, $3, 1, $4) ON CONFLICT (project_id, branch_name) DO NOTHING",
    )
    .bind(project_id)
    .bind(branch)
    .bind(base_commit)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn ensure_main_branch(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    ensure_branch_tx(&mut tx, project_id, user_id, "main").await?;
    tx.commit().await?;
    Ok(())
}

fn work_entry_from_row(row: &PgRow) -> WorkEntry {
    WorkEntry {
        dataset_key: row.get("dataset_key"),
        dataset_id: row.get("dataset_id"),
        image_id: row.get("image_id"),
        file_name: row.get("file_name"),
        image_hash: row.get("image_hash"),
        image_byte_size: row.get("image_byte_size"),
        image_mime_type: row.get("image_mime_type"),
        annotation_hash: row.get("annotation_hash"),
        annotation: row.get("annotation_json"),
        status: row.get("status"),
        qa_status: row.get("qa_status"),
        split: row.get("split"),
        width: row.get("width"),
        height: row.get("height"),
        review_note: row.get("review_note"),
        tags: row.get("tags_json"),
        source_item_id: row.get("source_item_id"),
        revision: row.get("revision"),
    }
}

async fn load_worktree(
    state: &AppState,
    project_id: Uuid,
    branch: &str,
) -> Result<
    (
        i64,
        Option<String>,
        ProjectMetadata,
        Vec<MetadataRow>,
        Vec<WorkEntry>,
    ),
    ApiError,
> {
    let worktree = sqlx::query(
        "SELECT revision, base_commit_id, project_metadata_json FROM data_project_worktrees WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("web branch worktree not found"))?;
    let project_metadata_value: Value = worktree.get("project_metadata_json");
    let project_metadata = parse_project_metadata(Some(&project_metadata_value))?;
    let metadata_rows = sqlx::query(
        "SELECT dataset_key, dataset_id, metadata_hash, metadata_json FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 ORDER BY dataset_key",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .map(|row| MetadataRow {
        dataset_key: row.get("dataset_key"),
        dataset_id: row.get("dataset_id"),
        metadata_hash: row.get("metadata_hash"),
        metadata: row.get("metadata_json"),
    })
    .collect::<Vec<_>>();
    let entries = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 ORDER BY dataset_key, image_id",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_all(&state.pool)
    .await?
    .iter()
    .map(work_entry_from_row)
    .collect::<Vec<_>>();
    Ok((
        worktree.get("revision"),
        worktree.get("base_commit_id"),
        project_metadata,
        metadata_rows,
        entries,
    ))
}

async fn load_worktree_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    branch: &str,
) -> Result<
    (
        i64,
        Option<String>,
        ProjectMetadata,
        Vec<MetadataRow>,
        Vec<WorkEntry>,
    ),
    ApiError,
> {
    let worktree = sqlx::query(
        "SELECT revision, base_commit_id, project_metadata_json FROM data_project_worktrees WHERE project_id = $1 AND branch_name = $2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("web branch worktree not found"))?;
    let project_metadata_value: Value = worktree.get("project_metadata_json");
    let project_metadata = parse_project_metadata(Some(&project_metadata_value))?;
    let metadata_rows = sqlx::query(
        "SELECT dataset_key, dataset_id, metadata_hash, metadata_json FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 ORDER BY dataset_key",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| MetadataRow {
        dataset_key: row.get("dataset_key"),
        dataset_id: row.get("dataset_id"),
        metadata_hash: row.get("metadata_hash"),
        metadata: row.get("metadata_json"),
    })
    .collect::<Vec<_>>();
    let entries = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 ORDER BY dataset_key, image_id FOR UPDATE",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_all(&mut **tx)
    .await?
    .iter()
    .map(work_entry_from_row)
    .collect::<Vec<_>>();
    Ok((
        worktree.get("revision"),
        worktree.get("base_commit_id"),
        project_metadata,
        metadata_rows,
        entries,
    ))
}

fn tree_document_json(tree: &TreeDocument) -> Value {
    let datasets = tree
        .metadata
        .iter()
        .map(|metadata| {
            json!({
                "datasetKey": metadata.dataset_key,
                // Native graph consumers treat datasetId as the canonical
                // dataset key.  Keep the hosted UUID in a separate field so
                // a browser worktree can still recover its source mapping.
                "datasetId": metadata.dataset_key,
                "datasetUuid": metadata.dataset_id,
                "metadataHash": metadata.metadata_hash
            })
        })
        .collect::<Vec<_>>();
    // 0005/native graph consumers define datasetMetadata as a hash map.  Keep
    // the raw JSON in a sidecar field for browser hydration, but never put a
    // JSON object in datasetMetadata: desktop sync parses that field as
    // BTreeMap<String, String>.
    let metadata = tree
        .metadata
        .iter()
        .map(|item| {
            (
                item.dataset_key.clone(),
                Value::String(
                    item.metadata_hash
                        .clone()
                        .unwrap_or_else(|| json_hash(&item.metadata).unwrap_or_default()),
                ),
            )
        })
        .collect::<Map<String, Value>>();
    let metadata_values = tree
        .metadata
        .iter()
        .map(|item| (item.dataset_key.clone(), item.metadata.clone()))
        .collect::<Map<String, Value>>();
    let entries = tree
        .entries
        .iter()
        .map(|entry| {
            json!({
                "datasetKey": entry.dataset_key,
                // `datasetId` is the graph identity used by desktop sync;
                // the optional UUID is only source-dataset provenance.
                "datasetId": entry.dataset_key,
                "datasetUuid": entry.dataset_id,
                "imageId": entry.image_id,
                "fileName": entry.file_name,
                "imageHash": entry.image_hash,
                "imageByteSize": entry.image_byte_size,
                "imageMimeType": entry.image_mime_type,
                "annotationHash": entry.annotation_hash,
                "annotation": entry.annotation,
                "status": entry.status,
                "qaStatus": entry.qa_status,
                "split": entry.split,
                "width": entry.width,
                "height": entry.height,
                "reviewNote": entry.review_note,
                "tags": entry.tags,
                "sourceItemId": entry.source_item_id
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schemaVersion": 1,
        "projectMetadata": data_project_metadata::to_value(&tree.project_metadata)
            .unwrap_or_else(|_| json!({ "schemaVersion": 1, "datasetGroups": [] })),
        "datasets": datasets,
        "datasetMetadata": metadata,
        "datasetMetadataValues": metadata_values,
        "datasetIds": tree
            .metadata
            .iter()
            .map(|metadata| Value::String(metadata.dataset_key.clone()))
            .collect::<Vec<_>>(),
        "entries": entries
    })
}

fn value_uuid(value: Option<&Value>) -> Option<Uuid> {
    value
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok())
}

fn metadata_hash_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| valid_hash(value))
        .map(ToOwned::to_owned)
}

fn metadata_object_marker(hash: &str) -> Value {
    json!({ "__webMetadataHash": hash })
}

fn annotation_object_marker(hash: &str) -> Value {
    json!({ "__webAnnotationHash": hash })
}

fn tree_document_from_json(value: &Value) -> Result<TreeDocument, ApiError> {
    if !value.is_object() {
        return Err(ApiError::bad_request("commit tree must be an object"));
    }
    let project_metadata = parse_project_metadata(value.get("projectMetadata"))?;
    let metadata_hash_values = value
        .get("datasetMetadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let metadata_raw_values = value
        .get("datasetMetadataValues")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut metadata_by_key = if metadata_raw_values.is_empty() {
        metadata_hash_values.clone()
    } else {
        metadata_raw_values
    }
    .into_iter()
    .collect::<BTreeMap<_, _>>();
    let mut metadata = Vec::new();
    if let Some(datasets) = value.get("datasets").and_then(Value::as_array) {
        for dataset in datasets {
            let key = dataset
                .get("datasetKey")
                .or_else(|| dataset.get("key"))
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::bad_request("tree dataset is missing datasetKey"))?;
            let key = validate_dataset_key(key)?;
            let dataset_id = value_uuid(
                dataset
                    .get("datasetUuid")
                    .or_else(|| dataset.get("datasetId")),
            );
            let metadata_hash = dataset
                .get("metadataHash")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
                .or_else(|| metadata_hash_values.get(&key).and_then(metadata_hash_value));
            let metadata_json = metadata_by_key
                .remove(&key)
                .map(|value| {
                    if let Some(hash) = metadata_hash_value(&value) {
                        metadata_object_marker(&hash)
                    } else {
                        value
                    }
                })
                .or_else(|| metadata_hash.as_deref().map(metadata_object_marker))
                .unwrap_or_else(|| json!({}));
            metadata.push(MetadataRow {
                dataset_key: key.to_string(),
                dataset_id,
                metadata_hash,
                metadata: metadata_json,
            });
        }
    }
    for (key, metadata_json) in metadata_by_key {
        let metadata_hash = metadata_hash_values
            .get(&key)
            .and_then(metadata_hash_value)
            .or_else(|| metadata_hash_value(&metadata_json));
        let metadata_json = if let Some(hash) = metadata_hash.as_deref() {
            metadata_object_marker(hash)
        } else {
            metadata_json
        };
        metadata.push(MetadataRow {
            dataset_key: validate_dataset_key(&key)?,
            dataset_id: None,
            metadata_hash,
            metadata: metadata_json,
        });
    }
    for (key, hash_value) in metadata_hash_values {
        if metadata.iter().any(|metadata| metadata.dataset_key == key) {
            continue;
        }
        let Some(hash) = metadata_hash_value(&hash_value) else {
            continue;
        };
        metadata.push(MetadataRow {
            dataset_key: validate_dataset_key(&key)?,
            dataset_id: None,
            metadata_hash: Some(hash.clone()),
            metadata: metadata_object_marker(&hash),
        });
    }
    if let Some(dataset_ids) = value.get("datasetIds").and_then(Value::as_array) {
        for dataset_id in dataset_ids.iter().filter_map(Value::as_str) {
            let key = validate_dataset_key(dataset_id)?;
            if metadata.iter().any(|metadata| metadata.dataset_key == key) {
                continue;
            }
            metadata.push(MetadataRow {
                dataset_key: key.to_string(),
                dataset_id: Uuid::parse_str(dataset_id).ok(),
                metadata_hash: None,
                metadata: json!({}),
            });
        }
    }
    metadata.sort_by(|left, right| left.dataset_key.cmp(&right.dataset_key));
    let metadata_dataset_keys = metadata
        .iter()
        .map(|metadata| metadata.dataset_key.clone())
        .chain(
            value
                .get("entries")
                .and_then(Value::as_array)
                .into_iter()
                .flat_map(|entries| entries.iter())
                .filter_map(|entry| {
                    entry
                        .get("datasetKey")
                        .or_else(|| entry.get("datasetId"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                }),
        );
    validate_project_metadata_for_tree(&project_metadata, metadata_dataset_keys)?;
    let entries = value
        .get("entries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|entry| {
            let dataset_key = entry
                .get("datasetKey")
                .or_else(|| entry.get("datasetId"))
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::bad_request("tree entry is missing datasetKey"))?;
            let dataset_key = validate_dataset_key(dataset_key)?;
            let image_id = entry
                .get("imageId")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::bad_request("tree entry is missing imageId"))?
                .to_string();
            let image_hash = entry
                .get("imageHash")
                .and_then(Value::as_str)
                .ok_or_else(|| ApiError::bad_request("tree entry is missing imageHash"))?
                .to_string();
            let annotation_hash = entry
                .get("annotationHash")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let annotation = entry
                .get("annotation")
                .or_else(|| entry.get("annotationJson"))
                .cloned()
                .unwrap_or_else(|| {
                    if valid_hash(&annotation_hash) {
                        annotation_object_marker(&annotation_hash)
                    } else {
                        json!([])
                    }
                });
            Ok(WorkEntry {
                dataset_key,
                dataset_id: value_uuid(entry.get("datasetUuid").or_else(|| entry.get("datasetId"))),
                image_id,
                file_name: entry
                    .get("fileName")
                    .and_then(Value::as_str)
                    .unwrap_or("image")
                    .to_string(),
                image_hash,
                image_byte_size: entry.get("imageByteSize").and_then(Value::as_i64),
                image_mime_type: entry
                    .get("imageMimeType")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                annotation_hash,
                annotation,
                status: entry
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("raw")
                    .to_string(),
                qa_status: entry
                    .get("qaStatus")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_string(),
                split: entry
                    .get("split")
                    .and_then(Value::as_str)
                    .unwrap_or("train")
                    .to_string(),
                width: entry.get("width").and_then(Value::as_i64).unwrap_or(0) as i32,
                height: entry.get("height").and_then(Value::as_i64).unwrap_or(0) as i32,
                review_note: entry
                    .get("reviewNote")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                tags: entry.get("tags").cloned().unwrap_or_else(|| json!([])),
                source_item_id: value_uuid(entry.get("sourceItemId")),
                revision: entry.get("revision").and_then(Value::as_i64).unwrap_or(1),
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(TreeDocument {
        project_metadata,
        metadata,
        entries,
    })
}

/// Decode the provisional tree stored by a merge session.
///
/// A merge candidate intentionally retains both sides of a conflicting
/// dataset-group assignment until the user chooses one.  The normal tree
/// decoder rejects that duplicate membership (as it must for commits and
/// ordinary worktrees), so use a validation copy with duplicate members
/// removed only while decoding the rest of the candidate.  The original
/// metadata is then restored on the returned tree, allowing the resolver to
/// apply the user's choice atomically instead of silently dropping a side.
fn tree_document_from_merge_candidate(value: &Value) -> Result<TreeDocument, ApiError> {
    if !value.is_object() {
        return Err(ApiError::bad_request("commit tree must be an object"));
    }
    let mut project_metadata = match value.get("projectMetadata") {
        Some(value) if !value.is_null() => serde_json::from_value(value.clone())
            .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))?,
        _ => ProjectMetadata::default(),
    };
    project_metadata.canonicalize();

    let mut validation_value = value.clone();
    let mut validation_metadata = project_metadata.clone();
    let mut seen_dataset_keys = BTreeSet::new();
    for group in &mut validation_metadata.dataset_groups {
        group
            .dataset_keys
            .retain(|dataset_key| seen_dataset_keys.insert(dataset_key.clone()));
    }
    let encoded_metadata = data_project_metadata::to_value(&validation_metadata)
        .map_err(ApiError::internal_message)?;
    validation_value
        .as_object_mut()
        .ok_or_else(|| ApiError::bad_request("commit tree must be an object"))?
        .insert("projectMetadata".to_string(), encoded_metadata);

    let mut tree = tree_document_from_json(&validation_value)?;
    let available_dataset_keys = tree_dataset_keys(&tree);
    validate_merge_project_metadata(&project_metadata, &available_dataset_keys)?;
    tree.project_metadata = project_metadata;
    Ok(tree)
}

async fn load_commit_tree(
    state: &AppState,
    project_id: Uuid,
    commit_id: Option<&str>,
) -> Result<TreeDocument, ApiError> {
    let Some(commit_id) = commit_id else {
        return Ok(TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: Vec::new(),
            entries: Vec::new(),
        });
    };
    let row = sqlx::query(
        "SELECT tree_json FROM data_project_commits WHERE project_id = $1 AND commit_id = $2",
    )
    .bind(project_id)
    .bind(commit_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("commit not found"))?;
    tree_document_from_json(&row.get::<Value, _>("tree_json"))
}

async fn load_ref_head(
    state: &AppState,
    project_id: Uuid,
    branch: &str,
) -> Result<Option<String>, ApiError> {
    let value: Option<Option<String>> = sqlx::query_scalar(
        "SELECT commit_id FROM data_project_refs r JOIN data_project_branches b ON b.project_id = r.project_id AND b.branch_name = r.ref_name WHERE r.project_id = $1 AND r.ref_name = $2 AND b.deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_optional(&state.pool)
    .await?;
    Ok(value.flatten())
}

fn tree_map_entries(tree: &TreeDocument) -> BTreeMap<(String, String), Value> {
    tree.entries
        .iter()
        .map(|entry| {
            (
                (entry.dataset_key.clone(), entry.image_id.clone()),
                json!({
                    "datasetKey": entry.dataset_key,
                    "imageId": entry.image_id,
                    "fileName": entry.file_name,
                    "imageHash": entry.image_hash,
                    "annotationHash": entry.annotation_hash,
                    "status": entry.status,
                    "qaStatus": entry.qa_status,
                    "split": entry.split,
                    "width": entry.width,
                    "height": entry.height,
                    "reviewNote": entry.review_note,
                    "tags": entry.tags
                }),
            )
        })
        .collect()
}

fn tree_map_metadata(tree: &TreeDocument) -> BTreeMap<String, Value> {
    tree.metadata
        .iter()
        .map(|metadata| (metadata.dataset_key.clone(), metadata.metadata.clone()))
        .collect()
}

fn tree_dataset_keys(tree: &TreeDocument) -> BTreeSet<String> {
    tree.metadata
        .iter()
        .map(|metadata| metadata.dataset_key.clone())
        .chain(tree.entries.iter().map(|entry| entry.dataset_key.clone()))
        .collect()
}

fn change(
    kind: impl Into<String>,
    dataset_key: &str,
    image_id: Option<&str>,
    file_name: Option<&str>,
    detail: impl Into<String>,
) -> WebChange {
    WebChange {
        kind: kind.into(),
        dataset_id: dataset_key.to_string(),
        image_id: image_id.map(ToOwned::to_owned),
        file_name: file_name.map(ToOwned::to_owned),
        detail: detail.into(),
    }
}

fn diff_trees(base: &TreeDocument, current: &TreeDocument) -> Vec<WebChange> {
    let mut changes = Vec::new();
    if base.project_metadata != current.project_metadata {
        changes.push(change(
            "dataset-group-changed",
            "__project__",
            None,
            None,
            "dataset group membership or names changed",
        ));
    }
    let base_metadata = tree_map_metadata(base);
    let current_metadata = tree_map_metadata(current);
    let metadata_keys = base_metadata
        .keys()
        .chain(current_metadata.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for key in metadata_keys {
        match (base_metadata.get(&key), current_metadata.get(&key)) {
            (None, Some(_)) => changes.push(change(
                "dataset-added",
                &key,
                None,
                None,
                "dataset membership added",
            )),
            (Some(_), None) => changes.push(change(
                "dataset-removed",
                &key,
                None,
                None,
                "dataset membership removed",
            )),
            (Some(before), Some(after)) if before != after => changes.push(change(
                "dataset-changed",
                &key,
                None,
                None,
                "dataset metadata or classes changed",
            )),
            _ => {}
        }
    }
    let base_entries = tree_map_entries(base);
    let current_entries = tree_map_entries(current);
    let keys = base_entries
        .keys()
        .chain(current_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    for (dataset_key, image_id) in keys {
        match (
            base_entries.get(&(dataset_key.clone(), image_id.clone())),
            current_entries.get(&(dataset_key.clone(), image_id.clone())),
        ) {
            (None, Some(after)) => changes.push(change(
                "image-added",
                &dataset_key,
                Some(&image_id),
                after.get("fileName").and_then(Value::as_str),
                "image added",
            )),
            (Some(before), None) => changes.push(change(
                "image-removed",
                &dataset_key,
                Some(&image_id),
                before.get("fileName").and_then(Value::as_str),
                "image removed",
            )),
            (Some(before), Some(after)) if before != after => {
                let kind = if before.get("annotationHash") != after.get("annotationHash") {
                    "annotation-changed"
                } else if before.get("status") != after.get("status")
                    || before.get("qaStatus") != after.get("qaStatus")
                {
                    "review-changed"
                } else if before.get("imageHash") != after.get("imageHash") {
                    "image-changed"
                } else {
                    "entry-changed"
                };
                changes.push(change(
                    kind,
                    &dataset_key,
                    Some(&image_id),
                    after.get("fileName").and_then(Value::as_str),
                    "image, annotation, review, split, or metadata changed",
                ));
            }
            _ => {}
        }
        if changes.len() >= MAX_CHANGE_COUNT {
            break;
        }
    }
    changes.truncate(MAX_CHANGE_COUNT);
    changes
}

fn counts_for_entries(entries: &[WorkEntry]) -> WebCounts {
    let mut counts = WebCounts {
        raw: 0,
        in_progress: 0,
        pending_review: 0,
        approved: 0,
        rejected: 0,
        total: entries.len() as i64,
    };
    for entry in entries {
        match status_bucket(&entry.status, &entry.qa_status, &entry.annotation) {
            "raw" => counts.raw += 1,
            "pendingReview" => counts.pending_review += 1,
            "approved" => counts.approved += 1,
            "rejected" => counts.rejected += 1,
            _ => counts.in_progress += 1,
        }
    }
    counts
}

async fn insert_metadata_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    branch: &str,
    metadata: &MetadataRow,
    user_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_worktree_metadata (project_id, branch_name, dataset_key, dataset_id, metadata_hash, metadata_json, revision, updated_by) VALUES ($1, $2, $3, $4, $5, $6, 1, $7) ON CONFLICT (project_id, branch_name, dataset_key) DO UPDATE SET dataset_id = excluded.dataset_id, metadata_hash = excluded.metadata_hash, metadata_json = excluded.metadata_json, revision = data_project_worktree_metadata.revision + 1, updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(project_id)
    .bind(branch)
    .bind(&metadata.dataset_key)
    .bind(metadata.dataset_id)
    .bind(&metadata.metadata_hash)
    .bind(&metadata.metadata)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_entry_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    branch: &str,
    entry: &WorkEntry,
    user_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_worktree_entries (project_id, branch_name, dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, 1, $20) ON CONFLICT (project_id, branch_name, dataset_key, image_id) DO UPDATE SET dataset_id = excluded.dataset_id, file_name = excluded.file_name, image_hash = excluded.image_hash, image_byte_size = excluded.image_byte_size, image_mime_type = excluded.image_mime_type, annotation_hash = excluded.annotation_hash, annotation_json = excluded.annotation_json, status = excluded.status, qa_status = excluded.qa_status, split = excluded.split, width = excluded.width, height = excluded.height, review_note = excluded.review_note, tags_json = excluded.tags_json, source_item_id = excluded.source_item_id, revision = data_project_worktree_entries.revision + 1, updated_by = excluded.updated_by, updated_at = now()",
    )
    .bind(project_id)
    .bind(branch)
    .bind(&entry.dataset_key)
    .bind(entry.dataset_id)
    .bind(&entry.image_id)
    .bind(&entry.file_name)
    .bind(&entry.image_hash)
    .bind(entry.image_byte_size)
    .bind(&entry.image_mime_type)
    .bind(&entry.annotation_hash)
    .bind(&entry.annotation)
    .bind(&entry.status)
    .bind(&entry.qa_status)
    .bind(&entry.split)
    .bind(entry.width)
    .bind(entry.height)
    .bind(&entry.review_note)
    .bind(&entry.tags)
    .bind(entry.source_item_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_tree_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    branch: &str,
    tree: &TreeDocument,
    user_id: Uuid,
    clear: bool,
) -> Result<(), ApiError> {
    if clear {
        sqlx::query(
            "DELETE FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2",
        )
        .bind(project_id)
        .bind(branch)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "DELETE FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2",
        )
        .bind(project_id)
        .bind(branch)
        .execute(&mut **tx)
        .await?;
    }
    let project_metadata = data_project_metadata::to_value(&tree.project_metadata)
        .map_err(ApiError::internal_message)?;
    sqlx::query(
        "UPDATE data_project_worktrees SET project_metadata_json = $3, updated_at = now(), updated_by = $4 WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .bind(project_metadata)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    for metadata in &tree.metadata {
        insert_metadata_tx(tx, project_id, branch, metadata, user_id).await?;
    }
    for entry in &tree.entries {
        insert_entry_tx(tx, project_id, branch, entry, user_id).await?;
    }
    Ok(())
}

async fn load_commit_tree_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    commit_id: Option<&str>,
) -> Result<TreeDocument, ApiError> {
    let Some(commit_id) = commit_id else {
        return Ok(TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: Vec::new(),
            entries: Vec::new(),
        });
    };
    let row = sqlx::query(
        "SELECT tree_json FROM data_project_commits WHERE project_id = $1 AND commit_id = $2",
    )
    .bind(project_id)
    .bind(commit_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("commit not found"))?;
    tree_document_from_json(&row.get::<Value, _>("tree_json"))
}

async fn populate_from_sources_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    branch: &str,
    user_id: Uuid,
) -> Result<(), ApiError> {
    let dataset_rows = sqlx::query(
        "SELECT pd.dataset_key, pd.dataset_id, COALESCE(d.schema_json, pd.metadata_json) AS metadata_json, d.organization_id AS source_organization_id, d.deleted_at AS source_deleted_at, d.status AS source_status, p.organization_id AS project_organization_id FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id LEFT JOIN datasets d ON d.id = pd.dataset_id WHERE pd.project_id = $1 ORDER BY pd.position, pd.dataset_key",
    )
    .bind(project_id)
    .fetch_all(&mut **tx)
    .await?;
    for dataset_row in dataset_rows {
        let dataset_key: String = dataset_row.get("dataset_key");
        let dataset_id: Option<Uuid> = dataset_row.get("dataset_id");
        if dataset_id.is_some() {
            let source_organization_id: Option<Uuid> = dataset_row.get("source_organization_id");
            let source_deleted_at: Option<DateTime<Utc>> = dataset_row.get("source_deleted_at");
            let source_status: Option<String> = dataset_row.get("source_status");
            let project_organization_id: Option<Uuid> = dataset_row.get("project_organization_id");
            if source_organization_id.is_none()
                || source_deleted_at.is_some()
                || source_status
                    .as_deref()
                    .is_some_and(|status| status == "deleted")
                || project_organization_id != source_organization_id
            {
                return Err(ApiError::conflict(
                    "SOURCE_DATASET_SCOPE_MISMATCH",
                    "data-project source dataset is deleted or outside the project organization",
                    json!({ "datasetKey": dataset_key, "datasetId": dataset_id }),
                ));
            }
        }
        let metadata = dataset_row.get::<Value, _>("metadata_json");
        let metadata_hash = json_hash(&metadata).ok();
        insert_metadata_tx(
            tx,
            project_id,
            branch,
            &MetadataRow {
                dataset_key: dataset_key.clone(),
                dataset_id,
                metadata_hash,
                metadata,
            },
            user_id,
        )
        .await?;
        let Some(dataset_id) = dataset_id else {
            continue;
        };
        let item_rows = sqlx::query(
            "SELECT di.id, di.file_name, di.split, di.annotation_json, db.object_key, db.content_hash, db.mime_type, db.width, db.height, db.byte_size FROM dataset_items di JOIN dataset_blobs db ON db.id = di.blob_id WHERE di.dataset_id = $1 ORDER BY di.created_at, di.id",
        )
        .bind(dataset_id)
        .fetch_all(&mut **tx)
        .await?;
        for item in item_rows {
            let image_hash: String = item.get("content_hash");
            let image_byte_size: i64 = item.get("byte_size");
            let image_mime_type: String = item.get("mime_type");
            let source_object_key: String = item.get("object_key");
            insert_project_blob_tx(
                tx,
                project_id,
                user_id,
                &image_hash,
                image_byte_size,
                "image",
                &image_mime_type,
                &source_object_key,
            )
            .await?;
            let annotation = item.get::<Value, _>("annotation_json");
            let status = status_for_annotation(&annotation);
            let qa_status = default_qa_status();
            let annotation_hash = annotation_hash(&annotation, &status, &qa_status)?;
            let entry = WorkEntry {
                dataset_key: dataset_key.clone(),
                dataset_id: Some(dataset_id),
                image_id: item.get::<Uuid, _>("id").to_string(),
                file_name: item.get("file_name"),
                image_hash,
                image_byte_size: Some(image_byte_size),
                image_mime_type: Some(image_mime_type),
                annotation_hash,
                annotation,
                status,
                qa_status,
                split: item.get("split"),
                width: item.get("width"),
                height: item.get("height"),
                review_note: None,
                tags: json!([]),
                source_item_id: Some(item.get("id")),
                revision: 1,
            };
            insert_entry_tx(tx, project_id, branch, &entry, user_id).await?;
        }
    }
    Ok(())
}

fn should_restore_empty_worktree_from_base(
    existing_entries: i64,
    existing_metadata: i64,
    base_commit_id: Option<&str>,
    worktree_revision: i64,
) -> bool {
    existing_entries == 0
        && existing_metadata == 0
        && base_commit_id.is_some()
        // Revision 1 is the branch-creation/first-bootstrap state.  A
        // committed or membership-edited worktree has advanced beyond it;
        // restoring its base snapshot would resurrect an intentional empty
        // tree after an unassign operation.
        && worktree_revision == 1
}

fn next_worktree_revision(current: i64) -> Result<i64, ApiError> {
    current.checked_add(1).ok_or_else(|| {
        ApiError::conflict(
            "WORKTREE_REVISION_EXHAUSTED",
            "worktree revision exhausted",
            Value::Null,
        )
    })
}

async fn ensure_worktree_initialized(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    branch: &str,
) -> Result<(), ApiError> {
    let branch = validate_branch(branch)?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let row = sqlx::query(
        "SELECT wt.base_commit_id, wt.revision, r.commit_id AS ref_commit_id FROM data_project_worktrees wt JOIN data_project_branches b ON b.project_id = wt.project_id AND b.branch_name = wt.branch_name LEFT JOIN data_project_refs r ON r.project_id = wt.project_id AND r.ref_name = wt.branch_name WHERE wt.project_id = $1 AND wt.branch_name = $2 AND b.deleted_at IS NULL FOR UPDATE OF wt",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project branch not found"))?;
    let worktree_revision: i64 = row.get("revision");
    let base_commit = row.get::<Option<String>, _>("base_commit_id");
    let ref_commit = row.get::<Option<String>, _>("ref_commit_id");
    let existing_entries: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_one(&mut *tx)
    .await?;
    let existing_metadata: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_one(&mut *tx)
    .await?;
    if base_commit != ref_commit {
        // Native sync can advance the immutable graph while a browser draft
        // is still present.  Refresh only a clean draft: a dirty browser
        // worktree is user state and must never be overwritten by a remote
        // ref update.  The row lock above serializes this compare-and-swap
        // with browser edits and the revision bump makes stale writes fail.
        let (_, _, current_metadata, current_metadata_rows, current_entries) =
            load_worktree_tx(&mut tx, project_id, &branch).await?;
        let current = TreeDocument {
            project_metadata: current_metadata,
            metadata: current_metadata_rows,
            entries: current_entries,
        };
        let base = load_commit_tree_tx(&mut tx, project_id, base_commit.as_deref()).await?;
        if diff_trees(&base, &current).is_empty() {
            let target = load_commit_tree_tx(&mut tx, project_id, ref_commit.as_deref()).await?;
            let next_revision = next_worktree_revision(worktree_revision)?;
            insert_tree_tx(&mut tx, project_id, &branch, &target, user_id, true).await?;
            sqlx::query(
                "UPDATE data_project_worktrees SET base_commit_id = $3, revision = $4, updated_by = $5, updated_at = now() WHERE project_id = $1 AND branch_name = $2",
            )
            .bind(project_id)
            .bind(&branch)
            .bind(ref_commit.as_deref())
            .bind(next_revision)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
    } else if existing_entries == 0 && existing_metadata == 0 {
        if should_restore_empty_worktree_from_base(
            existing_entries,
            existing_metadata,
            base_commit.as_deref(),
            worktree_revision,
        ) {
            let tree = load_commit_tree_tx(&mut tx, project_id, base_commit.as_deref()).await?;
            insert_tree_tx(&mut tx, project_id, &branch, &tree, user_id, false).await?;
        } else if base_commit.is_none() && worktree_revision == 1 {
            populate_from_sources_tx(&mut tx, project_id, &branch, user_id).await?;
        }
    }
    tx.commit().await?;
    Ok(())
}

async fn set_active_branch(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    branch: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_active_branches (project_id, user_id, branch_name) VALUES ($1, $2, $3) ON CONFLICT (project_id, user_id) DO UPDATE SET branch_name = excluded.branch_name, updated_at = now()",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(branch)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn active_branch(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
) -> Result<String, ApiError> {
    let branch = sqlx::query_scalar::<_, String>(
        "SELECT ab.branch_name FROM data_project_active_branches ab JOIN data_project_branches b ON b.project_id = ab.project_id AND b.branch_name = ab.branch_name WHERE ab.project_id = $1 AND ab.user_id = $2 AND b.deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(branch) = branch {
        return Ok(branch);
    }
    ensure_main_branch(state, project_id, user_id).await?;
    ensure_worktree_initialized(state, project_id, user_id, "main").await?;
    set_active_branch(state, project_id, user_id, "main").await?;
    Ok("main".to_string())
}

async fn ensure_existing_branch(
    state: &AppState,
    project_id: Uuid,
    branch: &str,
) -> Result<(), ApiError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM data_project_branches WHERE project_id = $1 AND branch_name = $2 AND deleted_at IS NULL)",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_one(&state.pool)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::not_found("data-project branch not found"))
    }
}

async fn load_commit_row(
    state: &AppState,
    project_id: Uuid,
    commit_id: &str,
) -> Result<PgRow, ApiError> {
    sqlx::query(
        "SELECT commit_id, parent_ids_json, message, tree_json, created_at FROM data_project_commits WHERE project_id = $1 AND commit_id = $2",
    )
    .bind(project_id)
    .bind(commit_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("commit not found"))
}

fn commit_from_row(
    project_id: Uuid,
    branch: Option<&str>,
    row: &PgRow,
) -> Result<WebCommit, ApiError> {
    let tree = tree_document_from_json(&row.get::<Value, _>("tree_json"))?;
    let parents = row
        .get::<Value, _>("parent_ids_json")
        .as_array()
        .cloned()
        .unwrap_or_default();
    let parent_id = parents
        .first()
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let second_parent_id = parents
        .get(1)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let dataset_ids = tree
        .metadata
        .iter()
        .map(|metadata| browser_dataset_id(project_id, &metadata.dataset_key))
        .collect::<Vec<_>>();
    let dataset_metadata = tree
        .metadata
        .iter()
        .map(|metadata| {
            (
                browser_dataset_id(project_id, &metadata.dataset_key),
                metadata.metadata_hash.clone().unwrap_or_else(|| {
                    json_hash(&metadata.metadata).unwrap_or_else(|_| String::new())
                }),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let entries = tree
        .entries
        .iter()
        .map(|entry| WebCommitEntry {
            dataset_id: browser_dataset_id(project_id, &entry.dataset_key),
            image_id: entry.image_id.clone(),
            file_name: entry.file_name.clone(),
            image_hash: entry.image_hash.clone(),
            annotation_hash: entry.annotation_hash.clone(),
            status: entry.status.clone(),
            qa_status: entry.qa_status.clone(),
            split: entry.split.clone(),
            width: entry.width,
            height: entry.height,
        })
        .collect();
    Ok(WebCommit {
        id: row.get("commit_id"),
        data_project_id: project_id,
        parent_id,
        second_parent_id,
        branch: branch.map(ToOwned::to_owned),
        message: row.get("message"),
        created_at: row.get("created_at"),
        dataset_ids,
        dataset_metadata,
        entries,
        project_metadata: tree.project_metadata,
    })
}

async fn project_summary(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
) -> Result<WebDataProject, ApiError> {
    let role = require_project(state, user_id, project_id, WebPermission::View).await?;
    let row = sqlx::query(
        "SELECT id, name, description, created_at, updated_at, revision FROM data_projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let dataset_keys = sqlx::query_scalar::<_, String>(
        "SELECT dataset_key FROM data_project_datasets WHERE project_id = $1 ORDER BY position, dataset_key",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(WebDataProject {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        dataset_ids: dataset_keys
            .iter()
            .map(|key| browser_dataset_id(project_id, key))
            .collect(),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
        account_scope: None,
        revision: row.get("revision"),
        role,
    })
}

async fn list_projects(state: &AppState, user_id: Uuid) -> Result<Vec<WebDataProject>, ApiError> {
    let project_ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT p.id FROM data_projects p JOIN data_project_members m ON m.project_id = p.id WHERE m.user_id = $1 AND m.deleted_at IS NULL AND p.deleted_at IS NULL AND p.status <> 'deleted' ORDER BY p.updated_at DESC, p.id",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    let mut result = Vec::with_capacity(project_ids.len());
    for project_id in project_ids {
        result.push(project_summary(state, user_id, project_id).await?);
    }
    Ok(result)
}

fn project_metadata_response(
    project_id: Uuid,
    branch: &str,
    revision: i64,
    metadata: &ProjectMetadata,
    can_edit: bool,
) -> Result<Value, ApiError> {
    Ok(json!({
        "dataProjectId": project_id,
        "branch": branch,
        "revision": revision,
        "projectMetadata": data_project_metadata::to_value(metadata)
            .map_err(ApiError::internal_message)?,
        "canEdit": can_edit
    }))
}

async fn get_data_project_dataset_groups(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let role = require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let (revision, _, project_metadata, _, _) = load_worktree(state, project_id, &branch).await?;
    project_metadata_response(
        project_id,
        &branch,
        revision,
        &project_metadata,
        role_allows(&role, WebPermission::Edit),
    )
}

async fn set_data_project_dataset_groups(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    let expected_revision = expected_revision(args)?.ok_or_else(|| {
        ApiError::bad_request("expectedRevision is required for dataset group updates")
    })?;
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let groups_value = arg(args, "groups")
        .cloned()
        .ok_or_else(|| ApiError::bad_request("groups is required"))?;
    let groups = if groups_value.is_array() {
        serde_json::from_value::<Vec<DatasetGroup>>(groups_value)
            .map_err(|error| ApiError::bad_request(format!("invalid groups: {error}")))?
    } else {
        parse_project_metadata(Some(&groups_value))?.dataset_groups
    };
    let mut project_metadata = ProjectMetadata {
        schema_version: data_project_metadata::PROJECT_METADATA_SCHEMA_VERSION,
        dataset_groups: groups,
    };
    project_metadata.canonicalize();
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let worktree = sqlx::query(
        "SELECT revision FROM data_project_worktrees WHERE project_id = $1 AND branch_name = $2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project branch worktree not found"))?;
    let current_revision: i64 = worktree.get("revision");
    if current_revision != expected_revision {
        return Err(ApiError::conflict(
            "WORKTREE_REVISION_MISMATCH",
            "working tree changed while updating dataset groups",
            json!({ "expectedRevision": expected_revision, "actualRevision": current_revision }),
        ));
    }
    let project_dataset_keys = sqlx::query_scalar::<_, String>(
        "SELECT dataset_key FROM data_project_datasets WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect::<BTreeSet<_>>();
    let branch_dataset_keys = sqlx::query_scalar::<_, String>(
        "SELECT dataset_key FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 UNION SELECT dataset_key FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect::<BTreeSet<_>>();
    let valid_dataset_keys = branch_dataset_keys
        .intersection(&project_dataset_keys)
        .cloned()
        .collect::<BTreeSet<_>>();
    data_project_metadata::validate(&project_metadata, Some(&valid_dataset_keys))
        .map_err(|error| ApiError::bad_request(format!("invalid groups: {error}")))?;
    let next_revision = next_worktree_revision(current_revision)?;
    let metadata_value =
        data_project_metadata::to_value(&project_metadata).map_err(ApiError::internal_message)?;
    let updated = sqlx::query(
        "UPDATE data_project_worktrees SET project_metadata_json = $3, revision = $4, updated_by = $5, updated_at = now() WHERE project_id = $1 AND branch_name = $2 AND revision = $6",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&metadata_value)
    .bind(next_revision)
    .bind(user.id)
    .bind(expected_revision)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::conflict(
            "WORKTREE_REVISION_MISMATCH",
            "working tree changed while updating dataset groups",
            Value::Null,
        ));
    }
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    project_metadata_response(project_id, &branch, next_revision, &project_metadata, true)
}

async fn create_project(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebDataProject, ApiError> {
    let name = required_string(args, "name")?;
    if name.len() > 240 {
        return Err(ApiError::bad_request("name is too long"));
    }
    let description = optional_string(args, "description")?.unwrap_or_default();
    if description.len() > 10_000 {
        return Err(ApiError::bad_request("description is too long"));
    }
    let organization_id = optional_uuid(args, "organizationId")?.or(user.organization_id);
    if organization_id.is_some_and(|organization_id| {
        user.organization_id
            .is_some_and(|user_org| user_org != organization_id)
            || user.organization_id.is_none()
    }) {
        return Err(ApiError::forbidden(
            "project organization must match the signed-in user",
        ));
    }
    let project_id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO data_projects (id, organization_id, owner_id, name, description, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $3, $3)",
    )
    .bind(project_id)
    .bind(organization_id)
    .bind(user.id)
    .bind(&name)
    .bind(&description)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_members (project_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    ensure_branch_tx(&mut tx, project_id, user.id, "main").await?;
    sqlx::query(
        "INSERT INTO data_project_active_branches (project_id, user_id, branch_name) VALUES ($1, $2, 'main') ON CONFLICT DO NOTHING",
    )
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    project_summary(state, user.id, project_id).await
}

fn metadata_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_manifest_string(value: &Value, key: &str) -> Option<String> {
    value
        .get("manifest")
        .and_then(|manifest| manifest.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn metadata_display_name(value: &Value, fallback: &str) -> String {
    metadata_manifest_string(value, "name")
        .or_else(|| metadata_string(value, "name"))
        .or_else(|| metadata_string(value, "datasetName"))
        .or_else(|| metadata_manifest_string(value, "sourceDatasetKey"))
        .unwrap_or_else(|| fallback.to_string())
}

fn metadata_format(value: &Value) -> Option<String> {
    metadata_manifest_string(value, "format").or_else(|| metadata_string(value, "format"))
}

fn metadata_display_description(value: &Value, format: Option<&str>) -> String {
    metadata_manifest_string(value, "description")
        .or_else(|| metadata_string(value, "description"))
        .or_else(|| format.map(|format| format!("远端历史数据集 · {format}")))
        .unwrap_or_else(|| "远端历史数据集".to_string())
}

fn metadata_status(value: &Value) -> String {
    metadata_string(value, "status")
        .or_else(|| metadata_manifest_string(value, "status"))
        .unwrap_or_else(|| "已导入".to_string())
}

fn annotation_types_for_schema(schema: &Value) -> Vec<String> {
    let existing = annotation_types(schema);
    if !existing.is_empty() {
        return existing;
    }
    match metadata_format(schema).as_deref() {
        Some("yolo-seg") => vec!["Polygon".to_string(), "BBox".to_string()],
        Some("image-classification") => vec!["Classification".to_string()],
        Some("yolo-detect") | Some("voc-detect") => vec!["BBox".to_string()],
        _ => Vec::new(),
    }
}

fn metadata_tags(value: &Value) -> Vec<String> {
    let mut tags = value
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(format) = metadata_format(value) {
        let tag = format!("format: {format}");
        if !tags.iter().any(|existing| existing == &tag) {
            tags.push(tag);
        }
    }
    tags
}

/// Resolve the immutable metadata object used by a graph-backed virtual
/// dataset.  Older push clients stored `{metadataHash: ...}` in the
/// membership row while current worktrees use the explicit marker; accept
/// both forms for listing without rewriting the graph or object store.
async fn listing_metadata_value(state: &AppState, project_id: Uuid, raw: &Value) -> Value {
    let candidate = raw
        .get("__webMetadataHash")
        .and_then(Value::as_str)
        .filter(|hash| valid_hash(hash))
        .or_else(|| {
            raw.get("metadataHash")
                .and_then(Value::as_str)
                .filter(|hash| valid_hash(hash))
        })
        .map(metadata_object_marker);
    match candidate {
        Some(marker) => hydrated_metadata_value(state, project_id, &marker)
            .await
            .unwrap_or_else(|_| raw.clone()),
        None => raw.clone(),
    }
}

async fn list_datasets(state: &AppState, user_id: Uuid) -> Result<Value, ApiError> {
    let rows = sqlx::query(
        "SELECT d.id, d.name, d.description, d.schema_json, d.status, d.revision, d.created_at, d.updated_at, pd.project_id, pd.dataset_key FROM datasets d LEFT JOIN data_project_datasets pd ON pd.dataset_id = d.id WHERE d.deleted_at IS NULL AND d.status <> 'deleted' AND ((pd.project_id IS NOT NULL AND EXISTS (SELECT 1 FROM data_project_members pm JOIN data_projects p ON p.id = pm.project_id WHERE pm.project_id = pd.project_id AND pm.user_id = $1 AND pm.deleted_at IS NULL AND p.deleted_at IS NULL AND p.status <> 'deleted')) OR (pd.project_id IS NULL AND EXISTS (SELECT 1 FROM dataset_members dm WHERE dm.dataset_id = d.id AND dm.user_id = $1 AND dm.deleted_at IS NULL))) ORDER BY d.updated_at DESC, d.id",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    let mut datasets = Vec::with_capacity(rows.len());
    for row in rows {
        let project_id: Option<Uuid> = row.get("project_id");
        let key: Option<String> = row.get("dataset_key");
        let id = project_id
            .zip(key.as_deref())
            .map(|(project_id, key)| browser_dataset_id(project_id, key))
            .unwrap_or_else(|| row.get::<Uuid, _>("id").to_string());
        if let Some(project_id) = project_id {
            if require_project(state, user_id, project_id, WebPermission::View)
                .await
                .is_err()
            {
                continue;
            }
        }
        datasets.push(json!({
            "id": id,
            "datasetId": row.get::<Uuid, _>("id"),
            "name": row.get::<String, _>("name"),
            "description": row.get::<String, _>("description"),
            "schema": row.get::<Value, _>("schema_json"),
            "status": row.get::<String, _>("status"),
            "revision": row.get::<i64, _>("revision"),
            "dataProjectId": project_id,
            "datasetKey": key,
            "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        }));
    }

    // A native/cloud graph can intentionally carry a dataset membership whose
    // `dataset_id` is NULL.  That is not an unavailable dataset: the stable
    // canonical key, immutable metadata object and worktree entries are the
    // dataset identity for the browser.  The old query above only joined the
    // hosted `datasets` table, which made these graph members disappear from
    // the browser project list even though every dataset/image command could
    // already resolve `<project>::<datasetKey>`.
    let virtual_rows = sqlx::query(
        "SELECT pd.project_id, pd.dataset_key, pd.metadata_json, p.created_at, p.updated_at FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id JOIN data_project_members pm ON pm.project_id = p.id WHERE pd.dataset_id IS NULL AND pm.user_id = $1 AND pm.deleted_at IS NULL AND p.deleted_at IS NULL AND p.status <> 'deleted' ORDER BY p.updated_at DESC, pd.position, pd.dataset_key",
    )
    .bind(user_id)
    .fetch_all(&state.pool)
    .await?;
    for row in virtual_rows {
        let project_id: Uuid = row.get("project_id");
        if require_project(state, user_id, project_id, WebPermission::View)
            .await
            .is_err()
        {
            continue;
        }
        let dataset_key: String = row.get("dataset_key");
        let mapping_metadata = row.get::<Value, _>("metadata_json");
        let branch = active_branch(state, project_id, user_id).await?;
        ensure_worktree_initialized(state, project_id, user_id, &branch).await?;
        let worktree_metadata = sqlx::query(
            "SELECT metadata_json FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
        )
        .bind(project_id)
        .bind(&branch)
        .bind(&dataset_key)
        .fetch_optional(&state.pool)
        .await?
        .map(|metadata| metadata.get::<Value, _>("metadata_json"));
        let raw_metadata = worktree_metadata.unwrap_or(mapping_metadata);
        let hydrated_metadata = listing_metadata_value(state, project_id, &raw_metadata).await;
        let format = metadata_format(&hydrated_metadata);
        datasets.push(json!({
            "id": browser_dataset_id(project_id, &dataset_key),
            "datasetId": Value::Null,
            "name": metadata_display_name(&hydrated_metadata, &dataset_key),
            "description": metadata_display_description(&hydrated_metadata, format.as_deref()),
            "schema": hydrated_metadata.clone(),
            "metadata": hydrated_metadata,
            "status": metadata_status(&hydrated_metadata),
            "format": format,
            "dataProjectId": project_id,
            "datasetKey": dataset_key,
            "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        }));
    }
    Ok(json!({ "datasets": datasets }))
}

async fn create_dataset(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let name = required_string(args, "name")?;
    if name.len() > 240 {
        return Err(ApiError::bad_request("dataset name is too long"));
    }
    let description = optional_string(args, "description")?.unwrap_or_default();
    let schema = optional_value(args, "schema")
        .or_else(|| optional_value(args, "classes"))
        .unwrap_or_else(|| json!({}));
    if !schema.is_object() && !schema.is_array() {
        return Err(ApiError::bad_request(
            "schema/classes must be an object or array",
        ));
    }
    let data_project_id = optional_uuid(args, "dataProjectId")?;
    if let Some(project_id) = data_project_id {
        require_project(state, user.id, project_id, WebPermission::Edit).await?;
    }
    let dataset_id = Uuid::new_v4();
    let dataset_key = dataset_id.to_string();
    let mut tx = state.pool.begin().await?;
    let organization_id = user.organization_id.ok_or_else(|| {
        ApiError::forbidden("an organization is required to create a server dataset")
    })?;
    if let Some(project_id) = data_project_id {
        let project_org: Option<Uuid> = sqlx::query_scalar(
            "SELECT organization_id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?;
        if project_org.is_some_and(|project_org| project_org != organization_id) {
            return Err(ApiError::forbidden(
                "dataset and data project organizations differ",
            ));
        }
    }
    sqlx::query(
        "INSERT INTO datasets (id, organization_id, owner_id, name, description, schema_json) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(dataset_id)
    .bind(organization_id)
    .bind(user.id)
    .bind(&name)
    .bind(&description)
    .bind(&schema)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO dataset_members (dataset_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(dataset_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    if let Some(project_id) = data_project_id {
        sqlx::query(
            "INSERT INTO data_project_datasets (project_id, dataset_key, dataset_id, metadata_json, position) VALUES ($1, $2, $3, $4, 0)",
        )
        .bind(project_id)
        .bind(&dataset_key)
        .bind(dataset_id)
        .bind(&schema)
        .execute(&mut *tx)
        .await?;
        ensure_branch_tx(&mut tx, project_id, user.id, "main").await?;
        bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    }
    tx.commit().await?;
    let id = data_project_id
        .map(|project_id| browser_dataset_id(project_id, &dataset_key))
        .unwrap_or_else(|| dataset_id.to_string());
    Ok(json!({
        "id": id,
        "datasetId": dataset_id,
        "datasetKey": data_project_id.map(|_| dataset_key.clone()),
        "dataProjectId": data_project_id,
        "name": name,
        "description": description,
        "schema": schema,
        "status": "active",
        "revision": 1
    }))
}

async fn populate_new_dataset_on_branch(
    state: &AppState,
    project_id: Uuid,
    branch: &str,
    dataset_key: &str,
    dataset_id: Uuid,
    user_id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let worktree_revision: i64 = sqlx::query_scalar(
        "SELECT wt.revision FROM data_project_worktrees wt JOIN data_project_branches b ON b.project_id = wt.project_id AND b.branch_name = wt.branch_name WHERE wt.project_id = $1 AND wt.branch_name = $2 AND b.deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project branch worktree not found"))?;
    let dataset = sqlx::query(
        "SELECT d.schema_json, d.organization_id, d.deleted_at, d.status, p.organization_id AS project_organization_id FROM datasets d JOIN data_projects p ON p.id = $2 WHERE d.id = $1 AND p.deleted_at IS NULL AND d.deleted_at IS NULL AND d.status <> 'deleted'",
    )
    .bind(dataset_id)
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let source_organization_id: Uuid = dataset.get("organization_id");
    let project_organization_id: Option<Uuid> = dataset.get("project_organization_id");
    if project_organization_id != Some(source_organization_id) {
        return Err(ApiError::conflict(
            "SOURCE_DATASET_SCOPE_MISMATCH",
            "data-project source dataset is outside the project organization",
            json!({ "datasetKey": dataset_key, "datasetId": dataset_id }),
        ));
    }
    let metadata = dataset.get::<Value, _>("schema_json");
    insert_metadata_tx(
        &mut tx,
        project_id,
        branch,
        &MetadataRow {
            dataset_key: dataset_key.to_string(),
            dataset_id: Some(dataset_id),
            metadata_hash: Some(json_hash(&metadata)?),
            metadata,
        },
        user_id,
    )
    .await?;
    let rows = sqlx::query(
        "SELECT di.id, di.file_name, di.split, di.annotation_json, db.object_key, db.content_hash, db.mime_type, db.width, db.height, db.byte_size FROM dataset_items di JOIN dataset_blobs db ON db.id = di.blob_id WHERE di.dataset_id = $1 ORDER BY di.created_at, di.id",
    )
    .bind(dataset_id)
    .fetch_all(&mut *tx)
    .await?;
    for row in rows {
        let image_hash: String = row.get("content_hash");
        let image_byte_size: i64 = row.get("byte_size");
        let image_mime_type: String = row.get("mime_type");
        let source_object_key: String = row.get("object_key");
        insert_project_blob_tx(
            &mut tx,
            project_id,
            user_id,
            &image_hash,
            image_byte_size,
            "image",
            &image_mime_type,
            &source_object_key,
        )
        .await?;
        let annotation = row.get::<Value, _>("annotation_json");
        let status = status_for_annotation(&annotation);
        let qa_status = default_qa_status();
        insert_entry_tx(
            &mut tx,
            project_id,
            branch,
            &WorkEntry {
                dataset_key: dataset_key.to_string(),
                dataset_id: Some(dataset_id),
                image_id: row.get::<Uuid, _>("id").to_string(),
                file_name: row.get("file_name"),
                image_hash,
                image_byte_size: Some(image_byte_size),
                image_mime_type: Some(image_mime_type),
                annotation_hash: annotation_hash(&annotation, &status, &qa_status)?,
                annotation,
                status,
                qa_status,
                split: row.get("split"),
                width: row.get("width"),
                height: row.get("height"),
                review_note: None,
                tags: json!([]),
                source_item_id: Some(row.get("id")),
                revision: 1,
            },
            user_id,
        )
        .await?;
    }
    let next_revision = next_worktree_revision(worktree_revision)?;
    let updated = sqlx::query(
        "UPDATE data_project_worktrees SET revision = $3, updated_by = $4, updated_at = now() WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .bind(next_revision)
    .bind(user_id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::not_found(
            "data-project branch worktree not found",
        ));
    }
    tx.commit().await?;
    Ok(())
}

async fn assign_dataset(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = required_string(args, "datasetId")?;
    let requested_project = optional_uuid(args, "dataProjectId")?;
    let (project_id, dataset_key, dataset_id) = if let Ok((encoded_project, key)) =
        parse_browser_dataset_id(&dataset_value)
    {
        let project_id = requested_project.unwrap_or(encoded_project);
        if project_id != encoded_project {
            return Err(ApiError::forbidden(
                "datasetId belongs to another data project",
            ));
        }
        let mapped = if requested_project.is_none() {
            // Unassignment is deliberately allowed for a tombstoned or
            // hidden source dataset: project ACL owns the relationship and
            // historical branch data must remain removable.
            require_project(state, user.id, project_id, WebPermission::Edit).await?;
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT dataset_id FROM data_project_datasets WHERE project_id = $1 AND dataset_key = $2",
            )
            .bind(project_id)
            .bind(&key)
            .fetch_optional(&state.pool)
            .await?
            .flatten()
        } else {
            ensure_project_dataset(state, user.id, project_id, &key, WebPermission::Edit).await?
        };
        (project_id, key, mapped)
    } else {
        let dataset_id = parse_uuid_string(&dataset_value, "datasetId")?;
        let project_id = requested_project.ok_or_else(|| {
            ApiError::bad_request("dataProjectId is required when assigning an unencoded dataset")
        })?;
        require_project(state, user.id, project_id, WebPermission::Edit).await?;
        let key = dataset_id.to_string();
        (project_id, key, Some(dataset_id))
    };
    if requested_project.is_none() {
        // A null project means unassign.  The browser ID still names the
        // owning project so the caller cannot remove another project's row.
        let mut tx = state.pool.begin().await?;
        sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
            .bind(project_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| ApiError::not_found("data project not found"))?;
        sqlx::query("DELETE FROM data_project_datasets WHERE project_id = $1 AND dataset_key = $2")
            .bind(project_id)
            .bind(&dataset_key)
            .execute(&mut *tx)
            .await?;
        let branch = sqlx::query_scalar::<_, String>(
            "SELECT branch_name FROM data_project_active_branches WHERE project_id = $1 AND user_id = $2",
        )
        .bind(project_id)
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or_else(|| "main".to_string());
        sqlx::query(
            "DELETE FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
        )
        .bind(project_id)
        .bind(&branch)
        .bind(&dataset_key)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
        )
        .bind(project_id)
        .bind(&branch)
        .bind(&dataset_key)
        .execute(&mut *tx)
        .await?;
        // The active branch no longer contains this dataset.  Remove it from
        // branch-local card groups as part of the same transaction so a later
        // commit cannot publish a group member that is absent from the
        // project membership snapshot.  Historical commits remain immutable.
        let mut project_metadata = sqlx::query_scalar::<_, Value>(
            "SELECT project_metadata_json FROM data_project_worktrees WHERE project_id = $1 AND branch_name = $2 FOR UPDATE",
        )
        .bind(project_id)
        .bind(&branch)
        .fetch_optional(&mut *tx)
        .await?
        .map(|value| parse_project_metadata(Some(&value)))
        .transpose()?;
        if let Some(metadata) = project_metadata.as_mut() {
            for group in &mut metadata.dataset_groups {
                group.dataset_keys.retain(|key| key != &dataset_key);
            }
            metadata.canonicalize();
        }
        let project_metadata_value = project_metadata
            .as_ref()
            .map(data_project_metadata::to_value)
            .transpose()
            .map_err(ApiError::internal_message)?;
        sqlx::query(
            "UPDATE data_project_worktrees SET project_metadata_json = COALESCE($4, project_metadata_json), revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND branch_name = $2",
        )
        .bind(project_id)
        .bind(&branch)
        .bind(user.id)
        .bind(project_metadata_value)
        .execute(&mut *tx)
        .await?;
        bump_project_revision_tx(&mut tx, project_id, user.id).await?;
        tx.commit().await?;
        return Ok(
            json!({ "datasetId": browser_dataset_id(project_id, &dataset_key), "dataProjectId": Value::Null, "removed": true }),
        );
    }
    let dataset_id = dataset_id.ok_or_else(|| ApiError::not_found("dataset is not available"))?;
    let dataset = sqlx::query(
        "SELECT organization_id, schema_json FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted'",
    )
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    if user
        .organization_id
        .is_some_and(|org| org != dataset.get::<Uuid, _>("organization_id"))
    {
        return Err(ApiError::forbidden(
            "dataset organization differs from signed-in user",
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    // Serialize all new ownership decisions on the source dataset row.  The
    // legacy graph predates this web invariant and may already contain more
    // than one mapping, so migration deliberately keeps those rows intact;
    // every new assignment is nevertheless rejected when another project
    // owns the dataset.
    sqlx::query("SELECT id FROM datasets WHERE id = $1 FOR UPDATE")
        .bind(dataset_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let existing: Vec<Uuid> = sqlx::query_scalar(
        "SELECT project_id FROM data_project_datasets WHERE dataset_id = $1 FOR UPDATE",
    )
    .bind(dataset_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(existing) = existing
        .into_iter()
        .find(|existing| *existing != project_id)
    {
        return Err(ApiError::conflict(
            "DATASET_ALREADY_ASSIGNED",
            "dataset already belongs to another data project",
            json!({ "dataProjectId": existing }),
        ));
    }
    sqlx::query(
        "INSERT INTO data_project_datasets (project_id, dataset_key, dataset_id, metadata_json, position) VALUES ($1, $2, $3, $4, 0) ON CONFLICT (project_id, dataset_key) DO UPDATE SET dataset_id = excluded.dataset_id, metadata_json = excluded.metadata_json, updated_at = now()",
    )
    .bind(project_id)
    .bind(&dataset_key)
    .bind(dataset_id)
    .bind(dataset.get::<Value, _>("schema_json"))
    .execute(&mut *tx)
    .await?;
    ensure_branch_tx(&mut tx, project_id, user.id, "main").await?;
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    let branch = active_branch(state, project_id, user.id).await?;
    populate_new_dataset_on_branch(
        state,
        project_id,
        &branch,
        &dataset_key,
        dataset_id,
        user.id,
    )
    .await?;
    Ok(json!({
        "datasetId": browser_dataset_id(project_id, &dataset_key),
        "dataProjectId": project_id,
        "datasetKey": dataset_key,
        "removed": false
    }))
}

async fn working_tree(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    requested_branch: Option<String>,
) -> Result<WebWorkingTree, ApiError> {
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = match requested_branch {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let (revision, base_commit_id, project_metadata, metadata, entries) =
        load_worktree(state, project_id, &branch).await?;
    let current = TreeDocument {
        project_metadata: project_metadata.clone(),
        metadata,
        entries: entries.clone(),
    };
    let base = load_commit_tree(state, project_id, base_commit_id.as_deref()).await?;
    let changes = diff_trees(&base, &current);
    let head_commit_id = load_ref_head(state, project_id, &branch).await?;
    Ok(WebWorkingTree {
        data_project_id: project_id,
        branch,
        revision,
        counts: counts_for_entries(&entries),
        dirty: !changes.is_empty(),
        changes,
        head_commit_id,
        project_metadata,
    })
}

async fn list_branches(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
) -> Result<Vec<WebBranch>, ApiError> {
    require_project(state, user.id, project_id, WebPermission::View).await?;
    ensure_main_branch(state, project_id, user.id).await?;
    let current = active_branch(state, project_id, user.id).await?;
    let rows = sqlx::query(
        "SELECT b.branch_name, b.protected, b.created_at, b.updated_at, r.commit_id FROM data_project_branches b JOIN data_project_refs r ON r.project_id = b.project_id AND r.ref_name = b.branch_name WHERE b.project_id = $1 AND b.deleted_at IS NULL ORDER BY b.branch_name",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| WebBranch {
            data_project_id: project_id,
            name: row.get("branch_name"),
            head_commit_id: row.get("commit_id"),
            current: row.get::<String, _>("branch_name") == current,
            protected: row.get("protected"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        })
        .collect())
}

async fn commit_graph(
    state: &AppState,
    project_id: Uuid,
) -> Result<HashMap<String, Vec<String>>, ApiError> {
    let rows = sqlx::query(
        "SELECT commit_id, parent_ids_json FROM data_project_commits WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(rows
        .iter()
        .map(|row| {
            let parents = row
                .get::<Value, _>("parent_ids_json")
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            (row.get("commit_id"), parents)
        })
        .collect())
}

fn ancestor_set(graph: &HashMap<String, Vec<String>>, start: Option<&str>) -> HashSet<String> {
    let mut seen = HashSet::new();
    let Some(start) = start else {
        return seen;
    };
    let mut stack = vec![start.to_string()];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(parents) = graph.get(&id) {
            stack.extend(parents.iter().cloned());
        }
    }
    seen
}

fn is_ancestor(
    graph: &HashMap<String, Vec<String>>,
    ancestor: Option<&str>,
    descendant: Option<&str>,
) -> bool {
    match (ancestor, descendant) {
        (None, _) => true,
        (Some(ancestor), Some(descendant)) => {
            ancestor_set(graph, Some(descendant)).contains(ancestor)
        }
        (Some(_), None) => false,
    }
}

async fn create_branch(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebBranch, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let branch = validate_branch(&required_string(args, "name")?)?;
    if branch == "main" {
        return Err(ApiError::conflict(
            "BRANCH_EXISTS",
            "main branch already exists",
            Value::Null,
        ));
    }
    let from_commit = optional_string(args, "fromCommitId")?;
    let source_branch = optional_string(args, "sourceBranch")?
        .map(|value| validate_branch(&value))
        .transpose()?
        .unwrap_or(active_branch(state, project_id, user.id).await?);
    ensure_worktree_initialized(state, project_id, user.id, &source_branch).await?;
    let source_head = load_ref_head(state, project_id, &source_branch).await?;
    let from_commit = from_commit.or(source_head);
    if let Some(commit_id) = from_commit.as_deref() {
        load_commit_row(state, project_id, commit_id).await?;
    }
    let source_tree = if from_commit.is_some() {
        load_commit_tree(state, project_id, from_commit.as_deref()).await?
    } else {
        let (_, _, project_metadata, metadata, entries) =
            load_worktree(state, project_id, &source_branch).await?;
        TreeDocument {
            project_metadata,
            metadata,
            entries,
        }
    };
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let existing: Option<Option<DateTime<Utc>>> = sqlx::query_scalar(
        "SELECT deleted_at FROM data_project_branches WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_optional(&mut *tx)
    .await?;
    match existing {
        Some(None) => {
            return Err(ApiError::conflict(
                "BRANCH_EXISTS",
                "branch already exists",
                json!({ "branch": branch }),
            ))
        }
        Some(Some(_)) => {
            return Err(ApiError::conflict(
                "BRANCH_NAME_RETIRED",
                "a deleted branch name cannot be reused",
                json!({ "branch": branch }),
            ))
        }
        None => {}
    }
    sqlx::query(
        "INSERT INTO data_project_branches (project_id, branch_name, protected, created_by) VALUES ($1, $2, false, $3)",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_refs (project_id, ref_name, commit_id, revision, updated_by) VALUES ($1, $2, $3, 1, $4)",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&from_commit)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_worktrees (project_id, branch_name, base_commit_id, revision, updated_by) VALUES ($1, $2, $3, 1, $4)",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&from_commit)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    insert_tree_tx(&mut tx, project_id, &branch, &source_tree, user.id, false).await?;
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    Ok(WebBranch {
        data_project_id: project_id,
        name: branch,
        head_commit_id: from_commit,
        current: false,
        protected: false,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    })
}

async fn switch_branch(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let target = validate_branch(&required_string(args, "branch")?)?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    ensure_existing_branch(state, project_id, &target).await?;
    let current = active_branch(state, project_id, user.id).await?;
    let current_tree = working_tree(state, user, project_id, Some(current.clone())).await?;
    if current != target && current_tree.dirty {
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit or discard working-tree changes before switching branches",
            json!({ "branch": current, "revision": current_tree.revision }),
        ));
    }
    ensure_worktree_initialized(state, project_id, user.id, &target).await?;
    let head = load_ref_head(state, project_id, &target).await?;
    set_active_branch(state, project_id, user.id, &target).await?;
    Ok(json!({
        "dataProjectId": project_id,
        "branch": target,
        "commitId": head,
        "path": "",
        "dirty": false
    }))
}

async fn delete_branch(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let branch = validate_branch(&required_string(args, "branch")?)?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let current = active_branch(state, project_id, user.id).await?;
    if branch == "main" || branch == current {
        return Err(ApiError::conflict(
            "BRANCH_PROTECTED",
            "main or the active branch cannot be deleted",
            json!({ "branch": branch }),
        ));
    }
    ensure_existing_branch(state, project_id, &branch).await?;
    let head = load_ref_head(state, project_id, &branch).await?;
    let main_head = load_ref_head(state, project_id, "main").await?;
    let graph = commit_graph(state, project_id).await?;
    if !is_ancestor(&graph, head.as_deref(), main_head.as_deref()) {
        return Err(ApiError::conflict(
            "BRANCH_NOT_MERGED",
            "branch must be merged into main before deletion",
            json!({ "branch": branch, "headCommitId": head, "mainHeadCommitId": main_head }),
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let current_head: Option<String> = sqlx::query_scalar(
        "SELECT commit_id FROM data_project_refs WHERE project_id = $1 AND ref_name = $2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    if current_head != head {
        return Err(ApiError::conflict(
            "HEAD_MOVED",
            "branch head changed while deleting the branch",
            json!({ "branch": branch, "expectedHeadCommitId": head, "actualHeadCommitId": current_head }),
        ));
    }
    sqlx::query(
        "UPDATE data_project_refs SET commit_id = NULL, revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND ref_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE data_project_branches SET deleted_at = now(), updated_at = now() WHERE project_id = $1 AND branch_name = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(&branch)
    .execute(&mut *tx)
    .await?;
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    Ok(json!({ "dataProjectId": project_id, "branch": branch, "deleted": true }))
}

async fn list_commits(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    requested_branch: Option<String>,
) -> Result<Vec<WebCommit>, ApiError> {
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = match requested_branch {
        Some(value) => validate_branch(&value)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let head = load_ref_head(state, project_id, &branch).await?;
    let reachable = ancestor_set(&commit_graph(state, project_id).await?, head.as_deref());
    if reachable.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT commit_id, parent_ids_json, message, tree_json, created_at FROM data_project_commits WHERE project_id = $1 AND commit_id = ANY($2) ORDER BY created_at DESC, commit_id DESC",
    )
    .bind(project_id)
    .bind(reachable.into_iter().collect::<Vec<_>>())
    .fetch_all(&state.pool)
    .await?;
    rows.iter()
        .map(|row| commit_from_row(project_id, Some(&branch), row))
        .collect()
}

async fn project_blob_exists(
    state: &AppState,
    project_id: Uuid,
    hash: &str,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2)",
    )
    .bind(project_id)
    .bind(hash)
    .fetch_one(&state.pool)
    .await?)
}

async fn project_owner_scope(state: &AppState, project_id: Uuid) -> Result<String, ApiError> {
    let owner_id: Uuid = sqlx::query_scalar(
        "SELECT owner_id FROM data_projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    Ok(format!(
        "{}\n{}",
        state.config.public_base_url.trim_end_matches('/'),
        owner_id
    ))
}

async fn source_dataset_name(
    state: &AppState,
    dataset_id: Option<Uuid>,
    fallback: &str,
) -> Result<String, ApiError> {
    let Some(dataset_id) = dataset_id else {
        return Ok(fallback.to_string());
    };
    Ok(
        sqlx::query_scalar::<_, String>("SELECT name FROM datasets WHERE id = $1")
            .bind(dataset_id)
            .fetch_optional(&state.pool)
            .await?
            .unwrap_or_else(|| fallback.to_string()),
    )
}

async fn insert_project_blob(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    hash: &str,
    byte_size: i64,
    kind: &str,
    mime_type: &str,
    object_key: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_blobs (project_id, content_hash, byte_size, kind, mime_type, object_key, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (project_id, content_hash, byte_size) DO NOTHING",
    )
    .bind(project_id)
    .bind(hash)
    .bind(byte_size)
    .bind(kind)
    .bind(mime_type)
    .bind(object_key)
    .bind(user_id)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn insert_project_blob_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    user_id: Uuid,
    hash: &str,
    byte_size: i64,
    kind: &str,
    mime_type: &str,
    object_key: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO data_project_blobs (project_id, content_hash, byte_size, kind, mime_type, object_key, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT (project_id, content_hash, byte_size) DO NOTHING",
    )
    .bind(project_id)
    .bind(hash)
    .bind(byte_size)
    .bind(kind)
    .bind(mime_type)
    .bind(object_key)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn bump_project_revision_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    user_id: Uuid,
) -> Result<(), ApiError> {
    let updated = sqlx::query(
        "UPDATE data_projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::not_found("data project not found"));
    }
    Ok(())
}

async fn store_json_object(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    value: &Value,
    kind: &str,
    mime_type: &str,
) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ApiError::internal_message(format!("encode object: {error}")))?;
    let hash = hex::encode(Sha256::digest(&bytes));
    if project_blob_exists(state, project_id, &hash).await? {
        return Ok(hash);
    }
    let key = object_key(project_id, &hash);
    state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&key)
        .content_type(mime_type)
        .metadata("sha256", hash.clone())
        .body(ByteStream::from(bytes.clone()))
        .send()
        .await
        .map_err(|error| ApiError::internal_message(format!("store web object: {error}")))?;
    insert_project_blob(
        state,
        project_id,
        user_id,
        &hash,
        bytes.len() as i64,
        kind,
        mime_type,
        &key,
    )
    .await?;
    Ok(hash)
}

async fn ensure_source_image_blob(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    entry: &WorkEntry,
) -> Result<(), ApiError> {
    if !valid_hash(&entry.image_hash) {
        return Err(ApiError::conflict(
            "INVALID_IMAGE_HASH",
            "image content hash is not a SHA-256 digest",
            json!({ "imageId": entry.image_id, "hash": entry.image_hash }),
        ));
    }
    if project_blob_exists(state, project_id, &entry.image_hash).await? {
        return Ok(());
    }
    let Some(source_item_id) = entry.source_item_id else {
        return Err(ApiError::conflict(
            "MISSING_IMAGE_OBJECT",
            "image object is not present in the data-project object store",
            json!({ "imageId": entry.image_id, "hash": entry.image_hash }),
        ));
    };
    let row = sqlx::query(
        "SELECT db.object_key, db.byte_size, db.mime_type, db.content_hash FROM dataset_items di JOIN dataset_blobs db ON db.id = di.blob_id WHERE di.id = $1",
    )
    .bind(source_item_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::conflict("MISSING_IMAGE_OBJECT", "source image object not found", json!({ "imageId": entry.image_id })))?;
    let content_hash: String = row.get("content_hash");
    if content_hash != entry.image_hash {
        return Err(ApiError::conflict(
            "IMAGE_HASH_CHANGED",
            "source image content changed since the web draft was captured",
            json!({ "imageId": entry.image_id, "expected": entry.image_hash, "actual": content_hash }),
        ));
    }
    let byte_size: i64 = row.get("byte_size");
    let mime_type: String = row.get("mime_type");
    let object_key: String = row.get("object_key");
    insert_project_blob(
        state,
        project_id,
        user_id,
        &entry.image_hash,
        byte_size,
        "image",
        &mime_type,
        &object_key,
    )
    .await
}

async fn materialize_tree_objects(
    state: &AppState,
    project_id: Uuid,
    user_id: Uuid,
    tree: &mut TreeDocument,
) -> Result<(), ApiError> {
    let owner_scope = project_owner_scope(state, project_id).await?;
    for metadata in &mut tree.metadata {
        let marker_hash = metadata
            .metadata
            .get("__webMetadataHash")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(hash) = marker_hash.as_deref() {
            if metadata.metadata_hash.as_deref() != Some(hash) {
                return Err(ApiError::conflict(
                    "METADATA_HASH_MISMATCH",
                    "dataset metadata marker does not match its tree hash",
                    json!({ "hash": hash, "treeHash": metadata.metadata_hash, "datasetKey": metadata.dataset_key }),
                ));
            }
            if !project_blob_exists(state, project_id, hash).await? {
                return Err(ApiError::conflict(
                    "MISSING_OBJECT",
                    "dataset metadata object is not present in the project store",
                    json!({ "hash": hash, "datasetKey": metadata.dataset_key }),
                ));
            }
            continue;
        }
        let dataset_name =
            source_dataset_name(state, metadata.dataset_id, &metadata.dataset_key).await?;
        let image_count = tree
            .entries
            .iter()
            .filter(|entry| entry.dataset_key == metadata.dataset_key)
            .count();
        metadata.metadata = canonical_metadata_value(
            &metadata.metadata,
            metadata.dataset_id,
            &metadata.dataset_key,
            &dataset_name,
            image_count,
            &owner_scope,
        );
        let hash = json_hash(&metadata.metadata)?;
        if metadata.metadata_hash.as_deref() != Some(hash.as_str()) {
            metadata.metadata_hash = Some(hash.clone());
        }
        if !project_blob_exists(state, project_id, &hash).await? {
            store_json_object(
                state,
                project_id,
                user_id,
                &metadata.metadata,
                "dataset-metadata",
                "application/json",
            )
            .await?;
        }
    }
    for entry in &mut tree.entries {
        ensure_source_image_blob(state, project_id, user_id, entry).await?;
        let annotation_marker = entry
            .annotation
            .get("__webAnnotationHash")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        if let Some(hash) = annotation_marker.as_deref() {
            if hash != entry.annotation_hash {
                return Err(ApiError::conflict(
                    "ANNOTATION_HASH_MISMATCH",
                    "annotation marker does not match its tree hash",
                    json!({ "hash": hash, "treeHash": entry.annotation_hash, "imageId": entry.image_id }),
                ));
            }
            if !project_blob_exists(state, project_id, hash).await? {
                return Err(ApiError::conflict(
                    "MISSING_OBJECT",
                    "annotation object is not present in the project store",
                    json!({ "hash": entry.annotation_hash, "imageId": entry.image_id }),
                ));
            }
            continue;
        }
        let expected_annotation_hash =
            annotation_hash(&entry.annotation, &entry.status, &entry.qa_status)?;
        if entry.annotation_hash != expected_annotation_hash {
            entry.annotation_hash = expected_annotation_hash;
        }
        if !project_blob_exists(state, project_id, &entry.annotation_hash).await? {
            let payload = annotation_payload(&entry.annotation, &entry.status, &entry.qa_status);
            store_json_object(
                state,
                project_id,
                user_id,
                &payload,
                "annotation",
                "application/json",
            )
            .await?;
        }
    }
    Ok(())
}

async fn validate_tree_objects_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    tree: &TreeDocument,
) -> Result<(), ApiError> {
    for metadata in &tree.metadata {
        let Some(hash) = metadata.metadata_hash.as_deref() else {
            continue;
        };
        if !valid_hash(hash) {
            return Err(ApiError::bad_request("dataset metadata hash is invalid"));
        }
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2)",
        )
        .bind(project_id)
        .bind(hash)
        .fetch_one(&mut **tx)
        .await?;
        if !exists {
            return Err(ApiError::conflict(
                "MISSING_OBJECT",
                "commit references a missing dataset metadata object",
                json!({ "hash": hash }),
            ));
        }
    }
    for entry in &tree.entries {
        for (kind, hash) in [
            ("image", entry.image_hash.as_str()),
            ("annotation", entry.annotation_hash.as_str()),
        ] {
            if !valid_hash(hash) {
                return Err(ApiError::bad_request(format!("{kind} hash is invalid")));
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2)",
            )
            .bind(project_id)
            .bind(hash)
            .fetch_one(&mut **tx)
            .await?;
            if !exists {
                return Err(ApiError::conflict(
                    "MISSING_OBJECT",
                    "commit references a missing object",
                    json!({ "kind": kind, "hash": hash, "imageId": entry.image_id }),
                ));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct MergeFinalizeContext {
    session_id: Uuid,
    resolving_snapshot: Value,
    final_resolutions: Value,
}

async fn create_commit_for_tree(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    branch: &str,
    tree: &TreeDocument,
    parents: Vec<String>,
    message: &str,
    expected_head: Option<&str>,
    expected_worktree_revision: Option<i64>,
) -> Result<WebCommit, ApiError> {
    create_commit_for_tree_inner(
        state,
        user,
        project_id,
        branch,
        tree,
        parents,
        message,
        expected_head,
        expected_worktree_revision,
        None,
    )
    .await
}

async fn create_commit_for_tree_with_merge_finalize(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    branch: &str,
    tree: &TreeDocument,
    parents: Vec<String>,
    message: &str,
    expected_head: Option<&str>,
    expected_worktree_revision: Option<i64>,
    merge_finalize: MergeFinalizeContext,
) -> Result<WebCommit, ApiError> {
    create_commit_for_tree_inner(
        state,
        user,
        project_id,
        branch,
        tree,
        parents,
        message,
        expected_head,
        expected_worktree_revision,
        Some(&merge_finalize),
    )
    .await
}

async fn create_commit_for_tree_inner(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    branch: &str,
    tree: &TreeDocument,
    parents: Vec<String>,
    message: &str,
    expected_head: Option<&str>,
    expected_worktree_revision: Option<i64>,
    merge_finalize: Option<&MergeFinalizeContext>,
) -> Result<WebCommit, ApiError> {
    let message = message.trim();
    if message.is_empty() || message.len() > 2_000 {
        return Err(ApiError::bad_request(
            "commit message is required and must be <= 2000 characters",
        ));
    }
    let commit_id = Uuid::new_v4().to_string();
    let tree_json = tree_document_json(tree);
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    // A commit must never resurrect a soft-deleted branch.  All normal
    // callers create/validate the branch before entering this helper, but the
    // project lock also serializes a concurrent delete and this explicit
    // active-row check keeps the invariant local to the commit transaction.
    sqlx::query(
        "SELECT branch_name FROM data_project_branches WHERE project_id = $1 AND branch_name = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project branch not found"))?;
    let ref_row = sqlx::query(
        "SELECT commit_id FROM data_project_refs WHERE project_id = $1 AND ref_name = $2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(branch)
    .fetch_one(&mut *tx)
    .await?;
    let current_head: Option<String> = ref_row.get("commit_id");
    if expected_head != current_head.as_deref() {
        return Err(ApiError::conflict(
            "HEAD_MOVED",
            "branch head changed while the web command was running",
            json!({ "expectedHeadCommitId": expected_head, "actualHeadCommitId": current_head }),
        ));
    }
    let (worktree_revision, base_commit_id, _, _, _) =
        load_worktree_tx(&mut tx, project_id, branch).await?;
    if expected_worktree_revision.is_some_and(|revision| revision != worktree_revision) {
        return Err(ApiError::conflict(
            "WORKTREE_REVISION_MISMATCH",
            "working tree changed while the web command was running",
            json!({ "expectedRevision": expected_worktree_revision, "actualRevision": worktree_revision }),
        ));
    }
    if base_commit_id != current_head {
        return Err(ApiError::conflict(
            "STALE_WORKTREE",
            "working tree is based on another branch head",
            json!({ "baseCommitId": base_commit_id, "headCommitId": current_head }),
        ));
    }
    for parent in &parents {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM data_project_commits WHERE project_id = $1 AND commit_id = $2)",
        )
        .bind(project_id)
        .bind(parent)
        .fetch_one(&mut *tx)
        .await?;
        if !exists {
            return Err(ApiError::conflict(
                "MISSING_PARENT",
                "commit parent does not exist",
                json!({ "parentId": parent }),
            ));
        }
    }
    let dataset_keys = tree_dataset_keys(tree);
    data_project_metadata::validate(&tree.project_metadata, Some(&dataset_keys))
        .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))?;
    validate_tree_objects_tx(&mut tx, project_id, tree).await?;
    sqlx::query(
        "INSERT INTO data_project_commits (project_id, commit_id, parent_ids_json, message, tree_json, metadata_json, created_by, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, now())",
    )
    .bind(project_id)
    .bind(&commit_id)
    .bind(serde_json::to_value(&parents).unwrap_or_else(|_| json!([])))
    .bind(message)
    .bind(&tree_json)
    .bind(json!({ "schemaVersion": 1, "branch": branch }))
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    let project_revision: i64 = sqlx::query_scalar(
        "UPDATE data_projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE data_project_refs SET commit_id = $3, revision = $4, updated_by = $5, updated_at = now() WHERE project_id = $1 AND ref_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .bind(&commit_id)
    .bind(project_revision)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE data_project_worktrees SET base_commit_id = $3, revision = revision + 1, updated_by = $4, updated_at = now() WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .bind(&commit_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    insert_tree_tx(&mut tx, project_id, branch, tree, user.id, true).await?;
    sqlx::query(
        "UPDATE data_project_branches SET updated_at = now() WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(branch)
    .execute(&mut *tx)
    .await?;
    if let Some(merge_finalize) = merge_finalize {
        let finalized = sqlx::query(
            "UPDATE data_project_merge_sessions SET candidate_tree_json = $2, resolutions_json = $3, status = 'resolved', updated_at = now() WHERE id = $1 AND status = 'open' AND resolutions_json = $4",
        )
        .bind(merge_finalize.session_id)
        .bind(&tree_json)
        .bind(&merge_finalize.final_resolutions)
        .bind(&merge_finalize.resolving_snapshot)
        .execute(&mut *tx)
        .await?;
        if finalized.rows_affected() != 1 {
            return Err(ApiError::conflict(
                "MERGE_SESSION_MOVED",
                "merge session changed while finalizing the merge commit",
                json!({ "sessionId": merge_finalize.session_id }),
            ));
        }
    }
    tx.commit().await?;
    let row = load_commit_row(state, project_id, &commit_id).await?;
    commit_from_row(project_id, Some(branch), &row)
}

async fn commit_worktree(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebCommit, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let (revision, base, project_metadata, metadata, entries) =
        load_worktree(state, project_id, &branch).await?;
    let mut tree = TreeDocument {
        project_metadata,
        metadata,
        entries,
    };
    let base_tree = load_commit_tree(state, project_id, base.as_deref()).await?;
    if diff_trees(&base_tree, &tree).is_empty() {
        return Err(ApiError::conflict(
            "EMPTY_COMMIT",
            "working tree has no changes to commit",
            Value::Null,
        ));
    }
    let expected_head = optional_string(args, "expectedHeadCommitId")?;
    if expected_head.is_some() && expected_head != base {
        return Err(ApiError::conflict(
            "STALE_WORKTREE",
            "expected head does not match the working tree base",
            json!({ "expectedHeadCommitId": expected_head, "baseCommitId": base }),
        ));
    }
    materialize_tree_objects(state, project_id, user.id, &mut tree).await?;
    let message = required_string(args, "message")?;
    create_commit_for_tree(
        state,
        user,
        project_id,
        &branch,
        &tree,
        base.clone().into_iter().collect(),
        &message,
        base.as_deref(),
        Some(revision),
    )
    .await
}

fn metadata_lookup(tree: &TreeDocument) -> BTreeMap<String, &MetadataRow> {
    tree.metadata
        .iter()
        .map(|metadata| (metadata.dataset_key.clone(), metadata))
        .collect()
}

fn entry_lookup(tree: &TreeDocument) -> BTreeMap<(String, String), &WorkEntry> {
    tree.entries
        .iter()
        .map(|entry| ((entry.dataset_key.clone(), entry.image_id.clone()), entry))
        .collect()
}

fn entry_value(entry: &WorkEntry) -> Value {
    json!({
        "datasetKey": entry.dataset_key,
        "datasetId": entry.dataset_id,
        "imageId": entry.image_id,
        "fileName": entry.file_name,
        "imageHash": entry.image_hash,
        "imageByteSize": entry.image_byte_size,
        "imageMimeType": entry.image_mime_type,
        "annotationHash": entry.annotation_hash,
        "annotation": entry.annotation,
        "status": entry.status,
        "qaStatus": entry.qa_status,
        "split": entry.split,
        "width": entry.width,
        "height": entry.height,
        "reviewNote": entry.review_note,
        "tags": entry.tags,
        "sourceItemId": entry.source_item_id
    })
}

fn value_or_null<T: Serialize>(value: Option<&T>) -> Value {
    value
        .map(|value| serde_json::to_value(value).unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn merge_choice(base: &Value, ours: &Value, theirs: &Value) -> (Value, bool) {
    if ours == theirs {
        (ours.clone(), false)
    } else if base == ours {
        (theirs.clone(), false)
    } else if base == theirs {
        (ours.clone(), false)
    } else {
        (ours.clone(), true)
    }
}

fn describe_conflict_value(value: &Value) -> Option<String> {
    if value.is_null() {
        return Some("删除/不存在".to_string());
    }
    if let Some(value) = value.as_str() {
        return Some(value.to_string());
    }
    if let Some(value) = value.as_bool() {
        return Some(
            if value {
                "属于分组"
            } else {
                "不属于分组"
            }
            .to_string(),
        );
    }
    let object = value.as_object()?;
    if let Some(name) = object.get("name").and_then(Value::as_str) {
        let count = object
            .get("datasetKeys")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        return Some(format!("{name} · {count} 个数据集"));
    }
    if let Some(file_name) = object.get("fileName").and_then(Value::as_str) {
        let mut result = file_name.to_string();
        if let Some(hash) = object.get("annotationHash").and_then(Value::as_str) {
            result.push_str(&format!(" · 标签 {}", &hash[..hash.len().min(10)]));
        }
        if let Some(status) = object.get("status").and_then(Value::as_str) {
            result.push_str(&format!(" · {status}"));
        }
        return Some(result);
    }
    if object.contains_key("classes") {
        return Some("类别定义".to_string());
    }
    Some("元数据或归属".to_string())
}

fn conflict_to_value(conflict: &WebConflict) -> Value {
    serde_json::to_value(conflict).unwrap_or_else(|_| {
        json!({
            "key": conflict.key,
            "kind": conflict.kind,
            "base": conflict.base,
            "ours": conflict.ours,
            "theirs": conflict.theirs
        })
    })
}

fn conflict_from_value(value: &Value) -> Result<WebConflict, ApiError> {
    serde_json::from_value(value.clone())
        .map_err(|error| ApiError::internal_message(format!("invalid merge conflict: {error}")))
}

fn entry_conflict_key(dataset_key: &str, image_id: &str) -> String {
    // PostgreSQL JSONB rejects U+0000 even when serde would otherwise escape
    // it.  Hex-encode both variable strings so the conflict key remains
    // deterministic and unambiguous for arbitrary UTF-8/path-like IDs.
    format!(
        "entry:v1:{}:{}",
        hex::encode(dataset_key.as_bytes()),
        hex::encode(image_id.as_bytes())
    )
}

fn decode_entry_conflict_key(key: &str) -> Result<(String, String), ApiError> {
    let encoded = key
        .strip_prefix("entry:v1:")
        .ok_or_else(|| ApiError::bad_request("invalid entry conflict key"))?;
    let (dataset_hex, image_hex) = encoded
        .split_once(':')
        .ok_or_else(|| ApiError::bad_request("invalid entry conflict key"))?;
    let dataset = hex::decode(dataset_hex)
        .ok()
        .and_then(|value| String::from_utf8(value).ok())
        .ok_or_else(|| ApiError::bad_request("invalid entry conflict dataset key"))?;
    let image = hex::decode(image_hex)
        .ok()
        .and_then(|value| String::from_utf8(value).ok())
        .ok_or_else(|| ApiError::bad_request("invalid entry conflict image ID"))?;
    Ok((dataset, image))
}

fn group_conflict_key(group_id: &str, field: &str, dataset_key: Option<&str>) -> String {
    let mut key = format!("group:v1:{}:{}", hex::encode(group_id.as_bytes()), field);
    if let Some(dataset_key) = dataset_key {
        key.push(':');
        key.push_str(&hex::encode(dataset_key.as_bytes()));
    }
    key
}

fn decode_group_conflict_key(key: &str) -> Result<(String, String, Option<String>), ApiError> {
    let encoded = key
        .strip_prefix("group:v1:")
        .ok_or_else(|| ApiError::bad_request("invalid dataset-group conflict key"))?;
    let mut parts = encoded.split(':');
    let group_hex = parts
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid dataset-group conflict key"))?;
    let field = parts
        .next()
        .ok_or_else(|| ApiError::bad_request("invalid dataset-group conflict key"))?;
    let member_hex = parts.next();
    if parts.next().is_some() || (field != "whole" && field != "name" && field != "member") {
        return Err(ApiError::bad_request("invalid dataset-group conflict key"));
    }
    let group_id = hex::decode(group_hex)
        .ok()
        .and_then(|value| String::from_utf8(value).ok())
        .ok_or_else(|| ApiError::bad_request("invalid dataset-group conflict group ID"))?;
    let member = match member_hex {
        Some(value) if field == "member" => Some(
            hex::decode(value)
                .ok()
                .and_then(|value| String::from_utf8(value).ok())
                .ok_or_else(|| ApiError::bad_request("invalid dataset-group member key"))?,
        ),
        Some(_) => {
            return Err(ApiError::bad_request(
                "dataset-group conflict field does not accept a member",
            ))
        }
        None if field == "member" => {
            return Err(ApiError::bad_request(
                "dataset-group member conflict is missing its dataset key",
            ))
        }
        None => None,
    };
    Ok((group_id, field.to_string(), member))
}

fn group_value(group: Option<&DatasetGroup>) -> Value {
    group
        .map(|group| serde_json::to_value(group).unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn group_from_value(value: &Value) -> Result<DatasetGroup, ApiError> {
    serde_json::from_value(value.clone())
        .map_err(|error| ApiError::bad_request(format!("invalid dataset-group value: {error}")))
}

fn group_lookup(metadata: &ProjectMetadata) -> BTreeMap<String, DatasetGroup> {
    metadata
        .dataset_groups
        .iter()
        .cloned()
        .map(|group| (group.id.clone(), group))
        .collect()
}

fn group_order(metadata: &ProjectMetadata) -> Vec<String> {
    metadata
        .dataset_groups
        .iter()
        .map(|group| group.id.clone())
        .collect()
}

fn dataset_group_assignment(metadata: &ProjectMetadata, dataset_key: &str) -> Option<String> {
    metadata
        .dataset_groups
        .iter()
        .find(|group| group.dataset_keys.iter().any(|key| key == dataset_key))
        .map(|group| group.id.clone())
}

/// Validate all structural metadata constraints while allowing a provisional
/// duplicate membership during a merge session.  A cross-group assignment is
/// represented as an explicit conflict and is removed atomically by the
/// resolver; silently dropping one side here would lose the user's choice.
fn validate_merge_project_metadata(
    metadata: &ProjectMetadata,
    available_dataset_keys: &BTreeSet<String>,
) -> Result<(), ApiError> {
    let mut validation = metadata.clone();
    let mut seen = BTreeSet::new();
    for group in &mut validation.dataset_groups {
        group.dataset_keys.retain(|key| seen.insert(key.clone()));
    }
    data_project_metadata::validate(&validation, Some(available_dataset_keys))
        .map_err(ApiError::bad_request)
}

fn merge_project_metadata(
    base: &ProjectMetadata,
    ours: &ProjectMetadata,
    theirs: &ProjectMetadata,
    available_dataset_keys: &BTreeSet<String>,
) -> Result<(ProjectMetadata, Vec<WebConflict>), ApiError> {
    if ours == theirs {
        let mut metadata = ours.clone();
        metadata.canonicalize();
        data_project_metadata::validate(&metadata, Some(available_dataset_keys))
            .map_err(ApiError::bad_request)?;
        return Ok((metadata, Vec::new()));
    }
    let base_groups = group_lookup(base);
    let ours_groups = group_lookup(ours);
    let theirs_groups = group_lookup(theirs);
    let group_ids = base_groups
        .keys()
        .chain(ours_groups.keys())
        .chain(theirs_groups.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut merged_groups = BTreeMap::<String, DatasetGroup>::new();
    let mut conflicts = Vec::new();
    for group_id in group_ids {
        let base_group = base_groups.get(&group_id);
        let ours_group = ours_groups.get(&group_id);
        let theirs_group = theirs_groups.get(&group_id);
        let base_value = group_value(base_group);
        let ours_value = group_value(ours_group);
        let theirs_value = group_value(theirs_group);

        // Divergent deletion versus modification is a real conflict.  Keep
        // the target (ours) in the candidate until the user explicitly picks
        // a side, matching the image/annotation conflict semantics.
        let presence_changed_on_ours = ours_value != base_value;
        let presence_changed_on_theirs = theirs_value != base_value;
        if (ours_group.is_none() || theirs_group.is_none())
            && presence_changed_on_ours
            && presence_changed_on_theirs
            && ours_value != theirs_value
        {
            conflicts.push(WebConflict {
                key: group_conflict_key(&group_id, "whole", None),
                kind: "dataset-group".to_string(),
                display_name: Some(group_id.clone()),
                base_description: describe_conflict_value(&base_value),
                ours_description: describe_conflict_value(&ours_value),
                theirs_description: describe_conflict_value(&theirs_value),
                base: (!base_value.is_null()).then_some(base_value),
                ours: (!ours_value.is_null()).then_some(ours_value),
                theirs: (!theirs_value.is_null()).then_some(theirs_value),
            });
            if let Some(group) = ours_group {
                merged_groups.insert(group_id, group.clone());
            }
            continue;
        }
        let (chosen_value, value_conflict) = merge_choice(&base_value, &ours_value, &theirs_value);
        if !value_conflict {
            if chosen_value.is_null() {
                continue;
            }
            let selected_group = if ours_value == chosen_value {
                ours_group
            } else if theirs_value == chosen_value {
                theirs_group
            } else {
                base_group
            }
            .ok_or_else(|| {
                ApiError::internal_message("missing selected dataset group during merge")
            })?;
            merged_groups.insert(group_id, selected_group.clone());
            continue;
        }
        // Both branches have a group with this ID but changed its fields.
        // Merge the editable fields independently so a name/member conflict
        // remains explainable and resolvable instead of being flattened.
        let ours_group = ours_group
            .ok_or_else(|| ApiError::internal_message("missing ours dataset group during merge"))?;
        let theirs_group = theirs_group.ok_or_else(|| {
            ApiError::internal_message("missing theirs dataset group during merge")
        })?;
        let mut merged = ours_group.clone();
        let base_name = base_group
            .map(|group| Value::String(group.name.clone()))
            .unwrap_or(Value::Null);
        let ours_name = Value::String(ours_group.name.clone());
        let theirs_name = Value::String(theirs_group.name.clone());
        let (name, name_conflict) = merge_choice(&base_name, &ours_name, &theirs_name);
        if name_conflict {
            conflicts.push(WebConflict {
                key: group_conflict_key(&group_id, "name", None),
                kind: "dataset-group-name".to_string(),
                display_name: Some(format!("{} 名称", group_id)),
                base_description: describe_conflict_value(&base_name),
                ours_description: describe_conflict_value(&ours_name),
                theirs_description: describe_conflict_value(&theirs_name),
                base: (!base_name.is_null()).then_some(base_name),
                ours: Some(ours_name),
                theirs: Some(theirs_name),
            });
        }
        merged.name = name.as_str().unwrap_or(&ours_group.name).to_string();

        let base_members = base_group
            .map(|group| group.dataset_keys.iter().cloned().collect::<BTreeSet<_>>())
            .unwrap_or_default();
        let ours_members = ours_group
            .dataset_keys
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let theirs_members = theirs_group
            .dataset_keys
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let member_keys = base_members
            .iter()
            .chain(ours_members.iter())
            .chain(theirs_members.iter())
            .cloned()
            .collect::<BTreeSet<_>>();
        merged.dataset_keys.clear();
        let mut selected_members = BTreeSet::new();
        for dataset_key in member_keys {
            let base_value = Value::Bool(base_members.contains(&dataset_key));
            let ours_value = Value::Bool(ours_members.contains(&dataset_key));
            let theirs_value = Value::Bool(theirs_members.contains(&dataset_key));
            let (selected, member_conflict) = merge_choice(&base_value, &ours_value, &theirs_value);
            if member_conflict {
                conflicts.push(WebConflict {
                    key: group_conflict_key(&group_id, "member", Some(&dataset_key)),
                    kind: "dataset-group-membership".to_string(),
                    display_name: Some(format!("{} · {}", merged.name, dataset_key)),
                    base_description: Some(
                        if base_value == Value::Bool(true) {
                            "属于分组"
                        } else {
                            "不属于分组"
                        }
                        .to_string(),
                    ),
                    ours_description: Some(
                        if ours_value == Value::Bool(true) {
                            "属于分组"
                        } else {
                            "不属于分组"
                        }
                        .to_string(),
                    ),
                    theirs_description: Some(
                        if theirs_value == Value::Bool(true) {
                            "属于分组"
                        } else {
                            "不属于分组"
                        }
                        .to_string(),
                    ),
                    base: Some(base_value),
                    ours: Some(ours_value),
                    theirs: Some(theirs_value),
                });
            }
            if selected.as_bool().unwrap_or(false) {
                selected_members.insert(dataset_key);
            }
        }
        // Dataset order follows the target branch first, then newly
        // introduced members from the source branch.  This keeps a merge
        // predictable and avoids alphabetically shuffling card columns.
        for dataset_key in ours_group
            .dataset_keys
            .iter()
            .chain(theirs_group.dataset_keys.iter())
        {
            if selected_members.remove(dataset_key) {
                merged.dataset_keys.push(dataset_key.clone());
            }
        }
        merged.dataset_keys.extend(selected_members);
        merged_groups.insert(group_id, merged);
    }

    // Keep target order, append source-only groups, then preserve any base
    // group that survived.  Positions are normalized after this ordering so
    // a merge does not accidentally reorder cards because both branches used
    // the same position number for a newly-created group.
    let mut ordered_ids = Vec::new();
    for id in group_order(ours)
        .into_iter()
        .chain(group_order(theirs))
        .chain(group_order(base))
    {
        if merged_groups.contains_key(&id) && !ordered_ids.contains(&id) {
            ordered_ids.push(id);
        }
    }
    let mut dataset_groups = ordered_ids
        .into_iter()
        .filter_map(|id| merged_groups.remove(&id))
        .filter_map(|mut group| {
            group
                .dataset_keys
                .retain(|key| available_dataset_keys.contains(key));
            (!group.dataset_keys.is_empty() || group.name.trim().is_empty() == false)
                .then_some(group)
        })
        .collect::<Vec<_>>();
    for (position, group) in dataset_groups.iter_mut().enumerate() {
        group.position = position as i32;
    }
    // The per-group field merge above is intentionally independent.  Moving
    // one dataset to different groups on both branches is the one
    // non-decomposable case: retaining both memberships would violate the
    // one-group invariant, while picking one would discard the other branch
    // before the user has resolved it.  Keep both in the provisional tree and
    // expose a regular member conflict; the resolver uses the three snapshots
    // to move the dataset atomically to the selected branch assignment.
    let mut assignment_dataset_keys = BTreeSet::new();
    for metadata in [base, ours, theirs] {
        for group in &metadata.dataset_groups {
            assignment_dataset_keys.extend(group.dataset_keys.iter().cloned());
        }
    }
    for dataset_key in assignment_dataset_keys {
        let base_assignment = dataset_group_assignment(base, &dataset_key);
        let ours_assignment = dataset_group_assignment(ours, &dataset_key);
        let theirs_assignment = dataset_group_assignment(theirs, &dataset_key);
        if ours_assignment == theirs_assignment
            || ours_assignment == base_assignment
            || theirs_assignment == base_assignment
        {
            continue;
        }
        let Some(conflict_group_id) = ours_assignment.clone().or(theirs_assignment.clone()) else {
            continue;
        };
        let key = group_conflict_key(&conflict_group_id, "member", Some(&dataset_key));
        if conflicts.iter().any(|conflict| conflict.key == key) {
            continue;
        }
        conflicts.push(WebConflict {
            key,
            kind: "dataset-group-membership".to_string(),
            display_name: Some(format!("分组归属 · {dataset_key}")),
            base_description: Some(match base_assignment.as_deref() {
                Some(group_id) => format!("属于分组 {group_id}"),
                None => "不属于任何分组".to_string(),
            }),
            ours_description: Some(match ours_assignment.as_deref() {
                Some(group_id) => format!("属于分组 {group_id}"),
                None => "不属于任何分组".to_string(),
            }),
            theirs_description: Some(match theirs_assignment.as_deref() {
                Some(group_id) => format!("属于分组 {group_id}"),
                None => "不属于任何分组".to_string(),
            }),
            base: Some(Value::Bool(
                base_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
            ours: Some(Value::Bool(
                ours_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
            theirs: Some(Value::Bool(
                theirs_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
        });
    }
    let metadata = ProjectMetadata {
        schema_version: data_project_metadata::PROJECT_METADATA_SCHEMA_VERSION,
        dataset_groups,
    };
    validate_merge_project_metadata(&metadata, available_dataset_keys)?;
    Ok((metadata, conflicts))
}

fn merge_trees(
    base: &TreeDocument,
    ours: &TreeDocument,
    theirs: &TreeDocument,
) -> Result<(TreeDocument, Vec<WebConflict>), ApiError> {
    let base_metadata = metadata_lookup(base);
    let ours_metadata = metadata_lookup(ours);
    let theirs_metadata = metadata_lookup(theirs);
    let metadata_keys = base_metadata
        .keys()
        .chain(ours_metadata.keys())
        .chain(theirs_metadata.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut metadata = Vec::new();
    let mut conflicts = Vec::new();
    for key in metadata_keys {
        let base_value = base_metadata
            .get(&key)
            .map(|item| item.metadata.clone())
            .unwrap_or(Value::Null);
        let ours_value = ours_metadata
            .get(&key)
            .map(|item| item.metadata.clone())
            .unwrap_or(Value::Null);
        let theirs_value = theirs_metadata
            .get(&key)
            .map(|item| item.metadata.clone())
            .unwrap_or(Value::Null);
        let (chosen, conflict) = merge_choice(&base_value, &ours_value, &theirs_value);
        if conflict {
            conflicts.push(WebConflict {
                key: format!("metadata:{key}"),
                kind: "metadata".to_string(),
                display_name: Some(format!("{key} 类别/元数据")),
                base_description: describe_conflict_value(&base_value),
                ours_description: describe_conflict_value(&ours_value),
                theirs_description: describe_conflict_value(&theirs_value),
                base: (!base_value.is_null()).then_some(base_value.clone()),
                ours: (!ours_value.is_null()).then_some(ours_value.clone()),
                theirs: (!theirs_value.is_null()).then_some(theirs_value.clone()),
            });
        }
        if chosen.is_null() {
            continue;
        }
        let source = ours_metadata
            .get(&key)
            .or_else(|| theirs_metadata.get(&key))
            .or_else(|| base_metadata.get(&key));
        metadata.push(MetadataRow {
            dataset_key: key,
            dataset_id: source.and_then(|row| row.dataset_id),
            metadata_hash: Some(json_hash(&chosen)?),
            metadata: chosen,
        });
    }
    let base_entries = entry_lookup(base);
    let ours_entries = entry_lookup(ours);
    let theirs_entries = entry_lookup(theirs);
    let entry_keys = base_entries
        .keys()
        .chain(ours_entries.keys())
        .chain(theirs_entries.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut entries = Vec::new();
    for (dataset_key, image_id) in entry_keys {
        let base_value = base_entries
            .get(&(dataset_key.clone(), image_id.clone()))
            .map(|entry| entry_value(entry))
            .unwrap_or(Value::Null);
        let ours_value = ours_entries
            .get(&(dataset_key.clone(), image_id.clone()))
            .map(|entry| entry_value(entry))
            .unwrap_or(Value::Null);
        let theirs_value = theirs_entries
            .get(&(dataset_key.clone(), image_id.clone()))
            .map(|entry| entry_value(entry))
            .unwrap_or(Value::Null);
        let (chosen, conflict) = merge_choice(&base_value, &ours_value, &theirs_value);
        if conflict {
            let file_name = chosen.get("fileName").and_then(Value::as_str);
            conflicts.push(WebConflict {
                key: entry_conflict_key(&dataset_key, &image_id),
                kind: "entry".to_string(),
                display_name: Some(file_name.unwrap_or(&image_id).to_string()),
                base_description: describe_conflict_value(&base_value),
                ours_description: describe_conflict_value(&ours_value),
                theirs_description: describe_conflict_value(&theirs_value),
                base: (!base_value.is_null()).then_some(base_value.clone()),
                ours: (!ours_value.is_null()).then_some(ours_value.clone()),
                theirs: (!theirs_value.is_null()).then_some(theirs_value.clone()),
            });
        }
        if chosen.is_null() {
            continue;
        }
        let tree = tree_document_from_json(&json!({
            "datasets": [{ "datasetKey": dataset_key }],
            "entries": [chosen]
        }))?;
        if let Some(entry) = tree.entries.into_iter().next() {
            entries.push(entry);
        }
    }
    let available_dataset_keys = metadata
        .iter()
        .map(|item| item.dataset_key.clone())
        .chain(entries.iter().map(|entry| entry.dataset_key.clone()))
        .collect::<BTreeSet<_>>();
    let (merged_project_metadata, group_conflicts) = merge_project_metadata(
        &base.project_metadata,
        &ours.project_metadata,
        &theirs.project_metadata,
        &available_dataset_keys,
    )?;
    conflicts.extend(group_conflicts);
    Ok((
        TreeDocument {
            project_metadata: merged_project_metadata,
            metadata,
            entries,
        },
        conflicts,
    ))
}

fn set_tree_conflict_value(
    tree: &mut TreeDocument,
    conflict: &WebConflict,
    resolution: &str,
) -> Result<(), ApiError> {
    set_tree_conflict_value_with_context(tree, conflict, resolution, None, None, None)
}

fn set_tree_conflict_value_with_context(
    tree: &mut TreeDocument,
    conflict: &WebConflict,
    resolution: &str,
    base_metadata: Option<&ProjectMetadata>,
    ours_metadata: Option<&ProjectMetadata>,
    theirs_metadata: Option<&ProjectMetadata>,
) -> Result<(), ApiError> {
    let value = match resolution {
        "base" => conflict.base.clone(),
        "ours" => conflict.ours.clone(),
        "theirs" => conflict.theirs.clone(),
        _ => {
            return Err(ApiError::bad_request(
                "merge resolution must be base, ours, or theirs",
            ))
        }
    };
    if conflict.kind.starts_with("dataset-group") {
        let (group_id, field, member_key) = decode_group_conflict_key(&conflict.key)?;
        if field == "whole" {
            tree.project_metadata
                .dataset_groups
                .retain(|group| group.id != group_id);
            if let Some(value) = value {
                let group = group_from_value(&value)?;
                if group.id != group_id {
                    return Err(ApiError::bad_request(
                        "dataset-group conflict value has a different group ID",
                    ));
                }
                tree.project_metadata.dataset_groups.push(group);
            }
            tree.project_metadata.canonicalize();
            let dataset_keys = tree_dataset_keys(tree);
            data_project_metadata::validate(&tree.project_metadata, Some(&dataset_keys))
                .map_err(ApiError::bad_request)?;
            return Ok(());
        }
        let groups = &mut tree.project_metadata.dataset_groups;
        let group_index = groups
            .iter()
            .position(|group| group.id == group_id)
            .ok_or_else(|| {
                ApiError::conflict(
                    "MERGE_GROUP_MISSING",
                    "dataset group no longer exists in the merge candidate",
                    json!({ "groupId": group_id }),
                )
            })?;
        match field.as_str() {
            "name" => {
                let Some(value) = value else {
                    return Err(ApiError::bad_request(
                        "dataset-group name resolution must contain a string",
                    ));
                };
                if value.is_null() {
                    // A group added independently on both branches has no
                    // base name.  Choosing `base` means removing that group.
                    groups.remove(group_index);
                } else if let Some(name) = value.as_str() {
                    groups[group_index].name = name.to_string();
                } else {
                    return Err(ApiError::bad_request(
                        "dataset-group name resolution must contain a string",
                    ));
                }
            }
            "member" => {
                let dataset_key = member_key.ok_or_else(|| {
                    ApiError::bad_request(
                        "dataset-group member conflict is missing its dataset key",
                    )
                })?;
                if let (Some(base_metadata), Some(ours_metadata), Some(theirs_metadata)) =
                    (base_metadata, ours_metadata, theirs_metadata)
                {
                    let base_assignment = dataset_group_assignment(base_metadata, &dataset_key);
                    let ours_assignment = dataset_group_assignment(ours_metadata, &dataset_key);
                    let theirs_assignment = dataset_group_assignment(theirs_metadata, &dataset_key);
                    let assignment_conflict = ours_assignment != theirs_assignment
                        && ours_assignment != base_assignment
                        && theirs_assignment != base_assignment;
                    if assignment_conflict {
                        let selected_assignment = match resolution {
                            "base" => base_assignment,
                            "ours" => ours_assignment,
                            "theirs" => theirs_assignment,
                            _ => unreachable!("resolution was validated above"),
                        };
                        for group in &mut tree.project_metadata.dataset_groups {
                            group.dataset_keys.retain(|key| key != &dataset_key);
                        }
                        if let Some(selected_group_id) = selected_assignment {
                            let group = tree
                                .project_metadata
                                .dataset_groups
                                .iter_mut()
                                .find(|group| group.id == selected_group_id)
                                .ok_or_else(|| {
                                    ApiError::conflict(
                                        "MERGE_GROUP_MISSING",
                                        "dataset group is missing from the merge candidate",
                                        json!({ "groupId": selected_group_id }),
                                    )
                                })?;
                            group.dataset_keys.push(dataset_key);
                        }
                        tree.project_metadata.canonicalize();
                        let dataset_keys = tree_dataset_keys(tree);
                        data_project_metadata::validate(
                            &tree.project_metadata,
                            Some(&dataset_keys),
                        )
                        .map_err(ApiError::bad_request)?;
                        return Ok(());
                    }
                }
                if value.and_then(|value| value.as_bool()).unwrap_or(false) {
                    if !groups[group_index]
                        .dataset_keys
                        .iter()
                        .any(|key| key == &dataset_key)
                    {
                        groups[group_index].dataset_keys.push(dataset_key);
                    }
                } else {
                    groups[group_index]
                        .dataset_keys
                        .retain(|key| key != &dataset_key);
                }
            }
            _ => {
                return Err(ApiError::bad_request(
                    "unsupported dataset-group conflict field",
                ))
            }
        }
        tree.project_metadata.canonicalize();
        let dataset_keys = tree_dataset_keys(tree);
        data_project_metadata::validate(&tree.project_metadata, Some(&dataset_keys))
            .map_err(ApiError::bad_request)?;
        return Ok(());
    }
    if conflict.kind == "metadata" {
        let key = conflict
            .key
            .strip_prefix("metadata:")
            .unwrap_or(&conflict.key);
        tree.metadata.retain(|metadata| metadata.dataset_key != key);
        if let Some(value) = value {
            tree.metadata.push(MetadataRow {
                dataset_key: key.to_string(),
                dataset_id: None,
                metadata_hash: Some(json_hash(&value)?),
                metadata: value,
            });
            tree.metadata
                .sort_by(|left, right| left.dataset_key.cmp(&right.dataset_key));
        }
        return Ok(());
    }
    let (dataset_key, image_id) = decode_entry_conflict_key(&conflict.key)?;
    tree.entries
        .retain(|entry| !(entry.dataset_key == dataset_key && entry.image_id == image_id));
    if let Some(value) = value {
        let entry_tree = tree_document_from_json(&json!({
            "datasets": [{ "datasetKey": dataset_key }],
            "entries": [value]
        }))?;
        if let Some(entry) = entry_tree.entries.into_iter().next() {
            tree.entries.push(entry);
        }
    }
    Ok(())
}

async fn common_ancestor(
    state: &AppState,
    project_id: Uuid,
    ours: Option<&str>,
    theirs: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let graph = commit_graph(state, project_id).await?;
    let ours_ancestors = ancestor_set(&graph, ours);
    if let Some(theirs) = theirs {
        let mut queue = vec![theirs.to_string()];
        let mut seen = HashSet::new();
        while let Some(id) = queue.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if ours_ancestors.contains(&id) {
                return Ok(Some(id));
            }
            if let Some(parents) = graph.get(&id) {
                queue.extend(parents.iter().cloned());
            }
        }
    }
    Ok(None)
}

async fn preview_merge(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebMergeResult, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let source = validate_branch(
        &merge_branch_value(args, "sourceBranch", "theirsBranch")?
            .ok_or_else(|| ApiError::bad_request("sourceBranch is required"))?,
    )?;
    let target = validate_branch(
        &merge_branch_value(args, "targetBranch", "oursBranch")?
            .unwrap_or(active_branch(state, project_id, user.id).await?),
    )?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    ensure_worktree_initialized(state, project_id, user.id, &source).await?;
    ensure_worktree_initialized(state, project_id, user.id, &target).await?;
    let target_tree = working_tree(state, user, project_id, Some(target.clone())).await?;
    if target_tree.dirty {
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit target working-tree changes before merging",
            json!({ "branch": target }),
        ));
    }
    let ours_head = load_ref_head(state, project_id, &target).await?;
    let theirs_head = load_ref_head(state, project_id, &source).await?;
    let base_head = common_ancestor(
        state,
        project_id,
        ours_head.as_deref(),
        theirs_head.as_deref(),
    )
    .await?;
    let base = load_commit_tree(state, project_id, base_head.as_deref()).await?;
    let ours = load_commit_tree(state, project_id, ours_head.as_deref()).await?;
    let theirs = load_commit_tree(state, project_id, theirs_head.as_deref()).await?;
    let (_, conflicts) = merge_trees(&base, &ours, &theirs)?;
    Ok(WebMergeResult {
        session_id: None,
        commit: None,
        conflicts,
    })
}

async fn merge_branches(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebMergeResult, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let source = validate_branch(
        &merge_branch_value(args, "sourceBranch", "theirsBranch")?
            .ok_or_else(|| ApiError::bad_request("sourceBranch is required"))?,
    )?;
    let target = validate_branch(
        &merge_branch_value(args, "targetBranch", "oursBranch")?
            .ok_or_else(|| ApiError::bad_request("targetBranch is required"))?,
    )?;
    if source == target {
        return Err(ApiError::bad_request(
            "source and target branches must differ",
        ));
    }
    let message = required_string(args, "message")?;
    ensure_worktree_initialized(state, project_id, user.id, &source).await?;
    ensure_worktree_initialized(state, project_id, user.id, &target).await?;
    let target_tree = working_tree(state, user, project_id, Some(target.clone())).await?;
    if target_tree.dirty {
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit target working-tree changes before merging",
            json!({ "branch": target }),
        ));
    }
    let ours_head = load_ref_head(state, project_id, &target).await?;
    let theirs_head = load_ref_head(state, project_id, &source).await?;
    let base_head = common_ancestor(
        state,
        project_id,
        ours_head.as_deref(),
        theirs_head.as_deref(),
    )
    .await?;
    let base = load_commit_tree(state, project_id, base_head.as_deref()).await?;
    let ours = load_commit_tree(state, project_id, ours_head.as_deref()).await?;
    let theirs = load_commit_tree(state, project_id, theirs_head.as_deref()).await?;
    let (mut merged, conflicts) = merge_trees(&base, &ours, &theirs)?;
    if !conflicts.is_empty() {
        let ref_revision: i64 = sqlx::query_scalar(
            "SELECT revision FROM data_project_refs WHERE project_id = $1 AND ref_name = $2",
        )
        .bind(project_id)
        .bind(&target)
        .fetch_one(&state.pool)
        .await?;
        let session_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO data_project_merge_sessions (id, project_id, target_branch, source_branch, base_commit_id, ours_commit_id, theirs_commit_id, expected_target_revision, message, candidate_tree_json, conflicts_json, resolutions_json, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, '{}', $12)",
        )
        .bind(session_id)
        .bind(project_id)
        .bind(&target)
        .bind(&source)
        .bind(base_head)
        .bind(&ours_head)
        .bind(&theirs_head)
        .bind(ref_revision)
        .bind(&message)
        .bind(tree_document_json(&merged))
        .bind(serde_json::to_value(&conflicts).unwrap_or_else(|_| json!([])))
        .bind(user.id)
        .execute(&state.pool)
        .await?;
        return Ok(WebMergeResult {
            session_id: Some(session_id),
            commit: None,
            conflicts,
        });
    }
    materialize_tree_objects(state, project_id, user.id, &mut merged).await?;
    let parents = ours_head.into_iter().chain(theirs_head).collect::<Vec<_>>();
    let commit = create_commit_for_tree(
        state,
        user,
        project_id,
        &target,
        &merged,
        parents,
        &message,
        target_tree.head_commit_id.as_deref(),
        Some(target_tree.revision),
    )
    .await?;
    Ok(WebMergeResult {
        session_id: None,
        commit: Some(commit),
        conflicts: Vec::new(),
    })
}

async fn clear_merge_resolution_lock(
    state: &AppState,
    session_id: Uuid,
    resolving: &Value,
) -> Result<(), ApiError> {
    let mut restored = resolving.clone();
    clear_merge_resolving_fields(&mut restored)?;
    sqlx::query(
        "UPDATE data_project_merge_sessions SET resolutions_json = $2, updated_at = now() WHERE id = $1 AND status = 'open' AND resolutions_json = $3",
    )
    .bind(session_id)
    .bind(restored)
    .bind(resolving)
    .execute(&state.pool)
    .await?;
    Ok(())
}

fn clear_merge_resolving_fields(value: &mut Value) -> Result<(), ApiError> {
    value
        .as_object_mut()
        .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?
        .retain(|key, _| key != "__resolving" && key != "__resolvingAt");
    Ok(())
}

fn merge_reservation_is_stale(value: &Value, now: DateTime<Utc>) -> bool {
    let Some(timestamp) = value.get("__resolvingAt").and_then(Value::as_str) else {
        // A boolean-only marker can only have come from a pre-TTL server or
        // a crashed request.  Reclaiming it is safe because the eventual
        // commit still uses the target ref/worktree CAS.
        return true;
    };
    let Ok(timestamp) = DateTime::parse_from_rfc3339(timestamp) else {
        return true;
    };
    now.signed_duration_since(timestamp.with_timezone(&Utc))
        >= Duration::seconds(MERGE_RESERVATION_TTL_SECONDS)
}

async fn resolve_merge(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebMergeResult, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let session_id = required_uuid(args, "sessionId")?;
    let conflict_key = required_string_alias(args, "conflictKey", "key")?;
    let resolution = required_string(args, "resolution")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT target_branch, source_branch, base_commit_id, ours_commit_id, theirs_commit_id, expected_target_revision, message, candidate_tree_json, conflicts_json, resolutions_json, status, expires_at FROM data_project_merge_sessions WHERE id = $1 AND project_id = $2 FOR UPDATE",
    )
    .bind(session_id)
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("merge session not found"))?;
    if row.get::<String, _>("status") != "open" {
        return Err(ApiError::conflict(
            "MERGE_SESSION_CLOSED",
            "merge session is no longer open",
            json!({ "sessionId": session_id }),
        ));
    }
    if row.get::<DateTime<Utc>, _>("expires_at") <= Utc::now() {
        return Err(ApiError::conflict(
            "MERGE_SESSION_EXPIRED",
            "merge session has expired",
            json!({ "sessionId": session_id }),
        ));
    }
    let conflicts = row
        .get::<Value, _>("conflicts_json")
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(conflict_from_value)
        .collect::<Result<Vec<_>, _>>()?;
    let conflict = conflicts
        .iter()
        .find(|conflict| conflict.key == conflict_key)
        .ok_or_else(|| ApiError::not_found("merge conflict not found"))?;
    let mut candidate =
        tree_document_from_merge_candidate(&row.get::<Value, _>("candidate_tree_json"))?;
    let base_head = row.get::<Option<String>, _>("base_commit_id");
    let ours_head = row.get::<Option<String>, _>("ours_commit_id");
    let theirs_head = row.get::<Option<String>, _>("theirs_commit_id");
    let base_tree = load_commit_tree_tx(&mut tx, project_id, base_head.as_deref()).await?;
    let ours_tree = load_commit_tree_tx(&mut tx, project_id, ours_head.as_deref()).await?;
    let theirs_tree = load_commit_tree_tx(&mut tx, project_id, theirs_head.as_deref()).await?;
    set_tree_conflict_value_with_context(
        &mut candidate,
        conflict,
        &resolution,
        Some(&base_tree.project_metadata),
        Some(&ours_tree.project_metadata),
        Some(&theirs_tree.project_metadata),
    )?;
    let mut resolutions = row.get::<Value, _>("resolutions_json");
    let has_resolving_marker = resolutions.get("__resolving").is_some();
    let reservation_stale =
        has_resolving_marker && merge_reservation_is_stale(&resolutions, Utc::now());
    {
        let resolutions_object = resolutions
            .as_object()
            .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?;
        if resolutions_object.contains_key("__resolving") {
            if !reservation_stale {
                return Err(ApiError::conflict(
                    "MERGE_SESSION_BUSY",
                    "merge session is being resolved by another request",
                    json!({ "sessionId": session_id }),
                ));
            }
        }
    }
    if reservation_stale {
        let previous_resolutions = resolutions.clone();
        clear_merge_resolving_fields(&mut resolutions)?;
        let recovered = sqlx::query(
            "UPDATE data_project_merge_sessions SET resolutions_json = $2, updated_at = now() WHERE id = $1 AND status = 'open' AND resolutions_json = $3",
        )
        .bind(session_id)
        .bind(&resolutions)
        .bind(previous_resolutions)
        .execute(&mut *tx)
        .await?;
        if recovered.rows_affected() != 1 {
            return Err(ApiError::conflict(
                "MERGE_SESSION_MOVED",
                "merge session changed while recovering its reservation",
                json!({ "sessionId": session_id }),
            ));
        }
    }
    {
        let resolutions_object = resolutions
            .as_object()
            .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?;
        if resolutions_object.contains_key(&conflict_key) {
            return Err(ApiError::conflict(
                "MERGE_CONFLICT_ALREADY_RESOLVED",
                "merge conflict has already been resolved",
                json!({ "conflictKey": conflict_key }),
            ));
        }
    }
    // This is the JSON value currently persisted by the session row.  Keep it
    // as the reservation CAS baseline before adding the resolution selected by
    // this request; the final-conflict path must compare against the database
    // row without that newly selected key.
    let reservation_cas_baseline = resolutions.clone();
    resolutions
        .as_object_mut()
        .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?
        .insert(conflict_key, Value::String(resolution));
    let unresolved = conflicts
        .iter()
        .filter(|conflict| resolutions.get(&conflict.key).is_none())
        .cloned()
        .collect::<Vec<_>>();
    if !unresolved.is_empty() {
        sqlx::query(
            "UPDATE data_project_merge_sessions SET candidate_tree_json = $2, resolutions_json = $3, updated_at = now() WHERE id = $1 AND status = 'open'",
        )
        .bind(session_id)
        .bind(tree_document_json(&candidate))
        .bind(resolutions)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        return Ok(WebMergeResult {
            session_id: Some(session_id),
            commit: None,
            conflicts: unresolved,
        });
    }
    let target = row.get::<String, _>("target_branch");
    let message = row.get::<String, _>("message");
    let expected_target_revision: i64 = row.get("expected_target_revision");
    let target_ref_revision: i64 = sqlx::query_scalar(
        "SELECT revision FROM data_project_refs WHERE project_id = $1 AND ref_name = $2 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&target)
    .fetch_one(&mut *tx)
    .await?;
    if target_ref_revision != expected_target_revision {
        return Err(ApiError::conflict(
            "MERGE_TARGET_MOVED",
            "merge target branch changed after the preview",
            json!({ "expectedRevision": expected_target_revision, "actualRevision": target_ref_revision }),
        ));
    }
    let previous_resolutions = reservation_cas_baseline;
    resolutions
        .as_object_mut()
        .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?
        .insert("__resolving".to_string(), Value::Bool(true));
    resolutions
        .as_object_mut()
        .ok_or_else(|| ApiError::internal_message("merge resolutions are not an object"))?
        .insert(
            "__resolvingAt".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
    let reserved = sqlx::query(
        "UPDATE data_project_merge_sessions SET resolutions_json = $2, updated_at = now() WHERE id = $1 AND status = 'open' AND resolutions_json = $3",
    )
    .bind(session_id)
    .bind(&resolutions)
    .bind(previous_resolutions)
    .execute(&mut *tx)
    .await?;
    if reserved.rows_affected() != 1 {
        return Err(ApiError::conflict(
            "MERGE_SESSION_MOVED",
            "merge session changed while resolving conflicts",
            json!({ "sessionId": session_id }),
        ));
    }
    tx.commit().await?;
    let tree_state = match working_tree(state, user, project_id, Some(target.clone())).await {
        Ok(tree_state) => tree_state,
        Err(error) => {
            let _ = clear_merge_resolution_lock(state, session_id, &resolutions).await;
            return Err(error);
        }
    };
    if tree_state.dirty {
        let _ = clear_merge_resolution_lock(state, session_id, &resolutions).await;
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit target working-tree changes before resolving merge",
            json!({ "branch": target }),
        ));
    }
    if let Err(error) = materialize_tree_objects(state, project_id, user.id, &mut candidate).await {
        let _ = clear_merge_resolution_lock(state, session_id, &resolutions).await;
        return Err(error);
    }
    let parents = ours_head
        .clone()
        .into_iter()
        .chain(theirs_head.clone())
        .collect::<Vec<_>>();
    let resolving_snapshot = resolutions.clone();
    let mut final_resolutions = resolutions;
    clear_merge_resolving_fields(&mut final_resolutions)?;
    let commit = match create_commit_for_tree_with_merge_finalize(
        state,
        user,
        project_id,
        &target,
        &candidate,
        parents,
        &message,
        ours_head.as_deref(),
        Some(tree_state.revision),
        MergeFinalizeContext {
            session_id,
            resolving_snapshot: resolving_snapshot.clone(),
            final_resolutions,
        },
    )
    .await
    {
        Ok(commit) => commit,
        Err(error) => {
            let _ = clear_merge_resolution_lock(state, session_id, &resolving_snapshot).await;
            return Err(error);
        }
    };
    Ok(WebMergeResult {
        session_id: None,
        commit: Some(commit),
        conflicts: Vec::new(),
    })
}

async fn restore_commit(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebCommit, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    let commit_id = required_string(args, "commitId")?;
    let message = required_string(args, "message")?;
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let current_tree = working_tree(state, user, project_id, Some(branch.clone())).await?;
    if current_tree.dirty {
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit or discard working-tree changes before restoring a snapshot",
            json!({ "branch": branch }),
        ));
    }
    let mut tree = load_commit_tree(state, project_id, Some(&commit_id)).await?;
    materialize_tree_objects(state, project_id, user.id, &mut tree).await?;
    create_commit_for_tree(
        state,
        user,
        project_id,
        &branch,
        &tree,
        current_tree.head_commit_id.clone().into_iter().collect(),
        &message,
        current_tree.head_commit_id.as_deref(),
        Some(current_tree.revision),
    )
    .await
}

async fn branch_for_dataset_args(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
    permission: WebPermission,
) -> Result<(Uuid, String, String, Option<Uuid>), ApiError> {
    let dataset_value = required_string(args, "datasetId")?;
    let (project_id, dataset_key, dataset_id) =
        resolve_dataset_identity(state, user.id, &dataset_value, permission).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    Ok((project_id, dataset_key, branch, dataset_id))
}

fn image_item_json(project_id: Uuid, entry: &WorkEntry, include_annotation: bool) -> Value {
    let dataset_id = browser_dataset_id(project_id, &entry.dataset_key);
    let mut item = json!({
        "id": entry.image_id,
        "imageId": entry.image_id,
        "datasetId": dataset_id,
        "fileName": entry.file_name,
        "width": entry.width,
        "height": entry.height,
        "split": entry.split,
        "status": entry.status,
        "qaStatus": entry.qa_status,
        "reviewNote": entry.review_note,
        "tags": entry.tags,
        "imageHash": entry.image_hash,
        "annotationHash": entry.annotation_hash,
        "imageUrl": format!(
            "/api/v1/web/projects/{project_id}/objects/{}",
            entry.image_hash
        ),
        "annotationUrl": format!(
            "/api/v1/web/projects/{project_id}/objects/{}",
            entry.annotation_hash
        ),
        "revision": entry.revision
    });
    if include_annotation {
        item["annotation"] = entry.annotation.clone();
    }
    item
}

async fn list_images(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::View).await?;
    let cursor = optional_string(args, "cursor")?;
    let limit = optional_i64(args, "limit")?
        .unwrap_or(100)
        .clamp(1, MAX_PAGE_SIZE);
    let offset = optional_i64(args, "offset")?.unwrap_or(0).max(0);
    let rows = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND ($4::TEXT IS NULL OR image_id > $4) ORDER BY image_id LIMIT $5 OFFSET $6",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(cursor.as_deref())
    .bind(limit + 1)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;
    let has_next = rows.len() as i64 > limit;
    let items = rows
        .iter()
        .take(limit as usize)
        .map(|row| image_item_json(project_id, &work_entry_from_row(row), false))
        .collect::<Vec<_>>();
    let next_cursor = has_next
        .then(|| {
            items
                .last()
                .and_then(|item| item.get("imageId"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .flatten();
    Ok(json!({
        "dataProjectId": project_id,
        "branch": branch,
        "datasetId": browser_dataset_id(project_id, &dataset_key),
        "items": items,
        "nextCursor": next_cursor
    }))
}

fn annotation_objects_for_class_samples(annotation: &Value) -> Vec<&Value> {
    annotation
        .as_array()
        .map(|items| items.iter().collect())
        .or_else(|| {
            annotation
                .get("objects")
                .or_else(|| annotation.get("annotations"))
                .and_then(Value::as_array)
                .map(|items| items.iter().collect())
        })
        .unwrap_or_default()
}

async fn list_class_samples(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::View).await?;
    let class_id = arg(args, "classId")
        .filter(|value| !value.is_null())
        .map(|value| {
            value
                .as_str()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| value.to_string())
        });
    let label = optional_string(args, "label")?;
    if class_id.is_none() && label.is_none() {
        return Err(ApiError::bad_request("classId or label is required"));
    }
    let offset = optional_i64(args, "offset")?.unwrap_or(0).max(0) as usize;
    let limit = optional_i64(args, "limit")?
        .unwrap_or(100)
        .clamp(1, MAX_PAGE_SIZE) as usize;
    let rows = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 ORDER BY image_id",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .fetch_all(&state.pool)
    .await?;
    let mut samples = Vec::new();
    for row in &rows {
        let entry = work_entry_from_row(row);
        let match_count = annotation_objects_for_class_samples(&entry.annotation)
            .iter()
            .filter(|object| {
                let matches_id = class_id.as_ref().is_none_or(|class_id| {
                    object
                        .get("classId")
                        .or_else(|| object.get("class_id"))
                        .map(|value| {
                            value
                                .as_str()
                                .map(ToOwned::to_owned)
                                .unwrap_or_else(|| value.to_string())
                                == *class_id
                        })
                        .unwrap_or(false)
                });
                let matches_label = label.as_ref().is_none_or(|label| {
                    object
                        .get("label")
                        .and_then(Value::as_str)
                        .is_some_and(|value| value == label)
                });
                matches_id && matches_label
            })
            .count();
        if match_count == 0 {
            continue;
        }
        samples.push(json!({
            "image": {
                "id": entry.image_id,
                "fileName": entry.file_name,
                "width": entry.width,
                "height": entry.height,
                "split": entry.split,
                "status": entry.status,
                "qaStatus": entry.qa_status,
                "reviewNote": entry.review_note,
                "tags": entry.tags
            },
            "matchCount": match_count
        }));
    }
    let samples = samples
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>();
    Ok(Value::Array(samples))
}

fn standalone_image_json(dataset_id: Uuid, row: &PgRow, include_annotation: bool) -> Value {
    let annotation = row.get::<Value, _>("annotation_json");
    let status = status_for_annotation(&annotation);
    let mut item = json!({
        "id": row.get::<Uuid, _>("id"),
        "imageId": row.get::<Uuid, _>("id"),
        "datasetId": dataset_id,
        "fileName": row.get::<String, _>("file_name"),
        "width": row.get::<i32, _>("width"),
        "height": row.get::<i32, _>("height"),
        "split": row.get::<String, _>("split"),
        "status": status,
        "qaStatus": "pending",
        "reviewNote": Value::Null,
        "tags": [],
        "imageHash": row.get::<String, _>("content_hash"),
        "annotationHash": row.get::<String, _>("annotation_hash"),
        "imageUrl": format!("/api/v1/web/datasets/{dataset_id}/images/{}/content", row.get::<Uuid, _>("id")),
        "revision": row.get::<i64, _>("revision")
    });
    if include_annotation {
        item["annotation"] = annotation;
    }
    item
}

async fn list_standalone_images(
    state: &AppState,
    user: &AuthUser,
    dataset_id: Uuid,
    args: &Value,
) -> Result<Value, ApiError> {
    require_standalone_dataset(state, user.id, dataset_id).await?;
    let offset = optional_i64(args, "offset")?.unwrap_or(0).max(0);
    let limit = optional_i64(args, "limit")?
        .unwrap_or(100)
        .clamp(1, MAX_PAGE_SIZE);
    let rows = sqlx::query(
        "SELECT di.id, di.file_name, di.split, di.annotation_json, di.annotation_hash, di.revision, db.content_hash, db.width, db.height FROM dataset_items di JOIN dataset_blobs db ON db.id = di.blob_id WHERE di.dataset_id = $1 ORDER BY di.created_at, di.id LIMIT $2 OFFSET $3",
    )
    .bind(dataset_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;
    Ok(Value::Array(
        rows.iter()
            .map(|row| standalone_image_json(dataset_id, row, false))
            .collect(),
    ))
}

async fn get_image(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let image_id = required_string(args, "imageId")?;
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::View).await?;
    let row = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    let mut entry = work_entry_from_row(&row);
    let raw_annotation = entry.annotation.clone();
    entry.annotation = hydrated_annotation_value(state, project_id, &raw_annotation).await?;
    Ok(image_item_json(project_id, &entry, true))
}

async fn hydrated_annotation_value(
    state: &AppState,
    project_id: Uuid,
    annotation: &Value,
) -> Result<Value, ApiError> {
    let Some(hash) = annotation
        .get("__webAnnotationHash")
        .and_then(Value::as_str)
        .filter(|hash| valid_hash(hash))
    else {
        return Ok(annotation.clone());
    };
    let row = sqlx::query(
        "SELECT object_key, byte_size FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 ORDER BY byte_size LIMIT 1",
    )
    .bind(project_id)
    .bind(hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::conflict("MISSING_OBJECT", "annotation object is missing", json!({ "hash": hash })))?;
    let byte_size: i64 = row.get("byte_size");
    if byte_size > 64 * 1024 * 1024 {
        return Err(ApiError::bad_request("annotation object is too large"));
    }
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(row.get::<String, _>("object_key"))
        .send()
        .await
        .map_err(|error| ApiError::not_found(format!("annotation object not found: {error}")))?;
    let mut bytes = Vec::with_capacity(byte_size.max(0) as usize);
    object
        .body
        .into_async_read()
        .take((64 * 1024 * 1024 + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ApiError::internal_message(format!("read annotation object: {error}")))?;
    if bytes.len() > 64 * 1024 * 1024 {
        return Err(ApiError::bad_request("annotation object is too large"));
    }
    let payload: Value = serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::internal_message(format!("decode annotation object: {error}"))
    })?;
    Ok(payload.get("objects").cloned().unwrap_or(payload))
}

async fn get_annotation(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let image_id = required_string(args, "imageId")?;
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::View).await?;
    let row = sqlx::query(
        "SELECT annotation_json, status, qa_status, revision, updated_at FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    let raw_annotation = row.get::<Value, _>("annotation_json");
    let annotation = hydrated_annotation_value(state, project_id, &raw_annotation).await?;
    Ok(json!({
        "imageId": image_id,
        "revision": row.get::<i64, _>("revision").to_string(),
        "objects": annotation,
        "status": row.get::<String, _>("status"),
        "qaStatus": row.get::<String, _>("qa_status"),
        "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
    }))
}

async fn save_annotation(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let image_id = required_string(args, "imageId")?;
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::Edit).await?;
    let annotation = optional_value(args, "annotation")
        .or_else(|| optional_value(args, "objects"))
        .ok_or_else(|| ApiError::bad_request("annotation/objects is required"))?;
    let base_revision = expected_revision(args)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT status, qa_status, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    let current_revision: i64 = row.get("revision");
    if base_revision.is_some_and(|revision| revision != current_revision) {
        return Err(ApiError::conflict(
            "IMAGE_REVISION_MISMATCH",
            "annotation was changed by another web client",
            json!({ "expectedRevision": base_revision, "actualRevision": current_revision }),
        ));
    }
    let status =
        optional_string(args, "status")?.unwrap_or_else(|| status_for_annotation(&annotation));
    let qa_status = optional_string(args, "qaStatus")?.unwrap_or_else(|| row.get("qa_status"));
    let hash = annotation_hash(&annotation, &status, &qa_status)?;
    tx.commit().await?;
    let payload = annotation_payload(&annotation, &status, &qa_status);
    let _ = store_json_object(
        state,
        project_id,
        user.id,
        &payload,
        "annotation",
        "application/json",
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    let latest_revision: i64 = row.get("revision");
    if latest_revision != current_revision {
        return Err(ApiError::conflict(
            "IMAGE_REVISION_MISMATCH",
            "annotation changed while its object was being stored",
            json!({ "expectedRevision": current_revision, "actualRevision": latest_revision }),
        ));
    }
    let next_revision = current_revision + 1;
    sqlx::query(
        "UPDATE data_project_worktree_entries SET annotation_hash = $5, annotation_json = $6, status = $7, qa_status = $8, revision = $9, updated_by = $10, updated_at = now() WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .bind(&hash)
    .bind(&annotation)
    .bind(&status)
    .bind(&qa_status)
    .bind(next_revision)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE data_project_worktrees SET revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND branch_name = $2",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(json!({
        "imageId": image_id,
        "revision": next_revision.to_string(),
        "objects": annotation,
        "status": status,
        "qaStatus": qa_status,
        "updatedAt": Utc::now(),
        "savedAt": Utc::now(),
        "auditEventId": Uuid::new_v4(),
        "sourceSync": "not-applicable",
        "sourceSyncMessage": Value::Null
    }))
}

async fn get_standalone_annotation(
    state: &AppState,
    user: &AuthUser,
    dataset_id: Uuid,
    image_id: &str,
) -> Result<Value, ApiError> {
    require_standalone_dataset(state, user.id, dataset_id).await?;
    let image_id = parse_uuid_string(image_id, "imageId")?;
    let row = sqlx::query(
        "SELECT annotation_json, annotation_hash, revision, updated_at FROM dataset_items WHERE dataset_id = $1 AND id = $2",
    )
    .bind(dataset_id)
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset image not found"))?;
    let objects = row.get::<Value, _>("annotation_json");
    Ok(json!({
        "imageId": image_id,
        "revision": row.get::<i64, _>("revision").to_string(),
        "objects": objects,
        "status": status_for_annotation(&objects),
        "qaStatus": "pending",
        "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
    }))
}

async fn save_standalone_annotation(
    state: &AppState,
    user: &AuthUser,
    dataset_id: Uuid,
    image_id: &str,
    args: &Value,
) -> Result<Value, ApiError> {
    require_standalone_dataset(state, user.id, dataset_id).await?;
    let image_id = parse_uuid_string(image_id, "imageId")?;
    let annotation = optional_value(args, "annotation")
        .or_else(|| optional_value(args, "objects"))
        .ok_or_else(|| ApiError::bad_request("annotation/objects is required"))?;
    let expected = expected_revision(args)?;
    let hash = json_hash(&annotation)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT revision FROM dataset_items WHERE dataset_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(dataset_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset image not found"))?;
    let current: i64 = row.get("revision");
    if expected.is_some_and(|expected| expected != current) {
        return Err(ApiError::conflict(
            "IMAGE_REVISION_MISMATCH",
            "annotation was changed by another web client",
            json!({ "expectedRevision": expected, "actualRevision": current }),
        ));
    }
    let next = current + 1;
    sqlx::query(
        "UPDATE dataset_items SET annotation_json = $3, annotation_hash = $4, revision = $5, updated_at = now() WHERE dataset_id = $1 AND id = $2",
    )
    .bind(dataset_id)
    .bind(image_id)
    .bind(&annotation)
    .bind(&hash)
    .bind(next)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE datasets SET revision = revision + 1, updated_at = now() WHERE id = $1")
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let now = Utc::now();
    Ok(json!({
        "imageId": image_id,
        "revision": next.to_string(),
        "objects": annotation,
        "status": status_for_annotation(&annotation),
        "qaStatus": "pending",
        "updatedAt": now,
        "savedAt": now,
        "auditEventId": Uuid::new_v4(),
        "sourceSync": "not-applicable",
        "sourceSyncMessage": Value::Null
    }))
}

async fn update_image_state(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let image_id = required_string(args, "imageId")?;
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::Edit).await?;
    let expected = expected_revision(args)?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT annotation_json, status, qa_status, split, review_note, tags_json, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    let revision: i64 = row.get("revision");
    if expected.is_some_and(|expected| expected != revision) {
        return Err(ApiError::conflict(
            "IMAGE_REVISION_MISMATCH",
            "image state changed by another web client",
            json!({ "expectedRevision": expected, "actualRevision": revision }),
        ));
    }
    let annotation = row.get::<Value, _>("annotation_json");
    let status = optional_string(args, "status")?.unwrap_or_else(|| row.get("status"));
    let qa_status = optional_string(args, "qaStatus")?.unwrap_or_else(|| row.get("qa_status"));
    let split = optional_string(args, "split")?.unwrap_or_else(|| row.get("split"));
    let review_note = optional_string(args, "reviewNote")?.or_else(|| row.get("review_note"));
    let tags = optional_value(args, "tags").unwrap_or_else(|| row.get("tags_json"));
    let next_revision = revision + 1;
    tx.commit().await?;
    let annotation = hydrated_annotation_value(state, project_id, &annotation).await?;
    let hash = annotation_hash(&annotation, &status, &qa_status)?;
    let payload = annotation_payload(&annotation, &status, &qa_status);
    let _ = store_json_object(
        state,
        project_id,
        user.id,
        &payload,
        "annotation",
        "application/json",
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let current: i64 = sqlx::query_scalar(
        "SELECT revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
    if current != revision {
        return Err(ApiError::conflict(
            "IMAGE_REVISION_MISMATCH",
            "image state changed while writing the object",
            json!({ "actualRevision": current }),
        ));
    }
    sqlx::query(
        "UPDATE data_project_worktree_entries SET annotation_hash = $5, status = $6, qa_status = $7, split = $8, review_note = $9, tags_json = $10, revision = $11, updated_by = $12, updated_at = now() WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(&image_id)
    .bind(hash)
    .bind(&status)
    .bind(&qa_status)
    .bind(&split)
    .bind(&review_note)
    .bind(&tags)
    .bind(next_revision)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE data_project_worktrees SET revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND branch_name = $2")
        .bind(project_id)
        .bind(&branch)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    get_image(state, user, &json!({ "datasetId": browser_dataset_id(project_id, dataset_key.as_str()), "branch": branch, "imageId": image_id })).await
}

async fn hydrated_metadata_value(
    state: &AppState,
    project_id: Uuid,
    metadata: &Value,
) -> Result<Value, ApiError> {
    let Some(hash) = metadata
        .get("__webMetadataHash")
        .and_then(Value::as_str)
        .filter(|hash| valid_hash(hash))
        .or_else(|| {
            metadata
                .get("metadataHash")
                .and_then(Value::as_str)
                .filter(|hash| valid_hash(hash))
        })
    else {
        return Ok(metadata.clone());
    };
    let row = sqlx::query(
        "SELECT object_key, byte_size FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 ORDER BY byte_size LIMIT 1",
    )
    .bind(project_id)
    .bind(hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::conflict("MISSING_OBJECT", "dataset metadata object is missing", json!({ "hash": hash })))?;
    let byte_size: i64 = row.get("byte_size");
    if byte_size > 16 * 1024 * 1024 {
        return Err(ApiError::bad_request(
            "dataset metadata object is too large",
        ));
    }
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(row.get::<String, _>("object_key"))
        .send()
        .await
        .map_err(|error| {
            ApiError::not_found(format!("dataset metadata object not found: {error}"))
        })?;
    let mut bytes = Vec::with_capacity(byte_size.max(0) as usize);
    object
        .body
        .into_async_read()
        .take((16 * 1024 * 1024 + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            ApiError::internal_message(format!("read dataset metadata object: {error}"))
        })?;
    if bytes.len() > 16 * 1024 * 1024 {
        return Err(ApiError::bad_request(
            "dataset metadata object is too large",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        ApiError::internal_message(format!("decode dataset metadata object: {error}"))
    })
}

async fn get_classes(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let (project_id, dataset_key, branch, _) =
        branch_for_dataset_args(state, user, args, WebPermission::View).await?;
    let row = sqlx::query(
        "SELECT metadata_json, metadata_hash, revision FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset metadata not found"))?;
    let raw_metadata = row.get::<Value, _>("metadata_json");
    let metadata = hydrated_metadata_value(state, project_id, &raw_metadata).await?;
    let classes = metadata
        .get("classes")
        .cloned()
        .or_else(|| metadata.get("labels").cloned())
        .unwrap_or_else(|| {
            if metadata.is_array() {
                metadata.clone()
            } else {
                json!([])
            }
        });
    Ok(json!({
        "dataProjectId": project_id,
        "datasetId": browser_dataset_id(project_id, &dataset_key),
        "branch": branch,
        "classes": classes,
        "metadata": metadata,
        "metadataHash": row.get::<Option<String>, _>("metadata_hash"),
        "revision": row.get::<i64, _>("revision")
    }))
}

async fn save_classes(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let (project_id, dataset_key, branch, dataset_id) =
        branch_for_dataset_args(state, user, args, WebPermission::Edit).await?;
    let metadata = optional_value(args, "metadata")
        .or_else(|| optional_value(args, "schema"))
        .or_else(|| optional_value(args, "classes").map(|classes| json!({ "classes": classes })))
        .ok_or_else(|| ApiError::bad_request("classes/schema/metadata is required"))?;
    if !metadata.is_object() && !metadata.is_array() {
        return Err(ApiError::bad_request("metadata must be an object or array"));
    }
    let metadata = if metadata.is_array() {
        json!({ "classes": metadata })
    } else {
        metadata
    };
    let owner_scope = project_owner_scope(state, project_id).await?;
    let dataset_name = source_dataset_name(state, dataset_id, &dataset_key).await?;
    let image_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .fetch_one(&state.pool)
    .await?;
    let metadata = canonical_metadata_value(
        &metadata,
        dataset_id,
        &dataset_key,
        &dataset_name,
        image_count as usize,
        &owner_scope,
    );
    let metadata_hash = json_hash(&metadata)?;
    let _ = store_json_object(
        state,
        project_id,
        user.id,
        &metadata,
        "dataset-metadata",
        "application/json",
    )
    .await?;
    let mut tx = state.pool.begin().await?;
    let current: i64 = sqlx::query_scalar(
        "SELECT revision FROM data_project_worktree_metadata WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 FOR UPDATE",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset metadata not found"))?;
    if let Some(expected) = expected_revision(args)? {
        if expected != current {
            return Err(ApiError::conflict(
                "METADATA_REVISION_MISMATCH",
                "dataset classes changed by another web client",
                json!({ "expectedRevision": expected, "actualRevision": current }),
            ));
        }
    }
    let next_revision = current + 1;
    sqlx::query(
        "UPDATE data_project_worktree_metadata SET dataset_id = COALESCE($4, dataset_id), metadata_hash = $5, metadata_json = $6, revision = $7, updated_by = $8, updated_at = now() WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
    )
    .bind(project_id)
    .bind(&branch)
    .bind(&dataset_key)
    .bind(dataset_id)
    .bind(&metadata_hash)
    .bind(&metadata)
    .bind(next_revision)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE data_project_worktrees SET revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND branch_name = $2")
        .bind(project_id)
        .bind(&branch)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    get_classes(state, user, &json!({ "datasetId": browser_dataset_id(project_id, dataset_key.as_str()), "branch": branch })).await
}

async fn submit_image(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let mut args = args.clone();
    if let Some(object) = args.as_object_mut() {
        object.insert(
            "status".to_string(),
            Value::String("pendingReview".to_string()),
        );
        object.insert(
            "qaStatus".to_string(),
            Value::String("pendingReview".to_string()),
        );
    }
    update_image_state(state, user, &args).await
}

async fn review_image(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    require_project(
        state,
        user.id,
        parse_uuid_string(
            &resolve_dataset_identity(
                state,
                user.id,
                &required_string(args, "datasetId")?,
                WebPermission::Edit,
            )
            .await?
            .0
            .to_string(),
            "dataProjectId",
        )?,
        WebPermission::Edit,
    )
    .await?;
    let decision = optional_string(args, "decision")?
        .or_else(|| optional_string(args, "qaStatus").ok().flatten())
        .unwrap_or_else(|| "approved".to_string());
    let normalized = match decision.to_ascii_lowercase().as_str() {
        "approved" | "approve" | "通过" | "pass" => "approved",
        "rejected" | "reject" | "驳回" => "rejected",
        _ => {
            return Err(ApiError::bad_request(
                "decision must be approved or rejected",
            ))
        }
    };
    let mut args = args.clone();
    if let Some(object) = args.as_object_mut() {
        object.insert("status".to_string(), Value::String(normalized.to_string()));
        object.insert(
            "qaStatus".to_string(),
            Value::String(normalized.to_string()),
        );
    }
    update_image_state(state, user, &args).await
}

async fn review_queue(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let rows = sqlx::query(
        "SELECT dataset_key, dataset_id, image_id, file_name, image_hash, image_byte_size, image_mime_type, annotation_hash, annotation_json, status, qa_status, split, width, height, review_note, tags_json, source_item_id, revision FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND (status IN ('pendingReview', 'pending_review', 'submitted') OR qa_status IN ('pendingReview', 'pending_review', '待审核')) ORDER BY updated_at, image_id",
    )
    .bind(project_id)
    .bind(&branch)
    .fetch_all(&state.pool)
    .await?;
    Ok(json!({
        "dataProjectId": project_id,
        "branch": branch,
        "items": rows.iter().map(|row| image_item_json(project_id, &work_entry_from_row(row), false)).collect::<Vec<_>>()
    }))
}

async fn upload_standalone_image(
    state: &AppState,
    user: &AuthUser,
    dataset_id: Uuid,
    file_name: String,
    headers: &HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    require_standalone_dataset(state, user.id, dataset_id).await?;
    let expected_size = headers
        .get("x-object-size")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok());
    let temp_path = temp_object_path(dataset_id);
    let upload_result: Result<(String, i64), ApiError> = async {
        let mut file = tokio::fs::File::create(&temp_path).await.map_err(|error| {
            ApiError::internal_message(format!("create upload temp file: {error}"))
        })?;
        let mut stream = body.into_data_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_i64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| ApiError::bad_request(format!("read upload body: {error}")))?;
            size = size.saturating_add(chunk.len() as i64);
            if size > MAX_WEB_IMAGE_BYTES {
                return Err(ApiError::bad_request("image upload is too large"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                ApiError::internal_message(format!("write upload temp file: {error}"))
            })?;
        }
        file.flush().await.map_err(|error| {
            ApiError::internal_message(format!("flush upload temp file: {error}"))
        })?;
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(ApiError::bad_request(
                "X-Object-Size does not match upload body",
            ));
        }
        Ok((hex::encode(hasher.finalize()), size))
    }
    .await;
    let (hash, byte_size) = match upload_result {
        Ok(value) => value,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error);
        }
    };
    let dimensions = match ImageReader::open(&temp_path)
        .map_err(|error| format!("uploaded file is not an image: {error}"))
        .and_then(|reader| {
            reader
                .with_guessed_format()
                .map_err(|error| format!("cannot detect uploaded image format: {error}"))
        })
        .and_then(|reader| {
            reader
                .into_dimensions()
                .map_err(|error| format!("cannot read image dimensions: {error}"))
        }) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::bad_request(error));
        }
    };
    if dimensions.0 == 0
        || dimensions.1 == 0
        || (dimensions.0 as u64).saturating_mul(dimensions.1 as u64) > MAX_WEB_IMAGE_PIXELS
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(ApiError::bad_request(
            "uploaded image dimensions exceed the supported pixel limit",
        ));
    }
    // Content type is derived from validated image bytes. Never serve a
    // client-supplied HTML/SVG type from this same-origin authenticated API.
    let mime_type = ImageReader::open(&temp_path)
        .map_err(|_| ApiError::bad_request("uploaded image cannot be opened"))?
        .with_guessed_format()
        .map_err(|_| ApiError::bad_request("uploaded image format cannot be detected"))?
        .format()
        .ok_or_else(|| ApiError::bad_request("uploaded image format is unsupported"))?
        .to_mime_type()
        .to_string();
    let key = format!("datasets/{dataset_id}/objects/{hash}");
    let stream = match ByteStream::from_path(&temp_path).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::internal_message(format!(
                "open image upload: {error}"
            )));
        }
    };
    let s3_result = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&key)
        .content_type(mime_type.clone())
        .metadata("sha256", hash.clone())
        .body(stream)
        .send()
        .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    s3_result
        .map_err(|error| ApiError::internal_message(format!("store image object: {error}")))?;
    let image_id = Uuid::new_v4();
    let annotation = json!([]);
    let annotation_hash = json_hash(&annotation)?;
    let organization_id: Uuid = sqlx::query_scalar(
        "SELECT organization_id FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted'",
    )
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let mut tx = state.pool.begin().await?;
    let blob_id: Uuid = sqlx::query_scalar(
        "INSERT INTO dataset_blobs (organization_id, object_key, file_name, content_hash, mime_type, width, height, byte_size) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (organization_id, content_hash, byte_size, mime_type) DO UPDATE SET object_key = dataset_blobs.object_key RETURNING id",
    )
    .bind(organization_id)
    .bind(&key)
    .bind(&file_name)
    .bind(&hash)
    .bind(&mime_type)
    .bind(dimensions.0 as i32)
    .bind(dimensions.1 as i32)
    .bind(byte_size)
    .fetch_one(&mut *tx)
    .await?;
    let object_key: String =
        sqlx::query_scalar("SELECT object_key FROM dataset_blobs WHERE id = $1")
            .bind(blob_id)
            .fetch_one(&mut *tx)
            .await?;
    sqlx::query(
        "INSERT INTO dataset_items (id, dataset_id, blob_id, client_key, file_name, split, annotation_json, annotation_hash) VALUES ($1, $2, $3, $4, $5, 'train', $6, $7)",
    )
    .bind(image_id)
    .bind(dataset_id)
    .bind(blob_id)
    .bind(image_id.to_string())
    .bind(&file_name)
    .bind(&annotation)
    .bind(&annotation_hash)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE datasets SET revision = revision + 1, updated_at = now() WHERE id = $1")
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "ok": true,
        "data": {
            "datasetId": dataset_id,
            "imageId": image_id,
            "imageHash": hash,
            "byteSize": byte_size,
            "width": dimensions.0,
            "height": dimensions.1,
            "objectKey": object_key
        }
    })))
}

pub async fn upload_image(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_value): Path<String>,
    Query(query): Query<UploadImageQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    let file_name = sanitize_file_name(&query.file_name)?;
    let target =
        resolve_web_dataset_target(&state, user.id, &dataset_value, WebPermission::Edit).await?;
    let (project_id, dataset_key, dataset_id) = match target {
        WebDatasetTarget::Project {
            project_id,
            dataset_key,
            dataset_id,
        } => (project_id, dataset_key, dataset_id),
        WebDatasetTarget::Standalone { dataset_id } => {
            return upload_standalone_image(&state, &user, dataset_id, file_name, &headers, body)
                .await;
        }
    };
    let branch = match query.branch {
        Some(branch) => branch,
        None => active_branch(&state, project_id, user.id).await?,
    };
    let branch = validate_branch(&branch)?;
    ensure_worktree_initialized(&state, project_id, user.id, &branch).await?;
    let expected_size = headers
        .get("x-object-size")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok());
    let temp_path = temp_object_path(project_id);
    let upload_result: Result<(String, i64), ApiError> = async {
        let mut file = tokio::fs::File::create(&temp_path).await.map_err(|error| {
            ApiError::internal_message(format!("create upload temp file: {error}"))
        })?;
        let mut stream = body.into_data_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_i64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| ApiError::bad_request(format!("read upload body: {error}")))?;
            size = size.saturating_add(chunk.len() as i64);
            if size > MAX_WEB_IMAGE_BYTES {
                return Err(ApiError::bad_request("image upload is too large"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                ApiError::internal_message(format!("write upload temp file: {error}"))
            })?;
        }
        file.flush().await.map_err(|error| {
            ApiError::internal_message(format!("flush upload temp file: {error}"))
        })?;
        if expected_size.is_some_and(|expected| expected != size) {
            return Err(ApiError::bad_request(
                "X-Object-Size does not match upload body",
            ));
        }
        Ok((hex::encode(hasher.finalize()), size))
    }
    .await;
    let (hash, byte_size) = match upload_result {
        Ok(value) => value,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(error);
        }
    };
    let dimensions = match ImageReader::open(&temp_path)
        .map_err(|error| format!("uploaded file is not an image: {error}"))
        .and_then(|reader| {
            reader
                .with_guessed_format()
                .map_err(|error| format!("cannot detect uploaded image format: {error}"))
        })
        .and_then(|reader| {
            reader
                .into_dimensions()
                .map_err(|error| format!("cannot read image dimensions: {error}"))
        }) {
        Ok(dimensions) => dimensions,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::bad_request(error));
        }
    };
    if dimensions.0 == 0
        || dimensions.1 == 0
        || (dimensions.0 as u64).saturating_mul(dimensions.1 as u64) > MAX_WEB_IMAGE_PIXELS
    {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(ApiError::bad_request(
            "uploaded image dimensions exceed the supported pixel limit",
        ));
    }
    // Content type is derived from validated image bytes. Never serve a
    // client-supplied HTML/SVG type from this same-origin authenticated API.
    let mime_type = ImageReader::open(&temp_path)
        .map_err(|_| ApiError::bad_request("uploaded image cannot be opened"))?
        .with_guessed_format()
        .map_err(|_| ApiError::bad_request("uploaded image format cannot be detected"))?
        .format()
        .ok_or_else(|| ApiError::bad_request("uploaded image format is unsupported"))?
        .to_mime_type()
        .to_string();
    let key = object_key(project_id, &hash);
    let stream = match ByteStream::from_path(&temp_path).await {
        Ok(stream) => stream,
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp_path).await;
            return Err(ApiError::internal_message(format!(
                "open image upload: {error}"
            )));
        }
    };
    let s3_result = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&key)
        .content_type(mime_type.clone())
        .metadata("sha256", hash.clone())
        .body(stream)
        .send()
        .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    s3_result
        .map_err(|error| ApiError::internal_message(format!("store image object: {error}")))?;
    insert_project_blob(
        &state, project_id, user.id, &hash, byte_size, "image", &mime_type, &key,
    )
    .await?;
    let annotation = json!([]);
    let status = "raw".to_string();
    let qa_status = default_qa_status();
    let annotation_hash = annotation_hash(&annotation, &status, &qa_status)?;
    let payload = annotation_payload(&annotation, &status, &qa_status);
    let _ = store_json_object(
        &state,
        project_id,
        user.id,
        &payload,
        "annotation",
        "application/json",
    )
    .await?;
    let image_id = Uuid::new_v4().to_string();
    let mut tx = state.pool.begin().await?;
    insert_entry_tx(
        &mut tx,
        project_id,
        &branch,
        &WorkEntry {
            dataset_key: dataset_key.clone(),
            // Native graph memberships deliberately have no hosted Dataset
            // UUID.  Keep the canonical dataset key as the identity and
            // attach newly uploaded bytes directly to this project worktree;
            // creating a synthetic hosted row here would change graph
            // membership and break desktop/cloud compatibility.
            dataset_id,
            image_id: image_id.clone(),
            file_name,
            image_hash: hash.clone(),
            image_byte_size: Some(byte_size),
            image_mime_type: Some(mime_type.clone()),
            annotation_hash,
            annotation,
            status,
            qa_status,
            split: "train".to_string(),
            width: dimensions.0 as i32,
            height: dimensions.1 as i32,
            review_note: None,
            tags: json!([]),
            source_item_id: None,
            revision: 1,
        },
        user.id,
    )
    .await?;
    sqlx::query("UPDATE data_project_worktrees SET revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND branch_name = $2")
        .bind(project_id)
        .bind(&branch)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "ok": true,
        "data": {
            "dataProjectId": project_id,
            "datasetId": browser_dataset_id(project_id, &dataset_key),
            "branch": branch,
            "imageId": image_id,
            "imageHash": hash,
            "byteSize": byte_size,
            "width": dimensions.0,
            "height": dimensions.1,
            "imageUrl": format!("/api/v1/web/projects/{project_id}/objects/{hash}")
        }
    })))
}

pub async fn download_image_content(
    State(state): State<AppState>,
    user: AuthUser,
    Path((dataset_value, image_id)): Path<(String, String)>,
    Query(query): Query<ImageContentQuery>,
) -> Result<Response, ApiError> {
    let target =
        resolve_web_dataset_target(&state, user.id, &dataset_value, WebPermission::View).await?;
    let (object_key, byte_size, mime_type) = match target {
        WebDatasetTarget::Project {
            project_id,
            dataset_key,
            ..
        } => {
            let branch = match query.branch {
                Some(branch) => validate_branch(&branch)?,
                None => active_branch(&state, project_id, user.id).await?,
            };
            ensure_worktree_initialized(&state, project_id, user.id, &branch).await?;
            let row = sqlx::query(
                "SELECT image_hash, image_byte_size, image_mime_type FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4",
            )
            .bind(project_id)
            .bind(&branch)
            .bind(&dataset_key)
            .bind(&image_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("data-project image not found"))?;
            let hash: String = row.get("image_hash");
            let blob = sqlx::query(
                "SELECT object_key, byte_size, mime_type FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 ORDER BY byte_size LIMIT 1",
            )
            .bind(project_id)
            .bind(&hash)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("image object not found"))?;
            (
                blob.get::<String, _>("object_key"),
                blob.get::<i64, _>("byte_size"),
                blob.get::<String, _>("mime_type"),
            )
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            require_standalone_dataset(&state, user.id, dataset_id).await?;
            let image_id = parse_uuid_string(&image_id, "imageId")?;
            let row = sqlx::query(
                "SELECT db.object_key, db.byte_size, db.mime_type FROM dataset_items di JOIN dataset_blobs db ON db.id = di.blob_id WHERE di.dataset_id = $1 AND di.id = $2",
            )
            .bind(dataset_id)
            .bind(image_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("dataset image not found"))?;
            (
                row.get::<String, _>("object_key"),
                row.get::<i64, _>("byte_size"),
                row.get::<String, _>("mime_type"),
            )
        }
    };
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .send()
        .await
        .map_err(|error| ApiError::not_found(format!("image object not found: {error}")))?;
    let mut response =
        Body::from_stream(ReaderStream::new(object.body.into_async_read())).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(value) = HeaderValue::from_str(&byte_size.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    Ok(response)
}

pub async fn download_object(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, hash)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    require_project(&state, user.id, project_id, WebPermission::View).await?;
    let hash = hash.to_ascii_lowercase();
    if !valid_hash(&hash) {
        return Err(ApiError::bad_request(
            "object hash must be a SHA-256 hex digest",
        ));
    }
    let row = sqlx::query(
        "SELECT object_key, byte_size, mime_type FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 ORDER BY byte_size LIMIT 1",
    )
    .bind(project_id)
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("web object not found"))?;
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(row.get::<String, _>("object_key"))
        .send()
        .await
        .map_err(|error| ApiError::not_found(format!("web object not found: {error}")))?;
    let mut response =
        Body::from_stream(ReaderStream::new(object.body.into_async_read())).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&row.get::<String, _>("mime_type"))
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(value) = HeaderValue::from_str(&row.get::<i64, _>("byte_size").to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    Ok(response)
}

async fn project_detail(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
) -> Result<Value, ApiError> {
    let project = project_summary(state, user.id, project_id).await?;
    let datasets = sqlx::query(
        "SELECT pd.dataset_key, pd.dataset_id, pd.metadata_json, pd.position, d.name, d.description, d.schema_json, d.status FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id JOIN users owner ON owner.id = p.owner_id LEFT JOIN datasets d ON d.id = pd.dataset_id WHERE pd.project_id = $1 AND (pd.dataset_id IS NULL OR (d.id IS NOT NULL AND d.deleted_at IS NULL AND d.status <> 'deleted' AND COALESCE(p.organization_id, owner.organization_id) IS NOT DISTINCT FROM d.organization_id)) ORDER BY pd.position, pd.dataset_key",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let active_branch_name = active_branch(state, project_id, user.id).await?;
    ensure_worktree_initialized(state, project_id, user.id, &active_branch_name).await?;
    let (_, _, project_metadata, _, _) =
        load_worktree(state, project_id, &active_branch_name).await?;
    let branches = list_branches(state, user, project_id).await?;
    Ok(json!({
        "project": project,
        "datasets": datasets.iter().map(|row| json!({
            "id": browser_dataset_id(project_id, &row.get::<String, _>("dataset_key")),
            "datasetId": row.get::<Option<Uuid>, _>("dataset_id"),
            "datasetKey": row.get::<String, _>("dataset_key"),
            "name": row.get::<Option<String>, _>("name"),
            "description": row.get::<Option<String>, _>("description"),
            "schema": row.get::<Option<Value>, _>("schema_json").unwrap_or_else(|| row.get::<Value, _>("metadata_json")),
            "metadata": row.get::<Value, _>("metadata_json"),
            "position": row.get::<i32, _>("position"),
            "status": row.get::<Option<String>, _>("status")
        })).collect::<Vec<_>>(),
        "branches": branches,
        "activeBranch": active_branch_name,
        "projectMetadata": data_project_metadata::to_value(&project_metadata)
            .map_err(ApiError::internal_message)?
    }))
}

fn schema_classes(schema: &Value) -> Value {
    schema
        .get("classes")
        .cloned()
        .or_else(|| schema.get("labels").cloned())
        .or_else(|| schema.as_array().map(|_| schema.clone()))
        .unwrap_or_else(|| json!([]))
}

fn annotation_types(schema: &Value) -> Vec<String> {
    schema
        .get("annotationTypes")
        .or_else(|| schema.get("annotation_types"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

async fn dataset_metrics(
    state: &AppState,
    user: &AuthUser,
    target: &WebDatasetTarget,
) -> Result<(i64, i64, i64), ApiError> {
    match target {
        WebDatasetTarget::Project {
            project_id,
            dataset_key,
            ..
        } => {
            let branch = active_branch(state, *project_id, user.id).await?;
            ensure_worktree_initialized(state, *project_id, user.id, &branch).await?;
            let rows = sqlx::query(
                "SELECT annotation_json, status, qa_status FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3",
            )
            .bind(project_id)
            .bind(&branch)
            .bind(dataset_key)
            .fetch_all(&state.pool)
            .await?;
            let total = rows.len() as i64;
            let mut annotated = 0_i64;
            let mut review = 0_i64;
            for row in rows {
                let annotation = row.get::<Value, _>("annotation_json");
                if !annotation_is_empty(&annotation) {
                    annotated += 1;
                }
                let status: String = row.get("status");
                let qa_status: String = row.get("qa_status");
                if matches!(
                    status_bucket(&status, &qa_status, &annotation),
                    "pendingReview" | "rejected"
                ) {
                    review += 1;
                }
            }
            Ok((total, annotated, review))
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            require_standalone_dataset(state, user.id, *dataset_id).await?;
            let rows =
                sqlx::query("SELECT annotation_json FROM dataset_items WHERE dataset_id = $1")
                    .bind(dataset_id)
                    .fetch_all(&state.pool)
                    .await?;
            let total = rows.len() as i64;
            let annotated = rows
                .iter()
                .filter(|row| !annotation_is_empty(&row.get::<Value, _>("annotation_json")))
                .count() as i64;
            Ok((total, annotated, 0))
        }
    }
}

async fn list_dataset_projects_command(
    state: &AppState,
    user: &AuthUser,
) -> Result<Value, ApiError> {
    let payload = list_datasets(state, user.id).await?;
    let datasets = payload
        .get("datasets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut projects = Vec::with_capacity(datasets.len());
    for dataset in datasets {
        let id = dataset
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal_message("dataset list item has no id"))?;
        let target = resolve_web_dataset_target(state, user.id, id, WebPermission::View).await?;
        let (image_count, annotated_count, review_count) =
            dataset_metrics(state, user, &target).await?;
        let schema = dataset
            .get("metadata")
            .or_else(|| dataset.get("schema"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        let class_count = schema_classes(&schema)
            .as_array()
            .map(|items| items.len())
            .unwrap_or(0);
        let tags = dataset
            .get("tags")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_else(|| metadata_tags(&schema));
        projects.push(json!({
            "id": id,
            "name": dataset.get("name").cloned().unwrap_or_else(|| json!("")),
            "description": dataset.get("description").cloned().unwrap_or_else(|| json!("")),
            "annotationTypes": annotation_types_for_schema(&schema),
            "imageCount": image_count,
            "annotatedPercent": if image_count == 0 { 0.0 } else { (annotated_count as f64 * 100.0) / image_count as f64 },
            "reviewCount": review_count,
            "issueCount": 0,
            "classCount": class_count,
            "tagGroupCount": 0,
            "status": dataset.get("status").cloned().unwrap_or_else(|| json!("active")),
            "tags": tags
        }));
    }
    Ok(Value::Array(projects))
}

async fn list_data_project_datasets_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = active_branch(state, project_id, user.id).await?;
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let (_, _, _, worktree_metadata, _) = load_worktree(state, project_id, &branch).await?;
    let worktree_metadata = worktree_metadata
        .into_iter()
        .map(|metadata| (metadata.dataset_key.clone(), metadata.metadata))
        .collect::<BTreeMap<_, _>>();
    let rows = sqlx::query(
        "SELECT pd.dataset_key, pd.dataset_id, COALESCE(d.schema_json, pd.metadata_json) AS metadata_json, d.name, d.description, d.status, p.created_at, pd.updated_at FROM data_project_datasets pd JOIN data_projects p ON p.id = pd.project_id JOIN users owner ON owner.id = p.owner_id LEFT JOIN datasets d ON d.id = pd.dataset_id WHERE pd.project_id = $1 AND (pd.dataset_id IS NULL OR (d.id IS NOT NULL AND d.deleted_at IS NULL AND d.status <> 'deleted' AND COALESCE(p.organization_id, owner.organization_id) IS NOT DISTINCT FROM d.organization_id)) ORDER BY pd.position, pd.dataset_key",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let mut projects = Vec::with_capacity(rows.len());
    for row in rows {
        let dataset_key: String = row.get("dataset_key");
        let dataset_id: Option<Uuid> = row.get("dataset_id");
        let browser_id = browser_dataset_id(project_id, &dataset_key);
        let raw_metadata = worktree_metadata
            .get(&dataset_key)
            .cloned()
            .unwrap_or_else(|| row.get("metadata_json"));
        let metadata = listing_metadata_value(state, project_id, &raw_metadata).await;
        let format = metadata_format(&metadata);
        let target = WebDatasetTarget::Project {
            project_id,
            dataset_key: dataset_key.clone(),
            dataset_id,
        };
        let (image_count, annotated_count, review_count) =
            dataset_metrics(state, user, &target).await?;
        let classes = schema_classes(&metadata);
        let class_count = classes.as_array().map_or(0, Vec::len);
        let name = row
            .get::<Option<String>, _>("name")
            .unwrap_or_else(|| metadata_display_name(&metadata, &dataset_key));
        let description = row
            .get::<Option<String>, _>("description")
            .unwrap_or_else(|| metadata_display_description(&metadata, format.as_deref()));
        let status = row
            .get::<Option<String>, _>("status")
            .unwrap_or_else(|| metadata_status(&metadata));
        projects.push(json!({
            "id": browser_id,
            "datasetId": dataset_id,
            "dataProjectId": project_id,
            "datasetKey": dataset_key,
            "name": name,
            "description": description,
            "schema": metadata.clone(),
            "metadata": metadata,
            "format": format,
            "annotationTypes": annotation_types_for_schema(&metadata),
            "imageCount": image_count,
            "annotatedPercent": if image_count == 0 { 0.0 } else { (annotated_count as f64 * 100.0) / image_count as f64 },
            "reviewCount": review_count,
            "issueCount": 0,
            "classCount": class_count,
            "tagGroupCount": 0,
            "status": status,
            "tags": metadata_tags(&metadata),
            "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        }));
    }
    Ok(Value::Array(projects))
}

async fn list_project_images_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = optional_string(args, "datasetId")?
        .or_else(|| optional_string(args, "projectId").ok().flatten())
        .ok_or_else(|| ApiError::bad_request("datasetId/projectId is required"))?;
    let target =
        resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::View).await?;
    match target {
        WebDatasetTarget::Project { .. } => {
            let mut forwarded = args.clone();
            if let Some(object) = forwarded.as_object_mut() {
                object.insert("datasetId".to_string(), Value::String(dataset_value));
            }
            let page = list_images(state, user, &forwarded).await?;
            Ok(page.get("items").cloned().unwrap_or_else(|| json!([])))
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            list_standalone_images(state, user, dataset_id, args).await
        }
    }
}

async fn project_detail_for_dataset(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = optional_string(args, "datasetId")?
        .or_else(|| optional_string(args, "projectId").ok().flatten())
        .ok_or_else(|| ApiError::bad_request("datasetId/projectId is required"))?;
    let target =
        resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::View).await?;
    let datasets = list_datasets(state, user.id).await?;
    let dataset = datasets
        .get("datasets")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("id").and_then(Value::as_str) == Some(dataset_value.as_str()))
        })
        .cloned()
        .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let schema = match &target {
        WebDatasetTarget::Project {
            project_id,
            dataset_key,
            ..
        } => get_classes(
            state,
            user,
            &json!({ "datasetId": browser_dataset_id(*project_id, dataset_key) }),
        )
        .await
        .ok()
        .and_then(|value| value.get("metadata").cloned())
        .unwrap_or_else(|| dataset.get("schema").cloned().unwrap_or_else(|| json!({}))),
        WebDatasetTarget::Standalone { .. } => {
            dataset.get("schema").cloned().unwrap_or_else(|| json!({}))
        }
    };
    let (image_count, annotated_count, review_count) =
        dataset_metrics(state, user, &target).await?;
    let classes = schema_classes(&schema);
    let class_stats = classes
        .as_array()
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let label = value
                        .as_str()
                        .map(ToOwned::to_owned)
                        .or_else(|| {
                            value
                                .get("label")
                                .and_then(Value::as_str)
                                .map(ToOwned::to_owned)
                        })
                        .unwrap_or_else(|| format!("class-{index}"));
                    json!({ "id": index, "label": label, "color": "#5b8def", "count": 0, "attributes": [] })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(json!({
        "project": {
            "id": dataset_value,
            "name": dataset.get("name").cloned().unwrap_or_else(|| json!("")),
            "description": dataset.get("description").cloned().unwrap_or_else(|| json!("")),
            "annotationTypes": annotation_types(&schema),
            "imageCount": image_count,
            "annotatedPercent": if image_count == 0 { 0.0 } else { (annotated_count as f64 * 100.0) / image_count as f64 },
            "reviewCount": review_count,
            "issueCount": 0,
            "classCount": class_stats.len(),
            "tagGroupCount": 0,
            "status": dataset.get("status").cloned().unwrap_or_else(|| json!("active")),
            "tags": []
        },
        "tagGroups": [],
        "classes": class_stats,
        "tasks": [],
        "qualityChecks": [],
        "exportPresets": []
    }))
}

async fn create_task(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Edit).await?;
    let name = required_string(args, "name")?;
    let images = arg(args, "imageIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let branch = active_branch(state, project_id, user.id).await?;
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let mut task_items = Vec::with_capacity(images.len());
    for image in images {
        let (dataset_key, image_id) = if let Some(value) = image.as_str() {
            if let Some((dataset, image)) = value.rsplit_once("::") {
                if let Ok((encoded_project, key)) = parse_browser_dataset_id(dataset) {
                    if encoded_project != project_id {
                        return Err(ApiError::forbidden(
                            "task item dataset belongs to another data project",
                        ));
                    }
                    (key, image.to_string())
                } else {
                    (dataset.to_string(), image.to_string())
                }
            } else {
                (String::new(), String::new())
            }
        } else {
            let dataset = image.get("datasetId").and_then(Value::as_str).unwrap_or("");
            let image_id = image.get("imageId").and_then(Value::as_str).unwrap_or("");
            if let Ok((encoded_project, key)) = parse_browser_dataset_id(dataset) {
                if encoded_project != project_id {
                    return Err(ApiError::forbidden(
                        "task item dataset belongs to another data project",
                    ));
                }
                (key, image_id.to_string())
            } else {
                (dataset.to_string(), image_id.to_string())
            }
        };
        if dataset_key.is_empty() || image_id.is_empty() {
            return Err(ApiError::bad_request(
                "each task item needs a project dataset and imageId",
            ));
        }
        task_items.push((dataset_key, image_id));
    }
    let item_count = task_items.len();
    let task_id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let dataset_keys = task_items
        .iter()
        .map(|(dataset_key, _)| dataset_key.clone())
        .collect::<BTreeSet<_>>();
    let owned_keys = sqlx::query_scalar::<_, String>(
        "SELECT dataset_key FROM data_project_datasets WHERE project_id = $1 AND dataset_key = ANY($2)",
    )
    .bind(project_id)
    .bind(dataset_keys.iter().cloned().collect::<Vec<_>>())
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect::<BTreeSet<_>>();
    if owned_keys.len() != dataset_keys.len() {
        return Err(ApiError::forbidden(
            "task contains a dataset outside this data project",
        ));
    }
    sqlx::query(
        "INSERT INTO data_project_tasks (id, project_id, name, status, created_by, updated_by) VALUES ($1, $2, $3, 'unassigned', $4, $4)",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(&name)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    for (dataset_key, image_id) in task_items {
        let image_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM data_project_worktree_entries WHERE project_id = $1 AND branch_name = $2 AND dataset_key = $3 AND image_id = $4)",
        )
        .bind(project_id)
        .bind(&branch)
        .bind(&dataset_key)
        .bind(&image_id)
        .fetch_one(&mut *tx)
        .await?;
        if !image_exists {
            return Err(ApiError::not_found(
                "task image is not in the project branch",
            ));
        }
        sqlx::query(
            "INSERT INTO data_project_task_items (task_id, dataset_key, image_id) VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
        )
        .bind(task_id)
        .bind(&dataset_key)
        .bind(&image_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(json!({
        "id": task_id,
        "dataProjectId": project_id,
        "name": name,
        "status": "unassigned",
        "itemCount": item_count
    }))
}

async fn list_tasks(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let rows = sqlx::query(
        "SELECT t.id, t.name, t.status, t.assignee_id, t.revision, t.created_at, t.updated_at, COUNT(i.image_id) AS item_count FROM data_project_tasks t LEFT JOIN data_project_task_items i ON i.task_id = t.id WHERE t.project_id = $1 AND t.deleted_at IS NULL GROUP BY t.id ORDER BY t.created_at DESC",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(json!({
        "dataProjectId": project_id,
        "tasks": rows.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "dataProjectId": project_id,
            "name": row.get::<String, _>("name"),
            "status": row.get::<String, _>("status"),
            "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
            "revision": row.get::<i64, _>("revision"),
            "itemCount": row.get::<i64, _>("item_count"),
            "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        })).collect::<Vec<_>>()
    }))
}

async fn export_project(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = match optional_string(args, "branch")? {
        Some(branch) => validate_branch(&branch)?,
        None => active_branch(state, project_id, user.id).await?,
    };
    ensure_worktree_initialized(state, project_id, user.id, &branch).await?;
    let (revision, base, project_metadata, metadata, entries) =
        load_worktree(state, project_id, &branch).await?;
    let tree = TreeDocument {
        project_metadata,
        metadata,
        entries,
    };
    let format = optional_string(args, "format")?.unwrap_or_else(|| "manifest-json".to_string());
    let manifest = json!({
        "schemaVersion": 1,
        "dataProjectId": project_id,
        "branch": branch,
        "baseCommitId": base,
        "workingTreeRevision": revision,
        "tree": tree_document_json(&tree)
    });
    let export_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO data_project_exports (id, project_id, branch_name, format, status, manifest_json, created_by) VALUES ($1, $2, $3, $4, 'succeeded', $5, $6)",
    )
    .bind(export_id)
    .bind(project_id)
    .bind(&branch)
    .bind(&format)
    .bind(&manifest)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(
        json!({ "id": export_id, "dataProjectId": project_id, "branch": branch, "format": format, "status": "succeeded", "manifest": manifest }),
    )
}

async fn sync_status(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let project = project_summary(state, user.id, project_id).await?;
    let tree = working_tree(state, user, project_id, None).await?;
    Ok(json!({
        "localProjectId": project_id,
        "remoteProjectId": project_id,
        "remoteRevision": project.revision,
        "localHead": tree.head_commit_id,
        "remoteHead": tree.head_commit_id,
        "dirty": tree.dirty,
        "state": if tree.dirty { "dirty" } else { "upToDate" },
        "trackingRef": format!("{}::main", project_id)
    }))
}

async fn publish_status(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let project = project_summary(state, user.id, project_id).await?;
    let tree = working_tree(state, user, project_id, None).await?;
    if tree.dirty {
        return Err(ApiError::conflict(
            "DIRTY_WORKTREE",
            "commit the web working tree before publishing",
            Value::Null,
        ));
    }
    Ok(json!({
        "localProjectId": project_id,
        "remoteProjectId": project_id,
        "remoteRevision": project.revision,
        "localHead": tree.head_commit_id,
        "remoteHead": tree.head_commit_id,
        "outcome": "upToDate",
        "trackingRef": format!("{}::main", project_id)
    }))
}

async fn list_members(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let rows = sqlx::query(
        "SELECT m.user_id, m.role, m.joined_at, u.email, u.display_name FROM data_project_members m JOIN users u ON u.id = m.user_id WHERE m.project_id = $1 AND m.deleted_at IS NULL AND u.deleted_at IS NULL ORDER BY m.joined_at, m.user_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(json!(rows
        .iter()
        .map(|row| json!({
            "userId": row.get::<Uuid, _>("user_id"),
            "role": row.get::<String, _>("role"),
            "email": row.get::<String, _>("email"),
            "displayName": row.get::<String, _>("display_name"),
            "joinedAt": row.get::<DateTime<Utc>, _>("joined_at")
        }))
        .collect::<Vec<_>>()))
}

async fn upsert_member(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Manage).await?;
    let member_id = required_uuid(args, "userId")?;
    let role = required_string(args, "role")?;
    if !matches!(role.as_str(), "owner" | "admin" | "editor" | "viewer") {
        return Err(ApiError::bad_request("invalid data-project member role"));
    }
    let target_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM users WHERE id = $1 AND deleted_at IS NULL)",
    )
    .bind(member_id)
    .fetch_one(&state.pool)
    .await?;
    if !target_exists {
        return Err(ApiError::not_found("user not found"));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    sqlx::query(
        "INSERT INTO data_project_members (project_id, user_id, role, deleted_at) VALUES ($1, $2, $3, NULL) ON CONFLICT (project_id, user_id) DO UPDATE SET role = excluded.role, deleted_at = NULL",
    )
    .bind(project_id)
    .bind(member_id)
    .bind(&role)
    .execute(&mut *tx)
    .await?;
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    Ok(json!({ "projectId": project_id, "userId": member_id, "role": role }))
}

async fn remove_member(state: &AppState, user: &AuthUser, args: &Value) -> Result<Value, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    require_project(state, user.id, project_id, WebPermission::Manage).await?;
    let member_id = required_uuid(args, "userId")?;
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM data_projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE")
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM data_project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(member_id)
    .fetch_optional(&mut *tx)
    .await?;
    if role.as_deref() == Some("owner") {
        return Err(ApiError::conflict(
            "OWNER_REQUIRED",
            "project owner cannot be removed",
            Value::Null,
        ));
    }
    sqlx::query("UPDATE data_project_members SET deleted_at = now() WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL")
        .bind(project_id)
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    bump_project_revision_tx(&mut tx, project_id, user.id).await?;
    tx.commit().await?;
    Ok(json!({ "projectId": project_id, "userId": member_id, "deleted": true }))
}

async fn get_commit(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<WebCommit, ApiError> {
    let project_id = required_uuid(args, "dataProjectId")?;
    let commit_id = required_string(args, "commitId")?;
    require_project(state, user.id, project_id, WebPermission::View).await?;
    let branch = optional_string(args, "branch")?;
    let row = load_commit_row(state, project_id, &commit_id).await?;
    commit_from_row(project_id, branch.as_deref(), &row)
}

fn serialize<T: Serialize>(value: T) -> Result<Value, ApiError> {
    serde_json::to_value(value)
        .map_err(|error| ApiError::internal_message(format!("encode web command result: {error}")))
}

/// Explicit browser command protocol.  The route is intentionally a single
/// POST endpoint so the web client can use the same camelCase DTOs as the
/// accepted native data-project UI without pretending that a browser has a
/// filesystem checkout.  `args` may be either the request body itself or an
/// `{ "args": { ... } }` wrapper.
fn dataset_value_from_command(args: &Value) -> Result<String, ApiError> {
    optional_string(args, "datasetId")?
        .or_else(|| optional_string(args, "projectId").ok().flatten())
        .ok_or_else(|| ApiError::bad_request("datasetId/projectId is required"))
}

async fn annotation_state_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    let image_id = required_string(args, "imageId")?;
    match resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::View).await? {
        WebDatasetTarget::Project { .. } => {
            let mut forwarded = args.clone();
            if let Some(object) = forwarded.as_object_mut() {
                object.insert("datasetId".to_string(), Value::String(dataset_value));
            }
            get_annotation(state, user, &forwarded).await
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            get_standalone_annotation(state, user, dataset_id, &image_id).await
        }
    }
}

async fn annotation_objects_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let state = annotation_state_command(state, user, args).await?;
    Ok(state.get("objects").cloned().unwrap_or_else(|| json!([])))
}

async fn save_annotation_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    let image_id = required_string(args, "imageId")?;
    match resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::Edit).await? {
        WebDatasetTarget::Project { .. } => {
            let mut forwarded = args.clone();
            if let Some(object) = forwarded.as_object_mut() {
                object.insert("datasetId".to_string(), Value::String(dataset_value));
            }
            save_annotation(state, user, &forwarded).await
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            save_standalone_annotation(state, user, dataset_id, &image_id, args).await
        }
    }
}

async fn set_verified_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    let verified = arg(args, "verified")
        .and_then(Value::as_bool)
        .ok_or_else(|| ApiError::bad_request("verified is required"))?;
    match resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::Edit).await? {
        WebDatasetTarget::Project { .. } => {
            let mut forwarded = args.clone();
            if let Some(object) = forwarded.as_object_mut() {
                object.insert("datasetId".to_string(), Value::String(dataset_value));
                object.insert(
                    "qaStatus".to_string(),
                    Value::String(if verified { "approved" } else { "pending" }.to_string()),
                );
            }
            update_image_state(state, user, &forwarded).await
        }
        WebDatasetTarget::Standalone { .. } => Err(ApiError::conflict(
            "DATASET_NOT_VERSIONED",
            "verification is only available after assigning the dataset to a data project",
            Value::Null,
        )),
    }
}

async fn project_args_from_dataset(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    let target =
        resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::View).await?;
    let WebDatasetTarget::Project { project_id, .. } = target else {
        return Err(ApiError::conflict(
            "DATASET_NOT_VERSIONED",
            "this command requires a dataset assigned to a data project",
            Value::Null,
        ));
    };
    let mut forwarded = args.clone();
    if let Some(object) = forwarded.as_object_mut() {
        object.insert(
            "dataProjectId".to_string(),
            Value::String(project_id.to_string()),
        );
        object.insert("datasetId".to_string(), Value::String(dataset_value));
    }
    Ok(forwarded)
}

async fn classes_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    match resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::View).await? {
        WebDatasetTarget::Project { .. } => {
            let mut forwarded = args.clone();
            if let Some(object) = forwarded.as_object_mut() {
                object.insert("datasetId".to_string(), Value::String(dataset_value));
            }
            get_classes(state, user, &forwarded).await
        }
        WebDatasetTarget::Standalone { dataset_id } => {
            let schema: Value = sqlx::query_scalar(
                "SELECT schema_json FROM datasets WHERE id = $1 AND deleted_at IS NULL",
            )
            .bind(dataset_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("dataset not found"))?;
            Ok(json!({
                "datasetId": dataset_id,
                "classes": schema_classes(&schema),
                "metadata": schema,
                "metadataHash": json_hash(&schema)?,
                "revision": 1
            }))
        }
    }
}

async fn save_classes_command(
    state: &AppState,
    user: &AuthUser,
    args: &Value,
) -> Result<Value, ApiError> {
    let dataset_value = dataset_value_from_command(args)?;
    let target =
        resolve_web_dataset_target(state, user.id, &dataset_value, WebPermission::Edit).await?;
    let WebDatasetTarget::Project { .. } = target else {
        return Err(ApiError::conflict(
            "DATASET_NOT_VERSIONED",
            "class edits are only available after assigning the dataset to a data project",
            Value::Null,
        ));
    };
    let mut forwarded = args.clone();
    if let Some(object) = forwarded.as_object_mut() {
        object.insert("datasetId".to_string(), Value::String(dataset_value));
    }
    save_classes(state, user, &forwarded).await
}

pub async fn command(
    State(state): State<AppState>,
    user: AuthUser,
    Path(command): Path<String>,
    Json(input): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let args = args_object(input)?;
    let name = command_name(&command);
    let data = match name.as_str() {
        "listdataprojects" => serialize(list_projects(&state, user.id).await?)?,
        "listdatasetprojects" => list_dataset_projects_command(&state, &user).await?,
        "createdataproject" => serialize(create_project(&state, &user, &args).await?)?,
        "getdataprojectdatasetgroups" => {
            get_data_project_dataset_groups(&state, &user, &args).await?
        }
        "setdataprojectdatasetgroups" => {
            set_data_project_dataset_groups(&state, &user, &args).await?
        }
        "getdataprojectdetail" | "getdataproject" => {
            let project_id = required_uuid(&args, "dataProjectId")?;
            project_detail(&state, &user, project_id).await?
        }
        "getprojectdetail" => project_detail_for_dataset(&state, &user, &args).await?,
        "assigndatasettodataproject" => assign_dataset(&state, &user, &args).await?,
        "listdataprojectdatasets" => {
            list_data_project_datasets_command(&state, &user, &args).await?
        }
        "listdatasets" => list_datasets(&state, user.id).await?,
        "createdataset" | "createdatasetproject" => create_dataset(&state, &user, &args).await?,
        "getdataprojectworkingtree" => {
            let project_id = required_uuid(&args, "dataProjectId")?;
            serialize(
                working_tree(&state, &user, project_id, optional_string(&args, "branch")?).await?,
            )?
        }
        "listdataprojectcommits" | "listdataprojectversioncommits" => {
            let project_id = required_uuid(&args, "dataProjectId")?;
            serialize(
                list_commits(&state, &user, project_id, optional_string(&args, "branch")?).await?,
            )?
        }
        "getdataprojectcommit" => serialize(get_commit(&state, &user, &args).await?)?,
        "listdataprojectbranches" => {
            let project_id = required_uuid(&args, "dataProjectId")?;
            serialize(list_branches(&state, &user, project_id).await?)?
        }
        "createdataprojectbranch" => serialize(create_branch(&state, &user, &args).await?)?,
        "switchdataprojectbranch" => switch_branch(&state, &user, &args).await?,
        "deletedataprojectbranch" => delete_branch(&state, &user, &args).await?,
        "commitdataproject" | "commitdataprojectbranch" => {
            serialize(commit_worktree(&state, &user, &args).await?)?
        }
        "previewdataprojectmerge" => serialize(preview_merge(&state, &user, &args).await?)?,
        "mergedataprojectbranches" => serialize(merge_branches(&state, &user, &args).await?)?,
        "resolvedataprojectmerge" => serialize(resolve_merge(&state, &user, &args).await?)?,
        "revertdataproject" | "revertdataprojectcommit" | "restoredataproject" => {
            serialize(restore_commit(&state, &user, &args).await?)?
        }
        "listdataprojectimages" | "listimages" => list_images(&state, &user, &args).await?,
        "listclasssamples" => list_class_samples(&state, &user, &args).await?,
        "listprojectimages" => list_project_images_command(&state, &user, &args).await?,
        "getdataprojectimage" | "getimage" => get_image(&state, &user, &args).await?,
        "getimageannotationstate" => annotation_state_command(&state, &user, &args).await?,
        "getimageannotations" => annotation_objects_command(&state, &user, &args).await?,
        "getdataprojectannotation" | "getannotation" => {
            get_annotation(&state, &user, &args).await?
        }
        "saveimageannotations" => save_annotation_command(&state, &user, &args).await?,
        "savedataprojectannotation" | "saveannotation" => {
            save_annotation(&state, &user, &args).await?
        }
        "getdataprojectclasses" => get_classes(&state, &user, &args).await?,
        "getclasses" => classes_command(&state, &user, &args).await?,
        "savedataprojectclasses" => save_classes(&state, &user, &args).await?,
        "saveclasses" => save_classes_command(&state, &user, &args).await?,
        "submitdataprojectannotation" | "submitdataprojectimage" | "submitimage" => {
            submit_image(&state, &user, &args).await?
        }
        "submitimageannotations" => {
            let forwarded = project_args_from_dataset(&state, &user, &args).await?;
            submit_image(&state, &user, &forwarded).await?
        }
        "setimageverified" => set_verified_command(&state, &user, &args).await?,
        "reviewdataprojectimage" | "reviewimage" => review_image(&state, &user, &args).await?,
        "listdataprojectreviewqueue" | "reviewqueue" => review_queue(&state, &user, &args).await?,
        "listreviewqueue" => {
            let forwarded = project_args_from_dataset(&state, &user, &args).await?;
            let queue = review_queue(&state, &user, &forwarded).await?;
            queue.get("items").cloned().unwrap_or_else(|| json!([]))
        }
        "reviewtaskitem" => {
            let forwarded = project_args_from_dataset(&state, &user, &args).await?;
            review_image(&state, &user, &forwarded).await?
        }
        "createdataprojecttask" | "createtask" => create_task(&state, &user, &args).await?,
        "createannotationtask" => {
            let forwarded = project_args_from_dataset(&state, &user, &args).await?;
            create_task(&state, &user, &forwarded).await?
        }
        "listdataprojecttasks" => list_tasks(&state, &user, &args).await?,
        "listtasks" => {
            let forwarded = project_args_from_dataset(&state, &user, &args).await?;
            list_tasks(&state, &user, &forwarded).await?
        }
        "exportdataproject" => export_project(&state, &user, &args).await?,
        "getdataprojectsyncstatus" => sync_status(&state, &user, &args).await?,
        "publishdataproject" | "syncdataproject" => publish_status(&state, &user, &args).await?,
        "listdataprojectcloudmembers" => list_members(&state, &user, &args).await?,
        "upsertdataprojectcloudmember" => upsert_member(&state, &user, &args).await?,
        "removedataprojectcloudmember" => remove_member(&state, &user, &args).await?,
        "discoverdataprojectremotes" => list_projects(&state, user.id)
            .await
            .map(|projects| json!(projects))?,
        "createremotedataproject" => serialize(create_project(&state, &user, &args).await?)?,
        // A browser has no native checkout to connect or clone.  Keep the
        // command names understood by the shared project UI, while making
        // their server-side meaning explicit: a remote project is already
        // the authoritative web project, so connecting returns its detail
        // and cloning returns the same project as the active web workspace.
        "connectremotedataproject" => {
            let project_id = required_uuid(&args, "remoteProjectId")
                .or_else(|_| required_uuid(&args, "dataProjectId"))?;
            project_detail(&state, &user, project_id).await?
        }
        "clonedataproject" | "cloneremotedataproject" => {
            let project_id = required_uuid(&args, "remoteProjectId")
                .or_else(|_| required_uuid(&args, "dataProjectId"))?;
            let project = project_detail(&state, &user, project_id).await?;
            let summary = project.get("project").unwrap_or(&project);
            let active_branch = project
                .get("activeBranch")
                .and_then(Value::as_str)
                .unwrap_or("main");
            let head = project
                .get("branches")
                .and_then(Value::as_array)
                .and_then(|branches| {
                    branches.iter().find(|branch| {
                        branch.get("name").and_then(Value::as_str) == Some(active_branch)
                    })
                })
                .and_then(|branch| branch.get("headCommitId"))
                .cloned()
                .unwrap_or(Value::Null);
            json!({
                "localProjectId": project_id,
                "remoteProjectId": project_id,
                "remoteRevision": summary.get("revision").cloned().unwrap_or(Value::Null),
                "localHead": head.clone(),
                "remoteHead": head,
                "outcome": "already-present",
                "trackingRef": Value::Null,
            })
        }
        _ => {
            return Err(ApiError::not_found(format!(
                "unsupported web data-project command: {command}"
            )))
        }
    };
    Ok(Json(json!({ "ok": true, "data": data })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(dataset_key: &str, image_id: &str, hash: &str, annotation: Value) -> WorkEntry {
        let status = status_for_annotation(&annotation);
        let qa_status = default_qa_status();
        WorkEntry {
            dataset_key: dataset_key.to_string(),
            dataset_id: None,
            image_id: image_id.to_string(),
            file_name: format!("{image_id}.png"),
            image_hash: hash.to_string(),
            image_byte_size: Some(4),
            image_mime_type: Some("image/png".to_string()),
            annotation_hash: annotation_hash(&annotation, &status, &qa_status).unwrap(),
            annotation,
            status,
            qa_status,
            split: "train".to_string(),
            width: 2,
            height: 2,
            review_note: None,
            tags: json!([]),
            source_item_id: None,
            revision: 1,
        }
    }

    #[test]
    fn status_bucket_does_not_treat_unreviewed_labels_as_approved() {
        assert_eq!(
            status_bucket("inProgress", "pending", &json!([{"label": "x"}])),
            "inProgress"
        );
        assert_eq!(
            status_bucket("inProgress", "approved", &json!([{"label": "x"}])),
            "approved"
        );
        assert_eq!(status_bucket("raw", "pending", &json!([])), "raw");
    }

    #[test]
    fn merge_branch_aliases_are_compatible_but_conflicts_are_rejected() {
        let args = json!({ "sourceBranch": "feature", "theirsBranch": "feature" });
        assert_eq!(
            merge_branch_value(&args, "sourceBranch", "theirsBranch").unwrap(),
            Some("feature".to_string())
        );

        let aliases = json!({ "oursBranch": "main", "theirsBranch": "feature" });
        assert_eq!(
            merge_branch_value(&aliases, "targetBranch", "oursBranch")
                .unwrap()
                .as_deref(),
            Some("main")
        );

        let conflicting = json!({ "sourceBranch": "feature", "theirsBranch": "other" });
        assert!(merge_branch_value(&conflicting, "sourceBranch", "theirsBranch").is_err());

        let resolution = json!({ "key": "entry:v1:aa:bb" });
        assert_eq!(
            required_string_alias(&resolution, "conflictKey", "key").unwrap(),
            "entry:v1:aa:bb"
        );
        let conflicting_resolution = json!({ "conflictKey": "one", "key": "two" });
        assert!(required_string_alias(&conflicting_resolution, "conflictKey", "key").is_err());
    }

    #[test]
    fn canonical_metadata_is_native_importable_and_preserves_owner_scope() {
        let metadata = canonical_metadata_value(
            &json!({ "classes": ["defect", { "id": 7, "label": "scratch", "color": "#f00" }] }),
            Some(Uuid::nil()),
            "road",
            "Road images",
            3,
            "https://example.test\nowner",
        );
        assert_eq!(
            metadata.get("accountScope").and_then(Value::as_str),
            Some("https://example.test\nowner")
        );
        let manifest = metadata.get("manifest").expect("manifest");
        assert_eq!(
            manifest.get("sourceDatasetKey").and_then(Value::as_str),
            Some("road")
        );
        assert_eq!(manifest.get("imageCount").and_then(Value::as_u64), Some(3));
        assert_eq!(manifest.get("classCount").and_then(Value::as_u64), Some(2));
        assert_eq!(metadata["classes"][0]["id"], json!(0));
        assert_eq!(metadata["classes"][1]["label"], json!("scratch"));
    }

    #[test]
    fn empty_unassigned_worktree_is_not_restored_from_old_base() {
        assert!(should_restore_empty_worktree_from_base(
            0,
            0,
            Some("initial"),
            1
        ));
        assert!(!should_restore_empty_worktree_from_base(
            0,
            0,
            Some("initial"),
            2
        ));
        assert!(!should_restore_empty_worktree_from_base(0, 0, None, 1));
    }

    #[test]
    fn dataset_membership_population_advances_worktree_revision() {
        assert_eq!(next_worktree_revision(1).unwrap(), 2);
        assert_eq!(next_worktree_revision(41).unwrap(), 42);
        assert!(next_worktree_revision(i64::MAX).is_err());
    }

    #[test]
    fn stale_merge_reservation_is_reclaimable_without_losing_resolutions() {
        let now = Utc::now();
        let mut stale = json!({
            "entry:image": "ours",
            "__resolving": true,
            "__resolvingAt": (now - Duration::seconds(MERGE_RESERVATION_TTL_SECONDS + 1)).to_rfc3339()
        });
        assert!(merge_reservation_is_stale(&stale, now));
        clear_merge_resolving_fields(&mut stale).unwrap();
        assert_eq!(
            stale.get("entry:image").and_then(Value::as_str),
            Some("ours")
        );
        assert!(stale.get("__resolving").is_none());
        assert!(stale.get("__resolvingAt").is_none());

        let fresh = json!({
            "__resolving": true,
            "__resolvingAt": (now - Duration::seconds(MERGE_RESERVATION_TTL_SECONDS - 1)).to_rfc3339()
        });
        assert!(!merge_reservation_is_stale(&fresh, now));
        assert!(merge_reservation_is_stale(
            &json!({ "__resolving": true }),
            now
        ));
    }

    #[test]
    fn entry_conflict_key_is_json_safe_and_round_trips_utf8_ids() {
        let dataset_key = "道路:检测/夜间";
        let image_id = "frame:001/相机";
        let key = entry_conflict_key(dataset_key, image_id);
        assert!(!key.contains('\0'));
        let encoded = serde_json::to_string(&json!({ "key": key })).unwrap();
        assert!(!encoded.contains("\\u0000"));
        assert_eq!(
            decode_entry_conflict_key(&key).unwrap(),
            (dataset_key.to_string(), image_id.to_string())
        );
    }

    #[test]
    fn diff_captures_membership_image_annotation_and_review_changes() {
        let before = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![MetadataRow {
                dataset_key: "d".to_string(),
                dataset_id: None,
                metadata_hash: None,
                metadata: json!({ "classes": ["old"] }),
            }],
            entries: vec![entry("d", "i", &"a".repeat(64), json!([]))],
        };
        let mut changed = entry("d", "i", &"b".repeat(64), json!([{"label": "new"}]));
        changed.qa_status = "approved".to_string();
        let after = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![MetadataRow {
                dataset_key: "d".to_string(),
                dataset_id: None,
                metadata_hash: None,
                metadata: json!({ "classes": ["new"] }),
            }],
            entries: vec![changed, entry("d2", "new", &"c".repeat(64), json!([]))],
        };
        let changes = diff_trees(&before, &after);
        assert!(changes
            .iter()
            .any(|change| change.kind == "dataset-changed"));
        assert!(changes
            .iter()
            .any(|change| change.kind == "annotation-changed"));
        assert!(changes.iter().any(|change| change.kind == "image-added"));
    }

    #[test]
    fn three_way_merge_preserves_real_conflicts() {
        let base = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![],
            entries: vec![entry("d", "i", &"a".repeat(64), json!([]))],
        };
        let ours = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![],
            entries: vec![entry("d", "i", &"b".repeat(64), json!([{"label": "ours"}]))],
        };
        let theirs = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![],
            entries: vec![entry(
                "d",
                "i",
                &"c".repeat(64),
                json!([{"label": "theirs"}]),
            )],
        };
        let (_, conflicts) = merge_trees(&base, &ours, &theirs).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].key.contains("entry:"));
        assert!(conflicts[0].ours.is_some());
        assert!(conflicts[0].theirs.is_some());
    }

    #[test]
    fn three_way_merge_merges_dataset_groups_and_resolves_name_conflict() {
        let metadata = |key: &str| MetadataRow {
            dataset_key: key.to_string(),
            dataset_id: None,
            metadata_hash: None,
            metadata: json!({ "classes": [] }),
        };
        let group = |name: &str, dataset_keys: &[&str]| DatasetGroup {
            id: "group-1".to_string(),
            name: name.to_string(),
            dataset_keys: dataset_keys.iter().map(|key| (*key).to_string()).collect(),
            position: 0,
        };
        let base = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![group("Base", &["road"])],
            },
            metadata: vec![metadata("road"), metadata("night")],
            entries: vec![],
        };
        let ours = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![group("Ours", &["road", "night"])],
            },
            metadata: base.metadata.clone(),
            entries: vec![],
        };
        let theirs = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![group("Theirs", &["road"])],
            },
            metadata: base.metadata.clone(),
            entries: vec![],
        };
        let (mut merged, conflicts) = merge_trees(&base, &ours, &theirs).unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, "dataset-group-name");
        assert_eq!(merged.project_metadata.dataset_groups[0].name, "Ours");
        assert_eq!(
            merged.project_metadata.dataset_groups[0].dataset_keys,
            vec!["road", "night"]
        );
        set_tree_conflict_value(&mut merged, &conflicts[0], "theirs").unwrap();
        assert_eq!(merged.project_metadata.dataset_groups[0].name, "Theirs");
    }

    #[test]
    fn three_way_merge_exposes_cross_group_assignment_and_resolves_atomically() {
        let metadata = MetadataRow {
            dataset_key: "road".to_string(),
            dataset_id: None,
            metadata_hash: None,
            metadata: json!({ "classes": [] }),
        };
        let group = |id: &str, name: &str, dataset_keys: &[&str]| DatasetGroup {
            id: id.to_string(),
            name: name.to_string(),
            dataset_keys: dataset_keys.iter().map(|key| (*key).to_string()).collect(),
            position: 0,
        };
        let base = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![group("source", "Source", &["road"])],
            },
            metadata: vec![metadata.clone()],
            entries: vec![],
        };
        let ours = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![
                    group("source", "Source", &[]),
                    group("target", "Target", &["road"]),
                ],
            },
            metadata: vec![metadata.clone()],
            entries: vec![],
        };
        let theirs = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![
                    group("source", "Source", &[]),
                    group("incoming", "Incoming", &["road"]),
                ],
            },
            metadata: vec![metadata],
            entries: vec![],
        };
        let (mut merged, conflicts) = merge_trees(&base, &ours, &theirs).unwrap();
        let conflict = conflicts
            .iter()
            .find(|conflict| conflict.kind == "dataset-group-membership")
            .expect("cross-group assignment conflict");
        assert_eq!(
            conflict.key,
            group_conflict_key("target", "member", Some("road"))
        );
        assert_eq!(
            merged
                .project_metadata
                .dataset_groups
                .iter()
                .filter(|group| group.dataset_keys.iter().any(|key| key == "road"))
                .count(),
            2
        );
        set_tree_conflict_value_with_context(
            &mut merged,
            conflict,
            "theirs",
            Some(&base.project_metadata),
            Some(&ours.project_metadata),
            Some(&theirs.project_metadata),
        )
        .unwrap();
        assert_eq!(
            merged
                .project_metadata
                .dataset_groups
                .iter()
                .filter(|group| group.dataset_keys.iter().any(|key| key == "road"))
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>(),
            vec!["incoming"]
        );
    }

    #[test]
    fn merge_candidate_parser_preserves_provisional_duplicate_membership() {
        let value = json!({
            "projectMetadata": {
                "schemaVersion": 1,
                "datasetGroups": [
                    {"id": "ours", "name": "Ours", "datasetKeys": ["road"]},
                    {"id": "theirs", "name": "Theirs", "datasetKeys": ["road"]}
                ]
            },
            "datasets": [{"datasetKey": "road"}],
            "entries": []
        });
        let tree = tree_document_from_merge_candidate(&value).expect("merge candidate");
        assert_eq!(
            tree.project_metadata
                .dataset_groups
                .iter()
                .filter(|group| group.dataset_keys.iter().any(|key| key == "road"))
                .count(),
            2
        );
        assert!(tree_document_from_json(&value).is_err());
    }

    #[test]
    fn dataset_group_conflict_keys_are_json_safe() {
        let key = group_conflict_key("夜间:道路", "member", Some("dataset/一"));
        assert!(!key.contains('\0'));
        let encoded = serde_json::to_string(&json!({ "key": key })).unwrap();
        assert!(!encoded.contains("\\u0000"));
        assert_eq!(
            decode_group_conflict_key(&key).unwrap(),
            (
                "夜间:道路".to_string(),
                "member".to_string(),
                Some("dataset/一".to_string())
            )
        );
    }

    #[test]
    fn three_way_merge_keeps_target_group_deletion_when_source_is_unchanged() {
        let metadata = MetadataRow {
            dataset_key: "road".to_string(),
            dataset_id: None,
            metadata_hash: None,
            metadata: json!({ "classes": [] }),
        };
        let base = TreeDocument {
            project_metadata: ProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![DatasetGroup {
                    id: "group-1".to_string(),
                    name: "Road".to_string(),
                    dataset_keys: vec!["road".to_string()],
                    position: 0,
                }],
            },
            metadata: vec![metadata.clone()],
            entries: vec![],
        };
        let ours = TreeDocument {
            project_metadata: ProjectMetadata::default(),
            metadata: vec![metadata.clone()],
            entries: vec![],
        };
        let (merged, conflicts) = merge_trees(&base, &ours, &base).unwrap();
        assert!(conflicts.is_empty());
        assert!(merged.project_metadata.dataset_groups.is_empty());
    }

    #[test]
    fn native_graph_hash_tree_remains_desktop_compatible() {
        let metadata_hash = "a".repeat(64);
        let annotation_hash = "b".repeat(64);
        let value = json!({
            "datasetIds": ["road"],
            "datasetMetadata": { "road": metadata_hash.clone() },
            "entries": [{
                "datasetId": "road",
                "imageId": "image-1",
                "fileName": "road.png",
                "imageHash": "c".repeat(64),
                "annotationHash": annotation_hash.clone(),
                "status": "inProgress",
                "qaStatus": "pending",
                "split": "train",
                "width": 10,
                "height": 10
            }]
        });
        let tree = tree_document_from_json(&value).expect("parse native graph tree");
        assert_eq!(tree.metadata.len(), 1);
        assert_eq!(tree.metadata[0].metadata_hash, Some("a".repeat(64)));
        assert_eq!(
            tree.metadata[0]
                .metadata
                .get("__webMetadataHash")
                .and_then(Value::as_str),
            Some(metadata_hash.as_str())
        );
        assert_eq!(
            tree.entries[0]
                .annotation
                .get("__webAnnotationHash")
                .and_then(Value::as_str),
            Some(annotation_hash.as_str())
        );
        let encoded = tree_document_json(&tree);
        assert_eq!(
            encoded
                .get("datasetMetadata")
                .and_then(|value| value.get("road"))
                .and_then(Value::as_str),
            Some(metadata_hash.as_str())
        );
        assert_eq!(
            encoded
                .get("entries")
                .and_then(Value::as_array)
                .and_then(|entries| entries.first())
                .and_then(|entry| entry.get("datasetId"))
                .and_then(Value::as_str),
            Some("road")
        );
    }

    #[test]
    fn virtual_dataset_listing_uses_manifest_identity_and_format() {
        let metadata = json!({
            "manifest": {
                "name": "夜间缺陷",
                "description": "来自桌面项目的不可变快照",
                "format": "yolo-seg",
                "sourceDatasetKey": "night"
            },
            "classes": [{ "id": 0, "label": "裂纹" }]
        });
        assert_eq!(metadata_display_name(&metadata, "fallback"), "夜间缺陷");
        assert_eq!(
            metadata_display_description(&metadata, metadata_format(&metadata).as_deref()),
            "来自桌面项目的不可变快照"
        );
        assert_eq!(metadata_format(&metadata).as_deref(), Some("yolo-seg"));
        assert_eq!(
            annotation_types_for_schema(&metadata),
            vec!["Polygon".to_string(), "BBox".to_string()]
        );
        assert_eq!(schema_classes(&metadata).as_array().map(Vec::len), Some(1));
        assert_eq!(
            browser_dataset_id(Uuid::nil(), "night"),
            "00000000-0000-0000-0000-000000000000::night"
        );
    }
}

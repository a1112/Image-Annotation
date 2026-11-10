//! Cloud repository for data projects.
//!
//! A data project is intentionally separate from the legacy collaboration
//! `projects` table.  Its membership, immutable commit graph, branch refs,
//! and content-addressed objects are all protected by the data-project ACL.
//! Dataset ACLs are not used as a shortcut for project history access: a
//! project clone must be able to retain the complete dataset membership and
//! immutable history it was granted.

use super::ApiError;
use crate::{
    auth::AuthUser,
    routes::data_project_metadata::{self, ProjectMetadata},
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, Postgres, Row, Transaction};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::PathBuf,
};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;
use uuid::Uuid;

const MAX_OBJECT_BYTES: i64 = 10 * 1024 * 1024 * 1024;
const MAX_GRAPH_COMMITS: usize = 100_000;
const MAX_GRAPH_OBJECTS: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DataProjectPermission {
    View,
    Edit,
    ManageMembers,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDataProject {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub organization_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDataProject {
    pub name: Option<String>,
    pub description: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectMemberInput {
    pub user_id: Uuid,
    pub role: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DatasetMembershipInput {
    pub dataset_key: String,
    pub dataset_id: Option<Uuid>,
    #[serde(default)]
    pub metadata: Value,
    #[serde(default)]
    pub position: i32,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct BlobDescriptor {
    pub hash: String,
    pub byte_size: i64,
    pub kind: String,
    #[serde(default = "default_mime_type")]
    pub mime_type: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CommitInput {
    pub commit_id: String,
    #[serde(default)]
    pub parent_ids: Vec<String>,
    pub message: String,
    pub tree: Value,
    #[serde(default)]
    pub metadata: Value,
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct RefUpdate {
    pub name: String,
    pub expected_commit_id: Option<String>,
    pub commit_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushRequest {
    pub expected_remote_revision: i64,
    #[serde(default)]
    pub datasets: Option<Vec<DatasetMembershipInput>>,
    #[serde(default)]
    pub commits: Vec<CommitInput>,
    #[serde(default)]
    pub refs: Vec<RefUpdate>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct DataProjectSummary {
    id: Uuid,
    organization_id: Option<Uuid>,
    owner_id: Uuid,
    name: String,
    description: String,
    status: String,
    revision: i64,
    role: String,
    dataset_count: i64,
    ref_count: i64,
    commit_count: i64,
    updated_at: DateTime<Utc>,
}

fn default_mime_type() -> String {
    "application/octet-stream".to_string()
}

fn validate_name(value: &str, field: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 240 {
        return Err(ApiError::bad_request(format!(
            "{field} is required and must be at most 240 characters"
        )));
    }
    Ok(value.to_string())
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_ref_name(value: &str) -> Result<String, ApiError> {
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
        return Err(ApiError::bad_request("invalid data-project ref name"));
    }
    Ok(value.to_string())
}

fn role_allows(role: &str, permission: DataProjectPermission) -> bool {
    match permission {
        DataProjectPermission::View => matches!(role, "owner" | "admin" | "editor" | "viewer"),
        DataProjectPermission::Edit => matches!(role, "owner" | "admin" | "editor"),
        DataProjectPermission::ManageMembers => matches!(role, "owner" | "admin"),
    }
}

/// A legacy data project may have a NULL `organization_id`.  In that case its
/// owner still defines the organization boundary when the owner belongs to an
/// organization.  Treating NULL as "any organization" would let a member from
/// one tenant add or read a project that was created by another tenant.
fn project_organization_scope(
    project_organization: Option<Uuid>,
    owner_organization: Option<Uuid>,
) -> Option<Uuid> {
    project_organization.or(owner_organization)
}

fn organization_scope_matches(
    project_organization: Option<Uuid>,
    owner_organization: Option<Uuid>,
    member_organization: Option<Uuid>,
) -> bool {
    project_organization_scope(project_organization, owner_organization) == member_organization
}

async fn require_data_project_permission(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    permission: DataProjectPermission,
) -> Result<String, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT dm.role
        FROM data_project_members dm
        JOIN data_projects p ON p.id = dm.project_id
        JOIN users u ON u.id = dm.user_id
        JOIN users owner ON owner.id = p.owner_id
        WHERE dm.project_id = $1
          AND dm.user_id = $2
          AND dm.deleted_at IS NULL
          AND p.deleted_at IS NULL
          AND u.deleted_at IS NULL
          AND COALESCE(p.organization_id, owner.organization_id)
              IS NOT DISTINCT FROM u.organization_id
        "#,
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
            "role {role} lacks data-project permission"
        )))
    }
}

async fn project_row(state: &AppState, project_id: Uuid) -> Result<PgRow, ApiError> {
    sqlx::query(
        "SELECT id, organization_id, owner_id, name, description, status, revision, updated_at FROM data_projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))
}

async fn summary(
    state: &AppState,
    project_id: Uuid,
    role: &str,
) -> Result<DataProjectSummary, ApiError> {
    let row = project_row(state, project_id).await?;
    let counts = sqlx::query(
        r#"
        SELECT
            (SELECT COUNT(*) FROM data_project_datasets WHERE project_id = $1) AS dataset_count,
            (SELECT COUNT(*) FROM data_project_refs WHERE project_id = $1) AS ref_count,
            (SELECT COUNT(*) FROM data_project_commits WHERE project_id = $1) AS commit_count
        "#,
    )
    .bind(project_id)
    .fetch_one(&state.pool)
    .await?;
    Ok(DataProjectSummary {
        id: row.get("id"),
        organization_id: row.get("organization_id"),
        owner_id: row.get("owner_id"),
        name: row.get("name"),
        description: row.get("description"),
        status: row.get("status"),
        revision: row.get("revision"),
        role: role.to_string(),
        dataset_count: counts.get("dataset_count"),
        ref_count: counts.get("ref_count"),
        commit_count: counts.get("commit_count"),
        updated_at: row.get("updated_at"),
    })
}

pub async fn list(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT p.id, p.organization_id, p.owner_id, p.name, p.description,
            p.status, p.revision, p.updated_at, dm.role,
            (SELECT COUNT(*) FROM data_project_datasets dpm WHERE dpm.project_id = p.id) AS dataset_count,
            (SELECT COUNT(*) FROM data_project_refs dpr WHERE dpr.project_id = p.id) AS ref_count,
            (SELECT COUNT(*) FROM data_project_commits dpc WHERE dpc.project_id = p.id) AS commit_count
        FROM data_projects p
        JOIN data_project_members dm ON dm.project_id = p.id
        JOIN users u ON u.id = dm.user_id
        JOIN users owner ON owner.id = p.owner_id
        WHERE dm.user_id = $1
          AND dm.deleted_at IS NULL
          AND p.deleted_at IS NULL
          AND u.deleted_at IS NULL
          AND COALESCE(p.organization_id, owner.organization_id)
              IS NOT DISTINCT FROM u.organization_id
        ORDER BY p.updated_at DESC, p.id
        "#,
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "id": row.get::<Uuid, _>("id"),
                    "organizationId": row.get::<Option<Uuid>, _>("organization_id"),
                    "ownerId": row.get::<Uuid, _>("owner_id"),
                    "name": row.get::<String, _>("name"),
                    "description": row.get::<String, _>("description"),
                    "status": row.get::<String, _>("status"),
                    "revision": row.get::<i64, _>("revision"),
                    "role": row.get::<String, _>("role"),
                    "datasetCount": row.get::<i64, _>("dataset_count"),
                    "refCount": row.get::<i64, _>("ref_count"),
                    "commitCount": row.get::<i64, _>("commit_count"),
                    "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
                })
            })
            .collect(),
    )))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<CreateDataProject>,
) -> Result<Json<Value>, ApiError> {
    let name = validate_name(&input.name, "data project name")?;
    if input.description.len() > 10_000 {
        return Err(ApiError::bad_request(
            "data project description is too long",
        ));
    }
    if input.organization_id.is_some_and(|organization_id| {
        user.organization_id
            .is_some_and(|user_organization| user_organization != organization_id)
            || user.organization_id.is_none()
    }) {
        return Err(ApiError::forbidden(
            "project organization must match the signed-in user",
        ));
    }
    let project_id = Uuid::new_v4();
    let organization_id = input.organization_id.or(user.organization_id);
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO data_projects (id, organization_id, owner_id, name, description, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $3, $3)",
    )
    .bind(project_id)
    .bind(organization_id)
    .bind(user.id)
    .bind(name)
    .bind(input.description.trim())
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO data_project_members (project_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let result = summary(&state, project_id, "owner").await?;
    Ok(Json(serde_json::to_value(result).map_err(|error| {
        ApiError::internal_message(format!("encode data project: {error}"))
    })?))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_data_project_permission(&state, user.id, project_id, DataProjectPermission::View)
            .await?;
    let result = summary(&state, project_id, &role).await?;
    Ok(Json(serde_json::to_value(result).map_err(|error| {
        ApiError::internal_message(format!("encode data project: {error}"))
    })?))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<UpdateDataProject>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_data_project_permission(&state, user.id, project_id, DataProjectPermission::Edit)
            .await?;
    if input.name.is_none() && input.description.is_none() && input.status.is_none() {
        return Err(ApiError::bad_request("data project update is empty"));
    }
    let name = input
        .name
        .as_deref()
        .map(|value| validate_name(value, "data project name"))
        .transpose()?;
    if input
        .description
        .as_deref()
        .is_some_and(|value| value.len() > 10_000)
    {
        return Err(ApiError::bad_request(
            "data project description is too long",
        ));
    }
    if input
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "active" | "archived"))
    {
        return Err(ApiError::bad_request("invalid data project status"));
    }
    sqlx::query(
        "UPDATE data_projects SET name = COALESCE($2, name), description = COALESCE($3, description), status = COALESCE($4, status), revision = revision + 1, updated_by = $5, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(name)
    .bind(input.description.as_deref())
    .bind(input.status.as_deref())
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    let result = summary(&state, project_id, &role).await?;
    Ok(Json(serde_json::to_value(result).map_err(|error| {
        ApiError::internal_message(format!("encode data project: {error}"))
    })?))
}

/// Tombstone a data project without removing its immutable graph or objects.
///
/// The project-level repository is intentionally recoverable at the storage
/// layer: deletion only hides the project from ACL lookups and list/graph
/// reads.  Keeping the rows and blobs means a later restore/migration can
/// preserve the commit DAG and content-addressed history.
pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_data_project_permission(
        &state,
        user.id,
        project_id,
        DataProjectPermission::ManageMembers,
    )
    .await?;
    let revision = sqlx::query_scalar::<_, i64>(
        "UPDATE data_projects SET status = 'deleted', deleted_at = now(), revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 AND deleted_at IS NULL RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    Ok(Json(json!({
        "projectId": project_id,
        "status": "deleted",
        "deleted": true,
        "revision": revision
    })))
}

pub async fn members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_data_project_permission(&state, user.id, project_id, DataProjectPermission::View)
        .await?;
    let rows = sqlx::query(
        "SELECT m.user_id, m.role, m.joined_at, u.email, u.display_name FROM data_project_members m JOIN users u ON u.id = m.user_id JOIN data_projects p ON p.id = m.project_id JOIN users owner ON owner.id = p.owner_id WHERE m.project_id = $1 AND m.deleted_at IS NULL AND u.deleted_at IS NULL AND p.deleted_at IS NULL AND COALESCE(p.organization_id, owner.organization_id) IS NOT DISTINCT FROM u.organization_id ORDER BY m.joined_at, m.user_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "userId": row.get::<Uuid, _>("user_id"),
                    "role": row.get::<String, _>("role"),
                    "email": row.get::<String, _>("email"),
                    "displayName": row.get::<String, _>("display_name"),
                    "joinedAt": row.get::<DateTime<Utc>, _>("joined_at")
                })
            })
            .collect(),
    )))
}

pub async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<DataProjectMemberInput>,
) -> Result<Json<Value>, ApiError> {
    if !matches!(input.role.as_str(), "owner" | "admin" | "editor" | "viewer") {
        return Err(ApiError::bad_request("invalid data-project role"));
    }
    // Keep the project lock for the complete role transition.  Without this,
    // two concurrent membership updates could both observe the last owner and
    // demote/remove it, or add a member after the project was deleted.
    let mut tx = state.pool.begin().await?;
    let project = sqlx::query(
        "SELECT p.organization_id AS project_organization, owner.organization_id AS owner_organization FROM data_projects p JOIN users owner ON owner.id = p.owner_id WHERE p.id = $1 AND p.deleted_at IS NULL FOR UPDATE OF p",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let project_org: Option<Uuid> = project.get("project_organization");
    let owner_org: Option<Uuid> = project.get("owner_organization");

    let actor_role = sqlx::query(
        "SELECT m.role FROM data_project_members m JOIN users member ON member.id = m.user_id WHERE m.project_id = $1 AND m.user_id = $2 AND m.deleted_at IS NULL AND member.deleted_at IS NULL AND COALESCE($3::UUID, $4::UUID) IS NOT DISTINCT FROM member.organization_id",
    )
    .bind(project_id)
    .bind(user.id)
    .bind(project_org)
    .bind(owner_org)
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.get::<String, _>("role"))
    .ok_or_else(|| ApiError::forbidden("data-project membership is required"))?;
    if !role_allows(&actor_role, DataProjectPermission::ManageMembers) {
        return Err(ApiError::forbidden(format!(
            "role {actor_role} lacks data-project permission"
        )));
    }
    if input.role == "owner" && actor_role != "owner" {
        return Err(ApiError::forbidden(
            "only an owner can grant the owner role",
        ));
    }

    let target = sqlx::query(
        "SELECT organization_id FROM users WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(input.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("user not found"))?;
    let user_org: Option<Uuid> = target.get("organization_id");
    if !organization_scope_matches(project_org, owner_org, user_org) {
        return Err(ApiError::forbidden(
            "data-project members must belong to the project organization",
        ));
    }

    let previous_role = sqlx::query(
        "SELECT role FROM data_project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(input.user_id)
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.get::<String, _>("role"));
    if previous_role.as_deref() == Some("owner") {
        if actor_role != "owner" {
            return Err(ApiError::forbidden(
                "only an owner can change another owner's role",
            ));
        }
        if input.role != "owner" {
            let owners: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM data_project_members WHERE project_id = $1 AND role = 'owner' AND deleted_at IS NULL",
            )
            .bind(project_id)
            .fetch_one(&mut *tx)
            .await?;
            if owners <= 1 {
                return Err(ApiError::bad_request("a data project must retain an owner"));
            }
        }
    }
    sqlx::query(
        "INSERT INTO data_project_members (project_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT(project_id, user_id) DO UPDATE SET role = excluded.role, deleted_at = NULL",
    )
    .bind(project_id)
    .bind(input.user_id)
    .bind(&input.role)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE data_projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1")
        .bind(project_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "userId": input.user_id, "role": input.role })))
}

pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, member_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let mut tx = state.pool.begin().await?;
    let project = sqlx::query(
        "SELECT p.organization_id AS project_organization, owner.organization_id AS owner_organization FROM data_projects p JOIN users owner ON owner.id = p.owner_id WHERE p.id = $1 AND p.deleted_at IS NULL FOR UPDATE OF p",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let project_org: Option<Uuid> = project.get("project_organization");
    let owner_org: Option<Uuid> = project.get("owner_organization");
    let actor_role = sqlx::query(
        "SELECT m.role FROM data_project_members m JOIN users member ON member.id = m.user_id WHERE m.project_id = $1 AND m.user_id = $2 AND m.deleted_at IS NULL AND member.deleted_at IS NULL AND COALESCE($3::UUID, $4::UUID) IS NOT DISTINCT FROM member.organization_id",
    )
    .bind(project_id)
    .bind(user.id)
    .bind(project_org)
    .bind(owner_org)
    .fetch_optional(&mut *tx)
    .await?
    .map(|row| row.get::<String, _>("role"))
    .ok_or_else(|| ApiError::forbidden("data-project membership is required"))?;
    if !role_allows(&actor_role, DataProjectPermission::ManageMembers) {
        return Err(ApiError::forbidden(format!(
            "role {actor_role} lacks data-project permission"
        )));
    }
    let target = sqlx::query(
        "SELECT role FROM data_project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(member_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project member not found"))?;
    let target_role: String = target.get("role");
    if target_role == "owner" && actor_role != "owner" {
        return Err(ApiError::forbidden(
            "only an owner can remove another owner",
        ));
    }
    if target_role == "owner" {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM data_project_members WHERE project_id = $1 AND role = 'owner' AND deleted_at IS NULL",
        )
        .bind(project_id)
        .fetch_one(&mut *tx)
        .await?;
        if count <= 1 {
            return Err(ApiError::bad_request("a data project must retain an owner"));
        }
    }
    sqlx::query("UPDATE data_project_members SET deleted_at = now() WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL")
        .bind(project_id)
        .bind(member_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE data_projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1")
        .bind(project_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "userId": member_id, "deleted": true })))
}

pub async fn graph(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_data_project_permission(&state, user.id, project_id, DataProjectPermission::View)
            .await?;
    let project = summary(&state, project_id, &role).await?;
    let member_rows = sqlx::query(
        "SELECT m.user_id, m.role, m.joined_at FROM data_project_members m JOIN users u ON u.id = m.user_id JOIN data_projects p ON p.id = m.project_id JOIN users owner ON owner.id = p.owner_id WHERE m.project_id = $1 AND m.deleted_at IS NULL AND u.deleted_at IS NULL AND p.deleted_at IS NULL AND COALESCE(p.organization_id, owner.organization_id) IS NOT DISTINCT FROM u.organization_id ORDER BY m.joined_at, m.user_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let dataset_rows = sqlx::query(
        "SELECT dataset_key, dataset_id, metadata_json, position, updated_at FROM data_project_datasets WHERE project_id = $1 ORDER BY position, dataset_key",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let commit_rows = sqlx::query(
        "SELECT commit_id, parent_ids_json, message, tree_json, metadata_json, created_by, created_at FROM data_project_commits WHERE project_id = $1 ORDER BY created_at, commit_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let ref_rows = sqlx::query(
        "SELECT ref_name, commit_id, revision, updated_by, updated_at FROM data_project_refs WHERE project_id = $1 ORDER BY ref_name",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let object_rows = sqlx::query(
        "SELECT content_hash, byte_size, kind, mime_type, created_at FROM data_project_blobs WHERE project_id = $1 ORDER BY content_hash, byte_size",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    if commit_rows.len() > MAX_GRAPH_COMMITS {
        return Err(ApiError::bad_request(
            "data-project commit graph is too large",
        ));
    }
    if object_rows.len() > MAX_GRAPH_OBJECTS {
        return Err(ApiError::bad_request(
            "data-project object graph is too large",
        ));
    }
    Ok(Json(json!({
        "project": project,
        "members": member_rows.iter().map(|row| json!({
            "userId": row.get::<Uuid, _>("user_id"),
            "role": row.get::<String, _>("role"),
            "joinedAt": row.get::<DateTime<Utc>, _>("joined_at")
        })).collect::<Vec<_>>(),
        "datasets": dataset_rows.iter().map(|row| json!({
            "datasetKey": row.get::<String, _>("dataset_key"),
            "datasetId": row.get::<Option<Uuid>, _>("dataset_id"),
            "metadata": row.get::<Value, _>("metadata_json"),
            "position": row.get::<i32, _>("position"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "commits": commit_rows.iter().map(|row| json!({
            "commitId": row.get::<String, _>("commit_id"),
            "parentIds": row.get::<Value, _>("parent_ids_json"),
            "message": row.get::<String, _>("message"),
            "tree": row.get::<Value, _>("tree_json"),
            "metadata": row.get::<Value, _>("metadata_json"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "createdAt": row.get::<DateTime<Utc>, _>("created_at")
        })).collect::<Vec<_>>(),
        "refs": ref_rows.iter().map(|row| json!({
            "name": row.get::<String, _>("ref_name"),
            "commitId": row.get::<Option<String>, _>("commit_id"),
            "revision": row.get::<i64, _>("revision"),
            "updatedBy": row.get::<Uuid, _>("updated_by"),
            "updatedAt": row.get::<DateTime<Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "objects": object_rows.iter().map(|row| json!({
            "hash": row.get::<String, _>("content_hash"),
            "byteSize": row.get::<i64, _>("byte_size"),
            "kind": row.get::<String, _>("kind"),
            "mimeType": row.get::<String, _>("mime_type"),
            "createdAt": row.get::<DateTime<Utc>, _>("created_at")
        })).collect::<Vec<_>>(),
        "remoteRevision": project.revision
    })))
}

pub async fn status(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_data_project_permission(&state, user.id, project_id, DataProjectPermission::View)
            .await?;
    let project = summary(&state, project_id, &role).await?;
    let refs = sqlx::query("SELECT ref_name, commit_id, revision FROM data_project_refs WHERE project_id = $1 ORDER BY ref_name")
        .bind(project_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(json!({
        "project": project,
        "remoteRevision": project.revision,
        "refs": refs.iter().map(|row| json!({
            "name": row.get::<String, _>("ref_name"),
            "commitId": row.get::<Option<String>, _>("commit_id"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>()
    })))
}

fn collect_hashes(value: &Value, hashes: &mut BTreeSet<String>) -> Result<(), ApiError> {
    match value {
        Value::Array(items) => {
            for item in items {
                collect_hashes(item, hashes)?;
            }
        }
        Value::Object(object) => {
            for (key, item) in object {
                let key = key.to_ascii_lowercase();
                let add_hash = |hash: &str, hashes: &mut BTreeSet<String>| {
                    if !valid_hash(hash) {
                        return Err(ApiError::bad_request(
                            "commit tree contains an invalid object hash",
                        ));
                    }
                    hashes.insert(hash.to_ascii_lowercase());
                    Ok(())
                };
                if key.ends_with("hash") {
                    let hash = item.as_str().ok_or_else(|| {
                        ApiError::bad_request("commit tree hash fields must contain strings")
                    })?;
                    add_hash(hash, hashes)?;
                } else if key.ends_with("hashes") {
                    match item {
                        Value::String(hash) => add_hash(hash, hashes)?,
                        Value::Array(values) => {
                            for value in values {
                                let hash = value.as_str().ok_or_else(|| {
                                    ApiError::bad_request(
                                        "commit tree hash list must contain strings",
                                    )
                                })?;
                                add_hash(hash, hashes)?;
                            }
                        }
                        Value::Object(values) => {
                            for value in values.values() {
                                let hash = value.as_str().ok_or_else(|| {
                                    ApiError::bad_request(
                                        "commit tree hash map must contain strings",
                                    )
                                })?;
                                add_hash(hash, hashes)?;
                            }
                        }
                        _ => {
                            return Err(ApiError::bad_request(
                                "commit tree hash collection has an invalid shape",
                            ));
                        }
                    }
                }
                collect_hashes(item, hashes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_commit_input(commit: &CommitInput) -> Result<BTreeSet<String>, ApiError> {
    if commit.commit_id.trim().is_empty() || commit.commit_id.len() > 256 {
        return Err(ApiError::bad_request(
            "commitId is required and must be at most 256 characters",
        ));
    }
    if commit.message.trim().is_empty() || commit.message.len() > 2_000 {
        return Err(ApiError::bad_request(
            "commit message is required and must be at most 2000 characters",
        ));
    }
    if commit
        .parent_ids
        .iter()
        .any(|parent| parent == &commit.commit_id)
    {
        return Err(ApiError::bad_request("a commit cannot parent itself"));
    }
    if commit.tree.is_null() || !commit.tree.is_object() {
        return Err(ApiError::bad_request("commit tree must be an object"));
    }
    if !commit.metadata.is_object() {
        return Err(ApiError::bad_request("commit metadata must be an object"));
    }
    let mut hashes = BTreeSet::new();
    collect_hashes(&commit.tree, &mut hashes)?;
    collect_hashes(&commit.metadata, &mut hashes)?;
    if hashes.len() > MAX_GRAPH_OBJECTS {
        return Err(ApiError::bad_request("commit references too many objects"));
    }
    Ok(hashes)
}

fn tree_dataset_keys(tree: &Value) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if let Some(dataset_ids) = tree.get("datasetIds").and_then(Value::as_array) {
        keys.extend(dataset_ids.iter().filter_map(Value::as_str).map(ToOwned::to_owned));
    }
    if let Some(datasets) = tree.get("datasets").and_then(Value::as_array) {
        for dataset in datasets {
            if let Some(key) = dataset
                .get("datasetKey")
                .or_else(|| dataset.get("key"))
                .or_else(|| dataset.get("datasetId"))
                .and_then(Value::as_str)
            {
                keys.insert(key.to_string());
            }
        }
    }
    for field in ["datasetMetadata", "datasetMetadataValues"] {
        if let Some(values) = tree.get(field).and_then(Value::as_object) {
            keys.extend(values.keys().cloned());
        }
    }
    if let Some(entries) = tree.get("entries").and_then(Value::as_array) {
        for entry in entries {
            if let Some(key) = entry
                .get("datasetKey")
                .or_else(|| entry.get("datasetId"))
                .and_then(Value::as_str)
            {
                keys.insert(key.to_string());
            }
        }
    }
    keys
}

fn parse_project_metadata_in_tree(
    tree: &Value,
) -> Result<(ProjectMetadata, bool), ApiError> {
    let has_field = tree.get("projectMetadata").is_some();
    let metadata = data_project_metadata::parse(tree.get("projectMetadata"))
        .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))?;
    let dataset_keys = tree_dataset_keys(tree);
    data_project_metadata::validate(&metadata, Some(&dataset_keys))
        .map_err(|error| ApiError::bad_request(format!("invalid projectMetadata: {error}")))?;
    Ok((metadata, has_field))
}

async fn validate_project_metadata_graph_compatibility(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    input_commits: &[CommitInput],
    commit: &CommitInput,
) -> Result<(), ApiError> {
    let (metadata, has_field) = parse_project_metadata_in_tree(&commit.tree)?;
    for parent_id in &commit.parent_ids {
        let parent_tree = if let Some(candidate) = input_commits
            .iter()
            .find(|candidate| candidate.commit_id == *parent_id)
        {
            Some(candidate.tree.clone())
        } else {
            sqlx::query_scalar::<_, Value>(
                "SELECT tree_json FROM data_project_commits WHERE project_id = $1 AND commit_id = $2",
            )
            .bind(project_id)
            .bind(parent_id)
            .fetch_optional(&mut **tx)
            .await?
        };
        let Some(parent_tree) = parent_tree else {
            continue;
        };
        let (parent_metadata, _) = parse_project_metadata_in_tree(&parent_tree)?;
        if !parent_metadata.is_empty() && (!has_field || metadata.is_empty()) {
            return Err(ApiError::conflict(
                "PROJECT_METADATA_REQUIRED",
                "this commit would drop enabled dataset-group metadata from its parent",
                json!({
                    "commitId": commit.commit_id,
                    "parentId": parent_id,
                    "capability": "projectMetadata.datasetGroups"
                }),
            ));
        }
    }
    Ok(())
}

fn validate_new_commit_graph(commits: &[CommitInput]) -> Result<(), ApiError> {
    if commits.len() > MAX_GRAPH_COMMITS {
        return Err(ApiError::bad_request("commit graph is too large"));
    }
    let mut parents = HashMap::<String, Vec<String>>::new();
    let mut object_hashes = BTreeSet::new();
    for commit in commits {
        object_hashes.extend(validate_commit_input(commit)?);
        if object_hashes.len() > MAX_GRAPH_OBJECTS {
            return Err(ApiError::bad_request(
                "commit graph references too many objects",
            ));
        }
        if parents
            .insert(commit.commit_id.clone(), commit.parent_ids.clone())
            .is_some()
        {
            return Err(ApiError::conflict(
                "DUPLICATE_COMMIT",
                "push contains duplicate commit IDs",
                json!({ "commitId": commit.commit_id }),
            ));
        }
    }
    let mut visiting = HashSet::new();
    let mut visited = HashSet::new();
    for id in parents.keys() {
        visit_graph(id, &parents, &mut visiting, &mut visited)?;
    }
    Ok(())
}

fn visit_graph(
    id: &str,
    parents: &HashMap<String, Vec<String>>,
    visiting: &mut HashSet<String>,
    visited: &mut HashSet<String>,
) -> Result<(), ApiError> {
    if visited.contains(id) {
        return Ok(());
    }
    if !visiting.insert(id.to_string()) {
        return Err(ApiError::bad_request("commit graph contains a cycle"));
    }
    if let Some(parent_ids) = parents.get(id) {
        for parent in parent_ids {
            if parents.contains_key(parent) {
                visit_graph(parent, parents, visiting, visited)?;
            }
        }
    }
    visiting.remove(id);
    visited.insert(id.to_string());
    Ok(())
}

async fn object_exists_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    hash: &str,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2)",
    )
    .bind(project_id)
    .bind(hash)
    .fetch_one(&mut **tx)
    .await?)
}

async fn commit_exists_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    commit_id: &str,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM data_project_commits WHERE project_id = $1 AND commit_id = $2)",
    )
    .bind(project_id)
    .bind(commit_id)
    .fetch_one(&mut **tx)
    .await?)
}

async fn is_ancestor_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
    descendant: &str,
    ancestor: &str,
) -> Result<bool, ApiError> {
    Ok(sqlx::query_scalar::<_, bool>(
        r#"
        WITH RECURSIVE graph(commit_id) AS (
            SELECT $2::TEXT
            UNION
            SELECT parent_id
            FROM graph g
            JOIN data_project_commits c
              ON c.project_id = $1 AND c.commit_id = g.commit_id
            CROSS JOIN LATERAL jsonb_array_elements_text(c.parent_ids_json) AS p(parent_id)
        )
        SELECT EXISTS (SELECT 1 FROM graph WHERE commit_id = $3)
        "#,
    )
    .bind(project_id)
    .bind(descendant)
    .bind(ancestor)
    .fetch_one(&mut **tx)
    .await?)
}

async fn validate_dataset_binding_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_organization: Option<Uuid>,
    owner_organization: Option<Uuid>,
    dataset_id: Uuid,
) -> Result<(), ApiError> {
    let dataset = sqlx::query(
        "SELECT organization_id FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted' FOR SHARE",
    )
    .bind(dataset_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found or deleted"))?;
    let dataset_organization: Uuid = dataset.get("organization_id");
    if !organization_scope_matches(
        project_organization,
        owner_organization,
        Some(dataset_organization),
    ) {
        return Err(ApiError::forbidden(
            "data-project dataset must belong to the project organization",
        ));
    }
    Ok(())
}

pub async fn push(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<PushRequest>,
) -> Result<Json<Value>, ApiError> {
    require_data_project_permission(&state, user.id, project_id, DataProjectPermission::Edit)
        .await?;
    if input.expected_remote_revision < 1 || input.refs.len() > 128 {
        return Err(ApiError::bad_request(
            "invalid expected remote revision or ref count",
        ));
    }
    if input.commits.len() > MAX_GRAPH_COMMITS {
        return Err(ApiError::bad_request("commit graph is too large"));
    }
    validate_new_commit_graph(&input.commits)?;
    let mut tx = state.pool.begin().await?;
    let project = sqlx::query(
        "SELECT p.revision, p.status, p.organization_id, owner.organization_id AS owner_organization FROM data_projects p JOIN users owner ON owner.id = p.owner_id WHERE p.id = $1 AND p.deleted_at IS NULL FOR UPDATE OF p",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("data project not found"))?;
    let remote_revision: i64 = project.get("revision");
    if remote_revision != input.expected_remote_revision {
        let refs = current_refs_tx(&mut tx, project_id).await?;
        return Err(ApiError::conflict(
            "REMOTE_REVISION_MISMATCH",
            "remote data-project revision changed",
            json!({ "remoteRevision": remote_revision, "refs": refs }),
        ));
    }
    if project.get::<String, _>("status") == "deleted" {
        return Err(ApiError::not_found("data project not found"));
    }
    let project_groups_enabled: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM data_project_worktrees WHERE project_id = $1 AND CASE WHEN jsonb_typeof(project_metadata_json->'datasetGroups') = 'array' THEN jsonb_array_length(project_metadata_json->'datasetGroups') ELSE 0 END > 0)",
    )
    .bind(project_id)
    .fetch_one(&mut *tx)
    .await?;
    for commit in &input.commits {
        let hashes = validate_commit_input(commit)?;
        let (metadata, has_metadata_field) = parse_project_metadata_in_tree(&commit.tree)?;
        if project_groups_enabled && (!has_metadata_field || metadata.is_empty()) {
            return Err(ApiError::conflict(
                "PROJECT_METADATA_REQUIRED",
                "this project has enabled dataset-group metadata; the pushed commit must carry it",
                json!({
                    "commitId": commit.commit_id,
                    "capability": "projectMetadata.datasetGroups"
                }),
            ));
        }
        validate_project_metadata_graph_compatibility(
            &mut tx,
            project_id,
            &input.commits,
            commit,
        )
        .await?;
        for hash in hashes {
            if !object_exists_tx(&mut tx, project_id, &hash).await? {
                return Err(ApiError::conflict(
                    "MISSING_OBJECT",
                    "commit references an object that has not been uploaded",
                    json!({ "hash": hash, "commitId": commit.commit_id }),
                ));
            }
        }
        for parent in &commit.parent_ids {
            if !commit_exists_tx(&mut tx, project_id, parent).await?
                && !input
                    .commits
                    .iter()
                    .any(|candidate| candidate.commit_id == *parent)
            {
                return Err(ApiError::conflict(
                    "MISSING_PARENT",
                    "commit references an unknown parent",
                    json!({ "commitId": commit.commit_id, "parentId": parent }),
                ));
            }
        }
        if let Some(existing) = sqlx::query(
            "SELECT parent_ids_json, message, tree_json, metadata_json FROM data_project_commits WHERE project_id = $1 AND commit_id = $2",
        )
        .bind(project_id)
        .bind(&commit.commit_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let same = existing.get::<Value, _>("parent_ids_json")
                == serde_json::to_value(&commit.parent_ids).unwrap_or(Value::Null)
                && existing.get::<String, _>("message") == commit.message
                && existing.get::<Value, _>("tree_json") == commit.tree
                && existing.get::<Value, _>("metadata_json") == commit.metadata;
            if !same {
                return Err(ApiError::conflict(
                    "IMMUTABLE_COMMIT_MISMATCH",
                    "commit ID already exists with different content",
                    json!({ "commitId": commit.commit_id }),
                ));
            }
        } else {
            sqlx::query(
                "INSERT INTO data_project_commits (project_id, commit_id, parent_ids_json, message, tree_json, metadata_json, created_by, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, COALESCE($8, now()))",
            )
            .bind(project_id)
            .bind(&commit.commit_id)
            .bind(serde_json::to_value(&commit.parent_ids).map_err(|error| ApiError::bad_request(format!("invalid commit parents: {error}")))?)
            .bind(commit.message.trim())
            .bind(&commit.tree)
            .bind(&commit.metadata)
            .bind(user.id)
            .bind(commit.created_at)
            .execute(&mut *tx)
            .await?;
        }
    }
    if let Some(datasets) = &input.datasets {
        if datasets.len() > 100_000 {
            return Err(ApiError::bad_request(
                "data-project membership is too large",
            ));
        }
        let mut seen = BTreeSet::new();
        for dataset in datasets {
            let key = dataset.dataset_key.trim();
            if key.is_empty() || key.len() > 256 || !seen.insert(key.to_string()) {
                return Err(ApiError::bad_request(
                    "invalid or duplicate dataset membership key",
                ));
            }
            if let Some(dataset_id) = dataset.dataset_id {
                validate_dataset_binding_tx(
                    &mut tx,
                    project.get("organization_id"),
                    project.get("owner_organization"),
                    dataset_id,
                )
                .await?;
            }
            sqlx::query(
                "INSERT INTO data_project_datasets (project_id, dataset_key, dataset_id, metadata_json, position) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(project_id, dataset_key) DO UPDATE SET dataset_id = excluded.dataset_id, metadata_json = excluded.metadata_json, position = excluded.position, updated_at = now()",
            )
            .bind(project_id)
            .bind(key)
            .bind(dataset.dataset_id)
            .bind(&dataset.metadata)
            .bind(dataset.position)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "DELETE FROM data_project_datasets WHERE project_id = $1 AND dataset_key <> ALL($2)",
        )
        .bind(project_id)
        .bind(seen.into_iter().collect::<Vec<_>>())
        .execute(&mut *tx)
        .await?;
    }
    for update in &input.refs {
        let name = validate_ref_name(&update.name)?;
        let current = sqlx::query(
            "SELECT commit_id FROM data_project_refs WHERE project_id = $1 AND ref_name = $2 FOR UPDATE",
        )
        .bind(project_id)
        .bind(&name)
        .fetch_optional(&mut *tx)
        .await?
        .and_then(|row| row.get::<Option<String>, _>("commit_id"));
        if current != update.expected_commit_id {
            return Err(ApiError::conflict(
                "NON_FAST_FORWARD",
                "remote ref changed or expected commit does not match",
                json!({ "ref": name, "expectedCommitId": update.expected_commit_id, "remoteCommitId": current }),
            ));
        }
        if name == "main" && current.is_some() && update.commit_id.is_none() {
            return Err(ApiError::conflict(
                "PROTECTED_REF",
                "the main branch cannot be deleted; restore a version with a new commit instead",
                json!({ "ref": name }),
            ));
        }
        if let Some(next) = &update.commit_id {
            if !commit_exists_tx(&mut tx, project_id, next).await? {
                return Err(ApiError::conflict(
                    "MISSING_COMMIT",
                    "ref points to an unknown commit",
                    json!({ "ref": name, "commitId": next }),
                ));
            }
            if let Some(current_id) = current.as_deref() {
                if !is_ancestor_tx(&mut tx, project_id, next, current_id).await? {
                    return Err(ApiError::conflict(
                        "NON_FAST_FORWARD",
                        "remote ref is not an ancestor of the pushed commit",
                        json!({ "ref": name, "remoteCommitId": current_id, "commitId": next }),
                    ));
                }
            }
        }
    }
    let new_revision: i64 = sqlx::query_scalar(
        "UPDATE data_projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    for update in &input.refs {
        let name = validate_ref_name(&update.name)?;
        sqlx::query(
            "INSERT INTO data_project_refs (project_id, ref_name, commit_id, revision, updated_by) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(project_id, ref_name) DO UPDATE SET commit_id = excluded.commit_id, revision = excluded.revision, updated_by = excluded.updated_by, updated_at = now()",
        )
        .bind(project_id)
        .bind(name)
        .bind(&update.commit_id)
        .bind(new_revision)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(Json(json!({
        "projectId": project_id,
        "remoteRevision": new_revision,
        "refs": current_refs(&state, project_id).await?
    })))
}

async fn current_refs_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
) -> Result<Vec<Value>, ApiError> {
    let rows = sqlx::query("SELECT ref_name, commit_id, revision FROM data_project_refs WHERE project_id = $1 ORDER BY ref_name")
        .bind(project_id)
        .fetch_all(&mut **tx)
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            json!({
                "name": row.get::<String, _>("ref_name"),
                "commitId": row.get::<Option<String>, _>("commit_id"),
                "revision": row.get::<i64, _>("revision")
            })
        })
        .collect())
}

async fn current_refs(state: &AppState, project_id: Uuid) -> Result<Vec<Value>, ApiError> {
    let rows = sqlx::query("SELECT ref_name, commit_id, revision FROM data_project_refs WHERE project_id = $1 ORDER BY ref_name")
        .bind(project_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(rows
        .iter()
        .map(|row| {
            json!({
                "name": row.get::<String, _>("ref_name"),
                "commitId": row.get::<Option<String>, _>("commit_id"),
                "revision": row.get::<i64, _>("revision")
            })
        })
        .collect())
}

fn object_key(project_id: Uuid, hash: &str) -> String {
    format!("data-projects/{project_id}/objects/{hash}")
}

fn temp_object_path(project_id: Uuid) -> PathBuf {
    std::env::temp_dir().join(format!(
        "image-annotation-data-project-{project_id}-{}",
        Uuid::new_v4()
    ))
}

pub async fn put_blob(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, hash)): Path<(Uuid, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    require_data_project_permission(&state, user.id, project_id, DataProjectPermission::Edit)
        .await?;
    let hash = hash.to_ascii_lowercase();
    if !valid_hash(&hash) {
        return Err(ApiError::bad_request(
            "object hash must be a SHA-256 hex digest",
        ));
    }
    let expected_size = headers
        .get("x-object-size")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or_else(|| ApiError::bad_request("X-Object-Size is required"))?;
    if !(0..=MAX_OBJECT_BYTES).contains(&expected_size) {
        return Err(ApiError::bad_request(
            "object size is outside the supported range",
        ));
    }
    let kind = headers
        .get("x-object-kind")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("other")
        .to_string();
    if !matches!(
        kind.as_str(),
        "image" | "annotation" | "dataset-metadata" | "project-metadata" | "other"
    ) {
        return Err(ApiError::bad_request("invalid object kind"));
    }
    let mime_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let temp_path = temp_object_path(project_id);
    let upload_result: Result<(String, i64), ApiError> = async {
        let mut file = tokio::fs::File::create(&temp_path).await.map_err(|error| {
            ApiError::internal_message(format!("create object temp file: {error}"))
        })?;
        let mut stream = body.into_data_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_i64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|error| ApiError::bad_request(format!("read object body: {error}")))?;
            size = size.saturating_add(chunk.len() as i64);
            if size > expected_size || size > MAX_OBJECT_BYTES {
                return Err(ApiError::bad_request("object size exceeds X-Object-Size"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                ApiError::internal_message(format!("write object temp file: {error}"))
            })?;
        }
        file.flush()
            .await
            .map_err(|error| ApiError::internal_message(format!("flush object: {error}")))?;
        let actual_hash = hex::encode(hasher.finalize());
        if size != expected_size || actual_hash != hash {
            return Err(ApiError::bad_request(
                "object SHA-256 or size does not match metadata",
            ));
        }
        Ok((actual_hash, size))
    }
    .await;
    if let Err(error) = upload_result {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(error);
    }
    let key = object_key(project_id, &hash);
    let stream = ByteStream::from_path(&temp_path)
        .await
        .map_err(|error| ApiError::internal_message(format!("open object temp file: {error}")))?;
    state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&key)
        .content_type(mime_type.clone())
        .metadata("sha256", hash.clone())
        .body(stream)
        .send()
        .await
        .map_err(|error| {
            ApiError::internal_message(format!("store data-project object: {error}"))
        })?;
    let _ = tokio::fs::remove_file(&temp_path).await;
    sqlx::query(
        "INSERT INTO data_project_blobs (project_id, content_hash, byte_size, kind, mime_type, object_key, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(project_id, content_hash, byte_size) DO NOTHING",
    )
    .bind(project_id)
    .bind(&hash)
    .bind(expected_size)
    .bind(&kind)
    .bind(&mime_type)
    .bind(&key)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(Json(
        json!({ "projectId": project_id, "hash": hash, "byteSize": expected_size, "kind": kind, "mimeType": mime_type }),
    ))
}

pub async fn get_blob(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, hash)): Path<(Uuid, String)>,
) -> Result<Response, ApiError> {
    require_data_project_permission(&state, user.id, project_id, DataProjectPermission::View)
        .await?;
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
    .ok_or_else(|| ApiError::not_found("data-project object not found"))?;
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(row.get::<String, _>("object_key"))
        .send()
        .await
        .map_err(|error| ApiError::not_found(format!("data-project object not found: {error}")))?;
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

#[cfg(test)]
mod tests {
    use super::{
        organization_scope_matches, parse_project_metadata_in_tree, project_organization_scope,
    };
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn explicit_project_organization_wins_over_legacy_owner_scope() {
        let project_org = Uuid::new_v4();
        let owner_org = Uuid::new_v4();
        assert_eq!(
            project_organization_scope(Some(project_org), Some(owner_org)),
            Some(project_org)
        );
        assert!(organization_scope_matches(
            Some(project_org),
            Some(owner_org),
            Some(project_org)
        ));
        assert!(!organization_scope_matches(
            Some(project_org),
            Some(owner_org),
            Some(owner_org)
        ));
    }

    #[test]
    fn legacy_null_project_uses_owner_organization_without_wildcarding() {
        let owner_org = Uuid::new_v4();
        assert_eq!(
            project_organization_scope(None, Some(owner_org)),
            Some(owner_org)
        );
        assert!(organization_scope_matches(
            None,
            Some(owner_org),
            Some(owner_org)
        ));
        assert!(!organization_scope_matches(
            None,
            Some(owner_org),
            Some(Uuid::new_v4())
        ));
        assert!(!organization_scope_matches(None, Some(owner_org), None));
    }

    #[test]
    fn orgless_legacy_project_only_matches_orgless_members() {
        let organization = Uuid::new_v4();
        assert!(organization_scope_matches(None, None, None));
        assert!(!organization_scope_matches(None, None, Some(organization)));
    }

    #[test]
    fn legacy_graph_tree_defaults_to_empty_groups_and_new_groups_validate_keys() {
        let (legacy, has_field) = parse_project_metadata_in_tree(&json!({
            "datasetIds": ["road"]
        }))
        .unwrap();
        assert!(!has_field);
        assert!(legacy.is_empty());

        let (metadata, has_field) = parse_project_metadata_in_tree(&json!({
            "datasetIds": ["road"],
            "projectMetadata": {
                "schemaVersion": 1,
                "datasetGroups": [{
                    "id": "roads",
                    "name": "Road",
                    "datasetKeys": ["road"],
                    "position": 0
                }]
            }
        }))
        .unwrap();
        assert!(has_field);
        assert_eq!(metadata.dataset_groups[0].dataset_keys, vec!["road"]);

        assert!(parse_project_metadata_in_tree(&json!({
            "datasetIds": ["road"],
            "projectMetadata": {
                "schemaVersion": 1,
                "datasetGroups": [{
                    "id": "roads",
                    "name": "Road",
                    "datasetKeys": ["other"],
                    "position": 0
                }]
            }
        }))
        .is_err());
    }
}

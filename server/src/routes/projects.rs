use super::ApiError;
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateProject {
    name: String,
    #[serde(default)]
    description: String,
    organization_id: Option<Uuid>,
    #[serde(default = "default_mode")]
    mode: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProject {
    name: Option<String>,
    description: Option<String>,
    status: Option<String>,
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AddMember {
    user_id: Uuid,
    role: String,
}

fn default_mode() -> String {
    "cloud_linked".to_string()
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT p.id, p.organization_id, p.name, p.description, p.mode, p.status,
            p.revision, p.updated_at, pm.role
        FROM projects p
        JOIN project_members pm ON pm.project_id = p.id
        WHERE pm.user_id = $1 AND pm.deleted_at IS NULL AND p.deleted_at IS NULL
        ORDER BY p.updated_at DESC
        "#,
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(rows.iter().map(project_json).collect())))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<CreateProject>,
) -> Result<Json<Value>, ApiError> {
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("project name is required"));
    }
    if !matches!(input.mode.as_str(), "local_only" | "cloud_linked" | "mirrored") {
        return Err(ApiError::bad_request("invalid project mode"));
    }
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO projects (id, organization_id, name, description, mode, status, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, 'active', $6, $6)",
    )
    .bind(id)
    .bind(input.organization_id.or(user.organization_id))
    .bind(input.name.trim())
    .bind(input.description.as_str())
    .bind(&input.mode)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, 'owner')",
    )
    .bind(id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    write_change(&mut tx, id, "project", &id.to_string(), "create", 1, json!({
        "id": id,
        "name": input.name,
        "description": input.description,
        "mode": input.mode,
        "status": "active"
    }))
    .await?;
    write_audit(&mut tx, id, user.id, "project.create", "project", &id.to_string())
        .await?;
    tx.commit().await?;
    get(State(state), user, Path(id)).await
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let row = sqlx::query(
        "SELECT id, organization_id, name, description, mode, status, revision, updated_at FROM projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    Ok(Json(project_json(&row)))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<UpdateProject>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData).await?;
    if input
        .status
        .as_deref()
        .is_some_and(|value| !matches!(value, "draft" | "active" | "archived"))
    {
        return Err(ApiError::bad_request("invalid project status"));
    }
    if input
        .mode
        .as_deref()
        .is_some_and(|value| !matches!(value, "local_only" | "cloud_linked" | "mirrored"))
    {
        return Err(ApiError::bad_request("invalid project mode"));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        UPDATE projects SET
            name = COALESCE($2, name),
            description = COALESCE($3, description),
            status = COALESCE($4, status),
            mode = COALESCE($5, mode),
            revision = revision + 1,
            updated_by = $6,
            updated_at = now()
        WHERE id = $1 AND deleted_at IS NULL
        RETURNING revision
        "#,
    )
    .bind(project_id)
    .bind(input.name.as_deref())
    .bind(input.description.as_deref())
    .bind(input.status.as_deref())
    .bind(input.mode.as_deref())
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    let revision: i64 = row.get("revision");
    write_change(&mut tx, project_id, "project", &project_id.to_string(), "update", revision, json!({
        "name": input.name,
        "description": input.description,
        "status": input.status,
        "mode": input.mode
    }))
    .await?;
    write_audit(&mut tx, project_id, user.id, "project.update", "project", &project_id.to_string())
        .await?;
    tx.commit().await?;
    get(State(state), user, Path(project_id)).await
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData)
            .await?;
    if role != "owner" {
        return Err(ApiError::forbidden(
            "only a project owner can delete the project",
        ));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "UPDATE projects SET status = 'deleted', deleted_at = now(), revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 AND deleted_at IS NULL RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    write_change(
        &mut tx,
        project_id,
        "project",
        &project_id.to_string(),
        "delete",
        row.get("revision"),
        json!({ "deletedAt": chrono::Utc::now() }),
    )
    .await?;
    write_audit(&mut tx, project_id, user.id, "project.delete", "project", &project_id.to_string())
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "deleted": true })))
}

pub async fn members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        "SELECT user_id, role, joined_at FROM project_members WHERE project_id = $1 AND deleted_at IS NULL ORDER BY joined_at",
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
                    "joinedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("joined_at")
                })
            })
            .collect(),
    )))
}

pub async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<AddMember>,
) -> Result<Json<Value>, ApiError> {
    let actor_role =
        require_project_permission(&state, user.id, project_id, ProjectPermission::ManageMembers)
            .await?;
    if !matches!(
        input.role.as_str(),
        "owner" | "manager" | "annotator" | "reviewer" | "viewer"
    ) {
        return Err(ApiError::bad_request("invalid project role"));
    }
    if input.role == "owner" && actor_role != "owner" {
        return Err(ApiError::forbidden(
            "only an owner can grant the owner role",
        ));
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT(project_id, user_id) DO UPDATE SET role = excluded.role, deleted_at = NULL",
    )
    .bind(project_id)
    .bind(input.user_id)
    .bind(&input.role)
    .execute(&mut *tx)
    .await?;
    let revision: i64 = sqlx::query(
        "UPDATE projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?
    .get("revision");
    write_change(
        &mut tx,
        project_id,
        "project_member",
        &input.user_id.to_string(),
        "update",
        revision,
        json!({ "userId": input.user_id, "role": input.role }),
    )
    .await?;
    write_audit(&mut tx, project_id, user.id, "member.upsert", "member", &input.user_id.to_string())
        .await?;
    tx.commit().await?;
    Ok(Json(json!({ "userId": input.user_id, "role": input.role })))
}

pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, member_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let actor_role =
        require_project_permission(&state, user.id, project_id, ProjectPermission::ManageMembers)
            .await?;
    let target = sqlx::query(
        "SELECT role FROM project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(member_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("project member not found"))?;
    let target_role: String = target.get("role");
    if target_role == "owner" {
        if actor_role != "owner" {
            return Err(ApiError::forbidden(
                "only an owner can remove another owner",
            ));
        }
        let owner_count: i64 = sqlx::query(
            "SELECT COUNT(*) AS count FROM project_members WHERE project_id = $1 AND role = 'owner' AND deleted_at IS NULL",
        )
        .bind(project_id)
        .fetch_one(&state.pool)
        .await?
        .get("count");
        if owner_count <= 1 {
            return Err(ApiError::bad_request(
                "a project must retain at least one owner",
            ));
        }
    }
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE project_members SET deleted_at = now() WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(member_id)
    .execute(&mut *tx)
    .await?;
    let revision: i64 = sqlx::query(
        "UPDATE projects SET revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?
    .get("revision");
    write_change(
        &mut tx,
        project_id,
        "project_member",
        &member_id.to_string(),
        "delete",
        revision,
        json!({ "userId": member_id, "deleted": true }),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "member.remove",
        "member",
        &member_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "userId": member_id, "deleted": true })))
}

pub(crate) async fn write_change(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    revision: i64,
    payload: Value,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO change_events (project_id, entity_type, entity_id, operation, revision, payload) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(project_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(operation)
    .bind(revision)
    .bind(payload)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn write_audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    user_id: Uuid,
    action: &str,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO audit_events (project_id, user_id, action, entity_type, entity_id, result) VALUES ($1, $2, $3, $4, $5, 'success')",
    )
    .bind(project_id)
    .bind(user_id)
    .bind(action)
    .bind(entity_type)
    .bind(entity_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn project_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "organizationId": row.get::<Option<Uuid>, _>("organization_id"),
        "name": row.get::<String, _>("name"),
        "description": row.get::<String, _>("description"),
        "mode": row.get::<String, _>("mode"),
        "status": row.get::<String, _>("status"),
        "revision": row.get::<i64, _>("revision"),
        "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at"),
        "role": row.try_get::<String, _>("role").ok()
    })
}

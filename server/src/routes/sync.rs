use super::{projects::write_audit, projects::write_change, ApiError};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushRequest {
    device_id: String,
    project_id: Uuid,
    operations: Vec<PushOperation>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushOperation {
    operation_id: String,
    entity_type: String,
    entity_id: String,
    operation: String,
    base_revision: Option<i64>,
    payload: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PushResult {
    operation_id: String,
    status: String,
    server_revision: Option<i64>,
    remote_payload: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangeQuery {
    cursor: Option<i64>,
    limit: Option<i64>,
}

pub async fn push(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<PushRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(
        &state,
        user.id,
        input.project_id,
        ProjectPermission::View,
    )
    .await?;
    if input.operations.len() > 500 {
        return Err(ApiError::bad_request("sync batch exceeds 500 operations"));
    }
    let mut results = Vec::with_capacity(input.operations.len());
    for operation in input.operations {
        if let Some(row) = sqlx::query(
            "SELECT result_json FROM processed_operations WHERE operation_id = $1 AND project_id = $2",
        )
        .bind(&operation.operation_id)
        .bind(input.project_id)
        .fetch_optional(&state.pool)
        .await?
        {
            let stored: Value = row.get("result_json");
            results.push(PushResult {
                operation_id: operation.operation_id,
                status: "duplicate".to_string(),
                server_revision: stored.get("serverRevision").and_then(Value::as_i64),
                remote_payload: stored.get("remotePayload").cloned(),
                error: None,
            });
            continue;
        }
        let permission = permission_for(&operation);
        if let Err((_, message)) =
            require_project_permission(&state, user.id, input.project_id, permission).await
        {
            results.push(PushResult {
                operation_id: operation.operation_id,
                status: "rejected".to_string(),
                server_revision: None,
                remote_payload: None,
                error: Some(message),
            });
            continue;
        }
        let result = apply_operation(
            &state,
            &user,
            input.project_id,
            &input.device_id,
            &operation,
        )
        .await;
        match result {
            Ok((revision, payload)) => {
                let stored = json!({
                    "operationId": operation.operation_id,
                    "status": "applied",
                    "serverRevision": revision,
                    "remotePayload": payload
                });
                sqlx::query(
                    "INSERT INTO processed_operations (operation_id, project_id, result_json) VALUES ($1, $2, $3)",
                )
                .bind(&operation.operation_id)
                .bind(input.project_id)
                .bind(&stored)
                .execute(&state.pool)
                .await?;
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "applied".to_string(),
                    server_revision: Some(revision),
                    remote_payload: Some(payload),
                    error: None,
                });
            }
            Err(OperationError::Conflict {
                current_revision,
                remote,
            }) => results.push(PushResult {
                operation_id: operation.operation_id,
                status: "conflict".to_string(),
                server_revision: Some(current_revision),
                remote_payload: Some(remote),
                error: None,
            }),
            Err(OperationError::Rejected(message)) => results.push(PushResult {
                operation_id: operation.operation_id,
                status: "rejected".to_string(),
                server_revision: None,
                remote_payload: None,
                error: Some(message),
            }),
            Err(OperationError::Retryable(message)) => results.push(PushResult {
                operation_id: operation.operation_id,
                status: "retryable".to_string(),
                server_revision: None,
                remote_payload: None,
                error: Some(message),
            }),
        }
    }
    Ok(Json(json!({ "results": results })))
}

pub async fn changes(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(query): Query<ChangeQuery>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let cursor = query.cursor.unwrap_or(0).max(0);
    let limit = query.limit.unwrap_or(500).clamp(1, 500);
    let rows = sqlx::query(
        "SELECT sequence, entity_type, entity_id, operation, revision, payload, created_at FROM change_events WHERE project_id = $1 AND sequence > $2 ORDER BY sequence LIMIT $3",
    )
    .bind(project_id)
    .bind(cursor)
    .bind(limit + 1)
    .fetch_all(&state.pool)
    .await?;
    let has_more = rows.len() as i64 > limit;
    let visible = rows.iter().take(limit as usize).collect::<Vec<_>>();
    let next_cursor = visible
        .last()
        .map(|row| row.get::<i64, _>("sequence"))
        .unwrap_or(cursor);
    Ok(Json(json!({
        "nextCursor": next_cursor.to_string(),
        "hasMore": has_more,
        "changes": visible.iter().map(|row| json!({
            "sequence": row.get::<i64, _>("sequence"),
            "entityType": row.get::<String, _>("entity_type"),
            "entityId": row.get::<String, _>("entity_id"),
            "operation": row.get::<String, _>("operation"),
            "revision": row.get::<i64, _>("revision"),
            "payload": row.get::<Value, _>("payload"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn bootstrap(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let project = sqlx::query(
        "SELECT id, name, description, mode, status, revision, updated_at FROM projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    let assets = sqlx::query(
        "SELECT id, client_key, file_name, object_key, content_hash, mime_type, width, height, byte_size, split, status, revision, updated_at FROM assets WHERE project_id = $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let annotations = sqlx::query(
        "SELECT id, image_id, revision, schema_version_id, object_json, status, updated_by, updated_at FROM annotations WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let issues = sqlx::query(
        "SELECT id, client_key, image_id, annotation_object_id, title, description, severity, status, reporter_id, assignee_id, revision, updated_at FROM issues WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let folders = sqlx::query(
        "SELECT id, client_key, parent_id, name, sort_order, revision, updated_at FROM folders WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let comments = sqlx::query(
        "SELECT c.id, c.issue_id, c.author_id, c.content, c.revision, c.created_at FROM issue_comments c JOIN issues i ON i.id = c.issue_id WHERE i.project_id = $1 AND c.deleted_at IS NULL ORDER BY c.created_at, c.id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let attachments = sqlx::query(
        "SELECT id, issue_id, client_key, file_name, object_key, content_hash, mime_type, byte_size, status, revision, created_by, created_at FROM issue_attachments WHERE project_id = $1 AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let members = sqlx::query(
        "SELECT user_id, role, joined_at FROM project_members WHERE project_id = $1 AND deleted_at IS NULL ORDER BY joined_at, user_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let folder_members = sqlx::query(
        "SELECT fm.folder_id, fm.image_id, fm.revision FROM folder_members fm JOIN folders f ON f.id = fm.folder_id WHERE f.project_id = $1 AND fm.deleted_at IS NULL ORDER BY fm.folder_id, fm.image_id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let max_sequence = sqlx::query(
        "SELECT COALESCE(MAX(sequence), 0) AS sequence FROM change_events WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_one(&state.pool)
    .await?
    .get::<i64, _>("sequence");
    Ok(Json(json!({
        "cursor": max_sequence.to_string(),
        "project": {
            "id": project.get::<Uuid, _>("id"),
            "name": project.get::<String, _>("name"),
            "description": project.get::<String, _>("description"),
            "mode": project.get::<String, _>("mode"),
            "status": project.get::<String, _>("status"),
            "revision": project.get::<i64, _>("revision"),
            "updatedAt": project.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        },
        "assets": assets.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": row.get::<Option<String>, _>("client_key"),
            "fileName": row.get::<String, _>("file_name"),
            "objectKey": row.get::<String, _>("object_key"),
            "contentHash": row.get::<String, _>("content_hash"),
            "mimeType": row.get::<String, _>("mime_type"),
            "width": row.get::<i32, _>("width"),
            "height": row.get::<i32, _>("height"),
            "byteSize": row.get::<i64, _>("byte_size"),
            "split": row.get::<String, _>("split"),
            "status": row.get::<String, _>("status"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>(),
        "annotations": annotations.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "revision": row.get::<i64, _>("revision"),
            "schemaVersionId": row.get::<Option<Uuid>, _>("schema_version_id"),
            "objects": row.get::<Value, _>("object_json"),
            "status": row.get::<String, _>("status"),
            "updatedBy": row.get::<Uuid, _>("updated_by")
        })).collect::<Vec<_>>(),
        "issues": issues.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": row.get::<Option<String>, _>("client_key"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "annotationObjectId": row.get::<Option<String>, _>("annotation_object_id"),
            "title": row.get::<String, _>("title"),
            "description": row.get::<String, _>("description"),
            "severity": row.get::<String, _>("severity"),
            "status": row.get::<String, _>("status"),
            "reporterId": row.get::<Uuid, _>("reporter_id"),
            "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>(),
        "folders": folders.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": row.get::<Option<String>, _>("client_key"),
            "parentId": row.get::<Option<Uuid>, _>("parent_id"),
            "name": row.get::<String, _>("name"),
            "sortOrder": row.get::<i32, _>("sort_order"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>(),
        "issueComments": comments.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "issueId": row.get::<Uuid, _>("issue_id"),
            "authorId": row.get::<Uuid, _>("author_id"),
            "content": row.get::<String, _>("content"),
            "revision": row.get::<i64, _>("revision"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        })).collect::<Vec<_>>(),
        "issueAttachments": attachments.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": row.get::<Option<String>, _>("client_key"),
            "projectId": project_id,
            "issueId": row.get::<Uuid, _>("issue_id"),
            "fileName": row.get::<String, _>("file_name"),
            "objectKey": row.get::<String, _>("object_key"),
            "contentHash": row.get::<String, _>("content_hash"),
            "mimeType": row.get::<String, _>("mime_type"),
            "byteSize": row.get::<i64, _>("byte_size"),
            "status": row.get::<String, _>("status"),
            "revision": row.get::<i64, _>("revision"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        })).collect::<Vec<_>>(),
        "members": members.iter().map(|row| json!({
            "userId": row.get::<Uuid, _>("user_id"),
            "role": row.get::<String, _>("role"),
            "joinedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("joined_at")
        })).collect::<Vec<_>>(),
        "folderMembers": folder_members.iter().map(|row| json!({
            "folderId": row.get::<Uuid, _>("folder_id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>()
    })))
}

enum OperationError {
    Conflict {
        current_revision: i64,
        remote: Value,
    },
    Rejected(String),
    Retryable(String),
}

async fn apply_operation(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    device_id: &str,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    match (operation.entity_type.as_str(), operation.operation.as_str()) {
        ("annotation", "update") => {
            apply_annotation(state, user, project_id, device_id, operation).await
        }
        ("annotation", "submit") => {
            apply_annotation_submit(state, user, project_id, device_id, operation).await
        }
        ("issue", "create") => apply_issue_create(state, user, project_id, operation).await,
        ("issue", "transition") | ("issue", "update") => {
            apply_issue_update(state, user, project_id, operation).await
        }
        ("issue_comment", "comment") => {
            apply_issue_comment(state, user, project_id, operation).await
        }
        ("folder", "create") | ("folder", "update") | ("folder", "delete") => {
            apply_folder(state, user, project_id, operation).await
        }
        ("folder_member", "update") => {
            apply_folder_member(state, user, project_id, operation).await
        }
        ("project", "update") => Ok((1, operation.payload.clone())),
        _ => Err(OperationError::Rejected(format!(
            "unsupported operation: {}/{}",
            operation.entity_type, operation.operation
        ))),
    }
}

async fn apply_annotation(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    device_id: &str,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let image_id = resolve_asset_id(state, project_id, &operation.entity_id).await?;
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let current = sqlx::query(
        "SELECT id, revision, object_json FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let current_revision = current.as_ref().map(|row| row.get("revision")).unwrap_or(0_i64);
    if operation.base_revision.unwrap_or(0) != current_revision {
        return Err(OperationError::Conflict {
            current_revision,
            remote: current
                .map(|row| row.get("object_json"))
                .unwrap_or_else(|| json!([])),
        });
    }
    let annotation_id = current
        .as_ref()
        .map(|row| row.get::<Uuid, _>("id"))
        .unwrap_or_else(Uuid::new_v4);
    let revision = current_revision + 1;
    let objects = operation
        .payload
        .get("objects")
        .cloned()
        .unwrap_or_else(|| json!([]));
    sqlx::query(
        "INSERT INTO annotations (id, project_id, image_id, revision, object_json, status, updated_by) VALUES ($1, $2, $3, $4, $5, 'draft', $6) ON CONFLICT(image_id) DO UPDATE SET revision = excluded.revision, object_json = excluded.object_json, status = 'draft', updated_by = excluded.updated_by, updated_at = now(), deleted_at = NULL",
    )
    .bind(annotation_id)
    .bind(project_id)
    .bind(image_id)
    .bind(revision)
    .bind(&objects)
    .bind(user.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    sqlx::query(
        "INSERT INTO annotation_versions (annotation_id, revision, object_json, operation_id, created_by) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(annotation_id)
    .bind(revision)
    .bind(&objects)
    .bind(&operation.operation_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let payload = json!({
        "id": annotation_id,
        "imageId": image_id,
        "clientImageId": operation.entity_id,
        "revision": revision,
        "objects": objects
    });
    write_change(
        &mut tx,
        project_id,
        "annotation",
        &operation.entity_id,
        "update",
        revision,
        payload.clone(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    sqlx::query(
        "INSERT INTO audit_events (project_id, user_id, device_id, action, entity_type, entity_id, result) VALUES ($1, $2, $3, 'annotation.save', 'annotation', $4, 'success')",
    )
    .bind(project_id)
    .bind(user.id)
    .bind(device_id)
    .bind(annotation_id.to_string())
    .execute(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn apply_annotation_submit(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    _device_id: &str,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let image_id = resolve_asset_id(state, project_id, &operation.entity_id).await?;
    let row = sqlx::query(
        "UPDATE annotations SET status = 'submitted', revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND image_id = $2 RETURNING id, revision",
    )
    .bind(project_id)
    .bind(image_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("annotation not found".to_string()))?;
    let revision: i64 = row.get("revision");
    Ok((revision, json!({ "imageId": image_id, "status": "submitted" })))
}

async fn apply_issue_create(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let image_key = operation
        .payload
        .get("imageId")
        .and_then(Value::as_str)
        .ok_or_else(|| OperationError::Rejected("issue imageId is required".to_string()))?;
    let image_id = resolve_asset_id(state, project_id, image_key).await?;
    let issue_id = Uuid::new_v4();
    let row = sqlx::query(
        "INSERT INTO issues (id, project_id, client_key, image_id, annotation_object_id, title, description, severity, status, source, reporter_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'open', $9, $10) ON CONFLICT(project_id, client_key) WHERE client_key IS NOT NULL DO UPDATE SET updated_at = issues.updated_at RETURNING id, revision",
    )
    .bind(issue_id)
    .bind(project_id)
    .bind(&operation.entity_id)
    .bind(image_id)
    .bind(operation.payload.get("annotationObjectId").and_then(Value::as_str))
    .bind(operation.payload.get("title").and_then(Value::as_str).unwrap_or("本地缺陷"))
    .bind(operation.payload.get("description").and_then(Value::as_str).unwrap_or(""))
    .bind(operation.payload.get("severity").and_then(Value::as_str).unwrap_or("major"))
    .bind(operation.payload.get("source").and_then(Value::as_str).unwrap_or("sync"))
    .bind(user.id)
    .fetch_one(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    let payload = json!({
        "id": id,
        "clientKey": operation.entity_id,
        "imageId": image_id,
        "clientImageId": image_key,
        "annotationObjectId": operation.payload.get("annotationObjectId"),
        "title": operation.payload.get("title").and_then(Value::as_str).unwrap_or("本地缺陷"),
        "description": operation.payload.get("description").and_then(Value::as_str).unwrap_or(""),
        "severity": operation.payload.get("severity").and_then(Value::as_str).unwrap_or("major"),
        "status": "open",
        "reporterId": user.id,
        "assigneeId": Value::Null,
        "revision": revision
    });
    write_change_pool(
        state,
        project_id,
        "issue",
        &operation.entity_id,
        "create",
        revision,
        &payload,
    )
    .await?;
    Ok((revision, payload))
}

async fn apply_issue_update(
    state: &AppState,
    _user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let row = sqlx::query(
        "SELECT id, revision, status, title, description, severity FROM issues WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(&operation.entity_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("issue not found".to_string()))?;
    let revision: i64 = row.get("revision");
    if operation.base_revision.is_some_and(|base| base != revision) {
        return Err(OperationError::Conflict {
            current_revision: revision,
            remote: json!({
                "status": row.get::<String, _>("status"),
                "title": row.get::<String, _>("title"),
                "description": row.get::<String, _>("description"),
                "severity": row.get::<String, _>("severity")
            }),
        });
    }
    let id: Uuid = row.get("id");
    let status = operation
        .payload
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| row.get::<String, _>("status"));
    let updated = sqlx::query(
        "UPDATE issues SET status = $2, title = COALESCE($3, title), description = COALESCE($4, description), severity = COALESCE($5, severity), revision = revision + 1, updated_at = now() WHERE id = $1 RETURNING revision, status, title, description, severity, image_id, reporter_id, assignee_id, client_key",
    )
    .bind(id)
    .bind(&status)
    .bind(operation.payload.get("title").and_then(Value::as_str))
    .bind(operation.payload.get("description").and_then(Value::as_str))
    .bind(operation.payload.get("severity").and_then(Value::as_str))
    .fetch_one(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let next_revision: i64 = updated.get("revision");
    let payload = json!({
        "id": id,
        "clientKey": updated.get::<Option<String>, _>("client_key").unwrap_or_else(|| operation.entity_id.clone()),
        "imageId": updated.get::<Uuid, _>("image_id"),
        "status": updated.get::<String, _>("status"),
        "title": updated.get::<String, _>("title"),
        "description": updated.get::<String, _>("description"),
        "severity": updated.get::<String, _>("severity"),
        "reporterId": updated.get::<Uuid, _>("reporter_id"),
        "assigneeId": updated.get::<Option<Uuid>, _>("assignee_id"),
        "revision": next_revision
    });
    write_change_pool(
        state,
        project_id,
        "issue",
        &operation.entity_id,
        &operation.operation,
        next_revision,
        &payload,
    )
    .await?;
    Ok((next_revision, payload))
}

async fn apply_issue_comment(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let issue_key = operation
        .payload
        .get("issueId")
        .and_then(Value::as_str)
        .ok_or_else(|| OperationError::Rejected("issueId is required".to_string()))?;
    let issue = sqlx::query(
        "SELECT id FROM issues WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(issue_key)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("issue not found".to_string()))?;
    let issue_id: Uuid = issue.get("id");
    let id = Uuid::new_v4();
    let content = operation
        .payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    sqlx::query(
        "INSERT INTO issue_comments (id, issue_id, author_id, content) VALUES ($1, $2, $3, $4)",
    )
    .bind(id)
    .bind(issue_id)
    .bind(user.id)
    .bind(content)
    .execute(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let payload = json!({
        "id": id,
        "clientKey": operation.entity_id,
        "issueId": issue_id,
        "content": content,
        "revision": 1
    });
    write_change_pool(
        state,
        project_id,
        "issue_comment",
        &operation.entity_id,
        "comment",
        1,
        &payload,
    )
    .await?;
    Ok((1, payload))
}

async fn apply_folder(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    if operation.operation == "create" {
        let id = Uuid::new_v4();
        let row = sqlx::query(
            "INSERT INTO folders (id, project_id, client_key, name, sort_order, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $6) ON CONFLICT(project_id, client_key) WHERE client_key IS NOT NULL DO UPDATE SET deleted_at = NULL RETURNING id, revision",
        )
        .bind(id)
        .bind(project_id)
        .bind(&operation.entity_id)
        .bind(operation.payload.get("name").and_then(Value::as_str).unwrap_or("文件夹"))
        .bind(operation.payload.get("sortOrder").and_then(Value::as_i64).unwrap_or(0) as i32)
        .bind(user.id)
        .fetch_one(&state.pool)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
        let revision: i64 = row.get("revision");
        let payload = json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": operation.entity_id,
            "name": operation.payload.get("name").and_then(Value::as_str).unwrap_or("文件夹"),
            "sortOrder": operation.payload.get("sortOrder").and_then(Value::as_i64).unwrap_or(0),
            "revision": revision
        });
        write_change_pool(
            state,
            project_id,
            "folder",
            &operation.entity_id,
            "create",
            revision,
            &payload,
        )
        .await?;
        return Ok((revision, payload));
    }
    let row = sqlx::query(
        "SELECT id, revision, name FROM folders WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(&operation.entity_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("folder not found".to_string()))?;
    let id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    if operation.base_revision.is_some_and(|base| base != revision) {
        return Err(OperationError::Conflict {
            current_revision: revision,
            remote: json!({ "name": row.get::<String, _>("name") }),
        });
    }
    let updated = if operation.operation == "delete" {
        sqlx::query(
            "UPDATE folders SET deleted_at = now(), revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision, name, sort_order",
        )
        .bind(id)
        .bind(user.id)
        .fetch_one(&state.pool)
        .await
    } else {
        sqlx::query(
            "UPDATE folders SET name = COALESCE($2, name), revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 RETURNING revision, name, sort_order",
        )
        .bind(id)
        .bind(operation.payload.get("name").and_then(Value::as_str))
        .bind(user.id)
        .fetch_one(&state.pool)
        .await
    }
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let revision: i64 = updated.get("revision");
    let payload = json!({
        "id": id,
        "clientKey": operation.entity_id,
        "operation": operation.operation,
        "name": updated.get::<String, _>("name"),
        "sortOrder": updated.get::<i32, _>("sort_order"),
        "revision": revision
    });
    write_change_pool(
        state,
        project_id,
        "folder",
        &operation.entity_id,
        &operation.operation,
        revision,
        &payload,
    )
    .await?;
    Ok((revision, payload))
}

async fn apply_folder_member(
    state: &AppState,
    _user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let folder_key = operation
        .payload
        .get("folderId")
        .and_then(Value::as_str)
        .ok_or_else(|| OperationError::Rejected("folderId is required".to_string()))?;
    let image_key = operation
        .payload
        .get("imageId")
        .and_then(Value::as_str)
        .ok_or_else(|| OperationError::Rejected("imageId is required".to_string()))?;
    let folder = sqlx::query(
        "SELECT id FROM folders WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(folder_key)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("folder not found".to_string()))?;
    let folder_id: Uuid = folder.get("id");
    let image_id = resolve_asset_id(state, project_id, image_key).await?;
    let row = sqlx::query(
        "INSERT INTO folder_members (folder_id, image_id, revision) VALUES ($1, $2, 1) ON CONFLICT(folder_id, image_id) DO UPDATE SET revision = folder_members.revision + 1, deleted_at = NULL RETURNING revision",
    )
    .bind(folder_id)
    .bind(image_id)
    .fetch_one(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let revision: i64 = row.get("revision");
    let payload = json!({
        "folderId": folder_id,
        "imageId": image_id,
        "revision": revision
    });
    let entity_id = format!("{folder_id}:{image_id}");
    write_change_pool(
        state,
        project_id,
        "folder_member",
        &entity_id,
        "update",
        revision,
        &payload,
    )
    .await?;
    Ok((revision, payload))
}

async fn resolve_asset_id(
    state: &AppState,
    project_id: Uuid,
    key: &str,
) -> Result<Uuid, OperationError> {
    sqlx::query(
        "SELECT id FROM assets WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(key)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .map(|row| row.get("id"))
    .ok_or_else(|| OperationError::Rejected(format!("asset not found: {key}")))
}

fn permission_for(operation: &PushOperation) -> ProjectPermission {
    match operation.entity_type.as_str() {
        "annotation" => ProjectPermission::Annotate,
        "issue" | "issue_comment" => ProjectPermission::Review,
        "folder" | "folder_member" | "project" => ProjectPermission::ManageData,
        _ => ProjectPermission::View,
    }
}

async fn write_change_pool(
    state: &AppState,
    project_id: Uuid,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    revision: i64,
    payload: &Value,
) -> Result<(), OperationError> {
    sqlx::query(
        "INSERT INTO change_events (project_id, entity_type, entity_id, operation, revision, payload) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(project_id)
    .bind(entity_type)
    .bind(entity_id)
    .bind(operation)
    .bind(revision)
    .bind(payload)
    .execute(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok(())
}

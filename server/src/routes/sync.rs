use super::{
    collaboration::{perform_review, schema_classes, ReviewRequest},
    projects::{write_audit, write_change},
    ApiError,
};
use crate::{
    auth::{
        require_deleted_project_changes, require_project_permission, AuthUser, ProjectPermission,
    },
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

#[derive(Debug, Serialize, Deserialize)]
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

enum OperationReservation {
    Claimed,
    Processing,
    Completed(Value),
}

async fn reserve_operation(
    state: &AppState,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<OperationReservation, ApiError> {
    let operation_hash = operation_fingerprint(operation);
    let mut tx = state.pool.begin().await?;
    // A stale reservation is reclaimed only while holding the project lock,
    // so event allocation and subsequent business writes remain serialized.
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    let inserted = sqlx::query(
        "INSERT INTO processed_operations (operation_id, project_id, result_json, state, claimed_at, request_hash) VALUES ($1, $2, '{}', 'processing', now(), $3) ON CONFLICT(operation_id) DO NOTHING RETURNING operation_id",
    )
    .bind(&operation.operation_id)
    .bind(project_id)
    .bind(&operation_hash)
    .fetch_optional(&mut *tx)
    .await?;
    if inserted.is_some() {
        tx.commit().await?;
        return Ok(OperationReservation::Claimed);
    }
    let row = sqlx::query(
        "SELECT project_id, state, result_json, request_hash, claimed_at FROM processed_operations WHERE operation_id = $1",
    )
    .bind(&operation.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::internal_message("operation reservation disappeared"))?;
    if row.get::<Uuid, _>("project_id") != project_id {
        tx.rollback().await?;
        return Err(ApiError::conflict(
            "OPERATION_ID_REUSED",
            "operationId belongs to another project",
            Value::Null,
        ));
    }
    if row
        .get::<Option<String>, _>("request_hash")
        .is_some_and(|hash| hash != operation_hash)
    {
        tx.rollback().await?;
        return Err(ApiError::conflict(
            "OPERATION_ID_REUSED",
            "operationId was already used with a different payload",
            Value::Null,
        ));
    }
    if row.get::<String, _>("state") == "completed" {
        let result = row.get("result_json");
        tx.commit().await?;
        Ok(OperationReservation::Completed(result))
    } else if row
        .get::<Option<chrono::DateTime<chrono::Utc>>, _>("claimed_at")
        .is_some_and(|claimed_at| claimed_at < chrono::Utc::now() - chrono::Duration::minutes(5))
    {
        let reclaimed = sqlx::query(
            "UPDATE processed_operations SET claimed_at = now(), request_hash = $3 WHERE operation_id = $1 AND project_id = $2 AND state = 'processing' AND claimed_at < now() - interval '5 minutes' RETURNING operation_id",
        )
        .bind(&operation.operation_id)
        .bind(project_id)
        .bind(&operation_hash)
        .fetch_optional(&mut *tx)
        .await?;
        tx.commit().await?;
        if reclaimed.is_some() {
            Ok(OperationReservation::Claimed)
        } else {
            Ok(OperationReservation::Processing)
        }
    } else {
        tx.commit().await?;
        Ok(OperationReservation::Processing)
    }
}

fn operation_fingerprint(operation: &PushOperation) -> String {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_vec(operation).unwrap_or_default();
    hex::encode(Sha256::digest(encoded))
}

async fn release_operation(
    state: &AppState,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(), ApiError> {
    sqlx::query(
        "DELETE FROM processed_operations WHERE operation_id = $1 AND project_id = $2 AND state = 'processing' AND (request_hash IS NULL OR request_hash = $3)",
    )
    .bind(&operation.operation_id)
    .bind(project_id)
    .bind(operation_fingerprint(operation))
    .execute(&state.pool)
    .await?;
    Ok(())
}

/// Persist the idempotency result in the same business transaction as the
/// entity mutation.  The push handler may update it again after the function
/// returns, but the in-transaction write closes the crash window between a
/// successful mutation and an acknowledgement.
pub(crate) async fn complete_operation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    operation_id: &str,
    revision: i64,
    payload: &Value,
) -> Result<(), ApiError> {
    let result = json!({
        "operationId": operation_id,
        "status": "applied",
        "serverRevision": revision,
        "remotePayload": payload
    });
    sqlx::query(
        "UPDATE processed_operations SET result_json = $3, state = 'completed', completed_at = now(), processed_at = now() WHERE operation_id = $1 AND project_id = $2",
    )
    .bind(operation_id)
    .bind(project_id)
    .bind(result)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub async fn push(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<PushRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, input.project_id, ProjectPermission::View).await?;
    if input.operations.len() > 500 {
        return Err(ApiError::bad_request("sync batch exceeds 500 operations"));
    }
    let mut results = Vec::with_capacity(input.operations.len());
    for operation in input.operations {
        match reserve_operation(&state, input.project_id, &operation).await? {
            OperationReservation::Completed(stored) => {
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "duplicate".to_string(),
                    server_revision: stored.get("serverRevision").and_then(Value::as_i64),
                    remote_payload: stored.get("remotePayload").cloned(),
                    error: None,
                });
                continue;
            }
            OperationReservation::Processing => {
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "retryable".to_string(),
                    server_revision: None,
                    remote_payload: None,
                    error: Some("operation is already being applied".to_string()),
                });
                continue;
            }
            OperationReservation::Claimed => {}
        }
        let permission = permission_for(&operation);
        if let Err((_, message)) =
            require_project_permission(&state, user.id, input.project_id, permission).await
        {
            release_operation(&state, input.project_id, &operation).await?;
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
                    "UPDATE processed_operations SET result_json = $3, state = 'completed', completed_at = now(), processed_at = now() WHERE operation_id = $1 AND project_id = $2",
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
            }) => {
                release_operation(&state, input.project_id, &operation).await?;
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "conflict".to_string(),
                    server_revision: Some(current_revision),
                    remote_payload: Some(remote),
                    error: None,
                })
            }
            Err(OperationError::Rejected(message)) => {
                release_operation(&state, input.project_id, &operation).await?;
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "rejected".to_string(),
                    server_revision: None,
                    remote_payload: None,
                    error: Some(message),
                })
            }
            Err(OperationError::Retryable(message)) => {
                release_operation(&state, input.project_id, &operation).await?;
                results.push(PushResult {
                    operation_id: operation.operation_id,
                    status: "retryable".to_string(),
                    server_revision: None,
                    remote_payload: None,
                    error: Some(message),
                })
            }
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
    match require_project_permission(&state, user.id, project_id, ProjectPermission::View).await {
        Ok(_) => {}
        Err((status, _)) if status == axum::http::StatusCode::FORBIDDEN => {
            require_deleted_project_changes(&state, user.id, project_id).await?;
        }
        Err(error) => return Err(error.into()),
    }
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
    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let project = sqlx::query(
        "SELECT id, name, description, mode, status, revision, updated_at FROM projects WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    let assets = sqlx::query(
        "SELECT id, client_key, file_name, object_key, content_hash, mime_type, width, height, byte_size, split, status, revision, updated_at, thumbnail_key, thumbnail_content_hash, thumbnail_mime_type, thumbnail_byte_size FROM assets WHERE project_id = $1 AND deleted_at IS NULL ORDER BY id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let annotations = sqlx::query(
        "SELECT id, image_id, revision, schema_version_id, object_json, status, updated_by, updated_at FROM annotations WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let issues = sqlx::query(
        "SELECT id, client_key, image_id, annotation_object_id, title, description, severity, status, reporter_id, assignee_id, revision, updated_at FROM issues WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let folders = sqlx::query(
        "SELECT id, client_key, parent_id, name, sort_order, revision, updated_at FROM folders WHERE project_id = $1 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let comments = sqlx::query(
        "SELECT c.id, c.issue_id, c.client_key, c.author_id, c.content, c.revision, c.created_at FROM issue_comments c JOIN issues i ON i.id = c.issue_id WHERE i.project_id = $1 AND c.deleted_at IS NULL ORDER BY c.created_at, c.id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let attachments = sqlx::query(
        "SELECT id, issue_id, client_key, file_name, object_key, content_hash, mime_type, byte_size, status, revision, created_by, created_at FROM issue_attachments WHERE project_id = $1 AND deleted_at IS NULL ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let members = sqlx::query(
        "SELECT user_id, role, joined_at FROM project_members WHERE project_id = $1 AND deleted_at IS NULL ORDER BY joined_at, user_id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let folder_members = sqlx::query(
        "SELECT fm.folder_id, fm.image_id, fm.revision FROM folder_members fm JOIN folders f ON f.id = fm.folder_id WHERE f.project_id = $1 AND fm.deleted_at IS NULL ORDER BY fm.folder_id, fm.image_id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let tasks = sqlx::query(
        "SELECT id, name, status, assignee_id, revision, created_by, updated_by, created_at, updated_at FROM tasks WHERE project_id = $1 AND deleted_at IS NULL ORDER BY updated_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let task_items = sqlx::query(
        "SELECT ti.id, ti.task_id, ti.image_id, a.client_key, ti.status, ti.assignee_id, ti.claimed_at, ti.revision, ti.updated_at FROM task_items ti JOIN tasks t ON t.id = ti.task_id JOIN assets a ON a.id = ti.image_id WHERE t.project_id = $1 AND ti.deleted_at IS NULL ORDER BY ti.task_id, ti.id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let qa_reviews = sqlx::query(
        "SELECT id, image_id, annotation_revision, decision, note, reviewer_id, operation_id, created_at FROM qa_reviews WHERE project_id = $1 ORDER BY created_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let label_schemas = sqlx::query(
        "SELECT id, name, schema_json, revision, created_by, updated_by, created_at, updated_at FROM label_schemas WHERE project_id = $1 AND deleted_at IS NULL ORDER BY updated_at, id",
    )
    .bind(project_id)
    .fetch_all(&mut *tx)
    .await?;
    let max_sequence = sqlx::query(
        "SELECT COALESCE(MAX(sequence), 0) AS sequence FROM change_events WHERE project_id = $1",
    )
    .bind(project_id)
    .fetch_one(&mut *tx)
    .await?
    .get::<i64, _>("sequence");
    let response = json!({
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
            "thumbnailKey": row.get::<Option<String>, _>("thumbnail_key"),
            "thumbnailContentHash": row.get::<Option<String>, _>("thumbnail_content_hash"),
            "thumbnailMimeType": row.get::<Option<String>, _>("thumbnail_mime_type"),
            "thumbnailByteSize": row.get::<Option<i64>, _>("thumbnail_byte_size"),
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
            "clientKey": row.get::<Option<String>, _>("client_key"),
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
        "tasks": tasks.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "projectId": project_id,
            "name": row.get::<String, _>("name"),
            "status": row.get::<String, _>("status"),
            "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
            "revision": row.get::<i64, _>("revision"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "updatedBy": row.get::<Uuid, _>("updated_by"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "taskItems": task_items.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "taskId": row.get::<Uuid, _>("task_id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "clientImageId": row.get::<Option<String>, _>("client_key"),
            "status": row.get::<String, _>("status"),
            "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
            "claimedAt": row.get::<Option<chrono::DateTime<chrono::Utc>>, _>("claimed_at"),
            "revision": row.get::<i64, _>("revision"),
            "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "qaReviews": qa_reviews.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "annotationRevision": row.get::<i64, _>("annotation_revision"),
            "decision": row.get::<String, _>("decision"),
            "note": row.get::<String, _>("note"),
            "reviewerId": row.get::<Uuid, _>("reviewer_id"),
            "operationId": row.get::<Option<String>, _>("operation_id"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
        })).collect::<Vec<_>>(),
        "labelSchemas": label_schemas.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "projectId": project_id,
            "name": row.get::<String, _>("name"),
            "schemaJson": row.get::<Value, _>("schema_json"),
            "schema": row.get::<Value, _>("schema_json"),
            "classes": schema_classes(&row.get::<Value, _>("schema_json")),
            "revision": row.get::<i64, _>("revision"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "updatedBy": row.get::<Uuid, _>("updated_by"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "labelSchema": label_schemas.last().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "projectId": project_id,
            "name": row.get::<String, _>("name"),
            "schemaJson": row.get::<Value, _>("schema_json"),
            "schema": row.get::<Value, _>("schema_json"),
            "classes": schema_classes(&row.get::<Value, _>("schema_json")),
            "revision": row.get::<i64, _>("revision"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "updatedBy": row.get::<Uuid, _>("updated_by"),
            "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
            "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        })),
        "folderMembers": folder_members.iter().map(|row| json!({
            "folderId": row.get::<Uuid, _>("folder_id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "revision": row.get::<i64, _>("revision")
        })).collect::<Vec<_>>()
    });
    tx.commit().await?;
    Ok(Json(response))
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
        ("qa_review", "transition")
        | ("qa_review", "review")
        | ("review", "transition")
        | ("review", "review") => apply_qa_review(state, user, project_id, operation).await,
        ("task_item", "claim") | ("task_item", "release") => {
            apply_task_item(state, user, project_id, operation).await
        }
        ("task", "create") => apply_task_create(state, user, project_id, operation).await,
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
        ("project", "update") => apply_project_update(state, user, project_id, operation).await,
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
    lock_project_tx(&mut tx, project_id).await?;
    let current = sqlx::query(
        "SELECT id, image_id, revision, object_json, status FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let current_revision = current
        .as_ref()
        .map(|row| row.get("revision"))
        .unwrap_or(0_i64);
    if operation.base_revision.unwrap_or(0) != current_revision {
        let remote = current
            .as_ref()
            .map(|row| {
                json!({
                    "id": row.get::<Uuid, _>("id"),
                    "imageId": row.get::<Uuid, _>("image_id"),
                    "revision": row.get::<i64, _>("revision"),
                    "objects": row.get::<Value, _>("object_json"),
                    "status": row.get::<String, _>("status")
                })
            })
            .unwrap_or_else(|| {
                json!({
                    "id": Value::Null,
                    "imageId": image_id,
                    "revision": 0,
                    "objects": [],
                    "status": "draft"
                })
            });
        return Err(OperationError::Conflict {
            current_revision,
            remote,
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
    complete_operation_tx(
        &mut tx,
        project_id,
        &operation.operation_id,
        revision,
        &payload,
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
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
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let current = sqlx::query(
        "SELECT id, image_id, revision, object_json, status FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("annotation not found".to_string()))?;
    let current_revision: i64 = current.get("revision");
    if operation
        .base_revision
        .is_some_and(|base| base != current_revision)
    {
        return Err(OperationError::Conflict {
            current_revision,
            remote: json!({
                "id": current.get::<Uuid, _>("id"),
                "imageId": image_id,
                "revision": current_revision,
                "objects": current.get::<Value, _>("object_json"),
                "status": current.get::<String, _>("status")
            }),
        });
    }
    let revision = current_revision + 1;
    let row = sqlx::query(
        "UPDATE annotations SET status = 'submitted', revision = $3, updated_by = $4, updated_at = now() WHERE id = $1 AND project_id = $2 RETURNING id, image_id, revision, object_json, status",
    )
    .bind(current.get::<Uuid, _>("id"))
    .bind(project_id)
    .bind(revision)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    sqlx::query(
        "UPDATE assets SET status = 'pending_review', revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 AND project_id = $2",
    )
    .bind(image_id)
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let payload = json!({
        "id": row.get::<Uuid, _>("id"),
        "imageId": image_id,
        "clientImageId": operation.entity_id,
        "revision": row.get::<i64, _>("revision"),
        "objects": row.get::<Value, _>("object_json"),
        "status": row.get::<String, _>("status")
    });
    write_change(
        &mut tx,
        project_id,
        "annotation",
        &operation.entity_id,
        "submit",
        revision,
        payload.clone(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "annotation.submit",
        "annotation",
        &row.get::<Uuid, _>("id").to_string(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    complete_operation_tx(
        &mut tx,
        project_id,
        &operation.operation_id,
        revision,
        &payload,
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn apply_qa_review(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let image_key = operation
        .payload
        .get("imageId")
        .and_then(Value::as_str)
        .unwrap_or(&operation.entity_id);
    let image_id = resolve_asset_id(state, project_id, image_key).await?;
    let decision = operation
        .payload
        .get("decision")
        .or_else(|| operation.payload.get("status"))
        .and_then(Value::as_str)
        .unwrap_or("rejected")
        .to_string();
    let annotation_revision = operation.base_revision.or_else(|| {
        operation
            .payload
            .get("annotationRevision")
            .and_then(Value::as_i64)
    });
    let request = ReviewRequest {
        decision,
        note: operation
            .payload
            .get("note")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        annotation_revision,
        operation_id: Some(operation.operation_id.clone()),
        issue_client_key: operation
            .payload
            .get("issueClientKey")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    };
    match perform_review(state, user, project_id, image_id, &request).await {
        Ok(payload) => Ok((
            payload
                .get("revision")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            payload,
        )),
        Err(error) if error.code == "ANNOTATION_REVISION_CONFLICT" => {
            let current_revision = error
                .details
                .get("currentRevision")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let remote = error
                .details
                .get("remoteAnnotation")
                .cloned()
                .unwrap_or_else(|| error.details.clone());
            Err(OperationError::Conflict {
                current_revision,
                remote,
            })
        }
        Err(error) if error.status.is_client_error() => {
            Err(OperationError::Rejected(error.message))
        }
        Err(error) => Err(OperationError::Retryable(error.message)),
    }
}

async fn apply_task_item(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let task_id = operation
        .payload
        .get("taskId")
        .and_then(Value::as_str)
        .or_else(|| operation.entity_id.split(':').next())
        .and_then(|value| Uuid::parse_str(value).ok())
        .ok_or_else(|| OperationError::Rejected("taskId is required".to_string()))?;
    let image_key = operation
        .payload
        .get("imageId")
        .and_then(Value::as_str)
        .or_else(|| operation.entity_id.split(':').nth(1))
        .ok_or_else(|| OperationError::Rejected("imageId is required".to_string()))?;
    let image_id = resolve_asset_id(state, project_id, image_key).await?;
    if let Some(base) = operation.base_revision {
        if let Some(row) = sqlx::query(
            "SELECT ti.id, ti.task_id, ti.image_id, ti.status, ti.assignee_id, ti.revision FROM task_items ti JOIN tasks t ON t.id = ti.task_id WHERE t.project_id = $1 AND ti.task_id = $2 AND ti.image_id = $3 AND ti.deleted_at IS NULL",
        )
        .bind(project_id)
        .bind(task_id)
        .bind(image_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?
        {
            let current: i64 = row.get("revision");
            if current != base {
                return Err(OperationError::Conflict {
                    current_revision: current,
                    remote: json!({
                        "id": row.get::<Uuid, _>("id"),
                        "taskId": task_id,
                        "imageId": image_id,
                        "status": row.get::<String, _>("status"),
                        "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
                        "revision": current,
                        "objects": []
                    }),
                });
            }
        }
    }
    let role = sqlx::query_scalar::<_, String>(
        "SELECT role FROM project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .unwrap_or_else(|| "annotator".to_string());
    let result = if operation.operation == "claim" {
        super::collaboration::claim_task_item(
            state,
            user,
            project_id,
            task_id,
            image_id,
            &role,
            operation.base_revision,
            Some(&operation.operation_id),
        )
        .await
    } else {
        super::collaboration::release_task_item(
            state,
            user,
            project_id,
            task_id,
            image_id,
            &role,
            operation.base_revision,
            Some(&operation.operation_id),
        )
        .await
    };
    match result {
        Ok(payload) => Ok((
            payload
                .get("revision")
                .and_then(Value::as_i64)
                .unwrap_or_default(),
            payload,
        )),
        Err(error) if error.status == axum::http::StatusCode::CONFLICT => {
            Err(OperationError::Conflict {
                current_revision: error
                    .details
                    .get("revision")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                remote: error.details,
            })
        }
        Err(error) if error.status.is_client_error() => {
            Err(OperationError::Rejected(error.message))
        }
        Err(error) => Err(OperationError::Retryable(error.message)),
    }
}

async fn apply_task_create(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let name = operation
        .payload
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("移动任务");
    if name.trim().is_empty() {
        return Err(OperationError::Rejected(
            "task name is required".to_string(),
        ));
    }
    let status = operation
        .payload
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unassigned");
    let task_id = Uuid::parse_str(&operation.entity_id).unwrap_or_else(|_| Uuid::new_v4());
    let assignee = operation
        .payload
        .get("assigneeId")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let image_ids = operation
        .payload
        .get("imageIds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let task = sqlx::query(
        "INSERT INTO tasks (id, project_id, name, status, assignee_id, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $6) ON CONFLICT(id) DO UPDATE SET name = excluded.name, status = excluded.status, assignee_id = excluded.assignee_id, updated_by = excluded.updated_by, updated_at = now() RETURNING id, revision",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(name.trim())
    .bind(status)
    .bind(assignee)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let mut item_ids = Vec::new();
    for image in image_ids {
        let key = image
            .as_str()
            .ok_or_else(|| OperationError::Rejected("imageIds must be strings".to_string()))?;
        let row = sqlx::query(
            "SELECT id FROM assets WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
        )
        .bind(project_id)
        .bind(key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?
        .ok_or_else(|| OperationError::Rejected(format!("asset not found: {key}")))?;
        let image_id: Uuid = row.get("id");
        let item = sqlx::query(
            "INSERT INTO task_items (task_id, image_id, status, assignee_id, updated_by) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(task_id, image_id) DO UPDATE SET deleted_at = NULL, status = excluded.status, assignee_id = excluded.assignee_id, updated_by = excluded.updated_by, updated_at = now() RETURNING id",
        )
        .bind(task_id)
        .bind(image_id)
        .bind(status)
        .bind(assignee)
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
        item_ids.push(item.get::<Uuid, _>("id"));
    }
    let revision: i64 = task.get("revision");
    let payload = json!({
        "id": task_id,
        "projectId": project_id,
        "name": name,
        "status": status,
        "assigneeId": assignee,
        "itemIds": item_ids,
        "revision": revision
    });
    write_change(
        &mut tx,
        project_id,
        "task",
        &operation.entity_id,
        "create",
        revision,
        payload.clone(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "task.create",
        "task",
        &task_id.to_string(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    complete_operation_tx(
        &mut tx,
        project_id,
        &operation.operation_id,
        revision,
        &payload,
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn lock_project_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
) -> Result<(), OperationError> {
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project_id)
        .execute(&mut **tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok(())
}

async fn finish_operation_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    user_id: Uuid,
    audit_action: &str,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    revision: i64,
    operation_id: &str,
    payload: &Value,
) -> Result<(), OperationError> {
    write_change(
        tx,
        project_id,
        entity_type,
        entity_id,
        operation,
        revision,
        payload.clone(),
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    write_audit(
        tx,
        project_id,
        user_id,
        audit_action,
        entity_type,
        entity_id,
    )
    .await
    .map_err(|error| OperationError::Retryable(error.message))?;
    complete_operation_tx(tx, project_id, operation_id, revision, payload)
        .await
        .map_err(|error| OperationError::Retryable(error.message))?;
    Ok(())
}

async fn complete_only_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: Uuid,
    operation_id: &str,
    revision: i64,
    payload: &Value,
) -> Result<(), OperationError> {
    complete_operation_tx(tx, project_id, operation_id, revision, payload)
        .await
        .map_err(|error| OperationError::Retryable(error.message))
}

async fn apply_project_update(
    _state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let mode = operation.payload.get("mode").and_then(Value::as_str);
    if mode.is_some_and(|value| !matches!(value, "local_only" | "cloud_linked" | "mirrored")) {
        return Err(OperationError::Rejected("invalid project mode".to_string()));
    }
    let status = operation.payload.get("status").and_then(Value::as_str);
    if status.is_some_and(|value| !matches!(value, "draft" | "active" | "archived" | "deleted")) {
        return Err(OperationError::Rejected(
            "invalid project status".to_string(),
        ));
    }
    let mut tx = _state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let current = sqlx::query(
        "SELECT id, name, description, mode, status, revision FROM projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("project not found".to_string()))?;
    let current_revision: i64 = current.get("revision");
    if operation
        .base_revision
        .is_some_and(|base| base != current_revision)
    {
        return Err(OperationError::Conflict {
            current_revision,
            remote: json!({
                "id": project_id,
                "name": current.get::<String, _>("name"),
                "description": current.get::<String, _>("description"),
                "mode": current.get::<String, _>("mode"),
                "status": current.get::<String, _>("status"),
                "revision": current_revision
            }),
        });
    }
    let updated = sqlx::query(
        "UPDATE projects SET mode = COALESCE($2, mode), status = COALESCE($3, status), name = COALESCE($4, name), description = COALESCE($5, description), revision = revision + 1, updated_by = $6, updated_at = now() WHERE id = $1 RETURNING id, name, description, mode, status, revision, updated_by, updated_at",
    )
    .bind(project_id)
    .bind(mode)
    .bind(status)
    .bind(operation.payload.get("name").and_then(Value::as_str))
    .bind(operation.payload.get("description").and_then(Value::as_str))
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let revision: i64 = updated.get("revision");
    let payload = json!({
        "id": updated.get::<Uuid, _>("id"),
        "name": updated.get::<String, _>("name"),
        "description": updated.get::<String, _>("description"),
        "mode": updated.get::<String, _>("mode"),
        "status": updated.get::<String, _>("status"),
        "revision": revision,
        "updatedBy": updated.get::<Uuid, _>("updated_by"),
        "updatedAt": updated.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
    });
    finish_operation_tx(
        &mut tx,
        project_id,
        user.id,
        "project.update",
        "project",
        &project_id.to_string(),
        &operation.operation,
        revision,
        &operation.operation_id,
        &payload,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
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
    let title = operation
        .payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("本地缺陷");
    let description = operation
        .payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("");
    let severity = operation
        .payload
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("major");
    let source = operation
        .payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("sync");
    let annotation_object_id = operation
        .payload
        .get("annotationObjectId")
        .and_then(Value::as_str);
    let assignee_id = operation
        .payload
        .get("assigneeId")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value).ok());
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let row = sqlx::query(
        "INSERT INTO issues (id, project_id, client_key, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'open', $9, $10, $11) ON CONFLICT(project_id, client_key) WHERE client_key IS NOT NULL DO NOTHING RETURNING id, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id, revision",
    )
    .bind(Uuid::new_v4())
    .bind(project_id)
    .bind(&operation.entity_id)
    .bind(image_id)
    .bind(annotation_object_id)
    .bind(title)
    .bind(description)
    .bind(severity)
    .bind(source)
    .bind(user.id)
    .bind(assignee_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let inserted = row.is_some();
    let row = if let Some(row) = row {
        row
    } else {
        sqlx::query(
            "SELECT id, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id, revision FROM issues WHERE project_id = $1 AND client_key = $2 AND deleted_at IS NULL",
        )
        .bind(project_id)
        .bind(&operation.entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?
        .ok_or_else(|| OperationError::Retryable("issue insert disappeared".to_string()))?
    };
    let id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    let payload = json!({
        "id": id,
        "clientKey": operation.entity_id,
        "imageId": row.get::<Uuid, _>("image_id"),
        "clientImageId": image_key,
        "annotationObjectId": row.get::<Option<String>, _>("annotation_object_id"),
        "title": row.get::<String, _>("title"),
        "description": row.get::<String, _>("description"),
        "severity": row.get::<String, _>("severity"),
        "status": row.get::<String, _>("status"),
        "source": row.get::<String, _>("source"),
        "reporterId": row.get::<Uuid, _>("reporter_id"),
        "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
        "revision": revision
    });
    if inserted {
        finish_operation_tx(
            &mut tx,
            project_id,
            user.id,
            "issue.create",
            "issue",
            &id.to_string(),
            "create",
            revision,
            &operation.operation_id,
            &payload,
        )
        .await?;
    } else {
        complete_only_tx(
            &mut tx,
            project_id,
            &operation.operation_id,
            revision,
            &payload,
        )
        .await?;
    }
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn apply_issue_update(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let row = sqlx::query(
        "SELECT id, revision, status, title, description, severity, image_id, reporter_id, assignee_id, client_key FROM issues WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(&operation.entity_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("issue not found".to_string()))?;
    let revision: i64 = row.get("revision");
    let id: Uuid = row.get("id");
    if operation.base_revision.is_some_and(|base| base != revision) {
        return Err(OperationError::Conflict {
            current_revision: revision,
            remote: json!({
                "id": id,
                "clientKey": row.get::<Option<String>, _>("client_key"),
                "imageId": row.get::<Uuid, _>("image_id"),
                "status": row.get::<String, _>("status"),
                "title": row.get::<String, _>("title"),
                "description": row.get::<String, _>("description"),
                "severity": row.get::<String, _>("severity"),
                "reporterId": row.get::<Uuid, _>("reporter_id"),
                "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
                "revision": revision
            }),
        });
    }
    let status = operation
        .payload
        .get("status")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| row.get::<String, _>("status"));
    let updated = sqlx::query(
        "UPDATE issues SET status = $2, title = COALESCE($3, title), description = COALESCE($4, description), severity = COALESCE($5, severity), revision = revision + 1, updated_at = now() WHERE id = $1 RETURNING id, revision, status, title, description, severity, image_id, reporter_id, assignee_id, client_key",
    )
    .bind(id)
    .bind(&status)
    .bind(operation.payload.get("title").and_then(Value::as_str))
    .bind(operation.payload.get("description").and_then(Value::as_str))
    .bind(operation.payload.get("severity").and_then(Value::as_str))
    .fetch_one(&mut *tx)
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
    finish_operation_tx(
        &mut tx,
        project_id,
        user.id,
        if operation.operation == "transition" {
            "issue.transition"
        } else {
            "issue.update"
        },
        "issue",
        &id.to_string(),
        &operation.operation,
        next_revision,
        &operation.operation_id,
        &payload,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
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
    let content = operation
        .payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    let inserted_row = sqlx::query(
        "INSERT INTO issue_comments (id, issue_id, client_key, author_id, content) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(issue_id, client_key) WHERE client_key IS NOT NULL DO NOTHING RETURNING id, revision, created_at, author_id, content",
    )
    .bind(Uuid::new_v4())
    .bind(issue_id)
    .bind(&operation.entity_id)
    .bind(user.id)
    .bind(content)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let inserted = inserted_row.is_some();
    let row = if let Some(row) = inserted_row {
        row
    } else {
        sqlx::query(
            "SELECT id, revision, created_at, author_id, content FROM issue_comments WHERE issue_id = $1 AND client_key = $2 AND deleted_at IS NULL",
        )
        .bind(issue_id)
        .bind(&operation.entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?
        .ok_or_else(|| OperationError::Retryable("issue comment insert disappeared".to_string()))?
    };
    let id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    let payload = json!({
        "id": id,
        "clientKey": operation.entity_id,
        "issueId": issue_id,
        "authorId": row.get::<Uuid, _>("author_id"),
        "content": row.get::<String, _>("content"),
        "revision": revision,
        "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at")
    });
    if inserted {
        finish_operation_tx(
            &mut tx,
            project_id,
            user.id,
            "issue.comment",
            "issue_comment",
            &id.to_string(),
            "comment",
            revision,
            &operation.operation_id,
            &payload,
        )
        .await?;
    } else {
        complete_only_tx(
            &mut tx,
            project_id,
            &operation.operation_id,
            revision,
            &payload,
        )
        .await?;
    }
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn apply_folder(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    operation: &PushOperation,
) -> Result<(i64, Value), OperationError> {
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    if operation.operation == "create" {
        let existing = sqlx::query(
            "SELECT id, revision, name, sort_order FROM folders WHERE project_id = $1 AND client_key = $2 FOR UPDATE",
        )
        .bind(project_id)
        .bind(&operation.entity_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
        let inserted = existing.is_none();
        let row = if let Some(row) = existing {
            sqlx::query("UPDATE folders SET deleted_at = NULL, updated_at = now(), updated_by = $2 WHERE id = $1 RETURNING id, revision, name, sort_order")
                .bind(row.get::<Uuid, _>("id"))
                .bind(user.id)
                .fetch_one(&mut *tx)
                .await
                .map_err(|error| OperationError::Retryable(error.to_string()))?
        } else {
            sqlx::query(
                "INSERT INTO folders (id, project_id, client_key, name, sort_order, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING id, revision, name, sort_order",
            )
            .bind(Uuid::new_v4())
            .bind(project_id)
            .bind(&operation.entity_id)
            .bind(operation.payload.get("name").and_then(Value::as_str).unwrap_or("文件夹"))
            .bind(operation.payload.get("sortOrder").and_then(Value::as_i64).unwrap_or(0) as i32)
            .bind(user.id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| OperationError::Retryable(error.to_string()))?
        };
        let id: Uuid = row.get("id");
        let revision: i64 = row.get("revision");
        let payload = json!({
            "id": id,
            "clientKey": operation.entity_id,
            "name": row.get::<String, _>("name"),
            "sortOrder": row.get::<i32, _>("sort_order"),
            "revision": revision
        });
        if inserted {
            finish_operation_tx(
                &mut tx,
                project_id,
                user.id,
                "folder.create",
                "folder",
                &id.to_string(),
                "create",
                revision,
                &operation.operation_id,
                &payload,
            )
            .await?;
        } else {
            complete_only_tx(
                &mut tx,
                project_id,
                &operation.operation_id,
                revision,
                &payload,
            )
            .await?;
        }
        tx.commit()
            .await
            .map_err(|error| OperationError::Retryable(error.to_string()))?;
        return Ok((revision, payload));
    }
    let row = sqlx::query(
        "SELECT id, client_key, revision, name, sort_order FROM folders WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(&operation.entity_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?
    .ok_or_else(|| OperationError::Rejected("folder not found".to_string()))?;
    let id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    if operation.base_revision.is_some_and(|base| base != revision) {
        return Err(OperationError::Conflict {
            current_revision: revision,
            remote: json!({
                "id": id,
                "clientKey": row.get::<Option<String>, _>("client_key"),
                "name": row.get::<String, _>("name"),
                "sortOrder": row.get::<i32, _>("sort_order"),
                "revision": revision
            }),
        });
    }
    let updated = if operation.operation == "delete" {
        sqlx::query(
            "UPDATE folders SET deleted_at = now(), revision = revision + 1, updated_by = $2, updated_at = now() WHERE id = $1 RETURNING revision, name, sort_order",
        )
        .bind(id)
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await
    } else {
        sqlx::query(
            "UPDATE folders SET name = COALESCE($2, name), revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 RETURNING revision, name, sort_order",
        )
        .bind(id)
        .bind(operation.payload.get("name").and_then(Value::as_str))
        .bind(user.id)
        .fetch_one(&mut *tx)
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
    finish_operation_tx(
        &mut tx,
        project_id,
        user.id,
        if operation.operation == "delete" {
            "folder.delete"
        } else {
            "folder.update"
        },
        "folder",
        &id.to_string(),
        &operation.operation,
        revision,
        &operation.operation_id,
        &payload,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    Ok((revision, payload))
}

async fn apply_folder_member(
    state: &AppState,
    user: &AuthUser,
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
    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
    lock_project_tx(&mut tx, project_id).await?;
    if let Some(base) = operation.base_revision {
        if let Some(current) = sqlx::query(
            "SELECT revision FROM folder_members WHERE folder_id = $1 AND image_id = $2 FOR UPDATE",
        )
        .bind(folder_id)
        .bind(image_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?
        {
            let current_revision: i64 = current.get("revision");
            if current_revision != base {
                return Err(OperationError::Conflict {
                    current_revision,
                    remote: json!({
                        "folderId": folder_id,
                        "imageId": image_id,
                        "revision": current_revision
                    }),
                });
            }
        }
    }
    let row = sqlx::query(
        "INSERT INTO folder_members (folder_id, image_id, revision) VALUES ($1, $2, 1) ON CONFLICT(folder_id, image_id) DO UPDATE SET revision = folder_members.revision + 1, deleted_at = NULL RETURNING revision",
    )
    .bind(folder_id)
    .bind(image_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|error| OperationError::Retryable(error.to_string()))?;
    let revision: i64 = row.get("revision");
    let payload = json!({
        "folderId": folder_id,
        "imageId": image_id,
        "revision": revision
    });
    let entity_id = format!("{folder_id}:{image_id}");
    finish_operation_tx(
        &mut tx,
        project_id,
        user.id,
        "folder.member.update",
        "folder_member",
        &entity_id,
        "update",
        revision,
        &operation.operation_id,
        &payload,
    )
    .await?;
    tx.commit()
        .await
        .map_err(|error| OperationError::Retryable(error.to_string()))?;
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
        "qa_review" | "review" => ProjectPermission::Review,
        "task_item" => ProjectPermission::TaskClaim,
        "task" => ProjectPermission::ManageData,
        "issue" => {
            if operation.operation == "create" {
                ProjectPermission::IssueWrite
            } else {
                ProjectPermission::Review
            }
        }
        "issue_comment" => ProjectPermission::IssueWrite,
        "folder" | "folder_member" | "project" => ProjectPermission::ManageData,
        _ => ProjectPermission::View,
    }
}

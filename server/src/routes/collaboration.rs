//! Project collaboration primitives shared by the REST and sync surfaces.
//!
//! The mobile client deliberately uses coarse grained operations (claim a task
//! item, submit an image for review, approve/reject a review).  Each operation
//! is committed together with its change event so a polling client can recover
//! from a lost response without observing a half-written state.

use super::{projects::write_audit, projects::write_change, ApiError};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateTaskRequest {
    pub name: String,
    #[serde(default = "default_task_status")]
    pub status: String,
    #[serde(default)]
    pub image_ids: Vec<String>,
    pub assignee_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReviewRequest {
    pub decision: String,
    #[serde(default)]
    pub note: String,
    pub annotation_revision: Option<i64>,
    pub operation_id: Option<String>,
    pub issue_client_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaWriteRequest {
    pub classes: Option<Vec<Value>>,
    pub schema: Option<Value>,
    pub base_revision: Option<i64>,
}

fn default_task_status() -> String {
    "unassigned".to_string()
}

pub async fn list_tasks(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        "SELECT id, name, status, assignee_id, revision, created_by, updated_by, created_at, updated_at FROM tasks WHERE project_id = $1 AND deleted_at IS NULL ORDER BY updated_at DESC, id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(task_json).collect::<Vec<_>>()
    })))
}

pub async fn create_task(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<CreateTaskRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData).await?;
    if input.name.trim().is_empty() {
        return Err(ApiError::bad_request("task name is required"));
    }
    if !valid_task_status(&input.status) {
        return Err(ApiError::bad_request("invalid task status"));
    }
    let mut tx = state.pool.begin().await?;
    let task_id = Uuid::new_v4();
    let task = sqlx::query(
        "INSERT INTO tasks (id, project_id, name, status, assignee_id, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $6) RETURNING id, name, status, assignee_id, revision, created_by, updated_by, created_at, updated_at",
    )
    .bind(task_id)
    .bind(project_id)
    .bind(input.name.trim())
    .bind(&input.status)
    .bind(input.assignee_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;

    let mut item_ids = Vec::new();
    for image_key in &input.image_ids {
        let image = sqlx::query(
            "SELECT id FROM assets WHERE project_id = $1 AND (id::text = $2 OR client_key = $2) AND deleted_at IS NULL",
        )
        .bind(project_id)
        .bind(image_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found(format!("asset not found: {image_key}")))?;
        let image_id: Uuid = image.get("id");
        let item = sqlx::query(
            "INSERT INTO task_items (task_id, image_id, status, assignee_id, updated_by) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(task_id, image_id) DO UPDATE SET deleted_at = NULL, status = excluded.status, assignee_id = excluded.assignee_id, updated_by = excluded.updated_by, updated_at = now() RETURNING id",
        )
        .bind(task_id)
        .bind(image_id)
        .bind(&input.status)
        .bind(input.assignee_id)
        .bind(user.id)
        .fetch_one(&mut *tx)
        .await?;
        item_ids.push(item.get::<Uuid, _>("id"));
    }
    let task_revision: i64 = task.get("revision");
    let payload = json!({
        "id": task_id,
        "projectId": project_id,
        "name": input.name.trim(),
        "status": input.status,
        "assigneeId": input.assignee_id,
        "itemIds": item_ids,
        "revision": task_revision
    });
    write_change(
        &mut tx,
        project_id,
        "task",
        &task_id.to_string(),
        "create",
        task_revision,
        payload.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "task.create",
        "task",
        &task_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(payload))
}

pub async fn list_task_items(
    State(state): State<AppState>,
    user: AuthUser,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let project_id = task_project(&state, task_id).await?;
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        "SELECT ti.id, ti.task_id, ti.image_id, a.client_key, ti.status, ti.assignee_id, ti.claimed_at, ti.revision, ti.updated_at FROM task_items ti JOIN assets a ON a.id = ti.image_id WHERE ti.task_id = $1 AND ti.deleted_at IS NULL ORDER BY ti.created_at, ti.id",
    )
    .bind(task_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "taskId": row.get::<Uuid, _>("task_id"),
            "imageId": row.get::<Uuid, _>("image_id"),
            "clientImageId": row.get::<Option<String>, _>("client_key"),
            "status": row.get::<String, _>("status"),
            "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
            "claimedAt": row.get::<Option<chrono::DateTime<Utc>>, _>("claimed_at"),
            "revision": row.get::<i64, _>("revision"),
            "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn claim(
    State(state): State<AppState>,
    user: AuthUser,
    Path((task_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let project_id = task_project(&state, task_id).await?;
    let role =
        require_project_permission(&state, user.id, project_id, ProjectPermission::TaskClaim)
            .await?;
    let payload = claim_task_item(
        &state, &user, project_id, task_id, image_id, &role, None, None,
    )
    .await?;
    Ok(Json(payload))
}

pub async fn release(
    State(state): State<AppState>,
    user: AuthUser,
    Path((task_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let project_id = task_project(&state, task_id).await?;
    let role =
        require_project_permission(&state, user.id, project_id, ProjectPermission::TaskClaim)
            .await?;
    let payload = release_task_item(
        &state, &user, project_id, task_id, image_id, &role, None, None,
    )
    .await?;
    Ok(Json(payload))
}

pub async fn review_queue(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    let rows = sqlx::query(
        "SELECT a.id AS image_id, a.client_key, a.file_name, a.status AS asset_status, a.revision AS asset_revision, an.id AS annotation_id, an.revision AS annotation_revision, an.object_json, an.status AS annotation_status, an.updated_at FROM assets a LEFT JOIN annotations an ON an.image_id = a.id AND an.deleted_at IS NULL WHERE a.project_id = $1 AND a.deleted_at IS NULL AND (a.status IN ('pending_review', 'submitted') OR an.status = 'submitted') ORDER BY a.updated_at, a.id",
    )
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
            "imageId": row.get::<Uuid, _>("image_id"),
            "clientImageId": row.get::<Option<String>, _>("client_key"),
            "fileName": row.get::<String, _>("file_name"),
            "assetStatus": row.get::<String, _>("asset_status"),
            "assetRevision": row.get::<i64, _>("asset_revision"),
            "annotationId": row.get::<Option<Uuid>, _>("annotation_id"),
            "annotationRevision": row.get::<Option<i64>, _>("annotation_revision"),
            "objects": row.get::<Option<Value>, _>("object_json").unwrap_or_else(|| json!([])),
            "annotationStatus": row.get::<Option<String>, _>("annotation_status"),
            "updatedAt": row.get::<Option<chrono::DateTime<Utc>>, _>("updated_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn review(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<ReviewRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    let result = perform_review(&state, &user, project_id, image_id, &input).await?;
    Ok(Json(result))
}

pub async fn get_schema(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::SchemaView).await?;
    let row = sqlx::query(
        "SELECT ls.id, ls.name, ls.schema_json, ls.revision, ls.updated_at FROM projects p JOIN label_schemas ls ON ls.project_id = p.id AND ls.deleted_at IS NULL WHERE p.id = $1 AND p.deleted_at IS NULL AND (p.label_schema_id = ls.id OR NOT EXISTS (SELECT 1 FROM label_schemas latest WHERE latest.project_id = p.id AND latest.deleted_at IS NULL AND latest.updated_at > ls.updated_at)) ORDER BY ls.updated_at DESC LIMIT 1",
    )
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("label schema not found"))?;
    let schema_json: Value = row.get("schema_json");
    let classes = schema_classes(&schema_json);
    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "projectId": project_id,
        "name": row.get::<String, _>("name"),
        "schemaJson": schema_json,
        "schema": schema_json,
        "classes": classes,
        "revision": row.get::<i64, _>("revision"),
        "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
    })))
}

pub async fn put_schema(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<SchemaWriteRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData).await?;
    let schema_json = match (input.schema, input.classes) {
        (Some(schema), _) if schema.is_object() || schema.is_array() => schema,
        (None, Some(classes)) => json!({ "classes": classes }),
        _ => return Err(ApiError::bad_request("schema or classes is required")),
    };
    let classes = schema_classes(&schema_json);
    let mut tx = state.pool.begin().await?;
    let project = sqlx::query(
        "SELECT label_schema_id FROM projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("project not found"))?;
    let existing_id: Option<Uuid> = project.get("label_schema_id");
    let existing = if let Some(schema_id) = existing_id {
        sqlx::query(
            "SELECT id, revision, name, schema_json FROM label_schemas WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(schema_id)
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
    } else {
        sqlx::query(
            "SELECT id, revision, name, schema_json FROM label_schemas WHERE project_id = $1 AND deleted_at IS NULL ORDER BY updated_at DESC, id DESC LIMIT 1 FOR UPDATE",
        )
        .bind(project_id)
        .fetch_optional(&mut *tx)
        .await?
    };
    if let (Some(expected), Some(row)) = (input.base_revision, existing.as_ref()) {
        let current: i64 = row.get("revision");
        if expected != current {
            let remote_schema: Value = row.get("schema_json");
            return Err(ApiError::conflict(
                "LABEL_SCHEMA_REVISION_CONFLICT",
                "label schema revision has changed",
                json!({
                    "baseRevision": expected,
                    "currentRevision": current,
                    "remoteSchema": {
                        "id": row.get::<Uuid, _>("id"),
                        "revision": current,
                        "schema": remote_schema,
                        "classes": schema_classes(&remote_schema)
                    }
                }),
            ));
        }
    }
    let schema_id = existing
        .as_ref()
        .map(|row| row.get::<Uuid, _>("id"))
        .unwrap_or_else(Uuid::new_v4);
    let revision = existing
        .as_ref()
        .map(|row| row.get::<i64, _>("revision") + 1)
        .unwrap_or(1);
    let name = existing
        .as_ref()
        .map(|row| row.get::<String, _>("name"))
        .unwrap_or_else(|| "默认标签 schema".to_string());
    sqlx::query(
        "INSERT INTO label_schemas (id, project_id, name, schema_json, revision, created_by, updated_by) VALUES ($1, $2, $3, $4, $5, $6, $6) ON CONFLICT(id) DO UPDATE SET schema_json = excluded.schema_json, revision = excluded.revision, updated_by = excluded.updated_by, updated_at = now(), deleted_at = NULL",
    )
    .bind(schema_id)
    .bind(project_id)
    .bind(&name)
    .bind(&schema_json)
    .bind(revision)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE projects SET label_schema_id = $2, revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1")
        .bind(project_id)
        .bind(schema_id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    let payload = json!({
        "id": schema_id,
        "projectId": project_id,
        "name": name,
        "schemaJson": schema_json,
        "schema": schema_json,
        "classes": classes,
        "revision": revision,
        "updatedBy": user.id,
        "updatedAt": Utc::now()
    });
    write_change(
        &mut tx,
        project_id,
        "label_schema",
        &schema_id.to_string(),
        "update",
        revision,
        payload.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "label_schema.update",
        "label_schema",
        &schema_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(payload))
}

/// Apply a review in one transaction.  Sync uses the same operation so REST
/// and offline replay have identical revision and conflict semantics.
pub async fn perform_review(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    image_id: Uuid,
    input: &ReviewRequest,
) -> Result<Value, ApiError> {
    if !matches!(input.decision.as_str(), "approved" | "rejected") {
        return Err(ApiError::bad_request(
            "decision must be approved or rejected",
        ));
    }
    let expected = input
        .annotation_revision
        .ok_or_else(|| ApiError::bad_request("annotationRevision is required"))?;
    let operation_id = input
        .operation_id
        .clone()
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    let mut tx = state.pool.begin().await?;
    // Serialize every review mutation and its change sequence with the
    // project's other sync operations. This also prevents an expired
    // operation reservation from racing an older in-flight reviewer.
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    let reserved = sqlx::query(
        "INSERT INTO processed_operations (operation_id, project_id, result_json, state, claimed_at) VALUES ($1, $2, '{}', 'processing', now()) ON CONFLICT(operation_id) DO NOTHING RETURNING operation_id",
    )
    .bind(&operation_id)
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?;
    if reserved.is_none() {
        let existing_operation = sqlx::query(
            "SELECT project_id, state, result_json FROM processed_operations WHERE operation_id = $1",
        )
        .bind(&operation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::internal_message("operation reservation disappeared"))?;
        if existing_operation.get::<Uuid, _>("project_id") != project_id {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another project",
                Value::Null,
            ));
        }
        if existing_operation.get::<String, _>("state") == "completed" {
            let stored: Value = existing_operation.get("result_json");
            return Ok(stored.get("remotePayload").cloned().unwrap_or(stored));
        }
        // A sync request reserves the operation before entering this shared
        // transaction. The processing row is expected here; the annotation
        // row lock below serializes concurrent owners.
    }
    if let Some(existing) = sqlx::query(
        "SELECT a.id, a.image_id, a.revision, a.object_json, a.status, q.id AS review_id, q.annotation_revision, q.decision, q.note FROM qa_reviews q JOIN annotations a ON a.image_id = q.image_id WHERE q.project_id = $1 AND q.operation_id = $2",
    )
    .bind(project_id)
    .bind(&operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        let current_revision: i64 = existing.get("revision");
        let payload = json!({
            "id": existing.get::<Uuid, _>("review_id"),
            "reviewId": existing.get::<Uuid, _>("review_id"),
            "annotationId": existing.get::<Uuid, _>("id"),
            "imageId": existing.get::<Uuid, _>("image_id"),
            "revision": current_revision,
            "annotationRevision": existing.get::<i64, _>("annotation_revision"),
            "objects": existing.get::<Value, _>("object_json"),
            "status": existing.get::<String, _>("status"),
            "decision": existing.get::<String, _>("decision"),
            "note": existing.get::<String, _>("note"),
            "operationId": operation_id
        });
        super::sync::complete_operation_tx(
            &mut tx,
            project_id,
            &operation_id,
            current_revision,
            &payload,
        )
        .await?;
        tx.commit().await?;
        return Ok(payload);
    }
    let current = sqlx::query(
        "SELECT id, image_id, revision, object_json, status FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("annotation not found"))?;
    let current_revision: i64 = current.get("revision");
    if current_revision != expected {
        return Err(ApiError::conflict(
            "ANNOTATION_REVISION_CONFLICT",
            "annotation revision has changed",
            json!({
                "baseRevision": expected,
                "currentRevision": current_revision,
                "remoteAnnotation": annotation_payload(&current, current_revision, &operation_id)
            }),
        ));
    }
    let annotation_id: Uuid = current.get("id");
    let next_revision = current_revision + 1;
    let status = if input.decision == "approved" {
        "approved"
    } else {
        "rejected"
    };
    let updated = sqlx::query(
        "UPDATE annotations SET status = $3, revision = $4, updated_by = $5, updated_at = now() WHERE id = $1 AND project_id = $2 RETURNING revision, object_json, status",
    )
    .bind(annotation_id)
    .bind(project_id)
    .bind(status)
    .bind(next_revision)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    let objects: Value = updated.get("object_json");
    let review_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO qa_reviews (id, project_id, image_id, annotation_revision, decision, note, reviewer_id, operation_id) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(review_id)
    .bind(project_id)
    .bind(image_id)
    .bind(expected)
    .bind(&input.decision)
    .bind(&input.note)
    .bind(user.id)
    .bind(&operation_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE assets SET status = $3, revision = revision + 1, updated_by = $4, updated_at = now() WHERE id = $1 AND project_id = $2",
    )
    .bind(image_id)
    .bind(project_id)
    .bind(status)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE task_items SET status = $2, revision = revision + 1, updated_by = $3, updated_at = now() WHERE image_id = $1 AND deleted_at IS NULL AND status IN ('pending_review', 'in_progress', 'assigned', 'unassigned', 'rejected')",
    )
    .bind(image_id)
    .bind(status)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    let issue = if input.decision == "rejected" {
        let issue_key = input
            .issue_client_key
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "")
            .to_string();
        let issue_key = if issue_key.is_empty() {
            format!("qa:{operation_id}")
        } else {
            issue_key
        };
        let inserted = sqlx::query(
            "INSERT INTO issues (id, project_id, client_key, image_id, title, description, severity, status, source, reporter_id) VALUES ($1, $2, $3, $4, '质检驳回', $5, 'major', 'open', 'qa', $6) ON CONFLICT(project_id, client_key) WHERE client_key IS NOT NULL DO NOTHING RETURNING id, revision",
        )
        .bind(Uuid::new_v4())
        .bind(project_id)
        .bind(&issue_key)
        .bind(image_id)
        .bind(&input.note)
        .bind(user.id)
        .fetch_optional(&mut *tx)
        .await?;
        let row = if let Some(row) = inserted {
            Some((
                row.get::<Uuid, _>("id"),
                row.get::<i64, _>("revision"),
                true,
            ))
        } else {
            sqlx::query(
                "SELECT id, revision FROM issues WHERE project_id = $1 AND client_key = $2 AND deleted_at IS NULL",
            )
            .bind(project_id)
            .bind(&issue_key)
            .fetch_optional(&mut *tx)
            .await?
            .map(|row| (row.get::<Uuid, _>("id"), row.get::<i64, _>("revision"), false))
        };
        row.map(|(id, revision, inserted)| (id, revision, inserted, issue_key))
    } else {
        None
    };
    if let Some((issue_id, issue_revision, inserted, issue_key)) = issue.as_ref() {
        if *inserted {
            let issue_payload = json!({
                "id": issue_id,
                "clientKey": issue_key,
                "imageId": image_id,
                "title": "质检驳回",
                "description": input.note,
                "severity": "major",
                "status": "open",
                "source": "qa",
                "reporterId": user.id,
                "assigneeId": Value::Null,
                "revision": issue_revision
            });
            write_change(
                &mut tx,
                project_id,
                "issue",
                &issue_id.to_string(),
                "create",
                *issue_revision,
                issue_payload,
            )
            .await?;
        }
    }
    let annotation_event = json!({
        "id": annotation_id,
        "imageId": image_id,
        "revision": next_revision,
        "objects": objects,
        "status": status,
        "updatedBy": user.id,
        "updatedAt": Utc::now()
    });
    let review_event = json!({
        "id": review_id,
        "imageId": image_id,
        "annotationRevision": expected,
        "revision": next_revision,
        "decision": input.decision,
        "note": input.note,
        "operationId": operation_id,
        "objects": objects
    });
    // The annotation and review are distinct sync entities.  Keep both
    // changes so clients can update annotation state and append the review
    // record without guessing which UUID `id` refers to.
    write_change(
        &mut tx,
        project_id,
        "annotation",
        &annotation_id.to_string(),
        "review",
        next_revision,
        annotation_event,
    )
    .await?;
    write_change(
        &mut tx,
        project_id,
        "qa_review",
        &review_id.to_string(),
        "transition",
        next_revision,
        review_event,
    )
    .await?;
    let payload = json!({
        "id": review_id,
        "reviewId": review_id,
        "annotationId": annotation_id,
        "imageId": image_id,
        "revision": next_revision,
        "annotationRevision": expected,
        "objects": objects,
        "status": status,
        "decision": input.decision,
        "note": input.note,
        "issueId": issue.as_ref().map(|(id, _, _, _)| id),
        "issueClientKey": issue.as_ref().map(|(_, _, _, key)| key),
        "operationId": operation_id,
        "updatedBy": user.id,
        "updatedAt": Utc::now()
    });
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "qa.review",
        "annotation",
        &annotation_id.to_string(),
    )
    .await?;
    super::sync::complete_operation_tx(&mut tx, project_id, &operation_id, next_revision, &payload)
        .await?;
    tx.commit().await?;
    Ok(payload)
}

/// Claim is intentionally an atomic UPDATE.  A task item can only transition
/// from a free/rejected state (or be renewed by its current assignee).
pub async fn claim_task_item(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    task_id: Uuid,
    image_id: Uuid,
    role: &str,
    expected_revision: Option<i64>,
    operation_id: Option<&str>,
) -> Result<Value, ApiError> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    let takeover = matches!(role, "owner" | "manager");
    let row = sqlx::query(
        "UPDATE task_items ti SET status = 'in_progress', assignee_id = $4, claimed_at = now(), revision = ti.revision + 1, updated_by = $4, updated_at = now() FROM tasks t WHERE ti.task_id = t.id AND t.project_id = $1 AND ti.task_id = $2 AND ti.image_id = $3 AND ti.deleted_at IS NULL AND t.deleted_at IS NULL AND ($6::bigint IS NULL OR ti.revision = $6) AND (ti.assignee_id IS NULL OR ti.assignee_id = $4 OR $5) AND ti.status IN ('unassigned', 'assigned', 'active', 'rejected', '草稿', 'in_progress') RETURNING ti.id, ti.task_id, ti.image_id, ti.status, ti.assignee_id, ti.claimed_at, ti.revision",
    )
    .bind(project_id)
    .bind(task_id)
    .bind(image_id)
    .bind(user.id)
    .bind(takeover)
    .bind(expected_revision)
    .fetch_optional(&mut *tx)
    .await?;
    let row = match row {
        Some(row) => row,
        None => {
            let current = sqlx::query(
                "SELECT id, status, assignee_id, revision FROM task_items WHERE task_id = $1 AND image_id = $2 AND deleted_at IS NULL",
            )
            .bind(task_id)
            .bind(image_id)
            .fetch_optional(&mut *tx)
            .await?;
            let details = current
                .map(|item| {
                    json!({
                        "id": item.get::<Uuid, _>("id"),
                        "imageId": image_id,
                        "status": item.get::<String, _>("status"),
                        "assigneeId": item.get::<Option<Uuid>, _>("assignee_id"),
                        "revision": item.get::<i64, _>("revision")
                    })
                })
                .unwrap_or(Value::Null);
            return Err(ApiError::conflict(
                "TASK_ITEM_CLAIMED",
                "task item is not available for claim",
                details,
            ));
        }
    };
    let revision: i64 = row.get("revision");
    let payload = json!({
        "id": row.get::<Uuid, _>("id"),
        "taskId": task_id,
        "imageId": image_id,
        "status": "in_progress",
        "assigneeId": user.id,
        "claimedAt": row.get::<chrono::DateTime<Utc>, _>("claimed_at"),
        "revision": revision
    });
    write_change(
        &mut tx,
        project_id,
        "task_item",
        &format!("{task_id}:{image_id}"),
        "claim",
        revision,
        payload.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "task_item.claim",
        "task_item",
        &format!("{task_id}:{image_id}"),
    )
    .await?;
    if let Some(operation_id) = operation_id {
        super::sync::complete_operation_tx(&mut tx, project_id, operation_id, revision, &payload)
            .await?;
    }
    tx.commit().await?;
    Ok(payload)
}

pub async fn release_task_item(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    task_id: Uuid,
    image_id: Uuid,
    role: &str,
    expected_revision: Option<i64>,
    operation_id: Option<&str>,
) -> Result<Value, ApiError> {
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT id FROM projects WHERE id = $1 FOR UPDATE")
        .bind(project_id)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "UPDATE task_items SET status = 'unassigned', assignee_id = NULL, claimed_at = NULL, revision = revision + 1, updated_by = $4, updated_at = now() WHERE task_id = $1 AND image_id = $2 AND deleted_at IS NULL AND ($5::bigint IS NULL OR revision = $5) AND ($3 OR assignee_id = $4) RETURNING id, revision",
    )
    .bind(task_id)
    .bind(image_id)
    .bind(matches!(role, "owner" | "manager"))
    .bind(user.id)
    .bind(expected_revision)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::forbidden("only the assignee or a project manager can release this item"))?;
    let revision: i64 = row.get("revision");
    let payload = json!({
        "id": row.get::<Uuid, _>("id"),
        "taskId": task_id,
        "imageId": image_id,
        "status": "unassigned",
        "assigneeId": null,
        "revision": revision
    });
    write_change(
        &mut tx,
        project_id,
        "task_item",
        &format!("{task_id}:{image_id}"),
        "release",
        revision,
        payload.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "task_item.release",
        "task_item",
        &format!("{task_id}:{image_id}"),
    )
    .await?;
    if let Some(operation_id) = operation_id {
        super::sync::complete_operation_tx(&mut tx, project_id, operation_id, revision, &payload)
            .await?;
    }
    tx.commit().await?;
    Ok(payload)
}

fn task_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "name": row.get::<String, _>("name"),
        "status": row.get::<String, _>("status"),
        "assigneeId": row.get::<Option<Uuid>, _>("assignee_id"),
        "revision": row.get::<i64, _>("revision"),
        "createdBy": row.get::<Uuid, _>("created_by"),
        "updatedBy": row.get::<Uuid, _>("updated_by"),
        "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at"),
        "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
    })
}

fn annotation_payload(row: &sqlx::postgres::PgRow, revision: i64, operation_id: &str) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "imageId": row.get::<Uuid, _>("image_id"),
        "revision": revision,
        "objects": row.get::<Value, _>("object_json"),
        "status": row.get::<String, _>("status"),
        "operationId": operation_id
    })
}

pub(crate) fn schema_classes(schema: &Value) -> Value {
    schema
        .get("classes")
        .cloned()
        .or_else(|| schema.as_array().map(|_| schema.clone()))
        .unwrap_or_else(|| json!([]))
}

fn valid_task_status(status: &str) -> bool {
    matches!(
        status,
        "unassigned"
            | "assigned"
            | "active"
            | "in_progress"
            | "pending_review"
            | "approved"
            | "rejected"
            | "草稿"
            | "进行中"
            | "待质检"
            | "通过"
            | "驳回"
    )
}

async fn task_project(state: &AppState, task_id: Uuid) -> Result<Uuid, ApiError> {
    sqlx::query_scalar("SELECT project_id FROM tasks WHERE id = $1 AND deleted_at IS NULL")
        .bind(task_id)
        .fetch_optional(&state.pool)
        .await?
        .ok_or_else(|| ApiError::not_found("task not found"))
}

#[allow(dead_code)]
async fn _transaction_marker(_: &mut Transaction<'_, Postgres>) {}

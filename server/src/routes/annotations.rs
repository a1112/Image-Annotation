use super::{projects::write_audit, projects::write_change, ApiError};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::collections::HashSet;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveAnnotation {
    base_revision: Option<i64>,
    schema_version_id: Option<Uuid>,
    objects: Value,
    operation_id: Option<String>,
    device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseRequest {
    device_id: String,
    lease_id: Option<Uuid>,
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    ensure_image(&state, project_id, image_id).await?;
    let row = sqlx::query(
        "SELECT id, revision, schema_version_id, object_json, status, updated_by, updated_at FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&state.pool)
    .await?;
    Ok(Json(row.map(|row| annotation_json(&row)).unwrap_or_else(
        || {
            json!({
                "id": null,
                "projectId": project_id,
                "imageId": image_id,
                "revision": 0,
                "schemaVersionId": null,
                "objects": [],
                "status": "draft",
                "updatedBy": null,
                "updatedAt": null
            })
        },
    )))
}

pub async fn put(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
    Json(input): Json<SaveAnnotation>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Annotate).await?;
    ensure_image(&state, project_id, image_id).await?;
    validate_objects(&input.objects)?;
    let expected_revision = input.base_revision.or_else(|| {
        headers
            .get(axum::http::header::IF_MATCH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim_matches('"').parse().ok())
    });
    let operation_id = input
        .operation_id
        .unwrap_or_else(|| Uuid::new_v4().to_string());
    let mut tx = state.pool.begin().await?;
    let reserved = sqlx::query(
        "INSERT INTO processed_operations (operation_id, project_id, result_json, state, claimed_at) VALUES ($1, $2, '{}', 'processing', now()) ON CONFLICT(operation_id) DO NOTHING RETURNING operation_id",
    )
    .bind(&operation_id)
    .bind(project_id)
    .fetch_optional(&mut *tx)
    .await?;
    if reserved.is_none() {
        let existing = sqlx::query(
            "SELECT project_id, state, result_json FROM processed_operations WHERE operation_id = $1",
        )
        .bind(&operation_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::internal_message("operation reservation disappeared"))?;
        if existing.get::<Uuid, _>("project_id") != project_id {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another project",
                Value::Null,
            ));
        }
        if existing.get::<String, _>("state") == "completed" {
            let stored: Value = existing.get("result_json");
            return Ok(Json(stored.get("remotePayload").cloned().unwrap_or(stored)));
        }
        return Err(ApiError::conflict(
            "OPERATION_IN_PROGRESS",
            "operationId is already being applied",
            Value::Null,
        ));
    }
    let current = sqlx::query(
        "SELECT id, image_id, revision, object_json, status FROM annotations WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_optional(&mut *tx)
    .await?;
    let current_revision = current
        .as_ref()
        .map(|row| row.get("revision"))
        .unwrap_or(0_i64);
    if expected_revision.unwrap_or(0) != current_revision {
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
                    "id": null,
                    "imageId": image_id,
                    "revision": 0,
                    "objects": [],
                    "status": "draft"
                })
            });
        return Err(ApiError::conflict(
            "ANNOTATION_REVISION_CONFLICT",
            "annotation revision has changed",
            json!({
                "baseRevision": expected_revision,
                "currentRevision": current_revision,
                "remoteAnnotation": remote
            }),
        ));
    }
    let annotation_id = current
        .as_ref()
        .map(|row| row.get::<Uuid, _>("id"))
        .unwrap_or_else(Uuid::new_v4);
    let revision = current_revision + 1;
    sqlx::query(
        r#"
        INSERT INTO annotations (
            id, project_id, image_id, revision, schema_version_id,
            object_json, status, updated_by
        ) VALUES ($1, $2, $3, $4, $5, $6, 'draft', $7)
        ON CONFLICT(image_id) DO UPDATE SET
            revision = excluded.revision,
            schema_version_id = excluded.schema_version_id,
            object_json = excluded.object_json,
            status = 'draft',
            updated_by = excluded.updated_by,
            updated_at = now(),
            deleted_at = NULL
        "#,
    )
    .bind(annotation_id)
    .bind(project_id)
    .bind(image_id)
    .bind(revision)
    .bind(input.schema_version_id)
    .bind(&input.objects)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO annotation_versions (annotation_id, revision, object_json, operation_id, created_by) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(annotation_id)
    .bind(revision)
    .bind(&input.objects)
    .bind(&operation_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE assets SET status = 'draft', revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 AND project_id = $2",
    )
    .bind(image_id)
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    let result = json!({
        "id": annotation_id,
        "imageId": image_id,
        "revision": revision,
        "objects": input.objects,
        "status": "draft",
        "operationId": operation_id,
        "updatedBy": user.id,
        "updatedAt": Utc::now()
    });
    write_change(
        &mut tx,
        project_id,
        "annotation",
        &annotation_id.to_string(),
        "update",
        revision,
        result.clone(),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "annotation.save",
        "annotation",
        &annotation_id.to_string(),
    )
    .await?;
    sqlx::query(
        "UPDATE processed_operations SET result_json = $3, state = 'completed', completed_at = now(), processed_at = now() WHERE operation_id = $1 AND project_id = $2",
    )
    .bind(operation_id)
    .bind(project_id)
    .bind(&result)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

pub async fn versions(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        r#"
        SELECT av.id, av.revision, av.object_json, av.operation_id, av.created_by, av.created_at
        FROM annotation_versions av
        JOIN annotations a ON a.id = av.annotation_id
        WHERE a.project_id = $1 AND a.image_id = $2
        ORDER BY av.revision DESC
        "#,
    )
    .bind(project_id)
    .bind(image_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(Value::Array(
        rows.iter()
            .map(|row| {
                json!({
                    "id": row.get::<Uuid, _>("id"),
                    "revision": row.get::<i64, _>("revision"),
                    "objects": row.get::<Value, _>("object_json"),
                    "operationId": row.get::<String, _>("operation_id"),
                    "createdBy": row.get::<Uuid, _>("created_by"),
                    "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at")
                })
            })
            .collect(),
    )))
}

pub async fn submit(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Annotate).await?;
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "UPDATE annotations SET status = 'submitted', revision = revision + 1, updated_by = $3, updated_at = now() WHERE project_id = $1 AND image_id = $2 AND deleted_at IS NULL RETURNING id, image_id, revision, object_json, status",
    )
    .bind(project_id)
    .bind(image_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("annotation not found"))?;
    let annotation_id: Uuid = row.get("id");
    let revision: i64 = row.get("revision");
    sqlx::query(
        "UPDATE assets SET status = 'pending_review', revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 AND project_id = $2",
    )
    .bind(image_id)
    .bind(project_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    write_change(
        &mut tx,
        project_id,
        "annotation",
        &annotation_id.to_string(),
        "submit",
        revision,
        json!({
            "id": annotation_id,
            "imageId": image_id,
            "revision": revision,
            "objects": row.get::<Value, _>("object_json"),
            "status": row.get::<String, _>("status")
        }),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "annotation.submit",
        "annotation",
        &annotation_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "id": annotation_id,
        "imageId": image_id,
        "revision": revision,
        "objects": row.get::<Value, _>("object_json"),
        "status": row.get::<String, _>("status")
    })))
}

pub async fn acquire_lease(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
    Json(input): Json<LeaseRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Annotate).await?;
    ensure_image(&state, project_id, image_id).await?;
    let lease_id = input.lease_id.unwrap_or_else(Uuid::new_v4);
    let expires_at = Utc::now() + Duration::minutes(5);
    let result = sqlx::query(
        r#"
        INSERT INTO edit_leases (image_id, project_id, lease_id, user_id, device_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT(image_id) DO UPDATE SET
            lease_id = excluded.lease_id,
            user_id = excluded.user_id,
            device_id = excluded.device_id,
            expires_at = excluded.expires_at,
            updated_at = now()
        WHERE edit_leases.expires_at < now()
           OR (edit_leases.user_id = excluded.user_id AND edit_leases.device_id = excluded.device_id)
        RETURNING lease_id, user_id, device_id, expires_at
        "#,
    )
    .bind(image_id)
    .bind(project_id)
    .bind(lease_id)
    .bind(user.id)
    .bind(&input.device_id)
    .bind(expires_at)
    .fetch_optional(&state.pool)
    .await?;
    let row = result.ok_or_else(|| {
        ApiError::conflict(
            "IMAGE_LEASE_CONFLICT",
            "image is being edited by another user",
            Value::Null,
        )
    })?;
    Ok(Json(json!({
        "leaseId": row.get::<Uuid, _>("lease_id"),
        "imageId": image_id,
        "userId": row.get::<Uuid, _>("user_id"),
        "deviceId": row.get::<String, _>("device_id"),
        "expiresAt": row.get::<chrono::DateTime<Utc>, _>("expires_at")
    })))
}

pub async fn release_lease(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, image_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::Annotate).await?;
    sqlx::query("DELETE FROM edit_leases WHERE image_id = $1 AND project_id = $2 AND user_id = $3")
        .bind(image_id)
        .bind(project_id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({ "released": true })))
}

async fn ensure_image(state: &AppState, project_id: Uuid, image_id: Uuid) -> Result<(), ApiError> {
    let exists = sqlx::query(
        "SELECT 1 FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
    )
    .bind(image_id)
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .is_some();
    if exists {
        Ok(())
    } else {
        Err(ApiError::not_found("image not found"))
    }
}

fn validate_objects(objects: &Value) -> Result<(), ApiError> {
    let list = objects
        .as_array()
        .ok_or_else(|| ApiError::bad_request("objects must be an array"))?;
    let mut ids = HashSet::new();
    for object in list {
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| ApiError::bad_request("every annotation object requires a stable id"))?;
        if !ids.insert(id) {
            return Err(ApiError::bad_request(
                "annotation object IDs must be unique",
            ));
        }
    }
    Ok(())
}

fn annotation_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "revision": row.get::<i64, _>("revision"),
        "schemaVersionId": row.get::<Option<Uuid>, _>("schema_version_id"),
        "objects": row.get::<Value, _>("object_json"),
        "status": row.get::<String, _>("status"),
        "updatedBy": row.get::<Uuid, _>("updated_by"),
        "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
    })
}

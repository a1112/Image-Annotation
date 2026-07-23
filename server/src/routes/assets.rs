use super::{projects::write_audit, projects::write_change, ApiError};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use aws_sdk_s3::presigning::PresigningConfig;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

#[derive(Debug, Deserialize)]
pub struct ImageQuery {
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadRequest {
    client_key: Option<String>,
    file_name: String,
    content_hash: String,
    mime_type: String,
    width: i32,
    height: i32,
    byte_size: i64,
    #[serde(default = "default_split")]
    split: String,
}

fn default_split() -> String {
    "train".to_string()
}

pub async fn list_images(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(query): Query<ImageQuery>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let cursor = query
        .cursor
        .as_deref()
        .and_then(|value| Uuid::parse_str(value).ok())
        .unwrap_or(Uuid::nil());
    let rows = sqlx::query(
        r#"
        SELECT id, client_key, file_name, object_key, content_hash, mime_type,
            width, height, byte_size, split, status, revision, updated_at
        FROM assets
        WHERE project_id = $1 AND deleted_at IS NULL AND id > $2
        ORDER BY id
        LIMIT $3
        "#,
    )
    .bind(project_id)
    .bind(cursor)
    .bind(limit)
    .fetch_all(&state.pool)
    .await?;
    let next_cursor = rows.last().map(|row| row.get::<Uuid, _>("id").to_string());
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
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
            "revision": row.get::<i64, _>("revision"),
            "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
        })).collect::<Vec<_>>(),
        "nextCursor": next_cursor
    })))
}

pub async fn create_upload_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<UploadRequest>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData).await?;
    if input.byte_size <= 0 || input.width <= 0 || input.height <= 0 {
        return Err(ApiError::bad_request("asset metadata is invalid"));
    }
    let candidate_id = Uuid::new_v4();
    let candidate_key = format!("projects/{project_id}/assets/{candidate_id}/original");
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        INSERT INTO assets (
            id, project_id, client_key, file_name, object_key, content_hash,
            mime_type, width, height, byte_size, split, status, created_by, updated_by
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'uploading', $12, $12)
        ON CONFLICT(project_id, client_key) WHERE client_key IS NOT NULL
        DO UPDATE SET
            status = CASE WHEN assets.status = 'failed' THEN 'uploading' ELSE assets.status END,
            updated_at = now()
        RETURNING id, object_key, file_name, content_hash, mime_type, width,
            height, byte_size, split, status
        "#,
    )
    .bind(candidate_id)
    .bind(project_id)
    .bind(&input.client_key)
    .bind(&input.file_name)
    .bind(&candidate_key)
    .bind(&input.content_hash)
    .bind(&input.mime_type)
    .bind(input.width)
    .bind(input.height)
    .bind(input.byte_size)
    .bind(&input.split)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    let asset_id: Uuid = row.get("id");
    let object_key: String = row.get("object_key");
    let status: String = row.get("status");
    let metadata_matches = row.get::<String, _>("file_name") == input.file_name
        && row.get::<String, _>("content_hash") == input.content_hash
        && row.get::<String, _>("mime_type") == input.mime_type
        && row.get::<i32, _>("width") == input.width
        && row.get::<i32, _>("height") == input.height
        && row.get::<i64, _>("byte_size") == input.byte_size
        && row.get::<String, _>("split") == input.split;
    if !metadata_matches {
        return Err(ApiError::conflict(
            "ASSET_CLIENT_KEY_CONFLICT",
            "clientKey already belongs to an asset with different metadata",
            json!({ "assetId": asset_id, "clientKey": input.client_key }),
        ));
    }
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "asset.upload_session",
        "asset",
        &asset_id.to_string(),
    )
    .await?;
    tx.commit().await?;

    if status == "ready" {
        return Ok(Json(json!({
            "assetId": asset_id,
            "objectKey": object_key,
            "uploadUrl": null,
            "method": "PUT",
            "expiresIn": 0,
            "alreadyComplete": true
        })));
    }
    let presigned = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .content_type(&input.mime_type)
        .metadata("sha256", &input.content_hash)
        .presigned(
            PresigningConfig::expires_in(Duration::from_secs(900))
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
        )
        .await
        .map_err(|error| ApiError::bad_request(format!("failed to create upload URL: {error}")))?;
    Ok(Json(json!({
        "assetId": asset_id,
        "objectKey": object_key,
        "uploadUrl": presigned.uri(),
        "method": "PUT",
        "expiresIn": 900,
        "alreadyComplete": false
    })))
}

pub async fn complete_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Path((project_id, asset_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_project_permission(&state, user.id, project_id, ProjectPermission::ManageData).await?;
    let row = sqlx::query(
        "SELECT client_key, file_name, object_key, content_hash, mime_type, width, height, byte_size, split FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("asset not found"))?;
    let object_key: String = row.get("object_key");
    let expected_size: i64 = row.get("byte_size");
    let expected_hash: String = row.get("content_hash");
    let client_key: Option<String> = row.get("client_key");
    let file_name: String = row.get("file_name");
    let mime_type: String = row.get("mime_type");
    let width: i32 = row.get("width");
    let height: i32 = row.get("height");
    let split: String = row.get("split");
    let head = state
        .s3
        .head_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("uploaded object not found: {error}")))?;
    if head.content_length().unwrap_or_default() != expected_size {
        return Err(ApiError::bad_request("uploaded object size mismatch"));
    }
    if head
        .metadata()
        .and_then(|metadata| metadata.get("sha256"))
        .is_some_and(|hash| hash != &expected_hash)
    {
        return Err(ApiError::bad_request("uploaded object hash metadata mismatch"));
    }
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "UPDATE assets SET status = 'ready', revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 AND project_id = $2 RETURNING revision",
    )
    .bind(asset_id)
    .bind(project_id)
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    let revision: i64 = row.get("revision");
    let change_key = client_key.as_deref().unwrap_or_else(|| {
        // The UUID string is created below and only used for this transaction.
        // Projects published by current clients always provide clientKey.
        ""
    });
    let fallback_key = asset_id.to_string();
    let change_key = if change_key.is_empty() {
        fallback_key.as_str()
    } else {
        change_key
    };
    write_change(
        &mut tx,
        project_id,
        "asset",
        change_key,
        "create",
        revision,
        json!({
            "id": asset_id,
            "clientKey": client_key,
            "fileName": file_name,
            "objectKey": object_key,
            "contentHash": expected_hash,
            "mimeType": mime_type,
            "width": width,
            "height": height,
            "byteSize": expected_size,
            "split": split,
            "status": "ready",
            "revision": revision
        }),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "asset.complete",
        "asset",
        &asset_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({ "assetId": asset_id, "status": "ready", "revision": revision })))
}

pub async fn download_url(
    State(state): State<AppState>,
    user: AuthUser,
    Path(asset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT project_id, file_name, object_key, content_hash, mime_type, byte_size FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("asset not found"))?;
    let project_id: Uuid = row.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let object_key: String = row.get("object_key");
    let presigned = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .presigned(
            PresigningConfig::expires_in(Duration::from_secs(300))
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
        )
        .await
        .map_err(|error| ApiError::bad_request(format!("failed to create download URL: {error}")))?;
    Ok(Json(json!({
        "assetId": asset_id,
        "downloadUrl": presigned.uri(),
        "fileName": row.get::<String, _>("file_name"),
        "contentHash": row.get::<String, _>("content_hash"),
        "mimeType": row.get::<String, _>("mime_type"),
        "byteSize": row.get::<i64, _>("byte_size"),
        "expiresIn": 300
    })))
}

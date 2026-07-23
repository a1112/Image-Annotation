use super::{
    projects::{write_audit, write_change},
    ApiError,
};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use aws_sdk_s3::presigning::PresigningConfig;
use axum::{
    extract::{Path, State},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
use std::time::Duration;
use uuid::Uuid;

const UPLOAD_EXPIRY_SECONDS: u64 = 900;
const DOWNLOAD_EXPIRY_SECONDS: u64 = 300;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentUploadRequest {
    client_key: Option<String>,
    file_name: String,
    content_hash: String,
    mime_type: String,
    byte_size: i64,
}

pub async fn list(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let project_id = issue_project_id(&state, issue_id).await?;
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let rows = sqlx::query(
        r#"
        SELECT id, client_key, file_name, content_hash, mime_type, byte_size,
            status, revision, created_by, created_at, updated_at
        FROM issue_attachments
        WHERE issue_id = $1 AND deleted_at IS NULL
        ORDER BY created_at, id
        "#,
    )
    .bind(issue_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(attachment_json).collect::<Vec<_>>()
    })))
}

pub async fn create_upload_session(
    State(state): State<AppState>,
    user: AuthUser,
    Path(issue_id): Path<Uuid>,
    Json(input): Json<AttachmentUploadRequest>,
) -> Result<Json<Value>, ApiError> {
    let project_id = issue_project_id(&state, issue_id).await?;
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;
    validate_upload(&input)?;

    let attachment_id = Uuid::new_v4();
    let object_key = format!(
        "projects/{project_id}/issues/{issue_id}/attachments/{attachment_id}/original"
    );
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        r#"
        INSERT INTO issue_attachments (
            id, issue_id, project_id, client_key, file_name, object_key,
            content_hash, mime_type, byte_size, status, created_by
        ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 'uploading', $10)
        "#,
    )
    .bind(attachment_id)
    .bind(issue_id)
    .bind(project_id)
    .bind(&input.client_key)
    .bind(&input.file_name)
    .bind(&object_key)
    .bind(&input.content_hash)
    .bind(&input.mime_type)
    .bind(input.byte_size)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "issue_attachment.upload_session",
        "issue_attachment",
        &attachment_id.to_string(),
    )
    .await?;
    tx.commit().await?;

    let presigned = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .content_type(&input.mime_type)
        .metadata("sha256", &input.content_hash)
        .presigned(
            PresigningConfig::expires_in(Duration::from_secs(UPLOAD_EXPIRY_SECONDS))
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
        )
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("failed to create attachment upload URL: {error}"))
        })?;
    Ok(Json(json!({
        "attachmentId": attachment_id,
        "issueId": issue_id,
        "objectKey": object_key,
        "uploadUrl": presigned.uri(),
        "method": "PUT",
        "expiresIn": UPLOAD_EXPIRY_SECONDS
    })))
}

pub async fn complete_upload(
    State(state): State<AppState>,
    user: AuthUser,
    Path((issue_id, attachment_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT project_id, client_key, file_name, object_key, content_hash,
            mime_type, byte_size
        FROM issue_attachments
        WHERE id = $1 AND issue_id = $2 AND deleted_at IS NULL
        "#,
    )
    .bind(attachment_id)
    .bind(issue_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("issue attachment not found"))?;
    let project_id: Uuid = row.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::Review).await?;

    let object_key: String = row.get("object_key");
    let expected_hash: String = row.get("content_hash");
    let expected_size: i64 = row.get("byte_size");
    let client_key: Option<String> = row.get("client_key");
    let file_name: String = row.get("file_name");
    let mime_type: String = row.get("mime_type");
    let head = state
        .s3
        .head_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .send()
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("uploaded attachment was not found: {error}"))
        })?;
    if head.content_length().unwrap_or_default() != expected_size {
        return Err(ApiError::bad_request("attachment size mismatch"));
    }
    if head
        .metadata()
        .and_then(|metadata| metadata.get("sha256"))
        .is_some_and(|hash| hash != &expected_hash)
    {
        return Err(ApiError::bad_request(
            "attachment checksum metadata mismatch",
        ));
    }

    let mut tx = state.pool.begin().await?;
    let updated = sqlx::query(
        r#"
        UPDATE issue_attachments
        SET status = 'ready', revision = revision + 1, updated_at = now()
        WHERE id = $1 AND issue_id = $2
        RETURNING revision
        "#,
    )
    .bind(attachment_id)
    .bind(issue_id)
    .fetch_one(&mut *tx)
    .await?;
    let revision: i64 = updated.get("revision");
    let entity_key = client_key
        .clone()
        .unwrap_or_else(|| attachment_id.to_string());
    write_change(
        &mut tx,
        project_id,
        "issue_attachment",
        &entity_key,
        "create",
        revision,
        json!({
            "id": attachment_id,
            "clientKey": client_key,
            "projectId": project_id,
            "issueId": issue_id,
            "fileName": file_name,
            "objectKey": object_key,
            "contentHash": expected_hash,
            "mimeType": mime_type,
            "byteSize": expected_size,
            "status": "ready",
            "revision": revision
        }),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "issue_attachment.complete",
        "issue_attachment",
        &attachment_id.to_string(),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(json!({
        "attachmentId": attachment_id,
        "status": "ready",
        "revision": revision
    })))
}

pub async fn download_url(
    State(state): State<AppState>,
    user: AuthUser,
    Path(attachment_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT project_id, issue_id, file_name, object_key, content_hash,
            mime_type, byte_size
        FROM issue_attachments
        WHERE id = $1 AND status = 'ready' AND deleted_at IS NULL
        "#,
    )
    .bind(attachment_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("issue attachment not found"))?;
    let project_id: Uuid = row.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let object_key: String = row.get("object_key");
    let presigned = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .presigned(
            PresigningConfig::expires_in(Duration::from_secs(DOWNLOAD_EXPIRY_SECONDS))
                .map_err(|error| ApiError::bad_request(error.to_string()))?,
        )
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("failed to create attachment download URL: {error}"))
        })?;
    Ok(Json(json!({
        "attachmentId": attachment_id,
        "issueId": row.get::<Uuid, _>("issue_id"),
        "downloadUrl": presigned.uri(),
        "fileName": row.get::<String, _>("file_name"),
        "contentHash": row.get::<String, _>("content_hash"),
        "mimeType": row.get::<String, _>("mime_type"),
        "byteSize": row.get::<i64, _>("byte_size"),
        "expiresIn": DOWNLOAD_EXPIRY_SECONDS
    })))
}

async fn issue_project_id(state: &AppState, issue_id: Uuid) -> Result<Uuid, ApiError> {
    sqlx::query("SELECT project_id FROM issues WHERE id = $1 AND deleted_at IS NULL")
        .bind(issue_id)
        .fetch_optional(&state.pool)
        .await?
        .map(|row| row.get("project_id"))
        .ok_or_else(|| ApiError::not_found("issue not found"))
}

fn validate_upload(input: &AttachmentUploadRequest) -> Result<(), ApiError> {
    if input.file_name.trim().is_empty()
        || input.mime_type.trim().is_empty()
        || input.byte_size <= 0
    {
        return Err(ApiError::bad_request("attachment metadata is invalid"));
    }
    if input.content_hash.len() != 64
        || !input
            .content_hash
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(ApiError::bad_request(
            "attachment contentHash must be a SHA-256 digest",
        ));
    }
    Ok(())
}

fn attachment_json(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "clientKey": row.get::<Option<String>, _>("client_key"),
        "fileName": row.get::<String, _>("file_name"),
        "contentHash": row.get::<String, _>("content_hash"),
        "mimeType": row.get::<String, _>("mime_type"),
        "byteSize": row.get::<i64, _>("byte_size"),
        "status": row.get::<String, _>("status"),
        "revision": row.get::<i64, _>("revision"),
        "createdBy": row.get::<Uuid, _>("created_by"),
        "createdAt": row.get::<chrono::DateTime<chrono::Utc>, _>("created_at"),
        "updatedAt": row.get::<chrono::DateTime<chrono::Utc>, _>("updated_at")
    })
}

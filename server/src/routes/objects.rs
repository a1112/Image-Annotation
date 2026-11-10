use super::{
    datasets::{project_organization, release_quota_reservation},
    ApiError,
};
use crate::{
    auth::{hash_token, AuthUser},
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::{Duration, Utc};
use futures_util::StreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub enum TicketDirection {
    Get,
    Put,
}

impl TicketDirection {
    fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Put => "put",
        }
    }
}

pub async fn issue_ticket(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    object_key: &str,
    asset_id: Option<Uuid>,
    attachment_id: Option<Uuid>,
    variant: &str,
    direction: TicketDirection,
    file_name: &str,
    mime_type: &str,
    content_hash: Option<&str>,
    expected_size: Option<i64>,
) -> Result<serde_json::Value, ApiError> {
    let raw_token = format!("{}.{}", Uuid::new_v4(), Uuid::new_v4());
    let expires_at = Utc::now() + Duration::minutes(5);
    sqlx::query(
        "INSERT INTO object_tickets (token_hash, project_id, asset_id, attachment_id, object_key, variant, direction, file_name, mime_type, content_hash, expected_size, expires_at, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
    )
    .bind(hash_token(&raw_token))
    .bind(project_id)
    .bind(asset_id)
    .bind(attachment_id)
    .bind(object_key)
    .bind(variant)
    .bind(direction.as_str())
    .bind(file_name)
    .bind(mime_type)
    .bind(content_hash)
    .bind(expected_size)
    .bind(expires_at)
    .bind(user.id)
    .execute(&state.pool)
    .await?;
    Ok(json!({
        "url": format!("{}/api/v1/object-tickets/{}", state.config.public_base_url, raw_token),
        "method": match direction { TicketDirection::Get => "GET", TicketDirection::Put => "PUT" },
        "expiresIn": 300,
        "expiresAt": expires_at,
        "fileName": file_name,
        "mimeType": mime_type,
        "contentHash": content_hash,
        "byteSize": expected_size
    }))
}

pub async fn get(
    State(state): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, ApiError> {
    let row = ticket_row(&state, &token, "get").await?;
    let object_key: String = row.get("object_key");
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .send()
        .await
        .map_err(|error| ApiError::not_found(format!("object not found: {error}")))?;
    let content_type: String = row.get("mime_type");
    let file_name: String = row.get("file_name");
    let length = object.content_length();
    let stream = ReaderStream::new(object.body.into_async_read());
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Some(length) = length {
        if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"{}\"",
        file_name.replace('"', "")
    )) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
}

pub async fn put(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let row = ticket_row(&state, &token, "put").await?;
    let ticket_id: Uuid = row.get("id");
    let project_id: Uuid = row.get("project_id");
    let asset_id: Option<Uuid> = row.get("asset_id");
    let attachment_id: Option<Uuid> = row.get("attachment_id");
    let object_key: String = row.get("object_key");
    let mime_type: String = row.get("mime_type");
    let content_hash: Option<String> = row.get("content_hash");
    let expected_size: Option<i64> = row.get("expected_size");
    if let (Some(expected), Some(actual)) = (
        expected_size,
        headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<i64>().ok()),
    ) {
        if expected != actual {
            let _ = release_ticket_reservation(&state, project_id, asset_id, attachment_id).await;
            return Err(ApiError::bad_request(
                "object size does not match upload metadata",
            ));
        }
    }
    // Axum's request body is not `Sync`, while the AWS SDK's upload body is.
    // Bridge the two with a bounded temporary file. This keeps memory bounded,
    // computes the digest over actual bytes, and lets MinIO receive a normal
    // seekable ByteStream.
    // A ticket can be retried after a transport failure. Keep each request's
    // bridge file distinct so concurrent retries cannot truncate one another's
    // upload before the ticket's used_at guard is observed.
    let temp_path = std::env::temp_dir().join(format!(
        "image-annotation-upload-{ticket_id}-{}",
        Uuid::new_v4()
    ));
    let upload_result: Result<(), ApiError> = async {
        let mut file = tokio::fs::File::create(&temp_path).await.map_err(|error| {
            ApiError::internal_message(format!("create upload temp file: {error}"))
        })?;
        let mut stream = body.into_data_stream();
        let mut hasher = Sha256::new();
        let mut size = 0_i64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| {
                ApiError::bad_request(format!("read upload body failed: {error}"))
            })?;
            size += chunk.len() as i64;
            if expected_size.is_some_and(|expected| size > expected) {
                return Err(ApiError::bad_request("object size exceeds upload metadata"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|error| {
                ApiError::internal_message(format!("write upload temp file: {error}"))
            })?;
        }
        file.flush().await.map_err(|error| {
            ApiError::internal_message(format!("flush upload temp file: {error}"))
        })?;
        if let Some(expected_size) = expected_size {
            if expected_size != size {
                return Err(ApiError::bad_request(
                    "object size does not match upload metadata",
                ));
            }
        }
        if let Some(expected_hash) = content_hash.as_deref() {
            if !hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_hash) {
                return Err(ApiError::bad_request(
                    "object SHA-256 does not match upload metadata",
                ));
            }
        }
        drop(file);
        let byte_stream = ByteStream::from_path(&temp_path).await.map_err(|error| {
            ApiError::internal_message(format!("open upload temp file: {error}"))
        })?;
        let mut put = state
            .s3
            .put_object()
            .bucket(&state.config.s3_bucket)
            .key(&object_key)
            .content_type(mime_type)
            .body(byte_stream);
        if let Some(content_hash) = content_hash.as_deref() {
            put = put.metadata("sha256", content_hash);
        }
        put.send()
            .await
            .map_err(|error| ApiError::bad_request(format!("object upload failed: {error}")))?;
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&temp_path).await;
    if let Err(error) = upload_result {
        let _ = release_ticket_reservation(&state, project_id, asset_id, attachment_id).await;
        return Err(error);
    }
    sqlx::query("UPDATE object_tickets SET used_at = now() WHERE id = $1")
        .bind(ticket_id)
        .execute(&state.pool)
        .await?;
    Ok((StatusCode::NO_CONTENT, Body::empty()).into_response())
}

/// Verify bytes rather than trusting user-provided object metadata. This is
/// called by completion endpoints after a ticket upload.
pub async fn verify_object_hash(
    state: &AppState,
    object_key: &str,
    expected_hash: &str,
    expected_size: i64,
) -> Result<(), ApiError> {
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .send()
        .await
        .map_err(|error| {
            ApiError::bad_request(format!("uploaded object was not found: {error}"))
        })?;
    let mut reader = object.body.into_async_read();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    let mut size = 0_i64;
    loop {
        let count = reader.read(&mut buffer).await.map_err(|error| {
            ApiError::bad_request(format!("uploaded object read failed: {error}"))
        })?;
        if count == 0 {
            break;
        }
        size += count as i64;
        hasher.update(&buffer[..count]);
        if size > expected_size {
            return Err(ApiError::bad_request("uploaded object size mismatch"));
        }
    }
    if size != expected_size || !hex::encode(hasher.finalize()).eq_ignore_ascii_case(expected_hash)
    {
        return Err(ApiError::bad_request(
            "uploaded object SHA-256 or size does not match metadata",
        ));
    }
    Ok(())
}

async fn ticket_row(
    state: &AppState,
    token: &str,
    direction: &str,
) -> Result<sqlx::postgres::PgRow, ApiError> {
    sqlx::query(
        "SELECT id, project_id, asset_id, attachment_id, object_key, file_name, mime_type, content_hash, expected_size FROM object_tickets WHERE token_hash = $1 AND direction = $2 AND expires_at > now() AND (direction = 'get' OR used_at IS NULL)",
    )
    .bind(hash_token(token))
    .bind(direction)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("object ticket is invalid or expired"))
}

async fn release_ticket_reservation(
    state: &AppState,
    project_id: Uuid,
    asset_id: Option<Uuid>,
    attachment_id: Option<Uuid>,
) -> Result<(), ApiError> {
    let Some(organization_id) = project_organization(&state.pool, project_id).await? else {
        return Ok(());
    };
    if let Some(asset_id) = asset_id {
        release_quota_reservation(&state.pool, organization_id, "asset", asset_id).await?;
    } else if let Some(attachment_id) = attachment_id {
        release_quota_reservation(
            &state.pool,
            organization_id,
            "issue_attachment",
            attachment_id,
        )
        .await?;
    }
    Ok(())
}

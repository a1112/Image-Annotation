use super::{
    datasets::{
        consume_quota_tx, project_organization, project_organization_tx, release_quota_reservation,
        reserve_quota_tx,
    },
    objects::{issue_ticket, verify_object_hash, TicketDirection},
    projects::{write_audit, write_change},
    ApiError,
};
use crate::{
    auth::{require_project_permission, AuthUser, ProjectPermission},
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    extract::{Path, Query, State},
    Json,
};
use image::{codecs::jpeg::JpegEncoder, ImageReader};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::io::AsyncWriteExt;
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

fn upload_is_complete(
    status: &str,
    thumbnail_key: Option<&str>,
    thumbnail_content_hash: Option<&str>,
    thumbnail_mime_type: Option<&str>,
    thumbnail_byte_size: Option<i64>,
) -> bool {
    !matches!(status, "uploading" | "failed")
        && thumbnail_key.is_some_and(|value| !value.trim().is_empty())
        && thumbnail_content_hash.is_some_and(|value| !value.trim().is_empty())
        && thumbnail_mime_type.is_some_and(|value| !value.trim().is_empty())
        && thumbnail_byte_size.is_some_and(|value| value > 0)
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
            width, height, byte_size, split, status, revision, updated_at,
            thumbnail_key, thumbnail_content_hash, thumbnail_mime_type,
            thumbnail_byte_size
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
            "thumbnailKey": row.get::<Option<String>, _>("thumbnail_key"),
            "thumbnailContentHash": row.get::<Option<String>, _>("thumbnail_content_hash"),
            "thumbnailMimeType": row.get::<Option<String>, _>("thumbnail_mime_type"),
            "thumbnailByteSize": row.get::<Option<i64>, _>("thumbnail_byte_size"),
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
            height, byte_size, split, status, revision, thumbnail_key,
            thumbnail_content_hash, thumbnail_mime_type, thumbnail_byte_size
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
    let revision: i64 = row.get("revision");
    let thumbnail_key: Option<String> = row.get("thumbnail_key");
    let thumbnail_content_hash: Option<String> = row.get("thumbnail_content_hash");
    let thumbnail_mime_type: Option<String> = row.get("thumbnail_mime_type");
    let thumbnail_byte_size: Option<i64> = row.get("thumbnail_byte_size");
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
    let already_complete = upload_is_complete(
        &status,
        thumbnail_key.as_deref(),
        thumbnail_content_hash.as_deref(),
        thumbnail_mime_type.as_deref(),
        thumbnail_byte_size,
    );
    if !already_complete && !matches!(status.as_str(), "uploading" | "failed") {
        return Err(ApiError::conflict(
            "ASSET_UPLOAD_ALREADY_COMPLETE",
            "asset is no longer uploadable but its thumbnail metadata is incomplete",
            json!({ "assetId": asset_id, "status": status, "revision": revision }),
        ));
    }
    if !already_complete {
        if let Some(organization_id) = project_organization_tx(&mut tx, project_id).await? {
            reserve_quota_tx(
                &mut tx,
                organization_id,
                "asset",
                asset_id,
                input.byte_size,
                1,
            )
            .await?;
        }
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

    if already_complete {
        return Ok(Json(json!({
            "assetId": asset_id,
            "objectKey": object_key,
            "uploadUrl": null,
            "method": "PUT",
            "expiresIn": 0,
            "alreadyComplete": true,
            "status": status,
            "revision": revision,
            "thumbnailKey": thumbnail_key,
            "thumbnailContentHash": thumbnail_content_hash,
            "thumbnailMimeType": thumbnail_mime_type,
            "thumbnailByteSize": thumbnail_byte_size
        })));
    }
    let mut ticket = issue_ticket(
        &state,
        &user,
        project_id,
        &object_key,
        Some(asset_id),
        None,
        "original",
        TicketDirection::Put,
        &input.file_name,
        &input.mime_type,
        Some(&input.content_hash),
        Some(input.byte_size),
    )
    .await?;
    ticket["contentHash"] = json!(input.content_hash);
    Ok(Json(json!({
        "assetId": asset_id,
        "objectKey": object_key,
        "uploadUrl": ticket["url"].clone(),
        "url": ticket["url"].clone(),
        "method": "PUT",
        "expiresIn": ticket["expiresIn"].clone(),
        "expiresAt": ticket["expiresAt"].clone(),
        "contentHash": ticket["contentHash"].clone(),
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
        "SELECT client_key, file_name, object_key, content_hash, mime_type, width, height, byte_size, split, status, revision, thumbnail_key, thumbnail_content_hash, thumbnail_mime_type, thumbnail_byte_size FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
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
    let status: String = row.get("status");
    let revision: i64 = row.get("revision");
    let thumbnail_key: Option<String> = row.get("thumbnail_key");
    let thumbnail_content_hash: Option<String> = row.get("thumbnail_content_hash");
    let thumbnail_mime_type: Option<String> = row.get("thumbnail_mime_type");
    let thumbnail_byte_size: Option<i64> = row.get("thumbnail_byte_size");
    if upload_is_complete(
        &status,
        thumbnail_key.as_deref(),
        thumbnail_content_hash.as_deref(),
        thumbnail_mime_type.as_deref(),
        thumbnail_byte_size,
    ) {
        return Ok(Json(json!({
            "assetId": asset_id,
            "status": status,
            "revision": revision,
            "thumbnailKey": thumbnail_key,
            "thumbnailContentHash": thumbnail_content_hash,
            "thumbnailMimeType": thumbnail_mime_type,
            "thumbnailByteSize": thumbnail_byte_size
        })));
    }
    if !matches!(status.as_str(), "uploading" | "failed") {
        return Err(ApiError::conflict(
            "ASSET_UPLOAD_ALREADY_COMPLETE",
            "asset is no longer uploadable but its thumbnail metadata is incomplete",
            json!({ "assetId": asset_id, "status": status, "revision": revision }),
        ));
    }
    let organization_id = project_organization(&state.pool, project_id).await?;
    let head = state
        .s3
        .head_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("uploaded object not found: {error}")));
    let head = match head {
        Ok(head) => head,
        Err(error) => {
            if let Some(organization_id) = organization_id {
                let _ = release_quota_reservation(&state.pool, organization_id, "asset", asset_id)
                    .await;
            }
            return Err(error);
        }
    };
    if head.content_length().unwrap_or_default() != expected_size {
        if let Some(organization_id) = organization_id {
            let _ =
                release_quota_reservation(&state.pool, organization_id, "asset", asset_id).await;
        }
        return Err(ApiError::bad_request("uploaded object size mismatch"));
    }
    if let Err(error) = verify_object_hash(&state, &object_key, &expected_hash, expected_size).await
    {
        if let Some(organization_id) = organization_id {
            let _ =
                release_quota_reservation(&state.pool, organization_id, "asset", asset_id).await;
        }
        return Err(error);
    }
    let thumbnail = match generate_thumbnail(&state, project_id, asset_id, &object_key).await {
        Ok(thumbnail) => thumbnail,
        Err(error) => {
            if let Some(organization_id) = organization_id {
                let _ = release_quota_reservation(&state.pool, organization_id, "asset", asset_id)
                    .await;
            }
            return Err(error);
        }
    };
    let mut tx = state.pool.begin().await?;
    if let Some(organization_id) = project_organization_tx(&mut tx, project_id).await? {
        let reservation_active = reserve_quota_tx(
            &mut tx,
            organization_id,
            "asset",
            asset_id,
            expected_size + thumbnail.byte_size,
            1,
        )
        .await?;
        if reservation_active {
            consume_quota_tx(
                &mut tx,
                organization_id,
                "asset",
                asset_id,
                expected_size + thumbnail.byte_size,
                1,
            )
            .await?;
        }
    }
    let row = sqlx::query(
        "UPDATE assets SET status = 'ready', thumbnail_key = $4, thumbnail_content_hash = $5, thumbnail_mime_type = $6, thumbnail_byte_size = $7, revision = revision + 1, updated_by = $3, updated_at = now() WHERE id = $1 AND project_id = $2 RETURNING revision",
    )
    .bind(asset_id)
    .bind(project_id)
    .bind(user.id)
    .bind(&thumbnail.object_key)
    .bind(&thumbnail.content_hash)
    .bind(&thumbnail.mime_type)
    .bind(thumbnail.byte_size)
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
            "thumbnailKey": thumbnail.object_key.clone(),
            "thumbnailContentHash": thumbnail.content_hash.clone(),
            "thumbnailMimeType": thumbnail.mime_type.clone(),
            "thumbnailByteSize": thumbnail.byte_size,
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
    Ok(Json(json!({
        "assetId": asset_id,
        "status": "ready",
        "revision": revision,
        "thumbnailKey": thumbnail.object_key,
        "thumbnailContentHash": thumbnail.content_hash,
        "thumbnailMimeType": thumbnail.mime_type,
        "thumbnailByteSize": thumbnail.byte_size
    })))
}

pub async fn download_url(
    State(state): State<AppState>,
    user: AuthUser,
    Path(asset_id): Path<Uuid>,
    Query(query): Query<DownloadQuery>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT project_id, file_name, object_key, thumbnail_key, content_hash, mime_type, byte_size, thumbnail_content_hash, thumbnail_mime_type, thumbnail_byte_size FROM assets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("asset not found"))?;
    let project_id: Uuid = row.get("project_id");
    require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    let variant = query.variant.as_deref().unwrap_or("original");
    let is_thumbnail = matches!(variant, "thumb" | "thumbnail");
    let (object_key, file_name, mime_type, content_hash, byte_size) = if is_thumbnail {
        (
            row.get::<Option<String>, _>("thumbnail_key")
                .ok_or_else(|| ApiError::not_found("thumbnail not found"))?,
            format!("{}.thumb.jpg", row.get::<String, _>("file_name")),
            row.get::<Option<String>, _>("thumbnail_mime_type")
                .ok_or_else(|| ApiError::not_found("thumbnail metadata not found"))?,
            row.get::<Option<String>, _>("thumbnail_content_hash")
                .ok_or_else(|| ApiError::not_found("thumbnail metadata not found"))?,
            row.get::<Option<i64>, _>("thumbnail_byte_size")
                .ok_or_else(|| ApiError::not_found("thumbnail metadata not found"))?,
        )
    } else {
        (
            row.get::<String, _>("object_key"),
            row.get::<String, _>("file_name"),
            row.get::<String, _>("mime_type"),
            row.get::<String, _>("content_hash"),
            row.get::<i64, _>("byte_size"),
        )
    };
    let ticket = issue_ticket(
        &state,
        &user,
        project_id,
        &object_key,
        Some(asset_id),
        None,
        variant,
        TicketDirection::Get,
        &file_name,
        &mime_type,
        Some(&content_hash),
        Some(byte_size),
    )
    .await?;
    Ok(Json(json!({
        "assetId": asset_id,
        "downloadUrl": ticket["url"].clone(),
        "url": ticket["url"].clone(),
        "variant": variant,
        "fileName": file_name,
        "contentHash": content_hash,
        "mimeType": mime_type,
        "byteSize": byte_size,
        "expiresIn": 300
    })))
}

#[derive(Debug)]
struct ThumbnailInfo {
    object_key: String,
    content_hash: String,
    mime_type: String,
    byte_size: i64,
}

/// Build a bounded JPEG preview from the verified original object and store it
/// under a deterministic key.  The preview has its own digest and byte size;
/// callers must never use the original asset metadata for a thumb ticket.
async fn generate_thumbnail(
    state: &AppState,
    project_id: Uuid,
    asset_id: Uuid,
    object_key: &str,
) -> Result<ThumbnailInfo, ApiError> {
    let suffix = Uuid::new_v4();
    let source_path =
        std::env::temp_dir().join(format!("image-annotation-source-{asset_id}-{suffix}"));
    let thumbnail_path =
        std::env::temp_dir().join(format!("image-annotation-thumb-{asset_id}-{suffix}.jpg"));
    let result = async {
        let object = state
            .s3
            .get_object()
            .bucket(&state.config.s3_bucket)
            .key(object_key)
            .send()
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("thumbnail source not found: {error}"))
            })?;
        let mut source = object.body.into_async_read();
        let mut file = tokio::fs::File::create(&source_path)
            .await
            .map_err(|error| {
                ApiError::internal_message(format!("create thumbnail source file: {error}"))
            })?;
        tokio::io::copy(&mut source, &mut file)
            .await
            .map_err(|error| {
                ApiError::bad_request(format!("read thumbnail source failed: {error}"))
            })?;
        file.flush().await.map_err(|error| {
            ApiError::internal_message(format!("flush thumbnail source file: {error}"))
        })?;
        drop(file);

        let decoded = ImageReader::open(&source_path)
            .map_err(|error| ApiError::bad_request(format!("unsupported image source: {error}")))?
            .with_guessed_format()
            .map_err(|error| ApiError::bad_request(format!("read image format failed: {error}")))?
            .decode()
            .map_err(|error| {
                ApiError::bad_request(format!("decode image source failed: {error}"))
            })?;
        let thumbnail = decoded.thumbnail(640, 640);
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, 85)
            .encode_image(&thumbnail)
            .map_err(|error| {
                ApiError::internal_message(format!("encode thumbnail failed: {error}"))
            })?;
        tokio::fs::write(&thumbnail_path, &encoded)
            .await
            .map_err(|error| {
                ApiError::internal_message(format!("write thumbnail file: {error}"))
            })?;

        let digest = hex::encode(Sha256::digest(&encoded));
        let thumbnail_key = format!("projects/{project_id}/assets/{asset_id}/thumbnail.jpg");
        let byte_stream = ByteStream::from_path(&thumbnail_path)
            .await
            .map_err(|error| ApiError::internal_message(format!("open thumbnail file: {error}")))?;
        state
            .s3
            .put_object()
            .bucket(&state.config.s3_bucket)
            .key(&thumbnail_key)
            .content_type("image/jpeg")
            .metadata("sha256", &digest)
            .body(byte_stream)
            .send()
            .await
            .map_err(|error| {
                ApiError::internal_message(format!("upload thumbnail failed: {error}"))
            })?;

        Ok(ThumbnailInfo {
            object_key: thumbnail_key,
            content_hash: digest,
            mime_type: "image/jpeg".to_string(),
            byte_size: encoded.len() as i64,
        })
    }
    .await;
    let _ = tokio::fs::remove_file(&source_path).await;
    let _ = tokio::fs::remove_file(&thumbnail_path).await;
    result
}

#[derive(Debug, Deserialize, Default)]
pub struct DownloadQuery {
    variant: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::upload_is_complete;

    #[test]
    fn completed_review_status_is_idempotent_when_thumbnail_metadata_is_valid() {
        assert!(upload_is_complete(
            "approved",
            Some("projects/p/assets/a/thumbnail.jpg"),
            Some("hash"),
            Some("image/jpeg"),
            Some(128),
        ));
        assert!(upload_is_complete(
            "pending_review",
            Some("thumb.jpg"),
            Some("hash"),
            Some("image/jpeg"),
            Some(1),
        ));
    }

    #[test]
    fn upload_status_or_incomplete_thumbnail_does_not_count_as_complete() {
        assert!(!upload_is_complete(
            "uploading",
            Some("thumb.jpg"),
            Some("hash"),
            Some("image/jpeg"),
            Some(1),
        ));
        assert!(!upload_is_complete(
            "approved",
            Some("thumb.jpg"),
            None,
            Some("image/jpeg"),
            Some(1),
        ));
        assert!(!upload_is_complete(
            "approved",
            Some("thumb.jpg"),
            Some("hash"),
            Some("image/jpeg"),
            Some(0),
        ));
    }
}

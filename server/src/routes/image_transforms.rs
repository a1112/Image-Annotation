//! Bounded, asynchronous image transformations.
//!
//! A transform is a durable PostgreSQL task.  Enqueueing only records the
//! source object's immutable key/digest and the requested operation; the
//! worker downloads and verifies that source, decodes it with image crate
//! limits, then uploads a new task-scoped object.  No source asset/blob row or
//! source object key is ever updated by this module.

use super::ApiError;
use crate::{
    auth::{
        require_dataset_permission, require_project_permission, AuthUser, DatasetPermission,
        ProjectPermission,
    },
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use image::{
    codecs::jpeg::JpegEncoder, DynamicImage, GenericImageView, ImageFormat, ImageReader, Limits,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, Postgres, Row};
use std::{io::Cursor, time::Duration};
use tokio::{io::AsyncReadExt, task::JoinHandle};
use tokio_util::io::ReaderStream;
use uuid::Uuid;

/// Encoded input, decoded pixels and output encoding are all bounded.  The
/// decoder's allocation limit is deliberately lower than an unconstrained
/// `image::DynamicImage` allocation; the pixel check below remains necessary
/// because some codecs account for allocation on a best-effort basis.
pub const MAX_INPUT_BYTES: i64 = 256 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;
pub const MAX_IMAGE_DIMENSION: u32 = 32_768;
pub const MAX_IMAGE_ALLOC_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CropInput {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ImageTransformRequest {
    /// Required idempotency key.  Repeating it for the same user returns the
    /// original task rather than creating a second output object.
    pub operation_id: String,
    /// `sourceAssetId` is used by the legacy project endpoint.  The
    /// data-project endpoint uses `sourceHash` instead.
    pub source_asset_id: Option<Uuid>,
    pub source_hash: Option<String>,
    /// Optional dataset boundary.  When supplied, the caller must have
    /// dataset View permission and the source must be a member of that
    /// dataset.
    pub dataset_id: Option<Uuid>,
    pub dataset_key: Option<String>,
    #[serde(alias = "outputFormat", alias = "targetFormat")]
    pub format: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub crop: Option<CropInput>,
    #[serde(default)]
    pub rotate: u16,
    #[serde(default = "default_flip")]
    pub flip: String,
    #[serde(default)]
    pub quality: Option<u8>,
}

fn default_flip() -> String {
    "none".to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputFormat {
    Jpeg,
    Png,
    WebP,
    Tiff,
}

impl OutputFormat {
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "jpg" | "jpeg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            "webp" => Ok(Self::WebP),
            "tif" | "tiff" => Ok(Self::Tiff),
            _ => Err(ApiError::bad_request(
                "format must be one of jpeg, png, webp, or tiff",
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Jpeg => "jpeg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Tiff => "tiff",
        }
    }

    fn mime_type(self) -> &'static str {
        match self {
            Self::Jpeg => "image/jpeg",
            Self::Png => "image/png",
            Self::WebP => "image/webp",
            Self::Tiff => "image/tiff",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::WebP => "webp",
            Self::Tiff => "tiff",
        }
    }

    fn image_format(self) -> ImageFormat {
        match self {
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Png => ImageFormat::Png,
            Self::WebP => ImageFormat::WebP,
            Self::Tiff => ImageFormat::Tiff,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flip {
    None,
    Horizontal,
    Vertical,
}

impl Flip {
    fn parse(value: &str) -> Result<Self, ApiError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "none" => Ok(Self::None),
            "horizontal" | "h" => Ok(Self::Horizontal),
            "vertical" | "v" => Ok(Self::Vertical),
            _ => Err(ApiError::bad_request(
                "flip must be none, horizontal, or vertical",
            )),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Horizontal => "horizontal",
            Self::Vertical => "vertical",
        }
    }
}

#[derive(Debug, Clone)]
struct TransformSpec {
    format: OutputFormat,
    width: Option<u32>,
    height: Option<u32>,
    crop: Option<CropInput>,
    rotate: u16,
    flip: Flip,
    quality: u8,
}

fn validate_spec(request: &ImageTransformRequest) -> Result<TransformSpec, ApiError> {
    let operation_id = request.operation_id.trim();
    if operation_id.is_empty() || operation_id.len() > 160 {
        return Err(ApiError::bad_request(
            "operationId is required and must be at most 160 characters",
        ));
    }
    for (field, value) in [("width", request.width), ("height", request.height)] {
        if value.is_some_and(|value| value == 0 || value > MAX_IMAGE_DIMENSION) {
            return Err(ApiError::bad_request(format!(
                "{field} is outside the supported image dimension range"
            )));
        }
    }
    if let Some(crop) = &request.crop {
        if crop.width == 0
            || crop.height == 0
            || crop.x > MAX_IMAGE_DIMENSION
            || crop.y > MAX_IMAGE_DIMENSION
            || crop.width > MAX_IMAGE_DIMENSION
            || crop.height > MAX_IMAGE_DIMENSION
        {
            return Err(ApiError::bad_request(
                "crop is outside the supported image dimension range",
            ));
        }
    }
    if !matches!(request.rotate, 0 | 90 | 180 | 270) {
        return Err(ApiError::bad_request(
            "rotate must be one of 0, 90, 180, or 270",
        ));
    }
    let format = OutputFormat::parse(&request.format)?;
    let flip = Flip::parse(&request.flip)?;
    let quality = request.quality.unwrap_or(85);
    if !(1..=100).contains(&quality) {
        return Err(ApiError::bad_request("quality must be between 1 and 100"));
    }
    Ok(TransformSpec {
        format,
        width: request.width,
        height: request.height,
        crop: request.crop.clone(),
        rotate: request.rotate,
        flip,
        quality,
    })
}

#[derive(Debug, Clone)]
struct SourceObject {
    scope: &'static str,
    project_id: Uuid,
    asset_id: Option<Uuid>,
    dataset_id: Option<Uuid>,
    dataset_key: Option<String>,
    hash: String,
    object_key: String,
    mime_type: String,
    byte_size: i64,
    width: Option<i32>,
    height: Option<i32>,
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.as_bytes().iter().all(|byte| byte.is_ascii_hexdigit())
}

async fn require_data_project_role(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    can_edit: bool,
) -> Result<String, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT dm.role
        FROM data_project_members dm
        JOIN data_projects p ON p.id = dm.project_id
        JOIN users u ON u.id = dm.user_id
        WHERE dm.project_id = $1
          AND dm.user_id = $2
          AND dm.deleted_at IS NULL
          AND p.deleted_at IS NULL
          AND u.deleted_at IS NULL
          AND (p.organization_id IS NULL OR p.organization_id = u.organization_id)
        "#,
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::forbidden("data-project membership is required"))?;
    let role: String = row.get("role");
    let allowed = if can_edit {
        matches!(role.as_str(), "owner" | "admin" | "editor")
    } else {
        matches!(role.as_str(), "owner" | "admin" | "editor" | "viewer")
    };
    if allowed {
        Ok(role)
    } else {
        Err(ApiError::forbidden(format!(
            "role {role} lacks data-project permission"
        )))
    }
}

async fn load_project_source(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    request: &ImageTransformRequest,
) -> Result<SourceObject, ApiError> {
    require_project_permission(state, user.id, project_id, ProjectPermission::ManageData).await?;
    if request.source_hash.is_some() || request.dataset_key.is_some() {
        return Err(ApiError::bad_request(
            "legacy project transforms use sourceAssetId and datasetId",
        ));
    }
    let asset_id = request
        .source_asset_id
        .ok_or_else(|| ApiError::bad_request("sourceAssetId is required"))?;
    let row = sqlx::query(
        "SELECT object_key, content_hash, mime_type, byte_size, width, height FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL",
    )
    .bind(asset_id)
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("asset not found"))?;
    let dataset_id = request.dataset_id;
    if let Some(dataset_id) = dataset_id {
        require_dataset_permission(state, user.id, dataset_id, DatasetPermission::View).await?;
        let linked = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM dataset_items WHERE dataset_id = $1 AND source_project_id = $2 AND source_asset_id = $3)",
        )
        .bind(dataset_id)
        .bind(project_id)
        .bind(asset_id)
        .fetch_one(&state.pool)
        .await?;
        if !linked {
            return Err(ApiError::not_found(
                "asset is not a member of the requested dataset",
            ));
        }
    }
    let hash: String = row.get("content_hash");
    if !valid_hash(&hash) {
        return Err(ApiError::conflict(
            "SOURCE_METADATA_INVALID",
            "source asset metadata has an invalid SHA-256 digest",
            Value::Null,
        ));
    }
    let byte_size: i64 = row.get("byte_size");
    if !(0..=MAX_INPUT_BYTES).contains(&byte_size) {
        return Err(ApiError::bad_request(
            "source image exceeds the transform input size limit",
        ));
    }
    Ok(SourceObject {
        scope: "project",
        project_id,
        asset_id: Some(asset_id),
        dataset_id,
        dataset_key: None,
        hash: hash.to_ascii_lowercase(),
        object_key: row.get("object_key"),
        mime_type: row.get("mime_type"),
        byte_size,
        width: row.get("width"),
        height: row.get("height"),
    })
}

async fn load_data_project_source(
    state: &AppState,
    user: &AuthUser,
    project_id: Uuid,
    request: &ImageTransformRequest,
) -> Result<SourceObject, ApiError> {
    require_data_project_role(state, user.id, project_id, true).await?;
    if request.source_asset_id.is_some() || request.dataset_id.is_some() {
        return Err(ApiError::bad_request(
            "data-project transforms use sourceHash and datasetKey",
        ));
    }
    let hash = request
        .source_hash
        .as_deref()
        .map(str::trim)
        .filter(|value| valid_hash(value))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| ApiError::bad_request("sourceHash must be a SHA-256 hex digest"))?;
    let dataset_key = request
        .dataset_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if let Some(dataset_key) = dataset_key.as_deref() {
        let linked = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM data_project_datasets WHERE project_id = $1 AND dataset_key = $2)",
        )
        .bind(project_id)
        .bind(dataset_key)
        .fetch_one(&state.pool)
        .await?;
        if !linked {
            return Err(ApiError::not_found(
                "dataset membership does not belong to this data project",
            ));
        }
    }
    let row = sqlx::query(
        "SELECT object_key, mime_type, byte_size FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 AND kind = 'image' ORDER BY byte_size LIMIT 1",
    )
    .bind(project_id)
    .bind(&hash)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("data-project image object not found"))?;
    let byte_size: i64 = row.get("byte_size");
    if !(0..=MAX_INPUT_BYTES).contains(&byte_size) {
        return Err(ApiError::bad_request(
            "source image exceeds the transform input size limit",
        ));
    }
    Ok(SourceObject {
        scope: "data-project",
        project_id,
        asset_id: None,
        dataset_id: None,
        dataset_key,
        hash,
        object_key: row.get("object_key"),
        mime_type: row.get("mime_type"),
        byte_size,
        width: None,
        height: None,
    })
}

#[derive(Debug)]
struct TaskRef {
    id: Uuid,
    status: String,
    created: bool,
}

async fn enqueue(
    state: &AppState,
    user: &AuthUser,
    request: &ImageTransformRequest,
    source: &SourceObject,
    spec: &TransformSpec,
) -> Result<TaskRef, ApiError> {
    let operation_id = request.operation_id.trim();
    let crop_values = source_crop_values(&spec.crop);
    let inserted = sqlx::query(
        r#"
        INSERT INTO image_transform_tasks (
            operation_id, source_scope, project_id, source_asset_id,
            source_dataset_id, source_dataset_key, source_hash,
            source_object_key, source_mime_type, source_byte_size,
            source_width, source_height, target_format, requested_width,
            requested_height, crop_x, crop_y, crop_width, crop_height,
            rotate, flip, quality, requested_by
        ) VALUES (
            $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13,
            $14, $15, $16, $17, $18, $19, $20, $21, $22, $23
        )
        ON CONFLICT (requested_by, operation_id) DO NOTHING
        RETURNING id, status
        "#,
    )
    .bind(operation_id)
    .bind(source.scope)
    .bind(source.project_id)
    .bind(source.asset_id)
    .bind(source.dataset_id)
    .bind(source.dataset_key.as_deref())
    .bind(&source.hash)
    .bind(&source.object_key)
    .bind(&source.mime_type)
    .bind(source.byte_size)
    .bind(source.width)
    .bind(source.height)
    .bind(spec.format.as_str())
    .bind(spec.width.map(|value| value as i32))
    .bind(spec.height.map(|value| value as i32))
    .bind(crop_values.0)
    .bind(crop_values.1)
    .bind(crop_values.2)
    .bind(crop_values.3)
    .bind(spec.rotate as i16)
    .bind(spec.flip.as_str())
    .bind(spec.quality as i16)
    .bind(user.id)
    .fetch_optional(&state.pool)
    .await?;
    if let Some(row) = inserted {
        return Ok(TaskRef {
            id: row.get("id"),
            status: row.get("status"),
            created: true,
        });
    }
    let existing = sqlx::query(
        "SELECT id, status, source_scope, project_id, source_asset_id, source_dataset_id, source_dataset_key, source_hash, target_format, requested_width, requested_height, crop_x, crop_y, crop_width, crop_height, rotate, flip, quality FROM image_transform_tasks WHERE requested_by = $1 AND operation_id = $2",
    )
    .bind(user.id)
    .bind(operation_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::conflict("OPERATION_RETRY", "transform is being retried", Value::Null))?;
    let same_source = existing.get::<String, _>("source_scope") == source.scope
        && existing.get::<Uuid, _>("project_id") == source.project_id
        && existing.get::<Option<Uuid>, _>("source_asset_id") == source.asset_id
        && existing.get::<Option<Uuid>, _>("source_dataset_id") == source.dataset_id
        && existing.get::<Option<String>, _>("source_dataset_key") == source.dataset_key
        && existing.get::<String, _>("source_hash") == source.hash
        && existing.get::<String, _>("target_format") == spec.format.as_str()
        && existing.get::<Option<i32>, _>("requested_width")
            == spec.width.map(|value| value as i32)
        && existing.get::<Option<i32>, _>("requested_height")
            == spec.height.map(|value| value as i32)
        && (
            existing.get::<Option<i32>, _>("crop_x"),
            existing.get::<Option<i32>, _>("crop_y"),
            existing.get::<Option<i32>, _>("crop_width"),
            existing.get::<Option<i32>, _>("crop_height"),
        ) == source_crop_values(&spec.crop)
        && existing.get::<i16, _>("rotate") == spec.rotate as i16
        && existing.get::<String, _>("flip") == spec.flip.as_str()
        && existing.get::<i16, _>("quality") == spec.quality as i16;
    if !same_source {
        return Err(ApiError::conflict(
            "OPERATION_ID_REUSED",
            "operationId is already used for a different transform",
            Value::Null,
        ));
    }
    Ok(TaskRef {
        id: existing.get("id"),
        status: existing.get("status"),
        created: false,
    })
}

fn source_crop_values(
    crop: &Option<CropInput>,
) -> (Option<i32>, Option<i32>, Option<i32>, Option<i32>) {
    crop.as_ref().map_or((None, None, None, None), |crop| {
        (
            Some(crop.x as i32),
            Some(crop.y as i32),
            Some(crop.width as i32),
            Some(crop.height as i32),
        )
    })
}

pub async fn create_project_transform(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(request): Json<ImageTransformRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let spec = validate_spec(&request)?;
    let source = load_project_source(&state, &user, project_id, &request).await?;
    let task = enqueue(&state, &user, &request, &source, &spec).await?;
    Ok((
        if task.created {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        Json(json!({
            "taskId": task.id,
            "status": task.status,
            "created": task.created,
            "pollUrl": format!("{}/api/v1/image-transforms/{}", state.config.public_base_url, task.id)
        })),
    ))
}

pub async fn create_data_project_transform(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(request): Json<ImageTransformRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let spec = validate_spec(&request)?;
    let source = load_data_project_source(&state, &user, project_id, &request).await?;
    let task = enqueue(&state, &user, &request, &source, &spec).await?;
    Ok((
        if task.created {
            StatusCode::ACCEPTED
        } else {
            StatusCode::OK
        },
        Json(json!({
            "taskId": task.id,
            "status": task.status,
            "created": task.created,
            "pollUrl": format!("{}/api/v1/image-transforms/{}", state.config.public_base_url, task.id)
        })),
    ))
}

pub async fn get_transform(
    State(state): State<AppState>,
    user: AuthUser,
    Path(task_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        r#"
        SELECT id, operation_id, source_scope, project_id, source_asset_id,
               source_dataset_id, source_dataset_key, source_hash,
               source_mime_type, source_byte_size, source_width, source_height,
               target_format, requested_width, requested_height, crop_x, crop_y,
               crop_width, crop_height, rotate, flip, quality, status,
               output_hash, output_object_key, output_mime_type, output_byte_size,
               output_width, output_height, error_code, error_message,
               requested_by, created_at, updated_at, started_at, finished_at
        FROM image_transform_tasks
        WHERE id = $1
        "#,
    )
    .bind(task_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("image transform task not found"))?;
    let scope: String = row.get("source_scope");
    let project_id: Uuid = row.get("project_id");
    if scope == "project" {
        require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    } else if scope == "data-project" {
        require_data_project_role(&state, user.id, project_id, false).await?;
    } else {
        return Err(ApiError::conflict(
            "TASK_SCOPE_INVALID",
            "transform task has an invalid source scope",
            Value::Null,
        ));
    }
    let status: String = row.get("status");
    let mut response = json!({
        "taskId": task_id,
        "operationId": row.get::<String, _>("operation_id"),
        "sourceScope": scope.clone(),
        "projectId": project_id,
        "sourceAssetId": row.get::<Option<Uuid>, _>("source_asset_id"),
        "sourceDatasetId": row.get::<Option<Uuid>, _>("source_dataset_id"),
        "sourceDatasetKey": row.get::<Option<String>, _>("source_dataset_key"),
        "sourceHash": row.get::<String, _>("source_hash"),
        "sourceMimeType": row.get::<String, _>("source_mime_type"),
        "sourceByteSize": row.get::<i64, _>("source_byte_size"),
        "sourceWidth": row.get::<Option<i32>, _>("source_width"),
        "sourceHeight": row.get::<Option<i32>, _>("source_height"),
        "format": row.get::<String, _>("target_format"),
        "width": row.get::<Option<i32>, _>("requested_width"),
        "height": row.get::<Option<i32>, _>("requested_height"),
        "crop": crop_json(&row),
        "rotate": row.get::<i16, _>("rotate"),
        "flip": row.get::<String, _>("flip"),
        "quality": row.get::<i16, _>("quality"),
        "status": status.clone(),
        "outputHash": row.get::<Option<String>, _>("output_hash"),
        "outputMimeType": row.get::<Option<String>, _>("output_mime_type"),
        "outputByteSize": row.get::<Option<i64>, _>("output_byte_size"),
        "outputWidth": row.get::<Option<i32>, _>("output_width"),
        "outputHeight": row.get::<Option<i32>, _>("output_height"),
        "errorCode": row.get::<Option<String>, _>("error_code"),
        "errorMessage": row.get::<Option<String>, _>("error_message"),
        "createdAt": row.get::<DateTime<Utc>, _>("created_at"),
        "updatedAt": row.get::<DateTime<Utc>, _>("updated_at"),
        "startedAt": row.get::<Option<DateTime<Utc>>, _>("started_at"),
        "finishedAt": row.get::<Option<DateTime<Utc>>, _>("finished_at"),
    });
    if status == "succeeded" {
        if let (Some(hash), Some(mime), Some(size)) = (
            row.get::<Option<String>, _>("output_hash"),
            row.get::<Option<String>, _>("output_mime_type"),
            row.get::<Option<i64>, _>("output_byte_size"),
        ) {
            let output_object_key = row
                .get::<Option<String>, _>("output_object_key")
                .ok_or_else(|| {
                    ApiError::conflict(
                        "TASK_OUTPUT_METADATA_INVALID",
                        "transform output metadata is incomplete",
                        Value::Null,
                    )
                })?;
            // This is an authenticated content route rather than an S3 URL:
            // every download re-checks the current project/data-project ACL,
            // so a membership revocation takes effect immediately.  Keep the
            // local binding here instead of exposing the object-store key.
            let _ = (hash, mime, size, output_object_key);
            response["downloadUrl"] = Value::String(format!(
                "{}/api/v1/image-transforms/{task_id}/download",
                state.config.public_base_url
            ));
        }
    }
    Ok(Json(response))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransformListQuery {
    dataset_key: Option<String>,
    limit: Option<i64>,
}

/// Conversion history is durable and remains discoverable after a browser
/// reload. Listing and each returned result use the current project ACL.
pub async fn list_data_project_transforms(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Query(query): Query<TransformListQuery>,
) -> Result<Json<Vec<Value>>, ApiError> {
    require_data_project_role(&state, user.id, project_id, false).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM image_transform_tasks WHERE source_scope = 'data-project' AND project_id = $1 AND ($2::text IS NULL OR source_dataset_key = $2) ORDER BY created_at DESC, id DESC LIMIT $3",
    )
    .bind(project_id)
    .bind(query.dataset_key)
    .bind(query.limit.unwrap_or(50).clamp(1, 100))
    .fetch_all(&state.pool)
    .await?;
    let mut tasks = Vec::with_capacity(ids.len());
    for id in ids {
        let Json(task) = get_transform(State(state.clone()), user.clone(), Path(id)).await?;
        tasks.push(task);
    }
    Ok(Json(tasks))
}

pub async fn download_transform(
    State(state): State<AppState>,
    user: AuthUser,
    Path(task_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let row = sqlx::query(
        "SELECT source_scope, project_id, status, output_object_key, output_hash, output_mime_type, output_byte_size FROM image_transform_tasks WHERE id = $1",
    )
    .bind(task_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("image transform task not found"))?;
    let source_scope: String = row.get("source_scope");
    let project_id: Uuid = row.get("project_id");
    if source_scope == "project" {
        require_project_permission(&state, user.id, project_id, ProjectPermission::View).await?;
    } else if source_scope == "data-project" {
        require_data_project_role(&state, user.id, project_id, false).await?;
    } else {
        return Err(ApiError::conflict(
            "TASK_SCOPE_INVALID",
            "transform task has an invalid source scope",
            Value::Null,
        ));
    }
    if row.get::<String, _>("status") != "succeeded" {
        return Err(ApiError::conflict(
            "IMAGE_TRANSFORM_NOT_READY",
            "image transform output is not ready",
            json!({"status": row.get::<String, _>("status")}),
        ));
    }
    let object_key = row
        .get::<Option<String>, _>("output_object_key")
        .ok_or_else(|| {
            ApiError::conflict(
                "TASK_OUTPUT_METADATA_INVALID",
                "transform output metadata is incomplete",
                Value::Null,
            )
        })?;
    let mime_type = row
        .get::<Option<String>, _>("output_mime_type")
        .ok_or_else(|| {
            ApiError::conflict(
                "TASK_OUTPUT_METADATA_INVALID",
                "transform output metadata is incomplete",
                Value::Null,
            )
        })?;
    let output_hash = row.get::<Option<String>, _>("output_hash").ok_or_else(|| {
        ApiError::conflict(
            "TASK_OUTPUT_METADATA_INVALID",
            "transform output metadata is incomplete",
            Value::Null,
        )
    })?;
    let expected_size = row
        .get::<Option<i64>, _>("output_byte_size")
        .ok_or_else(|| {
            ApiError::conflict(
                "TASK_OUTPUT_METADATA_INVALID",
                "transform output metadata is incomplete",
                Value::Null,
            )
        })?;
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(&object_key)
        .send()
        .await
        .map_err(|_| ApiError::not_found("image transform output not found"))?;
    if object
        .content_length()
        .is_some_and(|size| size < 0 || size != expected_size)
    {
        return Err(ApiError::conflict(
            "IMAGE_TRANSFORM_OUTPUT_INVALID",
            "image transform output size does not match metadata",
            Value::Null,
        ));
    }
    let mut response =
        Body::from_stream(ReaderStream::new(object.body.into_async_read())).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    if let Ok(value) = HeaderValue::from_str(&expected_size.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    if let Ok(value) = HeaderValue::from_str(&format!(
        "attachment; filename=\"image-transform-{task_id}.{}\"",
        extension_for_mime(&mime_type)
    )) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    if let Ok(value) = HeaderValue::from_str(&output_hash) {
        response.headers_mut().insert("x-content-sha256", value);
    }
    Ok(response)
}

fn crop_json(row: &PgRow) -> Value {
    let values = (
        row.get::<Option<i32>, _>("crop_x"),
        row.get::<Option<i32>, _>("crop_y"),
        row.get::<Option<i32>, _>("crop_width"),
        row.get::<Option<i32>, _>("crop_height"),
    );
    match values {
        (Some(x), Some(y), Some(width), Some(height)) => {
            json!({"x": x, "y": y, "width": width, "height": height})
        }
        _ => Value::Null,
    }
}

fn extension_for_mime(mime: &str) -> &'static str {
    match mime {
        "image/png" => "png",
        "image/webp" => "webp",
        "image/tiff" => "tiff",
        _ => "jpg",
    }
}

#[derive(Debug, Clone)]
struct EncodedImage {
    bytes: Vec<u8>,
    hash: String,
    width: u32,
    height: u32,
    mime_type: &'static str,
}

/// Decode, transform and encode bytes in a blocking worker.  The operation
/// order is crop, rotate, flip, resize; this is deterministic and makes the
/// crop coordinates refer to the original image as a web canvas normally does.
fn transform_bytes(input: &[u8], spec: &TransformSpec) -> Result<EncodedImage, String> {
    if input.len() > MAX_INPUT_BYTES as usize {
        return Err("source image exceeds the transform input size limit".to_string());
    }
    let mut reader = ImageReader::new(Cursor::new(input));
    // `image::io::Limits` is `#[non_exhaustive]` in image 0.25, so build it
    // from the crate's safe defaults and override the bounds we own.  Keeping
    // the default also preserves any future decoder limits added upstream.
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_ALLOC_BYTES);
    reader.limits(limits);
    let mut image = reader
        .with_guessed_format()
        .map_err(|error| format!("read image format: {error}"))?
        .decode()
        .map_err(|error| format!("decode image: {error}"))?;
    ensure_dimensions(&image)?;

    if let Some(crop) = &spec.crop {
        let (width, height) = image.dimensions();
        let right = u64::from(crop.x).saturating_add(u64::from(crop.width));
        let bottom = u64::from(crop.y).saturating_add(u64::from(crop.height));
        if right > u64::from(width) || bottom > u64::from(height) {
            return Err("crop is outside the source image bounds".to_string());
        }
        image = image.crop_imm(crop.x, crop.y, crop.width, crop.height);
    }
    image = match spec.rotate {
        90 => image.rotate90(),
        180 => image.rotate180(),
        270 => image.rotate270(),
        _ => image,
    };
    image = match spec.flip {
        Flip::Horizontal => image.fliph(),
        Flip::Vertical => image.flipv(),
        Flip::None => image,
    };
    if spec.width.is_some() || spec.height.is_some() {
        let (source_width, source_height) = image.dimensions();
        let (target_width, target_height) =
            resize_dimensions(source_width, source_height, spec.width, spec.height)?;
        image = if spec.width.is_some() && spec.height.is_some() {
            image.resize_exact(
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            image.resize(
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            )
        };
    }
    ensure_dimensions(&image)?;
    let (width, height) = image.dimensions();
    let mut output = Cursor::new(Vec::new());
    if spec.format == OutputFormat::Jpeg {
        JpegEncoder::new_with_quality(&mut output, spec.quality)
            .encode_image(&image)
            .map_err(|error| format!("encode JPEG: {error}"))?;
    } else {
        image
            .write_to(&mut output, spec.format.image_format())
            .map_err(|error| format!("encode {}: {error}", spec.format.as_str()))?;
    }
    let bytes = output.into_inner();
    if bytes.len() > MAX_OUTPUT_BYTES {
        return Err("transformed image exceeds the output size limit".to_string());
    }
    let hash = hex::encode(Sha256::digest(&bytes));
    Ok(EncodedImage {
        bytes,
        hash,
        width,
        height,
        mime_type: spec.format.mime_type(),
    })
}

fn ensure_dimensions(image: &DynamicImage) -> Result<(), String> {
    let (width, height) = image.dimensions();
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_IMAGE_PIXELS
    {
        return Err("image dimensions exceed the transform limits".to_string());
    }
    Ok(())
}

fn resize_dimensions(
    source_width: u32,
    source_height: u32,
    requested_width: Option<u32>,
    requested_height: Option<u32>,
) -> Result<(u32, u32), String> {
    let (width, height) = match (requested_width, requested_height) {
        (Some(width), Some(height)) => (width, height),
        (Some(width), None) => {
            let height = (u64::from(source_height) * u64::from(width)
                + u64::from(source_width) / 2)
                / u64::from(source_width);
            (width, height.max(1) as u32)
        }
        (None, Some(height)) => {
            let width = (u64::from(source_width) * u64::from(height)
                + u64::from(source_height) / 2)
                / u64::from(source_height);
            (width.max(1) as u32, height)
        }
        (None, None) => (source_width, source_height),
    };
    if width == 0
        || height == 0
        || width > MAX_IMAGE_DIMENSION
        || height > MAX_IMAGE_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_IMAGE_PIXELS
    {
        return Err("requested dimensions exceed the transform limits".to_string());
    }
    Ok((width, height))
}

#[derive(Debug, Clone)]
struct JobFailure {
    code: &'static str,
    message: String,
}

impl JobFailure {
    fn source(message: impl Into<String>) -> Self {
        Self {
            code: "SOURCE_READ_FAILED",
            message: message.into(),
        }
    }

    fn transform(message: impl Into<String>) -> Self {
        Self {
            code: "TRANSFORM_FAILED",
            message: message.into(),
        }
    }
}

#[derive(Debug)]
struct QueuedTask {
    id: Uuid,
    source_scope: String,
    project_id: Uuid,
    source_asset_id: Option<Uuid>,
    source_dataset_id: Option<Uuid>,
    source_dataset_key: Option<String>,
    source_hash: String,
    source_object_key: String,
    source_byte_size: i64,
    target_format: OutputFormat,
    requested_width: Option<u32>,
    requested_height: Option<u32>,
    crop: Option<CropInput>,
    rotate: u16,
    flip: Flip,
    quality: u8,
    requested_by: Uuid,
}

fn queued_task(row: &PgRow) -> Result<QueuedTask, JobFailure> {
    let target_format = OutputFormat::parse_for_worker(&row.get::<String, _>("target_format"))?;
    let flip = Flip::parse_for_worker(&row.get::<String, _>("flip"))?;
    let quality = row.get::<i16, _>("quality");
    let quality =
        u8::try_from(quality).map_err(|_| JobFailure::transform("invalid task quality"))?;
    let crop = match (
        row.get::<Option<i32>, _>("crop_x"),
        row.get::<Option<i32>, _>("crop_y"),
        row.get::<Option<i32>, _>("crop_width"),
        row.get::<Option<i32>, _>("crop_height"),
    ) {
        (Some(x), Some(y), Some(width), Some(height)) => Some(CropInput {
            x: u32::try_from(x).map_err(|_| JobFailure::transform("invalid crop x"))?,
            y: u32::try_from(y).map_err(|_| JobFailure::transform("invalid crop y"))?,
            width: u32::try_from(width).map_err(|_| JobFailure::transform("invalid crop width"))?,
            height: u32::try_from(height)
                .map_err(|_| JobFailure::transform("invalid crop height"))?,
        }),
        (None, None, None, None) => None,
        _ => return Err(JobFailure::transform("invalid task crop")),
    };
    let rotate = row.get::<i16, _>("rotate");
    let rotate =
        u16::try_from(rotate).map_err(|_| JobFailure::transform("invalid task rotation"))?;
    if !matches!(rotate, 0 | 90 | 180 | 270) {
        return Err(JobFailure::transform("invalid task rotation"));
    }
    Ok(QueuedTask {
        id: row.get("id"),
        source_scope: row.get("source_scope"),
        project_id: row.get("project_id"),
        source_asset_id: row.get("source_asset_id"),
        source_dataset_id: row.get("source_dataset_id"),
        source_dataset_key: row.get("source_dataset_key"),
        source_hash: row.get("source_hash"),
        source_object_key: row.get("source_object_key"),
        source_byte_size: row.get("source_byte_size"),
        target_format,
        requested_width: row
            .get::<Option<i32>, _>("requested_width")
            .map(|value| {
                u32::try_from(value).map_err(|_| JobFailure::transform("invalid task width"))
            })
            .transpose()?,
        requested_height: row
            .get::<Option<i32>, _>("requested_height")
            .map(|value| {
                u32::try_from(value).map_err(|_| JobFailure::transform("invalid task height"))
            })
            .transpose()?,
        crop,
        rotate,
        flip,
        quality,
        requested_by: row.get("requested_by"),
    })
}

impl OutputFormat {
    fn parse_for_worker(value: &str) -> Result<Self, JobFailure> {
        match value {
            "jpeg" => Ok(Self::Jpeg),
            "png" => Ok(Self::Png),
            "webp" => Ok(Self::WebP),
            "tiff" => Ok(Self::Tiff),
            _ => Err(JobFailure::transform("invalid task output format")),
        }
    }
}

impl Flip {
    fn parse_for_worker(value: &str) -> Result<Self, JobFailure> {
        match value {
            "none" => Ok(Self::None),
            "horizontal" => Ok(Self::Horizontal),
            "vertical" => Ok(Self::Vertical),
            _ => Err(JobFailure::transform("invalid task flip")),
        }
    }
}

pub fn spawn_worker(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(mut lock) = (match acquire_worker_lock(&state).await {
                Ok(lock) => lock,
                Err(error) => {
                    tracing::error!(error = %error, "image transform worker lock failed");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }) else {
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            if let Err(error) = recover_running_tasks(&state).await {
                tracing::error!(error = %error, "image transform task recovery failed");
            }
            loop {
                match claim_next_task(&state).await {
                    Ok(Some(task)) => process_task(&state, task).await,
                    Ok(None) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(error) => {
                        // ApiError intentionally does not implement Display;
                        // log only its stable public code rather than the
                        // request message/details.
                        tracing::error!(code = error.code, "image transform task claim failed");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
                if sqlx::query("SELECT 1").execute(&mut *lock).await.is_err() {
                    break;
                }
            }
        }
    })
}

async fn acquire_worker_lock(
    state: &AppState,
) -> Result<Option<sqlx::pool::PoolConnection<Postgres>>, sqlx::Error> {
    let mut connection = state.pool.acquire().await?;
    connection.close_on_drop();
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_lock(hashtext('image_annotation_image_transforms_worker'))",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(acquired.then_some(connection))
}

async fn recover_running_tasks(state: &AppState) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE image_transform_tasks SET status = 'queued', started_at = NULL, updated_at = now() WHERE status = 'running'",
    )
    .execute(&state.pool)
    .await
    .map(|_| ())
}

async fn claim_next_task(state: &AppState) -> Result<Option<QueuedTask>, ApiError> {
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        r#"
        SELECT id, source_scope, project_id, source_hash, source_object_key,
               source_asset_id, source_dataset_id, source_dataset_key,
               source_byte_size, target_format, requested_width,
               requested_height, crop_x, crop_y, crop_width, crop_height,
               rotate, flip, quality, requested_by
        FROM image_transform_tasks
        WHERE status = 'queued'
        ORDER BY created_at, id
        FOR UPDATE SKIP LOCKED
        LIMIT 1
        "#,
    )
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Ok(None);
    };
    let task = queued_task(&row).map_err(|error| ApiError::internal_message(error.message))?;
    sqlx::query(
        "UPDATE image_transform_tasks SET status = 'running', started_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(task.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(task))
}

async fn read_source_bytes(state: &AppState, task: &QueuedTask) -> Result<Vec<u8>, JobFailure> {
    if !(0..=MAX_INPUT_BYTES).contains(&task.source_byte_size) {
        return Err(JobFailure::source(
            "source object exceeds the transform input limit",
        ));
    }
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(&task.source_object_key)
        .send()
        .await
        .map_err(|_| JobFailure::source("source object could not be read"))?;
    if object
        .content_length()
        .is_some_and(|size| size < 0 || size > MAX_INPUT_BYTES)
    {
        return Err(JobFailure::source(
            "source object exceeds the transform input limit",
        ));
    }
    let hinted = object
        .content_length()
        .unwrap_or(task.source_byte_size.max(0)) as usize;
    let mut bytes = Vec::with_capacity(hinted.min(MAX_INPUT_BYTES as usize));
    let mut reader = object.body.into_async_read();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut buffer)
            .await
            .map_err(|_| JobFailure::source("source object body could not be read"))?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > MAX_INPUT_BYTES as usize {
            return Err(JobFailure::source(
                "source object exceeds the transform input limit",
            ));
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as i64 != task.source_byte_size {
        return Err(JobFailure::source(
            "source object size no longer matches metadata",
        ));
    }
    let digest = hex::encode(Sha256::digest(&bytes));
    if digest != task.source_hash.to_ascii_lowercase() {
        return Err(JobFailure::source(
            "source object digest no longer matches metadata",
        ));
    }
    Ok(bytes)
}

async fn ensure_task_access(state: &AppState, task: &QueuedTask) -> Result<(), JobFailure> {
    match task.source_scope.as_str() {
        "project" => {
            require_project_permission(
                state,
                task.requested_by,
                task.project_id,
                ProjectPermission::ManageData,
            )
            .await
            .map_err(|_| JobFailure::source("source project access is no longer available"))?;
            let asset_id = task
                .source_asset_id
                .ok_or_else(|| JobFailure::source("source asset metadata is incomplete"))?;
            let asset_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM assets WHERE id = $1 AND project_id = $2 AND deleted_at IS NULL)",
            )
            .bind(asset_id)
            .bind(task.project_id)
            .fetch_one(&state.pool)
            .await
            .map_err(|_| JobFailure::source("source asset metadata could not be checked"))?;
            if !asset_exists {
                return Err(JobFailure::source("source asset is no longer available"));
            }
            if let Some(dataset_id) = task.source_dataset_id {
                require_dataset_permission(
                    state,
                    task.requested_by,
                    dataset_id,
                    DatasetPermission::View,
                )
                .await
                .map_err(|_| JobFailure::source("source dataset access is no longer available"))?;
                let linked = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM dataset_items WHERE dataset_id = $1 AND source_project_id = $2 AND source_asset_id = $3)",
                )
                .bind(dataset_id)
                .bind(task.project_id)
                .bind(asset_id)
                .fetch_one(&state.pool)
                .await
                .map_err(|_| JobFailure::source("source dataset membership could not be checked"))?;
                if !linked {
                    return Err(JobFailure::source(
                        "source asset is no longer a member of the dataset",
                    ));
                }
            }
        }
        "data-project" => {
            require_data_project_role(state, task.requested_by, task.project_id, true)
                .await
                .map_err(|_| JobFailure::source("data-project access is no longer available"))?;
            if let Some(dataset_key) = task.source_dataset_key.as_deref() {
                let linked = sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS (SELECT 1 FROM data_project_datasets WHERE project_id = $1 AND dataset_key = $2)",
                )
                .bind(task.project_id)
                .bind(dataset_key)
                .fetch_one(&state.pool)
                .await
                .map_err(|_| JobFailure::source("data-project dataset membership could not be checked"))?;
                if !linked {
                    return Err(JobFailure::source(
                        "data-project dataset membership is no longer available",
                    ));
                }
            }
            let blob_exists = sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS (SELECT 1 FROM data_project_blobs WHERE project_id = $1 AND content_hash = $2 AND kind = 'image')",
            )
            .bind(task.project_id)
            .bind(&task.source_hash)
            .fetch_one(&state.pool)
            .await
            .map_err(|_| JobFailure::source("source data-project object metadata could not be checked"))?;
            if !blob_exists {
                return Err(JobFailure::source(
                    "source data-project object is no longer available",
                ));
            }
        }
        _ => return Err(JobFailure::source("source scope is invalid")),
    }
    Ok(())
}

async fn process_task(state: &AppState, task: QueuedTask) {
    let result = process_task_inner(state, &task).await;
    if let Err(error) = result {
        tracing::error!(task_id = %task.id, code = error.code, message = %error.message, "image transform failed");
        if let Err(update_error) = mark_failed(state, task.id, error.code, &error.message).await {
            tracing::error!(task_id = %task.id, error = %update_error, "failed to persist image transform error");
        }
    }
}

async fn process_task_inner(state: &AppState, task: &QueuedTask) -> Result<(), JobFailure> {
    ensure_task_access(state, task).await?;
    let input = read_source_bytes(state, task).await?;
    let spec = TransformSpec {
        format: task.target_format,
        width: task.requested_width,
        height: task.requested_height,
        crop: task.crop.clone(),
        rotate: task.rotate,
        flip: task.flip,
        quality: task.quality,
    };
    let encoded = tokio::task::spawn_blocking(move || transform_bytes(&input, &spec))
        .await
        .map_err(|_| JobFailure::transform("image transform worker stopped"))?
        .map_err(JobFailure::transform)?;
    let output_key = format!(
        "{}/{}/image-transform/{}/output.{}",
        if task.source_scope == "data-project" {
            "data-projects"
        } else {
            "projects"
        },
        task.project_id,
        task.id,
        task.target_format.extension()
    );
    state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&output_key)
        .content_type(encoded.mime_type)
        .metadata("sha256", encoded.hash.clone())
        .body(ByteStream::from(encoded.bytes.clone()))
        .send()
        .await
        .map_err(|_| JobFailure::transform("transformed object could not be stored"))?;

    let mut tx = state
        .pool
        .begin()
        .await
        .map_err(|_| JobFailure::transform("transform result could not be persisted"))?;
    sqlx::query(
        "UPDATE image_transform_tasks SET status = 'succeeded', output_hash = $2, output_object_key = $3, output_mime_type = $4, output_byte_size = $5, output_width = $6, output_height = $7, error_code = NULL, error_message = NULL, finished_at = now(), updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(task.id)
    .bind(&encoded.hash)
    .bind(&output_key)
    .bind(encoded.mime_type)
    .bind(encoded.bytes.len() as i64)
    .bind(encoded.width as i32)
    .bind(encoded.height as i32)
    .execute(&mut *tx)
    .await
    .map_err(|_| JobFailure::transform("transform result could not be persisted"))?;
    if task.source_scope == "data-project" {
        sqlx::query(
            "INSERT INTO data_project_blobs (project_id, content_hash, byte_size, kind, mime_type, object_key, created_by) VALUES ($1, $2, $3, 'image', $4, $5, $6) ON CONFLICT (project_id, content_hash, byte_size) DO NOTHING",
        )
        .bind(task.project_id)
        .bind(&encoded.hash)
        .bind(encoded.bytes.len() as i64)
        .bind(encoded.mime_type)
        .bind(&output_key)
        .bind(task.requested_by)
        .execute(&mut *tx)
        .await
        .map_err(|_| JobFailure::transform("data-project output metadata could not be persisted"))?;
    }
    tx.commit()
        .await
        .map_err(|_| JobFailure::transform("transform result could not be committed"))?;
    Ok(())
}

async fn mark_failed(
    state: &AppState,
    task_id: Uuid,
    code: &str,
    message: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE image_transform_tasks SET status = 'failed', error_code = $2, error_message = $3, finished_at = now(), updated_at = now() WHERE id = $1 AND status = 'running'",
    )
    .bind(task_id)
    .bind(code)
    .bind(message)
    .execute(&state.pool)
    .await
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgb};

    fn source_png() -> Vec<u8> {
        let image = ImageBuffer::from_fn(3, 2, |x, y| {
            Rgb([x as u8 * 50 + 10, y as u8 * 80 + 20, (x + y) as u8 * 20])
        });
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut output, ImageFormat::Png)
            .expect("PNG source encoding");
        output.into_inner()
    }

    fn spec(format: OutputFormat) -> TransformSpec {
        TransformSpec {
            format,
            width: None,
            height: None,
            crop: None,
            rotate: 0,
            flip: Flip::None,
            quality: 90,
        }
    }

    #[test]
    fn real_decode_transform_changes_geometry_and_pixels() {
        let input = source_png();
        let transformed = transform_bytes(
            &input,
            &TransformSpec {
                format: OutputFormat::Png,
                width: None,
                height: None,
                crop: Some(CropInput {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                rotate: 90,
                flip: Flip::Horizontal,
                quality: 90,
            },
        )
        .expect("PNG transform");
        assert_eq!((transformed.width, transformed.height), (2, 2));
        assert_ne!(input, transformed.bytes);
        let decoded = ImageReader::new(Cursor::new(&transformed.bytes))
            .with_guessed_format()
            .expect("output format")
            .decode()
            .expect("output decode");
        assert_eq!(decoded.dimensions(), (2, 2));
        assert_eq!(
            image::guess_format(&transformed.bytes).unwrap(),
            ImageFormat::Png
        );
    }

    #[test]
    fn real_decode_supports_all_requested_output_formats() {
        let input = source_png();
        for format in [
            OutputFormat::Jpeg,
            OutputFormat::Png,
            OutputFormat::WebP,
            OutputFormat::Tiff,
        ] {
            let transformed = transform_bytes(&input, &spec(format)).expect("format transform");
            assert!(!transformed.bytes.is_empty());
            assert_eq!(
                image::guess_format(&transformed.bytes).unwrap(),
                format.image_format()
            );
            assert_eq!((transformed.width, transformed.height), (3, 2));
        }
    }

    #[test]
    fn resource_limits_reject_oversized_requested_dimensions() {
        let input = source_png();
        let result = transform_bytes(
            &input,
            &TransformSpec {
                width: Some(MAX_IMAGE_DIMENSION),
                height: Some(MAX_IMAGE_DIMENSION),
                ..spec(OutputFormat::Png)
            },
        );
        assert!(result.is_err());
    }
}

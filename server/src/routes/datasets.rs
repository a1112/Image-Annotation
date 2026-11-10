//! Organization-scoped dataset hosting.
//!
//! A dataset is a durable catalog of verified project assets.  Capturing a
//! project copies metadata and annotation JSON into `dataset_items` while
//! `dataset_blobs` keeps the original MinIO object key, so capture and reuse
//! never upload the same original bytes twice.  Versions are immutable JSON
//! manifests; materialized project annotations are independent rows.

use super::{
    projects::{write_audit, write_change},
    ApiError,
};
use crate::{
    auth::{
        require_dataset_permission, require_project_permission, AuthUser, DatasetPermission,
        ProjectPermission,
    },
    state::AppState,
};
use axum::{
    extract::{Path, State},
    Json,
};
use chrono::{Duration, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, Pool, Postgres, Row, Transaction};
use std::collections::HashMap;
use uuid::Uuid;

pub const DEFAULT_QUOTA_BYTES: i64 = 10 * 1024 * 1024 * 1024;
pub const DEFAULT_QUOTA_ASSETS: i64 = 100_000;
const RESERVATION_TTL_MINUTES: i64 = 30;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateDataset {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateDataset {
    pub name: Option<String>,
    pub description: Option<String>,
    pub base_revision: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DatasetMemberInput {
    #[serde(default)]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub email: Option<String>,
    pub role: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureRequest {
    pub project_id: Uuid,
    pub operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateVersionRequest {
    pub name: String,
    pub operation_id: String,
    pub base_revision: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReuseRequest {
    pub version_id: Uuid,
    pub target_project_id: Option<Uuid>,
    pub project_name: Option<String>,
    pub operation_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageQuotaUpdate {
    pub limit_bytes: Option<i64>,
    pub max_assets: Option<i64>,
}

fn validate_operation_id(value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > 200 {
        return Err(ApiError::bad_request(
            "operationId is required and must be at most 200 characters",
        ));
    }
    Ok(())
}

fn validate_name(value: &str, field: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > 240 {
        return Err(ApiError::bad_request(format!(
            "{field} is required and must be at most 240 characters"
        )));
    }
    Ok(())
}

fn normalize_dataset_member_email(
    user_id: Option<Uuid>,
    email: Option<&str>,
) -> Result<Option<String>, ApiError> {
    match (user_id, email) {
        (Some(_), None) => Ok(None),
        (None, Some(email)) => {
            let normalized = email.trim().to_lowercase();
            if normalized.is_empty() || !normalized.contains('@') || normalized.len() > 320 {
                return Err(ApiError::bad_request(
                    "a valid email is required when userId is omitted",
                ));
            }
            Ok(Some(normalized))
        }
        (Some(_), Some(_)) | (None, None) => Err(ApiError::bad_request(
            "exactly one of userId or email is required",
        )),
    }
}

fn json_hash(value: &Value) -> Result<String, ApiError> {
    let bytes = serde_json::to_vec(value)
        .map_err(|error| ApiError::internal_message(format!("encode dataset JSON: {error}")))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn operation_hash(operation: &str, request: Value) -> Result<String, ApiError> {
    json_hash(&json!({ "operation": operation, "request": request }))
}

fn dataset_summary_json(row: &PgRow) -> Value {
    json!({
        "id": row.get::<Uuid, _>("id"),
        "organizationId": row.get::<Uuid, _>("organization_id"),
        "ownerId": row.get::<Uuid, _>("owner_id"),
        "name": row.get::<String, _>("name"),
        "description": row.get::<String, _>("description"),
        "schema": row.get::<Value, _>("schema_json"),
        "status": row.get::<String, _>("status"),
        "revision": row.get::<i64, _>("revision"),
        "itemCount": row.get::<i64, _>("item_count"),
        "totalBytes": row.get::<i64, _>("total_bytes"),
        "role": row.get::<String, _>("role"),
        "latestVersionId": row.get::<Option<Uuid>, _>("latest_version_id"),
        "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at"),
        "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
    })
}

async fn dataset_row(
    pool: &Pool<Postgres>,
    dataset_id: Uuid,
    user_id: Uuid,
) -> Result<PgRow, ApiError> {
    sqlx::query(
        r#"
        SELECT d.id, d.organization_id, d.owner_id, d.name, d.description,
            d.schema_json, d.status, d.revision, d.created_at, d.updated_at,
            dm.role,
            (SELECT COUNT(*) FROM dataset_items di WHERE di.dataset_id = d.id) AS item_count,
            (SELECT COALESCE(SUM(db.byte_size + COALESCE(db.thumbnail_byte_size, 0)), 0)::BIGINT
             FROM dataset_blobs db
             WHERE db.id IN (SELECT DISTINCT di2.blob_id FROM dataset_items di2 WHERE di2.dataset_id = d.id)) AS total_bytes,
            (SELECT dv.id FROM dataset_versions dv WHERE dv.dataset_id = d.id ORDER BY dv.version_number DESC LIMIT 1) AS latest_version_id
        FROM datasets d
        JOIN dataset_members dm ON dm.dataset_id = d.id AND dm.user_id = $2 AND dm.deleted_at IS NULL
        WHERE d.id = $1 AND d.deleted_at IS NULL
        "#,
    )
    .bind(dataset_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))
}

async fn ensure_org_quota_tx(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "INSERT INTO organization_quotas (organization_id) VALUES ($1) ON CONFLICT (organization_id) DO NOTHING",
    )
    .bind(organization_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn cleanup_expired_reservations_tx(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
) -> Result<(), ApiError> {
    // Every quota mutation takes this lock before touching reservations. This
    // prevents a release racing a reserve from deadlocking in the opposite
    // order and makes the counters a single serialized ledger.
    sqlx::query(
        "SELECT organization_id FROM organization_quotas WHERE organization_id = $1 FOR UPDATE",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;
    let rows = sqlx::query(
        r#"
        DELETE FROM quota_reservations AS qr
        WHERE qr.organization_id = $1
          AND qr.consumed_at IS NULL
          AND qr.released_at IS NULL
          AND qr.expires_at <= now()
          -- A queued/running dataset job owns its reservation even when the
          -- nominal TTL has elapsed.  The worker is responsible for
          -- releasing it on failure; deleting it here would let the job
          -- finish without a corresponding quota ledger entry.
          AND NOT EXISTS (
              SELECT 1
              FROM dataset_jobs AS dj
              WHERE dj.quota_reservation_id = qr.id
                AND dj.status IN ('queued', 'running')
          )
        RETURNING qr.byte_size, qr.asset_count
        "#,
    )
    .bind(organization_id)
    .fetch_all(&mut **tx)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let bytes: i64 = rows.iter().map(|row| row.get::<i64, _>("byte_size")).sum();
    let assets: i64 = rows
        .iter()
        .map(|row| row.get::<i64, _>("asset_count"))
        .sum();
    sqlx::query(
        "UPDATE organization_quotas SET reserved_bytes = GREATEST(0, reserved_bytes - $2), reserved_assets = GREATEST(0, reserved_assets - $3), revision = revision + 1, updated_at = now() WHERE organization_id = $1",
    )
    .bind(organization_id)
    .bind(bytes)
    .bind(assets)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Reserve physical bytes and/or logical image rows under a row lock.  The
/// returned flag is false only when this resource was already consumed, which
/// makes a repeated completion harmless.
pub(crate) async fn reserve_quota_tx(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
    byte_size: i64,
    asset_count: i64,
) -> Result<bool, ApiError> {
    if byte_size < 0 || asset_count < 0 {
        return Err(ApiError::bad_request("quota reservation is invalid"));
    }
    ensure_org_quota_tx(tx, organization_id).await?;
    cleanup_expired_reservations_tx(tx, organization_id).await?;
    let quota = sqlx::query(
        "SELECT limit_bytes, max_assets, used_bytes, used_assets, reserved_bytes, reserved_assets FROM organization_quotas WHERE organization_id = $1 FOR UPDATE",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;
    let existing = sqlx::query(
        "SELECT id, byte_size, asset_count, consumed_at, released_at FROM quota_reservations WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3 FOR UPDATE",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = existing {
        if row
            .get::<Option<chrono::DateTime<Utc>>, _>("consumed_at")
            .is_some()
        {
            return Ok(false);
        }
        let active = row
            .get::<Option<chrono::DateTime<Utc>>, _>("released_at")
            .is_none();
        let old_bytes = if active {
            row.get::<i64, _>("byte_size")
        } else {
            0
        };
        let old_assets = if active {
            row.get::<i64, _>("asset_count")
        } else {
            0
        };
        let next_reserved_bytes = quota.get::<i64, _>("reserved_bytes") - old_bytes + byte_size;
        let next_reserved_assets =
            quota.get::<i64, _>("reserved_assets") - old_assets + asset_count;
        if quota.get::<i64, _>("used_bytes") + next_reserved_bytes > quota.get("limit_bytes")
            || quota.get::<i64, _>("used_assets") + next_reserved_assets > quota.get("max_assets")
        {
            return Err(ApiError::conflict(
                "STORAGE_QUOTA_EXCEEDED",
                "organization storage quota exceeded",
                json!({
                    "limitBytes": quota.get::<i64, _>("limit_bytes"),
                    "usedBytes": quota.get::<i64, _>("used_bytes"),
                    "reservedBytes": quota.get::<i64, _>("reserved_bytes"),
                    "maxAssets": quota.get::<i64, _>("max_assets"),
                    "usedAssets": quota.get::<i64, _>("used_assets"),
                    "reservedAssets": quota.get::<i64, _>("reserved_assets")
                }),
            ));
        }
        sqlx::query(
            "UPDATE quota_reservations SET byte_size = $4, asset_count = $5, expires_at = now() + ($6 || ' minutes')::interval, consumed_at = NULL, consumed_byte_size = NULL, consumed_asset_count = NULL, released_at = NULL WHERE id = $1 AND organization_id = $2 AND resource_type = $3",
        )
        .bind(row.get::<Uuid, _>("id"))
        .bind(organization_id)
        .bind(resource_type)
        .bind(byte_size)
        .bind(asset_count)
        .bind(RESERVATION_TTL_MINUTES)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            "UPDATE organization_quotas SET reserved_bytes = $2, reserved_assets = $3, revision = revision + 1, updated_at = now() WHERE organization_id = $1",
        )
        .bind(organization_id)
        .bind(next_reserved_bytes)
        .bind(next_reserved_assets)
        .execute(&mut **tx)
        .await?;
        return Ok(true);
    }
    let next_reserved_bytes = quota.get::<i64, _>("reserved_bytes") + byte_size;
    let next_reserved_assets = quota.get::<i64, _>("reserved_assets") + asset_count;
    if quota.get::<i64, _>("used_bytes") + next_reserved_bytes > quota.get("limit_bytes")
        || quota.get::<i64, _>("used_assets") + next_reserved_assets > quota.get("max_assets")
    {
        return Err(ApiError::conflict(
            "STORAGE_QUOTA_EXCEEDED",
            "organization storage quota exceeded",
            json!({
                "limitBytes": quota.get::<i64, _>("limit_bytes"),
                "usedBytes": quota.get::<i64, _>("used_bytes"),
                "reservedBytes": quota.get::<i64, _>("reserved_bytes"),
                "maxAssets": quota.get::<i64, _>("max_assets"),
                "usedAssets": quota.get::<i64, _>("used_assets"),
                "reservedAssets": quota.get::<i64, _>("reserved_assets")
            }),
        ));
    }
    sqlx::query(
        "INSERT INTO quota_reservations (organization_id, resource_type, resource_id, byte_size, asset_count, expires_at) VALUES ($1, $2, $3, $4, $5, now() + ($6 || ' minutes')::interval)",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .bind(byte_size)
    .bind(asset_count)
    .bind(RESERVATION_TTL_MINUTES)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE organization_quotas SET reserved_bytes = $2, reserved_assets = $3, revision = revision + 1, updated_at = now() WHERE organization_id = $1",
    )
    .bind(organization_id)
    .bind(next_reserved_bytes)
    .bind(next_reserved_assets)
    .execute(&mut **tx)
    .await?;
    Ok(true)
}

pub(crate) async fn consume_quota_tx(
    tx: &mut Transaction<'_, Postgres>,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
    actual_bytes: i64,
    actual_assets: i64,
) -> Result<(), ApiError> {
    if actual_bytes < 0 || actual_assets < 0 {
        return Err(ApiError::bad_request("quota consumption is invalid"));
    }
    ensure_org_quota_tx(tx, organization_id).await?;
    let quota = sqlx::query(
        "SELECT limit_bytes, max_assets, used_bytes, used_assets, reserved_bytes, reserved_assets FROM organization_quotas WHERE organization_id = $1 FOR UPDATE",
    )
    .bind(organization_id)
    .fetch_one(&mut **tx)
    .await?;
    let reservation = sqlx::query(
        "SELECT id, byte_size, asset_count, consumed_at, released_at, consumed_byte_size, consumed_asset_count FROM quota_reservations WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3 FOR UPDATE",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = reservation else {
        return Err(ApiError::conflict(
            "QUOTA_RESERVATION_MISSING",
            "quota reservation is missing; the operation must be retried from reservation",
            json!({
                "organizationId": organization_id,
                "resourceType": resource_type,
                "resourceId": resource_id,
            }),
        ));
    };
    if row
        .get::<Option<chrono::DateTime<Utc>>, _>("consumed_at")
        .is_some()
    {
        let consumed_bytes = row.get::<Option<i64>, _>("consumed_byte_size");
        let consumed_assets = row.get::<Option<i64>, _>("consumed_asset_count");
        if consumed_bytes == Some(actual_bytes) && consumed_assets == Some(actual_assets) {
            // A retry of the same committed consumption is idempotent.  The
            // recorded amounts, rather than the original reservation, are
            // compared because jobs reserve an estimate and consume the
            // verified archive/output size.
            return Ok(());
        }
        return Err(ApiError::conflict(
            "QUOTA_RESERVATION_REPLAY_MISMATCH",
            "quota reservation was already consumed with different amounts",
            json!({
                "resourceType": resource_type,
                "resourceId": resource_id,
                "consumedBytes": consumed_bytes,
                "consumedAssets": consumed_assets,
                "requestedBytes": actual_bytes,
                "requestedAssets": actual_assets,
            }),
        ));
    }
    if row
        .get::<Option<chrono::DateTime<Utc>>, _>("released_at")
        .is_some()
    {
        return Err(ApiError::conflict(
            "QUOTA_RESERVATION_RELEASED",
            "quota reservation was released; create a new reservation before consuming",
            json!({
                "organizationId": organization_id,
                "resourceType": resource_type,
                "resourceId": resource_id,
            }),
        ));
    }
    let reserved_bytes: i64 = row.get("byte_size");
    let reserved_assets: i64 = row.get("asset_count");
    let next_used_bytes = quota.get::<i64, _>("used_bytes") + actual_bytes;
    let next_used_assets = quota.get::<i64, _>("used_assets") + actual_assets;
    let other_reserved_bytes = quota.get::<i64, _>("reserved_bytes") - reserved_bytes;
    let other_reserved_assets = quota.get::<i64, _>("reserved_assets") - reserved_assets;
    if next_used_bytes + other_reserved_bytes > quota.get("limit_bytes")
        || next_used_assets + other_reserved_assets > quota.get("max_assets")
    {
        return Err(ApiError::conflict(
            "STORAGE_QUOTA_EXCEEDED",
            "organization storage quota exceeded",
            Value::Null,
        ));
    }
    sqlx::query(
        "UPDATE organization_quotas SET used_bytes = $2, used_assets = $3, reserved_bytes = GREATEST(0, reserved_bytes - $4), reserved_assets = GREATEST(0, reserved_assets - $5), revision = revision + 1, updated_at = now() WHERE organization_id = $1",
    )
    .bind(organization_id)
    .bind(next_used_bytes)
    .bind(next_used_assets)
    .bind(reserved_bytes)
    .bind(reserved_assets)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE quota_reservations SET consumed_at = now(), consumed_byte_size = $2, consumed_asset_count = $3 WHERE id = $1",
    )
        .bind(row.get::<Uuid, _>("id"))
        .bind(actual_bytes)
        .bind(actual_assets)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

pub(crate) async fn release_quota_reservation(
    pool: &Pool<Postgres>,
    organization_id: Uuid,
    resource_type: &str,
    resource_id: Uuid,
) -> Result<(), ApiError> {
    let mut tx = pool.begin().await?;
    ensure_org_quota_tx(&mut tx, organization_id).await?;
    sqlx::query(
        "SELECT organization_id FROM organization_quotas WHERE organization_id = $1 FOR UPDATE",
    )
    .bind(organization_id)
    .fetch_one(&mut *tx)
    .await?;
    let reservation = sqlx::query(
        "SELECT id, byte_size, asset_count FROM quota_reservations WHERE organization_id = $1 AND resource_type = $2 AND resource_id = $3 AND consumed_at IS NULL AND released_at IS NULL FOR UPDATE",
    )
    .bind(organization_id)
    .bind(resource_type)
    .bind(resource_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(row) = reservation {
        sqlx::query(
            "UPDATE organization_quotas SET reserved_bytes = GREATEST(0, reserved_bytes - $2), reserved_assets = GREATEST(0, reserved_assets - $3), revision = revision + 1, updated_at = now() WHERE organization_id = $1",
        )
        .bind(organization_id)
        .bind(row.get::<i64, _>("byte_size"))
        .bind(row.get::<i64, _>("asset_count"))
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE quota_reservations SET released_at = now() WHERE id = $1")
            .bind(row.get::<Uuid, _>("id"))
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub(crate) async fn project_organization_tx(
    tx: &mut Transaction<'_, Postgres>,
    project_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(p.organization_id, owner_user.organization_id) FROM projects p LEFT JOIN LATERAL (SELECT u.organization_id FROM project_members pm JOIN users u ON u.id = pm.user_id WHERE pm.project_id = p.id AND pm.role = 'owner' AND pm.deleted_at IS NULL ORDER BY pm.joined_at, pm.user_id LIMIT 1) owner_user ON TRUE WHERE p.id = $1",
    )
    .bind(project_id)
    .fetch_optional(&mut **tx)
    .await?)
}

pub(crate) async fn project_organization(
    pool: &Pool<Postgres>,
    project_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    Ok(sqlx::query_scalar(
        "SELECT COALESCE(p.organization_id, owner_user.organization_id) FROM projects p LEFT JOIN LATERAL (SELECT u.organization_id FROM project_members pm JOIN users u ON u.id = pm.user_id WHERE pm.project_id = p.id AND pm.role = 'owner' AND pm.deleted_at IS NULL ORDER BY pm.joined_at, pm.user_id LIMIT 1) owner_user ON TRUE WHERE p.id = $1",
    )
    .bind(project_id)
    .fetch_optional(pool)
    .await?)
}

async fn is_organization_owner(pool: &Pool<Postgres>, user: &AuthUser) -> Result<bool, ApiError> {
    let Some(organization_id) = user.organization_id else {
        return Ok(false);
    };
    Ok(sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM organizations WHERE id = $1 AND owner_id = $2)",
    )
    .bind(organization_id)
    .bind(user.id)
    .fetch_one(pool)
    .await?)
}

fn quota_json(row: &PgRow, can_manage: bool) -> Value {
    json!({
        "organizationId": row.get::<Uuid, _>("organization_id"),
        "limitBytes": row.get::<i64, _>("limit_bytes"),
        "usedBytes": row.get::<i64, _>("used_bytes"),
        "reservedBytes": row.get::<i64, _>("reserved_bytes"),
        "maxAssets": row.get::<i64, _>("max_assets"),
        "usedAssets": row.get::<i64, _>("used_assets"),
        "reservedAssets": row.get::<i64, _>("reserved_assets"),
        "revision": row.get::<i64, _>("revision"),
        "canManage": can_manage,
        "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
    })
}

async fn quota_row(pool: &Pool<Postgres>, organization_id: Uuid) -> Result<PgRow, ApiError> {
    sqlx::query(
        "SELECT organization_id, limit_bytes, used_bytes, reserved_bytes, max_assets, used_assets, reserved_assets, revision, updated_at FROM organization_quotas WHERE organization_id = $1",
    )
    .bind(organization_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| ApiError::not_found("organization quota not found"))
}

pub async fn get_storage_quota(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    let organization_id = user
        .organization_id
        .ok_or_else(|| ApiError::forbidden("an organization is required"))?;
    let mut tx = state.pool.begin().await?;
    ensure_org_quota_tx(&mut tx, organization_id).await?;
    tx.commit().await?;
    let can_manage = is_organization_owner(&state.pool, &user).await?;
    Ok(Json(quota_json(
        &quota_row(&state.pool, organization_id).await?,
        can_manage,
    )))
}

pub async fn update_storage_quota(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<StorageQuotaUpdate>,
) -> Result<Json<Value>, ApiError> {
    let organization_id = user
        .organization_id
        .ok_or_else(|| ApiError::forbidden("an organization is required"))?;
    if !is_organization_owner(&state.pool, &user).await? {
        return Err(ApiError::forbidden(
            "only the organization owner can change storage quota",
        ));
    }
    if input.limit_bytes.is_some_and(|value| value < 0)
        || input.max_assets.is_some_and(|value| value < 0)
    {
        return Err(ApiError::bad_request("quota limits cannot be negative"));
    }
    let mut tx = state.pool.begin().await?;
    ensure_org_quota_tx(&mut tx, organization_id).await?;
    let current = sqlx::query(
        "SELECT used_bytes, reserved_bytes, used_assets, reserved_assets FROM organization_quotas WHERE organization_id = $1 FOR UPDATE",
    )
    .bind(organization_id)
    .fetch_one(&mut *tx)
    .await?;
    if input.limit_bytes.is_some_and(|value| {
        value < current.get::<i64, _>("used_bytes") + current.get::<i64, _>("reserved_bytes")
    }) || input.max_assets.is_some_and(|value| {
        value < current.get::<i64, _>("used_assets") + current.get::<i64, _>("reserved_assets")
    }) {
        return Err(ApiError::conflict(
            "STORAGE_QUOTA_BELOW_USAGE",
            "quota limit cannot be below current usage or reservations",
            Value::Null,
        ));
    }
    sqlx::query(
        "UPDATE organization_quotas SET limit_bytes = COALESCE($2, limit_bytes), max_assets = COALESCE($3, max_assets), revision = revision + 1, updated_at = now() WHERE organization_id = $1",
    )
    .bind(organization_id)
    .bind(input.limit_bytes)
    .bind(input.max_assets)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(quota_json(
        &quota_row(&state.pool, organization_id).await?,
        true,
    )))
}

pub async fn list(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    let rows = sqlx::query(
        r#"
        SELECT d.id, d.organization_id, d.owner_id, d.name, d.description,
            d.schema_json, d.status, d.revision, d.created_at, d.updated_at,
            dm.role,
            (SELECT COUNT(*) FROM dataset_items di WHERE di.dataset_id = d.id) AS item_count,
            (SELECT COALESCE(SUM(db.byte_size + COALESCE(db.thumbnail_byte_size, 0)), 0)::BIGINT
             FROM dataset_blobs db
             WHERE db.id IN (SELECT DISTINCT di2.blob_id FROM dataset_items di2 WHERE di2.dataset_id = d.id)) AS total_bytes,
            (SELECT dv.id FROM dataset_versions dv WHERE dv.dataset_id = d.id ORDER BY dv.version_number DESC LIMIT 1) AS latest_version_id
        FROM datasets d
        JOIN dataset_members dm ON dm.dataset_id = d.id AND dm.user_id = $1 AND dm.deleted_at IS NULL
        WHERE d.deleted_at IS NULL
        ORDER BY d.updated_at DESC, d.id
        "#,
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(dataset_summary_json).collect::<Vec<_>>()
    })))
}

pub async fn create(
    State(state): State<AppState>,
    user: AuthUser,
    Json(input): Json<CreateDataset>,
) -> Result<Json<Value>, ApiError> {
    validate_name(&input.name, "name")?;
    let organization_id = user
        .organization_id
        .ok_or_else(|| ApiError::forbidden("an organization is required"))?;
    let id = Uuid::new_v4();
    let mut tx = state.pool.begin().await?;
    ensure_org_quota_tx(&mut tx, organization_id).await?;
    sqlx::query(
        "INSERT INTO datasets (id, organization_id, owner_id, name, description) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(id)
    .bind(organization_id)
    .bind(user.id)
    .bind(input.name.trim())
    .bind(input.description.trim())
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO dataset_members (dataset_id, user_id, role) VALUES ($1, $2, 'owner')")
        .bind(id)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Json(dataset_summary_json(
        &dataset_row(&state.pool, id, user.id).await?,
    )))
}

pub async fn get(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    Ok(Json(dataset_summary_json(
        &dataset_row(&state.pool, dataset_id, user.id).await?,
    )))
}

pub async fn update(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<UpdateDataset>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::Manage).await?;
    if let Some(name) = input.name.as_deref() {
        validate_name(name, "name")?;
    }
    let mut tx = state.pool.begin().await?;
    let current = sqlx::query(
        "SELECT revision FROM datasets WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let current_revision: i64 = current.get("revision");
    if input
        .base_revision
        .is_some_and(|revision| revision != current_revision)
    {
        return Err(ApiError::conflict(
            "DATASET_REVISION_CONFLICT",
            "dataset revision does not match",
            json!({ "revision": current_revision }),
        ));
    }
    sqlx::query(
        "UPDATE datasets SET name = COALESCE($2, name), description = COALESCE($3, description), revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(dataset_id)
    .bind(input.name.as_deref())
    .bind(input.description.as_deref())
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(dataset_summary_json(
        &dataset_row(&state.pool, dataset_id, user.id).await?,
    )))
}

pub async fn delete(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let role =
        require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::Manage).await?;
    if role != "owner" {
        return Err(ApiError::forbidden(
            "only the dataset owner can archive the dataset",
        ));
    }
    sqlx::query(
        "UPDATE datasets SET status = 'archived', revision = revision + 1, updated_at = now() WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(dataset_id)
    .execute(&state.pool)
    .await?;
    Ok(Json(json!({ "id": dataset_id, "status": "archived" })))
}

pub async fn list_members(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let rows = sqlx::query(
        "SELECT dm.user_id, dm.role, dm.joined_at, u.email, u.display_name FROM dataset_members dm JOIN users u ON u.id = dm.user_id WHERE dm.dataset_id = $1 AND dm.deleted_at IS NULL ORDER BY dm.joined_at, dm.user_id",
    )
    .bind(dataset_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
            "userId": row.get::<Uuid, _>("user_id"),
            "role": row.get::<String, _>("role"),
            "email": row.get::<String, _>("email"),
            "displayName": row.get::<String, _>("display_name"),
            "joinedAt": row.get::<chrono::DateTime<Utc>, _>("joined_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn add_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<DatasetMemberInput>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(
        &state,
        user.id,
        dataset_id,
        DatasetPermission::ManageMembers,
    )
    .await?;
    if !matches!(input.role.as_str(), "manager" | "viewer") {
        return Err(ApiError::bad_request(
            "dataset role must be manager or viewer",
        ));
    }
    let normalized_email = normalize_dataset_member_email(input.user_id, input.email.as_deref())?;
    let mut tx = state.pool.begin().await?;
    let organization_id: Uuid = sqlx::query_scalar(
        "SELECT organization_id FROM datasets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let member_id: Option<Uuid> = match (input.user_id, normalized_email.as_deref()) {
        (Some(member_id), None) => sqlx::query_scalar(
            "SELECT id FROM users WHERE id = $1 AND organization_id = $2 AND status = 'active' AND deleted_at IS NULL",
        )
        .bind(member_id)
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?,
        (None, Some(email)) => sqlx::query_scalar(
            "SELECT id FROM users WHERE lower(email) = $1 AND organization_id = $2 AND status = 'active' AND deleted_at IS NULL",
        )
        .bind(email)
        .bind(organization_id)
        .fetch_optional(&mut *tx)
        .await?,
        _ => None,
    };
    let Some(member_id) = member_id else {
        return Err(ApiError::forbidden(
            "dataset member user is unknown, inactive, deleted, or outside the organization",
        ));
    };
    sqlx::query(
        "INSERT INTO dataset_members (dataset_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT(dataset_id, user_id) DO UPDATE SET role = excluded.role, deleted_at = NULL",
    )
    .bind(dataset_id)
    .bind(member_id)
    .bind(&input.role)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE datasets SET revision = revision + 1, updated_at = now() WHERE id = $1")
        .bind(dataset_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let mut result = json!({ "userId": member_id, "role": input.role });
    if let Some(email) = normalized_email {
        result["email"] = json!(email);
    }
    Ok(Json(result))
}

pub async fn remove_member(
    State(state): State<AppState>,
    user: AuthUser,
    Path((dataset_id, member_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(
        &state,
        user.id,
        dataset_id,
        DatasetPermission::ManageMembers,
    )
    .await?;
    let target_role: Option<String> = sqlx::query_scalar(
        "SELECT role FROM dataset_members WHERE dataset_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(dataset_id)
    .bind(member_id)
    .fetch_optional(&state.pool)
    .await?;
    let Some(target_role) = target_role else {
        return Err(ApiError::not_found("dataset member not found"));
    };
    if target_role == "owner" {
        return Err(ApiError::bad_request("dataset owner cannot be removed"));
    }
    sqlx::query(
        "UPDATE dataset_members SET deleted_at = now() WHERE dataset_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(dataset_id)
    .bind(member_id)
    .execute(&state.pool)
    .await?;
    sqlx::query("UPDATE datasets SET revision = revision + 1, updated_at = now() WHERE id = $1")
        .bind(dataset_id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({ "userId": member_id, "deleted": true })))
}

pub async fn list_items(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let rows = sqlx::query(
        r#"
        SELECT di.id, di.client_key, di.source_client_key, di.file_name, di.split, di.annotation_json,
            di.annotation_hash, di.source_project_id, di.source_asset_id,
            di.revision, di.created_at, di.updated_at,
            db.id AS blob_id, db.object_key, db.content_hash, db.mime_type,
            db.width, db.height, db.byte_size, db.thumbnail_key,
            db.thumbnail_content_hash, db.thumbnail_mime_type, db.thumbnail_byte_size
        FROM dataset_items di
        JOIN dataset_blobs db ON db.id = di.blob_id
        WHERE di.dataset_id = $1
        ORDER BY di.created_at, di.id
        "#,
    )
    .bind(dataset_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "clientKey": row.get::<Option<String>, _>("client_key"),
            "sourceClientKey": row.get::<Option<String>, _>("source_client_key"),
            "fileName": row.get::<String, _>("file_name"),
            "split": row.get::<String, _>("split"),
            "annotation": row.get::<Value, _>("annotation_json"),
            "annotationHash": row.get::<String, _>("annotation_hash"),
            "sourceProjectId": row.get::<Option<Uuid>, _>("source_project_id"),
            "sourceAssetId": row.get::<Option<Uuid>, _>("source_asset_id"),
            "revision": row.get::<i64, _>("revision"),
            "blob": {
                "id": row.get::<Uuid, _>("blob_id"),
                "objectKey": row.get::<String, _>("object_key"),
                "contentHash": row.get::<String, _>("content_hash"),
                "mimeType": row.get::<String, _>("mime_type"),
                "width": row.get::<i32, _>("width"),
                "height": row.get::<i32, _>("height"),
                "byteSize": row.get::<i64, _>("byte_size"),
                "thumbnailKey": row.get::<Option<String>, _>("thumbnail_key"),
                "thumbnailContentHash": row.get::<Option<String>, _>("thumbnail_content_hash"),
                "thumbnailMimeType": row.get::<Option<String>, _>("thumbnail_mime_type"),
                "thumbnailByteSize": row.get::<Option<i64>, _>("thumbnail_byte_size")
            },
            "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at"),
            "updatedAt": row.get::<chrono::DateTime<Utc>, _>("updated_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn capture(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<CaptureRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_operation_id(&input.operation_id)?;
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::Manage).await?;
    require_project_permission(&state, user.id, input.project_id, ProjectPermission::View)
        .await
        .map_err(ApiError::from)?;
    let request_hash = operation_hash("capture", json!({ "projectId": input.project_id }))?;

    let mut tx = state.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let dataset = sqlx::query(
        "SELECT organization_id, revision FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted' FOR UPDATE",
    )
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    if let Some(existing) = sqlx::query(
        "SELECT request_hash, result_json FROM dataset_operations WHERE dataset_id = $1 AND operation_id = $2",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.get::<String, _>("request_hash") != request_hash {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSE",
                "operationId was already used with a different request",
                Value::Null,
            ));
        }
        return Ok(Json(existing.get("result_json")));
    }
    let dataset_org: Uuid = dataset.get("organization_id");
    let _source_project = sqlx::query(
        "SELECT id, organization_id FROM projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(input.project_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("source project not found"))?;
    let project_org = project_organization_tx(&mut tx, input.project_id)
        .await?
        .ok_or_else(|| ApiError::forbidden("source project has no organization"))?;
    if project_org != dataset_org {
        return Err(ApiError::forbidden(
            "source project and dataset must belong to the same organization",
        ));
    }
    let schema = sqlx::query_scalar::<_, Value>(
        "SELECT schema_json FROM label_schemas WHERE project_id = $1 AND deleted_at IS NULL ORDER BY updated_at DESC, id DESC LIMIT 1",
    )
    .bind(input.project_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_else(|| json!({ "classes": [] }));
    let assets = sqlx::query(
        r#"
        SELECT a.id, a.client_key, a.file_name, a.object_key, a.content_hash,
            a.mime_type, a.width, a.height, a.byte_size, a.split,
            a.thumbnail_key, a.thumbnail_content_hash, a.thumbnail_mime_type,
            a.thumbnail_byte_size, a.revision,
            COALESCE(an.object_json, '[]'::jsonb) AS annotation_json
        FROM assets a
        LEFT JOIN annotations an ON an.image_id = a.id AND an.deleted_at IS NULL
        WHERE a.project_id = $1
          AND a.status NOT IN ('uploading', 'failed')
          AND a.deleted_at IS NULL
        ORDER BY a.id
        "#,
    )
    .bind(input.project_id)
    .fetch_all(&mut *tx)
    .await?;
    let existing_source_keys: HashMap<Uuid, String> = sqlx::query(
        "SELECT source_asset_id, client_key FROM dataset_items WHERE dataset_id = $1 AND source_project_id = $2 AND source_asset_id IS NOT NULL AND client_key IS NOT NULL",
    )
    .bind(dataset_id)
    .bind(input.project_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(|row| (row.get("source_asset_id"), row.get("client_key")))
    .collect();
    let new_item_count = assets
        .iter()
        .filter(|row| !existing_source_keys.contains_key(&row.get::<Uuid, _>("id")))
        .count() as i64;
    let reservation_id = Uuid::new_v4();
    let reservation_active = reserve_quota_tx(
        &mut tx,
        dataset_org,
        "dataset_capture",
        reservation_id,
        0,
        new_item_count,
    )
    .await?;

    sqlx::query(
        "UPDATE datasets SET schema_json = $2, revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(dataset_id)
    .bind(&schema)
    .execute(&mut *tx)
    .await?;
    for row in &assets {
        let source_asset_id: Uuid = row.get("id");
        let source_client_key = row
            .get::<Option<String>, _>("client_key")
            .unwrap_or_else(|| source_asset_id.to_string());
        let desired_client_key = format!("{}:{}", input.project_id, source_asset_id);
        let client_key = existing_source_keys
            .get(&source_asset_id)
            .cloned()
            .unwrap_or(desired_client_key);
        let blob_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO dataset_blobs (
                organization_id, object_key, file_name, content_hash, mime_type,
                width, height, byte_size, thumbnail_key, thumbnail_content_hash,
                thumbnail_mime_type, thumbnail_byte_size
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
            ON CONFLICT (organization_id, content_hash, byte_size, mime_type)
            DO UPDATE SET object_key = dataset_blobs.object_key
            RETURNING id
            "#,
        )
        .bind(dataset_org)
        .bind(row.get::<String, _>("object_key"))
        .bind(row.get::<String, _>("file_name"))
        .bind(row.get::<String, _>("content_hash"))
        .bind(row.get::<String, _>("mime_type"))
        .bind(row.get::<i32, _>("width"))
        .bind(row.get::<i32, _>("height"))
        .bind(row.get::<i64, _>("byte_size"))
        .bind(row.get::<Option<String>, _>("thumbnail_key"))
        .bind(row.get::<Option<String>, _>("thumbnail_content_hash"))
        .bind(row.get::<Option<String>, _>("thumbnail_mime_type"))
        .bind(row.get::<Option<i64>, _>("thumbnail_byte_size"))
        .fetch_one(&mut *tx)
        .await?;
        let annotation = row.get::<Value, _>("annotation_json");
        let annotation_hash = json_hash(&annotation)?;
        sqlx::query(
            r#"
            INSERT INTO dataset_items (
                dataset_id, blob_id, client_key, file_name, split,
                annotation_json, annotation_hash, source_project_id, source_asset_id,
                source_client_key
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
            ON CONFLICT (dataset_id, client_key) WHERE client_key IS NOT NULL
            DO UPDATE SET blob_id = excluded.blob_id, file_name = excluded.file_name,
                split = excluded.split, annotation_json = excluded.annotation_json,
                annotation_hash = excluded.annotation_hash,
                source_project_id = excluded.source_project_id,
                source_asset_id = excluded.source_asset_id,
                source_client_key = excluded.source_client_key,
                revision = dataset_items.revision + 1, updated_at = now()
            "#,
        )
        .bind(dataset_id)
        .bind(blob_id)
        .bind(&client_key)
        .bind(row.get::<String, _>("file_name"))
        .bind(row.get::<String, _>("split"))
        .bind(annotation)
        .bind(annotation_hash)
        .bind(input.project_id)
        .bind(source_asset_id)
        .bind(&source_client_key)
        .execute(&mut *tx)
        .await?;
    }
    if reservation_active {
        consume_quota_tx(
            &mut tx,
            dataset_org,
            "dataset_capture",
            reservation_id,
            0,
            new_item_count,
        )
        .await?;
    }
    let revision: i64 = sqlx::query_scalar("SELECT revision FROM datasets WHERE id = $1")
        .bind(dataset_id)
        .fetch_one(&mut *tx)
        .await?;
    let result = json!({
        "datasetId": dataset_id,
        "projectId": input.project_id,
        "operationId": input.operation_id,
        "capturedItems": assets.len(),
        "newItems": new_item_count,
        "revision": revision,
        "schema": schema
    });
    sqlx::query(
        "INSERT INTO dataset_operations (dataset_id, operation_id, operation, request_hash, result_json, created_by) VALUES ($1, $2, 'capture', $3, $4, $5)",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .bind(&request_hash)
    .bind(&result)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

pub async fn list_versions(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let rows = sqlx::query(
        "SELECT id, dataset_id, version_number, name, manifest_hash, manifest_byte_size, item_count, created_by, created_at FROM dataset_versions WHERE dataset_id = $1 ORDER BY version_number DESC",
    )
    .bind(dataset_id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| json!({
            "id": row.get::<Uuid, _>("id"),
            "datasetId": row.get::<Uuid, _>("dataset_id"),
            "version": row.get::<i64, _>("version_number"),
            "name": row.get::<String, _>("name"),
            "manifestHash": row.get::<String, _>("manifest_hash"),
            "manifestByteSize": row.get::<i64, _>("manifest_byte_size"),
            "itemCount": row.get::<i64, _>("item_count"),
            "createdBy": row.get::<Uuid, _>("created_by"),
            "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn get_version(
    State(state): State<AppState>,
    user: AuthUser,
    Path((dataset_id, version_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Value>, ApiError> {
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let row = sqlx::query(
        "SELECT id, dataset_id, version_number, name, manifest_json, manifest_hash, manifest_byte_size, item_count, created_by, created_at FROM dataset_versions WHERE id = $1 AND dataset_id = $2",
    )
    .bind(version_id)
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset version not found"))?;
    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "datasetId": row.get::<Uuid, _>("dataset_id"),
        "version": row.get::<i64, _>("version_number"),
        "name": row.get::<String, _>("name"),
        "manifest": row.get::<Value, _>("manifest_json"),
        "manifestHash": row.get::<String, _>("manifest_hash"),
        "manifestByteSize": row.get::<i64, _>("manifest_byte_size"),
        "itemCount": row.get::<i64, _>("item_count"),
        "createdBy": row.get::<Uuid, _>("created_by"),
        "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at")
    })))
}

pub async fn create_version(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<CreateVersionRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_name(&input.name, "name")?;
    validate_operation_id(&input.operation_id)?;
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::Manage).await?;
    let request_hash = operation_hash(
        "version",
        json!({ "name": input.name.trim(), "baseRevision": input.base_revision }),
    )?;
    let mut tx = state.pool.begin().await?;
    let dataset = sqlx::query(
        "SELECT organization_id, schema_json, revision FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted' FOR UPDATE",
    )
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    if let Some(existing) = sqlx::query(
        "SELECT request_hash, result_json FROM dataset_operations WHERE dataset_id = $1 AND operation_id = $2",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.get::<String, _>("request_hash") != request_hash {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSE",
                "operationId was already used with a different request",
                Value::Null,
            ));
        }
        return Ok(Json(existing.get("result_json")));
    }
    let current_revision: i64 = dataset.get("revision");
    if input
        .base_revision
        .is_some_and(|revision| revision != current_revision)
    {
        return Err(ApiError::conflict(
            "DATASET_REVISION_CONFLICT",
            "dataset revision does not match",
            json!({ "revision": current_revision }),
        ));
    }
    let rows = sqlx::query(
        r#"
        SELECT di.id, di.client_key, di.source_client_key, di.file_name, di.split, di.annotation_json,
            di.annotation_hash, di.source_project_id, di.source_asset_id,
            db.id AS blob_id, db.object_key, db.content_hash, db.mime_type,
            db.width, db.height, db.byte_size, db.thumbnail_key,
            db.thumbnail_content_hash, db.thumbnail_mime_type, db.thumbnail_byte_size
        FROM dataset_items di
        JOIN dataset_blobs db ON db.id = di.blob_id
        WHERE di.dataset_id = $1
        ORDER BY di.created_at, di.id
        "#,
    )
    .bind(dataset_id)
    .fetch_all(&mut *tx)
    .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.get::<Uuid, _>("id"),
                "clientKey": row.get::<Option<String>, _>("client_key"),
                "sourceClientKey": row.get::<Option<String>, _>("source_client_key"),
                "fileName": row.get::<String, _>("file_name"),
                "split": row.get::<String, _>("split"),
                "annotation": row.get::<Value, _>("annotation_json"),
                "annotationHash": row.get::<String, _>("annotation_hash"),
                "sourceProjectId": row.get::<Option<Uuid>, _>("source_project_id"),
                "sourceAssetId": row.get::<Option<Uuid>, _>("source_asset_id"),
                "blob": {
                    "id": row.get::<Uuid, _>("blob_id"),
                    "objectKey": row.get::<String, _>("object_key"),
                    "fileName": row.get::<String, _>("file_name"),
                    "contentHash": row.get::<String, _>("content_hash"),
                    "mimeType": row.get::<String, _>("mime_type"),
                    "width": row.get::<i32, _>("width"),
                    "height": row.get::<i32, _>("height"),
                    "byteSize": row.get::<i64, _>("byte_size"),
                    "thumbnailKey": row.get::<Option<String>, _>("thumbnail_key"),
                    "thumbnailContentHash": row.get::<Option<String>, _>("thumbnail_content_hash"),
                    "thumbnailMimeType": row.get::<Option<String>, _>("thumbnail_mime_type"),
                    "thumbnailByteSize": row.get::<Option<i64>, _>("thumbnail_byte_size")
                }
            })
        })
        .collect();
    let manifest = json!({
        "datasetId": dataset_id,
        "schema": dataset.get::<Value, _>("schema_json"),
        "items": items
    });
    let manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|error| ApiError::internal_message(format!("encode dataset manifest: {error}")))?;
    let manifest_hash = hex::encode(Sha256::digest(&manifest_bytes));
    let organization_id: Uuid = dataset.get("organization_id");
    let version_id = Uuid::new_v4();
    let reservation_active = reserve_quota_tx(
        &mut tx,
        organization_id,
        "dataset_version",
        version_id,
        manifest_bytes.len() as i64,
        0,
    )
    .await?;
    let version_number: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version_number), 0) + 1 FROM dataset_versions WHERE dataset_id = $1",
    )
    .bind(dataset_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO dataset_versions (id, dataset_id, version_number, name, manifest_json, manifest_hash, manifest_byte_size, item_count, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(version_id)
    .bind(dataset_id)
    .bind(version_number)
    .bind(input.name.trim())
    .bind(&manifest)
    .bind(&manifest_hash)
    .bind(manifest_bytes.len() as i64)
    .bind(items.len() as i64)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    if reservation_active {
        consume_quota_tx(
            &mut tx,
            organization_id,
            "dataset_version",
            version_id,
            manifest_bytes.len() as i64,
            0,
        )
        .await?;
    }
    let revision: i64 = sqlx::query_scalar(
        "UPDATE datasets SET revision = revision + 1, updated_at = now() WHERE id = $1 RETURNING revision",
    )
    .bind(dataset_id)
    .fetch_one(&mut *tx)
    .await?;
    let result = json!({
        "datasetId": dataset_id,
        "versionId": version_id,
        "version": version_number,
        "name": input.name.trim(),
        "manifestHash": manifest_hash,
        "manifestByteSize": manifest_bytes.len(),
        "itemCount": items.len(),
        "revision": revision,
        "operationId": input.operation_id
    });
    sqlx::query(
        "INSERT INTO dataset_operations (dataset_id, operation_id, operation, request_hash, result_json, created_by) VALUES ($1, $2, 'version', $3, $4, $5)",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .bind(&request_hash)
    .bind(&result)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

pub async fn reuse(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<ReuseRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_operation_id(&input.operation_id)?;
    if input.target_project_id.is_none() {
        if let Some(name) = input.project_name.as_deref() {
            validate_name(name, "projectName")?;
        }
    }
    let request_hash = operation_hash(
        "reuse",
        json!({
            "versionId": input.version_id,
            "targetProjectId": input.target_project_id,
            "projectName": input.project_name.as_deref()
        }),
    )?;
    require_dataset_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    if let Some(target_project_id) = input.target_project_id {
        require_project_permission(
            &state,
            user.id,
            target_project_id,
            ProjectPermission::ManageData,
        )
        .await
        .map_err(ApiError::from)?;
    }

    let mut tx = state.pool.begin().await?;
    let dataset = sqlx::query(
        "SELECT organization_id, name FROM datasets WHERE id = $1 AND deleted_at IS NULL AND status <> 'deleted' FOR UPDATE",
    )
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    if let Some(existing) = sqlx::query(
        "SELECT request_hash, result_json FROM dataset_operations WHERE dataset_id = $1 AND operation_id = $2",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if existing.get::<String, _>("request_hash") != request_hash {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSE",
                "operationId was already used with a different request",
                Value::Null,
            ));
        }
        return Ok(Json(existing.get("result_json")));
    }
    let organization_id: Uuid = dataset.get("organization_id");
    let version = sqlx::query(
        "SELECT id, name, manifest_json FROM dataset_versions WHERE id = $1 AND dataset_id = $2",
    )
    .bind(input.version_id)
    .bind(dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset version not found"))?;
    let version_name: String = version.get("name");
    let manifest: Value = version.get("manifest_json");
    let items = manifest
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::internal_message("dataset version manifest is invalid"))?;

    let project_id = if let Some(target_project_id) = input.target_project_id {
        let row = sqlx::query(
            "SELECT organization_id, name FROM projects WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(target_project_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("target project not found"))?;
        let target_org: Option<Uuid> = row.get("organization_id");
        if target_org != Some(organization_id) {
            return Err(ApiError::forbidden(
                "target project and dataset must belong to the same organization",
            ));
        }
        target_project_id
    } else {
        let id = Uuid::new_v4();
        let default_name: String = dataset.get("name");
        let name = input
            .project_name
            .as_deref()
            .unwrap_or(default_name.as_str())
            .trim()
            .to_string();
        validate_name(&name, "projectName")?;
        sqlx::query(
            "INSERT INTO projects (id, organization_id, name, description, mode, status, created_by, updated_by) VALUES ($1, $2, $3, $4, 'cloud_linked', 'active', $5, $5)",
        )
        .bind(id)
        .bind(organization_id)
        .bind(&name)
        .bind(format!("Materialized from dataset {}", dataset_id))
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
        write_change(
            &mut tx,
            id,
            "project",
            &id.to_string(),
            "create",
            1,
            json!({
                "id": id,
                "name": name,
                "description": format!("Materialized from dataset {}", dataset_id),
                "mode": "cloud_linked",
                "status": "active"
            }),
        )
        .await?;
        write_audit(
            &mut tx,
            id,
            user.id,
            "project.create_from_dataset",
            "project",
            &id.to_string(),
        )
        .await?;
        id
    };

    // Carry label classes into the materialized project.  A dataset version
    // is immutable, so this creates a project-local schema instead of
    // overwriting a schema that may already be in use by another workflow.
    let schema = manifest
        .get("schema")
        .cloned()
        .unwrap_or_else(|| json!({ "classes": [] }));
    let classes = schema
        .get("classes")
        .cloned()
        .or_else(|| schema.as_array().map(|value| Value::Array(value.clone())))
        .unwrap_or_else(|| json!([]));
    let schema_name = format!("{} (dataset {})", version_name, dataset_id);
    let schema_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO label_schemas (id, project_id, name, schema_json, revision, created_by, updated_by) VALUES ($1, $2, $3, $4, 1, $5, $5)",
    )
    .bind(schema_id)
    .bind(project_id)
    .bind(&schema_name)
    .bind(&schema)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    write_change(
        &mut tx,
        project_id,
        "label_schema",
        &schema_id.to_string(),
        "create",
        1,
        json!({
            "id": schema_id,
            "projectId": project_id,
            "name": schema_name,
            "schema": schema.clone(),
            "schemaJson": schema,
            "classes": classes,
            "revision": 1
        }),
    )
    .await?;
    write_audit(
        &mut tx,
        project_id,
        user.id,
        "label_schema.create_from_dataset",
        "label_schema",
        &schema_id.to_string(),
    )
    .await?;

    let mut new_assets = Vec::new();
    for item in items {
        let client_key = item
            .get("clientKey")
            .and_then(Value::as_str)
            .or_else(|| item.get("id").and_then(Value::as_str))
            .ok_or_else(|| ApiError::internal_message("dataset item has no client key"))?;
        let blob = item
            .get("blob")
            .ok_or_else(|| ApiError::internal_message("dataset item has no blob"))?;
        let content_hash = blob
            .get("contentHash")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal_message("dataset blob has no content hash"))?;
        let object_key = blob
            .get("objectKey")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal_message("dataset blob has no object key"))?;
        let existing = sqlx::query(
            "SELECT id, content_hash, object_key FROM assets WHERE project_id = $1 AND client_key = $2 AND deleted_at IS NULL FOR UPDATE",
        )
        .bind(project_id)
        .bind(client_key)
        .fetch_optional(&mut *tx)
        .await?;
        if let Some(existing) = existing {
            if existing.get::<String, _>("content_hash") != content_hash
                || existing.get::<String, _>("object_key") != object_key
            {
                return Err(ApiError::conflict(
                    "PROJECT_ASSET_CLIENT_KEY_CONFLICT",
                    "target project already has a different asset for this clientKey",
                    json!({ "clientKey": client_key }),
                ));
            }
            continue;
        }
        new_assets.push((client_key.to_string(), item, blob));
    }
    let reservation_id = Uuid::new_v4();
    let reservation_active = reserve_quota_tx(
        &mut tx,
        organization_id,
        "dataset_reuse",
        reservation_id,
        0,
        new_assets.len() as i64,
    )
    .await?;
    let mut reused = Vec::new();
    for (client_key, item, blob) in new_assets {
        let asset_id = Uuid::new_v4();
        let content_hash = blob
            .get("contentHash")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal_message("dataset blob has no content hash"))?;
        let object_key = blob
            .get("objectKey")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::internal_message("dataset blob has no object key"))?;
        let blob_id = blob
            .get("id")
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value).ok());
        let annotation = item.get("annotation").cloned().unwrap_or_else(|| json!([]));
        let revision: i64 = sqlx::query_scalar(
            r#"
            INSERT INTO assets (
                id, project_id, client_key, file_name, object_key, content_hash,
                mime_type, width, height, byte_size, split, status, revision,
                created_by, updated_by, thumbnail_key, thumbnail_content_hash,
                thumbnail_mime_type, thumbnail_byte_size, dataset_id,
                dataset_version_id, dataset_blob_id
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'ready', 1,
                $12, $12, $13, $14, $15, $16, $17, $18, $19)
            RETURNING revision
            "#,
        )
        .bind(asset_id)
        .bind(project_id)
        .bind(&client_key)
        .bind(
            item.get("fileName")
                .and_then(Value::as_str)
                .unwrap_or("dataset-item"),
        )
        .bind(object_key)
        .bind(content_hash)
        .bind(
            blob.get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream"),
        )
        .bind(blob.get("width").and_then(Value::as_i64).unwrap_or(1) as i32)
        .bind(blob.get("height").and_then(Value::as_i64).unwrap_or(1) as i32)
        .bind(blob.get("byteSize").and_then(Value::as_i64).unwrap_or(0))
        .bind(item.get("split").and_then(Value::as_str).unwrap_or("train"))
        .bind(user.id)
        .bind(blob.get("thumbnailKey").and_then(Value::as_str))
        .bind(blob.get("thumbnailContentHash").and_then(Value::as_str))
        .bind(blob.get("thumbnailMimeType").and_then(Value::as_str))
        .bind(blob.get("thumbnailByteSize").and_then(Value::as_i64))
        .bind(dataset_id)
        .bind(input.version_id)
        .bind(blob_id)
        .fetch_one(&mut *tx)
        .await?;
        write_change(
            &mut tx,
            project_id,
            "asset",
            &client_key,
            "create",
            revision,
            json!({
                "id": asset_id,
                "clientKey": client_key,
                "fileName": item.get("fileName"),
                "objectKey": object_key,
                "contentHash": content_hash,
                "mimeType": blob.get("mimeType"),
                "width": blob.get("width"),
                "height": blob.get("height"),
                "byteSize": blob.get("byteSize"),
                "split": item.get("split"),
                "status": "ready",
                "revision": revision,
                "thumbnailKey": blob.get("thumbnailKey"),
                "thumbnailContentHash": blob.get("thumbnailContentHash"),
                "thumbnailMimeType": blob.get("thumbnailMimeType"),
                "thumbnailByteSize": blob.get("thumbnailByteSize")
            }),
        )
        .await?;
        let annotation_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO annotations (id, project_id, image_id, revision, schema_version_id, object_json, status, updated_by) VALUES ($1, $2, $3, 1, $4, $5, 'draft', $6)",
        )
        .bind(annotation_id)
        .bind(project_id)
        .bind(asset_id)
        .bind(schema_id)
        .bind(&annotation)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
        let annotation_operation = format!("dataset:{}:{}", input.operation_id, asset_id);
        sqlx::query(
            "INSERT INTO annotation_versions (annotation_id, revision, object_json, operation_id, created_by) VALUES ($1, 1, $2, $3, $4)",
        )
        .bind(annotation_id)
        .bind(&annotation)
        .bind(annotation_operation)
        .bind(user.id)
        .execute(&mut *tx)
        .await?;
        write_change(
            &mut tx,
            project_id,
            "annotation",
            &client_key,
            "update",
            1,
            json!({
                "id": annotation_id,
                "imageId": asset_id,
                "clientImageId": client_key,
                "revision": 1,
                "objects": annotation,
                "status": "draft"
            }),
        )
        .await?;
        reused.push(json!({ "assetId": asset_id, "clientKey": client_key }));
    }
    if reservation_active {
        consume_quota_tx(
            &mut tx,
            organization_id,
            "dataset_reuse",
            reservation_id,
            0,
            reused.len() as i64,
        )
        .await?;
    }
    let result = json!({
        "datasetId": dataset_id,
        "versionId": input.version_id,
        "projectId": project_id,
        "schemaId": schema_id,
        "reusedAssets": reused,
        "operationId": input.operation_id
    });
    sqlx::query(
        "INSERT INTO dataset_operations (dataset_id, operation_id, operation, request_hash, result_json, created_by) VALUES ($1, $2, 'reuse', $3, $4, $5)",
    )
    .bind(dataset_id)
    .bind(&input.operation_id)
    .bind(&request_hash)
    .bind(&result)
    .bind(user.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::normalize_dataset_member_email;
    use uuid::Uuid;

    #[test]
    fn dataset_member_selector_accepts_uuid_or_normalized_email() {
        let user_id = Uuid::new_v4();
        assert_eq!(
            normalize_dataset_member_email(Some(user_id), None).unwrap(),
            None
        );
        assert_eq!(
            normalize_dataset_member_email(None, Some("  Member@Example.COM "))
                .unwrap()
                .as_deref(),
            Some("member@example.com")
        );
    }

    #[test]
    fn dataset_member_selector_rejects_missing_or_ambiguous_identity() {
        let user_id = Uuid::new_v4();
        assert!(normalize_dataset_member_email(None, None).is_err());
        assert!(normalize_dataset_member_email(Some(user_id), Some("member@example.com")).is_err());
        assert!(normalize_dataset_member_email(None, Some("not-an-email")).is_err());
    }
}

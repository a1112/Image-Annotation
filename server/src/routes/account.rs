use super::ApiError;
use crate::{
    auth::{
        hash_password, hash_token, issue_access_token, new_refresh_token, verify_password,
        AuthUser, ProjectPermission,
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
use sqlx::{Pool, Postgres, Row};
use uuid::Uuid;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LoginRequest {
    pub email: String,
    pub password: String,
    pub device_id: String,
    pub platform: String,
    #[serde(default)]
    pub device_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefreshRequest {
    pub refresh_token: String,
    pub device_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InvitationRequest {
    pub email: String,
    pub role: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptInvitationRequest {
    pub display_name: String,
    pub password: String,
    pub device_id: String,
    pub platform: String,
    #[serde(default)]
    pub device_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BootstrapAdminRequest {
    pub email: String,
    pub password: String,
    pub display_name: String,
    pub organization_name: String,
}

pub async fn login(
    State(state): State<AppState>,
    Json(input): Json<LoginRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_device(&input.device_id, &input.platform)?;
    let email = normalize_email(&input.email)?;
    let row = sqlx::query(
        "SELECT id, organization_id, password_hash, status FROM users WHERE lower(email) = $1",
    )
    .bind(&email)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::unauthorized("invalid email or password"))?;
    let password_hash: Option<String> = row.get("password_hash");
    if row.get::<String, _>("status") != "active"
        || password_hash
            .as_deref()
            .is_none_or(|encoded| !verify_password(&input.password, encoded))
    {
        return Err(ApiError::unauthorized("invalid email or password"));
    }
    let user_id: Uuid = row.get("id");
    let organization_id: Option<Uuid> = row.get("organization_id");
    let session = create_session(
        &state,
        user_id,
        &input.device_id,
        &input.platform,
        &input.device_name,
    )
    .await?;
    Ok(Json(
        session_response(&state, user_id, organization_id, &input.device_id, session).await?,
    ))
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(input): Json<RefreshRequest>,
) -> Result<Json<Value>, ApiError> {
    let hash = hash_token(&input.refresh_token);
    let mut tx = state.pool.begin().await?;
    let row = sqlx::query(
        "SELECT s.id, s.user_id, d.device_key, d.revoked_at AS device_revoked, u.organization_id, u.email, u.display_name, u.status, s.expires_at, s.revoked_at FROM sessions s JOIN devices d ON d.id = s.device_id JOIN users u ON u.id = s.user_id WHERE s.refresh_token_hash = $1 FOR UPDATE",
    )
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::unauthorized("refresh token is invalid"))?;
    let session_id: Uuid = row.get("id");
    let user_id: Uuid = row.get("user_id");
    let device_key: String = row.get("device_key");
    let expired = row.get::<chrono::DateTime<Utc>, _>("expires_at") <= Utc::now();
    if input
        .device_id
        .as_deref()
        .is_some_and(|value| value != device_key)
        || row.get::<String, _>("status") != "active"
        || row
            .get::<Option<chrono::DateTime<Utc>>, _>("device_revoked")
            .is_some()
        || row
            .get::<Option<chrono::DateTime<Utc>>, _>("revoked_at")
            .is_some()
        || expired
    {
        return Err(ApiError::unauthorized(
            "refresh session is revoked or expired",
        ));
    }
    let replacement = new_refresh_token();
    let expires_at = Utc::now() + Duration::seconds(state.config.refresh_ttl_seconds);
    sqlx::query("UPDATE sessions SET refresh_token_hash = $2, expires_at = $3, last_seen_at = now() WHERE id = $1")
        .bind(session_id)
        .bind(hash_token(&replacement))
        .bind(expires_at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let organization_id: Option<Uuid> = row.get("organization_id");
    let (access_token, access_expires) = issue_access_token(
        &state.config,
        user_id,
        organization_id,
        session_id,
        &device_key,
    )
    .map_err(ApiError::internal_message)?;
    Ok(Json(json!({
        "accessToken": access_token,
        "tokenType": "Bearer",
        "expiresAt": access_expires,
        "expiresAtUnix": access_expires.timestamp(),
        "expiresIn": state.config.access_ttl_seconds,
        "refreshToken": replacement,
        "refreshExpiresAt": expires_at,
        "sessionId": session_id,
        "deviceId": device_key,
        "accountId": user_id,
        "user": {
            "id": user_id,
            "email": row.get::<String, _>("email"),
            "displayName": row.get::<String, _>("display_name"),
            "organizationId": organization_id
        }
    })))
}

pub async fn logout(
    State(state): State<AppState>,
    user: AuthUser,
) -> Result<Json<Value>, ApiError> {
    if let Some(session_id) = user.session_id {
        sqlx::query("UPDATE sessions SET revoked_at = now() WHERE id = $1 AND user_id = $2")
            .bind(session_id)
            .bind(user.id)
            .execute(&state.pool)
            .await?;
    }
    Ok(Json(json!({ "revoked": true })))
}

pub async fn me(State(state): State<AppState>, user: AuthUser) -> Result<Json<Value>, ApiError> {
    let row = sqlx::query(
        "SELECT id, email, display_name, organization_id, status, created_at FROM users WHERE id = $1",
    )
    .bind(user.id)
    .fetch_one(&state.pool)
    .await?;
    let devices = sqlx::query(
        "SELECT id, device_key, platform, name, created_at, last_seen_at, revoked_at FROM devices WHERE user_id = $1 ORDER BY last_seen_at DESC",
    )
    .bind(user.id)
    .fetch_all(&state.pool)
    .await?;
    Ok(Json(json!({
        "id": row.get::<Uuid, _>("id"),
        "email": row.get::<String, _>("email"),
        "displayName": row.get::<String, _>("display_name"),
        "organizationId": row.get::<Option<Uuid>, _>("organization_id"),
        "status": row.get::<String, _>("status"),
        "createdAt": row.get::<chrono::DateTime<Utc>, _>("created_at"),
        "devices": devices.iter().map(|device| json!({
            "id": device.get::<Uuid, _>("id"),
            "deviceId": device.get::<String, _>("device_key"),
            "platform": device.get::<String, _>("platform"),
            "name": device.get::<String, _>("name"),
            "createdAt": device.get::<chrono::DateTime<Utc>, _>("created_at"),
            "lastSeenAt": device.get::<chrono::DateTime<Utc>, _>("last_seen_at"),
            "revokedAt": device.get::<Option<chrono::DateTime<Utc>>, _>("revoked_at")
        })).collect::<Vec<_>>()
    })))
}

pub async fn revoke_device(
    State(state): State<AppState>,
    user: AuthUser,
    Path(device_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    sqlx::query("UPDATE devices SET revoked_at = now() WHERE id = $1 AND user_id = $2")
        .bind(device_id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    sqlx::query("UPDATE sessions SET revoked_at = now() WHERE device_id = $1 AND user_id = $2")
        .bind(device_id)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({ "revoked": true })))
}

pub async fn invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(project_id): Path<Uuid>,
    Json(input): Json<InvitationRequest>,
) -> Result<Json<Value>, ApiError> {
    crate::auth::require_project_permission(
        &state,
        user.id,
        project_id,
        ProjectPermission::ManageMembers,
    )
    .await?;
    if !matches!(
        input.role.as_str(),
        "manager" | "annotator" | "reviewer" | "viewer"
    ) {
        return Err(ApiError::bad_request("invalid invitation role"));
    }
    let email = normalize_email(&input.email)?;
    let raw_token = new_refresh_token();
    let expires_at = Utc::now() + Duration::days(7);
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO project_invitations (id, project_id, email, role, token_hash, expires_at, invited_by) VALUES ($1, $2, $3, $4, $5, $6, $7)")
        .bind(id)
        .bind(project_id)
        .bind(&email)
        .bind(&input.role)
        .bind(hash_token(&raw_token))
        .bind(expires_at)
        .bind(user.id)
        .execute(&state.pool)
        .await?;
    Ok(Json(json!({
        "id": id,
        "email": email,
        "role": input.role,
        "expiresAt": expires_at,
        "token": raw_token,
        "acceptUrl": format!("{}/api/v1/auth/invitations/{}/accept", state.config.public_base_url, raw_token)
    })))
}

pub async fn accept_invitation(
    State(state): State<AppState>,
    Path(token): Path<String>,
    Json(input): Json<AcceptInvitationRequest>,
) -> Result<Json<Value>, ApiError> {
    validate_device(&input.device_id, &input.platform)?;
    let password_hash = hash_password(&input.password).map_err(ApiError::bad_request)?;
    let mut tx = state.pool.begin().await?;
    let invite = sqlx::query("SELECT id, project_id, email, role FROM project_invitations WHERE token_hash = $1 AND accepted_at IS NULL AND expires_at > now() FOR UPDATE")
        .bind(hash_token(&token))
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::not_found("invitation is invalid or expired"))?;
    let invitation_id: Uuid = invite.get("id");
    let project_id: Uuid = invite.get("project_id");
    let email: String = invite.get("email");
    let role: String = invite.get("role");
    let project_org: Option<Uuid> =
        sqlx::query_scalar("SELECT organization_id FROM projects WHERE id = $1")
            .bind(project_id)
            .fetch_one(&mut *tx)
            .await?;
    let existing = sqlx::query(
        "SELECT id, organization_id, password_hash FROM users WHERE lower(email) = $1 FOR UPDATE",
    )
    .bind(&email)
    .fetch_optional(&mut *tx)
    .await?;
    let user_id = if let Some(row) = existing {
        let id: Uuid = row.get("id");
        let existing_org: Option<Uuid> = row.get("organization_id");
        if project_org.is_some() && existing_org.is_some() && project_org != existing_org {
            return Err(ApiError::forbidden(
                "invitation belongs to another organization",
            ));
        }
        if row.get::<Option<String>, _>("password_hash").is_none() {
            sqlx::query("UPDATE users SET password_hash = $2, display_name = COALESCE(NULLIF($3, ''), display_name), status = 'active' WHERE id = $1")
                .bind(id)
                .bind(&password_hash)
                .bind(&input.display_name)
                .execute(&mut *tx)
                .await?;
        }
        id
    } else {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO users (id, organization_id, display_name, email, password_hash, status) VALUES ($1, $2, $3, $4, $5, 'active')")
            .bind(id)
            .bind(project_org)
            .bind(&input.display_name)
            .bind(&email)
            .bind(&password_hash)
            .execute(&mut *tx)
            .await?;
        id
    };
    sqlx::query("INSERT INTO project_members (project_id, user_id, role) VALUES ($1, $2, $3) ON CONFLICT(project_id, user_id) DO UPDATE SET role = excluded.role, deleted_at = NULL")
        .bind(project_id)
        .bind(user_id)
        .bind(&role)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE project_invitations SET accepted_at = now() WHERE id = $1")
        .bind(invitation_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    let session = create_session(
        &state,
        user_id,
        &input.device_id,
        &input.platform,
        &input.device_name,
    )
    .await?;
    Ok(Json(
        session_response(&state, user_id, project_org, &input.device_id, session).await?,
    ))
}

pub async fn bootstrap_admin(
    pool: &Pool<Postgres>,
    input: BootstrapAdminRequest,
) -> Result<Value, String> {
    let email = normalize_email(&input.email).map_err(|error| error.message)?;
    let password_hash = hash_password(&input.password)?;
    let mut tx = pool.begin().await.map_err(|error| error.to_string())?;
    let organization_id: Uuid =
        sqlx::query_scalar("INSERT INTO organizations (name) VALUES ($1) RETURNING id")
            .bind(input.organization_name.trim())
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| error.to_string())?;
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users (id, organization_id, display_name, email, password_hash, status) VALUES ($1, $2, $3, $4, $5, 'active')")
        .bind(user_id)
        .bind(organization_id)
        .bind(input.display_name.trim())
        .bind(email)
        .bind(password_hash)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query("UPDATE organizations SET owner_id = $2 WHERE id = $1")
        .bind(organization_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;
    tx.commit().await.map_err(|error| error.to_string())?;
    Ok(json!({ "userId": user_id, "organizationId": organization_id }))
}

struct SessionMaterial {
    id: Uuid,
    refresh_token: String,
    expires_at: chrono::DateTime<Utc>,
}

async fn create_session(
    state: &AppState,
    user_id: Uuid,
    device_key: &str,
    platform: &str,
    device_name: &str,
) -> Result<SessionMaterial, ApiError> {
    let device_id: Uuid = sqlx::query_scalar("INSERT INTO devices (user_id, device_key, platform, name) VALUES ($1, $2, $3, $4) ON CONFLICT(user_id, device_key) DO UPDATE SET platform = excluded.platform, name = excluded.name, last_seen_at = now(), revoked_at = NULL RETURNING id")
        .bind(user_id)
        .bind(device_key)
        .bind(platform)
        .bind(device_name)
        .fetch_one(&state.pool)
        .await?;
    let refresh_token = new_refresh_token();
    let expires_at = Utc::now() + Duration::seconds(state.config.refresh_ttl_seconds);
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO sessions (id, user_id, device_id, refresh_token_hash, expires_at) VALUES ($1, $2, $3, $4, $5)")
        .bind(id)
        .bind(user_id)
        .bind(device_id)
        .bind(hash_token(&refresh_token))
        .bind(expires_at)
        .execute(&state.pool)
        .await?;
    Ok(SessionMaterial {
        id,
        refresh_token,
        expires_at,
    })
}

async fn session_response(
    state: &AppState,
    user_id: Uuid,
    organization_id: Option<Uuid>,
    device_id: &str,
    session: SessionMaterial,
) -> Result<Value, ApiError> {
    let user_row = sqlx::query("SELECT email, display_name FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_one(&state.pool)
        .await?;
    let (access_token, access_expires) = issue_access_token(
        &state.config,
        user_id,
        organization_id,
        session.id,
        device_id,
    )
    .map_err(ApiError::internal_message)?;
    Ok(json!({
        "accessToken": access_token,
        "tokenType": "Bearer",
        "expiresAt": access_expires,
        "expiresAtUnix": access_expires.timestamp(),
        "expiresIn": state.config.access_ttl_seconds,
        "refreshToken": session.refresh_token,
        "refreshExpiresAt": session.expires_at,
        "sessionId": session.id,
        "deviceId": device_id,
        "accountId": user_id,
        "user": {
            "id": user_id,
            "email": user_row.get::<String, _>("email"),
            "displayName": user_row.get::<String, _>("display_name"),
            "organizationId": organization_id
        }
    }))
}

fn normalize_email(email: &str) -> Result<String, ApiError> {
    let value = email.trim().to_lowercase();
    if value.is_empty() || !value.contains('@') || value.len() > 320 {
        Err(ApiError::bad_request("a valid email is required"))
    } else {
        Ok(value)
    }
}

fn validate_device(device_id: &str, platform: &str) -> Result<(), ApiError> {
    if device_id.trim().is_empty() || device_id.len() > 200 {
        return Err(ApiError::bad_request("deviceId is required"));
    }
    if !matches!(platform, "ios" | "android" | "desktop" | "web") {
        return Err(ApiError::bad_request("invalid platform"));
    }
    Ok(())
}

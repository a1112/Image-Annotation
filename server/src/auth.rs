use crate::state::AppState;
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub exp: usize,
    pub organization_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub organization_id: Option<Uuid>,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let token = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .ok_or((StatusCode::UNAUTHORIZED, "missing bearer token".to_string()))?;
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(state.config.jwt_secret.as_bytes()),
            &Validation::new(Algorithm::HS256),
        )
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid access token".to_string()))?
        .claims;
        Ok(Self {
            id: claims.sub,
            organization_id: claims.organization_id,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectPermission {
    View,
    Annotate,
    Review,
    ManageData,
    ManageMembers,
    Export,
}

pub async fn require_project_permission(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    permission: ProjectPermission,
) -> Result<String, (StatusCode, String)> {
    let role = sqlx::query(
        "SELECT role FROM project_members WHERE project_id = $1 AND user_id = $2 AND deleted_at IS NULL",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal_error)?
    .map(|row| row.get::<String, _>("role"))
    .ok_or((StatusCode::FORBIDDEN, "project membership is required".to_string()))?;
    let allowed = match role.as_str() {
        "owner" => true,
        "manager" => true,
        "annotator" => matches!(permission, ProjectPermission::View | ProjectPermission::Annotate),
        "reviewer" => matches!(permission, ProjectPermission::View | ProjectPermission::Review),
        "viewer" => matches!(permission, ProjectPermission::View),
        _ => false,
    };
    if allowed {
        Ok(role)
    } else {
        Err((StatusCode::FORBIDDEN, format!("role {role} lacks permission")))
    }
}

pub fn internal_error(error: sqlx::Error) -> (StatusCode, String) {
    tracing::error!(error = %error, "database request failed");
    (StatusCode::INTERNAL_SERVER_ERROR, "database request failed".to_string())
}

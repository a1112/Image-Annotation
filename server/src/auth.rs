use crate::{config::Config, state::AppState};
use argon2::{
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString},
    Argon2,
};
use axum::{
    extract::FromRequestParts,
    http::{request::Parts, HeaderMap, Method, StatusCode, Uri},
};
use chrono::{Duration, Utc};
use jsonwebtoken::{decode, encode, Algorithm, DecodingKey, EncodingKey, Header, Validation};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claims {
    pub sub: Uuid,
    pub exp: usize,
    #[serde(default)]
    pub organization_id: Option<Uuid>,
    #[serde(default)]
    pub sid: Option<Uuid>,
    #[serde(default)]
    pub device_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AuthUser {
    pub id: Uuid,
    pub organization_id: Option<Uuid>,
    pub session_id: Option<Uuid>,
    pub device_id: Option<String>,
}

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let bearer = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "));
        let cookie_token = if bearer.is_none() {
            let cookie = cookie_value(&parts.headers, "ia_web_access");
            if cookie.is_some()
                && !matches!(parts.method, Method::GET | Method::HEAD | Method::OPTIONS)
            {
                validate_web_origin(&parts.headers, &state.config)?;
            }
            cookie
        } else {
            None
        };
        let token = bearer.or(cookie_token.as_deref()).ok_or((
            StatusCode::UNAUTHORIZED,
            "authentication is required".to_string(),
        ))?;
        let claims = decode::<Claims>(
            token,
            &DecodingKey::from_secret(state.config.jwt_secret.as_bytes()),
            &Validation::new(Algorithm::HS256),
        )
        .map_err(|_| (StatusCode::UNAUTHORIZED, "invalid access token".to_string()))?
        .claims;
        let row = sqlx::query(
            "SELECT organization_id, status FROM users WHERE id = $1 AND status = 'active' AND deleted_at IS NULL",
        )
        .bind(claims.sub)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::UNAUTHORIZED, "user is inactive".to_string()))?;

        let session_id = claims
            .sid
            .ok_or((StatusCode::UNAUTHORIZED, "session is required".to_string()))?;
        let session = sqlx::query(
            "SELECT s.id, d.device_key FROM sessions s JOIN devices d ON d.id = s.device_id WHERE s.id = $1 AND s.user_id = $2 AND s.revoked_at IS NULL AND s.expires_at > now() AND d.revoked_at IS NULL",
        )
        .bind(session_id)
        .bind(claims.sub)
        .fetch_optional(&state.pool)
        .await
        .map_err(internal_error)?
        .ok_or((StatusCode::UNAUTHORIZED, "session is revoked or expired".to_string()))?;
        sqlx::query("UPDATE sessions SET last_seen_at = now() WHERE id = $1")
            .bind(session_id)
            .execute(&state.pool)
            .await
            .map_err(internal_error)?;
        Ok(Self {
            id: claims.sub,
            organization_id: row.get("organization_id"),
            session_id: Some(session_id),
            device_id: Some(session.get("device_key")),
        })
    }
}

pub(crate) fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
}

/// Cookie authentication is restricted to the configured deployment origin.
/// Native clients continue to use explicit Bearer credentials and are not
/// affected by this browser-only CSRF boundary.
pub(crate) fn validate_web_origin(
    headers: &HeaderMap,
    config: &Config,
) -> Result<(), (StatusCode, String)> {
    let origin = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Uri>().ok());
    let expected = config.public_base_url.parse::<Uri>().ok();
    let same = origin.zip(expected).is_some_and(|(origin, expected)| {
        let scheme = origin.scheme_str();
        let expected_scheme = expected.scheme_str();
        let port = |uri: &Uri| {
            uri.port_u16().or(match uri.scheme_str() {
                Some("https") => Some(443),
                Some("http") => Some(80),
                _ => None,
            })
        };
        matches!(scheme, Some("https") | Some("http"))
            && scheme == expected_scheme
            && origin
                .host()
                .zip(expected.host())
                .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
            && port(&origin) == port(&expected)
            && origin.path() == "/"
            && origin.query().is_none()
    });
    if same {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "browser request origin is not allowed".to_string(),
        ))
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
    TaskClaim,
    IssueWrite,
    SchemaView,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetPermission {
    View,
    Manage,
    ManageMembers,
}

/// Dataset ACLs are intentionally separate from project membership.  A
/// dataset member may read an immutable version without gaining write access
/// to any project; materializing a version still requires project ManageData.
pub async fn require_dataset_permission(
    state: &AppState,
    user_id: Uuid,
    dataset_id: Uuid,
    permission: DatasetPermission,
) -> Result<String, (StatusCode, String)> {
    let row = sqlx::query(
        "SELECT dm.role FROM dataset_members dm JOIN datasets d ON d.id = dm.dataset_id JOIN users u ON u.id = dm.user_id WHERE dm.dataset_id = $1 AND dm.user_id = $2 AND dm.deleted_at IS NULL AND d.deleted_at IS NULL AND d.status <> 'deleted' AND d.organization_id = u.organization_id",
    )
    .bind(dataset_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal_error)?
    .map(|row| row.get::<String, _>("role"))
    .ok_or((StatusCode::FORBIDDEN, "dataset membership is required".to_string()))?;
    let allowed = match row.as_str() {
        "owner" => true,
        "manager" => matches!(
            permission,
            DatasetPermission::View | DatasetPermission::Manage | DatasetPermission::ManageMembers
        ),
        "viewer" => matches!(permission, DatasetPermission::View),
        _ => false,
    };
    if allowed {
        Ok(row)
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!("role {row} lacks dataset permission"),
        ))
    }
}

pub async fn require_project_permission(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
    permission: ProjectPermission,
) -> Result<String, (StatusCode, String)> {
    let role = sqlx::query(
        "SELECT pm.role FROM project_members pm JOIN projects p ON p.id = pm.project_id JOIN users u ON u.id = pm.user_id WHERE pm.project_id = $1 AND pm.user_id = $2 AND pm.deleted_at IS NULL AND p.deleted_at IS NULL AND (p.organization_id IS NULL OR p.organization_id = u.organization_id)",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_optional(&state.pool)
    .await
    .map_err(internal_error)?
    .map(|row| row.get::<String, _>("role"))
    .ok_or((StatusCode::FORBIDDEN, "project membership is required".to_string()))?;
    let allowed = match role.as_str() {
        "owner" | "manager" => true,
        "annotator" => matches!(
            permission,
            ProjectPermission::View
                | ProjectPermission::Annotate
                | ProjectPermission::TaskClaim
                | ProjectPermission::IssueWrite
                | ProjectPermission::SchemaView
        ),
        "reviewer" => matches!(
            permission,
            ProjectPermission::View
                | ProjectPermission::Review
                | ProjectPermission::TaskClaim
                | ProjectPermission::IssueWrite
                | ProjectPermission::SchemaView
        ),
        "viewer" => matches!(
            permission,
            ProjectPermission::View | ProjectPermission::SchemaView
        ),
        _ => false,
    };
    if allowed {
        Ok(role)
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!("role {role} lacks permission"),
        ))
    }
}

/// Change feeds remain readable after a project is tombstoned so that active
/// members on other devices can observe the deletion. This intentionally does
/// not reuse `require_project_permission`: that query excludes deleted
/// projects, while this one still requires active membership and org match.
/// Push/bootstrap and all business routes continue to use the normal check.
pub async fn require_deleted_project_changes(
    state: &AppState,
    user_id: Uuid,
    project_id: Uuid,
) -> Result<(), (StatusCode, String)> {
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM project_members pm JOIN projects p ON p.id = pm.project_id JOIN users u ON u.id = pm.user_id WHERE pm.project_id = $1 AND pm.user_id = $2 AND pm.deleted_at IS NULL AND p.deleted_at IS NOT NULL AND u.deleted_at IS NULL AND (p.organization_id IS NULL OR p.organization_id = u.organization_id))",
    )
    .bind(project_id)
    .bind(user_id)
    .fetch_one(&state.pool)
    .await
    .map_err(internal_error)?;
    if exists {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            "project membership is required".to_string(),
        ))
    }
}

pub fn hash_password(password: &str) -> Result<String, String> {
    if password.len() < 10 {
        return Err("password must contain at least 10 characters".to_string());
    }
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|error| format!("password hashing failed: {error}"))
}

pub fn verify_password(password: &str, encoded: &str) -> bool {
    PasswordHash::new(encoded).ok().is_some_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

pub fn issue_access_token(
    config: &Config,
    user_id: Uuid,
    organization_id: Option<Uuid>,
    session_id: Uuid,
    device_id: &str,
) -> Result<(String, chrono::DateTime<Utc>), String> {
    let expires_at = Utc::now() + Duration::seconds(config.access_ttl_seconds);
    let claims = Claims {
        sub: user_id,
        exp: expires_at.timestamp() as usize,
        organization_id,
        sid: Some(session_id),
        device_id: Some(device_id.to_string()),
    };
    encode(
        &Header::new(Algorithm::HS256),
        &claims,
        &EncodingKey::from_secret(config.jwt_secret.as_bytes()),
    )
    .map(|token| (token, expires_at))
    .map_err(|error| format!("access token signing failed: {error}"))
}

pub fn hash_token(token: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn new_refresh_token() -> String {
    format!("{}.{}", Uuid::new_v4(), Uuid::new_v4())
}

pub fn internal_error(error: sqlx::Error) -> (StatusCode, String) {
    tracing::error!(error = %error, "database request failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "database request failed".to_string(),
    )
}

#[cfg(test)]
mod browser_origin_tests {
    use super::*;

    fn config() -> Config {
        Config {
            bind_address: "127.0.0.1:8080".into(),
            database_url: "unused".into(),
            jwt_secret: "unused".into(),
            s3_bucket: "unused".into(),
            s3_endpoint: None,
            public_base_url: "https://example.test/image-annotation".into(),
            access_ttl_seconds: 900,
            refresh_ttl_seconds: 2_592_000,
        }
    }

    #[test]
    fn cookie_origin_rejects_missing_foreign_and_malformed_origins() {
        let config = config();
        let mut headers = HeaderMap::new();
        assert!(validate_web_origin(&headers, &config).is_err());
        for value in [
            "https://attacker.test",
            "http://example.test",
            "null",
            "https://example.test/path",
            "https://example.test:444",
        ] {
            headers.insert("origin", value.parse().unwrap());
            assert!(validate_web_origin(&headers, &config).is_err(), "{value}");
        }
        for value in ["https://example.test", "https://example.test:443"] {
            headers.insert("origin", value.parse().unwrap());
            assert!(validate_web_origin(&headers, &config).is_ok(), "{value}");
        }
    }

    #[test]
    fn cookies_are_matched_by_exact_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            "not_ia_web_access=foreign; ia_web_access=actual; other=value"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            cookie_value(&headers, "ia_web_access").as_deref(),
            Some("actual")
        );
        assert!(cookie_value(&headers, "ia_web_refresh").is_none());
    }
}

use crate::auth::AuthUser;
use anyhow::{ensure, Context, Result};
use axum::http::{header, HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use std::{collections::HashMap, env, time::Duration};
use uuid::Uuid;

#[derive(Clone)]
pub struct CentralAuth {
    client: Client,
    endpoint: String,
    key: String,
    issuer: String,
    users: HashMap<String, Uuid>,
}

#[derive(Deserialize)]
struct Identity {
    issuer: String,
    subject: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Session {
    id: Uuid,
    expires_at: DateTime<Utc>,
}
#[derive(Deserialize)]
struct State {
    identity: Option<Identity>,
    session: Option<Session>,
}

type Rejection = (StatusCode, String);
fn unavailable() -> Rejection {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "R-Auth session service unavailable".into(),
    )
}
fn unauthorized() -> Rejection {
    (
        StatusCode::UNAUTHORIZED,
        "Sign in to Image Annotation".into(),
    )
}

fn cookie_hash(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut matches = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok())
        .flat_map(|line| line.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter(|(key, _)| key.trim() == name);
    let (_, value) = matches.next()?;
    if matches.next().is_some()
        || value.len() != 43
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return None;
    }
    Some(hex::encode(Sha256::digest(value.as_bytes())))
}

impl CentralAuth {
    pub fn from_env() -> Result<Option<Self>> {
        let Ok(origin) = env::var("R_AUTH_ORIGIN") else {
            return Ok(None);
        };
        let url = Url::parse(&origin)?;
        ensure!(
            url.scheme() == "https" && url.origin().ascii_serialization() == origin,
            "R_AUTH_ORIGIN must be an HTTPS origin"
        );
        let issuer = env::var("OIDC_ISSUER")?;
        let key = env::var("R_AUTH_SESSION_KEY")?;
        ensure!(
            key.len() == 43
                && key
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Invalid project key"
        );
        let users = serde_json::from_str(
            &env::var("R_AUTH_USER_BINDINGS_JSON")
                .context("Configure Image Annotation user bindings")?,
        )?;
        let mut client = Client::builder()
            .https_only(true)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(5));
        if let Ok(file) = env::var("R_AUTH_CA_FILE") {
            client =
                client.add_root_certificate(reqwest::Certificate::from_pem(&std::fs::read(file)?)?);
        }
        Ok(Some(Self {
            client: client.build()?,
            endpoint: format!("{origin}/api/integrations/v1/session-store/image-annotation"),
            issuer,
            users,
            key,
        }))
    }

    async fn request(
        &self,
        operation: &str,
        headers: &HeaderMap,
    ) -> Result<reqwest::Response, Rejection> {
        let browser = cookie_hash(headers, "__Host-project_browser").ok_or_else(unauthorized)?;
        let proof = cookie_hash(headers, "__Host-project_session").ok_or_else(unauthorized)?;
        let answer = self
            .client
            .post(format!("{}/{operation}", self.endpoint))
            .bearer_auth(&self.key)
            .json(&json!({ "browserHash": browser, "sessionHash": proof }))
            .send()
            .await
            .map_err(|_| unavailable())?;
        if !answer.status().is_success() {
            return Err(unavailable());
        }
        Ok(answer)
    }

    pub async fn logout(&self, headers: &HeaderMap) -> Result<(), Rejection> {
        self.request("logout-session", headers).await?;
        Ok(())
    }

    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        pool: &PgPool,
    ) -> Result<AuthUser, Rejection> {
        let answer = self.request("state", headers).await?;
        let accepted: State = answer.json().await.map_err(|_| unavailable())?;
        let identity = accepted
            .identity
            .filter(|i| i.issuer == self.issuer)
            .ok_or_else(unauthorized)?;
        let session = accepted
            .session
            .filter(|s| s.expires_at > Utc::now())
            .ok_or_else(unauthorized)?;
        let user = self.users.get(&identity.subject).ok_or((
            StatusCode::FORBIDDEN,
            "Account has no Image Annotation binding".into(),
        ))?;
        let mut tx = pool.begin().await.map_err(|_| unavailable())?;
        let account = sqlx::query("SELECT organization_id FROM users WHERE id=$1 AND status='active' AND deleted_at IS NULL FOR SHARE")
            .bind(user).fetch_optional(&mut *tx).await.map_err(|_| unavailable())?.ok_or_else(unauthorized)?;
        let device_key = format!(
            "r-auth-{}",
            cookie_hash(headers, "__Host-project_browser").ok_or_else(unauthorized)?
        );
        let device: Option<Uuid> = sqlx::query_scalar("INSERT INTO devices(user_id,device_key,platform,name) VALUES($1,$2,'web','R-Auth Web')
            ON CONFLICT(user_id,device_key) DO UPDATE SET last_seen_at=now() WHERE devices.revoked_at IS NULL RETURNING id")
            .bind(user).bind(&device_key).fetch_optional(&mut *tx).await.map_err(|_| unavailable())?;
        let device = device.ok_or_else(unauthorized)?;
        let local = sqlx::query("INSERT INTO sessions(id,user_id,device_id,refresh_token_hash,expires_at) VALUES($1,$2,$3,$4,$5)
            ON CONFLICT(id) DO UPDATE SET last_seen_at=now() WHERE sessions.revoked_at IS NULL AND sessions.expires_at>now()
            AND sessions.user_id=EXCLUDED.user_id AND sessions.device_id=EXCLUDED.device_id RETURNING id")
            .bind(session.id).bind(user).bind(device).bind(format!("r-auth:{}", session.id)).bind(session.expires_at)
            .fetch_optional(&mut *tx).await.map_err(|_| unavailable())?;
        if local.is_none() {
            return Err(unauthorized());
        }
        tx.commit().await.map_err(|_| unavailable())?;
        Ok(AuthUser {
            id: *user,
            organization_id: account.get("organization_id"),
            session_id: Some(session.id),
            device_id: Some(device_key),
        })
    }
}

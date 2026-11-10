//! Same-origin browser sessions. Session authority remains in PostgreSQL;
//! access and rotating refresh credentials are HttpOnly cookies.
use super::{account, ApiError};
use crate::{
    auth::{cookie_value, validate_web_origin, AuthUser},
    state::AppState,
};
use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebLogin {
    pub email: String,
    pub password: String,
    pub device_id: String,
    #[serde(default)]
    pub device_name: String,
}

fn origin(headers: &HeaderMap, state: &AppState) -> Result<(), ApiError> {
    validate_web_origin(headers, &state.config).map_err(|(_, message)| ApiError::forbidden(message))
}

fn cookie_path(state: &AppState) -> String {
    state
        .config
        .public_base_url
        .parse::<axum::http::Uri>()
        .ok()
        .map(|uri| uri.path().trim_end_matches('/').to_string())
        .filter(|path| {
            !path.is_empty() && path.starts_with('/') && !path.contains([';', '\r', '\n'])
        })
        .unwrap_or_else(|| "/".to_string())
}

fn cookie(state: &AppState, name: &str, value: &str, ttl: i64) -> Result<HeaderValue, ApiError> {
    let secure = if state.config.public_base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    HeaderValue::from_str(&format!(
        "{name}={value}; Path={}; HttpOnly; SameSite=Lax; Max-Age={}{}",
        cookie_path(state),
        ttl.max(0),
        secure
    ))
    .map_err(|_| ApiError::internal_message("invalid browser session cookie"))
}

fn session_response(state: &AppState, mut payload: Value) -> Result<Response, ApiError> {
    let access = payload
        .get("accessToken")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal_message("login did not create an access credential"))?
        .to_string();
    let refresh = payload
        .get("refreshToken")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::internal_message("login did not create a refresh credential"))?
        .to_string();
    let access_cookie = cookie(
        state,
        "ia_web_access",
        &access,
        state.config.access_ttl_seconds,
    )?;
    let refresh_cookie = cookie(
        state,
        "ia_web_refresh",
        &refresh,
        state.config.refresh_ttl_seconds,
    )?;
    if let Some(object) = payload.as_object_mut() {
        for name in ["accessToken", "refreshToken", "tokenType", "sessionId"] {
            object.remove(name);
        }
        object.insert("authenticated".to_string(), Value::Bool(true));
    }
    let mut response = Json(payload).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, access_cookie);
    response
        .headers_mut()
        .append(header::SET_COOKIE, refresh_cookie);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<WebLogin>,
) -> Result<Response, ApiError> {
    origin(&headers, &state)?;
    let Json(payload) = account::login(
        State(state.clone()),
        Json(account::LoginRequest {
            email: input.email,
            password: input.password,
            device_id: input.device_id,
            platform: "web".to_string(),
            device_name: input.device_name,
        }),
    )
    .await?;
    session_response(&state, payload)
}

pub async fn refresh(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    origin(&headers, &state)?;
    let refresh_token = cookie_value(&headers, "ia_web_refresh")
        .ok_or_else(|| ApiError::unauthorized("browser session has expired"))?;
    let Json(payload) = account::refresh(
        State(state.clone()),
        Json(account::RefreshRequest {
            refresh_token,
            device_id: None,
        }),
    )
    .await?;
    session_response(&state, payload)
}

pub async fn session(State(state): State<AppState>, user: AuthUser) -> Result<Response, ApiError> {
    let device_id = user.device_id.clone();
    let Json(profile) = account::me(State(state), user).await?;
    let mut response =
        Json(json!({"authenticated":true,"user":profile,"deviceId":device_id})).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    origin(&headers, &state)?;
    // Revoke the session even when its short-lived access credential expired.
    if let Some(refresh) = cookie_value(&headers, "ia_web_refresh") {
        use sha2::{Digest, Sha256};
        let hash = format!("{:x}", Sha256::digest(refresh.as_bytes()));
        sqlx::query("UPDATE sessions SET revoked_at = now() WHERE refresh_token_hash = $1")
            .bind(hash)
            .execute(&state.pool)
            .await?;
    }
    let mut response = Json(json!({"authenticated":false,"revoked":true})).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie(&state, "ia_web_access", "", 0)?);
    response
        .headers_mut()
        .append(header::SET_COOKIE, cookie(&state, "ia_web_refresh", "", 0)?);
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

//! Native invitation acceptance for mobile clients.
//!
//! Invitation acceptance is deliberately separate from the authenticated
//! account commands: the invite token is the only credential required to
//! create the first session.  Access and refresh tokens stay in the native
//! credential store and the command only returns the same session summary as
//! the regular login command.

use super::account::RemoteSessionStatus;
use crate::credentials;
use serde_json::{json, Value};
use std::time::Duration;

const MAX_SERVER_URL_LENGTH: usize = 2048;
const MAX_INVITATION_TOKEN_LENGTH: usize = 256;

fn normalize_server_url(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_SERVER_URL_LENGTH {
        return Err("server URL is required and must be at most 2048 bytes".to_string());
    }
    if trimmed.chars().any(|character| character.is_control()) {
        return Err("server URL contains control characters".to_string());
    }
    let parsed =
        reqwest::Url::parse(trimmed).map_err(|error| format!("server URL is invalid: {error}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err("server URL must use http or https".to_string());
    }
    if parsed.host_str().is_none() || !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("server URL must contain a host and no user credentials".to_string());
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("server URL must not contain a query or fragment".to_string());
    }
    Ok(trimmed.trim_end_matches('/').to_string())
}

fn validate_invitation_token(token: &str) -> Result<&str, String> {
    let token = token.trim();
    // The deployed server currently emits UUID.UUID refresh-style invitation
    // tokens.  Dot is therefore accepted in addition to the URL-safe
    // alphabet; no slash, escape, query, or fragment character can enter the
    // path segment.
    if token.is_empty() || token.len() > MAX_INVITATION_TOKEN_LENGTH {
        return Err("invitation token is required and must be at most 256 bytes".to_string());
    }
    if !token.bytes().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, b'_' | b'-' | b'.')
    }) {
        return Err("invitation token contains invalid characters".to_string());
    }
    Ok(token)
}

fn platform_name() -> &'static str {
    if cfg!(target_os = "android") {
        "android"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else {
        "desktop"
    }
}

fn response_string(payload: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        payload
            .get(*name)
            .and_then(Value::as_str)
            .map(ToString::to_string)
    })
}

fn response_account_id(payload: &Value) -> Option<String> {
    response_string(payload, &["accountId", "account_id", "userId", "user_id"]).or_else(|| {
        payload
            .get("user")
            .and_then(|user| response_string(user, &["id", "userId", "user_id"]))
    })
}

fn response_expires_at(payload: &Value) -> Option<i64> {
    ["expiresAtUnix", "expires_at_unix"]
        .iter()
        .find_map(|name| payload.get(*name))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
                .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
        })
}

fn response_error(status: u16, payload: &Value) -> String {
    let message = response_string(payload, &["message", "error"])
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "invitation acceptance failed".to_string());
    format!("invitation acceptance failed ({status}): {message}")
}

#[tauri::command]
pub async fn auth_accept_invitation(
    server_url: String,
    token: String,
    display_name: String,
    password: String,
    device_name: Option<String>,
) -> Result<RemoteSessionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let server_url = normalize_server_url(&server_url)?;
        let token = validate_invitation_token(&token)?;
        if password.chars().count() < 10 {
            return Err("password must contain at least 10 characters".to_string());
        }
        if display_name.trim().is_empty() {
            return Err("display name is required".to_string());
        }
        let device_id = credentials::try_device_id()?;
        let device_name = device_name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| "Image Annotation Mobile".to_string());
        let endpoint = format!("{server_url}/api/v1/auth/invitations/{token}/accept");
        let client = reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(45))
            .build()
            .map_err(|error| format!("failed to create invitation client: {error}"))?;
        let response = client
            .post(endpoint)
            .json(&json!({
                "displayName": display_name,
                "password": password,
                "deviceId": device_id,
                "platform": platform_name(),
                "deviceName": device_name
            }))
            .send()
            .map_err(|error| format!("invitation request failed: {error}"))?;
        let status = response.status().as_u16();
        let payload = response
            .json::<Value>()
            .unwrap_or_else(|_| Value::Object(Default::default()));
        if !(200..300).contains(&status) {
            return Err(response_error(status, &payload));
        }

        let access_token = response_string(&payload, &["accessToken", "access_token"])
            .ok_or_else(|| "invitation response has no access token".to_string())?;
        let refresh_token = response_string(&payload, &["refreshToken", "refresh_token"])
            .ok_or_else(|| "invitation response has no refresh token".to_string())?;
        let account_id = response_account_id(&payload)
            .ok_or_else(|| "invitation response has no account id".to_string())?;
        let expires_at = response_expires_at(&payload);
        let session = credentials::SessionCredentials {
            server_url: server_url.clone(),
            account_id: account_id.clone(),
            access_token,
            refresh_token,
            device_id: device_id.clone(),
            access_expires_at: expires_at,
        };
        let scope = credentials::session_scope(&server_url, &account_id);
        credentials::store_session(&scope, &session)?;
        credentials::set_active_scope(&scope)?;

        Ok(RemoteSessionStatus {
            authenticated: true,
            server_url: Some(server_url),
            account_id: Some(account_id),
            device_id,
            access_expires_at: expires_at,
        })
    })
    .await
    .map_err(|error| format!("invitation worker failed: {error}"))?
}

use crate::{credentials, hybrid, project_fs, storage, sync_engine};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionStatus {
    pub authenticated: bool,
    pub server_url: Option<String>,
    pub account_id: Option<String>,
    pub device_id: String,
    pub access_expires_at: Option<i64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteLoginResponse {
    #[serde(alias = "access_token")]
    access_token: String,
    #[serde(alias = "refresh_token")]
    refresh_token: String,
    /// The API deliberately returns both the human-readable RFC3339 value
    /// and the Unix value. Keep them as separate fields: serde rejects a
    /// payload that contains two aliases for one field as a duplicate key.
    #[serde(default, rename = "expiresAt", alias = "expires_at")]
    access_expires_at: Option<Value>,
    #[serde(default, rename = "expiresAtUnix", alias = "expires_at_unix")]
    access_expires_at_unix: Option<Value>,
    #[serde(default)]
    user: Option<Value>,
    #[serde(default)]
    account_id: Option<String>,
}

impl RemoteLoginResponse {
    fn access_expiry(&self) -> Option<i64> {
        let parse = |value: &Value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
                .or_else(|| {
                    value.as_str().and_then(|value| {
                        value.parse::<i64>().ok().or_else(|| {
                            DateTime::parse_from_rfc3339(value)
                                .ok()
                                .map(|date| date.timestamp())
                        })
                    })
                })
        };
        self.access_expires_at_unix
            .as_ref()
            .and_then(parse)
            .or_else(|| self.access_expires_at.as_ref().and_then(parse))
    }
}

fn json_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| value.get(*name))
}

fn remote_url(server_url: &str, path: &str) -> Result<String, String> {
    let server_url = server_url.trim_end_matches('/');
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        return Err("server URL must use http or https".to_string());
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Ok(format!("{server_url}{path}"))
}

fn remote_http_request(
    method: &str,
    url: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Result<(u16, Value), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| format!("failed to create remote client: {error}"))?;
    let method = reqwest::Method::from_bytes(method.as_bytes())
        .map_err(|error| format!("invalid HTTP method: {error}"))?;
    let mut request = client.request(method, url);
    if let Some(token) = token.filter(|value| !value.trim().is_empty()) {
        request = request.bearer_auth(token.trim());
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .map_err(|error| format!("remote request failed: {error}"))?;
    let status = response.status().as_u16();
    let payload = response.json::<Value>().unwrap_or_else(|_| json!({}));
    Ok((status, payload))
}

fn session_summary(
    session: Option<credentials::SessionCredentials>,
) -> Result<RemoteSessionStatus, String> {
    let device_id = credentials::try_device_id()?;
    Ok(match session {
        Some(session) => RemoteSessionStatus {
            authenticated: !session.access_token.is_empty(),
            server_url: Some(session.server_url),
            account_id: Some(session.account_id),
            device_id,
            access_expires_at: session.access_expires_at,
        },
        None => RemoteSessionStatus {
            authenticated: false,
            server_url: None,
            account_id: None,
            device_id,
            access_expires_at: None,
        },
    })
}

#[tauri::command]
pub async fn auth_login(
    server_url: String,
    email: String,
    password: String,
    device_name: Option<String>,
) -> Result<RemoteSessionStatus, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let url = remote_url(&server_url, "/api/v1/auth/login")?;
        let device_id = credentials::try_device_id()?;
        let body = json!({
            "email": email,
            "password": password,
            "deviceId": device_id,
            "platform": if cfg!(target_os = "android") { "android" } else if cfg!(target_os = "ios") { "ios" } else { "desktop" },
            "deviceName": device_name.unwrap_or_else(|| "Image Annotation".to_string())
        });
        let (status, payload) = remote_http_request("POST", &url, None, Some(body))?;
        if !(200..300).contains(&status) {
            return Err(format!("login failed ({status}): {payload}"));
        }
        let response: RemoteLoginResponse = serde_json::from_value(payload)
            .map_err(|error| format!("login response is invalid: {error}"))?;
        let access_expires_at = response.access_expiry();
        let account_id = response
            .account_id
            .or_else(|| {
                response.user.as_ref().and_then(|user| {
                    json_field(user, &["id", "userId"])
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
            })
            .or_else(|| {
                let (me_status, me_payload) = remote_http_request(
                    "GET",
                    &remote_url(&server_url, "/api/v1/auth/me").ok()?,
                    Some(&response.access_token),
                    None,
                )
                .ok()?;
                if !(200..300).contains(&me_status) {
                    return None;
                }
                json_field(&me_payload, &["id", "accountId", "userId"])
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .ok_or_else(|| {
                "login response does not include account id (and /auth/me did not resolve it)"
                    .to_string()
            })?;
        let session = credentials::SessionCredentials {
            server_url: server_url.trim_end_matches('/').to_string(),
            account_id: account_id.clone(),
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            device_id,
            access_expires_at,
        };
        let scope = credentials::session_scope(&server_url, &account_id);
        credentials::store_session(&scope, &session)?;
        credentials::set_active_scope(&scope)?;
        session_summary(Some(session))
    })
    .await
    .map_err(|error| format!("login worker failed: {error}"))?
}

#[tauri::command]
pub async fn auth_refresh() -> Result<RemoteSessionStatus, String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(session) = credentials::refresh_current_session(true)? else {
            return Err("no authenticated session".to_string());
        };
        session_summary(Some(session))
    })
    .await
    .map_err(|error| format!("refresh worker failed: {error}"))?
}

#[tauri::command]
pub async fn auth_logout() -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(|| {
        let Some(current) = credentials::current_session()? else {
            return Ok(());
        };
        let url = remote_url(&current.server_url, "/api/v1/auth/logout")?;
        let _ = remote_http_request("POST", &url, Some(&current.access_token), None);
        let scope = credentials::session_scope(&current.server_url, &current.account_id);
        credentials::delete_session(&scope)?;
        // Remove the pre-scope migration key as well. This is harmless when
        // absent and prevents an old account-id-only record from reviving
        // after logout.
        if scope != current.account_id {
            credentials::delete_session(&current.account_id)?;
        }
        credentials::deactivate_scope(&scope)?;
        Ok(())
    })
    .await
    .map_err(|error| format!("logout worker failed: {error}"))?
}

#[tauri::command]
pub fn auth_status() -> Result<RemoteSessionStatus, String> {
    session_summary(credentials::current_session()?)
}

#[tauri::command]
pub async fn remote_request(
    method: String,
    path: String,
    body: Option<Value>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let current = credentials::refreshed_session()?
            .ok_or_else(|| "no authenticated session".to_string())?;
        let url = remote_url(&current.server_url, &path)?;
        let (status, payload) =
            remote_http_request(&method, &url, Some(&current.access_token), body)?;
        if !(200..300).contains(&status) {
            return Err(format!("remote request failed ({status}): {payload}"));
        }
        Ok(payload)
    })
    .await
    .map_err(|error| format!("remote request worker failed: {error}"))?
}

pub(crate) fn scoped_local_project_id(
    server_url: &str,
    account_id: &str,
    remote_project_id: &str,
) -> String {
    let scope = format!(
        "{}\n{}\n{}",
        server_url.trim_end_matches('/'),
        account_id,
        remote_project_id
    );
    format!("remote-{:x}", Sha256::digest(scope.as_bytes()))
}

fn validate_remote_project_id(project_id: &str) -> Result<(), String> {
    if project_id.trim().is_empty()
        || project_id.contains('/')
        || project_id.contains('\\')
        || project_id.contains('?')
        || project_id.contains('#')
    {
        return Err("remote project ID is invalid".to_string());
    }
    Ok(())
}

#[tauri::command]
pub async fn remote_project_open(
    server_url: String,
    remote_project_id: String,
    mode: Option<String>,
    cache_policy: Option<String>,
) -> Result<Value, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let current = credentials::refreshed_session()?
            .ok_or_else(|| "no authenticated session".to_string())?;
        let server_url = server_url.trim_end_matches('/').to_string();
        if server_url != current.server_url.trim_end_matches('/') {
            return Err("project server does not match the active account session".to_string());
        }
        validate_remote_project_id(&remote_project_id)?;
        let bootstrap_url = remote_url(
            &server_url,
            &format!("/api/v1/projects/{remote_project_id}/sync-bootstrap"),
        )?;
        let (status, payload) =
            remote_http_request("GET", &bootstrap_url, Some(&current.access_token), None)?;
        if !(200..300).contains(&status) {
            return Err(format!("project bootstrap failed ({status}): {payload}"));
        }
        let project = payload.get("project").cloned().unwrap_or_else(|| json!({}));
        let local_id =
            scoped_local_project_id(&server_url, &current.account_id, &remote_project_id);
        let name = project
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("Remote project")
            .to_string();
        let paths = project_fs::ensure_workspace_project_dirs(&local_id)?;
        let existing_manifest = project_fs::read_manifest(&local_id);
        let database_exists = paths.sqlite.exists();
        let created_at = existing_manifest
            .as_ref()
            .map(|manifest| manifest.created_at.clone())
            .unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_secs().to_string())
                    .unwrap_or_default()
            });
        let root_path = existing_manifest
            .as_ref()
            .map(|manifest| manifest.root_path.clone())
            .unwrap_or_else(|| paths.root.to_string_lossy().to_string());
        let class_count = existing_manifest
            .as_ref()
            .map(|manifest| manifest.class_count)
            .unwrap_or(0);
        let image_count = existing_manifest
            .as_ref()
            .map(|manifest| manifest.image_count)
            .unwrap_or_else(|| {
                payload
                    .get("assets")
                    .and_then(Value::as_array)
                    .map(|items| items.len() as u32)
                    .unwrap_or(0)
            });
        let manifest = project_fs::ProjectManifest {
            id: local_id.clone(),
            name,
            source_dataset_key: format!(
                "remote:{}:{}:{}",
                server_url, current.account_id, remote_project_id
            ),
            format: "remote".to_string(),
            root_path,
            created_at,
            class_count,
            image_count,
        };
        project_fs::write_manifest_to_path(&manifest, &paths.manifest)?;
        storage::initialize_project_database(&paths.sqlite)?;
        if database_exists {
            storage::upsert_project_metadata(&paths.sqlite, &manifest)?;
        } else {
            // A new remote project has no local image/class rows yet. This
            // initial insert is safe; subsequent opens must preserve offline
            // rows and tombstones instead of clearing the indexes.
            storage::upsert_project_index(&paths.sqlite, &manifest, &[], &[])?;
        }
        let mut connection =
            rusqlite::Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
        hybrid::cache_remote_project(
            &mut connection,
            &local_id,
            &server_url,
            &remote_project_id,
            &current.device_id,
            mode.as_deref().unwrap_or("cloud_linked"),
            cache_policy.as_deref().unwrap_or("on_demand"),
            true,
        )?;
        connection
            .execute(
                "UPDATE remote_project_configs SET account_id = ?2 WHERE project_id = ?1",
                rusqlite::params![local_id, current.account_id],
            )
            .map_err(|error| error.to_string())?;
        let sync = sync_engine::sync_project(&paths.sqlite, &local_id, None)?;
        Ok(json!({
            "projectId": local_id.clone(),
            "localProjectId": local_id,
            "remoteProjectId": remote_project_id,
            "serverUrl": server_url,
            "accountId": current.account_id,
            "sync": sync
        }))
    })
    .await
    .map_err(|error| format!("project open worker failed: {error}"))?
}

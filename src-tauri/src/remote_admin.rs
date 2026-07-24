use crate::credentials;
use reqwest::{
    blocking::{Client, Response},
    header::CONTENT_TYPE,
    Method,
};
use rusqlite::Connection;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use std::{path::Path, time::Duration};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProjectMember {
    pub user_id: String,
    pub role: String,
    pub joined_at: Option<String>,
}

struct RemoteContext {
    server_url: String,
    remote_project_id: String,
    access_token: String,
}

pub fn list_members(
    sqlite_path: &Path,
    local_project_id: &str,
) -> Result<Vec<RemoteProjectMember>, String> {
    let context = remote_context(sqlite_path, local_project_id)?;
    request(
        &context,
        Method::GET,
        &format!(
            "/api/v1/projects/{}/members",
            context.remote_project_id
        ),
        None,
    )
}

pub fn upsert_member(
    sqlite_path: &Path,
    local_project_id: &str,
    user_id: &str,
    role: &str,
) -> Result<RemoteProjectMember, String> {
    validate_role(role)?;
    let context = remote_context(sqlite_path, local_project_id)?;
    request(
        &context,
        Method::POST,
        &format!(
            "/api/v1/projects/{}/members",
            context.remote_project_id
        ),
        Some(json!({ "userId": user_id, "role": role })),
    )
}

pub fn remove_member(
    sqlite_path: &Path,
    local_project_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let context = remote_context(sqlite_path, local_project_id)?;
    let _: serde_json::Value = request(
        &context,
        Method::DELETE,
        &format!(
            "/api/v1/projects/{}/members/{}",
            context.remote_project_id, user_id
        ),
        None,
    )?;
    Ok(())
}

fn remote_context(sqlite_path: &Path, local_project_id: &str) -> Result<RemoteContext, String> {
    let connection = Connection::open(sqlite_path).map_err(|error| error.to_string())?;
    let (server_url, remote_project_id) = connection
        .query_row(
            "SELECT server_url, remote_project_id FROM remote_project_configs WHERE project_id = ?1",
            [local_project_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|error| format!("remote project is not configured: {error}"))?;
    let access_token = credentials::read_access_token(local_project_id)?
        .ok_or_else(|| "project credential is not configured".to_string())?;
    Ok(RemoteContext {
        server_url,
        remote_project_id,
        access_token,
    })
}

fn request<T: DeserializeOwned>(
    context: &RemoteContext,
    method: Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<T, String> {
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to create remote admin client: {error}"))?;
    let mut request = client
        .request(
            method,
            format!("{}{}", context.server_url.trim_end_matches('/'), path),
        )
        .bearer_auth(&context.access_token);
    if let Some(value) = body {
        request = request
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&value).map_err(|error| error.to_string())?);
    }
    decode_response(
        request
            .send()
            .map_err(|error| format!("remote member request failed: {error}"))?,
    )
}

fn decode_response<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("failed to read remote member response: {error}"))?;
    if !status.is_success() {
        let message = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("remote member request failed with {status}"));
        return Err(message);
    }
    serde_json::from_str(&text)
        .map_err(|error| format!("invalid remote member response: {error}"))
}

fn validate_role(role: &str) -> Result<(), String> {
    if matches!(
        role,
        "owner" | "manager" | "annotator" | "reviewer" | "viewer"
    ) {
        Ok(())
    } else {
        Err("invalid project role".to_string())
    }
}

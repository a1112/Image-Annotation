use crate::{credentials, hybrid, storage};
use reqwest::blocking::Client;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

static AUTO_SYNC_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncRunResult {
    pub project_id: String,
    pub pushed: u32,
    pub pulled: u32,
    pub conflicts: u32,
    pub failed: u32,
    pub cursor: String,
}

pub fn start_auto_sync() {
    if AUTO_SYNC_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(|| loop {
        for manifest in crate::project_fs::list_project_manifests() {
            let paths = crate::project_fs::project_paths(&manifest.id);
            if storage::initialize_project_database(&paths.sqlite).is_err() {
                continue;
            }
            let Ok(connection) = Connection::open(&paths.sqlite) else {
                continue;
            };
            let enabled = hybrid::remote_project_config(&connection, &manifest.id)
                .ok()
                .flatten()
                .map(|config| config.auto_sync)
                .unwrap_or(false);
            drop(connection);
            if enabled {
                let _ = sync_project(&paths.sqlite, &manifest.id, None);
            }
        }
        thread::sleep(Duration::from_secs(30));
    });
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushResponse {
    results: Vec<PushOperationResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushOperationResult {
    operation_id: String,
    status: String,
    server_revision: Option<i64>,
    remote_payload: Option<Value>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PullResponse {
    next_cursor: String,
    has_more: bool,
    changes: Vec<RemoteChange>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BootstrapResponse {
    cursor: String,
    #[serde(default)]
    assets: Vec<Value>,
    #[serde(default)]
    annotations: Vec<Value>,
    #[serde(default)]
    issues: Vec<Value>,
    #[serde(default)]
    folders: Vec<Value>,
    #[serde(default)]
    issue_comments: Vec<Value>,
    #[serde(default)]
    issue_attachments: Vec<Value>,
    #[serde(default)]
    folder_members: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RemoteChange {
    entity_type: String,
    entity_id: String,
    operation: String,
    revision: Option<i64>,
    payload: Value,
}

pub fn sync_project(
    database_path: &Path,
    project_id: &str,
    access_token: Option<&str>,
) -> Result<SyncRunResult, String> {
    storage::initialize_project_database(database_path)?;
    let mut connection = Connection::open(database_path).map_err(|err| err.to_string())?;
    let config = hybrid::remote_project_config(&connection, project_id)?
        .ok_or_else(|| "project is not linked to a remote server".to_string())?;
    let token = access_token
        .map(str::to_string)
        .or(credentials::read_access_token(project_id)?)
        .or_else(|| std::env::var("IMAGE_ANNOTATION_ACCESS_TOKEN").ok())
        .ok_or_else(|| "remote access token is unavailable".to_string())?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|err| err.to_string())?;
    let mut result = SyncRunResult {
        project_id: project_id.to_string(),
        pushed: 0,
        pulled: 0,
        conflicts: 0,
        failed: 0,
        cursor: hybrid::current_pull_cursor(&connection, project_id)?,
    };

    if result.cursor == "0" {
        bootstrap_project(
            &client,
            &token,
            &config,
            &mut connection,
            &mut result,
        )?;
    }
    push_outbox(&client, &token, &config, &mut connection, &mut result)?;
    pull_changes(&client, &token, &config, &mut connection, &mut result)?;
    Ok(result)
}

fn bootstrap_project(
    client: &Client,
    token: &str,
    config: &hybrid::RemoteProjectConfig,
    connection: &mut Connection,
    result: &mut SyncRunResult,
) -> Result<(), String> {
    let response = client
        .get(format!(
            "{}/api/v1/projects/{}/sync-bootstrap",
            config.server_url, config.remote_project_id
        ))
        .header(AUTHORIZATION, format!("Bearer {token}"))
        .send()
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("sync bootstrap failed: {}", response.status()));
    }
    let snapshot = response
        .json::<BootstrapResponse>()
        .map_err(|error| error.to_string())?;
    let BootstrapResponse {
        cursor,
        assets,
        annotations,
        issues,
        folders,
        issue_comments,
        issue_attachments,
        folder_members,
        ..
    } = snapshot;
    let mut entities = Vec::new();
    for (entity_type, payloads) in [
        ("asset", assets),
        ("annotation", annotations),
        ("issue", issues),
        ("issue_comment", issue_comments),
        ("issue_attachment", issue_attachments),
        ("folder", folders),
        ("folder_member", folder_members),
    ] {
        for payload in payloads {
            let entity_id = if entity_type == "folder_member" {
                format!(
                    "{}:{}",
                    payload
                        .get("folderId")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                    payload
                        .get("imageId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                )
            } else {
                payload
                    .get("clientKey")
                    .or_else(|| payload.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            let revision = payload.get("revision").and_then(Value::as_i64);
            entities.push((entity_type.to_string(), entity_id, revision, payload));
        }
    }
    result.pulled = result
        .pulled
        .saturating_add(u32::try_from(entities.len()).unwrap_or(u32::MAX));
    hybrid::apply_remote_snapshot(connection, &config.project_id, entities, &cursor)?;
    result.cursor = cursor;
    Ok(())
}

fn apply_bootstrap_entity(
    connection: &mut Connection,
    project_id: &str,
    entity_type: &str,
    payload: &Value,
) -> Result<(), String> {
    let entity_id = match entity_type {
        "asset" | "issue" | "folder" | "issue_attachment" => payload
            .get("clientKey")
            .and_then(Value::as_str)
            .or_else(|| payload.get("id").and_then(Value::as_str))
            .map(str::to_owned),
        "annotation" => payload
            .get("imageId")
            .and_then(Value::as_str)
            .and_then(|remote_id| {
                connection
                    .query_row(
                        "SELECT id FROM images WHERE remote_id = ?1",
                        [remote_id],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
            })
            .or_else(|| {
                payload
                    .get("imageId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            }),
        "folder_member" => {
            let folder_id = payload
                .get("folderId")
                .and_then(Value::as_str)
                .ok_or_else(|| "bootstrap folder member has no folderId".to_string())?;
            let image_id = payload
                .get("imageId")
                .and_then(Value::as_str)
                .ok_or_else(|| "bootstrap folder member has no imageId".to_string())?;
            Some(format!("{folder_id}:{image_id}"))
        }
        _ => payload
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    }
    .ok_or_else(|| format!("bootstrap {entity_type} has no entity identifier"))?;
    hybrid::apply_remote_change(
        connection,
        project_id,
        entity_type,
        &entity_id,
        "update",
        payload.get("revision").and_then(Value::as_i64),
        payload,
    )
}

fn push_outbox(
    client: &Client,
    token: &str,
    config: &hybrid::RemoteProjectConfig,
    connection: &mut Connection,
    result: &mut SyncRunResult,
) -> Result<(), String> {
    loop {
        let operations = hybrid::list_ready_outbox(connection, &config.project_id, 100)?;
        if operations.is_empty() {
            return Ok(());
        }
        let operation_payloads = operations
            .iter()
            .map(|operation| {
                json!({
                    "operationId": operation.id,
                    "entityType": operation.entity_type,
                    "entityId": operation.entity_id,
                    "operation": operation.operation,
                    "baseRevision": operation.base_revision,
                    "payload": operation.payload
                })
            })
            .collect::<Vec<_>>();
        let response = client
            .post(format!("{}/api/v1/sync/push", config.server_url))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .header(CONTENT_TYPE, "application/json")
            .header(
                "Idempotency-Key",
                operations.first().map(|item| item.id.as_str()).unwrap_or("empty"),
            )
            .json(&json!({
                "deviceId": config.device_id,
                "projectId": config.remote_project_id,
                "operations": operation_payloads
            }))
            .send();

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                for operation in &operations {
                    hybrid::mark_outbox_failed(connection, operation, &error.to_string(), true)?;
                    result.failed += 1;
                }
                return Ok(());
            }
        };
        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            let message = format!("remote authorization failed: {}", response.status());
            for operation in &operations {
                hybrid::mark_outbox_failed(connection, operation, &message, false)?;
                result.failed += 1;
            }
            return Err(message);
        }
        if response.status().as_u16() == 410 {
            result.cursor = "0".to_string();
            bootstrap_project(client, token, config, connection, result)?;
            return Ok(());
        }
        if !response.status().is_success() {
            let message = format!("sync push failed: {}", response.status());
            for operation in &operations {
                hybrid::mark_outbox_failed(connection, operation, &message, true)?;
                result.failed += 1;
            }
            return Ok(());
        }
        let payload = response.json::<PushResponse>().map_err(|err| err.to_string())?;
        for operation_result in payload.results {
            let Some(operation) = operations
                .iter()
                .find(|item| item.id == operation_result.operation_id)
            else {
                continue;
            };
            match operation_result.status.as_str() {
                "applied" | "duplicate" => {
                    hybrid::mark_outbox_applied(
                        connection,
                        operation,
                        operation_result.server_revision,
                        operation_result.remote_payload.as_ref(),
                    )?;
                    result.pushed += 1;
                }
                "conflict" => {
                    hybrid::record_operation_conflict(
                        connection,
                        operation,
                        operation_result.remote_payload.unwrap_or(Value::Null),
                    )?;
                    result.conflicts += 1;
                }
                "retryable" => {
                    hybrid::mark_outbox_failed(
                        connection,
                        operation,
                        operation_result.error.as_deref().unwrap_or("retryable remote error"),
                        true,
                    )?;
                    result.failed += 1;
                }
                _ => {
                    hybrid::mark_outbox_failed(
                        connection,
                        operation,
                        operation_result.error.as_deref().unwrap_or("remote operation rejected"),
                        false,
                    )?;
                    result.failed += 1;
                }
            }
        }
        if operations.len() < 100 {
            return Ok(());
        }
    }
}

fn pull_changes(
    client: &Client,
    token: &str,
    config: &hybrid::RemoteProjectConfig,
    connection: &mut Connection,
    result: &mut SyncRunResult,
) -> Result<(), String> {
    loop {
        let response = client
            .get(format!(
                "{}/api/v1/projects/{}/changes",
                config.server_url, config.remote_project_id
            ))
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .query(&[("cursor", result.cursor.as_str()), ("limit", "500")])
            .send()
            .map_err(|err| err.to_string())?;
        if !response.status().is_success() {
            return Err(format!("sync pull failed: {}", response.status()));
        }
        let payload = response.json::<PullResponse>().map_err(|err| err.to_string())?;
        for change in payload.changes {
            hybrid::apply_remote_change(
                connection,
                &config.project_id,
                &change.entity_type,
                &change.entity_id,
                &change.operation,
                change.revision,
                &change.payload,
            )?;
            result.pulled += 1;
        }
        hybrid::update_pull_cursor(connection, &config.project_id, &payload.next_cursor)?;
        result.cursor = payload.next_cursor;
        if !payload.has_more {
            return Ok(());
        }
    }
}

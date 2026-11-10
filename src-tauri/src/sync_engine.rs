use crate::{credentials, hybrid, storage};
use reqwest::blocking::Client;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

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
        sync_all_once();
        thread::sleep(Duration::from_secs(30));
    });
}

/// Desktop timer and mobile foreground/network triggers share the same durable engine.
pub fn sync_all_once() {
    for manifest in crate::project_fs::list_project_manifests() {
        let paths = crate::project_fs::project_paths(&manifest.id);
        if storage::initialize_project_database(&paths.sqlite).is_err() {
            continue;
        }
        let enabled = Connection::open(&paths.sqlite)
            .ok()
            .and_then(|connection| {
                hybrid::remote_project_config(&connection, &manifest.id)
                    .ok()
                    .flatten()
            })
            .is_some_and(|config| config.auto_sync);
        if enabled {
            let _ = sync_project(&paths.sqlite, &manifest.id, None);
        }
    }
}

// A database lease also protects a project used by another local backend process.
struct SyncLease {
    path: PathBuf,
    project_id: String,
    owner: String,
}
impl SyncLease {
    fn acquire(path: &Path, project_id: &str) -> Result<Self, String> {
        let connection = Connection::open(path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        let now = unix_time();
        let owner = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        let acquired = connection.execute(
            "INSERT INTO sync_run_locks(project_id,owner,expires_at) VALUES(?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET owner=excluded.owner,expires_at=excluded.expires_at WHERE sync_run_locks.expires_at <= ?4",
            params![project_id,owner,now+180,now],
        ).map_err(|error| error.to_string())?;
        if acquired != 1 {
            return Err("此项目正在同步，请稍后重试".into());
        }
        connection
            .execute(
                "UPDATE sync_outbox SET status='pending' WHERE project_id=?1 AND status='syncing'",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        Ok(Self {
            path: path.to_owned(),
            project_id: project_id.into(),
            owner,
        })
    }
    fn renew(&self, connection: &Connection) -> Result<(), String> {
        let changed = connection
            .execute(
                "UPDATE sync_run_locks SET expires_at=?3 WHERE project_id=?1 AND owner=?2",
                params![self.project_id, self.owner, unix_time() + 180],
            )
            .map_err(|error| error.to_string())?;
        if changed != 1 {
            return Err("项目同步租约已失效".into());
        }
        Ok(())
    }
}
impl Drop for SyncLease {
    fn drop(&mut self) {
        if let Ok(connection) = Connection::open(&self.path) {
            let _ = connection.execute(
                "DELETE FROM sync_run_locks WHERE project_id=?1 AND owner=?2",
                params![self.project_id, self.owner],
            );
        }
    }
}
fn unix_time() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[derive(Debug, Deserialize)]
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
    let lease = SyncLease::acquire(database_path, project_id)?;
    let mut connection = Connection::open(database_path).map_err(|error| error.to_string())?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|error| error.to_string())?;
    let config = hybrid::remote_project_config(&connection, project_id)?
        .ok_or("project is not linked to a remote server")?;
    let stored_token = match access_token {
        Some(token) => Some(token.to_owned()),
        None => credentials::read_access_token(project_id)?,
    };
    let token = stored_token
        .or_else(|| std::env::var("IMAGE_ANNOTATION_ACCESS_TOKEN").ok())
        .ok_or("remote access token is unavailable")?;
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|error| error.to_string())?;
    let mut result = SyncRunResult {
        project_id: project_id.into(),
        pushed: 0,
        pulled: 0,
        conflicts: 0,
        failed: 0,
        cursor: hybrid::current_pull_cursor(&connection, project_id)?,
    };
    let mode = connection
        .query_row(
            "SELECT mode FROM projects WHERE id=?1",
            [project_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if mode.as_deref() == Some("local_only") && !config.auto_sync {
        return Ok(result);
    }
    if result.cursor == "0" {
        if let Err(error) =
            bootstrap_project(&client, &token, &config, &mut connection, &mut result)
        {
            // A deleted project still exposes its tombstone to existing members.
            return match pull_changes(
                &client,
                &token,
                &config,
                &mut connection,
                &mut result,
                &lease,
            ) {
                Ok(()) => Ok(result),
                Err(_) => Err(error),
            };
        }
    }
    let pushed = push_outbox(
        &client,
        &token,
        &config,
        &mut connection,
        &mut result,
        &lease,
    );
    let pulled = pull_changes(
        &client,
        &token,
        &config,
        &mut connection,
        &mut result,
        &lease,
    );
    if let Err(error) = pulled {
        return Err(pushed.err().unwrap_or(error));
    }
    // A failed push may be explained by a project tombstone in the successful
    // pull. Retain the local drafts as an explicit conflict, not an opaque 403.

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
            config.server_url.trim_end_matches('/'),
            config.remote_project_id
        ))
        .bearer_auth(token)
        .send()
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("sync bootstrap failed: {}", response.status()));
    }
    let snapshot: Value = response.json().map_err(|error| error.to_string())?;
    let cursor = snapshot
        .get("cursor")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            snapshot
                .get("cursor")
                .and_then(Value::as_i64)
                .map(|v| v.to_string())
        })
        .ok_or("bootstrap cursor is missing")?;
    let mut entities = Vec::new();
    // Dependencies precede their children within the single SQLite transaction.
    for (field, kind) in [
        ("assets", "asset"),
        ("annotations", "annotation"),
        ("issues", "issue"),
        ("issueComments", "issue_comment"),
        ("issueAttachments", "issue_attachment"),
        ("folders", "folder"),
        ("folderMembers", "folder_member"),
        ("tasks", "task"),
        ("taskItems", "task_item"),
        ("qaReviews", "qa_review"),
        ("labelSchemas", "label_schema"),
    ] {
        if let Some(payloads) = snapshot.get(field).and_then(Value::as_array) {
            for payload in payloads {
                let id = if kind == "folder_member" {
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
                        .get("id")
                        .and_then(Value::as_str)
                        .or_else(|| payload.get("clientKey").and_then(Value::as_str))
                        .ok_or_else(|| format!("bootstrap {kind} has no id"))?
                        .to_owned()
                };
                entities.push((
                    kind.into(),
                    id,
                    payload.get("revision").and_then(Value::as_i64),
                    payload.clone(),
                ));
            }
        }
    }
    if let Some(payload) = snapshot.get("labelSchema").filter(|value| !value.is_null()) {
        entities.push((
            "label_schema".into(),
            payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("current")
                .into(),
            payload.get("revision").and_then(Value::as_i64),
            payload.clone(),
        ));
    }
    let count = entities.len();
    hybrid::apply_remote_snapshot(connection, &config.project_id, entities, &cursor)?;
    result.pulled = result.pulled.saturating_add(count as u32);
    result.cursor = cursor;
    Ok(())
}

fn push_outbox(
    client: &Client,
    token: &str,
    config: &hybrid::RemoteProjectConfig,
    connection: &mut Connection,
    result: &mut SyncRunResult,
    lease: &SyncLease,
) -> Result<(), String> {
    // Bound one foreground run; the next trigger continues a very large queue.
    for _ in 0..500 {
        lease.renew(connection)?;
        let operations = hybrid::list_ready_outbox(connection, &config.project_id, 100)?;
        if operations.is_empty() {
            return Ok(());
        }
        let payloads = operations
            .iter()
            .map(|operation| wire_operation(connection, operation))
            .collect::<Result<Vec<_>, _>>()?;
        {
            let tx = connection
                .transaction()
                .map_err(|error| error.to_string())?;
            for operation in &operations {
                tx.execute("UPDATE sync_outbox SET status='syncing',attempted_at=COALESCE(attempted_at,?2) WHERE id=?1",params![operation.id,unix_time().to_string()]).map_err(|error|error.to_string())?;
            }
            tx.commit().map_err(|error| error.to_string())?;
        }
        let response=match client.post(format!("{}/api/v1/sync/push",config.server_url.trim_end_matches('/'))).bearer_auth(token)
            .json(&json!({"deviceId":config.device_id,"projectId":config.remote_project_id,"operations":payloads})).send() {
            Ok(response)=>response,
            Err(error)=>{ for operation in &operations { hybrid::mark_outbox_failed(connection,operation,"网络连接中断",true)?; result.failed+=1; } return Err(format!("sync push connection failed: {error}")); }
        };
        if !response.status().is_success() {
            let retryable = !matches!(response.status().as_u16(), 400 | 401 | 403 | 404 | 422);
            let message = format!("sync push failed: {}", response.status());
            for operation in &operations {
                hybrid::mark_outbox_failed(connection, operation, &message, retryable)?;
                result.failed += 1;
            }
            return Err(message);
        }
        let payload = match response.json::<PushResponse>() {
            Ok(payload) => payload,
            Err(error) => {
                for operation in &operations {
                    hybrid::mark_outbox_failed(connection, operation, "同步响应格式无效", true)?;
                }
                return Err(format!("invalid sync response: {error}"));
            }
        };
        for operation in &operations {
            if !payload
                .results
                .iter()
                .any(|item| item.operation_id == operation.id)
            {
                hybrid::mark_outbox_failed(connection, operation, "服务器未确认该操作", true)?;
                result.failed += 1;
            }
        }
        for item in payload.results {
            let Some(operation) = operations
                .iter()
                .find(|operation| operation.id == item.operation_id)
            else {
                continue;
            };
            match item.status.as_str() {
                "applied" | "duplicate" => {
                    hybrid::mark_outbox_applied(
                        connection,
                        operation,
                        item.server_revision,
                        item.remote_payload.as_ref(),
                    )?;
                    result.pushed += 1;
                }
                "conflict" => {
                    hybrid::record_operation_conflict(
                        connection,
                        operation,
                        item.remote_payload.unwrap_or(Value::Null),
                    )?;
                    result.conflicts += 1;
                }
                other => {
                    hybrid::mark_outbox_failed(
                        connection,
                        operation,
                        item.error.as_deref().unwrap_or("remote operation rejected"),
                        other == "retryable",
                    )?;
                    result.failed += 1;
                }
            }
        }
        // Continue even after a short batch: acknowledgements can release dependent
        // saves/submissions which deliberately were absent from the previous batch.
    }
    Ok(())
}

fn pull_changes(
    client: &Client,
    token: &str,
    config: &hybrid::RemoteProjectConfig,
    connection: &mut Connection,
    result: &mut SyncRunResult,
    lease: &SyncLease,
) -> Result<(), String> {
    loop {
        lease.renew(connection)?;
        let response = client
            .get(format!(
                "{}/api/v1/projects/{}/changes",
                config.server_url.trim_end_matches('/'),
                config.remote_project_id
            ))
            .bearer_auth(token)
            .query(&[("cursor", result.cursor.as_str()), ("limit", "500")])
            .send()
            .map_err(|error| error.to_string())?;
        if response.status().as_u16() == 410 {
            return bootstrap_project(client, token, config, connection, result);
        }
        if !response.status().is_success() {
            return Err(format!("sync pull failed: {}", response.status()));
        }
        let payload: PullResponse = response.json().map_err(|error| error.to_string())?;
        let count = payload.changes.len();
        if payload.has_more && (count == 0 || payload.next_cursor == result.cursor) {
            return Err("服务器同步游标未前进".into());
        }
        hybrid::apply_remote_page(
            connection,
            &config.project_id,
            payload
                .changes
                .into_iter()
                .map(|item| {
                    (
                        item.entity_type,
                        item.entity_id,
                        item.operation,
                        item.revision,
                        item.payload,
                    )
                })
                .collect(),
            &payload.next_cursor,
        )?;
        result.pulled = result.pulled.saturating_add(count as u32);
        result.cursor = payload.next_cursor;
        if !payload.has_more {
            return Ok(());
        }
    }
}

fn wire_operation(
    connection: &Connection,
    operation: &hybrid::OutboxOperation,
) -> Result<Value, String> {
    let mut payload = operation.payload.clone();
    let table = match operation.entity_type.as_str() {
        "annotation" | "qa_review" | "asset" => Some("images"),
        "issue" => Some("issues"),
        "folder" => Some("folders"),
        _ => None,
    };
    let remote_id = if let Some(table) = table {
        remote_identifier(connection, table, &operation.entity_id)?
    } else {
        operation.entity_id.clone()
    };
    if let Some(object) = payload.as_object_mut() {
        object
            .entry("clientKey")
            .or_insert(json!(operation.entity_id));
        for (field, table) in [
            ("imageId", "images"),
            ("issueId", "issues"),
            ("folderId", "folders"),
            ("taskId", "tasks"),
        ] {
            if let Some(local) = object.get(field).and_then(Value::as_str) {
                let remote = remote_identifier(connection, table, local)?;
                object.insert(field.into(), json!(remote));
            }
        }
        if operation.entity_type == "qa_review" {
            object.insert(
                "annotationRevision".into(),
                json!(operation.base_revision.unwrap_or(0)),
            );
        }
    }
    Ok(
        json!({"operationId":operation.id,"entityType":operation.entity_type,"entityId":remote_id,"operation":operation.operation,"baseRevision":operation.base_revision,"payload":payload}),
    )
}
fn remote_identifier(connection: &Connection, table: &str, local: &str) -> Result<String, String> {
    connection
        .query_row(
            &format!("SELECT remote_id FROM {table} WHERE id=?1"),
            [local],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| error.to_string())
        .map(|value| value.flatten().unwrap_or_else(|| local.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_lease_excludes_other_processes_and_recovers_interrupted_outbox() {
        let root = std::env::temp_dir().join(format!(
            "annotation-sync-lease-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("project.sqlite");
        storage::initialize_project_database(&path).unwrap();
        let db = Connection::open(&path).unwrap();
        db.execute("INSERT INTO sync_outbox(id,project_id,entity_type,entity_id,operation,payload_json,status,created_at,updated_at,attempted_at) VALUES('op','p','annotation','image','update','{}','syncing','1','1','1')",[]).unwrap();
        let first = SyncLease::acquire(&path, "p").unwrap();
        assert!(SyncLease::acquire(&path, "p").is_err());
        assert_eq!(
            db.query_row("SELECT status FROM sync_outbox", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "pending"
        );
        assert_eq!(
            db.query_row("SELECT attempted_at FROM sync_outbox", [], |row| row
                .get::<_, String>(0))
                .unwrap(),
            "1"
        );
        drop(first);
        let second = SyncLease::acquire(&path, "p").unwrap();
        drop(second);
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }
}

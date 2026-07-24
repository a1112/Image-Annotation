use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const CURRENT_SCHEMA_VERSION: i64 = 6;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectMode {
    LocalOnly,
    CloudLinked,
    Mirrored,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Synced,
    Pending,
    Syncing,
    Conflict,
    Failed,
    LocalOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueRecord {
    pub id: String,
    pub project_id: String,
    pub image_id: String,
    pub annotation_object_id: Option<String>,
    pub title: String,
    pub description: String,
    pub severity: String,
    pub status: String,
    pub source: String,
    pub reporter_id: String,
    pub assignee_id: Option<String>,
    pub due_at: Option<String>,
    pub revision: i64,
    pub created_at: String,
    pub updated_at: String,
    pub resolved_at: Option<String>,
    pub deleted_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutboxOperation {
    pub id: String,
    pub project_id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub operation: String,
    pub base_revision: Option<i64>,
    pub payload: Value,
    pub status: String,
    pub retry_count: u32,
    pub next_retry_at: Option<String>,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncConflict {
    pub id: String,
    pub project_id: String,
    pub entity_type: String,
    pub entity_id: String,
    pub base: Option<Value>,
    pub local: Value,
    pub remote: Value,
    pub status: String,
    pub created_at: String,
    pub resolved_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncSummary {
    pub project_id: String,
    pub project_mode: String,
    pub pending_operations: u32,
    pub failed_operations: u32,
    pub conflict_count: u32,
    pub last_pulled_at: Option<String>,
    pub last_pushed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueCommentRecord {
    pub id: String,
    pub issue_id: String,
    pub author_id: String,
    pub content: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteProjectConfig {
    pub project_id: String,
    pub server_url: String,
    pub remote_project_id: String,
    pub device_id: String,
    pub cache_policy: String,
    pub auto_sync: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRecord {
    pub id: String,
    pub project_id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub sort_order: i64,
    pub revision: i64,
    pub image_count: u32,
    pub sync_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderMemberRecord {
    pub folder_id: String,
    pub image_id: String,
    pub revision: i64,
    pub sync_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderWorkspace {
    pub folders: Vec<FolderRecord>,
    pub members: Vec<FolderMemberRecord>,
}

pub fn prepare_database(path: &Path) -> Result<(), String> {
    if !path.exists() || fs::metadata(path).map(|meta| meta.len() == 0).unwrap_or(true) {
        return Ok(());
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let version = schema_version(&connection)?;
    drop(connection);
    if version >= CURRENT_SCHEMA_VERSION {
        return Ok(());
    }
    let backup = path.with_extension(format!(
        "sqlite.backup-v{version}-{}",
        now_unix_string()
    ));
    fs::copy(path, backup).map_err(|err| format!("failed to back up project database: {err}"))?;
    Ok(())
}

pub fn migrate_database(connection: &mut Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS schema_migrations (
                version INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                applied_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|err| err.to_string())?;

    let mut version = schema_version(connection)?;
    while version < CURRENT_SCHEMA_VERSION {
        let target = version + 1;
        let transaction = connection.transaction().map_err(|err| err.to_string())?;
        match target {
            1 => migrate_legacy_columns(&transaction)?,
            2 => migrate_hybrid_schema(&transaction)?,
            3 => migrate_legacy_review_issues(&transaction)?,
            4 => migrate_remote_configuration(&transaction)?,
            5 => migrate_remote_entity_ids(&transaction)?,
            6 => migrate_issue_attachments(&transaction)?,
            _ => return Err(format!("unknown schema migration: {target}")),
        }
        transaction
            .execute(
                "INSERT OR REPLACE INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![target, migration_name(target), now_unix_string()],
            )
            .map_err(|err| err.to_string())?;
        transaction
            .pragma_update(None, "user_version", target)
            .map_err(|err| err.to_string())?;
        transaction.commit().map_err(|err| err.to_string())?;
        version = target;
    }
    Ok(())
}

fn migrate_legacy_columns(connection: &Connection) -> Result<(), String> {
    for (table, column, definition) in [
        ("projects", "root_path", "TEXT NOT NULL DEFAULT ''"),
        ("projects", "class_count", "INTEGER NOT NULL DEFAULT 0"),
        ("projects", "image_count", "INTEGER NOT NULL DEFAULT 0"),
        ("projects", "updated_at", "TEXT NOT NULL DEFAULT ''"),
        ("images", "qa_status", "TEXT NOT NULL DEFAULT ''"),
        ("images", "review_note", "TEXT"),
        ("classes", "shortcut", "TEXT"),
        ("classes", "enabled", "INTEGER NOT NULL DEFAULT 1"),
        ("annotations", "revision", "TEXT NOT NULL DEFAULT ''"),
    ] {
        ensure_column(connection, table, column, definition)?;
    }
    Ok(())
}

fn migrate_hybrid_schema(connection: &Connection) -> Result<(), String> {
    for (table, column, definition) in [
        ("projects", "organization_id", "TEXT"),
        ("projects", "description", "TEXT NOT NULL DEFAULT ''"),
        ("projects", "mode", "TEXT NOT NULL DEFAULT 'local_only'"),
        ("projects", "status", "TEXT NOT NULL DEFAULT 'active'"),
        ("projects", "revision", "INTEGER NOT NULL DEFAULT 0"),
        ("projects", "server_revision", "INTEGER"),
        ("projects", "local_revision", "INTEGER NOT NULL DEFAULT 0"),
        ("projects", "sync_status", "TEXT NOT NULL DEFAULT 'local_only'"),
        ("projects", "last_synced_at", "TEXT"),
        ("projects", "sync_error", "TEXT"),
        ("projects", "dirty", "INTEGER NOT NULL DEFAULT 0"),
        ("projects", "created_by", "TEXT NOT NULL DEFAULT 'local-user'"),
        ("projects", "updated_by", "TEXT NOT NULL DEFAULT 'local-user'"),
        ("projects", "deleted_at", "TEXT"),
        ("images", "revision", "INTEGER NOT NULL DEFAULT 0"),
        ("images", "server_revision", "INTEGER"),
        ("images", "local_revision", "INTEGER NOT NULL DEFAULT 0"),
        ("images", "sync_status", "TEXT NOT NULL DEFAULT 'local_only'"),
        ("images", "last_synced_at", "TEXT"),
        ("images", "sync_error", "TEXT"),
        ("images", "dirty", "INTEGER NOT NULL DEFAULT 0"),
        ("images", "deleted_at", "TEXT"),
        ("annotations", "server_revision", "INTEGER"),
        ("annotations", "local_revision", "INTEGER NOT NULL DEFAULT 0"),
        ("annotations", "sync_status", "TEXT NOT NULL DEFAULT 'local_only'"),
        ("annotations", "last_synced_at", "TEXT"),
        ("annotations", "sync_error", "TEXT"),
        ("annotations", "dirty", "INTEGER NOT NULL DEFAULT 0"),
        ("annotations", "deleted_at", "TEXT"),
    ] {
        ensure_column(connection, table, column, definition)?;
    }

    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS project_members (
                project_id TEXT NOT NULL,
                user_id TEXT NOT NULL,
                role TEXT NOT NULL,
                joined_at TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0,
                deleted_at TEXT,
                PRIMARY KEY (project_id, user_id)
            );
            CREATE TABLE IF NOT EXISTS issues (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                image_id TEXT NOT NULL,
                annotation_object_id TEXT,
                title TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                severity TEXT NOT NULL,
                status TEXT NOT NULL,
                source TEXT NOT NULL,
                reporter_id TEXT NOT NULL,
                assignee_id TEXT,
                due_at TEXT,
                revision INTEGER NOT NULL DEFAULT 1,
                server_revision INTEGER,
                local_revision INTEGER NOT NULL DEFAULT 1,
                sync_status TEXT NOT NULL DEFAULT 'local_only',
                sync_error TEXT,
                dirty INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                resolved_at TEXT,
                deleted_at TEXT
            );
            CREATE TABLE IF NOT EXISTS issue_comments (
                id TEXT PRIMARY KEY,
                issue_id TEXT NOT NULL,
                author_id TEXT NOT NULL,
                content TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT,
                FOREIGN KEY(issue_id) REFERENCES issues(id)
            );
            CREATE TABLE IF NOT EXISTS issue_attachments (
                id TEXT PRIMARY KEY,
                issue_id TEXT NOT NULL,
                object_key TEXT NOT NULL,
                file_name TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                created_by TEXT NOT NULL,
                created_at TEXT NOT NULL,
                deleted_at TEXT,
                FOREIGN KEY(issue_id) REFERENCES issues(id)
            );
            CREATE TABLE IF NOT EXISTS issue_events (
                id TEXT PRIMARY KEY,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                before_json TEXT,
                after_json TEXT,
                actor_id TEXT NOT NULL,
                created_at TEXT NOT NULL,
                FOREIGN KEY(issue_id) REFERENCES issues(id)
            );
            CREATE TABLE IF NOT EXISTS sync_outbox (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                operation TEXT NOT NULL,
                base_revision INTEGER,
                payload_json TEXT NOT NULL,
                status TEXT NOT NULL,
                retry_count INTEGER NOT NULL DEFAULT 0,
                next_retry_at TEXT,
                error_message TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_sync_outbox_ready
                ON sync_outbox(project_id, status, next_retry_at, created_at);
            CREATE TABLE IF NOT EXISTS sync_cursors (
                project_id TEXT PRIMARY KEY,
                server_cursor TEXT NOT NULL DEFAULT '',
                last_pulled_at TEXT,
                last_pushed_at TEXT
            );
            CREATE TABLE IF NOT EXISTS sync_conflicts (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                entity_type TEXT NOT NULL,
                entity_id TEXT NOT NULL,
                base_json TEXT,
                local_json TEXT NOT NULL,
                remote_json TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                resolved_at TEXT
            );
            CREATE TABLE IF NOT EXISTS asset_cache (
                asset_id TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                local_path TEXT NOT NULL,
                cache_kind TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                last_accessed_at TEXT NOT NULL,
                verified_at TEXT,
                pinned INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS folders (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                parent_id TEXT,
                name TEXT NOT NULL,
                sort_order INTEGER NOT NULL DEFAULT 0,
                revision INTEGER NOT NULL DEFAULT 1,
                server_revision INTEGER,
                local_revision INTEGER NOT NULL DEFAULT 1,
                sync_status TEXT NOT NULL DEFAULT 'local_only',
                dirty INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT
            );
            CREATE TABLE IF NOT EXISTS folder_members (
                folder_id TEXT NOT NULL,
                image_id TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 1,
                server_revision INTEGER,
                local_revision INTEGER NOT NULL DEFAULT 1,
                sync_status TEXT NOT NULL DEFAULT 'local_only',
                dirty INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                deleted_at TEXT,
                PRIMARY KEY(folder_id, image_id),
                FOREIGN KEY(folder_id) REFERENCES folders(id)
            );
            "#,
        )
        .map_err(|err| err.to_string())
}

fn migrate_legacy_review_issues(connection: &Connection) -> Result<(), String> {
    let project_id = project_id_for_connection(connection, Path::new(""))?;
    let now = now_unix_string();
    connection
        .execute(
            r#"
            INSERT OR IGNORE INTO issues (
                id, project_id, image_id, title, description, severity, status,
                source, reporter_id, revision, local_revision, sync_status,
                dirty, created_at, updated_at
            )
            SELECT 'migration-' || id, ?1, id, '历史质检驳回', review_note,
                'major', 'open', 'migration', 'migration', 1, 1,
                'local_only', 0, ?2, ?2
            FROM images
            WHERE qa_status = '驳回' AND review_note IS NOT NULL AND TRIM(review_note) <> ''
            "#,
            params![project_id, now],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn migrate_remote_configuration(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS remote_project_configs (
                project_id TEXT PRIMARY KEY,
                server_url TEXT NOT NULL,
                remote_project_id TEXT NOT NULL,
                device_id TEXT NOT NULL,
                cache_policy TEXT NOT NULL DEFAULT 'on_demand',
                auto_sync INTEGER NOT NULL DEFAULT 1,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|err| err.to_string())
}

fn migrate_remote_entity_ids(connection: &Connection) -> Result<(), String> {
    for (table, column, definition) in [
        ("projects", "remote_id", "TEXT"),
        ("images", "remote_id", "TEXT"),
        ("images", "content_hash", "TEXT"),
        ("images", "mime_type", "TEXT"),
        ("images", "byte_size", "INTEGER"),
        ("images", "object_key", "TEXT"),
        ("annotations", "remote_id", "TEXT"),
        ("issues", "remote_id", "TEXT"),
        ("folders", "remote_id", "TEXT"),
    ] {
        ensure_column(connection, table, column, definition)?;
    }
    connection
        .execute_batch(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS idx_images_remote_id
                ON images(remote_id) WHERE remote_id IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_remote_id
                ON issues(remote_id) WHERE remote_id IS NOT NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS idx_folders_remote_id
                ON folders(remote_id) WHERE remote_id IS NOT NULL;
            "#,
        )
        .map_err(|err| err.to_string())
}

pub fn mark_annotation_pending(
    transaction: &Transaction<'_>,
    database_path: &Path,
    image_id: &str,
    base_revision: Option<i64>,
    payload: Value,
) -> Result<String, String> {
    transaction
        .execute(
            "UPDATE annotations SET local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, sync_error = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE images SET local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, sync_error = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        transaction,
        &project_id_for_connection(transaction, database_path)?,
        "annotation",
        image_id,
        "update",
        base_revision,
        payload,
    )
}

pub fn mark_submission_pending(
    transaction: &Transaction<'_>,
    database_path: &Path,
    image_id: &str,
) -> Result<String, String> {
    transaction
        .execute(
            "UPDATE images SET local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, sync_error = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        transaction,
        &project_id_for_connection(transaction, database_path)?,
        "annotation",
        image_id,
        "submit",
        None,
        json!({ "imageId": image_id }),
    )
}

pub fn record_review_operation(
    transaction: &Transaction<'_>,
    database_path: &Path,
    image_id: &str,
    decision: &str,
    note: &str,
) -> Result<Option<String>, String> {
    let project_id = project_id_for_connection(transaction, database_path)?;
    transaction
        .execute(
            "UPDATE images SET local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, sync_error = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        transaction,
        &project_id,
        "qa_review",
        image_id,
        "transition",
        None,
        json!({ "imageId": image_id, "decision": decision, "note": note }),
    )?;
    if !matches!(decision, "rejected" | "驳回") {
        return Ok(None);
    }

    let issue_id = unique_id("issue");
    let now = now_unix_string();
    let description = if note.trim().is_empty() {
        "质检驳回，等待修复".to_string()
    } else {
        note.trim().to_string()
    };
    transaction
        .execute(
            r#"
            INSERT INTO issues (
                id, project_id, image_id, title, description, severity, status,
                source, reporter_id, revision, local_revision, sync_status,
                dirty, created_at, updated_at
            ) VALUES (?1, ?2, ?3, '质检驳回', ?4, 'major', 'open', 'qa_review',
                'local-reviewer', 1, 1, 'pending', 1, ?5, ?5)
            "#,
            params![issue_id, project_id, image_id, description, now],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO issue_events (id, issue_id, event_type, after_json, actor_id, created_at) VALUES (?1, ?2, 'created', ?3, 'local-reviewer', ?4)",
            params![
                unique_id("issue-event"),
                issue_id,
                json!({ "status": "open", "severity": "major" }).to_string(),
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        transaction,
        &project_id,
        "issue",
        &issue_id,
        "create",
        None,
        json!({
            "id": issue_id,
            "imageId": image_id,
            "title": "质检驳回",
            "description": description,
            "severity": "major",
            "status": "open",
            "source": "qa_review"
        }),
    )?;
    Ok(Some(issue_id))
}

pub fn enqueue_operation(
    transaction: &Transaction<'_>,
    project_id: &str,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    base_revision: Option<i64>,
    payload: Value,
) -> Result<String, String> {
    let id = unique_id("op");
    let now = now_unix_string();
    transaction
        .execute(
            "INSERT INTO sync_outbox (id, project_id, entity_type, entity_id, operation, base_revision, payload_json, status, retry_count, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 0, ?8, ?8)",
            params![id, project_id, entity_type, entity_id, operation, base_revision, payload.to_string(), now],
        )
        .map_err(|err| err.to_string())?;
    Ok(id)
}

pub fn list_issues(
    connection: &Connection,
    project_id: &str,
    include_closed: bool,
) -> Result<Vec<IssueRecord>, String> {
    let sql = if include_closed {
        "SELECT id, project_id, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id, due_at, revision, created_at, updated_at, resolved_at, deleted_at FROM issues WHERE project_id = ?1 AND deleted_at IS NULL ORDER BY updated_at DESC"
    } else {
        "SELECT id, project_id, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id, due_at, revision, created_at, updated_at, resolved_at, deleted_at FROM issues WHERE project_id = ?1 AND status <> 'closed' AND deleted_at IS NULL ORDER BY updated_at DESC"
    };
    let mut statement = connection.prepare(sql).map_err(|err| err.to_string())?;
    let records = statement
        .query_map(params![project_id], |row| {
            Ok(IssueRecord {
                id: row.get(0)?,
                project_id: row.get(1)?,
                image_id: row.get(2)?,
                annotation_object_id: row.get(3)?,
                title: row.get(4)?,
                description: row.get(5)?,
                severity: row.get(6)?,
                status: row.get(7)?,
                source: row.get(8)?,
                reporter_id: row.get(9)?,
                assignee_id: row.get(10)?,
                due_at: row.get(11)?,
                revision: row.get(12)?,
                created_at: row.get(13)?,
                updated_at: row.get(14)?,
                resolved_at: row.get(15)?,
                deleted_at: row.get(16)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(records)
}

pub fn create_issue(
    connection: &mut Connection,
    project_id: &str,
    image_id: &str,
    annotation_object_id: Option<&str>,
    title: &str,
    description: &str,
    severity: &str,
    assignee_id: Option<&str>,
) -> Result<IssueRecord, String> {
    validate_severity(severity)?;
    if title.trim().is_empty() {
        return Err("issue title is required".to_string());
    }
    let id = unique_id("issue");
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            r#"
            INSERT INTO issues (
                id, project_id, image_id, annotation_object_id, title, description,
                severity, status, source, reporter_id, assignee_id, revision,
                local_revision, sync_status, dirty, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', 'manual',
                'local-user', ?8, 1, 1, 'pending', 1, ?9, ?9)
            "#,
            params![
                id,
                project_id,
                image_id,
                annotation_object_id,
                title.trim(),
                description.trim(),
                severity,
                assignee_id,
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO issue_events (id, issue_id, event_type, after_json, actor_id, created_at) VALUES (?1, ?2, 'created', ?3, 'local-user', ?4)",
            params![
                unique_id("issue-event"),
                id,
                json!({ "status": "open", "severity": severity }).to_string(),
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "issue",
        &id,
        "create",
        None,
        json!({
            "id": id,
            "imageId": image_id,
            "annotationObjectId": annotation_object_id,
            "title": title.trim(),
            "description": description.trim(),
            "severity": severity,
            "status": "open",
            "assigneeId": assignee_id
        }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    get_issue(connection, &id)
}

pub fn transition_issue(
    connection: &mut Connection,
    project_id: &str,
    issue_id: &str,
    next_status: &str,
) -> Result<IssueRecord, String> {
    let issue = get_issue(connection, issue_id)?;
    if issue.project_id != project_id {
        return Err("issue does not belong to project".to_string());
    }
    if !valid_issue_transition(&issue.status, next_status) {
        return Err(format!(
            "invalid issue transition: {} -> {next_status}",
            issue.status
        ));
    }
    let now = now_unix_string();
    let resolved_at = matches!(next_status, "resolved" | "closed").then_some(now.clone());
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE issues SET status = ?2, revision = revision + 1, local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, updated_at = ?3, resolved_at = ?4 WHERE id = ?1",
            params![issue_id, next_status, now, resolved_at],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO issue_events (id, issue_id, event_type, before_json, after_json, actor_id, created_at) VALUES (?1, ?2, 'status_changed', ?3, ?4, 'local-user', ?5)",
            params![
                unique_id("issue-event"),
                issue_id,
                json!({ "status": issue.status }).to_string(),
                json!({ "status": next_status }).to_string(),
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "issue",
        issue_id,
        "transition",
        Some(issue.revision),
        json!({ "status": next_status }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    get_issue(connection, issue_id)
}

pub fn add_issue_comment(
    connection: &mut Connection,
    project_id: &str,
    issue_id: &str,
    content: &str,
) -> Result<IssueCommentRecord, String> {
    let issue = get_issue(connection, issue_id)?;
    if issue.project_id != project_id {
        return Err("issue does not belong to project".to_string());
    }
    if content.trim().is_empty() {
        return Err("comment content is required".to_string());
    }
    let id = unique_id("issue-comment");
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO issue_comments (id, issue_id, author_id, content, revision, created_at, updated_at) VALUES (?1, ?2, 'local-user', ?3, 1, ?4, ?4)",
            params![id, issue_id, content.trim(), now],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO issue_events (id, issue_id, event_type, after_json, actor_id, created_at) VALUES (?1, ?2, 'commented', ?3, 'local-user', ?4)",
            params![
                unique_id("issue-event"),
                issue_id,
                json!({ "commentId": id }).to_string(),
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "issue_comment",
        &id,
        "comment",
        None,
        json!({ "issueId": issue_id, "content": content.trim() }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(IssueCommentRecord {
        id,
        issue_id: issue_id.to_string(),
        author_id: "local-user".to_string(),
        content: content.trim().to_string(),
        created_at: now.clone(),
        updated_at: now,
    })
}

pub fn list_issue_comments(
    connection: &Connection,
    issue_id: &str,
) -> Result<Vec<IssueCommentRecord>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, issue_id, author_id, content, created_at, updated_at FROM issue_comments WHERE issue_id = ?1 AND deleted_at IS NULL ORDER BY created_at",
        )
        .map_err(|err| err.to_string())?;
    let records = statement
        .query_map(params![issue_id], |row| {
            Ok(IssueCommentRecord {
                id: row.get(0)?,
                issue_id: row.get(1)?,
                author_id: row.get(2)?,
                content: row.get(3)?,
                created_at: row.get(4)?,
                updated_at: row.get(5)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(records)
}

pub fn list_ready_outbox(
    connection: &Connection,
    project_id: &str,
    limit: u32,
) -> Result<Vec<OutboxOperation>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, entity_type, entity_id, operation, base_revision, payload_json, status, retry_count, next_retry_at, error_message, created_at, updated_at FROM sync_outbox WHERE project_id = ?1 AND (status = 'pending' OR (status = 'failed' AND next_retry_at IS NOT NULL AND next_retry_at <= ?2)) ORDER BY created_at LIMIT ?3",
        )
        .map_err(|err| err.to_string())?;
    let records = statement
        .query_map(
            params![project_id, now_unix_string(), limit.clamp(1, 500)],
            |row| {
                let payload_json: String = row.get(6)?;
                Ok(OutboxOperation {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    entity_type: row.get(2)?,
                    entity_id: row.get(3)?,
                    operation: row.get(4)?,
                    base_revision: row.get(5)?,
                    payload: serde_json::from_str(&payload_json).unwrap_or(Value::Null),
                    status: row.get(7)?,
                    retry_count: row.get(8)?,
                    next_retry_at: row.get(9)?,
                    error_message: row.get(10)?,
                    created_at: row.get(11)?,
                    updated_at: row.get(12)?,
                })
            },
        )
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(records)
}

pub fn configure_remote_project(
    connection: &mut Connection,
    project_id: &str,
    server_url: &str,
    remote_project_id: &str,
    device_id: &str,
    mode: &str,
    cache_policy: &str,
    auto_sync: bool,
) -> Result<RemoteProjectConfig, String> {
    validate_project_mode(mode)?;
    validate_cache_policy(cache_policy)?;
    if !server_url.starts_with("http://") && !server_url.starts_with("https://") {
        return Err("server URL must use http or https".to_string());
    }
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE projects SET mode = ?2, sync_status = 'pending', dirty = 1, local_revision = local_revision + 1, updated_at = ?3 WHERE id = ?1",
            params![project_id, mode, now],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            r#"
            INSERT INTO remote_project_configs (
                project_id, server_url, remote_project_id, device_id,
                cache_policy, auto_sync, created_at, updated_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
            ON CONFLICT(project_id) DO UPDATE SET
                server_url = excluded.server_url,
                remote_project_id = excluded.remote_project_id,
                device_id = excluded.device_id,
                cache_policy = excluded.cache_policy,
                auto_sync = excluded.auto_sync,
                updated_at = excluded.updated_at
            "#,
            params![
                project_id,
                server_url.trim_end_matches('/'),
                remote_project_id,
                device_id,
                cache_policy,
                auto_sync as i64,
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "project",
        project_id,
        "update",
        None,
        json!({ "mode": mode, "cachePolicy": cache_policy }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    remote_project_config(connection, project_id)?
        .ok_or_else(|| "remote project configuration was not saved".to_string())
}

pub fn folder_workspace(
    connection: &mut Connection,
    project_id: &str,
) -> Result<FolderWorkspace, String> {
    seed_split_folders(connection, project_id)?;
    read_folder_workspace(connection, project_id)
}

pub fn migrate_legacy_folders(
    connection: &mut Connection,
    project_id: &str,
    names: &[String],
    assignments: &HashMap<String, String>,
) -> Result<FolderWorkspace, String> {
    seed_split_folders(connection, project_id)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    let mut folder_ids = HashMap::new();
    for name in names {
        if name.trim().is_empty() {
            continue;
        }
        let folder_id = ensure_named_folder(&transaction, project_id, name.trim(), true)?;
        folder_ids.insert(name.clone(), folder_id);
    }
    for (image_id, folder_name) in assignments {
        if folder_name.trim().is_empty() {
            continue;
        }
        let folder_id = if let Some(id) = folder_ids.get(folder_name) {
            id.clone()
        } else {
            let id = ensure_named_folder(&transaction, project_id, folder_name.trim(), true)?;
            folder_ids.insert(folder_name.clone(), id.clone());
            id
        };
        move_image_to_folder_in_transaction(
            &transaction,
            project_id,
            image_id,
            &folder_id,
            true,
        )?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    read_folder_workspace(connection, project_id)
}

pub fn create_folder(
    connection: &mut Connection,
    project_id: &str,
    name: &str,
    parent_id: Option<&str>,
) -> Result<FolderWorkspace, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("folder name is required".to_string());
    }
    let duplicate = connection
        .query_row(
            "SELECT COUNT(*) FROM folders WHERE project_id = ?1 AND name = ?2 AND deleted_at IS NULL",
            params![project_id, name],
            |row| row.get::<_, u32>(0),
        )
        .map_err(|err| err.to_string())?;
    if duplicate > 0 {
        return Err(format!("folder already exists: {name}"));
    }
    let id = unique_id("folder");
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO folders (id, project_id, parent_id, name, sort_order, revision, local_revision, sync_status, dirty, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, 0, 1, 1, 'pending', 1, ?5, ?5)",
            params![id, project_id, parent_id, name, now],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "folder",
        &id,
        "create",
        None,
        json!({ "id": id, "parentId": parent_id, "name": name, "sortOrder": 0 }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    read_folder_workspace(connection, project_id)
}

pub fn rename_folder(
    connection: &mut Connection,
    project_id: &str,
    folder_id: &str,
    name: &str,
) -> Result<FolderWorkspace, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("folder name is required".to_string());
    }
    let revision = connection
        .query_row(
            "SELECT revision FROM folders WHERE id = ?1 AND project_id = ?2 AND deleted_at IS NULL",
            params![folder_id, project_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("folder not found: {folder_id}"))?;
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE folders SET name = ?2, revision = revision + 1, local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, updated_at = ?3 WHERE id = ?1",
            params![folder_id, name, now],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "folder",
        folder_id,
        "update",
        Some(revision),
        json!({ "name": name }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    read_folder_workspace(connection, project_id)
}

pub fn delete_folder(
    connection: &mut Connection,
    project_id: &str,
    folder_id: &str,
) -> Result<FolderWorkspace, String> {
    let revision = connection
        .query_row(
            "SELECT revision FROM folders WHERE id = ?1 AND project_id = ?2 AND deleted_at IS NULL",
            params![folder_id, project_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("folder not found: {folder_id}"))?;
    let ungrouped_id = ensure_named_folder(connection, project_id, "未分组", false)?;
    let image_ids = {
        let mut statement = connection
            .prepare(
                "SELECT image_id FROM folder_members WHERE folder_id = ?1 AND deleted_at IS NULL",
            )
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![folder_id], |row| row.get::<_, String>(0))
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        rows
    };
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    for image_id in image_ids {
        move_image_to_folder_in_transaction(
            &transaction,
            project_id,
            &image_id,
            &ungrouped_id,
            true,
        )?;
    }
    transaction
        .execute(
            "UPDATE folders SET deleted_at = ?2, revision = revision + 1, local_revision = local_revision + 1, sync_status = 'pending', dirty = 1, updated_at = ?2 WHERE id = ?1",
            params![folder_id, now_unix_string()],
        )
        .map_err(|err| err.to_string())?;
    enqueue_operation(
        &transaction,
        project_id,
        "folder",
        folder_id,
        "delete",
        Some(revision),
        json!({ "id": folder_id }),
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    read_folder_workspace(connection, project_id)
}

pub fn move_image_to_folder(
    connection: &mut Connection,
    project_id: &str,
    image_id: &str,
    folder_id: &str,
) -> Result<FolderWorkspace, String> {
    let folder_exists = connection
        .query_row(
            "SELECT COUNT(*) FROM folders WHERE id = ?1 AND project_id = ?2 AND deleted_at IS NULL",
            params![folder_id, project_id],
            |row| row.get::<_, u32>(0),
        )
        .map_err(|err| err.to_string())?;
    if folder_exists == 0 {
        return Err(format!("folder not found: {folder_id}"));
    }
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    move_image_to_folder_in_transaction(
        &transaction,
        project_id,
        image_id,
        folder_id,
        true,
    )?;
    transaction.commit().map_err(|err| err.to_string())?;
    read_folder_workspace(connection, project_id)
}

pub fn remote_project_config(
    connection: &Connection,
    project_id: &str,
) -> Result<Option<RemoteProjectConfig>, String> {
    connection
        .query_row(
            "SELECT project_id, server_url, remote_project_id, device_id, cache_policy, auto_sync FROM remote_project_configs WHERE project_id = ?1",
            params![project_id],
            |row| {
                Ok(RemoteProjectConfig {
                    project_id: row.get(0)?,
                    server_url: row.get(1)?,
                    remote_project_id: row.get(2)?,
                    device_id: row.get(3)?,
                    cache_policy: row.get(4)?,
                    auto_sync: row.get::<_, i64>(5)? != 0,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
}

pub fn mark_outbox_applied(
    connection: &mut Connection,
    operation: &OutboxOperation,
    server_revision: Option<i64>,
    remote_payload: Option<&Value>,
) -> Result<(), String> {
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE sync_outbox SET status = 'synced', error_message = NULL, next_retry_at = NULL, updated_at = ?2 WHERE id = ?1",
            params![operation.id, now],
        )
        .map_err(|err| err.to_string())?;
    match operation.entity_type.as_str() {
        "asset" => {
            let remote_id = remote_payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str);
            transaction
                .execute(
                    "UPDATE images SET remote_id = COALESCE(?2, remote_id), server_revision = ?3, sync_status = 'synced', dirty = 0, last_synced_at = ?4, sync_error = NULL WHERE id = ?1",
                    params![operation.entity_id, remote_id, server_revision, now],
                )
                .map_err(|err| err.to_string())?;
        }
        "annotation" => {
            let remote_id = remote_payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str);
            let remote_image_id = remote_payload
                .and_then(|payload| payload.get("imageId"))
                .and_then(Value::as_str);
            transaction
                .execute(
                    "UPDATE annotations SET remote_id = COALESCE(?2, remote_id), server_revision = ?3, sync_status = 'synced', dirty = 0, last_synced_at = ?4, sync_error = NULL WHERE id = ?1",
                    params![operation.entity_id, remote_id, server_revision, now],
                )
                .map_err(|err| err.to_string())?;
            transaction
                .execute(
                    "UPDATE images SET remote_id = COALESCE(?2, remote_id), server_revision = COALESCE(?3, server_revision), sync_status = 'synced', dirty = 0, last_synced_at = ?4, sync_error = NULL WHERE id = ?1",
                    params![operation.entity_id, remote_image_id, server_revision, now],
                )
                .map_err(|err| err.to_string())?;
        }
        "issue" => {
            let remote_id = remote_payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str);
            transaction
                .execute(
                    "UPDATE issues SET remote_id = COALESCE(?2, remote_id), server_revision = ?3, sync_status = 'synced', dirty = 0, sync_error = NULL WHERE id = ?1",
                    params![operation.entity_id, remote_id, server_revision],
                )
                .map_err(|err| err.to_string())?;
        }
        "folder" => {
            let remote_id = remote_payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str);
            transaction
                .execute(
                    "UPDATE folders SET remote_id = COALESCE(?2, remote_id), server_revision = ?3, sync_status = 'synced', dirty = 0 WHERE id = ?1",
                    params![operation.entity_id, remote_id, server_revision],
                )
                .map_err(|err| err.to_string())?;
        }
        "issue_attachment" => {
            let remote_id = remote_payload
                .and_then(|payload| payload.get("id"))
                .and_then(Value::as_str)
                ;
            transaction
                .execute(
                    "UPDATE issue_attachments SET remote_id = COALESCE(?2, remote_id), upload_state = 'ready', revision = COALESCE(?3, revision), sync_state = 'clean', updated_at = ?4 WHERE id = ?1",
                    params![operation.entity_id, remote_id, server_revision, now],
                )
                .map_err(|err| err.to_string())?;
        }
        _ => {}
    }
    transaction
        .execute(
            "INSERT INTO sync_cursors (project_id, server_cursor, last_pushed_at) VALUES (?1, '', ?2) ON CONFLICT(project_id) DO UPDATE SET last_pushed_at = excluded.last_pushed_at",
            params![operation.project_id, now],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())
}

pub fn mark_outbox_failed(
    connection: &Connection,
    operation: &OutboxOperation,
    error_message: &str,
    retryable: bool,
) -> Result<(), String> {
    let retry_count = operation.retry_count + 1;
    let next_retry_at = retryable.then(|| {
        let delay = match retry_count {
            1 => 0,
            2 => 2,
            3 => 5,
            4 => 15,
            value => (2_u64.pow(value.min(12) - 4) * 15).min(300),
        };
        (now_unix() + delay).to_string()
    });
    connection
        .execute(
            "UPDATE sync_outbox SET status = ?2, retry_count = ?3, next_retry_at = ?4, error_message = ?5, updated_at = ?6 WHERE id = ?1",
            params![
                operation.id,
                if retryable { "failed" } else { "rejected" },
                retry_count,
                next_retry_at,
                error_message,
                now_unix_string()
            ],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn record_operation_conflict(
    connection: &mut Connection,
    operation: &OutboxOperation,
    remote: Value,
) -> Result<SyncConflict, String> {
    let id = format!("conflict-{}", operation.id);
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT OR REPLACE INTO sync_conflicts (id, project_id, entity_type, entity_id, base_json, local_json, remote_json, status, created_at, resolved_at) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 'open', ?7, NULL)",
            params![
                id,
                operation.project_id,
                operation.entity_type,
                operation.entity_id,
                operation.payload.to_string(),
                remote.to_string(),
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE sync_outbox SET status = 'conflict', error_message = 'remote revision conflict', updated_at = ?2 WHERE id = ?1",
            params![operation.id, now],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(SyncConflict {
        id,
        project_id: operation.project_id.clone(),
        entity_type: operation.entity_type.clone(),
        entity_id: operation.entity_id.clone(),
        base: None,
        local: operation.payload.clone(),
        remote,
        status: "open".to_string(),
        created_at: now,
        resolved_at: None,
    })
}

pub fn list_conflicts(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<SyncConflict>, String> {
    let mut statement = connection
        .prepare(
            "SELECT id, project_id, entity_type, entity_id, base_json, local_json, remote_json, status, created_at, resolved_at FROM sync_conflicts WHERE project_id = ?1 ORDER BY created_at DESC",
        )
        .map_err(|err| err.to_string())?;
    let records = statement
        .query_map(params![project_id], |row| {
            let base_json: Option<String> = row.get(4)?;
            let local_json: String = row.get(5)?;
            let remote_json: String = row.get(6)?;
            Ok(SyncConflict {
                id: row.get(0)?,
                project_id: row.get(1)?,
                entity_type: row.get(2)?,
                entity_id: row.get(3)?,
                base: base_json.and_then(|value| serde_json::from_str(&value).ok()),
                local: serde_json::from_str(&local_json).unwrap_or(Value::Null),
                remote: serde_json::from_str(&remote_json).unwrap_or(Value::Null),
                status: row.get(7)?,
                created_at: row.get(8)?,
                resolved_at: row.get(9)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(records)
}

pub fn resolve_conflict(
    connection: &mut Connection,
    project_id: &str,
    conflict_id: &str,
    resolution: &str,
) -> Result<(), String> {
    if !matches!(resolution, "local" | "remote") {
        return Err("conflict resolution must be local or remote".to_string());
    }
    let conflict = list_conflicts(connection, project_id)?
        .into_iter()
        .find(|item| item.id == conflict_id)
        .ok_or_else(|| format!("conflict not found: {conflict_id}"))?;
    if conflict.status != "open" {
        return Err("conflict has already been resolved".to_string());
    }
    let now = now_unix_string();
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    if resolution == "local" {
        enqueue_operation(
            &transaction,
            project_id,
            &conflict.entity_type,
            &conflict.entity_id,
            "update",
            None,
            conflict.local.clone(),
        )?;
    } else {
        apply_remote_payload(
            &transaction,
            project_id,
            &conflict.entity_type,
            &conflict.entity_id,
            None,
            &conflict.remote,
        )?;
    }
    transaction
        .execute(
            "UPDATE sync_conflicts SET status = 'resolved', resolved_at = ?2 WHERE id = ?1",
            params![conflict_id, now],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())
}

pub fn apply_remote_change(
    connection: &mut Connection,
    project_id: &str,
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    revision: Option<i64>,
    payload: &Value,
) -> Result<(), String> {
    let dirty = entity_dirty(connection, entity_type, entity_id)?;
    if dirty {
        let id = unique_id("conflict");
        connection
            .execute(
                "INSERT INTO sync_conflicts (id, project_id, entity_type, entity_id, base_json, local_json, remote_json, status, created_at) VALUES (?1, ?2, ?3, ?4, NULL, '{}', ?5, 'open', ?6)",
                params![id, project_id, entity_type, entity_id, payload.to_string(), now_unix_string()],
            )
            .map_err(|err| err.to_string())?;
        return Ok(());
    }
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    if operation == "delete" {
        mark_entity_deleted(&transaction, entity_type, entity_id)?;
    } else {
        apply_remote_payload(
            &transaction,
            project_id,
            entity_type,
            entity_id,
            revision,
            payload,
        )?;
    }
    transaction.commit().map_err(|err| err.to_string())
}

pub fn apply_remote_snapshot(
    connection: &mut Connection,
    project_id: &str,
    entities: Vec<(String, String, Option<i64>, Value)>,
    cursor: &str,
) -> Result<(), String> {
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    for (entity_type, entity_id, revision, payload) in entities {
        if entity_dirty(&transaction, &entity_type, &entity_id)? {
            transaction
                .execute(
                    "INSERT INTO sync_conflicts (id, project_id, entity_type, entity_id, base_json, local_json, remote_json, status, created_at) VALUES (?1, ?2, ?3, ?4, NULL, '{}', ?5, 'open', ?6)",
                    params![
                        unique_id("conflict"),
                        project_id,
                        entity_type,
                        entity_id,
                        payload.to_string(),
                        now_unix_string()
                    ],
                )
                .map_err(|err| err.to_string())?;
            continue;
        }
        apply_remote_payload(
            &transaction,
            project_id,
            &entity_type,
            &entity_id,
            revision,
            &payload,
        )?;
    }
    transaction
        .execute(
            "INSERT INTO sync_cursors (project_id, server_cursor, last_pulled_at) VALUES (?1, ?2, ?3) ON CONFLICT(project_id) DO UPDATE SET server_cursor = excluded.server_cursor, last_pulled_at = excluded.last_pulled_at",
            params![project_id, cursor, now_unix_string()],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())
}

pub fn update_pull_cursor(
    connection: &Connection,
    project_id: &str,
    cursor: &str,
) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO sync_cursors (project_id, server_cursor, last_pulled_at) VALUES (?1, ?2, ?3) ON CONFLICT(project_id) DO UPDATE SET server_cursor = excluded.server_cursor, last_pulled_at = excluded.last_pulled_at",
            params![project_id, cursor, now_unix_string()],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn current_pull_cursor(
    connection: &Connection,
    project_id: &str,
) -> Result<String, String> {
    connection
        .query_row(
            "SELECT server_cursor FROM sync_cursors WHERE project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())
        .map(|value| value.unwrap_or_else(|| "0".to_string()))
}

pub fn sync_summary(connection: &Connection, project_id: &str) -> Result<SyncSummary, String> {
    let project_mode = connection
        .query_row(
            "SELECT mode FROM projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())?
        .unwrap_or_else(|| "local_only".to_string());
    let cursor = connection
        .query_row(
            "SELECT last_pulled_at, last_pushed_at FROM sync_cursors WHERE project_id = ?1",
            params![project_id],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()
        .map_err(|err| err.to_string())?;
    Ok(SyncSummary {
        project_id: project_id.to_string(),
        project_mode,
        pending_operations: count_rows(
            connection,
            "sync_outbox",
            "project_id = ?1 AND status IN ('pending', 'syncing')",
            project_id,
        )?,
        failed_operations: count_rows(
            connection,
            "sync_outbox",
            "project_id = ?1 AND status = 'failed'",
            project_id,
        )?,
        conflict_count: count_rows(
            connection,
            "sync_conflicts",
            "project_id = ?1 AND status = 'open'",
            project_id,
        )?,
        last_pulled_at: cursor.as_ref().and_then(|value| value.0.clone()),
        last_pushed_at: cursor.and_then(|value| value.1),
    })
}

fn count_rows(
    connection: &Connection,
    table: &str,
    predicate: &str,
    project_id: &str,
) -> Result<u32, String> {
    connection
        .query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())
}

fn get_issue(connection: &Connection, issue_id: &str) -> Result<IssueRecord, String> {
    connection
        .query_row(
            "SELECT id, project_id, image_id, annotation_object_id, title, description, severity, status, source, reporter_id, assignee_id, due_at, revision, created_at, updated_at, resolved_at, deleted_at FROM issues WHERE id = ?1 AND deleted_at IS NULL",
            params![issue_id],
            |row| {
                Ok(IssueRecord {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    image_id: row.get(2)?,
                    annotation_object_id: row.get(3)?,
                    title: row.get(4)?,
                    description: row.get(5)?,
                    severity: row.get(6)?,
                    status: row.get(7)?,
                    source: row.get(8)?,
                    reporter_id: row.get(9)?,
                    assignee_id: row.get(10)?,
                    due_at: row.get(11)?,
                    revision: row.get(12)?,
                    created_at: row.get(13)?,
                    updated_at: row.get(14)?,
                    resolved_at: row.get(15)?,
                    deleted_at: row.get(16)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())?
        .ok_or_else(|| format!("issue not found: {issue_id}"))
}

fn seed_split_folders(connection: &mut Connection, project_id: &str) -> Result<(), String> {
    let rows = {
        let mut statement = connection
            .prepare("SELECT id, split FROM images WHERE deleted_at IS NULL ORDER BY id")
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        rows
    };
    if rows.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    let mut folders: HashMap<String, String> = HashMap::new();
    for (image_id, split) in rows {
        let name = if split.trim().is_empty() { "未分组" } else { split.trim() };
        let folder_id = if let Some(id) = folders.get(name) {
            id.clone()
        } else {
            let id = ensure_named_folder(&transaction, project_id, name, false)?;
            folders.insert(name.to_string(), id.clone());
            id
        };
        transaction
            .execute(
                "INSERT OR IGNORE INTO folder_members (folder_id, image_id, revision, local_revision, sync_status, dirty, created_at) VALUES (?1, ?2, 1, 0, 'local_only', 0, ?3)",
                params![folder_id, image_id, now_unix_string()],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())
}

fn ensure_named_folder(
    connection: &Connection,
    project_id: &str,
    name: &str,
    pending: bool,
) -> Result<String, String> {
    let existing = connection
        .query_row(
            "SELECT id FROM folders WHERE project_id = ?1 AND name = ?2 AND deleted_at IS NULL LIMIT 1",
            params![project_id, name],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|err| err.to_string())?;
    if let Some(id) = existing {
        return Ok(id);
    }
    let id = unique_id("folder");
    let now = now_unix_string();
    connection
        .execute(
            "INSERT INTO folders (id, project_id, name, sort_order, revision, local_revision, sync_status, dirty, created_at, updated_at) VALUES (?1, ?2, ?3, 0, 1, ?4, ?5, ?6, ?7, ?7)",
            params![
                id,
                project_id,
                name,
                if pending { 1 } else { 0 },
                if pending { "pending" } else { "local_only" },
                if pending { 1 } else { 0 },
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    if pending {
        let operation_id = unique_id("op");
        connection
            .execute(
                "INSERT INTO sync_outbox (id, project_id, entity_type, entity_id, operation, payload_json, status, retry_count, created_at, updated_at) VALUES (?1, ?2, 'folder', ?3, 'create', ?4, 'pending', 0, ?5, ?5)",
                params![operation_id, project_id, id, json!({ "id": id, "name": name }).to_string(), now],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(id)
}

fn move_image_to_folder_in_transaction(
    transaction: &Transaction<'_>,
    project_id: &str,
    image_id: &str,
    folder_id: &str,
    pending: bool,
) -> Result<(), String> {
    let now = now_unix_string();
    let previous = {
        let mut statement = transaction
            .prepare(
                "SELECT folder_id FROM folder_members WHERE image_id = ?1 AND deleted_at IS NULL AND folder_id <> ?2",
            )
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![image_id, folder_id], |row| row.get::<_, String>(0))
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        rows
    };
    for previous_folder_id in previous {
        transaction
            .execute(
                "UPDATE folder_members SET deleted_at = ?3, revision = revision + 1, local_revision = local_revision + 1, sync_status = ?4, dirty = ?5 WHERE folder_id = ?1 AND image_id = ?2",
                params![
                    previous_folder_id,
                    image_id,
                    now,
                    if pending { "pending" } else { "local_only" },
                    if pending { 1 } else { 0 }
                ],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction
        .execute(
            r#"
            INSERT INTO folder_members (
                folder_id, image_id, revision, local_revision, sync_status,
                dirty, created_at, deleted_at
            ) VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, NULL)
            ON CONFLICT(folder_id, image_id) DO UPDATE SET
                revision = folder_members.revision + 1,
                local_revision = folder_members.local_revision + excluded.local_revision,
                sync_status = excluded.sync_status,
                dirty = excluded.dirty,
                deleted_at = NULL
            "#,
            params![
                folder_id,
                image_id,
                if pending { 1 } else { 0 },
                if pending { "pending" } else { "local_only" },
                if pending { 1 } else { 0 },
                now
            ],
        )
        .map_err(|err| err.to_string())?;
    if pending {
        enqueue_operation(
            transaction,
            project_id,
            "folder_member",
            &format!("{folder_id}:{image_id}"),
            "update",
            None,
            json!({ "folderId": folder_id, "imageId": image_id }),
        )?;
    }
    Ok(())
}

fn read_folder_workspace(
    connection: &Connection,
    project_id: &str,
) -> Result<FolderWorkspace, String> {
    let folders = {
        let mut statement = connection
            .prepare(
                r#"
                SELECT f.id, f.project_id, f.parent_id, f.name, f.sort_order,
                    f.revision, f.sync_status, COUNT(fm.image_id)
                FROM folders f
                LEFT JOIN folder_members fm
                    ON fm.folder_id = f.id AND fm.deleted_at IS NULL
                WHERE f.project_id = ?1 AND f.deleted_at IS NULL
                GROUP BY f.id
                ORDER BY f.sort_order, f.name
                "#,
            )
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![project_id], |row| {
                Ok(FolderRecord {
                    id: row.get(0)?,
                    project_id: row.get(1)?,
                    parent_id: row.get(2)?,
                    name: row.get(3)?,
                    sort_order: row.get(4)?,
                    revision: row.get(5)?,
                    sync_status: row.get(6)?,
                    image_count: row.get(7)?,
                })
            })
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        rows
    };
    let members = {
        let mut statement = connection
            .prepare(
                "SELECT fm.folder_id, fm.image_id, fm.revision, fm.sync_status FROM folder_members fm JOIN folders f ON f.id = fm.folder_id WHERE f.project_id = ?1 AND f.deleted_at IS NULL AND fm.deleted_at IS NULL",
            )
            .map_err(|err| err.to_string())?;
        let rows = statement
            .query_map(params![project_id], |row| {
                Ok(FolderMemberRecord {
                    folder_id: row.get(0)?,
                    image_id: row.get(1)?,
                    revision: row.get(2)?,
                    sync_status: row.get(3)?,
                })
            })
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?;
        rows
    };
    Ok(FolderWorkspace { folders, members })
}

fn entity_dirty(
    connection: &Connection,
    entity_type: &str,
    entity_id: &str,
) -> Result<bool, String> {
    if entity_type == "issue_attachment" {
        return connection
            .query_row(
                "SELECT sync_state FROM issue_attachments WHERE id = ?1",
                params![entity_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())
            .map(|state| {
                state.is_some_and(|value| {
                    matches!(value.as_str(), "dirty" | "pending" | "failed" | "conflict")
                })
            });
    }
    let table = match entity_type {
        "asset" => "images",
        "annotation" => "annotations",
        "issue" => "issues",
        "folder" => "folders",
        _ => return Ok(false),
    };
    connection
        .query_row(
            &format!("SELECT dirty FROM {table} WHERE id = ?1"),
            params![entity_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|err| err.to_string())
        .map(|value| value.unwrap_or(0) != 0)
}

fn apply_remote_payload(
    connection: &Connection,
    project_id: &str,
    entity_type: &str,
    entity_id: &str,
    revision: Option<i64>,
    payload: &Value,
) -> Result<(), String> {
    match entity_type {
        "asset" => {
            let remote_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(entity_id);
            let local_id = payload
                .get("clientKey")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    connection
                        .query_row(
                            "SELECT id FROM images WHERE remote_id = ?1",
                            params![remote_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| entity_id.to_string());
            connection
                .execute(
                    r#"
                    INSERT INTO images (
                        id, file_name, width, height, split, status, remote_id,
                        content_hash, mime_type, byte_size, object_key, revision,
                        server_revision, local_revision, sync_status, dirty
                    ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, '未标注', ?6,
                        ?7, ?8, ?9, ?10, ?11, ?11, 0, 'synced', 0
                    )
                    ON CONFLICT(id) DO UPDATE SET
                        file_name = excluded.file_name,
                        width = excluded.width,
                        height = excluded.height,
                        split = excluded.split,
                        remote_id = excluded.remote_id,
                        content_hash = excluded.content_hash,
                        mime_type = excluded.mime_type,
                        byte_size = excluded.byte_size,
                        object_key = excluded.object_key,
                        revision = excluded.revision,
                        server_revision = excluded.server_revision,
                        sync_status = 'synced',
                        dirty = 0
                    "#,
                    params![
                        local_id,
                        payload
                            .get("fileName")
                            .and_then(Value::as_str)
                            .unwrap_or("remote-image"),
                        payload.get("width").and_then(Value::as_i64).unwrap_or(1),
                        payload.get("height").and_then(Value::as_i64).unwrap_or(1),
                        payload
                            .get("split")
                            .and_then(Value::as_str)
                            .unwrap_or("train"),
                        remote_id,
                        payload
                            .get("contentHash")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        payload
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("application/octet-stream"),
                        payload
                            .get("byteSize")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                        payload
                            .get("objectKey")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        revision.unwrap_or(1)
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        "annotation" => {
            let object_json = payload
                .get("objects")
                .cloned()
                .unwrap_or_else(|| payload.clone())
                .to_string();
            let remote_annotation_id = payload.get("id").and_then(Value::as_str);
            let remote_image_id = payload.get("imageId").and_then(Value::as_str);
            let local_image_id = remote_image_id
                .and_then(|remote_id| {
                    connection
                        .query_row(
                            "SELECT id FROM images WHERE remote_id = ?1",
                            params![remote_id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| entity_id.to_string());
            connection
                .execute(
                    "INSERT INTO annotations (id, image_id, remote_id, revision, object_json, updated_at, server_revision, sync_status, dirty) VALUES (?1, ?1, ?2, ?3, ?4, ?5, ?6, 'synced', 0) ON CONFLICT(id) DO UPDATE SET remote_id = COALESCE(excluded.remote_id, annotations.remote_id), revision = excluded.revision, object_json = excluded.object_json, updated_at = excluded.updated_at, server_revision = excluded.server_revision, sync_status = 'synced', dirty = 0",
                    params![
                        local_image_id,
                        remote_annotation_id,
                        format!("server-{}", revision.unwrap_or_default()),
                        object_json,
                        now_unix_string(),
                        revision
                    ],
                )
                .map_err(|err| err.to_string())?;
        }
        "issue" => {
            let remote_id = payload.get("id").and_then(Value::as_str);
            let local_issue_id = remote_id
                .and_then(|id| {
                    connection
                        .query_row(
                            "SELECT id FROM issues WHERE remote_id = ?1",
                            params![id],
                            |row| row.get::<_, String>(0),
                        )
                        .optional()
                        .ok()
                        .flatten()
                })
                .unwrap_or_else(|| entity_id.to_string());
            let remote_image_id = payload.get("imageId").and_then(Value::as_str);
            let local_image_id = payload
                .get("clientImageId")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| {
                    remote_image_id.and_then(|remote_id| {
                        connection
                            .query_row(
                                "SELECT id FROM images WHERE remote_id = ?1",
                                params![remote_id],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()
                            .ok()
                            .flatten()
                    })
                })
                .unwrap_or_else(|| remote_image_id.unwrap_or("").to_string());
            connection
                .execute(
                    "INSERT INTO issues (id, remote_id, project_id, image_id, title, description, severity, status, source, reporter_id, revision, server_revision, local_revision, sync_status, dirty, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'remote', ?9, ?10, ?10, 0, 'synced', 0, ?11, ?11) ON CONFLICT(id) DO UPDATE SET remote_id = COALESCE(excluded.remote_id, issues.remote_id), title = excluded.title, description = excluded.description, severity = excluded.severity, status = excluded.status, revision = excluded.revision, server_revision = excluded.server_revision, sync_status = 'synced', dirty = 0, updated_at = excluded.updated_at",
                    params![
                        local_issue_id,
                        remote_id,
                        project_id,
                        local_image_id,
                        payload.get("title").and_then(Value::as_str).unwrap_or("远程缺陷"),
                        payload.get("description").and_then(Value::as_str).unwrap_or(""),
                        payload.get("severity").and_then(Value::as_str).unwrap_or("major"),
                        payload.get("status").and_then(Value::as_str).unwrap_or("open"),
                        payload.get("reporterId").and_then(Value::as_str).unwrap_or("remote-user"),
                        revision.unwrap_or(1),
                        now_unix_string()
                    ],
                )
                .map_err(|err| err.to_string())?;
        }
        "folder" => {
            let remote_id = payload.get("id").and_then(Value::as_str);
            let local_id = payload
                .get("clientKey")
                .and_then(Value::as_str)
                .unwrap_or(entity_id);
            connection
                .execute(
                    "INSERT INTO folders (id, project_id, remote_id, parent_id, name, sort_order, revision, server_revision, local_revision, sync_status, dirty, created_by, updated_by, created_at, updated_at) VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?6, 0, 'synced', 0, 'remote-user', 'remote-user', ?7, ?7) ON CONFLICT(id) DO UPDATE SET remote_id = COALESCE(excluded.remote_id, folders.remote_id), name = excluded.name, sort_order = excluded.sort_order, revision = excluded.revision, server_revision = excluded.server_revision, sync_status = 'synced', dirty = 0, updated_at = excluded.updated_at",
                    params![
                        local_id,
                        project_id,
                        remote_id,
                        payload.get("name").and_then(Value::as_str).unwrap_or("远程文件夹"),
                        payload.get("sortOrder").and_then(Value::as_i64).unwrap_or(0),
                        revision.unwrap_or(1),
                        now_unix_string()
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        "folder_member" => {
            let remote_folder_id = payload
                .get("folderId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let remote_image_id = payload
                .get("imageId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let local_folder_id = connection
                .query_row(
                    "SELECT id FROM folders WHERE remote_id = ?1 OR id = ?1 LIMIT 1",
                    params![remote_folder_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| remote_folder_id.to_string());
            let local_image_id = connection
                .query_row(
                    "SELECT id FROM images WHERE remote_id = ?1 OR id = ?1 LIMIT 1",
                    params![remote_image_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| remote_image_id.to_string());
            connection
                .execute(
                    "INSERT INTO folder_members (folder_id, image_id, revision, server_revision, local_revision, sync_status, dirty, created_at, updated_at) VALUES (?1, ?2, ?3, ?3, 0, 'synced', 0, ?4, ?4) ON CONFLICT(folder_id, image_id) DO UPDATE SET revision = excluded.revision, server_revision = excluded.server_revision, sync_status = 'synced', dirty = 0, deleted_at = NULL, updated_at = excluded.updated_at",
                    params![
                        local_folder_id,
                        local_image_id,
                        revision.unwrap_or(1),
                        now_unix_string()
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        "issue_comment" => {
            let remote_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(entity_id);
            let remote_issue_id = payload
                .get("issueId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let local_issue_id = connection
                .query_row(
                    "SELECT id FROM issues WHERE remote_id = ?1 OR id = ?1 LIMIT 1",
                    params![remote_issue_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| remote_issue_id.to_string());
            connection
                .execute(
                    "INSERT INTO issue_comments (id, issue_id, author_id, content, revision, created_at, updated_at) VALUES (?1, ?2, 'remote-user', ?3, ?4, ?5, ?5) ON CONFLICT(id) DO UPDATE SET content = excluded.content, revision = excluded.revision, updated_at = excluded.updated_at",
                    params![
                        remote_id,
                        local_issue_id,
                        payload.get("content").and_then(Value::as_str).unwrap_or(""),
                        revision.unwrap_or(1),
                        now_unix_string()
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        "issue_attachment" => {
            let remote_id = payload
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or(entity_id);
            let local_id = payload
                .get("clientKey")
                .and_then(Value::as_str)
                .unwrap_or(entity_id);
            let remote_issue_id = payload
                .get("issueId")
                .and_then(Value::as_str)
                .unwrap_or("");
            let local_issue_id = connection
                .query_row(
                    "SELECT id FROM issues WHERE remote_id = ?1 OR id = ?1 LIMIT 1",
                    params![remote_issue_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?
                .unwrap_or_else(|| remote_issue_id.to_string());
            connection
                .execute(
                    r#"
                    INSERT INTO issue_attachments (
                        id, project_id, issue_id, remote_id, file_name, local_path,
                        object_key, content_hash, mime_type, byte_size, upload_state,
                        revision, sync_state, created_by, created_at, updated_at
                    ) VALUES (
                        ?1, ?2, ?3, ?4, ?5, '', ?6, ?7, ?8, ?9, 'ready', ?10,
                        'clean', 'remote-user', ?11, ?11
                    )
                    ON CONFLICT(id) DO UPDATE SET
                        remote_id = excluded.remote_id,
                        file_name = excluded.file_name,
                        object_key = excluded.object_key,
                        content_hash = excluded.content_hash,
                        mime_type = excluded.mime_type,
                        byte_size = excluded.byte_size,
                        upload_state = 'ready',
                        revision = excluded.revision,
                        sync_state = 'clean',
                        updated_at = excluded.updated_at
                    "#,
                    params![
                        local_id,
                        project_id,
                        local_issue_id,
                        remote_id,
                        payload
                            .get("fileName")
                            .and_then(Value::as_str)
                            .unwrap_or("attachment"),
                        payload
                            .get("objectKey")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        payload
                            .get("contentHash")
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                        payload
                            .get("mimeType")
                            .and_then(Value::as_str)
                            .unwrap_or("application/octet-stream"),
                        payload
                            .get("byteSize")
                            .and_then(Value::as_i64)
                            .unwrap_or(0),
                        revision.unwrap_or(1),
                        now_unix_string()
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        _ => {}
    }
    Ok(())
}

fn mark_entity_deleted(
    connection: &Connection,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), String> {
    if entity_type == "issue_attachment" {
        connection
            .execute(
                "UPDATE issue_attachments SET deleted_at = ?2, sync_state = 'clean' WHERE id = ?1",
                params![entity_id, now_unix_string()],
            )
            .map_err(|error| error.to_string())?;
        return Ok(());
    }
    let table = match entity_type {
        "asset" => "images",
        "annotation" => "annotations",
        "issue" => "issues",
        "folder" => "folders",
        _ => return Ok(()),
    };
    connection
        .execute(
            &format!(
                "UPDATE {table} SET deleted_at = ?2, sync_status = 'synced', dirty = 0 WHERE id = ?1"
            ),
            params![entity_id, now_unix_string()],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn validate_project_mode(mode: &str) -> Result<(), String> {
    if matches!(mode, "local_only" | "cloud_linked" | "mirrored") {
        Ok(())
    } else {
        Err(format!("invalid project mode: {mode}"))
    }
}

fn validate_cache_policy(policy: &str) -> Result<(), String> {
    if matches!(policy, "thumbnail_only" | "on_demand" | "full_mirror") {
        Ok(())
    } else {
        Err(format!("invalid cache policy: {policy}"))
    }
}

fn validate_severity(severity: &str) -> Result<(), String> {
    if matches!(
        severity,
        "blocker" | "critical" | "major" | "minor" | "suggestion"
    ) {
        Ok(())
    } else {
        Err(format!("invalid issue severity: {severity}"))
    }
}

fn valid_issue_transition(current: &str, next: &str) -> bool {
    matches!(
        (current, next),
        ("open", "in_progress")
            | ("in_progress", "resolved")
            | ("resolved", "pending_review")
            | ("pending_review", "closed")
            | ("pending_review", "reopened")
            | ("reopened", "in_progress")
            | ("closed", "reopened")
    )
}

fn project_id_for_connection(connection: &Connection, path: &Path) -> Result<String, String> {
    let stored = connection
        .query_row("SELECT id FROM projects LIMIT 1", [], |row| row.get(0))
        .optional()
        .map_err(|err| err.to_string())?;
    Ok(stored.unwrap_or_else(|| {
        path.parent()
            .and_then(|parent| parent.file_name())
            .and_then(|value| value.to_str())
            .unwrap_or("local-project")
            .to_string()
    }))
}

fn schema_version(connection: &Connection) -> Result<i64, String> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|err| err.to_string())
}

fn ensure_column(
    connection: &Connection,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|err| err.to_string())?;
    let exists = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|err| err.to_string())?
        .filter_map(Result::ok)
        .any(|name| name == column);
    drop(statement);
    if !exists {
        connection
            .execute(
                &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                [],
            )
            .map_err(|err| err.to_string())?;
    }
    Ok(())
}

fn migration_name(version: i64) -> &'static str {
    match version {
        1 => "legacy-columns",
        2 => "hybrid-project-management",
        3 => "legacy-review-issues",
        4 => "remote-project-configuration",
        5 => "remote-entity-identifiers",
        6 => "issue-attachments",
        _ => "unknown",
    }
}

fn migrate_issue_attachments(connection: &Connection) -> Result<(), String> {
    connection
        .execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS issue_attachments (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                issue_id TEXT NOT NULL,
                remote_id TEXT,
                file_name TEXT NOT NULL,
                local_path TEXT NOT NULL,
                object_key TEXT NOT NULL DEFAULT '',
                content_hash TEXT NOT NULL,
                mime_type TEXT NOT NULL,
                byte_size INTEGER NOT NULL,
                upload_state TEXT NOT NULL DEFAULT 'pending',
                revision INTEGER NOT NULL DEFAULT 1,
                sync_state TEXT NOT NULL DEFAULT 'local_only',
                created_by TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                deleted_at TEXT,
                FOREIGN KEY(issue_id) REFERENCES issues(id)
            );
            "#,
        )
        .map_err(|error| error.to_string())?;
    for (column, definition) in [
        ("project_id", "TEXT NOT NULL DEFAULT ''"),
        ("remote_id", "TEXT"),
        ("local_path", "TEXT NOT NULL DEFAULT ''"),
        ("object_key", "TEXT NOT NULL DEFAULT ''"),
        ("content_hash", "TEXT NOT NULL DEFAULT ''"),
        ("upload_state", "TEXT NOT NULL DEFAULT 'pending'"),
        ("revision", "INTEGER NOT NULL DEFAULT 1"),
        ("sync_state", "TEXT NOT NULL DEFAULT 'local_only'"),
        ("updated_at", "TEXT NOT NULL DEFAULT ''"),
        ("deleted_at", "TEXT"),
    ] {
        ensure_column(connection, "issue_attachments", column, definition)?;
    }
    connection
        .execute_batch(
            r#"
            CREATE UNIQUE INDEX IF NOT EXISTS issue_attachments_remote_id_unique
                ON issue_attachments(remote_id)
                WHERE remote_id IS NOT NULL;
            CREATE INDEX IF NOT EXISTS issue_attachments_issue_index
                ON issue_attachments(issue_id, created_at)
                WHERE deleted_at IS NULL;
            "#,
        )
        .map_err(|error| error.to_string())
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{prefix}-{nanos}")
}

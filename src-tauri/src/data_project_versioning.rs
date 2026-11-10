//! Local branch/DAG versioning for data projects.
//!
//! The first data-project implementation stores immutable image, annotation,
//! and metadata objects in `data-project-objects/` and records a linear
//! history in `data-projects.sqlite`.  This module layers branch refs and
//! two-parent merge commits on top of those records.  Checkouts are materialized
//! beneath the workspace root; an imported source directory is never changed.

use crate::project_fs;
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Cursor, Read, Write},
    path::{Component, Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::SystemTime,
};
use uuid::Uuid;

const OBJECT_BUFFER_SIZE: usize = 1024 * 1024;
const DEFAULT_BRANCH: &str = "main";
static VERSIONING_OPERATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn operation_lock() -> Result<MutexGuard<'static, ()>, String> {
    VERSIONING_OPERATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "data-project operation lock is poisoned".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectBranch {
    pub data_project_id: String,
    pub name: String,
    pub head_commit_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub current: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VersionCommitEntry {
    pub dataset_id: String,
    pub image_id: String,
    pub file_name: String,
    pub image_hash: String,
    pub annotation_hash: String,
    pub status: String,
    pub qa_status: String,
    pub split: String,
    pub width: u32,
    pub height: u32,
}

/// Project-level metadata is intentionally embedded in every immutable tree.
/// The flattened map keeps fields introduced by a newer client intact when an
/// older client edits only the dataset groups.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectDatasetGroup {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub dataset_keys: Vec<String>,
    #[serde(default)]
    pub position: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectMetadata {
    #[serde(default = "default_project_metadata_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub dataset_groups: Vec<DataProjectDatasetGroup>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl Default for DataProjectMetadata {
    fn default() -> Self {
        Self {
            schema_version: 1,
            dataset_groups: Vec::new(),
            extra: BTreeMap::new(),
        }
    }
}

fn default_project_metadata_schema_version() -> u32 {
    1
}

/// Decode old rows/manifests where `projectMetadata` did not exist.  An empty
/// object is a valid legacy value and becomes schema version 1 with no groups.
pub(crate) fn parse_project_metadata_json(value: &str) -> Result<DataProjectMetadata, String> {
    if value.trim().is_empty() || value.trim() == "null" {
        return Ok(DataProjectMetadata::default());
    }
    let mut metadata: DataProjectMetadata = serde_json::from_str(value)
        .map_err(|error| format!("invalid project metadata: {error}"))?;
    normalize_project_metadata(&mut metadata, None)?;
    Ok(metadata)
}

fn normalize_project_metadata(
    metadata: &mut DataProjectMetadata,
    allowed_dataset_ids: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    if metadata.schema_version != 1 {
        return Err(format!(
            "unsupported project metadata schema version: {}",
            metadata.schema_version
        ));
    }
    if metadata.dataset_groups.len() > 10_000 {
        return Err("too many data-project dataset groups".to_string());
    }
    let mut ids = BTreeSet::new();
    let mut dataset_keys = BTreeSet::new();
    for group in &mut metadata.dataset_groups {
        group.id = group.id.trim().to_string();
        group.name = group.name.trim().to_string();
        if group.id.is_empty() || group.id.len() > 128 || group.id.chars().any(char::is_control) {
            return Err("invalid dataset group id".to_string());
        }
        if group.name.is_empty() || group.name.len() > 240 {
            return Err("invalid dataset group name".to_string());
        }
        if group.position < 0 {
            return Err("dataset group position must be non-negative".to_string());
        }
        if !ids.insert(group.id.clone()) {
            return Err(format!("duplicate dataset group id: {}", group.id));
        }
        // Membership order is user-visible in the grouping board and is part
        // of the merge contract (target members first, incoming-only members
        // appended).  Deduplicate without sorting so checkout/merge order is
        // not silently rewritten.
        let mut seen_members = BTreeSet::new();
        group.dataset_keys.retain(|key| seen_members.insert(key.clone()));
        if group.dataset_keys.len() > 100_000 {
            return Err("too many dataset keys in dataset group".to_string());
        }
        for key in &group.dataset_keys {
            if key.is_empty() || key.len() > 256 || key.contains("::") || key.contains('\0') {
                return Err(format!("invalid dataset group member key: {key}"));
            }
            if !dataset_keys.insert(key.clone()) {
                return Err(format!("dataset belongs to more than one group: {key}"));
            }
            if let Some(allowed) = allowed_dataset_ids {
                if !allowed.contains(key) {
                    return Err(format!("dataset group references unknown dataset: {key}"));
                }
            }
        }
    }
    metadata.dataset_groups.sort_by(|left, right| {
        left.position
            .cmp(&right.position)
            .then(left.id.cmp(&right.id))
    });
    Ok(())
}

pub(crate) fn validate_project_metadata(
    metadata: &mut DataProjectMetadata,
    allowed_dataset_ids: &[String],
) -> Result<(), String> {
    let allowed = allowed_dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    normalize_project_metadata(metadata, Some(&allowed))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VersionCommit {
    pub id: String,
    pub data_project_id: String,
    pub branch: Option<String>,
    pub parent_id: Option<String>,
    pub second_parent_id: Option<String>,
    pub message: String,
    pub created_at: String,
    pub dataset_ids: Vec<String>,
    pub dataset_metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub project_metadata: DataProjectMetadata,
    pub entries: Vec<VersionCommitEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectCheckout {
    pub data_project_id: String,
    pub branch: String,
    pub commit_id: Option<String>,
    pub path: String,
    pub dirty: bool,
}

/// The immutable-entry shape exposed to the existing dataset workbench.  It
/// deliberately mirrors the fields that the legacy image index exposes while
/// keeping the physical checkout path private to this module.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectCheckoutImage {
    pub id: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub split: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CheckoutAnnotationWriteResult {
    pub revision: String,
    pub saved_at: String,
    pub audit_event_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct VersionConflict {
    pub key: String,
    pub kind: String,
    pub base: Option<Value>,
    pub ours: Option<Value>,
    pub theirs: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ours_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs_description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MergeResult {
    pub session_id: Option<String>,
    pub commit: Option<VersionCommit>,
    pub conflicts: Vec<VersionConflict>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectMergeSession {
    pub id: String,
    pub data_project_id: String,
    pub ours_branch: String,
    pub theirs_branch: String,
    pub base_commit_id: Option<String>,
    pub ours_commit_id: String,
    pub theirs_commit_id: String,
    pub message: String,
    pub status: String,
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectGraph {
    pub data_project_id: String,
    pub branches: Vec<DataProjectBranch>,
    pub commits: Vec<VersionCommit>,
    pub object_hashes: Vec<String>,
}

/// A graph bundle intentionally contains references, not duplicated image
/// bytes. Transfer callers stream each `object_hashes` member through the
/// object read/write API so a large project never creates a second full copy
/// in a JSON response.
pub type DataProjectGraphBundle = DataProjectGraph;

#[derive(Debug, Clone, PartialEq)]
struct Snapshot {
    dataset_ids: Vec<String>,
    metadata: BTreeMap<String, String>,
    project_metadata: DataProjectMetadata,
    entries: BTreeMap<String, VersionCommitEntry>,
}

#[derive(Debug, Clone)]
struct MergePlan {
    snapshot: Snapshot,
    conflicts: Vec<VersionConflict>,
}

fn now_string() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn db_path() -> PathBuf {
    project_fs::workspace_data_root().join("data-projects.sqlite")
}

fn objects_root() -> PathBuf {
    project_fs::workspace_data_root().join("data-project-objects")
}

fn check_name(value: &str, field: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 {
        return Err(format!("invalid {field}"));
    }
    if value == "." || value == ".." || value.contains('/') || value.contains('\\') {
        return Err(format!("invalid {field}"));
    }
    Ok(value.to_string())
}

fn open_db() -> Result<Connection, String> {
    let path = db_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let connection = Connection::open(&path).map_err(|error| error.to_string())?;
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            PRAGMA busy_timeout = 5000;
            CREATE TABLE IF NOT EXISTS data_projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                head_commit_id TEXT,
                account_scope TEXT
            );
            CREATE TABLE IF NOT EXISTS data_project_datasets (
                dataset_id TEXT PRIMARY KEY,
                data_project_id TEXT NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
                assigned_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS data_project_commits (
                id TEXT PRIMARY KEY,
                data_project_id TEXT NOT NULL REFERENCES data_projects(id) ON DELETE CASCADE,
                parent_id TEXT REFERENCES data_project_commits(id),
                parent2_id TEXT,
                message TEXT NOT NULL,
                created_at TEXT NOT NULL,
                dataset_ids_json TEXT NOT NULL,
                dataset_metadata_json TEXT NOT NULL DEFAULT '{}',
                project_metadata_json TEXT NOT NULL DEFAULT '{"schemaVersion":1,"datasetGroups":[]}'
            );
            CREATE TABLE IF NOT EXISTS data_project_commit_entries (
                commit_id TEXT NOT NULL REFERENCES data_project_commits(id) ON DELETE CASCADE,
                dataset_id TEXT NOT NULL,
                image_id TEXT NOT NULL,
                file_name TEXT NOT NULL,
                image_hash TEXT NOT NULL,
                annotation_hash TEXT NOT NULL,
                status TEXT NOT NULL,
                qa_status TEXT NOT NULL,
                split TEXT NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                PRIMARY KEY (commit_id, dataset_id, image_id)
            );
            CREATE TABLE IF NOT EXISTS data_project_objects (
                hash TEXT PRIMARY KEY,
                kind TEXT NOT NULL,
                byte_length INTEGER NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS data_project_branches (
                data_project_id TEXT NOT NULL,
                name TEXT NOT NULL,
                head_commit_id TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                revision INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (data_project_id, name)
            );
            CREATE TABLE IF NOT EXISTS data_project_active_branches (
                data_project_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS data_project_commit_parents (
                commit_id TEXT NOT NULL,
                parent_id TEXT NOT NULL,
                parent_index INTEGER NOT NULL,
                PRIMARY KEY (commit_id, parent_index)
            );
            CREATE TABLE IF NOT EXISTS data_project_merge_sessions (
                id TEXT PRIMARY KEY,
                data_project_id TEXT NOT NULL,
                ours_branch TEXT NOT NULL,
                theirs_branch TEXT NOT NULL,
                base_commit_id TEXT,
                ours_commit_id TEXT NOT NULL,
                theirs_commit_id TEXT NOT NULL,
                message TEXT NOT NULL,
                status TEXT NOT NULL,
                resolutions_json TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS data_project_merge_conflicts (
                session_id TEXT NOT NULL,
                conflict_key TEXT NOT NULL,
                kind TEXT NOT NULL,
                base_json TEXT,
                ours_json TEXT,
                theirs_json TEXT,
                PRIMARY KEY (session_id, conflict_key)
            );
            CREATE TABLE IF NOT EXISTS data_project_trusted_scopes (
                data_project_id TEXT NOT NULL,
                source_scope TEXT NOT NULL,
                created_at TEXT NOT NULL,
                PRIMARY KEY (data_project_id, source_scope)
            );
            CREATE INDEX IF NOT EXISTS idx_data_project_branches_project
              ON data_project_branches(data_project_id);
            CREATE INDEX IF NOT EXISTS idx_data_project_parents_parent
              ON data_project_commit_parents(parent_id);
            "#,
        )
        .map_err(|error| format!("initialize versioning schema: {error}"))?;
    // The original data-project schema predates two-parent commits. Perform
    // this migration here too, since versioning APIs may be the first caller.
    let commit_columns = connection
        .prepare("PRAGMA table_info(data_project_commits)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !commit_columns.iter().any(|column| column == "parent2_id") {
        connection
            .execute(
                "ALTER TABLE data_project_commits ADD COLUMN parent2_id TEXT",
                [],
            )
            .map_err(|error| format!("migrate two-parent commit schema: {error}"))?;
    }
    if !commit_columns
        .iter()
        .any(|column| column == "dataset_metadata_json")
    {
        connection
            .execute(
                "ALTER TABLE data_project_commits ADD COLUMN dataset_metadata_json TEXT NOT NULL DEFAULT '{}'",
                [],
            )
            .map_err(|error| format!("migrate dataset metadata schema: {error}"))?;
    }
    if !commit_columns
        .iter()
        .any(|column| column == "project_metadata_json")
    {
        connection
            .execute(
                "ALTER TABLE data_project_commits ADD COLUMN project_metadata_json TEXT NOT NULL DEFAULT '{\"schemaVersion\":1,\"datasetGroups\":[]}'",
                [],
            )
            .map_err(|error| format!("migrate project metadata schema: {error}"))?;
    }
    let project_columns = connection
        .prepare("PRAGMA table_info(data_projects)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !project_columns
        .iter()
        .any(|column| column == "account_scope")
    {
        connection
            .execute(
                "ALTER TABLE data_projects ADD COLUMN account_scope TEXT",
                [],
            )
            .map_err(|error| format!("migrate account scope schema: {error}"))?;
    }
    let branch_columns = connection
        .prepare("PRAGMA table_info(data_project_branches)")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if !branch_columns.iter().any(|column| column == "revision") {
        connection
            .execute(
                "ALTER TABLE data_project_branches ADD COLUMN revision INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|error| format!("migrate branch revision schema: {error}"))?;
    }
    // Existing linear commits have their first parent in data_project_commits.
    // Rebuilding the parent table is idempotent and does not alter old IDs.
    let mut statement = connection
        .prepare("SELECT id, parent_id FROM data_project_commits WHERE parent_id IS NOT NULL")
        .map_err(|error| error.to_string())?;
    let parents = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    for (commit_id, parent_id) in parents {
        connection
            .execute(
                "INSERT OR IGNORE INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 0)",
                params![commit_id, parent_id],
            )
            .map_err(|error| error.to_string())?;
    }
    let mut second_statement = connection
        .prepare("SELECT id, parent2_id FROM data_project_commits WHERE parent2_id IS NOT NULL")
        .map_err(|error| error.to_string())?;
    let second_parents = second_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(second_statement);
    for (commit_id, parent_id) in second_parents {
        connection
            .execute(
                "INSERT OR IGNORE INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 1)",
                params![commit_id, parent_id],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(connection)
}

fn ensure_project(connection: &Connection, project_id: &str) -> Result<(), String> {
    let scope: Option<String> = connection
        .query_row(
            "SELECT account_scope FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .flatten();
    if connection
        .query_row(
            "SELECT 1 FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .is_none()
    {
        return Err(format!("data project not found: {project_id}"));
    }
    if let Some(scope) = scope {
        let Some(session) = crate::credentials::current_session()? else {
            return Err("data project is outside the active account scope".to_string());
        };
        let current = format!(
            "{}\n{}",
            session.server_url.trim_end_matches('/'),
            session.account_id
        );
        if current != scope {
            return Err("data project is outside the active account scope".to_string());
        }
    }
    Ok(())
}

fn project_scope(connection: &Connection, project_id: &str) -> Result<Option<String>, String> {
    let value: Option<Option<String>> = connection
        .query_row(
            "SELECT account_scope FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    Ok(value.flatten())
}

// A metadata object is either new (with an explicit accountScope claim) or
// legacy. Legacy history is deliberately not guessed to be local/remote:
// without evidence it is rejected, preventing a stale remote object from
// becoming visible after an account switch.
fn metadata_scope_evidence(hash: &str) -> Result<Option<Option<String>>, String> {
    let bytes = read_data_project_object(hash)?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid dataset metadata object {hash}: {error}"))?;
    let Some(scope) = value.get("accountScope") else {
        return Ok(None);
    };
    if scope.is_null() {
        Ok(Some(None))
    } else {
        let scope = scope
            .as_str()
            .ok_or_else(|| format!("invalid accountScope in dataset metadata object {hash}"))?;
        Ok(Some(Some(scope.to_string())))
    }
}

fn history_scope_authorized(connection: &Connection, project_id: &str) -> Result<(), String> {
    ensure_project(connection, project_id)?;
    let scope = project_scope(connection, project_id)?;
    let mut trusted_scopes = BTreeSet::new();
    let mut trusted_statement = connection
        .prepare("SELECT source_scope FROM data_project_trusted_scopes WHERE data_project_id = ?1")
        .map_err(|error| error.to_string())?;
    for row in trusted_statement
        .query_map(params![project_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
    {
        trusted_scopes.insert(row.map_err(|error| error.to_string())?);
    }
    let mut statement = connection
        .prepare(
            "SELECT c.dataset_metadata_json, EXISTS(SELECT 1 FROM data_project_commit_entries e WHERE e.commit_id = c.id) FROM data_project_commits c WHERE c.data_project_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for (metadata_json, has_entries) in rows {
        let metadata = serde_json::from_str::<BTreeMap<String, String>>(&metadata_json)
            .map_err(|error| format!("invalid commit dataset metadata: {error}"))?;
        if has_entries && metadata.is_empty() {
            return Err("data-project history lacks dataset account-scope evidence".to_string());
        }
        for hash in metadata.values() {
            let Some(evidence) = metadata_scope_evidence(hash)? else {
                return Err(
                    "data-project history contains legacy metadata without account-scope evidence"
                        .to_string(),
                );
            };
            match (scope.as_deref(), evidence) {
                (None, None) => {}
                (Some(expected), Some(actual))
                    if expected == actual.as_str() || trusted_scopes.contains(&actual) => {}
                // A project can be bound to a remote scope after it already
                // contained local data. Explicit `accountScope: null` keeps
                // those local objects shareable without accepting legacy
                // metadata that has no evidence at all.
                (Some(_), None) => {}
                _ => {
                    return Err(
                        "data-project history is outside the active account scope".to_string()
                    );
                }
            }
        }
    }
    Ok(())
}

fn metadata_snapshot_authorized(
    connection: &Connection,
    project_id: &str,
    metadata: &BTreeMap<String, String>,
) -> Result<(), String> {
    let scope = project_scope(connection, project_id)?;
    let mut trusted_scopes = BTreeSet::new();
    let mut statement = connection
        .prepare("SELECT source_scope FROM data_project_trusted_scopes WHERE data_project_id = ?1")
        .map_err(|error| error.to_string())?;
    for row in statement
        .query_map(params![project_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
    {
        trusted_scopes.insert(row.map_err(|error| error.to_string())?);
    }
    for hash in metadata.values() {
        let Some(evidence) = metadata_scope_evidence(hash)? else {
            return Err("working tree metadata lacks account-scope evidence".to_string());
        };
        match (scope.as_deref(), evidence) {
            (None, None) => {}
            (Some(expected), Some(actual))
                if expected == actual.as_str() || trusted_scopes.contains(&actual) => {}
            (Some(_), None) => {}
            _ => {
                return Err("working tree metadata is outside the active account scope".to_string())
            }
        }
    }
    Ok(())
}

fn ensure_default_branch(connection: &Connection, project_id: &str) -> Result<(), String> {
    let exists: Option<i64> = connection
        .query_row(
            "SELECT 1 FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
            params![project_id, DEFAULT_BRANCH],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if exists.is_some() {
        return Ok(());
    }
    let head: Option<String> = connection
        .query_row(
            "SELECT head_commit_id FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let now = now_string();
    connection
        .execute(
            "INSERT INTO data_project_branches (data_project_id, name, head_commit_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![project_id, DEFAULT_BRANCH, head, now],
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "INSERT OR IGNORE INTO data_project_active_branches (data_project_id, name, updated_at) VALUES (?1, ?2, ?3)",
            params![project_id, DEFAULT_BRANCH, now],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn active_branch_name(connection: &Connection, project_id: &str) -> Result<String, String> {
    ensure_default_branch(connection, project_id)?;
    // The legacy commit API advances data_projects.head_commit_id directly.
    // Keep the active branch ref in lockstep so old and new callers observe
    // the same current history without rewriting any commit IDs.
    let active: String = connection
        .query_row(
            "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let project_head: Option<String> = connection
        .query_row(
            "SELECT head_commit_id FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    connection
        .execute(
            "UPDATE data_project_branches SET head_commit_id = ?3 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id IS NOT ?3",
            params![project_id, active, project_head],
        )
        .map_err(|error| error.to_string())?;
    Ok(active)
}

fn branch_row(
    connection: &Connection,
    project_id: &str,
    name: &str,
) -> Result<DataProjectBranch, String> {
    let active = active_branch_name(connection, project_id)?;
    connection
        .query_row(
            "SELECT name, head_commit_id, created_at, updated_at FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
            params![project_id, name],
            |row| {
                Ok(DataProjectBranch {
                    data_project_id: project_id.to_string(),
                    name: row.get(0)?,
                    head_commit_id: row.get(1)?,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                    current: active == name,
                })
            },
        )
        .map_err(|error| format!("branch not found: {project_id}/{name}: {error}"))
}

pub fn list_data_project_branches(project_id: &str) -> Result<Vec<DataProjectBranch>, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let active = active_branch_name(&connection, project_id)?;
    let mut statement = connection
        .prepare("SELECT name, head_commit_id, created_at, updated_at FROM data_project_branches WHERE data_project_id = ?1 ORDER BY name")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![project_id], |row| {
            Ok(DataProjectBranch {
                data_project_id: project_id.to_string(),
                name: row.get(0)?,
                head_commit_id: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
                current: active == row.get::<_, String>(0)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string());
    rows
}

pub fn create_data_project_branch(
    project_id: &str,
    name: &str,
    from_commit_id: Option<&str>,
) -> Result<DataProjectBranch, String> {
    let _operation_guard = operation_lock()?;
    let name = check_name(name, "branch name")?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let source = if let Some(commit_id) = from_commit_id {
        validate_commit_exists(&connection, project_id, commit_id)?;
        Some(commit_id.to_string())
    } else {
        let active = active_branch_name(&connection, project_id)?;
        branch_row(&connection, project_id, &active)?.head_commit_id
    };
    if connection
        .query_row(
            "SELECT 1 FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
            params![project_id, name],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .is_some()
    {
        return Err(format!("branch already exists: {name}"));
    }
    let now = now_string();
    connection
        .execute(
            "INSERT INTO data_project_branches (data_project_id, name, head_commit_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![project_id, name, source, now],
        )
        .map_err(|error| error.to_string())?;
    branch_row(&connection, project_id, &name)
}

/// Atomically advance a branch ref.  `expected_head_id` must be `None` for an
/// empty branch; a caller never gets an implicit force-update of a divergent
/// ref.  This is the primitive used by sync/import code after all objects and
/// commit rows have been validated.
pub fn compare_and_swap_data_project_branch(
    project_id: &str,
    branch: &str,
    expected_head_id: Option<&str>,
    new_head_id: Option<&str>,
) -> Result<DataProjectBranch, String> {
    let _operation_guard = operation_lock()?;
    let branch = check_name(branch, "branch name")?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    if let Some(commit_id) = new_head_id {
        validate_commit_exists(&connection, project_id, commit_id)?;
    }
    let active = active_branch_name(&connection, project_id)?;
    let before = branch_row(&connection, project_id, &branch)?;
    let target_snapshot = snapshot_from_commit(&connection, project_id, new_head_id)?;
    for dataset_id in &target_snapshot.dataset_ids {
        crate::data_projects::ensure_dataset_visible_to_current_account(dataset_id)?;
    }
    validate_membership_targets(&connection, project_id, &target_snapshot.dataset_ids)?;
    let old_membership = project_membership_rows(&connection, project_id)?;
    let (old_project_head, old_project_updated_at): (Option<String>, String) = connection
        .query_row(
            "SELECT head_commit_id, updated_at FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    if active == branch {
        if checkout_dirty(&checkout_root(project_id, &branch))? {
            return Err("active checkout has uncommitted changes".to_string());
        }
        ensure_no_unflushed_sync_state(&connection, project_id, &target_snapshot.dataset_ids)?;
    }
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let now = now_string();
    let updated = transaction
        .execute(
            "UPDATE data_project_branches SET head_commit_id = ?3, updated_at = ?4 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id IS ?5",
            params![project_id, branch, new_head_id, now, expected_head_id],
        )
        .map_err(|error| error.to_string())?;
    if updated != 1 {
        return Err("branch head changed; compare-and-swap rejected".to_string());
    }
    if active == branch {
        transaction
            .execute(
                "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
                params![project_id],
            )
            .map_err(|error| error.to_string())?;
        for dataset_id in &target_snapshot.dataset_ids {
            transaction
                .execute(
                    "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3)",
                    params![dataset_id, project_id, now],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![project_id, new_head_id, now],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    if active == branch {
        if let Err(error) = materialize_commit(&connection, project_id, &branch, new_head_id) {
            let rollback = rollback_failed_branch_cas(
                &connection,
                project_id,
                &branch,
                expected_head_id,
                new_head_id,
                &before.updated_at,
                &old_project_head,
                &old_project_updated_at,
                &old_membership,
            );
            return match rollback {
                Ok(()) => Err(format!("materialize data-project branch head: {error}")),
                Err(rollback_error) => Err(format!(
                    "materialize data-project branch head: {error}; rollback failed: {rollback_error}"
                )),
            };
        }
    }
    branch_row(&connection, project_id, &branch)
}

fn project_membership_rows(
    connection: &Connection,
    project_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let mut statement = connection
        .prepare(
            "SELECT dataset_id, assigned_at FROM data_project_datasets WHERE data_project_id = ?1 ORDER BY dataset_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![project_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(rows)
}

fn rollback_failed_branch_cas(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    expected_head_id: Option<&str>,
    new_head_id: Option<&str>,
    old_branch_updated_at: &str,
    old_project_head: &Option<String>,
    old_project_updated_at: &str,
    old_membership: &[(String, String)],
) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let restored = transaction
        .execute(
            "UPDATE data_project_branches SET head_commit_id = ?3, updated_at = ?4 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id IS ?5",
            params![
                project_id,
                branch,
                expected_head_id,
                old_branch_updated_at,
                new_head_id
            ],
        )
        .map_err(|error| error.to_string())?;
    if restored != 1 {
        return Err("branch head changed while rolling back materialization".to_string());
    }
    let active: String = transaction
        .query_row(
            "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if active == branch {
        let restored_project = transaction
            .execute(
                "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1 AND head_commit_id IS ?4",
                params![
                    project_id,
                    old_project_head.as_deref(),
                    old_project_updated_at,
                    new_head_id
                ],
            )
            .map_err(|error| error.to_string())?;
        if restored_project != 1 {
            return Err("data project HEAD changed while rolling back materialization".to_string());
        }
        transaction
            .execute(
                "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
                params![project_id],
            )
            .map_err(|error| error.to_string())?;
        for (dataset_id, assigned_at) in old_membership {
            transaction
                .execute(
                    "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3)",
                    params![dataset_id, project_id, assigned_at],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn commit_parent_ids(connection: &Connection, commit_id: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare("SELECT parent_id FROM data_project_commit_parents WHERE commit_id = ?1 ORDER BY parent_index")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![commit_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string());
    rows
}

fn validate_commit_exists(
    connection: &Connection,
    project_id: &str,
    commit_id: &str,
) -> Result<(), String> {
    let owner: Option<String> = connection
        .query_row(
            "SELECT data_project_id FROM data_project_commits WHERE id = ?1",
            params![commit_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if owner.as_deref() != Some(project_id) {
        return Err(format!("commit not found in data project: {commit_id}"));
    }
    Ok(())
}

fn commit_row(
    connection: &Connection,
    project_id: &str,
    commit_id: &str,
    branch: Option<String>,
) -> Result<VersionCommit, String> {
    validate_commit_exists(connection, project_id, commit_id)?;
    let (
        parent_id,
        second_parent_id,
        message,
        created_at,
        dataset_json,
        metadata_json,
        project_metadata_json,
    ): (
        Option<String>,
        Option<String>,
        String,
        String,
        String,
        String,
        String,
    ) = connection
        .query_row(
            "SELECT c.parent_id, c.parent2_id, c.message, c.created_at, c.dataset_ids_json, c.dataset_metadata_json, c.project_metadata_json FROM data_project_commits c WHERE c.id = ?1",
            params![commit_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
        )
        .map_err(|error| error.to_string())?;
    let dataset_ids = serde_json::from_str(&dataset_json).map_err(|error| error.to_string())?;
    let dataset_metadata =
        serde_json::from_str(&metadata_json).map_err(|error| error.to_string())?;
    let project_metadata = parse_project_metadata_json(&project_metadata_json)?;
    let mut statement = connection
        .prepare("SELECT dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height FROM data_project_commit_entries WHERE commit_id = ?1 ORDER BY dataset_id, image_id")
        .map_err(|error| error.to_string())?;
    let entries = statement
        .query_map(params![commit_id], |row| {
            Ok(VersionCommitEntry {
                dataset_id: row.get(0)?,
                image_id: row.get(1)?,
                file_name: row.get(2)?,
                image_hash: row.get(3)?,
                annotation_hash: row.get(4)?,
                status: row.get(5)?,
                qa_status: row.get(6)?,
                split: row.get(7)?,
                width: row.get(8)?,
                height: row.get(9)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(VersionCommit {
        id: commit_id.to_string(),
        data_project_id: project_id.to_string(),
        branch,
        parent_id,
        second_parent_id,
        message,
        created_at,
        dataset_ids,
        dataset_metadata,
        project_metadata,
        entries,
    })
}

pub fn list_data_project_version_commits(
    project_id: &str,
    branch: Option<&str>,
) -> Result<Vec<VersionCommit>, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    let branch = if let Some(name) = branch {
        check_name(name, "branch name")?
    } else {
        active_branch_name(&connection, project_id)?
    };
    let head = branch_row(&connection, project_id, &branch)?.head_commit_id;
    let mut commits = Vec::new();
    let mut queue = head.into_iter().collect::<VecDeque<_>>();
    let mut seen = BTreeSet::new();
    while let Some(commit_id) = queue.pop_front() {
        if !seen.insert(commit_id.clone()) {
            continue;
        }
        let commit = commit_row(&connection, project_id, &commit_id, Some(branch.clone()))?;
        for parent in commit_parent_ids(&connection, &commit_id)? {
            queue.push_back(parent);
        }
        commits.push(commit);
    }
    commits.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then(right.id.cmp(&left.id))
    });
    Ok(commits)
}

fn checkout_root(project_id: &str, branch: &str) -> PathBuf {
    project_fs::workspace_data_root()
        .join("data-project-checkouts")
        .join(project_id)
        .join(branch)
}

fn checkout_state_path(root: &Path) -> PathBuf {
    root.join(".data-project-checkout.json")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CheckoutState {
    project_id: String,
    branch: String,
    commit_id: Option<String>,
    tree_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckoutManifest {
    project_id: String,
    commit_id: Option<String>,
    datasets: Vec<String>,
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    project_metadata: DataProjectMetadata,
    entries: Vec<VersionCommitEntry>,
}

fn path_is_safe_relative(path: &str) -> bool {
    let value = Path::new(path);
    !value.is_absolute()
        && value.components().all(|component| {
            !matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn object_path(hash: &str) -> Result<PathBuf, String> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid object hash".to_string());
    }
    Ok(objects_root().join(hash))
}

fn hash_reader(reader: &mut impl Read) -> Result<(String, u64), String> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; OBJECT_BUFFER_SIZE];
    let mut length = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length += read as u64;
    }
    Ok((format!("{:x}", hasher.finalize()), length))
}

fn verify_object_file(hash: &str) -> Result<u64, String> {
    let path = object_path(hash)?;
    let mut file = File::open(&path).map_err(|error| format!("read object {hash}: {error}"))?;
    let (actual, length) = hash_reader(&mut file)?;
    if actual != hash {
        return Err(format!("object integrity check failed: {hash}"));
    }
    Ok(length)
}

pub fn read_data_project_object(hash: &str) -> Result<Vec<u8>, String> {
    let path = object_path(hash)?;
    let bytes = fs::read(&path).map_err(|error| format!("read object {hash}: {error}"))?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let (actual, _) = hash_reader(&mut cursor)?;
    if actual != hash {
        return Err(format!("object integrity check failed: {hash}"));
    }
    Ok(bytes)
}

pub fn write_data_project_object(hash: &str, bytes: &[u8]) -> Result<(), String> {
    let _operation_guard = operation_lock()?;
    let expected = object_path(hash)?;
    let mut cursor = Cursor::new(bytes);
    let (actual, _) = hash_reader(&mut cursor)?;
    if actual != hash {
        return Err("object hash does not match content".to_string());
    }
    fs::create_dir_all(objects_root()).map_err(|error| error.to_string())?;
    if expected.exists() {
        let current = read_data_project_object(hash)?;
        if current != bytes {
            return Err("refusing to overwrite a different immutable object".to_string());
        }
        record_object(hash, bytes.len() as u64)?;
        return Ok(());
    }
    let temporary = objects_root().join(format!(".import-{}", Uuid::new_v4()));
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
    }
    match fs::rename(&temporary, &expected) {
        Ok(()) => {
            record_object(hash, bytes.len() as u64)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another importer won the race.  Immutable objects are valid
            // only when the winner wrote the same bytes.
            let current = read_data_project_object(hash)?;
            if current == bytes {
                record_object(hash, bytes.len() as u64)?;
                Ok(())
            } else {
                Err("refusing to overwrite a different immutable object".to_string())
            }
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error.to_string())
        }
    }
}

fn validate_object(hash: &str) -> Result<(), String> {
    verify_object_file(hash).map(|_| ())
}

fn record_object(hash: &str, byte_length: u64) -> Result<(), String> {
    let connection = open_db()?;
    connection
        .execute(
            "INSERT OR IGNORE INTO data_project_objects (hash, kind, byte_length, created_at) VALUES (?1, 'imported', ?2, ?3)",
            params![hash, byte_length, now_string()],
        )
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn copy_verified_object(hash: &str, destination: &Path) -> Result<(), String> {
    let source = object_path(hash)?;
    let mut input = File::open(&source).map_err(|error| format!("read object {hash}: {error}"))?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let temporary = destination.with_extension(format!("import-{}", Uuid::new_v4()));
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; OBJECT_BUFFER_SIZE];
    let result = (|| {
        loop {
            let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| error.to_string())?;
        }
        output.sync_all().map_err(|error| error.to_string())?;
        let actual = format!("{:x}", hasher.finalize());
        if actual != hash {
            return Err(format!("object integrity check failed: {hash}"));
        }
        fs::rename(&temporary, destination).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read checkout file {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "checkout file is not a regular file: {}",
            path.display()
        ));
    }
    let mut input = File::open(path).map_err(|error| error.to_string())?;
    hash_reader(&mut input)
}

fn store_file_object(path: &Path) -> Result<(String, u64), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("read checkout file {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "checkout file is not a regular file: {}",
            path.display()
        ));
    }
    fs::create_dir_all(objects_root()).map_err(|error| error.to_string())?;
    let temporary = objects_root().join(format!(".checkout-object-{}", Uuid::new_v4()));
    let result = (|| {
        let mut input = File::open(path).map_err(|error| error.to_string())?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; OBJECT_BUFFER_SIZE];
        let mut length = 0u64;
        loop {
            let read = input.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| error.to_string())?;
            length += read as u64;
        }
        output.sync_all().map_err(|error| error.to_string())?;
        let hash = format!("{:x}", hasher.finalize());
        let target = object_path(&hash)?;
        if target.exists() {
            if verify_object_file(&hash)? != length {
                return Err(format!("content-addressed object collision: {hash}"));
            }
            fs::remove_file(&temporary).map_err(|error| error.to_string())?;
        } else {
            fs::rename(&temporary, &target).map_err(|error| error.to_string())?;
        }
        record_object(&hash, length)?;
        Ok((hash, length))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn snapshot_from_commit(
    connection: &Connection,
    project_id: &str,
    commit_id: Option<&str>,
) -> Result<Snapshot, String> {
    let Some(commit_id) = commit_id else {
        return Ok(Snapshot {
            dataset_ids: Vec::new(),
            metadata: BTreeMap::new(),
            project_metadata: DataProjectMetadata::default(),
            entries: BTreeMap::new(),
        });
    };
    let commit = commit_row(connection, project_id, commit_id, None)?;
    let entries = commit
        .entries
        .into_iter()
        .map(|entry| (entry_key(&entry), entry))
        .collect();
    Ok(Snapshot {
        dataset_ids: commit.dataset_ids,
        metadata: commit.dataset_metadata,
        project_metadata: commit.project_metadata,
        entries,
    })
}

fn entry_key(entry: &VersionCommitEntry) -> String {
    format!("{}\u{1f}{}", entry.dataset_id, entry.image_id)
}

fn value_entry(value: Option<&Value>) -> Option<VersionCommitEntry> {
    value.and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn merge_optional_value(
    key: String,
    kind: &str,
    base: Option<Value>,
    ours: Option<Value>,
    theirs: Option<Value>,
    conflicts: &mut Vec<VersionConflict>,
) -> Option<Value> {
    if ours == theirs {
        ours
    } else if ours == base {
        theirs
    } else if theirs == base {
        ours
    } else {
        conflicts.push(VersionConflict {
            key,
            kind: kind.to_string(),
            base,
            ours,
            theirs,
            display_name: None,
            base_description: None,
            ours_description: None,
            theirs_description: None,
        });
        None
    }
}

fn merge_entry_fields(
    key: &str,
    base: Option<&VersionCommitEntry>,
    ours: Option<&VersionCommitEntry>,
    theirs: Option<&VersionCommitEntry>,
    conflicts: &mut Vec<VersionConflict>,
) -> Option<VersionCommitEntry> {
    // Add/delete and delete/edit conflicts cannot be decomposed because there
    // is no entry to carry the independent fields.  Keep the old whole-entry
    // conflict shape for those cases.
    match (base, ours, theirs) {
        (None, None, None) => return None,
        (None, Some(ours), None) => return Some(ours.clone()),
        (None, None, Some(theirs)) => return Some(theirs.clone()),
        (Some(base), Some(ours), None) => {
            return merge_optional_value(
                format!("entry:{key}"),
                "entry",
                Some(serde_json::to_value(base).ok()?),
                Some(serde_json::to_value(ours).ok()?),
                None,
                conflicts,
            )
            .as_ref()
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        }
        (Some(base), None, Some(theirs)) => {
            return merge_optional_value(
                format!("entry:{key}"),
                "entry",
                Some(serde_json::to_value(base).ok()?),
                None,
                Some(serde_json::to_value(theirs).ok()?),
                conflicts,
            )
            .as_ref()
            .and_then(|value| serde_json::from_value(value.clone()).ok())
        }
        (None, Some(ours), Some(theirs)) if ours == theirs => return Some(ours.clone()),
        (None, Some(ours), Some(theirs)) => {
            let selected = merge_optional_value(
                format!("entry:{key}"),
                "entry",
                None,
                Some(serde_json::to_value(ours).ok()?),
                Some(serde_json::to_value(theirs).ok()?),
                conflicts,
            );
            return selected.and_then(|value| serde_json::from_value(value).ok());
        }
        (Some(_), Some(_), Some(_)) => {}
        (_, None, None) => return None,
    }

    let base = base?;
    let ours = ours?;
    let theirs = theirs?;
    let mut merged = ours.clone();
    let fields = [
        (
            "fileName",
            "image",
            serde_json::json!(base.file_name),
            serde_json::json!(ours.file_name),
            serde_json::json!(theirs.file_name),
        ),
        (
            "imageHash",
            "image",
            serde_json::json!(base.image_hash),
            serde_json::json!(ours.image_hash),
            serde_json::json!(theirs.image_hash),
        ),
        (
            "annotationHash",
            "annotation",
            serde_json::json!(base.annotation_hash),
            serde_json::json!(ours.annotation_hash),
            serde_json::json!(theirs.annotation_hash),
        ),
        (
            "status",
            "review",
            serde_json::json!(base.status),
            serde_json::json!(ours.status),
            serde_json::json!(theirs.status),
        ),
        (
            "qaStatus",
            "review",
            serde_json::json!(base.qa_status),
            serde_json::json!(ours.qa_status),
            serde_json::json!(theirs.qa_status),
        ),
        (
            "split",
            "split",
            serde_json::json!(base.split),
            serde_json::json!(ours.split),
            serde_json::json!(theirs.split),
        ),
        (
            "width",
            "image",
            serde_json::json!(base.width),
            serde_json::json!(ours.width),
            serde_json::json!(theirs.width),
        ),
        (
            "height",
            "image",
            serde_json::json!(base.height),
            serde_json::json!(ours.height),
            serde_json::json!(theirs.height),
        ),
    ];
    for (field, kind, base_value, ours_value, theirs_value) in fields {
        let selected = merge_optional_value(
            format!("entry:{key}:{field}"),
            kind,
            Some(base_value),
            Some(ours_value),
            Some(theirs_value),
            conflicts,
        );
        let Some(selected) = selected else {
            // Keep ours as a provisional materialized value.  A conflict
            // session cannot commit until resolve_data_project_merge patches
            // this field explicitly.
            continue;
        };
        match field {
            "fileName" => merged.file_name = selected.as_str()?.to_string(),
            "imageHash" => merged.image_hash = selected.as_str()?.to_string(),
            "annotationHash" => merged.annotation_hash = selected.as_str()?.to_string(),
            "status" => merged.status = selected.as_str()?.to_string(),
            "qaStatus" => merged.qa_status = selected.as_str()?.to_string(),
            "split" => merged.split = selected.as_str()?.to_string(),
            "width" => merged.width = selected.as_u64()?.try_into().ok()?,
            "height" => merged.height = selected.as_u64()?.try_into().ok()?,
            _ => {}
        }
    }
    Some(merged)
}

fn apply_entry_conflict_value(
    entry: &mut VersionCommitEntry,
    field: &str,
    value: Option<Value>,
) -> Result<(), String> {
    let value = value.ok_or_else(|| format!("entry field was deleted: {field}"))?;
    match field {
        "fileName" => {
            entry.file_name = value
                .as_str()
                .ok_or_else(|| "invalid merged fileName".to_string())?
                .to_string()
        }
        "imageHash" => {
            entry.image_hash = value
                .as_str()
                .ok_or_else(|| "invalid merged imageHash".to_string())?
                .to_string()
        }
        "annotationHash" => {
            entry.annotation_hash = value
                .as_str()
                .ok_or_else(|| "invalid merged annotationHash".to_string())?
                .to_string()
        }
        "status" => {
            entry.status = value
                .as_str()
                .ok_or_else(|| "invalid merged status".to_string())?
                .to_string()
        }
        "qaStatus" => {
            entry.qa_status = value
                .as_str()
                .ok_or_else(|| "invalid merged qaStatus".to_string())?
                .to_string()
        }
        "split" => {
            entry.split = value
                .as_str()
                .ok_or_else(|| "invalid merged split".to_string())?
                .to_string()
        }
        "width" => {
            entry.width = value
                .as_u64()
                .ok_or_else(|| "invalid merged width".to_string())?
                .try_into()
                .map_err(|_| "merged width is too large".to_string())?
        }
        "height" => {
            entry.height = value
                .as_u64()
                .ok_or_else(|| "invalid merged height".to_string())?
                .try_into()
                .map_err(|_| "merged height is too large".to_string())?
        }
        _ => return Err(format!("unknown entry merge field: {field}")),
    }
    Ok(())
}

fn common_ancestor(
    connection: &Connection,
    ours: &str,
    theirs: &str,
) -> Result<Option<String>, String> {
    let mut ours_depth = BTreeMap::new();
    let mut queue = VecDeque::from([(ours.to_string(), 0usize)]);
    while let Some((commit_id, depth)) = queue.pop_front() {
        if ours_depth.contains_key(&commit_id) {
            continue;
        }
        ours_depth.insert(commit_id.clone(), depth);
        for parent in commit_parent_ids(connection, &commit_id)? {
            queue.push_back((parent, depth + 1));
        }
    }
    let mut best: Option<(usize, String)> = None;
    let mut queue = VecDeque::from([(theirs.to_string(), 0usize)]);
    let mut seen = BTreeSet::new();
    while let Some((commit_id, depth)) = queue.pop_front() {
        if !seen.insert(commit_id.clone()) {
            continue;
        }
        if let Some(left_depth) = ours_depth.get(&commit_id) {
            let candidate = (left_depth + depth, commit_id.clone());
            if best.as_ref().map_or(true, |current| candidate < *current) {
                best = Some(candidate);
            }
        }
        for parent in commit_parent_ids(connection, &commit_id)? {
            queue.push_back((parent, depth + 1));
        }
    }
    Ok(best.map(|(_, commit_id)| commit_id))
}

fn encode_group_conflict_component(value: &str) -> String {
    // Group IDs and dataset keys are user/project data.  Hex keeps conflict
    // keys unambiguous even when an ID contains ':' or non-ASCII characters,
    // and matches the server's persisted merge-session key format without a
    // new native dependency.
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn group_conflict_key(group_id: &str, field: &str, dataset_key: Option<&str>) -> String {
    let mut key = format!(
        "group:v1:{}:{field}",
        encode_group_conflict_component(group_id)
    );
    if let Some(dataset_key) = dataset_key {
        key.push(':');
        key.push_str(&encode_group_conflict_component(dataset_key));
    }
    key
}

fn decode_group_conflict_component(value: &str) -> Result<String, String> {
    if value.len() % 2 != 0 {
        return Err("invalid dataset-group conflict key".to_string());
    }
    let mut bytes = Vec::with_capacity(value.len() / 2);
    let mut chars = value.as_bytes().chunks_exact(2);
    while let Some(pair) = chars.next() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid dataset-group conflict key".to_string())?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| "invalid dataset-group conflict key".to_string())?;
        bytes.push(((high << 4) | low) as u8);
    }
    String::from_utf8(bytes).map_err(|_| "invalid dataset-group conflict key encoding".to_string())
}

fn decode_group_conflict_key(
    key: &str,
) -> Result<(String, String, Option<String>), String> {
    let encoded = key
        .strip_prefix("group:v1:")
        .ok_or_else(|| "invalid dataset-group conflict key".to_string())?;
    let mut parts = encoded.split(':');
    let group_id = decode_group_conflict_component(
        parts
            .next()
            .ok_or_else(|| "invalid dataset-group conflict key".to_string())?,
    )?;
    let field = parts
        .next()
        .ok_or_else(|| "invalid dataset-group conflict key".to_string())?;
    if !matches!(field, "whole" | "name" | "member") {
        return Err("invalid dataset-group conflict field".to_string());
    }
    let member = match parts.next() {
        Some(value) if field == "member" => Some(decode_group_conflict_component(value)?),
        Some(_) => return Err("dataset-group conflict field does not accept a member".to_string()),
        None if field == "member" => {
            return Err("dataset-group member conflict is missing its dataset key".to_string())
        }
        None => None,
    };
    if parts.next().is_some() {
        return Err("invalid dataset-group conflict key".to_string());
    }
    Ok((group_id, field.to_string(), member))
}

fn group_value(group: Option<&DataProjectDatasetGroup>) -> Value {
    group
        .map(|group| serde_json::to_value(group).unwrap_or(Value::Null))
        .unwrap_or(Value::Null)
}

fn group_name_value(group: Option<&DataProjectDatasetGroup>) -> Value {
    group
        .map(|group| Value::String(group.name.clone()))
        .unwrap_or(Value::Null)
}

fn group_membership_value(
    group: Option<&DataProjectDatasetGroup>,
    dataset_key: &str,
) -> Value {
    Value::Bool(
        group
            .is_some_and(|group| group.dataset_keys.iter().any(|key| key == dataset_key)),
    )
}

fn dataset_group_assignment(
    metadata: &DataProjectMetadata,
    dataset_key: &str,
) -> Option<String> {
    metadata
        .dataset_groups
        .iter()
        .find(|group| group.dataset_keys.iter().any(|key| key == dataset_key))
        .map(|group| group.id.clone())
}

fn merge_project_metadata(
    base: &DataProjectMetadata,
    ours: &DataProjectMetadata,
    theirs: &DataProjectMetadata,
) -> (DataProjectMetadata, Vec<VersionConflict>) {
    let mut conflicts = Vec::new();
    let base_groups = base
        .dataset_groups
        .iter()
        .map(|group| (group.id.clone(), group))
        .collect::<BTreeMap<_, _>>();
    let ours_groups = ours
        .dataset_groups
        .iter()
        .map(|group| (group.id.clone(), group))
        .collect::<BTreeMap<_, _>>();
    let theirs_groups = theirs
        .dataset_groups
        .iter()
        .map(|group| (group.id.clone(), group))
        .collect::<BTreeMap<_, _>>();
    let mut group_ids = BTreeSet::new();
    group_ids.extend(base_groups.keys().cloned());
    group_ids.extend(ours_groups.keys().cloned());
    group_ids.extend(theirs_groups.keys().cloned());

    let mut selected_groups = BTreeMap::<String, DataProjectDatasetGroup>::new();
    for id in group_ids {
        let base_group = base_groups.get(&id).copied();
        let ours_group = ours_groups.get(&id).copied();
        let theirs_group = theirs_groups.get(&id).copied();
        let base_value = group_value(base_group);
        let ours_value = group_value(ours_group);
        let theirs_value = group_value(theirs_group);

        // Unchanged-vs-changed and identical edits can be selected as a
        // whole.  Divergent deletion versus modification is not decomposable;
        // keep ours as the provisional candidate and expose a whole-group
        // conflict for explicit resolution.
        if ours_value == theirs_value {
            if let Some(group) = ours_group.or(theirs_group).or(base_group) {
                selected_groups.insert(id, group.clone());
            }
            continue;
        }
        if ours_value == base_value {
            if let Some(group) = theirs_group {
                selected_groups.insert(id, group.clone());
            }
            continue;
        }
        if theirs_value == base_value {
            if let Some(group) = ours_group {
                selected_groups.insert(id, group.clone());
            }
            continue;
        }
        let (Some(ours_group), Some(theirs_group)) = (ours_group, theirs_group) else {
            conflicts.push(VersionConflict {
                key: group_conflict_key(&id, "whole", None),
                kind: "dataset-group".to_string(),
                base: (!base_value.is_null()).then_some(base_value),
                ours: (!ours_value.is_null()).then_some(ours_value),
                theirs: (!theirs_value.is_null()).then_some(theirs_value),
                display_name: Some(id.clone()),
                base_description: None,
                ours_description: None,
                theirs_description: None,
            });
            if let Some(group) = ours_group.or(theirs_group) {
                selected_groups.insert(id, group.clone());
            }
            continue;
        };

        let mut merged = ours_group.clone();
        let name = merge_optional_value(
            group_conflict_key(&id, "name", None),
            "dataset-group-name",
            Some(group_name_value(base_group)),
            Some(group_name_value(Some(ours_group))),
            Some(group_name_value(Some(theirs_group))),
            &mut conflicts,
        )
        .unwrap_or_else(|| group_name_value(Some(ours_group)));
        merged.name = name.as_str().unwrap_or(&ours_group.name).to_string();

        let mut member_keys = BTreeSet::new();
        for group in [base_group, Some(ours_group), Some(theirs_group)]
            .into_iter()
            .flatten()
        {
            member_keys.extend(group.dataset_keys.iter().cloned());
        }
        let mut selected_members = BTreeSet::new();
        for dataset_key in member_keys {
            let selected = merge_optional_value(
                group_conflict_key(&id, "member", Some(&dataset_key)),
                "dataset-group-membership",
                Some(group_membership_value(base_group, &dataset_key)),
                Some(group_membership_value(Some(ours_group), &dataset_key)),
                Some(group_membership_value(Some(theirs_group), &dataset_key)),
                &mut conflicts,
            )
            .unwrap_or_else(|| group_membership_value(Some(ours_group), &dataset_key));
            if selected.as_bool() == Some(true) {
                selected_members.insert(dataset_key);
            }
        }

        // Keep the target branch's member order, then append incoming-only
        // members.  The remaining BTreeSet path is only a defensive fallback
        // for malformed/legacy ordering and remains deterministic.
        merged.dataset_keys.clear();
        for dataset_key in ours_group
            .dataset_keys
            .iter()
            .chain(theirs_group.dataset_keys.iter())
        {
            if selected_members.remove(dataset_key) {
                merged.dataset_keys.push(dataset_key.clone());
            }
        }
        merged.dataset_keys.extend(selected_members);
        selected_groups.insert(id, merged);
    }

    // A dataset may only belong to one group in a valid tree. Membership
    // edits are otherwise independently mergeable, but moving the same
    // dataset to different groups on both branches is a real assignment
    // conflict: the per-group merge above would otherwise retain both groups
    // and the defensive dedupe in merge_snapshots would silently discard the
    // incoming choice. Keep the regular server-compatible member conflict
    // shape; the resolver uses both branch snapshots to move the dataset
    // atomically when the incoming side is selected.
    let mut assignment_dataset_keys = BTreeSet::new();
    for metadata in [base, ours, theirs] {
        for group in &metadata.dataset_groups {
            assignment_dataset_keys.extend(group.dataset_keys.iter().cloned());
        }
    }
    for dataset_key in assignment_dataset_keys {
        let base_assignment = dataset_group_assignment(base, &dataset_key);
        let ours_assignment = dataset_group_assignment(ours, &dataset_key);
        let theirs_assignment = dataset_group_assignment(theirs, &dataset_key);
        if ours_assignment == theirs_assignment
            || ours_assignment == base_assignment
            || theirs_assignment == base_assignment
        {
            continue;
        }
        let conflict_group_id = ours_assignment
            .clone()
            .or_else(|| theirs_assignment.clone());
        let Some(conflict_group_id) = conflict_group_id else {
            continue;
        };
        let key = group_conflict_key(&conflict_group_id, "member", Some(&dataset_key));
        if conflicts.iter().any(|conflict| conflict.key == key) {
            continue;
        }
        conflicts.push(VersionConflict {
            key,
            kind: "dataset-group-membership".to_string(),
            base: Some(Value::Bool(
                base_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
            ours: Some(Value::Bool(
                ours_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
            theirs: Some(Value::Bool(
                theirs_assignment.as_deref() == Some(conflict_group_id.as_str()),
            )),
            display_name: None,
            base_description: None,
            ours_description: None,
            theirs_description: None,
        });
    }

    // Keep the target (ours) order, then append groups introduced by the
    // incoming branch and finally any surviving base-only groups.  Positions
    // are normalized after selection so a merge is deterministic.
    let mut order = ours
        .dataset_groups
        .iter()
        .map(|group| group.id.clone())
        .collect::<Vec<_>>();
    for group in theirs.dataset_groups.iter().chain(base.dataset_groups.iter()) {
        if !order.contains(&group.id) {
            order.push(group.id.clone());
        }
    }
    let mut dataset_groups = Vec::new();
    for id in order {
        let Some(mut group) = selected_groups.remove(&id) else {
            continue;
        };
        group.position = dataset_groups.len() as i32;
        dataset_groups.push(group);
    }

    let mut extra = BTreeMap::new();
    let mut extra_keys = BTreeSet::new();
    extra_keys.extend(base.extra.keys().cloned());
    extra_keys.extend(ours.extra.keys().cloned());
    extra_keys.extend(theirs.extra.keys().cloned());
    for key in extra_keys {
        if let Some(value) = merge_optional_value(
            format!("projectMetadata:{key}"),
            "projectMetadata",
            base.extra.get(&key).cloned(),
            ours.extra.get(&key).cloned(),
            theirs.extra.get(&key).cloned(),
            &mut conflicts,
        ) {
            extra.insert(key, value);
        }
    }
    let schema_value = merge_optional_value(
        "projectMetadata:schemaVersion".to_string(),
        "projectMetadata",
        Some(Value::from(base.schema_version)),
        Some(Value::from(ours.schema_version)),
        Some(Value::from(theirs.schema_version)),
        &mut conflicts,
    )
    .and_then(|value| value.as_u64())
    .unwrap_or(1) as u32;
    (
        DataProjectMetadata {
            schema_version: schema_value,
            dataset_groups,
            extra,
        },
        conflicts,
    )
}

fn merge_snapshots(base: &Snapshot, ours: &Snapshot, theirs: &Snapshot) -> MergePlan {
    let mut keys = BTreeSet::new();
    keys.extend(base.entries.keys().cloned());
    keys.extend(ours.entries.keys().cloned());
    keys.extend(theirs.entries.keys().cloned());
    let mut merged_entries = BTreeMap::new();
    let mut conflicts = Vec::new();
    for key in keys {
        if let Some(entry) = merge_entry_fields(
            &key,
            base.entries.get(&key),
            ours.entries.get(&key),
            theirs.entries.get(&key),
            &mut conflicts,
        ) {
            merged_entries.insert(key, entry);
        }
    }
    let mut metadata = BTreeMap::new();
    let mut metadata_keys = BTreeSet::new();
    metadata_keys.extend(base.metadata.keys().cloned());
    metadata_keys.extend(ours.metadata.keys().cloned());
    metadata_keys.extend(theirs.metadata.keys().cloned());
    for key in metadata_keys {
        let selected = merge_optional_value(
            format!("metadata:{key}"),
            "metadata",
            base.metadata.get(&key).cloned().map(Value::String),
            ours.metadata.get(&key).cloned().map(Value::String),
            theirs.metadata.get(&key).cloned().map(Value::String),
            &mut conflicts,
        );
        if let Some(Value::String(value)) = selected {
            metadata.insert(key, value);
        }
    }
    let mut dataset_keys = BTreeSet::new();
    dataset_keys.extend(base.dataset_ids.iter().cloned());
    dataset_keys.extend(ours.dataset_ids.iter().cloned());
    dataset_keys.extend(theirs.dataset_ids.iter().cloned());
    let base_set = base.dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let ours_set = ours.dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let theirs_set = theirs.dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut dataset_ids = BTreeSet::new();
    for key in dataset_keys {
        let selected = merge_optional_value(
            format!("membership:{key}"),
            "membership",
            Some(Value::Bool(base_set.contains(&key))),
            Some(Value::Bool(ours_set.contains(&key))),
            Some(Value::Bool(theirs_set.contains(&key))),
            &mut conflicts,
        );
        if selected == Some(Value::Bool(true)) {
            dataset_ids.insert(key);
        }
    }
    let (mut project_metadata, project_metadata_conflicts) = merge_project_metadata(
        &base.project_metadata,
        &ours.project_metadata,
        &theirs.project_metadata,
    );
    // Membership changes and group edits are merged independently.  A
    // dataset removed from the merged snapshot must not remain referenced by
    // a group, and two branches assigning the same dataset to different
    // groups must still produce a valid single-owner grouping.  Target order
    // wins deterministically for this otherwise non-decomposable legacy
    // shape; explicit field conflicts remain available for same-group edits.
    let allowed_dataset_ids = dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut assigned_dataset_ids = BTreeSet::new();
    for group in &mut project_metadata.dataset_groups {
        group
            .dataset_keys
            .retain(|key| allowed_dataset_ids.contains(key) && assigned_dataset_ids.insert(key.clone()));
    }
    let _ = normalize_project_metadata(&mut project_metadata, None);
    conflicts.extend(project_metadata_conflicts);
    MergePlan {
        snapshot: Snapshot {
            dataset_ids: dataset_ids.into_iter().collect(),
            metadata,
            project_metadata,
            entries: merged_entries,
        },
        conflicts,
    }
}

fn short_hash(value: &str) -> String {
    value.chars().take(12).collect()
}

fn is_content_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn display_number(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_f64) else {
        return "?".to_string();
    };
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.1}")
    }
}

fn display_scalar(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "未设置".to_string(),
        Some(Value::Bool(true)) => "是".to_string(),
        Some(Value::Bool(false)) => "否".to_string(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::String(text)) if text.is_empty() => "空".to_string(),
        Some(Value::String(text)) if is_content_hash(text) => {
            format!("内容对象 {}…", short_hash(text))
        }
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(values)) => format!("{} 个值", values.len()),
        Some(Value::Object(_)) => "结构化内容".to_string(),
    }
}

fn annotation_object_description(value: &Value) -> String {
    let label = value
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.is_empty())
        .unwrap_or("未命名");
    let kind = value.get("type").and_then(Value::as_str).unwrap_or("形状");
    if let Some(bbox) = value.get("bbox").and_then(Value::as_object) {
        return format!(
            "{label} [{},{},{},{}]",
            display_number(bbox.get("x")),
            display_number(bbox.get("y")),
            display_number(bbox.get("width")),
            display_number(bbox.get("height"))
        );
    }
    let point_count = value
        .get("polygon")
        .or_else(|| value.get("points"))
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if point_count > 0 {
        return format!("{label} {kind}（{point_count} 个点）");
    }
    format!("{label} {kind}")
}

fn annotation_value_description(value: Option<&Value>) -> String {
    let Some(hash) = value.and_then(Value::as_str) else {
        return "删除标注（无标签）".to_string();
    };
    let Ok(bytes) = read_data_project_object(hash) else {
        return format!("标签内容已变更（对象 {}…）", short_hash(hash));
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        return format!("标签内容已变更（对象 {}…）", short_hash(hash));
    };
    let objects = document
        .as_array()
        .or_else(|| document.get("objects").and_then(Value::as_array));
    let Some(objects) = objects else {
        return "标签内容（格式未知）".to_string();
    };
    if objects.is_empty() {
        return "0 个标注（空标签）".to_string();
    }
    let mut descriptions = objects
        .iter()
        .take(3)
        .map(annotation_object_description)
        .collect::<Vec<_>>();
    if objects.len() > 3 {
        descriptions.push("…".to_string());
    }
    format!("{} 个标注：{}", objects.len(), descriptions.join("；"))
}

fn metadata_value_description(value: Option<&Value>) -> String {
    let Some(hash) = value.and_then(Value::as_str) else {
        return "删除数据集元数据".to_string();
    };
    let Ok(bytes) = read_data_project_object(hash) else {
        return format!("数据集元数据已变更（对象 {}…）", short_hash(hash));
    };
    let Ok(document) = serde_json::from_slice::<Value>(&bytes) else {
        return format!("数据集元数据已变更（对象 {}…）", short_hash(hash));
    };
    let labels = document
        .get("classes")
        .and_then(Value::as_array)
        .map(|classes| {
            classes
                .iter()
                .filter_map(|class| class.get("label").and_then(Value::as_str))
                .take(3)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if labels.is_empty() {
        "数据集元数据已变更".to_string()
    } else {
        format!("{} 个类别：{}", labels.len(), labels.join("、"))
    }
}

fn conflict_entry<'a>(
    snapshot: &'a Snapshot,
    conflict: &VersionConflict,
) -> Option<&'a VersionCommitEntry> {
    let raw = conflict.key.strip_prefix("entry:")?;
    let entry_key = if conflict.kind == "entry" {
        raw
    } else {
        raw.rsplit_once(':')?.0
    };
    snapshot.entries.get(entry_key)
}

fn conflict_field(conflict: &VersionConflict) -> Option<&str> {
    let raw = conflict.key.strip_prefix("entry:")?;
    if conflict.kind == "entry" {
        None
    } else {
        raw.rsplit_once(':').map(|(_, field)| field)
    }
}

fn image_value_description(
    field: Option<&str>,
    value: Option<&Value>,
    entry: Option<&VersionCommitEntry>,
) -> String {
    let file_name = entry
        .map(|entry| entry.file_name.as_str())
        .unwrap_or("图像");
    match field {
        Some("imageHash") => format!(
            "图像 {file_name}（{}×{}，内容已变更）",
            entry.map(|entry| entry.width).unwrap_or_default(),
            entry.map(|entry| entry.height).unwrap_or_default()
        ),
        Some("fileName") => format!(
            "文件名：{}（{}×{}）",
            display_scalar(value),
            entry.map(|entry| entry.width).unwrap_or_default(),
            entry.map(|entry| entry.height).unwrap_or_default()
        ),
        Some("width") | Some("height") => format!(
            "图像 {file_name} 尺寸：{}×{}",
            if field == Some("width") {
                display_scalar(value)
            } else {
                entry
                    .map(|entry| entry.width.to_string())
                    .unwrap_or_else(|| "?".to_string())
            },
            if field == Some("height") {
                display_scalar(value)
            } else {
                entry
                    .map(|entry| entry.height.to_string())
                    .unwrap_or_else(|| "?".to_string())
            }
        ),
        _ => format!("图像 {file_name}：{}", display_scalar(value)),
    }
}

fn conflict_side_description(
    conflict: &VersionConflict,
    value: Option<&Value>,
    entry: Option<&VersionCommitEntry>,
) -> String {
    match conflict.kind.as_str() {
        "annotation" => annotation_value_description(value),
        "image" => image_value_description(conflict_field(conflict), value, entry),
        "review" => {
            let field = if conflict_field(conflict) == Some("qaStatus") {
                "审核状态"
            } else {
                "处理状态"
            };
            format!("{field}：{}", display_scalar(value))
        }
        "split" => format!("数据划分：{}", display_scalar(value)),
        "entry" => {
            if let Some(value) = value
                .and_then(|value| serde_json::from_value::<VersionCommitEntry>(value.clone()).ok())
            {
                format!(
                    "图像 {}（{}×{}，{}）",
                    value.file_name,
                    value.width,
                    value.height,
                    annotation_value_description(Some(&Value::String(value.annotation_hash)))
                )
            } else {
                "删除图像条目".to_string()
            }
        }
        "membership" => {
            if value.and_then(Value::as_bool) == Some(true) {
                "归属该数据集".to_string()
            } else {
                "不归属该数据集".to_string()
            }
        }
        "metadata" => metadata_value_description(value),
        "dataset-group" | "dataset-group-name" | "dataset-group-membership" => {
            match decode_group_conflict_key(&conflict.key) {
                Ok((group_id, field, _)) if field == "name" => {
                    format!("分组 {group_id} 名称：{}", display_scalar(value))
                }
                Ok((group_id, field, Some(dataset_key))) if field == "member" => format!(
                    "分组 {group_id} · {dataset_key}：{}",
                    if value.and_then(Value::as_bool) == Some(true) {
                        "属于分组"
                    } else {
                        "不属于分组"
                    }
                ),
                Ok((group_id, _, _)) => {
                    format!("数据集分组 {group_id}：{}", display_scalar(value))
                }
                Err(_) => format!("数据集分组：{}", display_scalar(value)),
            }
        }
        _ => display_scalar(value),
    }
}

fn enrich_conflicts(
    conflicts: &mut [VersionConflict],
    base: &Snapshot,
    ours: &Snapshot,
    theirs: &Snapshot,
) {
    for conflict in conflicts {
        let base_entry = conflict_entry(base, conflict);
        let ours_entry = conflict_entry(ours, conflict);
        let theirs_entry = conflict_entry(theirs, conflict);
        let entry = ours_entry.or(theirs_entry).or(base_entry);
        let display_name = match conflict.kind.as_str() {
            "membership" => format!(
                "数据集归属 · {}",
                conflict.key.trim_start_matches("membership:")
            ),
            "metadata" => format!(
                "数据集元数据 · {}",
                conflict.key.trim_start_matches("metadata:")
            ),
            "annotation" => format!(
                "标注内容 · {}",
                entry
                    .map(|entry| entry.file_name.as_str())
                    .unwrap_or("未知图像")
            ),
            "review" => format!(
                "审核状态 · {}",
                entry
                    .map(|entry| entry.file_name.as_str())
                    .unwrap_or("未知图像")
            ),
            "split" => format!(
                "数据划分 · {}",
                entry
                    .map(|entry| entry.file_name.as_str())
                    .unwrap_or("未知图像")
            ),
            "image" => format!(
                "图像内容 · {}",
                entry
                    .map(|entry| entry.file_name.as_str())
                    .unwrap_or("未知图像")
            ),
            "dataset-group" => format!("数据集分组 · {}", conflict.key),
            "dataset-group-name" => format!("数据集分组名称 · {}", conflict.key),
            "dataset-group-membership" => format!("数据集分组归属 · {}", conflict.key),
            "entry" => format!(
                "图像条目 · {}",
                entry
                    .map(|entry| entry.file_name.as_str())
                    .unwrap_or("未知图像")
            ),
            _ => format!("冲突 · {}", conflict.key),
        };
        conflict.display_name = Some(display_name);
        conflict.base_description = Some(conflict_side_description(
            conflict,
            conflict.base.as_ref(),
            base_entry,
        ));
        conflict.ours_description = Some(conflict_side_description(
            conflict,
            conflict.ours.as_ref(),
            ours_entry,
        ));
        conflict.theirs_description = Some(conflict_side_description(
            conflict,
            conflict.theirs.as_ref(),
            theirs_entry,
        ));
    }
}

fn commit_snapshot(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    parent_id: &str,
    second_parent_id: Option<&str>,
    message: &str,
    snapshot: &Snapshot,
) -> Result<VersionCommit, String> {
    if message.trim().is_empty() || message.len() > 256 {
        return Err("invalid commit message".to_string());
    }
    let branch_row = branch_row(connection, project_id, branch)?;
    if branch_row.head_commit_id.as_deref() != Some(parent_id) {
        return Err("branch head changed; retry from the current head".to_string());
    }
    validate_membership_targets(connection, project_id, &snapshot.dataset_ids)?;
    let mut project_metadata = snapshot.project_metadata.clone();
    validate_project_metadata(&mut project_metadata, &snapshot.dataset_ids)?;
    let id = Uuid::new_v4().to_string();
    let created_at = now_string();
    let dataset_json =
        serde_json::to_string(&snapshot.dataset_ids).map_err(|error| error.to_string())?;
    let metadata_json =
        serde_json::to_string(&snapshot.metadata).map_err(|error| error.to_string())?;
    let project_metadata_json = serde_json::to_string(&project_metadata)
        .map_err(|error| format!("serialize project metadata: {error}"))?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    transaction.execute(
        "INSERT INTO data_project_commits (id, data_project_id, parent_id, parent2_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![id, project_id, parent_id, second_parent_id, message, created_at, dataset_json, metadata_json, project_metadata_json],
    ).map_err(|error| format!("create version commit: {error}"))?;
    transaction.execute("INSERT INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 0)", params![id, parent_id]).map_err(|error| error.to_string())?;
    if let Some(second) = second_parent_id {
        transaction.execute("INSERT INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 1)", params![id, second]).map_err(|error| error.to_string())?;
    }
    for hash in snapshot.metadata.values() {
        validate_object(hash)?;
    }
    for entry in snapshot.entries.values() {
        validate_object(&entry.image_hash)?;
        validate_object(&entry.annotation_hash)?;
        transaction.execute(
            "INSERT INTO data_project_commit_entries (commit_id, dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![id, entry.dataset_id, entry.image_id, entry.file_name, entry.image_hash, entry.annotation_hash, entry.status, entry.qa_status, entry.split, entry.width, entry.height],
        ).map_err(|error| error.to_string())?;
    }
    let now = now_string();
    let changed = transaction.execute(
        "UPDATE data_project_branches SET head_commit_id = ?3, updated_at = ?4 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id = ?5",
        params![project_id, branch, id, now, parent_id],
    ).map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("branch head changed during commit".to_string());
    }
    let active: String = transaction
        .query_row(
            "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if active == branch {
        transaction
            .execute(
                "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
                params![project_id],
            )
            .map_err(|error| error.to_string())?;
        for dataset_id in &snapshot.dataset_ids {
            transaction
                .execute(
                    "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3)",
                    params![dataset_id, project_id, now],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![project_id, id, now],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    commit_row(connection, project_id, &id, Some(branch.to_string()))
}

/// Commit a snapshot and materialize its controlled checkout as one logical
/// operation.  `commit_snapshot` must finish its SQLite transaction before it
/// can return the immutable commit, so a materialization failure is repaired
/// immediately in a second transaction: the branch ref, project HEAD,
/// membership rows, and just-created commit rows are restored to `parent_id`.
/// The checkout swap itself is staging/backup based and leaves the previous
/// checkout in place when any filesystem step fails.
fn commit_snapshot_and_materialize(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    parent_id: &str,
    second_parent_id: Option<&str>,
    message: &str,
    snapshot: &Snapshot,
    parent_snapshot: &Snapshot,
) -> Result<VersionCommit, String> {
    let commit = commit_snapshot(
        connection,
        project_id,
        branch,
        parent_id,
        second_parent_id,
        message,
        snapshot,
    )?;
    if let Err(error) = materialize_commit(connection, project_id, branch, Some(&commit.id)) {
        let rollback = rollback_failed_materialized_commit(
            connection,
            project_id,
            branch,
            &commit.id,
            parent_id,
            parent_snapshot,
        );
        return match rollback {
            Ok(()) => Err(format!("materialize data-project commit: {error}")),
            Err(rollback_error) => Err(format!(
                "materialize data-project commit: {error}; rollback failed: {rollback_error}"
            )),
        };
    }
    Ok(commit)
}

fn rollback_failed_materialized_commit(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    commit_id: &str,
    parent_id: &str,
    parent_snapshot: &Snapshot,
) -> Result<(), String> {
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let changed = transaction
        .execute(
            "UPDATE data_project_branches SET head_commit_id = ?3, updated_at = ?4 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id = ?5",
            params![project_id, branch, parent_id, now_string(), commit_id],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("branch head changed while rolling back materialization".to_string());
    }

    let active: String = transaction
        .query_row(
            "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if active == branch {
        transaction
            .execute(
                "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
                params![project_id],
            )
            .map_err(|error| error.to_string())?;
        for dataset_id in &parent_snapshot.dataset_ids {
            transaction
                .execute(
                    "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3)",
                    params![dataset_id, project_id, now_string()],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1",
                params![project_id, parent_id, now_string()],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "DELETE FROM data_project_commit_entries WHERE commit_id = ?1",
            params![commit_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM data_project_commit_parents WHERE commit_id = ?1",
            params![commit_id],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM data_project_commits WHERE id = ?1 AND data_project_id = ?2",
            params![commit_id, project_id],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

/// Commit the repository-backed working tree through the legacy capture path,
/// then attach that immutable commit to the selected branch.  Keeping this
/// adapter here means existing image/label validation and cloud identities stay
/// in one place while branch refs still use the same CAS rules as merges.
pub fn commit_data_project_branch(
    project_id: &str,
    branch: &str,
    message: &str,
) -> Result<VersionCommit, String> {
    let _operation_guard = operation_lock()?;
    let branch = check_name(branch, "branch name")?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    if active_branch_name(&connection, project_id)? != branch {
        return Err("branch must be active before committing".to_string());
    }
    let checkout_path = checkout_root(project_id, &branch);
    if checkout_dirty(&checkout_path)? {
        drop(connection);
        return commit_data_project_checkout_unlocked(project_id, &branch, message);
    }
    // A clean materialized checkout is already the authoritative branch
    // worktree.  Do not fall through to the source-backed adapter (which
    // would re-enter the public checkout commit while this operation lock is
    // held).
    if checkout_state_path(&checkout_path).is_file()
        && checkout_path.join("manifest.json").is_file()
    {
        return Err("working tree is clean; refusing empty data-project commit".to_string());
    }
    ensure_active_clean(&connection, project_id)?;
    let expected_head = branch_row(&connection, project_id, &branch)?.head_commit_id;
    let project_head: Option<String> = connection
        .query_row(
            "SELECT head_commit_id FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if expected_head != project_head {
        return Err("active branch and project HEAD are out of sync".to_string());
    }
    drop(connection);

    let legacy = crate::data_projects::commit_data_project(project_id, message)?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    // The legacy capture produced valid immutable objects but predates the
    // controlled worktree.  Materialize those objects before exposing the
    // branch ref so a branch commit API call immediately becomes the source
    // used by the annotation workbench.
    materialize_commit(&connection, project_id, &branch, Some(&legacy.id))?;
    let updated = connection
        .execute(
            "UPDATE data_project_branches SET head_commit_id = ?3, updated_at = ?4 WHERE data_project_id = ?1 AND name = ?2 AND head_commit_id IS ?5",
            params![project_id, branch, legacy.id, legacy.created_at, expected_head],
        )
        .map_err(|error| error.to_string())?;
    if updated != 1 {
        return Err("branch head changed while committing".to_string());
    }
    if let Some(parent) = legacy.parent_id.as_deref() {
        connection
            .execute(
                "INSERT OR IGNORE INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 0)",
                params![legacy.id, parent],
            )
            .map_err(|error| error.to_string())?;
    }
    commit_row(&connection, project_id, &legacy.id, Some(branch))
}

fn active_checkout(
    connection: &Connection,
    project_id: &str,
) -> Result<(String, DataProjectBranch), String> {
    let branch = active_branch_name(connection, project_id)?;
    let row = branch_row(connection, project_id, &branch)?;
    Ok((branch, row))
}

fn validate_membership_targets(
    connection: &Connection,
    project_id: &str,
    dataset_ids: &[String],
) -> Result<(), String> {
    let mut unique = BTreeSet::new();
    for dataset_id in dataset_ids {
        if !unique.insert(dataset_id) {
            return Err(format!("duplicate dataset in checkout: {dataset_id}"));
        }
        let owner: Option<String> = connection
            .query_row(
                "SELECT data_project_id FROM data_project_datasets WHERE dataset_id = ?1",
                params![dataset_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if let Some(owner) = owner {
            if owner != project_id {
                return Err(format!(
                    "dataset belongs to another data project: {dataset_id}"
                ));
            }
        }
    }
    Ok(())
}

fn dataset_has_unflushed_sync_state(dataset_id: &str) -> Result<bool, String> {
    let paths = project_fs::project_paths(dataset_id);
    if !paths.sqlite.is_file() {
        return Ok(false);
    }
    let connection = Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let has_outbox: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'sync_outbox')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if has_outbox {
        let pending: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_outbox WHERE status NOT IN ('synced', 'superseded'))",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if pending {
            return Ok(true);
        }
    }
    for table in ["projects", "images", "annotations"] {
        let has_table: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                params![table],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if has_table {
            let sql = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE dirty != 0)");
            let dirty: bool = connection
                .query_row(&sql, [], |row| row.get(0))
                .map_err(|error| error.to_string())?;
            if dirty {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn ensure_no_unflushed_sync_state(
    connection: &Connection,
    project_id: &str,
    dataset_ids: &[String],
) -> Result<(), String> {
    validate_membership_targets(connection, project_id, dataset_ids)?;
    for dataset_id in dataset_ids {
        if dataset_has_unflushed_sync_state(dataset_id)? {
            return Err(format!(
                "dataset has pending sync/outbox or offline drafts: {dataset_id}"
            ));
        }
    }
    Ok(())
}

fn tree_hash(root: &Path) -> Result<String, String> {
    let mut paths = Vec::new();
    if root.exists() {
        for item in walkdir::WalkDir::new(root) {
            let item = item.map_err(|error| error.to_string())?;
            if !item.file_type().is_file() || item.path() == checkout_state_path(root) {
                continue;
            }
            let relative = item
                .path()
                .strip_prefix(root)
                .map_err(|error| error.to_string())?;
            paths.push(relative.to_path_buf());
        }
    }
    paths.sort();
    let mut hasher = Sha256::new();
    for relative in paths {
        hasher.update(relative.to_string_lossy().as_bytes());
        let mut file = File::open(root.join(&relative)).map_err(|error| error.to_string())?;
        let mut buffer = vec![0u8; OBJECT_BUFFER_SIZE];
        loop {
            let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn checkout_dirty(root: &Path) -> Result<bool, String> {
    let state_path = checkout_state_path(root);
    if !state_path.exists() {
        return Ok(root.exists()
            && fs::read_dir(root)
                .map_err(|error| error.to_string())?
                .next()
                .is_some());
    }
    let state: CheckoutState =
        serde_json::from_slice(&fs::read(&state_path).map_err(|error| error.to_string())?)
            .map_err(|error| format!("invalid checkout state: {error}"))?;
    Ok(state.tree_hash != tree_hash(root)?)
}

fn ensure_active_clean(connection: &Connection, project_id: &str) -> Result<(), String> {
    let (branch, _) = active_checkout(connection, project_id)?;
    if checkout_dirty(&checkout_root(project_id, &branch))? {
        return Err("active checkout has uncommitted changes".to_string());
    }
    Ok(())
}

fn materialize_commit(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    commit_id: Option<&str>,
) -> Result<DataProjectCheckout, String> {
    let snapshot = snapshot_from_commit(connection, project_id, commit_id)?;
    let root = checkout_root(project_id, branch);
    let parent = root
        .parent()
        .ok_or_else(|| "invalid checkout root".to_string())?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".checkout-{}", Uuid::new_v4()));
    fs::create_dir_all(&temporary).map_err(|error| error.to_string())?;
    let result = (|| {
        let mut manifest_entries = Vec::new();
        for dataset_id in &snapshot.dataset_ids {
            if !path_is_safe_relative(dataset_id) {
                return Err(format!("unsafe checkout dataset id: {dataset_id}"));
            }
            let dataset_root = temporary.join("datasets").join(dataset_id);
            fs::create_dir_all(dataset_root.join("images")).map_err(|error| error.to_string())?;
            fs::create_dir_all(dataset_root.join("annotations"))
                .map_err(|error| error.to_string())?;
            if let Some(metadata_hash) = snapshot.metadata.get(dataset_id) {
                validate_object(metadata_hash)?;
                copy_verified_object(metadata_hash, &dataset_root.join("metadata.json"))?;
            }
        }
        for entry in snapshot.entries.values() {
            if !path_is_safe_relative(&entry.file_name) {
                return Err(format!("unsafe checkout file name: {}", entry.file_name));
            }
            validate_object(&entry.image_hash)?;
            validate_object(&entry.annotation_hash)?;
            if !path_is_safe_relative(&entry.dataset_id) || !path_is_safe_relative(&entry.image_id)
            {
                return Err("unsafe checkout dataset or image id".to_string());
            }
            let dataset_root = temporary.join("datasets").join(&entry.dataset_id);
            fs::create_dir_all(dataset_root.join("images")).map_err(|error| error.to_string())?;
            fs::create_dir_all(dataset_root.join("annotations"))
                .map_err(|error| error.to_string())?;
            copy_verified_object(
                &entry.image_hash,
                &dataset_root.join("images").join(&entry.file_name),
            )?;
            copy_verified_object(
                &entry.annotation_hash,
                &dataset_root
                    .join("annotations")
                    .join(format!("{}.json", entry.image_id)),
            )?;
            manifest_entries.push(entry);
        }
        let manifest = json!({ "projectId": project_id, "commitId": commit_id, "datasets": snapshot.dataset_ids, "metadata": snapshot.metadata, "projectMetadata": snapshot.project_metadata, "entries": manifest_entries });
        fs::write(
            temporary.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        let hash = tree_hash(&temporary)?;
        let state = CheckoutState {
            project_id: project_id.to_string(),
            branch: branch.to_string(),
            commit_id: commit_id.map(str::to_string),
            tree_hash: hash,
        };
        fs::write(
            checkout_state_path(&temporary),
            serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if root.exists() {
            let backup = parent.join(format!(".checkout-backup-{}", Uuid::new_v4()));
            fs::rename(&root, &backup).map_err(|error| error.to_string())?;
            if let Err(error) = fs::rename(&temporary, &root) {
                let _ = fs::rename(&backup, &root);
                return Err(error.to_string());
            }
            let _ = fs::remove_dir_all(&backup);
        } else {
            fs::rename(&temporary, &root).map_err(|error| error.to_string())?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&temporary);
    }
    result?;
    Ok(DataProjectCheckout {
        data_project_id: project_id.to_string(),
        branch: branch.to_string(),
        commit_id: commit_id.map(str::to_string),
        path: root.to_string_lossy().to_string(),
        dirty: false,
    })
}

fn snapshot_from_checkout(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    persist_objects: bool,
) -> Result<Snapshot, String> {
    let root = checkout_root(project_id, branch);
    let state: CheckoutState = serde_json::from_slice(
        &fs::read(checkout_state_path(&root)).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid checkout state: {error}"))?;
    if state.project_id != project_id || state.branch != branch {
        return Err("checkout belongs to another data project or branch".to_string());
    }
    let current_head = branch_row(connection, project_id, branch)?.head_commit_id;
    if state.commit_id != current_head {
        return Err("checkout is not based on the current branch head".to_string());
    }
    let manifest: CheckoutManifest = serde_json::from_slice(
        &fs::read(root.join("manifest.json")).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid checkout manifest: {error}"))?;
    if manifest.project_id != project_id || manifest.commit_id != current_head {
        return Err("checkout manifest is stale".to_string());
    }
    let mut dataset_ids = BTreeSet::new();
    for dataset_id in &manifest.datasets {
        if !path_is_safe_relative(dataset_id) || !dataset_ids.insert(dataset_id.clone()) {
            return Err(format!(
                "invalid or duplicate checkout dataset: {dataset_id}"
            ));
        }
    }
    let mut metadata = BTreeMap::new();
    for dataset_id in &manifest.datasets {
        let dataset_root = root.join("datasets").join(dataset_id);
        let metadata_path = dataset_root.join("metadata.json");
        let hash = if metadata_path.exists() {
            if persist_objects {
                store_file_object(&metadata_path)?.0
            } else {
                hash_file(&metadata_path)?.0
            }
        } else {
            manifest
                .metadata
                .get(dataset_id)
                .cloned()
                .ok_or_else(|| format!("checkout metadata missing for dataset {dataset_id}"))?
        };
        validate_object(&hash)?;
        metadata.insert(dataset_id.clone(), hash);
    }
    for key in manifest.metadata.keys() {
        if !dataset_ids.contains(key) {
            return Err(format!("checkout metadata has unknown dataset: {key}"));
        }
    }
    let mut entries = BTreeMap::new();
    for mut entry in manifest.entries {
        if !dataset_ids.contains(&entry.dataset_id)
            || !path_is_safe_relative(&entry.dataset_id)
            || !path_is_safe_relative(&entry.image_id)
            || !path_is_safe_relative(&entry.file_name)
        {
            return Err(format!("invalid checkout entry: {}", entry.image_id));
        }
        let image_path = root
            .join("datasets")
            .join(&entry.dataset_id)
            .join("images")
            .join(&entry.file_name);
        let annotation_path = root
            .join("datasets")
            .join(&entry.dataset_id)
            .join("annotations")
            .join(format!("{}.json", entry.image_id));
        entry.image_hash = if persist_objects {
            store_file_object(&image_path)?.0
        } else {
            hash_file(&image_path)?.0
        };
        let (annotation_hash, _) = if persist_objects {
            store_file_object(&annotation_path)?
        } else {
            hash_file(&annotation_path)?
        };
        let annotation_bytes = fs::read(&annotation_path).map_err(|error| error.to_string())?;
        serde_json::from_slice::<Value>(&annotation_bytes)
            .map_err(|error| format!("invalid checkout annotation {}: {error}", entry.image_id))?;
        entry.annotation_hash = annotation_hash;
        let key = entry_key(&entry);
        if entries.insert(key, entry).is_some() {
            return Err("checkout contains duplicate dataset/image entry".to_string());
        }
    }
    metadata_snapshot_authorized(connection, project_id, &metadata)?;
    let mut project_metadata = manifest.project_metadata;
    let dataset_ids = dataset_ids.into_iter().collect::<Vec<_>>();
    validate_project_metadata(&mut project_metadata, &dataset_ids)?;
    Ok(Snapshot {
        dataset_ids,
        metadata,
        project_metadata,
        entries,
    })
}

fn checkout_status_bucket(status: &str, qa_status: &str) -> &'static str {
    let status = status.trim().to_ascii_lowercase();
    let qa_status = qa_status.trim().to_ascii_lowercase();
    if matches!(
        qa_status.as_str(),
        "approved" | "pass" | "passed" | "通过" | "已通过" | "ok"
    ) {
        "approved"
    } else if matches!(
        qa_status.as_str(),
        "rejected" | "reject" | "failed" | "不通过" | "已驳回"
    ) {
        "rejected"
    } else if matches!(
        qa_status.as_str(),
        "pending" | "pendingreview" | "pending_review" | "待审核" | "待复核"
    ) {
        "pendingReview"
    } else if matches!(status.as_str(), "raw" | "unannotated" | "未标注" | "") {
        "raw"
    } else {
        "inProgress"
    }
}

fn snapshot_changes(
    current: &Snapshot,
    baseline: &Snapshot,
) -> Vec<crate::data_projects::DataProjectChange> {
    let mut changes = Vec::new();
    let current_datasets = current.dataset_ids.iter().collect::<BTreeSet<_>>();
    let baseline_datasets = baseline.dataset_ids.iter().collect::<BTreeSet<_>>();
    for dataset_id in current_datasets.difference(&baseline_datasets) {
        changes.push(crate::data_projects::DataProjectChange {
            kind: "dataset-added".to_string(),
            dataset_id: dataset_id.to_string(),
            image_id: None,
            file_name: None,
            detail: "dataset added to branch working tree".to_string(),
        });
    }
    for dataset_id in baseline_datasets.difference(&current_datasets) {
        changes.push(crate::data_projects::DataProjectChange {
            kind: "dataset-removed".to_string(),
            dataset_id: dataset_id.to_string(),
            image_id: None,
            file_name: None,
            detail: "dataset removed from branch working tree".to_string(),
        });
    }
    let mut keys = BTreeSet::new();
    keys.extend(current.entries.keys().cloned());
    keys.extend(baseline.entries.keys().cloned());
    for key in keys {
        let current_entry = current.entries.get(&key);
        let baseline_entry = baseline.entries.get(&key);
        let Some(entry) = current_entry.or(baseline_entry) else {
            continue;
        };
        let mut kinds = Vec::new();
        match (baseline_entry, current_entry) {
            (None, Some(_)) => kinds.push("image-added"),
            (Some(_), None) => kinds.push("image-removed"),
            (Some(before), Some(after)) => {
                if before.image_hash != after.image_hash {
                    kinds.push("image-changed");
                }
                if before.annotation_hash != after.annotation_hash {
                    kinds.push("annotation-changed");
                }
                if before.status != after.status || before.qa_status != after.qa_status {
                    kinds.push("review-changed");
                }
                if before.split != after.split {
                    kinds.push("split-changed");
                }
            }
            _ => {}
        }
        for kind in kinds {
            changes.push(crate::data_projects::DataProjectChange {
                kind: kind.to_string(),
                dataset_id: entry.dataset_id.clone(),
                image_id: Some(entry.image_id.clone()),
                file_name: Some(entry.file_name.clone()),
                detail: "branch checkout differs from HEAD".to_string(),
            });
        }
    }
    let mut metadata_keys = BTreeSet::new();
    metadata_keys.extend(current.metadata.keys().cloned());
    metadata_keys.extend(baseline.metadata.keys().cloned());
    for dataset_id in metadata_keys {
        if current.metadata.get(&dataset_id) != baseline.metadata.get(&dataset_id) {
            changes.push(crate::data_projects::DataProjectChange {
                kind: "dataset-metadata-changed".to_string(),
                dataset_id,
                image_id: None,
                file_name: None,
                detail: "dataset classes or manifest changed".to_string(),
            });
        }
    }
    if current.project_metadata != baseline.project_metadata {
        changes.push(crate::data_projects::DataProjectChange {
            kind: "project-metadata-changed".to_string(),
            dataset_id: String::new(),
            image_id: None,
            file_name: None,
            detail: "data-project dataset groups or metadata changed".to_string(),
        });
    }
    changes
}

/// Read the branch's controlled worktree without persisting objects.  This is
/// the adapter used by the annotation UI after branch checkout; it keeps the
/// legacy source dataset and its cloud IDs untouched while exposing real
/// branch changes to the existing working-tree contract.
pub fn get_data_project_checkout_working_tree(
    project_id: &str,
) -> Result<crate::data_projects::DataProjectWorkingTree, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    let branch = active_branch_name(&connection, project_id)?;
    let head = branch_row(&connection, project_id, &branch)?.head_commit_id;
    let Some(head_id) = head.as_deref() else {
        return Err("active branch has no checkout commit".to_string());
    };
    let root = checkout_root(project_id, &branch);
    let tree_before = tree_hash(&root)?;
    let current = snapshot_from_checkout(&connection, project_id, &branch, false)?;
    let tree_after = tree_hash(&root)?;
    if tree_before != tree_after {
        return Err("data-project checkout changed while capturing working tree".to_string());
    }
    let baseline = snapshot_from_commit(&connection, project_id, Some(head_id))?;
    let mut counts = crate::data_projects::DataProjectCounts::default();
    for entry in current.entries.values() {
        counts.total += 1;
        match checkout_status_bucket(&entry.status, &entry.qa_status) {
            "raw" => counts.raw += 1,
            "inProgress" => counts.in_progress += 1,
            "pendingReview" => counts.pending_review += 1,
            "approved" => counts.approved += 1,
            "rejected" => counts.rejected += 1,
            _ => {}
        }
    }
    Ok(crate::data_projects::DataProjectWorkingTree {
        data_project_id: project_id.to_string(),
        counts,
        changes: snapshot_changes(&current, &baseline),
        head_commit_id: Some(head_id.to_string()),
    })
}

pub fn get_data_project_versioned_working_tree(
    project_id: &str,
) -> Result<crate::data_projects::DataProjectWorkingTree, String> {
    // The public status route must also support source-backed projects and
    // a freshly imported graph whose managed checkout has not been created.
    // The shared entry point materializes an existing HEAD before capture.
    crate::data_projects::get_data_project_working_tree(project_id)
}

/// Return the materialized checkout for the active branch, if this project
/// has one.  A project may have a legacy/source-backed HEAD without a
/// checkout directory; callers must keep using the source-backed path in that
/// case.  A present but malformed checkout is an error and is never silently
/// treated as a clean source tree.
pub fn active_data_project_checkout(
    project_id: &str,
) -> Result<Option<DataProjectCheckout>, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let branch = active_branch_name(&connection, project_id)?;
    let target = branch_row(&connection, project_id, &branch)?;
    let Some(commit_id) = target.head_commit_id else {
        return Ok(None);
    };
    let root = checkout_root(project_id, &branch);
    if !checkout_state_path(&root).is_file() || !root.join("manifest.json").is_file() {
        return Ok(None);
    }
    let state: CheckoutState = serde_json::from_slice(
        &fs::read(checkout_state_path(&root)).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid checkout state: {error}"))?;
    if state.project_id != project_id || state.branch != branch {
        return Err("checkout belongs to another data project or branch".to_string());
    }
    if state.commit_id.as_deref() != Some(commit_id.as_str()) {
        if checkout_dirty(&root)? {
            return Err("active checkout is stale and has uncommitted changes".to_string());
        }
        // A clean checkout can be refreshed by
        // `materialize_active_data_project_checkout`; do not report it as the
        // current worktree merely because its old manifest still exists.
        return Ok(None);
    }
    Ok(Some(DataProjectCheckout {
        data_project_id: project_id.to_string(),
        branch,
        commit_id: Some(commit_id),
        path: root.to_string_lossy().to_string(),
        dirty: checkout_dirty(&root)?,
    }))
}

/// Materialize the current branch head when a legacy/source-backed commit has
/// not yet had a controlled checkout.  Objects are only read and verified;
/// this never rewrites the source dataset or creates a second object copy.
pub fn materialize_active_data_project_checkout(
    project_id: &str,
) -> Result<Option<DataProjectCheckout>, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let branch = active_branch_name(&connection, project_id)?;
    let target = branch_row(&connection, project_id, &branch)?;
    let Some(commit_id) = target.head_commit_id else {
        return Ok(None);
    };
    let root = checkout_root(project_id, &branch);
    if checkout_state_path(&root).is_file() && root.join("manifest.json").is_file() {
        let state: CheckoutState = serde_json::from_slice(
            &fs::read(checkout_state_path(&root)).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("invalid checkout state: {error}"))?;
        if state.project_id != project_id || state.branch != branch {
            return Err("checkout belongs to another data project or branch".to_string());
        }
        if state.commit_id.as_deref() == Some(commit_id.as_str()) {
            return Ok(Some(DataProjectCheckout {
                data_project_id: project_id.to_string(),
                branch,
                commit_id: Some(commit_id),
                path: root.to_string_lossy().to_string(),
                dirty: checkout_dirty(&root)?,
            }));
        }
        if checkout_dirty(&root)? {
            return Err("active checkout is stale and has uncommitted changes".to_string());
        }
    }
    if let Some(parent) = root.parent() {
        // The clone path has no checkout directory yet. Create the parent
        // explicitly before staging so first materialization does not depend
        // on a prior branch switch having created it.
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    materialize_commit(&connection, project_id, &branch, Some(&commit_id)).map(Some)
}

/// Mark a dataset removal in the active controlled worktree.  The immutable
/// parent commit remains untouched; the next commit then records the
/// membership/image removal as a normal branch change.  Orphaned files are
/// left under the private checkout until that commit materializes a clean
/// tree, so a failed write cannot damage an external dataset.
pub fn remove_dataset_from_active_data_project_checkout(
    project_id: &str,
    dataset_id: &str,
) -> Result<bool, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let branch = active_branch_name(&connection, project_id)?;
    let Some(_head) = branch_row(&connection, project_id, &branch)?.head_commit_id else {
        return Ok(false);
    };
    let root = checkout_root(project_id, &branch);
    let manifest_path = root.join("manifest.json");
    if !manifest_path.is_file() {
        return Ok(false);
    }
    let original = fs::read(&manifest_path).map_err(|error| error.to_string())?;
    let mut manifest: Value = serde_json::from_slice(&original)
        .map_err(|error| format!("invalid active checkout manifest: {error}"))?;
    let had_dataset = manifest
        .get("datasets")
        .and_then(Value::as_array)
        .is_some_and(|datasets| {
            datasets
                .iter()
                .any(|value| value.as_str() == Some(dataset_id))
        });
    if !had_dataset {
        return Ok(false);
    }
    if let Some(datasets) = manifest.get_mut("datasets").and_then(Value::as_array_mut) {
        datasets.retain(|value| value.as_str() != Some(dataset_id));
    }
    if let Some(metadata) = manifest.get_mut("metadata").and_then(Value::as_object_mut) {
        metadata.remove(dataset_id);
    }
    if let Some(entries) = manifest.get_mut("entries").and_then(Value::as_array_mut) {
        entries.retain(|entry| entry.get("datasetId").and_then(Value::as_str) != Some(dataset_id));
    }
    if let Some(groups) = manifest
        .get_mut("projectMetadata")
        .and_then(|value| value.get_mut("datasetGroups"))
        .and_then(Value::as_array_mut)
    {
        for group in groups {
            if let Some(keys) = group.get_mut("datasetKeys").and_then(Value::as_array_mut) {
                keys.retain(|key| key.as_str() != Some(dataset_id));
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    atomic_write_file(&manifest_path, &bytes).map_err(|error| {
        let _ = atomic_write_file(&manifest_path, &original);
        error
    })?;
    Ok(true)
}

fn assigned_project_for_dataset(
    connection: &Connection,
    dataset_id: &str,
) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT data_project_id FROM data_project_datasets WHERE dataset_id = ?1",
            params![dataset_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn active_checkout_for_dataset(
    connection: &Connection,
    dataset_id: &str,
) -> Result<Option<(String, String)>, String> {
    let Some(project_id) = assigned_project_for_dataset(connection, dataset_id)? else {
        return Ok(None);
    };
    crate::data_projects::ensure_dataset_visible_to_current_account(dataset_id)?;
    history_scope_authorized(connection, &project_id)?;
    ensure_default_branch(connection, &project_id)?;
    let branch = active_branch_name(connection, &project_id)?;
    let head = branch_row(connection, &project_id, &branch)?.head_commit_id;
    let Some(_head) = head else {
        return Ok(None);
    };
    let root = checkout_root(&project_id, &branch);
    if !checkout_state_path(&root).is_file() || !root.join("manifest.json").is_file() {
        return Ok(None);
    }
    // Validate the checkout before handing a path to the workbench.  This
    // keeps a damaged branch from falling back to the external source tree.
    let _ = snapshot_from_checkout(connection, &project_id, &branch, false)?;
    Ok(Some((project_id, branch)))
}

fn active_checkout_path_for_dataset(
    connection: &Connection,
    dataset_id: &str,
    image_id: &str,
    annotations: bool,
) -> Result<Option<PathBuf>, String> {
    let Some((project_id, branch)) = active_checkout_for_dataset(connection, dataset_id)? else {
        return Ok(None);
    };
    let root = checkout_root(&project_id, &branch);
    let snapshot = snapshot_from_checkout(connection, &project_id, &branch, false)?;
    let entry = snapshot
        .entries
        .values()
        .find(|entry| entry.dataset_id == dataset_id && entry.image_id == image_id)
        .ok_or_else(|| {
            format!(
                "image {dataset_id}:{image_id} is not present in the active data-project checkout"
            )
        })?;
    let path = if annotations {
        root.join("datasets")
            .join(dataset_id)
            .join("annotations")
            .join(format!("{}.json", entry.image_id))
    } else {
        root.join("datasets")
            .join(dataset_id)
            .join("images")
            .join(&entry.file_name)
    };
    if !path.is_file() {
        return Err(format!(
            "active checkout file is missing: {}",
            path.display()
        ));
    }
    Ok(Some(path))
}

/// Resolve an image in the active data-project checkout using only the
/// dataset id that the legacy workbench already carries.  `None` means this
/// dataset is not currently backed by a materialized project checkout.
pub fn active_data_project_checkout_image_path_for_dataset(
    dataset_id: &str,
    image_id: &str,
) -> Result<Option<PathBuf>, String> {
    let connection = open_db()?;
    active_checkout_path_for_dataset(&connection, dataset_id, image_id, false)
}

pub fn active_data_project_checkout_annotation_path_for_dataset(
    dataset_id: &str,
    image_id: &str,
) -> Result<Option<PathBuf>, String> {
    let connection = open_db()?;
    active_checkout_path_for_dataset(&connection, dataset_id, image_id, true)
}

pub fn read_active_data_project_checkout_annotation_for_dataset(
    dataset_id: &str,
    image_id: &str,
) -> Result<Option<Value>, String> {
    let Some(path) =
        active_data_project_checkout_annotation_path_for_dataset(dataset_id, image_id)?
    else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|error| {
        format!(
            "read active checkout annotation {}: {error}",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("invalid active checkout annotation: {error}"))
}

pub fn active_data_project_checkout_entries(
    dataset_id: &str,
) -> Result<Option<Vec<VersionCommitEntry>>, String> {
    let connection = open_db()?;
    let Some((project_id, branch)) = active_checkout_for_dataset(&connection, dataset_id)? else {
        return Ok(None);
    };
    let snapshot = snapshot_from_checkout(&connection, &project_id, &branch, false)?;
    Ok(Some(
        snapshot
            .entries
            .into_values()
            .filter(|entry| entry.dataset_id == dataset_id)
            .collect(),
    ))
}

pub fn active_data_project_checkout_metadata(dataset_id: &str) -> Result<Option<Value>, String> {
    let connection = open_db()?;
    let Some((project_id, branch)) = active_checkout_for_dataset(&connection, dataset_id)? else {
        return Ok(None);
    };
    let snapshot = snapshot_from_checkout(&connection, &project_id, &branch, false)?;
    let Some(expected_hash) = snapshot.metadata.get(dataset_id) else {
        return Err(format!(
            "active checkout metadata is missing for dataset {dataset_id}"
        ));
    };
    let path = checkout_root(&project_id, &branch)
        .join("datasets")
        .join(dataset_id)
        .join("metadata.json");
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let mut cursor = Cursor::new(bytes.as_slice());
    let (actual_hash, _) = hash_reader(&mut cursor)?;
    if &actual_hash != expected_hash {
        return Err(format!(
            "active checkout metadata hash mismatch: {dataset_id}"
        ));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("invalid active checkout metadata: {error}"))
}

fn active_checkout_annotation_value(
    connection: &Connection,
    dataset_id: &str,
    image_id: &str,
) -> Result<Option<(PathBuf, Value)>, String> {
    let Some(path) = active_checkout_path_for_dataset(connection, dataset_id, image_id, true)?
    else {
        return Ok(None);
    };
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid active checkout annotation: {error}"))?;
    Ok(Some((path, value)))
}

fn atomic_write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = parent.join(format!(".write-{}", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(&temporary, path).map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

/// Save a canonical annotation wrapper into the controlled checkout.  The
/// wrapper contains objects plus review/category metadata, so a save remains
/// branch-local and is captured by the next immutable commit.  `None` means
/// the dataset is source-backed and the caller should use the legacy storage
/// path.  The checkout's state hash is intentionally not advanced: a save is
/// a real dirty worktree change until the user commits it.
pub fn write_active_data_project_checkout_annotation(
    dataset_id: &str,
    image_id: &str,
    expected_revision: Option<&str>,
    mut value: Value,
    status: &str,
    qa_status: &str,
    review_note: Option<&str>,
    verified: Option<bool>,
) -> Result<Option<CheckoutAnnotationWriteResult>, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    let Some((path, current)) =
        active_checkout_annotation_value(&connection, dataset_id, image_id)?
    else {
        return Ok(None);
    };
    let Some(project_id) = assigned_project_for_dataset(&connection, dataset_id)? else {
        return Err("dataset is no longer assigned to a data project".to_string());
    };
    let branch = active_branch_name(&connection, &project_id)?;
    let current_revision = current
        .get("revision")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(expected) = expected_revision {
        if current_revision.as_deref() != Some(expected) {
            return Err(format!(
                "annotation revision changed for {dataset_id}:{image_id}"
            ));
        }
    }
    if !value.is_object() {
        return Err("active checkout annotation must be a JSON object".to_string());
    }
    let revision = format!("branch-rev-{}", Uuid::new_v4());
    let saved_at = now_string();
    value["revision"] = Value::String(revision.clone());
    value["status"] = Value::String(status.to_string());
    value["qaStatus"] = Value::String(qa_status.to_string());
    value["updatedAt"] = Value::String(saved_at.clone());
    if let Some(verified) = verified {
        value["verified"] = Value::Bool(verified);
    }
    value["reviewNote"] = review_note
        .map(|note| Value::String(note.to_string()))
        .unwrap_or(Value::Null);
    let bytes = serde_json::to_vec_pretty(&value).map_err(|error| error.to_string())?;
    let (annotation_hash, _) = {
        let mut cursor = Cursor::new(bytes.as_slice());
        hash_reader(&mut cursor)?
    };
    let original_bytes = fs::read(&path).map_err(|error| error.to_string())?;
    let manifest_path = checkout_root(&project_id, &branch).join("manifest.json");
    let original_manifest = fs::read(&manifest_path).map_err(|error| error.to_string())?;
    let mut manifest: Value = serde_json::from_slice(&original_manifest)
        .map_err(|error| format!("invalid active checkout manifest: {error}"))?;
    let entry = manifest
        .get_mut("entries")
        .and_then(Value::as_array_mut)
        .and_then(|entries| {
            entries.iter_mut().find(|entry| {
                entry.get("datasetId").and_then(Value::as_str) == Some(dataset_id)
                    && entry.get("imageId").and_then(Value::as_str) == Some(image_id)
            })
        })
        .ok_or_else(|| format!("active checkout entry is missing: {dataset_id}:{image_id}"))?;
    entry["annotationHash"] = Value::String(annotation_hash);
    entry["status"] = Value::String(status.to_string());
    entry["qaStatus"] = Value::String(qa_status.to_string());
    entry["reviewNote"] = review_note
        .map(|note| Value::String(note.to_string()))
        .unwrap_or(Value::Null);
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?;
    if let Err(error) = atomic_write_file(&path, &bytes) {
        return Err(error);
    }
    if let Err(error) = atomic_write_file(&manifest_path, &manifest_bytes) {
        let _ = atomic_write_file(&path, &original_bytes);
        let _ = atomic_write_file(&manifest_path, &original_manifest);
        return Err(error);
    }
    Ok(Some(CheckoutAnnotationWriteResult {
        revision,
        saved_at: saved_at.clone(),
        audit_event_id: format!("branch-audit-{}", Uuid::new_v4()),
    }))
}

pub fn update_active_data_project_checkout_image_state(
    dataset_id: &str,
    image_id: &str,
    status: Option<&str>,
    qa_status: Option<&str>,
    review_note: Option<Option<&str>>,
    verified: Option<bool>,
) -> Result<bool, String> {
    let connection = open_db()?;
    let Some((_path, mut value)) =
        active_checkout_annotation_value(&connection, dataset_id, image_id)?
    else {
        return Ok(false);
    };
    let current_status = value
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("草稿")
        .to_string();
    let current_qa = value
        .get("qaStatus")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let current_note = value
        .get("reviewNote")
        .and_then(Value::as_str)
        .map(str::to_string);
    let note = match review_note {
        Some(note) => note.map(str::to_string),
        None => current_note,
    };
    value["status"] = Value::String(status.unwrap_or(&current_status).to_string());
    value["qaStatus"] = Value::String(qa_status.unwrap_or(&current_qa).to_string());
    let result = write_active_data_project_checkout_annotation(
        dataset_id,
        image_id,
        None,
        value,
        status.unwrap_or(&current_status),
        qa_status.unwrap_or(&current_qa),
        note.as_deref(),
        verified,
    )?;
    Ok(result.is_some())
}

fn checkout_entry_path(
    connection: &Connection,
    project_id: &str,
    dataset_id: &str,
    image_id: &str,
    annotations: bool,
) -> Result<PathBuf, String> {
    if !path_is_safe_relative(dataset_id) || !path_is_safe_relative(image_id) {
        return Err("invalid checkout dataset or image id".to_string());
    }
    let branch = active_branch_name(connection, project_id)?;
    let root = checkout_root(project_id, &branch);
    let snapshot = snapshot_from_checkout(connection, project_id, &branch, false)?;
    let entry = snapshot
        .entries
        .values()
        .find(|entry| entry.dataset_id == dataset_id && entry.image_id == image_id)
        .ok_or_else(|| "image is not present in the active checkout".to_string())?;
    let path = if annotations {
        root.join("datasets")
            .join(dataset_id)
            .join("annotations")
            .join(format!("{}.json", entry.image_id))
    } else {
        root.join("datasets")
            .join(dataset_id)
            .join("images")
            .join(&entry.file_name)
    };
    Ok(path)
}

pub fn active_data_project_checkout_image_path(
    project_id: &str,
    dataset_id: &str,
    image_id: &str,
) -> Result<String, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    Ok(
        checkout_entry_path(&connection, project_id, dataset_id, image_id, false)?
            .to_string_lossy()
            .to_string(),
    )
}

pub fn active_data_project_checkout_annotation_path(
    project_id: &str,
    dataset_id: &str,
    image_id: &str,
) -> Result<String, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    Ok(
        checkout_entry_path(&connection, project_id, dataset_id, image_id, true)?
            .to_string_lossy()
            .to_string(),
    )
}

pub fn read_active_data_project_checkout_annotation(
    project_id: &str,
    dataset_id: &str,
    image_id: &str,
) -> Result<Value, String> {
    let _operation_guard = operation_lock()?;
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    let path = checkout_entry_path(&connection, project_id, dataset_id, image_id, true)?;
    let bytes = fs::read(&path).map_err(|error| error.to_string())?;
    serde_json::from_slice(&bytes).map_err(|error| format!("invalid checkout annotation: {error}"))
}

fn update_checkout_commit_state(
    project_id: &str,
    branch: &str,
    commit_id: &str,
) -> Result<(), String> {
    let root = checkout_root(project_id, branch);
    let manifest_path = root.join("manifest.json");
    let mut manifest: Value =
        serde_json::from_slice(&fs::read(&manifest_path).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?;
    manifest["commitId"] = Value::String(commit_id.to_string());
    let temporary = manifest_path.with_extension(format!("commit-{}", Uuid::new_v4()));
    fs::write(
        &temporary,
        serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::rename(&temporary, &manifest_path).map_err(|error| error.to_string())?;
    let state = CheckoutState {
        project_id: project_id.to_string(),
        branch: branch.to_string(),
        commit_id: Some(commit_id.to_string()),
        tree_hash: tree_hash(&root)?,
    };
    fs::write(
        checkout_state_path(&root),
        serde_json::to_vec_pretty(&state).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn commit_data_project_checkout_unlocked(
    project_id: &str,
    branch: &str,
    message: &str,
) -> Result<VersionCommit, String> {
    let branch = check_name(branch, "branch name")?;
    if message.trim().is_empty() || message.len() > 256 {
        return Err("invalid commit message".to_string());
    }
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    if active_branch_name(&connection, project_id)? != branch {
        return Err("branch must be active before committing checkout changes".to_string());
    }
    let current = snapshot_from_checkout(&connection, project_id, &branch, true)?;
    let verification = snapshot_from_checkout(&connection, project_id, &branch, false)?;
    if current != verification {
        return Err("data-project checkout changed while archiving; commit aborted".to_string());
    }
    let parent_id = branch_row(&connection, project_id, &branch)?
        .head_commit_id
        .ok_or_else(|| "branch has no commit to update".to_string())?;
    let parent = snapshot_from_commit(&connection, project_id, Some(&parent_id))?;
    if current == parent {
        return Err("working tree is clean; refusing empty data-project commit".to_string());
    }
    let commit = commit_snapshot(
        &connection,
        project_id,
        &branch,
        &parent_id,
        None,
        message,
        &current,
    )?;
    update_checkout_commit_state(project_id, &branch, &commit.id)?;
    Ok(commit)
}

pub fn commit_data_project_checkout(
    project_id: &str,
    branch: &str,
    message: &str,
) -> Result<VersionCommit, String> {
    let _operation_guard = operation_lock()?;
    commit_data_project_checkout_unlocked(project_id, branch, message)
}

pub fn switch_data_project_branch(
    project_id: &str,
    branch: &str,
) -> Result<DataProjectCheckout, String> {
    let _operation_guard = operation_lock()?;
    let branch = check_name(branch, "branch name")?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    ensure_active_clean(&connection, project_id)?;
    let target = branch_row(&connection, project_id, &branch)?;
    if checkout_dirty(&checkout_root(project_id, &branch))? {
        return Err("target checkout has uncommitted changes".to_string());
    }
    let target_snapshot =
        snapshot_from_commit(&connection, project_id, target.head_commit_id.as_deref())?;
    let active_name = active_branch_name(&connection, project_id)?;
    let active_head = branch_row(&connection, project_id, &active_name)?.head_commit_id;
    let active_snapshot = snapshot_from_commit(&connection, project_id, active_head.as_deref())?;
    ensure_no_unflushed_sync_state(&connection, project_id, &active_snapshot.dataset_ids)?;
    ensure_no_unflushed_sync_state(&connection, project_id, &target_snapshot.dataset_ids)?;
    let checkout = materialize_commit(
        &connection,
        project_id,
        &branch,
        target.head_commit_id.as_deref(),
    )?;
    let transaction = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let now = now_string();
    transaction
        .execute(
            "DELETE FROM data_project_datasets WHERE data_project_id = ?1",
            params![project_id],
        )
        .map_err(|error| error.to_string())?;
    for dataset_id in &target_snapshot.dataset_ids {
        transaction
            .execute(
                "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3)",
                params![dataset_id, project_id, now],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction
        .execute(
            "INSERT INTO data_project_active_branches (data_project_id, name, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT(data_project_id) DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
            params![project_id, branch, now],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![project_id, target.head_commit_id, now],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(checkout)
}

pub fn data_project_checkout_status(
    project_id: &str,
    branch: Option<&str>,
) -> Result<DataProjectCheckout, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    let branch = if let Some(branch) = branch {
        check_name(branch, "branch name")?
    } else {
        active_branch_name(&connection, project_id)?
    };
    let target = branch_row(&connection, project_id, &branch)?;
    let root = checkout_root(project_id, &branch);
    Ok(DataProjectCheckout {
        data_project_id: project_id.to_string(),
        branch,
        commit_id: target.head_commit_id,
        path: root.to_string_lossy().to_string(),
        dirty: checkout_dirty(&root)?,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectDatasetGroupsState {
    pub data_project_id: String,
    pub branch: String,
    /// Branch-local mutable worktree revision.  This is deliberately numeric
    /// to match the web worktree contract; it is a CAS token, not a commit ID.
    pub revision: i64,
    pub project_metadata: DataProjectMetadata,
    pub can_edit: bool,
}

fn read_checkout_project_metadata(
    connection: &Connection,
    project_id: &str,
    branch: &str,
    head: Option<&str>,
) -> Result<Option<DataProjectMetadata>, String> {
    let root = checkout_root(project_id, branch);
    let manifest_path = root.join("manifest.json");
    let state_path = checkout_state_path(&root);
    if !manifest_path.is_file() || !state_path.is_file() {
        return Ok(None);
    }
    let state: CheckoutState = serde_json::from_slice(
        &fs::read(&state_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid checkout state: {error}"))?;
    if state.project_id != project_id || state.branch != branch || state.commit_id.as_deref() != head {
        return Ok(None);
    }
    let value: Value = serde_json::from_slice(
        &fs::read(&manifest_path).map_err(|error| error.to_string())?,
    )
    .map_err(|error| format!("invalid checkout manifest: {error}"))?;
    if value.get("projectId").and_then(Value::as_str) != Some(project_id)
        || value.get("commitId").and_then(Value::as_str) != head
    {
        return Err("checkout manifest is stale".to_string());
    }
    let datasets = value
        .get("datasets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect::<Vec<_>>();
    let metadata = value
        .get("projectMetadata")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let mut metadata: DataProjectMetadata = serde_json::from_value(metadata)
        .map_err(|error| format!("invalid checkout project metadata: {error}"))?;
    validate_project_metadata(&mut metadata, &datasets)?;
    // Ensure this helper is never used to bypass the checkout's scope checks.
    let _ = connection;
    Ok(Some(metadata))
}

fn branch_project_metadata(
    connection: &Connection,
    project_id: &str,
    branch: &str,
) -> Result<(Option<String>, i64, DataProjectMetadata), String> {
    let branch_row = branch_row(connection, project_id, branch)?;
    let head = branch_row.head_commit_id.clone();
    let revision: i64 = connection
        .query_row(
            "SELECT revision FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
            params![project_id, branch],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if let Some(metadata) = read_checkout_project_metadata(
        connection,
        project_id,
        branch,
        head.as_deref(),
    )? {
        return Ok((head, revision, metadata));
    }
    let metadata = snapshot_from_commit(connection, project_id, head.as_deref())?.project_metadata;
    Ok((head, revision, metadata))
}

pub fn get_data_project_dataset_groups(
    project_id: &str,
    branch: Option<&str>,
) -> Result<DataProjectDatasetGroupsState, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let branch = if let Some(branch) = branch {
        check_name(branch, "branch name")?
    } else {
        active_branch_name(&connection, project_id)?
    };
    let (_, revision, project_metadata) = branch_project_metadata(&connection, project_id, &branch)?;
    Ok(DataProjectDatasetGroupsState {
        data_project_id: project_id.to_string(),
        branch,
        revision,
        project_metadata,
        can_edit: true,
    })
}

pub fn set_data_project_dataset_groups(
    project_id: &str,
    branch: Option<&str>,
    expected_revision: i64,
    groups: Vec<DataProjectDatasetGroup>,
) -> Result<DataProjectDatasetGroupsState, String> {
    let _operation_guard = operation_lock()?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    let branch = if let Some(branch) = branch {
        check_name(branch, "branch name")?
    } else {
        active_branch_name(&connection, project_id)?
    };
    if active_branch_name(&connection, project_id)? != branch {
        return Err("dataset groups can only be edited on the active branch".to_string());
    }
    let mut head = branch_row(&connection, project_id, &branch)?.head_commit_id;
    // A legacy/source-backed project may be grouped before its first explicit
    // version commit. Capture that source tree as the immutable baseline first
    // so setting groups never replaces real images/labels with an empty
    // checkout. Empty projects still use an empty checkout below.
    if head.is_none() {
        let dataset_count: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM data_project_datasets WHERE data_project_id = ?1",
                params![project_id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if dataset_count > 0 {
            drop(connection);
            crate::data_projects::commit_data_project(project_id, "Initial data-project snapshot")?;
            connection = open_db()?;
            history_scope_authorized(&connection, project_id)?;
            ensure_default_branch(&connection, project_id)?;
            head = branch_row(&connection, project_id, &branch)?.head_commit_id;
        }
    }
    let (_, revision, mut current_metadata) = branch_project_metadata(&connection, project_id, &branch)?;
    if revision != expected_revision {
        return Err(format!(
            "data-project worktree revision changed: expected {expected_revision}, actual {revision}"
        ));
    }
    let dataset_ids = snapshot_from_commit(&connection, project_id, head.as_deref())?.dataset_ids;
    let mut next_metadata = DataProjectMetadata {
        schema_version: 1,
        dataset_groups: groups,
        extra: std::mem::take(&mut current_metadata.extra),
    };
    validate_project_metadata(&mut next_metadata, &dataset_ids)?;
    let root = checkout_root(project_id, &branch);
    if head.is_none() {
        materialize_commit(&connection, project_id, &branch, None)?;
    } else if read_checkout_project_metadata(&connection, project_id, &branch, head.as_deref())?.is_none() {
        materialize_commit(&connection, project_id, &branch, head.as_deref())?;
    }
    let manifest_path = root.join("manifest.json");
    let original = fs::read(&manifest_path).map_err(|error| error.to_string())?;
    let mut manifest: Value = serde_json::from_slice(&original)
        .map_err(|error| format!("invalid active checkout manifest: {error}"))?;
    manifest["projectMetadata"] = serde_json::to_value(&next_metadata)
        .map_err(|error| format!("encode project metadata: {error}"))?;
    atomic_write_file(
        &manifest_path,
        &serde_json::to_vec_pretty(&manifest).map_err(|error| error.to_string())?,
    )?;
    let updated = connection
        .execute(
            "UPDATE data_project_branches SET revision = revision + 1, updated_at = ?3 WHERE data_project_id = ?1 AND name = ?2 AND revision = ?4",
            params![project_id, branch, now_string(), expected_revision],
        )
        .map_err(|error| error.to_string())?;
    if updated != 1 {
        let _ = atomic_write_file(&manifest_path, &original);
        return Err("data-project worktree revision changed; retry".to_string());
    }
    let next_revision = revision.saturating_add(1);
    Ok(DataProjectDatasetGroupsState {
        data_project_id: project_id.to_string(),
        branch,
        revision: next_revision,
        project_metadata: next_metadata,
        can_edit: true,
    })
}

pub fn checkout_data_project_branch(
    project_id: &str,
    branch: &str,
) -> Result<DataProjectCheckout, String> {
    switch_data_project_branch(project_id, branch)
}

pub fn delete_data_project_branch(project_id: &str, branch: &str) -> Result<(), String> {
    let _operation_guard = operation_lock()?;
    let branch = check_name(branch, "branch name")?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    if branch == "main" {
        return Err("cannot delete the protected main branch".to_string());
    }
    let active = active_branch_name(&connection, project_id)?;
    if active == branch {
        return Err("cannot delete the active branch".to_string());
    }
    if checkout_dirty(&checkout_root(project_id, &branch))? {
        return Err("cannot delete a branch with uncommitted checkout changes".to_string());
    }
    let target = branch_row(&connection, project_id, &branch)?;
    if let Some(head) = target.head_commit_id.as_deref() {
        let branches = list_data_project_branches(project_id)?;
        let mut merged = false;
        for item in branches.iter().filter(|item| item.name != branch) {
            if let Some(descendant) = item.head_commit_id.as_deref() {
                if is_ancestor(&connection, head, descendant)? {
                    merged = true;
                    break;
                }
            }
        }
        if !merged {
            return Err("branch must be merged before deletion".to_string());
        }
    }
    connection
        .execute(
            "DELETE FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
            params![project_id, branch],
        )
        .map_err(|error| error.to_string())?;
    let root = checkout_root(project_id, &branch);
    if root.exists() {
        fs::remove_dir_all(root).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn is_ancestor(connection: &Connection, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let mut queue = VecDeque::from([descendant.to_string()]);
    let mut seen = BTreeSet::new();
    while let Some(commit_id) = queue.pop_front() {
        if !seen.insert(commit_id.clone()) {
            continue;
        }
        if commit_id == ancestor {
            return Ok(true);
        }
        queue.extend(commit_parent_ids(connection, &commit_id)?);
    }
    Ok(false)
}

fn create_merge_session(
    connection: &Connection,
    project_id: &str,
    ours_branch: &str,
    theirs_branch: &str,
    base: Option<&str>,
    ours: &str,
    theirs: &str,
    message: &str,
    plan: &MergePlan,
) -> Result<MergeResult, String> {
    let id = Uuid::new_v4().to_string();
    let now = now_string();
    let session = connection
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    session.execute("INSERT INTO data_project_merge_sessions (id, data_project_id, ours_branch, theirs_branch, base_commit_id, ours_commit_id, theirs_commit_id, message, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'conflicted', ?9)", params![id, project_id, ours_branch, theirs_branch, base, ours, theirs, message, now]).map_err(|error| error.to_string())?;
    for conflict in &plan.conflicts {
        session.execute("INSERT INTO data_project_merge_conflicts (session_id, conflict_key, kind, base_json, ours_json, theirs_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![id, conflict.key, conflict.kind, conflict.base.as_ref().map(|value| value.to_string()), conflict.ours.as_ref().map(|value| value.to_string()), conflict.theirs.as_ref().map(|value| value.to_string())]).map_err(|error| error.to_string())?;
    }
    session.commit().map_err(|error| error.to_string())?;
    Ok(MergeResult {
        session_id: Some(id),
        commit: None,
        conflicts: plan.conflicts.clone(),
    })
}

pub fn preview_data_project_merge(
    project_id: &str,
    ours_branch: &str,
    theirs_branch: &str,
) -> Result<MergeResult, String> {
    let _operation_guard = operation_lock()?;
    let ours_branch = check_name(ours_branch, "ours branch")?;
    let theirs_branch = check_name(theirs_branch, "theirs branch")?;
    if ours_branch == theirs_branch {
        return Err("cannot merge a branch into itself".to_string());
    }
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    ensure_active_clean(&connection, project_id)?;
    if active_branch_name(&connection, project_id)? != ours_branch {
        return Err("ours branch must be the active branch".to_string());
    }
    let ours = branch_row(&connection, project_id, &ours_branch)?;
    let theirs = branch_row(&connection, project_id, &theirs_branch)?;
    let ours_id = ours
        .head_commit_id
        .ok_or_else(|| "ours branch has no commit".to_string())?;
    let theirs_id = theirs
        .head_commit_id
        .ok_or_else(|| "theirs branch has no commit".to_string())?;
    let base_id = common_ancestor(&connection, &ours_id, &theirs_id)?;
    let base = snapshot_from_commit(&connection, project_id, base_id.as_deref())?;
    let ours_snapshot = snapshot_from_commit(&connection, project_id, Some(&ours_id))?;
    let theirs_snapshot = snapshot_from_commit(&connection, project_id, Some(&theirs_id))?;
    ensure_no_unflushed_sync_state(&connection, project_id, &ours_snapshot.dataset_ids)?;
    ensure_no_unflushed_sync_state(&connection, project_id, &theirs_snapshot.dataset_ids)?;
    let mut plan = merge_snapshots(&base, &ours_snapshot, &theirs_snapshot);
    enrich_conflicts(&mut plan.conflicts, &base, &ours_snapshot, &theirs_snapshot);
    Ok(MergeResult {
        session_id: None,
        commit: None,
        conflicts: plan.conflicts,
    })
}

pub fn merge_data_project_branches(
    project_id: &str,
    ours_branch: &str,
    theirs_branch: &str,
    message: &str,
) -> Result<MergeResult, String> {
    let _operation_guard = operation_lock()?;
    let ours_branch = check_name(ours_branch, "ours branch")?;
    let theirs_branch = check_name(theirs_branch, "theirs branch")?;
    if ours_branch == theirs_branch {
        return Err("cannot merge a branch into itself".to_string());
    }
    if message.trim().is_empty() || message.len() > 256 {
        return Err("invalid commit message".to_string());
    }
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_default_branch(&connection, project_id)?;
    ensure_active_clean(&connection, project_id)?;
    if active_branch_name(&connection, project_id)? != ours_branch {
        return Err("ours branch must be the active branch".to_string());
    }
    let ours = branch_row(&connection, project_id, &ours_branch)?;
    let theirs = branch_row(&connection, project_id, &theirs_branch)?;
    let ours_id = ours
        .head_commit_id
        .ok_or_else(|| "ours branch has no commit".to_string())?;
    let theirs_id = theirs
        .head_commit_id
        .ok_or_else(|| "theirs branch has no commit".to_string())?;
    let base_id = common_ancestor(&connection, &ours_id, &theirs_id)?;
    let base = snapshot_from_commit(&connection, project_id, base_id.as_deref())?;
    let ours_snapshot = snapshot_from_commit(&connection, project_id, Some(&ours_id))?;
    let theirs_snapshot = snapshot_from_commit(&connection, project_id, Some(&theirs_id))?;
    ensure_no_unflushed_sync_state(&connection, project_id, &ours_snapshot.dataset_ids)?;
    ensure_no_unflushed_sync_state(&connection, project_id, &theirs_snapshot.dataset_ids)?;
    let mut plan = merge_snapshots(&base, &ours_snapshot, &theirs_snapshot);
    if !plan.conflicts.is_empty() {
        enrich_conflicts(&mut plan.conflicts, &base, &ours_snapshot, &theirs_snapshot);
        return create_merge_session(
            &connection,
            project_id,
            &ours_branch,
            &theirs_branch,
            base_id.as_deref(),
            &ours_id,
            &theirs_id,
            message,
            &plan,
        );
    }
    let commit = commit_snapshot_and_materialize(
        &connection,
        project_id,
        &ours_branch,
        &ours_id,
        Some(&theirs_id),
        message,
        &plan.snapshot,
        &ours_snapshot,
    )?;
    Ok(MergeResult {
        session_id: None,
        commit: Some(commit),
        conflicts: Vec::new(),
    })
}

fn load_merge_session(
    connection: &Connection,
    project_id: &str,
    session_id: &str,
) -> Result<(DataProjectMergeSession, BTreeMap<String, String>), String> {
    let (session, resolutions): (DataProjectMergeSession, String) = connection.query_row("SELECT id, ours_branch, theirs_branch, base_commit_id, ours_commit_id, theirs_commit_id, message, status, created_at, resolutions_json FROM data_project_merge_sessions WHERE id = ?1 AND data_project_id = ?2", params![session_id, project_id], |row| Ok((DataProjectMergeSession { id: row.get(0)?, data_project_id: project_id.to_string(), ours_branch: row.get(1)?, theirs_branch: row.get(2)?, base_commit_id: row.get(3)?, ours_commit_id: row.get(4)?, theirs_commit_id: row.get(5)?, message: row.get(6)?, status: row.get(7)?, created_at: row.get(8)? }, row.get(9)?))).map_err(|error| error.to_string())?;
    let resolutions = serde_json::from_str(&resolutions).map_err(|error| error.to_string())?;
    Ok((session, resolutions))
}

pub fn resolve_data_project_merge(
    project_id: &str,
    session_id: &str,
    conflict_key: &str,
    resolution: &str,
) -> Result<MergeResult, String> {
    let _operation_guard = operation_lock()?;
    if !matches!(resolution, "ours" | "theirs") {
        return Err("resolution must be ours or theirs".to_string());
    }
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_active_clean(&connection, project_id)?;
    let (session, mut resolutions) = load_merge_session(&connection, project_id, session_id)?;
    if session.status != "conflicted" {
        return Err("merge session is no longer active".to_string());
    }
    if active_branch_name(&connection, project_id)? != session.ours_branch {
        return Err("merge ours branch is no longer active".to_string());
    }
    let known: Option<i64> = connection.query_row("SELECT 1 FROM data_project_merge_conflicts WHERE session_id = ?1 AND conflict_key = ?2", params![session_id, conflict_key], |row| row.get(0)).optional().map_err(|error| error.to_string())?;
    if known.is_none() {
        return Err("merge conflict not found".to_string());
    }
    resolutions.insert(conflict_key.to_string(), resolution.to_string());
    let base = snapshot_from_commit(&connection, project_id, session.base_commit_id.as_deref())?;
    let ours = snapshot_from_commit(&connection, project_id, Some(&session.ours_commit_id))?;
    let theirs = snapshot_from_commit(&connection, project_id, Some(&session.theirs_commit_id))?;
    ensure_no_unflushed_sync_state(&connection, project_id, &ours.dataset_ids)?;
    ensure_no_unflushed_sync_state(&connection, project_id, &theirs.dataset_ids)?;
    let mut plan = merge_snapshots(&base, &ours, &theirs);
    let unresolved = plan
        .conflicts
        .iter()
        .filter(|conflict| !resolutions.contains_key(&conflict.key))
        .count();
    if unresolved > 0 {
        enrich_conflicts(&mut plan.conflicts, &base, &ours, &theirs);
        connection
            .execute(
                "UPDATE data_project_merge_sessions SET resolutions_json = ?2 WHERE id = ?1",
                params![
                    session_id,
                    serde_json::to_string(&resolutions).map_err(|error| error.to_string())?
                ],
            )
            .map_err(|error| error.to_string())?;
        return Ok(MergeResult {
            session_id: Some(session_id.to_string()),
            commit: None,
            conflicts: plan.conflicts,
        });
    }
    let mut resolved_snapshot = plan.snapshot;
    for conflict in &plan.conflicts {
        let selected = if resolutions.get(&conflict.key).map(String::as_str) == Some("ours") {
            conflict.ours.clone()
        } else {
            conflict.theirs.clone()
        };
        match conflict.kind.as_str() {
            "entry" => {
                if let Some(entry) = value_entry(selected.as_ref()) {
                    resolved_snapshot
                        .entries
                        .insert(conflict.key.trim_start_matches("entry:").to_string(), entry);
                } else {
                    resolved_snapshot
                        .entries
                        .remove(conflict.key.trim_start_matches("entry:"));
                }
            }
            "image" | "annotation" | "review" | "split" => {
                let raw_key = conflict
                    .key
                    .strip_prefix("entry:")
                    .ok_or_else(|| format!("invalid entry conflict key: {}", conflict.key))?;
                let (entry_key, field) = raw_key
                    .rsplit_once(':')
                    .ok_or_else(|| format!("invalid entry field conflict key: {}", conflict.key))?;
                let entry = resolved_snapshot
                    .entries
                    .get_mut(entry_key)
                    .ok_or_else(|| {
                        format!("merged entry is missing for conflict: {}", conflict.key)
                    })?;
                apply_entry_conflict_value(entry, field, selected)?;
            }
            "metadata" => {
                let key = conflict.key.trim_start_matches("metadata:");
                if let Some(Value::String(value)) = selected {
                    resolved_snapshot.metadata.insert(key.to_string(), value);
                } else {
                    resolved_snapshot.metadata.remove(key);
                }
            }
            "membership" => {
                let key = conflict.key.trim_start_matches("membership:");
                if selected == Some(Value::Bool(true)) {
                    if !resolved_snapshot.dataset_ids.contains(&key.to_string()) {
                        resolved_snapshot.dataset_ids.push(key.to_string());
                    }
                } else {
                    resolved_snapshot.dataset_ids.retain(|item| item != key);
                }
            }
            "dataset-group" | "dataset-group-name" | "dataset-group-membership" => {
                let (group_id, field, member_key) = if conflict.key.starts_with("group:v1:") {
                    decode_group_conflict_key(&conflict.key)?
                } else if let Some(group_id) = conflict.key.strip_prefix("group:") {
                    // Merge sessions created before the field-level group
                    // format was introduced remain resolvable as whole-group
                    // conflicts.
                    (group_id.to_string(), "whole".to_string(), None)
                } else {
                    return Err(format!("invalid dataset-group conflict key: {}", conflict.key));
                };
                if field == "member" {
                    let dataset_key = member_key.as_deref().ok_or_else(|| {
                        "dataset-group member conflict is missing its dataset key".to_string()
                    })?;
                    let base_assignment =
                        dataset_group_assignment(&base.project_metadata, dataset_key);
                    let ours_assignment =
                        dataset_group_assignment(&ours.project_metadata, dataset_key);
                    let theirs_assignment =
                        dataset_group_assignment(&theirs.project_metadata, dataset_key);
                    let assignment_conflict = ours_assignment != theirs_assignment
                        && ours_assignment != base_assignment
                        && theirs_assignment != base_assignment;
                    if assignment_conflict {
                        let selected_assignment = if resolutions
                            .get(&conflict.key)
                            .map(String::as_str)
                            == Some("ours")
                        {
                            ours_assignment
                        } else {
                            theirs_assignment
                        };
                        for group in &mut resolved_snapshot.project_metadata.dataset_groups {
                            group.dataset_keys.retain(|key| key != dataset_key);
                        }
                        if let Some(selected_group_id) = selected_assignment {
                            let group = resolved_snapshot
                                .project_metadata
                                .dataset_groups
                                .iter_mut()
                                .find(|group| group.id == selected_group_id)
                                .ok_or_else(|| {
                                    format!("dataset group is missing: {selected_group_id}")
                                })?;
                            group.dataset_keys.push(dataset_key.to_string());
                        }
                        normalize_project_metadata(&mut resolved_snapshot.project_metadata, None)?;
                        continue;
                    }
                }
                if field == "whole" {
                    resolved_snapshot
                        .project_metadata
                        .dataset_groups
                        .retain(|group| group.id != group_id);
                    if let Some(value) = selected {
                        let mut group: DataProjectDatasetGroup = serde_json::from_value(value)
                            .map_err(|error| format!("invalid dataset group resolution: {error}"))?;
                        if group.id != group_id {
                            return Err("dataset-group resolution has a different group ID".to_string());
                        }
                        group.position = resolved_snapshot.project_metadata.dataset_groups.len() as i32;
                        resolved_snapshot.project_metadata.dataset_groups.push(group);
                    }
                } else {
                    let group = resolved_snapshot
                        .project_metadata
                        .dataset_groups
                        .iter_mut()
                        .find(|group| group.id == group_id)
                        .ok_or_else(|| format!("dataset group is missing: {group_id}"))?;
                    match field.as_str() {
                        "name" => {
                            group.name = selected
                                .and_then(|value| value.as_str().map(str::to_string))
                                .ok_or_else(|| "invalid dataset-group name resolution".to_string())?;
                        }
                        "member" => {
                            let dataset_key = member_key.ok_or_else(|| {
                                "dataset-group member conflict is missing its dataset key".to_string()
                            })?;
                            if selected.and_then(|value| value.as_bool()) == Some(true) {
                                if !group.dataset_keys.iter().any(|key| key == &dataset_key) {
                                    group.dataset_keys.push(dataset_key);
                                }
                            } else {
                                group.dataset_keys.retain(|key| key != &dataset_key);
                            }
                        }
                        _ => return Err("unsupported dataset-group conflict field".to_string()),
                    }
                }
                normalize_project_metadata(&mut resolved_snapshot.project_metadata, None)?;
            }
            "projectMetadata" => {
                if let Some(group_id) = conflict.key.strip_prefix("group:") {
                    resolved_snapshot
                        .project_metadata
                        .dataset_groups
                        .retain(|group| group.id != group_id);
                    if let Some(value) = selected {
                        let mut group: DataProjectDatasetGroup = serde_json::from_value(value)
                            .map_err(|error| format!("invalid dataset group resolution: {error}"))?;
                        group.position = resolved_snapshot.project_metadata.dataset_groups.len() as i32;
                        resolved_snapshot.project_metadata.dataset_groups.push(group);
                    }
                    normalize_project_metadata(&mut resolved_snapshot.project_metadata, None)?;
                } else if let Some(key) = conflict.key.strip_prefix("projectMetadata:") {
                    if key == "schemaVersion" {
                        resolved_snapshot.project_metadata.schema_version = selected
                            .and_then(|value| value.as_u64())
                            .ok_or_else(|| "invalid project metadata schema resolution".to_string())?
                            as u32;
                    } else if let Some(value) = selected {
                        resolved_snapshot
                            .project_metadata
                            .extra
                            .insert(key.to_string(), value);
                    } else {
                        resolved_snapshot.project_metadata.extra.remove(key);
                    }
                }
            }
            _ => {}
        }
    }
    resolved_snapshot.dataset_ids.sort();
    validate_project_metadata(
        &mut resolved_snapshot.project_metadata,
        &resolved_snapshot.dataset_ids,
    )?;
    let commit = commit_snapshot_and_materialize(
        &connection,
        project_id,
        &session.ours_branch,
        &session.ours_commit_id,
        Some(&session.theirs_commit_id),
        &session.message,
        &resolved_snapshot,
        &ours,
    )?;
    connection.execute("UPDATE data_project_merge_sessions SET status = 'resolved', resolutions_json = ?2 WHERE id = ?1", params![session_id, serde_json::to_string(&resolutions).map_err(|error| error.to_string())?]).map_err(|error| error.to_string())?;
    plan.conflicts.clear();
    Ok(MergeResult {
        session_id: None,
        commit: Some(commit),
        conflicts: Vec::new(),
    })
}

pub fn revert_data_project_commit(
    project_id: &str,
    branch: &str,
    commit_id: &str,
    message: &str,
) -> Result<VersionCommit, String> {
    let _operation_guard = operation_lock()?;
    let branch = check_name(branch, "branch name")?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    ensure_active_clean(&connection, project_id)?;
    if active_branch_name(&connection, project_id)? != branch {
        return Err("branch must be active before reverting".to_string());
    }
    let branch_row = branch_row(&connection, project_id, &branch)?;
    let head = branch_row
        .head_commit_id
        .ok_or_else(|| "branch has no commit".to_string())?;
    validate_commit_exists(&connection, project_id, commit_id)?;
    // The user-facing action is a snapshot restore, not a Git inverse patch:
    // selected history (including the initial commit) becomes the content of
    // a new child commit while the current HEAD remains its sole parent.
    let target = snapshot_from_commit(&connection, project_id, Some(commit_id))?;
    let current = snapshot_from_commit(&connection, project_id, Some(&head))?;
    if target == current {
        return Err(
            "working tree is already at the selected commit; refusing empty restore commit"
                .to_string(),
        );
    }
    ensure_no_unflushed_sync_state(&connection, project_id, &current.dataset_ids)?;
    ensure_no_unflushed_sync_state(&connection, project_id, &target.dataset_ids)?;
    commit_snapshot_and_materialize(
        &connection,
        project_id,
        &branch,
        &head,
        None,
        message,
        &target,
        &current,
    )
}

pub fn export_data_project_graph(project_id: &str) -> Result<DataProjectGraph, String> {
    let connection = open_db()?;
    history_scope_authorized(&connection, project_id)?;
    let branches = list_data_project_branches(project_id)?;
    let mut commit_ids = BTreeSet::new();
    for branch in &branches {
        let mut queue = branch
            .head_commit_id
            .clone()
            .into_iter()
            .collect::<VecDeque<_>>();
        while let Some(commit_id) = queue.pop_front() {
            if !commit_ids.insert(commit_id.clone()) {
                continue;
            }
            queue.extend(commit_parent_ids(&connection, &commit_id)?);
        }
    }
    let mut commits = Vec::with_capacity(commit_ids.len());
    for commit_id in commit_ids {
        let commit = commit_row(&connection, project_id, &commit_id, None)?;
        for hash in commit.dataset_metadata.values() {
            validate_object(hash)?;
        }
        for entry in &commit.entries {
            validate_object(&entry.image_hash)?;
            validate_object(&entry.annotation_hash)?;
        }
        commits.push(commit);
    }
    commits.sort_by(|left, right| {
        left.created_at
            .cmp(&right.created_at)
            .then(left.id.cmp(&right.id))
    });
    let mut object_hashes = BTreeSet::new();
    for commit in &commits {
        object_hashes.extend(commit.dataset_metadata.values().cloned());
        for entry in &commit.entries {
            object_hashes.insert(entry.image_hash.clone());
            object_hashes.insert(entry.annotation_hash.clone());
        }
    }
    Ok(DataProjectGraph {
        data_project_id: project_id.to_string(),
        branches,
        commits,
        object_hashes: object_hashes.into_iter().collect(),
    })
}

pub fn export_data_project_graph_bundle(
    project_id: &str,
) -> Result<DataProjectGraphBundle, String> {
    export_data_project_graph(project_id)
}

pub fn import_data_project_graph(graph: &DataProjectGraph) -> Result<(), String> {
    import_data_project_graph_authorized(graph, None)
}

pub fn import_data_project_graph_bundle(bundle: &DataProjectGraphBundle) -> Result<(), String> {
    import_data_project_graph(bundle)
}

/// Import a graph after the cloud layer has authenticated the remote binding.
/// `trusted_source_scope` is the original owner's scope embedded in metadata;
/// it is persisted as a project trust record and never rewrites object bytes.
/// Callers must pass `None` for an untrusted/local graph.
pub(crate) fn commit_timestamps_equal(left: &str, right: &str) -> bool {
    if left == right {
        return true;
    }
    match (
        DateTime::parse_from_rfc3339(left),
        DateTime::parse_from_rfc3339(right),
    ) {
        // The cloud stores PostgreSQL timestamptz values at microsecond
        // precision and serializes fractional seconds without trailing zeroes.
        (Ok(left), Ok(right)) => left.timestamp_micros() == right.timestamp_micros(),
        _ => false,
    }
}

pub fn import_data_project_graph_authorized(
    graph: &DataProjectGraph,
    trusted_source_scope: Option<&str>,
) -> Result<(), String> {
    let _operation_guard = operation_lock()?;
    let mut connection = open_db()?;
    history_scope_authorized(&connection, &graph.data_project_id)?;
    ensure_default_branch(&connection, &graph.data_project_id)?;
    ensure_active_clean(&connection, &graph.data_project_id)?;
    let existing_scope = project_scope(&connection, &graph.data_project_id)?;
    let current_scope = crate::credentials::current_session()?.map(|session| {
        format!(
            "{}\n{}",
            session.server_url.trim_end_matches('/'),
            session.account_id
        )
    });
    let trusted_scope = trusted_source_scope
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string);
    if trusted_source_scope.is_some() && current_scope.is_none() {
        return Err("trusted graph import requires an active account session".to_string());
    }
    if let (Some(expected), Some(current)) = (existing_scope.as_deref(), current_scope.as_deref()) {
        if expected != current {
            return Err("data project is outside the active account scope".to_string());
        }
    }

    let mut commits_by_id = BTreeMap::new();
    for commit in &graph.commits {
        if commit.data_project_id != graph.data_project_id {
            return Err("graph contains a commit from another data project".to_string());
        }
        if commit.id.trim().is_empty() || commit.id.len() > 128 {
            return Err("invalid commit id in graph".to_string());
        }
        if commits_by_id.insert(commit.id.clone(), commit).is_some() {
            return Err(format!("graph contains duplicate commit: {}", commit.id));
        }
        for parent in [commit.parent_id.as_ref(), commit.second_parent_id.as_ref()]
            .into_iter()
            .flatten()
        {
            if parent == &commit.id {
                return Err(format!("commit is its own parent: {}", commit.id));
            }
        }
        if commit.entries.iter().any(|entry| {
            !path_is_safe_relative(&entry.file_name)
                || !path_is_safe_relative(&entry.dataset_id)
                || !path_is_safe_relative(&entry.image_id)
        }) {
            return Err(format!(
                "graph contains an unsafe entry in commit {}",
                commit.id
            ));
        }
        for entry in &commit.entries {
            validate_object(&entry.image_hash)?;
            validate_object(&entry.annotation_hash)?;
        }
        for hash in commit.dataset_metadata.values() {
            validate_object(hash)?;
        }
    }
    for hash in &graph.object_hashes {
        validate_object(hash)?;
    }

    // Validate metadata scope before mutating the database.  This is also the
    // evidence check for imported legacy graphs; no account is inferred from
    // the caller's current membership.
    let scope = existing_scope.as_deref().or(current_scope.as_deref());
    for commit in commits_by_id.values() {
        if !commit.entries.is_empty() && commit.dataset_metadata.is_empty() {
            return Err("graph commit lacks dataset account-scope metadata".to_string());
        }
        for hash in commit.dataset_metadata.values() {
            let Some(evidence) = metadata_scope_evidence(hash)? else {
                return Err("graph contains metadata without account-scope evidence".to_string());
            };
            match evidence {
                None if scope.is_none() || existing_scope.is_some() => {}
                Some(actual)
                    if (existing_scope.is_some() && scope == Some(actual.as_str()))
                        || trusted_scope.as_deref() == Some(actual.as_str()) => {}
                _ => return Err("graph metadata is outside the active account scope".to_string()),
            }
        }
    }

    // Insert commits in topological order. Existing rows must be byte-for-byte
    // equivalent at the logical record level; an import can never overwrite a
    // local commit or silently replace an object reference.
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let active_branch: String = transaction
        .query_row(
            "SELECT name FROM data_project_active_branches WHERE data_project_id = ?1",
            params![graph.data_project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    for hash in &graph.object_hashes {
        let size = fs::metadata(object_path(hash)?)
            .map_err(|error| error.to_string())?
            .len();
        transaction.execute(
            "INSERT OR IGNORE INTO data_project_objects (hash, kind, byte_length, created_at) VALUES (?1, 'imported', ?2, ?3)",
            params![hash, size, now_string()],
        ).map_err(|error| error.to_string())?;
    }
    let mut pending = commits_by_id.values().copied().collect::<Vec<_>>();
    let mut inserted = BTreeSet::new();
    while !pending.is_empty() {
        let mut progress = false;
        let mut next = Vec::new();
        for commit in pending {
            let existing = transaction
                .query_row(
                    "SELECT 1 FROM data_project_commits WHERE id = ?1 AND data_project_id = ?2",
                    params![commit.id, graph.data_project_id],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| error.to_string())?;
            if existing.is_some() {
                let current = commit_row(&transaction, &graph.data_project_id, &commit.id, None)?;
                if current.parent_id != commit.parent_id
                    || current.second_parent_id != commit.second_parent_id
                    || current.message != commit.message
                    || !commit_timestamps_equal(&current.created_at, &commit.created_at)
                    || current.dataset_ids != commit.dataset_ids
                    || current.dataset_metadata != commit.dataset_metadata
                    || current.project_metadata != commit.project_metadata
                    || current.entries != commit.entries
                {
                    return Err(format!(
                        "existing commit differs from imported commit: {}",
                        commit.id
                    ));
                }
                inserted.insert(commit.id.clone());
                progress = true;
                continue;
            }
            let mut parents_ready = true;
            for parent in [commit.parent_id.as_ref(), commit.second_parent_id.as_ref()]
                .into_iter()
                .flatten()
            {
                if inserted.contains(parent) {
                    continue;
                }
                let exists: Option<i64> = transaction
                    .query_row(
                        "SELECT 1 FROM data_project_commits WHERE id = ?1 AND data_project_id = ?2",
                        params![parent, graph.data_project_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                if exists.is_none() {
                    parents_ready = false;
                    break;
                }
            }
            if !parents_ready {
                next.push(commit);
                continue;
            }
            let dataset_json =
                serde_json::to_string(&commit.dataset_ids).map_err(|error| error.to_string())?;
            let metadata_json = serde_json::to_string(&commit.dataset_metadata)
                .map_err(|error| error.to_string())?;
            let mut project_metadata = commit.project_metadata.clone();
            validate_project_metadata(&mut project_metadata, &commit.dataset_ids)?;
            let project_metadata_json = serde_json::to_string(&project_metadata)
                .map_err(|error| error.to_string())?;
            transaction
                .execute(
                    "INSERT INTO data_project_commits (id, data_project_id, parent_id, parent2_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        commit.id,
                        graph.data_project_id,
                        commit.parent_id,
                        commit.second_parent_id,
                        commit.message,
                        commit.created_at,
                        dataset_json,
                        metadata_json,
                        project_metadata_json
                    ],
                )
                .map_err(|error| error.to_string())?;
            if let Some(parent) = commit.parent_id.as_deref() {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 0)",
                        params![commit.id, parent],
                    )
                    .map_err(|error| error.to_string())?;
            }
            if let Some(parent) = commit.second_parent_id.as_deref() {
                transaction
                    .execute(
                        "INSERT OR IGNORE INTO data_project_commit_parents (commit_id, parent_id, parent_index) VALUES (?1, ?2, 1)",
                        params![commit.id, parent],
                    )
                    .map_err(|error| error.to_string())?;
            }
            for entry in &commit.entries {
                transaction
                    .execute(
                        "INSERT INTO data_project_commit_entries (commit_id, dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                        params![
                            commit.id,
                            entry.dataset_id,
                            entry.image_id,
                            entry.file_name,
                            entry.image_hash,
                            entry.annotation_hash,
                            entry.status,
                            entry.qa_status,
                            entry.split,
                            entry.width,
                            entry.height
                        ],
                    )
                    .map_err(|error| error.to_string())?;
            }
            inserted.insert(commit.id.clone());
            progress = true;
        }
        if !progress {
            return Err("graph contains a missing or cyclic commit parent".to_string());
        }
        pending = next;
    }

    for branch in &graph.branches {
        if branch.data_project_id != graph.data_project_id {
            return Err("graph contains a branch from another data project".to_string());
        }
        check_name(&branch.name, "branch name")?;
        if let Some(head) = branch.head_commit_id.as_deref() {
            validate_commit_exists(&transaction, &graph.data_project_id, head)?;
        }
        let current: Option<Option<String>> = transaction
            .query_row(
                "SELECT head_commit_id FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
                params![graph.data_project_id, branch.name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let resulting_head = match current.flatten() {
            None => branch.head_commit_id.clone(),
            Some(current_head) => match branch.head_commit_id.as_deref() {
                None => Some(current_head),
                Some(incoming_head) if incoming_head == current_head => Some(current_head),
                Some(incoming_head) if is_ancestor(&transaction, &current_head, incoming_head)? => {
                    Some(incoming_head.to_string())
                }
                Some(incoming_head) if is_ancestor(&transaction, incoming_head, &current_head)? => {
                    Some(current_head)
                }
                Some(_) => {
                    return Err(format!(
                        "import would move divergent branch {}",
                        branch.name
                    ));
                }
            },
        };
        let current_head: Option<String> = transaction
            .query_row(
                "SELECT head_commit_id FROM data_project_branches WHERE data_project_id = ?1 AND name = ?2",
                params![graph.data_project_id, branch.name],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten();
        if active_branch == branch.name && current_head != resulting_head {
            return Err("cannot move the active branch by graph import; checkout the imported head explicitly".to_string());
        }
        transaction
            .execute(
                "INSERT INTO data_project_branches (data_project_id, name, head_commit_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(data_project_id, name) DO UPDATE SET head_commit_id = excluded.head_commit_id, updated_at = excluded.updated_at",
                params![
                    graph.data_project_id,
                    branch.name,
                    resulting_head,
                    branch.created_at,
                    branch.updated_at
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    if let Some(trusted_scope) = trusted_scope {
        let current_scope = current_scope
            .as_deref()
            .ok_or_else(|| "trusted graph import requires an active account session".to_string())?;
        if existing_scope.is_none() {
            transaction
                .execute(
                    "UPDATE data_projects SET account_scope = ?2, updated_at = ?3 WHERE id = ?1",
                    params![graph.data_project_id, current_scope, now_string()],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction
            .execute(
                "INSERT OR IGNORE INTO data_project_trusted_scopes (data_project_id, source_scope, created_at) VALUES (?1, ?2, ?3)",
                params![graph.data_project_id, trusted_scope, now_string()],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_commit_timestamps_use_database_precision_for_identity() {
        assert!(commit_timestamps_equal(
            "2026-10-26T10:35:40.103095000Z",
            "2026-10-26T10:35:40.103095Z"
        ));
        assert!(commit_timestamps_equal(
            "2026-10-26T10:35:40.103095321Z",
            "2026-10-26T18:35:40.103095+08:00"
        ));
        assert!(!commit_timestamps_equal(
            "2026-10-26T10:35:40.103095Z",
            "2026-10-26T10:35:40.103096Z"
        ));
    }
    use crate::{
        data_projects::{assign_dataset_to_data_project, commit_data_project, create_data_project},
        datasets::create_dataset_project,
        domain::SampleRepository,
    };
    use std::{collections::BTreeMap, fs, path::PathBuf};

    #[test]
    fn project_metadata_keeps_unknown_fields_and_rejects_duplicate_members() {
        let mut metadata = parse_project_metadata_json(
            r#"{"schemaVersion":1,"datasetGroups":[{"id":"g","name":"Group","datasetKeys":["dataset-a"],"position":0}],"futureField":{"enabled":true}}"#,
        )
        .expect("parse project metadata");
        assert_eq!(
            metadata
                .extra
                .get("futureField")
                .and_then(Value::as_object)
                .and_then(|value| value.get("enabled"))
                .and_then(Value::as_bool),
            Some(true)
        );
        assert!(validate_project_metadata(
            &mut DataProjectMetadata {
                schema_version: 1,
                dataset_groups: vec![
                    DataProjectDatasetGroup {
                        id: "a".to_string(),
                        name: "A".to_string(),
                        dataset_keys: vec!["dataset".to_string()],
                        position: 0,
                    },
                    DataProjectDatasetGroup {
                        id: "b".to_string(),
                        name: "B".to_string(),
                        dataset_keys: vec!["dataset".to_string()],
                        position: 1,
                    },
                ],
                extra: BTreeMap::new(),
            },
            &["dataset".to_string()],
        )
        .is_err());
        assert_eq!(metadata.dataset_groups[0].dataset_keys, vec!["dataset-a"]);
    }

    #[test]
    fn project_metadata_merge_is_field_level_and_target_ordered() {
        let group = |id: &str, name: &str, keys: &[&str]| DataProjectDatasetGroup {
            id: id.to_string(),
            name: name.to_string(),
            dataset_keys: keys.iter().map(|key| (*key).to_string()).collect(),
            position: 0,
        };
        let base = DataProjectMetadata::default();
        let ours = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![group("group", "Target", &["dataset-a"])],
            extra: BTreeMap::new(),
        };
        let theirs = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![group("group", "Incoming", &["dataset-b"])],
            extra: BTreeMap::new(),
        };
        let (merged, conflicts) = merge_project_metadata(&base, &ours, &theirs);
        assert_eq!(merged.dataset_groups.len(), 1);
        assert_eq!(merged.dataset_groups[0].name, "Target");
        assert_eq!(
            merged.dataset_groups[0].dataset_keys,
            vec!["dataset-a", "dataset-b"]
        );
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].kind, "dataset-group-name");
        assert_eq!(conflicts[0].key, group_conflict_key("group", "name", None));
        assert_eq!(decode_group_conflict_key(&conflicts[0].key).unwrap(), (
            "group".to_string(),
            "name".to_string(),
            None
        ));
    }

    #[test]
    fn project_metadata_merge_exposes_cross_group_assignment_conflict() {
        let group = |id: &str, name: &str, keys: &[&str]| DataProjectDatasetGroup {
            id: id.to_string(),
            name: name.to_string(),
            dataset_keys: keys.iter().map(|key| (*key).to_string()).collect(),
            position: 0,
        };
        let base = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![group("source", "Source", &["dataset"])],
            extra: BTreeMap::new(),
        };
        let ours = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![
                group("source", "Source", &[]),
                group("target", "Target", &["dataset"]),
            ],
            extra: BTreeMap::new(),
        };
        let theirs = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![
                group("source", "Source", &[]),
                group("incoming", "Incoming", &["dataset"]),
            ],
            extra: BTreeMap::new(),
        };
        let (merged, conflicts) = merge_project_metadata(&base, &ours, &theirs);
        let conflict = conflicts
            .iter()
            .find(|conflict| conflict.kind == "dataset-group-membership")
            .expect("cross-group assignment conflict");
        assert_eq!(
            conflict.key,
            group_conflict_key("target", "member", Some("dataset"))
        );
        assert_eq!(conflict.base, Some(Value::Bool(false)));
        assert_eq!(conflict.ours, Some(Value::Bool(true)));
        assert_eq!(conflict.theirs, Some(Value::Bool(false)));
        assert_eq!(
            merged
                .dataset_groups
                .iter()
                .filter(|group| group.dataset_keys.iter().any(|key| key == "dataset"))
                .count(),
            2
        );
    }

    #[test]
    fn project_metadata_merge_reports_delete_vs_modify_as_whole_group_conflict() {
        let group = |id: &str, name: &str| DataProjectDatasetGroup {
            id: id.to_string(),
            name: name.to_string(),
            dataset_keys: vec!["dataset".to_string()],
            position: 0,
        };
        let base = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![group("group", "Base")],
            extra: BTreeMap::new(),
        };
        let ours = DataProjectMetadata::default();
        let theirs = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![group("group", "Modified")],
            extra: BTreeMap::new(),
        };

        let (merged, conflicts) = merge_project_metadata(&base, &ours, &theirs);
        let conflict = conflicts
            .iter()
            .find(|conflict| conflict.kind == "dataset-group")
            .expect("delete-vs-modify whole-group conflict");
        assert_eq!(conflict.key, group_conflict_key("group", "whole", None));
        assert_eq!(conflict.base, Some(serde_json::to_value(group("group", "Base")).unwrap()));
        assert_eq!(conflict.ours, None);
        assert_eq!(conflict.theirs, Some(serde_json::to_value(group("group", "Modified")).unwrap()));
        assert_eq!(merged.dataset_groups.len(), 1);
        assert_eq!(merged.dataset_groups[0].name, "Modified");
    }

    #[test]
    fn project_metadata_merge_auto_merges_independent_group_modifications() {
        let group = |id: &str, name: &str, dataset_key: &str, position: i32| {
            DataProjectDatasetGroup {
                id: id.to_string(),
                name: name.to_string(),
                dataset_keys: vec![dataset_key.to_string()],
                position,
            }
        };
        let base = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![
                group("first", "First", "dataset-a", 0),
                group("second", "Second", "dataset-b", 1),
            ],
            extra: BTreeMap::new(),
        };
        let ours = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![
                group("first", "First ours", "dataset-a", 0),
                group("second", "Second", "dataset-b", 1),
            ],
            extra: BTreeMap::new(),
        };
        let theirs = DataProjectMetadata {
            schema_version: 1,
            dataset_groups: vec![
                group("first", "First", "dataset-a", 0),
                group("second", "Second theirs", "dataset-b", 1),
            ],
            extra: BTreeMap::new(),
        };

        let (merged, conflicts) = merge_project_metadata(&base, &ours, &theirs);
        assert!(conflicts.is_empty());
        assert_eq!(
            merged
                .dataset_groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            vec!["First ours", "Second theirs"]
        );
        assert_eq!(
            merged
                .dataset_groups
                .iter()
                .map(|group| group.position)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
    }

    #[test]
    fn annotation_conflict_descriptions_are_human_readable() {
        let annotation_hash = |label: &str, x: f64| {
            let bytes = serde_json::to_vec(&serde_json::json!({
                "objects": [{
                    "id": format!("object-{label}"),
                    "classId": 0,
                    "label": label,
                    "type": "bbox",
                    "bbox": {"x": x, "y": 2.0, "width": 12.0, "height": 8.0},
                    "attributes": {}
                }]
            }))
            .expect("serialize annotation fixture");
            let hash = format!("{:x}", Sha256::digest(&bytes));
            write_data_project_object(&hash, &bytes).expect("store annotation fixture");
            hash
        };
        let make_entry = |hash: String| VersionCommitEntry {
            dataset_id: "dataset-fixture".to_string(),
            image_id: "image-fixture".to_string(),
            file_name: "sample.png".to_string(),
            image_hash: "0".repeat(64),
            annotation_hash: hash,
            status: "已标注".to_string(),
            qa_status: "待审核".to_string(),
            split: "train".to_string(),
            width: 640,
            height: 480,
        };
        let base_entry = make_entry(annotation_hash("旧缺陷", 1.0));
        let ours_entry = make_entry(annotation_hash("缺陷", 10.0));
        let theirs_entry = make_entry(annotation_hash("裂纹", 20.0));
        let snapshot = |entry: VersionCommitEntry| Snapshot {
            dataset_ids: vec!["dataset-fixture".to_string()],
            metadata: std::collections::BTreeMap::new(),
            project_metadata: DataProjectMetadata::default(),
            entries: std::collections::BTreeMap::from([(entry_key(&entry), entry)]),
        };
        let base = snapshot(base_entry);
        let ours = snapshot(ours_entry);
        let theirs = snapshot(theirs_entry);
        let mut plan = merge_snapshots(&base, &ours, &theirs);
        enrich_conflicts(&mut plan.conflicts, &base, &ours, &theirs);
        let conflict = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == "annotation")
            .expect("annotation conflict");
        assert_eq!(
            conflict.display_name.as_deref(),
            Some("标注内容 · sample.png")
        );
        assert!(conflict
            .base_description
            .as_deref()
            .is_some_and(
                |description| description.contains("1 个标注") && description.contains("旧缺陷")
            ));
        assert!(conflict
            .ours_description
            .as_deref()
            .is_some_and(
                |description| description.contains("缺陷") && description.contains("[10,2,12,8]")
            ));
        assert!(conflict
            .theirs_description
            .as_deref()
            .is_some_and(
                |description| description.contains("裂纹") && description.contains("[20,2,12,8]")
            ));
        let serialized = serde_json::to_value(conflict).expect("serialize display conflict");
        assert!(serialized.get("displayName").is_some());
        assert!(serialized.get("baseDescription").is_some());
    }

    fn real_fixture() -> (String, String, VersionCommit) {
        let dataset_name = format!("Versioning fixture {}", Uuid::new_v4());
        let dataset = create_dataset_project(&dataset_name, "yolo-detect", "demo-bbox")
            .expect("create real dataset fixture");
        let project = create_data_project("Versioning fixture project", "branch test")
            .expect("create real project fixture");
        assign_dataset_to_data_project(Some(&project.id), &dataset.id).expect("assign fixture");
        let first = commit_data_project(&project.id, "fixture baseline")
            .expect("commit real fixture baseline");
        (
            project.id,
            dataset.id,
            VersionCommit {
                id: first.id,
                data_project_id: first.data_project_id,
                branch: Some("main".to_string()),
                parent_id: first.parent_id,
                second_parent_id: None,
                message: first.message,
                created_at: first.created_at,
                dataset_ids: first.dataset_ids,
                dataset_metadata: first.dataset_metadata,
                project_metadata: first.project_metadata,
                entries: first
                    .entries
                    .into_iter()
                    .map(|entry| VersionCommitEntry {
                        dataset_id: entry.dataset_id,
                        image_id: entry.image_id,
                        file_name: entry.file_name,
                        image_hash: entry.image_hash,
                        annotation_hash: entry.annotation_hash,
                        status: entry.status,
                        qa_status: entry.qa_status,
                        split: entry.split,
                        width: entry.width,
                        height: entry.height,
                    })
                    .collect(),
            },
        )
    }

    #[test]
    fn real_versioned_working_tree_entrypoint_handles_source_fallback_and_first_checkout() {
        // A newly assigned dataset has no immutable HEAD yet.  The public
        // versioned entry point must still expose the legacy source-backed
        // capture instead of trying to read a non-existent checkout.
        let source_dataset_name = format!("Versioning source fallback {}", Uuid::new_v4());
        let source_dataset =
            create_dataset_project(&source_dataset_name, "yolo-detect", "demo-bbox")
                .expect("create source fallback dataset");
        let source_project = create_data_project(
            "Versioning source fallback project",
            "source-backed before first commit",
        )
        .expect("create source fallback project");
        assign_dataset_to_data_project(Some(&source_project.id), &source_dataset.id)
            .expect("assign source fallback dataset");
        let source_tree = get_data_project_versioned_working_tree(&source_project.id)
            .expect("capture source-backed working tree");
        assert!(source_tree.head_commit_id.is_none());
        assert!(source_tree.counts.total > 0);
        let source_checkout = checkout_root(&source_project.id, "main");
        assert!(!checkout_state_path(&source_checkout).exists());

        // After the first immutable commit, the same public entry point must
        // create the private managed checkout on demand.  This is the path
        // used immediately after a remote clone/import when no branch switch
        // has materialized a checkout yet.
        let (project_id, _dataset_id, baseline) = real_fixture();
        let checkout = checkout_root(&project_id, "main");
        assert!(!checkout_state_path(&checkout).exists());
        let tree = get_data_project_versioned_working_tree(&project_id)
            .expect("materialize first managed checkout through public entry point");
        assert_eq!(tree.head_commit_id.as_deref(), Some(baseline.id.as_str()));
        assert!(tree.changes.is_empty());
        assert!(checkout_state_path(&checkout).is_file());
    }

    #[test]
    fn real_cas_materializes_active_head_and_keeps_inactive_membership_isolated() {
        let (project_id, dataset_id, baseline) = real_fixture();
        create_data_project_branch(&project_id, "feature", Some(&baseline.id))
            .expect("create feature branch");
        create_data_project_branch(&project_id, "staging", Some(&baseline.id))
            .expect("create staging branch");
        let connection = open_db().expect("open versioning database");
        let empty_snapshot = Snapshot {
            dataset_ids: Vec::new(),
            metadata: std::collections::BTreeMap::new(),
            project_metadata: DataProjectMetadata::default(),
            entries: std::collections::BTreeMap::new(),
        };
        let feature_commit = commit_snapshot(
            &connection,
            &project_id,
            "staging",
            &baseline.id,
            None,
            "remote membership removal",
            &empty_snapshot,
        )
        .expect("create imported-style feature commit");
        drop(connection);

        let inactive = compare_and_swap_data_project_branch(
            &project_id,
            "feature",
            Some(&baseline.id),
            Some(&feature_commit.id),
        )
        .expect("advance inactive branch");
        assert!(!inactive.current);
        let connection = open_db().expect("reopen versioning database");
        assert_eq!(
            project_membership_rows(&connection, &project_id)
                .expect("read active membership")
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>(),
            vec![dataset_id.clone()]
        );
        drop(connection);

        let active = compare_and_swap_data_project_branch(
            &project_id,
            "main",
            Some(&baseline.id),
            Some(&feature_commit.id),
        )
        .expect("advance active branch");
        assert!(active.current);
        let connection = open_db().expect("reopen active project database");
        assert!(project_membership_rows(&connection, &project_id)
            .expect("read updated membership")
            .is_empty());
        drop(connection);
        assert!(checkout_state_path(&checkout_root(&project_id, "main")).is_file());
    }

    #[test]
    fn real_checkout_branch_keeps_external_source_and_rejects_dirty_switch() {
        let (project_id, dataset_id, baseline) = real_fixture();
        let source_path = SampleRepository::new()
            .image_path(&dataset_id, &baseline.entries[0].image_id)
            .expect("fixture source image");
        let original = fs::read(&source_path).expect("read source image");
        create_data_project_branch(&project_id, "feature", Some(&baseline.id))
            .expect("create feature branch");
        let feature_checkout =
            switch_data_project_branch(&project_id, "feature").expect("checkout feature");
        let entry = &baseline.entries[0];
        let checkout_image = PathBuf::from(&feature_checkout.path)
            .join("datasets")
            .join(&dataset_id)
            .join("images")
            .join(&entry.file_name);
        let mut changed = original.clone();
        changed.push(0x7f);
        fs::write(&checkout_image, &changed).expect("edit controlled checkout");
        let feature_commit = commit_data_project_checkout(&project_id, "feature", "feature image")
            .expect("commit controlled checkout");
        assert_ne!(
            feature_commit.entries[0].image_hash,
            baseline.entries[0].image_hash
        );
        assert_eq!(
            fs::read(&source_path).expect("source remains untouched"),
            original
        );

        switch_data_project_branch(&project_id, "main").expect("switch to clean main");
        assert!(switch_data_project_branch(&project_id, "feature").is_ok());
        let dirty_path = checkout_image;
        let mut dirty = fs::read(&dirty_path).expect("read feature checkout");
        dirty.push(0x01);
        fs::write(&dirty_path, dirty).expect("make checkout dirty");
        assert!(switch_data_project_branch(&project_id, "main").is_err());
    }

    #[test]
    fn real_fixture_workbench_label_save_and_dataset_removal_are_committed() {
        let (project_id, dataset_id, baseline) = real_fixture();
        let image_id = baseline.entries[0].image_id.clone();
        let source_path = SampleRepository::new()
            .image_path(&dataset_id, &image_id)
            .expect("fixture source image");
        let source_bytes = fs::read(&source_path).expect("read source image");
        materialize_active_data_project_checkout(&project_id)
            .expect("materialize main checkout")
            .expect("main checkout exists");

        let repository = SampleRepository::new();
        let state = repository.image_annotation_state(&dataset_id, &image_id);
        let saved = repository
            .save_image_annotations_with_revision(
                &dataset_id,
                &image_id,
                state.revision.clone(),
                state.objects.clone(),
            )
            .expect("save branch-local label");
        assert_ne!(state.revision.as_deref(), Some(saved.revision.as_str()));
        let tree = crate::data_projects::get_data_project_working_tree(&project_id)
            .expect("read dirty branch worktree");
        assert!(tree
            .changes
            .iter()
            .any(|change| change.kind == "annotation-changed"));
        let edited = crate::data_projects::commit_data_project(&project_id, "branch label edit")
            .expect("commit branch-local label");
        assert_eq!(edited.parent_id.as_deref(), Some(baseline.id.as_str()));

        crate::data_projects::assign_dataset_to_data_project(None, &dataset_id)
            .expect("unassign dataset while preserving history");
        let removal_tree = crate::data_projects::get_data_project_working_tree(&project_id)
            .expect("read membership removal");
        assert!(removal_tree
            .changes
            .iter()
            .any(|change| change.kind == "dataset-removed"));
        let removed = crate::data_projects::commit_data_project(&project_id, "remove dataset")
            .expect("commit dataset removal");
        assert!(removed.dataset_ids.is_empty());
        assert!(removed.entries.is_empty());
        assert_eq!(
            fs::read(&source_path).expect("source remains untouched"),
            source_bytes
        );
    }

    #[test]
    fn real_dataset_groups_cas_branch_merge_graph_and_revert_are_persisted() {
        let suffix = Uuid::new_v4().to_string();
        let first_dataset = create_dataset_project(
            &format!("Grouping fixture first {suffix}"),
            "yolo-detect",
            "demo-bbox",
        )
        .expect("create first grouping dataset");
        let second_dataset = create_dataset_project(
            &format!("Grouping fixture second {suffix}"),
            "yolo-detect",
            "demo-bbox",
        )
        .expect("create second grouping dataset");
        let project = create_data_project("Grouping fixture project", "dataset groups")
            .expect("create grouping project");
        assign_dataset_to_data_project(Some(&project.id), &first_dataset.id)
            .expect("assign first grouping dataset");
        assign_dataset_to_data_project(Some(&project.id), &second_dataset.id)
            .expect("assign second grouping dataset");
        let baseline = commit_data_project(&project.id, "grouping baseline")
            .expect("commit grouping baseline");

        let groups = vec![
            DataProjectDatasetGroup {
                id: "group-a".to_string(),
                name: "Raw input".to_string(),
                dataset_keys: vec![first_dataset.id.clone()],
                position: 0,
            },
            DataProjectDatasetGroup {
                id: "group-b".to_string(),
                name: "Reviewed".to_string(),
                dataset_keys: vec![second_dataset.id.clone()],
                position: 1,
            },
        ];
        let initial_state = get_data_project_dataset_groups(&project.id, Some("main"))
            .expect("read initial grouping state");
        assert_eq!(initial_state.revision, 0);
        let grouped_state = set_data_project_dataset_groups(
            &project.id,
            Some("main"),
            initial_state.revision,
            groups.clone(),
        )
        .expect("set groups in the real checkout");
        assert_eq!(grouped_state.revision, 1);
        assert!(set_data_project_dataset_groups(
            &project.id,
            Some("main"),
            initial_state.revision,
            groups.clone(),
        )
        .is_err());
        let dirty = crate::data_projects::get_data_project_working_tree(&project.id)
            .expect("read group-only dirty tree");
        assert!(dirty
            .changes
            .iter()
            .any(|change| change.kind == "project-metadata-changed"));
        let grouped_commit = commit_data_project(&project.id, "save dataset groups")
            .expect("commit group-only change");
        assert_eq!(grouped_commit.project_metadata.dataset_groups, groups);
        assert!(crate::data_projects::get_data_project_working_tree(&project.id)
            .expect("read clean grouped tree")
            .changes
            .is_empty());

        create_data_project_branch(&project.id, "feature", Some(&grouped_commit.id))
            .expect("create grouping feature branch");
        switch_data_project_branch(&project.id, "feature").expect("checkout grouping feature");
        let feature_state = get_data_project_dataset_groups(&project.id, Some("feature"))
            .expect("read feature grouping state");
        assert_eq!(feature_state.revision, 0);
        let mut feature_groups = groups.clone();
        feature_groups[0].name = "Feature input".to_string();
        set_data_project_dataset_groups(
            &project.id,
            Some("feature"),
            feature_state.revision,
            feature_groups,
        )
        .expect("edit feature grouping name");
        let feature_commit = commit_data_project(&project.id, "feature grouping name")
            .expect("commit feature grouping name");

        switch_data_project_branch(&project.id, "main").expect("checkout grouped main");
        let main_state = get_data_project_dataset_groups(&project.id, Some("main"))
            .expect("read main grouping state");
        assert_eq!(main_state.revision, grouped_state.revision);
        let mut main_groups = groups.clone();
        main_groups[0].name = "Main input".to_string();
        set_data_project_dataset_groups(
            &project.id,
            Some("main"),
            main_state.revision,
            main_groups,
        )
        .expect("edit main grouping name");
        let main_commit = commit_data_project(&project.id, "main grouping name")
            .expect("commit main grouping name");

        let preview = preview_data_project_merge(&project.id, "main", "feature")
            .expect("preview grouping merge");
        let group_name_conflict = preview
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == "dataset-group-name")
            .expect("group name conflict");
        let merge = merge_data_project_branches(
            &project.id,
            "main",
            "feature",
            "merge dataset groups",
        )
        .expect("create grouping merge session");
        let session_id = merge.session_id.expect("grouping merge session");
        let resolved = resolve_data_project_merge(
            &project.id,
            &session_id,
            &group_name_conflict.key,
            "ours",
        )
        .expect("resolve group name conflict");
        let merge_commit = resolved.commit.expect("grouping merge commit");
        assert_eq!(merge_commit.parent_id.as_deref(), Some(main_commit.id.as_str()));
        assert_eq!(
            merge_commit.second_parent_id.as_deref(),
            Some(feature_commit.id.as_str())
        );
        assert_eq!(
            merge_commit.project_metadata.dataset_groups[0].name,
            "Main input"
        );

        let graph = export_data_project_graph(&project.id).expect("export grouping graph");
        assert!(graph.commits.iter().any(|commit| {
            commit.id == merge_commit.id
                && commit.project_metadata.dataset_groups[0].dataset_keys
                    == vec![first_dataset.id.clone()]
        }));
        let reverted = revert_data_project_commit(
            &project.id,
            "main",
            &baseline.id,
            "restore grouping baseline",
        )
        .expect("revert grouping metadata");
        assert!(reverted.project_metadata.dataset_groups.is_empty());
        assert_ne!(reverted.id, baseline.id);
    }

    #[test]
    fn real_fixture_three_way_conflict_resolve_and_revert_preserve_dag() {
        let (project_id, dataset_id, baseline) = real_fixture();
        create_data_project_branch(&project_id, "feature", Some(&baseline.id))
            .expect("create feature branch");
        let feature_checkout =
            switch_data_project_branch(&project_id, "feature").expect("checkout feature");
        let entry = &baseline.entries[0];
        let feature_path = PathBuf::from(&feature_checkout.path)
            .join("datasets")
            .join(&dataset_id)
            .join("images")
            .join(&entry.file_name);
        let mut feature_bytes = fs::read(&feature_path).expect("read feature image");
        feature_bytes.push(0x21);
        fs::write(&feature_path, feature_bytes).expect("edit feature image");
        let feature_commit = commit_data_project_checkout(&project_id, "feature", "feature edit")
            .expect("commit feature edit");

        switch_data_project_branch(&project_id, "main").expect("checkout main");
        let main_checkout =
            data_project_checkout_status(&project_id, Some("main")).expect("main status");
        let main_path = PathBuf::from(&main_checkout.path)
            .join("datasets")
            .join(&dataset_id)
            .join("images")
            .join(&entry.file_name);
        let mut main_bytes = fs::read(&main_path).expect("read main image");
        main_bytes.push(0x22);
        fs::write(&main_path, main_bytes).expect("edit main image");
        let main_commit = commit_data_project_checkout(&project_id, "main", "main edit")
            .expect("commit main edit");

        let preview =
            preview_data_project_merge(&project_id, "main", "feature").expect("preview merge");
        let conflict = preview
            .conflicts
            .iter()
            .find(|item| item.kind == "image")
            .expect("image conflict");
        let merged =
            merge_data_project_branches(&project_id, "main", "feature", "merge fixture branches")
                .expect("create merge session");
        let session_id = merged.session_id.expect("merge conflict session");
        let resolved =
            resolve_data_project_merge(&project_id, &session_id, &conflict.key, "theirs")
                .expect("resolve merge conflict");
        let merge_commit = resolved.commit.expect("resolved merge commit");
        assert_eq!(
            merge_commit.second_parent_id.as_deref(),
            Some(feature_commit.id.as_str())
        );
        assert_eq!(
            merge_commit.parent_id.as_deref(),
            Some(main_commit.id.as_str())
        );

        let reverted = revert_data_project_commit(
            &project_id,
            "main",
            &main_commit.id,
            "restore fixture main snapshot",
        )
        .expect("restore main snapshot");
        assert_eq!(
            reverted.parent_id.as_deref(),
            Some(merge_commit.id.as_str())
        );
        assert_eq!(
            reverted.entries[0].image_hash,
            main_commit.entries[0].image_hash
        );
        delete_data_project_branch(&project_id, "feature").expect("delete merged branch");
        assert!(list_data_project_branches(&project_id)
            .expect("list branches")
            .iter()
            .all(|branch| branch.name != "feature"));
    }
}

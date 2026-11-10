//! Local data-project membership and content-addressed history.
//!
//! Existing dataset directories and their per-dataset cloud identities remain
//! untouched. This module stores only the project-level index, immutable
//! objects, and commit metadata in the workspace root.

use crate::{
    domain::{DatasetImage, SampleRepository},
    project_fs::{self, ProjectManifest, ProjectPaths},
    storage,
};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};
use uuid::Uuid;

const HASH_BUFFER_SIZE: usize = 1024 * 1024;

fn sqlite_byte_length(value: u64) -> Result<i64, String> {
    i64::try_from(value).map_err(|_| "object is too large for SQLite integer".to_string())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProject {
    pub id: String,
    pub name: String,
    pub description: String,
    pub dataset_ids: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    pub account_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectCounts {
    pub raw: u64,
    pub in_progress: u64,
    pub pending_review: u64,
    pub approved: u64,
    pub rejected: u64,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectChange {
    pub kind: String,
    pub dataset_id: String,
    pub image_id: Option<String>,
    pub file_name: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectWorkingTree {
    pub data_project_id: String,
    pub counts: DataProjectCounts,
    pub changes: Vec<DataProjectChange>,
    pub head_commit_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectCommitEntry {
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct DataProjectCommit {
    pub id: String,
    pub data_project_id: String,
    pub parent_id: Option<String>,
    pub message: String,
    pub created_at: String,
    pub dataset_ids: Vec<String>,
    pub dataset_metadata: BTreeMap<String, String>,
    #[serde(default)]
    pub project_metadata: crate::data_project_versioning::DataProjectMetadata,
    pub entries: Vec<DataProjectCommitEntry>,
}

#[derive(Debug, Clone)]
struct CapturedEntry {
    entry: DataProjectCommitEntry,
    object_count: usize,
    image_length: u64,
    annotation_length: u64,
}

#[derive(Debug, Clone)]
struct CapturedProject {
    dataset_ids: Vec<String>,
    dataset_metadata: BTreeMap<String, String>,
    dataset_metadata_lengths: BTreeMap<String, u64>,
    entries: Vec<CapturedEntry>,
}

fn now_string() -> String {
    DateTime::<Utc>::from(SystemTime::now()).to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn database_path() -> PathBuf {
    project_fs::workspace_data_root().join("data-projects.sqlite")
}

fn objects_root() -> PathBuf {
    project_fs::workspace_data_root().join("data-project-objects")
}

fn open_database() -> Result<Connection, String> {
    let path = database_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("create data-project root: {error}"))?;
    }
    let connection = Connection::open(&path)
        .map_err(|error| format!("open data-project database {}: {error}", path.display()))?;
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
            CREATE INDEX IF NOT EXISTS idx_data_project_datasets_project
              ON data_project_datasets(data_project_id);
            CREATE INDEX IF NOT EXISTS idx_data_project_commits_project
              ON data_project_commits(data_project_id, created_at);
            CREATE INDEX IF NOT EXISTS idx_data_project_entries_commit
              ON data_project_commit_entries(commit_id);
            "#,
        )
        .map_err(|error| format!("initialize data-project database: {error}"))?;
    // Databases created by the first version of this module do not have the
    // metadata column; keep their commits readable while adding it lazily.
    let has_metadata_column = connection
        .prepare("PRAGMA table_info(data_project_commits)")
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .iter()
        .any(|column| column == "dataset_metadata_json");
    if !has_metadata_column {
        connection
            .execute(
                "ALTER TABLE data_project_commits ADD COLUMN dataset_metadata_json TEXT NOT NULL DEFAULT '{}'",
                [],
            )
            .map_err(|error| format!("migrate data-project schema: {error}"))?;
    }
    let has_project_metadata_column = connection
        .prepare("PRAGMA table_info(data_project_commits)")
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .iter()
        .any(|column| column == "project_metadata_json");
    if !has_project_metadata_column {
        connection
            .execute(
                "ALTER TABLE data_project_commits ADD COLUMN project_metadata_json TEXT NOT NULL DEFAULT '{\"schemaVersion\":1,\"datasetGroups\":[]}'",
                [],
            )
            .map_err(|error| format!("migrate project metadata schema: {error}"))?;
    }
    let has_account_scope_column = connection
        .prepare("PRAGMA table_info(data_projects)")
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("inspect data-project schema: {error}"))?
        .iter()
        .any(|column| column == "account_scope");
    if !has_account_scope_column {
        connection
            .execute(
                "ALTER TABLE data_projects ADD COLUMN account_scope TEXT",
                [],
            )
            .map_err(|error| format!("migrate data-project schema: {error}"))?;
    }
    Ok(connection)
}

fn non_empty_name(value: &str, field: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{field} must not be empty"));
    }
    if value.len() > 256 {
        return Err(format!("{field} is too long"));
    }
    Ok(value.to_string())
}

fn read_data_project_row(row: &Row<'_>) -> rusqlite::Result<DataProject> {
    Ok(DataProject {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        dataset_ids: Vec::new(),
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        account_scope: row.get(5)?,
    })
}

fn load_project_by_id(connection: &Connection, id: &str) -> Result<DataProject, String> {
    let mut project = connection
        .query_row(
            "SELECT id, name, description, created_at, updated_at, account_scope FROM data_projects WHERE id = ?1",
            params![id],
            read_data_project_row,
        )
        .map_err(|error| format!("read data project {id}: {error}"))?;
    project.dataset_ids = load_dataset_ids(connection, id)?;
    Ok(project)
}

fn load_dataset_ids(connection: &Connection, project_id: &str) -> Result<Vec<String>, String> {
    let mut statement = connection
        .prepare(
            "SELECT dataset_id FROM data_project_datasets WHERE data_project_id = ?1 ORDER BY dataset_id",
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![project_id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string());
    rows
}

pub fn list_data_projects() -> Result<Vec<DataProject>, String> {
    let connection = open_database()?;
    let mut statement = connection
        .prepare(
            "SELECT id, name, description, created_at, updated_at, account_scope FROM data_projects ORDER BY name, id",
        )
        .map_err(|error| error.to_string())?;
    let projects = statement
        .query_map([], read_data_project_row)
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    let mut visible_projects = Vec::with_capacity(projects.len());
    for mut project in projects {
        project.dataset_ids = load_dataset_ids(&connection, &project.id)?;
        if project_visible_for_list(&project)? && project_history_authorized(&connection, &project)?
        {
            visible_projects.push(project);
        }
    }
    Ok(visible_projects)
}

/// Return only the dataset projects assigned to the requested data project.
/// This is intentionally not implemented by returning the global dataset
/// catalog: a missing/unauthorized project must fail closed instead of making
/// unrelated datasets appear to belong to it.
pub fn list_data_project_datasets(
    data_project_id: &str,
) -> Result<Vec<crate::domain::DatasetProject>, String> {
    let connection = open_database()?;
    let project = load_project_by_id(&connection, data_project_id)?;
    ensure_current_membership_visible(&project)?;
    let expected = project.dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut datasets = SampleRepository::new()
        .dataset_projects()
        .into_iter()
        .filter(|dataset| expected.contains(&dataset.id))
        .collect::<Vec<_>>();
    datasets.sort_by(|left, right| left.id.cmp(&right.id));
    if datasets.len() != expected.len() {
        let found = datasets
            .iter()
            .map(|dataset| dataset.id.as_str())
            .collect::<BTreeSet<_>>();
        let missing = expected
            .iter()
            .find(|dataset_id| !found.contains(dataset_id.as_str()))
            .cloned()
            .unwrap_or_default();
        return Err(format!("data-project dataset is not available: {missing}"));
    }
    Ok(datasets)
}

pub fn create_data_project(name: &str, description: &str) -> Result<DataProject, String> {
    let name = non_empty_name(name, "data project name")?;
    let description = description.trim().to_string();
    let id = format!("data-project-{}", Uuid::new_v4());
    let now = now_string();
    let connection = open_database()?;
    connection
        .execute(
            "INSERT INTO data_projects (id, name, description, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?4)",
            params![id, name, description, now],
        )
        .map_err(|error| format!("create data project: {error}"))?;
    load_project_by_id(&connection, &id)
}

fn dataset_exists(dataset_id: &str) -> bool {
    project_fs::list_project_manifests()
        .iter()
        .any(|manifest| manifest.id == dataset_id)
        || {
            let paths = project_fs::project_paths(dataset_id);
            paths.root.exists() || paths.sqlite.exists()
        }
}

fn scoped_remote_project_id(server_url: &str, account_id: &str, remote_project_id: &str) -> String {
    let scope = format!(
        "{}\n{}\n{}",
        server_url.trim_end_matches('/'),
        account_id,
        remote_project_id
    );
    format!("remote-{:x}", Sha256::digest(scope.as_bytes()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DatasetVisibility {
    Local,
    Remote(String),
    Hidden,
}

fn active_account_scope() -> Result<Option<String>, String> {
    Ok(crate::credentials::current_session()?.map(|session| {
        format!(
            "{}\n{}",
            session.server_url.trim_end_matches('/'),
            session.account_id
        )
    }))
}

/// Resolve the active-account visibility and stable account scope for one
/// dataset. Local datasets intentionally remain visible across account
/// switches; remote datasets require a matching server/account/scoped ID.
fn dataset_visibility(dataset_id: &str) -> Result<DatasetVisibility, String> {
    let paths = project_fs::project_paths(dataset_id);
    if !paths.sqlite.exists() {
        return Ok(DatasetVisibility::Hidden);
    }
    storage::initialize_project_database(&paths.sqlite)?;
    let connection = Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let deleted_at = connection
        .query_row(
            "SELECT deleted_at FROM projects WHERE id = ?1",
            params![dataset_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("read dataset tombstone {dataset_id}: {error}"))?;
    if deleted_at.is_some() {
        return Ok(DatasetVisibility::Hidden);
    }
    let remote = connection
        .query_row(
            "SELECT server_url, remote_project_id, account_id FROM remote_project_configs WHERE project_id = ?1",
            params![dataset_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| format!("read remote dataset scope {dataset_id}: {error}"))?;
    let Some((server_url, remote_project_id, owner_account_id)) = remote else {
        return Ok(DatasetVisibility::Local);
    };
    // A legacy remote row without an owner is ambiguous after account
    // switching. Keep it inaccessible rather than exposing another account's
    // cached images or labels.
    let Some(owner_account_id) = owner_account_id else {
        return Ok(DatasetVisibility::Hidden);
    };
    let Some(session) = crate::credentials::current_session()? else {
        return Ok(DatasetVisibility::Hidden);
    };
    if server_url.trim_end_matches('/') != session.server_url.trim_end_matches('/')
        || owner_account_id != session.account_id
    {
        return Ok(DatasetVisibility::Hidden);
    }
    let source_key = project_fs::read_manifest(dataset_id)
        .map(|manifest| manifest.source_dataset_key)
        .unwrap_or_default();
    if source_key.starts_with("remote:") {
        let expected_source = format!(
            "remote:{}:{}:{}",
            server_url.trim_end_matches('/'),
            session.account_id,
            remote_project_id
        );
        if dataset_id
            != scoped_remote_project_id(&server_url, &session.account_id, &remote_project_id)
            || source_key != expected_source
        {
            return Ok(DatasetVisibility::Hidden);
        }
    }
    Ok(DatasetVisibility::Remote(format!(
        "{}\n{}",
        server_url.trim_end_matches('/'),
        session.account_id
    )))
}

/// Shared fail-closed visibility check for the branch/workbench adapter.  A
/// checkout can only redirect reads for a dataset that the current account
/// may still access; local datasets retain the historical cross-account
/// behavior.
pub(crate) fn ensure_dataset_visible_to_current_account(dataset_id: &str) -> Result<(), String> {
    if dataset_visibility(dataset_id)? == DatasetVisibility::Hidden {
        return Err(format!(
            "dataset is not visible to the active account: {dataset_id}"
        ));
    }
    Ok(())
}

fn project_scope_visible(account_scope: Option<&str>) -> Result<bool, String> {
    let Some(account_scope) = account_scope else {
        return Ok(true);
    };
    Ok(active_account_scope()?.as_deref() == Some(account_scope))
}

fn ensure_current_membership_visible(project: &DataProject) -> Result<(), String> {
    if !project_scope_visible(project.account_scope.as_deref())? {
        return Err("data project is outside the active account scope".to_string());
    }
    for dataset_id in &project.dataset_ids {
        match dataset_visibility(dataset_id)? {
            DatasetVisibility::Hidden => {
                return Err("data project contains a dataset outside the active account".to_string())
            }
            DatasetVisibility::Local => {}
            DatasetVisibility::Remote(scope) => {
                if project.account_scope.as_deref() != Some(scope.as_str()) {
                    return Err(
                        "data project remote dataset scope does not match project scope"
                            .to_string(),
                    );
                }
            }
        }
    }
    Ok(())
}

fn project_visible_for_list(project: &DataProject) -> Result<bool, String> {
    if !project_scope_visible(project.account_scope.as_deref())? {
        return Ok(false);
    }
    // A NULL project scope can only be shared as a local project. If it still
    // points at a remote dataset, require an explicit assignment that binds
    // the project before listing it.
    if project.account_scope.is_none() {
        for dataset_id in &project.dataset_ids {
            match dataset_visibility(dataset_id)? {
                DatasetVisibility::Hidden | DatasetVisibility::Remote(_) => return Ok(false),
                DatasetVisibility::Local => {}
            }
        }
    }
    Ok(true)
}

pub fn assign_dataset_to_data_project(
    data_project_id: Option<&str>,
    dataset_id: &str,
) -> Result<(), String> {
    let dataset_id = non_empty_name(dataset_id, "dataset id")?;
    let visibility = if data_project_id.is_some() {
        if !dataset_exists(&dataset_id) {
            return Err(format!("dataset not found: {dataset_id}"));
        }
        let visibility = dataset_visibility(&dataset_id)?;
        if visibility == DatasetVisibility::Hidden {
            return Err(format!(
                "dataset is not visible to the active account: {dataset_id}"
            ));
        }
        visibility
    } else {
        // Unassignment is a repair operation. It must be able to remove a
        // stale membership even when the source directory, tombstone, or
        // previous account's remote credentials are no longer available.
        DatasetVisibility::Local
    };
    let mut connection = open_database()?;
    let previous_project_id: Option<String> = connection
        .query_row(
            "SELECT data_project_id FROM data_project_datasets WHERE dataset_id = ?1",
            params![dataset_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let transaction = connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    if data_project_id.is_some() {
        if let Some(previous_id) = previous_project_id.as_deref() {
            let previous_project = load_project_by_id(&transaction, previous_id)?;
            if !project_scope_visible(previous_project.account_scope.as_deref())? {
                return Err(
                    "dataset belongs to a data project outside the active account scope"
                        .to_string(),
                );
            }
        }
    }
    let now = now_string();
    if let Some(project_id) = data_project_id {
        let project = load_project_by_id(&transaction, project_id)?;
        ensure_current_membership_visible(&project)?;
        let bound_scope = match visibility {
            DatasetVisibility::Local => project.account_scope.clone(),
            DatasetVisibility::Remote(scope) => {
                if project
                    .account_scope
                    .as_deref()
                    .is_some_and(|existing| existing != scope)
                {
                    return Err("data project is bound to another account scope".to_string());
                }
                Some(scope)
            }
            DatasetVisibility::Hidden => unreachable!(),
        };
        transaction.execute(
            "INSERT INTO data_project_datasets (dataset_id, data_project_id, assigned_at) VALUES (?1, ?2, ?3) ON CONFLICT(dataset_id) DO UPDATE SET data_project_id = excluded.data_project_id, assigned_at = excluded.assigned_at",
            params![dataset_id, project_id, now],
        ).map_err(|error| format!("assign dataset: {error}"))?;
        transaction
            .execute(
                "UPDATE data_projects SET updated_at = ?2, account_scope = ?3 WHERE id = ?1",
                params![project_id, now, bound_scope],
            )
            .map_err(|error| error.to_string())?;
    } else {
        transaction
            .execute(
                "DELETE FROM data_project_datasets WHERE dataset_id = ?1",
                params![dataset_id],
            )
            .map_err(|error| format!("unassign dataset: {error}"))?;
    }
    if let Some(previous_id) = previous_project_id.as_deref() {
        transaction
            .execute(
                "UPDATE data_projects SET updated_at = ?2 WHERE id = ?1",
                params![previous_id, now],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    if let Some(previous_id) = previous_project_id.as_deref() {
        if data_project_id != Some(previous_id)
            && project_scope_visible(
                load_project_by_id(&connection, previous_id)?
                    .account_scope
                    .as_deref(),
            )?
        {
            crate::data_project_versioning::remove_dataset_from_active_data_project_checkout(
                previous_id,
                &dataset_id,
            )?;
        }
    }
    Ok(())
}

fn to_domain_image(image: &storage::StoredImage) -> DatasetImage {
    DatasetImage {
        id: image.id.clone(),
        file_name: image.file_name.clone(),
        width: image.width,
        height: image.height,
        split: image.split.clone(),
        status: image.status.clone(),
        qa_status: image.qa_status.clone(),
        review_note: image.review_note.clone(),
        tags: Vec::new(),
    }
}

fn manifest_for_dataset(paths: &ProjectPaths, dataset_id: &str) -> Result<ProjectManifest, String> {
    storage::read_project_manifest(&paths.sqlite)?
        .or_else(|| project_fs::read_manifest(dataset_id))
        .ok_or_else(|| format!("project manifest not found for dataset {dataset_id}"))
}

fn canonical_source_root(manifest: &ProjectManifest, paths: &ProjectPaths) -> PathBuf {
    let configured = PathBuf::from(&manifest.root_path);
    if configured.is_dir() {
        configured
    } else {
        paths.raw.clone()
    }
}

/// Hash and publish a source file in one streaming pass. A temporary object
/// is renamed into place only after its hash and complete contents are known.
fn hash_and_store(path: &Path) -> Result<(String, u64), String> {
    let root = objects_root();
    fs::create_dir_all(&root).map_err(|error| format!("create object store: {error}"))?;
    let temp = root.join(format!(".tmp-{}", Uuid::new_v4()));
    let result = (|| {
        let mut input = File::open(path)
            .map_err(|error| format!("open object source {}: {error}", path.display()))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .map_err(|error| format!("create object temporary file: {error}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
        let mut length = 0u64;
        loop {
            let read = input
                .read(&mut buffer)
                .map_err(|error| format!("read object source {}: {error}", path.display()))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            output
                .write_all(&buffer[..read])
                .map_err(|error| format!("write object temporary file: {error}"))?;
            length = length.saturating_add(read as u64);
        }
        output
            .sync_all()
            .map_err(|error| format!("flush object temporary file: {error}"))?;
        let hash = format!("{:x}", hasher.finalize());
        let target = root.join(&hash);
        if target.exists() {
            let (existing_hash, existing_length) = hash_file(&target)?;
            if existing_hash != hash || existing_length != length {
                return Err(format!("content-addressed object collision for {hash}"));
            }
            fs::remove_file(&temp).map_err(|error| format!("remove duplicate object: {error}"))?;
        } else {
            fs::rename(&temp, &target)
                .map_err(|error| format!("publish object {}: {error}", target.display()))?;
        }
        Ok((hash, length))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn capture_dataset(
    repository: &SampleRepository,
    dataset_id: &str,
    persist_objects: bool,
) -> Result<Vec<CapturedEntry>, String> {
    let paths = project_fs::project_paths(dataset_id);
    let manifest = manifest_for_dataset(&paths, dataset_id)?;
    let classes = storage::read_classes(&paths.sqlite)?;
    let images = storage::read_images(&paths.sqlite, None)?;
    let source_root = canonical_source_root(&manifest, &paths);
    let mut entries = Vec::with_capacity(images.len());
    for stored in images {
        let image = to_domain_image(&stored);
        // This validates dimensions and parses SQLite/native/external labels
        // without converting parse errors into an empty annotation set.
        let state = repository.snapshot_annotation_state_strict(
            &paths,
            &manifest,
            &classes,
            &source_root,
            &image,
        )?;
        let image_path = repository
            .image_path(dataset_id, &image.id)
            .ok_or_else(|| format!("image not found for {}:{}", dataset_id, image.id))?;
        let (image_hash, image_length) = if persist_objects {
            hash_and_store(&image_path)?
        } else {
            hash_file(&image_path)?
        };
        // Re-read the source after publishing. If a producer replaced or
        // modified the image while it was being copied, fail the capture so
        // no commit can point at a torn object.
        let (source_hash_after_copy, _) = hash_file(&image_path)?;
        if source_hash_after_copy != image_hash {
            return Err(format!(
                "image changed while capturing {}:{}",
                dataset_id, image.id
            ));
        }
        let state_after_copy = repository.snapshot_annotation_state_strict(
            &paths,
            &manifest,
            &classes,
            &source_root,
            &image,
        )?;
        let state_before_bytes =
            serde_json::to_vec(&(&state.objects, &state.verified, &state.revision))
                .map_err(|error| format!("serialize annotation capture check: {error}"))?;
        let state_after_bytes = serde_json::to_vec(&(
            &state_after_copy.objects,
            &state_after_copy.verified,
            &state_after_copy.revision,
        ))
        .map_err(|error| format!("serialize annotation capture check: {error}"))?;
        if state_before_bytes != state_after_bytes {
            return Err(format!(
                "annotation changed while capturing {}:{}",
                dataset_id, image.id
            ));
        }
        // Include the parsed labels, classes, verification and review state in
        // the annotation object so class or empty-label state changes cannot
        // be hidden behind an unchanged geometry hash.
        let annotation_payload = serde_json::json!({
            "objects": &state.objects,
            "classes": classes.iter().map(|class| {
                serde_json::json!({
                    "id": class.id,
                    "label": class.label,
                })
            }).collect::<Vec<_>>(),
            "status": &image.status,
            "qaStatus": &image.qa_status,
            "verified": &state.verified,
            "revision": &state.revision,
        });
        let payload_objects = serde_json::to_value(&annotation_payload)
            .map_err(|error| format!("serialize annotation version: {error}"))?;
        let annotation_bytes = serde_json::to_vec(&payload_objects)
            .map_err(|error| format!("serialize annotation version bytes: {error}"))?;
        let (annotation_hash, annotation_length) = if persist_objects {
            store_content_object_bytes(&annotation_bytes)?
        } else {
            hash_bytes(&annotation_bytes)
        };
        entries.push(CapturedEntry {
            entry: DataProjectCommitEntry {
                dataset_id: dataset_id.to_string(),
                image_id: image.id,
                file_name: image.file_name,
                image_hash,
                annotation_hash,
                status: image.status,
                qa_status: image.qa_status,
                split: image.split,
                width: image.width,
                height: image.height,
            },
            object_count: state.objects.len(),
            image_length,
            annotation_length,
        });
    }
    entries.sort_by(|left, right| left.entry.image_id.cmp(&right.entry.image_id));
    Ok(entries)
}

fn store_content_object_bytes(bytes: &[u8]) -> Result<(String, u64), String> {
    let root = objects_root();
    fs::create_dir_all(&root)
        .map_err(|error| format!("create annotation object store: {error}"))?;
    let temp = root.join(format!(".annotation-{}.json", Uuid::new_v4()));
    fs::write(&temp, bytes).map_err(|error| format!("write annotation object: {error}"))?;
    let result = hash_and_store(&temp);
    let _ = fs::remove_file(&temp);
    result
}

fn hash_file(path: &Path) -> Result<(String, u64), String> {
    let mut input = File::open(path)
        .map_err(|error| format!("open source for verification {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_SIZE];
    let mut length = 0u64;
    loop {
        let read = input
            .read(&mut buffer)
            .map_err(|error| format!("read source for verification {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        length = length.saturating_add(read as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), length))
}

fn hash_bytes(bytes: &[u8]) -> (String, u64) {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    (format!("{:x}", hasher.finalize()), bytes.len() as u64)
}

fn dataset_metadata_payload(
    dataset_id: &str,
    account_scope: Option<&str>,
) -> Result<(String, Vec<u8>), String> {
    let paths = project_fs::project_paths(dataset_id);
    let manifest = manifest_for_dataset(&paths, dataset_id)?;
    let classes = storage::read_classes(&paths.sqlite)?;
    let metadata = serde_json::json!({
        "manifest": manifest,
        "accountScope": account_scope,
        "classes": classes.iter().map(|class| serde_json::json!({
            "id": class.id,
            "label": class.label,
            "color": class.color,
        })).collect::<Vec<_>>(),
    });
    let bytes = serde_json::to_vec(&metadata)
        .map_err(|error| format!("serialize dataset metadata: {error}"))?;
    Ok((hash_bytes(&bytes).0, bytes))
}

fn capture_project(
    connection: &Connection,
    project_id: &str,
    persist_objects: bool,
) -> Result<CapturedProject, String> {
    let dataset_ids = load_dataset_ids(connection, project_id)?;
    let account_scope: Option<String> = connection
        .query_row(
            "SELECT account_scope FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let repository = SampleRepository::new();
    let mut entries = Vec::new();
    let mut dataset_metadata = BTreeMap::new();
    let mut dataset_metadata_lengths = BTreeMap::new();
    for dataset_id in &dataset_ids {
        if dataset_visibility(dataset_id)? == DatasetVisibility::Hidden {
            return Err("data project contains a dataset outside the active account".to_string());
        }
        let (metadata_hash, metadata_bytes) =
            dataset_metadata_payload(dataset_id, account_scope.as_deref())?;
        if persist_objects {
            let (stored_hash, metadata_length) = store_content_object_bytes(&metadata_bytes)?;
            if stored_hash != metadata_hash {
                return Err(format!(
                    "dataset metadata changed while archiving: {dataset_id}"
                ));
            }
            dataset_metadata_lengths.insert(dataset_id.clone(), metadata_length);
        } else {
            dataset_metadata_lengths.insert(dataset_id.clone(), metadata_bytes.len() as u64);
        }
        dataset_metadata.insert(dataset_id.clone(), metadata_hash);
        entries.extend(capture_dataset(&repository, dataset_id, persist_objects)?);
    }
    Ok(CapturedProject {
        dataset_ids,
        dataset_metadata,
        dataset_metadata_lengths,
        entries,
    })
}

fn latest_head(connection: &Connection, project_id: &str) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT head_commit_id FROM data_projects WHERE id = ?1",
            params![project_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())
        .map(|value: Option<Option<String>>| value.flatten())
}

fn load_commit_dataset_ids(
    connection: &Connection,
    commit_id: &str,
) -> Result<Vec<String>, String> {
    let value: String = connection
        .query_row(
            "SELECT dataset_ids_json FROM data_project_commits WHERE id = ?1",
            params![commit_id],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&value).map_err(|error| format!("invalid commit dataset list: {error}"))
}

fn load_commit_dataset_metadata(
    connection: &Connection,
    commit_id: &str,
) -> Result<BTreeMap<String, String>, String> {
    let value: String = connection
        .query_row(
            "SELECT dataset_metadata_json FROM data_project_commits WHERE id = ?1",
            params![commit_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| "{}".to_string());
    serde_json::from_str(&value)
        .map_err(|error| format!("invalid commit dataset metadata: {error}"))
}

fn load_commit_project_metadata(
    connection: &Connection,
    commit_id: &str,
) -> Result<crate::data_project_versioning::DataProjectMetadata, String> {
    let value: String = connection
        .query_row(
            "SELECT project_metadata_json FROM data_project_commits WHERE id = ?1",
            params![commit_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .unwrap_or_else(|| "{}".to_string());
    crate::data_project_versioning::parse_project_metadata_json(&value)
}

/// Read the accountScope claim embedded in a metadata object. `None` means
/// the object predates account-scope evidence and must fail closed for a
/// project whose scope is not already established.
fn metadata_object_account_scope(hash: &str) -> Result<Option<Option<String>>, String> {
    let path = objects_root().join(hash);
    if !path.is_file() {
        return Ok(None);
    }
    let data = fs::read_to_string(&path)
        .map_err(|error| format!("read dataset metadata object {hash}: {error}"))?;
    let value: serde_json::Value = serde_json::from_str(&data)
        .map_err(|error| format!("invalid dataset metadata object {hash}: {error}"))?;
    let Some(scope) = value.get("accountScope") else {
        return Ok(None);
    };
    if scope.is_null() {
        return Ok(Some(None));
    }
    let scope = scope
        .as_str()
        .ok_or_else(|| format!("invalid accountScope in dataset metadata object {hash}"))?;
    Ok(Some(Some(scope.to_string())))
}

fn commit_metadata_authorized(
    commit_metadata: &BTreeMap<String, String>,
    project_scope: Option<&str>,
    trusted_scopes: &BTreeSet<String>,
) -> Result<bool, String> {
    for hash in commit_metadata.values() {
        let Some(scope_evidence) = metadata_object_account_scope(hash)? else {
            return Ok(false);
        };
        match (project_scope, scope_evidence) {
            (None, None) => {}
            (None, Some(_)) => return Ok(false),
            (Some(_), None) => {}
            (Some(expected), Some(actual))
                if actual == expected || trusted_scopes.contains(&actual) => {}
            (Some(_), Some(_)) => return Ok(false),
        }
    }
    Ok(true)
}

fn project_history_authorized(
    connection: &Connection,
    project: &DataProject,
) -> Result<bool, String> {
    if !project_scope_visible(project.account_scope.as_deref())? {
        return Ok(false);
    }
    // Cross-account clones retain immutable metadata objects with the
    // original owner's accountScope.  The versioning layer records that
    // source scope only after an authenticated, ACL-checked graph import.
    // Treat the binding as project-local trust evidence; never accept an
    // arbitrary foreign scope or infer it from current dataset membership.
    let mut trusted_scopes = BTreeSet::new();
    let trusted_table_exists: bool = connection
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'data_project_trusted_scopes')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if trusted_table_exists {
        let mut trusted_statement = connection
            .prepare(
                "SELECT source_scope FROM data_project_trusted_scopes WHERE data_project_id = ?1",
            )
            .map_err(|error| error.to_string())?;
        for row in trusted_statement
            .query_map(params![project.id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
        {
            trusted_scopes.insert(row.map_err(|error| error.to_string())?);
        }
    }
    let mut statement = connection
        .prepare(
            "SELECT c.dataset_metadata_json, EXISTS(SELECT 1 FROM data_project_commit_entries e WHERE e.commit_id = c.id) FROM data_project_commits c WHERE c.data_project_id = ?1",
        )
        .map_err(|error| error.to_string())?;
    let metadata_rows = statement
        .query_map(params![project.id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? != 0))
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    for (metadata_json, has_entries) in metadata_rows {
        let metadata = serde_json::from_str::<BTreeMap<String, String>>(&metadata_json)
            .map_err(|error| format!("invalid commit dataset metadata: {error}"))?;
        if has_entries && metadata.is_empty() {
            return Ok(false);
        }
        if !commit_metadata_authorized(
            &metadata,
            project.account_scope.as_deref(),
            &trusted_scopes,
        )? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn load_commit_entries(
    connection: &Connection,
    commit_id: &str,
) -> Result<Vec<DataProjectCommitEntry>, String> {
    let mut statement = connection
        .prepare("SELECT dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height FROM data_project_commit_entries WHERE commit_id = ?1 ORDER BY dataset_id, image_id")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![commit_id], |row| {
            Ok(DataProjectCommitEntry {
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
        .map_err(|error| error.to_string());
    rows
}

fn entry_key(entry: &DataProjectCommitEntry) -> (String, String) {
    (entry.dataset_id.clone(), entry.image_id.clone())
}

fn make_changes(
    current: &CapturedProject,
    head_dataset_ids: &[String],
    head_dataset_metadata: &BTreeMap<String, String>,
    head_entries: &[DataProjectCommitEntry],
) -> Vec<DataProjectChange> {
    let current_dataset_ids = current.dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let head_dataset_ids = head_dataset_ids.iter().cloned().collect::<BTreeSet<_>>();
    let mut changes = Vec::new();
    for dataset_id in current_dataset_ids.difference(&head_dataset_ids) {
        changes.push(DataProjectChange {
            kind: "dataset-added".to_string(),
            dataset_id: dataset_id.clone(),
            image_id: None,
            file_name: None,
            detail: "数据集加入项目".to_string(),
        });
    }
    for dataset_id in head_dataset_ids.difference(&current_dataset_ids) {
        changes.push(DataProjectChange {
            kind: "dataset-removed".to_string(),
            dataset_id: dataset_id.clone(),
            image_id: None,
            file_name: None,
            detail: "数据集移出项目".to_string(),
        });
    }
    for dataset_id in current_dataset_ids.intersection(&head_dataset_ids) {
        if current.dataset_metadata.get(dataset_id) != head_dataset_metadata.get(dataset_id) {
            changes.push(DataProjectChange {
                kind: "dataset-changed".to_string(),
                dataset_id: dataset_id.clone(),
                image_id: None,
                file_name: None,
                detail: "数据集信息或类别定义发生变化".to_string(),
            });
        }
    }
    let current_entries = current
        .entries
        .iter()
        .map(|entry| (entry_key(&entry.entry), entry))
        .collect::<BTreeMap<_, _>>();
    let previous_entries = head_entries
        .iter()
        .map(|entry| (entry_key(entry), entry))
        .collect::<BTreeMap<_, _>>();
    for (key, entry) in &current_entries {
        let (dataset_id, image_id) = key;
        let (kind, detail) = match previous_entries.get(key) {
            None => ("image-added", "新增图像".to_string()),
            Some(previous) if *previous != &entry.entry => {
                let mut details = Vec::new();
                if previous.image_hash != entry.entry.image_hash {
                    details.push("图像内容");
                }
                if previous.annotation_hash != entry.entry.annotation_hash {
                    details.push("标签、类别或审核状态");
                }
                if previous.file_name != entry.entry.file_name
                    || previous.split != entry.entry.split
                    || previous.width != entry.entry.width
                    || previous.height != entry.entry.height
                {
                    details.push("图像信息");
                }
                ("image-changed", format!("{}发生变化", details.join("、")))
            }
            Some(_) => continue,
        };
        changes.push(DataProjectChange {
            kind: kind.to_string(),
            dataset_id: dataset_id.clone(),
            image_id: Some(image_id.clone()),
            file_name: Some(entry.entry.file_name.clone()),
            detail,
        });
    }
    for (key, entry) in &previous_entries {
        if !current_entries.contains_key(key) {
            changes.push(DataProjectChange {
                kind: "image-removed".to_string(),
                dataset_id: entry.dataset_id.clone(),
                image_id: Some(entry.image_id.clone()),
                file_name: Some(entry.file_name.clone()),
                detail: "移除图像".to_string(),
            });
        }
    }
    changes.sort_by(|left, right| {
        left.dataset_id
            .cmp(&right.dataset_id)
            .then(left.image_id.cmp(&right.image_id))
            .then(left.kind.cmp(&right.kind))
    });
    changes
}

fn status_bucket(status: &str, qa_status: &str, object_count: usize) -> &'static str {
    let status = status.trim().to_lowercase();
    let qa_status = qa_status.trim().to_lowercase();
    if matches!(qa_status.as_str(), "驳回" | "rejected")
        || matches!(status.as_str(), "驳回" | "rejected")
    {
        "rejected"
    } else if matches!(
        qa_status.as_str(),
        "待质检" | "submitted" | "pending" | "pending_review"
    ) || matches!(status.as_str(), "待质检" | "submitted" | "pending_review")
    {
        "pendingReview"
    } else if qa_status == "通过"
        || qa_status == "已通过"
        || qa_status.eq_ignore_ascii_case("approved")
        || status == "通过"
        || status == "已通过"
        || status == "已审核"
        || status.eq_ignore_ascii_case("approved")
    {
        "approved"
    } else if matches!(status.as_str(), "未标注" | "unannotated" | "raw" | "") && object_count == 0
    {
        "raw"
    } else {
        // An imported/annotated image without a review decision is still
        // work in progress. An empty annotation is considered approved only
        // when the explicit QA state says it passed.
        "inProgress"
    }
}

pub fn get_data_project_working_tree(project_id: &str) -> Result<DataProjectWorkingTree, String> {
    let connection = open_database()?;
    let baseline = load_project_by_id(&connection, project_id)?;
    ensure_current_membership_visible(&baseline)?;
    // A versioned project owns a materialized branch worktree.  Delegate
    // before the source-backed capture so the annotation UI and the project
    // dashboard observe the same branch files instead of the unchanged
    // external dataset directory.
    if crate::data_project_versioning::active_data_project_checkout(project_id)?.is_some()
        || crate::data_project_versioning::materialize_active_data_project_checkout(project_id)?
            .is_some()
    {
        return crate::data_project_versioning::get_data_project_checkout_working_tree(project_id);
    }
    let current = capture_project(&connection, project_id, false)?;
    let after_capture = load_project_by_id(&connection, project_id)?;
    if baseline.updated_at != after_capture.updated_at
        || baseline.dataset_ids != after_capture.dataset_ids
    {
        return Err("data project changed while capturing working tree".to_string());
    }
    let head_commit_id = latest_head(&connection, project_id)?;
    let (head_dataset_ids, head_dataset_metadata, head_entries) =
        if let Some(head) = head_commit_id.as_deref() {
            (
                load_commit_dataset_ids(&connection, head)?,
                load_commit_dataset_metadata(&connection, head)?,
                load_commit_entries(&connection, head)?,
            )
        } else {
            (Vec::new(), BTreeMap::new(), Vec::new())
        };
    let mut counts = DataProjectCounts::default();
    for entry in &current.entries {
        counts.total += 1;
        match status_bucket(
            &entry.entry.status,
            &entry.entry.qa_status,
            entry.object_count,
        ) {
            "raw" => counts.raw += 1,
            "inProgress" => counts.in_progress += 1,
            "pendingReview" => counts.pending_review += 1,
            "approved" => counts.approved += 1,
            "rejected" => counts.rejected += 1,
            _ => {}
        }
    }
    Ok(DataProjectWorkingTree {
        data_project_id: project_id.to_string(),
        counts,
        changes: make_changes(
            &current,
            &head_dataset_ids,
            &head_dataset_metadata,
            &head_entries,
        ),
        head_commit_id,
    })
}

pub fn commit_data_project(project_id: &str, message: &str) -> Result<DataProjectCommit, String> {
    let message = non_empty_name(message, "commit message")?;
    let mut connection = open_database()?;
    let baseline = load_project_by_id(&connection, project_id)?;
    ensure_current_membership_visible(&baseline)?;
    // Once a branch checkout is materialized, all writes must go through its
    // immutable-object capture.  This prevents the legacy source path from
    // committing a different tree after the workbench edited the controlled
    // copy.
    if let Some(checkout) =
        crate::data_project_versioning::active_data_project_checkout(project_id)?
    {
        let versioned = crate::data_project_versioning::commit_data_project_checkout(
            project_id,
            &checkout.branch,
            &message,
        )?;
        return Ok(DataProjectCommit {
            id: versioned.id,
            data_project_id: versioned.data_project_id,
            parent_id: versioned.parent_id,
            message: versioned.message,
            created_at: versioned.created_at,
            dataset_ids: versioned.dataset_ids,
            dataset_metadata: versioned.dataset_metadata,
            project_metadata: versioned.project_metadata,
            entries: versioned
                .entries
                .into_iter()
                .map(|entry| DataProjectCommitEntry {
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
        });
    }
    let archived = capture_project(&connection, project_id, true)?;
    let verification = capture_project(&connection, project_id, false)?;
    if archived.dataset_ids != verification.dataset_ids
        || archived.dataset_metadata != verification.dataset_metadata
        || archived
            .entries
            .iter()
            .map(|entry| &entry.entry)
            .ne(verification.entries.iter().map(|entry| &entry.entry))
    {
        return Err("dataset or labels changed while archiving; commit aborted".to_string());
    }
    let current = archived;
    let parent_id = latest_head(&connection, project_id)?;
    let (parent_datasets, parent_metadata, parent_entries) =
        if let Some(parent) = parent_id.as_deref() {
            (
                load_commit_dataset_ids(&connection, parent)?,
                load_commit_dataset_metadata(&connection, parent)?,
                load_commit_entries(&connection, parent)?,
            )
        } else {
            (Vec::new(), BTreeMap::new(), Vec::new())
        };
    let changes = make_changes(
        &current,
        &parent_datasets,
        &parent_metadata,
        &parent_entries,
    );
    if changes.is_empty() {
        return Err("working tree is clean; refusing empty data-project commit".to_string());
    }
    let commit_id = Uuid::new_v4().to_string();
    let created_at = now_string();
    let dataset_ids_json = serde_json::to_string(&current.dataset_ids)
        .map_err(|error| format!("serialize commit datasets: {error}"))?;
    let dataset_metadata_json = serde_json::to_string(&current.dataset_metadata)
        .map_err(|error| format!("serialize dataset metadata: {error}"))?;
    let transaction = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    let current_meta = load_project_by_id(&transaction, project_id)?;
    if current_meta.updated_at != baseline.updated_at
        || current_meta.dataset_ids != baseline.dataset_ids
        || latest_head(&transaction, project_id)? != parent_id
    {
        return Err("data project changed while capturing; commit aborted".to_string());
    }
    transaction
        .execute(
            "INSERT INTO data_project_commits (id, data_project_id, parent_id, message, created_at, dataset_ids_json, dataset_metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![commit_id, project_id, parent_id, message, created_at, dataset_ids_json, dataset_metadata_json],
        )
        .map_err(|error| format!("write data-project commit: {error}"))?;
    for (dataset_id, metadata_hash) in &current.dataset_metadata {
        if let Some(metadata_length) = current.dataset_metadata_lengths.get(dataset_id) {
            let metadata_length = sqlite_byte_length(*metadata_length)?;
            transaction
                .execute(
                    "INSERT OR IGNORE INTO data_project_objects (hash, kind, byte_length, created_at) VALUES (?1, 'dataset-metadata', ?2, ?3)",
                    params![metadata_hash, metadata_length, created_at],
                )
                .map_err(|error| format!("record dataset metadata object: {error}"))?;
        }
    }
    for captured in &current.entries {
        let entry = &captured.entry;
        let image_length = sqlite_byte_length(captured.image_length)?;
        let annotation_length = sqlite_byte_length(captured.annotation_length)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO data_project_objects (hash, kind, byte_length, created_at) VALUES (?1, 'image', ?2, ?3)",
                params![entry.image_hash, image_length, created_at],
            )
            .map_err(|error| format!("record image object: {error}"))?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO data_project_objects (hash, kind, byte_length, created_at) VALUES (?1, 'annotation', ?2, ?3)",
                params![entry.annotation_hash, annotation_length, created_at],
            )
            .map_err(|error| format!("record annotation object: {error}"))?;
        transaction
            .execute(
                "INSERT INTO data_project_commit_entries (commit_id, dataset_id, image_id, file_name, image_hash, annotation_hash, status, qa_status, split, width, height) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    commit_id,
                    entry.dataset_id,
                    entry.image_id,
                    entry.file_name,
                    entry.image_hash,
                    entry.annotation_hash,
                    entry.status,
                    entry.qa_status,
                    entry.split,
                    entry.width,
                    entry.height,
                ],
            )
            .map_err(|error| format!("write data-project entry: {error}"))?;
    }
    let advanced = transaction
        .execute(
            "UPDATE data_projects SET head_commit_id = ?2, updated_at = ?3 WHERE id = ?1 AND head_commit_id IS ?4 AND updated_at = ?5",
            params![project_id, commit_id, created_at, parent_id, baseline.updated_at],
        )
        .map_err(|error| format!("advance data-project head: {error}"))?;
    if advanced != 1 {
        return Err("data project changed before HEAD update; commit aborted".to_string());
    }
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(DataProjectCommit {
        id: commit_id,
        data_project_id: project_id.to_string(),
        parent_id,
        message,
        created_at,
        dataset_ids: current.dataset_ids,
        dataset_metadata: current.dataset_metadata,
        project_metadata: crate::data_project_versioning::DataProjectMetadata::default(),
        entries: current
            .entries
            .into_iter()
            .map(|entry| entry.entry)
            .collect(),
    })
}

pub fn list_data_project_commits(project_id: &str) -> Result<Vec<DataProjectCommit>, String> {
    let connection = open_database()?;
    let project = load_project_by_id(&connection, project_id)?;
    if !project_history_authorized(&connection, &project)? {
        return Err("data-project history is outside the active account scope".to_string());
    }
    let mut statement = connection
        .prepare("SELECT id, parent_id, message, created_at, dataset_ids_json, dataset_metadata_json, project_metadata_json FROM data_project_commits WHERE data_project_id = ?1 ORDER BY created_at DESC, id DESC")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map(params![project_id], |row| {
            let dataset_json: String = row.get(4)?;
            let dataset_ids =
                serde_json::from_str::<Vec<String>>(&dataset_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        4,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            let metadata_json: String = row.get(5)?;
            let dataset_metadata = serde_json::from_str::<BTreeMap<String, String>>(&metadata_json)
                .map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
            let project_metadata_json: String = row.get(6)?;
            let project_metadata = crate::data_project_versioning::parse_project_metadata_json(
                &project_metadata_json,
            )
            .map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    6,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
                )
            })?;
            Ok(DataProjectCommit {
                id: row.get(0)?,
                data_project_id: project_id.to_string(),
                parent_id: row.get(1)?,
                message: row.get(2)?,
                created_at: row.get(3)?,
                dataset_ids,
                dataset_metadata,
                project_metadata,
                entries: Vec::new(),
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);
    let mut commits = rows;
    for commit in &mut commits {
        commit.entries = load_commit_entries(&connection, &commit.id)?;
    }
    Ok(commits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_bucket_keeps_raw_and_approved_empty_distinct() {
        assert_eq!(status_bucket("未标注", "", 0), "raw");
        assert_eq!(status_bucket("已标注", "", 0), "inProgress");
        assert_eq!(status_bucket("草稿", "", 0), "inProgress");
        assert_eq!(status_bucket("已标注", "待质检", 2), "pendingReview");
        assert_eq!(status_bucket("草稿", "驳回", 2), "rejected");
        assert_eq!(status_bucket("已标注", "通过", 0), "approved");
        assert_eq!(status_bucket("unannotated", "", 0), "raw");
        assert_eq!(status_bucket("unannotated", "approved", 0), "approved");
        assert_eq!(status_bucket("submitted", "", 2), "pendingReview");
        assert_eq!(status_bucket("draft", "rejected", 2), "rejected");
        assert_eq!(status_bucket("approved", "", 0), "approved");
    }

    #[test]
    fn object_hash_is_content_addressed() {
        let root = objects_root();
        let _ = fs::create_dir_all(&root);
        let path = root.join(format!("test-source-{}", Uuid::new_v4()));
        fs::write(&path, b"content-addressed-test").unwrap();
        let (first, length) = hash_and_store(&path).unwrap();
        let (second, second_length) = hash_and_store(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(length, second_length);
        assert_eq!(
            fs::read(root.join(first)).unwrap(),
            b"content-addressed-test"
        );
        fs::remove_file(path).unwrap();
    }

    fn synthetic_entry(image_hash: &str, annotation_hash: &str) -> CapturedEntry {
        CapturedEntry {
            entry: DataProjectCommitEntry {
                dataset_id: "dataset-a".to_string(),
                image_id: "image-a".to_string(),
                file_name: "image-a.png".to_string(),
                image_hash: image_hash.to_string(),
                annotation_hash: annotation_hash.to_string(),
                status: "草稿".to_string(),
                qa_status: String::new(),
                split: "train".to_string(),
                width: 4,
                height: 4,
            },
            object_count: 1,
            image_length: 1,
            annotation_length: 1,
        }
    }

    #[test]
    fn changes_track_membership_image_and_metadata_without_false_dirty_state() {
        let mut current = CapturedProject {
            dataset_ids: vec!["dataset-a".to_string()],
            dataset_metadata: BTreeMap::from([("dataset-a".to_string(), "meta-2".to_string())]),
            dataset_metadata_lengths: BTreeMap::from([("dataset-a".to_string(), 1)]),
            entries: vec![synthetic_entry("image-2", "labels-2")],
        };
        let head_entry = synthetic_entry("image-1", "labels-1").entry;
        let head = CapturedProject {
            dataset_ids: vec!["dataset-a".to_string()],
            dataset_metadata: BTreeMap::from([("dataset-a".to_string(), "meta-1".to_string())]),
            dataset_metadata_lengths: BTreeMap::from([("dataset-a".to_string(), 1)]),
            entries: vec![synthetic_entry("image-1", "labels-1")],
        };
        let changes = make_changes(
            &current,
            &head.dataset_ids,
            &head.dataset_metadata,
            std::slice::from_ref(&head_entry),
        );
        assert!(changes
            .iter()
            .any(|change| change.kind == "dataset-changed"));
        assert!(changes.iter().any(|change| change.kind == "image-changed"));

        current.entries[0] = synthetic_entry("image-1", "labels-1");
        current.dataset_metadata = head.dataset_metadata.clone();
        let clean = make_changes(
            &current,
            &head.dataset_ids,
            &head.dataset_metadata,
            &head
                .entries
                .iter()
                .map(|entry| entry.entry.clone())
                .collect::<Vec<_>>(),
        );
        assert!(clean.is_empty(), "identical capture must be clean");
    }

    #[test]
    fn removed_dataset_remains_a_diff_against_history() {
        let current = CapturedProject {
            dataset_ids: Vec::new(),
            dataset_metadata: BTreeMap::new(),
            dataset_metadata_lengths: BTreeMap::new(),
            entries: Vec::new(),
        };
        let previous = synthetic_entry("image-1", "labels-1").entry;
        let changes = make_changes(
            &current,
            &["dataset-a".to_string()],
            &BTreeMap::from([("dataset-a".to_string(), "meta-1".to_string())]),
            &[previous],
        );
        assert!(changes
            .iter()
            .any(|change| change.kind == "dataset-removed"));
        assert!(changes.iter().any(|change| change.kind == "image-removed"));
    }

    #[test]
    fn legacy_scope_without_metadata_evidence_is_rejected() {
        let metadata = BTreeMap::from([("dataset-a".to_string(), "missing-object".to_string())]);
        let trusted_scopes = BTreeSet::new();
        assert!(!commit_metadata_authorized(&metadata, None, &trusted_scopes).unwrap());
        assert!(commit_metadata_authorized(&BTreeMap::new(), None, &trusted_scopes).unwrap());
    }

    #[test]
    fn real_dataset_membership_label_change_and_removal_are_committed() {
        let dataset_name = format!("Data Project Fixture {}", Uuid::new_v4());
        let dataset =
            crate::datasets::create_dataset_project(&dataset_name, "yolo-detect", "demo-bbox")
                .expect("create real dataset fixture");
        let project = create_data_project("Fixture project", "real dataset history").unwrap();
        let second_project =
            create_data_project("Fixture second project", "membership move").unwrap();

        assign_dataset_to_data_project(Some(&project.id), &dataset.id).unwrap();
        let listed = list_data_projects().unwrap();
        assert_eq!(
            listed
                .iter()
                .find(|item| item.id == project.id)
                .unwrap()
                .dataset_ids,
            vec![dataset.id.clone()]
        );

        // The dataset membership is unique and durable: moving it updates the
        // index rather than creating a second association.
        assign_dataset_to_data_project(Some(&second_project.id), &dataset.id).unwrap();
        let moved = list_data_projects().unwrap();
        assert!(moved
            .iter()
            .find(|item| item.id == project.id)
            .unwrap()
            .dataset_ids
            .is_empty());
        assert_eq!(
            moved
                .iter()
                .find(|item| item.id == second_project.id)
                .unwrap()
                .dataset_ids,
            vec![dataset.id.clone()]
        );
        assign_dataset_to_data_project(Some(&project.id), &dataset.id).unwrap();

        get_data_project_working_tree(&project.id).unwrap();
        assert!(
            !objects_root().exists(),
            "status checks must not archive data"
        );
        let paths = project_fs::project_paths(&dataset.id);
        let label_path = paths.raw.join("labels").join("train").join("demo_001.txt");
        let source_label_before = fs::read(&label_path).unwrap();
        let source_image_path = paths.raw.join("images").join("train").join("demo_001.png");
        let source_image_before = fs::read(&source_image_path).unwrap();
        let first = commit_data_project(&project.id, "initial fixture import").unwrap();
        assert_eq!(first.entries.len(), 3);
        assert!(commit_data_project(&project.id, "empty fixture commit").is_err());

        // The first post-commit tree read materializes the private checkout.
        // Edits must go through the same SampleRepository path used by the
        // annotation UI; changing the imported YOLO source is intentionally
        // not a branch edit.
        get_data_project_working_tree(&project.id).unwrap();
        let repository = SampleRepository::new();
        let fixture_image_id = first.entries.iter().find(|entry| entry.file_name.ends_with("demo_001.png")).unwrap().image_id.clone();
        let state = repository.image_annotation_state(&dataset.id, &fixture_image_id);
        let mut edited_objects = state.objects.clone();
        if let Some(object) = edited_objects.first_mut() {
            object
                .attributes
                .insert("fixtureEdit".to_string(), serde_json::json!(true));
        }
        repository
            .save_image_annotations_with_revision(
                &dataset.id,
                &fixture_image_id,
                state.revision.clone(),
                edited_objects,
            )
            .unwrap();
        assert_eq!(fs::read(&label_path).unwrap(), source_label_before);
        let tree = get_data_project_working_tree(&project.id).unwrap();
        assert!(tree
            .changes
            .iter()
            .any(|change| change.kind == "annotation-changed" && change.image_id.is_some()));
        let second = commit_data_project(&project.id, "save fixture label").unwrap();
        assert_eq!(second.parent_id.as_deref(), Some(first.id.as_str()));

        let original_entry = second
            .entries
            .iter()
            .find(|entry| entry.image_id == fixture_image_id)
            .unwrap();
        let image_path = repository
            .image_path(&dataset.id, &original_entry.image_id)
            .unwrap();
        assert_ne!(
            image_path, source_image_path,
            "the workbench must resolve the first checkout to its controlled image copy"
        );
        let original_bytes = fs::read(&source_image_path).unwrap();
        image::RgbImage::from_pixel(
            original_entry.width,
            original_entry.height,
            image::Rgb([230, 20, 30]),
        )
        .save(&image_path)
        .unwrap();
        let image_commit = commit_data_project(&project.id, "replace fixture image").unwrap();
        assert_ne!(
            image_commit.entries[0].image_hash,
            original_entry.image_hash
        );
        assert_eq!(
            fs::read(&source_image_path).unwrap(),
            source_image_before,
            "branch image edits must not overwrite the imported source"
        );
        assert_eq!(
            fs::read(objects_root().join(&original_entry.image_hash)).unwrap(),
            original_bytes,
            "history retains the original image bytes"
        );

        let annotation_path = crate::data_project_versioning::active_data_project_checkout_annotation_path_for_dataset(
            &dataset.id,
            &original_entry.image_id,
        )
        .unwrap()
        .unwrap();
        let valid_annotation = fs::read(&annotation_path).unwrap();
        fs::write(&annotation_path, b"{invalid annotation").unwrap();
        assert!(commit_data_project(&project.id, "invalid checkout labels").is_err());
        assert_eq!(
            list_data_project_commits(&project.id).unwrap()[0].id,
            image_commit.id,
            "failed captures must not advance HEAD"
        );
        fs::write(&annotation_path, valid_annotation).unwrap();

        assign_dataset_to_data_project(None, &dataset.id).unwrap();
        let removed_tree = get_data_project_working_tree(&project.id).unwrap();
        assert!(removed_tree
            .changes
            .iter()
            .any(|change| change.kind == "dataset-removed"));
        assert!(removed_tree
            .changes
            .iter()
            .any(|change| change.kind == "image-removed"));
        let removed = commit_data_project(&project.id, "remove fixture dataset").unwrap();
        assert!(removed.entries.is_empty());

        let history = list_data_project_commits(&project.id).unwrap();
        assert!(history
            .iter()
            .flat_map(|commit| commit.entries.iter())
            .any(|entry| entry.dataset_id == dataset.id));
    }
}

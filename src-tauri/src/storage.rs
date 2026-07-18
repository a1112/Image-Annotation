use crate::project_fs::ProjectManifest;
use rusqlite::{
    params, params_from_iter, types::Value as SqlValue, Connection, OpenFlags, OptionalExtension,
    TransactionBehavior,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq)]
pub struct StoredImage {
    pub id: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub split: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSampleMetadata {
    pub split: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleMutationEvidence {
    None,
    Committed,
    Compensated,
    Indeterminate,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredClass {
    pub id: u32,
    pub label: String,
    pub color: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSampleClass {
    pub id: u32,
    pub label: String,
    pub object_count: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredSample {
    pub image: StoredImage,
    pub annotation_revision: Option<String>,
    pub annotation_updated_at: Option<String>,
    pub annotation_count: u32,
    pub classes: Vec<StoredSampleClass>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StoredSampleFilter {
    pub sample_id: Option<String>,
    pub split: Option<String>,
    pub status: Option<String>,
    pub qa_status: Option<String>,
    pub class_id: Option<u32>,
    pub label: Option<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredSamplePage {
    pub total: u64,
    pub items: Vec<StoredSample>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredDatasetSource {
    pub format: String,
    pub mode: String,
    pub root_path: String,
    pub annotation_path: Option<String>,
    pub options_json: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct StoredImageSource {
    pub image_id: String,
    pub relative_path: String,
    pub external_id: Option<String>,
    pub annotation_path: Option<String>,
    pub source_version: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationPayload {
    pub image_id: String,
    pub revision: String,
    pub object_json: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationSaveResult {
    pub revision: String,
    pub saved_at: String,
    pub audit_event_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnnotationRevisionExpectation {
    Missing,
    AnyExisting,
    Strong(Vec<String>),
    Never,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteAnnotationSaveResult {
    pub revision: String,
    pub saved_at: String,
    pub previous_annotation: Option<AnnotationPayload>,
    pub previous_metadata: StoredSampleMetadata,
    pub previous_source: Option<StoredImageSource>,
    pub applied_metadata: StoredSampleMetadata,
    pub applied_source: StoredImageSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteWorkflowState {
    pub image_id: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteMutationError {
    NotFound,
    RevisionConflict,
    Storage(String),
}

impl From<String> for RemoteMutationError {
    fn from(error: String) -> Self {
        Self::Storage(error)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AnnotationVersionRecord {
    pub id: String,
    pub image_id: String,
    pub revision: String,
    pub object_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotRecord {
    pub id: String,
    pub name: String,
    pub image_count: u32,
    pub manifest_json: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExportRecord {
    pub id: String,
    pub snapshot_id: String,
    pub format: String,
    pub status: String,
    pub output_path: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportRecord {
    pub id: String,
    pub source_path: String,
    pub status: String,
    pub message: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRecord {
    pub id: String,
    pub name: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskItemRecord {
    pub id: String,
    pub task_id: String,
    pub image_id: String,
    pub status: String,
    pub qa_status: String,
    pub review_note: Option<String>,
    pub locked_at: Option<String>,
}

pub fn initialize_project_database(path: &Path) -> Result<(), String> {
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE IF NOT EXISTS projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                source_dataset_key TEXT NOT NULL,
                format TEXT NOT NULL,
                root_path TEXT NOT NULL DEFAULT '',
                class_count INTEGER NOT NULL DEFAULT 0,
                image_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS images (
                id TEXT PRIMARY KEY,
                file_name TEXT NOT NULL,
                width INTEGER NOT NULL,
                height INTEGER NOT NULL,
                split TEXT NOT NULL,
                status TEXT NOT NULL,
                qa_status TEXT NOT NULL DEFAULT '',
                review_note TEXT
            );
            CREATE TABLE IF NOT EXISTS classes (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL,
                color TEXT NOT NULL,
                shortcut TEXT,
                enabled INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE IF NOT EXISTS label_schema_versions (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                class_count INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS dataset_sources (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                format TEXT NOT NULL,
                mode TEXT NOT NULL,
                root_path TEXT NOT NULL,
                annotation_path TEXT,
                options_json TEXT NOT NULL DEFAULT '{}'
            );
            CREATE TABLE IF NOT EXISTS image_sources (
                image_id TEXT PRIMARY KEY,
                relative_path TEXT NOT NULL,
                external_id TEXT,
                annotation_path TEXT,
                source_version TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE IF NOT EXISTS annotations (
                id TEXT PRIMARY KEY,
                image_id TEXT NOT NULL,
                revision TEXT NOT NULL DEFAULT '',
                object_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS sample_class_links (
                image_id TEXT NOT NULL,
                class_id INTEGER NOT NULL,
                object_count INTEGER NOT NULL DEFAULT 1,
                PRIMARY KEY (image_id, class_id)
            );
            CREATE INDEX IF NOT EXISTS idx_sample_class_links_class
                ON sample_class_links (class_id, image_id);
            CREATE TABLE IF NOT EXISTS annotation_versions (
                id TEXT PRIMARY KEY,
                image_id TEXT NOT NULL,
                revision TEXT NOT NULL,
                object_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS task_items (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                image_id TEXT NOT NULL,
                status TEXT NOT NULL,
                qa_status TEXT NOT NULL DEFAULT '',
                review_note TEXT,
                locked_at TEXT
            );
            CREATE TABLE IF NOT EXISTS qa_reviews (
                id TEXT PRIMARY KEY,
                image_id TEXT NOT NULL,
                decision TEXT NOT NULL,
                note TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS snapshots (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                image_count INTEGER NOT NULL,
                manifest_json TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS exports (
                id TEXT PRIMARY KEY,
                snapshot_id TEXT NOT NULL,
                format TEXT NOT NULL,
                status TEXT NOT NULL,
                output_path TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS imports (
                id TEXT PRIMARY KEY,
                source_path TEXT NOT NULL,
                status TEXT NOT NULL,
                message TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS audit_events (
                id TEXT PRIMARY KEY,
                action TEXT NOT NULL,
                image_id TEXT,
                message TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL
            );
            "#,
        )
        .map_err(|err| err.to_string())?;
    for statement in [
        "ALTER TABLE projects ADD COLUMN root_path TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE projects ADD COLUMN class_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE projects ADD COLUMN image_count INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE projects ADD COLUMN updated_at TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE images ADD COLUMN qa_status TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE images ADD COLUMN review_note TEXT",
        "ALTER TABLE classes ADD COLUMN shortcut TEXT",
        "ALTER TABLE classes ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
        "ALTER TABLE annotations ADD COLUMN revision TEXT NOT NULL DEFAULT ''",
    ] {
        let _ = connection.execute(statement, []);
    }
    validate_project_database_artifacts(path)
}

pub fn upsert_project_index(
    path: &Path,
    manifest: &ProjectManifest,
    images: &[StoredImage],
    classes: &[StoredClass],
) -> Result<(), String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    transaction
        .execute(
            r#"
            INSERT INTO projects (id, name, source_dataset_key, format, root_path, class_count, image_count, created_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            ON CONFLICT(id) DO UPDATE SET
              name = excluded.name,
              source_dataset_key = excluded.source_dataset_key,
              format = excluded.format,
              root_path = excluded.root_path,
              class_count = excluded.class_count,
              image_count = excluded.image_count,
              created_at = excluded.created_at
            "#,
            params![
                manifest.id,
                manifest.name,
                manifest.source_dataset_key,
                manifest.format,
                manifest.root_path,
                manifest.class_count,
                manifest.image_count,
                manifest.created_at,
            ],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM sample_class_links", [])
        .map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM images", [])
        .map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM classes", [])
        .map_err(|err| err.to_string())?;

    for image in images {
        transaction
            .execute(
                "INSERT INTO images (id, file_name, width, height, split, status, qa_status, review_note) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    image.id,
                    image.file_name,
                    image.width,
                    image.height,
                    image.split,
                    image.status,
                    image.qa_status,
                    image.review_note,
                ],
            )
            .map_err(|err| err.to_string())?;
    }

    for class in classes {
        transaction
            .execute(
                "INSERT INTO classes (id, label, color) VALUES (?1, ?2, ?3)",
                params![class.id, class.label, class.color],
            )
            .map_err(|err| err.to_string())?;
    }

    validate_project_database_artifacts(path)?;
    transaction.commit().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)
}

pub fn read_project_manifest(path: &Path) -> Result<Option<ProjectManifest>, String> {
    if !path.exists() {
        return Ok(None);
    }
    validate_project_database_artifacts(path)?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    let manifest = connection
        .query_row(
            "SELECT id, name, source_dataset_key, format, root_path, class_count, image_count, created_at FROM projects LIMIT 1",
            [],
            |row| {
                Ok(ProjectManifest {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    source_dataset_key: row.get(2)?,
                    format: row.get(3)?,
                    root_path: row.get(4)?,
                    class_count: row.get::<_, u32>(5)?,
                    image_count: row.get::<_, u32>(6)?,
                    created_at: row.get(7)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    Ok(manifest)
}

pub fn update_project_name(path: &Path, project_id: &str, name: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    let updated = transaction
        .execute(
            "UPDATE projects SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now_unix_string(), project_id],
        )
        .map_err(|err| err.to_string())?;
    if updated != 1 {
        return Err("project index was not found".to_string());
    }
    validate_project_database_artifacts(path)?;
    transaction.commit().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)
}

pub fn validate_project_database_artifacts(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "project database path has no parent".to_string())?;
    let canonical_parent = fs::canonicalize(parent).map_err(|error| error.to_string())?;
    for artifact in [
        path.to_path_buf(),
        sqlite_sidecar_path(path, "-journal"),
        sqlite_sidecar_path(path, "-wal"),
        sqlite_sidecar_path(path, "-shm"),
    ] {
        validate_optional_database_artifact(&canonical_parent, &artifact)?;
    }
    Ok(())
}

fn open_project_database_writable(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())
}

fn sqlite_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut sidecar = OsString::from(path.as_os_str());
    sidecar.push(suffix);
    PathBuf::from(sidecar)
}

fn validate_optional_database_artifact(parent: &Path, path: &Path) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
        return Err("project database artifact is not a regular file".to_string());
    }
    let canonical = fs::canonicalize(path).map_err(|error| error.to_string())?;
    if canonical.parent() == Some(parent) {
        Ok(())
    } else {
        Err("project database artifact is outside its project directory".to_string())
    }
}

fn is_symlink_or_reparse(metadata: &fs::Metadata) -> bool {
    if metadata.file_type().is_symlink() {
        return true;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        false
    }
}

pub fn write_dataset_source(path: &Path, source: &StoredDatasetSource) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            r#"
            INSERT INTO dataset_sources (id, format, mode, root_path, annotation_path, options_json)
            VALUES (1, ?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(id) DO UPDATE SET
              format = excluded.format,
              mode = excluded.mode,
              root_path = excluded.root_path,
              annotation_path = excluded.annotation_path,
              options_json = excluded.options_json
            "#,
            params![
                source.format,
                source.mode,
                source.root_path,
                source.annotation_path,
                source.options_json,
            ],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn read_dataset_source(path: &Path) -> Result<Option<StoredDatasetSource>, String> {
    if !path.exists() {
        return Ok(None);
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .query_row(
            "SELECT format, mode, root_path, annotation_path, options_json FROM dataset_sources WHERE id = 1",
            [],
            |row| {
                Ok(StoredDatasetSource {
                    format: row.get(0)?,
                    mode: row.get(1)?,
                    root_path: row.get(2)?,
                    annotation_path: row.get(3)?,
                    options_json: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
}

pub fn replace_image_sources(path: &Path, sources: &[StoredImageSource]) -> Result<(), String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM image_sources", [])
        .map_err(|err| err.to_string())?;
    for source in sources {
        transaction
            .execute(
                "INSERT INTO image_sources (image_id, relative_path, external_id, annotation_path, source_version) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    source.image_id,
                    source.relative_path,
                    source.external_id,
                    source.annotation_path,
                    source.source_version,
                ],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())
}

pub fn write_image_source(path: &Path, source: &StoredImageSource) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            r#"
            INSERT INTO image_sources (image_id, relative_path, external_id, annotation_path, source_version)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(image_id) DO UPDATE SET
              relative_path = excluded.relative_path,
              external_id = excluded.external_id,
              annotation_path = excluded.annotation_path,
              source_version = excluded.source_version
            "#,
            params![
                source.image_id,
                source.relative_path,
                source.external_id,
                source.annotation_path,
                source.source_version,
            ],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn read_image_source(path: &Path, image_id: &str) -> Result<Option<StoredImageSource>, String> {
    if !path.exists() {
        return Ok(None);
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .query_row(
            "SELECT image_id, relative_path, external_id, annotation_path, source_version FROM image_sources WHERE image_id = ?1",
            params![image_id],
            |row| {
                Ok(StoredImageSource {
                    image_id: row.get(0)?,
                    relative_path: row.get(1)?,
                    external_id: row.get(2)?,
                    annotation_path: row.get(3)?,
                    source_version: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
}

pub fn read_image_sources(path: &Path) -> Result<Vec<StoredImageSource>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT image_id, relative_path, external_id, annotation_path, source_version FROM image_sources ORDER BY image_id",
        )
        .map_err(|err| err.to_string())?;
    let sources = statement
        .query_map([], |row| {
            Ok(StoredImageSource {
                image_id: row.get(0)?,
                relative_path: row.get(1)?,
                external_id: row.get(2)?,
                annotation_path: row.get(3)?,
                source_version: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(sources)
}

pub fn read_images(path: &Path, split: Option<&str>) -> Result<Vec<StoredImage>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let sql = if split.is_some() {
        "SELECT id, file_name, width, height, split, status, qa_status, review_note FROM images WHERE split = ?1 ORDER BY file_name"
    } else {
        "SELECT id, file_name, width, height, split, status, qa_status, review_note FROM images ORDER BY file_name"
    };
    let mut statement = connection.prepare(sql).map_err(|err| err.to_string())?;
    let rows = if let Some(split) = split {
        statement
            .query_map(params![split], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    } else {
        statement
            .query_map([], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    };
    Ok(rows)
}

pub fn read_images_page(
    path: &Path,
    split: Option<&str>,
    offset: u32,
    limit: u32,
) -> Result<Vec<StoredImage>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let limit = limit.clamp(1, 500);
    let sql = if split.is_some() {
        "SELECT id, file_name, width, height, split, status, qa_status, review_note FROM images WHERE split = ?1 ORDER BY file_name LIMIT ?2 OFFSET ?3"
    } else {
        "SELECT id, file_name, width, height, split, status, qa_status, review_note FROM images ORDER BY file_name LIMIT ?1 OFFSET ?2"
    };
    let mut statement = connection.prepare(sql).map_err(|err| err.to_string())?;
    let rows = if let Some(split) = split {
        statement
            .query_map(params![split, limit, offset], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    } else {
        statement
            .query_map(params![limit, offset], stored_image_from_row)
            .map_err(|err| err.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| err.to_string())?
    };
    Ok(rows)
}

pub fn read_classes(path: &Path) -> Result<Vec<StoredClass>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, label, color FROM classes ORDER BY id")
        .map_err(|err| err.to_string())?;
    let classes = statement
        .query_map([], |row| {
            Ok(StoredClass {
                id: row.get(0)?,
                label: row.get(1)?,
                color: row.get(2)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(classes)
}

pub fn read_enabled_classes(path: &Path) -> Result<Vec<StoredClass>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let mut statement = connection
        .prepare("SELECT id, label, color FROM classes WHERE enabled = 1 ORDER BY id")
        .map_err(|err| err.to_string())?;
    let classes = statement
        .query_map([], |row| {
            Ok(StoredClass {
                id: row.get(0)?,
                label: row.get(1)?,
                color: row.get(2)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(classes)
}

pub fn refresh_sample_class_links(
    path: &Path,
    classification_links: &[(String, u32)],
) -> Result<(), String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute("DELETE FROM sample_class_links", [])
        .map_err(|err| err.to_string())?;
    for (image_id, class_id) in classification_links {
        transaction
            .execute(
                r#"
                INSERT INTO sample_class_links (image_id, class_id, object_count)
                VALUES (?1, ?2, 1)
                ON CONFLICT(image_id, class_id) DO UPDATE SET
                    object_count = MAX(sample_class_links.object_count, excluded.object_count)
                "#,
                params![image_id, class_id],
            )
            .map_err(|err| err.to_string())?;
    }
    validate_project_database_artifacts(path)?;
    transaction.commit().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)
}

pub fn has_sample_class_links(path: &Path) -> Result<bool, String> {
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let exists = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sample_class_links LIMIT 1)",
            [],
            |row| row.get(0),
        )
        .map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    Ok(exists)
}

pub fn query_samples(
    path: &Path,
    filter: &StoredSampleFilter,
    offset: u32,
    limit: u32,
) -> Result<StoredSamplePage, String> {
    if !path.exists() {
        return Ok(StoredSamplePage {
            total: 0,
            items: Vec::new(),
        });
    }
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    validate_project_database_artifacts(path)?;
    let (where_sql, values) = sample_filter_sql(filter);
    let total_sql = format!("SELECT COUNT(*) FROM images AS i {where_sql}");
    let total = connection
        .query_row(&total_sql, params_from_iter(values.iter()), |row| {
            row.get::<_, u64>(0)
        })
        .map_err(|err| err.to_string())?;

    let mut page_values = values;
    page_values.push(SqlValue::Integer(i64::from(limit)));
    page_values.push(SqlValue::Integer(i64::from(offset)));
    let row_sql = format!(
        r#"
        SELECT
            i.id,
            i.file_name,
            i.width,
            i.height,
            i.split,
            i.status,
            i.qa_status,
            i.review_note,
            NULLIF(a.revision, ''),
            a.updated_at,
            CASE
                WHEN a.object_json IS NOT NULL THEN json_array_length(a.object_json)
                ELSE (
                    SELECT COALESCE(SUM(links.object_count), 0)
                    FROM sample_class_links AS links
                    WHERE links.image_id = i.id
                )
            END
        FROM images AS i
        LEFT JOIN annotations AS a ON a.image_id = i.id
        {where_sql}
        ORDER BY i.file_name, i.id
        LIMIT ? OFFSET ?
        "#
    );
    let mut statement = connection
        .prepare(&row_sql)
        .map_err(|err| err.to_string())?;
    let mut items = statement
        .query_map(params_from_iter(page_values.iter()), |row| {
            Ok(StoredSample {
                image: stored_image_from_row(row)?,
                annotation_revision: row.get(8)?,
                annotation_updated_at: row.get(9)?,
                annotation_count: row.get(10)?,
                classes: Vec::new(),
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;

    attach_sample_classes(&connection, &mut items)?;
    validate_project_database_artifacts(path)?;
    Ok(StoredSamplePage { total, items })
}

pub fn update_sample_metadata(
    path: &Path,
    image_id: &str,
    operation_id: &str,
    split: Option<&str>,
    status: Option<&str>,
    qa_status: Option<&str>,
    review_note: Option<&str>,
) -> Result<bool, String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    let updated = transaction
        .execute(
            r#"
            UPDATE images
            SET
                split = COALESCE(?2, split),
                status = COALESCE(?3, status),
                qa_status = COALESCE(?4, qa_status),
                review_note = COALESCE(?5, review_note)
            WHERE id = ?1
            "#,
            params![image_id, split, status, qa_status, review_note],
        )
        .map_err(|err| err.to_string())?;
    if updated == 1 {
        transaction
            .execute(
                "INSERT INTO audit_events (id, action, image_id, message, created_at)
                 VALUES (?1, 'sample.update', ?2, '更新样本元数据', ?3)",
                params![operation_id, image_id, now_unix_string()],
            )
            .map_err(|err| err.to_string())?;
    }
    validate_project_database_artifacts(path)?;
    transaction.commit().map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    Ok(updated == 1)
}

pub fn read_sample_metadata(
    path: &Path,
    image_id: &str,
) -> Result<Option<StoredSampleMetadata>, String> {
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let metadata = connection
        .query_row(
            "SELECT split, status, qa_status, review_note
             FROM images WHERE id = ?1",
            [image_id],
            |row| {
                Ok(StoredSampleMetadata {
                    split: row.get(0)?,
                    status: row.get(1)?,
                    qa_status: row.get(2)?,
                    review_note: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;
    Ok(metadata)
}

pub fn sample_mutation_evidence(
    path: &Path,
    operation_id: &str,
    image_id: &str,
) -> Result<SampleMutationEvidence, String> {
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let rollback_id = format!("{operation_id}:rollback");
    let mut statement = connection
        .prepare(
            "SELECT id, action, image_id
             FROM audit_events
             WHERE id = ?1 OR id = ?2
             ORDER BY id",
        )
        .map_err(|err| err.to_string())?;
    let records = statement
        .query_map(params![operation_id, rollback_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    validate_project_database_artifacts(path)?;

    let mut committed = false;
    let mut compensated = false;
    for (id, action, recorded_image_id) in records {
        let valid_image = recorded_image_id.as_deref() == Some(image_id);
        if id == operation_id {
            if action != "sample.update" || !valid_image {
                return Ok(SampleMutationEvidence::Indeterminate);
            }
            committed = true;
        } else if id == rollback_id {
            if action != "sample.update.rollback" || !valid_image {
                return Ok(SampleMutationEvidence::Indeterminate);
            }
            compensated = true;
        } else {
            return Ok(SampleMutationEvidence::Indeterminate);
        }
    }

    if compensated {
        Ok(SampleMutationEvidence::Compensated)
    } else if committed {
        Ok(SampleMutationEvidence::Committed)
    } else {
        Ok(SampleMutationEvidence::None)
    }
}

pub fn restore_sample_metadata(
    path: &Path,
    image_id: &str,
    operation_id: &str,
    split: &str,
    status: &str,
    qa_status: &str,
    review_note: Option<&str>,
) -> Result<(), String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    let updated = transaction
        .execute(
            "UPDATE images
             SET split = ?2, status = ?3, qa_status = ?4, review_note = ?5
             WHERE id = ?1",
            params![image_id, split, status, qa_status, review_note],
        )
        .map_err(|err| err.to_string())?;
    if updated == 1 {
        transaction
            .execute(
                "INSERT INTO audit_events (id, action, image_id, message, created_at)
                 VALUES (?1, 'sample.update.rollback', ?2, '回滚样本元数据更新', ?3)",
                params![
                    format!("{operation_id}:rollback"),
                    image_id,
                    now_unix_string()
                ],
            )
            .map_err(|err| err.to_string())?;
    }
    validate_project_database_artifacts(path)?;
    if updated == 1 {
        transaction.commit().map_err(|err| err.to_string())?;
        validate_project_database_artifacts(path)
    } else {
        Err("sample metadata rollback target was not found".to_string())
    }
}

fn open_project_database_read_only(path: &Path) -> Result<Connection, String> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW,
    )
    .map_err(|error| error.to_string())
}

fn sample_filter_sql(filter: &StoredSampleFilter) -> (String, Vec<SqlValue>) {
    let mut clauses = Vec::<String>::new();
    let mut values = Vec::new();
    if let Some(sample_id) = &filter.sample_id {
        clauses.push("i.id = ?".to_string());
        values.push(SqlValue::Text(sample_id.clone()));
    }
    if let Some(split) = &filter.split {
        clauses.push("i.split = ?".to_string());
        values.push(SqlValue::Text(split.clone()));
    }
    if let Some(status) = &filter.status {
        clauses.push("i.status = ?".to_string());
        values.push(SqlValue::Text(status.clone()));
    }
    if let Some(qa_status) = &filter.qa_status {
        clauses.push("i.qa_status = ?".to_string());
        values.push(SqlValue::Text(qa_status.clone()));
    }
    if filter.class_id.is_some() || filter.label.is_some() {
        let mut link_clauses = vec!["links.image_id = i.id".to_string()];
        let mut annotation_clauses = vec![
            "annotations.image_id = i.id".to_string(),
            "json_type(objects.value, '$.classId') = 'integer'".to_string(),
        ];
        if let Some(class_id) = filter.class_id {
            link_clauses.push("links.class_id = ?".to_string());
            values.push(SqlValue::Integer(i64::from(class_id)));
        }
        if let Some(label) = &filter.label {
            link_clauses.push("LOWER(classes.label) = LOWER(?)".to_string());
            values.push(SqlValue::Text(label.clone()));
        }
        if let Some(class_id) = filter.class_id {
            annotation_clauses
                .push("CAST(json_extract(objects.value, '$.classId') AS INTEGER) = ?".to_string());
            values.push(SqlValue::Integer(i64::from(class_id)));
        }
        if let Some(label) = &filter.label {
            annotation_clauses.push(
                "LOWER(COALESCE(
                    json_extract(objects.value, '$.label'),
                    annotation_classes.label
                )) = LOWER(?)"
                    .to_string(),
            );
            values.push(SqlValue::Text(label.clone()));
        }
        clauses.push(format!(
            "EXISTS (
                SELECT 1
                FROM sample_class_links AS links
                JOIN classes ON classes.id = links.class_id
                WHERE {}
                UNION ALL
                SELECT 1
                FROM annotations
                JOIN json_each(annotations.object_json) AS objects
                LEFT JOIN classes AS annotation_classes
                    ON annotation_classes.id =
                        CAST(json_extract(objects.value, '$.classId') AS INTEGER)
                WHERE {}
            )",
            link_clauses.join(" AND "),
            annotation_clauses.join(" AND ")
        ));
    }
    if let Some(query) = &filter.query {
        clauses.push(
            "(LOWER(i.id) LIKE ? ESCAPE '\\' OR LOWER(i.file_name) LIKE ? ESCAPE '\\')".into(),
        );
        let pattern = format!("%{}%", escape_like_pattern(&query.to_lowercase()));
        values.push(SqlValue::Text(pattern.clone()));
        values.push(SqlValue::Text(pattern));
    }
    if clauses.is_empty() {
        (String::new(), values)
    } else {
        (format!("WHERE {}", clauses.join(" AND ")), values)
    }
}

fn attach_sample_classes(
    connection: &Connection,
    items: &mut [StoredSample],
) -> Result<(), String> {
    if items.is_empty() {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", items.len())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        r#"
        SELECT
            associations.image_id,
            associations.class_id,
            associations.label,
            MAX(associations.object_count)
        FROM (
            SELECT
                links.image_id,
                classes.id AS class_id,
                classes.label,
                links.object_count
            FROM sample_class_links AS links
            JOIN classes ON classes.id = links.class_id
            WHERE links.image_id IN ({placeholders})
            UNION ALL
            SELECT
                annotations.image_id,
                CAST(json_extract(objects.value, '$.classId') AS INTEGER) AS class_id,
                COALESCE(
                    json_extract(objects.value, '$.label'),
                    annotation_classes.label
                ) AS label,
                COUNT(*) AS object_count
            FROM annotations
            JOIN json_each(annotations.object_json) AS objects
            LEFT JOIN classes AS annotation_classes
                ON annotation_classes.id =
                    CAST(json_extract(objects.value, '$.classId') AS INTEGER)
            WHERE annotations.image_id IN ({placeholders})
                AND json_type(objects.value, '$.classId') = 'integer'
            GROUP BY
                annotations.image_id,
                CAST(json_extract(objects.value, '$.classId') AS INTEGER),
                COALESCE(
                    json_extract(objects.value, '$.label'),
                    annotation_classes.label
                )
        ) AS associations
        GROUP BY
            associations.image_id,
            associations.class_id,
            associations.label
        ORDER BY associations.image_id, associations.class_id
        "#
    );
    let mut ids = items
        .iter()
        .map(|item| SqlValue::Text(item.image.id.clone()))
        .collect::<Vec<_>>();
    ids.extend(
        items
            .iter()
            .map(|item| SqlValue::Text(item.image.id.clone())),
    );
    let mut statement = connection.prepare(&sql).map_err(|err| err.to_string())?;
    let rows = statement
        .query_map(params_from_iter(ids.iter()), |row| {
            Ok((
                row.get::<_, String>(0)?,
                StoredSampleClass {
                    id: row.get(1)?,
                    label: row.get(2)?,
                    object_count: row.get(3)?,
                },
            ))
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let mut classes_by_image = BTreeMap::<String, Vec<StoredSampleClass>>::new();
    for (image_id, class) in rows {
        classes_by_image.entry(image_id).or_default().push(class);
    }
    for item in items {
        item.classes = classes_by_image.remove(&item.image.id).unwrap_or_default();
    }
    Ok(())
}

fn escape_like_pattern(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn stored_image_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredImage> {
    Ok(StoredImage {
        id: row.get(0)?,
        file_name: row.get(1)?,
        width: row.get(2)?,
        height: row.get(3)?,
        split: row.get(4)?,
        status: row.get(5)?,
        qa_status: row.get(6)?,
        review_note: row.get(7)?,
    })
}

pub fn save_annotation_payload(
    path: &Path,
    image_id: &str,
    expected_revision: Option<&str>,
    object_json: &str,
) -> Result<AnnotationSaveResult, String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    let current_revision = current_annotation_revision(&connection, image_id)?;
    if current_revision.as_deref() != expected_revision {
        return Err(format!(
            "annotation revision conflict for {image_id}: expected {:?}, current {:?}",
            expected_revision, current_revision
        ));
    }

    let revision = unique_id("rev");
    let saved_at = now_unix_string();
    let audit_event_id = unique_id("audit");
    let version_id = unique_id("ann-version");
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            r#"
            INSERT INTO annotations (id, image_id, revision, object_json, updated_at)
            VALUES (?1, ?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
              revision = excluded.revision,
              object_json = excluded.object_json,
              updated_at = excluded.updated_at
            "#,
            params![image_id, revision, object_json, saved_at],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO annotation_versions (id, image_id, revision, object_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![version_id, image_id, revision, object_json, saved_at],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE images SET status = '草稿', qa_status = '', review_note = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at) VALUES (?1, 'annotation.save', ?2, '保存标注草稿', ?3)",
            params![audit_event_id, image_id, saved_at],
        )
        .map_err(|err| err.to_string())?;
    transaction.commit().map_err(|err| err.to_string())?;

    Ok(AnnotationSaveResult {
        revision,
        saved_at,
        audit_event_id,
    })
}

pub fn read_annotation_payload(
    path: &Path,
    image_id: &str,
) -> Result<Option<AnnotationPayload>, String> {
    if !path.exists() {
        return Ok(None);
    }
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    connection
        .query_row(
            "SELECT image_id, revision, object_json, updated_at FROM annotations WHERE id = ?1",
            params![image_id],
            |row| {
                Ok(AnnotationPayload {
                    image_id: row.get(0)?,
                    revision: row.get(1)?,
                    object_json: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|err| err.to_string())
}

pub fn read_annotation_versions(
    path: &Path,
    image_id: &str,
) -> Result<Vec<AnnotationVersionRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let mut statement = connection
        .prepare(
            "SELECT id, image_id, revision, object_json, created_at
             FROM annotation_versions
             WHERE image_id = ?1
             ORDER BY created_at, rowid",
        )
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map(params![image_id], |row| {
            Ok(AnnotationVersionRecord {
                id: row.get(0)?,
                image_id: row.get(1)?,
                revision: row.get(2)?,
                object_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn save_remote_annotation_payload(
    path: &Path,
    image_id: &str,
    expectation: &AnnotationRevisionExpectation,
    object_json: &str,
    operation_id: &str,
    source: &StoredImageSource,
) -> Result<RemoteAnnotationSaveResult, RemoteMutationError> {
    initialize_project_database(path).map_err(RemoteMutationError::Storage)?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    let mut connection =
        open_project_database_writable(path).map_err(RemoteMutationError::Storage)?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    let previous_metadata = transaction
        .query_row(
            "SELECT split, status, qa_status, review_note FROM images WHERE id = ?1",
            [image_id],
            |row| {
                Ok(StoredSampleMetadata {
                    split: row.get(0)?,
                    status: row.get(1)?,
                    qa_status: row.get(2)?,
                    review_note: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?
        .ok_or(RemoteMutationError::NotFound)?;
    let previous_annotation = transaction
        .query_row(
            "SELECT image_id, revision, object_json, updated_at
             FROM annotations WHERE id = ?1",
            [image_id],
            |row| {
                Ok(AnnotationPayload {
                    image_id: row.get(0)?,
                    revision: row.get(1)?,
                    object_json: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    let previous_source = transaction
        .query_row(
            "SELECT image_id, relative_path, external_id, annotation_path, source_version
             FROM image_sources WHERE image_id = ?1",
            [image_id],
            |row| {
                Ok(StoredImageSource {
                    image_id: row.get(0)?,
                    relative_path: row.get(1)?,
                    external_id: row.get(2)?,
                    annotation_path: row.get(3)?,
                    source_version: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    let current_revision = previous_annotation
        .as_ref()
        .map(|annotation| annotation.revision.as_str());
    if !annotation_expectation_matches(expectation, current_revision) {
        return Err(RemoteMutationError::RevisionConflict);
    }

    let revision = unique_id("rev");
    let saved_at = now_unix_millis_string();
    transaction
        .execute(
            r#"
            INSERT INTO annotations (id, image_id, revision, object_json, updated_at)
            VALUES (?1, ?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
                revision = excluded.revision,
                object_json = excluded.object_json,
                updated_at = excluded.updated_at
            "#,
            params![image_id, revision, object_json, saved_at],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO annotation_versions
                (id, image_id, revision, object_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                format!("{operation_id}:version"),
                image_id,
                revision,
                object_json,
                saved_at
            ],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "UPDATE images
             SET status = '草稿', qa_status = '', review_note = NULL
             WHERE id = ?1",
            [image_id],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            r#"
            INSERT INTO image_sources
                (image_id, relative_path, external_id, annotation_path, source_version)
            VALUES (?1, ?2, ?3, ?4, ?5)
            ON CONFLICT(image_id) DO UPDATE SET
                relative_path = excluded.relative_path,
                external_id = excluded.external_id,
                annotation_path = excluded.annotation_path,
                source_version = excluded.source_version
            "#,
            params![
                source.image_id,
                source.relative_path,
                source.external_id,
                source.annotation_path,
                source.source_version,
            ],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at)
             VALUES (?1, 'annotation.save', ?2, 'remote annotation saved', ?3)",
            params![operation_id, image_id, saved_at],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .commit()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;

    let applied_metadata = StoredSampleMetadata {
        split: previous_metadata.split.clone(),
        status: "草稿".to_string(),
        qa_status: String::new(),
        review_note: None,
    };
    Ok(RemoteAnnotationSaveResult {
        revision,
        saved_at,
        previous_annotation,
        previous_metadata,
        previous_source,
        applied_metadata,
        applied_source: source.clone(),
    })
}

pub fn compensate_remote_annotation_save(
    path: &Path,
    image_id: &str,
    operation_id: &str,
    saved: &RemoteAnnotationSaveResult,
) -> Result<bool, String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let mut connection = open_project_database_writable(path)?;
    validate_project_database_artifacts(path)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let rollback_id = format!("{operation_id}:rollback");
    let rollback_exists = transaction
        .query_row(
            "SELECT action, image_id FROM audit_events WHERE id = ?1",
            [&rollback_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?
        .is_some_and(|(action, event_image_id)| {
            action == "annotation.save.rollback" && event_image_id.as_deref() == Some(image_id)
        });
    if rollback_exists {
        return Ok(true);
    }
    let current_revision = current_annotation_revision(&transaction, image_id)?;
    let committed_event = transaction
        .query_row(
            "SELECT action, image_id FROM audit_events WHERE id = ?1",
            [operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if current_revision.as_deref() != Some(saved.revision.as_str())
        || committed_event
            .as_ref()
            .is_none_or(|(action, event_image_id)| {
                action != "annotation.save" || event_image_id.as_deref() != Some(image_id)
            })
    {
        return Ok(false);
    }
    let current_metadata = transaction
        .query_row(
            "SELECT split, status, qa_status, review_note FROM images WHERE id = ?1",
            [image_id],
            |row| {
                Ok(StoredSampleMetadata {
                    split: row.get(0)?,
                    status: row.get(1)?,
                    qa_status: row.get(2)?,
                    review_note: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let current_source = transaction
        .query_row(
            "SELECT image_id, relative_path, external_id, annotation_path, source_version
             FROM image_sources WHERE image_id = ?1",
            [image_id],
            |row| {
                Ok(StoredImageSource {
                    image_id: row.get(0)?,
                    relative_path: row.get(1)?,
                    external_id: row.get(2)?,
                    annotation_path: row.get(3)?,
                    source_version: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if current_metadata.as_ref() != Some(&saved.applied_metadata)
        || current_source.as_ref() != Some(&saved.applied_source)
    {
        return Ok(false);
    }

    match saved.previous_annotation.as_ref() {
        Some(previous) => {
            transaction
                .execute(
                    "UPDATE annotations
                     SET revision = ?2, object_json = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![
                        image_id,
                        previous.revision,
                        previous.object_json,
                        previous.updated_at
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        None => {
            transaction
                .execute("DELETE FROM annotations WHERE id = ?1", [image_id])
                .map_err(|error| error.to_string())?;
        }
    }
    transaction
        .execute(
            "DELETE FROM annotation_versions WHERE id = ?1 AND revision = ?2",
            params![format!("{operation_id}:version"), saved.revision],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "UPDATE images
             SET split = ?2, status = ?3, qa_status = ?4, review_note = ?5
             WHERE id = ?1",
            params![
                image_id,
                saved.previous_metadata.split,
                saved.previous_metadata.status,
                saved.previous_metadata.qa_status,
                saved.previous_metadata.review_note
            ],
        )
        .map_err(|error| error.to_string())?;
    match saved.previous_source.as_ref() {
        Some(source) => {
            transaction
                .execute(
                    r#"
                    INSERT INTO image_sources
                        (image_id, relative_path, external_id, annotation_path, source_version)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(image_id) DO UPDATE SET
                        relative_path = excluded.relative_path,
                        external_id = excluded.external_id,
                        annotation_path = excluded.annotation_path,
                        source_version = excluded.source_version
                    "#,
                    params![
                        source.image_id,
                        source.relative_path,
                        source.external_id,
                        source.annotation_path,
                        source.source_version,
                    ],
                )
                .map_err(|error| error.to_string())?;
        }
        None => {
            transaction
                .execute("DELETE FROM image_sources WHERE image_id = ?1", [image_id])
                .map_err(|error| error.to_string())?;
        }
    }
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at)
             VALUES (?1, 'annotation.save.rollback', ?2, 'remote annotation save compensated', ?3)",
            params![rollback_id, image_id, now_unix_millis_string()],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())?;
    validate_project_database_artifacts(path)?;
    Ok(true)
}

pub fn submit_remote_annotation(
    path: &Path,
    image_id: &str,
    operation_id: &str,
) -> Result<RemoteWorkflowState, RemoteMutationError> {
    initialize_project_database(path).map_err(RemoteMutationError::Storage)?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    let mut connection =
        open_project_database_writable(path).map_err(RemoteMutationError::Storage)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    let exists = transaction
        .query_row("SELECT 1 FROM images WHERE id = ?1", [image_id], |_| Ok(()))
        .optional()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?
        .is_some();
    if !exists {
        return Err(RemoteMutationError::NotFound);
    }
    transaction
        .execute(
            "UPDATE images
             SET status = '待质检', qa_status = '待质检', review_note = NULL
             WHERE id = ?1",
            [image_id],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "UPDATE task_items
             SET status = '待质检', qa_status = '待质检', review_note = NULL
             WHERE image_id = ?1",
            [image_id],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at)
             VALUES (?1, 'annotation.submit', ?2, 'remote annotation submitted', ?3)",
            params![operation_id, image_id, now_unix_millis_string()],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .commit()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    read_remote_workflow_state(path, image_id)
        .map_err(RemoteMutationError::Storage)?
        .ok_or(RemoteMutationError::NotFound)
}

pub fn review_remote_annotation(
    path: &Path,
    image_id: &str,
    operation_id: &str,
    decision: &str,
    note: &str,
) -> Result<RemoteWorkflowState, RemoteMutationError> {
    let (status, qa_status, message) = match decision {
        "approved" => ("通过", "通过", "remote review approved"),
        "rejected" => ("草稿", "驳回", "remote review rejected"),
        _ => {
            return Err(RemoteMutationError::Storage(
                "invalid review decision".to_string(),
            ))
        }
    };
    initialize_project_database(path).map_err(RemoteMutationError::Storage)?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    let mut connection =
        open_project_database_writable(path).map_err(RemoteMutationError::Storage)?;
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    let exists = transaction
        .query_row("SELECT 1 FROM images WHERE id = ?1", [image_id], |_| Ok(()))
        .optional()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?
        .is_some();
    if !exists {
        return Err(RemoteMutationError::NotFound);
    }
    transaction
        .execute(
            "UPDATE images SET status = ?2, qa_status = ?3, review_note = ?4 WHERE id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "UPDATE task_items
             SET status = ?2, qa_status = ?3, review_note = ?4
             WHERE image_id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO qa_reviews (id, image_id, decision, note, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                operation_id,
                image_id,
                qa_status,
                note,
                now_unix_millis_string()
            ],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at)
             VALUES (?1, 'qa.review', ?2, ?3, ?4)",
            params![operation_id, image_id, message, now_unix_millis_string()],
        )
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    transaction
        .commit()
        .map_err(|error| RemoteMutationError::Storage(error.to_string()))?;
    validate_project_database_artifacts(path).map_err(RemoteMutationError::Storage)?;
    read_remote_workflow_state(path, image_id)
        .map_err(RemoteMutationError::Storage)?
        .ok_or(RemoteMutationError::NotFound)
}

pub fn read_remote_workflow_state(
    path: &Path,
    image_id: &str,
) -> Result<Option<RemoteWorkflowState>, String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    connection
        .query_row(
            "SELECT id, status, qa_status, review_note FROM images WHERE id = ?1",
            [image_id],
            |row| {
                Ok(RemoteWorkflowState {
                    image_id: row.get(0)?,
                    status: row.get(1)?,
                    qa_status: row.get(2)?,
                    review_note: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

pub fn remote_mutation_evidence(
    path: &Path,
    operation_id: &str,
    image_id: &str,
    committed_action: &str,
    compensated_action: Option<&str>,
) -> Result<SampleMutationEvidence, String> {
    initialize_project_database(path)?;
    validate_project_database_artifacts(path)?;
    let connection = open_project_database_read_only(path)?;
    let committed = connection
        .query_row(
            "SELECT action, image_id FROM audit_events WHERE id = ?1",
            [operation_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let rollback_id = format!("{operation_id}:rollback");
    let compensated = connection
        .query_row(
            "SELECT action, image_id FROM audit_events WHERE id = ?1",
            [&rollback_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;

    let committed_matches = committed.as_ref().is_some_and(|(action, event_image_id)| {
        action == committed_action && event_image_id.as_deref() == Some(image_id)
    });
    let compensated_matches = compensated_action.is_some_and(|expected| {
        compensated
            .as_ref()
            .is_some_and(|(action, event_image_id)| {
                action == expected && event_image_id.as_deref() == Some(image_id)
            })
    });
    if committed_matches && compensated_matches {
        Ok(SampleMutationEvidence::Compensated)
    } else if committed_matches && compensated.is_none() {
        Ok(SampleMutationEvidence::Committed)
    } else if committed.is_none() && compensated.is_none() {
        Ok(SampleMutationEvidence::None)
    } else {
        Ok(SampleMutationEvidence::Indeterminate)
    }
}

pub fn submit_image_for_review(path: &Path, image_id: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let now = now_unix_string();
    let audit_event_id = unique_id("audit");
    connection
        .execute(
            "UPDATE images SET status = '待质检', qa_status = '待质检', review_note = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    connection
        .execute(
            "UPDATE task_items SET status = '待质检', qa_status = '待质检', review_note = NULL WHERE image_id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    connection
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at) VALUES (?1, 'annotation.submit', ?2, '提交质检', ?3)",
            params![audit_event_id, image_id, now],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn review_image(path: &Path, image_id: &str, decision: &str, note: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let (status, qa_status, message) = match decision {
        "approved" | "通过" => ("通过", "通过", "质检通过"),
        "rejected" | "驳回" => ("草稿", "驳回", "质检驳回"),
        other => return Err(format!("unknown review decision: {other}")),
    };
    let now = now_unix_string();
    let review_id = unique_id("review");
    let audit_event_id = unique_id("audit");
    connection
        .execute(
            "UPDATE images SET status = ?2, qa_status = ?3, review_note = ?4 WHERE id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|err| err.to_string())?;
    connection
        .execute(
            "UPDATE task_items SET status = ?2, qa_status = ?3, review_note = ?4 WHERE image_id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|err| err.to_string())?;
    connection
        .execute(
            "INSERT INTO qa_reviews (id, image_id, decision, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![review_id, image_id, qa_status, note, now],
        )
        .map_err(|err| err.to_string())?;
    connection
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at) VALUES (?1, 'qa.review', ?2, ?3, ?4)",
            params![audit_event_id, image_id, message, now],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn read_review_queue(path: &Path) -> Result<Vec<StoredImage>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, file_name, width, height, split, status, qa_status, review_note FROM images WHERE qa_status = '待质检' ORDER BY file_name")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], stored_image_from_row)
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn create_snapshot_record(
    path: &Path,
    name: &str,
    manifest_json: &str,
    image_count: u32,
) -> Result<SnapshotRecord, String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let record = SnapshotRecord {
        id: unique_id("snapshot"),
        name: name.to_string(),
        image_count,
        manifest_json: manifest_json.to_string(),
        created_at: now_unix_string(),
    };
    connection
        .execute(
            "INSERT INTO snapshots (id, name, image_count, manifest_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record.id, record.name, record.image_count, record.manifest_json, record.created_at],
        )
        .map_err(|err| err.to_string())?;
    Ok(record)
}

pub fn list_snapshot_records(path: &Path) -> Result<Vec<SnapshotRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, name, image_count, manifest_json, created_at FROM snapshots ORDER BY created_at DESC")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(SnapshotRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                image_count: row.get(2)?,
                manifest_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn create_export_record(
    path: &Path,
    snapshot_id: &str,
    format: &str,
    output_path: &str,
) -> Result<ExportRecord, String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let record = ExportRecord {
        id: unique_id("export"),
        snapshot_id: snapshot_id.to_string(),
        format: format.to_string(),
        status: "completed".to_string(),
        output_path: output_path.to_string(),
        created_at: now_unix_string(),
    };
    connection
        .execute(
            "INSERT INTO exports (id, snapshot_id, format, status, output_path, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![record.id, record.snapshot_id, record.format, record.status, record.output_path, record.created_at],
        )
        .map_err(|err| err.to_string())?;
    Ok(record)
}

pub fn list_export_records(path: &Path) -> Result<Vec<ExportRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, snapshot_id, format, status, output_path, created_at FROM exports ORDER BY created_at DESC")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(ExportRecord {
                id: row.get(0)?,
                snapshot_id: row.get(1)?,
                format: row.get(2)?,
                status: row.get(3)?,
                output_path: row.get(4)?,
                created_at: row.get(5)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn record_import(
    path: &Path,
    source_path: &str,
    status: &str,
    message: &str,
) -> Result<ImportRecord, String> {
    initialize_project_database(path)?;
    let record = ImportRecord {
        id: unique_id("import"),
        source_path: source_path.to_string(),
        status: status.to_string(),
        message: message.to_string(),
        created_at: now_unix_string(),
    };
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            "INSERT INTO imports (id, source_path, status, message, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![record.id, record.source_path, record.status, record.message, record.created_at],
        )
        .map_err(|err| err.to_string())?;
    Ok(record)
}

pub fn list_import_records(path: &Path) -> Result<Vec<ImportRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare("SELECT id, source_path, status, message, created_at FROM imports ORDER BY created_at DESC")
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(ImportRecord {
                id: row.get(0)?,
                source_path: row.get(1)?,
                status: row.get(2)?,
                message: row.get(3)?,
                created_at: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn create_annotation_task_record(
    path: &Path,
    name: &str,
    image_ids: &[&str],
) -> Result<TaskRecord, String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    let now = now_unix_string();
    let task = TaskRecord {
        id: unique_id("task"),
        name: name.to_string(),
        status: "进行中".to_string(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO tasks (id, name, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![task.id, task.name, task.status, task.created_at, task.updated_at],
        )
        .map_err(|err| err.to_string())?;
    for image_id in image_ids {
        transaction
            .execute(
                "INSERT INTO task_items (id, task_id, image_id, status, qa_status, review_note, locked_at) VALUES (?1, ?2, ?3, '草稿', '', NULL, NULL)",
                params![unique_id("task-item"), task.id, image_id],
            )
            .map_err(|err| err.to_string())?;
    }
    transaction.commit().map_err(|err| err.to_string())?;
    Ok(task)
}

pub fn list_task_records(path: &Path) -> Result<Vec<TaskRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT id, name, status, created_at, updated_at FROM tasks ORDER BY created_at DESC",
        )
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(TaskRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                status: row.get(2)?,
                created_at: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn list_task_item_records(path: &Path, task_id: &str) -> Result<Vec<TaskItemRecord>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT id, task_id, image_id, status, qa_status, review_note, locked_at FROM task_items WHERE task_id = ?1 ORDER BY image_id",
        )
        .map_err(|err| err.to_string())?;
    let rows = statement
        .query_map(params![task_id], |row| {
            Ok(TaskItemRecord {
                id: row.get(0)?,
                task_id: row.get(1)?,
                image_id: row.get(2)?,
                status: row.get(3)?,
                qa_status: row.get(4)?,
                review_note: row.get(5)?,
                locked_at: row.get(6)?,
            })
        })
        .map_err(|err| err.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    Ok(rows)
}

pub fn claim_task_item(path: &Path, task_id: &str, image_id: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            "UPDATE task_items SET status = '标注中', locked_at = ?3 WHERE task_id = ?1 AND image_id = ?2",
            params![task_id, image_id, now_unix_string()],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

pub fn release_task_item(path: &Path, task_id: &str, image_id: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
        .execute(
            "UPDATE task_items SET status = '草稿', locked_at = NULL WHERE task_id = ?1 AND image_id = ?2",
            params![task_id, image_id],
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

fn current_annotation_revision(
    connection: &Connection,
    image_id: &str,
) -> Result<Option<String>, String> {
    connection
        .query_row(
            "SELECT revision FROM annotations WHERE id = ?1",
            params![image_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|err| err.to_string())
}

fn annotation_expectation_matches(
    expectation: &AnnotationRevisionExpectation,
    current_revision: Option<&str>,
) -> bool {
    match expectation {
        AnnotationRevisionExpectation::Missing => current_revision.is_none(),
        AnnotationRevisionExpectation::AnyExisting => current_revision.is_some(),
        AnnotationRevisionExpectation::Strong(revisions) => current_revision
            .is_some_and(|current| revisions.iter().any(|revision| revision == current)),
        AnnotationRevisionExpectation::Never => false,
    }
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn now_unix_millis_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn unique_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("{prefix}-{nanos}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_project_database_schema() {
        let path = std::env::temp_dir().join("image_annotation_schema_test.sqlite");
        let _ = std::fs::remove_file(&path);

        initialize_project_database(&path).unwrap();
        let connection = Connection::open(&path).unwrap();
        let count: u32 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN ('projects', 'images', 'classes', 'annotations', 'annotation_versions', 'tasks', 'task_items', 'qa_reviews', 'snapshots', 'exports', 'imports', 'audit_events', 'label_schema_versions')",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(count, 13);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn records_dataset_import_history() {
        let path = std::env::temp_dir().join("image_annotation_import_history_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();

        let record = record_import(
            &path,
            r"L:\data_tool\datas\lg\1580_2d\train",
            "completed",
            "已链接本机目录并索引 1580 张图片",
        )
        .unwrap();
        let imports = list_import_records(&path).unwrap();

        assert_eq!(imports.len(), 1);
        assert_eq!(imports[0].id, record.id);
        assert_eq!(
            imports[0].source_path,
            r"L:\data_tool\datas\lg\1580_2d\train"
        );
        assert_eq!(imports[0].status, "completed");
        assert!(imports[0].message.contains("1580"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn saves_annotation_revisions_and_rejects_stale_revision() {
        let path = std::env::temp_dir().join("image_annotation_revision_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();

        let first = save_annotation_payload(&path, "img-1", None, r#"[{"id":"a"}]"#).unwrap();
        assert!(!first.revision.is_empty());

        let stale =
            save_annotation_payload(&path, "img-1", Some("stale-revision"), r#"[{"id":"b"}]"#);
        assert!(stale.is_err());

        let second =
            save_annotation_payload(&path, "img-1", Some(&first.revision), r#"[{"id":"b"}]"#)
                .unwrap();
        let state = read_annotation_payload(&path, "img-1").unwrap().unwrap();
        let versions = read_annotation_versions(&path, "img-1").unwrap();

        assert_eq!(state.revision, second.revision);
        assert_eq!(state.object_json, r#"[{"id":"b"}]"#);
        assert_eq!(versions.len(), 2);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn submits_and_reviews_image_status() {
        let path = std::env::temp_dir().join("image_annotation_review_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        seed_test_image(&path, "img-1");

        submit_image_for_review(&path, "img-1").unwrap();
        assert_eq!(
            read_images(&path, None).unwrap()[0].status,
            "待质检".to_string()
        );

        review_image(&path, "img-1", "approved", "可以入库").unwrap();
        let image = read_images(&path, None).unwrap().remove(0);

        assert_eq!(image.status, "通过");
        assert_eq!(image.qa_status, "通过");
        assert_eq!(image.review_note, Some("可以入库".to_string()));
        assert_eq!(read_review_queue(&path).unwrap().len(), 0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn snapshots_and_exports_are_persisted_from_database_state() {
        let path = std::env::temp_dir().join("image_annotation_snapshot_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        seed_test_image(&path, "img-1");
        save_annotation_payload(&path, "img-1", None, r#"[{"id":"a"}]"#).unwrap();
        submit_image_for_review(&path, "img-1").unwrap();

        let snapshot = create_snapshot_record(&path, "v1", r#"{"images":["img-1"]}"#, 1).unwrap();
        let export = create_export_record(&path, &snapshot.id, "yolo", "exports/v1").unwrap();

        assert_eq!(list_snapshot_records(&path).unwrap()[0].id, snapshot.id);
        assert_eq!(list_export_records(&path).unwrap()[0].id, export.id);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn creates_claims_and_releases_annotation_task_items() {
        let path = std::env::temp_dir().join("image_annotation_task_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        seed_test_image(&path, "img-1");
        seed_test_image(&path, "img-2");

        let task = create_annotation_task_record(&path, "第一轮标注", &["img-1", "img-2"]).unwrap();
        let items = list_task_item_records(&path, &task.id).unwrap();
        assert_eq!(items.len(), 2);

        claim_task_item(&path, &task.id, "img-1").unwrap();
        let claimed = list_task_item_records(&path, &task.id)
            .unwrap()
            .into_iter()
            .find(|item| item.image_id == "img-1")
            .unwrap();
        assert_eq!(claimed.status, "标注中");
        assert!(claimed.locked_at.is_some());

        release_task_item(&path, &task.id, "img-1").unwrap();
        let released = list_task_item_records(&path, &task.id)
            .unwrap()
            .into_iter()
            .find(|item| item.image_id == "img-1")
            .unwrap();
        assert_eq!(released.status, "草稿");
        assert_eq!(released.locked_at, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_annotation_compensation_preserves_concurrent_metadata_changes() {
        let path =
            std::env::temp_dir().join("image_annotation_remote_compensation_metadata_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        seed_test_image(&path, "img-1");
        let source = StoredImageSource {
            image_id: "img-1".to_string(),
            relative_path: "images/img-1.jpg".to_string(),
            external_id: None,
            annotation_path: Some("labels/img-1.txt".to_string()),
            source_version: "sha256:applied".to_string(),
        };
        let saved = save_remote_annotation_payload(
            &path,
            "img-1",
            &AnnotationRevisionExpectation::Missing,
            r#"[{"id":"applied"}]"#,
            "metadata-cas-operation",
            &source,
        )
        .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE images
                 SET split = 'val', status = '通过', qa_status = '通过',
                     review_note = 'concurrent metadata'
                 WHERE id = 'img-1'",
                [],
            )
            .unwrap();

        let compensated =
            compensate_remote_annotation_save(&path, "img-1", "metadata-cas-operation", &saved)
                .unwrap();

        assert!(!compensated);
        assert_eq!(
            read_sample_metadata(&path, "img-1").unwrap().unwrap(),
            StoredSampleMetadata {
                split: "val".to_string(),
                status: "通过".to_string(),
                qa_status: "通过".to_string(),
                review_note: Some("concurrent metadata".to_string()),
            }
        );
        assert_eq!(
            read_annotation_payload(&path, "img-1")
                .unwrap()
                .unwrap()
                .revision,
            saved.revision
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn remote_annotation_compensation_preserves_concurrent_source_changes() {
        let path =
            std::env::temp_dir().join("image_annotation_remote_compensation_source_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        seed_test_image(&path, "img-1");
        let source = StoredImageSource {
            image_id: "img-1".to_string(),
            relative_path: "images/img-1.jpg".to_string(),
            external_id: None,
            annotation_path: Some("labels/img-1.txt".to_string()),
            source_version: "sha256:applied".to_string(),
        };
        let saved = save_remote_annotation_payload(
            &path,
            "img-1",
            &AnnotationRevisionExpectation::Missing,
            r#"[{"id":"applied"}]"#,
            "source-cas-operation",
            &source,
        )
        .unwrap();
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE image_sources
                 SET annotation_path = 'labels/external.txt',
                     source_version = 'sha256:external'
                 WHERE image_id = 'img-1'",
                [],
            )
            .unwrap();

        let compensated =
            compensate_remote_annotation_save(&path, "img-1", "source-cas-operation", &saved)
                .unwrap();

        assert!(!compensated);
        let current = read_image_source(&path, "img-1").unwrap().unwrap();
        assert_eq!(
            current.annotation_path.as_deref(),
            Some("labels/external.txt")
        );
        assert_eq!(current.source_version, "sha256:external");
        assert_eq!(
            read_annotation_payload(&path, "img-1")
                .unwrap()
                .unwrap()
                .revision,
            saved.revision
        );
        let _ = std::fs::remove_file(path);
    }

    fn seed_test_image(path: &Path, image_id: &str) {
        let connection = Connection::open(path).unwrap();
        connection
            .execute(
                "INSERT INTO images (id, file_name, width, height, split, status) VALUES (?1, ?2, 640, 480, 'train', '草稿')",
                params![image_id, format!("{image_id}.jpg")],
            )
            .unwrap();
    }

    #[test]
    fn writes_and_reads_project_image_and_class_index() {
        let path = std::env::temp_dir().join("image_annotation_index_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        let manifest = ProjectManifest {
            id: "fixture".to_string(),
            name: "Fixture".to_string(),
            source_dataset_key: "fixture".to_string(),
            format: "yolo-detect".to_string(),
            root_path: "F:/fixture".to_string(),
            created_at: "1".to_string(),
            class_count: 2,
            image_count: 1,
        };
        let images = vec![StoredImage {
            id: "0001".to_string(),
            file_name: "0001.png".to_string(),
            width: 4,
            height: 3,
            split: "train".to_string(),
            status: "已标注".to_string(),
            qa_status: String::new(),
            review_note: None,
        }];
        let classes = vec![
            StoredClass {
                id: 0,
                label: "person".to_string(),
                color: "#1fa7ff".to_string(),
            },
            StoredClass {
                id: 1,
                label: "car".to_string(),
                color: "#cc54d8".to_string(),
            },
        ];

        upsert_project_index(&path, &manifest, &images, &classes).unwrap();

        assert_eq!(read_project_manifest(&path).unwrap().unwrap().id, "fixture");
        assert_eq!(read_images(&path, None).unwrap(), images);
        assert_eq!(read_classes(&path).unwrap(), classes);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn reads_images_with_limit_and_offset_for_large_local_projects() {
        let path = std::env::temp_dir().join("image_annotation_paged_images_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        let manifest = ProjectManifest {
            id: "large-local".to_string(),
            name: "Large Local".to_string(),
            source_dataset_key: "local-linked".to_string(),
            format: "voc-detect".to_string(),
            root_path: "L:/large".to_string(),
            created_at: "1".to_string(),
            class_count: 1,
            image_count: 5,
        };
        let images: Vec<_> = (1..=5)
            .map(|index| StoredImage {
                id: format!("img-{index}"),
                file_name: format!("img-{index}.jpg"),
                width: 4,
                height: 3,
                split: "train".to_string(),
                status: "草稿".to_string(),
                qa_status: String::new(),
                review_note: None,
            })
            .collect();

        upsert_project_index(&path, &manifest, &images, &[]).unwrap();

        let page = read_images_page(&path, None, 2, 2).unwrap();

        assert_eq!(
            page.iter()
                .map(|image| image.id.as_str())
                .collect::<Vec<_>>(),
            vec!["img-3", "img-4"]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn source_mapping_records_round_trip() {
        let path = std::env::temp_dir().join("image_annotation_source_mapping_test.sqlite");
        let _ = std::fs::remove_file(&path);
        initialize_project_database(&path).unwrap();
        let source = StoredDatasetSource {
            format: "coco".to_string(),
            mode: "linked".to_string(),
            root_path: "L:/dataset".to_string(),
            annotation_path: Some("L:/dataset/annotations.json".to_string()),
            options_json: "{}".to_string(),
        };
        let mapping = StoredImageSource {
            image_id: "img-1".to_string(),
            relative_path: "images/a.jpg".to_string(),
            external_id: Some("42".to_string()),
            annotation_path: Some("annotations.json".to_string()),
            source_version: "100:1234".to_string(),
        };

        write_dataset_source(&path, &source).unwrap();
        replace_image_sources(&path, std::slice::from_ref(&mapping)).unwrap();

        assert_eq!(read_dataset_source(&path).unwrap(), Some(source));
        assert_eq!(read_image_source(&path, "img-1").unwrap(), Some(mapping));
        let _ = std::fs::remove_file(path);
    }
}

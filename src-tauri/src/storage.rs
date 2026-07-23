use crate::{hybrid, project_fs::ProjectManifest};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
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

#[derive(Debug, Clone, PartialEq)]
pub struct StoredClass {
    pub id: u32,
    pub label: String,
    pub color: String,
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
    hybrid::prepare_database(path)
        .map_err(|err| format!("prepare database {}: {err}", path.display()))?;
    let mut connection = Connection::open(path)
        .map_err(|err| format!("open database {}: {err}", path.display()))?;
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
            CREATE TABLE IF NOT EXISTS annotations (
                id TEXT PRIMARY KEY,
                image_id TEXT NOT NULL,
                revision TEXT NOT NULL DEFAULT '',
                object_json TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
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
        .map_err(|err| format!("create base schema {}: {err}", path.display()))?;
    hybrid::migrate_database(&mut connection)
        .map_err(|err| format!("migrate database {}: {err}", path.display()))
}

pub fn upsert_project_index(
    path: &Path,
    manifest: &ProjectManifest,
    images: &[StoredImage],
    classes: &[StoredClass],
) -> Result<(), String> {
    let mut connection = Connection::open(path)
        .map_err(|err| format!("open project index {}: {err}", path.display()))?;
    let transaction = connection
        .transaction()
        .map_err(|err| format!("begin project index transaction {}: {err}", path.display()))?;
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
        .map_err(|err| format!("upsert project index {}: {err}", path.display()))?;
    transaction
        .execute("DELETE FROM images", [])
        .map_err(|err| format!("clear image index {}: {err}", path.display()))?;
    transaction
        .execute("DELETE FROM classes", [])
        .map_err(|err| format!("clear class index {}: {err}", path.display()))?;

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
            .map_err(|err| format!("write image index {}: {err}", path.display()))?;
    }

    for class in classes {
        transaction
            .execute(
                "INSERT INTO classes (id, label, color) VALUES (?1, ?2, ?3)",
                params![class.id, class.label, class.color],
            )
            .map_err(|err| format!("write class index {}: {err}", path.display()))?;
    }

    transaction
        .commit()
        .map_err(|err| format!("commit project index {}: {err}", path.display()))
}

pub fn read_project_manifest(path: &Path) -> Result<Option<ProjectManifest>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    connection
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
        .map_err(|err| err.to_string())
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
    hybrid::mark_annotation_pending(
        &transaction,
        path,
        image_id,
        None,
        serde_json::json!({
            "imageId": image_id,
            "revision": revision,
            "objects": serde_json::from_str::<serde_json::Value>(object_json)
                .unwrap_or(serde_json::Value::Null)
        }),
    )?;
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
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
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
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    let mut statement = connection
        .prepare(
            "SELECT id, image_id, revision, object_json, created_at FROM annotation_versions WHERE image_id = ?1 ORDER BY created_at",
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

pub fn submit_image_for_review(path: &Path, image_id: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    let now = now_unix_string();
    let audit_event_id = unique_id("audit");
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE images SET status = '待质检', qa_status = '待质检', review_note = NULL WHERE id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE task_items SET status = '待质检', qa_status = '待质检', review_note = NULL WHERE image_id = ?1",
            params![image_id],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at) VALUES (?1, 'annotation.submit', ?2, '提交质检', ?3)",
            params![audit_event_id, image_id, now],
        )
        .map_err(|err| err.to_string())?;
    hybrid::mark_submission_pending(&transaction, path, image_id)?;
    transaction.commit().map_err(|err| err.to_string())
}

pub fn review_image(path: &Path, image_id: &str, decision: &str, note: &str) -> Result<(), String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    let (status, qa_status, message) = match decision {
        "approved" | "通过" => ("通过", "通过", "质检通过"),
        "rejected" | "驳回" => ("草稿", "驳回", "质检驳回"),
        other => return Err(format!("unknown review decision: {other}")),
    };
    let now = now_unix_string();
    let review_id = unique_id("review");
    let audit_event_id = unique_id("audit");
    let transaction = connection.transaction().map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE images SET status = ?2, qa_status = ?3, review_note = ?4 WHERE id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "UPDATE task_items SET status = ?2, qa_status = ?3, review_note = ?4 WHERE image_id = ?1",
            params![image_id, status, qa_status, note],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO qa_reviews (id, image_id, decision, note, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![review_id, image_id, qa_status, note, now],
        )
        .map_err(|err| err.to_string())?;
    transaction
        .execute(
            "INSERT INTO audit_events (id, action, image_id, message, created_at) VALUES (?1, 'qa.review', ?2, ?3, ?4)",
            params![audit_event_id, image_id, message, now],
        )
        .map_err(|err| err.to_string())?;
    hybrid::record_review_operation(&transaction, path, image_id, decision, note)?;
    transaction.commit().map_err(|err| err.to_string())
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

pub fn list_issue_records(
    path: &Path,
    project_id: &str,
    include_closed: bool,
) -> Result<Vec<hybrid::IssueRecord>, String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::list_issues(&connection, project_id, include_closed)
}

pub fn create_issue_record(
    path: &Path,
    project_id: &str,
    image_id: &str,
    annotation_object_id: Option<&str>,
    title: &str,
    description: &str,
    severity: &str,
    assignee_id: Option<&str>,
) -> Result<hybrid::IssueRecord, String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::create_issue(
        &mut connection,
        project_id,
        image_id,
        annotation_object_id,
        title,
        description,
        severity,
        assignee_id,
    )
}

pub fn transition_issue_record(
    path: &Path,
    project_id: &str,
    issue_id: &str,
    next_status: &str,
) -> Result<hybrid::IssueRecord, String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::transition_issue(&mut connection, project_id, issue_id, next_status)
}

pub fn add_issue_comment_record(
    path: &Path,
    project_id: &str,
    issue_id: &str,
    content: &str,
) -> Result<hybrid::IssueCommentRecord, String> {
    initialize_project_database(path)?;
    let mut connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::add_issue_comment(&mut connection, project_id, issue_id, content)
}

pub fn list_issue_comment_records(
    path: &Path,
    issue_id: &str,
) -> Result<Vec<hybrid::IssueCommentRecord>, String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::list_issue_comments(&connection, issue_id)
}

pub fn read_sync_summary(
    path: &Path,
    project_id: &str,
) -> Result<hybrid::SyncSummary, String> {
    initialize_project_database(path)?;
    let connection = Connection::open(path).map_err(|err| err.to_string())?;
    hybrid::sync_summary(&connection, project_id)
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

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
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
}

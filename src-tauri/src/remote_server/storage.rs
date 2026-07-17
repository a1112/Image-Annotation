use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};

static SERVER_DATABASE_INITIALIZATION: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone)]
pub(super) struct ServerStorage {
    path: PathBuf,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct AuditEntry<'a> {
    pub request_id: &'a str,
    pub role: &'a str,
    pub action: &'a str,
    pub project_id: Option<&'a str>,
    pub image_id: Option<&'a str>,
    pub message: &'a str,
}

impl ServerStorage {
    pub(super) fn initialize(data_dir: &Path) -> Result<Self, String> {
        let storage = Self {
            path: data_dir.join("server.sqlite"),
        };
        let _initialization_guard = SERVER_DATABASE_INITIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let connection = storage.connection()?;
        if journal_mode(&connection)? != "wal" {
            connection
                .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
                .map_err(|error| error.to_string())?;
        }
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| error.to_string())?;
        if !schema_is_current(&connection)? {
            connection
                .execute_batch(
                    r#"
                    CREATE TABLE IF NOT EXISTS service_audit (
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        request_id TEXT NOT NULL,
                        role TEXT NOT NULL,
                        action TEXT NOT NULL,
                        project_id TEXT,
                        image_id TEXT,
                        message TEXT NOT NULL DEFAULT '',
                        created_at TEXT NOT NULL
                    );
                    CREATE INDEX IF NOT EXISTS idx_service_audit_request_id
                        ON service_audit(request_id);
                    CREATE INDEX IF NOT EXISTS idx_service_audit_project_id
                        ON service_audit(project_id);

                    CREATE TABLE IF NOT EXISTS trashed_projects (
                        project_id TEXT PRIMARY KEY,
                        trashed_at TEXT NOT NULL
                    );

                    CREATE TABLE IF NOT EXISTS import_sessions (
                        id TEXT PRIMARY KEY,
                        project_id TEXT,
                        status TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );

                    CREATE TABLE IF NOT EXISTS project_metadata (
                        project_id TEXT PRIMARY KEY,
                        description TEXT NOT NULL DEFAULT ''
                    );
                    "#,
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(storage)
    }

    pub(super) fn is_trashed(&self, project_id: &str) -> Result<bool, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT 1 FROM trashed_projects WHERE project_id = ?1",
                [project_id],
                |_| Ok(()),
            )
            .optional()
            .map(|row| row.is_some())
            .map_err(|error| error.to_string())
    }

    pub(super) fn description(&self, project_id: &str) -> Result<Option<String>, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT description FROM project_metadata WHERE project_id = ?1",
                [project_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub(super) fn record_audit(&self, entry: AuditEntry<'_>) -> Result<(), String> {
        let connection = self.connection()?;
        insert_audit(&connection, entry)
    }

    pub(super) fn update_metadata_and_audit(
        &self,
        project_id: &str,
        description: Option<&str>,
        entry: AuditEntry<'_>,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        if let Some(description) = description {
            transaction
                .execute(
                    r#"
                    INSERT INTO project_metadata (project_id, description)
                    VALUES (?1, ?2)
                    ON CONFLICT(project_id) DO UPDATE SET description = excluded.description
                    "#,
                    params![project_id, description],
                )
                .map_err(|error| error.to_string())?;
        }
        insert_audit(&transaction, entry)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn record_trash(
        &self,
        project_id: &str,
        entry: AuditEntry<'_>,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "INSERT INTO trashed_projects (project_id, trashed_at) VALUES (?1, ?2)",
                params![project_id, now_unix_string()],
            )
            .map_err(|error| error.to_string())?;
        insert_audit(&transaction, entry)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn clear_trash(
        &self,
        project_id: &str,
        entry: AuditEntry<'_>,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let removed = transaction
            .execute(
                "DELETE FROM trashed_projects WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        if removed != 1 {
            return Err("trash tombstone was not found".to_string());
        }
        insert_audit(&transaction, entry)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    fn connection(&self) -> Result<Connection, String> {
        let connection = Connection::open(&self.path).map_err(|error| error.to_string())?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| error.to_string())?;
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|error| error.to_string())?;
        Ok(connection)
    }
}

fn journal_mode(connection: &Connection) -> Result<String, String> {
    connection
        .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        .map(|mode| mode.to_ascii_lowercase())
        .map_err(|error| error.to_string())
}

fn schema_is_current(connection: &Connection) -> Result<bool, String> {
    connection
        .query_row(
            r#"
            SELECT COUNT(*)
            FROM sqlite_schema
            WHERE
                (type = 'table' AND name IN (
                    'service_audit',
                    'trashed_projects',
                    'import_sessions',
                    'project_metadata'
                ))
                OR
                (type = 'index' AND name IN (
                    'idx_service_audit_request_id',
                    'idx_service_audit_project_id'
                ))
            "#,
            [],
            |row| row.get::<_, usize>(0),
        )
        .map(|object_count| object_count == 6)
        .map_err(|error| error.to_string())
}

fn insert_audit(connection: &Connection, entry: AuditEntry<'_>) -> Result<(), String> {
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                request_id, role, action, project_id, image_id, message, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            "#,
            params![
                entry.request_id,
                entry.role,
                entry.action,
                entry.project_id,
                entry.image_id,
                entry.message,
                now_unix_string(),
            ],
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

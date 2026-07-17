use std::{
    path::{Path, PathBuf},
    sync::Mutex,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

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

#[derive(Debug, Clone, Copy)]
pub(super) struct AuditOperation {
    id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TrashState {
    Trashing,
    Trashed,
    Restoring,
}

impl TrashState {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "trashing" => Ok(Self::Trashing),
            "trashed" => Ok(Self::Trashed),
            "restoring" => Ok(Self::Restoring),
            _ => Err("invalid project trash state".to_string()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TrashRecord {
    pub project_id: String,
    pub state: TrashState,
}

impl ServerStorage {
    pub(super) fn initialize(data_dir: &Path) -> Result<Self, String> {
        let storage = Self {
            path: data_dir.join("server.sqlite"),
        };
        let _initialization_guard = SERVER_DATABASE_INITIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut connection = storage.connection()?;
        if journal_mode(&connection)? != "wal" {
            let configured_mode = connection
                .query_row("PRAGMA journal_mode = WAL", [], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| error.to_string())?;
            if !configured_mode.eq_ignore_ascii_case("wal") {
                return Err("server database does not support WAL mode".to_string());
            }
        }
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| error.to_string())?;
        if !schema_is_current(&connection)? {
            initialize_schema(&mut connection)?;
        }
        Ok(storage)
    }

    pub(super) fn trash_state(&self, project_id: &str) -> Result<Option<TrashState>, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT state FROM trashed_projects WHERE project_id = ?1",
                [project_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(|state| TrashState::parse(&state))
            .transpose()
    }

    pub(super) fn trash_records(&self) -> Result<Vec<TrashRecord>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT project_id, state FROM trashed_projects ORDER BY project_id")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            let (project_id, state) = row.map_err(|error| error.to_string())?;
            Ok(TrashRecord {
                project_id,
                state: TrashState::parse(&state)?,
            })
        })
        .collect()
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

    pub(super) fn begin_audit(&self, entry: AuditEntry<'_>) -> Result<AuditOperation, String> {
        let connection = self.connection()?;
        insert_audit(&connection, entry, "intent").map(|id| AuditOperation { id })
    }

    pub(super) fn complete_audit(
        &self,
        operation: AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        update_audit(&connection, operation, "completed", message)
    }

    pub(super) fn fail_audit(
        &self,
        operation: AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        update_audit(&connection, operation, "failed", message)
    }

    pub(super) fn complete_metadata_and_audit(
        &self,
        project_id: &str,
        description: Option<&str>,
        operation: AuditOperation,
        message: &str,
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
        update_audit(&transaction, operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn begin_trash(
        &self,
        project_id: &str,
        entry: AuditEntry<'_>,
    ) -> Result<AuditOperation, String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let operation = AuditOperation {
            id: insert_audit(&transaction, entry, "intent")?,
        };
        transaction
            .execute(
                r#"
                INSERT INTO trashed_projects (project_id, trashed_at, state)
                VALUES (?1, ?2, 'trashing')
                "#,
                params![project_id, now_unix_string()],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(operation)
    }

    pub(super) fn complete_trash(
        &self,
        project_id: &str,
        operation: AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let updated = transaction
            .execute(
                "UPDATE trashed_projects SET state = 'trashed' WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("trash tombstone was not found".to_string());
        }
        update_audit(&transaction, operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn fail_trash(
        &self,
        project_id: &str,
        operation: AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM trashed_projects WHERE project_id = ?1 AND state = 'trashing'",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        update_audit(&transaction, operation, "failed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn begin_restore(
        &self,
        project_id: &str,
        entry: AuditEntry<'_>,
    ) -> Result<AuditOperation, String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let operation = AuditOperation {
            id: insert_audit(&transaction, entry, "intent")?,
        };
        let updated = transaction
            .execute(
                "UPDATE trashed_projects SET state = 'restoring' WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("trash tombstone was not found".to_string());
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(operation)
    }

    pub(super) fn complete_restore(
        &self,
        project_id: &str,
        operation: AuditOperation,
        message: &str,
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
        update_audit(&transaction, operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn fail_restore(
        &self,
        project_id: &str,
        operation: AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "UPDATE trashed_projects SET state = 'trashed' WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        update_audit(&transaction, operation, "failed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn reconcile_trashed(&self, project_id: &str) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "UPDATE trashed_projects SET state = 'trashed' WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        complete_latest_intent(
            &transaction,
            project_id,
            "delete_project",
            "project trash reconciled",
        )?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn reconcile_restored(&self, project_id: &str) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM trashed_projects WHERE project_id = ?1",
                [project_id],
            )
            .map_err(|error| error.to_string())?;
        complete_latest_intent(
            &transaction,
            project_id,
            "restore_project",
            "project restore reconciled",
        )?;
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
    let object_count = connection
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
        .map_err(|error| error.to_string())?;
    Ok(object_count == 6
        && column_exists(connection, "service_audit", "status")?
        && column_exists(connection, "trashed_projects", "state")?)
}

fn initialize_schema(connection: &mut Connection) -> Result<(), String> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    transaction
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
                status TEXT NOT NULL DEFAULT 'completed',
                created_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_service_audit_request_id
                ON service_audit(request_id);
            CREATE INDEX IF NOT EXISTS idx_service_audit_project_id
                ON service_audit(project_id);

            CREATE TABLE IF NOT EXISTS trashed_projects (
                project_id TEXT PRIMARY KEY,
                trashed_at TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'trashed'
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
    if !column_exists(&transaction, "service_audit", "status")? {
        transaction
            .execute(
                "ALTER TABLE service_audit ADD COLUMN status TEXT NOT NULL DEFAULT 'completed'",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    if !column_exists(&transaction, "trashed_projects", "state")? {
        transaction
            .execute(
                "ALTER TABLE trashed_projects ADD COLUMN state TEXT NOT NULL DEFAULT 'trashed'",
                [],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())
}

fn column_exists(connection: &Connection, table: &str, column: &str) -> Result<bool, String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|error| error.to_string())?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| error.to_string())?;
    for name in columns {
        if name.map_err(|error| error.to_string())? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

fn insert_audit(
    connection: &Connection,
    entry: AuditEntry<'_>,
    status: &str,
) -> Result<i64, String> {
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                request_id, role, action, project_id, image_id, message, status, created_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
            "#,
            params![
                entry.request_id,
                entry.role,
                entry.action,
                entry.project_id,
                entry.image_id,
                entry.message,
                status,
                now_unix_string(),
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(connection.last_insert_rowid())
}

fn update_audit(
    connection: &Connection,
    operation: AuditOperation,
    status: &str,
    message: &str,
) -> Result<(), String> {
    let updated = connection
        .execute(
            "UPDATE service_audit SET status = ?1, message = ?2 WHERE id = ?3",
            params![status, message, operation.id],
        )
        .map_err(|error| error.to_string())?;
    if updated == 1 {
        Ok(())
    } else {
        Err("audit operation was not found".to_string())
    }
}

fn complete_latest_intent(
    transaction: &Transaction<'_>,
    project_id: &str,
    action: &str,
    message: &str,
) -> Result<(), String> {
    transaction
        .execute(
            r#"
            UPDATE service_audit
            SET status = 'completed', message = ?1
            WHERE id = (
                SELECT id FROM service_audit
                WHERE project_id = ?2 AND action = ?3 AND status = 'intent'
                ORDER BY id DESC
                LIMIT 1
            )
            "#,
            params![message, project_id, action],
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

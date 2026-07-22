use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

static SERVER_DATABASE_INITIALIZATION: Mutex<()> = Mutex::new(());
static OPERATION_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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
    pub payload: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AuditOperation {
    pub operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RestoreOperation {
    pub operation: AuditOperation,
    previous_operation_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OperationRecord {
    pub operation_id: String,
    pub action: String,
    pub project_id: Option<String>,
    pub image_id: Option<String>,
    pub state: String,
    pub payload: String,
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
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImportRecord {
    pub id: String,
    pub project_id: String,
    pub state: String,
    pub staging_path: String,
    pub detected_format: Option<String>,
    pub analysis_json: Option<String>,
    pub bytes_received: u64,
    pub file_count: u32,
    pub error_message: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl ServerStorage {
    pub(super) fn initialize(data_dir: &Path) -> Result<Self, String> {
        let storage = Self {
            path: data_dir.join("server.sqlite"),
        };
        let _initialization_guard = SERVER_DATABASE_INITIALIZATION
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        validate_optional_database_file(data_dir, &storage.path)
            .map_err(|error| format!("main database path validation failed: {error}"))?;
        for path in [
            data_dir.join("server.sqlite-wal"),
            data_dir.join("server.sqlite-shm"),
        ] {
            validate_optional_database_sidecar(data_dir, &path)
                .map_err(|error| format!("database sidecar validation failed: {error}"))?;
        }
        let mut connection = storage
            .connection()
            .map_err(|error| format!("database connection failed: {error}"))?;
        validate_optional_database_file(data_dir, &storage.path)
            .map_err(|error| format!("opened database path validation failed: {error}"))?;
        if journal_mode(&connection)
            .map_err(|error| format!("journal mode read failed: {error}"))?
            != "wal"
        {
            let configured_mode = connection
                .query_row("PRAGMA journal_mode = WAL", [], |row| {
                    row.get::<_, String>(0)
                })
                .map_err(|error| format!("journal mode configuration failed: {error}"))?;
            if !configured_mode.eq_ignore_ascii_case("wal") {
                return Err("server database does not support WAL mode".to_string());
            }
        }
        connection
            .pragma_update(None, "synchronous", "NORMAL")
            .map_err(|error| format!("synchronous mode configuration failed: {error}"))?;
        if !schema_is_current(&connection)
            .map_err(|error| format!("schema inspection failed: {error}"))?
        {
            initialize_schema(&mut connection)
                .map_err(|error| format!("schema initialization failed: {error}"))?;
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
            .prepare(
                "SELECT project_id, state, operation_id
                 FROM trashed_projects ORDER BY project_id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| {
            let (project_id, state, operation_id) = row.map_err(|error| error.to_string())?;
            Ok(TrashRecord {
                project_id,
                state: TrashState::parse(&state)?,
                operation_id,
            })
        })
        .collect()
    }

    pub(super) fn operation(&self, operation_id: &str) -> Result<Option<OperationRecord>, String> {
        let connection = self.connection()?;
        read_operation(&connection, operation_id)
    }

    pub(super) fn pending_operations(&self) -> Result<Vec<OperationRecord>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT operation_id, action, project_id, image_id, state, payload
                 FROM service_audit
                 WHERE state = 'pending'
                 ORDER BY id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], operation_from_row)
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string()))
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

    pub(super) fn create_import(&self, record: &ImportRecord) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO import_sessions (
                    id, project_id, status, staging_path, detected_format,
                    analysis_json, bytes_received, file_count, error_message,
                    created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    record.id,
                    record.project_id,
                    record.state,
                    record.staging_path,
                    record.detected_format,
                    record.analysis_json,
                    record.bytes_received,
                    record.file_count,
                    record.error_message,
                    record.created_at,
                    record.updated_at,
                ],
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    pub(super) fn import(&self, import_id: &str) -> Result<Option<ImportRecord>, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, project_id, status, staging_path, detected_format,
                        analysis_json, bytes_received, file_count, error_message,
                        created_at, updated_at
                 FROM import_sessions WHERE id = ?1",
                [import_id],
                import_record_from_row,
            )
            .optional()
            .map_err(|error| error.to_string())
    }

    pub(super) fn imports(&self) -> Result<Vec<ImportRecord>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, project_id, status, staging_path, detected_format,
                        analysis_json, bytes_received, file_count, error_message,
                        created_at, updated_at
                 FROM import_sessions ORDER BY created_at, id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], import_record_from_row)
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string()))
            .collect()
    }

    pub(super) fn finish_import_analysis(
        &self,
        import_id: &str,
        detected_format: &str,
        analysis_json: &str,
        bytes_received: u64,
        file_count: u32,
    ) -> Result<(), String> {
        self.transition_import(
            import_id,
            "staged",
            "analyzed",
            Some(detected_format),
            Some(analysis_json),
            Some(bytes_received),
            Some(file_count),
            None,
        )
    }

    pub(super) fn begin_import_commit(&self, import_id: &str) -> Result<(), String> {
        self.transition_import(
            import_id,
            "analyzed",
            "committing",
            None,
            None,
            None,
            None,
            None,
        )
    }

    pub(super) fn complete_import(&self, import_id: &str) -> Result<(), String> {
        self.transition_import(
            import_id,
            "committing",
            "completed",
            None,
            None,
            None,
            None,
            None,
        )
    }

    pub(super) fn fail_import(&self, import_id: &str, message: &str) -> Result<(), String> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE import_sessions
                 SET status = 'failed', error_message = ?1, updated_at = ?2
                 WHERE id = ?3 AND status NOT IN ('completed', 'cancelled')",
                params![message, now_unix_string(), import_id],
            )
            .map_err(|error| error.to_string())?;
        if updated == 1 {
            Ok(())
        } else {
            Err("import session cannot transition to failed".to_string())
        }
    }

    pub(super) fn cancel_import(&self, import_id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE import_sessions
                 SET status = 'cancelled', updated_at = ?1
                 WHERE id = ?2 AND status IN ('staged', 'analyzed', 'failed')",
                params![now_unix_string(), import_id],
            )
            .map_err(|error| error.to_string())?;
        if updated == 1 {
            Ok(())
        } else {
            Err("import session cannot be cancelled".to_string())
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transition_import(
        &self,
        import_id: &str,
        expected: &str,
        next: &str,
        detected_format: Option<&str>,
        analysis_json: Option<&str>,
        bytes_received: Option<u64>,
        file_count: Option<u32>,
        error_message: Option<&str>,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE import_sessions SET
                    status = ?1,
                    detected_format = COALESCE(?2, detected_format),
                    analysis_json = COALESCE(?3, analysis_json),
                    bytes_received = COALESCE(?4, bytes_received),
                    file_count = COALESCE(?5, file_count),
                    error_message = ?6,
                    updated_at = ?7
                 WHERE id = ?8 AND status = ?9",
                params![
                    next,
                    detected_format,
                    analysis_json,
                    bytes_received,
                    file_count,
                    error_message,
                    now_unix_string(),
                    import_id,
                    expected,
                ],
            )
            .map_err(|error| error.to_string())?;
        if updated == 1 {
            Ok(())
        } else {
            Err("import session state did not match".to_string())
        }
    }

    pub(super) fn begin_audit(&self, entry: AuditEntry<'_>) -> Result<AuditOperation, String> {
        let connection = self.connection()?;
        insert_operation(&connection, entry)
    }

    pub(super) fn complete_audit(
        &self,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        update_operation(&connection, operation, "completed", message)
    }

    pub(super) fn fail_audit(
        &self,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        update_operation(&connection, operation, "failed", message)
    }

    pub(super) fn mark_audit_indeterminate(
        &self,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        update_operation(&connection, operation, "indeterminate", message)
    }

    pub(super) fn note_pending_audit(
        &self,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE service_audit
                 SET message = ?1, updated_at = ?2
                 WHERE operation_id = ?3 AND state = 'pending'",
                params![message, now_unix_string(), operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        if updated == 1 {
            Ok(())
        } else {
            Err("audit operation state did not match pending".to_string())
        }
    }

    pub(super) fn mark_orphan_audit_indeterminate(
        &self,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let connection = self.connection()?;
        let updated = connection
            .execute(
                "UPDATE service_audit
                 SET status = 'indeterminate', state = 'indeterminate',
                     message = ?1, updated_at = ?2
                 WHERE operation_id = ?3
                   AND state IN ('completed', 'indeterminate')",
                params![message, now_unix_string(), operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        if updated == 1 {
            Ok(())
        } else {
            Err("orphan audit operation was not completed or indeterminate".to_string())
        }
    }

    pub(super) fn complete_metadata_and_audit(
        &self,
        project_id: &str,
        description: Option<&str>,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        set_description(&transaction, project_id, description)?;
        update_operation(&transaction, operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn fail_update_and_restore_metadata(
        &self,
        project_id: &str,
        old_description: Option<&str>,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        set_description(&transaction, project_id, old_description)?;
        update_operation(&transaction, operation, "failed", message)?;
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
        let operation = insert_operation(&transaction, entry)?;
        transaction
            .execute(
                r#"
                INSERT INTO trashed_projects (
                    project_id, trashed_at, state, operation_id
                )
                VALUES (?1, ?2, 'trashing', ?3)
                "#,
                params![project_id, now_unix_string(), operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(operation)
    }

    pub(super) fn complete_trash(
        &self,
        project_id: &str,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let updated = transaction
            .execute(
                "UPDATE trashed_projects SET state = 'trashed'
                 WHERE project_id = ?1 AND operation_id = ?2 AND state = 'trashing'",
                params![project_id, operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("trash tombstone operation did not match".to_string());
        }
        update_operation(&transaction, operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn fail_trash(
        &self,
        project_id: &str,
        operation: &AuditOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        transaction
            .execute(
                "DELETE FROM trashed_projects
                 WHERE project_id = ?1 AND operation_id = ?2 AND state = 'trashing'",
                params![project_id, operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        update_operation(&transaction, operation, "failed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn begin_restore(
        &self,
        project_id: &str,
        entry: AuditEntry<'_>,
    ) -> Result<RestoreOperation, String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let previous_operation_id = transaction
            .query_row(
                "SELECT operation_id FROM trashed_projects
                 WHERE project_id = ?1 AND state = 'trashed'",
                [project_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .flatten()
            .ok_or_else(|| "completed trash operation was not found".to_string())?;
        let operation = insert_operation(&transaction, entry)?;
        let updated = transaction
            .execute(
                "UPDATE trashed_projects
                 SET state = 'restoring', operation_id = ?2
                 WHERE project_id = ?1 AND state = 'trashed'",
                params![project_id, operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("trash tombstone was not found".to_string());
        }
        transaction.commit().map_err(|error| error.to_string())?;
        Ok(RestoreOperation {
            operation,
            previous_operation_id,
        })
    }

    pub(super) fn complete_restore(
        &self,
        project_id: &str,
        restore: &RestoreOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let removed = transaction
            .execute(
                "DELETE FROM trashed_projects
                 WHERE project_id = ?1 AND operation_id = ?2 AND state = 'restoring'",
                params![project_id, restore.operation.operation_id],
            )
            .map_err(|error| error.to_string())?;
        if removed != 1 {
            return Err("trash tombstone operation did not match".to_string());
        }
        update_operation(&transaction, &restore.operation, "completed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn fail_restore(
        &self,
        project_id: &str,
        restore: &RestoreOperation,
        message: &str,
    ) -> Result<(), String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction()
            .map_err(|error| error.to_string())?;
        let updated = transaction
            .execute(
                "UPDATE trashed_projects
                 SET state = 'trashed', operation_id = ?3
                 WHERE project_id = ?1 AND operation_id = ?2 AND state = 'restoring'",
                params![
                    project_id,
                    restore.operation.operation_id,
                    restore.previous_operation_id
                ],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("trash tombstone operation did not match".to_string());
        }
        update_operation(&transaction, &restore.operation, "failed", message)?;
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn reconcile_trashed(
        &self,
        project_id: &str,
        operation: &AuditOperation,
    ) -> Result<(), String> {
        self.complete_trash(project_id, operation, "project trash reconciled")
    }

    pub(super) fn reconcile_restored(
        &self,
        project_id: &str,
        operation: &AuditOperation,
    ) -> Result<(), String> {
        let restore = RestoreOperation {
            operation: operation.clone(),
            previous_operation_id: String::new(),
        };
        self.complete_restore(project_id, &restore, "project restore reconciled")
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

fn validate_optional_database_file(root: &Path, path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.to_string()),
    };
    if is_symlink_or_reparse(&metadata) || !metadata.is_file() {
        return Err("server database path is not a regular file".to_string());
    }
    let canonical = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
    if canonical.parent() == Some(root) {
        Ok(())
    } else {
        Err("server database path is outside the configured data root".to_string())
    }
}

fn validate_optional_database_sidecar(root: &Path, path: &Path) -> Result<(), String> {
    const TRANSIENT_RETRIES: usize = 50;
    const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(2);

    if path.parent() != Some(root) {
        return Err("server database sidecar is outside the configured data root".to_string());
    }
    for attempt in 0..=TRANSIENT_RETRIES {
        match std::fs::symlink_metadata(path) {
            Ok(metadata) if is_symlink_or_reparse(&metadata) || !metadata.is_file() => {
                return Err("server database sidecar is not a regular file".to_string());
            }
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error)
                if error.kind() == std::io::ErrorKind::PermissionDenied
                    && attempt < TRANSIENT_RETRIES =>
            {
                std::thread::sleep(TRANSIENT_RETRY_DELAY);
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    Err("server database sidecar validation failed".to_string())
}

fn is_symlink_or_reparse(metadata: &std::fs::Metadata) -> bool {
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
                    'idx_service_audit_project_id',
                    'idx_service_audit_operation_id'
                ))
            "#,
            [],
            |row| row.get::<_, usize>(0),
        )
        .map_err(|error| error.to_string())?;
    Ok(object_count == 7
        && column_exists(connection, "service_audit", "status")?
        && column_exists(connection, "service_audit", "operation_id")?
        && column_exists(connection, "service_audit", "state")?
        && column_exists(connection, "service_audit", "payload")?
        && column_exists(connection, "service_audit", "updated_at")?
        && column_exists(connection, "trashed_projects", "state")?
        && column_exists(connection, "trashed_projects", "operation_id")?
        && column_exists(connection, "import_sessions", "staging_path")?
        && column_exists(connection, "import_sessions", "detected_format")?
        && column_exists(connection, "import_sessions", "analysis_json")?
        && column_exists(connection, "import_sessions", "bytes_received")?
        && column_exists(connection, "import_sessions", "file_count")?
        && column_exists(connection, "import_sessions", "error_message")?)
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
                operation_id TEXT NOT NULL UNIQUE,
                request_id TEXT NOT NULL,
                role TEXT NOT NULL,
                action TEXT NOT NULL,
                project_id TEXT,
                image_id TEXT,
                message TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'completed',
                state TEXT NOT NULL DEFAULT 'completed',
                payload TEXT NOT NULL DEFAULT '{}',
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_service_audit_request_id
                ON service_audit(request_id);
            CREATE INDEX IF NOT EXISTS idx_service_audit_project_id
                ON service_audit(project_id);

            CREATE TABLE IF NOT EXISTS trashed_projects (
                project_id TEXT PRIMARY KEY,
                trashed_at TEXT NOT NULL,
                state TEXT NOT NULL DEFAULT 'trashed',
                operation_id TEXT NOT NULL,
                FOREIGN KEY(operation_id) REFERENCES service_audit(operation_id)
            );

            CREATE TABLE IF NOT EXISTS import_sessions (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                status TEXT NOT NULL,
                staging_path TEXT NOT NULL DEFAULT '',
                detected_format TEXT,
                analysis_json TEXT,
                bytes_received INTEGER NOT NULL DEFAULT 0,
                file_count INTEGER NOT NULL DEFAULT 0,
                error_message TEXT,
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
    for (table, column, definition) in [
        (
            "service_audit",
            "status",
            "TEXT NOT NULL DEFAULT 'completed'",
        ),
        ("service_audit", "operation_id", "TEXT"),
        (
            "service_audit",
            "state",
            "TEXT NOT NULL DEFAULT 'completed'",
        ),
        ("service_audit", "payload", "TEXT NOT NULL DEFAULT '{}'"),
        ("service_audit", "updated_at", "TEXT NOT NULL DEFAULT ''"),
        (
            "trashed_projects",
            "state",
            "TEXT NOT NULL DEFAULT 'trashed'",
        ),
        ("trashed_projects", "operation_id", "TEXT"),
        (
            "import_sessions",
            "staging_path",
            "TEXT NOT NULL DEFAULT ''",
        ),
        ("import_sessions", "detected_format", "TEXT"),
        ("import_sessions", "analysis_json", "TEXT"),
        (
            "import_sessions",
            "bytes_received",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "import_sessions",
            "file_count",
            "INTEGER NOT NULL DEFAULT 0",
        ),
        ("import_sessions", "error_message", "TEXT"),
    ] {
        if !column_exists(&transaction, table, column)? {
            transaction
                .execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"),
                    [],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    transaction
        .execute(
            "UPDATE service_audit
             SET operation_id = 'legacy-' || id
             WHERE operation_id IS NULL OR operation_id = ''",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "DELETE FROM import_sessions
             WHERE project_id IS NULL OR project_id = '' OR staging_path = ''",
            [],
        )
        .map_err(|error| error.to_string())?;
    migrate_legacy_tombstone_operations(&transaction)?;
    transaction
        .execute(
            "UPDATE service_audit
             SET state = CASE WHEN status = 'intent' THEN 'pending' ELSE status END,
                 status = CASE WHEN status = 'intent' THEN 'pending' ELSE status END,
                 updated_at = CASE WHEN updated_at = '' THEN created_at ELSE updated_at END",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction
        .execute(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_service_audit_operation_id
             ON service_audit(operation_id)",
            [],
        )
        .map_err(|error| error.to_string())?;
    transaction.commit().map_err(|error| error.to_string())
}

fn migrate_legacy_tombstone_operations(transaction: &Transaction<'_>) -> Result<(), String> {
    let legacy_tombstones = {
        let mut statement = transaction
            .prepare(
                "SELECT project_id, state, trashed_at
                 FROM trashed_projects
                 WHERE operation_id IS NULL OR operation_id = ''
                 ORDER BY project_id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?
    };

    for (project_id, trash_state, created_at) in legacy_tombstones {
        let (action, operation_state) = match trash_state.as_str() {
            "trashing" => ("delete_project", "pending"),
            "trashed" => ("delete_project", "completed"),
            "restoring" => ("restore_project", "pending"),
            _ => return Err("legacy trash tombstone has an invalid state".to_string()),
        };
        let operation_id = next_operation_id();
        let request_id = format!("legacy-migration-{operation_id}");
        let payload = serde_json::json!({ "projectId": project_id }).to_string();
        transaction
            .execute(
                r#"
                INSERT INTO service_audit (
                    operation_id, request_id, role, action, project_id, image_id,
                    message, status, state, payload, created_at, updated_at
                )
                VALUES (
                    ?1, ?2, 'system', ?3, ?4, NULL,
                    'legacy trash tombstone migrated', ?5, ?5, ?6, ?7, ?7
                )
                "#,
                params![
                    operation_id,
                    request_id,
                    action,
                    project_id,
                    operation_state,
                    payload,
                    created_at
                ],
            )
            .map_err(|error| error.to_string())?;
        let updated = transaction
            .execute(
                "UPDATE trashed_projects
                 SET operation_id = ?1
                 WHERE project_id = ?2
                   AND (operation_id IS NULL OR operation_id = '')",
                params![operation_id, project_id],
            )
            .map_err(|error| error.to_string())?;
        if updated != 1 {
            return Err("legacy trash tombstone binding changed during migration".to_string());
        }
    }
    Ok(())
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

fn import_record_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ImportRecord> {
    Ok(ImportRecord {
        id: row.get(0)?,
        project_id: row.get(1)?,
        state: row.get(2)?,
        staging_path: row.get(3)?,
        detected_format: row.get(4)?,
        analysis_json: row.get(5)?,
        bytes_received: row.get(6)?,
        file_count: row.get(7)?,
        error_message: row.get(8)?,
        created_at: row.get(9)?,
        updated_at: row.get(10)?,
    })
}

fn insert_operation(
    connection: &Connection,
    entry: AuditEntry<'_>,
) -> Result<AuditOperation, String> {
    let operation = AuditOperation {
        operation_id: next_operation_id(),
    };
    let now = now_unix_string();
    connection
        .execute(
            r#"
            INSERT INTO service_audit (
                operation_id, request_id, role, action, project_id, image_id,
                message, status, state, payload, created_at, updated_at
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', 'pending', ?8, ?9, ?9)
            "#,
            params![
                operation.operation_id,
                entry.request_id,
                entry.role,
                entry.action,
                entry.project_id,
                entry.image_id,
                entry.message,
                entry.payload,
                now,
            ],
        )
        .map_err(|error| error.to_string())?;
    Ok(operation)
}

fn update_operation(
    connection: &Connection,
    operation: &AuditOperation,
    state: &str,
    message: &str,
) -> Result<(), String> {
    let updated = connection
        .execute(
            "UPDATE service_audit
             SET status = ?1, state = ?1, message = ?2, updated_at = ?3
             WHERE operation_id = ?4 AND state = 'pending'",
            params![state, message, now_unix_string(), operation.operation_id],
        )
        .map_err(|error| error.to_string())?;
    if updated == 1 {
        return Ok(());
    }
    let actual_state = connection
        .query_row(
            "SELECT state FROM service_audit WHERE operation_id = ?1",
            [&operation.operation_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if actual_state.as_deref() == Some(state) {
        Ok(())
    } else {
        Err("audit operation state did not match pending".to_string())
    }
}

fn read_operation(
    connection: &Connection,
    operation_id: &str,
) -> Result<Option<OperationRecord>, String> {
    connection
        .query_row(
            "SELECT operation_id, action, project_id, image_id, state, payload
             FROM service_audit WHERE operation_id = ?1",
            [operation_id],
            operation_from_row,
        )
        .optional()
        .map_err(|error| error.to_string())
}

fn operation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OperationRecord> {
    Ok(OperationRecord {
        operation_id: row.get(0)?,
        action: row.get(1)?,
        project_id: row.get(2)?,
        image_id: row.get(3)?,
        state: row.get(4)?,
        payload: row.get(5)?,
    })
}

fn set_description(
    connection: &Connection,
    project_id: &str,
    description: Option<&str>,
) -> Result<(), String> {
    match description {
        Some(description) => connection
            .execute(
                r#"
                INSERT INTO project_metadata (project_id, description)
                VALUES (?1, ?2)
                ON CONFLICT(project_id) DO UPDATE SET description = excluded.description
                "#,
                params![project_id, description],
            )
            .map(|_| ())
            .map_err(|error| error.to_string()),
        None => connection
            .execute(
                "DELETE FROM project_metadata WHERE project_id = ?1",
                [project_id],
            )
            .map(|_| ())
            .map_err(|error| error.to_string()),
    }
}

fn next_operation_id() -> String {
    format!(
        "operation-{}-{}-{}",
        now_unix_string(),
        std::process::id(),
        OPERATION_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_unix_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string()
}

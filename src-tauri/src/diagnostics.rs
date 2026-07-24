use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HybridDiagnostics {
    pub project_id: String,
    pub project_mode: String,
    pub server_url: Option<String>,
    pub remote_project_id: Option<String>,
    pub device_id: Option<String>,
    pub cache_policy: Option<String>,
    pub auto_sync: bool,
    pub cursor: Option<String>,
    pub last_pulled_at: Option<String>,
    pub last_pushed_at: Option<String>,
    pub pending_operations: u64,
    pub retrying_operations: u64,
    pub failed_operations: u64,
    pub oldest_pending_at: Option<String>,
    pub conflict_count: u64,
    pub cache_entries: u64,
    pub cache_bytes: u64,
    pub last_error: Option<String>,
}

pub fn collect(sqlite_path: &Path, project_id: &str) -> Result<HybridDiagnostics, String> {
    let connection = Connection::open(sqlite_path).map_err(|error| error.to_string())?;
    let remote = connection
        .query_row(
            r#"
            SELECT server_url, remote_project_id, device_id, cache_policy, auto_sync
            FROM remote_project_configs
            WHERE project_id = ?1
            "#,
            [project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let project_mode = connection
        .query_row(
            "SELECT project_mode FROM projects WHERE id = ?1",
            [project_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .unwrap_or(None)
        .unwrap_or_else(|| {
            if remote.is_some() {
                "cloud_linked".to_string()
            } else {
                "local_only".to_string()
            }
        });
    let cursor = optional_text(
        &connection,
        "SELECT cursor FROM sync_cursors WHERE project_id = ?1 LIMIT 1",
        project_id,
    );
    let last_pulled_at = optional_text(
        &connection,
        "SELECT last_pulled_at FROM sync_cursors WHERE project_id = ?1 LIMIT 1",
        project_id,
    );
    let last_pushed_at = optional_text(
        &connection,
        "SELECT last_pushed_at FROM sync_cursors WHERE project_id = ?1 LIMIT 1",
        project_id,
    );
    let pending_operations = count(
        &connection,
        "SELECT COUNT(*) FROM outbox WHERE project_id = ?1 AND status = 'pending'",
        project_id,
    );
    let retrying_operations = count(
        &connection,
        "SELECT COUNT(*) FROM outbox WHERE project_id = ?1 AND status IN ('retrying', 'sending')",
        project_id,
    );
    let failed_operations = count(
        &connection,
        "SELECT COUNT(*) FROM outbox WHERE project_id = ?1 AND status = 'failed'",
        project_id,
    );
    let oldest_pending_at = optional_text(
        &connection,
        "SELECT MIN(created_at) FROM outbox WHERE project_id = ?1 AND status IN ('pending', 'retrying', 'sending', 'failed')",
        project_id,
    );
    let conflict_count = count(
        &connection,
        "SELECT COUNT(*) FROM sync_conflicts WHERE project_id = ?1 AND status = 'open'",
        project_id,
    );
    let (cache_entries, cache_bytes) = connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0) FROM asset_cache",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?.max(0) as u64,
                    row.get::<_, i64>(1)?.max(0) as u64,
                ))
            },
        )
        .unwrap_or((0, 0));
    let last_error = optional_text(
        &connection,
        "SELECT last_error FROM outbox WHERE project_id = ?1 AND last_error IS NOT NULL ORDER BY updated_at DESC LIMIT 1",
        project_id,
    )
    .map(sanitize_error);
    let (server_url, remote_project_id, device_id, cache_policy, auto_sync) =
        remote.map_or((None, None, None, None, false), |value| {
            (
                Some(value.0),
                Some(value.1),
                Some(value.2),
                Some(value.3),
                value.4,
            )
        });

    Ok(HybridDiagnostics {
        project_id: project_id.to_string(),
        project_mode,
        server_url,
        remote_project_id,
        device_id,
        cache_policy,
        auto_sync,
        cursor,
        last_pulled_at,
        last_pushed_at,
        pending_operations,
        retrying_operations,
        failed_operations,
        oldest_pending_at,
        conflict_count,
        cache_entries,
        cache_bytes,
        last_error,
    })
}

fn count(connection: &Connection, sql: &str, project_id: &str) -> u64 {
    connection
        .query_row(sql, [project_id], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        .max(0) as u64
}

fn optional_text(connection: &Connection, sql: &str, project_id: &str) -> Option<String> {
    connection
        .query_row(sql, params![project_id], |row| row.get::<_, Option<String>>(0))
        .optional()
        .ok()
        .flatten()
        .flatten()
}

fn sanitize_error(value: String) -> String {
    let mut sanitized = value;
    for marker in ["Bearer ", "X-Amz-Signature=", "access_token=", "token="] {
        if let Some(index) = sanitized.find(marker) {
            sanitized.truncate(index);
            sanitized.push_str("[redacted]");
        }
    }
    sanitized.chars().take(500).collect()
}

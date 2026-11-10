//! Task collaboration commands.
//!
//! A project that is linked to the collaboration server must claim/release a
//! task item on the server first.  The local SQLite row is updated only after
//! that request succeeds, so a rejected claim cannot leave the UI reporting a
//! lock that the server did not grant.  Local-only projects keep the legacy
//! SQLite-only behaviour.

use crate::{commands::account, project_fs, storage};
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

struct RemoteTaskItem {
    task_id: String,
    image_id: String,
}

fn remote_task_item(
    project_id: &str,
    task_id: &str,
    image_id: &str,
) -> Result<Option<RemoteTaskItem>, String> {
    let paths = project_fs::project_paths(project_id);
    if !paths.sqlite.exists() {
        return Ok(None);
    }
    storage::initialize_project_database(&paths.sqlite)?;
    let connection = Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    let configured = connection
        .query_row(
            "SELECT remote_project_id FROM remote_project_configs WHERE project_id = ?1",
            [project_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some(_remote_project_id) = configured else {
        return Ok(None);
    };

    // A linked project must have the server UUID mapping produced by bootstrap
    // or sync.  Falling back to local IDs here would send a scoped local ID to
    // the REST route and could claim an unrelated item on another project.
    let remote_task_id = connection
        .query_row(
            "SELECT remote_id FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            [task_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("task mapping is unavailable: {error}"))?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "linked task has no remote ID; sync the project first".to_string())?;
    let remote_image_id = connection
        .query_row(
            "SELECT remote_id FROM images WHERE id = ?1 AND deleted_at IS NULL",
            [image_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(|error| format!("image mapping is unavailable: {error}"))?
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| "linked image has no remote ID; sync the project first".to_string())?;

    Ok(Some(RemoteTaskItem {
        task_id: remote_task_id,
        image_id: remote_image_id,
    }))
}

async fn remote_task_item_request(
    method: &str,
    project_id: String,
    task_id: String,
    image_id: String,
) -> Result<Option<Value>, String> {
    let mapped = tauri::async_runtime::spawn_blocking(move || {
        remote_task_item(&project_id, &task_id, &image_id)
    })
    .await
    .map_err(|error| format!("task mapping worker failed: {error}"))??;
    let Some(mapped) = mapped else {
        return Ok(None);
    };
    let path = format!(
        "/api/v1/tasks/{}/items/{}/claim",
        mapped.task_id, mapped.image_id
    );
    Ok(Some(
        account::remote_request(method.to_string(), path, None).await?,
    ))
}

#[tauri::command]
pub async fn claim_task_item(
    project_id: String,
    task_id: String,
    image_id: String,
) -> Result<(), String> {
    let remote = remote_task_item_request(
        "POST",
        project_id.clone(),
        task_id.clone(),
        image_id.clone(),
    )
    .await?;
    tauri::async_runtime::spawn_blocking(move || {
        // For a linked project this point is reached only after the atomic
        // server claim returned successfully.  For local-only projects this
        // retains the established desktop behaviour.
        let paths = project_fs::project_paths(&project_id);
        storage::claim_task_item(&paths.sqlite, &task_id, &image_id)
    })
    .await
    .map_err(|error| format!("claim task worker failed: {error}"))??;
    let _ = remote;
    Ok(())
}

#[tauri::command]
pub async fn release_task_item(
    project_id: String,
    task_id: String,
    image_id: String,
) -> Result<(), String> {
    let mapped = tauri::async_runtime::spawn_blocking({
        let project_id = project_id.clone();
        let task_id = task_id.clone();
        let image_id = image_id.clone();
        move || remote_task_item(&project_id, &task_id, &image_id)
    })
    .await
    .map_err(|error| format!("task mapping worker failed: {error}"))??;

    if let Some(mapped) = mapped {
        let path = format!(
            "/api/v1/tasks/{}/items/{}/claim",
            mapped.task_id, mapped.image_id
        );
        account::remote_request("DELETE".to_string(), path, None).await?;
    }

    tauri::async_runtime::spawn_blocking(move || {
        let paths = project_fs::project_paths(&project_id);
        storage::release_task_item(&paths.sqlite, &task_id, &image_id)
    })
    .await
    .map_err(|error| format!("release task worker failed: {error}"))??;
    Ok(())
}

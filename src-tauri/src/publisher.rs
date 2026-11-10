use crate::{
    credentials,
    domain::{DatasetImage, SampleRepository},
    hybrid,
    project_fs::ProjectPaths,
    storage, sync_engine,
};
use reqwest::{
    blocking::{Client, Response},
    header::{HeaderName, HeaderValue, CONTENT_TYPE},
    Method,
};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const HASH_BUFFER_SIZE: usize = 128 * 1024;
const PUSH_BATCH_SIZE: usize = 250;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublishResult {
    pub remote_project_id: String,
    pub created_remote_project: bool,
    pub uploaded_assets: u64,
    pub reused_assets: u64,
    pub initialized_annotations: u64,
    pub conflicts: u64,
    pub sync: sync_engine::SyncRunResult,
}

#[derive(Debug, Deserialize)]
struct RemoteProject {
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadSession {
    asset_id: String,
    object_key: String,
    upload_url: Option<String>,
    #[serde(default)]
    already_complete: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushResponse {
    results: Vec<PushResult>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PushResult {
    operation_id: String,
    status: String,
    server_revision: Option<i64>,
    remote_payload: Option<Value>,
    error: Option<String>,
}

struct LocalImage {
    id: String,
    file_name: String,
    width: i64,
    height: i64,
    split: String,
    remote_id: Option<String>,
}

struct InitialAnnotation {
    id: String,
    image_id: String,
    revision: String,
    objects: Value,
}

struct RemoteContext {
    local_project_id: String,
    server_url: String,
    remote_project_id: String,
    access_token: String,
    device_id: String,
}

pub fn publish(
    paths: &ProjectPaths,
    project_id: &str,
    server_url: &str,
    device_id: &str,
    mode: &str,
    cache_policy: &str,
    access_token: Option<&str>,
) -> Result<PublishResult, String> {
    validate_publish_input(server_url, device_id, mode, cache_policy)?;
    let mut connection = Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    hybrid::migrate_database(&mut connection)?;

    // Resolve the project scope before resolving credentials.  A publish
    // token supplied by a caller is deliberately transient: it is commonly a
    // freshly-issued CLI/native token and must not become a stale project
    // credential.  When no token is supplied, prefer the active session (and
    // refresh it under the shared credential mutex) so first publish works
    // before a remote_project_configs row exists.
    let configured_scope = connection
        .query_row(
            "SELECT server_url, remote_project_id, account_id
             FROM remote_project_configs WHERE project_id = ?1",
            [project_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let requested_server = server_url.trim_end_matches('/');
    if let Some((configured_server, _, _)) = configured_scope.as_ref() {
        if configured_server.trim_end_matches('/') != requested_server {
            return Err(
                "project is already configured for a different collaboration server".to_string(),
            );
        }
    }

    let explicit_token = access_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    // An explicit token avoids opening the native credential store unless an
    // existing project owner must be checked.  This keeps the CLI/native
    // acceptance path independent of a desktop keyring and avoids persisting
    // a token whose lifetime is controlled by the caller.
    let needs_owner_check = explicit_token.is_some()
        && configured_scope
            .as_ref()
            .and_then(|(_, _, account_id)| account_id.as_deref())
            .is_some();
    let active_session = if explicit_token.is_some() && !needs_owner_check {
        None
    } else {
        credentials::refreshed_session()?
    };

    if let Some((_, _, configured_account)) = configured_scope.as_ref() {
        if let Some(session) = active_session.as_ref() {
            if session.server_url.trim_end_matches('/') != requested_server {
                return Err("active account session does not match the project server".to_string());
            }
            if configured_account
                .as_deref()
                .is_some_and(|owner| owner != session.account_id)
            {
                return Err("active account does not own this remote project".to_string());
            }
        } else if configured_account.is_some() {
            return Err("existing remote project requires its owning account session".to_string());
        }
    }

    if let Some(session) = active_session.as_ref() {
        if session.server_url.trim_end_matches('/') != requested_server {
            return Err("active account session does not match the publish server".to_string());
        }
    }
    let access_token = match explicit_token {
        Some(token) => token,
        None => active_session
            .as_ref()
            .map(|session| session.access_token.trim())
            .filter(|token| !token.is_empty())
            .map(ToOwned::to_owned)
            .or(credentials::read_access_token(project_id)?)
            .ok_or_else(|| "project credential is required before publishing".to_string())?,
    };
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to create publish client: {error}"))?;

    materialize_source_annotations(&mut connection, paths, project_id)?;

    let existing_remote = connection
        .query_row(
            "SELECT remote_project_id FROM remote_project_configs WHERE project_id = ?1",
            [project_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let (remote_project_id, created_remote_project) = if let Some(remote_id) = existing_remote {
        (remote_id, false)
    } else {
        let (name, source_dataset_key) = connection
            .query_row(
                "SELECT name, source_dataset_key FROM projects WHERE id = ?1",
                [project_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .map_err(|error| format!("local project metadata is unavailable: {error}"))?;
        let project: RemoteProject = request_json(
            &client,
            &access_token,
            Method::POST,
            &format!("{}/api/v1/projects", server_url.trim_end_matches('/')),
            Some(json!({
                "name": name,
                "description": format!("Published from {source_dataset_key}"),
                "mode": mode
            })),
        )?;
        (project.id, true)
    };

    // Creation already sets the remote mode. Re-publishing links the local
    // cache without manufacturing another project update with an old base.
    hybrid::cache_remote_project(
        &mut connection,
        project_id,
        server_url,
        &remote_project_id,
        device_id,
        mode,
        cache_policy,
        true,
    )?;
    if let Some(session) = active_session.as_ref() {
        if session.server_url.trim_end_matches('/') == server_url.trim_end_matches('/') {
            connection
                .execute(
                    "UPDATE remote_project_configs SET account_id=?2 WHERE project_id=?1",
                    params![project_id, session.account_id],
                )
                .map_err(|error| error.to_string())?;
        }
    }
    let context = RemoteContext {
        local_project_id: project_id.into(),
        server_url: server_url.trim_end_matches('/').to_string(),
        remote_project_id: remote_project_id.clone(),
        access_token,
        device_id: device_id.to_string(),
    };

    if created_remote_project {
        let mut statement = connection
            .prepare("SELECT id,label,color FROM classes ORDER BY id")
            .map_err(|error| error.to_string())?;
        let classes=statement.query_map([],|row|Ok(json!({"id":row.get::<_,i64>(0)?,"label":row.get::<_,String>(1)?,"color":row.get::<_,String>(2)?}))).map_err(|error|error.to_string())?.collect::<Result<Vec<_>,_>>().map_err(|error|error.to_string())?;
        drop(statement);
        let _: Value = request_json(
            &client,
            &context.access_token,
            Method::PUT,
            &format!(
                "{}/api/v1/projects/{}/label-schema",
                context.server_url, context.remote_project_id
            ),
            Some(json!({"classes":classes,"baseRevision":0})),
        )?;
    }
    let images = read_images(&connection)?;
    let asset_root = resolve_asset_root(&connection, paths, project_id);
    let mut uploaded_assets = 0_u64;
    let mut reused_assets = 0_u64;
    for image in images {
        if image.remote_id.is_some() {
            reused_assets += 1;
            continue;
        }
        let file_path = resolve_image_path(&asset_root, &paths.raw, &image.file_name)?;
        let (content_hash, byte_size) = digest_file(&file_path)?;
        let mime_type = mime_type_for(&file_path);
        let session: UploadSession = request_json(
            &client,
            &context.access_token,
            Method::POST,
            &format!(
                "{}/api/v1/projects/{}/assets/upload-session",
                context.server_url, context.remote_project_id
            ),
            Some(json!({
                "clientKey": image.id,
                "fileName": image.file_name,
                "contentHash": content_hash,
                "mimeType": mime_type,
                "width": image.width.max(1),
                "height": image.height.max(1),
                "byteSize": byte_size,
                "split": image.split
            })),
        )?;
        if session.already_complete {
            reused_assets += 1;
        } else {
            let upload_url = session
                .upload_url
                .as_deref()
                .ok_or_else(|| "upload session did not include an upload URL".to_string())?;
            upload_file(&client, upload_url, &file_path, &mime_type, &content_hash)?;
            let _: Value = request_json(
                &client,
                &context.access_token,
                Method::POST,
                &format!(
                    "{}/api/v1/projects/{}/assets/{}/complete",
                    context.server_url, context.remote_project_id, session.asset_id
                ),
                None,
            )?;
            uploaded_assets += 1;
        }
        connection
            .execute(
                r#"
                UPDATE images SET
                    remote_id = ?2,
                    content_hash = ?3,
                    mime_type = ?4,
                    byte_size = ?5,
                    object_key = ?6,
                    sync_status = 'synced',
                    dirty = 0
                WHERE id = ?1
                "#,
                params![
                    image.id,
                    session.asset_id,
                    content_hash,
                    mime_type,
                    byte_size as i64,
                    session.object_key
                ],
            )
            .map_err(|error| format!("failed to save remote asset mapping: {error}"))?;
    }

    let annotations = read_initial_annotations(&connection)?;
    let (initialized_annotations, conflicts) =
        push_initial_annotations(&client, &context, &mut connection, annotations)?;
    drop(connection);
    let sync = sync_engine::sync_project(&paths.sqlite, project_id, Some(&context.access_token))?;
    Ok(PublishResult {
        remote_project_id,
        created_remote_project,
        uploaded_assets,
        reused_assets,
        initialized_annotations,
        conflicts: conflicts + u64::from(sync.conflicts),
        sync,
    })
}

fn read_images(connection: &Connection) -> Result<Vec<LocalImage>, String> {
    let mut statement = connection
        .prepare("SELECT id, file_name, width, height, split, remote_id FROM images WHERE deleted_at IS NULL ORDER BY id")
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok(LocalImage {
                id: row.get(0)?,
                file_name: row.get(1)?,
                width: row.get(2)?,
                height: row.get(3)?,
                split: row.get(4)?,
                remote_id: row.get(5)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(rows)
}

/// Untouched imported labels can still live only in their source files. Read
/// them through the same strict parser used by snapshots before contacting the
/// server, while retaining every newer SQLite draft and its durable operation.
fn materialize_source_annotations(
    connection: &mut Connection,
    paths: &ProjectPaths,
    project_id: &str,
) -> Result<(), String> {
    let manifest = storage::read_project_manifest(&paths.sqlite)?
        .ok_or("local project metadata is unavailable")?;
    let classes = storage::read_classes(&paths.sqlite)?;
    let source_root = resolve_asset_root(connection, paths, project_id);
    let repository = SampleRepository::new();
    let mut missing = Vec::new();
    for image in storage::read_images(&paths.sqlite, None)? {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM annotations WHERE image_id=?1)",
                [&image.id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if exists {
            continue;
        }
        let image = DatasetImage {
            id: image.id,
            file_name: image.file_name,
            width: image.width,
            height: image.height,
            split: image.split,
            status: image.status,
            qa_status: image.qa_status,
            review_note: image.review_note,
            tags: Vec::new(),
        };
        let state = repository.snapshot_annotation_state_strict(
            paths,
            &manifest,
            &classes,
            &source_root,
            &image,
        )?;
        missing.push((image.id, state));
    }
    let tx = connection
        .transaction()
        .map_err(|error| error.to_string())?;
    for (image_id, state) in missing {
        let objects = serde_json::to_string(&state.objects).map_err(|error| error.to_string())?;
        tx.execute(
            "INSERT INTO annotations(id,image_id,revision,object_json,updated_at) VALUES(?1,?1,?2,?3,?4) ON CONFLICT(id) DO NOTHING",
            params![image_id, state.revision.unwrap_or_else(|| format!("source-{image_id}")), objects, state.updated_at.unwrap_or_else(|| "0".into())],
        ).map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

fn read_initial_annotations(connection: &Connection) -> Result<Vec<InitialAnnotation>, String> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT annotations.id, annotations.image_id, annotations.revision,
                annotations.object_json
            FROM annotations
            JOIN images ON images.id = annotations.image_id
            WHERE images.deleted_at IS NULL AND images.remote_id IS NOT NULL AND annotations.remote_id IS NULL
              AND NOT EXISTS (SELECT 1 FROM sync_outbox o WHERE o.entity_type='annotation' AND o.operation='update'
                AND o.entity_id=annotations.image_id AND (o.status IN ('pending','syncing','conflict')
                  OR (o.status='failed' AND o.next_retry_at IS NOT NULL)))
            ORDER BY annotations.id
            "#,
        )
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            let raw: String = row.get(3)?;
            Ok(InitialAnnotation {
                id: row.get(0)?,
                image_id: row.get(1)?,
                revision: row.get(2)?,
                objects: serde_json::from_str(&raw).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    Ok(rows)
}

fn push_initial_annotations(
    client: &Client,
    context: &RemoteContext,
    connection: &mut Connection,
    annotations: Vec<InitialAnnotation>,
) -> Result<(u64, u64), String> {
    let mut initialized = 0_u64;
    let mut conflicts = 0_u64;
    for batch in annotations.chunks(PUSH_BATCH_SIZE) {
        let operations = batch
            .iter()
            .map(|annotation| {
                json!({
                    "operationId": format!(
                        "publish:{}:{}:{}",
                        context.remote_project_id, annotation.id, annotation.revision
                    ),
                    "entityType": "annotation",
                    "entityId": annotation.image_id,
                    "operation": "update",
                    "baseRevision": 0,
                    "payload": { "objects": annotation.objects }
                })
            })
            .collect::<Vec<_>>();
        let response: PushResponse = request_json(
            client,
            &context.access_token,
            Method::POST,
            &format!("{}/api/v1/sync/push", context.server_url),
            Some(json!({
                "deviceId": context.device_id,
                "projectId": context.remote_project_id,
                "operations": operations
            })),
        )?;
        for result in response.results {
            let annotation = batch.iter().find(|annotation| {
                result.operation_id
                    == format!(
                        "publish:{}:{}:{}",
                        context.remote_project_id, annotation.id, annotation.revision
                    )
            });
            match result.status.as_str() {
                "applied" | "duplicate" => {
                    if let (Some(annotation), Some(payload)) =
                        (annotation, result.remote_payload.as_ref())
                    {
                        let revision = result
                            .server_revision
                            .or_else(|| payload.get("revision").and_then(Value::as_i64));
                        let tx = connection
                            .transaction()
                            .map_err(|error| error.to_string())?;
                        let updated = tx.execute("UPDATE annotations SET remote_id=?2,server_revision=?3,sync_status='synced',dirty=0 WHERE id=?1 AND revision=?4",params![annotation.id,payload.get("id").and_then(Value::as_str),revision,annotation.revision]).map_err(|error|error.to_string())?;
                        if updated == 1 {
                            // A successful publish contains the current snapshot. Retain
                            // rejected attempts for audit, but let later submit operations
                            // proceed instead of remaining blocked behind a terminal error.
                            tx.execute("UPDATE sync_outbox SET status='superseded' WHERE project_id=?1 AND entity_type='annotation' AND entity_id=?2 AND operation='update' AND (status='rejected' OR (status='failed' AND next_retry_at IS NULL))",params![context.local_project_id,annotation.image_id]).map_err(|error|error.to_string())?;
                        }
                        tx.execute("INSERT INTO sync_entity_bases(project_id,entity_type,entity_id,revision,payload_json) VALUES(?1,'annotation',?2,?3,?4) ON CONFLICT(project_id,entity_type,entity_id) DO UPDATE SET revision=excluded.revision,payload_json=excluded.payload_json",params![context.local_project_id,annotation.image_id,revision,payload.to_string()]).map_err(|error|error.to_string())?;
                        tx.execute("UPDATE sync_outbox SET base_revision=?3,base_json=?4 WHERE project_id=?1 AND entity_id=?2 AND status='pending' AND attempted_at IS NULL",params![context.local_project_id,annotation.image_id,revision,payload.to_string()]).map_err(|error|error.to_string())?;
                        tx.commit().map_err(|error| error.to_string())?;
                    }
                    initialized += 1;
                }
                "conflict" => conflicts += 1,
                _ => {
                    return Err(result
                        .error
                        .unwrap_or_else(|| format!("initial annotation push {}", result.status)));
                }
            }
        }
    }
    Ok((initialized, conflicts))
}

fn request_json<T: DeserializeOwned>(
    client: &Client,
    access_token: &str,
    method: Method,
    url: &str,
    body: Option<Value>,
) -> Result<T, String> {
    let mut request = client.request(method, url).bearer_auth(access_token);
    if let Some(value) = body {
        request = request.json(&value);
    }
    decode_response(
        request
            .send()
            .map_err(|error| format!("publish request failed: {error}"))?,
    )
}

fn decode_response<T: DeserializeOwned>(response: Response) -> Result<T, String> {
    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("failed to read publish response: {error}"))?;
    if !status.is_success() {
        let message = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("message")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| format!("publish request failed with {status}"));
        return Err(message);
    }
    serde_json::from_str(&text).map_err(|error| format!("invalid publish response: {error}"))
}

fn upload_file(
    client: &Client,
    upload_url: &str,
    file_path: &Path,
    mime_type: &str,
    content_hash: &str,
) -> Result<(), String> {
    let file = File::open(file_path)
        .map_err(|error| format!("failed to open {}: {error}", file_path.display()))?;
    let response = client
        .put(upload_url)
        .header(CONTENT_TYPE, mime_type)
        .header(
            HeaderName::from_static("x-amz-meta-sha256"),
            HeaderValue::from_str(content_hash).map_err(|error| error.to_string())?,
        )
        .body(reqwest::blocking::Body::new(file))
        .send()
        .map_err(|error| format!("object upload failed: {error}"))?;
    if response.status().is_success() {
        Ok(())
    } else {
        Err(format!("object upload failed with {}", response.status()))
    }
}

fn digest_file(path: &Path) -> Result<(String, u64), String> {
    let mut file =
        File::open(path).map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    let mut buffer = vec![0_u8; HASH_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to hash {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_size = byte_size.saturating_add(read as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), byte_size))
}

fn resolve_asset_root(connection: &Connection, paths: &ProjectPaths, project_id: &str) -> PathBuf {
    connection
        .query_row(
            "SELECT root_path FROM projects WHERE id = ?1",
            [project_id],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .filter(|path| path.exists())
        .unwrap_or_else(|| paths.raw.clone())
}

fn resolve_image_path(
    asset_root: &Path,
    fallback_root: &Path,
    file_name: &str,
) -> Result<PathBuf, String> {
    [
        asset_root.join(file_name),
        asset_root.join("images").join(file_name),
        fallback_root.join(file_name),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .ok_or_else(|| format!("image file is missing: {file_name}"))
}

fn mime_type_for(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        _ => "application/octet-stream",
    }
    .to_string()
}

fn validate_publish_input(
    server_url: &str,
    device_id: &str,
    mode: &str,
    cache_policy: &str,
) -> Result<(), String> {
    if !(server_url.starts_with("http://") || server_url.starts_with("https://")) {
        return Err("server URL must use http or https".to_string());
    }
    if device_id.trim().is_empty() {
        return Err("device ID is required".to_string());
    }
    if !matches!(mode, "cloud_linked" | "mirrored") {
        return Err("publish mode must be cloud_linked or mirrored".to_string());
    }
    if !matches!(cache_policy, "thumbnail_only" | "on_demand" | "full_mirror") {
        return Err("invalid cache policy".to_string());
    }
    Ok(())
}

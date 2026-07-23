use crate::{credentials, hybrid, project_fs::ProjectPaths, sync_engine};
use reqwest::{
    blocking::{Client, Response},
    header::{CONTENT_TYPE, HeaderName, HeaderValue},
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
    if let Some(token) = access_token.filter(|value| !value.trim().is_empty()) {
        credentials::store_access_token(project_id, token.trim())?;
    }
    let access_token = credentials::read_access_token(project_id)?
        .ok_or_else(|| "project credential is required before publishing".to_string())?;
    let mut connection =
        Connection::open(&paths.sqlite).map_err(|error| error.to_string())?;
    hybrid::migrate_database(&mut connection)?;
    let client = Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to create publish client: {error}"))?;

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

    hybrid::configure_remote_project(
        &mut connection,
        project_id,
        server_url,
        &remote_project_id,
        device_id,
        mode,
        cache_policy,
        true,
    )?;
    let context = RemoteContext {
        server_url: server_url.trim_end_matches('/').to_string(),
        remote_project_id: remote_project_id.clone(),
        access_token,
        device_id: device_id.to_string(),
    };

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
            upload_file(
                &client,
                upload_url,
                &file_path,
                &mime_type,
                &content_hash,
            )?;
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
    let sync =
        sync_engine::sync_project(&paths.sqlite, project_id, Some(&context.access_token))?;
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
        .prepare(
            "SELECT id, file_name, width, height, split, remote_id FROM images ORDER BY id",
        )
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

fn read_initial_annotations(connection: &Connection) -> Result<Vec<InitialAnnotation>, String> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT annotations.id, annotations.image_id, annotations.revision,
                annotations.object_json
            FROM annotations
            JOIN images ON images.id = annotations.image_id
            WHERE images.remote_id IS NOT NULL
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
                objects: serde_json::from_str(&raw).unwrap_or_else(|_| json!([])),
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
                        connection
                            .execute(
                                "UPDATE annotations SET remote_id = ?2, sync_status = 'synced', dirty = 0 WHERE id = ?1",
                                params![
                                    annotation.id,
                                    payload.get("id").and_then(Value::as_str)
                                ],
                            )
                            .map_err(|error| error.to_string())?;
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
            .and_then(|value| value.get("message").and_then(Value::as_str).map(str::to_owned))
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

fn resolve_asset_root(
    connection: &Connection,
    paths: &ProjectPaths,
    project_id: &str,
) -> PathBuf {
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
    if !matches!(
        cache_policy,
        "thumbnail_only" | "on_demand" | "full_mirror"
    ) {
        return Err("invalid cache policy".to_string());
    }
    Ok(())
}

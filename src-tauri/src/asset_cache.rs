use reqwest::blocking::Client;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};

const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
const COPY_BUFFER_SIZE: usize = 128 * 1024;

#[derive(Debug, Clone)]
pub struct CacheFetchRequest {
    pub download_url: String,
    pub content_hash: String,
    pub expected_size: Option<u64>,
    pub extension: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheFetchResult {
    pub path: PathBuf,
    pub byte_size: u64,
    pub content_hash: String,
    pub cache_hit: bool,
}

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub asset_id: String,
    pub path: PathBuf,
    pub byte_size: u64,
    pub last_accessed_unix_ms: i64,
    pub protected: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCacheRecord {
    pub asset_id: String,
    pub content_hash: String,
    pub local_path: String,
    pub cache_kind: String,
    pub byte_size: u64,
    pub last_accessed_at: String,
    pub verified_at: Option<String>,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCacheSummary {
    pub entry_count: u64,
    pub byte_size: u64,
    pub pinned_count: u64,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssetCacheCleanupResult {
    pub removed_count: usize,
    pub removed_bytes: u64,
    pub remaining: AssetCacheSummary,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteDownloadTicket {
    pub asset_id: String,
    pub download_url: String,
    pub file_name: String,
    pub content_hash: String,
    pub mime_type: String,
    pub byte_size: u64,
    pub expires_in: u64,
}

pub fn fetch(cache_root: &Path, request: &CacheFetchRequest) -> Result<CacheFetchResult, String> {
    let expected_hash = normalize_hash(&request.content_hash)?;
    let destination = cache_path(cache_root, &expected_hash, request.extension.as_deref())?;

    if destination.is_file() {
        let (actual_hash, byte_size) = digest_file(&destination)?;
        if actual_hash == expected_hash && size_matches(byte_size, request.expected_size) {
            return Ok(CacheFetchResult {
                path: destination,
                byte_size,
                content_hash: actual_hash,
                cache_hit: true,
            });
        }
        fs::remove_file(&destination).map_err(|error| {
            format!(
                "failed to remove invalid cached object {}: {error}",
                destination.display()
            )
        })?;
    }

    let parent = destination
        .parent()
        .ok_or_else(|| "cache destination has no parent directory".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create cache directory: {error}"))?;

    let temporary = destination.with_extension(format!(
        "{}.part-{}",
        destination
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("cache"),
        std::process::id()
    ));
    let result = download_and_verify(&temporary, request, &expected_hash);
    match result {
        Ok((actual_hash, byte_size)) => {
            if destination.exists() {
                fs::remove_file(&destination)
                    .map_err(|error| format!("failed to replace cached object: {error}"))?;
            }
            fs::rename(&temporary, &destination)
                .map_err(|error| format!("failed to commit cached object: {error}"))?;
            Ok(CacheFetchResult {
                path: destination,
                byte_size,
                content_hash: actual_hash,
                cache_hit: false,
            })
        }
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            Err(error)
        }
    }
}

pub fn request_download_ticket(
    server_url: &str,
    remote_asset_id: &str,
    access_token: &str,
) -> Result<RemoteDownloadTicket, String> {
    let endpoint = format!(
        "{}/api/v1/assets/{}/download-url",
        server_url.trim_end_matches('/'),
        remote_asset_id
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| format!("failed to create asset metadata client: {error}"))?;
    let response = client
        .get(endpoint)
        .bearer_auth(access_token)
        .send()
        .map_err(|error| format!("failed to request asset download metadata: {error}"))?
        .error_for_status()
        .map_err(|error| format!("asset download metadata was rejected: {error}"))?;
    response
        .json()
        .map_err(|error| format!("invalid asset download metadata: {error}"))
}

pub fn register(
    connection: &Connection,
    asset_id: &str,
    result: &CacheFetchResult,
    pinned: bool,
) -> Result<AssetCacheRecord, String> {
    let local_path = result.path.to_string_lossy().into_owned();
    connection
        .execute(
            r#"
            INSERT INTO asset_cache (
                asset_id, content_hash, local_path, cache_kind, byte_size,
                last_accessed_at, verified_at, pinned
            ) VALUES (
                ?1, ?2, ?3, 'original', ?4,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?5
            )
            ON CONFLICT(asset_id) DO UPDATE SET
                content_hash = excluded.content_hash,
                local_path = excluded.local_path,
                cache_kind = excluded.cache_kind,
                byte_size = excluded.byte_size,
                last_accessed_at = excluded.last_accessed_at,
                verified_at = excluded.verified_at,
                pinned = MAX(asset_cache.pinned, excluded.pinned)
            "#,
            params![
                asset_id,
                result.content_hash,
                local_path,
                result.byte_size as i64,
                i64::from(pinned)
            ],
        )
        .map_err(|error| format!("failed to register asset cache entry: {error}"))?;
    find_record(connection, asset_id)?
        .ok_or_else(|| "cached asset was not registered".to_string())
}

pub fn touch(
    connection: &Connection,
    asset_id: &str,
    pinned: bool,
) -> Result<Option<AssetCacheRecord>, String> {
    let local_path = connection
        .query_row(
            "SELECT local_path FROM asset_cache WHERE asset_id = ?1",
            [asset_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(path) = local_path.as_deref() {
        if Path::new(path).is_file() {
            connection
                .execute(
                    "UPDATE asset_cache SET last_accessed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), pinned = MAX(pinned, ?2) WHERE asset_id = ?1",
                    params![asset_id, i64::from(pinned)],
                )
                .map_err(|error| error.to_string())?;
            return find_record(connection, asset_id);
        }
        connection
            .execute("DELETE FROM asset_cache WHERE asset_id = ?1", [asset_id])
            .map_err(|error| error.to_string())?;
    }
    Ok(None)
}

pub fn summary(connection: &Connection) -> Result<AssetCacheSummary, String> {
    connection
        .query_row(
            "SELECT COUNT(*), COALESCE(SUM(byte_size), 0), COALESCE(SUM(CASE WHEN pinned = 1 THEN 1 ELSE 0 END), 0) FROM asset_cache",
            [],
            |row| {
                Ok(AssetCacheSummary {
                    entry_count: row.get::<_, i64>(0)?.max(0) as u64,
                    byte_size: row.get::<_, i64>(1)?.max(0) as u64,
                    pinned_count: row.get::<_, i64>(2)?.max(0) as u64,
                })
            },
        )
        .map_err(|error| error.to_string())
}

pub fn cleanup_registered(
    connection: &mut Connection,
    target_bytes: u64,
) -> Result<AssetCacheCleanupResult, String> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT
                cache.asset_id,
                cache.local_path,
                cache.byte_size,
                COALESCE(CAST(strftime('%s', cache.last_accessed_at) AS INTEGER) * 1000, 0),
                CASE
                    WHEN cache.pinned = 1 THEN 1
                    WHEN EXISTS (
                        SELECT 1 FROM images
                        WHERE images.id = cache.asset_id
                          AND (
                              images.dirty = 1
                              OR images.sync_status IN ('dirty', 'pending', 'failed', 'conflict')
                          )
                    ) THEN 1
                    WHEN EXISTS (
                        SELECT 1 FROM outbox
                        WHERE outbox.entity_id = cache.asset_id
                          AND outbox.status IN ('pending', 'retrying', 'failed')
                    ) THEN 1
                    ELSE 0
                END
            FROM asset_cache AS cache
            "#,
        )
        .map_err(|error| error.to_string())?;
    let mut entries = statement
        .query_map([], |row| {
            Ok(CacheEntry {
                asset_id: row.get(0)?,
                path: PathBuf::from(row.get::<_, String>(1)?),
                byte_size: row.get::<_, i64>(2)?.max(0) as u64,
                last_accessed_unix_ms: row.get(3)?,
                protected: row.get::<_, i64>(4)? != 0,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    drop(statement);

    let before = entries
        .iter()
        .filter(|entry| entry.path.is_file())
        .map(|entry| entry.byte_size)
        .sum::<u64>();
    let removed_paths = cleanup(&mut entries, target_bytes)?;
    let removed_bytes = entries
        .iter()
        .filter(|entry| removed_paths.contains(&entry.path))
        .map(|entry| entry.byte_size)
        .sum::<u64>();
    let transaction = connection.transaction().map_err(|error| error.to_string())?;
    for path in &removed_paths {
        transaction
            .execute(
                "DELETE FROM asset_cache WHERE local_path = ?1",
                [path.to_string_lossy().as_ref()],
            )
            .map_err(|error| error.to_string())?;
    }
    transaction.commit().map_err(|error| error.to_string())?;
    let remaining = summary(connection)?;
    debug_assert_eq!(before.saturating_sub(removed_bytes), remaining.byte_size);
    Ok(AssetCacheCleanupResult {
        removed_count: removed_paths.len(),
        removed_bytes,
        remaining,
    })
}

pub fn default_cache_root(sqlite_path: &Path) -> PathBuf {
    sqlite_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("cache")
        .join("assets")
}

fn find_record(
    connection: &Connection,
    asset_id: &str,
) -> Result<Option<AssetCacheRecord>, String> {
    connection
        .query_row(
            "SELECT asset_id, content_hash, local_path, cache_kind, byte_size, last_accessed_at, verified_at, pinned FROM asset_cache WHERE asset_id = ?1",
            [asset_id],
            |row| {
                Ok(AssetCacheRecord {
                    asset_id: row.get(0)?,
                    content_hash: row.get(1)?,
                    local_path: row.get(2)?,
                    cache_kind: row.get(3)?,
                    byte_size: row.get::<_, i64>(4)?.max(0) as u64,
                    last_accessed_at: row.get(5)?,
                    verified_at: row.get(6)?,
                    pinned: row.get::<_, i64>(7)? != 0,
                })
            },
        )
        .optional()
        .map_err(|error| error.to_string())
}

pub fn cleanup(
    entries: &mut [CacheEntry],
    target_bytes: u64,
) -> Result<Vec<PathBuf>, String> {
    let mut current_bytes = entries
        .iter()
        .filter(|entry| entry.path.is_file())
        .map(|entry| entry.byte_size)
        .sum::<u64>();
    if current_bytes <= target_bytes {
        return Ok(Vec::new());
    }

    entries.sort_by_key(|entry| entry.last_accessed_unix_ms);
    let mut removed = Vec::new();
    for entry in entries.iter() {
        if current_bytes <= target_bytes {
            break;
        }
        if entry.protected || !entry.path.is_file() {
            continue;
        }
        fs::remove_file(&entry.path).map_err(|error| {
            format!(
                "failed to remove cached object {}: {error}",
                entry.path.display()
            )
        })?;
        current_bytes = current_bytes.saturating_sub(entry.byte_size);
        removed.push(entry.path.clone());
    }
    Ok(removed)
}

pub fn cache_path(
    cache_root: &Path,
    content_hash: &str,
    extension: Option<&str>,
) -> Result<PathBuf, String> {
    let hash = normalize_hash(content_hash)?;
    let extension = sanitize_extension(extension);
    let file_name = match extension {
        Some(value) => format!("{hash}.{value}"),
        None => hash.clone(),
    };
    Ok(cache_root.join(&hash[..2]).join(file_name))
}

fn download_and_verify(
    temporary: &Path,
    request: &CacheFetchRequest,
    expected_hash: &str,
) -> Result<(String, u64), String> {
    let client = Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|error| format!("failed to create cache download client: {error}"))?;
    let mut response = client
        .get(&request.download_url)
        .send()
        .map_err(|error| format!("asset download failed: {error}"))?
        .error_for_status()
        .map_err(|error| format!("asset download was rejected: {error}"))?;
    let mut file = File::create(temporary)
        .map_err(|error| format!("failed to create temporary cache file: {error}"))?;
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];

    loop {
        let read = response
            .read(&mut buffer)
            .map_err(|error| format!("failed while reading asset response: {error}"))?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .map_err(|error| format!("failed while writing cached asset: {error}"))?;
        hasher.update(&buffer[..read]);
        byte_size = byte_size.saturating_add(read as u64);
    }
    file.sync_all()
        .map_err(|error| format!("failed to flush cached asset: {error}"))?;

    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        return Err(format!(
            "asset checksum mismatch: expected {expected_hash}, received {actual_hash}"
        ));
    }
    if !size_matches(byte_size, request.expected_size) {
        return Err(format!(
            "asset size mismatch: expected {}, received {byte_size}",
            request.expected_size.unwrap_or_default()
        ));
    }
    Ok((actual_hash, byte_size))
}

fn digest_file(path: &Path) -> Result<(String, u64), String> {
    let mut file = File::open(path)
        .map_err(|error| format!("failed to open cached object {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut byte_size = 0_u64;
    let mut buffer = vec![0_u8; COPY_BUFFER_SIZE];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to verify cached object: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        byte_size = byte_size.saturating_add(read as u64);
    }
    Ok((format!("{:x}", hasher.finalize()), byte_size))
}

fn normalize_hash(value: &str) -> Result<String, String> {
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.len() != 64 || !normalized.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("content hash must be a 64-character SHA-256 hex digest".to_string());
    }
    Ok(normalized)
}

fn sanitize_extension(value: Option<&str>) -> Option<String> {
    value.and_then(|extension| {
        let normalized = extension
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase();
        if normalized.is_empty()
            || normalized.len() > 12
            || !normalized
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric())
        {
            None
        } else {
            Some(normalized)
        }
    })
}

fn size_matches(actual: u64, expected: Option<u64>) -> bool {
    expected.map_or(true, |value| value == actual)
}

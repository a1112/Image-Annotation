//! Persistent dataset import/export jobs.
//!
//! This module deliberately keeps the HTTP/worker code separate from the
//! dataset and quota migration.  `server_review` owns the migration and route
//! wiring; the SQL contract used here is documented next to the queries so it
//! can be checked without changing the already-applied migrations.
//!
//! The migration must provide a `dataset_jobs` table with these columns:
//!
//! * `id`, `dataset_id`, `kind` (`import`/`export`), `format`,
//!   `operation_id`, `requested_by`, `version_id`, `status`,
//!   `input_object_key`, `output_object_key`, `input_content_hash`,
//!   `output_content_hash`, `input_byte_size`, `output_byte_size`,
//!   `quota_reservation_id`, `error_code`, `error_message`, `created_at`,
//!   `updated_at`, `started_at`, and `finished_at`;
//! * a unique constraint on `(dataset_id, operation_id)`.
//!
//! Quota is intentionally a database helper boundary.  The dataset migration
//! should expose `reserve_dataset_job_quota(dataset_id, user_id, bytes,
//! operation_id)` returning a reservation UUID and
//! `release_dataset_job_quota(reservation_id)`.  Keeping the reservation in
//! the job row makes a crash/retry idempotent and lets a failed job release its
//! bytes without touching the imported dataset.

use super::{objects::verify_object_hash, ApiError};
use crate::{
    auth::{require_dataset_permission, AuthUser, DatasetPermission},
    state::AppState,
};
use aws_sdk_s3::primitives::ByteStream;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use futures_util::StreamExt;
use image::ImageReader;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{postgres::PgRow, Postgres, Row, Transaction};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::{self, Cursor, Read, Seek, SeekFrom, Write},
    path::{Component, Path as FsPath},
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{OwnedSemaphorePermit, Semaphore},
    task::JoinHandle,
};
use uuid::Uuid;
use zip::{read::ZipFile, write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

/// The request body is bounded before it reaches the ZIP parser.  Deployments
/// with larger datasets should use a resumable object upload and enqueue the
/// resulting object through the same job table rather than raising this limit.
pub const MAX_COMPRESSED_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_EXPANDED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_ENTRY_PATH_BYTES: usize = 4096;
pub const MAX_COMPRESSION_RATIO: u64 = 1_000;
pub const MAX_IMAGE_PIXELS: u64 = 100_000_000;
pub const MAX_IMAGE_DECODE_BYTES: u64 = 256 * 1024 * 1024;

// ZIP validation is CPU- and allocation-heavy.  The durable worker is also
// single-flight, so serialize request-side validation to keep many concurrent
// uploads from multiplying the bounded archive peak in memory.
static IMPORT_VALIDATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

// Operation locks are held across the object upload and the durable job
// insert.  Bound the number of requests that can wait on a PostgreSQL session
// advisory lock before acquiring a pool connection: otherwise a burst of
// distinct operation IDs can occupy every connection while the transactions
// that finish those jobs wait forever for a connection of their own.
const MAX_OPERATION_LOCKS: usize = 8;
static OPERATION_LOCK_SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn operation_lock_semaphore() -> &'static Arc<Semaphore> {
    OPERATION_LOCK_SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(MAX_OPERATION_LOCKS)))
}

const JOB_COLUMNS: &str = r#"
    id, dataset_id, kind, format, operation_id, requested_by, version_id,
    status, input_object_key, output_object_key, input_content_hash,
    output_content_hash, input_byte_size, output_byte_size,
    quota_reservation_id, error_code, error_message, created_at, updated_at,
    started_at, finished_at
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatasetFormat {
    Native,
    Coco,
    Yolo,
    YoloSeg,
    LabelMe,
}

impl DatasetFormat {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "native" | "image-annotation" | "image_annotation" => Ok(Self::Native),
            "coco" | "coco-json" => Ok(Self::Coco),
            "yolo" | "yolo-detect" => Ok(Self::Yolo),
            "yolo-seg" | "yolo-segmentation" => Ok(Self::YoloSeg),
            "labelme" | "label-me" => Ok(Self::LabelMe),
            other => Err(format!("unsupported dataset format '{other}'")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Coco => "coco",
            Self::Yolo => "yolo",
            Self::YoloSeg => "yolo-seg",
            Self::LabelMe => "labelme",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ArchiveLimits {
    pub compressed_bytes: usize,
    pub expanded_bytes: u64,
    pub entries: usize,
    pub path_bytes: usize,
    pub compression_ratio: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            compressed_bytes: MAX_COMPRESSED_BYTES,
            expanded_bytes: MAX_EXPANDED_BYTES,
            entries: MAX_ENTRIES,
            path_bytes: MAX_ENTRY_PATH_BYTES,
            compression_ratio: MAX_COMPRESSION_RATIO,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub path: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ImportedImage {
    pub key: String,
    pub file_name: String,
    pub width: u32,
    pub height: u32,
    pub split: String,
    pub mime_type: String,
    pub content_hash: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ImportedAnnotation {
    pub image_key: String,
    pub objects: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct ImportBundle {
    pub format: DatasetFormat,
    pub classes: Vec<String>,
    pub images: Vec<ImportedImage>,
    pub annotations: Vec<ImportedAnnotation>,
    pub manifest: Value,
}

#[derive(Debug, Deserialize)]
pub struct ImportQuery {
    pub format: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportJobRequest {
    pub version_id: Uuid,
    pub format: String,
    pub operation_id: String,
}

#[derive(Debug, Clone)]
struct JobRecord {
    id: Uuid,
    dataset_id: Uuid,
    kind: String,
    format: String,
    operation_id: String,
    requested_by: Uuid,
    version_id: Option<Uuid>,
    status: String,
    input_object_key: Option<String>,
    output_object_key: Option<String>,
    input_content_hash: Option<String>,
    output_content_hash: Option<String>,
    input_byte_size: Option<i64>,
    output_byte_size: Option<i64>,
    quota_reservation_id: Option<Uuid>,
    error_code: Option<String>,
    error_message: Option<String>,
}

fn normalize_zip_path(raw: &str, limit: usize) -> Result<String, String> {
    if raw.is_empty() || raw.as_bytes().len() > limit || raw.contains('\0') {
        return Err("archive entry path is empty, too long, or contains NUL".to_string());
    }
    // Backslashes are rejected instead of translated.  This keeps the path
    // contract identical on Unix and Windows and prevents a Unix server from
    // accepting a path that becomes traversal after extraction on a client.
    if raw.contains('\\') || raw.starts_with('/') || raw.starts_with('~') {
        return Err(format!("unsafe archive entry path '{raw}'"));
    }
    if raw.len() >= 2 && raw.as_bytes()[1] == b':' {
        return Err(format!("absolute archive entry path '{raw}'"));
    }
    let mut components = Vec::new();
    for component in FsPath::new(raw).components() {
        match component {
            Component::Normal(part) => {
                let part = part
                    .to_str()
                    .ok_or_else(|| format!("archive entry path is not UTF-8: '{raw}'"))?;
                if part.is_empty() {
                    return Err(format!("empty archive path component in '{raw}'"));
                }
                components.push(part.to_string());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("path traversal in archive entry '{raw}'"));
            }
        }
    }
    if components.is_empty() {
        return Err(format!("archive entry path has no file name: '{raw}'"));
    }
    Ok(components.join("/"))
}

fn is_zip_symlink(entry: &ZipFile<'_>) -> bool {
    entry
        .unix_mode()
        .is_some_and(|mode| mode & 0o170000 == 0o120000)
}

fn zip_le_u16(input: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        input.get(offset..offset.checked_add(2)?)?.try_into().ok()?,
    ))
}

fn zip_le_u32(input: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        input.get(offset..offset.checked_add(4)?)?.try_into().ok()?,
    ))
}

/// Validate the raw central directory before handing the archive to the ZIP
/// crate.  `ZipArchive` intentionally indexes by filename and can collapse
/// duplicate central-directory names, which would otherwise let a hostile
/// archive bypass our duplicate-path check and silently select one payload.
/// ZIP64 and multi-disk archives are rejected explicitly because the bounded
/// 64 MiB input contract does not need either representation.
fn validate_zip_central_directory(input: &[u8], limits: ArchiveLimits) -> Result<usize, String> {
    const EOCD_LEN: usize = 22;
    const EOCD_SIGNATURE: &[u8] = b"PK\x05\x06";
    const CENTRAL_SIGNATURE: &[u8] = b"PK\x01\x02";
    const ZIP64_LOCATOR_SIGNATURE: &[u8] = b"PK\x06\x07";
    const CENTRAL_HEADER_LEN: usize = 46;

    if input.len() < EOCD_LEN {
        return Err("ZIP is missing an end-of-central-directory record".to_string());
    }
    let search_start = input.len().saturating_sub(EOCD_LEN + u16::MAX as usize);
    let eocd_offset = (search_start..=input.len() - EOCD_LEN)
        .rev()
        .find(|offset| {
            input.get(*offset..*offset + EOCD_SIGNATURE.len()) == Some(EOCD_SIGNATURE)
                && zip_le_u16(input, *offset + 20)
                    .and_then(|comment_len| (*offset).checked_add(EOCD_LEN + comment_len as usize))
                    == Some(input.len())
        })
        .ok_or_else(|| "ZIP has an invalid end-of-central-directory record".to_string())?;

    let disk_number = zip_le_u16(input, eocd_offset + 4)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;
    let central_disk = zip_le_u16(input, eocd_offset + 6)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;
    let entries_on_disk = zip_le_u16(input, eocd_offset + 8)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;
    let total_entries = zip_le_u16(input, eocd_offset + 10)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;
    let central_size = zip_le_u32(input, eocd_offset + 12)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;
    let central_offset = zip_le_u32(input, eocd_offset + 16)
        .ok_or_else(|| "ZIP end-of-central-directory record is truncated".to_string())?;

    let has_zip64_locator = eocd_offset >= 20
        && input.get(eocd_offset - 20..eocd_offset - 16) == Some(ZIP64_LOCATOR_SIGNATURE);
    if has_zip64_locator
        || entries_on_disk == u16::MAX
        || total_entries == u16::MAX
        || central_size == u32::MAX
        || central_offset == u32::MAX
    {
        return Err("ZIP64 archives are not supported".to_string());
    }
    if disk_number != 0 || central_disk != 0 || entries_on_disk != total_entries {
        return Err("multi-disk ZIP archives are not supported".to_string());
    }
    let total_entries = total_entries as usize;
    if total_entries > limits.entries {
        return Err(format!(
            "archive contains more than {} entries",
            limits.entries
        ));
    }
    let central_start = central_offset as usize;
    let central_end = central_start
        .checked_add(central_size as usize)
        .ok_or_else(|| "ZIP central directory range overflows".to_string())?;
    if central_end != eocd_offset || central_end > input.len() {
        return Err("ZIP central directory range is invalid".to_string());
    }

    let mut names = HashSet::with_capacity(total_entries);
    let mut cursor = central_start;
    for index in 0..total_entries {
        if cursor.checked_add(CENTRAL_HEADER_LEN).is_none()
            || cursor + CENTRAL_HEADER_LEN > central_end
        {
            return Err(format!("ZIP central directory entry {index} is truncated"));
        }
        if input.get(cursor..cursor + CENTRAL_SIGNATURE.len()) != Some(CENTRAL_SIGNATURE) {
            return Err(format!(
                "ZIP central directory entry {index} has an invalid signature"
            ));
        }
        let compressed_size = zip_le_u32(input, cursor + 20)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?;
        let uncompressed_size = zip_le_u32(input, cursor + 24)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?;
        let name_len = zip_le_u16(input, cursor + 28)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?
            as usize;
        let extra_len = zip_le_u16(input, cursor + 30)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?
            as usize;
        let comment_len = zip_le_u16(input, cursor + 32)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?
            as usize;
        let entry_disk = zip_le_u16(input, cursor + 34)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?;
        let local_header_offset = zip_le_u32(input, cursor + 42)
            .ok_or_else(|| format!("ZIP central directory entry {index} is truncated"))?;
        if compressed_size == u32::MAX
            || uncompressed_size == u32::MAX
            || local_header_offset == u32::MAX
        {
            return Err("ZIP64 entries are not supported".to_string());
        }
        if entry_disk != 0 {
            return Err("multi-disk ZIP entries are not supported".to_string());
        }
        let record_len = CENTRAL_HEADER_LEN
            .checked_add(name_len)
            .and_then(|length| length.checked_add(extra_len))
            .and_then(|length| length.checked_add(comment_len))
            .ok_or_else(|| format!("ZIP central directory entry {index} is too large"))?;
        let record_end = cursor
            .checked_add(record_len)
            .ok_or_else(|| format!("ZIP central directory entry {index} overflows"))?;
        if record_end > central_end {
            return Err(format!(
                "ZIP central directory entry {index} exceeds its declared range"
            ));
        }
        let raw_name = input
            .get(cursor + CENTRAL_HEADER_LEN..cursor + CENTRAL_HEADER_LEN + name_len)
            .ok_or_else(|| format!("ZIP central directory entry {index} has no name"))?;
        let name = std::str::from_utf8(raw_name)
            .map_err(|_| format!("ZIP central directory entry {index} has a non-UTF-8 name"))?;
        let normalized = normalize_zip_path(name, limits.path_bytes)?;
        if !names.insert(normalized.clone()) {
            return Err(format!("duplicate archive entry path '{normalized}'"));
        }
        cursor = record_end;
    }
    if cursor != central_end {
        return Err("ZIP central directory entry count does not match its size".to_string());
    }
    Ok(total_entries)
}

/// Read a ZIP into bounded memory after checking every entry's path, type,
/// expansion ratio, and aggregate expansion.  The worker repeats this check
/// after reading from MinIO so a queued job cannot be bypassed by changing its
/// input object.
pub fn read_archive(input: &[u8], limits: ArchiveLimits) -> Result<Vec<ArchiveEntry>, String> {
    if input.len() > limits.compressed_bytes {
        return Err(format!(
            "compressed archive exceeds {} bytes",
            limits.compressed_bytes
        ));
    }
    let declared_entries = validate_zip_central_directory(input, limits)?;
    let mut archive = ZipArchive::new(Cursor::new(input))
        .map_err(|error| format!("invalid ZIP archive: {error}"))?;
    if archive.len() != declared_entries {
        return Err(format!(
            "ZIP central directory entry count mismatch: declared {declared_entries}, read {}",
            archive.len()
        ));
    }
    if archive.len() > limits.entries {
        return Err(format!(
            "archive contains more than {} entries",
            limits.entries
        ));
    }
    let mut paths = HashSet::with_capacity(archive.len());
    let mut expanded = 0_u64;
    let mut files = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| format!("read ZIP entry {index}: {error}"))?;
        let path = normalize_zip_path(entry.name(), limits.path_bytes)?;
        if !paths.insert(path.clone()) {
            return Err(format!("duplicate archive entry path '{path}'"));
        }
        if is_zip_symlink(&entry) {
            return Err(format!("symlink archive entry is not allowed: '{path}'"));
        }
        let compressed = entry.compressed_size();
        let uncompressed = entry.size();
        if uncompressed > limits.expanded_bytes
            || expanded.saturating_add(uncompressed) > limits.expanded_bytes
        {
            return Err(format!(
                "expanded archive exceeds {} bytes",
                limits.expanded_bytes
            ));
        }
        if uncompressed > 0 {
            if compressed == 0 || uncompressed > compressed.saturating_mul(limits.compression_ratio)
            {
                return Err(format!("archive compression ratio is unsafe for '{path}'"));
            }
        }
        expanded = expanded.saturating_add(uncompressed);
        if entry.is_dir() {
            continue;
        }
        let mut bytes = Vec::with_capacity(uncompressed.min(8 * 1024 * 1024) as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|error| format!("read ZIP entry '{path}': {error}"))?;
        if bytes.len() as u64 != uncompressed {
            return Err(format!("ZIP entry '{path}' changed size while reading"));
        }
        files.push(ArchiveEntry { path, bytes });
    }
    Ok(files)
}

fn entries_by_path(entries: Vec<ArchiveEntry>) -> BTreeMap<String, Vec<u8>> {
    entries
        .into_iter()
        .map(|entry| (entry.path, entry.bytes))
        .collect()
}

fn find_entry<'a>(
    entries: &'a BTreeMap<String, Vec<u8>>,
    path: &str,
) -> Option<(&'a str, &'a [u8])> {
    entries
        .get_key_value(path)
        .map(|(key, value)| (key.as_str(), value.as_slice()))
}

fn find_json_entry<'a>(
    entries: &'a BTreeMap<String, Vec<u8>>,
    preferred: &[&str],
) -> Option<(&'a str, &'a [u8])> {
    for path in preferred {
        if let Some(entry) = find_entry(entries, path) {
            return Some(entry);
        }
    }
    entries
        .iter()
        .find(|(path, bytes)| {
            path.ends_with(".json") && serde_json::from_slice::<Value>(bytes).is_ok()
        })
        .map(|(path, bytes)| (path.as_str(), bytes.as_slice()))
}

fn safe_reference_path(value: &str) -> Result<String, String> {
    normalize_zip_path(value, MAX_ENTRY_PATH_BYTES)
}

fn image_entry<'a>(
    entries: &'a BTreeMap<String, Vec<u8>>,
    reference: &str,
) -> Result<(&'a str, &'a [u8]), String> {
    let reference = safe_reference_path(reference)?;
    let candidates = [
        reference.clone(),
        format!("images/{reference}"),
        format!("assets/{reference}"),
    ];
    for candidate in candidates {
        if let Some(entry) = find_entry(entries, &candidate) {
            return Ok(entry);
        }
    }
    let basename = FsPath::new(&reference)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&reference);
    let matches = entries
        .iter()
        .filter(|(path, _)| {
            path.starts_with("images/") || path.starts_with("assets/") || is_image_path(path)
        })
        .filter(|(path, _)| {
            FsPath::new(path).file_name().and_then(|name| name.to_str()) == Some(basename)
        })
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        Ok((matches[0].0.as_str(), matches[0].1.as_slice()))
    } else if matches.is_empty() {
        Err(format!("image file '{reference}' is missing from archive"))
    } else {
        Err(format!("image reference '{reference}' is ambiguous"))
    }
}

fn is_image_path(path: &str) -> bool {
    matches!(
        FsPath::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.to_ascii_lowercase())
            .as_deref(),
        Some("jpg" | "jpeg" | "png" | "webp" | "bmp" | "tif" | "tiff")
    )
}

fn image_dimensions(bytes: &[u8], file_name: &str) -> Result<(u32, u32), String> {
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| format!("decode image '{file_name}' format: {error}"))?;
    reader
        .into_dimensions()
        .map_err(|error| format!("read image '{file_name}' dimensions: {error}"))
}

fn image_mime(file_name: &str) -> &'static str {
    match FsPath::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("tif" | "tiff") => "image/tiff",
        _ => "image/png",
    }
}

fn number(value: &Value, field: &str) -> Result<f64, String> {
    let value = value
        .as_f64()
        .ok_or_else(|| format!("{field} must be a number"))?;
    if value.is_finite() {
        Ok(value)
    } else {
        Err(format!("{field} must be finite"))
    }
}

fn class_id(value: Option<&Value>, fallback: usize) -> Result<u32, String> {
    match value {
        Some(value) => value
            .as_u64()
            .filter(|value| *value <= u32::MAX as u64)
            .map(|value| value as u32)
            .ok_or_else(|| "classId must be a non-negative integer".to_string()),
        None => Ok(fallback as u32),
    }
}

fn object_type(object: &Value) -> Result<&str, String> {
    object
        .get("type")
        .or_else(|| object.get("objectType"))
        .and_then(Value::as_str)
        .ok_or_else(|| "annotation object is missing type".to_string())
}

fn validate_objects(objects: &[Value], context: &str) -> Result<(), String> {
    let mut ids = HashSet::new();
    for (index, object) in objects.iter().enumerate() {
        let kind = object_type(object)?;
        let id = object
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| format!("{context} annotation {index} is missing id"))?;
        if object
            .get("classId")
            .or_else(|| object.get("class_id"))
            .and_then(Value::as_u64)
            .is_none()
        {
            return Err(format!("{context} annotation {index} is missing classId"));
        }
        if object
            .get("label")
            .and_then(Value::as_str)
            .filter(|label| !label.trim().is_empty())
            .is_none()
        {
            return Err(format!("{context} annotation {index} is missing label"));
        }
        if !object.get("attributes").is_some_and(Value::is_object) {
            return Err(format!(
                "{context} annotation {index} is missing attributes object"
            ));
        }
        let supported = matches!(
            kind,
            "bbox"
                | "polygon"
                | "classification"
                | "oriented_rectangle"
                | "circle"
                | "line"
                | "linestrip"
                | "point"
                | "points"
                | "mask"
        );
        if !supported {
            return Err(format!(
                "{context} annotation {index} has unsupported shape '{kind}'"
            ));
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("{context} contains duplicate annotation id '{id}'"));
        }
        match kind {
            "bbox" => {
                let bbox = object
                    .get("bbox")
                    .and_then(Value::as_object)
                    .ok_or_else(|| format!("{context} bbox is missing geometry"))?;
                let x = number(bbox.get("x").ok_or("bbox x is missing")?, "bbox x")?;
                let y = number(bbox.get("y").ok_or("bbox y is missing")?, "bbox y")?;
                let width = number(
                    bbox.get("width").ok_or("bbox width is missing")?,
                    "bbox width",
                )?;
                let height = number(
                    bbox.get("height").ok_or("bbox height is missing")?,
                    "bbox height",
                )?;
                if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                    return Err(format!("{context} contains an invalid bbox"));
                }
            }
            "polygon" => {
                let points = object
                    .get("polygon")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("{context} polygon is missing points"))?;
                if points.len() < 3 {
                    return Err(format!("{context} polygon needs at least three points"));
                }
                for point in points {
                    let x = number(point.get("x").ok_or("polygon x is missing")?, "polygon x")?;
                    let y = number(point.get("y").ok_or("polygon y is missing")?, "polygon y")?;
                    if x < 0.0 || y < 0.0 {
                        return Err(format!("{context} contains a negative polygon point"));
                    }
                }
            }
            "classification" => {}
            _ => {
                let points = object
                    .get("points")
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("{context} shape '{kind}' is missing points"))?;
                let valid_count = match kind {
                    "oriented_rectangle" => points.len() == 4,
                    "circle" | "line" | "mask" => points.len() == 2,
                    "point" => points.len() == 1,
                    "linestrip" | "points" => points.len() >= 2,
                    _ => false,
                };
                if !valid_count {
                    return Err(format!("{context} shape '{kind}' has too few points"));
                }
                if kind == "mask"
                    && object
                        .get("maskData")
                        .or_else(|| object.get("mask_data"))
                        .and_then(Value::as_str)
                        .is_none_or(|data| data.is_empty())
                {
                    return Err(format!("{context} mask is missing maskData"));
                }
                for point in points {
                    let x = number(point.get("x").ok_or("shape x is missing")?, "shape x")?;
                    let y = number(point.get("y").ok_or("shape y is missing")?, "shape y")?;
                    if x < 0.0 || y < 0.0 {
                        return Err(format!("{context} shape '{kind}' has a negative point"));
                    }
                }
            }
        }
    }
    Ok(())
}

fn normalize_native_object(mut object: Value, index: usize) -> Result<Value, String> {
    let object_map = object
        .as_object_mut()
        .ok_or_else(|| format!("native annotation {index} must be an object"))?;
    if !object_map.contains_key("id") {
        object_map.insert("id".to_string(), json!(format!("annotation-{index}")));
    }
    if !object_map.contains_key("attributes") {
        object_map.insert("attributes".to_string(), json!({}));
    }
    if let Some(class_id) = object_map.remove("class_id") {
        object_map.entry("classId").or_insert(class_id);
    }
    if let Some(object_type) = object_map.remove("object_type") {
        object_map.entry("type").or_insert(object_type);
    }
    if let Some(mask_data) = object_map.remove("mask_data") {
        object_map.entry("maskData").or_insert(mask_data);
    }
    Ok(Value::Object(object_map.clone()))
}

fn labels_from_objects(objects: &[Value], labels: &mut BTreeMap<u32, String>) {
    for object in objects {
        if let Some(id) = object
            .get("classId")
            .or_else(|| object.get("class_id"))
            .and_then(Value::as_u64)
        {
            if let Some(label) = object.get("label").and_then(Value::as_str) {
                labels.entry(id as u32).or_insert_with(|| label.to_string());
            }
        }
    }
}

fn classes_from_labels(labels: &BTreeMap<u32, String>) -> Vec<String> {
    let Some(max_class) = labels.keys().max().copied() else {
        return Vec::new();
    };
    (0..=max_class)
        .map(|class| {
            labels
                .get(&class)
                .cloned()
                .unwrap_or_else(|| format!("class_{class}"))
        })
        .collect()
}

fn imported_image(
    key: String,
    file_name: String,
    split: String,
    width: Option<u32>,
    height: Option<u32>,
    bytes: &[u8],
) -> Result<ImportedImage, String> {
    if bytes.len() > MAX_COMPRESSED_BYTES {
        return Err(format!(
            "image '{file_name}' exceeds {} encoded bytes",
            MAX_COMPRESSED_BYTES
        ));
    }
    let (detected_width, detected_height) = image_dimensions(bytes, &file_name)?;
    let width = width.unwrap_or(detected_width);
    let height = height.unwrap_or(detected_height);
    if width == 0
        || height == 0
        || width != detected_width
        || height != detected_height
        || (width as u64).saturating_mul(height as u64) > MAX_IMAGE_PIXELS
        || (width as u64)
            .saturating_mul(height as u64)
            .saturating_mul(4)
            > MAX_IMAGE_DECODE_BYTES
    {
        return Err(format!(
            "image '{file_name}' dimensions are invalid or exceed the pixel limit"
        ));
    }
    let mime_type = image_mime(&file_name).to_string();
    Ok(ImportedImage {
        key,
        file_name,
        width,
        height,
        split,
        mime_type,
        content_hash: hex::encode(Sha256::digest(bytes)),
        bytes: bytes.to_vec(),
    })
}

/// Parse the supported dataset formats into the common server representation.
/// This function is pure and is also used by the worker after it downloads the
/// queued object, which means malformed archives never create partial rows.
pub fn parse_import_archive(
    input: &[u8],
    format: DatasetFormat,
    limits: ArchiveLimits,
) -> Result<ImportBundle, String> {
    let entries = entries_by_path(read_archive(input, limits)?);
    let bundle = match format {
        DatasetFormat::Native => parse_native(entries),
        DatasetFormat::Coco => parse_coco(entries),
        DatasetFormat::Yolo => parse_yolo(entries, false),
        DatasetFormat::YoloSeg => parse_yolo(entries, true),
        DatasetFormat::LabelMe => parse_labelme(entries),
    }?;
    if bundle.images.is_empty() || bundle.annotations.len() != bundle.images.len() {
        return Err("dataset archive contains no complete image records".to_string());
    }
    Ok(bundle)
}

fn parse_native(entries: BTreeMap<String, Vec<u8>>) -> Result<ImportBundle, String> {
    let (_, manifest_bytes) = find_json_entry(&entries, &["manifest.json"])
        .ok_or_else(|| "native archive is missing manifest.json".to_string())?;
    let manifest: Value = serde_json::from_slice(manifest_bytes)
        .map_err(|error| format!("parse native manifest: {error}"))?;
    let rows = manifest
        .get("annotations")
        .and_then(Value::as_array)
        .ok_or_else(|| "native manifest is missing annotations".to_string())?;
    if rows.is_empty() {
        return Err("native manifest contains no images".to_string());
    }
    let mut labels = BTreeMap::new();
    for (index, class) in manifest
        .get("classes")
        .or_else(|| {
            manifest
                .get("schema")
                .and_then(|schema| schema.get("classes"))
        })
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let label = class
            .as_str()
            .or_else(|| class.get("name").and_then(Value::as_str))
            .or_else(|| class.get("label").and_then(Value::as_str))
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| format!("native class {index} is invalid"))?;
        labels.insert(index as u32, label.to_string());
    }
    let mut images = Vec::with_capacity(rows.len());
    let mut annotations = Vec::with_capacity(rows.len());
    let mut keys = HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let file_name = row
            .get("fileName")
            .or_else(|| row.get("file_name"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("native image {index} is missing fileName"))?;
        let (entry_path, bytes) = image_entry(&entries, file_name)?;
        let key = row
            .get("imageId")
            .or_else(|| row.get("image_id"))
            .or_else(|| row.get("id"))
            .and_then(Value::as_str)
            .unwrap_or(entry_path)
            .to_string();
        if !keys.insert(key.clone()) {
            return Err(format!("native manifest contains duplicate image '{key}'"));
        }
        let objects = row
            .get("objects")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("native image '{key}' is missing objects"))?
            .iter()
            .enumerate()
            .map(|(object_index, object)| normalize_native_object(object.clone(), object_index))
            .collect::<Result<Vec<_>, _>>()?;
        validate_objects(&objects, &format!("native image '{key}'"))?;
        labels_from_objects(&objects, &mut labels);
        let width = row
            .get("width")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let height = row
            .get("height")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let file_name = file_name
            .strip_prefix("images/")
            .unwrap_or(file_name)
            .to_string();
        images.push(imported_image(
            key.clone(),
            file_name,
            row.get("split")
                .and_then(Value::as_str)
                .unwrap_or("train")
                .to_string(),
            width,
            height,
            bytes,
        )?);
        annotations.push(ImportedAnnotation {
            image_key: key,
            objects,
        });
    }
    Ok(ImportBundle {
        format: DatasetFormat::Native,
        classes: classes_from_labels(&labels),
        images,
        annotations,
        manifest,
    })
}

fn parse_coco(entries: BTreeMap<String, Vec<u8>>) -> Result<ImportBundle, String> {
    let (_, annotation_bytes) = find_json_entry(&entries, &["annotations.json", "coco.json"])
        .ok_or_else(|| "COCO archive is missing annotations.json".to_string())?;
    let root: Value = serde_json::from_slice(annotation_bytes)
        .map_err(|error| format!("parse COCO annotations: {error}"))?;
    let images_json = root
        .get("images")
        .and_then(Value::as_array)
        .ok_or_else(|| "COCO annotations.images must be an array".to_string())?;
    let categories_json = root
        .get("categories")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut category_ids = BTreeMap::new();
    let mut classes = Vec::new();
    for (index, category) in categories_json.iter().enumerate() {
        let id = category
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("COCO category {index} is missing integer id"))?;
        if id < 0 {
            return Err(format!("COCO category {id} is negative"));
        }
        let label = category
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .ok_or_else(|| format!("COCO category {id} is missing name"))?
            .to_string();
        let class_id = index as u32;
        if category_ids.insert(id, (class_id, label.clone())).is_some() {
            return Err(format!("COCO contains duplicate category id {id}"));
        }
        classes.push(label);
    }
    let mut images = Vec::with_capacity(images_json.len());
    let mut image_lookup = HashMap::new();
    let mut seen_keys = HashSet::new();
    for (index, image) in images_json.iter().enumerate() {
        let source_id = image
            .get("id")
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|id| id.to_string())
                    .or_else(|| value.as_str().map(str::to_string))
            })
            .ok_or_else(|| format!("COCO image {index} is missing id"))?;
        let file_name = image
            .get("file_name")
            .or_else(|| image.get("fileName"))
            .and_then(Value::as_str)
            .ok_or_else(|| format!("COCO image '{source_id}' is missing file_name"))?;
        let (entry_path, bytes) = image_entry(&entries, file_name)?;
        let key = source_id.clone();
        if !seen_keys.insert(key.clone()) {
            return Err(format!("COCO contains duplicate image id '{key}'"));
        }
        let width = image
            .get("width")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let height = image
            .get("height")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let imported = imported_image(
            key.clone(),
            file_name.to_string(),
            image
                .get("split")
                .and_then(Value::as_str)
                .unwrap_or("train")
                .to_string(),
            width,
            height,
            bytes,
        )?;
        image_lookup.insert(source_id, key.clone());
        // Keep the normalized ZIP path in the manifest so re-export does not
        // rely on a potentially ambiguous basename.
        let mut imported = imported;
        imported.file_name = entry_path
            .strip_prefix("images/")
            .unwrap_or(entry_path)
            .to_string();
        images.push(imported);
    }
    let mut annotation_map: HashMap<String, Vec<Value>> = HashMap::new();
    for (index, annotation) in root
        .get("annotations")
        .and_then(Value::as_array)
        .ok_or_else(|| "COCO annotations.annotations must be an array".to_string())?
        .iter()
        .enumerate()
    {
        let image_id = annotation
            .get("image_id")
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|id| id.to_string())
                    .or_else(|| value.as_str().map(str::to_string))
            })
            .ok_or_else(|| format!("COCO annotation {index} is missing image_id"))?;
        let image_key = image_lookup
            .get(&image_id)
            .cloned()
            .ok_or_else(|| format!("COCO annotation references unknown image '{image_id}'"))?;
        let category_id = annotation
            .get("category_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("COCO annotation {index} is missing category_id"))?;
        let (class_id, label) = category_ids.get(&category_id).cloned().ok_or_else(|| {
            format!("COCO annotation {index} references unknown category id {category_id}")
        })?;
        let object_id = annotation
            .get("source_id")
            .or_else(|| annotation.get("id"))
            .and_then(|value| {
                value
                    .as_i64()
                    .map(|id| id.to_string())
                    .or_else(|| value.as_str().map(str::to_string))
            })
            .unwrap_or_else(|| format!("coco-{index}"));
        let mut object = json!({
            "id": object_id,
            "classId": class_id,
            "label": label,
            "attributes": annotation.get("attributes").cloned().unwrap_or_else(|| json!({}))
        });
        let segmentation = annotation.get("segmentation");
        let has_polygon_segmentation = match segmentation {
            Some(Value::Array(items)) if !items.is_empty() => true,
            Some(Value::Array(_)) | None => false,
            Some(_) => {
                return Err(format!(
                    "COCO annotation {index} uses RLE segmentation, which is unsupported"
                ));
            }
        };
        if has_polygon_segmentation {
            let segmentation = segmentation.expect("checked above");
            let polygon = coco_polygon(segmentation, &format!("COCO annotation {index}"))?;
            object["type"] = json!("polygon");
            object["polygon"] = json!(polygon);
        } else {
            let bbox = annotation
                .get("bbox")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("COCO annotation {index} has no bbox or segmentation"))?;
            if bbox.len() != 4 {
                return Err(format!(
                    "COCO annotation {index} bbox must have four values"
                ));
            }
            let x = number(&bbox[0], "COCO bbox x")?;
            let y = number(&bbox[1], "COCO bbox y")?;
            let width = number(&bbox[2], "COCO bbox width")?;
            let height = number(&bbox[3], "COCO bbox height")?;
            if x < 0.0 || y < 0.0 || width <= 0.0 || height <= 0.0 {
                return Err(format!("COCO annotation {index} bbox is invalid"));
            }
            object["type"] = json!("bbox");
            object["bbox"] = json!({"x": x, "y": y, "width": width, "height": height});
        }
        let image_objects = annotation_map.entry(image_key).or_default();
        image_objects.push(object);
    }
    // `build_export_archive` uses this explicit extension for image-level
    // classification because standard COCO annotations are object-oriented.
    // Read it back instead of silently dropping classification objects during
    // a server export/import round trip.
    if let Some(image_categories) = root.get("image_categories") {
        let image_categories = image_categories
            .as_array()
            .ok_or_else(|| "COCO image_categories must be an array when present".to_string())?;
        for (index, image_category) in image_categories.iter().enumerate() {
            let image_id = image_category
                .get("image_id")
                .and_then(|value| {
                    value
                        .as_i64()
                        .map(|id| id.to_string())
                        .or_else(|| value.as_str().map(str::to_string))
                })
                .ok_or_else(|| format!("COCO image category {index} is missing image_id"))?;
            let image_key = image_lookup.get(&image_id).cloned().ok_or_else(|| {
                format!("COCO image category references unknown image '{image_id}'")
            })?;
            let category_id = image_category
                .get("category_id")
                .and_then(Value::as_i64)
                .ok_or_else(|| format!("COCO image category {index} is missing category_id"))?;
            let (class_id, label) = category_ids.get(&category_id).cloned().ok_or_else(|| {
                format!("COCO image category {index} references unknown category id {category_id}")
            })?;
            let object_id = image_category
                .get("source_id")
                .or_else(|| image_category.get("id"))
                .and_then(|value| {
                    value
                        .as_i64()
                        .map(|id| id.to_string())
                        .or_else(|| value.as_str().map(str::to_string))
                })
                .unwrap_or_else(|| format!("coco-image-category-{index}"));
            annotation_map.entry(image_key).or_default().push(json!({
                "id": object_id,
                "classId": class_id,
                "label": label,
                "type": "classification",
                "attributes": image_category
                    .get("attributes")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
            }));
        }
    }
    let mut annotations = Vec::new();
    for image in &images {
        let objects = annotation_map.remove(&image.key).unwrap_or_default();
        validate_objects(&objects, &format!("COCO image '{}'", image.key))?;
        annotations.push(ImportedAnnotation {
            image_key: image.key.clone(),
            objects,
        });
    }
    let manifest = json!({
        "format": "coco",
        "annotations": annotations.iter().zip(images.iter()).map(|(annotation, image)| json!({
            "imageId": annotation.image_key.clone(),
            "fileName": image.file_name.clone(),
            "width": image.width,
            "height": image.height,
            "split": image.split.clone(),
            "objects": annotation.objects.clone(),
        })).collect::<Vec<_>>()
    });
    Ok(ImportBundle {
        format: DatasetFormat::Coco,
        classes,
        images,
        annotations,
        manifest,
    })
}

fn coco_polygon(value: &Value, context: &str) -> Result<Vec<Value>, String> {
    let polygons = value.as_array().ok_or_else(|| {
        format!("{context} segmentation must be a polygon array; RLE is unsupported")
    })?;
    if polygons.len() != 1 {
        return Err(format!(
            "{context} contains multiple polygon segments, which are unsupported"
        ));
    }
    let first = polygons
        .first()
        .ok_or_else(|| format!("{context} segmentation is empty"))?;
    let values = first
        .as_array()
        .ok_or_else(|| format!("{context} segmentation must contain coordinate arrays"))?;
    if values.len() < 6 || values.len() % 2 != 0 {
        return Err(format!(
            "{context} segmentation polygon needs at least three points"
        ));
    }
    let mut points = Vec::with_capacity(values.len() / 2);
    for pair in values.chunks_exact(2) {
        let x = number(&pair[0], "COCO polygon x")?;
        let y = number(&pair[1], "COCO polygon y")?;
        if x < 0.0 || y < 0.0 {
            return Err(format!("{context} segmentation contains a negative point"));
        }
        points.push(json!({"x": x, "y": y}));
    }
    Ok(points)
}

fn parse_yolo(
    entries: BTreeMap<String, Vec<u8>>,
    segmentation: bool,
) -> Result<ImportBundle, String> {
    let mut classes = Vec::new();
    for path in [
        "classes.txt",
        "obj.names",
        "images/classes.txt",
        "labels/classes.txt",
    ] {
        if let Some((_, bytes)) = find_entry(&entries, path) {
            classes = String::from_utf8(bytes.to_vec())
                .map_err(|error| format!("parse YOLO classes: {error}"))?
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_string)
                .collect();
            break;
        }
    }
    let image_paths = entries
        .keys()
        .filter(|path| is_image_path(path))
        .cloned()
        .collect::<Vec<_>>();
    if image_paths.is_empty() {
        return Err("YOLO archive contains no images".to_string());
    }
    let mut images = Vec::with_capacity(image_paths.len());
    let mut annotations = Vec::with_capacity(image_paths.len());
    let mut labels = classes
        .iter()
        .enumerate()
        .map(|(class, label)| (class as u32, label.clone()))
        .collect::<BTreeMap<_, _>>();
    // A YOLO archive may intentionally contain no label files when every
    // image is unannotated.  Once one image has a matching label file,
    // however, a missing sibling is ambiguous (and commonly indicates a
    // malformed archive), so reject it instead of silently turning it into
    // an empty annotation list.  An explicit empty .txt file remains the
    // supported representation for an unannotated image in a mixed archive.
    let image_label_paths = image_paths
        .iter()
        .map(|path| (path.clone(), yolo_label_path(path, &entries)))
        .collect::<Vec<_>>();
    let has_label_file = image_label_paths
        .iter()
        .any(|(_, label_path)| label_path.is_some());
    if has_label_file {
        if let Some((path, None)) = image_label_paths
            .iter()
            .find(|(_, label_path)| label_path.is_none())
        {
            return Err(format!(
                "YOLO image '{path}' has no matching label file; use an empty .txt file to mark it unannotated"
            ));
        }
    }
    for (path, label_path) in image_label_paths {
        let bytes = entries
            .get(&path)
            .ok_or_else(|| format!("missing image bytes for '{path}'"))?;
        let key = path.clone();
        let file_name = path.strip_prefix("images/").unwrap_or(&path).to_string();
        let image = imported_image(key.clone(), file_name, yolo_split(&path), None, None, bytes)?;
        let mut objects = Vec::new();
        if let Some(label_path) = label_path {
            let text = String::from_utf8(
                entries
                    .get(&label_path)
                    .cloned()
                    .ok_or_else(|| format!("missing YOLO label '{label_path}'"))?,
            )
            .map_err(|error| format!("parse YOLO label '{label_path}': {error}"))?;
            for (index, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    continue;
                }
                let values = line.split_whitespace().collect::<Vec<_>>();
                if (segmentation && (values.len() < 7 || values.len() % 2 == 0))
                    || (!segmentation && values.len() != 5)
                {
                    return Err(format!(
                        "YOLO label '{label_path}' line {} has unsupported shape",
                        index + 1
                    ));
                }
                let class = parse_u32(values[0], "YOLO class id")?;
                let label = classes
                    .get(class as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("class_{class}"));
                labels.entry(class).or_insert_with(|| label.clone());
                let id = format!("{}-{}", key, index);
                let object = if segmentation {
                    let mut points = Vec::with_capacity((values.len() - 1) / 2);
                    for pair in values[1..].chunks_exact(2) {
                        let x = normalized_value(pair[0], "YOLO x")?;
                        let y = normalized_value(pair[1], "YOLO y")?;
                        points.push(
                            json!({"x": x * image.width as f64, "y": y * image.height as f64}),
                        );
                    }
                    json!({"id": id, "classId": class, "label": label, "type": "polygon", "polygon": points, "attributes": {}})
                } else {
                    let center_x = normalized_value(values[1], "YOLO center x")?;
                    let center_y = normalized_value(values[2], "YOLO center y")?;
                    let width = normalized_value(values[3], "YOLO width")?;
                    let height = normalized_value(values[4], "YOLO height")?;
                    if width <= 0.0
                        || height <= 0.0
                        || center_x - width / 2.0 < 0.0
                        || center_y - height / 2.0 < 0.0
                        || center_x + width / 2.0 > 1.0
                        || center_y + height / 2.0 > 1.0
                    {
                        return Err(format!(
                            "YOLO label '{label_path}' line {} has an out-of-bounds bbox",
                            index + 1
                        ));
                    }
                    json!({
                        "id": id,
                        "classId": class,
                        "label": label,
                        "type": "bbox",
                        "bbox": {"x": (center_x - width / 2.0) * image.width as f64, "y": (center_y - height / 2.0) * image.height as f64, "width": width * image.width as f64, "height": height * image.height as f64},
                        "attributes": {}
                    })
                };
                objects.push(object);
            }
        }
        validate_objects(&objects, &format!("YOLO image '{path}'"))?;
        images.push(image);
        annotations.push(ImportedAnnotation {
            image_key: key,
            objects,
        });
    }
    classes = classes_from_labels(&labels);
    let manifest = json!({
        "format": if segmentation { "yolo-seg" } else { "yolo" },
        "annotationFilesPresent": has_label_file,
        "annotations": annotations.iter().zip(images.iter()).map(|(annotation, image)| json!({
            "imageId": annotation.image_key.clone(),
            "fileName": image.file_name.clone(),
            "width": image.width,
            "height": image.height,
            "split": image.split.clone(),
            "objects": annotation.objects.clone(),
        })).collect::<Vec<_>>()
    });
    Ok(ImportBundle {
        format: if segmentation {
            DatasetFormat::YoloSeg
        } else {
            DatasetFormat::Yolo
        },
        classes,
        images,
        annotations,
        manifest,
    })
}

fn yolo_split(path: &str) -> String {
    if path.contains("/val/") || path.starts_with("val/") {
        "val".to_string()
    } else if path.contains("/test/") || path.starts_with("test/") {
        "test".to_string()
    } else {
        "train".to_string()
    }
}

fn yolo_label_path(path: &str, entries: &BTreeMap<String, Vec<u8>>) -> Option<String> {
    let stem = FsPath::new(path).file_stem()?.to_str()?;
    let mut candidates = Vec::new();
    if let Some(relative) = path.strip_prefix("images/") {
        candidates.push(format!(
            "labels/{}",
            FsPath::new(relative)
                .with_extension("txt")
                .to_string_lossy()
        ));
    }
    if let Some((prefix, relative)) = path.split_once("/images/") {
        candidates.push(format!(
            "{prefix}/labels/{}",
            FsPath::new(relative)
                .with_extension("txt")
                .to_string_lossy()
        ));
    }
    candidates.push(
        FsPath::new(path)
            .with_extension("txt")
            .to_string_lossy()
            .to_string(),
    );
    candidates.push(format!("labels/{stem}.txt"));
    candidates
        .into_iter()
        .find(|candidate| entries.contains_key(candidate))
}

fn parse_u32(value: &str, field: &str) -> Result<u32, String> {
    value
        .parse::<u32>()
        .map_err(|error| format!("{field} '{value}' is invalid: {error}"))
}

fn normalized_value(value: &str, field: &str) -> Result<f64, String> {
    let value = value
        .parse::<f64>()
        .map_err(|error| format!("{field} '{value}' is invalid: {error}"))?;
    if !value.is_finite() || !(0.0..=1.0).contains(&value) {
        return Err(format!("{field} must be within [0,1]"));
    }
    Ok(value)
}

fn parse_labelme(entries: BTreeMap<String, Vec<u8>>) -> Result<ImportBundle, String> {
    let json_paths = entries
        .keys()
        .filter(|path| path.ends_with(".json") && path != &"manifest.json")
        .cloned()
        .collect::<Vec<_>>();
    if json_paths.is_empty() {
        return Err("LabelMe archive contains no annotation JSON files".to_string());
    }
    let mut images = Vec::new();
    let mut annotations = Vec::new();
    let mut labels = BTreeMap::new();
    let mut image_seen = HashSet::new();
    for (json_index, json_path) in json_paths.iter().enumerate() {
        let root: Value = serde_json::from_slice(
            entries
                .get(json_path)
                .ok_or_else(|| format!("missing LabelMe file '{json_path}'"))?,
        )
        .map_err(|error| format!("parse LabelMe file '{json_path}': {error}"))?;
        let shapes = root
            .get("shapes")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("LabelMe file '{json_path}' is missing shapes"))?;
        let reference = root
            .get("imagePath")
            .and_then(Value::as_str)
            .or_else(|| root.get("image_path").and_then(Value::as_str))
            .ok_or_else(|| format!("LabelMe file '{json_path}' is missing imagePath"))?;
        let (entry_path, bytes) = image_entry(&entries, reference)?;
        let key = entry_path.to_string();
        if !image_seen.insert(key.clone()) {
            return Err(format!("LabelMe image '{key}' is annotated more than once"));
        }
        let width = root
            .get("imageWidth")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let height = root
            .get("imageHeight")
            .and_then(Value::as_u64)
            .map(|value| value as u32);
        let file_name = entry_path
            .strip_prefix("images/")
            .unwrap_or(entry_path)
            .to_string();
        let image = imported_image(
            key.clone(),
            file_name,
            "train".to_string(),
            width,
            height,
            bytes,
        )?;
        let mut objects = Vec::with_capacity(shapes.len());
        for (shape_index, shape) in shapes.iter().enumerate() {
            let label = shape
                .get("label")
                .and_then(Value::as_str)
                .filter(|label| !label.trim().is_empty())
                .ok_or_else(|| format!("LabelMe shape {shape_index} has no label"))?
                .to_string();
            let class = labels
                .iter()
                .find_map(|(class, value)| (value == &label).then_some(*class))
                .unwrap_or(labels.len() as u32);
            labels.entry(class).or_insert_with(|| label.clone());
            let points = shape
                .get("points")
                .and_then(Value::as_array)
                .ok_or_else(|| format!("LabelMe shape {shape_index} has no points"))?;
            let points = points
                .iter()
                .map(|point| {
                    let pair = point
                        .as_array()
                        .ok_or_else(|| format!("LabelMe shape {shape_index} point is invalid"))?;
                    if pair.len() != 2 {
                        return Err(format!(
                            "LabelMe shape {shape_index} point needs two values"
                        ));
                    }
                    let x = number(&pair[0], "LabelMe x")?;
                    let y = number(&pair[1], "LabelMe y")?;
                    if x < 0.0 || y < 0.0 {
                        return Err(format!(
                            "LabelMe shape {shape_index} contains a negative point"
                        ));
                    }
                    Ok((x, y))
                })
                .collect::<Result<Vec<_>, String>>()?;
            let shape_type = shape
                .get("shape_type")
                .and_then(Value::as_str)
                .unwrap_or("polygon");
            let object_id = shape
                .get("imageAnnotationId")
                .or_else(|| shape.get("id"))
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("labelme-{json_index}-{shape_index}"));
            let object = match shape_type {
                "rectangle" => {
                    if points.len() != 2 {
                        return Err(format!(
                            "LabelMe shape {shape_index} rectangle needs two points"
                        ));
                    }
                    let (x1, y1) = points[0];
                    let (x2, y2) = points[1];
                    json!({"id": object_id, "classId": class, "label": label, "type": "bbox", "bbox": {"x": x1.min(x2), "y": y1.min(y2), "width": (x1-x2).abs(), "height": (y1-y2).abs()}, "attributes": {}})
                }
                "polygon" => {
                    json!({"id": object_id, "classId": class, "label": label, "type": "polygon", "polygon": points.into_iter().map(|(x,y)| json!({"x":x,"y":y})).collect::<Vec<_>>(), "attributes": {}})
                }
                other => {
                    return Err(format!(
                        "LabelMe shape {shape_index} has unsupported type '{other}'"
                    ))
                }
            };
            objects.push(object);
        }
        validate_objects(&objects, &format!("LabelMe image '{key}'"))?;
        images.push(image);
        annotations.push(ImportedAnnotation {
            image_key: key,
            objects,
        });
        if json_index > MAX_ENTRIES {
            return Err("too many LabelMe files".to_string());
        }
    }
    let manifest = json!({
        "format": "labelme",
        "annotations": annotations.iter().zip(images.iter()).map(|(annotation, image)| json!({
            "imageId": annotation.image_key.clone(),
            "fileName": image.file_name.clone(),
            "width": image.width,
            "height": image.height,
            "split": image.split.clone(),
            "objects": annotation.objects.clone(),
        })).collect::<Vec<_>>()
    });
    Ok(ImportBundle {
        format: DatasetFormat::LabelMe,
        classes: labels.values().cloned().collect(),
        images,
        annotations,
        manifest,
    })
}

fn export_item_prefix(row: &Value, index: usize) -> String {
    for field in ["itemId", "id", "imageId", "clientKey", "objectKey"] {
        let Some(value) = row.get(field).and_then(Value::as_str) else {
            continue;
        };
        let prefix = value
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                    character
                } else {
                    '_'
                }
            })
            .take(80)
            .collect::<String>();
        if !prefix.is_empty() {
            return prefix;
        }
    }
    format!("item-{index}")
}

/// Give colliding source names deterministic package names while preserving
/// the original display name in each format's metadata.  The immutable item
/// identity is preferred over the row index, so the same version exports the
/// same names on every retry.
fn export_file_names(
    rows: &[Value],
    original_file_names: &[String],
) -> Result<Vec<String>, String> {
    if rows.len() != original_file_names.len() {
        return Err("dataset export image metadata is inconsistent".to_string());
    }
    let mut counts = HashMap::<String, usize>::new();
    for file_name in original_file_names {
        *counts.entry(file_name.clone()).or_default() += 1;
    }
    let mut used = HashSet::with_capacity(rows.len());
    let mut archive_file_names = Vec::with_capacity(rows.len());
    for (index, (row, original_file_name)) in
        rows.iter().zip(original_file_names.iter()).enumerate()
    {
        let prefix = export_item_prefix(row, index);
        let mut candidate = if counts.get(original_file_name).copied().unwrap_or(0) > 1 {
            format!("{prefix}-{original_file_name}")
        } else {
            original_file_name.clone()
        };
        if !used.insert(candidate.clone()) {
            let mut suffix = 1_u32;
            loop {
                let next = format!("{prefix}-{suffix}-{original_file_name}");
                if used.insert(next.clone()) {
                    candidate = next;
                    break;
                }
                suffix = suffix.saturating_add(1);
                if suffix == u32::MAX {
                    return Err(format!(
                        "cannot create a unique export filename for '{original_file_name}'"
                    ));
                }
            }
        }
        // Prefixing and the original path are both validated independently;
        // run the combined name through the archive path contract as well.
        archive_file_names.push(safe_reference_path(&candidate)?);
    }
    Ok(archive_file_names)
}

/// Build a deterministic ZIP from a frozen dataset-version manifest.
/// `image_bytes` is preferably keyed by the immutable object key or item id
/// (`objectKey`, `itemId`, `id`, `imageId`, or `clientKey`); the original
/// `fileName` remains a compatibility fallback for callers of this pure helper.
/// No shape is silently discarded:
/// a format that cannot represent an object fails the whole job.
pub fn build_export_archive(
    format: DatasetFormat,
    manifest: &Value,
    image_bytes: &BTreeMap<String, Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let rows = manifest
        .get("annotations")
        .and_then(Value::as_array)
        .ok_or_else(|| "dataset export manifest is missing annotations".to_string())?;
    if rows.is_empty() {
        return Err("dataset export manifest contains no images".to_string());
    }
    let original_file_names = rows
        .iter()
        .map(version_file_name)
        .collect::<Result<Vec<_>, _>>()?;
    let archive_file_names = export_file_names(rows, &original_file_names)?;
    let mut output = LimitedCursor::new(MAX_COMPRESSED_BYTES);
    let mut writer = ZipWriter::new(&mut output);
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut output_paths = HashSet::new();
    let mut categories = BTreeMap::<u32, String>::new();
    for ((row, file_name), archive_file_name) in rows
        .iter()
        .zip(original_file_names.iter())
        .zip(archive_file_names.iter())
    {
        let objects = version_objects(row)?;
        for object in &objects {
            let class = object_class_id(object)?;
            let label = object_label(object)?;
            categories.entry(class).or_insert(label);
        }
        let bytes = lookup_export_image(image_bytes, row, file_name)?;
        let output_path = format!("images/{archive_file_name}");
        add_zip_entry(&mut writer, &mut output_paths, &output_path, bytes, options)?;
    }

    match format {
        DatasetFormat::Native => {
            let mut native_manifest = manifest.clone();
            let native_rows = native_manifest
                .get_mut("annotations")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "dataset export manifest is missing annotations".to_string())?;
            for ((row, original_file_name), archive_file_name) in native_rows
                .iter_mut()
                .zip(original_file_names.iter())
                .zip(archive_file_names.iter())
            {
                let object = row
                    .as_object_mut()
                    .ok_or_else(|| "dataset export annotation row must be an object".to_string())?;
                object.insert(
                    "fileName".to_string(),
                    Value::String(archive_file_name.clone()),
                );
                if original_file_name != archive_file_name {
                    object.insert(
                        "displayFileName".to_string(),
                        Value::String(original_file_name.clone()),
                    );
                }
            }
            let manifest_bytes = serde_json::to_vec_pretty(&native_manifest)
                .map_err(|error| format!("serialize native manifest: {error}"))?;
            add_zip_entry(
                &mut writer,
                &mut output_paths,
                "manifest.json",
                &manifest_bytes,
                options,
            )?;
        }
        DatasetFormat::Coco => {
            let mut coco_images = Vec::with_capacity(rows.len());
            let mut coco_annotations = Vec::new();
            let mut image_categories = Vec::new();
            let mut annotation_id = 1_u64;
            for (image_index, row) in rows.iter().enumerate() {
                let file_name = &original_file_names[image_index];
                let archive_file_name = &archive_file_names[image_index];
                let width = row.get("width").and_then(Value::as_u64).ok_or_else(|| {
                    format!("dataset version image '{file_name}' is missing width")
                })?;
                let height = row.get("height").and_then(Value::as_u64).ok_or_else(|| {
                    format!("dataset version image '{file_name}' is missing height")
                })?;
                if width == 0 || height == 0 {
                    return Err(format!(
                        "dataset version image '{file_name}' has invalid dimensions"
                    ));
                }
                let image_id = (image_index + 1) as u64;
                coco_images.push(json!({
                    "id": image_id,
                    "file_name": archive_file_name,
                    "original_file_name": file_name,
                    "width": width,
                    "height": height,
                    "split": row.get("split").and_then(Value::as_str).unwrap_or("train"),
                    "source_id": row.get("imageId").cloned().unwrap_or(Value::Null),
                }));
                for object in version_objects(row)? {
                    let class = object_class_id(&object)?;
                    let object_id = object
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| "annotation")
                        .to_string();
                    match object_type(&object)? {
                        "classification" => {
                            image_categories.push(json!({
                                "image_id": image_id,
                                "category_id": class as u64 + 1,
                                "source_id": object.get("id").cloned().unwrap_or(Value::Null),
                                "attributes": object
                                    .get("attributes")
                                    .cloned()
                                    .unwrap_or_else(|| json!({})),
                            }));
                        }
                        "bbox" => {
                            let bbox = object_bbox(&object)?;
                            let area = bbox[2] * bbox[3];
                            coco_annotations.push(json!({
                                "id": annotation_id,
                                "image_id": image_id,
                                "category_id": class as u64 + 1,
                                "bbox": bbox,
                                "segmentation": [],
                                "area": area,
                                "iscrowd": 0,
                                "source_id": object_id,
                            }));
                            annotation_id += 1;
                        }
                        "polygon" => {
                            let polygon = object_polygon(&object)?;
                            let segmentation = polygon
                                .iter()
                                .flat_map(|point| {
                                    [
                                        point.get("x").cloned().unwrap_or(Value::Null),
                                        point.get("y").cloned().unwrap_or(Value::Null),
                                    ]
                                })
                                .collect::<Vec<_>>();
                            let bbox = polygon_bbox(&polygon)?;
                            let area = polygon_area(&polygon);
                            coco_annotations.push(json!({
                                "id": annotation_id,
                                "image_id": image_id,
                                "category_id": class as u64 + 1,
                                "bbox": bbox,
                                "segmentation": [segmentation],
                                "area": area,
                                "iscrowd": 0,
                                "source_id": object_id,
                            }));
                            annotation_id += 1;
                        }
                        other => {
                            return Err(format!(
                                "COCO cannot represent annotation '{object_id}' of type '{other}'"
                            ));
                        }
                    }
                }
            }
            let categories = categories
                .iter()
                .map(|(class, label)| json!({"id": *class as u64 + 1, "name": label, "supercategory": ""}))
                .collect::<Vec<_>>();
            let coco = json!({
                "info": {"description": manifest.get("name").and_then(Value::as_str).unwrap_or("Image Annotation"), "version": "1.0"},
                "licenses": [],
                "images": coco_images,
                "categories": categories,
                "annotations": coco_annotations,
                "image_categories": image_categories,
            });
            let bytes = serde_json::to_vec_pretty(&coco)
                .map_err(|error| format!("serialize COCO annotations: {error}"))?;
            add_zip_entry(
                &mut writer,
                &mut output_paths,
                "annotations.json",
                &bytes,
                options,
            )?;
        }
        DatasetFormat::Yolo | DatasetFormat::YoloSeg => {
            let class_names = categories
                .keys()
                .max()
                .map(|max_class| {
                    (0..=*max_class)
                        .map(|class| {
                            categories
                                .get(&class)
                                .cloned()
                                .unwrap_or_else(|| format!("class_{class}"))
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let classes = class_names.join("\n") + "\n";
            add_zip_entry(
                &mut writer,
                &mut output_paths,
                "classes.txt",
                classes.as_bytes(),
                options,
            )?;
            for (image_index, row) in rows.iter().enumerate() {
                let file_name = &original_file_names[image_index];
                let archive_file_name = &archive_file_names[image_index];
                let width = row.get("width").and_then(Value::as_f64).ok_or_else(|| {
                    format!("dataset version image '{file_name}' is missing width")
                })?;
                let height = row.get("height").and_then(Value::as_f64).ok_or_else(|| {
                    format!("dataset version image '{file_name}' is missing height")
                })?;
                if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
                    return Err(format!(
                        "dataset version image '{file_name}' has invalid dimensions"
                    ));
                }
                let mut lines = String::new();
                for object in version_objects(row)? {
                    let class = object_class_id(&object)?;
                    match (format, object_type(&object)?) {
                        (DatasetFormat::Yolo, "bbox") => {
                            let bbox = object_bbox(&object)?;
                            let center_x = bbox[0] + bbox[2] / 2.0;
                            let center_y = bbox[1] + bbox[3] / 2.0;
                            if bbox[0] < 0.0
                                || bbox[1] < 0.0
                                || bbox[0] + bbox[2] > width
                                || bbox[1] + bbox[3] > height
                            {
                                return Err(format!(
                                    "YOLO cannot represent out-of-bounds bbox on '{file_name}'"
                                ));
                            }
                            lines.push_str(&format!(
                                "{} {:.6} {:.6} {:.6} {:.6}\n",
                                class,
                                center_x / width,
                                center_y / height,
                                bbox[2] / width,
                                bbox[3] / height
                            ));
                        }
                        (DatasetFormat::YoloSeg, "polygon") => {
                            let polygon = object_polygon(&object)?;
                            if polygon.iter().any(|point| {
                                let x = point.get("x").and_then(Value::as_f64).unwrap_or(-1.0);
                                let y = point.get("y").and_then(Value::as_f64).unwrap_or(-1.0);
                                x < 0.0 || y < 0.0 || x > width || y > height
                            }) {
                                return Err(format!(
                                    "YOLO segmentation cannot represent out-of-bounds polygon on '{file_name}'"
                                ));
                            }
                            lines.push_str(&class.to_string());
                            for point in polygon {
                                lines.push_str(&format!(
                                    " {:.6} {:.6}",
                                    number(
                                        point.get("x").ok_or("polygon x is missing")?,
                                        "polygon x"
                                    )? / width,
                                    number(
                                        point.get("y").ok_or("polygon y is missing")?,
                                        "polygon y"
                                    )? / height,
                                ));
                            }
                            lines.push('\n');
                        }
                        (_, kind) => {
                            return Err(format!(
                                "{} cannot represent annotation of type '{kind}'",
                                format.as_str()
                            ));
                        }
                    }
                }
                let label_path = format!(
                    "labels/{}.txt",
                    FsPath::new(archive_file_name)
                        .with_extension("")
                        .to_string_lossy()
                );
                add_zip_entry(
                    &mut writer,
                    &mut output_paths,
                    &label_path,
                    lines.as_bytes(),
                    options,
                )?;
            }
        }
        DatasetFormat::LabelMe => {
            for (image_index, row) in rows.iter().enumerate() {
                let file_name = &original_file_names[image_index];
                let archive_file_name = &archive_file_names[image_index];
                let image_path = format!("images/{archive_file_name}");
                let shapes = version_objects(row)?
                    .iter()
                    .map(labelme_shape)
                    .collect::<Result<Vec<_>, _>>()?;
                let labelme = json!({
                    "version": "5.0.0",
                    "flags": {},
                    "shapes": shapes,
                    "imagePath": image_path,
                    "originalFileName": file_name,
                    "imageData": Value::Null,
                    "imageHeight": row.get("height").cloned().unwrap_or(Value::Null),
                    "imageWidth": row.get("width").cloned().unwrap_or(Value::Null),
                });
                // Keep the image extension in the annotation filename so
                // `a.png` and `a.jpg` cannot collide at `annotations/a.json`.
                let path = format!("annotations/{archive_file_name}.json");
                let bytes = serde_json::to_vec_pretty(&labelme)
                    .map_err(|error| format!("serialize LabelMe annotation: {error}"))?;
                add_zip_entry(&mut writer, &mut output_paths, &path, &bytes, options)?;
            }
        }
    }
    writer.finish().map_err(|error| {
        if error.to_string().contains("dataset export ZIP exceeds") {
            "dataset export exceeds the compressed size limit".to_string()
        } else {
            format!("finish export ZIP: {error}")
        }
    })?;
    Ok(output.into_inner())
}

/// A bounded seekable writer prevents a format conversion from constructing a
/// ZIP larger than the public compressed-output limit before the caller can
/// reject it.  This is deliberately separate from `ArchiveLimits`, which
/// protects input expansion.
struct LimitedCursor {
    inner: Cursor<Vec<u8>>,
    limit: usize,
}

impl LimitedCursor {
    fn new(limit: usize) -> Self {
        Self {
            inner: Cursor::new(Vec::new()),
            limit,
        }
    }

    fn into_inner(self) -> Vec<u8> {
        self.inner.into_inner()
    }
}

impl Write for LimitedCursor {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let position = self.inner.position() as usize;
        if position > self.limit || bytes.len() > self.limit.saturating_sub(position) {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "dataset export ZIP exceeds the compressed size limit",
            ));
        }
        self.inner.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for LimitedCursor {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = self.inner.seek(position)?;
        if next as usize > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "dataset export ZIP exceeds the compressed size limit",
            ));
        }
        Ok(next)
    }
}

fn add_zip_entry<W: Write + Seek>(
    writer: &mut ZipWriter<W>,
    paths: &mut HashSet<String>,
    path: &str,
    bytes: &[u8],
    options: SimpleFileOptions,
) -> Result<(), String> {
    let path = normalize_zip_path(path, MAX_ENTRY_PATH_BYTES)?;
    if paths.len() >= MAX_ENTRIES {
        return Err(format!(
            "dataset export contains more than {} entries",
            MAX_ENTRIES
        ));
    }
    if !paths.insert(path.clone()) {
        return Err(format!("duplicate output archive path '{path}'"));
    }
    writer
        .start_file(path, options)
        .map_err(|error| format!("create export entry: {error}"))?;
    writer
        .write_all(bytes)
        .map_err(|error| format!("write export entry: {error}"))?;
    Ok(())
}

fn version_file_name(row: &Value) -> Result<String, String> {
    let file_name = row
        .get("fileName")
        .or_else(|| row.get("file_name"))
        .and_then(Value::as_str)
        .ok_or_else(|| "dataset version image is missing fileName".to_string())?;
    safe_reference_path(file_name)
}

fn version_objects(row: &Value) -> Result<Vec<Value>, String> {
    let objects = row
        .get("objects")
        .and_then(Value::as_array)
        .ok_or_else(|| "dataset version image is missing objects".to_string())?
        .clone();
    validate_objects(&objects, "dataset version")?;
    Ok(objects)
}

fn lookup_export_image<'a>(
    image_bytes: &'a BTreeMap<String, Vec<u8>>,
    row: &Value,
    file_name: &str,
) -> Result<&'a [u8], String> {
    for field in ["objectKey", "itemId", "id", "imageId", "clientKey"] {
        if let Some(image_id) = row.get(field).and_then(Value::as_str) {
            if let Some(bytes) = image_bytes.get(image_id) {
                return Ok(bytes);
            }
        }
    }
    if let Some(bytes) = image_bytes.get(file_name) {
        return Ok(bytes);
    }
    Err(format!(
        "dataset version image '{file_name}' is missing object bytes"
    ))
}

fn object_class_id(object: &Value) -> Result<u32, String> {
    class_id(object.get("classId").or_else(|| object.get("class_id")), 0)
}

fn object_label(object: &Value) -> Result<String, String> {
    object
        .get("label")
        .and_then(Value::as_str)
        .filter(|label| !label.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| "annotation object is missing label".to_string())
}

fn object_bbox(object: &Value) -> Result<Vec<f64>, String> {
    let bbox = object
        .get("bbox")
        .and_then(Value::as_object)
        .ok_or_else(|| "bbox annotation is missing geometry".to_string())?;
    let values = [
        number(bbox.get("x").ok_or("bbox x is missing")?, "bbox x")?,
        number(bbox.get("y").ok_or("bbox y is missing")?, "bbox y")?,
        number(
            bbox.get("width").ok_or("bbox width is missing")?,
            "bbox width",
        )?,
        number(
            bbox.get("height").ok_or("bbox height is missing")?,
            "bbox height",
        )?,
    ];
    if values[0] < 0.0 || values[1] < 0.0 || values[2] <= 0.0 || values[3] <= 0.0 {
        return Err("bbox annotation is invalid".to_string());
    }
    Ok(values.to_vec())
}

fn object_polygon(object: &Value) -> Result<Vec<Value>, String> {
    let polygon = object
        .get("polygon")
        .and_then(Value::as_array)
        .ok_or_else(|| "polygon annotation is missing points".to_string())?;
    if polygon.len() < 3 {
        return Err("polygon annotation needs at least three points".to_string());
    }
    for point in polygon {
        number(point.get("x").ok_or("polygon x is missing")?, "polygon x")?;
        number(point.get("y").ok_or("polygon y is missing")?, "polygon y")?;
    }
    Ok(polygon.clone())
}

fn polygon_bbox(polygon: &[Value]) -> Result<Vec<f64>, String> {
    let mut x_min = f64::INFINITY;
    let mut y_min = f64::INFINITY;
    let mut x_max = f64::NEG_INFINITY;
    let mut y_max = f64::NEG_INFINITY;
    for point in polygon {
        let x = number(point.get("x").ok_or("polygon x is missing")?, "polygon x")?;
        let y = number(point.get("y").ok_or("polygon y is missing")?, "polygon y")?;
        x_min = x_min.min(x);
        y_min = y_min.min(y);
        x_max = x_max.max(x);
        y_max = y_max.max(y);
    }
    Ok(vec![x_min, y_min, x_max - x_min, y_max - y_min])
}

fn polygon_area(polygon: &[Value]) -> f64 {
    let mut area = 0.0;
    for pair in polygon
        .iter()
        .cycle()
        .take(polygon.len() + 1)
        .collect::<Vec<_>>()
        .windows(2)
    {
        let x1 = pair[0].get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y1 = pair[0].get("y").and_then(Value::as_f64).unwrap_or(0.0);
        let x2 = pair[1].get("x").and_then(Value::as_f64).unwrap_or(0.0);
        let y2 = pair[1].get("y").and_then(Value::as_f64).unwrap_or(0.0);
        area += x1 * y2 - x2 * y1;
    }
    area.abs() / 2.0
}

fn labelme_shape(object: &Value) -> Result<Value, String> {
    let label = object_label(object)?;
    let id = object
        .get("id")
        .cloned()
        .unwrap_or_else(|| json!("annotation"));
    match object_type(object)? {
        "bbox" => {
            let bbox = object_bbox(object)?;
            Ok(json!({
                "label": label,
                "points": [[bbox[0], bbox[1]], [bbox[0] + bbox[2], bbox[1] + bbox[3]]],
                "group_id": Value::Null,
                "description": "",
                "shape_type": "rectangle",
                "flags": {},
                "imageAnnotationId": id,
            }))
        }
        "polygon" => {
            let points = object_polygon(object)?
                .iter()
                .map(|point| json!([point.get("x"), point.get("y")]))
                .collect::<Vec<_>>();
            Ok(json!({
                "label": label,
                "points": points,
                "group_id": Value::Null,
                "description": "",
                "shape_type": "polygon",
                "flags": {},
                "imageAnnotationId": id,
            }))
        }
        other => Err(format!(
            "LabelMe cannot represent annotation type '{other}'"
        )),
    }
}

fn operation_id_from_headers(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get("x-operation-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::bad_request("X-Operation-Id is required"))?;
    if value.len() > 200 {
        return Err(ApiError::bad_request("X-Operation-Id is too long"));
    }
    Ok(value.to_string())
}

fn require_zip_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        });
    if content_type.as_deref() != Some("application/zip") {
        return Err(ApiError::bad_request(
            "dataset import requires Content-Type: application/zip",
        ));
    }
    Ok(())
}

fn requested_content_hash(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get("x-content-sha256") else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .map_err(|_| ApiError::bad_request("X-Content-SHA256 is not valid ASCII"))?
        .trim()
        .to_ascii_lowercase();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::bad_request(
            "X-Content-SHA256 must be a SHA-256 hex digest",
        ));
    }
    Ok(Some(value))
}

fn validate_operation_id(value: &str) -> Result<(), ApiError> {
    if value.trim().is_empty() || value.len() > 200 {
        return Err(ApiError::bad_request("operationId is invalid"));
    }
    Ok(())
}

fn job_from_row(row: &PgRow) -> JobRecord {
    JobRecord {
        id: row.get("id"),
        dataset_id: row.get("dataset_id"),
        kind: row.get("kind"),
        format: row.get("format"),
        operation_id: row.get("operation_id"),
        requested_by: row.get("requested_by"),
        version_id: row.try_get::<Option<Uuid>, _>("version_id").ok().flatten(),
        status: row.get("status"),
        input_object_key: row
            .try_get::<Option<String>, _>("input_object_key")
            .ok()
            .flatten(),
        output_object_key: row
            .try_get::<Option<String>, _>("output_object_key")
            .ok()
            .flatten(),
        input_content_hash: row
            .try_get::<Option<String>, _>("input_content_hash")
            .ok()
            .flatten(),
        output_content_hash: row
            .try_get::<Option<String>, _>("output_content_hash")
            .ok()
            .flatten(),
        input_byte_size: row
            .try_get::<Option<i64>, _>("input_byte_size")
            .ok()
            .flatten(),
        output_byte_size: row
            .try_get::<Option<i64>, _>("output_byte_size")
            .ok()
            .flatten(),
        quota_reservation_id: row
            .try_get::<Option<Uuid>, _>("quota_reservation_id")
            .ok()
            .flatten(),
        error_code: row
            .try_get::<Option<String>, _>("error_code")
            .ok()
            .flatten(),
        error_message: row
            .try_get::<Option<String>, _>("error_message")
            .ok()
            .flatten(),
    }
}

fn job_json(job: &JobRecord) -> Value {
    json!({
        "id": job.id,
        "datasetId": job.dataset_id,
        "kind": job.kind,
        "format": job.format,
        "operationId": job.operation_id,
        "requestedBy": job.requested_by,
        "versionId": job.version_id,
        "status": job.status,
        "inputContentHash": job.input_content_hash,
        "outputContentHash": job.output_content_hash,
        "inputByteSize": job.input_byte_size,
        "outputByteSize": job.output_byte_size,
        "errorCode": job.error_code,
        "errorMessage": job.error_message,
    })
}

async fn find_job_by_operation(
    state: &AppState,
    dataset_id: Uuid,
    operation_id: &str,
) -> Result<Option<JobRecord>, ApiError> {
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM dataset_jobs WHERE dataset_id = $1 AND operation_id = $2"
    );
    Ok(sqlx::query(&sql)
        .bind(dataset_id)
        .bind(operation_id)
        .fetch_optional(&state.pool)
        .await?
        .as_ref()
        .map(job_from_row))
}

async fn find_job_by_id(state: &AppState, job_id: Uuid) -> Result<JobRecord, ApiError> {
    let sql = format!("SELECT {JOB_COLUMNS} FROM dataset_jobs WHERE id = $1");
    sqlx::query(&sql)
        .bind(job_id)
        .fetch_optional(&state.pool)
        .await?
        .map(|row| job_from_row(&row))
        .ok_or_else(|| ApiError::not_found("dataset job not found"))
}

/// Serialize requests that carry the same `(dataset, operationId)` across the
/// quota reservation, object upload and durable job insert.  A transaction
/// advisory lock is too short-lived for imports because the archive upload is
/// deliberately outside the SQL transaction; keep a dedicated connection for
/// the whole request instead.
struct OperationLockGuard {
    // The permit is held for exactly as long as the dedicated advisory-lock
    // connection.  It must be part of the same guard so a returned connection
    // can never outlive the concurrency bound.
    _permit: OwnedSemaphorePermit,
    _connection: sqlx::pool::PoolConnection<Postgres>,
}

async fn acquire_operation_lock(
    state: &AppState,
    dataset_id: Uuid,
    operation_id: &str,
) -> Result<OperationLockGuard, ApiError> {
    // Wait before touching the pool.  A bounded number of callers can then
    // hold a session-level lock while all other callers remain outside the
    // pool, leaving connections available for quota and job transactions.
    let permit = operation_lock_semaphore()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| ApiError::internal_message("dataset job lock limiter is unavailable"))?;
    let mut connection = state.pool.acquire().await?;
    // `pg_advisory_lock` is session scoped.  Returning this connection to the
    // pool would silently keep the lock attached to an unrelated future
    // request.  Closing on drop makes the lifetime explicit and lets
    // PostgreSQL release the lock even on every early-return/error path.
    connection.close_on_drop();
    let lock_key = format!("image_annotation_dataset_job:{dataset_id}:{operation_id}");
    sqlx::query("SELECT pg_advisory_lock(hashtext($1))")
        .bind(lock_key)
        .execute(&mut *connection)
        .await?;
    Ok(OperationLockGuard {
        _permit: permit,
        _connection: connection,
    })
}

async fn reserve_job_quota(
    tx: &mut Transaction<'_, Postgres>,
    dataset_id: Uuid,
    user_id: Uuid,
    bytes: i64,
    operation_id: &str,
) -> Result<Option<Uuid>, ApiError> {
    if bytes <= 0 {
        return Ok(None);
    }
    // See the module-level contract.  This is a dynamic query so the server
    // binary can be built before the migration is installed on an old volume.
    let row = sqlx::query("SELECT reserve_dataset_job_quota($1, $2, $3, $4) AS reservation_id")
        .bind(dataset_id)
        .bind(user_id)
        .bind(bytes)
        .bind(operation_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|error| {
            tracing::warn!(%error, "dataset job quota reservation failed");
            ApiError::conflict(
                "DATASET_QUOTA_EXCEEDED",
                "dataset quota is unavailable or exhausted",
                Value::Null,
            )
        })?;
    Ok(row
        .try_get::<Option<Uuid>, _>("reservation_id")
        .ok()
        .flatten())
}

async fn release_job_quota(state: &AppState, reservation_id: Option<Uuid>) {
    let Some(reservation_id) = reservation_id else {
        return;
    };
    if let Err(error) = sqlx::query("SELECT release_dataset_job_quota($1)")
        .bind(reservation_id)
        .execute(&state.pool)
        .await
    {
        tracing::error!(%error, %reservation_id, "failed to release dataset job quota");
    }
}

async fn hold_job_quota(state: &AppState, reservation_id: Option<Uuid>) {
    let Some(reservation_id) = reservation_id else {
        return;
    };
    if let Err(error) = sqlx::query(
        "UPDATE quota_reservations SET expires_at = 'infinity'::timestamptz WHERE id = $1 AND consumed_at IS NULL AND released_at IS NULL",
    )
    .bind(reservation_id)
    .execute(&state.pool)
    .await
    {
        tracing::error!(%error, %reservation_id, "failed to hold dataset job quota for cleanup");
    }
}

async fn release_or_hold_job_quota(
    state: &AppState,
    reservation_id: Option<Uuid>,
    cleanup_succeeded: bool,
) {
    if cleanup_succeeded {
        release_job_quota(state, reservation_id).await;
    } else {
        hold_job_quota(state, reservation_id).await;
    }
}

/// Persist a failed upload when S3 cannot confirm deletion.  The row is
/// deliberately `failed` rather than `queued`: it must not be claimed by the
/// normal worker, but the cleanup scanner can find it on the next process
/// iteration and release its reservation only after the object is gone.
async fn persist_pending_import_job(
    state: &AppState,
    job_id: Uuid,
    dataset_id: Uuid,
    format: DatasetFormat,
    operation_id: &str,
    requested_by: Uuid,
    input_object_key: &str,
    input_content_hash: &str,
    input_byte_size: i64,
    reservation_id: Option<Uuid>,
    message: &str,
) -> Result<(), ApiError> {
    let message = message.chars().take(1000).collect::<String>();
    sqlx::query(
        r#"INSERT INTO dataset_jobs (
            id, dataset_id, kind, format, operation_id, requested_by, status,
            input_object_key, input_content_hash, input_byte_size,
            quota_reservation_id, error_code, error_message, updated_at,
            finished_at
        ) VALUES ($1, $2, 'import', $3, $4, $5, 'failed', $6, $7, $8, $9,
                  'CLEANUP_PENDING', $10, now(), now())
        ON CONFLICT (dataset_id, operation_id) DO NOTHING"#,
    )
    .bind(job_id)
    .bind(dataset_id)
    .bind(format.as_str())
    .bind(operation_id)
    .bind(requested_by)
    .bind(input_object_key)
    .bind(input_content_hash)
    .bind(input_byte_size)
    .bind(reservation_id)
    .bind(message)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn load_project_job_permission(
    state: &AppState,
    user_id: Uuid,
    dataset_id: Uuid,
    permission: DatasetPermission,
) -> Result<(), ApiError> {
    require_dataset_permission(state, user_id, dataset_id, permission).await?;
    Ok(())
}

async fn estimate_export_bytes(
    state: &AppState,
    dataset_id: Uuid,
    version_id: Uuid,
) -> Result<i64, ApiError> {
    let manifest = sqlx::query_scalar::<_, Value>(
        "SELECT manifest_json FROM dataset_versions WHERE id = $1 AND dataset_id = $2",
    )
    .bind(version_id)
    .bind(dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset version not found"))?;
    let rows = manifest
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("dataset version has no images"))?;
    let source_bytes = rows.iter().try_fold(0_i64, |total, row| {
        let declared = row
            .get("blob")
            .and_then(|blob| blob.get("byteSize"))
            .and_then(Value::as_i64)
            .ok_or_else(|| ApiError::bad_request("dataset version blob is missing byteSize"))?;
        if declared < 0 {
            return Err(ApiError::bad_request(
                "dataset version blob has an invalid byteSize",
            ));
        }
        Ok(total.saturating_add(declared))
    })?;
    let manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|error| ApiError::internal_message(format!("encode dataset manifest: {error}")))?
        .len() as i64;
    let estimated = source_bytes.saturating_add(manifest_bytes);
    // Metadata and ZIP headers are small, but reserve a little headroom so a
    // successful export cannot exceed the same quota it was checked against.
    Ok(estimated
        .saturating_add(estimated / 10)
        .saturating_add(1)
        .max(1))
}

pub async fn create_import_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Query(query): Query<ImportQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Json<Value>, ApiError> {
    load_project_job_permission(&state, user.id, dataset_id, DatasetPermission::Manage).await?;
    require_zip_content_type(&headers)?;
    let format = DatasetFormat::parse(&query.format).map_err(ApiError::bad_request)?;
    let operation_id = operation_id_from_headers(&headers)?;
    let requested_hash = requested_content_hash(&headers)?;
    let mut compressed = Vec::new();
    let mut stream = body.into_data_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|error| ApiError::bad_request(format!("read import archive: {error}")))?;
        if compressed.len().saturating_add(chunk.len()) > MAX_COMPRESSED_BYTES {
            return Err(ApiError::bad_request(format!(
                "compressed archive exceeds {} bytes",
                MAX_COMPRESSED_BYTES
            )));
        }
        compressed.extend_from_slice(&chunk);
    }
    let input_hash = hex::encode(Sha256::digest(&compressed));
    if requested_hash
        .as_deref()
        .is_some_and(|expected| expected != input_hash)
    {
        return Err(ApiError::bad_request(
            "X-Content-SHA256 does not match the archive",
        ));
    }
    // The body hash is deliberately computed before checking an existing
    // operation.  Replaying an operation with a different body must be a
    // conflict; returning the old job before consuming the body would make a
    // typo in the content hash silently look successful.
    if let Some(existing) = find_job_by_operation(&state, dataset_id, &operation_id).await? {
        if existing.kind != "import" || existing.format != format.as_str() {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another dataset job",
                json!({"jobId": existing.id}),
            ));
        }
        if existing.input_content_hash.as_deref() != Some(input_hash.as_str())
            || existing.input_byte_size != Some(compressed.len() as i64)
        {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId was already used with different archive content",
                json!({"jobId": existing.id}),
            ));
        }
        return Ok(Json(job_json(&existing)));
    }
    // Validate before storing the object.  The worker repeats this after a
    // restart, but early rejection saves quota and avoids orphan objects.
    {
        let validation_lock = IMPORT_VALIDATION_LOCK.get_or_init(|| Mutex::new(()));
        let _validation_guard = validation_lock
            .lock()
            .map_err(|_| ApiError::internal_message("dataset validation worker is unavailable"))?;
        parse_import_archive(&compressed, format, ArchiveLimits::default())
            .map_err(ApiError::bad_request)?;
    }
    // The first lookup above is a fast path. Re-check after taking the
    // session lock so concurrent identical requests cannot both reserve quota
    // before either one inserts the durable job row.
    let _operation_lock = acquire_operation_lock(&state, dataset_id, &operation_id).await?;
    if let Some(existing) = find_job_by_operation(&state, dataset_id, &operation_id).await? {
        if existing.kind != "import" || existing.format != format.as_str() {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another dataset job",
                json!({"jobId": existing.id}),
            ));
        }
        if existing.input_content_hash.as_deref() != Some(input_hash.as_str())
            || existing.input_byte_size != Some(compressed.len() as i64)
        {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId was already used with different archive content",
                json!({"jobId": existing.id}),
            ));
        }
        return Ok(Json(job_json(&existing)));
    }
    let compressed_len = compressed.len() as i64;
    let job_id = Uuid::new_v4();
    let input_object_key = format!("datasets/{dataset_id}/dataset-jobs/{job_id}/input.zip");
    // Reserve quota before writing the object.  A crash after this commit but
    // before the upload leaves only a bounded, expiring reservation; it does
    // not create an unaccounted object.  The durable job row is inserted only
    // after the upload succeeds so the worker cannot race a partial object.
    let reservation = {
        let mut tx = state.pool.begin().await?;
        let reservation = reserve_job_quota(
            &mut tx,
            dataset_id,
            user.id,
            compressed.len() as i64,
            &operation_id,
        )
        .await?;
        tx.commit().await?;
        reservation
    };
    if let Err(error) = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&input_object_key)
        .content_type("application/zip")
        .metadata("sha256", &input_hash)
        .body(ByteStream::from(compressed))
        .send()
        .await
    {
        let message = format!("store import archive: {error}");
        let cleanup_succeeded = state
            .s3
            .delete_object()
            .bucket(&state.config.s3_bucket)
            .key(&input_object_key)
            .send()
            .await
            .is_ok();
        if !cleanup_succeeded {
            if let Err(persist_error) = persist_pending_import_job(
                &state,
                job_id,
                dataset_id,
                format,
                &operation_id,
                user.id,
                &input_object_key,
                &input_hash,
                compressed_len,
                reservation,
                &message,
            )
            .await
            {
                tracing::error!(
                    ?persist_error,
                    job_id = %job_id,
                    "failed to persist pending import cleanup after upload error"
                );
            }
        }
        release_or_hold_job_quota(&state, reservation, cleanup_succeeded).await;
        return Err(ApiError::internal_message(message));
    }

    let sql = format!(
        r#"INSERT INTO dataset_jobs (
            id, dataset_id, kind, format, operation_id, requested_by, status,
            input_object_key, input_content_hash, input_byte_size,
            quota_reservation_id
        ) VALUES ($1, $2, 'import', $3, $4, $5, 'queued', $6, $7, $8, $9)
        ON CONFLICT (dataset_id, operation_id) DO NOTHING
        RETURNING {JOB_COLUMNS}"#
    );
    let result = async {
        let mut tx = state.pool.begin().await?;
        let row = sqlx::query(&sql)
            .bind(job_id)
            .bind(dataset_id)
            .bind(format.as_str())
            .bind(&operation_id)
            .bind(user.id)
            .bind(&input_object_key)
            .bind(&input_hash)
            .bind(compressed_len)
            .bind(reservation)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            let existing = find_job_by_operation(&state, dataset_id, &operation_id)
                .await?
                .ok_or_else(|| ApiError::internal_message("dataset job conflict disappeared"))?;
            if existing.kind != "import"
                || existing.format != format.as_str()
                || existing.input_content_hash.as_deref() != Some(input_hash.as_str())
                || existing.input_byte_size != Some(compressed_len)
            {
                return Err(ApiError::conflict(
                    "OPERATION_ID_REUSED",
                    "operationId was already used with different archive content",
                    json!({"jobId": existing.id}),
                ));
            }
            return Ok(existing);
        };
        tx.commit().await?;
        Ok::<JobRecord, ApiError>(job_from_row(&row))
    }
    .await;
    match result {
        Ok(job) => {
            if job.id != job_id {
                let cleanup_succeeded = state
                    .s3
                    .delete_object()
                    .bucket(&state.config.s3_bucket)
                    .key(&input_object_key)
                    .send()
                    .await
                    .is_ok();
                release_or_hold_job_quota(&state, reservation, cleanup_succeeded).await;
            }
            Ok(Json(job_json(&job)))
        }
        Err(error) => {
            let cleanup_succeeded = state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(&input_object_key)
                .send()
                .await
                .is_ok();
            if !cleanup_succeeded {
                if let Err(persist_error) = persist_pending_import_job(
                    &state,
                    job_id,
                    dataset_id,
                    format,
                    &operation_id,
                    user.id,
                    &input_object_key,
                    &input_hash,
                    compressed_len,
                    reservation,
                    &error.message,
                )
                .await
                {
                    tracing::error!(
                        ?persist_error,
                        job_id = %job_id,
                        "failed to persist pending import cleanup after job insert error"
                    );
                }
            }
            release_or_hold_job_quota(&state, reservation, cleanup_succeeded).await;
            Err(error)
        }
    }
}

pub async fn create_export_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
    Json(input): Json<ExportJobRequest>,
) -> Result<Json<Value>, ApiError> {
    load_project_job_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let format = DatasetFormat::parse(&input.format).map_err(ApiError::bad_request)?;
    validate_operation_id(&input.operation_id)?;
    if let Some(existing) = find_job_by_operation(&state, dataset_id, &input.operation_id).await? {
        if existing.kind != "export"
            || existing.format != format.as_str()
            || existing.version_id != Some(input.version_id)
        {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another dataset job",
                json!({"jobId": existing.id}),
            ));
        }
        return Ok(Json(job_json(&existing)));
    }
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM dataset_versions WHERE id = $1 AND dataset_id = $2)",
    )
    .bind(input.version_id)
    .bind(dataset_id)
    .fetch_one(&state.pool)
    .await?;
    if !exists {
        return Err(ApiError::not_found("dataset version not found"));
    }
    let _operation_lock = acquire_operation_lock(&state, dataset_id, &input.operation_id).await?;
    if let Some(existing) = find_job_by_operation(&state, dataset_id, &input.operation_id).await? {
        if existing.kind != "export"
            || existing.format != format.as_str()
            || existing.version_id != Some(input.version_id)
        {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another dataset job",
                json!({"jobId": existing.id}),
            ));
        }
        return Ok(Json(job_json(&existing)));
    }
    let estimated_bytes = estimate_export_bytes(&state, dataset_id, input.version_id).await?;
    if estimated_bytes > MAX_COMPRESSED_BYTES as i64 {
        return Err(ApiError::bad_request(format!(
            "dataset export estimate exceeds {} bytes",
            MAX_COMPRESSED_BYTES
        )));
    }
    let job_id = Uuid::new_v4();
    let sql = format!(
        r#"INSERT INTO dataset_jobs (
            id, dataset_id, kind, format, operation_id, requested_by, version_id,
            status, quota_reservation_id
        ) VALUES ($1, $2, 'export', $3, $4, $5, $6, 'queued', $7)
        ON CONFLICT (dataset_id, operation_id) DO NOTHING
        RETURNING {JOB_COLUMNS}"#
    );
    let mut tx = state.pool.begin().await?;
    let reservation = reserve_job_quota(
        &mut tx,
        dataset_id,
        user.id,
        estimated_bytes,
        &input.operation_id,
    )
    .await?;
    let row = sqlx::query(&sql)
        .bind(job_id)
        .bind(dataset_id)
        .bind(format.as_str())
        .bind(&input.operation_id)
        .bind(user.id)
        .bind(input.version_id)
        .bind(reservation)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        let existing = find_job_by_operation(&state, dataset_id, &input.operation_id)
            .await?
            .ok_or_else(|| ApiError::internal_message("dataset job conflict disappeared"))?;
        if existing.kind != "export"
            || existing.format != format.as_str()
            || existing.version_id != Some(input.version_id)
        {
            return Err(ApiError::conflict(
                "OPERATION_ID_REUSED",
                "operationId belongs to another dataset job",
                json!({"jobId": existing.id}),
            ));
        }
        return Ok(Json(job_json(&existing)));
    };
    tx.commit().await?;
    Ok(Json(job_json(&job_from_row(&row))))
}

pub async fn list_jobs(
    State(state): State<AppState>,
    user: AuthUser,
    Path(dataset_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    load_project_job_permission(&state, user.id, dataset_id, DatasetPermission::View).await?;
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM dataset_jobs WHERE dataset_id = $1 ORDER BY created_at DESC, id DESC"
    );
    let rows = sqlx::query(&sql)
        .bind(dataset_id)
        .fetch_all(&state.pool)
        .await?;
    Ok(Json(json!({
        "items": rows.iter().map(|row| job_json(&job_from_row(row))).collect::<Vec<_>>()
    })))
}

pub async fn get_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(job_id): Path<Uuid>,
) -> Result<Json<Value>, ApiError> {
    let job = find_job_by_id(&state, job_id).await?;
    load_project_job_permission(&state, user.id, job.dataset_id, DatasetPermission::View).await?;
    Ok(Json(job_json(&job)))
}

pub async fn download_job(
    State(state): State<AppState>,
    user: AuthUser,
    Path(job_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    let job = find_job_by_id(&state, job_id).await?;
    load_project_job_permission(&state, user.id, job.dataset_id, DatasetPermission::View).await?;
    if job.kind != "export" || job.status != "succeeded" {
        return Err(ApiError::conflict(
            "DATASET_JOB_NOT_READY",
            "dataset export is not ready",
            json!({"status": job.status}),
        ));
    }
    let object_key = job
        .output_object_key
        .as_deref()
        .ok_or_else(|| ApiError::not_found("dataset export output not found"))?;
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .send()
        .await
        .map_err(|error| {
            ApiError::not_found(format!("dataset export output not found: {error}"))
        })?;
    let content_length = object.content_length();
    if content_length.is_some_and(|length| length < 0 || length as usize > MAX_COMPRESSED_BYTES) {
        return Err(ApiError::conflict(
            "DATASET_JOB_OUTPUT_TOO_LARGE",
            "dataset export output exceeds the server limit",
            Value::Null,
        ));
    }
    let stream = tokio_util::io::ReaderStream::new(object.body.into_async_read());
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/zip"),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&format!("attachment; filename=dataset-job-{job_id}.zip"))
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    if let Some(length) = content_length {
        if let Ok(value) = HeaderValue::from_str(&length.to_string()) {
            response.headers_mut().insert(header::CONTENT_LENGTH, value);
        }
    }
    if let Some(hash) = job.output_content_hash.as_deref() {
        if let Ok(value) = HeaderValue::from_str(hash) {
            response.headers_mut().insert("x-content-sha256", value);
        }
    }
    Ok(response)
}

/// Start the single job worker.  `main.rs` calls this after migrations and
/// before serving requests.  A running job is put back into `queued` state on
/// startup; only the worker that atomically claims it can execute it.
pub fn spawn_worker(state: AppState) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Some(mut worker_lock) = (match acquire_worker_lock(&state).await {
                Ok(lock) => lock,
                Err(error) => {
                    tracing::error!(?error, "failed to acquire dataset job worker lock");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    continue;
                }
            }) else {
                // Another API process may currently own the lock. Keep this
                // worker alive so it can take over after that process exits.
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            };
            if let Err(error) = recover_running_jobs(&state).await {
                tracing::error!(?error, "failed to recover dataset jobs");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
            if !run_worker_until_lock_loss(&state, &mut worker_lock).await {
                tracing::warn!("dataset job worker advisory lock connection was lost; retrying");
            }
        }
    })
}

/// Keep a PostgreSQL session-level advisory lock for the lifetime of the
/// worker.  It makes startup recovery safe when more than one API process is
/// deployed: only the process that owns the lock may reset running jobs or
/// claim queued work, and a crashed process releases the lock automatically.
async fn acquire_worker_lock(
    state: &AppState,
) -> Result<Option<sqlx::pool::PoolConnection<Postgres>>, ApiError> {
    let mut connection = state.pool.acquire().await?;
    // This connection owns a session-level lock for the entire worker loop;
    // never return it to the shared pool, otherwise a later borrower would
    // inherit the worker lock.  Closing on drop also covers query errors before
    // the caller gets a chance to enter the loop.
    connection.close_on_drop();
    let acquired: bool = sqlx::query_scalar(
        "SELECT pg_try_advisory_lock(hashtext('image_annotation_dataset_jobs_worker'))",
    )
    .fetch_one(&mut *connection)
    .await?;
    Ok(acquired.then_some(connection))
}

async fn ping_worker_lock(
    connection: &mut sqlx::pool::PoolConnection<Postgres>,
) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT 1").execute(&mut **connection).await?;
    Ok(())
}

async fn run_worker_until_lock_loss(
    state: &AppState,
    worker_lock: &mut sqlx::pool::PoolConnection<Postgres>,
) -> bool {
    loop {
        if let Err(error) = retry_pending_cleanup_jobs(state).await {
            tracing::warn!(?error, "pending dataset import cleanup scan failed");
        }
        match claim_next_job(state).await {
            Ok(Some(job)) => {
                if !process_job_with_lock(state, job, worker_lock).await {
                    return false;
                }
            }
            Ok(None) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if ping_worker_lock(worker_lock).await.is_err() {
                    return false;
                }
            }
            Err(error) => {
                tracing::error!(?error, "dataset job worker claim failed");
                tokio::time::sleep(Duration::from_secs(2)).await;
                if ping_worker_lock(worker_lock).await.is_err() {
                    return false;
                }
            }
        }
    }
}

async fn process_job_with_lock(
    state: &AppState,
    job: JobRecord,
    worker_lock: &mut sqlx::pool::PoolConnection<Postgres>,
) -> bool {
    let process = process_job(state, job);
    tokio::pin!(process);
    let mut health = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = &mut process => return true,
            _ = health.tick() => {
                if ping_worker_lock(worker_lock).await.is_err() {
                    // Dropping `process` rolls back any in-flight SQL
                    // transaction. The next worker will recover the running
                    // row and retry it after the advisory session is gone.
                    return false;
                }
            }
        }
    }
}

async fn recover_running_jobs(state: &AppState) -> Result<(), ApiError> {
    sqlx::query(
        "UPDATE dataset_jobs SET status = 'queued', started_at = NULL, updated_at = now() WHERE status = 'running'",
    )
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn claim_next_job(state: &AppState) -> Result<Option<JobRecord>, ApiError> {
    let mut tx = state.pool.begin().await?;
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM dataset_jobs WHERE status = 'queued' ORDER BY created_at, id FOR UPDATE SKIP LOCKED LIMIT 1"
    );
    let Some(row) = sqlx::query(&sql).fetch_optional(&mut *tx).await? else {
        tx.rollback().await?;
        return Ok(None);
    };
    let job = job_from_row(&row);
    sqlx::query(
        "UPDATE dataset_jobs SET status = 'running', started_at = now(), updated_at = now() WHERE id = $1",
    )
    .bind(job.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Some(JobRecord {
        status: "running".to_string(),
        ..job
    }))
}

async fn process_job(state: &AppState, job: JobRecord) {
    let result = match job.kind.as_str() {
        "import" => run_import_job(state, &job).await,
        "export" => run_export_job(state, &job).await,
        other => Err(ApiError::bad_request(format!(
            "unsupported dataset job kind '{other}'"
        ))),
    };
    if let Err(error) = result {
        tracing::error!(job_id = %job.id, %error.message, "dataset job failed");
        let code = if error.code == "CLEANUP_PENDING" {
            "CLEANUP_PENDING"
        } else {
            "JOB_FAILED"
        };
        mark_job_failed(state, &job, code, &error.message).await;
    }
}

async fn download_object(
    state: &AppState,
    object_key: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, ApiError> {
    let object = state
        .s3
        .get_object()
        .bucket(&state.config.s3_bucket)
        .key(object_key)
        .send()
        .await
        .map_err(|error| ApiError::bad_request(format!("read dataset object: {error}")))?;
    if object
        .content_length()
        .is_some_and(|length| length < 0 || length as usize > max_bytes)
    {
        return Err(ApiError::bad_request(format!(
            "dataset object exceeds {} bytes",
            max_bytes
        )));
    }
    let content_length = object.content_length().unwrap_or(0).max(0) as usize;
    let mut reader = object.body.into_async_read();
    let mut bytes = Vec::with_capacity(content_length.min(max_bytes));
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|error| ApiError::bad_request(format!("read dataset object body: {error}")))?;
        if count == 0 {
            break;
        }
        if bytes.len().saturating_add(count) > max_bytes {
            return Err(ApiError::bad_request(format!(
                "dataset object exceeds {} bytes",
                max_bytes
            )));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(bytes)
}

#[derive(Debug, Clone)]
struct PreparedImage {
    source: ImportedImage,
    asset_id: Uuid,
    object_key: String,
    needs_upload: bool,
}

fn import_blob_object_key(dataset_id: Uuid, job_id: Uuid, cache_key: &str) -> String {
    let object_identity = hex::encode(Sha256::digest(cache_key.as_bytes()));
    format!("datasets/{dataset_id}/dataset-jobs/{job_id}/blobs/{object_identity}")
}

fn import_annotation_map(annotations: &[ImportedAnnotation]) -> HashMap<String, Vec<Value>> {
    annotations
        .iter()
        .map(|annotation| (annotation.image_key.clone(), annotation.objects.clone()))
        .collect()
}

/// Estimate the exact JSON shape written by `commit_import`, with bounded
/// headroom for item UUIDs/object-key differences caused by a concurrent
/// deduplicating import.  This estimate is reserved before the first new blob
/// is uploaded, so a large expanded archive cannot bypass the quota ledger.
fn estimate_import_manifest_bytes(
    dataset_id: Uuid,
    format: DatasetFormat,
    schema: &Value,
    classes: &[String],
    annotations: &[ImportedAnnotation],
    prepared: &[PreparedImage],
) -> Result<i64, ApiError> {
    let annotation_map = import_annotation_map(annotations);
    let rows = prepared
        .iter()
        .map(|item| {
            let objects = annotation_map
                .get(&item.source.key)
                .cloned()
                .unwrap_or_default();
            let annotation_json = Value::Array(objects);
            let annotation_bytes = serde_json::to_vec(&annotation_json).map_err(|error| {
                ApiError::internal_message(format!("encode dataset annotation: {error}"))
            })?;
            let annotation_hash = hex::encode(Sha256::digest(&annotation_bytes));
            Ok::<Value, ApiError>(json!({
                "id": Uuid::nil(),
                "clientKey": item.source.key.clone(),
                "fileName": item.source.file_name.clone(),
                "width": item.source.width,
                "height": item.source.height,
                "split": item.source.split.clone(),
                "annotation": annotation_json,
                "annotationHash": annotation_hash,
                "blob": {
                    "id": item.asset_id,
                    "objectKey": item.object_key.clone(),
                    "fileName": item.source.file_name.clone(),
                    "contentHash": item.source.content_hash.clone(),
                    "mimeType": item.source.mime_type.clone(),
                    "width": item.source.width,
                    "height": item.source.height,
                    "byteSize": item.source.bytes.len()
                }
            }))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let manifest = json!({
        "datasetId": dataset_id,
        "format": format.as_str(),
        "schema": schema,
        "classes": classes,
        "items": rows,
    });
    let serialized = serde_json::to_vec(&manifest)
        .map_err(|error| ApiError::internal_message(format!("encode dataset version: {error}")))?;
    let headroom = (prepared.len() as i64)
        .saturating_mul(1024)
        .saturating_add(4096);
    Ok((serialized.len() as i64).saturating_add(headroom).max(1))
}

fn count_new_import_items(existing_keys: &HashSet<String>, prepared: &[PreparedImage]) -> i64 {
    let mut seen = HashSet::new();
    prepared
        .iter()
        .filter(|item| {
            !existing_keys.contains(&item.source.key) && seen.insert(item.source.key.clone())
        })
        .count() as i64
}

fn import_reservation_bytes(
    input_bytes: i64,
    new_blob_bytes: i64,
    manifest_bytes: i64,
) -> Result<i64, ApiError> {
    if input_bytes < 0 || new_blob_bytes < 0 || manifest_bytes < 0 {
        return Err(ApiError::bad_request("import quota estimate is invalid"));
    }
    Ok(input_bytes
        .saturating_add(new_blob_bytes)
        .saturating_add(manifest_bytes))
}

async fn expand_import_reservation(
    state: &AppState,
    job: &JobRecord,
    organization_id: Uuid,
    reserved_bytes: i64,
    reserved_assets: i64,
) -> Result<(), ApiError> {
    let reservation_id = job.quota_reservation_id.ok_or_else(|| {
        ApiError::conflict(
            "QUOTA_RESERVATION_MISSING",
            "import job has no quota reservation",
            Value::Null,
        )
    })?;
    let mut tx = state.pool.begin().await?;
    let updated = super::datasets::reserve_quota_tx(
        &mut tx,
        organization_id,
        "dataset_export",
        reservation_id,
        reserved_bytes,
        reserved_assets,
    )
    .await?;
    if !updated {
        tx.rollback().await?;
        return Err(ApiError::conflict(
            "QUOTA_RESERVATION_REPLAY",
            "import job quota reservation was already consumed",
            Value::Null,
        ));
    }
    tx.commit().await?;
    Ok(())
}

/// Remove only temporary blob keys written by this job when they are no
/// longer referenced by a committed dataset blob.  Every new import object is
/// job-scoped, so a failed import can never delete an object uploaded by a
/// different job.  The reference check also handles a concurrent import that
/// won the dataset_blobs uniqueness race.
async fn cleanup_unreferenced_import_objects(
    state: &AppState,
    job: &JobRecord,
    keys: &[String],
) -> bool {
    let job_prefix = format!("datasets/{}/dataset-jobs/{}/blobs/", job.dataset_id, job.id);
    let mut unique = HashSet::new();
    let mut clean = true;
    for key in keys {
        if !unique.insert(key.as_str()) {
            continue;
        }
        if !key.starts_with(&job_prefix) {
            tracing::warn!(
                job_id = %job.id,
                object_key = %key,
                "refusing to clean dataset import object outside the job prefix"
            );
            clean = false;
            continue;
        }
        let referenced = match sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS(SELECT 1 FROM dataset_blobs WHERE object_key = $1)",
        )
        .bind(key)
        .fetch_one(&state.pool)
        .await
        {
            Ok(referenced) => referenced,
            Err(error) => {
                tracing::warn!(%error, object_key = %key, "cannot determine dataset import object ownership");
                clean = false;
                continue;
            }
        };
        if referenced {
            continue;
        }
        if let Err(error) = state
            .s3
            .delete_object()
            .bucket(&state.config.s3_bucket)
            .key(key)
            .send()
            .await
        {
            tracing::warn!(%error, object_key = %key, "failed to clean unreferenced dataset import object");
            clean = false;
        }
    }
    clean
}

async fn list_job_import_blob_keys(
    state: &AppState,
    job: &JobRecord,
) -> Result<Vec<String>, ApiError> {
    let prefix = format!("datasets/{}/dataset-jobs/{}/blobs/", job.dataset_id, job.id);
    let mut keys = Vec::new();
    let mut continuation = None;
    loop {
        let mut request = state
            .s3
            .list_objects_v2()
            .bucket(&state.config.s3_bucket)
            .prefix(&prefix);
        if let Some(token) = continuation.as_deref() {
            request = request.continuation_token(token);
        }
        let page = request.send().await.map_err(|error| {
            ApiError::internal_message(format!("list dataset import objects: {error}"))
        })?;
        keys.extend(
            page.contents()
                .iter()
                .filter_map(|object| object.key().map(str::to_string)),
        );
        continuation = page.next_continuation_token().map(str::to_string);
        if continuation.is_none() {
            break;
        }
    }
    Ok(keys)
}

fn cleanup_pending_error(message: impl Into<String>) -> ApiError {
    ApiError::conflict("CLEANUP_PENDING", message, Value::Null)
}

async fn cleanup_pending_job(state: &AppState, job: &JobRecord) -> Result<bool, ApiError> {
    let mut clean = true;
    if job.kind == "import" {
        let keys = list_job_import_blob_keys(state, job).await?;
        clean = cleanup_unreferenced_import_objects(state, job, &keys).await;
        if let Some(input_key) = job.input_object_key.as_deref() {
            let expected = format!(
                "datasets/{}/dataset-jobs/{}/input.zip",
                job.dataset_id, job.id
            );
            if input_key != expected {
                tracing::warn!(job_id = %job.id, object_key = %input_key, "refusing to clean unexpected dataset import input object");
                clean = false;
            } else if let Err(error) = state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(input_key)
                .send()
                .await
            {
                tracing::warn!(%error, job_id = %job.id, "failed to clean pending dataset import input object");
                clean = false;
            }
        }
    } else if job.kind == "export" {
        let expected = format!(
            "datasets/{}/dataset-jobs/{}/output.zip",
            job.dataset_id, job.id
        );
        if let Some(output_key) = job.output_object_key.as_deref() {
            if output_key != expected {
                tracing::warn!(job_id = %job.id, object_key = %output_key, "refusing to clean unexpected dataset export output object");
                clean = false;
            } else if let Err(error) = state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(output_key)
                .send()
                .await
            {
                tracing::warn!(%error, job_id = %job.id, "failed to clean pending dataset export output object");
                clean = false;
            }
        } else if let Err(error) = state
            .s3
            .delete_object()
            .bucket(&state.config.s3_bucket)
            .key(&expected)
            .send()
            .await
        {
            tracing::warn!(%error, job_id = %job.id, "failed to clean pending dataset export output object");
            clean = false;
        }
    } else {
        tracing::warn!(job_id = %job.id, kind = %job.kind, "refusing cleanup for unknown dataset job kind");
        clean = false;
    }
    Ok(clean)
}

async fn finalize_pending_cleanup_job(state: &AppState, job: &JobRecord) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "UPDATE dataset_jobs SET error_code = 'JOB_FAILED', error_message = 'job failed; temporary objects cleaned', updated_at = now() WHERE id = $1 AND status = 'failed' AND error_code = 'CLEANUP_PENDING'",
    )
    .bind(job.id)
    .execute(&mut *tx)
    .await?;
    if let Some(reservation_id) = job.quota_reservation_id {
        sqlx::query("SELECT release_dataset_job_quota($1)")
            .bind(reservation_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn retry_pending_cleanup_jobs(state: &AppState) -> Result<(), ApiError> {
    let sql = format!(
        "SELECT {JOB_COLUMNS} FROM dataset_jobs WHERE status = 'failed' AND error_code = 'CLEANUP_PENDING' ORDER BY updated_at, id LIMIT 8"
    );
    let rows = sqlx::query(&sql).fetch_all(&state.pool).await?;
    for row in rows {
        let job = job_from_row(&row);
        match cleanup_pending_job(state, &job).await {
            Ok(true) => {
                if let Err(error) = finalize_pending_cleanup_job(state, &job).await {
                    tracing::error!(?error, job_id = %job.id, "failed to finalize pending dataset import cleanup");
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(?error, job_id = %job.id, "pending dataset import cleanup will retry");
            }
        }
    }
    Ok(())
}

async fn run_import_job(state: &AppState, job: &JobRecord) -> Result<(), ApiError> {
    let input_key = job
        .input_object_key
        .as_deref()
        .ok_or_else(|| ApiError::bad_request("import job has no input object"))?;
    let compressed = download_object(state, input_key, MAX_COMPRESSED_BYTES).await?;
    let input_byte_size = compressed.len() as i64;
    if job
        .input_content_hash
        .as_deref()
        .is_some_and(|expected| expected != hex::encode(Sha256::digest(&compressed)))
    {
        return Err(ApiError::bad_request(
            "import archive content hash mismatch",
        ));
    }
    let format = DatasetFormat::parse(&job.format).map_err(ApiError::bad_request)?;
    let bundle = {
        let validation_lock = IMPORT_VALIDATION_LOCK.get_or_init(|| Mutex::new(()));
        let _validation_guard = validation_lock
            .lock()
            .map_err(|_| ApiError::internal_message("dataset validation worker is unavailable"))?;
        parse_import_archive(&compressed, format, ArchiveLimits::default())
            .map_err(ApiError::bad_request)?
    };
    drop(compressed);
    let dataset = sqlx::query(
        "SELECT organization_id, schema_json FROM datasets WHERE id = $1 AND deleted_at IS NULL",
    )
    .bind(job.dataset_id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let organization_id: Uuid = dataset.get("organization_id");
    let existing_schema: Value = dataset.get("schema_json");
    let ImportBundle {
        format: bundle_format,
        classes,
        images,
        annotations,
        manifest: _,
    } = bundle;
    let mut prepared = Vec::with_capacity(images.len());
    let mut new_blob_keys = HashSet::new();
    let mut new_blob_bytes = 0_i64;
    let mut blob_cache = HashMap::<String, (Uuid, String, bool)>::new();
    for source in images {
        let cache_key = format!(
            "{}:{}:{}",
            source.content_hash,
            source.bytes.len(),
            source.mime_type
        );
        let (blob_id, object_key, needs_upload) = if let Some(blob) = blob_cache.get(&cache_key) {
            blob.clone()
        } else {
            let existing = sqlx::query(
                "SELECT id, object_key FROM dataset_blobs WHERE organization_id = $1 AND content_hash = $2 AND byte_size = $3 AND mime_type = $4",
            )
            .bind(organization_id)
            .bind(&source.content_hash)
            .bind(source.bytes.len() as i64)
            .bind(&source.mime_type)
            .fetch_optional(&state.pool)
            .await?
            .map(|row| {
                (
                    row.get::<Uuid, _>("id"),
                    row.get::<String, _>("object_key"),
                    false,
                )
            });
            let blob = if let Some((blob_id, object_key, _)) = existing {
                // A capture may have retained a project asset object key for
                // this content. Never write a second deterministic object
                // before checking that the existing reference is valid.
                verify_object_hash(
                    state,
                    &object_key,
                    &source.content_hash,
                    source.bytes.len() as i64,
                )
                .await?;
                (blob_id, object_key, false)
            } else {
                let blob_id = Uuid::new_v4();
                // New bytes are isolated to this job. If another import wins
                // the dataset_blobs race, commit_import removes this
                // unreferenced temporary object before committing its rows.
                let object_key = import_blob_object_key(job.dataset_id, job.id, &cache_key);
                if new_blob_keys.insert(object_key.clone()) {
                    new_blob_bytes = new_blob_bytes.saturating_add(source.bytes.len() as i64);
                }
                (blob_id, object_key, true)
            };
            blob_cache.insert(cache_key, blob.clone());
            blob
        };
        prepared.push(PreparedImage {
            source,
            asset_id: blob_id,
            object_key,
            needs_upload,
        });
    }

    let existing_keys: HashSet<String> = sqlx::query_scalar(
        "SELECT client_key FROM dataset_items WHERE dataset_id = $1 AND client_key IS NOT NULL",
    )
    .bind(job.dataset_id)
    .fetch_all(&state.pool)
    .await?
    .into_iter()
    .collect();
    let schema = if classes.is_empty() {
        existing_schema
    } else {
        let mut schema = existing_schema;
        if let Some(schema_object) = schema.as_object_mut() {
            schema_object.insert("classes".to_string(), json!(classes));
            schema
        } else {
            json!({ "classes": classes })
        }
    };
    let manifest_budget = estimate_import_manifest_bytes(
        job.dataset_id,
        bundle_format,
        &schema,
        &classes,
        &annotations,
        &prepared,
    )?;
    let reserved_bytes = import_reservation_bytes(
        job.input_byte_size.unwrap_or(input_byte_size),
        new_blob_bytes,
        manifest_budget,
    )?;
    let reserved_assets = count_new_import_items(&existing_keys, &prepared);
    expand_import_reservation(state, job, organization_id, reserved_bytes, reserved_assets).await?;

    let mut uploaded_object_keys = Vec::new();
    let mut upload_keys = HashSet::new();
    for item in prepared.iter().filter(|item| item.needs_upload) {
        if !upload_keys.insert(item.object_key.clone()) {
            continue;
        }
        // Track the key before PUT: an SDK error can still leave a completed
        // object in an eventually-consistent S3-compatible store.
        uploaded_object_keys.push(item.object_key.clone());
        if let Err(error) = state
            .s3
            .put_object()
            .bucket(&state.config.s3_bucket)
            .key(&item.object_key)
            .content_type(&item.source.mime_type)
            .metadata("sha256", &item.source.content_hash)
            .body(ByteStream::from(item.source.bytes.clone()))
            .send()
            .await
        {
            let clean =
                cleanup_unreferenced_import_objects(state, job, &uploaded_object_keys).await;
            if !clean {
                return Err(cleanup_pending_error(format!(
                    "store imported image failed: {error}; temporary object cleanup is pending"
                )));
            }
            return Err(ApiError::internal_message(format!(
                "store imported image: {error}"
            )));
        }
    }
    let result = commit_import(state, job, bundle_format, &classes, &annotations, &prepared).await;
    let clean = cleanup_unreferenced_import_objects(state, job, &uploaded_object_keys).await;
    match result {
        Ok(()) => {
            if !clean {
                tracing::error!(
                    job_id = %job.id,
                    "dataset import committed but temporary object cleanup needs investigation"
                );
            }
            Ok(())
        }
        Err(error) if clean => Err(error),
        Err(error) => Err(cleanup_pending_error(format!(
            "{}; temporary object cleanup is pending",
            error.message
        ))),
    }
}

async fn commit_import(
    state: &AppState,
    job: &JobRecord,
    format: DatasetFormat,
    classes: &[String],
    annotations: &[ImportedAnnotation],
    prepared: &[PreparedImage],
) -> Result<(), ApiError> {
    let mut tx = state.pool.begin().await?;
    let dataset = sqlx::query(
        "SELECT organization_id, schema_json FROM datasets WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(job.dataset_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| ApiError::not_found("dataset not found"))?;
    let organization_id: Uuid = dataset.get("organization_id");
    let existing_schema: Value = dataset.get("schema_json");
    let schema = if classes.is_empty() {
        existing_schema
    } else {
        let mut schema = existing_schema;
        if let Some(schema_object) = schema.as_object_mut() {
            schema_object.insert("classes".to_string(), json!(classes));
            schema
        } else {
            json!({ "classes": classes })
        }
    };
    sqlx::query(
        "UPDATE datasets SET schema_json = $2, revision = revision + 1, updated_at = now() WHERE id = $1",
    )
    .bind(job.dataset_id)
    .bind(&schema)
    .execute(&mut *tx)
    .await?;
    let existing_keys: HashSet<String> = sqlx::query_scalar(
        "SELECT client_key FROM dataset_items WHERE dataset_id = $1 AND client_key IS NOT NULL",
    )
    .bind(job.dataset_id)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let annotation_map = annotations
        .iter()
        .map(|annotation| (annotation.image_key.clone(), annotation.objects.clone()))
        .collect::<HashMap<_, _>>();
    let mut manifest_rows = Vec::with_capacity(prepared.len());
    // The input archive is retained as the durable import audit artifact. It
    // is therefore charged exactly once together with newly materialized
    // blobs and the immutable version manifest; failed jobs delete it before
    // releasing (or, when deletion fails, holding) their reservation.
    let mut actual_bytes = job.input_byte_size.unwrap_or(0).max(0);
    let mut actual_assets = 0_i64;
    let mut counted_item_keys = HashSet::new();
    for item in prepared {
        let objects = annotation_map
            .get(&item.source.key)
            .cloned()
            .unwrap_or_default();
        let annotation_json = Value::Array(objects.clone());
        let annotation_bytes = serde_json::to_vec(&annotation_json).map_err(|error| {
            ApiError::internal_message(format!("encode dataset annotation: {error}"))
        })?;
        let annotation_hash = hex::encode(Sha256::digest(&annotation_bytes));
        let inserted_blob = sqlx::query(
            r#"INSERT INTO dataset_blobs (
                id, organization_id, object_key, file_name, content_hash,
                mime_type, width, height, byte_size
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
            ON CONFLICT (organization_id, content_hash, byte_size, mime_type) DO NOTHING
            RETURNING id, object_key"#,
        )
        .bind(item.asset_id)
        .bind(organization_id)
        .bind(&item.object_key)
        .bind(&item.source.file_name)
        .bind(&item.source.content_hash)
        .bind(&item.source.mime_type)
        .bind(item.source.width as i32)
        .bind(item.source.height as i32)
        .bind(item.source.bytes.len() as i64)
        .fetch_optional(&mut *tx)
        .await?;
        let (blob_id, blob_object_key, blob_is_new) = if let Some(row) = inserted_blob {
            (
                row.get::<Uuid, _>("id"),
                row.get::<String, _>("object_key"),
                true,
            )
        } else {
            let row = sqlx::query(
                "SELECT id, object_key FROM dataset_blobs WHERE organization_id = $1 AND content_hash = $2 AND byte_size = $3 AND mime_type = $4",
            )
            .bind(organization_id)
            .bind(&item.source.content_hash)
            .bind(item.source.bytes.len() as i64)
            .bind(&item.source.mime_type)
            .fetch_one(&mut *tx)
            .await?;
            (
                row.get::<Uuid, _>("id"),
                row.get::<String, _>("object_key"),
                false,
            )
        };
        if !blob_is_new
            && blob_object_key.as_str() != item.object_key.as_str()
            && item.object_key.starts_with(&format!(
                "datasets/{}/dataset-jobs/{}/blobs/",
                job.dataset_id, job.id
            ))
        {
            state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(&item.object_key)
                .send()
                .await
                .map_err(|error| {
                    ApiError::internal_message(format!(
                        "clean duplicate imported image object: {error}"
                    ))
                })?;
        }
        if blob_is_new {
            actual_bytes = actual_bytes.saturating_add(item.source.bytes.len() as i64);
        }
        if !existing_keys.contains(&item.source.key)
            && counted_item_keys.insert(item.source.key.clone())
        {
            actual_assets = actual_assets.saturating_add(1);
        }
        let item_row = sqlx::query(
            r#"INSERT INTO dataset_items (
                id, dataset_id, blob_id, client_key, file_name, split,
                annotation_json, annotation_hash
            ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT(dataset_id, client_key) WHERE client_key IS NOT NULL DO UPDATE SET
                blob_id = excluded.blob_id,
                file_name = excluded.file_name,
                split = excluded.split,
                annotation_json = excluded.annotation_json,
                annotation_hash = excluded.annotation_hash,
                revision = dataset_items.revision + 1,
                updated_at = now()
            RETURNING id, revision"#,
        )
        .bind(Uuid::new_v4())
        .bind(job.dataset_id)
        .bind(blob_id)
        .bind(&item.source.key)
        .bind(&item.source.file_name)
        .bind(&item.source.split)
        .bind(&annotation_json)
        .bind(&annotation_hash)
        .fetch_one(&mut *tx)
        .await?;
        let item_id: Uuid = item_row.get("id");
        manifest_rows.push(json!({
            "id": item_id,
            "clientKey": item.source.key.clone(),
            "fileName": item.source.file_name.clone(),
            "width": item.source.width,
            "height": item.source.height,
            "split": item.source.split.clone(),
            "annotation": annotation_json,
            "annotationHash": annotation_hash,
            "blob": {
                "id": blob_id,
                "objectKey": blob_object_key,
                "fileName": item.source.file_name.clone(),
                "contentHash": item.source.content_hash.clone(),
                "mimeType": item.source.mime_type.clone(),
                "width": item.source.width,
                "height": item.source.height,
                "byteSize": item.source.bytes.len()
            }
        }));
    }
    let manifest = json!({
        "datasetId": job.dataset_id,
        "format": format.as_str(),
        "schema": schema,
        "classes": classes,
        "items": manifest_rows,
    });
    let manifest_bytes = serde_json::to_vec(&manifest)
        .map_err(|error| ApiError::internal_message(format!("encode dataset version: {error}")))?;
    let manifest_hash = hex::encode(Sha256::digest(&manifest_bytes));
    actual_bytes = actual_bytes.saturating_add(manifest_bytes.len() as i64);
    let version_id = Uuid::new_v4();
    let version_number: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version_number), 0) + 1 FROM dataset_versions WHERE dataset_id = $1",
    )
    .bind(job.dataset_id)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO dataset_versions (id, dataset_id, version_number, name, manifest_json, manifest_hash, manifest_byte_size, item_count, created_by) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(version_id)
    .bind(job.dataset_id)
    .bind(version_number)
    .bind(format!("Import {}", Utc::now().to_rfc3339()))
    .bind(&manifest)
    .bind(&manifest_hash)
    .bind(manifest_bytes.len() as i64)
    .bind(prepared.len() as i64)
    .bind(job.requested_by)
    .execute(&mut *tx)
    .await?;
    if let Some(reservation_id) = job.quota_reservation_id {
        super::datasets::consume_quota_tx(
            &mut tx,
            organization_id,
            "dataset_export",
            reservation_id,
            actual_bytes,
            actual_assets,
        )
        .await?;
    }
    sqlx::query(
        "UPDATE dataset_jobs SET status = 'succeeded', version_id = $2, updated_at = now(), finished_at = now(), error_code = NULL, error_message = NULL WHERE id = $1 AND status = 'running'",
    )
    .bind(job.id)
    .bind(version_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn run_export_job(state: &AppState, job: &JobRecord) -> Result<(), ApiError> {
    let version_id = job
        .version_id
        .ok_or_else(|| ApiError::bad_request("export job has no versionId"))?;
    let row =
        sqlx::query("SELECT manifest_json FROM dataset_versions WHERE id = $1 AND dataset_id = $2")
            .bind(version_id)
            .bind(job.dataset_id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| ApiError::not_found("dataset version not found"))?;
    let version_manifest: Value = row.get("manifest_json");
    let rows = version_manifest
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::bad_request("dataset version has no images"))?;
    let mut image_bytes = BTreeMap::new();
    let mut downloaded_bytes = 0_u64;
    let mut export_rows = Vec::with_capacity(rows.len());
    for image in rows {
        let file_name = image
            .get("fileName")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("dataset item is missing fileName"))?
            .to_string();
        let key = image
            .get("clientKey")
            .or_else(|| image.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("dataset item is missing clientKey"))?
            .to_string();
        let item_id = image
            .get("id")
            .or_else(|| image.get("itemId"))
            .or_else(|| image.get("clientKey"))
            .or_else(|| image.get("imageId"))
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("dataset item is missing immutable item id"))?
            .to_string();
        let blob = image
            .get("blob")
            .ok_or_else(|| ApiError::bad_request("dataset item is missing blob"))?;
        let object_key = blob
            .get("objectKey")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("dataset blob is missing objectKey"))?;
        let expected_hash = blob
            .get("contentHash")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::bad_request("dataset blob is missing contentHash"))?;
        let declared_size = blob.get("byteSize").and_then(Value::as_u64).unwrap_or(0);
        let remaining = MAX_EXPANDED_BYTES.saturating_sub(downloaded_bytes);
        if declared_size > remaining {
            return Err(ApiError::bad_request(
                "dataset export source bytes exceed the expanded size limit",
            ));
        }
        let bytes = download_object(state, object_key, remaining as usize).await?;
        if bytes.len() as u64 != declared_size {
            return Err(ApiError::bad_request(format!(
                "dataset blob '{}' byteSize does not match the immutable version manifest",
                file_name
            )));
        }
        downloaded_bytes = downloaded_bytes.saturating_add(bytes.len() as u64);
        let actual_hash = hex::encode(Sha256::digest(&bytes));
        if actual_hash != expected_hash {
            return Err(ApiError::bad_request(format!(
                "dataset blob '{}' content hash mismatch",
                file_name
            )));
        }
        // Keep one owned copy per immutable blob/object key. The archive
        // builder derives a collision-free package filename separately, so
        // two source items with the same display name cannot overwrite bytes.
        image_bytes.insert(object_key.to_string(), bytes);
        export_rows.push(json!({
            "imageId": key,
            "itemId": item_id,
            "objectKey": object_key,
            "fileName": file_name,
            "width": blob.get("width").cloned().unwrap_or_else(|| image.get("width").cloned().unwrap_or(Value::Null)),
            "height": blob.get("height").cloned().unwrap_or_else(|| image.get("height").cloned().unwrap_or(Value::Null)),
            "split": image.get("split").cloned().unwrap_or_else(|| json!("train")),
            "objects": image.get("annotation").cloned().unwrap_or_else(|| json!([])),
        }));
    }
    let mut manifest = json!({
        "datasetId": job.dataset_id,
        "name": format!("dataset-version-{version_id}"),
        "annotations": export_rows,
    });
    if let Some(schema) = version_manifest.get("schema") {
        manifest["schema"] = schema.clone();
    }
    if let Some(classes) = version_manifest.get("classes") {
        manifest["classes"] = classes.clone();
    }
    let format = DatasetFormat::parse(&job.format).map_err(ApiError::bad_request)?;
    let output =
        build_export_archive(format, &manifest, &image_bytes).map_err(ApiError::bad_request)?;
    if output.len() > MAX_COMPRESSED_BYTES {
        return Err(ApiError::bad_request(format!(
            "dataset export exceeds {} bytes",
            MAX_COMPRESSED_BYTES
        )));
    }
    let output_hash = hex::encode(Sha256::digest(&output));
    let output_len = output.len() as i64;
    let output_key = format!(
        "datasets/{}/dataset-jobs/{}/output.zip",
        job.dataset_id, job.id
    );
    let upload_result = state
        .s3
        .put_object()
        .bucket(&state.config.s3_bucket)
        .key(&output_key)
        .content_type("application/zip")
        .metadata("sha256", &output_hash)
        .body(ByteStream::from(output))
        .send()
        .await;
    if let Err(error) = upload_result {
        let cleanup_succeeded = state
            .s3
            .delete_object()
            .bucket(&state.config.s3_bucket)
            .key(&output_key)
            .send()
            .await
            .is_ok();
        if !cleanup_succeeded {
            return Err(cleanup_pending_error(format!(
                "store dataset export: {error}; output cleanup is pending"
            )));
        }
        return Err(ApiError::internal_message(format!(
            "store dataset export: {error}"
        )));
    }
    let result = async {
        let mut tx = state.pool.begin().await?;
        if let Some(reservation_id) = job.quota_reservation_id {
            let organization_id: Uuid = sqlx::query_scalar(
                "SELECT organization_id FROM datasets WHERE id = $1 AND deleted_at IS NULL FOR UPDATE",
            )
            .bind(job.dataset_id)
            .fetch_one(&mut *tx)
            .await?;
            super::datasets::consume_quota_tx(
                &mut tx,
                organization_id,
                "dataset_export",
                reservation_id,
                output_len,
                0,
            )
            .await?;
        }
        sqlx::query(
            "UPDATE dataset_jobs SET status = 'succeeded', output_object_key = $2, output_content_hash = $3, output_byte_size = $4, updated_at = now(), finished_at = now(), error_code = NULL, error_message = NULL WHERE id = $1 AND status = 'running'",
        )
        .bind(job.id)
        .bind(&output_key)
        .bind(&output_hash)
        .bind(output_len)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok::<(), ApiError>(())
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let cleanup_succeeded = state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(&output_key)
                .send()
                .await
                .is_ok();
            if cleanup_succeeded {
                Err(error)
            } else {
                Err(cleanup_pending_error(format!(
                    "{}; output cleanup is pending",
                    error.message
                )))
            }
        }
    }
}

async fn mark_job_failed(state: &AppState, job: &JobRecord, code: &str, message: &str) {
    let message = message.chars().take(1000).collect::<String>();
    let mut final_code = code;
    if job.kind == "import" && code != "CLEANUP_PENDING" {
        if let Some(object_key) = job.input_object_key.as_deref() {
            let expected = format!(
                "datasets/{}/dataset-jobs/{}/input.zip",
                job.dataset_id, job.id
            );
            if object_key != expected {
                tracing::warn!(job_id = %job.id, object_key = %object_key, "refusing to clean unexpected failed dataset import input object");
                final_code = "CLEANUP_PENDING";
            } else if let Err(error) = state
                .s3
                .delete_object()
                .bucket(&state.config.s3_bucket)
                .key(object_key)
                .send()
                .await
            {
                tracing::warn!(%error, job_id = %job.id, "failed to clean failed dataset import object");
                final_code = "CLEANUP_PENDING";
            }
        }
    }
    let cleanup_pending = final_code == "CLEANUP_PENDING";
    let result = async {
        let mut tx = state.pool.begin().await?;
        sqlx::query(
            "UPDATE dataset_jobs SET status = 'failed', error_code = $2, error_message = $3, updated_at = now(), finished_at = now() WHERE id = $1 AND status IN ('queued', 'running')",
        )
        .bind(job.id)
        .bind(final_code)
        .bind(&message)
        .execute(&mut *tx)
        .await?;
        if let Some(reservation_id) = job.quota_reservation_id {
            if cleanup_pending {
                sqlx::query(
                    "UPDATE quota_reservations SET expires_at = 'infinity'::timestamptz WHERE id = $1 AND consumed_at IS NULL AND released_at IS NULL",
                )
                .bind(reservation_id)
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("SELECT release_dataset_job_quota($1)")
                    .bind(reservation_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await
    }
    .await;
    if let Err(error) = result {
        tracing::error!(%error, job_id = %job.id, "failed to mark dataset job failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

    fn sample_png() -> Vec<u8> {
        let image = RgbImage::from_pixel(4, 3, Rgb([40, 80, 120]));
        let mut output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(image)
            .write_to(&mut output, ImageFormat::Png)
            .unwrap();
        output.into_inner()
    }

    fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut output = Cursor::new(Vec::new());
        {
            let mut writer = ZipWriter::new(&mut output);
            let options =
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
            for (path, bytes) in entries {
                writer.start_file(*path, options).unwrap();
                writer.write_all(bytes).unwrap();
            }
            writer.finish().unwrap();
        }
        output.into_inner()
    }

    #[test]
    fn rejects_zip_slip_duplicate_and_ratio_bombs() {
        let traversal = zip_bytes(&[("../escape.txt", b"x")]);
        assert!(read_archive(&traversal, ArchiveLimits::default())
            .unwrap_err()
            .contains("traversal"));

        // `zip` correctly rejects duplicate names while writing, so build a
        // valid two-entry archive first and mutate the second name in both
        // local/central headers to create the hostile input for the reader.
        let mut duplicate = zip_bytes(&[("a.txt", b"a"), ("b.txt", b"b")]);
        for index in 0..=duplicate.len() - b"b.txt".len() {
            if &duplicate[index..index + b"b.txt".len()] == b"b.txt" {
                duplicate[index] = b'a';
            }
        }
        assert!(read_archive(&duplicate, ArchiveLimits::default())
            .unwrap_err()
            .contains("duplicate"));

        let compressed = zip_bytes(&[("large.txt", &vec![b'a'; 16 * 1024])]);
        let limits = ArchiveLimits {
            compression_ratio: 1,
            ..ArchiveLimits::default()
        };
        assert!(read_archive(&compressed, limits)
            .unwrap_err()
            .contains("ratio"));
    }

    #[test]
    fn import_quota_reservation_includes_input_blobs_and_manifest_once() {
        let target = import_reservation_bytes(17, 31, 43).unwrap();
        assert_eq!(target, 91);
        assert!(target >= 17 + 31 + 43);
        assert_eq!(
            import_reservation_bytes(-1, 31, 43).unwrap_err().code,
            "BAD_REQUEST"
        );
    }

    #[test]
    fn native_manifest_round_trips_images_and_bbox() {
        let image = sample_png();
        let manifest = json!({
            "format": "native",
            "name": "fixture",
            "annotations": [{
                "imageId": "image-1",
                "fileName": "sample.png",
                "width": 4,
                "height": 3,
                "split": "train",
                "objects": [{
                    "id": "box-1",
                    "classId": 0,
                    "label": "box",
                    "type": "bbox",
                    "bbox": {"x": 1.0, "y": 1.0, "width": 2.0, "height": 1.0},
                    "attributes": {}
                }]
            }]
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let archive = zip_bytes(&[
            ("manifest.json", manifest_bytes.as_slice()),
            ("images/sample.png", image.as_slice()),
        ]);
        let bundle =
            parse_import_archive(&archive, DatasetFormat::Native, ArchiveLimits::default())
                .unwrap();
        assert_eq!(bundle.images.len(), 1);
        assert_eq!(bundle.annotations[0].objects.len(), 1);
        assert_eq!(bundle.classes, vec!["box"]);

        let mut image_bytes = BTreeMap::new();
        image_bytes.insert("sample.png".to_string(), image);
        let exported =
            build_export_archive(DatasetFormat::Native, &manifest, &image_bytes).unwrap();
        let files = read_archive(&exported, ArchiveLimits::default()).unwrap();
        assert!(files.iter().any(|file| file.path == "manifest.json"));
        assert!(files.iter().any(|file| file.path == "images/sample.png"));
    }

    #[test]
    fn native_export_names_duplicate_files_by_item_id() {
        let first = sample_png();
        let mut second_output = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(4, 3, Rgb([200, 40, 80])))
            .write_to(&mut second_output, ImageFormat::Png)
            .unwrap();
        let second = second_output.into_inner();
        let manifest = json!({
            "annotations": [
                {
                    "imageId": "image-1",
                    "objectKey": "objects/blob-1",
                    "fileName": "sample.png",
                    "width": 4,
                    "height": 3,
                    "objects": [{
                        "id": "box-1",
                        "classId": 0,
                        "label": "box",
                        "type": "bbox",
                        "bbox": {"x": 0.0, "y": 0.0, "width": 1.0, "height": 1.0},
                        "attributes": {}
                    }]
                },
                {
                    "imageId": "image-2",
                    "objectKey": "objects/blob-2",
                    "fileName": "sample.png",
                    "width": 4,
                    "height": 3,
                    "objects": [{
                        "id": "box-2",
                        "classId": 0,
                        "label": "box",
                        "type": "bbox",
                        "bbox": {"x": 1.0, "y": 1.0, "width": 2.0, "height": 1.0},
                        "attributes": {}
                    }]
                }
            ]
        });
        let mut image_bytes = BTreeMap::new();
        image_bytes.insert("objects/blob-1".to_string(), first.clone());
        image_bytes.insert("objects/blob-2".to_string(), second.clone());
        let archive = build_export_archive(DatasetFormat::Native, &manifest, &image_bytes).unwrap();
        let files = read_archive(&archive, ArchiveLimits::default()).unwrap();
        let image_paths = files
            .iter()
            .filter(|file| file.path.starts_with("images/"))
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        assert_eq!(image_paths.len(), 2);
        assert_ne!(image_paths[0], image_paths[1]);
        assert!(image_paths.iter().all(|path| path.contains("sample.png")));

        let manifest_bytes = files
            .iter()
            .find(|file| file.path == "manifest.json")
            .map(|file| file.bytes.as_slice())
            .unwrap();
        let exported_manifest: Value = serde_json::from_slice(manifest_bytes).unwrap();
        let exported_rows = exported_manifest["annotations"].as_array().unwrap();
        assert_eq!(exported_rows.len(), 2);
        assert_eq!(exported_rows[0]["displayFileName"], "sample.png");
        assert_eq!(exported_rows[1]["displayFileName"], "sample.png");
        assert_ne!(exported_rows[0]["fileName"], exported_rows[1]["fileName"]);

        let imported =
            parse_import_archive(&archive, DatasetFormat::Native, ArchiveLimits::default())
                .unwrap();
        assert_eq!(imported.images.len(), 2);
        assert_ne!(
            imported.images[0].content_hash,
            imported.images[1].content_hash
        );
    }

    #[test]
    fn coco_supports_bbox_and_polygon_but_rejects_rle() {
        let image = sample_png();
        let coco = json!({
            "images": [{"id": 7, "file_name": "sample.png", "width": 4, "height": 3}],
            "categories": [{"id": 2, "name": "box"}, {"id": 4, "name": "region"}],
            "annotations": [
                {"id": 1, "image_id": 7, "category_id": 2, "bbox": [0, 0, 2, 1]},
                {"id": 2, "image_id": 7, "category_id": 4, "segmentation": [[0, 0, 3, 0, 3, 2]]}
            ]
        });
        let coco_bytes = serde_json::to_vec(&coco).unwrap();
        let archive = zip_bytes(&[
            ("annotations.json", coco_bytes.as_slice()),
            ("images/sample.png", image.as_slice()),
        ]);
        let bundle =
            parse_import_archive(&archive, DatasetFormat::Coco, ArchiveLimits::default()).unwrap();
        assert_eq!(bundle.annotations[0].objects.len(), 2);
        assert_eq!(bundle.annotations[0].objects[0]["type"], "bbox");
        assert_eq!(bundle.annotations[0].objects[1]["type"], "polygon");

        let mut invalid = coco;
        invalid["annotations"][0]["segmentation"] = json!({"counts": "RLE", "size": [3, 4]});
        let invalid_bytes = serde_json::to_vec(&invalid).unwrap();
        // Parse the RLE contract with a valid image archive.
        let invalid_archive = zip_bytes(&[
            ("annotations.json", invalid_bytes.as_slice()),
            ("images/sample.png", sample_png().as_slice()),
        ]);
        let error = parse_import_archive(
            &invalid_archive,
            DatasetFormat::Coco,
            ArchiveLimits::default(),
        )
        .unwrap_err();
        assert!(
            error.contains("RLE") || error.contains("polygon"),
            "{error}"
        );
        let _ = invalid_archive;
    }

    #[test]
    fn coco_classification_round_trips_with_attributes() {
        let manifest = json!({
            "name": "classification fixture",
            "classes": ["cat"],
            "annotations": [{
                "imageId": "image-1",
                "fileName": "sample.png",
                "width": 4,
                "height": 3,
                "objects": [{
                    "id": "label-1",
                    "classId": 0,
                    "label": "cat",
                    "type": "classification",
                    "attributes": {"source": "fixture"}
                }]
            }]
        });
        let mut image_bytes = BTreeMap::new();
        image_bytes.insert("sample.png".to_string(), sample_png());
        let archive = build_export_archive(DatasetFormat::Coco, &manifest, &image_bytes).unwrap();
        let bundle =
            parse_import_archive(&archive, DatasetFormat::Coco, ArchiveLimits::default()).unwrap();
        let object = &bundle.annotations[0].objects[0];
        assert_eq!(object["type"], "classification");
        assert_eq!(object["id"], "label-1");
        assert_eq!(object["attributes"]["source"], "fixture");
    }

    #[test]
    fn yolo_and_labelme_are_supported_with_explicit_shape_errors() {
        let image = sample_png();
        let yolo_label = b"0 0.5 0.5 0.5 0.5\n";
        let yolo = zip_bytes(&[
            ("classes.txt", b"box\n"),
            ("images/sample.png", image.as_slice()),
            ("labels/sample.txt", yolo_label),
        ]);
        let bundle =
            parse_import_archive(&yolo, DatasetFormat::Yolo, ArchiveLimits::default()).unwrap();
        assert_eq!(bundle.annotations[0].objects[0]["type"], "bbox");

        let yolo_seg = zip_bytes(&[
            ("classes.txt", b"region\n"),
            ("images/train/sample.png", image.as_slice()),
            ("labels/train/sample.txt", b"0 0.1 0.2 0.8 0.2 0.8 0.8\n"),
        ]);
        let bundle =
            parse_import_archive(&yolo_seg, DatasetFormat::YoloSeg, ArchiveLimits::default())
                .unwrap();
        assert_eq!(bundle.annotations[0].objects[0]["type"], "polygon");

        let mixed_missing_label = zip_bytes(&[
            ("classes.txt", b"box\n"),
            ("images/one.png", image.as_slice()),
            ("images/two.png", image.as_slice()),
            ("labels/one.txt", yolo_label),
        ]);
        let error = parse_import_archive(
            &mixed_missing_label,
            DatasetFormat::Yolo,
            ArchiveLimits::default(),
        )
        .unwrap_err();
        assert!(error.contains("no matching label file"), "{error}");

        let all_unlabeled = zip_bytes(&[
            ("classes.txt", b"box\n"),
            ("images/one.png", image.as_slice()),
            ("images/two.png", image.as_slice()),
        ]);
        let bundle = parse_import_archive(
            &all_unlabeled,
            DatasetFormat::Yolo,
            ArchiveLimits::default(),
        )
        .unwrap();
        assert_eq!(bundle.manifest["annotationFilesPresent"], false);
        assert!(bundle
            .annotations
            .iter()
            .all(|item| item.objects.is_empty()));

        let labelme_root = json!({
            "imagePath": "sample.png",
            "imageWidth": 4,
            "imageHeight": 3,
            "shapes": [{"label": "region", "shape_type": "polygon", "points": [[0,0],[3,0],[3,2]]}]
        });
        let labelme_bytes = serde_json::to_vec(&labelme_root).unwrap();
        let labelme = zip_bytes(&[
            ("sample.json", labelme_bytes.as_slice()),
            ("images/sample.png", image.as_slice()),
        ]);
        let bundle =
            parse_import_archive(&labelme, DatasetFormat::LabelMe, ArchiveLimits::default())
                .unwrap();
        assert_eq!(bundle.annotations[0].objects[0]["type"], "polygon");

        let unsupported = json!({
            "annotations": [{"fileName": "sample.png", "width": 4, "height": 3, "objects": [{"id": "polygon", "label": "region", "classId": 0, "type": "polygon", "polygon": [{"x": 0.0, "y": 0.0}, {"x": 3.0, "y": 0.0}, {"x": 3.0, "y": 2.0}], "attributes": {}}]}]
        });
        let mut image_bytes = BTreeMap::new();
        image_bytes.insert("sample.png".to_string(), sample_png());
        let error =
            build_export_archive(DatasetFormat::Yolo, &unsupported, &image_bytes).unwrap_err();
        assert!(error.contains("cannot represent"), "{error}");
    }
}

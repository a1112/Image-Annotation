use std::{
    fs,
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use axum::{
    extract::{
        multipart::{MultipartError, MultipartRejection},
        rejection::{JsonRejection, PathRejection},
        Multipart, Path as AxumPath, State,
    },
    http::StatusCode,
    response::Response,
    routing::{delete, get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use tokio::{io::AsyncWriteExt, task};
use tower_http::request_id::RequestId;
use zip::ZipArchive;

use super::{
    error::{request_id_value, success, ApiError},
    service::{ImportView, RemoteSampleService, ServiceError},
    with_upload_body_limit,
};

const MAX_FILES: u32 = 10_000;
const MAX_RELATIVE_PATH_BYTES: usize = 1_024;
const MAX_COMPONENT_BYTES: usize = 255;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommitImportRequest {
    format: String,
}

pub(super) fn reader_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route("/imports/{importId}", get(get_import))
        .with_state(service)
}

pub(super) fn editor_routes(service: RemoteSampleService, max_upload_bytes: usize) -> Router {
    let upload = with_upload_body_limit(
        Router::<ImportState>::new().route("/projects/{projectId}/imports", post(upload_import)),
        max_upload_bytes,
    );
    upload
        .merge(
            Router::<ImportState>::new()
                .route("/imports/{importId}", delete(cancel_import))
                .route("/imports/{importId}/commit", post(commit_import)),
        )
        .with_state(ImportState {
            service,
            max_upload_bytes: max_upload_bytes as u64,
        })
}

#[derive(Clone)]
struct ImportState {
    service: RemoteSampleService,
    max_upload_bytes: u64,
}

async fn upload_import(
    State(state): State<ImportState>,
    Extension(request_id): Extension<RequestId>,
    path: Result<AxumPath<String>, PathRejection>,
    multipart: Result<Multipart, MultipartRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match path {
        Ok(AxumPath(value)) => value,
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let mut multipart = match multipart {
        Ok(value) => value,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::payload_too_large().into_response(response_request_id)
        }
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let service = state.service.clone();
    let target = match run_blocking(move || service.begin_import(&project_id)).await {
        Ok(value) => value,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let import_id = target.id.clone();
    let upload =
        stream_multipart(&mut multipart, &target.payload_dir, state.max_upload_bytes).await;
    let (bytes_received, file_count, zip_path) = match upload {
        Ok(value) => value,
        Err(UploadError::TooLarge) => {
            state
                .service
                .fail_import(&import_id, "upload exceeds configured limit");
            return ApiError::payload_too_large().into_response(response_request_id);
        }
        Err(UploadError::Invalid) => {
            state
                .service
                .fail_import(&import_id, "upload contains invalid files");
            return ApiError::validation().into_response(response_request_id);
        }
        Err(UploadError::Storage) => {
            state
                .service
                .fail_import(&import_id, "upload storage failed");
            return ApiError::from(ServiceError::Storage).into_response(response_request_id);
        }
    };
    if let Some(zip_path) = zip_path {
        let payload_dir = target.payload_dir.clone();
        let max_upload_bytes = state.max_upload_bytes;
        match task::spawn_blocking(move || extract_zip(&zip_path, &payload_dir, max_upload_bytes))
            .await
        {
            Ok(Ok(())) => {}
            Ok(Err(UploadError::Invalid)) => {
                state
                    .service
                    .fail_import(&import_id, "archive is invalid or unsafe");
                return ApiError::validation().into_response(response_request_id);
            }
            Ok(Err(UploadError::TooLarge)) => {
                state
                    .service
                    .fail_import(&import_id, "archive exceeds configured limit");
                return ApiError::payload_too_large().into_response(response_request_id);
            }
            _ => {
                state
                    .service
                    .fail_import(&import_id, "archive extraction failed");
                return ApiError::from(ServiceError::Storage).into_response(response_request_id);
            }
        }
    }
    let service = state.service.clone();
    let analysis_id = import_id.clone();
    let result =
        run_blocking(move || service.analyze_import(&analysis_id, bytes_received, file_count))
            .await;
    match result {
        Ok(view) => success(StatusCode::CREATED, view, response_request_id),
        Err(error) => {
            state
                .service
                .fail_import(&import_id, "dataset analysis failed");
            ApiError::from(error).into_response(response_request_id)
        }
    }
}

async fn get_import(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    path: Result<AxumPath<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let import_id = match path {
        Ok(AxumPath(value)) => value,
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let result = run_blocking(move || service.get_import(&import_id)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn commit_import(
    State(state): State<ImportState>,
    Extension(request_id): Extension<RequestId>,
    path: Result<AxumPath<String>, PathRejection>,
    payload: Result<Json<CommitImportRequest>, JsonRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let import_id = match path {
        Ok(AxumPath(value)) => value,
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let Json(payload) = match payload {
        Ok(value) => value,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return ApiError::payload_too_large().into_response(response_request_id)
        }
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let service = state.service;
    let result = run_blocking(move || service.commit_import(&import_id, &payload.format)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn cancel_import(
    State(state): State<ImportState>,
    Extension(request_id): Extension<RequestId>,
    path: Result<AxumPath<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let import_id = match path {
        Ok(AxumPath(value)) => value,
        Err(_) => return ApiError::validation().into_response(response_request_id),
    };
    let service = state.service;
    let result = run_blocking(move || service.cancel_import(&import_id)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

#[derive(Debug)]
enum UploadError {
    Invalid,
    TooLarge,
    Storage,
}

async fn stream_multipart(
    multipart: &mut Multipart,
    payload_dir: &Path,
    max_upload_bytes: u64,
) -> Result<(u64, u32, Option<PathBuf>), UploadError> {
    let mut bytes_received = 0_u64;
    let mut file_count = 0_u32;
    let mut zip_path = None;
    while let Some(mut field) = multipart.next_field().await.map_err(map_multipart_error)? {
        let file_name = field.file_name().ok_or(UploadError::Invalid)?;
        let relative = safe_relative_path(file_name)?;
        validate_extension(&relative)?;
        file_count = file_count.checked_add(1).ok_or(UploadError::Invalid)?;
        if file_count > MAX_FILES {
            return Err(UploadError::Invalid);
        }
        let target = payload_dir.join(&relative);
        if target.exists() {
            return Err(UploadError::Invalid);
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|_| UploadError::Storage)?;
        }
        let mut output = tokio::fs::File::create(&target)
            .await
            .map_err(|_| UploadError::Storage)?;
        while let Some(chunk) = field.chunk().await.map_err(map_multipart_error)? {
            bytes_received = bytes_received
                .checked_add(chunk.len() as u64)
                .ok_or(UploadError::TooLarge)?;
            if bytes_received > max_upload_bytes {
                return Err(UploadError::TooLarge);
            }
            output
                .write_all(&chunk)
                .await
                .map_err(|_| UploadError::Storage)?;
        }
        output.flush().await.map_err(|_| UploadError::Storage)?;
        if relative
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"))
        {
            zip_path = Some(target);
        }
    }
    if file_count == 0 || (zip_path.is_some() && file_count != 1) {
        return Err(UploadError::Invalid);
    }
    Ok((bytes_received, file_count, zip_path))
}

fn safe_relative_path(value: &str) -> Result<PathBuf, UploadError> {
    if value.is_empty() || value.len() > MAX_RELATIVE_PATH_BYTES || value.contains('\0') {
        return Err(UploadError::Invalid);
    }
    let normalized = value.replace('\\', "/");
    let path = Path::new(&normalized);
    if path.is_absolute() {
        return Err(UploadError::Invalid);
    }
    let mut safe = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(value)
                if !value.is_empty() && value.as_encoded_bytes().len() <= MAX_COMPONENT_BYTES =>
            {
                safe.push(value)
            }
            _ => return Err(UploadError::Invalid),
        }
    }
    if safe.as_os_str().is_empty() {
        Err(UploadError::Invalid)
    } else {
        Ok(safe)
    }
}

fn validate_extension(path: &Path) -> Result<(), UploadError> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .ok_or(UploadError::Invalid)?;
    if matches!(
        extension.as_str(),
        "jpg"
            | "jpeg"
            | "png"
            | "bmp"
            | "webp"
            | "tif"
            | "tiff"
            | "txt"
            | "xml"
            | "json"
            | "yaml"
            | "yml"
            | "zip"
    ) {
        Ok(())
    } else {
        Err(UploadError::Invalid)
    }
}

fn extract_zip(
    zip_path: &Path,
    payload_dir: &Path,
    max_extracted_bytes: u64,
) -> Result<(), UploadError> {
    let file = fs::File::open(zip_path).map_err(|_| UploadError::Storage)?;
    let mut archive = ZipArchive::new(file).map_err(|_| UploadError::Invalid)?;
    if archive.len() > MAX_FILES as usize {
        return Err(UploadError::Invalid);
    }
    let extraction = payload_dir.join(".extracted");
    fs::create_dir(&extraction).map_err(|_| UploadError::Storage)?;
    let mut extracted_bytes = 0_u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|_| UploadError::Invalid)?;
        let enclosed = entry.enclosed_name().ok_or(UploadError::Invalid)?;
        let relative = safe_relative_path(&enclosed.to_string_lossy())?;
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o040000 && kind != 0o100000 {
                return Err(UploadError::Invalid);
            }
        }
        let target = extraction.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target).map_err(|_| UploadError::Storage)?;
            continue;
        }
        if !entry.is_file() {
            return Err(UploadError::Invalid);
        }
        validate_extension(&target)?;
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|_| UploadError::Storage)?;
        }
        let mut output = fs::File::create(&target).map_err(|_| UploadError::Storage)?;
        let remaining = max_extracted_bytes.saturating_sub(extracted_bytes);
        let copied = io::copy(
            &mut Read::take(&mut entry, remaining.saturating_add(1)),
            &mut output,
        )
        .map_err(|_| UploadError::Storage)?;
        extracted_bytes = extracted_bytes
            .checked_add(copied)
            .ok_or(UploadError::TooLarge)?;
        if extracted_bytes > max_extracted_bytes {
            return Err(UploadError::TooLarge);
        }
    }
    fs::remove_file(zip_path).map_err(|_| UploadError::Storage)?;
    for entry in fs::read_dir(&extraction).map_err(|_| UploadError::Storage)? {
        let entry = entry.map_err(|_| UploadError::Storage)?;
        fs::rename(entry.path(), payload_dir.join(entry.file_name()))
            .map_err(|_| UploadError::Storage)?;
    }
    fs::remove_dir(&extraction).map_err(|_| UploadError::Storage)?;
    Ok(())
}

fn map_multipart_error(error: MultipartError) -> UploadError {
    if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
        UploadError::TooLarge
    } else {
        UploadError::Invalid
    }
}

fn service_response(
    status: StatusCode,
    result: Result<ImportView, ServiceError>,
    request_id: String,
) -> Response {
    match result {
        Ok(data) => success(status, data, request_id),
        Err(error) => ApiError::from(error).into_response(request_id),
    }
}

async fn run_blocking<T, F>(operation: F) -> Result<T, ServiceError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ServiceError> + Send + 'static,
{
    match task::spawn_blocking(operation).await {
        Ok(result) => result,
        Err(error) => {
            tracing::error!(%error, "remote import task failed");
            Err(ServiceError::Storage)
        }
    }
}

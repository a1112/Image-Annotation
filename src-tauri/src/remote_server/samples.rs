use std::fmt::Write as _;

use axum::{
    body::Body,
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Method, StatusCode},
    response::Response,
    routing::{get, patch},
    Extension, Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt},
    task,
};
use tokio_util::io::ReaderStream;
use tower_http::request_id::RequestId;

use super::{
    error::{request_id_value, success, ApiError},
    service::{
        AssetPayload, AssetSource, RemoteSampleService, SamplePatch, SampleQueryOptions,
        ServiceError,
    },
    Role,
};

const DEFAULT_SAMPLE_LIMIT: u32 = 50;
const MAX_SAMPLE_LIMIT: u32 = 500;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SampleQuery {
    #[serde(default)]
    offset: u32,
    #[serde(default = "default_sample_limit")]
    limit: u32,
    split: Option<String>,
    status: Option<String>,
    qa_status: Option<String>,
    class_id: Option<u32>,
    label: Option<String>,
    #[serde(rename = "q")]
    query: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateSampleRequest {
    split: Option<String>,
    status: Option<String>,
    qa_status: Option<String>,
    review_note: Option<String>,
}

pub(super) fn reader_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route("/projects/{projectId}/samples", get(list_samples))
        .route("/projects/{projectId}/samples/{sampleId}", get(get_sample))
        .route(
            "/projects/{projectId}/samples/{sampleId}/content",
            get(get_sample_content),
        )
        .route(
            "/projects/{projectId}/samples/{sampleId}/thumbnail",
            get(get_sample_thumbnail),
        )
        .with_state(service)
}

pub(super) fn editor_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route(
            "/projects/{projectId}/samples/{sampleId}",
            patch(update_sample),
        )
        .with_state(service)
}

async fn list_samples(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    query: Result<Query<SampleQuery>, QueryRejection>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match extract_project_id(path) {
        Ok(project_id) => project_id,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let query = match query {
        Ok(Query(query)) if (1..=MAX_SAMPLE_LIMIT).contains(&query.limit) => query,
        _ => return ApiError::validation().into_response(response_request_id),
    };
    let result = run_blocking(move || {
        service.list_samples(
            &project_id,
            SampleQueryOptions {
                offset: query.offset,
                limit: query.limit,
                split: query.split,
                status: query.status,
                qa_status: query.qa_status,
                class_id: query.class_id,
                label: query.label,
                query: query.query,
            },
        )
    })
    .await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn get_sample(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.get_sample(&project_id, &sample_id)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn update_sample(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
    payload: Result<Json<UpdateSampleRequest>, JsonRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let payload = match extract_json(payload) {
        Ok(payload) => payload,
        Err(error) => return error.into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result = run_blocking(move || {
        service.update_sample(
            &project_id,
            &sample_id,
            &audit_request_id,
            role,
            SamplePatch {
                split: payload.split,
                status: payload.status,
                qa_status: payload.qa_status,
                review_note: payload.review_note,
            },
        )
    })
    .await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn get_sample_content(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    method: Method,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.sample_content(&project_id, &sample_id)).await;
    asset_service_response(result, &method, &headers, response_request_id).await
}

async fn get_sample_thumbnail(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    method: Method,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.sample_thumbnail(&project_id, &sample_id)).await;
    asset_service_response(result, &method, &headers, response_request_id).await
}

async fn asset_service_response(
    result: Result<AssetPayload, ServiceError>,
    method: &Method,
    request_headers: &HeaderMap,
    request_id: String,
) -> Response {
    let asset = match result {
        Ok(asset) => asset,
        Err(error) => return ApiError::from(error).into_response(request_id),
    };
    let content_disposition = content_disposition(&asset.download_name);
    if request_headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|value| etag_matches(value, &asset.etag))
    {
        return asset_response(
            StatusCode::NOT_MODIFIED,
            asset.content_type,
            &asset.etag,
            content_disposition,
            None,
            asset.size,
            Body::empty(),
        );
    }

    let size = asset.size;
    let requested_range = match request_headers.get(header::RANGE) {
        Some(value) => match value
            .to_str()
            .ok()
            .and_then(|value| parse_single_range(value, size).ok())
        {
            Some(range) => Some(range),
            None => {
                let mut response = ApiError::range_not_satisfiable(size).into_response(request_id);
                response
                    .headers_mut()
                    .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}"))
                        .expect("content range is valid"),
                );
                response
                    .headers_mut()
                    .insert(header::CONTENT_DISPOSITION, content_disposition);
                return response;
            }
        },
        None => None,
    };

    match requested_range {
        Some((start, end)) => {
            let length = end - start + 1;
            let body = match asset_body(asset.source, method, start, length).await {
                Ok(body) => body,
                Err(error) => return ApiError::from(error).into_response(request_id),
            };
            asset_response(
                StatusCode::PARTIAL_CONTENT,
                asset.content_type,
                &asset.etag,
                content_disposition,
                Some((start, end, size)),
                length,
                body,
            )
        }
        None => {
            let body = match asset_body(asset.source, method, 0, size).await {
                Ok(body) => body,
                Err(error) => return ApiError::from(error).into_response(request_id),
            };
            asset_response(
                StatusCode::OK,
                asset.content_type,
                &asset.etag,
                content_disposition,
                None,
                size,
                body,
            )
        }
    }
}

async fn asset_body(
    source: AssetSource,
    method: &Method,
    start: u64,
    length: u64,
) -> Result<Body, ServiceError> {
    if method == Method::HEAD {
        return Ok(Body::empty());
    }
    match source {
        AssetSource::Bytes(bytes) => {
            let end = start.checked_add(length).ok_or(ServiceError::Storage)?;
            let slice = bytes
                .get(start as usize..end as usize)
                .ok_or(ServiceError::Storage)?;
            Ok(Body::from(slice.to_vec()))
        }
        AssetSource::File(file) => {
            let mut file = tokio::fs::File::from_std(file);
            if start > 0 {
                file.seek(std::io::SeekFrom::Start(start))
                    .await
                    .map_err(|_| ServiceError::Storage)?;
            }
            Ok(Body::from_stream(ReaderStream::new(file.take(length))))
        }
    }
}

fn asset_response(
    status: StatusCode,
    content_type: &'static str,
    etag: &str,
    content_disposition: HeaderValue,
    content_range: Option<(u64, u64, u64)>,
    content_length: u64,
    body: Body,
) -> Response {
    let mut response = Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ETAG, etag)
        .header(header::CONTENT_DISPOSITION, content_disposition)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, content_length)
        .body(body)
        .expect("asset response headers are valid");
    if let Some((start, end, size)) = content_range {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{size}"))
                .expect("content range is valid"),
        );
    }
    response
}

fn content_disposition(download_name: &str) -> HeaderValue {
    let mut fallback = download_name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, ' ' | '.' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if fallback.trim_matches(['.', ' ']).is_empty() {
        fallback = "download".to_string();
    }
    let encoded = rfc5987_encode(download_name);
    HeaderValue::from_str(&format!(
        "inline; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("sanitized content disposition is valid")
}

fn rfc5987_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(
                byte,
                b'!' | b'#' | b'$' | b'&' | b'+' | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            )
        {
            encoded.push(char::from(byte));
        } else {
            write!(&mut encoded, "%{byte:02X}").expect("writing to a string cannot fail");
        }
    }
    encoded
}

fn etag_matches(value: &HeaderValue, etag: &str) -> bool {
    let Ok(value) = value.to_str() else {
        return false;
    };
    let normalized_etag = etag.strip_prefix("W/").unwrap_or(etag);
    value.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == normalized_etag
    })
}

fn parse_single_range(value: &str, size: u64) -> Result<(u64, u64), ()> {
    if size == 0 || !value.starts_with("bytes=") || value.contains(',') {
        return Err(());
    }
    let specification = &value["bytes=".len()..];
    let (start, end) = specification.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<u64>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let length = suffix.min(size);
        return Ok((size - length, size - 1));
    }
    let start = start.parse::<u64>().map_err(|_| ())?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        end.parse::<u64>().map_err(|_| ())?.min(size - 1)
    };
    if start > end {
        Err(())
    } else {
        Ok((start, end))
    }
}

fn extract_project_id(path: Result<Path<String>, PathRejection>) -> Result<String, ServiceError> {
    path.map(|Path(project_id)| project_id)
        .map_err(|_| ServiceError::Validation)
}

fn extract_sample_path(
    path: Result<Path<(String, String)>, PathRejection>,
) -> Result<(String, String), ServiceError> {
    path.map(|Path(path)| path)
        .map_err(|_| ServiceError::Validation)
}

fn extract_json<T>(payload: Result<Json<T>, JsonRejection>) -> Result<T, ApiError>
where
    T: DeserializeOwned,
{
    payload.map(|Json(payload)| payload).map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::payload_too_large()
        } else {
            ApiError::validation()
        }
    })
}

fn service_response<T>(
    status: StatusCode,
    result: Result<T, ServiceError>,
    request_id: String,
) -> Response
where
    T: Serialize,
{
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
            tracing::error!(%error, "remote sample service task failed");
            Err(ServiceError::storage())
        }
    }
}

const fn default_sample_limit() -> u32 {
    DEFAULT_SAMPLE_LIMIT
}

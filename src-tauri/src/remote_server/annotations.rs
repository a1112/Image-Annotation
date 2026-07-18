use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::Response,
    routing::{get, post, put},
    Extension, Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use tokio::task;
use tower_http::request_id::RequestId;

use crate::{domain::AnnotationState, storage::AnnotationRevisionExpectation};

use super::{
    error::{request_id_value, success, ApiError},
    service::{RemoteSampleService, ServiceError},
    Role,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SaveAnnotationsRequest {
    objects: Vec<Value>,
    #[serde(default)]
    revision: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReviewAnnotationsRequest {
    decision: String,
    #[serde(default)]
    note: String,
}

pub(super) fn reader_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route(
            "/projects/{projectId}/samples/{sampleId}/annotations",
            get(get_annotations),
        )
        .route(
            "/projects/{projectId}/samples/{sampleId}/annotations/history",
            get(get_annotation_history),
        )
        .with_state(service)
}

pub(super) fn editor_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route(
            "/projects/{projectId}/samples/{sampleId}/annotations",
            put(save_annotations),
        )
        .route(
            "/projects/{projectId}/samples/{sampleId}/submit",
            post(submit_annotations),
        )
        .route(
            "/projects/{projectId}/samples/{sampleId}/review",
            post(review_annotations),
        )
        .with_state(service)
}

async fn get_annotations(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.annotation_state(&project_id, &sample_id)).await;
    annotation_response(result, response_request_id)
}

async fn save_annotations(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    headers: HeaderMap,
    path: Result<Path<(String, String)>, PathRejection>,
    payload: Result<Json<SaveAnnotationsRequest>, JsonRejection>,
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
    let expectation = match parse_if_match(&headers).and_then(|expectation| {
        merge_revision_expectation(expectation, payload.revision.as_deref())
    }) {
        Ok(expectation) => expectation,
        Err(error) => return error.into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result = run_blocking(move || {
        service.save_annotations(
            &project_id,
            &sample_id,
            &audit_request_id,
            role,
            expectation,
            payload.objects,
        )
    })
    .await;
    annotation_response(result, response_request_id)
}

async fn get_annotation_history(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.annotation_history(&project_id, &sample_id)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn submit_annotations(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let (project_id, sample_id) = match extract_sample_path(path) {
        Ok(path) => path,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result = run_blocking(move || {
        service.submit_annotations(&project_id, &sample_id, &audit_request_id, role)
    })
    .await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn review_annotations(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<(String, String)>, PathRejection>,
    payload: Result<Json<ReviewAnnotationsRequest>, JsonRejection>,
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
        service.review_annotations(
            &project_id,
            &sample_id,
            &audit_request_id,
            role,
            &payload.decision,
            &payload.note,
        )
    })
    .await;
    service_response(StatusCode::OK, result, response_request_id)
}

fn annotation_response(
    result: Result<AnnotationState, ServiceError>,
    request_id: String,
) -> Response {
    let state = match result {
        Ok(state) => state,
        Err(error) => return ApiError::from(error).into_response(request_id),
    };
    let etag = match state.revision.as_deref().map(revision_etag) {
        Some(Ok(etag)) => Some(etag),
        Some(Err(error)) => return ApiError::from(error).into_response(request_id),
        None => None,
    };
    let mut response = success(StatusCode::OK, state, request_id);
    if let Some(etag) = etag {
        response.headers_mut().insert(header::ETAG, etag);
    }
    response
}

fn revision_etag(revision: &str) -> Result<HeaderValue, ServiceError> {
    if revision.is_empty()
        || revision.len() > 256
        || !revision.is_ascii()
        || revision
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'"' | b'\\'))
    {
        return Err(ServiceError::storage());
    }
    HeaderValue::from_str(&format!("\"{revision}\"")).map_err(|_| ServiceError::storage())
}

fn parse_if_match(headers: &HeaderMap) -> Result<AnnotationRevisionExpectation, ApiError> {
    let values = headers.get_all(header::IF_MATCH);
    if values.iter().next().is_none() {
        return Ok(AnnotationRevisionExpectation::Missing);
    }
    let mut tags = Vec::new();
    for value in values {
        let value = value.to_str().map_err(|_| ApiError::validation())?;
        for candidate in value.split(',') {
            let candidate = candidate.trim();
            if candidate.is_empty() {
                return Err(ApiError::validation());
            }
            tags.push(candidate.to_string());
        }
    }
    if tags.iter().any(|tag| tag == "*") {
        return if tags.len() == 1 {
            Ok(AnnotationRevisionExpectation::AnyExisting)
        } else {
            Err(ApiError::validation())
        };
    }

    let mut strong = Vec::new();
    for tag in tags {
        let (weak, encoded) = match tag.strip_prefix("W/") {
            Some(encoded) => (true, encoded),
            None => (false, tag.as_str()),
        };
        if encoded.len() < 2 || !encoded.starts_with('"') || !encoded.ends_with('"') {
            return Err(ApiError::validation());
        }
        let revision = &encoded[1..encoded.len() - 1];
        if !valid_revision(revision) {
            return Err(ApiError::validation());
        }
        if !weak {
            strong.push(revision.to_string());
        }
    }
    if strong.is_empty() {
        Ok(AnnotationRevisionExpectation::Never)
    } else {
        Ok(AnnotationRevisionExpectation::Strong(strong))
    }
}

fn merge_revision_expectation(
    header: AnnotationRevisionExpectation,
    body_revision: Option<&str>,
) -> Result<AnnotationRevisionExpectation, ApiError> {
    let Some(body_revision) = body_revision else {
        return Ok(header);
    };
    if !valid_revision(body_revision) {
        return Err(ApiError::validation());
    }
    match header {
        AnnotationRevisionExpectation::Missing => Ok(AnnotationRevisionExpectation::Strong(vec![
            body_revision.to_string(),
        ])),
        AnnotationRevisionExpectation::Strong(revisions)
            if revisions.iter().any(|revision| revision == body_revision) =>
        {
            Ok(AnnotationRevisionExpectation::Strong(vec![
                body_revision.to_string()
            ]))
        }
        AnnotationRevisionExpectation::AnyExisting
        | AnnotationRevisionExpectation::Strong(_)
        | AnnotationRevisionExpectation::Never => Err(ApiError::validation()),
    }
}

fn valid_revision(revision: &str) -> bool {
    !revision.is_empty()
        && revision.len() <= 256
        && revision.is_ascii()
        && !revision
            .bytes()
            .any(|byte| byte.is_ascii_control() || matches!(byte, b'"' | b'\\'))
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
            tracing::error!(%error, "remote annotation service task failed");
            Err(ServiceError::storage())
        }
    }
}

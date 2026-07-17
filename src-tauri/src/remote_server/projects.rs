use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection},
        Path, State,
    },
    http::StatusCode,
    response::Response,
    routing::{get, patch, post},
    Extension, Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::task;
use tower_http::request_id::RequestId;

use super::{
    error::{request_id_value, success, ApiError},
    service::{RemoteSampleService, ServiceError},
    Role,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CreateProjectRequest {
    name: String,
    dataset_type: String,
    #[serde(default = "default_demo_template")]
    demo_template: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateProjectRequest {
    name: Option<String>,
    description: Option<String>,
}

pub(super) fn reader_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route("/projects", get(list_projects))
        .route("/projects/{projectId}", get(get_project))
        .with_state(service)
}

pub(super) fn admin_routes(service: RemoteSampleService) -> Router {
    Router::new()
        .route("/projects", post(create_project))
        .route(
            "/projects/{projectId}",
            patch(update_project).delete(delete_project),
        )
        .route("/projects/{projectId}/restore", post(restore_project))
        .with_state(service)
}

async fn list_projects(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
) -> Response {
    let result = run_blocking(move || service.list_projects()).await;
    service_response(StatusCode::OK, result, request_id_value(&request_id))
}

async fn get_project(
    State(service): State<RemoteSampleService>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match extract_project_id(path) {
        Ok(project_id) => project_id,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let result = run_blocking(move || service.get_project(&project_id)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn create_project(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    payload: Result<Json<CreateProjectRequest>, JsonRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let payload = match extract_json(payload) {
        Ok(payload) => payload,
        Err(error) => return error.into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result = run_blocking(move || {
        service.create_project(
            &payload.name,
            &payload.dataset_type,
            &payload.demo_template,
            &audit_request_id,
            role,
        )
    })
    .await;
    service_response(StatusCode::CREATED, result, response_request_id)
}

async fn update_project(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<String>, PathRejection>,
    payload: Result<Json<UpdateProjectRequest>, JsonRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match extract_project_id(path) {
        Ok(project_id) => project_id,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let payload = match extract_json(payload) {
        Ok(payload) => payload,
        Err(error) => return error.into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result = run_blocking(move || {
        service.update_project(
            &project_id,
            payload.name.as_deref(),
            payload.description.as_deref(),
            &audit_request_id,
            role,
        )
    })
    .await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn delete_project(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match extract_project_id(path) {
        Ok(project_id) => project_id,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result =
        run_blocking(move || service.delete_project(&project_id, &audit_request_id, role)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

async fn restore_project(
    State(service): State<RemoteSampleService>,
    Extension(role): Extension<Role>,
    Extension(request_id): Extension<RequestId>,
    path: Result<Path<String>, PathRejection>,
) -> Response {
    let response_request_id = request_id_value(&request_id);
    let project_id = match extract_project_id(path) {
        Ok(project_id) => project_id,
        Err(error) => return ApiError::from(error).into_response(response_request_id),
    };
    let audit_request_id = response_request_id.clone();
    let result =
        run_blocking(move || service.restore_project(&project_id, &audit_request_id, role)).await;
    service_response(StatusCode::OK, result, response_request_id)
}

fn extract_project_id(path: Result<Path<String>, PathRejection>) -> Result<String, ServiceError> {
    path.map(|Path(project_id)| project_id)
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

fn default_demo_template() -> String {
    "empty".to_string()
}

use std::{
    fmt,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    http::{Extensions, Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::{Map, Value};
use tower_http::request_id::{MakeRequestId, RequestId};

use super::{config::ConfigError, service::ServiceError};

#[derive(Debug)]
pub struct ServerBuildError {
    code: &'static str,
    message: &'static str,
}

impl ServerBuildError {
    pub(crate) const fn initialization_failed() -> Self {
        Self {
            code: "server_initialization_failed",
            message: "the remote sample service could not initialize its data storage",
        }
    }

    pub(crate) const fn data_root_in_use() -> Self {
        Self {
            code: "data_root_in_use",
            message: "the configured data directory is already used by another server process",
        }
    }

    pub(crate) const fn project_state_conflict() -> Self {
        Self {
            code: "project_state_conflict",
            message: "active and trashed project storage are both present",
        }
    }

    pub const fn code(&self) -> &'static str {
        self.code
    }
}

impl From<ConfigError> for ServerBuildError {
    fn from(error: ConfigError) -> Self {
        Self {
            code: error.code(),
            message: "the remote sample server configuration is invalid",
        }
    }
}

impl fmt::Display for ServerBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.message)
    }
}

impl std::error::Error for ServerBuildError {}

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    details: Value,
}

impl ApiError {
    pub(crate) fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "missing or invalid bearer token",
        )
    }

    pub(crate) fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "the authenticated role does not have permission for this operation",
        )
    }

    pub(crate) fn validation() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "validation",
            "the request contains invalid project data",
        )
    }

    pub(crate) fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "the requested API route does not exist",
        )
    }

    pub(crate) fn payload_too_large() -> Self {
        Self::from_rejection(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "the request body exceeds the configured limit",
        )
    }

    pub(crate) fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "the request method is not supported for this API route",
        )
    }

    fn conflict() -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "conflict",
            "the project state conflicts with this operation",
        )
    }

    fn storage() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "storage",
            "the server could not complete the storage operation",
        )
    }

    pub(crate) fn range_not_satisfiable(size: u64) -> Self {
        Self {
            status: StatusCode::RANGE_NOT_SATISFIABLE,
            code: "range_not_satisfiable",
            message: "the requested byte range cannot be satisfied",
            details: serde_json::json!({ "size": size }),
        }
    }

    fn unsupported_image_format() -> Self {
        Self::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_image_format",
            "the image format cannot be decoded for thumbnail generation",
        )
    }

    pub(crate) fn into_response(self, request_id: String) -> Response {
        let status = self.status;
        let envelope = ErrorEnvelope {
            error: ErrorPayload {
                code: self.code,
                message: self.message,
                details: self.details,
            },
            request_id,
        };

        let mut response = (status, Json(envelope)).into_response();
        response.extensions_mut().insert(ErrorEnvelopeMarker);
        response
    }

    pub(crate) fn from_rejection(
        status: StatusCode,
        code: &'static str,
        message: &'static str,
    ) -> Self {
        Self::new(status, code, message)
    }

    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Value::Object(Map::new()),
        }
    }
}

impl From<ServiceError> for ApiError {
    fn from(error: ServiceError) -> Self {
        match error {
            ServiceError::Validation => Self::validation(),
            ServiceError::NotFound => Self::not_found(),
            ServiceError::Conflict => Self::conflict(),
            ServiceError::UnsupportedMedia => Self::unsupported_image_format(),
            ServiceError::Storage => Self::storage(),
        }
    }
}

#[derive(Clone, Copy)]
struct ErrorEnvelopeMarker;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ErrorEnvelope {
    error: ErrorPayload,
    request_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorPayload {
    code: &'static str,
    message: &'static str,
    details: Value,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SuccessEnvelope<T> {
    data: T,
    request_id: String,
}

pub(crate) fn success<T>(status: StatusCode, data: T, request_id: String) -> Response
where
    T: Serialize,
{
    (status, Json(SuccessEnvelope { data, request_id })).into_response()
}

pub(crate) fn is_error_envelope(response: &Response) -> bool {
    response.extensions().get::<ErrorEnvelopeMarker>().is_some()
}

pub(crate) fn request_id(extensions: &Extensions) -> String {
    extensions
        .get::<RequestId>()
        .and_then(|request_id| request_id.header_value().to_str().ok())
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(next_request_id)
}

pub(crate) fn request_id_value(request_id: &RequestId) -> String {
    request_id
        .header_value()
        .to_str()
        .ok()
        .filter(|request_id| !request_id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(next_request_id)
}

#[derive(Clone, Default)]
pub(crate) struct RequestIdGenerator;

impl MakeRequestId for RequestIdGenerator {
    fn make_request_id<B>(&mut self, _request: &Request<B>) -> Option<RequestId> {
        next_request_id().parse().ok().map(RequestId::new)
    }
}

fn next_request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let sequence = REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("request-{timestamp}-{sequence}")
}

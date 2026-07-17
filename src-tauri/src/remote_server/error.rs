use std::{
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

    pub(crate) fn not_implemented() -> Self {
        Self::new(
            StatusCode::NOT_IMPLEMENTED,
            "not_implemented",
            "project creation is not implemented yet",
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
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "the request body exceeds the configured upload limit",
        )
    }

    pub(crate) fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "the request method is not supported for this API route",
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

        (status, Json(envelope)).into_response()
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

pub(crate) fn request_id(extensions: &Extensions) -> String {
    extensions
        .get::<RequestId>()
        .and_then(|request_id| request_id.header_value().to_str().ok())
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

pub mod account;
pub mod annotations;
pub mod assets;
pub mod attachments;
pub mod collaboration;
pub mod data_project_metadata;
pub mod data_projects;
pub mod dataset_jobs;
pub mod datasets;
pub mod issues;
pub mod image_transforms;
pub mod objects;
pub mod projects;
pub mod sync;
pub mod web_auth;
pub mod web_workspace;

use axum::Json;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{json, Value};

pub async fn health() -> Json<Value> {
    Json(json!({ "status": "ok", "service": "image-annotation-server", "version": "0.1.0" }))
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ApiError {
    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "UNAUTHORIZED",
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "BAD_REQUEST",
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "FORBIDDEN",
            message: message.into(),
            details: Value::Null,
        }
    }

    pub fn conflict(code: &'static str, message: impl Into<String>, details: Value) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code,
            message: message.into(),
            details,
        }
    }

    pub fn internal_message(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "INTERNAL_ERROR",
            message: message.into(),
            details: Value::Null,
        }
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "database request failed");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "DATABASE_ERROR",
            message: "database request failed".to_string(),
            details: Value::Null,
        }
    }
}

impl From<(StatusCode, String)> for ApiError {
    fn from((status, message): (StatusCode, String)) -> Self {
        Self {
            status,
            code: if status == StatusCode::FORBIDDEN {
                "FORBIDDEN"
            } else {
                "REQUEST_ERROR"
            },
            message,
            details: Value::Null,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({
                "code": self.code,
                "message": self.message,
                "details": self.details
            })),
        )
            .into_response()
    }
}

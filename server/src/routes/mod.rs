pub mod annotations;
pub mod attachments;
pub mod assets;
pub mod issues;
pub mod projects;
pub mod sync;

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

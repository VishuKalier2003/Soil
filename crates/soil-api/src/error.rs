use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("bad request: {0}")]
    BadRequest(String),
    /// Validation failed; carries the report.
    #[error("invalid")]
    Invalid(Value),
    #[error("internal error: {0}")]
    Internal(String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, body) = match &self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, json!({"error": m})),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, json!({"error": m})),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, json!({"error": m})),
            ApiError::Invalid(report) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                json!({"error": "validation failed", "report": report}),
            ),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": m})),
        };
        (status, Json(body)).into_response()
    }
}

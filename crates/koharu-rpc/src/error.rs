//! Error-to-HTTP mapping shared by every route handler.
//!
//! Domain errors arrive as `anyhow` chains from the shared `App`; the layer
//! classifies the well-known precondition failures so clients can react,
//! and surfaces everything else as a server fault with the full chain.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

#[derive(Debug)]
pub struct ApiError(anyhow::Error);

impl ApiError {
    /// Explicit client-error constructor for handler-level validation.
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self(anyhow::anyhow!(message.into()))
    }

    /// Classifies an error message into an HTTP status. The strings mirror
    /// the domain invariants raised by `koharu_app::core`; unclassified
    /// errors are treated as server faults.
    fn status(message: &str) -> StatusCode {
        if message.contains("no project is open")
            || message.contains("another process is already running")
            || message.contains("while processing is running")
        {
            StatusCode::CONFLICT
        } else if message.contains("is not running")
            || message.contains("does not exist")
            || message.contains("was not found")
        {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

impl<E> From<E> for ApiError
where
    E: Into<anyhow::Error>,
{
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let message = format!("{:#}", self.0);
        let status = Self::status(&message);
        if status.is_server_error() {
            tracing::error!(%message, "api request failed");
        }
        (status, Json(json!({ "error": message }))).into_response()
    }
}

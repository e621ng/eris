use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// API errors. Bodies are JSON like the C++ (`{"message": ...}`); the one
/// deliberate addition is `{"error": "not_indexed"}` for post-ID query misses.
/// IqdbProxy branches only on status != 200, so status choice is free to be
/// sane: 400 validation, 404 missing, 503 database down, 500 bugs.
#[derive(Debug)]
pub enum ApiError {
  BadRequest(String),
  NotFound,
  NotIndexed,
  Unavailable(String),
  Internal(String),
}

impl IntoResponse for ApiError {
  fn into_response(self) -> Response {
    let (status, body) = match self {
      ApiError::BadRequest(message) => (StatusCode::BAD_REQUEST, json!({ "message": message })),
      ApiError::NotFound => (StatusCode::NOT_FOUND, json!({ "message": "Not found" })),
      ApiError::NotIndexed => (StatusCode::NOT_FOUND, json!({ "error": "not_indexed" })),
      ApiError::Unavailable(message) => (
        StatusCode::SERVICE_UNAVAILABLE,
        json!({ "message": message }),
      ),
      ApiError::Internal(message) => (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({ "message": message }),
      ),
    };
    (status, Json(body)).into_response()
  }
}

impl From<eris_store::StoreError> for ApiError {
  fn from(error: eris_store::StoreError) -> Self {
    match &error {
      eris_store::StoreError::CorruptRow { .. } => {
        tracing::error!(%error, "corrupt database row");
        ApiError::Internal("corrupt database row".into())
      }
      _ => {
        tracing::error!(%error, "database error");
        ApiError::Unavailable("database unavailable".into())
      }
    }
  }
}

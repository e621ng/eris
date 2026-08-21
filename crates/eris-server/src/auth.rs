use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use subtle::ConstantTimeEq;

use crate::state::AppState;

/// Paths that stay open even with a token configured: probes and metrics are
/// reached by infrastructure that can't carry credentials; bind-address and
/// firewall rules cover them.
const EXEMPT: &[&str] = &["/healthz", "/ready", "/metrics"];

pub async fn require_bearer(
  State(state): State<Arc<AppState>>,
  request: Request,
  next: Next,
) -> Response {
  let Some(expected) = &state.token else {
    return next.run(request).await; // no token configured: auth disabled
  };
  if EXEMPT.contains(&request.uri().path()) {
    return next.run(request).await;
  }

  let authorized = request
    .headers()
    .get(axum::http::header::AUTHORIZATION)
    .and_then(|v| v.to_str().ok())
    .and_then(|v| v.strip_prefix("Bearer "))
    .is_some_and(|token| token.as_bytes().ct_eq(expected.as_bytes()).into());

  if authorized {
    next.run(request).await
  } else {
    (
      StatusCode::UNAUTHORIZED,
      Json(serde_json::json!({ "message": "Unauthorized" })),
    )
      .into_response()
  }
}

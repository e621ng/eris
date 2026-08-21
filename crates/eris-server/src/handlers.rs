use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::Json;
use eris_core::hash;
use serde_json::{json, Value};

use crate::dto::{clamp_limit, ImagePostBody, QueryBody};
use crate::error::ApiError;
use crate::state::AppState;

fn parse_post_id(id: u64) -> Result<i32, ApiError> {
  i32::try_from(id).map_err(|_| ApiError::BadRequest(format!("post id {id} out of range")))
}

/// POST /images/{id} – compute the signature from channel data and persist it.
/// The local index (and every other node's) picks the change up through the
/// event feed; there is deliberately no immediate local apply.
pub async fn post_image(
  State(state): State<Arc<AppState>>,
  Path(id): Path<u64>,
  Json(body): Json<ImagePostBody>,
) -> Result<Json<Value>, ApiError> {
  let post_id = parse_post_id(id)?;
  let [r, g, b] = body.channels.validate()?;

  let sig = tokio::task::spawn_blocking(move || eris_core::from_channels(&r, &g, &b))
    .await
    .map_err(|e| ApiError::Internal(format!("signature task failed: {e}")))?;

  eris_store::writes::upsert_image(&state.pool, post_id, &sig).await?;
  metrics::counter!("eris_writes_total", "op" => "upsert").increment(1);

  Ok(Json(
    json!({ "post_id": post_id, "hash": hash::encode(&sig) }),
  ))
}

/// DELETE /images/{id} – 200 whether or not the post existed: the C++ behaves
/// that way and IqdbRemoveJob raises on any non-200 (it fires for
/// never-indexed posts routinely).
pub async fn delete_image(
  State(state): State<Arc<AppState>>,
  Path(id): Path<u64>,
) -> Result<Json<Value>, ApiError> {
  let post_id = parse_post_id(id)?;
  let existed = eris_store::writes::delete_image(&state.pool, post_id).await?;
  if existed {
    metrics::counter!("eris_writes_total", "op" => "delete").increment(1);
  }
  Ok(Json(json!({ "post_id": post_id })))
}

/// GET /images/{id} – the stored hash, served from RAM.
pub async fn get_image(
  State(state): State<Arc<AppState>>,
  Path(id): Path<u64>,
) -> Result<Json<Value>, ApiError> {
  let post_id = parse_post_id(id)?;
  let encoded = {
    let index = state.index.read();
    index.get(post_id as u32).map(hash::encode)
  };
  match encoded {
    Some(hash) => Ok(Json(json!({ "post_id": post_id, "hash": hash }))),
    None => Err(ApiError::NotFound),
  }
}

/// POST /query – similarity search by hash, channels, or post_id (that
/// precedence, matching the C++ for the first two; post_id is additive).
pub async fn query(
  State(state): State<Arc<AppState>>,
  Json(body): Json<QueryBody>,
) -> Result<Json<Value>, ApiError> {
  let limit = clamp_limit(body.limit)?;
  let min_score = body.min_score.map(|v| v as f32);

  let (sig, kind) = if let Some(hash_param) = body.hash {
    let sig =
      hash::decode(&hash_param).map_err(|e| ApiError::BadRequest(format!("invalid hash: {e}")))?;
    (sig, "hash")
  } else if let Some(channels) = body.channels {
    let [r, g, b] = channels.validate()?;
    let sig = tokio::task::spawn_blocking(move || eris_core::from_channels(&r, &g, &b))
      .await
      .map_err(|e| ApiError::Internal(format!("signature task failed: {e}")))?;
    (sig, "channels")
  } else if let Some(post_id) = body.post_id {
    let sig = {
      let index = state.index.read();
      index.get(post_id).cloned()
    };
    (sig.ok_or(ApiError::NotIndexed)?, "post_id")
  } else {
    return Err(ApiError::BadRequest(
      "POST /query requires `hash`, `channels`, or `post_id`".into(),
    ));
  };

  let started = Instant::now();
  let index = Arc::clone(&state.index);
  let results: Vec<(i64, f32, String)> = tokio::task::spawn_blocking(move || {
    let index = index.read();
    eris_core::query(&index, &sig, limit, min_score)
      .into_iter()
      .map(|m| {
        let hash = index.get(m.post_id).map(hash::encode).unwrap_or_default(); // unreachable: results are live entries
        (m.post_id as i64, m.score, hash)
      })
      .collect()
  })
  .await
  .map_err(|e| ApiError::Internal(format!("query task failed: {e}")))?;

  metrics::histogram!("eris_query_duration_seconds", "kind" => kind)
    .record(started.elapsed().as_secs_f64());

  let body: Vec<Value> = results
    .into_iter()
    .map(|(post_id, score, hash)| json!({ "post_id": post_id, "score": score, "hash": hash }))
    .collect();
  Ok(Json(Value::Array(body)))
}

/// GET /status – keeps the C++ `"images"` key, plus operational detail.
pub async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
  let stats = {
    let index = state.index.read();
    index.stats()
  };
  let feed = *state.feed.borrow();
  let tombstone_ratio = if stats.live + stats.tombstones > 0 {
    stats.tombstones as f64 / (stats.live + stats.tombstones) as f64
  } else {
    0.0
  };
  Json(json!({
      "images": stats.live,
      "tombstones": stats.tombstones,
      "tombstone_ratio": tombstone_ratio,
      "chunks": stats.chunks,
      "index_heap_bytes": stats.heap_bytes,
      "cursor": feed.cursor,
      "lag_seconds": feed.lag_seconds,
      "ready": state.is_ready(),
      "bootstrap_seconds": state.bootstrap_seconds,
      "uptime_seconds": state.started.elapsed().as_secs(),
      "version": env!("CARGO_PKG_VERSION"),
  }))
}

pub async fn healthz() -> &'static str {
  "ok"
}

pub async fn ready(State(state): State<Arc<AppState>>) -> Result<&'static str, ApiError> {
  if state.is_ready() {
    Ok("ready")
  } else {
    Err(ApiError::Unavailable("not ready".into()))
  }
}

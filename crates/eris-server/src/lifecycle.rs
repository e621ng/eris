use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, MatchedPath, Request};
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use eris_core::Index;
use eris_store::bootstrap::bootstrap;
use eris_store::events::{EventOp, ImageEvent};
use eris_store::feed::{FeedExit, FeedStatus, Follower};
use parking_lot::RwLock;
use tokio::sync::watch;
use tower_http::trace::TraceLayer;

use crate::config::Config;
use crate::state::{AppState, FeedHealth};
use crate::{auth, handlers};

/// Apply a batch of already-parsed feed events to the index. Called under the
/// write lock; must stay pure in-memory work.
pub fn apply_events(index: &mut Index, events: Vec<ImageEvent>) {
  for event in events {
    match event.op {
      EventOp::Upsert => index.insert(
        event.post_id as u32,
        event.sig.expect("upsert event carries a signature"),
      ),
      EventOp::Delete => {
        index.remove(event.post_id as u32);
      }
    }
  }
}

pub struct Started {
  pub state: Arc<AppState>,
  pub router: Router,
}

/// Connect, migrate, bootstrap, and spawn the feed/prune background tasks.
/// Returns the ready-to-serve router; `serve` binds and runs it.
pub async fn start(config: &Config) -> anyhow::Result<Started> {
  let pool = eris_store::connect(&config.database_url, config.db_max_conns).await?;
  if config.migrate {
    eris_store::migrate(&pool).await?;
  }

  tracing::info!("bootstrapping index from database...");
  let boot_started = Instant::now();
  let snapshot = bootstrap(&pool).await?;
  let bootstrap_seconds = boot_started.elapsed().as_secs_f64();
  let stats = snapshot.index.stats();
  tracing::info!(
    images = stats.live,
    chunks = stats.chunks,
    heap_mb = stats.heap_bytes / (1024 * 1024),
    seconds = format!("{bootstrap_seconds:.1}"),
    cursor = snapshot.cursor,
    "bootstrap complete"
  );
  metrics::gauge!("eris_bootstrap_duration_seconds").set(bootstrap_seconds);
  metrics::gauge!("eris_index_images").set(stats.live as f64);

  let index = Arc::new(RwLock::new(snapshot.index));
  let (health_tx, health_rx) = watch::channel(FeedHealth {
    cursor: snapshot.cursor,
    lag_seconds: 0.0,
    updated_at: Instant::now(),
  });

  let state = Arc::new(AppState {
    index: Arc::clone(&index),
    pool: pool.clone(),
    feed: health_rx,
    ready: Arc::new(AtomicBool::new(true)),
    started: Instant::now(),
    token: config.token.clone(),
    ready_max_lag_s: config.ready_max_lag_s,
    bootstrap_seconds,
  });

  spawn_feed_task(
    pool.clone(),
    Arc::clone(&index),
    snapshot.cursor,
    Duration::from_millis(config.feed_interval_ms),
    health_tx,
  );
  spawn_prune_task(
    pool,
    Duration::from_secs(config.prune_interval_s),
    Duration::from_secs(config.event_retention_s),
  );

  let router = Router::new()
    .route(
      "/images/{id}",
      post(handlers::post_image)
        .delete(handlers::delete_image)
        .get(handlers::get_image),
    )
    .route("/query", post(handlers::query))
    .route("/status", get(handlers::status))
    .route("/healthz", get(handlers::healthz))
    .route("/ready", get(handlers::ready))
    .route("/metrics", get(crate::metrics::render))
    .layer(middleware::from_fn_with_state(
      Arc::clone(&state),
      auth::require_bearer,
    ))
    .layer(middleware::from_fn(track_http))
    .layer(DefaultBodyLimit::max(config.body_limit))
    .layer(TraceLayer::new_for_http())
    .with_state(Arc::clone(&state));

  Ok(Started { state, router })
}

pub async fn serve(config: Config) -> anyhow::Result<()> {
  // Touch the recorder before anything records into it.
  let _ = crate::metrics::handle();

  let started = start(&config).await?;
  let listener = tokio::net::TcpListener::bind(&config.listen).await?;
  tracing::info!(listen = %config.listen, "serving");
  axum::serve(listener, started.router)
    .with_graceful_shutdown(shutdown_signal())
    .await?;
  Ok(())
}

/// The follower loop: tail events forever; on horizon breach, re-bootstrap in
/// the background (queries keep serving the stale index) and swap; on
/// database errors, back off and resume from the retained cursor.
fn spawn_feed_task(
  pool: sqlx::PgPool,
  index: Arc<RwLock<Index>>,
  cursor: i64,
  interval: Duration,
  health_tx: watch::Sender<FeedHealth>,
) {
  tokio::spawn(async move {
    let (status_tx, mut status_rx) = watch::channel(FeedStatus {
      cursor,
      lag_seconds: 0.0,
    });

    // Forward store-level feed status into the timestamped server watch.
    tokio::spawn(async move {
      while status_rx.changed().await.is_ok() {
        let status = *status_rx.borrow_and_update();
        metrics::gauge!("eris_feed_lag_seconds").set(status.lag_seconds);
        metrics::gauge!("eris_feed_cursor").set(status.cursor as f64);
        let _ = health_tx.send(FeedHealth {
          cursor: status.cursor,
          lag_seconds: status.lag_seconds,
          updated_at: Instant::now(),
        });
      }
    });

    let mut follower = Follower::new(pool.clone(), cursor, interval);
    loop {
      let apply_index = Arc::clone(&index);
      let result = follower
        .run(
          move |events| {
            let mut index = apply_index.write();
            apply_events(&mut index, events);
            let stats = index.stats();
            drop(index);
            metrics::gauge!("eris_index_images").set(stats.live as f64);
            metrics::gauge!("eris_index_tombstone_ratio")
              .set(stats.tombstones as f64 / (stats.live + stats.tombstones).max(1) as f64);
          },
          &status_tx,
        )
        .await;

      match result {
        Ok(FeedExit::HorizonBreached) => {
          tracing::warn!(
            cursor = follower.cursor(),
            "event log pruned past cursor; re-bootstrapping in background"
          );
          match bootstrap(&pool).await {
            Ok(snapshot) => {
              let live = snapshot.index.stats().live;
              *index.write() = snapshot.index;
              follower.set_cursor(snapshot.cursor);
              tracing::info!(
                images = live,
                cursor = snapshot.cursor,
                "re-bootstrap complete"
              );
            }
            Err(error) => {
              tracing::error!(%error, "re-bootstrap failed; retrying in 10s");
              tokio::time::sleep(Duration::from_secs(10)).await;
            }
          }
        }
        Err(error) => {
          tracing::error!(%error, "feed error; retrying in 5s");
          tokio::time::sleep(Duration::from_secs(5)).await;
        }
      }
    }
  });
}

fn spawn_prune_task(pool: sqlx::PgPool, interval: Duration, retention: Duration) {
  tokio::spawn(async move {
    loop {
      tokio::time::sleep(interval).await;
      match eris_store::prune::prune_events(&pool, retention).await {
        Ok(Some(deleted)) if deleted > 0 => {
          tracing::info!(deleted, "pruned event log");
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%error, "event prune failed"),
      }
    }
  });
}

async fn track_http(request: Request, next: Next) -> Response {
  let route = request
    .extensions()
    .get::<MatchedPath>()
    .map(|m| m.as_str().to_owned())
    .unwrap_or_else(|| "unmatched".to_owned());
  let response = next.run(request).await;
  metrics::counter!(
      "eris_http_requests_total",
      "route" => route,
      "status" => response.status().as_u16().to_string()
  )
  .increment(1);
  response
}

async fn shutdown_signal() {
  let ctrl_c = async {
    tokio::signal::ctrl_c()
      .await
      .expect("install ctrl-c handler");
  };
  let terminate = async {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
      .expect("install SIGTERM handler")
      .recv()
      .await;
  };
  tokio::select! {
      _ = ctrl_c => {}
      _ = terminate => {}
  }
  tracing::info!("shutdown signal received");
}

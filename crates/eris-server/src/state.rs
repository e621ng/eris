use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use eris_core::Index;
use parking_lot::RwLock;
use sqlx::PgPool;
use tokio::sync::watch;

/// Feed health as observed by this node, timestamped so /ready can detect a
/// stalled follower (errors, re-bootstrap in progress) as staleness.
#[derive(Debug, Clone, Copy)]
pub struct FeedHealth {
  pub cursor: i64,
  pub lag_seconds: f64,
  pub updated_at: Instant,
}

/// Shared server state.
///
/// Locking discipline: the index RwLock is only ever held for pure in-memory
/// work – never across an .await and never around database calls. Queries run
/// in spawn_blocking and acquire the read lock inside the blocking closure;
/// the feed task acquires the write lock only to apply already-parsed events.
pub struct AppState {
  pub index: Arc<RwLock<Index>>,
  pub pool: PgPool,
  pub feed: watch::Receiver<FeedHealth>,
  pub ready: Arc<AtomicBool>,
  pub started: Instant,
  pub token: Option<String>,
  pub ready_max_lag_s: f64,
  pub bootstrap_seconds: f64,
}

impl AppState {
  pub fn is_ready(&self) -> bool {
    if !self.ready.load(std::sync::atomic::Ordering::Relaxed) {
      return false;
    }
    let feed = *self.feed.borrow();
    // A follower that has stopped reporting (database outage or
    // re-bootstrap in progress) counts as lagging.
    let staleness = feed.updated_at.elapsed().as_secs_f64();
    feed.lag_seconds <= self.ready_max_lag_s && staleness <= self.ready_max_lag_s
  }
}

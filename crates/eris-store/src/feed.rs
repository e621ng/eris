use std::time::Duration;

use sqlx::postgres::PgListener;
use sqlx::PgPool;
use tokio::sync::watch;

use crate::events::{fetch_events, ImageEvent};
use crate::StoreError;

/// The NOTIFY channel the images trigger fires on.
pub const NOTIFY_CHANNEL: &str = "eris_events";

const BATCH: i64 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FeedStatus {
  /// Last applied event seq.
  pub cursor: i64,
  /// Age in seconds of the oldest unapplied event (0.0 when caught up).
  pub lag_seconds: f64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum FeedExit {
  /// The event log was pruned past our cursor; the caller must re-bootstrap.
  HorizonBreached,
}

pub struct Follower {
  pool: PgPool,
  cursor: i64,
  poll_interval: Duration,
}

impl Follower {
  pub fn new(pool: PgPool, cursor: i64, poll_interval: Duration) -> Self {
    Follower {
      pool,
      cursor,
      poll_interval,
    }
  }

  /// The last applied event seq. Survives `run` returning with an error, so
  /// a caller can retry without replaying.
  pub fn cursor(&self) -> i64 {
    self.cursor
  }

  /// Reset after a re-bootstrap.
  pub fn set_cursor(&mut self, cursor: i64) {
    self.cursor = cursor;
  }

  /// Tail the event log until the horizon overtakes the cursor (caller
  /// re-bootstraps) or an unrecoverable database error occurs.
  ///
  /// `apply` receives parsed event batches; it runs synchronously on this
  /// task, so the caller's locking discipline (no I/O under the index lock)
  /// is preserved by construction: all I/O happens here, before the call.
  ///
  /// LISTEN/NOTIFY is a latency optimization only – the poll tick alone is
  /// sufficient for correctness, and the follower degrades to pure polling
  /// if the listener connection fails.
  pub async fn run(
    &mut self,
    mut apply: impl FnMut(Vec<ImageEvent>),
    status: &watch::Sender<FeedStatus>,
  ) -> Result<FeedExit, StoreError> {
    let mut listener = self.try_listen().await;
    let mut tick = tokio::time::interval(self.poll_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
      // Wait for a tick or a notification, whichever comes first.
      tokio::select! {
          _ = tick.tick() => {}
          received = recv_or_pending(&mut listener) => {
              if !received {
                  listener = None; // degrade to polling; retry next cycle
              }
          }
      }
      if listener.is_none() {
        listener = self.try_listen().await;
      }

      // Drain everything currently in the log.
      loop {
        let events = fetch_events(&self.pool, self.cursor, BATCH).await?;
        let n = events.len();
        if n == 0 {
          break;
        }
        self.cursor = events.last().expect("non-empty").seq;
        apply(events);
        if (n as i64) < BATCH {
          break;
        }
      }

      // Horizon and lag in one round trip.
      let row: (i64, f64) = sqlx::query_as(
        "SELECT (SELECT prune_horizon FROM feed_meta),
                    coalesce((SELECT extract(epoch from now() - min(at))
                              FROM image_events WHERE seq > $1), 0.0)::float8",
      )
      .bind(self.cursor)
      .fetch_one(&self.pool)
      .await?;
      let (horizon, lag_seconds) = row;

      let _ = status.send(FeedStatus {
        cursor: self.cursor,
        lag_seconds,
      });

      if self.cursor < horizon {
        return Ok(FeedExit::HorizonBreached);
      }
    }
  }

  async fn try_listen(&self) -> Option<PgListener> {
    match PgListener::connect_with(&self.pool).await {
      Ok(mut listener) => match listener.listen(NOTIFY_CHANNEL).await {
        Ok(()) => Some(listener),
        Err(error) => {
          tracing::warn!(%error, "LISTEN failed; falling back to polling");
          None
        }
      },
      Err(error) => {
        tracing::warn!(%error, "listener connection failed; falling back to polling");
        None
      }
    }
  }
}

/// Resolve when a notification arrives (true) or the listener errors (false);
/// pend forever when there is no listener so the poll tick drives the loop.
async fn recv_or_pending(listener: &mut Option<PgListener>) -> bool {
  match listener {
    Some(l) => l.recv().await.is_ok(),
    None => std::future::pending().await,
  }
}

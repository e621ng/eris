use std::time::Duration;

use sqlx::PgPool;

use crate::StoreError;

/// Advisory lock key guarding the pruner (distinct from the trigger's key) so
/// N nodes running the pruner concurrently don't stampede.
const PRUNE_LOCK_KEY: i64 = 1163086164;

/// Delete events older than `retention` and advance the prune horizon.
/// Returns the number of events deleted, or None if another node holds the
/// prune lock.
pub async fn prune_events(pool: &PgPool, retention: Duration) -> Result<Option<u64>, StoreError> {
  let mut tx = pool.begin().await?;

  let got_lock: bool = sqlx::query_scalar("SELECT pg_try_advisory_xact_lock($1)")
    .bind(PRUNE_LOCK_KEY)
    .fetch_one(&mut *tx)
    .await?;
  if !got_lock {
    return Ok(None);
  }

  let horizon: i64 = sqlx::query_scalar(
    "SELECT coalesce(max(seq), 0) FROM image_events
     WHERE at < now() - make_interval(secs => $1)",
  )
  .bind(retention.as_secs_f64())
  .fetch_one(&mut *tx)
  .await?;

  if horizon == 0 {
    tx.commit().await?;
    return Ok(Some(0));
  }

  let deleted = sqlx::query("DELETE FROM image_events WHERE seq <= $1")
    .bind(horizon)
    .execute(&mut *tx)
    .await?
    .rows_affected();

  sqlx::query("UPDATE feed_meta SET prune_horizon = greatest(prune_horizon, $1)")
    .bind(horizon)
    .execute(&mut *tx)
    .await?;

  tx.commit().await?;
  Ok(Some(deleted))
}

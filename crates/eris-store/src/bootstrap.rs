use eris_core::{HaarSignature, Index};
use sqlx::{PgPool, Row};

use crate::StoreError;

pub struct Bootstrap {
  pub index: Index,
  /// The event cursor as of the snapshot: every event not reflected in the
  /// index has seq > cursor.
  pub cursor: i64,
}

/// Load the full `images` table into a fresh index inside one REPEATABLE READ
/// snapshot, and return the event cursor that snapshot corresponds to.
///
/// Correctness: the event trigger holds an advisory xact lock until commit, so
/// event seq order equals commit order. Any transaction invisible to this
/// snapshot therefore has all its events at seq > cursor – tailing from
/// `cursor` cannot miss anything, and re-applying an overlap is harmless
/// because events are idempotent.
pub async fn bootstrap(pool: &PgPool) -> Result<Bootstrap, StoreError> {
  let mut tx = pool.begin().await?;
  sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
    .execute(&mut *tx)
    .await?;

  let cursor: i64 = sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM image_events")
    .fetch_one(&mut *tx)
    .await?;

  // Rows stream through a bounded channel into a blocking builder thread,
  // capping the memory spike to a few batches while parse and index build
  // overlap with the network reads.
  let (batch_tx, batch_rx) = std::sync::mpsc::sync_channel::<Vec<(u32, HaarSignature)>>(4);
  let builder =
    tokio::task::spawn_blocking(move || Index::bulk_build(batch_rx.into_iter().flatten()));

  const PAGE: i64 = 10_000;
  let mut last_post_id: i64 = -1;
  loop {
    let rows = sqlx::query(
      "SELECT post_id, avglf1, avglf2, avglf3, sig
         FROM images WHERE post_id > $1 ORDER BY post_id LIMIT $2",
    )
    .bind(last_post_id)
    .bind(PAGE)
    .fetch_all(&mut *tx)
    .await?;

    let n = rows.len();
    if n == 0 {
      break;
    }

    let mut batch = Vec::with_capacity(n);
    for row in &rows {
      let post_id: i32 = row.try_get("post_id")?;
      let avglf = [
        row.try_get::<f64, _>("avglf1")?,
        row.try_get::<f64, _>("avglf2")?,
        row.try_get::<f64, _>("avglf3")?,
      ];
      let blob: Vec<u8> = row.try_get("sig")?;
      let sig = HaarSignature::from_blob(avglf, &blob).map_err(|e| StoreError::CorruptRow {
        post_id,
        reason: e.to_string(),
      })?;
      batch.push((post_id as u32, sig));
      last_post_id = post_id as i64;
    }

    if batch_tx.send(batch).is_err() {
      break; // builder panicked; join below surfaces it
    }
    if n < PAGE as usize {
      break;
    }
  }
  drop(batch_tx);

  let index = builder.await.expect("index builder panicked");
  tx.commit().await?;

  Ok(Bootstrap { index, cursor })
}

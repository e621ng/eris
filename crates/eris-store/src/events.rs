use eris_core::HaarSignature;
use sqlx::postgres::PgRow;
use sqlx::Row;

use crate::StoreError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOp {
  Upsert,
  Delete,
}

#[derive(Debug, Clone)]
pub struct ImageEvent {
  pub seq: i64,
  pub op: EventOp,
  pub post_id: i32,
  /// Present for upserts, absent for deletes.
  pub sig: Option<HaarSignature>,
}

pub(crate) fn event_from_row(row: &PgRow) -> Result<ImageEvent, StoreError> {
  let seq: i64 = row.try_get("seq")?;
  let op: i16 = row.try_get("op")?;
  let post_id: i32 = row.try_get("post_id")?;

  let (op, sig) = match op {
    1 => {
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
      (EventOp::Upsert, Some(sig))
    }
    2 => (EventOp::Delete, None),
    other => {
      return Err(StoreError::CorruptRow {
        post_id,
        reason: format!("unknown event op {other}"),
      })
    }
  };

  Ok(ImageEvent {
    seq,
    op,
    post_id,
    sig,
  })
}

/// Fetch up to `limit` events after `cursor`, in seq order.
pub async fn fetch_events(
  pool: &sqlx::PgPool,
  cursor: i64,
  limit: i64,
) -> Result<Vec<ImageEvent>, StoreError> {
  let rows = sqlx::query(
    "SELECT seq, op, post_id, avglf1, avglf2, avglf3, sig
     FROM image_events WHERE seq > $1 ORDER BY seq LIMIT $2",
  )
  .bind(cursor)
  .bind(limit)
  .fetch_all(pool)
  .await?;

  rows.iter().map(event_from_row).collect()
}

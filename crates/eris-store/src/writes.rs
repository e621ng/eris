use eris_core::HaarSignature;
use sqlx::PgExecutor;

use crate::StoreError;

/// Insert or replace a post's signature. The trigger records the event in the
/// same transaction.
pub async fn upsert_image<'e>(
  exec: impl PgExecutor<'e>,
  post_id: i32,
  sig: &HaarSignature,
) -> Result<(), StoreError> {
  sqlx::query(
    "INSERT INTO images (post_id, avglf1, avglf2, avglf3, sig)
     VALUES ($1, $2, $3, $4, $5)
     ON CONFLICT (post_id) DO UPDATE
         SET avglf1 = excluded.avglf1,
             avglf2 = excluded.avglf2,
             avglf3 = excluded.avglf3,
             sig = excluded.sig,
             updated_at = now()",
  )
  .bind(post_id)
  .bind(sig.avglf[0])
  .bind(sig.avglf[1])
  .bind(sig.avglf[2])
  .bind(sig.sig_blob().to_vec())
  .execute(exec)
  .await?;
  Ok(())
}

/// Delete a post's signature. Returns whether a row existed.
pub async fn delete_image<'e>(exec: impl PgExecutor<'e>, post_id: i32) -> Result<bool, StoreError> {
  let result = sqlx::query("DELETE FROM images WHERE post_id = $1")
    .bind(post_id)
    .execute(exec)
    .await?;
  Ok(result.rows_affected() > 0)
}

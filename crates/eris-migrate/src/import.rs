use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use sqlx::{PgPool, Row};

use crate::pgcopy::CopyEncoder;
use crate::sqlite::{self, SigRow};

const CHUNK_BYTES: usize = 8 << 20;
const DELTA_BATCH: usize = 1000;

#[derive(Debug)]
pub struct ImportReport {
  pub rows: u64,
  pub seconds: f64,
}

/// Full import: TRUNCATE + binary COPY with the event trigger suppressed on
/// the importing session (followers bootstrap instead of replaying millions
/// of events).
pub async fn full_import(sqlite_path: &Path, pool: &PgPool) -> Result<ImportReport> {
  let started = Instant::now();
  let mut conn = pool.acquire().await?;

  sqlx::query("SELECT set_config('eris.skip_events', 'on', false)")
    .execute(&mut *conn)
    .await?;
  sqlx::query("TRUNCATE images")
    .execute(&mut *conn)
    .await
    .context("truncate images")?;

  // Reader thread streams encoded COPY chunks; blocking_send provides
  // backpressure against the async COPY writer.
  let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
  let path: PathBuf = sqlite_path.to_owned();
  let reader = tokio::task::spawn_blocking(move || -> Result<u64> {
    let conn = sqlite::open_readonly(&path)?;
    let mut encoder = CopyEncoder::new();
    let n = sqlite::for_each_row(&conn, |row| {
      encoder.push_row(&row);
      if encoder.len() >= CHUNK_BYTES {
        tx.blocking_send(encoder.take())
          .map_err(|_| anyhow::anyhow!("copy writer dropped"))?;
      }
      Ok(())
    })?;
    let mut tail = encoder.take();
    tail.extend_from_slice(&CopyEncoder::trailer());
    tx.blocking_send(tail)
      .map_err(|_| anyhow::anyhow!("copy writer dropped"))?;
    Ok(n)
  });

  let mut copy = conn
    .copy_in_raw("COPY images (post_id, avglf1, avglf2, avglf3, sig) FROM STDIN (FORMAT binary)")
    .await?;
  while let Some(chunk) = rx.recv().await {
    copy.send(chunk).await.context("send COPY chunk")?;
  }
  copy.finish().await.context("finish COPY")?;

  let rows = reader.await.expect("reader thread panicked")?;
  Ok(ImportReport {
    rows,
    seconds: started.elapsed().as_secs_f64(),
  })
}

#[derive(Debug, Default)]
pub struct DeltaReport {
  pub upserted: u64,
  pub deleted: u64,
  pub unchanged: u64,
  pub seconds: f64,
}

/// Incremental import: streaming merge-join on post_id between the SQLite
/// source and the Postgres target (both ordered), applying only differences –
/// with the event trigger ACTIVE, so followers converge on the delta.
pub async fn incremental_import(sqlite_path: &Path, pool: &PgPool) -> Result<DeltaReport> {
  let started = Instant::now();

  // Source rows via a bounded channel from a reader thread.
  let (tx, mut rx) = tokio::sync::mpsc::channel::<SigRow>(DELTA_BATCH * 4);
  let path: PathBuf = sqlite_path.to_owned();
  let reader = tokio::task::spawn_blocking(move || -> Result<u64> {
    let conn = sqlite::open_readonly(&path)?;
    sqlite::for_each_row(&conn, |row| {
      tx.blocking_send(row)
        .map_err(|_| anyhow::anyhow!("merge task dropped"))
    })
  });

  let mut report = DeltaReport::default();
  let mut upserts: Vec<SigRow> = Vec::new();
  let mut deletes: Vec<i32> = Vec::new();

  // Target side: stream the whole table in post_id order on one connection.
  let mut target_conn = pool.acquire().await?;
  let mut target_rows =
    sqlx::query("SELECT post_id, avglf1, avglf2, avglf3, sig FROM images ORDER BY post_id")
      .fetch(&mut *target_conn);

  let mut source = rx.recv().await;
  let mut target = next_target(&mut target_rows).await?;

  loop {
    match (&source, &target) {
      (None, None) => break,
      (Some(s), None) => {
        report.upserted += 1;
        upserts.push(s.clone());
        source = rx.recv().await;
      }
      (None, Some(t)) => {
        report.deleted += 1;
        deletes.push(t.post_id);
        target = next_target(&mut target_rows).await?;
      }
      (Some(s), Some(t)) => {
        if s.post_id < t.post_id {
          report.upserted += 1;
          upserts.push(s.clone());
          source = rx.recv().await;
        } else if s.post_id > t.post_id {
          report.deleted += 1;
          deletes.push(t.post_id);
          target = next_target(&mut target_rows).await?;
        } else {
          // Bit-compare avglf and the blob; upsert only real change.
          let same = s.sig == t.sig
            && s
              .avglf
              .iter()
              .zip(t.avglf.iter())
              .all(|(a, b)| a.to_bits() == b.to_bits());
          if same {
            report.unchanged += 1;
          } else {
            report.upserted += 1;
            upserts.push(s.clone());
          }
          source = rx.recv().await;
          target = next_target(&mut target_rows).await?;
        }
      }
    }

    if upserts.len() >= DELTA_BATCH {
      flush_upserts(pool, &mut upserts).await?;
    }
    if deletes.len() >= DELTA_BATCH {
      flush_deletes(pool, &mut deletes).await?;
    }
  }
  drop(target_rows);
  flush_upserts(pool, &mut upserts).await?;
  flush_deletes(pool, &mut deletes).await?;

  reader.await.expect("reader thread panicked")?;
  report.seconds = started.elapsed().as_secs_f64();
  Ok(report)
}

async fn next_target(
  stream: &mut (impl futures_util::Stream<Item = Result<sqlx::postgres::PgRow, sqlx::Error>> + Unpin),
) -> Result<Option<SigRow>> {
  match stream.next().await {
    None => Ok(None),
    Some(row) => {
      let row = row?;
      Ok(Some(SigRow {
        post_id: row.try_get("post_id")?,
        avglf: [
          row.try_get("avglf1")?,
          row.try_get("avglf2")?,
          row.try_get("avglf3")?,
        ],
        sig: row.try_get("sig")?,
      }))
    }
  }
}

async fn flush_upserts(pool: &PgPool, rows: &mut Vec<SigRow>) -> Result<()> {
  if rows.is_empty() {
    return Ok(());
  }
  let post_ids: Vec<i32> = rows.iter().map(|r| r.post_id).collect();
  let a1: Vec<f64> = rows.iter().map(|r| r.avglf[0]).collect();
  let a2: Vec<f64> = rows.iter().map(|r| r.avglf[1]).collect();
  let a3: Vec<f64> = rows.iter().map(|r| r.avglf[2]).collect();
  let sigs: Vec<Vec<u8>> = rows.iter().map(|r| r.sig.clone()).collect();
  sqlx::query(
    "INSERT INTO images (post_id, avglf1, avglf2, avglf3, sig)
     SELECT * FROM UNNEST($1::int4[], $2::float8[], $3::float8[], $4::float8[], $5::bytea[])
     ON CONFLICT (post_id) DO UPDATE
         SET avglf1 = excluded.avglf1, avglf2 = excluded.avglf2,
             avglf3 = excluded.avglf3, sig = excluded.sig, updated_at = now()",
  )
  .bind(post_ids)
  .bind(a1)
  .bind(a2)
  .bind(a3)
  .bind(sigs)
  .execute(pool)
  .await?;
  rows.clear();
  Ok(())
}

async fn flush_deletes(pool: &PgPool, post_ids: &mut Vec<i32>) -> Result<()> {
  if post_ids.is_empty() {
    return Ok(());
  }
  sqlx::query("DELETE FROM images WHERE post_id = ANY($1)")
    .bind(std::mem::take(post_ids))
    .execute(pool)
    .await?;
  Ok(())
}

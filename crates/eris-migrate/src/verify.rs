use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use eris_core::{hash, HaarSignature};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use sqlx::{PgPool, Row};

use crate::sqlite;

#[derive(Debug)]
pub struct VerifyReport {
  pub sqlite_rows: u64,
  pub pg_rows: u64,
  pub sampled: usize,
  pub mismatches: usize,
}

/// Compare row counts exactly and a seeded sample of full hash strings
/// end-to-end (this covers the avglf f64 bits and every blob byte at once).
pub async fn verify(
  sqlite_path: &Path,
  pool: &PgPool,
  sample: usize,
  seed: u64,
) -> Result<VerifyReport> {
  let conn = sqlite::open_readonly(sqlite_path)?;
  let sqlite_rows = sqlite::count(&conn)?;
  let pg_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM images")
    .fetch_one(pool)
    .await?;

  // Collect (post_id -> expected hash) for a random sample.
  let mut post_ids: Vec<i32> = Vec::with_capacity(sqlite_rows as usize);
  sqlite::for_each_row(&conn, |row| {
    post_ids.push(row.post_id);
    Ok(())
  })?;
  let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
  post_ids.shuffle(&mut rng);
  post_ids.truncate(sample);
  post_ids.sort_unstable();

  let mut expected: HashMap<i32, String> = HashMap::with_capacity(post_ids.len());
  {
    let mut stmt =
      conn.prepare("SELECT avglf1, avglf2, avglf3, sig FROM images WHERE post_id = ?1")?;
    for &post_id in &post_ids {
      let (a1, a2, a3, sig): (f64, f64, f64, Vec<u8>) = stmt.query_row([post_id], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
      })?;
      let sig = HaarSignature::from_blob([a1, a2, a3], &sig)
        .map_err(|e| anyhow::anyhow!("sqlite post {post_id}: {e}"))?;
      expected.insert(post_id, hash::encode(&sig));
    }
  }

  let rows =
    sqlx::query("SELECT post_id, avglf1, avglf2, avglf3, sig FROM images WHERE post_id = ANY($1)")
      .bind(&post_ids)
      .fetch_all(pool)
      .await?;

  let mut mismatches = 0usize;
  let mut seen = 0usize;
  for row in rows {
    let post_id: i32 = row.try_get("post_id")?;
    let avglf = [
      row.try_get::<f64, _>("avglf1")?,
      row.try_get::<f64, _>("avglf2")?,
      row.try_get::<f64, _>("avglf3")?,
    ];
    let blob: Vec<u8> = row.try_get("sig")?;
    let sig = HaarSignature::from_blob(avglf, &blob)
      .map_err(|e| anyhow::anyhow!("pg post {post_id}: {e}"))?;
    let got = hash::encode(&sig);
    seen += 1;
    if expected.get(&post_id) != Some(&got) {
      mismatches += 1;
      tracing::error!(post_id, "hash mismatch between sqlite and postgres");
    }
  }
  if seen != post_ids.len() {
    bail!(
      "sampled {} post ids but postgres returned {seen}",
      post_ids.len()
    );
  }

  Ok(VerifyReport {
    sqlite_rows,
    pg_rows: pg_rows as u64,
    sampled: seen,
    mismatches,
  })
}

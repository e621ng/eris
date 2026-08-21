//! Query-corpus and subsample generation from a production SQLite snapshot.

use std::io::Write;
use std::path::Path;

use anyhow::Result;
use eris_core::{hash, HaarSignature};
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::{Deserialize, Serialize};

use crate::sqlite::{self, SigRow};

#[derive(Debug, Serialize, Deserialize)]
pub struct CorpusEntry {
  pub hash: String,
  pub kind: String,
}

fn stratum(row: &SigRow) -> &'static str {
  let chroma = row.avglf[1].abs() + row.avglf[2].abs();
  if chroma < 0.006 {
    "grayscale"
  } else if chroma < 0.007 {
    "near_threshold"
  } else if row.avglf[0] < 0.05 {
    "near_black"
  } else {
    "random"
  }
}

/// Sample a stratified query corpus: plain random rows plus every row of the
/// interesting strata (grayscale detection boundary, near-black), capped per
/// stratum. Deterministic under `seed`.
pub fn generate(sqlite_path: &Path, out: &Path, n: usize, seed: u64) -> Result<()> {
  let conn = sqlite::open_readonly(sqlite_path)?;

  let mut by_stratum: std::collections::HashMap<&'static str, Vec<SigRow>> = Default::default();
  sqlite::for_each_row(&conn, |row| {
    by_stratum.entry(stratum(&row)).or_default().push(row);
    Ok(())
  })?;

  let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
  // Random gets half the budget; the special strata split the rest.
  let strata = ["random", "grayscale", "near_threshold", "near_black"];
  let budgets = [n / 2, n / 6, n / 6, n / 6];

  let mut entries: Vec<CorpusEntry> = Vec::new();
  for (name, budget) in strata.iter().zip(budgets) {
    let Some(rows) = by_stratum.get_mut(*name) else {
      continue;
    };
    rows.shuffle(&mut rng);
    for row in rows.iter().take(budget.max(1)) {
      let sig = HaarSignature::from_blob(row.avglf, &row.sig)
        .map_err(|e| anyhow::anyhow!("post {}: {e}", row.post_id))?;
      entries.push(CorpusEntry {
        hash: hash::encode(&sig),
        kind: name.to_string(),
      });
    }
  }
  // Synthetic images cover the transform itself (incl. pure black).
  for case in 0..eris_core::testimages::NUM_CASES {
    let [r, g, b] = eris_core::testimages::case(case);
    let sig = eris_core::from_channels(&r, &g, &b);
    entries.push(CorpusEntry {
      hash: hash::encode(&sig),
      kind: format!("synthetic_{case}"),
    });
  }

  let mut file = std::io::BufWriter::new(std::fs::File::create(out)?);
  for entry in &entries {
    writeln!(file, "{}", serde_json::to_string(entry)?)?;
  }
  file.flush()?;
  println!(
    "wrote {} corpus entries to {}",
    entries.len(),
    out.display()
  );
  Ok(())
}

pub fn load(path: &Path) -> Result<Vec<CorpusEntry>> {
  let content = std::fs::read_to_string(path)?;
  content
    .lines()
    .filter(|l| !l.trim().is_empty())
    .map(|l| Ok(serde_json::from_str(l)?))
    .collect()
}

/// Write a deterministic random subsample of the production database as a
/// standalone SQLite file with the same schema (used as the CI fixture that
/// both the C++ oracle and ERIS index).
pub fn subsample(sqlite_path: &Path, out: &Path, n: usize, seed: u64) -> Result<()> {
  let conn = sqlite::open_readonly(sqlite_path)?;
  let mut rows: Vec<SigRow> = Vec::new();
  sqlite::for_each_row(&conn, |row| {
    rows.push(row);
    Ok(())
  })?;
  let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
  rows.shuffle(&mut rng);
  rows.truncate(n);
  rows.sort_unstable_by_key(|r| r.post_id);

  if out.exists() {
    std::fs::remove_file(out)?;
  }
  let mut dest = rusqlite::Connection::open(out)?;
  // Match the C++ sqlite_orm schema exactly so the oracle can open it.
  dest.execute_batch(
    "CREATE TABLE images (
        id INTEGER PRIMARY KEY NOT NULL,
        post_id INTEGER UNIQUE NOT NULL,
        avglf1 REAL NOT NULL, avglf2 REAL NOT NULL, avglf3 REAL NOT NULL,
        sig BLOB NOT NULL
    )",
  )?;
  let tx = dest.transaction()?;
  {
    let mut stmt = tx.prepare(
      "INSERT INTO images (id, post_id, avglf1, avglf2, avglf3, sig)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (i, row) in rows.iter().enumerate() {
      stmt.execute(rusqlite::params![
        i as i64 + 1,
        row.post_id,
        row.avglf[0],
        row.avglf[1],
        row.avglf[2],
        row.sig,
      ])?;
    }
  }
  tx.commit()?;
  println!("wrote {} rows to {}", rows.len(), out.display());
  Ok(())
}

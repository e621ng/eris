//! The differential harness: replay a query corpus against the C++ oracle and
//! an ERIS node, and fail on any divergence outside the accepted classes.
//!
//! Accepted divergences (documented in the design doc):
//! - subject-only results whose avglf[0] is bitwise 0.0 (pure-black images
//!   are findable in ERIS, invisible in the C++);
//! - rank permutations / boundary substitutions among results whose scores
//!   are within EPSILON of each other (tie order at the limit cutoff).

use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::json;

use crate::corpus;

const EPSILON: f64 = 0.01;

#[derive(Debug, Clone, Serialize)]
struct Entry {
  post_id: i64,
  score: f64,
  avglf1_bits: u64,
}

#[derive(Debug, Serialize)]
pub struct QueryVerdict {
  pub hash: String,
  pub kind: String,
  pub ok: bool,
  pub max_delta: f64,
  pub problems: Vec<String>,
  oracle: Vec<Entry>,
  subject: Vec<Entry>,
}

#[derive(Debug, Serialize)]
pub struct DiffReport {
  pub queries: usize,
  pub failed: usize,
  pub max_delta: f64,
  pub verdicts: Vec<QueryVerdict>,
}

async fn run_query(
  client: &reqwest::Client,
  base: &str,
  hash: &str,
  limit: usize,
) -> Result<Vec<Entry>> {
  let response = client
    .post(format!("{base}/query"))
    .json(&json!({ "hash": hash, "limit": limit }))
    .send()
    .await
    .with_context(|| format!("query against {base}"))?;
  anyhow::ensure!(
    response.status().is_success(),
    "{base} returned {}",
    response.status()
  );
  let body: serde_json::Value = response.json().await?;
  let arr = body.as_array().context("query response is not an array")?;
  arr
    .iter()
    .map(|entry| {
      let post_id = entry["post_id"].as_i64().context("post_id")?;
      let score = entry["score"].as_f64().context("score")?;
      // avglf[0] rides in the first 16 hex chars of the entry hash.
      let hash = entry["hash"].as_str().context("hash")?;
      let avglf1_bits = u64::from_str_radix(hash.get(0..16).context("short hash")?, 16)?;
      Ok(Entry {
        post_id,
        score,
        avglf1_bits,
      })
    })
    .collect()
}

fn judge(hash: &str, kind: &str, oracle: Vec<Entry>, subject: Vec<Entry>) -> QueryVerdict {
  let mut problems = Vec::new();
  let mut max_delta = 0.0f64;

  // Pure-black entries (avglf[0] bitwise 0.0) exist only on the subject
  // side – the C++ can never return them – and they displace the oracle's
  // tail out of the subject's limited top-K. All boundary accounting
  // therefore excludes them.
  let nonblack_min = subject
    .iter()
    .filter(|e| e.avglf1_bits != 0)
    .map(|e| e.score)
    .fold(f64::INFINITY, f64::min);
  let oracle_min = oracle.iter().map(|e| e.score).fold(f64::INFINITY, f64::min);

  for o in &oracle {
    match subject.iter().find(|s| s.post_id == o.post_id) {
      Some(s) => {
        let delta = (s.score - o.score).abs();
        max_delta = max_delta.max(delta);
        if delta > EPSILON {
          problems.push(format!(
            "post {}: score {} (oracle) vs {} (subject)",
            o.post_id, o.score, s.score
          ));
        }
      }
      None => {
        // Tail displacement (by pure-black extras) or a boundary tie
        // is fine; a real divergence is the subject keeping a
        // strictly worse non-black entry while omitting this one.
        if nonblack_min.is_finite() && o.score > nonblack_min + EPSILON {
          problems.push(format!(
            "post {} (score {}) missing from subject results",
            o.post_id, o.score
          ));
        }
      }
    }
  }
  for s in &subject {
    if s.avglf1_bits == 0 {
      continue; // documented divergence: pure-black findable in ERIS
    }
    if oracle.iter().any(|o| o.post_id == s.post_id) {
      continue;
    }
    // The oracle omitting an entry better than its own worst kept result
    // would be a real divergence; at or below the boundary it's a tie.
    if oracle_min.is_finite() && s.score > oracle_min + EPSILON {
      problems.push(format!(
        "post {} (score {}) present only in subject results",
        s.post_id, s.score
      ));
    }
  }

  QueryVerdict {
    hash: hash.to_owned(),
    kind: kind.to_owned(),
    ok: problems.is_empty(),
    max_delta,
    problems,
    oracle,
    subject,
  }
}

pub async fn run(
  corpus_path: &Path,
  oracle_url: &str,
  subject_url: &str,
  limit: usize,
  report_path: Option<&Path>,
) -> Result<DiffReport> {
  let entries = corpus::load(corpus_path)?;
  let client = reqwest::Client::new();

  let mut verdicts = Vec::with_capacity(entries.len());
  for entry in &entries {
    let oracle = run_query(&client, oracle_url, &entry.hash, limit).await?;
    let subject = run_query(&client, subject_url, &entry.hash, limit).await?;
    verdicts.push(judge(&entry.hash, &entry.kind, oracle, subject));
  }

  let failed = verdicts.iter().filter(|v| !v.ok).count();
  let max_delta = verdicts.iter().map(|v| v.max_delta).fold(0.0, f64::max);
  let report = DiffReport {
    queries: verdicts.len(),
    failed,
    max_delta,
    verdicts,
  };

  if let Some(path) = report_path {
    std::fs::write(path, serde_json::to_string_pretty(&report)?)?;
  }
  println!(
    "differential: {} queries, {} failed, max |Δscore| = {:.6}",
    report.queries, report.failed, report.max_delta
  );
  for verdict in report.verdicts.iter().filter(|v| !v.ok).take(10) {
    println!("FAIL [{}] {}...", verdict.kind, &verdict.hash[..24]);
    for problem in verdict.problems.iter().take(5) {
      println!("  {problem}");
    }
  }
  Ok(report)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn entry(post_id: i64, score: f64, black: bool) -> Entry {
    Entry {
      post_id,
      score,
      avglf1_bits: if black { 0 } else { 0x3fe0000000000000 },
    }
  }

  #[test]
  fn identical_results_pass() {
    let list = vec![entry(1, 99.0, false), entry(2, 80.0, false)];
    let v = judge("h", "k", list.clone(), list);
    assert!(v.ok, "{:?}", v.problems);
  }

  #[test]
  fn score_drift_fails() {
    let v = judge(
      "h",
      "k",
      vec![entry(1, 99.0, false)],
      vec![entry(1, 98.5, false)],
    );
    assert!(!v.ok);
  }

  #[test]
  fn pure_black_displacement_accepted() {
    // Subject's top-K holds black entries; the oracle's tail (worse than
    // the subject's worst non-black) is displaced out. Accepted.
    let oracle = vec![entry(1, 99.9, false), entry(2, 99.5, false)];
    let subject = vec![entry(10, 100.0, true), entry(1, 99.9, false)];
    let v = judge("h", "k", oracle, subject);
    assert!(v.ok, "{:?}", v.problems);
  }

  #[test]
  fn fully_black_subject_accepted() {
    let oracle = vec![entry(1, 99.9, false)];
    let subject = vec![entry(10, 100.0, true), entry(11, 100.0, true)];
    let v = judge("h", "k", oracle, subject);
    assert!(v.ok, "{:?}", v.problems);
  }

  #[test]
  fn genuinely_missing_entry_fails() {
    // The subject kept something worse (80.0) while omitting 99.5:
    // that's a real divergence, not displacement.
    let oracle = vec![entry(1, 99.9, false), entry(2, 99.5, false)];
    let subject = vec![entry(1, 99.9, false), entry(3, 80.0, false)];
    let v = judge("h", "k", oracle, subject);
    assert!(!v.ok);
    // ...and both directions trip: 2 missing, 3 subject-only above the
    // oracle's worst? (3 scores 80.0 < oracle_min 99.5: not flagged.)
    assert_eq!(v.problems.len(), 1);
  }

  #[test]
  fn subject_only_better_than_oracle_worst_fails() {
    let oracle = vec![entry(1, 99.9, false), entry(2, 90.0, false)];
    let subject = vec![entry(1, 99.9, false), entry(3, 95.0, false)];
    let v = judge("h", "k", oracle, subject);
    assert!(!v.ok);
  }
}

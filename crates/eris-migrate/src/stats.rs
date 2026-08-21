//! M-stats: build the full index from a SQLite snapshot in-process and report
//! memory, build time, and query latency – the measurement behind the RSS and
//! bootstrap acceptance gates, runnable without Postgres or a server.

use std::path::Path;
use std::time::Instant;

use anyhow::Result;
use eris_core::{HaarSignature, Index};

use crate::sqlite;

pub fn run(sqlite_path: &Path, sample_queries: usize) -> Result<()> {
  let conn = sqlite::open_readonly(sqlite_path)?;
  let total = sqlite::count(&conn)?;
  println!("rows in sqlite: {total}");

  let started = Instant::now();
  let mut rows: Vec<(u32, HaarSignature)> = Vec::with_capacity(total as usize);
  sqlite::for_each_row(&conn, |row| {
    let sig = HaarSignature::from_blob(row.avglf, &row.sig)
      .map_err(|e| anyhow::anyhow!("post {}: {e}", row.post_id))?;
    rows.push((row.post_id as u32, sig));
    Ok(())
  })?;
  let read_seconds = started.elapsed().as_secs_f64();
  println!("sqlite read + parse: {read_seconds:.1}s");

  // Keep some query probes before the rows move into the index.
  let probes: Vec<HaarSignature> = rows
    .iter()
    .step_by((rows.len() / sample_queries.max(1)).max(1))
    .take(sample_queries)
    .map(|(_, sig)| sig.clone())
    .collect();

  let started = Instant::now();
  let index = Index::bulk_build(rows);
  let build_seconds = started.elapsed().as_secs_f64();
  let stats = index.stats();
  println!(
    "index build: {build_seconds:.1}s ({} images, {} chunks)",
    stats.live, stats.chunks
  );
  println!(
    "index heap estimate: {:.2} GiB",
    stats.heap_bytes as f64 / (1 << 30) as f64
  );
  if let Some(rss) = rss_bytes() {
    println!("process RSS: {:.2} GiB", rss as f64 / (1 << 30) as f64);
  }

  if !probes.is_empty() {
    let mut hist = hdrhistogram::Histogram::<u64>::new(3)?;
    for sig in &probes {
      let started = Instant::now();
      let results = eris_core::query(&index, sig, 10, None);
      hist.record(started.elapsed().as_micros() as u64)?;
      assert!(!results.is_empty());
    }
    println!(
      "query latency over {} probes: p50 {:.1}ms p90 {:.1}ms p99 {:.1}ms max {:.1}ms",
      probes.len(),
      hist.value_at_quantile(0.50) as f64 / 1000.0,
      hist.value_at_quantile(0.90) as f64 / 1000.0,
      hist.value_at_quantile(0.99) as f64 / 1000.0,
      hist.max() as f64 / 1000.0,
    );
  }
  Ok(())
}

/// VmRSS from /proc/self/status, Linux only.
pub fn rss_bytes() -> Option<u64> {
  let status = std::fs::read_to_string("/proc/self/status").ok()?;
  let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
  let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
  Some(kb * 1024)
}

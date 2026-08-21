//! Load generation and convergence measurement against a running ERIS node.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::json;

use crate::corpus;

pub struct BenchOptions {
  pub subject_url: String,
  pub corpus_path: std::path::PathBuf,
  pub concurrency: usize,
  pub duration: Duration,
  pub limit: usize,
}

/// Sustained query load: `concurrency` workers looping over the corpus.
pub async fn run(options: BenchOptions) -> Result<()> {
  let hashes: Vec<String> = corpus::load(&options.corpus_path)?
    .into_iter()
    .map(|e| e.hash)
    .collect();
  anyhow::ensure!(!hashes.is_empty(), "empty corpus");
  let hashes = Arc::new(hashes);

  let counter = Arc::new(AtomicU64::new(0));
  let errors = Arc::new(AtomicU64::new(0));
  let deadline = Instant::now() + options.duration;
  let started = Instant::now();

  let mut workers = Vec::new();
  for worker_id in 0..options.concurrency {
    let hashes = Arc::clone(&hashes);
    let counter = Arc::clone(&counter);
    let errors = Arc::clone(&errors);
    let url = format!("{}/query", options.subject_url);
    let limit = options.limit;
    workers.push(tokio::spawn(async move {
      let client = reqwest::Client::new();
      let mut hist = hdrhistogram::Histogram::<u64>::new(3).expect("histogram");
      let mut i = worker_id;
      while Instant::now() < deadline {
        let hash = &hashes[i % hashes.len()];
        i += options.concurrency.max(1);
        let request_started = Instant::now();
        let response = client
          .post(&url)
          .json(&json!({ "hash": hash, "limit": limit }))
          .send()
          .await;
        match response {
          Ok(r) if r.status().is_success() => {
            let _ = r.bytes().await;
            hist
              .record(request_started.elapsed().as_micros() as u64)
              .ok();
            counter.fetch_add(1, Ordering::Relaxed);
          }
          _ => {
            errors.fetch_add(1, Ordering::Relaxed);
          }
        }
      }
      hist
    }));
  }

  let mut merged = hdrhistogram::Histogram::<u64>::new(3)?;
  for worker in workers {
    merged.add(worker.await.expect("worker panicked"))?;
  }

  let elapsed = started.elapsed().as_secs_f64();
  let total = counter.load(Ordering::Relaxed);
  let failed = errors.load(Ordering::Relaxed);
  println!(
    "{} queries in {elapsed:.1}s = {:.1} qps ({} errors) at concurrency {}",
    total,
    total as f64 / elapsed,
    failed,
    options.concurrency
  );
  let ms = |q: f64| merged.value_at_quantile(q) as f64 / 1000.0;
  println!(
    "latency: p50 {:.1}ms p90 {:.1}ms p99 {:.1}ms max {:.1}ms",
    ms(0.50),
    ms(0.90),
    ms(0.99),
    merged.max() as f64 / 1000.0
  );
  anyhow::ensure!(failed == 0, "{failed} requests failed");
  Ok(())
}

/// Write-to-visible convergence: POST an image to `write_url`, poll
/// `read_url` until it appears, repeat. Reports the distribution.
pub async fn convergence(write_url: &str, read_url: &str, iterations: usize) -> Result<()> {
  let client = reqwest::Client::new();
  let mut hist = hdrhistogram::Histogram::<u64>::new(3)?;

  for i in 0..iterations {
    let post_id = 990_000_000 + i as u32;
    let [r, g, b] = eris_core::testimages::case(i as u32 % eris_core::testimages::NUM_CASES);
    let body = json!({ "channels": { "r": r, "g": g, "b": b } });
    let started = Instant::now();
    let response = client
      .post(format!("{write_url}/images/{post_id}"))
      .json(&body)
      .send()
      .await
      .context("write")?;
    anyhow::ensure!(
      response.status().is_success(),
      "write failed: {}",
      response.status()
    );

    loop {
      let response = client
        .get(format!("{read_url}/images/{post_id}"))
        .send()
        .await
        .context("read")?;
      if response.status().is_success() {
        break;
      }
      anyhow::ensure!(
        started.elapsed() < Duration::from_secs(30),
        "post {post_id} not visible after 30s"
      );
      tokio::time::sleep(Duration::from_millis(20)).await;
    }
    hist.record(started.elapsed().as_millis() as u64)?;

    // Clean up (and let deletes exercise the path too).
    client
      .delete(format!("{write_url}/images/{post_id}"))
      .send()
      .await
      .context("cleanup delete")?;
  }

  println!(
    "write→visible over {iterations} iterations: p50 {}ms p90 {}ms p99 {}ms max {}ms",
    hist.value_at_quantile(0.50),
    hist.value_at_quantile(0.90),
    hist.value_at_quantile(0.99),
    hist.max()
  );
  Ok(())
}

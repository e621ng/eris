use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod bench;
mod corpus;
mod diff;
mod fixtures;
mod import;
mod pgcopy;
mod sqlite;
mod stats;
mod verify;

#[derive(Parser)]
#[command(
  name = "eris-migrate",
  version,
  about = "ERIS migration and verification tooling"
)]
struct Cli {
  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Import a C++ IQDB SQLite database into Postgres.
  Import {
    #[arg(long)]
    sqlite: PathBuf,
    #[arg(long, env = "ERIS_DATABASE_URL")]
    database_url: String,
    /// Merge-join delta sync instead of TRUNCATE + COPY; events fire so
    /// running followers converge on the difference.
    #[arg(long)]
    incremental: bool,
  },
  /// Verify Postgres contents against the SQLite source.
  Verify {
    #[arg(long)]
    sqlite: PathBuf,
    #[arg(long, env = "ERIS_DATABASE_URL")]
    database_url: String,
    #[arg(long, default_value_t = 10_000)]
    sample: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
  },
  /// Build the full index in-process and report memory/latency (M-stats).
  Stats {
    #[arg(long)]
    sqlite: PathBuf,
    #[arg(long, default_value_t = 100)]
    sample_queries: usize,
  },
  /// Generate a stratified query corpus (JSONL of hashes).
  Corpus {
    #[arg(long)]
    sqlite: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 10_000)]
    n: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
  },
  /// Write a deterministic SQLite subsample (CI fixture).
  Subsample {
    #[arg(long)]
    sqlite: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value_t = 10_000)]
    n: usize,
    #[arg(long, default_value_t = 42)]
    seed: u64,
  },
  /// Record golden signature fixtures from the C++ oracle.
  Fixtures {
    #[arg(long, default_value = "http://127.0.0.1:5588")]
    oracle: String,
    #[arg(long, default_value = "testdata/golden_channels.json")]
    out: PathBuf,
  },
  /// Differential run: corpus vs oracle vs subject.
  Diff {
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long)]
    oracle: String,
    #[arg(long)]
    subject: String,
    #[arg(long, default_value_t = 20)]
    limit: usize,
    #[arg(long)]
    report: Option<PathBuf>,
  },
  /// Sustained query load against a running node.
  Bench {
    #[arg(long)]
    subject: String,
    #[arg(long)]
    corpus: PathBuf,
    #[arg(long, default_value_t = 50)]
    concurrency: usize,
    #[arg(long, default_value_t = 60)]
    duration_s: u64,
    #[arg(long, default_value_t = 10)]
    limit: usize,
  },
  /// Write-to-visible convergence measurement (write to one node, read
  /// from another).
  Convergence {
    #[arg(long)]
    write: String,
    #[arg(long)]
    read: String,
    #[arg(long, default_value_t = 100)]
    iterations: usize,
  },
}

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,sqlx=warn".into()),
    )
    .init();

  match Cli::parse().command {
    Command::Import {
      sqlite,
      database_url,
      incremental,
    } => {
      let pool = eris_store::connect(&database_url, 5).await?;
      eris_store::migrate(&pool).await?;
      if incremental {
        let report = import::incremental_import(&sqlite, &pool).await?;
        println!(
          "incremental import: {} upserted, {} deleted, {} unchanged in {:.1}s",
          report.upserted, report.deleted, report.unchanged, report.seconds
        );
      } else {
        let report = import::full_import(&sqlite, &pool).await?;
        println!(
          "full import: {} rows in {:.1}s",
          report.rows, report.seconds
        );
      }
    }
    Command::Verify {
      sqlite,
      database_url,
      sample,
      seed,
    } => {
      let pool = eris_store::connect(&database_url, 5).await?;
      let report = verify::verify(&sqlite, &pool, sample, seed).await?;
      println!(
        "verify: sqlite={} pg={} sampled={} mismatches={}",
        report.sqlite_rows, report.pg_rows, report.sampled, report.mismatches
      );
      anyhow::ensure!(
        report.sqlite_rows == report.pg_rows && report.mismatches == 0,
        "verification FAILED"
      );
      println!("verification OK");
    }
    Command::Stats {
      sqlite,
      sample_queries,
    } => {
      stats::run(&sqlite, sample_queries)?;
    }
    Command::Corpus {
      sqlite,
      out,
      n,
      seed,
    } => {
      corpus::generate(&sqlite, &out, n, seed)?;
    }
    Command::Subsample {
      sqlite,
      out,
      n,
      seed,
    } => {
      corpus::subsample(&sqlite, &out, n, seed)?;
    }
    Command::Fixtures { oracle, out } => {
      fixtures::generate(&oracle, &out).await?;
    }
    Command::Diff {
      corpus,
      oracle,
      subject,
      limit,
      report,
    } => {
      let result = diff::run(&corpus, &oracle, &subject, limit, report.as_deref()).await?;
      anyhow::ensure!(result.failed == 0, "{} queries diverged", result.failed);
    }
    Command::Bench {
      subject,
      corpus,
      concurrency,
      duration_s,
      limit,
    } => {
      bench::run(bench::BenchOptions {
        subject_url: subject,
        corpus_path: corpus,
        concurrency,
        duration: Duration::from_secs(duration_s),
        limit,
      })
      .await?;
    }
    Command::Convergence {
      write,
      read,
      iterations,
    } => {
      bench::convergence(&write, &read, iterations).await?;
    }
  }
  Ok(())
}

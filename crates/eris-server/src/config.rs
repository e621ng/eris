use clap::Parser;

/// ERIS server configuration; every flag doubles as an ERIS_* env var.
#[derive(Parser, Debug, Clone)]
#[command(name = "eris-server", version, about = "E621 Reverse Image Search")]
pub struct Config {
  /// Postgres connection URL.
  #[arg(long, env = "ERIS_DATABASE_URL")]
  pub database_url: String,

  /// Address to listen on.
  #[arg(long, env = "ERIS_LISTEN", default_value = "0.0.0.0:5588")]
  pub listen: String,

  /// Bearer token. Unset = auth disabled (drop-in compatibility with an
  /// unmodified IqdbProxy, which sends no credentials).
  #[arg(long, env = "ERIS_TOKEN")]
  pub token: Option<String>,

  /// Event feed poll interval in milliseconds (NOTIFY wakes it earlier).
  #[arg(long, env = "ERIS_FEED_INTERVAL_MS", default_value_t = 2000)]
  pub feed_interval_ms: u64,

  /// /ready reports unready when feed lag exceeds this many seconds.
  #[arg(long, env = "ERIS_READY_MAX_LAG_S", default_value_t = 30.0)]
  pub ready_max_lag_s: f64,

  /// Maximum request body size in bytes.
  #[arg(long, env = "ERIS_BODY_LIMIT", default_value_t = 1024 * 1024)]
  pub body_limit: usize,

  /// Postgres connection pool size.
  #[arg(long, env = "ERIS_DB_MAX_CONNS", default_value_t = 10)]
  pub db_max_conns: u32,

  /// Run schema migrations at startup.
  #[arg(long, env = "ERIS_MIGRATE", default_value_t = true, action = clap::ArgAction::Set)]
  pub migrate: bool,

  /// Seconds between event-log prune attempts.
  #[arg(long, env = "ERIS_PRUNE_INTERVAL_S", default_value_t = 3600)]
  pub prune_interval_s: u64,

  /// Event retention window in seconds (default 7 days).
  #[arg(long, env = "ERIS_EVENT_RETENTION_S", default_value_t = 7 * 24 * 3600)]
  pub event_retention_s: u64,
}

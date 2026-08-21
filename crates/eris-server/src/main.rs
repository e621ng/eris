use clap::Parser;
use eris_server::{config, lifecycle};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,sqlx=warn".into()),
    )
    .init();

  let config = config::Config::parse();
  lifecycle::serve(config).await
}

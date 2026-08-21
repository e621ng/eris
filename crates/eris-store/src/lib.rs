//! eris-store: Postgres persistence – schema, writes, snapshot bootstrap, and
//! the trigger-fed event feed that keeps every node's index converged.

pub mod bootstrap;
pub mod events;
pub mod feed;
pub mod prune;
pub mod writes;

use sqlx::postgres::{PgPool, PgPoolOptions};

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
  #[error("database error: {0}")]
  Sqlx(#[from] sqlx::Error),
  #[error("migration error: {0}")]
  Migrate(#[from] sqlx::migrate::MigrateError),
  #[error("corrupt row for post {post_id}: {reason}")]
  CorruptRow { post_id: i32, reason: String },
}

pub async fn connect(url: &str, max_connections: u32) -> Result<PgPool, StoreError> {
  Ok(
    PgPoolOptions::new()
      .max_connections(max_connections)
      .connect(url)
      .await?,
  )
}

/// Apply the schema migrations (embedded at compile time).
pub async fn migrate(pool: &PgPool) -> Result<(), StoreError> {
  sqlx::migrate!("./migrations").run(pool).await?;
  Ok(())
}

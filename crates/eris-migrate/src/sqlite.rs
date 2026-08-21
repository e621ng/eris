use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};

/// One row of the legacy C++ IQDB SQLite database.
#[derive(Debug, Clone)]
pub struct SigRow {
  pub post_id: i32,
  pub avglf: [f64; 3],
  pub sig: Vec<u8>,
}

pub fn open_readonly(path: &Path) -> Result<Connection> {
  Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
    .with_context(|| format!("open sqlite database {}", path.display()))
}

pub fn count(conn: &Connection) -> Result<u64> {
  Ok(conn.query_row("SELECT count(*) FROM images", [], |row| {
    row.get::<_, i64>(0)
  })? as u64)
}

/// Visit every row in post_id order. Returns the number of rows visited.
pub fn for_each_row(conn: &Connection, mut f: impl FnMut(SigRow) -> Result<()>) -> Result<u64> {
  let mut stmt =
    conn.prepare("SELECT post_id, avglf1, avglf2, avglf3, sig FROM images ORDER BY post_id")?;
  let mut rows = stmt.query([])?;
  let mut n = 0u64;
  while let Some(row) = rows.next()? {
    let sig_row = SigRow {
      post_id: row.get(0)?,
      avglf: [row.get(1)?, row.get(2)?, row.get(3)?],
      sig: row.get(4)?,
    };
    anyhow::ensure!(
      sig_row.sig.len() == 240,
      "post {} has a {}-byte sig blob",
      sig_row.post_id,
      sig_row.sig.len()
    );
    f(sig_row)?;
    n += 1;
  }
  Ok(n)
}

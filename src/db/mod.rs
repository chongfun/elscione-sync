pub mod models;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};
use std::sync::{Arc, Mutex};

use crate::config::Config;

/// Thread-safe handle to the SQLite database.
pub type Db = Arc<Mutex<Connection>>;

/// Open (or create) the SQLite state database and run pending migrations.
pub fn open(_config: &Config) -> Result<Db> {
    let db_path = crate::config::default_db_path();
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(&db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;

    // Enable WAL for better concurrent read performance.
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;

    run_migrations(&mut conn)?;

    tracing::info!("Database opened at {}", db_path.display());
    Ok(Arc::new(Mutex::new(conn)))
}

fn run_migrations(conn: &mut Connection) -> Result<()> {
    let migrations = Migrations::new(vec![
        M::up(schema::V1_INITIAL),
    ]);
    migrations
        .to_latest(conn)
        .context("running database migrations")?;
    Ok(())
}

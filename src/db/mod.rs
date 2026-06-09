pub mod models;
pub mod schema;

use anyhow::{Context, Result};
use rusqlite::Connection;
use rusqlite_migration::{Migrations, M};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Thread-safe handle to the SQLite database.
pub type Db = Arc<Mutex<Connection>>;

pub fn open_at(db_path: &Path) -> Result<Db> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;

    // Enable WAL for better concurrent read performance.
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA busy_timeout=5000;
         PRAGMA synchronous=NORMAL;
         PRAGMA cache_size=-8000;
         PRAGMA foreign_keys=ON;"
    )?;

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

/// Run a database operation on a blocking thread pool, returning the result.
pub async fn run_blocking<F, R>(db: &Db, f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R> + Send + 'static,
    R: Send + 'static,
{
    let db = db.clone();
    tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap();
        f(&conn)
    })
    .await
    .context("database task panicked")?
}


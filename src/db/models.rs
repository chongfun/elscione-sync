use rusqlite::{params, params_from_iter, types::Type, Connection, Result as SqlResult, Row};

// ---------------------------------------------------------------------------
// FileRecord
// ---------------------------------------------------------------------------

/// Represents a row read from the `files` table (fields actually used by the app).
#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: i64,
    pub remote_url: String,
    pub remote_path: String,
    pub last_modified: Option<String>,
    pub size_bytes: Option<i64>,
    pub status: FileStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileStatus {
    Pending,
    Downloading,
    Done,
    Error,
    Skipped,
}

impl FileStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Downloading => "downloading",
            Self::Done => "done",
            Self::Error => "error",
            Self::Skipped => "skipped",
        }
    }
}

impl std::str::FromStr for FileStatus {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "downloading" => Ok(Self::Downloading),
            "done" => Ok(Self::Done),
            "error" => Ok(Self::Error),
            "skipped" => Ok(Self::Skipped),
            other => Err(format!("unknown file status: {other}")),
        }
    }
}

impl FileRecord {
    fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        let status_str: String = row.get(5)?;
        let status = status_str.parse().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
            )
        })?;
        Ok(Self {
            id: row.get(0)?,
            remote_url: row.get(1)?,
            remote_path: row.get(2)?,
            last_modified: row.get(3)?,
            size_bytes: row.get(4)?,
            status,
        })
    }
}

// ---------------------------------------------------------------------------
// File operations
// ---------------------------------------------------------------------------

const FILE_COLUMNS: &str = "id, remote_url, remote_path, last_modified, size_bytes, status";

/// Insert or update a file record.
pub fn upsert_file(
    conn: &Connection,
    remote_url: &str,
    remote_path: &str,
    last_modified: Option<&str>,
    size_bytes: Option<i64>,
) -> SqlResult<i64> {
    conn.execute(
        "INSERT INTO files (remote_url, remote_path, last_modified, size_bytes)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(remote_url) DO UPDATE SET
             last_modified = excluded.last_modified,
             size_bytes    = excluded.size_bytes,
             status        = CASE 
                                WHEN status IN ('skipped', 'error') THEN 'pending'
                                WHEN status = 'done' AND files.last_modified != excluded.last_modified THEN 'pending'
                                ELSE status
                             END
         WHERE status != 'done'
            OR last_modified != excluded.last_modified",
        params![remote_url, remote_path, last_modified, size_bytes],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Load all files with the given status.
pub fn files_by_status(conn: &Connection, status: &str) -> SqlResult<Vec<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILE_COLUMNS} FROM files WHERE status = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![status], FileRecord::from_row)?;
    rows.collect()
}

/// Load up to `limit` files with the given status.
pub fn files_by_status_limited(
    conn: &Connection,
    status: &str,
    limit: usize,
) -> SqlResult<Vec<FileRecord>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {FILE_COLUMNS} FROM files WHERE status = ?1 ORDER BY id LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![status, limit as i64], FileRecord::from_row)?;
    rows.collect()
}

/// Count files grouped by status.
pub fn status_counts(conn: &Connection) -> SqlResult<Vec<(String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT status, COUNT(*) FROM files GROUP BY status ORDER BY status")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Number of files with status 'pending'.
pub fn pending_count(conn: &Connection) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM files WHERE status='pending'",
        [],
        |row| row.get(0),
    )
}

/// Sum of size_bytes for pending + downloading files.
pub fn pending_bytes(conn: &Connection) -> SqlResult<i64> {
    conn.query_row(
        "SELECT COALESCE(SUM(size_bytes), 0) FROM files WHERE status IN ('pending','downloading')",
        [],
        |row| row.get(0),
    )
}

/// Update status (and optionally error message / checksum / completed_at) for a file.
pub fn set_file_status(
    conn: &Connection,
    id: i64,
    status: &str,
    error_message: Option<&str>,
    checksum: Option<&str>,
) -> SqlResult<()> {
    if status == "done" {
        conn.execute(
            "UPDATE files SET status=?1, error_message=?2, checksum_sha256=?3,
             completed_at=datetime('now') WHERE id=?4 AND status != 'done'",
            params![status, error_message, checksum, id],
        )?;
    } else {
        conn.execute(
            "UPDATE files SET status=?1, error_message=?2, checksum_sha256=?3 WHERE id=?4 AND status != 'done'",
            params![status, error_message, checksum, id],
        )?;
    }
    Ok(())
}

/// Increment retry_count and set status to 'error'.
pub fn record_error(conn: &Connection, id: i64, message: &str) -> SqlResult<()> {
    conn.execute(
        "UPDATE files SET status='error', error_message=?1, retry_count=retry_count+1 WHERE id=?2 AND status != 'done'",
        params![message, id],
    )?;
    Ok(())
}

/// Reset 'downloading' (interrupted), 'error', and 'skipped' files back to 'pending'.
pub fn reset_interrupted(conn: &Connection, errors_only: bool) -> SqlResult<usize> {
    if errors_only {
        let n = conn.execute(
            "UPDATE files SET status='pending', error_message=NULL WHERE status='error'",
            [],
        )?;
        Ok(n)
    } else {
        let n = conn.execute(
            "UPDATE files SET status='pending', error_message=NULL
             WHERE status IN ('downloading', 'error', 'skipped')",
            [],
        )?;
        Ok(n)
    }
}

/// List files with optional path/status filters.
pub fn list_files(
    conn: &Connection,
    path_filter: Option<&str>,
    status_filter: Option<&str>,
) -> SqlResult<Vec<FileRecord>> {
    let mut query = format!("SELECT {FILE_COLUMNS} FROM files WHERE 1=1");
    let mut query_params = Vec::new();
    if let Some(path_filter) = path_filter {
        query.push_str(" AND remote_path LIKE ?");
        query_params.push(format!("%{path_filter}%"));
    }
    if let Some(status_filter) = status_filter {
        query.push_str(" AND status = ?");
        query_params.push(status_filter.to_owned());
    }
    query.push_str(" ORDER BY remote_path LIMIT 500");

    let mut stmt = conn.prepare(&query)?;

    let rows = stmt.query_map(params_from_iter(query_params.iter()), FileRecord::from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// CrawlQueue operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct CrawlEntry {
    pub id: i64,
    pub url: String,
    pub depth: i64,
}

pub fn enqueue_crawl(conn: &Connection, url: &str, depth: i64) -> SqlResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO crawl_queue (url, depth) VALUES (?1, ?2)",
        params![url, depth],
    )?;
    Ok(())
}

/// Completely clear the crawl queue.
pub fn clear_crawl_queue(conn: &Connection) -> SqlResult<()> {
    conn.execute("DELETE FROM crawl_queue", [])?;
    Ok(())
}

pub fn next_crawl_entries(conn: &Connection, limit: usize) -> SqlResult<Vec<CrawlEntry>> {
    let mut stmt = conn.prepare(
        "SELECT id, url, depth FROM crawl_queue
         WHERE status='pending' ORDER BY depth, id LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit as i64], |row| {
        Ok(CrawlEntry {
            id: row.get(0)?,
            url: row.get(1)?,
            depth: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn mark_crawl_done(conn: &Connection, id: i64) -> SqlResult<()> {
    conn.execute(
        "UPDATE crawl_queue SET status='done' WHERE id=?1",
        params![id],
    )?;
    Ok(())
}

pub fn mark_crawl_error(conn: &Connection, id: i64) -> SqlResult<()> {
    conn.execute(
        "UPDATE crawl_queue SET status='error' WHERE id=?1",
        params![id],
    )?;
    Ok(())
}

pub fn has_pending_crawl(conn: &Connection) -> SqlResult<bool> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM crawl_queue WHERE status='pending'",
        [],
        |r| r.get(0),
    )?;
    Ok(count > 0)
}

/// Reset all 'error' entries in the crawl queue back to 'pending'.
pub fn reset_crawl_errors(conn: &Connection) -> SqlResult<usize> {
    let n = conn.execute(
        "UPDATE crawl_queue SET status='pending' WHERE status='error'",
        [],
    )?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// SelectedFolder operations
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct SelectedFolder {
    pub path: String,
    pub enabled: bool,
    pub size_bytes: Option<i64>,
}

pub fn load_selected_folders(conn: &Connection) -> SqlResult<Vec<SelectedFolder>> {
    let mut stmt =
        conn.prepare("SELECT path, enabled, size_bytes FROM selected_folders ORDER BY path")?;
    let rows = stmt.query_map([], |row| {
        Ok(SelectedFolder {
            path: row.get(0)?,
            enabled: row.get::<_, i64>(1)? != 0,
            size_bytes: row.get(2)?,
        })
    })?;
    rows.collect()
}

pub fn save_selected_folders(conn: &Connection, folders: &[SelectedFolder]) -> SqlResult<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM selected_folders", [])?;
    {
        let mut stmt = tx.prepare_cached(
            "INSERT INTO selected_folders (path, enabled, size_bytes) VALUES (?1, ?2, ?3)",
        )?;
        for f in folders {
            stmt.execute(params![f.path, f.enabled as i64, f.size_bytes])?;
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn has_any_selected_folders(conn: &Connection) -> SqlResult<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM selected_folders", [], |r| r.get(0))?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory database");
        conn.execute_batch(schema::V1_INITIAL)
            .expect("create schema");
        conn
    }

    fn insert_file(conn: &Connection, remote_path: &str, status: &str) {
        conn.execute(
            "INSERT INTO files (remote_url, remote_path, status, size_bytes)
             VALUES (?1, ?2, ?3, 42)",
            params![
                format!("https://example.test{remote_path}"),
                remote_path,
                status
            ],
        )
        .expect("insert file");
    }

    #[test]
    fn list_files_handles_all_filter_combinations() {
        let conn = test_conn();
        insert_file(&conn, "/Books/a.epub", "pending");
        insert_file(&conn, "/Manga/b.cbz", "done");

        let all = list_files(&conn, None, None).expect("list all");
        assert_eq!(all.len(), 2);

        let by_path = list_files(&conn, Some("Books"), None).expect("filter by path");
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].remote_path, "/Books/a.epub");

        let by_status = list_files(&conn, None, Some("done")).expect("filter by status");
        assert_eq!(by_status.len(), 1);
        assert_eq!(by_status[0].remote_path, "/Manga/b.cbz");

        let by_both = list_files(&conn, Some("Books"), Some("pending")).expect("filter by both");
        assert_eq!(by_both.len(), 1);
        assert_eq!(by_both[0].remote_path, "/Books/a.epub");
    }

    #[test]
    fn files_by_status_limited_caps_result_count() {
        let conn = test_conn();
        insert_file(&conn, "/Books/a.epub", "pending");
        insert_file(&conn, "/Books/b.epub", "pending");
        insert_file(&conn, "/Books/c.epub", "pending");

        let files = files_by_status_limited(&conn, "pending", 2).expect("load pending batch");

        assert_eq!(files.len(), 2);
        assert_eq!(files[0].remote_path, "/Books/a.epub");
        assert_eq!(files[1].remote_path, "/Books/b.epub");
    }

    #[test]
    fn schema_rejects_invalid_file_status() {
        let conn = test_conn();

        let result = conn.execute(
            "INSERT INTO files (remote_url, remote_path, status)
             VALUES ('https://example.test/bad.epub', '/bad.epub', 'mystery')",
            [],
        );

        assert!(result.is_err());
    }

    #[test]
    fn invalid_file_status_rows_are_not_treated_as_pending() {
        let conn = test_conn();
        conn.execute_batch("PRAGMA ignore_check_constraints = ON;")
            .expect("disable check constraints");
        insert_file(&conn, "/bad.epub", "mystery");
        conn.execute_batch("PRAGMA ignore_check_constraints = OFF;")
            .expect("enable check constraints");

        let result = list_files(&conn, None, None);

        assert!(result.is_err());
    }
}

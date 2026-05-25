use rusqlite::{params, Connection, Result as SqlResult, Row};

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
    type Err = anyhow::Error;
    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "downloading" => Ok(Self::Downloading),
            "done" => Ok(Self::Done),
            "error" => Ok(Self::Error),
            "skipped" => Ok(Self::Skipped),
            other => anyhow::bail!("unknown file status: {other}"),
        }
    }
}

impl FileRecord {
    fn from_row(row: &Row<'_>) -> SqlResult<Self> {
        let status_str: String = row.get(5)?;
        let status = status_str.parse().unwrap_or(FileStatus::Pending);
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

const FILE_COLUMNS: &str =
    "id, remote_url, remote_path, last_modified, size_bytes, status";

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

/// Count files grouped by status.
pub fn status_counts(conn: &Connection) -> SqlResult<Vec<(String, i64)>> {
    let mut stmt =
        conn.prepare("SELECT status, COUNT(*) FROM files GROUP BY status ORDER BY status")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
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
    if path_filter.is_some() {
        query.push_str(" AND remote_path LIKE ?1");
    }
    if status_filter.is_some() {
        query.push_str(" AND status = ?2");
    }
    query.push_str(" ORDER BY remote_path LIMIT 500");

    let mut stmt = conn.prepare(&query)?;
    let path_param = path_filter.map(|f| format!("%{f}%")).unwrap_or_default();
    let status_param = status_filter.unwrap_or("");

    let rows = stmt.query_map(params![path_param, status_param], FileRecord::from_row)?;
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

pub fn has_pending_crawl(conn: &Connection) -> SqlResult<bool> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM crawl_queue WHERE status='pending'", [], |r| {
            r.get(0)
        })?;
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
    conn.execute("DELETE FROM selected_folders", [])?;
    for f in folders {
        conn.execute(
            "INSERT INTO selected_folders (path, enabled, size_bytes) VALUES (?1, ?2, ?3)",
            params![f.path, f.enabled as i64, f.size_bytes],
        )?;
    }
    Ok(())
}

pub fn has_any_selected_folders(conn: &Connection) -> SqlResult<bool> {
    let count: i64 =
        conn.query_row("SELECT COUNT(*) FROM selected_folders", [], |r| r.get(0))?;
    Ok(count > 0)
}

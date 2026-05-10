/// V1 initial schema migration.
pub const V1_INITIAL: &str = "
-- Tracks every discovered remote file.
CREATE TABLE IF NOT EXISTS files (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    remote_url      TEXT NOT NULL UNIQUE,
    remote_path     TEXT NOT NULL,
    last_modified   TEXT,
    size_bytes      INTEGER,
    local_path      TEXT,
    status          TEXT NOT NULL DEFAULT 'pending',
    error_message   TEXT,
    retry_count     INTEGER NOT NULL DEFAULT 0,
    discovered_at   TEXT NOT NULL DEFAULT (datetime('now')),
    completed_at    TEXT,
    checksum_sha256 TEXT
);

-- Tracks the BFS crawl queue so crawling can be paused and resumed.
CREATE TABLE IF NOT EXISTS crawl_queue (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    url           TEXT NOT NULL UNIQUE,
    status        TEXT NOT NULL DEFAULT 'pending',
    depth         INTEGER NOT NULL DEFAULT 0,
    discovered_at TEXT NOT NULL DEFAULT (datetime('now'))
);

-- Persisted folder selections from the TUI.
CREATE TABLE IF NOT EXISTS selected_folders (
    path        TEXT PRIMARY KEY,
    enabled     INTEGER NOT NULL DEFAULT 1,
    size_bytes  INTEGER
);

CREATE INDEX IF NOT EXISTS idx_files_status ON files(status);
CREATE INDEX IF NOT EXISTS idx_files_remote_path ON files(remote_path);
CREATE INDEX IF NOT EXISTS idx_crawl_queue_status ON crawl_queue(status);
";

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
    status          TEXT NOT NULL DEFAULT 'pending'
                    CHECK (status IN ('pending', 'downloading', 'done', 'error', 'skipped')),
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
    status        TEXT NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending', 'done', 'error')),
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

/// V2 adds status validation for databases created before V1 had CHECK constraints.
pub const V2_STATUS_VALIDATION: &str = "
CREATE TRIGGER IF NOT EXISTS validate_files_status_insert
BEFORE INSERT ON files
WHEN NEW.status NOT IN ('pending', 'downloading', 'done', 'error', 'skipped')
BEGIN
    SELECT RAISE(ABORT, 'invalid files.status');
END;

CREATE TRIGGER IF NOT EXISTS validate_files_status_update
BEFORE UPDATE OF status ON files
WHEN NEW.status NOT IN ('pending', 'downloading', 'done', 'error', 'skipped')
BEGIN
    SELECT RAISE(ABORT, 'invalid files.status');
END;

CREATE TRIGGER IF NOT EXISTS validate_crawl_queue_status_insert
BEFORE INSERT ON crawl_queue
WHEN NEW.status NOT IN ('pending', 'done', 'error')
BEGIN
    SELECT RAISE(ABORT, 'invalid crawl_queue.status');
END;

CREATE TRIGGER IF NOT EXISTS validate_crawl_queue_status_update
BEFORE UPDATE OF status ON crawl_queue
WHEN NEW.status NOT IN ('pending', 'done', 'error')
BEGIN
    SELECT RAISE(ABORT, 'invalid crawl_queue.status');
END;
";

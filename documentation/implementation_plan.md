# Implementation Plan: Elscione File Synchronizer

## Goal Description
The objective of this project is to build a robust, resumable file mirroring CLI tool (`elscione-sync`) in Rust that securely crawls and downloads files from the `server.elscione.com` h5ai server. 

The application has been designed to handle massive directories (14,000+ files) via stateful persistence, smart concurrency, and resilience against server timeouts, compression encoding, and terminal rendering limits over multi-hour sync sessions.

`server.elscione.com` hosts a large library of manga, light novels, audiobooks, books, and music organized in a custom Nginx/Apache-style autoindex directory tree. Each entry in the directory listing exposes three columns: **Name**, **Last modified** (`YYYY-MM-DD HH:MM`), and **Size**. The site explicitly warns that crawlers may not work, so well-behaved, rate-limited access is essential.

Top-level categories observed:
- `Books`
- `Games`
- `Manga`
- `Movies`
- `Music`
- `Officially Translated Light Novels`
- `Officially Translated LN Audiobooks`
- `ライトノベル - Officially Untranslated Light Novels`

The structure is typically 3–4 levels deep (Root → Category → Series → Files).

## Decisions Locked
* **Language & Tooling:** Rust via Cargo, using `tokio` for async execution, `reqwest` for HTTP communication, `rusqlite` for state management, and `indicatif` for terminal UI progress bars.
* **API Protocol:** Use raw JSON POST requests directed at `{base_url}/?`, mimicking the web client payload (`{"action":"get","items":{"href":"...","what":1}}`) instead of form-encoded data.
* **State Management:** A local `state.db` SQLite database using `crawl_queue`, `files`, and `selected_folders` tables to enforce atomicity and support resumability.
* **Resiliency vs Freshness:** If a sync is started and the crawl queue is completely empty (no pending/error nodes), the crawler drops the old queue states and performs a fresh discovery run to find newly added server files.

## As Built

> This section describes the **actual implementation** as it exists in the codebase.

---

### Module Structure

```
src/
├── main.rs                    # Entry point: CLI dispatch, tracing init, config load
├── cli.rs                     # clap CLI definitions (Commands + SyncOpts)
├── config.rs                  # Config structs + TOML load/save + platform paths
├── sync.rs                    # Sync orchestration state machine
├── db/
│   ├── mod.rs                 # DB connection setup + migrations
│   ├── schema.rs              # Database schema definitions
│   └── models.rs              # All SQL queries (upsert, status transitions, counts)
├── crawler/
│   ├── mod.rs                 # BFS crawl orchestration via h5ai JSON API
│   ├── parser.rs              # DirEntry struct (stripped of HTML parsing)
│   └── rate_limiter.rs        # reqwest client builder + inter-request delay
├── downloader/
│   ├── mod.rs                 # Concurrent download manager (semaphore, progress bars)
│   └── file_writer.rs         # Streaming write to .part temp file + atomic rename
└── tui/
    ├── mod.rs                 # ratatui app loop entry point
    └── folder_selector.rs     # Interactive checkbox folder selector widget
```

**Note:** Database schema is defined in `db/schema.rs`, and migrations are run in `db/mod.rs`.

---

### Dependencies (`Cargo.toml`)

| Crate | Version | Purpose |
|---|---|---|
| `tokio` | 1 (full) | Async runtime |
| `reqwest` | 0.12 | HTTP client (rustls-tls, stream, json, gzip, brotli, deflate) |
| `rusqlite` | 0.32 (bundled) | SQLite state database |
| `rusqlite_migration` | 1 | Schema migration runner |
| `serde` / `serde_json` / `toml` | 1 | Config + API serialization |
| `clap` | 4 (derive) | CLI argument parsing |
| `ratatui` | 0.29 | Interactive TUI folder selector |
| `crossterm` | 0.28 | Terminal backend for ratatui |
| `indicatif` | 0.17 (tokio) | Progress bars (MultiProgress) |
| `chrono` | 0.4 | Timestamp parsing from h5ai responses |
| `anyhow` / `thiserror` | 1/2 | Error handling |
| `tracing` + `tracing-subscriber` | 0.1/0.3 | Structured, levelled logging |
| `futures` | 0.3 | Stream utilities (bytes_stream) |
| `filetime` | 0.2 | Setting mtime from server Last-Modified |
| `sha2` | 0.10 | SHA-256 checksum during download |
| `bytesize` | 1 | Human-readable file size formatting |
| `directories` | 5 | XDG-compliant platform config/data paths |

---

### Config File

Platform location: determined by `directories::ProjectDirs::from("com", "elscione", "elscione-sync")`; on macOS this is `~/Library/Application Support/com.elscione.elscione-sync/config.toml`.

```toml
[server]
base_url = "https://server.elscione.com/"
user_agent = "elscione-sync/0.1.0 (personal mirror)"

[output]
dir = "~/elscione-mirror"

[concurrency]
max_parallel_downloads = 2
delay_between_requests_ms = 1500  # Applied between each download request
crawl_delay_ms = 500              # Applied between each crawl request
max_crawl_retries = 3

[rate_limit]
backoff_initial_secs = 60
backoff_max_secs = 900
backoff_multiplier = 2.0

[sync]
include_folders = []           # Empty = all folders (or use TUI selector)
exclude_patterns = []          # Substring match against URL
allowed_extensions = []        # Empty = all filetypes; e.g. ["epub", "cbz"]
redownload_on_size_mismatch = true
```

---

### State Database

Platform location: determined by `directories::ProjectDirs::from("com", "elscione", "elscione-sync")`; on macOS this is `~/Library/Application Support/com.elscione.elscione-sync/state.db`.

#### `crawl_queue` table
Tracks folder discovery state for resumable BFS crawl.

| Column | Type | Notes |
|---|---|---|
| `id` | INTEGER PK | |
| `url` | TEXT UNIQUE | Full URL of the directory |
| `status` | TEXT | `pending` \| `done` \| `error` |
| `depth` | INTEGER | Depth from root (0 = root) |
| `discovered_at` | TEXT | ISO8601 timestamp |

#### `files` table
Tracks every discovered file and its download state.

| Column | Type | Notes |
|---|---|---|
| `id` | INTEGER PK | |
| `remote_url` | TEXT UNIQUE | Full download URL |
| `remote_path` | TEXT | Server path relative to base URL |
| `last_modified` | TEXT | From h5ai API (formatted `YYYY-MM-DD HH:MM`) |
| `size_bytes` | INTEGER | From h5ai API |
| `local_path` | TEXT | Reserved for a resolved local destination path |
| `status` | TEXT | `pending` \| `downloading` \| `done` \| `error` \| `skipped` |
| `error_message` | TEXT | Populated on failure |
| `retry_count` | INTEGER | Incremented when a download attempt records an error |
| `discovered_at` | TEXT | ISO8601 timestamp |
| `completed_at` | TEXT | ISO8601 timestamp |
| `checksum_sha256` | TEXT | SHA-256 hex, populated after download |

#### `selected_folders` table
Persists user's interactive TUI folder selections.

| Column | Type | Notes |
|---|---|---|
| `path` | TEXT PK | e.g. `Officially Translated Light Novels` |
| `enabled` | INTEGER | 1 = selected, 0 = deselected |
| `size_bytes` | INTEGER | Total size in bytes of the folder contents (if known) |

---

### Crawler (`src/crawler/mod.rs`)

The crawler uses the **h5ai internal JSON API** — HTML scraping was abandoned because the server's directory listings are rendered client-side via JavaScript.

#### API Protocol
1. **GET** the directory URL to extract the `clckd` CSRF token from a `<meta name="clckd" content="...">` tag.
2. **POST** a raw JSON body to `{base_url}/?`:
   ```json
   {"action": "get", "items": {"href": "/path/to/dir/", "what": 1}}
   ```
   With header `x-h5ai-clckd: {token}`.
3. Parse the JSON `items` array — each entry has `href`, `time` (epoch ms), and `size` (bytes).

#### Crawl Logic
- BFS via the `crawl_queue` table in batches of 10 entries at a time.
- If the queue is entirely empty on startup → clears old `done` entries and starts fresh from `base_url`.
- Depth-1 filtering: only descends into folders matching `include_folders`.
- URL-based exclusion: skips URLs containing any `exclude_patterns` substring.
- Extension filtering: if `allowed_extensions` is configured, non-matching files are not inserted into the `files` table at all during crawl.
- Files already inserted are updated via upsert: `skipped`/`error` records are reset to `pending` if the server metadata has changed.

---

### Downloader (`src/downloader/`)

#### Extension Pre-filtering
Before downloading, any `pending` files in the DB that don't match `allowed_extensions` are immediately marked `skipped`. This handles the case where extension filters were added after a crawl.

#### Concurrency Model
- A `tokio::sync::Semaphore` limits concurrent downloads to `max_parallel_downloads`.
- Each file download runs as an independent `tokio::spawn` task holding one permit.
- Files are marked `downloading` before the task starts, and `done`/`error` upon completion.
- A shared `Arc<AtomicU64>` tracks bytes remaining across all concurrent tasks.

#### Progress Display
- An outer `[elapsed] [bar] N/M files (X remaining)` bar tracks overall batch progress.
- An inner `{spinner} filename [bar] bytes/total_bytes` bar tracks each individual file.
- Upon completion, the individual bar is explicitly **removed** from `MultiProgress` (preventing memory and redraw performance degradation over thousands of files) and replaced with a static `✓ filename (size)` line printed to the terminal.

#### File Writing (`src/downloader/file_writer.rs`)
- Downloads to a `.part` temp file in the same target directory.
- Streams bytes directly from the HTTP response to disk using `AsyncWriteExt`.
- Computes a SHA-256 hash inline during streaming.
- Atomically renames `.part` → final filename on success.
- Sets file `mtime` to match the server's `Last-Modified` via `filetime`.

#### HTTP Client Configuration
- `connect_timeout`: 30 seconds for connection establishment.
- Overall request timeout: 1 hour.
- `tcp_keepalive`: 60 seconds.
- Decompression: gzip, brotli, deflate (enabled to prevent "stream error decoding response body").
- Inter-request delay: configurable `delay_between_requests_ms`.

---

### CLI Reference

```
elscione-sync [OPTIONS] [COMMAND]

GLOBAL OPTIONS:
  --config <PATH>    Path to config file (default: platform config dir)
  -v, --verbose      Enable debug logging

COMMANDS:
  sync [OPTIONS]     Discover files, then start or resume downloading (default command if none given)
  select             Open interactive ratatui folder selector
  status             Print file count summary grouped by status
  reset [--errors-only]  Reset interrupted/error/skipped files to 'pending'
  list [--filter <str>] [--status <str>]  List up to 500 files with optional filters
  edit-config        Open config.toml in $EDITOR (or 'open -t' on macOS)

SYNC OPTIONS:
  --output <DIR>          Override output directory
  --concurrency <N>       Override max parallel downloads
  --delay <MS>            Override delay between requests
  --include <FOLDER>...   Include specific folder(s), overrides DB selections
  --exclude <PATTERN>...  Exclude URLs containing a pattern; trailing /* or /** is ignored
  --extension <EXT>...    Only download files with this extension (e.g. epub)
  --dry-run               Plan without downloading; marks files as 'skipped'
  --resume                Skip crawl phase, only download pending files already in the database
```

---

### Key Files Summary

| Path | Purpose |
|---|---|
| `src/main.rs` | Entry point; CLI dispatch, tracing, config load, DB open |
| `src/cli.rs` | clap CLI definitions; `Commands` enum and `SyncOpts` struct |
| `src/config.rs` | `Config` struct hierarchy; TOML load/save; platform path resolution |
| `src/sync.rs` | Orchestrates selector → crawl → download phases; `status`, `reset`, `list` commands |
| `src/db/mod.rs` | Opens SQLite connection, runs migrations |
| `src/db/models.rs` | All DB query functions (upsert, status transitions, counts, resets) |
| `src/crawler/mod.rs` | h5ai API interaction; BFS queue consumption; extension + folder filtering |
| `src/crawler/parser.rs` | `DirEntry` struct definition |
| `src/crawler/rate_limiter.rs` | `RateLimiter` (inter-request sleep); `build_client` (reqwest config) |
| `src/downloader/mod.rs` | Pre-filter pending files; semaphore-bounded concurrent downloads; progress bars |
| `src/downloader/file_writer.rs` | Stream HTTP → `.part` file; atomic rename; mtime preservation; SHA-256 |
| `src/tui/mod.rs` | ratatui event loop entry point |
| `src/tui/folder_selector.rs` | Interactive checkbox tree for selecting top-level sync folders |
| `documentation/implementation_plan.md` | This document |

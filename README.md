# elscione-sync

`elscione-sync` is a robust, resumable, and concurrent file mirroring CLI tool built in Rust. It is specifically designed to safely crawl and mirror the 14,000+ files from the JavaScript-rendered `server.elscione.com` (`h5ai` server) to your local machine.

It utilizes an embedded SQLite database to persist state, ensuring that if you lose connection or cancel a 9-hour sync, you can resume exactly where you left off without re-downloading files or re-crawling the entire server.

For a deep dive into the architecture, state machine, and data models, please see the **[Implementation Plan & Architecture Document](documentation/implementation_plan.md)**.

---

## Installation

Ensure you have Rust installed, then clone the repository and build:

```bash
cargo build --release
```

---

## Usage

You can run the tool via `cargo run -- [COMMAND] [OPTIONS]` or by executing the compiled binary `target/release/elscione-sync [COMMAND] [OPTIONS]`.

If no command is provided, it defaults to the `sync` command.

### Global Options
- `--config <PATH>` : Path to a custom config file (Defaults to `~/.config/elscione-sync/config.toml` or your platform's standard config directory).
- `-v`, `--verbose` : Enable debug-level logging.

---

### Commands

#### `sync`
Starts or resumes the synchronization process. This is the primary command.
- **First run**: If no folders have been selected yet, this will automatically launch the interactive TUI folder selector.
- **Subsequent runs**: It will pick up pending files from the database and begin downloading them concurrently.

**Sync Options:**
- `--output <DIR>` : Override the download destination directory.
- `--concurrency <N>` : Override the maximum number of parallel downloads (default is 2).
- `--delay <MS>` : Override the delay between HTTP requests in milliseconds.
- `--include <FOLDER>...` : Include specific top-level folder(s), temporarily overriding your saved DB selections.
- `--exclude <PATTERN>...` : Exclude specific URL patterns (e.g. `--exclude "Games/**"`).
- `--extension <EXT>...` : Only download files with this extension (e.g. `--extension epub`). Can be repeated.
- `--dry-run` : Crawls the server and populates the database, but marks all files as `skipped` instead of downloading them.
- `--resume` : Completely skips the discovery/crawl phase and immediately starts downloading files currently marked as `pending` in the database.

#### `select`
Opens the interactive terminal UI (ratatui) to browse the server's root directories and select which folders you want to sync. Selections are saved to the database.

#### `status`
Prints a summary table showing how many files are currently in each state (`pending`, `done`, `skipped`, `error`, `downloading`), along with the total remaining download size.

#### `reset`
Resets files in a bad state back to `pending` so they can be retried on the next `sync`.
- **Default behavior**: Resets files marked as `error`, `skipped`, and `downloading` (interrupted).
- `--errors-only` : Only resets files that explicitly failed with an `error`.

#### `list`
Lists the files currently tracked in the local database.
- `--filter <str>` : Filter output by a substring in the file path.
- `--status <str>` : Filter output by a specific status (e.g. `pending`, `done`).

#### `edit-config`
Instantly opens your `config.toml` file in your system's default `$EDITOR` (or `open -t` on macOS, `notepad` on Windows).

---

## Configuration

By default, `elscione-sync` generates a configuration file at `~/.config/elscione-sync/config.toml` (or the equivalent XDG/macOS application support directory). 

You can quickly edit this file by running:
```bash
cargo run -- edit-config
```

Example `config.toml`:
```toml
[server]
base_url = "https://server.elscione.com/"
user_agent = "elscione-sync/0.1.0 (personal mirror)"

[output]
dir = "~/elscione-mirror"

[concurrency]
max_parallel_downloads = 2
delay_between_requests_ms = 1500
crawl_delay_ms = 500

[rate_limit]
backoff_initial_secs = 60
backoff_max_secs = 900
backoff_multiplier = 2.0

[sync]
# Array of allowed file extensions. If empty, all files are downloaded.
allowed_extensions = ["epub", "cbz"]
redownload_on_size_mismatch = true
```

---

## State & Resumability

`elscione-sync` uses a local SQLite database (`~/.local/share/elscione-sync/state.db`) to track progress.

1. **Crawl Phase:** The crawler securely bypasses JavaScript rendering by interacting with the internal JSON API. It catalogs files into the DB as `pending`.
2. **Download Phase:** The downloader pulls batches of `pending` files and downloads them to `.part` files before atomically renaming them. If a download is interrupted (e.g. via `Ctrl-C`), the `.part` file is kept. On the next run, the database resets the interrupted file to `pending` and it is retried.

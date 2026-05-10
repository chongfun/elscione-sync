use anyhow::Result;
use bytesize::ByteSize;
use tracing::info;

use crate::cli::SyncOpts;
use crate::config::Config;
use crate::db::{models, Db};
use crate::{crawler, downloader, tui};

/// Main sync entry point — runs folder selection (if needed), crawl, and download phases.
pub async fn run(mut config: Config, db: Db, opts: SyncOpts) -> Result<()> {
    // Apply CLI overrides to config.
    if let Some(output) = opts.output {
        config.output.dir = output;
    }
    if let Some(concurrency) = opts.concurrency {
        config.concurrency.max_parallel_downloads = concurrency;
    }
    if let Some(delay) = opts.delay {
        config.concurrency.delay_between_requests_ms = delay;
    }

    // First run: open folder selector if no selections saved and no CLI include specified.
    let has_selections = {
        let conn = db.lock().unwrap();
        models::has_any_selected_folders(&conn)?
    };

    if !has_selections && opts.include.is_empty() {
        info!("No folder selections found — launching interactive selector.");
        tui::run_folder_selector(&config, &db).await?;
    }

    // Merge DB selections with CLI --include overrides.
    let include_folders: Vec<String> = if !opts.include.is_empty() {
        opts.include.clone()
    } else {
        let conn = db.lock().unwrap();
        models::load_selected_folders(&conn)?
            .into_iter()
            .filter(|f| f.enabled)
            .map(|f| f.path)
            .collect()
    };

    if include_folders.is_empty() && config.sync.include_folders.is_empty() {
        info!("No folders selected — nothing to sync.");
        return Ok(());
    }

    // Reset any stale 'downloading' records from a previous interrupted run.
    {
        let conn = db.lock().unwrap();
        let reset = models::reset_interrupted(&conn, false)?;
        if reset > 0 {
            info!("Reset {reset} interrupted downloads to 'pending'.");
        }
    }

    // ── Crawl Phase ──
    if !opts.resume {
        info!("Starting crawl phase …");
        crawler::run(
            &config,
            &db,
            &include_folders,
            &opts.exclude,
        )
        .await?;
    } else {
        info!("--resume: skipping crawl, downloading pending files only.");
    }

    // Print pending summary.
    {
        let conn = db.lock().unwrap();
        let pending_bytes = models::pending_bytes(&conn)?;
        let pending_count = models::files_by_status(&conn, "pending")?.len();
        info!(
            "Download queue: {} file(s) / {}",
            pending_count,
            ByteSize(pending_bytes as u64)
        );
    }

    // ── Download Phase ──
    info!("Starting download phase …");
    downloader::run(&config, &db, opts.dry_run).await?;

    print_status(&db)?;
    Ok(())
}

/// Print a summary table of file counts by status.
pub fn print_status(db: &Db) -> Result<()> {
    let conn = db.lock().unwrap();
    let counts = models::status_counts(&conn)?;
    let pending_bytes = models::pending_bytes(&conn)?;

    println!("\n─── elscione-sync status ───────────────────");
    for (status, count) in &counts {
        println!("  {:<12} {}", status, count);
    }
    if pending_bytes > 0 {
        println!("  {:<12} {}", "pending size", ByteSize(pending_bytes as u64));
    }
    println!("────────────────────────────────────────────\n");
    Ok(())
}

/// Reset interrupted/error records back to 'pending'.
pub fn reset(db: &Db, errors_only: bool) -> Result<()> {
    let conn = db.lock().unwrap();
    let n = models::reset_interrupted(&conn, errors_only)?;
    println!("Reset {n} file(s) to 'pending'.");
    Ok(())
}

/// List files with optional filters.
pub fn list(db: &Db, filter: Option<&str>, status: Option<&str>) -> Result<()> {
    let conn = db.lock().unwrap();
    let files = models::list_files(&conn, filter, status)?;
    println!("{:<10} {:<12} {:<10} {}", "ID", "STATUS", "SIZE", "PATH");
    for f in &files {
        let size = f
            .size_bytes
            .map(|b| ByteSize(b as u64).to_string())
            .unwrap_or_else(|| "—".to_owned());
        println!("{:<10} {:<12} {:<10} {}", f.id, f.status.as_str(), size, f.remote_path);
    }
    println!("({} result(s))", files.len());
    Ok(())
}

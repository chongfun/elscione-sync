use anyhow::Result;
use bytesize::ByteSize;
use tracing::info;

use crate::cli::SyncOpts;
use crate::config::Config;
use crate::db::{models, Db};
use crate::{crawler, downloader, tui};

/// Main sync entry point — runs folder selection (if needed), crawl, and download phases.
pub async fn run(
    mut config: Config,
    db: Db,
    opts: SyncOpts,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
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
    if !opts.extensions.is_empty() {
        config.sync.allowed_extensions = opts
            .extensions
            .iter()
            .map(|ext| ext.trim_start_matches('.').to_owned())
            .collect();
    }

    let session = crate::crawler::session::ElscioneSession::new(
        &config.server.base_url,
        &config.server.user_agent,
        config.server.cookie.as_deref(),
    )?;

    // First run: open folder selector if no selections saved and no CLI include specified.
    let has_selections =
        crate::db::run_blocking(&db, |conn| Ok(models::has_any_selected_folders(conn)?)).await?;

    if !has_selections && opts.include.is_empty() {
        info!("No folder selections found — launching interactive selector.");
        tui::run_folder_selector(&config, &session, &db).await?;
    }

    // Merge DB selections with CLI --include overrides.
    let include_folders: Vec<String> = if !opts.include.is_empty() {
        opts.include.clone()
    } else {
        crate::db::run_blocking(&db, |conn| Ok(models::load_selected_folders(conn)?))
            .await?
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
        let reset =
            crate::db::run_blocking(&db, |conn| Ok(models::reset_interrupted(conn, false)?))
                .await?;
        if reset > 0 {
            info!("Reset {reset} interrupted downloads to 'pending'.");
        }
    }

    // ── Crawl Phase ──
    if !opts.resume {
        if cancel_token.is_cancelled() {
            info!("Sync cancelled before crawl phase.");
            return Ok(());
        }
        info!("Starting crawl phase …");
        crawler::run(
            &config,
            &session,
            &db,
            &include_folders,
            &opts.exclude,
            cancel_token.clone(),
        )
        .await?;
    } else {
        info!("--resume: skipping crawl, verifying session clearance before download.");
        session.ensure_cleared(&config.server.base_url).await?;
    }

    if cancel_token.is_cancelled() {
        info!("Sync cancelled before download phase.");
        return Ok(());
    }

    // ── Download Phase ──
    info!("Starting download phase …");
    downloader::run(&config, &session, &db, opts.dry_run, cancel_token.clone()).await?;

    if cancel_token.is_cancelled() {
        info!("Sync was cancelled by user.");
    } else {
        print_status(&db).await?;
    }
    Ok(())
}

/// Print a summary table of file counts by status.
pub async fn print_status(db: &Db) -> Result<()> {
    let (counts, pending_bytes) = crate::db::run_blocking(db, |conn| {
        let counts = models::status_counts(conn)?;
        let pending_bytes = models::pending_bytes(conn)?;
        Ok((counts, pending_bytes))
    })
    .await?;

    println!("\n─── elscione-sync status ───────────────────");
    for (status, count) in &counts {
        println!("  {:<12} {}", status, count);
    }
    if pending_bytes > 0 {
        println!(
            "  {:<12} {}",
            "pending size",
            ByteSize(pending_bytes as u64)
        );
    }
    println!("────────────────────────────────────────────\n");
    Ok(())
}

/// Reset interrupted/error records back to 'pending'.
pub async fn reset(db: &Db, errors_only: bool) -> Result<()> {
    let n = crate::db::run_blocking(db, move |conn| {
        Ok(models::reset_interrupted(conn, errors_only)?)
    })
    .await?;
    println!("Reset {n} file(s) to 'pending'.");
    Ok(())
}

/// List files with optional filters.
pub async fn list(db: &Db, filter: Option<&str>, status: Option<&str>) -> Result<()> {
    let filter = filter.map(|s| s.to_owned());
    let status = status.map(|s| s.to_owned());
    let files = crate::db::run_blocking(db, move |conn| {
        Ok(models::list_files(
            conn,
            filter.as_deref(),
            status.as_deref(),
        )?)
    })
    .await?;
    println!("{:<10} {:<12} {:<10} PATH", "ID", "STATUS", "SIZE");
    for f in &files {
        let size = f
            .size_bytes
            .map(|b| ByteSize(b as u64).to_string())
            .unwrap_or_else(|| "—".to_owned());
        println!(
            "{:<10} {:<12} {:<10} {}",
            f.id,
            f.status.as_str(),
            size,
            f.remote_path
        );
    }
    println!("({} result(s))", files.len());
    Ok(())
}

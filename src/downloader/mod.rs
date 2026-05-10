pub mod file_writer;

use anyhow::Result;
use bytesize::ByteSize;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::crawler::rate_limiter::{build_client, RateLimiter};
use crate::db::{models, Db};

/// Run the download phase — pulls all `pending` files from the DB and downloads
/// them concurrently up to `max_parallel_downloads`.
pub async fn run(config: &Config, db: &Db, dry_run: bool) -> Result<()> {
    let client = build_client(&config.server.user_agent)?;
    let limiter = RateLimiter::new(config.concurrency.delay_between_requests_ms);
    let semaphore = Arc::new(Semaphore::new(config.concurrency.max_parallel_downloads));
    let multi = MultiProgress::new();
    let output_dir = config.output.dir.clone();

    loop {
        let mut pending = {
            let conn = db.lock().unwrap();
            models::files_by_status(&conn, "pending")?
        };

        if !config.sync.allowed_extensions.is_empty() {
            let mut filtered = Vec::new();
            for record in pending {
                let ext = std::path::Path::new(&record.remote_url)
                    .extension()
                    .and_then(|s| s.to_str())
                    .unwrap_or("");
                
                if config.sync.allowed_extensions.iter().any(|a| a.eq_ignore_ascii_case(ext)) {
                    filtered.push(record);
                } else {
                    debug!("Skipping file during download (extension not allowed): {}", record.remote_url);
                    let conn = db.lock().unwrap();
                    let _ = models::set_file_status(&conn, record.id, "skipped", None, None);
                }
            }
            pending = filtered;
        }

        if pending.is_empty() {
            // Check if there are any pending files left in the DB. If we skipped the whole
            // batch, we should fetch the next batch. If there are truly no pending files, break.
            let count = {
                let conn = db.lock().unwrap();
                conn.query_row("SELECT COUNT(*) FROM files WHERE status='pending'", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
            };
            if count == 0 {
                break;
            } else {
                continue;
            }
        }

        // Calculate total size of this batch for the overall progress bar.
        let total_bytes: u64 = pending
            .iter()
            .filter_map(|f| f.size_bytes)
            .map(|b| b as u64)
            .sum();

        let overall = multi.add(ProgressBar::new(pending.len() as u64));
        overall.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} files  ({msg})",
            )
            .unwrap(),
        );
        overall.set_message(format!("{} remaining", ByteSize(total_bytes)));

        let remaining_bytes = Arc::new(std::sync::atomic::AtomicU64::new(total_bytes));

        let mut handles = Vec::new();

        for record in pending {
            if dry_run {
                info!("[DRY RUN] Would download: {}", record.remote_path);
                let conn = db.lock().unwrap();
                models::set_file_status(&conn, record.id, "skipped", None, None)?;
                overall.inc(1);
                continue;
            }

            // Mark as downloading.
            {
                let conn = db.lock().unwrap();
                models::set_file_status(&conn, record.id, "downloading", None, None)?;
            }

            let permit = semaphore.clone().acquire_owned().await?;
            let client = client.clone();
            let limiter = limiter.clone();
            let db = db.clone();
            let output_dir = output_dir.clone();
            let multi = multi.clone();
            let overall = overall.clone();
            let remaining_bytes = remaining_bytes.clone();
            let redownload_on_mismatch = config.sync.redownload_on_size_mismatch;

            let handle = tokio::spawn(async move {
                let _permit = permit;

                let dest_path = {
                    let rel = record.remote_path.trim_start_matches('/');
                    output_dir.join(rel)
                };

                // Check if file already exists with matching size (skip if not redownload_on_mismatch).
                if dest_path.exists() {
                    if let (Some(expected_size), Ok(meta)) =
                        (record.size_bytes, std::fs::metadata(&dest_path))
                    {
                        let local_size = meta.len() as i64;
                        if local_size == expected_size {
                            info!("Already complete (size match): {}", dest_path.display());
                            let conn = db.lock().unwrap();
                            let _ = models::set_file_status(&conn, record.id, "done", None, None);
                            overall.inc(1);
                            return;
                        } else if !redownload_on_mismatch {
                            warn!(
                                "Size mismatch but redownload_on_size_mismatch=false, skipping: {}",
                                dest_path.display()
                            );
                            let conn = db.lock().unwrap();
                            let _ = models::set_file_status(&conn, record.id, "skipped", None, None);
                            overall.inc(1);
                            return;
                        }
                    }
                }

                // Per-file progress bar.
                let file_name = dest_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let pb = multi.add(ProgressBar::new(
                    record.size_bytes.unwrap_or(0) as u64,
                ));
                pb.set_style(
                    ProgressStyle::with_template(
                        "  {spinner:.green} {wide_msg} [{bar:30.green/dim}] {bytes}/{total_bytes}",
                    )
                    .unwrap()
                    .progress_chars("=>-"),
                );
                pb.set_message(file_name.clone());

                // Wait for rate limiter before each download.
                limiter.wait().await;

                match file_writer::download_file(
                    &client,
                    &record.remote_url,
                    &dest_path,
                    record.last_modified.as_deref(),
                    Some(&pb),
                    Some(&overall),
                    Some(remaining_bytes),
                )
                .await
                {
                    Ok(checksum) => {
                        // Force the progress bar to 100% in case the actual file size
                        // was slightly smaller than the server's reported size.
                        if let Some(len) = pb.length() {
                            pb.set_position(len);
                        }
                        
                        // Change the style slightly for completed items and leave it on screen
                        pb.set_style(
                            indicatif::ProgressStyle::with_template(
                                "  ✓ {wide_msg} [{bar:30.green/dim}] {bytes}/{total_bytes}",
                            )
                            .unwrap()
                            .progress_chars("=>-"),
                        );
                        pb.finish_with_message(file_name.clone());

                        let conn = db.lock().unwrap();
                        let _ = models::set_file_status(
                            &conn,
                            record.id,
                            "done",
                            None,
                            Some(&checksum),
                        );
                    }
                    Err(e) => {
                        pb.finish_and_clear();
                        let _ = multi.println(format!("✗ {}: {}", record.remote_path, e));
                        let conn = db.lock().unwrap();
                        let _ = models::record_error(&conn, record.id, &e.to_string());
                    }
                }

                overall.inc(1);
            });

            handles.push(handle);
        }

        // Await all spawned tasks.
        for h in handles {
            let _ = h.await;
        }
        overall.finish_and_clear();

        // Check if any new pending files appeared during this batch (from a concurrent crawl).
        let remaining = {
            let conn = db.lock().unwrap();
            models::files_by_status(&conn, "pending")?.len()
        };
        if remaining == 0 {
            break;
        }
    }

    info!("Download phase complete.");
    Ok(())
}

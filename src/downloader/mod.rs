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

const DOWNLOAD_BATCH_SIZE: usize = 128;

/// Whether a remote URL's file extension is in the allowed list (case-insensitive).
fn extension_allowed(remote_url: &str, allowed_extensions: &[String]) -> bool {
    let ext = std::path::Path::new(remote_url)
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    allowed_extensions
        .iter()
        .any(|a| a.eq_ignore_ascii_case(ext))
}

pub async fn run(
    config: &Config,
    db: &Db,
    dry_run: bool,
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    // 1. Initial cleanup/sync: Filter and mark files that don't match allowed extensions as skipped
    // This ensures our "Download queue" log and progress bars are accurate.
    if !config.sync.allowed_extensions.is_empty() {
        let allowed_extensions = config.sync.allowed_extensions.clone();
        crate::db::run_blocking(db, move |conn| {
            let pending = models::files_by_status(conn, "pending")?;
            for record in pending {
                if !extension_allowed(&record.remote_url, &allowed_extensions) {
                    let _ = models::set_file_status(conn, record.id, "skipped", None, None);
                }
            }
            Ok(())
        })
        .await?;
    }

    // 2. Print accurate summary
    {
        let (pending_count, pending_bytes) = crate::db::run_blocking(db, |conn| {
            let pending_bytes = models::pending_bytes(conn)?;
            let pending_count = models::pending_count(conn)?;
            Ok((pending_count, pending_bytes))
        })
        .await?;
        info!(
            "Download queue: {} file(s) / {}",
            pending_count,
            ByteSize(pending_bytes as u64)
        );
    }

    let client = build_client(&config.server.user_agent, config.server.cookie.as_deref())?;
    let limiter = RateLimiter::new(config.concurrency.delay_between_requests_ms);
    let semaphore = Arc::new(Semaphore::new(config.concurrency.max_parallel_downloads));
    let multi = MultiProgress::new();
    let output_dir = config.output.dir.clone();

    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        let mut pending = crate::db::run_blocking(db, |conn| {
            Ok(models::files_by_status_limited(
                conn,
                "pending",
                DOWNLOAD_BATCH_SIZE,
            )?)
        })
        .await?;

        if !config.sync.allowed_extensions.is_empty() {
            let allowed_extensions = config.sync.allowed_extensions.clone();
            let pending_clone = pending.clone();
            let (filtered, to_skip) = crate::db::run_blocking(db, move |conn| {
                let mut filtered = Vec::new();
                let mut to_skip = Vec::new();
                for record in pending_clone {
                    if extension_allowed(&record.remote_url, &allowed_extensions) {
                        filtered.push(record);
                    } else {
                        to_skip.push(record.id);
                    }
                }
                for id in &to_skip {
                    let _ = models::set_file_status(conn, *id, "skipped", None, None);
                }
                Ok((filtered, to_skip))
            })
            .await?;
            pending = filtered;
            for id in to_skip {
                debug!(
                    "Skipping file during download (extension not allowed) ID: {}",
                    id
                );
            }
        }

        if pending.is_empty() {
            // Check if there are any pending files left in the DB. If we skipped the whole
            // batch, we should fetch the next batch. If there are truly no pending files, break.
            let count =
                crate::db::run_blocking(db, |conn| Ok(models::pending_count(conn)?)).await?;
            if count == 0 {
                break;
            } else {
                continue;
            }
        }

        let mut actual_remaining_bytes = 0;
        for record in &pending {
            let mut expected = record.size_bytes.unwrap_or(0) as u64;
            if expected > 0 {
                let dest_path = {
                    let rel = record.remote_path.trim_start_matches('/');
                    output_dir.join(rel)
                };
                let mut part_path = dest_path.clone();
                let name = part_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                part_path.set_file_name(format!("{name}.part"));

                if part_path.exists() {
                    if let Ok(meta) = std::fs::metadata(&part_path) {
                        expected = expected.saturating_sub(meta.len());
                    }
                }
                actual_remaining_bytes += expected;
            }
        }

        let overall = multi.add(ProgressBar::new(pending.len() as u64));
        overall.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] {bar:40.cyan/blue} {pos}/{len} files  ({msg})",
            )
            .unwrap(),
        );
        overall.set_message(format!("{} remaining", ByteSize(actual_remaining_bytes)));

        let remaining_bytes = Arc::new(std::sync::atomic::AtomicU64::new(actual_remaining_bytes));

        let mut handles = Vec::new();

        for record in pending {
            if cancel_token.is_cancelled() {
                break;
            }

            if dry_run {
                info!("[DRY RUN] Would download: {}", record.remote_path);
                let record_id = record.id;
                crate::db::run_blocking(db, move |conn| {
                    Ok(models::set_file_status(
                        conn, record_id, "skipped", None, None,
                    )?)
                })
                .await?;
                overall.inc(1);
                continue;
            }

            // Mark as downloading.
            {
                let record_id = record.id;
                crate::db::run_blocking(db, move |conn| {
                    Ok(models::set_file_status(
                        conn,
                        record_id,
                        "downloading",
                        None,
                        None,
                    )?)
                })
                .await?;
            }

            let client = client.clone();
            let limiter = limiter.clone();
            let db = db.clone();
            let output_dir = output_dir.clone();
            let multi = multi.clone();
            let overall = overall.clone();
            let remaining_bytes = remaining_bytes.clone();
            let redownload_on_mismatch = config.sync.redownload_on_size_mismatch;
            let cancel_token = cancel_token.clone();
            let semaphore = semaphore.clone();
            let backoff_initial = config.rate_limit.backoff_initial_secs;
            let backoff_max = config.rate_limit.backoff_max_secs;
            let backoff_mult = config.rate_limit.backoff_multiplier;

            let handle = tokio::spawn(async move {
                let _permit = tokio::select! {
                    p = semaphore.acquire_owned() => match p {
                        Ok(permit) => permit,
                        Err(e) => {
                            warn!("Semaphore acquire failed: {e}");
                            return;
                        }
                    },
                    _ = cancel_token.cancelled() => {
                        return;
                    }
                };

                let mut ghost_client = match client.to_ghostwire() {
                    Ok(gc) => gc,
                    Err(e) => {
                        warn!("Failed to create Ghostwire client: {e}");
                        let record_id = record.id;
                        let _ = crate::db::run_blocking(&db, move |conn| {
                            Ok(models::set_file_status(
                                conn, record_id, "pending", None, None,
                            )?)
                        })
                        .await;
                        return;
                    }
                };

                if cancel_token.is_cancelled() {
                    let record_id = record.id;
                    let _ = crate::db::run_blocking(&db, move |conn| {
                        Ok(models::set_file_status(
                            conn, record_id, "pending", None, None,
                        )?)
                    })
                    .await;
                    return;
                }

                let dest_path = {
                    let rel = record.remote_path.trim_start_matches('/');
                    output_dir.join(rel)
                };

                // Check if file already exists with matching size (skip if not redownload_on_mismatch).
                let mut already_exists_and_done = false;
                let mut already_exists_and_skipped = false;
                if dest_path.exists() {
                    if let (Some(expected_size), Ok(meta)) =
                        (record.size_bytes, std::fs::metadata(&dest_path))
                    {
                        let local_size = meta.len() as i64;
                        if local_size == expected_size {
                            info!("Already complete (size match): {}", dest_path.display());
                            let record_id = record.id;
                            let _ = crate::db::run_blocking(&db, move |conn| {
                                Ok(models::set_file_status(
                                    conn, record_id, "done", None, None,
                                )?)
                            })
                            .await;
                            overall.inc(1);
                            already_exists_and_done = true;
                        } else if !redownload_on_mismatch {
                            warn!(
                                "Size mismatch but redownload_on_size_mismatch=false, skipping: {}",
                                dest_path.display()
                            );
                            let record_id = record.id;
                            let _ = crate::db::run_blocking(&db, move |conn| {
                                Ok(models::set_file_status(
                                    conn, record_id, "skipped", None, None,
                                )?)
                            })
                            .await;
                            overall.inc(1);
                            already_exists_and_skipped = true;
                        }
                    }
                }

                if already_exists_and_done || already_exists_and_skipped {
                    return;
                }

                // Per-file progress bar.
                let file_name = dest_path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                let pb = multi.add(ProgressBar::new(record.size_bytes.unwrap_or(0) as u64));
                pb.set_style(
                    ProgressStyle::with_template(
                        "  {spinner:.green} {wide_msg} [{bar:30.green/dim}] {bytes}/{total_bytes}",
                    )
                    .unwrap()
                    .progress_chars("=>-"),
                );
                pb.set_message(file_name.clone());

                let mut backoff = backoff_initial;
                let mut attempts = 0;
                let max_attempts = 5;

                loop {
                    attempts += 1;

                    // Wait for rate limiter before each download.
                    let lim_wait = tokio::select! {
                        _ = limiter.wait() => Ok(()),
                        _ = cancel_token.cancelled() => Err(()),
                    };
                    if lim_wait.is_err() {
                        pb.finish_and_clear();
                        multi.remove(&pb);
                        let record_id = record.id;
                        let _ = crate::db::run_blocking(&db, move |conn| {
                            Ok(models::set_file_status(
                                conn, record_id, "pending", None, None,
                            )?)
                        })
                        .await;
                        return;
                    }

                    let download_fut = file_writer::download_file(
                        &mut ghost_client,
                        file_writer::DownloadRequest {
                            cookie: client.cookie.as_deref(),
                            url: &record.remote_url,
                            dest_path: &dest_path,
                            last_modified: record.last_modified.as_deref(),
                        },
                        file_writer::DownloadProgress {
                            file: Some(&pb),
                            overall: Some(&overall),
                            remaining_bytes: Some(remaining_bytes.clone()),
                        },
                    );

                    let result = tokio::select! {
                        res = download_fut => res,
                        _ = cancel_token.cancelled() => {
                            pb.finish_and_clear();
                            multi.remove(&pb);
                            let _ = multi.println(format!("  ✗ {}: Interrupted by user cancellation", record.remote_path));
                            let record_id = record.id;
                            let _ = crate::db::run_blocking(&db, move |conn| {
                                Ok(models::set_file_status(conn, record_id, "pending", None, None)?)
                            }).await;
                            return;
                        }
                    };

                    match result {
                        Ok(checksum) => {
                            // Force the progress bar to 100% and finish it so it stops ticking.
                            if let Some(len) = pb.length() {
                                pb.set_position(len);
                            }
                            pb.finish_and_clear();

                            // Explicitly remove it from MultiProgress to prevent memory/redraw leaks
                            // over long syncs.
                            multi.remove(&pb);

                            // Print a clean, permanent success line
                            let size_str =
                                bytesize::ByteSize(record.size_bytes.unwrap_or(0) as u64)
                                    .to_string();
                            let _ = multi.println(format!("  ✓ {} ({})", file_name, size_str));

                            let record_id = record.id;
                            let checksum_clone = checksum.clone();
                            let _ = crate::db::run_blocking(&db, move |conn| {
                                Ok(models::set_file_status(
                                    conn,
                                    record_id,
                                    "done",
                                    None,
                                    Some(&checksum_clone),
                                )?)
                            })
                            .await;
                            break;
                        }
                        Err(e) => {
                            if attempts >= max_attempts {
                                pb.finish_and_clear();
                                multi.remove(&pb);
                                let _ = multi.println(format!(
                                    "  ✗ {} (failed after {} attempts): {}",
                                    record.remote_path, attempts, e
                                ));
                                let record_id = record.id;
                                let err_msg = e.to_string();
                                let _ = crate::db::run_blocking(&db, move |conn| {
                                    Ok(models::record_error(conn, record_id, &err_msg)?)
                                })
                                .await;
                                break;
                            } else {
                                let _ = multi.println(format!(
                                    "  ⚠ {} failed (attempt {}): {}. Retrying in {}s...",
                                    file_name, attempts, e, backoff
                                ));

                                // Sleep and wait for backoff or cancellation
                                let sleep_res = tokio::select! {
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff)) => Ok(()),
                                    _ = cancel_token.cancelled() => Err(()),
                                };
                                if sleep_res.is_err() {
                                    pb.finish_and_clear();
                                    multi.remove(&pb);
                                    let record_id = record.id;
                                    let _ = crate::db::run_blocking(&db, move |conn| {
                                        Ok(models::set_file_status(
                                            conn, record_id, "pending", None, None,
                                        )?)
                                    })
                                    .await;
                                    return;
                                }

                                backoff =
                                    (backoff as f64 * backoff_mult).min(backoff_max as f64) as u64;
                            }
                        }
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
        let remaining =
            crate::db::run_blocking(db, |conn| Ok(models::pending_count(conn)?)).await?;
        if remaining == 0 {
            break;
        }
    }

    info!("Download phase complete.");
    Ok(())
}

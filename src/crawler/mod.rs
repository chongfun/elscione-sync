pub mod parser;
pub mod rate_limiter;

use anyhow::{Context, Result};
use chrono::DateTime;
use futures::stream::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::db::{models, Db};
use rate_limiter::{build_client, RateLimiter};

/// Crawl the server starting from the configured base URL.
pub async fn run(
    config: &Config,
    db: &Db,
    include_overrides: &[String],
    exclude_overrides: &[String],
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
    let client = build_client(&config.server.user_agent, config.server.cookie.as_deref())?;
    let limiter = RateLimiter::new(config.concurrency.crawl_delay_ms);

    let include_folders: Vec<String> = if !include_overrides.is_empty() {
        include_overrides.to_vec()
    } else {
        config.sync.include_folders.clone()
    };

    let exclude_patterns: Vec<String> = if !exclude_overrides.is_empty() {
        exclude_overrides.to_vec()
    } else {
        config.sync.exclude_patterns.clone()
    };

    // Seed crawl queue if empty, and reset any previous errors.
    {
        let base_url = config.server.base_url.clone();
        crate::db::run_blocking(db, move |conn| {
            if !models::has_pending_crawl(conn)? {
                // If no pending work, clear the queue (done/error entries) to allow a fresh discovery.
                models::clear_crawl_queue(conn)?;
                models::enqueue_crawl(conn, &base_url, 0)?;
                info!(
                    "Crawl queue was empty; starting fresh discovery from {}",
                    base_url
                );
            } else {
                let reset_count = models::reset_crawl_errors(conn)?;
                if reset_count > 0 {
                    info!("Reset {} failed crawl entries to pending.", reset_count);
                }
                info!("Resuming crawl from existing queue entries.");
            }
            Ok(())
        })
        .await?;
    }

    // Spinner to show live crawl activity.
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .unwrap()
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    spinner.enable_steady_tick(Duration::from_millis(80));

    let mut visited: HashSet<String> = HashSet::new();
    let mut files_found: u64 = 0;

    let sem = Arc::new(Semaphore::new(4));

    loop {
        if cancel_token.is_cancelled() {
            break;
        }

        let entries =
            crate::db::run_blocking(db, |conn| Ok(models::next_crawl_entries(conn, 10)?)).await?;

        if entries.is_empty() {
            break;
        }

        let mut futures = futures::stream::FuturesUnordered::new();

        for entry in entries {
            if cancel_token.is_cancelled() {
                break;
            }

            if visited.contains(&entry.url) {
                let entry_id = entry.id;
                crate::db::run_blocking(db, move |conn| {
                    Ok(models::mark_crawl_done(conn, entry_id)?)
                })
                .await?;
                continue;
            }
            visited.insert(entry.url.clone());

            // Apply folder filter at depth 1.
            if entry.depth == 1 && !include_folders.is_empty() {
                let name = last_segment(&entry.url);
                if !include_folders.iter().any(|f| name.contains(f.as_str())) {
                    debug!(
                        "Skipping folder (not in selection): {} (name: {})",
                        entry.url, name
                    );
                    let entry_id = entry.id;
                    crate::db::run_blocking(db, move |conn| {
                        Ok(models::mark_crawl_done(conn, entry_id)?)
                    })
                    .await?;
                    continue;
                } else {
                    debug!("Entering selected folder: {}", entry.url);
                }
            }

            if is_excluded(&entry.url, &exclude_patterns) {
                debug!("Excluded by pattern: {}", entry.url);
                let entry_id = entry.id;
                crate::db::run_blocking(db, move |conn| {
                    Ok(models::mark_crawl_done(conn, entry_id)?)
                })
                .await?;
                continue;
            }

            let sem = sem.clone();
            let client = client.clone();
            let limiter = limiter.clone();
            let url = entry.url.clone();
            let base_url = config.server.base_url.clone();
            let entry_id = entry.id;
            let entry_depth = entry.depth;
            let cancel_token = cancel_token.clone();
            let max_retries = config.concurrency.max_crawl_retries;

            futures.push(tokio::spawn(async move {
                let _permit = tokio::select! {
                    p = sem.acquire_owned() => match p {
                        Ok(permit) => permit,
                        Err(e) => {
                            warn!("Semaphore acquire failed: {e}");
                            return None;
                        }
                    },
                    _ = cancel_token.cancelled() => return None,
                };

                let mut ghost_client = match client.to_ghostwire() {
                    Ok(gc) => gc,
                    Err(e) => {
                        warn!("Failed to create Ghostwire client: {e}");
                        return None;
                    }
                };

                let cookie = client.cookie.clone();
                let mut attempts = 0;
                let res = loop {
                    attempts += 1;

                    let res = tokio::select! {
                        res = fetch_directory(&mut ghost_client, cookie.as_deref(), &limiter, &url, &base_url) => res,
                        _ = cancel_token.cancelled() => return None,
                    };

                    match res {
                        Ok(entries) => break Ok(entries),
                        Err(e) => {
                            if attempts >= max_retries {
                                break Err(e);
                            } else {
                                warn!("Attempt {} to fetch {} failed: {}. Retrying...", attempts, url, e);
                                let sleep_res = tokio::select! {
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => Ok(()),
                                    _ = cancel_token.cancelled() => Err(()),
                                };
                                if sleep_res.is_err() {
                                    return None;
                                }
                            }
                        }
                    }
                };

                Some((entry_id, entry_depth, url, res))
            }));
        }

        while let Some(res) = futures.next().await {
            let (entry_id, entry_depth, url, fetch_res) = match res {
                Ok(Some(val)) => val,
                _ => continue, // either joined thread panicked or it was cancelled
            };

            spinner.set_message(format!("Crawling ({} files found): {}", files_found, url));

            match fetch_res {
                Ok(dir_entries) => {
                    if dir_entries.is_empty() {
                        warn!("No entries found at {} — server may require JavaScript or block crawlers", url);
                    }

                    let allowed_extensions = config.sync.allowed_extensions.clone();
                    let base_url = config.server.base_url.clone();
                    let db_clone = db.clone();
                    let dir_entries_clone = dir_entries.clone();

                    let added_files = crate::db::run_blocking(&db_clone, move |conn| {
                        let tx = conn.unchecked_transaction()?;
                        let mut added = 0;
                        for de in &dir_entries_clone {
                            if de.is_dir {
                                models::enqueue_crawl(&tx, &de.url, entry_depth + 1)?;
                            } else {
                                // Apply extension filter
                                if !allowed_extensions.is_empty()
                                    && !crate::downloader::extension_allowed(
                                        &de.url,
                                        &allowed_extensions,
                                    )
                                {
                                    debug!("Skipping file (extension not allowed): {}", de.url);
                                    continue;
                                }

                                let remote_path = url_to_path(&de.url, &base_url);
                                models::upsert_file(
                                    &tx,
                                    &de.url,
                                    &remote_path,
                                    de.last_modified.as_deref(),
                                    de.size_bytes,
                                )?;
                                added += 1;
                            }
                        }
                        models::mark_crawl_done(&tx, entry_id)?;
                        tx.commit()?;
                        Ok(added)
                    })
                    .await?;
                    files_found += added_files;
                }
                Err(e) => {
                    warn!("Failed to fetch {url}: {e}");
                    let db_clone = db.clone();
                    crate::db::run_blocking(&db_clone, move |conn| {
                        Ok(models::mark_crawl_error(conn, entry_id)?)
                    })
                    .await?;
                }
            }
        }
    }

    spinner.finish_and_clear();
    info!("Crawl complete — {} file(s) discovered.", files_found);
    Ok(())
}

/// Fetch a directory listing via the h5ai JSON API.
async fn fetch_directory(
    client: &mut ghostwire::Ghostwire,
    cookie: Option<&str>,
    limiter: &RateLimiter,
    url: &str,
    base_url: &str,
) -> Result<Vec<parser::DirEntry>> {
    limiter.wait().await;
    try_h5ai(client, cookie, base_url, url).await
}

/// Attempt to retrieve a directory listing via the h5ai JSON API.
/// Returns an error if the API is unavailable or returns an invalid response.
pub(crate) async fn try_h5ai(
    client: &mut ghostwire::Ghostwire,
    cookie: Option<&str>,
    base_url: &str,
    dir_url: &str,
) -> Result<Vec<parser::DirEntry>> {
    let base = base_url.trim_end_matches('/');

    // Extract the URL path relative to the server root, e.g. "/" or "/Manga/".
    let mut href = dir_url
        .strip_prefix(base)
        .map(|s| s.to_owned())
        .unwrap_or_else(|| "/".to_owned());

    if !href.starts_with('/') {
        href = format!("/{href}");
    }
    if href.is_empty() {
        href = "/".to_owned();
    }

    // ── Step 1: GET the page to extract the h5ai CSRF token ("clckd"). ──────
    let mut get_opts = ghostwire::RequestOptions::default();
    if let Some(cookie_str) = cookie {
        if let Ok(val) = reqwest::header::HeaderValue::from_str(cookie_str) {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::COOKIE, val);
            get_opts.headers = Some(headers);
        }
    }

    let page_html = client
        .request(reqwest::Method::GET, dir_url, get_opts)
        .await
        .with_context(|| format!("h5ai page GET failed for {dir_url}"))?
        .text()
        .await
        .with_context(|| format!("reading h5ai page body for {dir_url}"))?;
    let clckd = extract_clckd(&page_html);

    // ── Step 2: POST to the h5ai API. ────────────────────────────────────────
    // h5ai expects a raw JSON body with "items" (singular) containing the href.
    let json_payload = serde_json::json!({
        "action": "get",
        "items": {
            "href": href,
            "what": 1
        }
    });

    let api_url = format!("{base}/?");

    let mut opts = ghostwire::RequestOptions::default();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        "Content-Type",
        reqwest::header::HeaderValue::from_static("application/json;charset=utf-8"),
    );

    if let Some(cookie_str) = cookie {
        if let Ok(val) = reqwest::header::HeaderValue::from_str(cookie_str) {
            headers.insert(reqwest::header::COOKIE, val);
        }
    }

    if let Some(token) = &clckd {
        if let Ok(val) = reqwest::header::HeaderValue::from_str(token) {
            headers.insert("x-h5ai-clckd", val);
        }
    }
    opts.headers = Some(headers);
    opts.body_bytes = Some(bytes::Bytes::from(serde_json::to_vec(&json_payload)?));

    let resp = client
        .request(reqwest::Method::POST, &api_url, opts)
        .await
        .with_context(|| format!("h5ai API POST failed for {api_url}"))?;

    let status = resp.status();

    if !status.is_success() {
        anyhow::bail!("h5ai API returned HTTP {status} for {api_url}");
    }

    let text = resp
        .text()
        .await
        .with_context(|| format!("reading h5ai API body for {api_url}"))?;

    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing h5ai JSON from {api_url}"))?;

    parse_h5ai_response(&json, base, &href)
}

/// Extract the h5ai CSRF token from the `<meta name="clckd">` tag.
fn extract_clckd(html: &str) -> Option<String> {
    // Look for: <meta name="clckd" content="HEXTOKEN" />
    let needle = r#"name="clckd" content=""#;
    let start = html.find(needle)? + needle.len();
    let end = html[start..].find('"')? + start;
    Some(html[start..end].to_owned())
}

/// Parse an h5ai JSON API response into `DirEntry` values.
fn parse_h5ai_response(
    json: &serde_json::Value,
    base_url: &str,
    current_href: &str,
) -> Result<Vec<parser::DirEntry>> {
    let items = json
        .get("items")
        .and_then(|items| items.as_array())
        .context("h5ai response missing items array")?;

    let mut entries = Vec::new();
    let current = current_href.trim_end_matches('/');
    let parsed_base = reqwest::Url::parse(base_url).ok();

    for item in items {
        let Some(href) = item.get("href").and_then(|href| href.as_str()) else {
            debug!("Skipping h5ai entry without href: {item}");
            continue;
        };
        let h = href.trim_end_matches('/');

        // Skip the directory itself and parent directory.
        if h == current || h.is_empty() || h == "/" || !href.starts_with(current_href) {
            continue;
        }

        let is_dir = href.ends_with('/');
        let name = last_segment(href);

        if name.is_empty() {
            continue;
        }

        let url = parsed_base
            .as_ref()
            .and_then(|base| base.join(href).ok())
            .map(|joined| joined.to_string())
            .unwrap_or_else(|| {
                format!(
                    "{}/{}",
                    base_url.trim_end_matches('/'),
                    href.trim_start_matches('/')
                )
            });

        // h5ai provides timestamps in milliseconds since epoch.
        let last_modified = item
            .get("time")
            .and_then(|t| t.as_i64())
            .and_then(|ms| DateTime::from_timestamp(ms / 1000, 0))
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string());

        let size_bytes = item
            .get("size")
            .and_then(|s| s.as_i64())
            .filter(|&s| s >= 0);

        entries.push(parser::DirEntry {
            url,
            name,
            is_dir,
            last_modified,
            size_bytes,
        });
    }

    Ok(entries)
}

fn url_to_path(url: &str, base: &str) -> String {
    let base = base.trim_end_matches('/');
    match url.strip_prefix(base) {
        Some(s) if s.starts_with('/') => s.to_owned(),
        Some(s) => format!("/{s}"),
        None => url.to_owned(),
    }
}

fn last_segment(url: &str) -> String {
    url.trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_owned()
}

fn is_excluded(url: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| {
        let pattern = p.trim_end_matches("/**").trim_end_matches("/*");
        url.contains(pattern)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h5ai_response_errors_when_items_are_missing() {
        let json = serde_json::json!({ "error": "server failure" });

        let result = parse_h5ai_response(&json, "https://example.test", "/");

        assert!(result.is_err());
    }

    #[test]
    fn h5ai_response_parses_files_and_directories() {
        let json = serde_json::json!({
            "items": [
                { "href": "/" },
                { "href": "/Books/", "time": 0 },
                { "href": "/Books/example.epub", "time": 60_000, "size": 12345 },
                { "not_href": true },
            ]
        });

        let entries = parse_h5ai_response(&json, "https://example.test", "/").unwrap();

        assert_eq!(entries.len(), 2);

        let dir = &entries[0];
        assert!(dir.is_dir);
        assert_eq!(dir.name, "Books");
        assert_eq!(dir.url, "https://example.test/Books/");
        assert_eq!(dir.size_bytes, None);

        let file = &entries[1];
        assert!(!file.is_dir);
        assert_eq!(file.name, "example.epub");
        assert_eq!(file.url, "https://example.test/Books/example.epub");
        assert_eq!(file.size_bytes, Some(12345));
        assert_eq!(file.last_modified.as_deref(), Some("1970-01-01 00:01"));
    }

    #[test]
    fn extract_clckd_finds_meta_token() {
        let html = r#"<head><meta name="clckd" content="abc123"/></head>"#;

        assert_eq!(extract_clckd(html), Some("abc123".to_owned()));
        assert_eq!(extract_clckd("<html></html>"), None);
    }

    #[test]
    fn url_to_path_strips_base_url() {
        assert_eq!(
            url_to_path("https://example.test/Books/a.epub", "https://example.test/"),
            "/Books/a.epub"
        );
        assert_eq!(
            url_to_path("https://other.test/x", "https://example.test/"),
            "https://other.test/x"
        );
    }
}

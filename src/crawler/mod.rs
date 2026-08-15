pub mod parser;
pub mod rate_limiter;
pub mod session;

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
use rate_limiter::RateLimiter;
use session::ElscioneSession;

/// Crawl the server starting from the configured base URL.
pub async fn run(
    config: &Config,
    session: &ElscioneSession,
    db: &Db,
    include_overrides: &[String],
    exclude_overrides: &[String],
    cancel_token: tokio_util::sync::CancellationToken,
) -> Result<()> {
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

    let sem = Arc::new(Semaphore::new(config.concurrency.max_parallel_crawls));

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

            // Apply folder filter at depth 1. Exact name match, so selecting
            // "Art" does not also pull in "Articles".
            if entry.depth == 1 && !include_folders.is_empty() {
                let name = last_segment(&entry.url);
                if !include_folders.contains(&name) {
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

            if is_excluded(
                &url_to_path(&entry.url, &config.server.base_url),
                &exclude_patterns,
            ) {
                debug!("Excluded by pattern: {}", entry.url);
                let entry_id = entry.id;
                crate::db::run_blocking(db, move |conn| {
                    Ok(models::mark_crawl_done(conn, entry_id)?)
                })
                .await?;
                continue;
            }

            let sem = sem.clone();
            let session = session.clone();
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

                let mut attempts = 0;
                let res = loop {
                    attempts += 1;

                    let res = tokio::select! {
                        res = fetch_directory(&session, &limiter, &url, &base_url) => res,
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
                    let exclude_patterns = exclude_patterns.clone();
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
                                if is_excluded(&remote_path, &exclude_patterns) {
                                    debug!("Excluded by pattern: {}", de.url);
                                    continue;
                                }
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
    session: &ElscioneSession,
    limiter: &RateLimiter,
    url: &str,
    base_url: &str,
) -> Result<Vec<parser::DirEntry>> {
    try_h5ai(session, limiter, base_url, url).await
}

/// Attempt to retrieve a directory listing via the h5ai JSON API.
/// Returns an error if the API is unavailable or returns an invalid response.
pub(crate) async fn try_h5ai(
    session: &ElscioneSession,
    limiter: &RateLimiter,
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
    limiter.wait().await;
    let (get_status, _headers, page_html) = session
        .get_html(dir_url)
        .await
        .with_context(|| format!("h5ai page GET failed for {dir_url}"))?;

    if !get_status.is_success() {
        let title = parser::extract_title(&page_html);
        anyhow::bail!(
            "h5ai page GET returned HTTP {get_status} for {dir_url} (title: {title:?})"
        );
    }

    let clckd = match parser::extract_clckd(&page_html) {
        Some(token) => token,
        None => {
            let title = parser::extract_title(&page_html);
            anyhow::bail!(
                "h5ai CSRF token ('clckd') not found in page HTML for {dir_url} (HTTP {get_status}, title: {title:?}). Server may require a Cloudflare session or have changed its markup."
            );
        }
    };

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

    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(val) = reqwest::header::HeaderValue::from_str(&clckd) {
        headers.insert("x-h5ai-clckd", val);
    }

    limiter.wait().await;
    let (post_status, _post_headers, text) = session
        .post_json(&api_url, &json_payload, Some(headers))
        .await
        .with_context(|| format!("h5ai API POST failed for {api_url}"))?;

    if !post_status.is_success() {
        anyhow::bail!("h5ai API returned HTTP {post_status} for {api_url}");
    }

    let json: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing h5ai JSON from {api_url}"))?;

    parse_h5ai_response(&json, base, &href)
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

/// Whether a server-relative path matches any exclude pattern.
///
/// Patterns containing `/` are globbed against the whole relative path
/// (e.g. `Manga/**`); bare patterns are globbed against each path segment
/// (e.g. `Drafts`, `*.zip`).
fn is_excluded(rel_path: &str, patterns: &[String]) -> bool {
    let rel = rel_path.trim_matches('/');
    patterns.iter().any(|p| {
        let pat = p.trim_start_matches('/');
        match glob::Pattern::new(pat) {
            Ok(g) => {
                if pat.contains('/') {
                    g.matches(rel)
                } else {
                    rel.split('/').any(|segment| g.matches(segment))
                }
            }
            Err(e) => {
                warn!("Invalid exclude pattern {p:?} ({e}); treating as substring match");
                rel.contains(pat)
            }
        }
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
    fn exclude_patterns_with_slash_glob_the_full_path() {
        let patterns = vec!["Manga/**".to_owned()];

        assert!(is_excluded("/Manga/Series", &patterns));
        assert!(is_excluded("/Manga/Series/vol1.epub", &patterns));
        assert!(!is_excluded("/MangaExtra/Series", &patterns));
        assert!(!is_excluded("/Books/Manga-Guide.epub", &patterns));
    }

    #[test]
    fn bare_exclude_patterns_glob_each_segment() {
        let by_name = vec!["Drafts".to_owned()];
        assert!(is_excluded("/Books/Drafts", &by_name));
        assert!(is_excluded("/Books/Drafts/notes.txt", &by_name));
        assert!(!is_excluded("/Books/Drafts2", &by_name));

        let by_ext = vec!["*.zip".to_owned()];
        assert!(is_excluded("/Books/archive.zip", &by_ext));
        assert!(!is_excluded("/Books/book.epub", &by_ext));
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

    #[tokio::test]
    async fn test_session_cookie_reuse() {
        let mock_server = wiremock::MockServer::start().await;

        // Step 1: Initial page GET returns Set-Cookie and clckd meta
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/dir/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .append_header("Set-Cookie", "cf_clearance=valid_clearance_123; Path=/")
                    .set_body_string(r#"<html><head><meta name="clckd" content="test_clckd_token"></head><body></body></html>"#),
            )
            .mount(&mock_server)
            .await;

        // Step 2: API POST requires Cookie: cf_clearance=valid_clearance_123 and x-h5ai-clckd header
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/"))
            .and(wiremock::matchers::header("x-h5ai-clckd", "test_clckd_token"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({
                        "items": [
                            { "href": "/dir/" },
                            { "href": "/dir/file.epub", "time": 1000, "size": 100 }
                        ]
                    })),
            )
            .mount(&mock_server)
            .await;

        let session = ElscioneSession::new(&mock_server.uri(), "test-ua", None).unwrap();
        let limiter = RateLimiter::new(0);
        let dir_url = format!("{}/dir/", mock_server.uri());
        let entries = try_h5ai(&session, &limiter, &mock_server.uri(), &dir_url).await.unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "file.epub");
    }

    #[tokio::test]
    async fn test_challenge_redirect_cookie_shared_with_binary_download() {
        let mock_server = wiremock::MockServer::start().await;

        // 1. Initial request to /auth redirects to /welcome and sets cookie on the 302 redirect
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/auth"))
            .respond_with(
                wiremock::ResponseTemplate::new(302)
                    .append_header("Location", "/welcome")
                    .append_header("Set-Cookie", "cf_clearance=redirect_secret_cookie; Path=/"),
            )
            .mount(&mock_server)
            .await;

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/welcome"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_string("<html><body>Welcome</body></html>"),
            )
            .mount(&mock_server)
            .await;

        // 2. Binary download request to /download.bin REQUIRES the cookie that was set during redirect
        let binary_payload = vec![0xDE, 0xAD, 0xBE, 0xEF, 0xFF, 0x00];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/download.bin"))
            .and(wiremock::matchers::header("cookie", "cf_clearance=redirect_secret_cookie"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(binary_payload.clone()),
            )
            .mount(&mock_server)
            .await;

        let session = ElscioneSession::new(&mock_server.uri(), "test-ua", None).unwrap();

        // Perform initial clearance check through Ghostwire on /auth (follows redirect, sets cookie)
        session.ensure_cleared(&format!("{}/auth", mock_server.uri())).await.unwrap();

        // Download file through raw binary streaming client (reqwest::Client)
        let temp_dir = std::env::temp_dir().join("elscione_test_redirect_cookie");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let dest = temp_dir.join("download.bin");

        let checksum = crate::downloader::file_writer::download_file(
            session.http_client(),
            crate::downloader::file_writer::DownloadRequest {
                url: &format!("{}/download.bin", mock_server.uri()),
                dest_path: &dest,
                last_modified: None,
            },
            crate::downloader::file_writer::DownloadProgress::default(),
            None,
        )
        .await
        .unwrap();

        let data = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(data, binary_payload);
        assert!(!checksum.is_empty());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_stale_configured_cookie_overwritten_by_challenge_in_download() {
        let mock_server = wiremock::MockServer::start().await;

        // Challenge probe endpoint replaces the stale cookie with a fresh one
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/probe"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .append_header("Set-Cookie", "cf_clearance=FRESH_COOKIE_456; Path=/")
                    .set_body_string("<html><head><title>OK</title></head><body>Probe OK</body></html>"),
            )
            .mount(&mock_server)
            .await;

        // Binary download REQUIRES the FRESH cookie; sending the STALE cookie would fail
        let binary_payload = vec![0xCA, 0xFE, 0xBA, 0xBE];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/book.epub"))
            .and(wiremock::matchers::header("cookie", "cf_clearance=FRESH_COOKIE_456"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(binary_payload.clone()),
            )
            .mount(&mock_server)
            .await;

        // Session starts with a configured stale cookie
        let session = ElscioneSession::new(
            &mock_server.uri(),
            "test-ua",
            Some("cf_clearance=STALE_COOKIE_123"),
        )
        .unwrap();

        // Ensure cleared updates the jar with the fresh cookie
        session
            .ensure_cleared(&format!("{}/probe", mock_server.uri()))
            .await
            .unwrap();

        let temp_dir = std::env::temp_dir().join("elscione_test_stale_cookie_refresh");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let dest = temp_dir.join("book.epub");

        let checksum = crate::downloader::file_writer::download_file(
            session.http_client(),
            crate::downloader::file_writer::DownloadRequest {
                url: &format!("{}/book.epub", mock_server.uri()),
                dest_path: &dest,
                last_modified: None,
            },
            crate::downloader::file_writer::DownloadProgress::default(),
            None,
        )
        .await
        .unwrap();

        let data = tokio::fs::read(&dest).await.unwrap();
        assert_eq!(data, binary_payload);
        assert!(!checksum.is_empty());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_downloader_403_triggers_single_flight_session_refresh() {
        let mock_server = wiremock::MockServer::start().await;

        // Initial base URL probe for session setup
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .append_header("Set-Cookie", "cf_clearance=refreshed_clearance_789; Path=/")
                    .set_body_string("<html><head><title>Index</title></head><body>Index</body></html>"),
            )
            .mount(&mock_server)
            .await;

        // First attempt without fresh cookie returns 403 Forbidden
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/file1.epub"))
            .respond_with(wiremock::ResponseTemplate::new(403))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        // Second attempt with refreshed cookie returns 200 OK
        let binary_payload = vec![1, 2, 3, 4, 5];
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/file1.epub"))
            .and(wiremock::matchers::header("cookie", "cf_clearance=refreshed_clearance_789"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(binary_payload.clone()),
            )
            .mount(&mock_server)
            .await;

        let temp_dir = std::env::temp_dir().join("elscione_test_403_refresh");
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let db_path = temp_dir.join("state.db");
        let out_dir = temp_dir.join("output");
        tokio::fs::create_dir_all(&out_dir).await.unwrap();

        let db = crate::db::open_at(&db_path).unwrap();
        let file_url = format!("{}/file1.epub", mock_server.uri());
        crate::db::run_blocking(&db, move |conn| {
            crate::db::models::upsert_file(
                conn,
                &file_url,
                "/file1.epub",
                None,
                Some(5),
            )?;
            Ok(())
        })
        .await
        .unwrap();

        let mut config = crate::config::Config::default();
        config.server.base_url = mock_server.uri();
        config.output.dir = out_dir.clone();
        config.concurrency.delay_between_requests_ms = 0;

        let session = ElscioneSession::new(&mock_server.uri(), "test-ua", None).unwrap();
        let cancel_token = tokio_util::sync::CancellationToken::new();

        crate::downloader::run(&config, &session, &db, false, cancel_token).await.unwrap();

        let status = crate::db::run_blocking(&db, |conn| {
            let files = crate::db::models::files_by_status(conn, "done")?;
            Ok(files)
        })
        .await
        .unwrap();

        assert_eq!(status.len(), 1);
        assert_eq!(status[0].remote_path, "/file1.epub");

        let data = tokio::fs::read(out_dir.join("file1.epub")).await.unwrap();
        assert_eq!(data, binary_payload);

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}

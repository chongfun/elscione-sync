pub mod parser;
pub mod rate_limiter;

use anyhow::Result;
use chrono::DateTime;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::time::Duration;
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
) -> Result<()> {
    let client = build_client(&config.server.user_agent)?;
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
        let conn = db.lock().unwrap();
        
        if !models::has_pending_crawl(&conn)? {
            // If no pending work, clear the queue (done/error entries) to allow a fresh discovery.
            models::clear_crawl_queue(&conn)?;
            models::enqueue_crawl(&conn, &config.server.base_url, 0)?;
            info!("Crawl queue was empty; starting fresh discovery from {}", config.server.base_url);
        } else {
            let reset_count = models::reset_crawl_errors(&conn)?;
            if reset_count > 0 {
                info!("Reset {} failed crawl entries to pending.", reset_count);
            }
            info!("Resuming crawl from existing queue entries.");
        }
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

    loop {
        let entries = {
            let conn = db.lock().unwrap();
            models::next_crawl_entries(&conn, 10)?
        };

        if entries.is_empty() {
            break;
        }

        for entry in entries {
            if visited.contains(&entry.url) {
                let conn = db.lock().unwrap();
                models::mark_crawl_done(&conn, entry.id)?;
                continue;
            }
            visited.insert(entry.url.clone());

            // Apply folder filter at depth 1.
            if entry.depth == 1 && !include_folders.is_empty() {
                let name = last_segment(&entry.url);
                if !include_folders.iter().any(|f| name.contains(f.as_str())) {
                    debug!("Skipping folder (not in selection): {} (name: {})", entry.url, name);
                    let conn = db.lock().unwrap();
                    models::mark_crawl_done(&conn, entry.id)?;
                    continue;
                } else {
                    debug!("Entering selected folder: {}", entry.url);
                }
            }

            if is_excluded(&entry.url, &exclude_patterns) {
                debug!("Excluded by pattern: {}", entry.url);
                let conn = db.lock().unwrap();
                models::mark_crawl_done(&conn, entry.id)?;
                continue;
            }

            spinner.set_message(format!(
                "Crawling ({} files found): {}",
                files_found,
                entry.url
            ));

            let dir_entries = match fetch_directory(&client, &limiter, &entry.url, &config.server.base_url).await {
                Ok(e) => e,
                Err(e) => {
                    warn!("Failed to fetch {}: {e}", entry.url);
                    let conn = db.lock().unwrap();
                    conn.execute(
                        "UPDATE crawl_queue SET status='error' WHERE id=?1",
                        rusqlite::params![entry.id],
                    )?;
                    continue;
                }
            };

            if dir_entries.is_empty() {
                warn!("No entries found at {} — server may require JavaScript or block crawlers", entry.url);
            }

            {
                let conn = db.lock().unwrap();
                for de in &dir_entries {
                    if de.is_dir {
                        models::enqueue_crawl(&conn, &de.url, entry.depth + 1)?;
                    } else {
                        // Apply extension filter
                        if !config.sync.allowed_extensions.is_empty() {
                            let ext = std::path::Path::new(&de.url)
                                .extension()
                                .and_then(|s| s.to_str())
                                .unwrap_or("");
                            
                            if !config.sync.allowed_extensions.iter().any(|allowed| allowed.eq_ignore_ascii_case(ext)) {
                                debug!("Skipping file (extension not allowed): {}", de.url);
                                continue;
                            }
                        }

                        let remote_path = url_to_path(&de.url, &config.server.base_url);
                        models::upsert_file(
                            &conn,
                            &de.url,
                            &remote_path,
                            de.last_modified.as_deref(),
                            de.size_bytes,
                        )?;
                        files_found += 1;
                    }
                }
                models::mark_crawl_done(&conn, entry.id)?;
            }
        }
    }

    spinner.finish_and_clear();
    info!("Crawl complete — {} file(s) discovered.", files_found);
    Ok(())
}

/// Fetch a directory listing via the h5ai JSON API.
async fn fetch_directory(
    client: &reqwest::Client,
    limiter: &RateLimiter,
    url: &str,
    base_url: &str,
) -> Result<Vec<parser::DirEntry>> {
    limiter.wait().await;
    match try_h5ai(client, base_url, url).await {
        Some(entries) => Ok(entries),
        None => {
            warn!("h5ai API returned no data for {url}");
            Ok(vec![])
        }
    }
}

/// Attempt to retrieve a directory listing via the h5ai JSON API.
/// Returns `None` if the API is not available on this server.
pub(crate) async fn try_h5ai(
    client: &reqwest::Client,
    base_url: &str,
    dir_url: &str,
) -> Option<Vec<parser::DirEntry>> {
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
    let page_html = match client.get(dir_url).send().await {
        Ok(r) => r.text().await.unwrap_or_default(),
        Err(e) => {
            debug!("  [h5ai] GET {dir_url} failed: {e}");
            String::new()
        }
    };
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

    let mut req = client
        .post(&api_url)
        .header("Content-Type", "application/json;charset=utf-8")
        .json(&json_payload);

    if let Some(token) = &clckd {
        req = req.header("x-h5ai-clckd", token);
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            debug!("  [h5ai] POST failed: {e}");
            return None;
        }
    };

    let status = resp.status();

    if !status.is_success() {
        return None;
    }

    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            debug!("  [h5ai] Failed to read response body: {e}");
            return None;
        }
    };

    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(j) => j,
        Err(e) => {
            debug!("  [h5ai] JSON parse error: {e}");
            return None;
        }
    };

    let entries = parse_h5ai_response(&json, base, &href);
    entries
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
) -> Option<Vec<parser::DirEntry>> {
    let items = json.get("items")?.as_array()?;

    let mut entries = Vec::new();
    let current = current_href.trim_end_matches('/');

    for item in items {
        let href = item.get("href")?.as_str()?;
        let h = href.trim_end_matches('/');

        // Skip the directory itself and parent directory.
        if h == current || h.is_empty() || h == "/" || !href.starts_with(current_href) {
            continue;
        }

        let is_dir = href.ends_with('/');
        let name = href
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
            .to_owned();

        if name.is_empty() {
            continue;
        }

        let url = format!("{}/{}", base_url.trim_end_matches('/'), href.trim_start_matches('/'));

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

    Some(entries)
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

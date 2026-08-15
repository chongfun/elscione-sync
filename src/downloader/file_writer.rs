use filetime::FileTime;
use futures::StreamExt;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("HTTP {status} for {url}")]
    HttpStatus {
        status: reqwest::StatusCode,
        retry_after: Option<Duration>,
        url: String,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct DownloadRequest<'a> {
    pub cookie: Option<&'a str>,
    pub url: &'a str,
    pub dest_path: &'a Path,
    pub last_modified: Option<&'a str>,
}

#[derive(Default)]
pub struct DownloadProgress<'a> {
    pub file: Option<&'a ProgressBar>,
    pub overall: Option<&'a ProgressBar>,
    pub remaining_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
}

/// Path of the `.part` temp file used while downloading `dest_path`.
pub fn part_path_for(dest_path: &Path) -> PathBuf {
    let mut p = dest_path.to_path_buf();
    let name = p
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    p.set_file_name(format!("{name}.part"));
    p
}

/// Parse `Retry-After` header value into a `Duration`.
///
/// Supports integer seconds (e.g. `120`) and RFC 2822 / RFC 1123 HTTP dates.
pub fn parse_retry_after(header_val: &str) -> Option<Duration> {
    let s = header_val.trim();
    if let Ok(seconds) = s.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc2822(s) {
        let now = chrono::Utc::now();
        let target = date.with_timezone(&chrono::Utc);
        if target > now {
            if let Ok(dur) = (target - now).to_std() {
                return Some(dur);
            }
        }
    }
    None
}

/// Download `url` to `dest_path`, writing via a `.part` temp file and atomically
/// renaming on completion.
///
/// Streams raw bytes directly via `reqwest::Client` without intermediate UTF-8 string conversion,
/// ensuring byte-transparency for binary files (EPUB, PDF, ZIP, etc.).
///
/// Returns the hex-encoded SHA-256 checksum of the downloaded bytes.
pub async fn download_file(
    client: &reqwest::Client,
    request: DownloadRequest<'_>,
    progress: DownloadProgress<'_>,
) -> Result<String, DownloadError> {
    let DownloadRequest {
        cookie,
        url,
        dest_path,
        last_modified,
    } = request;

    // Ensure parent directory exists.
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let part_path: PathBuf = part_path_for(dest_path);

    let mut resumed_bytes = 0;
    if part_path.exists() {
        if let Ok(meta) = fs::metadata(&part_path).await {
            let len = meta.len();
            if len > 0 {
                resumed_bytes = len;
            }
        }
    }

    let mut req = client.get(url);
    if resumed_bytes > 0 {
        req = req.header(
            reqwest::header::RANGE,
            format!("bytes={}-", resumed_bytes),
        );
    }
    if let Some(cookie_str) = cookie {
        req = req.header(reqwest::header::COOKIE, cookie_str);
    }

    let mut response = req
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;

    // Handle 416 Range Not Satisfiable by truncating and retrying from 0
    if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        resumed_bytes = 0;
        if part_path.exists() {
            let _ = fs::remove_file(&part_path).await;
        }
        let mut retry_req = client.get(url);
        if let Some(cookie_str) = cookie {
            retry_req = retry_req.header(reqwest::header::COOKIE, cookie_str);
        }
        response = retry_req
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} (retry after 416): {e}"))?;
    }

    if !response.status().is_success() {
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);

        return Err(DownloadError::HttpStatus {
            status,
            retry_after,
            url: url.to_string(),
        });
    }

    // A 206 must resume exactly where we asked, or appending would corrupt the
    // file. Drop the part file so the retry restarts from scratch.
    if response.status() == reqwest::StatusCode::PARTIAL_CONTENT && resumed_bytes > 0 {
        let range_start = response
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(content_range_start);
        if range_start != Some(resumed_bytes) {
            let _ = fs::remove_file(&part_path).await;
            return Err(DownloadError::Other(anyhow::anyhow!(
                "206 response resumed at {range_start:?} instead of requested offset \
                 {resumed_bytes} for {url}; discarding partial file"
            )));
        }
    }

    let mut hasher = Sha256::new();
    let mut file = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT && resumed_bytes > 0
    {
        // Read existing bytes from the part file to update the hasher.
        let mut file_read = File::open(&part_path).await?;
        let mut buffer = vec![0u8; 65536];
        loop {
            let bytes_read = tokio::io::AsyncReadExt::read(&mut file_read, &mut buffer).await?;
            if bytes_read == 0 {
                break;
            }
            hasher.update(&buffer[..bytes_read]);
        }
        drop(file_read);

        if let Some(p) = progress.file {
            p.inc(resumed_bytes);
        }

        fs::OpenOptions::new()
            .write(true)
            .append(true)
            .open(&part_path)
            .await?
    } else {
        if part_path.exists() {
            let _ = fs::remove_file(&part_path).await;
        }
        File::create(&part_path).await?
    };

    let mut stream = response.bytes_stream();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("stream error: {e}"))?;
        let len = chunk.len() as u64;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;

        if let Some(p) = progress.file {
            p.inc(len);
        }
        if let (Some(o), Some(rem)) = (progress.overall, &progress.remaining_bytes) {
            let prev = rem
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |v| Some(v.saturating_sub(len)),
                )
                .unwrap_or(0);
            let current_rem = prev.saturating_sub(len);
            o.set_message(format!("{} remaining", bytesize::ByteSize(current_rem)));
        }
    }
    file.sync_all().await?;
    drop(file);

    // Atomic rename.
    fs::rename(&part_path, dest_path).await?;
    debug!("Saved: {}", dest_path.display());

    // Set file mtime to match server Last-Modified.
    if let Some(lm) = last_modified {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(lm, "%Y-%m-%d %H:%M") {
            let epoch = dt.and_utc().timestamp();
            let ft = FileTime::from_unix_time(epoch, 0);
            if let Err(e) = filetime::set_file_mtime(dest_path, ft) {
                warn!("Could not set mtime on {}: {e}", dest_path.display());
            }
        }
    }

    let checksum = format!("{:x}", hasher.finalize());
    Ok(checksum)
}

/// Parse the start offset from a `Content-Range` header value like
/// `bytes 100-999/1000`. Returns `None` for unsatisfiable (`bytes */1000`)
/// or malformed values.
fn content_range_start(value: &str) -> Option<u64> {
    value
        .trim()
        .strip_prefix("bytes ")?
        .split('-')
        .next()?
        .trim()
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_start_parses_valid_headers() {
        assert_eq!(content_range_start("bytes 100-999/1000"), Some(100));
        assert_eq!(content_range_start("bytes 0-0/1"), Some(0));
    }

    #[test]
    fn content_range_start_rejects_malformed_headers() {
        assert_eq!(content_range_start("bytes */1000"), None);
        assert_eq!(content_range_start("items 100-999/1000"), None);
        assert_eq!(content_range_start(""), None);
    }

    #[test]
    fn test_parse_retry_after() {
        assert_eq!(parse_retry_after("120"), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("0"), Some(Duration::from_secs(0)));
        assert_eq!(parse_retry_after("invalid"), None);
    }

    #[tokio::test]
    async fn test_binary_roundtrip_invalid_utf8() {
        let mock_server = wiremock::MockServer::start().await;
        // Byte payload containing deliberate non-UTF-8 sequences (high bytes, invalid lead bytes)
        let invalid_utf8_data = vec![
            0xFF, 0xFE, 0x00, 0xC0, 0xAF, 0x80, 0x81, 0xE0, 0x80, 0x80,
            0xF0, 0x80, 0x80, 0x80, 0xED, 0xA0, 0x80, 0x01, 0x02, 0x03,
        ];
        let expected_hash = format!("{:x}", Sha256::digest(&invalid_utf8_data));

        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/test.bin"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_bytes(invalid_utf8_data.clone()),
            )
            .mount(&mock_server)
            .await;

        let temp_dir = std::env::temp_dir().join(format!(
            "elscione_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&temp_dir).await.unwrap();
        let dest_path = temp_dir.join("test.bin");

        let client = reqwest::Client::new();
        let checksum = download_file(
            &client,
            DownloadRequest {
                cookie: None,
                url: &format!("{}/test.bin", mock_server.uri()),
                dest_path: &dest_path,
                last_modified: None,
            },
            DownloadProgress::default(),
        )
        .await
        .unwrap();

        assert_eq!(checksum, expected_hash);
        let downloaded_bytes = fs::read(&dest_path).await.unwrap();
        assert_eq!(downloaded_bytes, invalid_utf8_data);

        let _ = fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn test_download_404_error() {
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/missing.bin"))
            .respond_with(wiremock::ResponseTemplate::new(404))
            .mount(&mock_server)
            .await;

        let temp_dir = std::env::temp_dir().join("elscione_test_404");
        let dest_path = temp_dir.join("missing.bin");

        let client = reqwest::Client::new();
        let res = download_file(
            &client,
            DownloadRequest {
                cookie: None,
                url: &format!("{}/missing.bin", mock_server.uri()),
                dest_path: &dest_path,
                last_modified: None,
            },
            DownloadProgress::default(),
        )
        .await;

        match res {
            Err(DownloadError::HttpStatus { status, .. }) => {
                assert_eq!(status, reqwest::StatusCode::NOT_FOUND);
            }
            other => panic!("Expected HttpStatus 404 error, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_download_429_retry_after() {
        let mock_server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path("/rate_limited.bin"))
            .respond_with(
                wiremock::ResponseTemplate::new(429)
                    .insert_header("Retry-After", "45"),
            )
            .mount(&mock_server)
            .await;

        let temp_dir = std::env::temp_dir().join("elscione_test_429");
        let dest_path = temp_dir.join("rate_limited.bin");

        let client = reqwest::Client::new();
        let res = download_file(
            &client,
            DownloadRequest {
                cookie: None,
                url: &format!("{}/rate_limited.bin", mock_server.uri()),
                dest_path: &dest_path,
                last_modified: None,
            },
            DownloadProgress::default(),
        )
        .await;

        match res {
            Err(DownloadError::HttpStatus {
                status,
                retry_after,
                ..
            }) => {
                assert_eq!(status, reqwest::StatusCode::TOO_MANY_REQUESTS);
                assert_eq!(retry_after, Some(Duration::from_secs(45)));
            }
            other => panic!("Expected HttpStatus 429 error with Retry-After, got {:?}", other),
        }
    }
}

use anyhow::Result;
use filetime::FileTime;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};

/// Download `url` to `dest_path`, writing via a `.part` temp file and atomically
/// renaming on completion.
///
/// Returns the hex-encoded SHA-256 checksum of the downloaded bytes.
pub async fn download_file(
    client: &mut ghostwire::Ghostwire,
    cookie: Option<&str>,
    url: &str,
    dest_path: &Path,
    last_modified: Option<&str>,
    pb: Option<&ProgressBar>,
    overall: Option<&ProgressBar>,
    remaining_bytes: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
) -> Result<String> {
    // Ensure parent directory exists.
    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let part_path: PathBuf = {
        let mut p = dest_path.to_path_buf();
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        p.set_file_name(format!("{name}.part"));
        p
    };

    let mut resumed_bytes = 0;
    if part_path.exists() {
        if let Ok(meta) = fs::metadata(&part_path).await {
            let len = meta.len();
            if len > 0 {
                resumed_bytes = len;
            }
        }
    }

    let mut opts = ghostwire::RequestOptions::default();
    let mut headers = reqwest::header::HeaderMap::new();
    if resumed_bytes > 0 {
        headers.insert(
            "Range",
            reqwest::header::HeaderValue::from_str(&format!("bytes={}-", resumed_bytes))?,
        );
    }
    if let Some(cookie_str) = cookie {
        headers.insert(
            reqwest::header::COOKIE,
            reqwest::header::HeaderValue::from_str(cookie_str)?,
        );
    }
    if !headers.is_empty() {
        opts.headers = Some(headers);
    }

    let mut response = client
        .request(reqwest::Method::GET, url, opts)
        .await
        .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;

    // Handle 416 Range Not Satisfiable by truncating and retrying from 0
    if response.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        resumed_bytes = 0;
        if part_path.exists() {
            let _ = fs::remove_file(&part_path).await;
        }
        let mut opts = ghostwire::RequestOptions::default();
        if let Some(cookie_str) = cookie {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::COOKIE,
                reqwest::header::HeaderValue::from_str(cookie_str)?,
            );
            opts.headers = Some(headers);
        }
        response = client
            .request(reqwest::Method::GET, url, opts)
            .await
            .map_err(|e| anyhow::anyhow!("GET {url} (retry after 416): {e}"))?;
    }

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "HTTP {} for {url}",
            response.status().as_u16()
        ));
    }

    let mut hasher = Sha256::new();
    let mut file = if response.status() == reqwest::StatusCode::PARTIAL_CONTENT && resumed_bytes > 0 {
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

        if let Some(p) = pb {
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

    use futures::StreamExt;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| anyhow::anyhow!("stream error: {e}"))?;
        let len = chunk.len() as u64;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        
        if let Some(p) = pb {
            p.inc(len);
        }
        if let (Some(o), Some(rem)) = (overall, &remaining_bytes) {
            let current_rem = rem.fetch_sub(len, std::sync::atomic::Ordering::Relaxed).saturating_sub(len);
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

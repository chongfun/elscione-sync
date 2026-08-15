use anyhow::{Context, Result};
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::debug;

/// Long-lived session manager for all HTTP interactions with server.elscione.com.
///
/// Ensures a shared cookie jar and client identity across directory crawling and file downloads,
/// while keeping challenge-aware handling separate from binary streaming.
#[derive(Clone)]
pub struct ElscioneSession {
    #[allow(dead_code)]
    base_url: reqwest::Url,
    cookie_jar: Arc<reqwest::cookie::Jar>,
    http_client: reqwest::Client,
    ghostwire_client: Arc<Mutex<ghostwire::Ghostwire>>,
    cookie_header: Option<String>,
}

impl ElscioneSession {
    /// Create a new long-lived session.
    pub fn new(base_url_str: &str, user_agent: &str, cookie_opt: Option<&str>) -> Result<Self> {
        let base_url = reqwest::Url::parse(base_url_str)
            .with_context(|| format!("Invalid base URL: {base_url_str}"))?;

        let cookie_jar = Arc::new(reqwest::cookie::Jar::default());

        if let Some(cookie_str) = cookie_opt {
            for part in cookie_str.split(';') {
                let trimmed = part.trim();
                if !trimmed.is_empty() {
                    cookie_jar.add_cookie_str(trimmed, &base_url);
                }
            }
        }

        let mut default_headers = reqwest::header::HeaderMap::new();
        default_headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static(
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
            ),
        );
        default_headers.insert(
            reqwest::header::ACCEPT_LANGUAGE,
            reqwest::header::HeaderValue::from_static("en-US,en;q=0.5"),
        );

        let http_client = reqwest::Client::builder()
            .default_headers(default_headers)
            .user_agent(user_agent)
            .cookie_provider(cookie_jar.clone())
            .gzip(true)
            .brotli(true)
            .deflate(true)
            .build()
            .context("Building reqwest client")?;

        let user_agent_opts = ghostwire::UserAgentOptions {
            custom: Some(user_agent.to_string()),
            ..Default::default()
        };

        let gw = ghostwire::Ghostwire::builder()
            .user_agent_opts(user_agent_opts)
            .min_request_interval_secs(0.0)
            .auto_refresh_on_403(false) // Handle 403 explicitly in application logic
            .stealth(ghostwire::StealthConfig {
                enabled: true,
                human_like_delays: false,
                randomize_headers: false,
                browser_quirks: true,
                min_delay_secs: 0.0,
                max_delay_secs: 0.0,
            })
            .build()
            .map_err(|e| anyhow::anyhow!("Building ghostwire client: {e}"))?;

        Ok(Self {
            base_url,
            cookie_jar,
            http_client,
            ghostwire_client: Arc::new(Mutex::new(gw)),
            cookie_header: cookie_opt.map(|s| s.to_string()),
        })
    }

    /// Access the base URL.
    #[allow(dead_code)]
    pub fn base_url(&self) -> &reqwest::Url {
        &self.base_url
    }

    /// Access the underlying `reqwest::Client` configured with the shared cookie jar.
    ///
    /// Use this for binary-transparent, streaming file transfers.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    /// Access the shared cookie jar.
    #[allow(dead_code)]
    pub fn cookie_jar(&self) -> &Arc<reqwest::cookie::Jar> {
        &self.cookie_jar
    }

    /// Ingest `Set-Cookie` headers into the shared cookie jar.
    pub fn sync_cookies_from_headers(&self, headers: &reqwest::header::HeaderMap, url: &reqwest::Url) {
        for cookie_val in headers.get_all(reqwest::header::SET_COOKIE) {
            if let Ok(cookie_str) = cookie_val.to_str() {
                debug!("Syncing cookie into session: {}", cookie_str);
                self.cookie_jar.add_cookie_str(cookie_str, url);
            }
        }
    }

    /// Perform a GET request for an HTML page, extracting any session cookies set by the server.
    pub async fn get_html(&self, url: &str) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String)> {
        let mut opts = ghostwire::RequestOptions::default();
        if let Some(cookie_str) = &self.cookie_header {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(cookie_str) {
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(reqwest::header::COOKIE, val);
                opts.headers = Some(headers);
            }
        }

        let mut gw = self.ghostwire_client.lock().await;
        let resp = gw
            .request(reqwest::Method::GET, url, opts)
            .await
            .map_err(|e| anyhow::anyhow!("GET {url}: {e}"))?;

        let status = resp.status();
        let headers = resp.headers().clone();

        if let Ok(parsed_url) = reqwest::Url::parse(url) {
            self.sync_cookies_from_headers(&headers, &parsed_url);
        }

        let text = resp
            .text()
            .await
            .with_context(|| format!("Reading body from GET {url}"))?;

        Ok((status, headers, text))
    }

    /// Perform a JSON POST to an API endpoint (e.g. h5ai `/?`).
    pub async fn post_json(
        &self,
        url: &str,
        json_payload: &serde_json::Value,
        extra_headers: Option<reqwest::header::HeaderMap>,
    ) -> Result<(reqwest::StatusCode, reqwest::header::HeaderMap, String)> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json;charset=utf-8"),
        );

        if let Some(cookie_str) = &self.cookie_header {
            if let Ok(val) = reqwest::header::HeaderValue::from_str(cookie_str) {
                headers.insert(reqwest::header::COOKIE, val);
            }
        }

        if let Some(extra) = extra_headers {
            for (k, v) in extra {
                if let Some(k) = k {
                    headers.insert(k, v);
                }
            }
        }

        let mut opts = ghostwire::RequestOptions::default();
        opts.headers = Some(headers);
        opts.body_bytes = Some(bytes::Bytes::from(serde_json::to_vec(json_payload)?));

        let mut gw = self.ghostwire_client.lock().await;
        let resp = gw
            .request(reqwest::Method::POST, url, opts)
            .await
            .map_err(|e| anyhow::anyhow!("POST {url}: {e}"))?;

        let status = resp.status();
        let headers = resp.headers().clone();

        if let Ok(parsed_url) = reqwest::Url::parse(url) {
            self.sync_cookies_from_headers(&headers, &parsed_url);
        }

        let text = resp
            .text()
            .await
            .with_context(|| format!("Reading body from POST {url}"))?;

        Ok((status, headers, text))
    }
}

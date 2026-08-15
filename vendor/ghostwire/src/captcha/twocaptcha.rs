//! 2Captcha / RuCaptcha solver.

use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;

use super::{CaptchaConfig, CaptchaKind, CaptchaSolver};
use crate::error::{GhostwireError, Result};

const HOST: &str = "https://2captcha.com";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(180);

#[derive(Deserialize)]
struct ApiResponse {
    status: u8,
    request: Option<String>,
}

pub struct TwoCaptchaSolver {
    client: reqwest::Client,
}

impl TwoCaptchaSolver {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    fn captcha_method(kind: &CaptchaKind) -> &'static str {
        match kind {
            CaptchaKind::ReCaptcha => "userrecaptcha",
            CaptchaKind::HCaptcha => "hcaptcha",
            CaptchaKind::Turnstile => "turnstile",
        }
    }

    async fn submit_task(
        &self,
        kind: &CaptchaKind,
        page_url: &str,
        site_key: &str,
        api_key: &str,
        proxy: Option<&str>,
    ) -> Result<String> {
        let key_param = match kind {
            CaptchaKind::ReCaptcha => "googlekey",
            _ => "sitekey",
        };

        let mut params = vec![
            ("key", api_key.to_string()),
            ("pageurl", page_url.to_string()),
            ("json", "1".to_string()),
            ("soft_id", "2905".to_string()),
            ("method", Self::captcha_method(kind).to_string()),
            (key_param, site_key.to_string()),
        ];

        if let Some(p) = proxy {
            if let Ok(parsed) = url::Url::parse(p) {
                params.push(("proxy", parsed.to_string()));
                params.push(("proxytype", parsed.scheme().to_ascii_uppercase()));
            }
        }

        let resp: ApiResponse = self
            .client
            .post(format!("{HOST}/in.php"))
            .form(&params)
            .send()
            .await
            .map_err(GhostwireError::HttpError)?
            .json()
            .await
            .map_err(GhostwireError::HttpError)?;

        if resp.status == 1 {
            resp.request
                .ok_or_else(|| GhostwireError::CaptchaBadJobID("2captcha: no job id".into()))
        } else {
            Err(GhostwireError::CaptchaAPIError(
                resp.request.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    async fn poll_result(&self, job_id: &str, api_key: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + MAX_WAIT;

        loop {
            sleep(POLL_INTERVAL).await;

            if tokio::time::Instant::now() > deadline {
                return Err(GhostwireError::CaptchaTimeout(format!(
                    "2captcha: job {job_id} timed out"
                )));
            }

            let resp: ApiResponse = self
                .client
                .get(format!("{HOST}/res.php"))
                .query(&[
                    ("key", api_key),
                    ("action", "get"),
                    ("id", job_id),
                    ("json", "1"),
                ])
                .send()
                .await
                .map_err(GhostwireError::HttpError)?
                .json()
                .await
                .map_err(GhostwireError::HttpError)?;

            if resp.status == 1 {
                return resp.request.ok_or_else(|| {
                    GhostwireError::CaptchaAPIError("2captcha: empty result".into())
                });
            }

            // CAPCHA_NOT_READY – keep polling.
            if resp.request.as_deref() == Some("CAPCHA_NOT_READY") {
                continue;
            }

            return Err(GhostwireError::CaptchaAPIError(
                resp.request.unwrap_or_else(|| "unknown error".into()),
            ));
        }
    }
}

#[async_trait]
impl CaptchaSolver for TwoCaptchaSolver {
    async fn solve(
        &self,
        kind: CaptchaKind,
        page_url: &str,
        site_key: &str,
        config: &CaptchaConfig,
    ) -> Result<String> {
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            GhostwireError::CaptchaParameter("2captcha: missing api_key".into())
        })?;

        let proxy = if config.no_proxy {
            None
        } else {
            config.proxy.as_deref()
        };

        let job_id = self
            .submit_task(&kind, page_url, site_key, api_key, proxy)
            .await?;

        self.poll_result(&job_id, api_key).await
    }
}

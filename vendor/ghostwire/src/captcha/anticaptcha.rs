//! Anti-Captcha solver (https://anti-captcha.com).

use async_trait::async_trait;

use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;

use super::{CaptchaConfig, CaptchaKind, CaptchaSolver};
use crate::error::{GhostwireError, Result};

const HOST: &str = "https://api.anti-captcha.com";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(180);

fn task_type(kind: &CaptchaKind, proxy: bool) -> &'static str {
    let proxyless = !proxy;
    match kind {
        CaptchaKind::ReCaptcha => {
            if proxyless {
                "NoCaptchaTaskProxyless"
            } else {
                "NoCaptchaTask"
            }
        }
        CaptchaKind::HCaptcha => {
            if proxyless {
                "HCaptchaTaskProxyless"
            } else {
                "HCaptchaTask"
            }
        }
        CaptchaKind::Turnstile => {
            if proxyless {
                "TurnstileTaskProxyless"
            } else {
                "TurnstileTask"
            }
        }
    }
}

pub struct AntiCaptchaSolver {
    client: reqwest::Client,
}

impl AntiCaptchaSolver {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    async fn create_task(
        &self,
        kind: &CaptchaKind,
        page_url: &str,
        site_key: &str,
        client_key: &str,
        proxy: Option<&str>,
    ) -> Result<u64> {
        let has_proxy = proxy.is_some();
        let mut task = json!({
            "type": task_type(kind, has_proxy),
            "websiteURL": page_url,
            "websiteKey": site_key,
        });

        if let Some(p) = proxy {
            if let Ok(parsed) = url::Url::parse(p) {
                task["proxyType"] = json!(parsed.scheme());
                task["proxyAddress"] = json!(parsed.host_str().unwrap_or(""));
                task["proxyPort"] = json!(parsed.port().unwrap_or(8080));
                if let Some(u) = parsed
                    .username()
                    .is_empty()
                    .then(|| None)
                    .unwrap_or(Some(parsed.username()))
                {
                    task["proxyLogin"] = json!(u);
                }
                if let Some(pw) = parsed.password() {
                    task["proxyPassword"] = json!(pw);
                }
            }
        }

        let body = json!({
            "clientKey": client_key,
            "task": task,
            "softId": 959,
        });

        let resp: Value = self
            .client
            .post(format!("{HOST}/createTask"))
            .json(&body)
            .send()
            .await
            .map_err(GhostwireError::HttpError)?
            .json()
            .await
            .map_err(GhostwireError::HttpError)?;

        let error_id = resp["errorId"].as_u64().unwrap_or(0);
        if error_id != 0 {
            let msg = resp["errorDescription"]
                .as_str()
                .unwrap_or("unknown error")
                .to_string();
            return Err(GhostwireError::CaptchaAPIError(msg));
        }

        resp["taskId"]
            .as_u64()
            .ok_or_else(|| GhostwireError::CaptchaBadJobID("anticaptcha: no taskId".into()))
    }

    async fn poll_result(&self, task_id: u64, client_key: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + MAX_WAIT;

        loop {
            sleep(POLL_INTERVAL).await;

            if tokio::time::Instant::now() > deadline {
                return Err(GhostwireError::CaptchaTimeout(format!(
                    "anticaptcha: task {task_id} timed out"
                )));
            }

            let body = json!({
                "clientKey": client_key,
                "taskId": task_id,
            });

            let resp: Value = self
                .client
                .post(format!("{HOST}/getTaskResult"))
                .json(&body)
                .send()
                .await
                .map_err(GhostwireError::HttpError)?
                .json()
                .await
                .map_err(GhostwireError::HttpError)?;

            let error_id = resp["errorId"].as_u64().unwrap_or(0);
            if error_id != 0 {
                let msg = resp["errorDescription"]
                    .as_str()
                    .unwrap_or("unknown error")
                    .to_string();
                return Err(GhostwireError::CaptchaAPIError(msg));
            }

            if resp["status"].as_str() == Some("ready") {
                let solution = &resp["solution"];
                if let Some(token) = solution["token"].as_str() {
                    return Ok(token.to_string());
                }
                if let Some(token) = solution["gRecaptchaResponse"].as_str() {
                    return Ok(token.to_string());
                }
                return Err(GhostwireError::CaptchaAPIError(
                    "anticaptcha: no token in solution".into(),
                ));
            }
            // processing – keep polling
        }
    }
}

#[async_trait]
impl CaptchaSolver for AntiCaptchaSolver {
    async fn solve(
        &self,
        kind: CaptchaKind,
        page_url: &str,
        site_key: &str,
        config: &CaptchaConfig,
    ) -> Result<String> {
        let client_key = config
            .client_key
            .as_deref()
            .or(config.api_key.as_deref())
            .ok_or_else(|| {
                GhostwireError::CaptchaParameter("anticaptcha: missing clientKey".into())
            })?;

        let proxy = if config.no_proxy {
            None
        } else {
            config.proxy.as_deref()
        };

        let task_id = self
            .create_task(&kind, page_url, site_key, client_key, proxy)
            .await?;

        self.poll_result(task_id, client_key).await
    }
}

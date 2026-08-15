//! CapSolver solver (https://capsolver.com).

use async_trait::async_trait;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::sleep;

use super::{CaptchaConfig, CaptchaKind, CaptchaSolver};
use crate::error::{GhostwireError, Result};

const HOST: &str = "https://api.capsolver.com";
const APP_ID: &str = "9E717405-8C70-49B3-B277-7C2F2196484B";
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_WAIT: Duration = Duration::from_secs(180);

fn task_type(kind: &CaptchaKind, proxyless: bool) -> &'static str {
    match kind {
        CaptchaKind::ReCaptcha => {
            if proxyless {
                "ReCaptchaV2TaskProxyless"
            } else {
                "ReCaptchaV2Task"
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
                "AntiCloudflareTaskProxyless"
            } else {
                "AntiCloudflareTask"
            }
        }
    }
}

pub struct CapsolverSolver {
    client: reqwest::Client,
}

impl CapsolverSolver {
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
        api_key: &str,
        proxy: Option<&str>,
    ) -> Result<String> {
        let proxyless = proxy.is_none();
        let mut task = json!({
            "type": task_type(kind, proxyless),
            "websiteURL": page_url,
            "websiteKey": site_key,
        });

        if kind == &CaptchaKind::Turnstile {
            task["metadata"] = json!({"type": "turnstile"});
        }

        if let Some(p) = proxy {
            task["proxy"] = json!(p);
        }

        let body = json!({
            "clientKey": api_key,
            "appId": APP_ID,
            "task": task,
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

        if let Some(err) = resp["errorDescription"].as_str() {
            if !err.is_empty() && !err.contains("Current system busy") {
                return Err(GhostwireError::CaptchaAPIError(err.to_string()));
            }
        }

        resp["taskId"]
            .as_str()
            .map(|s| s.to_string())
            .or_else(|| resp["taskId"].as_u64().map(|n| n.to_string()))
            .ok_or_else(|| GhostwireError::CaptchaBadJobID("capsolver: no taskId".into()))
    }

    async fn poll_result(&self, task_id: &str, api_key: &str) -> Result<String> {
        let deadline = tokio::time::Instant::now() + MAX_WAIT;

        loop {
            sleep(POLL_INTERVAL).await;

            if tokio::time::Instant::now() > deadline {
                return Err(GhostwireError::CaptchaTimeout(format!(
                    "capsolver: task {task_id} timed out"
                )));
            }

            let body = json!({
                "clientKey": api_key,
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

            if let Some(err) = resp["errorDescription"].as_str() {
                if !err.is_empty() && !err.contains("Current system busy") {
                    return Err(GhostwireError::CaptchaAPIError(err.to_string()));
                }
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
                    "capsolver: no token in solution".into(),
                ));
            }
        }
    }
}

#[async_trait]
impl CaptchaSolver for CapsolverSolver {
    async fn solve(
        &self,
        kind: CaptchaKind,
        page_url: &str,
        site_key: &str,
        config: &CaptchaConfig,
    ) -> Result<String> {
        let api_key = config.api_key.as_deref().ok_or_else(|| {
            GhostwireError::CaptchaParameter("capsolver: missing api_key".into())
        })?;

        let proxy = if config.no_proxy {
            None
        } else {
            config.proxy.as_deref()
        };

        let task_id = self
            .create_task(&kind, page_url, site_key, api_key, proxy)
            .await?;

        self.poll_result(&task_id, api_key).await
    }
}

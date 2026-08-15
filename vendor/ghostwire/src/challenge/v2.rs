//! Cloudflare v2 challenge detection and solving.

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

use super::*;
use crate::error::{GhostwireError, Result};

static RE_CF_OPT: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?s)window\._cf_chl_opt\s*=\s*(\{.*?\});").unwrap());

static RE_FORM_ACTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)<form[^>]*?id="challenge-form"[^>]*?action="([^"]+)""#).unwrap()
});

static RE_R_TOKEN: Lazy<Regex> = Lazy::new(|| Regex::new(r#"name="r" value="([^"]+)""#).unwrap());

static RE_SITEKEY: Lazy<Regex> = Lazy::new(|| Regex::new(r#"data-sitekey="([^"]+)""#).unwrap());

/// Handles Cloudflare v2 challenges.
pub struct CloudflareV2;

impl CloudflareV2 {
    pub fn is_v2_js_challenge(status: u16, server: &str, body: &str) -> bool {
        matches!(status, 403 | 429 | 503) && is_cloudflare_server(server) && RE_V2_JS.is_match(body)
    }

    pub fn is_v2_captcha_challenge(status: u16, server: &str, body: &str) -> bool {
        status == 403 && is_cloudflare_server(server) && RE_NEW_CAPTCHA.is_match(body)
    }

    /// Extract the challenge metadata embedded in `window._cf_chl_opt`.
    pub fn extract_challenge_data(body: &str) -> Result<serde_json::Value> {
        let caps = RE_CF_OPT
            .captures(body)
            .ok_or_else(|| GhostwireError::ChallengeError("Cannot find _cf_chl_opt".into()))?;
        let json_str = caps.get(1).unwrap().as_str();
        serde_json::from_str(json_str).map_err(Into::into)
    }

    /// Extract the challenge form action URL.
    pub fn extract_form_action(body: &str) -> Result<String> {
        let caps = RE_FORM_ACTION.captures(body).ok_or_else(|| {
            GhostwireError::ChallengeError("Cannot find challenge form action".into())
        })?;
        Ok(caps.get(1).unwrap().as_str().to_string())
    }

    /// Build the POST payload for a v2 JS challenge.
    pub fn build_js_payload(
        body: &str,
        challenge_data: &serde_json::Value,
    ) -> Result<Vec<(String, String)>> {
        let r = RE_R_TOKEN
            .captures(body)
            .ok_or_else(|| GhostwireError::ChallengeError("Cannot find r token".into()))?
            .get(1)
            .unwrap()
            .as_str()
            .to_string();

        let mut payload = vec![
            ("r".to_string(), r),
            ("cf_ch_verify".to_string(), "plat".to_string()),
            ("vc".to_string(), String::new()),
            ("captcha_vc".to_string(), String::new()),
            ("cf_captcha_kind".to_string(), "h".to_string()),
            ("h-captcha-response".to_string(), String::new()),
        ];

        if let Some(cv_id) = challenge_data.get("cvId").and_then(|v| v.as_str()) {
            payload.push(("cv_chal_id".to_string(), cv_id.to_string()));
        }
        if let Some(page_data) = challenge_data.get("chlPageData").and_then(|v| v.as_str()) {
            payload.push(("cf_chl_page_data".to_string(), page_data.to_string()));
        }

        Ok(payload)
    }

    /// Build the POST payload for a v2 captcha challenge with a solved token.
    pub fn build_captcha_payload(
        body: &str,
        challenge_data: &serde_json::Value,
        captcha_token: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut payload = Self::build_js_payload(body, challenge_data)?;
        // Replace blank h-captcha-response with the solved token.
        for (k, v) in &mut payload {
            if k == "h-captcha-response" {
                *v = captcha_token.to_string();
            }
        }
        Ok(payload)
    }

    /// Extract the hCaptcha site key from the page.
    pub fn extract_site_key(body: &str) -> Result<String> {
        let caps = RE_SITEKEY.captures(body).ok_or_else(|| {
            GhostwireError::CaptchaError("Cannot find hCaptcha site key".into())
        })?;
        Ok(caps.get(1).unwrap().as_str().to_string())
    }

    /// Resolve a relative form action to an absolute URL.
    pub fn resolve_url(page_url: &str, action: &str) -> Result<String> {
        if action.starts_with("http") {
            return Ok(action.to_string());
        }
        let base = Url::parse(page_url)?;
        Ok(format!(
            "{}://{}{}",
            base.scheme(),
            base.host_str().unwrap_or(""),
            action
        ))
    }
}

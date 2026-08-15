//! Cloudflare Turnstile challenge detection and solving.

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

use super::*;
use crate::error::{GhostwireError, Result};

static RE_FORM_ACTION: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(?s)<form[^>]*action="([^"]+)""#).unwrap());

static RE_INPUTS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<input[^>]*name="([^"]+)"[^>]*value="([^"]*)""#).unwrap());

/// Handles Cloudflare Turnstile challenges.
pub struct CloudflareTurnstile;

impl CloudflareTurnstile {
    pub fn is_turnstile_challenge(status: u16, server: &str, body: &str) -> bool {
        matches!(status, 403 | 429 | 503)
            && is_cloudflare_server(server)
            && (RE_TURNSTILE_CLASS.is_match(body)
                || RE_TURNSTILE_SCRIPT.is_match(body)
                || RE_TURNSTILE_SITEKEY.is_match(body))
    }

    /// Extract the Turnstile site key from the page.
    pub fn extract_site_key(body: &str) -> Result<String> {
        RE_TURNSTILE_SITEKEY
            .captures(body)
            .map(|c| c.get(1).unwrap().as_str().to_string())
            .ok_or_else(|| {
                GhostwireError::TurnstileError("Cannot find Turnstile site key".into())
            })
    }

    /// Extract the form action URL (falls back to the page URL path).
    pub fn extract_form_action(body: &str, page_url: &str) -> Result<String> {
        if let Some(caps) = RE_FORM_ACTION.captures(body) {
            let action = caps.get(1).unwrap().as_str();
            return Self::resolve_url(page_url, action);
        }
        // Fall back: POST to the same path.
        let base = Url::parse(page_url)?;
        Ok(format!(
            "{}://{}{}",
            base.scheme(),
            base.host_str().unwrap_or(""),
            base.path()
        ))
    }

    /// Build the POST payload, injecting the solved Turnstile token.
    pub fn build_payload(body: &str, token: &str) -> Vec<(String, String)> {
        let mut payload: Vec<(String, String)> = RE_INPUTS
            .captures_iter(body)
            .map(|c| {
                (
                    c.get(1).unwrap().as_str().to_string(),
                    c.get(2).unwrap().as_str().to_string(),
                )
            })
            .filter(|(k, _)| k != "cf-turnstile-response")
            .collect();

        payload.push(("cf-turnstile-response".to_string(), token.to_string()));
        payload
    }

    /// Resolve a potentially relative action URL to absolute.
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


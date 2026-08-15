//! Cloudflare v1 (IUAM / legacy) challenge detection and solving.

use once_cell::sync::Lazy;
use regex::Regex;
use url::Url;

use super::*;
use crate::error::{GhostwireError, Result};

static RE_SUBMIT_DELAY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"submit\(\);\r?\n\s*},\s*([0-9]+)").unwrap());

/// Handles Cloudflare v1 (legacy IUAM / hCaptcha) challenges.
pub struct CloudflareV1;

impl CloudflareV1 {
    /// Returns `true` when the response is a v1 IUAM challenge page.
    pub fn is_iuam_challenge(status: u16, server: &str, body: &str) -> bool {
        matches!(status, 429 | 503)
            && is_cloudflare_server(server)
            && RE_IUAM.is_match(body)
            && RE_IUAM_FORM.is_match(body)
    }

    /// Returns `true` when the response is the *new-style* IUAM challenge.
    pub fn is_new_iuam_challenge(status: u16, server: &str, body: &str) -> bool {
        Self::is_iuam_challenge(status, server, body) && RE_NEW_IUAM.is_match(body)
    }

    /// Returns `true` when the response is a hCaptcha challenge.
    pub fn is_captcha_challenge(status: u16, server: &str, body: &str) -> bool {
        status == 403
            && is_cloudflare_server(server)
            && RE_CAPTCHA_TRACE.is_match(body)
            && RE_CAPTCHA_FORM.is_match(body)
    }

    /// Returns `true` when the response is the new-style captcha challenge.
    pub fn is_new_captcha_challenge(status: u16, server: &str, body: &str) -> bool {
        Self::is_captcha_challenge(status, server, body) && RE_NEW_CAPTCHA.is_match(body)
    }

    /// Returns `true` when Cloudflare has blocked the request with error 1020.
    pub fn is_firewall_blocked(status: u16, server: &str, body: &str) -> bool {
        status == 403 && is_cloudflare_server(server) && RE_FIREWALL_1020.is_match(body)
    }

    /// Classify the response, returning the challenge type or `None`.
    pub fn classify(status: u16, server: &str, body: &str) -> Option<V1ChallengeKind> {
        if Self::is_firewall_blocked(status, server, body) {
            return Some(V1ChallengeKind::Firewall1020);
        }
        if Self::is_new_captcha_challenge(status, server, body) {
            return Some(V1ChallengeKind::NewCaptcha);
        }
        if Self::is_new_iuam_challenge(status, server, body) {
            return Some(V1ChallengeKind::NewIUAM);
        }
        if Self::is_captcha_challenge(status, server, body) {
            return Some(V1ChallengeKind::Captcha);
        }
        if Self::is_iuam_challenge(status, server, body) {
            return Some(V1ChallengeKind::IUAM);
        }
        None
    }

    /// Extract the IUAM challenge submission data from the page body.
    ///
    /// Returns `(submit_url, form_data)` where `form_data` contains the
    /// `r`, `jschl_vc`, and `pass` fields plus the computed `jschl_answer`.
    pub fn extract_iuam_params(
        body: &str,
        page_url: &str,
        js_answer: f64,
    ) -> Result<(String, Vec<(String, String)>)> {
        static RE_FORM: Lazy<Regex> = Lazy::new(|| {
            Regex::new(
                r#"(?s)<form (?P<form>.*?="challenge-form" action="(?P<uuid>.*?__cf_chl_f_tk=\S+)".*?</form>)"#,
            )
            .unwrap()
        });

        let caps = RE_FORM.captures(body).ok_or_else(|| {
            GhostwireError::IUAMError("Cannot find IUAM challenge form".into())
        })?;

        let form_html = caps.name("form").unwrap().as_str();
        let uuid = caps.name("uuid").unwrap().as_str();

        // Parse input fields.
        static RE_INP: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?s)<input\s([^>]*?)/>"#).unwrap());
        static RE_ATTR: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(\S+)="([^"]*)""#).unwrap());

        let mut payload: Vec<(String, String)> = Vec::new();
        for input_cap in RE_INP.captures_iter(form_html) {
            let attrs_str = input_cap.get(1).unwrap().as_str();
            let attrs: std::collections::HashMap<&str, &str> = RE_ATTR
                .captures_iter(attrs_str)
                .filter_map(|c| {
                    let k = c.get(1)?.as_str();
                    let v = c.get(2)?.as_str();
                    Some((k, v))
                })
                .collect();

            if let Some(name) = attrs.get("name") {
                if ["r", "jschl_vc", "pass"].contains(name) {
                    let val = attrs.get("value").copied().unwrap_or("").to_string();
                    payload.push((name.to_string(), val));
                }
            }
        }

        payload.push(("jschl_answer".to_string(), format!("{js_answer:.10}")));

        let parsed = Url::parse(page_url)?;
        let submit_url = format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or(""),
            html_escape::decode_html_entities(uuid)
        );

        Ok((submit_url, payload))
    }

    /// Extract the delay (in seconds) requested by the IUAM challenge.
    pub fn extract_delay(body: &str) -> Option<f64> {
        let caps = RE_SUBMIT_DELAY.captures(body)?;
        caps.get(1)?
            .as_str()
            .parse::<f64>()
            .ok()
            .map(|ms| ms / 1000.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum V1ChallengeKind {
    IUAM,
    NewIUAM,
    Captcha,
    NewCaptcha,
    Firewall1020,
}

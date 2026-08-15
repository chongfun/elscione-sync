//! User-agent & browser header management.
//!
//! Reads browser fingerprint data embedded at compile-time from `browsers.json`.

use std::collections::HashMap;

use once_cell::sync::Lazy;
use rand::prelude::IndexedRandom;
use serde::Deserialize;

// Embed the JSON at compile time so the binary is self-contained.
const BROWSERS_JSON: &str = include_str!("../data/browsers.json");

// ── JSON schema ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize, Clone)]
pub struct BrowserHeaders {
    #[serde(rename = "User-Agent")]
    pub user_agent: Option<String>,
    #[serde(rename = "Accept")]
    pub accept: String,
    #[serde(rename = "Accept-Language")]
    pub accept_language: String,
    #[serde(rename = "Accept-Encoding")]
    pub accept_encoding: String,
}

#[derive(Debug, Deserialize)]
struct BrowsersJson {
    headers: HashMap<String, BrowserHeaders>,
    #[serde(rename = "cipherSuite")]
    cipher_suite: HashMap<String, Vec<String>>,
    user_agents: UserAgentsByDevice,
}

#[derive(Debug, Deserialize)]
struct UserAgentsByDevice {
    desktop: HashMap<String, HashMap<String, Vec<String>>>,
    mobile: HashMap<String, HashMap<String, Vec<String>>>,
}

// ── Parsed browser database ───────────────────────────────────────────────────

static BROWSERS: Lazy<BrowsersJson> =
    Lazy::new(|| serde_json::from_str(BROWSERS_JSON).expect("browsers.json is malformed"));

// ── Public API ────────────────────────────────────────────────────────────────

/// Supported browsers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Browser {
    Chrome,
    Firefox,
}

impl Browser {
    fn key(&self) -> &'static str {
        match self {
            Browser::Chrome => "chrome",
            Browser::Firefox => "firefox",
        }
    }
}

/// Target device class.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceType {
    Desktop,
    Mobile,
    Any,
}

/// Platform (OS) selection.
#[derive(Debug, Clone)]
pub enum Platform {
    Any,
    Named(String),
}

/// Options for user-agent selection.
#[derive(Debug, Clone)]
pub struct UserAgentOptions {
    /// Force a specific browser.
    pub browser: Option<Browser>,
    /// Force a specific platform.
    pub platform: Option<String>,
    /// Include desktop agents.
    pub desktop: bool,
    /// Include mobile agents.
    pub mobile: bool,
    /// Use a fully custom user-agent string.
    pub custom: Option<String>,
    /// Allow brotli encoding.
    pub allow_brotli: bool,
}

impl Default for UserAgentOptions {
    fn default() -> Self {
        UserAgentOptions {
            browser: None,
            platform: None,
            desktop: true,
            mobile: true,
            custom: None,
            allow_brotli: false,
        }
    }
}

/// A resolved user-agent configuration ready to be attached to requests.
#[derive(Debug, Clone)]
pub struct UserAgent {
    pub user_agent_string: String,
    pub headers: BrowserHeaders,
    pub cipher_suite: Vec<String>,
    pub browser: Browser,
}

impl UserAgent {
    /// Build a `UserAgent` from options.
    pub fn new(opts: &UserAgentOptions) -> crate::error::Result<Self> {
        let db = &*BROWSERS;

        // ── Custom UA ─────────────────────────────────────────────────────────
        if let Some(custom) = &opts.custom {
            // Try to match the custom UA to a known browser for headers/cipher.
            let matched = db
                .user_agents
                .desktop
                .iter()
                .chain(db.user_agents.mobile.iter())
                .find_map(|(_platform, browser_map)| {
                    browser_map.iter().find_map(|(browser_key, agents)| {
                        if agents.iter().any(|a| a.contains(custom.as_str())) {
                            Some(browser_key.clone())
                        } else {
                            None
                        }
                    })
                });

            let (browser, headers, cipher_suite) = if let Some(bk) = matched {
                let b = if bk == "firefox" {
                    Browser::Firefox
                } else {
                    Browser::Chrome
                };
                let h = db.headers[&bk].clone();
                let c = db.cipher_suite[&bk].clone();
                (b, h, c)
            } else {
                // Unknown UA – use Chrome defaults.
                let h = db.headers["chrome"].clone();
                let c = db.cipher_suite["chrome"].clone();
                (Browser::Chrome, h, c)
            };

            let mut headers = headers;
            headers.user_agent = Some(custom.clone());
            let headers = strip_brotli_if_needed(headers, opts.allow_brotli);

            return Ok(UserAgent {
                user_agent_string: custom.clone(),
                headers,
                cipher_suite,
                browser,
            });
        }

        // ── Random / filtered selection ───────────────────────────────────────
        let mut rng = rand::rng();

        // Collect candidate (platform, browser, agents) combos – owned data to
        // avoid lifetime issues with the `db` static reference.
        let mut candidates: Vec<(String, Browser, Vec<String>)> = Vec::new();

        let mut collect = |device_map: &HashMap<String, HashMap<String, Vec<String>>>| {
            for (platform, browser_map) in device_map {
                if let Some(pf) = &opts.platform {
                    if platform != pf {
                        continue;
                    }
                }
                for (bk, agents) in browser_map {
                    if agents.is_empty() {
                        continue;
                    }
                    let b = if bk == "firefox" {
                        Browser::Firefox
                    } else {
                        Browser::Chrome
                    };
                    if let Some(bf) = &opts.browser {
                        if &b != bf {
                            continue;
                        }
                    }
                    candidates.push((platform.clone(), b, agents.clone()));
                }
            }
        };

        if opts.desktop {
            collect(&db.user_agents.desktop);
        }
        if opts.mobile {
            collect(&db.user_agents.mobile);
        }

        if candidates.is_empty() {
            return Err(crate::error::GhostwireError::Other(
                "No matching user agents found for the given options".to_string(),
            ));
        }

        let (_, browser, agents): &(String, Browser, Vec<String>) =
            candidates.choose(&mut rng).unwrap();
        let ua_string = agents.choose(&mut rng).unwrap().clone();
        let bk = browser.key();
        let mut headers = db.headers[bk].clone();
        headers.user_agent = Some(ua_string.clone());
        let headers = strip_brotli_if_needed(headers, opts.allow_brotli);
        let cipher_suite = db.cipher_suite[bk].clone();

        Ok(UserAgent {
            user_agent_string: ua_string,
            headers,
            cipher_suite,
            browser: browser.clone(),
        })
    }

    /// Produce the default `reqwest` header map for this user-agent.
    pub fn header_map(&self) -> reqwest::header::HeaderMap {
        use reqwest::header::{ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, HeaderValue, USER_AGENT};
        let mut map = reqwest::header::HeaderMap::new();
        map.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent_string).unwrap(),
        );
        map.insert(ACCEPT, HeaderValue::from_str(&self.headers.accept).unwrap());
        map.insert(
            ACCEPT_LANGUAGE,
            HeaderValue::from_str(&self.headers.accept_language).unwrap(),
        );
        map.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_str(&self.headers.accept_encoding).unwrap(),
        );
        map
    }
}

fn strip_brotli_if_needed(mut headers: BrowserHeaders, allow_brotli: bool) -> BrowserHeaders {
    if !allow_brotli && headers.accept_encoding.contains("br") {
        headers.accept_encoding = headers
            .accept_encoding
            .split(',')
            .map(str::trim)
            .filter(|e| *e != "br")
            .collect::<Vec<_>>()
            .join(", ");
    }
    headers
}

//! Stealth techniques: human-like delays, header randomisation, browser quirks.

use std::time::{Duration, Instant};

use rand::prelude::RngExt;
use reqwest::header::{HeaderMap, HeaderValue};

/// Configuration for stealth mode.
#[derive(Debug, Clone)]
pub struct StealthConfig {
    pub enabled: bool,
    pub human_like_delays: bool,
    pub randomize_headers: bool,
    pub browser_quirks: bool,
    pub min_delay_secs: f64,
    pub max_delay_secs: f64,
}

impl Default for StealthConfig {
    fn default() -> Self {
        StealthConfig {
            enabled: true,
            human_like_delays: true,
            randomize_headers: true,
            browser_quirks: true,
            min_delay_secs: 0.5,
            max_delay_secs: 2.0,
        }
    }
}

/// Stealth state (tracks request history for delay calculations).
pub struct StealthState {
    pub config: StealthConfig,
    request_count: u64,
    last_request: Option<Instant>,
}

impl StealthState {
    pub fn new(config: StealthConfig) -> Self {
        StealthState {
            config,
            request_count: 0,
            last_request: None,
        }
    }

    /// Block until the human-like delay has elapsed, then record this request.
    pub async fn pre_request(&mut self) {
        if self.config.enabled && self.config.human_like_delays && self.request_count > 0 {
            let delay = {
                let mut rng = rand::rng();
                let mut d =
                    rng.random_range(self.config.min_delay_secs..=self.config.max_delay_secs);

                // 10% chance of a slightly longer pause.
                if rng.random_bool(0.1) {
                    d *= 1.5;
                }
                d.min(10.0)
            };

            if delay >= 0.1 {
                tokio::time::sleep(Duration::from_secs_f64(delay)).await;
            }
        }

        self.request_count += 1;
        self.last_request = Some(Instant::now());
    }

    /// Enrich a header map with random / browser-quirk headers.
    pub fn apply_to_headers(&self, headers: &mut HeaderMap, user_agent: &str) {
        if !self.config.enabled {
            return;
        }

        let mut rng = rand::rng();

        // ── Randomise Accept ──────────────────────────────────────────────────
        if self.config.randomize_headers && !headers.contains_key("Accept") {
            let options = [
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,*/*;q=0.8",
                "text/html,application/xhtml+xml,application/xml;q=0.9,image/webp,image/apng,*/*;q=0.8",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            ];
            let chosen = options[rng.random_range(0..options.len())];
            headers.insert(reqwest::header::ACCEPT, HeaderValue::from_static(chosen));
        }

        // ── Randomise Accept-Language ─────────────────────────────────────────
        if self.config.randomize_headers && !headers.contains_key("Accept-Language") {
            let options = [
                "en-US,en;q=0.9",
                "en-US,en;q=0.8",
                "en-GB,en;q=0.9,en-US;q=0.8",
                "en-CA,en;q=0.9,en-US;q=0.8",
            ];
            let chosen = options[rng.random_range(0..options.len())];
            headers.insert(
                reqwest::header::ACCEPT_LANGUAGE,
                HeaderValue::from_static(chosen),
            );
        }

        // ── DNT (50 % chance) ─────────────────────────────────────────────────
        if self.config.randomize_headers && rng.random_bool(0.5) {
            headers.insert("dnt", HeaderValue::from_static("1"));
        }

        // ── Browser quirks ────────────────────────────────────────────────────
        if self.config.browser_quirks {
            let is_firefox = user_agent.contains("Firefox/");

            if is_firefox {
                if !headers.contains_key("Upgrade-Insecure-Requests") {
                    headers.insert("upgrade-insecure-requests", HeaderValue::from_static("1"));
                }
            } else {
                // Chrome-style sec-ch-ua headers.
                if !headers.contains_key("sec-ch-ua") {
                    headers.insert(
                        "sec-ch-ua",
                        HeaderValue::from_static(
                            r#""Google Chrome";v="120", "Not;A=Brand";v="8", "Chromium";v="120""#,
                        ),
                    );
                    headers.insert("sec-ch-ua-mobile", HeaderValue::from_static("?0"));
                    headers.insert(
                        "sec-ch-ua-platform",
                        HeaderValue::from_static("\"Windows\""),
                    );
                }
                if !headers.contains_key("Sec-Fetch-Site") {
                    headers.insert("sec-fetch-site", HeaderValue::from_static("none"));
                    headers.insert("sec-fetch-mode", HeaderValue::from_static("navigate"));
                    headers.insert("sec-fetch-user", HeaderValue::from_static("?1"));
                    headers.insert("sec-fetch-dest", HeaderValue::from_static("document"));
                }
            }
        }
    }
}

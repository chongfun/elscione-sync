//! Proxy pool management with sequential, random and "smart" rotation strategies.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rand::prelude::IndexedRandom;

use crate::error::{GhostwireError, Result};

/// How to pick the next proxy from the pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationStrategy {
    Sequential,
    Random,
    Smart,
}

impl Default for RotationStrategy {
    fn default() -> Self {
        RotationStrategy::Sequential
    }
}

#[derive(Debug, Clone, Default)]
struct ProxyStats {
    successes: u64,
    failures: u64,
}

/// Manages a pool of proxy URLs and rotates through them.
#[derive(Debug, Clone)]
pub struct ProxyManager {
    proxies: Vec<String>,
    strategy: RotationStrategy,
    ban_duration: Duration,
    banned: HashMap<String, Instant>,
    stats: HashMap<String, ProxyStats>,
    sequential_index: usize,
}

impl ProxyManager {
    pub fn new(proxies: Vec<String>, strategy: RotationStrategy, ban_secs: u64) -> Self {
        Self {
            proxies,
            strategy,
            ban_duration: Duration::from_secs(ban_secs),
            banned: HashMap::new(),
            stats: HashMap::new(),
            sequential_index: 0,
        }
    }

    /// Returns `true` if any proxies have been configured.
    pub fn has_proxies(&self) -> bool {
        !self.proxies.is_empty()
    }

    fn available(&self) -> Vec<&String> {
        let now = Instant::now();
        self.proxies
            .iter()
            .filter(|p| {
                self.banned
                    .get(*p)
                    .map_or(true, |t| now.duration_since(*t) > self.ban_duration)
            })
            .collect()
    }

    /// Returns the next proxy URL formatted as `{"http": url, "https": url}`.
    pub fn next_proxy(&mut self) -> Option<String> {
        let available: Vec<String> = self.available().iter().map(|s| (*s).clone()).collect();

        if available.is_empty() {
            // Unban the least-recently-banned proxy.
            let oldest = self
                .banned
                .iter()
                .min_by_key(|(_, t)| *t)
                .map(|(p, _)| p.clone())?;
            self.banned.remove(&oldest);
            return Some(oldest);
        }

        let chosen = match &self.strategy {
            RotationStrategy::Random => {
                let mut rng = rand::rng();
                available.choose(&mut rng).cloned()?
            }
            RotationStrategy::Smart => available
                .iter()
                .max_by(|a, b| {
                    let score_a = self.success_rate(a);
                    let score_b = self.success_rate(b);
                    score_a.partial_cmp(&score_b).unwrap()
                })
                .cloned()?,
            RotationStrategy::Sequential => {
                let idx = self.sequential_index % available.len();
                self.sequential_index += 1;
                available[idx].clone()
            }
        };

        Some(chosen)
    }

    fn success_rate(&self, proxy: &str) -> f64 {
        let s = self.stats.get(proxy).cloned().unwrap_or_default();
        let total = s.successes + s.failures;
        if total == 0 {
            0.5 // neutral prior
        } else {
            s.successes as f64 / total as f64
        }
    }

    pub fn report_success(&mut self, proxy: &str) {
        self.stats.entry(proxy.to_string()).or_default().successes += 1;
        self.banned.remove(proxy);
    }

    pub fn report_failure(&mut self, proxy: &str) {
        self.stats.entry(proxy.to_string()).or_default().failures += 1;
        self.banned.insert(proxy.to_string(), Instant::now());
    }

    pub fn add_proxy(&mut self, proxy: String) {
        if !self.proxies.contains(&proxy) {
            self.proxies.push(proxy);
        }
    }

    pub fn remove_proxy(&mut self, proxy: &str) {
        self.proxies.retain(|p| p != proxy);
        self.banned.remove(proxy);
        self.stats.remove(proxy);
    }

    /// Format a raw proxy URL for use with `reqwest`.
    pub fn format_proxy(proxy_url: &str) -> Result<reqwest::Proxy> {
        let url = if proxy_url.contains("://") {
            proxy_url.to_string()
        } else {
            format!("http://{proxy_url}")
        };
        reqwest::Proxy::all(&url).map_err(|e| GhostwireError::ProxyError(e.to_string()))
    }

    pub fn get_stats(&self) -> serde_json::Value {
        use serde_json::json;
        let now = Instant::now();
        let available = self
            .proxies
            .iter()
            .filter(|p| {
                self.banned
                    .get(*p)
                    .map_or(true, |t| now.duration_since(*t) > self.ban_duration)
            })
            .count();
        json!({
            "total_proxies": self.proxies.len(),
            "available_proxies": available,
            "banned_proxies": self.banned.len(),
        })
    }
}

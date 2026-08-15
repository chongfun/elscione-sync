//! Captcha solver trait and built-in provider implementations.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

// ── Captcha type ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CaptchaKind {
    ReCaptcha,
    HCaptcha,
    Turnstile,
}

// ── Provider config ───────────────────────────────────────────────────────────

/// Configuration passed to a captcha solver.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CaptchaConfig {
    /// Which provider to use (e.g. "2captcha", "anticaptcha", "capsolver").
    pub provider: String,
    /// Provider API key (field name varies by provider but stored here uniformly).
    pub api_key: Option<String>,
    /// clientKey for anticaptcha-style APIs.
    pub client_key: Option<String>,
    /// Optional proxy to pass to the captcha service.
    pub proxy: Option<String>,
    /// If true, don't forward the proxy to the captcha service.
    pub no_proxy: bool,
}

// ── Solver trait ──────────────────────────────────────────────────────────────

/// Any struct implementing this trait can solve captcha challenges.
#[async_trait]
pub trait CaptchaSolver: Send + Sync {
    /// Submit the captcha and return the solved token.
    async fn solve(
        &self,
        kind: CaptchaKind,
        page_url: &str,
        site_key: &str,
        config: &CaptchaConfig,
    ) -> Result<String>;
}

// ── Built-in providers ────────────────────────────────────────────────────────

pub mod anticaptcha;
pub mod capsolver;
pub mod twocaptcha;

pub use anticaptcha::AntiCaptchaSolver;
pub use capsolver::CapsolverSolver;
pub use twocaptcha::TwoCaptchaSolver;

/// Create a solver from a `CaptchaConfig`.
pub fn make_solver(config: &CaptchaConfig) -> Option<Box<dyn CaptchaSolver>> {
    match config.provider.to_lowercase().as_str() {
        "2captcha" => Some(Box::new(TwoCaptchaSolver::new())),
        "anticaptcha" => Some(Box::new(AntiCaptchaSolver::new())),
        "capsolver" => Some(Box::new(CapsolverSolver::new())),
        _ => None,
    }
}

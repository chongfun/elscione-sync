//! Error types for ghostwire.

use thiserror::Error;

/// Top-level error type.
#[derive(Debug, Error)]
pub enum GhostwireError {
    // ── Cloudflare challenge errors ───────────────────────────────────────────
    #[error("Cloudflare loop protection: tried to solve {0} time(s) in a row")]
    LoopProtection(usize),

    #[error("Cloudflare IUAM error: {0}")]
    IUAMError(String),

    #[error("Cloudflare challenge error: {0}")]
    ChallengeError(String),

    #[error("Cloudflare solve error: {0}")]
    SolveError(String),

    #[error("Cloudflare code 1020 – request blocked by firewall")]
    FirewallBlocked,

    #[error("Cloudflare captcha error: {0}")]
    CaptchaError(String),

    #[error("No captcha provider configured: {0}")]
    CaptchaProviderMissing(String),

    #[error("Cloudflare Turnstile error: {0}")]
    TurnstileError(String),

    #[error("Cloudflare v3 error: {0}")]
    V3Error(String),

    // ── Captcha solver errors ─────────────────────────────────────────────────
    #[error("Captcha service unavailable: {0}")]
    CaptchaServiceUnavailable(String),

    #[error("Captcha API error: {0}")]
    CaptchaAPIError(String),

    #[error("Captcha account error: {0}")]
    CaptchaAccountError(String),

    #[error("Captcha timeout: {0}")]
    CaptchaTimeout(String),

    #[error("Captcha parameter error: {0}")]
    CaptchaParameter(String),

    #[error("Bad captcha job ID: {0}")]
    CaptchaBadJobID(String),

    #[error("Captcha report error: {0}")]
    CaptchaReportError(String),

    // ── Network / HTTP errors ─────────────────────────────────────────────────
    #[error("HTTP error: {0}")]
    HttpError(#[from] reqwest::Error),

    #[error("URL parse error: {0}")]
    UrlParseError(#[from] url::ParseError),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("Regex error: {0}")]
    RegexError(#[from] regex::Error),

    // ── Proxy errors ──────────────────────────────────────────────────────────
    #[error("Proxy error: {0}")]
    ProxyError(String),

    // ── Generic ───────────────────────────────────────────────────────────────
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, GhostwireError>;

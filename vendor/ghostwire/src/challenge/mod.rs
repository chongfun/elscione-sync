//! Cloudflare challenge detection and solving modules.

pub mod js_interp;
pub mod turnstile;
pub mod v1;
pub mod v2;
pub mod v3;

pub use js_interp::JsInterpreter;
pub use turnstile::CloudflareTurnstile;
pub use v1::CloudflareV1;
pub use v2::CloudflareV2;
pub use v3::CloudflareV3;

use once_cell::sync::Lazy;
use regex::Regex;

// ── Common regexes ────────────────────────────────────────────────────────────

pub(crate) static RE_CF_SERVER: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?i)^cloudflare").unwrap());

pub(crate) static RE_IUAM: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/cdn-cgi/images/trace/jsch/").unwrap());

pub(crate) static RE_IUAM_FORM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)<form [^>]*?="challenge-form" action="/\S+__cf_chl_f_tk="#).unwrap()
});

pub(crate) static RE_NEW_IUAM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)cpo\.src\s*=\s*['"]/cdn-cgi/challenge-platform/\S+orchestrate/jsch/v1"#)
        .unwrap()
});

pub(crate) static RE_CAPTCHA_TRACE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"/cdn-cgi/images/trace/(captcha|managed)/").unwrap());

pub(crate) static RE_CAPTCHA_FORM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)<form [^>]*?="challenge-form" action="/\S+__cf_chl_f_tk="#).unwrap()
});

pub(crate) static RE_NEW_CAPTCHA: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?s)cpo\.src\s*=\s*['"]/cdn-cgi/challenge-platform/\S+orchestrate/(captcha|managed)/v1"#,
    )
    .unwrap()
});

pub(crate) static RE_V2_JS: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)cpo\.src\s*=\s*['"]/cdn-cgi/challenge-platform/\S+orchestrate/jsch/v1"#)
        .unwrap()
});

pub(crate) static RE_FIREWALL_1020: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"<span class="cf-error-code">1020</span>"#).unwrap());

pub(crate) static RE_V3_PLATFORM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)cpo\.src\s*=\s*['"]/cdn-cgi/challenge-platform/\S+orchestrate/jsch/v3"#)
        .unwrap()
});

pub(crate) static RE_V3_CTX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"window\._cf_chl_ctx\s*=").unwrap());

pub(crate) static RE_V3_FORM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"(?s)<form[^>]*id="challenge-form"[^>]*action="[^"]*__cf_chl_rt_tk="#).unwrap()
});

pub(crate) static RE_TURNSTILE_CLASS: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"class="cf-turnstile""#).unwrap());

pub(crate) static RE_TURNSTILE_SCRIPT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"src="https://challenges\.cloudflare\.com/turnstile/v0/api\.js"#).unwrap()
});

pub(crate) static RE_TURNSTILE_SITEKEY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"data-sitekey="([0-9A-Za-z_\-]{20,})""#).unwrap());

// ── Helper ────────────────────────────────────────────────────────────────────

/// Detect whether a response comes from Cloudflare.
pub fn is_cloudflare_server(server_header: &str) -> bool {
    RE_CF_SERVER.is_match(server_header)
}

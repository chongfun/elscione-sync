# ghostwire

An async Rust library for making HTTP requests to websites protected by Cloudflare's anti-bot systems. Automatically detects and handles Cloudflare challenges, with support for proxy rotation, stealth mode, and third-party captcha solvers.

> **Disclaimer:** This project is intended for legitimate use cases such as accessing your own services, automated testing, and research. Always respect a website's Terms of Service and `robots.txt`. The authors take no responsibility for misuse.

---

## Credits

### Cloudflare

This library exists because the original Python `cloudscraper` was needed in a Rust project and no equivalent existed. Cloudflare's challenge infrastructure — IUAM, managed challenges, Turnstile, and the v2/v3 JavaScript orchestration platform — is what the underlying logic was built to handle, and that carries over here unchanged.

### cloudscraper (Python)

The original logic, challenge classification, interpreter approach, and overall architecture of this library are a direct port of [cloudscraper](https://github.com/VeNoMouS/cloudscraper) by **VeNoMouS**, which is itself a fork/evolution of [cfscrape](https://github.com/Anorov/cfscrape) by **Anorov**.

Without their years of reverse engineering, documenting, and maintaining compatibility with Cloudflare's ever-changing challenges, this Rust port would not exist. All credit for the underlying approach belongs to them.

### AI

This entire Rust codebase — every module, every regex, every async challenge handler, every test — was written by **Claude Sonnet 4.6** (Anthropic) in a single session, guided by a human who provided the Python source and said "rewrite this in Rust properly."

No Rust was written by hand. The human's role was direction, not implementation.

---

## Features

- **Cloudflare v1** — Legacy IUAM JavaScript challenge and hCaptcha bypass
- **Cloudflare v2** — Modern JS orchestration (`jsch/v1`) and managed captcha challenge bypass
- **Cloudflare v3** — JavaScript VM challenge (`jsch/v3`) with fallback answer generation
- **Turnstile** — Cloudflare Turnstile CAPTCHA bypass via third-party solver integration
- **Stealth mode** — Human-like request delays, randomised `Accept`/`Accept-Language` headers, Chrome and Firefox browser quirks (`sec-ch-ua`, `Sec-Fetch-*`, `Upgrade-Insecure-Requests`)
- **Proxy rotation** — Sequential, random, and smart (success-rate-weighted) strategies with automatic ban/unban
- **Captcha providers** — Built-in async support for [2captcha](https://2captcha.com), [AntiCaptcha](https://anti-captcha.com), and [CapSolver](https://capsolver.com)
- **Realistic TLS** — `rustls` with browser-matching cipher suites loaded from `browsers.json`, embedded at compile time
- **Cookie persistence** — Session cookies automatically maintained across redirects and challenge submissions
- **Loop protection** — Configurable solve-depth limit prevents infinite challenge retry loops
- **CLI** — `ghostwire <url>` binary for quick command-line use

---

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
ghostwire = { path = "." }
tokio = { version = "1", features = ["full"] }
```

---

## Quick Start

```rust
use ghostwire::Ghostwire;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut ghostwire = Ghostwire::new()?;
    let resp = ghostwire.get("https://example.com").await?;
    println!("Status: {}", resp.status());
    println!("{}", resp.text().await?);
    Ok(())
}
```

---

## Usage

### With a captcha solver (Turnstile / hCaptcha)

Required when Cloudflare serves a challenge that cannot be solved algorithmically.

```rust
use ghostwire::{Ghostwire, captcha::CaptchaConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let captcha = CaptchaConfig {
        provider: "2captcha".into(),
        api_key: Some(std::env::var("TWOCAPTCHA_API_KEY")?),
        ..Default::default()
    };

    let mut ghostwire = Ghostwire::builder()
        .captcha(captcha)
        .debug(true)
        .build()?;

    let resp = ghostwire.get("https://protected.example.com").await?;
    println!("{}", resp.text().await?);
    Ok(())
}
```

### With proxy rotation

```rust
use ghostwire::{Ghostwire, proxy_manager::RotationStrategy};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut ghostwire = Ghostwire::builder()
        .proxies(vec![
            "http://proxy1.example.com:8080".into(),
            "http://proxy2.example.com:8080".into(),
            "socks5://proxy3.example.com:1080".into(),
        ])
        .proxy_rotation(RotationStrategy::Smart)
        .proxy_ban_secs(120)
        .build()?;

    let resp = ghostwire.get("https://example.com").await?;
    println!("{}", resp.status());
    Ok(())
}
```

### Stealth mode configuration

Stealth mode is enabled by default. Delays and header randomisation can be tuned:

```rust
use ghostwire::{Ghostwire, StealthConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let stealth = StealthConfig {
        enabled: true,
        human_like_delays: true,
        randomize_headers: true,
        browser_quirks: true,
        min_delay_secs: 1.0,
        max_delay_secs: 4.0,
    };

    let mut ghostwire = Ghostwire::builder()
        .stealth(stealth)
        .build()?;

    let resp = ghostwire.get("https://example.com").await?;
    println!("{}", resp.status());
    Ok(())
}
```

### Custom browser / user-agent

```rust
use ghostwire::{Ghostwire, user_agent::{Browser, UserAgentOptions}};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ua_opts = UserAgentOptions {
        browser: Some(Browser::Firefox),
        platform: Some("linux".into()),
        desktop: true,
        mobile: false,
        allow_brotli: false,
        ..Default::default()
    };

    let mut ghostwire = Ghostwire::builder()
        .user_agent_opts(ua_opts)
        .build()?;

    let resp = ghostwire.get("https://example.com").await?;
    println!("{}", resp.status());
    Ok(())
}
```

### Selectively disabling challenge handlers

```rust
let mut ghostwire = Ghostwire::builder()
    .disable_v1(true)        // skip legacy IUAM
    .disable_v2(true)        // skip v2 JS / managed challenges
    .disable_v3(true)        // skip v3 JavaScript VM challenges
    .disable_turnstile(true) // skip Turnstile challenges
    .build()?;
```

### POST requests

```rust
// URL-encoded form
ghostwire.post_form("https://example.com/login", vec![
    ("user".into(), "alice".into()),
    ("pass".into(), "hunter2".into()),
]).await?;

// Raw bytes
use bytes::Bytes;
let body = Bytes::from(r#"{"key":"value"}"#);
ghostwire.post_bytes("https://api.example.com/data", body).await?;
```

---

## CLI

```
USAGE:
    ghostwire [OPTIONS] <URL>

ARGS:
    <URL>    URL to fetch

OPTIONS:
    -X, --method <METHOD>                  HTTP method [default: GET]
    -p, --proxy <PROXY>                    Proxy URL (e.g. http://user:pass@host:port)
        --captcha-provider <PROVIDER>      Captcha provider: 2captcha | anticaptcha | capsolver
        --api-key <KEY>                    API key for the captcha provider
    -d, --debug                            Enable debug logging
        --no-stealth                       Disable stealth mode
    -h, --help                             Print help
    -V, --version                          Print version
```

Examples:

```bash
# Basic fetch
ghostwire https://example.com

# With debug output
ghostwire --debug https://example.com

# With captcha solver
ghostwire https://protected.example.com \
    --captcha-provider 2captcha \
    --api-key YOUR_API_KEY

# Through a proxy
ghostwire --proxy http://user:pass@host:8080 https://example.com

# POST request
ghostwire -X POST https://httpbin.org/post
```

---

## Captcha Providers

| Provider    | `provider` value | Required key field |
|-------------|------------------|--------------------|
| 2captcha    | `"2captcha"`     | `api_key`          |
| AntiCaptcha | `"anticaptcha"`  | `client_key`       |
| CapSolver   | `"capsolver"`    | `api_key`          |

All three providers support reCAPTCHA v2, hCaptcha, and Cloudflare Turnstile. Proxy forwarding to the solver service is supported via the `proxy` field on `CaptchaConfig`.

---

## Challenge Support Matrix

| Challenge type        | Solved automatically | Requires captcha provider |
|-----------------------|:--------------------:|:-------------------------:|
| CF v1 IUAM (legacy)   | ✅                   | ❌                        |
| CF v1 hCaptcha        | ✅                   | ✅                        |
| CF v2 JS orchestrated | ✅                   | ❌                        |
| CF v2 managed captcha | ✅                   | ✅                        |
| CF v3 JavaScript VM   | ✅         | ❌                        |
| Cloudflare Turnstile  | ✅                   | ✅                        |
| Firewall rule 1020    | ❌ (error returned)  | ❌                        |

---

## Architecture

```
src/
├── lib.rs              # Crate root and public re-exports
├── main.rs             # CLI binary (clap)
├── error.rs            # GhostwireError enum (thiserror)
├── client.rs           # Ghostwire + GhostwireBuilder
├── user_agent.rs       # Browser fingerprint selection (browsers.json)
├── proxy_manager.rs    # Proxy pool and rotation strategies
├── stealth.rs          # Delays, header randomisation, browser quirks
├── challenge/
│   ├── mod.rs          # Shared lazy regex statics
│   ├── v1.rs           # CF v1 IUAM / hCaptcha detection & extraction
│   ├── v2.rs           # CF v2 JS / captcha detection & payload building
│   ├── v3.rs           # CF v3 JS-VM detection & fallback answer generation
│   └── turnstile.rs    # Turnstile detection, site key extraction, payload
└── captcha/
    ├── mod.rs          # CaptchaSolver trait + make_solver() factory
    ├── twocaptcha.rs   # 2captcha / RuCaptcha async solver
    ├── anticaptcha.rs  # Anti-Captcha async solver
    └── capsolver.rs    # CapSolver async solver
data/
└── browsers.json       # Browser fingerprint DB — embedded at compile time
tests/
└── integration_tests.rs
```

---

## Running the Tests

```bash
cargo test
```

All 25 integration tests run locally without network access using in-process fixture HTML.

---

## License

MIT. See [LICENSE](LICENSE).

---

*ghostwire was written entirely by an AI. The human asked, the machine delivered.*

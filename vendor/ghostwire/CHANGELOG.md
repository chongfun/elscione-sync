# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

---

## [Unreleased]

## [1.1.0] - 2026-03-21

### Added

- JavaScript challenge solving using `v8`, `node (command)`, `bun (command)`, `boa_engine` or fallback answer generation.

## [1.0.1] - 2026-03-19

### Fixed

- README code examples updated to use `Ghostwire`, `GhostwireBuilder`, and `mut ghostwire` throughout — stale `CloudScraper` / `scraper` references left over from the rename have been removed.
- README architecture section updated to reflect `GhostwireError` and `Ghostwire` + `GhostwireBuilder` type names.
- README installation and CLI sections updated to reference the `ghostwire` crate name.

## [1.0.0] - 2026-03-19

### Added

- Initial release as **Ghostwire** (formerly `flaregun`, originally a Rust port of the Python `cloudscraper` library).
- `Ghostwire` async HTTP client wrapping `reqwest` with automatic Cloudflare challenge handling.
- `GhostwireBuilder` fluent builder for configuring the client.
- **Cloudflare v1** — Legacy IUAM JavaScript challenge detection and solving.
- **Cloudflare v1 hCaptcha** — Legacy captcha challenge detection with third-party solver integration.
- **Cloudflare v2 JS** — Modern JS orchestration (`jsch/v1`) challenge bypass.
- **Cloudflare v2 managed captcha** — hCaptcha bypass via third-party solver.
- **Cloudflare v3** — JavaScript VM challenge (`jsch/v3`) with deterministic fallback answer generation.
- **Cloudflare Turnstile** — Turnstile CAPTCHA bypass via third-party solver integration.
- **Stealth mode** — Human-like request delays, randomised `Accept` / `Accept-Language` headers, Chrome and Firefox browser quirks (`sec-ch-ua`, `Sec-Fetch-*`, `Upgrade-Insecure-Requests`).
- **Proxy rotation** — Sequential, random, and smart (success-rate-weighted) strategies with automatic ban/unban and configurable ban duration.
- **Captcha providers** — Built-in async support for [2captcha](https://2captcha.com), [AntiCaptcha](https://anti-captcha.com), and [CapSolver](https://capsolver.com).
- **Realistic TLS** — `rustls` with browser-matching cipher suites loaded from an embedded `browsers.json` fingerprint database.
- **Cookie persistence** — Session cookies automatically maintained across redirects and challenge submissions via `reqwest_cookie_store`.
- **Loop protection** — Configurable `solve_depth` limit prevents infinite challenge retry loops.
- **Rate limiting** — Configurable minimum interval between consecutive requests (`min_request_interval_secs`).
- **403 auto-retry** — Optional automatic retry on 403 responses with configurable max attempts.
- **`RequestOptions`** — Per-request control over headers, form body, raw bytes, timeout, and redirect policy.
- `GhostwireError` error enum covering HTTP, URL parsing, JSON, regex, proxy, captcha, and challenge failure cases.
- 25 integration tests covering UA selection, proxy management, challenge detection, stealth headers, and client construction — all running locally without network access.
- `scrape` example binary for CLI usage.

### Fixed

- `Ghostwire::request` future made `Send` by scoping `rand::rng()` (`ThreadRng`) inside a block so it is dropped before any `.await` point in `StealthState::pre_request`.

[1.0.1]: https://github.com/nacht-org/ghostwire/releases/tag/v1.0.1
[1.0.0]: https://github.com/nacht-org/ghostwire/releases/tag/v1.0.0

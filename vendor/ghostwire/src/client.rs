//! Core `Ghostwire` client – wraps `reqwest` with Cloudflare bypass logic.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderValue, ORIGIN, REFERER};
use reqwest::{Method, Response};
use tokio::sync::Mutex;
use tracing::{debug, instrument, warn};
use url::Url;

use crate::captcha::{CaptchaConfig, CaptchaKind, make_solver};
use crate::challenge::js_interp::JsInterpreter;
use crate::challenge::turnstile::CloudflareTurnstile;
use crate::challenge::v1::{CloudflareV1, V1ChallengeKind};
use crate::challenge::v2::CloudflareV2;
use crate::challenge::v3::CloudflareV3;
use crate::error::{GhostwireError, Result};
use crate::proxy_manager::{ProxyManager, RotationStrategy};
use crate::stealth::{StealthConfig, StealthState};
use crate::user_agent::{UserAgent, UserAgentOptions};

// ── Builder ───────────────────────────────────────────────────────────────────

/// Fluent builder for `Ghostwire`.
#[derive(Debug, Clone)]
pub struct GhostwireBuilder {
    // Challenge control
    pub disable_v1: bool,
    pub disable_v2: bool,
    pub disable_v3: bool,
    pub disable_turnstile: bool,

    /// Optional fixed delay before challenge submission (seconds).
    pub delay: Option<f64>,

    /// Maximum number of challenge-solve iterations before giving up.
    pub solve_depth: usize,

    /// Optional captcha provider configuration.
    pub captcha: Option<CaptchaConfig>,

    /// Double-down: re-request after a captcha to see if cfuid alone suffices.
    pub double_down: bool,

    /// Stealth mode configuration.
    pub stealth: StealthConfig,

    /// User-agent selection options.
    pub user_agent_opts: UserAgentOptions,

    // Proxy pool
    pub proxies: Vec<String>,
    pub proxy_rotation: RotationStrategy,
    pub proxy_ban_secs: u64,

    // Session refresh
    pub session_refresh_secs: u64,
    pub auto_refresh_on_403: bool,
    pub max_403_retries: usize,

    /// Minimum seconds between consecutive requests.
    pub min_request_interval_secs: f64,

    /// Print debug information to the log.
    pub debug: bool,

    /// JavaScript interpreter to use when solving v3 VM challenges.
    ///
    /// Defaults to [`JsInterpreter::Auto`], which tries boa → v8 → node → bun
    /// in order and falls back to a heuristic answer if none are available.
    pub js_interpreter: JsInterpreter,

    /// Optional external cookie provider/jar.
    pub cookie_provider: Option<Arc<reqwest::cookie::Jar>>,
}

impl Default for GhostwireBuilder {
    fn default() -> Self {
        GhostwireBuilder {
            disable_v1: false,
            disable_v2: false,
            disable_v3: false,
            disable_turnstile: false,
            delay: None,
            solve_depth: 3,
            captcha: None,
            double_down: true,
            stealth: StealthConfig::default(),
            user_agent_opts: UserAgentOptions {
                desktop: true,
                mobile: true,
                allow_brotli: false,
                ..Default::default()
            },
            proxies: Vec::new(),
            proxy_rotation: RotationStrategy::Sequential,
            proxy_ban_secs: 300,
            session_refresh_secs: 3600,
            auto_refresh_on_403: true,
            max_403_retries: 3,
            min_request_interval_secs: 1.0,
            debug: false,
            js_interpreter: JsInterpreter::Auto,
            cookie_provider: None,
        }
    }
}

impl GhostwireBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn debug(mut self, v: bool) -> Self {
        self.debug = v;
        self
    }
    pub fn disable_v1(mut self, v: bool) -> Self {
        self.disable_v1 = v;
        self
    }
    pub fn disable_v2(mut self, v: bool) -> Self {
        self.disable_v2 = v;
        self
    }
    pub fn disable_v3(mut self, v: bool) -> Self {
        self.disable_v3 = v;
        self
    }
    pub fn disable_turnstile(mut self, v: bool) -> Self {
        self.disable_turnstile = v;
        self
    }
    pub fn delay(mut self, v: f64) -> Self {
        self.delay = Some(v);
        self
    }
    pub fn solve_depth(mut self, v: usize) -> Self {
        self.solve_depth = v;
        self
    }
    pub fn captcha(mut self, v: CaptchaConfig) -> Self {
        self.captcha = Some(v);
        self
    }
    pub fn double_down(mut self, v: bool) -> Self {
        self.double_down = v;
        self
    }
    pub fn stealth(mut self, v: StealthConfig) -> Self {
        self.stealth = v;
        self
    }
    pub fn user_agent_opts(mut self, v: UserAgentOptions) -> Self {
        self.user_agent_opts = v;
        self
    }
    pub fn add_proxy(mut self, v: impl Into<String>) -> Self {
        self.proxies.push(v.into());
        self
    }
    pub fn proxies(mut self, v: Vec<String>) -> Self {
        self.proxies = v;
        self
    }
    pub fn proxy_rotation(mut self, v: RotationStrategy) -> Self {
        self.proxy_rotation = v;
        self
    }
    pub fn proxy_ban_secs(mut self, v: u64) -> Self {
        self.proxy_ban_secs = v;
        self
    }
    pub fn session_refresh_secs(mut self, v: u64) -> Self {
        self.session_refresh_secs = v;
        self
    }
    pub fn auto_refresh_on_403(mut self, v: bool) -> Self {
        self.auto_refresh_on_403 = v;
        self
    }
    pub fn max_403_retries(mut self, v: usize) -> Self {
        self.max_403_retries = v;
        self
    }
    pub fn min_request_interval_secs(mut self, v: f64) -> Self {
        self.min_request_interval_secs = v;
        self
    }

    /// Set the JavaScript interpreter used to solve Cloudflare v3 VM challenges.
    ///
    /// ```rust,no_run
    /// use ghostwire::Ghostwire;
    /// use ghostwire::challenge::JsInterpreter;
    ///
    /// let client = Ghostwire::builder()
    ///     .js_interpreter(JsInterpreter::Node)
    ///     .build()
    ///     .unwrap();
    /// ```
    pub fn js_interpreter(mut self, interp: JsInterpreter) -> Self {
        self.js_interpreter = interp;
        self
    }

    /// Set an external cookie provider/jar to share cookie state with caller.
    pub fn cookie_provider(mut self, jar: Arc<reqwest::cookie::Jar>) -> Self {
        self.cookie_provider = Some(jar);
        self
    }

    /// Consume the builder and produce a `Ghostwire`.
    pub fn build(self) -> Result<Ghostwire> {
        let ua = UserAgent::new(&self.user_agent_opts)?;

        let default_headers = ua.header_map();

        let mut client_builder = reqwest::Client::builder()
            .default_headers(default_headers)
            .gzip(true)
            .brotli(self.user_agent_opts.allow_brotli)
            .deflate(true);

        if let Some(jar) = self.cookie_provider.clone() {
            client_builder = client_builder.cookie_provider(jar);
        } else {
            client_builder = client_builder.cookie_store(true);
        }

        let client = client_builder
            .build()
            .map_err(GhostwireError::HttpError)?;

        let proxy_manager = ProxyManager::new(
            self.proxies.clone(),
            self.proxy_rotation.clone(),
            self.proxy_ban_secs,
        );

        let stealth_state = StealthState::new(self.stealth.clone());
        let js_interpreter = self.js_interpreter.clone();

        Ok(Ghostwire {
            client,
            user_agent: ua,
            config: Arc::new(self),
            js_interpreter,
            proxy_manager: Arc::new(Mutex::new(proxy_manager)),
            stealth: Arc::new(Mutex::new(stealth_state)),
            solve_depth: 0,
            session_start: Instant::now(),
            request_count: 0,
            last_request: None,
            retry_403_count: 0,
        })
    }
}

// ── Ghostwire ──────────────────────────────────────────────────────────────

/// A Cloudflare-aware async HTTP client.
pub struct Ghostwire {
    pub(crate) client: reqwest::Client,
    pub(crate) user_agent: UserAgent,
    pub(crate) config: Arc<GhostwireBuilder>,
    pub(crate) js_interpreter: JsInterpreter,
    #[allow(dead_code)]
    pub(crate) proxy_manager: Arc<Mutex<ProxyManager>>,
    pub(crate) stealth: Arc<Mutex<StealthState>>,

    // Runtime state.
    solve_depth: usize,
    #[allow(dead_code)]
    session_start: Instant,
    #[allow(dead_code)]
    request_count: u64,
    last_request: Option<Instant>,
    retry_403_count: usize,
}

impl Ghostwire {
    /// Create a `Ghostwire` with sensible defaults.
    pub fn new() -> Result<Self> {
        GhostwireBuilder::new().build()
    }

    /// Return a fluent builder.
    pub fn builder() -> GhostwireBuilder {
        GhostwireBuilder::new()
    }

    /// Return a reference to the inner reqwest client.
    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    // ── Convenience HTTP methods ──────────────────────────────────────────────

    pub async fn get(&mut self, url: &str) -> Result<Response> {
        self.request(Method::GET, url, RequestOptions::default())
            .await
    }

    pub async fn post_bytes(&mut self, url: &str, body: Bytes) -> Result<Response> {
        self.request(
            Method::POST,
            url,
            RequestOptions {
                body_bytes: Some(body),
                ..Default::default()
            },
        )
        .await
    }

    pub async fn post_form(&mut self, url: &str, form: Vec<(String, String)>) -> Result<Response> {
        self.request(
            Method::POST,
            url,
            RequestOptions {
                form: Some(form),
                ..Default::default()
            },
        )
        .await
    }

    // ── Core request dispatch ─────────────────────────────────────────────────

    /// Send an HTTP request, automatically handling Cloudflare challenges.
    #[instrument(
        name = "request",
        skip(self, opts),
        fields(method = %method, url = %url, depth = self.solve_depth)
    )]
    pub async fn request(
        &mut self,
        method: Method,
        url: &str,
        opts: RequestOptions,
    ) -> Result<Response> {
        // Rate-limit consecutive requests.
        self.throttle().await;

        // Stealth pre-request hook (human-like delay, etc.).
        {
            let mut stealth = self.stealth.lock().await;
            stealth.pre_request().await;
        }

        // Perform the raw HTTP request.
        let response = self.raw_request(method.clone(), url, &opts).await?;

        let status = response.status().as_u16();
        let server = response
            .headers()
            .get("server")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let headers_clone = response.headers().clone();
        let final_url = response.url().clone();

        debug!(method = %method, url = %url, status = status, "response received");

        // ── Loop-protection ──────────────────────────────────────────────────
        if self.solve_depth >= self.config.solve_depth {
            let depth = self.solve_depth;
            self.solve_depth = 0;
            return Err(GhostwireError::LoopProtection(depth));
        }

        // Collect body text for challenge detection (consumes the response).
        let body = response.text().await.map_err(GhostwireError::HttpError)?;

        // ── Turnstile ────────────────────────────────────────────────────────
        if !self.config.disable_turnstile
            && CloudflareTurnstile::is_turnstile_challenge(status, &server, &body)
        {
            debug!(depth = self.solve_depth + 1, "detected Turnstile challenge");
            self.solve_depth += 1;
            return self.handle_turnstile(final_url.as_str(), &body, opts).await;
        }

        // ── v3 ───────────────────────────────────────────────────────────────
        if !self.config.disable_v3 && CloudflareV3::is_v3_challenge(status, &server, &body) {
            debug!(depth = self.solve_depth + 1, "detected v3 challenge");
            self.solve_depth += 1;
            return self.handle_v3(final_url.as_str(), &body, opts).await;
        }

        // ── v2 ───────────────────────────────────────────────────────────────
        if !self.config.disable_v2 {
            if CloudflareV2::is_v2_captcha_challenge(status, &server, &body) {
                self.solve_depth += 1;
                return self
                    .handle_v2_captcha(final_url.as_str(), &body, opts)
                    .await;
            }
            if CloudflareV2::is_v2_js_challenge(status, &server, &body) {
                self.solve_depth += 1;
                return self.handle_v2_js(final_url.as_str(), &body, opts).await;
            }
        }

        // ── v1 ───────────────────────────────────────────────────────────────
        if !self.config.disable_v1 {
            match CloudflareV1::classify(status, &server, &body) {
                Some(V1ChallengeKind::Firewall1020) => {
                    return Err(GhostwireError::FirewallBlocked);
                }
                Some(V1ChallengeKind::NewIUAM) | Some(V1ChallengeKind::NewCaptcha) => {
                    return Err(GhostwireError::ChallengeError(
                        "Detected a Cloudflare v2 challenge – configure a captcha provider.".into(),
                    ));
                }
                Some(V1ChallengeKind::IUAM) => {
                    self.solve_depth += 1;
                    return self.handle_v1_iuam(final_url.as_str(), &body, opts).await;
                }
                Some(V1ChallengeKind::Captcha) => {
                    self.solve_depth += 1;
                    return self
                        .handle_v1_captcha(final_url.as_str(), &body, opts)
                        .await;
                }
                None => {}
            }
        }

        // ── 403 auto-refresh ──────────────────────────────────────────────────
        if status == 403 && self.config.auto_refresh_on_403 {
            if self.retry_403_count < self.config.max_403_retries {
                self.retry_403_count += 1;
                warn!(
                    retry = self.retry_403_count,
                    max = self.config.max_403_retries,
                    "403 received, retrying"
                );
                return Box::pin(self.request(method, url, opts)).await;
            }
        }

        // No challenge detected – reconstruct response from collected parts.
        self.solve_depth = 0;
        self.retry_403_count = 0;
        build_text_response(status, headers_clone, body)
    }

    // ── Internal request builder ──────────────────────────────────────────────

    async fn raw_request(
        &self,
        method: Method,
        url: &str,
        opts: &RequestOptions,
    ) -> Result<Response> {
        let mut req = self.client.request(method, url);

        // Per-request extra headers.
        if let Some(h) = &opts.headers {
            req = req.headers(h.clone());
        }

        // Stealth-mode extra headers.
        let ua_str = self.user_agent.user_agent_string.clone();
        let mut extra = HeaderMap::new();
        {
            let stealth = self.stealth.lock().await;
            stealth.apply_to_headers(&mut extra, &ua_str);
        }
        req = req.headers(extra);

        // Body: form takes precedence over raw bytes.
        if let Some(form) = &opts.form {
            req = req.form(form);
        } else if let Some(body) = opts.body_bytes.clone() {
            req = req.body(body);
        }

        // Optional per-request timeout.
        if let Some(t) = opts.timeout {
            req = req.timeout(t);
        }

        // Redirect policy.
        if opts.follow_redirects == Some(false) {
            // reqwest's RequestBuilder doesn't expose a per-request redirect
            // override, so we build a one-shot client with redirects disabled,
            // then replay the already-configured request through it.
            let built = req.build().map_err(GhostwireError::HttpError)?;
            let method = built.method().clone();
            let url = built.url().clone();

            let no_redirect_client = reqwest::Client::builder()
                .cookie_store(true)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(GhostwireError::HttpError)?;

            return no_redirect_client
                .request(method, url)
                .send()
                .await
                .map_err(GhostwireError::HttpError);
        }

        req.send().await.map_err(GhostwireError::HttpError)
    }

    // ── Throttle ──────────────────────────────────────────────────────────────

    async fn throttle(&mut self) {
        if let Some(last) = self.last_request {
            let elapsed = last.elapsed().as_secs_f64();
            let min = self.config.min_request_interval_secs;
            if elapsed < min {
                tokio::time::sleep(Duration::from_secs_f64(min - elapsed)).await;
            }
        }
        self.last_request = Some(Instant::now());
        self.request_count += 1;
    }

    // ── Challenge handlers ────────────────────────────────────────────────────

    async fn handle_v1_iuam(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let delay = self
            .config
            .delay
            .unwrap_or_else(|| CloudflareV1::extract_delay(body).unwrap_or(5.0));
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        // Fallback answer: domain length (works for the simple arithmetic IUAM).
        let domain = Url::parse(page_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();
        let answer = domain.len() as f64;

        let (submit_url, form_data) = CloudflareV1::extract_iuam_params(body, page_url, answer)?;

        let parsed = Url::parse(page_url)?;
        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));

        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(form_data),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }

    async fn handle_v1_captcha(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let captcha_cfg = self.config.captcha.as_ref().ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing(
                "No captcha provider configured for v1 captcha challenge.".into(),
            )
        })?;

        if captcha_cfg.provider == "return_response" {
            return build_text_response(403, HeaderMap::new(), body.to_string());
        }

        let solver = make_solver(captcha_cfg).ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing(format!(
                "Unknown captcha provider: {}",
                captcha_cfg.provider
            ))
        })?;

        // Extract the hCaptcha site key.
        let site_key = {
            static RE_SITEKEY: once_cell::sync::Lazy<regex::Regex> =
                once_cell::sync::Lazy::new(|| {
                    regex::Regex::new(r#"data-sitekey="([^"]+)""#).unwrap()
                });
            RE_SITEKEY
                .captures(body)
                .map(|c| c.get(1).unwrap().as_str().to_string())
                .ok_or_else(|| GhostwireError::CaptchaError("Cannot find site key".into()))?
        };

        let token = solver
            .solve(CaptchaKind::HCaptcha, page_url, &site_key, captcha_cfg)
            .await?;

        // Extract the form action.
        let uuid = {
            static RE_FORM: once_cell::sync::Lazy<regex::Regex> = once_cell::sync::Lazy::new(
                || {
                    regex::Regex::new(
                        r#"(?s)<form [^>]*?="challenge-form" action="(?P<uuid>[^"]+__cf_chl_f_tk=[^"]+)""#,
                    )
                    .unwrap()
                },
            );
            RE_FORM
                .captures(body)
                .and_then(|c| c.name("uuid").map(|m| m.as_str().to_string()))
                .ok_or_else(|| GhostwireError::CaptchaError("Cannot find captcha form".into()))?
        };

        let parsed = Url::parse(page_url)?;
        let submit_url = format!(
            "{}://{}{}",
            parsed.scheme(),
            parsed.host_str().unwrap_or(""),
            html_escape::decode_html_entities(&uuid)
        );

        let form_data = vec![
            ("cf-turnstile-response".to_string(), token.clone()),
            ("h-captcha-response".to_string(), token),
        ];

        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(form_data),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }

    async fn handle_v2_js(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let challenge_data = CloudflareV2::extract_challenge_data(body)?;
        let action = CloudflareV2::extract_form_action(body)?;
        let submit_url = CloudflareV2::resolve_url(page_url, &action)?;

        let delay = self
            .config
            .delay
            .unwrap_or_else(|| rand::random::<f64>() * 4.0 + 1.0);
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        let payload = CloudflareV2::build_js_payload(body, &challenge_data)?;

        let parsed = Url::parse(page_url)?;
        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(payload),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }

    async fn handle_v2_captcha(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let captcha_cfg = self.config.captcha.as_ref().ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing("No captcha provider configured".into())
        })?;

        let solver = make_solver(captcha_cfg).ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing(format!(
                "Unknown captcha provider: {}",
                captcha_cfg.provider
            ))
        })?;

        let site_key = CloudflareV2::extract_site_key(body)?;
        let token = solver
            .solve(CaptchaKind::HCaptcha, page_url, &site_key, captcha_cfg)
            .await?;

        let challenge_data = CloudflareV2::extract_challenge_data(body)?;
        let action = CloudflareV2::extract_form_action(body)?;
        let submit_url = CloudflareV2::resolve_url(page_url, &action)?;
        let payload = CloudflareV2::build_captcha_payload(body, &challenge_data, &token)?;

        let parsed = Url::parse(page_url)?;
        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(payload),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }

    async fn handle_v3(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let challenge_data = CloudflareV3::extract_challenge_data(body);

        let action = challenge_data.form_action.as_deref().ok_or_else(|| {
            GhostwireError::V3Error("Cannot find v3 challenge form action".into())
        })?;
        let submit_url = CloudflareV3::resolve_url(page_url, action)?;

        let delay = self
            .config
            .delay
            .unwrap_or_else(|| rand::random::<f64>() * 4.0 + 1.0);
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        let domain = Url::parse(page_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default();

        let answer =
            CloudflareV3::execute_vm_challenge(&challenge_data, &domain, &self.js_interpreter);
        let payload = CloudflareV3::build_payload(body, &answer)?;

        let parsed = Url::parse(page_url)?;
        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(payload),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }

    async fn handle_turnstile(
        &mut self,
        page_url: &str,
        body: &str,
        _opts: RequestOptions,
    ) -> Result<Response> {
        let captcha_cfg = self.config.captcha.as_ref().ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing(
                "Turnstile detected but no captcha provider configured.".into(),
            )
        })?;

        let solver = make_solver(captcha_cfg).ok_or_else(|| {
            GhostwireError::CaptchaProviderMissing(format!(
                "Unknown captcha provider: {}",
                captcha_cfg.provider
            ))
        })?;

        let site_key = CloudflareTurnstile::extract_site_key(body)?;
        let token = solver
            .solve(CaptchaKind::Turnstile, page_url, &site_key, captcha_cfg)
            .await?;

        let submit_url = CloudflareTurnstile::extract_form_action(body, page_url)?;
        let payload = CloudflareTurnstile::build_payload(body, &token);

        let delay = self
            .config
            .delay
            .unwrap_or_else(|| rand::random::<f64>() * 4.0 + 1.0);
        tokio::time::sleep(Duration::from_secs_f64(delay)).await;

        let parsed = Url::parse(page_url)?;
        let origin = format!("{}://{}", parsed.scheme(), parsed.host_str().unwrap_or(""));
        let mut headers = HeaderMap::new();
        headers.insert(ORIGIN, HeaderValue::from_str(&origin).unwrap());
        headers.insert(REFERER, HeaderValue::from_str(page_url).unwrap());

        let post_opts = RequestOptions {
            form: Some(payload),
            headers: Some(headers),
            follow_redirects: Some(true),
            ..Default::default()
        };

        Box::pin(self.request(Method::POST, &submit_url, post_opts)).await
    }
}

// ── RequestOptions ────────────────────────────────────────────────────────────

/// Per-request options passed to `Ghostwire::request`.
#[derive(Default)]
pub struct RequestOptions {
    /// Extra headers merged on top of defaults.
    pub headers: Option<HeaderMap>,
    /// URL-encoded form body. Takes precedence over `body_bytes`.
    pub form: Option<Vec<(String, String)>>,
    /// Raw byte body.
    pub body_bytes: Option<Bytes>,
    /// Per-request timeout.
    pub timeout: Option<Duration>,
    /// `Some(false)` = do NOT follow redirects; `None`/`Some(true)` = follow.
    pub follow_redirects: Option<bool>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Reconstruct a `reqwest::Response` from raw status, headers and body text.
///
/// `reqwest::Response` can be constructed from an `http::Response<Bytes>` via
/// its `From` implementation, which is the approach we use here.
fn build_text_response(status: u16, headers: HeaderMap, body: String) -> Result<Response> {
    let body_bytes = Bytes::from(body.into_bytes());
    let mut builder = http::Response::builder().status(status);
    for (k, v) in &headers {
        builder = builder.header(k, v);
    }
    let http_resp = builder
        .body(body_bytes)
        .map_err(|e| GhostwireError::Other(e.to_string()))?;
    Ok(reqwest::Response::from(http_resp))
}

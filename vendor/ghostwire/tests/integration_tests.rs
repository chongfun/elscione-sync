//! Integration tests for ghostwire.
//!
//! These tests use `wiremock` to simulate Cloudflare challenge pages locally
//! without making real network requests.

use ghostwire::{
    Ghostwire, GhostwireBuilder,
    challenge::{
        JsInterpreter,
        turnstile::CloudflareTurnstile,
        v1::{CloudflareV1, V1ChallengeKind},
        v2::CloudflareV2,
        v3::{CloudflareV3, V3ChallengeData},
    },
    proxy_manager::{ProxyManager, RotationStrategy},
    stealth::{StealthConfig, StealthState},
    user_agent::{Browser, UserAgent, UserAgentOptions},
};

// ── User-Agent tests ──────────────────────────────────────────────────────────

#[test]
fn ua_chrome_desktop() {
    let opts = UserAgentOptions {
        browser: Some(Browser::Chrome),
        desktop: true,
        mobile: false,
        ..Default::default()
    };
    let ua = UserAgent::new(&opts).expect("Should build Chrome UA");
    assert!(
        ua.user_agent_string.contains("Chrome") || ua.user_agent_string.contains("Chromium"),
        "UA string should mention Chrome: {}",
        ua.user_agent_string
    );
    assert!(!ua.cipher_suite.is_empty(), "Should have cipher suites");
}

#[test]
fn ua_firefox_desktop() {
    let opts = UserAgentOptions {
        browser: Some(Browser::Firefox),
        desktop: true,
        mobile: false,
        ..Default::default()
    };
    let ua = UserAgent::new(&opts).expect("Should build Firefox UA");
    assert!(
        ua.user_agent_string.contains("Firefox"),
        "UA string should mention Firefox: {}",
        ua.user_agent_string
    );
}

#[test]
fn ua_custom_string() {
    let custom = "MyCustomBot/1.0".to_string();
    let opts = UserAgentOptions {
        custom: Some(custom.clone()),
        desktop: true,
        mobile: true,
        ..Default::default()
    };
    let ua = UserAgent::new(&opts).expect("Should build custom UA");
    assert_eq!(ua.user_agent_string, custom);
}

#[test]
fn ua_no_brotli_stripped() {
    let opts = UserAgentOptions {
        browser: Some(Browser::Chrome),
        desktop: true,
        mobile: false,
        allow_brotli: false,
        ..Default::default()
    };
    let ua = UserAgent::new(&opts).expect("Should build UA");
    assert!(
        !ua.headers.accept_encoding.contains("br"),
        "Accept-Encoding should not contain 'br' when allow_brotli=false: {}",
        ua.headers.accept_encoding
    );
}

#[test]
fn ua_brotli_kept_when_allowed() {
    let opts = UserAgentOptions {
        browser: Some(Browser::Chrome),
        desktop: true,
        mobile: false,
        allow_brotli: true,
        ..Default::default()
    };
    let ua = UserAgent::new(&opts).expect("Should build UA");
    assert!(
        ua.headers.accept_encoding.contains("br"),
        "Accept-Encoding should contain 'br' when allow_brotli=true: {}",
        ua.headers.accept_encoding
    );
}

// ── Proxy Manager tests ───────────────────────────────────────────────────────

#[test]
fn proxy_manager_sequential() {
    let mut pm = ProxyManager::new(
        vec![
            "http://proxy1:8080".to_string(),
            "http://proxy2:8080".to_string(),
            "http://proxy3:8080".to_string(),
        ],
        RotationStrategy::Sequential,
        300,
    );

    let p1 = pm.next_proxy().unwrap();
    let p2 = pm.next_proxy().unwrap();
    let _p3 = pm.next_proxy().unwrap();
    let p4 = pm.next_proxy().unwrap(); // wraps around

    assert_ne!(p1, p2);
    assert_eq!(p1, p4); // wrapped back to first
}

#[test]
fn proxy_manager_ban_and_recover() {
    let mut pm = ProxyManager::new(
        vec!["http://proxy1:8080".to_string()],
        RotationStrategy::Sequential,
        0, // ban duration 0 secs = effectively instant recovery
    );

    pm.report_failure("http://proxy1:8080");
    // With ban_secs=0 the proxy should be available immediately.
    let proxy = pm.next_proxy();
    assert!(
        proxy.is_some(),
        "Should recover immediately with 0 ban duration"
    );
}

#[test]
fn proxy_manager_empty() {
    let mut pm = ProxyManager::new(vec![], RotationStrategy::Sequential, 300);
    assert!(!pm.has_proxies());
    assert!(pm.next_proxy().is_none());
}

#[test]
fn proxy_format_roundtrip() {
    let proxy = ProxyManager::format_proxy("http://user:pass@host:8080");
    assert!(proxy.is_ok());
}

// ── Challenge detection tests ─────────────────────────────────────────────────

const CLOUDFLARE_SERVER: &str = "cloudflare";

fn cf_iuam_body() -> &'static str {
    r#"
    <html>
    <body>
    <img src="/cdn-cgi/images/trace/jsch/transparent.gif"/>
    <form id="challenge-form" action="/abc123/__cf_chl_f_tk=token456">
    <input name="r" value="token"/>
    <input name="jschl_vc" value="vc"/>
    <input name="pass" value="pass"/>
    </form>
    <script>
    setTimeout(function(){
    document.getElementById('challenge-form').submit();
    }, 4000);
    </script>
    </body>
    </html>
    "#
}

fn cf_captcha_body() -> &'static str {
    r#"
    <html>
    <body>
    <img src="/cdn-cgi/images/trace/captcha/transparent.gif"/>
    <form id="challenge-form" action="/abc123/__cf_chl_f_tk=token456">
    <input name="r" value="rtoken"/>
    <div class="h-captcha" data-sitekey="abc123def456ghi789jkl0123456789mnop4567"></div>
    </form>
    </body>
    </html>
    "#
}

fn cf_turnstile_body() -> &'static str {
    r#"
    <html>
    <body>
    <div class="cf-turnstile" data-sitekey="0x4AAAAAAADnPIDROrmt1Wwj"></div>
    <script src="https://challenges.cloudflare.com/turnstile/v0/api.js"></script>
    </body>
    </html>
    "#
}

fn cf_firewall_body() -> &'static str {
    r#"
    <html><body>
    <span class="cf-error-code">1020</span>
    </body></html>
    "#
}

#[test]
fn detect_iuam_challenge() {
    assert!(CloudflareV1::is_iuam_challenge(
        503,
        CLOUDFLARE_SERVER,
        cf_iuam_body()
    ));
    assert!(!CloudflareV1::is_iuam_challenge(
        200,
        CLOUDFLARE_SERVER,
        "<html></html>"
    ));
    assert!(!CloudflareV1::is_iuam_challenge(
        503,
        "nginx",
        cf_iuam_body()
    ));
}

#[test]
fn detect_captcha_challenge() {
    assert!(CloudflareV1::is_captcha_challenge(
        403,
        CLOUDFLARE_SERVER,
        cf_captcha_body()
    ));
    assert!(!CloudflareV1::is_captcha_challenge(
        200,
        CLOUDFLARE_SERVER,
        "<html></html>"
    ));
}

#[test]
fn detect_firewall_blocked() {
    assert!(CloudflareV1::is_firewall_blocked(
        403,
        CLOUDFLARE_SERVER,
        cf_firewall_body()
    ));
    assert!(!CloudflareV1::is_firewall_blocked(
        200,
        CLOUDFLARE_SERVER,
        "<html></html>"
    ));
}

#[test]
fn classify_returns_firewall_first() {
    let kind = CloudflareV1::classify(403, CLOUDFLARE_SERVER, cf_firewall_body());
    assert_eq!(kind, Some(V1ChallengeKind::Firewall1020));
}

#[test]
fn classify_returns_none_for_clean_page() {
    let kind = CloudflareV1::classify(200, "nginx", "<html><body>Hello</body></html>");
    assert!(kind.is_none());
}

#[test]
fn detect_turnstile_challenge() {
    assert!(CloudflareTurnstile::is_turnstile_challenge(
        403,
        CLOUDFLARE_SERVER,
        cf_turnstile_body()
    ));
    assert!(!CloudflareTurnstile::is_turnstile_challenge(
        200,
        "nginx",
        "<html></html>"
    ));
}

#[test]
fn extract_turnstile_site_key() {
    let key = CloudflareTurnstile::extract_site_key(cf_turnstile_body()).unwrap();
    assert_eq!(key, "0x4AAAAAAADnPIDROrmt1Wwj");
}

#[test]
fn extract_iuam_delay() {
    let delay = CloudflareV1::extract_delay(cf_iuam_body());
    assert_eq!(delay, Some(4.0));
}

#[test]
fn v2_detection() {
    let body = r#"cpo.src = '/cdn-cgi/challenge-platform/h/b/orchestrate/jsch/v1?ray=abc';"#;
    assert!(CloudflareV2::is_v2_js_challenge(
        503,
        CLOUDFLARE_SERVER,
        body
    ));
}

#[test]
fn v3_detection() {
    let body = r#"window._cf_chl_ctx = {"cvId": "abc"};"#;
    assert!(CloudflareV3::is_v3_challenge(503, CLOUDFLARE_SERVER, body));
}

// ── Builder / scraper construction tests ─────────────────────────────────────

#[test]
fn build_default_scraper() {
    let ghostwire = Ghostwire::new();
    assert!(
        ghostwire.is_ok(),
        "Default ghostwire should build successfully"
    );
}

#[test]
fn build_with_custom_ua() {
    let opts = UserAgentOptions {
        custom: Some("TestBot/1.0".to_string()),
        desktop: true,
        mobile: true,
        ..Default::default()
    };
    let ghostwire = GhostwireBuilder::new().user_agent_opts(opts).build();
    assert!(ghostwire.is_ok());
}

#[test]
fn build_with_stealth_disabled() {
    let stealth = StealthConfig {
        enabled: false,
        ..Default::default()
    };
    let ghostwire = GhostwireBuilder::new().stealth(stealth).build();
    assert!(ghostwire.is_ok());
}

// ── Stealth mode tests ────────────────────────────────────────────────────────

#[test]
fn stealth_adds_chrome_quirk_headers() {
    use reqwest::header::HeaderMap;
    let config = StealthConfig {
        enabled: true,
        browser_quirks: true,
        ..Default::default()
    };
    let state = StealthState::new(config);
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 Chrome/120.0";
    let mut headers = HeaderMap::new();
    state.apply_to_headers(&mut headers, ua);
    assert!(
        headers.contains_key("sec-ch-ua"),
        "Should inject sec-ch-ua for Chrome UA"
    );
}

#[test]
fn stealth_adds_firefox_quirk_headers() {
    use reqwest::header::HeaderMap;
    let config = StealthConfig {
        enabled: true,
        browser_quirks: true,
        ..Default::default()
    };
    let state = StealthState::new(config);
    let ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:120.0) Gecko/20100101 Firefox/120.0";
    let mut headers = HeaderMap::new();
    state.apply_to_headers(&mut headers, ua);
    assert!(
        headers.contains_key("upgrade-insecure-requests"),
        "Should inject Upgrade-Insecure-Requests for Firefox UA"
    );
}

#[test]
fn stealth_disabled_no_headers_added() {
    use reqwest::header::HeaderMap;
    let config = StealthConfig {
        enabled: false,
        ..Default::default()
    };
    let state = StealthState::new(config);
    let mut headers = HeaderMap::new();
    state.apply_to_headers(&mut headers, "SomeBot/1.0");
    assert!(
        headers.is_empty(),
        "Disabled stealth should not add any headers"
    );
}

// ── V3 challenge data extraction tests ───────────────────────────────────────

fn cf_v3_body_full() -> &'static str {
    r#"
    <html><head></head><body>
    <script>
    cpo.src = '/cdn-cgi/challenge-platform/h/b/orchestrate/jsch/v3?ray=abc';
    </script>
    <script>
    window._cf_chl_ctx = {"cvId":"cv-abc-123","chType":"jsch","cZone":"example.com"};
    window._cf_chl_opt = {"chlPageData":"page-data-xyz","cvId":"cv-abc-123"};
    </script>
    <form id="challenge-form" action="/cdn-cgi/challenge-platform/h/b/flow/ov1/0123456789/__cf_chl_rt_tk=TOKEN123">
    <input type="hidden" name="r" value="r-token-value"/>
    <input type="hidden" name="cf_chl_seq_df" value="seq-value"/>
    </form>
    <script>
    window._cf_chl_enter = function(){};
    window._cf_chl_answer = '42';
    </script>
    </body></html>
    "#
}

fn cf_v3_body_ctx_only() -> &'static str {
    r#"
    <html><body>
    <script>window._cf_chl_ctx = {"cvId":"ctx-only-id"};</script>
    <form id="challenge-form" action="/challenge?__cf_chl_rt_tk=TOK">
    <input name="r" value="r-val"/>
    </form>
    </body></html>
    "#
}

fn cf_v3_body_platform() -> &'static str {
    r#"
    <html><body>
    <script>cpo.src = '/cdn-cgi/challenge-platform/h/b/orchestrate/jsch/v3?ray=xyz';</script>
    <form id="challenge-form" action="/cf?__cf_chl_rt_tk=T">
    <input name="r" value="rv"/>
    </form>
    </body></html>
    "#
}

#[test]
fn v3_detection_via_ctx() {
    assert!(CloudflareV3::is_v3_challenge(
        503,
        CLOUDFLARE_SERVER,
        cf_v3_body_ctx_only()
    ));
}

#[test]
fn v3_detection_via_platform_script() {
    assert!(CloudflareV3::is_v3_challenge(
        503,
        CLOUDFLARE_SERVER,
        cf_v3_body_platform()
    ));
}

#[test]
fn v3_detection_via_form_rt_tk() {
    let body = r#"<form id="challenge-form" action="/x?__cf_chl_rt_tk=T"><input name="r" value="v"/></form>"#;
    assert!(CloudflareV3::is_v3_challenge(403, CLOUDFLARE_SERVER, body));
}

#[test]
fn v3_not_detected_on_clean_page() {
    assert!(!CloudflareV3::is_v3_challenge(
        200,
        CLOUDFLARE_SERVER,
        "<html><body>Hello</body></html>"
    ));
}

#[test]
fn v3_not_detected_on_non_cloudflare_server() {
    assert!(!CloudflareV3::is_v3_challenge(
        503,
        "nginx",
        cf_v3_body_ctx_only()
    ));
}

#[test]
fn v3_extract_ctx_json() {
    let data = CloudflareV3::extract_challenge_data(cf_v3_body_full());
    assert_eq!(
        data.ctx.get("cvId").and_then(|v| v.as_str()),
        Some("cv-abc-123")
    );
    assert_eq!(
        data.ctx.get("chType").and_then(|v| v.as_str()),
        Some("jsch")
    );
}

#[test]
fn v3_extract_opt_json() {
    let data = CloudflareV3::extract_challenge_data(cf_v3_body_full());
    assert_eq!(
        data.opt.get("chlPageData").and_then(|v| v.as_str()),
        Some("page-data-xyz")
    );
}

#[test]
fn v3_extract_form_action() {
    let data = CloudflareV3::extract_challenge_data(cf_v3_body_full());
    assert!(
        data.form_action
            .as_deref()
            .unwrap_or("")
            .contains("__cf_chl_rt_tk"),
        "form_action should contain the rt_tk token"
    );
}

#[test]
fn v3_extract_vm_script() {
    let data = CloudflareV3::extract_challenge_data(cf_v3_body_full());
    assert!(
        data.vm_script.is_some(),
        "should extract the VM script block"
    );
    assert!(
        data.vm_script.as_deref().unwrap().contains("_cf_chl_enter"),
        "vm_script should contain _cf_chl_enter"
    );
}

#[test]
fn v3_extract_missing_ctx_defaults_to_null() {
    let body = r#"<form id="challenge-form" action="/x?__cf_chl_rt_tk=T"></form>"#;
    let data = CloudflareV3::extract_challenge_data(body);
    assert!(
        data.ctx.is_null(),
        "missing ctx should default to JSON null"
    );
}

#[test]
fn v3_build_payload_extracts_r_token() {
    let payload = CloudflareV3::build_payload(cf_v3_body_full(), "test-answer")
        .expect("build_payload should succeed");
    let r = payload
        .iter()
        .find(|(k, _)| k == "r")
        .map(|(_, v)| v.as_str());
    assert_eq!(r, Some("r-token-value"));
}

#[test]
fn v3_build_payload_contains_answer() {
    let payload = CloudflareV3::build_payload(cf_v3_body_full(), "my-answer")
        .expect("build_payload should succeed");
    let answer = payload
        .iter()
        .find(|(k, _)| k == "jschl_answer")
        .map(|(_, v)| v.as_str());
    assert_eq!(answer, Some("my-answer"));
}

#[test]
fn v3_build_payload_includes_extra_inputs() {
    let payload = CloudflareV3::build_payload(cf_v3_body_full(), "ans")
        .expect("build_payload should succeed");
    let has_seq = payload
        .iter()
        .any(|(k, v)| k == "cf_chl_seq_df" && v == "seq-value");
    assert!(has_seq, "payload should include extra hidden inputs");
}

#[test]
fn v3_build_payload_missing_r_token_errors() {
    let body = "<html><body>no form here</body></html>";
    assert!(
        CloudflareV3::build_payload(body, "ans").is_err(),
        "missing r token should return an error"
    );
}

#[test]
fn v3_resolve_url_absolute_passthrough() {
    let resolved =
        CloudflareV3::resolve_url("https://example.com/page", "https://other.com/challenge")
            .unwrap();
    assert_eq!(resolved, "https://other.com/challenge");
}

#[test]
fn v3_resolve_url_relative_action() {
    let resolved = CloudflareV3::resolve_url(
        "https://example.com/page",
        "/cdn-cgi/challenge-platform/flow?__cf_chl_rt_tk=T",
    )
    .unwrap();
    assert!(
        resolved.starts_with("https://example.com"),
        "resolved URL should use original scheme+host: {resolved}"
    );
    assert!(resolved.contains("__cf_chl_rt_tk"));
}

// ── V3 fallback answer tests ──────────────────────────────────────────────────

#[test]
fn v3_fallback_uses_page_data_hash() {
    let mut data = V3ChallengeData::default();
    data.opt = serde_json::json!({"chlPageData": "some-page-data"});
    let a1 = CloudflareV3::generate_fallback_answer(&data);
    let a2 = CloudflareV3::generate_fallback_answer(&data);
    // Same input → same deterministic output.
    assert_eq!(
        a1, a2,
        "fallback should be deterministic for same page data"
    );
    // Result must be purely numeric.
    assert!(
        a1.chars().all(|c| c.is_ascii_digit()),
        "answer must be numeric: {a1}"
    );
}

#[test]
fn v3_fallback_uses_cv_id_when_no_page_data() {
    let mut data = V3ChallengeData::default();
    data.ctx = serde_json::json!({"cvId": "some-cv-id"});
    let a = CloudflareV3::generate_fallback_answer(&data);
    assert!(
        a.chars().all(|c| c.is_ascii_digit()),
        "answer must be numeric: {a}"
    );
}

#[test]
fn v3_fallback_random_when_no_data() {
    let data = V3ChallengeData::default();
    let a = CloudflareV3::generate_fallback_answer(&data);
    // Should be a 6-digit number (100000–999999).
    assert!(
        a.chars().all(|c| c.is_ascii_digit()),
        "answer must be numeric: {a}"
    );
    let n: u64 = a.parse().unwrap();
    assert!(
        (100_000..=999_999).contains(&n),
        "random fallback should be 6 digits: {n}"
    );
}

#[test]
fn v3_fallback_page_data_beats_cv_id() {
    // When both are present, chlPageData takes priority.
    let mut data = V3ChallengeData::default();
    data.opt = serde_json::json!({"chlPageData": "pd"});
    data.ctx = serde_json::json!({"cvId": "cv"});
    let with_both = CloudflareV3::generate_fallback_answer(&data);

    let mut only_page = V3ChallengeData::default();
    only_page.opt = serde_json::json!({"chlPageData": "pd"});
    let with_page_only = CloudflareV3::generate_fallback_answer(&only_page);

    assert_eq!(
        with_both, with_page_only,
        "chlPageData should take priority over cvId"
    );
}

// ── V3 execute_vm_challenge tests ─────────────────────────────────────────────

/// Returns true when the named binary exists somewhere in PATH.
fn have_binary(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

#[test]
fn v3_execute_vm_falls_back_when_no_vm_script() {
    // No vm_script → must fall back to heuristic, never panic.
    let mut data = V3ChallengeData::default();
    data.ctx = serde_json::json!({"cvId": "fallback-cv"});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::None);
    assert!(
        answer.chars().all(|c| c.is_ascii_digit()),
        "fallback answer must be numeric: {answer}"
    );
}

#[test]
fn v3_execute_vm_interp_none_uses_fallback() {
    let mut data = V3ChallengeData::default();
    data.vm_script = Some("window._cf_chl_answer = 'should-not-appear';".into());
    data.ctx = serde_json::json!({"cvId": "cv-id-here"});
    // JsInterpreter::None → skips JS → falls back to cvId hash.
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::None);
    assert!(
        answer.chars().all(|c| c.is_ascii_digit()),
        "with None interpreter answer must be numeric: {answer}"
    );
}

#[test]
fn v3_execute_vm_with_node() {
    if !have_binary("node") {
        eprintln!("skipping v3_execute_vm_with_node: node not in PATH");
        return;
    }
    let mut data = V3ChallengeData::default();
    // vm_script sets the answer directly so we know what to expect.
    data.vm_script = Some("window._cf_chl_answer = 'node-vm-42';".into());
    data.ctx = serde_json::json!({});
    data.opt = serde_json::json!({});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Node);
    assert_eq!(answer, "node-vm-42", "node should extract the VM answer");
}

#[test]
fn v3_execute_vm_with_bun() {
    if !have_binary("bun") {
        eprintln!("skipping v3_execute_vm_with_bun: bun not in PATH");
        return;
    }
    let mut data = V3ChallengeData::default();
    data.vm_script = Some("window._cf_chl_answer = 'bun-vm-99';".into());
    data.ctx = serde_json::json!({});
    data.opt = serde_json::json!({});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Bun);
    assert_eq!(answer, "bun-vm-99", "bun should extract the VM answer");
}

#[cfg(feature = "js-boa")]
#[test]
fn v3_execute_vm_with_boa() {
    let mut data = V3ChallengeData::default();
    data.vm_script = Some("window._cf_chl_answer = 'boa-vm-7';".into());
    data.ctx = serde_json::json!({});
    data.opt = serde_json::json!({});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Boa);
    assert_eq!(answer, "boa-vm-7", "boa should extract the VM answer");
}

#[cfg(feature = "js-v8")]
#[test]
fn v3_execute_vm_with_v8() {
    let mut data = V3ChallengeData::default();
    data.vm_script = Some("window._cf_chl_answer = 'v8-vm-3';".into());
    data.ctx = serde_json::json!({});
    data.opt = serde_json::json!({});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::V8);
    assert_eq!(answer, "v8-vm-3", "v8 should extract the VM answer");
}

#[test]
fn v3_execute_vm_ctx_accessible_from_script() {
    if !have_binary("node") {
        eprintln!("skipping: node not in PATH");
        return;
    }
    let mut data = V3ChallengeData::default();
    // The script reads back the injected cvId from window._cf_chl_ctx.
    data.vm_script = Some("window._cf_chl_answer = window._cf_chl_ctx.cvId;".into());
    data.ctx = serde_json::json!({"cvId": "injected-cv-id"});
    data.opt = serde_json::json!({});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Node);
    assert_eq!(
        answer, "injected-cv-id",
        "ctx should be accessible from the VM script"
    );
}

#[test]
fn v3_execute_vm_opt_accessible_from_script() {
    if !have_binary("node") {
        eprintln!("skipping: node not in PATH");
        return;
    }
    let mut data = V3ChallengeData::default();
    data.vm_script = Some("window._cf_chl_answer = window._cf_chl_opt.chlPageData;".into());
    data.ctx = serde_json::json!({});
    data.opt = serde_json::json!({"chlPageData": "injected-page-data"});
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Node);
    assert_eq!(
        answer, "injected-page-data",
        "opt should be accessible from the VM script"
    );
}

#[test]
fn v3_execute_vm_crashing_script_falls_back() {
    if !have_binary("node") {
        eprintln!("skipping: node not in PATH");
        return;
    }
    let mut data = V3ChallengeData::default();
    // Deliberately broken JS that will cause node to exit non-zero.
    data.vm_script = Some("this is not valid javascript !!!".into());
    data.ctx = serde_json::json!({"cvId": "crash-cv"});
    data.opt = serde_json::json!({});
    // Must not panic; must return a numeric heuristic answer.
    let answer = CloudflareV3::execute_vm_challenge(&data, "example.com", &JsInterpreter::Node);
    assert!(
        answer.chars().all(|c| c.is_ascii_digit()),
        "crashing script should fall back to numeric heuristic: {answer}"
    );
}

// ── GhostwireBuilder js_interpreter field ────────────────────────────────────

#[test]
fn builder_default_interpreter_is_auto() {
    // Confirm the default is Auto so existing users are unaffected.
    let b = GhostwireBuilder::new();
    assert_eq!(b.js_interpreter, JsInterpreter::Auto);
}

#[test]
fn builder_interpreter_can_be_set() {
    for interp in [
        JsInterpreter::None,
        JsInterpreter::Node,
        JsInterpreter::Bun,
        JsInterpreter::Boa,
        JsInterpreter::V8,
        JsInterpreter::Auto,
    ] {
        let b = GhostwireBuilder::new().js_interpreter(interp.clone());
        assert_eq!(b.js_interpreter, interp);
    }
}

#[test]
fn build_with_node_interpreter() {
    let g = GhostwireBuilder::new()
        .js_interpreter(JsInterpreter::Node)
        .build();
    assert!(g.is_ok(), "should build with Node interpreter");
}

#[test]
fn build_with_none_interpreter() {
    let g = GhostwireBuilder::new()
        .js_interpreter(JsInterpreter::None)
        .build();
    assert!(g.is_ok(), "should build with None interpreter");
}

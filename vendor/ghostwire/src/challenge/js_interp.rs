//! JavaScript interpreter abstraction for Cloudflare v3 VM challenge solving.
//!
//! Interpreter chain (first success wins):
//!  1. `js-v8`  feature → [`v8`] bindings
//!  2. `node`   binary  → subprocess
//!  3. `bun`    binary  → subprocess
//!  4. `js-boa` feature → [`boa_engine`]
//!  5. Heuristic fallback (caller's responsibility)

use tracing::{debug, warn};

#[derive(Debug)]
pub enum JsResult {
    Ok(String),
    Unavailable,
}

/// Selects which JavaScript engine is used to solve v3 VM challenges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsInterpreter {
    /// Try v8 → node → bun → boa in order.
    Auto,
    /// Requires the `js-boa` feature.
    Boa,
    /// Requires the `js-v8` feature.
    V8,
    Node,
    Bun,
    /// Skip JS entirely; caller falls back to heuristic answer.
    None,
}

impl Default for JsInterpreter {
    fn default() -> Self {
        Self::Auto
    }
}

impl JsInterpreter {
    pub fn eval(&self, script: &str, domain: &str) -> JsResult {
        match self {
            Self::Auto => eval_auto(script, domain),
            Self::Boa => eval_boa(script, domain),
            Self::V8 => eval_v8(script, domain),
            Self::Node => eval_subprocess(script, domain, SubprocessKind::Node),
            Self::Bun => eval_subprocess(script, domain, SubprocessKind::Bun),
            Self::None => JsResult::Unavailable,
        }
    }

    /// Wraps `raw_script` in a browser-like environment, injects `_cf_chl_ctx`
    /// and `_cf_chl_opt`, and returns `window._cf_chl_answer` when done.
    pub fn build_vm_script(
        raw_script: &str,
        domain: &str,
        ctx_json: &str,
        opt_json: &str,
    ) -> String {
        format!(
            r#"
(function() {{
    var _answer;
    var window = {{
        location: {{
            href: 'https://{domain}/',
            hostname: '{domain}',
            protocol: 'https:',
            pathname: '/',
            host: '{domain}',
            origin: 'https://{domain}'
        }},
        navigator: {{
            userAgent: 'Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36',
            platform: 'Win32',
            language: 'en-US',
            languages: ['en-US', 'en'],
            hardwareConcurrency: 4,
            cookieEnabled: true
        }},
        document: {{
            getElementById: function(id) {{ return {{ value: '', style: {{}} }}; }},
            createElement: function(tag) {{
                return {{
                    firstChild: {{ href: 'https://{domain}/' }},
                    style: {{}}
                }};
            }},
            cookie: ''
        }},
        screen: {{ width: 1920, height: 1080, colorDepth: 24 }},
        _cf_chl_ctx: {ctx_json},
        _cf_chl_opt: {opt_json},
        _cf_chl_enter: function() {{ return true; }}
    }};
    var document  = window.document;
    var location  = window.location;
    var navigator = window.navigator;
    var screen    = window.screen;
    var setTimeout  = function(fn, _ms) {{ try {{ fn(); }} catch(e) {{}} }};
    var setInterval = function(_fn, _ms) {{ return 0; }};
    var clearTimeout  = function() {{}};
    var clearInterval = function() {{}};

    try {{
        {raw_script}
    }} catch(e) {{}}

    if (typeof window._cf_chl_answer !== 'undefined') {{
        _answer = String(window._cf_chl_answer);
    }} else if (typeof _cf_chl_answer !== 'undefined') {{
        _answer = String(_cf_chl_answer);
    }} else {{
        _answer = '';
    }}
    return _answer;
}})()
"#,
            domain = domain,
            ctx_json = ctx_json,
            opt_json = opt_json,
            raw_script = raw_script,
        )
    }
}

fn eval_auto(script: &str, domain: &str) -> JsResult {
    let r = eval_v8(script, domain);
    if matches!(r, JsResult::Ok(_)) {
        return r;
    }
    let r = eval_subprocess(script, domain, SubprocessKind::Node);
    if matches!(r, JsResult::Ok(_)) {
        return r;
    }
    let r = eval_subprocess(script, domain, SubprocessKind::Bun);
    if matches!(r, JsResult::Ok(_)) {
        return r;
    }
    let r = eval_boa(script, domain);
    if matches!(r, JsResult::Ok(_)) {
        return r;
    }
    warn!("All JS interpreters failed or unavailable for domain={domain}");
    JsResult::Unavailable
}

#[cfg(feature = "js-boa")]
fn eval_boa(script: &str, _domain: &str) -> JsResult {
    use boa_engine::{Context, Source};

    debug!("Trying boa_engine");
    let mut ctx = Context::default();
    match ctx.eval(Source::from_bytes(script)) {
        Ok(val) => {
            let s = val
                .to_string(&mut ctx)
                .map(|js_str| js_str.to_std_string().unwrap_or_default())
                .unwrap_or_default();
            debug!("boa_engine result: {s:?}");
            if s.is_empty() || s == "undefined" || s == "null" {
                JsResult::Unavailable
            } else {
                JsResult::Ok(s)
            }
        }
        Err(e) => {
            warn!("boa_engine error: {e}");
            JsResult::Unavailable
        }
    }
}

#[cfg(not(feature = "js-boa"))]
fn eval_boa(_script: &str, _domain: &str) -> JsResult {
    JsResult::Unavailable
}

#[cfg(feature = "js-v8")]
fn eval_v8(script: &str, _domain: &str) -> JsResult {
    debug!("Trying v8");

    static V8_INIT: std::sync::Once = std::sync::Once::new();
    V8_INIT.call_once(|| {
        let platform = v8::new_default_platform(0, false).make_shared();
        v8::V8::initialize_platform(platform);
        v8::V8::initialize();
    });

    let isolate = &mut v8::Isolate::new(Default::default());
    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    let source_str = match v8::String::new(scope, script) {
        Some(s) => s,
        None => return JsResult::Unavailable,
    };
    let script_obj = match v8::Script::compile(scope, source_str, None) {
        Some(s) => s,
        None => {
            warn!("v8: compilation failed");
            return JsResult::Unavailable;
        }
    };
    match script_obj.run(scope) {
        Some(val) => {
            let s = val.to_rust_string_lossy(scope);
            debug!("v8 result: {s:?}");
            if s.is_empty() || s == "undefined" || s == "null" {
                JsResult::Unavailable
            } else {
                JsResult::Ok(s)
            }
        }
        None => {
            warn!("v8: execution failed");
            JsResult::Unavailable
        }
    }
}

#[cfg(not(feature = "js-v8"))]
fn eval_v8(_script: &str, _domain: &str) -> JsResult {
    JsResult::Unavailable
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubprocessKind {
    Node,
    Bun,
}

impl SubprocessKind {
    fn binary(self) -> &'static str {
        match self {
            Self::Node => "node",
            Self::Bun => "bun",
        }
    }

    fn wrap(self, script: &str) -> String {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let encoded = STANDARD.encode(script.as_bytes());
        match self {
            Self::Node => format!(
                r#"
var src = Buffer.from('{encoded}', 'base64').toString('utf8');
var vm  = require('vm');
var ctx = vm.createContext({{
    console: console,
    Math: Math,
    JSON: JSON,
    parseInt: parseInt,
    parseFloat: parseFloat,
    encodeURIComponent: encodeURIComponent,
    decodeURIComponent: decodeURIComponent,
    setTimeout:  function(fn, ms) {{ try {{ fn(); }} catch(e) {{}} }},
    setInterval: function()       {{ return 0; }},
    clearTimeout:  function() {{}},
    clearInterval: function() {{}},
}});
try {{
    var result = vm.runInContext(src, ctx, {{ timeout: 5000 }});
    if (result !== undefined && result !== null) {{
        process.stdout.write(String(result));
    }}
}} catch(e) {{
    process.stderr.write('node-interp error: ' + e.message);
    process.exit(1);
}}
"#
            ),
            Self::Bun => format!(
                r#"
var src = Buffer.from('{encoded}', 'base64').toString('utf8');
var fn_ = new Function(src + '\n;return (typeof _answer !== "undefined" ? _answer : "");');
try {{
    var result = fn_();
    if (result !== undefined && result !== null) {{
        process.stdout.write(String(result));
    }}
}} catch(e) {{
    process.stderr.write('bun-interp error: ' + e.message);
    process.exit(1);
}}
"#
            ),
        }
    }
}

fn eval_subprocess(script: &str, domain: &str, kind: SubprocessKind) -> JsResult {
    use std::process::{Command, Stdio};

    let binary = kind.binary();
    debug!("Trying {binary} for domain={domain}");

    if which(binary).is_none() {
        return JsResult::Unavailable;
    }

    let wrapper = kind.wrap(script);
    let mut child = match Command::new(binary)
        .arg("-e")
        .arg(&wrapper)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("{binary} spawn error: {e}");
            return JsResult::Unavailable;
        }
    };

    let timeout = std::time::Duration::from_secs(8);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                warn!("{binary} timed out");
                return JsResult::Unavailable;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => {
                warn!("{binary} wait error: {e}");
                return JsResult::Unavailable;
            }
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => {
            warn!("{binary} output error: {e}");
            return JsResult::Unavailable;
        }
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("{binary} exited non-zero: {stderr}");
        return JsResult::Unavailable;
    }

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    debug!("{binary} stdout: {stdout:?}");

    if stdout.is_empty() || stdout == "undefined" || stdout == "null" {
        JsResult::Unavailable
    } else {
        JsResult::Ok(stdout)
    }
}

fn which(binary: &str) -> Option<std::path::PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(target_os = "windows")]
        {
            let exe = dir.join(format!("{binary}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have(bin: &str) -> bool {
        which(bin).is_some()
    }

    const SIMPLE_EXPR: &str = "(function(){ return String(6 * 7); })()";

    #[test]
    fn build_vm_script_contains_domain() {
        let s = JsInterpreter::build_vm_script("/* raw */", "example.com", "{}", "{}");
        assert!(s.contains("example.com"));
    }

    #[test]
    fn build_vm_script_injects_ctx_opt() {
        let ctx = r#"{"cvId":"abc"}"#;
        let opt = r#"{"chlPageData":"xyz"}"#;
        let s = JsInterpreter::build_vm_script("/* raw */", "example.com", ctx, opt);
        assert!(s.contains(ctx));
        assert!(s.contains(opt));
    }

    #[test]
    fn build_vm_script_has_window_stubs() {
        let s = JsInterpreter::build_vm_script("", "x.com", "{}", "{}");
        for stub in &["setTimeout", "setInterval", "clearTimeout", "clearInterval"] {
            assert!(s.contains(stub), "missing timer stub: {stub}");
        }
        for prop in &["navigator", "document", "location", "screen"] {
            assert!(s.contains(prop), "missing window property: {prop}");
        }
    }

    #[test]
    fn build_vm_script_captures_answer() {
        let s = JsInterpreter::build_vm_script("", "x.com", "{}", "{}");
        assert!(s.contains("window._cf_chl_answer"));
        assert!(s.contains("_cf_chl_answer"));
        assert!(s.contains("return _answer"));
    }

    #[test]
    fn none_always_unavailable() {
        assert!(matches!(
            JsInterpreter::None.eval(SIMPLE_EXPR, "example.com"),
            JsResult::Unavailable
        ));
    }

    #[test]
    fn node_eval_simple_arithmetic() {
        if !have("node") {
            return;
        }
        match JsInterpreter::Node.eval(SIMPLE_EXPR, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "42"),
            JsResult::Unavailable => panic!("node returned Unavailable"),
        }
    }

    #[test]
    fn node_eval_window_answer_extraction() {
        if !have("node") {
            return;
        }
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = 'node-answer';",
            "test.example.com",
            "{}",
            "{}",
        );
        match JsInterpreter::Node.eval(&full, "test.example.com") {
            JsResult::Ok(v) => assert_eq!(v, "node-answer"),
            JsResult::Unavailable => panic!("node eval failed"),
        }
    }

    #[test]
    fn node_eval_ctx_opt_accessible() {
        if !have("node") {
            return;
        }
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = window._cf_chl_ctx.cvId;",
            "example.com",
            r#"{"cvId":"test-cv-123"}"#,
            "{}",
        );
        match JsInterpreter::Node.eval(&full, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "test-cv-123"),
            JsResult::Unavailable => panic!("node eval failed"),
        }
    }

    #[test]
    fn node_eval_bad_script_returns_unavailable() {
        if !have("node") {
            return;
        }
        assert!(matches!(
            JsInterpreter::Node.eval("this is not javascript !!!", "example.com"),
            JsResult::Unavailable
        ));
    }

    #[test]
    fn node_eval_empty_result_is_unavailable() {
        if !have("node") {
            return;
        }
        assert!(matches!(
            JsInterpreter::Node.eval("(function(){})()", "example.com"),
            JsResult::Unavailable
        ));
    }

    #[test]
    fn bun_eval_simple_arithmetic() {
        if !have("bun") {
            return;
        }
        match JsInterpreter::Bun.eval(SIMPLE_EXPR, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "42"),
            JsResult::Unavailable => panic!("bun returned Unavailable"),
        }
    }

    #[test]
    fn bun_eval_window_answer_extraction() {
        if !have("bun") {
            return;
        }
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = 'bun-answer';",
            "test.example.com",
            "{}",
            "{}",
        );
        match JsInterpreter::Bun.eval(&full, "test.example.com") {
            JsResult::Ok(v) => assert_eq!(v, "bun-answer"),
            JsResult::Unavailable => panic!("bun eval failed"),
        }
    }

    #[cfg(feature = "js-boa")]
    #[test]
    fn boa_eval_simple_arithmetic() {
        match JsInterpreter::Boa.eval(SIMPLE_EXPR, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "42"),
            JsResult::Unavailable => panic!("boa returned Unavailable"),
        }
    }

    #[cfg(feature = "js-boa")]
    #[test]
    fn boa_eval_window_answer_extraction() {
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = 'boa-answer';",
            "test.example.com",
            "{}",
            "{}",
        );
        match JsInterpreter::Boa.eval(&full, "test.example.com") {
            JsResult::Ok(v) => assert_eq!(v, "boa-answer"),
            JsResult::Unavailable => panic!("boa eval failed"),
        }
    }

    #[cfg(feature = "js-boa")]
    #[test]
    fn boa_eval_ctx_opt_accessible() {
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = window._cf_chl_ctx.cvId;",
            "example.com",
            r#"{"cvId":"boa-cv-id"}"#,
            "{}",
        );
        match JsInterpreter::Boa.eval(&full, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "boa-cv-id"),
            JsResult::Unavailable => panic!("boa eval failed"),
        }
    }

    #[cfg(feature = "js-boa")]
    #[test]
    fn boa_eval_bad_script_returns_unavailable() {
        assert!(matches!(
            JsInterpreter::Boa.eval("throw new Error('boom');", "example.com"),
            JsResult::Unavailable
        ));
    }

    #[cfg(feature = "js-v8")]
    #[test]
    fn v8_eval_simple_arithmetic() {
        match JsInterpreter::V8.eval(SIMPLE_EXPR, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "42"),
            JsResult::Unavailable => panic!("v8 returned Unavailable"),
        }
    }

    #[cfg(feature = "js-v8")]
    #[test]
    fn v8_eval_window_answer_extraction() {
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = 'v8-answer';",
            "test.example.com",
            "{}",
            "{}",
        );
        match JsInterpreter::V8.eval(&full, "test.example.com") {
            JsResult::Ok(v) => assert_eq!(v, "v8-answer"),
            JsResult::Unavailable => panic!("v8 eval failed"),
        }
    }

    #[cfg(feature = "js-v8")]
    #[test]
    fn v8_eval_ctx_opt_accessible() {
        let full = JsInterpreter::build_vm_script(
            "window._cf_chl_answer = window._cf_chl_ctx.cvId;",
            "example.com",
            r#"{"cvId":"v8-cv-id"}"#,
            "{}",
        );
        match JsInterpreter::V8.eval(&full, "example.com") {
            JsResult::Ok(v) => assert_eq!(v, "v8-cv-id"),
            JsResult::Unavailable => panic!("v8 eval failed"),
        }
    }

    #[test]
    fn auto_returns_ok_when_any_engine_works() {
        let r = JsInterpreter::Auto.eval(SIMPLE_EXPR, "example.com");
        match r {
            JsResult::Ok(v) => assert_eq!(v, "42"),
            JsResult::Unavailable => {
                eprintln!("auto: no JS engine available in this environment");
            }
        }
    }

    #[test]
    fn node_wrap_base64_roundtrips_unicode() {
        if !have("node") {
            return;
        }
        match JsInterpreter::Node.eval(
            "(function(){ return String('\u{1F600}'); })()",
            "example.com",
        ) {
            JsResult::Ok(v) => assert_eq!(v, "\u{1F600}"),
            JsResult::Unavailable => panic!("node unicode roundtrip failed"),
        }
    }

    #[test]
    fn which_finds_sh() {
        assert!(which("sh").is_some());
    }

    #[test]
    fn which_returns_none_for_nonexistent() {
        assert!(which("__ghostwire_no_such_binary__").is_none());
    }
}

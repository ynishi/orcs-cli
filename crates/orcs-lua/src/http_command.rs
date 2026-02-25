//! Lua-exposed HTTP client for `orcs.http()`.
//!
//! Provides a blocking HTTP client (via `reqwest`) exposed to Lua as
//! `orcs.http(method, url, opts)`. Gated by `Capability::HTTP`.
//!
//! # Design
//!
//! Rust owns the transport layer (TLS, timeout, error classification).
//! Lua owns the application logic (request construction, response parsing).
//! Async reqwest is bridged into sync Lua context via
//! `tokio::task::block_in_place(|| handle.block_on(...))`.
//!
//! ```text
//! Lua: orcs.http("POST", url, { headers={...}, body="...", timeout=30 })
//!   → Capability::HTTP gate (ctx_fns / child)
//!   → http_request_impl (Rust/reqwest)
//!   → { ok, status, headers, body, error, error_kind }
//! ```

use mlua::{Lua, Table};
use std::time::Duration;

use crate::llm_command::retry::{
    classify_reqwest_error, read_body_limited, truncate_for_error, ReadBodyError,
};

/// Default timeout in seconds for HTTP requests.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Maximum response body size (10 MiB).
const MAX_BODY_SIZE: u64 = 10 * 1024 * 1024;

/// Registers `orcs.http` as a deny-by-default stub.
///
/// The real implementation is injected by `ctx_fns.rs` / `child.rs`
/// when a `ChildContext` with `Capability::HTTP` is available.
pub fn register_http_deny_stub(lua: &Lua, orcs_table: &Table) -> Result<(), mlua::Error> {
    if orcs_table.get::<mlua::Function>("http").is_err() {
        let http_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set(
                "error",
                "http denied: no execution context (ChildContext with Capability::HTTP required)",
            )?;
            result.set("error_kind", "permission_denied")?;
            Ok(result)
        })?;
        orcs_table.set("http", http_fn)?;
    }
    Ok(())
}

/// Executes an HTTP request using reqwest. Called from capability-gated context.
///
/// # Arguments (from Lua)
///
/// * `method` - HTTP method: "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD"
/// * `url` - Request URL (must be http:// or https://)
/// * `opts` - Optional table:
///   - `headers` - Table of {name = value} pairs
///   - `body` - Request body string
///   - `timeout` - Timeout in seconds (default: 30)
///
/// # Returns (Lua table)
///
/// * `ok` - boolean, true if HTTP response received (even 4xx/5xx)
/// * `status` - HTTP status code (number)
/// * `headers` - Response headers as {name = value} table
/// * `body` - Response body as string
/// * `error` - Error message (only when ok=false)
/// * `error_kind` - Error classification: "timeout", "dns", "connection_refused",
///   "tls", "too_large", "invalid_url", "network", "unknown"
pub fn http_request_impl(lua: &Lua, args: (String, String, Option<Table>)) -> mlua::Result<Table> {
    let (method, url, opts) = args;

    // Validate URL scheme
    if !url.starts_with("http://") && !url.starts_with("https://") {
        let result = lua.create_table()?;
        result.set("ok", false)?;
        result.set(
            "error",
            format!(
                "invalid URL scheme: URL must start with http:// or https://, got: {}",
                truncate_for_error(&url, 100)
            ),
        )?;
        result.set("error_kind", "invalid_url")?;
        return Ok(result);
    }

    // Parse options
    let timeout_secs = opts
        .as_ref()
        .and_then(|o| o.get::<u64>("timeout").ok())
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let body: Option<String> = opts.as_ref().and_then(|o| o.get::<String>("body").ok());

    // Collect headers from opts
    let mut extra_headers: Vec<(String, String)> = Vec::new();
    if let Some(ref o) = opts {
        if let Ok(headers) = o.get::<Table>("headers") {
            for (name, value) in headers.pairs::<String, String>().flatten() {
                extra_headers.push((name, value));
            }
        }
    }

    // Check if Content-Type is explicitly set
    let has_content_type = extra_headers
        .iter()
        .any(|(k, _)| k.to_lowercase() == "content-type");

    // Validate method
    let method_upper = method.to_uppercase();
    let reqwest_method = match method_upper.as_str() {
        "GET" => reqwest::Method::GET,
        "POST" => reqwest::Method::POST,
        "PUT" => reqwest::Method::PUT,
        "DELETE" => reqwest::Method::DELETE,
        "PATCH" => reqwest::Method::PATCH,
        "HEAD" => reqwest::Method::HEAD,
        _ => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", format!("unsupported HTTP method: {method_upper}"))?;
            result.set("error_kind", "invalid_method")?;
            return Ok(result);
        }
    };

    // Get shared client (reused across all HTTP requests within this Lua VM)
    let client = crate::llm_command::get_or_init_http_client(lua)?;

    // Get tokio runtime handle for async→sync bridge
    let handle = tokio::runtime::Handle::try_current().map_err(|_| {
        mlua::Error::RuntimeError("no tokio runtime available for async HTTP".into())
    })?;

    // Build request with per-request timeout
    let mut req = client
        .request(reqwest_method, &url)
        .timeout(Duration::from_secs(timeout_secs));
    for (name, value) in &extra_headers {
        req = req.header(name.as_str(), value.as_str());
    }

    // Default Content-Type to application/json when body is present
    if !has_content_type && body.is_some() {
        req = req.header("Content-Type", "application/json");
    }

    if let Some(ref body_str) = body {
        req = req.body(body_str.clone());
    }

    // Execute request via async→sync bridge (block_in_place allows nesting in multi-thread runtime)
    match tokio::task::block_in_place(|| handle.block_on(req.send())) {
        Ok(resp) => build_success_response(lua, resp),
        Err(e) => build_error_response(lua, e),
    }
}

/// Builds a Lua table from a successful reqwest response.
///
/// Uses [`read_body_limited`] to enforce `MAX_BODY_SIZE` during streaming,
/// preventing unbounded memory allocation from oversized responses.
fn build_success_response(lua: &Lua, resp: reqwest::Response) -> mlua::Result<Table> {
    let status = resp.status().as_u16();

    // Collect response headers before consuming the body
    let headers_table = lua.create_table()?;
    for (name, value) in resp.headers() {
        if let Ok(v) = value.to_str() {
            headers_table.set(name.as_str(), v)?;
        }
    }

    // Read body with streaming size limit
    let result = lua.create_table()?;
    result.set("ok", true)?;
    result.set("status", status)?;
    result.set("headers", headers_table)?;

    match read_body_limited(resp, MAX_BODY_SIZE) {
        Ok(body_str) => {
            result.set("body", body_str)?;
        }
        Err(ReadBodyError::TooLarge) => {
            result.set("body", "")?;
            result.set("error", "response body exceeds size limit")?;
            result.set("error_kind", "too_large")?;
        }
        Err(ReadBodyError::InvalidUtf8) => {
            result.set("body", "")?;
            result.set("error", "response body is not valid UTF-8")?;
            result.set("error_kind", "network")?;
        }
        Err(ReadBodyError::NoRuntime) => {
            result.set("body", "")?;
            result.set("error", "no tokio runtime available for reading body")?;
            result.set("error_kind", "network")?;
        }
        Err(ReadBodyError::Network(msg)) => {
            result.set("body", "")?;
            result.set("error", format!("failed to read response body: {msg}"))?;
            result.set("error_kind", "network")?;
        }
    }

    Ok(result)
}

/// Builds a Lua error table from a reqwest error.
fn build_error_response(lua: &Lua, error: reqwest::Error) -> mlua::Result<Table> {
    let (error_kind, error_msg) = classify_reqwest_error(&error);

    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", error_msg)?;
    result.set("error_kind", error_kind)?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::ensure_orcs_table;

    #[test]
    fn deny_stub_returns_permission_denied() {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");
        register_http_deny_stub(&lua, &orcs).expect("register stub");

        let result: Table = lua
            .load(r#"return orcs.http("GET", "http://example.com")"#)
            .eval()
            .expect("should return deny table");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error: String = result.get("error").expect("get error");
        assert!(
            error.contains("http denied"),
            "expected permission denied, got: {error}"
        );
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "permission_denied"
        );
    }

    #[test]
    fn invalid_url_scheme_returns_error() {
        let lua = Lua::new();
        let result = http_request_impl(&lua, ("GET".into(), "ftp://example.com".into(), None))
            .expect("should not panic");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "invalid_url"
        );
    }

    #[test]
    fn unsupported_method_returns_error() {
        let lua = Lua::new();
        let result = http_request_impl(&lua, ("CONNECT".into(), "http://localhost".into(), None))
            .expect("should not panic");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "invalid_method"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connection_refused_returns_error_kind() {
        let lua = Lua::new();
        // Port 1 is very unlikely to be open
        let opts = lua.create_table().expect("create opts");
        opts.set("timeout", 2).expect("set timeout");

        let result = http_request_impl(
            &lua,
            ("GET".into(), "http://127.0.0.1:1/test".into(), Some(opts)),
        )
        .expect("should not panic");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error_kind: String = result.get("error_kind").expect("get error_kind");
        assert!(
            error_kind == "connection_refused"
                || error_kind == "network"
                || error_kind == "timeout",
            "expected connection error kind, got: {error_kind}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn dns_failure_returns_error_kind() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("timeout", 3).expect("set timeout");

        let result = http_request_impl(
            &lua,
            (
                "GET".into(),
                "http://this-domain-does-not-exist-12345.invalid/test".into(),
                Some(opts),
            ),
        )
        .expect("should not panic");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error_kind: String = result.get("error_kind").expect("get error_kind");
        // DNS resolution may fail differently on different systems
        assert!(
            error_kind == "dns" || error_kind == "network" || error_kind == "timeout",
            "expected dns/network error kind, got: {error_kind}"
        );
    }

    #[test]
    fn truncate_for_error_handles_ascii() {
        assert_eq!(truncate_for_error("hello", 10), "hello");
        assert_eq!(truncate_for_error("hello world", 5), "hello");
    }

    #[test]
    fn truncate_for_error_handles_utf8() {
        // "あいう" is 9 bytes (3 chars × 3 bytes)
        let s = "あいう";
        let t = truncate_for_error(s, 4);
        assert_eq!(t, "あ"); // 3 bytes, not 4 (boundary)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn opts_timeout_is_respected() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("timeout", 1).expect("set timeout");

        // This will attempt to connect to a non-routable IP, should timeout quickly
        let start = std::time::Instant::now();
        let result = http_request_impl(
            &lua,
            (
                "GET".into(),
                "http://192.0.2.1/test".into(), // TEST-NET, non-routable
                Some(opts),
            ),
        )
        .expect("should not panic");

        let elapsed = start.elapsed();
        assert!(!result.get::<bool>("ok").expect("get ok"));
        // Should timeout within ~3 seconds (1s timeout + overhead)
        assert!(
            elapsed.as_secs() < 5,
            "should timeout quickly, took: {:?}",
            elapsed
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn headers_are_passed_through() {
        // This test verifies the code path that sets headers.
        // We can't test actual HTTP without a server, but we can verify
        // the opts parsing doesn't crash.
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        let headers = lua.create_table().expect("create headers");
        headers
            .set("Authorization", "Bearer test-token")
            .expect("set auth");
        headers.set("X-Custom", "custom-value").expect("set custom");
        opts.set("headers", headers).expect("set headers");
        opts.set("timeout", 1).expect("set timeout");

        // Will fail to connect but shouldn't panic on header processing
        let result = http_request_impl(
            &lua,
            ("POST".into(), "http://127.0.0.1:1/test".into(), Some(opts)),
        )
        .expect("should not panic on header processing");

        assert!(!result.get::<bool>("ok").expect("get ok"));
    }
}

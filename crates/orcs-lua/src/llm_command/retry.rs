//! HTTP retry logic and error classification for LLM requests.
//!
//! Uses `reqwest` for both OpenAI-compatible and Anthropic providers.
//! Retry policy: exponential backoff for 429/5xx and transient transport errors.

use std::time::Duration;

use super::provider::Provider;
use super::{LlmOpts, RETRY_BASE_DELAY_MS, RETRY_MAX_DELAY_SECS};

// ── HTTP Send ─────────────────────────────────────────────────────────

/// Error type for send_with_retry.
pub(super) enum SendError {
    Transport(reqwest::Error),
    /// Non-retryable error with pre-classified kind and message.
    Classified {
        kind: &'static str,
        message: String,
    },
}

/// Send an HTTP POST with retry logic. Returns the raw response on success.
///
/// Bridges async reqwest into the sync Lua call context via `tokio::runtime::Handle`.
pub(super) fn send_with_retry(
    client: &reqwest::Client,
    url: &str,
    opts: &LlmOpts,
    body_str: &str,
) -> Result<reqwest::Response, SendError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| SendError::Classified {
        kind: "runtime",
        message: "no tokio runtime available for async HTTP".to_string(),
    })?;

    // Pre-allocate body String outside the retry loop to avoid repeated conversion.
    let body_owned = body_str.to_string();

    for attempt in 0..=opts.max_retries {
        let mut req = client
            .post(url)
            .timeout(Duration::from_secs(opts.timeout))
            .header("Content-Type", "application/json");

        match opts.provider {
            Provider::OpenAICompat => {
                if let Some(ref key) = opts.api_key {
                    req = req.header("Authorization", format!("Bearer {}", key));
                }
            }
            Provider::Anthropic => {
                if let Some(ref key) = opts.api_key {
                    req = req.header("x-api-key", key.as_str());
                }
                req = req.header("anthropic-version", "2023-06-01");
            }
        }

        let result =
            tokio::task::block_in_place(|| handle.block_on(req.body(body_owned.clone()).send()));

        match result {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if attempt < opts.max_retries && should_retry_status(status) {
                    let delay = compute_retry_delay_from_headers(&resp, attempt);
                    tracing::debug!(
                        "LLM HTTP {status}, retry {}/{} after {delay:?}",
                        attempt + 1,
                        opts.max_retries
                    );
                    drop(resp);
                    std::thread::sleep(delay);
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if attempt < opts.max_retries && is_retryable_transport(&e) {
                    let delay = exponential_backoff(attempt);
                    tracing::debug!(
                        "LLM transport error, retry {}/{} after {delay:?}",
                        attempt + 1,
                        opts.max_retries
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                return Err(SendError::Transport(e));
            }
        }
    }
    // The loop above always returns on the final iteration (attempt == max_retries),
    // but the compiler cannot prove this statically. Provide a safe fallback.
    Err(SendError::Classified {
        kind: "network",
        message: "retry loop exhausted without returning".to_string(),
    })
}

// ── Error Helpers ──────────────────────────────────────────────────────

/// Build an error Lua table from a reqwest error.
pub(super) fn build_error_result(
    lua: &mlua::Lua,
    error: reqwest::Error,
    session_id: &str,
) -> mlua::Result<mlua::Table> {
    let (error_kind, error_msg) = classify_reqwest_error(&error);

    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", error_msg)?;
    result.set("error_kind", error_kind)?;
    result.set("session_id", session_id)?;
    Ok(result)
}

/// Build an error Lua table from a pre-classified SendError.
pub(super) fn build_classified_error_result(
    lua: &mlua::Lua,
    kind: &str,
    message: &str,
    session_id: &str,
) -> mlua::Result<mlua::Table> {
    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", message)?;
    result.set("error_kind", kind)?;
    result.set("session_id", session_id)?;
    Ok(result)
}

/// Classify a reqwest error into kind + message.
pub(crate) fn classify_reqwest_error(error: &reqwest::Error) -> (&'static str, String) {
    let msg = error.to_string();

    if error.is_timeout() {
        return ("timeout", msg);
    }
    if error.is_connect() {
        // Heuristic sub-classification of connection errors
        let lower = msg.to_lowercase();
        if lower.contains("connection refused") {
            return ("connection_refused", msg);
        }
        if lower.contains("dns") || lower.contains("resolve") || lower.contains("name resolution") {
            return ("dns", msg);
        }
        if lower.contains("reset") {
            return ("connection_reset", msg);
        }
        return ("network", msg);
    }
    if error.is_request() {
        let lower = msg.to_lowercase();
        if lower.contains("tls") || lower.contains("ssl") || lower.contains("certificate") {
            return ("tls", msg);
        }
    }
    if error.is_decode() {
        return ("parse_error", msg);
    }

    ("network", msg)
}

/// Classify HTTP status code into an error kind string.
pub(super) fn classify_http_status(status: u16) -> &'static str {
    match status {
        401 => "auth_error",
        403 => "auth_error",
        404 => "not_found",
        429 => "rate_limit",
        500..=599 => "server_error",
        _ => "http_error",
    }
}

/// Truncate a string for safe inclusion in error messages.
pub(crate) fn truncate_for_error(s: &str, max: usize) -> &str {
    if s.len() <= max {
        s
    } else {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Read a response body as a UTF-8 string with a size limit.
///
/// Unlike `resp.text()`, this stops reading once `max_bytes` is reached,
/// preventing unbounded memory allocation from oversized responses.
///
/// Bridges async reqwest into the sync context via `block_in_place`.
pub(crate) fn read_body_limited(
    resp: reqwest::Response,
    max_bytes: u64,
) -> Result<String, ReadBodyError> {
    let handle = tokio::runtime::Handle::try_current().map_err(|_| ReadBodyError::NoRuntime)?;

    let bytes = tokio::task::block_in_place(|| {
        handle.block_on(async {
            let mut resp = resp;
            let mut buf: Vec<u8> = Vec::new();

            while let Some(chunk) = resp
                .chunk()
                .await
                .map_err(|e| ReadBodyError::Network(e.to_string()))?
            {
                if (buf.len() + chunk.len()) as u64 > max_bytes {
                    return Err(ReadBodyError::TooLarge);
                }
                buf.extend_from_slice(&chunk);
            }

            String::from_utf8(buf).map_err(|_| ReadBodyError::InvalidUtf8)
        })
    })?;

    Ok(bytes)
}

/// Errors from [`read_body_limited`].
#[derive(Debug)]
pub(crate) enum ReadBodyError {
    /// No tokio runtime available.
    NoRuntime,
    /// Response body exceeded the size limit.
    TooLarge,
    /// Response body is not valid UTF-8.
    InvalidUtf8,
    /// Network/transport error while reading.
    Network(String),
}

// ── Retry Helpers ─────────────────────────────────────────────────────

/// Returns true if the HTTP status warrants a retry.
fn should_retry_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Compute retry delay from response headers or exponential backoff.
///
/// For 429 responses, respects `Retry-After` header (integer seconds, capped).
/// Otherwise falls back to exponential backoff.
fn compute_retry_delay_from_headers(resp: &reqwest::Response, attempt: u32) -> Duration {
    if let Some(val) = resp.headers().get("retry-after") {
        if let Ok(s) = val.to_str() {
            if let Ok(secs) = s.trim().parse::<u64>() {
                let capped = secs.min(RETRY_MAX_DELAY_SECS);
                return Duration::from_secs(capped);
            }
        }
    }
    exponential_backoff(attempt)
}

/// Exponential backoff: 1s, 2s, 4s, ... capped at RETRY_MAX_DELAY_SECS.
fn exponential_backoff(attempt: u32) -> Duration {
    let delay_ms = RETRY_BASE_DELAY_MS.saturating_mul(2u64.saturating_pow(attempt));
    let cap_ms = RETRY_MAX_DELAY_SECS.saturating_mul(1000);
    Duration::from_millis(delay_ms.min(cap_ms))
}

/// Returns true if a transport error is worth retrying.
///
/// Retries: timeout, connection reset.
/// Does NOT retry: connection refused (server not running), DNS, TLS.
fn is_retryable_transport(error: &reqwest::Error) -> bool {
    if error.is_timeout() {
        return true;
    }
    if error.is_connect() {
        let msg = error.to_string().to_lowercase();
        return msg.contains("reset");
    }
    false
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_http_status_categories() {
        assert_eq!(classify_http_status(401), "auth_error");
        assert_eq!(classify_http_status(403), "auth_error");
        assert_eq!(classify_http_status(404), "not_found");
        assert_eq!(classify_http_status(429), "rate_limit");
        assert_eq!(classify_http_status(500), "server_error");
        assert_eq!(classify_http_status(503), "server_error");
        assert_eq!(classify_http_status(400), "http_error");
    }

    #[test]
    fn truncate_for_error_ascii() {
        assert_eq!(truncate_for_error("hello", 10), "hello");
        assert_eq!(truncate_for_error("hello world", 5), "hello");
    }

    #[test]
    fn truncate_for_error_utf8() {
        let s = "あいう"; // 9 bytes
        let t = truncate_for_error(s, 4);
        assert_eq!(t, "あ"); // 3 bytes boundary
    }

    #[test]
    fn should_retry_status_429() {
        assert!(should_retry_status(429));
    }

    #[test]
    fn should_retry_status_5xx() {
        assert!(should_retry_status(500));
        assert!(should_retry_status(502));
        assert!(should_retry_status(503));
        assert!(should_retry_status(599));
    }

    #[test]
    fn should_not_retry_status_2xx_4xx() {
        assert!(!should_retry_status(200));
        assert!(!should_retry_status(201));
        assert!(!should_retry_status(400));
        assert!(!should_retry_status(401));
        assert!(!should_retry_status(404));
    }

    #[test]
    fn exponential_backoff_progression() {
        let d0 = exponential_backoff(0);
        let d1 = exponential_backoff(1);
        let d2 = exponential_backoff(2);

        assert_eq!(d0, Duration::from_millis(RETRY_BASE_DELAY_MS));
        assert_eq!(d1, Duration::from_millis(RETRY_BASE_DELAY_MS * 2));
        assert_eq!(d2, Duration::from_millis(RETRY_BASE_DELAY_MS * 4));
    }

    #[test]
    fn exponential_backoff_capped() {
        // Very high attempt should be capped
        let d = exponential_backoff(20);
        assert!(
            d <= Duration::from_secs(RETRY_MAX_DELAY_SECS),
            "backoff should be capped, got: {:?}",
            d
        );
    }
}

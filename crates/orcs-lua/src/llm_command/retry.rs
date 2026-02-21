use std::time::Duration;

use super::provider::Provider;
use super::{LlmOpts, RETRY_BASE_DELAY_MS, RETRY_MAX_DELAY_SECS};

// ── HTTP Send ─────────────────────────────────────────────────────────

/// Error type for send_with_retry.
pub(super) enum SendError {
    Transport(ureq::Error),
}

/// Send an HTTP request with retry logic. Returns the raw response on success.
pub(super) fn send_with_retry(
    agent: &ureq::Agent,
    url: &str,
    opts: &LlmOpts,
    body_str: &str,
) -> Result<ureq::http::Response<ureq::Body>, SendError> {
    for attempt in 0..=opts.max_retries {
        let mut req = agent.post(url);
        req = req.header("Content-Type", "application/json");

        match opts.provider {
            Provider::Ollama => {}
            Provider::OpenAI => {
                if let Some(ref key) = opts.api_key {
                    req = req.header("Authorization", &format!("Bearer {}", key));
                }
            }
            Provider::Anthropic => {
                if let Some(ref key) = opts.api_key {
                    req = req.header("x-api-key", key);
                }
                req = req.header("anthropic-version", "2023-06-01");
            }
        }

        match req.send(body_str.as_bytes()) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if attempt < opts.max_retries && should_retry_status(status) {
                    let delay = compute_retry_delay(&resp, attempt);
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
    // Unreachable: loop always returns
    unreachable!("retry loop exhausted without returning")
}

// ── Error Helpers ──────────────────────────────────────────────────────

/// Build an error Lua table from a ureq error.
pub(super) fn build_error_result(
    lua: &mlua::Lua,
    error: ureq::Error,
    session_id: &str,
) -> mlua::Result<mlua::Table> {
    let (error_kind, error_msg) = classify_ureq_error(&error);

    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", error_msg)?;
    result.set("error_kind", error_kind)?;
    result.set("session_id", session_id)?;
    Ok(result)
}

/// Classify a ureq error into kind + message.
pub(super) fn classify_ureq_error(error: &ureq::Error) -> (&'static str, String) {
    let msg = error.to_string();

    // Walk error chain for IO errors
    let io_err = {
        let mut source: Option<&dyn std::error::Error> = Some(error);
        let mut found = None;
        while let Some(err) = source {
            if let Some(io) = err.downcast_ref::<std::io::Error>() {
                found = Some(io);
                break;
            }
            source = err.source();
        }
        found
    };

    if let Some(io) = io_err {
        match io.kind() {
            std::io::ErrorKind::TimedOut => return ("timeout", msg),
            std::io::ErrorKind::ConnectionRefused => return ("connection_refused", msg),
            std::io::ErrorKind::ConnectionReset => return ("connection_reset", msg),
            _ => {}
        }
    }

    // String-based heuristics
    let lower = msg.to_lowercase();
    if lower.contains("timeout") || lower.contains("timed out") {
        ("timeout", msg)
    } else if lower.contains("dns")
        || lower.contains("resolve")
        || lower.contains("name resolution")
    {
        ("dns", msg)
    } else if lower.contains("connection refused") {
        ("connection_refused", msg)
    } else if lower.contains("tls") || lower.contains("ssl") || lower.contains("certificate") {
        ("tls", msg)
    } else {
        ("network", msg)
    }
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
pub(super) fn truncate_for_error(s: &str, max: usize) -> &str {
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

// ── Retry Helpers ─────────────────────────────────────────────────────

/// Returns true if the HTTP status warrants a retry.
fn should_retry_status(status: u16) -> bool {
    status == 429 || (500..600).contains(&status)
}

/// Compute retry delay from response headers or exponential backoff.
///
/// For 429 responses, respects `Retry-After` header (integer seconds, capped).
/// Otherwise falls back to exponential backoff.
fn compute_retry_delay(resp: &ureq::http::Response<ureq::Body>, attempt: u32) -> Duration {
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
fn is_retryable_transport(error: &ureq::Error) -> bool {
    let (kind, _) = classify_ureq_error(error);
    matches!(kind, "timeout" | "connection_reset")
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

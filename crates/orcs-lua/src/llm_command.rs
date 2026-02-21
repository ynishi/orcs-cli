//! Multi-provider LLM client for `orcs.llm()`.
//!
//! Direct HTTP implementation (via `ureq`) supporting Ollama, OpenAI, and Anthropic.
//! Replaces the previous Claude CLI handler with a provider-agnostic blocking HTTP client.
//!
//! # Architecture
//!
//! ```text
//! Lua: orcs.llm(prompt, opts)
//!   → Capability::LLM gate (ctx_fns / child)
//!   → llm_request_impl (Rust/ureq)
//!       ├── Ollama:    POST {base_url}/api/chat
//!       ├── OpenAI:    POST {base_url}/v1/chat/completions
//!       └── Anthropic: POST {base_url}/v1/messages
//! ```
//!
//! # Session Management
//!
//! Conversation history is stored in-memory per Lua VM via `SessionStore` (Lua app_data).
//! - `session_id = nil` → create new session (UUID v4), return session_id in response
//! - `session_id = "existing-id"` → append to existing history and continue
//!
//! # Rate Limiting & Retry
//!
//! Automatic retry with exponential backoff for transient errors:
//! - HTTP 429: respects `Retry-After` header, falls back to exponential backoff
//! - HTTP 5xx: exponential backoff (1s, 2s, 4s, capped at 30s)
//! - Transport errors (timeout, connection reset): exponential backoff
//! - Default: 2 retries (3 total attempts), configurable via `opts.max_retries`
//!
//! # Session Persistence
//!
//! - `orcs.llm_dump_sessions()` → JSON string of all session histories
//! - `orcs.llm_load_sessions(json)` → restore sessions from JSON
//!
//! # Technical Debt
//!
//! - Streaming not supported (`stream: false` fixed)
//! - Tool use / function calling not supported

use mlua::{Lua, Table};
use orcs_types::intent::MessageContent;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

/// Default timeout in seconds for LLM requests.
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Default max_tokens for Anthropic (required field).
const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;

/// Maximum response body size (10 MiB).
const MAX_BODY_SIZE: u64 = 10 * 1024 * 1024;

/// Default number of retries for transient errors (429, 5xx).
const DEFAULT_MAX_RETRIES: u32 = 2;

/// Base delay for exponential backoff (milliseconds).
const RETRY_BASE_DELAY_MS: u64 = 1000;

/// Maximum delay between retries (seconds).
const RETRY_MAX_DELAY_SECS: u64 = 30;

// ── Provider ───────────────────────────────────────────────────────────

/// Supported LLM providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provider {
    Ollama,
    OpenAI,
    Anthropic,
}

impl Provider {
    /// Parse provider from string (case-insensitive).
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_lowercase().as_str() {
            "ollama" => Ok(Self::Ollama),
            "openai" => Ok(Self::OpenAI),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "unsupported provider: '{}' (expected: ollama, openai, anthropic)",
                other
            )),
        }
    }

    /// Default base URL for this provider.
    fn default_base_url(self) -> &'static str {
        match self {
            Self::Ollama => "http://localhost:11434",
            Self::OpenAI => "https://api.openai.com",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Default model for this provider.
    fn default_model(self) -> &'static str {
        match self {
            Self::Ollama => "llama3.2",
            Self::OpenAI => "gpt-4o",
            Self::Anthropic => "claude-sonnet-4-20250514",
        }
    }

    /// Chat API path for this provider.
    fn chat_path(self) -> &'static str {
        match self {
            Self::Ollama => "/api/chat",
            Self::OpenAI => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }

    /// Environment variable name for the API key.
    fn api_key_env(self) -> Option<&'static str> {
        match self {
            Self::Ollama => None,
            Self::OpenAI => Some("OPENAI_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
        }
    }

    /// Health-check path for this provider.
    ///
    /// - Ollama: `GET /` returns `"Ollama is running"` (no auth)
    /// - OpenAI: `GET /v1/models` (requires auth, but proves connectivity)
    /// - Anthropic: no dedicated health endpoint; we probe the chat path
    ///   expecting a non-timeout response (e.g. 401/405) to prove connectivity
    fn health_path(self) -> &'static str {
        match self {
            Self::Ollama => "/",
            Self::OpenAI => "/v1/models",
            Self::Anthropic => "/v1/messages",
        }
    }
}

// ── Session Store ──────────────────────────────────────────────────────

/// A single message in the conversation history.
///
/// Supports both plain text and structured content blocks (tool_use, tool_result).
/// Backward-compatible: `MessageContent::Text(s)` serializes as a plain string.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    role: String,
    content: MessageContent,
}

/// In-memory conversation store, keyed by session_id.
/// Stored in Lua app_data for per-VM isolation.
struct SessionStore(HashMap<String, Vec<Message>>);

impl SessionStore {
    fn new() -> Self {
        Self(HashMap::new())
    }
}

// ── Parsed Options ─────────────────────────────────────────────────────

/// Parsed and validated options from the Lua opts table.
#[derive(Debug)]
struct LlmOpts {
    provider: Provider,
    base_url: String,
    model: String,
    api_key: Option<String>,
    system_prompt: Option<String>,
    session_id: Option<String>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    timeout: u64,
    max_retries: u32,
}

impl LlmOpts {
    /// Parse from Lua opts table. Missing fields use provider defaults.
    fn from_lua(opts: Option<&Table>) -> Result<Self, String> {
        let provider_str = opts
            .and_then(|o| o.get::<String>("provider").ok())
            .unwrap_or_else(|| "ollama".to_string());
        let provider = Provider::from_str(&provider_str)?;

        // base_url resolution: opts.base_url > ORCS_LLM_BASE_URL env > provider default
        let base_url = opts
            .and_then(|o| o.get::<String>("base_url").ok())
            .or_else(|| std::env::var("ORCS_LLM_BASE_URL").ok())
            .unwrap_or_else(|| provider.default_base_url().to_string());

        let model = opts
            .and_then(|o| o.get::<String>("model").ok())
            .unwrap_or_else(|| provider.default_model().to_string());

        // API key resolution: opts.api_key > env var > None
        let api_key = opts
            .and_then(|o| o.get::<String>("api_key").ok())
            .or_else(|| {
                provider
                    .api_key_env()
                    .and_then(|env_name| std::env::var(env_name).ok())
            });

        let system_prompt = opts.and_then(|o| o.get::<String>("system_prompt").ok());
        let session_id = opts.and_then(|o| o.get::<String>("session_id").ok());
        let temperature = opts.and_then(|o| o.get::<f64>("temperature").ok());
        let max_tokens = opts.and_then(|o| o.get::<u64>("max_tokens").ok());

        let timeout = opts
            .and_then(|o| o.get::<u64>("timeout").ok())
            .unwrap_or(DEFAULT_TIMEOUT_SECS);

        let max_retries = opts
            .and_then(|o| o.get::<u32>("max_retries").ok())
            .unwrap_or(DEFAULT_MAX_RETRIES);

        Ok(Self {
            provider,
            base_url,
            model,
            api_key,
            system_prompt,
            session_id,
            temperature,
            max_tokens,
            timeout,
            max_retries,
        })
    }
}

// ── Ping Implementation ───────────────────────────────────────────────

/// Default timeout in seconds for health-check pings.
const PING_TIMEOUT_SECS: u64 = 5;

/// Executes a lightweight connectivity check against the LLM provider.
///
/// Sends a single HTTP GET to the provider's health endpoint and measures
/// round-trip latency. Does **not** consume tokens or create sessions.
///
/// # Arguments (from Lua)
///
/// * `opts` - Optional table:
///   - `provider`  - "ollama" (default), "openai", "anthropic"
///   - `base_url`  - Provider base URL (default per provider)
///   - `api_key`   - API key (falls back to env var)
///   - `timeout`   - Timeout in seconds (default: 5)
///
/// # Returns (Lua table)
///
/// * `ok`         - boolean (true if HTTP response received, even non-2xx)
/// * `provider`   - Provider name string
/// * `base_url`   - Resolved base URL
/// * `latency_ms` - Round-trip time in milliseconds
/// * `status`     - HTTP status code (when response received)
/// * `error`      - Error message (when ok=false)
/// * `error_kind` - Error classification (when ok=false)
pub fn llm_ping_impl(lua: &Lua, opts: Option<Table>) -> mlua::Result<Table> {
    // Parse provider/base_url/api_key from opts (reuse LlmOpts parsing logic)
    let provider_str = opts
        .as_ref()
        .and_then(|o| o.get::<String>("provider").ok())
        .unwrap_or_else(|| "ollama".to_string());
    let provider = match Provider::from_str(&provider_str) {
        Ok(p) => p,
        Err(e) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", e)?;
            result.set("error_kind", "invalid_options")?;
            return Ok(result);
        }
    };

    let base_url = opts
        .as_ref()
        .and_then(|o| o.get::<String>("base_url").ok())
        .or_else(|| std::env::var("ORCS_LLM_BASE_URL").ok())
        .unwrap_or_else(|| provider.default_base_url().to_string());

    let api_key = opts
        .as_ref()
        .and_then(|o| o.get::<String>("api_key").ok())
        .or_else(|| {
            provider
                .api_key_env()
                .and_then(|env_name| std::env::var(env_name).ok())
        });

    let timeout = opts
        .as_ref()
        .and_then(|o| o.get::<u64>("timeout").ok())
        .unwrap_or(PING_TIMEOUT_SECS);

    // Build URL
    let url = format!(
        "{}{}",
        base_url.trim_end_matches('/'),
        provider.health_path()
    );

    // Configure agent with short timeout
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    // Send GET and measure latency
    let start = std::time::Instant::now();

    let mut req = agent.get(&url);
    // Attach auth headers for providers that need them
    match provider {
        Provider::Ollama => {}
        Provider::OpenAI => {
            if let Some(ref key) = api_key {
                req = req.header("Authorization", &format!("Bearer {}", key));
            }
        }
        Provider::Anthropic => {
            if let Some(ref key) = api_key {
                req = req.header("x-api-key", key);
            }
            req = req.header("anthropic-version", "2023-06-01");
        }
    }

    let result = lua.create_table()?;
    result.set("provider", format!("{:?}", provider).to_lowercase())?;
    result.set("base_url", base_url.as_str())?;

    match req.call() {
        Ok(resp) => {
            let latency = start.elapsed();
            let status = resp.status().as_u16();
            result.set("ok", true)?;
            result.set("status", status)?;
            result.set("latency_ms", latency.as_millis() as u64)?;
        }
        Err(e) => {
            let latency = start.elapsed();
            result.set("latency_ms", latency.as_millis() as u64)?;

            // If we got an HTTP response (non-2xx), connectivity is confirmed
            if let ureq::Error::StatusCode(status) = &e {
                result.set("ok", true)?;
                result.set("status", *status)?;
            } else {
                let (error_kind, error_msg) = classify_ureq_error(&e);
                result.set("ok", false)?;
                result.set("error", error_msg)?;
                result.set("error_kind", error_kind)?;
            }
        }
    }

    Ok(result)
}

// ── Deny Stub ──────────────────────────────────────────────────────────

/// Registers `orcs.llm` as a deny-by-default stub.
///
/// The real implementation is injected by `ctx_fns.rs` / `child.rs`
/// when a `ChildContext` with `Capability::LLM` is available.
pub fn register_llm_deny_stub(lua: &Lua, orcs_table: &Table) -> Result<(), mlua::Error> {
    if orcs_table.get::<mlua::Function>("llm").is_err() {
        let llm_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set(
                "error",
                "llm denied: no execution context (ChildContext with Capability::LLM required)",
            )?;
            result.set("error_kind", "permission_denied")?;
            Ok(result)
        })?;
        orcs_table.set("llm", llm_fn)?;
    }

    // llm_ping deny stub
    if orcs_table.get::<mlua::Function>("llm_ping").is_err() {
        let ping_fn = lua.create_function(|lua, _args: mlua::MultiValue| {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set(
                "error",
                "llm_ping denied: no execution context (ChildContext with Capability::LLM required)",
            )?;
            result.set("error_kind", "permission_denied")?;
            Ok(result)
        })?;
        orcs_table.set("llm_ping", ping_fn)?;
    }

    // Session persistence: dump all sessions to JSON string
    let dump_fn = lua.create_function(|lua, ()| {
        ensure_session_store(lua);
        match lua.app_data_ref::<SessionStore>() {
            Some(store) => serde_json::to_string(&store.0)
                .map_err(|e| mlua::Error::RuntimeError(format!("session serialize error: {e}"))),
            None => Ok("{}".to_string()),
        }
    })?;
    orcs_table.set("llm_dump_sessions", dump_fn)?;

    // Session persistence: load sessions from JSON string
    let load_fn = lua.create_function(|lua, json_str: String| {
        let sessions: HashMap<String, Vec<Message>> = serde_json::from_str(&json_str)
            .map_err(|e| mlua::Error::RuntimeError(format!("session deserialize error: {e}")))?;
        let count = sessions.len();
        let _ = lua.remove_app_data::<SessionStore>();
        lua.set_app_data(SessionStore(sessions));

        let result = lua.create_table()?;
        result.set("ok", true)?;
        result.set("count", count)?;
        Ok(result)
    })?;
    orcs_table.set("llm_load_sessions", load_fn)?;

    Ok(())
}

// ── Request Implementation ─────────────────────────────────────────────

/// Executes an LLM chat request. Called from capability-gated context.
///
/// # Arguments (from Lua)
///
/// * `prompt` - User message text
/// * `opts` - Optional table:
///   - `provider` - "ollama" (default), "openai", "anthropic"
///   - `base_url` - Provider base URL (default per provider)
///   - `model` - Model name (default per provider)
///   - `api_key` - API key (falls back to env var)
///   - `system_prompt` - System prompt text
///   - `session_id` - Session ID for multi-turn (nil = new session)
///   - `temperature` - Sampling temperature
///   - `max_tokens` - Max completion tokens
///   - `timeout` - Request timeout in seconds (default: 120)
///
/// # Returns (Lua table)
///
/// * `ok` - boolean
/// * `content` - Response text (when ok=true)
/// * `model` - Model name from response
/// * `session_id` - Session ID (new or existing)
/// * `error` - Error message (when ok=false)
/// * `error_kind` - Error classification
pub fn llm_request_impl(lua: &Lua, args: (String, Option<Table>)) -> mlua::Result<Table> {
    let (prompt, opts) = args;

    // Parse options
    let llm_opts = match LlmOpts::from_lua(opts.as_ref()) {
        Ok(o) => o,
        Err(e) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", e)?;
            result.set("error_kind", "invalid_options")?;
            return Ok(result);
        }
    };

    // Validate API key requirement
    if llm_opts.provider != Provider::Ollama && llm_opts.api_key.is_none() {
        let env_name = llm_opts
            .provider
            .api_key_env()
            .unwrap_or("(unknown env var)");
        let result = lua.create_table()?;
        result.set("ok", false)?;
        result.set(
            "error",
            format!(
                "API key required for {:?}: set opts.api_key or {} environment variable",
                llm_opts.provider, env_name
            ),
        )?;
        result.set("error_kind", "missing_api_key")?;
        return Ok(result);
    }

    // Session management: get or create session
    let session_id = resolve_session_id(lua, &llm_opts.session_id);

    // Build messages from session history + current prompt
    let messages = build_messages(lua, &session_id, &prompt, &llm_opts);

    // Build request body
    let request_body = match build_request_body(&llm_opts, &messages) {
        Ok(body) => body,
        Err(e) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", e)?;
            result.set("error_kind", "request_build_error")?;
            return Ok(result);
        }
    };

    // Build URL
    let url = format!(
        "{}{}",
        llm_opts.base_url.trim_end_matches('/'),
        llm_opts.provider.chat_path()
    );

    // Configure ureq agent (reused across retries)
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(llm_opts.timeout)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let body_str = request_body.to_string();
    tracing::debug!(
        "llm request: {} {} ({}B)",
        llm_opts.provider.chat_path(),
        llm_opts.model,
        body_str.len()
    );

    // Execute with retry loop
    for attempt in 0..=llm_opts.max_retries {
        // Build request fresh each attempt (send() consumes self)
        let mut req = agent.post(&url);
        req = req.header("Content-Type", "application/json");

        match llm_opts.provider {
            Provider::Ollama => {}
            Provider::OpenAI => {
                if let Some(ref key) = llm_opts.api_key {
                    req = req.header("Authorization", &format!("Bearer {}", key));
                }
            }
            Provider::Anthropic => {
                if let Some(ref key) = llm_opts.api_key {
                    req = req.header("x-api-key", key);
                }
                req = req.header("anthropic-version", "2023-06-01");
            }
        }

        match req.send(body_str.as_bytes()) {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if attempt < llm_opts.max_retries && should_retry_status(status) {
                    let delay = compute_retry_delay(&resp, attempt);
                    tracing::debug!(
                        "LLM HTTP {status}, retry {}/{} after {delay:?}",
                        attempt + 1,
                        llm_opts.max_retries
                    );
                    drop(resp);
                    std::thread::sleep(delay);
                    continue;
                }
                return parse_response(lua, resp, &llm_opts, &session_id, &prompt);
            }
            Err(e) => {
                if attempt < llm_opts.max_retries && is_retryable_transport(&e) {
                    let delay = exponential_backoff(attempt);
                    tracing::debug!(
                        "LLM transport error, retry {}/{} after {delay:?}",
                        attempt + 1,
                        llm_opts.max_retries
                    );
                    std::thread::sleep(delay);
                    continue;
                }
                return build_error_result(lua, e, &session_id);
            }
        }
    }

    // Safety fallback (last iteration always returns above)
    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", "retry loop exhausted")?;
    result.set("error_kind", "internal")?;
    result.set("session_id", session_id)?;
    Ok(result)
}

// ── Session Helpers ────────────────────────────────────────────────────

/// Resolve or create a session ID. Returns the session_id string.
fn resolve_session_id(lua: &Lua, requested: &Option<String>) -> String {
    match requested {
        Some(id) if !id.is_empty() => {
            // Ensure store exists
            ensure_session_store(lua);
            id.clone()
        }
        _ => {
            // Generate new session ID
            let id = format!("sess-{}", uuid::Uuid::new_v4());
            ensure_session_store(lua);

            // Create empty history
            if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
                store.0.insert(id.clone(), Vec::new());
                lua.set_app_data(store);
            }
            id
        }
    }
}

/// Ensure SessionStore exists in app_data.
fn ensure_session_store(lua: &Lua) {
    if lua.app_data_ref::<SessionStore>().is_none() {
        lua.set_app_data(SessionStore::new());
    }
}

/// Build messages array from session history + current prompt.
fn build_messages(lua: &Lua, session_id: &str, prompt: &str, opts: &LlmOpts) -> Vec<Message> {
    let mut messages = Vec::new();

    // Get history from session store
    if let Some(store) = lua.app_data_ref::<SessionStore>() {
        if let Some(history) = store.0.get(session_id) {
            messages.extend(history.iter().cloned());
        }
    }

    // Add system prompt if this is the first message and system_prompt is set
    if messages.is_empty() {
        if let Some(ref sys) = opts.system_prompt {
            // For Anthropic, system goes at top level (handled in build_request_body).
            // For Ollama/OpenAI, system goes in messages.
            if opts.provider != Provider::Anthropic {
                messages.push(Message {
                    role: "system".to_string(),
                    content: MessageContent::Text(sys.clone()),
                });
            }
        }
    }

    // Add current user message
    messages.push(Message {
        role: "user".to_string(),
        content: MessageContent::Text(prompt.to_string()),
    });

    messages
}

/// Store assistant response and user message in session history.
fn update_session(lua: &Lua, session_id: &str, user_msg: &str, assistant_msg: &str) {
    if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
        let history = store.0.entry(session_id.to_string()).or_default();
        history.push(Message {
            role: "user".to_string(),
            content: MessageContent::Text(user_msg.to_string()),
        });
        history.push(Message {
            role: "assistant".to_string(),
            content: MessageContent::Text(assistant_msg.to_string()),
        });
        lua.set_app_data(store);
    }
}

// ── Request Body Builders ──────────────────────────────────────────────

/// Build the JSON request body for the given provider.
fn build_request_body(opts: &LlmOpts, messages: &[Message]) -> Result<serde_json::Value, String> {
    match opts.provider {
        Provider::Ollama => build_ollama_body(opts, messages),
        Provider::OpenAI => build_openai_body(opts, messages),
        Provider::Anthropic => build_anthropic_body(opts, messages),
    }
}

fn build_ollama_body(opts: &LlmOpts, messages: &[Message]) -> Result<serde_json::Value, String> {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let content_val = serde_json::to_value(&m.content).unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "role": m.role,
                "content": content_val,
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": opts.model,
        "messages": msgs,
        "stream": false,
    });

    // Ollama puts temperature and num_predict inside "options"
    let mut options = serde_json::Map::new();
    if let Some(temp) = opts.temperature {
        options.insert("temperature".into(), serde_json::json!(temp));
    }
    if let Some(max) = opts.max_tokens {
        options.insert("num_predict".into(), serde_json::json!(max));
    }
    if !options.is_empty() {
        body["options"] = serde_json::Value::Object(options);
    }

    Ok(body)
}

fn build_openai_body(opts: &LlmOpts, messages: &[Message]) -> Result<serde_json::Value, String> {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let content_val = serde_json::to_value(&m.content).unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "role": m.role,
                "content": content_val,
            })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": opts.model,
        "messages": msgs,
        "stream": false,
    });

    if let Some(temp) = opts.temperature {
        body["temperature"] = serde_json::json!(temp);
    }
    if let Some(max) = opts.max_tokens {
        body["max_tokens"] = serde_json::json!(max);
    }

    Ok(body)
}

fn build_anthropic_body(opts: &LlmOpts, messages: &[Message]) -> Result<serde_json::Value, String> {
    // Anthropic: system prompt is top-level, NOT in messages.
    // Filter out any system messages from the messages array.
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| {
            let content_val = serde_json::to_value(&m.content).unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "role": m.role,
                "content": content_val,
            })
        })
        .collect();

    // max_tokens is required for Anthropic
    let max_tokens = opts.max_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);

    let mut body = serde_json::json!({
        "model": opts.model,
        "messages": msgs,
        "stream": false,
        "max_tokens": max_tokens,
    });

    // System prompt at top level
    if let Some(ref sys) = opts.system_prompt {
        body["system"] = serde_json::json!(sys);
    }

    if let Some(temp) = opts.temperature {
        body["temperature"] = serde_json::json!(temp);
    }

    Ok(body)
}

// ── Response Parsers ───────────────────────────────────────────────────

/// Parse a successful HTTP response and extract the assistant's reply.
fn parse_response(
    lua: &Lua,
    mut resp: ureq::http::Response<ureq::Body>,
    opts: &LlmOpts,
    session_id: &str,
    user_prompt: &str,
) -> mlua::Result<Table> {
    let status = resp.status().as_u16();

    // Read body
    let body_text = {
        use std::io::Read;
        let mut buf = Vec::new();
        let reader = resp.body_mut().as_reader();
        match reader.take(MAX_BODY_SIZE).read_to_end(&mut buf) {
            Ok(n) if n as u64 >= MAX_BODY_SIZE => {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", "response body exceeds size limit")?;
                result.set("error_kind", "too_large")?;
                result.set("session_id", session_id)?;
                return Ok(result);
            }
            Ok(_) => String::from_utf8(buf).map_err(|_| {
                mlua::Error::RuntimeError("LLM response body is not valid UTF-8".into())
            })?,
            Err(e) => {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", format!("failed to read response body: {}", e))?;
                result.set("error_kind", "network")?;
                result.set("session_id", session_id)?;
                return Ok(result);
            }
        }
    };

    // Non-2xx status
    if !(200..300).contains(&status) {
        let result = lua.create_table()?;
        result.set("ok", false)?;
        result.set(
            "error",
            format!("HTTP {}: {}", status, truncate_for_error(&body_text, 500)),
        )?;
        result.set("error_kind", classify_http_status(status))?;
        result.set("session_id", session_id)?;
        return Ok(result);
    }

    // Parse JSON
    let json: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
        mlua::Error::RuntimeError(format!(
            "failed to parse LLM response JSON: {} (body: {})",
            e,
            truncate_for_error(&body_text, 200)
        ))
    })?;

    // Extract content based on provider
    let (content, model) = match opts.provider {
        Provider::Ollama => parse_ollama_response(&json),
        Provider::OpenAI => parse_openai_response(&json),
        Provider::Anthropic => parse_anthropic_response(&json),
    };

    let content = match content {
        Ok(c) => c,
        Err(e) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", e)?;
            result.set("error_kind", "parse_error")?;
            result.set("session_id", session_id)?;
            return Ok(result);
        }
    };

    // Update session history
    update_session(lua, session_id, user_prompt, &content);

    // Build success response
    let result = lua.create_table()?;
    result.set("ok", true)?;
    result.set("content", content)?;
    result.set("model", model.unwrap_or_else(|| opts.model.clone()))?;
    result.set("session_id", session_id)?;

    Ok(result)
}

fn parse_ollama_response(json: &serde_json::Value) -> (Result<String, String>, Option<String>) {
    let content = json
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "Ollama: missing message.content in response: {}",
                truncate_for_error(&json.to_string(), 200)
            )
        });

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    (content, model)
}

fn parse_openai_response(json: &serde_json::Value) -> (Result<String, String>, Option<String>) {
    let content = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "OpenAI: missing choices[0].message.content in response: {}",
                truncate_for_error(&json.to_string(), 200)
            )
        });

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    (content, model)
}

fn parse_anthropic_response(json: &serde_json::Value) -> (Result<String, String>, Option<String>) {
    let content = json
        .get("content")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("text"))
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "Anthropic: missing content[0].text in response: {}",
                truncate_for_error(&json.to_string(), 200)
            )
        });

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    (content, model)
}

// ── Error Helpers ──────────────────────────────────────────────────────

/// Build an error Lua table from a ureq error.
fn build_error_result(lua: &Lua, error: ureq::Error, session_id: &str) -> mlua::Result<Table> {
    let (error_kind, error_msg) = classify_ureq_error(&error);

    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set("error", error_msg)?;
    result.set("error_kind", error_kind)?;
    result.set("session_id", session_id)?;
    Ok(result)
}

/// Classify a ureq error into kind + message.
fn classify_ureq_error(error: &ureq::Error) -> (&'static str, String) {
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
fn classify_http_status(status: u16) -> &'static str {
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
fn truncate_for_error(s: &str, max: usize) -> &str {
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

    // ── Provider unit tests ────────────────────────────────────────────

    #[test]
    fn provider_from_str_valid() {
        assert_eq!(
            Provider::from_str("ollama").expect("should parse ollama"),
            Provider::Ollama
        );
        assert_eq!(
            Provider::from_str("OpenAI").expect("should parse OpenAI"),
            Provider::OpenAI
        );
        assert_eq!(
            Provider::from_str("ANTHROPIC").expect("should parse ANTHROPIC"),
            Provider::Anthropic
        );
    }

    #[test]
    fn provider_from_str_invalid() {
        let err = Provider::from_str("gemini").expect_err("should reject gemini");
        assert!(
            err.contains("unsupported provider"),
            "error should mention unsupported, got: {}",
            err
        );
    }

    #[test]
    fn provider_defaults() {
        assert_eq!(
            Provider::Ollama.default_base_url(),
            "http://localhost:11434"
        );
        assert_eq!(
            Provider::OpenAI.default_base_url(),
            "https://api.openai.com"
        );
        assert_eq!(
            Provider::Anthropic.default_base_url(),
            "https://api.anthropic.com"
        );

        assert_eq!(Provider::Ollama.default_model(), "llama3.2");
        assert_eq!(Provider::OpenAI.default_model(), "gpt-4o");
        assert_eq!(
            Provider::Anthropic.default_model(),
            "claude-sonnet-4-20250514"
        );

        assert_eq!(Provider::Ollama.chat_path(), "/api/chat");
        assert_eq!(Provider::OpenAI.chat_path(), "/v1/chat/completions");
        assert_eq!(Provider::Anthropic.chat_path(), "/v1/messages");
    }

    #[test]
    fn provider_api_key_env() {
        assert_eq!(Provider::Ollama.api_key_env(), None);
        assert_eq!(Provider::OpenAI.api_key_env(), Some("OPENAI_API_KEY"));
        assert_eq!(Provider::Anthropic.api_key_env(), Some("ANTHROPIC_API_KEY"));
    }

    // ── LlmOpts tests ─────────────────────────────────────────────────

    #[test]
    fn llm_opts_defaults_to_ollama() {
        let opts = LlmOpts::from_lua(None).expect("should parse None opts");
        assert_eq!(opts.provider, Provider::Ollama);
        assert_eq!(opts.base_url, "http://localhost:11434");
        assert_eq!(opts.model, "llama3.2");
        assert_eq!(opts.timeout, 120);
        assert!(opts.api_key.is_none());
    }

    #[test]
    fn llm_opts_parses_provider() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("provider", "anthropic").expect("set provider");
        tbl.set("api_key", "test-key").expect("set api_key");

        let opts = LlmOpts::from_lua(Some(&tbl)).expect("should parse opts");
        assert_eq!(opts.provider, Provider::Anthropic);
        assert_eq!(opts.base_url, "https://api.anthropic.com");
        assert_eq!(opts.model, "claude-sonnet-4-20250514");
        assert_eq!(opts.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn llm_opts_custom_overrides() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("provider", "openai").expect("set provider");
        tbl.set("base_url", "https://custom.api.com")
            .expect("set base_url");
        tbl.set("model", "gpt-4o-mini").expect("set model");
        tbl.set("temperature", 0.5).expect("set temperature");
        tbl.set("max_tokens", 2048u64).expect("set max_tokens");
        tbl.set("timeout", 60u64).expect("set timeout");
        tbl.set("api_key", "sk-test").expect("set api_key");

        let opts = LlmOpts::from_lua(Some(&tbl)).expect("should parse opts");
        assert_eq!(opts.provider, Provider::OpenAI);
        assert_eq!(opts.base_url, "https://custom.api.com");
        assert_eq!(opts.model, "gpt-4o-mini");
        assert_eq!(opts.temperature, Some(0.5));
        assert_eq!(opts.max_tokens, Some(2048));
        assert_eq!(opts.timeout, 60);
        assert_eq!(opts.api_key.as_deref(), Some("sk-test"));
    }

    #[test]
    fn llm_opts_invalid_provider() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("provider", "gpt").expect("set provider");

        let err = LlmOpts::from_lua(Some(&tbl)).expect_err("should reject invalid provider");
        assert!(
            err.contains("unsupported provider"),
            "error should mention unsupported, got: {}",
            err
        );
    }

    // ── Request body builder tests ─────────────────────────────────────

    #[test]
    fn build_ollama_body_basic() {
        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: "http://localhost:11434".into(),
            model: "llama3.2".into(),
            api_key: None,
            system_prompt: None,
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };
        let messages = vec![Message {
            role: "user".into(),
            content: "hello".into(),
        }];

        let body = build_ollama_body(&opts, &messages).expect("should build body");
        assert_eq!(body["model"], "llama3.2");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        assert!(body.get("options").is_none(), "no options when empty");
    }

    #[test]
    fn build_ollama_body_with_options() {
        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: "http://localhost:11434".into(),
            model: "llama3.2".into(),
            api_key: None,
            system_prompt: None,
            session_id: None,
            temperature: Some(0.7),
            max_tokens: Some(4096),
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };
        let messages = vec![Message {
            role: "user".into(),
            content: "hi".into(),
        }];

        let body = build_ollama_body(&opts, &messages).expect("should build body");
        assert_eq!(body["options"]["temperature"], 0.7);
        assert_eq!(body["options"]["num_predict"], 4096);
    }

    #[test]
    fn build_openai_body_basic() {
        let opts = LlmOpts {
            provider: Provider::OpenAI,
            base_url: "https://api.openai.com".into(),
            model: "gpt-4o".into(),
            api_key: Some("sk-test".into()),
            system_prompt: None,
            session_id: None,
            temperature: Some(0.5),
            max_tokens: Some(1024),
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };
        let messages = vec![
            Message {
                role: "system".into(),
                content: "You are helpful.".into(),
            },
            Message {
                role: "user".into(),
                content: "hi".into(),
            },
        ];

        let body = build_openai_body(&opts, &messages).expect("should build body");
        assert_eq!(body["model"], "gpt-4o");
        assert_eq!(body["stream"], false);
        assert_eq!(body["temperature"], 0.5);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(
            body["messages"].as_array().expect("messages array").len(),
            2
        );
    }

    #[test]
    fn build_anthropic_body_system_at_top_level() {
        let opts = LlmOpts {
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-20250514".into(),
            api_key: Some("sk-ant-test".into()),
            system_prompt: Some("You are a coding assistant.".into()),
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };
        // messages might include a system message from non-Anthropic path,
        // but Anthropic builder should filter it out.
        let messages = vec![
            Message {
                role: "system".into(),
                content: "filtered out".into(),
            },
            Message {
                role: "user".into(),
                content: "hello".into(),
            },
        ];

        let body = build_anthropic_body(&opts, &messages).expect("should build body");

        // System at top level
        assert_eq!(body["system"], "You are a coding assistant.");

        // Messages should NOT contain system role
        let msgs = body["messages"].as_array().expect("messages array");
        assert_eq!(msgs.len(), 1, "system message should be filtered out");
        assert_eq!(msgs[0]["role"], "user");

        // max_tokens defaults to 4096
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn build_anthropic_body_custom_max_tokens() {
        let opts = LlmOpts {
            provider: Provider::Anthropic,
            base_url: "https://api.anthropic.com".into(),
            model: "claude-sonnet-4-20250514".into(),
            api_key: Some("sk-ant-test".into()),
            system_prompt: None,
            session_id: None,
            temperature: Some(0.3),
            max_tokens: Some(8192),
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };
        let messages = vec![Message {
            role: "user".into(),
            content: "hello".into(),
        }];

        let body = build_anthropic_body(&opts, &messages).expect("should build body");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["temperature"], 0.3);
        assert!(body.get("system").is_none(), "no system when not provided");
    }

    // ── Response parser tests ──────────────────────────────────────────

    #[test]
    fn parse_ollama_response_success() {
        let json = serde_json::json!({
            "model": "llama3.2",
            "message": {
                "role": "assistant",
                "content": "Hello! How can I help?"
            },
            "done": true,
            "done_reason": "stop"
        });
        let (content, model) = parse_ollama_response(&json);
        assert_eq!(
            content.expect("should parse content"),
            "Hello! How can I help?"
        );
        assert_eq!(model, Some("llama3.2".to_string()));
    }

    #[test]
    fn parse_ollama_response_missing_content() {
        let json = serde_json::json!({"model": "llama3.2"});
        let (content, _) = parse_ollama_response(&json);
        assert!(content.is_err(), "should error on missing content");
        let err = content.expect_err("expected error");
        assert!(
            err.contains("missing message.content"),
            "error should mention path, got: {}",
            err
        );
    }

    #[test]
    fn parse_openai_response_success() {
        let json = serde_json::json!({
            "id": "chatcmpl-xxx",
            "model": "gpt-4o-2024-05-13",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello from OpenAI!"
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        });
        let (content, model) = parse_openai_response(&json);
        assert_eq!(content.expect("should parse content"), "Hello from OpenAI!");
        assert_eq!(model, Some("gpt-4o-2024-05-13".to_string()));
    }

    #[test]
    fn parse_openai_response_missing_choices() {
        let json = serde_json::json!({"model": "gpt-4o"});
        let (content, _) = parse_openai_response(&json);
        assert!(content.is_err(), "should error on missing choices");
    }

    #[test]
    fn parse_anthropic_response_success() {
        let json = serde_json::json!({
            "id": "msg_xxx",
            "type": "message",
            "model": "claude-sonnet-4-20250514",
            "content": [{
                "type": "text",
                "text": "Hello from Anthropic!"
            }],
            "stop_reason": "end_turn",
            "usage": {
                "input_tokens": 10,
                "output_tokens": 8
            }
        });
        let (content, model) = parse_anthropic_response(&json);
        assert_eq!(
            content.expect("should parse content"),
            "Hello from Anthropic!"
        );
        assert_eq!(model, Some("claude-sonnet-4-20250514".to_string()));
    }

    #[test]
    fn parse_anthropic_response_missing_content() {
        let json = serde_json::json!({"model": "claude-sonnet-4-20250514"});
        let (content, _) = parse_anthropic_response(&json);
        assert!(content.is_err(), "should error on missing content");
        let err = content.expect_err("expected error");
        assert!(
            err.contains("missing content[0].text"),
            "error should mention path, got: {}",
            err
        );
    }

    // ── Session management tests ───────────────────────────────────────

    #[test]
    fn session_store_create_new() {
        let lua = Lua::new();

        let session_id = resolve_session_id(&lua, &None);
        assert!(
            session_id.starts_with("sess-"),
            "new session should start with sess-, got: {}",
            session_id
        );

        // Session should exist in store
        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        assert!(
            store.0.contains_key(&session_id),
            "session should be created in store"
        );
    }

    #[test]
    fn session_store_reuse_existing() {
        let lua = Lua::new();

        let id = resolve_session_id(&lua, &Some("my-session".to_string()));
        assert_eq!(id, "my-session");
    }

    #[test]
    fn session_history_update() {
        let lua = Lua::new();
        let sid = resolve_session_id(&lua, &None);

        update_session(&lua, &sid, "hello", "world");

        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        let history = store.0.get(&sid).expect("session should exist");
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].role, "user");
        assert_eq!(history[0].content.text(), Some("hello"));
        assert_eq!(history[1].role, "assistant");
        assert_eq!(history[1].content.text(), Some("world"));
    }

    #[test]
    fn build_messages_with_system_prompt_ollama() {
        let lua = Lua::new();
        let sid = resolve_session_id(&lua, &None);

        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            system_prompt: Some("Be helpful.".into()),
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };

        let msgs = build_messages(&lua, &sid, "hi", &opts);
        assert_eq!(msgs.len(), 2, "system + user");
        assert_eq!(msgs[0].role, "system");
        assert_eq!(msgs[0].content.text(), Some("Be helpful."));
        assert_eq!(msgs[1].role, "user");
        assert_eq!(msgs[1].content.text(), Some("hi"));
    }

    #[test]
    fn build_messages_with_system_prompt_anthropic_excluded() {
        let lua = Lua::new();
        let sid = resolve_session_id(&lua, &None);

        let opts = LlmOpts {
            provider: Provider::Anthropic,
            base_url: String::new(),
            model: String::new(),
            api_key: Some("key".into()),
            system_prompt: Some("Be helpful.".into()),
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };

        let msgs = build_messages(&lua, &sid, "hi", &opts);
        // Anthropic: system prompt NOT added to messages (handled at request body level)
        assert_eq!(msgs.len(), 1, "only user message for Anthropic");
        assert_eq!(msgs[0].role, "user");
    }

    #[test]
    fn build_messages_with_history() {
        let lua = Lua::new();
        let sid = resolve_session_id(&lua, &None);

        // Simulate previous exchange
        update_session(&lua, &sid, "first question", "first answer");

        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: String::new(),
            model: String::new(),
            api_key: None,
            system_prompt: Some("Be helpful.".into()),
            session_id: Some(sid.clone()),
            temperature: None,
            max_tokens: None,
            timeout: 120,
            max_retries: DEFAULT_MAX_RETRIES,
        };

        let msgs = build_messages(&lua, &sid, "second question", &opts);
        // History has 2 messages + 1 new user message = 3
        // (system_prompt NOT added because history is non-empty)
        assert_eq!(msgs.len(), 3, "history(2) + new user(1)");
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content.text(), Some("first question"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content.text(), Some("first answer"));
        assert_eq!(msgs[2].role, "user");
        assert_eq!(msgs[2].content.text(), Some("second question"));
    }

    // ── Error classification tests ─────────────────────────────────────

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

    // ── Deny stub test ─────────────────────────────────────────────────

    #[test]
    fn deny_stub_returns_permission_denied() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        let result: Table = lua
            .load(r#"return orcs.llm("hello")"#)
            .eval()
            .expect("should return deny table");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error: String = result.get("error").expect("get error");
        assert!(
            error.contains("llm denied"),
            "expected permission denied, got: {error}"
        );
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "permission_denied"
        );
    }

    // ── Integration: llm_request_impl with missing API key ─────────────

    #[test]
    fn openai_missing_api_key_returns_error() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "openai").expect("set provider");
        // Deliberately not setting api_key or env var

        // Temporarily clear the env var to ensure test isolation
        let prev = std::env::var("OPENAI_API_KEY").ok();
        std::env::remove_var("OPENAI_API_KEY");

        let result =
            llm_request_impl(&lua, ("hello".into(), Some(opts))).expect("should not panic");

        // Restore env
        if let Some(val) = prev {
            std::env::set_var("OPENAI_API_KEY", val);
        }

        assert!(!result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "missing_api_key"
        );
        let error: String = result.get("error").expect("get error");
        assert!(
            error.contains("OPENAI_API_KEY"),
            "error should mention env var, got: {}",
            error
        );
    }

    #[test]
    fn anthropic_missing_api_key_returns_error() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "anthropic").expect("set provider");

        let prev = std::env::var("ANTHROPIC_API_KEY").ok();
        std::env::remove_var("ANTHROPIC_API_KEY");

        let result =
            llm_request_impl(&lua, ("hello".into(), Some(opts))).expect("should not panic");

        if let Some(val) = prev {
            std::env::set_var("ANTHROPIC_API_KEY", val);
        }

        assert!(!result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "missing_api_key"
        );
    }

    #[test]
    fn ollama_no_api_key_required() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("timeout", 1u64).expect("set timeout");

        // This will fail to connect (Ollama likely not running) but should not
        // fail due to missing API key
        let result =
            llm_request_impl(&lua, ("hello".into(), Some(opts))).expect("should not panic");

        // If Ollama is running, ok=true; if not, ok=false but NOT missing_api_key
        if !result.get::<bool>("ok").expect("get ok") {
            let error_kind: String = result.get("error_kind").expect("get error_kind");
            assert_ne!(
                error_kind, "missing_api_key",
                "ollama should not require API key"
            );
        }
    }

    // ── Integration: connection error ──────────────────────────────────

    #[test]
    fn connection_refused_returns_network_error() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("base_url", "http://127.0.0.1:1")
            .expect("set base_url");
        opts.set("timeout", 2u64).expect("set timeout");

        let result =
            llm_request_impl(&lua, ("hello".into(), Some(opts))).expect("should not panic");
        assert!(!result.get::<bool>("ok").expect("get ok"));

        let error_kind: String = result.get("error_kind").expect("get error_kind");
        assert!(
            error_kind == "connection_refused"
                || error_kind == "network"
                || error_kind == "timeout",
            "expected connection error, got: {}",
            error_kind
        );

        // Should still have session_id
        let session_id: String = result.get("session_id").expect("get session_id");
        assert!(
            session_id.starts_with("sess-"),
            "should have session_id, got: {}",
            session_id
        );
    }

    /// connection_refused is NOT retried by default (server not running).
    /// With max_retries=0, no retry attempt is made.
    #[test]
    fn connection_refused_no_retry_with_zero_retries() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("base_url", "http://127.0.0.1:1")
            .expect("set base_url");
        opts.set("timeout", 1u64).expect("set timeout");
        opts.set("max_retries", 0u32).expect("set max_retries");

        let start = std::time::Instant::now();
        let result =
            llm_request_impl(&lua, ("hello".into(), Some(opts))).expect("should not panic");
        let elapsed = start.elapsed();

        assert!(!result.get::<bool>("ok").expect("get ok"));
        // Should complete quickly (no retry delay)
        assert!(
            elapsed < Duration::from_secs(5),
            "should not retry, elapsed: {:?}",
            elapsed
        );
    }

    // ── Retry helper tests ────────────────────────────────────────────

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

    // ── LlmOpts max_retries tests ────────────────────────────────────

    #[test]
    fn llm_opts_default_max_retries() {
        let opts = LlmOpts::from_lua(None).expect("should parse None opts");
        assert_eq!(opts.max_retries, DEFAULT_MAX_RETRIES);
    }

    #[test]
    fn llm_opts_custom_max_retries() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("max_retries", 5u32).expect("set max_retries");

        let opts = LlmOpts::from_lua(Some(&tbl)).expect("should parse opts");
        assert_eq!(opts.max_retries, 5);
    }

    #[test]
    fn llm_opts_zero_max_retries() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("max_retries", 0u32).expect("set max_retries");

        let opts = LlmOpts::from_lua(Some(&tbl)).expect("should parse opts");
        assert_eq!(opts.max_retries, 0);
    }

    // ── Session persistence tests ─────────────────────────────────────

    #[test]
    fn session_dump_empty() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        let json: String = lua
            .load(r#"return orcs.llm_dump_sessions()"#)
            .eval()
            .expect("should return json string");
        assert_eq!(json, "{}");
    }

    #[test]
    fn session_dump_with_history() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        // Create a session with some history
        let sid = resolve_session_id(&lua, &None);
        update_session(&lua, &sid, "hello", "world");

        let json: String = lua
            .load(r#"return orcs.llm_dump_sessions()"#)
            .eval()
            .expect("should return json string");

        let parsed: serde_json::Value = serde_json::from_str(&json).expect("should be valid JSON");
        assert!(parsed.is_object(), "should be JSON object");
        let sessions = parsed.as_object().expect("should be object");
        assert_eq!(sessions.len(), 1, "should have one session");

        let history = sessions.get(&sid).expect("should have session by id");
        let msgs = history.as_array().expect("should be array");
        assert_eq!(msgs.len(), 2, "should have 2 messages");
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], "hello");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"], "world");
    }

    #[test]
    fn session_load_roundtrip() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        // Create sessions
        let sid1 = resolve_session_id(&lua, &None);
        update_session(&lua, &sid1, "q1", "a1");
        let sid2 = resolve_session_id(&lua, &None);
        update_session(&lua, &sid2, "q2", "a2");

        // Dump
        let json: String = lua
            .load(r#"return orcs.llm_dump_sessions()"#)
            .eval()
            .expect("dump should succeed");

        // Clear store
        let _ = lua.remove_app_data::<SessionStore>();

        // Load back
        lua.globals()
            .get::<Table>("orcs")
            .expect("get orcs table")
            .get::<mlua::Function>("llm_load_sessions")
            .expect("get load fn")
            .call::<Table>(json.clone())
            .expect("load should succeed");

        // Verify restored
        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        assert_eq!(store.0.len(), 2, "should have 2 sessions");
        let h1 = store.0.get(&sid1).expect("session 1 should exist");
        assert_eq!(h1.len(), 2);
        assert_eq!(h1[0].content.text(), Some("q1"));
        assert_eq!(h1[1].content.text(), Some("a1"));
    }

    #[test]
    fn session_load_invalid_json() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        let result = lua
            .load(r#"return orcs.llm_load_sessions("not valid json")"#)
            .eval::<Table>();

        assert!(
            result.is_err(),
            "should error on invalid JSON, got: {:?}",
            result
        );
    }

    // ── llm_ping tests ──────────────────────────────────────────────────

    #[test]
    fn ping_defaults_to_ollama() {
        let lua = Lua::new();
        let result = llm_ping_impl(&lua, None).expect("should not panic");

        let provider: String = result.get("provider").expect("get provider");
        assert_eq!(provider, "ollama");

        let base_url: String = result.get("base_url").expect("get base_url");
        assert_eq!(base_url, "http://localhost:11434");

        // latency_ms is always present
        let _: u64 = result.get("latency_ms").expect("get latency_ms");
    }

    #[test]
    fn ping_invalid_provider() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "gemini").expect("set provider");

        let result = llm_ping_impl(&lua, Some(opts)).expect("should not panic");
        assert!(!result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "invalid_options"
        );
    }

    #[test]
    fn ping_connection_refused() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("base_url", "http://127.0.0.1:1")
            .expect("set base_url");
        opts.set("timeout", 2u64).expect("set timeout");

        let result = llm_ping_impl(&lua, Some(opts)).expect("should not panic");
        assert!(
            !result.get::<bool>("ok").expect("get ok"),
            "should fail when nothing is listening"
        );

        let error_kind: String = result.get("error_kind").expect("get error_kind");
        assert!(
            error_kind == "connection_refused"
                || error_kind == "network"
                || error_kind == "timeout",
            "expected connection error, got: {}",
            error_kind
        );
    }

    #[test]
    fn ping_deny_stub_returns_permission_denied() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        let result: Table = lua
            .load(r#"return orcs.llm_ping()"#)
            .eval()
            .expect("should return deny table");

        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error: String = result.get("error").expect("get error");
        assert!(
            error.contains("llm_ping denied"),
            "expected permission denied, got: {error}"
        );
        assert_eq!(
            result.get::<String>("error_kind").expect("get error_kind"),
            "permission_denied"
        );
    }

    /// E2E: ping a real Ollama server.
    /// Run with: cargo test -p orcs-lua --lib llm_command::tests::e2e_ollama_ping -- --ignored --nocapture
    #[test]
    #[ignore = "requires running Ollama server"]
    fn e2e_ollama_ping() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("timeout", 5u64).expect("set timeout");

        let result = llm_ping_impl(&lua, Some(opts)).expect("should not panic");

        let ok = result.get::<bool>("ok").expect("get ok");
        assert!(ok, "should succeed with running Ollama");

        let status: u16 = result.get("status").expect("get status");
        assert_eq!(status, 200, "Ollama root should return 200");

        let latency: u64 = result.get("latency_ms").expect("get latency_ms");
        assert!(
            latency < 5000,
            "latency should be under 5s, got: {}ms",
            latency
        );

        let provider: String = result.get("provider").expect("get provider");
        assert_eq!(provider, "ollama");

        eprintln!("[E2E] ping ok={ok} status={status} latency={latency}ms");
    }

    // ── E2E tests (require running Ollama) ──────────────────────────────

    /// E2E: single-turn call to real Ollama server.
    /// Run with: cargo test -p orcs-lua --lib llm_command::tests::e2e_ollama_single_turn -- --ignored --nocapture
    #[test]
    #[ignore = "requires running Ollama server"]
    fn e2e_ollama_single_turn() {
        let lua = Lua::new();
        let opts = lua.create_table().expect("create opts");
        opts.set("provider", "ollama").expect("set provider");
        opts.set("model", "qwen2.5-coder:1.5b").expect("set model");
        opts.set("timeout", 30u64).expect("set timeout");
        opts.set("max_retries", 0u32).expect("set max_retries");

        let result = llm_request_impl(&lua, ("Say exactly: HELLO_ORCS".into(), Some(opts)))
            .expect("should not panic");

        let ok = result.get::<bool>("ok").expect("get ok");
        assert!(ok, "should succeed with running Ollama");

        let content: String = result.get("content").expect("get content");
        assert!(!content.is_empty(), "content should not be empty");

        let session_id: String = result.get("session_id").expect("get session_id");
        assert!(
            session_id.starts_with("sess-"),
            "should have session_id, got: {}",
            session_id
        );

        let model: String = result.get("model").expect("get model");
        assert!(
            model.contains("qwen"),
            "model should contain qwen, got: {}",
            model
        );

        eprintln!("[E2E] ok={ok} model={model} session_id={session_id}");
        eprintln!("[E2E] content: {content}");
    }

    /// E2E: multi-turn session with real Ollama server.
    /// Run with: cargo test -p orcs-lua --lib llm_command::tests::e2e_ollama_multi_turn -- --ignored --nocapture
    #[test]
    #[ignore = "requires running Ollama server"]
    fn e2e_ollama_multi_turn() {
        let lua = Lua::new();

        // Turn 1
        let opts1 = lua.create_table().expect("create opts");
        opts1.set("provider", "ollama").expect("set provider");
        opts1.set("model", "qwen2.5-coder:1.5b").expect("set model");
        opts1.set("timeout", 30u64).expect("set timeout");
        opts1
            .set("system_prompt", "You are a helpful assistant. Be concise.")
            .expect("set system_prompt");

        let r1 = llm_request_impl(
            &lua,
            (
                "My name is ORCS_TEST_USER. Remember it.".into(),
                Some(opts1),
            ),
        )
        .expect("turn 1 should not panic");

        assert!(
            r1.get::<bool>("ok").expect("get ok"),
            "turn 1 should succeed"
        );
        let sid: String = r1.get("session_id").expect("get session_id");
        let content1: String = r1.get("content").expect("get content");
        eprintln!("[E2E turn 1] session={sid} content: {content1}");

        // Turn 2: use same session
        let opts2 = lua.create_table().expect("create opts");
        opts2.set("provider", "ollama").expect("set provider");
        opts2.set("model", "qwen2.5-coder:1.5b").expect("set model");
        opts2.set("timeout", 30u64).expect("set timeout");
        opts2
            .set("session_id", sid.as_str())
            .expect("set session_id");

        let r2 = llm_request_impl(&lua, ("What is my name?".into(), Some(opts2)))
            .expect("turn 2 should not panic");

        assert!(
            r2.get::<bool>("ok").expect("get ok"),
            "turn 2 should succeed"
        );
        let sid2: String = r2.get("session_id").expect("get session_id");
        assert_eq!(sid, sid2, "session_id should be preserved across turns");

        let content2: String = r2.get("content").expect("get content");
        eprintln!("[E2E turn 2] content: {content2}");

        // Verify session store has history
        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        let history = store.0.get(&sid).expect("session should exist");
        // Turn 1: user + assistant = 2, Turn 2: user + assistant = 2, total = 4
        assert_eq!(
            history.len(),
            4,
            "session should have 4 messages (2 turns), got: {}",
            history.len()
        );
    }

    #[test]
    fn session_load_returns_count() {
        let lua = Lua::new();
        let orcs = crate::orcs_helpers::ensure_orcs_table(&lua).expect("create orcs table");
        register_llm_deny_stub(&lua, &orcs).expect("register stub");

        let json = r#"{"sess-1": [{"role":"user","content":"hi"}], "sess-2": []}"#;
        let result: Table = lua
            .load(format!(
                r#"return orcs.llm_load_sessions('{}')"#,
                json.replace('\'', "\\'")
            ))
            .eval()
            .expect("load should succeed");

        assert!(result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<i64>("count").expect("get count"),
            2,
            "should report 2 sessions loaded"
        );
    }
}

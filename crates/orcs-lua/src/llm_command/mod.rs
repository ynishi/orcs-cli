//! Multi-provider LLM client for `orcs.llm()`.
//!
//! Direct HTTP implementation (via `ureq`) supporting Ollama, OpenAI, and Anthropic.
//! Provider-agnostic blocking HTTP client supporting Ollama, OpenAI, and Anthropic.
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
//! - Multi-turn tool loops not supported (Phase 6: resolve flow)

mod provider;
pub(crate) mod resolve;
mod retry;
mod session;

use mlua::{Lua, Table};
use orcs_types::intent::StopReason;
use std::collections::HashMap;
use std::time::Duration;

use provider::{build_request_body, build_tools_for_provider, Provider};
use resolve::{
    build_assistant_content_blocks, build_lua_result, dispatch_intents_to_results,
    parse_response_body, ResponseOrError,
};
use retry::{build_error_result, classify_ureq_error, send_with_retry, SendError};
use session::{
    append_message, build_messages, ensure_session_store, resolve_session_id, update_session,
    Message, SessionStore,
};

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

/// Default maximum number of tool-loop turns (resolve mode).
const DEFAULT_MAX_TOOL_TURNS: u32 = 10;

// ── Parsed Options ─────────────────────────────────────────────────────

/// Parsed and validated options from the Lua opts table.
#[derive(Debug)]
pub(super) struct LlmOpts {
    pub provider: Provider,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub session_id: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub timeout: u64,
    pub max_retries: u32,
    /// Whether to send IntentDefs as tools to the LLM (default: true).
    pub tools: bool,
    /// Whether to auto-resolve intents in Rust (default: false).
    /// When true, tool_call intents are dispatched automatically and
    /// results are fed back to the LLM in a multi-turn loop.
    /// When false, intents are returned to Lua for manual dispatch.
    pub resolve: bool,
    /// Maximum number of tool-loop turns before stopping (default: 10).
    pub max_tool_turns: u32,
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

        let tools = opts
            .and_then(|o| o.get::<bool>("tools").ok())
            .unwrap_or(true);

        let resolve = opts
            .and_then(|o| o.get::<bool>("resolve").ok())
            .unwrap_or(false);

        let max_tool_turns = opts
            .and_then(|o| o.get::<u32>("max_tool_turns").ok())
            .unwrap_or(DEFAULT_MAX_TOOL_TURNS);

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
            tools,
            resolve,
            max_tool_turns,
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

    // Build tools JSON from IntentRegistry (when opts.tools is true)
    let tools_json = if llm_opts.tools {
        build_tools_for_provider(lua, llm_opts.provider)
    } else {
        None
    };

    // Build URL
    let url = format!(
        "{}{}",
        llm_opts.base_url.trim_end_matches('/'),
        llm_opts.provider.chat_path()
    );

    // Configure ureq agent (reused across retries and tool turns)
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(llm_opts.timeout)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    // ── First turn: build messages from history + prompt ──
    let mut messages = build_messages(lua, &session_id, &prompt, &llm_opts);

    // ── Tool loop ──
    // Each iteration: build body → send → parse → if tool_use && resolve → dispatch → append results → repeat
    for tool_turn in 0..=llm_opts.max_tool_turns {
        let request_body = match build_request_body(&llm_opts, &messages, tools_json.as_ref()) {
            Ok(body) => body,
            Err(e) => {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", e)?;
                result.set("error_kind", "request_build_error")?;
                return Ok(result);
            }
        };

        let body_str = request_body.to_string();
        tracing::debug!(
            "llm request turn={}: {} {} ({}B)",
            tool_turn,
            llm_opts.provider.chat_path(),
            llm_opts.model,
            body_str.len()
        );

        // Send with retry
        let resp = match send_with_retry(&agent, &url, &llm_opts, &body_str) {
            Ok(resp) => resp,
            Err(SendError::Transport(e)) => return build_error_result(lua, e, &session_id),
        };

        // Parse response
        let parsed_resp = match parse_response_body(lua, resp, &llm_opts, &session_id)? {
            ResponseOrError::Parsed(p) => p,
            ResponseOrError::ErrorTable(t) => return Ok(t),
        };

        let is_tool_use = parsed_resp.stop_reason == StopReason::ToolUse;
        let should_resolve = is_tool_use && llm_opts.resolve && !parsed_resp.intents.is_empty();

        if should_resolve && tool_turn < llm_opts.max_tool_turns {
            // ── Auto-resolve: dispatch intents and continue loop ──

            // Build assistant message with ContentBlocks (preserves tool_use blocks)
            let assistant_blocks = build_assistant_content_blocks(&parsed_resp);
            messages.push(session::Message {
                role: orcs_types::intent::Role::Assistant,
                content: assistant_blocks.clone(),
            });
            append_message(
                lua,
                &session_id,
                orcs_types::intent::Role::Assistant,
                assistant_blocks,
            );

            // Dispatch each intent and collect tool results
            let tool_result_content = dispatch_intents_to_results(lua, &parsed_resp.intents)?;
            messages.push(session::Message {
                role: orcs_types::intent::Role::User,
                content: tool_result_content.clone(),
            });
            append_message(
                lua,
                &session_id,
                orcs_types::intent::Role::User,
                tool_result_content,
            );

            let intent_names: Vec<&str> = parsed_resp
                .intents
                .iter()
                .map(|i| i.name.as_str())
                .collect();
            tracing::info!(
                "tool turn {}: resolved {} intent(s) [{}], continuing",
                tool_turn,
                parsed_resp.intents.len(),
                intent_names.join(", ")
            );
            continue;
        }

        // ── Final response: return to Lua ──
        // For non-resolve mode or final turn: store text-only in session
        update_session(lua, &session_id, &prompt, &parsed_resp.content);

        return build_lua_result(lua, &parsed_resp, &llm_opts, &session_id);
    }

    // Tool loop exhausted
    let result = lua.create_table()?;
    result.set("ok", false)?;
    result.set(
        "error",
        format!(
            "tool loop exceeded max_tool_turns ({})",
            llm_opts.max_tool_turns
        ),
    )?;
    result.set("error_kind", "tool_loop_limit")?;
    result.set("session_id", session_id)?;
    Ok(result)
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn llm_opts_resolve_defaults() {
        let lua = Lua::new();
        let opts_tbl = lua.create_table().expect("create table");
        let opts = LlmOpts::from_lua(Some(&opts_tbl)).expect("parse opts");
        assert!(!opts.resolve, "resolve should default to false");
        assert_eq!(
            opts.max_tool_turns, DEFAULT_MAX_TOOL_TURNS,
            "max_tool_turns should default"
        );
    }

    #[test]
    fn llm_opts_resolve_custom() {
        let lua = Lua::new();
        let opts_tbl = lua.create_table().expect("create table");
        opts_tbl.set("resolve", true).expect("set resolve");
        opts_tbl
            .set("max_tool_turns", 3u32)
            .expect("set max_tool_turns");
        let opts = LlmOpts::from_lua(Some(&opts_tbl)).expect("parse opts");
        assert!(opts.resolve, "resolve should be true");
        assert_eq!(opts.max_tool_turns, 3, "max_tool_turns should be 3");
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

    // ── Ping tests ──────────────────────────────────────────────────────

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

    // ── E2E tests (require running Ollama) ──────────────────────────────

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
}

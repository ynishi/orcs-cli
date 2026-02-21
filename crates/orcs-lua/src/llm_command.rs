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
//! - Multi-turn tool loops not supported (Phase 6: resolve flow)

use mlua::{Lua, Table};
use orcs_types::intent::{ActionIntent, IntentDef, MessageContent, Role, StopReason};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

use crate::llm_adapter;
use crate::tool_registry::IntentRegistry;
use crate::types::serde_json_to_lua;

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
    role: Role,
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
    /// Whether to send IntentDefs as tools to the LLM (default: true).
    tools: bool,
    /// Whether to auto-resolve intents in Rust (default: false).
    /// When true, tool_call intents are dispatched automatically and
    /// results are fed back to the LLM in a multi-turn loop.
    /// When false, intents are returned to Lua for manual dispatch.
    resolve: bool,
    /// Maximum number of tool-loop turns before stopping (default: 10).
    max_tool_turns: u32,
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
            messages.push(Message {
                role: Role::Assistant,
                content: assistant_blocks.clone(),
            });
            append_message(lua, &session_id, Role::Assistant, assistant_blocks);

            // Dispatch each intent and collect tool results
            let tool_result_content = dispatch_intents_to_results(lua, &parsed_resp.intents)?;
            messages.push(Message {
                role: Role::User,
                content: tool_result_content.clone(),
            });
            append_message(lua, &session_id, Role::User, tool_result_content);

            tracing::debug!(
                "tool turn {}: resolved {} intents, continuing",
                tool_turn,
                parsed_resp.intents.len()
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
                    role: Role::System,
                    content: MessageContent::Text(sys.clone()),
                });
            }
        }
    }

    // Add current user message
    messages.push(Message {
        role: Role::User,
        content: MessageContent::Text(prompt.to_string()),
    });

    messages
}

/// Store assistant response and user message in session history (text-only).
fn update_session(lua: &Lua, session_id: &str, user_msg: &str, assistant_msg: &str) {
    if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
        let history = store.0.entry(session_id.to_string()).or_default();
        history.push(Message {
            role: Role::User,
            content: MessageContent::Text(user_msg.to_string()),
        });
        history.push(Message {
            role: Role::Assistant,
            content: MessageContent::Text(assistant_msg.to_string()),
        });
        lua.set_app_data(store);
    }
}

/// Append a single message to session history (supports ContentBlock::Blocks).
fn append_message(lua: &Lua, session_id: &str, role: Role, content: MessageContent) {
    if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
        let history = store.0.entry(session_id.to_string()).or_default();
        history.push(Message { role, content });
        lua.set_app_data(store);
    }
}

// ── Request Body Builders ──────────────────────────────────────────────

/// Build the JSON request body for the given provider.
fn build_request_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match opts.provider {
        Provider::Ollama => build_ollama_body(opts, messages, tools),
        Provider::OpenAI => build_openai_body(opts, messages, tools),
        Provider::Anthropic => build_anthropic_body(opts, messages, tools),
    }
}

/// Convert a single internal Message to OpenAI/Ollama wire format.
///
/// A single Message may expand to multiple wire messages:
/// - `MessageContent::Blocks` with `ToolUse` → assistant message with `tool_calls` array
/// - `MessageContent::Blocks` with `ToolResult` → one `role: "tool"` message per result
///
/// `stringify_args`: when true, tool_call arguments are serialized as a JSON
/// string (OpenAI format); when false, kept as a JSON object (Ollama format).
fn message_to_openai_wire(m: &Message, stringify_args: bool) -> Vec<serde_json::Value> {
    use orcs_types::intent::ContentBlock;

    match &m.content {
        MessageContent::Text(s) => {
            vec![serde_json::json!({"role": m.role, "content": s})]
        }
        MessageContent::Blocks(blocks) => {
            let mut text_parts: Vec<&str> = Vec::new();
            let mut tool_uses: Vec<(&str, &str, &serde_json::Value)> = Vec::new();
            let mut tool_results: Vec<(&str, &str)> = Vec::new();

            for block in blocks {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.as_str()),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_uses.push((id.as_str(), name.as_str(), input));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        tool_results.push((tool_use_id.as_str(), content.as_str()));
                    }
                }
            }

            let mut msgs = Vec::new();

            // Assistant message with tool_calls
            if !tool_uses.is_empty() {
                let content_val = if text_parts.is_empty() {
                    serde_json::Value::Null
                } else {
                    serde_json::Value::String(text_parts.join(""))
                };

                let calls: Vec<serde_json::Value> = tool_uses
                    .iter()
                    .map(|(id, name, input)| {
                        let args_val = if stringify_args {
                            serde_json::Value::String(
                                serde_json::to_string(input).unwrap_or_default(),
                            )
                        } else {
                            (*input).clone()
                        };
                        serde_json::json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": args_val,
                            }
                        })
                    })
                    .collect();

                msgs.push(serde_json::json!({
                    "role": "assistant",
                    "content": content_val,
                    "tool_calls": calls,
                }));
            }

            // Tool results: one message per result with role "tool"
            for (tool_use_id, content) in &tool_results {
                msgs.push(serde_json::json!({
                    "role": Role::Tool,
                    "tool_call_id": tool_use_id,
                    "content": content,
                }));
            }

            // Text-only blocks (no tool_use or tool_result)
            if tool_uses.is_empty() && tool_results.is_empty() {
                let text = if text_parts.is_empty() {
                    String::new()
                } else {
                    text_parts.join("")
                };
                msgs.push(serde_json::json!({"role": m.role, "content": text}));
            }

            if msgs.is_empty() {
                msgs.push(serde_json::json!({"role": m.role, "content": ""}));
            }

            msgs
        }
    }
}

fn build_ollama_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .flat_map(|m| message_to_openai_wire(m, false))
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

    if let Some(t) = tools {
        body["tools"] = t.clone();
    }

    Ok(body)
}

fn build_openai_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .flat_map(|m| message_to_openai_wire(m, true))
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

    if let Some(t) = tools {
        body["tools"] = t.clone();
    }

    Ok(body)
}

fn build_anthropic_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    // Anthropic: system prompt is top-level, NOT in messages.
    // Filter out any system messages from the messages array.
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != Role::System)
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

    if let Some(t) = tools {
        body["tools"] = t.clone();
    }

    Ok(body)
}

// ── Tools JSON Builders ──────────────────────────────────────────────

/// Build tools JSON from IntentRegistry for the given provider.
///
/// Returns `None` if the registry is empty or not initialized.
fn build_tools_for_provider(lua: &Lua, provider: Provider) -> Option<serde_json::Value> {
    let registry = lua.app_data_ref::<IntentRegistry>()?;
    if registry.is_empty() {
        return None;
    }
    let defs = registry.all();
    let tools = match provider {
        Provider::Anthropic => build_tools_anthropic_format(defs),
        Provider::Ollama | Provider::OpenAI => build_tools_openai_format(defs),
    };
    Some(tools)
}

/// Build tools array in OpenAI/Ollama format.
///
/// ```json
/// [{ "type": "function", "function": { "name": "...", "description": "...", "parameters": {...} } }]
/// ```
fn build_tools_openai_format(defs: &[IntentDef]) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = defs
        .iter()
        .map(|d| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": d.name,
                    "description": d.description,
                    "parameters": d.parameters,
                }
            })
        })
        .collect();
    serde_json::Value::Array(tools)
}

/// Build tools array in Anthropic format.
///
/// ```json
/// [{ "name": "...", "description": "...", "input_schema": {...} }]
/// ```
fn build_tools_anthropic_format(defs: &[IntentDef]) -> serde_json::Value {
    let tools: Vec<serde_json::Value> = defs
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": d.name,
                "description": d.description,
                "input_schema": d.parameters,
            })
        })
        .collect();
    serde_json::Value::Array(tools)
}

// ── Parsed LLM Response ─────────────────────────────────────────────

/// Provider-agnostic parsed LLM response.
struct ParsedLlmResponse {
    /// Text content extracted from the response.
    content: String,
    /// Model name from the response (may differ from requested model).
    model: Option<String>,
    /// Normalized stop reason.
    stop_reason: StopReason,
    /// Tool-use intents extracted from the response (empty if text-only).
    intents: Vec<ActionIntent>,
}

/// Parse a provider response JSON into a unified `ParsedLlmResponse`.
fn parse_provider_response(
    provider: Provider,
    json: &serde_json::Value,
) -> Result<ParsedLlmResponse, String> {
    let blocks = match provider {
        Provider::Ollama => llm_adapter::extract_content_ollama(json),
        Provider::OpenAI => llm_adapter::extract_content_openai(json),
        Provider::Anthropic => llm_adapter::extract_content_anthropic(json),
    };

    let stop_reason = match provider {
        Provider::Ollama => llm_adapter::extract_stop_reason_ollama(json),
        Provider::OpenAI => llm_adapter::extract_stop_reason_openai(json),
        Provider::Anthropic => llm_adapter::extract_stop_reason_anthropic(json),
    };

    let intents = llm_adapter::content_blocks_to_intents(&blocks);
    let message_content = llm_adapter::blocks_to_message_content(blocks);

    // Extract text for session history and backward-compatible `content` field
    let content = message_content.text().unwrap_or("").to_string();

    let model = json
        .get("model")
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());

    Ok(ParsedLlmResponse {
        content,
        model,
        stop_reason,
        intents,
    })
}

/// Convert StopReason to a Lua-friendly string.
fn stop_reason_to_str(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
    }
}

// ── HTTP Send ─────────────────────────────────────────────────────────

/// Error type for send_with_retry.
enum SendError {
    Transport(ureq::Error),
}

/// Send an HTTP request with retry logic. Returns the raw response on success.
fn send_with_retry(
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

// ── Response Parsers ───────────────────────────────────────────────────

/// Either a parsed response or an error Lua table.
enum ResponseOrError {
    Parsed(ParsedLlmResponse),
    ErrorTable(Table),
}

/// Parse HTTP response body into ParsedLlmResponse or error table.
fn parse_response_body(
    lua: &Lua,
    mut resp: ureq::http::Response<ureq::Body>,
    opts: &LlmOpts,
    session_id: &str,
) -> mlua::Result<ResponseOrError> {
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
                return Ok(ResponseOrError::ErrorTable(result));
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
                return Ok(ResponseOrError::ErrorTable(result));
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
        return Ok(ResponseOrError::ErrorTable(result));
    }

    // Parse JSON
    let json: serde_json::Value = serde_json::from_str(&body_text).map_err(|e| {
        mlua::Error::RuntimeError(format!(
            "failed to parse LLM response JSON: {} (body: {})",
            e,
            truncate_for_error(&body_text, 200)
        ))
    })?;

    // Extract content via adapter-based parsing
    let parsed = match parse_provider_response(opts.provider, &json) {
        Ok(p) => p,
        Err(e) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", e)?;
            result.set("error_kind", "parse_error")?;
            result.set("session_id", session_id)?;
            return Ok(ResponseOrError::ErrorTable(result));
        }
    };

    Ok(ResponseOrError::Parsed(parsed))
}

// ── Resolve-flow Helpers ──────────────────────────────────────────────

/// Build MessageContent::Blocks from a ParsedLlmResponse for session storage.
///
/// Reconstructs the assistant message as ContentBlocks so that tool_use blocks
/// are preserved in session history for multi-turn tool loops.
fn build_assistant_content_blocks(parsed: &ParsedLlmResponse) -> MessageContent {
    use orcs_types::intent::ContentBlock;

    let mut blocks = Vec::new();

    // Add text block if non-empty
    if !parsed.content.is_empty() {
        blocks.push(ContentBlock::Text {
            text: parsed.content.clone(),
        });
    }

    // Add tool_use blocks from intents
    for intent in &parsed.intents {
        blocks.push(ContentBlock::ToolUse {
            id: intent.id.clone(),
            name: intent.name.clone(),
            input: intent.params.clone(),
        });
    }

    if blocks.is_empty() {
        MessageContent::Text(String::new())
    } else {
        MessageContent::Blocks(blocks)
    }
}

/// Dispatch intents via `orcs.dispatch()` and collect ToolResult content blocks.
///
/// Each intent is dispatched through the Lua `orcs.dispatch(name, params)` function.
/// The result (or error) is wrapped into a `ContentBlock::ToolResult` for the next
/// LLM turn.
fn dispatch_intents_to_results(
    lua: &Lua,
    intents: &[ActionIntent],
) -> mlua::Result<MessageContent> {
    use orcs_types::intent::ContentBlock;

    let orcs: Table = lua.globals().get("orcs")?;
    let dispatch_fn: mlua::Function = orcs.get("dispatch")?;

    let mut blocks = Vec::new();

    for intent in intents {
        let params_lua = serde_json_to_lua(&intent.params, lua)?;
        let dispatch_result: mlua::Result<Table> =
            dispatch_fn.call((intent.name.as_str(), params_lua));

        let (content_str, is_error) = match dispatch_result {
            Ok(tbl) => {
                let ok: bool = tbl.get("ok").unwrap_or(false);
                if ok {
                    // Try "data" field first (Component dispatch normalizes to {ok, data}).
                    // If absent or nil, serialize the entire result table — Internal tools
                    // return fields directly (e.g. {ok, content, size} for read).
                    let data = match tbl.get::<mlua::Value>("data") {
                        Ok(v) if !matches!(v, mlua::Value::Nil) => Some(v),
                        _ => None,
                    };
                    let text = match data {
                        Some(mlua::Value::String(s)) => {
                            s.to_str().map_or_else(|_| String::new(), |b| b.to_string())
                        }
                        Some(v) => {
                            // Convert Lua value to JSON for structured data
                            serde_json::to_string(&lua_value_to_json(v))
                                .unwrap_or_else(|_| "{}".to_string())
                        }
                        None => {
                            // No "data" field: serialize the whole result table so
                            // the LLM receives actual tool output (file content, etc.)
                            serde_json::to_string(&lua_value_to_json(mlua::Value::Table(tbl)))
                                .unwrap_or_else(|_| "{}".to_string())
                        }
                    };
                    (text, false)
                } else {
                    let err: String = tbl
                        .get("error")
                        .unwrap_or_else(|_| "dispatch failed".to_string());
                    (err, true)
                }
            }
            Err(e) => (format!("dispatch error: {e}"), true),
        };

        blocks.push(ContentBlock::ToolResult {
            tool_use_id: intent.id.clone(),
            content: content_str,
            is_error: Some(is_error),
        });
    }

    Ok(MessageContent::Blocks(blocks))
}

/// Build the final Lua result table from a ParsedLlmResponse.
fn build_lua_result(
    lua: &Lua,
    parsed: &ParsedLlmResponse,
    opts: &LlmOpts,
    session_id: &str,
) -> mlua::Result<Table> {
    let result = lua.create_table()?;
    result.set("ok", true)?;
    result.set("content", parsed.content.as_str())?;
    result.set(
        "model",
        parsed.model.as_deref().unwrap_or(opts.model.as_str()),
    )?;
    result.set("session_id", session_id)?;
    result.set("stop_reason", stop_reason_to_str(&parsed.stop_reason))?;

    // Intents array (empty when text-only response)
    if !parsed.intents.is_empty() {
        let intents_table = lua.create_table()?;
        for (i, intent) in parsed.intents.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("id", intent.id.as_str())?;
            entry.set("name", intent.name.as_str())?;
            entry.set("params", serde_json_to_lua(&intent.params, lua)?)?;
            intents_table.set(i + 1, entry)?;
        }
        result.set("intents", intents_table)?;
    }

    Ok(result)
}

/// Convert a Lua value to serde_json::Value (best-effort).
fn lua_value_to_json(val: mlua::Value) -> serde_json::Value {
    match val {
        mlua::Value::Nil => serde_json::Value::Null,
        mlua::Value::Boolean(b) => serde_json::Value::Bool(b),
        mlua::Value::Integer(n) => serde_json::json!(n),
        mlua::Value::Number(n) => serde_json::json!(n),
        mlua::Value::String(s) => {
            serde_json::Value::String(s.to_str().map_or_else(|_| String::new(), |b| b.to_string()))
        }
        mlua::Value::Table(t) => {
            // Check if it's an array (sequential integer keys starting from 1)
            let len = t.raw_len();
            if len > 0 {
                let arr: Vec<serde_json::Value> = (1..=len)
                    .filter_map(|i| t.get::<mlua::Value>(i).ok())
                    .map(lua_value_to_json)
                    .collect();
                serde_json::Value::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                if let Ok(pairs) = t
                    .pairs::<mlua::Value, mlua::Value>()
                    .collect::<Result<Vec<_>, _>>()
                {
                    for (k, v) in pairs {
                        if let mlua::Value::String(ks) = k {
                            let key = ks
                                .to_str()
                                .map_or_else(|_| String::new(), |b| b.to_string());
                            map.insert(key, lua_value_to_json(v));
                        }
                    }
                }
                serde_json::Value::Object(map)
            }
        }
        other => {
            tracing::debug!(
                "lua_value_to_json: unconvertible Lua type '{}' mapped to null",
                other.type_name()
            );
            serde_json::Value::Null
        }
    }
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
        }];

        let body = build_ollama_body(&opts, &messages, None).expect("should build body");
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let messages = vec![Message {
            role: Role::User,
            content: "hi".into(),
        }];

        let body = build_ollama_body(&opts, &messages, None).expect("should build body");
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let messages = vec![
            Message {
                role: Role::System,
                content: "You are helpful.".into(),
            },
            Message {
                role: Role::User,
                content: "hi".into(),
            },
        ];

        let body = build_openai_body(&opts, &messages, None).expect("should build body");
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        // messages might include a system message from non-Anthropic path,
        // but Anthropic builder should filter it out.
        let messages = vec![
            Message {
                role: Role::System,
                content: "filtered out".into(),
            },
            Message {
                role: Role::User,
                content: "hello".into(),
            },
        ];

        let body = build_anthropic_body(&opts, &messages, None).expect("should build body");

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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let messages = vec![Message {
            role: Role::User,
            content: "hello".into(),
        }];

        let body = build_anthropic_body(&opts, &messages, None).expect("should build body");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["temperature"], 0.3);
        assert!(body.get("system").is_none(), "no system when not provided");
    }

    // ── Response parser tests (via parse_provider_response) ────────────

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
        let parsed =
            parse_provider_response(Provider::Ollama, &json).expect("should parse ollama response");
        assert_eq!(parsed.content, "Hello! How can I help?");
        assert_eq!(parsed.model, Some("llama3.2".to_string()));
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert!(
            parsed.intents.is_empty(),
            "text-only response has no intents"
        );
    }

    #[test]
    fn parse_ollama_response_empty_returns_empty_content() {
        let json = serde_json::json!({"model": "llama3.2"});
        let parsed = parse_provider_response(Provider::Ollama, &json)
            .expect("should parse even with missing message");
        assert_eq!(parsed.content, "", "empty response → empty content");
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
        let parsed =
            parse_provider_response(Provider::OpenAI, &json).expect("should parse openai response");
        assert_eq!(parsed.content, "Hello from OpenAI!");
        assert_eq!(parsed.model, Some("gpt-4o-2024-05-13".to_string()));
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert!(parsed.intents.is_empty());
    }

    #[test]
    fn parse_openai_response_empty_returns_empty_content() {
        let json = serde_json::json!({"model": "gpt-4o"});
        let parsed = parse_provider_response(Provider::OpenAI, &json)
            .expect("should parse even with missing choices");
        assert_eq!(parsed.content, "");
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
        let parsed = parse_provider_response(Provider::Anthropic, &json)
            .expect("should parse anthropic response");
        assert_eq!(parsed.content, "Hello from Anthropic!");
        assert_eq!(parsed.model, Some("claude-sonnet-4-20250514".to_string()));
        assert_eq!(parsed.stop_reason, StopReason::EndTurn);
        assert!(parsed.intents.is_empty());
    }

    #[test]
    fn parse_anthropic_response_empty_returns_empty_content() {
        let json = serde_json::json!({"model": "claude-sonnet-4-20250514"});
        let parsed = parse_provider_response(Provider::Anthropic, &json)
            .expect("should parse even with missing content");
        assert_eq!(parsed.content, "");
    }

    #[test]
    fn parse_openai_tool_calls_response() {
        let json = serde_json::json!({
            "model": "gpt-4o",
            "choices": [{
                "message": {
                    "content": "Let me read that.",
                    "tool_calls": [{
                        "id": "call_abc",
                        "type": "function",
                        "function": {
                            "name": "read",
                            "arguments": "{\"path\":\"main.rs\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let parsed = parse_provider_response(Provider::OpenAI, &json)
            .expect("should parse tool_calls response");
        assert_eq!(parsed.content, "Let me read that.");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.intents.len(), 1);
        assert_eq!(parsed.intents[0].name, "read");
        assert_eq!(parsed.intents[0].id, "call_abc");
        assert_eq!(parsed.intents[0].params["path"], "main.rs");
    }

    #[test]
    fn parse_anthropic_tool_use_response() {
        let json = serde_json::json!({
            "model": "claude-sonnet-4-20250514",
            "content": [
                { "type": "text", "text": "I'll check." },
                { "type": "tool_use", "id": "toolu_1", "name": "grep",
                  "input": { "pattern": "fn main", "path": "." } }
            ],
            "stop_reason": "tool_use"
        });
        let parsed = parse_provider_response(Provider::Anthropic, &json)
            .expect("should parse tool_use response");
        assert_eq!(parsed.content, "I'll check.");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.intents.len(), 1);
        assert_eq!(parsed.intents[0].name, "grep");
    }

    #[test]
    fn parse_ollama_tool_calls_response() {
        let json = serde_json::json!({
            "model": "llama3.2",
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "exec",
                        "arguments": { "cmd": "cargo test" }
                    }
                }]
            },
            "done": true
        });
        let parsed = parse_provider_response(Provider::Ollama, &json)
            .expect("should parse ollama tool_calls");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.intents.len(), 1);
        assert_eq!(parsed.intents[0].name, "exec");
        assert_eq!(parsed.intents[0].params["cmd"], "cargo test");
    }

    // ── Tools JSON builder tests ────────────────────────────────────────

    #[test]
    fn build_tools_openai_format_structure() {
        let registry = IntentRegistry::new();
        let tools = build_tools_openai_format(registry.all());
        let arr = tools.as_array().expect("should be array");
        assert_eq!(arr.len(), 8, "8 builtin tools");

        let first = &arr[0];
        assert_eq!(first["type"], "function");
        assert_eq!(first["function"]["name"], "read");
        assert!(first["function"]["description"].is_string());
        assert!(first["function"]["parameters"].is_object());
    }

    #[test]
    fn build_tools_anthropic_format_structure() {
        let registry = IntentRegistry::new();
        let tools = build_tools_anthropic_format(registry.all());
        let arr = tools.as_array().expect("should be array");
        assert_eq!(arr.len(), 8);

        let first = &arr[0];
        assert_eq!(first["name"], "read");
        assert!(first["description"].is_string());
        assert!(first["input_schema"].is_object());
        // Should NOT have "type": "function" wrapper
        assert!(first.get("type").is_none());
    }

    #[test]
    fn stop_reason_to_str_values() {
        assert_eq!(stop_reason_to_str(&StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason_to_str(&StopReason::ToolUse), "tool_use");
        assert_eq!(stop_reason_to_str(&StopReason::MaxTokens), "max_tokens");
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
        assert_eq!(history[0].role, Role::User);
        assert_eq!(history[0].content.text(), Some("hello"));
        assert_eq!(history[1].role, Role::Assistant);
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };

        let msgs = build_messages(&lua, &sid, "hi", &opts);
        assert_eq!(msgs.len(), 2, "system + user");
        assert_eq!(msgs[0].role, Role::System);
        assert_eq!(msgs[0].content.text(), Some("Be helpful."));
        assert_eq!(msgs[1].role, Role::User);
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };

        let msgs = build_messages(&lua, &sid, "hi", &opts);
        // Anthropic: system prompt NOT added to messages (handled at request body level)
        assert_eq!(msgs.len(), 1, "only user message for Anthropic");
        assert_eq!(msgs[0].role, Role::User);
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
            tools: true,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };

        let msgs = build_messages(&lua, &sid, "second question", &opts);
        // History has 2 messages + 1 new user message = 3
        // (system_prompt NOT added because history is non-empty)
        assert_eq!(msgs.len(), 3, "history(2) + new user(1)");
        assert_eq!(msgs[0].role, Role::User);
        assert_eq!(msgs[0].content.text(), Some("first question"));
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[1].content.text(), Some("first answer"));
        assert_eq!(msgs[2].role, Role::User);
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

    // ── Resolve-flow unit tests ───────────────────────────────────────

    #[test]
    fn build_assistant_content_blocks_text_only() {
        let parsed = ParsedLlmResponse {
            content: "Hello".to_string(),
            model: None,
            stop_reason: StopReason::EndTurn,
            intents: vec![],
        };
        let blocks = build_assistant_content_blocks(&parsed);
        assert_eq!(
            blocks.text().expect("should have text"),
            "Hello",
            "text-only response should produce text block"
        );
    }

    #[test]
    fn build_assistant_content_blocks_with_intents() {
        use orcs_types::intent::ContentBlock;

        let parsed = ParsedLlmResponse {
            content: "Let me read that file.".to_string(),
            model: None,
            stop_reason: StopReason::ToolUse,
            intents: vec![ActionIntent {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                params: serde_json::json!({"path": "/tmp/test.txt"}),
                meta: Default::default(),
            }],
        };
        let blocks = build_assistant_content_blocks(&parsed);
        match blocks {
            MessageContent::Blocks(ref b) => {
                assert_eq!(b.len(), 2, "should have text + tool_use blocks");
                assert!(
                    matches!(&b[0], ContentBlock::Text { text } if text == "Let me read that file."),
                    "first block should be text"
                );
                assert!(
                    matches!(&b[1], ContentBlock::ToolUse { id, name, .. } if id == "call_1" && name == "read_file"),
                    "second block should be tool_use"
                );
            }
            _ => panic!("expected Blocks variant"),
        }
    }

    #[test]
    fn build_assistant_content_blocks_empty_content_with_intent() {
        let parsed = ParsedLlmResponse {
            content: String::new(),
            model: None,
            stop_reason: StopReason::ToolUse,
            intents: vec![ActionIntent {
                id: "call_1".to_string(),
                name: "list_files".to_string(),
                params: serde_json::json!({}),
                meta: Default::default(),
            }],
        };
        let blocks = build_assistant_content_blocks(&parsed);
        match blocks {
            MessageContent::Blocks(ref b) => {
                assert_eq!(
                    b.len(),
                    1,
                    "should have only tool_use block (no empty text)"
                );
            }
            _ => panic!("expected Blocks variant"),
        }
    }

    #[test]
    fn build_lua_result_basic() {
        let lua = Lua::new();
        let parsed = ParsedLlmResponse {
            content: "Hello world".to_string(),
            model: Some("test-model".to_string()),
            stop_reason: StopReason::EndTurn,
            intents: vec![],
        };
        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
            api_key: None,
            system_prompt: None,
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: DEFAULT_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            tools: false,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let result = build_lua_result(&lua, &parsed, &opts, "sess-test")
            .expect("build_lua_result should succeed");
        assert!(result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("content").expect("get content"),
            "Hello world"
        );
        assert_eq!(
            result.get::<String>("model").expect("get model"),
            "test-model"
        );
        assert_eq!(
            result.get::<String>("session_id").expect("get session_id"),
            "sess-test"
        );
        assert_eq!(
            result
                .get::<String>("stop_reason")
                .expect("get stop_reason"),
            "end_turn"
        );
    }

    #[test]
    fn build_lua_result_with_intents() {
        let lua = Lua::new();
        let parsed = ParsedLlmResponse {
            content: "".to_string(),
            model: None,
            stop_reason: StopReason::ToolUse,
            intents: vec![
                ActionIntent {
                    id: "c1".to_string(),
                    name: "tool_a".to_string(),
                    params: serde_json::json!({"x": 1}),
                    meta: Default::default(),
                },
                ActionIntent {
                    id: "c2".to_string(),
                    name: "tool_b".to_string(),
                    params: serde_json::json!({}),
                    meta: Default::default(),
                },
            ],
        };
        let opts = LlmOpts {
            provider: Provider::Ollama,
            base_url: "http://localhost:11434".to_string(),
            model: "llama3.2".to_string(),
            api_key: None,
            system_prompt: None,
            session_id: None,
            temperature: None,
            max_tokens: None,
            timeout: DEFAULT_TIMEOUT_SECS,
            max_retries: DEFAULT_MAX_RETRIES,
            tools: false,
            resolve: false,
            max_tool_turns: DEFAULT_MAX_TOOL_TURNS,
        };
        let result = build_lua_result(&lua, &parsed, &opts, "sess-test")
            .expect("build_lua_result should succeed");
        let intents: Table = result.get("intents").expect("should have intents");
        assert_eq!(intents.raw_len(), 2, "should have 2 intents");

        let first: Table = intents.get(1).expect("get first intent");
        assert_eq!(first.get::<String>("id").expect("id"), "c1");
        assert_eq!(first.get::<String>("name").expect("name"), "tool_a");
    }

    #[test]
    fn lua_value_to_json_primitives() {
        assert_eq!(lua_value_to_json(mlua::Value::Nil), serde_json::Value::Null);
        assert_eq!(
            lua_value_to_json(mlua::Value::Boolean(true)),
            serde_json::Value::Bool(true)
        );
        assert_eq!(
            lua_value_to_json(mlua::Value::Integer(42)),
            serde_json::json!(42)
        );
        assert_eq!(
            lua_value_to_json(mlua::Value::Number(1.23)),
            serde_json::json!(1.23)
        );
    }

    #[test]
    fn lua_value_to_json_table_as_object() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set("key", "value").expect("set key");
        let json = lua_value_to_json(mlua::Value::Table(tbl));
        assert_eq!(json, serde_json::json!({"key": "value"}));
    }

    #[test]
    fn lua_value_to_json_table_as_array() {
        let lua = Lua::new();
        let tbl = lua.create_table().expect("create table");
        tbl.set(1, "a").expect("set 1");
        tbl.set(2, "b").expect("set 2");
        let json = lua_value_to_json(mlua::Value::Table(tbl));
        assert_eq!(json, serde_json::json!(["a", "b"]));
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

    #[test]
    fn append_message_stores_in_session() {
        let lua = Lua::new();
        ensure_session_store(&lua);
        let session_id = "test-session";

        // Create session
        if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
            store.0.insert(session_id.to_string(), Vec::new());
            lua.set_app_data(store);
        }

        // Append a text message
        append_message(
            &lua,
            session_id,
            Role::User,
            MessageContent::Text("hello".to_string()),
        );

        // Verify
        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        let history = store.0.get(session_id).expect("session should exist");
        assert_eq!(history.len(), 1, "should have 1 message");
        assert_eq!(history[0].role, Role::User);
        assert_eq!(
            history[0].content.text().expect("should have text"),
            "hello"
        );
    }

    #[test]
    fn append_message_stores_blocks() {
        use orcs_types::intent::ContentBlock;

        let lua = Lua::new();
        ensure_session_store(&lua);
        let session_id = "test-blocks";

        if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
            store.0.insert(session_id.to_string(), Vec::new());
            lua.set_app_data(store);
        }

        let blocks = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "thinking".to_string(),
            },
            ContentBlock::ToolUse {
                id: "c1".to_string(),
                name: "read_file".to_string(),
                input: serde_json::json!({"path": "/tmp/f"}),
            },
        ]);
        append_message(&lua, session_id, Role::Assistant, blocks);

        let store = lua
            .app_data_ref::<SessionStore>()
            .expect("store should exist");
        let history = store.0.get(session_id).expect("session should exist");
        assert_eq!(history.len(), 1);
        match &history[0].content {
            MessageContent::Blocks(b) => assert_eq!(b.len(), 2, "should have 2 content blocks"),
            _ => panic!("expected Blocks variant"),
        }
    }

    // ── message_to_openai_wire tests ──────────────────────────────────

    #[test]
    fn openai_wire_text_message() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
        };
        let wire = message_to_openai_wire(&msg, true);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "hello");
    }

    #[test]
    fn openai_wire_assistant_tool_calls() {
        use orcs_types::intent::ContentBlock;

        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::Text {
                    text: "Let me read that.".to_string(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "src/main.rs"}),
                },
            ]),
        };

        // OpenAI format: stringify_args = true
        let wire = message_to_openai_wire(&msg, true);
        assert_eq!(wire.len(), 1, "should produce 1 assistant message");
        assert_eq!(wire[0]["role"], "assistant");
        assert_eq!(wire[0]["content"], "Let me read that.");

        let tool_calls = wire[0]["tool_calls"]
            .as_array()
            .expect("should have tool_calls");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0]["id"], "call_1");
        assert_eq!(tool_calls[0]["type"], "function");
        assert_eq!(tool_calls[0]["function"]["name"], "read");
        // OpenAI: arguments is a JSON string
        let args_str = tool_calls[0]["function"]["arguments"]
            .as_str()
            .expect("args should be string");
        let args_parsed: serde_json::Value =
            serde_json::from_str(args_str).expect("should parse args");
        assert_eq!(args_parsed["path"], "src/main.rs");
    }

    #[test]
    fn ollama_wire_assistant_tool_calls_object_args() {
        use orcs_types::intent::ContentBlock;

        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![ContentBlock::ToolUse {
                id: "call_1".to_string(),
                name: "exec".to_string(),
                input: serde_json::json!({"cmd": "ls"}),
            }]),
        };

        // Ollama format: stringify_args = false
        let wire = message_to_openai_wire(&msg, false);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "assistant");
        // content should be null when no text
        assert!(wire[0]["content"].is_null());

        let tool_calls = wire[0]["tool_calls"]
            .as_array()
            .expect("should have tool_calls");
        // Ollama: arguments is a JSON object (not a string)
        assert!(
            tool_calls[0]["function"]["arguments"].is_object(),
            "Ollama args should be an object, got: {}",
            tool_calls[0]["function"]["arguments"]
        );
        assert_eq!(tool_calls[0]["function"]["arguments"]["cmd"], "ls");
    }

    #[test]
    fn openai_wire_tool_results_expand_to_separate_messages() {
        use orcs_types::intent::ContentBlock;

        let msg = Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: "fn main() {}".to_string(),
                    is_error: None,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "call_2".to_string(),
                    content: "line 1\nline 2".to_string(),
                    is_error: Some(false),
                },
            ]),
        };

        let wire = message_to_openai_wire(&msg, true);
        assert_eq!(
            wire.len(),
            2,
            "each ToolResult should become a separate message"
        );

        assert_eq!(wire[0]["role"], "tool");
        assert_eq!(wire[0]["tool_call_id"], "call_1");
        assert_eq!(wire[0]["content"], "fn main() {}");

        assert_eq!(wire[1]["role"], "tool");
        assert_eq!(wire[1]["tool_call_id"], "call_2");
        assert_eq!(wire[1]["content"], "line 1\nline 2");
    }

    #[test]
    fn openai_wire_multiple_tool_calls() {
        use orcs_types::intent::ContentBlock;

        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![
                ContentBlock::ToolUse {
                    id: "c1".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "a.rs"}),
                },
                ContentBlock::ToolUse {
                    id: "c2".to_string(),
                    name: "read".to_string(),
                    input: serde_json::json!({"path": "b.rs"}),
                },
            ]),
        };

        let wire = message_to_openai_wire(&msg, true);
        assert_eq!(wire.len(), 1, "all tool_calls in one message");
        let calls = wire[0]["tool_calls"].as_array().expect("tool_calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "read");
        assert_eq!(calls[1]["function"]["name"], "read");
    }

    #[test]
    fn openai_wire_empty_blocks_fallback() {
        let msg = Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![]),
        };
        let wire = message_to_openai_wire(&msg, true);
        assert_eq!(wire.len(), 1, "empty blocks should produce fallback");
        assert_eq!(wire[0]["role"], "assistant");
    }

    // ── dispatch_intents_to_results Internal tool test ─────────────────

    #[test]
    fn dispatch_intents_serializes_internal_tool_result() {
        use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
        use std::sync::Arc;

        // Create sandbox
        let dir = std::env::temp_dir().join(format!(
            "orcs-dispatch-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let dir = dir.canonicalize().expect("canonicalize");
        let sandbox = ProjectSandbox::new(&dir).expect("test sandbox");
        let sandbox: Arc<dyn SandboxPolicy> = Arc::new(sandbox);

        // Set up Lua with orcs base functions + dispatch
        let lua = Lua::new();
        crate::orcs_helpers::register_base_orcs_functions(&lua, sandbox)
            .expect("register base functions");

        // Write a test file so orcs.read returns real data
        std::fs::write(dir.join("test.txt"), "hello from dispatch test").expect("write test file");

        let intents = vec![ActionIntent {
            id: "call_test".to_string(),
            name: "read".to_string(),
            params: serde_json::json!({"path": dir.join("test.txt").to_string_lossy().to_string()}),
            meta: Default::default(),
        }];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        match result {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                match &blocks[0] {
                    orcs_types::intent::ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "call_test");
                        assert_eq!(*is_error, Some(false), "should succeed");
                        // Should contain actual file content, not just "ok"
                        assert!(
                            content.contains("hello from dispatch test"),
                            "result should contain file content, got: {content}"
                        );
                    }
                    other => panic!("expected ToolResult, got: {other:?}"),
                }
            }
            other => panic!("expected Blocks, got: {other:?}"),
        }
    }
}

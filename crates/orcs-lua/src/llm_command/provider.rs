//! LLM provider abstraction and request body builders.
//!
//! # Design Decision: OpenAI API Format Unification
//!
//! All OpenAI-compatible servers (OpenAI, Ollama, llama.cpp, vLLM, LM Studio)
//! are handled via a single `OpenAICompat` variant using the standard
//! `/v1/chat/completions` endpoint. The only difference is `base_url`.
//!
//! This design was chosen because:
//! - Ollama natively supports `/v1/chat/completions` (OpenAI-compat mode)
//! - llama.cpp server implements the same OpenAI-compatible API
//! - vLLM, SGLang, LM Studio all follow the OpenAI spec
//! - Maintaining per-server adapters (e.g. Ollama `/api/chat`) was unnecessary
//!   code surface for functionally identical behavior
//!
//! Anthropic uses a distinct wire format (`/v1/messages` with different JSON
//! schema) and is handled as a separate variant with `reqwest` directly.
//!
//! # Continuous Batching (llama.cpp / vLLM)
//!
//! These servers implement continuous batching at the inference engine level.
//! No special batch API is needed — parallel HTTP requests are automatically
//! batched internally via `--parallel N` slots. Use `tokio::spawn` /
//! `futures::join_all` for client-side parallelism.

use mlua::Lua;
use orcs_types::intent::{ContentBlock, IntentDef, Role};

use super::session::Message;
use super::{LlmOpts, ANTHROPIC_DEFAULT_MAX_TOKENS};
use crate::tool_registry::IntentRegistry;

// ── Provider ───────────────────────────────────────────────────────────

/// Supported LLM provider categories.
///
/// `OpenAICompat` covers any server implementing the OpenAI Chat Completions API:
/// - OpenAI (`https://api.openai.com`)
/// - Ollama (`http://localhost:11434`)
/// - llama.cpp server (`http://localhost:8080`)
/// - vLLM, SGLang, LM Studio, etc.
///
/// `Anthropic` uses a distinct wire format and is handled separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    /// Any server implementing the OpenAI Chat Completions API.
    OpenAICompat,
    /// Anthropic Messages API (distinct wire format).
    Anthropic,
}

impl std::str::FromStr for Provider {
    type Err = String;

    /// Parse provider from string (case-insensitive).
    ///
    /// Accepts: "openai", "ollama", "openai_compat", "anthropic".
    /// "ollama" maps to `OpenAICompat` since Ollama supports `/v1/chat/completions`.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "ollama" | "openai" | "openai_compat" => Ok(Self::OpenAICompat),
            "anthropic" => Ok(Self::Anthropic),
            other => Err(format!(
                "unsupported provider: '{}' (expected: ollama, openai, openai_compat, anthropic)",
                other
            )),
        }
    }
}

impl Provider {
    /// Stable string identifier for this provider (used in Lua result tables).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAICompat => "openai_compat",
            Self::Anthropic => "anthropic",
        }
    }

    /// Default base URL for this provider.
    ///
    /// `OpenAICompat` defaults to Ollama's local URL. For OpenAI or other
    /// remote servers, set `base_url` explicitly or use `ORCS_LLM_BASE_URL`.
    pub fn default_base_url(self) -> &'static str {
        match self {
            Self::OpenAICompat => "http://localhost:11434",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }

    /// Default model for this provider.
    pub fn default_model(self) -> &'static str {
        match self {
            Self::OpenAICompat => "llama3.2",
            Self::Anthropic => "claude-sonnet-4-20250514",
        }
    }

    /// Chat API path for this provider.
    pub fn chat_path(self) -> &'static str {
        match self {
            Self::OpenAICompat => "/v1/chat/completions",
            Self::Anthropic => "/v1/messages",
        }
    }

    /// Environment variable name for the API key.
    ///
    /// For `OpenAICompat`, checks `OPENAI_API_KEY`. Local servers (Ollama,
    /// llama.cpp) simply won't have this set, which is fine — the key is
    /// sent only when present.
    pub fn api_key_env(self) -> Option<&'static str> {
        match self {
            Self::OpenAICompat => Some("OPENAI_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
        }
    }

    /// Health-check path for this provider.
    ///
    /// - OpenAICompat: `GET /v1/models` (standard OpenAI endpoint)
    /// - Anthropic: `POST /v1/messages` (no dedicated health endpoint)
    pub fn health_path(self) -> &'static str {
        match self {
            Self::OpenAICompat => "/v1/models",
            Self::Anthropic => "/v1/messages",
        }
    }
}

// ── Request Body Builders ──────────────────────────────────────────────

/// Build the JSON request body for the given provider.
pub(super) fn build_request_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    match opts.provider {
        Provider::OpenAICompat => build_openai_body(opts, messages, tools),
        Provider::Anthropic => build_anthropic_body(opts, messages, tools),
    }
}

/// Convert a single internal Message to OpenAI wire format.
///
/// A single Message may expand to multiple wire messages:
/// - `MessageContent::Blocks` with `ToolUse` → assistant message with `tool_calls` array
/// - `MessageContent::Blocks` with `ToolResult` → one `role: "tool"` message per result
///
/// Tool call arguments are always serialized as JSON strings per the OpenAI spec.
fn message_to_openai_wire(m: &Message) -> Vec<serde_json::Value> {
    use orcs_types::intent::MessageContent;

    match &m.content {
        MessageContent::Text(s) => {
            vec![serde_json::json!({"role": m.role, "content": s})]
        }
        MessageContent::Blocks(blocks) => {
            let classified = classify_content_blocks(blocks);
            blocks_to_wire_messages(&classified, &m.role)
        }
    }
}

/// Classified content blocks for wire format conversion.
struct ClassifiedBlocks<'a> {
    text_parts: Vec<&'a str>,
    tool_uses: Vec<(&'a str, &'a str, &'a serde_json::Value)>,
    tool_results: Vec<(&'a str, &'a str)>,
}

/// Partition content blocks into text, tool_use, and tool_result groups.
fn classify_content_blocks(blocks: &[ContentBlock]) -> ClassifiedBlocks<'_> {
    let mut text_parts = Vec::new();
    let mut tool_uses = Vec::new();
    let mut tool_results = Vec::new();

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

    ClassifiedBlocks {
        text_parts,
        tool_uses,
        tool_results,
    }
}

/// Convert classified blocks into OpenAI wire-format JSON messages.
fn blocks_to_wire_messages(cb: &ClassifiedBlocks<'_>, role: &Role) -> Vec<serde_json::Value> {
    let mut msgs = Vec::new();

    // Assistant message with tool_calls
    if !cb.tool_uses.is_empty() {
        let content_val = if cb.text_parts.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::String(cb.text_parts.join(""))
        };

        let calls: Vec<serde_json::Value> = cb
            .tool_uses
            .iter()
            .map(|(id, name, input)| build_openai_tool_call(id, name, input))
            .collect();

        msgs.push(serde_json::json!({
            "role": "assistant",
            "content": content_val,
            "tool_calls": calls,
        }));
    }

    // Tool results: one message per result with role "tool"
    for (tool_use_id, content) in &cb.tool_results {
        msgs.push(serde_json::json!({
            "role": Role::Tool,
            "tool_call_id": tool_use_id,
            "content": content,
        }));
    }

    // Text-only blocks (no tool_use or tool_result)
    if cb.tool_uses.is_empty() && cb.tool_results.is_empty() {
        let text = cb.text_parts.join("");
        msgs.push(serde_json::json!({"role": role, "content": text}));
    }

    if msgs.is_empty() {
        msgs.push(serde_json::json!({"role": role, "content": ""}));
    }

    msgs
}

/// Build a single OpenAI-format tool_call entry.
///
/// Arguments are always serialized as a JSON string (OpenAI spec).
/// OpenAI-compat servers (Ollama, llama.cpp, vLLM) accept this format.
fn build_openai_tool_call(id: &str, name: &str, input: &serde_json::Value) -> serde_json::Value {
    let args_str = serde_json::to_string(input).unwrap_or_else(|e| {
        tracing::warn!(
            "failed to serialize tool_call arguments for '{}' (id={}): {e}, falling back to {{}}",
            name,
            id
        );
        "{}".to_string()
    });
    serde_json::json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": args_str,
        }
    })
}

fn build_openai_body(
    opts: &LlmOpts,
    messages: &[Message],
    tools: Option<&serde_json::Value>,
) -> Result<serde_json::Value, String> {
    let msgs: Vec<serde_json::Value> = messages.iter().flat_map(message_to_openai_wire).collect();

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
            let content_val = serde_json::to_value(&m.content).map_err(|e| {
                format!(
                    "failed to serialize MessageContent for Anthropic (role={:?}): {e}",
                    m.role
                )
            })?;
            Ok(serde_json::json!({
                "role": m.role,
                "content": content_val,
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;

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
pub(super) fn build_tools_for_provider(lua: &Lua, provider: Provider) -> Option<serde_json::Value> {
    let registry = match lua.app_data_ref::<IntentRegistry>() {
        Some(r) => r,
        None => {
            tracing::warn!("build_tools_for_provider: IntentRegistry not found in app_data");
            return None;
        }
    };
    if registry.is_empty() {
        tracing::warn!("build_tools_for_provider: IntentRegistry is empty");
        return None;
    }
    let defs = registry.all();
    tracing::debug!(
        "build_tools_for_provider: {} intents for {:?}",
        defs.len(),
        provider
    );
    let tools = match provider {
        Provider::Anthropic => build_tools_anthropic_format(defs),
        Provider::OpenAICompat => build_tools_openai_format(defs),
    };
    Some(tools)
}

/// Build tools array in OpenAI format.
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

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::{DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOOL_TURNS};
    use super::*;
    use orcs_types::intent::MessageContent;

    // ── Provider unit tests ────────────────────────────────────────────

    #[test]
    fn provider_from_str_valid() {
        assert_eq!(
            "ollama"
                .parse::<Provider>()
                .expect("should parse ollama as OpenAICompat"),
            Provider::OpenAICompat
        );
        assert_eq!(
            "OpenAI"
                .parse::<Provider>()
                .expect("should parse OpenAI as OpenAICompat"),
            Provider::OpenAICompat
        );
        assert_eq!(
            "openai_compat"
                .parse::<Provider>()
                .expect("should parse openai_compat"),
            Provider::OpenAICompat
        );
        assert_eq!(
            "ANTHROPIC"
                .parse::<Provider>()
                .expect("should parse ANTHROPIC"),
            Provider::Anthropic
        );
    }

    #[test]
    fn provider_from_str_invalid() {
        let err = "gemini"
            .parse::<Provider>()
            .expect_err("should reject gemini");
        assert!(
            err.contains("unsupported provider"),
            "error should mention unsupported, got: {}",
            err
        );
    }

    #[test]
    fn provider_defaults() {
        assert_eq!(
            Provider::OpenAICompat.default_base_url(),
            "http://localhost:11434"
        );
        assert_eq!(
            Provider::Anthropic.default_base_url(),
            "https://api.anthropic.com"
        );

        assert_eq!(Provider::OpenAICompat.default_model(), "llama3.2");
        assert_eq!(
            Provider::Anthropic.default_model(),
            "claude-sonnet-4-20250514"
        );

        // All OpenAI-compat providers use the same standard path
        assert_eq!(Provider::OpenAICompat.chat_path(), "/v1/chat/completions");
        assert_eq!(Provider::Anthropic.chat_path(), "/v1/messages");
    }

    #[test]
    fn provider_api_key_env() {
        assert_eq!(Provider::OpenAICompat.api_key_env(), Some("OPENAI_API_KEY"));
        assert_eq!(Provider::Anthropic.api_key_env(), Some("ANTHROPIC_API_KEY"));
    }

    // ── Request body builder tests ─────────────────────────────────────

    #[test]
    fn build_openai_body_basic() {
        let opts = LlmOpts {
            provider: Provider::OpenAICompat,
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

        let body = build_openai_body(&opts, &messages, None).expect("should build body");
        assert_eq!(body["model"], "llama3.2");
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][0]["content"], "hello");
        // No Ollama-specific "options" key — unified format
        assert!(
            body.get("options").is_none(),
            "should not have Ollama 'options' key"
        );
    }

    #[test]
    fn build_openai_body_with_options() {
        let opts = LlmOpts {
            provider: Provider::OpenAICompat,
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

        let body = build_openai_body(&opts, &messages, None).expect("should build body");
        // Standard OpenAI fields (not Ollama options.num_predict)
        assert_eq!(body["temperature"], 0.7);
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn build_openai_body_openai_server() {
        let opts = LlmOpts {
            provider: Provider::OpenAICompat,
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

    // ── message_to_openai_wire tests ──────────────────────────────────

    #[test]
    fn openai_wire_text_message() {
        let msg = Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
        };
        let wire = message_to_openai_wire(&msg);
        assert_eq!(wire.len(), 1);
        assert_eq!(wire[0]["role"], "user");
        assert_eq!(wire[0]["content"], "hello");
    }

    #[test]
    fn openai_wire_assistant_tool_calls() {
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

        let wire = message_to_openai_wire(&msg);
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
        // Arguments are always a JSON string (OpenAI standard)
        let args_str = tool_calls[0]["function"]["arguments"]
            .as_str()
            .expect("args should be string per OpenAI spec");
        let args_parsed: serde_json::Value =
            serde_json::from_str(args_str).expect("should parse args");
        assert_eq!(args_parsed["path"], "src/main.rs");
    }

    #[test]
    fn openai_wire_tool_results_expand_to_separate_messages() {
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

        let wire = message_to_openai_wire(&msg);
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

        let wire = message_to_openai_wire(&msg);
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
        let wire = message_to_openai_wire(&msg);
        assert_eq!(wire.len(), 1, "empty blocks should produce fallback");
        assert_eq!(wire[0]["role"], "assistant");
    }
}

use mlua::{Lua, Table};
use orcs_types::intent::{ActionIntent, ContentBlock, MessageContent, StopReason};

use super::provider::Provider;
use super::retry::{classify_http_status, read_body_limited, truncate_for_error, ReadBodyError};
use super::{LlmOpts, MAX_BODY_SIZE};
use crate::llm_adapter;
use crate::types::serde_json_to_lua;

// ── Parsed LLM Response ─────────────────────────────────────────────

/// Provider-agnostic parsed LLM response.
pub(super) struct ParsedLlmResponse {
    /// Text content extracted from the response.
    pub content: String,
    /// Model name from the response (may differ from requested model).
    pub model: Option<String>,
    /// Normalized stop reason.
    pub stop_reason: StopReason,
    /// Tool-use intents extracted from the response (empty if text-only).
    pub intents: Vec<ActionIntent>,
}

/// Parse a provider response JSON into a unified `ParsedLlmResponse`.
pub(super) fn parse_provider_response(
    provider: Provider,
    json: &serde_json::Value,
) -> Result<ParsedLlmResponse, String> {
    let blocks = match provider.wire_format() {
        super::provider::WireFormat::OpenAI => llm_adapter::extract_content_openai(json),
        super::provider::WireFormat::Anthropic => llm_adapter::extract_content_anthropic(json),
    };

    let stop_reason = match provider.wire_format() {
        super::provider::WireFormat::OpenAI => llm_adapter::extract_stop_reason_openai(json),
        super::provider::WireFormat::Anthropic => llm_adapter::extract_stop_reason_anthropic(json),
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
pub(super) fn stop_reason_to_str(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::ToolUse => "tool_use",
        StopReason::MaxTokens => "max_tokens",
    }
}

// ── Response Parsers ───────────────────────────────────────────────────

/// Either a parsed response or an error Lua table.
pub(super) enum ResponseOrError {
    Parsed(ParsedLlmResponse),
    ErrorTable(Table),
}

/// Parse HTTP response body into ParsedLlmResponse or error table.
///
/// Uses [`read_body_limited`] to enforce `MAX_BODY_SIZE` during streaming,
/// preventing unbounded memory allocation from oversized responses.
pub(super) fn parse_response_body(
    lua: &Lua,
    resp: reqwest::Response,
    opts: &LlmOpts,
    session_id: &str,
) -> mlua::Result<ResponseOrError> {
    let status = resp.status().as_u16();

    // Read body with streaming size limit
    let body_text = match read_body_limited(resp, MAX_BODY_SIZE) {
        Ok(text) => text,
        Err(ReadBodyError::TooLarge) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", "response body exceeds size limit")?;
            result.set("error_kind", "too_large")?;
            result.set("session_id", session_id)?;
            return Ok(ResponseOrError::ErrorTable(result));
        }
        Err(ReadBodyError::InvalidUtf8) => {
            return Err(mlua::Error::RuntimeError(
                "LLM response body is not valid UTF-8".into(),
            ));
        }
        Err(ReadBodyError::NoRuntime) => {
            return Err(mlua::Error::RuntimeError(
                "no tokio runtime available for reading response body".into(),
            ));
        }
        Err(ReadBodyError::Network(msg)) => {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", format!("failed to read response body: {}", msg))?;
            result.set("error_kind", "network")?;
            result.set("session_id", session_id)?;
            return Ok(ResponseOrError::ErrorTable(result));
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
pub(super) fn build_assistant_content_blocks(parsed: &ParsedLlmResponse) -> MessageContent {
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
pub(crate) fn dispatch_intents_to_results(
    lua: &Lua,
    intents: &[ActionIntent],
) -> mlua::Result<MessageContent> {
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
                            crate::types::lua_to_json(v, lua)
                                .and_then(|j| {
                                    serde_json::to_string(&j)
                                        .map_err(|e| mlua::Error::SerializeError(e.to_string()))
                                })
                                .unwrap_or_else(|_| "{}".to_string())
                        }
                        None => {
                            // No "data" field: serialize the whole result table so
                            // the LLM receives actual tool output (file content, etc.)
                            crate::types::lua_to_json(mlua::Value::Table(tbl), lua)
                                .and_then(|j| {
                                    serde_json::to_string(&j)
                                        .map_err(|e| mlua::Error::SerializeError(e.to_string()))
                                })
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
            Err(e) => {
                // Propagate Suspended so ChannelRunner can drive HIL approval.
                if crate::is_suspended_error(&e) {
                    return Err(e);
                }
                (format!("dispatch error: {e}"), true)
            }
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
pub(super) fn build_lua_result(
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

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::{DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOOL_TURNS, DEFAULT_TIMEOUT_SECS};
    use super::*;

    /// Ollama `/v1/chat/completions` returns OpenAI-compatible format.
    #[test]
    fn parse_ollama_response_success() {
        let json = serde_json::json!({
            "model": "llama3.2",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help?"
                },
                "finish_reason": "stop"
            }]
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
    fn parse_ollama_empty_returns_empty_content() {
        let json = serde_json::json!({"model": "llama3.2"});
        let parsed = parse_provider_response(Provider::Ollama, &json)
            .expect("should parse even with missing choices");
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

    /// Ollama tool_calls via OpenAI-compatible wire format.
    #[test]
    fn parse_ollama_tool_calls_response() {
        let json = serde_json::json!({
            "model": "llama3.2",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_001",
                        "type": "function",
                        "function": {
                            "name": "exec",
                            "arguments": "{\"cmd\":\"cargo test\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let parsed = parse_provider_response(Provider::Ollama, &json)
            .expect("should parse ollama tool_calls");
        assert_eq!(parsed.stop_reason, StopReason::ToolUse);
        assert_eq!(parsed.intents.len(), 1);
        assert_eq!(parsed.intents[0].name, "exec");
        assert_eq!(parsed.intents[0].params["cmd"], "cargo test");
    }

    #[test]
    fn stop_reason_to_str_values() {
        assert_eq!(stop_reason_to_str(&StopReason::EndTurn), "end_turn");
        assert_eq!(stop_reason_to_str(&StopReason::ToolUse), "tool_use");
        assert_eq!(stop_reason_to_str(&StopReason::MaxTokens), "max_tokens");
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
                    ContentBlock::ToolResult {
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

    // ── dispatch_intents_to_results Component resolver tests ─────────

    /// Helper: set up a Lua VM with base orcs functions, dispatch functions,
    /// and a mock `orcs.request` that returns pre-configured responses.
    ///
    /// The mock intercepts `orcs.request(target, op, payload)` calls from
    /// `dispatch_component` and returns a Lua table matching the RPC contract.
    fn setup_lua_with_component_mock(
        mock_success: bool,
        mock_data: serde_json::Value,
        mock_error: &str,
    ) -> Lua {
        use mlua::LuaSerdeExt;
        use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
        use std::sync::Arc;

        let sandbox = ProjectSandbox::new(".").expect("test sandbox");
        let sandbox: Arc<dyn SandboxPolicy> = Arc::new(sandbox);

        let lua = Lua::new();
        crate::orcs_helpers::register_base_orcs_functions(&lua, sandbox)
            .expect("register base functions");
        crate::tool_registry::register_dispatch_functions(&lua)
            .expect("register dispatch functions");

        // Override orcs.request with mock
        let mock_data = Arc::new(mock_data);
        let mock_error = Arc::new(mock_error.to_string());
        let mock_fn = lua
            .create_function(
                move |lua, (_target, _op, _payload): (String, String, mlua::Value)| {
                    let result = lua.create_table()?;
                    result.set("success", mock_success)?;
                    if mock_success {
                        let lua_val = lua.to_value(mock_data.as_ref())?;
                        result.set("data", lua_val)?;
                    } else {
                        result.set("error", mock_error.as_str())?;
                    }
                    Ok(result)
                },
            )
            .expect("create mock request function");

        let orcs: Table = lua.globals().get("orcs").expect("orcs table");
        orcs.set("request", mock_fn).expect("set mock orcs.request");

        lua
    }

    /// Helper: extract a single ToolResult from dispatch result.
    fn expect_single_tool_result(result: MessageContent) -> (String, String, Option<bool>) {
        match result {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1, "expected exactly 1 block");
                match blocks.into_iter().next().expect("block exists") {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => (tool_use_id, content, is_error),
                    other => panic!("expected ToolResult, got: {other:?}"),
                }
            }
            other => panic!("expected Blocks, got: {other:?}"),
        }
    }

    #[test]
    fn dispatch_intents_routes_component_to_tool_result() {
        // Mock skill_manager returning skill body data
        let mock_response = serde_json::json!({
            "name": "hello-world",
            "body": "Say hello to the user warmly.",
            "description": "A greeting skill"
        });
        let lua = setup_lua_with_component_mock(true, mock_response, "");

        // Register a Component intent via Lua
        let orcs: Table = lua.globals().get("orcs").expect("orcs table");
        let register_fn: mlua::Function = orcs.get("register_intent").expect("register_intent fn");
        let def = lua.create_table().expect("create def table");
        def.set("name", "hello-world").expect("set name");
        def.set(
            "description",
            "A greeting skill [Skill: invoke to retrieve full instructions]",
        )
        .expect("set description");
        def.set("component", "skill::skill_manager")
            .expect("set component");
        def.set("operation", "execute").expect("set operation");
        let reg_result: Table = register_fn.call(def).expect("register_intent call");
        assert!(
            reg_result.get::<bool>("ok").expect("get ok"),
            "register_intent should succeed"
        );

        // Dispatch intent
        let intents = vec![ActionIntent {
            id: "tool_call_1".to_string(),
            name: "hello-world".to_string(),
            params: serde_json::json!({"name": "hello-world"}),
            meta: Default::default(),
        }];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        let (tool_use_id, content, is_error) = expect_single_tool_result(result);
        assert_eq!(tool_use_id, "tool_call_1");
        assert_eq!(is_error, Some(false), "should not be an error");

        // Content should be JSON containing the skill body
        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("content should be valid JSON");
        assert_eq!(parsed["name"], "hello-world");
        assert_eq!(parsed["body"], "Say hello to the user warmly.");
        assert_eq!(parsed["description"], "A greeting skill");
    }

    #[test]
    fn dispatch_intents_component_error_returns_is_error() {
        let lua = setup_lua_with_component_mock(
            false,
            serde_json::Value::Null,
            "skill not found: nonexistent",
        );

        // Register a Component intent
        let orcs: Table = lua.globals().get("orcs").expect("orcs table");
        let register_fn: mlua::Function = orcs.get("register_intent").expect("register_intent fn");
        let def = lua.create_table().expect("create def table");
        def.set("name", "nonexistent-skill").expect("set name");
        def.set("description", "A missing skill")
            .expect("set description");
        def.set("component", "skill::skill_manager")
            .expect("set component");
        def.set("operation", "execute").expect("set operation");
        let reg_result: Table = register_fn.call(def).expect("register_intent call");
        assert!(
            reg_result.get::<bool>("ok").expect("get ok"),
            "register_intent should succeed"
        );

        let intents = vec![ActionIntent {
            id: "tool_err_1".to_string(),
            name: "nonexistent-skill".to_string(),
            params: serde_json::json!({"name": "nonexistent-skill"}),
            meta: Default::default(),
        }];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        let (tool_use_id, content, is_error) = expect_single_tool_result(result);
        assert_eq!(tool_use_id, "tool_err_1");
        assert_eq!(is_error, Some(true), "should be an error");
        assert!(
            content.contains("skill not found"),
            "error content should contain error message, got: {content}"
        );
    }

    #[test]
    fn register_intent_then_dispatch_roundtrip() {
        // Tests the full chain: register_intent → IntentRegistry → dispatch → Component resolver → ToolResult
        let mock_response = serde_json::json!({
            "name": "code-review",
            "body": "Review the code for correctness and style.\n1. Check error handling\n2. Verify tests",
            "description": "Code review skill"
        });
        let lua = setup_lua_with_component_mock(true, mock_response, "");

        // Register with params schema (like agent_mgr.lua does)
        let orcs: Table = lua.globals().get("orcs").expect("orcs table");
        let register_fn: mlua::Function = orcs.get("register_intent").expect("register_intent fn");

        let params = lua.create_table().expect("create params table");
        let name_param = lua.create_table().expect("create name_param");
        name_param.set("type", "string").expect("set type");
        name_param
            .set("description", "Name of the skill to execute")
            .expect("set description");
        name_param.set("required", true).expect("set required");
        params.set("name", name_param).expect("set name param");

        let def = lua.create_table().expect("create def table");
        def.set("name", "code-review").expect("set name");
        def.set("description", "Code review skill [Skill]")
            .expect("set description");
        def.set("component", "skill::skill_manager")
            .expect("set component");
        def.set("operation", "execute").expect("set operation");
        def.set("params", params).expect("set params");

        let reg_result: Table = register_fn.call(def).expect("register_intent call");
        assert!(
            reg_result.get::<bool>("ok").expect("get ok"),
            "register_intent should succeed"
        );

        // Verify intent is in registry via orcs.intent_defs()
        let intent_defs_fn: mlua::Function = orcs.get("intent_defs").expect("intent_defs fn");
        let defs: Table = intent_defs_fn.call(()).expect("intent_defs call");
        let mut found = false;
        for pair in defs.pairs::<i64, Table>() {
            let (_, entry) = pair.expect("iterate intent defs");
            let name: String = entry.get("name").expect("get name");
            if name == "code-review" {
                found = true;
                let desc: String = entry.get("description").expect("get description");
                assert!(
                    desc.contains("Code review"),
                    "description should match, got: {desc}"
                );
                break;
            }
        }
        assert!(found, "code-review should appear in intent_defs()");

        // Dispatch via dispatch_intents_to_results (the actual resolve path)
        let intents = vec![ActionIntent {
            id: "call_review".to_string(),
            name: "code-review".to_string(),
            params: serde_json::json!({"name": "code-review"}),
            meta: Default::default(),
        }];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        let (tool_use_id, content, is_error) = expect_single_tool_result(result);
        assert_eq!(tool_use_id, "call_review");
        assert_eq!(is_error, Some(false));

        let parsed: serde_json::Value =
            serde_json::from_str(&content).expect("content should be valid JSON");
        assert_eq!(parsed["name"], "code-review");
        assert!(
            parsed["body"]
                .as_str()
                .expect("body should be string")
                .contains("Review the code"),
            "body should contain skill instructions"
        );
    }

    #[test]
    fn dispatch_intents_multiple_intents_returns_multiple_tool_results() {
        use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
        use std::sync::Arc;

        // Create sandbox with a test file for Internal tool (read)
        let dir = std::env::temp_dir().join(format!(
            "orcs-multi-intent-{}-{}",
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

        std::fs::write(dir.join("a.txt"), "file_a_content").expect("write a.txt");

        let lua = Lua::new();
        crate::orcs_helpers::register_base_orcs_functions(&lua, sandbox)
            .expect("register base functions");
        crate::tool_registry::register_dispatch_functions(&lua)
            .expect("register dispatch functions");

        // Mock orcs.request for Component resolver
        lua.load(
            r#"
            orcs.request = function(target, op, payload)
                return { success = true, data = { name = "mock-skill", body = "mock body" } }
            end
            "#,
        )
        .exec()
        .expect("mock orcs.request");

        // Register a Component intent
        lua.load(
            r#"
            orcs.register_intent({
                name = "my-skill",
                description = "test skill",
                component = "skill::skill_manager",
                operation = "execute",
            })
            "#,
        )
        .exec()
        .expect("register intent");

        // Dispatch 3 intents: Internal(read) + Component(my-skill) + Internal(read)
        let intents = vec![
            ActionIntent {
                id: "call_1".to_string(),
                name: "read".to_string(),
                params: serde_json::json!({"path": dir.join("a.txt").to_string_lossy().to_string()}),
                meta: Default::default(),
            },
            ActionIntent {
                id: "call_2".to_string(),
                name: "my-skill".to_string(),
                params: serde_json::json!({"name": "my-skill"}),
                meta: Default::default(),
            },
            ActionIntent {
                id: "call_3".to_string(),
                name: "read".to_string(),
                params: serde_json::json!({"path": dir.join("a.txt").to_string_lossy().to_string()}),
                meta: Default::default(),
            },
        ];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        match result {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 3, "should have 3 ToolResult blocks");

                // Block 0: Internal read
                match &blocks[0] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "call_1");
                        assert_eq!(*is_error, Some(false));
                        assert!(
                            content.contains("file_a_content"),
                            "read result should contain file content, got: {content}"
                        );
                    }
                    other => panic!("block 0: expected ToolResult, got: {other:?}"),
                }

                // Block 1: Component skill
                match &blocks[1] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        assert_eq!(tool_use_id, "call_2");
                        assert_eq!(*is_error, Some(false));
                        let parsed: serde_json::Value =
                            serde_json::from_str(content).expect("should be valid JSON");
                        assert_eq!(parsed["name"], "mock-skill");
                        assert_eq!(parsed["body"], "mock body");
                    }
                    other => panic!("block 1: expected ToolResult, got: {other:?}"),
                }

                // Block 2: Internal read (same file)
                match &blocks[2] {
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => {
                        assert_eq!(tool_use_id, "call_3");
                        assert_eq!(*is_error, Some(false));
                    }
                    other => panic!("block 2: expected ToolResult, got: {other:?}"),
                }
            }
            other => panic!("expected Blocks, got: {other:?}"),
        }
    }

    #[test]
    fn dispatch_intents_unknown_intent_returns_is_error() {
        use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
        use std::sync::Arc;

        let sandbox = ProjectSandbox::new(".").expect("test sandbox");
        let sandbox: Arc<dyn SandboxPolicy> = Arc::new(sandbox);

        let lua = Lua::new();
        crate::orcs_helpers::register_base_orcs_functions(&lua, sandbox)
            .expect("register base functions");
        crate::tool_registry::register_dispatch_functions(&lua)
            .expect("register dispatch functions");

        // Dispatch an intent whose name is NOT in the registry
        let intents = vec![ActionIntent {
            id: "call_unknown".to_string(),
            name: "totally-unknown-tool".to_string(),
            params: serde_json::json!({"arg": "value"}),
            meta: Default::default(),
        }];

        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        let (tool_use_id, content, is_error) = expect_single_tool_result(result);
        assert_eq!(tool_use_id, "call_unknown");
        assert_eq!(is_error, Some(true), "unknown intent should be an error");
        assert!(
            content.contains("unknown intent"),
            "error should mention unknown intent, got: {content}"
        );
    }

    #[test]
    fn dispatch_intents_empty_returns_empty_blocks() {
        use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
        use std::sync::Arc;

        let sandbox = ProjectSandbox::new(".").expect("test sandbox");
        let sandbox: Arc<dyn SandboxPolicy> = Arc::new(sandbox);

        let lua = Lua::new();
        crate::orcs_helpers::register_base_orcs_functions(&lua, sandbox)
            .expect("register base functions");
        crate::tool_registry::register_dispatch_functions(&lua)
            .expect("register dispatch functions");

        let intents: Vec<ActionIntent> = vec![];
        let result = dispatch_intents_to_results(&lua, &intents).expect("dispatch should succeed");

        match result {
            MessageContent::Blocks(blocks) => {
                assert!(
                    blocks.is_empty(),
                    "empty intents should produce empty blocks"
                );
            }
            other => panic!("expected Blocks, got: {other:?}"),
        }
    }
}

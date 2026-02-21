use mlua::{Lua, Table};
use orcs_types::intent::{ActionIntent, ContentBlock, MessageContent, StopReason};

use super::provider::Provider;
use super::retry::{classify_http_status, truncate_for_error};
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
pub(super) fn parse_response_body(
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
pub(super) fn dispatch_intents_to_results(
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
}

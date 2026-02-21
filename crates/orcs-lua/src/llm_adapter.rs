//! LLM response → ActionIntent adapter.
//!
//! Converts provider-specific tool_calls responses into the unified
//! `ActionIntent` domain model. Each provider has a different wire format:
//!
//! ```text
//! OpenAI:    choices[0].message.tool_calls[].{id, function.name, function.arguments}
//! Anthropic: content[].{type: "tool_use", id, name, input}
//! Ollama:    message.tool_calls[].function.{name, arguments}
//! ```
//!
//! This module normalizes all three into `Vec<ContentBlock>` and
//! `Vec<ActionIntent>`, plus a unified `StopReason`.
//!
//! # Phase 2 Scope
//!
//! Pure data transformation — no Lua interaction, no side effects.
//! Later phases wire this into `llm_command.rs`'s response flow.

use orcs_types::intent::{ActionIntent, ContentBlock, MessageContent, StopReason};

// ── ContentBlock Extraction ─────────────────────────────────────────

/// Extract content blocks from an OpenAI chat completion response.
///
/// OpenAI format:
/// ```json
/// {
///   "choices": [{
///     "message": {
///       "content": "some text",
///       "tool_calls": [
///         { "id": "call_abc", "type": "function",
///           "function": { "name": "read", "arguments": "{\"path\":\"x\"}" } }
///       ]
///     }
///   }]
/// }
/// ```
pub fn extract_content_openai(json: &serde_json::Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    let message = match json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
    {
        Some(m) => m,
        None => return blocks,
    };

    // Text content (may be null when only tool_calls)
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    // Tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tool_calls {
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            let func = match tc.get("function") {
                Some(f) => f,
                None => continue,
            };

            let name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // OpenAI sends arguments as a JSON string, not an object
            let input = func
                .get("arguments")
                .and_then(|v| v.as_str())
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

            blocks.push(ContentBlock::ToolUse { id, name, input });
        }
    }

    blocks
}

/// Extract content blocks from an Anthropic Messages API response.
///
/// Anthropic format:
/// ```json
/// {
///   "content": [
///     { "type": "text", "text": "Let me read that file." },
///     { "type": "tool_use", "id": "toolu_abc", "name": "read",
///       "input": { "path": "x" } }
///   ]
/// }
/// ```
pub fn extract_content_anthropic(json: &serde_json::Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    let content_array = match json.get("content").and_then(|c| c.as_array()) {
        Some(arr) => arr,
        None => return blocks,
    };

    for item in content_array {
        let block_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match block_type {
            "text" => {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        blocks.push(ContentBlock::Text {
                            text: text.to_string(),
                        });
                    }
                }
            }
            "tool_use" => {
                let id = item
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = item
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                // Anthropic sends input as an object directly (not a string)
                let input = item
                    .get("input")
                    .cloned()
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                blocks.push(ContentBlock::ToolUse { id, name, input });
            }
            _ => {
                // Unknown block types ignored (future-proof)
            }
        }
    }

    blocks
}

/// Extract content blocks from an Ollama chat response.
///
/// Ollama format (OpenAI-compatible):
/// ```json
/// {
///   "message": {
///     "role": "assistant",
///     "content": "some text",
///     "tool_calls": [
///       { "function": { "name": "read", "arguments": { "path": "x" } } }
///     ]
///   }
/// }
/// ```
///
/// Note: Ollama tool_calls may omit `id` and send `arguments` as an object
/// (not a JSON string like OpenAI). This function handles both formats.
pub fn extract_content_ollama(json: &serde_json::Value) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    let message = match json.get("message") {
        Some(m) => m,
        None => return blocks,
    };

    // Text content
    if let Some(text) = message.get("content").and_then(|c| c.as_str()) {
        if !text.is_empty() {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
        }
    }

    // Tool calls
    if let Some(tool_calls) = message.get("tool_calls").and_then(|t| t.as_array()) {
        for (idx, tc) in tool_calls.iter().enumerate() {
            let func = match tc.get("function") {
                Some(f) => f,
                None => continue,
            };

            let name = func
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            // Ollama sends arguments as an object (not a string)
            // but also supports the OpenAI string format
            let input = match func.get("arguments") {
                Some(serde_json::Value::String(s)) => serde_json::from_str(s)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new())),
                Some(obj @ serde_json::Value::Object(_)) => obj.clone(),
                _ => serde_json::Value::Object(serde_json::Map::new()),
            };

            // Ollama may not provide tool call IDs; generate a synthetic one
            let id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("ollama_call_{idx}"));

            blocks.push(ContentBlock::ToolUse { id, name, input });
        }
    }

    blocks
}

// ── StopReason Extraction ───────────────────────────────────────────

/// Extract normalized `StopReason` from an OpenAI response.
///
/// OpenAI: `choices[0].finish_reason` → "stop" | "tool_calls" | "length"
pub fn extract_stop_reason_openai(json: &serde_json::Value) -> StopReason {
    let reason = json
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finish_reason"))
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    match reason {
        "tool_calls" => StopReason::ToolUse,
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn, // "stop" and unknown
    }
}

/// Extract normalized `StopReason` from an Anthropic response.
///
/// Anthropic: `stop_reason` → "end_turn" | "tool_use" | "max_tokens"
pub fn extract_stop_reason_anthropic(json: &serde_json::Value) -> StopReason {
    let reason = json
        .get("stop_reason")
        .and_then(|r| r.as_str())
        .unwrap_or("end_turn");

    match reason {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        _ => StopReason::EndTurn, // "end_turn" and unknown
    }
}

/// Extract normalized `StopReason` from an Ollama response.
///
/// Ollama: `done_reason` → "stop" | "length" (tool_calls inferred from presence)
pub fn extract_stop_reason_ollama(json: &serde_json::Value) -> StopReason {
    // Ollama signals tool use by including tool_calls in message, not via done_reason
    let has_tool_calls = json
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|t| t.as_array())
        .is_some_and(|arr| !arr.is_empty());

    if has_tool_calls {
        return StopReason::ToolUse;
    }

    let reason = json
        .get("done_reason")
        .and_then(|r| r.as_str())
        .unwrap_or("stop");

    match reason {
        "length" => StopReason::MaxTokens,
        _ => StopReason::EndTurn, // "stop" and unknown
    }
}

// ── ContentBlock → ActionIntent Conversion ──────────────────────────

/// Convert tool_use content blocks into `ActionIntent`s.
///
/// Only `ContentBlock::ToolUse` blocks are converted; text blocks are skipped.
pub fn content_blocks_to_intents(blocks: &[ContentBlock]) -> Vec<ActionIntent> {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, name, input } => {
                Some(ActionIntent::from_llm_tool_call(id, name, input.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Build a `MessageContent` from extracted content blocks.
///
/// Returns `MessageContent::Text` when there's only a single text block
/// (backward-compatible). Returns `MessageContent::Blocks` when there are
/// multiple blocks or any tool_use blocks.
pub fn blocks_to_message_content(blocks: Vec<ContentBlock>) -> MessageContent {
    if blocks.len() == 1 {
        if let ContentBlock::Text { ref text } = blocks[0] {
            return MessageContent::Text(text.clone());
        }
    }
    if blocks.is_empty() {
        return MessageContent::Text(String::new());
    }
    MessageContent::Blocks(blocks)
}

// ── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── OpenAI extraction ────────────────────────────────────────

    #[test]
    fn openai_text_only_response() {
        let json = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello!"
                },
                "finish_reason": "stop"
            }]
        });

        let blocks = extract_content_openai(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello!"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn openai_tool_calls_response() {
        let json = serde_json::json!({
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [
                        {
                            "id": "call_abc123",
                            "type": "function",
                            "function": {
                                "name": "read",
                                "arguments": "{\"path\":\"src/main.rs\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let blocks = extract_content_openai(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_abc123");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "src/main.rs");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn openai_mixed_text_and_tool_calls() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "Let me read that file.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {
                                "name": "read",
                                "arguments": "{\"path\":\"main.rs\"}"
                            }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": {
                                "name": "grep",
                                "arguments": "{\"pattern\":\"fn main\"}"
                            }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let blocks = extract_content_openai(&json);
        assert_eq!(blocks.len(), 3, "text + 2 tool_calls");

        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Let me read that file."),
            other => panic!("expected Text, got: {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "read");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
        match &blocks[2] {
            ContentBlock::ToolUse { id, name, .. } => {
                assert_eq!(id, "call_2");
                assert_eq!(name, "grep");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn openai_malformed_arguments_defaults_to_empty_object() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call_x",
                        "function": {
                            "name": "read",
                            "arguments": "not valid json"
                        }
                    }]
                }
            }]
        });

        let blocks = extract_content_openai(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert!(input.is_object(), "should default to empty object");
                assert!(input.as_object().expect("is object").is_empty());
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn openai_empty_response() {
        let json = serde_json::json!({});
        let blocks = extract_content_openai(&json);
        assert!(blocks.is_empty());
    }

    // ── Anthropic extraction ─────────────────────────────────────

    #[test]
    fn anthropic_text_only_response() {
        let json = serde_json::json!({
            "content": [
                { "type": "text", "text": "Here is the answer." }
            ],
            "stop_reason": "end_turn"
        });

        let blocks = extract_content_anthropic(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Here is the answer."),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn anthropic_tool_use_response() {
        let json = serde_json::json!({
            "content": [
                { "type": "text", "text": "I'll read that file." },
                {
                    "type": "tool_use",
                    "id": "toolu_abc123",
                    "name": "read",
                    "input": { "path": "src/main.rs" }
                }
            ],
            "stop_reason": "tool_use"
        });

        let blocks = extract_content_anthropic(&json);
        assert_eq!(blocks.len(), 2);

        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "I'll read that file."),
            other => panic!("expected Text, got: {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "toolu_abc123");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "src/main.rs");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn anthropic_multiple_tool_uses() {
        let json = serde_json::json!({
            "content": [
                {
                    "type": "tool_use",
                    "id": "toolu_1",
                    "name": "read",
                    "input": { "path": "a.rs" }
                },
                {
                    "type": "tool_use",
                    "id": "toolu_2",
                    "name": "read",
                    "input": { "path": "b.rs" }
                }
            ],
            "stop_reason": "tool_use"
        });

        let blocks = extract_content_anthropic(&json);
        assert_eq!(blocks.len(), 2);
    }

    #[test]
    fn anthropic_unknown_block_type_ignored() {
        let json = serde_json::json!({
            "content": [
                { "type": "text", "text": "hi" },
                { "type": "thinking", "thinking": "..." },
                { "type": "tool_use", "id": "t1", "name": "read", "input": {} }
            ]
        });

        let blocks = extract_content_anthropic(&json);
        assert_eq!(blocks.len(), 2, "thinking block should be skipped");
    }

    #[test]
    fn anthropic_empty_text_skipped() {
        let json = serde_json::json!({
            "content": [
                { "type": "text", "text": "" },
                { "type": "tool_use", "id": "t1", "name": "exec", "input": {"cmd": "ls"} }
            ]
        });

        let blocks = extract_content_anthropic(&json);
        assert_eq!(blocks.len(), 1, "empty text block should be skipped");
        assert!(matches!(&blocks[0], ContentBlock::ToolUse { .. }));
    }

    #[test]
    fn anthropic_empty_response() {
        let json = serde_json::json!({});
        let blocks = extract_content_anthropic(&json);
        assert!(blocks.is_empty());
    }

    // ── Ollama extraction ────────────────────────────────────────

    #[test]
    fn ollama_text_only_response() {
        let json = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "Hello from Ollama!"
            },
            "done": true,
            "done_reason": "stop"
        });

        let blocks = extract_content_ollama(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => assert_eq!(text, "Hello from Ollama!"),
            other => panic!("expected Text, got: {other:?}"),
        }
    }

    #[test]
    fn ollama_tool_calls_with_object_arguments() {
        let json = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "read",
                        "arguments": { "path": "main.rs" }
                    }
                }]
            },
            "done": true
        });

        let blocks = extract_content_ollama(&json);
        // Empty content string is skipped
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "ollama_call_0", "should generate synthetic ID");
                assert_eq!(name, "read");
                assert_eq!(input["path"], "main.rs");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn ollama_tool_calls_with_string_arguments() {
        let json = serde_json::json!({
            "message": {
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "function": {
                        "name": "exec",
                        "arguments": "{\"cmd\":\"ls -la\"}"
                    }
                }]
            }
        });

        let blocks = extract_content_ollama(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { input, .. } => {
                assert_eq!(input["cmd"], "ls -la");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn ollama_tool_calls_with_explicit_id() {
        let json = serde_json::json!({
            "message": {
                "tool_calls": [{
                    "id": "my_call",
                    "function": {
                        "name": "read",
                        "arguments": { "path": "x" }
                    }
                }]
            }
        });

        let blocks = extract_content_ollama(&json);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::ToolUse { id, .. } => {
                assert_eq!(id, "my_call", "explicit ID should be preserved");
            }
            other => panic!("expected ToolUse, got: {other:?}"),
        }
    }

    #[test]
    fn ollama_empty_response() {
        let json = serde_json::json!({});
        let blocks = extract_content_ollama(&json);
        assert!(blocks.is_empty());
    }

    // ── StopReason extraction ────────────────────────────────────

    #[test]
    fn stop_reason_openai_stop() {
        let json = serde_json::json!({
            "choices": [{ "finish_reason": "stop" }]
        });
        assert_eq!(extract_stop_reason_openai(&json), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_openai_tool_calls() {
        let json = serde_json::json!({
            "choices": [{ "finish_reason": "tool_calls" }]
        });
        assert_eq!(extract_stop_reason_openai(&json), StopReason::ToolUse);
    }

    #[test]
    fn stop_reason_openai_length() {
        let json = serde_json::json!({
            "choices": [{ "finish_reason": "length" }]
        });
        assert_eq!(extract_stop_reason_openai(&json), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_openai_missing_defaults_end_turn() {
        let json = serde_json::json!({});
        assert_eq!(extract_stop_reason_openai(&json), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_anthropic_end_turn() {
        let json = serde_json::json!({ "stop_reason": "end_turn" });
        assert_eq!(extract_stop_reason_anthropic(&json), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_anthropic_tool_use() {
        let json = serde_json::json!({ "stop_reason": "tool_use" });
        assert_eq!(extract_stop_reason_anthropic(&json), StopReason::ToolUse);
    }

    #[test]
    fn stop_reason_anthropic_max_tokens() {
        let json = serde_json::json!({ "stop_reason": "max_tokens" });
        assert_eq!(extract_stop_reason_anthropic(&json), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_anthropic_missing_defaults_end_turn() {
        let json = serde_json::json!({});
        assert_eq!(extract_stop_reason_anthropic(&json), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_ollama_text_only() {
        let json = serde_json::json!({
            "message": { "content": "hi" },
            "done_reason": "stop"
        });
        assert_eq!(extract_stop_reason_ollama(&json), StopReason::EndTurn);
    }

    #[test]
    fn stop_reason_ollama_with_tool_calls() {
        let json = serde_json::json!({
            "message": {
                "tool_calls": [{ "function": { "name": "read" } }]
            },
            "done_reason": "stop"
        });
        // tool_calls presence overrides done_reason
        assert_eq!(extract_stop_reason_ollama(&json), StopReason::ToolUse);
    }

    #[test]
    fn stop_reason_ollama_length() {
        let json = serde_json::json!({
            "message": { "content": "truncated..." },
            "done_reason": "length"
        });
        assert_eq!(extract_stop_reason_ollama(&json), StopReason::MaxTokens);
    }

    #[test]
    fn stop_reason_ollama_empty_tool_calls_not_tool_use() {
        let json = serde_json::json!({
            "message": { "tool_calls": [] },
            "done_reason": "stop"
        });
        assert_eq!(
            extract_stop_reason_ollama(&json),
            StopReason::EndTurn,
            "empty tool_calls should not trigger ToolUse"
        );
    }

    // ── ContentBlock → ActionIntent conversion ───────────────────

    #[test]
    fn content_blocks_to_intents_filters_text() {
        let blocks = vec![
            ContentBlock::Text {
                text: "thinking...".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            },
            ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "exec".into(),
                input: serde_json::json!({"cmd": "ls"}),
            },
        ];

        let intents = content_blocks_to_intents(&blocks);
        assert_eq!(intents.len(), 2, "text blocks should be filtered out");

        assert_eq!(intents[0].id, "call_1");
        assert_eq!(intents[0].name, "read");
        assert_eq!(intents[0].params["path"], "x");
        assert_eq!(
            intents[0].meta.source,
            orcs_types::intent::IntentSource::LlmToolCall
        );

        assert_eq!(intents[1].id, "call_2");
        assert_eq!(intents[1].name, "exec");
    }

    #[test]
    fn content_blocks_to_intents_empty() {
        let blocks: Vec<ContentBlock> = vec![];
        let intents = content_blocks_to_intents(&blocks);
        assert!(intents.is_empty());
    }

    #[test]
    fn content_blocks_to_intents_text_only_returns_empty() {
        let blocks = vec![ContentBlock::Text {
            text: "no tools here".into(),
        }];
        let intents = content_blocks_to_intents(&blocks);
        assert!(intents.is_empty());
    }

    #[test]
    fn content_blocks_to_intents_preserves_tool_result_filtering() {
        let blocks = vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "result data".into(),
                is_error: None,
            },
            ContentBlock::ToolUse {
                id: "call_2".into(),
                name: "write".into(),
                input: serde_json::json!({"path": "a.txt", "content": "hi"}),
            },
        ];

        let intents = content_blocks_to_intents(&blocks);
        assert_eq!(intents.len(), 1, "ToolResult should be filtered out");
        assert_eq!(intents[0].name, "write");
    }

    // ── blocks_to_message_content ────────────────────────────────

    #[test]
    fn single_text_block_returns_text_variant() {
        let blocks = vec![ContentBlock::Text {
            text: "hello".into(),
        }];
        let content = blocks_to_message_content(blocks);
        assert!(matches!(content, MessageContent::Text(ref s) if s == "hello"));
    }

    #[test]
    fn empty_blocks_returns_empty_text() {
        let content = blocks_to_message_content(vec![]);
        assert!(matches!(content, MessageContent::Text(ref s) if s.is_empty()));
    }

    #[test]
    fn mixed_blocks_returns_blocks_variant() {
        let blocks = vec![
            ContentBlock::Text { text: "hi".into() },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({}),
            },
        ];
        let content = blocks_to_message_content(blocks);
        assert!(matches!(content, MessageContent::Blocks(ref b) if b.len() == 2));
    }

    #[test]
    fn single_tool_use_returns_blocks_variant() {
        let blocks = vec![ContentBlock::ToolUse {
            id: "c1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        }];
        let content = blocks_to_message_content(blocks);
        // Single ToolUse is NOT collapsed to Text
        assert!(matches!(content, MessageContent::Blocks(_)));
    }

    // ── End-to-end: provider response → ActionIntents ────────────

    #[test]
    fn e2e_openai_response_to_intents() {
        let json = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "I'll read both files.",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "read", "arguments": "{\"path\":\"a.rs\"}" }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": { "name": "read", "arguments": "{\"path\":\"b.rs\"}" }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        });

        let blocks = extract_content_openai(&json);
        let stop = extract_stop_reason_openai(&json);
        let intents = content_blocks_to_intents(&blocks);

        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(intents.len(), 2);
        assert_eq!(intents[0].params["path"], "a.rs");
        assert_eq!(intents[1].params["path"], "b.rs");
    }

    #[test]
    fn e2e_anthropic_response_to_intents() {
        let json = serde_json::json!({
            "id": "msg_xxx",
            "type": "message",
            "model": "claude-sonnet-4-20250514",
            "content": [
                { "type": "text", "text": "Let me check." },
                {
                    "type": "tool_use",
                    "id": "toolu_abc",
                    "name": "grep",
                    "input": { "pattern": "fn main", "path": "." }
                }
            ],
            "stop_reason": "tool_use"
        });

        let blocks = extract_content_anthropic(&json);
        let stop = extract_stop_reason_anthropic(&json);
        let intents = content_blocks_to_intents(&blocks);
        let content = blocks_to_message_content(blocks);

        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].name, "grep");
        assert_eq!(intents[0].id, "toolu_abc");
        assert_eq!(content.text(), Some("Let me check."));
    }

    #[test]
    fn e2e_ollama_response_to_intents() {
        let json = serde_json::json!({
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
            "done": true,
            "done_reason": "stop"
        });

        let blocks = extract_content_ollama(&json);
        let stop = extract_stop_reason_ollama(&json);
        let intents = content_blocks_to_intents(&blocks);

        assert_eq!(stop, StopReason::ToolUse);
        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].name, "exec");
        assert_eq!(intents[0].params["cmd"], "cargo test");
        assert!(
            intents[0].id.starts_with("ollama_call_"),
            "should have synthetic ID"
        );
    }
}

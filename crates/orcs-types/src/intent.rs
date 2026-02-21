//! ActionIntent domain types.
//!
//! `ActionIntent` is the central domain concept in ORCS: a declaration of
//! "what someone wants to do", independent of who issued it (LLM, Lua, User)
//! or how it gets resolved (Internal Rust dispatch, Component RPC, etc.).
//!
//! # Design Rationale
//!
//! Three previously separate dispatch paths converge into one:
//!
//! ```text
//! LLM tool_calls ─────┐
//!                      │
//! orcs.dispatch() ────>├──→ ActionIntent ──→ Resolver ──→ Executor
//!                      │
//! orcs.request() ─────┘
//! ```
//!
//! Rust encapsulates all complexity (provider differences, permission checks,
//! session management, Component RPC, Hook execution) so that Lua callers
//! deal only with intents and results.
//!
//! # Lifecycle
//!
//! 1. **Declaration** — `ActionIntent` created (from LLM, Lua, or system)
//! 2. **Enrichment** — `IntentMeta` attached (priority, confidence, latency)
//! 3. **Resolution** — `IntentResolver` found via `IntentRegistry`
//! 4. **Execution** — Resolver dispatches (Internal tool / Component RPC)
//! 5. **Result** — `IntentResult` returned to caller

use serde::{Deserialize, Serialize};

// ── ActionIntent ─────────────────────────────────────────────────────

/// A declaration of "what someone wants to do".
///
/// Provider-agnostic, source-agnostic. This is the domain primitive
/// that unifies LLM tool_calls, `orcs.dispatch()`, and Component RPC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionIntent {
    /// Correlation ID (from LLM tool_call id, or auto-generated).
    pub id: String,

    /// Action name (e.g. "read", "exec", "run_skill").
    pub name: String,

    /// Arguments (JSON object). Schema validated at resolution time.
    pub params: serde_json::Value,

    /// Execution metadata (priority, confidence, latency hint, etc.).
    #[serde(default)]
    pub meta: IntentMeta,
}

impl ActionIntent {
    /// Create a new intent with auto-generated ID.
    pub fn new(name: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            id: format!("intent-{}", uuid::Uuid::new_v4()),
            name: name.into(),
            params,
            meta: IntentMeta::default(),
        }
    }

    /// Create from an LLM tool_call (preserves the provider-assigned ID).
    pub fn from_llm_tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        params: serde_json::Value,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            params,
            meta: IntentMeta {
                source: IntentSource::LlmToolCall,
                ..IntentMeta::default()
            },
        }
    }

    /// Attach metadata (builder-style).
    #[must_use]
    pub fn with_meta(mut self, meta: IntentMeta) -> Self {
        self.meta = meta;
        self
    }
}

// ── IntentMeta ───────────────────────────────────────────────────────

/// Execution metadata attached to an intent.
///
/// All fields are optional; callers set what they know.
/// The resolver / executor may use these for scheduling, UX feedback,
/// or Human-in-the-Loop gating.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentMeta {
    /// Who issued the intent (diagnostic / audit).
    #[serde(default)]
    pub source: IntentSource,

    /// Scheduling priority.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,

    /// Estimated execution time in milliseconds (UX feedback).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_latency_ms: Option<u64>,

    /// Issuer's confidence that this intent is correct (0.0–1.0).
    /// Below a threshold → Human-in-the-Loop confirmation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

// ── IntentSource ─────────────────────────────────────────────────────

/// Who issued the intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentSource {
    /// Lua script via `orcs.dispatch()` or `orcs.intent()`.
    #[default]
    Lua,
    /// LLM provider's tool_calls response.
    LlmToolCall,
    /// System-generated (timer, hook, etc.).
    System,
}

// ── Priority ─────────────────────────────────────────────────────────

/// Scheduling priority for intent execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    Low,
    Normal,
    High,
    Critical,
}

// ── IntentResolver ───────────────────────────────────────────────────

/// Where an intent gets resolved.
///
/// The `IntentRegistry` maps intent names to resolvers. When an intent
/// arrives, the registry looks up the resolver and dispatches accordingly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IntentResolver {
    /// Rust-internal dispatch (the 8 builtin tools: read, write, grep, …).
    Internal,

    /// Component RPC via EventBus.
    Component {
        /// Fully-qualified component name (e.g. "lua::skill_manager").
        component_fqn: String,
        /// Operation name passed to the component's `on_request`.
        operation: String,
    },
}

// ── IntentDef ────────────────────────────────────────────────────────

/// Definition of a named intent, registered in `IntentRegistry`.
///
/// Serves two purposes:
/// 1. Generate LLM API `tools` array (name + description + parameters)
/// 2. Route intents to their resolver (Internal / Component)
#[derive(Debug, Clone)]
pub struct IntentDef {
    /// Unique intent name (e.g. "read", "run_skill").
    pub name: String,

    /// Human-readable description (sent to LLM).
    pub description: String,

    /// JSON Schema for parameters (sent to LLM).
    pub parameters: serde_json::Value,

    /// Where this intent gets resolved.
    pub resolver: IntentResolver,

    /// Default metadata applied when the intent is created from this def.
    #[allow(dead_code)]
    pub default_meta: IntentMeta,
}

// ── IntentResult ─────────────────────────────────────────────────────

/// Outcome of executing an intent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntentResult {
    /// Correlation ID (matches `ActionIntent.id`).
    pub intent_id: String,

    /// Intent name (for diagnostics).
    pub name: String,

    /// Whether execution succeeded.
    pub ok: bool,

    /// Result payload (tool output on success, error detail on failure).
    pub content: serde_json::Value,

    /// Error message (when `ok == false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,

    /// Execution duration in milliseconds.
    pub duration_ms: u64,
}

// ── StopReason ───────────────────────────────────────────────────────

/// Why the LLM stopped generating.
///
/// Normalized from provider-specific values:
/// - Ollama:    tool_calls array presence
/// - OpenAI:    `finish_reason == "tool_calls"`
/// - Anthropic: `stop_reason == "tool_use"`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// Model finished its response (final answer).
    EndTurn,
    /// Model wants to use tools (intents issued).
    ToolUse,
    /// Output truncated by token limit.
    MaxTokens,
}

// ── Role ─────────────────────────────────────────────────────────────

/// Conversation message role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

// ── MessageContent ───────────────────────────────────────────────────

/// Message content: plain text or structured blocks.
///
/// Internal representation uses the Anthropic content-block model
/// (the only format where text + tool_use coexist in a single message).
/// Converted to provider-specific wire format in `build_*_body`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text (common case, backward-compatible).
    Text(String),
    /// Structured content blocks (tool_use, tool_result, mixed).
    Blocks(Vec<ContentBlock>),
}

impl MessageContent {
    /// Extract the text portion, ignoring tool blocks.
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Text(s) => Some(s),
            Self::Blocks(blocks) => blocks.iter().find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            }),
        }
    }
}

// ── ContentBlock ─────────────────────────────────────────────────────

/// A single block within a structured message.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// Text content.
    #[serde(rename = "text")]
    Text { text: String },

    /// Tool use request (assistant → system).
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// Tool result feedback (system → assistant).
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ActionIntent ─────────────────────────────────────────────

    #[test]
    fn action_intent_new_generates_id() {
        let intent = ActionIntent::new("read", serde_json::json!({"path": "src/main.rs"}));
        assert!(
            intent.id.starts_with("intent-"),
            "expected intent- prefix, got: {}",
            intent.id
        );
        assert_eq!(intent.name, "read");
        assert_eq!(intent.params["path"], "src/main.rs");
        assert_eq!(intent.meta.source, IntentSource::Lua);
    }

    #[test]
    fn action_intent_from_llm_tool_call() {
        let intent =
            ActionIntent::from_llm_tool_call("call_abc", "exec", serde_json::json!({"cmd": "ls"}));
        assert_eq!(intent.id, "call_abc");
        assert_eq!(intent.name, "exec");
        assert_eq!(intent.meta.source, IntentSource::LlmToolCall);
    }

    #[test]
    fn action_intent_with_meta() {
        let meta = IntentMeta {
            source: IntentSource::System,
            priority: Some(Priority::High),
            expected_latency_ms: Some(500),
            confidence: Some(0.95),
        };
        let intent = ActionIntent::new("read", serde_json::json!({})).with_meta(meta);
        assert_eq!(intent.meta.source, IntentSource::System);
        assert_eq!(intent.meta.priority, Some(Priority::High));
        assert_eq!(intent.meta.expected_latency_ms, Some(500));
        assert!(
            (intent.meta.confidence.expect("should have confidence") - 0.95).abs() < f64::EPSILON
        );
    }

    #[test]
    fn action_intent_unique_ids() {
        let a = ActionIntent::new("read", serde_json::json!({}));
        let b = ActionIntent::new("read", serde_json::json!({}));
        assert_ne!(a.id, b.id, "each intent should get a unique ID");
    }

    // ── Serialization round-trip ─────────────────────────────────

    #[test]
    fn action_intent_serde_roundtrip() {
        let intent = ActionIntent::new(
            "write",
            serde_json::json!({"path": "a.txt", "content": "hi"}),
        );
        let json = serde_json::to_string(&intent).expect("serialize ActionIntent");
        let back: ActionIntent = serde_json::from_str(&json).expect("deserialize ActionIntent");
        assert_eq!(back.id, intent.id);
        assert_eq!(back.name, "write");
        assert_eq!(back.params["path"], "a.txt");
    }

    #[test]
    fn intent_meta_default_serde() {
        let meta = IntentMeta::default();
        let json = serde_json::to_string(&meta).expect("serialize IntentMeta default");
        assert!(
            json.contains(r#""source":"lua"#),
            "default source should be lua, got: {}",
            json
        );
        // Optional fields omitted
        assert!(
            !json.contains("priority"),
            "priority should be skipped when None"
        );
    }

    // ── IntentSource ─────────────────────────────────────────────

    #[test]
    fn intent_source_default_is_lua() {
        assert_eq!(IntentSource::default(), IntentSource::Lua);
    }

    #[test]
    fn intent_source_serde_variants() {
        let cases = [
            (IntentSource::Lua, r#""lua""#),
            (IntentSource::LlmToolCall, r#""llm_tool_call""#),
            (IntentSource::System, r#""system""#),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("serialize IntentSource");
            assert_eq!(json, expected, "IntentSource::{variant:?}");
            let back: IntentSource = serde_json::from_str(&json).expect("deserialize IntentSource");
            assert_eq!(back, variant);
        }
    }

    // ── Priority ─────────────────────────────────────────────────

    #[test]
    fn priority_ordering() {
        assert!(Priority::Low < Priority::Normal);
        assert!(Priority::Normal < Priority::High);
        assert!(Priority::High < Priority::Critical);
    }

    #[test]
    fn priority_serde_variants() {
        let cases = [
            (Priority::Low, r#""low""#),
            (Priority::Normal, r#""normal""#),
            (Priority::High, r#""high""#),
            (Priority::Critical, r#""critical""#),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("serialize Priority");
            assert_eq!(json, expected, "Priority::{variant:?}");
            let back: Priority = serde_json::from_str(&json).expect("deserialize Priority");
            assert_eq!(back, variant);
        }
    }

    // ── IntentResolver ───────────────────────────────────────────

    #[test]
    fn intent_resolver_internal_eq() {
        assert_eq!(IntentResolver::Internal, IntentResolver::Internal);
    }

    #[test]
    fn intent_resolver_component_eq() {
        let a = IntentResolver::Component {
            component_fqn: "lua::skill_manager".into(),
            operation: "execute".into(),
        };
        let b = IntentResolver::Component {
            component_fqn: "lua::skill_manager".into(),
            operation: "execute".into(),
        };
        assert_eq!(a, b);
    }

    #[test]
    fn intent_resolver_different_ne() {
        let internal = IntentResolver::Internal;
        let component = IntentResolver::Component {
            component_fqn: "lua::x".into(),
            operation: "op".into(),
        };
        assert_ne!(internal, component);
    }

    // ── IntentDef ────────────────────────────────────────────────

    #[test]
    fn intent_def_construction() {
        let def = IntentDef {
            name: "read".into(),
            description: "Read file contents".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path" }
                },
                "required": ["path"]
            }),
            resolver: IntentResolver::Internal,
            default_meta: IntentMeta::default(),
        };
        assert_eq!(def.name, "read");
        assert_eq!(def.resolver, IntentResolver::Internal);
        assert!(def.parameters["properties"]["path"].is_object());
    }

    // ── IntentResult ─────────────────────────────────────────────

    #[test]
    fn intent_result_success() {
        let result = IntentResult {
            intent_id: "intent-123".into(),
            name: "read".into(),
            ok: true,
            content: serde_json::json!({"content": "fn main() {}", "size": 13}),
            error: None,
            duration_ms: 5,
        };
        assert!(result.ok);
        assert!(result.error.is_none());
        assert_eq!(result.content["size"], 13);
    }

    #[test]
    fn intent_result_failure() {
        let result = IntentResult {
            intent_id: "intent-456".into(),
            name: "read".into(),
            ok: false,
            content: serde_json::Value::Null,
            error: Some("file not found".into()),
            duration_ms: 1,
        };
        assert!(!result.ok);
        assert_eq!(result.error.as_deref(), Some("file not found"),);
    }

    #[test]
    fn intent_result_serde_roundtrip() {
        let result = IntentResult {
            intent_id: "i-1".into(),
            name: "exec".into(),
            ok: true,
            content: serde_json::json!({"stdout": "hello"}),
            error: None,
            duration_ms: 42,
        };
        let json = serde_json::to_string(&result).expect("serialize IntentResult");
        let back: IntentResult = serde_json::from_str(&json).expect("deserialize IntentResult");
        assert_eq!(back.intent_id, "i-1");
        assert_eq!(back.duration_ms, 42);
        // error=None should be omitted in JSON
        assert!(!json.contains("error"));
    }

    // ── StopReason ───────────────────────────────────────────────

    #[test]
    fn stop_reason_serde_variants() {
        let cases = [
            (StopReason::EndTurn, r#""end_turn""#),
            (StopReason::ToolUse, r#""tool_use""#),
            (StopReason::MaxTokens, r#""max_tokens""#),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("serialize StopReason");
            assert_eq!(json, expected, "StopReason::{variant:?}");
            let back: StopReason = serde_json::from_str(&json).expect("deserialize StopReason");
            assert_eq!(back, variant);
        }
    }

    // ── Role ─────────────────────────────────────────────────────

    #[test]
    fn role_serde_variants() {
        let cases = [
            (Role::System, r#""system""#),
            (Role::User, r#""user""#),
            (Role::Assistant, r#""assistant""#),
        ];
        for (variant, expected) in cases {
            let json = serde_json::to_string(&variant).expect("serialize Role");
            assert_eq!(json, expected, "Role::{variant:?}");
            let back: Role = serde_json::from_str(&json).expect("deserialize Role");
            assert_eq!(back, variant);
        }
    }

    // ── MessageContent ───────────────────────────────────────────

    #[test]
    fn message_content_text_variant() {
        let content = MessageContent::Text("hello".into());
        assert_eq!(content.text(), Some("hello"));

        let json = serde_json::to_string(&content).expect("serialize Text");
        assert_eq!(json, r#""hello""#);
    }

    #[test]
    fn message_content_blocks_text_extraction() {
        let content = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "thinking...".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "x"}),
            },
        ]);
        assert_eq!(content.text(), Some("thinking..."));
    }

    #[test]
    fn message_content_blocks_no_text() {
        let content = MessageContent::Blocks(vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: serde_json::json!({}),
        }]);
        assert_eq!(content.text(), None);
    }

    #[test]
    fn message_content_serde_roundtrip_text() {
        let original = MessageContent::Text("plain text".into());
        let json = serde_json::to_string(&original).expect("serialize");
        let back: MessageContent = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.text(), Some("plain text"));
    }

    #[test]
    fn message_content_serde_roundtrip_blocks() {
        let original = MessageContent::Blocks(vec![
            ContentBlock::Text {
                text: "here is the file:".into(),
            },
            ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "main.rs"}),
            },
        ]);
        let json = serde_json::to_string(&original).expect("serialize");
        let back: MessageContent = serde_json::from_str(&json).expect("deserialize");
        match back {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 2);
                match &blocks[0] {
                    ContentBlock::Text { text } => assert_eq!(text, "here is the file:"),
                    other => panic!("expected Text block, got: {other:?}"),
                }
                match &blocks[1] {
                    ContentBlock::ToolUse { id, name, input } => {
                        assert_eq!(id, "c1");
                        assert_eq!(name, "read");
                        assert_eq!(input["path"], "main.rs");
                    }
                    other => panic!("expected ToolUse block, got: {other:?}"),
                }
            }
            other => panic!("expected Blocks, got: {other:?}"),
        }
    }

    // ── ContentBlock ─────────────────────────────────────────────

    #[test]
    fn content_block_tool_result_serde() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: "fn main() {}".into(),
            is_error: None,
        };
        let json = serde_json::to_string(&block).expect("serialize ToolResult");
        assert!(json.contains(r#""type":"tool_result""#));
        assert!(!json.contains("is_error"), "None should be omitted");

        let back: ContentBlock = serde_json::from_str(&json).expect("deserialize ToolResult");
        match back {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "c1");
                assert_eq!(content, "fn main() {}");
                assert!(is_error.is_none());
            }
            other => panic!("expected ToolResult, got: {other:?}"),
        }
    }

    #[test]
    fn content_block_tool_result_with_error() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "c2".into(),
            content: "permission denied".into(),
            is_error: Some(true),
        };
        let json = serde_json::to_string(&block).expect("serialize");
        assert!(json.contains(r#""is_error":true"#));
    }
}

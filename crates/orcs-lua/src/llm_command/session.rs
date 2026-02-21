use mlua::Lua;
use orcs_types::intent::{MessageContent, Role};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::provider::Provider;
use super::LlmOpts;

// ── Session Store ──────────────────────────────────────────────────────

/// A single message in the conversation history.
///
/// Supports both plain text and structured content blocks (tool_use, tool_result).
/// Backward-compatible: `MessageContent::Text(s)` serializes as a plain string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Message {
    pub role: Role,
    pub content: MessageContent,
}

/// In-memory conversation store, keyed by session_id.
/// Stored in Lua app_data for per-VM isolation.
pub(super) struct SessionStore(pub HashMap<String, Vec<Message>>);

impl SessionStore {
    pub fn new() -> Self {
        Self(HashMap::new())
    }
}

// ── Session Helpers ────────────────────────────────────────────────────

/// Resolve or create a session ID. Returns the session_id string.
pub(super) fn resolve_session_id(lua: &Lua, requested: &Option<String>) -> String {
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
pub(super) fn ensure_session_store(lua: &Lua) {
    if lua.app_data_ref::<SessionStore>().is_none() {
        lua.set_app_data(SessionStore::new());
    }
}

/// Build messages array from session history + current prompt.
pub(super) fn build_messages(
    lua: &Lua,
    session_id: &str,
    prompt: &str,
    opts: &LlmOpts,
) -> Vec<Message> {
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
pub(super) fn update_session(lua: &Lua, session_id: &str, user_msg: &str, assistant_msg: &str) {
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
pub(super) fn append_message(lua: &Lua, session_id: &str, role: Role, content: MessageContent) {
    if let Some(mut store) = lua.remove_app_data::<SessionStore>() {
        let history = store.0.entry(session_id.to_string()).or_default();
        history.push(Message { role, content });
        lua.set_app_data(store);
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::{DEFAULT_MAX_RETRIES, DEFAULT_MAX_TOOL_TURNS};
    use super::*;

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
}

//! Type conversions between Rust and Lua.

use mlua::{FromLua, IntoLua, Lua, Result as LuaResult, Value};
use orcs_component::{EventCategory, Status};
use orcs_event::{Request, Signal, SignalKind, SignalResponse};
use serde::{Deserialize, Serialize};

/// Request representation for Lua.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaRequest {
    /// Request ID.
    pub id: String,
    /// Operation name.
    pub operation: String,
    /// Category.
    pub category: String,
    /// Payload as JSON value.
    pub payload: serde_json::Value,
}

impl LuaRequest {
    /// Creates from orcs Request.
    #[must_use]
    pub fn from_request(request: &Request) -> Self {
        Self {
            id: request.id.to_string(),
            operation: request.operation.clone(),
            category: request.category.to_string(),
            payload: request.payload.clone(),
        }
    }
}

impl IntoLua for LuaRequest {
    fn into_lua(self, lua: &Lua) -> LuaResult<Value> {
        let table = lua.create_table()?;
        table.set("id", self.id)?;
        table.set("operation", self.operation)?;
        table.set("category", self.category)?;
        // Convert payload to Lua value
        let payload: Value = lua
            .load(format!("return {}", json_to_lua(&self.payload)))
            .eval()?;
        table.set("payload", payload)?;
        Ok(Value::Table(table))
    }
}

/// Signal representation for Lua.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LuaSignal {
    /// Signal kind.
    pub kind: String,
    /// Signal scope.
    pub scope: String,
    /// Optional target ID.
    pub target_id: Option<String>,
    /// Approval ID (for Approve/Reject/Modify signals).
    pub approval_id: Option<String>,
    /// Rejection reason (for Reject signals).
    pub reason: Option<String>,
}

impl LuaSignal {
    /// Creates from orcs Signal.
    #[must_use]
    pub fn from_signal(signal: &Signal) -> Self {
        let (approval_id, reason) = match &signal.kind {
            SignalKind::Approve { approval_id } => (Some(approval_id.clone()), None),
            SignalKind::Reject {
                approval_id,
                reason,
            } => (Some(approval_id.clone()), reason.clone()),
            SignalKind::Modify { approval_id, .. } => (Some(approval_id.clone()), None),
            _ => (None, None),
        };

        Self {
            kind: signal_kind_to_string(&signal.kind),
            scope: signal.scope.to_string(),
            target_id: None,
            approval_id,
            reason,
        }
    }
}

/// Convert SignalKind to string.
fn signal_kind_to_string(kind: &SignalKind) -> String {
    match kind {
        SignalKind::Veto => "Veto".to_string(),
        SignalKind::Cancel => "Cancel".to_string(),
        SignalKind::Pause => "Pause".to_string(),
        SignalKind::Resume => "Resume".to_string(),
        SignalKind::Steer { .. } => "Steer".to_string(),
        SignalKind::Approve { .. } => "Approve".to_string(),
        SignalKind::Reject { .. } => "Reject".to_string(),
        SignalKind::Modify { .. } => "Modify".to_string(),
    }
}

impl IntoLua for LuaSignal {
    fn into_lua(self, lua: &Lua) -> LuaResult<Value> {
        let table = lua.create_table()?;
        table.set("kind", self.kind)?;
        table.set("scope", self.scope)?;
        if let Some(id) = self.target_id {
            table.set("target_id", id)?;
        }
        if let Some(id) = self.approval_id {
            table.set("approval_id", id)?;
        }
        if let Some(reason) = self.reason {
            table.set("reason", reason)?;
        }
        Ok(Value::Table(table))
    }
}

/// Response from Lua script.
#[derive(Debug, Clone)]
pub struct LuaResponse {
    /// Success flag.
    pub success: bool,
    /// Response data (if success).
    pub data: Option<serde_json::Value>,
    /// Error message (if failed).
    pub error: Option<String>,
}

impl FromLua for LuaResponse {
    fn from_lua(value: Value, lua: &Lua) -> LuaResult<Self> {
        match value {
            Value::Table(table) => {
                let success: bool = table.get("success").unwrap_or(true);
                let data = table
                    .get::<Value>("data")
                    .ok()
                    .and_then(|v| lua_to_json(v, lua).ok());
                let error: Option<String> = table.get("error").ok();
                Ok(Self {
                    success,
                    data,
                    error,
                })
            }
            Value::Nil => Ok(Self {
                success: true,
                data: None,
                error: None,
            }),
            _ => {
                // Treat any other value as data
                let data = lua_to_json(value, lua).ok();
                Ok(Self {
                    success: true,
                    data,
                    error: None,
                })
            }
        }
    }
}

/// Parse SignalResponse from Lua string.
pub fn parse_signal_response(s: &str) -> SignalResponse {
    match s.to_lowercase().as_str() {
        "handled" => SignalResponse::Handled,
        "abort" => SignalResponse::Abort,
        _ => SignalResponse::Ignored,
    }
}

/// Parse EventCategory from Lua string.
pub fn parse_event_category(s: &str) -> Option<EventCategory> {
    match s {
        "Lifecycle" => Some(EventCategory::Lifecycle),
        "Hil" => Some(EventCategory::Hil),
        "Echo" => Some(EventCategory::Echo),
        "UserInput" => Some(EventCategory::UserInput),
        "Output" => Some(EventCategory::Output),
        _ => {
            // Treat as Extension category
            Some(EventCategory::Extension {
                namespace: "lua".to_string(),
                kind: s.to_string(),
            })
        }
    }
}

/// Parse Status from Lua string.
pub fn parse_status(s: &str) -> Status {
    match s.to_lowercase().as_str() {
        "initializing" => Status::Initializing,
        "idle" => Status::Idle,
        "running" => Status::Running,
        "paused" => Status::Paused,
        "awaitingapproval" | "awaiting_approval" => Status::AwaitingApproval,
        "completed" => Status::Completed,
        "error" => Status::Error,
        "aborted" => Status::Aborted,
        _ => Status::Idle,
    }
}

/// Escape a string for safe inclusion in Lua code.
///
/// Handles all control characters and special sequences to prevent
/// Lua code injection attacks.
fn escape_lua_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 2);
    result.push('"');
    for c in s.chars() {
        match c {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\0' => result.push_str("\\0"),
            // Bell, backspace, form feed, vertical tab
            '\x07' => result.push_str("\\a"),
            '\x08' => result.push_str("\\b"),
            '\x0C' => result.push_str("\\f"),
            '\x0B' => result.push_str("\\v"),
            // Other control characters (0x00-0x1F except already handled)
            c if c.is_control() => {
                result.push_str(&format!("\\{:03}", c as u32));
            }
            c => result.push(c),
        }
    }
    result.push('"');
    result
}

/// Escape a key for use in Lua table.
fn escape_lua_key(key: &str) -> String {
    // Check if key is a valid Lua identifier
    let is_valid_identifier = !key.is_empty()
        && key
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');

    if is_valid_identifier {
        key.to_string()
    } else {
        format!("[{}]", escape_lua_string(key))
    }
}

/// Convert JSON value to Lua string representation.
///
/// # Security
///
/// All string values are properly escaped to prevent Lua code injection.
pub fn json_to_lua(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "nil".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => escape_lua_string(s),
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(json_to_lua).collect();
            format!("{{{}}}", items.join(", "))
        }
        serde_json::Value::Object(obj) => {
            let items: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("{} = {}", escape_lua_key(k), json_to_lua(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Convert Lua value to JSON.
#[allow(clippy::only_used_in_recursion)]
pub fn lua_to_json(value: Value, lua: &Lua) -> Result<serde_json::Value, mlua::Error> {
    match value {
        Value::Nil => Ok(serde_json::Value::Null),
        Value::Boolean(b) => Ok(serde_json::Value::Bool(b)),
        Value::Integer(i) => Ok(serde_json::Value::Number(i.into())),
        Value::Number(n) => serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .ok_or_else(|| mlua::Error::SerializeError("invalid number".into())),
        Value::String(s) => Ok(serde_json::Value::String(s.to_str()?.to_string())),
        Value::Table(table) => {
            // Check if it's an array or object
            let len = table.raw_len();
            if len > 0 {
                // Treat as array
                let mut arr = Vec::new();
                for i in 1..=len {
                    let v: Value = table.raw_get(i)?;
                    arr.push(lua_to_json(v, lua)?);
                }
                Ok(serde_json::Value::Array(arr))
            } else {
                // Treat as object
                let mut map = serde_json::Map::new();
                for pair in table.pairs::<String, Value>() {
                    let (k, v) = pair?;
                    map.insert(k, lua_to_json(v, lua)?);
                }
                Ok(serde_json::Value::Object(map))
            }
        }
        _ => Err(mlua::Error::SerializeError("unsupported type".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_signal_response_variants() {
        assert!(matches!(
            parse_signal_response("Handled"),
            SignalResponse::Handled
        ));
        assert!(matches!(
            parse_signal_response("Abort"),
            SignalResponse::Abort
        ));
        assert!(matches!(
            parse_signal_response("Ignored"),
            SignalResponse::Ignored
        ));
        assert!(matches!(
            parse_signal_response("unknown"),
            SignalResponse::Ignored
        ));
    }

    #[test]
    fn parse_event_category_variants() {
        assert_eq!(parse_event_category("Echo"), Some(EventCategory::Echo));
        assert_eq!(parse_event_category("Hil"), Some(EventCategory::Hil));
        // Unknown categories become Extension
        assert!(matches!(
            parse_event_category("Custom"),
            Some(EventCategory::Extension { .. })
        ));
    }

    #[test]
    fn parse_status_variants() {
        assert_eq!(parse_status("idle"), Status::Idle);
        assert_eq!(parse_status("Running"), Status::Running);
        assert_eq!(parse_status("aborted"), Status::Aborted);
    }

    #[test]
    fn json_to_lua_conversion() {
        assert_eq!(json_to_lua(&serde_json::json!(null)), "nil");
        assert_eq!(json_to_lua(&serde_json::json!(true)), "true");
        assert_eq!(json_to_lua(&serde_json::json!(42)), "42");
        assert_eq!(json_to_lua(&serde_json::json!("hello")), "\"hello\"");
    }

    // === Security Tests for Lua Injection Prevention ===

    #[test]
    fn escape_lua_string_basic_escapes() {
        // Backslash
        assert_eq!(escape_lua_string(r"a\b"), r#""a\\b""#);
        // Double quote
        assert_eq!(escape_lua_string(r#"a"b"#), r#""a\"b""#);
        // Newline
        assert_eq!(escape_lua_string("a\nb"), r#""a\nb""#);
        // Carriage return
        assert_eq!(escape_lua_string("a\rb"), r#""a\rb""#);
        // Tab
        assert_eq!(escape_lua_string("a\tb"), r#""a\tb""#);
        // Null
        assert_eq!(escape_lua_string("a\0b"), r#""a\0b""#);
    }

    #[test]
    fn escape_lua_string_control_chars() {
        // Bell
        assert_eq!(escape_lua_string("a\x07b"), r#""a\ab""#);
        // Backspace
        assert_eq!(escape_lua_string("a\x08b"), r#""a\bb""#);
        // Form feed
        assert_eq!(escape_lua_string("a\x0Cb"), r#""a\fb""#);
        // Vertical tab
        assert_eq!(escape_lua_string("a\x0Bb"), r#""a\vb""#);
    }

    #[test]
    fn escape_lua_string_injection_attempt() {
        // Attempt to break out of string and execute code
        let malicious = "hello\nend; os.execute('rm -rf /')--";
        let escaped = escape_lua_string(malicious);
        // Should be safely escaped, not executable
        assert_eq!(escaped, r#""hello\nend; os.execute('rm -rf /')--""#);

        // Verify it doesn't contain unescaped newline
        assert!(!escaped.contains('\n'));
    }

    #[test]
    fn escape_lua_string_unicode() {
        // Unicode should pass through unchanged
        assert_eq!(escape_lua_string("日本語"), "\"日本語\"");
        assert_eq!(escape_lua_string("emoji: 🎉"), "\"emoji: 🎉\"");
    }

    #[test]
    fn escape_lua_string_empty() {
        assert_eq!(escape_lua_string(""), "\"\"");
    }

    #[test]
    fn escape_lua_key_valid_identifiers() {
        assert_eq!(escape_lua_key("foo"), "foo");
        assert_eq!(escape_lua_key("_bar"), "_bar");
        assert_eq!(escape_lua_key("baz123"), "baz123");
        assert_eq!(escape_lua_key("a_b_c"), "a_b_c");
    }

    #[test]
    fn escape_lua_key_invalid_identifiers() {
        // Starts with number
        assert_eq!(escape_lua_key("123abc"), "[\"123abc\"]");
        // Contains special chars
        assert_eq!(escape_lua_key("foo-bar"), "[\"foo-bar\"]");
        // Contains space
        assert_eq!(escape_lua_key("foo bar"), "[\"foo bar\"]");
        // Empty key
        assert_eq!(escape_lua_key(""), "[\"\"]");
    }

    #[test]
    fn json_to_lua_nested_structure() {
        let nested = serde_json::json!({
            "level1": {
                "level2": {
                    "value": "deep"
                }
            }
        });
        let lua_str = json_to_lua(&nested);
        assert!(lua_str.contains("level1"));
        assert!(lua_str.contains("level2"));
        assert!(lua_str.contains("\"deep\""));
    }

    #[test]
    fn json_to_lua_array() {
        let arr = serde_json::json!([1, 2, "three", null]);
        let lua_str = json_to_lua(&arr);
        assert_eq!(lua_str, "{1, 2, \"three\", nil}");
    }

    #[test]
    fn json_to_lua_special_string_in_object() {
        let obj = serde_json::json!({
            "key": "value\nwith\nnewlines"
        });
        let lua_str = json_to_lua(&obj);
        // Newlines should be escaped
        assert!(!lua_str.contains('\n'));
        assert!(lua_str.contains(r"\n"));
    }

    #[test]
    fn json_to_lua_execution_safety() {
        // Create a Lua instance and verify the escaped string is safe
        let lua = Lua::new();

        // Malicious payload that tries to break out
        let payload = serde_json::json!({
            "data": "test\"); os.execute(\"echo pwned"
        });
        let lua_code = format!("return {}", json_to_lua(&payload));

        // Should parse safely without executing os.execute
        let result: mlua::Result<mlua::Table> = lua.load(&lua_code).eval();
        assert!(result.is_ok(), "Lua code should parse safely");

        // Verify the data is intact (escaped)
        let table = result.unwrap();
        let data: String = table.get("data").unwrap();
        assert!(data.contains("os.execute"));
    }

    // === Boundary Tests for lua_to_json ===

    #[test]
    fn lua_to_json_empty_table() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        let result = lua_to_json(Value::Table(table), &lua).unwrap();
        assert_eq!(result, serde_json::json!({}));
    }

    #[test]
    fn lua_to_json_array_table() {
        let lua = Lua::new();
        let table = lua.create_table().unwrap();
        table.raw_set(1, "a").unwrap();
        table.raw_set(2, "b").unwrap();
        let result = lua_to_json(Value::Table(table), &lua).unwrap();
        assert_eq!(result, serde_json::json!(["a", "b"]));
    }

    #[test]
    fn lua_to_json_mixed_numbers() {
        let lua = Lua::new();

        // Integer
        let int_result = lua_to_json(Value::Integer(42), &lua).unwrap();
        assert_eq!(int_result, serde_json::json!(42));

        // Float
        let float_result = lua_to_json(Value::Number(3.14), &lua).unwrap();
        assert!(float_result.as_f64().unwrap() - 3.14 < 0.001);
    }
}

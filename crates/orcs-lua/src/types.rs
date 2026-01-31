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
}

impl LuaSignal {
    /// Creates from orcs Signal.
    #[must_use]
    pub fn from_signal(signal: &Signal) -> Self {
        Self {
            kind: signal_kind_to_string(&signal.kind),
            scope: signal.scope.to_string(),
            target_id: None,
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

/// Convert JSON value to Lua string representation.
fn json_to_lua(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "nil".to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => {
            format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(json_to_lua).collect();
            format!("{{{}}}", items.join(", "))
        }
        serde_json::Value::Object(obj) => {
            let items: Vec<String> = obj
                .iter()
                .map(|(k, v)| format!("[\"{}\"] = {}", k, json_to_lua(v)))
                .collect();
            format!("{{{}}}", items.join(", "))
        }
    }
}

/// Convert Lua value to JSON.
#[allow(clippy::only_used_in_recursion)]
fn lua_to_json(value: Value, lua: &Lua) -> Result<serde_json::Value, mlua::Error> {
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
}

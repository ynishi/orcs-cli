//! LuaComponent implementation.
//!
//! Wraps a Lua script to implement the Component trait.

use crate::error::LuaError;
use crate::types::{
    parse_event_category, parse_signal_response, LuaRequest, LuaResponse, LuaSignal,
};
use mlua::{Function, Lua, RegistryKey, Table};
use orcs_component::{Component, ComponentError, EventCategory, Status};
use orcs_event::{Request, Signal, SignalResponse};
use orcs_types::ComponentId;
use serde_json::Value as JsonValue;
use std::path::Path;
use std::sync::Mutex;

/// A component implemented in Lua.
///
/// Loads a Lua script and delegates Component trait methods to Lua functions.
///
/// # Script Format
///
/// The Lua script must return a table with the following structure:
///
/// ```lua
/// return {
///     id = "component-id",           -- Required: unique identifier
///     subscriptions = {"Echo"},      -- Required: event categories
///
///     on_request = function(req)     -- Required: handle requests
///         return { success = true, data = ... }
///     end,
///
///     on_signal = function(sig)      -- Required: handle signals
///         return "Handled" | "Ignored" | "Abort"
///     end,
///
///     init = function()              -- Optional: initialization
///     end,
///
///     shutdown = function()          -- Optional: cleanup
///     end,
/// }
/// ```
pub struct LuaComponent {
    /// Lua runtime (wrapped in Mutex for Send+Sync).
    lua: Mutex<Lua>,
    /// Component identifier.
    id: ComponentId,
    /// Subscribed event categories.
    subscriptions: Vec<EventCategory>,
    /// Current status.
    status: Status,
    /// Registry key for on_request callback.
    on_request_key: RegistryKey,
    /// Registry key for on_signal callback.
    on_signal_key: RegistryKey,
    /// Registry key for init callback (optional).
    init_key: Option<RegistryKey>,
    /// Registry key for shutdown callback (optional).
    shutdown_key: Option<RegistryKey>,
    /// Script path (for hot reload).
    script_path: Option<String>,
}

// SAFETY: Lua runtime is protected by Mutex
unsafe impl Send for LuaComponent {}
unsafe impl Sync for LuaComponent {}

impl LuaComponent {
    /// Creates a new LuaComponent from a script file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the Lua script
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Script file not found
    /// - Script syntax error
    /// - Missing required fields/callbacks
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self, LuaError> {
        let path = path.as_ref();
        let script = std::fs::read_to_string(path)
            .map_err(|_| LuaError::ScriptNotFound(path.display().to_string()))?;

        let mut component = Self::from_script(&script)?;
        component.script_path = Some(path.display().to_string());
        Ok(component)
    }

    /// Creates a new LuaComponent from a script string.
    ///
    /// # Arguments
    ///
    /// * `script` - Lua script content
    ///
    /// # Errors
    ///
    /// Returns error if script is invalid.
    pub fn from_script(script: &str) -> Result<Self, LuaError> {
        let lua = Lua::new();

        // Execute script and get the returned table
        let component_table: Table = lua
            .load(script)
            .eval()
            .map_err(|e| LuaError::InvalidScript(e.to_string()))?;

        // Extract id and namespace
        let id_str: String = component_table
            .get("id")
            .map_err(|_| LuaError::MissingCallback("id".to_string()))?;
        let namespace: String = component_table
            .get("namespace")
            .unwrap_or_else(|_| "lua".to_string());
        let id = ComponentId::new(namespace, &id_str);

        // Extract subscriptions
        let subs_table: Table = component_table
            .get("subscriptions")
            .map_err(|_| LuaError::MissingCallback("subscriptions".to_string()))?;

        let mut subscriptions = Vec::new();
        for pair in subs_table.pairs::<i64, String>() {
            let (_, cat_str) = pair.map_err(|e| LuaError::TypeError(e.to_string()))?;
            if let Some(cat) = parse_event_category(&cat_str) {
                subscriptions.push(cat);
            }
        }

        // Extract required callbacks
        let on_request_fn: Function = component_table
            .get("on_request")
            .map_err(|_| LuaError::MissingCallback("on_request".to_string()))?;

        let on_signal_fn: Function = component_table
            .get("on_signal")
            .map_err(|_| LuaError::MissingCallback("on_signal".to_string()))?;

        // Store callbacks in registry
        let on_request_key = lua.create_registry_value(on_request_fn)?;
        let on_signal_key = lua.create_registry_value(on_signal_fn)?;

        // Extract optional callbacks
        let init_key = component_table
            .get::<Function>("init")
            .ok()
            .map(|f| lua.create_registry_value(f))
            .transpose()?;

        let shutdown_key = component_table
            .get::<Function>("shutdown")
            .ok()
            .map(|f| lua.create_registry_value(f))
            .transpose()?;

        Ok(Self {
            lua: Mutex::new(lua),
            id,
            subscriptions,
            status: Status::Idle,
            on_request_key,
            on_signal_key,
            init_key,
            shutdown_key,
            script_path: None,
        })
    }

    /// Returns the script path if loaded from file.
    #[must_use]
    pub fn script_path(&self) -> Option<&str> {
        self.script_path.as_deref()
    }

    /// Reloads the script from file.
    ///
    /// # Errors
    ///
    /// Returns error if reload fails.
    pub fn reload(&mut self) -> Result<(), LuaError> {
        let Some(path) = &self.script_path else {
            return Err(LuaError::InvalidScript("no script path".into()));
        };

        let new_component = Self::from_file(path)?;

        // Swap internals
        self.lua = new_component.lua;
        self.subscriptions = new_component.subscriptions;
        self.on_request_key = new_component.on_request_key;
        self.on_signal_key = new_component.on_signal_key;
        self.init_key = new_component.init_key;
        self.shutdown_key = new_component.shutdown_key;

        tracing::info!("Reloaded Lua component: {}", self.id);
        Ok(())
    }
}

impl Component for LuaComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> Vec<EventCategory> {
        self.subscriptions.clone()
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<JsonValue, ComponentError> {
        self.status = Status::Running;

        let lua = self
            .lua
            .lock()
            .map_err(|e| ComponentError::ExecutionFailed(format!("lua lock failed: {}", e)))?;

        // Get callback from registry
        let on_request: Function = lua
            .registry_value(&self.on_request_key)
            .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;

        // Convert request to Lua
        let lua_req = LuaRequest::from_request(request);

        // Call Lua function
        let result: LuaResponse = on_request
            .call(lua_req)
            .map_err(|e| ComponentError::ExecutionFailed(format!("lua call failed: {}", e)))?;

        drop(lua);
        self.status = Status::Idle;

        if result.success {
            Ok(result.data.unwrap_or(JsonValue::Null))
        } else {
            Err(ComponentError::ExecutionFailed(
                result.error.unwrap_or_else(|| "unknown error".into()),
            ))
        }
    }

    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        let Ok(lua) = self.lua.lock() else {
            return SignalResponse::Ignored;
        };

        let Ok(on_signal): Result<Function, _> = lua.registry_value(&self.on_signal_key) else {
            return SignalResponse::Ignored;
        };

        let lua_sig = LuaSignal::from_signal(signal);

        let result: Result<String, _> = on_signal.call(lua_sig);

        match result {
            Ok(response_str) => {
                let response = parse_signal_response(&response_str);
                if matches!(response, SignalResponse::Abort) {
                    drop(lua);
                    self.status = Status::Aborted;
                }
                response
            }
            Err(e) => {
                tracing::warn!("Lua on_signal error: {}", e);
                SignalResponse::Ignored
            }
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;
    }

    fn init(&mut self) -> Result<(), ComponentError> {
        let Some(init_key) = &self.init_key else {
            return Ok(());
        };

        let lua = self
            .lua
            .lock()
            .map_err(|e| ComponentError::ExecutionFailed(format!("lua lock failed: {}", e)))?;

        let init_fn: Function = lua
            .registry_value(init_key)
            .map_err(|e| ComponentError::ExecutionFailed(e.to_string()))?;

        init_fn
            .call::<()>(())
            .map_err(|e| ComponentError::ExecutionFailed(format!("init failed: {}", e)))?;

        Ok(())
    }

    fn shutdown(&mut self) {
        let Some(shutdown_key) = &self.shutdown_key else {
            return;
        };

        let Ok(lua) = self.lua.lock() else {
            return;
        };

        if let Ok(shutdown_fn) = lua.registry_value::<Function>(shutdown_key) {
            if let Err(e) = shutdown_fn.call::<()>(()) {
                tracing::warn!("Lua shutdown error: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_event::Request;
    use orcs_types::{ChannelId, ComponentId, Principal};

    fn create_test_request(operation: &str, payload: JsonValue) -> Request {
        Request::new(
            EventCategory::Echo,
            operation,
            ComponentId::builtin("test"),
            ChannelId::new(),
            payload,
        )
    }

    fn test_principal() -> Principal {
        Principal::User(orcs_types::PrincipalId::new())
    }

    #[test]
    fn load_simple_component() {
        let script = r#"
            return {
                id = "test-component",
                subscriptions = {"Echo"},
                on_request = function(req)
                    return { success = true, data = req.payload }
                end,
                on_signal = function(sig)
                    return "Ignored"
                end,
            }
        "#;

        let component = LuaComponent::from_script(script).expect("load script");
        assert!(component.id().fqn().contains("test-component"));
        assert_eq!(component.subscriptions(), vec![EventCategory::Echo]);
    }

    #[test]
    fn handle_request() {
        let script = r#"
            return {
                id = "echo-lua",
                subscriptions = {"Echo"},
                on_request = function(req)
                    if req.operation == "echo" then
                        return { success = true, data = "echoed" }
                    end
                    return { success = false, error = "unknown" }
                end,
                on_signal = function(sig)
                    return "Ignored"
                end,
            }
        "#;

        let mut component = LuaComponent::from_script(script).expect("load script");

        let req = create_test_request("echo", JsonValue::String("hello".into()));
        let result = component.on_request(&req);

        assert!(result.is_ok());
        assert_eq!(result.unwrap(), JsonValue::String("echoed".into()));
    }

    #[test]
    fn handle_signal_abort() {
        let script = r#"
            return {
                id = "signal-test",
                subscriptions = {"Echo"},
                on_request = function(req)
                    return { success = true }
                end,
                on_signal = function(sig)
                    if sig.kind == "Veto" then
                        return "Abort"
                    end
                    return "Ignored"
                end,
            }
        "#;

        let mut component = LuaComponent::from_script(script).expect("load script");

        let signal = Signal::veto(test_principal());
        let response = component.on_signal(&signal);

        assert!(matches!(response, SignalResponse::Abort));
        assert_eq!(component.status(), Status::Aborted);
    }

    #[test]
    fn missing_callback_error() {
        let script = r#"
            return {
                id = "incomplete",
                subscriptions = {"Echo"},
                -- missing on_request and on_signal
            }
        "#;

        let result = LuaComponent::from_script(script);
        assert!(result.is_err());
    }
}

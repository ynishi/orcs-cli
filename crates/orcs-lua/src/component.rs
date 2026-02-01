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
use tokio::process::Command;

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

// SAFETY: LuaComponent can be safely sent between threads and accessed concurrently.
//
// Justification:
// 1. mlua is built with "send" feature (see Cargo.toml), which enables thread-safe
//    Lua state allocation and makes the allocator thread-safe.
// 2. The Lua runtime is wrapped in Mutex<Lua>, ensuring exclusive mutable access.
//    All methods that access the Lua state acquire the lock first.
// 3. All Lua callbacks are stored in the Lua registry via RegistryKey, which is
//    designed for this use case. RegistryKey itself is Send.
// 4. No raw Lua values (userdata, functions) escape the Mutex guard scope.
//    Values are converted to/from Rust types within the lock scope.
// 5. The remaining fields (id, subscriptions, status, script_path) are all Send+Sync.
//
// The "send" feature documentation: https://docs.rs/mlua/latest/mlua/#async-send
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

        // Register orcs helper functions
        Self::register_orcs_functions(&lua)?;

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

    /// Registers orcs helper functions in Lua.
    ///
    /// Available functions:
    /// - `orcs.exec(cmd)` - Execute shell command and return output
    /// - `orcs.exec_streaming(cmd, callback)` - Execute with streaming output
    fn register_orcs_functions(lua: &Lua) -> Result<(), LuaError> {
        let orcs_table = lua.create_table()?;

        // orcs.exec(cmd) -> {ok, stdout, stderr, code}
        // Uses tokio::process::Command for non-blocking execution.
        // When called from spawn_blocking context, uses Handle::current() to run async code.
        let exec_fn = lua.create_function(|lua, cmd: String| {
            tracing::debug!("Lua exec: {}", cmd);

            // Try to get current tokio runtime handle for async execution.
            // If running in spawn_blocking context, this will succeed.
            let output = match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    // We have a runtime handle - run async command
                    handle.block_on(async { Command::new("sh").arg("-c").arg(&cmd).output().await })
                }
                Err(_) => {
                    // No runtime available - fallback to sync execution
                    tracing::warn!("No tokio runtime available, using sync execution");
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .output()
                }
            }
            .map_err(|e| mlua::Error::ExternalError(std::sync::Arc::new(e)))?;

            let result = lua.create_table()?;
            result.set("ok", output.status.success())?;
            result.set(
                "stdout",
                String::from_utf8_lossy(&output.stdout).to_string(),
            )?;
            result.set(
                "stderr",
                String::from_utf8_lossy(&output.stderr).to_string(),
            )?;
            result.set("code", output.status.code().unwrap_or(-1))?;

            Ok(result)
        })?;
        orcs_table.set("exec", exec_fn)?;

        // orcs.log(level, msg)
        let log_fn = lua.create_function(|_, (level, msg): (String, String)| {
            match level.to_lowercase().as_str() {
                "debug" => tracing::debug!("[lua] {}", msg),
                "info" => tracing::info!("[lua] {}", msg),
                "warn" => tracing::warn!("[lua] {}", msg),
                "error" => tracing::error!("[lua] {}", msg),
                _ => tracing::info!("[lua] {}", msg),
            }
            Ok(())
        })?;
        orcs_table.set("log", log_fn)?;

        lua.globals().set("orcs", orcs_table)?;

        Ok(())
    }
}

impl Component for LuaComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> &[EventCategory] {
        &self.subscriptions
    }

    fn status(&self) -> Status {
        self.status
    }

    fn on_request(&mut self, request: &Request) -> Result<JsonValue, ComponentError> {
        self.status = Status::Running;

        let lua = self.lua.lock().map_err(|e| {
            tracing::error!("Lua mutex poisoned: {}", e);
            ComponentError::ExecutionFailed("lua runtime unavailable".to_string())
        })?;

        // Get callback from registry
        let on_request: Function = lua.registry_value(&self.on_request_key).map_err(|e| {
            tracing::debug!("Failed to get on_request from registry: {}", e);
            ComponentError::ExecutionFailed("lua callback not found".to_string())
        })?;

        // Convert request to Lua
        let lua_req = LuaRequest::from_request(request);

        // Call Lua function
        let result: LuaResponse = on_request.call(lua_req).map_err(|e| {
            // Sanitize error message to avoid leaking internal details
            tracing::debug!("Lua on_request error: {}", e);
            ComponentError::ExecutionFailed("lua script execution failed".to_string())
        })?;

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

        let lua = self.lua.lock().map_err(|e| {
            tracing::error!("Lua mutex poisoned in init: {}", e);
            ComponentError::ExecutionFailed("lua runtime unavailable".to_string())
        })?;

        let init_fn: Function = lua.registry_value(init_key).map_err(|e| {
            tracing::debug!("Failed to get init from registry: {}", e);
            ComponentError::ExecutionFailed("lua init callback not found".to_string())
        })?;

        init_fn.call::<()>(()).map_err(|e| {
            tracing::debug!("Lua init error: {}", e);
            ComponentError::ExecutionFailed("lua init callback failed".to_string())
        })?;

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
        assert_eq!(component.subscriptions(), &[EventCategory::Echo]);
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

    // --- orcs.exec tests ---

    mod exec_tests {
        use super::*;

        /// Helper to create a component that uses orcs.exec
        fn create_exec_component(cmd: &str) -> LuaComponent {
            let script = format!(
                r#"
                return {{
                    id = "exec-test",
                    subscriptions = {{"Echo"}},
                    on_request = function(req)
                        local result = orcs.exec("{}")
                        -- Always return success=true so we can inspect the exec result
                        return {{
                            success = true,
                            data = {{
                                stdout = result.stdout,
                                stderr = result.stderr,
                                code = result.code,
                                ok = result.ok
                            }}
                        }}
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }}
            "#,
                cmd
            );
            LuaComponent::from_script(&script).expect("load exec script")
        }

        #[test]
        fn exec_echo_command_sync() {
            // Test orcs.exec in sync context (no tokio runtime)
            let mut component = create_exec_component("echo 'hello world'");

            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(data["ok"].as_bool().unwrap_or(false));
            assert!(data["stdout"].as_str().unwrap().contains("hello world"));
            assert_eq!(data["code"].as_i64(), Some(0));
        }

        #[test]
        fn exec_failing_command_sync() {
            // Test orcs.exec with a failing command
            let mut component = create_exec_component("exit 42");

            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(!data["ok"].as_bool().unwrap_or(true));
            assert_eq!(data["code"].as_i64(), Some(42));
        }

        #[test]
        fn exec_stderr_captured() {
            // Test that stderr is captured
            let mut component = create_exec_component("echo 'error output' >&2");

            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(data["stderr"].as_str().unwrap().contains("error output"));
        }

        #[tokio::test]
        async fn exec_in_async_context() {
            // Test orcs.exec when called from async context (via spawn_blocking)
            let mut component = create_exec_component("echo 'async test'");

            let req = create_test_request("test", JsonValue::Null);

            // Simulate spawn_blocking context like ClientRunner does
            let result = tokio::task::spawn_blocking(move || component.on_request(&req))
                .await
                .expect("spawn_blocking should succeed");

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(data["ok"].as_bool().unwrap_or(false));
            assert!(data["stdout"].as_str().unwrap().contains("async test"));
        }

        #[tokio::test]
        async fn exec_complex_command_in_async() {
            // Test with a more complex command involving pipes
            let mut component = create_exec_component("echo 'line1\\nline2' | wc -l");

            let req = create_test_request("test", JsonValue::Null);

            let result = tokio::task::spawn_blocking(move || component.on_request(&req))
                .await
                .expect("spawn_blocking should succeed");

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(data["ok"].as_bool().unwrap_or(false));
            // wc -l should return "2" (with possible whitespace)
            let stdout = data["stdout"].as_str().unwrap().trim();
            assert!(stdout.contains('2') || stdout == "1"); // Different systems may count differently
        }

        #[test]
        fn exec_with_special_characters() {
            // Test command with special characters
            let script = r#"
                return {
                    id = "exec-special",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local result = orcs.exec("echo \"quotes and 'apostrophes'\"")
                        return {
                            success = result.ok,
                            data = { stdout = result.stdout }
                        }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

            let mut component = LuaComponent::from_script(script).expect("load script");
            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            let data = result.unwrap();
            assert!(data["stdout"].as_str().unwrap().contains("quotes"));
        }
    }

    // --- orcs.log tests ---

    mod log_tests {
        use super::*;

        #[test]
        fn log_levels_work() {
            let script = r#"
                return {
                    id = "log-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.log("debug", "debug message")
                        orcs.log("info", "info message")
                        orcs.log("warn", "warn message")
                        orcs.log("error", "error message")
                        orcs.log("unknown", "unknown level")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

            let mut component = LuaComponent::from_script(script).expect("load script");
            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            // Should complete without error (log output goes to tracing)
            assert!(result.is_ok());
        }
    }
}

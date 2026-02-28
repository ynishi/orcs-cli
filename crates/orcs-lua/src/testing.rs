//! Lua component testing harness.
//!
//! Provides specialized testing infrastructure for Lua-based components.
//!
//! # Features
//!
//! - Script-based component testing
//! - Request/Signal logging for snapshot testing
//! - Hot reload testing support
//! - Lua-specific error handling
//!
//! # Example
//!
//! ```
//! use orcs_lua::testing::LuaTestHarness;
//! use orcs_component::EventCategory;
//! use orcs_event::SignalResponse;
//! use orcs_runtime::sandbox::ProjectSandbox;
//! use serde_json::json;
//! use std::sync::Arc;
//!
//! let script = r#"
//!     return {
//!         id = "echo",
//!         subscriptions = {"Echo"},
//!         on_request = function(req)
//!             return { success = true, data = req.payload }
//!         end,
//!         on_signal = function(sig)
//!             if sig.kind == "Veto" then return "Abort" end
//!             return "Handled"
//!         end,
//!     }
//! "#;
//!
//! let sandbox = Arc::new(ProjectSandbox::new(".").expect("sandbox init"));
//! let mut harness = LuaTestHarness::from_script(script, sandbox).expect("harness init");
//!
//! // Test request
//! let result = harness.request(EventCategory::Echo, "echo", json!({"msg": "hello"}));
//! assert!(result.is_ok());
//!
//! // Test veto signal
//! let response = harness.veto();
//! assert_eq!(response, SignalResponse::Abort);
//! ```

use crate::{LuaComponent, LuaError};
use mlua::LuaSerdeExt;
use orcs_component::testing::{ComponentTestHarness, RequestRecord, SignalRecord};
use orcs_component::{Component, ComponentError, EventCategory, Status};
use orcs_event::SignalResponse;
use orcs_runtime::sandbox::SandboxPolicy;
use orcs_types::ComponentId;
use serde_json::Value;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Test harness for Lua-based components.
///
/// Wraps [`ComponentTestHarness`] with Lua-specific functionality:
/// - Script loading from string or file
/// - Hot reload testing
/// - Lua error context
pub struct LuaTestHarness {
    /// Inner component test harness.
    inner: ComponentTestHarness<LuaComponent>,
    /// Original script source (for debugging).
    script_source: Option<String>,
}

impl LuaTestHarness {
    /// Creates a test harness from a Lua script string.
    ///
    /// # Arguments
    ///
    /// * `script` - Lua script content
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if the script is invalid.
    ///
    /// # Example
    ///
    /// ```
    /// use orcs_lua::testing::LuaTestHarness;
    /// use orcs_runtime::sandbox::ProjectSandbox;
    /// use std::sync::Arc;
    ///
    /// let script = r#"
    ///     return {
    ///         id = "test",
    ///         subscriptions = {"Echo"},
    ///         on_request = function(req) return { success = true } end,
    ///         on_signal = function(sig) return "Ignored" end,
    ///     }
    /// "#;
    ///
    /// let sandbox = Arc::new(ProjectSandbox::new(".").expect("sandbox init"));
    /// let harness = LuaTestHarness::from_script(script, sandbox).expect("harness init");
    /// assert_eq!(harness.id().name, "test");
    /// ```
    pub fn from_script(script: &str, sandbox: Arc<dyn SandboxPolicy>) -> Result<Self, LuaError> {
        let component = LuaComponent::from_script(script, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script.to_string()),
        })
    }

    /// Creates a test harness from a Lua script file.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to the Lua script file
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if the file cannot be read or parsed.
    pub fn from_file<P: AsRef<Path>>(
        path: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let script = std::fs::read_to_string(path.as_ref())
            .map_err(|_| LuaError::ScriptNotFound(path.as_ref().display().to_string()))?;
        let component = LuaComponent::from_file(path, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script),
        })
    }

    /// Creates a test harness from a component directory containing `init.lua`.
    ///
    /// The directory is added to Lua's search paths, enabling `require()` for
    /// co-located modules (e.g. `require("helper")` resolves `helper.lua` in
    /// the same directory).
    ///
    /// # Arguments
    ///
    /// * `dir` - Path to the component directory
    /// * `sandbox` - Sandbox policy for file operations
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if `init.lua` not found or script is invalid.
    pub fn from_dir<P: AsRef<Path>>(
        dir: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let init_path = dir.as_ref().join("init.lua");
        let script = std::fs::read_to_string(&init_path)
            .map_err(|_| LuaError::ScriptNotFound(init_path.display().to_string()))?;
        let component = LuaComponent::from_dir(dir, sandbox)?;
        Ok(Self {
            inner: ComponentTestHarness::new(component),
            script_source: Some(script),
        })
    }

    /// Returns a reference to the underlying LuaComponent.
    pub fn component(&self) -> &LuaComponent {
        self.inner.component()
    }

    /// Returns a mutable reference to the underlying LuaComponent.
    pub fn component_mut(&mut self) -> &mut LuaComponent {
        self.inner.component_mut()
    }

    /// Returns the original script source.
    pub fn script_source(&self) -> Option<&str> {
        self.script_source.as_deref()
    }

    /// Reloads the component from its script file.
    ///
    /// Only works if the component was loaded from a file.
    ///
    /// # Errors
    ///
    /// Returns [`LuaError`] if reload fails.
    pub fn reload(&mut self) -> Result<(), LuaError> {
        self.inner.component_mut().reload()
    }

    /// Calls `init()` on the component with an empty config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init(&mut self) -> Result<(), ComponentError> {
        self.inner.init()
    }

    /// Calls `init()` on the component with the given config.
    ///
    /// # Errors
    ///
    /// Returns the component's initialization error if any.
    pub fn init_with_config(&mut self, config: &serde_json::Value) -> Result<(), ComponentError> {
        self.inner.init_with_config(config)
    }

    /// Sends a request to the component.
    ///
    /// # Arguments
    ///
    /// * `category` - Event category
    /// * `operation` - Operation name
    /// * `payload` - Request payload
    ///
    /// # Returns
    ///
    /// The result of the request.
    pub fn request(
        &mut self,
        category: EventCategory,
        operation: &str,
        payload: Value,
    ) -> Result<Value, ComponentError> {
        self.inner.request(category, operation, payload)
    }

    /// Sends a Veto signal to the component.
    ///
    /// # Returns
    ///
    /// The component's response (should be `Abort` for properly implemented components).
    pub fn veto(&mut self) -> SignalResponse {
        self.inner.veto()
    }

    /// Sends a Cancel signal for the test channel.
    pub fn cancel(&mut self) -> SignalResponse {
        self.inner.cancel()
    }

    /// Sends an Approve signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    pub fn approve(&mut self, approval_id: &str) -> SignalResponse {
        self.inner.approve(approval_id)
    }

    /// Sends a Reject signal.
    ///
    /// # Arguments
    ///
    /// * `approval_id` - The approval request ID
    /// * `reason` - Optional rejection reason
    pub fn reject(&mut self, approval_id: &str, reason: Option<String>) -> SignalResponse {
        self.inner.reject(approval_id, reason)
    }

    /// Calls `abort()` on the component.
    pub fn abort(&mut self) {
        self.inner.abort();
    }

    /// Calls `shutdown()` on the component.
    pub fn shutdown(&mut self) {
        self.inner.shutdown();
    }

    /// Returns the current status of the component.
    pub fn status(&self) -> Status {
        self.inner.status()
    }

    /// Returns the component ID.
    pub fn id(&self) -> &ComponentId {
        self.inner.id()
    }

    /// Returns the request log for snapshot testing.
    pub fn request_log(&self) -> &[RequestRecord] {
        self.inner.request_log()
    }

    /// Returns the signal log for snapshot testing.
    pub fn signal_log(&self) -> &[SignalRecord] {
        self.inner.signal_log()
    }

    /// Clears all logs.
    pub fn clear_logs(&mut self) {
        self.inner.clear_logs();
    }

    /// Returns the subscribed event categories.
    pub fn subscriptions(&self) -> &[EventCategory] {
        self.component().subscriptions()
    }

    /// Injects a mock `orcs.llm()` that captures prompts and returns mock responses.
    ///
    /// Each call to `orcs.llm()` in Lua will:
    /// 1. Push the prompt string into the captured list
    /// 2. Return `{ok = true, content = <next_response>}` from the queue
    ///    (or `"mock response"` if queue is exhausted)
    ///
    /// # Returns
    ///
    /// Shared reference to captured prompts for assertion.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_llm_mock(&self, responses: Vec<String>) -> Arc<Mutex<Vec<String>>> {
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));

        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            let mock_fn = lua
                .create_function(move |lua, prompt: String| {
                    captured_clone.lock().expect("captured mutex").push(prompt);

                    let mut resps = responses.lock().expect("responses mutex");
                    let content = resps
                        .pop_front()
                        .unwrap_or_else(|| "mock response".to_string());

                    let result = lua.create_table()?;
                    result.set("ok", true)?;
                    result.set("content", content)?;
                    Ok(result)
                })
                .expect("create mock llm function");

            orcs.set("llm", mock_fn).expect("set mock orcs.llm");
        });

        captured
    }

    /// Injects a mock `orcs.request()` that returns a fixed response for a target.
    ///
    /// When Lua calls `orcs.request(target, op, payload)`, the mock checks
    /// if a response is registered for `target`; if so, returns it.
    /// Otherwise returns `{success = false, error = "mock: no handler"}`.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_request_mock(&self, handlers: Vec<(String, Value)>) {
        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            let handlers = Arc::new(handlers);

            let mock_fn = lua
                .create_function(
                    move |lua, (target, _op, _payload): (String, String, mlua::Value)| {
                        let result = lua.create_table()?;

                        for (pattern, response) in handlers.iter() {
                            if target == pattern.as_str() {
                                result.set("success", true)?;
                                let lua_val = lua.to_value(response)?;
                                result.set("data", lua_val)?;
                                return Ok(result);
                            }
                        }

                        result.set("success", false)?;
                        result.set("error", format!("mock: no handler for {target}"))?;
                        Ok(result)
                    },
                )
                .expect("create mock request function");

            orcs.set("request", mock_fn).expect("set mock orcs.request");
        });
    }

    /// Injects a mock `orcs.tool_descriptions()` that returns a fixed string.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_tool_descriptions_mock(&self, descriptions: &str) {
        let desc: Arc<String> = Arc::new(descriptions.to_string());
        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            let mock_fn = lua
                .create_function(move |_lua, ()| Ok((*desc).clone()))
                .expect("create mock tool_descriptions function");

            orcs.set("tool_descriptions", mock_fn)
                .expect("set mock orcs.tool_descriptions");
        });
    }

    /// Injects a mock `orcs.spawn_runner()` that captures arguments and returns
    /// pre-configured responses.
    ///
    /// Each call to `orcs.spawn_runner({script=..., id=...})` in Lua will:
    /// 1. Push the arguments table (as JSON Value) into the captured list
    /// 2. Return `{ok = true, fqn = <fqn>, channel_id = <channel_id>}` from the queue
    ///    (or `{ok = false, error = "mock: no more responses"}` if queue is exhausted)
    ///
    /// # Arguments
    ///
    /// * `responses` - Queue of `(fqn, channel_id)` pairs to return
    ///
    /// # Returns
    ///
    /// Shared reference to captured arguments for assertion.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_spawn_runner_mock(
        &self,
        responses: Vec<(String, String)>,
    ) -> Arc<Mutex<Vec<Value>>> {
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let responses = Arc::new(Mutex::new(VecDeque::from(responses)));

        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            let mock_fn = lua
                .create_function(move |lua, args: mlua::Value| {
                    let json_args: serde_json::Value = lua.from_value(args)?;
                    captured_clone
                        .lock()
                        .expect("captured mutex")
                        .push(json_args);

                    let result = lua.create_table()?;
                    let mut resps = responses.lock().expect("responses mutex");
                    if let Some((fqn, channel_id)) = resps.pop_front() {
                        result.set("ok", true)?;
                        result.set("fqn", fqn)?;
                        result.set("channel_id", channel_id)?;
                    } else {
                        result.set("ok", false)?;
                        result.set("error", "mock: no more spawn_runner responses")?;
                    }
                    Ok(result)
                })
                .expect("create mock spawn_runner function");

            orcs.set("spawn_runner", mock_fn)
                .expect("set mock orcs.spawn_runner");
        });

        captured
    }

    /// Injects mock MCP functions (`orcs.mcp_servers`, `orcs.mcp_tools`, `orcs.mcp_call`).
    ///
    /// - `orcs.mcp_servers()` returns `{ok = true, servers = servers}`
    /// - `orcs.mcp_tools(server?)` returns `{ok = true, tools = tools}`
    /// - `orcs.mcp_call(server, tool, args)` captures call args and returns
    ///   `{ok = true, content = [{type = "text", text = <response>}]}` from the queue
    ///
    /// # Arguments
    ///
    /// * `servers` - List of server name strings
    /// * `tools` - List of `{server, tool, description}` objects
    /// * `call_responses` - Queue of text responses for `mcp_call`
    ///
    /// # Returns
    ///
    /// Shared reference to captured `mcp_call` arguments for assertion.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_mcp_mocks(
        &self,
        servers: Vec<String>,
        tools: Vec<Value>,
        call_responses: Vec<String>,
    ) -> Arc<Mutex<Vec<Value>>> {
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let servers = Arc::new(servers);
        let tools = Arc::new(tools);
        let call_responses = Arc::new(Mutex::new(VecDeque::from(call_responses)));

        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            // orcs.mcp_servers()
            let servers_clone = Arc::clone(&servers);
            let servers_fn = lua
                .create_function(move |lua, ()| {
                    let result = lua.create_table()?;
                    result.set("ok", true)?;
                    let list = lua.create_table()?;
                    for (i, name) in servers_clone.iter().enumerate() {
                        list.set(i + 1, name.as_str())?;
                    }
                    result.set("servers", list)?;
                    Ok(result)
                })
                .expect("create mock mcp_servers function");
            orcs.set("mcp_servers", servers_fn)
                .expect("set mock orcs.mcp_servers");

            // orcs.mcp_tools(server?)
            let tools_clone = Arc::clone(&tools);
            let tools_fn = lua
                .create_function(move |lua, server_filter: Option<String>| {
                    let result = lua.create_table()?;
                    result.set("ok", true)?;
                    let list = lua.create_table()?;
                    let mut idx = 1;
                    for tool_val in tools_clone.iter() {
                        let matches = match &server_filter {
                            Some(s) => tool_val.get("server").and_then(|v| v.as_str()) == Some(s),
                            None => true,
                        };
                        if matches {
                            let lua_val = lua.to_value(tool_val)?;
                            list.set(idx, lua_val)?;
                            idx += 1;
                        }
                    }
                    result.set("tools", list)?;
                    Ok(result)
                })
                .expect("create mock mcp_tools function");
            orcs.set("mcp_tools", tools_fn)
                .expect("set mock orcs.mcp_tools");

            // orcs.mcp_call(server, tool, args)
            let responses = Arc::clone(&call_responses);
            let call_fn = lua
                .create_function(
                    move |lua, (server, tool, args): (String, String, mlua::Value)| {
                        let json_args: Value = lua.from_value(args)?;
                        captured_clone
                            .lock()
                            .expect("captured mutex")
                            .push(serde_json::json!({
                                "server": server,
                                "tool": tool,
                                "args": json_args,
                            }));

                        let result = lua.create_table()?;
                        let mut resps = responses.lock().expect("responses mutex");
                        if let Some(text) = resps.pop_front() {
                            result.set("ok", true)?;
                            let content = lua.create_table()?;
                            let item = lua.create_table()?;
                            item.set("type", "text")?;
                            item.set("text", text)?;
                            content.set(1, item)?;
                            result.set("content", content)?;
                        } else {
                            result.set("ok", false)?;
                            result.set("error", "mock: no more mcp_call responses")?;
                        }
                        Ok(result)
                    },
                )
                .expect("create mock mcp_call function");
            orcs.set("mcp_call", call_fn)
                .expect("set mock orcs.mcp_call");
        });

        captured
    }

    /// Injects no-op stubs for output-related `orcs.*` functions.
    ///
    /// Stubs the following functions that require EventEmitter (not available
    /// in test harness):
    /// - `orcs.output(msg)` — captures messages
    /// - `orcs.output_with_level(msg, level)` — captures messages
    /// - `orcs.emit_event(category, kind, payload)` — no-op
    /// - `orcs.board_recent(limit)` — returns empty table
    /// - `orcs.hook(name, fn)` — no-op
    ///
    /// # Returns
    ///
    /// Shared reference to captured output messages.
    ///
    /// # Panics
    ///
    /// Panics if the Lua state is unavailable.
    pub fn inject_output_stubs(&self) -> Arc<Mutex<Vec<String>>> {
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let captured_clone2 = Arc::clone(&captured);

        self.component().with_lua(|lua| {
            let orcs: mlua::Table = lua
                .globals()
                .get("orcs")
                .expect("orcs global table missing");

            // orcs.output(msg)
            let output_fn = lua
                .create_function(move |_, msg: String| {
                    captured_clone.lock().expect("captured mutex").push(msg);
                    Ok(())
                })
                .expect("create output stub");
            orcs.set("output", output_fn).expect("set output stub");

            // orcs.output_with_level(msg, level)
            let output_level_fn = lua
                .create_function(move |_, (msg, _level): (String, String)| {
                    captured_clone2.lock().expect("captured mutex").push(msg);
                    Ok(())
                })
                .expect("create output_with_level stub");
            orcs.set("output_with_level", output_level_fn)
                .expect("set output_with_level stub");

            // orcs.emit_event(category, kind, payload) — no-op
            let emit_fn = lua
                .create_function(|_, (_cat, _kind, _payload): (String, String, mlua::Value)| Ok(()))
                .expect("create emit_event stub");
            orcs.set("emit_event", emit_fn)
                .expect("set emit_event stub");

            // orcs.board_recent(limit) — returns empty table
            let board_fn = lua
                .create_function(|lua, _limit: mlua::Value| {
                    lua.create_table().map(mlua::Value::Table)
                })
                .expect("create board_recent stub");
            orcs.set("board_recent", board_fn)
                .expect("set board_recent stub");

            // orcs.hook(name, fn) — no-op
            let hook_fn = lua
                .create_function(|_, (_name, _callback): (String, mlua::Function)| Ok(()))
                .expect("create hook stub");
            orcs.set("hook", hook_fn).expect("set hook stub");
        });

        captured
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_policy() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    const ECHO_SCRIPT: &str = r#"
        return {
            id = "echo-test",
            subscriptions = {"Echo"},
            on_request = function(req)
                if req.operation == "echo" then
                    return { success = true, data = req.payload }
                end
                return { success = false, error = "unknown operation" }
            end,
            on_signal = function(sig)
                if sig.kind == "Veto" then
                    return "Abort"
                end
                return "Handled"
            end,
        }
    "#;

    const LIFECYCLE_SCRIPT: &str = r#"
        local state = { initialized = false, shutdown = false }

        return {
            id = "lifecycle-test",
            subscriptions = {"Lifecycle"},

            init = function()
                state.initialized = true
            end,

            shutdown = function()
                state.shutdown = true
            end,

            on_request = function(req)
                if req.operation == "get_state" then
                    return {
                        success = true,
                        data = {
                            initialized = state.initialized,
                            shutdown = state.shutdown
                        }
                    }
                end
                return { success = true }
            end,

            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn harness_from_script() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness from echo script");
        assert_eq!(harness.id().name, "echo-test");
        assert!(harness.script_source().is_some());
    }

    #[test]
    fn harness_request_success() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for request success test");

        let result = harness.request(
            EventCategory::Echo,
            "echo",
            serde_json::json!({"msg": "hello"}),
        );

        assert!(result.is_ok());
        assert_eq!(
            result.expect("should return echo payload"),
            serde_json::json!({"msg": "hello"})
        );
        assert_eq!(harness.request_log().len(), 1);
    }

    #[test]
    fn harness_request_error() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for request error test");

        let result = harness.request(EventCategory::Echo, "unknown", Value::Null);

        assert!(result.is_err());
        assert_eq!(harness.request_log().len(), 1);
    }

    #[test]
    fn harness_veto_aborts() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for veto test");

        let response = harness.veto();

        assert_eq!(response, SignalResponse::Abort);
        assert_eq!(harness.status(), Status::Aborted);
        assert_eq!(harness.signal_log().len(), 1);
    }

    #[test]
    fn harness_cancel_handled() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for cancel test");

        let response = harness.cancel();

        assert_eq!(response, SignalResponse::Handled);
        assert_eq!(harness.status(), Status::Idle);
    }

    #[test]
    fn harness_lifecycle() {
        let mut harness = LuaTestHarness::from_script(LIFECYCLE_SCRIPT, test_policy())
            .expect("should create harness for lifecycle test");

        // Init
        assert!(harness.init().is_ok());

        // Verify init was called
        let result = harness
            .request(EventCategory::Lifecycle, "get_state", Value::Null)
            .expect("should get state after init");
        assert_eq!(result["initialized"], true);
        assert_eq!(result["shutdown"], false);

        // Shutdown
        harness.shutdown();
    }

    #[test]
    fn harness_subscriptions() {
        let harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for subscriptions test");
        assert_eq!(harness.subscriptions(), &[EventCategory::Echo]);
    }

    #[test]
    fn harness_clear_logs() {
        let mut harness = LuaTestHarness::from_script(ECHO_SCRIPT, test_policy())
            .expect("should create harness for clear logs test");

        harness
            .request(EventCategory::Echo, "echo", Value::Null)
            .ok();
        harness.cancel();

        assert_eq!(harness.request_log().len(), 1);
        assert_eq!(harness.signal_log().len(), 1);

        harness.clear_logs();

        assert_eq!(harness.request_log().len(), 0);
        assert_eq!(harness.signal_log().len(), 0);
    }

    #[test]
    fn invalid_script_error() {
        let result = LuaTestHarness::from_script("invalid lua {{{{", test_policy());
        assert!(result.is_err());
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

        let result = LuaTestHarness::from_script(script, test_policy());
        assert!(result.is_err());
    }

    // Script that calls orcs.llm() and returns the result via on_request
    const LLM_CALLER_SCRIPT: &str = r#"
        return {
            id = "llm-caller",
            subscriptions = {"Echo"},
            on_request = function(req)
                local resp = orcs.llm(req.payload.prompt or "default prompt")
                if resp and resp.ok then
                    return { success = true, data = { content = resp.content } }
                end
                return { success = false, error = "llm failed" }
            end,
            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    // Script that calls orcs.request() and returns the result
    const REQUEST_CALLER_SCRIPT: &str = r#"
        return {
            id = "request-caller",
            subscriptions = {"Echo"},
            on_request = function(req)
                local target = req.payload.target or ""
                local op = req.payload.op or "status"
                local resp = orcs.request(target, op, {})
                if resp and resp.success then
                    return { success = true, data = resp.data }
                end
                return { success = false, error = resp and resp.error or "request failed" }
            end,
            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    // Script that calls orcs.tool_descriptions() and returns the result
    const TOOL_DESC_CALLER_SCRIPT: &str = r#"
        return {
            id = "tool-desc-caller",
            subscriptions = {"Echo"},
            on_request = function(req)
                local desc = orcs.tool_descriptions()
                return { success = true, data = { descriptions = desc } }
            end,
            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn inject_llm_mock_captures_prompt_and_returns_response() {
        let mut harness = LuaTestHarness::from_script(LLM_CALLER_SCRIPT, test_policy())
            .expect("llm-caller script should load");

        let captured = harness.inject_llm_mock(vec!["first reply".to_string()]);

        let result = harness
            .request(
                EventCategory::Echo,
                "call_llm",
                serde_json::json!({"prompt": "hello LLM"}),
            )
            .expect("request should succeed");

        // Verify prompt was captured
        let prompts = captured.lock().expect("captured mutex");
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], "hello LLM");

        // Verify response content
        assert_eq!(result["content"], "first reply");
    }

    #[test]
    fn inject_llm_mock_falls_back_when_queue_exhausted() {
        let mut harness = LuaTestHarness::from_script(LLM_CALLER_SCRIPT, test_policy())
            .expect("llm-caller script should load");

        let captured = harness.inject_llm_mock(vec![]); // empty queue

        let result = harness
            .request(
                EventCategory::Echo,
                "call_llm",
                serde_json::json!({"prompt": "test"}),
            )
            .expect("request should succeed");

        let prompts = captured.lock().expect("captured mutex");
        assert_eq!(prompts.len(), 1);
        assert_eq!(result["content"], "mock response");
    }

    #[test]
    fn inject_llm_mock_returns_responses_in_order() {
        let mut harness = LuaTestHarness::from_script(LLM_CALLER_SCRIPT, test_policy())
            .expect("llm-caller script should load");

        let captured = harness.inject_llm_mock(vec!["first".to_string(), "second".to_string()]);

        let r1 = harness
            .request(
                EventCategory::Echo,
                "x",
                serde_json::json!({"prompt": "p1"}),
            )
            .expect("first request");
        let r2 = harness
            .request(
                EventCategory::Echo,
                "x",
                serde_json::json!({"prompt": "p2"}),
            )
            .expect("second request");
        let r3 = harness
            .request(
                EventCategory::Echo,
                "x",
                serde_json::json!({"prompt": "p3"}),
            )
            .expect("third request (fallback)");

        assert_eq!(r1["content"], "first");
        assert_eq!(r2["content"], "second");
        assert_eq!(r3["content"], "mock response");

        let prompts = captured.lock().expect("captured mutex");
        assert_eq!(prompts.len(), 3);
        assert_eq!(prompts[0], "p1");
        assert_eq!(prompts[1], "p2");
        assert_eq!(prompts[2], "p3");
    }

    #[test]
    fn inject_request_mock_returns_matching_handler() {
        let mut harness = LuaTestHarness::from_script(REQUEST_CALLER_SCRIPT, test_policy())
            .expect("request-caller script should load");

        harness.inject_request_mock(vec![(
            "skill::skill_manager".to_string(),
            serde_json::json!({"skills": ["coding"]}),
        )]);

        let result = harness
            .request(
                EventCategory::Echo,
                "call_req",
                serde_json::json!({"target": "skill::skill_manager", "op": "list"}),
            )
            .expect("request should succeed");

        assert_eq!(result["skills"][0], "coding");
    }

    #[test]
    fn inject_request_mock_returns_error_for_unregistered_target() {
        let mut harness = LuaTestHarness::from_script(REQUEST_CALLER_SCRIPT, test_policy())
            .expect("request-caller script should load");

        harness.inject_request_mock(vec![]); // no handlers

        let result = harness.request(
            EventCategory::Echo,
            "call_req",
            serde_json::json!({"target": "unknown::component"}),
        );

        assert!(result.is_err());
    }

    #[test]
    fn inject_tool_descriptions_mock_returns_fixed_string() {
        let mut harness = LuaTestHarness::from_script(TOOL_DESC_CALLER_SCRIPT, test_policy())
            .expect("tool-desc-caller script should load");

        harness.inject_tool_descriptions_mock("## Tools\n- tool_a\n- tool_b");

        let result = harness
            .request(EventCategory::Echo, "get_tools", Value::Null)
            .expect("request should succeed");

        assert_eq!(result["descriptions"], "## Tools\n- tool_a\n- tool_b");
    }

    // ---------------------------------------------------------------
    // inject_spawn_runner_mock tests
    // ---------------------------------------------------------------

    // Script that calls orcs.spawn_runner() during on_request and exposes result
    const SPAWN_RUNNER_CALLER_SCRIPT: &str = r#"
        return {
            id = "spawn-caller",
            subscriptions = {"Echo"},
            on_request = function(req)
                local args = req.payload.spawn_args or { script = "default" }
                local result = orcs.spawn_runner(args)
                if result and result.ok then
                    return {
                        success = true,
                        data = { fqn = result.fqn, channel_id = result.channel_id }
                    }
                end
                return {
                    success = false,
                    error = result and result.error or "spawn_runner failed"
                }
            end,
            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn inject_spawn_runner_mock_captures_args_and_returns_response() {
        let mut harness = LuaTestHarness::from_script(SPAWN_RUNNER_CALLER_SCRIPT, test_policy())
            .expect("spawn-caller script should load");

        let captured = harness.inject_spawn_runner_mock(vec![(
            "agent::common-agent".to_string(),
            "ch-001".to_string(),
        )]);

        let result = harness
            .request(
                EventCategory::Echo,
                "spawn",
                serde_json::json!({"spawn_args": {"script": "worker.lua", "id": "llm"}}),
            )
            .expect("request should succeed");

        // Verify captured args
        let args = captured.lock().expect("captured mutex");
        assert_eq!(args.len(), 1);
        assert_eq!(args[0]["script"], "worker.lua");
        assert_eq!(args[0]["id"], "llm");

        // Verify returned fqn/channel_id
        assert_eq!(result["fqn"], "agent::common-agent");
        assert_eq!(result["channel_id"], "ch-001");
    }

    #[test]
    fn inject_spawn_runner_mock_returns_error_when_queue_exhausted() {
        let mut harness = LuaTestHarness::from_script(SPAWN_RUNNER_CALLER_SCRIPT, test_policy())
            .expect("spawn-caller script should load");

        let captured = harness.inject_spawn_runner_mock(vec![]); // empty queue

        let result = harness.request(
            EventCategory::Echo,
            "spawn",
            serde_json::json!({"spawn_args": {"script": "x.lua"}}),
        );

        // Should fail because mock has no responses
        assert!(result.is_err());

        // Args should still be captured
        let args = captured.lock().expect("captured mutex");
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn inject_spawn_runner_mock_consumes_queue_in_order() {
        let mut harness = LuaTestHarness::from_script(SPAWN_RUNNER_CALLER_SCRIPT, test_policy())
            .expect("spawn-caller script should load");

        let captured = harness.inject_spawn_runner_mock(vec![
            ("ns::first".to_string(), "ch-A".to_string()),
            ("ns::second".to_string(), "ch-B".to_string()),
        ]);

        let r1 = harness
            .request(
                EventCategory::Echo,
                "spawn",
                serde_json::json!({"spawn_args": {"script": "a.lua"}}),
            )
            .expect("first spawn should succeed");

        let r2 = harness
            .request(
                EventCategory::Echo,
                "spawn",
                serde_json::json!({"spawn_args": {"script": "b.lua"}}),
            )
            .expect("second spawn should succeed");

        // Third call exhausts queue
        let r3 = harness.request(
            EventCategory::Echo,
            "spawn",
            serde_json::json!({"spawn_args": {"script": "c.lua"}}),
        );

        assert_eq!(r1["fqn"], "ns::first");
        assert_eq!(r1["channel_id"], "ch-A");
        assert_eq!(r2["fqn"], "ns::second");
        assert_eq!(r2["channel_id"], "ch-B");
        assert!(r3.is_err());

        let args = captured.lock().expect("captured mutex");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0]["script"], "a.lua");
        assert_eq!(args[1]["script"], "b.lua");
        assert_eq!(args[2]["script"], "c.lua");
    }

    // ---------------------------------------------------------------
    // inject_output_stubs tests
    // ---------------------------------------------------------------

    // Script that exercises all output-related orcs.* stubs
    const OUTPUT_STUBS_SCRIPT: &str = r#"
        return {
            id = "output-exerciser",
            subscriptions = {"Echo"},
            on_request = function(req)
                local op = req.operation

                if op == "test_output" then
                    orcs.output("hello from output")
                    return { success = true }
                elseif op == "test_output_with_level" then
                    orcs.output_with_level("warn message", "warn")
                    return { success = true }
                elseif op == "test_emit_event" then
                    orcs.emit_event("TestCat", "TestKind", { key = "val" })
                    return { success = true }
                elseif op == "test_board_recent" then
                    local items = orcs.board_recent(10)
                    return { success = true, data = { count = #items } }
                elseif op == "test_hook" then
                    orcs.hook("before_action", function() end)
                    return { success = true }
                elseif op == "test_all" then
                    orcs.output("msg1")
                    orcs.output_with_level("msg2", "info")
                    orcs.emit_event("Cat", "Kind", {})
                    local b = orcs.board_recent(5)
                    orcs.hook("h", function() end)
                    return { success = true, data = { output_count = 2, board_size = #b } }
                end
                return { success = false, error = "unknown op" }
            end,
            on_signal = function(sig)
                return "Handled"
            end,
        }
    "#;

    #[test]
    fn inject_output_stubs_captures_output_messages() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        let captured = harness.inject_output_stubs();

        harness
            .request(EventCategory::Echo, "test_output", Value::Null)
            .expect("test_output should succeed");

        let msgs = captured.lock().expect("captured mutex");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "hello from output");
    }

    #[test]
    fn inject_output_stubs_captures_output_with_level_messages() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        let captured = harness.inject_output_stubs();

        harness
            .request(EventCategory::Echo, "test_output_with_level", Value::Null)
            .expect("test_output_with_level should succeed");

        let msgs = captured.lock().expect("captured mutex");
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0], "warn message");
    }

    #[test]
    fn inject_output_stubs_emit_event_does_not_panic() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        harness.inject_output_stubs();

        harness
            .request(EventCategory::Echo, "test_emit_event", Value::Null)
            .expect("emit_event stub should not fail");
    }

    #[test]
    fn inject_output_stubs_board_recent_returns_empty_table() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        harness.inject_output_stubs();

        let result = harness
            .request(EventCategory::Echo, "test_board_recent", Value::Null)
            .expect("board_recent stub should not fail");

        assert_eq!(result["count"], 0);
    }

    #[test]
    fn inject_output_stubs_hook_does_not_panic() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        harness.inject_output_stubs();

        harness
            .request(EventCategory::Echo, "test_hook", Value::Null)
            .expect("hook stub should not fail");
    }

    #[test]
    fn inject_output_stubs_all_stubs_work_together() {
        let mut harness = LuaTestHarness::from_script(OUTPUT_STUBS_SCRIPT, test_policy())
            .expect("output-exerciser script should load");

        let captured = harness.inject_output_stubs();

        let result = harness
            .request(EventCategory::Echo, "test_all", Value::Null)
            .expect("all stubs should work together");

        // Both output + output_with_level captured
        let msgs = captured.lock().expect("captured mutex");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], "msg1");
        assert_eq!(msgs[1], "msg2");

        assert_eq!(result["output_count"], 2);
        assert_eq!(result["board_size"], 0);
    }
}

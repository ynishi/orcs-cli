//! LuaChild implementation.
//!
//! Wraps a Lua table to implement the Child trait.
//!
//! # RunnableChild Support
//!
//! LuaChild can implement [`RunnableChild`] if the Lua table contains
//! a `run` function. This enables the child to actively execute work.
//!
//! ```lua
//! -- Worker child with run capability
//! return {
//!     id = "worker-1",
//!
//!     run = function(input)
//!         -- Do work
//!         return { result = input.value * 2 }
//!     end,
//!
//!     on_signal = function(sig)
//!         if sig.kind == "Veto" then return "Abort" end
//!         return "Handled"
//!     end,
//! }
//! ```

use crate::error::LuaError;
use crate::orcs_helpers::register_base_orcs_functions;
use crate::types::{parse_signal_response, parse_status, LuaResponse, LuaSignal};
use mlua::{Function, Lua, RegistryKey, Table, Value};
use orcs_component::{
    Child, ChildConfig, ChildContext, ChildError, ChildResult, Identifiable, RunnableChild,
    SignalReceiver, Status, Statusable,
};
use orcs_event::{Signal, SignalResponse};
use orcs_runtime::sandbox::SandboxPolicy;
use std::sync::{Arc, Mutex};

/// A child entity implemented in Lua.
///
/// Can be used inside a LuaComponent to manage child entities.
///
/// # Lua Table Format
///
/// ## Basic Child (no run capability)
///
/// ```lua
/// {
///     id = "child-1",
///     status = "Idle",  -- Optional, defaults to "Idle"
///
///     on_signal = function(sig)
///         return "Handled" | "Ignored" | "Abort"
///     end,
///
///     abort = function()  -- Optional
///     end,
/// }
/// ```
///
/// ## Runnable Child (with run capability)
///
/// ```lua
/// {
///     id = "worker-1",
///
///     run = function(input)
///         -- Perform work
///         return { success = true, data = { result = input.value * 2 } }
///     end,
///
///     on_signal = function(sig)
///         return "Handled"
///     end,
/// }
/// ```
pub struct LuaChild {
    /// Shared Lua runtime (from parent component).
    lua: Arc<Mutex<Lua>>,
    /// Child identifier.
    id: String,
    /// Current status.
    status: Status,
    /// Registry key for on_signal callback.
    on_signal_key: RegistryKey,
    /// Registry key for abort callback (optional).
    abort_key: Option<RegistryKey>,
    /// Registry key for run callback (optional, makes child runnable).
    run_key: Option<RegistryKey>,
    /// Runtime context for spawning sub-children and emitting output.
    context: Option<Box<dyn ChildContext>>,
}

// SAFETY: LuaChild can be safely sent between threads and accessed concurrently.
//
// Justification:
// 1. mlua is built with "send" feature (see Cargo.toml), which enables thread-safe
//    Lua state allocation.
// 2. The Lua runtime is wrapped in Arc<Mutex<Lua>>, ensuring exclusive access.
//    All methods that access the Lua state acquire the lock first.
// 3. Lua callbacks are stored in the registry via RegistryKey, which is Send.
// 4. No raw Lua values escape the Mutex guard scope.
// 5. The remaining fields (id, status) are all Send+Sync.
unsafe impl Send for LuaChild {}
unsafe impl Sync for LuaChild {}

impl LuaChild {
    /// Creates a LuaChild from a Lua table.
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `table` - Lua table defining the child
    ///
    /// # Errors
    ///
    /// Returns error if table is missing required fields.
    pub fn from_table(
        lua: Arc<Mutex<Lua>>,
        table: Table,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let lua_guard = lua.lock().map_err(|e| {
            LuaError::Runtime(mlua::Error::ExternalError(Arc::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        // Register base orcs functions (log, exec, read, write, grep, glob)
        register_base_orcs_functions(&lua_guard, sandbox)?;

        // Extract id
        let id: String = table
            .get("id")
            .map_err(|_| LuaError::MissingCallback("id".to_string()))?;

        // Extract status (optional)
        let status_str: String = table.get("status").unwrap_or_else(|_| "Idle".to_string());
        let status = parse_status(&status_str);

        // Extract on_signal callback
        let on_signal_fn: Function = table
            .get("on_signal")
            .map_err(|_| LuaError::MissingCallback("on_signal".to_string()))?;

        let on_signal_key = lua_guard.create_registry_value(on_signal_fn)?;

        // Extract abort callback (optional)
        let abort_key = table
            .get::<Function>("abort")
            .ok()
            .map(|f| lua_guard.create_registry_value(f))
            .transpose()?;

        // Extract run callback (optional, makes child runnable)
        let run_key = table
            .get::<Function>("run")
            .ok()
            .map(|f| lua_guard.create_registry_value(f))
            .transpose()?;

        drop(lua_guard);

        Ok(Self {
            lua,
            id,
            status,
            on_signal_key,
            abort_key,
            run_key,
            context: None,
        })
    }

    /// Sets the runtime context for this child.
    ///
    /// The context enables spawning sub-children and emitting output.
    /// Call this before running the child.
    pub fn set_context(&mut self, context: Box<dyn ChildContext>) {
        self.context = Some(context);
    }

    /// Returns `true` if this child has a context set.
    #[must_use]
    pub fn has_context(&self) -> bool {
        self.context.is_some()
    }

    /// Creates a LuaChild from a Lua table, requiring a run callback.
    ///
    /// Use this when you need a [`RunnableChild`] that can execute work.
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `table` - Lua table defining the child (must have `run` function)
    ///
    /// # Errors
    ///
    /// Returns error if table is missing required fields including `run`.
    pub fn from_table_runnable(
        lua: Arc<Mutex<Lua>>,
        table: Table,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        // Check if run exists before full parsing
        if table.get::<Function>("run").is_err() {
            return Err(LuaError::MissingCallback("run".to_string()));
        }
        Self::from_table(lua, table, sandbox)
    }

    /// Returns `true` if this child has a run callback (is runnable).
    #[must_use]
    pub fn is_runnable(&self) -> bool {
        self.run_key.is_some()
    }

    /// Creates a simple LuaChild with just an ID.
    ///
    /// The on_signal callback will return Ignored for all signals.
    pub fn simple(
        lua: Arc<Mutex<Lua>>,
        id: impl Into<String>,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let id = id.into();
        let lua_guard = lua.lock().map_err(|e| {
            LuaError::Runtime(mlua::Error::ExternalError(Arc::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        // Register base orcs functions (log, exec, read, write, grep, glob)
        register_base_orcs_functions(&lua_guard, sandbox)?;

        // Create a simple on_signal function that returns "Ignored"
        let on_signal_fn = lua_guard.create_function(|_, _: mlua::Value| Ok("Ignored"))?;

        let on_signal_key = lua_guard.create_registry_value(on_signal_fn)?;

        drop(lua_guard);

        Ok(Self {
            lua,
            id,
            status: Status::Idle,
            on_signal_key,
            abort_key: None,
            run_key: None,
            context: None,
        })
    }

    /// Creates a LuaChild from inline script content.
    ///
    /// The script should return a Lua table with the child definition.
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `script` - Inline Lua script
    ///
    /// # Errors
    ///
    /// Returns error if script is invalid or missing required fields.
    pub fn from_script(
        lua: Arc<Mutex<Lua>>,
        script: &str,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let lua_guard = lua.lock().map_err(|e| {
            LuaError::Runtime(mlua::Error::ExternalError(Arc::new(std::io::Error::other(
                e.to_string(),
            ))))
        })?;

        let table: Table = lua_guard
            .load(script)
            .eval()
            .map_err(|e| LuaError::InvalidScript(e.to_string()))?;

        drop(lua_guard);

        Self::from_table(lua, table, sandbox)
    }
}

impl Identifiable for LuaChild {
    fn id(&self) -> &str {
        &self.id
    }
}

impl SignalReceiver for LuaChild {
    fn on_signal(&mut self, signal: &Signal) -> SignalResponse {
        let Ok(lua) = self.lua.lock() else {
            return SignalResponse::Ignored;
        };

        // Register context functions if context is available
        // This allows on_signal to use orcs.emit_output, etc.
        if let Some(ctx) = &self.context {
            if let Err(e) = register_context_functions(&lua, ctx.clone_box()) {
                tracing::warn!("Failed to register context functions in on_signal: {}", e);
            }
        }

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
                tracing::warn!("Lua child on_signal error: {}", e);
                SignalResponse::Ignored
            }
        }
    }

    fn abort(&mut self) {
        self.status = Status::Aborted;

        // Call Lua abort if available
        if let Some(abort_key) = &self.abort_key {
            if let Ok(lua) = self.lua.lock() {
                // Register context functions if context is available
                // This allows abort() to use orcs.emit_output, etc.
                if let Some(ctx) = &self.context {
                    if let Err(e) = register_context_functions(&lua, ctx.clone_box()) {
                        tracing::warn!("Failed to register context functions in abort: {}", e);
                    }
                }

                if let Ok(abort_fn) = lua.registry_value::<Function>(abort_key) {
                    if let Err(e) = abort_fn.call::<()>(()) {
                        tracing::warn!("Lua child abort error: {}", e);
                    }
                }
            }
        }
    }
}

impl Statusable for LuaChild {
    fn status(&self) -> Status {
        self.status
    }
}

impl Child for LuaChild {}

impl RunnableChild for LuaChild {
    fn run(&mut self, input: serde_json::Value) -> ChildResult {
        // Check if we have a run callback
        let Some(run_key) = &self.run_key else {
            return ChildResult::Err(ChildError::ExecutionFailed {
                reason: "child is not runnable (no run callback)".into(),
            });
        };

        // Update status
        self.status = Status::Running;

        // Get Lua lock
        let lua = match self.lua.lock() {
            Ok(l) => l,
            Err(e) => {
                self.status = Status::Error;
                return ChildResult::Err(ChildError::Internal(format!("lua lock failed: {}", e)));
            }
        };

        // Register context-dependent functions if context is available
        if let Some(ctx) = &self.context {
            if let Err(e) = register_context_functions(&lua, ctx.clone_box()) {
                drop(lua);
                self.status = Status::Error;
                return ChildResult::Err(ChildError::Internal(format!(
                    "failed to register context functions: {}",
                    e
                )));
            }
        }

        // Get run function from registry
        let run_fn: Function = match lua.registry_value(run_key) {
            Ok(f) => f,
            Err(e) => {
                drop(lua);
                self.status = Status::Error;
                return ChildResult::Err(ChildError::Internal(format!(
                    "run callback not found: {}",
                    e
                )));
            }
        };

        // Convert input to Lua value
        let lua_input = match serde_json_to_lua(&input, &lua) {
            Ok(v) => v,
            Err(e) => {
                drop(lua);
                self.status = Status::Error;
                return ChildResult::Err(ChildError::InvalidInput(format!(
                    "failed to convert input: {}",
                    e
                )));
            }
        };

        // Call the run function
        let result: Result<LuaResponse, _> = run_fn.call(lua_input);

        drop(lua);

        match result {
            Ok(response) => {
                if response.success {
                    self.status = Status::Idle;
                    ChildResult::Ok(response.data.unwrap_or(serde_json::Value::Null))
                } else {
                    self.status = Status::Error;
                    ChildResult::Err(ChildError::ExecutionFailed {
                        reason: response.error.unwrap_or_else(|| "unknown error".into()),
                    })
                }
            }
            Err(e) => {
                // Check if it was aborted
                if self.status == Status::Aborted {
                    ChildResult::Aborted
                } else {
                    self.status = Status::Error;
                    ChildResult::Err(ChildError::ExecutionFailed {
                        reason: format!("run callback failed: {}", e),
                    })
                }
            }
        }
    }
}

/// Wrapper to store ChildContext in Lua's app_data.
struct ContextWrapper(Mutex<Box<dyn ChildContext>>);

/// Register context-dependent functions in Lua's orcs table.
///
/// This adds:
/// - `orcs.spawn_child(config)` - Spawn a sub-child
/// - `orcs.emit_output(message, [level])` - Emit output to parent
/// - `orcs.child_count()` - Get current child count
/// - `orcs.max_children()` - Get max allowed children
fn register_context_functions(lua: &Lua, ctx: Box<dyn ChildContext>) -> Result<(), mlua::Error> {
    // Store context in app_data
    lua.set_app_data(ContextWrapper(Mutex::new(ctx)));

    // Get or create orcs table
    let orcs_table: Table = lua.globals().get("orcs").unwrap_or_else(|_| {
        let t = lua.create_table().expect("create table");
        lua.globals().set("orcs", t.clone()).expect("set orcs");
        t
    });

    // orcs.spawn_child(config) -> { ok, id, error }
    // config = { id = "child-id", script = "..." } or { id = "child-id", path = "..." }
    let spawn_child_fn = lua.create_function(|lua, config: Table| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper
            .0
            .lock()
            .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

        // Parse config
        let id: String = config
            .get("id")
            .map_err(|_| mlua::Error::RuntimeError("config.id required".into()))?;

        let child_config = if let Ok(script) = config.get::<String>("script") {
            ChildConfig::from_inline(&id, script)
        } else if let Ok(path) = config.get::<String>("path") {
            ChildConfig::from_file(&id, path)
        } else {
            ChildConfig::new(&id)
        };

        // Spawn the child
        let result = lua.create_table()?;
        match ctx.spawn_child(child_config) {
            Ok(handle) => {
                result.set("ok", true)?;
                result.set("id", handle.id().to_string())?;
            }
            Err(e) => {
                result.set("ok", false)?;
                result.set("error", e.to_string())?;
            }
        }

        Ok(result)
    })?;
    orcs_table.set("spawn_child", spawn_child_fn)?;

    // orcs.emit_output(message, [level])
    let emit_output_fn =
        lua.create_function(|lua, (message, level): (String, Option<String>)| {
            let wrapper = lua
                .app_data_ref::<ContextWrapper>()
                .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

            let ctx = wrapper
                .0
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            match level {
                Some(lvl) => ctx.emit_output_with_level(&message, &lvl),
                None => ctx.emit_output(&message),
            }

            Ok(())
        })?;
    orcs_table.set("emit_output", emit_output_fn)?;

    // orcs.child_count() -> number
    let child_count_fn = lua.create_function(|lua, ()| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper
            .0
            .lock()
            .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

        Ok(ctx.child_count())
    })?;
    orcs_table.set("child_count", child_count_fn)?;

    // orcs.max_children() -> number
    let max_children_fn = lua.create_function(|lua, ()| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper
            .0
            .lock()
            .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

        Ok(ctx.max_children())
    })?;
    orcs_table.set("max_children", max_children_fn)?;

    Ok(())
}

/// Convert serde_json::Value to Lua Value.
fn serde_json_to_lua(value: &serde_json::Value, lua: &Lua) -> Result<Value, mlua::Error> {
    match value {
        serde_json::Value::Null => Ok(Value::Nil),
        serde_json::Value::Bool(b) => Ok(Value::Boolean(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Value::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Value::Number(f))
            } else {
                Err(mlua::Error::SerializeError("invalid number".into()))
            }
        }
        serde_json::Value::String(s) => Ok(Value::String(lua.create_string(s)?)),
        serde_json::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.raw_set(i + 1, serde_json_to_lua(v, lua)?)?;
            }
            Ok(Value::Table(table))
        }
        serde_json::Value::Object(obj) => {
            let table = lua.create_table()?;
            for (k, v) in obj {
                table.set(k.as_str(), serde_json_to_lua(v, lua)?)?;
            }
            Ok(Value::Table(table))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    #[test]
    fn child_identifiable() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

        assert_eq!(child.id(), "child-1");
    }

    #[test]
    fn child_statusable() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn child_abort_changes_status() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let mut child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

        child.abort();
        assert_eq!(child.status(), Status::Aborted);
    }

    #[test]
    fn child_is_object_safe() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

        // Should compile - proves Child is object-safe
        let _boxed: Box<dyn Child> = Box::new(child);
    }

    // --- RunnableChild tests ---

    #[test]
    fn simple_child_is_not_runnable() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

        assert!(!child.is_runnable());
    }

    #[test]
    fn runnable_child_from_script() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let script = r#"
            return {
                id = "worker",
                run = function(input)
                    return { success = true, data = { doubled = input.value * 2 } }
                end,
                on_signal = function(sig)
                    return "Handled"
                end,
            }
        "#;

        let child = LuaChild::from_script(lua, script, test_sandbox()).expect("create child");
        assert!(child.is_runnable());
        assert_eq!(child.id(), "worker");
    }

    #[test]
    fn runnable_child_run_success() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let script = r#"
            return {
                id = "worker",
                run = function(input)
                    return { success = true, data = { result = input.value + 10 } }
                end,
                on_signal = function(sig)
                    return "Handled"
                end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).expect("create child");
        let input = serde_json::json!({"value": 5});
        let result = child.run(input);

        assert!(result.is_ok());
        if let ChildResult::Ok(data) = result {
            assert_eq!(data["result"], 15);
        }
        assert_eq!(child.status(), Status::Idle);
    }

    #[test]
    fn runnable_child_run_error() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let script = r#"
            return {
                id = "failing-worker",
                run = function(input)
                    return { success = false, error = "something went wrong" }
                end,
                on_signal = function(sig)
                    return "Handled"
                end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).expect("create child");
        let result = child.run(serde_json::json!({}));

        assert!(result.is_err());
        if let ChildResult::Err(err) = result {
            assert!(err.to_string().contains("something went wrong"));
            assert_eq!(err.kind(), "execution_failed");
        }
        assert_eq!(child.status(), Status::Error);
    }

    #[test]
    fn non_runnable_child_run_returns_error() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let mut child =
            LuaChild::simple(lua, "simple-child", test_sandbox()).expect("create child");

        let result = child.run(serde_json::json!({}));
        assert!(result.is_err());
        if let ChildResult::Err(err) = result {
            assert!(err.to_string().contains("not runnable"));
            assert_eq!(err.kind(), "execution_failed");
        }
    }

    #[test]
    fn from_table_runnable_requires_run() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let lua_guard = lua.lock().unwrap();
        let table = lua_guard.create_table().unwrap();
        table.set("id", "test").unwrap();
        // Create a simple on_signal function
        let on_signal_fn = lua_guard
            .create_function(|_, _: mlua::Value| Ok("Ignored"))
            .unwrap();
        table.set("on_signal", on_signal_fn).unwrap();
        // Note: no run function

        drop(lua_guard);

        let result = LuaChild::from_table_runnable(lua, table, test_sandbox());
        assert!(result.is_err());
    }

    #[test]
    fn runnable_child_is_object_safe() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let script = r#"
            return {
                id = "worker",
                run = function(input) return input end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let child = LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

        // Should compile - proves RunnableChild is object-safe
        let _boxed: Box<dyn RunnableChild> = Box::new(child);
    }

    #[test]
    fn run_with_complex_input() {
        let lua = Arc::new(Mutex::new(Lua::new()));
        let script = r#"
            return {
                id = "complex-worker",
                run = function(input)
                    local sum = 0
                    for i, v in ipairs(input.numbers) do
                        sum = sum + v
                    end
                    return {
                        success = true,
                        data = {
                            sum = sum,
                            name = input.name,
                            nested = { ok = true }
                        }
                    }
                end,
                on_signal = function(sig) return "Handled" end,
            }
        "#;

        let mut child = LuaChild::from_script(lua, script, test_sandbox()).expect("create child");
        let input = serde_json::json!({
            "name": "test",
            "numbers": [1, 2, 3, 4, 5]
        });
        let result = child.run(input);

        assert!(result.is_ok());
        if let ChildResult::Ok(data) = result {
            assert_eq!(data["sum"], 15);
            assert_eq!(data["name"], "test");
            assert_eq!(data["nested"]["ok"], true);
        }
    }

    // --- Context tests ---

    mod context_tests {
        use super::*;
        use orcs_component::{ChildHandle, SpawnError};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Mock ChildContext for testing.
        #[derive(Debug)]
        struct MockContext {
            parent_id: String,
            spawn_count: Arc<AtomicUsize>,
            emit_count: Arc<AtomicUsize>,
            max_children: usize,
        }

        impl MockContext {
            fn new(parent_id: &str) -> Self {
                Self {
                    parent_id: parent_id.into(),
                    spawn_count: Arc::new(AtomicUsize::new(0)),
                    emit_count: Arc::new(AtomicUsize::new(0)),
                    max_children: 10,
                }
            }
        }

        /// Mock child handle for testing.
        #[derive(Debug)]
        struct MockHandle {
            id: String,
        }

        impl ChildHandle for MockHandle {
            fn id(&self) -> &str {
                &self.id
            }

            fn status(&self) -> Status {
                Status::Idle
            }

            fn run_sync(
                &mut self,
                _input: serde_json::Value,
            ) -> Result<ChildResult, orcs_component::RunError> {
                Ok(ChildResult::Ok(serde_json::Value::Null))
            }

            fn abort(&mut self) {}

            fn is_finished(&self) -> bool {
                false
            }
        }

        impl ChildContext for MockContext {
            fn parent_id(&self) -> &str {
                &self.parent_id
            }

            fn emit_output(&self, _message: &str) {
                self.emit_count.fetch_add(1, Ordering::SeqCst);
            }

            fn emit_output_with_level(&self, _message: &str, _level: &str) {
                self.emit_count.fetch_add(1, Ordering::SeqCst);
            }

            fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
                self.spawn_count.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(MockHandle { id: config.id }))
            }

            fn child_count(&self) -> usize {
                self.spawn_count.load(Ordering::SeqCst)
            }

            fn max_children(&self) -> usize {
                self.max_children
            }

            fn send_to_child(
                &self,
                _child_id: &str,
                _input: serde_json::Value,
            ) -> Result<ChildResult, orcs_component::RunError> {
                Ok(ChildResult::Ok(serde_json::json!({"mock": true})))
            }

            fn clone_box(&self) -> Box<dyn ChildContext> {
                Box::new(Self {
                    parent_id: self.parent_id.clone(),
                    spawn_count: Arc::clone(&self.spawn_count),
                    emit_count: Arc::clone(&self.emit_count),
                    max_children: self.max_children,
                })
            }
        }

        #[test]
        fn set_context() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let mut child = LuaChild::simple(lua, "child-1", test_sandbox()).expect("create child");

            assert!(!child.has_context());

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            assert!(child.has_context());
        }

        #[test]
        fn emit_output_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "emitter",
                    run = function(input)
                        orcs.emit_output("Hello from Lua!")
                        orcs.emit_output("Warning!", "warn")
                        return { success = true }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            let ctx = MockContext::new("parent");
            let emit_count = Arc::clone(&ctx.emit_count);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok());
            assert_eq!(emit_count.load(Ordering::SeqCst), 2);
        }

        #[test]
        fn child_count_and_max_children_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "counter",
                    run = function(input)
                        local count = orcs.child_count()
                        local max = orcs.max_children()
                        return { success = true, data = { count = count, max = max } }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok());
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["count"], 0);
                assert_eq!(data["max"], 10);
            }
        }

        #[test]
        fn spawn_child_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "spawner",
                    run = function(input)
                        local result = orcs.spawn_child({ id = "sub-child-1" })
                        if result.ok then
                            return { success = true, data = { spawned_id = result.id } }
                        else
                            return { success = false, error = result.error }
                        end
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            let ctx = MockContext::new("parent");
            let spawn_count = Arc::clone(&ctx.spawn_count);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok());
            assert_eq!(spawn_count.load(Ordering::SeqCst), 1);

            if let ChildResult::Ok(data) = result {
                assert_eq!(data["spawned_id"], "sub-child-1");
            }
        }

        #[test]
        fn spawn_child_with_inline_script() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "spawner",
                    run = function(input)
                        local result = orcs.spawn_child({
                            id = "inline-child",
                            script = "return { run = function(i) return i end }"
                        })
                        return { success = result.ok, data = { id = result.id } }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok());
        }

        #[test]
        fn no_context_functions_without_context() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "no-context",
                    run = function(input)
                        -- orcs table or emit_output should not exist without context
                        if orcs and orcs.emit_output then
                            return { success = false, error = "emit_output should not exist" }
                        end
                        return { success = true }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");
            // Note: NOT setting context

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok());
        }
    }
}

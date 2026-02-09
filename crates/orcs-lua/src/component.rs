//! LuaComponent implementation.
//!
//! Wraps a Lua script to implement the Component trait.

mod ctx_fns;
mod emitter_fns;

use crate::error::LuaError;
use crate::lua_env::LuaEnv;
use crate::types::{
    parse_event_category, parse_signal_response, LuaRequest, LuaResponse, LuaSignal,
};
use mlua::{Function, Lua, RegistryKey, Table};
use orcs_component::{
    ChildContext, Component, ComponentError, ComponentLoader, Emitter, EventCategory, RuntimeHints,
    SpawnError, Status,
};
use orcs_event::{Request, Signal, SignalResponse};
use orcs_runtime::sandbox::SandboxPolicy;
use orcs_types::ComponentId;
use serde_json::Value as JsonValue;
use std::path::Path;
use std::sync::{Arc, Mutex};

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
    /// Event emitter for ChannelRunner mode.
    ///
    /// When set, allows Lua scripts to emit events via `orcs.output()`.
    /// This enables ChannelRunner-based execution (IO-less, event-only).
    emitter: Option<Arc<Mutex<Box<dyn Emitter>>>>,
    /// Child context for spawning and managing children.
    ///
    /// When set, allows Lua scripts to spawn children via `orcs.spawn_child()`.
    /// This enables the Manager-Worker pattern where Components manage Children.
    child_context: Option<Arc<Mutex<Box<dyn ChildContext>>>>,
    /// Sandbox policy for file operations.
    ///
    /// Injected at construction time. Used by `orcs.read/write/grep/glob` in Lua.
    /// Stored for use during `reload()`.
    sandbox: Arc<dyn SandboxPolicy>,
    /// Runtime hints declared by the Lua script.
    hints: RuntimeHints,
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
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Script file not found
    /// - Script syntax error
    /// - Missing required fields/callbacks
    pub fn from_file<P: AsRef<Path>>(
        path: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let path = path.as_ref();
        let script = std::fs::read_to_string(path)
            .map_err(|_| LuaError::ScriptNotFound(path.display().to_string()))?;

        let script_dir = path.parent().map(|p| p.to_path_buf());
        let mut component = Self::from_script_inner(&script, sandbox, script_dir.as_deref())?;
        component.script_path = Some(path.display().to_string());
        Ok(component)
    }

    /// Creates a new LuaComponent from a directory containing `init.lua`.
    ///
    /// The directory is added to Lua's `package.path`, enabling standard
    /// `require()` for co-located modules (e.g. `require("lib.my_module")`).
    ///
    /// # Directory Structure
    ///
    /// ```text
    /// components/my_component/
    ///   init.lua              -- entry point (must return component table)
    ///   lib/
    ///     helper.lua          -- require("lib.helper")
    ///   vendor/
    ///     lua_solver/init.lua -- require("vendor.lua_solver")
    /// ```
    ///
    /// # Errors
    ///
    /// Returns error if `init.lua` not found or script is invalid.
    pub fn from_dir<P: AsRef<Path>>(
        dir: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let dir = dir.as_ref();
        let init_path = dir.join("init.lua");
        let script = std::fs::read_to_string(&init_path)
            .map_err(|_| LuaError::ScriptNotFound(init_path.display().to_string()))?;

        let mut component = Self::from_script_inner(&script, sandbox, Some(dir))?;
        component.script_path = Some(init_path.display().to_string());
        Ok(component)
    }

    /// Creates a new LuaComponent from a script string.
    ///
    /// # Arguments
    ///
    /// * `script` - Lua script content
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
    ///
    /// # Errors
    ///
    /// Returns error if script is invalid.
    pub fn from_script(script: &str, sandbox: Arc<dyn SandboxPolicy>) -> Result<Self, LuaError> {
        Self::from_script_inner(script, sandbox, None)
    }

    /// Internal: creates a LuaComponent with optional search path setup.
    ///
    /// When `script_dir` is provided, it is added to `LuaEnv`'s search paths
    /// so that `require()` resolves co-located modules with sandbox validation.
    fn from_script_inner(
        script: &str,
        sandbox: Arc<dyn SandboxPolicy>,
        script_dir: Option<&Path>,
    ) -> Result<Self, LuaError> {
        // Build LuaEnv with sandbox and optional script directory as search path.
        let mut lua_env = LuaEnv::new(Arc::clone(&sandbox));
        if let Some(dir) = script_dir {
            lua_env = lua_env.with_search_path(dir);
        }

        // Create configured Lua VM (orcs.*, tools, sandboxed require).
        let lua = lua_env.create_lua()?;

        // Register Component-specific output placeholders.
        // These are overridden by real emitter functions via set_emitter().
        {
            let orcs_table: Table = lua.globals().get("orcs")?;
            let output_noop = lua.create_function(|_, msg: String| {
                tracing::debug!("[lua] orcs.output called without emitter: {}", msg);
                Ok(())
            })?;
            orcs_table.set("output", output_noop)?;

            let output_level_noop = lua.create_function(|_, (msg, _level): (String, String)| {
                tracing::debug!(
                    "[lua] orcs.output_with_level called without emitter: {}",
                    msg
                );
                Ok(())
            })?;
            orcs_table.set("output_with_level", output_level_noop)?;
        }

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

        // Extract runtime hints (all optional, default false)
        let hints = RuntimeHints {
            output_to_io: component_table.get("output_to_io").unwrap_or(false),
            elevated: component_table.get("elevated").unwrap_or(false),
            child_spawner: component_table.get("child_spawner").unwrap_or(false),
        };

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
            emitter: None,
            child_context: None,
            sandbox,
            hints,
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

        let new_component = Self::from_file(path, Arc::clone(&self.sandbox))?;

        // Swap internals (preserve emitter)
        self.lua = new_component.lua;
        self.subscriptions = new_component.subscriptions;
        self.on_request_key = new_component.on_request_key;
        self.on_signal_key = new_component.on_signal_key;
        self.init_key = new_component.init_key;
        self.shutdown_key = new_component.shutdown_key;
        // Note: emitter is preserved across reload

        // Re-register orcs.output if emitter is set
        if let Some(emitter) = &self.emitter {
            let lua = self
                .lua
                .lock()
                .map_err(|e| LuaError::InvalidScript(format!("lua mutex poisoned: {}", e)))?;
            emitter_fns::register(&lua, Arc::clone(emitter))?;
        }

        // Re-register child context functions if child_context is set
        if let Some(ctx) = &self.child_context {
            let lua = self
                .lua
                .lock()
                .map_err(|e| LuaError::InvalidScript(format!("lua mutex poisoned: {}", e)))?;
            ctx_fns::register(&lua, Arc::clone(ctx), Arc::clone(&self.sandbox))?;
        }

        tracing::info!("Reloaded Lua component: {}", self.id);
        Ok(())
    }

    /// Returns whether this component has an emitter set.
    ///
    /// When true, the component can emit events via `orcs.output()`.
    #[must_use]
    pub fn has_emitter(&self) -> bool {
        self.emitter.is_some()
    }

    /// Returns whether this component has a child context set.
    ///
    /// When true, the component can spawn children via `orcs.spawn_child()`.
    #[must_use]
    pub fn has_child_context(&self) -> bool {
        self.child_context.is_some()
    }

    /// Sets the child context for spawning and managing children.
    ///
    /// Once set, the Lua script can use:
    /// - `orcs.spawn_child(config)` - Spawn a child
    /// - `orcs.child_count()` - Get current child count
    /// - `orcs.max_children()` - Get max allowed children
    ///
    /// # Arguments
    ///
    /// * `ctx` - The child context
    pub fn set_child_context(&mut self, ctx: Box<dyn ChildContext>) {
        let ctx_arc = Arc::new(Mutex::new(ctx));
        self.child_context = Some(Arc::clone(&ctx_arc));

        // Register child context functions in Lua
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) = ctx_fns::register(&lua, ctx_arc, Arc::clone(&self.sandbox)) {
                tracing::warn!("Failed to register child context functions: {}", e);
            }
        }
    }
}

impl Component for LuaComponent {
    fn id(&self) -> &ComponentId {
        &self.id
    }

    fn subscriptions(&self) -> &[EventCategory] {
        &self.subscriptions
    }

    fn runtime_hints(&self) -> RuntimeHints {
        self.hints.clone()
    }

    fn status(&self) -> Status {
        self.status
    }

    #[tracing::instrument(
        skip(self, request),
        fields(component = %self.id.fqn(), operation = %request.operation)
    )]
    fn on_request(&mut self, request: &Request) -> Result<JsonValue, ComponentError> {
        if self.status == Status::Aborted {
            return Err(ComponentError::ExecutionFailed(
                "component is aborted".to_string(),
            ));
        }
        self.status = Status::Running;

        let lua = self.lua.lock().map_err(|e| {
            tracing::error!(error = %e, "Lua mutex poisoned");
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

    #[tracing::instrument(
        skip(self, signal),
        fields(component = %self.id.fqn(), signal_kind = ?signal.kind)
    )]
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

    #[tracing::instrument(skip(self), fields(component = %self.id.fqn()))]
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

    #[tracing::instrument(skip(self), fields(component = %self.id.fqn()))]
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

    fn set_emitter(&mut self, emitter: Box<dyn Emitter>) {
        let emitter_arc = Arc::new(Mutex::new(emitter));
        self.emitter = Some(Arc::clone(&emitter_arc));

        // Register emitter-backed Lua functions (orcs.output, orcs.emit_event)
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) = emitter_fns::register(&lua, emitter_arc) {
                tracing::warn!("Failed to register emitter functions: {}", e);
            }
        }
    }

    fn set_child_context(&mut self, ctx: Box<dyn ChildContext>) {
        let ctx_arc = Arc::new(Mutex::new(ctx));
        self.child_context = Some(Arc::clone(&ctx_arc));

        // Register child context functions in Lua
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) = ctx_fns::register(&lua, ctx_arc, Arc::clone(&self.sandbox)) {
                tracing::warn!("Failed to register child context functions: {}", e);
            }
        }
    }
}

/// ComponentLoader implementation for Lua components.
///
/// Allows creating LuaComponent instances from inline script content
/// for use with ChildContext::spawn_runner_from_script().
#[derive(Clone)]
pub struct LuaComponentLoader {
    sandbox: Arc<dyn SandboxPolicy>,
}

impl LuaComponentLoader {
    /// Creates a new LuaComponentLoader with the given sandbox policy.
    #[must_use]
    pub fn new(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self { sandbox }
    }
}

impl ComponentLoader for LuaComponentLoader {
    fn load_from_script(
        &self,
        script: &str,
        _id: Option<&str>,
    ) -> Result<Box<dyn Component>, SpawnError> {
        // Note: id parameter is ignored; LuaComponent extracts ID from script
        LuaComponent::from_script(script, Arc::clone(&self.sandbox))
            .map(|c| Box::new(c) as Box<dyn Component>)
            .map_err(|e| SpawnError::InvalidScript(e.to_string()))
    }
}

#[cfg(test)]
mod tests;

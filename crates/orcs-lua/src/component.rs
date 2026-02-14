//! LuaComponent implementation.
//!
//! Wraps a Lua script to implement the Component trait.

mod ctx_fns;
mod emitter_fns;

/// Truncate a string to at most `max_bytes`, respecting UTF-8 char boundaries.
fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

use crate::error::LuaError;
use crate::lua_env::LuaEnv;
use crate::types::{
    parse_event_category, parse_signal_response, LuaRequest, LuaResponse, LuaSignal,
};
use mlua::{Function, IntoLua, Lua, LuaSerdeExt, RegistryKey, Table, Value as LuaValue};
use orcs_component::{
    ChildContext, Component, ComponentError, ComponentLoader, ComponentSnapshot, Emitter,
    EventCategory, RuntimeHints, SnapshotError, SpawnError, Status, SubscriptionEntry,
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
    /// Subscribed event categories (for Component::subscriptions()).
    subscriptions: Vec<EventCategory>,
    /// Subscription entries with optional operation filters (for Component::subscription_entries()).
    subscription_entries: Vec<SubscriptionEntry>,
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
    /// Registry key for snapshot callback (optional).
    snapshot_key: Option<RegistryKey>,
    /// Registry key for restore callback (optional).
    restore_key: Option<RegistryKey>,
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
                tracing::warn!(
                    "[lua] orcs.output called without emitter (noop): {}",
                    truncate_utf8(&msg, 100)
                );
                Ok(())
            })?;
            orcs_table.set("output", output_noop)?;

            let output_level_noop = lua.create_function(|_, (msg, _level): (String, String)| {
                tracing::warn!(
                    "[lua] orcs.output_with_level called without emitter (noop): {}",
                    truncate_utf8(&msg, 100)
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

        // Extract subscriptions (supports both string and table entries)
        //
        // String form (all operations):
        //   subscriptions = { "Echo", "UserInput" }
        //
        // Table form (specific operations):
        //   subscriptions = {
        //       "UserInput",
        //       { category = "Extension", operations = {"route_response"} },
        //   }
        let subs_table: Table = component_table
            .get("subscriptions")
            .map_err(|_| LuaError::MissingCallback("subscriptions".to_string()))?;

        let mut subscriptions = Vec::new();
        let mut subscription_entries = Vec::new();
        for pair in subs_table.pairs::<i64, LuaValue>() {
            let (_, value) = pair.map_err(|e| LuaError::TypeError(e.to_string()))?;
            match &value {
                LuaValue::String(s) => {
                    let cat_str = s.to_str().map_err(|e| LuaError::TypeError(e.to_string()))?;
                    if let Some(cat) = parse_event_category(&cat_str) {
                        subscriptions.push(cat.clone());
                        subscription_entries.push(SubscriptionEntry::all(cat));
                    }
                }
                LuaValue::Table(tbl) => {
                    // Table form: { category = "Extension", operations = {"op1", "op2"} }
                    let cat_str: String = tbl.get("category").map_err(|e| {
                        LuaError::TypeError(format!(
                            "subscription table must have 'category' field: {e}"
                        ))
                    })?;
                    if let Some(cat) = parse_event_category(&cat_str) {
                        subscriptions.push(cat.clone());
                        // Parse optional operations list
                        let ops_table: Option<Table> = tbl.get("operations").ok();
                        if let Some(ops) = ops_table {
                            let mut op_names = Vec::new();
                            for (_, op) in ops.pairs::<i64, String>().flatten() {
                                op_names.push(op);
                            }
                            subscription_entries
                                .push(SubscriptionEntry::with_operations(cat, op_names));
                        } else {
                            subscription_entries.push(SubscriptionEntry::all(cat));
                        }
                    }
                }
                _ => {
                    tracing::warn!("subscription entry must be a string or table, ignoring");
                }
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

        let snapshot_key = component_table
            .get::<Function>("snapshot")
            .ok()
            .map(|f| lua.create_registry_value(f))
            .transpose()?;

        let restore_key = component_table
            .get::<Function>("restore")
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
            subscription_entries,
            status: Status::Idle,
            on_request_key,
            on_signal_key,
            init_key,
            shutdown_key,
            snapshot_key,
            restore_key,
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
        self.snapshot_key = new_component.snapshot_key;
        self.restore_key = new_component.restore_key;
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
        self.install_child_context(ctx);
    }

    /// Shared implementation for `set_child_context` (inherent + trait).
    ///
    /// Extracts hook registry, registers ctx functions, and wires up hooks.
    fn install_child_context(&mut self, ctx: Box<dyn ChildContext>) {
        let hook_registry = ctx
            .extension("hook_registry")
            .and_then(|any| any.downcast::<orcs_hook::SharedHookRegistry>().ok())
            .map(|boxed| *boxed);

        let ctx_arc = Arc::new(Mutex::new(ctx));
        self.child_context = Some(Arc::clone(&ctx_arc));

        let Ok(lua) = self.lua.lock() else {
            tracing::error!(component = %self.id.fqn(), "Lua mutex poisoned in install_child_context");
            return;
        };

        if let Err(e) = ctx_fns::register(&lua, ctx_arc, Arc::clone(&self.sandbox)) {
            tracing::warn!("Failed to register child context functions: {}", e);
        }

        let Some(registry) = hook_registry else {
            return;
        };

        if let Err(e) =
            crate::hook_helpers::register_hook_function(&lua, registry.clone(), self.id.clone())
        {
            tracing::warn!("Failed to register orcs.hook(): {}", e);
        } else {
            tracing::debug!(component = %self.id.fqn(), "orcs.hook() registered");
        }

        if let Err(e) = crate::hook_helpers::register_unhook_function(&lua, registry.clone()) {
            tracing::warn!("Failed to register orcs.unhook(): {}", e);
        }

        lua.set_app_data(crate::tools::ToolHookContext {
            registry,
            component_id: self.id.clone(),
        });
        if let Err(e) = crate::tools::wrap_tools_with_hooks(&lua) {
            tracing::warn!("Failed to wrap tools with hooks: {}", e);
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

    fn subscription_entries(&self) -> Vec<SubscriptionEntry> {
        self.subscription_entries.clone()
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

    #[tracing::instrument(skip(self, config), fields(component = %self.id.fqn()))]
    fn init(&mut self, config: &serde_json::Value) -> Result<(), ComponentError> {
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

        // Convert JSON config to Lua value; pass nil if null or empty object
        let lua_config = if config.is_null()
            || (config.is_object() && config.as_object().is_none_or(serde_json::Map::is_empty))
        {
            mlua::Value::Nil
        } else {
            lua.to_value(config).map_err(|e| {
                tracing::debug!("Failed to convert config to Lua: {}", e);
                ComponentError::ExecutionFailed("config conversion failed".to_string())
            })?
        };

        init_fn.call::<()>(lua_config).map_err(|e| {
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

    fn snapshot(&self) -> Result<ComponentSnapshot, SnapshotError> {
        let Some(snapshot_key) = &self.snapshot_key else {
            return Err(SnapshotError::NotSupported(self.id.fqn()));
        };

        let lua = self
            .lua
            .lock()
            .map_err(|e| SnapshotError::InvalidData(format!("lua mutex poisoned: {e}")))?;

        let snapshot_fn: Function = lua
            .registry_value(snapshot_key)
            .map_err(|e| SnapshotError::InvalidData(format!("snapshot callback not found: {e}")))?;

        let lua_result: LuaValue = snapshot_fn
            .call(())
            .map_err(|e| SnapshotError::InvalidData(format!("snapshot callback failed: {e}")))?;

        let json_value = lua_value_to_json(&lua_result);
        ComponentSnapshot::from_state(self.id.fqn(), &json_value)
    }

    fn restore(&mut self, snapshot: &ComponentSnapshot) -> Result<(), SnapshotError> {
        let Some(restore_key) = &self.restore_key else {
            return Err(SnapshotError::NotSupported(self.id.fqn()));
        };

        snapshot.validate(&self.id.fqn())?;

        let lua = self
            .lua
            .lock()
            .map_err(|e| SnapshotError::InvalidData(format!("lua mutex poisoned: {e}")))?;

        let restore_fn: Function = lua
            .registry_value(restore_key)
            .map_err(|e| SnapshotError::InvalidData(format!("restore callback not found: {e}")))?;

        let lua_value = json_to_lua_value(&lua, &snapshot.state).map_err(|e| {
            SnapshotError::InvalidData(format!("failed to convert snapshot to lua: {e}"))
        })?;

        restore_fn
            .call::<()>(lua_value)
            .map_err(|e| SnapshotError::RestoreFailed {
                component: self.id.fqn(),
                reason: format!("restore callback failed: {e}"),
            })?;

        Ok(())
    }

    fn set_emitter(&mut self, emitter: Box<dyn Emitter>) {
        let emitter_arc = Arc::new(Mutex::new(emitter));
        self.emitter = Some(Arc::clone(&emitter_arc));

        // Register emitter-backed Lua functions (orcs.output, orcs.emit_event)
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) = emitter_fns::register(&lua, emitter_arc) {
                tracing::warn!("Failed to register emitter functions: {}", e);
            }
        } else {
            tracing::warn!(
                component = %self.id.fqn(),
                "set_emitter: Lua lock failed — noop orcs.output remains active"
            );
        }
    }

    fn set_child_context(&mut self, ctx: Box<dyn ChildContext>) {
        self.install_child_context(ctx);
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

// === JSON ↔ Lua conversion helpers for snapshot/restore ===

/// Converts a Lua value to a serde_json::Value.
fn lua_value_to_json(value: &LuaValue) -> JsonValue {
    match value {
        LuaValue::Nil => JsonValue::Null,
        LuaValue::Boolean(b) => JsonValue::Bool(*b),
        LuaValue::Integer(i) => JsonValue::Number((*i).into()),
        LuaValue::Number(n) => serde_json::Number::from_f64(*n)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null),
        LuaValue::String(s) => JsonValue::String(s.to_string_lossy().to_string()),
        LuaValue::Table(table) => {
            // Detect array vs object: check if sequential integer keys starting at 1
            let len = table.raw_len();
            let is_array = len > 0
                && table
                    .clone()
                    .pairs::<i64, LuaValue>()
                    .enumerate()
                    .all(|(idx, pair)| pair.map(|(k, _)| k == (idx as i64 + 1)).unwrap_or(false));

            if is_array {
                let arr: Vec<JsonValue> = table
                    .clone()
                    .sequence_values::<LuaValue>()
                    .filter_map(|v| v.ok())
                    .map(|v| lua_value_to_json(&v))
                    .collect();
                JsonValue::Array(arr)
            } else {
                let mut map = serde_json::Map::new();
                if let Ok(pairs) = table
                    .clone()
                    .pairs::<LuaValue, LuaValue>()
                    .collect::<Result<Vec<_>, _>>()
                {
                    for (k, v) in pairs {
                        let key = match &k {
                            LuaValue::String(s) => s.to_string_lossy().to_string(),
                            LuaValue::Integer(i) => i.to_string(),
                            _ => continue,
                        };
                        map.insert(key, lua_value_to_json(&v));
                    }
                }
                JsonValue::Object(map)
            }
        }
        _ => JsonValue::Null,
    }
}

/// Converts a serde_json::Value to a Lua value.
fn json_to_lua_value(lua: &Lua, value: &JsonValue) -> Result<LuaValue, mlua::Error> {
    match value {
        JsonValue::Null => Ok(LuaValue::Nil),
        JsonValue::Bool(b) => Ok(LuaValue::Boolean(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(LuaValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(LuaValue::Number(f))
            } else {
                Ok(LuaValue::Nil)
            }
        }
        JsonValue::String(s) => s.as_str().into_lua(lua),
        JsonValue::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                let lua_val = json_to_lua_value(lua, v)?;
                table.raw_set(i + 1, lua_val)?;
            }
            Ok(LuaValue::Table(table))
        }
        JsonValue::Object(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                let lua_val = json_to_lua_value(lua, v)?;
                table.raw_set(k.as_str(), lua_val)?;
            }
            Ok(LuaValue::Table(table))
        }
    }
}

#[cfg(test)]
mod tests;

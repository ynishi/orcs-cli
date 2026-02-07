//! LuaComponent implementation.
//!
//! Wraps a Lua script to implement the Component trait.

use crate::error::LuaError;
use crate::types::{
    lua_to_json, parse_event_category, parse_signal_response, serde_json_to_lua, LuaRequest,
    LuaResponse, LuaSignal,
};
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table, Value as LuaValue};
use orcs_component::{
    ChildConfig, ChildContext, Component, ComponentError, ComponentLoader, Emitter, EventCategory,
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

        let mut component = Self::from_script(&script, sandbox)?;
        component.script_path = Some(path.display().to_string());
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
        let lua = Lua::new();

        // Register orcs helper functions
        Self::register_orcs_functions(&lua, Arc::clone(&sandbox))?;

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
            emitter: None,
            child_context: None,
            sandbox,
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
            Self::register_emitter_functions(&lua, Arc::clone(emitter))?;
        }

        // Re-register child context functions if child_context is set
        if let Some(ctx) = &self.child_context {
            let lua = self
                .lua
                .lock()
                .map_err(|e| LuaError::InvalidScript(format!("lua mutex poisoned: {}", e)))?;
            Self::register_child_context_functions(
                &lua,
                Arc::clone(ctx),
                Arc::clone(&self.sandbox),
            )?;
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
            if let Err(e) =
                Self::register_child_context_functions(&lua, ctx_arc, Arc::clone(&self.sandbox))
            {
                tracing::warn!("Failed to register child context functions: {}", e);
            }
        }
    }

    /// Registers child context functions in Lua's orcs table.
    ///
    /// Overrides file tools with capability-checked versions and adds:
    /// - `orcs.read/write/grep/glob/mkdir/remove/mv` - Capability-gated file tools
    /// - `orcs.exec(cmd)` - Execute shell command (permission-checked, cwd = sandbox root)
    /// - `orcs.llm(prompt)` - Call LLM (capability-checked, cwd = sandbox root)
    /// - `orcs.check_command(cmd)` - Check command permission without executing
    /// - `orcs.grant_command(pattern)` - Grant a command pattern (after HIL approval)
    /// - `orcs.spawn_child(config)` - Spawn a child
    /// - `orcs.child_count()` - Get current child count
    /// - `orcs.max_children()` - Get max allowed children
    fn register_child_context_functions(
        lua: &Lua,
        ctx: Arc<Mutex<Box<dyn ChildContext>>>,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<(), LuaError> {
        let orcs_table: Table = lua.globals().get("orcs")?;
        let sandbox_root = sandbox.root().to_path_buf();

        // ── Override file tools with capability-gated versions ──────────
        Self::register_capability_gated_tools(lua, &orcs_table, &ctx, &sandbox)?;

        // Override orcs.exec with permission-checked version
        // This replaces the basic exec from register_orcs_functions
        // Uses check_command_permission() which respects dynamic grants from HIL approval
        let ctx_clone = Arc::clone(&ctx);
        let exec_sandbox_root = sandbox_root.clone();
        let exec_fn = lua.create_function(move |lua, cmd: String| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            // Permission check via check_command_permission (respects dynamic grants)
            let permission = ctx_guard.check_command_permission(&cmd);
            match &permission {
                orcs_component::CommandPermission::Allowed => {
                    // Proceed to execution
                }
                orcs_component::CommandPermission::Denied(reason) => {
                    let result = lua.create_table()?;
                    result.set("ok", false)?;
                    result.set("stdout", "")?;
                    result.set("stderr", format!("permission denied: {}", reason))?;
                    result.set("code", -1)?;
                    return Ok(result);
                }
                orcs_component::CommandPermission::RequiresApproval { .. } => {
                    let result = lua.create_table()?;
                    result.set("ok", false)?;
                    result.set("stdout", "")?;
                    result.set(
                        "stderr",
                        "permission denied: command requires approval (use orcs.check_command first)",
                    )?;
                    result.set("code", -1)?;
                    return Ok(result);
                }
            }

            tracing::debug!("Lua exec (authorized): {}", cmd);

            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(&cmd)
                .current_dir(&exec_sandbox_root)
                .output()
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
            match output.status.code() {
                Some(code) => result.set("code", code)?,
                None => {
                    result.set("code", mlua::Value::Nil)?;
                    result.set("signal_terminated", true)?;
                }
            }

            Ok(result)
        })?;
        orcs_table.set("exec", exec_fn)?;

        // Override orcs.llm with capability-checked version
        // Requires Capability::LLM. Calls `claude -p` with sandbox root as cwd.
        {
            let ctx_clone = Arc::clone(&ctx);
            let llm_sandbox_root = sandbox_root.clone();
            let llm_fn = lua.create_function(move |lua, prompt: String| {
                let result = lua.create_table()?;

                // Capability check
                let ctx_guard = ctx_clone.lock().map_err(|e| {
                    mlua::Error::RuntimeError(format!("context lock failed: {}", e))
                })?;

                if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                    result.set("ok", false)?;
                    result.set("error", "permission denied: Capability::LLM not granted")?;
                    return Ok(result);
                }
                drop(ctx_guard);

                let output = std::process::Command::new("claude")
                    .arg("-p")
                    .arg(&prompt)
                    .current_dir(&llm_sandbox_root)
                    .output();

                match output {
                    Ok(out) if out.status.success() => {
                        let content = String::from_utf8_lossy(&out.stdout).to_string();
                        result.set("ok", true)?;
                        result.set("content", content)?;
                    }
                    Ok(out) => {
                        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                        result.set("ok", false)?;
                        result.set("error", if stderr.is_empty() { stdout } else { stderr })?;
                    }
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", format!("failed to spawn claude: {e}"))?;
                    }
                }

                Ok(result)
            })?;
            orcs_table.set("llm", llm_fn)?;
        }

        // orcs.spawn_child(config) -> { ok, id, handle, error }
        // config = { id = "child-id", script = "..." } or { id = "child-id", path = "..." }
        let ctx_clone = Arc::clone(&ctx);
        let spawn_child_fn = lua.create_function(move |lua, config: Table| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            // Permission check
            if !ctx_guard.can_spawn_child_auth() {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set(
                    "error",
                    "permission denied: spawn_child requires elevated session",
                )?;
                return Ok(result);
            }

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
            match ctx_guard.spawn_child(child_config) {
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

        // orcs.check_command(cmd) -> { status, reason?, grant_pattern?, description? }
        let ctx_clone = Arc::clone(&ctx);
        let check_command_fn = lua.create_function(move |lua, cmd: String| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            let permission = ctx_guard.check_command_permission(&cmd);
            let result = lua.create_table()?;
            result.set("status", permission.status_str())?;

            match &permission {
                orcs_component::CommandPermission::Denied(reason) => {
                    result.set("reason", reason.as_str())?;
                }
                orcs_component::CommandPermission::RequiresApproval {
                    grant_pattern,
                    description,
                } => {
                    result.set("grant_pattern", grant_pattern.as_str())?;
                    result.set("description", description.as_str())?;
                }
                orcs_component::CommandPermission::Allowed => {}
            }

            Ok(result)
        })?;
        orcs_table.set("check_command", check_command_fn)?;

        // orcs.grant_command(pattern) -> nil
        let ctx_clone = Arc::clone(&ctx);
        let grant_command_fn = lua.create_function(move |_, pattern: String| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            ctx_guard.grant_command(&pattern);
            tracing::info!("Lua grant_command: {}", pattern);
            Ok(())
        })?;
        orcs_table.set("grant_command", grant_command_fn)?;

        // orcs.request_approval(operation, description) -> approval_id
        let ctx_clone = Arc::clone(&ctx);
        let request_approval_fn =
            lua.create_function(move |_, (operation, description): (String, String)| {
                let ctx_guard = ctx_clone.lock().map_err(|e| {
                    mlua::Error::RuntimeError(format!("context lock failed: {}", e))
                })?;

                let approval_id = ctx_guard.emit_approval_request(&operation, &description);
                Ok(approval_id)
            })?;
        orcs_table.set("request_approval", request_approval_fn)?;

        // orcs.child_count() -> number
        let ctx_clone = Arc::clone(&ctx);
        let child_count_fn = lua.create_function(move |_, ()| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;
            Ok(ctx_guard.child_count())
        })?;
        orcs_table.set("child_count", child_count_fn)?;

        // orcs.max_children() -> number
        let ctx_clone = Arc::clone(&ctx);
        let max_children_fn = lua.create_function(move |_, ()| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;
            Ok(ctx_guard.max_children())
        })?;
        orcs_table.set("max_children", max_children_fn)?;

        // orcs.send_to_child(child_id, message) -> { ok, result, error }
        let ctx_clone = Arc::clone(&ctx);
        let send_to_child_fn =
            lua.create_function(move |lua, (child_id, message): (String, mlua::Value)| {
                let ctx_guard = ctx_clone.lock().map_err(|e| {
                    mlua::Error::RuntimeError(format!("context lock failed: {}", e))
                })?;

                // Convert Lua value to JSON
                let input = lua_to_json(message, lua)?;

                let result_table = lua.create_table()?;
                match ctx_guard.send_to_child(&child_id, input) {
                    Ok(child_result) => {
                        result_table.set("ok", true)?;
                        // Convert ChildResult to Lua
                        match child_result {
                            orcs_component::ChildResult::Ok(data) => {
                                // Convert JSON to Lua value safely (no eval)
                                let lua_data = serde_json_to_lua(&data, lua)?;
                                result_table.set("result", lua_data)?;
                            }
                            orcs_component::ChildResult::Err(e) => {
                                result_table.set("ok", false)?;
                                result_table.set("error", e.to_string())?;
                            }
                            orcs_component::ChildResult::Aborted => {
                                result_table.set("ok", false)?;
                                result_table.set("error", "child aborted")?;
                            }
                        }
                    }
                    Err(e) => {
                        result_table.set("ok", false)?;
                        result_table.set("error", e.to_string())?;
                    }
                }

                Ok(result_table)
            })?;
        orcs_table.set("send_to_child", send_to_child_fn)?;

        // orcs.spawn_runner(config) -> { ok, channel_id, error }
        // config = { script = "...", id = "optional-id" }
        // Spawns a Component as a separate ChannelRunner for parallel execution
        let ctx_clone = Arc::clone(&ctx);
        let spawn_runner_fn = lua.create_function(move |lua, config: Table| {
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            // Permission check
            if !ctx_guard.can_spawn_runner_auth() {
                let result_table = lua.create_table()?;
                result_table.set("ok", false)?;
                result_table.set(
                    "error",
                    "permission denied: spawn_runner requires elevated session",
                )?;
                return Ok(result_table);
            }

            // Parse config - script is required
            let script: String = config
                .get("script")
                .map_err(|_| mlua::Error::RuntimeError("config.script required".into()))?;

            // ID is optional
            let id: Option<String> = config.get("id").ok();

            let result_table = lua.create_table()?;
            match ctx_guard.spawn_runner_from_script(&script, id.as_deref()) {
                Ok(channel_id) => {
                    result_table.set("ok", true)?;
                    result_table.set("channel_id", channel_id.to_string())?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e.to_string())?;
                }
            }

            Ok(result_table)
        })?;
        orcs_table.set("spawn_runner", spawn_runner_fn)?;

        tracing::debug!(
            "Registered orcs.spawn_child, child_count, max_children, send_to_child, spawn_runner functions"
        );
        Ok(())
    }

    /// Overrides file tools with capability-gated versions.
    ///
    /// Capability mapping:
    /// - `READ`   → `orcs.read`, `orcs.grep`, `orcs.glob`
    /// - `WRITE`  → `orcs.write`, `orcs.mkdir`
    /// - `DELETE` → `orcs.remove`, `orcs.mv`
    fn register_capability_gated_tools(
        lua: &Lua,
        orcs_table: &Table,
        ctx: &Arc<Mutex<Box<dyn ChildContext>>>,
        sandbox: &Arc<dyn SandboxPolicy>,
    ) -> Result<(), LuaError> {
        use crate::tools::{
            tool_glob, tool_grep, tool_mkdir, tool_mv, tool_read, tool_remove, tool_write,
        };
        use orcs_component::Capability;

        // Helper: deny result for missing capability
        fn cap_denied(lua: &Lua, cap_name: &str) -> mlua::Result<Table> {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set(
                "error",
                format!("permission denied: Capability::{cap_name} not granted"),
            )?;
            Ok(result)
        }

        // orcs.read — requires READ
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, path: String| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::READ) {
                    return cap_denied(lua, "READ");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_read(&path, sb.as_ref()) {
                    Ok((content, size)) => {
                        result.set("ok", true)?;
                        result.set("content", content)?;
                        result.set("size", size)?;
                    }
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("read", f)?;
        }

        // orcs.write — requires WRITE
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, (path, content): (String, String)| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::WRITE) {
                    return cap_denied(lua, "WRITE");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_write(&path, &content, sb.as_ref()) {
                    Ok(bytes) => {
                        result.set("ok", true)?;
                        result.set("bytes_written", bytes)?;
                    }
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("write", f)?;
        }

        // orcs.grep — requires READ
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, (pattern, path): (String, String)| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::READ) {
                    return cap_denied(lua, "READ");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_grep(&pattern, &path, sb.as_ref()) {
                    Ok(grep_matches) => {
                        let matches_table = lua.create_table()?;
                        for (i, m) in grep_matches.iter().enumerate() {
                            let entry = lua.create_table()?;
                            entry.set("line_number", m.line_number)?;
                            entry.set("line", m.line.as_str())?;
                            matches_table.set(i + 1, entry)?;
                        }
                        result.set("ok", true)?;
                        result.set("matches", matches_table)?;
                        result.set("count", grep_matches.len())?;
                    }
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("grep", f)?;
        }

        // orcs.glob — requires READ
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, (pattern, dir): (String, Option<String>)| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::READ) {
                    return cap_denied(lua, "READ");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_glob(&pattern, dir.as_deref(), sb.as_ref()) {
                    Ok(files) => {
                        let files_table = lua.create_table()?;
                        for (i, f) in files.iter().enumerate() {
                            files_table.set(i + 1, f.as_str())?;
                        }
                        result.set("ok", true)?;
                        result.set("files", files_table)?;
                        result.set("count", files.len())?;
                    }
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("glob", f)?;
        }

        // orcs.mkdir — requires WRITE
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, path: String| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::WRITE) {
                    return cap_denied(lua, "WRITE");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_mkdir(&path, sb.as_ref()) {
                    Ok(()) => result.set("ok", true)?,
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("mkdir", f)?;
        }

        // orcs.remove — requires DELETE
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, path: String| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                if !g.has_capability(Capability::DELETE) {
                    return cap_denied(lua, "DELETE");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_remove(&path, sb.as_ref()) {
                    Ok(()) => result.set("ok", true)?,
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("remove", f)?;
        }

        // orcs.mv — requires DELETE (source removed) + WRITE (dest created)
        {
            let ctx_c = Arc::clone(ctx);
            let sb = Arc::clone(sandbox);
            let f = lua.create_function(move |lua, (src, dst): (String, String)| {
                let g = ctx_c
                    .lock()
                    .map_err(|e| mlua::Error::RuntimeError(format!("context lock: {e}")))?;
                let needs = Capability::DELETE | Capability::WRITE;
                if !g.capabilities().contains(needs) {
                    return cap_denied(lua, "DELETE | WRITE");
                }
                drop(g);
                let result = lua.create_table()?;
                match tool_mv(&src, &dst, sb.as_ref()) {
                    Ok(()) => result.set("ok", true)?,
                    Err(e) => {
                        result.set("ok", false)?;
                        result.set("error", e)?;
                    }
                }
                Ok(result)
            })?;
            orcs_table.set("mv", f)?;
        }

        tracing::debug!("Overrode file tools with capability-gated versions");
        Ok(())
    }

    /// Registers orcs helper functions in Lua.
    ///
    /// Available functions:
    /// - `orcs.exec(cmd)` - Execute shell command and return output
    /// - `orcs.log(level, msg)` - Log a message at specified level
    ///
    /// Note: `orcs.output(msg)` is registered separately via `set_emitter()`.
    fn register_orcs_functions(lua: &Lua, sandbox: Arc<dyn SandboxPolicy>) -> Result<(), LuaError> {
        let orcs_table = lua.create_table()?;

        // orcs.exec(cmd) -> {ok=false, ...}
        // Deny by default: exec requires ChildContext with permission checking.
        // Overridden by permission-checked version in register_child_context_functions.
        let exec_fn = lua.create_function(|lua, _cmd: String| {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("stdout", "")?;
            result.set(
                "stderr",
                "exec denied: no execution context (ChildContext required)",
            )?;
            result.set("code", -1)?;
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

        // orcs.output - placeholder (registered with emitter later)
        // This allows scripts to check for the function even without emitter
        let output_noop_fn = lua.create_function(|_, msg: String| {
            tracing::debug!("[lua] orcs.output called without emitter: {}", msg);
            Ok(())
        })?;
        orcs_table.set("output", output_noop_fn)?;

        // orcs.output_with_level - placeholder
        let output_level_noop_fn = lua.create_function(|_, (msg, _level): (String, String)| {
            tracing::debug!(
                "[lua] orcs.output_with_level called without emitter: {}",
                msg
            );
            Ok(())
        })?;
        orcs_table.set("output_with_level", output_level_noop_fn)?;

        // orcs.pwd - sandbox root as string
        orcs_table.set("pwd", sandbox.root().display().to_string())?;

        lua.globals().set("orcs", orcs_table)?;

        // Register native Rust tools (read, write, grep, glob)
        crate::tools::register_tool_functions(lua, sandbox)?;

        // Disable dangerous Lua stdlib functions (io.*, os.execute, loadfile, dofile)
        crate::orcs_helpers::sandbox_lua_globals(lua)?;

        Ok(())
    }

    /// Registers emitter-backed Lua functions (orcs.output, orcs.emit_event).
    ///
    /// Called when `set_emitter()` is invoked to enable event emission.
    fn register_emitter_functions(
        lua: &Lua,
        emitter: Arc<Mutex<Box<dyn Emitter>>>,
    ) -> Result<(), LuaError> {
        let orcs_table: Table = lua.globals().get("orcs")?;

        // orcs.output(msg) - emit output event via emitter
        let emitter_clone = Arc::clone(&emitter);
        let output_fn = lua.create_function(move |_, msg: String| {
            if let Ok(em) = emitter_clone.lock() {
                em.emit_output(&msg);
            }
            Ok(())
        })?;
        orcs_table.set("output", output_fn)?;

        // orcs.output_with_level(msg, level) - emit output with level
        let emitter_clone2 = Arc::clone(&emitter);
        let output_level_fn = lua.create_function(move |_, (msg, level): (String, String)| {
            if let Ok(em) = emitter_clone2.lock() {
                em.emit_output_with_level(&msg, &level);
            }
            Ok(())
        })?;
        orcs_table.set("output_with_level", output_level_fn)?;

        // orcs.emit_event(category, operation, payload) - broadcast Extension event
        let emitter_clone3 = Arc::clone(&emitter);
        let emit_event_fn =
            lua.create_function(move |lua, (category, operation, payload): (String, String, LuaValue)| {
                let json_payload: serde_json::Value = lua.from_value(payload)?;
                if let Ok(em) = emitter_clone3.lock() {
                    em.emit_event(&category, &operation, json_payload);
                }
                Ok(())
            })?;
        orcs_table.set("emit_event", emit_event_fn)?;

        tracing::debug!("Registered orcs.output and orcs.emit_event functions with emitter");
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

    fn set_emitter(&mut self, emitter: Box<dyn Emitter>) {
        let emitter_arc = Arc::new(Mutex::new(emitter));
        self.emitter = Some(Arc::clone(&emitter_arc));

        // Register emitter-backed Lua functions (orcs.output, orcs.emit_event)
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) = Self::register_emitter_functions(&lua, emitter_arc) {
                tracing::warn!("Failed to register emitter functions: {}", e);
            }
        }
    }

    fn set_child_context(&mut self, ctx: Box<dyn ChildContext>) {
        let ctx_arc = Arc::new(Mutex::new(ctx));
        self.child_context = Some(Arc::clone(&ctx_arc));

        // Register child context functions in Lua
        if let Ok(lua) = self.lua.lock() {
            if let Err(e) =
                Self::register_child_context_functions(&lua, ctx_arc, Arc::clone(&self.sandbox))
            {
                tracing::warn!("Failed to register child context functions: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_event::Request;
    use orcs_runtime::sandbox::ProjectSandbox;
    use orcs_types::{ChannelId, ComponentId, Principal};

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

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

        let component = LuaComponent::from_script(script, test_sandbox()).expect("load script");
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

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

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

        let mut component = LuaComponent::from_script(script, test_sandbox()).expect("load script");

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

        let result = LuaComponent::from_script(script, test_sandbox());
        assert!(result.is_err());
    }

    // --- orcs.exec tests ---

    mod exec_tests {
        use super::*;
        use orcs_component::{
            ChildConfig, ChildHandle, ChildResult, CommandPermission, SpawnError,
        };
        /// Minimal permissive ChildContext for testing exec with permission checking.
        #[derive(Debug)]
        struct PermissiveContext;

        #[derive(Debug)]
        struct StubHandle {
            id: String,
        }

        impl ChildHandle for StubHandle {
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

        impl ChildContext for PermissiveContext {
            fn parent_id(&self) -> &str {
                "test-parent"
            }
            fn emit_output(&self, _message: &str) {}
            fn emit_output_with_level(&self, _message: &str, _level: &str) {}
            fn spawn_child(&self, config: ChildConfig) -> Result<Box<dyn ChildHandle>, SpawnError> {
                Ok(Box::new(StubHandle { id: config.id }))
            }
            fn child_count(&self) -> usize {
                0
            }
            fn max_children(&self) -> usize {
                10
            }
            fn send_to_child(
                &self,
                _child_id: &str,
                _input: serde_json::Value,
            ) -> Result<ChildResult, orcs_component::RunError> {
                Ok(ChildResult::Ok(serde_json::Value::Null))
            }
            fn check_command_permission(&self, _cmd: &str) -> CommandPermission {
                CommandPermission::Allowed
            }
            fn clone_box(&self) -> Box<dyn ChildContext> {
                Box::new(PermissiveContext)
            }
        }

        /// Helper to create a component that uses orcs.exec
        fn create_exec_component(cmd: &str) -> LuaComponent {
            let script = format!(
                r#"
                return {{
                    id = "exec-test",
                    subscriptions = {{"Echo"}},
                    on_request = function(req)
                        local result = orcs.exec("{}")
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
            let mut comp =
                LuaComponent::from_script(&script, test_sandbox()).expect("load exec script");
            comp.set_child_context(Box::new(PermissiveContext));
            comp
        }

        #[test]
        fn exec_denied_without_context() {
            // Without ChildContext, exec should deny
            let script = r#"
                return {
                    id = "exec-deny",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        local result = orcs.exec("echo hello")
                        return {
                            success = true,
                            data = {
                                ok = result.ok,
                                stderr = result.stderr,
                                code = result.code
                            }
                        }
                    end,
                    on_signal = function(sig) return "Ignored" end,
                }
            "#;

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");
            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req).expect("should succeed");

            assert!(!result["ok"].as_bool().unwrap_or(true));
            let stderr = result["stderr"].as_str().unwrap();
            assert!(
                stderr.contains("exec denied"),
                "expected 'exec denied', got: {stderr}"
            );
        }

        #[test]
        fn exec_echo_command_with_context() {
            let mut component = create_exec_component("echo 'hello world'");

            let req = create_test_request("test", JsonValue::Null);
            let data = component.on_request(&req).expect("should succeed");

            assert!(data["ok"].as_bool().unwrap_or(false));
            assert!(data["stdout"].as_str().unwrap().contains("hello world"));
            assert_eq!(data["code"].as_i64(), Some(0));
        }

        #[test]
        fn exec_failing_command() {
            let mut component = create_exec_component("exit 42");

            let req = create_test_request("test", JsonValue::Null);
            let data = component.on_request(&req).expect("should succeed");

            assert!(!data["ok"].as_bool().unwrap_or(true));
            assert_eq!(data["code"].as_i64(), Some(42));
        }

        #[test]
        fn exec_stderr_captured() {
            let mut component = create_exec_component("echo 'error output' >&2");

            let req = create_test_request("test", JsonValue::Null);
            let data = component.on_request(&req).expect("should succeed");

            assert!(data["stderr"].as_str().unwrap().contains("error output"));
        }

        #[tokio::test]
        async fn exec_in_async_context() {
            let mut component = create_exec_component("echo 'async test'");

            let req = create_test_request("test", JsonValue::Null);

            let result = tokio::task::spawn_blocking(move || component.on_request(&req))
                .await
                .expect("spawn_blocking should succeed");

            let data = result.expect("should succeed");
            assert!(data["ok"].as_bool().unwrap_or(false));
            assert!(data["stdout"].as_str().unwrap().contains("async test"));
        }

        #[test]
        fn exec_with_special_characters() {
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

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");
            component.set_child_context(Box::new(PermissiveContext));
            let req = create_test_request("test", JsonValue::Null);
            let data = component.on_request(&req).expect("should succeed");

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

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");
            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            // Should complete without error (log output goes to tracing)
            assert!(result.is_ok());
        }
    }

    // --- orcs.output / Emitter tests ---

    mod emitter_tests {
        use super::*;
        use orcs_component::Emitter;
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Mock emitter that records calls for testing.
        #[derive(Debug, Clone)]
        struct MockEmitter {
            call_count: Arc<AtomicUsize>,
        }

        impl MockEmitter {
            fn new() -> Self {
                Self {
                    call_count: Arc::new(AtomicUsize::new(0)),
                }
            }

            fn call_count(&self) -> usize {
                self.call_count.load(Ordering::SeqCst)
            }
        }

        impl Emitter for MockEmitter {
            fn emit_output(&self, _message: &str) {
                self.call_count.fetch_add(1, Ordering::SeqCst);
            }

            fn emit_output_with_level(&self, _message: &str, _level: &str) {
                self.call_count.fetch_add(1, Ordering::SeqCst);
            }

            fn clone_box(&self) -> Box<dyn Emitter> {
                Box::new(self.clone())
            }
        }

        #[test]
        fn has_emitter_returns_false_initially() {
            let component = LuaComponent::from_script(
                r#"
                return {
                    id = "test",
                    subscriptions = {"Echo"},
                    on_request = function(req) return { success = true } end,
                    on_signal = function(sig) return "Ignored" end,
                }
                "#,
                test_sandbox(),
            )
            .expect("load script");

            assert!(!component.has_emitter());
        }

        #[test]
        fn set_emitter_enables_has_emitter() {
            let mut component = LuaComponent::from_script(
                r#"
                return {
                    id = "test",
                    subscriptions = {"Echo"},
                    on_request = function(req) return { success = true } end,
                    on_signal = function(sig) return "Ignored" end,
                }
                "#,
                test_sandbox(),
            )
            .expect("load script");

            let emitter = MockEmitter::new();
            component.set_emitter(Box::new(emitter));

            assert!(component.has_emitter());
        }

        #[test]
        fn orcs_output_calls_emitter() {
            let script = r#"
                return {
                    id = "output-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.output("Hello from Lua!")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");

            let emitter = MockEmitter::new();
            let emitter_clone = emitter.clone();
            component.set_emitter(Box::new(emitter));

            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            assert!(
                emitter_clone.call_count() >= 1,
                "emitter should have been called"
            );
        }

        #[test]
        fn orcs_output_with_level_calls_emitter() {
            let script = r#"
                return {
                    id = "output-level-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        orcs.output_with_level("Warning message", "warn")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");

            let emitter = MockEmitter::new();
            let emitter_clone = emitter.clone();
            component.set_emitter(Box::new(emitter));

            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            assert!(result.is_ok());
            assert!(
                emitter_clone.call_count() >= 1,
                "emitter should have been called"
            );
        }

        #[test]
        fn orcs_output_without_emitter_is_noop() {
            let script = r#"
                return {
                    id = "output-noop-test",
                    subscriptions = {"Echo"},
                    on_request = function(req)
                        -- This should not panic even without emitter
                        orcs.output("Message without emitter")
                        return { success = true }
                    end,
                    on_signal = function(sig)
                        return "Ignored"
                    end,
                }
            "#;

            let mut component =
                LuaComponent::from_script(script, test_sandbox()).expect("load script");

            // Note: no emitter set
            let req = create_test_request("test", JsonValue::Null);
            let result = component.on_request(&req);

            // Should complete without error
            assert!(result.is_ok());
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

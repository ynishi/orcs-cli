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
use crate::types::serde_json_to_lua;
use crate::types::{parse_signal_response, parse_status, LuaResponse, LuaSignal};
use mlua::{Function, Lua, LuaSerdeExt, RegistryKey, Table};
use orcs_component::{
    Child, ChildConfig, ChildContext, ChildError, ChildResult, Identifiable, RunnableChild,
    SignalReceiver, Status, Statusable,
};
use orcs_event::{Signal, SignalResponse};
use orcs_runtime::sandbox::SandboxPolicy;
use parking_lot::Mutex;
use std::sync::Arc;

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
    /// Sandbox policy for file operations (needed for capability-gated tool registration).
    sandbox: Arc<dyn SandboxPolicy>,
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
//
// FOR AI: #[allow(unsafe_code)] MUST be placed on each `unsafe impl` individually.
// DO NOT use crate-root #![allow(unsafe_code)] — that disables the unsafe_code lint
// for the entire crate, silently allowing any future unsafe code to go undetected.
// mlua's `Lua` type is !Send + !Sync, so these manual Send/Sync impls are the ONLY
// places in this crate that require unsafe. Keep the allow scoped here exclusively.
#[allow(unsafe_code)]
unsafe impl Send for LuaChild {}
#[allow(unsafe_code)]
unsafe impl Sync for LuaChild {}

impl LuaChild {
    /// Creates a LuaChild from a Lua table.
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `table` - Lua table defining the child
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
    ///
    /// # Errors
    ///
    /// Returns error if table is missing required fields.
    pub fn from_table(
        lua: Arc<Mutex<Lua>>,
        table: Table,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let lua_guard = lua.lock();

        // Register base orcs functions only if not already set up.
        // If the parent VM was configured via LuaEnv, calling register_base_orcs_functions
        // again would destroy the custom require() via sandbox_lua_globals().
        if !is_orcs_initialized(&lua_guard) {
            register_base_orcs_functions(&lua_guard, Arc::clone(&sandbox))?;
        }

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
            sandbox,
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
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
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
    ///
    /// # Arguments
    ///
    /// * `lua` - Shared Lua runtime
    /// * `id` - Child identifier
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
    pub fn simple(
        lua: Arc<Mutex<Lua>>,
        id: impl Into<String>,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let id = id.into();
        let lua_guard = lua.lock();

        // Register base orcs functions only if not already set up.
        if !is_orcs_initialized(&lua_guard) {
            register_base_orcs_functions(&lua_guard, Arc::clone(&sandbox))?;
        }

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
            sandbox,
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
    /// * `sandbox` - Sandbox policy for file operations and exec cwd
    ///
    /// # Errors
    ///
    /// Returns error if script is invalid or missing required fields.
    pub fn from_script(
        lua: Arc<Mutex<Lua>>,
        script: &str,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<Self, LuaError> {
        let lua_guard = lua.lock();

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
        let lua = self.lua.lock();

        // Register context functions if context is available
        // This allows on_signal to use orcs.emit_output, etc.
        if let Some(ctx) = &self.context {
            if let Err(e) = register_context_functions(&lua, ctx.clone_box(), &self.sandbox) {
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
            let lua = self.lua.lock();
            // Register context functions if context is available
            // This allows abort() to use orcs.emit_output, etc.
            if let Some(ctx) = &self.context {
                if let Err(e) = register_context_functions(&lua, ctx.clone_box(), &self.sandbox) {
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

impl Statusable for LuaChild {
    fn status(&self) -> Status {
        self.status
    }
}

impl Child for LuaChild {
    fn set_context(&mut self, ctx: Box<dyn ChildContext>) {
        self.context = Some(ctx);
    }
}

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
        let lua = self.lua.lock();

        // Register context-dependent functions if context is available
        if let Some(ctx) = &self.context {
            if let Err(e) = register_context_functions(&lua, ctx.clone_box(), &self.sandbox) {
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

/// Checks if the Lua VM already has orcs functions registered.
///
/// Returns `true` if the `orcs` table exists and has `log` registered.
/// This prevents double-initialization which would destroy custom `require()`
/// set up by [`LuaEnv`](crate::LuaEnv).
fn is_orcs_initialized(lua: &Lua) -> bool {
    lua.globals()
        .get::<Table>("orcs")
        .and_then(|t| t.get::<Function>("log"))
        .is_ok()
}

/// Register context-dependent functions in Lua's orcs table.
///
/// This adds:
/// - Capability-gated file tools (read, write, grep, glob, mkdir, remove, mv)
/// - `orcs.exec(cmd)` - Permission-checked shell execution
/// - `orcs.exec_argv(program, args, opts)` - Permission-checked shell-free execution
/// - `orcs.spawn_child(config)` - Spawn a sub-child
/// - `orcs.emit_output(message, [level])` - Emit output to parent
/// - `orcs.child_count()` - Get current child count
/// - `orcs.max_children()` - Get max allowed children
fn register_context_functions(
    lua: &Lua,
    ctx: Box<dyn ChildContext>,
    sandbox: &Arc<dyn SandboxPolicy>,
) -> Result<(), mlua::Error> {
    use crate::context_wrapper::ContextWrapper;

    // Store context in app_data (shared ContextWrapper used by dispatch_rust_tool)
    lua.set_app_data(ContextWrapper(Arc::new(Mutex::new(ctx))));

    // Get or create orcs table
    let orcs_table: Table = match lua.globals().get("orcs") {
        Ok(t) => t,
        Err(_) => {
            let t = lua.create_table()?;
            lua.globals().set("orcs", t.clone())?;
            t
        }
    };

    // Note: capability checks for file tools are handled by dispatch_rust_tool
    // via ContextWrapper in app_data (set above or by caller).

    // orcs.exec(cmd) -> {ok, stdout, stderr, code}
    // Permission-checked override: replaces the deny-by-default from register_base_orcs_functions.
    let exec_fn = lua.create_function(|lua, cmd: String| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        // Capability gate: EXECUTE required
        if !ctx.has_capability(orcs_component::Capability::EXECUTE) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("stdout", "")?;
            result.set(
                "stderr",
                "permission denied: Capability::EXECUTE not granted",
            )?;
            result.set("code", -1)?;
            return Ok(result);
        }

        let permission = ctx.check_command_permission(&cmd);
        match &permission {
            orcs_component::CommandPermission::Allowed => {}
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

    // orcs.exec_argv(program, args [, opts]) -> {ok, stdout, stderr, code}
    // Shell-free execution: bypasses sh -c entirely.
    // Permission-checked via Capability::EXECUTE + check_command_permission(program).
    let sandbox_root = sandbox.root().to_path_buf();
    let exec_argv_fn = lua.create_function(
        move |lua, (program, args, opts): (String, Table, Option<Table>)| {
            let wrapper = lua
                .app_data_ref::<ContextWrapper>()
                .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

            let ctx = wrapper.0.lock();

            // Capability gate: EXECUTE required
            if !ctx.has_capability(orcs_component::Capability::EXECUTE) {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("stdout", "")?;
                result.set(
                    "stderr",
                    "permission denied: Capability::EXECUTE not granted",
                )?;
                result.set("code", -1)?;
                return Ok(result);
            }

            // Permission check on program name
            let permission = ctx.check_command_permission(&program);
            match &permission {
                orcs_component::CommandPermission::Allowed => {}
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
            drop(ctx);

            tracing::debug!("Lua exec_argv (authorized): {}", program);

            crate::sanitize::exec_argv_impl(
                lua,
                &program,
                &args,
                opts.as_ref(),
                &sandbox_root,
            )
        },
    )?;
    orcs_table.set("exec_argv", exec_argv_fn)?;

    // orcs.llm(prompt [, opts]) -> { ok, content?, model?, session_id?, error?, error_kind? }
    // Capability-checked override: delegates to llm_command::llm_request_impl.
    // Requires Capability::LLM.
    let llm_fn = lua.create_function(move |lua, args: (String, Option<Table>)| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        if !ctx.has_capability(orcs_component::Capability::LLM) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", "permission denied: Capability::LLM not granted")?;
            result.set("error_kind", "permission_denied")?;
            return Ok(result);
        }
        drop(ctx);

        crate::llm_command::llm_request_impl(lua, args)
    })?;
    orcs_table.set("llm", llm_fn)?;

    // orcs.http(method, url [, opts]) -> { ok, status?, headers?, body?, error?, error_kind? }
    // Capability-checked override: delegates to http_command::http_request_impl.
    // Requires Capability::HTTP.
    let http_fn = lua.create_function(|lua, args: (String, String, Option<Table>)| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        if !ctx.has_capability(orcs_component::Capability::HTTP) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", "permission denied: Capability::HTTP not granted")?;
            result.set("error_kind", "permission_denied")?;
            return Ok(result);
        }
        drop(ctx);

        crate::http_command::http_request_impl(lua, args)
    })?;
    orcs_table.set("http", http_fn)?;

    // orcs.spawn_child(config) -> { ok, id, error }
    // config = { id = "child-id", script = "..." } or { id = "child-id", path = "..." }
    let spawn_child_fn = lua.create_function(|lua, config: Table| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        // Capability gate: SPAWN required
        if !ctx.has_capability(orcs_component::Capability::SPAWN) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", "permission denied: Capability::SPAWN not granted")?;
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

            let ctx = wrapper.0.lock();

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

        let ctx = wrapper.0.lock();

        Ok(ctx.child_count())
    })?;
    orcs_table.set("child_count", child_count_fn)?;

    // orcs.max_children() -> number
    let max_children_fn = lua.create_function(|lua, ()| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        Ok(ctx.max_children())
    })?;
    orcs_table.set("max_children", max_children_fn)?;

    // orcs.check_command(cmd) -> { status, reason?, grant_pattern?, description? }
    let check_command_fn = lua.create_function(|lua, cmd: String| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        let permission = ctx.check_command_permission(&cmd);
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
    let grant_command_fn = lua.create_function(|lua, pattern: String| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        ctx.grant_command(&pattern);
        tracing::info!("Lua grant_command: {}", pattern);
        Ok(())
    })?;
    orcs_table.set("grant_command", grant_command_fn)?;

    // orcs.request_approval(operation, description) -> approval_id
    let request_approval_fn =
        lua.create_function(|lua, (operation, description): (String, String)| {
            let wrapper = lua
                .app_data_ref::<ContextWrapper>()
                .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

            let ctx = wrapper.0.lock();

            let approval_id = ctx.emit_approval_request(&operation, &description);
            Ok(approval_id)
        })?;
    orcs_table.set("request_approval", request_approval_fn)?;

    // orcs.request(target_fqn, operation, payload, opts?) -> { success, data?, error? }
    // Component-to-Component RPC from child context
    let request_fn =
        lua.create_function(
            |lua,
             (target, operation, payload, opts): (
                String,
                String,
                mlua::Value,
                Option<mlua::Table>,
            )| {
                let wrapper = lua
                    .app_data_ref::<ContextWrapper>()
                    .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

                let ctx = wrapper.0.lock();

                let json_payload: serde_json::Value = lua.from_value(payload)?;
                let timeout_ms = opts.and_then(|t| t.get::<u64>("timeout_ms").ok());

                let result = lua.create_table()?;
                match ctx.request(&target, &operation, json_payload, timeout_ms) {
                    Ok(value) => {
                        result.set("success", true)?;
                        let lua_data = lua.to_value(&value)?;
                        result.set("data", lua_data)?;
                    }
                    Err(err) => {
                        result.set("success", false)?;
                        result.set("error", err)?;
                    }
                }
                Ok(result)
            },
        )?;
    orcs_table.set("request", request_fn)?;

    // orcs.request_batch(requests) -> [ { success, data?, error? }, ... ]
    //
    // requests: Lua table (array) of { target, operation, payload, timeout_ms? }
    //
    // All RPC calls execute concurrently (tokio::spawn under the hood).
    // Results are returned in the same order as the input array.
    let request_batch_fn = lua.create_function(|lua, requests: mlua::Table| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        // Convert Lua array to Vec<(target, operation, payload, timeout_ms)>
        let len = requests.len()? as usize;
        let mut batch = Vec::with_capacity(len);
        for i in 1..=len {
            let entry: mlua::Table = requests.get(i)?;
            let target: String = entry.get("target").map_err(|_| {
                mlua::Error::RuntimeError(format!("requests[{}].target required", i))
            })?;
            let operation: String = entry.get("operation").map_err(|_| {
                mlua::Error::RuntimeError(format!("requests[{}].operation required", i))
            })?;
            let payload_val: mlua::Value = entry.get("payload").unwrap_or(mlua::Value::Nil);
            let json_payload = crate::types::lua_to_json(payload_val, lua)?;
            let timeout_ms: Option<u64> = entry.get("timeout_ms").ok();
            batch.push((target, operation, json_payload, timeout_ms));
        }

        // Execute batch (concurrent under the hood)
        let results = ctx.request_batch(batch);
        drop(ctx);

        // Convert results to Lua table array
        let results_table = lua.create_table()?;
        for (i, result) in results.into_iter().enumerate() {
            let entry = lua.create_table()?;
            match result {
                Ok(value) => {
                    entry.set("success", true)?;
                    let lua_data = lua.to_value(&value)?;
                    entry.set("data", lua_data)?;
                }
                Err(err) => {
                    entry.set("success", false)?;
                    entry.set("error", err)?;
                }
            }
            results_table.set(i + 1, entry)?; // Lua 1-indexed
        }

        Ok(results_table)
    })?;
    orcs_table.set("request_batch", request_batch_fn)?;

    // orcs.send_to_child(child_id, message) -> { ok, result?, error? }
    let send_to_child_fn =
        lua.create_function(|lua, (child_id, message): (String, mlua::Value)| {
            let wrapper = lua
                .app_data_ref::<ContextWrapper>()
                .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

            let ctx = wrapper.0.lock();

            let input = crate::types::lua_to_json(message, lua)?;

            let result_table = lua.create_table()?;
            match ctx.send_to_child(&child_id, input) {
                Ok(child_result) => {
                    result_table.set("ok", true)?;
                    match child_result {
                        orcs_component::ChildResult::Ok(data) => {
                            let lua_data = crate::types::serde_json_to_lua(&data, lua)?;
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

    // orcs.send_to_child_async(child_id, message) -> { ok, error? }
    //
    // Fire-and-forget: starts the child in a background thread and returns
    // immediately. The child's output flows through emit_output automatically.
    let send_to_child_async_fn =
        lua.create_function(|lua, (child_id, message): (String, mlua::Value)| {
            let wrapper = lua
                .app_data_ref::<ContextWrapper>()
                .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

            let ctx = wrapper.0.lock();

            let input = crate::types::lua_to_json(message, lua)?;

            let result_table = lua.create_table()?;
            match ctx.send_to_child_async(&child_id, input) {
                Ok(()) => {
                    result_table.set("ok", true)?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e.to_string())?;
                }
            }

            Ok(result_table)
        })?;
    orcs_table.set("send_to_child_async", send_to_child_async_fn)?;

    // orcs.send_to_children_batch(ids, inputs) -> [ { ok, result?, error? }, ... ]
    //
    // ids:    Lua table (array) of child ID strings
    // inputs: Lua table (array) of input values (same length as ids)
    //
    // Returns a Lua table (array) of result tables, one per child,
    // in the same order as the input arrays.
    let send_batch_fn = lua.create_function(|lua, (ids, inputs): (mlua::Table, mlua::Table)| {
        let wrapper = lua
            .app_data_ref::<ContextWrapper>()
            .ok_or_else(|| mlua::Error::RuntimeError("no context available".into()))?;

        let ctx = wrapper.0.lock();

        // Convert Lua arrays to Vec<(String, Value)>
        let mut requests = Vec::new();
        let ids_len = ids.len()? as usize;
        let inputs_len = inputs.len()? as usize;
        if ids_len != inputs_len {
            return Err(mlua::Error::RuntimeError(format!(
                "ids length ({}) != inputs length ({})",
                ids_len, inputs_len
            )));
        }

        for i in 1..=ids_len {
            let id: String = ids.get(i)?;
            let input_val: mlua::Value = inputs.get(i)?;
            let json_input = crate::types::lua_to_json(input_val, lua)?;
            requests.push((id, json_input));
        }

        // Execute batch (parallel under the hood)
        let results = ctx.send_to_children_batch(requests);
        drop(ctx);

        // Convert results to Lua table array
        let results_table = lua.create_table()?;
        for (i, (_id, result)) in results.into_iter().enumerate() {
            let entry = lua.create_table()?;
            match result {
                Ok(child_result) => {
                    entry.set("ok", true)?;
                    match child_result {
                        orcs_component::ChildResult::Ok(data) => {
                            let lua_data = crate::types::serde_json_to_lua(&data, lua)?;
                            entry.set("result", lua_data)?;
                        }
                        orcs_component::ChildResult::Err(e) => {
                            entry.set("ok", false)?;
                            entry.set("error", e.to_string())?;
                        }
                        orcs_component::ChildResult::Aborted => {
                            entry.set("ok", false)?;
                            entry.set("error", "child aborted")?;
                        }
                    }
                }
                Err(e) => {
                    entry.set("ok", false)?;
                    entry.set("error", e.to_string())?;
                }
            }
            results_table.set(i + 1, entry)?; // Lua 1-indexed
        }

        Ok(results_table)
    })?;
    orcs_table.set("send_to_children_batch", send_batch_fn)?;

    Ok(())
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
        let lua_guard = lua.lock();
        let table = lua_guard
            .create_table()
            .expect("should create lua table for from_table_runnable test");
        table
            .set("id", "test")
            .expect("should set id on test table");
        // Create a simple on_signal function
        let on_signal_fn = lua_guard
            .create_function(|_, _: mlua::Value| Ok("Ignored"))
            .expect("should create on_signal function");
        table
            .set("on_signal", on_signal_fn)
            .expect("should set on_signal on test table");
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
        use orcs_component::{Capability, ChildHandle, SpawnError};
        use std::sync::atomic::{AtomicUsize, Ordering};

        /// Mock ChildContext for testing.
        #[derive(Debug)]
        struct MockContext {
            parent_id: String,
            spawn_count: Arc<AtomicUsize>,
            emit_count: Arc<AtomicUsize>,
            max_children: usize,
            capabilities: Capability,
        }

        impl MockContext {
            fn new(parent_id: &str) -> Self {
                Self {
                    parent_id: parent_id.into(),
                    spawn_count: Arc::new(AtomicUsize::new(0)),
                    emit_count: Arc::new(AtomicUsize::new(0)),
                    max_children: 10,
                    capabilities: Capability::ALL,
                }
            }

            fn with_capabilities(mut self, caps: Capability) -> Self {
                self.capabilities = caps;
                self
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

            fn capabilities(&self) -> Capability {
                self.capabilities
            }

            fn clone_box(&self) -> Box<dyn ChildContext> {
                Box::new(Self {
                    parent_id: self.parent_id.clone(),
                    spawn_count: Arc::clone(&self.spawn_count),
                    emit_count: Arc::clone(&self.emit_count),
                    max_children: self.max_children,
                    capabilities: self.capabilities,
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
        fn check_command_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "checker",
                    run = function(input)
                        local check = orcs.check_command("ls -la")
                        return { success = true, data = { status = check.status } }
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
                // MockContext uses default impl → Allowed
                assert_eq!(data["status"], "allowed");
            }
        }

        #[test]
        fn grant_command_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "granter",
                    run = function(input)
                        -- Should not error (default impl is no-op)
                        orcs.grant_command("ls")
                        return { success = true }
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
        fn request_approval_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "approver",
                    run = function(input)
                        local id = orcs.request_approval("exec", "Run dangerous command")
                        return { success = true, data = { approval_id = id } }
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
                // Default impl returns empty string
                assert_eq!(data["approval_id"], "");
            }
        }

        #[test]
        fn send_to_child_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "sender",
                    run = function(input)
                        local result = orcs.send_to_child("worker-1", { message = "hello" })
                        return { success = result.ok, data = { result = result.result } }
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
                // MockContext returns {"mock": true}
                assert_eq!(data["result"]["mock"], true);
            }
        }

        #[test]
        fn send_to_children_batch_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "batch-sender",
                    run = function(input)
                        local ids = {"worker-1", "worker-1"}
                        local inputs = {
                            { message = "hello" },
                            { message = "world" },
                        }
                        local results = orcs.send_to_children_batch(ids, inputs)
                        if not results then
                            return { success = false, error = "nil results" }
                        end
                        return {
                            success = true,
                            data = {
                                count = #results,
                                first_ok = results[1] and results[1].ok,
                                second_ok = results[2] and results[2].ok,
                            },
                        }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create batch-sender");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "batch-sender should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["count"], 2, "should return 2 results");
                assert_eq!(data["first_ok"], true, "first result should be ok");
                assert_eq!(data["second_ok"], true, "second result should be ok");
            }
        }

        #[test]
        fn send_to_children_batch_length_mismatch_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "mismatch-sender",
                    run = function(input)
                        -- This should error: ids has 2 elements, inputs has 1
                        orcs.send_to_children_batch({"a", "b"}, {{x=1}})
                        return { success = true }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create mismatch-sender");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            // The length mismatch RuntimeError should propagate through Lua
            // and cause the run callback to fail.
            assert!(
                result.is_err(),
                "run should fail when batch ids/inputs length mismatch"
            );
            if let ChildResult::Err(err) = result {
                let msg = err.to_string();
                assert!(
                    msg.contains("length"),
                    "error should mention length mismatch, got: {}",
                    msg
                );
            }
        }

        #[test]
        fn request_batch_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "rpc-batcher",
                    run = function(input)
                        local results = orcs.request_batch({
                            { target = "comp-a", operation = "ping", payload = {} },
                            { target = "comp-b", operation = "ping", payload = { x = 1 } },
                        })
                        if not results then
                            return { success = false, error = "nil results" }
                        end
                        return {
                            success = true,
                            data = {
                                count = #results,
                                -- MockContext returns error (RPC not supported)
                                first_success = results[1] and results[1].success,
                                second_success = results[2] and results[2].success,
                                first_error = results[1] and results[1].error or "",
                            },
                        }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create rpc-batcher");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "rpc-batcher should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["count"], 2, "should return 2 results");
                // MockContext default: "request not supported by this context"
                assert_eq!(
                    data["first_success"], false,
                    "first should fail (no RPC in mock)"
                );
                assert_eq!(
                    data["second_success"], false,
                    "second should fail (no RPC in mock)"
                );
                let err = data["first_error"].as_str().unwrap_or("");
                assert!(
                    err.contains("not supported"),
                    "error should mention not supported, got: {}",
                    err
                );
            }
        }

        #[test]
        fn request_batch_empty_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "empty-batcher",
                    run = function(input)
                        local results = orcs.request_batch({})
                        return {
                            success = true,
                            data = { count = #results },
                        }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create empty-batcher");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "empty-batcher should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["count"], 0, "empty batch should return 0 results");
            }
        }

        #[test]
        fn request_batch_missing_target_errors_via_lua() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "bad-batcher",
                    run = function(input)
                        -- Missing 'target' field should error
                        orcs.request_batch({
                            { operation = "ping", payload = {} },
                        })
                        return { success = true }
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create bad-batcher");

            let ctx = MockContext::new("parent");
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_err(), "should fail when target is missing");
            if let ChildResult::Err(err) = result {
                let msg = err.to_string();
                assert!(
                    msg.contains("target"),
                    "error should mention target, got: {}",
                    msg
                );
            }
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

        // --- Capability gate tests ---

        #[test]
        fn exec_denied_without_execute_capability() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "exec-worker",
                    run = function(input)
                        local result = orcs.exec("echo hello")
                        return { success = true, data = {
                            ok = result.ok,
                            stderr = result.stderr or "",
                            code = result.code,
                        }}
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            // READ only — no EXECUTE
            let ctx = MockContext::new("parent").with_capabilities(Capability::READ);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "run itself should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["ok"], false, "exec should be denied");
                let stderr = data["stderr"].as_str().unwrap_or("");
                assert!(
                    stderr.contains("Capability::EXECUTE"),
                    "stderr should mention EXECUTE, got: {}",
                    stderr
                );
                assert_eq!(data["code"], -1);
            }
        }

        #[test]
        fn exec_allowed_with_execute_capability() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "exec-worker",
                    run = function(input)
                        local result = orcs.exec("echo cap-test-ok")
                        return { success = true, data = {
                            ok = result.ok,
                            stdout = result.stdout or "",
                        }}
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            // EXECUTE granted (MockContext has no auth → permissive)
            let ctx = MockContext::new("parent").with_capabilities(Capability::EXECUTE);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "run itself should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["ok"], true, "exec should be allowed");
                let stdout = data["stdout"].as_str().unwrap_or("");
                assert!(
                    stdout.contains("cap-test-ok"),
                    "stdout should contain output, got: {}",
                    stdout
                );
            }
        }

        #[test]
        fn spawn_child_denied_without_spawn_capability() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "spawner",
                    run = function(input)
                        local result = orcs.spawn_child({ id = "sub-child" })
                        return { success = true, data = {
                            ok = result.ok,
                            error = result.error or "",
                        }}
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            // READ | EXECUTE only — no SPAWN
            let ctx = MockContext::new("parent")
                .with_capabilities(Capability::READ | Capability::EXECUTE);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "run itself should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["ok"], false, "spawn should be denied");
                let error = data["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("Capability::SPAWN"),
                    "error should mention SPAWN, got: {}",
                    error
                );
            }
        }

        #[test]
        fn spawn_child_allowed_with_spawn_capability() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "spawner",
                    run = function(input)
                        local result = orcs.spawn_child({ id = "sub-child" })
                        return { success = true, data = {
                            ok = result.ok,
                            id = result.id or "",
                        }}
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            // SPAWN granted
            let ctx = MockContext::new("parent").with_capabilities(Capability::SPAWN);
            let spawn_count = Arc::clone(&ctx.spawn_count);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "run itself should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["ok"], true, "spawn should be allowed");
                assert_eq!(data["id"], "sub-child");
            }
            assert_eq!(
                spawn_count.load(Ordering::SeqCst),
                1,
                "spawn_child should have been called"
            );
        }

        #[test]
        fn llm_denied_without_llm_capability() {
            let lua = Arc::new(Mutex::new(Lua::new()));
            let script = r#"
                return {
                    id = "test-worker",
                    run = function(input)
                        local result = orcs.llm("hello")
                        return { success = true, data = {
                            ok = result.ok,
                            error = result.error or "",
                        }}
                    end,
                    on_signal = function(sig) return "Handled" end,
                }
            "#;

            let mut child =
                LuaChild::from_script(lua, script, test_sandbox()).expect("create child");

            // READ | EXECUTE — no LLM
            let ctx = MockContext::new("parent")
                .with_capabilities(Capability::READ | Capability::EXECUTE);
            child.set_context(Box::new(ctx));

            let result = child.run(serde_json::json!({}));
            assert!(result.is_ok(), "run itself should succeed");
            if let ChildResult::Ok(data) = result {
                assert_eq!(data["ok"], false, "llm should be denied");
                let error = data["error"].as_str().unwrap_or("");
                assert!(
                    error.contains("Capability::LLM"),
                    "error should mention LLM, got: {}",
                    error
                );
            }
        }
    }
}

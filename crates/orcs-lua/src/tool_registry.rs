//! Intent registry: structured intent definitions with unified dispatch.
//!
//! Provides a single source of truth for intent metadata (name, description,
//! parameters as JSON Schema, resolver) and a unified `orcs.dispatch(name, args)`
//! function that validates and routes to the appropriate resolver.
//!
//! # Design
//!
//! ```text
//! IntentRegistry (dynamic, stored in Lua app_data)
//!   ├── IntentDef { name, description, parameters (JSON Schema), resolver }
//!   │     resolver = Internal       → dispatch_internal() (8 builtin tools)
//!   │     resolver = Component{..}  → Component RPC via EventBus
//!   │
//!   └── Lua API:
//!         orcs.dispatch(name, args)   → resolve + execute
//!         orcs.tool_schemas()         → legacy Lua table format (backward compat)
//!         orcs.intent_defs()          → JSON Schema format (for LLM tools param)
//!         orcs.register_intent(def)   → dynamic addition (Component tools)
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::LuaError;
use crate::types::serde_json_to_lua;
use mlua::{Lua, Table};
use orcs_component::tool::RustTool;
use orcs_component::ComponentError;
use orcs_types::intent::{IntentDef, IntentResolver};

// ── IntentRegistry ───────────────────────────────────────────────────

/// Registry of named intents. Stored in Lua app_data.
///
/// Initialized with 8 builtin Internal tools. Components can register
/// additional intents at runtime via `orcs.register_intent()`.
///
/// Uses `Vec` for ordered iteration and `HashMap<String, usize>` index
/// for O(1) name lookup.
///
/// For `IntentResolver::Internal` tools, the registry also stores
/// `Arc<dyn RustTool>` instances for direct Rust dispatch (no Lua roundtrip).
pub struct IntentRegistry {
    defs: Vec<IntentDef>,
    /// Maps intent name → index in `defs` for O(1) lookup.
    index: HashMap<String, usize>,
    /// Maps intent name → RustTool for Internal dispatch.
    /// Not all Internal intents have a RustTool (e.g., `exec`
    /// retains its Lua dispatch path for per-command HIL approval).
    tools: HashMap<String, Arc<dyn RustTool>>,
}

impl Default for IntentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl IntentRegistry {
    /// Create a new registry pre-populated with the 8 builtin tools.
    pub fn new() -> Self {
        let builtin_tools = crate::builtin_tools::builtin_rust_tools();

        // Collect IntentDefs from RustTool instances (7 file tools)
        let mut defs: Vec<IntentDef> = builtin_tools.iter().map(|t| t.intent_def()).collect();
        let tools: HashMap<String, Arc<dyn RustTool>> = builtin_tools
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();

        // Add exec IntentDef (no RustTool — retains Lua dispatch for HIL)
        defs.push(exec_intent_def());

        let index = defs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name.clone(), i))
            .collect();

        Self { defs, index, tools }
    }

    /// Look up an intent definition by name. O(1).
    pub fn get(&self, name: &str) -> Option<&IntentDef> {
        self.index.get(name).map(|&i| &self.defs[i])
    }

    /// Look up a RustTool by name. O(1).
    pub fn get_tool(&self, name: &str) -> Option<&Arc<dyn RustTool>> {
        self.tools.get(name)
    }

    /// Register a new intent definition. Returns error if name is already taken.
    pub fn register(&mut self, def: IntentDef) -> Result<(), String> {
        if self.index.contains_key(&def.name) {
            return Err(format!("intent already registered: {}", def.name));
        }
        let idx = self.defs.len();
        self.index.insert(def.name.clone(), idx);
        self.defs.push(def);
        Ok(())
    }

    /// Register a RustTool (creates IntentDef automatically).
    /// Returns error if name is already taken.
    pub fn register_tool(&mut self, tool: Arc<dyn RustTool>) -> Result<(), String> {
        let def = tool.intent_def();
        let name = def.name.clone();
        self.register(def)?;
        self.tools.insert(name, tool);
        Ok(())
    }

    /// All registered intent definitions (insertion order).
    pub fn all(&self) -> &[IntentDef] {
        &self.defs
    }

    /// Number of registered intents.
    pub fn len(&self) -> usize {
        self.defs.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

// ── Exec IntentDef (not a RustTool — per-command HIL) ────────────────

/// The `exec` tool IntentDef.
///
/// `exec` is NOT a RustTool because its per-command approval flow
/// requires `ChildContext` / `ComponentError::Suspended`, which is
/// tightly coupled to the Lua runtime. It retains the Lua dispatch path.
fn exec_intent_def() -> IntentDef {
    IntentDef {
        name: "exec".into(),
        description: "Execute shell command. cwd = project root.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {
                    "type": "string",
                    "description": "Shell command to execute",
                }
            },
            "required": ["cmd"],
        }),
        resolver: IntentResolver::Internal,
    }
}

// ── Dispatch ─────────────────────────────────────────────────────────

/// Dispatches a tool call by name. Routes through IntentRegistry.
///
/// 1. Look up intent in registry
/// 2. Route by resolver: Internal → dispatch_internal, Component → (future)
/// 3. Unknown name → error
fn dispatch_tool(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
    let resolver = {
        let registry = ensure_registry(lua)?;
        match registry.get(name) {
            Some(def) => def.resolver.clone(),
            None => {
                let result = lua.create_table()?;
                set_error(&result, &format!("unknown intent: {name}"))?;
                return Ok(result);
            }
        }
    };

    let start = std::time::Instant::now();
    let result = match resolver {
        IntentResolver::Internal => dispatch_internal(lua, name, args),
        IntentResolver::Component {
            component_fqn,
            operation,
            timeout_ms,
        } => dispatch_component(lua, name, &component_fqn, &operation, args, timeout_ms),
        IntentResolver::Mcp {
            server_name,
            tool_name,
        } => dispatch_mcp(lua, name, &server_name, &tool_name, args),
    };
    let duration_ms = start.elapsed().as_millis() as u64;
    let ok = result
        .as_ref()
        .map(|t| t.get::<bool>("ok").unwrap_or(false))
        .unwrap_or(false);
    tracing::info!(
        "intent dispatch: {name} → {ok} ({duration_ms}ms)",
        ok = if ok { "ok" } else { "err" }
    );
    result
}

/// Checks if a mutating intent requires HIL approval before execution.
///
/// Uses [`ChildContext::is_command_granted`] with a synthetic `"intent:<name>"`
/// command. This intentionally bypasses session elevation so that mutating
/// intents always require explicit approval on first use:
///
/// - **Previously granted** → auto-allowed (`GrantPolicy` stores `"intent:<name>"`)
/// - **Not yet granted** → `Suspended` (triggers HIL approval flow)
///
/// After the user approves, `GrantPolicy::grant("intent:<name>")` is persisted by
/// the ChannelRunner, so subsequent calls for the same intent are auto-allowed.
///
/// # Caller responsibility
///
/// The caller (`dispatch_internal`) must filter out exempt intents (exec,
/// read-only tools) BEFORE calling this function. This avoids a redundant
/// registry lookup — the caller already holds the tool reference.
fn check_mutation_approval(lua: &Lua, name: &str, args: &Table) -> mlua::Result<()> {
    // Access ChildContext from app_data. If not set (e.g., tests without
    // Component context), fall through to permissive mode.
    let wrapper = match lua.app_data_ref::<crate::context_wrapper::ContextWrapper>() {
        Some(w) => w,
        None => return Ok(()),
    };
    let ctx = wrapper.0.lock();

    // Check grants directly — elevation is intentionally ignored so that
    // mutating intents always require explicit approval on first use.
    let intent_cmd = format!("intent:{name}");
    if ctx.is_command_granted(&intent_cmd) {
        return Ok(());
    }

    // Not yet granted → suspend for HIL approval
    let approval_id = format!("ap-{}", uuid::Uuid::new_v4());
    let detail = build_intent_description(name, args);

    tracing::info!(
        approval_id = %approval_id,
        intent = %name,
        grant_pattern = %intent_cmd,
        "intent requires approval, suspending"
    );

    Err(mlua::Error::ExternalError(std::sync::Arc::new(
        ComponentError::Suspended {
            approval_id,
            grant_pattern: intent_cmd.clone(),
            pending_request: serde_json::json!({
                "command": intent_cmd,
                "description": detail,
            }),
        },
    )))
}

/// Builds a human-readable description for an intent approval request.
fn build_intent_description(name: &str, args: &Table) -> String {
    let path_or = |key: &str| -> String {
        args.get::<String>(key)
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "<unknown>".to_string())
    };

    match name {
        "write" => format!("Write to file: {}", path_or("path")),
        "remove" => format!("Remove: {}", path_or("path")),
        "mv" => format!("Move: {} -> {}", path_or("src"), path_or("dst")),
        "mkdir" => format!("Create directory: {}", path_or("path")),
        _ => format!("Execute intent: {name}"),
    }
}

/// Dispatches an Internal intent.
///
/// For tools with a [`RustTool`] implementation (7 file tools), dispatches
/// directly in Rust — no Lua roundtrip. Falls back to Lua dispatch for
/// `exec` (per-command HIL approval) and any future Internal-only intents.
///
/// Mutating intents (write, remove, mv, mkdir) are subject to HIL approval
/// via [`check_mutation_approval`]. Read-only intents and `exec` (which has
/// its own per-command approval flow) skip this check.
fn dispatch_internal(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
    // Single registry lookup — used for both approval check and dispatch.
    let tool = {
        let registry = ensure_registry(lua)?;
        registry.get_tool(name).cloned()
    };

    // HIL approval for mutating intents (write, remove, mv, mkdir).
    // Exempt: exec (own per-command approval), read-only tools.
    if name != "exec" {
        let is_read_only = tool.as_ref().is_some_and(|t| t.is_read_only());
        if !is_read_only {
            check_mutation_approval(lua, name, args)?;
        }
    }

    if let Some(tool) = tool {
        return dispatch_rust_tool(lua, &*tool, args);
    }

    // Fallback to Lua dispatch (exec, future Internal-only intents).
    dispatch_internal_lua(lua, name, args)
}

/// Dispatches a RustTool: capability check → execute → convert to Lua table.
///
/// Used by `dispatch_internal` (for `orcs.dispatch` calls) and by
/// positional wrapper functions (`orcs.read(path)` etc.) in `tools.rs`.
///
/// # Lock scope
///
/// When a [`ContextWrapper`] is present in `app_data`, the inner
/// `Mutex<Box<dyn ChildContext>>` is held for the entire duration of
/// `tool.execute()`. This means:
///
/// - **Tool authors must NOT access `ContextWrapper` from within
///   `execute()`** — doing so would deadlock (`parking_lot::Mutex`
///   is non-reentrant).
/// - The `ToolContext` passed to `execute()` already provides
///   `child_ctx()` for any runtime interaction the tool may need.
/// - For the 7 builtin file tools (synchronous I/O), this lock
///   duration is negligible. Custom tools with long-running I/O
///   should consider whether holding the lock is acceptable.
pub(crate) fn dispatch_rust_tool(
    lua: &Lua,
    tool: &dyn RustTool,
    args: &Table,
) -> mlua::Result<Table> {
    use crate::context_wrapper::ContextWrapper;
    use orcs_component::tool::{ToolContext, ToolError};

    // Prepare sandbox and args before locking ContextWrapper.
    let sandbox: Arc<dyn orcs_runtime::sandbox::SandboxPolicy> = Arc::clone(
        &*lua
            .app_data_ref::<Arc<dyn orcs_runtime::sandbox::SandboxPolicy>>()
            .ok_or_else(|| mlua::Error::RuntimeError("sandbox not available".into()))?,
    );
    let json_args = crate::types::lua_to_json(mlua::Value::Table(args.clone()), lua)?;

    let cap = tool.required_capability();

    // Single lock scope: capability check + execute.
    // When ContextWrapper is absent (tests without Component context),
    // runs in permissive mode — no lock needed.
    let exec_result: Result<serde_json::Value, ToolError> =
        match lua.app_data_ref::<ContextWrapper>() {
            Some(wrapper) => {
                let guard = wrapper.0.lock();
                let child_ctx: &dyn orcs_component::ChildContext = &**guard;

                if !child_ctx.capabilities().contains(cap) {
                    Err(ToolError::new(format!(
                        "permission denied: {cap} not granted"
                    )))
                } else {
                    let ctx = ToolContext::new(&*sandbox).with_child_ctx(child_ctx);
                    tool.execute(json_args, &ctx)
                }
            }
            None => {
                let ctx = ToolContext::new(&*sandbox);
                tool.execute(json_args, &ctx)
            }
        };

    let result_table = lua.create_table()?;
    match exec_result {
        Ok(value) => {
            result_table.set("ok", true)?;
            // Merge result fields into the table.
            if let Some(obj) = value.as_object() {
                for (k, v) in obj {
                    let lua_val = crate::types::serde_json_to_lua(v, lua)?;
                    result_table.set(k.as_str(), lua_val)?;
                }
            }
        }
        Err(e) => {
            result_table.set("ok", false)?;
            result_table.set("error", e.message().to_string())?;
        }
    }

    Ok(result_table)
}

/// Lua-based dispatch fallback (exec and future Internal-only intents).
fn dispatch_internal_lua(lua: &Lua, name: &str, args: &Table) -> mlua::Result<Table> {
    let orcs: Table = lua.globals().get("orcs")?;

    match name {
        "exec" => {
            let cmd: String = get_required_arg(args, "cmd")?;
            let f: mlua::Function = orcs.get("exec")?;
            f.call(cmd)
        }
        _ => {
            // Internal resolver for unknown name — should not happen if
            // registry is consistent, but handle defensively.
            let result = lua.create_table()?;
            set_error(
                &result,
                &format!("internal dispatch error: no handler for '{name}'"),
            )?;
            Ok(result)
        }
    }
}

/// Dispatches a Component intent via RPC.
///
/// Calls `orcs.request(component_fqn, operation, args)` which is
/// registered by emitter_fns.rs (Component) or child.rs (ChildContext).
///
/// The RPC returns `{ success: bool, data?, error? }`. This function
/// normalizes the response to `{ ok: bool, data?, error?, duration_ms }`,
/// matching the Internal dispatch contract.
fn dispatch_component(
    lua: &Lua,
    intent_name: &str,
    component_fqn: &str,
    operation: &str,
    args: &Table,
    timeout_ms: Option<u64>,
) -> mlua::Result<Table> {
    let orcs: Table = lua.globals().get("orcs")?;

    let request_fn = match orcs.get::<mlua::Function>("request") {
        Ok(f) => f,
        Err(_) => {
            let result = lua.create_table()?;
            set_error(
                &result,
                "component dispatch unavailable: no execution context (orcs.request not registered)",
            )?;
            return Ok(result);
        }
    };

    // Build RPC payload (shallow copy of args table)
    let payload = lua.create_table()?;
    for pair in args.pairs::<mlua::Value, mlua::Value>() {
        let (k, v) = pair?;
        payload.set(k, v)?;
    }

    // Build optional opts table (timeout override for long-running RPCs)
    let opts = match timeout_ms {
        Some(ms) => {
            let t = lua.create_table()?;
            t.set("timeout_ms", ms)?;
            mlua::Value::Table(t)
        }
        None => mlua::Value::Nil,
    };

    // Execute with timing
    let start = std::time::Instant::now();
    let rpc_result: Table = request_fn.call((component_fqn, operation, payload, opts))?;
    let duration_ms = start.elapsed().as_millis() as u64;

    tracing::debug!(
        "component dispatch: {intent_name} → {component_fqn}::{operation} ({duration_ms}ms)"
    );

    // Normalize { success, data, error } → { ok, data, error, duration_ms }
    let result = lua.create_table()?;
    let success: bool = rpc_result.get("success").unwrap_or(false);
    result.set("ok", success)?;
    result.set("duration_ms", duration_ms)?;

    if success {
        // Forward data if present
        if let Ok(data) = rpc_result.get::<mlua::Value>("data") {
            result.set("data", data)?;
        }
    } else {
        // Forward error message
        let error_msg: String = rpc_result
            .get("error")
            .unwrap_or_else(|_| format!("component RPC failed: {component_fqn}::{operation}"));
        result.set("error", error_msg)?;
    }

    Ok(result)
}

/// Wrapper for `Arc<McpClientManager>` stored in Lua `app_data`.
///
/// Each Lua VM holds a clone of the shared manager. Set during
/// [`install_child_context`](crate::component) via the ChildContext
/// extension mechanism.
pub(crate) struct SharedMcpManager(pub Arc<orcs_mcp::McpClientManager>);

/// Deferred MCP IntentDefs, stored when IntentRegistry doesn't exist yet.
///
/// Consumed by [`ensure_registry`] on first registry access.
///
/// # Invariant
///
/// `ensure_registry` is called by every dispatch and query path
/// (`dispatch_tool`, `generate_descriptions`, `register_dispatch_functions`),
/// so pending defs are guaranteed to be drained before any tool operation.
/// If a new code path accesses `IntentRegistry` directly without calling
/// `ensure_registry`, these defs would be silently dropped.
pub(crate) struct PendingMcpDefs(pub Vec<orcs_types::intent::IntentDef>);

/// Dispatches an MCP tool invocation.
///
/// Bridges async `McpClientManager::call_tool` into the sync Lua call
/// context via `tokio::task::block_in_place`. Converts `CallToolResult`
/// content to a Lua table matching the dispatch contract:
/// `{ ok: bool, content?: string, error?: string }`.
///
/// # Runtime requirement
///
/// Requires a **multi-thread** tokio runtime (`block_in_place` panics on
/// `current_thread`). The caller must not hold any `RwLock` on
/// `McpClientManager` — `call_tool` acquires `tool_routes` and `servers`
/// read locks internally.
fn dispatch_mcp(
    lua: &Lua,
    intent_name: &str,
    server_name: &str,
    tool_name: &str,
    args: &Table,
) -> mlua::Result<Table> {
    // Retrieve McpClientManager from app_data
    let manager = match lua.app_data_ref::<SharedMcpManager>() {
        Some(m) => Arc::clone(&m.0),
        None => {
            let result = lua.create_table()?;
            set_error(
                &result,
                &format!("MCP client not initialized: {intent_name} (server={server_name})"),
            )?;
            return Ok(result);
        }
    };

    // Convert Lua args → JSON for MCP call
    let json_args = crate::types::lua_to_json(mlua::Value::Table(args.clone()), lua)?;

    // Bridge async → sync via tokio runtime handle
    let handle = tokio::runtime::Handle::try_current()
        .map_err(|_| mlua::Error::RuntimeError("no tokio runtime available for MCP call".into()))?;

    let namespaced = format!("mcp:{server_name}:{tool_name}");
    let call_result =
        tokio::task::block_in_place(|| handle.block_on(manager.call_tool(&namespaced, json_args)));

    // Convert CallToolResult → Lua table
    let result = lua.create_table()?;
    match call_result {
        Ok(tool_result) => {
            let is_error = tool_result.is_error.unwrap_or(false);
            result.set("ok", !is_error)?;

            // Extract text from Content items
            let text = orcs_mcp::content_to_text(&tool_result.content);
            if !text.is_empty() {
                if is_error {
                    result.set("error", text)?;
                } else {
                    result.set("content", text)?;
                }
            }
        }
        Err(e) => {
            set_error(&result, &format!("MCP call failed: {e}"))?;
        }
    }

    Ok(result)
}

// ── Registry Helpers ─────────────────────────────────────────────────

/// Ensure IntentRegistry exists in app_data. Returns a reference.
pub(crate) fn ensure_registry(lua: &Lua) -> mlua::Result<mlua::AppDataRef<'_, IntentRegistry>> {
    if lua.app_data_ref::<IntentRegistry>().is_none() {
        lua.set_app_data(IntentRegistry::new());
    }

    // Register any deferred MCP IntentDefs
    if let Some(pending) = lua.remove_app_data::<PendingMcpDefs>() {
        if let Some(mut registry) = lua.app_data_mut::<IntentRegistry>() {
            for def in pending.0 {
                if let Err(e) = registry.register(def) {
                    tracing::warn!(error = %e, "Failed to register deferred MCP intent");
                }
            }
        }
    }

    lua.app_data_ref::<IntentRegistry>().ok_or_else(|| {
        mlua::Error::RuntimeError("IntentRegistry not available after initialization".into())
    })
}

/// Generates formatted tool descriptions from the registry.
pub fn generate_descriptions(lua: &Lua) -> String {
    let registry = match ensure_registry(lua) {
        Ok(r) => r,
        Err(_) => return "IntentRegistry not available.\n".to_string(),
    };

    let mut out = String::from("Available tools:\n\n");

    for def in registry.all() {
        // Extract argument names from JSON Schema
        let args_fmt = extract_arg_names(&def.parameters);
        out.push_str(&format!(
            "{}({}) - {}\n",
            def.name, args_fmt, def.description
        ));
    }

    out.push_str("\norcs.pwd - Project root path (string).\n");
    out
}

/// Extract argument names from a JSON Schema `properties` + `required` for display.
fn extract_arg_names(schema: &serde_json::Value) -> String {
    let properties = match schema.get("properties").and_then(|p| p.as_object()) {
        Some(p) => p,
        None => return String::new(),
    };

    let required: Vec<&str> = schema
        .get("required")
        .and_then(|r| r.as_array())
        .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
        .unwrap_or_default();

    properties
        .keys()
        .map(|name| {
            if required.contains(&name.as_str()) {
                name.clone()
            } else {
                format!("{name}?")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

// ── Arg extraction helpers ───────────────────────────────────────────

/// Extracts a required string argument from the args table.
fn get_required_arg(args: &Table, name: &str) -> mlua::Result<String> {
    args.get::<String>(name)
        .map_err(|_| mlua::Error::RuntimeError(format!("missing required argument: {name}")))
}

/// Sets error fields on a result table.
fn set_error(result: &Table, msg: &str) -> mlua::Result<()> {
    result.set("ok", false)?;
    result.set("error", msg.to_string())?;
    Ok(())
}

// ── Lua API Registration ─────────────────────────────────────────────

/// Registers intent-based Lua APIs in the runtime.
///
/// - `orcs.dispatch(name, args)` — unified intent dispatcher
/// - `orcs.tool_schemas()` — legacy Lua table format (backward compat)
/// - `orcs.intent_defs()` — JSON Schema format (for LLM tools param)
/// - `orcs.register_intent(def)` — dynamic intent registration
/// - `orcs.tool_descriptions()` — formatted text descriptions
pub fn register_dispatch_functions(lua: &Lua) -> Result<(), LuaError> {
    // Ensure registry is initialized
    if lua.app_data_ref::<IntentRegistry>().is_none() {
        lua.set_app_data(IntentRegistry::new());
    }

    let orcs_table: Table = lua.globals().get("orcs")?;

    // orcs.dispatch(name, args) -> result table
    let dispatch_fn =
        lua.create_function(|lua, (name, args): (String, Table)| dispatch_tool(lua, &name, &args))?;
    orcs_table.set("dispatch", dispatch_fn)?;

    // orcs.tool_schemas() -> legacy Lua table format (backward compat)
    let schemas_fn = lua.create_function(|lua, ()| {
        let registry = ensure_registry(lua)?;
        let result = lua.create_table()?;

        for (i, def) in registry.all().iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", def.name.as_str())?;
            entry.set("description", def.description.as_str())?;

            // Convert JSON Schema properties to legacy args format
            let args_table = lua.create_table()?;
            if let Some(properties) = def.parameters.get("properties").and_then(|p| p.as_object()) {
                let required: Vec<&str> = def
                    .parameters
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                for (j, (prop_name, prop_schema)) in properties.iter().enumerate() {
                    let arg_entry = lua.create_table()?;
                    arg_entry.set("name", prop_name.as_str())?;

                    let is_required = required.contains(&prop_name.as_str());
                    let type_str = if is_required { "string" } else { "string?" };
                    arg_entry.set("type", type_str)?;
                    arg_entry.set("required", is_required)?;

                    let description = prop_schema
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("");
                    arg_entry.set("description", description)?;

                    args_table.set(j + 1, arg_entry)?;
                }
            }
            entry.set("args", args_table)?;
            result.set(i + 1, entry)?;
        }

        Ok(result)
    })?;
    orcs_table.set("tool_schemas", schemas_fn)?;

    // orcs.intent_defs() -> JSON Schema format for LLM tools parameter
    let intent_defs_fn = lua.create_function(|lua, ()| {
        let registry = ensure_registry(lua)?;
        let result = lua.create_table()?;

        for (i, def) in registry.all().iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", def.name.as_str())?;
            entry.set("description", def.description.as_str())?;

            // parameters as native Lua table (JSON Schema → Lua via serde_json_to_lua)
            let params_value = serde_json_to_lua(&def.parameters, lua)?;
            entry.set("parameters", params_value)?;

            result.set(i + 1, entry)?;
        }

        Ok(result)
    })?;
    orcs_table.set("intent_defs", intent_defs_fn)?;

    // orcs.register_intent(def) -> register a new intent definition
    let register_fn = lua.create_function(|lua, def_table: Table| {
        let name: String = def_table
            .get("name")
            .map_err(|_| mlua::Error::RuntimeError("register_intent: 'name' is required".into()))?;
        let description: String = def_table.get("description").map_err(|_| {
            mlua::Error::RuntimeError("register_intent: 'description' is required".into())
        })?;

        // Component resolver fields
        let component_fqn: String = def_table.get("component").map_err(|_| {
            mlua::Error::RuntimeError("register_intent: 'component' is required".into())
        })?;
        let operation: String = def_table
            .get("operation")
            .unwrap_or_else(|_| "execute".to_string());

        // Parameters: accept a table or default to empty schema
        let parameters = match def_table.get::<Table>("params") {
            Ok(params_table) => {
                // Convert Lua table to JSON Schema
                let mut properties = serde_json::Map::new();
                let mut required = Vec::new();

                for pair in params_table.pairs::<String, Table>() {
                    let (param_name, param_def) = pair?;
                    let type_str: String = param_def
                        .get("type")
                        .unwrap_or_else(|_| "string".to_string());
                    let desc: String = param_def
                        .get("description")
                        .unwrap_or_else(|_| String::new());
                    let is_required: bool = param_def.get("required").unwrap_or(false);

                    properties.insert(
                        param_name.clone(),
                        serde_json::json!({
                            "type": type_str,
                            "description": desc,
                        }),
                    );
                    if is_required {
                        required.push(serde_json::Value::String(param_name));
                    }
                }

                serde_json::json!({
                    "type": "object",
                    "properties": properties,
                    "required": required,
                })
            }
            Err(_) => serde_json::json!({"type": "object", "properties": {}}),
        };

        // Optional RPC timeout override (ms)
        let timeout_ms: Option<u64> = def_table.get("timeout_ms").ok();

        let intent_def = IntentDef {
            name: name.clone(),
            description,
            parameters,
            resolver: IntentResolver::Component {
                component_fqn,
                operation,
                timeout_ms,
            },
        };

        // Mutate registry
        if let Some(mut registry) = lua.remove_app_data::<IntentRegistry>() {
            let result = registry.register(intent_def);
            lua.set_app_data(registry);

            let result_table = lua.create_table()?;
            match result {
                Ok(()) => {
                    result_table.set("ok", true)?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e)?;
                }
            }
            Ok(result_table)
        } else {
            Err(mlua::Error::RuntimeError(
                "IntentRegistry not initialized".into(),
            ))
        }
    })?;
    orcs_table.set("register_intent", register_fn)?;

    // orcs.tool_descriptions() -> formatted text (dynamic: re-reads registry each call)
    let tool_desc_fn = lua.create_function(|lua, ()| Ok(generate_descriptions(lua)))?;
    orcs_table.set("tool_descriptions", tool_desc_fn)?;

    // ── MCP convenience APIs ────────────────────────────────────────

    // orcs.mcp_servers() -> { ok, servers: [{name}] }
    let mcp_servers_fn = lua.create_function(|lua, ()| {
        let result = lua.create_table()?;

        let manager = match lua.app_data_ref::<SharedMcpManager>() {
            Some(m) => Arc::clone(&m.0),
            None => {
                result.set("ok", true)?;
                result.set("servers", lua.create_table()?)?;
                return Ok(result);
            }
        };

        let handle = tokio::runtime::Handle::try_current().map_err(|_| {
            mlua::Error::RuntimeError("no tokio runtime available for mcp_servers".into())
        })?;

        let names = tokio::task::block_in_place(|| handle.block_on(manager.connected_servers()));

        let servers = lua.create_table()?;
        for (i, name) in names.iter().enumerate() {
            let entry = lua.create_table()?;
            entry.set("name", name.as_str())?;
            servers.set(i + 1, entry)?;
        }

        result.set("ok", true)?;
        result.set("servers", servers)?;
        Ok(result)
    })?;
    orcs_table.set("mcp_servers", mcp_servers_fn)?;

    // orcs.mcp_tools(server_name?) -> { ok, tools: [{name, description, server, parameters}] }
    let mcp_tools_fn = lua.create_function(|lua, server_filter: Option<String>| {
        let result = lua.create_table()?;

        let registry = match lua.app_data_ref::<IntentRegistry>() {
            Some(r) => r,
            None => {
                result.set("ok", true)?;
                result.set("tools", lua.create_table()?)?;
                return Ok(result);
            }
        };

        let tools = lua.create_table()?;
        let mut idx = 0usize;

        for def in registry.all() {
            if let IntentResolver::Mcp {
                ref server_name,
                ref tool_name,
            } = def.resolver
            {
                // Apply optional server name filter
                if let Some(ref filter) = server_filter {
                    if server_name != filter {
                        continue;
                    }
                }

                idx += 1;
                let entry = lua.create_table()?;
                entry.set("name", def.name.as_str())?;
                entry.set("description", def.description.as_str())?;
                entry.set("server", server_name.as_str())?;
                entry.set("tool", tool_name.as_str())?;

                let params_value = serde_json_to_lua(&def.parameters, lua)?;
                entry.set("parameters", params_value)?;

                tools.set(idx, entry)?;
            }
        }

        result.set("ok", true)?;
        result.set("tools", tools)?;
        Ok(result)
    })?;
    orcs_table.set("mcp_tools", mcp_tools_fn)?;

    // orcs.mcp_call(server, tool, args) -> { ok, content?, error? }
    let mcp_call_fn =
        lua.create_function(|lua, (server, tool, args): (String, String, Table)| {
            let namespaced = format!("mcp:{server}:{tool}");
            dispatch_mcp(lua, &namespaced, &server, &tool, &args)
        })?;
    orcs_table.set("mcp_call", mcp_call_fn)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::register_base_orcs_functions;
    use orcs_runtime::sandbox::{ProjectSandbox, SandboxPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn test_sandbox() -> (PathBuf, Arc<dyn SandboxPolicy>) {
        let dir = tempdir();
        let sandbox = ProjectSandbox::new(&dir).expect("test sandbox");
        (dir, Arc::new(sandbox))
    }

    fn tempdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orcs-registry-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after epoch")
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("should create temp dir");
        dir.canonicalize().expect("should canonicalize temp dir")
    }

    fn setup_lua(sandbox: Arc<dyn SandboxPolicy>) -> Lua {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, sandbox).expect("should register base functions");
        lua
    }

    // --- IntentRegistry unit tests ---

    #[test]
    fn registry_new_has_8_builtins() {
        let registry = IntentRegistry::new();
        assert_eq!(registry.len(), 8, "should have 8 builtin intents");
    }

    #[test]
    fn registry_get_existing() {
        let registry = IntentRegistry::new();
        let def = registry.get("read").expect("'read' should exist");
        assert_eq!(def.name, "read");
        assert_eq!(def.resolver, IntentResolver::Internal);
    }

    #[test]
    fn registry_get_nonexistent() {
        let registry = IntentRegistry::new();
        assert!(registry.get("nonexistent").is_none());
    }

    #[test]
    fn registry_register_new_intent() {
        let mut registry = IntentRegistry::new();
        let def = IntentDef {
            name: "custom_tool".into(),
            description: "A custom tool".into(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            resolver: IntentResolver::Component {
                component_fqn: "lua::my_comp".into(),
                operation: "execute".into(),
                timeout_ms: None,
            },
        };
        registry
            .register(def)
            .expect("should register successfully");
        assert_eq!(registry.len(), 9);
        assert!(registry.get("custom_tool").is_some());
    }

    #[test]
    fn registry_register_duplicate_fails() {
        let mut registry = IntentRegistry::new();
        let def = IntentDef {
            name: "read".into(),
            description: "duplicate".into(),
            parameters: serde_json::json!({}),
            resolver: IntentResolver::Internal,
        };
        let err = registry.register(def).expect_err("should reject duplicate");
        assert!(
            err.contains("already registered"),
            "error should mention duplicate, got: {err}"
        );
    }

    #[test]
    fn registry_all_intent_defs_have_json_schema() {
        let registry = IntentRegistry::new();
        for def in registry.all() {
            assert_eq!(
                def.parameters.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "intent '{}' should have JSON Schema with type=object",
                def.name
            );
            assert!(
                def.parameters.get("properties").is_some(),
                "intent '{}' should have properties",
                def.name
            );
        }
    }

    #[test]
    fn builtin_intent_names_match_expected() {
        let registry = IntentRegistry::new();
        let names: Vec<&str> = registry.all().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["read", "write", "grep", "glob", "mkdir", "remove", "mv", "exec"]
        );
    }

    // --- dispatch tests (unchanged behavior) ---

    #[test]
    fn dispatch_read() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("test.txt"), "hello dispatch").expect("should write test file");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("read", {{path="{}"}})"#,
                root.join("test.txt").display()
            ))
            .eval()
            .expect("dispatch read should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "hello dispatch"
        );
    }

    #[test]
    fn dispatch_write_and_read() {
        let (root, sandbox) = test_sandbox();
        let path = root.join("written.txt");

        let lua = setup_lua(sandbox);
        let code = format!(
            r#"
            local w = orcs.dispatch("write", {{path="{p}", content="via dispatch"}})
            local r = orcs.dispatch("read", {{path="{p}"}})
            return r
            "#,
            p = path.display()
        );
        let result: Table = lua
            .load(&code)
            .eval()
            .expect("dispatch write+read should succeed");
        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(
            result
                .get::<String>("content")
                .expect("should have content"),
            "via dispatch"
        );
    }

    #[test]
    fn dispatch_grep() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("search.txt"), "line one\nline two\nthird")
            .expect("should write search file");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("grep", {{pattern="line", path="{}"}})"#,
                root.join("search.txt").display()
            ))
            .eval()
            .expect("dispatch grep should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(result.get::<usize>("count").expect("should have count"), 2);
    }

    #[test]
    fn dispatch_glob() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("a.rs"), "").expect("write a.rs");
        fs::write(root.join("b.rs"), "").expect("write b.rs");
        fs::write(root.join("c.txt"), "").expect("write c.txt");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("glob", {{pattern="*.rs", dir="{}"}})"#,
                root.display()
            ))
            .eval()
            .expect("dispatch glob should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert_eq!(result.get::<usize>("count").expect("should have count"), 2);
    }

    #[test]
    fn dispatch_mkdir_remove() {
        let (root, sandbox) = test_sandbox();
        let dir_path = root.join("sub/deep");

        let lua = setup_lua(sandbox);
        let code = format!(
            r#"
            local m = orcs.dispatch("mkdir", {{path="{p}"}})
            local r = orcs.dispatch("remove", {{path="{p}"}})
            return {{mkdir=m, remove=r}}
            "#,
            p = dir_path.display()
        );
        let result: Table = lua
            .load(&code)
            .eval()
            .expect("dispatch mkdir+remove should succeed");
        let mkdir: Table = result.get("mkdir").expect("should have mkdir");
        let remove: Table = result.get("remove").expect("should have remove");
        assert!(mkdir.get::<bool>("ok").expect("mkdir ok"));
        assert!(remove.get::<bool>("ok").expect("remove ok"));
    }

    #[test]
    fn dispatch_mv() {
        let (root, sandbox) = test_sandbox();
        let src = root.join("src.txt");
        let dst = root.join("dst.txt");
        fs::write(&src, "move me").expect("write src");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("mv", {{src="{}", dst="{}"}})"#,
                src.display(),
                dst.display()
            ))
            .eval()
            .expect("dispatch mv should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok field"));
        assert!(dst.exists());
        assert!(!src.exists());
    }

    #[test]
    fn dispatch_unknown_tool() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(r#"return orcs.dispatch("nonexistent", {arg="val"})"#)
            .eval()
            .expect("dispatch unknown should return error table");

        assert!(!result.get::<bool>("ok").expect("should have ok field"));
        assert!(result
            .get::<String>("error")
            .expect("should have error")
            .contains("unknown intent"));
    }

    #[test]
    fn dispatch_missing_required_arg() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Missing required arg returns {ok: false, error: "..."} (not a Lua error).
        // Validation is handled by RustTool::execute, which returns ToolError
        // translated to a result table — consistent with other tool errors.
        let result: Table = lua
            .load(r#"return orcs.dispatch("read", {})"#)
            .eval()
            .expect("dispatch should return result table, not throw");

        let ok: bool = result.get("ok").expect("should have 'ok' field");
        assert!(!ok, "dispatch with missing arg should return ok=false");
        let err: String = result.get("error").expect("should have 'error' field");
        assert!(
            err.contains("missing required argument"),
            "error should mention missing arg, got: {err}"
        );
    }

    // --- tool_schemas tests (backward compat) ---

    #[test]
    fn tool_schemas_returns_all() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let schemas: Table = lua
            .load("return orcs.tool_schemas()")
            .eval()
            .expect("tool_schemas should return table");

        let count = schemas.len().expect("should have length") as usize;
        assert_eq!(count, 8, "should return 8 builtin tools");

        // Verify first schema structure (backward compat format)
        let first: Table = schemas.get(1).expect("should have first entry");
        assert_eq!(
            first.get::<String>("name").expect("should have name"),
            "read"
        );
        assert!(!first
            .get::<String>("description")
            .expect("should have description")
            .is_empty());

        let args: Table = first.get("args").expect("should have args");
        let first_arg: Table = args.get(1).expect("should have first arg");
        assert_eq!(first_arg.get::<String>("name").expect("arg name"), "path");
        assert_eq!(first_arg.get::<String>("type").expect("arg type"), "string");
        assert!(first_arg.get::<bool>("required").expect("arg required"));
    }

    // --- generate_descriptions tests ---

    #[test]
    fn descriptions_include_all_tools() {
        let lua = Lua::new();
        lua.set_app_data(IntentRegistry::new());
        let desc = generate_descriptions(&lua);
        let expected_tools = [
            "read", "write", "grep", "glob", "mkdir", "remove", "mv", "exec",
        ];
        for tool in expected_tools {
            assert!(desc.contains(tool), "missing tool in descriptions: {tool}");
        }
    }

    // --- exec dispatch delegates to registered function ---

    #[test]
    fn dispatch_exec_uses_registered_exec() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Default exec is deny-stub
        let result: Table = lua
            .load(r#"return orcs.dispatch("exec", {cmd="echo hi"})"#)
            .eval()
            .expect("dispatch exec should return table");

        // Should return the deny-stub result (not error, just ok=false)
        assert!(!result.get::<bool>("ok").expect("should have ok field"));
    }

    // --- intent_defs tests ---

    #[test]
    fn intent_defs_returns_all() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let defs: Table = lua
            .load("return orcs.intent_defs()")
            .eval()
            .expect("intent_defs should return table");

        let count = defs.len().expect("should have length") as usize;
        assert_eq!(count, 8, "should return 8 builtin intents");

        let first: Table = defs.get(1).expect("should have first entry");
        assert_eq!(
            first.get::<String>("name").expect("should have name"),
            "read"
        );
        assert!(!first
            .get::<String>("description")
            .expect("should have description")
            .is_empty());

        // parameters should be present (as string or table)
        assert!(
            first.get::<mlua::Value>("parameters").is_ok(),
            "should have parameters"
        );
    }

    // --- register_intent tests ---

    #[test]
    fn register_intent_adds_to_registry() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "custom_action",
                    description = "A custom action",
                    component = "lua::my_component",
                    operation = "do_stuff",
                    params = {
                        input = { type = "string", description = "Input data", required = true },
                    },
                })
                "#,
            )
            .eval()
            .expect("register_intent should return table");

        assert!(
            result.get::<bool>("ok").expect("should have ok field"),
            "registration should succeed"
        );

        // Verify it appears in tool_schemas
        let schemas: Table = lua
            .load("return orcs.tool_schemas()")
            .eval()
            .expect("tool_schemas after register");
        let count = schemas.len().expect("should have length") as usize;
        assert_eq!(count, 9, "should now have 9 intents (8 builtin + 1 custom)");
    }

    #[test]
    fn register_intent_duplicate_fails() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "read",
                    description = "duplicate",
                    component = "lua::x",
                })
                "#,
            )
            .eval()
            .expect("register_intent should return table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok field"),
            "duplicate registration should fail"
        );
        assert!(result
            .get::<String>("error")
            .expect("should have error")
            .contains("already registered"));
    }

    // --- Component dispatch tests ---

    #[test]
    fn dispatch_component_no_request_fn_returns_error() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register a Component intent without providing orcs.request
        lua.load(
            r#"
            orcs.register_intent({
                name = "comp_action",
                description = "component action",
                component = "lua::test_comp",
                operation = "do_stuff",
            })
            "#,
        )
        .exec()
        .expect("register should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("comp_action", {input="hello"})"#)
            .eval()
            .expect("should return error table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should fail without orcs.request"
        );
        let error: String = result.get("error").expect("should have error");
        assert!(
            error.contains("no execution context"),
            "error should mention missing context, got: {error}"
        );
    }

    #[test]
    fn dispatch_component_success_normalized() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register a Component intent
        lua.load(
            r#"
            orcs.register_intent({
                name = "mock_comp",
                description = "mock component",
                component = "lua::mock",
                operation = "echo",
            })
            "#,
        )
        .exec()
        .expect("register should succeed");

        // Mock orcs.request to return { success=true, data={echo="hi"} }
        lua.load(
            r#"
            orcs.request = function(target, operation, payload)
                return { success = true, data = { echo = payload.input, target = target, op = operation } }
            end
            "#,
        )
        .exec()
        .expect("mock should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("mock_comp", {input="hello"})"#)
            .eval()
            .expect("dispatch should return table");

        // Normalized response: ok (not success)
        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "should succeed"
        );

        // duration_ms should be present
        let duration: u64 = result.get("duration_ms").expect("should have duration_ms");
        assert!(
            duration < 1000,
            "local mock should be fast, got: {duration}ms"
        );

        // data should be forwarded
        let data: Table = result.get("data").expect("should have data");
        assert_eq!(
            data.get::<String>("echo").expect("should have echo"),
            "hello"
        );
        assert_eq!(
            data.get::<String>("target").expect("should have target"),
            "lua::mock"
        );
        assert_eq!(data.get::<String>("op").expect("should have op"), "echo");
    }

    #[test]
    fn dispatch_component_failure_normalized() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register + mock failing request
        lua.load(
            r#"
            orcs.register_intent({
                name = "fail_comp",
                description = "failing component",
                component = "lua::fail",
                operation = "explode",
            })
            orcs.request = function(target, operation, payload)
                return { success = false, error = "component exploded" }
            end
            "#,
        )
        .exec()
        .expect("setup should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("fail_comp", {})"#)
            .eval()
            .expect("dispatch should return table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "should report failure"
        );
        let error: String = result.get("error").expect("should have error");
        assert_eq!(error, "component exploded");

        // duration_ms still present
        assert!(result.get::<u64>("duration_ms").is_ok());
    }

    #[test]
    fn dispatch_component_forwards_all_args() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        lua.load(
            r#"
            orcs.register_intent({
                name = "args_comp",
                description = "args test",
                component = "lua::args_test",
                operation = "check_args",
            })
            -- Mock that captures and returns the payload
            orcs.request = function(target, operation, payload)
                return { success = true, data = payload }
            end
            "#,
        )
        .exec()
        .expect("setup should succeed");

        let result: Table = lua
            .load(r#"return orcs.dispatch("args_comp", {a="1", b="2", c="3"})"#)
            .eval()
            .expect("dispatch should return table");

        assert!(result.get::<bool>("ok").expect("should have ok"));
        let data: Table = result.get("data").expect("should have data");
        assert_eq!(data.get::<String>("a").expect("arg a"), "1");
        assert_eq!(data.get::<String>("b").expect("arg b"), "2");
        assert_eq!(data.get::<String>("c").expect("arg c"), "3");
    }

    // --- register_intent validation tests ---

    #[test]
    fn register_intent_missing_name_errors() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(
                r#"
                return orcs.register_intent({
                    description = "no name",
                    component = "lua::x",
                })
                "#,
            )
            .eval::<Table>();

        assert!(result.is_err(), "missing 'name' should cause a Lua error");
        let err = result.expect_err("should error").to_string();
        assert!(
            err.contains("name"),
            "error should mention 'name', got: {err}"
        );
    }

    #[test]
    fn register_intent_missing_description_errors() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "no_desc",
                    component = "lua::x",
                })
                "#,
            )
            .eval::<Table>();

        assert!(
            result.is_err(),
            "missing 'description' should cause a Lua error"
        );
        let err = result.expect_err("should error").to_string();
        assert!(
            err.contains("description"),
            "error should mention 'description', got: {err}"
        );
    }

    #[test]
    fn register_intent_missing_component_errors() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "no_comp",
                    description = "missing component",
                })
                "#,
            )
            .eval::<Table>();

        assert!(
            result.is_err(),
            "missing 'component' should cause a Lua error"
        );
        let err = result.expect_err("should error").to_string();
        assert!(
            err.contains("component"),
            "error should mention 'component', got: {err}"
        );
    }

    #[test]
    fn register_intent_defaults_operation_to_execute() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Register without specifying operation
        let result: Table = lua
            .load(
                r#"
                return orcs.register_intent({
                    name = "default_op",
                    description = "test default operation",
                    component = "lua::test_comp",
                })
                "#,
            )
            .eval()
            .expect("register_intent should return table");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "registration should succeed"
        );

        // Dispatch it to verify operation defaults to "execute"
        // Mock orcs.request to capture the operation argument
        lua.load(
            r#"
            orcs.request = function(target, operation, payload)
                return { success = true, data = { captured_op = operation } }
            end
            "#,
        )
        .exec()
        .expect("mock should succeed");

        let dispatch_result: Table = lua
            .load(r#"return orcs.dispatch("default_op", {})"#)
            .eval()
            .expect("dispatch should return table");

        assert!(dispatch_result.get::<bool>("ok").expect("should have ok"));
        let data: Table = dispatch_result.get("data").expect("should have data");
        assert_eq!(
            data.get::<String>("captured_op").expect("captured_op"),
            "execute",
            "operation should default to 'execute'"
        );
    }

    // --- register_tool tests ---

    #[test]
    fn register_tool_adds_intent_and_tool() {
        use orcs_component::tool::{RustTool, ToolContext, ToolError};
        use orcs_component::Capability;

        struct PingTool;

        impl RustTool for PingTool {
            fn name(&self) -> &str {
                "ping"
            }
            fn description(&self) -> &str {
                "Returns pong"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn required_capability(&self) -> Capability {
                Capability::READ
            }
            fn is_read_only(&self) -> bool {
                true
            }
            fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext<'_>,
            ) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({"reply": "pong"}))
            }
        }

        let mut registry = IntentRegistry::new();
        assert_eq!(registry.len(), 8, "starts with 8 builtins");

        registry
            .register_tool(Arc::new(PingTool))
            .expect("should register ping tool");

        assert_eq!(registry.len(), 9, "should now have 9");
        assert!(registry.get("ping").is_some(), "IntentDef should exist");
        assert!(registry.get_tool("ping").is_some(), "RustTool should exist");
        assert_eq!(
            registry.get("ping").expect("ping def").resolver,
            IntentResolver::Internal,
            "resolver should be Internal"
        );
    }

    #[test]
    fn register_tool_duplicate_fails() {
        use orcs_component::tool::{RustTool, ToolContext, ToolError};
        use orcs_component::Capability;

        struct DupTool;

        impl RustTool for DupTool {
            fn name(&self) -> &str {
                "read"
            }
            fn description(&self) -> &str {
                "duplicate of builtin"
            }
            fn parameters_schema(&self) -> serde_json::Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            fn required_capability(&self) -> Capability {
                Capability::READ
            }
            fn is_read_only(&self) -> bool {
                true
            }
            fn execute(
                &self,
                _args: serde_json::Value,
                _ctx: &ToolContext<'_>,
            ) -> Result<serde_json::Value, ToolError> {
                Ok(serde_json::json!({}))
            }
        }

        let mut registry = IntentRegistry::new();
        let err = registry
            .register_tool(Arc::new(DupTool))
            .expect_err("should reject duplicate");
        assert!(
            err.contains("already registered"),
            "error should mention duplicate, got: {err}"
        );
    }

    // --- dispatch_rust_tool: integer type preservation ---

    #[test]
    fn grep_dispatch_preserves_integer_line_number() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("nums.txt"), "alpha\nbeta\nalpha again\n")
            .expect("should write test file");

        let lua = setup_lua(sandbox);
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("grep", {{pattern="alpha", path="{}"}})"#,
                root.join("nums.txt").display()
            ))
            .eval()
            .expect("dispatch grep should succeed");

        assert!(result.get::<bool>("ok").expect("should have ok"));

        let matches: Table = result.get("matches").expect("should have matches");
        let first: Table = matches.get(1).expect("should have first match");

        // Verify line_number is Lua integer (not float).
        // mlua's get::<i64> succeeds only for Lua integers.
        let line_num: i64 = first
            .get("line_number")
            .expect("line_number should be accessible as i64");
        assert_eq!(line_num, 1, "first match should be line 1");

        // Also verify count is integer.
        let count: i64 = result
            .get("count")
            .expect("count should be accessible as i64");
        assert_eq!(count, 2, "should find 2 matches");
    }

    // --- dispatch_rust_tool: capability check ---

    /// Minimal mock ChildContext for testing capability gating.
    #[derive(Debug, Clone)]
    struct CapTestContext {
        caps: orcs_component::Capability,
    }

    impl orcs_component::ChildContext for CapTestContext {
        fn parent_id(&self) -> &str {
            "cap-test"
        }
        fn emit_output(&self, _msg: &str) {}
        fn emit_output_with_level(&self, _msg: &str, _level: &str) {}
        fn child_count(&self) -> usize {
            0
        }
        fn max_children(&self) -> usize {
            0
        }
        fn spawn_child(
            &self,
            _config: orcs_component::ChildConfig,
        ) -> Result<Box<dyn orcs_component::ChildHandle>, orcs_component::SpawnError> {
            Err(orcs_component::SpawnError::Internal("stub".into()))
        }
        fn send_to_child(
            &self,
            _id: &str,
            _input: serde_json::Value,
        ) -> Result<orcs_component::ChildResult, orcs_component::RunError> {
            Err(orcs_component::RunError::NotFound("stub".into()))
        }
        fn capabilities(&self) -> orcs_component::Capability {
            self.caps
        }
        fn check_command_permission(&self, _cmd: &str) -> orcs_component::CommandPermission {
            orcs_component::CommandPermission::Denied("stub".into())
        }
        fn can_execute_command(&self, _cmd: &str) -> bool {
            false
        }
        fn can_spawn_child_auth(&self) -> bool {
            false
        }
        fn can_spawn_runner_auth(&self) -> bool {
            false
        }
        fn grant_command(&self, _pattern: &str) {}
        fn spawn_runner_from_script(
            &self,
            _script: &str,
            _id: Option<&str>,
            _globals: Option<&serde_json::Map<String, serde_json::Value>>,
        ) -> Result<(orcs_types::ChannelId, String), orcs_component::SpawnError> {
            Err(orcs_component::SpawnError::Internal("stub".into()))
        }
        fn clone_box(&self) -> Box<dyn orcs_component::ChildContext> {
            Box::new(self.clone())
        }
    }

    #[test]
    fn dispatch_rust_tool_denies_without_capability() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("secret.txt"), "classified").expect("should write test file");

        let lua = setup_lua(sandbox);

        // Set ContextWrapper with NO capabilities.
        use crate::context_wrapper::ContextWrapper;
        use parking_lot::Mutex;
        let ctx: Box<dyn orcs_component::ChildContext> = Box::new(CapTestContext {
            caps: orcs_component::Capability::empty(),
        });
        lua.set_app_data(ContextWrapper(Arc::new(Mutex::new(ctx))));

        // read requires READ capability — should be denied.
        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("read", {{path="{}"}})"#,
                root.join("secret.txt").display()
            ))
            .eval()
            .expect("dispatch should return result table, not throw");

        let ok: bool = result.get("ok").expect("should have ok");
        assert!(!ok, "should be denied");
        let err: String = result.get("error").expect("should have error");
        assert!(
            err.contains("permission denied"),
            "error should mention permission denied, got: {err}"
        );
    }

    #[test]
    fn dispatch_rust_tool_allows_with_capability() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("allowed.txt"), "public data").expect("should write test file");

        let lua = setup_lua(sandbox);

        // Set ContextWrapper with READ capability.
        use crate::context_wrapper::ContextWrapper;
        use parking_lot::Mutex;
        let ctx: Box<dyn orcs_component::ChildContext> = Box::new(CapTestContext {
            caps: orcs_component::Capability::READ,
        });
        lua.set_app_data(ContextWrapper(Arc::new(Mutex::new(ctx))));

        let result: Table = lua
            .load(format!(
                r#"return orcs.dispatch("read", {{path="{}"}})"#,
                root.join("allowed.txt").display()
            ))
            .eval()
            .expect("dispatch should return result table");

        let ok: bool = result.get("ok").expect("should have ok");
        assert!(ok, "should succeed with READ capability");
        let content: String = result.get("content").expect("should have content");
        assert_eq!(content, "public data");
    }

    #[test]
    fn positional_write_denied_with_read_only_cap() {
        let (root, sandbox) = test_sandbox();

        let lua = setup_lua(sandbox);

        // Set ContextWrapper with READ only — WRITE not granted.
        // Uses positional orcs.write() which goes directly to dispatch_rust_tool
        // (bypassing HIL approval in dispatch_internal).
        use crate::context_wrapper::ContextWrapper;
        use parking_lot::Mutex;
        let ctx: Box<dyn orcs_component::ChildContext> = Box::new(CapTestContext {
            caps: orcs_component::Capability::READ,
        });
        lua.set_app_data(ContextWrapper(Arc::new(Mutex::new(ctx))));

        let result: Table = lua
            .load(format!(
                r#"return orcs.write("{}", "hacked")"#,
                root.join("nope.txt").display()
            ))
            .eval()
            .expect("positional write should return result table");

        let ok: bool = result.get("ok").expect("should have ok");
        assert!(!ok, "write should be denied with READ-only cap");
        let err: String = result.get("error").expect("should have error");
        assert!(
            err.contains("permission denied"),
            "error should mention permission denied, got: {err}"
        );
    }

    // --- dispatch_rust_tool: positional wrapper with capability ---

    #[test]
    fn positional_read_denied_without_capability() {
        let (root, sandbox) = test_sandbox();
        fs::write(root.join("pos_secret.txt"), "restricted").expect("should write test file");

        let lua = setup_lua(sandbox);

        use crate::context_wrapper::ContextWrapper;
        use parking_lot::Mutex;
        let ctx: Box<dyn orcs_component::ChildContext> = Box::new(CapTestContext {
            caps: orcs_component::Capability::empty(),
        });
        lua.set_app_data(ContextWrapper(Arc::new(Mutex::new(ctx))));

        // orcs.read() positional wrapper should also check capability.
        let result: Table = lua
            .load(format!(
                r#"return orcs.read("{}")"#,
                root.join("pos_secret.txt").display()
            ))
            .eval()
            .expect("positional read should return result table");

        let ok: bool = result.get("ok").expect("should have ok");
        assert!(!ok, "positional read should be denied");
        let err: String = result.get("error").expect("should have error");
        assert!(
            err.contains("permission denied"),
            "error should mention permission denied, got: {err}"
        );
    }

    // --- MCP Lua API tests ---

    #[test]
    fn mcp_servers_returns_ok_without_manager() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load("return orcs.mcp_servers()")
            .eval()
            .expect("mcp_servers should return table");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "mcp_servers without manager should return ok=true"
        );
        let servers: Table = result.get("servers").expect("should have servers");
        assert_eq!(
            servers.len().expect("servers length"),
            0,
            "servers list should be empty when no manager is set"
        );
    }

    #[test]
    fn mcp_tools_returns_empty_without_mcp_intents() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load("return orcs.mcp_tools()")
            .eval()
            .expect("mcp_tools should return table");

        assert!(
            result.get::<bool>("ok").expect("should have ok"),
            "mcp_tools should return ok=true"
        );
        let tools: Table = result.get("tools").expect("should have tools");
        assert_eq!(
            tools.len().expect("tools length"),
            0,
            "tools list should be empty when no MCP intents registered"
        );
    }

    #[test]
    fn mcp_tools_filters_by_server() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        // Manually register MCP intents to test filtering
        {
            let mut registry = lua
                .remove_app_data::<IntentRegistry>()
                .expect("registry should exist");
            let def_a = IntentDef {
                name: "mcp:srv_a:tool1".into(),
                description: "[MCP:srv_a] tool1 desc".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                resolver: IntentResolver::Mcp {
                    server_name: "srv_a".into(),
                    tool_name: "tool1".into(),
                },
            };
            let def_b = IntentDef {
                name: "mcp:srv_b:tool2".into(),
                description: "[MCP:srv_b] tool2 desc".into(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                resolver: IntentResolver::Mcp {
                    server_name: "srv_b".into(),
                    tool_name: "tool2".into(),
                },
            };
            registry.register(def_a).expect("register def_a");
            registry.register(def_b).expect("register def_b");
            lua.set_app_data(registry);
        }

        // Without filter: both tools
        let all: Table = lua
            .load("return orcs.mcp_tools()")
            .eval()
            .expect("mcp_tools() should succeed");
        assert!(all.get::<bool>("ok").expect("should have ok"));
        let all_tools: Table = all.get("tools").expect("should have tools");
        assert_eq!(
            all_tools.len().expect("all tools length"),
            2,
            "should list both MCP tools"
        );

        // With filter: only srv_a
        let filtered: Table = lua
            .load(r#"return orcs.mcp_tools("srv_a")"#)
            .eval()
            .expect("mcp_tools('srv_a') should succeed");
        assert!(filtered.get::<bool>("ok").expect("should have ok"));
        let filtered_tools: Table = filtered.get("tools").expect("should have tools");
        assert_eq!(
            filtered_tools.len().expect("filtered tools length"),
            1,
            "should list only srv_a tools"
        );
        let first: Table = filtered_tools.get(1).expect("first tool entry");
        assert_eq!(
            first.get::<String>("server").expect("should have server"),
            "srv_a"
        );
        assert_eq!(
            first.get::<String>("tool").expect("should have tool"),
            "tool1"
        );
    }

    #[test]
    fn mcp_call_without_manager_returns_error() {
        let (_, sandbox) = test_sandbox();
        let lua = setup_lua(sandbox);

        let result: Table = lua
            .load(r#"return orcs.mcp_call("srv", "tool", {})"#)
            .eval()
            .expect("mcp_call should return error table");

        assert!(
            !result.get::<bool>("ok").expect("should have ok"),
            "mcp_call without manager should return ok=false"
        );
        let err: String = result.get("error").expect("should have error");
        assert!(
            err.contains("MCP client not initialized"),
            "error should mention not initialized, got: {err}"
        );
    }
}

//! Hook helpers for Lua integration.
//!
//! Provides [`LuaHook`] (a Lua-backed [`Hook`] implementation),
//! shorthand descriptor parsing, and `HookContext ↔ Lua table`
//! conversion utilities.
//!
//! # `orcs.hook()` API
//!
//! Two calling conventions:
//!
//! ```lua
//! -- Shorthand: "fql:hook_point"
//! orcs.hook("builtin::llm:request.pre_dispatch", function(ctx)
//!     ctx.payload.injected = true
//!     return ctx
//! end)
//!
//! -- Table form (full control)
//! orcs.hook({
//!     fql      = "builtin::llm",
//!     point    = "request.pre_dispatch",
//!     handler  = function(ctx) return ctx end,
//!     priority = 50,   -- optional (default 100)
//!     id       = "my-hook",  -- optional
//! })
//! ```
//!
//! # Return value conventions
//!
//! | Lua return | HookAction |
//! |------------|------------|
//! | `nil` | `Continue(original_ctx)` |
//! | context table (has `hook_point`) | `Continue(parsed_ctx)` |
//! | `{ action = "continue", ctx = ... }` | `Continue(...)` |
//! | `{ action = "skip", result = ... }` | `Skip(value)` |
//! | `{ action = "abort", reason = "..." }` | `Abort { reason }` |
//! | `{ action = "replace", result = ... }` | `Replace(value)` |

use crate::error::LuaError;
use crate::orcs_helpers::ensure_orcs_table;
use mlua::{Function, Lua, LuaSerdeExt, Value};
use orcs_hook::{FqlPattern, Hook, HookAction, HookContext, HookPoint, SharedHookRegistry};
use orcs_types::ComponentId;
use parking_lot::Mutex;

// ── LuaHook ──────────────────────────────────────────────────────

/// A hook backed by a Lua function.
///
/// The handler function is stored in Lua's registry via [`mlua::RegistryKey`].
/// A cloned [`Lua`] handle provides thread-safe access to the same Lua state
/// (mlua uses internal locking with the `send` feature).
///
/// Thread-safety: `Mutex<Lua>` is `Send + Sync` because `Lua: Send`.
pub struct LuaHook {
    id: String,
    fql: FqlPattern,
    point: HookPoint,
    priority: i32,
    func_key: mlua::RegistryKey,
    lua: Mutex<Lua>,
}

impl LuaHook {
    /// Creates a new `LuaHook`.
    ///
    /// Stores `func` in Lua's registry and clones the `Lua` handle
    /// for later invocation from any thread.
    ///
    /// # Errors
    ///
    /// Returns an error if the function cannot be stored in the registry.
    pub fn new(
        id: String,
        fql: FqlPattern,
        point: HookPoint,
        priority: i32,
        lua: &Lua,
        func: &Function,
    ) -> Result<Self, mlua::Error> {
        let func_key = lua.create_registry_value(func.clone())?;
        Ok(Self {
            id,
            fql,
            point,
            priority,
            func_key,
            lua: Mutex::new(lua.clone()),
        })
    }

    /// Attempts to execute the hook, returning errors as `mlua::Error`.
    fn try_execute(&self, ctx: &HookContext) -> Result<HookAction, mlua::Error> {
        let lua = self.lua.lock();

        let func: Function = lua.registry_value(&self.func_key)?;
        let ctx_value: Value = lua.to_value(ctx)?;
        let result: Value = func.call(ctx_value)?;

        parse_hook_return(&lua, result, ctx)
    }
}

impl Hook for LuaHook {
    fn id(&self) -> &str {
        &self.id
    }

    fn fql_pattern(&self) -> &FqlPattern {
        &self.fql
    }

    fn hook_point(&self) -> HookPoint {
        self.point
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn execute(&self, ctx: HookContext) -> HookAction {
        match self.try_execute(&ctx) {
            Ok(action) => action,
            Err(e) => {
                tracing::error!(hook_id = %self.id, "LuaHook execution failed: {e}");
                HookAction::Continue(Box::new(ctx))
            }
        }
    }
}

impl std::fmt::Debug for LuaHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LuaHook")
            .field("id", &self.id)
            .field("fql", &self.fql)
            .field("point", &self.point)
            .field("priority", &self.priority)
            .finish()
    }
}

// ── Descriptor parsing ──────────────────────────────────────────

/// Parses a hook descriptor shorthand into FQL pattern and HookPoint.
///
/// Uses known-prefix matching (ADR 16.2 B案) to split the FQL part
/// from the HookPoint suffix at the *last* `:` whose right-hand side
/// starts with one of [`HookPoint::KNOWN_PREFIXES`].
///
/// # Examples
///
/// ```
/// # use orcs_lua::hook_helpers::parse_hook_descriptor;
/// let (fql, point) = parse_hook_descriptor("builtin::llm:request.pre_dispatch").unwrap();
/// assert_eq!(fql.to_string(), "builtin::llm");
/// assert_eq!(point.to_string(), "request.pre_dispatch");
/// ```
pub fn parse_hook_descriptor(descriptor: &str) -> Result<(FqlPattern, HookPoint), String> {
    // Scan for the last `:` where the suffix is a known HookPoint prefix.
    let mut split_pos = None;
    for (i, ch) in descriptor.char_indices() {
        if ch == ':' {
            let suffix = &descriptor[i + 1..];
            if HookPoint::KNOWN_PREFIXES
                .iter()
                .any(|p| suffix.starts_with(p))
            {
                split_pos = Some(i);
            }
        }
    }

    let pos = split_pos.ok_or_else(|| {
        format!(
            "invalid hook descriptor '{descriptor}': \
             expected '<fql>:<hook_point>' \
             (e.g., 'builtin::llm:request.pre_dispatch')"
        )
    })?;

    let fql_str = &descriptor[..pos];
    let point_str = &descriptor[pos + 1..];

    let fql = FqlPattern::parse(fql_str)
        .map_err(|e| format!("invalid FQL in descriptor '{descriptor}': {e}"))?;
    let point: HookPoint = point_str
        .parse()
        .map_err(|e| format!("invalid hook point in descriptor '{descriptor}': {e}"))?;

    Ok((fql, point))
}

// ── HookContext ↔ Lua conversion ────────────────────────────────

/// Converts a [`HookContext`] to a Lua value (table) via serde.
///
/// # Errors
///
/// Returns an error if serialization fails.
pub fn hook_context_to_lua(lua: &Lua, ctx: &HookContext) -> Result<Value, mlua::Error> {
    lua.to_value(ctx)
}

/// Converts a Lua value (table) back to a [`HookContext`] via serde.
///
/// # Errors
///
/// Returns an error if deserialization fails.
pub fn lua_to_hook_context(lua: &Lua, value: Value) -> Result<HookContext, mlua::Error> {
    lua.from_value(value)
}

// ── Return-value parsing ────────────────────────────────────────

/// Parses the return value from a Lua hook handler into a [`HookAction`].
///
/// See module-level docs for the full return-value conventions.
///
/// # Errors
///
/// Returns an error for unrecognised action strings or type mismatches.
pub fn parse_hook_return(
    lua: &Lua,
    result: Value,
    original_ctx: &HookContext,
) -> Result<HookAction, mlua::Error> {
    match result {
        // nil → pass through unchanged
        Value::Nil => Ok(HookAction::Continue(Box::new(original_ctx.clone()))),

        Value::Table(ref table) => {
            // Check for explicit action table (has "action" key)
            if let Ok(action_str) = table.get::<String>("action") {
                return parse_action_table(lua, &action_str, table, original_ctx);
            }

            // Check for context table (has "hook_point" key)
            if table.contains_key("hook_point")? {
                let ctx: HookContext = lua.from_value(result)?;
                return Ok(HookAction::Continue(Box::new(ctx)));
            }

            // Unknown table: treat as modified payload
            let mut ctx = original_ctx.clone();
            ctx.payload = lua.from_value(result)?;
            Ok(HookAction::Continue(Box::new(ctx)))
        }

        _ => Err(mlua::Error::RuntimeError(format!(
            "hook must return nil, a context table, or an action table (got {})",
            result.type_name()
        ))),
    }
}

/// Parses an explicit `{ action = "...", ... }` table.
fn parse_action_table(
    lua: &Lua,
    action: &str,
    table: &mlua::Table,
    original_ctx: &HookContext,
) -> Result<HookAction, mlua::Error> {
    match action {
        "continue" => {
            let ctx_val: Value = table.get("ctx").unwrap_or(Value::Nil);
            if ctx_val == Value::Nil {
                Ok(HookAction::Continue(Box::new(original_ctx.clone())))
            } else {
                let ctx: HookContext = lua.from_value(ctx_val)?;
                Ok(HookAction::Continue(Box::new(ctx)))
            }
        }
        "skip" => {
            let result_val: Value = table.get("result").unwrap_or(Value::Nil);
            let json_val: serde_json::Value = lua.from_value(result_val)?;
            Ok(HookAction::Skip(json_val))
        }
        "abort" => {
            let reason: String = table
                .get("reason")
                .unwrap_or_else(|_| "aborted by lua hook".to_string());
            Ok(HookAction::Abort { reason })
        }
        "replace" => {
            let result_val: Value = table.get("result").unwrap_or(Value::Nil);
            let json_val: serde_json::Value = lua.from_value(result_val)?;
            Ok(HookAction::Replace(json_val))
        }
        other => Err(mlua::Error::RuntimeError(format!(
            "unknown hook action: '{other}' (expected: continue, skip, abort, replace)"
        ))),
    }
}

// ── orcs.hook() registration ────────────────────────────────────

/// Registers the `orcs.hook()` function on the `orcs` global table.
///
/// Requires:
/// - `registry` — the shared hook registry for storing hooks
/// - `component_id` — the owning component (for `register_owned()`)
///
/// The registered hooks are automatically cleaned up when
/// [`HookRegistry::unregister_by_owner()`] is called with the same
/// component ID (typically on component shutdown).
///
/// # Errors
///
/// Returns an error if Lua function creation or table insertion fails.
pub fn register_hook_function(
    lua: &Lua,
    registry: SharedHookRegistry,
    component_id: ComponentId,
) -> Result<(), LuaError> {
    let orcs_table = ensure_orcs_table(lua)?;

    let comp_id = component_id.clone();
    let comp_fqn = component_id.fqn();

    let hook_fn = lua.create_function(move |lua, args: mlua::MultiValue| {
        let args_vec: Vec<Value> = args.into_vec();

        let (id, fql, point, priority, func) = match args_vec.len() {
            // ── Shorthand: orcs.hook("fql:point", handler) ──
            2 => {
                let descriptor = match &args_vec[0] {
                    Value::String(s) => s.to_str()?.to_string(),
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "orcs.hook(): first arg must be a descriptor string".to_string(),
                        ))
                    }
                };
                let handler = match &args_vec[1] {
                    Value::Function(f) => f.clone(),
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "orcs.hook(): second arg must be a function".to_string(),
                        ))
                    }
                };

                let (fql, point) =
                    parse_hook_descriptor(&descriptor).map_err(mlua::Error::RuntimeError)?;

                let id = format!("lua:{comp_fqn}:{point}");
                (id, fql, point, 100i32, handler)
            }

            // ── Table form: orcs.hook({ fql=..., point=..., handler=... }) ──
            1 => {
                let table = match &args_vec[0] {
                    Value::Table(t) => t,
                    _ => {
                        return Err(mlua::Error::RuntimeError(
                            "orcs.hook(): arg must be a descriptor string or table".to_string(),
                        ))
                    }
                };

                let fql_str: String = table.get("fql").map_err(|_| {
                    mlua::Error::RuntimeError("orcs.hook() table: 'fql' field required".to_string())
                })?;
                let point_str: String = table.get("point").map_err(|_| {
                    mlua::Error::RuntimeError(
                        "orcs.hook() table: 'point' field required".to_string(),
                    )
                })?;
                let handler: Function = table.get("handler").map_err(|_| {
                    mlua::Error::RuntimeError(
                        "orcs.hook() table: 'handler' field required".to_string(),
                    )
                })?;

                let fql = FqlPattern::parse(&fql_str).map_err(|e| {
                    mlua::Error::RuntimeError(format!("orcs.hook(): invalid FQL: {e}"))
                })?;
                let point: HookPoint = point_str.parse().map_err(|e| {
                    mlua::Error::RuntimeError(format!("orcs.hook(): invalid hook point: {e}"))
                })?;

                let priority: i32 = table.get("priority").unwrap_or(100);
                let id: String = table
                    .get("id")
                    .unwrap_or_else(|_| format!("lua:{comp_fqn}:{point}"));

                (id, fql, point, priority, handler)
            }

            n => {
                return Err(mlua::Error::RuntimeError(format!(
                    "orcs.hook() expects 1 or 2 arguments, got {n}"
                )))
            }
        };

        let lua_hook = LuaHook::new(id.clone(), fql, point, priority, lua, &func)
            .map_err(|e| mlua::Error::RuntimeError(format!("failed to create LuaHook: {e}")))?;

        let mut guard = registry
            .write()
            .map_err(|e| mlua::Error::RuntimeError(format!("hook registry lock poisoned: {e}")))?;
        guard.register_owned(Box::new(lua_hook), comp_id.clone());

        tracing::debug!(hook_id = %id, "Lua hook registered");
        Ok(())
    })?;

    orcs_table.set("hook", hook_fn)?;
    Ok(())
}

/// Registers a default (deny) `orcs.hook()` stub.
///
/// Used when no hook registry is available yet.
/// Returns an error table `{ ok = false, error = "..." }`.
pub fn register_hook_stub(lua: &Lua) -> Result<(), LuaError> {
    let orcs_table = ensure_orcs_table(lua)?;

    if orcs_table.get::<Function>("hook").is_ok() {
        return Ok(()); // Already registered (possibly the real one)
    }

    let stub = lua.create_function(|_, _args: mlua::MultiValue| {
        Err::<(), _>(mlua::Error::RuntimeError(
            "orcs.hook(): no hook registry available (ChildContext required)".to_string(),
        ))
    })?;

    orcs_table.set("hook", stub)?;
    Ok(())
}

// ── orcs.unhook() registration ───────────────────────────────────

/// Registers the `orcs.unhook(id)` function on the `orcs` global table.
///
/// Removes a previously registered hook by its ID. Returns `true` if
/// the hook was found and removed, `false` otherwise.
///
/// # Errors
///
/// Returns an error if Lua function creation or table insertion fails.
pub fn register_unhook_function(lua: &Lua, registry: SharedHookRegistry) -> Result<(), LuaError> {
    let orcs_table = ensure_orcs_table(lua)?;

    let unhook_fn = lua.create_function(move |_, id: String| {
        let mut guard = registry
            .write()
            .map_err(|e| mlua::Error::RuntimeError(format!("hook registry lock poisoned: {e}")))?;
        let removed = guard.unregister(&id);
        if removed {
            tracing::debug!(hook_id = %id, "Lua hook unregistered");
        }
        Ok(removed)
    })?;

    orcs_table.set("unhook", unhook_fn)?;
    Ok(())
}

// ── Config-based hook loading ────────────────────────────────────

/// Error from loading a single hook definition.
#[derive(Debug)]
pub struct HookLoadError {
    /// Label of the hook that failed (id or "<anonymous>").
    pub hook_label: String,
    /// Description of the error.
    pub error: String,
}

impl std::fmt::Display for HookLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "hook '{}': {}", self.hook_label, self.error)
    }
}

/// Result of loading hooks from configuration.
#[derive(Debug)]
pub struct HookLoadResult {
    /// Number of hooks successfully loaded.
    pub loaded: usize,
    /// Number of disabled hooks that were skipped.
    pub skipped: usize,
    /// Errors encountered during loading (non-fatal per hook).
    pub errors: Vec<HookLoadError>,
}

/// Loads hooks from a [`HooksConfig`] into a [`SharedHookRegistry`].
///
/// For each enabled hook definition:
/// - `handler_inline`: Evaluates the inline Lua code as a function
/// - `script`: Reads and evaluates the script file relative to `base_path`
///
/// Disabled hooks are silently skipped. Invalid hooks produce errors
/// but do not prevent other hooks from loading.
///
/// # Arguments
///
/// * `config` - Hook definitions from TOML/JSON configuration
/// * `registry` - Shared hook registry to register into
/// * `base_path` - Base directory for resolving relative script paths
///
/// # Returns
///
/// A summary of loaded hooks, skipped hooks, and any errors.
pub fn load_hooks_from_config(
    config: &orcs_hook::HooksConfig,
    registry: &SharedHookRegistry,
    base_path: &std::path::Path,
) -> HookLoadResult {
    let mut loaded = 0;
    let mut skipped = 0;
    let mut errors = Vec::new();

    for def in &config.hooks {
        let label = def.id.as_deref().unwrap_or("<anonymous>").to_string();

        if !def.enabled {
            skipped += 1;
            tracing::debug!(hook = %label, "Skipping disabled hook");
            continue;
        }

        if let Err(e) = def.validate() {
            errors.push(HookLoadError {
                hook_label: label,
                error: e.to_string(),
            });
            continue;
        }

        let fql = match FqlPattern::parse(&def.fql) {
            Ok(f) => f,
            Err(e) => {
                errors.push(HookLoadError {
                    hook_label: label,
                    error: format!("invalid FQL: {e}"),
                });
                continue;
            }
        };

        let point: HookPoint = match def.point.parse() {
            Ok(p) => p,
            Err(e) => {
                errors.push(HookLoadError {
                    hook_label: label,
                    error: format!("invalid hook point: {e}"),
                });
                continue;
            }
        };

        let lua = Lua::new();

        let func_result = if let Some(inline) = &def.handler_inline {
            load_inline_handler(&lua, inline)
        } else if let Some(script_path) = &def.script {
            load_script_handler(&lua, base_path, script_path)
        } else {
            Err(mlua::Error::RuntimeError(
                "no handler specified".to_string(),
            ))
        };

        let func = match func_result {
            Ok(f) => f,
            Err(e) => {
                errors.push(HookLoadError {
                    hook_label: label,
                    error: format!("failed to load handler: {e}"),
                });
                continue;
            }
        };

        let hook_id = def
            .id
            .clone()
            .unwrap_or_else(|| format!("config:{point}:{loaded}"));

        let lua_hook = match LuaHook::new(hook_id.clone(), fql, point, def.priority, &lua, &func) {
            Ok(h) => h,
            Err(e) => {
                errors.push(HookLoadError {
                    hook_label: label,
                    error: format!("failed to create LuaHook: {e}"),
                });
                continue;
            }
        };

        match registry.write() {
            Ok(mut guard) => {
                guard.register(Box::new(lua_hook));
                loaded += 1;
                tracing::info!(hook = %label, point = %point, "Loaded hook from config");
            }
            Err(e) => {
                errors.push(HookLoadError {
                    hook_label: label,
                    error: format!("registry lock poisoned: {e}"),
                });
            }
        }
    }

    HookLoadResult {
        loaded,
        skipped,
        errors,
    }
}

/// Loads an inline handler string as a Lua function.
///
/// Tries the code as-is first, then wraps with `return` if needed.
fn load_inline_handler(lua: &Lua, inline: &str) -> Result<Function, mlua::Error> {
    // Try with "return" prefix first (most common: bare function literal)
    let with_return = format!("return {inline}");
    lua.load(&with_return).eval::<Function>().or_else(|_| {
        // Try as-is (user wrote "return function(...) ... end")
        lua.load(inline).eval::<Function>()
    })
}

/// Loads a script file as a Lua function.
///
/// The script should return a function when evaluated.
fn load_script_handler(
    lua: &Lua,
    base_path: &std::path::Path,
    script_path: &str,
) -> Result<Function, mlua::Error> {
    let full_path = base_path.join(script_path);
    let script = std::fs::read_to_string(&full_path).map_err(|e| {
        mlua::Error::RuntimeError(format!(
            "failed to read script '{}': {e}",
            full_path.display()
        ))
    })?;

    let with_return = format!("return {script}");
    lua.load(&script)
        .eval::<Function>()
        .or_else(|_| lua.load(&with_return).eval::<Function>())
}

// ── Tests ───────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_hook::HookRegistry;
    use orcs_types::{ChannelId, Principal};
    use serde_json::json;
    use std::sync::{Arc, RwLock};

    fn test_lua() -> Lua {
        Lua::new()
    }

    fn test_ctx() -> HookContext {
        HookContext::new(
            HookPoint::RequestPreDispatch,
            ComponentId::builtin("llm"),
            ChannelId::new(),
            Principal::System,
            12345,
            json!({"operation": "chat"}),
        )
    }

    fn test_registry() -> SharedHookRegistry {
        Arc::new(RwLock::new(HookRegistry::new()))
    }

    // ── parse_hook_descriptor ───────────────────────────────────

    #[test]
    fn parse_descriptor_basic() {
        let (fql, point) = parse_hook_descriptor("builtin::llm:request.pre_dispatch").unwrap();
        assert_eq!(fql.to_string(), "builtin::llm");
        assert_eq!(point, HookPoint::RequestPreDispatch);
    }

    #[test]
    fn parse_descriptor_wildcard_fql() {
        let (fql, point) = parse_hook_descriptor("*::*:component.pre_init").unwrap();
        assert_eq!(fql.to_string(), "*::*");
        assert_eq!(point, HookPoint::ComponentPreInit);
    }

    #[test]
    fn parse_descriptor_with_child_path() {
        let (fql, point) =
            parse_hook_descriptor("builtin::llm/agent-1:signal.pre_dispatch").unwrap();
        assert_eq!(fql.to_string(), "builtin::llm/agent-1");
        assert_eq!(point, HookPoint::SignalPreDispatch);
    }

    #[test]
    fn parse_descriptor_all_points() {
        let descriptors = [
            ("*::*:component.pre_init", HookPoint::ComponentPreInit),
            ("*::*:component.post_init", HookPoint::ComponentPostInit),
            ("*::*:request.pre_dispatch", HookPoint::RequestPreDispatch),
            ("*::*:request.post_dispatch", HookPoint::RequestPostDispatch),
            ("*::*:signal.pre_dispatch", HookPoint::SignalPreDispatch),
            ("*::*:signal.post_dispatch", HookPoint::SignalPostDispatch),
            ("*::*:child.pre_spawn", HookPoint::ChildPreSpawn),
            ("*::*:tool.pre_execute", HookPoint::ToolPreExecute),
            ("*::*:auth.pre_check", HookPoint::AuthPreCheck),
            ("*::*:bus.pre_broadcast", HookPoint::BusPreBroadcast),
        ];
        for (desc, expected) in &descriptors {
            let (_, point) = parse_hook_descriptor(desc)
                .unwrap_or_else(|e| panic!("failed to parse '{desc}': {e}"));
            assert_eq!(point, *expected, "mismatch for '{desc}'");
        }
    }

    #[test]
    fn parse_descriptor_invalid_no_hookpoint() {
        let result = parse_hook_descriptor("builtin::llm");
        assert!(result.is_err());
    }

    #[test]
    fn parse_descriptor_invalid_empty() {
        let result = parse_hook_descriptor("");
        assert!(result.is_err());
    }

    #[test]
    fn parse_descriptor_invalid_bad_fql() {
        let result = parse_hook_descriptor("nocolon:request.pre_dispatch");
        assert!(result.is_err());
    }

    #[test]
    fn parse_descriptor_invalid_bad_point() {
        let result = parse_hook_descriptor("builtin::llm:nonexistent.point");
        assert!(result.is_err());
    }

    // ── HookContext ↔ Lua roundtrip ─────────────────────────────

    #[test]
    fn context_lua_roundtrip() {
        let lua = test_lua();
        let ctx = test_ctx();

        let lua_val = hook_context_to_lua(&lua, &ctx).expect("to_lua");
        let restored = lua_to_hook_context(&lua, lua_val).expect("from_lua");

        assert_eq!(restored.hook_point, ctx.hook_point);
        assert_eq!(restored.payload, ctx.payload);
        assert_eq!(restored.depth, ctx.depth);
        assert_eq!(restored.max_depth, ctx.max_depth);
    }

    #[test]
    fn context_with_metadata_roundtrip() {
        let lua = test_lua();
        let ctx = test_ctx().with_metadata("audit", json!("abc-123"));

        let lua_val = hook_context_to_lua(&lua, &ctx).expect("to_lua");
        let restored = lua_to_hook_context(&lua, lua_val).expect("from_lua");

        assert_eq!(restored.metadata.get("audit"), Some(&json!("abc-123")));
    }

    // ── parse_hook_return ───────────────────────────────────────

    #[test]
    fn return_nil_is_continue() {
        let lua = test_lua();
        let ctx = test_ctx();

        let action = parse_hook_return(&lua, Value::Nil, &ctx).unwrap();
        assert!(action.is_continue());
    }

    #[test]
    fn return_context_table_is_continue() {
        let lua = test_lua();
        let ctx = test_ctx();

        // Create a context table in Lua (has hook_point key)
        let ctx_value = hook_context_to_lua(&lua, &ctx).unwrap();
        let action = parse_hook_return(&lua, ctx_value, &ctx).unwrap();
        assert!(action.is_continue());
    }

    #[test]
    fn return_action_skip() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua
            .load(r#"return { action = "skip", result = { cached = true } }"#)
            .eval()
            .unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        assert!(action.is_skip());
        if let HookAction::Skip(val) = action {
            assert_eq!(val, json!({"cached": true}));
        }
    }

    #[test]
    fn return_action_abort() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua
            .load(r#"return { action = "abort", reason = "policy violation" }"#)
            .eval()
            .unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        assert!(action.is_abort());
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "policy violation");
        }
    }

    #[test]
    fn return_action_replace() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua
            .load(r#"return { action = "replace", result = { new_data = 42 } }"#)
            .eval()
            .unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        assert!(action.is_replace());
        if let HookAction::Replace(val) = action {
            assert_eq!(val, json!({"new_data": 42}));
        }
    }

    #[test]
    fn return_action_continue_explicit() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua
            .load(r#"return { action = "continue" }"#)
            .eval()
            .unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        assert!(action.is_continue());
    }

    #[test]
    fn return_action_abort_default_reason() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua.load(r#"return { action = "abort" }"#).eval().unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "aborted by lua hook");
        } else {
            panic!("expected Abort");
        }
    }

    #[test]
    fn return_unknown_action_errors() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua.load(r#"return { action = "invalid" }"#).eval().unwrap();

        let result = parse_hook_return(&lua, table, &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn return_number_errors() {
        let lua = test_lua();
        let ctx = test_ctx();

        let result = parse_hook_return(&lua, Value::Integer(42), &ctx);
        assert!(result.is_err());
    }

    #[test]
    fn return_unknown_table_updates_payload() {
        let lua = test_lua();
        let ctx = test_ctx();

        let table: Value = lua
            .load(r#"return { custom_field = "hello" }"#)
            .eval()
            .unwrap();

        let action = parse_hook_return(&lua, table, &ctx).unwrap();
        assert!(action.is_continue());
        if let HookAction::Continue(new_ctx) = action {
            assert_eq!(new_ctx.payload, json!({"custom_field": "hello"}));
        }
    }

    // ── LuaHook execution ───────────────────────────────────────

    #[test]
    fn lua_hook_pass_through() {
        let lua = test_lua();
        let func: Function = lua.load("function(ctx) return ctx end").eval().unwrap();

        let hook = LuaHook::new(
            "test-pass".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        assert_eq!(hook.id(), "test-pass");
        assert_eq!(hook.hook_point(), HookPoint::RequestPreDispatch);
        assert_eq!(hook.priority(), 100);

        let ctx = test_ctx();
        let action = hook.execute(ctx);
        assert!(action.is_continue());
    }

    #[test]
    fn lua_hook_modifies_payload() {
        let lua = test_lua();
        let func: Function = lua
            .load(
                r#"
                function(ctx)
                    ctx.payload.injected = true
                    return ctx
                end
                "#,
            )
            .eval()
            .unwrap();

        let hook = LuaHook::new(
            "test-mod".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        let ctx = test_ctx();
        let action = hook.execute(ctx);

        if let HookAction::Continue(new_ctx) = action {
            assert_eq!(new_ctx.payload["injected"], json!(true));
            // Original fields preserved
            assert_eq!(new_ctx.payload["operation"], json!("chat"));
        } else {
            panic!("expected Continue");
        }
    }

    #[test]
    fn lua_hook_returns_skip() {
        let lua = test_lua();
        let func: Function = lua
            .load(r#"function(ctx) return { action = "skip", result = { cached = true } } end"#)
            .eval()
            .unwrap();

        let hook = LuaHook::new(
            "test-skip".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        let action = hook.execute(test_ctx());
        assert!(action.is_skip());
    }

    #[test]
    fn lua_hook_returns_abort() {
        let lua = test_lua();
        let func: Function = lua
            .load(r#"function(ctx) return { action = "abort", reason = "blocked" } end"#)
            .eval()
            .unwrap();

        let hook = LuaHook::new(
            "test-abort".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        let action = hook.execute(test_ctx());
        assert!(action.is_abort());
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "blocked");
        }
    }

    #[test]
    fn lua_hook_error_falls_back_to_continue() {
        let lua = test_lua();
        let func: Function = lua
            .load(r#"function(ctx) error("intentional error") end"#)
            .eval()
            .unwrap();

        let hook = LuaHook::new(
            "test-err".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        let ctx = test_ctx();
        let action = hook.execute(ctx.clone());

        // On error, should fall back to Continue with original ctx
        assert!(action.is_continue());
        if let HookAction::Continue(result_ctx) = action {
            assert_eq!(result_ctx.payload, ctx.payload);
        }
    }

    #[test]
    fn lua_hook_nil_return_is_continue() {
        let lua = test_lua();
        let func: Function = lua
            .load("function(ctx) end") // returns nil
            .eval()
            .unwrap();

        let hook = LuaHook::new(
            "test-nil".to_string(),
            FqlPattern::parse("*::*").unwrap(),
            HookPoint::RequestPreDispatch,
            100,
            &lua,
            &func,
        )
        .unwrap();

        let action = hook.execute(test_ctx());
        assert!(action.is_continue());
    }

    // ── register_hook_function integration ──────────────────────

    #[test]
    fn register_hook_from_lua_shorthand() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        lua.load(
            r#"
            orcs.hook("*::*:request.pre_dispatch", function(ctx)
                return ctx
            end)
            "#,
        )
        .exec()
        .expect("orcs.hook() should succeed");

        let guard = registry.read().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn register_hook_from_lua_table_form() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        lua.load(
            r#"
            orcs.hook({
                fql = "builtin::llm",
                point = "request.pre_dispatch",
                handler = function(ctx) return ctx end,
                priority = 50,
                id = "custom-hook-id",
            })
            "#,
        )
        .exec()
        .expect("orcs.hook() table form should succeed");

        let guard = registry.read().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn register_multiple_hooks() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        lua.load(
            r#"
            orcs.hook("*::*:request.pre_dispatch", function(ctx) return ctx end)
            orcs.hook("*::*:signal.pre_dispatch", function(ctx) return ctx end)
            orcs.hook("builtin::llm:component.pre_init", function(ctx) return ctx end)
            "#,
        )
        .exec()
        .expect("multiple hooks should succeed");

        let guard = registry.read().unwrap();
        assert_eq!(guard.len(), 3);
    }

    #[test]
    fn register_hook_invalid_descriptor_errors() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        let result = lua
            .load(r#"orcs.hook("invalid_no_point", function(ctx) return ctx end)"#)
            .exec();

        assert!(result.is_err());
    }

    #[test]
    fn register_hook_wrong_args_errors() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        // Too many args
        let result = lua.load(r#"orcs.hook("a", "b", "c")"#).exec();
        assert!(result.is_err());

        // Zero args
        let result = lua.load(r#"orcs.hook()"#).exec();
        assert!(result.is_err());
    }

    #[test]
    fn hook_stub_returns_error() {
        let lua = test_lua();
        let orcs_table = lua.create_table().unwrap();
        lua.globals().set("orcs", orcs_table).unwrap();

        register_hook_stub(&lua).unwrap();

        let result = lua
            .load(r#"orcs.hook("*::*:request.pre_dispatch", function(ctx) end)"#)
            .exec();
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no hook registry"),
            "expected 'no hook registry' in error, got: {err}"
        );
    }

    // ── LuaHook with registry dispatch ──────────────────────────

    #[test]
    fn lua_hook_dispatched_through_registry() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        // Register a hook that adds a field to the payload
        lua.load(
            r#"
            orcs.hook("*::*:request.pre_dispatch", function(ctx)
                ctx.payload.hook_ran = true
                return ctx
            end)
            "#,
        )
        .exec()
        .unwrap();

        // Dispatch through the registry
        let ctx = test_ctx();
        let guard = registry.read().unwrap();
        let action = guard.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert!(action.is_continue());
        if let HookAction::Continue(result_ctx) = action {
            assert_eq!(result_ctx.payload["hook_ran"], json!(true));
            assert_eq!(result_ctx.payload["operation"], json!("chat"));
        }
    }

    #[test]
    fn lua_hook_abort_stops_dispatch() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();

        lua.load(
            r#"
            orcs.hook("*::*:request.pre_dispatch", function(ctx)
                return { action = "abort", reason = "denied by lua" }
            end)
            "#,
        )
        .exec()
        .unwrap();

        let ctx = test_ctx();
        let guard = registry.read().unwrap();
        let action = guard.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );

        assert!(action.is_abort());
        if let HookAction::Abort { reason } = action {
            assert_eq!(reason, "denied by lua");
        }
    }

    // ── register_unhook_function ─────────────────────────────────

    #[test]
    fn unhook_removes_registered_hook() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();
        register_unhook_function(&lua, Arc::clone(&registry)).unwrap();

        // Register a hook
        lua.load(
            r#"
            orcs.hook({
                fql = "*::*",
                point = "request.pre_dispatch",
                handler = function(ctx) return ctx end,
                id = "removable-hook",
            })
            "#,
        )
        .exec()
        .unwrap();

        assert_eq!(registry.read().unwrap().len(), 1);

        // Unhook it
        let removed: bool = lua
            .load(r#"return orcs.unhook("removable-hook")"#)
            .eval()
            .unwrap();
        assert!(removed);
        assert_eq!(registry.read().unwrap().len(), 0);
    }

    #[test]
    fn unhook_returns_false_for_unknown_id() {
        let lua = test_lua();
        let registry = test_registry();

        register_unhook_function(&lua, Arc::clone(&registry)).unwrap();

        let removed: bool = lua
            .load(r#"return orcs.unhook("nonexistent")"#)
            .eval()
            .unwrap();
        assert!(!removed);
    }

    #[test]
    fn unhook_after_hook_roundtrip() {
        let lua = test_lua();
        let registry = test_registry();
        let comp_id = ComponentId::builtin("test-comp");

        register_hook_function(&lua, Arc::clone(&registry), comp_id).unwrap();
        register_unhook_function(&lua, Arc::clone(&registry)).unwrap();

        // Register 2 hooks, remove 1
        lua.load(
            r#"
            orcs.hook("*::*:request.pre_dispatch", function(ctx) return ctx end)
            orcs.hook({
                fql = "*::*",
                point = "signal.pre_dispatch",
                handler = function(ctx) return ctx end,
                id = "keep-this",
            })
            "#,
        )
        .exec()
        .unwrap();

        assert_eq!(registry.read().unwrap().len(), 2);

        // Remove the auto-named one (lua:builtin::test-comp:request.pre_dispatch)
        let removed: bool = lua
            .load(r#"return orcs.unhook("lua:builtin::test-comp:request.pre_dispatch")"#)
            .eval()
            .unwrap();
        assert!(removed);
        assert_eq!(registry.read().unwrap().len(), 1);
    }

    // ── load_hooks_from_config ──────────────────────────────────

    fn make_hook_def(id: &str, fql: &str, point: &str, inline: &str) -> orcs_hook::HookDef {
        orcs_hook::HookDef {
            id: Some(id.to_string()),
            fql: fql.to_string(),
            point: point.to_string(),
            script: None,
            handler_inline: Some(inline.to_string()),
            priority: 100,
            enabled: true,
        }
    }

    #[test]
    fn load_config_inline_handler() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![make_hook_def(
                "test-inline",
                "*::*",
                "request.pre_dispatch",
                "function(ctx) return ctx end",
            )],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 1);
        assert_eq!(result.skipped, 0);
        assert!(
            result.errors.is_empty(),
            "errors: {:?}",
            result.errors.iter().map(|e| &e.error).collect::<Vec<_>>()
        );
    }

    #[test]
    fn load_config_disabled_skipped() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![{
                let mut def = make_hook_def(
                    "disabled",
                    "*::*",
                    "request.pre_dispatch",
                    "function(ctx) return ctx end",
                );
                def.enabled = false;
                def
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 0);
        assert_eq!(result.skipped, 1);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn load_config_invalid_handler_errors() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![make_hook_def(
                "bad-syntax",
                "*::*",
                "request.pre_dispatch",
                "not a valid lua function %%%",
            )],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].error.contains("failed to load handler"));
    }

    #[test]
    fn load_config_invalid_fql_errors() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![{
                let mut def = make_hook_def(
                    "bad-fql",
                    "invalid",
                    "request.pre_dispatch",
                    "function(ctx) return ctx end",
                );
                // fql "invalid" won't parse (needs "::")
                def.fql = "invalid".to_string();
                def
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn load_config_no_handler_errors() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("no-handler".to_string()),
                fql: "*::*".to_string(),
                point: "request.pre_dispatch".to_string(),
                script: None,
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 0);
        assert_eq!(result.errors.len(), 1);
    }

    #[test]
    fn load_config_multiple_hooks() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![
                make_hook_def(
                    "h1",
                    "*::*",
                    "request.pre_dispatch",
                    "function(ctx) return ctx end",
                ),
                make_hook_def(
                    "h2",
                    "builtin::llm",
                    "signal.pre_dispatch",
                    "function(ctx) return ctx end",
                ),
                {
                    let mut def = make_hook_def(
                        "h3-disabled",
                        "*::*",
                        "tool.pre_execute",
                        "function(ctx) return ctx end",
                    );
                    def.enabled = false;
                    def
                },
            ],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 2);
        assert_eq!(result.skipped, 1);
        assert!(result.errors.is_empty());

        let guard = registry.read().unwrap();
        assert_eq!(guard.len(), 2);
    }

    #[test]
    fn load_config_script_file_not_found() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("file-hook".to_string()),
                fql: "*::*".to_string(),
                point: "request.pre_dispatch".to_string(),
                script: Some("nonexistent/hook.lua".to_string()),
                handler_inline: None,
                priority: 100,
                enabled: true,
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 0);
        assert_eq!(result.errors.len(), 1);
        assert!(result.errors[0].error.contains("failed to load handler"));
    }

    #[test]
    fn load_config_script_file_success() {
        // Create a temp file with a Lua function
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("test_hook.lua");
        std::fs::write(
            &script_path,
            "return function(ctx) ctx.payload.from_file = true return ctx end",
        )
        .unwrap();

        let config = orcs_hook::HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: Some("file-hook".to_string()),
                fql: "*::*".to_string(),
                point: "request.pre_dispatch".to_string(),
                script: Some("test_hook.lua".to_string()),
                handler_inline: None,
                priority: 50,
                enabled: true,
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, dir.path());

        assert_eq!(result.loaded, 1);
        assert!(result.errors.is_empty());

        // Verify the hook dispatches correctly
        let guard = registry.read().unwrap();
        let ctx = test_ctx();
        let action = guard.dispatch(
            HookPoint::RequestPreDispatch,
            &ComponentId::builtin("llm"),
            None,
            ctx,
        );
        assert!(action.is_continue());
        if let HookAction::Continue(result_ctx) = action {
            assert_eq!(result_ctx.payload["from_file"], json!(true));
        }
    }

    #[test]
    fn load_config_inline_handler_with_return() {
        // User writes "return function(...)" explicitly
        let config = orcs_hook::HooksConfig {
            hooks: vec![make_hook_def(
                "with-return",
                "*::*",
                "request.pre_dispatch",
                "return function(ctx) return ctx end",
            )],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 1);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn load_config_anonymous_hook_gets_generated_id() {
        let config = orcs_hook::HooksConfig {
            hooks: vec![orcs_hook::HookDef {
                id: None,
                fql: "*::*".to_string(),
                point: "request.pre_dispatch".to_string(),
                script: None,
                handler_inline: Some("function(ctx) return ctx end".to_string()),
                priority: 100,
                enabled: true,
            }],
        };
        let registry = test_registry();
        let result = load_hooks_from_config(&config, &registry, std::path::Path::new("."));

        assert_eq!(result.loaded, 1);
        assert!(result.errors.is_empty());
    }

    #[test]
    fn hook_load_error_display() {
        let err = HookLoadError {
            hook_label: "my-hook".to_string(),
            error: "syntax error".to_string(),
        };
        assert_eq!(err.to_string(), "hook 'my-hook': syntax error");
    }
}

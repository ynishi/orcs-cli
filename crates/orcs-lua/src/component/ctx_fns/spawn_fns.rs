//! Child/runner spawning and lifecycle function registration.
//!
//! Functions registered:
//! - `orcs.spawn_child(config)` — spawn a child process
//! - `orcs.spawn_runner(config)` — spawn a ChannelRunner
//! - `orcs.child_count()` — current child count
//! - `orcs.max_children()` — max allowed children
//! - `orcs.request_approval(op, desc)` — request HIL approval

use mlua::{Lua, LuaSerdeExt, Table};
use orcs_component::{ChildConfig, ChildContext};
use parking_lot::Mutex;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    orcs_table: &Table,
    ctx: &Arc<Mutex<Box<dyn ChildContext>>>,
) -> Result<(), mlua::Error> {
    // ── orcs.spawn_child(config) ─────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let spawn_child_fn = lua.create_function(move |lua, config: Table| {
            let ctx_guard = ctx_clone.lock();

            // Capability gate: SPAWN required
            if !ctx_guard.has_capability(orcs_component::Capability::SPAWN) {
                return simple_error_table(lua, "permission denied: Capability::SPAWN not granted");
            }

            // Auth permission check
            if !ctx_guard.can_spawn_child_auth() {
                return simple_error_table(
                    lua,
                    "permission denied: spawn_child requires elevated session",
                );
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
    }

    // ── orcs.request_approval(operation, description) ────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let request_approval_fn =
            lua.create_function(move |_, (operation, description): (String, String)| {
                let ctx_guard = ctx_clone.lock();

                let approval_id = ctx_guard.emit_approval_request(&operation, &description);
                Ok(approval_id)
            })?;
        orcs_table.set("request_approval", request_approval_fn)?;
    }

    // ── orcs.child_count() ───────────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let child_count_fn = lua.create_function(move |_, ()| {
            let ctx_guard = ctx_clone.lock();
            Ok(ctx_guard.child_count())
        })?;
        orcs_table.set("child_count", child_count_fn)?;
    }

    // ── orcs.max_children() ──────────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let max_children_fn = lua.create_function(move |_, ()| {
            let ctx_guard = ctx_clone.lock();
            Ok(ctx_guard.max_children())
        })?;
        orcs_table.set("max_children", max_children_fn)?;
    }

    // ── orcs.spawn_runner(config) ────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let spawn_runner_fn = lua.create_function(move |lua, config: Table| {
            let ctx_guard = ctx_clone.lock();

            // Capability gate: SPAWN required
            if !ctx_guard.has_capability(orcs_component::Capability::SPAWN) {
                return simple_error_table(lua, "permission denied: Capability::SPAWN not granted");
            }

            // Auth permission check
            if !ctx_guard.can_spawn_runner_auth() {
                return simple_error_table(
                    lua,
                    "permission denied: spawn_runner requires elevated session",
                );
            }

            // ID is optional
            let id: Option<String> = config.get("id").ok();

            // Parse optional globals (only meaningful for script path, ignored for builtin)
            let globals_raw: mlua::Value = config.get("globals")?;
            let globals = parse_globals_from_lua(lua, globals_raw)?;

            // Resolve script: builtin name takes precedence over inline script
            let spawn_result = if let Ok(builtin_name) = config.get::<String>("builtin") {
                ctx_guard.spawn_runner_from_builtin(&builtin_name, id.as_deref(), globals.as_ref())
            } else if let Ok(script) = config.get::<String>("script") {
                ctx_guard.spawn_runner_from_script(&script, id.as_deref(), globals.as_ref())
            } else {
                return Err(mlua::Error::RuntimeError(
                    "config.builtin or config.script required".into(),
                ));
            };

            let result_table = lua.create_table()?;
            match spawn_result {
                Ok((channel_id, fqn)) => {
                    result_table.set("ok", true)?;
                    result_table.set("channel_id", channel_id.to_string())?;
                    result_table.set("fqn", fqn)?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e.to_string())?;
                }
            }

            Ok(result_table)
        })?;
        orcs_table.set("spawn_runner", spawn_runner_fn)?;
    }

    // ── orcs.request_stop() ────────────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let request_stop_fn = lua.create_function(move |lua, ()| {
            let ctx_guard = ctx_clone.lock();

            let result_table = lua.create_table()?;
            match ctx_guard.request_stop() {
                Ok(()) => {
                    result_table.set("ok", true)?;
                }
                Err(e) => {
                    result_table.set("ok", false)?;
                    result_table.set("error", e)?;
                }
            }
            Ok(result_table)
        })?;
        orcs_table.set("request_stop", request_stop_fn)?;
    }

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Build a simple error table: `{ ok=false, error=msg }`.
fn simple_error_table(lua: &Lua, msg: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("ok", false)?;
    t.set("error", msg)?;
    Ok(t)
}

/// Parse a Lua value from `config.globals` into a typed `serde_json::Map`.
///
/// Design decisions (parse, don't validate):
///   1. `Nil` (field absent) → `Ok(None)`.
///   2. Serialization failure (functions, userdata, etc.) → `RuntimeError`
///      so the Lua caller gets clear feedback.
///   3. Non-object JSON values (array, string, …) → `RuntimeError` with
///      the actual type name for diagnostics.
///   4. Valid object table → `Ok(Some(map))`. Downstream functions accept
///      the already-parsed `Map` and never need `unreachable!()` branches.
fn parse_globals_from_lua(
    lua: &mlua::Lua,
    raw: mlua::Value,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, mlua::Error> {
    match raw {
        mlua::Value::Nil => Ok(None),
        other => {
            let val: serde_json::Value = lua.from_value(other).map_err(|e| {
                mlua::Error::RuntimeError(format!(
                    "config.globals must be a serializable table: {e}"
                ))
            })?;
            match val {
                serde_json::Value::Object(map) => Ok(Some(map)),
                other => Err(mlua::Error::RuntimeError(format!(
                    "config.globals must be a table (JSON object), got {}",
                    match other {
                        serde_json::Value::Array(_) => "array",
                        serde_json::Value::String(_) => "string",
                        serde_json::Value::Number(_) => "number",
                        serde_json::Value::Bool(_) => "boolean",
                        serde_json::Value::Null => "null",
                        // Object is already matched above; this arm is
                        // unreachable. serde_json::Value is exhaustive
                        // (not #[non_exhaustive]), so no wildcard needed.
                        serde_json::Value::Object(_) => "object",
                    }
                ))),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_globals_from_lua tests ─────────────────────────────────

    #[test]
    fn parse_globals_nil_returns_none() {
        let lua = mlua::Lua::new();
        let result =
            parse_globals_from_lua(&lua, mlua::Value::Nil).expect("Nil should return Ok(None)");
        assert!(result.is_none());
    }

    #[test]
    fn parse_globals_valid_table_returns_map() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().expect("create table");
        table.set("name", "agent-1").expect("set name");
        table.set("level", 42).expect("set level");

        let result = parse_globals_from_lua(&lua, mlua::Value::Table(table))
            .expect("valid table should succeed");
        let map = result.expect("should be Some");
        assert_eq!(map.get("name").and_then(|v| v.as_str()), Some("agent-1"));
        assert_eq!(map.get("level").and_then(|v| v.as_i64()), Some(42));
    }

    #[test]
    fn parse_globals_nested_table_returns_nested_map() {
        let lua = mlua::Lua::new();
        let inner = lua.create_table().expect("create inner");
        inner.set("model", "gpt-4").expect("set model");

        let outer = lua.create_table().expect("create outer");
        outer.set("config", inner).expect("set config");

        let result = parse_globals_from_lua(&lua, mlua::Value::Table(outer))
            .expect("nested table should succeed");
        let map = result.expect("should be Some");
        let config = map.get("config").expect("config key should exist");
        assert_eq!(config.get("model").and_then(|v| v.as_str()), Some("gpt-4"));
    }

    #[test]
    fn parse_globals_string_returns_error() {
        let lua = mlua::Lua::new();
        let val = mlua::Value::String(lua.create_string("not a table").expect("create string"));

        let err = parse_globals_from_lua(&lua, val).expect_err("string should fail");
        assert!(
            err.to_string().contains("must be a table (JSON object)"),
            "error message should mention type constraint, got: {err}"
        );
        assert!(
            err.to_string().contains("got string"),
            "error message should include actual type, got: {err}"
        );
    }

    #[test]
    fn parse_globals_array_table_returns_error() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().expect("create table");
        table.set(1, "a").expect("set index 1");
        table.set(2, "b").expect("set index 2");
        table.set(3, "c").expect("set index 3");

        let err = parse_globals_from_lua(&lua, mlua::Value::Table(table))
            .expect_err("array table should fail");
        assert!(
            err.to_string().contains("got array"),
            "error message should say 'got array', got: {err}"
        );
    }

    #[test]
    fn parse_globals_table_with_function_returns_error() {
        let lua = mlua::Lua::new();
        let table = lua.create_table().expect("create table");
        let func = lua
            .create_function(|_, ()| Ok(()))
            .expect("create function");
        table.set("callback", func).expect("set function value");

        let err = parse_globals_from_lua(&lua, mlua::Value::Table(table))
            .expect_err("table with function should fail");
        assert!(
            err.to_string().contains("must be a serializable table"),
            "error message should mention serialization, got: {err}"
        );
    }

    #[test]
    fn parse_globals_number_returns_error() {
        let lua = mlua::Lua::new();
        let val = mlua::Value::Number(42.0);

        let err = parse_globals_from_lua(&lua, val).expect_err("number should fail");
        assert!(
            err.to_string().contains("got number"),
            "error message should say 'got number', got: {err}"
        );
    }

    #[test]
    fn parse_globals_boolean_returns_error() {
        let lua = mlua::Lua::new();
        let val = mlua::Value::Boolean(true);

        let err = parse_globals_from_lua(&lua, val).expect_err("boolean should fail");
        assert!(
            err.to_string().contains("got boolean"),
            "error message should say 'got boolean', got: {err}"
        );
    }

    // ── simple_error_table tests ─────────────────────────────────────

    #[test]
    fn simple_error_table_has_correct_fields() {
        let lua = mlua::Lua::new();
        let table =
            simple_error_table(&lua, "spawn failed").expect("simple_error_table should not fail");
        assert_eq!(
            table.get::<bool>("ok").expect("ok field"),
            false,
            "ok should be false"
        );
        assert_eq!(
            table.get::<String>("error").expect("error field"),
            "spawn failed",
        );
        // error_kind should NOT exist
        assert!(
            table.get::<String>("error_kind").is_err(),
            "simple_error_table should not have error_kind"
        );
    }

    #[test]
    fn simple_error_table_produces_valid_table() {
        let lua = mlua::Lua::new();
        let _ = simple_error_table(&lua, "x").expect("should produce valid table");
    }
}

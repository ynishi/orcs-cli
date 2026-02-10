//! Child context Lua function registration.
//!
//! Registers the following functions into the `orcs` Lua table:
//! - `orcs.exec(cmd)` — permission-checked shell execution
//! - `orcs.llm(prompt)` — capability-gated LLM call
//! - `orcs.spawn_child(config)` — spawn a child process
//! - `orcs.check_command(cmd)` — check command permission
//! - `orcs.grant_command(pattern)` — grant a command pattern
//! - `orcs.request_approval(op, desc)` — request HIL approval
//! - `orcs.child_count()` — current child count
//! - `orcs.max_children()` — max allowed children
//! - `orcs.send_to_child(id, msg)` — send message to child
//! - `orcs.spawn_runner(config)` — spawn a ChannelRunner

use crate::error::LuaError;
use crate::types::{lua_to_json, serde_json_to_lua};
use mlua::{Lua, Table};
use orcs_component::{ChildConfig, ChildContext};
use orcs_runtime::sandbox::SandboxPolicy;
use std::sync::{Arc, Mutex};

/// Registers child-context functions into the `orcs` Lua table.
///
/// Also overrides file tools with capability-gated versions via `cap_tools`.
pub(super) fn register(
    lua: &Lua,
    ctx: Arc<Mutex<Box<dyn ChildContext>>>,
    sandbox: Arc<dyn SandboxPolicy>,
) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;
    let sandbox_root = sandbox.root().to_path_buf();

    // ── Override file tools with capability-gated versions ──────────
    // Store context in app_data for shared cap_tools implementation.
    lua.set_app_data(crate::cap_tools::ContextWrapper(Arc::clone(&ctx)));
    crate::cap_tools::register_capability_gated_tools(lua, &orcs_table, &sandbox)?;

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

    // Override orcs.llm with capability-checked version supporting session management.
    // Requires Capability::LLM.
    //
    // orcs.llm(prompt [, opts]) -> { ok, content?, error?, session_id? }
    //   opts.new_session = true  → start tracked session
    //   opts.resume = "<uuid>"   → continue existing session
    {
        let ctx_clone = Arc::clone(&ctx);
        let llm_sandbox_root = sandbox_root.clone();
        let llm_fn = lua.create_function(move |lua, (prompt, opts): (String, Option<Table>)| {
            // Capability check
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

            if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", "permission denied: Capability::LLM not granted")?;
                return Ok(result);
            }
            drop(ctx_guard);

            let mode = crate::llm_command::parse_session_mode(opts.as_ref())?;
            let llm_result = crate::llm_command::execute_llm(&prompt, &mode, &llm_sandbox_root);
            crate::llm_command::result_to_lua_table(lua, &llm_result)
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
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

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
            let ctx_guard = ctx_clone
                .lock()
                .map_err(|e| mlua::Error::RuntimeError(format!("context lock failed: {}", e)))?;

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

//! Child context Lua function registration.
//!
//! Registers the following functions into the `orcs` Lua table:
//! - `orcs.exec(cmd)` — permission-checked shell execution
//! - `orcs.llm(prompt)` — capability-gated LLM call
//! - `orcs.llm_ping([opts])` — capability-gated LLM connectivity check
//! - `orcs.spawn_child(config)` — spawn a child process
//! - `orcs.check_command(cmd)` — check command permission
//! - `orcs.grant_command(pattern)` — grant a command pattern
//! - `orcs.request_approval(op, desc)` — request HIL approval
//! - `orcs.child_count()` — current child count
//! - `orcs.max_children()` — max allowed children
//! - `orcs.send_to_child(id, msg)` — send message to child
//! - `orcs.send_to_children_batch(ids, inputs)` — parallel batch send
//! - `orcs.request_batch(requests)` — parallel RPC batch
//! - `orcs.spawn_runner(config)` — spawn a ChannelRunner

use crate::error::LuaError;
use crate::types::{lua_to_json, serde_json_to_lua};
use mlua::{Lua, LuaSerdeExt, Table};
use orcs_component::{ChildConfig, ChildContext, ComponentError};
use orcs_runtime::sandbox::SandboxPolicy;
use parking_lot::Mutex;
use std::sync::Arc;

/// Registers child-context functions into the `orcs` Lua table.
///
/// Also stores `ContextWrapper` for capability-gated dispatch via `dispatch_rust_tool`.
pub(super) fn register(
    lua: &Lua,
    ctx: Arc<Mutex<Box<dyn ChildContext>>>,
    sandbox: Arc<dyn SandboxPolicy>,
) -> Result<(), LuaError> {
    let orcs_table: Table = lua.globals().get("orcs")?;
    let sandbox_root = sandbox.root().to_path_buf();

    // ── Store ChildContext for capability-gated dispatch ────────────
    // dispatch_rust_tool reads ContextWrapper from app_data for capability checks.
    lua.set_app_data(crate::context_wrapper::ContextWrapper(Arc::clone(&ctx)));

    // Override orcs.exec with permission-checked version
    // This replaces the basic exec from register_orcs_functions
    // Uses check_command_permission() which respects dynamic grants from HIL approval
    let ctx_clone = Arc::clone(&ctx);
    let exec_sandbox_root = sandbox_root.clone();
    let exec_fn = lua.create_function(move |lua, cmd: String| {
        let ctx_guard = ctx_clone.lock();

        // Capability gate: EXECUTE required
        if !ctx_guard.has_capability(orcs_component::Capability::EXECUTE) {
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
            orcs_component::CommandPermission::RequiresApproval {
                grant_pattern,
                description,
            } => {
                let approval_id = format!("ap-{}", uuid::Uuid::new_v4());
                tracing::info!(
                    approval_id = %approval_id,
                    grant_pattern = %grant_pattern,
                    cmd = %cmd,
                    "exec requires approval, suspending"
                );
                return Err(mlua::Error::ExternalError(std::sync::Arc::new(
                    ComponentError::Suspended {
                        approval_id,
                        grant_pattern: grant_pattern.clone(),
                        pending_request: serde_json::json!({
                            "command": cmd,
                            "description": description,
                        }),
                    },
                )));
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

    // orcs.exec_argv(program, args [, opts]) -> {ok, stdout, stderr, code}
    // Shell-free execution: bypasses sh -c entirely.
    // Permission-checked via Capability::EXECUTE + check_command_permission(program).
    {
        let ctx_clone = Arc::clone(&ctx);
        let argv_sandbox_root = sandbox_root.clone();
        let exec_argv_fn = lua.create_function(
            move |lua, (program, args, opts): (String, Table, Option<Table>)| {
                let ctx_guard = ctx_clone.lock();

                // Capability gate: EXECUTE required
                if !ctx_guard.has_capability(orcs_component::Capability::EXECUTE) {
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
                let permission = ctx_guard.check_command_permission(&program);
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
                    orcs_component::CommandPermission::RequiresApproval {
                        grant_pattern,
                        description,
                    } => {
                        let approval_id = format!("ap-{}", uuid::Uuid::new_v4());
                        tracing::info!(
                            approval_id = %approval_id,
                            grant_pattern = %grant_pattern,
                            program = %program,
                            "exec_argv requires approval, suspending"
                        );
                        return Err(mlua::Error::ExternalError(std::sync::Arc::new(
                            ComponentError::Suspended {
                                approval_id,
                                grant_pattern: grant_pattern.clone(),
                                pending_request: serde_json::json!({
                                    "command": program,
                                    "description": description,
                                }),
                            },
                        )));
                    }
                }
                drop(ctx_guard);

                tracing::debug!("Lua exec_argv (authorized): {}", program);

                crate::sanitize::exec_argv_impl(
                    lua,
                    &program,
                    &args,
                    opts.as_ref(),
                    &argv_sandbox_root,
                )
            },
        )?;
        orcs_table.set("exec_argv", exec_argv_fn)?;
    }

    // Override orcs.llm with capability-checked version that delegates to
    // llm_command::llm_request_impl (multi-provider HTTP client).
    // Requires Capability::LLM.
    //
    // orcs.llm(prompt [, opts]) -> { ok, content?, model?, session_id?, error?, error_kind? }
    {
        let ctx_clone = Arc::clone(&ctx);

        let llm_fn = lua.create_function(move |lua, args: (String, Option<Table>)| {
            // Capability check
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", "permission denied: Capability::LLM not granted")?;
                result.set("error_kind", "permission_denied")?;
                return Ok(result);
            }
            drop(ctx_guard);

            crate::llm_command::llm_request_impl(lua, args)
        })?;
        orcs_table.set("llm", llm_fn)?;
    }

    // Override orcs.llm_ping with capability-checked version.
    // Requires Capability::LLM. Lightweight connectivity check (no tokens consumed).
    //
    // orcs.llm_ping([opts]) -> { ok, provider, base_url, latency_ms, status?, error?, error_kind? }
    {
        let ctx_clone = Arc::clone(&ctx);

        let ping_fn = lua.create_function(move |lua, opts: Option<Table>| {
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", "permission denied: Capability::LLM not granted")?;
                result.set("error_kind", "permission_denied")?;
                return Ok(result);
            }
            drop(ctx_guard);

            crate::llm_command::llm_ping_impl(lua, opts)
        })?;
        orcs_table.set("llm_ping", ping_fn)?;
    }

    // Override orcs.http with capability-checked version.
    // Requires Capability::HTTP. Delegates to http_command::http_request_impl.
    //
    // orcs.http(method, url [, opts]) -> { ok, status?, headers?, body?, error?, error_kind? }
    {
        let ctx_clone = Arc::clone(&ctx);

        let http_fn = lua.create_function(move |lua, args: (String, String, Option<Table>)| {
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::HTTP) {
                let result = lua.create_table()?;
                result.set("ok", false)?;
                result.set("error", "permission denied: Capability::HTTP not granted")?;
                result.set("error_kind", "permission_denied")?;
                return Ok(result);
            }
            drop(ctx_guard);

            crate::http_command::http_request_impl(lua, args)
        })?;
        orcs_table.set("http", http_fn)?;
    }

    // orcs.spawn_child(config) -> { ok, id, handle, error }
    // config = { id = "child-id", script = "..." } or { id = "child-id", path = "..." }
    let ctx_clone = Arc::clone(&ctx);
    let spawn_child_fn = lua.create_function(move |lua, config: Table| {
        let ctx_guard = ctx_clone.lock();

        // Capability gate: SPAWN required
        if !ctx_guard.has_capability(orcs_component::Capability::SPAWN) {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set("error", "permission denied: Capability::SPAWN not granted")?;
            return Ok(result);
        }

        // Auth permission check
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
        let ctx_guard = ctx_clone.lock();

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
        let ctx_guard = ctx_clone.lock();

        ctx_guard.grant_command(&pattern);
        tracing::info!("Lua grant_command: {}", pattern);
        Ok(())
    })?;
    orcs_table.set("grant_command", grant_command_fn)?;

    // orcs.request_approval(operation, description) -> approval_id
    let ctx_clone = Arc::clone(&ctx);
    let request_approval_fn =
        lua.create_function(move |_, (operation, description): (String, String)| {
            let ctx_guard = ctx_clone.lock();

            let approval_id = ctx_guard.emit_approval_request(&operation, &description);
            Ok(approval_id)
        })?;
    orcs_table.set("request_approval", request_approval_fn)?;

    // orcs.child_count() -> number
    let ctx_clone = Arc::clone(&ctx);
    let child_count_fn = lua.create_function(move |_, ()| {
        let ctx_guard = ctx_clone.lock();
        Ok(ctx_guard.child_count())
    })?;
    orcs_table.set("child_count", child_count_fn)?;

    // orcs.max_children() -> number
    let ctx_clone = Arc::clone(&ctx);
    let max_children_fn = lua.create_function(move |_, ()| {
        let ctx_guard = ctx_clone.lock();
        Ok(ctx_guard.max_children())
    })?;
    orcs_table.set("max_children", max_children_fn)?;

    // orcs.send_to_child(child_id, message) -> { ok, result, error }
    let ctx_clone = Arc::clone(&ctx);
    let send_to_child_fn =
        lua.create_function(move |lua, (child_id, message): (String, mlua::Value)| {
            let ctx_guard = ctx_clone.lock();

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

    // orcs.send_to_child_async(child_id, message) -> { ok, error? }
    let ctx_clone = Arc::clone(&ctx);
    let send_to_child_async_fn =
        lua.create_function(move |lua, (child_id, message): (String, mlua::Value)| {
            let ctx_guard = ctx_clone.lock();

            let input = lua_to_json(message, lua)?;

            let result_table = lua.create_table()?;
            match ctx_guard.send_to_child_async(&child_id, input) {
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
    let ctx_clone = Arc::clone(&ctx);
    let send_batch_fn =
        lua.create_function(move |lua, (ids, inputs): (mlua::Table, mlua::Table)| {
            let ctx_guard = ctx_clone.lock();

            let ids_len = ids.len()? as usize;
            let inputs_len = inputs.len()? as usize;
            if ids_len != inputs_len {
                return Err(mlua::Error::RuntimeError(format!(
                    "ids length ({}) != inputs length ({})",
                    ids_len, inputs_len
                )));
            }

            let mut requests = Vec::with_capacity(ids_len);
            for i in 1..=ids_len {
                let id: String = ids.get(i)?;
                let input_val: mlua::Value = inputs.get(i)?;
                let json_input = lua_to_json(input_val, lua)?;
                requests.push((id, json_input));
            }

            let results = ctx_guard.send_to_children_batch(requests);
            drop(ctx_guard);

            let results_table = lua.create_table()?;
            for (i, (_id, result)) in results.into_iter().enumerate() {
                let entry = lua.create_table()?;
                match result {
                    Ok(orcs_component::ChildResult::Ok(data)) => {
                        entry.set("ok", true)?;
                        let lua_data = serde_json_to_lua(&data, lua)?;
                        entry.set("result", lua_data)?;
                    }
                    Ok(orcs_component::ChildResult::Err(e)) => {
                        entry.set("ok", false)?;
                        entry.set("error", e.to_string())?;
                    }
                    Ok(orcs_component::ChildResult::Aborted) => {
                        entry.set("ok", false)?;
                        entry.set("error", "child aborted")?;
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

    // orcs.request_batch(requests) -> [ { success, data?, error? }, ... ]
    //
    // requests: Lua table (array) of { target, operation, payload, timeout_ms? }
    //
    // All RPC calls execute concurrently (via ChildContext::request_batch).
    // Results are returned in the same order as the input array.
    let ctx_clone = Arc::clone(&ctx);
    let request_batch_fn = lua.create_function(move |lua, requests: mlua::Table| {
        let ctx_guard = ctx_clone.lock();

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
            let json_payload = lua_to_json(payload_val, lua)?;
            let timeout_ms: Option<u64> = entry.get("timeout_ms").ok();
            batch.push((target, operation, json_payload, timeout_ms));
        }

        let results = ctx_guard.request_batch(batch);
        drop(ctx_guard);

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

    // orcs.spawn_runner(config) -> { ok, channel_id, fqn, error }
    // config = { script = "...", id = "optional-id" }
    // Spawns a Component as a separate ChannelRunner for parallel execution.
    // The returned `fqn` can be used immediately with orcs.request(fqn, ...).
    let ctx_clone = Arc::clone(&ctx);
    let spawn_runner_fn = lua.create_function(move |lua, config: Table| {
        let ctx_guard = ctx_clone.lock();

        // Capability gate: SPAWN required
        if !ctx_guard.has_capability(orcs_component::Capability::SPAWN) {
            let result_table = lua.create_table()?;
            result_table.set("ok", false)?;
            result_table.set("error", "permission denied: Capability::SPAWN not granted")?;
            return Ok(result_table);
        }

        // Auth permission check
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

    tracing::debug!(
        "Registered orcs.spawn_child, child_count, max_children, send_to_child, send_to_children_batch, request_batch, spawn_runner functions"
    );
    Ok(())
}

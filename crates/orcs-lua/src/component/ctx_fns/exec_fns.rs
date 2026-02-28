//! Execution, permission, and capability-gated service function registration.
//!
//! Functions registered:
//! - `orcs.exec(cmd)` — permission-checked shell execution
//! - `orcs.exec_argv(program, args [, opts])` — shell-free execution
//! - `orcs.llm(prompt [, opts])` — capability-gated LLM call
//! - `orcs.llm_ping([opts])` — LLM connectivity check
//! - `orcs.http(method, url [, opts])` — capability-gated HTTP request
//! - `orcs.check_command(cmd)` — check command permission
//! - `orcs.grant_command(pattern)` — grant a command pattern

use mlua::{Lua, Table};
use orcs_component::{ChildContext, ComponentError};
use parking_lot::Mutex;
use std::sync::Arc;

pub(super) fn register(
    lua: &Lua,
    orcs_table: &Table,
    ctx: &Arc<Mutex<Box<dyn ChildContext>>>,
    sandbox_root: &std::path::Path,
) -> Result<(), mlua::Error> {
    // ── orcs.exec(cmd) ───────────────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let exec_sandbox_root = sandbox_root.to_path_buf();
        let exec_fn = lua.create_function(move |lua, cmd: String| {
            let ctx_guard = ctx_clone.lock();

            // Capability gate: EXECUTE required
            if !ctx_guard.has_capability(orcs_component::Capability::EXECUTE) {
                return exec_error_table(lua, "permission denied: Capability::EXECUTE not granted");
            }

            // Permission check via check_command_permission (respects dynamic grants)
            let permission = ctx_guard.check_command_permission(&cmd);
            match &permission {
                orcs_component::CommandPermission::Allowed => {
                    // Proceed to execution
                }
                orcs_component::CommandPermission::Denied(reason) => {
                    return exec_error_table(lua, &format!("permission denied: {}", reason));
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
                    return Err(mlua::Error::ExternalError(Arc::new(
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
                .map_err(|e| mlua::Error::ExternalError(Arc::new(e)))?;

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
    }

    // ── orcs.exec_argv(program, args [, opts]) ───────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let argv_sandbox_root = sandbox_root.to_path_buf();
        let exec_argv_fn = lua.create_function(
            move |lua, (program, args, opts): (String, Table, Option<Table>)| {
                let ctx_guard = ctx_clone.lock();

                // Capability gate: EXECUTE required
                if !ctx_guard.has_capability(orcs_component::Capability::EXECUTE) {
                    return exec_error_table(
                        lua,
                        "permission denied: Capability::EXECUTE not granted",
                    );
                }

                // Permission check on program name
                let permission = ctx_guard.check_command_permission(&program);
                match &permission {
                    orcs_component::CommandPermission::Allowed => {}
                    orcs_component::CommandPermission::Denied(reason) => {
                        return exec_error_table(lua, &format!("permission denied: {}", reason));
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
                        return Err(mlua::Error::ExternalError(Arc::new(
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

    // ── orcs.llm(prompt [, opts]) ────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let llm_fn = lua.create_function(move |lua, args: (String, Option<Table>)| {
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                return capability_error_table(
                    lua,
                    "permission denied: Capability::LLM not granted",
                );
            }
            drop(ctx_guard);

            crate::llm_command::llm_request_impl(lua, args)
        })?;
        orcs_table.set("llm", llm_fn)?;
    }

    // ── orcs.llm_ping([opts]) ────────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let ping_fn = lua.create_function(move |lua, opts: Option<Table>| {
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::LLM) {
                return capability_error_table(
                    lua,
                    "permission denied: Capability::LLM not granted",
                );
            }
            drop(ctx_guard);

            crate::llm_command::llm_ping_impl(lua, opts)
        })?;
        orcs_table.set("llm_ping", ping_fn)?;
    }

    // ── orcs.http(method, url [, opts]) ──────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let http_fn = lua.create_function(move |lua, args: (String, String, Option<Table>)| {
            let ctx_guard = ctx_clone.lock();

            if !ctx_guard.has_capability(orcs_component::Capability::HTTP) {
                return capability_error_table(
                    lua,
                    "permission denied: Capability::HTTP not granted",
                );
            }
            drop(ctx_guard);

            crate::http_command::http_request_impl(lua, args)
        })?;
        orcs_table.set("http", http_fn)?;
    }

    // ── orcs.check_command(cmd) ──────────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
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
    }

    // ── orcs.grant_command(pattern) ──────────────────────────────────
    {
        let ctx_clone = Arc::clone(ctx);
        let grant_command_fn = lua.create_function(move |_, pattern: String| {
            let ctx_guard = ctx_clone.lock();

            ctx_guard.grant_command(&pattern);
            tracing::info!("Lua grant_command: {}", pattern);
            Ok(())
        })?;
        orcs_table.set("grant_command", grant_command_fn)?;
    }

    Ok(())
}

// ── Capability-gate error table helpers ──────────────────────────────

/// Build an exec-style error table: `{ ok=false, stdout="", stderr=msg, code=-1 }`.
fn exec_error_table(lua: &Lua, stderr_msg: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("ok", false)?;
    t.set("stdout", "")?;
    t.set("stderr", stderr_msg)?;
    t.set("code", -1)?;
    Ok(t)
}

/// Build a capability error table: `{ ok=false, error=msg, error_kind="permission_denied" }`.
fn capability_error_table(lua: &Lua, msg: &str) -> mlua::Result<Table> {
    let t = lua.create_table()?;
    t.set("ok", false)?;
    t.set("error", msg)?;
    t.set("error_kind", "permission_denied")?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_error_table_has_correct_fields() {
        let lua = mlua::Lua::new();
        let table = exec_error_table(&lua, "some error").expect("exec_error_table should not fail");
        assert_eq!(
            table.get::<bool>("ok").expect("ok field"),
            false,
            "ok should be false"
        );
        assert_eq!(
            table.get::<String>("stdout").expect("stdout field"),
            "",
            "stdout should be empty"
        );
        assert_eq!(
            table.get::<String>("stderr").expect("stderr field"),
            "some error",
            "stderr should contain the message"
        );
        assert_eq!(
            table.get::<i32>("code").expect("code field"),
            -1,
            "code should be -1"
        );
    }

    #[test]
    fn capability_error_table_has_correct_fields() {
        let lua = mlua::Lua::new();
        let table = capability_error_table(&lua, "permission denied: Capability::LLM not granted")
            .expect("capability_error_table should not fail");
        assert_eq!(
            table.get::<bool>("ok").expect("ok field"),
            false,
            "ok should be false"
        );
        assert_eq!(
            table.get::<String>("error").expect("error field"),
            "permission denied: Capability::LLM not granted",
        );
        assert_eq!(
            table.get::<String>("error_kind").expect("error_kind field"),
            "permission_denied",
        );
    }

    #[test]
    fn error_helpers_produce_valid_tables() {
        let lua = mlua::Lua::new();
        let _ = exec_error_table(&lua, "x").expect("should produce valid table");
        let _ = capability_error_table(&lua, "x").expect("should produce valid table");
    }
}

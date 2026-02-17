//! Input sanitization and structured execution for Lua.
//!
//! Provides Rust-implemented sanitization primitives exposed to Lua,
//! powered by the [`shell_sanitize`] crate. Lua script authors use
//! these to validate arguments before passing them to execution APIs.
//!
//! # Lua API
//!
//! | Function | Preset | Use case |
//! |----------|--------|----------|
//! | `orcs.sanitize_arg(s)` | `command_arg` | model name, session_id, flags |
//! | `orcs.sanitize_path(s)` | `file_path` | relative paths within sandbox |
//! | `orcs.sanitize_strict(s)` | `strict` | values reaching a shell |
//!
//! Each returns `{ok, value, error, violations}`.
//!
//! # Structured Execution
//!
//! | Function | Description |
//! |----------|-------------|
//! | `orcs.exec_argv(program, args [, opts])` | Shell-free execution via `Command::new` |
//!
//! `exec_argv` bypasses the shell entirely. Arguments are passed directly
//! to the OS exec layer, making shell injection structurally impossible.
//!
//! # Example
//!
//! ```lua
//! local check = orcs.sanitize_arg(opts.model)
//! if not check.ok then return { ok = false, error = check.error } end
//!
//! local result = orcs.exec_argv("claude", {
//!     "-p", "--output-format", "json",
//!     "--model", check.value,
//!     prompt,
//! }, { env_remove = {"CLAUDECODE"} })
//! ```

use mlua::{Lua, Table};
use shell_sanitize_rules::presets;

/// Registers sanitization functions into the `orcs` Lua table.
///
/// Adds:
/// - `orcs.sanitize_arg(s)` — command_arg preset (ControlChar only)
/// - `orcs.sanitize_path(s)` — file_path preset (PathTraversal + ControlChar)
/// - `orcs.sanitize_strict(s)` — strict preset (all 5 rules)
///
/// These are always available (no Capability gate — they are pure validation).
pub fn register_sanitize_functions(lua: &Lua, orcs_table: &Table) -> Result<(), mlua::Error> {
    // orcs.sanitize_arg(s) -> {ok, value, error, violations}
    let sanitize_arg_fn = lua.create_function(|lua, input: String| {
        let sanitizer = presets::command_arg();
        sanitize_to_lua(lua, &sanitizer, &input)
    })?;
    orcs_table.set("sanitize_arg", sanitize_arg_fn)?;

    // orcs.sanitize_path(s) -> {ok, value, error, violations}
    let sanitize_path_fn = lua.create_function(|lua, input: String| {
        let sanitizer = presets::file_path();
        sanitize_to_lua(lua, &sanitizer, &input)
    })?;
    orcs_table.set("sanitize_path", sanitize_path_fn)?;

    // orcs.sanitize_strict(s) -> {ok, value, error, violations}
    let sanitize_strict_fn = lua.create_function(|lua, input: String| {
        let sanitizer = presets::strict();
        sanitize_to_lua(lua, &sanitizer, &input)
    })?;
    orcs_table.set("sanitize_strict", sanitize_strict_fn)?;

    Ok(())
}

/// Runs a sanitizer and converts the result to a Lua table.
///
/// On success: `{ok=true, value="sanitized_string"}`
/// On failure: `{ok=false, error="human-readable", violations={...}}`
fn sanitize_to_lua<T: shell_sanitize::MarkerType>(
    lua: &Lua,
    sanitizer: &shell_sanitize::Sanitizer<T>,
    input: &str,
) -> Result<Table, mlua::Error> {
    let result = lua.create_table()?;

    match sanitizer.sanitize(input) {
        Ok(sanitized) => {
            result.set("ok", true)?;
            result.set("value", sanitized.as_str())?;
        }
        Err(err) => {
            result.set("ok", false)?;
            result.set("error", err.to_string())?;

            // Structured violations array for programmatic access
            let violations_table = lua.create_table()?;
            for (i, v) in err.violations.iter().enumerate() {
                let entry = lua.create_table()?;
                entry.set("rule", v.rule_name)?;
                entry.set("message", v.message.as_str())?;
                if let Some(pos) = v.position {
                    entry.set("position", pos)?;
                }
                if let Some(ref frag) = v.fragment {
                    entry.set("fragment", frag.as_str())?;
                }
                violations_table.set(i + 1, entry)?; // Lua 1-indexed
            }
            result.set("violations", violations_table)?;
        }
    }

    Ok(result)
}

/// Registers `orcs.exec_argv` (base version, deny-by-default).
///
/// This stub always denies execution. The permission-checked override
/// is registered by `register_exec_argv_with_context` when a ChildContext
/// is available.
pub fn register_exec_argv_deny(lua: &Lua, orcs_table: &Table) -> Result<(), mlua::Error> {
    let exec_argv_fn = lua.create_function(|lua, (_program, _args): (String, Table)| {
        let result = lua.create_table()?;
        result.set("ok", false)?;
        result.set("stdout", "")?;
        result.set(
            "stderr",
            "exec_argv denied: no execution context (ChildContext required)",
        )?;
        result.set("code", -1)?;
        Ok(result)
    })?;
    orcs_table.set("exec_argv", exec_argv_fn)?;
    Ok(())
}

/// Builds and executes a `Command` from program + args + opts.
///
/// Shared implementation used by both `child.rs` and `ctx_fns.rs`
/// after capability/permission checks have passed.
///
/// # Arguments
///
/// * `program` - Executable name or path
/// * `args` - Lua table (array) of string arguments
/// * `opts` - Optional Lua table: `{ env_remove = {"VAR1", ...}, cwd = "path" }`
/// * `default_cwd` - Fallback working directory (sandbox root)
pub fn exec_argv_impl(
    lua: &Lua,
    program: &str,
    args: &Table,
    opts: Option<&Table>,
    default_cwd: &std::path::Path,
) -> Result<Table, mlua::Error> {
    // Collect args from Lua table
    let mut arg_vec: Vec<String> = Vec::new();
    let len = args.len()? as usize;
    for i in 1..=len {
        let arg: String = args.get(i)?;
        arg_vec.push(arg);
    }

    let mut cmd = std::process::Command::new(program);
    cmd.args(&arg_vec);

    // Handle opts.env_remove
    if let Some(opts_table) = opts {
        if let Ok(env_remove) = opts_table.get::<Table>("env_remove") {
            let env_len = env_remove.len()? as usize;
            for i in 1..=env_len {
                if let Ok(var) = env_remove.get::<String>(i) {
                    cmd.env_remove(&var);
                }
            }
        }

        // Handle opts.cwd
        if let Ok(cwd) = opts_table.get::<String>("cwd") {
            cmd.current_dir(&cwd);
        } else {
            cmd.current_dir(default_cwd);
        }
    } else {
        cmd.current_dir(default_cwd);
    }

    let output = cmd
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orcs_helpers::ensure_orcs_table;

    fn setup_lua() -> (Lua, Table) {
        let lua = Lua::new();
        let orcs = ensure_orcs_table(&lua).expect("create orcs table");
        register_sanitize_functions(&lua, &orcs).expect("register sanitize functions");
        register_exec_argv_deny(&lua, &orcs).expect("register exec_argv deny");
        (lua, orcs)
    }

    // ── sanitize_arg tests ──

    #[test]
    fn sanitize_arg_accepts_clean_string() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_arg("claude-haiku-4-5-20251001")"#)
            .eval()
            .expect("eval sanitize_arg");
        assert!(result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("value").expect("get value"),
            "claude-haiku-4-5-20251001"
        );
    }

    #[test]
    fn sanitize_arg_rejects_null_byte() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_arg("model\0injected")"#)
            .eval()
            .expect("eval sanitize_arg");
        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error = result.get::<String>("error").expect("get error");
        assert!(
            error.contains("violation"),
            "error should mention violation, got: {}",
            error
        );
    }

    #[test]
    fn sanitize_arg_rejects_control_char() {
        let (lua, _) = setup_lua();
        // \x01 = SOH control character
        let result: Table = lua
            .load(r#"return orcs.sanitize_arg("bad" .. string.char(1) .. "input")"#)
            .eval()
            .expect("eval sanitize_arg");
        assert!(!result.get::<bool>("ok").expect("get ok"));

        // Check violations array
        let violations: Table = result.get("violations").expect("get violations");
        let first: Table = violations.get(1).expect("get first violation");
        let rule = first.get::<String>("rule").expect("get rule name");
        assert_eq!(rule, "control_char");
    }

    #[test]
    fn sanitize_arg_allows_normal_punctuation() {
        let (lua, _) = setup_lua();
        // command_arg preset only checks control chars — shell metacharacters are fine
        // because Command::new().arg() doesn't go through a shell
        let result: Table = lua
            .load(r#"return orcs.sanitize_arg("hello world $VAR 'quoted'")"#)
            .eval()
            .expect("eval sanitize_arg");
        assert!(
            result.get::<bool>("ok").expect("get ok"),
            "command_arg should allow shell metacharacters"
        );
    }

    // ── sanitize_path tests ──

    #[test]
    fn sanitize_path_accepts_relative_path() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_path("src/lib.rs")"#)
            .eval()
            .expect("eval sanitize_path");
        assert!(result.get::<bool>("ok").expect("get ok"));
        assert_eq!(
            result.get::<String>("value").expect("get value"),
            "src/lib.rs"
        );
    }

    #[test]
    fn sanitize_path_rejects_traversal() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_path("../../etc/passwd")"#)
            .eval()
            .expect("eval sanitize_path");
        assert!(!result.get::<bool>("ok").expect("get ok"));
        let error = result.get::<String>("error").expect("get error");
        assert!(
            error.contains("violation"),
            "should reject path traversal, got: {}",
            error
        );
    }

    #[test]
    fn sanitize_path_rejects_absolute_path() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_path("/etc/shadow")"#)
            .eval()
            .expect("eval sanitize_path");
        assert!(!result.get::<bool>("ok").expect("get ok"));
    }

    // ── sanitize_strict tests ──

    #[test]
    fn sanitize_strict_accepts_clean_string() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_strict("safe-filename.txt")"#)
            .eval()
            .expect("eval sanitize_strict");
        assert!(result.get::<bool>("ok").expect("get ok"));
    }

    #[test]
    fn sanitize_strict_rejects_shell_metachar() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_strict("file; rm -rf /")"#)
            .eval()
            .expect("eval sanitize_strict");
        assert!(!result.get::<bool>("ok").expect("get ok"));
    }

    #[test]
    fn sanitize_strict_rejects_env_expansion() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.sanitize_strict("$HOME/.ssh/id_rsa")"#)
            .eval()
            .expect("eval sanitize_strict");
        assert!(!result.get::<bool>("ok").expect("get ok"));
    }

    // ── exec_argv deny-by-default tests ──

    #[test]
    fn exec_argv_denied_without_context() {
        let (lua, _) = setup_lua();
        let result: Table = lua
            .load(r#"return orcs.exec_argv("echo", {"hello"})"#)
            .eval()
            .expect("eval exec_argv");
        assert!(!result.get::<bool>("ok").expect("get ok"));
        let stderr = result.get::<String>("stderr").expect("get stderr");
        assert!(
            stderr.contains("exec_argv denied"),
            "should be denied, got: {}",
            stderr
        );
    }

    // ── exec_argv_impl tests ──

    #[test]
    fn exec_argv_impl_runs_command() {
        let lua = Lua::new();
        let args = lua.create_table().expect("create args table");
        args.set(1, "hello from exec_argv").expect("set arg 1");

        let result = exec_argv_impl(&lua, "echo", &args, None, std::path::Path::new("."))
            .expect("exec_argv_impl should succeed");

        assert!(result.get::<bool>("ok").expect("get ok"));
        let stdout = result.get::<String>("stdout").expect("get stdout");
        assert!(
            stdout.contains("hello from exec_argv"),
            "stdout should contain output, got: {}",
            stdout
        );
    }

    #[test]
    fn exec_argv_impl_with_env_remove() {
        let lua = Lua::new();
        let args = lua.create_table().expect("create args table");
        args.set(1, "-c").expect("set arg");
        args.set(2, "echo ${TEST_SANITIZE_VAR:-unset}")
            .expect("set arg 2");

        let opts = lua.create_table().expect("create opts table");
        let env_remove = lua.create_table().expect("create env_remove");
        env_remove.set(1, "TEST_SANITIZE_VAR").expect("set env var");
        opts.set("env_remove", env_remove).expect("set env_remove");

        // Set the env var first so we can verify it gets removed
        std::env::set_var("TEST_SANITIZE_VAR", "should_be_removed");

        let result = exec_argv_impl(&lua, "sh", &args, Some(&opts), std::path::Path::new("."))
            .expect("exec_argv_impl should succeed");

        // Clean up
        std::env::remove_var("TEST_SANITIZE_VAR");

        assert!(result.get::<bool>("ok").expect("get ok"));
        let stdout = result.get::<String>("stdout").expect("get stdout");
        assert!(
            stdout.contains("unset"),
            "env var should have been removed, got: {}",
            stdout
        );
    }

    #[test]
    fn exec_argv_impl_nonexistent_program() {
        let lua = Lua::new();
        let args = lua.create_table().expect("create args table");

        let result = exec_argv_impl(
            &lua,
            "nonexistent_program_xyz_12345",
            &args,
            None,
            std::path::Path::new("."),
        );

        assert!(result.is_err(), "should fail for nonexistent program");
    }

    #[test]
    fn exec_argv_impl_no_shell_injection() {
        let lua = Lua::new();
        let args = lua.create_table().expect("create args table");
        // This would be dangerous with sh -c, but exec_argv passes it as a literal arg
        args.set(1, "hello; echo INJECTED").expect("set arg");

        let result = exec_argv_impl(&lua, "echo", &args, None, std::path::Path::new("."))
            .expect("exec_argv_impl should succeed");

        let stdout = result.get::<String>("stdout").expect("get stdout");
        // The semicolon should appear literally in output, not execute a second command
        assert!(
            stdout.contains("hello; echo INJECTED"),
            "should treat semicolon as literal, got: {}",
            stdout
        );
    }
}

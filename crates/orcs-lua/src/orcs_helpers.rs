//! Common orcs helper functions for Lua.
//!
//! Provides base orcs functions that are shared between LuaComponent and LuaChild.
//! All functions are registered under the `orcs` global table in Lua.
//!
//! # Available Functions
//!
//! | Function | Description |
//! |----------|-------------|
//! | `orcs.log(level, msg)` | Log a message at specified level |
//! | `orcs.exec(cmd)` | Execute shell command (cwd = sandbox root) |
//! | `orcs.pwd` | Sandbox root path as string |
//! | `orcs.read(path)` | Read file contents (native Rust) |
//! | `orcs.write(path, content)` | Write file contents (atomic, native Rust) |
//! | `orcs.grep(pattern, path)` | Search with regex (native Rust) |
//! | `orcs.glob(pattern, dir?)` | Find files by pattern (native Rust) |
//! | `orcs.mkdir(path)` | Create directory with parents (native Rust) |
//! | `orcs.remove(path)` | Remove file or directory (native Rust) |
//! | `orcs.mv(src, dst)` | Move / rename (native Rust) |
//!
//! # Globals Sandboxing
//!
//! After registration, `sandbox_lua_globals` is called to disable
//! dangerous Lua stdlib functions (`io.*`, `os.execute`, `loadfile`, etc.)
//! that would bypass the sandbox.
//!
//! # Usage
//!
//! ```ignore
//! use orcs_lua::orcs_helpers::register_base_orcs_functions;
//! use orcs_runtime::sandbox::ProjectSandbox;
//! use std::sync::Arc;
//!
//! let lua = Lua::new();
//! let sandbox = Arc::new(ProjectSandbox::new(".").unwrap());
//! register_base_orcs_functions(&lua, sandbox)?;
//!
//! // Now Lua scripts can use:
//! // orcs.log("info", "Hello")
//! // orcs.exec("ls -la")
//! // orcs.pwd  -- sandbox root path
//! ```

use crate::error::LuaError;
use mlua::{Lua, LuaSerdeExt, Table};
use orcs_runtime::sandbox::SandboxPolicy;
use std::process::Command;
use std::sync::Arc;

/// Global table name for orcs functions in Lua.
const ORCS_TABLE_NAME: &str = "orcs";

/// Registers base orcs helper functions in Lua.
///
/// Creates the `orcs` global table if it doesn't exist and adds:
/// - `orcs.log(level, msg)` - Log a message at specified level
/// - `orcs.exec(cmd)` - Execute shell command (cwd = sandbox root)
/// - `orcs.pwd` - Sandbox root path as string
///
/// Then delegates to [`register_tool_functions`](crate::tools::register_tool_functions)
/// for native file operations (`read`, `write`, `grep`, `glob`, `mkdir`, `remove`, `mv`)
/// and calls `sandbox_lua_globals` to disable dangerous Lua stdlib functions.
///
/// # Arguments
///
/// * `lua` - The Lua runtime
/// * `sandbox` - Sandbox policy controlling file access and exec cwd
///
/// # Errors
///
/// Returns error if function registration fails.
pub fn register_base_orcs_functions(
    lua: &Lua,
    sandbox: Arc<dyn SandboxPolicy>,
) -> Result<(), LuaError> {
    // Get or create orcs table
    let orcs_table = ensure_orcs_table(lua)?;

    // Store sandbox in app_data for RustTool dispatch (dispatch_rust_tool).
    lua.set_app_data(Arc::clone(&sandbox));

    // Register orcs.llm() deny stub (overridden by ctx_fns/child with Capability::LLM).
    crate::llm_command::register_llm_deny_stub(lua, &orcs_table)?;

    // Register orcs.http() deny stub (overridden by ctx_fns/child with Capability::HTTP).
    crate::http_command::register_http_deny_stub(lua, &orcs_table)?;

    // Register sanitization primitives (always available, no Capability gate).
    crate::sanitize::register_sanitize_functions(lua, &orcs_table)?;
    crate::sanitize::register_exec_argv_deny(lua, &orcs_table)?;

    // orcs.log(level, msg) - Log a message
    if orcs_table.get::<mlua::Function>("log").is_err() {
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
    }

    // orcs.exec(cmd) -> {ok=false, ...}
    // Deny by default: exec requires ChildContext with permission checking.
    // Overridden by permission-checked version when ChildContext is set.
    if orcs_table.get::<mlua::Function>("exec").is_err() {
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
    }

    // orcs.pwd - sandbox root as string (always available)
    orcs_table.set("pwd", sandbox.root().display().to_string())?;

    // orcs.tool_descriptions() -> string
    // Returns formatted tool reference for prompt embedding.
    {
        let tool_desc = lua.create_function(|_, ()| Ok(TOOL_DESCRIPTIONS))?;
        orcs_table.set("tool_descriptions", tool_desc)?;
    }

    // orcs.json_parse(str) -> value
    // Parses a JSON string into a Lua value (table/string/number/boolean/nil).
    {
        let json_parse = lua.create_function(|lua, s: String| {
            let value: serde_json::Value = serde_json::from_str(&s)
                .map_err(|e| mlua::Error::RuntimeError(format!("json parse error: {e}")))?;
            lua.to_value(&value)
        })?;
        orcs_table.set("json_parse", json_parse)?;
    }

    // orcs.json_encode(value) -> string
    // Encodes a Lua value (table/string/number/boolean/nil) into a JSON string.
    {
        let json_encode = lua.create_function(|lua, value: mlua::Value| {
            let json_value: serde_json::Value = lua.from_value(value)?;
            serde_json::to_string(&json_value)
                .map_err(|e| mlua::Error::RuntimeError(format!("json encode error: {e}")))
        })?;
        orcs_table.set("json_encode", json_encode)?;
    }

    // orcs.toml_parse(str) -> value
    // Parses a TOML string into a Lua value (table).
    {
        let toml_parse = lua.create_function(|lua, s: String| {
            let value: toml::Value = toml::from_str(&s)
                .map_err(|e| mlua::Error::RuntimeError(format!("toml parse error: {e}")))?;
            toml_value_to_lua(lua, &value)
        })?;
        orcs_table.set("toml_parse", toml_parse)?;
    }

    // orcs.toml_encode(value) -> string
    // Encodes a Lua table into a TOML string.
    {
        let toml_encode = lua.create_function(|lua, value: mlua::Value| {
            let toml_value = lua_to_toml_value(lua, &value)?;
            toml::to_string_pretty(&toml_value)
                .map_err(|e| mlua::Error::RuntimeError(format!("toml encode error: {e}")))
        })?;
        orcs_table.set("toml_encode", toml_encode)?;
    }

    // orcs.git_info() -> {ok, branch, commit_short, dirty}
    // Runs git commands directly from Rust (no sandbox exec, no permission needed).
    {
        let sandbox_root = sandbox.root().to_path_buf();
        let git_info_fn = lua.create_function(move |lua, ()| {
            let result = lua.create_table()?;

            // Check if inside a git repo
            let repo_check = Command::new("git")
                .args(["rev-parse", "--is-inside-work-tree"])
                .current_dir(&sandbox_root)
                .output();

            let is_repo = repo_check
                .as_ref()
                .map(|o| o.status.success())
                .unwrap_or(false);

            if !is_repo {
                result.set("ok", false)?;
                return Ok(result);
            }

            result.set("ok", true)?;

            // Branch name
            if let Ok(output) = Command::new("git")
                .args(["rev-parse", "--abbrev-ref", "HEAD"])
                .current_dir(&sandbox_root)
                .output()
            {
                if output.status.success() {
                    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    result.set("branch", branch)?;
                }
            }

            // Short commit hash
            if let Ok(output) = Command::new("git")
                .args(["rev-parse", "--short", "HEAD"])
                .current_dir(&sandbox_root)
                .output()
            {
                if output.status.success() {
                    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    result.set("commit_short", commit)?;
                }
            }

            // Dirty status (any uncommitted changes)
            if let Ok(output) = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&sandbox_root)
                .output()
            {
                if output.status.success() {
                    let dirty = !output.stdout.is_empty();
                    result.set("dirty", dirty)?;
                }
            }

            Ok(result)
        })?;
        orcs_table.set("git_info", git_info_fn)?;
    }

    // Register native Rust tools (read, write, grep, glob, mkdir, remove, mv)
    crate::tools::register_tool_functions(lua, Arc::clone(&sandbox))?;

    // Register dispatch and tool_schemas
    crate::tool_registry::register_dispatch_functions(lua)?;

    // Register orcs.resolve_loop (provider-agnostic tool resolution loop)
    crate::resolve_loop::register_resolve_loop(lua, &orcs_table)?;

    // Disable dangerous Lua stdlib functions
    sandbox_lua_globals(lua)?;

    Ok(())
}

/// Tool descriptions for prompt embedding.
///
/// Kept in sync with the actual registered tools in [`register_tool_functions`](crate::tools::register_tool_functions).
const TOOL_DESCRIPTIONS: &str = r#"Available tools (call via orcs.*):

orcs.read(path) -> {ok, content, size, error}
  Read file contents. path is relative to project root.

orcs.write(path, content) -> {ok, bytes_written, error}
  Write file contents (atomic). Creates parent dirs.

orcs.grep(pattern, path) -> {ok, matches[], count, error}
  Search with regex. matches[i] = {line_number, line}.
  path can be file or directory (recursive).

orcs.glob(pattern, dir?) -> {ok, files[], count, error}
  Find files by glob pattern. dir defaults to project root.

orcs.mkdir(path) -> {ok, error}
  Create directory (with parents).

orcs.remove(path) -> {ok, error}
  Remove file or directory.

orcs.mv(src, dst) -> {ok, error}
  Move / rename file or directory.

orcs.exec(cmd) -> {ok, stdout, stderr, code}
  Execute shell command via sh -c. cwd = project root.

orcs.exec_argv(program, args [, opts]) -> {ok, stdout, stderr, code}
  Execute command without shell. args is an array of strings.
  opts.env_remove = {"VAR1", ...} to remove env vars.
  opts.cwd = "path" to override working directory.

orcs.sanitize_arg(s) -> {ok, value, error, violations}
  Validate argument (rejects control chars, NUL).

orcs.sanitize_path(s) -> {ok, value, error, violations}
  Validate relative path (rejects traversal, absolute paths, control chars).

orcs.sanitize_strict(s) -> {ok, value, error, violations}
  Validate with all rules (shell meta, env expansion, glob, traversal, control).

orcs.http(method, url [, opts]) -> {ok, status, headers, body, error, error_kind}
  HTTP request via reqwest. method = "GET"/"POST"/"PUT"/"DELETE"/"PATCH"/"HEAD".
  opts.headers = {name=value, ...} request headers.
  opts.body = "..." request body string.
  opts.timeout = 30 timeout in seconds (default 30).
  error_kind = "timeout"/"dns"/"connection_refused"/"tls"/"too_large"/"network"/"permission_denied".

orcs.pwd
  Project root path (string).

orcs.git_info() -> {ok, branch, commit_short, dirty}
  Git repository info. ok=false if not in a git repo.

"#;

/// Disables dangerous Lua standard library functions.
///
/// Removes filesystem, process, and introspection access that would bypass the sandbox:
/// - `io.*` — entire module (use `orcs.read`/`orcs.write` instead)
/// - `os.execute`, `os.remove`, `os.rename`, `os.exit`, `os.tmpname`
/// - `loadfile`, `dofile` — arbitrary file loading
/// - `load` — dynamic code generation from strings
/// - `debug` — introspection (stack frames, registry, upvalue manipulation)
/// - `require`, `package` — C module loading and arbitrary file require
///
/// Preserves safe `os` functions: `os.time`, `os.clock`, `os.date`, `os.difftime`.
pub(crate) fn sandbox_lua_globals(lua: &Lua) -> Result<(), LuaError> {
    lua.load(
        r#"
        -- Remove entire io module (orcs.read/write replaces it)
        io = nil

        -- Remove dangerous os functions, keep time-related ones
        if os then
            os.execute = nil
            os.remove = nil
            os.rename = nil
            os.exit = nil
            os.tmpname = nil
        end

        -- Remove arbitrary file/code loading
        loadfile = nil
        dofile = nil
        load = nil

        -- Remove introspection (can bypass sandbox via registry/upvalue access)
        debug = nil

        -- Remove module loading (can load C modules or arbitrary files)
        require = nil
        package = nil
        "#,
    )
    .exec()
    .map_err(LuaError::Runtime)?;

    Ok(())
}

/// Ensures the orcs table exists in Lua globals.
///
/// Creates an empty orcs table if it doesn't exist.
/// Use this before registering context-specific functions.
///
/// # Arguments
///
/// * `lua` - The Lua runtime
///
/// # Returns
///
/// The orcs table (existing or newly created).
///
/// # Errors
///
/// Returns error if table creation or global registration fails.
pub fn ensure_orcs_table(lua: &Lua) -> Result<Table, LuaError> {
    match lua.globals().get::<Table>(ORCS_TABLE_NAME) {
        Ok(table) => Ok(table),
        Err(_) => {
            let table = lua.create_table()?;
            lua.globals().set(ORCS_TABLE_NAME, table.clone())?;
            Ok(table)
        }
    }
}

/// Converts a TOML value to a Lua value.
fn toml_value_to_lua(lua: &Lua, value: &toml::Value) -> mlua::Result<mlua::Value> {
    match value {
        toml::Value::String(s) => Ok(mlua::Value::String(lua.create_string(s)?)),
        toml::Value::Integer(n) => Ok(mlua::Value::Integer(*n)),
        toml::Value::Float(f) => Ok(mlua::Value::Number(*f)),
        toml::Value::Boolean(b) => Ok(mlua::Value::Boolean(*b)),
        toml::Value::Datetime(dt) => Ok(mlua::Value::String(lua.create_string(dt.to_string())?)),
        toml::Value::Array(arr) => {
            let table = lua.create_table()?;
            for (i, v) in arr.iter().enumerate() {
                table.set(i + 1, toml_value_to_lua(lua, v)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
        toml::Value::Table(map) => {
            let table = lua.create_table()?;
            for (k, v) in map {
                table.set(k.as_str(), toml_value_to_lua(lua, v)?)?;
            }
            Ok(mlua::Value::Table(table))
        }
    }
}

/// Converts a Lua value to a TOML value.
fn lua_to_toml_value(_lua: &Lua, value: &mlua::Value) -> mlua::Result<toml::Value> {
    match value {
        mlua::Value::Boolean(b) => Ok(toml::Value::Boolean(*b)),
        mlua::Value::Integer(n) => Ok(toml::Value::Integer(*n)),
        mlua::Value::Number(f) => Ok(toml::Value::Float(*f)),
        mlua::Value::String(s) => Ok(toml::Value::String(s.to_str()?.to_string())),
        mlua::Value::Table(t) => {
            // Single-pass: collect pairs and detect array simultaneously.
            // Array = all keys are consecutive integers starting from 1.
            let mut int_entries: Vec<(i64, mlua::Value)> = Vec::new();
            let mut is_array = true;
            let mut expected_idx: i64 = 1;

            for pair in t.clone().pairs::<mlua::Value, mlua::Value>() {
                let (k, v) = pair?;
                if is_array {
                    if let mlua::Value::Integer(i) = k {
                        if i == expected_idx {
                            int_entries.push((i, v));
                            expected_idx += 1;
                            continue;
                        }
                    }
                    // Not a consecutive integer key — fall back to table
                    is_array = false;
                }
            }

            if is_array && !int_entries.is_empty() {
                let mut arr = Vec::with_capacity(int_entries.len());
                for (_, v) in int_entries {
                    arr.push(lua_to_toml_value(_lua, &v)?);
                }
                Ok(toml::Value::Array(arr))
            } else {
                let mut map = toml::map::Map::new();
                for pair in t.clone().pairs::<String, mlua::Value>() {
                    let (k, v) = pair?;
                    if matches!(v, mlua::Value::Nil) {
                        continue; // skip nil values
                    }
                    map.insert(k, lua_to_toml_value(_lua, &v)?);
                }
                Ok(toml::Value::Table(map))
            }
        }
        mlua::Value::Nil => Err(mlua::Error::RuntimeError(
            "cannot convert nil to TOML value".to_string(),
        )),
        _ => Err(mlua::Error::RuntimeError(format!(
            "cannot convert {:?} to TOML",
            value
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn orcs_runtime::sandbox::SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    #[test]
    fn register_base_functions() {
        let lua = Lua::new();
        let result = register_base_orcs_functions(&lua, test_sandbox());
        assert!(result.is_ok());

        // Verify orcs table exists
        let orcs: Table = lua.globals().get(ORCS_TABLE_NAME).expect("orcs table");

        // Verify functions exist
        assert!(orcs.get::<mlua::Function>("log").is_ok());
        assert!(orcs.get::<mlua::Function>("exec").is_ok());
    }

    #[test]
    fn register_is_idempotent() {
        let lua = Lua::new();
        let sb = test_sandbox();

        // Register twice
        register_base_orcs_functions(&lua, Arc::clone(&sb))
            .expect("should register base functions first time");
        register_base_orcs_functions(&lua, sb)
            .expect("should register base functions second time (idempotent)");

        // Should still work
        let orcs: Table = lua.globals().get(ORCS_TABLE_NAME).expect("orcs table");
        assert!(orcs.get::<mlua::Function>("log").is_ok());
    }

    #[test]
    fn log_function_works() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for log test");

        // Should not panic
        lua.load(r#"orcs.log("info", "test message")"#)
            .exec()
            .expect("log should work");
    }

    #[test]
    fn exec_denied_without_context() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for exec denied test");

        let result: mlua::Table = lua
            .load(r#"return orcs.exec("echo hello")"#)
            .eval()
            .expect("exec should return deny table");

        assert!(!result
            .get::<bool>("ok")
            .expect("should have ok field in exec result"));
        let stderr = result
            .get::<String>("stderr")
            .expect("should have stderr field in exec result");
        assert!(
            stderr.contains("exec denied"),
            "expected 'exec denied', got: {stderr}"
        );
        assert_eq!(
            result
                .get::<i64>("code")
                .expect("should have code field in exec result"),
            -1
        );
    }

    #[test]
    fn ensure_orcs_table_creates_if_missing() {
        let lua = Lua::new();

        // orcs doesn't exist yet
        assert!(lua.globals().get::<Table>(ORCS_TABLE_NAME).is_err());

        // ensure creates it
        let table = ensure_orcs_table(&lua).expect("should create orcs table when missing");
        assert!(table.is_empty());

        // Now it exists
        assert!(lua.globals().get::<Table>(ORCS_TABLE_NAME).is_ok());
    }

    #[test]
    fn json_parse_object() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for json_parse object test");

        let result: Table = lua
            .load(r#"return orcs.json_parse('{"name":"test","count":42,"active":true}')"#)
            .eval()
            .expect("should parse json object");
        assert_eq!(
            result
                .get::<String>("name")
                .expect("should have name field"),
            "test"
        );
        assert_eq!(
            result.get::<i64>("count").expect("should have count field"),
            42
        );
        assert!(result
            .get::<bool>("active")
            .expect("should have active field"));
    }

    #[test]
    fn json_parse_array() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for json_parse array test");

        let result: Table = lua
            .load(r#"return orcs.json_parse('[1,2,3]')"#)
            .eval()
            .expect("should parse json array");
        assert_eq!(result.get::<i64>(1).expect("should have index 1"), 1);
        assert_eq!(result.get::<i64>(2).expect("should have index 2"), 2);
        assert_eq!(result.get::<i64>(3).expect("should have index 3"), 3);
    }

    #[test]
    fn json_parse_invalid_returns_error() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for json_parse invalid test");

        let result = lua
            .load(r#"return orcs.json_parse('not json')"#)
            .eval::<mlua::Value>();
        assert!(result.is_err());
    }

    #[test]
    fn json_encode_table() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for json_encode test");

        let result: String = lua
            .load(r#"return orcs.json_encode({name="test", count=42})"#)
            .eval()
            .expect("should encode lua table to json string");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should parse json_encode output");
        assert_eq!(parsed["name"], "test");
        assert_eq!(parsed["count"], 42);
    }

    #[test]
    fn json_roundtrip() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for json roundtrip test");

        let result: String = lua
            .load(
                r#"
                local obj = orcs.json_parse('{"tool":"read","args":{"path":"src/main.rs"}}')
                return orcs.json_encode(obj)
                "#,
            )
            .eval()
            .expect("should complete json roundtrip");
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("should parse json roundtrip output");
        assert_eq!(parsed["tool"], "read");
        assert_eq!(parsed["args"]["path"], "src/main.rs");
    }

    #[test]
    fn ensure_orcs_table_returns_existing() {
        let lua = Lua::new();

        // Create orcs table with a value
        lua.load(r#"orcs = { test = 123 }"#)
            .exec()
            .expect("should create orcs table with test field");

        // ensure returns existing
        let table = ensure_orcs_table(&lua).expect("should return existing orcs table");
        assert_eq!(
            table
                .get::<i32>("test")
                .expect("should have test field in existing orcs table"),
            123
        );
    }

    #[test]
    fn toml_parse_table() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for toml_parse table test");

        let result: Table = lua
            .load(
                r#"return orcs.toml_parse([[
[profile]
name = "test"
description = "a test profile"

[config]
debug = true
]])"#,
            )
            .eval()
            .expect("should parse toml table");

        let profile: Table = result.get("profile").expect("should have profile section");
        assert_eq!(
            profile
                .get::<String>("name")
                .expect("should have name in profile"),
            "test"
        );
        assert_eq!(
            profile
                .get::<String>("description")
                .expect("should have description in profile"),
            "a test profile"
        );

        let config: Table = result.get("config").expect("should have config section");
        assert!(config
            .get::<bool>("debug")
            .expect("should have debug in config"));
    }

    #[test]
    fn toml_parse_array() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for toml_parse array test");

        let result: Table = lua
            .load(
                r#"return orcs.toml_parse([[
[components.skill_manager]
activate = ["rust-dev", "git-workflow"]
]])"#,
            )
            .eval()
            .expect("should parse toml with array");

        let components: Table = result
            .get("components")
            .expect("should have components section");
        let sm: Table = components
            .get("skill_manager")
            .expect("should have skill_manager in components");
        let activate: Table = sm.get("activate").expect("should have activate array");
        assert_eq!(
            activate
                .get::<String>(1)
                .expect("should have first element in activate"),
            "rust-dev"
        );
        assert_eq!(
            activate
                .get::<String>(2)
                .expect("should have second element in activate"),
            "git-workflow"
        );
    }

    #[test]
    fn toml_parse_invalid_returns_error() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for toml_parse invalid test");

        let result = lua
            .load(r#"return orcs.toml_parse('not valid toml {{{{')"#)
            .eval::<mlua::Value>();
        assert!(result.is_err());
    }

    #[test]
    fn toml_encode_table() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for toml_encode test");

        let result: String = lua
            .load(r#"return orcs.toml_encode({name = "test", count = 42})"#)
            .eval()
            .expect("should encode lua table to toml string");
        assert!(result.contains("name"));
        assert!(result.contains("test"));
    }

    #[test]
    fn git_info_returns_table() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox()).expect("register should succeed");

        let result: Table = lua
            .load("return orcs.git_info()")
            .eval()
            .expect("git_info should return a table");

        // We're running inside the orcs-cli git repo, so ok should be true
        let ok = result.get::<bool>("ok").expect("should have ok field");
        assert!(ok, "expected ok=true since we're in a git repo");

        let branch = result
            .get::<String>("branch")
            .expect("should have branch field");
        assert!(!branch.is_empty(), "branch should not be empty");

        let commit = result
            .get::<String>("commit_short")
            .expect("should have commit_short field");
        assert!(!commit.is_empty(), "commit_short should not be empty");

        // dirty is a bool, just verify it exists
        let _dirty = result
            .get::<bool>("dirty")
            .expect("should have dirty field");
    }

    #[test]
    fn toml_roundtrip() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox())
            .expect("should register base functions for toml roundtrip test");

        let result: String = lua
            .load(
                r#"
                local obj = orcs.toml_parse([[
name = "roundtrip"
value = 123
]])
                return orcs.toml_encode(obj)
                "#,
            )
            .eval()
            .expect("should complete toml roundtrip");
        assert!(result.contains("roundtrip"));
        assert!(result.contains("123"));
    }
}

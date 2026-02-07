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
use mlua::{Lua, Table};
use orcs_runtime::sandbox::SandboxPolicy;
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

    // orcs.llm(prompt) -> {ok=false, error="..."}
    // Deny by default: llm requires ChildContext with Capability::LLM.
    // Overridden by capability-checked version when ChildContext is set.
    if orcs_table.get::<mlua::Function>("llm").is_err() {
        let llm_fn = lua.create_function(|lua, _prompt: String| {
            let result = lua.create_table()?;
            result.set("ok", false)?;
            result.set(
                "error",
                "llm denied: no execution context (ChildContext with Capability::LLM required)",
            )?;
            Ok(result)
        })?;
        orcs_table.set("llm", llm_fn)?;
    }

    // orcs.tool_descriptions() -> string
    // Returns formatted tool reference for prompt embedding.
    {
        let tool_desc = lua.create_function(|_, ()| Ok(TOOL_DESCRIPTIONS))?;
        orcs_table.set("tool_descriptions", tool_desc)?;
    }

    // Register native Rust tools (read, write, grep, glob, mkdir, remove, mv)
    crate::tools::register_tool_functions(lua, sandbox)?;

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
  Execute shell command. cwd = project root.

orcs.pwd
  Project root path (string).
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
        register_base_orcs_functions(&lua, Arc::clone(&sb)).unwrap();
        register_base_orcs_functions(&lua, sb).unwrap();

        // Should still work
        let orcs: Table = lua.globals().get(ORCS_TABLE_NAME).expect("orcs table");
        assert!(orcs.get::<mlua::Function>("log").is_ok());
    }

    #[test]
    fn log_function_works() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox()).unwrap();

        // Should not panic
        lua.load(r#"orcs.log("info", "test message")"#)
            .exec()
            .expect("log should work");
    }

    #[test]
    fn exec_denied_without_context() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua, test_sandbox()).unwrap();

        let result: mlua::Table = lua
            .load(r#"return orcs.exec("echo hello")"#)
            .eval()
            .expect("exec should return deny table");

        assert!(!result.get::<bool>("ok").unwrap());
        let stderr = result.get::<String>("stderr").unwrap();
        assert!(
            stderr.contains("exec denied"),
            "expected 'exec denied', got: {stderr}"
        );
        assert_eq!(result.get::<i64>("code").unwrap(), -1);
    }

    #[test]
    fn ensure_orcs_table_creates_if_missing() {
        let lua = Lua::new();

        // orcs doesn't exist yet
        assert!(lua.globals().get::<Table>(ORCS_TABLE_NAME).is_err());

        // ensure creates it
        let table = ensure_orcs_table(&lua).unwrap();
        assert!(table.is_empty());

        // Now it exists
        assert!(lua.globals().get::<Table>(ORCS_TABLE_NAME).is_ok());
    }

    #[test]
    fn ensure_orcs_table_returns_existing() {
        let lua = Lua::new();

        // Create orcs table with a value
        lua.load(r#"orcs = { test = 123 }"#).exec().unwrap();

        // ensure returns existing
        let table = ensure_orcs_table(&lua).unwrap();
        assert_eq!(table.get::<i32>("test").unwrap(), 123);
    }
}

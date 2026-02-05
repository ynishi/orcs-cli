//! Common orcs helper functions for Lua.
//!
//! Provides base orcs functions that are shared between LuaComponent and LuaChild.
//!
//! # Available Functions
//!
//! | Function | Description |
//! |----------|-------------|
//! | `orcs.log(level, msg)` | Log a message at specified level |
//! | `orcs.exec(cmd)` | Execute shell command and return output |
//! | `orcs.read(path)` | Read file contents (native Rust) |
//! | `orcs.write(path, content)` | Write file contents (atomic, native Rust) |
//! | `orcs.grep(pattern, path)` | Search with regex (native Rust) |
//! | `orcs.glob(pattern, dir?)` | Find files by pattern (native Rust) |
//!
//! # Usage
//!
//! ```ignore
//! use orcs_lua::orcs_helpers::register_base_orcs_functions;
//!
//! let lua = Lua::new();
//! register_base_orcs_functions(&lua)?;
//!
//! // Now Lua scripts can use:
//! // orcs.log("info", "Hello")
//! // orcs.exec("ls -la")
//! ```

use crate::error::LuaError;
use mlua::{Lua, Table};
use tokio::process::Command;

/// Global table name for orcs functions in Lua.
const ORCS_TABLE_NAME: &str = "orcs";

/// Registers base orcs helper functions in Lua.
///
/// Creates the `orcs` global table if it doesn't exist and adds:
/// - `orcs.log(level, msg)` - Log a message at specified level
/// - `orcs.exec(cmd)` - Execute shell command and return output
///
/// # Arguments
///
/// * `lua` - The Lua runtime
///
/// # Errors
///
/// Returns error if function registration fails.
pub fn register_base_orcs_functions(lua: &Lua) -> Result<(), LuaError> {
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

    // orcs.exec(cmd) -> {ok, stdout, stderr, code}
    if orcs_table.get::<mlua::Function>("exec").is_err() {
        let exec_fn = lua.create_function(|lua, cmd: String| {
            tracing::debug!("Lua exec: {}", cmd);

            // Try to get current tokio runtime handle for async execution.
            let output = match tokio::runtime::Handle::try_current() {
                Ok(handle) => {
                    handle.block_on(async { Command::new("sh").arg("-c").arg(&cmd).output().await })
                }
                Err(_) => {
                    tracing::warn!("No tokio runtime available, using sync execution");
                    std::process::Command::new("sh")
                        .arg("-c")
                        .arg(&cmd)
                        .output()
                }
            }
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
            result.set("code", output.status.code().unwrap_or(-1))?;

            Ok(result)
        })?;
        orcs_table.set("exec", exec_fn)?;
    }

    // Register native Rust tools (read, write, grep, glob)
    crate::tools::register_tool_functions(lua)?;

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

    #[test]
    fn register_base_functions() {
        let lua = Lua::new();
        let result = register_base_orcs_functions(&lua);
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

        // Register twice
        register_base_orcs_functions(&lua).unwrap();
        register_base_orcs_functions(&lua).unwrap();

        // Should still work
        let orcs: Table = lua.globals().get(ORCS_TABLE_NAME).expect("orcs table");
        assert!(orcs.get::<mlua::Function>("log").is_ok());
    }

    #[test]
    fn log_function_works() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua).unwrap();

        // Should not panic
        lua.load(r#"orcs.log("info", "test message")"#)
            .exec()
            .expect("log should work");
    }

    #[test]
    fn exec_function_works() {
        let lua = Lua::new();
        register_base_orcs_functions(&lua).unwrap();

        let result: mlua::Table = lua
            .load(r#"return orcs.exec("echo hello")"#)
            .eval()
            .expect("exec should work");

        assert!(result.get::<bool>("ok").unwrap());
        assert!(result.get::<String>("stdout").unwrap().contains("hello"));
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

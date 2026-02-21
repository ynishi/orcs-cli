//! Unified Lua environment setup.
//!
//! Provides a single entry point for creating sandboxed Lua VMs with:
//! - `orcs.*` helper functions (log, exec, read, write, grep, glob, etc.)
//! - Custom `require()` with sandbox-aware search paths
//! - Dangerous Lua stdlib functions disabled (io.*, os.execute, etc.)
//!
//! # Search Order for `require()`
//!
//! ```text
//! require("lib.helper")
//!   1. Filesystem: {search_path}/lib/helper.lua      (sandbox-validated)
//!   2. Filesystem: {search_path}/lib/helper/init.lua  (sandbox-validated)
//!   → error if not found
//! ```
//!
//! # Example
//!
//! ```ignore
//! use orcs_lua::LuaEnv;
//!
//! let env = LuaEnv::new(sandbox)
//!     .with_search_path("/project/.orcs/components/my-comp");
//!
//! let lua = env.create_lua()?;
//! // Lua scripts can now use:
//! //   require("helper")       -- from component dir
//! ```

use crate::error::LuaError;
use mlua::{Function, Lua, Table};
use orcs_runtime::sandbox::SandboxPolicy;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Unified Lua environment configuration.
///
/// Creates Lua VMs with consistent sandbox, `orcs.*` functions,
/// and a unified `require()` that resolves modules from
/// configured search paths (filesystem, sandbox-validated).
#[derive(Debug, Clone)]
pub struct LuaEnv {
    /// Sandbox policy for file operations and require() path validation.
    sandbox: Arc<dyn SandboxPolicy>,
    /// Search paths for `require()` resolution (priority order).
    search_paths: Vec<PathBuf>,
}

impl LuaEnv {
    /// Creates a new LuaEnv with the given sandbox policy.
    #[must_use]
    pub fn new(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self {
            sandbox,
            search_paths: Vec::new(),
        }
    }

    /// Adds a search path for `require()` resolution.
    ///
    /// Paths are searched in the order they are added.
    /// Each path is validated against the sandbox at require-time.
    #[must_use]
    pub fn with_search_path(mut self, path: impl AsRef<Path>) -> Self {
        self.search_paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Adds multiple search paths.
    #[must_use]
    pub fn with_search_paths(mut self, paths: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        for p in paths {
            self.search_paths.push(p.as_ref().to_path_buf());
        }
        self
    }

    /// Returns configured search paths.
    #[must_use]
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.search_paths
    }

    /// Creates a new sandboxed Lua VM.
    ///
    /// The returned Lua VM has:
    /// - `orcs.*` functions registered (log, exec, read, write, grep, glob, etc.)
    /// - Dangerous Lua stdlib functions disabled
    /// - Custom `require()` with sandbox-aware module resolution
    /// - `package.loaded` cache for loaded modules
    ///
    /// # Errors
    ///
    /// Returns error if VM setup fails.
    pub fn create_lua(&self) -> Result<Lua, LuaError> {
        let lua = Lua::new();

        // Save require/package BEFORE sandbox kills them.
        let saved_require: Option<Function> = lua.globals().get("require").ok();
        let saved_package: Option<Table> = lua.globals().get("package").ok();

        // Register all orcs.* functions + sandbox globals.
        // This calls sandbox_lua_globals() which sets require=nil, package=nil.
        crate::orcs_helpers::register_base_orcs_functions(&lua, Arc::clone(&self.sandbox))?;

        // Install our custom require() with sandboxed searchers.
        self.setup_require(&lua, saved_require, saved_package)?;

        Ok(lua)
    }

    /// Sets up a sandboxed `require()` function.
    ///
    /// Replaces the standard Lua `require` with a custom implementation that:
    /// 1. Checks `package.loaded` cache
    /// 2. Searches filesystem paths (sandbox-validated)
    /// 3. Returns error if not found
    ///
    /// `package.path` and `package.cpath` are set to empty strings
    /// to prevent any default search behavior.
    fn setup_require(
        &self,
        lua: &Lua,
        saved_require: Option<Function>,
        saved_package: Option<Table>,
    ) -> Result<(), LuaError> {
        // If Lua didn't have require/package (shouldn't happen), nothing to set up.
        let Some(_original_require) = saved_require else {
            tracing::warn!("Lua VM missing require function, skipping require setup");
            return Ok(());
        };
        let Some(package) = saved_package else {
            tracing::warn!("Lua VM missing package table, skipping require setup");
            return Ok(());
        };

        // Disable default search paths entirely.
        package
            .set("path", "")
            .map_err(|e| LuaError::InvalidScript(format!("set package.path: {e}")))?;
        package
            .set("cpath", "")
            .map_err(|e| LuaError::InvalidScript(format!("set package.cpath: {e}")))?;

        // Preserve existing package.loaded (or create fresh).
        let loaded: Table = match package.get("loaded") {
            Ok(t) => t,
            Err(_) => lua
                .create_table()
                .map_err(|e| LuaError::InvalidScript(format!("create package.loaded: {e}")))?,
        };
        package
            .set("loaded", loaded)
            .map_err(|e| LuaError::InvalidScript(format!("set package.loaded: {e}")))?;

        // Clear searchers (we use our own require implementation).
        let empty_searchers = lua
            .create_table()
            .map_err(|e| LuaError::InvalidScript(format!("create searchers: {e}")))?;
        package
            .set("searchers", empty_searchers)
            .map_err(|e| LuaError::InvalidScript(format!("set package.searchers: {e}")))?;

        // Restore package table (sanitized).
        lua.globals()
            .set("package", package)
            .map_err(|e| LuaError::InvalidScript(format!("restore package: {e}")))?;

        // Create our custom require() function.
        let search_paths = self.search_paths.clone();

        let custom_require = lua.create_function(move |lua, name: String| {
            // 1. Check package.loaded cache
            let package: Table = lua
                .globals()
                .get("package")
                .map_err(|e| mlua::Error::RuntimeError(format!("package table missing: {e}")))?;
            let loaded: Table = package
                .get("loaded")
                .map_err(|e| mlua::Error::RuntimeError(format!("package.loaded missing: {e}")))?;

            if let Ok(cached) = loaded.get::<mlua::Value>(name.as_str()) {
                if cached != mlua::Value::Nil {
                    return Ok(cached);
                }
            }

            // 2. Try filesystem search paths
            let module_rel = name.replace('.', "/");
            for base in &search_paths {
                // Try {base}/{module}.lua
                let file_path = base.join(format!("{module_rel}.lua"));
                if let Some(source) = try_read_within_base(&file_path, base) {
                    let result = eval_module(lua, &source, &name, &file_path)?;
                    loaded.set(name.as_str(), result.clone())?;
                    return Ok(result);
                }

                // Try {base}/{module}/init.lua
                let init_path = base.join(&module_rel).join("init.lua");
                if let Some(source) = try_read_within_base(&init_path, base) {
                    let result = eval_module(lua, &source, &name, &init_path)?;
                    loaded.set(name.as_str(), result.clone())?;
                    return Ok(result);
                }
            }

            // 3. Not found
            let searched: Vec<_> = search_paths
                .iter()
                .flat_map(|base| {
                    [
                        format!("{}/{module_rel}.lua", base.display()),
                        format!("{}/{module_rel}/init.lua", base.display()),
                    ]
                })
                .collect();

            Err(mlua::Error::RuntimeError(format!(
                "module '{}' not found (searched: {})",
                name,
                searched.join(", ")
            )))
        })?;

        lua.globals()
            .set("require", custom_require)
            .map_err(|e| LuaError::InvalidScript(format!("set require: {e}")))?;

        Ok(())
    }
}

/// Tries to read a file, validating it stays within the base directory.
///
/// Search paths are set by Rust code (not Lua), so they are trusted.
/// We validate that the resolved path doesn't escape the base directory
/// via symlinks or `..` components (path traversal prevention).
fn try_read_within_base(path: &Path, base: &Path) -> Option<String> {
    let canonical = path.canonicalize().ok()?;
    let base_canonical = base.canonicalize().ok()?;
    if !canonical.starts_with(&base_canonical) {
        return None;
    }
    if canonical.is_file() {
        std::fs::read_to_string(&canonical).ok()
    } else {
        None
    }
}

/// Evaluates a Lua module source and returns its result.
fn eval_module(
    lua: &Lua,
    source: &str,
    name: &str,
    path: &Path,
) -> Result<mlua::Value, mlua::Error> {
    lua.load(source)
        .set_name(format!("{name} ({path})", path = path.display()))
        .eval()
        .map_err(|e| {
            mlua::Error::RuntimeError(format!(
                "error loading module '{name}' from {}: {e}",
                path.display()
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    // ─── Basic VM creation ───────────────────────────────────────────

    #[test]
    fn create_lua_returns_working_vm() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua should succeed");

        // orcs.* should be available
        let result: String = lua
            .load(r#"return type(orcs.log)"#)
            .eval()
            .expect("orcs.log should exist");
        assert_eq!(result, "function");
    }

    #[test]
    fn create_lua_has_require() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(require)"#)
            .eval()
            .expect("require should exist");
        assert_eq!(result, "function");
    }

    #[test]
    fn create_lua_has_package() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(package)"#)
            .eval()
            .expect("package should exist");
        assert_eq!(result, "table");
    }

    // ─── Sandbox: dangerous globals removed ──────────────────────────

    #[test]
    fn sandbox_io_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua.load(r#"return type(io)"#).eval().expect("eval");
        assert_eq!(result, "nil");
    }

    #[test]
    fn sandbox_loadfile_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua.load(r#"return type(loadfile)"#).eval().expect("eval");
        assert_eq!(result, "nil");
    }

    #[test]
    fn sandbox_debug_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua.load(r#"return type(debug)"#).eval().expect("eval");
        assert_eq!(result, "nil");
    }

    #[test]
    fn sandbox_cpath_empty() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua.load(r#"return package.cpath"#).eval().expect("eval");
        assert_eq!(result, "");
    }

    #[test]
    fn sandbox_path_empty() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua.load(r#"return package.path"#).eval().expect("eval");
        assert_eq!(result, "");
    }

    #[test]
    fn sandbox_os_execute_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(os.execute)"#)
            .eval()
            .expect("eval os.execute type");
        assert_eq!(result, "nil", "os.execute must be removed");
    }

    #[test]
    fn sandbox_os_remove_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(os.remove)"#)
            .eval()
            .expect("eval os.remove type");
        assert_eq!(result, "nil", "os.remove must be removed");
    }

    #[test]
    fn sandbox_dofile_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(dofile)"#)
            .eval()
            .expect("eval dofile type");
        assert_eq!(result, "nil", "dofile must be removed");
    }

    #[test]
    fn sandbox_load_removed() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(load)"#)
            .eval()
            .expect("eval load type");
        assert_eq!(result, "nil", "load must be removed");
    }

    #[test]
    fn sandbox_os_safe_functions_preserved() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return type(os.time)"#)
            .eval()
            .expect("eval os.time type");
        assert_eq!(result, "function", "os.time must be preserved");

        let result: String = lua
            .load(r#"return type(os.clock)"#)
            .eval()
            .expect("eval os.clock type");
        assert_eq!(result, "function", "os.clock must be preserved");
    }

    // ─── require() error handling ──────────────────────────────────

    #[test]
    fn require_nonexistent_errors() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result = lua.load(r#"require("nonexistent_module_xyz")"#).exec();
        assert!(result.is_err());
        let err_str = result.unwrap_err().to_string();
        assert!(
            err_str.contains("not found"),
            "error should say 'not found', got: {err_str}"
        );
    }

    // ─── require() with filesystem ───────────────────────────────────

    #[test]
    fn require_filesystem_module() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let lib_dir = dir.path().join("lib");
        std::fs::create_dir_all(&lib_dir).expect("create lib dir");
        std::fs::write(
            lib_dir.join("helper.lua"),
            r#"
            local M = {}
            function M.greet() return "hello from helper" end
            return M
            "#,
        )
        .expect("write helper.lua");

        let sandbox = Arc::new(ProjectSandbox::new(dir.path()).expect("sandbox for tempdir"));
        let env = LuaEnv::new(sandbox).with_search_path(dir.path());

        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(
                r#"
                local helper = require("lib.helper")
                return helper.greet()
                "#,
            )
            .eval()
            .expect("require filesystem module");
        assert_eq!(result, "hello from helper");
    }

    #[test]
    fn require_filesystem_init_lua() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let mod_dir = dir.path().join("mymod");
        std::fs::create_dir_all(&mod_dir).expect("create mymod dir");
        std::fs::write(mod_dir.join("init.lua"), r#"return { name = "mymod" }"#)
            .expect("write init.lua");

        let sandbox = Arc::new(ProjectSandbox::new(dir.path()).expect("sandbox"));
        let env = LuaEnv::new(sandbox).with_search_path(dir.path());

        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(
                r#"
                local m = require("mymod")
                return m.name
                "#,
            )
            .eval()
            .expect("require init.lua");
        assert_eq!(result, "mymod");
    }

    #[test]
    fn require_filesystem_is_cached() {
        let dir = tempfile::tempdir().expect("create temp dir");
        std::fs::write(
            dir.path().join("counter.lua"),
            r#"
            _counter = (_counter or 0) + 1
            return { count = _counter }
            "#,
        )
        .expect("write counter.lua");

        let sandbox = Arc::new(ProjectSandbox::new(dir.path()).expect("sandbox"));
        let env = LuaEnv::new(sandbox).with_search_path(dir.path());

        let lua = env.create_lua().expect("create_lua");

        let result: i64 = lua
            .load(
                r#"
                local a = require("counter")
                local b = require("counter")
                -- If cached, count should be 1 (loaded once)
                -- If not cached, count would be 2
                return a.count
                "#,
            )
            .eval()
            .expect("require should cache");
        assert_eq!(result, 1, "module should be loaded only once");
    }

    // ─── Search path priority ────────────────────────────────────────

    #[test]
    fn search_path_priority_first_wins() {
        let dir1 = tempfile::tempdir().expect("create temp dir 1");
        let dir2 = tempfile::tempdir().expect("create temp dir 2");

        std::fs::write(
            dir1.path().join("shared.lua"),
            r#"return { source = "dir1" }"#,
        )
        .expect("write shared.lua to dir1");
        std::fs::write(
            dir2.path().join("shared.lua"),
            r#"return { source = "dir2" }"#,
        )
        .expect("write shared.lua to dir2");

        // dir1 has higher priority (added first)
        let sandbox = Arc::new(ProjectSandbox::new(dir1.path()).expect("sandbox"));
        let env = LuaEnv::new(sandbox)
            .with_search_path(dir1.path())
            .with_search_path(dir2.path());

        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(
                r#"
                local m = require("shared")
                return m.source
                "#,
            )
            .eval()
            .expect("require shared");
        assert_eq!(result, "dir1", "first search path should win");
    }

    #[test]
    fn require_filesystem_module_loads() {
        let dir = tempfile::tempdir().expect("create temp dir");
        std::fs::write(
            dir.path().join("my_module.lua"),
            r#"return { source = "filesystem" }"#,
        )
        .expect("write module");

        let sandbox = Arc::new(ProjectSandbox::new(dir.path()).expect("sandbox"));
        let env = LuaEnv::new(sandbox).with_search_path(dir.path());

        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(
                r#"
                local m = require("my_module")
                return m.source
                "#,
            )
            .eval()
            .expect("require should load from filesystem");
        assert_eq!(result, "filesystem");
    }

    // ─── Error messages ──────────────────────────────────────────────

    #[test]
    fn require_error_lists_searched_paths() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let sandbox = Arc::new(ProjectSandbox::new(dir.path()).expect("sandbox"));
        let env = LuaEnv::new(sandbox).with_search_path(dir.path());

        let lua = env.create_lua().expect("create_lua");

        let result = lua.load(r#"require("missing")"#).exec();
        let err_str = result.unwrap_err().to_string();
        assert!(err_str.contains("missing"), "should contain module name");
        assert!(
            err_str.contains("not found"),
            "should say not found: {err_str}"
        );
    }

    // ─── Builder API ─────────────────────────────────────────────────

    #[test]
    fn with_search_paths_batch() {
        let env = LuaEnv::new(test_sandbox()).with_search_paths(["/a", "/b", "/c"]);
        assert_eq!(env.search_paths().len(), 3);
    }

    #[test]
    fn search_paths_empty_by_default() {
        let env = LuaEnv::new(test_sandbox());
        assert!(env.search_paths().is_empty());
    }

    // ─── orcs.* functions still work ─────────────────────────────────

    #[test]
    fn orcs_log_works() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        lua.load(r#"orcs.log("info", "hello from LuaEnv")"#)
            .exec()
            .expect("orcs.log should work");
    }

    #[test]
    fn orcs_pwd_works() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: String = lua
            .load(r#"return orcs.pwd"#)
            .eval()
            .expect("orcs.pwd should work");
        assert!(!result.is_empty(), "pwd should not be empty");
    }

    #[test]
    fn orcs_json_parse_works() {
        let env = LuaEnv::new(test_sandbox());
        let lua = env.create_lua().expect("create_lua");

        let result: i64 = lua
            .load(r#"return orcs.json_parse('{"x":42}').x"#)
            .eval()
            .expect("json_parse should work");
        assert_eq!(result, 42);
    }
}

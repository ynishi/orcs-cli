//! Lua script loader with filesystem-based search paths.
//!
//! # Loading Priority
//!
//! Search paths in order (first match wins):
//! - `{path}/{name}.lua` (single file)
//! - `{path}/{name}/init.lua` (directory component)
//!
//! # Example
//!
//! ```ignore
//! use orcs_lua::ScriptLoader;
//!
//! let loader = ScriptLoader::new(sandbox)
//!     .with_path("~/.orcs/components")
//!     .with_path("/versioned/builtins/components");
//!
//! let component = loader.load("echo")?;
//! ```

use crate::component::LuaComponent;
use crate::error::LuaError;
use orcs_runtime::sandbox::SandboxPolicy;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Result of batch loading operation.
///
/// Contains both successfully loaded components and warnings for failures.
/// This allows the application to continue even when some scripts fail to load.
#[derive(Default)]
pub struct LoadResult {
    /// Successfully loaded components with their names.
    pub loaded: Vec<(String, LuaComponent)>,
    /// Warnings for scripts that failed to load.
    pub warnings: Vec<LoadWarning>,
}

impl std::fmt::Debug for LoadResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadResult")
            .field("loaded_count", &self.loaded.len())
            .field("warnings", &self.warnings)
            .finish()
    }
}

impl LoadResult {
    /// Returns true if any scripts were loaded.
    #[must_use]
    pub fn has_loaded(&self) -> bool {
        !self.loaded.is_empty()
    }

    /// Returns true if any warnings occurred.
    #[must_use]
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }

    /// Returns the count of loaded scripts.
    #[must_use]
    pub fn loaded_count(&self) -> usize {
        self.loaded.len()
    }

    /// Returns the count of warnings.
    #[must_use]
    pub fn warning_count(&self) -> usize {
        self.warnings.len()
    }
}

/// Warning for a failed script load.
///
/// Contains the path that was attempted and the error that occurred.
#[derive(Debug)]
pub struct LoadWarning {
    /// Path to the script that failed to load.
    pub path: PathBuf,
    /// Error that occurred during loading.
    pub error: LuaError,
}

impl std::fmt::Display for LoadWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.error)
    }
}

/// Script loader with configurable search paths.
///
/// Search order: configured search paths in order added.
#[derive(Debug, Clone)]
pub struct ScriptLoader {
    /// Search paths for scripts (priority order).
    search_paths: Vec<PathBuf>,
    /// Sandbox policy for file operations in loaded components.
    sandbox: Arc<dyn SandboxPolicy>,
}

impl ScriptLoader {
    /// Creates a new loader.
    ///
    /// The sandbox is passed to all components loaded by this loader.
    #[must_use]
    pub fn new(sandbox: Arc<dyn SandboxPolicy>) -> Self {
        Self {
            search_paths: Vec::new(),
            sandbox,
        }
    }

    /// Adds a search path.
    ///
    /// Paths are searched in the order they are added.
    #[must_use]
    pub fn with_path(mut self, path: impl AsRef<Path>) -> Self {
        self.search_paths.push(path.as_ref().to_path_buf());
        self
    }

    /// Adds project root's `scripts/` directory to search paths.
    ///
    /// Convenience method for `with_path(root.join("scripts"))`.
    #[must_use]
    pub fn with_project_root(mut self, root: impl AsRef<Path>) -> Self {
        self.search_paths.push(root.as_ref().join("scripts"));
        self
    }

    /// Adds multiple search paths.
    #[must_use]
    pub fn with_paths(mut self, paths: impl IntoIterator<Item = impl AsRef<Path>>) -> Self {
        for path in paths {
            self.search_paths.push(path.as_ref().to_path_buf());
        }
        self
    }

    /// Returns configured search paths.
    #[must_use]
    pub fn search_paths(&self) -> &[PathBuf] {
        &self.search_paths
    }

    /// Loads a script by name.
    ///
    /// Search order:
    /// 1. Each search path: `{path}/{name}.lua` (single file)
    /// 2. Each search path: `{path}/{name}/init.lua` (directory with co-located modules)
    ///
    /// Directory-based loading sets up `package.path` so that standard
    /// `require()` resolves co-located modules within the component directory.
    ///
    /// # Errors
    ///
    /// Returns error if script not found in any location.
    pub fn load(&self, name: &str) -> Result<LuaComponent, LuaError> {
        for path in &self.search_paths {
            // 1. Single file: {path}/{name}.lua
            let file_path = path.join(format!("{name}.lua"));
            if file_path.exists() {
                return LuaComponent::from_file(&file_path, Arc::clone(&self.sandbox));
            }

            // 2. Directory: {path}/{name}/init.lua
            let dir_path = path.join(name);
            if dir_path.join("init.lua").exists() {
                return LuaComponent::from_dir(&dir_path, Arc::clone(&self.sandbox));
            }
        }

        // Build error message with searched locations
        let searched: Vec<_> = self
            .search_paths
            .iter()
            .flat_map(|p| {
                [
                    p.join(format!("{name}.lua")).display().to_string(),
                    p.join(name).join("init.lua").display().to_string(),
                ]
            })
            .collect();

        Err(LuaError::ScriptNotFound(format!(
            "{} (searched: {})",
            name,
            searched.join(", ")
        )))
    }

    /// Lists all available script names.
    ///
    /// Includes scripts from all search paths and embedded (if enabled).
    #[must_use]
    pub fn list_available(&self) -> Vec<String> {
        use std::collections::HashSet;
        let mut names: HashSet<String> = HashSet::new();

        // Collect from search paths (single files + directories with init.lua)
        for dir in &self.search_paths {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() && path.extension().is_some_and(|ext| ext == "lua") {
                        if let Some(stem) = path.file_stem() {
                            names.insert(stem.to_string_lossy().into_owned());
                        }
                    } else if path.is_dir() && path.join("init.lua").exists() {
                        if let Some(name) = path.file_name() {
                            names.insert(name.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }

        let mut result: Vec<String> = names.into_iter().collect();
        result.sort();
        result
    }

    // === Batch loading methods ===

    /// Loads all scripts from configured search paths.
    ///
    /// Scans each search path for `.lua` files and attempts to load them.
    /// Errors are collected as warnings, not propagated—allowing partial success.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let loader = ScriptLoader::new()
    ///     .with_path("~/.orcs/scripts")
    ///     .with_path(".orcs/scripts");
    ///
    /// let result = loader.load_all();
    ///
    /// // Log warnings but continue
    /// for warn in &result.warnings {
    ///     eprintln!("Warning: {}", warn);
    /// }
    ///
    /// // Use loaded components
    /// for (name, component) in result.loaded {
    ///     println!("Loaded: {}", name);
    /// }
    /// ```
    #[must_use]
    pub fn load_all(&self) -> LoadResult {
        let mut result = LoadResult::default();

        for dir in &self.search_paths {
            if !dir.exists() {
                continue;
            }

            let entries = match std::fs::read_dir(dir) {
                Ok(e) => e,
                Err(e) => {
                    result.warnings.push(LoadWarning {
                        path: dir.clone(),
                        error: LuaError::ScriptNotFound(format!("failed to read directory: {}", e)),
                    });
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();

                if path.is_file() && path.extension().is_some_and(|ext| ext == "lua") {
                    // Single file: {dir}/{name}.lua
                    match LuaComponent::from_file(&path, Arc::clone(&self.sandbox)) {
                        Ok(component) => {
                            let name = path
                                .file_stem()
                                .map(|s| s.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            result.loaded.push((name, component));
                        }
                        Err(e) => {
                            result.warnings.push(LoadWarning { path, error: e });
                        }
                    }
                } else if path.is_dir() && path.join("init.lua").exists() {
                    // Directory component: {dir}/{name}/init.lua
                    let name = path
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    match LuaComponent::from_dir(&path, Arc::clone(&self.sandbox)) {
                        Ok(component) => {
                            result.loaded.push((name, component));
                        }
                        Err(e) => {
                            result.warnings.push(LoadWarning { path, error: e });
                        }
                    }
                }
            }
        }

        result
    }

    /// Loads all scripts from a single directory.
    ///
    /// Convenience method equivalent to:
    /// ```ignore
    /// ScriptLoader::new(sandbox)
    ///     .with_path(path)
    ///     .load_all()
    /// ```
    #[must_use]
    pub fn load_dir(path: &Path, sandbox: Arc<dyn SandboxPolicy>) -> LoadResult {
        Self::new(sandbox).with_path(path).load_all()
    }

    /// Loads a script from a specific file path.
    ///
    /// # Errors
    ///
    /// Returns error if file not found or invalid.
    pub fn load_file<P: AsRef<Path>>(
        path: P,
        sandbox: Arc<dyn SandboxPolicy>,
    ) -> Result<LuaComponent, LuaError> {
        LuaComponent::from_file(path, sandbox)
    }

    /// Returns the crate's built-in scripts directory.
    ///
    /// This is the `scripts/` directory relative to the crate root.
    /// Mainly useful for development and testing.
    #[must_use]
    pub fn crate_scripts_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("scripts")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orcs_component::Component;
    use orcs_runtime::sandbox::ProjectSandbox;

    fn test_sandbox() -> Arc<dyn SandboxPolicy> {
        Arc::new(ProjectSandbox::new(".").expect("test sandbox"))
    }

    #[test]
    fn new_loader_has_empty_search_paths() {
        let loader = ScriptLoader::new(test_sandbox());
        assert!(loader.search_paths.is_empty());
    }

    #[test]
    fn with_path_adds_to_search_paths() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path("/foo")
            .with_path("/bar");
        assert_eq!(loader.search_paths.len(), 2);
        assert_eq!(loader.search_paths[0], PathBuf::from("/foo"));
        assert_eq!(loader.search_paths[1], PathBuf::from("/bar"));
    }

    #[test]
    fn with_project_root_adds_scripts_subdir() {
        let loader = ScriptLoader::new(test_sandbox()).with_project_root("/project");
        assert_eq!(loader.search_paths[0], PathBuf::from("/project/scripts"));
    }

    #[test]
    fn load_from_crate_scripts_dir() {
        let loader = ScriptLoader::new(test_sandbox()).with_path(ScriptLoader::crate_scripts_dir());
        let component = loader.load("echo");
        assert!(component.is_ok());
    }

    #[test]
    fn load_not_found_shows_searched_paths() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path("/nonexistent/path")
            .without_embedded_fallback();
        let result = loader.load("missing");
        let Err(err) = result else {
            panic!("expected error");
        };
        let err_str = err.to_string();
        assert!(err_str.contains("/nonexistent/path"));
        assert!(err_str.contains("missing"));
    }

    #[test]
    fn list_available_includes_filesystem() {
        let loader = ScriptLoader::new(test_sandbox()).with_path(ScriptLoader::crate_scripts_dir());
        let names = loader.list_available();
        assert!(names.contains(&"echo".to_string()));
    }

    #[test]
    fn crate_scripts_dir_exists() {
        let dir = ScriptLoader::crate_scripts_dir();
        assert!(dir.exists(), "scripts dir should exist: {:?}", dir);
    }

    // === Batch loading tests ===

    #[test]
    fn load_all_from_crate_scripts_dir() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path(ScriptLoader::crate_scripts_dir())
            .without_embedded_fallback();
        let result = loader.load_all();

        // Should load at least echo.lua
        assert!(result.has_loaded());
        assert!(result.loaded_count() >= 1);

        // Verify echo is loaded
        let names: Vec<&str> = result.loaded.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"echo"));
    }

    #[test]
    fn load_all_empty_for_nonexistent_dir() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path("/nonexistent/path")
            .without_embedded_fallback();
        let result = loader.load_all();

        // Nonexistent dirs are skipped, not errors
        assert!(!result.has_loaded());
        assert!(!result.has_warnings());
    }

    #[test]
    fn load_dir_convenience() {
        let result = ScriptLoader::load_dir(&ScriptLoader::crate_scripts_dir(), test_sandbox());
        assert!(result.has_loaded());
    }

    #[test]
    fn load_result_debug() {
        let result = LoadResult::default();
        let debug_str = format!("{:?}", result);
        assert!(debug_str.contains("LoadResult"));
        assert!(debug_str.contains("loaded_count"));
    }

    #[test]
    fn load_warning_display() {
        let warn = LoadWarning {
            path: PathBuf::from("/test/script.lua"),
            error: LuaError::ScriptNotFound("test".into()),
        };
        let display_str = format!("{}", warn);
        assert!(display_str.contains("/test/script.lua"));
    }

    // === Directory-based loading tests ===

    #[test]
    fn from_dir_loads_init_lua() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let init = dir.path().join("init.lua");
        std::fs::write(
            &init,
            r#"return {
                id = "dir-component",
                namespace = "test",
                subscriptions = {"Echo"},
                on_request = function(r) return {success=true, data={}} end,
                on_signal = function(s) return "Handled" end,
            }"#,
        )
        .expect("write init.lua");

        let sb = test_sandbox();
        let component = LuaComponent::from_dir(dir.path(), sb).expect("load dir component");
        assert_eq!(component.id().name, "dir-component");
    }

    #[test]
    fn from_dir_require_colocated_module() {
        let dir = tempfile::tempdir().expect("create temp dir");

        // Create lib/helper.lua
        std::fs::create_dir_all(dir.path().join("lib")).expect("create lib dir");
        std::fs::write(
            dir.path().join("lib").join("helper.lua"),
            r#"local M = {}
            function M.greet() return "hello from helper" end
            return M"#,
        )
        .expect("write helper.lua");

        // Create init.lua that uses require
        std::fs::write(
            dir.path().join("init.lua"),
            r#"local helper = require("lib.helper")
            return {
                id = "require-test",
                namespace = "test",
                subscriptions = {"Echo"},
                on_request = function(r) return {success=true, data={msg=helper.greet()}} end,
                on_signal = function(s) return "Handled" end,
            }"#,
        )
        .expect("write init.lua");

        let sb = test_sandbox();
        let component =
            LuaComponent::from_dir(dir.path(), sb).expect("load require-test component");
        assert_eq!(component.id().name, "require-test");
    }

    #[test]
    fn loader_finds_directory_component() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let comp_dir = dir.path().join("my_comp");
        std::fs::create_dir_all(&comp_dir).expect("create my_comp dir");
        std::fs::write(
            comp_dir.join("init.lua"),
            r#"return {
                id = "my_comp",
                namespace = "test",
                subscriptions = {"Echo"},
                on_request = function(r) return {success=true, data={}} end,
                on_signal = function(s) return "Handled" end,
            }"#,
        )
        .expect("write my_comp/init.lua");

        let sb = test_sandbox();
        let loader = ScriptLoader::new(sb).with_path(dir.path());
        let component = loader.load("my_comp").expect("load my_comp");
        assert_eq!(component.id().name, "my_comp");
    }

    #[test]
    fn load_skill_manager_as_directory_component() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path(ScriptLoader::crate_scripts_dir())
            .without_embedded_fallback();

        let component = loader
            .load("skill_manager")
            .expect("skill_manager should load as directory component via init.lua");

        assert_eq!(component.id().name, "skill_manager");
    }

    #[test]
    fn list_available_includes_skill_manager_directory() {
        let loader = ScriptLoader::new(test_sandbox())
            .with_path(ScriptLoader::crate_scripts_dir())
            .without_embedded_fallback();
        let names = loader.list_available();
        assert!(
            names.contains(&"skill_manager".to_string()),
            "directory component should appear in list: {names:?}"
        );
    }

    #[test]
    fn from_dir_require_flat_colocated_module() {
        // Verify that require("module_name") resolves to a flat co-located file
        // (no lib/ subdirectory needed).
        let dir = tempfile::tempdir().expect("create temp dir");

        std::fs::write(
            dir.path().join("my_helper.lua"),
            r#"local M = {}
            function M.value() return 42 end
            return M"#,
        )
        .expect("write my_helper.lua");

        std::fs::write(
            dir.path().join("init.lua"),
            r#"local helper = require("my_helper")
            return {
                id = "flat-require-test",
                namespace = "test",
                subscriptions = {"Echo"},
                on_request = function(r) return {success=true, data={v=helper.value()}} end,
                on_signal = function(s) return "Handled" end,
            }"#,
        )
        .expect("write init.lua");

        let component = LuaComponent::from_dir(dir.path(), test_sandbox())
            .expect("load flat require component");
        assert_eq!(component.id().name, "flat-require-test");
    }

    #[test]
    fn list_available_includes_directories() {
        let dir = tempfile::tempdir().expect("create temp dir");
        let comp_dir = dir.path().join("dir_comp");
        std::fs::create_dir_all(&comp_dir).expect("create dir_comp dir");
        std::fs::write(comp_dir.join("init.lua"), "return {}").expect("write init.lua");

        let sb = test_sandbox();
        let loader = ScriptLoader::new(sb)
            .with_path(dir.path())
            .without_embedded_fallback();
        let names = loader.list_available();
        assert!(names.contains(&"dir_comp".to_string()));
    }
}

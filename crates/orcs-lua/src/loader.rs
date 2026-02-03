//! Lua script loader with embedded and runtime support.
//!
//! # Loading Priority
//!
//! 1. Search paths in order (first match wins)
//! 2. Embedded script (fallback, if enabled)
//!
//! # Example
//!
//! ```ignore
//! use orcs_lua::ScriptLoader;
//! use std::path::Path;
//!
//! // Create loader with project root
//! let loader = ScriptLoader::new()
//!     .with_project_root("/path/to/project")
//!     .with_path("/additional/scripts");
//!
//! // Load script (searches paths, then embedded)
//! let component = loader.load("echo")?;
//!
//! // Static methods still available
//! let component = ScriptLoader::load_embedded("echo")?;
//! ```

use crate::component::LuaComponent;
use crate::embedded;
use crate::error::LuaError;
use std::path::{Path, PathBuf};

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
/// Search order:
/// 1. Configured search paths (in order added)
/// 2. Embedded scripts (if `use_embedded_fallback` is true)
#[derive(Debug, Clone)]
pub struct ScriptLoader {
    /// Search paths for scripts (priority order).
    search_paths: Vec<PathBuf>,
    /// Whether to use embedded scripts as fallback.
    use_embedded_fallback: bool,
}

impl Default for ScriptLoader {
    fn default() -> Self {
        Self::new()
    }
}

impl ScriptLoader {
    /// Creates a new loader with embedded fallback enabled.
    #[must_use]
    pub fn new() -> Self {
        Self {
            search_paths: Vec::new(),
            use_embedded_fallback: true,
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

    /// Disables embedded script fallback.
    ///
    /// When disabled, only filesystem scripts are loaded.
    #[must_use]
    pub fn without_embedded_fallback(mut self) -> Self {
        self.use_embedded_fallback = false;
        self
    }

    /// Enables embedded script fallback (default).
    #[must_use]
    pub fn with_embedded_fallback(mut self) -> Self {
        self.use_embedded_fallback = true;
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
    /// 1. Each search path: `{path}/{name}.lua`
    /// 2. Embedded script (if enabled)
    ///
    /// # Errors
    ///
    /// Returns error if script not found in any location.
    pub fn load(&self, name: &str) -> Result<LuaComponent, LuaError> {
        // Search filesystem paths first
        for path in &self.search_paths {
            let file_path = path.join(format!("{}.lua", name));
            if file_path.exists() {
                return LuaComponent::from_file(&file_path);
            }
        }

        // Fallback to embedded
        if self.use_embedded_fallback {
            if let Some(script) = embedded::get(name) {
                return LuaComponent::from_script(script);
            }
        }

        // Build error message with searched locations
        let mut searched = self
            .search_paths
            .iter()
            .map(|p| p.join(format!("{}.lua", name)).display().to_string())
            .collect::<Vec<_>>();

        if self.use_embedded_fallback {
            searched.push(format!("embedded:{}", name));
        }

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

        // Collect from search paths
        for dir in &self.search_paths {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|ext| ext == "lua") {
                        if let Some(stem) = path.file_stem() {
                            names.insert(stem.to_string_lossy().into_owned());
                        }
                    }
                }
            }
        }

        // Add embedded scripts
        if self.use_embedded_fallback {
            for name in embedded::list() {
                names.insert(name.to_string());
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
    /// **Note**: Embedded scripts are NOT included. Use [`load_embedded`] separately.
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

                // Skip non-.lua files
                if path.extension().is_none_or(|ext| ext != "lua") {
                    continue;
                }

                // Skip directories
                if path.is_dir() {
                    continue;
                }

                match LuaComponent::from_file(&path) {
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
            }
        }

        result
    }

    /// Loads all scripts from a single directory.
    ///
    /// Convenience method equivalent to:
    /// ```ignore
    /// ScriptLoader::new()
    ///     .with_path(path)
    ///     .without_embedded_fallback()
    ///     .load_all()
    /// ```
    #[must_use]
    pub fn load_dir(path: &Path) -> LoadResult {
        Self::new()
            .with_path(path)
            .without_embedded_fallback()
            .load_all()
    }

    // === Static methods (backward compatibility) ===

    /// Loads an embedded script by name.
    ///
    /// # Errors
    ///
    /// Returns error if script name not found in embedded scripts.
    pub fn load_embedded(name: &str) -> Result<LuaComponent, LuaError> {
        let script = embedded::get(name)
            .ok_or_else(|| LuaError::ScriptNotFound(format!("embedded:{}", name)))?;

        LuaComponent::from_script(script)
    }

    /// Loads a script from a specific file path.
    ///
    /// No fallback to embedded scripts.
    ///
    /// # Errors
    ///
    /// Returns error if file not found or invalid.
    pub fn load_file<P: AsRef<Path>>(path: P) -> Result<LuaComponent, LuaError> {
        LuaComponent::from_file(path)
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

    #[test]
    fn new_loader_has_embedded_fallback() {
        let loader = ScriptLoader::new();
        assert!(loader.use_embedded_fallback);
        assert!(loader.search_paths.is_empty());
    }

    #[test]
    fn with_path_adds_to_search_paths() {
        let loader = ScriptLoader::new().with_path("/foo").with_path("/bar");
        assert_eq!(loader.search_paths.len(), 2);
        assert_eq!(loader.search_paths[0], PathBuf::from("/foo"));
        assert_eq!(loader.search_paths[1], PathBuf::from("/bar"));
    }

    #[test]
    fn with_project_root_adds_scripts_subdir() {
        let loader = ScriptLoader::new().with_project_root("/project");
        assert_eq!(loader.search_paths[0], PathBuf::from("/project/scripts"));
    }

    #[test]
    fn without_embedded_fallback_disables_it() {
        let loader = ScriptLoader::new().without_embedded_fallback();
        assert!(!loader.use_embedded_fallback);
    }

    #[test]
    fn load_from_embedded() {
        let loader = ScriptLoader::new();
        let component = loader.load("echo").expect("echo should load from embedded");
        assert!(component.id().fqn().contains("echo"));
    }

    #[test]
    fn load_from_crate_scripts_dir() {
        let loader = ScriptLoader::new().with_path(ScriptLoader::crate_scripts_dir());
        let component = loader.load("echo");
        assert!(component.is_ok());
    }

    #[test]
    fn load_not_found_shows_searched_paths() {
        let loader = ScriptLoader::new()
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
    fn list_available_includes_embedded() {
        let loader = ScriptLoader::new();
        let names = loader.list_available();
        assert!(names.contains(&"echo".to_string()));
    }

    #[test]
    fn list_available_includes_filesystem() {
        let loader = ScriptLoader::new().with_path(ScriptLoader::crate_scripts_dir());
        let names = loader.list_available();
        assert!(names.contains(&"echo".to_string()));
    }

    // Static method tests
    #[test]
    fn static_load_embedded() {
        let component = ScriptLoader::load_embedded("echo");
        assert!(component.is_ok());
    }

    #[test]
    fn static_load_embedded_not_found() {
        let result = ScriptLoader::load_embedded("nonexistent");
        assert!(result.is_err());
    }

    #[test]
    fn crate_scripts_dir_exists() {
        let dir = ScriptLoader::crate_scripts_dir();
        assert!(dir.exists(), "scripts dir should exist: {:?}", dir);
    }

    // === Batch loading tests ===

    #[test]
    fn load_all_from_crate_scripts_dir() {
        let loader = ScriptLoader::new()
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
        let loader = ScriptLoader::new()
            .with_path("/nonexistent/path")
            .without_embedded_fallback();
        let result = loader.load_all();

        // Nonexistent dirs are skipped, not errors
        assert!(!result.has_loaded());
        assert!(!result.has_warnings());
    }

    #[test]
    fn load_dir_convenience() {
        let result = ScriptLoader::load_dir(&ScriptLoader::crate_scripts_dir());
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
}

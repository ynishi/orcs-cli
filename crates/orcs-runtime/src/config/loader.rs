//! Configuration loader with hierarchical merging.
//!
//! # Load Order
//!
//! 1. Default values (compile-time)
//! 2. Global config (`~/.orcs/config.toml`)
//! 3. Project config (`.orcs/config.toml`)
//! 4. Profile config (`.orcs/profiles/{name}.toml` `[config]` section)
//! 5. Environment variables (`ORCS_*`, including `ORCS_PROFILE`)
//!
//! Each layer overrides the previous.

use super::{
    default_config_path, profile::ProfileStore, ConfigError, OrcsConfig, PROJECT_CONFIG_DIR,
    PROJECT_CONFIG_FILE,
};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Helper macro for parsing boolean environment variables.
macro_rules! parse_env_bool {
    ($config:expr, $field:expr, $var:literal) => {
        if let Ok(val) = std::env::var($var) {
            $field = parse_bool(&val)
                .ok_or_else(|| ConfigError::invalid_env_var($var, "expected bool"))?;
        }
    };
}

/// Configuration loader with builder pattern.
///
/// # Example
///
/// ```ignore
/// use orcs_runtime::config::ConfigLoader;
///
/// let config = ConfigLoader::new()
///     .with_project_root("/path/to/project")
///     .skip_env_vars()  // For testing
///     .load()?;
/// ```
#[derive(Debug, Clone)]
pub struct ConfigLoader {
    /// Global config file path (defaults to ~/.orcs/config.toml).
    global_config_path: Option<PathBuf>,

    /// Project root directory.
    project_root: Option<PathBuf>,

    /// Profile name to activate.
    ///
    /// When set, the profile's `[config]` section is merged as a layer
    /// between project config and env vars.
    profile: Option<String>,

    /// Skip environment variable loading.
    skip_env: bool,

    /// Skip global config loading.
    skip_global: bool,

    /// Skip project config loading.
    skip_project: bool,
}

impl ConfigLoader {
    /// Creates a new loader with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            global_config_path: None,
            project_root: None,
            profile: None,
            skip_env: false,
            skip_global: false,
            skip_project: false,
        }
    }

    /// Sets a custom global config path.
    #[must_use]
    pub fn with_global_config(mut self, path: impl Into<PathBuf>) -> Self {
        self.global_config_path = Some(path.into());
        self
    }

    /// Sets the project root directory.
    ///
    /// Project config will be loaded from `<project_root>/.orcs/config.toml`.
    #[must_use]
    pub fn with_project_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.project_root = Some(path.into());
        self
    }

    /// Sets the profile to activate.
    ///
    /// The profile's `[config]` section is merged as a layer
    /// between project config and env vars. The profile is
    /// loaded from `ProfileStore` search dirs.
    #[must_use]
    pub fn with_profile(mut self, name: impl Into<String>) -> Self {
        self.profile = Some(name.into());
        self
    }

    /// Skips environment variable loading.
    ///
    /// Useful for testing with deterministic config.
    #[must_use]
    pub fn skip_env_vars(mut self) -> Self {
        self.skip_env = true;
        self
    }

    /// Skips global config loading.
    #[must_use]
    pub fn skip_global_config(mut self) -> Self {
        self.skip_global = true;
        self
    }

    /// Skips project config loading.
    #[must_use]
    pub fn skip_project_config(mut self) -> Self {
        self.skip_project = true;
        self
    }

    /// Loads and merges configuration from all sources.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if any config file exists but cannot be parsed.
    /// Missing config files are silently ignored.
    pub fn load(&self) -> Result<OrcsConfig, ConfigError> {
        // Start with defaults
        let mut config = OrcsConfig::default();

        // Layer 1: Global config
        if !self.skip_global {
            let global_path = self
                .global_config_path
                .clone()
                .unwrap_or_else(default_config_path);

            if let Some(global_config) = self.load_file(&global_path)? {
                debug!(path = %global_path.display(), "Loaded global config");
                config.merge(&global_config);
            }
        }

        // Layer 2: Project config
        if !self.skip_project {
            if let Some(ref project_root) = self.project_root {
                let project_config_path = project_root
                    .join(PROJECT_CONFIG_DIR)
                    .join(PROJECT_CONFIG_FILE);

                if let Some(project_config) = self.load_file(&project_config_path)? {
                    debug!(
                        path = %project_config_path.display(),
                        project = %project_root.display(),
                        "Loaded project config"
                    );
                    config.merge(&project_config);
                }
            }
        }

        // Layer 3: Profile config overlay
        // Resolve profile name: explicit > ORCS_PROFILE env var
        let profile_name = self
            .profile
            .clone()
            .or_else(|| std::env::var("ORCS_PROFILE").ok());

        if let Some(ref name) = profile_name {
            let store = ProfileStore::new(self.project_root.as_deref());
            match store.load(name) {
                Ok(profile_def) => {
                    if let Some(ref profile_config) = profile_def.config {
                        debug!(profile = %name, "Applying profile config overlay");
                        config.merge(profile_config);
                    }
                }
                Err(e) => {
                    debug!(profile = %name, error = %e, "Profile not found, skipping config overlay");
                }
            }
        }

        // Layer 4: Environment variables
        if !self.skip_env {
            self.apply_env_vars(&mut config)?;
        }

        Ok(config)
    }

    /// Loads a config file, returning None if it doesn't exist.
    fn load_file(&self, path: &Path) -> Result<Option<OrcsConfig>, ConfigError> {
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::read_file(path, e))?;

        let config =
            OrcsConfig::from_toml(&content).map_err(|e| ConfigError::parse_toml(path, e))?;

        Ok(Some(config))
    }

    /// Applies environment variable overrides.
    fn apply_env_vars(&self, config: &mut OrcsConfig) -> Result<(), ConfigError> {
        // Boolean environment variables
        parse_env_bool!(config, config.debug, "ORCS_DEBUG");
        parse_env_bool!(config, config.hil.auto_approve, "ORCS_AUTO_APPROVE");
        parse_env_bool!(config, config.ui.verbose, "ORCS_VERBOSE");
        parse_env_bool!(config, config.ui.color, "ORCS_COLOR");
        parse_env_bool!(config, config.scripts.auto_load, "ORCS_SCRIPTS_AUTO_LOAD");

        // String environment variables
        if let Ok(val) = std::env::var("ORCS_MODEL") {
            config.model.default = val;
        }

        // Path environment variables
        if let Ok(val) = std::env::var("ORCS_SESSION_PATH") {
            config.paths.session_dir = Some(PathBuf::from(val));
        }

        Ok(())
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Parses a boolean from string.
///
/// Accepts: "true", "false", "1", "0", "yes", "no" (case-insensitive).
fn parse_bool(s: &str) -> Option<bool> {
    match s.to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Saves a config to the global config file.
///
/// Creates the parent directory if needed.
///
/// # Errors
///
/// Returns [`ConfigError`] if the file cannot be written.
pub fn save_global_config(config: &OrcsConfig) -> Result<(), ConfigError> {
    let path = default_config_path();

    // Ensure parent directory exists
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| ConfigError::create_dir(parent, e))?;
        }
    }

    let toml = config.to_toml()?;
    std::fs::write(&path, toml).map_err(|e| ConfigError::write_file(&path, e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_config_file(dir: &Path, content: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_defaults_only() {
        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .skip_env_vars()
            .load()
            .unwrap();

        assert_eq!(config, OrcsConfig::default());
    }

    #[test]
    fn load_global_config() {
        let temp = TempDir::new().unwrap();
        let config_path = create_config_file(
            temp.path(),
            r#"
debug = true

[model]
default = "test-model"
"#,
        );

        let config = ConfigLoader::new()
            .with_global_config(&config_path)
            .skip_project_config()
            .skip_env_vars()
            .load()
            .unwrap();

        assert!(config.debug);
        assert_eq!(config.model.default, "test-model");
    }

    #[test]
    fn load_project_overrides_global() {
        let global_temp = TempDir::new().unwrap();
        let project_temp = TempDir::new().unwrap();

        // Create .orcs directory in project
        let orcs_dir = project_temp.path().join(".orcs");
        std::fs::create_dir_all(&orcs_dir).unwrap();

        // Global config
        let global_path = create_config_file(
            global_temp.path(),
            r#"
debug = true

[model]
default = "global-model"
"#,
        );

        // Project config
        create_config_file(
            &orcs_dir,
            r#"
[model]
default = "project-model"
"#,
        );

        let config = ConfigLoader::new()
            .with_global_config(&global_path)
            .with_project_root(project_temp.path())
            .skip_env_vars()
            .load()
            .unwrap();

        // debug from global (not overridden in project)
        assert!(config.debug);
        // model from project (overrides global)
        assert_eq!(config.model.default, "project-model");
    }

    #[test]
    fn missing_config_files_ok() {
        let config = ConfigLoader::new()
            .with_global_config("/nonexistent/path/config.toml")
            .with_project_root("/nonexistent/project")
            .skip_env_vars()
            .load()
            .unwrap();

        // Should return defaults
        assert_eq!(config, OrcsConfig::default());
    }

    #[test]
    fn parse_bool_values() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("TRUE"), Some(true));
        assert_eq!(parse_bool("1"), Some(true));
        assert_eq!(parse_bool("yes"), Some(true));
        assert_eq!(parse_bool("on"), Some(true));

        assert_eq!(parse_bool("false"), Some(false));
        assert_eq!(parse_bool("FALSE"), Some(false));
        assert_eq!(parse_bool("0"), Some(false));
        assert_eq!(parse_bool("no"), Some(false));
        assert_eq!(parse_bool("off"), Some(false));

        assert_eq!(parse_bool("invalid"), None);
    }

    #[test]
    fn load_with_profile_overlay() {
        let project_temp = TempDir::new().unwrap();

        // Create profile
        let profiles_dir = project_temp.path().join(".orcs").join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        std::fs::write(
            profiles_dir.join("test-profile.toml"),
            r#"
[profile]
name = "test-profile"
description = "Test profile"

[config]
debug = true

[config.model]
default = "profile-model"
"#,
        )
        .unwrap();

        let config = ConfigLoader::new()
            .skip_global_config()
            .with_project_root(project_temp.path())
            .with_profile("test-profile")
            .skip_env_vars()
            .load()
            .unwrap();

        assert!(config.debug);
        assert_eq!(config.model.default, "profile-model");
    }

    #[test]
    fn load_with_nonexistent_profile_ignores() {
        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .with_profile("nonexistent")
            .skip_env_vars()
            .load()
            .unwrap();

        // Should fall back to defaults
        assert_eq!(config, OrcsConfig::default());
    }

    #[test]
    fn env_var_override() {
        // This test modifies env vars, run in isolation
        std::env::set_var("ORCS_DEBUG", "true");
        std::env::set_var("ORCS_MODEL", "env-model");

        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .load()
            .unwrap();

        assert!(config.debug);
        assert_eq!(config.model.default, "env-model");

        // Cleanup
        std::env::remove_var("ORCS_DEBUG");
        std::env::remove_var("ORCS_MODEL");
    }
}

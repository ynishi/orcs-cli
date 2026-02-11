//! Configuration loader with hierarchical merging.
//!
//! # Load Order
//!
//! 1. Default values (compile-time)
//! 2. Global config (`~/.orcs/config.toml`)
//! 3. Project config (`.orcs/config.toml`)
//! 4. Profile config (`.orcs/profiles/{name}.toml` `[config]` section)
//! 5. Environment overrides (`ORCS_*`, including `ORCS_PROFILE`)
//!
//! Each layer overrides the previous.

use super::{
    default_config_path, profile::ProfileStore, ConfigError, OrcsConfig, PROJECT_CONFIG_DIR,
    PROJECT_CONFIG_FILE,
};
use std::path::{Path, PathBuf};
use tracing::debug;

/// Environment variable overrides.
///
/// Captures all `ORCS_*` environment variables as typed fields.
/// Constructed once via [`from_env()`](Self::from_env) at startup,
/// or directly in tests without touching the process environment.
///
/// # Example
///
/// ```ignore
/// // Production: read from process environment
/// let overrides = EnvOverrides::from_env()?;
///
/// // Test: construct directly (no env mutation)
/// let overrides = EnvOverrides {
///     debug: Some(true),
///     model: Some("test-model".into()),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Default)]
pub struct EnvOverrides {
    /// `ORCS_DEBUG`
    pub debug: Option<bool>,
    /// `ORCS_AUTO_APPROVE`
    pub auto_approve: Option<bool>,
    /// `ORCS_VERBOSE`
    pub verbose: Option<bool>,
    /// `ORCS_COLOR`
    pub color: Option<bool>,
    /// `ORCS_SCRIPTS_AUTO_LOAD`
    pub scripts_auto_load: Option<bool>,
    /// `ORCS_MODEL`
    pub model: Option<String>,
    /// `ORCS_SESSION_PATH`
    pub session_path: Option<PathBuf>,
    /// `ORCS_BUILTINS_DIR`
    pub builtins_dir: Option<PathBuf>,
    /// `ORCS_PROFILE`
    pub profile: Option<String>,
}

impl EnvOverrides {
    /// Reads all `ORCS_*` environment variables from the process environment.
    ///
    /// This is the **only** place that calls `std::env::var` for config.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::InvalidEnvVar`] if a boolean variable
    /// contains an unparseable value.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            debug: read_env_bool("ORCS_DEBUG")?,
            auto_approve: read_env_bool("ORCS_AUTO_APPROVE")?,
            verbose: read_env_bool("ORCS_VERBOSE")?,
            color: read_env_bool("ORCS_COLOR")?,
            scripts_auto_load: read_env_bool("ORCS_SCRIPTS_AUTO_LOAD")?,
            model: read_env_string("ORCS_MODEL"),
            session_path: read_env_string("ORCS_SESSION_PATH").map(PathBuf::from),
            builtins_dir: read_env_string("ORCS_BUILTINS_DIR").map(PathBuf::from),
            profile: read_env_string("ORCS_PROFILE"),
        })
    }
}

/// Reads a boolean environment variable.
///
/// Returns `Ok(None)` if not set, `Ok(Some(bool))` if valid,
/// `Err` if set but not parseable as bool.
fn read_env_bool(name: &str) -> Result<Option<bool>, ConfigError> {
    match std::env::var(name) {
        Ok(val) => parse_bool(&val)
            .map(Some)
            .ok_or_else(|| ConfigError::invalid_env_var(name, "expected bool")),
        Err(_) => Ok(None),
    }
}

/// Reads a string environment variable. Returns `None` if not set.
fn read_env_string(name: &str) -> Option<String> {
    std::env::var(name).ok()
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

    /// Profile name to activate (explicit, highest priority).
    ///
    /// When set, this takes precedence over `ORCS_PROFILE` env var.
    profile: Option<String>,

    /// Pre-built environment overrides.
    ///
    /// - `Some(overrides)` + `skip_env=false` → use these (test injection).
    /// - `None` + `skip_env=false` → call `EnvOverrides::from_env()`.
    /// - `skip_env=true` → no env overrides applied regardless.
    env_overrides: Option<EnvOverrides>,

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
            env_overrides: None,
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

    /// Injects pre-built environment overrides.
    ///
    /// When set, `load()` uses these instead of reading `std::env::var`.
    /// This enables deterministic testing without env mutation.
    ///
    /// Ignored if [`skip_env_vars()`](Self::skip_env_vars) is also called.
    #[must_use]
    pub fn with_env_overrides(mut self, overrides: EnvOverrides) -> Self {
        self.env_overrides = Some(overrides);
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
        // Resolve environment overrides once
        let overrides = self.resolve_env_overrides()?;

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
        // Priority: explicit > env override
        let profile_name = self
            .profile
            .clone()
            .or_else(|| overrides.as_ref().and_then(|o| o.profile.clone()));

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

        // Layer 4: Environment overrides
        if let Some(ref ov) = overrides {
            Self::apply_overrides(&mut config, ov);
        }

        Ok(config)
    }

    /// Resolves environment overrides based on builder state.
    ///
    /// - `skip_env=true` → `None` (no overrides)
    /// - `env_overrides=Some(x)` → `Some(x)` (injected)
    /// - otherwise → `Some(EnvOverrides::from_env()?)` (read from process)
    fn resolve_env_overrides(&self) -> Result<Option<EnvOverrides>, ConfigError> {
        if self.skip_env {
            return Ok(None);
        }

        if let Some(ref ov) = self.env_overrides {
            return Ok(Some(ov.clone()));
        }

        EnvOverrides::from_env().map(Some)
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

    /// Applies environment overrides to config.
    ///
    /// Pure function: reads from `EnvOverrides` fields only.
    fn apply_overrides(config: &mut OrcsConfig, ov: &EnvOverrides) {
        if let Some(v) = ov.debug {
            config.debug = v;
        }
        if let Some(v) = ov.auto_approve {
            config.hil.auto_approve = v;
        }
        if let Some(v) = ov.verbose {
            config.ui.verbose = v;
        }
        if let Some(v) = ov.color {
            config.ui.color = v;
        }
        if let Some(v) = ov.scripts_auto_load {
            config.scripts.auto_load = v;
        }
        if let Some(ref v) = ov.model {
            config.model.default.clone_from(v);
        }
        if let Some(ref v) = ov.session_path {
            config.paths.session_dir = Some(v.clone());
        }
        if let Some(ref v) = ov.builtins_dir {
            config.components.builtins_dir = v.clone();
        }
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

    // --- EnvOverrides tests (no set_var/remove_var) ---

    #[test]
    fn env_overrides_applied() {
        let overrides = EnvOverrides {
            debug: Some(true),
            model: Some("env-model".into()),
            ..Default::default()
        };

        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .with_env_overrides(overrides)
            .load()
            .unwrap();

        assert!(config.debug);
        assert_eq!(config.model.default, "env-model");
    }

    #[test]
    fn env_overrides_all_fields() {
        let overrides = EnvOverrides {
            debug: Some(true),
            auto_approve: Some(true),
            verbose: Some(true),
            color: Some(false),
            scripts_auto_load: Some(false),
            model: Some("override-model".into()),
            session_path: Some(PathBuf::from("/custom/sessions")),
            profile: None,
        };

        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .with_env_overrides(overrides)
            .load()
            .unwrap();

        assert!(config.debug);
        assert!(config.hil.auto_approve);
        assert!(config.ui.verbose);
        assert!(!config.ui.color);
        assert!(!config.scripts.auto_load);
        assert_eq!(config.model.default, "override-model");
        assert_eq!(
            config.paths.session_dir,
            Some(PathBuf::from("/custom/sessions"))
        );
    }

    #[test]
    fn skip_env_ignores_injected_overrides() {
        let overrides = EnvOverrides {
            debug: Some(true),
            ..Default::default()
        };

        // skip_env_vars takes precedence
        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .with_env_overrides(overrides)
            .skip_env_vars()
            .load()
            .unwrap();

        assert!(!config.debug); // default, not overridden
    }

    #[test]
    fn env_profile_activates_profile() {
        let project_temp = TempDir::new().unwrap();

        let profiles_dir = project_temp.path().join(".orcs").join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();
        std::fs::write(
            profiles_dir.join("env-profile.toml"),
            r#"
[profile]
name = "env-profile"

[config]
debug = true
"#,
        )
        .unwrap();

        let overrides = EnvOverrides {
            profile: Some("env-profile".into()),
            ..Default::default()
        };

        let config = ConfigLoader::new()
            .skip_global_config()
            .with_project_root(project_temp.path())
            .with_env_overrides(overrides)
            .load()
            .unwrap();

        assert!(config.debug);
    }

    #[test]
    fn explicit_profile_overrides_env_profile() {
        let project_temp = TempDir::new().unwrap();
        let profiles_dir = project_temp.path().join(".orcs").join("profiles");
        std::fs::create_dir_all(&profiles_dir).unwrap();

        std::fs::write(
            profiles_dir.join("env-profile.toml"),
            r#"
[profile]
name = "env-profile"

[config.model]
default = "env-model"
"#,
        )
        .unwrap();

        std::fs::write(
            profiles_dir.join("explicit-profile.toml"),
            r#"
[profile]
name = "explicit-profile"

[config.model]
default = "explicit-model"
"#,
        )
        .unwrap();

        let overrides = EnvOverrides {
            profile: Some("env-profile".into()),
            ..Default::default()
        };

        let config = ConfigLoader::new()
            .skip_global_config()
            .with_project_root(project_temp.path())
            .with_profile("explicit-profile")
            .with_env_overrides(overrides)
            .load()
            .unwrap();

        // explicit profile wins over env
        assert_eq!(config.model.default, "explicit-model");
    }

    #[test]
    fn env_overrides_default_is_empty() {
        let ov = EnvOverrides::default();
        assert!(ov.debug.is_none());
        assert!(ov.auto_approve.is_none());
        assert!(ov.verbose.is_none());
        assert!(ov.color.is_none());
        assert!(ov.scripts_auto_load.is_none());
        assert!(ov.model.is_none());
        assert!(ov.session_path.is_none());
        assert!(ov.profile.is_none());
    }

    #[test]
    fn empty_overrides_preserve_defaults() {
        let config = ConfigLoader::new()
            .skip_global_config()
            .skip_project_config()
            .with_env_overrides(EnvOverrides::default())
            .load()
            .unwrap();

        assert_eq!(config, OrcsConfig::default());
    }
}

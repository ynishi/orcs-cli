//! Configuration types.
//!
//! All types implement [`Default`] for compile-time fallback values.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main configuration structure.
///
/// This is the unified configuration after merging all layers.
///
/// # Serialization
///
/// Serializes to TOML for file storage. Fields with `#[serde(default)]`
/// are optional in the config file.
///
/// # Example
///
/// ```
/// use orcs_runtime::config::OrcsConfig;
///
/// let config = OrcsConfig::default();
/// assert!(!config.debug);
/// ```
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OrcsConfig {
    /// Enable debug mode (verbose logging, diagnostics).
    pub debug: bool,

    /// Model configuration.
    pub model: ModelConfig,

    /// Human-in-the-loop configuration.
    pub hil: HilConfig,

    /// Path configuration.
    pub paths: PathsConfig,

    /// UI configuration.
    pub ui: UiConfig,

    /// Script configuration.
    pub scripts: ScriptsConfig,
}

impl OrcsConfig {
    /// Creates a new config with default values.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Serializes to TOML string.
    ///
    /// # Errors
    ///
    /// Returns error if serialization fails.
    pub fn to_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(self)
    }

    /// Deserializes from TOML string.
    ///
    /// # Errors
    ///
    /// Returns error if deserialization fails.
    pub fn from_toml(toml_str: &str) -> Result<Self, toml::de::Error> {
        toml::from_str(toml_str)
    }

    /// Merges another config into this one.
    ///
    /// Values from `other` override values in `self` only if they
    /// differ from the default. This enables layered configuration.
    pub fn merge(&mut self, other: &Self) {
        let default = Self::default();

        // Only override if other differs from default
        if other.debug != default.debug {
            self.debug = other.debug;
        }

        self.model.merge(&other.model);
        self.hil.merge(&other.hil);
        self.paths.merge(&other.paths);
        self.ui.merge(&other.ui);
        self.scripts.merge(&other.scripts);
    }
}

/// Model configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ModelConfig {
    /// Default model to use.
    pub default: String,

    /// Temperature for generation (0.0-1.0).
    pub temperature: f32,

    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            default: "claude-3-opus".into(),
            temperature: 0.7,
            max_tokens: None,
        }
    }
}

impl ModelConfig {
    fn merge(&mut self, other: &Self) {
        let default = Self::default();

        if other.default != default.default {
            self.default = other.default.clone();
        }
        if (other.temperature - default.temperature).abs() > f32::EPSILON {
            self.temperature = other.temperature;
        }
        if other.max_tokens.is_some() {
            self.max_tokens = other.max_tokens;
        }
    }
}

/// Human-in-the-loop configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HilConfig {
    /// Automatically approve all requests (dangerous).
    pub auto_approve: bool,

    /// Timeout for approval requests in milliseconds.
    pub timeout_ms: u64,

    /// Require approval for destructive operations.
    pub require_approval_destructive: bool,
}

impl Default for HilConfig {
    fn default() -> Self {
        Self {
            auto_approve: false,
            timeout_ms: 30_000,
            require_approval_destructive: true,
        }
    }
}

impl HilConfig {
    fn merge(&mut self, other: &Self) {
        let default = Self::default();

        if other.auto_approve != default.auto_approve {
            self.auto_approve = other.auto_approve;
        }
        if other.timeout_ms != default.timeout_ms {
            self.timeout_ms = other.timeout_ms;
        }
        if other.require_approval_destructive != default.require_approval_destructive {
            self.require_approval_destructive = other.require_approval_destructive;
        }
    }
}

/// Path configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PathsConfig {
    /// Session storage directory.
    pub session_dir: Option<PathBuf>,

    /// Cache directory.
    pub cache_dir: Option<PathBuf>,
}

impl PathsConfig {
    fn merge(&mut self, other: &Self) {
        if other.session_dir.is_some() {
            self.session_dir = other.session_dir.clone();
        }
        if other.cache_dir.is_some() {
            self.cache_dir = other.cache_dir.clone();
        }
    }

    /// Returns the session directory, falling back to default.
    #[must_use]
    pub fn session_dir_or_default(&self) -> PathBuf {
        self.session_dir
            .clone()
            .unwrap_or_else(crate::session::default_session_path)
    }
}

/// UI configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct UiConfig {
    /// Verbose output mode.
    pub verbose: bool,

    /// Enable color output.
    pub color: bool,

    /// Enable emoji in output.
    pub emoji: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            verbose: false,
            color: true,
            emoji: false,
        }
    }
}

impl UiConfig {
    fn merge(&mut self, other: &Self) {
        let default = Self::default();

        if other.verbose != default.verbose {
            self.verbose = other.verbose;
        }
        if other.color != default.color {
            self.color = other.color;
        }
        if other.emoji != default.emoji {
            self.emoji = other.emoji;
        }
    }
}

/// Script loading configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ScriptsConfig {
    /// Script search directories (in priority order).
    ///
    /// Supports:
    /// - Absolute paths: `/opt/orcs/scripts`
    /// - Home expansion: `~/.orcs/scripts`
    /// - Relative paths: `.orcs/scripts` (resolved against project root)
    pub dirs: Vec<PathBuf>,

    /// Auto-load all scripts from dirs on startup.
    ///
    /// When enabled, all `.lua` files in configured directories
    /// are automatically loaded at application startup.
    pub auto_load: bool,
}

impl ScriptsConfig {
    /// Resolves configured directories with tilde expansion and project root.
    ///
    /// - Absolute paths are used as-is
    /// - `~` is expanded to home directory
    /// - Relative paths are resolved against project root (if provided)
    /// - Non-existent directories are filtered out
    #[must_use]
    pub fn resolve_dirs(&self, project_root: Option<&std::path::Path>) -> Vec<PathBuf> {
        self.dirs
            .iter()
            .filter_map(|p| {
                let expanded = expand_tilde(p);
                if expanded.is_absolute() {
                    Some(expanded)
                } else {
                    project_root.map(|root| root.join(&expanded))
                }
            })
            .filter(|p| p.exists())
            .collect()
    }

    fn merge(&mut self, other: &Self) {
        // Append other's dirs (don't replace, accumulate)
        if !other.dirs.is_empty() {
            self.dirs.extend(other.dirs.iter().cloned());
        }
        // auto_load: true overrides false
        if other.auto_load {
            self.auto_load = true;
        }
    }
}

/// Expands `~` to home directory.
fn expand_tilde(path: &std::path::Path) -> PathBuf {
    if let Ok(stripped) = path.strip_prefix("~") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = OrcsConfig::default();
        assert!(!config.debug);
        assert_eq!(config.model.default, "claude-3-opus");
        assert!(!config.hil.auto_approve);
    }

    #[test]
    fn toml_roundtrip() {
        let config = OrcsConfig::default();
        let toml = config.to_toml().unwrap();
        let restored = OrcsConfig::from_toml(&toml).unwrap();
        assert_eq!(config, restored);
    }

    #[test]
    fn toml_partial_parse() {
        let toml = r#"
debug = true

[model]
default = "custom-model"
"#;
        let config = OrcsConfig::from_toml(toml).unwrap();
        assert!(config.debug);
        assert_eq!(config.model.default, "custom-model");
        // Defaults for unspecified fields
        assert!(!config.hil.auto_approve);
    }

    #[test]
    fn merge_overrides_non_default() {
        let mut base = OrcsConfig::default();
        let overlay = OrcsConfig {
            debug: true,
            model: ModelConfig {
                default: "custom".into(),
                ..Default::default()
            },
            ..Default::default()
        };

        base.merge(&overlay);

        assert!(base.debug);
        assert_eq!(base.model.default, "custom");
        // Should keep base value for unmodified fields
        assert!(!base.hil.auto_approve);
    }

    #[test]
    fn merge_keeps_base_when_overlay_is_default() {
        let mut base = OrcsConfig {
            debug: true,
            ..Default::default()
        };
        let overlay = OrcsConfig::default();

        base.merge(&overlay);

        // Should keep base value since overlay is default
        assert!(base.debug);
    }

    // === ScriptsConfig tests ===

    #[test]
    fn scripts_config_default() {
        let config = ScriptsConfig::default();
        assert!(config.dirs.is_empty());
        assert!(!config.auto_load);
    }

    #[test]
    fn scripts_config_toml_parse() {
        let toml = r#"
[scripts]
dirs = ["~/.orcs/scripts", ".orcs/scripts"]
auto_load = true
"#;
        let config = OrcsConfig::from_toml(toml).unwrap();
        assert_eq!(config.scripts.dirs.len(), 2);
        assert!(config.scripts.auto_load);
    }

    #[test]
    fn scripts_config_merge_accumulates_dirs() {
        let mut base = ScriptsConfig {
            dirs: vec![PathBuf::from("/base/scripts")],
            auto_load: false,
        };
        let overlay = ScriptsConfig {
            dirs: vec![PathBuf::from("/overlay/scripts")],
            auto_load: true,
        };

        base.merge(&overlay);

        assert_eq!(base.dirs.len(), 2);
        assert!(base.dirs.contains(&PathBuf::from("/base/scripts")));
        assert!(base.dirs.contains(&PathBuf::from("/overlay/scripts")));
        assert!(base.auto_load);
    }

    #[test]
    fn expand_tilde_with_home() {
        let path = PathBuf::from("~/.orcs/scripts");
        let expanded = expand_tilde(&path);

        // Should not start with ~
        assert!(!expanded.starts_with("~"));

        // Should end with .orcs/scripts
        assert!(expanded.ends_with(".orcs/scripts"));
    }

    #[test]
    fn expand_tilde_absolute_unchanged() {
        let path = PathBuf::from("/absolute/path");
        let expanded = expand_tilde(&path);
        assert_eq!(expanded, path);
    }

    #[test]
    fn resolve_dirs_filters_nonexistent() {
        let config = ScriptsConfig {
            dirs: vec![PathBuf::from("/nonexistent/path")],
            auto_load: true,
        };
        let resolved = config.resolve_dirs(None);

        // Nonexistent paths are filtered out
        assert!(resolved.is_empty());
    }
}
